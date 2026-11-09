//! The dot mask's per-frame step, shell-neutral: when the relocation
//! mask (`render::dot_mask`) is re-rendered, from the frame's camera, its
//! basemap draws and its cover, and the ring of basemap tiles past the
//! cover the mask rasterises. `map_view` drives it from `paint`; the
//! browser and the phone drive the same functions with their own frame's
//! basemap draw list (dasymetric plan Task 10, DP-7) -- nothing here names
//! gpui, winit or the DOM.
//!
//! See `docs/superpowers/specs/2026-09-27-dasymetric-dot-placement-design.md`.

use crate::overlay::dotgrid::layer::DotLayers;

/// The basemap tiles past the `visible` cover the dot mask rasterises
/// this frame (`render::dot_mask::mask_ring_tiles`, DP-5 ruling): none
/// without a relocating layer, and none where
/// `render::dots::relocation_camera` says the frame does not relocate (a
/// globe frame, below z15, above 30 deg of pitch).
pub fn mask_ring_for_frame(
    layers: &DotLayers,
    camera: &crate::render::Camera,
    globe: &crate::render::globe::GlobeCover,
    visible: &[crate::geo::TileId],
) -> Vec<crate::geo::TileId> {
    if !layers.wants_mask() {
        return Vec::new();
    }
    let Some(cell) = layers.finest_relocating_cell() else { return Vec::new() };
    crate::render::dot_mask::mask_ring_tiles(camera, globe.transition(), cell, visible)
}

/// Whether the frame's basemap cover has no holes: every wanted cover
/// tile is drawn by its own `CachedTile` (in `own`, the tiles drawn whole
/// at their own id), except where `nothing_here` says there is nothing to
/// draw -- a tile the source reported absent, or one whose mesh is empty
/// (never uploaded). A stand-in, clipped ancestor or descendant, only ever
/// fills a wanted tile that is not drawn by its own, so it needs no check
/// of its own: its target either is excluded or already makes the cover
/// incomplete. Only such a frame re-keys the dot mask at once
/// ([`MaskRekey`]): a mask drawn over a hole would read land there, and
/// one drawn from a stand-in would change again the moment the tile lands.
pub fn cover_is_complete(
    wanted: &[crate::geo::TileId],
    own: &std::collections::HashSet<crate::geo::TileId>,
    nothing_here: impl Fn(crate::geo::TileId) -> bool,
) -> bool {
    wanted.iter().all(|&tile| own.contains(&tile) || nothing_here(tile))
}

/// The frame's mask key: the grid over `relocation_region(camera,
/// transition, cell)` grown by one data cell `cell` (unit world), and
/// `(TileId, mesh_generation)` of every basemap tile drawn at its own id,
/// sorted. `None` when the frame needs no mask (a globe frame, or cells
/// under `RELOCATION_MIN_CELL_PX` everywhere).
pub fn frame_mask_key(
    camera: &crate::render::Camera,
    transition: f32,
    cell: f64,
    max_dim: u32,
    drawn: impl IntoIterator<Item = (crate::geo::TileId, u64)>,
    previous: Option<&crate::render::dot_mask::MaskGrid>,
) -> Option<crate::render::dot_mask::MaskKey> {
    use crate::render::dot_mask::{relocation_region, MaskGrid, MaskKey};
    let region = relocation_region(camera, transition, cell)?;
    // The bound mask's grid: its zoom holds while the region sits between
    // the climb side and the cap (`MaskGrid::plan_after`).
    let bound = crate::render::dot_mask::mask_extent_bound(camera, transition, cell);
    let grid = MaskGrid::plan_after(camera.zoom, region, cell, max_dim, previous, bound)?;
    let mut tiles: Vec<_> = drawn.into_iter().collect();
    tiles.sort_unstable_by_key(|(t, generation)| (t.z, t.x, t.y, *generation));
    tiles.dedup();
    Some(MaskKey { grid, tiles })
}

/// Frames a changed key waits, unchanged, for its cover's holes to fill
/// before the mask is re-keyed anyway: a tile that keeps failing (the
/// fetch backoff) must not hold relocation off forever. While it waits the
/// map asks for frames ([`MaskStep::Waiting`]), so a still map reaches it.
pub const REKEY_FALLBACK_FRAMES: u32 = 30;

/// When the dot mask is re-rendered: a new key over a hole-free cover at
/// once, or -- after [`REKEY_FALLBACK_FRAMES`] frames of incomplete covers
/// in a row, whatever their keys -- anyway. Otherwise the previous mask
/// stays bound. The count runs from the FIRST incomplete frame: a key
/// changing while it waits (a pan, a ring tile arriving) does not restart
/// it (Task 9 review m2).
///
/// The fallback fires ONCE per incomplete episode (review fix 2): an
/// episode is the incomplete covers over one mask grid. After it fires,
/// further incomplete keys on the same grid (a tile that keeps failing,
/// its generation or a ring tile changing) neither re-render nor ask for
/// frames ([`Decision::Hold`]), so a still map idles. The episode ends when
/// a cover is complete, or when the grid moves (a pan past a snap, a zoom:
/// new ground, which gets its own one fallback).
#[derive(Default)]
pub struct MaskRekey {
    /// Frames of incomplete covers waited in a row.
    waiting: Option<u32>,
    /// The grid whose incomplete episode's fallback already fired.
    fired: Option<crate::render::dot_mask::MaskGrid>,
    /// The mask pass's GPU timer: `None` until first asked, then `Some`
    /// of the device's answer (probe builds).
    #[cfg(feature = "probe")]
    timer: Option<Option<crate::render::mask_timer::MaskPassTimer>>,
}

impl MaskRekey {
    /// Whether to re-render for `key` (already known to differ from the
    /// mask's) on a frame whose cover is `complete` or not.
    pub fn decide(&mut self, key: &crate::render::dot_mask::MaskKey, complete: bool) -> Decision {
        if complete {
            self.waiting = None;
            self.fired = None;
            return Decision::Rekey;
        }
        if self.fired.as_ref() == Some(&key.grid) {
            return Decision::Hold;
        }
        let frames = self.waiting.get_or_insert(0);
        *frames += 1;
        if *frames >= REKEY_FALLBACK_FRAMES {
            self.waiting = None;
            self.fired = Some(key.grid);
            return Decision::Rekey;
        }
        Decision::Wait
    }

    /// The mask's key is the frame's: nothing waits.
    fn settled(&mut self) {
        self.waiting = None;
    }
}

/// [`MaskRekey::decide`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Re-render the mask for the key.
    Rekey,
    /// Keep the mask and ask for another frame: the fallback is counting.
    Wait,
    /// Keep the mask and ask for nothing: this episode's fallback fired.
    Hold,
}

/// One basemap draw of the frame, as the mask pass needs it.
pub struct DrawnBasemapTile<'a> {
    pub tile: crate::geo::TileId,
    pub buffers: &'a crate::render::TileBuffers,
    pub mesh_generation: u64,
    /// False for a clipped ancestor stand-in.
    pub whole: bool,
}

/// What [`step_dot_mask`] did this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskStep {
    /// The frame needs no new mask (the key is the mask's, or no mask).
    Unchanged,
    /// The mask was re-rendered.
    Rekeyed,
    /// A new key waits for its cover's holes: ask for another frame.
    Waiting,
}

/// The paint-side mask step, before the dot sync: the frame's key from
/// its whole-drawn basemap tiles; when it differs from `mask`'s, the cover
/// check (skipped when it does not) and [`MaskRekey`]'s decision; and on a
/// re-key the mask pass, in its own submit ahead of the frame's (queue
/// order puts it before the main pass that reads it).
#[allow(clippy::too_many_arguments)]
pub fn step_dot_mask(
    ctx: &crate::render::GpuContext,
    pipeline: &crate::render::TilePipeline,
    mask: &mut Option<crate::render::dot_mask::DotMaskGpu>,
    rekey: &mut MaskRekey,
    camera: &crate::render::Camera,
    transition: f32,
    cell: f64,
    drawn: &[DrawnBasemapTile<'_>],
    wanted: &[crate::geo::TileId],
    nothing_here: impl Fn(crate::geo::TileId) -> bool,
) -> MaskStep {
    // The last mask pass's GPU stamps, resolved on a later command buffer
    // than the pass's (probe builds; see `render::mask_timer::MaskPassTimer`).
    #[cfg(feature = "probe")]
    if let Some(timer) = rekey.timer.get_or_insert_with(|| crate::render::mask_timer::MaskPassTimer::new(ctx)) {
        timer.collect(ctx);
    }
    let whole = || drawn.iter().filter(|d| d.whole);
    let max_dim = ctx.device.limits().max_texture_dimension_2d;
    let previous = mask.as_ref().and_then(crate::render::dot_mask::DotMaskGpu::key).map(|k| k.grid);
    let Some(key) = frame_mask_key(camera, transition, cell, max_dim, whole().map(|d| (d.tile, d.mesh_generation)), previous.as_ref()) else {
        return MaskStep::Unchanged;
    };
    if mask.as_ref().and_then(crate::render::dot_mask::DotMaskGpu::key) == Some(&key) {
        rekey.settled();
        return MaskStep::Unchanged;
    }
    let own: std::collections::HashSet<crate::geo::TileId> = whole().map(|d| d.tile).collect();
    match rekey.decide(&key, cover_is_complete(wanted, &own, nothing_here)) {
        Decision::Rekey => {}
        Decision::Wait => return MaskStep::Waiting,
        Decision::Hold => return MaskStep::Unchanged,
    }
    #[cfg(feature = "probe")]
    let started = crate::timing::Instant::now();
    let tiles: Vec<crate::render::gpu::MaskTileDraw<'_>> =
        whole().map(|d| crate::render::gpu::MaskTileDraw::for_tile(d.buffers, d.tile)).collect();
    let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dot-mask") });
    #[cfg(feature = "probe")]
    let timer = rekey.timer.as_ref().and_then(Option::as_ref);
    #[cfg(feature = "probe")]
    let writes = timer.and_then(crate::render::mask_timer::MaskPassTimer::writes);
    #[cfg(not(feature = "probe"))]
    let writes = None;
    let timed = writes.is_some();
    pipeline.encode_dot_mask_timed(ctx, &mut encoder, mask, key, &tiles, writes);
    ctx.queue.submit(Some(encoder.finish()));
    #[cfg(feature = "probe")]
    if let Some(timer) = timer.filter(|_| timed) {
        timer.mark_written();
    }
    #[cfg(not(feature = "probe"))]
    let _ = timed;
    #[cfg(feature = "probe")]
    {
        use std::sync::atomic::Ordering::Relaxed;
        let ns = started.elapsed().as_nanos() as u64;
        crate::render::dot_mask::MASK_REKEYS_TOTAL.fetch_add(1, Relaxed);
        crate::render::dot_mask::MASK_PASS_NS_TOTAL.fetch_add(ns, Relaxed);
        crate::render::dot_mask::MASK_PASS_NS_MAX.fetch_max(ns, Relaxed);
    }
    MaskStep::Rekeyed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::overlay::dotgrid::layer::{DotStyle, GHSL_ATTRIBUTION};
    use crate::overlay::dotgrid::{grid, GridDescriptor, GridTile, MemoryGridSource};

    /// The user's city view, flat, `size` px at `zoom`, centred `at` (a
    /// fraction of a tile, x) across and halfway down the tile at the
    /// cover's zoom (`floor(zoom)`) holding (43.95, 56.30).
    fn city_camera(zoom: f64, at: f64, size: (f32, f32)) -> crate::render::Camera {
        let n = f64::from(1u32 << zoom.floor() as u32);
        let (wx, wy) = crate::overlay::geom::lon_lat_to_world(43.95, 56.30);
        let world = f64::from(crate::render::WORLD_SIZE);
        let (x, y) = ((wx / world * n).floor() + at, (wy / world * n).floor() + 0.5);
        let lon = x / n * 360.0 - 180.0;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
        crate::render::Camera {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: size.0,
            viewport_height_px: size.1,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    /// `camera` moved `px` screen pixels east.
    fn panned(camera: &crate::render::Camera, px: f64) -> crate::render::Camera {
        let deg_per_px = 360.0 / (f64::from(crate::render::WORLD_SIZE) * camera.zoom.exp2());
        crate::render::Camera { center_lon: camera.center_lon + px * deg_per_px, ..*camera }
    }

    fn cover(camera: &crate::render::Camera) -> Vec<crate::geo::TileId> {
        crate::render::cover_plan::flat_cover(camera, camera.zoom.floor() as u8).tiles
    }

    /// 30″ cells, in unit world.
    const CELL: f64 = 1.0 / 43_200.0;

    /// The mask's generation across frames, as `step_dot_mask` drives it
    /// without a GPU: a key equal to the mask's does nothing, a new one
    /// goes through [`MaskRekey`].
    #[derive(Default)]
    struct Mask {
        key: Option<crate::render::dot_mask::MaskKey>,
        generation: u64,
        rekey: MaskRekey,
        /// Tiles with nothing to draw (absent, or an empty mesh).
        nothing: Vec<crate::geo::TileId>,
        /// Frames that asked for another frame (`Decision::Wait`).
        waits: u32,
    }

    impl Mask {
        /// One frame: `drawn` is `model_buffers` as (tile, whole,
        /// mesh_generation).
        fn frame(&mut self, camera: &crate::render::Camera, drawn: &[(crate::geo::TileId, bool, u64)]) {
            let key = frame_mask_key(camera, 0.0, CELL, 4096, drawn.iter().filter(|d| d.1).map(|d| (d.0, d.2)), None).expect("a flat z15 view has a mask");
            if self.key.as_ref() == Some(&key) {
                self.rekey.settled();
                return;
            }
            let own: std::collections::HashSet<_> = drawn.iter().filter(|d| d.1).map(|d| d.0).collect();
            let complete = cover_is_complete(&cover(camera), &own, |t| self.nothing.contains(&t));
            match self.rekey.decide(&key, complete) {
                Decision::Rekey => {
                    self.key = Some(key);
                    self.generation += 1;
                }
                Decision::Wait => self.waits += 1,
                Decision::Hold => {}
            }
        }
    }

    /// A frame of `camera`'s cover with `tile` not drawn by its own: its
    /// z13 ancestor stands in, clipped.
    fn with_stand_in_for(camera: &crate::render::Camera, tile: crate::geo::TileId) -> Vec<(crate::geo::TileId, bool, u64)> {
        let parent = crate::geo::TileId { z: tile.z - 1, x: tile.x / 2, y: tile.y / 2 };
        let mut drawn: Vec<_> = cover(camera).into_iter().filter(|&t| t != tile).map(|t| (t, true, 1)).collect();
        drawn.push((parent, false, 1));
        drawn
    }

    /// The pan of `the_mask_is_not_rekeyed_while_the_cover_has_holes`:
    /// the first camera, the second, and the tile the pan brings in.
    fn pan_bringing_a_tile() -> (crate::render::Camera, crate::render::Camera, crate::geo::TileId) {
        let first = city_camera(15.0, 0.75, (800.0, 600.0));
        let second = panned(&first, 300.0);
        let new = *cover(&second).iter().find(|t| !cover(&first).contains(t)).expect("the pan brings a new tile into view");
        (first, second, new)
    }

    #[test]
    fn an_empty_or_absent_tile_behind_a_resident_ancestor_rekeys() {
        for why in ["an empty mesh", "absent at the source"] {
            let (first, second, tile) = pan_bringing_a_tile();
            let mut mask = Mask::default();
            mask.frame(&first, &cover(&first).iter().map(|&t| (t, true, 1)).collect::<Vec<_>>());
            // The tile has nothing to draw (`paint` passes both kinds the
            // same way): the ancestor stand-in pass 1 still pushes for it
            // is not a hole.
            mask.nothing = vec![tile];
            mask.frame(&second, &with_stand_in_for(&second, tile));
            assert_eq!(mask.generation, 2, "{why}: a tile with nothing to draw held the mask");
        }
    }

    #[test]
    fn a_permanently_failing_tile_rekeys_after_the_fallback() {
        let (first, second, tile) = pan_bringing_a_tile();
        let mut mask = Mask::default();
        mask.frame(&first, &cover(&first).iter().map(|&t| (t, true, 1)).collect::<Vec<_>>());
        let drawn = with_stand_in_for(&second, tile);
        for frame in 1..REKEY_FALLBACK_FRAMES {
            mask.frame(&second, &drawn);
            assert_eq!(mask.generation, 1, "re-keyed over a hole at frame {frame}");
        }
        mask.frame(&second, &drawn);
        assert_eq!(mask.generation, 2, "the failing tile held the mask past the fallback");
        // Review fix 2: the fallback fires once. The same failing cover
        // past frame 90 -- the same key, then a drawn tile's generation
        // changing every frame (so the key does too, on the same grid): no
        // second re-key and no frame asked for.
        let waits = mask.waits;
        for frame in REKEY_FALLBACK_FRAMES + 1..=3 * REKEY_FALLBACK_FRAMES {
            let mut drawn = with_stand_in_for(&second, tile);
            if frame > 2 * REKEY_FALLBACK_FRAMES {
                drawn[0].2 = u64::from(frame);
            }
            mask.frame(&second, &drawn);
            assert_eq!(mask.generation, 2, "re-keyed again at frame {frame}");
        }
        assert_eq!(mask.waits, waits, "frames asked for after the fallback fired");
        // A complete cover ends the episode.
        mask.frame(&second, &cover(&second).iter().map(|&t| (t, true, 1)).collect::<Vec<_>>());
        assert_eq!(mask.generation, 3);
        // And a new incomplete key starts its own wait.
        let third = panned(&second, 5.0 * 512.0);
        let tile = cover(&third)[0];
        let waits = mask.waits;
        mask.frame(&third, &with_stand_in_for(&third, tile));
        assert_eq!((mask.generation, mask.waits), (3, waits + 1), "waiting, and asking for frames");
    }

    #[test]
    fn the_mask_is_not_rekeyed_while_the_cover_has_holes() {
        let mut mask = Mask::default();
        // Frame 1: a complete cover, the view's east edge 16 px short of
        // the next tile column.
        let first = city_camera(15.0, 0.75, (800.0, 600.0));
        let own = |tiles: &[crate::geo::TileId]| tiles.iter().map(|&t| (t, true, 1)).collect::<Vec<_>>();
        mask.frame(&first, &own(&cover(&first)));
        assert_eq!(mask.generation, 1);
        // Frame 2: 300 texels east; one newly visible tile is still loading
        // and its z13 ancestor stands in, clipped.
        let second = panned(&first, 300.0);
        let wanted = cover(&second);
        let loading = *wanted.iter().find(|t| !cover(&first).contains(t)).expect("the pan brings a new tile into view");
        let parent = crate::geo::TileId { z: loading.z - 1, x: loading.x / 2, y: loading.y / 2 };
        let mut drawn: Vec<_> = wanted.iter().filter(|&&t| t != loading).map(|&t| (t, true, 1)).collect();
        drawn.push((parent, false, 1));
        let key = frame_mask_key(&second, 0.0, CELL, 4096, drawn.iter().filter(|d| d.1).map(|d| (d.0, d.2)), None).unwrap();
        assert_ne!(Some(&key), mask.key.as_ref(), "the pan changed the key");
        mask.frame(&second, &drawn);
        assert_eq!(mask.generation, 1, "a cover with a hole re-keyed the mask");
        // Frame 3: the tile lands.
        let mut drawn: Vec<_> = wanted.iter().filter(|&&t| t != loading).map(|&t| (t, true, 1)).collect();
        drawn.push((loading, true, 2));
        mask.frame(&second, &drawn);
        assert_eq!(mask.generation, 2);
        // A frame with nothing new keeps it.
        mask.frame(&second, &drawn);
        assert_eq!(mask.generation, 2);
    }

    #[test]
    fn a_steady_pan_under_256_texels_does_not_rekey() {
        let mut mask = Mask::default();
        // 200 px square in the middle of one tile: 156 px from its edges.
        let start = city_camera(15.0, 0.5, (200.0, 200.0));
        // A pan that brings no new tile into the cover: the grid is snapped
        // to the cover's tiles, so nothing else can move it.
        let steps: Vec<_> = (0..=15).map(|i| panned(&start, f64::from(i) * 10.0)).collect();
        let tiles = cover(&start);
        assert!(steps.iter().all(|c| cover(c) == tiles), "the fixture's pan crosses a tile edge");
        for camera in &steps {
            let drawn: Vec<_> = tiles.iter().map(|&t| (t, true, 1)).collect();
            mask.frame(camera, &drawn);
        }
        assert_eq!(mask.generation, 1, "150 px of pan re-keyed the mask");
    }

    #[test]
    fn a_rebaked_tile_rekeys_and_a_descendant_stand_in_is_a_hole() {
        let camera = city_camera(15.0, 0.5, (800.0, 600.0));
        let tiles = cover(&camera);
        let own: std::collections::HashSet<_> = tiles.iter().copied().collect();
        assert!(cover_is_complete(&tiles, &own, |_| false));
        // A missing tile drawn by its four children is still a hole: the
        // children are not the tile the cover asked for.
        let t = tiles[0];
        let mut kids: std::collections::HashSet<_> = tiles[1..].iter().copied().collect();
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            kids.insert(crate::geo::TileId { z: t.z + 1, x: t.x * 2 + dx, y: t.y * 2 + dy });
        }
        assert!(!cover_is_complete(&tiles, &kids, |_| false));
        let key = |generation: u64| frame_mask_key(&camera, 0.0, CELL, 4096, tiles.iter().map(|&t| (t, generation)), None).unwrap();
        assert_ne!(key(1), key(2), "a reinstall changes the key");
        assert_eq!(MaskRekey::default().decide(&key(2), true), Decision::Rekey, "and re-renders the mask on a complete cover");
        // No mask at all on the globe.
        assert!(frame_mask_key(&camera, 0.5, CELL, 4096, tiles.iter().map(|&t| (t, 1)), None).is_none());
    }

    /// A basemap tile that is one full-tile building, installed with its
    /// mask lists the way the upload loop installs one.
    fn building_tile(ctx: &crate::render::GpuContext, pipeline: &crate::render::TilePipeline) -> crate::render::TileBuffers {
        let extent = crate::mvt::MVT_EXTENT as i32;
        let decoded = crate::mvt::DecodedTile {
            data_hash: 0,
            layers: vec![crate::mvt::Layer {
                name: "building".to_string(),
                features: vec![crate::mvt::Feature {
                    geometry: geo_types::Geometry::Polygon(geo_types::Polygon::new(
                        geo_types::LineString::from(vec![(0, 0), (extent, 0), (extent, extent), (0, extent), (0, 0)]),
                        vec![],
                    )),
                    tags: Default::default(),
                }],
            }],
        };
        let style = crate::style::Style {
            layers: vec![crate::style::StyleLayer::Fill {
                id: "building".to_string(),
                source_layer: "building".to_string(),
                filter: crate::style::Filter::Any,
                color: crate::style::ZoomValue::constant(crate::style::Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 }),
                opacity: crate::style::ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: Some(0),
                max_zoom: None,
            }],
            ..crate::style::Style::default()
        };
        let matched = crate::style::match_tile_static_masked(&decoded, &style, true);
        let (plan, ranks) = crate::style::resolve_plan_with_mask(&decoded, &style, &matched, 14);
        let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
            &plan,
            crate::mvt::MVT_EXTENT,
            crate::render::mesh::COORD_SCALE,
            crate::render::subdivide::Subdivision::NONE,
        );
        pipeline.install_basemap_mesh_masked(ctx, &mesh, &spans, &ranks)
    }

    /// The paint-side mask step end to end on a headless device: the
    /// mask renders over a cover whose one missing tile has an empty mesh
    /// (its clipped ancestor standing in), holds while a loading tile
    /// leaves a hole, re-keys when it lands, and the dot sync rebinds
    /// every group to each new mask texture (not on a same-size re-key),
    /// and to the land texel without one.
    #[test]
    fn the_paint_side_mask_step_rekeys_past_empty_tiles_and_the_dot_groups_follow() {
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let (first, second, new_tile) = pan_bringing_a_tile();
        let buffers = building_tile(&ctx, &pipeline);
        let parent = crate::geo::TileId { z: new_tile.z - 1, x: new_tile.x / 2, y: new_tile.y / 2 };
        let drawn = |camera: &crate::render::Camera, missing: Option<crate::geo::TileId>, generation: u64| {
            let mut drawn: Vec<DrawnBasemapTile<'_>> = cover(camera)
                .into_iter()
                .filter(|&t| Some(t) != missing)
                .map(|tile| DrawnBasemapTile { tile, buffers: &buffers, mesh_generation: generation, whole: true })
                .collect();
            if missing.is_some() {
                drawn.push(DrawnBasemapTile { tile: parent, buffers: &buffers, mesh_generation: 1, whole: false });
            }
            drawn
        };
        let (mut mask, mut rekey) = (None, MaskRekey::default());
        let mut step = |mask: &mut Option<crate::render::dot_mask::DotMaskGpu>,
                        camera: &crate::render::Camera,
                        drawn: &[DrawnBasemapTile<'_>],
                        empty: Option<crate::geo::TileId>| {
            step_dot_mask(&ctx, &pipeline, mask, &mut rekey, camera, 0.0, CELL, drawn, &cover(camera), |t| Some(t) == empty)
        };
        // The pan's new tile has an empty mesh: never uploaded, its
        // ancestor stands in clipped -- and the mask renders anyway.
        assert_eq!(step(&mut mask, &second, &drawn(&second, Some(new_tile), 1), Some(new_tile)), MaskStep::Rekeyed);
        let m = mask.as_ref().expect("a mask");
        assert_eq!(m.generation(), 1);
        let texels = m.read_back(&ctx);
        let building = crate::overlay::dotgrid::mask_class::MaskRank::Building as u8;
        let key = m.key().expect("keyed");
        let texel_at = |tile: crate::geo::TileId| {
            let n = f64::from(1u32 << tile.z);
            let [x, y] = key.grid.texel_of((f64::from(tile.x) + 0.5) / n, (f64::from(tile.y) + 0.5) / n).expect("inside the mask");
            texels[y as usize * key.grid.size[0] as usize + x as usize]
        };
        let drawn_tile = *cover(&second).iter().find(|&&t| t != new_tile).unwrap();
        assert_eq!(texel_at(drawn_tile), building, "the buildings reached the mask");
        // No tile drew there: no data (DP-5 ruling), so the dots over it
        // keep their place rather than guess.
        assert_eq!(texel_at(new_tile), crate::render::dot_mask::MASK_NO_DATA_RANK as u8, "the empty tile has no data");
        // The same frame again: nothing to do.
        assert_eq!(step(&mut mask, &second, &drawn(&second, Some(new_tile), 1), Some(new_tile)), MaskStep::Unchanged);
        // Back to the first camera with one tile loading: a hole, it waits.
        let loading = cover(&first)[0];
        let holed = drawn(&first, Some(loading), 1);
        assert_eq!(step(&mut mask, &first, &holed, None), MaskStep::Waiting);
        assert_eq!(mask.as_ref().unwrap().generation(), 1);

        // The dot groups bind the live mask, follow a re-key, and drop to
        // the land texel with no mask.
        let d = crate::overlay::dotgrid::GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: crate::overlay::dotgrid::grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: String::new(),
        };
        let source = Arc::new(MemoryGridSource::new(d, |_| GridTile { cells: vec![(0, 1000.0, 1000.0)], total: 1000.0, ..Default::default() }));
        let mut layers = crate::overlay::host::DotLayers::new();
        layers.set_source("pop", source, DotStyle::default());
        let globe_cover = crate::render::globe::GlobeCover::FLAT;
        for _ in 0..10 {
            layers.frame(&second, &globe_cover, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        let frame = crate::render::globe::globe_frame(&second);
        let globe = crate::render::globe::GlobeUpload::new(&frame, &second, crate::mvt::MVT_EXTENT as f32, glam::DVec3::ZERO, 1.0);
        assert_eq!(globe.transition(), 0.0, "a flat frame");
        let mut gpu = crate::overlay::host::DotLayersGpu::default();
        let sync = |gpu: &mut crate::overlay::host::DotLayersGpu, layers: &mut crate::overlay::host::DotLayers, mask: Option<&crate::render::dot_mask::DotMaskGpu>| {
            let bound = mask.and_then(|m| m.key().map(|k| (m, &k.grid)));
            gpu.sync(layers, &ctx, &pipeline, &second, &globe, bound);
            gpu.mask_generations()
        };
        let bound = sync(&mut gpu, &mut layers, mask.as_ref());
        assert!(!bound.is_empty(), "the layer drew nothing");
        assert!(bound.iter().all(|g| *g == Some(1)), "{bound:?}");
        // A re-key into a texture of the same size (the same camera, a
        // tile's mesh re-baked): the groups keep their binding -- only the
        // uniforms are rewritten (final review m2).
        let size = mask.as_ref().unwrap().size();
        assert_eq!(step(&mut mask, &second, &drawn(&second, Some(new_tile), 7), Some(new_tile)), MaskStep::Rekeyed);
        assert_eq!(mask.as_ref().unwrap().size(), size, "the same camera keeps the grid");
        assert_eq!(mask.as_ref().unwrap().generation(), 1, "the same texture");
        let bound = sync(&mut gpu, &mut layers, mask.as_ref());
        assert!(bound.iter().all(|g| *g == Some(1)), "a same-size re-key rebuilt the groups: {bound:?}");
        // A re-key (the tile lands, the camera pans back): every group
        // follows the mask's texture.
        assert_eq!(step(&mut mask, &first, &drawn(&first, None, 2), None), MaskStep::Rekeyed);
        let live = mask.as_ref().unwrap().generation();
        assert_eq!(live == 1, mask.as_ref().unwrap().size() == size, "a new texture only on a new size");
        let bound = sync(&mut gpu, &mut layers, mask.as_ref());
        assert!(bound.iter().all(|g| *g == Some(live)), "{bound:?}");
        let bound = sync(&mut gpu, &mut layers, None);
        assert!(bound.iter().all(Option::is_none), "{bound:?}");
    }

    /// **DP-1, pinned.** The overlap fade and relocation move dots and
    /// fade them, never change which or how many: over a populated view
    /// and a bound synthetic mask, `DotStats` (dots drawn and in view,
    /// people in view, slots, population drawn, bytes) are identical for
    /// every combination of `overlap_gamma` 0 / 0.35 and relocation off /
    /// on -- with the mask bound in every run.
    #[test]
    fn the_counts_are_identical_across_the_fade_and_relocation() {
        use crate::overlay::dotgrid::placement::SyntheticMask;
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let camera = crate::render::Camera {
            center_lon: 90.4125,
            center_lat: 23.8103,
            zoom: 15.5,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: GHSL_ATTRIBUTION.to_string(),
        };
        // Every cell within 30 of the camera's populated, 0-49 people.
        let (row_c, col_c) = (((90.0 - camera.center_lat) * 120.0) as i64, ((camera.center_lon + 180.0) * 120.0) as i64);
        let descriptor = d.clone();
        let source = Arc::new(MemoryGridSource::new(d, move |key| {
            let w = grid::cell_window(&descriptor, key.level, key.tile);
            let mut cells = Vec::new();
            for r in (row_c - 30).max(i64::from(w.row0))..(row_c + 30).min(i64::from(w.row0 + w.rows)) {
                for c in (col_c - 30).max(i64::from(w.col0))..(col_c + 30).min(i64::from(w.col0 + w.cols)) {
                    let i = (r - i64::from(w.row0)) as u32 * w.cols + (c - i64::from(w.col0)) as u32;
                    let pop = ((i.wrapping_mul(2_654_435_761) >> 16) % 50) as f32;
                    if key.level == 0 && pop > 0.0 {
                        cells.push((i, pop, pop));
                    }
                }
            }
            let total = cells.iter().map(|c| f64::from(c.1)).sum();
            GridTile { cells, total, ..Default::default() }
        }));
        let region = crate::render::dot_mask::relocation_region(&camera, 0.0, CELL).expect("30\" cells are big at z15.5");
        let grid = crate::render::dot_mask::MaskGrid::plan(camera.zoom, region, CELL, 4096).expect("a grid");
        let mask = crate::render::dot_mask::DotMaskGpu::from_texels(&ctx, grid, &SyntheticMask::Checker { tenths: 3 }.texels(&grid));
        let frame = crate::render::globe::globe_frame(&camera);
        let globe = crate::render::globe::GlobeUpload::new(&frame, &camera, crate::mvt::MVT_EXTENT as f32, glam::DVec3::ZERO, 1.0);
        assert_eq!(globe.transition(), 0.0, "a flat frame");
        let mut seen = Vec::new();
        for gamma in [0.0, 0.35] {
            for relocate in [false, true] {
                let style = DotStyle { overlap_gamma: gamma, relocate, ..DotStyle::default() };
                let mut layers = crate::overlay::host::DotLayers::new();
                layers.set_source("pop", source.clone(), style);
                for _ in 0..10 {
                    layers.frame(&camera, &crate::render::globe::GlobeCover::FLAT, crate::timing::Instant::now());
                    while let Some(job) = layers.next_job() {
                        layers.land(job.run(|_| Ok(Vec::new())));
                    }
                }
                let mut gpu = crate::overlay::host::DotLayersGpu::default();
                gpu.sync(&mut layers, &ctx, &pipeline, &camera, &globe, mask.key().map(|k| (&mask, &k.grid)));
                let bound = gpu.mask_generations();
                assert!(!bound.is_empty(), "gamma {gamma} relocate {relocate}: nothing drawn");
                // Off binds the mask too, with `mask_k` 0
                // (`relocate_off_writes_mask_k_zero_on_every_draw`).
                assert!(bound.iter().all(Option::is_some), "gamma {gamma} relocate {relocate}: {bound:?}");
                seen.push(((gamma, relocate), layers.stats("pop").expect("the layer")));
            }
        }
        let (first, stats) = seen[0];
        assert!(stats.dots_drawn > 1_000 && stats.people_in_view > 10.0 && stats.dots_in_view > 0, "{stats:?}");
        for (variant, other) in &seen[1..] {
            assert_eq!(*other, stats, "{variant:?} vs {first:?}");
        }
    }

}
