//! The dot-density layer every shell drives: which count tiles a frame
//! wants and in what order, which request goes out next, what landed (and
//! whether it still belongs to the source that asked), which ancestor
//! stands in while a tile is missing, the view's population per dot, and --
//! in [`DotLayersGpu`] -- what goes on the GPU and in which draw.
//!
//! The `overlay::host` pattern (`tests/overlays_are_driven_through_the_host.rs`
//! holds this file to the same rules): no platform, no executor, no IO and
//! no std clock. Work leaves as a [`DotJob`] and comes back through
//! [`DotLayers::land`]; the shell decides where it runs. Native runs it on
//! tokio's blocking pool with `fetch_native::execute` (`map_view/dots.rs`);
//! the browser and the phone will run the same jobs with their own fetchers
//! (Tasks 16/17).
//!
//! # A frame
//!
//! [`DotLayers::frame`]: `level_for_zoom` picks the pyramid level, the
//! frame's cover at that level's data zoom becomes [`plan_dots`]' keys,
//! the [`PpdController`] updates from the visible population, and old
//! decoded tiles past [`DECODED_TILES_PER_SOURCE`] are evicted.
//! [`DotLayers::next_job`] then hands out fetches (parents first, at most
//! [`MAX_REQUESTS_IN_FLIGHT`] per source, up to `max_batch` same-level
//! keys per request) and block rebuilds (`level::needs_rebuild`).
//!
//! # Generations
//!
//! Every [`DotLayers::set_source`] gets a fresh generation from one
//! counter that never goes back -- replacing a source, or removing it and
//! adding the same id again, both move it. A job carries the generation it
//! was issued under, and [`DotLayers::land`] drops (and counts, see
//! [`DotLayers::stale_dropped`]) any result whose generation is not the
//! source's current one: a tile from a replaced source is never installed.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::blocks::{self, TileBlocks};
use super::in_view::{self, CellIndex, ViewRegion};
use super::level::{self, PpdController};
use super::{grid, GridCountSource, GridDescriptor, GridError, GridRequest, GridTile, GridTileKey};
use crate::geo::TileId;
use crate::render::dots::DotParams;
use crate::style::Rgba;
use crate::timing::Instant;

/// The credit GHSL's CC BY 4.0 licence requires. The archive's own
/// metadata carries it (`GridDescriptor::attribution`); this is the string
/// the converter writes and a test can name.
pub const GHSL_ATTRIBUTION: &str = "GHS-POP R2023A, European Commission JRC";

/// What a relocating layer appends to its source's credit
/// ([`DotLayers::attributions`]): a dot's place inside its cell comes from
/// the basemap, not the data, so the map says the placement is
/// illustrative. Appended to the dot credit only -- never in place of the
/// basemap's credit or GHSL's.
pub const ILLUSTRATIVE_PLACEMENT_NOTE: &str = "dot positions within each cell are illustrative";

/// Requests one source may have out at once (fetches and block rebuilds
/// together).
pub const MAX_REQUESTS_IN_FLIGHT: usize = 4;

/// Decoded tiles (and their built blocks) a source keeps: the least
/// recently wanted beyond this are dropped, never one the current frame
/// wants or draws. The GPU copy goes with its tile.
pub const DECODED_TILES_PER_SOURCE: usize = 64;

/// How long a key whose request failed waits before it is asked for again.
pub const FAILED_BACKOFF: Duration = Duration::from_secs(5);

/// Deepest a stand-in's exclusion grid goes below the stand-in (32x32
/// cells), and the most drawn descendants it arranges boxes around. Past
/// either, the stand-in draws alone over its whole tile and its
/// descendants wait (no hole, no double draw -- a coarser look).
const MAX_EXCLUSION_DEPTH: u8 = 5;
const MAX_EXCLUSION_TILES: usize = 16;

/// The density ramp a layer is coloured with, low to high.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotRamp {
    Viridis,
    Magma,
    /// Live epochs (Task 15): each dot coloured by its cell's change from
    /// epoch A to B -- blue for fewer people, near-white for no change, red
    /// for more (ColorBrewer RdBu, reversed), across
    /// `render::dots::DOT_DIFF_LOG2_SPAN`. A layer without `epoch_b` reads
    /// no change everywhere.
    Difference,
    /// Race dots (C14): each dot coloured by its group
    /// (`render::dots::COLOR_CATEGORICAL`), [`RACE_PALETTE`]'s entry for
    /// it -- not a ramp. A unit variant: a second palette waits for a
    /// second user.
    Categorical,
}

const fn hex(rgb: u32) -> Rgba {
    Rgba {
        r: ((rgb >> 16) & 0xff) as f32 / 255.0,
        g: ((rgb >> 8) & 0xff) as f32 / 255.0,
        b: (rgb & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// The 8 groups' colours, in `group_id` order (spec §1.1, Walker's
/// `people` layer's `circle-color`): the Okabe–Ito colour-blind-safe
/// palette, grey `#777777` standing in for its black. Hispanic or Latino,
/// White, Black, American Indian / Alaska Native, Asian, Native Hawaiian /
/// Pacific Islander, some other race, two or more races.
pub const RACE_PALETTE: [Rgba; 8] = [
    hex(0xE69F00),
    hex(0x0072B2),
    hex(0x009E73),
    hex(0xCC79A7),
    hex(0xD55E00),
    hex(0x56B4E9),
    hex(0x777777),
    hex(0xF0E442),
];

/// Walker's `circle-radius` (spec §1.2): `interpolate linear zoom 3 ->
/// 0.55, 7 -> 0.8, 12 -> 1.6, 14 -> 2.4` px, for
/// [`DotStyle::radius_stops`].
pub const RACE_RADIUS_STOPS: [(f64, f32); 4] = [(3.0, 0.55), (7.0, 0.8), (12.0, 1.6), (14.0, 2.4)];

/// Walker's dot opacity (spec §1.2), for [`DotStyle::opacity`].
pub const RACE_OPACITY: f32 = 0.8;

impl DotRamp {
    /// Colour stops for `render::dots::ramp_paints`: matplotlib's
    /// perceptually uniform maps, sampled at even steps; for
    /// [`Self::Categorical`], the 8 group colours ([`RACE_PALETTE`]).
    #[must_use]
    pub fn stops(self) -> Vec<Rgba> {
        let stops: &[u32] = match self {
            Self::Viridis => &[0x440154, 0x3b528b, 0x21918c, 0x5ec962, 0xfde725],
            Self::Magma => &[0x000004, 0x3b0f70, 0x8c2981, 0xde4968, 0xfe9f6d, 0xfcfdbf],
            Self::Difference => &[0x2166ac, 0x67a9cf, 0xf7f7f7, 0xef8a62, 0xb2182b],
            Self::Categorical => return RACE_PALETTE.to_vec(),
        };
        stops.iter().map(|&c| hex(c)).collect()
    }

    /// The layer's paint-table row, `render::dots::DOT_RAMP_LEN` entries
    /// of `(from, to)` for `compact::paint_table`, every alpha times
    /// `opacity` (C16: no uniform of its own). A density ramp is
    /// interpolated through [`Self::stops`] (`render::dots::ramp_paints`);
    /// [`Self::Categorical`] puts group `g`'s colour at entry `g` (0-7,
    /// what `vs_dot` reads under `COLOR_CATEGORICAL`), and the rest of the
    /// row is never read.
    #[must_use]
    pub fn paints(self, opacity: f32) -> Vec<(Rgba, Rgba)> {
        let mut paints = match self {
            Self::Categorical => {
                let mut row: Vec<(Rgba, Rgba)> = RACE_PALETTE.iter().map(|&c| (c, c)).collect();
                row.resize(crate::render::dots::DOT_RAMP_LEN, (RACE_PALETTE[7], RACE_PALETTE[7]));
                row
            }
            _ => crate::render::dots::ramp_paints(&self.stops()),
        };
        let opacity = if opacity.is_nan() { 1.0 } else { opacity.clamp(0.0, 1.0) };
        for (from, to) in &mut paints {
            from.a *= opacity;
            to.a *= opacity;
        }
        paints
    }
}

/// How a dot-density source is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct DotStyle {
    /// Live dots the view aims for (`PpdController`'s budget).
    pub budget_dots: u32,
    /// Dot radius in pixels at zoom 0 and zoom 16, linear in zoom between.
    pub radius_px_z0: f32,
    pub radius_px_z16: f32,
    /// How much of the radius the densest cells lose, `[0, 1)`: the
    /// spec's "shrinking by up to 30 % with density", so crowded cells
    /// read as dots, not solid squares (`render::dots::dot_radius_px`).
    pub density_shrink: f32,
    /// How fast a dense cell's dots fade with their expected overlap, so a
    /// dense core reads as a gradient, not a solid block
    /// (`render::dots::overlap_alpha`'s `gamma`, controller ruling A);
    /// 0 turns the fade off. Alpha only: which dots exist is unchanged.
    pub overlap_gamma: f32,
    pub ramp: DotRamp,
    /// Which epoch's counts to ask for.
    pub epoch: u16,
    /// Live epochs (Task 15): a second epoch whose counts come with
    /// `epoch`'s in the same tiles (`GridTileKey::epoch_b`), so the dots
    /// can move between the two with [`Self::epoch_mix`] alone -- no
    /// fetch, no rebuild, no upload. `None`: one epoch.
    pub epoch_b: Option<u16>,
    /// How far from `epoch` toward `epoch_b` the dots are, `[0, 1]`:
    /// `DotParams::t`. 0 draws `epoch` exactly, 1 `epoch_b`. Ignored
    /// without `epoch_b`.
    pub epoch_mix: f32,
    /// Dasymetric placement: each dot moves, inside its own cell, to the
    /// best-ranked of `mask_k` candidates against the basemap's mask
    /// (buildings first; off water and green). On by default (controller
    /// ruling). Position only: which dots exist never depends on it.
    pub relocate: bool,
    /// Candidates per dot while relocating (`render::dots::relocation_k`);
    /// 0 or 1 is today's placement. Capped at `render::dots::MASK_K_MAX`.
    pub mask_k: u32,
    /// The legend's toggles under [`DotRamp::Categorical`]: group `g` is
    /// drawn while bit `g` is set (`DotParams::category_mask`). A toggled-off
    /// group's dots are dead, not drawn black; the ppd is unchanged (it
    /// budgets the tiles' whole population). Ignored by the density ramps.
    pub category_mask: u8,
    /// The layer's opacity, `[0, 1]`: multiplies every paint-table alpha on
    /// the CPU (C16), so a change rebuilds the layer's paint table and
    /// nothing else.
    pub opacity: f32,
    /// Piecewise-linear dot radius `(zoom, px)`, ascending in zoom, held
    /// flat outside them (Walker's `interpolate linear zoom`,
    /// [`RACE_RADIUS_STOPS`]). `None`: `radius_px_z0` -> `radius_px_z16`.
    pub radius_stops: Option<Vec<(f64, f32)>>,
    /// Below this camera zoom the layer is dropped from the frame's plan:
    /// nothing wanted, nothing drawn, its GPU copies freed.
    pub min_zoom: f64,
}

impl Default for DotStyle {
    /// `render::dots::NATIVE_WINDOW_DOT_BUDGET` (MG-11's measured window
    /// budget, stricter than MG-8's `NATIVE_DOT_BUDGET`; not the spec's
    /// 4 M estimate), radii 0.6 -> 2.5 px shrinking up to 30 % with
    /// density, Viridis, epoch 2025.
    fn default() -> Self {
        Self {
            budget_dots: crate::render::dots::NATIVE_WINDOW_DOT_BUDGET.min(crate::render::dots::NATIVE_DOT_BUDGET),
            radius_px_z0: 0.6,
            radius_px_z16: 2.5,
            density_shrink: 0.3,
            // FO-1 at a Nizhny Novgorod z10 view:
            // the smallest gamma that passes FO-1 and FO-2
            // (`docs/measurements/2026-09-27-dasymetric-dot-placement.md`).
            overlap_gamma: 0.35,
            ramp: DotRamp::Viridis,
            epoch: 2025,
            epoch_b: None,
            epoch_mix: 0.0,
            relocate: true,
            mask_k: crate::render::dots::DEFAULT_MASK_K_30SS,
            category_mask: 0xFF,
            opacity: 1.0,
            radius_stops: None,
            min_zoom: 0.0,
        }
    }
}

/// The shell a map is drawn in, for [`DotStyle::for_platform`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// The gpui window (`MapView`).
    Native,
    /// The iOS shell (`ios/`).
    Ios,
    /// The browser shell (`web/`).
    Web,
}

/// The race dots' live-dot budget on `platform` (race plan, MG-R15): the
/// shell's own budget ([`DotStyle::for_platform`]) capped by
/// `render::dots::RACE_DOT_BUDGET` (MG-R10). [`DotStyle::race`] uses it.
#[must_use]
pub fn race_budget_for(platform: Platform) -> u32 {
    DotStyle::for_platform(platform).budget_dots.min(crate::render::dots::RACE_DOT_BUDGET)
}

impl DotStyle {
    /// The default style for a shell: its live-dot budget, and whether it
    /// relocates. The budgets are the measured constants in
    /// `render::dots` -- native's is [`DotStyle::default`]'s (the stricter
    /// of `NATIVE_DOT_BUDGET` and `NATIVE_WINDOW_DOT_BUDGET`), the web's
    /// `WEB_DOT_BUDGET`, the phone's `IOS_DOT_BUDGET` (unmeasured until
    /// MG-25). The phone and the browser render no dot mask yet (their
    /// basemaps carry no mask lists: DP-7's shell halves), so their dots
    /// sit uniformly in their cells (`relocate: false`).
    #[must_use]
    pub fn for_platform(platform: Platform) -> Self {
        use crate::render::dots::{IOS_DOT_BUDGET, WEB_DOT_BUDGET};
        let native = Self::default();
        match platform {
            Platform::Native => native,
            Platform::Ios => Self { budget_dots: IOS_DOT_BUDGET, relocate: false, ..native },
            Platform::Web => Self { budget_dots: WEB_DOT_BUDGET, relocate: false, ..native },
        }
    }

    /// The race-dots style (spec §1.2, C12, C16) on `platform`'s budget:
    /// the categorical palette at Walker's opacity 0.8 and radius stops,
    /// every group on, and the grid-cell terms off -- no relocation
    /// (`relocate: false`, `mask_k` 0), no density shrink and no overlap
    /// fade -- which a triangle tile must draw without (`DotParams::
    /// cell_terms_off`, asserted where every draw's params pass).
    #[must_use]
    pub fn race(platform: Platform) -> Self {
        Self {
            ramp: DotRamp::Categorical,
            epoch: 2020,
            relocate: false,
            mask_k: 0,
            density_shrink: 0.0,
            overlap_gamma: 0.0,
            opacity: RACE_OPACITY,
            radius_stops: Some(RACE_RADIUS_STOPS.to_vec()),
            budget_dots: race_budget_for(platform),
            ..Self::for_platform(platform)
        }
    }

    /// The dot radius at `zoom`: [`Self::radius_stops`] interpolated
    /// linearly, held flat outside the first and last stop; without stops,
    /// linear from `radius_px_z0` to `radius_px_z16`, held outside 0..16.
    #[must_use]
    pub fn radius_px(&self, zoom: f64) -> f32 {
        match self.radius_stops.as_deref() {
            Some([]) | None => {
                let t = (zoom / 16.0).clamp(0.0, 1.0) as f32;
                self.radius_px_z0 + (self.radius_px_z16 - self.radius_px_z0) * t
            }
            Some(stops) => {
                let (first, last) = (stops[0], stops[stops.len() - 1]);
                if zoom.is_nan() || zoom <= first.0 {
                    return first.1;
                }
                if zoom >= last.0 {
                    return last.1;
                }
                let at = stops.windows(2).find(|w| zoom < w[1].0).expect("zoom is below the last stop");
                let (a, b) = (at[0], at[1]);
                let t = ((zoom - a.0) / (b.0 - a.0)) as f32;
                a.1 + (b.1 - a.1) * t
            }
        }
    }

    /// A draw's live-epoch fields (Task 15): `t` is [`Self::epoch_mix`]
    /// (clamped to `[0, 1]`) for a key with `epoch_b`, 0 otherwise; and
    /// `color_mode` is `COLOR_DIFFERENCE` under [`DotRamp::Difference`],
    /// `COLOR_CATEGORICAL` under [`DotRamp::Categorical`] (`COLOR_DENSITY`
    /// otherwise). Uniform only: the tile's blocks and texture are the same
    /// at every mix.
    pub fn live_params(&self, params: &mut DotParams, key: &GridTileKey) {
        params.t = if key.epoch_b.is_some() { self.mix() } else { 0.0 };
        params.color_mode = match self.ramp {
            DotRamp::Difference => crate::render::dots::COLOR_DIFFERENCE,
            DotRamp::Categorical => crate::render::dots::COLOR_CATEGORICAL,
            DotRamp::Viridis | DotRamp::Magma => crate::render::dots::COLOR_DENSITY,
        };
    }

    /// The mix the dots are drawn at: [`Self::epoch_mix`] clamped to
    /// `[0, 1]` (NaN is 0) with `epoch_b`, 0 without -- also the mix the
    /// counter's people are counted at.
    #[must_use]
    pub fn mix(&self) -> f32 {
        if self.epoch_b.is_some() {
            clamp_mix(self.epoch_mix)
        } else {
            0.0
        }
    }

    /// Whether this style moves dots at all: `relocate` with more than one
    /// candidate. The basemap builds its mask only while a layer does.
    #[must_use]
    pub fn relocates(&self) -> bool {
        self.relocate && self.mask_k > 1
    }

    /// One draw's `params` with its relocation filled in: `mask_k` from
    /// [`crate::render::dots::relocation_k`] -- 0 unless this style
    /// relocates, `mask` is bound, the frame is flat (relocation is off
    /// whenever the globe transition is above 0: ruling), the camera is at
    /// `RELOCATION_MIN_ZOOM` (z15) or closer and pitched no more than
    /// `RELOCATION_MAX_PITCH_DEG` (rulings; `pitch` in radians) and the tile's
    /// cells are at least `RELOCATION_MIN_CELL_PX` on screen at `zoom` --
    /// and, when it relocates, the tile's lookup into `mask`. `mask_k` is 0
    /// whenever the draw does not relocate, so a group bound to the land
    /// texel never pays for the candidate loop.
    #[must_use]
    pub fn relocated(
        &self,
        params: DotParams,
        tile: TileId,
        zoom: f64,
        pitch: f32,
        transition: f32,
        mask: Option<&crate::render::dot_mask::MaskGrid>,
    ) -> DotParams {
        use crate::render::dots::{px_per_lyon, relocation_k, MASK_K_MAX};
        let cell_px = params.cell_width * px_per_lyon(zoom as f32, params.unit_world_per_lyon);
        let flat_and_close = crate::render::dots::relocation_camera(zoom, pitch, transition);
        let k = relocation_k(cell_px, mask.is_some() && flat_and_close, self.relocate, self.mask_k.min(MASK_K_MAX));
        match mask {
            Some(grid) if k > 1 => {
                let n = f64::from(1u32 << tile.z);
                let origin = (f64::from(tile.x) / n, f64::from(tile.y) / n);
                params.relocating(k, grid.for_tile(origin, 1.0 / blocks::OVERLAY_LYON_PER_UNIT_WORLD))
            }
            _ => DotParams { mask_k: 0, ..params },
        }
    }
}

/// An epoch mix in `[0, 1]`; NaN is 0.
#[must_use]
pub fn clamp_mix(t: f32) -> f32 {
    if t.is_nan() {
        0.0
    } else {
        t.clamp(0.0, 1.0)
    }
}

/// For the on-screen counter and the perf gate.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DotStats {
    /// Dot slots the frame's draws issue (the live prefix of every drawn
    /// tile's buckets): an upper bound of the dots alive this frame.
    pub dots_drawn: u64,
    /// The view's population per dot.
    pub ppd: f32,
    pub level: u8,
    /// Cover tiles decoded, of those the frame wants (parents included).
    pub tiles_ready: u32,
    pub tiles_wanted: u32,
    /// Ancestors drawn this frame in place of missing tiles.
    pub standin_tiles: u32,
    /// Block textures and uniforms on the GPU.
    pub gpu_bytes: u64,
    /// People whose cells (or parts, by centroid) are centred in the view
    /// (`in_view`), all 8 groups whatever the legend's `category_mask`
    /// (review F5: the ppd budgets them all, and so does this): every
    /// drawn tile's cells, a stand-in's less those its exclusion boxes
    /// keep from drawing, so no cell counts twice. The counter's figure.
    pub people_in_view: f64,
    /// Dots in view: `people_in_view / ppd`, the EXPECTED count -- each
    /// cell's live dots are `pop / ppd` on average (`hash::dot_alive`), so
    /// this is exact to within the one fractional dot a cell may or may
    /// not light.
    pub dots_in_view: u64,
    /// Tiles the frame draws, and tiles with a block texture on the GPU.
    pub tiles_drawn: u32,
    pub gpu_tiles: u32,
    /// Slots built in the drawn tiles (`TileBlocks::slots`).
    pub slots_drawn: u64,
    /// Population of the drawn tiles, whole.
    pub pop_drawn: f64,
    /// Lowest and highest `ppd_build` over the drawn tiles.
    pub built_min: f32,
    pub built_max: f32,
    /// Requests that failed, and the kind of the last failure.
    pub source_errors: u64,
    pub last_error: Option<&'static str>,
    /// Regions of a part plan drawn by cells instead of parts
    /// (`DotPlan::part_fallback`): over [`MAX_PART_TILES_PER_FRAME`], or a
    /// tilted view's far field. T8's HUD can say "cells (part cap)".
    pub part_fallback_tiles: u32,
    /// Tile rebuilds landed since the layer was set (a held tile's blocks
    /// rebuilt at a new ppd): what a pan or a crossover costs in rebuilds
    /// (T8 review F4).
    pub rebuilds_total: u64,
}

/// One draw a frame makes: a decoded tile, and the boxes (in its own lyon
/// units, [`blocks::OVERLAY_LYON_PER_UNIT_WORLD`] per world) of cells it
/// must NOT draw because a finer drawn tile covers them. A tile drawing
/// itself excludes nothing ([`DotParams::NONE_EXCLUDED`]).
///
/// Two boxes, not the brief's one: `DotParams` carries two, and a parent
/// with two diagonal children ready needs both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DotDraw {
    pub key: GridTileKey,
    pub exclude: [[f32; 4]; 2],
    /// Drawn in place of a missing finer tile.
    pub standin: bool,
}

/// What a frame wants and draws.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DotPlan {
    /// In request order: every cover key's parent (level + 1) first, so a
    /// stand-in exists as early as possible, then the cover itself.
    pub wanted: Vec<GridTileKey>,
    /// Coarse first.
    pub draw: Vec<DotDraw>,
    /// Regions of a part plan drawn by cells instead: part-zoom tiles over
    /// [`MAX_PART_TILES_PER_FRAME`], and far-field tiles between the cell
    /// levels' base and the part zoom. 0 for a cells plan.
    pub part_fallback: u32,
    /// The cell blocks a part plan draws as parts ([`select_parts`]).
    pub part_blocks: Vec<TileId>,
}

/// Levels below a missing key [`plan_dots`] searches for held tiles when
/// no ancestor is held: a zoom-out of up to this many levels past what has
/// landed still draws (85 lookups per key at most).
const MAX_CHILD_FALLBACK_LEVELS: u8 = 3;

fn children_of(t: TileId) -> [TileId; 4] {
    let (x, y, z) = (t.x * 2, t.y * 2, t.z + 1);
    [TileId { z, x, y }, TileId { z, x: x + 1, y }, TileId { z, x, y: y + 1 }, TileId { z, x: x + 1, y: y + 1 }]
}

/// `t`'s ancestor at zoom `z` (`t` itself at its own zoom).
fn ancestor(t: TileId, z: u8) -> TileId {
    let shift = t.z - z;
    TileId { z, x: t.x >> shift, y: t.y >> shift }
}

fn is_descendant(t: TileId, of: TileId) -> bool {
    t.z > of.z && ancestor(t, of.z) == of
}

/// The frame's keys, requests and draws for descriptor `d` at camera
/// `zoom`, from `cover` -- the frame's tile cover, at any zoom: a tile
/// finer than the level's data zoom is its data-zoom ancestor (overzoom),
/// one coarser (a tilted cover's far field) is a key of the coarser level
/// baked at its zoom. `ready(key)` says whether a key's blocks are held.
///
/// A missing cover key is drawn by its nearest ready ancestor, with
/// exclusion boxes over the drawn tiles inside it -- as `map_view/points.rs`
/// draws a coarse layer outside its finer ones' boxes. A stand-in has two
/// boxes; drawn tiles inside it that no two boxes can hold without also
/// covering a missing one wait (the stand-in draws their cells instead),
/// so there is never a hole and never a cell drawn twice.
pub fn plan_dots(d: &GridDescriptor, epoch: u16, zoom: f64, cover: &[TileId], ready: &dyn Fn(&GridTileKey) -> bool) -> DotPlan {
    plan_dots_epochs(d, epoch, None, zoom, cover, ready)
}

/// [`plan_dots`] for a pair of epochs (Task 15's live epochs): every key
/// it wants and draws carries `epoch_b`.
pub fn plan_dots_epochs(
    d: &GridDescriptor,
    epoch: u16,
    epoch_b: Option<u16>,
    zoom: f64,
    cover: &[TileId],
    ready: &dyn Fn(&GridTileKey) -> bool,
) -> DotPlan {
    plan_dots_kind(d, PlanKind::Cells { level: level::level_for_zoom(d, zoom) }, epoch, epoch_b, cover, ready)
}

/// What a frame of a dot layer draws: one pyramid level's cell tiles, or
/// -- a race archive at or above its crossover -- block-part tiles at
/// their data zoom (C7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Cells { level: u8 },
    Parts { data_zoom: u8 },
}

impl PlanKind {
    /// The Mercator zoom the frame's cover is asked at.
    #[must_use]
    pub fn cover_zoom(self, d: &GridDescriptor) -> u8 {
        match self {
            Self::Cells { level } => grid::data_zoom(d, level),
            Self::Parts { data_zoom } => data_zoom,
        }
    }
}

/// A race archive's plan at camera `zoom` (C7): part tiles at
/// `part_zoom` from `crossover` up (overzoomed above `part_zoom`), the
/// grid's cell levels by `level::level_for_zoom` below it.
#[must_use]
pub fn race_plan_kind(r: &super::RaceDescriptor, zoom: f64) -> PlanKind {
    if zoom >= r.crossover {
        PlanKind::Parts { data_zoom: r.part_zoom }
    } else {
        PlanKind::Cells { level: level::level_for_zoom(&r.grid, zoom) }
    }
}

/// How far past a race archive's crossover the camera must go before the
/// layer switches between cells and parts (T7 F8, controller ruling): a
/// layer drawing parts keeps them down to `crossover - 0.1`, one drawing
/// cells keeps them up to `crossover + 0.1`, so a camera hovering at the
/// line does not swap every dot back and forth.
pub const CROSSOVER_HYSTERESIS: f64 = 0.1;

/// [`race_plan_kind`] with [`CROSSOVER_HYSTERESIS`]: `parts_before` is
/// whether the layer's last frame drew parts (`None` for its first frame,
/// which takes the plain rule).
#[must_use]
pub fn race_plan_kind_after(r: &super::RaceDescriptor, zoom: f64, parts_before: Option<bool>) -> PlanKind {
    let parts = match parts_before {
        Some(true) => zoom >= r.crossover - CROSSOVER_HYSTERESIS,
        Some(false) => zoom > r.crossover + CROSSOVER_HYSTERESIS,
        None => zoom >= r.crossover,
    };
    if parts {
        PlanKind::Parts { data_zoom: r.part_zoom }
    } else {
        PlanKind::Cells { level: level::level_for_zoom(&r.grid, zoom) }
    }
}

/// A source's plan at camera `zoom`: [`race_plan_kind`] for a race
/// source, the level `level::level_for_zoom` picks otherwise.
#[must_use]
pub fn plan_kind(source: &dyn GridCountSource, zoom: f64) -> PlanKind {
    plan_kind_after(source, zoom, None)
}

/// [`plan_kind`], a race source's crossover with hysteresis
/// ([`race_plan_kind_after`]).
#[must_use]
pub fn plan_kind_after(source: &dyn GridCountSource, zoom: f64, parts_before: Option<bool>) -> PlanKind {
    match source.race() {
        Some(r) => race_plan_kind_after(r, zoom, parts_before),
        None => PlanKind::Cells { level: level::level_for_zoom(source.descriptor(), zoom) },
    }
}

/// How far, in part tiles, a block part may reach past its owner tile's
/// edge on each side (Task 5 review F4): the converter caps a part's
/// bounding box at 2^17 of the 2^19 quanta of a z12 tile, and a vertex
/// is at most 2/3 of that from the centroid that picks the owner. A part
/// tile is never clipped to its own rect; the cover is grown by this much
/// instead ([`part_ring`]).
pub const PART_OVERHANG_TILES: f64 = 1.0 / 6.0;

/// Most part tiles one frame of a layer newly selects (review F1,
/// controller ruling). A part tile decodes to ~80 B a part -- the real
/// Chicago z12 tile is 8,106 parts, ~650 KB before its blocks and texture,
/// and the codec allows 2^19 -- and a wanted tile is never evicted, so an
/// uncapped cover (a 4K window at z9.72 reaches `MAX_COVER_TILES`, 512)
/// held up to a gigabyte. Parts are chosen a whole cell block (the
/// level-0 cell tile's area) at a time, blocks nearest the camera first
/// ([`rank_part_cover`]); the rest of the view falls back to the grid's
/// categorical cells, which carry the same people ([`plan_parts`]).
/// [`DotStats::part_fallback_tiles`] counts the part-zoom tiles drawn as
/// cells instead.
pub const MAX_PART_TILES_PER_FRAME: usize = 256;

/// The hard ceiling on a frame's part tiles, ring and all: a block chosen
/// last frame stays chosen while the part tiles ranked before it and its
/// own stay within this (cap-boundary hysteresis, review r1: the seam
/// between parts and cells does not move frame to frame), and the
/// overhang ring and a promotion ([`plan_parts`]) never pass it.
pub const PART_TILES_CEILING: usize = 288;

/// Rank for draw order and stand-in order, coarse (high) first: a part
/// key is finer than every cell level (its `level` is the
/// [`GridTileKey::PART_LEVEL`] sentinel, C7, and must not sort as the
/// coarsest).
fn draw_rank(k: &GridTileKey) -> i16 {
    if k.is_part() {
        -1
    } else {
        i16::from(k.level)
    }
}

/// [`plan_dots`] for a [`PlanKind`]. With [`PlanKind::Parts`]:
/// - a cover tile at or past the part zoom is its part-zoom ancestor's
///   part key (overzoom), a cell block at a time, within
///   [`MAX_PART_TILES_PER_FRAME`] ([`select_parts`]);
/// - every other tile draws the grid's categorical cells, never parts (the
///   far field of a tilted view needs no per-block triangles, as a
///   MapLibre LOD tile draws its own zoom's data): a block over the cap or
///   holding far-field tiles draws its level-0 cell tile, a tile at or
///   coarser than the cell base its own zoom's cell tile -- each with
///   exclusion boxes over the overhang of the part tiles drawn beside it
///   ([`strip_groups`]: at most a strip [`PART_OVERHANG_TILES`] wide of
///   its cells dropped, never a strip drawn twice);
/// - a part key's parent, wanted first so a stand-in exists, is its
///   level-0 cell tile; a missing part tile is stood in for by its
///   nearest held cell ancestor (level 0 up), with exclusion boxes over
///   the drawn part tiles inside it;
/// - a cell STAND-IN (for a part tile still loading) excludes cells by
///   their SW corner, while parts are owned by centroid and reach up to
///   [`PART_OVERHANG_TILES`] past their tile: next to a drawn part tile, a
///   band that thin can draw twice (and a missing neighbour's overhang is
///   a hole) until the neighbour lands -- transient only, as MapLibre's
///   parent stand-ins overlap (review F3); the cap's and the far field's
///   cell regions are not stand-ins and are boxed as above. A part tile
///   whose fetch keeps failing keeps its band through each backoff;
///   `DotStats::source_errors` shows it;
/// - a part key has no pyramid level: nothing derives a zoom from its
///   `level` (C7), it is never searched below, and it draws after every
///   cell tile ([`draw_rank`]).
pub fn plan_dots_kind(
    d: &GridDescriptor,
    kind: PlanKind,
    epoch: u16,
    epoch_b: Option<u16>,
    cover: &[TileId],
    ready: &dyn Fn(&GridTileKey) -> bool,
) -> DotPlan {
    match kind {
        PlanKind::Cells { level } => plan_inner(d, level.min(d.levels - 1), None, epoch, epoch_b, cover, ready),
        PlanKind::Parts { data_zoom } => plan_parts(d, data_zoom, epoch, epoch_b, cover, &[], &HashSet::new(), ready),
    }
}

/// A part plan ([`PlanKind::Parts`] at `pz`) with its overhang `ring`
/// ([`part_ring`]) and the blocks chosen last frame (`sticky`, the
/// hysteresis of [`PART_TILES_CEILING`]). See [`plan_dots_kind`] and
/// [`select_parts`].
#[allow(clippy::too_many_arguments)]
pub fn plan_parts(
    d: &GridDescriptor,
    pz: u8,
    epoch: u16,
    epoch_b: Option<u16>,
    cover: &[TileId],
    ring: &[TileId],
    sticky: &HashSet<TileId>,
    ready: &dyn Fn(&GridTileKey) -> bool,
) -> DotPlan {
    let selection = select_parts(d, pz, cover, ring, sticky);
    plan_inner(d, 0, Some((pz, &selection)), epoch, epoch_b, cover, ready)
}

/// What a part plan draws as parts and what as cells ([`select_parts`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartSelection {
    /// Part-zoom tiles drawn as parts, cover order then ring.
    pub parts: Vec<TileId>,
    /// Level-0 cell tiles (the cell blocks, at `max_data_zoom`) drawn as
    /// cells: blocks over the cap, and blocks holding a tilted view's far
    /// field between the cell base and the part zoom.
    pub cell_blocks: Vec<TileId>,
    /// Cover tiles at or coarser than the cell base: their own cell tiles.
    pub coarse: Vec<TileId>,
    /// The blocks drawn as parts (next frame's `sticky`).
    pub part_blocks: Vec<TileId>,
    /// Cover tiles at or past the part zoom, and far-field tiles, drawn by
    /// cells instead of parts.
    pub fallback: u32,
}

/// Which of a part plan's cover draws as parts (C7, review F1 and its
/// round 1):
/// - the unit is the cell block, a level-0 cell tile's area: a block is
///   all parts or all cells, so the cells' only border with parts is a
///   block edge, where [`strip_groups`] can box it;
/// - a block holding any far-field tile (between the cell base and the
///   part zoom, from a tilted view's LOD) is cells;
/// - the rest, nearest first in cover order, are parts while the part
///   tiles so far stay within [`MAX_PART_TILES_PER_FRAME`], or
///   [`PART_TILES_CEILING`] for a block in `sticky`;
/// - the ring's tiles are parts when their block is, or has no cover tile
///   (off screen), within the ceiling; never inside a coarse cover tile;
/// - a cell region whose borders with drawn parts need more than the two
///   exclusion boxes a draw has (a notch in the parts) is promoted to
///   parts within the ceiling, or else its part neighbours go to cells --
///   so every cell draw boxes out every overhang that reaches it.
pub fn select_parts(d: &GridDescriptor, pz: u8, cover: &[TileId], ring: &[TileId], sticky: &HashSet<TileId>) -> PartSelection {
    let mdz = d.max_data_zoom;
    let block = |t: TileId| ancestor(t, mdz);
    let mut blocks: Vec<TileId> = Vec::new();
    let mut block_tiles: HashMap<TileId, Vec<TileId>> = HashMap::new();
    let mut forced: HashSet<TileId> = HashSet::new();
    let mut coarse: Vec<TileId> = Vec::new();
    let mut seen = HashSet::new();
    let mut far = 0u32;
    for &t in cover {
        if t.z >= pz {
            let pt = ancestor(t, pz);
            if seen.insert(pt) {
                let b = block(pt);
                let tiles = block_tiles.entry(b).or_default();
                if tiles.is_empty() {
                    blocks.push(b);
                }
                tiles.push(pt);
            }
        } else if t.z > mdz {
            far += 1;
            if forced.insert(block(t)) && !block_tiles.contains_key(&block(t)) {
                blocks.push(block(t));
                block_tiles.insert(block(t), Vec::new());
            }
        } else if !coarse.contains(&t) {
            coarse.push(t);
        }
    }
    let n_of = |b: &TileId| block_tiles.get(b).map_or(0, Vec::len);
    let mut selected: HashSet<TileId> = HashSet::new();
    let mut count = 0usize;
    for b in &blocks {
        if forced.contains(b) {
            continue;
        }
        let limit = if sticky.contains(b) { PART_TILES_CEILING } else { MAX_PART_TILES_PER_FRAME };
        if count + n_of(b) <= limit {
            selected.insert(*b);
            count += n_of(b);
        }
    }
    let in_coarse = |t: TileId| coarse.iter().any(|&c| is_descendant(t, c));
    let mut banned: HashSet<TileId> = HashSet::new();
    let mut demoting = false;
    let mut planned: Vec<TileId>;
    let mut rounds = 0;
    loop {
        rounds += 1;
        planned = blocks.iter().filter(|b| selected.contains(b)).flat_map(|b| block_tiles[b].iter().copied()).collect();
        let cells: Vec<TileId> =
            blocks.iter().filter(|b| !selected.contains(b)).copied().chain(coarse.iter().copied()).collect();
        let mut have: HashSet<TileId> = planned.iter().copied().collect();
        for &r in ring {
            // A ring tile is off screen (or in a part block): it draws only
            // for the overhang it brings into view, and never next to a
            // cell region -- there it would need a strip of its own, and
            // its overhang is at most a strip at the view's edge.
            let b = block(r);
            let ok = selected.contains(&b) || (!block_tiles.contains_key(&b) && !banned.contains(&b));
            if ok
                && !in_coarse(r)
                && planned.len() < PART_TILES_CEILING
                && cells.iter().all(|&c| strip_groups(c, pz, &[r]).is_empty())
                && have.insert(r)
            {
                planned.push(r);
            }
        }
        if rounds > 64 {
            // Cannot happen (promotions are bounded, then only demotions),
            // but all cells is always a valid plan.
            planned.clear();
            selected.clear();
            break;
        }
        let bad: Vec<TileId> = cells.iter().copied().filter(|&c| strip_groups(c, pz, &planned).len() > 2).collect();
        if bad.is_empty() {
            break;
        }
        for c in bad {
            let promotable = c.z == mdz && block_tiles.contains_key(&c) && !forced.contains(&c) && !selected.contains(&c);
            if !demoting && promotable && count + n_of(&c) <= PART_TILES_CEILING {
                selected.insert(c);
                count += n_of(&c);
                continue;
            }
            demoting = true;
            for p in planned.iter().filter(|&&p| !strip_groups(c, pz, &[p]).is_empty()) {
                let b = block(*p);
                if selected.remove(&b) {
                    count -= n_of(&b);
                } else {
                    banned.insert(b);
                }
            }
        }
    }
    let part_blocks: Vec<TileId> = blocks.iter().filter(|b| selected.contains(b)).copied().collect();
    let cell_blocks: Vec<TileId> = blocks.iter().filter(|b| !selected.contains(b)).copied().collect();
    let fallback = far + cell_blocks.iter().map(|b| n_of(b) as u32).sum::<u32>();
    PartSelection { parts: planned, cell_blocks, coarse, part_blocks, fallback }
}

/// The exclusion boxes a cell tile `c` needs against drawn part tiles
/// `parts` (at zoom `pz`) outside it, as `[x0, y0, x1, y1]` in part-tile
/// units (unwrapped to `c`'s side of the antimeridian): each part tile's
/// rect grown by [`PART_OVERHANG_TILES`], merged by the side of `c` it
/// borders (a corner joins a side's box when there is one). Over those
/// boxes `c`'s cells are not drawn: a part reaching into `c` never draws
/// over a cell of `c` (review r1 HIGH). The error is bounded: `c` drops at
/// most a strip [`PART_OVERHANG_TILES`] of a part tile wide along the
/// edges it shares with drawn parts -- the cap's and the far field's
/// boundary only; a flat view under the cap has no cell region.
fn strip_groups(c: TileId, pz: u8, parts: &[TileId]) -> Vec<[f64; 4]> {
    let s = pz - c.z;
    let n = f64::from(1u32 << pz);
    let (cx0, cy0) = (f64::from(c.x << s), f64::from(c.y << s));
    let side = f64::from(1u32 << s);
    let (cx1, cy1) = (cx0 + side, cy0 + side);
    let g = PART_OVERHANG_TILES;
    let union = |a: [f64; 4], b: [f64; 4]| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])];
    // Sides: 0 W, 1 E, 2 N, 3 S; corners 4 + (h, v).
    let mut sides: [Option<[f64; 4]>; 4] = [None; 4];
    let mut corners: Vec<(usize, usize, [f64; 4])> = Vec::new();
    for p in parts {
        for shift in [0.0, -n, n] {
            let (px, py) = (f64::from(p.x) + shift, f64::from(p.y));
            let grown = [px - g, py - g, px + 1.0 + g, py + 1.0 + g];
            if !(grown[0] < cx1 && grown[2] > cx0 && grown[1] < cy1 && grown[3] > cy0) {
                continue;
            }
            let h = if px + 1.0 <= cx0 { Some(0) } else if px >= cx1 { Some(1) } else { None };
            let v = if py + 1.0 <= cy0 { Some(2) } else if py >= cy1 { Some(3) } else { None };
            match (h, v) {
                (Some(i), None) | (None, Some(i)) => sides[i] = Some(sides[i].map_or(grown, |b| union(b, grown))),
                (Some(hh), Some(vv)) => corners.push((hh, vv, grown)),
                (None, None) => {} // inside `c`: not a cell region's neighbour
            }
        }
    }
    let mut extra: Vec<([usize; 2], [f64; 4])> = Vec::new();
    for (hh, vv, grown) in corners {
        if let Some(b) = sides[hh].as_mut() {
            *b = union(*b, grown);
        } else if let Some(b) = sides[vv].as_mut() {
            *b = union(*b, grown);
        } else if let Some(e) = extra.iter_mut().find(|e| e.0 == [hh, vv]) {
            e.1 = union(e.1, grown);
        } else {
            extra.push(([hh, vv], grown));
        }
    }
    sides.iter().flatten().copied().chain(extra.into_iter().map(|e| e.1)).collect()
}

/// [`strip_groups`]' boxes in `c`'s own lyon units, as a draw's two
/// exclusion boxes, each grown by `margin` part tiles on every side: a
/// cell is cut by its SW corner and reaches one cell north-east of it, so
/// the margin is one of `c`'s cells (at its most stretched latitude) --
/// the strip a cell region drops is at most [`PART_OVERHANG_TILES`] plus
/// one cell wide. Any group past two (a transient load order; the plan
/// never needs more) is merged into the second.
fn strip_boxes(c: &GridTileKey, pz: u8, parts: &[TileId], margin: f64) -> [[f32; 4]; 2] {
    let groups: Vec<[f64; 4]> = strip_groups(c.tile, pz, parts)
        .into_iter()
        .map(|b| [b[0] - margin, b[1] - margin, b[2] + margin, b[3] + margin])
        .collect();
    let mut out = [DotParams::NONE_EXCLUDED; 2];
    let n = f64::from(1u32 << pz);
    let z = f64::from(1u32 << c.tile.z);
    let (ox, oy) = (f64::from(c.tile.x) / z, f64::from(c.tile.y) / z);
    let l = blocks::OVERLAY_LYON_PER_UNIT_WORLD;
    let merged = match groups.len() {
        0 => return out,
        1 | 2 => groups,
        _ => {
            let rest = groups[1..].iter().fold(groups[1], |a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]);
            vec![groups[0], rest]
        }
    };
    for (slot, b) in merged.iter().enumerate() {
        out[slot] =
            [((b[0] / n - ox) * l) as f32, ((b[1] / n - oy) * l) as f32, ((b[2] / n - ox) * l) as f32, ((b[3] / n - oy) * l) as f32];
    }
    out
}

/// [`plan_dots_kind`]'s body: a cells plan at `level`, or -- with `part`
/// -- a part plan at its zoom drawing `part`'s selection.
fn plan_inner(
    d: &GridDescriptor,
    level: u8,
    part: Option<(u8, &PartSelection)>,
    epoch: u16,
    epoch_b: Option<u16>,
    cover: &[TileId],
    ready: &dyn Fn(&GridTileKey) -> bool,
) -> DotPlan {
    let coarsest = d.levels - 1;
    let key = |tile: TileId, level: u8| GridTileKey { tile, level, epoch, epoch_b };
    let part_key = |tile: TileId| GridTileKey { tile, level: GridTileKey::PART_LEVEL, epoch, epoch_b };
    let dz = grid::data_zoom(d, level);

    // 1. The cover as data keys, deduplicated in cover order; a part plan
    //    draws its selection's part tiles, its cell blocks' level-0 tiles
    //    and its coarse tiles' own cell tiles.
    let mut cover_keys: Vec<GridTileKey> = Vec::new();
    let mut seen = HashSet::new();
    let part_cover: Vec<TileId>;
    let cover = if let Some((_, sel)) = part {
        for &t in &sel.parts {
            if seen.insert(part_key(t)) {
                cover_keys.push(part_key(t));
            }
        }
        for &b in &sel.cell_blocks {
            if seen.insert(key(b, 0)) {
                cover_keys.push(key(b, 0));
            }
        }
        part_cover = sel.coarse.clone();
        &part_cover[..]
    } else {
        cover
    };
    for &t in cover {
        let mut push = |k: GridTileKey| {
            if seen.insert(k) {
                cover_keys.push(k);
            }
        };
        if t.z >= dz {
            push(key(ancestor(t, dz), level));
        } else if d.max_data_zoom - t.z <= coarsest {
            push(key(t, d.max_data_zoom - t.z));
        } else {
            // Coarser than the coarsest level: its tiles at that level.
            let cz = grid::data_zoom(d, coarsest);
            let n = 1u32 << (cz - t.z);
            for dy in 0..n {
                for dx in 0..n {
                    push(key(TileId { z: cz, x: t.x * n + dx, y: t.y * n + dy }, coarsest));
                }
            }
        }
    }

    // 2. Parents first, then the cover.
    let mut wanted = Vec::with_capacity(cover_keys.len() * 2);
    let mut wanted_seen = HashSet::new();
    for k in &cover_keys {
        if k.is_part() {
            // The cell tile a part tile's people also live in (Task 5
            // review F4(d)): level 0, at the grid's base zoom.
            let parent = key(ancestor(k.tile, d.max_data_zoom), 0);
            if wanted_seen.insert(parent) {
                wanted.push(parent);
            }
        } else if k.level < coarsest {
            let parent = key(ancestor(k.tile, k.tile.z - 1), k.level + 1);
            if wanted_seen.insert(parent) {
                wanted.push(parent);
            }
        }
    }
    for k in &cover_keys {
        if wanted_seen.insert(*k) {
            wanted.push(*k);
        }
    }

    // 3. Draws: ready cover keys, and each missing one's nearest ready
    //    ancestor.
    struct Item {
        key: GridTileKey,
        standin: bool,
        kept: bool,
        exclude: [[f32; 4]; 2],
    }
    let mut items: Vec<Item> = Vec::new();
    let mut standing_in: HashMap<GridTileKey, Vec<TileId>> = HashMap::new();
    for k in &cover_keys {
        if ready(k) {
            items.push(Item { key: *k, standin: false, kept: true, exclude: [DotParams::NONE_EXCLUDED; 2] });
            continue;
        }
        let first = if k.is_part() { 0 } else { k.level + 1 };
        let found = (first..=coarsest).map(|l| key(ancestor(k.tile, grid::data_zoom(d, l)), l)).find(|a| ready(a));
        if let Some(a) = found {
            let missing = standing_in.entry(a).or_default();
            if missing.is_empty() {
                items.push(Item { key: a, standin: true, kept: true, exclude: [DotParams::NONE_EXCLUDED; 2] });
            }
            missing.push(k.tile);
        } else {
            // No ancestor held (a zoom-out of more than one level before
            // the new level lands): the held DESCENDANTS draw instead,
            // the coarsest held one on each branch -- MapLibre's
            // `findLoadedChildren` retain. They cannot overlap each other
            // (one per branch) or any stand-in (a stand-in over them would
            // be an ancestor of `k`, and there is none), so they draw
            // with no boxes.
            let mut stack = vec![(*k, MAX_CHILD_FALLBACK_LEVELS)];
            while let Some((at, depth)) = stack.pop() {
                // A part key has no finer level; a level-0 key none either.
                if at.is_part() || at.level == 0 || depth == 0 {
                    continue;
                }
                for child in children_of(at.tile) {
                    let c = key(child, at.level - 1);
                    if ready(&c) {
                        items.push(Item { key: c, standin: true, kept: true, exclude: [DotParams::NONE_EXCLUDED; 2] });
                    } else {
                        stack.push((c, depth - 1));
                    }
                }
            }
        }
    }

    // 4. Each stand-in's boxes, coarsest first: a drawn tile no box can
    //    hold is dropped, and the stand-in draws its cells.
    let mut order: Vec<usize> = (0..items.len()).filter(|&i| items[i].standin).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(draw_rank(&items[i].key)));
    for s in order {
        if !items[s].kept {
            continue;
        }
        let s_tile = items[s].key.tile;
        let inside: Vec<usize> =
            (0..items.len()).filter(|&i| i != s && items[i].kept && is_descendant(items[i].key.tile, s_tile)).collect();
        if inside.is_empty() {
            continue;
        }
        let drawn: Vec<TileId> = inside.iter().map(|&i| items[i].key.tile).collect();
        let missing = standing_in.get(&items[s].key).cloned().unwrap_or_default();
        let (depth, rects) = exclusion_rects(s_tile, &drawn, &missing).unwrap_or((0, Vec::new()));
        for (slot, rect) in rects.iter().enumerate() {
            items[s].exclude[slot] = exclusion_box(d, &items[s].key, depth, *rect);
        }
        for &i in &inside {
            let cells = tile_cells(s_tile, items[i].key.tile, depth);
            let held = rects.iter().any(|r| depth > 0 && contains(r, &cells));
            if !held {
                items[i].kept = false;
            }
        }
    }

    // 5. A part plan's cell draws box out the overhang of the part tiles
    //    drawn next to them ([`strip_groups`]).
    if let Some((pz, _)) = part {
        let drawn: Vec<TileId> = items.iter().filter(|i| i.kept && i.key.is_part()).map(|i| i.key.tile).collect();
        for item in items.iter_mut().filter(|i| i.kept && !i.standin && !i.key.is_part()) {
            // One of the tile's cells, in part tiles, at its most stretched
            // latitude (Mercator y grows as 1 / cos(lat)).
            let bounds = crate::geo::tile_bounds_lonlat(item.key.tile);
            let lat = bounds.max_lat.abs().max(bounds.min_lat.abs()).min(85.0).to_radians();
            let cell = grid::cell_arcsec(d, item.key.level) / 3600.0 / 360.0 * f64::from(1u32 << pz);
            item.exclude = strip_boxes(&item.key, pz, &drawn, cell / lat.cos());
        }
    }

    items.sort_by_key(|item| std::cmp::Reverse(draw_rank(&item.key)));
    let draw = items
        .into_iter()
        .filter(|item| item.kept)
        .map(|item| DotDraw { key: item.key, exclude: item.exclude, standin: item.standin })
        .collect();
    let (part_fallback, part_blocks) = part.map_or((0, Vec::new()), |(_, sel)| (sel.fallback, sel.part_blocks.clone()));
    DotPlan { wanted, draw, part_fallback, part_blocks }
}

/// `t`'s cells `[x0, y0, x1, y1)` in the grid `depth` levels below `s`.
fn tile_cells(s: TileId, t: TileId, depth: u8) -> [u32; 4] {
    let dt = t.z - s.z;
    if dt > depth {
        return [0, 0, 0, 0];
    }
    let scale = depth - dt;
    let x0 = (t.x - (s.x << dt)) << scale;
    let y0 = (t.y - (s.y << dt)) << scale;
    [x0, y0, x0 + (1 << scale), y0 + (1 << scale)]
}

fn contains(r: &[u32; 4], c: &[u32; 4]) -> bool {
    c[0] >= r[0] && c[1] >= r[1] && c[2] <= r[2] && c[3] <= r[3] && c[0] < c[2]
}

fn disjoint(r: &[u32; 4], c: &[u32; 4]) -> bool {
    c[2] <= r[0] || c[0] >= r[2] || c[3] <= r[1] || c[1] >= r[3]
}

/// Up to two rectangles of cells, `depth` levels below stand-in `s`, that
/// each hold whole drawn tiles and touch no missing tile, chosen to hold
/// as much drawn area as possible. `None` when the tiles are too deep or
/// too many to arrange (`MAX_EXCLUSION_*`).
///
/// A rectangle's edges can be taken from the drawn tiles' own edges: any
/// valid rectangle shrinks to the bounding box of the drawn tiles it
/// holds and stays valid, so the search is over those edges only.
fn exclusion_rects(s: TileId, drawn: &[TileId], missing: &[TileId]) -> Option<(u8, Vec<[u32; 4]>)> {
    let depth = drawn.iter().chain(missing).map(|t| t.z - s.z).max().unwrap_or(0);
    if depth == 0 || depth > MAX_EXCLUSION_DEPTH || drawn.len() > MAX_EXCLUSION_TILES {
        return None;
    }
    let drawn: Vec<[u32; 4]> = drawn.iter().map(|&t| tile_cells(s, t, depth)).collect();
    let missing: Vec<[u32; 4]> = missing.iter().map(|&t| tile_cells(s, t, depth)).collect();
    let mut xs0: Vec<u32> = drawn.iter().map(|c| c[0]).collect();
    let mut ys0: Vec<u32> = drawn.iter().map(|c| c[1]).collect();
    let mut xs1: Vec<u32> = drawn.iter().map(|c| c[2]).collect();
    let mut ys1: Vec<u32> = drawn.iter().map(|c| c[3]).collect();
    for v in [&mut xs0, &mut ys0, &mut xs1, &mut ys1] {
        v.sort_unstable();
        v.dedup();
    }
    let area = |c: &[u32; 4]| u64::from(c[2] - c[0]) * u64::from(c[3] - c[1]);
    let mut taken = vec![false; drawn.len()];
    let mut rects = Vec::new();
    for _ in 0..2 {
        let mut best: Option<([u32; 4], u64, u64)> = None;
        for &x0 in &xs0 {
            for &x1 in xs1.iter().filter(|&&x1| x1 > x0) {
                for &y0 in &ys0 {
                    for &y1 in ys1.iter().filter(|&&y1| y1 > y0) {
                        let r = [x0, y0, x1, y1];
                        if missing.iter().any(|m| !disjoint(&r, m)) {
                            continue;
                        }
                        let mut score = 0;
                        let mut valid = true;
                        for (c, &done) in drawn.iter().zip(&taken) {
                            if contains(&r, c) {
                                score += if done { 0 } else { area(c) };
                            } else if !disjoint(&r, c) {
                                valid = false;
                                break;
                            }
                        }
                        let better = best.is_none_or(|(_, s, a)| score > s || (score == s && area(&r) < a));
                        if valid && score > 0 && better {
                            best = Some((r, score, area(&r)));
                        }
                    }
                }
            }
        }
        let Some((r, _, _)) = best else { break };
        for (c, done) in drawn.iter().zip(taken.iter_mut()) {
            *done |= contains(&r, c);
        }
        rects.push(r);
    }
    Some((depth, rects))
}

/// A rectangle of cells (`depth` below stand-in `s`) as a `DotParams`
/// exclusion box in `s`'s lyon units.
///
/// The shader tests a block's ORIGIN -- its cell's SW corner -- and a cell
/// belongs to a tile by its CENTRE. So the stand-in cells whose centres
/// fall in the rectangle are found by `grid::cell_window`'s own row/col
/// rule (the rule every tile's cells are cut by), and the box runs half a
/// cell past the first and last of their SW corners: its edges fall
/// between lattice points, never on one, so f32 rounding cannot move a
/// cell across it.
fn exclusion_box(d: &GridDescriptor, s: &GridTileKey, depth: u8, r: [u32; 4]) -> [f32; 4] {
    let n = f64::from(1u32 << (s.tile.z + depth));
    let wx = |c: u32| (f64::from(s.tile.x << depth) + f64::from(c)) / n;
    let wy = |c: u32| (f64::from(s.tile.y << depth) + f64::from(c)) / n;
    let lon = |x: f64| x * 360.0 - 180.0;
    let lat = |y: f64| (std::f64::consts::PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
    let arc = grid::cell_arcsec(d, s.level) / 3600.0;
    let half = arc / 2.0;
    // `cell_window`: rows [row0, row_end), cols [col0, col_end).
    let first = |v: f64| (v / arc - 0.5).ceil();
    let (row0, row_end) = (first(d.origin_lat - lat(wy(r[1]))), first(d.origin_lat - lat(wy(r[3]))));
    let (col0, col_end) = (first(lon(wx(r[0])) - d.origin_lon), first(lon(wx(r[2])) - d.origin_lon));
    // SW corners: lat origin - (row + 1) arc, lon origin + col arc.
    let lat_top = d.origin_lat - (row0 + 1.0) * arc + half;
    let lat_bottom = d.origin_lat - row_end * arc - half;
    let lon_west = d.origin_lon + col0 * arc - half;
    let lon_east = d.origin_lon + (col_end - 1.0) * arc + half;
    let world = f64::from(crate::render::WORLD_SIZE);
    let to_y = |lat: f64| crate::overlay::geom::lon_lat_to_world(0.0, lat).1 / world;
    let to_x = |lon: f64| (lon + 180.0) / 360.0;
    let z = f64::from(1u32 << s.tile.z);
    let (ox, oy) = (f64::from(s.tile.x) / z, f64::from(s.tile.y) / z);
    let l = blocks::OVERLAY_LYON_PER_UNIT_WORLD;
    [
        ((to_x(lon_west) - ox) * l) as f32,
        ((to_y(lat_top) - oy) * l) as f32,
        ((to_x(lon_east) - ox) * l) as f32,
        ((to_y(lat_bottom) - oy) * l) as f32,
    ]
}

/// A short, fixed name for a failure: `DotStats::last_error`.
fn error_name(e: &GridError) -> &'static str {
    match e {
        GridError::Truncated => "truncated",
        GridError::BadMagic => "bad magic",
        GridError::BadVersion(_) => "bad version",
        GridError::IndexOutOfTile { .. } => "index out of tile",
        GridError::Inflate(_) => "inflate",
        GridError::Http { .. } => "http",
        GridError::Server(_) => "server",
        GridError::Io(_) => "io",
        GridError::Timeout(_) => "timeout",
    }
}

/// A decoded tile and the blocks built from it.
struct HeldTile {
    tile: Arc<GridTile>,
    /// [`slot_pop`] of the tile, once at landing.
    slot_pop: f64,
    /// Epoch B's people in the tile (`Σ pop_b`; `tile.total` without
    /// `epoch_b`), once at landing: the counter mixes `total` toward it.
    total_b: f64,
    blocks: Arc<TileBlocks>,
    /// Its cells for the counter, built on first need ([`HeldCells`]).
    index: HeldCells,
    /// The frame that last wanted or drew it (LRU).
    used: u64,
}

/// The counter's cell index of a held tile, built the first time a frame
/// needs it and dropped with the tile: only for a tile the view's boundary
/// cuts ([`in_view::Coverage::Edge`]) -- a tile inside the view counts its
/// [`GridTile::total`], one outside nothing. Built for every tile it cost
/// 12 bytes a cell (`world_dots --measure` peak RSS 175 -> 216-275 MB).
#[derive(Debug, Default)]
struct HeldCells {
    index: OnceLock<Arc<CellIndex>>,
}

impl From<Arc<CellIndex>> for HeldCells {
    fn from(index: Arc<CellIndex>) -> Self {
        Self { index: OnceLock::from(index) }
    }
}

impl HeldCells {
    fn index(&self, d: &GridDescriptor, key: &GridTileKey, tile: &GridTile) -> &CellIndex {
        self.index.get_or_init(|| Arc::new(CellIndex::new(tile, &grid::cell_window(d, key.level, key.tile))))
    }
}

struct DotLayer {
    id: String,
    source: Arc<dyn GridCountSource>,
    style: DotStyle,
    generation: u64,
    tiles: HashMap<GridTileKey, HeldTile>,
    fetching: HashSet<GridTileKey>,
    rebuilding: HashSet<GridTileKey>,
    requests: usize,
    /// When a key's request failed: stamped at the next frame (`land` has
    /// no clock), then asked for again after `FAILED_BACKOFF`.
    failed: HashMap<GridTileKey, Option<Instant>>,
    ppd: PpdController,
    seeded: bool,
    last_frame: Option<Instant>,
    view_ppd: f32,
    zoom: f64,
    plan: DotPlan,
    frame: u64,
    stats: DotStats,
    /// The last `people_in_view` and what it was computed from: reused
    /// while the view, the draws and their tiles are all unchanged.
    view_cache: ViewCache,
    /// The epochs drawn before the last [`DotLayers::set_epochs`], while
    /// the new pair's tiles are still landing: their held tiles stand in
    /// (review M-1: no blank frame, no ppd dip). See [`Standing`] for
    /// when it is released.
    standing: Option<Standing>,
    /// The cell blocks the last part plan drew as parts: kept as parts up
    /// to [`PART_TILES_CEILING`] (hysteresis).
    part_blocks: HashSet<TileId>,
    /// Whether the last planned frame drew parts ([`race_plan_kind_after`]'s
    /// crossover hysteresis); `None` before the first.
    parts_before: Option<bool>,
}

/// Most frames an old epoch pair stands in after [`DotLayers::set_epochs`]
/// (R1-M-1): past this the new pair draws whatever it holds, with the
/// usual stand-ins and holes, however its fetches are going. About 5 s at
/// 120 Hz; frames rather than wall time so a test can step it exactly.
pub const STANDING_MAX_FRAMES: u32 = 600;

/// An old epoch pair standing in while the new one lands. Released --
/// and every held tile of any other pair dropped, so memory is one pair
/// again -- at the first frame when every key the new pair wants is
/// either held or has FAILED since the step (a key erroring every time
/// must not hold two pairs forever; it goes on retrying after its backoff
/// as any failed key does), or after [`STANDING_MAX_FRAMES`] frames.
#[derive(Debug, Clone, PartialEq)]
struct Standing {
    pair: (u16, Option<u16>),
    frames: u32,
    /// New-pair keys whose fetch failed since the step: not waited for.
    failed: HashSet<GridTileKey>,
}

/// What [`DotLayer::view_cache`] was computed from.
#[derive(Default)]
struct ViewCache {
    view: Option<ViewRegion>,
    draw: Vec<DotDraw>,
    /// The drawn tiles' decoded cells (`Arc::as_ptr`): a rebuild keeps
    /// them, a new fetch does not.
    tiles: Vec<usize>,
    /// The epoch mix it was counted at.
    t: f32,
    people: f64,
    /// Of `people`, those counted from part tiles: the ppd's budget input
    /// for them ([`budget_pop`]).
    part_people: f64,
    excluded: ExcludedCache,
}

/// A stand-in's excluded people ([`in_view::excluded_pop`]) by its key,
/// cells and boxes: they change only as finer tiles land, not as the
/// view moves.
type ExcludedCache = Vec<(GridTileKey, usize, [[f32; 4]; 2], (f64, f64))>;

impl DotLayer {
    /// The ppd a tile's blocks are built at and drawn with: the view's,
    /// the same for every tile (fix round 1). The budget already bounds
    /// every drawn tile together (`frame_with_cover`), so a tile needs no
    /// floor of its own -- the old `total / budget` floor let each of N
    /// tiles carry a whole budget, and drew tiles side by side at
    /// different ppds that the counter could not report.
    fn tile_ppd(&self) -> f32 {
        self.view_ppd.max(1.0)
    }
}

/// Every dot-density source of one map, in the order they were added.
#[derive(Default)]
pub struct DotLayers {
    layers: Vec<DotLayer>,
    next_generation: u64,
    stale_dropped: u64,
}

/// Work for a shell to run off its UI thread (or inline): a fetch and
/// decode of a batch of keys, or a block rebuild of a held tile at a new
/// ppd. Run it with [`DotJob::run`] and hand the result to
/// [`DotLayers::land`].
pub struct DotJob {
    layer: String,
    generation: u64,
    work: Work,
}

enum Work {
    Fetch { source: Arc<dyn GridCountSource>, keys: Vec<GridTileKey>, view_ppd: f32, budget: f64, seeded: bool },
    Rebuild {
        source: Arc<dyn GridCountSource>,
        key: GridTileKey,
        tile: Arc<GridTile>,
        view_ppd: f32,
        budget: f64,
    },
}

/// Who a job was for, kept by the shell while the job runs so that a job
/// that never returns a result (it panicked) can still be landed as a
/// failure with [`DotLanded::abandoned`], freeing its request slot.
#[derive(Debug, Clone)]
pub struct DotTicket {
    layer: String,
    generation: u64,
    keys: Vec<GridTileKey>,
    rebuild: bool,
}

/// A job's result, tagged with the source generation it was issued under.
pub struct DotLanded {
    layer: String,
    generation: u64,
    keys: Vec<GridTileKey>,
    rebuild: bool,
    result: Result<Vec<Built>, GridError>,
}

impl DotLanded {
    /// A failure for a job that returned nothing (its worker panicked):
    /// landing it frees the job's slot and backs its keys off, as any
    /// failed request does.
    #[must_use]
    pub fn abandoned(ticket: DotTicket) -> Self {
        Self {
            layer: ticket.layer,
            generation: ticket.generation,
            keys: ticket.keys,
            rebuild: ticket.rebuild,
            result: Err(GridError::Server("the dot job panicked".to_string())),
        }
    }
}

/// A tile a job built: its key, cells and blocks.
type Built = (GridTileKey, Arc<GridTile>, Arc<TileBlocks>);

fn build(
    source: &dyn GridCountSource,
    key: GridTileKey,
    tile: Arc<GridTile>,
    view_ppd: f32,
    budget: f64,
    floor_pop: f64,
) -> Built {
    // Never below `floor_pop / budget`: a job can run before the ppd is
    // seeded (the first fetches) or after it moved. `floor_pop` is the
    // tile's own total once seeded (one tile alone is never built over the
    // budget; the view's ppd is above that anyway, as it budgets every
    // drawn tile together) and, before, its batch's total times the
    // requests that can be in flight, so the first fetches together stay
    // within about √2 budgets of slots (fix round 3).
    let ppd = view_ppd.max((floor_pop / budget.max(1.0)) as f32).max(1.0);
    let ppd_build = level::ppd_build_for(ppd);
    let blocks = blocks::build_blocks(source.descriptor(), &key, &tile, ppd_build, blocks::OVERLAY_LYON_PER_UNIT_WORLD);
    (key, tile, Arc::new(blocks))
}

impl DotJob {
    /// Who the job is for: keep it while the job runs, and land
    /// [`DotLanded::abandoned`] with it if the job never returns.
    #[must_use]
    pub fn ticket(&self) -> DotTicket {
        let (keys, rebuild) = match &self.work {
            Work::Fetch { keys, .. } => (keys.clone(), false),
            Work::Rebuild { key, .. } => (vec![*key], true),
        };
        DotTicket { layer: self.layer.clone(), generation: self.generation, keys, rebuild }
    }

    /// The IO the job needs, if any (a rebuild needs none).
    #[must_use]
    pub fn request(&self) -> Option<GridRequest> {
        match &self.work {
            Work::Fetch { source, keys, .. } => Some(source.request(keys)),
            Work::Rebuild { .. } => None,
        }
    }

    /// Runs the job: `fetch` turns the request into bytes (native:
    /// `fetch_native::execute`), then the tiles are decoded and their
    /// blocks built. A key the source did not return is an empty tile.
    pub fn run(self, fetch: impl FnOnce(&GridRequest) -> Result<Vec<u8>, GridError>) -> DotLanded {
        let (keys, rebuild, result) = match self.work {
            Work::Fetch { source, keys, view_ppd, budget, seeded } => {
                let result = fetch(&source.request(&keys)).and_then(|body| source.decode(&keys, &body)).map(|tiles| {
                    let mut by_key: HashMap<GridTileKey, GridTile> = tiles.into_iter().collect();
                    let batch: f64 = by_key.iter().map(|(k, t)| slot_pop(k, t)).sum();
                    keys.iter()
                        .map(|k| {
                            let tile = Arc::new(by_key.remove(k).unwrap_or_default());
                            let floor = if seeded { build_floor_pop(k, &tile) } else { batch * MAX_REQUESTS_IN_FLIGHT as f64 };
                            build(&*source, *k, tile, view_ppd, budget, floor)
                        })
                        .collect()
                });
                (keys, false, result)
            }
            Work::Rebuild { source, key, tile, view_ppd, budget } => {
                let floor = build_floor_pop(&key, &tile);
                (vec![key], true, Ok(vec![build(&*source, key, tile, view_ppd, budget, floor)]))
            }
        };
        DotLanded { layer: self.layer, generation: self.generation, keys, rebuild, result }
    }
}

/// Runs `job` on the current thread, turning a panic inside it into
/// [`DotLanded::abandoned`] instead of losing the ticket -- the rule
/// `MapView::drive_dot_sources` and `ios::dots::DotWorker` both must
/// follow: a job whose run panics still lands (as a failure), or its
/// request slot is held forever. Native reaches the same end through its
/// runtime's own panic-catching (`tokio::task::JoinHandle`'s `Err` on
/// `spawn_blocking`) rather than by calling this directly, since it
/// already has a runtime and this crate does not depend on tokio; a shell
/// with no runtime -- iOS's `DotWorker`, running jobs on its own plain
/// `std::thread` workers -- calls it so the rule is exercised under
/// `cargo test`, which `ios/`'s own crate is not (see
/// `scripts/check-ios.sh`).
#[must_use]
pub fn run_job_catching_panic(job: DotJob, fetch: impl FnOnce(&GridRequest) -> Result<Vec<u8>, GridError>) -> DotLanded {
    let ticket = job.ticket();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run(fetch))).unwrap_or_else(|_| DotLanded::abandoned(ticket))
}

impl DotLayers {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Adds source `id`, or replaces it (a new generation: everything held
    /// or in flight for the old one is dropped, and lands nowhere).
    pub fn set_source(&mut self, id: &str, source: Arc<dyn GridCountSource>, style: DotStyle) {
        // A zero budget is held as one dot: every ppd divides by it.
        let style = DotStyle { budget_dots: style.budget_dots.max(1), ..style };
        self.next_generation += 1;
        let layer = DotLayer {
            id: id.to_string(),
            ppd: PpdController::new(f64::from(style.budget_dots)),
            source,
            style,
            generation: self.next_generation,
            tiles: HashMap::new(),
            fetching: HashSet::new(),
            rebuilding: HashSet::new(),
            requests: 0,
            failed: HashMap::new(),
            seeded: false,
            last_frame: None,
            view_ppd: 1.0,
            zoom: 0.0,
            plan: DotPlan::default(),
            frame: 0,
            stats: DotStats::default(),
            view_cache: ViewCache::default(),
            standing: None,
            part_blocks: HashSet::new(),
            parts_before: None,
        };
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(old) => *old = layer,
            None => self.layers.push(layer),
        }
    }

    /// Live epochs (Task 15): layer `id` draws `epoch`, or the pair
    /// `epoch`/`epoch_b` its [`DotStyle::epoch_mix`] moves between. The
    /// new keys are fetched as any others (the old ones age out of the
    /// LRU); nothing else about the layer is reset -- same generation,
    /// same ppd. True when the layer exists and its epochs changed.
    pub fn set_epochs(&mut self, id: &str, epoch: u16, epoch_b: Option<u16>) -> bool {
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(layer) if (layer.style.epoch, layer.style.epoch_b) != (epoch, epoch_b) => {
                // Stepping again before the last step landed keeps the
                // pair that is actually drawn standing in; stepping back
                // to it needs no stand-in.
                let drawn = layer.standing.as_ref().map_or((layer.style.epoch, layer.style.epoch_b), |s| s.pair);
                layer.standing =
                    (drawn != (epoch, epoch_b)).then(|| Standing { pair: drawn, frames: 0, failed: HashSet::new() });
                layer.style.epoch = epoch;
                layer.style.epoch_b = epoch_b;
                true
            }
            _ => false,
        }
    }

    /// Layer `id`'s mix from `epoch` to `epoch_b`, clamped to `[0, 1]`
    /// (NaN is 0). Changes only the draws' `DotParams::t`: no key, no
    /// fetch, no rebuild, no upload. True when the layer exists and the
    /// mix changed.
    pub fn set_epoch_mix(&mut self, id: &str, t: f32) -> bool {
        let t = clamp_mix(t);
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(layer) if layer.style.epoch_mix != t => {
                layer.style.epoch_mix = t;
                true
            }
            _ => false,
        }
    }

    /// Layer `id`'s colour ramp (e.g. [`DotRamp::Difference`]): the next
    /// sync makes the layer's paint table again; tiles and blocks stay.
    /// True when the layer exists and its ramp changed.
    pub fn set_ramp(&mut self, id: &str, ramp: DotRamp) -> bool {
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(layer) if layer.style.ramp != ramp => {
                layer.style.ramp = ramp;
                true
            }
            _ => false,
        }
    }

    /// Layer `id`'s legend toggles ([`DotStyle::category_mask`]): the next
    /// sync writes them to every drawn tile's `DotParams`; no fetch, no
    /// rebuild, no upload, and the ppd is unchanged (it budgets the tiles'
    /// whole population). True when the layer exists and its mask changed.
    pub fn set_category_mask(&mut self, id: &str, mask: u8) -> bool {
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(layer) if layer.style.category_mask != mask => {
                layer.style.category_mask = mask;
                true
            }
            _ => false,
        }
    }

    /// Layer `id`'s opacity ([`DotStyle::opacity`], clamped to `[0, 1]`,
    /// NaN is 1): the next sync makes the layer's paint table again. True
    /// when the layer exists and its opacity changed.
    pub fn set_opacity(&mut self, id: &str, opacity: f32) -> bool {
        let opacity = if opacity.is_nan() { 1.0 } else { opacity.clamp(0.0, 1.0) };
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(layer) if layer.style.opacity != opacity => {
                layer.style.opacity = opacity;
                true
            }
            _ => false,
        }
    }

    /// Layer `id`'s min zoom ([`DotStyle::min_zoom`]). True when the layer
    /// exists and its min zoom changed.
    pub fn set_min_zoom(&mut self, id: &str, min_zoom: f64) -> bool {
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(layer) if layer.style.min_zoom != min_zoom => {
                layer.style.min_zoom = min_zoom;
                true
            }
            _ => false,
        }
    }

    /// Layer `id`'s style as it is now.
    #[must_use]
    pub fn style(&self, id: &str) -> Option<&DotStyle> {
        self.layers.iter().find(|l| l.id == id).map(|l| &l.style)
    }

    /// Removes source `id`; true when there was one.
    pub fn remove_source(&mut self, id: &str) -> bool {
        let before = self.layers.len();
        self.layers.retain(|l| l.id != id);
        self.layers.len() != before
    }

    /// Every source's credit, deduplicated, in source order. A relocating
    /// layer's credit carries [`ILLUSTRATIVE_PLACEMENT_NOTE`] after it.
    #[must_use]
    pub fn attributions(&self) -> Vec<String> {
        // A credit any relocating layer carries gets the note wherever it
        // appears, so two layers sharing a credit read once in either order.
        let relocated: HashSet<&str> = self
            .layers
            .iter()
            .filter(|l| l.style.relocates())
            .map(|l| l.source.descriptor().attribution.trim())
            .collect();
        let mut out: Vec<String> = Vec::new();
        for layer in &self.layers {
            let credit = layer.source.descriptor().attribution.trim();
            if credit.is_empty() {
                continue;
            }
            let credit = if relocated.contains(credit) {
                format!("{credit}; {ILLUSTRATIVE_PLACEMENT_NOTE}")
            } else {
                credit.to_string()
            };
            if !out.contains(&credit) {
                out.push(credit);
            }
        }
        out
    }

    /// Whether any layer relocates its dots ([`DotStyle::relocates`]): the
    /// basemap builds its mask lists, and the map its mask, only then.
    #[must_use]
    pub fn wants_mask(&self) -> bool {
        self.layers.iter().any(|l| l.style.relocates())
    }

    /// The finest data cell a relocating layer draws this frame, in unit
    /// world (its width): what `dot_mask::relocation_region` measures on
    /// screen, and the margin the mask grows by. `None` when no relocating
    /// layer draws a tile.
    #[must_use]
    pub fn finest_relocating_cell(&self) -> Option<f64> {
        self.layers
            .iter()
            .filter(|l| l.style.relocates())
            .flat_map(|l| l.plan.draw.iter().filter_map(|d| l.tiles.get(&d.key)))
            .map(|held| f64::from(held.blocks.cell_width) / blocks::OVERLAY_LYON_PER_UNIT_WORLD)
            .filter(|w| *w > 0.0)
            .min_by(f64::total_cmp)
    }

    /// Every drawn tile's params for layer `id`, in draw order, as
    /// [`DotLayersGpu::sync`] writes them for a camera at `zoom` and
    /// `pitch` (radians), globe `transition`, against `mask`
    /// ([`tile_params`]; before the GPU fills the bucket runs). Empty for
    /// no such layer.
    #[must_use]
    pub fn frame_params(
        &self,
        id: &str,
        zoom: f64,
        pitch: f32,
        transition: f32,
        mask: Option<&crate::render::dot_mask::MaskGrid>,
    ) -> Vec<DotParams> {
        let Some(layer) = self.layers.iter().find(|l| l.id == id) else { return Vec::new() };
        layer
            .plan
            .draw
            .iter()
            .filter_map(|draw| {
                let held = layer.tiles.get(&draw.key)?;
                (held.blocks.slots > 0).then(|| {
                    tile_params(&layer.style, draw, &held.blocks, layer.tile_ppd(), zoom, pitch, transition, mask).0
                })
            })
            .collect()
    }

    /// Every drawn tile's params for layer `id` as `DotLayersGpu::sync`
    /// would write them at `zoom` against `mask`, flat and untilted.
    #[cfg(test)]
    fn draw_params(&self, id: &str, zoom: f64, transition: f32, mask: Option<&crate::render::dot_mask::MaskGrid>) -> Vec<DotParams> {
        self.frame_params(id, zoom, 0.0, transition, mask)
    }

    #[must_use]
    pub fn stats(&self, id: &str) -> Option<DotStats> {
        self.layers.iter().find(|l| l.id == id).map(|l| l.stats)
    }

    /// Source `id`'s plan for the last frame.
    #[must_use]
    pub fn plan(&self, id: &str) -> Option<&DotPlan> {
        self.layers.iter().find(|l| l.id == id).map(|l| &l.plan)
    }

    /// A decoded tile source `id` holds.
    #[must_use]
    pub fn tile(&self, id: &str, key: &GridTileKey) -> Option<Arc<GridTile>> {
        self.layers.iter().find(|l| l.id == id).and_then(|l| l.tiles.get(key)).map(|h| Arc::clone(&h.tile))
    }

    /// How long until the earliest failed key may be asked for again, or
    /// `None` when nothing is backing off. Nothing else wakes a map that
    /// is sitting still after a failure, so a shell sets a timer for this
    /// and repaints when it fires.
    #[must_use]
    pub fn next_retry(&self, now: Instant) -> Option<Duration> {
        self.layers
            .iter()
            .flat_map(|l| l.failed.values())
            .map(|at| at.map_or(FAILED_BACKOFF, |at| FAILED_BACKOFF.saturating_sub(now.duration_since(at))))
            .min()
    }

    /// Results dropped because their source was replaced or removed.
    #[must_use]
    pub fn stale_dropped(&self) -> u64 {
        self.stale_dropped
    }

    /// The keys source `id` holds (tests: which epochs are in memory).
    #[cfg(test)]
    fn held_keys(&self, id: &str) -> Vec<GridTileKey> {
        self.layers.iter().find(|l| l.id == id).map(|l| l.tiles.keys().copied().collect()).unwrap_or_default()
    }

    /// Decoded tiles source `id` holds.
    #[must_use]
    pub fn tiles_held(&self, id: &str) -> usize {
        self.layers.iter().find(|l| l.id == id).map_or(0, |l| l.tiles.len())
    }

    /// One frame for a camera: [`Self::frame_with_cover`] over the frame's
    /// cover at each source's data zoom (the globe's when it has one, the
    /// planar `flat_cover` otherwise -- `cover_plan::tile_cover`) and the
    /// viewport's Mercator box. True when there is work to hand out.
    pub fn frame(&mut self, camera: &crate::render::Camera, globe: &crate::render::globe::GlobeCover, now: Instant) -> bool {
        if self.layers.is_empty() {
            return false;
        }
        // Part tiles are capped nearest the eye first: the eye's ground
        // point on a flat frame (the cover walk's own ranking), the
        // camera's centre on the globe.
        let focus = if globe.is_globe() {
            let (x, y) = crate::overlay::geom::lon_lat_to_world(camera.center_lon, camera.center_lat);
            glam::DVec2::new(x, y)
        } else {
            crate::render::eye_ground_point(camera)
        };
        self.frame_focused(camera.zoom, |z| dot_cover(camera, globe, z), &ViewRegion::for_frame(camera, globe), Some(focus), now)
    }

    /// [`Self::frame_with_view`] with the view a unit-world Mercator box
    /// `min..max`.
    pub fn frame_with_cover(
        &mut self,
        zoom: f64,
        cover_at: impl Fn(u8) -> Vec<TileId>,
        viewport: ([f64; 2], [f64; 2]),
        now: Instant,
    ) -> bool {
        self.frame_with_view(zoom, cover_at, &ViewRegion::mercator_box(viewport.0, viewport.1), now)
    }

    /// One frame: plans every source at `zoom` from `cover_at(data_zoom)`,
    /// updates its ppd from the drawn tiles' population, counts the people
    /// `view` shows ([`DotStats::people_in_view`]), and evicts old tiles.
    /// True when [`Self::next_job`] has something to hand out.
    ///
    /// A part plan ranks its part-zoom tiles nearest the middle of the
    /// cover's first (for [`MAX_PART_TILES_PER_FRAME`]); [`Self::frame`]
    /// ranks them from the camera's eye.
    pub fn frame_with_view(
        &mut self,
        zoom: f64,
        cover_at: impl Fn(u8) -> Vec<TileId>,
        view: &ViewRegion,
        now: Instant,
    ) -> bool {
        self.frame_focused(zoom, cover_at, view, None, now)
    }

    /// [`Self::frame_with_view`], ranking part tiles by ground distance
    /// from `focus` (world units) when given.
    fn frame_focused(
        &mut self,
        zoom: f64,
        cover_at: impl Fn(u8) -> Vec<TileId>,
        view: &ViewRegion,
        focus: Option<glam::DVec2>,
        now: Instant,
    ) -> bool {
        let mut work = false;
        for layer in &mut self.layers {
            layer.frame += 1;
            layer.zoom = zoom;
            // Below its min zoom (C16) the layer plans nothing: no fetch,
            // no draw, and `DotLayersGpu::sync` frees its GPU copies. Its
            // ppd is held for when it comes back.
            if zoom < layer.style.min_zoom {
                layer.plan = DotPlan::default();
                layer.stats.people_in_view = 0.0;
                layer.stats.dots_in_view = 0;
                layer.stats.tiles_wanted = 0;
                layer.stats.tiles_ready = 0;
                layer.stats.standin_tiles = 0;
                layer.stats.part_fallback_tiles = 0;
                continue;
            }
            let d = layer.source.descriptor().clone();
            let kind = plan_kind_after(&*layer.source, zoom, layer.parts_before);
            layer.parts_before = Some(matches!(kind, PlanKind::Parts { .. }));
            let mut cover = cover_at(kind.cover_zoom(&d));
            let mut ring = Vec::new();
            if let PlanKind::Parts { data_zoom } = kind {
                cover = rank_part_cover(cover, data_zoom, focus);
                ring = part_ring(&cover, data_zoom, view);
            }
            let sticky = std::mem::take(&mut layer.part_blocks);
            let plan_for = |a: u16, b: Option<u16>, tiles: &HashMap<GridTileKey, HeldTile>| match kind {
                PlanKind::Parts { data_zoom } => {
                    plan_parts(&d, data_zoom, a, b, &cover, &ring, &sticky, &|k| tiles.contains_key(k))
                }
                PlanKind::Cells { .. } => plan_dots_kind(&d, kind, a, b, &cover, &|k| tiles.contains_key(k)),
            };
            let (epoch, epoch_b) = (layer.style.epoch, layer.style.epoch_b);
            let mut plan = plan_for(epoch, epoch_b, &layer.tiles);
            layer.part_blocks = plan.part_blocks.iter().copied().collect();
            // After `set_epochs`: until the new pair is all held, the old
            // pair's held tiles draw (its own plan, stand-ins and all) and
            // the new pair's keys are fetched.
            if let Some(standing) = &mut layer.standing {
                standing.frames += 1;
                let settled = plan.wanted.iter().all(|k| layer.tiles.contains_key(k) || standing.failed.contains(k));
                if settled || standing.frames > STANDING_MAX_FRAMES {
                    layer.standing = None;
                    // One pair in memory again: the old pair's tiles (and,
                    // through `keeps_gpu_copy`, their GPU copies) go.
                    let pair = (layer.style.epoch, layer.style.epoch_b);
                    layer.tiles.retain(|k, _| (k.epoch, k.epoch_b) == pair);
                } else {
                    let (a, b) = standing.pair;
                    let old = plan_for(a, b, &layer.tiles);
                    if !old.draw.is_empty() {
                        plan.draw = old.draw;
                    }
                }
            }
            for k in plan.wanted.iter().chain(plan.draw.iter().map(|x| &x.key)) {
                if let Some(held) = layer.tiles.get_mut(k) {
                    held.used = layer.frame;
                }
            }

            // The budget is the GPU's (MG-8 measured it as live dots
            // DRAWN), and a draw makes every drawn tile's whole population
            // alive, on screen or not: so the ppd budgets the drawn
            // tiles' population, eased down by the controller and raised
            // at once when a frame draws more (fix round 1: budgeting the
            // population on screen issued 12.6-17.3 M slots for 2.8 M).
            // The people in view are for the stats (the counter), and --
            // for part tiles only -- the budget ([`budget_pop`], T8 fix
            // round 0: part tiles never get finer than `part_zoom`).
            let (people, part_people) =
                cached_view_pop(&mut layer.view_cache, &d, &plan, &layer.tiles, view, layer.style.mix());
            let drawn = budget_pop(&plan, &layer.tiles, part_people);
            let dt = layer.last_frame.map_or(0.0, |last| now.duration_since(last).as_secs_f64());
            layer.last_frame = Some(now);
            if !layer.seeded {
                if drawn > 0.0 {
                    layer.view_ppd = layer.ppd.snap(drawn);
                    layer.seeded = true;
                }
            } else if drawn > 0.0 || plan.wanted.iter().all(|k| layer.tiles.contains_key(k) || layer.failed.contains_key(k)) {
                // Nothing drawn while the view's tiles are still loading
                // is a hole that says nothing about the view: hold the ppd
                // rather than ease it toward 1 and rebuild everything when
                // they land (M-1). A settled empty view (open ocean: every
                // wanted tile held, or failed) eases as before (R1-N-1).
                layer.view_ppd = layer.ppd.update(drawn, dt);
                if f64::from(layer.view_ppd) < drawn / f64::from(layer.style.budget_dots) {
                    layer.view_ppd = layer.ppd.snap(drawn);
                }
            }
            layer.stats.people_in_view = people;
            layer.stats.dots_in_view = (people / f64::from(layer.view_ppd.max(1.0))).round() as u64;

            for stamp in layer.failed.values_mut() {
                stamp.get_or_insert(now);
            }
            layer.failed.retain(|_, at| at.is_none_or(|at| now.duration_since(at) < FAILED_BACKOFF));

            // LRU: never a tile this frame wants or draws.
            if layer.tiles.len() > DECODED_TILES_PER_SOURCE {
                let mut old: Vec<(u64, GridTileKey)> =
                    layer.tiles.iter().filter(|(_, h)| h.used < layer.frame).map(|(k, h)| (h.used, *k)).collect();
                old.sort_unstable_by_key(|(used, _)| *used);
                let excess = layer.tiles.len() - DECODED_TILES_PER_SOURCE;
                for (_, k) in old.into_iter().take(excess) {
                    layer.tiles.remove(&k);
                }
            }

            let ready = plan.wanted.iter().filter(|k| layer.tiles.contains_key(k)).count();
            layer.stats.ppd = layer.view_ppd;
            layer.stats.level = match kind {
                PlanKind::Cells { level } => level,
                PlanKind::Parts { .. } => GridTileKey::PART_LEVEL,
            };
            layer.stats.tiles_wanted = plan.wanted.len() as u32;
            layer.stats.tiles_ready = ready as u32;
            layer.stats.standin_tiles = plan.draw.iter().filter(|x| x.standin).count() as u32;
            layer.stats.part_fallback_tiles = plan.part_fallback;
            layer.plan = plan;
            work |= Self::fetch_batch(layer, false).is_some() || Self::rebuild_key(layer).is_some();
        }
        work
    }

    /// The next keys `layer` would fetch: the first wanted key not held,
    /// in flight or backing off, and up to `max_batch` more of its level
    /// and epoch. Marks them in flight when `take`.
    fn fetch_batch(layer: &mut DotLayer, take: bool) -> Option<Vec<GridTileKey>> {
        if layer.requests >= MAX_REQUESTS_IN_FLIGHT {
            return None;
        }
        let max = layer.source.max_batch().max(1);
        let mut batch: Vec<GridTileKey> = Vec::new();
        for k in &layer.plan.wanted {
            if layer.tiles.contains_key(k) || layer.fetching.contains(k) || layer.failed.contains_key(k) {
                continue;
            }
            if batch.first().is_some_and(|f| f.level != k.level || f.epoch != k.epoch || f.epoch_b != k.epoch_b) {
                continue;
            }
            batch.push(*k);
            if batch.len() == max {
                break;
            }
        }
        if batch.is_empty() {
            return None;
        }
        if take {
            layer.fetching.extend(batch.iter().copied());
            layer.requests += 1;
        }
        Some(batch)
    }

    /// A drawn tile whose blocks were built for a ppd `needs_rebuild` says
    /// no longer fits, and that is not already rebuilding.
    fn rebuild_key(layer: &DotLayer) -> Option<GridTileKey> {
        if layer.requests >= MAX_REQUESTS_IN_FLIGHT {
            return None;
        }
        layer.plan.draw.iter().map(|x| x.key).find(|k| {
            !layer.rebuilding.contains(k)
                && layer.tiles.get(k).is_some_and(|h| level::needs_rebuild(h.blocks.ppd_build, layer.tile_ppd()))
        })
    }

    /// The next job, if any source has one: its fetches first (parents,
    /// then the cover), then its rebuilds.
    pub fn next_job(&mut self) -> Option<DotJob> {
        for layer in &mut self.layers {
            let budget = f64::from(layer.style.budget_dots);
            if let Some(keys) = Self::fetch_batch(layer, true) {
                return Some(DotJob {
                    layer: layer.id.clone(),
                    generation: layer.generation,
                    work: Work::Fetch {
                        source: Arc::clone(&layer.source),
                        keys,
                        view_ppd: layer.view_ppd,
                        budget,
                        seeded: layer.seeded,
                    },
                });
            }
            if let Some(key) = Self::rebuild_key(layer) {
                let tile = Arc::clone(&layer.tiles[&key].tile);
                layer.rebuilding.insert(key);
                layer.requests += 1;
                return Some(DotJob {
                    layer: layer.id.clone(),
                    generation: layer.generation,
                    work: Work::Rebuild { source: Arc::clone(&layer.source), key, tile, view_ppd: layer.view_ppd, budget },
                });
            }
        }
        None
    }

    /// Installs a job's result. False -- and counted in
    /// [`Self::stale_dropped`] -- when its source has since been replaced
    /// or removed: nothing of it is installed.
    pub fn land(&mut self, landed: DotLanded) -> bool {
        let Some(layer) = self.layers.iter_mut().find(|l| l.id == landed.layer && l.generation == landed.generation)
        else {
            self.stale_dropped += 1;
            return false;
        };
        layer.requests = layer.requests.saturating_sub(1);
        for k in &landed.keys {
            if landed.rebuild {
                layer.rebuilding.remove(k);
            } else {
                layer.fetching.remove(k);
            }
        }
        match landed.result {
            Ok(tiles) => {
                if landed.rebuild {
                    layer.stats.rebuilds_total += tiles.len() as u64;
                }
                for (key, tile, blocks) in tiles {
                    layer.failed.remove(&key);
                    // A rebuild keeps the tile's cells, and what the
                    // counter built of them.
                    let index = match layer.tiles.remove(&key) {
                        Some(old) if Arc::ptr_eq(&old.tile, &tile) => old.index,
                        _ => HeldCells::default(),
                    };
                    let slot_pop = slot_pop(&key, &tile);
                    let total_b = total_b(&key, &tile);
                    layer.tiles.insert(key, HeldTile { tile, slot_pop, total_b, blocks, index, used: layer.frame });
                }
            }
            Err(e) => {
                layer.stats.source_errors += 1;
                layer.stats.last_error = Some(error_name(&e));
                if let Some(standing) = &mut layer.standing {
                    standing.failed.extend(landed.keys.iter().copied());
                }
                for k in landed.keys {
                    layer.failed.insert(k, None);
                }
            }
        }
        true
    }
}

/// The frame's dot cover at data zoom `z`: the globe's cover, trimmed to
/// the tiles the view shows ([`globe_shows`]), or the planar
/// `flat_cover` on a flat frame.
fn dot_cover(camera: &crate::render::Camera, globe: &crate::render::globe::GlobeCover, z: u8) -> Vec<TileId> {
    let cover = crate::render::cover_plan::tile_cover(z, globe, || crate::render::cover_plan::flat_cover(camera, z).tiles);
    if globe.is_globe() {
        cover.into_iter().filter(|&t| globe_shows(globe, t)).collect()
    } else {
        cover
    }
}

/// A part plan's cover with its part-zoom tiles (those at `pz` or finer)
/// first, nearest `focus` first (ground distance, world units; stable), and
/// the rest after in cover order: [`plan_dots_kind`] makes part tiles of
/// the first [`MAX_PART_TILES_PER_FRAME`]. Without a focus, the middle of
/// the part-zoom tiles' bounding box.
fn rank_part_cover(mut cover: Vec<TileId>, pz: u8, focus: Option<glam::DVec2>) -> Vec<TileId> {
    let world = f64::from(crate::render::WORLD_SIZE);
    let centre = |t: &TileId| {
        let size = world / f64::from(1u32 << t.z);
        glam::DVec2::new((f64::from(t.x) + 0.5) * size, (f64::from(t.y) + 0.5) * size)
    };
    let focus = focus.or_else(|| {
        let (lo, hi) = cover.iter().filter(|t| t.z >= pz).map(centre).fold(
            (glam::DVec2::splat(f64::INFINITY), glam::DVec2::splat(f64::NEG_INFINITY)),
            |(lo, hi), c| (lo.min(c), hi.max(c)),
        );
        lo.x.is_finite().then(|| (lo + hi) * 0.5)
    });
    let Some(focus) = focus else { return cover };
    let rank = |t: &TileId| {
        if t.z >= pz {
            (0u8, crate::render::cover_plan::ground_distance_to_tile(*t, focus))
        } else {
            (1u8, 0.0)
        }
    };
    cover.sort_by(|a, b| {
        let (ra, rb) = (rank(a), rank(b));
        ra.0.cmp(&rb.0).then(ra.1.total_cmp(&rb.1))
    });
    cover
}

/// A part plan's overhang ring (C7, Task 5 review F4(b)): every `pz`
/// neighbour of a cover tile at or past `pz` (overzoomed tiles by their
/// `pz` ancestor) whose rect grown by [`PART_OVERHANG_TILES`] reaches
/// `view` -- a part owned by a tile just off screen can reach into it --
/// less the cover's own tiles and any inside a coarser cover tile (the far
/// field, which draws cells: review F9). [`select_parts`] decides which
/// are drawn, within [`PART_TILES_CEILING`].
fn part_ring(cover: &[TileId], pz: u8, view: &ViewRegion) -> Vec<TileId> {
    let z = pz;
    let parts: Vec<TileId> = cover.iter().filter(|t| t.z >= z).map(|&t| ancestor(t, z)).collect();
    let have: HashSet<TileId> = parts.iter().copied().collect();
    let coarse: Vec<TileId> = cover.iter().copied().filter(|t| t.z < z).collect();
    let n = 1i64 << z;
    let mut ring = Vec::new();
    let mut tried = HashSet::new();
    for t in &parts {
        for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
            let y = i64::from(t.y) + dy;
            if !(0..n).contains(&y) {
                continue;
            }
            let x = (i64::from(t.x) + dx).rem_euclid(n);
            let nb = TileId { z, x: x as u32, y: y as u32 };
            if have.contains(&nb) || !tried.insert(nb) || coarse.iter().any(|&c| is_descendant(nb, c)) {
                continue;
            }
            if in_view::part_coverage(nb, PART_OVERHANG_TILES, view) != in_view::Coverage::Outside {
                ring.push(nb);
            }
        }
    }
    ring
}

/// `cover` and its [`part_ring`] (tests).
#[cfg(test)]
fn grow_part_cover(cover: Vec<TileId>, pz: u8, view: &ViewRegion) -> Vec<TileId> {
    let ring = part_ring(&cover, pz, view);
    cover.into_iter().chain(ring).collect()
}

/// Levels below a globe cover tile [`globe_shows`] tests.
const GLOBE_CULL_DEPTH: u8 = 2;

/// Whether any part of `t` is on this globe frame, to a sixteenth of its
/// area: some descendant [`GLOBE_CULL_DEPTH`] levels down passes
/// `GlobeCover::tile_visible`.
///
/// The globe's cover tests each tile's bounding BALL against the frustum,
/// which keeps a margin of about one tile around the view (MapLibre's
/// `isTileVisible` does the same; for basemap tiles the margin is cheap).
/// A dot tile's cost is its population, so the margin was most of the
/// draw: India z5 in a 1280x800 window drew 34 tiles and 4.46 B people for
/// 1.28 B on screen (fix round 1). The test stays conservative -- a tile
/// with any visible point has a visible descendant, and `tile_visible`
/// never rejects a visible tile -- so nothing on screen is dropped; the
/// descendants' balls are 4x smaller, and so is the margin.
fn globe_shows(globe: &crate::render::globe::GlobeCover, t: TileId) -> bool {
    let n = 1u32 << GLOBE_CULL_DEPTH;
    let z = t.z + GLOBE_CULL_DEPTH;
    (0..n * n).any(|i| globe.tile_visible(TileId { z, x: t.x * n + i % n, y: t.y * n + i / n }))
}

/// Population of every tile the frame draws, whole: what a draw makes
/// alive at a ppd. A stand-in counts all of it -- its excluded cells'
/// slots are issued too.
#[cfg(test)]
fn drawn_pop(plan: &DotPlan, tiles: &HashMap<GridTileKey, HeldTile>) -> f64 {
    plan.draw.iter().filter_map(|x| tiles.get(&x.key)).map(|h| h.slot_pop).sum()
}

/// [`drawn_pop`] over the cell tiles alone: part tiles are budgeted by
/// their people in view instead ([`budget_pop`]).
fn drawn_cell_pop(plan: &DotPlan, tiles: &HashMap<GridTileKey, HeldTile>) -> f64 {
    plan.draw.iter().filter(|x| !x.key.is_part()).filter_map(|x| tiles.get(&x.key)).map(|h| h.slot_pop).sum()
}

/// How far past `budget_dots` the dots a frame DRAWS may go when part
/// tiles are budgeted by their people in view ([`budget_pop`]): at most 2x
/// (T8 review F1, controller ruling -- the window budget was sized for
/// frame time, MG-8 costs dots drawn, and 3x would break it).
pub const PART_DRAWN_OVER_BUDGET: f64 = 2.0;

/// The population the ppd budgets (controller ruling, T8 fix round 0 and
/// review F1): every drawn CELL tile whole (MG-8: a draw makes its whole
/// population alive, on screen or not), and the PART tiles' people in view
/// (`part_people`, by part centroid, as the counter counts them) -- but
/// never under [`PART_DRAWN_OVER_BUDGET`]'s share of everything drawn
/// whole, so the dots a frame draws stay within 2x `budget_dots`.
///
/// Part tiles stop getting finer at `part_zoom` (z12), so budgeting them
/// whole held the ppd at ~2.5 over Manhattan up to z14 and one dot per
/// person never came; budgeting what is on screen lets it fall as the view
/// narrows. A source without part tiles (world dots) budgets exactly as
/// before: `max(cells, cells / 2)` is `cells`.
fn budget_pop(plan: &DotPlan, tiles: &HashMap<GridTileKey, HeldTile>, part_people: f64) -> f64 {
    let cells = drawn_cell_pop(plan, tiles);
    let parts_whole: f64 = plan.draw.iter().filter(|x| x.key.is_part()).filter_map(|x| tiles.get(&x.key)).map(|h| h.slot_pop).sum();
    (cells + part_people).max((cells + parts_whole) / PART_DRAWN_OVER_BUDGET)
}

/// The population a tile's slots are built for: its `total`, or -- live,
/// with `epoch_b` -- the sum over cells of the larger epoch's count, as
/// `blocks::build_blocks` decomposes them. The ppd budgets this, so the
/// dots alive at any `epoch_mix` stay inside the budget and moving the
/// mix never moves the ppd (which would rebuild tiles mid-animation).
fn slot_pop(key: &GridTileKey, tile: &GridTile) -> f64 {
    if key.epoch_b.is_some() {
        tile.cells.iter().map(|&(_, a, b)| f64::from(a.max(b))).sum()
    } else {
        tile.total
    }
}

/// The population a seeded build floors its ppd by (`build`'s
/// `floor_pop`): a cell tile's [`slot_pop`], and none for a part tile --
/// part tiles are budgeted by their people in view ([`budget_pop`]), so a
/// dense part tile (a z12 tile of Manhattan holds ~1 M people) must build
/// at the view's ppd, down to 1, or it would hold fewer slots than the
/// view's ppd lights.
fn build_floor_pop(key: &GridTileKey, tile: &GridTile) -> f64 {
    if key.is_part() {
        0.0
    } else {
        slot_pop(key, tile)
    }
}

/// Epoch B's people in a tile: `Σ pop_b` for a key with `epoch_b`, else
/// its `total` (epoch A is the only one).
fn total_b(key: &GridTileKey, tile: &GridTile) -> f64 {
    if key.epoch_b.is_some() {
        tile.cells.iter().map(|&(_, _, b)| f64::from(b)).sum()
    } else {
        tile.total
    }
}

impl HeldTile {
    /// The tile's people at epoch mix `t`: `total` toward `total_b`.
    fn total_at(&self, t: f32) -> f64 {
        mix_f64(self.tile.total, self.total_b, t)
    }
}

/// `a + (b - a) * t`, exactly `a` at 0 and `b` at 1 (`t` in `[0, 1]`).
fn mix_f64(a: f64, b: f64, t: f32) -> f64 {
    if t <= 0.0 {
        a
    } else if t >= 1.0 {
        b
    } else {
        let t = f64::from(t);
        a * (1.0 - t) + b * t
    }
}

/// People whose cells are centred in `view`, over every draw of `plan`:
/// each drawn tile's cells less the ones its exclusion boxes keep from
/// drawing (`in_view::pop_in_view`). `plan_dots` never draws a cell twice
/// -- a stand-in's boxes exclude exactly the cells a drawn finer tile
/// holds -- so neither does this sum.
///
/// Only a tile the view's boundary cuts is walked by its cells, with a
/// [`CellIndex`] built the first time it is needed and kept on the held
/// tile; a tile inside the view is its whole population (less a
/// stand-in's excluded cells), one outside it nothing.
#[cfg(test)]
fn view_pop(d: &GridDescriptor, plan: &DotPlan, tiles: &HashMap<GridTileKey, HeldTile>, view: &ViewRegion) -> f64 {
    view_pop_with(d, plan, tiles, view, 0.0, &mut Vec::new()).0
}

fn view_pop_with(
    d: &GridDescriptor,
    plan: &DotPlan,
    tiles: &HashMap<GridTileKey, HeldTile>,
    view: &ViewRegion,
    t: f32,
    excluded: &mut ExcludedCache,
) -> (f64, f64) {
    let mut kept: ExcludedCache = Vec::new();
    let mut sum = 0.0;
    let mut parts = 0.0;
    for x in &plan.draw {
        let Some(h) = tiles.get(&x.key) else { continue };
        if x.key.is_part() {
            // A part tile counts by its parts' centroids, which all lie in
            // the tile (its owner); it is never a stand-in, so it has no
            // excluded parts (C7).
            let people = match in_view::coverage(d, &x.key, view) {
                in_view::Coverage::Outside => 0.0,
                in_view::Coverage::Inside => h.total_at(t),
                in_view::Coverage::Edge => in_view::part_pop_in_view(&h.tile, view),
            };
            sum += people;
            parts += people;
            continue;
        }
        sum += match in_view::coverage(d, &x.key, view) {
            in_view::Coverage::Outside => 0.0,
            in_view::Coverage::Edge => {
                in_view::pop_in_view_at(d, &x.key, h.index.index(d, &x.key, &h.tile), &x.exclude, view, t)
            }
            in_view::Coverage::Inside => {
                let ptr = Arc::as_ptr(&h.tile) as usize;
                let cut = match excluded.iter().find(|e| e.0 == x.key && e.1 == ptr && e.2 == x.exclude) {
                    Some(e) => e.3,
                    None => in_view::excluded_pops(d, &x.key, &h.tile, &x.exclude),
                };
                kept.push((x.key, ptr, x.exclude, cut));
                h.total_at(t) - mix_f64(cut.0, cut.1, t)
            }
        };
    }
    // Only the tiles drawn inside the view this frame are kept.
    *excluded = kept;
    (sum, parts)
}

/// [`view_pop`], reused while the view, the draws and the tiles they
/// draw are the frame before's: a still map costs nothing, and a moving
/// one a row walk of the drawn tiles its boundary cuts (`in_view`'s
/// module doc).
fn cached_view_pop(
    cache: &mut ViewCache,
    d: &GridDescriptor,
    plan: &DotPlan,
    tiles: &HashMap<GridTileKey, HeldTile>,
    view: &ViewRegion,
    t: f32,
) -> (f64, f64) {
    let held: Vec<usize> = plan.draw.iter().map(|x| tiles.get(&x.key).map_or(0, |h| Arc::as_ptr(&h.tile) as usize)).collect();
    // Keyed on `t` too, so an epoch animation re-counts every frame (R1-N-2):
    // bounded by the drawn tiles and the rows the view's edge cuts (an
    // inside tile's excluded people are cached by key, not `t`), and inside
    // MG-23's measured frame time.
    if cache.view.as_ref() == Some(view) && cache.draw == plan.draw && cache.tiles == held && cache.t == t {
        return (cache.people, cache.part_people);
    }
    let (people, part_people) = view_pop_with(d, plan, tiles, view, t, &mut cache.excluded);
    cache.t = t;
    cache.view = Some(view.clone());
    cache.draw.clone_from(&plan.draw);
    cache.tiles = held;
    cache.people = people;
    cache.part_people = part_people;
    (people, part_people)
}

/// One drawn tile's `DotParams` and its NW corner in world units, as
/// [`DotLayersGpu::sync`] writes them (C15: pure, so a test can read what
/// the uniform carries without a GPU): [`DotParams::for_tile`] at the
/// view's `ppd` and the style's radius at `zoom`, the style's shrink and
/// fade, its live-epoch fields and colour mode ([`DotStyle::live_params`];
/// `COLOR_CATEGORICAL` under [`DotRamp::Categorical`], set there so it is
/// never overwritten), the legend's [`DotStyle::category_mask`], and its
/// relocation ([`DotStyle::relocated`]). Bucket runs are filled later, by
/// `TilePipeline::create_dot_bind_group`.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn tile_params(
    style: &DotStyle,
    draw: &DotDraw,
    blocks: &TileBlocks,
    ppd: f32,
    zoom: f64,
    pitch: f32,
    transition: f32,
    mask: Option<&crate::render::dot_mask::MaskGrid>,
) -> (DotParams, (f64, f64)) {
    let (mut params, origin_world) = DotParams::for_tile(draw.key.tile, blocks, draw.exclude, ppd, style.radius_px(zoom));
    params.radius_shrink = style.density_shrink;
    params.overlap_gamma = style.overlap_gamma;
    style.live_params(&mut params, &draw.key);
    params.category_mask = u32::from(style.category_mask);
    if params.shape == crate::render::dots::SHAPE_TRIANGLE {
        // C12 as an engine invariant (review F4): a triangle has no cell,
        // so whatever style draws it, the cell-only terms are off --
        // `DotStyle::race` sets them off too, and `DotTileGpu::layout`'s
        // assert stays the backstop.
        params.radius_shrink = 0.0;
        params.overlap_gamma = 0.0;
        return (DotParams { mask_k: 0, ..params }, origin_world);
    }
    (style.relocated(params, draw.key.tile, zoom, pitch, transition, mask), origin_world)
}

/// Whether the GPU copy of `key`'s blocks `on_gpu` stays.
/// Only while the frame draws the tile, from the blocks it holds now: a
/// texture is as big as its tile's blocks, and the decoded-tile LRU holds
/// up to [`DECODED_TILES_PER_SOURCE`] tiles, so keeping every held tile's
/// texture tied GPU memory to where the view HAD been (fix round 1: 43-52
/// textures for 30-33 drawn tiles, 168 MB). A tile coming back is
/// uploaded again from its held blocks.
fn keeps_gpu_copy(plan: &DotPlan, tiles: &HashMap<GridTileKey, HeldTile>, key: &GridTileKey, on_gpu: &Arc<TileBlocks>) -> bool {
    plan.draw.iter().any(|x| x.key == *key) && tiles.get(key).is_some_and(|h| Arc::ptr_eq(&h.blocks, on_gpu))
}

/// One drawn tile on the GPU: its block texture and dot group (rebuilt
/// when its blocks are), and its model uniform.
struct HeldDotGpu {
    blocks: Arc<TileBlocks>,
    tile: crate::render::gpu::DotTileGpu,
    group: crate::render::gpu::DotGroupGpu,
    model: crate::render::alloc::TrackedBuffer,
    model_group: wgpu::BindGroup,
    model_buffer_bytes: u64,
    /// The `DotParams` last written for it (before `DotTileGpu::layout`).
    params: DotParams,
}

struct LayerGpu {
    id: String,
    generation: u64,
    /// The ramp and opacity `table` was made for.
    ramp: (DotRamp, f32),
    table: crate::render::gpu::PaintTableGpu,
    tiles: HashMap<GridTileKey, HeldDotGpu>,
    order: Vec<GridTileKey>,
}

/// The per-GPU-context half: every source's drawn tiles on the GPU, and
/// the paint-time pass that uploads, re-uniforms and lists them.
#[derive(Default)]
pub struct DotLayersGpu {
    layers: Vec<LayerGpu>,
}

/// What a map's dot tiles have sent to the GPU: counted by the pipeline
/// (`TilePipeline::dot_uploads`), inside the calls that send it.
pub use crate::render::gpu::DotUploads;

impl DotLayersGpu {
    /// Uploads what the frame draws and is not on the GPU yet (or was
    /// rebuilt), frees what its source no longer holds, and writes every
    /// drawn tile's model uniform and `DotParams` for this camera. Reports
    /// dots drawn and GPU bytes back into `layers`' stats.
    pub fn sync(
        &mut self,
        layers: &mut DotLayers,
        ctx: &crate::render::GpuContext,
        pipeline: &crate::render::TilePipeline,
        camera: &crate::render::Camera,
        globe: &crate::render::globe::GlobeUpload,
        mask: Option<(&crate::render::dot_mask::DotMaskGpu, &crate::render::dot_mask::MaskGrid)>,
    ) {
        let transition = globe.transition();
        self.layers.retain(|g| layers.layers.iter().any(|l| l.id == g.id && l.generation == g.generation));
        for layer in &mut layers.layers {
            let table_for = |(ramp, opacity): (DotRamp, f32)| {
                pipeline.create_paint_table(ctx, &crate::render::compact::paint_table(&ramp.paints(opacity)))
            };
            // A new ramp (the difference toggle) or opacity swaps the paint
            // table and re-binds the held tiles' groups over it; their
            // block textures stay (no upload).
            let paint = (layer.style.ramp, layer.style.opacity);
            let mut regroup = false;
            let at = match self.layers.iter().position(|g| g.id == layer.id) {
                Some(at) if self.layers[at].ramp == paint => at,
                Some(at) => {
                    self.layers[at].table = table_for(paint);
                    self.layers[at].ramp = paint;
                    regroup = true;
                    at
                }
                None => {
                    self.layers.push(LayerGpu {
                        id: layer.id.clone(),
                        generation: layer.generation,
                        ramp: paint,
                        table: table_for(paint),
                        tiles: HashMap::new(),
                        order: Vec::new(),
                    });
                    self.layers.len() - 1
                }
            };
            let gpu = &mut self.layers[at];
            gpu.tiles.retain(|k, held| keeps_gpu_copy(&layer.plan, &layer.tiles, k, &held.blocks));
            gpu.order.clear();
            let mut dots = 0u64;
            let (mut tiles_drawn, mut slots_drawn, mut pop_drawn) = (0u32, 0u64, 0f64);
            let (mut built_min, mut built_max) = (f32::INFINITY, 0f32);
            for draw in &layer.plan.draw {
                let Some(held) = layer.tiles.get(&draw.key) else { continue };
                if held.blocks.slots == 0 {
                    continue;
                }
                tiles_drawn += 1;
                slots_drawn += held.blocks.slots;
                pop_drawn += held.total_at(layer.style.mix());
                built_min = built_min.min(held.blocks.ppd_build);
                built_max = built_max.max(held.blocks.ppd_build);
                let (params, origin_world) = tile_params(
                    &layer.style,
                    draw,
                    &held.blocks,
                    layer.tile_ppd(),
                    camera.zoom,
                    camera.pitch,
                    transition,
                    mask.map(|(_, grid)| grid),
                );
                // The group binds the live mask whenever there is one (the
                // land texel otherwise), and is recreated when the mask it
                // bound is stale: `write_dot_params` rewrites the uniform,
                // never the binding.
                let bound = mask.map(|(gpu_mask, _)| gpu_mask);
                let generation = bound.map(crate::render::dot_mask::DotMaskGpu::generation);
                let matrix = crate::render::combined_overlay_matrix(camera, origin_world).to_cols_array_2d();
                match gpu.tiles.get_mut(&draw.key) {
                    Some(on_gpu) => {
                        pipeline.write_model_uniform(ctx, &on_gpu.model, matrix, 0.0, 0.0, globe.for_overlay(origin_world));
                        on_gpu.params = params;
                        if on_gpu.group.mask_generation() == generation && !regroup {
                            pipeline.write_dot_params(ctx, &on_gpu.group, &on_gpu.tile, params);
                        } else {
                            on_gpu.group = pipeline.create_dot_bind_group(ctx, &gpu.table, &on_gpu.tile, bound, params);
                        }
                    }
                    None => {
                        let b = &held.blocks;
                        let tile = pipeline.create_dot_tile_with(ctx, &b.buckets, &b.pop_b, &b.comps, &b.shapes);
                        let group = pipeline.create_dot_bind_group(ctx, &gpu.table, &tile, bound, params);
                        let (model, model_group) =
                            pipeline.create_model_bind_group(ctx, matrix, 0.0, 0.0, globe.for_overlay(origin_world));
                        let model_buffer_bytes = model.tracked_size();
                        gpu.tiles.insert(
                            draw.key,
                            HeldDotGpu { blocks: Arc::clone(&held.blocks), tile, group, model, model_group, model_buffer_bytes, params },
                        );
                    }
                }
                dots += u64::from(gpu.tiles[&draw.key].group.vertices() / crate::render::dots::DOT_VERTICES);
                gpu.order.push(draw.key);
            }
            // A held tile this frame did not draw keeps no stale mask: a
            // group still bound to an older mask texture would hold that
            // texture (up to 16 MB) alive. Every tile `keeps_gpu_copy`
            // keeps is drawn above today, so this is the guard for when
            // that stops being true.
            let live = mask.map(|(gpu_mask, _)| gpu_mask.generation());
            let LayerGpu { tiles, order, .. } = gpu;
            tiles.retain(|key, held| order.contains(key) || held.group.mask_generation().is_none_or(|g| Some(g) == live));
            layer.stats.dots_drawn = dots;
            layer.stats.tiles_drawn = tiles_drawn;
            layer.stats.gpu_tiles = gpu.tiles.len() as u32;
            layer.stats.slots_drawn = slots_drawn;
            layer.stats.pop_drawn = pop_drawn;
            layer.stats.built_min = built_min;
            layer.stats.built_max = built_max;
            layer.stats.gpu_bytes = gpu
                .tiles
                .values()
                .map(|h| h.tile.device_bytes() + h.group.device_bytes() + h.model_buffer_bytes)
                .sum();
        }
    }

    /// Appends every source's dot draws, coarse tiles first within a
    /// source, sources in the order they were added. `flat` is the frame's
    /// `transition == 0` (`vs_dot_flat`).
    pub fn push_draws<'a>(&'a self, flat: bool, draws: &mut Vec<crate::render::TileDraw<'a>>) {
        for layer in &self.layers {
            for key in &layer.order {
                if let Some(held) = layer.tiles.get(key) {
                    draws.push(crate::render::TileDraw::Dots(&held.group, &held.model_group, flat));
                }
            }
        }
    }

    /// Every drawn tile's `DotParams` for layer `id` as last written,
    /// in draw order (tests: what the uniform carries).
    #[cfg(test)]
    pub(crate) fn drawn_params(&self, id: &str) -> Vec<DotParams> {
        self.layers
            .iter()
            .filter(|l| l.id == id)
            .flat_map(|l| l.order.iter().filter_map(|k| l.tiles.get(k)))
            .map(|h| h.params)
            .collect()
    }

    /// Every held tile's bound mask generation (`None`: the land texel).
    #[cfg(test)]
    pub(crate) fn mask_generations(&self) -> Vec<Option<u64>> {
        self.layers.iter().flat_map(|l| l.tiles.values()).map(|h| h.group.mask_generation()).collect()
    }

    /// Device bytes held for every source.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.layers
            .iter()
            .flat_map(|l| l.tiles.values())
            .map(|h| h.tile.device_bytes() + h.group.device_bytes() + h.model_buffer_bytes)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::dotgrid::{grid, GridTile, MemoryGridSource};

    fn d30() -> GridDescriptor {
        GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: GHSL_ATTRIBUTION.to_string(),
        }
    }

    fn key(tile: TileId, level: u8) -> GridTileKey {
        GridTileKey { tile, level, epoch: 2025, epoch_b: None }
    }

    fn children(t: TileId) -> Vec<TileId> {
        let (x, y, z) = (t.x * 2, t.y * 2, t.z + 1);
        vec![TileId { z, x, y }, TileId { z, x: x + 1, y }, TileId { z, x, y: y + 1 }, TileId { z, x: x + 1, y: y + 1 }]
    }

    fn inside(p: [f32; 2], b: [f32; 4]) -> bool {
        p[0] >= b[0] && p[1] >= b[1] && p[0] <= b[2] && p[1] <= b[3]
    }

    /// The z6 tile holding a lon/lat, by the same `floor` `tile_of_cell` uses.
    fn z6_of(lon: f64, lat: f64) -> TileId {
        let (wx, wy) = crate::overlay::geom::lon_lat_to_world(lon, lat);
        let world = f64::from(crate::render::WORLD_SIZE);
        TileId { z: 6, x: (wx / world * 64.0).floor() as u32, y: (wy / world * 64.0).floor() as u32 }
    }

    #[test]
    fn a_missing_tile_is_drawn_by_its_parent_with_the_ready_siblings_excluded() {
        // 30″ descriptor, zoom 5.5 (level 0, data z6). Cover = the 4
        // children of z5 tile (16,16). Ready: the parent and 2 of the 4
        // children -- diagonal, so no single box holds both.
        let d = d30();
        let parent_tile = TileId { z: 5, x: 16, y: 16 };
        let parent = key(parent_tile, 1);
        let kids = children(parent_tile);
        let ready_kids = [kids[0], kids[3]];
        let ready = |k: &GridTileKey| *k == parent || (k.level == 0 && ready_kids.contains(&k.tile));
        let plan = plan_dots(&d, 2025, 5.5, &kids, &ready);

        let drawn: Vec<GridTileKey> = plan.draw.iter().map(|x| x.key).collect();
        assert_eq!(drawn.len(), 3, "2 ready children + the parent: {drawn:?}");
        for kid in ready_kids {
            let draw = plan.draw.iter().find(|x| x.key == key(kid, 0)).expect("a ready child draws itself");
            assert_eq!(draw.exclude, [DotParams::NONE_EXCLUDED; 2]);
            assert!(!draw.standin);
        }
        let standin = plan.draw.iter().find(|x| x.key == parent).expect("the parent stands in");
        assert!(standin.standin);

        // Every parent cell is excluded exactly when its centre
        // lies in a READY child -- tested on the block origin
        // `build_blocks` gives it (the cell's SW corner, tile-local).
        let w = grid::cell_window(&d, 1, parent_tile);
        let (ox, oy) = (16.0 / 32.0, 16.0 / 32.0);
        let lyon = blocks::OVERLAY_LYON_PER_UNIT_WORLD;
        let (mut excluded, mut drawn_cells) = (0, 0);
        for idx in 0..w.rows * w.cols {
            let (row, col) = grid::row_col(&w, idx);
            let (lat, lon) = grid::cell_centre(&d, 1, row, col);
            let (sx, sy) = grid::cell_sw_mercator(&d, 1, row, col);
            let origin = [((sx - ox) * lyon) as f32, ((sy - oy) * lyon) as f32];
            let is_excluded = standin.exclude.iter().any(|b| inside(origin, *b));
            let in_ready = ready_kids.contains(&z6_of(lon, lat));
            assert_eq!(is_excluded, in_ready, "cell ({row},{col}) at ({lon:.4},{lat:.4})");
            if is_excluded { excluded += 1 } else { drawn_cells += 1 }
        }
        assert!(excluded > 100 && drawn_cells > 100, "{excluded} excluded, {drawn_cells} drawn");

        for k in [parent, key(kids[1], 0), key(kids[2], 0)] {
            assert!(plan.wanted.contains(&k), "{k:?} is wanted");
        }
    }

    /// A zoom-out of two levels before the new level lands: no ancestor
    /// of the level-2 key is held, but its level-0 (and one level-1)
    /// descendants are -- they draw in its place, none over another.
    #[test]
    fn a_zoom_out_of_two_levels_draws_the_held_descendants() {
        let d = d30();
        // zoom 4.0 on 30″: level floor(6.40 - 4.0) = 2, data z4.
        let top = TileId { z: 4, x: 8, y: 8 };
        let quads = children(top); // z5, level 1
        let fine: Vec<TileId> = quads.iter().flat_map(|&q| children(q)).collect(); // z6, level 0
        // Held: quadrant 0 at level 1, and the level-0 tiles of the other three.
        let held = |k: &GridTileKey| {
            (k.level == 1 && k.tile == quads[0]) || (k.level == 0 && fine.contains(&k.tile) && !is_descendant(k.tile, quads[0]))
        };
        let plan = plan_dots(&d, 2025, 4.0, &[top], &held);
        assert!(plan.wanted.contains(&key(top, 2)));
        let drawn: Vec<GridTileKey> = plan.draw.iter().map(|x| x.key).collect();
        assert_eq!(drawn.len(), 1 + 12, "the held level-1 quadrant and 12 level-0 tiles: {drawn:?}");
        assert!(drawn.contains(&key(quads[0], 1)));
        for a in &drawn {
            assert!(held(a));
            for b in &drawn {
                assert!(!is_descendant(a.tile, b.tile), "{a:?} is under {b:?}: drawn twice");
            }
        }
        assert!(plan.draw.iter().all(|x| x.exclude == [DotParams::NONE_EXCLUDED; 2]));
        assert!(plan.draw.iter().all(|x| x.standin), "all drawn in place of the missing key");
    }

    #[test]
    fn parents_are_wanted_before_children() {
        let d = d30();
        let cover: Vec<TileId> =
            (31..34).flat_map(|x| (30..33).map(move |y| TileId { z: 6, x, y })).collect();
        let plan = plan_dots(&d, 2025, 5.5, &cover, &|_| false);
        let last_parent = plan.wanted.iter().rposition(|k| k.level == 1).expect("parents are wanted");
        let first_child = plan.wanted.iter().position(|k| k.level == 0).expect("the cover is wanted");
        assert!(last_parent < first_child, "{:?}", plan.wanted);
        assert_eq!(plan.wanted.iter().filter(|k| k.level == 0).count(), 9);
        assert_eq!(plan.wanted.iter().filter(|k| k.level == 1).count(), 4, "(31..34)x(30..33) at z6 spans 2x2 z5 parents");
        assert!(plan.draw.is_empty(), "nothing is ready, nothing draws");
    }

    #[test]
    fn zoom_above_the_base_uses_level_zero_overzoomed() {
        // zoom 12 on 30″ -> keys at z6.
        let d = d30();
        let cover: Vec<TileId> =
            (2048..2051).flat_map(|x| (2040..2043).map(move |y| TileId { z: 12, x, y })).collect();
        let plan = plan_dots(&d, 2025, 12.0, &cover, &|_| false);
        let base: Vec<&GridTileKey> = plan.wanted.iter().filter(|k| k.level == 0).collect();
        assert_eq!(base.len(), 1, "{:?}", plan.wanted);
        assert_eq!(base[0].tile, TileId { z: 6, x: 32, y: 31 });
        assert!(plan.wanted.contains(&key(TileId { z: 5, x: 16, y: 15 }, 1)));
        assert!(plan.wanted.iter().all(|k| k.epoch == 2025 && k.epoch_b.is_none()));
    }

    fn small_source() -> Arc<dyn GridCountSource> {
        Arc::new(MemoryGridSource::new(d30(), |_| GridTile { cells: vec![(0, 50.0, 50.0), (7, 3.0, 3.0)], total: 53.0, ..Default::default() }))
    }

    fn drive_one_job(layers: &mut DotLayers) -> DotJob {
        let now = crate::timing::Instant::now();
        let kids = children(TileId { z: 5, x: 16, y: 16 });
        layers.frame_with_cover(5.5, |_| kids.clone(), ([0.49, 0.49], [0.51, 0.51]), now);
        layers.next_job().expect("a fetch for the missing cover")
    }

    /// Review focus 4: a source replaced (or removed) while its tiles are
    /// in flight never has an old tile installed under the new one.
    #[test]
    fn a_tile_from_a_replaced_source_is_never_installed() {
        let mut layers = DotLayers::new();
        layers.set_source("pop", small_source(), DotStyle::default());
        let job = drive_one_job(&mut layers);
        // Replaced while the job runs: generation 1 -> 2.
        layers.set_source("pop", small_source(), DotStyle::default());
        let landed = job.run(|_| Ok(Vec::new()));
        assert!(!layers.land(landed), "a generation-1 result is dropped under generation 2");
        assert_eq!(layers.stale_dropped(), 1);
        assert_eq!(layers.tiles_held("pop"), 0, "nothing from the old source was installed");

        // The new generation's own job still lands.
        let job = drive_one_job(&mut layers);
        assert!(layers.land(job.run(|_| Ok(Vec::new()))));
        assert!(layers.tiles_held("pop") > 0);

        // Removed while in flight: dropped too, and re-adding the same id
        // is a NEW generation, not the removed one.
        let job = drive_one_job(&mut layers);
        layers.remove_source("pop");
        layers.set_source("pop", small_source(), DotStyle::default());
        assert!(!layers.land(job.run(|_| Ok(Vec::new()))));
        assert_eq!(layers.stale_dropped(), 2);
        assert_eq!(layers.tiles_held("pop"), 0);
    }

    /// A job whose worker panicked is landed from its ticket: its slot
    /// and its keys are freed, the failure counted, and the keys back off.
    #[test]
    fn an_abandoned_job_frees_its_slot() {
        let mut layers = DotLayers::new();
        layers.set_source("pop", small_source(), DotStyle::default());
        let job = drive_one_job(&mut layers);
        let ticket = job.ticket();
        drop(job);
        assert_eq!(layers.layers[0].requests, 1);
        assert!(layers.land(DotLanded::abandoned(ticket)));
        let layer = &layers.layers[0];
        assert_eq!(layer.requests, 0, "the slot is free");
        assert!(layer.fetching.is_empty(), "no key is held in flight forever");
        assert_eq!(layer.stats.source_errors, 1);
        assert_eq!(layer.stats.last_error, Some("server"));
        assert_eq!(layers.next_retry(crate::timing::Instant::now()), Some(FAILED_BACKOFF));
    }

    /// M-6/R-7: `run_job_catching_panic` is the one place that rule is
    /// implemented and tested -- a job whose `run` panics still lands (as
    /// `DotLanded::abandoned`), freeing its slot, rather than holding it
    /// forever. `ios::dots::DotWorker` calls this directly (it has no
    /// runtime of its own to catch the panic for it); native's
    /// `MapView::drive_dot_sources` reaches the same end through
    /// `tokio::task::JoinHandle`'s `Err`, which this test does not cover,
    /// but the landing rule it relies on is the same one asserted here and
    /// in `an_abandoned_job_frees_its_slot`.
    #[test]
    fn a_panicking_job_still_lands() {
        let mut layers = DotLayers::new();
        layers.set_source("pop", small_source(), DotStyle::default());
        let job = drive_one_job(&mut layers);
        assert_eq!(layers.layers[0].requests, 1, "not yet landed");
        let landed = run_job_catching_panic(job, |_| panic!("a worker's fetch blew up"));
        assert!(layers.land(landed));
        let layer = &layers.layers[0];
        assert_eq!(layer.requests, 0, "the slot is free");
        assert!(layer.fetching.is_empty(), "no key is held in flight forever");
        assert_eq!(layer.stats.source_errors, 1);
        assert_eq!(layer.stats.last_error, Some("server"));
    }

    fn india_z5_globe() -> (crate::render::Camera, crate::render::globe::GlobeCover) {
        let camera = crate::render::Camera {
            center_lon: 78.9,
            center_lat: 22.0,
            zoom: 5.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1280.0,
            viewport_height_px: 800.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let globe = crate::render::globe::GlobeCover::new(&crate::render::globe::globe_frame(&camera));
        (camera, globe)
    }

    /// Fix round 1 (MG-11): the globe's cover keeps about a tile of margin
    /// round the view (a bounding-ball test). For dots that margin was most
    /// of the draw -- 34 z5 tiles at India z5 in a 1280x800 window, for a
    /// view about 5 x 3.1 tiles wide. The dot cover keeps what the view
    /// shows, and never a tile under the view.
    #[test]
    fn the_globe_dot_cover_keeps_only_tiles_the_view_shows() {
        let (camera, globe) = india_z5_globe();
        assert!(globe.is_globe(), "z5 is a globe frame");
        let full = globe.cover(5).expect("a globe cover");
        let kept = dot_cover(&camera, &globe, 5);
        assert!(full.len() >= 30, "the globe cover this guards against: {}", full.len());
        assert!(kept.len() <= 20, "{} dot cover tiles for a 5 x 3.1-tile view (globe cover {})", kept.len(), full.len());
        assert!(kept.iter().all(|t| full.contains(t)));
        let world = f64::from(crate::render::WORLD_SIZE);
        for (x, y) in [(640.0, 400.0), (1.0, 1.0), (1279.0, 1.0), (1.0, 799.0), (1279.0, 799.0)] {
            let p = crate::render::unproject_to_ground_f64(&camera, x, y).expect("on the ground");
            let t = TileId { z: 5, x: (p.x / world * 32.0).floor() as u32, y: (p.y / world * 32.0).floor() as u32 };
            assert!(kept.contains(&t), "the tile under screen ({x}, {y}), {t:?}, is kept");
        }
    }

    /// Fix round 1 (MG-11, and the counter): the budget bounds the dots
    /// the frame's draws can make alive -- every drawn tile's WHOLE
    /// population over one ppd -- and every tile draws at that one ppd,
    /// the one the stats (and the counter) report. Before, the ppd
    /// budgeted the population on screen and each tile had its own floor
    /// (`total / budget`), so N tiles could each carry a whole budget.
    /// Final review: `DotStyle { budget_dots: 0 }` is held as one dot, so
    /// no ppd or budget division sees a zero.
    #[test]
    fn a_zero_dot_budget_is_held_as_one() {
        let src: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d30(), |_| GridTile::default()));
        let mut layers = DotLayers::new();
        layers.set_source("pop", src, DotStyle { budget_dots: 0, ..DotStyle::default() });
        assert_eq!(layers.layers[0].style.budget_dots, 1);
    }

    #[test]
    fn the_budget_bounds_every_drawn_tiles_dots_at_one_ppd() {
        let budget = 1_000u32;
        let src: Arc<dyn GridCountSource> =
            Arc::new(MemoryGridSource::new(d30(), |_| GridTile { cells: vec![(0, 3_000.0, 3_000.0), (9, 1.0, 1.0)], total: 3_001.0, ..Default::default() }));
        let mut layers = DotLayers::new();
        layers.set_source("pop", src, DotStyle { budget_dots: budget, ..DotStyle::default() });
        let cover: Vec<TileId> = children(TileId { z: 5, x: 16, y: 16 });
        // A viewport inside one of the four z6 tiles: a quarter of its area.
        let viewport = ([0.5, 0.5], [0.5 + 1.0 / 128.0, 0.5 + 1.0 / 128.0]);
        for _ in 0..40 {
            let now = crate::timing::Instant::now();
            layers.frame_with_cover(6.6, |_| cover.clone(), viewport, now);
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        let layer = &layers.layers[0];
        let stats = layers.stats("pop").expect("the layer");
        let drawn: Vec<&HeldTile> = layer.plan.draw.iter().map(|x| &layer.tiles[&x.key]).collect();
        assert_eq!(drawn.len(), 4);
        let mut dots = 0.0;
        for held in &drawn {
            let ppd = layer.tile_ppd();
            assert_eq!(ppd, stats.ppd, "every tile draws at the ppd the stats report");
            dots += held.tile.total / f64::from(ppd);
        }
        assert!(dots <= f64::from(budget) * 1.001, "{dots:.0} dots can be alive against a budget of {budget}");
        assert!(dots >= f64::from(budget) * 0.8, "{dots:.0} dots: the budget is used");
        // Each tile's two cells sit in its NW corner: only tile (32, 32)'s
        // are in the viewport (its NW quarter), all 3,001 people (to the
        // float rounding of their area shares).
        assert!((stats.people_in_view - 3_001.0).abs() < 1e-6, "{}", stats.people_in_view);
        let in_view = 3_001.0 / f64::from(stats.ppd);
        assert!((stats.dots_in_view as f64 - in_view).abs() <= 0.5, "{} in view, expected {in_view:.1}", stats.dots_in_view);
    }

    /// Fix round 1: a tile built before the ppd is seeded (the first
    /// fetch goes out before anything is drawn) or at a stale one is
    /// still built within the budget. Without a floor the first tiles were
    /// built at ppd 1 -- RSS 3 GB in the first pan run. Fix round 3: the
    /// SUM over every request the first frame sends, not each tile alone
    /// (a per-tile floor gave each of K tiles √2 budgets).
    #[test]
    fn a_tile_is_never_built_over_the_budget() {
        let budget = 1_000u32;
        let src: Arc<dyn GridCountSource> =
            Arc::new(MemoryGridSource::new(d30(), |_| GridTile { cells: vec![(0, 1.0e7, 1.0e7)], total: 1.0e7, ..Default::default() }));
        let mut layers = DotLayers::new();
        layers.set_source("pop", src, DotStyle { budget_dots: budget, ..DotStyle::default() });
        // Every request the layer allows goes out before anything lands
        // (no frame between them seeds the ppd).
        let now = crate::timing::Instant::now();
        let kids = children(TileId { z: 5, x: 16, y: 16 });
        layers.frame_with_cover(5.5, |_| kids.clone(), ([0.49, 0.49], [0.51, 0.51]), now);
        let mut jobs = Vec::new();
        while let Some(job) = layers.next_job() {
            jobs.push(job);
        }
        assert_eq!(jobs.len(), MAX_REQUESTS_IN_FLIGHT, "the first frame fills every request slot");
        for job in jobs {
            assert!(layers.land(job.run(|_| Ok(Vec::new()))));
        }
        let sum: u64 = layers.layers[0].tiles.values().map(|h| h.blocks.slots).sum();
        assert!(sum as f64 <= f64::from(budget) * 2.0, "{sum} slots over {} tiles against a budget of {budget}", layers.layers[0].tiles.len());
    }

    /// Fix round 1 (MG-11 GPU bytes): a block texture is on the GPU only
    /// while its tile is drawn. Before, every decoded tile the LRU held
    /// kept its texture (43-52 textures for 30-33 drawn in the pan).
    #[test]
    fn only_drawn_tiles_keep_a_block_texture() {
        let blocks = Arc::new(TileBlocks::default());
        let a = key(TileId { z: 6, x: 1, y: 1 }, 0);
        let b = key(TileId { z: 6, x: 2, y: 1 }, 0);
        let mut tiles = HashMap::new();
        for k in [a, b] {
            tiles.insert(k, HeldTile {
                slot_pop: 0.0,
                total_b: 0.0,
                tile: Arc::new(GridTile::default()),
                blocks: Arc::clone(&blocks),
                index: Arc::new(CellIndex::default()).into(),
                used: 0,
            });
        }
        let plan = DotPlan { wanted: vec![a, b], draw: vec![DotDraw { key: a, exclude: [DotParams::NONE_EXCLUDED; 2], standin: false }], ..Default::default() };
        assert!(keeps_gpu_copy(&plan, &tiles, &a, &blocks));
        assert!(!keeps_gpu_copy(&plan, &tiles, &b, &blocks), "held but not drawn: freed");
        assert!(!keeps_gpu_copy(&plan, &tiles, &a, &Arc::new(TileBlocks::default())), "rebuilt: the old copy goes");
    }

    /// Fix round 2: a map's default budget is the stricter of MG-8's
    /// headless one and MG-11's window one.
    #[test]
    fn the_default_budget_is_the_stricter_of_mg8_and_mg11() {
        use crate::render::dots::{NATIVE_DOT_BUDGET, NATIVE_WINDOW_DOT_BUDGET};
        assert_eq!(DotStyle::default().budget_dots, NATIVE_WINDOW_DOT_BUDGET.min(NATIVE_DOT_BUDGET));
        assert_eq!(DotStyle::default().budget_dots, 1_500_000);
    }

    /// Race Task 15 (MG-R15): the shells draw race dots on their own
    /// budgets, capped by `RACE_DOT_BUDGET` (MG-R10).
    #[test]
    fn race_budgets_per_platform() {
        use crate::render::dots::{IOS_DOT_BUDGET, NATIVE_DOT_BUDGET, RACE_DOT_BUDGET, WEB_DOT_BUDGET};
        assert_eq!(race_budget_for(Platform::Web), WEB_DOT_BUDGET.min(RACE_DOT_BUDGET));
        assert_eq!(race_budget_for(Platform::Ios), IOS_DOT_BUDGET.min(RACE_DOT_BUDGET));
        assert_eq!(race_budget_for(Platform::Native), DotStyle::default().budget_dots.min(RACE_DOT_BUDGET));
        assert_eq!(RACE_DOT_BUDGET, NATIVE_DOT_BUDGET, "MG-R10 passed at 2.7 M: no race-specific cut");
        for platform in [Platform::Native, Platform::Ios, Platform::Web] {
            assert_eq!(DotStyle::race(platform).budget_dots, race_budget_for(platform), "{platform:?}");
        }
    }

    /// Task 17: the phone's budget is 2 M (the plan's figure; MG-25 owed),
    /// and the other shells' are their measured constants, not new numbers.
    #[test]
    fn ios_budget_is_two_million() {
        use crate::render::dots::{IOS_DOT_BUDGET, WEB_DOT_BUDGET};
        assert_eq!(DotStyle::for_platform(Platform::Ios).budget_dots, 2_000_000);
        assert_eq!(DotStyle::for_platform(Platform::Ios).budget_dots, IOS_DOT_BUDGET);
        assert_eq!(DotStyle::for_platform(Platform::Native).budget_dots, DotStyle::default().budget_dots);
        assert_eq!(DotStyle::for_platform(Platform::Web).budget_dots, WEB_DOT_BUDGET);
    }

    /// The shells with no dot mask pass (the phone, the browser: DP-7's
    /// shell halves) place dots uniformly; native keeps the default. The
    /// rest of the style is the default everywhere.
    #[test]
    fn only_native_relocates_by_default() {
        assert!(DotStyle::for_platform(Platform::Native).relocates());
        for platform in [Platform::Ios, Platform::Web] {
            let style = DotStyle::for_platform(platform);
            assert!(!style.relocates(), "{platform:?}");
            assert_eq!(style.epoch, DotStyle::default().epoch);
            assert_eq!(style.overlap_gamma, DotStyle::default().overlap_gamma);
            assert_eq!(style.ramp, DotStyle::default().ramp);
        }
    }

    /// The overlap fade is on by default at FO-1's measured gamma.
    #[test]
    fn the_default_style_fades_by_overlap_at_the_measured_gamma() {
        assert_eq!(DotStyle::default().overlap_gamma, 0.35);
    }

    /// The spec's density shrink is on by default: the densest cells'
    /// dots lose 30 % of their radius.
    #[test]
    fn the_default_style_shrinks_dense_dots_by_thirty_percent() {
        let style = DotStyle::default();
        assert_eq!(style.density_shrink, 0.3);
        let r = style.radius_px(9.5);
        assert!((crate::render::dots::dot_radius_px(r, style.density_shrink, 1.0) - 0.7 * r).abs() < 1e-6);
        assert_eq!(crate::render::dots::dot_radius_px(r, style.density_shrink, 0.0), r);
    }

    /// Every level-`level` cell of `tile`'s window, with a population from
    /// its row and column (so no two neighbours are alike).
    fn every_cell(d: &GridDescriptor, level: u8, tile: TileId) -> GridTile {
        let w = grid::cell_window(d, level, tile);
        let cells: Vec<(u32, f32, f32)> = (0..w.rows * w.cols)
            .map(|i| {
                let pop = 1.0 + ((i * 7 + u32::from(level) * 3) % 13) as f32;
                (i, pop, pop)
            })
            .collect();
        let total = cells.iter().map(|c| f64::from(c.1)).sum();
        GridTile { cells, total, ..Default::default() }
    }

    /// The people of `tile` in the unit-world box, cell by cell: each
    /// cell's population times the share of its Mercator rectangle inside
    /// the box -- and only the cells `keep` (on the cell's centre) says are
    /// drawn.
    fn brute_people(d: &GridDescriptor, key: &GridTileKey, tile: &GridTile, b: ([f64; 2], [f64; 2]), keep: &dyn Fn(f64, f64) -> bool) -> f64 {
        let w = grid::cell_window(d, key.level, key.tile);
        tile.cells
            .iter()
            .map(|&(idx, pop, _)| {
                let (row, col) = grid::row_col(&w, idx);
                let (lat, lon) = grid::cell_centre(d, key.level, row, col);
                if !keep(lon, lat) {
                    return 0.0;
                }
                f64::from(pop) * cell_share(d, key.level, row, col, b)
            })
            .sum()
    }

    /// The share of cell (`row`, `col`)'s Mercator rectangle inside box `b`.
    fn cell_share(d: &GridDescriptor, level: u8, row: u32, col: u32, b: ([f64; 2], [f64; 2])) -> f64 {
        let (x0, y1) = grid::cell_sw_mercator(d, level, row, col);
        let (x1, y0) = grid::cell_sw_mercator(d, level, row - 1, col + 1);
        let ox = (x1.min(b.1[0]) - x0.max(b.0[0])).max(0.0) / (x1 - x0);
        let oy = (y1.min(b.1[1]) - y0.max(b.0[1])).max(0.0) / (y1 - y0);
        ox * oy
    }

    /// Epoch B of [`every_cell`]: a different count in every cell (some
    /// fewer, some more people), and 0 in every fifth -- a cell that is
    /// only in epoch A.
    fn every_cell_b(d: &GridDescriptor, level: u8, tile: TileId) -> GridTile {
        let a = every_cell(d, level, tile);
        let cells: Vec<(u32, f32, f32)> =
            a.cells.iter().map(|&(i, p, _)| (i, p, if i % 5 == 0 { 0.0 } else { p * ((i % 4) as f32 + 0.5) })).collect();
        GridTile { cells, total: a.total, ..Default::default() }
    }

    /// Settled stats of one layer over `t` at `viewport`, drawing `source`
    /// with `style`.
    fn settled_one(source: Arc<dyn GridCountSource>, style: DotStyle, t: TileId, viewport: ([f64; 2], [f64; 2])) -> DotStats {
        let mut layers = DotLayers::new();
        layers.set_source("pop", source, style);
        for _ in 0..10 {
            layers.frame_with_cover(6.2, |_| vec![t], viewport, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        layers.stats("pop").unwrap()
    }

    /// A source whose `decode` fails for one key, every time: a tile that
    /// never lands (R1-M-1).
    struct FailsOne {
        d: GridDescriptor,
        bad: GridTileKey,
    }

    impl GridCountSource for FailsOne {
        fn descriptor(&self) -> &GridDescriptor {
            &self.d
        }
        fn request(&self, _keys: &[GridTileKey]) -> GridRequest {
            GridRequest::Absent
        }
        fn decode(&self, keys: &[GridTileKey], _body: &[u8]) -> Result<Vec<(GridTileKey, GridTile)>, GridError> {
            if keys.contains(&self.bad) {
                return Err(GridError::Server("this tile always fails".to_string()));
            }
            Ok(keys.iter().map(|k| (*k, every_cell(&self.d, k.level, k.tile))).collect())
        }
    }

    const PAIR_COVER: [TileId; 4] =
        [TileId { z: 6, x: 40, y: 20 }, TileId { z: 6, x: 41, y: 20 }, TileId { z: 6, x: 40, y: 21 }, TileId { z: 6, x: 41, y: 21 }];

    fn pair_frame(layers: &mut DotLayers, run_jobs: bool) {
        let view = ([40.0 / 64.0, 20.0 / 64.0], [42.0 / 64.0, 22.0 / 64.0]);
        layers.frame_with_cover(6.2, |_| PAIR_COVER.to_vec(), view, crate::timing::Instant::now());
        if run_jobs {
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
    }

    /// R1-M-1: one key of the new pair fails every time. The old pair
    /// stands in only until that failure has landed; then the new pair
    /// draws what it holds (a stand-in or a hole for the failed tile, as
    /// for any failed key) and only the new pair is held.
    #[test]
    fn a_key_that_always_fails_releases_the_old_pair() {
        let bad = GridTileKey { tile: PAIR_COVER[3], level: 0, epoch: 1975, epoch_b: Some(1980) };
        let mut layers = DotLayers::new();
        let style = DotStyle { epoch: 1975, epoch_b: Some(2030), ..DotStyle::default() };
        layers.set_source("pop", Arc::new(FailsOne { d: d30(), bad }), style);
        for _ in 0..10 {
            pair_frame(&mut layers, true);
        }
        let old = |k: &GridTileKey| k.epoch_b == Some(2030);
        assert!(!layers.plan("pop").unwrap().draw.is_empty() && layers.plan("pop").unwrap().draw.iter().all(|x| old(&x.key)));
        assert!(layers.set_epochs("pop", 1975, Some(1980)));
        // The step's first frame: nothing of the new pair yet, the old draws.
        pair_frame(&mut layers, false);
        assert!(layers.plan("pop").unwrap().draw.iter().all(|x| old(&x.key)), "the old pair stands in");
        assert!(layers.held_keys("pop").iter().any(old));
        for _ in 0..10 {
            pair_frame(&mut layers, true);
        }
        assert!(layers.stats("pop").unwrap().source_errors > 0, "the bad key failed");
        let plan = layers.plan("pop").unwrap();
        assert!(!plan.draw.is_empty(), "the new pair draws what it holds");
        assert!(plan.draw.iter().all(|x| x.key.epoch_b == Some(1980)), "released: {plan:?}");
        assert!(plan.wanted.contains(&bad), "the failed key is still wanted (retried after its backoff)");
        let held = layers.held_keys("pop");
        assert!(!held.is_empty() && held.iter().all(|k| k.epoch_b == Some(1980)), "one pair in memory: {held:?}");
    }

    /// R1-M-1's bound: new-pair keys that neither land nor fail (their
    /// jobs never return) hold the old pair for `STANDING_MAX_FRAMES`
    /// frames, then it is released and dropped all the same.
    #[test]
    fn the_old_pair_stands_in_for_a_bounded_number_of_frames() {
        let mut layers = DotLayers::new();
        let style = DotStyle { epoch: 1975, epoch_b: Some(2030), ..DotStyle::default() };
        let never = GridTileKey { tile: TileId { z: 0, x: 0, y: 0 }, level: 0, epoch: 0, epoch_b: None };
        layers.set_source("pop", Arc::new(FailsOne { d: d30(), bad: never }), style);
        for _ in 0..10 {
            pair_frame(&mut layers, true);
        }
        layers.set_epochs("pop", 1975, Some(1980));
        // The new pair's jobs are handed out and never landed.
        while layers.next_job().is_some() {}
        for frame in 1..=STANDING_MAX_FRAMES {
            pair_frame(&mut layers, false);
            let draw = &layers.plan("pop").unwrap().draw;
            assert!(!draw.is_empty() && draw.iter().all(|x| x.key.epoch_b == Some(2030)), "frame {frame}: the old pair stands in");
        }
        pair_frame(&mut layers, false);
        assert!(layers.plan("pop").unwrap().draw.is_empty(), "released: the new pair holds nothing yet, a hole as for any missing tile");
        assert!(layers.held_keys("pop").is_empty(), "the old pair's tiles are dropped");
    }

    /// R1-N-1: the ppd holds through a hole only while the view's tiles
    /// are loading; a settled view with no people (open ocean) eases toward 1
    /// as before.
    #[test]
    fn an_empty_settled_view_eases_the_ppd_and_a_loading_hole_holds_it() {
        let src: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d30(), |k| {
            if k.tile.x >= 50 { GridTile::default() } else { every_cell(&d30(), k.level, k.tile) }
        }));
        let t0 = crate::timing::Instant::now();
        let at = |ms: u64| t0.after(Duration::from_millis(ms));
        let land_view = ([40.0 / 64.0, 20.0 / 64.0], [41.0 / 64.0, 21.0 / 64.0]);
        let sea_view = ([50.0 / 64.0, 20.0 / 64.0], [51.0 / 64.0, 21.0 / 64.0]);
        let (land, sea) = (TileId { z: 6, x: 40, y: 20 }, TileId { z: 6, x: 50, y: 20 });
        let run = |layers: &mut DotLayers| {
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        };
        let mut layers = DotLayers::new();
        layers.set_source("pop", src, DotStyle { budget_dots: 1000, ..DotStyle::default() });
        let mut ms = 0;
        for _ in 0..10 {
            ms += 16;
            layers.frame_with_cover(6.2, |_| vec![land], land_view, at(ms));
            run(&mut layers);
        }
        let seeded = layers.stats("pop").unwrap().ppd;
        assert!(seeded > 10.0, "{seeded}");
        // Over the sea with its tiles in flight (jobs handed out, not
        // landed): nothing drawn, the ppd holds.
        ms += 16;
        layers.frame_with_cover(6.2, |_| vec![sea], sea_view, at(ms));
        let jobs: Vec<DotJob> = std::iter::from_fn(|| layers.next_job()).collect();
        assert!(!jobs.is_empty());
        for _ in 0..30 {
            ms += 16;
            layers.frame_with_cover(6.2, |_| vec![sea], sea_view, at(ms));
            assert_eq!(layers.stats("pop").unwrap().ppd, seeded, "a loading hole holds the ppd");
        }
        // Landed and empty: nothing in flight, the ppd eases toward 1.
        for job in jobs {
            layers.land(job.run(|_| Ok(Vec::new())));
        }
        for _ in 0..120 {
            ms += 16;
            layers.frame_with_cover(6.2, |_| vec![sea], sea_view, at(ms));
            run(&mut layers);
        }
        let eased = layers.stats("pop").unwrap().ppd;
        assert!(eased < seeded / 10.0, "an empty settled view eases: {seeded} -> {eased}");
    }

    /// Review I-1: the counter reads the view the dots draw. A live layer
    /// at mix 1 counts epoch B's people (the same as a layer of epoch B
    /// alone), at 0.5 the midpoint of A's and B's, at 0 A's -- for a view
    /// that cuts the tile (the cell index) and one that holds it whole
    /// (the tile totals). `pop_drawn` and `dots_in_view` follow.
    #[test]
    fn people_in_view_follows_the_epoch_mix() {
        let t = TileId { z: 6, x: 40, y: 20 };
        let (n, d) = (64.0, d30());
        let (x0, y0) = (f64::from(t.x) / n, f64::from(t.y) / n);
        let cut = ([x0 + 0.3 / n, y0 + 0.2 / n], [x0 + 0.7 / n, y0 + 0.9 / n]);
        let whole = ([x0 - 0.5 / n, y0 - 0.5 / n], [x0 + 1.5 / n, y0 + 1.5 / n]);
        let only_a: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d30(), |k| every_cell(&d30(), k.level, k.tile)));
        let only_b: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d30(), |k| {
            let b = every_cell_b(&d30(), k.level, k.tile);
            let cells: Vec<(u32, f32, f32)> = b.cells.iter().filter(|c| c.2 > 0.0).map(|&(i, _, pb)| (i, pb, pb)).collect();
            let total = cells.iter().map(|c| f64::from(c.1)).sum();
            GridTile { cells, total, ..Default::default() }
        }));
        let live: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d, |k| every_cell_b(&d30(), k.level, k.tile)));
        let pair = |mix: f32| DotStyle { epoch: 1975, epoch_b: Some(2030), epoch_mix: mix, ..DotStyle::default() };
        let close = |got: f64, want: f64, what: &str| {
            assert!((got - want).abs() <= 1e-9 * want.abs().max(1.0), "{what}: {got} vs {want}");
        };
        for (name, view) in [("cut", cut), ("whole", whole)] {
            let a = settled_one(Arc::clone(&only_a), DotStyle::default(), t, view);
            let b = settled_one(Arc::clone(&only_b), DotStyle::default(), t, view);
            assert!(a.people_in_view > 0.0 && (a.people_in_view - b.people_in_view).abs() > 1.0, "{name}: A and B differ");
            let at = |mix: f32| settled_one(Arc::clone(&live), pair(mix), t, view);
            let (l0, l1, lh) = (at(0.0), at(1.0), at(0.5));
            close(l0.people_in_view, a.people_in_view, &format!("{name}: t 0 is epoch A"));
            close(l1.people_in_view, b.people_in_view, &format!("{name}: t 1 is epoch B"));
            close(lh.people_in_view, 0.5 * (a.people_in_view + b.people_in_view), &format!("{name}: t 0.5 is the midpoint"));
            close(l1.pop_drawn, b.pop_drawn, &format!("{name}: pop_drawn at t 1"));
            close(lh.pop_drawn, 0.5 * (a.pop_drawn + b.pop_drawn), &format!("{name}: pop_drawn at t 0.5"));
            assert_eq!(l1.dots_in_view, (l1.people_in_view / f64::from(l1.ppd.max(1.0))).round() as u64);
        }
        // Moving the mix alone re-counts (the view cache keys on it).
        let mut layers = DotLayers::new();
        layers.set_source("pop", Arc::clone(&live), pair(0.0));
        for _ in 0..10 {
            layers.frame_with_cover(6.2, |_| vec![t], cut, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        let before = layers.stats("pop").unwrap().people_in_view;
        layers.set_epoch_mix("pop", 1.0);
        layers.frame_with_cover(6.2, |_| vec![t], cut, crate::timing::Instant::now());
        close(layers.stats("pop").unwrap().people_in_view, settled_one(only_b, DotStyle::default(), t, cut).people_in_view, "re-counted at t 1");
        assert!(before != layers.stats("pop").unwrap().people_in_view);
    }

    fn one_tile_layers(t: TileId, viewport: ([f64; 2], [f64; 2])) -> DotStats {
        let src: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d30(), move |k| every_cell(&d30(), k.level, k.tile)));
        let mut layers = DotLayers::new();
        layers.set_source("pop", src, DotStyle::default());
        for _ in 0..10 {
            layers.frame_with_cover(6.2, |_| vec![t], viewport, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        layers.stats("pop").unwrap()
    }

    /// A second defect (street zoom, central Nizhny Novgorod):
    /// "0 people in view · 0 dots" with dots on screen -- a view smaller
    /// than one 30″ cell held no cell CENTRE. A cell counts by the share
    /// of its area in view: a view inside one cell is that cell's
    /// population times the view's share of it.
    #[test]
    fn a_view_inside_one_cell_counts_its_share_of_the_cell() {
        let d = d30();
        let t = TileId { z: 6, x: 40, y: 20 };
        let w = grid::cell_window(&d, 0, t);
        let (row, col) = (w.row0 + 100, w.col0 + 200);
        let (x0, y1) = grid::cell_sw_mercator(&d, 0, row, col);
        let (x1, y0) = grid::cell_sw_mercator(&d, 0, row - 1, col + 1);
        let (cw, ch) = (x1 - x0, y1 - y0);
        // A tenth of the cell's width and an eighth of its height, inside it.
        let b = ([x0 + 0.3 * cw, y0 + 0.4 * ch], [x0 + 0.4 * cw, y0 + 0.525 * ch]);
        let stats = one_tile_layers(t, b);
        let pop = every_cell(&d, 0, t).cells[grid::local_index(&w, row, col) as usize].1;
        let expected = f64::from(pop) * 0.1 * 0.125;
        assert!((stats.people_in_view - expected).abs() < 1e-6 * expected, "{} people in view, expected {expected}", stats.people_in_view);
    }

    /// A view straddling a lattice corner takes its share of each of the
    /// four cells around it.
    #[test]
    fn a_view_across_a_cell_corner_sums_four_shares() {
        let d = d30();
        let t = TileId { z: 6, x: 40, y: 20 };
        let w = grid::cell_window(&d, 0, t);
        let (row, col) = (w.row0 + 100, w.col0 + 200);
        // The corner shared by (row, col) and (row - 1, col + 1)'s SW.
        let (cx, cy) = grid::cell_sw_mercator(&d, 0, row - 1, col + 1);
        let (x0, y1) = grid::cell_sw_mercator(&d, 0, row, col);
        let (cw, ch) = (cx - x0, y1 - cy);
        let b = ([cx - 0.3 * cw, cy - 0.2 * ch], [cx + 0.1 * cw, cy + 0.45 * ch]);
        let stats = one_tile_layers(t, b);
        let tile = every_cell(&d, 0, t);
        let mut expected = 0.0;
        for (r, c) in [(row - 1, col), (row - 1, col + 1), (row, col), (row, col + 1)] {
            let share = cell_share(&d, 0, r, c, b);
            assert!(share > 0.01, "({r}, {c}) is under the view");
            expected += f64::from(tile.cells[grid::local_index(&w, r, c) as usize].1) * share;
        }
        assert!((stats.people_in_view - expected).abs() < 1e-6 * expected, "{} people in view, expected {expected}", stats.people_in_view);
    }

    /// The defect (Nizhny Novgorod: "1,350,908 dots drawn" for a
    /// city of 1.3 M at 1 dot = 7): the counter reads the people in the
    /// VIEW, not the drawn tiles' whole population. One z6 tile drawn, a
    /// box over part of it: every cell's share of its area in the box.
    #[test]
    fn people_in_view_are_the_cells_centred_in_the_viewport() {
        let d = d30();
        let t = TileId { z: 6, x: 40, y: 20 };
        let src: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d30(), move |k| every_cell(&d30(), k.level, k.tile)));
        let mut layers = DotLayers::new();
        layers.set_source("pop", src, DotStyle::default());
        let n = 64.0;
        // Edges off the cell lattice (0.3 of a z6 tile is exactly on a
        // column centre -- a tie either count may break its own way).
        let viewport = ([(40.0 + 0.3037) / n, (20.0 + 0.2513) / n], [(40.0 + 0.5531) / n, (20.0 + 0.6019) / n]);
        for _ in 0..10 {
            layers.frame_with_cover(6.2, |_| vec![t], viewport, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        let plan = layers.plan("pop").unwrap().clone();
        assert_eq!(plan.draw.len(), 1, "{:?}", plan.draw);
        assert!(!plan.draw[0].standin);
        let stats = layers.stats("pop").unwrap();
        let expected = brute_people(&d, &key(t, 0), &every_cell(&d, 0, t), viewport, &|_, _| true);
        assert!(expected > 1_000.0 && expected < every_cell(&d, 0, t).total * 0.2, "a real part of the tile: {expected}");
        assert!((stats.people_in_view - expected).abs() < 1e-6, "{} people in view, expected {expected}", stats.people_in_view);
        assert_eq!(stats.dots_in_view, (expected / f64::from(stats.ppd)).round() as u64);
    }

    /// A stand-in and the ready tiles beside it are counted once: the
    /// parent's cells that its exclusion boxes keep from drawing (those
    /// centred in a ready child) are not in view, and the ready children
    /// count their own cells.
    #[test]
    fn people_in_view_count_a_stand_in_once() {
        let d = d30();
        let parent_tile = TileId { z: 5, x: 16, y: 16 };
        let parent = key(parent_tile, 1);
        let kids = children(parent_tile);
        let ready_kids = [kids[0], kids[3]];
        let ready = |k: &GridTileKey| *k == parent || (k.level == 0 && ready_kids.contains(&k.tile));
        let plan = plan_dots(&d, 2025, 5.5, &kids, &ready);
        assert_eq!(plan.draw.len(), 3);
        let mut tiles = HashMap::new();
        for draw in &plan.draw {
            let tile = Arc::new(every_cell(&d, draw.key.level, draw.key.tile));
            let w = grid::cell_window(&d, draw.key.level, draw.key.tile);
            let index = Arc::new(super::super::in_view::CellIndex::new(&tile, &w));
            tiles.insert(draw.key, HeldTile { slot_pop: tile.total, total_b: tile.total, tile, blocks: Arc::new(TileBlocks::default()), index: index.into(), used: 0 });
        }
        // A box across the middle of the parent: parts of all four kids.
        let viewport = ([0.5 + 0.2037 / 32.0, 0.5 + 0.3013 / 32.0], [0.5 + 0.7031 / 32.0, 0.5 + 0.8019 / 32.0]);
        let view = super::super::in_view::ViewRegion::mercator_box(viewport.0, viewport.1);
        let got = view_pop(&d, &plan, &tiles, &view);
        let mut expected = 0.0;
        for kid in ready_kids {
            expected += brute_people(&d, &key(kid, 0), &tiles[&key(kid, 0)].tile, viewport, &|_, _| true);
        }
        expected += brute_people(&d, &parent, &tiles[&parent].tile, viewport, &|lon, lat| !ready_kids.contains(&z6_of(lon, lat)));
        assert!(expected > 10_000.0, "{expected}");
        assert!((got - expected).abs() < 1e-6, "{got} people in view, expected {expected}");
    }

    /// The RSS fix: a drawn tile the view's boundary does not cut builds
    /// no cell index -- inside it counts whole, outside not at all -- and
    /// one it cuts builds its index once, on the held tile, and reuses it.
    #[test]
    fn only_a_tile_the_view_cuts_builds_a_cell_index_once() {
        let d = d30();
        let t = TileId { z: 6, x: 40, y: 20 };
        let k = key(t, 0);
        let tile = Arc::new(every_cell(&d, 0, t));
        let mut tiles = HashMap::new();
        tiles.insert(k, HeldTile { slot_pop: tile.total, total_b: tile.total, tile: Arc::clone(&tile), blocks: Arc::new(TileBlocks::default()), index: HeldCells::default(), used: 0 });
        let plan = DotPlan { wanted: vec![k], draw: vec![DotDraw { key: k, exclude: [DotParams::NONE_EXCLUDED; 2], standin: false }], ..Default::default() };
        let n = 64.0;
        let boxed = |b: ([f64; 2], [f64; 2])| in_view::ViewRegion::mercator_box(b.0, b.1);
        let around = ([(40.0 - 0.2) / n, (20.0 - 0.2) / n], [(41.0 + 0.2) / n, (21.0 + 0.2) / n]);
        let got = view_pop(&d, &plan, &tiles, &boxed(around));
        assert!((got - tile.total).abs() < 1e-6, "inside: {got} vs {}", tile.total);
        assert!(tiles[&k].index.index.get().is_none(), "a tile inside the view builds no index");
        let away = ([(42.0 + 0.1) / n, (20.0) / n], [(43.0) / n, (21.0) / n]);
        assert_eq!(view_pop(&d, &plan, &tiles, &boxed(away)), 0.0);
        assert!(tiles[&k].index.index.get().is_none(), "a tile outside the view builds no index");
        let cut = ([(40.0 + 0.3037) / n, (20.0 + 0.2513) / n], [(40.0 + 0.5531) / n, (20.0 + 0.6019) / n]);
        let got = view_pop(&d, &plan, &tiles, &boxed(cut));
        let expected = brute_people(&d, &k, &tile, cut, &|_, _| true);
        assert!((got - expected).abs() < 1e-6, "{got} vs {expected}");
        let built = Arc::as_ptr(tiles[&k].index.index.get().expect("a tile the view cuts builds its index"));
        let cut2 = ([(40.0 + 0.1037) / n, (20.0 + 0.4513) / n], [(40.0 + 0.7531) / n, (20.0 + 1.6019) / n]);
        let got = view_pop(&d, &plan, &tiles, &boxed(cut2));
        let expected = brute_people(&d, &k, &tile, cut2, &|_, _| true);
        assert!((got - expected).abs() < 1e-6, "{got} vs {expected}");
        assert_eq!(Arc::as_ptr(tiles[&k].index.index.get().unwrap()), built, "built once, then reused");
    }

    /// A stand-in inside the view counts its whole population less the
    /// cells its boxes exclude, with no index: the same once-only count
    /// as [`people_in_view_count_a_stand_in_once`], cell by cell.
    #[test]
    fn a_stand_in_inside_the_view_counts_once_without_an_index() {
        let d = d30();
        let parent_tile = TileId { z: 5, x: 16, y: 16 };
        let parent = key(parent_tile, 1);
        let kids = children(parent_tile);
        let ready_kids = [kids[0], kids[3]];
        let ready = |k: &GridTileKey| *k == parent || (k.level == 0 && ready_kids.contains(&k.tile));
        let plan = plan_dots(&d, 2025, 5.5, &kids, &ready);
        assert_eq!(plan.draw.len(), 3);
        let mut tiles = HashMap::new();
        for draw in &plan.draw {
            let tile = Arc::new(every_cell(&d, draw.key.level, draw.key.tile));
            tiles.insert(draw.key, HeldTile { slot_pop: tile.total, total_b: tile.total, tile, blocks: Arc::new(TileBlocks::default()), index: HeldCells::default(), used: 0 });
        }
        let viewport = ([0.5 - 0.1 / 32.0, 0.5 - 0.1 / 32.0], [0.5 + 1.1 / 32.0, 0.5 + 1.1 / 32.0]);
        let view = in_view::ViewRegion::mercator_box(viewport.0, viewport.1);
        let mut cache = ViewCache::default();
        let got = cached_view_pop(&mut cache, &d, &plan, &tiles, &view, 0.0).0;
        let mut expected = 0.0;
        for kid in ready_kids {
            expected += brute_people(&d, &key(kid, 0), &tiles[&key(kid, 0)].tile, viewport, &|_, _| true);
        }
        expected += brute_people(&d, &parent, &tiles[&parent].tile, viewport, &|lon, lat| !ready_kids.contains(&z6_of(lon, lat)));
        assert!((got - expected).abs() < 1e-6, "{got} people in view, expected {expected}");
        assert!(tiles.values().all(|h| h.index.index.get().is_none()), "no tile inside the view builds an index");
        assert_eq!(cache.excluded.len(), 3, "each inside draw's excluded people are kept for the next frame");
        // The view moves, still around every tile: the same count, from
        // the kept exclusions.
        let moved = in_view::ViewRegion::mercator_box([viewport.0[0] - 1e-4, viewport.0[1]], viewport.1);
        let again = cached_view_pop(&mut cache, &d, &plan, &tiles, &moved, 0.0).0;
        assert!((again - expected).abs() < 1e-6, "{again} vs {expected}");
    }

    /// A cell index costs about 6 bytes a populated cell (a `u16` column
    /// and the cell's `f32` people), an `f64` every 64 cells and 4 bytes a
    /// window row, with no slack: half of the 12 a cell (`u32` index,
    /// `f64` sum) it had.
    #[test]
    fn a_cell_index_is_about_six_bytes_a_cell() {
        let d = d30();
        let t = TileId { z: 6, x: 40, y: 20 };
        let w = grid::cell_window(&d, 0, t);
        let bound = |cells: usize| 6 * cells + 8 * (cells / 64 + 2) + 4 * (w.rows as usize + 1);
        let tile = every_cell(&d, 0, t);
        let index = CellIndex::new(&tile, &w);
        let cells = tile.cells.len();
        assert!(index.heap_bytes() <= bound(cells), "{} bytes for {cells} cells, {} rows: over {}", index.heap_bytes(), w.rows, bound(cells));
        // Every eighth cell: the index shrinks with the populated cells.
        let sparse = GridTile { cells: tile.cells.iter().copied().step_by(8).collect(), total: 0.0, ..Default::default() };
        let index = CellIndex::new(&sparse, &w);
        assert!(index.heap_bytes() <= bound(sparse.cells.len()), "{} bytes: over {}", index.heap_bytes(), bound(sparse.cells.len()));
    }

    /// Large, fractional populations in long rows (a z4 window: 2,700
    /// columns) read back as an f64 walk of the cells does: the edge-tile
    /// path within 1e-6 of the true people in view for wide, narrow and
    /// one-cell views, and any run of one row's columns within 1e-6 of its
    /// own sum.
    #[test]
    fn a_cell_index_keeps_large_fractional_populations_exact() {
        let d = d30();
        let t = TileId { z: 4, x: 10, y: 5 };
        let k = key(t, 0);
        let w = grid::cell_window(&d, 0, t);
        assert!(w.cols > 2_000, "{} columns", w.cols);
        let mut seed = 0x5eed_u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        // 160 rows in the middle of the window, a third of their cells
        // empty, the rest up to 2.5e7 with a fraction.
        let (band0, band) = (w.rows / 2, 160);
        let mut cells = Vec::new();
        for r in band0..band0 + band {
            for c in 0..w.cols {
                if rnd() < 0.33 {
                    continue;
                }
                let pop = (rnd() * 2.5e7).floor() + rnd();
                cells.push((r * w.cols + c, pop as f32, pop as f32));
            }
        }
        let total = cells.iter().map(|c| f64::from(c.1)).sum();
        let tile = GridTile { cells, total, ..Default::default() };
        let index = CellIndex::new(&tile, &w);
        // Row runs against the cells summed in f64.
        let mut worst_row: f64 = 0.0;
        for _ in 0..20_000 {
            let r = band0 + (rnd() * f64::from(band)) as u32;
            let c0 = (rnd() * f64::from(w.cols)) as u32;
            let c1 = (c0 + 1 + (rnd() * f64::from(w.cols - c0)) as u32).min(w.cols);
            let (i0, i1) = (r * w.cols + c0, r * w.cols + c1);
            let (a, b) = (tile.cells.partition_point(|c| c.0 < i0), tile.cells.partition_point(|c| c.0 < i1));
            let want: f64 = tile.cells[a..b].iter().map(|c| f64::from(c.1)).sum();
            let got = index.pop_in_row(r, c0, c1);
            if want > 0.0 {
                worst_row = worst_row.max((got - want).abs() / want);
            }
        }
        // Views: the whole band, a column and a half down it, a few cells,
        // then random boxes; mercator y of a window row's north edge.
        let row_y = |r: u32| grid::cell_sw_mercator(&d, 0, w.row0 + r - 1, w.col0).1;
        let col_x = |c: f64| grid::cell_sw_mercator(&d, 0, w.row0, w.col0).0 + c * (grid::cell_sw_mercator(&d, 0, w.row0, w.col0 + 1).0 - grid::cell_sw_mercator(&d, 0, w.row0, w.col0).0);
        let (y0, y1) = (row_y(band0) + 1e-7, row_y(band0 + band) - 1e-7);
        let mut views = vec![
            ([col_x(10.3), y0], [col_x(2_600.7), y1]),
            ([col_x(1_200.25), y0], [col_x(1_201.75), y1]),
            ([col_x(700.4), row_y(band0 + 50) + 1e-7], [col_x(703.6), row_y(band0 + 53) - 1e-7]),
        ];
        for _ in 0..12 {
            let (a, b) = (rnd() * f64::from(w.cols), rnd() * 40.0);
            let (ra, rb) = (rnd(), rnd());
            views.push(([col_x(a), y0 + (y1 - y0) * ra.min(rb)], [col_x((a + b).min(f64::from(w.cols))), y0 + (y1 - y0) * ra.max(rb)]));
        }
        let (mut worst_abs, mut worst_rel): (f64, f64) = (0.0, 0.0);
        for v in views {
            let got = in_view::pop_in_view(&d, &k, &index, &[DotParams::NONE_EXCLUDED; 2], &in_view::ViewRegion::mercator_box(v.0, v.1));
            let want = brute_people(&d, &k, &tile, v, &|_, _| true);
            assert!(want > 0.0, "{v:?}");
            worst_abs = worst_abs.max((got - want).abs());
            worst_rel = worst_rel.max((got - want).abs() / want);
        }
        println!("cell index precision: views worst abs {worst_abs:.3e} rel {worst_rel:.3e}; row runs worst rel {worst_row:.3e}");
        assert!(worst_rel < 1e-6, "views: worst relative error {worst_rel:.3e} (abs {worst_abs:.3e})");
        assert!(worst_row < 1e-6, "row runs: worst relative error {worst_row:.3e}");
    }

    #[test]
    fn the_layers_credit_the_descriptors_attribution() {
        let mut layers = DotLayers::new();
        assert!(layers.attributions().is_empty());
        layers.set_source("pop", small_source(), DotStyle { relocate: false, ..DotStyle::default() });
        assert_eq!(layers.attributions(), vec![GHSL_ATTRIBUTION.to_string()]);
    }

    #[test]
    fn relocation_appends_the_illustrative_note_to_the_dot_credit_only() {
        let mut layers = DotLayers::new();
        layers.set_source("pop", small_source(), DotStyle::default());
        assert!(DotStyle::default().relocate, "relocation is on by default (ruling)");
        assert_eq!(
            layers.attributions(),
            vec!["GHS-POP R2023A, European Commission JRC; dot positions within each cell are illustrative".to_string()]
        );
        layers.set_source("pop", small_source(), DotStyle { relocate: false, ..DotStyle::default() });
        assert_eq!(layers.attributions(), vec![GHSL_ATTRIBUTION.to_string()]);
        // One candidate is today's placement: no note.
        layers.set_source("pop", small_source(), DotStyle { mask_k: 1, ..DotStyle::default() });
        assert_eq!(layers.attributions(), vec![GHSL_ATTRIBUTION.to_string()]);
    }

    #[test]
    fn two_layers_sharing_a_credit_read_it_once_in_either_order() {
        let noted = format!("{GHSL_ATTRIBUTION}; {ILLUSTRATIVE_PLACEMENT_NOTE}");
        for plain_first in [true, false] {
            let mut layers = DotLayers::new();
            let plain = DotStyle { relocate: false, ..DotStyle::default() };
            let (a, b) = if plain_first { (plain, DotStyle::default()) } else { (DotStyle::default(), plain) };
            layers.set_source("a", small_source(), a);
            layers.set_source("b", small_source(), b);
            assert_eq!(layers.attributions(), vec![noted.clone()], "plain first: {plain_first}");
        }
    }

    /// A layer settled on one z6 data tile (every cell populated) at `zoom`.
    fn settled(style: DotStyle, zoom: f64) -> (DotLayers, TileId) {
        let t = z6_of(44.0, 56.3);
        let src: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d30(), move |k| every_cell(&d30(), k.level, k.tile)));
        let mut layers = DotLayers::new();
        layers.set_source("pop", src, style);
        let viewport = ([0.62, 0.31], [0.63, 0.32]);
        for _ in 0..10 {
            layers.frame_with_cover(zoom, |_| vec![t], viewport, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        (layers, t)
    }

    /// A mask grid over tile `t`.
    fn grid_over(t: TileId, zoom: f64) -> crate::render::dot_mask::MaskGrid {
        let n = f64::from(1u32 << t.z);
        let region = [f64::from(t.x) / n, f64::from(t.y) / n, f64::from(t.x + 1) / n, f64::from(t.y + 1) / n];
        crate::render::dot_mask::MaskGrid::plan(zoom, region, 0.0, 4096).expect("a grid")
    }

    #[test]
    fn relocate_off_writes_mask_k_zero_on_every_draw() {
        let zoom = 15.0;
        let (on, t) = settled(DotStyle::default(), zoom);
        let grid = grid_over(t, zoom);
        let drawn = on.draw_params("pop", zoom, 0.0, Some(&grid));
        assert!(!drawn.is_empty(), "the layer draws nothing");
        // Not vacuous: relocation on moves every draw at z15 (30″ cells are
        // ~390 px there).
        assert!(drawn.iter().all(|p| p.mask_k == crate::render::dots::DEFAULT_MASK_K_30SS), "{drawn:?}");
        let (off, _) = settled(DotStyle { relocate: false, ..DotStyle::default() }, zoom);
        let drawn = off.draw_params("pop", zoom, 0.0, Some(&grid));
        assert!(!drawn.is_empty());
        for p in &drawn {
            assert_eq!(p.mask_k, 0);
        }
        // No mask bound, or a globe frame: 0 as well.
        assert!(on.draw_params("pop", zoom, 0.0, None).iter().all(|p| p.mask_k == 0));
        assert!(on.draw_params("pop", zoom, 0.5, Some(&grid)).iter().all(|p| p.mask_k == 0));
    }

    #[test]
    fn below_z15_every_draw_writes_mask_k_zero() {
        // A 30″ cell at z6 is 2^6 * 512 / 43,200 = 0.76 px wide: under the
        // 2-px floor. At z8 (3 px) and z14.99 (~390 px) the cells are big
        // enough, but relocation starts at z15 (Task 9 ruling).
        let zoom = 6.0;
        let (layers, t) = settled(DotStyle::default(), zoom);
        let grid = grid_over(t, zoom);
        for z in [6.0, 8.0, 14.99] {
            let drawn = layers.draw_params("pop", z, 0.0, Some(&grid));
            assert!(!drawn.is_empty(), "the layer draws nothing");
            assert!(drawn.iter().all(|p| p.mask_k == 0), "z{z} relocated");
        }
        assert!(layers.draw_params("pop", 15.0, 0.0, Some(&grid)).iter().all(|p| p.mask_k > 1), "z15 relocates");
    }

    /// Relocation is off above 30 deg pitch (Task 9 ruling), mask bound or
    /// not: a pitched pan moves no dot by construction.
    #[test]
    fn above_thirty_degrees_of_pitch_every_draw_writes_mask_k_zero() {
        let zoom = 15.0;
        let (layers, t) = settled(DotStyle::default(), zoom);
        let grid = grid_over(t, zoom);
        let layer = layers.layers.iter().find(|l| l.id == "pop").expect("the layer");
        let k_at = |pitch_deg: f32| -> Vec<u32> {
            layer
                .plan
                .draw
                .iter()
                .filter_map(|draw| {
                    let held = layer.tiles.get(&draw.key)?;
                    let (params, _) = DotParams::for_tile(draw.key.tile, &held.blocks, draw.exclude, layer.tile_ppd(), 1.0);
                    Some(layer.style.relocated(params, draw.key.tile, zoom, pitch_deg.to_radians(), 0.0, Some(&grid)).mask_k)
                })
                .collect()
        };
        assert!(!k_at(0.0).is_empty() && k_at(30.0).iter().all(|&k| k > 1), "flat and 30 deg relocate");
        for pitch in [30.5, 45.0, 60.0] {
            assert!(k_at(pitch).iter().all(|&k| k == 0), "{pitch} deg relocated");
        }
    }

    // ---- Task 7: race archives (palette, legend mask, part tiles) ----

    /// The race archive's descriptor (C7): 3″, 10 levels, level 0 at z9,
    /// part tiles at z12 from zoom 9.72.
    fn race_descriptor() -> crate::overlay::dotgrid::RaceDescriptor {
        crate::overlay::dotgrid::RaceDescriptor {
            grid: GridDescriptor {
                base_arcsec: 3.0,
                origin_lat: 90.0,
                origin_lon: -180.0,
                levels: 10,
                max_data_zoom: 9,
                epochs: vec![2020],
                attribution: "U.S. Census Bureau".to_string(),
            },
            groups: 8,
            part_zoom: 12,
            crossover: 9.72,
        }
    }

    /// The z12 tile the race tests look at (central US), and its 2x2
    /// block `RACE_TILES` a side.
    const RACE_ORIGIN: TileId = TileId { z: 12, x: 1050, y: 1520 };

    /// `people` people in a part tile, as 64 small triangles on an 8x8
    /// grid inside it, every group present (`counts[g]` grows with `g`).
    fn race_parts(tile: TileId, people: u32) -> GridTile {
        let n = f64::from(1u32 << tile.z);
        let per = people / 64;
        let mut parts = Vec::new();
        for i in 0..64u32 {
            let (cx, cy) = (f64::from(i % 8) + 0.5, f64::from(i / 8) + 0.5);
            let at = |dx: f64, dy: f64| [(f64::from(tile.x) + (cx + dx) / 8.0) / n, (f64::from(tile.y) + (cy + dy) / 8.0) / n];
            let mut counts = [0u32; 8];
            let mut left = per;
            for (g, c) in counts.iter_mut().enumerate().rev() {
                *c = if g == 0 { left } else { (per * (g as u32 + 1) / 36).min(left) };
                left -= *c;
            }
            parts.push(crate::overlay::dotgrid::codec_cat::PartRecord { a: at(-0.3, -0.3), b: at(0.3, -0.3), c: at(-0.3, 0.3), counts });
        }
        let total = parts.iter().map(|p| p.counts.iter().map(|&c| f64::from(c)).sum::<f64>()).sum();
        GridTile { parts, total, ..Default::default() }
    }

    /// A categorical cell tile: a few populated cells of `people` each.
    fn race_cells(d: &GridDescriptor, key: &GridTileKey, people: u32) -> GridTile {
        let w = grid::cell_window(d, key.level, key.tile);
        let cells: Vec<u32> = (0..8).map(|i| (w.rows / 2 + i) * w.cols + w.cols / 2).filter(|&i| i < w.rows * w.cols).collect();
        let cats: Vec<[u32; 8]> = cells.iter().map(|_| [people / 8; 8]).collect();
        let cells = cells.iter().zip(&cats).map(|(&i, c)| {
            let pop = c.iter().sum::<u32>() as f32;
            (i, pop, pop)
        });
        let cells: Vec<(u32, f32, f32)> = cells.collect();
        let total = cells.iter().map(|c| f64::from(c.1)).sum();
        GridTile { cells, total, cats, ..Default::default() }
    }

    /// Race layers over a seeded in-memory race source: `people` in every
    /// part tile, categorical cells at every level, drawn with
    /// [`DotStyle::race`].
    fn synthetic_race_layers(people: u32) -> DotLayers {
        let race = race_descriptor();
        let d = race.grid.clone();
        let source = MemoryGridSource::for_race(race, move |k| {
            if k.is_part() {
                race_parts(k.tile, people)
            } else {
                race_cells(&d, k, people / 16)
            }
        });
        let mut layers = DotLayers::new();
        layers.set_source("race", Arc::new(source), DotStyle::race(Platform::Native));
        layers
    }

    /// What one settled frame of layer "race" writes.
    struct RaceFrame {
        ppd: f32,
        params: Vec<DotParams>,
        stats: DotStats,
    }

    /// Settles layer "race" at camera `zoom` over [`RACE_ORIGIN`]'s 2x2
    /// block (as its ancestors below z12, as its descendants above): the
    /// view is the block, and the cover at any zoom is what the block
    /// touches there.
    fn plan_frame(layers: &mut DotLayers, zoom: f64) -> RaceFrame {
        let n12 = f64::from(1u32 << 12);
        let view = ([f64::from(RACE_ORIGIN.x) / n12, f64::from(RACE_ORIGIN.y) / n12], [f64::from(RACE_ORIGIN.x + 2) / n12, f64::from(RACE_ORIGIN.y + 2) / n12]);
        let cover_at = |z: u8| -> Vec<TileId> {
            let n = f64::from(1u32 << z);
            let (x0, y0) = ((view.0[0] * n).floor() as u32, (view.0[1] * n).floor() as u32);
            let (x1, y1) = (((view.1[0] * n).ceil() as u32).max(x0 + 1), ((view.1[1] * n).ceil() as u32).max(y0 + 1));
            (y0..y1).flat_map(|y| (x0..x1).map(move |x| TileId { z, x, y })).collect()
        };
        for _ in 0..10 {
            layers.frame_with_cover(zoom, cover_at, view, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        let stats = layers.stats("race").expect("the race layer");
        RaceFrame { ppd: stats.ppd, params: layers.frame_params("race", zoom, 0.0, 0.0, None), stats }
    }

    /// Settles layer "race" at `zoom` in a `w`x`h` px window centred on
    /// [`RACE_ORIGIN`]'s 2x2 block (256 px tiles), with the cover at any
    /// zoom the tiles the window touches.
    fn window_frame(layers: &mut DotLayers, zoom: f64, w: f64, h: f64) -> DotStats {
        window_frame_at(layers, zoom, w, h, (1.0, 1.0))
    }

    /// [`window_frame`] centred `at` z12 tiles from [`RACE_ORIGIN`]'s NW
    /// corner ((1, 1): the 2x2 block's middle; (0.5, 0.5): the middle of
    /// its NW tile).
    fn window_frame_at(layers: &mut DotLayers, zoom: f64, w: f64, h: f64, at: (f64, f64)) -> DotStats {
        let n12 = f64::from(1u32 << 12);
        let centre = [(f64::from(RACE_ORIGIN.x) + at.0) / n12, (f64::from(RACE_ORIGIN.y) + at.1) / n12];
        let world_px = 256.0 * 2f64.powf(zoom);
        let (hw, hh) = (w / 2.0 / world_px, h / 2.0 / world_px);
        let view = ([centre[0] - hw, centre[1] - hh], [centre[0] + hw, centre[1] + hh]);
        let cover_at = |z: u8| -> Vec<TileId> {
            let n = f64::from(1u32 << z);
            let (x0, y0) = ((view.0[0] * n).floor() as u32, (view.0[1] * n).floor() as u32);
            let (x1, y1) = (((view.1[0] * n).ceil() as u32).max(x0 + 1), ((view.1[1] * n).ceil() as u32).max(y0 + 1));
            (y0..y1).flat_map(|y| (x0..x1).map(move |x| TileId { z, x, y })).collect()
        };
        let t0 = crate::timing::Instant::now();
        for i in 0..200u64 {
            let now = t0.after(Duration::from_millis(100 * i));
            layers.frame_with_cover(zoom, cover_at, view, now);
            // Bounded: a tile that never satisfies `needs_rebuild` (built
            // floored above the view's ppd, review F3's regression) asks
            // for a rebuild forever -- fail, don't hang.
            let mut jobs = 0;
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
                jobs += 1;
                assert!(jobs < 10_000, "frame {i}: jobs never run out (a rebuild loop)");
            }
        }
        layers.stats("race").expect("the race layer")
    }

    /// The dots the frame's drawn tiles make alive at its ppd: every
    /// drawn tile whole, on screen or not (what MG-8 costs).
    fn drawn_dots(layers: &DotLayers) -> f64 {
        let layer = &layers.layers[0];
        drawn_pop(&layer.plan, &layer.tiles) / f64::from(layer.tile_ppd())
    }

    /// T8 fix round 0 (controller ruling): part tiles are budgeted by the
    /// people in view, so a dense view (700 k people per z12 tile, ~13,000
    /// per km² at 40°N) reaches one dot per person by z14 in a 1280x800
    /// window under the native 1.5 M budget -- where budgeting the drawn
    /// part tiles whole held it at 1.9 (4 tiles, 2.8 M). Review F1: never
    /// more than 2x the budget drawn on the way.
    #[test]
    fn a_dense_view_reaches_one_dot_per_person_by_z14() {
        let mut layers = synthetic_race_layers(700_000);
        let budget = f64::from(DotStyle::race(Platform::Native).budget_dots);
        assert_eq!(budget, 1_500_000.0);
        let mut ppd_at = Vec::new();
        for zoom in [12.0, 13.0, 13.5, 14.0] {
            let s = window_frame(&mut layers, zoom, 1280.0, 800.0);
            assert_eq!(s.level, GridTileKey::PART_LEVEL, "z{zoom} draws parts");
            // The ppd budgets what is on screen, never less.
            assert!(f64::from(s.ppd) + 1e-3 >= s.people_in_view / budget, "z{zoom}: ppd {} for {} in view", s.ppd, s.people_in_view);
            let drawn = drawn_dots(&layers);
            assert!(drawn <= PART_DRAWN_OVER_BUDGET * budget * 1.001, "z{zoom}: {drawn:.0} dots drawn");
            ppd_at.push((zoom, s.ppd));
        }
        let layer = &layers.layers[0];
        let whole = drawn_pop(&layer.plan, &layer.tiles);
        assert!(whole / budget > 1.5, "z14: the drawn part tiles hold {whole} people -- whole-tile budgeting would not reach 1");
        let at14 = ppd_at[3].1;
        assert_eq!(at14, 1.0, "one dot per person at z14 ({ppd_at:?})");
        assert!(ppd_at.windows(2).all(|w| w[1].1 <= w[0].1 + 1e-3), "the ppd falls as the view narrows: {ppd_at:?}");
    }

    /// T8 review F1 (controller ruling): budgeting part tiles by the people
    /// in view never lets the dots DRAWN -- every drawn tile's whole
    /// population, on screen or not -- exceed 2x `budget_dots`. A sparse
    /// view (the middle of one tile) among dense tiles, and the block's
    /// middle, where the view touches four.
    #[test]
    fn part_tiles_never_draw_more_than_twice_the_budget() {
        let budget = f64::from(DotStyle::race(Platform::Native).budget_dots);
        for (people, at) in [(4_000_000, (1.0, 1.0)), (4_000_000, (0.5, 0.5)), (2_000_000, (1.0, 1.0))] {
            let mut layers = synthetic_race_layers(people);
            for zoom in [12.5, 13.0, 14.0] {
                let s = window_frame_at(&mut layers, zoom, 1280.0, 800.0, at);
                assert_eq!(s.level, GridTileKey::PART_LEVEL);
                let drawn = drawn_dots(&layers);
                assert!(drawn <= PART_DRAWN_OVER_BUDGET * budget * 1.001, "{people} per tile at {at:?} z{zoom}: {drawn:.0} dots drawn, ppd {}", s.ppd);
            }
        }
    }

    /// T8 fix round 0, made to discriminate (review F3): a part tile
    /// holding more people than the budget is built at the VIEW's ppd, not
    /// floored at its whole population over the budget -- else it holds
    /// fewer slots than the view's ppd lights. 1.5 M people in one tile
    /// under a 1 M budget, a small window in its middle (nothing else
    /// drawn): the view's ppd is 1, the old floor 1.5 (`ppd_build` 1.06).
    #[test]
    fn a_part_tile_over_the_budget_is_built_at_the_views_ppd() {
        let mut layers = synthetic_race_layers(1_500_000);
        layers.layers[0].style.budget_dots = 1_000_000;
        layers.layers[0].ppd = PpdController::new(1_000_000.0);
        let s = window_frame_at(&mut layers, 14.0, 400.0, 400.0, (0.5, 0.5));
        let layer = &layers.layers[0];
        assert_eq!(layer.plan.draw.len(), 1, "one part tile drawn: {:?}", layer.plan.draw);
        assert_eq!(s.ppd, 1.0);
        let held = &layer.tiles[&layer.plan.draw[0].key];
        assert_eq!(held.blocks.ppd_build, 1.0, "built at the view's ppd, not the tile's 1.5 M over the 1 M budget");
    }

    /// T8 fix round 0 (F8, controller ruling): the crossover has a 0.1
    /// zoom hysteresis -- a layer drawing parts keeps them down to
    /// `crossover - 0.1`, one drawing cells keeps them up to
    /// `crossover + 0.1`; a first frame takes the plain rule.
    #[test]
    fn the_crossover_holds_its_side_for_a_tenth_of_a_zoom() {
        let r = race_descriptor();
        let parts = PlanKind::Parts { data_zoom: 12 };
        assert_eq!(race_plan_kind_after(&r, 9.72, None), parts);
        assert_eq!(race_plan_kind_after(&r, 9.71, None), PlanKind::Cells { level: 0 });
        assert_eq!(race_plan_kind_after(&r, 9.63, Some(true)), parts);
        assert_eq!(race_plan_kind_after(&r, 9.61, Some(true)), PlanKind::Cells { level: 0 });
        assert_eq!(race_plan_kind_after(&r, 9.81, Some(false)), PlanKind::Cells { level: 0 });
        assert_eq!(race_plan_kind_after(&r, 9.83, Some(false)), parts);

        // Through a layer: hovering +- 0.05 around the line never switches.
        let mut layers = synthetic_race_layers(64_000);
        let level_at = |layers: &mut DotLayers, z: f64| window_frame(layers, z, 1280.0, 800.0).level;
        assert_eq!(level_at(&mut layers, 10.0), GridTileKey::PART_LEVEL);
        for z in [9.67, 9.77, 9.67, 9.70] {
            assert_eq!(level_at(&mut layers, z), GridTileKey::PART_LEVEL, "parts held at z{z}");
        }
        assert_ne!(level_at(&mut layers, 9.60), GridTileKey::PART_LEVEL, "cells below crossover - 0.1");
        for z in [9.77, 9.67, 9.80] {
            assert_ne!(level_at(&mut layers, z), GridTileKey::PART_LEVEL, "cells held at z{z}");
        }
        assert_eq!(level_at(&mut layers, 9.85), GridTileKey::PART_LEVEL, "parts above crossover + 0.1");
    }

    #[test]
    fn race_archives_switch_to_parts_at_the_crossover() {
        let d = race_descriptor(); // 3″, levels 10, part_zoom 12, crossover 9.72
        assert_eq!(race_plan_kind(&d, 9.7), PlanKind::Cells { level: 0 });
        assert_eq!(race_plan_kind(&d, 9.72), PlanKind::Parts { data_zoom: 12 });
        assert_eq!(race_plan_kind(&d, 9.73), PlanKind::Parts { data_zoom: 12 });
        assert_eq!(race_plan_kind(&d, 14.0), PlanKind::Parts { data_zoom: 12 });
        assert_eq!(race_plan_kind(&d, 5.0), PlanKind::Cells { level: 4 });
    }

    #[test]
    fn a_source_without_a_race_key_always_plans_cells() {
        let src = small_source();
        assert_eq!(plan_kind(&*src, 14.0), PlanKind::Cells { level: 0 });
        assert_eq!(plan_kind(&*src, 3.0), PlanKind::Cells { level: level::level_for_zoom(&d30(), 3.0) });
    }

    #[test]
    fn an_all_zero_category_mask_draws_nothing_and_keeps_ppd() {
        let mut layers = synthetic_race_layers(1_000_000);
        let before = plan_frame(&mut layers, 12.0);
        assert!(!before.params.is_empty(), "the race layer draws part tiles at z12");
        assert!(before.ppd > 1.0, "a million people a tile is over the budget: ppd {}", before.ppd);
        assert!(before.params.iter().all(|p| p.category_mask == 0xFF));
        assert!(layers.set_category_mask("race", 0));
        assert!(!layers.set_category_mask("race", 0), "unchanged");
        let after = plan_frame(&mut layers, 12.0);
        assert_eq!(after.ppd, before.ppd);
        assert_eq!(after.params.len(), before.params.len());
        assert!(after.params.iter().all(|p| p.category_mask == 0));
        // Nothing else of the draw moves: the mask is one uniform word.
        for (a, b) in after.params.iter().zip(&before.params) {
            assert_eq!(DotParams { category_mask: 0, ..*b }, *a);
        }
    }

    #[test]
    fn the_race_palette_is_walkers() {
        assert_eq!(RACE_PALETTE[0], hex(0xE69F00));
        assert_eq!(RACE_PALETTE[1], hex(0x0072B2));
        assert_eq!(RACE_PALETTE[2], hex(0x009E73));
        assert_eq!(RACE_PALETTE[3], hex(0xCC79A7));
        assert_eq!(RACE_PALETTE[4], hex(0xD55E00));
        assert_eq!(RACE_PALETTE[5], hex(0x56B4E9));
        assert_eq!(RACE_PALETTE[6], hex(0x777777));
        assert_eq!(RACE_PALETTE[7], hex(0xF0E442));
        assert_eq!(DotRamp::Categorical.stops(), RACE_PALETTE.to_vec());
    }

    /// The categorical row puts group g's colour at entry g (what `vs_dot`
    /// reads under COLOR_CATEGORICAL), and opacity scales every alpha on
    /// the CPU (C16); opacity 1 leaves a density ramp bit for bit.
    #[test]
    fn the_categorical_row_is_the_palette_at_the_layers_opacity() {
        let row = DotRamp::Categorical.paints(0.8);
        assert_eq!(row.len(), crate::render::dots::DOT_RAMP_LEN);
        for (g, (from, to)) in row.iter().take(8).enumerate() {
            let want = Rgba { a: 0.8, ..RACE_PALETTE[g] };
            assert_eq!((*from, *to), (want, want), "group {g}");
        }
        assert_eq!(DotRamp::Viridis.paints(1.0), crate::render::dots::ramp_paints(&DotRamp::Viridis.stops()));
        assert!(DotRamp::Magma.paints(0.5).iter().all(|(f, _)| f.a == 0.5));
    }

    /// C12 and T6's carry: the race style draws a part tile as a triangle,
    /// categorically, with the grid-cell terms off (so `DotTileGpu::layout`'s
    /// C12 assert holds), and colour mode categorical survives
    /// `live_params`.
    #[test]
    fn the_race_style_draws_parts_as_categorical_triangles_with_cell_terms_off() {
        let mut layers = synthetic_race_layers(10_000);
        let f = plan_frame(&mut layers, 13.0);
        assert!(!f.params.is_empty());
        for p in &f.params {
            assert_eq!(p.shape, crate::render::dots::SHAPE_TRIANGLE);
            assert_eq!(p.color_mode, crate::render::dots::COLOR_CATEGORICAL);
            assert!(p.cell_terms_off(), "{p:?}");
            assert_eq!(p.category_mask, 0xFF);
            assert!((p.radius_px - (1.6 + 0.4)).abs() < 1e-6, "Walker's stops at z13: {}", p.radius_px);
        }
        assert_eq!(f.stats.level, GridTileKey::PART_LEVEL);
        // Below the crossover: categorical cells, the cell shape.
        let f = plan_frame(&mut layers, 9.5);
        assert!(!f.params.is_empty());
        assert!(f.params.iter().all(|p| p.shape == crate::render::dots::SHAPE_CELL && p.color_mode == crate::render::dots::COLOR_CATEGORICAL));
        assert_eq!(f.stats.level, 0);
    }

    /// A density layer is untouched by the new style fields: its colour
    /// mode, mask and radius are as before.
    #[test]
    fn a_density_layer_keeps_its_colour_mode_and_every_group() {
        let (layers, _) = settled(DotStyle::default(), 12.0);
        let params = layers.frame_params("pop", 12.0, 0.0, 0.0, None);
        assert!(!params.is_empty());
        assert!(params.iter().all(|p| p.color_mode == crate::render::dots::COLOR_DENSITY && p.category_mask == 0xFF));
        assert!(params.iter().all(|p| p.radius_px == DotStyle::default().radius_px(12.0)));
    }

    #[test]
    fn radius_stops_interpolate_linearly_and_hold_outside() {
        let s = DotStyle::race(Platform::Native);
        assert_eq!(s.radius_px(1.0), 0.55);
        assert_eq!(s.radius_px(3.0), 0.55);
        assert!((s.radius_px(5.0) - 0.675).abs() < 1e-6);
        assert_eq!(s.radius_px(12.0), 1.6);
        assert!((s.radius_px(13.0) - 2.0).abs() < 1e-6);
        assert_eq!(s.radius_px(20.0), 2.4);
        assert_eq!(s.radius_px(f64::NAN), 0.55);
        let linear = DotStyle::default();
        assert_eq!(linear.radius_px(8.0), DotStyle { radius_stops: Some(Vec::new()), ..linear.clone() }.radius_px(8.0));
    }

    /// C16: below its min zoom the layer wants and draws nothing, and its
    /// ppd waits; above it, it draws again.
    #[test]
    fn below_its_min_zoom_a_layer_plans_nothing() {
        let mut layers = synthetic_race_layers(10_000);
        let f = plan_frame(&mut layers, 12.5);
        assert!(!f.params.is_empty());
        assert!(layers.set_min_zoom("race", 13.0));
        let below = plan_frame(&mut layers, 12.5);
        assert!(below.params.is_empty());
        assert_eq!(layers.plan("race").unwrap(), &DotPlan::default());
        assert_eq!(below.ppd, f.ppd);
        assert!(layers.next_job().is_none());
        assert!(!plan_frame(&mut layers, 13.5).params.is_empty());
    }

    #[test]
    fn set_opacity_clamps_and_reports_a_change() {
        let mut layers = synthetic_race_layers(100);
        assert_eq!(layers.style("race").unwrap().opacity, RACE_OPACITY);
        assert!(layers.set_opacity("race", 2.0));
        assert_eq!(layers.style("race").unwrap().opacity, 1.0);
        assert!(!layers.set_opacity("race", 1.5), "still 1 after clamping");
        assert!(!layers.set_opacity("nope", 0.5));
    }

    /// C7: a plan with part keys never names a level outside the pyramid:
    /// every key is a part key at the part zoom, or a cell key whose level
    /// is below `levels` and whose tile sits at that level's data zoom --
    /// whatever is held, and for a cover of any zoom (a tilted view's far
    /// field included).
    #[test]
    fn a_plan_with_part_keys_never_produces_an_out_of_range_level() {
        let race = race_descriptor();
        let d = &race.grid;
        let kind = PlanKind::Parts { data_zoom: race.part_zoom };
        let near: Vec<TileId> = (0..3).flat_map(|dx| (0..2).map(move |dy| TileId { z: 12, x: 1050 + dx, y: 1520 + dy })).collect();
        let covers: Vec<Vec<TileId>> = vec![
            near.clone(),
            near.iter().flat_map(|&t| children(t)).collect(),
            // Far field: z11, z10 (drawn by their z9 cells), z9 and z6 (cells).
            [near.clone(), vec![TileId { z: 11, x: 526, y: 758 }, TileId { z: 10, x: 263, y: 378 }, TileId { z: 9, x: 131, y: 188 }, TileId { z: 6, x: 16, y: 23 }]].concat(),
        ];
        let part_ready = |k: &GridTileKey| k.is_part() && k.tile.x.is_multiple_of(2);
        let parent_ready = |k: &GridTileKey| !k.is_part() && k.level == 0;
        let coarse_ready = |k: &GridTileKey| !k.is_part() && k.level == 3;
        let readies: [&dyn Fn(&GridTileKey) -> bool; 5] = [&|_| false, &|_| true, &part_ready, &parent_ready, &coarse_ready];
        for cover in &covers {
            for ready in readies {
                let plan = plan_dots_kind(d, kind, 2020, None, cover, ready);
                assert!(!plan.wanted.is_empty());
                for k in plan.wanted.iter().chain(plan.draw.iter().map(|x| &x.key)) {
                    if k.is_part() {
                        assert_eq!(k.tile.z, race.part_zoom, "{k:?}");
                    } else {
                        assert!(k.level < d.levels, "{k:?}");
                        assert_eq!(k.tile.z, grid::data_zoom(d, k.level), "{k:?}");
                    }
                }
                // Coarse first: every cell draw before every part draw.
                let first_part = plan.draw.iter().position(|x| x.key.is_part()).unwrap_or(plan.draw.len());
                assert!(plan.draw[first_part..].iter().all(|x| x.key.is_part()), "{:?}", plan.draw);
            }
        }
    }

    /// A missing part tile is stood in for by its level-0 cell tile, which
    /// boxes out the part tiles already drawn inside it: no hole, no part
    /// drawn twice; its parent is wanted first.
    #[test]
    fn a_missing_part_tile_is_stood_in_for_by_its_cell_tile() {
        let race = race_descriptor();
        let d = &race.grid;
        let cover = [TileId { z: 12, x: 1048, y: 1520 }, TileId { z: 12, x: 1049, y: 1520 }];
        let parent = GridTileKey { tile: TileId { z: 9, x: 131, y: 190 }, level: 0, epoch: 2020, epoch_b: None };
        let held = GridTileKey::part(cover[0], 2020);
        let plan = plan_dots_kind(d, PlanKind::Parts { data_zoom: 12 }, 2020, None, &cover, &|k| *k == parent || *k == held);
        assert_eq!(plan.wanted[0], parent, "the parent is wanted first");
        assert_eq!(plan.draw.len(), 2, "{:?}", plan.draw);
        assert_eq!(plan.draw[0].key, parent);
        assert!(plan.draw[0].standin);
        assert_ne!(plan.draw[0].exclude[0], DotParams::NONE_EXCLUDED, "the stand-in boxes out the drawn part tile");
        assert_eq!(plan.draw[1].key, held);
        // Nothing held: the whole cover is a hole, never a panic.
        let plan = plan_dots_kind(d, PlanKind::Parts { data_zoom: 12 }, 2020, None, &cover, &|_| false);
        assert!(plan.draw.is_empty());
    }

    /// Task 5 review F4(b): a part tile owns its parts by centroid, and a
    /// part reaches up to 1/6 of a tile past its edge, so the cover takes
    /// in the neighbours whose overhang reaches the view -- and only those.
    #[test]
    fn the_part_cover_takes_in_neighbours_the_overhang_reaches() {
        let n = f64::from(1u32 << 12);
        let t = TileId { z: 12, x: 1050, y: 1520 };
        let (x0, y0) = (f64::from(t.x) / n, f64::from(t.y) / n);
        // A view inside `t`, 0.1 tile from its west edge and far (0.4
        // tile) from the others.
        let view = ViewRegion::mercator_box([x0 + 0.1 / n, y0 + 0.4 / n], [x0 + 0.6 / n, y0 + 0.6 / n]);
        let grown = grow_part_cover(vec![t], 12, &view);
        assert_eq!(grown, vec![t, TileId { z: 12, x: 1049, y: 1520 }], "only the west neighbour reaches");
        // At the tile's middle nothing reaches.
        let view = ViewRegion::mercator_box([x0 + 0.3 / n, y0 + 0.3 / n], [x0 + 0.7 / n, y0 + 0.7 / n]);
        assert_eq!(grow_part_cover(vec![t], 12, &view), vec![t]);
        // The antimeridian wraps: the west neighbour of column 0 is 4095.
        let edge = TileId { z: 12, x: 0, y: 1520 };
        let view = ViewRegion::mercator_box([0.0, y0 + 0.4 / n], [0.05 / n, y0 + 0.6 / n]);
        assert_eq!(grow_part_cover(vec![edge], 12, &view), vec![edge, TileId { z: 12, x: 4095, y: 1520 }]);
        // Row 0 has no row above it: nothing is asked for off the world.
        let top = TileId { z: 12, x: 5, y: 0 };
        let view = ViewRegion::mercator_box([5.4 / n, 0.0], [5.6 / n, 0.05 / n]);
        assert_eq!(grow_part_cover(vec![top], 12, &view), vec![top]);
    }

    /// The counter counts a part tile's people by their parts' centroids:
    /// all of them for a tile inside the view, those whose centroid is in
    /// view for a tile the view cuts.
    #[test]
    fn people_in_view_count_parts_by_centroid() {
        let mut layers = synthetic_race_layers(64_000);
        let f = plan_frame(&mut layers, 12.0);
        // The 2x2 block is the view: its 4 tiles are whole, the ring's
        // parts all lie outside it.
        assert_eq!(f.stats.people_in_view, 4.0 * 64_000.0, "{:?}", f.stats);
        let n = f64::from(1u32 << 12);
        let (x0, y0) = (f64::from(RACE_ORIGIN.x) / n, f64::from(RACE_ORIGIN.y) / n);
        // Half a tile: the west 4 of 8 columns of parts.
        let view = ViewRegion::mercator_box([x0, y0], [x0 + 0.5 / n, y0 + 1.0 / n]);
        assert_eq!(in_view::part_pop_in_view(&race_parts(RACE_ORIGIN, 64_000), &view), 32_000.0);
        assert_eq!(in_view::part_coverage(RACE_ORIGIN, 0.0, &view), in_view::Coverage::Edge);
    }

    // ---- Task 7 fix round 1: the part cap (review F1) ----

    fn part_keys(plan: &DotPlan) -> HashSet<GridTileKey> {
        plan.wanted.iter().chain(plan.draw.iter().map(|x| &x.key)).copied().filter(GridTileKey::is_part).collect()
    }

    /// The review's pitched mixed-zoom cover: 150 z12, 150 z11 and 150 z10
    /// tiles. The far field draws cells, never parts: only the 150 z12
    /// tiles are part keys, within the cap, and the far field's regions
    /// are counted as fallback.
    #[test]
    fn a_pitched_mixed_cover_plans_at_most_the_part_cap() {
        let race = race_descriptor();
        let kind = PlanKind::Parts { data_zoom: 12 };
        let mut cover: Vec<TileId> = (0..150).map(|i| TileId { z: 12, x: 1000 + i % 15, y: 1500 + i / 15 }).collect();
        cover.extend((0..150).map(|i| TileId { z: 11, x: 520 + i % 15, y: 760 + i / 15 }));
        cover.extend((0..150).map(|i| TileId { z: 10, x: 270 + i % 15, y: 390 + i / 15 }));
        let readies: [&dyn Fn(&GridTileKey) -> bool; 2] = [&|_| false, &|_| true];
        for ready in readies {
            let plan = plan_dots_kind(&race.grid, kind, 2020, None, &cover, ready);
            let parts = part_keys(&plan);
            assert_eq!(parts.len(), 150);
            assert!(parts.len() <= MAX_PART_TILES_PER_FRAME);
            assert!(parts.iter().all(|k| k.tile.z == 12 && cover.contains(&k.tile)));
            assert_eq!(plan.part_fallback, 300);
            // Cells for the far field: their z9 tiles are wanted.
            assert!(plan.wanted.iter().any(|k| !k.is_part() && k.level == 0 && k.tile == ancestor(TileId { z: 10, x: 270, y: 390 }, 9)));
        }
        // All held: the far field is drawn by its blocks' cell tiles.
        let plan = plan_dots_kind(&race.grid, kind, 2020, None, &cover, &|_| true);
        assert!(plan.draw.iter().any(|x| !x.key.is_part() && x.key.level == 0 && x.key.tile == ancestor(TileId { z: 10, x: 270, y: 390 }, 9)));
    }

    /// An oversized flat cover (512 z12 tiles, `MAX_COVER_TILES`): whole
    /// cell blocks are parts, nearest (first in cover order) first, within
    /// the cap -- the ceiling with a notch promoted -- and the rest cells.
    #[test]
    fn an_oversized_flat_cover_is_capped_in_cover_order() {
        let race = race_descriptor();
        let cover: Vec<TileId> = (0..512).map(|i| TileId { z: 12, x: 1000 + i % 32, y: 1500 + i / 32 }).collect();
        let plan = plan_dots_kind(&race.grid, PlanKind::Parts { data_zoom: 12 }, 2020, None, &cover, &|_| false);
        let parts = part_keys(&plan);
        assert!(!parts.is_empty() && parts.len() <= PART_TILES_CEILING, "{}", parts.len());
        assert!(parts.contains(&GridTileKey::part(cover[0], 2020)), "the first block is parts");
        assert_eq!(plan.part_fallback as usize + parts.len(), 512);
        // Blocks are whole: a part tile's block holds no cell-drawn cover tile.
        let blocks: HashSet<TileId> = parts.iter().map(|k| ancestor(k.tile, 9)).collect();
        assert!(cover.iter().filter(|t| blocks.contains(&ancestor(**t, 9))).all(|&t| parts.contains(&GridTileKey::part(t, 2020))));
        assert!(plan.wanted.iter().any(|k| !k.is_part() && k.level == 0 && !blocks.contains(&k.tile)), "cell blocks are wanted");
    }

    /// A flat z12 window within the cap plans exactly its cover as parts.
    #[test]
    fn a_flat_z12_window_plans_exactly_its_cover() {
        let race = race_descriptor();
        let cover: Vec<TileId> = (0..120).map(|i| TileId { z: 12, x: 1000 + i % 12, y: 1500 + i / 12 }).collect();
        let plan = plan_dots_kind(&race.grid, PlanKind::Parts { data_zoom: 12 }, 2020, None, &cover, &|_| false);
        let want: HashSet<GridTileKey> = cover.iter().map(|&t| GridTileKey::part(t, 2020)).collect();
        assert_eq!(part_keys(&plan), want);
        assert_eq!(plan.part_fallback, 0);
        // Overzoomed (a z14 window): its z12 ancestors, exactly.
        let fine: Vec<TileId> = cover.iter().flat_map(|&t| children(t)).collect();
        assert_eq!(part_keys(&plan_dots_kind(&race.grid, PlanKind::Parts { data_zoom: 12 }, 2020, None, &fine, &|_| false)), want);
    }

    /// The frame ranks part tiles nearest the focus first: over the cap,
    /// the part tiles kept are the nearest, and the ring is never added
    /// past the cap.
    #[test]
    fn over_the_cap_the_nearest_tiles_are_parts() {
        let cover: Vec<TileId> = (0..400).map(|i| TileId { z: 12, x: 1000 + i % 20, y: 1500 + i / 20 }).collect();
        let world = f64::from(crate::render::WORLD_SIZE);
        let size = world / 4096.0;
        // The focus at the cover's NW corner.
        let focus = glam::DVec2::new(1000.0 * size, 1500.0 * size);
        let ranked = rank_part_cover(cover.clone(), 12, Some(focus));
        let d = |t: &TileId| crate::render::cover_plan::ground_distance_to_tile(*t, focus);
        assert!(ranked.windows(2).all(|w| d(&w[0]) <= d(&w[1])));
        let n = 4096.0;
        let view = ViewRegion::mercator_box([1000.0 / n, 1500.0 / n], [1020.0 / n, 1520.0 / n]);
        let ring = part_ring(&ranked, 12, &view);
        let plan = plan_parts(&race_descriptor().grid, 12, 2020, None, &ranked, &ring, &HashSet::new(), &|_| false);
        assert!(part_keys(&plan).len() <= PART_TILES_CEILING, "the ring never passes the ceiling");
        let far = GridTileKey::part(TileId { z: 12, x: 1019, y: 1519 }, 2020);
        let near = GridTileKey::part(TileId { z: 12, x: 1000, y: 1500 }, 2020);
        assert!(part_keys(&plan).contains(&near) && !part_keys(&plan).contains(&far));
    }

    /// Review F9: a ring neighbour inside a coarser cover tile (the far
    /// field, drawn by cells) is not added -- it would draw twice.
    #[test]
    fn the_ring_skips_neighbours_inside_the_far_field() {
        let t = TileId { z: 12, x: 1048, y: 1520 };
        let far = TileId { z: 9, x: 130, y: 190 }; // holds x 1040..1048
        assert!(is_descendant(TileId { z: 12, x: 1047, y: 1520 }, far));
        let n = 4096.0;
        let view = ViewRegion::mercator_box([1048.0 / n, 1520.4 / n], [1048.1 / n, 1520.6 / n]);
        assert_eq!(grow_part_cover(vec![t, far], 12, &view), vec![t, far]);
        assert_eq!(grow_part_cover(vec![t], 12, &view), vec![t, TileId { z: 12, x: 1047, y: 1520 }]);
    }

    /// Review F10: the frame applies the ring -- a view hugging a tile's
    /// west edge wants the west neighbour's part tile.
    #[test]
    fn the_frame_wants_the_ring_the_overhang_reaches() {
        let mut layers = synthetic_race_layers(1_000);
        let n = 4096.0;
        let (x, y) = (f64::from(RACE_ORIGIN.x), f64::from(RACE_ORIGIN.y));
        let view = ([x / n, (y + 0.4) / n], [(x + 0.3) / n, (y + 0.6) / n]);
        layers.frame_with_cover(12.0, |z| vec![ancestor(RACE_ORIGIN, z.min(12))], view, crate::timing::Instant::now());
        let west = GridTileKey::part(TileId { z: 12, x: RACE_ORIGIN.x - 1, y: RACE_ORIGIN.y }, 2020);
        assert!(layers.plan("race").unwrap().wanted.contains(&west));
    }

    /// Review F4: a part tile drawn with a cell style (the default: it
    /// relocates, shrinks and fades) still gets the cell terms off.
    #[test]
    fn a_part_tile_draws_with_cell_terms_off_whatever_the_style() {
        let race = race_descriptor();
        let d = race.grid.clone();
        let source = MemoryGridSource::for_race(race, move |k| if k.is_part() { race_parts(k.tile, 5_000) } else { race_cells(&d, k, 100) });
        let mut layers = DotLayers::new();
        layers.set_source("race", Arc::new(source), DotStyle { relocate: true, mask_k: 8, ..DotStyle::default() });
        plan_frame(&mut layers, 16.0);
        let grid = grid_over(RACE_ORIGIN, 16.0);
        let params = layers.frame_params("race", 16.0, 0.0, 0.0, Some(&grid));
        assert!(!params.is_empty());
        assert!(params.iter().all(|p| p.shape == crate::render::dots::SHAPE_TRIANGLE && p.cell_terms_off()), "{params:?}");
    }

    /// A consistent race source: every z12 tile holds one part of `p`
    /// people centred on a 3″ cell centre, and every level-0 cell tile
    /// holds those same people in that cell -- so parts and cells are
    /// the same people.
    fn consistent_race_layers(p: u32) -> DotLayers {
        let race = race_descriptor();
        let d = race.grid.clone();
        let arc = 3.0 / 3600.0;
        // The level-0 cell whose centre sits near the middle of z12 `t`.
        let cell_of = move |t: TileId| {
            let (lon, lat) = crate::geo::tile_xy_to_lon_lat(f64::from(t.x) + 0.5, f64::from(t.y) + 0.5, 12.0);
            (((90.0 - lat) / arc).floor() as u32, ((lon + 180.0) / arc).floor() as u32)
        };
        let counts = move |t: TileId| {
            let mut c = [0u32; 8];
            c[((t.x + t.y) % 8) as usize] = p;
            c
        };
        let source = MemoryGridSource::for_race(race, move |k| {
            if k.is_part() {
                let (row, col) = cell_of(k.tile);
                let (lat, lon) = grid::cell_centre(&d, 0, row, col);
                let (x, y) = crate::overlay::geom::lon_lat_to_world(lon, lat);
                let w = f64::from(crate::render::WORLD_SIZE);
                let (cx, cy) = (x / w, y / w);
                let e = 1e-7;
                let part = crate::overlay::dotgrid::codec_cat::PartRecord {
                    a: [cx - e, cy - e],
                    b: [cx + 2.0 * e, cy - e],
                    c: [cx - e, cy + 2.0 * e],
                    counts: counts(k.tile),
                };
                return GridTile { parts: vec![part], total: f64::from(p), ..Default::default() };
            }
            if k.level != 0 {
                return GridTile::default(); // only level 0 is drawn here
            }
            let w = grid::cell_window(&d, 0, k.tile);
            let dz = 12 - k.tile.z;
            let mut cells: Vec<(u32, [u32; 8])> = Vec::new();
            for dy in 0..1u32 << dz {
                for dx in 0..1u32 << dz {
                    let t = TileId { z: 12, x: (k.tile.x << dz) + dx, y: (k.tile.y << dz) + dy };
                    let (row, col) = cell_of(t);
                    assert_eq!(grid::tile_of_cell(&d, 0, row, col), Some(k.tile));
                    cells.push((grid::local_index(&w, row, col), counts(t)));
                }
            }
            cells.sort_by_key(|c| c.0);
            let total = f64::from(p) * cells.len() as f64;
            GridTile {
                cells: cells.iter().map(|&(i, _)| (i, p as f32, p as f32)).collect(),
                cats: cells.iter().map(|c| c.1).collect(),
                total,
                ..Default::default()
            }
        });
        let mut layers = DotLayers::new();
        layers.set_source("race", Arc::new(source), DotStyle::race(Platform::Native));
        layers
    }

    /// Over the cap, the fallback cells carry the same people the part
    /// tiles would: a 20x20 z12 view (400 tiles) counts 400 p people in
    /// view whether 256 of them are parts and the rest cells, or all
    /// parts.
    #[test]
    fn the_fallback_cells_sum_to_the_same_people() {
        let p = 1_000u32;
        let n = 4096.0;
        let (x0, y0) = (1040u32, 1520u32);
        let view = ([f64::from(x0) / n, f64::from(y0) / n], [f64::from(x0 + 20) / n, f64::from(y0 + 20) / n]);
        let cover_at = |z: u8| -> Vec<TileId> {
            let s = 12 - z.min(12);
            (y0 >> s..=(y0 + 19) >> s).flat_map(|y| (x0 >> s..=(x0 + 19) >> s).map(move |x| TileId { z: z.min(12), x, y })).collect()
        };
        let mut layers = consistent_race_layers(p);
        for _ in 0..12 {
            layers.frame_with_cover(12.0, cover_at, view, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        let stats = layers.stats("race").unwrap();
        let plan = layers.plan("race").unwrap();
        let in_cover = part_keys(plan).iter().filter(|k| (x0..x0 + 20).contains(&k.tile.x) && (y0..y0 + 20).contains(&k.tile.y)).count();
        assert!(in_cover > 0 && stats.part_fallback_tiles > 0, "{in_cover} parts, {} fallback", stats.part_fallback_tiles);
        assert_eq!(stats.part_fallback_tiles as usize + in_cover, 400);
        assert!(plan.draw.iter().any(|x| !x.key.is_part()), "the fallback draws cells");
        assert_eq!(stats.people_in_view, 400.0 * f64::from(p), "{stats:?}");
        // The same view zoomed below the crossover: all cells, same people.
        let mut cells = consistent_race_layers(p);
        for _ in 0..6 {
            cells.frame_with_cover(9.5, cover_at, view, crate::timing::Instant::now());
            while let Some(job) = cells.next_job() {
                cells.land(job.run(|_| Ok(Vec::new())));
            }
        }
        assert_eq!(cells.stats("race").unwrap().people_in_view, 400.0 * f64::from(p));
    }

    // ---- Task 7 fix round 2: the seam between parts and cells ----

    #[test]
    fn a_strip_is_the_part_tiles_grown_by_the_overhang_merged_by_side() {
        let c = TileId { z: 9, x: 130, y: 190 }; // z12 x 1040..1048, y 1520..1528
        let g = PART_OVERHANG_TILES;
        // Two west neighbours: one west strip reaching g into c.
        let west = strip_groups(c, 12, &[TileId { z: 12, x: 1039, y: 1521 }, TileId { z: 12, x: 1039, y: 1523 }]);
        assert_eq!(west, vec![[1039.0 - g, 1521.0 - g, 1040.0 + g, 1524.0 + g]]);
        // A NW corner joins the west strip; a lone SE corner is its own box.
        let groups = strip_groups(c, 12, &[TileId { z: 12, x: 1039, y: 1522 }, TileId { z: 12, x: 1039, y: 1519 }, TileId { z: 12, x: 1048, y: 1528 }]);
        assert_eq!(groups.len(), 2, "{groups:?}");
        // Far or inside: nothing.
        assert!(strip_groups(c, 12, &[TileId { z: 12, x: 1037, y: 1522 }, TileId { z: 12, x: 1042, y: 1522 }]).is_empty());
        // Across the antimeridian.
        let edge = TileId { z: 9, x: 0, y: 190 };
        assert_eq!(strip_groups(edge, 12, &[TileId { z: 12, x: 4095, y: 1522 }]).len(), 1);
    }

    /// A cell block walled in by parts on three sides would need three
    /// boxes: it is promoted to parts (within the ceiling).
    #[test]
    fn a_notch_in_the_parts_is_promoted() {
        let race = race_descriptor();
        // Tiles of block (bx, by) in columns cx and rows cy.
        let block = |bx: u32, by: u32, cx: std::ops::Range<u32>, cy: std::ops::Range<u32>| -> Vec<TileId> {
            cy.flat_map(|y| cx.clone().map(move |x| TileId { z: 12, x: bx * 8 + x, y: by * 8 + y })).collect()
        };
        // 48 tiles each of the blocks W, N and E of (131, 191), next to it;
        // a lone full block far off (208 tiles in all); then the notch's
        // 64, which the cap (256) leaves out and the ceiling (288) holds.
        let mut cover: Vec<TileId> = Vec::new();
        cover.extend(block(130, 191, 2..8, 0..8));
        cover.extend(block(131, 190, 0..8, 2..8));
        cover.extend(block(132, 191, 0..6, 0..8));
        cover.extend(block(140, 191, 0..8, 0..8));
        cover.extend(block(131, 191, 0..8, 0..8));
        let sel = select_parts(&race.grid, 12, &cover, &[], &HashSet::new());
        let notch = TileId { z: 9, x: 131, y: 191 };
        assert!(sel.part_blocks.contains(&notch), "{sel:?}");
        assert!(sel.parts.len() <= PART_TILES_CEILING);
        for c in sel.cell_blocks.iter().chain(&sel.coarse) {
            assert!(strip_groups(*c, 12, &sel.parts).len() <= 2);
        }
    }

    /// Cap hysteresis (review r1): a block chosen last frame stays parts
    /// while it fits under the ceiling; a new one needs the cap.
    #[test]
    fn a_block_chosen_last_frame_stays_parts_up_to_the_ceiling() {
        let race = race_descriptor();
        // Four full blocks (256 tiles) in a row, then 16 tiles of a fifth.
        let mut cover: Vec<TileId> = Vec::new();
        for bx in 130..134u32 {
            cover.extend((0..64).map(|i| TileId { z: 12, x: bx * 8 + i % 8, y: 1520 + i / 8 }));
        }
        cover.extend((0..16).map(|i| TileId { z: 12, x: 134 * 8 + i % 8, y: 1520 + i / 8 }));
        let fifth = TileId { z: 9, x: 134, y: 190 };
        let fresh = select_parts(&race.grid, 12, &cover, &[], &HashSet::new());
        assert!(!fresh.part_blocks.contains(&fifth), "over the cap when new");
        let sticky: HashSet<TileId> = [fifth].into_iter().chain(fresh.part_blocks.iter().copied()).collect();
        let kept = select_parts(&race.grid, 12, &cover, &[], &sticky);
        assert!(kept.part_blocks.contains(&fifth), "kept under the ceiling");
        assert_eq!(kept.parts.len(), 272);
        // Past the ceiling even a sticky block goes.
        let mut more = cover.clone();
        more.extend((16..48).map(|i| TileId { z: 12, x: 134 * 8 + i % 8, y: 1520 + i / 8 }));
        assert!(!select_parts(&race.grid, 12, &more, &[], &sticky).part_blocks.contains(&fifth));
    }

    /// A source of realistic parts: in every z12 tile, triangles owned by
    /// centroid a hair inside each edge that reach 0.165 tile past it (the
    /// most `split_part` allows is 1/6), and interior ones; and level-0 cells on every 4th row and column.
    fn overhanging_race_layers() -> DotLayers {
        let race = race_descriptor();
        let d = race.grid.clone();
        let source = MemoryGridSource::for_race(race, move |k| {
            if k.is_part() {
                let n = 4096.0;
                let (x0, y0) = (f64::from(k.tile.x), f64::from(k.tile.y));
                let mut parts = Vec::new();
                let mut tri = |cx: f64, cy: f64, dir: (f64, f64)| {
                    // Vertex a 0.166 from the centroid along `dir`, b and c
                    // 0.083 back and 0.03 aside: the centroid is (cx, cy).
                    let (px, py) = (-dir.1, dir.0);
                    let at = |t: f64, s: f64| [(cx + dir.0 * t + px * s) / n, (cy + dir.1 * t + py * s) / n];
                    parts.push(crate::overlay::dotgrid::codec_cat::PartRecord {
                        a: at(0.166, 0.0),
                        b: at(-0.083, -0.03),
                        c: at(-0.083, 0.03),
                        counts: [1, 1, 0, 0, 0, 0, 0, 1],
                    });
                };
                for i in 0..10 {
                    let f = (f64::from(i) + 0.5) / 10.0;
                    // Centroids a hair inside each edge: the overhang is
                    // all but the full 1/6 tile (Task 5 review F4's bound).
                    tri(x0 + 0.001, y0 + f, (-1.0, 0.0));
                    tri(x0 + 0.999, y0 + f, (1.0, 0.0));
                    tri(x0 + f, y0 + 0.001, (0.0, -1.0));
                    tri(x0 + f, y0 + 0.999, (0.0, 1.0));
                    tri(x0 + f, y0 + 0.5, (1.0, 0.0));
                }
                let total = parts.iter().map(|p| p.counts.iter().map(|&c| f64::from(c)).sum::<f64>()).sum();
                return GridTile { parts, total, ..Default::default() };
            }
            if k.level != 0 {
                return GridTile::default();
            }
            let w = grid::cell_window(&d, 0, k.tile);
            let mut cells = Vec::new();
            let mut cats = Vec::new();
            for r in (0..w.rows).step_by(4) {
                for c in (0..w.cols).step_by(4) {
                    cells.push((r * w.cols + c, 2.0, 2.0));
                    cats.push([1, 0, 0, 0, 0, 0, 0, 1]);
                }
            }
            let total = cells.len() as f64 * 2.0;
            GridTile { cells, cats, total, ..Default::default() }
        });
        let mut layers = DotLayers::new();
        let style = DotStyle { budget_dots: 50_000_000, ..DotStyle::race(Platform::Native) };
        layers.set_source("race", Arc::new(source), style);
        layers
    }

    /// Review r1 HIGH, at the dot level: in a view over the part cap (6
    /// cell blocks of z12 tiles: some parts, some cells), no part dot --
    /// placed as `vs_dot` places it, overhang and all -- lands in a cell
    /// that a cell draw draws (its SW corner outside both exclusion boxes).
    /// Parts reaching 0.12 tile into a cell region would, without the
    /// overhang strips.
    #[test]
    fn no_part_dot_lands_in_a_drawn_cell() {
        use crate::overlay::dotgrid::{hash, parts as tri};
        let mut layers = overhanging_race_layers();
        let (x0, y0) = (1040u32, 1520u32);
        let n = 4096.0;
        let view = ([f64::from(x0) / n, f64::from(y0) / n], [f64::from(x0 + 24) / n, f64::from(y0 + 16) / n]);
        let cover_at = |z: u8| -> Vec<TileId> {
            let s = 12 - z.min(12);
            (y0 >> s..=(y0 + 15) >> s).flat_map(|y| (x0 >> s..=(x0 + 23) >> s).map(move |x| TileId { z: z.min(12), x, y })).collect()
        };
        for _ in 0..8 {
            layers.frame_with_cover(12.0, cover_at, view, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        let stats = layers.stats("race").unwrap();
        let plan = layers.plan("race").unwrap().clone();
        assert!(plan.draw.iter().all(|x| !x.standin), "settled");
        let cell_draws: Vec<&DotDraw> = plan.draw.iter().filter(|x| !x.key.is_part()).collect();
        assert!(!cell_draws.is_empty() && plan.draw.iter().any(|x| x.key.is_part()), "a mixed view: {} fallback", stats.part_fallback_tiles);
        let d = race_descriptor().grid;
        let l = blocks::OVERLAY_LYON_PER_UNIT_WORLD;
        // Every cell a cell draw draws, by (row, col).
        let mut drawn: HashSet<(u32, u32)> = HashSet::new();
        for x in &cell_draws {
            let tile = layers.tile("race", &x.key).unwrap();
            let w = grid::cell_window(&d, x.key.level, x.key.tile);
            let z = f64::from(1u32 << x.key.tile.z);
            let (ox, oy) = (f64::from(x.key.tile.x) / z, f64::from(x.key.tile.y) / z);
            let inside = |p: [f32; 2], b: [f32; 4]| p[0] >= b[0] && p[1] >= b[1] && p[0] <= b[2] && p[1] <= b[3];
            for &(idx, pop, _) in &tile.cells {
                let (row, col) = grid::row_col(&w, idx);
                let (sx, sy) = grid::cell_sw_mercator(&d, x.key.level, row, col);
                let o = [((sx - ox) * l) as f32, ((sy - oy) * l) as f32];
                if pop > 0.0 && !inside(o, x.exclude[0]) && !inside(o, x.exclude[1]) {
                    drawn.insert((row, col));
                }
            }
        }
        // Every part dot, placed as `vs_dot` places it at the view's ppd.
        let arc = 3.0 / 3600.0;
        let (mut dots, mut doubled) = (0u64, 0u64);
        for x in plan.draw.iter().filter(|x| x.key.is_part()) {
            let tile = layers.tile("race", &x.key).unwrap();
            let z = f64::from(1u32 << x.key.tile.z);
            let (ox, oy) = (f64::from(x.key.tile.x) / z, f64::from(x.key.tile.y) / z);
            for p in &tile.parts {
                let origin = [((p.a[0] - ox) * l) as f32, ((p.a[1] - oy) * l) as f32];
                let e1 = [((p.b[0] - p.a[0]) * l) as f32, ((p.b[1] - p.a[1]) * l) as f32];
                let e2 = [((p.c[0] - p.a[0]) * l) as f32, ((p.c[1] - p.a[1]) * l) as f32];
                let key = tri::part_key(origin, e1, e2);
                let pop: u32 = p.counts.iter().sum();
                for i in 0..pop {
                    if !hash::dot_alive(pop as f32, stats.ppd, i, key) {
                        continue;
                    }
                    let (_, u, v) = hash::dot_hashes(key, i);
                    let e1w = [p.b[0] - p.a[0], p.b[1] - p.a[1]];
                    let e2w = [p.c[0] - p.a[0], p.c[1] - p.a[1]];
                    let at = tri::sample_in_triangle(p.a, e1w, e2w, u, v);
                    let lon = at[0] * 360.0 - 180.0;
                    let lat = (std::f64::consts::PI * (1.0 - 2.0 * at[1])).sinh().atan().to_degrees();
                    let cell = (((90.0 - lat) / arc).floor() as u32, ((lon + 180.0) / arc).floor() as u32);
                    dots += 1;
                    doubled += u64::from(drawn.contains(&cell));
                }
            }
        }
        eprintln!("{dots} part dots, {} drawn cells, {doubled} doubled; {} fallback", drawn.len(), stats.part_fallback_tiles);
        assert!(dots > 10_000 && drawn.len() > 10_000);
        assert_eq!(doubled, 0, "part dots land in drawn cells");
    }
}
