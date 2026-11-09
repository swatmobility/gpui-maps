//! The world-anchored dot mask: one `R8Uint` texture of basemap ranks
//! (`overlay::dotgrid::mask_class::MaskRank`) whose texel grid is fixed to
//! the WORLD at the camera's integer zoom, so a dot's class depends on where
//! it is on Earth and which basemap tiles are loaded, never on the camera's
//! sub-pixel position (the spec's section 3.4, option D).
//!
//! - [`MaskGrid`] is the pure half: the texel ladder (`2^(zm+9)` texels per
//!   unit world, one per logical pixel at an integer zoom, never finer than
//!   z16's), the origin snapped to [`MASK_SNAP_TEXELS`] in world texel
//!   coordinates, the size cap, and the maps from a dot tile's lyon units
//!   and from a unit-world point to texels.
//! - [`relocation_region`] is the ground the mask must cover: the flat
//!   cover's box, cut where the data cells fall below
//!   `RELOCATION_MIN_CELL_PX` on screen (spec 3.5a).
//! - [`DotMaskGpu`] is the texture itself, rendered by
//!   `TilePipeline::encode_dot_mask` from the basemap tiles' mask index lists
//!   and re-rendered only when its [`MaskKey`] changes.
//!
//! The mask is never persisted and never read back outside
//! `#[cfg(any(test, feature = "probe"))]` (DP-8): the texture's copy-source
//! usage and [`DotMaskGpu::read_back`] exist only there.

use crate::overlay::dotgrid::mask_class::MaskRank;
use crate::render::dots::RELOCATION_MIN_CELL_PX;
use crate::render::Camera;

/// The finest mask zoom: texels never finer than `2^-25` world units
/// (about 1.2 m at the equator).
pub const MASK_MAX_ZOOM: u8 = 16;
/// The origin snaps to multiples of this many texels in world texel
/// coordinates, so a pan moves it once per 256 texels (256 px at an integer
/// zoom) and most frames keep the mask.
pub const MASK_SNAP_TEXELS: i64 = 256;
/// A mask that dropped a level below the cap climbs back only when the
/// finer grid's sides fit this (Task 9 ruling: 14 snaps of 256, 512
/// texels under [`MASK_TEXTURE_CAP`]).
pub const MASK_CLIMB_SIDE: i64 = 3584;
/// The texture's largest side, before the device's own
/// `max_texture_dimension_2d`: 4096² at one byte is 16 MB.
pub const MASK_TEXTURE_CAP: u32 = 4096;
/// A texel no drawn basemap tile covers: the mask margin past the drawn
/// tiles, a hole, and anything outside the mask rect. Below water; a dot
/// that reads it keeps candidate 0 (`hash::choose`, DP-5 ruling).
pub const MASK_NO_DATA_RANK: u32 = crate::overlay::dotgrid::hash::RANK_NO_DATA as u32;
/// What the mask is cleared to: no data. Each drawn tile's footprint is
/// then drawn as `MaskRank::Land` under its ranked fills.
pub const MASK_CLEAR_RANK: u32 = MASK_NO_DATA_RANK;
// `shader.wgsl`'s `DOT_RANK_NO_DATA` (what `mask_rank` returns outside the
// mask) is this; `the_shader_no_data_rank_matches_the_mask_clear_rank` pins it.
const _: () = assert!(MASK_CLEAR_RANK == 0 && (MaskRank::Water as u32) > MASK_NO_DATA_RANK);

/// `log2` of the texels per unit world at mask zoom 0: 512, one per logical
/// pixel of a 512-px world.
const TEXELS_PER_WORLD_LOG2_AT_Z0: i32 = 9;

/// A mask's texel grid: `2^(zm+9)` texels per unit world, NW origin in world
/// texel coordinates (a multiple of MASK_SNAP_TEXELS), size in texels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaskGrid {
    pub zm: u8,
    pub origin: [i64; 2],
    pub size: [u32; 2],
}

impl MaskGrid {
    #[must_use]
    pub fn texels_per_unit_world(&self) -> f64 {
        2f64.powi(i32::from(self.zm) + TEXELS_PER_WORLD_LOG2_AT_Z0)
    }

    /// `region`: unit-world [min_x, min_y, max_x, max_y] (x may exceed [0,1) across the
    /// antimeridian; y is clamped to the Mercator square), grown by `margin` (one data cell's
    /// width) east and west and by that cell's height ([`cell_margin_y`]) north and south: a
    /// cell is `stretch` times taller than wide in unit world, so a width-only margin let a
    /// cell on screen reach past the mask at high latitudes (DP-5, Nizhny Novgorod).
    /// None when the region is empty. Drops `zm` by one while either size > `max_dim.min(MASK_TEXTURE_CAP)`.
    ///
    /// The size depends only on the region's WIDTH and height, never on
    /// where it sits: `ceil(extent / 256) * 256 + 256` texels, the extra 256
    /// holding the up-to-255 texels the snapped origin sits below the
    /// region's edge. A pan therefore keeps the texture (no reallocation)
    /// and moves only the origin, in 256-texel steps.
    #[must_use]
    pub fn plan(zoom: f64, region: [f64; 4], margin: f64, max_dim: u32) -> Option<MaskGrid> {
        Self::plan_after(zoom, region, margin, max_dim, None, None)
    }

    /// [`Self::plan`] with hysteresis on the mask zoom (Task 9 ruling): a
    /// level is dropped only when a side would pass the cap, and -- when
    /// `previous` sat below the level the cap allows -- climbed back only
    /// while the finer grid's sides fit [`MASK_CLIMB_SIDE`]. A pan whose
    /// region hovers at the cap then keeps one resolution (a level flip
    /// re-places every dot whose candidates straddle a coarser texel).
    ///
    /// `bound`, when given, is the grown region's largest extent over any
    /// position of the same camera ([`mask_extent_bound`]): the level is
    /// chosen by it, so a pan -- which moves the cover's tile box by a
    /// whole tile -- never changes the level; the texture is still sized to
    /// the region itself.
    #[must_use]
    pub fn plan_after(
        zoom: f64,
        region: [f64; 4],
        margin: f64,
        max_dim: u32,
        previous: Option<&MaskGrid>,
        bound: Option<[f64; 2]>,
    ) -> Option<MaskGrid> {
        let [mut x0, mut y0, mut x1, mut y1] = region;
        let margin_y = cell_margin_y(region, margin);
        x0 -= margin;
        y0 -= margin_y;
        x1 += margin;
        y1 += margin_y;
        y0 = y0.max(0.0);
        y1 = y1.min(1.0);
        // More than one world wide covers every world copy's texel twice;
        // one world is all a candidate can be in.
        x1 = x1.min(x0 + 1.0);
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite() && x0 < x1 && y0 < y1 && zoom.is_finite()) {
            return None;
        }
        let cap = i64::from(max_dim.clamp(1, MASK_TEXTURE_CAP));
        let side = |extent: f64, t: f64| -> i64 {
            let texels = (extent * t).ceil() as i64;
            (texels + MASK_SNAP_TEXELS - 1).div_euclid(MASK_SNAP_TEXELS) * MASK_SNAP_TEXELS + MASK_SNAP_TEXELS
        };
        let extent = bound.map_or((x1 - x0, y1 - y0), |[w, h]| (w.max(x1 - x0), h.max(y1 - y0)));
        let sides = |zm: u8| {
            let t = MaskGrid { zm, origin: [0, 0], size: [0, 0] }.texels_per_unit_world();
            (side(extent.0, t), side(extent.1, t))
        };
        // The finest level the cap allows.
        let top = zoom.floor().clamp(0.0, f64::from(MASK_MAX_ZOOM)) as u8;
        let mut zm = top;
        while zm > 0 && { let (w, h) = sides(zm); w > cap || h > cap } {
            zm -= 1;
        }
        // Below it last time -- or no mask yet, which starts low: climb
        // only while the finer grid fits the climb side.
        if let Some(p) = previous.map(|p| p.zm).filter(|&p| p < zm) {
            let climb = MASK_CLIMB_SIDE.min(cap);
            let mut at = p;
            while at < zm && { let (w, h) = sides(at + 1); w <= climb && h <= climb } {
                at += 1;
            }
            zm = at;
        }
        let t = MaskGrid { zm, origin: [0, 0], size: [0, 0] }.texels_per_unit_world();
        let (w, h) = (side(x1 - x0, t), side(y1 - y0, t));
        let snap = |v: f64| (v * t).floor() as i64;
        let origin = [
            snap(x0).div_euclid(MASK_SNAP_TEXELS) * MASK_SNAP_TEXELS,
            snap(y0).div_euclid(MASK_SNAP_TEXELS) * MASK_SNAP_TEXELS,
        ];
        Some(MaskGrid { zm, origin, size: [w.min(cap) as u32, h.min(cap) as u32] })
    }

    /// The whole-world shift that puts unit-world `x` in the world copy
    /// nearest this grid's centre: a tile or point one world east or west
    /// of the grid addresses the same texels as its copy inside it.
    fn world_copy_shift(&self, x: f64) -> f64 {
        let t = self.texels_per_unit_world();
        let centre = (self.origin[0] as f64 + f64::from(self.size[0]) * 0.5) / t;
        (centre - x).round()
    }

    /// How a dot tile finds its candidates' texels in this grid, computed
    /// in f64 from the tile's unit-world origin (its world copy included):
    /// the shader computes `i32(floor(p * texels_per_lyon + frac)) +
    /// int_offset` for a candidate at tile lyon `p` ([`MaskTileLookup::texel`]).
    ///
    /// **Translation-invariant by construction.** The f32 part,
    /// `floor(p * texels_per_lyon + frac)`, depends only on the tile and
    /// the texel size -- never on the mask origin -- and the origin enters
    /// only through the integer `int_offset`. So a snap of the origin by
    /// 256 texels moves every lookup by exactly 256 and no candidate
    /// changes texel (DP-5). An f32 lyon origin `(p - origin) * tpl`
    /// would re-round at every snap.
    ///
    /// `frac` is the tile origin's sub-texel position: 0 whenever the
    /// tile corner is on the texel lattice, i.e. `zm + 9 >= tile z`
    /// (always for the z9 dot tiles).
    ///
    /// **`frac` never rounds up to 1.0.** A fraction just below 1 in f64
    /// could round to `1.0f32` and shift a lookup by a texel, but only a
    /// tile corner OFF the lattice has a non-zero fraction, and the
    /// coarsest mask zoom (0, 512 texels per world) already puts every
    /// corner of a tile at z <= 9 -- every dot tile -- on it: `frac` is
    /// exactly 0 there (`the_texel_mapping_matches_an_independent_f64_oracle`).
    #[must_use]
    pub fn for_tile(&self, tile_origin_unit_world: (f64, f64), unit_world_per_lyon: f64) -> MaskTileLookup {
        let t = self.texels_per_unit_world();
        let x = tile_origin_unit_world.0 + self.world_copy_shift(tile_origin_unit_world.0);
        let split = |world_texel: f64, origin: i64| {
            let whole = world_texel.floor();
            let offset = (whole as i64 - origin).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
            (offset, (world_texel - whole) as f32)
        };
        let (ix, fx) = split(x * t, self.origin[0]);
        let (iy, fy) = split(tile_origin_unit_world.1 * t, self.origin[1]);
        MaskTileLookup { int_offset: [ix, iy], frac: [fx, fy], texels_per_lyon: (unit_world_per_lyon * t) as f32 }
    }

    /// Whether the unit-world rect `[min_x, min_y, max_x, max_y]` (taken
    /// in the world copy nearest the grid) overlaps the grid: the mask
    /// pass's cheap reject for a tile that cannot write a texel.
    #[must_use]
    pub fn touches(&self, rect: [f64; 4]) -> bool {
        let t = self.texels_per_unit_world();
        let shift = self.world_copy_shift(0.5 * (rect[0] + rect[2]));
        let (x0, x1) = ((rect[0] + shift) * t, (rect[2] + shift) * t);
        let (y0, y1) = (rect[1] * t, rect[3] * t);
        let (gx0, gy0) = (self.origin[0] as f64, self.origin[1] as f64);
        let (gx1, gy1) = (gx0 + f64::from(self.size[0]), gy0 + f64::from(self.size[1]));
        x0 < gx1 && x1 > gx0 && y0 < gy1 && y1 > gy0
    }

    /// A tile's unit-world origin in this grid's texels (fractional, in
    /// f64), the tile taken in the world copy nearest the grid: the one
    /// figure both the mask pass (`TilePipeline::encode_dot_mask`) and a
    /// dot tile's lookup ([`Self::for_tile`]) are built on.
    #[must_use]
    pub(crate) fn tile_origin_texels(&self, tile_origin_unit_world: (f64, f64)) -> [f64; 2] {
        let t = self.texels_per_unit_world();
        let x = tile_origin_unit_world.0 + self.world_copy_shift(tile_origin_unit_world.0);
        [x * t - self.origin[0] as f64, tile_origin_unit_world.1 * t - self.origin[1] as f64]
    }

    /// The texel under a unit-world point, or None outside the grid.
    ///
    /// Outside the Mercator square (`y` outside `[0, 1)`) there is no mask
    /// and a candidate there stays put.
    #[must_use]
    pub fn texel_of(&self, x: f64, y: f64) -> Option<[u32; 2]> {
        if !(0.0..1.0).contains(&y) || !x.is_finite() {
            return None;
        }
        let t = self.texels_per_unit_world();
        let x = x + self.world_copy_shift(x);
        let tx = (x * t).floor() as i64 - self.origin[0];
        let ty = (y * t).floor() as i64 - self.origin[1];
        let inside = |v: i64, n: u32| (0..i64::from(n)).contains(&v);
        (inside(tx, self.size[0]) && inside(ty, self.size[1])).then_some([tx as u32, ty as u32])
    }
}

/// A dot tile's lookup into a [`MaskGrid`] ([`MaskGrid::for_tile`]): the
/// tile origin's whole-texel offset from the mask origin, its sub-texel
/// fraction, and the texels per tile lyon unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaskTileLookup {
    pub int_offset: [i32; 2],
    pub frac: [f32; 2],
    pub texels_per_lyon: f32,
}

impl MaskTileLookup {
    /// The mask texel of a candidate at tile lyon `p`, in f32 exactly as
    /// the shader computes it (unfused multiply-add; a GPU that fuses it
    /// still never involves the mask origin in the rounding).
    #[must_use]
    pub fn texel(&self, p: [f32; 2]) -> [i32; 2] {
        let axis = |k: usize| (p[k] * self.texels_per_lyon + self.frac[k]).floor() as i32 + self.int_offset[k];
        [axis(0), axis(1)]
    }
}

/// Ground under the viewport where a data cell is >= RELOCATION_MIN_CELL_PX on screen:
/// the flat cover's box, cut at the first screen row (from the bottom) whose ground
/// spacing makes a cell smaller than that. Unit world.
///
/// The box is `cover_plan::flat_cover`'s tiles at the camera's integer
/// zoom, each taken in the world copy nearest the camera (so a view across
/// the antimeridian gives one box with `x` past 1, not the whole world).
/// A screen row is at one view depth under any bearing, so a cell's size
/// is measured at the viewport's centre column; the ground below the cut
/// row is the box of its four corners' ground points. `None` when even the
/// bottom row's cells are too small (a wide view), or when the camera sees
/// no ground.
///
/// `None` wherever `render::dots::relocation_camera` says the camera does
/// not relocate: below `RELOCATION_MIN_ZOOM` (z15), above
/// `RELOCATION_MAX_PITCH_DEG` (30 deg) (Task 9 rulings), and on any frame
/// whose globe `transition` is not exactly 0 -- relocation is off on the
/// globe (ruling), and the flat camera's ground is not what a globe frame
/// shows, so a caller must not build a mask from it.
#[must_use]
pub fn relocation_region(camera: &Camera, transition: f32, cell_width_unit_world: f64) -> Option<[f64; 4]> {
    let (w, h) = (camera.viewport_width_px, camera.viewport_height_px);
    if !crate::render::dots::relocation_camera(camera.zoom, camera.pitch, transition) {
        return None;
    }
    if !(w > 0.0 && h > 0.0 && cell_width_unit_world > 0.0) {
        return None;
    }
    let world = f64::from(crate::render::WORLD_SIZE);
    // A cell's on-screen width at screen row `y`, from the ground a pixel
    // step along the row spans.
    let cell_px = |y: f32| -> Option<f64> {
        let a = crate::render::unproject_to_ground_f64(camera, w * 0.5, y)?;
        let b = crate::render::unproject_to_ground_f64(camera, w * 0.5 + 1.0, y)?;
        let unit_world_per_px = (b - a).truncate().length() / world;
        (unit_world_per_px > 0.0).then(|| cell_width_unit_world / unit_world_per_px)
    };
    if cell_px(h).is_none_or(|px| px < f64::from(RELOCATION_MIN_CELL_PX)) {
        return None;
    }
    let zoom = camera.zoom.floor().clamp(0.0, 30.0) as u8;
    let cover = crate::render::cover_plan::flat_cover(camera, zoom);
    let centre_x = camera.world_center_f64().x / world;
    let mut cover_box = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for tile in &cover.tiles {
        let n = f64::from(1u32 << tile.z);
        let (x0, y0) = (f64::from(tile.x) / n, f64::from(tile.y) / n);
        let x0 = x0 + (centre_x - (x0 + 0.5 / n)).round();
        cover_box = [cover_box[0].min(x0), cover_box[1].min(y0), cover_box[2].max(x0 + 1.0 / n), cover_box[3].max(y0 + 1.0 / n)];
    }
    if !(cover_box[0] < cover_box[2] && cover_box[1] < cover_box[3]) {
        return None;
    }
    if cell_px(0.0).is_some_and(|px| px >= f64::from(RELOCATION_MIN_CELL_PX)) {
        return Some(cover_box);
    }
    // Cells shrink monotonically up the screen: bisect for the row where
    // they reach the minimum.
    let (mut lo, mut hi) = (0.0f32, h);
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if cell_px(mid).is_some_and(|px| px >= f64::from(RELOCATION_MIN_CELL_PX)) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let cut = hi;
    let mut ground = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for (sx, sy) in [(0.0, h), (w, h), (0.0, cut), (w, cut)] {
        let p = crate::render::unproject_to_ground_f64(camera, sx, sy)? / world;
        ground = [ground[0].min(p.x), ground[1].min(p.y), ground[2].max(p.x), ground[3].max(p.y)];
    }
    let region = [
        cover_box[0].max(ground[0]),
        cover_box[1].max(ground[1]),
        cover_box[2].min(ground[2]),
        cover_box[3].min(ground[3]),
    ];
    (region[0] < region[2] && region[1] < region[3]).then_some(region)
}

/// A data cell's height in unit world at `region`'s poleward edge, one
/// cell further out: `cell_width` times the Mercator stretch
/// `cosh(pi (1 - 2 y))` there -- the north/south margin that holds every
/// cell touching `region` (`MaskGrid::plan`, [`mask_ring_tiles`]).
#[must_use]
pub fn cell_margin_y(region: [f64; 4], cell_width: f64) -> f64 {
    let stretch = |y: f64| (std::f64::consts::PI * (1.0 - 2.0 * y.clamp(0.0, 1.0))).cosh();
    let at_edge = stretch(region[1]).max(stretch(region[3]));
    let (y0, y1) = (region[1] - cell_width * at_edge, region[3] + cell_width * at_edge);
    cell_width * stretch(y0).max(stretch(y1)).max(at_edge)
}

/// The largest extent (unit world, x and y) the mask's grown region can
/// have for `camera` at any position: the view's ground, plus the cover
/// tile it may reach past on each side (the region is the cover's tile
/// box), plus the one-cell margin each way. What `MaskGrid::plan_after`
/// picks the level by, so a pan never changes it. `None` without a region.
#[must_use]
pub fn mask_extent_bound(camera: &Camera, transition: f32, cell_width_unit_world: f64) -> Option<[f64; 2]> {
    let region = relocation_region(camera, transition, cell_width_unit_world)?;
    let world = f64::from(crate::render::WORLD_SIZE);
    let (w, h) = (camera.viewport_width_px, camera.viewport_height_px);
    let tile = 1.0 / 2f64.powi(camera.zoom.floor().clamp(0.0, 30.0) as i32);
    let corners: Option<Vec<_>> =
        [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].iter().map(|&(sx, sy)| crate::render::unproject_to_ground_f64(camera, sx, sy)).collect();
    let (ew, eh) = corners.map_or((region[2] - region[0], region[3] - region[1]), |c| {
        let xs = c.iter().map(|p| p.x / world);
        let ys = c.iter().map(|p| p.y / world);
        let ew = xs.clone().fold(f64::NEG_INFINITY, f64::max) - xs.fold(f64::INFINITY, f64::min);
        let eh = ys.clone().fold(f64::NEG_INFINITY, f64::max) - ys.fold(f64::INFINITY, f64::min);
        ((ew + 2.0 * tile).min(region[2] - region[0] + tile), (eh + 2.0 * tile).min(region[3] - region[1] + tile))
    });
    let my = cell_margin_y(region, cell_width_unit_world);
    Some([ew + 2.0 * cell_width_unit_world, eh + 2.0 * my])
}

/// Below this zoom the mask draws the drawn cover alone (DP-5 ruling).
pub const MASK_RING_MIN_ZOOM: f64 = crate::render::dots::RELOCATION_MIN_ZOOM;
/// The most device bytes of ring tiles the mask pass draws, nearest the
/// view first -- the ring's only cap (Task 9 ruling: a tile count cut the
/// z16+ rings short, 36 tiles at Nizhny z16.5).
pub const MASK_RING_MAX_BYTES: usize = 32 << 20;

/// The basemap tiles the mask needs past the drawn `cover` (DP-5 ruling):
/// every tile at the cover's finest zoom that touches the view's ground
/// (clipped to the relocation region) grown by one data cell -- its width
/// east and west, its height ([`cell_margin_y`]) north and south -- and
/// lies under no cover tile. They are fetched and rasterised into the mask,
/// never drawn: a cell on screen reaches at most one cell past the view,
/// so with them every texel its candidates can read has a tile behind it,
/// and the tile columns a steady pan brings in land past every on-screen
/// cell. Empty below [`MASK_RING_MIN_ZOOM`], on a globe frame, or when the
/// frame has no relocation region. Nearest the region first (the window
/// stops at [`MASK_RING_MAX_BYTES`]). Whether there is a ring at all is
/// `relocation_region`'s answer alone (`render::dots::relocation_camera`:
/// z15 or closer, pitched at most 30 deg), so a camera that relocates
/// always gets its ring (Task 9 review I1: a second, slack-free pitch test
/// here left exactly 30 deg relocating with no ring).
#[must_use]
pub fn mask_ring_tiles(
    camera: &Camera,
    transition: f32,
    cell_width_unit_world: f64,
    cover: &[crate::geo::TileId],
) -> Vec<crate::geo::TileId> {
    let (Some(region), Some(z)) = (relocation_region(camera, transition, cell_width_unit_world), cover.iter().map(|t| t.z).max())
    else {
        return Vec::new();
    };
    let n = 1i64 << z;
    let scale = n as f64;
    let (m, my) = (cell_width_unit_world, cell_margin_y(region, cell_width_unit_world));
    // What an on-screen cell's candidates reach: the VIEW's ground (not
    // the cover box around it, up to a tile larger each way), clipped to
    // the region, grown by one cell.
    let world = f64::from(crate::render::WORLD_SIZE);
    let (w, h) = (camera.viewport_width_px, camera.viewport_height_px);
    let corners: Option<Vec<_>> =
        [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].iter().map(|&(sx, sy)| crate::render::unproject_to_ground_f64(camera, sx, sy)).collect();
    let view = corners.map_or(region, |c| {
        let xs = c.iter().map(|p| p.x / world);
        let ys = c.iter().map(|p| p.y / world);
        let (vx0, vx1) = (xs.clone().fold(f64::INFINITY, f64::min), xs.fold(f64::NEG_INFINITY, f64::max));
        let (vy0, vy1) = (ys.clone().fold(f64::INFINITY, f64::min), ys.fold(f64::NEG_INFINITY, f64::max));
        [vx0.max(region[0]), vy0.max(region[1]), vx1.min(region[2]), vy1.min(region[3])]
    });
    let x0 = ((view[0] - m) * scale).floor() as i64;
    let x1 = ((view[2] + m) * scale).ceil() as i64;
    let y0 = (((view[1] - my) * scale).floor() as i64).max(0);
    let y1 = (((view[3] + my) * scale).ceil() as i64).min(n);
    let under_cover = |x: u32, y: u32| cover.iter().any(|c| c.z <= z && (x >> (z - c.z), y >> (z - c.z)) == (c.x, c.y));
    let mut ring = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            let (x, y) = (x.rem_euclid(n) as u32, y as u32);
            let tile = crate::geo::TileId { z, x, y };
            if !under_cover(x, y) && !ring.contains(&tile) {
                ring.push(tile);
            }
        }
    }
    let centre = ((region[0] + region[2]) / 2.0, (region[1] + region[3]) / 2.0);
    let side = 1.0 / scale;
    let tile_centre = |t: &crate::geo::TileId| {
        // The copy of the tile nearest the region (across the antimeridian).
        let x = (f64::from(t.x) + 0.5) * side;
        (x + (centre.0 - x).round(), (f64::from(t.y) + 0.5) * side)
    };
    // Nearest the region first, measured in cells (a cell is `my / m`
    // times taller than wide): the cap cuts the outermost ring evenly.
    let dist = |t: &crate::geo::TileId| {
        let (x, y) = tile_centre(t);
        let dx = ((region[0] - (x + side / 2.0)).max(0.0)).max((x - side / 2.0) - region[2]).max(0.0);
        let dy = ((region[1] - (y + side / 2.0)).max(0.0)).max((y - side / 2.0) - region[3]).max(0.0);
        (dx / m).max(dy / my)
    };
    ring.sort_by(|a, b| dist(a).total_cmp(&dist(b)).then((a.y, a.x).cmp(&(b.y, b.x))));
    ring
}

/// The prefix of `ring` (nearest first) within `cap` bytes: a resident
/// tile at its device bytes (`bytes_of`), one not yet resident at
/// `estimate`. What a map requests, uploads and draws into the mask
/// (Task 9 review I2: the cap bounds the ring's fetches and uploads, not
/// only the mask pass).
#[must_use]
pub fn cap_ring(
    ring: &[crate::geo::TileId],
    bytes_of: impl Fn(crate::geo::TileId) -> Option<usize>,
    estimate: usize,
    cap: usize,
) -> Vec<crate::geo::TileId> {
    let mut held = 0usize;
    let mut out = Vec::new();
    for &tile in ring {
        let bytes = bytes_of(tile).unwrap_or(estimate);
        if held + bytes > cap {
            break;
        }
        held += bytes;
        out.push(tile);
    }
    out
}

/// What makes a mask stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskKey {
    pub grid: MaskGrid,
    pub tiles: Vec<(crate::geo::TileId, u64)>,
}

/// Mask re-keys (re-renders) a map has run, all maps together: DP-3's
/// re-key rate (`world_dots` prints it). Probe builds only.
#[cfg(feature = "probe")]
pub static MASK_REKEYS_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The mask pass's CPU wall time, encode through submit, summed and at its
/// largest, in nanoseconds (probe builds; `world_dots` prints it). Not the
/// GPU's execution time: the pass is its own submit, off the frame's
/// timestamp query.
#[cfg(feature = "probe")]
pub static MASK_PASS_NS_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "probe")]
pub static MASK_PASS_NS_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The texture usages a mask needs beyond `RENDER_ATTACHMENT |
/// TEXTURE_BINDING`: the copy-source usage [`DotMaskGpu::read_back`] needs,
/// in test and probe builds only (DP-8).
#[cfg(any(test, feature = "probe"))]
const READ_BACK_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::COPY_SRC;
#[cfg(not(any(test, feature = "probe")))]
const READ_BACK_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::empty();

/// The mask texture: `R8Uint`, one rank per texel, sized to a
/// [`MaskGrid`], the key of what was last drawn into it, and its generation
/// (the texture's identity).
pub struct DotMaskGpu {
    /// Read only by `read_back`; the view alone would keep it alive.
    #[cfg_attr(not(any(test, feature = "probe")), allow(dead_code))]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: [u32; 2],
    key: Option<MaskKey>,
    /// The texture's identity: bumped when a new texture replaces the
    /// last (a resize), never by a render into the same one. A dot bind
    /// group remembers the generation it bound and rebinds only when it is
    /// stale -- a re-key that keeps the size rewrites the draws' uniforms
    /// (`write_dot_params`), not their groups (final review m2).
    generation: u64,
}

impl DotMaskGpu {
    /// A fresh `size` texture that continues `generation`: its own is
    /// `generation + 1`, so a group bound to the texture it replaces reads
    /// as stale. Keyless until drawn.
    pub(crate) fn new(ctx: &crate::render::gpu::GpuContext, size: [u32; 2], generation: u64) -> Self {
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dot-mask"),
            size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Uint,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | READ_BACK_USAGE,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { texture, view, size, key: None, generation: generation + 1 }
    }

    /// A mask of `grid`'s size holding `texels` (row-major, `grid.size[0]`
    /// per row), keyed to `grid` with no tiles: the synthetic masks the
    /// relocation tests and `capture::CaptureDots::synthetic_mask` draw
    /// against. `COPY_DST` only (DP-8 allows it): written, never read back.
    pub(crate) fn from_texels(ctx: &crate::render::gpu::GpuContext, grid: MaskGrid, texels: &[u8]) -> Self {
        let [w, h] = grid.size;
        assert_eq!(texels.len(), w as usize * h as usize, "one byte per texel");
        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dot-mask-test"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            texels,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: Some(h) },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { texture, view, size: grid.size, key: Some(MaskKey { grid, tiles: Vec::new() }), generation: 1 }
    }

    #[must_use]
    pub fn key(&self) -> Option<&MaskKey> {
        self.key.as_ref()
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The texture's size in texels.
    #[must_use]
    pub(crate) fn size(&self) -> [u32; 2] {
        self.size
    }

    /// Records that `key` was just drawn into the texture. The generation
    /// holds: the groups bound to this texture still read it.
    pub(crate) fn drawn(&mut self, key: MaskKey) {
        self.key = Some(key);
    }

    /// The texels, row-major, `size[0]` bytes per row. Blocks on the device.
    /// Test and probe builds only: the mask is never read back otherwise (DP-8).
    #[cfg(any(test, feature = "probe"))]
    pub fn read_back(&self, ctx: &crate::render::GpuContext) -> Vec<u8> {
        let [w, h] = self.size;
        let bytes_per_row = w.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = crate::render::alloc::TrackedBuffer::new(
            &ctx.device,
            &wgpu::BufferDescriptor {
                label: Some("dot-mask-read-back"),
                size: u64::from(bytes_per_row) * u64::from(h),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            },
        );
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dot-mask-read-back") });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("the device polls");
        rx.recv().expect("the map callback ran").expect("the read-back maps");
        let data = slice.get_mapped_range().expect("mapped after the poll");
        data.chunks(bytes_per_row as usize).flat_map(|row| &row[..w as usize]).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL_30SS: f64 = 1.0 / 43_200.0;

    fn camera(zoom: f64, pitch_deg: f32) -> Camera {
        Camera {
            center_lon: 139.7,
            center_lat: 35.68,
            zoom,
            pitch: pitch_deg.to_radians(),
            bearing: 0.0,
            viewport_width_px: 1280.0,
            viewport_height_px: 800.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    fn cover_box(camera: &Camera) -> [f64; 4] {
        let zoom = camera.zoom.floor() as u8;
        let cover = crate::render::cover_plan::flat_cover(camera, zoom);
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for t in &cover.tiles {
            let n = f64::from(1u32 << t.z);
            let (x, y) = (f64::from(t.x) / n, f64::from(t.y) / n);
            b = [b[0].min(x), b[1].min(y), b[2].max(x + 1.0 / n), b[3].max(y + 1.0 / n)];
        }
        b
    }

    /// DP-5 ruling: at z >= 15 the mask also rasterises the basemap tiles
    /// past the drawn cover that its grid reaches (the region grown by one
    /// cell), so every texel an on-screen cell's candidate can read has a
    /// tile behind it. None of them is a cover tile; together with the
    /// cover they tile the grown region; below z15 there are none.
    #[test]
    fn the_mask_ring_covers_the_grown_region_past_the_cover() {
        for (zoom, pitch) in [(15.0, 0.0), (15.4, 0.0), (16.5, 0.0), (15.0, 25.0), (15.0, 30.0)] {
            let cam = camera(zoom, pitch);
            let z = zoom.floor() as u8;
            let cover = crate::render::cover_plan::flat_cover(&cam, z).tiles;
            let ring = mask_ring_tiles(&cam, 0.0, CELL_30SS, &cover);
            assert!(!ring.is_empty(), "z{zoom} p{pitch}: a ring");
            let top = cover.iter().map(|t| t.z).max().unwrap();
            assert!(ring.iter().all(|t| t.z == top && !cover.contains(t)), "z{zoom}: ring tiles at the cover's zoom, off the cover");
            let region = relocation_region(&cam, 0.0, CELL_30SS).unwrap();
            let n = f64::from(1u32 << top);
            let covered = |x: f64, y: f64| {
                let (tx, ty) = ((x * n).floor() as u32, (y * n).floor() as u32);
                ring.iter().any(|t| (t.x, t.y) == (tx, ty))
                    || cover.iter().any(|c| (tx >> (top - c.z), ty >> (top - c.z)) == (c.x, c.y))
            };
            let my = cell_margin_y(region, CELL_30SS);
            assert!(my > CELL_30SS, "a cell at 35.7 N is taller than wide");
            // Every point of the view grown by one cell -- what a cell on
            // screen reaches -- has a tile behind it.
            let world = f64::from(crate::render::WORLD_SIZE);
            let (w, h) = (cam.viewport_width_px, cam.viewport_height_px);
            let corners: Vec<_> = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)]
                .iter()
                .map(|&(sx, sy)| crate::render::unproject_to_ground_f64(&cam, sx, sy).unwrap() / world)
                .collect();
            let (vx0, vx1) = (corners.iter().map(|p| p.x).fold(f64::INFINITY, f64::min), corners.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max));
            let (vy0, vy1) = (corners.iter().map(|p| p.y).fold(f64::INFINITY, f64::min), corners.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max));
            // The view as the ring sees it: clipped to the region (the
            // pitched far ground past the 2-px cut is not relocated).
            let [x0, y0, x1, y1] =
                [vx0.max(region[0]) - CELL_30SS, vy0.max(region[1]) - my, vx1.min(region[2]) + CELL_30SS, vy1.min(region[3]) + my];
            for i in 0..=40 {
                for j in 0..=40 {
                    let (x, y) = (x0 + (x1 - x0) * f64::from(i) / 40.0 * 0.9999, y0 + (y1 - y0) * f64::from(j) / 40.0 * 0.9999);
                    assert!(covered(x, y), "z{zoom} p{pitch}: ({x}, {y}) has no tile behind it");
                }
            }
        }
        // Above 30 deg there is no ring (Task 9 ruling: its RSS).
        let tilted = camera(15.0, 45.0);
        let tilted_cover = crate::render::cover_plan::flat_cover(&tilted, 15).tiles;
        assert!(mask_ring_tiles(&tilted, 0.0, CELL_30SS, &tilted_cover).is_empty(), "no ring above 30 deg");
        let cam = camera(14.9, 0.0);
        let cover = crate::render::cover_plan::flat_cover(&cam, 14).tiles;
        assert!(mask_ring_tiles(&cam, 0.0, CELL_30SS, &cover).is_empty(), "no ring below z15");
        assert!(mask_ring_tiles(&camera(15.0, 0.0), 0.5, CELL_30SS, &cover).is_empty(), "no ring on the globe");
    }

    /// Task 9 ruling: the mask drops a zoom level only when a side would
    /// pass the 4,096-texel cap, and climbs back only when the finer side
    /// fits in [`MASK_CLIMB_SIDE`] -- a region whose finer side sits between
    /// the two keeps whichever level the previous mask had.
    #[test]
    fn the_mask_zoom_has_hysteresis_between_the_climb_side_and_the_cap() {
        let t16 = 2f64.powi(25);
        // A region whose zm-16 side is `texels` texels tall (plus the snap).
        let region = |texels: f64| [0.5, 0.3, 0.5 + 1000.0 / t16, 0.3 + texels / t16];
        let over = MaskGrid::plan(16.5, region(3900.0), 0.0, 8192).unwrap();
        assert_eq!(over.zm, 15, "a 4,096+ side drops a level");
        let fits = MaskGrid::plan(16.5, region(3000.0), 0.0, 8192).unwrap();
        assert_eq!(fits.zm, 16);
        // In the band (finer side 3,840 with the snap): no previous -> the
        // cap decides; after a drop it stays down; after 16 it stays up.
        let band = region(3500.0 + 100.0);
        assert_eq!(MaskGrid::plan(16.5, band, 0.0, 8192).unwrap().zm, 16);
        assert_eq!(MaskGrid::plan_after(16.5, band, 0.0, 8192, Some(&over), None).unwrap().zm, 15, "stays dropped in the band");
        assert_eq!(MaskGrid::plan_after(16.5, band, 0.0, 8192, Some(&fits), None).unwrap().zm, 16, "stays up in the band");
        // Out of the band either way, the previous level does not matter.
        assert_eq!(MaskGrid::plan_after(16.5, region(3900.0), 0.0, 8192, Some(&fits), None).unwrap().zm, 15);
        assert_eq!(MaskGrid::plan_after(16.5, region(3000.0), 0.0, 8192, Some(&over), None).unwrap().zm, 16, "climbs back when it fits the climb side");
        // A bound picks the level for every position of the camera: a
        // region that fits alone still takes the bound's level, and the
        // texture keeps the region's own size.
        let bounded = MaskGrid::plan_after(16.5, region(3000.0), 0.0, 8192, None, Some([1000.0 / t16, 3900.0 / t16])).unwrap();
        assert_eq!(bounded.zm, 15);
        assert_eq!(bounded.size, MaskGrid::plan(15.5, region(3000.0), 0.0, 8192).unwrap().size, "sized to the region at zm 15");
    }

    /// Task 9 review I2: the ring is cut at the byte cap, nearest first,
    /// counting resident tiles at their bytes and the rest at the estimate.
    #[test]
    fn the_ring_is_cut_at_the_byte_cap() {
        let t = |x: u32| crate::geo::TileId { z: 15, x, y: 0 };
        let ring: Vec<_> = (0..10).map(t).collect();
        // Tiles 0 and 1 resident at 10 and 30 bytes; the rest estimated at 20.
        let bytes = |tile: crate::geo::TileId| match tile.x {
            0 => Some(10),
            1 => Some(30),
            _ => None,
        };
        assert_eq!(cap_ring(&ring, bytes, 20, 99), vec![t(0), t(1), t(2), t(3)], "10 + 30 + 20 + 20 = 80; a fifth tile passes 99");
        assert_eq!(cap_ring(&ring, bytes, 20, 100).len(), 5, "the cap is inclusive");
        assert_eq!(cap_ring(&ring, bytes, 20, 9), Vec::new());
        assert_eq!(cap_ring(&ring, bytes, 20, 10_000), ring);
    }

    #[test]
    fn the_texel_size_is_one_logical_pixel_at_integer_zoom_and_never_finer_than_z16() {
        let g = MaskGrid::plan(14.7, [0.5, 0.5, 0.5001, 0.5001], 0.0, 4096).unwrap();
        assert_eq!(g.zm, 14);
        assert_eq!(g.texels_per_unit_world(), 2f64.powi(23));
        assert_eq!(MaskGrid::plan(19.0, [0.5, 0.5, 0.50001, 0.50001], 0.0, 4096).unwrap().zm, 16);
    }

    #[test]
    fn the_origin_snaps_to_256_texels_and_a_small_pan_keeps_the_grid() {
        let t = 2f64.powi(23);
        // 10 texels into a 256-texel block, 1000.5 texels wide.
        let x = (4_194_304.0 + 10.0) / t;
        let region = |dx: f64| [x + dx / t, 0.4, x + (dx + 1000.5) / t, 0.4 + 700.25 / t];
        let a = MaskGrid::plan(14.2, region(0.0), 0.0, 4096).unwrap();
        let b = MaskGrid::plan(14.2, region(100.0), 0.0, 4096).unwrap();
        let c = MaskGrid::plan(14.2, region(300.0), 0.0, 4096).unwrap();
        assert_eq!(a.origin[0].rem_euclid(MASK_SNAP_TEXELS), 0);
        assert_eq!(a.origin[1].rem_euclid(MASK_SNAP_TEXELS), 0);
        assert_eq!(a, b, "a 100-texel pan inside one block keeps the grid");
        assert_eq!(c.origin[0] - a.origin[0], 256);
        assert_eq!((c.origin[1], c.size, c.zm), (a.origin[1], a.size, a.zm));
        // Every texel of the region is inside the grid.
        for g in [a, b, c] {
            assert!(g.size[0] >= 1001 && g.size[1] >= 701);
        }
        assert!(c.texel_of(x + 1300.0 / t, 0.4 + 700.0 / t).is_some());
    }

    #[test]
    fn a_region_over_the_texture_cap_drops_one_zoom_deterministically() {
        let t = 2f64.powi(23);
        let region = [0.25, 0.25, 0.25 + 5000.0 / t, 0.25 + 100.0 / t];
        let g = MaskGrid::plan(14.0, region, 0.0, 2048).unwrap();
        assert_eq!(g.zm, 12, "5000 texels at z14 -> 2500 at z13 (over 2048 with the snap slack) -> 1250 at z12");
        assert!(g.size[0] <= 2048 && g.size[1] <= 2048);
        for _ in 0..3 {
            assert_eq!(MaskGrid::plan(14.0, region, 0.0, 2048), Some(g));
        }
        // The cap is also MASK_TEXTURE_CAP when the device allows more.
        let wide = [0.25, 0.25, 0.25 + 5000.0 / t, 0.25 + 100.0 / t];
        assert_eq!(MaskGrid::plan(14.0, wide, 0.0, 16384).unwrap().zm, 13);
    }

    #[test]
    fn the_mask_origin_is_the_same_texel_for_every_world_copy() {
        let g = MaskGrid::plan(14.0, [0.999, 0.4, 1.001, 0.4001], 0.0, 4096).unwrap();
        let upl = 1.0 / (crate::overlay::geom::OVERLAY_UNITS_PER_WORLD * f64::from(crate::render::WORLD_SIZE));
        // A dot tile whose origin is just east of the antimeridian, and its
        // copy one world east; a candidate 1234.5 lyon (x) and 777 lyon (y)
        // into it.
        let candidate = [123_456.5f32, 7_777.0f32];
        let texel = |tile_x: f64| {
            let t = g.for_tile((tile_x, 0.4), upl).texel(candidate);
            [i64::from(t[0]), i64::from(t[1])]
        };
        let (a, b) = (texel(0.0), texel(1.0));
        assert_eq!(a, b);
        // And it is the texel `texel_of` gives for the same unit-world point.
        let (px, py) = (f64::from(candidate[0]) * upl, 0.4 + f64::from(candidate[1]) * upl);
        let expect = g.texel_of(px, py).expect("inside the grid");
        assert_eq!(g.texel_of(1.0 + px, py), Some(expect));
        assert_eq!([i64::from(expect[0]), i64::from(expect[1])], a);
    }

    #[test]
    fn a_candidate_beyond_the_mercator_limit_stays_put() {
        let g = MaskGrid::plan(3.0, [0.4, -0.2, 0.6, 0.1], 0.0, 4096).unwrap();
        assert_eq!(g.origin[1], 0, "y is clamped to the Mercator square");
        assert_eq!(g.texel_of(0.5, -0.01), None);
        assert_eq!(g.texel_of(0.5, 1.0), None);
        assert!(g.texel_of(0.5, 0.01).is_some());
        assert_eq!(MaskGrid::plan(3.0, [0.4, -0.2, 0.6, -0.1], 0.0, 4096), None, "wholly off the square");
    }

    #[test]
    fn the_region_stops_where_cells_fall_below_two_pixels() {
        let flat = camera(15.0, 0.0);
        assert_eq!(relocation_region(&flat, 0.0, CELL_30SS), Some(cover_box(&flat)));
        // Pitched (30 deg, the most that relocates), with tiny cells: 30"
        // cells stay above 2 px past the cover's own far edge, so they
        // would not show the cut.
        let pitched = camera(15.0, 30.0);
        let cover = cover_box(&pitched);
        let coarse = relocation_region(&pitched, 0.0, CELL_30SS).expect("near ground has big cells");
        assert_eq!(coarse[1], cover[1], "30\" cells reach the cover's far edge at z15");
        let region = relocation_region(&pitched, 0.0, CELL_30SS / 150.0).expect("near ground has big cells");
        // Bearing 0 looks north: the far edge is the box's min y.
        assert!(region[1] > cover[1], "cut {region:?} vs cover {cover:?}");
        assert!(region[3] <= cover[3] && region[0] >= cover[0] && region[2] <= cover[2]);
        assert!(region[1] > coarse[1], "smaller cells stop nearer");
        let km = 1.0 / 40_075.0;
        assert_eq!(relocation_region(&camera(7.0, 0.0), 0.0, km), None, "1 km cells at z7 are 1.6 px");
        // Relocation starts at z15 (Task 9 ruling), whatever the cells.
        assert_eq!(relocation_region(&camera(14.99, 0.0), 0.0, CELL_30SS), None);
        // ... and above 30 deg of pitch.
        assert!(relocation_region(&camera(15.0, 30.0), 0.0, CELL_30SS).is_some());
        assert_eq!(relocation_region(&camera(15.0, 30.5), 0.0, CELL_30SS), None);
    }

    /// Relocation is off on the globe: any transition above 0 has no region.
    #[test]
    fn a_globe_frame_has_no_relocation_region() {
        let flat = camera(15.0, 0.0);
        assert!(relocation_region(&flat, 0.0, CELL_30SS).is_some());
        for transition in [1e-6, 0.5, 1.0, f32::NAN] {
            assert_eq!(relocation_region(&flat, transition, CELL_30SS), None, "transition {transition}");
        }
    }

    /// DP-5's premise, bit-exact: a candidate's WORLD texel (mask origin +
    /// the lookup) is the same for every snapped mask origin, at every
    /// mask zoom / tile zoom pairing, for random tile-local points.
    #[test]
    fn a_candidates_world_texel_is_identical_across_1000_snapped_origins() {
        let lyon_per_unit_world = crate::overlay::dotgrid::blocks::OVERLAY_LYON_PER_UNIT_WORLD;
        let upl = 1.0 / lyon_per_unit_world;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut unit = || {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for (zm, tz) in [(14u8, 14u8), (15, 14), (16, 14), (16, 15), (16, 16)] {
            let n = f64::from(1u32 << tz);
            let tile = (f64::from((0.53 * n) as u32) / n, f64::from((0.41 * n) as u32) / n);
            let tile_lyon = lyon_per_unit_world / n;
            let points: Vec<[f32; 2]> =
                (0..200).map(|_| [(unit() * tile_lyon) as f32, (unit() * tile_lyon) as f32]).collect();
            let t = 2f64.powi(i32::from(zm) + 9);
            let base = [((tile.0 * t) as i64 / 256 - 2000) * 256, ((tile.1 * t) as i64 / 256 - 2000) * 256];
            let world_texels = |k: i64| {
                let grid = MaskGrid { zm, origin: [base[0] + 256 * k, base[1] + 256 * (k / 3)], size: [4096, 4096] };
                let lookup = grid.for_tile(tile, upl);
                if zm + 9 >= tz {
                    assert_eq!(lookup.frac, [0.0, 0.0], "zm {zm} tz {tz}: the tile corner is on the lattice");
                }
                points
                    .iter()
                    .map(|&p| {
                        let texel = lookup.texel(p);
                        [grid.origin[0] + i64::from(texel[0]), grid.origin[1] + i64::from(texel[1])]
                    })
                    .collect::<Vec<_>>()
            };
            let first = world_texels(0);
            for k in 1..1000 {
                assert_eq!(world_texels(k), first, "zm {zm} tz {tz}: origin step {k} moved a candidate");
            }
        }
    }

    /// The texel mapping against an oracle that shares none of
    /// `for_tile`'s arithmetic: the candidate's unit-world point in f64,
    /// `floor(x * 2^(zm+9)) - origin`. At every mask zoom / tile zoom
    /// pairing a dot tile can meet, every candidate farther than 0.05
    /// texel from a texel edge maps to the oracle's texel (the f32 lookup
    /// may only disagree on an edge), and `frac` is exactly 0.
    #[test]
    fn the_texel_mapping_matches_an_independent_f64_oracle() {
        let lyon_per_unit_world = crate::overlay::dotgrid::blocks::OVERLAY_LYON_PER_UNIT_WORLD;
        let upl = 1.0 / lyon_per_unit_world;
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut unit = || {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let (mut checked, mut edge) = (0u64, 0u64);
        for tz in [4u8, 6, 9] {
            for zm in [0u8, 3, 6, 10, 12, 14, 16] {
                let n = f64::from(1u32 << tz);
                let tile = (f64::from((0.713 * n) as u32) / n, f64::from((0.389 * n) as u32) / n);
                let t = 2f64.powi(i32::from(zm) + 9);
                // A grid at most half a world wide (as `plan` makes them)
                // whose origin sits (snapped) a little west and north of
                // the tile corner.
                let side = (t as u32 / 2).min(MASK_TEXTURE_CAP);
                let back = i64::from(side / 4).min(1000);
                let snap = |v: f64| ((v * t).floor() as i64 - back).div_euclid(MASK_SNAP_TEXELS) * MASK_SNAP_TEXELS;
                let grid = MaskGrid { zm, origin: [snap(tile.0), snap(tile.1)], size: [side, side] };
                let lookup = grid.for_tile(tile, upl);
                assert_eq!(lookup.frac, [0.0, 0.0], "zm {zm} tz {tz}: a z<=9 tile corner is on every mask lattice");
                // Candidates within 3000 texels (or half the grid) of the
                // tile corner, or the whole tile when it is smaller.
                let span = (lyon_per_unit_world / n).min(f64::from(side / 2).min(3000.0) / t * lyon_per_unit_world);
                for _ in 0..2_000 {
                    let p = [(unit() * span) as f32, (unit() * span) as f32];
                    let world = [(tile.0 + f64::from(p[0]) * upl) * t, (tile.1 + f64::from(p[1]) * upl) * t];
                    let oracle = [world[0].floor() as i64 - grid.origin[0], world[1].floor() as i64 - grid.origin[1]];
                    let near_edge = world.iter().any(|w| (w - w.round()).abs() < 0.05);
                    let got = lookup.texel(p);
                    checked += 1;
                    if near_edge {
                        edge += 1;
                        continue;
                    }
                    assert_eq!([i64::from(got[0]), i64::from(got[1])], oracle, "zm {zm} tz {tz} p {p:?}");
                    if (0..i64::from(side)).contains(&oracle[0]) && (0..i64::from(side)).contains(&oracle[1]) {
                        let (ux, uy) = (world[0] / t, world[1] / t);
                        assert_eq!(grid.texel_of(ux, uy).map(|v| [i64::from(v[0]), i64::from(v[1])]), Some(oracle));
                    }
                }
            }
        }
        assert!(edge * 5 < checked, "{edge} of {checked} near an edge: the check checked little");
    }

    /// A grid across the antimeridian touches rects on both sides of the
    /// seam, in either world copy, and one straddling it; not ones far off.
    #[test]
    fn a_grid_spanning_the_antimeridian_touches_both_sides_of_the_seam() {
        let g = MaskGrid::plan(14.0, [0.9999, 0.4, 1.0001, 0.4001], 0.0, 4096).unwrap();
        assert_eq!(g.zm, 14);
        let t = g.texels_per_unit_world();
        assert!((g.origin[0] as f64) < 0.9999 * t && (g.origin[0] as f64 + f64::from(g.size[0])) > 1.0001 * t, "{g:?}");
        let (y0, y1) = (0.40001, 0.40002);
        for (x0, x1) in [(0.99995, 0.99999), (0.00001, 0.00004), (1.00001, 1.00004), (-0.00004, -0.00001), (0.99998, 1.00002)] {
            assert!(g.touches([x0, y0, x1, y1]), "[{x0}, {x1}] touches");
        }
        for (x0, x1) in [(0.9, 0.91), (0.3, 0.31), (0.001, 0.002), (0.998, 0.999)] {
            assert!(!g.touches([x0, y0, x1, y1]), "[{x0}, {x1}] is off the grid");
        }
        assert!(!g.touches([0.99995, 0.5, 0.99999, 0.51]), "off in y");
    }

    /// The mask pass's cull: a rect overlapping the grid touches it, one
    /// beyond any edge does not, and a rect one world away touches as its
    /// copy does.
    #[test]
    fn a_rect_off_the_grid_does_not_touch_it() {
        let g = MaskGrid::plan(14.0, [0.5, 0.4, 0.5001, 0.4001], 0.0, 4096).unwrap();
        let inside = [0.50005, 0.40005, 0.5002, 0.4002];
        assert!(g.touches(inside));
        assert!(g.touches([inside[0] + 1.0, inside[1], inside[2] + 1.0, inside[3]]));
        assert!(!g.touches([0.6, 0.4, 0.61, 0.41]));
        assert!(!g.touches([0.5, 0.3, 0.5001, 0.31]));
    }
}
