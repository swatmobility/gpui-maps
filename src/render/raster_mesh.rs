//! **The square a raster tile wears its texture on.**
//!
//! A raster tile has no geometry in the source bytes -- there is nothing to
//! tessellate. What it needs is the tile's own square, cut into enough
//! pieces that projecting its corners onto the sphere does not leave a
//! chord where a curve should be. That is exactly the job
//! [`crate::render::subdivide`] does for fills, and this module takes the
//! subdivision from the same place fills do
//! ([`granularity_for_zoom`](crate::render::subdivide::granularity_for_zoom))
//! so that a raster tile and the vector tile over it cut the sphere
//! **identically**, by construction rather than by two implementations
//! agreeing.
//!
//! # Why this is a separate mesh and not a `PaintOp`
//!
//! Because it is not triangles-with-colours. `render::mesh::Vertex` is 88
//! bytes of paint attributes -- two colours, two widths, a gap pair, a dash
//! run -- and a raster tile needs none of them and one thing that vertex
//! has no room for: a texture coordinate. Threading a `PaintOp::Raster`
//! through the tessellation path would mean touching ten exhaustive
//! matches to carry something that cannot be tessellated. See the plan
//! document's decision 2.
//!
//! # One mesh per granularity, not one per tile
//!
//! This is the whole reason a raster tile costs no per-tile geometry. A
//! fill mesh is per-tile because the fills differ per tile. A raster
//! tile's square does not: every tile at one zoom has the SAME granularity
//! (`granularity_for_zoom` is a pure function of `z`) and the same `[0,1]`
//! square. At z4 the entire cover shares one 5x5 vertex grid, uploaded
//! once.
//!
//! This mirrors the reference's `_tileMeshCache`, keyed on granularity and
//! pole flags (`vertical_perspective_projection.ts:71-97`, BSD-3).
//!
//! # What this module deliberately does NOT do
//!
//! **No border ring.** MapLibre generates an optional `EXTENT / 128`
//! border (`create_tile_mesh.ts:13`, `:121`) and draws raster tiles in two
//! stencil passes -- borderless first, then borders into whatever pixels
//! the stencil says nobody covered (`draw_raster.ts:55-77`). That machinery
//! exists because **its** neighbours can carry different granularities and
//! therefore cut their shared edge at different points. Ours cannot:
//! `granularity_for_zoom` depends only on `z`, and a cover is one `z`, so
//! two adjacent tiles cut their shared edge at identical parameter values
//! and the two chord polygons share it exactly. A border and a stencil
//! pass here would buy nothing and cost a second draw of every tile.
//!
//! That claim is graded on pixels, not asserted -- see the tile-boundary
//! capture in the plan document's Task 6. If it ever fails, the fix is the
//! reference's border, not a texture-coordinate inset (which would scale
//! the imagery by `N/(N-1)` and trade a boundary colour error for a
//! whole-tile registration error).

use crate::render::subdivide::granularity_for_zoom;

/// One vertex of a raster tile's square.
///
/// Four f32s, against `render::mesh::Vertex`'s twenty-two. A raster tile
/// carries no colour, no width and no dash -- its entire appearance is the
/// texel it samples and the one `raster-opacity` scalar in the model
/// uniform.
///
/// # Why `position` is in lyon units and `uv` is separate
///
/// `position` is in **lyon units** -- MVT coordinates times
/// `mesh::COORD_SCALE` -- because that is the space
/// `render::combined_tile_matrix` maps to clip space, and reusing that
/// matrix is what makes the FLAT path correct with no new maths at all.
/// On the globe path the shader recovers `[0,1]` tile space the same way
/// `shader.wgsl:600` does, by multiplying by `model.lyon_to_tile_unit`.
///
/// `uv` cannot be derived from `position` by that route, because
/// `lyon_to_tile_unit` is **0.0** on a flat frame (`GlobeParams::FLAT`),
/// where every globe reader is deliberately behind a `transition > 0.0`
/// branch. So the texture coordinate is carried explicitly. It costs 8
/// bytes on a vertex that is shared by every tile at its zoom -- 67 KB for
/// the whole of z0, once.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct RasterVertex {
    /// Lyon units: `[0, extent * COORD_SCALE]` on both axes.
    pub position: [f32; 2],
    /// Texture coordinate in `[0, 1]`, v increasing downward to match the
    /// row order `image::RgbaImage` and `wgpu::Queue::write_texture` both
    /// use.
    pub uv: [f32; 2],
    /// [`POLE_NONE`], [`POLE_NORTH`] or [`POLE_SOUTH`].
    ///
    /// The same convention `render::subdivide::POLE_NORTH_MARKER` sets for
    /// fills, deliberately: a pole vertex keeps the POSITION of the tile
    /// edge it was grown from, and the shader snaps its sphere position to
    /// `(0, +/-1, 0)` rather than letting `project_to_sphere` put it at
    /// Mercator's +/-85.051 edge. Carried in its own field here rather
    /// than smuggled into another, because unlike `mesh::Vertex` this
    /// struct has no field whose value space is already provably free.
    pub pole: f32,
    /// Padding to a 16-byte stride. Named rather than implicit so the
    /// vertex-attribute array in `raster_pipeline` and this struct cannot
    /// drift apart silently.
    pub _pad: f32,
}

/// Not a pole vertex.
pub const POLE_NONE: f32 = 0.0;
/// This vertex is really the north pole. Mirrors
/// `render::subdivide::POLE_NORTH_MARKER`.
pub const POLE_NORTH: f32 = -1.0;
/// This vertex is really the south pole. Mirrors
/// `render::subdivide::POLE_SOUTH_MARKER`.
pub const POLE_SOUTH: f32 = -2.0;

/// A raster tile's square, cut into `granularity` x `granularity` quads.
#[derive(Debug, Clone, PartialEq)]
pub struct RasterMesh {
    pub vertices: Vec<RasterVertex>,
    pub indices: Vec<u32>,
}

impl RasterMesh {
    /// `true` when there is nothing to draw. Never the case for a
    /// granularity of 1 or more, but callers should not have to know that.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// The key a [`RasterMesh`] is cached under.
///
/// Granularity plus the two pole flags -- exactly the reference's mesh
/// cache key (`vertical_perspective_projection.ts:71-73`). Every tile at
/// one zoom that is not on the top or bottom Mercator row shares one
/// entry, which is all but `2 * 2^z` of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RasterMeshKey {
    pub granularity: u32,
    pub north_pole: bool,
    pub south_pole: bool,
}

impl RasterMeshKey {
    /// The key for a given tile, taking its granularity from the same
    /// function the fill path takes it from.
    #[must_use]
    pub fn for_tile(tile: crate::geo::TileId) -> Self {
        let last_row = (1u32 << tile.z) - 1;
        Self {
            granularity: granularity_for_zoom(tile.z),
            north_pole: tile.y == 0,
            south_pole: tile.y == last_row,
        }
    }
}

/// Builds the square for one [`RasterMeshKey`].
///
/// `extent` and `coord_scale` are the tile's MVT extent
/// (`mvt::MVT_EXTENT`) and `mesh::COORD_SCALE`, passed rather than read so
/// this function stays a pure function of its arguments and its tests need
/// no fixtures.
///
/// # The grid
///
/// `(g+1)^2` vertices and `6 * g^2` indices, two triangles per quad, wound
/// counter-clockwise in a y-down coordinate system. The pipeline does no
/// face culling (`PrimitiveState::default()`, as `TilePipeline` uses), so
/// the winding is a convention rather than a correctness requirement --
/// but it is the same convention `render::mesh` emits, and a future cull
/// should not have to discover that the two disagree.
///
/// # The pole rows
///
/// When `north_pole` is set, a row of triangles is grown from the tile's
/// top edge up to a single shared pole vertex, and that vertex is marked
/// [`POLE_NORTH`] so the shader snaps it to the true pole. Its `uv.y` is
/// clamped to 0, which is the reference's own pole handling
/// (`raster.vertex.glsl:43-52`): without the clamp the pole vertex would
/// sample whatever texel its interpolated coordinate landed on and the
/// cap would come out a smear of one row of the image.
///
/// This is a CAP, not a replacement of the top row -- the same shape
/// `subdivide::append_pole_caps` builds for fills, for the same reason:
/// the tile's own square still has to be drawn where it is, and the cap
/// covers the ground between Mercator's edge and the pole that no tile
/// covers at all.
#[must_use]
pub fn build(key: RasterMeshKey, extent: u32, coord_scale: f32) -> RasterMesh {
    let g = key.granularity.max(1);
    let span = extent as f32 * coord_scale;
    let step = span / g as f32;

    let row_len = g + 1;
    let mut vertices = Vec::with_capacity((row_len * row_len) as usize);
    for row in 0..row_len {
        for col in 0..row_len {
            let fx = col as f32 / g as f32;
            let fy = row as f32 / g as f32;
            vertices.push(RasterVertex {
                position: [col as f32 * step, row as f32 * step],
                uv: [fx, fy],
                pole: POLE_NONE,
                _pad: 0.0,
            });
        }
    }

    let mut indices = Vec::with_capacity((6 * g * g) as usize);
    let at = |row: u32, col: u32| row * row_len + col;
    for row in 0..g {
        for col in 0..g {
            let (tl, tr, bl, br) =
                (at(row, col), at(row, col + 1), at(row + 1, col), at(row + 1, col + 1));
            indices.extend_from_slice(&[tl, bl, tr, tr, bl, br]);
        }
    }

    // The caps, after the body, so a reader of the index buffer sees the
    // tile itself first. `step` is the cell width, so the cap's triangles
    // are one per top-edge (or bottom-edge) cell.
    if key.north_pole {
        let pole = vertices.len() as u32;
        // Position is the tile's top-left corner: a pole vertex keeps the
        // position of an edge vertex it was grown from and is relocated by
        // the shader, never by its coordinate. See `RasterVertex::pole`.
        vertices.push(RasterVertex {
            position: [0.0, 0.0],
            uv: [0.0, 0.0],
            pole: POLE_NORTH,
            _pad: 0.0,
        });
        for col in 0..g {
            indices.extend_from_slice(&[at(0, col), pole, at(0, col + 1)]);
        }
    }
    if key.south_pole {
        let pole = vertices.len() as u32;
        vertices.push(RasterVertex {
            position: [0.0, span],
            uv: [0.0, 1.0],
            pole: POLE_SOUTH,
            _pad: 0.0,
        });
        for col in 0..g {
            indices.extend_from_slice(&[at(g, col), at(g, col + 1), pole]);
        }
    }

    RasterMesh { vertices, indices }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::TileId;
    use crate::mvt::MVT_EXTENT;
    use crate::render::mesh::COORD_SCALE;

    fn plain(granularity: u32) -> RasterMeshKey {
        RasterMeshKey { granularity, north_pole: false, south_pole: false }
    }

    /// The counts the whole design rests on: `(g+1)^2` vertices and
    /// `6g^2` indices, for every granularity `granularity_for_zoom` can
    /// return.
    #[test]
    fn the_grid_has_the_vertex_and_index_counts_its_granularity_implies() {
        for z in 0..=8u8 {
            let g = granularity_for_zoom(z);
            let mesh = build(plain(g), MVT_EXTENT, COORD_SCALE);
            assert_eq!(
                mesh.vertices.len(),
                ((g + 1) * (g + 1)) as usize,
                "z{z} (granularity {g}) vertex count"
            );
            assert_eq!(mesh.indices.len(), (6 * g * g) as usize, "z{z} index count");
            assert!(
                mesh.indices.iter().all(|&i| (i as usize) < mesh.vertices.len()),
                "z{z} has an index past the end of its vertex buffer"
            );
        }
    }

    /// Every vertex inside the tile, and the four corners exactly on it.
    /// A raster tile that overhung its own square would draw over its
    /// neighbour -- which is the failure a border ring would deliberately
    /// introduce, and which this module deliberately does not have.
    #[test]
    fn the_square_covers_its_tile_exactly_and_overhangs_nothing() {
        let span = MVT_EXTENT as f32 * COORD_SCALE;
        let mesh = build(plain(4), MVT_EXTENT, COORD_SCALE);

        for v in &mesh.vertices {
            assert!(
                (0.0..=span).contains(&v.position[0]) && (0.0..=span).contains(&v.position[1]),
                "{v:?} is outside the tile's own square"
            );
            assert!(
                (0.0..=1.0).contains(&v.uv[0]) && (0.0..=1.0).contains(&v.uv[1]),
                "{v:?} samples outside its own texture"
            );
        }

        for corner in [[0.0, 0.0], [span, 0.0], [0.0, span], [span, span]] {
            assert_eq!(
                mesh.vertices.iter().filter(|v| v.position == corner).count(),
                1,
                "corner {corner:?} must be present exactly once"
            );
        }
    }

    /// The texture coordinate has to track the position across the whole
    /// grid, not just at the corners. A `uv` that drifted from the
    /// position -- an off-by-one in the divisor, say -- would put the
    /// imagery a fraction of a tile off, which reads as "the satellite
    /// layer doesn't line up with the roads" and is very hard to see in a
    /// single tile.
    #[test]
    fn every_texture_coordinate_is_its_own_position_in_the_tile() {
        let span = MVT_EXTENT as f32 * COORD_SCALE;
        let mesh = build(plain(8), MVT_EXTENT, COORD_SCALE);
        for v in mesh.vertices.iter().filter(|v| v.pole == POLE_NONE) {
            assert!(
                (v.uv[0] - v.position[0] / span).abs() < 1e-6,
                "{v:?}: u and x disagree"
            );
            assert!(
                (v.uv[1] - v.position[1] / span).abs() < 1e-6,
                "{v:?}: v and y disagree"
            );
        }
    }

    /// **Two neighbours cut their shared edge at identical parameters.**
    /// This is the argument the no-border decision rests on, made
    /// arithmetically here and on pixels in the capture test: because
    /// `granularity_for_zoom` reads only `z`, two tiles of one cover
    /// produce bit-identical edge coordinates, so their chord polygons
    /// share the edge exactly and no sliver can open between them.
    #[test]
    fn neighbouring_tiles_at_one_zoom_cut_their_shared_edge_identically() {
        for z in 0..=6u8 {
            let left = RasterMeshKey::for_tile(TileId { z, x: 1, y: 1 });
            let right = RasterMeshKey::for_tile(TileId { z, x: 2, y: 1 });
            assert_eq!(
                left.granularity, right.granularity,
                "z{z}: neighbours with different granularities WOULD need the \
                 reference's border ring"
            );

            let a = build(left, MVT_EXTENT, COORD_SCALE);
            let b = build(right, MVT_EXTENT, COORD_SCALE);
            let span = MVT_EXTENT as f32 * COORD_SCALE;
            // `a`'s right edge and `b`'s left edge, as the y values each
            // one cuts at. Bit-identical, not merely close.
            // Pole-cap vertices are excluded, and that exclusion is the
            // point rather than a convenience. A cap vertex KEEPS the
            // position of the tile edge it grew from and is relocated by
            // the shader (see `RasterVertex::pole`), so at z1 the south
            // cap's vertex sits at `[0, span]` -- on the left edge, by
            // coordinate, while being nowhere near it on the sphere.
            // Comparing it as edge geometry made this test fail with one
            // list a single entry longer than the other, which is a real
            // property of the mesh and not a seam.
            let edge_of = |mesh: &RasterMesh, x: f32| -> Vec<f32> {
                let mut ys: Vec<f32> = mesh
                    .vertices
                    .iter()
                    .filter(|v| v.pole == POLE_NONE && v.position[0] == x)
                    .map(|v| v.position[1])
                    .collect();
                ys.sort_by(f32::total_cmp);
                ys
            };
            assert_eq!(
                edge_of(&a, span),
                edge_of(&b, 0.0),
                "z{z}: the two sides of a shared edge are cut at different points"
            );
        }
    }

    /// A tile on the top Mercator row grows a north cap; one in the
    /// middle grows neither. The cap's vertex is marked, not moved -- the
    /// shader relocates it, exactly as it does for a fill's pole vertex.
    #[test]
    fn only_the_pole_rows_grow_a_cap_and_its_vertex_is_marked_not_moved() {
        let plain_mesh = build(plain(4), MVT_EXTENT, COORD_SCALE);

        let north = build(
            RasterMeshKey { granularity: 4, north_pole: true, south_pole: false },
            MVT_EXTENT,
            COORD_SCALE,
        );
        assert_eq!(north.vertices.len(), plain_mesh.vertices.len() + 1);
        assert_eq!(north.indices.len(), plain_mesh.indices.len() + 3 * 4);

        let marked: Vec<&RasterVertex> =
            north.vertices.iter().filter(|v| v.pole != POLE_NONE).collect();
        assert_eq!(marked.len(), 1, "one cap, one pole vertex");
        assert_eq!(marked[0].pole, POLE_NORTH);
        assert_eq!(
            marked[0].uv[1], 0.0,
            "the pole vertex must sample the top row of the image -- the \
             reference clamps this (raster.vertex.glsl:43-52) and without it \
             the cap is a smear"
        );
        assert!(
            marked[0].position[1] == 0.0,
            "a pole vertex keeps the POSITION of the edge it grew from; the \
             shader is what relocates it"
        );

        // z1/y1 is the bottom row of a 2x2 grid: south only.
        let bottom = RasterMeshKey::for_tile(TileId { z: 1, x: 0, y: 1 });
        assert!(bottom.south_pole && !bottom.north_pole);
        // z2/y1 is an inner row: neither.
        let inner = RasterMeshKey::for_tile(TileId { z: 2, x: 1, y: 1 });
        assert!(!inner.south_pole && !inner.north_pole);
    }

    /// The cache key is what makes "one mesh per granularity, not per
    /// tile" true. Every non-pole-row tile at one zoom must produce the
    /// same key, or the cache degenerates into one entry per tile and the
    /// design's main saving is gone.
    #[test]
    fn every_inner_tile_at_one_zoom_shares_one_mesh_key() {
        let z = 4u8;
        let keys: std::collections::HashSet<RasterMeshKey> = (1..(1u32 << z) - 1)
            .flat_map(|y| (0..1u32 << z).map(move |x| RasterMeshKey::for_tile(TileId { z, x, y })))
            .collect();
        assert_eq!(keys.len(), 1, "got {keys:?}");
    }
}
