//! Bake-time subdivision: the reason the globe's silhouette is a circle and
//! not a polygon.
//!
//! A port, in structure and in the numbers it chooses, of MapLibre GL JS's
//! `src/render/subdivision.ts` (BSD-3, `Copyright (c) 2023, MapLibre
//! contributors`). Where it diverges the divergence is named, with the
//! reference's file and line, in the item that diverges.
//!
//! # Why this exists at all
//!
//! `shader.wgsl`'s `project_to_sphere` runs PER VERTEX. A z2 tile spanning
//! 90 degrees of longitude tessellates to two triangles, so the sphere
//! projection moves its four corners and the rasterizer draws straight lines
//! between them: a chord through the planet, not an arc across it. The
//! silhouette of a globe covered in such quads is a POLYGON — and that is
//! exactly what it looked like before this module existed. The GPU's own
//! far-side clip (`globe_clipping_z`, `z > w` with `unclipped_depth` false)
//! cuts along the same straight chord for the same reason, so it cannot
//! round the limb either. Only more vertices can.
//!
//! # Why unconditionally, and not per projection
//!
//! **This is the load-bearing divergence of the whole globe plan** (see
//! `docs/superpowers/plans/2026-09-13-globe-projection.md`, "Where we
//! diverge" #2), so it is stated here rather than left to the plan.
//!
//! MapLibre bakes granularity at parse time on the worker
//! (`vector_tile_source.ts:218` -> `worker_tile.ts:67,188`), so a bucket
//! serves ONE projection, and `style.ts:1755-1762`'s
//! `_setProjectionInternal` calls `reload()` on every source when the
//! projection name changes. Worse, within `globe` the granularity delegates
//! by transition state (`globe_projection.ts:59-61`), so tiles fetched past
//! the transition are parsed unsubdivided while already-parsed tiles keep
//! their old mesh.
//!
//! We do not need any of that, because **Mercator is affine in tile space**:
//! subdividing a triangle and projecting it flat covers the same area, and
//! the extra vertices are the only cost.
//! `subdividing_a_flat_tile_opens_no_crack` (in `render::gpu`) proves
//! that claim against the rasterizer rather than asserting it. So
//! [`Subdivision::for_tile`] depends on the tile's OWN zoom and nothing
//! else — never on the camera, never on `transition` — which buys: one mesh
//! per tile forever, no re-parse on a projection change, no cache
//! invalidation, and no split-brain where neighbouring tiles disagree
//! because they were parsed either side of a transition.
//!
//! # What it costs, measured
//!
//! Real tiles (OpenFreeMap planet, Liberty style, whole-tile
//! `tessellate_plan_with_ranges`, p50 of 7 runs with the spread, release
//! build, Apple M-series):
//!
//! ```text
//!   tile        g   unsubdivided       subdivided        mesh bytes
//!   z0/0/0     64   1.901 ms           5.307 ms          1.61 -> 7.77 MB
//!                   (1.224-2.638)      (4.230-7.198)
//!   z1/0/0     32   0.291 ms           1.134 ms          0.61 -> 2.68 MB
//!                   (0.271-0.419)      (1.126-1.377)
//!   z2/1/1     16   0.661 ms           1.685 ms          1.04 -> 2.42 MB
//!                   (0.624-0.804)      (1.641-1.874)
//!   z5/16/10    2  10.647 ms          11.237 ms         47.52 -> 47.79 MB
//!                   (10.281-12.030)   (11.021-12.591)
//!   z14 (fixt)  1   0.608 ms           0.601 ms          2.52 -> 2.52 MB
//! ```
//!
//! All of it is BAKE time, on the blocking pool (`map_view::bake_tile`, via
//! `fetch_and_bake`'s `spawn_blocking`). **No `paint()` phase moves** and
//! none was added: subdivision does no per-frame work at all, which is the
//! whole reason it is at bake time.
//!
//! The z14 row is the one to read twice. `granularity_for_zoom(14)` is 1, so
//! that tile takes `Subdivision::NONE` and the bake is the SAME CODE PATH it
//! was before this module existed -- the 7 ms difference between those two
//! columns is run-to-run noise on an identical mesh, which is why the plan's
//! `dense_tile.tessellate.p50_ms` budget cannot move.
//!
//! # 32-bit indices
//!
//! The reference flips its index buffer from 16- to 32-bit above 65,536
//! vertices (`create_tile_mesh.ts:134-141`). Checked rather than assumed:
//! [`crate::render::TileMesh`] is `VertexBuffers<Vertex, u32>` and has been
//! since before the globe, so there is nothing to flip and no 65,536 cliff
//! to fall off.

use std::collections::HashMap;

use lyon::math::Point;
use lyon::tessellation::VertexBuffers;

use crate::geo::TileId;
use crate::render::mesh::Vertex;

/// Granularity at zoom 0, halved per zoom level — the `baseZoomGranularity`
/// of `SubdivisionGranularityExpression`
/// (`subdivision_granularity_settings.ts:19-39`).
///
/// **64, where the reference uses 128 for fills and 512 for lines**
/// (`vertical_perspective_projection.ts:13-26`), and this is OUR number, not
/// MapLibre's. The reasoning, and the price:
///
/// A tile at zoom `z` spans `360 / 2^z` degrees of longitude, and
/// [`granularity_for_zoom`] is `max(base >> z, 1)`, so a subdivision cell
/// spans `(360 / 2^z) / (base >> z)` = **`360 / base` degrees at every zoom
/// level in the globe band** — one constant, by construction. At `base = 64`
/// that is `5.625` degrees per cell.
///
/// A straight chord across `alpha` radians of a sphere whose screen radius
/// is `r` pixels departs from the arc by `r * (1 - cos(alpha/2))`, which for
/// small `alpha` is `r * alpha^2 / 8`. At `alpha = 5.625` degrees
/// (`0.09817` rad) that is `r * 0.001205` px:
///
/// ```text
///   globe screen radius      limb error at base 64    at the reference's 128
///     161 px (the disc `at_full_transition...` renders)   0.19 px   0.05 px
///     379 px (the 758 px disc of a 1024x768 window at z3) 0.46 px   0.11 px
///    1500 px (a full-screen globe on a 4K display)        1.81 px   0.45 px
/// ```
///
/// So 64 costs about a third of a pixel of limb error at the window this
/// project actually renders in, and buys a **4x** reduction in the vertex
/// count of every subdivided fill (vertices go as `(g+1)^2`). That trade is
/// made deliberately and in this direction because `render::mesh::Vertex` is
/// still **88 bytes, 64 of them per-op-constant paint** — the globe plan's
/// Task 4 (the per-op paint table, `3.4x`) is NOT done, so every vertex this
/// module creates is priced at 88 bytes rather than 26. Raising this
/// constant to the reference's 128 is a one-line change once Task 4 lands;
/// the figures that justify either choice are in
/// `subdivision_mesh_cost_per_tile_is_what_the_report_claims`.
pub const BASE_GRANULARITY: u32 = 64;

/// Granularity for a tile of zoom `z`: `max(BASE_GRANULARITY >> z, 1)`.
///
/// `Math.max(Math.floor(base / (1 << zoom)), min, 1)` in the reference
/// (`subdivision_granularity_settings.ts:36-39`), with **`min = 1`** where
/// the reference's `fill` uses 2 and its `line` uses 0.
///
/// `min = 1` is not a corner cut, it is the same criterion continued: at
/// `z = 6` a whole tile spans `5.625` degrees, which is already ONE cell of
/// [`BASE_GRANULARITY`]'s grid, so there is nothing left to split. What is
/// left is the sphere's own bow across one unsubdivided tile, and it is
/// already sub-pixel by the time the grid runs out: 1.6 px at z7, 0.8 px at
/// z8, halving per level. (That sentence used to read "`transition_for_zoom`
/// is exactly 0.0 at and above `GLOBE_ZOOM_MAX = 6`, so a tile at z6 or
/// deeper is only ever drawn flat". It is not, any more: the projection
/// blend moved to z11-z12, where the reference puts it, so a z7 tile is
/// drawn on the sphere. The chord figures above are why that does not
/// change this constant.)
/// The reference keeps `fill` at 2 forever and says why it does not matter
/// ("fill geometry tends to already be highly tessellated and granular at
/// high zooms", `vertical_perspective_projection.ts:17-19`); its `min = 32`
/// for `tile` exists for raster tiles, which this crate does not have, and
/// its `stencil` expression for tile stencil masks, which it does not have
/// either.
///
/// The practical consequence is worth stating plainly: **at z6 and deeper
/// the bake is bit-identical to what it was before this module existed**,
/// which is why `dense_tile.tessellate.p50_ms` (17.94 ms at z14) cannot
/// move.
///
/// ```text
///   z      0   1   2   3   4   5   6+
///   g     64  32  16   8   4   2   1
/// ```
#[must_use]
pub fn granularity_for_zoom(z: u8) -> u32 {
    if z >= 32 {
        return 1;
    }
    (BASE_GRANULARITY >> z).max(1)
}

/// What subdivision one tile's triangulation gets: the grid, and whether the
/// tile touches a pole.
///
/// A plain value rather than a [`TileId`], for the same reason
/// `map_view::bake_tile` takes `tile_units_per_px` as a scalar: the bake is
/// deliberately blind to tile IDENTITY (it must not be able to look anything
/// up), while a derived parameter is fine. `Subdivision::for_tile` is
/// computed at the call site, where the `TileId` legitimately lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subdivision {
    /// Cells per tile edge. `1` means no subdivision at all.
    pub granularity: u32,
    /// This tile borders Mercator's north edge (`tile.y == 0`), so the fills
    /// that reach that edge are extended to the north pole. MapLibre's
    /// `_handlePoles`' `canonical.y === 0` (`subdivision.ts:448-450`).
    pub north_pole: bool,
    /// The same at the south edge, `tile.y == (1 << tile.z) - 1`
    /// (`subdivision.ts:451-453`).
    pub south_pole: bool,
}

impl Subdivision {
    /// No subdivision and no poles: the pre-globe behaviour, named once so
    /// the callers that must reproduce it exactly say so in one word.
    ///
    /// Same idiom as `render::GlobeParams::FLAT` and
    /// `render::globe::GlobeCover::FLAT`.
    pub const NONE: Self = Self { granularity: 1, north_pole: false, south_pole: false };

    /// **An overlay mesh's grid** -- a runtime GeoJSON source's, and the
    /// route line's -- always used with [`OVERLAY_SUBDIVISION_EXTENT`] as
    /// the extent, so a cell is [`OVERLAY_CELL_LYON_UNITS`] of the overlay's
    /// local grid: 1/1024 of the world, a z10 tile's width, 0.35 degrees of
    /// longitude.
    ///
    /// # Why an overlay needs its own grid, and why this fine
    ///
    /// A tile gets `granularity_for_zoom(tile.z)` because a tile is drawn
    /// only near its own zoom, so a cell only has to be small relative to
    /// THAT zoom's globe, and from z6 down a whole tile already is. An
    /// overlay mesh is ONE mesh for every zoom -- a feature a thousand
    /// kilometres wide tessellates to a few triangles whose corners are
    /// degrees apart -- and `shader.wgsl` projects per vertex, so such a
    /// triangle is drawn as a flat chord plane through the planet. MapLibre
    /// never meets this because its GeoJSON source is re-tiled per zoom
    /// (geojson-vt) and each tile is then subdivided like any other; this
    /// crate's overlay is not tiled, so the cut has to be fine enough for
    /// the deepest zoom the sphere is drawn at.
    ///
    /// That zoom is [`crate::render::globe::PROJECTION_BLEND_ZOOM_MAX`], 12,
    /// with the sphere fully on through z11. A chord across `alpha` radians
    /// of a sphere of screen radius `r` sags `r * alpha^2 / 8` px. At z11
    /// `r = 512 * 2^11 / 2pi = 166,886` px and a 1/1024 cell is `alpha =
    /// 0.00614`, so the sag is **0.79 px**, and it halves per zoom level
    /// out from there -- the same order as a z8 tile's own unsubdivided
    /// bow. At z12 the geometry is pure Mercator and the sag is not drawn.
    ///
    /// **It costs little** because overlay features are almost always
    /// smaller than a cell: a triangle inside one cell is passed through
    /// untouched (`subdivide_fill_span`'s first loop), and a line gains a
    /// vertex only where it crosses a grid line. The cells are anchored at
    /// the overlay's origin rather than the world's, which is harmless:
    /// alignment matters only where two meshes must share a seam, and an
    /// overlay source is one mesh.
    ///
    /// Granularity 512 is not the cell count per world edge -- it only sets
    /// the per-primitive runaway budget (`MAX_CELLS_PER_PRIMITIVE *
    /// granularity` = 2048 cells, 720 degrees, so no real primitive is ever
    /// passed through unsplit) and, with the extent, the cell size. 512 is
    /// the largest power of two whose extent still fits a `u32`.
    pub const OVERLAY: Self = Self { granularity: 512, north_pole: false, south_pole: false };

    /// The subdivision a tile of this id gets.
    #[must_use]
    pub fn for_tile(tile: TileId) -> Self {
        let last_row = (1u32 << tile.z) - 1;
        Self {
            granularity: granularity_for_zoom(tile.z),
            north_pole: tile.y == 0,
            south_pole: tile.y == last_row,
        }
    }

    /// Nothing to do: neither a grid to cut on nor a pole to close.
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.granularity < 2
    }

    /// The subdivision cell's size in LYON units — the space
    /// `render::mesh`'s vertex positions are in, which is source coordinates
    /// times `COORD_SCALE`.
    ///
    /// This is the one thing that makes `tessellate_plan_with_ranges`'
    /// long-dead `extent` parameter live again: a cell is `EXTENT /
    /// granularity` in the reference (`subdivision.ts:46`), and the tile's
    /// extent is not recoverable from a vertex position.
    fn cell_size(&self, extent: u32, coord_scale: f32) -> f32 {
        (extent as f32 * coord_scale) / self.granularity as f32
    }
}

/// The most cells across, on one axis, that one primitive is allowed to be
/// split into — beyond it the primitive is passed through whole.
///
/// **This is a guard against a hang, not a quality knob.** The reference
/// does not need one: its coordinates are validated into signed 16 bits
/// (`_vertexToIndex` throws outside ±32768, `subdivision.ts:61-64`), so a
/// triangle can never span more than eight tiles. Ours are lyon units from
/// `mvt::decode`, which deliberately reproduces `mvt-reader`'s behaviour of
/// clamping a runaway geometry cursor at `i32::MAX` rather than rejecting
/// the tile (`mvt.rs`, "a cursor that overflows `i32` clamps to
/// `i32::MAX`", a case reported against OpenFreeMap). A triangle reaching
/// 2e9 MVT units is 2e12 lyon units across, and at z0's 64,000-unit cell
/// that is `2e12 / 64_000` = **31.25 million** cell rows, with a comparable
/// number of columns inside each — on the bake pool, for one malformed
/// feature. That figure is arithmetic, not an observed hang: running it to
/// find out would exhaust the machine, so it was deliberately not run.
///
/// `4 * granularity` — four tile widths — is chosen to be far beyond any
/// real tile buffer (encoders clip to a fraction of the extent) while
/// leaving no room for the pathological case. Geometry that far outside its
/// own tile is not on screen, so passing it through unsplit costs nothing
/// visible; `a_runaway_coordinate_is_passed_through_instead_of_hanging`
/// pins that it is passed through and not dropped.
const MAX_CELLS_PER_PRIMITIVE: i64 = 4;

/// One cell of [`Subdivision::OVERLAY`]'s grid, in overlay lyon units
/// (local grid units at `OVERLAY_COORD_SCALE` = 1): 1/1024 of the world.
pub const OVERLAY_CELL_LYON_UNITS: f64 =
    crate::render::camera::WORLD_SIZE as f64 * crate::overlay::geom::OVERLAY_UNITS_PER_WORLD / 1024.0;

/// The extent an overlay mesh is tessellated with, so that
/// `Subdivision::OVERLAY`'s cell is [`OVERLAY_CELL_LYON_UNITS`]. The
/// tessellator reads the extent for nothing else.
pub const OVERLAY_SUBDIVISION_EXTENT: u32 =
    (OVERLAY_CELL_LYON_UNITS * Subdivision::OVERLAY.granularity as f64) as u32;

/// `Vertex::is_stroke` for a vertex that is really the NORTH pole, wherever
/// its `position` says it is.
///
/// # Why the marker is in `is_stroke` and not in the position
///
/// The reference marks a pole vertex by its COORDINATE: `rawPos.y <
/// -32767.5` for north and `> 32766.5` for south
/// (`_projection_globe.vertex.glsl:83-90`), against sentinels
/// `NORTH_POLE_Y = -32768` / `SOUTH_POLE_Y = 32767`
/// (`subdivision.ts:24-25`). Those are values in MVT **extent** space, where
/// a tile is 0..8192 and -32768 is four tiles away.
///
/// **This crate's vertex positions are lyon units — MVT coordinates times
/// `COORD_SCALE` (1000) — where -32768 is an ordinary coordinate about a
/// third of the way across a tile.** Porting the test as written would
/// discard real geometry. And no coordinate-space sentinel is provably safe
/// here either: `mvt::decode` reproduces `mvt-reader`'s behaviour of
/// clamping a geometry cursor only at `i32::MAX`, so a decoded coordinate is
/// bounded by `2^31 * COORD_SCALE` and not by the tile. A band chosen
/// outside "anything a real encoder emits" would be a convention, not a
/// guarantee.
///
/// `is_stroke` IS a guarantee. It is the one vertex attribute that is a pure
/// enumeration written from a literal by the mesh builder and never derived
/// from data: `0.0` in `FillCtor::new_vertex` (`mesh.rs`) and `1.0` in
/// `StrokeCtor::new_vertex`, unconditionally, with no input reaching either.
/// `rewrite_paint_attributes` never touches it (it is geometry-derived, like
/// `position` and `line_distance`). So a third and fourth value in it cannot
/// collide with anything, by construction rather than by convention — and it
/// costs zero extra bytes per vertex, which matters while a vertex is still
/// 88 of them.
///
/// Both markers are negative, so every existing reader — `shader.wgsl`'s
/// `input.is_stroke > 0.5`, `mesh::collapsed_normal_share`'s
/// `v.is_stroke > 0.5` — keeps treating a pole vertex as the fill vertex it
/// is.
pub const POLE_NORTH_MARKER: f32 = -1.0;

/// `Vertex::is_stroke` for a vertex that is really the SOUTH pole. See
/// [`POLE_NORTH_MARKER`] for why the marker lives in this field.
pub const POLE_SOUTH_MARKER: f32 = -2.0;

/// Working buffers for one tile's worth of subdivision, built once per
/// `append_ops` call and threaded through every op — the discipline
/// `append_ops` already applies to its `FillTessellator` and its three point
/// `Vec`s, and Organic Maps' `BatchersPool` in Rust terms.
#[derive(Default)]
pub(crate) struct SubdivideScratch {
    /// Position -> index, so a grid corner shared by four cells is one
    /// vertex and not four. The reference's `_vertexDictionary`
    /// (`subdivision.ts:36,50-75`), keyed there on packed 16-bit integer
    /// coordinates because its positions are integers; keyed here on the two
    /// `f32` bit patterns, because ours are not and must not be rounded to
    /// integers (see [`intersect`]).
    dedupe: HashMap<u64, u32>,
    /// The same for pole vertices, keyed on `x` and which pole. A separate
    /// map because a pole vertex shares its POSITION with the tile-edge
    /// vertex it was grown from (see [`append_pole_caps`]) and differs only
    /// in `is_stroke`, so one map keyed on position would fold the two
    /// together.
    pole_dedupe: HashMap<u64, u32>,
    /// The op's input triangle indices, copied out before the output
    /// overwrites them in place.
    input: Vec<u32>,
    /// The output triangle indices for the op.
    output: Vec<u32>,
    /// A triangle clipped to one cell ROW, and then to one CELL. Two buffers
    /// plus a swap partner, because Sutherland-Hodgman clips one half-plane
    /// at a time.
    row: Vec<Point>,
    cell: Vec<Point>,
    swap: Vec<Point>,
    /// A polyline with grid crossings inserted, for [`subdivide_line`].
    line: Vec<Point>,
}

impl SubdivideScratch {
    fn clear_for_op(&mut self) {
        self.dedupe.clear();
        self.pole_dedupe.clear();
        self.input.clear();
        self.output.clear();
    }
}

/// Where a segment `a -> b` crosses `coord` on `axis` (0 = x, 1 = y), as a
/// point.
///
/// # Why the endpoints are ordered first, and how load-bearing that is
///
/// Two triangles sharing an edge traverse it in opposite directions, and
/// `a + (b - a) * t` is not in general the same `f32` as
/// `b + (a - b) * (1 - t)`. Left alone, the two clipped triangles can land
/// their shared vertex one or two ULPs apart, and a vertex that is not
/// bit-identical is not the SAME vertex: the dedupe below would mint a
/// second one, and the two triangles would meet along two almost-coincident
/// edges instead of one shared one. Ordering the endpoints on their bit
/// patterns — a total order, chosen for being free rather than for meaning
/// anything — makes the two computations literally the same expression.
///
/// MapLibre does not have to solve this: it rounds every generated vertex to
/// an integer in extent space (`_vertexToIndex`, `subdivision.ts:61-75`), so
/// both sides agree by quantisation. That option was open here too — one
/// lyon unit is a thousandth of an MVT unit, about 10 metres at z0 — but
/// rounding MOVES geometry, and
/// `subdivision_multiplies_triangles_without_moving_or_losing_geometry` is a
/// property worth keeping exactly rather than to within half a lyon unit.
///
/// **How much this is worth was measured, and the honest answer is: not
/// much yet.** Deleting the ordering left every test in this module and
/// every pixel test in `render::gpu` passing, including
/// `subdividing_a_flat_tile_opens_no_crack` and
/// `subdivision_leaves_no_crack_between_cells`. That is not a reason to
/// remove it — a 1-ULP gap at a coordinate near 4e6 is about 1e-4 px on
/// screen, far too narrow for a pixel centre to fall into, so the defect it
/// prevents is a duplicated vertex and a mesh that is only accidentally
/// watertight rather than a visible crack. It is recorded here because the
/// alternative was to claim it fixes something measured, and it does not.
pub(crate) fn intersect(a: Point, b: Point, axis: usize, coord: f32) -> Point {
    // Ordered on the bit patterns, not on the values: a total order is all
    // this needs, and the bits give one for free with no NaN case to think
    // about.
    let (p, q) = if (a.x.to_bits(), a.y.to_bits()) <= (b.x.to_bits(), b.y.to_bits()) {
        (a, b)
    } else {
        (b, a)
    };
    let (p_on, q_on) = if axis == 0 { (p.x, q.x) } else { (p.y, q.y) };
    let denominator = q_on - p_on;
    // A segment parallel to the clip line has no single crossing; the caller
    // only asks when the two endpoints straddle `coord`, so this is a guard
    // against f32 underflow rather than a real case.
    if denominator == 0.0 {
        return p;
    }
    let t = (coord - p_on) / denominator;
    let mut out = lyon::math::point(p.x + (q.x - p.x) * t, p.y + (q.y - p.y) * t);
    // Exactly on the clip line, not one ULP off it: the interpolation above
    // is what decides the OTHER axis, and pinning this one makes the grid
    // corner a shared vertex rather than four near-misses.
    if axis == 0 {
        out.x = coord;
    } else {
        out.y = coord;
    }
    out
}

/// Sutherland-Hodgman against one axis-aligned half-plane: keep the part of
/// `input` on the `keep_above` side of `coord` on `axis`, into `output`.
fn clip_half_plane(
    input: &[Point],
    output: &mut Vec<Point>,
    axis: usize,
    coord: f32,
    keep_above: bool,
) {
    output.clear();
    if input.is_empty() {
        return;
    }
    let inside = |p: &Point| {
        let v = if axis == 0 { p.x } else { p.y };
        if keep_above {
            v >= coord
        } else {
            v <= coord
        }
    };
    let mut previous = input[input.len() - 1];
    let mut previous_inside = inside(&previous);
    for &current in input {
        let current_inside = inside(&current);
        if current_inside != previous_inside {
            output.push(intersect(previous, current, axis, coord));
        }
        if current_inside {
            output.push(current);
        }
        previous = current;
        previous_inside = current_inside;
    }
}

/// Clips `input` to the band `low..high` on `axis`, into `output`, using
/// `swap` for the intermediate.
fn clip_band(
    input: &[Point],
    output: &mut Vec<Point>,
    swap: &mut Vec<Point>,
    axis: usize,
    low: f32,
    high: f32,
) {
    clip_half_plane(input, swap, axis, low, true);
    clip_half_plane(swap, output, axis, high, false);
}

/// Subdivides the triangles an op has just appended to `buffers`, in place,
/// on a grid of `cell_size` lyon units.
///
/// # This is the scanline split, restated as clipping
///
/// The reference walks, for each triangle, the cell ROWS it overlaps, builds
/// an ordered vertex ring for the intersection of the triangle with that
/// row, subdivides that ring along the Y-parallel axes and triangulates it
/// (`_subdivideTrianglesScanline`, `subdivision.ts:82`;
/// `_scanlineGenerateVertexRingForCellRow`, `:165`;
/// `scanlineTriangulateVertexRing`, `:954`). Most of its complexity is in
/// getting the ring's ORDER right through a dozen enumerated cases —
/// parallel edges, an edge lying on a boundary, a vertex exactly on a
/// boundary, which of the other two edges the inter-edge run should follow.
///
/// This does the same two-stage split — rows first, then columns within a
/// row — by convex clipping, which produces the ring in order by
/// construction and therefore has none of those cases. The output differs
/// from the reference's in exactly one way: the reference triangulates a
/// whole cell row at once, so a triangle may span several columns, where
/// this fans each CELL separately and emits a few more triangles along the
/// diagonal of a clipped shape. For a tile-covering polygon the two agree
/// exactly — `(g+1)^2` vertices and `2*g^2` triangles, the grid-mesh figures
/// of `create_tile_mesh.ts:121-131` — which is the case the memory budget is
/// measured on.
///
/// **Rows before columns is not a free choice, it is what keeps the mesh
/// watertight.** Two cells in the same row cut the SAME row polygon at their
/// shared column boundary, so both compute the identical crossing; two cells
/// in adjacent rows cut the SAME original triangle at their shared row
/// boundary, likewise. Clipping columns first would break the second of
/// those. Together with [`intersect`]'s endpoint ordering, that is the whole
/// argument that no grid line becomes a crack —
/// `subdividing_a_flat_tile_opens_no_crack` is the proof against the
/// rasterizer.
pub(crate) fn subdivide_fill_span<V: SubdivVertex>(
    buffers: &mut VertexBuffers<V, u32>,
    vertex_start: usize,
    index_start: usize,
    granularity: u32,
    cell_size: f32,
    scratch: &mut SubdivideScratch,
) {
    debug_assert!(cell_size > 0.0, "a cell must have a size");
    scratch.clear_for_op();
    scratch.input.extend_from_slice(&buffers.indices[index_start..]);
    if scratch.input.is_empty() {
        return;
    }
    let cell_of = |v: f32| (v / cell_size).floor() as i64;

    // Does anything in this op straddle a cell boundary at all? Answered
    // FIRST, and for a real cost: the dedupe seeding below is one hash
    // insert per vertex of the op, and at the granularity of 2 that z5
    // tiles get, almost every op in a dense tile lies wholly inside one
    // cell and needs neither the map nor the rewrite.
    //
    // Measured on an OpenFreeMap z5 tile (1,929 ops, 398,050 vertices,
    // Liberty style), whole-tile `tessellate_plan_with_ranges`, p50 of 7:
    //
    //   seeding unconditional   12.864 ms  against 11.033 ms unsubdivided  +16.6%
    //   with this check         11.237 ms  against 10.647 ms unsubdivided   +5.5%
    //
    // so it recovers **68%** of what subdivision cost that tile, and the
    // output is byte-identical either way (399,825 vertices both times).
    let mut splits_anything = false;
    for triangle in scratch.input.as_chunks::<3>().0.iter() {
        let [a, b, c] = [
            buffers.vertices[triangle[0] as usize].position(),
            buffers.vertices[triangle[1] as usize].position(),
            buffers.vertices[triangle[2] as usize].position(),
        ];
        let rows = cell_of(a[1].min(b[1]).min(c[1])) != cell_of(a[1].max(b[1]).max(c[1]));
        let cols = cell_of(a[0].min(b[0]).min(c[0])) != cell_of(a[0].max(b[0]).max(c[0]));
        if rows || cols {
            splits_anything = true;
            break;
        }
    }
    if !splits_anything {
        return;
    }

    // Seed the dedupe with the op's own vertices, so a clip that lands
    // exactly on an input vertex reuses it -- the reference's
    // `_initializeVertices` (`subdivision.ts:569-573`). Without it a
    // tile-covering quad's own four corners would be duplicated by the
    // cells that touch them.
    for (index, vertex) in buffers.vertices.iter().enumerate().skip(vertex_start) {
        let position = vertex.position();
        scratch.dedupe.entry(position_key(position)).or_insert(index as u32);
    }

    for triangle in scratch.input.as_chunks::<3>().0.iter() {
        let [ia, ib, ic] = [triangle[0], triangle[1], triangle[2]];
        let a = buffers.vertices[ia as usize].position();
        let b = buffers.vertices[ib as usize].position();
        let c = buffers.vertices[ic as usize].position();
        let min_x = a[0].min(b[0]).min(c[0]);
        let max_x = a[0].max(b[0]).max(c[0]);
        let min_y = a[1].min(b[1]).min(c[1]);
        let max_y = a[1].max(b[1]).max(c[1]);
        let (row_first, row_last) = (cell_of(min_y), cell_of(max_y));
        let (col_first, col_last) = (cell_of(min_x), cell_of(max_x));
        // Wholly inside one cell: pass it through untouched, indices and
        // all. The reference's identical short-circuit
        // (`subdivision.ts:142-145`), and the reason a dense z14 tile at
        // granularity 2 barely moves -- almost every triangle in it takes
        // this branch.
        //
        // The same branch also catches a runaway coordinate, which is a
        // hang and not a cosmetic problem -- see
        // `MAX_CELLS_PER_PRIMITIVE`.
        let budget = MAX_CELLS_PER_PRIMITIVE * i64::from(granularity);
        if (row_first == row_last && col_first == col_last)
            || row_last - row_first > budget
            || col_last - col_first > budget
        {
            scratch.output.extend_from_slice(triangle);
            continue;
        }
        // The op's own paint, carried onto every vertex the split creates.
        // Every vertex lyon's fill tessellator emits for one op is identical
        // but for its position (`FillCtor::new_vertex` writes the op's two
        // colours and zeroes for everything else), so the triangle's own
        // first vertex is an exact template.
        let template = buffers.vertices[ia as usize];
        let triangle_points =
            [lyon::math::point(a[0], a[1]), lyon::math::point(b[0], b[1]), lyon::math::point(c[0], c[1])];

        for row in row_first..=row_last {
            let low = row as f32 * cell_size;
            clip_band(&triangle_points, &mut scratch.row, &mut scratch.swap, 1, low, low + cell_size);
            if scratch.row.len() < 3 {
                continue;
            }
            let mut row_min_x = f32::INFINITY;
            let mut row_max_x = f32::NEG_INFINITY;
            for p in &scratch.row {
                row_min_x = row_min_x.min(p.x);
                row_max_x = row_max_x.max(p.x);
            }
            let (first, last) = (cell_of(row_min_x), cell_of(row_max_x));
            if first == last {
                // The row polygon already lies in one column, so clipping to
                // it would be a no-op. Fan it directly.
                emit_fan(buffers, &scratch.row, &template, &mut scratch.dedupe, &mut scratch.output);
                continue;
            }
            for col in first..=last {
                let low = col as f32 * cell_size;
                // `scratch.row` is the input here and `scratch.cell` the
                // output, so the two cannot be the same buffer -- which is
                // why there are three and not two.
                clip_band(&scratch.row, &mut scratch.cell, &mut scratch.swap, 0, low, low + cell_size);
                if scratch.cell.len() < 3 {
                    continue;
                }
                emit_fan(buffers, &scratch.cell, &template, &mut scratch.dedupe, &mut scratch.output);
            }
        }
    }

    buffers.indices.truncate(index_start);
    buffers.indices.extend_from_slice(&scratch.output);
}

/// A vertex [`subdivide_fill_span`] can split: a position, and a copy of
/// itself moved to another one (every other attribute carried over).
///
/// Generic so the same watertight grid split serves the fill mesh's
/// 88-byte [`Vertex`] and the extrusion roofs' bare lyon positions
/// (`render::extrusion`, whose roofs are cut BEFORE their positions are
/// rounded into the 12-byte `ExtrusionVertex`).
pub(crate) trait SubdivVertex: Copy {
    fn position(&self) -> [f32; 2];
    #[must_use]
    fn at(&self, position: [f32; 2]) -> Self;
}

impl SubdivVertex for Vertex {
    fn position(&self) -> [f32; 2] {
        self.position
    }
    fn at(&self, position: [f32; 2]) -> Self {
        Self { position, ..*self }
    }
}

impl SubdivVertex for [f32; 2] {
    fn position(&self) -> [f32; 2] {
        *self
    }
    fn at(&self, position: [f32; 2]) -> Self {
        position
    }
}

/// Fan-triangulates one convex polygon into `output`, interning its vertices
/// through `dedupe`.
fn emit_fan<V: SubdivVertex>(
    buffers: &mut VertexBuffers<V, u32>,
    polygon: &[Point],
    template: &V,
    dedupe: &mut HashMap<u64, u32>,
    output: &mut Vec<u32>,
) {
    let mut fan = [0u32; 3];
    for (offset, _) in polygon.iter().enumerate().skip(2) {
        for (slot, corner) in [0usize, offset - 1, offset].into_iter().enumerate() {
            let position = [polygon[corner].x, polygon[corner].y];
            fan[slot] = *dedupe.entry(position_key(position)).or_insert_with(|| {
                let index = buffers.vertices.len() as u32;
                buffers.vertices.push(template.at(position));
                index
            });
        }
        // A clip can collapse a corner onto its neighbour exactly; the
        // resulting zero-area triangle would draw nothing, so drop it rather
        // than carry three more indices for it.
        if fan[0] != fan[1] && fan[1] != fan[2] && fan[2] != fan[0] {
            output.extend_from_slice(&fan);
        }
    }
}

/// The dedupe key for a position: the two `f32` bit patterns, packed.
fn position_key(position: [f32; 2]) -> u64 {
    (u64::from(position[0].to_bits()) << 32) | u64::from(position[1].to_bits())
}

/// Grows every triangle edge that lies exactly on the tile's north or south
/// edge into a quad reaching the pole, closing the sphere.
///
/// `_fillPoles`/`_generatePoleQuad` (`subdivision.ts:523,491`), and the same
/// precondition: it must only run on a tile that borders the pole, and it
/// assumes the geometry has been clipped to `0..extent`.
///
/// # The pole vertex sits ON the tile edge, and that is the whole trick
///
/// The reference puts its pole vertices at a far-away sentinel coordinate
/// (`NORTH_POLE_Y = -32768`), which in the flat projection is geometry four
/// tiles off the top of the map. It then has to HIDE that: its
/// `interpolateProjection` pushes a pole vertex's clip `z` to 100.0 until
/// the last 2% of the transition (`_projection_globe.vertex.glsl:124-128`),
/// which is a hard reveal — the caps pop in.
///
/// Here the pole vertex keeps the `x` AND the `y` of the tile-edge vertex it
/// grew from, and is marked a pole only by [`POLE_NORTH_MARKER`] in
/// `is_stroke`. So:
///
///  - flat, the quad is exactly degenerate — both new vertices coincide with
///    the two they were grown from — and rasterizes to nothing, with no
///    special case anywhere in the shader. That is what keeps
///    `transition == 0` pixel-identical;
///  - on the globe, `vs_main` replaces the sphere position with
///    `(0, +/-1, 0)` and the quad is the cap;
///  - and THROUGH the transition, `interpolate_projection`'s own
///    `mix(flat, globe, transition)` grows each cap continuously out of a
///    zero-area sliver. **This is why the reference's pole fade
///    (`:124-128`) is deliberately not ported: there is nothing to fade.**
fn append_pole_caps(
    buffers: &mut VertexBuffers<Vertex, u32>,
    index_start: usize,
    north: bool,
    south: bool,
    south_edge_y: f32,
    scratch: &mut SubdivideScratch,
) {
    // Captured before the loop, so the quads this appends are not themselves
    // scanned for edges to grow -- the reference reads `indices.length` once
    // for the same reason (`subdivision.ts:529`).
    let scanned = buffers.indices.len();
    let mut triangle = index_start;
    while triangle + 2 < scanned {
        let corners = [
            buffers.indices[triangle],
            buffers.indices[triangle + 1],
            buffers.indices[triangle + 2],
        ];
        triangle += 3;
        let ys = corners.map(|index| buffers.vertices[index as usize].position[1]);
        for side in 0..3 {
            let (a, b) = (corners[side], corners[(side + 1) % 3]);
            let (ya, yb) = (ys[side], ys[(side + 1) % 3]);
            if north && ya == 0.0 && yb == 0.0 {
                grow_pole_quad(buffers, a, b, POLE_NORTH_MARKER, &mut scratch.pole_dedupe);
            }
            if south && ya == south_edge_y && yb == south_edge_y {
                grow_pole_quad(buffers, a, b, POLE_SOUTH_MARKER, &mut scratch.pole_dedupe);
            }
        }
    }
}

/// One quad from the edge `i0 -> i1` to `marker`'s pole.
///
/// `_generatePoleQuad` (`subdivision.ts:491-509`), with its winding flip.
/// The edge `i0 -> i1` comes from a fill triangle whose interior is on the
/// tile's side of the edge, and the cap grows on the other side, so the
/// quad is wound the other way round to face the same way as that
/// triangle: `fs_main` drops a globe fill that faces away (see
/// [`orient_fill_triangles`]).
fn grow_pole_quad(
    buffers: &mut VertexBuffers<Vertex, u32>,
    i0: u32,
    i1: u32,
    marker: f32,
    pole_dedupe: &mut HashMap<u64, u32>,
) {
    let mut pole_of = |buffers: &mut VertexBuffers<Vertex, u32>, from: u32| -> u32 {
        // Keyed on `x` and which pole, NOT on the whole position: a pole
        // vertex shares its position with the edge vertex it grew from, so a
        // position key would fold the two together.
        let key = (u64::from(buffers.vertices[from as usize].position[0].to_bits()) << 1)
            | u64::from(marker == POLE_SOUTH_MARKER);
        if let Some(existing) = pole_dedupe.get(&key) {
            return *existing;
        }
        let index = buffers.vertices.len() as u32;
        let mut vertex = buffers.vertices[from as usize];
        vertex.is_stroke = marker;
        buffers.vertices.push(vertex);
        pole_dedupe.insert(key, index);
        index
    };
    let p0 = pole_of(buffers, i0);
    let p1 = pole_of(buffers, i1);
    if p0 == p1 {
        // A zero-length edge along the tile boundary: no quad to grow.
        return;
    }
    buffers.indices.extend_from_slice(&[i1, i0, p0, p1, i1, p0]);
}

/// Gives every fill triangle from `index_start` on the same winding:
/// [`FILL_WINDING_SIGN`] for its signed area in tile space.
///
/// # Why a fill's winding is now load-bearing
///
/// On the globe, `shader.wgsl`'s `fs_main` drops a fill fragment that faces
/// away from the camera. The clipping plane alone cannot hide the far side
/// at the limb: a chord triangle that straddles the horizon plane keeps the
/// part of it on the near side, and when that triangle faces away it lies
/// under a near-side triangle that covers the same pixels. A translucent
/// fill then composites there twice, which is the pale rim
/// `a_translucent_fill_is_composited_exactly_once_per_pixel_on_the_globe`
/// grades. Depth does not stop it: the far triangle wins whenever it is
/// drawn first.
///
/// lyon does not document the winding of what it emits (see
/// `render::extrusion`, which normalises its roofs for the same reason), so
/// it is set here rather than assumed. Every fill passes through
/// [`subdivide_fill`], with or without a grid, and the grid split and the
/// pole caps both keep the winding they are given. A zero-area triangle is
/// left alone; it draws nothing.
fn orient_fill_triangles(buffers: &mut VertexBuffers<Vertex, u32>, index_start: usize) {
    let VertexBuffers { vertices, indices } = buffers;
    for triangle in indices[index_start..].as_chunks_mut::<3>().0 {
        let [a, b, c] = triangle.map(|i| vertices[i as usize].position);
        let cross = (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]);
        if cross * FILL_WINDING_SIGN < 0.0 {
            triangle.swap(1, 2);
        }
    }
}

/// The sign of a fill triangle's tile-space signed area after
/// [`orient_fill_triangles`]: the winding the GPU reports as front-facing
/// for a near-side triangle on the globe.
pub(crate) const FILL_WINDING_SIGN: f32 = -1.0;

/// Everything one fill op needs after lyon has written its triangles: one
/// winding, the grid split, then the pole caps.
///
/// Poles AFTER subdivision, not before, because the split is what turns one
/// long edge along the tile boundary into `granularity` short ones — and a
/// cap grown from one long edge would be a single triangle from a chord,
/// which is the faceted limb this module exists to remove, pointing at the
/// pole instead of along it.
pub(crate) fn subdivide_fill(
    buffers: &mut VertexBuffers<Vertex, u32>,
    vertex_start: usize,
    index_start: usize,
    subdivision: Subdivision,
    extent: u32,
    coord_scale: f32,
    scratch: &mut SubdivideScratch,
) {
    orient_fill_triangles(buffers, index_start);
    if subdivision.is_none() {
        return;
    }
    subdivide_fill_span(
        buffers,
        vertex_start,
        index_start,
        subdivision.granularity,
        subdivision.cell_size(extent, coord_scale),
        scratch,
    );
    if subdivision.north_pole || subdivision.south_pole {
        append_pole_caps(
            buffers,
            index_start,
            subdivision.north_pole,
            subdivision.south_pole,
            extent as f32 * coord_scale,
            scratch,
        );
    }
}

/// How many points [`subdivide_line`] will insert into this line string.
///
/// Exists so `mesh::estimate_buffer_capacity` stays an upper bound on what
/// the tessellator is about to write. That estimate is not a
/// micro-optimisation: it feeds one `reserve_exact` whose absence
/// reintroduces a multi-gigabyte RSS leak (see `mesh::reserve_for_ops`), and
/// subdivision adds points the per-point estimate knows nothing about.
///
/// Exact, and cheap: the number of grid lines a segment crosses on each axis
/// is a difference of two `floor`s, and the estimate already walks the same
/// coordinates to count them.
///
/// **This bound covers LINES only, and fills are deliberately not bounded**
/// — the cell count a triangle soup lands in is not a function of the
/// polygon's point count, and a sound bound over an op's bounding box
/// (`triangles * rows * cols`) would over-reserve by more than the growth it
/// is protecting against, and every over-reserved byte stays resident for
/// the mesh's life. What makes that safe is [`granularity_for_zoom`]'s
/// `min = 1`: fills grow past the reservation only at z5 and shallower,
/// where a whole tile's mesh is small enough that `libmalloc` recycles the
/// one or two reallocations (measured at "a deliberately crude 2,000,000
/// vertex reservation against the same 2,486,864-vertex mesh was already
/// flat", `mesh::reserve_for_ops`), while every z6-and-deeper tile — every
/// dense tile, every 60 MB mesh — gets `Subdivision::NONE` and so is
/// unchanged. The measured low-zoom figures are in the task report and in
/// `subdivision_mesh_cost_per_tile_is_what_the_report_claims`.
#[must_use]
pub(crate) fn line_crossings(
    line: &geo_types::LineString<i32>,
    subdivision: Subdivision,
    extent: u32,
    coord_scale: f32,
) -> usize {
    if subdivision.is_none() {
        return 0;
    }
    let cell_size = f64::from(subdivision.cell_size(extent, coord_scale));
    // The same cap the split itself applies, or the estimate would be an
    // upper bound on work the split never does -- see
    // `MAX_CELLS_PER_PRIMITIVE`.
    let budget = (MAX_CELLS_PER_PRIMITIVE * i64::from(subdivision.granularity)) as f64;
    let mut total = 0usize;
    for window in line.0.windows(2) {
        for axis in 0..2 {
            let (from, to) = if axis == 0 {
                (window[0].x, window[1].x)
            } else {
                (window[0].y, window[1].y)
            };
            let scaled = |v: i32| (f64::from(v) * f64::from(coord_scale) / cell_size).floor();
            let crossings = (scaled(to) - scaled(from)).abs();
            if crossings > budget {
                continue;
            }
            total += crossings as usize;
        }
    }
    total
}

/// Inserts a vertex wherever a polyline crosses a grid line, in place.
///
/// `subdivideVertexLine` (`subdivision.ts:757`). Ours runs on the points
/// BEFORE lyon's stroke tessellator sees them, which is the same position in
/// the pipeline as the reference's (it subdivides the line layer's input
/// geometry, not its extruded quads) and the only position that can work: a
/// stroke's extrusion direction lives in `Vertex::normal` and is applied in
/// the vertex shader, so a grid cut through an already-extruded ribbon would
/// have to invent normals for the interior points and would narrow the
/// ribbon where it cut.
///
/// The inserted points are exactly collinear with the segment they split, so
/// the ribbon lyon produces is geometrically identical — every join it adds
/// at one of them is a straight join. What changes is the vertex count, and
/// on the globe the fact that each of those points is projected onto the
/// sphere individually instead of being interpolated across a chord.
pub(crate) fn subdivide_line(
    points: &mut Vec<Point>,
    subdivision: Subdivision,
    extent: u32,
    coord_scale: f32,
    scratch: &mut SubdivideScratch,
) {
    if subdivision.is_none() || points.len() < 2 {
        return;
    }
    let cell_size = subdivision.cell_size(extent, coord_scale);
    let crossings = &mut scratch.line;
    crossings.clear();
    crossings.reserve(points.len());
    for window in points.windows(2) {
        let (a, b) = (window[0], window[1]);
        crossings.push(a);
        // The grid lines strictly between the two endpoints, on each axis,
        // in order along the segment. Collected as points rather than as
        // parameters so `intersect`'s canonical endpoint ordering is the one
        // thing deciding each coordinate -- see its doc comment.
        let first = crossings.len();
        let budget = MAX_CELLS_PER_PRIMITIVE * i64::from(subdivision.granularity);
        for axis in 0..2 {
            let (from, to) = if axis == 0 { (a.x, b.x) } else { (a.y, b.y) };
            let (low, high) = if from <= to { (from, to) } else { (to, from) };
            let first_line = (low / cell_size).floor() as i64 + 1;
            let last_line = (high / cell_size).ceil() as i64;
            // A segment spanning more cells than any real tile buffer can
            // is left alone rather than split into millions of points -- see
            // `MAX_CELLS_PER_PRIMITIVE`.
            if last_line - first_line > budget {
                continue;
            }
            let mut line = first_line;
            while (line as f32) * cell_size < high {
                crossings.push(intersect(a, b, axis, line as f32 * cell_size));
                line += 1;
            }
        }
        // Along the segment, so the polyline stays a polyline. Sorted on the
        // dominant axis, whose ordering is strict for every crossing this
        // pushed.
        let ascending = if (b.x - a.x).abs() >= (b.y - a.y).abs() {
            b.x >= a.x
        } else {
            b.y >= a.y
        };
        let dominant_x = (b.x - a.x).abs() >= (b.y - a.y).abs();
        crossings[first..].sort_by(|p, q| {
            let (p, q) = if dominant_x { (p.x, q.x) } else { (p.y, q.y) };
            if ascending {
                p.partial_cmp(&q).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                q.partial_cmp(&p).unwrap_or(std::cmp::Ordering::Equal)
            }
        });
    }
    crossings.push(points[points.len() - 1]);
    // A crossing that coincides with a point already in the line would be a
    // zero-length segment, which lyon's stroke tessellator merges anyway;
    // dropping it here keeps the vertex count honest.
    points.clear();
    for point in crossings.iter() {
        if points.last() != Some(point) {
            points.push(*point);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::mesh::COORD_SCALE;

    const EXTENT: u32 = 4096;

    fn fill_vertex(x: f32, y: f32) -> Vertex {
        Vertex {
            position: [x, y],
            normal: [0.0, 0.0],
            color: [1.0, 0.0, 0.0, 1.0],
            is_stroke: 0.0,
            half_width_px: 0.0,
            color_to: [1.0, 0.0, 0.0, 1.0],
            half_width_px_to: 0.0,
            gap_half_px: 0.0,
            gap_half_px_to: 0.0,
            line_distance: 0.0,
            dash_runs: [0.0; 4],
            line_progress: -1.0,
        }
    }

    /// One triangle, in lyon units, as a fresh `VertexBuffers`.
    fn triangle(points: [(f32, f32); 3]) -> VertexBuffers<Vertex, u32> {
        let mut buffers = VertexBuffers::new();
        for (x, y) in points {
            buffers.vertices.push(fill_vertex(x, y));
        }
        buffers.indices.extend_from_slice(&[0, 1, 2]);
        buffers
    }

    fn area(buffers: &VertexBuffers<Vertex, u32>) -> f64 {
        buffers
            .indices
            .as_chunks::<3>().0.iter()
            .map(|t| {
                let p = |i: u32| {
                    let v = buffers.vertices[i as usize].position;
                    (f64::from(v[0]), f64::from(v[1]))
                };
                let (ax, ay) = p(t[0]);
                let (bx, by) = p(t[1]);
                let (cx, cy) = p(t[2]);
                ((bx - ax) * (cy - ay) - (cx - ax) * (by - ay)).abs() / 2.0
            })
            .sum()
    }

    /// Whether `point` is inside (or on) the triangle `t`, to `slack` lyon
    /// units.
    fn inside(point: [f32; 2], t: [(f32, f32); 3], slack: f64) -> bool {
        let (px, py) = (f64::from(point[0]), f64::from(point[1]));
        let sign = |(ax, ay): (f32, f32), (bx, by): (f32, f32)| {
            let (ax, ay, bx, by) = (f64::from(ax), f64::from(ay), f64::from(bx), f64::from(by));
            let length = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt().max(1.0);
            ((bx - ax) * (py - ay) - (px - ax) * (by - ay)) / length
        };
        let d = [sign(t[0], t[1]), sign(t[1], t[2]), sign(t[2], t[0])];
        d.iter().all(|v| *v >= -slack) || d.iter().all(|v| *v <= slack)
    }

    /// **The granularity table, and that it is a constant angular cell.**
    ///
    /// The whole justification for `BASE_GRANULARITY` being 64 rather than
    /// the reference's 128 is a pixel figure derived from the cell's angular
    /// size, so the angular size is what gets pinned -- an off-by-one in the
    /// shift would otherwise change the visual claim silently.
    #[test]
    fn the_granularity_table_is_one_constant_angular_cell_across_the_globe_band() {
        assert_eq!(
            (0..=8).map(granularity_for_zoom).collect::<Vec<_>>(),
            vec![64, 32, 16, 8, 4, 2, 1, 1, 1],
        );
        for z in 0..=6u8 {
            let degrees_per_tile = 360.0 / f64::from(1u32 << z);
            let degrees_per_cell = degrees_per_tile / f64::from(granularity_for_zoom(z));
            assert!(
                (degrees_per_cell - 5.625).abs() < 1e-12,
                "z{z}: a cell must span 360/{BASE_GRANULARITY} degrees at every level in the \
                 globe band, got {degrees_per_cell}"
            );
        }
        // The limb error this buys, which is the number the constant's doc
        // comment quotes. `r * (1 - cos(alpha/2))` at 5.625 degrees.
        let alpha = 5.625f64.to_radians();
        let error_per_px = 1.0 - (alpha / 2.0).cos();
        for (radius, expected) in [(161.0, 0.19), (379.0, 0.46), (1500.0, 1.81)] {
            let error = radius * error_per_px;
            assert!(
                (error - expected).abs() < 0.005,
                "a {radius} px globe's limb departs from the circle by {error} px, not {expected}"
            );
        }
    }

    #[test]
    fn the_pole_rows_are_the_only_ones_that_get_caps() {
        let at = |z, y| Subdivision::for_tile(TileId { z, x: 0, y });
        assert_eq!(
            at(0, 0),
            Subdivision { granularity: 64, north_pole: true, south_pole: true },
            "the z0 tile is both pole rows at once"
        );
        assert_eq!(at(2, 0), Subdivision { granularity: 16, north_pole: true, south_pole: false });
        assert_eq!(at(2, 1), Subdivision { granularity: 16, north_pole: false, south_pole: false });
        assert_eq!(at(2, 3), Subdivision { granularity: 16, north_pole: false, south_pole: true });
        // A z14 tile on the north row is STILL nothing to do, poles included.
        // `granularity_for_zoom(14)` is 1, `is_none` is therefore true, and
        // `subdivide_fill` returns before it reaches `append_pole_caps`.
        // That is deliberate: a cap grown from a z14 pole tile's top edge
        // spans the same 85.051-to-90 degrees a z0 cap does, out of a tile
        // 0.0022 degrees wide, so it is a spike rather than a cap -- and the
        // only camera that can see it is one already showing the hole the
        // Mercator grid leaves there. `granularity_for_zoom(14)` being 1 is
        // what skips it, and that is the behaviour both the flat and the
        // spherical projection want.
        let deep_pole = Subdivision::for_tile(TileId { z: 14, x: 0, y: 0 });
        assert_eq!(
            deep_pole,
            Subdivision { granularity: 1, north_pole: true, south_pole: false }
        );
        assert!(deep_pole.is_none(), "a z14 pole tile has nothing to subdivide and no cap to grow");
    }

    /// **The core acceptance test for the split: it multiplies triangles,
    /// moves no geometry, and preserves area.**
    ///
    /// The plan's Step 1 for this task, with the tolerances made explicit.
    /// A triangle covering half the tile at granularity 8 must come back as
    /// a grid-clipped soup: at least 64 triangles (the reference's own
    /// `g^2 * 2` for a full cover is 128, and a half-tile triangle gets
    /// about half of it), every vertex inside the input triangle, and the
    /// same area.
    #[test]
    fn subdivision_multiplies_triangles_without_moving_or_losing_geometry() {
        let span = EXTENT as f32 * COORD_SCALE;
        let corners = [(0.0, 0.0), (span, 0.0), (span, span)];
        let mut buffers = triangle(corners);
        let before = area(&buffers);
        let before_triangles = buffers.indices.len() / 3;
        let mut scratch = SubdivideScratch::default();
        subdivide_fill_span(&mut buffers, 0, 0, 8, span / 8.0, &mut scratch);
        let after = area(&buffers);
        let after_triangles = buffers.indices.len() / 3;

        assert_eq!(before_triangles, 1);
        assert!(
            after_triangles >= 64,
            "a triangle covering half a tile at granularity 8 must be cut into at least 64 \
             triangles, got {after_triangles}"
        );
        // Relative, because the absolute area here is 8.4e12 lyon units^2
        // and an f32 coordinate cannot carry 1e-6 of that.
        assert!(
            ((after - before) / before).abs() < 1e-6,
            "subdivision must preserve area: {before} -> {after}"
        );
        // Half a lyon unit -- a thousandth of an MVT unit, and a hundred
        // times tighter than the rounding the reference applies.
        for vertex in &buffers.vertices {
            assert!(
                inside(vertex.position, corners, 0.5),
                "subdivision moved a vertex outside its own triangle: {:?}",
                vertex.position
            );
        }
    }

    /// **A tile-covering quad subdivides to exactly the reference's grid
    /// mesh**, which is the figure every memory number in this module's docs
    /// and in the task report is computed from: `v = (g+1)^2`,
    /// `i = g^2 * 6` (`create_tile_mesh.ts:121-131`).
    ///
    /// Getting `(g+1)^2` rather than `4 * g^2` is entirely down to the
    /// dedupe map: four cells meet at every interior grid corner.
    #[test]
    fn a_tile_covering_quad_subdivides_to_exactly_the_reference_grid_mesh() {
        let span = EXTENT as f32 * COORD_SCALE;
        for granularity in [2u32, 4, 8, 16, 64] {
            let mut buffers = VertexBuffers::new();
            for (x, y) in [(0.0, 0.0), (span, 0.0), (span, span), (0.0, span)] {
                buffers.vertices.push(fill_vertex(x, y));
            }
            buffers.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
            let mut scratch = SubdivideScratch::default();
            subdivide_fill_span(&mut buffers, 0, 0, granularity, span / granularity as f32, &mut scratch);
            assert_eq!(
                buffers.vertices.len(),
                ((granularity + 1) * (granularity + 1)) as usize,
                "granularity {granularity}: (g+1)^2 vertices"
            );
            assert_eq!(
                buffers.indices.len(),
                (granularity * granularity * 6) as usize,
                "granularity {granularity}: g^2 * 6 indices"
            );
        }
    }

    /// **No grid line is a crack.** Two triangles sharing a diagonal are
    /// subdivided independently; every vertex either side of a shared grid
    /// line must come out at the IDENTICAL `f32`, or the mesh has a sliver
    /// of background along every cell boundary. See [`intersect`].
    ///
    /// Checked structurally rather than by pixel here (the pixel proof is
    /// `subdividing_a_flat_tile_opens_no_crack` in `render::gpu`):
    /// the subdivided quad's total area must equal the undivided one's
    /// exactly, and every edge of the output must be shared by exactly two
    /// triangles or lie on the quad's boundary.
    #[test]
    fn subdivision_leaves_no_crack_between_cells() {
        let span = EXTENT as f32 * COORD_SCALE;
        let mut buffers = VertexBuffers::new();
        for (x, y) in [(0.0, 0.0), (span, 0.0), (span, span), (0.0, span)] {
            buffers.vertices.push(fill_vertex(x, y));
        }
        buffers.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
        let before = area(&buffers);
        let mut scratch = SubdivideScratch::default();
        subdivide_fill_span(&mut buffers, 0, 0, 8, span / 8.0, &mut scratch);
        assert!(
            ((area(&buffers) - before) / before).abs() < 1e-6,
            "a watertight subdivision has the same area as its input"
        );

        let mut edges: HashMap<(u32, u32), usize> = HashMap::new();
        for t in buffers.indices.as_chunks::<3>().0.iter() {
            for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                *edges.entry((a.min(b), a.max(b))).or_default() += 1;
            }
        }
        let boundary = |index: u32| {
            let p = buffers.vertices[index as usize].position;
            p[0] == 0.0 || p[1] == 0.0 || p[0] == span || p[1] == span
        };
        for ((a, b), count) in &edges {
            if *count == 2 {
                continue;
            }
            assert!(
                *count == 1 && boundary(*a) && boundary(*b),
                "an interior edge shared by {count} triangles is a crack: {:?} -> {:?}",
                buffers.vertices[*a as usize].position,
                buffers.vertices[*b as usize].position
            );
        }
    }

    /// **The pole cap is degenerate in tile space and marked in
    /// `is_stroke`** -- the two properties that let it be emitted
    /// unconditionally without changing one flat pixel. See
    /// [`append_pole_caps`].
    #[test]
    fn a_pole_cap_grows_from_the_tile_edge_and_is_flat_degenerate() {
        let span = EXTENT as f32 * COORD_SCALE;
        // A quad covering the whole tile, so it has an edge on both the
        // north (y = 0) and the south (y = span) boundary.
        let mut buffers = VertexBuffers::new();
        for (x, y) in [(0.0, 0.0), (span, 0.0), (span, span), (0.0, span)] {
            buffers.vertices.push(fill_vertex(x, y));
        }
        buffers.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
        let mut scratch = SubdivideScratch::default();
        let subdivision = Subdivision { granularity: 4, north_pole: true, south_pole: true };
        subdivide_fill(&mut buffers, 0, 0, subdivision, EXTENT, COORD_SCALE, &mut scratch);

        let poles: Vec<&Vertex> =
            buffers.vertices.iter().filter(|v| v.is_stroke < -0.5).collect();
        // Five grid columns along each edge -> five pole vertices per pole.
        assert_eq!(poles.len(), 10, "one pole vertex per grid column per pole");
        assert_eq!(poles.iter().filter(|v| v.is_stroke == POLE_NORTH_MARKER).count(), 5);
        assert_eq!(poles.iter().filter(|v| v.is_stroke == POLE_SOUTH_MARKER).count(), 5);
        for vertex in &poles {
            let on_edge = vertex.position[1] == 0.0 || vertex.position[1] == span;
            assert!(
                on_edge,
                "a pole vertex must keep the tile-edge y it grew from, so the cap is degenerate \
                 when drawn flat; got {:?}",
                vertex.position
            );
        }
        // Eight cap quads (four columns, two poles), two triangles each, on
        // top of the 4x4 grid's 32.
        assert_eq!(buffers.indices.len() / 3, 32 + 8 * 2);
        // And the flat area is unchanged, because every cap triangle has
        // zero area in tile space.
        assert!(
            ((area(&buffers) - f64::from(span) * f64::from(span)) / (f64::from(span) * f64::from(span)))
                .abs()
                < 1e-6,
            "the caps must contribute no tile-space area"
        );
    }

    /// **The per-tile mesh cost at the granularity this module chose**, as
    /// reported for Task 5, pinned so the report cannot drift from the code.
    ///
    /// The worst realistic low-zoom fill is one polygon covering the whole
    /// tile (an ocean, a landcover band), which subdivides to exactly the
    /// reference's grid mesh — `v = (g+1)^2`, `i = g^2 * 6`
    /// (`create_tile_mesh.ts:121-131`). At 88 bytes per [`Vertex`] and 4 per
    /// `u32` index that is:
    ///
    /// ```text
    ///   z   g    vertices   indices   vertex bytes   total bytes
    ///   0  64       4,225    24,576        371,800       470,104
    ///   1  32       1,089     6,144         95,832       120,408
    ///   2  16         289     1,536         25,432        31,576
    ///   3   8          81       384          7,128         8,664
    ///   4   4          25        96          2,200         2,584
    ///   5   2           9        24            792           888
    ///   6   1           4         6            352           376
    /// ```
    ///
    /// **0.35 MB of vertices for the z0 tile, against the 1.46 MB the plan
    /// quotes for MapLibre's `fill` base of 128** — the 4x this module's
    /// [`BASE_GRANULARITY`] buys, and the reason a granularity below the
    /// reference's is the right call while `Vertex` is still 88 bytes.
    ///
    /// Real tiles, for scale (OpenFreeMap planet z0/z1/z2/z5, Liberty style,
    /// whole-tile mesh `footprint_bytes`, measured):
    ///
    /// ```text
    ///   tile          unsubdivided   subdivided   ratio
    ///   z0/0/0            1.61 MB      7.77 MB     4.8x
    ///   z1/0/0            0.61 MB      2.68 MB     4.4x
    ///   z2/1/1            1.04 MB      2.42 MB     2.3x
    ///   z5/16/10         47.52 MB     47.79 MB    1.006x
    ///   z14 (fixture)     2.52 MB      2.52 MB     1.000x  (granularity 1)
    /// ```
    #[test]
    fn subdivision_mesh_cost_per_tile_is_what_the_report_claims() {
        let span = EXTENT as f32 * COORD_SCALE;
        let expected: [(u8, u32, usize, usize, usize); 7] = [
            // Bytes at `Vertex`'s 92 (88 before `line_progress`).
            (0, 64, 4_225, 24_576, 487_004),
            (1, 32, 1_089, 6_144, 124_764),
            (2, 16, 289, 1_536, 32_732),
            (3, 8, 81, 384, 8_988),
            (4, 4, 25, 96, 2_684),
            (5, 2, 9, 24, 924),
            (6, 1, 4, 6, 392),
        ];
        for (z, granularity, vertices, indices, bytes) in expected {
            assert_eq!(granularity_for_zoom(z), granularity, "z{z}");
            let mut buffers = VertexBuffers::new();
            for (x, y) in [(0.0, 0.0), (span, 0.0), (span, span), (0.0, span)] {
                buffers.vertices.push(fill_vertex(x, y));
            }
            buffers.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
            let mut scratch = SubdivideScratch::default();
            subdivide_fill(
                &mut buffers,
                0,
                0,
                Subdivision { granularity, north_pole: false, south_pole: false },
                EXTENT,
                COORD_SCALE,
                &mut scratch,
            );
            assert_eq!(buffers.vertices.len(), vertices, "z{z} vertices");
            assert_eq!(buffers.indices.len(), indices, "z{z} indices");
            let measured = buffers.vertices.len() * std::mem::size_of::<Vertex>()
                + buffers.indices.len() * std::mem::size_of::<u32>();
            assert_eq!(measured, bytes, "z{z} bytes");
        }
        // And the headline comparison with the reference's own base, which
        // is the whole justification for `BASE_GRANULARITY`: 92 bytes a
        // vertex (88 before `line_progress`), `(g+1)^2` vertices.
        assert_eq!(65 * 65 * std::mem::size_of::<Vertex>(), 388_700);
        assert_eq!(129 * 129 * std::mem::size_of::<Vertex>(), 1_530_972);
    }

    #[test]
    fn a_tile_that_borders_no_pole_gets_no_cap() {
        let span = EXTENT as f32 * COORD_SCALE;
        let mut buffers = triangle([(0.0, 0.0), (span, 0.0), (span, span)]);
        let mut scratch = SubdivideScratch::default();
        subdivide_fill(&mut buffers, 0, 0, Subdivision { granularity: 4, north_pole: false, south_pole: false }, EXTENT, COORD_SCALE, &mut scratch);
        assert!(buffers.vertices.iter().all(|v| v.is_stroke == 0.0));
    }

    /// **`Subdivision::NONE` is a no-op, byte for byte.** The guarantee that
    /// z6 and deeper -- every dense tile, every 60 MB mesh -- is untouched.
    /// The overlay grid is the cell its doc comment prices, and the
    /// extent that produces it survives the `u32` it travels in.
    #[test]
    fn the_overlay_grid_is_a_z10_tile_and_its_extent_fits() {
        let cell = Subdivision::OVERLAY
            .cell_size(OVERLAY_SUBDIVISION_EXTENT, crate::render::mesh::OVERLAY_COORD_SCALE);
        assert_eq!(f64::from(cell), OVERLAY_CELL_LYON_UNITS, "the cell must be exact");
        let world = crate::render::camera::WORLD_SIZE as f64 * crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;
        assert_eq!(world / OVERLAY_CELL_LYON_UNITS, 1024.0);
        assert!(
            u64::from(OVERLAY_SUBDIVISION_EXTENT) == (OVERLAY_CELL_LYON_UNITS * 512.0) as u64,
            "the extent overflowed its u32"
        );
    }

    /// Every fill triangle leaves [`subdivide_fill`] in one winding,
    /// whichever it arrived in, with or without a grid. `fs_main` drops a
    /// globe fill that faces away, so a triangle in the other winding is a
    /// hole. A pole cap is flat-degenerate here (zero area), so its winding
    /// is graded on the GPU instead, by the pole cameras of
    /// `a_translucent_fill_is_composited_exactly_once_per_pixel_on_the_globe`.
    #[test]
    fn every_fill_triangle_and_pole_cap_leaves_in_one_winding() {
        let span = EXTENT as f32 * COORD_SCALE;
        let signed = |buffers: &VertexBuffers<Vertex, u32>, t: &[u32; 3]| {
            let p = |i: u32| buffers.vertices[i as usize].position;
            let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
            (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])
        };
        for subdivision in [
            Subdivision::NONE,
            Subdivision { granularity: 4, north_pole: true, south_pole: true },
        ] {
            for ring in [
                [(0.0, 0.0), (span, 0.0), (span, span)],
                [(0.0, 0.0), (span, span), (span, 0.0)],
                [(0.0, span), (span, span), (0.0, 0.0)],
            ] {
                let mut buffers = triangle(ring);
                let mut scratch = SubdivideScratch::default();
                subdivide_fill(&mut buffers, 0, 0, subdivision, EXTENT, COORD_SCALE, &mut scratch);
                for t in buffers.indices.as_chunks::<3>().0 {
                    let area = signed(&buffers, t);
                    if area != 0.0 {
                        assert!(area * FILL_WINDING_SIGN > 0.0, "{subdivision:?} {ring:?}: {t:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn no_subdivision_changes_nothing() {
        let span = EXTENT as f32 * COORD_SCALE;
        // Already in the bake's winding, so only the grid could move it.
        let mut buffers = triangle([(0.0, 0.0), (span, span), (span, 0.0)]);
        let before = (buffers.vertices.clone(), buffers.indices.clone());
        let mut scratch = SubdivideScratch::default();
        subdivide_fill(&mut buffers, 0, 0, Subdivision::NONE, EXTENT, COORD_SCALE, &mut scratch);
        assert_eq!((buffers.vertices, buffers.indices), before);

        let mut points = vec![lyon::math::point(0.0, 0.0), lyon::math::point(span, span)];
        let expected = points.clone();
        subdivide_line(&mut points, Subdivision::NONE, EXTENT, COORD_SCALE, &mut scratch);
        assert_eq!(points, expected);
    }

    /// **A line gains a point at every grid crossing, in order, collinear.**
    ///
    /// Order is the assertion that matters: a polyline whose inserted points
    /// are out of order is not a longer version of the same line, it is a
    /// zig-zag, and lyon would happily extrude the zig-zag.
    #[test]
    fn a_subdivided_line_is_the_same_line_with_more_points_in_order() {
        let span = EXTENT as f32 * COORD_SCALE;
        let mut scratch = SubdivideScratch::default();
        let subdivision = Subdivision { granularity: 8, north_pole: false, south_pole: false };
        // A diagonal crosses seven interior x lines and seven y lines, and
        // at granularity 8 on the diagonal they coincide, so seven points.
        let mut diagonal = vec![lyon::math::point(0.0, 0.0), lyon::math::point(span, span)];
        subdivide_line(&mut diagonal, subdivision, EXTENT, COORD_SCALE, &mut scratch);
        assert_eq!(diagonal.len(), 9, "two endpoints and seven crossings");
        for pair in diagonal.windows(2) {
            assert!(pair[1].x > pair[0].x, "the points must stay in order along the line");
            assert!((pair[1].x - pair[1].y).abs() < 1.0, "and stay on the line");
        }

        // A shallow line, where the x and y grids do NOT coincide: 7 x
        // crossings plus 1 y crossing.
        let mut shallow =
            vec![lyon::math::point(0.0, 0.0), lyon::math::point(span, span / 8.0 + 1.0)];
        subdivide_line(&mut shallow, subdivision, EXTENT, COORD_SCALE, &mut scratch);
        assert_eq!(shallow.len(), 2 + 7 + 1);
        for pair in shallow.windows(2) {
            assert!(pair[1].x > pair[0].x, "in order");
        }
        // Reversed, the same set of points must come back in the reverse
        // order and at the identical coordinates -- the crack argument for
        // lines.
        let mut reversed =
            vec![lyon::math::point(span, span / 8.0 + 1.0), lyon::math::point(0.0, 0.0)];
        subdivide_line(&mut reversed, subdivision, EXTENT, COORD_SCALE, &mut scratch);
        reversed.reverse();
        assert_eq!(reversed, shallow);
    }

    /// **[`line_crossings`] is an upper bound on what [`subdivide_line`]
    /// inserts**, which is the only property `mesh::estimate_buffer_capacity`
    /// needs of it -- an estimate that undershoots is a reallocation, and
    /// `mesh::reserve_for_ops` explains at length what reallocations cost
    /// this crate.
    ///
    /// Checked over a spread of directions including the axis-parallel and
    /// grid-aligned cases, where the two disagree deliberately: a crossing
    /// that lands exactly on an existing point is dropped by
    /// `subdivide_line` (it would be a zero-length segment) but still
    /// counted here, so the bound is loose by one there rather than tight.
    #[test]
    fn the_line_estimate_never_undershoots_what_the_split_inserts() {
        let cell = EXTENT / 8; // granularity 8, in MVT units
        let subdivision = Subdivision { granularity: 8, north_pole: false, south_pole: false };
        let mut scratch = SubdivideScratch::default();
        let cases: [Vec<(i32, i32)>; 6] = [
            vec![(0, 0), (4096, 4096)],
            vec![(0, 0), (4096, 0)],
            vec![(0, 0), (0, 4096)],
            vec![(13, 4090), (4001, 7)],
            // Exactly on the grid at both ends, both axes.
            vec![(0, 0), (cell as i32 * 3, cell as i32 * 5)],
            // A zig-zag, so the per-segment counts have to add up.
            vec![(100, 100), (2000, 300), (500, 3000), (4000, 4000), (10, 2048)],
        ];
        for coords in cases {
            let line = geo_types::LineString::from(coords.clone());
            let bound = line_crossings(&line, subdivision, EXTENT, COORD_SCALE);
            let mut points: Vec<Point> = coords
                .iter()
                .map(|(x, y)| lyon::math::point(*x as f32 * COORD_SCALE, *y as f32 * COORD_SCALE))
                .collect();
            let before = points.len();
            subdivide_line(&mut points, subdivision, EXTENT, COORD_SCALE, &mut scratch);
            let inserted = points.len() - before;
            assert!(
                inserted <= bound,
                "{coords:?}: the split inserted {inserted} points but the estimate allowed only \
                 {bound} -- `mesh::estimate_buffer_capacity` would under-reserve"
            );
        }
    }

    /// **A runaway coordinate is passed through, not split and not
    /// dropped** — the [`MAX_CELLS_PER_PRIMITIVE`] guard.
    ///
    /// `mvt::decode` clamps an overflowing geometry cursor at `i32::MAX`
    /// rather than rejecting the tile, so a decoded coordinate is bounded by
    /// `2^31 * COORD_SCALE` and not by the tile. Without the cap the row
    /// loop in [`subdivide_fill_span`] would run 31.25 million times for the
    /// one triangle below, with a comparable number of columns inside each —
    /// see [`MAX_CELLS_PER_PRIMITIVE`] for that arithmetic, and for why it
    /// was not run to confirm. What this test pins is the behaviour the cap
    /// chooses: the primitive comes through UNSPLIT and still drawn, rather
    /// than dropped.
    #[test]
    fn a_runaway_coordinate_is_passed_through_instead_of_hanging() {
        let mut scratch = SubdivideScratch::default();
        let subdivision = Subdivision { granularity: 64, north_pole: false, south_pole: false };
        // 2e9 MVT units, i.e. 2e12 lyon units: 31 million cell rows at z0's
        // 64,000-unit cell.
        let far = 2.0e9f32 * COORD_SCALE;
        let mut buffers = triangle([(0.0, 0.0), (far, far), (far, 0.0)]);
        let before = buffers.indices.clone();
        subdivide_fill(&mut buffers, 0, 0, subdivision, EXTENT, COORD_SCALE, &mut scratch);
        assert_eq!(
            buffers.indices, before,
            "a primitive spanning more than {MAX_CELLS_PER_PRIMITIVE} tile widths must come \
             through unsplit, and must still be DRAWN -- dropping it would lose real geometry \
             from a tile whose only fault is one bad coordinate"
        );

        // The same for a line, and for its estimate, which must not promise
        // capacity for a split that no longer happens.
        let line = geo_types::LineString::from(vec![(0, 0), (2_000_000_000, 7)]);
        let bound = line_crossings(&line, subdivision, EXTENT, COORD_SCALE);
        let mut points = vec![lyon::math::point(0.0, 0.0), lyon::math::point(far, 7.0 * COORD_SCALE)];
        subdivide_line(&mut points, subdivision, EXTENT, COORD_SCALE, &mut scratch);
        assert_eq!(points.len(), 2, "a runaway segment must come through unsplit");
        assert!(bound <= 256, "the estimate must be capped too, got {bound}");
    }

    /// A vertical and a horizontal line, which are the two cases
    /// [`subdivide_line`]'s dominant-axis ordering has to get right without
    /// dividing by a zero delta.
    #[test]
    fn axis_parallel_lines_subdivide_without_dividing_by_zero() {
        let span = EXTENT as f32 * COORD_SCALE;
        let mut scratch = SubdivideScratch::default();
        let subdivision = Subdivision { granularity: 4, north_pole: false, south_pole: false };
        for (a, b, expected) in [
            (lyon::math::point(0.0, 0.0), lyon::math::point(span, 0.0), 5),
            (lyon::math::point(0.0, 0.0), lyon::math::point(0.0, span), 5),
            (lyon::math::point(span, span), lyon::math::point(span, 0.0), 5),
        ] {
            let mut points = vec![a, b];
            subdivide_line(&mut points, subdivision, EXTENT, COORD_SCALE, &mut scratch);
            assert_eq!(points.len(), expected, "{a:?} -> {b:?}");
            assert_eq!(points[0], a);
            assert_eq!(points[points.len() - 1], b);
        }
    }

    /// Tessellates closed rings into triangles the way `mesh::append_ops`
    /// does -- `FillOptions::tolerance(0.1)` with `FillRule::NonZero`, the
    /// exact options of `mesh.rs`'s fill path -- so the fixtures below are
    /// real lyon output and not hand-wound quads that happen to follow the
    /// grid.
    fn tessellate_rings(rings: &[&[(f32, f32)]]) -> VertexBuffers<Vertex, u32> {
        use lyon::path::Path;
        use lyon::tessellation::{
            BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, FillVertexConstructor,
        };
        struct Ctor;
        impl FillVertexConstructor<Vertex> for Ctor {
            fn new_vertex(&mut self, vertex: FillVertex) -> Vertex {
                fill_vertex(vertex.position().x, vertex.position().y)
            }
        }
        let mut builder = Path::builder();
        for ring in rings {
            builder.begin(lyon::math::point(ring[0].0, ring[0].1));
            for (x, y) in &ring[1..] {
                builder.line_to(lyon::math::point(*x, *y));
            }
            builder.end(true);
        }
        let path = builder.build();
        let mut buffers = VertexBuffers::new();
        FillTessellator::new()
            .tessellate_path(
                &path,
                &FillOptions::tolerance(0.1).with_fill_rule(FillRule::NonZero),
                &mut BuffersBuilder::new(&mut buffers, Ctor),
            )
            .expect("the fixture rings tessellate");
        buffers
    }

    /// One closed ring of a fixture polygon, in lyon units.
    type Ring = Vec<(f32, f32)>;

    /// A vertex that lands in the INTERIOR of a triangle edge it is not an
    /// endpoint of -- the definition of a T-junction.
    #[derive(Debug)]
    struct TJunction {
        /// The edge, by the two vertex indices that bound it.
        edge: (u32, u32),
        /// The vertex that lands inside it.
        vertex: u32,
        /// Where along the edge, as a fraction.
        along: f64,
        /// How far off the edge, in lyon units.
        across: f64,
    }

    /// Every [`TJunction`] in a mesh.
    ///
    /// **This is the T-junction instrument, and it is deliberately not a
    /// pixel test.** A T-junction conserves area, creates no degenerate
    /// triangle and duplicates nothing, so every check this module and
    /// `render::gpu` already run is blind to it by construction. It is also
    /// invisible FLAT -- the inserted vertex is exactly on the straight edge
    /// it splits, so the two sides rasterize to the same pixels. It only
    /// opens on the GLOBE, because `project_to_sphere` runs per vertex: the
    /// side without the vertex draws the chord, the side with it draws two
    /// chords through the arc, and the backdrop shows between them by the
    /// sagitta of one cell -- `r * (1 - cos(alpha/2))`, which
    /// [`BASE_GRANULARITY`] costs out at 0.19 px on the disc the tests
    /// render and 1.81 px on a 4K globe. Along cell boundaries, which on the
    /// globe are parallels and meridians.
    ///
    /// `ON_LINE` is one lyon unit -- a thousandth of an MVT unit, about
    /// 2e-7 of a tile -- so a coincidence at this distance IS a T-junction
    /// and not a near miss.
    fn t_junctions(buffers: &VertexBuffers<Vertex, u32>) -> Vec<TJunction> {
        const ON_LINE: f64 = 1.0;
        let mut edges: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
        for t in buffers.indices.as_chunks::<3>().0.iter() {
            for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                edges.insert((a.min(b), a.max(b)));
            }
        }
        let at = |index: u32| {
            let v = buffers.vertices[index as usize].position;
            (f64::from(v[0]), f64::from(v[1]))
        };
        let mut found = Vec::new();
        for &(ia, ib) in &edges {
            let (ax, ay) = at(ia);
            let (bx, by) = at(ib);
            let (dx, dy) = (bx - ax, by - ay);
            let length = (dx * dx + dy * dy).sqrt();
            // A degenerate edge has no interior to land in.
            if length <= 2.0 * ON_LINE {
                continue;
            }
            for index in 0..buffers.vertices.len() as u32 {
                if index == ia || index == ib {
                    continue;
                }
                let (vx, vy) = at(index);
                let across = ((vx - ax) * dy - (vy - ay) * dx).abs() / length;
                if across > ON_LINE {
                    continue;
                }
                let along = ((vx - ax) * dx + (vy - ay) * dy) / length;
                if along <= ON_LINE || along >= length - ON_LINE {
                    continue;
                }
                found.push(TJunction {
                    edge: (ia, ib),
                    vertex: index,
                    along: along / length,
                    across,
                });
            }
        }
        found
    }

    /// Fixtures chosen so that NONE of them follows the subdivision grid: a
    /// grid-aligned shape cannot exhibit a T-junction and would pass
    /// vacuously. Each ring is in lyon units, as a fraction of the tile
    /// span, at coordinates with no relationship to any multiple of a cell.
    fn off_grid_fixtures(span: f32) -> Vec<(&'static str, Vec<Ring>)> {
        let s = |f: f64| (f * f64::from(span)) as f32;
        vec![
            // A long band at a shallow angle, crossing every column and
            // several rows -- the case where one edge enters and leaves a
            // cell through opposite sides and the clip inserts a vertex on
            // both.
            (
                "shallow diagonal band",
                vec![vec![
                    (s(0.017), s(0.1234)),
                    (s(0.983), s(0.4071)),
                    (s(0.983), s(0.4771)),
                    (s(0.017), s(0.1934)),
                ]],
            ),
            // Concave, and it re-enters cells it has already left: the case
            // where one cell's clip output is two separate runs of the same
            // ring.
            (
                "concave ring re-entering cells",
                vec![vec![
                    (s(0.113), s(0.131)),
                    (s(0.871), s(0.193)),
                    (s(0.869), s(0.317)),
                    (s(0.371), s(0.289)),
                    (s(0.413), s(0.611)),
                    (s(0.887), s(0.593)),
                    (s(0.891), s(0.733)),
                    (s(0.127), s(0.771)),
                ]],
            ),
            // An edge that passes a hundredth of a lyon unit from the grid
            // intersection at (3, 4) cells without going through it, and is
            // very nearly parallel to the row boundary it crosses.
            (
                "edge grazing a grid intersection",
                vec![vec![
                    (s(0.0241), 2_048_000.0 - 0.37),
                    (s(0.9767), 2_048_000.0 + 0.41),
                    (s(0.5113), s(0.7431)),
                ]],
            ),
            // Coordinates with no short binary expansion, so the two
            // traversals of a shared edge round differently if anything
            // depends on traversal order.
            (
                "irrational coordinates",
                vec![vec![
                    (s(0.412_345_671_9), s(0.173_948_206_3)),
                    (s(0.938_271_604_5), s(0.659_182_773_1)),
                    (s(0.127_486_935_7), s(0.841_602_957_4)),
                ]],
            ),
            // **The reported case, literally**: a large landcover area is
            // one that runs off the side of its tile, so
            // `style::clip_fill_geometry_to_tile_extent` leaves it with two
            // edges lying EXACTLY on x = 0 and x = extent -- which are cell
            // lines -- while its north and south boundaries are wherever the
            // coastline is. `clip_half_plane` calls a point on the line
            // inside for BOTH half-planes, so an edge lying along one is the
            // case its `>=` / `<=` decide.
            (
                "clipped to the tile box, off-grid north and south",
                vec![vec![
                    (0.0, s(0.213)),
                    (span, s(0.147)),
                    (span, s(0.729)),
                    (0.0, s(0.811)),
                ]],
            ),
            // An edge lying exactly along an INTERIOR cell line for part of
            // its length and then leaving it: x = 1_536_000 is cell 3 of 8
            // and cell 6 of 16, and 1.5 cells of 4 -- so the same fixture is
            // on the grid at two granularities and off it at the third.
            (
                "boundary along an interior cell line, then off it",
                vec![vec![
                    (s(0.107), s(0.231)),
                    (1_536_000.0, s(0.197)),
                    (1_536_000.0, 2_048_000.0),
                    (s(0.883), 2_048_000.0),
                    (s(0.851), s(0.913)),
                    (s(0.091), s(0.877)),
                ]],
            ),
            // A ring with a hole, filling most of the tile: many cells fully
            // interior, and an inner boundary at another off-grid angle.
            (
                "holed polygon over most of the tile",
                vec![
                    vec![(s(0.031), s(0.047)), (s(0.967), s(0.023)), (s(0.981), s(0.971)), (s(0.019), s(0.953))],
                    vec![(s(0.313), s(0.271)), (s(0.667), s(0.359)), (s(0.607), s(0.701)), (s(0.291), s(0.633))],
                ],
            ),
        ]
    }

    /// **No cell boundary is a T-junction.**
    ///
    /// The one surface `subdivision_leaves_no_crack_between_cells` and
    /// `render::gpu`'s lit whole-level sweep cannot grade: both take
    /// geometry that FOLLOWS the grid, where the two sides of a cell
    /// boundary place vertices at the same places for the same reason.
    /// These fixtures do not follow the grid, so the two sides only agree if
    /// [`subdivide_fill_span`] makes them.
    ///
    /// Graded against the UNSUBDIVIDED tessellation of the same rings, not
    /// against zero: lyon's own output is the baseline, and only what
    /// subdivision ADDS is this module's defect.
    #[test]
    fn subdividing_an_off_grid_polygon_opens_no_t_junction() {
        let span = EXTENT as f32 * COORD_SCALE;
        // The instrument first, on a mesh built to contain exactly one
        // T-junction: a zero from a detector that cannot detect is the same
        // exit code as a zero from a watertight mesh, and this test's whole
        // value is in telling those two apart.
        let mut planted = VertexBuffers::new();
        for (x, y) in [
            (0.0, 0.0),            // 0
            (span, 0.0),           // 1
            (span * 0.5, 0.5),     // 2 -- on the 0 -> 1 edge, to half a lyon unit
            (span * 0.5, -span),   // 3
            (span * 0.25, span),   // 4
        ] {
            planted.vertices.push(fill_vertex(x, y));
        }
        // Below the edge, one triangle spanning the whole of it; above it,
        // two that stop at the planted vertex.
        planted.indices.extend_from_slice(&[0, 1, 4, 0, 2, 3, 2, 1, 3]);
        let seen = t_junctions(&planted);
        assert_eq!(
            seen.len(),
            1,
            "the instrument must see the planted T-junction, and only it: {seen:?}"
        );
        assert_eq!(seen[0].vertex, 2, "and must name the planted vertex");

        let mut report = Vec::new();
        for (name, rings) in off_grid_fixtures(span) {
            let rings: Vec<&[(f32, f32)]> = rings.iter().map(Vec::as_slice).collect();
            let before = tessellate_rings(&rings);
            let baseline = t_junctions(&before).len();
            for granularity in [4u32, 8, 16] {
                let mut buffers = tessellate_rings(&rings);
                let triangles_in = buffers.indices.len() / 3;
                let mut scratch = SubdivideScratch::default();
                subdivide_fill_span(
                    &mut buffers,
                    0,
                    0,
                    granularity,
                    span / granularity as f32,
                    &mut scratch,
                );
                let found = t_junctions(&buffers);
                report.push(format!(
                    "{name} @ g{granularity}: {triangles_in} -> {} triangles, \
                     {} vertices, {} T-junctions (lyon baseline {baseline})",
                    buffers.indices.len() / 3,
                    buffers.vertices.len(),
                    found.len(),
                ));
                assert!(
                    found.len() <= baseline,
                    "{name} @ granularity {granularity}: subdivision opened {} T-junctions \
                     where lyon's own tessellation had {baseline}. First: vertex {:?} sits \
                     {:.6} lyon units off the edge {:?} -> {:?}, {:.1}% along it.\n{}",
                    found.len() - baseline,
                    buffers.vertices[found[0].vertex as usize].position,
                    found[0].across,
                    buffers.vertices[found[0].edge.0 as usize].position,
                    buffers.vertices[found[0].edge.1 as usize].position,
                    found[0].along * 100.0,
                    report.join("\n"),
                );
            }
        }
        // Printed so the measurement is on the record even when it passes:
        // a vacuous pass and a real one look identical from the exit code.
        println!("{}", report.join("\n"));
    }

    /// The grid split never emits OVERLAPPING area, over the awkward shapes
    /// the half-tile acceptance test above does not reach. A translucent
    /// fill drawn twice over itself blends paler than one drawn once, so
    /// overlapping cells would show as pale, straight-edged patches on the
    /// grid; missing area would show as gaps.
    ///
    /// Graded by AREA because a vertex/triangle count only pins the
    /// tile-covering quad, where the grid is exact. Includes a polygon that
    /// reaches outside the tile the way an MVT buffer does.
    #[test]
    fn the_grid_split_conserves_area_over_awkward_shapes() {
        use lyon::math::point as pt;
        let span = EXTENT as f32 * COORD_SCALE;
        for (name, ring) in [
            ("tile-covering quad", vec![pt(0.0, 0.0), pt(span, 0.0), pt(span, span), pt(0.0, span)]),
            ("diagonal triangle", vec![pt(0.0, 0.0), pt(span, span * 0.5), pt(span * 0.25, span)]),
            ("thin sliver", vec![pt(0.0, 0.0), pt(span, span * 0.01), pt(0.0, span * 0.02)]),
            (
                "reaches outside the tile (MVT buffer)",
                vec![
                    pt(-span * 0.1, -span * 0.1),
                    pt(span * 1.1, -span * 0.1),
                    pt(span * 1.1, span * 0.37),
                    pt(-span * 0.1, span * 0.37),
                ],
            ),
            (
                "off-grid corners",
                vec![
                    pt(span * 0.031, span * 0.017),
                    pt(span * 0.911, span * 0.233),
                    pt(span * 0.677, span * 0.981),
                    pt(span * 0.101, span * 0.503),
                ],
            ),
        ] {
            let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
            let template = Vertex { position: [0.0, 0.0], ..bytemuck::Zeroable::zeroed() };
            for p in &ring {
                buffers.vertices.push(Vertex { position: [p.x, p.y], ..template });
            }
            for i in 2..ring.len() as u32 {
                buffers.indices.extend_from_slice(&[0, i - 1, i]);
            }
            let before = area(&buffers);
            let mut scratch = SubdivideScratch::default();
            subdivide_fill(
                &mut buffers,
                0,
                0,
                Subdivision { granularity: 16, north_pole: false, south_pole: false },
                EXTENT,
                COORD_SCALE,
                &mut scratch,
            );
            let after = area(&buffers);
            let rel = ((after - before) / before).abs();
            println!("  {name:38} before {before:.6e} after {after:.6e}  rel {rel:.3e}  tris {}", buffers.indices.len() / 3);
            assert!(
                rel < 1e-4,
                "{name}: the split changed the total triangle area by {rel:.3e} \
                 -- above zero means cells OVERLAP (pale double-blended patches) or leave gaps"
            );
        }
    }
}
