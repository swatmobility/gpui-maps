//! GPU data for the world dot-density overlay: one `DotBlock` per emitted
//! block of same-population dots, bucketed by block size so the vertex
//! shader (`vs_dot`, `shader.wgsl`) can pull each block from a texture with a
//! fixed dot count per bucket instead of a variable-length loop.
//!
//! `overlay::dotgrid::blocks::build_blocks` is the producer: it
//! turns a decoded `GridTile`'s populated cells into these blocks. See
//! `docs/superpowers/specs/2026-09-26-world-dot-density-design.md` for the
//! design this implements.

/// One emitted block: up to `DOT_BUCKETS[..]` dots of the same cell, all
/// sharing that cell's population and origin. 16 bytes -- exactly one
/// `Rgba32Uint` texel of the tile's block texture (`shader.wgsl`'s
/// `dot_blocks`):
/// `origin` (the cell's SW corner, tile-local lyon units), `pop` (the
/// cell's population -- shared by every block a cell decomposes into, and
/// by every dot the shader hashes within this block), and `first_dot` (the
/// index, within the cell's own dot numbering, of this block's first dot --
/// `vs_dot` adds the pulled vertex's within-block dot offset to it).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct DotBlock {
    pub origin: [f32; 2],
    pub pop: f32,
    pub first_dot: u32,
}

/// Block sizes a cell's dot count is greedily decomposed into, largest
/// first. Powers of 4 (`4^0..4^3`): greedy decomposition against a run of
/// `4^k` sizes is exact for any count (a base-4 digit expansion), so no
/// cell's dots are ever over- or under-allocated a slot.
pub const DOT_BUCKETS: [u32; 4] = [1, 4, 16, 64];

/// **The native live-dot budget**: the largest measured that kept the
/// WORST of three runs inside MG-8's criteria with 0.3 ms of margin (flat
/// <= 7.7 ms, globe <= 11.7 ms) on the M4 Pro, headless, 1920x1080, from a
/// binary newer than its sources (`docs/measurements/
/// 2026-09-26-world-dot-density.md`, load average 4.3-4.6). At 2.8 M live
/// dots the worst runs were 7.57 ms flat (`vs_dot_flat`) and 11.45 ms
/// globe (`vs_dot`); 3.0 M reached 7.89 / 12.00 and 4 M (the spec's
/// estimate) 10.60 / 15.85. The globe binds: 30″ views are all below zoom
/// 11, where the transition is 1. The headless dot-pass bound only:
/// `DotStyle::budget_dots` defaults to the stricter of this and
/// [`NATIVE_WINDOW_DOT_BUDGET`] (MG-11), which is the window's.
///
/// **Re-set to 2.7 M at the dasymetric merge** by MG-8's own rule (the
/// largest budget whose quiet worst flat time passes, controller ruling on
/// the final review's I1b): with relocation (K = 8, `vs_dot_flat`) the
/// 2.8 M figure had drifted to 7.89-7.92 ms worst flat (K 0 7.80), over
/// 7.7. Quiet re-runs, 3 processes each, load 2.4-3.3, K 8 / checker30
/// (`docs/measurements/2026-09-27-dasymetric-dot-placement.md`, MG-8
/// rows): 2.7 M worst flat 7.57 / 7.57 / 7.58 ms (globe K 0, the path that
/// ships there, 9.35); 2.6 M 7.34 ms. The window's default (1.5 M) is
/// unaffected.
pub const NATIVE_DOT_BUDGET: u32 = 2_700_000;

/// **The native WINDOW's live-dot budget, and `DotStyle`'s default**:
/// MG-11 measured in `world_dots`' 1280x800 window (2x Retina, 120 Hz),
/// India pan and Dhaka zoom sweep, criterion frame p95 <= 16.7 ms and 0
/// frames over 33 ms (Task 8 fix round 2,
/// `docs/measurements/2026-09-26-world-dot-density.md`). 1.5 M held p95 in
/// all 12 runs over two sessions (pan 8.56-16.46, sweep 9.19-10.93 ms);
/// 1.8 M held it in the first session (pan 15.59-16.28) but missed in the
/// second, under a busier WindowServer (16.85-17.03); 2.0 M and 2.2 M
/// missed in both. Two of the 1.5 M runs had one frame over 33 ms each
/// (33.7, 37.4), not root-caused. Lower than [`NATIVE_DOT_BUDGET`] (MG-8:
/// headless 1920x1080, the dot pass alone) because the window has twice
/// the pixels and draws the basemap too; a map uses the stricter of the
/// two.
pub const NATIVE_WINDOW_DOT_BUDGET: u32 = 1_500_000;

/// **The browser's live-dot budget** (Task 16): MG-24's criterion is set
/// at 1.5 M dots -- WebGPU p95 <= 16.7 ms, `?gl=1` p95 <= 33 ms -- the
/// native window's figure, so the two shells draw the same dots at a
/// preset. `?dot_budget=` overrides it (`shell_fetch::web_dot_budget`),
/// which is how MG-24's 1.0 M WebGL2 fallback is run.
pub const WEB_DOT_BUDGET: u32 = 1_500_000;

/// **The phone's live-dot budget** (Task 17): 2 M dots, the plan's figure
/// for the iOS shell, NOT yet measured -- MG-25 (iPhone 15 Pro Max, India
/// z5 pan and Dhaka z10, fps >= 60 with p95 <= 16.7 ms, no debug assert)
/// is owed. Between the native window's measured 1.5 M
/// ([`NATIVE_WINDOW_DOT_BUDGET`]) and the headless 2.7 M
/// ([`NATIVE_DOT_BUDGET`]); if MG-25 misses, measure `vs_dot_flat` on the
/// phone before lowering it (the plan's rule).
pub const IOS_DOT_BUDGET: u32 = 2_000_000;

/// **The race dots' cap on any shell's budget** (race plan, MG-R10): the
/// categorical and triangle `vs_dot` path held MG-8's criterion at 2.7 M
/// (2,686,066 synthetic part dots, worst of 3: flat 6.65 ms <= 7.7, globe
/// 8.34 ms <= 11.7), so it needs no cut of its own and equals
/// [`NATIVE_DOT_BUDGET`]. `overlay::dotgrid::layer::race_budget_for`
/// applies it per shell.
pub const RACE_DOT_BUDGET: u32 = NATIVE_DOT_BUDGET;

static DOT_BLOCKS_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Blocks dropped, process-wide, because a tile held more than its block
/// texture can (`TilePipeline::create_dot_tile`). Zero at any budget this
/// crate ships: the texture holds 4 M blocks even on WebGL2.
#[must_use]
pub fn dot_blocks_dropped_total() -> u64 {
    DOT_BLOCKS_DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Counts `n` dropped blocks, and says so on stderr the first time.
pub(crate) fn note_dot_blocks_dropped(n: u64) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    DOT_BLOCKS_DROPPED.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    ONCE.call_once(|| {
        eprintln!(
            "gpui-maps dots: a tile held more blocks than its texture can; dropped {n} \
             (the sparsest, first to die as ppd rises). Further drops are counted in \
             render::dots::dot_blocks_dropped_total()."
        );
    });
}

/// Vertices per dot slot: one triangle circumscribing the dot's disc
/// (`shader.wgsl`'s `dot_slot`). MG-8 measured the two-triangle quad at
/// 20.8 ms against this triangle's 13.6 ms for the same 4 M dots.
pub const DOT_VERTICES: u32 = 3;

/// Texels per row of a tile's `DotBlock` texture (`shader.wgsl`'s
/// `DOT_TEXTURE_WIDTH`, `the_shader_texture_width_matches_the_rust_one`):
/// WebGL2's downlevel `max_texture_dimension_2d`, so a tile holds up to
/// 2048 x 2048 = 4 M blocks on every platform.
pub const DOT_TEXTURE_WIDTH: u32 = 2048;

/// `vs_dot`'s per-tile uniform, `shader.wgsl`'s `DotParams` at group 2
/// binding 1 (the slot `CircleParams` takes in the circle pipeline). 144
/// bytes: WGSL rounds a struct holding a `vec4` up to a multiple of 16,
/// and `shape` and `category_mask` fill the two words that rounding would
/// otherwise leave as padding, so the Rust side is the same size.
///
/// Everything here is per TILE: a tile is ONE draw whose vertices run
/// through its four buckets in turn, and `seg_vertex`/`seg_block` (filled
/// by `TilePipeline::create_dot_bind_group` from the tile's
/// `DotTileGpu`) are where each bucket starts.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct DotParams {
    /// Boxes, in the tile's lyon units, whose cells a stand-in does NOT
    /// draw -- tested against a block's `origin` (the cell's SW corner).
    pub exclude_a: [f32; 4],
    pub exclude_b: [f32; 4],
    /// First vertex of each bucket's run in the tile's draw.
    pub seg_vertex: [u32; 4],
    /// First block (texel) of each bucket's run.
    pub seg_block: [u32; 4],
    /// Population per dot of the VIEW (not the tile's `ppd_build`).
    pub ppd: f32,
    /// Cell width in lyon units (`TileBlocks::cell_width`).
    pub cell_width: f32,
    /// The tile's Mercator y of its north edge, in unit world `[0, 1)`, for
    /// `cosh(psi)`.
    pub tile_origin_y: f32,
    pub radius_px: f32,
    /// Live-mode mix toward `pop_b`; 0 otherwise.
    pub t: f32,
    /// Paint-table row of the 256-entry density ramp.
    pub ramp_row: u32,
    /// 1 when the tile's block texture carries a `pop_b` region (live
    /// mode); set by `DotTileGpu::layout`.
    pub live: u32,
    /// Unit world per lyon unit: turns the tile-local `y` into the Mercator
    /// y `cosh(psi)` needs, and the cell width into the arc the density's
    /// cell area needs. `1 / lyon_per_world` of the `build_blocks` call
    /// that made the tile's blocks.
    pub unit_world_per_lyon: f32,
    /// Texel of block 0's `pop_b`, when live.
    pub pop_b_offset: u32,
    /// How much of `radius_px` the densest cells lose: a dot's radius is
    /// `radius_px * (1 - radius_shrink * level)`, `level` its cell's place
    /// on the density ramp in `[0, 1]` ([`dot_radius_px`]). `DotStyle`'s
    /// `density_shrink`; 0 keeps every dot at `radius_px`.
    pub radius_shrink: f32,
    /// How fast a dense cell's dots fade with their expected overlap
    /// ([`overlap_alpha`]'s `gamma`): `DotStyle`'s `overlap_gamma`; 0 = no
    /// fade. Alpha only -- which dots exist never depends on it.
    pub overlap_gamma: f32,
    /// Relocation candidates per dot ([`relocation_k`]): 0 or 1 draws
    /// today's placement and skips every mask read; K > 1 moves each dot to
    /// the best-ranked of its first K candidates
    /// (`overlay::dotgrid::hash::{dot_candidates, choose}`), inside its own
    /// cell. Position only -- which dots exist never depends on it.
    pub mask_k: u32,
    /// The tile's lookup into the bound mask
    /// (`dot_mask::MaskGrid::for_tile`): a candidate at tile lyon `p` reads
    /// texel `floor(p * mask_texels_per_lyon + mask_frac) +
    /// mask_int_offset`. Unused while `mask_k <= 1`.
    pub mask_int_offset: [i32; 2],
    pub mask_frac: [f32; 2],
    pub mask_texels_per_lyon: f32,
    /// How a dot is coloured: [`COLOR_DENSITY`] (0) by its cell's density
    /// on the ramp; [`COLOR_DIFFERENCE`] (1) by its cell's change from
    /// epoch A to B (the difference ramp, [`difference_level`]) -- only a
    /// live tile (`live` 1) has a B, and a tile without one reads no
    /// change; [`COLOR_CATEGORICAL`] (2) by its group
    /// (`overlay::dotgrid::composition::group_of` over the block's
    /// composition texel), entry `group` of paint row `ramp_row`.
    pub color_mode: u32,
    /// [`SHAPE_CELL`]: a block's dots fill the rectangle north-east of its
    /// `origin`, `cell_width` wide. [`SHAPE_TRIANGLE`]: they fill the
    /// triangle `origin`, `origin + e1`, `origin + e2` of the block's
    /// shape texel (`overlay::dotgrid::parts::sample_in_triangle`), hashed
    /// by `parts::part_key`. A triangle tile's style turns relocation, the
    /// density shrink and the overlap fade off (C12): the shader does not
    /// branch on the shape for them.
    pub shape: u32,
    /// The legend's toggles under [`COLOR_CATEGORICAL`]: a dot whose group
    /// bit is 0 is sent off-clip like a dead slot. Ignored in the other
    /// colour modes; [`CATEGORY_MASK_ALL`] draws every group.
    pub category_mask: u32,
}

impl DotParams {
    /// Whether the terms that assume a grid cell are off: relocation
    /// (`mask_k <= 1`), the density shrink and the overlap fade. A
    /// [`SHAPE_TRIANGLE`] draw must have them off (C12): the shader reads
    /// them through `cell_width`, which a triangle tile does not have.
    #[must_use]
    pub fn cell_terms_off(&self) -> bool {
        self.mask_k <= 1 && self.radius_shrink == 0.0 && self.overlap_gamma == 0.0
    }
}

/// [`DotParams::color_mode`]: colour by density on the ramp.
pub const COLOR_DENSITY: u32 = 0;
/// [`DotParams::color_mode`]: colour by the change from epoch A to B.
pub const COLOR_DIFFERENCE: u32 = 1;
/// [`DotParams::color_mode`]: colour by group, from the composition texel.
pub const COLOR_CATEGORICAL: u32 = 2;
/// [`DotParams::shape`]: a grid cell's rectangle.
pub const SHAPE_CELL: u32 = 0;
/// [`DotParams::shape`]: a block part's triangle.
pub const SHAPE_TRIANGLE: u32 = 1;
/// [`DotParams::category_mask`] with every one of the 8 groups on.
pub const CATEGORY_MASK_ALL: u32 = 0xFF;

/// A dot's radius in pixels on the density ramp: `radius_px` shrunk by up
/// to `shrink` (the spec's "up to 30 % with density") as its cell's
/// `level` -- `log10(people per km^2) / DOT_DENSITY_LOG10_MAX`, clamped to
/// `[0, 1]`, the same place the ramp colour is read from -- goes from 0 to
/// 1. `vs_dot`'s `dot_slot` computes exactly this.
#[must_use]
pub fn dot_radius_px(radius_px: f32, shrink: f32, level: f32) -> f32 {
    radius_px * (1.0 - shrink * level.clamp(0.0, 1.0))
}

/// The minimum on-screen width of a data cell, in logical pixels, for
/// relocation: below it a dot's move is under 2 px, so the draw keeps
/// today's placement and the mask need not cover that ground
/// (`dot_mask::relocation_region`; spec 3.5a).
pub const RELOCATION_MIN_CELL_PX: f32 = 2.0;

/// Relocation starts at this camera zoom (Task 9 ruling): below it a dot
/// layer draws the uniform placement (the fade only), no mask pass runs and
/// no mask ring is fetched -- `dot_mask::relocation_region` is `None` and
/// `DotStyle::relocated` gives `mask_k` 0.
pub const RELOCATION_MIN_ZOOM: f64 = 15.0;

/// Relocation is off above this camera pitch (Task 9 ruling), as on the
/// globe and below [`RELOCATION_MIN_ZOOM`]: no mask pass, no ring, `mask_k`
/// 0. A pitched view's ring cost a median +87 MB of RSS, and without it an
/// arriving tile re-placed the edge cells' dots (visible jumps).
pub const RELOCATION_MAX_PITCH_DEG: f32 = 30.0;

/// Whether a camera at `zoom`, `pitch` (radians) and globe `transition`
/// relocates at all: a flat frame (`transition` exactly 0 -- relocation is
/// off on the globe by ruling, and an unknown or out-of-range transition is
/// not flat), z15 or closer, and pitched no more than
/// [`RELOCATION_MAX_PITCH_DEG`]. The one spelling of the gate: the mask
/// region (`dot_mask::relocation_region`, and through it the ring on every
/// shell and in the capture) and each draw's `mask_k`
/// (`DotStyle::relocated`) ask it.
#[must_use]
pub fn relocation_camera(zoom: f64, pitch: f32, transition: f32) -> bool {
    // A hair of slack: 30 deg through radians and back reads 30.000002.
    transition == 0.0 && zoom >= RELOCATION_MIN_ZOOM && pitch.to_degrees() <= RELOCATION_MAX_PITCH_DEG + 1e-3
}

/// Relocation candidates per dot on the 30″ grid, `DotStyle::mask_k`'s
/// default, chosen by DP-2 (`capture::tests::mg8_vertex_cost` with
/// `MG8_K`/`MG8_MASK`, 2.8 M live dots, every dot relocating, each K
/// paired with a K = 0 run in the same process): the largest of {4, 6, 8}.
/// On the flat path (`vs_dot_flat`, the only path that relocates --
/// relocation is off on the globe) K = 8 costs +1.2 to +1.6 % (paired
/// median) over K = 0 on every mask; forced onto the globe path it would
/// cost +26 % (K = 6: +16 %, K = 4: +8 %), which is why the globe must
/// not relocate at this K. `docs/measurements/2026-09-27-dasymetric-dot-placement.md`.
pub const DEFAULT_MASK_K_30SS: u32 = 8;

/// The most candidates a dot ever tries: `shader.wgsl`'s `DOT_MASK_K_MAX`,
/// which caps the uniform's `mask_k` so a bad value cannot run the loop
/// long. Twice the plan's largest K (8): the WGSL chain test runs K = 16.
pub const MASK_K_MAX: u32 = 16;

/// Expected overlaps at which the fade starts: below it a dot keeps its alpha.
pub const OVERLAP_KNEE: f32 = 1.0;
/// The lowest alpha the fade gives a live dot. FO-2 passed at 0.1 (world
/// 93 %, India 87 % of the fade-off ink at gamma 0.35), so it stays.
pub const OVERLAP_ALPHA_FLOOR: f32 = 0.1;

/// **The saturation cap** on a density-ramp cell (`shader.wgsl`'s
/// `DOT_INK_CAP`, [`ink_cap_keep`]): the optical depth -- dots per pixel
/// times each one's alpha -- past which a cell's further dots are not
/// drawn. `exp(-8)` is 1/2981 of the background left, under one 8-bit
/// step, so the pixel is its dots' colour either way. Measured on the
/// Studio's opening globe (1280x800 at 2x, 869 k dots): the tile pass went
/// from 15-18 ms to 5-9 ms, and the globe's pixels outside the ppd label
/// moved by no more than two re-runs of master do (`docs/measurements/
/// 2026-10-04-dot-gpu.md`).
pub const INK_CAP: f32 = 8.0;

/// [`INK_CAP`] for race dots (categorical colour, or part triangles): a
/// cell's neighbours are other colours, so the dots it spills over them
/// matter, and its ink is spread over the cell grown by a dot's diameter
/// against a cap four times higher. 16 and 24 left Chicago's and Detroit's
/// minority cores visibly thinner; 32 is at master's own run-to-run noise.
pub const INK_CAP_RACE: f32 = 32.0;

/// How many of a cell's `quota` dots `dot_slot` draws -- dots of radius
/// `hw` px and alpha `a` over a cell of `area_px` px^2 -- as the index
/// bound `dot < keep`: everything (`u32::MAX`-ish) unless the cell's ink
/// is over the cap, then the prefix whose ink is exactly the cap.
/// `shader.wgsl`'s `ink_cap_keep`, line for line. Draw-only: what is
/// ALIVE (`hash::dot_alive`, the stats, the label) never depends on it.
#[must_use]
pub fn ink_cap_keep(quota: f32, hw: f32, a: f32, area_px: f32, race: bool) -> f32 {
    let q = quota.max(0.0);
    let disc = std::f32::consts::PI * hw * hw;
    let (spread, cap) = if race {
        let reach = area_px.max(0.0).sqrt() + 2.0 * hw;
        (reach * reach, INK_CAP_RACE)
    } else {
        (area_px.max(disc), INK_CAP)
    };
    let ink = q * disc * a / spread;
    if ink > cap {
        q * cap / ink
    } else {
        4_294_967_040.0
    }
}

/// λ: the expected number of the cell's live dots covering one pixel of it.
/// `quota = pop / ppd`, `r` the drawn radius in px, `cell_area_px` the cell's
/// on-screen area (`(cell_width * px_per_lyon)^2 * stretch`). `shader.wgsl`'s
/// `dot_slot` computes exactly this.
#[must_use]
pub fn expected_overlap(quota: f32, r: f32, cell_area_px: f32) -> f32 {
    quota.max(0.0) * std::f32::consts::PI * r * r / cell_area_px.max(1e-6)
}

/// **Fade by expected overlap** (controller ruling A): a dense cell's dots
/// lose alpha as `lambda` ([`expected_overlap`]) grows, so its expected
/// ink `1 - e^{-alpha lambda}` keeps rising with the population -- a
/// gradient -- instead of saturating into a solid square. 1 at `lambda <=
/// OVERLAP_KNEE` or `gamma == 0`; else `max(OVERLAP_ALPHA_FLOOR,
/// (OVERLAP_KNEE / lambda)^gamma)`. Finite for every input. `shader.wgsl`'s
/// `overlap_alpha`, line for line.
#[must_use]
pub fn overlap_alpha(lambda: f32, gamma: f32) -> f32 {
    if gamma <= 0.0 || !(lambda > OVERLAP_KNEE) {
        return 1.0;
    }
    (OVERLAP_KNEE / lambda).powf(gamma).clamp(OVERLAP_ALPHA_FLOOR, 1.0)
}

/// px per lyon unit on a flat frame: `2^zoom * WORLD_SIZE * unit_world_per_lyon`.
#[must_use]
pub fn px_per_lyon(zoom: f32, unit_world_per_lyon: f32) -> f32 {
    zoom.exp2() * crate::render::WORLD_SIZE * unit_world_per_lyon
}

impl DotParams {
    /// An exclusion box nothing is inside (min > max).
    pub const NONE_EXCLUDED: [f32; 4] = [1.0, 1.0, -1.0, -1.0];

    /// These params relocating against a mask: `mask_k` candidates (from
    /// [`relocation_k`]) looked up through `lookup`, the tile's
    /// `MaskGrid::for_tile`.
    #[must_use]
    pub fn relocating(self, mask_k: u32, lookup: crate::render::dot_mask::MaskTileLookup) -> Self {
        debug_assert!(mask_k <= MASK_K_MAX, "mask_k {mask_k} over MASK_K_MAX {MASK_K_MAX}");
        DotParams {
            mask_k,
            mask_int_offset: lookup.int_offset,
            mask_frac: lookup.frac,
            mask_texels_per_lyon: lookup.texels_per_lyon,
            ..self
        }
    }
}

/// K for one draw: 0 (today's shader) unless relocation is on, a basemap
/// mask is bound, and the draw's cells are >= [`RELOCATION_MIN_CELL_PX`]
/// on screen. A NaN `cell_px` is 0.
#[must_use]
pub fn relocation_k(cell_px: f32, mask_bound: bool, relocate: bool, k: u32) -> u32 {
    if relocate && mask_bound && cell_px >= RELOCATION_MIN_CELL_PX {
        k
    } else {
        0
    }
}

/// The density ramp's span: `vs_dot` maps `log10(people per km^2)` from 0
/// (1 per km^2) to this (100,000 per km^2) onto ramp entries 0..=255.
/// `shader.wgsl`'s `DOT_DENSITY_LOG10_MAX` must equal it
/// (`the_shader_density_span_matches_the_rust_one`).
pub const DOT_DENSITY_LOG10_MAX: f32 = 5.0;

/// The difference ramp's span (Task 15): `vs_dot` maps `log2` of a
/// cell's growth from epoch A to B, `(pop_b + 1) / (pop + 1)`, from
/// `-DOT_DIFF_LOG2_SPAN` (4x fewer people) to `+DOT_DIFF_LOG2_SPAN` (4x
/// more) onto ramp entries 0..=255; no change is the middle entry.
/// `shader.wgsl`'s `DOT_DIFF_LOG2_SPAN` must equal it.
pub const DOT_DIFF_LOG2_SPAN: f32 = 2.0;

/// Where a cell of `pop` people in epoch A and `pop_b` in epoch B sits on
/// the difference ramp, `[0, 1]` (0.5: no change). `shader.wgsl`'s
/// `dot_slot` computes exactly this when `DotParams::difference` is 1.
#[must_use]
pub fn difference_level(pop: f32, pop_b: f32) -> f32 {
    let growth = ((pop_b.max(0.0) + 1.0) / (pop.max(0.0) + 1.0)).log2();
    (0.5 + 0.5 * growth / DOT_DIFF_LOG2_SPAN).clamp(0.0, 1.0)
}

/// Entries in a density ramp: one paint-table row of `(from, to)` pairs.
pub const DOT_RAMP_LEN: usize = 256;

/// A 256-entry density ramp (`DOT_RAMP_LEN`), linearly interpolated through
/// `stops` (at least one), as `(colour, colour)` pairs for
/// `compact::paint_table`: `vs_dot` reads the `from` of entry `i` only.
#[must_use]
pub fn ramp_paints(stops: &[crate::style::Rgba]) -> Vec<(crate::style::Rgba, crate::style::Rgba)> {
    assert!(!stops.is_empty(), "a ramp needs at least one stop");
    (0..DOT_RAMP_LEN)
        .map(|i| {
            let c = if stops.len() == 1 {
                stops[0]
            } else {
                let at = i as f32 / (DOT_RAMP_LEN - 1) as f32 * (stops.len() - 1) as f32;
                let k = (at.floor() as usize).min(stops.len() - 2);
                crate::style::Lerp::lerp(&stops[k], &stops[k + 1], at - k as f32)
            };
            (c, c)
        })
        .collect()
}

/// The WebGL2 check behind `the_dot_shader_is_valid_glsl_es_300`, for any
/// WGSL module: it validates, no vector-condition `select` picks between
/// non-float vectors (naga writes that `mix(uvec, uvec, bvec)`, which ESSL
/// 3.00 lacks), each named entry point translates through naga's GLSL ES
/// 3.00 backend with no such `mix(` in the text, and `glslangValidator`
/// accepts it when it is on `PATH`. `donut_shader.wgsl` goes through it too.
#[cfg(test)]
pub(crate) fn assert_valid_glsl_es_300(src: &str, entry_points: &[(&str, naga::ShaderStage)]) {
    use naga::back::glsl;
    let module = naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(src)));
    let info = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
        .validate(&module)
        .expect("the shader validates");
    // The IR rule, over every function and entry point.
    let check = |f: &naga::Function, fi: &naga::valid::FunctionInfo, name: &str| {
        for (h, e) in f.expressions.iter() {
            if let naga::Expression::Select { condition, .. } = *e {
                let cond = fi[condition].ty.inner_with(&module.types);
                let out = fi[h].ty.inner_with(&module.types);
                if let naga::TypeInner::Vector { .. } = cond {
                    assert!(
                        out.scalar_kind() == Some(naga::ScalarKind::Float),
                        "{name}: a vector-condition select over {out:?} is mix(.., bvec) in GLSL, not ESSL 3.00"
                    );
                }
            }
        }
    };
    for (h, f) in module.functions.iter() {
        check(f, &info[h], f.name.as_deref().unwrap_or("?"));
    }
    for (i, ep) in module.entry_points.iter().enumerate() {
        check(&ep.function, info.get_entry_point(i), &ep.name);
    }
    for &(ep, stage) in entry_points {
        let options = glsl::Options { version: glsl::Version::Embedded { version: 300, is_webgl: true }, ..Default::default() };
        let pipeline = glsl::PipelineOptions { shader_stage: stage, entry_point: ep.to_string(), multiview: None };
        let mut out = String::new();
        glsl::Writer::new(&mut out, &module, &info, &options, &pipeline, naga::proc::BoundsCheckPolicies::default())
            .and_then(|mut w| w.write().map(|_| ()))
            .unwrap_or_else(|e| panic!("{ep}: naga's GLSL ES 300 backend: {e:?}"));
        assert!(out.starts_with("#version 300 es"), "{ep}");
        for bad in ["mix(uvec", "mix(ivec", "mix(bvec", "mix(uint(", "mix(int(", "mix(bool("] {
            assert!(!out.contains(bad), "{ep}: `{bad}` is not ESSL 3.00");
        }
        let ext = if stage == naga::ShaderStage::Fragment { "frag" } else { "vert" };
        let validator = std::process::Command::new("glslangValidator").arg("--version").output();
        if validator.is_ok_and(|o| o.status.success()) {
            let path = std::env::temp_dir().join(format!("gpui-maps-{ep}-{}-{}.{ext}", std::process::id(), out.len()));
            std::fs::write(&path, &out).expect("write the GLSL");
            let run = std::process::Command::new("glslangValidator").arg("-S").arg(ext).arg(&path).output().expect("glslangValidator");
            let _ = std::fs::remove_file(&path);
            assert!(run.status.success(), "{ep}: glslangValidator: {}", String::from_utf8_lossy(&run.stdout));
        } else {
            eprintln!("{ep}: glslangValidator is not installed; checked the IR and the text only");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<DotBlock>(), 16);
    }

    #[test]
    fn the_shader_texture_width_matches_the_rust_one() {
        let src = include_str!("shader.wgsl");
        assert!(src.contains(&format!("const DOT_TEXTURE_WIDTH: u32 = {DOT_TEXTURE_WIDTH}u;")));
    }

    #[test]
    fn the_shader_vertex_count_matches_the_rust_one() {
        let src = include_str!("shader.wgsl");
        assert!(src.contains(&format!("const DOT_VERTICES: u32 = {DOT_VERTICES}u;")));
    }

    #[test]
    fn overlap_alpha_is_one_below_the_knee_and_when_off() {
        assert_eq!(overlap_alpha(0.3, 0.5), 1.0);
        assert_eq!(overlap_alpha(OVERLAP_KNEE, 0.5), 1.0);
        assert_eq!(overlap_alpha(50.0, 0.0), 1.0);
    }

    #[test]
    fn overlap_alpha_falls_with_overlap_and_expected_ink_still_rises() {
        // Expected ink of a pixel under Poisson(lambda) dots of alpha a: 1 - e^{-a lambda}.
        let ink = |l: f32| 1.0 - (-overlap_alpha(l, 0.5) * l).exp();
        let mut last = 0.0;
        for l in [1.5f32, 3.0, 6.0, 12.0, 24.0] {
            assert!(overlap_alpha(l, 0.5) < overlap_alpha(l / 2.0, 0.5).max(1.0) + 1e-6);
            assert!(ink(l) > last, "ink must keep rising: a gradient, not a plateau");
            last = ink(l);
        }
    }

    #[test]
    fn overlap_alpha_is_finite_and_floored_for_any_input() {
        for l in [0.0f32, 1e-30, 1.0, 1e6, 1e30, f32::INFINITY] {
            for g in [0.0f32, 0.35, 0.5, 0.7, 1.0] {
                let a = overlap_alpha(l, g);
                assert!(a.is_finite() && (OVERLAP_ALPHA_FLOOR..=1.0).contains(&a), "l={l} g={g} a={a}");
            }
        }
        assert!(expected_overlap(10.0, 1.0, 0.0).is_finite(), "zero-area cell must not divide by zero");
    }

    /// 144, not the plan's 128: the mask lookup is `int_offset` (vec2<i32>) +
    /// `frac` (vec2<f32>) + `texels_per_lyon` -- five words after
    /// `mask_k`, not the plan's three -- and a uniform struct's size rounds
    /// up to 16 (132 -> 144).
    #[test]
    fn a_dot_params_is_144_bytes() {
        assert_eq!(std::mem::size_of::<DotParams>(), 144);
        assert_eq!(std::mem::offset_of!(DotParams, mask_k), 108);
        assert_eq!(std::mem::offset_of!(DotParams, mask_int_offset), 112, "vec2<i32> aligns to 8");
        assert_eq!(std::mem::offset_of!(DotParams, mask_frac), 120);
        assert_eq!(std::mem::offset_of!(DotParams, mask_texels_per_lyon), 128);
    }

    #[test]
    fn relocation_is_off_below_two_pixel_cells_without_a_mask_or_when_switched_off() {
        assert_eq!(relocation_k(1.9, true, true, 8), 0);
        assert_eq!(relocation_k(2.0, true, true, 8), 8);
        assert_eq!(relocation_k(50.0, false, true, 8), 0);
        assert_eq!(relocation_k(50.0, true, false, 8), 0);
        assert_eq!(relocation_k(f32::NAN, true, true, 8), 0);
    }

    /// The one relocation gate (final review m1): the zoom, the pitch and
    /// the globe transition, spelled once. The mask region, the ring, the
    /// capture's ring and `DotStyle::relocated` all ask it.
    #[test]
    fn relocation_camera_is_flat_z15_or_closer_and_pitched_at_most_thirty_degrees() {
        let thirty = RELOCATION_MAX_PITCH_DEG.to_radians();
        assert!(relocation_camera(15.0, 0.0, 0.0));
        assert!(relocation_camera(18.0, thirty, 0.0), "exactly 30 deg relocates");
        assert!(!relocation_camera(14.99, 0.0, 0.0), "below z15");
        assert!(!relocation_camera(15.0, (RELOCATION_MAX_PITCH_DEG + 0.5).to_radians(), 0.0), "past 30 deg");
        assert!(!relocation_camera(15.0, 0.0, 1e-6), "any globe transition");
        assert!(!relocation_camera(15.0, 0.0, 1.0));
        assert!(!relocation_camera(15.0, 0.0, f32::NAN), "an unknown transition is not flat");
        assert!(!relocation_camera(15.0, 0.0, -0.1), "a transition outside 0..=1 is not flat");
    }

    #[test]
    fn the_shader_dot_params_field_order_matches_the_rust_one() {
        let src = include_str!("shader.wgsl");
        let body = src.split_once("struct DotParams {").expect("DotParams is declared").1;
        let body = body.split_once("\n}").expect("the struct is closed").0;
        let members: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//") && l.contains(':'))
            .map(|l| l.split(':').next().expect("a name").trim())
            .collect();
        let tail = ["overlap_gamma", "mask_k", "mask_int_offset", "mask_frac", "mask_texels_per_lyon", "color_mode", "shape", "category_mask"];
        assert!(members.ends_with(&tail), "shader DotParams members: {members:?}");
        assert!(src.contains("    mask_int_offset: vec2<i32>,") && src.contains("    mask_frac: vec2<f32>,"));
    }

    #[test]
    fn the_shader_difference_span_matches_the_rust_one() {
        let src = include_str!("shader.wgsl");
        assert!(src.contains(&format!("const DOT_DIFF_LOG2_SPAN: f32 = {DOT_DIFF_LOG2_SPAN:.1};")));
        assert_eq!(std::mem::offset_of!(DotParams, color_mode), 132);
        assert_eq!(std::mem::offset_of!(DotParams, shape), 136);
        assert_eq!(std::mem::offset_of!(DotParams, category_mask), 140);
    }

    #[test]
    fn the_shader_shape_constants_match_the_rust_ones() {
        let src = include_str!("shader.wgsl");
        assert!(src.contains(&format!("const DOT_SHAPE_CELL: u32 = {SHAPE_CELL}u;")));
        assert!(src.contains(&format!("const DOT_SHAPE_TRIANGLE: u32 = {SHAPE_TRIANGLE}u;")));
        assert!(src.contains(&format!("const DOT_COLOR_DIFFERENCE: u32 = {COLOR_DIFFERENCE}u;")));
        assert!(src.contains(&format!("const DOT_COLOR_CATEGORICAL: u32 = {COLOR_CATEGORICAL}u;")));
        assert_eq!(COLOR_DENSITY, 0, "a zeroed DotParams colours by density");
        assert_eq!(SHAPE_CELL, 0, "a zeroed DotParams draws cells");
    }

    /// Task 2's F1: the WGSL alive rule is `hash::dot_alive`'s
    /// integer-exact one (C2), spelled in the shader once, in `dot_slot`
    /// -- and the race dots skip the last dot's fade (T2's open question).
    /// `capture::tests::the_last_of_a_thousand_people_draws_at_full_alpha`
    /// shows it on the GPU.
    #[test]
    fn the_shader_alive_rule_is_the_integer_exact_one() {
        let src = include_str!("shader.wgsl");
        let rule = "let in_quota = dot < n_whole || (dot == n_whole && h_t < q_pos - whole);";
        assert_eq!(src.matches(rule).count(), 1, "the alive rule, once");
        assert!(src.contains("let n_whole = u32(min(whole, 4294967040.0));"));
        assert!(!src.contains("f32(dot) + h_t <"), "the old f32 sum rule");
        assert!(src.contains("let fade = select(saturate((quota - f32(dot) - h_t) * 4.0), 1.0, race);"));
    }

    /// **Task 6 review F-1: the dot shader compiles as WebGL2's GLSL.**
    /// `vs_dot` and `vs_dot_flat` go through naga's GLSL ES 3.00 backend
    /// (what wgpu's GL backend runs on `?gl=1`), and:
    /// - no `select` with a VECTOR condition picks between non-float
    ///   vectors: naga writes it `mix(uvec, uvec, bvec)`, an overload
    ///   ESSL 3.00 does not have (3.10 added it), and the whole dot shader
    ///   -- world dots included -- then fails to compile in the browser;
    /// - no such `mix(` survives in the text;
    /// - when `glslangValidator` is on `PATH`, it accepts the output as a
    ///   vertex shader.
    #[test]
    fn the_dot_shader_is_valid_glsl_es_300() {
        assert_valid_glsl_es_300(
            include_str!("shader.wgsl"),
            &[("vs_dot", naga::ShaderStage::Vertex), ("vs_dot_flat", naga::ShaderStage::Vertex)],
        );
    }

    /// Task 2's F2: the shader's group rule is `composition::group_of`'s
    /// (spec §3.3), salt and all.
    #[test]
    fn the_shader_group_rule_is_group_of() {
        let src = include_str!("shader.wgsl");
        assert!(src.contains(&format!("const DOT_GROUP_SALT: u32 = {:#X}u;", crate::overlay::dotgrid::composition::OFFSET_SALT).replace("0X", "0x")));
        assert!(src.contains("let off = pcg(key ^ DOT_GROUP_SALT) % d;"));
        assert!(src.contains("let p = (stride * (i % d) + off) % d;"));
    }

    #[test]
    fn the_difference_level_is_half_at_no_change_and_clamps_past_four_times() {
        assert_eq!(difference_level(99.0, 99.0), 0.5);
        assert_eq!(difference_level(0.0, 0.0), 0.5);
        assert!((difference_level(99.0, 399.0) - 1.0).abs() < 1e-6, "4x more is the top");
        assert!((difference_level(399.0, 99.0) - 0.0).abs() < 1e-6, "4x fewer is the bottom");
        assert!((difference_level(99.0, 199.0) - 0.75).abs() < 1e-6);
        assert_eq!(difference_level(1.0, 1e9), 1.0);
    }

    #[test]
    fn the_shader_building_rank_matches_the_cpu_chains() {
        let src = include_str!("shader.wgsl");
        let rank = crate::overlay::dotgrid::hash::RANK_BUILDING;
        assert!(src.contains(&format!("const DOT_RANK_BUILDING: u32 = {rank}u;")));
    }

    #[test]
    fn the_shader_no_data_rank_matches_the_mask_clear_rank() {
        let src = include_str!("shader.wgsl");
        let no_data = u32::from(crate::overlay::dotgrid::hash::RANK_NO_DATA);
        assert_eq!(no_data, crate::render::dot_mask::MASK_CLEAR_RANK);
        assert_eq!(no_data, crate::render::dot_mask::MASK_NO_DATA_RANK);
        assert!(src.contains(&format!("const DOT_RANK_NO_DATA: u32 = {no_data}u;")));
        assert!(src.contains("{ return DOT_RANK_NO_DATA; }"), "mask_rank returns the named rank outside the mask");
        assert!(src.contains("if (rank == DOT_RANK_NO_DATA) {"), "a no-data candidate ends the search at candidate 0");
    }

    #[test]
    fn the_shader_caps_mask_k_at_the_rust_cap() {
        let src = include_str!("shader.wgsl");
        assert!(src.contains(&format!("const DOT_MASK_K_MAX: u32 = {MASK_K_MAX}u;")));
        assert!(src.contains("min(dot_params.mask_k, DOT_MASK_K_MAX)"));
        const { assert!(DEFAULT_MASK_K_30SS <= MASK_K_MAX) };
    }

    #[test]
    fn the_shader_overlap_constants_match_the_rust_ones() {
        let src = include_str!("shader.wgsl");
        assert!(src.contains(&format!("const DOT_OVERLAP_KNEE: f32 = {:?};", OVERLAP_KNEE)));
        assert!(src.contains(&format!("const DOT_OVERLAP_ALPHA_FLOOR: f32 = {:?};", OVERLAP_ALPHA_FLOOR)));
        assert!(src.contains(&format!("const DOT_WORLD_SIZE: f32 = {:?};", crate::render::WORLD_SIZE)));
    }

    #[test]
    fn the_shader_ink_caps_match_the_rust_ones() {
        let src = include_str!("shader.wgsl");
        assert!(src.contains(&format!("const DOT_INK_CAP: f32 = {INK_CAP:?};")));
        assert!(src.contains(&format!("const DOT_INK_CAP_RACE: f32 = {INK_CAP_RACE:?};")));
        assert!(src.contains("let keep = ink_cap_keep(quota, s.half_width, s.color.a, area_px, race);"));
    }

    #[test]
    fn the_ink_cap_never_touches_a_sparse_cell_or_a_cell_smaller_than_its_dots() {
        // A few dots over a cell many dots wide: nothing capped.
        assert!(ink_cap_keep(10.0, 0.5, 1.0, 100.0, false) > 1e9);
        // A cell smaller than one dot spreads its ink over one disc: it
        // needs INK_CAP / a dots stacked on it before any is skipped.
        assert!(ink_cap_keep(INK_CAP - 0.5, 0.5, 1.0, 0.01, false) > 1e9);
        // A race cell of one or two dots never is, whatever its area.
        assert!(ink_cap_keep(2.0, 0.5, 0.8, 0.0, true) > 1e9);
    }

    #[test]
    fn a_capped_cell_keeps_exactly_the_cap_of_ink_and_more_as_the_cap_rises() {
        let (quota, hw, a, area) = (5000.0_f32, 0.5_f32, 0.3_f32, 4.0_f32);
        let keep = ink_cap_keep(quota, hw, a, area, false);
        assert!(keep < quota);
        let kept_ink = keep * std::f32::consts::PI * hw * hw * a / area.max(std::f32::consts::PI * hw * hw);
        assert!((kept_ink - INK_CAP).abs() < 1e-3, "{kept_ink}");
        // the race rule spreads wider and caps higher: it keeps more.
        assert!(ink_cap_keep(quota, hw, a, area, true) > keep);
        // nesting: a finer ppd (more dots) never keeps fewer.
        assert!(ink_cap_keep(2.0 * quota, hw, a, area, false) >= keep - 1e-3);
    }

    #[test]
    fn the_shader_density_span_matches_the_rust_one() {
        let src = include_str!("shader.wgsl");
        assert!(
            src.contains(&format!("const DOT_DENSITY_LOG10_MAX: f32 = {DOT_DENSITY_LOG10_MAX:?};")),
            "shader.wgsl's DOT_DENSITY_LOG10_MAX must equal render::dots::DOT_DENSITY_LOG10_MAX"
        );
    }
}
