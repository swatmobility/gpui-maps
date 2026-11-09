//! Which tiles a frame's cover contains -- gpui-free, shared by every shell.
//!
//! `src/map_view.rs`'s `MapView::tiles_for_zoom` already asked
//! [`GlobeCover`] before falling back to a flat/planar cover -- that is why
//! native never showed a blank hemisphere. `web/src/app.rs`'s `cover()` and
//! `prefetch_cover()`, and `ios/src/app.rs`'s `cover_at()`, never asked it at
//! all: they called `crate::geo::visible_tiles` unconditionally, a flat
//! Mercator viewport bbox whose x range is clamped to `[0, n-1]`. A flat bbox
//! cannot cover a sphere showing a hemisphere, so the browser rendered a
//! globe with roughly half of it blank grey (Liberty's own
//! `background-color: #f8f4f0` showing through wherever no tile was ever
//! requested).
//!
//! [`tile_cover`] is the fix: the one decision of whether THIS frame's cover
//! comes from the globe or from the plane, pulled out so every shell goes
//! through it and none can quietly go back to asking the flat function on
//! its own -- see `tests/space_plan_parity.rs`'s
//! `no_shell_calls_geo_visible_tiles_directly`, which fails the build if one
//! does.
//!
//! **`crate::geo::visible_tiles` is no longer called for the flat cover at
//! all.** [`flat_cover`] is the ONE planar cover every shell uses: a
//! frustum-trimmed traversal that ranks tiles nearest the camera first to
//! decide which survive [`MAX_COVER_TILES`], then hands the survivors back in
//! `geo::visible_tiles`' own column-major `(x, y)` order (ruling F3: at pitch
//! 0 the draw list, and so every equal-priority label contest, is unchanged).
//! Plan Task 13's `tests/one_cover_for_every_shell.rs` is the build gate
//! that will fail if a shell calls anything else.
//!
//! # Why one formula now, when the frame-core design said two
//!
//! There used to be two flat formulas, and unifying them was an explicit
//! non-goal of the frame-core design
//! (`docs/superpowers/specs/2026-09-22-frame-core-extraction-design.md`,
//! "Making the shells identical"). That non-goal rested on one premise:
//! "neither shell varies pitch yet". The 3D camera work
//! (`docs/superpowers/specs/2026-09-24-3d-camera-parity-design.md`) removes
//! that premise, since iOS and the browser both tilt now. The old browser/iOS
//! formula (`flat_cover_scaled`, retired) had no pitch term and loaded too
//! few tiles the moment either tilted. Native's (`flat_cover_pitch_aware`,
//! retired) selected a square around the `ground_radius` circle about the
//! centre. At 60 degrees of pitch and this renderer's 60-degree FOV that
//! circle is the whole world, so it could not be enumerated at all. See the
//! measurements in `docs/measurements/2026-09-24-tilted-tile-cover.md`.
//!
//! `src/geo.rs`'s `visible_tiles_are_clamped_to_valid_range` still pins the
//! flat clamp of `geo::visible_tiles` itself; nothing here touches it.

use crate::geo::TileId;
use crate::render::camera::Camera;
use crate::render::globe::GlobeCover;

/// The cover for one frame at `zoom`: the globe's cover when this frame has
/// one ([`GlobeCover::is_globe`]), the caller's flat/planar cover otherwise.
///
/// This is [`MapView::tiles_for_zoom`]'s own `match globe.cover(zoom) { .. }`
/// (`src/map_view.rs`), pulled out so a shell cannot skip it. `flat_cover` is
/// only invoked when the globe reports `None` -- lazily, so a frame with the
/// globe up never pays for a flat-cover computation nothing will use.
///
/// Plain data in, a plain `Vec<TileId>` out: no `&self`, no `gpui`. Callers
/// still resolve their own zoom (clamped to the source's max) before
/// reaching this function, and hand it `|| flat_cover(camera, zoom).tiles`
/// for the planar arm.
///
/// [`MapView::tiles_for_zoom`]: ../../map_view/struct.MapView.html#method.tiles_for_zoom
#[must_use]
pub fn tile_cover(zoom: u8, globe: &GlobeCover, flat_cover: impl FnOnce() -> Vec<TileId>) -> Vec<TileId> {
    match globe.cover(zoom) {
        Some(cover) => cover,
        None => flat_cover(),
    }
}

/// How many tiles the retired `flat_cover_pitch_aware` WOULD have returned
/// at `zoom`, without building them: `geo::visible_tiles`' own index
/// arithmetic over the same `pitch_aware_tile_bbox` square, with the two
/// ranges multiplied. The "before frustum trimming" column of the
/// tilted-cover measurement, which at 60 degrees is the whole world and
/// cannot be enumerated.
#[must_use]
pub fn bbox_tile_count(camera: &Camera, zoom: u8) -> u64 {
    let (min_lon, min_lat, max_lon, max_lat) = crate::render::camera::pitch_aware_tile_bbox(camera);
    let center = ((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0);
    let width_px = (max_lon - min_lon) / 360.0 * 2f64.powi(i32::from(zoom)) * crate::geo::TILE_SIZE;
    let (center_x, center_y) = crate::geo::lon_lat_to_tile_xy(center.0, center.1, f64::from(zoom));
    let half_tiles = (width_px / crate::geo::TILE_SIZE) / 2.0;
    let n = 1i64 << zoom;
    let span = |c: f64| {
        let lo = ((c - half_tiles).floor() as i64).clamp(0, n - 1);
        let hi = ((c + half_tiles).ceil() as i64).clamp(0, n - 1);
        (hi - lo + 1) as u64
    };
    span(center_x) * span(center_y)
}

/// Ground distance, in world units, from `from` to the nearest point of
/// `tile`. A parent is never farther than any of its children, which is what
/// makes [`frustum_cover`]'s best-first order exact.
pub(crate) fn ground_distance_to_tile(tile: TileId, from: glam::DVec2) -> f64 {
    let size = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << tile.z);
    let (x0, y0) = (f64::from(tile.x) * size, f64::from(tile.y) * size);
    let nearest = glam::DVec2::new(from.x.clamp(x0, x0 + size), from.y.clamp(y0, y0 + size));
    (nearest - from).length()
}

/// Conservative frustum test on a tile's four ground corners, in f64 against
/// a frustum built once.
///
/// Wrong only toward "visible": a tile with ALL corners behind the near
/// plane is rejected outright (a convex quad whose corners are all behind a
/// plane is wholly behind it). A tile that straddles the near plane (some
/// corners in front, some behind) is NOT judged on its raw, unclipped
/// corners -- behind the eye, clip `x`/`y` flip sign, so an unclipped
/// straddling quad's "is every corner outside the same side plane" test is
/// meaningless and silently accepts geometry nowhere near the view. Fix
/// round 1 measured exactly this: at 45 degrees pitch, bearing 0, the near
/// plane cuts the ground along a line under the eye's ground point, and
/// EVERY tile in that one row straddled it -- accepting a whole `2^z`-wide
/// row of tiles that were nowhere near the frustum's side planes. Instead,
/// a straddling quad is clipped against the near plane first
/// (Sutherland-Hodgman on the quad's actual edge loop -- valid because clip
/// coordinates are affine in world position, so clipping in clip space and
/// clipping the world-space quad agree), and the side/far tests below run
/// on the clipped polygon, which sits entirely at or in front of the near
/// plane.
///
/// The side and far tests carry a 1e-6 relative margin. That margin does
/// NOT establish that f64 here never rejects a tile the f32
/// `tile_in_frustum` keeps -- at z16, f32 world coordinates near 400 carry
/// about 3e-5 of rounding, i.e. roughly 2.5e-3 relative error in clip
/// `x`/`w`, which swamps a 1e-6 margin. `the_frustum_cover_has_no_holes`
/// (below) is what actually establishes the no-holes property, by testing
/// unprojected ground points directly rather than trusting this margin to
/// cover that gap (see IMPORTANT 2 of fix round 1: `tile_in_frustum` itself
/// has a genuine false positive at high pitch/bearing that no f64 margin
/// here could paper over, since the two functions test different things).
fn tile_may_be_visible(tile: TileId, view_projection: &glam::DMat4, near: f64, far: f64) -> bool {
    let size = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << tile.z);
    let (x0, y0) = (f64::from(tile.x) * size, f64::from(tile.y) * size);
    // Loop order (adjacent corners share an edge), NOT the min/max "diagonal"
    // order -- Sutherland-Hodgman clips edges, so it needs the quad's actual
    // boundary walk.
    let loop_corners = [(x0, y0), (x0 + size, y0), (x0 + size, y0 + size), (x0, y0 + size)];
    let clip = loop_corners.map(|(x, y)| *view_projection * glam::DVec4::new(x, y, 0.0, 1.0));
    clip_quad_may_be_visible(clip, near, far)
}

/// [`tile_may_be_visible`]'s test on the quad's four clip-space corners, in
/// loop order. Split out so a degenerate quad can be fed to it directly.
fn clip_quad_may_be_visible(clip: [glam::DVec4; 4], near: f64, far: f64) -> bool {
    let behind = clip.iter().filter(|c| c.w <= near).count();
    if behind == 4 {
        return false;
    }
    // A fixed-capacity polygon on the stack, not a `Vec`: this runs for
    // every tile `frustum_cover` pops, every frame. For an exact affine `w`,
    // clipping a quad against ONE plane adds at most one vertex, so five
    // would do. EIGHT because `w` is not always exact: a NaN (which counts
    // as neither behind above nor in front below) or a rounded `w` hugging
    // the plane can make the corners alternate in/out, and each of the four
    // edges then contributes its start corner AND a crossing. Eight is the
    // most the loop below can ever write, so it cannot index out of bounds
    // in the per-frame path (Task 9 minor 2). Whatever such a polygon holds,
    // the tests below answer "may be visible" for NaN (every comparison
    // with NaN is false), which is the direction this function may err in.
    let mut polygon = [glam::DVec4::ZERO; 8];
    let mut len = 0;
    if behind == 0 {
        polygon[..4].copy_from_slice(&clip);
        len = 4;
    } else {
        for i in 0..4 {
            let a = clip[i];
            let b = clip[(i + 1) % 4];
            let (a_in, b_in) = (a.w > near, b.w > near);
            if a_in {
                polygon[len] = a;
                len += 1;
            }
            if a_in != b_in {
                let t = (near - a.w) / (b.w - a.w);
                polygon[len] = a + (b - a) * t;
                len += 1;
            }
        }
    }
    let polygon = &polygon[..len];
    if polygon.is_empty() {
        // Only a NaN `w` gets here (fewer than four corners behind, yet none
        // in front), and `all` over nothing would answer "outside".
        return true;
    }
    let slack = |w: f64| w * (1.0 + 1e-6);
    let all = |outside: &dyn Fn(&glam::DVec4) -> bool| polygon.iter().all(outside);
    !(all(&|c| c.x > slack(c.w))
        || all(&|c| c.x < -slack(c.w))
        || all(&|c| c.y > slack(c.w))
        || all(&|c| c.y < -slack(c.w))
        || all(&|c| c.w > slack(far)))
}

/// Every tile of level `zoom` that may be inside the view frustum, NEAREST
/// THE EYE'S GROUND POINT FIRST, stopping after `limit`.
///
/// A best-first descent from the z0 tile: pop the nearest unexpanded tile,
/// drop it if it cannot be visible, emit it if it is at `zoom`, otherwise
/// push its four children. A parent's distance never exceeds a child's
/// ([`ground_distance_to_tile`]), so tiles come out in non-decreasing
/// distance. The first `limit` are exactly the `limit` nearest visible
/// tiles, and the work is proportional to what is emitted, not to the
/// frustum's area. The frustum reaches the horizon at high pitch, and its
/// area is then unbounded.
///
/// This is the UNIFORM-zoom walk: every tile at `zoom`. [`flat_cover`]
/// uses it whenever the distance LOD is off (at or below
/// [`lod_min_pitch`]), which is what keeps every such frame -- pitch 0
/// above all -- bit-identical to the cover before the LOD existed. Above
/// that pitch [`flat_cover`] walks [`cover_walk`] with a per-tile target
/// zoom instead.
///
/// F3 (Task 9 ruling): this nearest-first order is for the CALLER to use
/// when choosing which tiles survive a cap; [`flat_cover`] re-sorts its
/// survivors back into `geo::visible_tiles`' order. This function returns
/// the ranked order directly (`print_tilted_cover_table` reads index 511
/// off it).
///
/// Production never passes an unbounded `limit`: at 60 degrees the
/// frustum reaches the horizon and an unbounded walk enumerates ~4^zoom
/// tiles. [`flat_cover`] is the one production caller.
#[must_use]
pub(crate) fn frustum_cover(camera: &Camera, zoom: u8, limit: usize) -> Vec<TileId> {
    cover_walk(camera, |_, _| zoom, None, limit)
}

/// The one quadtree walk behind every flat cover: best-first from z0 by
/// ground distance from the eye's ground point, each visible tile emitted
/// once its own zoom reaches `target(tile, distance)`, otherwise split.
///
/// - `target` is the zoom this tile's ground wants. Constant for the
///   uniform cover ([`frustum_cover`]); distance-dependent for the LOD
///   cover ([`Lod::target`]). Because a tile is emitted OR split, never
///   both, no emitted tile is another's ancestor, and because every
///   visible tile on the path from z0 to a visible ground point is walked
///   until one is emitted, no visible ground point is left uncovered (the
///   frustum test only ever errs toward "visible").
/// - `horizon`, when set, drops a tile whose NEAREST ground point lies
///   farther than it -- MapLibre's far plane stops the ground at
///   `maxMercatorHorizonAngle` in the same spirit (see [`Lod::horizon`]).
/// - `limit` bounds the emitted tiles; the walk stops at it.
fn cover_walk(camera: &Camera, target: impl Fn(TileId, f64) -> u8, horizon: Option<f64>, limit: usize) -> Vec<TileId> {
    use std::cmp::Reverse;
    let view_projection = camera.view_projection_f64();
    let (near, far) = crate::render::camera::near_far(camera);
    let (near, far) = (f64::from(near), f64::from(far));
    let eye = crate::render::camera::eye_ground_point(camera);
    let key = |t: TileId| Reverse((ground_distance_to_tile(t, eye).to_bits(), t.z, t.x, t.y));
    let mut heap = std::collections::BinaryHeap::new();
    heap.push(key(TileId { z: 0, x: 0, y: 0 }));
    let mut out = Vec::new();
    while let Some(Reverse((distance, z, x, y))) = heap.pop() {
        if out.len() >= limit {
            break;
        }
        let tile = TileId { z, x, y };
        if !tile_may_be_visible(tile, &view_projection, near, far) {
            continue;
        }
        let distance = f64::from_bits(distance);
        if horizon.is_some_and(|h| distance > h) {
            continue;
        }
        if z >= target(tile, distance) {
            out.push(tile);
            continue;
        }
        for dy in 0..2 {
            for dx in 0..2 {
                heap.push(key(TileId { z: z + 1, x: x * 2 + dx, y: y * 2 + dy }));
            }
        }
    }
    out
}

/// The most tiles one flat cover may hold.
///
/// Set in Task 9 of `docs/superpowers/plans/2026-09-24-3d-camera-parity.md`
/// by one rule over `docs/measurements/2026-09-24-tilted-tile-cover.md`: the
/// smallest multiple of 64 at or above 1.1x the largest frustum count at
/// pitch 0 or 45 degrees, and never below the old 512. The table's largest
/// such count is 64 (1290x2796, z16, 45 degrees); 1.1 x 64 = 70.4, the next
/// multiple of 64 is 128, and the floor lifts that to 512. Below 50 degrees
/// (where the horizon fade is zero) the cap must cut nothing;
/// `the_cap_never_cuts_a_frame_the_fade_cannot_hide` holds it to that.
///
/// Since the distance LOD ([`Lod`]) it is a SAFETY cap only: a tilted
/// cover is now a few hundred tiles at most on the measured viewports
/// (see the measurement's LOD table), so the cap no longer decides where
/// a 60-degree cover ends -- the LOD's horizon does. If it ever does cut,
/// it cuts nearest-first and says where in [`FlatCover::reach`], exactly
/// as before.
pub const MAX_COVER_TILES: usize = 512;

/// One frame's flat cover, and where it stopped.
#[derive(Debug, Clone, PartialEq)]
pub struct FlatCover {
    /// The tiles, finest zoom first, then in `geo::visible_tiles`' `(x, y)`
    /// order within one zoom -- NOT nearest first. Nearness decides only
    /// which tiles survive the cap; the order they are handed back in is the
    /// one every shell drew in before this cover existed, because
    /// equal-priority labels are placed in tile order and a reorder could
    /// change which one wins at pitch 0 (ruling F3). A uniform-zoom cover
    /// (every frame at or below [`lod_min_pitch`], so pitch 0) has one zoom,
    /// and the order is exactly `(x, y)`. Under the LOD the finer, nearer
    /// tiles come first, so their labels win collisions over the coarse
    /// far ground's.
    pub tiles: Vec<TileId>,
    /// Ground distance, in world units from the eye's ground point, of the
    /// nearest tile the cover LEFT OUT -- by the cap, or by the LOD's
    /// horizon ([`Lod::horizon`]), whichever is nearer. All ground closer
    /// than this is covered. `None` when nothing visible was left out:
    /// coarse far tiles are still covered ground, not a cut.
    pub reach: Option<f64>,
    /// Ground distance, in world units from the eye's ground point, of the
    /// nearest tile coarser than [`CoverZoom::building_floor`] -- where the
    /// building data ends. `None` without a floor, or when every tile in
    /// the cover carries buildings (the horizon fade, `reach`, then ends
    /// them). The extrusion shader fades buildings out before it
    /// (`building_fade_band`), so they never stop on a hard line.
    pub building_reach: Option<f64>,
}

/// Which zoom a flat cover is asked at, and what its distance LOD is
/// centred on.
///
/// A bare `u8` converts to the common case (the display cover): tiles at
/// `zoom`, the LOD centred on the camera's own zoom -- or on `zoom` when
/// that is finer, as for the next-zoom prefetch. The two other shapes are
/// built explicitly: [`CoverZoom::coarser`] for the coarse prefetch cover
/// and `raster_plan`'s imagery request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoverZoom {
    /// The finest zoom the cover uses: every tile at pitch 0, and the
    /// ground nearest the eye under the LOD. The source-clamped tile zoom
    /// for the display cover.
    pub zoom: u8,
    /// MapLibre's `requestedCenterZoom`: the continuous zoom at which a tile
    /// is drawn 1:1 at the look-at point. `None` means
    /// `max(camera.zoom, zoom)`.
    pub center_zoom: Option<f64>,
    /// Round the LOD's per-tile zoom instead of flooring it -- MapLibre's
    /// `roundZoom`, which raster sources set (`raster_cover_zoom` rounds
    /// for the same reason).
    pub round: bool,
    /// The coarsest zoom the LOD may drop to: the source's minimum. The
    /// tile sources here declare none, so 0.
    pub min_zoom: u8,
    /// **The building floor**: while a `fill-extrusion` layer draws, the
    /// coarsest tile zoom that still carries building data (13 for both
    /// Mapbox Streets v8 and OpenMapTiles; a z12 tile has no `building`
    /// layer at all). Under the LOD the cover keeps every tile nearest the
    /// eye at or above it until [`BUILDING_COVER_BUDGET`] tiles, then lets
    /// the LOD drop as before; [`FlatCover::building_reach`] says where the
    /// building-capable ground ends. `None` (every cover but the display
    /// cover of a map with buildings) changes nothing.
    pub building_floor: Option<u8>,
    /// **The eye camera's far cutoff** ([`crate::camera_control::EyeCamera`]):
    /// ground farther than this from the eye's ground point, in world units,
    /// is never covered -- no tile whose nearest point lies past it is
    /// requested. It replaces the LOD's own horizon ([`Lod::horizon`]),
    /// which an eye a metre over the road would put ~80 m out, and the
    /// eye fog ([`eye_fog`]) ends inside it. `None` (every map that has not
    /// opted into the eye camera) changes nothing.
    pub cutoff: Option<f64>,
}

/// The most tiles a cover may hold while it keeps the ground nearest the eye
/// at [`CoverZoom::building_floor`] or finer: the tilted-cover budget (105,
/// `docs/measurements/2026-09-24-tilted-tile-cover.md`'s ceiling for a
/// 50-degree cover). Past it the far ground drops to the LOD's own zoom and
/// the buildings end, faded (`FlatCover::building_reach`).
pub const BUILDING_COVER_BUDGET: usize = 105;

/// How many tiles past the LOD's own cover the building floor may add.
/// Every z12 tile the floor refines is four z13 tiles of buildings, so an
/// unbounded floor more than doubled a 60-degree Manhattan frame's
/// triangles (13 tiles, 1.6M triangles, to 53 and 3.7M) and tripled
/// Shinjuku's; 8 extra still cost Shinjuku +55%. At 4, 1024 px, z16,
/// pitch 60: Manhattan 13 -> 16 tiles (+12% triangles), Shinjuku 13 -> 15
/// (+15%), Singapore 11 -> 15 (+26%). Frame time matters more than far
/// buildings, and the fade hides where they end.
pub const BUILDING_EXTRA_TILES: usize = 4;

/// Bisection steps for the floored band's extent: 1/1024 of the visible
/// ground's depth.
const BUILDING_BAND_BISECTIONS: u32 = 10;

/// The coarsest tile zoom that carries a `building` layer in both schemas
/// this renderer reads: Mapbox Streets v8 and OpenMapTiles (OpenFreeMap)
/// both start it at z13; their z12 tiles have none. The display cover's
/// [`CoverZoom::building_floor`] while buildings draw.
pub const BUILDING_SOURCE_MIN_ZOOM: u8 = 13;

/// The building floor for a source whose building layer starts at
/// `stated` (TileJSON `vector_layers[].minzoom`,
/// `TileSource::layer_min_zoom`), or [`BUILDING_SOURCE_MIN_ZOOM`] when the
/// source does not say.
#[must_use]
pub fn building_floor_for(stated: Option<u8>) -> u8 {
    stated.unwrap_or(BUILDING_SOURCE_MIN_ZOOM)
}

impl From<u8> for CoverZoom {
    fn from(zoom: u8) -> Self {
        Self { zoom, center_zoom: None, round: false, min_zoom: 0, building_floor: None, cutoff: None }
    }
}

impl CoverZoom {
    /// The coarse prefetch cover for a display cover at `tile_zoom`:
    /// tiles at `coarse`, and its LOD shifted down by the same
    /// `tile_zoom - coarse` levels, so each prefetch tile is (up to the
    /// frustum's coarser trim) the `delta`-level ancestor of the display
    /// cover's tiles under it. Centring it on the camera's zoom instead
    /// would keep the coarse cover at `coarse` out to 2^delta times
    /// farther than the display cover keeps its own zoom, which is the
    /// 512-tile prefetch backlog again.
    #[must_use]
    pub fn coarser(camera: &Camera, tile_zoom: u8, coarse: u8) -> Self {
        let delta = f64::from(tile_zoom.saturating_sub(coarse));
        Self {
            zoom: coarse,
            center_zoom: Some(camera.zoom.max(f64::from(tile_zoom)) - delta),
            round: false,
            min_zoom: 0,
            building_floor: None,
            cutoff: None,
        }
    }

    /// This request with the eye camera's far cutoff ([`Self::cutoff`]).
    #[must_use]
    pub fn with_cutoff(self, cutoff: Option<f64>) -> Self {
        Self { cutoff: cutoff.filter(|c| c.is_finite() && *c > 0.0), ..self }
    }
}

/// MapLibre GL JS's `maxMercatorHorizonAngle`
/// (`src/geo/projection/mercator_utils.ts`, v6.7.0): the angle from the
/// nadir past which Mercator ground counts as horizon.
pub const LOD_HORIZON_ANGLE_DEG: f64 = 89.25;
/// `defaultMaxZoomLevelsOnScreen` (`covering_tiles.ts`).
const LOD_MAX_ZOOM_LEVELS_ON_SCREEN: f64 = 9.314;
/// `defaultTileCountMaxMinRatio` (`covering_tiles.ts`).
const LOD_TILE_COUNT_MAX_MIN_RATIO: f64 = 3.0;

/// The pitch at or below which the cover has ONE zoom: MapLibre's
/// `MercatorCoveringTilesDetailsProvider::allowVariableZoom`,
/// `clamp(78.5 - zfov / 2, 0, 60)` degrees, with this renderer's 60-degree
/// vertical FOV and no roll -- 48.5 degrees. Every frame at or below it
/// (pitch 0 above all) takes the uniform walk, unchanged.
#[must_use]
pub fn lod_min_pitch() -> f64 {
    lod_min_pitch_at(crate::render::camera::FOV_Y_RADIANS_F64)
}

/// [`lod_min_pitch`] for a camera of vertical FOV `fov_y` radians
/// (`Camera::fov_y_rad`): a wider FOV reaches the horizon at a lower
/// pitch, so the variable zoom starts lower.
#[must_use]
pub fn lod_min_pitch_at(fov_y: f64) -> f64 {
    (78.5 - fov_y.to_degrees() / 2.0).clamp(0.0, 60.0).to_radians()
}

/// `integralOfCosXByP` (`covering_tiles.ts`): the definite integral of
/// `cos(x)^p` from `x1` to `x2`, by ten-point midpoint rule, verbatim.
fn integral_of_cos_x_by_p(p: f64, x1: f64, x2: f64) -> f64 {
    const POINTS: u32 = 10;
    let dx = (x2 - x1) / f64::from(POINTS);
    (0..POINTS).map(|i| dx * (x1 + (f64::from(i) + 0.5) / f64::from(POINTS) * (x2 - x1)).cos().powf(p)).sum()
}

/// The distance LOD: MapLibre GL JS's `createCalculateTileZoomFunction`
/// (`src/geo/projection/covering_tiles.ts`, v6.7.0, BSD-3-Clause), with its
/// default `maxZoomLevelsOnScreen = 9.314` and `tileCountMaxMinRatio = 3`,
/// evaluated with this renderer's 60-degree vertical FOV.
///
/// Per tile (`distance_2d` = ground distance from the eye's ground point to
/// the tile's nearest point, as MapLibre's `distanceToTile2d` against the
/// tile's AABB), the desired zoom is
///
/// ```text
/// centre + log2(d_centre_3d / d_tile_3d / max(0.5, cos(fov / 2)))
///        + b * log2(cos(tile_pitch)) / 2
///        - log2(max(1, tile_count / tile_count_pitch0 / ratio)) / 2
/// ```
///
/// where `b` (`pitchTileLoadingBehavior`) is fixed so that, with the
/// horizon at the top of the screen, about `maxZoomLevelsOnScreen` levels
/// are on screen, and `tile_pitch = atan(distance_2d / eye_height)`. The
/// first term is the plain perspective rule -- one zoom level coarser per
/// doubling of 3D distance past the look-at point; the second drops a
/// little more as the ground turns edge-on toward the horizon.
///
/// Three deliberate departures, each in the direction of "no hole, no
/// regression":
/// - the result is clamped to `[min_zoom, zoom]`, where MapLibre allows up
///   to the SOURCE maximum near the eye (finer than the centre) and DROPS
///   a tile below `minzoom` (a hole); here the near ground stays at the
///   cover's own zoom, as before the LOD, and the far ground never goes
///   below the source minimum;
/// - [`Lod::horizon`] is MapLibre's far plane expressed as a ground
///   distance, applied to the walk rather than to the projection;
/// - it only runs above [`lod_min_pitch`], exactly as MapLibre gates it.
struct Lod {
    center_zoom: f64,
    zoom: u8,
    min_zoom: u8,
    round: bool,
    /// Eye height over the ground (`distanceToTileZ`), world units.
    eye_height: f64,
    /// Eye to look-at point (`distanceToCenter3D`), world units.
    center_distance: f64,
    fov_deg: f64,
    /// `pitchTileLoadingBehavior`.
    pitch_behaviour: f64,
    /// `-log2(max(1, tileCount / tileCountPitch0 / ratio)) / 2`: depends on
    /// the camera only, so computed once.
    tile_count_term: f64,
}

impl Lod {
    fn new(camera: &Camera, request: CoverZoom) -> Self {
        let fov_deg = camera.fov_y_rad().to_degrees();
        let horizon = LOD_HORIZON_ANGLE_DEG;
        let pitch_behaviour = 2.0
            * ((LOD_MAX_ZOOM_LEVELS_ON_SCREEN - 1.0)
                / ((horizon - fov_deg).to_radians().cos() / horizon.to_radians().cos()).log2()
                - 1.0);
        let center_distance = camera.distance();
        let pitch = f64::from(camera.pitch);
        let eye_height = center_distance * pitch.cos();
        // `centerPitch = acos(distanceToTileZ / distanceToCenter3D)`: the
        // camera's own pitch, since the look-at point is on the ground.
        let center_pitch = (eye_height / center_distance).clamp(-1.0, 1.0).acos();
        let half_fov = (fov_deg / 2.0).to_radians();
        let p = pitch_behaviour - 1.0;
        let tile_count_pitch0 = 2.0 * integral_of_cos_x_by_p(p, 0.0, half_fov);
        let highest = horizon.to_radians().min(center_pitch + half_fov);
        let lowest = highest.min(center_pitch - half_fov);
        let tile_count = integral_of_cos_x_by_p(p, lowest, highest);
        let tile_count_term = -(tile_count / tile_count_pitch0 / LOD_TILE_COUNT_MAX_MIN_RATIO).max(1.0).log2() / 2.0;
        Self {
            center_zoom: request.center_zoom.unwrap_or_else(|| camera.zoom.max(f64::from(request.zoom))),
            zoom: request.zoom,
            min_zoom: request.min_zoom.min(request.zoom),
            round: request.round,
            eye_height,
            center_distance,
            fov_deg,
            pitch_behaviour,
            tile_count_term,
        }
    }

    /// The continuous desired zoom of ground `distance_2d` from the eye's
    /// ground point (MapLibre's `thisTileDesiredZ`, before rounding).
    fn desired(&self, distance_2d: f64) -> f64 {
        let tile_pitch = (distance_2d / self.eye_height).atan();
        let tile_distance_3d = distance_2d.hypot(self.eye_height);
        let cos_half_fov = (self.fov_deg / 2.0).to_radians().cos().max(0.5);
        self.center_zoom
            + (self.center_distance / tile_distance_3d / cos_half_fov).log2()
            + self.pitch_behaviour * tile_pitch.cos().log2() / 2.0
            + self.tile_count_term
    }

    /// The integer zoom a tile whose nearest ground is `distance_2d` away
    /// should be emitted at, clamped to `[min_zoom, zoom]`.
    fn target(&self, distance_2d: f64) -> u8 {
        let desired = self.desired(distance_2d);
        let desired = if self.round { desired.round() } else { desired.floor() };
        if desired.is_nan() {
            return self.zoom;
        }
        desired.clamp(f64::from(self.min_zoom), f64::from(self.zoom)) as u8
    }

    /// MapLibre's `maxMercatorHorizonAngle` as a ground distance: ground
    /// farther than `eye_height * tan(89.25 deg)` from the eye's ground
    /// point is past the horizon, and the walk leaves it out (reporting it
    /// as [`FlatCover::reach`], so the fade hides it). On screen only
    /// within 0.75 degrees of the true horizon -- above about 59.25 degrees
    /// of pitch at this FOV -- so it cuts nothing at any lower pitch.
    fn horizon(&self) -> f64 {
        self.eye_height * LOD_HORIZON_ANGLE_DEG.to_radians().tan()
    }
}

/// The farthest any on-screen ground lies from the eye's ground point, or
/// infinity when the top of the screen shows sky (a corner ray misses the
/// ground).
///
/// The ground the viewport shows is a convex polygon whose far vertices are
/// the screen's corner rays, so its farthest point from any fixed point is
/// one of them; the top-edge midpoint is sampled as well, for free
/// margin. Used by the LOD walk to drop a tile whose NEAREST point is
/// farther than any visible ground: the frustum test errs toward "visible"
/// for a big coarse tile straddling the near plane, and under the LOD such
/// a tile can already be at its target zoom, so it would be emitted (and
/// fetched) instead of being split and discarded. Also decides whether the
/// LOD's horizon is on screen at all.
fn visible_ground_extent(camera: &Camera) -> f64 {
    let eye = crate::render::camera::eye_ground_point(camera);
    let (w, h) = (camera.viewport_width_px, camera.viewport_height_px);
    let mut extent: f64 = 0.0;
    for (sx, sy) in [(0.0, 0.0), (w, 0.0), (w / 2.0, 0.0), (0.0, h), (w, h)] {
        match crate::render::camera::unproject_to_ground_f64(camera, sx, sy) {
            Some(g) => extent = extent.max((g.truncate() - eye).length()),
            None => return f64::INFINITY,
        }
    }
    // Relative slack for rounding in the unprojection.
    extent * (1.0 + 1e-6)
}

/// THE planar cover, for every shell and every zoom a shell asks at
/// (display, prefetch, imagery).
///
/// At or below [`lod_min_pitch`]: the frustum's tiles at `zoom`, ranked
/// nearest the eye's ground point first ([`frustum_cover`]), cut at
/// [`MAX_COVER_TILES`], then put back in `(x, y)` order (see
/// [`FlatCover::tiles`]) -- bit-identical to the cover before the LOD.
///
/// Above it: a MIXED-zoom quadtree cover ([`Lod`], MapLibre's
/// distance-based level of detail): the cover's own `zoom` near the eye,
/// one level coarser per doubling of distance past the look-at point and a
/// little more toward the horizon, never below `min_zoom`, stopping at
/// the LOD's horizon. No tile is another's ancestor, and every visible
/// ground point short of [`FlatCover::reach`] is in exactly one tile. The
/// safety cap still applies, nearest first.
///
/// One tile past the cap is asked for so the cut is known and measured,
/// never guessed; the traversal is always bounded.
///
/// Works in world coordinates, so a cover at a source-clamped `zoom` below
/// the camera's (overzoom) is correct by construction -- the pixel-space
/// division the retired browser/iOS formula needed has no counterpart here.
#[must_use]
pub fn flat_cover(camera: &Camera, zoom: impl Into<CoverZoom>) -> FlatCover {
    let request = zoom.into();
    let lod_on = f64::from(camera.pitch) > lod_min_pitch_at(camera.fov_y_rad());
    let walk = if lod_on {
        let lod = Lod::new(camera, request);
        let extent = visible_ground_extent(camera);
        // The eye camera's cutoff replaces the LOD's horizon: from a metre
        // over the road that horizon is ~80 m out, far short of the view.
        let horizon = request.cutoff.unwrap_or_else(|| lod.horizon());
        let stop = Some(horizon.min(extent));
        let base = cover_walk(camera, |_, distance| lod.target(distance), stop, MAX_COVER_TILES + 1);
        let tiles = match request.building_floor.map(|f| f.min(request.zoom)) {
            // The floor only matters where the LOD would drop below it.
            Some(floor) if base.iter().any(|t| t.z < floor) => {
                let budget = (base.len() + BUILDING_EXTRA_TILES).min(BUILDING_COVER_BUDGET);
                // Floored out to `band`, the LOD's own zoom past it.
                let walk = |band: f64| {
                    cover_walk(camera, |_, d| if d < band { lod.target(d).max(floor) } else { lod.target(d) }, stop, budget + 1)
                };
                let all = walk(f64::INFINITY);
                if all.len() <= budget {
                    all
                } else {
                    // The farthest band that fits the budget: the count
                    // grows with the band, so bisect it (each walk is a
                    // few dozen tiles).
                    let (mut fits, mut over) = (0.0, horizon.min(extent));
                    for _ in 0..BUILDING_BAND_BISECTIONS {
                        let mid = 0.5 * (fits + over);
                        if walk(mid).len() <= budget {
                            fits = mid;
                        } else {
                            over = mid;
                        }
                    }
                    walk(fits)
                }
            }
            _ => base,
        };
        // Every tile holding ground nearer than the horizon has its nearest
        // point nearer too, so it was walked: all ground short of the
        // horizon is covered, and the horizon itself is the cut -- when
        // ground past it is on screen at all.
        (tiles, (extent > horizon).then_some(horizon))
    } else if let Some(cutoff) = request.cutoff {
        let tiles = cover_walk(camera, |_, _| request.zoom, Some(cutoff), MAX_COVER_TILES + 1);
        (tiles, (visible_ground_extent(camera) > cutoff).then_some(cutoff))
    } else {
        (frustum_cover(camera, request.zoom, MAX_COVER_TILES + 1), None)
    };
    let (mut tiles, horizon_cut) = walk;
    let capped = if tiles.len() > MAX_COVER_TILES {
        let cut = tiles.pop().expect("longer than the cap, so not empty");
        Some(ground_distance_to_tile(cut, crate::render::camera::eye_ground_point(camera)))
    } else {
        None
    };
    // Finest first, then `(x, y)`: exactly `(x, y)` for a one-zoom cover.
    tiles.sort_unstable_by_key(|t| (std::cmp::Reverse(t.z), t.x, t.y));
    let building_reach = request.building_floor.and_then(|floor| {
        let eye = crate::render::camera::eye_ground_point(camera);
        tiles
            .iter()
            .filter(|t| t.z < floor.min(request.zoom))
            .map(|&t| ground_distance_to_tile(t, eye))
            .min_by(f64::total_cmp)
    });
    FlatCover { tiles, reach: nearer_cut(capped, horizon_cut), building_reach }
}

/// The zoom levels a cover spans, finest first, for a status line:
/// `(finest, coarsest)`. `None` for an empty cover.
#[must_use]
pub fn zoom_span(tiles: &[TileId]) -> Option<(u8, u8)> {
    let finest = tiles.iter().map(|t| t.z).max()?;
    let coarsest = tiles.iter().map(|t| t.z).min()?;
    Some((finest, coarsest))
}

/// The status lines' `tiles z..` field: `"14"` for a one-zoom cover (so a
/// flat log line reads exactly as before the distance LOD), `"14..9"` for a
/// mixed one, and the tile zoom itself when the cover is empty.
#[must_use]
pub fn zoom_span_label(span: Option<(u8, u8)>, tile_zoom: u8) -> String {
    match span {
        Some((finest, coarsest)) if finest != coarsest => format!("{finest}..{coarsest}"),
        Some((finest, _)) => finest.to_string(),
        None => tile_zoom.to_string(),
    }
}

/// Below this pitch the horizon fade is exactly zero, so every frame the map
/// drew before the 3D camera work draws unchanged.
pub const FOG_MIN_PITCH: f32 = 50.0 * std::f32::consts::PI / 180.0;
/// The fade ends at this fraction of the nearest depth at which the cut can
/// appear on screen: strictly inside it, so no missing tile ever shows
/// unfogged. A design value; see the plan's Task 11.
pub const FOG_END_MARGIN: f64 = 0.98;
/// The fade starts this fraction of the way back from its end toward the
/// look-at point's depth. A design value.
pub const FOG_BAND_FRACTION: f64 = 0.3;
/// The absolute floor on the band's width, `end - start`, beside the
/// relative one (`1e-3 * end`): ruling F12. `shader.wgsl`'s `smoothstep` is
/// undefined for `edge0 >= edge1`, so [`horizon_fog`] never hands it one.
pub const FOG_MIN_BAND_EPSILON: f32 = 1e-12;

/// How many azimuths [`horizon_fog`] samples on the cut circle before
/// refining where the circle crosses the screen's edge.
const FOG_AZIMUTH_SAMPLES: u32 = 64;
/// Bisection steps per screen-edge crossing: 2^-40 of one sample's arc,
/// far below anything f32 depth can resolve.
const FOG_EDGE_BISECTIONS: u32 = 40;

/// The horizon fade for this frame, given where the cover was cut (`reach`,
/// from [`FlatCover::reach`]) and the colour the frame is cleared to.
///
/// The end is set from the NEAREST depth at which ground at or beyond the
/// cut distance is on screen. Depth (clip `w`) is affine over the ground
/// plane, and the ground the viewport shows is a convex polygon, so over
/// "on screen AND at least `reach` from the eye's ground point" the minimum
/// sits either on the circle of radius `reach` or at a corner of that
/// polygon. Both are searched:
///
/// - the circle: `FOG_AZIMUTH_SAMPLES` azimuths, and wherever two adjacent
///   samples disagree about being on screen, a bisection to the exact point
///   the circle crosses the screen's edge. On the visible arc depth is
///   LOWEST at those crossings (it grows toward the view direction), so a
///   grid alone would overshoot the true minimum by up to one sample step;
/// - the screen's four corners and four edge midpoints, unprojected to the
///   ground, counted when they lie at or beyond `reach` (ruling F12).
///
/// Every farther -- possibly missing -- tile is therefore at or past the
/// end. The band is then guaranteed non-degenerate in f32: `end >= start +
/// max(1e-3 * end, FOG_MIN_BAND_EPSILON)` (ruling F12).
#[must_use]
pub fn horizon_fog(camera: &Camera, reach: Option<f64>, background: Option<crate::style::Rgba>) -> crate::render::gpu::HorizonFog {
    use crate::render::gpu::HorizonFog;
    let (Some(reach), Some(background)) = (reach, background) else { return HorizonFog::NONE };
    if camera.pitch.is_nan() || camera.pitch < FOG_MIN_PITCH || !(reach.is_finite() && reach > 0.0) {
        return HorizonFog::NONE;
    }
    let Some(nearest_depth) = nearest_depth_beyond(camera, reach) else { return HorizonFog::NONE };
    let end = nearest_depth * FOG_END_MARGIN;
    let start = end - (end - camera.distance()).max(0.0) * FOG_BAND_FRACTION;
    let end = end as f32;
    if !(end.is_normal() && end > 0.0) {
        return HorizonFog::NONE;
    }
    let min_band = (1e-3 * end).max(FOG_MIN_BAND_EPSILON);
    let mut start = (start as f32).min(end - min_band);
    if start + min_band > end {
        // `(end - b) + b` can round one ulp past `end`; two bands back cannot.
        start = end - 2.0 * min_band;
    }
    HorizonFog { start, end, color: [background.r, background.g, background.b, background.a] }
}

/// Where the eye camera's distance fog starts, as a share of the depth
/// at which it ends: the near quarter of the view is clear, the rest
/// thickens toward the sky's horizon colour. A design value.
pub const EYE_FOG_START_FRACTION: f64 = 0.25;
/// The most of the fog's end a clear distance ([`eye_fog_clear`]) may keep
/// clear: past it the fade would be a step. A design value.
pub const EYE_FOG_MAX_CLEAR_FRACTION: f64 = 0.6;

/// **The eye camera's distance fog** ([`crate::camera_control::EyeCamera`]):
/// the ground, the lines and the extrusions thin toward `color` -- the
/// sky's horizon colour, which the eye frame also clears to -- ending
/// strictly inside the cover's `cutoff`, so the cut is never seen. Unlike
/// [`horizon_fog`] it is on at any pitch and starts near the eye
/// ([`EYE_FOG_START_FRACTION`]): from a metre over the road the look-at
/// point is a dozen metres out, so the horizon fade's band (from the
/// look-at depth to the cut) would leave the whole street unfogged and
/// then wall it off. `HorizonFog::NONE` when the cut is off screen.
#[must_use]
pub fn eye_fog(camera: &Camera, cutoff: f64, color: crate::style::Rgba) -> crate::render::gpu::HorizonFog {
    eye_fog_clear(camera, cutoff, 0.0, color)
}

/// [`eye_fog`] kept clear to at least `clear` view depth (world units):
/// the fog starts at the larger of [`EYE_FOG_START_FRACTION`] of its end
/// and `clear`, never closer than two minimum bands before the end. An
/// eye camera high over the city
/// (`camera_control::EyeCamera::clear_world`) passes the clear distance
/// its height earns; `clear <= 0` (or not finite) is [`eye_fog`] exactly.
#[must_use]
pub fn eye_fog_clear(camera: &Camera, cutoff: f64, clear: f64, color: crate::style::Rgba) -> crate::render::gpu::HorizonFog {
    use crate::render::gpu::HorizonFog;
    if camera.pitch.is_nan() || !(cutoff.is_finite() && cutoff > 0.0) {
        return HorizonFog::NONE;
    }
    let clear = if clear.is_finite() { clear.max(0.0) } else { 0.0 };
    // A high eye's cut can lie past every ground the frame shows (its
    // reach grew with height): the fog still thickens toward the cutoff,
    // as view depth, so the far city fades instead of ending sharp. With
    // no clear distance (every eye camera but an aerial one) an off-screen
    // cut is no fog, as before.
    let nearest_depth = match nearest_depth_beyond(camera, cutoff) {
        Some(d) => d,
        None if clear > 0.0 && camera.pitch > 0.0 => cutoff / FOG_END_MARGIN,
        None => return HorizonFog::NONE,
    };
    let end = (nearest_depth * FOG_END_MARGIN) as f32;
    if !(end.is_normal() && end > 0.0) {
        return HorizonFog::NONE;
    }
    let min_band = (1e-3 * end).max(FOG_MIN_BAND_EPSILON);
    let clear = clear.min(f64::from(end) * EYE_FOG_MAX_CLEAR_FRACTION);
    let start = ((f64::from(end) * EYE_FOG_START_FRACTION).max(clear) as f32).min(end - 2.0 * min_band);
    HorizonFog { start, end, color: [color.r, color.g, color.b, color.a] }
}

/// The share of the depth between the look-at point and the building cut
/// over which buildings sink away ([`building_fade_band`]).
pub const BUILDING_FADE_FRACTION: f64 = 0.35;

/// Where this frame's buildings fade out, as a view-depth band `[start,
/// end]` (clip `w`, the units the extrusion shader's fog depth is in), given
/// [`FlatCover::building_reach`]: the nearest depth at which ground at or
/// beyond the building cut is on screen, times [`FOG_END_MARGIN`], is the
/// end; the band reaches back [`BUILDING_FADE_FRACTION`] of the way to the
/// look-at point. Every building on a tile past the cut is at or beyond the
/// end, so the shader's fade (height and colour to zero by `end`) leaves
/// no hard line where the building data stops. `None` -- no fade -- when
/// nothing was cut or the cut is off screen.
#[must_use]
pub fn building_fade_band(camera: &Camera, building_reach: Option<f64>) -> Option<[f32; 2]> {
    let reach = building_reach.filter(|r| r.is_finite() && *r > 0.0)?;
    if camera.pitch.is_nan() {
        return None;
    }
    let end = nearest_depth_beyond(camera, reach)? * FOG_END_MARGIN;
    let start = end - (end - camera.distance()).max(0.0) * BUILDING_FADE_FRACTION;
    let (start, end) = (start as f32, end as f32);
    if !(end.is_normal() && end > 0.0) {
        return None;
    }
    let min_band = (1e-3 * end).max(FOG_MIN_BAND_EPSILON);
    Some([start.min(end - 2.0 * min_band), end])
}

/// The nearest view depth (clip `w`) at which ground at or beyond `reach`
/// (world units from the eye's ground point) is on screen; `None` when no
/// such ground is. [`horizon_fog`]'s search, shared with
/// [`building_fade_band`].
fn nearest_depth_beyond(camera: &Camera, reach: f64) -> Option<f64> {
    let view_projection = camera.view_projection_f64();
    let eye = crate::render::camera::eye_ground_point(camera);
    let clip_at = |p: glam::DVec2| view_projection * glam::DVec4::new(p.x, p.y, 0.0, 1.0);
    let on_screen = |c: glam::DVec4| c.w > 0.0 && c.x.abs() <= c.w && c.y.abs() <= c.w;
    let on_circle = |a: f64| clip_at(eye + reach * glam::DVec2::new(a.cos(), a.sin()));

    let mut nearest_depth = f64::INFINITY;
    let step = std::f64::consts::TAU / f64::from(FOG_AZIMUTH_SAMPLES);
    for i in 0..FOG_AZIMUTH_SAMPLES {
        let (a0, a1) = (step * f64::from(i), step * f64::from(i + 1));
        let (c0, c1) = (on_circle(a0), on_circle(a1));
        if on_screen(c0) {
            nearest_depth = nearest_depth.min(c0.w);
        }
        if on_screen(c0) != on_screen(c1) {
            // Keep `inside` on screen and close in on the edge from it.
            let (mut inside, mut outside) = if on_screen(c0) { (a0, a1) } else { (a1, a0) };
            for _ in 0..FOG_EDGE_BISECTIONS {
                let mid = 0.5 * (inside + outside);
                if on_screen(on_circle(mid)) {
                    inside = mid;
                } else {
                    outside = mid;
                }
            }
            nearest_depth = nearest_depth.min(on_circle(inside).w);
        }
    }
    let (w, h) = (camera.viewport_width_px, camera.viewport_height_px);
    for (sx, sy) in [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h), (w / 2.0, 0.0), (w, h / 2.0), (w / 2.0, h), (0.0, h / 2.0)] {
        let Some(ground) = crate::render::camera::unproject_to_ground_f64(camera, sx, sy) else { continue };
        let ground = ground.truncate();
        if (ground - eye).length() >= reach {
            let c = clip_at(ground);
            if c.w > 0.0 {
                nearest_depth = nearest_depth.min(c.w);
            }
        }
    }
    nearest_depth.is_finite().then_some(nearest_depth)
}

/// The nearer of two cover cuts, where `None` means "not cut".
#[must_use]
pub fn nearer_cut(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// THE frame's horizon fade -- the one call all three shells make, so the
/// decision cannot drift between them.
///
/// - `vector_reach` is this frame's vector cover cut ([`FlatCover::reach`]).
/// - `raster_reach` is the imagery cover's cut, `None` unless a raster
///   layer draws this frame. Imagery is covered at its own zoom
///   (`raster_plan::raster_cover_zoom`, up to the camera's + 2), so its cut
///   can land at about a quarter of the vector one; the fade ends inside the
///   NEARER cut, or the imagery's missing tiles show as a hard edge.
/// - `background` is the colour the flat frame clears to (see each shell).
/// - Off on every frame with any globe in it (`space_transition > 0`), as
///   before.
#[must_use]
pub fn frame_fog(
    camera: &Camera,
    vector_reach: Option<f64>,
    raster_reach: Option<f64>,
    background: Option<crate::style::Rgba>,
    space_transition: f32,
) -> crate::render::gpu::HorizonFog {
    if space_transition > 0.0 {
        return crate::render::gpu::HorizonFog::NONE;
    }
    horizon_fog(camera, nearer_cut(vector_reach, raster_reach), background)
}

/// The colour a web or iOS flat frame ACTUALLY clears to, as the fade's
/// target: the style's `background` when it has one, else the shell's own
/// fallback clear (`self.background`, white by default).
///
/// Those shells used to hand `frame_fog` the style's `background` alone,
/// so a style with no background layer got no fade at all while its frame
/// still cleared to the fallback -- the missing tiles past the cut showed
/// as a hard edge against it. Native does NOT use this: it clears to
/// TRANSPARENT in that case, and fading toward alpha 0 would draw a black
/// band (see `MapView::paint`).
#[must_use]
pub fn fog_background(style_background: Option<crate::style::Rgba>, fallback_clear: wgpu::Color) -> crate::style::Rgba {
    style_background.unwrap_or(crate::style::Rgba {
        r: fallback_clear.r as f32,
        g: fallback_clear.g as f32,
        b: fallback_clear.b as f32,
        a: fallback_clear.a as f32,
    })
}

/// A cover in FETCH order: nearest the eye's ground point first, ties in
/// the cover's own order. Only for the network queue -- the draw order is
/// the cover's, untouched. At 60 degrees a cover in `(x, y)` order put the
/// foreground at the back of the queue, so the ground under the camera
/// loaded last.
#[must_use]
pub fn nearest_first(tiles: &[TileId], camera: &Camera) -> Vec<TileId> {
    let eye = crate::render::camera::eye_ground_point(camera);
    let mut keyed: Vec<(f64, TileId)> = tiles.iter().map(|&t| (ground_distance_to_tile(t, eye), t)).collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
    keyed.into_iter().map(|(_, t)| t).collect()
}

/// The bake order for one frame: `cover` tiles with no mesh at all, then
/// `cover` tiles baked at a zoom bucket other than `display_zoom`, then the
/// coarse prefetch `cover`'s own leftovers -- the same three priority
/// classes `web/src/app.rs` and `ios/src/app.rs`'s `bake_some` have always
/// used, kept in that order, but each class is now sorted nearest-the-eye
/// first ([`nearest_first`]) instead of staying in `cover()`'s row-major
/// `(x, y)` draw order. At 60 degrees of pitch that draw order put the far
/// rows first, so the ground under the camera -- `missing_mesh`'s own front
/// row -- was baked LAST: this is the bake-side twin of the fetch fix.
///
/// `baked_zoom(tile)` is `None` for a tile with no mesh yet, `Some(z)` for
/// one baked at zoom bucket `z`. `is_decoded(tile)` gates the result to
/// tiles a shell can actually bake right now, exactly as both shells'
/// `.filter(|t| self.decoded.contains_key(t))` did before this function
/// existed. The returned `bool` is `true` only for a tile drawn from the
/// `coarse` prefetch cover (`web/src/app.rs`'s `is_prefetch`); iOS's
/// `bake_some` ignores it.
///
/// Does NOT touch `cover` or `coarse` themselves -- both are read through
/// `&[TileId]`, and the draw list a shell builds from `cover()` afterward
/// is whatever the caller already had.
#[must_use]
pub fn bake_order(
    cover: &[TileId],
    coarse: &[TileId],
    baked_zoom: impl Fn(TileId) -> Option<u8>,
    is_decoded: impl Fn(TileId) -> bool,
    display_zoom: u8,
    camera: &Camera,
) -> Vec<(TileId, bool)> {
    let missing_mesh: Vec<TileId> = cover.iter().copied().filter(|&t| baked_zoom(t).is_none()).collect();
    let wrong_bucket: Vec<TileId> =
        cover.iter().copied().filter(|&t| baked_zoom(t).is_some_and(|z| z != display_zoom)).collect();
    // Under the distance LOD the far ground of the display cover and of the
    // coarse cover can be the SAME coarse tile; it is baked once, in its
    // cover class, not a second time as an optional prefetch leftover.
    let in_cover: std::collections::HashSet<TileId> = cover.iter().copied().collect();
    let optional: Vec<TileId> = coarse
        .iter()
        .copied()
        .filter(|t| !in_cover.contains(t))
        .filter(|&t| baked_zoom(t).map(|z| z != display_zoom).unwrap_or(true))
        .collect();

    nearest_first(&missing_mesh, camera)
        .into_iter()
        .map(|t| (t, false))
        .chain(nearest_first(&wrong_bucket, camera).into_iter().map(|t| (t, false)))
        .chain(nearest_first(&optional, camera).into_iter().map(|t| (t, true)))
        .filter(|(t, _)| is_decoded(*t))
        .collect()
}

/// A shell frame's basemap draw list and its building list, selected in ONE
/// pass over the frame's draw order, so the buildings can never name a tile
/// the flat list does not.
///
/// `draws` is the shell's `FramePlan::draws` (stand-ins first, then the
/// cover tiles that have their own mesh) and `lookup` resolves a tile to
/// its resident GPU tile. Every resolved tile goes into the flat list; only
/// the ones in `cover` go into the building list.
///
/// **Stand-ins carry no buildings (Task 12 review, Ruling F1).** `MapView`
/// draws a stand-in's buildings through a model group clipped to the
/// missing cover tile (R21). The web and iOS shells have no clipped model
/// groups: a stand-in there paints its whole extent and relies on painter's
/// order to sit under the loaded tiles, which works for flat fills and not
/// for depth-tested walls -- an ancestor's unclipped buildings would
/// double or z-fight the loaded children's. So the shells do not clip;
/// they leave every stand-in (ancestor or descendant) out of the building
/// list, and a tile's buildings appear when the tile itself lands.
pub fn split_basemap_and_building_tiles<'a, G>(
    draws: &[TileId],
    cover: &[TileId],
    mut lookup: impl FnMut(TileId) -> Option<&'a G>,
) -> (Vec<&'a G>, Vec<&'a G>) {
    let cover: std::collections::HashSet<TileId> = cover.iter().copied().collect();
    let mut flat = Vec::with_capacity(draws.len());
    let mut buildings = Vec::with_capacity(draws.len());
    for &tile in draws {
        let Some(gpu) = lookup(tile) else { continue };
        flat.push(gpu);
        if cover.contains(&tile) {
            buildings.push(gpu);
        }
    }
    (flat, buildings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manhattan_60() -> Camera {
        Camera {
            center_lon: -73.9857,
            center_lat: 40.7484,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 30f32.to_radians(),
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// The eye camera at Marina Bay: 1.1 m over the road, pitched
    /// `pitch_deg`, facing `bearing_deg`, in a `w` x `h` view.
    fn eye_camera(pitch_deg: f32, bearing_deg: f32, w: f32, h: f32) -> Camera {
        let t = crate::camera_control::eye_target((103.8590, 1.2914), 1.1, bearing_deg.to_radians(), pitch_deg.to_radians(), h);
        let (lon, lat) = t.center.unwrap();
        Camera { center_lon: lon, center_lat: lat, zoom: t.zoom.unwrap(), pitch: t.pitch.unwrap(), bearing: t.bearing.unwrap(), viewport_width_px: w, viewport_height_px: h, fov_y: Camera::DEFAULT_FOV_Y }
    }

    /// The eye camera's tile budget: the most tiles its cover holds at
    /// pitch 85 with a 600 m cutoff, over the source zooms and viewports
    /// below: 14 measured at source zooms 14-18 and 32 up to z22
    /// (2026-10-10), held at 40 -- under the 105-tile tilted budget.
    const EYE_COVER_BUDGET: usize = 40;

    /// **The eye camera's cover is bounded by its cutoff** (the MapLibre way
    /// for a high-pitch cover: a far limit and the distance LOD, not a
    /// count). At pitch 85, 1.1 m up, every viewport and source zoom (14
    /// to 22) stays
    /// within [`EYE_COVER_BUDGET`] tiles; nothing whose nearest ground is
    /// past the cutoff is requested; the cut is reported for the fog; and
    /// without the cutoff the LOD's horizon would end the road ~80 m out.
    #[test]
    fn the_eye_cover_stops_at_its_cutoff_within_a_budget() {
        let fog_m = 600.0;
        let mut most = 0;
        for (w, h) in [(1024.0, 768.0), (1440.0, 900.0), (2560.0, 1440.0), (430.0, 932.0)] {
            for bearing in [0.0, 37.0, 90.0, 215.0] {
                let cam = eye_camera(85.0, bearing, w, h);
                assert!((cam.distance() * cam.pitch.cos() as f64 * crate::camera_control::metres_per_world_unit(cam.center_lat) - 1.1).abs() < 1e-6, "the eye is 1.1 m up");
                let cutoff = fog_m / crate::camera_control::metres_per_world_unit(cam.center_lat);
                let eye = crate::render::camera::eye_ground_point(&cam);
                for zoom in [14u8, 16, 18, 20, 22] {
                    let cover = flat_cover(&cam, CoverZoom::from(zoom).with_cutoff(Some(cutoff)));
                    most = most.max(cover.tiles.len());
                    assert!(cover.tiles.len() <= EYE_COVER_BUDGET, "{w}x{h} b{bearing} z{zoom}: {} tiles", cover.tiles.len());
                    for t in &cover.tiles {
                        let d = ground_distance_to_tile(*t, eye);
                        assert!(d <= cutoff, "{t:?} starts {:.0} m out, past the {fog_m} m cutoff", d * crate::camera_control::metres_per_world_unit(cam.center_lat));
                    }
                    assert_eq!(cover.reach, Some(cutoff), "the cut is the cutoff, for the fog");
                }
            }
        }
        eprintln!("eye cover: at most {most} tiles at pitch 85, 1.1 m, {fog_m} m");
        // Without it the LOD's own horizon would end the road ~80 m out.
        let cam = eye_camera(85.0, 0.0, 1024.0, 768.0);
        let lod_m = flat_cover(&cam, 18).reach.expect("the LOD horizon cuts") * crate::camera_control::metres_per_world_unit(cam.center_lat);
        assert!(lod_m < 100.0, "{lod_m}");
    }

    /// A WIDER eye camera (`Camera::fov_y`) is still bounded by its
    /// cutoff: the cockpit's 78 degrees at 1.0 m and pitch 85, and the
    /// close chase's 65 degrees at 1.8 m and pitch 80, over the same
    /// viewports, bearings and source zooms, stay within
    /// [`EYE_COVER_BUDGET`] and never fetch past the fog.
    #[test]
    fn a_wide_fov_eye_cover_stays_within_the_budget() {
        let fog_m = 600.0;
        let mut most = 0;
        for (fov_deg, height_m, pitch_deg) in [(78.0f64, 1.0, 85.0f32), (65.0, 1.8, 80.0)] {
            for (w, h) in [(1024.0, 768.0), (1440.0, 900.0), (2560.0, 1440.0), (430.0, 932.0)] {
                for bearing in [0.0f32, 37.0, 90.0, 215.0] {
                    let fov = fov_deg.to_radians();
                    let t = crate::camera_control::eye_target_fov((103.8590, 1.2914), height_m, bearing.to_radians(), pitch_deg.to_radians(), h, fov);
                    let (lon, lat) = t.center.unwrap();
                    let cam = Camera { center_lon: lon, center_lat: lat, zoom: t.zoom.unwrap(), pitch: t.pitch.unwrap(), bearing: t.bearing.unwrap(), viewport_width_px: w, viewport_height_px: h, fov_y: fov };
                    assert!((cam.eye_height_m() - height_m).abs() < 1e-6, "the eye is {height_m} m up at {fov_deg} deg: {}", cam.eye_height_m());
                    let cutoff = fog_m / crate::camera_control::metres_per_world_unit(cam.center_lat);
                    let eye = crate::render::camera::eye_ground_point(&cam);
                    for zoom in [14u8, 16, 18, 20, 22] {
                        let cover = flat_cover(&cam, CoverZoom::from(zoom).with_cutoff(Some(cutoff)));
                        most = most.max(cover.tiles.len());
                        assert!(cover.tiles.len() <= EYE_COVER_BUDGET, "{fov_deg} deg {w}x{h} b{bearing} z{zoom}: {} tiles", cover.tiles.len());
                        for t in &cover.tiles {
                            assert!(ground_distance_to_tile(*t, eye) <= cutoff, "{t:?} past the cutoff");
                        }
                    }
                }
            }
        }
        eprintln!("wide eye cover: at most {most} tiles");
    }

    /// Every map that has not asked for the eye camera gets the cover it
    /// always had: `with_cutoff(None)` is the plain request.
    #[test]
    fn no_cutoff_is_the_cover_as_before() {
        for cam in [manhattan_60(), Camera { pitch: 0.0, ..manhattan_60() }, Camera { pitch: 0.7, ..manhattan_60() }] {
            assert_eq!(flat_cover(&cam, CoverZoom::from(16).with_cutoff(None)), flat_cover(&cam, 16));
            assert_eq!(CoverZoom::from(16).with_cutoff(Some(f64::NAN)), CoverZoom::from(16));
        }
    }

    /// The eye fog: clear near the eye, thick before the cut (so no
    /// missing tile shows), in the colour asked for.
    #[test]
    fn the_eye_fog_ends_inside_the_cutoff() {
        let cam = eye_camera(85.0, 20.0, 1024.0, 768.0);
        let cutoff = 600.0 / crate::camera_control::metres_per_world_unit(cam.center_lat);
        let colour = crate::style::Rgba { r: 0.1, g: 0.2, b: 0.3, a: 1.0 };
        let fog = eye_fog(&cam, cutoff, colour);
        let nearest = nearest_depth_beyond(&cam, cutoff).expect("the cut is on screen");
        assert!(f64::from(fog.end) < nearest && fog.start < fog.end && fog.start > 0.0, "{fog:?} vs {nearest}");
        assert!(f64::from(fog.start) < 0.3 * nearest);
        assert_eq!(fog.color, [0.1, 0.2, 0.3, 1.0]);
        // Looking straight down from a few metres the cut is off screen.
        let down = Camera { pitch: 0.0, ..cam };
        assert_eq!(eye_fog(&down, cutoff, colour), crate::render::gpu::HorizonFog::NONE);
    }

    /// A camera `h_m` over Midtown at `pitch_deg`, looking north-north-east
    /// through the Digital twin's 46-degree lens in a 1401 x 887 view.
    fn aerial(h_m: f64, pitch_deg: f32) -> Camera {
        let fov = 46f64.to_radians();
        let t = crate::camera_control::eye_target_fov((-73.99, 40.747), h_m, 20f32.to_radians(), pitch_deg.to_radians(), 887.0, fov);
        let (lon, lat) = t.center.unwrap();
        Camera { center_lon: lon, center_lat: lat, zoom: t.zoom.unwrap(), pitch: t.pitch.unwrap(), bearing: t.bearing.unwrap(), viewport_width_px: 1401.0, viewport_height_px: 887.0, fov_y: fov }
    }

    /// No clear distance is the eye fog exactly, cut on screen or not.
    #[test]
    fn eye_fog_clear_without_a_clear_distance_is_the_eye_fog() {
        let colour = crate::style::Rgba { r: 0.1, g: 0.2, b: 0.3, a: 1.0 };
        for cam in [eye_camera(85.0, 20.0, 1024.0, 768.0), aerial(1500.0, 60.0), aerial(4000.0, 72.0)] {
            for metres in [600.0, 8_000.0, 30_000.0] {
                let cutoff = metres / crate::camera_control::metres_per_world_unit(cam.center_lat);
                for clear in [0.0, -1.0, f64::NAN] {
                    assert_eq!(eye_fog_clear(&cam, cutoff, clear, colour), eye_fog(&cam, cutoff, colour));
                }
            }
        }
    }

    /// **The aerial fog** (the user's "very very foggy" twin from 1.5 km):
    /// with the old 8 km cut the fog starts at 1.6 km of depth, under the
    /// bottom of the frame; with the reach and clear distance a 1.5 km
    /// eye earns (26 km, 9.75 km) it starts at the clear distance; the
    /// cut being past every visible ground still leaves a fade, ending at
    /// the cutoff; and an over-long clear distance still leaves a band.
    #[test]
    fn an_aerial_eye_fog_stays_clear_to_its_clear_distance() {
        let colour = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let cam = aerial(1500.0, 60.0);
        let mpu = crate::camera_control::metres_per_world_unit(cam.center_lat);
        let old = eye_fog(&cam, 8_000.0 / mpu, colour);
        assert!(f64::from(old.start) * mpu < 2_000.0, "the old fog starts {:.0} m deep", f64::from(old.start) * mpu);
        let (cutoff, clear) = (26_000.0 / mpu, 9_750.0 / mpu);
        assert!(nearest_depth_beyond(&cam, cutoff).is_none(), "the 26 km cut is past the frame");
        let fog = eye_fog_clear(&cam, cutoff, clear, colour);
        assert!((f64::from(fog.start) - clear).abs() < 1e-3 * clear, "{fog:?}");
        assert!((f64::from(fog.end) - cutoff).abs() < 1e-3 * cutoff, "{fog:?}");
        let tight = eye_fog_clear(&cam, cutoff, 10.0 * cutoff, colour);
        assert!(f64::from(tight.start) <= EYE_FOG_MAX_CLEAR_FRACTION * f64::from(tight.end) * 1.0001 && tight.start < tight.end, "{tight:?}");
        // A cut on screen (a steep high eye) still ends the fog inside it.
        let steep = aerial(4000.0, 72.0);
        let cut = 56_000.0 / crate::camera_control::metres_per_world_unit(steep.center_lat);
        let nearest = nearest_depth_beyond(&steep, cut).expect("the horizon is in view");
        let fog = eye_fog_clear(&steep, cut, 26_000.0 / mpu, colour);
        assert!(f64::from(fog.end) < nearest && fog.start < fog.end, "{fog:?} vs {nearest}");
    }

    /// The far reach a high eye gets stays a modest cover: the distance LOD
    /// coarsens it, so the twin's reach (12 m per metre over 100 m, at
    /// most 200 km) holds every aerial view under the tilted-cover budget.
    #[test]
    fn an_aerial_reach_is_a_bounded_cover() {
        let mut most = 0;
        for (h_m, pitch) in [(1500.0, 60.0f32), (3000.0, 60.0), (4000.0, 72.0), (6000.0, 75.0), (10_000.0, 58.0), (12_000.0, 72.0), (12_000.0, 80.0)] {
            let cam = aerial(h_m, pitch);
            let mpu = crate::camera_control::metres_per_world_unit(cam.center_lat);
            let cutoff = (8_000.0 + 12.0 * (h_m - 100.0)).min(200_000.0) / mpu;
            for z in [cam.zoom.floor() as u8, cam.zoom.ceil() as u8 + 1] {
                let cover = flat_cover(&cam, CoverZoom { building_floor: Some(BUILDING_SOURCE_MIN_ZOOM), ..CoverZoom::from(z).with_cutoff(Some(cutoff)) });
                most = most.max(cover.tiles.len());
                assert!(cover.tiles.len() <= BUILDING_COVER_BUDGET, "{h_m} m p{pitch} z{z}: {} tiles", cover.tiles.len());
            }
        }
        eprintln!("aerial cover: at most {most} tiles");
    }

    fn floored(zoom: u8) -> CoverZoom {
        CoverZoom { building_floor: Some(BUILDING_SOURCE_MIN_ZOOM), ..zoom.into() }
    }

    /// (h) part 1: with buildings on, the cover keeps the near ground on
    /// tiles that carry building data farther out than the LOD alone does,
    /// and pays at most `BUILDING_EXTRA_TILES` tiles for it.
    #[test]
    fn the_building_floor_pushes_building_tiles_out_within_its_budget() {
        let cam = manhattan_60();
        let eye = crate::render::camera::eye_ground_point(&cam);
        let base = flat_cover(&cam, 14);
        let floor = flat_cover(&cam, floored(14));
        assert_eq!(base.building_reach, None, "no floor asked, none reported");
        let base_reach = base
            .tiles
            .iter()
            .filter(|t| t.z < BUILDING_SOURCE_MIN_ZOOM)
            .map(|&t| ground_distance_to_tile(t, eye))
            .min_by(f64::total_cmp)
            .expect("precondition: the LOD alone drops below z13 at 60 degrees");
        let reach = floor.building_reach.expect("the floored band still ends inside the horizon");
        assert!(reach > base_reach * 1.2, "building tiles reach {reach} against {base_reach} without the floor");
        assert!(floor.tiles.len() <= base.tiles.len() + BUILDING_EXTRA_TILES, "{} tiles against {}", floor.tiles.len(), base.tiles.len());
        assert_eq!(floor.reach, base.reach, "the horizon cut does not move");
        for (i, a) in floor.tiles.iter().enumerate() {
            for b in &floor.tiles[i + 1..] {
                let (hi, lo) = if a.z >= b.z { (a, b) } else { (b, a) };
                let shift = hi.z - lo.z;
                assert!(!(hi.x >> shift == lo.x && hi.y >> shift == lo.y), "{lo:?} overlaps {hi:?}");
            }
        }
    }

    /// A source whose building layer starts at z14 (TileJSON
    /// `vector_layers[].minzoom`, `TileSource::layer_min_zoom`) floors the
    /// near ground at z14, not the default 13.
    #[test]
    fn a_z14_building_source_floors_the_cover_at_z14() {
        assert_eq!(building_floor_for(Some(14)), 14);
        assert_eq!(building_floor_for(None), BUILDING_SOURCE_MIN_ZOOM);
        let cam = manhattan_60();
        let eye = crate::render::camera::eye_ground_point(&cam);
        let at14 = flat_cover(&cam, CoverZoom { building_floor: Some(building_floor_for(Some(14))), ..16.into() });
        let reach = at14.building_reach.expect("the z14 band ends inside the horizon");
        for t in &at14.tiles {
            if ground_distance_to_tile(*t, eye) < reach {
                assert!(t.z >= 14, "{t:?} nearer than the building cut is coarser than the source's z14");
            }
        }
        assert!(at14.tiles.iter().any(|t| t.z < 14), "past the cut the LOD drops as before");
        assert!(at14.tiles.len() <= flat_cover(&cam, 16).tiles.len() + BUILDING_EXTRA_TILES);
    }

    /// A camera whose cover never drops below the floor is left alone.
    #[test]
    fn the_building_floor_changes_nothing_where_the_lod_stays_above_it() {
        let cam = Camera { pitch: 0.0, ..manhattan_60() };
        assert_eq!(flat_cover(&cam, floored(14)).tiles, flat_cover(&cam, 14).tiles);
        assert_eq!(flat_cover(&cam, floored(14)).building_reach, None);
    }

    /// (h) part 2: every building on ground at or past the building cut is
    /// at or past the fade's end, where the shader's height and colour fade
    /// reach zero, so buildings never stop on a hard line; and the band
    /// starts past the look-at point, so near buildings are untouched.
    #[test]
    fn buildings_fade_to_nothing_by_the_building_cut() {
        let cam = manhattan_60();
        let reach = flat_cover(&cam, floored(14)).building_reach.expect("a cut");
        let [start, end] = building_fade_band(&cam, Some(reach)).expect("the cut is on screen");
        assert!(start < end && f64::from(start) > cam.distance(), "band {start}..{end}, look-at depth {}", cam.distance());
        let view_projection = cam.view_projection_f64();
        let eye = crate::render::camera::eye_ground_point(&cam);
        let fade = |w: f32| 1.0 - { let t = ((w - start) / (end - start)).clamp(0.0, 1.0); t * t * (3.0 - 2.0 * t) };
        let mut beyond = 0;
        for sy in (0..=1024).step_by(8) {
            for sx in (0..=1024).step_by(8) {
                let Some(g) = crate::render::camera::unproject_to_ground_f64(&cam, sx as f32, sy as f32) else { continue };
                if (g.truncate() - eye).length() >= reach {
                    let w = (view_projection * glam::DVec4::new(g.x, g.y, 0.0, 1.0)).w as f32;
                    assert_eq!(fade(w), 0.0, "ground past the cut at ({sx}, {sy}) still draws buildings");
                    beyond += 1;
                }
            }
        }
        assert!(beyond > 0, "the cut is on screen");
        assert_eq!(building_fade_band(&cam, None), None);
    }

    /// Ruling F1 (Task 12 review): a stand-in draws flat but carries no
    /// buildings; a cover tile draws both; a tile with no resident GPU tile
    /// is in neither list; the flat order is the draw order.
    #[test]
    fn stand_ins_draw_flat_and_carry_no_buildings() {
        let t = |z, x, y| TileId { z, x, y };
        let ancestor = t(13, 7276, 3225);
        let descendant = t(15, 29106, 12902);
        let loaded = t(14, 14552, 6450);
        let missing = t(14, 14553, 6450);
        let draws = [ancestor, descendant, loaded, missing];
        let cover = [loaded, missing, t(14, 14553, 6451)];
        let resident = |tile: TileId| -> Option<&'static u8> {
            match tile {
                x if x == ancestor => Some(&1),
                x if x == descendant => Some(&2),
                x if x == loaded => Some(&3),
                _ => None,
            }
        };
        let (flat, buildings) = split_basemap_and_building_tiles(&draws, &cover, resident);
        assert_eq!(flat, vec![&1, &2, &3], "every resident tile draws flat, in draw order");
        assert_eq!(buildings, vec![&3], "only the loaded cover tile carries buildings");
    }

    /// The fetch order is the draw cover re-sorted by ground distance from
    /// the eye -- the same tiles, the nearest first -- and the draw cover
    /// itself keeps its `(x, y)` order.
    #[test]
    fn a_cover_is_fetched_nearest_the_eye_first_without_reordering_the_draw() {
        let cam = singapore(16.0, 60.0, 0.3, 900.0, 900.0);
        let cover = flat_cover(&cam, 16).tiles;
        let fetch = nearest_first(&cover, &cam);
        let eye = crate::render::camera::eye_ground_point(&cam);
        let d: Vec<f64> = fetch.iter().map(|t| ground_distance_to_tile(*t, eye)).collect();
        assert!(d.windows(2).all(|w| w[0] <= w[1]), "not nearest first");
        let (mut a, mut b) = (cover.clone(), fetch.clone());
        a.sort_unstable_by_key(|t| (std::cmp::Reverse(t.z), t.x, t.y));
        b.sort_unstable_by_key(|t| (std::cmp::Reverse(t.z), t.x, t.y));
        assert_eq!(a, b, "the same tiles");
        assert_eq!(cover, a, "the draw cover is still in its (finest first, then x, y) order");
        assert_ne!(fetch, cover, "at 60 degrees the two orders differ");
    }

    /// At pitch 60 / bearing 0, with no tile baked yet, `bake_order`'s first
    /// candidates must be the tiles nearest the eye's ground point -- NOT
    /// `cover()`'s first (row-major) entries, which at this pitch are the
    /// far row. This is the exact defect: the ground under the camera baked
    /// last because `cover()`'s draw order fed `bake_some`'s `todo` list
    /// unsorted.
    #[test]
    fn missing_mesh_bakes_nearest_the_eye_first_not_cover_order() {
        let cam = singapore(16.0, 60.0, 0.0, 900.0, 900.0);
        let cover = flat_cover(&cam, 16).tiles;
        assert!(cover.len() > 8, "need enough tiles for the first row to differ from the nearest ones");
        let order = bake_order(&cover, &[], |_| None, |_| true, 16, &cam);
        let eye = crate::render::camera::eye_ground_point(&cam);

        let first_n = 4;
        let nearest_by_distance = {
            let mut d: Vec<f64> = cover.iter().map(|t| ground_distance_to_tile(*t, eye)).collect();
            d.sort_by(f64::total_cmp);
            d[first_n - 1]
        };
        for (tile, _) in order.iter().take(first_n) {
            assert!(
                ground_distance_to_tile(*tile, eye) <= nearest_by_distance,
                "bake_order's early candidates must be among the nearest tiles, got {tile:?} at distance {}",
                ground_distance_to_tile(*tile, eye)
            );
        }
        // And this is NOT simply `cover`'s own order: the far row that
        // `cover()` lists first must not be `bake_order`'s first entry.
        assert_ne!(order.first().map(|(t, _)| *t), cover.first().copied(), "bake_order must not just echo cover()'s draw order");
    }

    /// Class priority survives the reordering: every `missing_mesh` tile
    /// (no mesh at all) comes before every `wrong_bucket` tile (baked, but
    /// at a stale zoom), which comes before every `optional` prefetch tile
    /// -- even when a far `missing_mesh` tile is farther from the eye than a
    /// near `optional` one. Nearest-first is scoped to WITHIN a class, not
    /// across classes.
    #[test]
    fn class_priority_survives_the_nearest_first_reorder() {
        let cam = singapore(16.0, 60.0, 0.0, 900.0, 900.0);
        let cover = flat_cover(&cam, 16).tiles;
        assert!(cover.len() >= 4);
        let eye = crate::render::camera::eye_ground_point(&cam);
        let mut by_distance = cover.clone();
        by_distance.sort_by(|a, b| ground_distance_to_tile(*a, eye).total_cmp(&ground_distance_to_tile(*b, eye)));

        // The NEAREST cover tile is "wrong_bucket" (baked at zoom 15, not
        // 16); every other cover tile has no mesh at all ("missing_mesh").
        let near_wrong_bucket = by_distance[0];
        let missing_mesh_tiles: Vec<TileId> = cover.iter().copied().filter(|t| *t != near_wrong_bucket).collect();
        // A coarse prefetch tile the eye is nearer to than any missing_mesh
        // tile would be, if class alone did not win.
        let coarse = crate::render::cover_plan::flat_cover(&cam, 14).tiles;

        let order = bake_order(
            &cover,
            &coarse,
            |t| if t == near_wrong_bucket { Some(15) } else { None },
            |_| true,
            16,
            &cam,
        );
        let classes: Vec<&'static str> = order
            .iter()
            .map(|(t, is_optional)| {
                if *is_optional {
                    "optional"
                } else if missing_mesh_tiles.contains(t) {
                    "missing_mesh"
                } else {
                    "wrong_bucket"
                }
            })
            .collect();
        let last_missing = classes.iter().rposition(|c| *c == "missing_mesh");
        let first_wrong = classes.iter().position(|c| *c == "wrong_bucket");
        let last_wrong = classes.iter().rposition(|c| *c == "wrong_bucket");
        let first_optional = classes.iter().position(|c| *c == "optional");
        if let (Some(last_missing), Some(first_wrong)) = (last_missing, first_wrong) {
            assert!(last_missing < first_wrong, "a missing_mesh tile must never come after a wrong_bucket tile");
        }
        if let (Some(last_wrong), Some(first_optional)) = (last_wrong, first_optional) {
            assert!(last_wrong < first_optional, "a wrong_bucket tile must never come after an optional tile");
        }
    }

    /// `bake_order` must never reorder `cover()`'s own draw list -- it reads
    /// `cover` through a shared reference and returns a SEPARATE `Vec`. This
    /// pins down constraint 3 in the fix: the draw list (and so pitch-0
    /// label/paint order) is untouched by the bake-order fix.
    #[test]
    fn bake_order_does_not_touch_the_draw_cover() {
        let cam = singapore(16.0, 60.0, 0.0, 900.0, 900.0);
        let cover = flat_cover(&cam, 16).tiles;
        let before = cover.clone();
        let _ = bake_order(&cover, &[], |_| None, |_| true, 16, &cam);
        assert_eq!(cover, before, "cover must be unchanged after bake_order reads it");
    }

    /// A style with no `background` layer still fades on web and iOS --
    /// toward the colour those frames clear to -- and one with a background
    /// fades toward the style's own colour, as before.
    #[test]
    fn a_style_without_a_background_fades_toward_the_shells_clear_colour() {
        let white = wgpu::Color::WHITE;
        let cam = singapore(16.0, 60.0, 0.5, 900.0, 900.0);
        let reach = flat_cover(&cam, 16).reach;
        assert_eq!(frame_fog(&cam, reach, None, None, 0.0), crate::render::gpu::HorizonFog::NONE, "the old input: no fade");
        let fog = frame_fog(&cam, reach, None, Some(fog_background(None, white)), 0.0);
        assert!(fog.end > 0.0, "{fog:?}");
        assert_eq!(fog.color, [1.0, 1.0, 1.0, 1.0]);

        let style = crate::style::Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 };
        assert_eq!(fog_background(Some(style), white), style, "a style background wins over the fallback");
    }

    #[test]
    fn the_nearer_cut_is_the_smaller_reach_and_none_is_no_cut() {
        assert_eq!(nearer_cut(None, None), None);
        assert_eq!(nearer_cut(Some(3.0), None), Some(3.0));
        assert_eq!(nearer_cut(None, Some(2.0)), Some(2.0));
        assert_eq!(nearer_cut(Some(3.0), Some(2.0)), Some(2.0));
        assert_eq!(nearer_cut(Some(1.0), Some(2.0)), Some(1.0));
    }

    /// When the imagery cover is cut nearer the eye than the vectors, the
    /// frame's fade ends inside THAT cut; with no imagery drawing it is the
    /// vector fade, unchanged; and a globe frame fades nothing whatever it
    /// is handed.
    ///
    /// Before the distance LOD a two-zooms-finer imagery cover really was
    /// cut nearer (the cap held a quarter of the ground). Under the LOD
    /// both covers end at the same horizon (it depends on the eye height,
    /// not on the zoom), so the nearer imagery cut here is a fabricated
    /// half-distance one: `frame_fog` must still honour it if the cap ever
    /// makes one.
    #[test]
    fn the_frame_fade_ends_inside_the_nearer_of_the_vector_and_raster_cuts() {
        let bg = Some(crate::style::Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 });
        let cam = singapore(16.0, 60.0, 0.5, 900.0, 900.0);
        let vector = flat_cover(&cam, 16).reach.expect("the vector cover stops at the LOD horizon at 60 degrees");
        assert_eq!(flat_cover(&cam, 18).reach, Some(vector), "a finer cover stops at the same horizon");
        let raster = vector * 0.5;

        let vector_only = frame_fog(&cam, Some(vector), None, bg, 0.0);
        assert_eq!(vector_only, horizon_fog(&cam, Some(vector), bg));
        let with_imagery = frame_fog(&cam, Some(vector), Some(raster), bg, 0.0);
        assert_eq!(with_imagery, horizon_fog(&cam, Some(raster), bg));
        assert!(with_imagery.end < vector_only.end, "{with_imagery:?} vs {vector_only:?}");

        for transition in [1e-6, 0.5, 1.0] {
            assert_eq!(frame_fog(&cam, Some(vector), Some(raster), bg, transition), crate::render::gpu::HorizonFog::NONE);
        }
    }
    use crate::render::camera::Camera;
    use crate::render::globe::globe_frame;

    fn camera(zoom: f64) -> Camera {
        Camera {
            center_lon: 13.4,
            center_lat: 52.5,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1200.0,
            viewport_height_px: 900.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// Above the globe's band (`GlobeCover::FLAT`), `tile_cover` must run
    /// the caller's flat cover -- the globe reports no cover of its own to
    /// use instead.
    #[test]
    fn flat_globe_falls_back_to_the_caller_flat_cover() {
        let flat_marker = vec![TileId { z: 9, x: 1, y: 1 }];
        let result = tile_cover(9, &GlobeCover::FLAT, || flat_marker.clone());
        assert_eq!(result, flat_marker, "GlobeCover::FLAT must take the flat_cover arm");
    }

    /// Inside the globe's band, `tile_cover` must return the GLOBE's answer,
    /// not the caller's flat one -- the exact defect this module exists to
    /// fix: a flat function's answer standing in for a spherical frame.
    #[test]
    fn globe_cover_wins_over_the_caller_flat_cover_when_the_globe_is_up() {
        let cam = camera(3.0); // well inside the globe band (transition == 1.0 below z11)
        let frame = globe_frame(&cam);
        let globe = GlobeCover::new(&frame);
        assert!(globe.is_globe(), "z3 must be inside the globe band for this test to grade anything");

        let poisoned = vec![TileId { z: 255, x: 0, y: 0 }]; // a value the globe arm can never produce
        let result = tile_cover(3, &globe, || poisoned.clone());
        assert_ne!(result, poisoned, "the globe branch must win; the flat closure must not run");
        assert_eq!(result, globe.cover(3).unwrap(), "must match GlobeCover::cover exactly");
    }

    /// `tile_cover` must not invoke `flat_cover` at all when the globe has
    /// an answer -- proven by a closure that panics if called, not merely by
    /// checking the returned value differs from a poisoned one.
    #[test]
    fn the_flat_closure_never_runs_when_the_globe_has_an_answer() {
        let cam = camera(3.0);
        let frame = globe_frame(&cam);
        let globe = GlobeCover::new(&frame);
        assert!(globe.is_globe());

        let _ = tile_cover(3, &globe, || panic!("flat_cover must not run when the globe covers this frame"));
    }

    /// Non-visual proof that the globe's cover does not gap at the
    /// antimeridian seam.
    ///
    /// A camera looking straight at longitude 180 sees BOTH tile columns
    /// that border the seam: `x == 0` (the tile starting at -180) and
    /// `x == n - 1` (the tile ending at +180) are adjacent on the sphere,
    /// even though they sit at opposite ends of the mercator x range. A
    /// flat function asked for this camera's cover would have no reason to
    /// return both -- `geo::visible_tiles`' own clamp
    /// (`visible_tiles_are_clamped_to_valid_range`, `src/geo.rs:133`) keeps
    /// every x inside `[0, n-1]`, but nothing makes a flat rectangular
    /// query straddle the two ends of that range at once, which is exactly
    /// what a viewer centred on the dateline needs. `GlobeCover::cover`
    /// works in continuous spherical angle (`plane_spherical_x`, via
    /// `atan2`), not in tile-index space, so the wrap is not a special case
    /// for it -- this test is what pins that down as a requirement rather
    /// than an accident.
    #[test]
    fn the_globe_cover_spans_both_sides_of_the_antimeridian_seam() {
        for lon in [180.0, -180.0] {
            let cam = Camera { center_lon: lon, center_lat: 0.0, ..camera(3.0) };
            let frame = globe_frame(&cam);
            let globe = GlobeCover::new(&frame);
            assert!(globe.is_globe(), "z3 must be inside the globe band for this test to grade anything");

            let zoom = 3u8;
            let n = 1u32 << zoom;
            let cover =
                tile_cover(zoom, &globe, || panic!("the globe must answer; no flat fallback at z3"));
            assert!(!cover.is_empty(), "lon {lon}: the cover must not be empty at a dateline-facing camera");

            let has_west_edge = cover.iter().any(|t| t.z == zoom && t.x == 0);
            let has_east_edge = cover.iter().any(|t| t.z == zoom && t.x == n - 1);
            assert!(
                has_west_edge && has_east_edge,
                "lon {lon}: cover must include both x=0 and x={} (the two tile columns \
                 adjacent across the antimeridian seam) -- got {cover:?}",
                n - 1,
            );
        }
    }

    fn singapore(zoom: f64, pitch_deg: f32, bearing: f32, w: f32, h: f32) -> Camera {
        Camera {
            center_lon: 103.8198,
            center_lat: 1.3521,
            zoom,
            pitch: pitch_deg.to_radians(),
            bearing,
            viewport_width_px: w,
            viewport_height_px: h,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// Tile at `zoom` containing world-space ground point `(x, y)`, clamped
    /// to `[0, 2^zoom - 1]` per axis -- the same clamp `geo::visible_tiles`
    /// applies for a point that projects off the edge of the flat Mercator
    /// plane.
    fn tile_at_world_point(zoom: u8, x: f64, y: f64) -> TileId {
        let size = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << zoom);
        let n = 1i64 << zoom;
        let idx = |v: f64| ((v / size).floor() as i64).clamp(0, n - 1) as u32;
        TileId { z: zoom, x: idx(x), y: idx(y) }
    }

    #[test]
    fn the_frustum_cover_has_no_holes() {
        // Was `the_frustum_cover_keeps_every_tile_native_draws_today`,
        // comparing against `crate::render::tile_in_frustum`'s answer.
        // Fix round 1, IMPORTANT 2 found that reference has a genuine false
        // positive at high pitch/bearing: tile (16, 51666, 32522) at
        // 430x932, pitch 48, bearing 0.6 has three corners behind the near
        // plane and its one front corner at NDC x = 1.93 -- comfortably
        // outside the view -- yet `tile_in_frustum`'s corner-only test (no
        // near-plane clip) calls it visible. Comparing against that answer
        // would fail on the reference's bug, not a real hole in the new
        // cover.
        //
        // Testing actual ground visibility instead: unproject a grid of
        // screen points (this includes every corner and every edge midpoint,
        // since STEPS is even) and assert the tile under every point that
        // actually lands on the ground is in the cover. A point that doesn't
        // land on the ground (`unproject_to_ground_f64` returns `None`) is
        // skipped -- there is no tile to check.
        const STEPS: i32 = 8;
        for (w, h) in [(900.0f32, 900.0), (430.0, 932.0)] {
            for pitch in [0.0, 30.0, 48.0] {
                for bearing in [0.0f32, 0.6] {
                    let cam = singapore(16.0, pitch, bearing, w, h);
                    let cover: std::collections::HashSet<TileId> =
                        frustum_cover(&cam, 16, usize::MAX).into_iter().collect();
                    for iy in 0..=STEPS {
                        for ix in 0..=STEPS {
                            let sx = w * ix as f32 / STEPS as f32;
                            let sy = h * iy as f32 / STEPS as f32;
                            if let Some(ground) = crate::render::unproject_to_ground_f64(&cam, sx, sy) {
                                let tile = tile_at_world_point(16, ground.x, ground.y);
                                assert!(
                                    cover.contains(&tile),
                                    "{w}x{h} pitch {pitch} bearing {bearing}: screen ({sx},{sy}) unprojects to {tile:?}, not in the cover"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_near_plane_clip_does_not_accept_a_whole_tile_row() {
        // Fix round 1, CRITICAL 1: before the near-plane clip, a tile
        // straddling the near plane was accepted on its unclipped corners
        // alone. At 45 degrees pitch, bearing 0, the near plane cuts the
        // ground along a line under the eye's ground point, and EVERY tile
        // in that one z16 row straddled it -- the reviewer's `cover.py`
        // reproduced a whole 16384/65536-wide row accepted this way. Two
        // invariants pin the fix down: the trimmed cover can never exceed
        // what the (coarser, untrimmed) bbox itself would enumerate, and no
        // single row holds anywhere near a whole world's width of tiles.
        for pitch in [45.0, 50.0] {
            for (w, h, zoom) in [(900.0f32, 900.0, 16u8), (430.0, 932.0, 16), (1290.0, 2796.0, 16)] {
                let cam = singapore(f64::from(zoom), pitch, 0.0, w, h);
                let cover = frustum_cover(&cam, zoom, usize::MAX);
                let bbox = bbox_tile_count(&cam, zoom);
                assert!(
                    (cover.len() as u64) <= bbox,
                    "pitch {pitch} {w}x{h}: cover {} exceeds bbox {bbox}",
                    cover.len()
                );
                let mut rows: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
                for t in &cover {
                    *rows.entry(t.y).or_insert(0) += 1;
                }
                let max_row = rows.values().copied().max().unwrap_or(0);
                assert!(
                    max_row <= 200,
                    "pitch {pitch} {w}x{h}: a tile row holds {max_row} tiles (near-plane clip regression)"
                );
            }
        }
    }

    #[test]
    fn the_frustum_cover_comes_out_nearest_the_camera_first() {
        let cam = singapore(16.0, 55.0, 0.3, 900.0, 900.0);
        let eye = crate::render::camera::eye_ground_point(&cam);
        let cover = frustum_cover(&cam, 16, 300);
        let d: Vec<f64> = cover.iter().map(|t| ground_distance_to_tile(*t, eye)).collect();
        assert!(d.windows(2).all(|w| w[0] <= w[1]), "not sorted by distance from the eye's ground point");
    }

    #[test]
    fn at_sixty_degrees_the_bbox_is_the_world_and_the_traversal_still_stops_at_its_limit() {
        // The hazard Deviation 2 describes, pinned: this is why the cap and
        // MAX_PITCH land together in Task 9.
        let cam = singapore(16.0, 60.0, 0.0, 900.0, 900.0);
        assert!(bbox_tile_count(&cam, 16) >= 1_000_000_000, "{}", bbox_tile_count(&cam, 16));
        assert_eq!(frustum_cover(&cam, 16, 512).len(), 512);
    }

    #[test]
    fn the_cap_never_cuts_a_frame_the_fade_cannot_hide() {
        // Below 50 degrees the horizon fade is exactly zero, so a cut there
        // would show as a hard edge. MAX_COVER_TILES must hold every visible
        // tile at 49.9 degrees on every measured viewport and zoom.
        for (w, h) in [(1290.0, 2796.0), (430.0, 932.0), (900.0, 900.0)] {
            for zoom in [14u8, 16, 18] {
                for bearing in [0.0f32, 0.6] {
                    let cam = singapore(f64::from(zoom), 49.9, bearing, w, h);
                    // Unbounded is safe here, in a test: at 49.9 degrees the
                    // top ray is 10 degrees short of the horizon.
                    let all = frustum_cover(&cam, zoom, usize::MAX).len();
                    assert!(all <= MAX_COVER_TILES, "{w}x{h} z{zoom} bearing {bearing}: {all} visible > cap {MAX_COVER_TILES}");
                    assert_eq!(flat_cover(&cam, zoom).reach, None);
                }
            }
        }
    }

    /// At 60 degrees the top ray is horizontal. Before the distance LOD the
    /// cap cut the cover at 512 z16 tiles; now the cover reaches the LOD's
    /// horizon in a few dozen mixed-zoom tiles, and THAT is where it says
    /// it stops -- the cap never engages.
    #[test]
    fn at_max_pitch_the_cover_stops_at_the_lod_horizon_and_says_where() {
        for (w, h) in [(900.0, 900.0), (430.0, 932.0), (1290.0, 2796.0)] {
            let cam = singapore(16.0, 60.0, 0.4, w, h);
            let cover = flat_cover(&cam, 16);
            assert!(cover.tiles.len() < MAX_COVER_TILES, "{w}x{h}: the cap cut");
            let reach = cover.reach.expect("the horizon is on screen at 60 degrees");
            let lod = Lod::new(&cam, 16u8.into());
            assert_eq!(reach, lod.horizon(), "{w}x{h}: the reach is the LOD horizon, not a cap cut");
            let eye = crate::render::camera::eye_ground_point(&cam);
            assert!(cover.tiles.iter().all(|t| ground_distance_to_tile(*t, eye) <= reach), "a kept tile lies beyond the reach");
        }
        // The uniform walk still stops at the cap, nearest first.
        let cam = singapore(16.0, 60.0, 0.4, 900.0, 900.0);
        assert_eq!(frustum_cover(&cam, 16, MAX_COVER_TILES).len(), MAX_COVER_TILES);
    }

    #[test]
    fn at_pitch_zero_the_cover_is_the_viewport() {
        let cam = singapore(16.0, 0.0, 0.0, 900.0, 900.0);
        let cover = flat_cover(&cam, 16);
        assert_eq!(cover.reach, None);
        for tile in &cover.tiles {
            assert!(crate::render::tile_in_frustum(*tile, &cam), "{tile:?} is outside a flat frame");
        }
        // 900 px at z16 with 512 px tiles: at most 3 x 3 whole tiles
        // straddled, so 16 at most.
        assert!(cover.tiles.len() <= 16, "{}", cover.tiles.len());
    }

    #[test]
    fn the_unified_cover_covers_an_overzoomed_viewport() {
        // The browser and iOS cap tile zoom at the source's 14 while the
        // camera sits at 17.5. The four viewport corners must each land in a
        // returned z14 tile.
        let cam = singapore(17.5, 0.0, 0.0, 900.0, 900.0);
        let tiles: std::collections::HashSet<TileId> = flat_cover(&cam, 14).tiles.into_iter().collect();
        for (sx, sy) in [(0.0, 0.0), (899.0, 0.0), (0.0, 899.0), (899.0, 899.0)] {
            let g = crate::render::unproject_to_ground_f64(&cam, sx, sy).unwrap();
            let world = f64::from(crate::render::WORLD_SIZE);
            let n = f64::from(1u32 << 14);
            let t = TileId { z: 14, x: (g.x / world * n) as u32, y: (g.y / world * n) as u32 };
            assert!(tiles.contains(&t), "corner ({sx}, {sy}) lands in {t:?}, not in the cover");
        }
    }

    /// The retired native formula (`flat_cover_pitch_aware`), verbatim, kept ONLY as the reference the
    /// pitch-0 identity test below compares against.
    fn reference_native_square_cover(camera: &Camera, zoom: u8) -> Vec<TileId> {
        let (min_lon, min_lat, max_lon, max_lat) = crate::render::camera::pitch_aware_tile_bbox(camera);
        let center = ((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0);
        let width_px = (max_lon - min_lon) / 360.0 * (2f64.powi(i32::from(zoom))) * crate::geo::TILE_SIZE;
        crate::geo::visible_tiles(center, zoom, (width_px, width_px))
    }

    /// The retired browser/iOS formula (`flat_cover_scaled`), verbatim, for
    /// the same purpose.
    fn reference_browser_viewport_cover(camera: &Camera, zoom: u8) -> Vec<TileId> {
        let scale = 2f64.powf(camera.zoom - zoom as f64);
        crate::geo::visible_tiles(
            (camera.center_lon, camera.center_lat),
            zoom,
            (camera.viewport_width_px as f64 / scale, camera.viewport_height_px as f64 / scale),
        )
    }

    #[test]
    fn at_pitch_zero_native_draws_the_same_tiles_in_the_same_order() {
        // Ruling F3: the pitch-0 frame must not change. Native never drew
        // its whole cover: `paint` filters it through `tile_in_frustum` into
        // `visible_and_culled`, and THAT list is what is drawn and what
        // labels are placed from, in its order. The retired cover was a
        // square around the viewport (25 tiles at 900x900 z16 where 9 are
        // on screen), so the covers themselves differ by design; the list
        // native draws from must not differ by one element or one position.
        for (w, h) in [(1290.0, 2796.0), (430.0, 932.0), (900.0, 900.0)] {
            for zoom in [14u8, 16, 18] {
                let cam = singapore(f64::from(zoom), 0.0, 0.0, w, h);
                let drawn = |tiles: Vec<TileId>| -> Vec<TileId> {
                    tiles.into_iter().filter(|&t| crate::render::tile_in_frustum(t, &cam)).collect()
                };
                let new = flat_cover(&cam, zoom);
                assert_eq!(new.reach, None);
                assert_eq!(
                    drawn(new.tiles.clone()),
                    drawn(reference_native_square_cover(&cam, zoom)),
                    "{w}x{h} z{zoom}: native's pitch-0 draw list changed"
                );
                // Nothing the new cover returns is thrown away at draw time.
                assert_eq!(drawn(new.tiles.clone()), new.tiles, "{w}x{h} z{zoom}");
            }
        }
    }

    #[test]
    fn at_pitch_zero_the_browser_and_ios_keep_every_on_screen_tile_in_order() {
        // The browser and iOS drew their whole cover (no `tile_in_frustum`
        // pass). `geo::visible_tiles` rounds its max edge UP, so the
        // retired formula also returned one off-screen column and row past
        // the right and bottom edges. The new cover drops only those: every
        // tile of the old cover that touches the viewport is still there,
        // in the same relative order.
        for (w, h) in [(1290.0, 2796.0), (430.0, 932.0), (900.0, 900.0)] {
            for zoom in [14u8, 16, 18] {
                let cam = singapore(f64::from(zoom), 0.0, 0.0, w, h);
                let on_screen: Vec<TileId> = reference_browser_viewport_cover(&cam, zoom)
                    .into_iter()
                    .filter(|&t| crate::render::tile_in_frustum(t, &cam))
                    .collect();
                assert_eq!(flat_cover(&cam, zoom).tiles, on_screen, "{w}x{h} z{zoom}");
            }
        }
    }

    /// The measurement itself. `#[ignore]` because it prints a report rather
    /// than asserting one; Task 8's Step 4 runs it and records the output.
    #[test]
    #[ignore = "prints docs/measurements/2026-09-24-tilted-tile-cover.md's table; run with --ignored --nocapture"]
    fn print_tilted_cover_table() {
        const LIMIT: usize = 200_000;
        println!("| viewport (logical px) | zoom | pitch | bbox tiles, before trim | frustum tiles, after trim | reach of the 512th nearest, multiples of `Camera::distance()` from the eye's ground point |");
        println!("|---|---|---|---|---|---|");
        for (w, h) in [(1290.0f32, 2796.0f32), (430.0, 932.0), (900.0, 900.0)] {
            for zoom in [14u8, 16, 18] {
                for pitch in [0.0f32, 45.0, 50.0, 60.0] {
                    let cam = singapore(f64::from(zoom), pitch, 0.0, w, h);
                    let before = bbox_tile_count(&cam, zoom);
                    let after = frustum_cover(&cam, zoom, LIMIT);
                    let shown = if after.len() >= LIMIT { format!(">= {LIMIT} (limit)") } else { after.len().to_string() };
                    let eye = crate::render::camera::eye_ground_point(&cam);
                    // F16(c): the "512th nearest" is index 511 (0-based), not 512.
                    let reach = after
                        .get(511)
                        .map_or("fewer than 512 tiles".to_string(), |t| format!("{:.2}", ground_distance_to_tile(*t, eye) / cam.distance()));
                    println!("| {w}x{h} | z{zoom} | {pitch} | {before} | {shown} | {reach} |");
                }
            }
        }
    }

    /// The distance-LOD measurement: the uniform cover (capped, as before
    /// the LOD) beside the LOD cover, with its zoom span and reach.
    #[test]
    #[ignore = "prints docs/measurements/2026-09-24-tilted-tile-cover.md's LOD table; run with --ignored --nocapture"]
    fn print_lod_cover_table() {
        println!("| viewport (logical px) | zoom | pitch | uniform cover (before, capped at 512) | LOD cover (after) | zooms | reach, x `distance()` |");
        println!("|---|---|---|---|---|---|---|");
        for (w, h) in [(1290.0f32, 2796.0f32), (430.0, 932.0), (900.0, 900.0)] {
            for zoom in [14u8, 16, 18] {
                for pitch in [0.0f32, 45.0, 50.0, 55.0, 60.0] {
                    let cam = singapore(f64::from(zoom), pitch, 0.0, w, h);
                    let before = frustum_cover(&cam, zoom, MAX_COVER_TILES).len();
                    let after = flat_cover(&cam, zoom);
                    let (fine, coarse) = zoom_span(&after.tiles).unwrap();
                    let span = if fine == coarse { format!("z{fine}") } else { format!("z{fine}..z{coarse}") };
                    let reach = after.reach.map_or("-".to_string(), |r| format!("{:.1}", r / cam.distance()));
                    println!("| {w}x{h} | z{zoom} | {pitch} | {before} | {} | {span} | {reach} |", after.tiles.len());
                }
            }
        }
        println!();
        println!("| viewport | camera zoom | pitch | coarse prefetch z10, before (uniform) | after (`CoverZoom::coarser`) |");
        println!("|---|---|---|---|---|");
        for (w, h) in [(1290.0f32, 2796.0f32), (430.0, 932.0), (900.0, 900.0)] {
            for pitch in [0.0f32, 50.0, 60.0] {
                let cam = singapore(15.38, pitch, 0.0, w, h);
                let before = frustum_cover(&cam, 10, MAX_COVER_TILES).len();
                let after = flat_cover(&cam, CoverZoom::coarser(&cam, 14, 10)).tiles.len();
                println!("| {w}x{h} | 15.38 (tiles z14) | {pitch} | {before} | {after} |");
            }
        }
    }

    #[test]
    fn below_fifty_degrees_the_fade_is_zero() {
        let bg = Some(crate::style::Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 });
        for pitch in [0.0, 30.0, 45.0, 49.99] {
            let cam = singapore(16.0, pitch, 0.0, 900.0, 900.0);
            // Even with a (hypothetical) cut, the fade must not engage below 50.
            assert_eq!(horizon_fog(&cam, Some(0.01), bg), crate::render::gpu::HorizonFog::NONE, "pitch {pitch}");
        }
    }

    /// The pitch-0 identity, end to end: below 50 degrees the uniform the
    /// shells write is exactly zero in every field, whatever the cover's
    /// actual reach is (the real cover cuts nothing there, so `None`; a
    /// fabricated `Some` must not engage it either).
    #[test]
    fn below_fifty_degrees_the_fade_uniform_is_all_zero() {
        let bg = Some(crate::style::Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 });
        for (w, h) in [(1290.0, 2796.0), (430.0, 932.0), (900.0, 900.0)] {
            for pitch in [0.0, 45.0, 49.9] {
                let cam = singapore(16.0, pitch, 0.3, w, h);
                let actual = flat_cover(&cam, 16).reach;
                for reach in [actual, Some(cam.distance()), Some(0.01)] {
                    let fog = horizon_fog(&cam, reach, bg);
                    assert_eq!((fog.start, fog.end, fog.color), (0.0, 0.0, [0.0; 4]), "{w}x{h} pitch {pitch}");
                }
            }
        }
    }

    #[test]
    fn nothing_cut_or_no_background_means_no_fade() {
        let cam = singapore(16.0, 60.0, 0.0, 900.0, 900.0);
        let bg = Some(crate::style::Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 });
        assert_eq!(horizon_fog(&cam, None, bg), crate::render::gpu::HorizonFog::NONE);
        assert_eq!(horizon_fog(&cam, Some(0.01), None), crate::render::gpu::HorizonFog::NONE);
    }

    #[test]
    fn at_max_pitch_the_fade_ends_inside_the_cut_and_starts_beyond_the_centre() {
        let bg = crate::style::Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 };
        for (w, h) in [(900.0, 900.0), (430.0, 932.0)] {
            let cam = singapore(16.0, 60.0, 0.5, w, h);
            let reach = flat_cover(&cam, 16).reach.expect("capped at 60 degrees");
            let fog = horizon_fog(&cam, Some(reach), Some(bg));
            assert!(fog.end > 0.0 && fog.start < fog.end, "{fog:?}");
            assert!(f64::from(fog.start) > cam.distance(), "the middle of the screen is never fogged: {fog:?}");
            // Every visible ground point at the cut distance is at or past
            // the end depth, so no fragment of a missing tile is unfogged.
            let vp = cam.view_projection_f64();
            let eye = crate::render::camera::eye_ground_point(&cam);
            for i in 0..720 {
                let a = std::f64::consts::TAU * f64::from(i) / 720.0;
                let c = vp * glam::DVec4::new(eye.x + reach * a.cos(), eye.y + reach * a.sin(), 0.0, 1.0);
                if c.w > 0.0 && c.x.abs() <= c.w && c.y.abs() <= c.w {
                    assert!(c.w >= f64::from(fog.end), "{w}x{h}: a point at the cut shows at depth {} < fog end {}", c.w, fog.end);
                }
            }
            assert_eq!(fog.color, [bg.r, bg.g, bg.b, bg.a]);
        }
    }

    /// Ruling F12, half one: the band can never collapse, so `smoothstep`
    /// is never handed `edge0 >= edge1`. Swept over reaches from far inside
    /// the screen's nearest ground (where the unguarded band is empty) to
    /// far beyond it, on every measured viewport and a spread of bearings.
    #[test]
    fn the_fade_band_never_collapses() {
        let bg = Some(crate::style::Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 });
        let mut engaged = 0;
        for (w, h) in [(1290.0, 2796.0), (430.0, 932.0), (900.0, 900.0)] {
            for zoom in [2.0, 14.0, 16.0, 22.0] {
                for bearing in [0.0f32, 0.5, 2.0, 4.0] {
                    for pitch in [50.0f32, 55.0, 60.0] {
                        let cam = singapore(zoom, pitch, bearing, w, h);
                        for k in [1e-6, 1e-3, 0.1, 0.5, 1.0, 1.5, 3.0, 9.0, 20.0, 28.0, 100.0] {
                            let fog = horizon_fog(&cam, Some(k * cam.distance()), bg);
                            if fog == crate::render::gpu::HorizonFog::NONE {
                                continue;
                            }
                            engaged += 1;
                            let min_band = (1e-3 * fog.end).max(FOG_MIN_BAND_EPSILON);
                            assert!(fog.end > 0.0 && fog.end.is_finite() && fog.start.is_finite(), "{fog:?}");
                            assert!(
                                fog.end >= fog.start + min_band,
                                "{w}x{h} z{zoom} bearing {bearing} pitch {pitch} reach {k}x: degenerate band {fog:?}"
                            );
                        }
                    }
                }
            }
        }
        assert!(engaged > 100, "the sweep must actually exercise the fade, engaged {engaged} times");
    }

    /// Ruling F12, half two: the nearest on-screen depth of the cut is found
    /// at the screen's EDGE, not between two azimuth samples. On the circle
    /// `|p - eye| = reach`, depth is lowest where the circle leaves the
    /// viewport, so a coarse sample can land just inside and overshoot the
    /// true minimum; a dense 20,000-azimuth scan must never find a visible
    /// cut point shallower than the end.
    #[test]
    fn the_fade_ends_before_the_cut_at_the_screen_edge_on_every_viewport_and_bearing() {
        let bg = Some(crate::style::Rgba { r: 0.97, g: 0.96, b: 0.94, a: 1.0 });
        for (w, h) in [(1290.0, 2796.0), (430.0, 932.0), (900.0, 900.0)] {
            for bearing in [0.0f32, 0.3, 1.1, 2.9, 5.0] {
                for pitch in [52.0f32, 60.0] {
                    let cam = singapore(16.0, pitch, bearing, w, h);
                    let Some(reach) = flat_cover(&cam, 16).reach else { continue };
                    let fog = horizon_fog(&cam, Some(reach), bg);
                    assert!(fog.end > 0.0, "{w}x{h} bearing {bearing} pitch {pitch}: cut but no fade");
                    let vp = cam.view_projection_f64();
                    let eye = crate::render::camera::eye_ground_point(&cam);
                    for i in 0..20_000 {
                        let a = std::f64::consts::TAU * f64::from(i) / 20_000.0;
                        let c = vp * glam::DVec4::new(eye.x + reach * a.cos(), eye.y + reach * a.sin(), 0.0, 1.0);
                        if c.w > 0.0 && c.x.abs() <= c.w && c.y.abs() <= c.w {
                            assert!(
                                c.w >= f64::from(fog.end),
                                "{w}x{h} bearing {bearing} pitch {pitch}: cut shows at depth {} < end {}",
                                c.w,
                                fog.end
                            );
                        }
                    }
                }
            }
        }
    }

    /// Task 9 minor 2: a quad whose corners alternate in front of and behind
    /// the near plane -- impossible for an exact affine `w`, reachable once
    /// `w` is NaN or rounded -- yields up to eight polygon vertices. The old
    /// five-slot stack polygon indexed out of bounds and panicked in the
    /// per-frame path; it must instead answer "may be visible".
    #[test]
    fn a_degenerate_or_nan_clip_quad_never_panics_and_counts_as_visible() {
        let (near, far) = (1.0, 1e9);
        let alternating = [
            glam::DVec4::new(0.0, 0.0, 0.5, 2.0),
            glam::DVec4::new(0.0, 0.0, 0.5, 0.0),
            glam::DVec4::new(0.0, 0.0, 0.5, 2.0),
            glam::DVec4::new(0.0, 0.0, 0.5, 0.0),
        ];
        assert!(clip_quad_may_be_visible(alternating, near, far));
        let nan = glam::DVec4::new(f64::NAN, f64::NAN, f64::NAN, f64::NAN);
        let with_nan = [alternating[0], nan, alternating[2], nan];
        assert!(clip_quad_may_be_visible(with_nan, near, far));
        assert!(clip_quad_may_be_visible([nan; 4], near, far));
        let behind = glam::DVec4::new(0.0, 0.0, 0.5, 0.0);
        assert!(clip_quad_may_be_visible([behind, nan, nan, nan], near, far), "an empty clip is not a verdict");
        // And through the public entry point, with a poisoned matrix.
        let poisoned = glam::DMat4::from_cols_array(&[f64::NAN; 16]);
        assert!(tile_may_be_visible(TileId { z: 3, x: 2, y: 5 }, &poisoned, near, far));
    }

    /// Task 9 minor 1 / ruling F10: the GLOBE arm has no cap of its own.
    /// Measured at `MAX_PITCH` over the whole band it stays under
    /// `MAX_COVER_TILES` (largest: 451 at 1290x2796, z11.0), so the globe
    /// needs no cut and no reach. Held here so a camera change that pushes
    /// it past the cap fails instead of silently truncating.
    #[test]
    fn the_globe_cover_at_max_pitch_fits_under_the_cap() {
        let pitch = crate::camera_control::MAX_PITCH.to_degrees();
        // How many cases the globe arm actually answered: a camera change
        // that took every one of them out of the band would otherwise leave
        // this passing on zero checks.
        let (mut engaged, mut cases) = (0, 0);
        for (w, h) in [(1290.0, 2796.0), (430.0, 932.0), (900.0, 900.0)] {
            for bearing in [0.0f32, 0.7, 2.5] {
                for tenth in 50..120 {
                    let zoom = f64::from(tenth) / 10.0;
                    let cam = singapore(zoom, pitch, bearing, w, h);
                    let globe = GlobeCover::new(&globe_frame(&cam));
                    cases += 1;
                    if let Some(cover) = globe.cover(zoom.floor() as u8) {
                        engaged += 1;
                        assert!(
                            cover.len() <= MAX_COVER_TILES,
                            "{w}x{h} z{zoom} bearing {bearing}: globe cover {} > {MAX_COVER_TILES}",
                            cover.len()
                        );
                    }
                }
            }
        }
        assert!(engaged > 0, "the globe arm answered none of the {cases} cases: this test checked nothing");
    }

    /// Task 11's fold-in from Task 9 (minor 1): the GLOBE arm's cover length
    /// at `MAX_PITCH` across the globe band, which has no cap of its own.
    /// Prints the table recorded in `docs/measurements/2026-09-24-tilted-tile-cover.md`.
    #[test]
    #[ignore = "prints the globe-cover table; run with --ignored --nocapture"]
    fn print_globe_cover_table() {
        println!("| viewport (logical px) | camera zoom | cover zoom | pitch | globe? | GlobeCover::cover len |");
        println!("|---|---|---|---|---|---|");
        for (w, h) in [(1290.0f32, 2796.0f32), (430.0, 932.0), (900.0, 900.0)] {
            for zoom in [5.0f64, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 11.5, 11.9] {
                let cam = singapore(zoom, 60.0, 0.0, w, h);
                let globe = GlobeCover::new(&globe_frame(&cam));
                let z = zoom.floor() as u8;
                let len = globe.cover(z).map_or("-".to_string(), |c| c.len().to_string());
                println!("| {w}x{h} | {zoom} | z{z} | 60 | {} | {len} |", globe.is_globe());
            }
        }
    }
}


/// The distance LOD's contract, in terms of `flat_cover`'s public shape
/// only (so these compile, and fail, against the uniform cover too).
#[cfg(test)]
mod lod_tests {
    use super::*;
    use crate::render::camera::Camera;

    const VIEWPORTS: [(f32, f32); 3] = [(1290.0, 2796.0), (430.0, 932.0), (900.0, 900.0)];
    const BEARINGS: [f32; 4] = [0.0, 0.6, 2.2, 4.0];

    /// The most tiles a 60-degree cover may hold on the measured viewports.
    /// The uniform cover hit `MAX_COVER_TILES` (512) on every one of them.
    const LOD_COVER_BOUND: usize = 64;

    fn singapore(zoom: f64, pitch_deg: f32, bearing: f32, w: f32, h: f32) -> Camera {
        Camera {
            center_lon: 103.8198,
            center_lat: 1.3521,
            zoom,
            pitch: pitch_deg.to_radians(),
            bearing,
            viewport_width_px: w,
            viewport_height_px: h,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    fn contains(ancestor: TileId, tile: TileId) -> bool {
        ancestor.z <= tile.z && (tile.x >> (tile.z - ancestor.z)) == ancestor.x && (tile.y >> (tile.z - ancestor.z)) == ancestor.y
    }

    fn tile_at(zoom: u8, p: glam::DVec2) -> TileId {
        let size = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << zoom);
        let n = (1i64 << zoom) - 1;
        let idx = |v: f64| ((v / size).floor() as i64).clamp(0, n) as u32;
        TileId { z: zoom, x: idx(p.x), y: idx(p.y) }
    }

    #[test]
    fn a_sixty_degree_cover_is_a_few_dozen_tiles_not_the_cap() {
        for (w, h) in VIEWPORTS {
            for zoom in [14u8, 16, 18] {
                for bearing in BEARINGS {
                    let cam = singapore(f64::from(zoom), 60.0, bearing, w, h);
                    let n = flat_cover(&cam, zoom).tiles.len();
                    assert!(n <= LOD_COVER_BOUND, "{w}x{h} z{zoom} bearing {bearing}: {n} tiles > {LOD_COVER_BOUND}");
                }
            }
        }
    }

    /// The iPhone status line's case: camera z15.38, source-clamped tiles
    /// z14, and the coarse prefetch cover four levels down at z10 -- which
    /// also hit 512 at 60 degrees.
    #[test]
    fn the_coarse_prefetch_cover_at_sixty_degrees_is_small_too() {
        let cam = singapore(15.38, 60.0, 0.0, 430.0, 932.0);
        let display = flat_cover(&cam, 14).tiles.len();
        let prefetch = flat_cover(&cam, 10).tiles.len();
        assert!(display <= LOD_COVER_BOUND, "display {display}");
        assert!(prefetch <= LOD_COVER_BOUND / 2, "prefetch {prefetch}");
    }

    #[test]
    fn near_the_eye_the_cover_is_at_full_zoom_and_the_far_ground_is_coarser() {
        for (w, h) in VIEWPORTS {
            for zoom in [14u8, 16, 18] {
                for bearing in BEARINGS {
                    let cam = singapore(f64::from(zoom), 60.0, bearing, w, h);
                    let tiles = flat_cover(&cam, zoom).tiles;
                    // The bottom edge of the screen: the nearest ground.
                    for sx in [0.0, w / 2.0, w] {
                        let g = crate::render::unproject_to_ground_f64(&cam, sx, h).unwrap().truncate();
                        let under = tiles.iter().find(|t| contains(**t, tile_at(zoom, g)));
                        assert_eq!(under.map(|t| t.z), Some(zoom), "{w}x{h} z{zoom} bearing {bearing}: bottom ({sx}, {h})");
                    }
                    let coarsest = tiles.iter().map(|t| t.z).min().unwrap();
                    assert!(coarsest + 2 <= zoom, "{w}x{h} z{zoom} bearing {bearing}: coarsest z{coarsest}, the far ground is not coarser");
                }
            }
        }
    }

    /// Up the middle of the screen, bottom to top, the zoom of the tile
    /// under each ground point never goes back up.
    #[test]
    fn tile_zoom_never_increases_along_the_view_direction() {
        for (w, h) in VIEWPORTS {
            for pitch in [50.0f32, 55.0, 60.0] {
                for bearing in BEARINGS {
                    let cam = singapore(16.0, pitch, bearing, w, h);
                    let cover = flat_cover(&cam, 16);
                    let eye = crate::render::camera::eye_ground_point(&cam);
                    let mut last = u8::MAX;
                    for i in 0..=400 {
                        let sy = h * (1.0 - i as f32 / 400.0);
                        let Some(g) = crate::render::unproject_to_ground_f64(&cam, w / 2.0, sy) else { continue };
                        let g = g.truncate();
                        if cover.reach.is_some_and(|r| (g - eye).length() >= r) {
                            break;
                        }
                        let t = cover.tiles.iter().find(|t| contains(**t, tile_at(24, g))).expect("covered");
                        assert!(t.z <= last, "{w}x{h} pitch {pitch} bearing {bearing}: z{} after z{last} at screen y {sy}", t.z);
                        last = t.z;
                    }
                }
            }
        }
    }

    /// No tile together with its own ancestor, and every visible ground
    /// point short of the reach is in EXACTLY one tile.
    #[test]
    fn the_mixed_cover_has_no_overlaps_and_no_holes() {
        const STEPS: i32 = 24;
        for (w, h) in VIEWPORTS {
            for zoom in [14u8, 16, 18] {
                for pitch in [30.0f32, 48.0, 50.0, 55.0, 60.0] {
                    for bearing in BEARINGS {
                        let cam = singapore(f64::from(zoom), pitch, bearing, w, h);
                        let cover = flat_cover(&cam, zoom);
                        for (i, a) in cover.tiles.iter().enumerate() {
                            for b in &cover.tiles[i + 1..] {
                                assert!(!contains(*a, *b) && !contains(*b, *a), "{w}x{h} z{zoom} pitch {pitch}: {a:?} overlaps {b:?}");
                            }
                        }
                        let eye = crate::render::camera::eye_ground_point(&cam);
                        for iy in 0..=STEPS {
                            for ix in 0..=STEPS {
                                let sx = w * ix as f32 / STEPS as f32;
                                let sy = h * iy as f32 / STEPS as f32;
                                let Some(g) = crate::render::unproject_to_ground_f64(&cam, sx, sy) else { continue };
                                let g = g.truncate();
                                if cover.reach.is_some_and(|r| (g - eye).length() >= r) {
                                    continue;
                                }
                                let point = tile_at(24, g);
                                let n = cover.tiles.iter().filter(|t| contains(**t, point)).count();
                                assert_eq!(n, 1, "{w}x{h} z{zoom} pitch {pitch} bearing {bearing}: screen ({sx}, {sy}) is in {n} tiles");
                            }
                        }
                    }
                }
            }
        }
    }

    /// The port against MapLibre GL JS itself: `createCalculateTileZoomFunction
    /// (9.314, 3.0)` from `covering_tiles.ts` (v6.7.0, with
    /// `maxMercatorHorizonAngle = 89.25`), run verbatim under node with
    /// `requestedCenterZoom`, `distanceToTile2D = k * D`, `distanceToTileZ =
    /// D * cos(pitch)`, `distanceToCenter3D = D` and a 60-degree FOV.
    #[test]
    fn the_lod_zoom_is_maplibres_to_within_rounding() {
        let reference: [(f64, f32, f64, f64); 8] = [
            (16.0, 60.0, 0.0, 17.207518749639423),
            (16.0, 60.0, 0.84, 15.880223047906215),
            (16.0, 60.0, 4.0, 13.075417078486439),
            (16.0, 60.0, 38.0, 8.633619405612404),
            (16.0, 50.0, 1.0, 15.627840091989704),
            (15.38, 55.0, 2.5, 13.424196564417262),
            (14.0, 80.0, 3.0, 11.08902234464108),
            (16.0, 49.0, 10.0, 11.418379440526243),
        ];
        for (zoom, pitch, k, expected) in reference {
            let cam = singapore(zoom, pitch, 0.0, 900.0, 900.0);
            let lod = Lod::new(&cam, CoverZoom { zoom: 22, center_zoom: Some(zoom), round: false, min_zoom: 0, building_floor: None, cutoff: None });
            let got = lod.desired(k * cam.distance());
            // The camera stores pitch as f32, so cos(pitch) differs from
            // the JS double in the 8th digit.
            assert!((got - expected).abs() < 1e-6, "z{zoom} pitch {pitch} d {k}D: {got} vs MapLibre {expected}");
        }
    }

    /// At or below MapLibre's variable-zoom pitch (48.5 degrees at this
    /// FOV) the cover is the uniform one, tile for tile and in order.
    #[test]
    fn at_or_below_the_lod_pitch_the_cover_is_the_uniform_one() {
        assert!((lod_min_pitch().to_degrees() - 48.5).abs() < 1e-9, "{}", lod_min_pitch().to_degrees());
        for (w, h) in VIEWPORTS {
            for zoom in [14u8, 16, 18] {
                for pitch in [0.0f32, 30.0, 45.0, 48.4] {
                    for bearing in BEARINGS {
                        let cam = singapore(f64::from(zoom), pitch, bearing, w, h);
                        let mut uniform = frustum_cover(&cam, zoom, MAX_COVER_TILES);
                        uniform.sort_unstable_by_key(|t| (t.x, t.y));
                        let cover = flat_cover(&cam, zoom);
                        assert_eq!(cover.tiles, uniform, "{w}x{h} z{zoom} pitch {pitch} bearing {bearing}");
                        assert_eq!(cover.reach, None);
                    }
                }
            }
        }
    }

    /// The coarse prefetch request: never finer than `coarse`, no holes, no
    /// overlaps, no bigger than the display cover, and (its LOD shifted
    /// down by the same delta) every display tile has an ancestor-or-self
    /// at least `delta` levels up... or at the source minimum.
    #[test]
    fn the_coarse_prefetch_cover_is_the_display_lod_shifted_down() {
        for (w, h) in VIEWPORTS {
            for pitch in [0.0f32, 50.0, 60.0] {
                for bearing in BEARINGS {
                    let cam = singapore(15.38, pitch, bearing, w, h);
                    let display = flat_cover(&cam, 14).tiles;
                    let request = CoverZoom::coarser(&cam, 14, 10);
                    assert_eq!(request.zoom, 10);
                    let prefetch = flat_cover(&cam, request).tiles;
                    let eye = crate::render::camera::eye_ground_point(&cam);
                    let nearest = display.iter().min_by(|a, b| ground_distance_to_tile(**a, eye).total_cmp(&ground_distance_to_tile(**b, eye))).unwrap();
                    let over_nearest = prefetch.iter().find(|p| contains(**p, *nearest)).expect("the nearest display tile has a stand-in");
                    assert_eq!(over_nearest.z + 4, nearest.z, "{w}x{h} pitch {pitch} bearing {bearing}: near the eye the stand-in is delta levels up");
                    assert!(prefetch.iter().all(|t| t.z <= 10), "{w}x{h} pitch {pitch}: finer than z10");
                    assert!(prefetch.len() <= display.len().max(4), "{w}x{h} pitch {pitch}: {} > {}", prefetch.len(), display.len());
                    // Every display tile has a prefetch tile over it (itself
                    // or an ancestor). Near the eye it is `delta` levels up
                    // (asserted above); toward the horizon fewer, down to the
                    // same tile, because a bigger tile's nearest point is
                    // nearer the eye, so its own LOD target is relatively
                    // finer -- which only means the far prefetch costs nothing
                    // the display cover does not already fetch.
                    for d in &display {
                        let _ = prefetch.iter().find(|p| contains(**p, *d)).unwrap_or_else(|| panic!("{w}x{h} pitch {pitch} bearing {bearing}: no prefetch tile over {d:?}"));
                    }
                }
            }
        }
        // At pitch 0 it is exactly the old coarse cover.
        let cam = singapore(15.38, 0.0, 0.0, 430.0, 932.0);
        assert_eq!(flat_cover(&cam, CoverZoom::coarser(&cam, 14, 10)), flat_cover(&cam, 10));
    }

    /// Imagery: the request's zoom is `raster_cover_zoom`'s, rounded, and
    /// under the LOD the near ground stays at it.
    #[test]
    fn the_imagery_request_keeps_its_rounded_zoom_near_the_eye() {
        let request = crate::render::raster_plan::raster_cover_request(16.3, 256, 3.0, 22);
        assert_eq!(request.zoom, crate::render::raster_plan::raster_cover_zoom(16.3, 256, 3.0, 22));
        assert!(request.round);
        let cam = singapore(16.3, 60.0, 0.0, 430.0, 932.0);
        let cover = flat_cover(&cam, request);
        assert_eq!(zoom_span(&cover.tiles).map(|s| s.0), Some(request.zoom));
        // 256 px imagery at 3x is covered at z19: finer tiles than the
        // vectors', so more of them -- but the uniform cover was 512.
        assert!(cover.tiles.len() <= MAX_COVER_TILES / 2, "{}", cover.tiles.len());
        assert_eq!(frustum_cover(&cam, request.zoom, MAX_COVER_TILES).len(), MAX_COVER_TILES);
        // Flat: every tile at the request's zoom, as before.
        let flat = singapore(16.3, 0.0, 0.0, 430.0, 932.0);
        assert_eq!(flat_cover(&flat, request), flat_cover(&flat, request.zoom));
    }

    /// Under the LOD the display cover's and the coarse cover's far ground
    /// can be the same tile; `bake_order` bakes it once, in its cover class.
    #[test]
    fn a_tile_in_both_covers_is_baked_once_as_a_cover_tile() {
        let cam = singapore(15.38, 60.0, 0.0, 1290.0, 2796.0);
        let cover = flat_cover(&cam, 14).tiles;
        let coarse = flat_cover(&cam, CoverZoom::coarser(&cam, 14, 10)).tiles;
        let shared: Vec<TileId> = coarse.iter().copied().filter(|t| cover.contains(t)).collect();
        assert!(!shared.is_empty(), "this case needs a tile in both covers");
        let order = bake_order(&cover, &coarse, |_| None, |_| true, 15, &cam);
        let unique: std::collections::HashSet<TileId> = order.iter().map(|(t, _)| *t).collect();
        assert_eq!(unique.len(), order.len(), "a tile is baked twice");
        for t in shared {
            assert!(order.contains(&(t, false)), "{t:?} must bake as a cover tile");
        }
    }

    /// The number this fix's own memory review is against: a source whose
    /// ceiling moved from z14 to z16 (Mapbox composite, see
    /// `crate::source_zoom`) asks the LOD for tiles two levels finer, which
    /// is roughly 4x as many per unit ground area. `flat_cover` never reads
    /// a source's ceiling itself -- it takes whatever zoom its caller
    /// already clamped to (`tile_zoom()` in both shells, `cover_tile_zoom`
    /// natively) -- so this is not a regression test for the defect (that
    /// lived entirely in the shells' now-removed `SOURCE_MAX_ZOOM`
    /// constant); it is the reportable evidence that asking the LOD at the
    /// new ceiling still holds, on the phone viewport the task measured
    /// against (430x932, an iPhone 15 Pro Max/14 Plus in points).
    #[test]
    fn a_z16_source_ceiling_keeps_tile_counts_reasonable_on_a_phone_viewport() {
        let (w, h) = (430.0, 932.0);
        for zoom in [16u8, 17] {
            let flat_n = flat_cover(&singapore(f64::from(zoom), 0.0, 0.0, w, h), zoom).tiles.len();
            let pitched_n = flat_cover(&singapore(f64::from(zoom), 60.0, 0.0, w, h), zoom).tiles.len();
            eprintln!("[source-max-zoom] z{zoom} {w}x{h}: pitch 0 -> {flat_n} tiles, pitch 60 -> {pitched_n} tiles");
            assert!(flat_n <= MAX_COVER_TILES, "z{zoom} pitch 0: {flat_n} tiles > cap {MAX_COVER_TILES}");
            assert!(pitched_n <= LOD_COVER_BOUND, "z{zoom} pitch 60: {pitched_n} tiles > LOD bound {LOD_COVER_BOUND}");
        }
    }

    #[test]
    fn a_mixed_cover_draws_finest_first_then_in_x_y_order() {
        let cam = singapore(16.0, 60.0, 0.6, 430.0, 932.0);
        let tiles = flat_cover(&cam, 16).tiles;
        assert!(tiles.windows(2).all(|w| (std::cmp::Reverse(w[0].z), w[0].x, w[0].y) < (std::cmp::Reverse(w[1].z), w[1].x, w[1].y)));
        assert_eq!(zoom_span(&tiles).map(|s| s.0), Some(16));
        assert_eq!(zoom_span(&[]), None);
        assert_eq!(zoom_span_label(zoom_span(&tiles), 16), format!("16..{}", zoom_span(&tiles).unwrap().1));
        assert_eq!(zoom_span_label(Some((14, 14)), 14), "14");
        assert_eq!(zoom_span_label(None, 13), "13");
    }
}
