//! A repeatable memory measurement for this renderer: the same map, at the
//! same camera, in a fresh process, N times, reported with its variance.
//!
//! # Why this exists
//!
//! Four separate memory attributions were made against this renderer and
//! all four were wrong -- compiled style size as the baseline, layer
//! fan-out as the multiplier, per-push work as the residual, and ~20 MB of
//! residency per tile. What they had in common was the instrument: an RSS
//! delta between two single runs. RSS on this platform varies between
//! identical runs by more than any of the effects being chased (the same
//! binary has been measured at 859 MB and at 442 MB), so a before/after
//! pair cannot separate an effect from the noise it is sitting in.
//!
//! **The noise floor is the deliverable.** Until the run-to-run standard
//! deviation at a fixed camera is known, no memory claim about this
//! renderer is worth making, including a claim this harness produces. Every
//! number it prints comes with a standard deviation for exactly that
//! reason, and a difference smaller than that deviation is not a finding.
//!
//! # What it controls for
//!
//! - **A fixed camera, never a sweep.** A camera sweep loads a
//!   nondeterministic tile set -- which tiles arrive depends on timing --
//!   so two sweeps are not two samples of one thing.
//! - **A settle condition, never a fixed sleep.** The sample is taken when
//!   `tiles_resident` has been unchanged for a while (see [`SettleDetector`]),
//!   so a slow network run and a fast one are sampled at the same point in
//!   the map's life rather than at the same wall-clock instant.
//! - **A fresh process per run.** In-process repeats share allocator state:
//!   the second iteration starts on pages the first one already made
//!   resident, which is precisely the effect being measured. Each run is a
//!   new process.
//! - **One tile source for every style.** The two styles this compares name
//!   different tilesets in their own documents (three against one), and
//!   comparing them as they ship would confound "which style" with "how
//!   much tile data". Both are pointed at the same composite source, so the
//!   style is the only variable.
//!
//! # Running it
//!
//! ```text
//! cargo run --release --example memory_probe
//! ```
//!
//! Configured entirely by environment, so a run is reproducible by pasting
//! the command that produced it:
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `GPUI_MAP_PROBE_RUNS` | 5 | fresh processes to launch |
//! | `GPUI_MAP_PROBE_STYLE` | `examples/mapbox-standard-dark-style.json` | style document under test |
//! | `GPUI_MAP_PROBE_TILESETS` | the composite from `examples/mapbox-streets-style.json` | comma-joined Mapbox tileset ids, held fixed across styles |
//! | `GPUI_MAP_PROBE_SPRITES` | `1` | fetch the style's sprite sheet; `0` to take that difference out of a style comparison |
//! | `GPUI_MAP_PROBE_SETTLE_SECS` | 4 | how long `tiles_resident` must hold still |
//! | `GPUI_MAP_PROBE_TIMEOUT_SECS` | 120 | give up on a run that never settles |
//! | `GPUI_MAP_LON` / `GPUI_MAP_LAT` / `GPUI_MAP_ZOOM` | Singapore, z14 | the fixed camera |
//! | `GPUI_MAP_PROBE_WIDTH` / `GPUI_MAP_PROBE_HEIGHT` | 1024 x 768 | the window, and therefore how many tiles cover it |
//! | `GPUI_MAP_PROBE_SOURCE` | `http` | `empty` for the zero-tile floor (see below) |
//! | `GPUI_MAP_PROBE_DROP` | none | a cache to empty at settle time, and measure the heap either side of |
//! | `GPUI_MAP_PROBE_STRIP` | none | `tags` or `geometry`: free that half of every decoded tile instead. Needs `--features probe` |
//! | `GPUI_MAP_PROBE_TOUR` | `0` | run an `n`-move deterministic pan/zoom tour before settling (see below) |
//! | `GPUI_MAP_PROBE_SWEEP` | `0` | `1` makes those `n` moves a pure ZOOM sweep at a fixed position, four integer crossings, no panning |
//! | `GPUI_MAP_PROBE_PREFETCH_DELTA` | unset (`MapView`'s own default, 4) | the coarse prefetch cover's zoom delta; `0` switches the cover off, which is the OFF arm of its A/B |
//!
//! # Varying the tile count on purpose
//!
//! The camera stays fixed and the WINDOW changes size. Tiles cover an area,
//! so the cover grows with the viewport while every other input -- zoom,
//! centre, style, tileset -- is held exactly where it was; that makes
//! `tiles_resident` the one thing that differs between two configurations,
//! which is what a per-tile figure needs. Changing the zoom instead would
//! also change how much data is inside each tile, and a slope taken across
//! that is not a per-tile slope.
//!
//! `GPUI_MAP_PROBE_SOURCE=empty` is the other end of the same line: a
//! [`gpui_maps::tile_source::EmptyTileSource`], so every fetch fails
//! immediately and no tile is ever resident. Everything else still happens
//! -- the style compiles, the window paints, wgpu builds its device and
//! pipelines, the sprite sheet uploads -- so what it measures is the floor
//! the tiles are stacked on top of. It settles under a different rule; see
//! [`FloorSettleDetector`] for why that is not a hole in the guard below.
//!
//! # Attributing the heap to a cache, by freeing it
//!
//! `GPUI_MAP_PROBE_DROP=<cache>` empties one of the renderer's per-tile
//! caches the instant the map settles, with the allocator's in-use figure
//! read either side of the call. The difference is what that cache was
//! holding **that nothing else holds a reference to**, which is the figure
//! worth having: several of these caches share their contents through
//! `Arc`, so an accessor that walked them adding up sizes would count the
//! shared bytes once per cache and report a total larger than the heap.
//! Freeing settles the sharing question for free -- whatever is still
//! referenced elsewhere does not go.
//!
//! The names are `plans`, `matches`, `decoded`, `symbols`, `gpu_meshes`,
//! `meshes`, `prefetched` and `labels`; see `MapView::TileCacheKind`. Every
//! run also reports each cache's ENTRY COUNT, drop or no drop, because
//! `tiles_resident` counts `gpu_meshes` alone and a prefetch burst puts
//! several times that many tiles into the caches that are not `gpu_meshes`.
//!
//! # Splitting one cache in half, the same way
//!
//! `GPUI_MAP_PROBE_STRIP=tags` (or `=geometry`) frees ONE HALF of every
//! decoded tile in place at settle time, brackets it with the same two
//! allocator reads, and so splits `decoded_cache`'s per-tile cost between
//! the geometry and the per-feature `HashMap<String, TagValue>`. In place
//! rather than via a stripped clone: a clone would allocate the surviving
//! half again before the original went, and the heap either side would
//! then measure a copy rather than a free.
//!
//! It needs `--features probe`, unlike every other knob here, because it
//! is the one that is not safe to leave reachable: a tile whose tags have
//! been freed still sits in `decoded_cache` looking like a tile, and a
//! rebake off it would filter every data-driven expression against an
//! empty property map and draw a wrong map silently. Without the feature
//! the variable is a hard error rather than a no-op -- a silent no-op
//! would report "0.0 MB freed" and read as a result.
//!
//! Every run with the feature on also carries a `census_*` block: features,
//! tag entries, and DISTINCT key strings against the total allocated. The
//! bytes alone cannot say whether interning would pay; that ratio can.
//!
//! ```text
//! cargo run --release --features probe --example memory_probe
//! ```
//!
//! `MAPBOX_ACCESS_TOKEN` is required, as for every other example here.
//!
//! # The ceiling, as against the resting figure
//!
//! Every figure this project holds was taken at a FIXED camera, and a
//! fixed camera measures a map at rest -- not the map anyone uses.
//! `GPUI_MAP_PROBE_TOUR=<n>` pans and zooms the camera through `n` moves
//! before the sample is taken, so a run reports three heap figures rather
//! than one: the steady state (from a run with no tour), the PEAK reached
//! anywhere during the tour, and the SETTLED figure after it.
//!
//! The tour is a closed circuit -- see [`tour_moves`] -- and lands exactly
//! on the camera it started from. That is what makes the settled figure
//! comparable with a no-tour run's: the two are the same camera with the
//! same cover, so the ratio between them is what the tour caused to be
//! RETAINED rather than merely visited. A ratio near 1 says the caches
//! gave back what they took; a ratio well above it is a retention the
//! fixed-camera figures could never have shown.
//!
//! The figure is reported as `tour_heap_ratio`, PAIRED: the denominator
//! is `pre_tour_heap_bytes`, the fixed-camera heap read in the same
//! process moments before the first move. A ratio of two means taken from
//! two different processes would carry both processes' variance; this
//! one carries neither, and it exposes a run that toured nothing all by
//! itself, because such a run reports 1.00.
//!
//! # Why a move waits for the cover, not for a still counter
//!
//! **`tiles_resident` holding still cannot end a move.** The count stops
//! moving on a map that has finished arriving AND on a map that will
//! never arrive, and those are the same observation. Two things go wrong
//! if a tour ends its moves on it:
//!
//! - **macOS stops delivering frames to an occluded window.** A map that
//!   stops painting stops requesting tiles, so the count freezes at
//!   whatever it had reached -- non-zero and unchanged, which is
//!   precisely the definition of settled. Every move then "settles"
//!   instantly and the run reports a confident heap for a camera nobody
//!   was looking at. `cx.activate(true)` at startup does NOT prevent
//!   this: it was in place for every run measured here, and **6 of 28
//!   tour runs still came back frozen at the fixed camera's own 20
//!   tiles**, 4 of them with `tour_gpu_buffers_created == 0`.
//! - **A move can end with fetches still outstanding**, so how much of
//!   the circuit a run loads depends on network timing. That puts the
//!   non-determinism INSIDE the measured quantity: of those same 28 runs
//!   only 11 retained the same tile set, and the rest landed anywhere
//!   between the two ends.
//!
//! So a move ends when the map has FINISHED ARRIVING -- every tile of
//! `MapView::cover_progress`'s cover resident with nothing in flight,
//! held for a full settle window (the prefetch a zoom breath triggers
//! lands after the cover does, and sampling between the two would count
//! it in some runs and not others). A move that never gets there
//! abandons the tour, and `tour_unsettled_moves`,
//! `tour_gpu_buffers_created` and `tour_failed_tiles` make the parent
//! EXCLUDE the run rather than average it in. A tour run is slow by
//! construction; the run timeout and the watcher's grace period both
//! carry the allowance.
//!
//! # What a `--features probe` run adds
//!
//! Counters that exist for claims this instrument could not otherwise
//! check, all from `render::alloc`:
//!
//! - `retention_bound_min` / `retention_bound_max` -- the smallest and
//!   largest bound `MapView::apply_retention_capacity` derived over the
//!   whole run. Equal-and-nonzero means the bound is STABLE; a spread
//!   means it is moving, and a bound that moves inside a zoom level
//!   evicts on a wobble rather than on memory pressure. The first
//!   viewport-derived bound this project shipped did exactly that,
//!   falling ~2.4x across each level, and it took arithmetic on the
//!   renderer's geometry to notice. Now it is one field. **On this
//!   branch both currently read 0** -- `apply_retention_capacity` does
//!   not exist (a prior retention design was reverted), so nothing calls
//!   `note_retention_bound`, and equal-at-zero here means "no producer",
//!   not "a stable bound of zero". Wiring a real producer is later plan
//!   work.
//! - `visible_tiles_without_stand_in_total` -- (visible tile, frame)
//!   pairs `paint` drew with no mesh of their own AND no stand-in: no
//!   covering set of cached descendants, no cached ancestor. Retention is
//!   supposed to change how long a tile is kept and never what is drawn,
//!   and the stand-in path is where that claim is at risk, because an
//!   ancestor held for a not-yet-loaded tile is never in the visible set
//!   and is therefore the LRU-coldest thing in `gpu_meshes`. **This is
//!   cumulative for the WHOLE PROCESS** -- window open, the pre-tour
//!   settle (cold-cache startup, where every visible tile takes the
//!   counted branch every frame until tiles land), and then the tour, if
//!   any. It is holes/process, not holes/sweep, and the pre-tour share is
//!   not knowable from this field alone. On a tour run, prefer
//!   `tour_visible_tiles_without_stand_in` (below), which is a delta
//!   isolating the tour's own contribution; use this cumulative field
//!   only when comparing against a figure that was itself recorded
//!   cumulatively (as every `task2-wip` figure was).
//! - `tour_visible_tiles_without_stand_in` -- holes counted strictly
//!   AFTER the pre-tour settle, i.e. during the tour itself. A delta
//!   against the cumulative counter above, the same way
//!   `tour_gpu_buffers_created` is a delta rather than the process
//!   total. `None`/absent on a fixed-camera (no-tour) run. This is the
//!   figure a later task's "did the sweep get better" comparison should
//!   use, since a startup-phase change (e.g. a wider prefetch cover)
//!   would otherwise be misread as a sweep improvement or regression.
//! - `uncovered_ancestors_total` -- present in every sample and **reads 0
//!   on this branch**. It is not wired to anything: see
//!   `render::alloc::UNCOVERED_ANCESTORS`'s doc for why it must not be
//!   wired to the same call site as `visible_tiles_without_stand_in_total`
//!   with a fixed count of 1 (that would make it a same-count alias
//!   measuring the hole count under a different name, not the broader
//!   ancestor-demand figure `task2-wip` recorded 151-207/sweep under this
//!   same field name). A real producer is later plan work.
//! - `tile_fetches_total` / `tile_fetch_bytes_total` -- the BANDWIDTH
//!   pair. Requests issued (counted at issue, so a 404 or a timeout still
//!   counts -- it still cost a round trip) and bytes returned by the ones
//!   that succeeded (pre-decode, as the `TileSource` handed them back, so
//!   no headers and no TLS overhead). Both are cumulative for the whole
//!   process and, like the hole counter, mostly startup: the cold settle
//!   fetches an entire first cover.
//! - `tour_tile_fetches` / `tour_tile_fetch_bytes` -- the same pair as a
//!   delta across the tour, and the figures a change that trades
//!   bandwidth for holes must be judged on. Fetches and holes have to be
//!   read TOGETHER: fewer holes bought by more requests is a trade, not a
//!   win, and the trade cannot be assessed from either number alone.
//! - `tour_frame_ms_max` / `tour_frame_ms_p99` / `tour_frame_ms_mean` /
//!   `tour_frames`, and the same four under `pre_tour_`. Wall time inside
//!   `MapView::paint`, timed by a guard that records on drop so a frame
//!   taken by one of `paint`'s early returns still counts. **Read the MAX
//!   first.** A frame budget is a ceiling and a mean cannot violate one:
//!   a rebake landing on the four frames of a sweep that cross an integer
//!   zoom is four frames in ~11,000, invisible in a mean and invisible in
//!   `tour_seconds`. These are NOT deltas -- a maximum cannot be
//!   differenced -- so the counters are RESET at the tour boundary and
//!   the pre-tour phase is reported separately.
//! - `tour_ancestor_stand_ins`, `tour_ancestor_stand_in_zoom_distance_mean`
//!   / `_max` / `_sum`, `tour_descendant_stand_ins`. The successor to
//!   `visible_tiles_without_stand_in_total`, which a coarse prefetch cover
//!   drives to zero and holds there: once a coarse cover is resident the
//!   ancestor search always finds SOMETHING, so "was there a stand-in"
//!   stops being a question. "How far magnified was it" is the question
//!   that remains, and the mean distance is it -- a parent one zoom up is
//!   nearly the right picture, an ancestor four zooms up is a smear.
//!   Lower is better; 0 is unreachable, since an ancestor is at least one
//!   level up. `descendant_stand_ins` counts the good branch (finer tiles,
//!   no magnification at all) so the distances are read against how often
//!   it was taken instead.
//!
//! Note that `--features probe` also turns on the decoded-tile census,
//! which allocates while it counts, so heap figures from a probe-feature
//! run are not comparable with the shipped binary's. Use those runs for
//! the counters.
//!
//! Every run also reports each per-tile cache's LENGTH against its
//! CAPACITY (`occupancy_len_*` / `occupancy_cap_*`, from
//! `MapView::cache_occupancy`), and the viewport the CAMERA actually had
//! (`camera_viewport_*_px`, beside the `viewport_*_px` the harness asked
//! for -- a window that did not open at the requested size used to report
//! the request back). At a fixed camera every one of them holds
//! the visible set and nothing more; whether that survives a tour is the
//! question the occupancy table answers.
//!
//! # A run that reports zero tiles is not a measurement
//!
//! If the Mac's screen is locked the window never paints, no bake is ever
//! spawned, and no tile is ever fetched. The map is then genuinely using
//! almost no memory and the harness will say so, which looks like a result
//! and is not one. Runs that never became resident are reported as
//! `UNSETTLED` and excluded from the statistics rather than quietly averaged
//! in.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// Blocks on `sprite::fetch_sprite_atlas`, which exists only with the
/// `sprite_http_fetch` feature (a default one). A `--no-default-features`
/// build gets an error here and draws without icons.
fn fetch_sprites_blocking(
    handle: &tokio::runtime::Handle,
    url: &str,
) -> anyhow::Result<gpui_maps::sprite::SpriteAtlas> {
    #[cfg(feature = "sprite_http_fetch")]
    return handle.block_on(gpui_maps::sprite::fetch_sprite_atlas(url, &reqwest::Client::new()));
    #[cfg(not(feature = "sprite_http_fetch"))]
    {
        let _ = (handle, url);
        Err(anyhow::anyhow!("built without the sprite_http_fetch feature"))
    }
}

/// `MAPBOX_ACCESS_TOKEN`, gated: `None` unless `GPUI_MAP_ALLOW_MAPBOX=1`.
/// Mapbox is off by default (see `kit/src/kit.rs::MAPBOX_ALLOW_VAR`,
/// the same switch). Never printed.
fn gated_mapbox_token() -> Option<String> {
    if std::env::var("GPUI_MAP_ALLOW_MAPBOX").ok().as_deref().map(str::trim) != Some("1") {
        return None;
    }
    std::env::var("MAPBOX_ACCESS_TOKEN").ok().filter(|t| !t.trim().is_empty())
}

fn main() {
    // One binary, two roles. The child is the thing being measured, so it
    // must be a process of its own -- see the module doc on allocator
    // state. `current_exe` rather than re-invoking cargo: the parent is
    // already the exact binary the child should be.
    if std::env::var_os(CHILD_MARKER).is_some() {
        child::run();
    } else {
        parent::run();
    }
}

/// Set on the child's environment by the parent, and on nothing else.
const CHILD_MARKER: &str = "GPUI_MAP_PROBE_CHILD";

/// The one line of the child's stdout the parent reads. Everything else the
/// child prints -- style diagnostics, gpui's own chatter -- is ignored, so
/// this prefix is the whole protocol between them.
const STATS_PREFIX: &str = "PROBE_STATS ";

// ---------------------------------------------------------------------------
// The settle detector
// ---------------------------------------------------------------------------

/// What [`SettleDetector`] makes of the samples it has seen so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settle {
    /// No tile has ever become resident. Not settled, however long it
    /// stays this way: a map with nothing on it is not a map that has
    /// finished loading, and treating it as settled is how a locked screen
    /// produces a confident measurement of nothing.
    Empty,
    /// Tiles are resident but the count is still moving, or has not held
    /// still for long enough yet.
    Loading,
    /// `tiles_resident` has been non-zero and unchanged for the whole
    /// window. Sample now.
    Settled,
}

/// Decides when a map has stopped loading, from nothing but the
/// `tiles_resident` counter and a clock.
///
/// A fixed sleep cannot do this job. Tiles arrive over the network, so the
/// time from launch to a full screen varies with the weather; sleeping long
/// enough for the slowest run means sampling every fast run well after it
/// settled, and sleeping for the average means sampling some runs mid-load.
/// Either way the thing being averaged is not the same thing each time.
///
/// The rule is deliberately narrow: **non-zero and unchanged for the whole
/// window**. Both halves earn their place --
///
/// - *non-zero*, because zero is unchanged forever and would otherwise
///   settle instantly (see [`Settle::Empty`]);
/// - *for the whole window*, because tiles land in bursts, and a gap
///   between two bursts is not the end of loading. Any change restarts the
///   clock.
struct SettleDetector {
    /// How long the count must hold still.
    window: Duration,
    /// The last count seen, or `None` before the first sample.
    last: Option<u64>,
    /// When the count last changed -- the start of the current window.
    unchanged_since: Instant,
}

impl SettleDetector {
    fn new(window: Duration, now: Instant) -> Self {
        Self { window, last: None, unchanged_since: now }
    }

    /// Feeds one sample in and reports where the run stands.
    fn observe(&mut self, now: Instant, tiles_resident: u64) -> Settle {
        if self.last != Some(tiles_resident) {
            self.last = Some(tiles_resident);
            self.unchanged_since = now;
        }
        if tiles_resident == 0 {
            return Settle::Empty;
        }
        if now.duration_since(self.unchanged_since) >= self.window {
            Settle::Settled
        } else {
            Settle::Loading
        }
    }
}

/// How far the heap may drift inside the settle window and still count as
/// still, for [`FloorSettleDetector`]. One MiB against a floor of several
/// hundred is well under a tenth of a percent -- comfortably above the
/// jitter of a process that is merely idling, and far below anything that
/// would matter to the figure being taken.
const FLOOR_HEAP_TOLERANCE_BYTES: u64 = 1 << 20;

/// Decides when a map that will never hold a tile has finished starting up.
///
/// [`SettleDetector`] cannot be reused for this and must not be softened
/// into it: its refusal to ever settle on zero tiles is the single thing
/// standing between a locked screen and a confident measurement of nothing.
/// So the zero-tile floor gets a detector of its own, which restates that
/// same guard in the only terms available to a map with no tiles in it:
///
/// - **Proof that the window painted.** `gpu_buffers_created_total` leaves
///   zero inside `MapView::ensure_gpu`, which runs in `paint` and nowhere
///   else. A window that never painted has no GPU buffers, so it never
///   settles here either -- exactly the outcome a locked screen deserves.
/// - **Proof that allocation stopped.** Startup allocates in bursts (style
///   compile, wgpu device and pipelines, the sprite upload), and with no
///   tile arrivals to watch, a heap that has stopped moving is the only
///   signal left that the bursts are over.
/// - **Proof that it really is the floor.** A resident tile means this run
///   is not measuring the floor at all, so the clock restarts and the run
///   runs out its timeout and is excluded, rather than reporting a
///   contaminated number as though it were the floor.
///
/// The stillness test compares against the heap at the START of the window,
/// not against the previous sample. A creep of a few hundred KB per sample
/// would pass a per-sample epsilon indefinitely while adding tens of MB
/// across the window; measured from the anchor, it cannot.
struct FloorSettleDetector {
    /// How long the heap must hold still.
    window: Duration,
    /// How far it may drift within the window and still count as still.
    tolerance: u64,
    /// The heap at the start of the current window, or `None` before the
    /// first sample that could open one.
    anchor: Option<u64>,
    /// When the current window opened.
    stable_since: Instant,
}

impl FloorSettleDetector {
    fn new(window: Duration, tolerance: u64, now: Instant) -> Self {
        Self { window, tolerance, anchor: None, stable_since: now }
    }

    /// Feeds one sample in and reports where the run stands.
    fn observe(
        &mut self,
        now: Instant,
        tiles_resident: u64,
        gpu_buffers_created_total: u64,
        heap_in_use_bytes: u64,
    ) -> Settle {
        if tiles_resident > 0 {
            // Not the floor. Say `Loading` rather than `Settled` however
            // long it holds: this configuration asked for zero tiles, and a
            // run that got some is a different measurement wearing the
            // floor's label.
            self.anchor = None;
            self.stable_since = now;
            return Settle::Loading;
        }
        if gpu_buffers_created_total == 0 {
            // Nothing has painted yet, so there is no floor to report --
            // only a process that has opened a window and may never draw
            // into it.
            self.anchor = None;
            self.stable_since = now;
            return Settle::Empty;
        }
        match self.anchor {
            Some(anchor) if heap_in_use_bytes.abs_diff(anchor) <= self.tolerance => {}
            _ => {
                self.anchor = Some(heap_in_use_bytes);
                self.stable_since = now;
            }
        }
        if now.duration_since(self.stable_since) >= self.window {
            Settle::Settled
        } else {
            Settle::Loading
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// Mean, spread and standard deviation of one counter across the runs.
///
/// The standard deviation is the SAMPLE deviation (Bessel's `n - 1`), not
/// the population one: these runs are a sample of the runs that could have
/// happened, and with the handful of runs anyone will actually sit through
/// the difference between the two divisors is not negligible.
#[derive(Debug, Clone, Copy)]
struct Summary {
    runs: usize,
    mean: f64,
    min: f64,
    max: f64,
    std_dev: f64,
}

impl Summary {
    /// `None` for an empty sample -- there is no mean of nothing, and
    /// returning a zero would put a number where the absence of one belongs.
    fn of(values: &[f64]) -> Option<Self> {
        let (first, _) = values.split_first()?;
        let runs = values.len();
        let mean = values.iter().sum::<f64>() / runs as f64;
        let mut min = *first;
        let mut max = *first;
        for value in values {
            min = min.min(*value);
            max = max.max(*value);
        }
        // A single run has a mean and no spread. Reporting 0.0 there would
        // read as "measured, and it does not vary", which is the exact
        // false confidence this harness exists to stop.
        let std_dev = if runs < 2 {
            f64::NAN
        } else {
            let variance =
                values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (runs - 1) as f64;
            variance.sqrt()
        };
        Some(Self { runs, mean, min, max, std_dev })
    }

    /// The deviation as a percentage of the mean -- the number that decides
    /// whether a difference between two configurations is worth discussing.
    fn relative_std_dev_percent(&self) -> f64 {
        if self.mean == 0.0 {
            f64::NAN
        } else {
            self.std_dev / self.mean * 100.0
        }
    }
}

/// Renders a byte count the way a person reads one, while the raw figure
/// stays in the JSON for anyone doing arithmetic on it.
fn megabytes(bytes: f64) -> String {
    format!("{:.1} MB", bytes / 1_048_576.0)
}

// ---------------------------------------------------------------------------
// The parent: launch N processes, aggregate what they report
// ---------------------------------------------------------------------------

mod parent {
    use super::{
        megabytes, Summary, CHILD_MARKER, STATS_PREFIX,
    };

    pub(super) fn run() {
        let runs: usize = env_or("GPUI_MAP_PROBE_RUNS", 5);
        let style = super::style_path();
        let label = std::path::Path::new(&style)
            .file_stem()
            .map_or_else(|| style.clone(), |stem| stem.to_string_lossy().into_owned());

        let exe = std::env::current_exe().expect("the probe must know its own path");
        println!("memory_probe: {runs} runs of {label}");
        println!("memory_probe: camera {:?}", super::camera_from_env());
        println!("memory_probe: viewport {:?}", super::viewport_from_env());
        println!(
            "memory_probe: tile source {}",
            if super::zero_tile_floor() { "empty (zero-tile floor)" } else { "http" }
        );
        println!("memory_probe: settle window {}s", super::settle_secs());
        println!("memory_probe: drop cache {}", super::drop_target_name(super::drop_target()));
        println!(
            "memory_probe: camera tour {}",
            match super::tour_len() {
                0 => "none (fixed camera)".to_owned(),
                moves if super::zoom_sweep_requested() =>
                    format!("{moves} moves, zoom sweep 14->15->13->14 at a fixed position"),
                moves => format!("{moves} moves, closed circuit back to the start camera"),
            }
        );
        println!();

        let mut samples: Vec<serde_json::Value> = Vec::new();
        for run in 1..=runs {
            print!("  run {run}/{runs} ... ");
            use std::io::Write;
            let _ = std::io::stdout().flush();

            let output = std::process::Command::new(&exe)
                .env(CHILD_MARKER, run.to_string())
                .stdin(std::process::Stdio::null())
                .output()
                .expect("the probe must be able to launch itself");

            // `output()` drains stdout and stderr concurrently. Reading them
            // in sequence would deadlock the moment the child's ~600 style
            // skip notices filled the stderr pipe.
            let stdout = String::from_utf8_lossy(&output.stdout);
            let Some(stats) = stdout
                .lines()
                .find_map(|line| line.strip_prefix(STATS_PREFIX))
                .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            else {
                println!("NO SAMPLE (exit {:?})", output.status.code());
                eprintln!(
                    "  last stderr: {}",
                    String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("(none)")
                );
                continue;
            };

            let settled = stats["settled"].as_bool().unwrap_or(false);
            let tiles = stats["tiles_resident"].as_u64().unwrap_or(0);
            let reported = stats["cache_report"].as_bool().unwrap_or(false);
            let invalid_tour = tour_invalidity(&stats);
            if settled && reported && invalid_tour.is_some() {
                // Excluded on the same principle as an UNSETTLED run, and
                // it has to be enforced HERE rather than left in the
                // sample: a tour run that measured nothing prints a
                // perfectly ordinary heap figure, and averaging it in
                // produces a mixture of two populations that describes
                // neither.
                println!(
                    "INVALID TOUR ({}), tiles {tiles} -- EXCLUDED",
                    invalid_tour.unwrap_or_default()
                );
            } else if settled && !reported {
                // The map settled and the main thread never took the
                // sample. Excluded for the same reason an unsettled run is:
                // it is a measurement of a process in a state nobody asked
                // to measure.
                println!("NO CACHE REPORT (main thread never sampled), tiles {tiles} -- EXCLUDED");
            } else if settled {
                let freed = stats["heap_freed_bytes"].as_f64().unwrap_or(0.0);
                println!(
                    "rss {}  heap {}  tiles {tiles}  freed {}",
                    megabytes(stats["rss_bytes"].as_f64().unwrap_or(0.0)),
                    megabytes(stats["heap_in_use_bytes"].as_f64().unwrap_or(0.0)),
                    megabytes(freed),
                );
                samples.push(stats);
            } else {
                // Excluded, loudly. A run that never got a tile resident is
                // a measurement of a map that never drew -- averaging it in
                // would drag every figure toward a number that describes
                // nothing.
                println!("UNSETTLED after {}s, tiles {tiles} -- EXCLUDED", super::timeout_secs());
            }
        }

        println!();
        report(&label, &samples);
    }

    fn report(label: &str, samples: &[serde_json::Value]) {
        if samples.is_empty() {
            println!("no settled runs -- nothing to report.");
            println!(
                "if every run said UNSETTLED with 0 tiles, check whether the screen is locked: \
                 a window that never paints never fetches a tile."
            );
            return;
        }

        // Every integer field of the sample, aggregated -- including any
        // counter added to `AllocationStats` after this was written, which
        // is the point of driving the table off the data rather than a list.
        let mut fields: Vec<String> = samples[0]
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .filter(|(_, value)| value.is_number())
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        fields.sort();

        println!("{label}: {} settled run(s)", samples.len());
        println!(
            "{:<32} {:>14} {:>14} {:>14} {:>14} {:>8}",
            "field", "mean", "min", "max", "std dev", "sd/mean"
        );
        for field in &fields {
            let values: Vec<f64> =
                samples.iter().filter_map(|sample| sample[field.as_str()].as_f64()).collect();
            let Some(summary) = Summary::of(&values) else { continue };
            println!(
                "{:<32} {:>14.1} {:>14.1} {:>14.1} {:>14.1} {:>7.2}%",
                field,
                summary.mean,
                summary.min,
                summary.max,
                summary.std_dev,
                summary.relative_std_dev_percent(),
            );
        }

        println!();
        println!("the ones that matter, in human units:");
        for field in [
            "rss_bytes",
            "heap_in_use_bytes",
            "heap_freed_bytes",
            "tour_peak_heap_bytes",
            "tour_peak_settled_heap_bytes",
            "pre_tour_heap_bytes",
        ] {
            let values: Vec<f64> =
                samples.iter().filter_map(|sample| sample[field].as_f64()).collect();
            let Some(summary) = Summary::of(&values) else { continue };
            println!(
                "  {field:<20} {} +/- {}  (min {}, max {}, n={})",
                megabytes(summary.mean),
                megabytes(summary.std_dev),
                megabytes(summary.min),
                megabytes(summary.max),
                summary.runs,
            );
        }

        println!();
        println!("raw samples (JSON, one per settled run):");
        for sample in samples {
            println!("{}", serde_json::to_string(sample).unwrap_or_default());
        }
    }

    /// Why this tour run is not a measurement of a tour, or `None` if it
    /// is one. Always `None` for a fixed-camera run, which has no tour to
    /// invalidate.
    ///
    /// Each rule names a way the map can look settled while having toured
    /// nothing, and every one of them was observed in practice before it
    /// was written down -- see the module doc on the occluded window.
    pub(super) fn tour_invalidity(stats: &serde_json::Value) -> Option<String> {
        let moves = stats["tour_moves"].as_u64().unwrap_or(0);
        if moves == 0 {
            return None;
        }
        let field = |name: &str| stats[name].as_u64().unwrap_or(0);
        if field("tour_gpu_buffers_created") == 0 {
            // `gpu_buffers_created_total` leaves zero inside `ensure_gpu`,
            // which runs in `paint` and nowhere else.
            return Some("the window never painted a new tile".to_owned());
        }
        let unsettled = field("tour_unsettled_moves");
        if unsettled > 0 {
            return Some(format!(
                "{unsettled} of {moves} moves never finished arriving"
            ));
        }
        let failed = field("tour_failed_tiles");
        if failed > 0 {
            return Some(format!(
                "{failed} tiles failed to fetch, so the retained set is the network's \
                 doing as much as the camera's"
            ));
        }
        let pre_tiles = field("pre_tour_tiles_resident");
        if let Some(per_tile) = field("pre_tour_heap_bytes").checked_div(pre_tiles) {
            if per_tile > PRE_TOUR_HEAP_PER_TILE_CEILING {
                return Some(format!(
                    "the fixed-camera baseline was {} MB over {pre_tiles} tiles \
                     ({} MB/tile), so the ratio's denominator is not a steady state",
                    field("pre_tour_heap_bytes") / (1 << 20),
                    per_tile / (1 << 20),
                ));
            }
        }
        // The heap-per-tile rule above catches bytes inflating faster than
        // tile count; it does nothing when the TILE COUNT itself is what
        // inflated (bytes/tile does not rise -- it can even fall, as here,
        // since more of the heap goes to tiles at a coarser prefetch tier).
        // A run in this project's own Task 0 baseline batch held 108 tiles
        // resident pre-tour against a same-camera cover of 20-ish -- 5.4x
        // surplus, per-tile heap actually BELOW its sibling runs' band,
        // every other validity field green -- and went on to report a
        // 0-hole tour against a 362-hole cumulative total, because every
        // one of those holes happened during the abnormally long pre-tour
        // settle rather than during the tour. `pre_tour_cover` is what the
        // SAME camera, at that SAME instant, actually needed
        // (`cover_progress().cover`), so this comparison needs no
        // hardcoded tile count and holds for any viewport/zoom the harness
        // is configured with.
        let cover = field("pre_tour_cover");
        if cover > 0 && pre_tiles > cover.saturating_mul(PRE_TOUR_SURPLUS_RATIO_CEILING) {
            return Some(format!(
                "the fixed-camera baseline held {pre_tiles} tiles resident against a \
                 same-camera cover of {cover} ({:.1}x), so the pre-tour settle was not a \
                 genuine steady state",
                pre_tiles as f64 / cover as f64,
            ));
        }
        None
    }

    /// Ceiling on `pre_tour_tiles_resident / pre_tour_cover`, above which
    /// the pre-tour settle held surplus tiles rather than a genuine steady
    /// state for the camera being measured.
    ///
    /// Calibrated the same way as `PRE_TOUR_HEAP_PER_TILE_CEILING`: every
    /// valid run this instrument has produced held `pre_tour_tiles_resident`
    /// within a tile or two of `pre_tour_cover` (ratio ~1.0-1.2 -- the
    /// pre-tour steady state IS the cover, with at most a sliver of
    /// prefetch). The one anomaly observed held 5.4x. 3x sits well clear of
    /// every normal run and well short of the anomaly, leaving headroom for
    /// legitimate prefetch this instrument has not yet exercised at scale.
    const PRE_TOUR_SURPLUS_RATIO_CEILING: u64 = 3;

    /// Ceiling on the fixed-camera heap PER RESIDENT TILE, above which the
    /// pre-tour baseline is not a steady state and the run is excluded.
    ///
    /// Calibrated, not guessed. Every valid run this instrument has ever
    /// produced sits at 4.4-4.8 MB/tile (92 MB over 20 tiles), which
    /// matches the `18.1 + 3.685 x entries` heap model from the other
    /// direction. One run reported 483 MB over the same 20 tiles -- 24
    /// MB/tile, five times normal -- with entirely normal tile counters
    /// and a normal 65 pre-tour GPU buffers, and no rule here caught it.
    /// 12 MiB/tile is 2.6x the observed normal and half the observed
    /// anomaly.
    ///
    /// **This rule exists because `tour_heap_ratio` is the wrong guard for
    /// this failure.** The anomaly is in the ratio's DENOMINATOR, and an
    /// inflated denominator drives the ratio DOWN -- toward passing the
    /// budget it is supposed to test. That run reported 1.34 where a normal
    /// one reports 4.0.
    ///
    /// The underlying cause is a candidate rather than a finding, and it is
    /// worth writing down: `SettleDetector::observe` watches only
    /// `tiles_resident` and never looks at the heap, while
    /// `FloorSettleDetector` in this same file does, and its doc comment
    /// says exactly why -- startup allocates in bursts and a still heap is
    /// the only signal they are over. So a tile-bearing run can settle with
    /// startup allocation still live, and `pre_tour_heap_bytes` is read
    /// right there. Adding heap-stillness to the tile-bearing settle path
    /// would fix it at the source, but it would also change what every
    /// figure already taken with this instrument means, so the exclusion
    /// rule comes first.
    const PRE_TOUR_HEAP_PER_TILE_CEILING: u64 = 12 << 20;

    fn env_or(name: &str, fallback: usize) -> usize {
        std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(fallback)
    }
}

// ---------------------------------------------------------------------------
// Shared configuration
// ---------------------------------------------------------------------------

/// The fixed camera. z14 over Singapore by default -- a zoom with a real
/// tile set behind it, unlike z12, and the zoom the contradictory earlier
/// figures were taken at.
fn camera_from_env() -> (f64, f64, f64) {
    fn or(name: &str, fallback: f64) -> f64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(fallback)
    }
    (or("GPUI_MAP_LON", 103.8198), or("GPUI_MAP_LAT", 1.3521), or("GPUI_MAP_ZOOM", 14.0))
}

fn style_path() -> String {
    std::env::var("GPUI_MAP_PROBE_STYLE")
        .unwrap_or_else(|_| "examples/mapbox-standard-dark-style.json".to_owned())
}

/// The window, in logical pixels -- and therefore, at a fixed camera, how
/// many tiles it takes to cover it. See the module doc on varying the tile
/// count.
fn viewport_from_env() -> (f64, f64) {
    fn or(name: &str, fallback: f64) -> f64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(fallback)
    }
    (or("GPUI_MAP_PROBE_WIDTH", 1024.0), or("GPUI_MAP_PROBE_HEIGHT", 768.0))
}

/// Whether this run measures a map with tiles or the floor underneath one.
fn zero_tile_floor() -> bool {
    std::env::var("GPUI_MAP_PROBE_SOURCE").as_deref() == Ok("empty")
}

/// The cache this run empties at settle time, if any. An unrecognised name
/// is a configuration error rather than a silent "no drop": a run that
/// quietly measured nothing would be averaged in as though it had measured
/// something.
fn drop_target() -> Option<gpui_maps::map_view::TileCacheKind> {
    use gpui_maps::map_view::TileCacheKind as K;
    let name = std::env::var("GPUI_MAP_PROBE_DROP").ok()?;
    let name = name.trim();
    if name.is_empty() || name == "none" {
        return None;
    }
    Some(match name {
        "plans" => K::Plans,
        "matches" => K::Matches,
        "decoded" => K::Decoded,
        "symbols" => K::Symbols,
        "gpu_meshes" => K::GpuMeshes,
        "meshes" => K::Meshes,
        "prefetched" => K::PrefetchedMeshes,
        "labels" => K::LabelShapes,
        other => panic!(
            "GPUI_MAP_PROBE_DROP={other} names no cache; expected one of plans, matches, \
             decoded, symbols, gpu_meshes, meshes, prefetched, labels"
        ),
    })
}

/// The name a `TileCacheKind` is selected by, for the sample it appears in.
fn drop_target_name(kind: Option<gpui_maps::map_view::TileCacheKind>) -> &'static str {
    use gpui_maps::map_view::TileCacheKind as K;
    match kind {
        None => "none",
        Some(K::Plans) => "plans",
        Some(K::Matches) => "matches",
        Some(K::Decoded) => "decoded",
        Some(K::Symbols) => "symbols",
        Some(K::GpuMeshes) => "gpu_meshes",
        Some(K::Meshes) => "meshes",
        Some(K::PrefetchedMeshes) => "prefetched",
        Some(K::LabelShapes) => "labels",
    }
}

/// Which half of every cached decoded tile this run frees at settle time,
/// if any -- `tags` or `geometry`. Names the same measurement as
/// `GPUI_MAP_PROBE_DROP`, one level further in: instead of asking what a
/// whole cache holds, it asks what one half of `decoded_cache`'s contents
/// holds, so the 34.5 MB a resident tile costs can be split.
///
/// Only exists with the `probe` cargo feature. Without it the variable is
/// a hard error rather than a no-op, because the failure mode of a silent
/// no-op here is a run that reports "0.0 MB freed" and reads as a result.
#[cfg(feature = "probe")]
fn strip_target() -> Option<gpui_maps::mvt::DecodedTilePart> {
    use gpui_maps::mvt::DecodedTilePart as P;
    let name = std::env::var("GPUI_MAP_PROBE_STRIP").ok()?;
    let name = name.trim();
    if name.is_empty() || name == "none" {
        return None;
    }
    Some(match name {
        "tags" => P::Tags,
        "geometry" => P::Geometry,
        other => panic!(
            "GPUI_MAP_PROBE_STRIP={other} names no half of a decoded tile; \
             expected one of tags, geometry"
        ),
    })
}

/// The name a `DecodedTilePart` is selected by, for the sample.
#[cfg(feature = "probe")]
fn strip_target_name(part: Option<gpui_maps::mvt::DecodedTilePart>) -> &'static str {
    use gpui_maps::mvt::DecodedTilePart as P;
    match part {
        None => "none",
        Some(P::Tags) => "tags",
        Some(P::Geometry) => "geometry",
    }
}

/// Refuses a `GPUI_MAP_PROBE_STRIP` that this binary cannot honour. See
/// [`strip_target`] for why this is a panic and not a shrug.
#[cfg(not(feature = "probe"))]
fn strip_target() -> Option<std::convert::Infallible> {
    match std::env::var("GPUI_MAP_PROBE_STRIP").ok().as_deref().map(str::trim) {
        None | Some("") | Some("none") => None,
        Some(other) => panic!(
            "GPUI_MAP_PROBE_STRIP={other} needs the `probe` cargo feature, which is off \
             by default: cargo run --release --features probe --example memory_probe"
        ),
    }
}

#[cfg(not(feature = "probe"))]
fn strip_target_name(_part: Option<std::convert::Infallible>) -> &'static str {
    "none"
}

fn settle_secs() -> u64 {
    std::env::var("GPUI_MAP_PROBE_SETTLE_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(4)
}

/// How long a run may take before it is given up on.
///
/// The default carries the tour's own allowance, because a tour run is
/// legitimately slow: it settles once per move, and a timeout tuned for a
/// map that loads once would shoot a working run in the head halfway
/// round. An explicit setting is honoured as given -- if someone bounds a
/// run by hand, the tour does not get to overrule them.
fn timeout_secs() -> u64 {
    std::env::var("GPUI_MAP_PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| 120 + tour_allowance_secs())
}

/// How many moves of the camera tour this run makes before it settles and
/// samples. Zero -- the default -- is the fixed camera every figure in this
/// project's record was taken at.
fn tour_len() -> usize {
    std::env::var("GPUI_MAP_PROBE_TOUR")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// The longest one tour move may take before the tour gives up on it and
/// moves on, counting it as unsettled.
///
/// A move that never settles is reported rather than waited out forever:
/// the count of them appears in the sample, so a run whose tour did not
/// actually complete cannot be read as one that did.
fn tour_move_timeout_secs() -> u64 {
    settle_secs() * 2 + 20
}

/// The wall-clock the tour is allowed on top of everything a fixed-camera
/// run needs.
fn tour_allowance_secs() -> u64 {
    tour_len() as u64 * tour_move_timeout_secs()
}

/// The coarse prefetch cover's zoom delta, or `None` to leave
/// `MapView`'s own default (4) alone.
///
/// `GPUI_MAP_PROBE_PREFETCH_DELTA=0` is the OFF arm of the coarse-cover
/// A/B and `=4` is the ON arm. Driving both arms from one binary through
/// the product's own documented knob is deliberate: a before/after built
/// from two commits differs in every incidental way two builds can, and
/// this comparison is about one number.
fn prefetch_zoom_delta_from_env() -> Option<u8> {
    std::env::var("GPUI_MAP_PROBE_PREFETCH_DELTA").ok()?.trim().parse().ok()
}

/// Whether `GPUI_MAP_PROBE_TOUR` drives the pan circuit or a pure zoom
/// sweep.
///
/// The circuit pans AND breathes, so a hole counted during it could have
/// come from either. `GPUI_MAP_PROBE_SWEEP=1` holds the camera still and
/// moves only the zoom, which isolates the integer crossing -- the place
/// the stand-in path is under the most pressure, because the bound is at
/// its smallest at high `frac(zoom)` and `next_zoom_prefetch_tiles` fires
/// there too.
fn zoom_sweep_requested() -> bool {
    matches!(std::env::var("GPUI_MAP_PROBE_SWEEP").as_deref(), Ok("1"))
}

/// A deterministic zoom sweep at a fixed ground position: 14 -> 15 -> 13
/// -> 14, so it crosses an integer zoom level four times and passes
/// through `frac = 0.7..1.0` on both sides of each crossing.
///
/// Closed, like the circuit, and for the same reason: it ends on the
/// camera it began at, so the settled sample is comparable with a no-tour
/// run's.
fn zoom_sweep_moves(count: usize) -> Vec<(f64, f64, f64)> {
    const LON: f64 = 103.8198;
    const LAT: f64 = 1.3521;
    const ZOOM: f64 = 14.0;
    let mut moves: Vec<(f64, f64, f64)> = (0..count)
        .map(|i| {
            let t = i as f64 / count as f64;
            (LON, LAT, ZOOM + (t * std::f64::consts::TAU).sin())
        })
        .collect();
    if let Some(last) = moves.last_mut() {
        *last = (LON, LAT, ZOOM);
    }
    moves
}

/// A deterministic pan/zoom tour: a closed rectangular circuit around
/// the start point with a zoom breath, returning exactly to the camera
/// it began at.
///
/// Closed and fixed on purpose. The settled sample after the tour is
/// compared against the steady-state sample at the same camera, so the
/// only difference between them is what the tour caused to be RETAINED.
/// A random walk would make the two incomparable.
fn tour_moves(count: usize) -> Vec<(f64, f64, f64)> {
    if zoom_sweep_requested() {
        return zoom_sweep_moves(count);
    }
    const LON: f64 = 103.8198;
    const LAT: f64 = 1.3521;
    const ZOOM: f64 = 14.0;
    // ~1.5 tiles of pan per side at z14, so each leg pulls in a fresh
    // cover rather than nudging within one.
    const SPAN: f64 = 0.02;
    let mut moves = Vec::with_capacity(count);
    for i in 0..count {
        let t = i as f64 / count as f64;
        let leg = (t * 4.0).floor();
        let along = t * 4.0 - leg;
        let (dx, dy) = match leg as u32 {
            0 => (along, 0.0),
            1 => (1.0, along),
            2 => (1.0 - along, 1.0),
            _ => (0.0, 1.0 - along),
        };
        // One zoom breath over the circuit: out on the way there, back
        // on the way home. Integer-crossing is the point -- it is what
        // makes `prefetched_meshes` fill.
        let zoom = ZOOM - (t * std::f64::consts::TAU).sin();
        moves.push((LON + dx * SPAN, LAT + dy * SPAN, zoom));
    }
    // Land exactly back on the start camera.
    if let Some(last) = moves.last_mut() {
        *last = (LON, LAT, ZOOM);
    }
    moves
}

// ---------------------------------------------------------------------------
// The child: one measured process
// ---------------------------------------------------------------------------

mod child {
    use super::{
        camera_from_env, drop_target, drop_target_name, settle_secs, strip_target,
        strip_target_name, style_path, timeout_secs, tour_len, tour_move_timeout_secs,
        tour_moves, viewport_from_env, zero_tile_floor, Arc, Duration, FloorSettleDetector,
        Instant, Settle, SettleDetector, FLOOR_HEAP_TOLERANCE_BYTES, STATS_PREFIX,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Raised by the settle watcher, read by the cache reporter on the main
    /// thread. The watcher decides WHEN to sample (it is the only thing
    /// that can, since a wedged main thread would never notice); the main
    /// thread is the only place that can reach the `MapView` to read or
    /// empty a cache, so the sample is taken there once the watcher says
    /// the moment has come.
    static SETTLED: AtomicBool = AtomicBool::new(false);

    /// How long the watcher waits for the main thread to take the sample
    /// before giving up and reporting one without any cache figures in it.
    /// A run that reaches this is a run whose main thread stopped serving
    /// tasks, and the parent excludes it rather than averaging in a
    /// measurement of a wedged process.
    ///
    /// The tour's own allowance is added, because on a tour run the main
    /// thread is *supposed* to take minutes between the first settle and
    /// the sample -- it is driving the camera round. Without this the
    /// backstop would fire on every tour and the run would be excluded for
    /// doing exactly what it was asked to do.
    fn cache_report_grace() -> Duration {
        Duration::from_secs(20 + super::tour_allowance_secs())
    }

    use gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};
    use gpui_maps::map_view::MapView;
    use gpui_maps::tile_source::http::HttpTileSource;
    use gpui_maps::tile_source::TileSource;

    pub(super) fn run() {
        let style_path = style_path();
        let style_bytes = std::fs::read(&style_path)
            .unwrap_or_else(|e| panic!("run from the repo root so {style_path} resolves: {e}"));
        let style = gpui_maps::style_spec::parse_and_compile(&style_bytes)
            .expect("the style under test must compile");
        let compiled_layers = style.layers.len();

        let token = crate::gated_mapbox_token()
            .expect("set GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN to run this example (a Mapbox public 'pk.' token)");

        // Held FIXED across styles on purpose -- see the module doc. The
        // default is the three-tileset composite `mapbox_streets_demo` has
        // already been verified against live.
        let tileset_ids = std::env::var("GPUI_MAP_PROBE_TILESETS").unwrap_or_else(|_| {
            "mapbox.mapbox-streets-v8,mapbox.mapbox-terrain-v2,mapbox.mapbox-bathymetry-v2"
                .to_owned()
        });

        let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        let handle = runtime.handle().clone();

        let sprites = if std::env::var("GPUI_MAP_PROBE_SPRITES").as_deref() == Ok("0") {
            None
        } else {
            style
                .sprite
                .as_deref()
                .and_then(|reference| {
                    gpui_maps::sprite::resolve_sprite_url(reference, Some(&token)).ok()
                })
                .and_then(|base| crate::fetch_sprites_blocking(&handle, &base).ok())
                .map(Arc::new)
        };

        let (width, height) = viewport_from_env();
        let floor = zero_tile_floor();

        // The tour's circuit is anchored on fixed constants, so a run that
        // starts somewhere else would settle at one camera, tour around
        // Singapore, and land back at Singapore -- and its settled figure
        // would then be compared against a steady state taken somewhere
        // else entirely. Refuse rather than quietly measure two cameras.
        if tour_len() > 0 {
            let start = tour_moves(1)[0];
            assert!(
                camera_from_env() == start,
                "GPUI_MAP_PROBE_TOUR only measures against the tour's own start camera \
                 {start:?}; this run's camera is {:?}, so the settled-after-tour figure \
                 would not be comparable with any steady state",
                camera_from_env()
            );
        }

        gpui_platform::application().run(move |cx: &mut App| {
            let source: Arc<dyn TileSource> = if floor {
                // Fetches fail immediately, so no tile is ever decoded,
                // meshed or made resident -- and everything that is not a
                // tile still happens. See the module doc.
                Arc::new(gpui_maps::tile_source::EmptyTileSource)
            } else {
                let tile_url = format!(
                    "https://api.mapbox.com/v4/{tileset_ids}/{{z}}/{{x}}/{{y}}.mvt\
                     ?access_token={token}"
                );
                Arc::new(HttpTileSource::new(tile_url, 16, "(c) Mapbox (c) OpenStreetMap"))
            };

            // Started before the window opens so the timeout covers the
            // whole life of the run, including a first frame that never
            // arrives.
            watch_for_settle(compiled_layers, width, height);

            // TOP-LEFT, not centred, and this is a correctness matter
            // rather than a courtesy. macOS stops delivering frames to a
            // window that is FULLY covered by opaque windows, and a
            // 1024x768 window centred on a 1728x1117 display sits entirely
            // inside any maximised browser or editor -- so the moment the
            // probe loses focus it stops painting, stops fetching, and
            // `tour_gpu_buffers_created` sticks at 0. Measured: with the
            // display carrying three ~1677x1084 windows, EVERY tour run
            // came back `INVALID TOUR (the window never painted a new
            // tile)`; moved to the top-left corner, where a strip of it
            // stays uncovered, they complete. `cx.activate(true)` below
            // does not prevent this and never did (see the module doc).
            //
            // This is the same reasoning, and the same corner, as
            // `kit/src/window.rs` -- inlined rather than shared
            // because that helper is for demos a human is looking at, and
            // this one must not move if the demos' default ever does.
            // Position enters no measured quantity: the viewport size, the
            // camera and therefore the tile cover are unchanged by it.
            let bounds = Bounds {
                origin: gpui::point(px(0.0), px(38.0)),
                size: size(px(width as f32), px(height as f32)),
            };
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |_window, cx| {
                    let view = cx.new(|_cx| {
                        let (lon, lat, zoom) = camera_from_env();
                        let mut view = MapView::new(
                            lon, lat, zoom, width, height, style, source, handle,
                        );
                        if let Some(sprites) = sprites {
                            view.set_sprites(sprites);
                        }
                        // The A/B switch for the coarse prefetch cover.
                        // Set explicitly by both arms of a comparison so
                        // the two runs differ ONLY in this knob and not in
                        // which binary they came from -- a before/after
                        // built from two commits also differs in every
                        // incidental way two builds can, and one of those
                        // (a build without `--features probe`, whose
                        // counters are simply absent) has already cost this
                        // project a discarded batch.
                        if let Some(delta) = super::prefetch_zoom_delta_from_env() {
                            view.set_prefetch_zoom_delta(delta);
                        }
                        view
                    });
                    report_caches_once_settled(view.clone(), compiled_layers, width, height, cx);
                    view
                },
            )
            .expect("failed to open window");

            // The window must be FRONTMOST, not merely open, or this
            // measures nothing. macOS stops delivering frames to an
            // occluded window, and the child is launched from a terminal
            // that keeps focus, so without this the map paints its first
            // frame, requests its tiles, and then never paints again: the
            // decoded tiles pile up while `mesh_installs_total` and
            // `tiles_resident` both stay at zero. That is the same
            // false-negative shape as the locked screen this harness
            // already guards against -- a process using memory in a way
            // no real map ever would -- and the settle detector's
            // non-zero guard correctly refuses to report it, so the
            // symptom is an UNSETTLED run with 0 tiles and a 790 MB heap.
            cx.activate(true);
        });

        std::mem::forget(runtime);
    }

    /// Takes the sample, on the main thread, the moment the settle watcher
    /// says the map is ready -- because the main thread is the only place a
    /// `MapView`'s caches can be reached at all.
    ///
    /// The heap is read immediately either side of the drop, inside the
    /// same `update`, so nothing else runs in between: no paint, no tile
    /// arrival, no rebake. The difference is that cache and nothing else.
    fn report_caches_once_settled(
        map: gpui::Entity<MapView>,
        compiled_layers: usize,
        width: f64,
        height: f64,
        cx: &mut App,
    ) {
        let target = drop_target();
        let strip = strip_target();
        let tour = tour_len();
        cx.spawn(async move |cx| {
            // The watcher thread owns the settle decision -- it is the only
            // thing that keeps sampling through a wedged main thread -- so
            // this waits for its verdict rather than forming its own.
            loop {
                cx.background_executor().timer(Duration::from_millis(50)).await;
                if SETTLED.load(Ordering::Relaxed) {
                    break;
                }
            }
            let tour_report = if tour > 0 {
                match drive_tour(&map, tour, cx).await {
                    Some(report) => Some(report),
                    // The view is gone, so there is nothing left to sample
                    // and no reason to keep going; the watcher's own grace
                    // period reports the run as one without cache figures.
                    None => return,
                }
            } else {
                None
            };
            map.update(cx, |view, _cx| {
                let counts = view.cache_entry_counts();
                let occupancy = view.cache_occupancy();
                let camera_viewport = view.viewport_px();
                // Outside the bracketed window on purpose: the census
                // builds distinct-key sets of its own, and an instrument
                // that allocates between the two heap reads is measuring
                // itself alongside the thing.
                let census = census_decoded(view);
                let stats = gpui_maps::render::allocation_stats();
                let heap_before = gpui_maps::render::heap_in_use_bytes();
                let dropped = target.map(|kind| view.drop_cache_for_measurement(kind));
                let stripped = strip.map(|part| strip_decoded(view, part));
                let heap_after = gpui_maps::render::heap_in_use_bytes();
                emit(
                    &stats,
                    true,
                    Duration::ZERO,
                    compiled_layers,
                    width,
                    height,
                    Some(CacheReport {
                        counts,
                        occupancy,
                        dropped,
                        stripped,
                        strip_name: strip_target_name(strip),
                        census,
                        heap_before,
                        heap_after,
                        tour: tour_report,
                        camera_viewport,
                    }),
                );
                std::process::exit(0);
            });
        })
        .detach();
    }

    /// Drives the deterministic camera tour, one move at a time, and
    /// reports what the heap did while it ran. `None` if the view went
    /// away mid-tour.
    ///
    /// Each move waits for the SAME settle rule the fixed-camera run
    /// waits for, on a detector of its own: a fresh detector starts with
    /// no previous count, so it cannot mistake "this move happened to
    /// need the same tiles" for "the move has finished loading" -- every
    /// move dwells at least a full settle window.
    ///
    /// The peak is taken over EVERY poll, not only over the settled
    /// instants, because the whole question a tour asks is what the heap
    /// reaches in transit. A per-move-settled maximum is reported
    /// alongside it, and the gap between the two is how much of the peak
    /// is transient.
    async fn drive_tour(
        map: &gpui::Entity<MapView>,
        count: usize,
        cx: &mut gpui::AsyncApp,
    ) -> Option<TourReport> {
        let window = Duration::from_secs(settle_secs());
        let move_timeout = Duration::from_secs(tour_move_timeout_secs());
        let started = Instant::now();
        // Taken after the pre-tour settle, so it is a genuine fixed-camera
        // steady state -- the same quantity a no-tour run reports -- read
        // in THIS process. The ratio against it is therefore paired: it
        // carries none of the cross-process variance a ratio of two means
        // from two different processes would, and it needs no auxiliary
        // counter to expose a run that never toured, because such a run
        // reports a ratio of 1.00.
        let pre_tour_heap = gpui_maps::render::heap_in_use_bytes();
        // Read in the same breath as the heap above, because the two are
        // only meaningful together: a fixed-camera heap is a heap PER
        // RESIDENT TILE, and that ratio is what separates a normal
        // baseline from one taken while startup allocation was still in
        // flight. See `tour_invalidity`.
        let pre_tour_tiles = gpui_maps::render::allocation_stats().tiles_resident;
        // The cover a run 5 of this file's own baseline batch (Task 0,
        // 2026-08-31) exposed a gap in: `pre_tour_tiles` above is what is
        // RESIDENT, but nothing checked it against what the CURRENT camera
        // actually NEEDS. That run held 108 tiles resident pre-tour against
        // a normal 20 (5.4x). Its per-tile heap was 4.09 MiB/tile -- BELOW
        // the 4.56-4.71 MiB/tile band its own sibling runs occupied, not
        // merely inside a normal range -- so `PRE_TOUR_HEAP_PER_TILE_CEILING`
        // (an upper bound only) never had a chance to fire, and it went on
        // to report a 0-hole tour against a 362-hole cumulative total: every
        // one of those holes happened during an abnormally long pre-tour
        // settle, not during the tour, and nothing here would have said so
        // without this. `cover_progress().cover` is `visible_tiles().len()`
        // for THIS camera, so the comparison in `tour_invalidity` below is
        // camera/viewport-agnostic -- it needs no hardcoded tile count.
        let pre_tour_cover =
            map.update(cx, |view, _cx| view.cover_progress().cover);
        let mut peak = pre_tour_heap;
        let mut settled_peak = peak;
        let mut completed = 0usize;
        let mut max_pending = 0usize;
        let buffers_at_start = gpui_maps::render::allocation_stats().gpu_buffers_created_total;
        // Same idiom as `buffers_at_start`: the cumulative counter is
        // process-global and includes the pre-tour settle (cold-cache
        // startup, where `gpu_meshes` is empty and every visible tile
        // takes the counted branch every frame until tiles land), so a
        // raw read at the end conflates "holes during startup" with
        // "holes during the sweep". Snapshotting here and reporting the
        // delta isolates the tour's own contribution -- see
        // `tour_visible_tiles_without_stand_in` below.
        #[cfg(feature = "probe")]
        let holes_at_start =
            gpui_maps::render::allocation_stats().visible_tiles_without_stand_in_total;
        // The bandwidth pair, snapshotted for exactly the same reason as
        // `holes_at_start`: the cold-cache settle fetches the whole first
        // cover, so a raw end-of-process total is dominated by startup and
        // says nothing about what the SWEEP costs. The delta is what a
        // change that adds requests has to be judged on -- see
        // `tour_tile_fetches`.
        #[cfg(feature = "probe")]
        let (fetches_at_start, fetch_bytes_at_start) = {
            let stats = gpui_maps::render::allocation_stats();
            (stats.tile_fetches_total, stats.tile_fetch_bytes_total)
        };
        // Frame timings and stand-in distances are RESET here rather than
        // snapshotted, because both carry a MAXIMUM and a maximum cannot be
        // turned into a delta: subtracting two readings of a running max
        // tells you nothing unless the tour's own worst happened to beat
        // startup's. The pre-tour values are captured first so nothing is
        // thrown away -- the cold settle's worst frame is itself worth
        // reporting -- and everything after this point belongs to the tour.
        //
        // The worst frame is the number that decides whether this change is
        // affordable. A rebake landing on the four frames of a sweep that
        // cross an integer zoom is four frames in ~11,000: invisible in a
        // mean, invisible in `tour_seconds`, and a dropped frame every time
        // the user crosses a zoom.
        #[cfg(feature = "probe")]
        let pre_tour_frames = {
            let stats = gpui_maps::render::allocation_stats();
            let frames = PhaseFrames {
                count: stats.frame_count,
                nanos_sum: stats.frame_nanos_sum,
                nanos_max: stats.frame_nanos_max,
                ms_p99: stats.frame_ms_p99,
                ms_p50: stats.frame_ms_p50,
                ancestor_stand_ins: stats.ancestor_stand_ins_total,
                ancestor_zoom_distance_sum: stats.ancestor_stand_in_zoom_distance_sum,
                ancestor_zoom_distance_max: stats.ancestor_stand_in_zoom_distance_max,
                descendant_stand_ins: stats.descendant_stand_ins_total,
            };
            gpui_maps::render::reset_frame_stats();
            frames
        };
        for (lon, lat, zoom) in tour_moves(count) {
            map.update(cx, |view, cx| {
                view.set_camera(lon, lat, zoom);
                // `set_camera` takes no context, like every other camera
                // method on `MapView`, so the repaint that fetches the new
                // cover is this caller's job.
                cx.notify();
            });
            let move_started = Instant::now();
            let mut arrived_since: Option<Instant> = None;
            let mut timed_out = false;
            loop {
                cx.background_executor().timer(Duration::from_millis(50)).await;
                let now = Instant::now();
                peak = peak.max(gpui_maps::render::heap_in_use_bytes());
                let progress = map.update(cx, |view, _cx| view.cover_progress());
                max_pending = max_pending.max(progress.pending);
                // The move is over when the map has FINISHED ARRIVING, not
                // when it has merely stopped changing. `tiles_resident`
                // holding still is the same observation on a loaded map and
                // on a window that stopped painting, and a tour driven off
                // it loads a different tile set on every run -- which puts
                // the non-determinism inside the quantity being measured.
                // Requiring the cover to be resident with nothing in flight
                // makes the retained set a function of the camera path and
                // nothing else.
                let arrived = progress.resident == progress.cover && progress.pending == 0;
                match (arrived, arrived_since) {
                    (true, None) => arrived_since = Some(now),
                    (false, _) => arrived_since = None,
                    (true, Some(_)) => {}
                }
                // Held for the full window, not merely touched: the
                // prefetch a zoom breath triggers arrives after the cover
                // does, and sampling between the two would count it in some
                // runs and not others.
                if arrived_since.is_some_and(|at| now.duration_since(at) >= window) {
                    completed += 1;
                    break;
                }
                if now.duration_since(move_started) >= move_timeout {
                    timed_out = true;
                    break;
                }
            }
            settled_peak = settled_peak.max(gpui_maps::render::heap_in_use_bytes());
            if timed_out {
                // Abandoned rather than waited out. A move that never
                // finished arriving means this run is not a tour of this
                // map, the parent will exclude it, and driving the
                // remaining moves would only spend minutes producing a
                // number nothing may use.
                break;
            }
        }
        let progress = map.update(cx, |view, _cx| view.cover_progress());
        Some(TourReport {
            moves: count,
            completed_moves: completed,
            peak_heap_bytes: peak,
            peak_settled_heap_bytes: settled_peak,
            pre_tour_heap_bytes: pre_tour_heap,
            pre_tour_tiles_resident: pre_tour_tiles,
            pre_tour_cover,
            unsettled_moves: count - completed,
            failed_tiles: progress.failed,
            max_pending,
            gpu_buffers_created: gpui_maps::render::allocation_stats()
                .gpu_buffers_created_total
                .saturating_sub(buffers_at_start),
            #[cfg(feature = "probe")]
            visible_tiles_without_stand_in: gpui_maps::render::allocation_stats()
                .visible_tiles_without_stand_in_total
                .saturating_sub(holes_at_start),
            #[cfg(feature = "probe")]
            tile_fetches: gpui_maps::render::allocation_stats()
                .tile_fetches_total
                .saturating_sub(fetches_at_start),
            #[cfg(feature = "probe")]
            tile_fetch_bytes: gpui_maps::render::allocation_stats()
                .tile_fetch_bytes_total
                .saturating_sub(fetch_bytes_at_start),
            #[cfg(feature = "probe")]
            pre_tour_frames,
            #[cfg(feature = "probe")]
            frames: {
                let stats = gpui_maps::render::allocation_stats();
                PhaseFrames {
                    count: stats.frame_count,
                    nanos_sum: stats.frame_nanos_sum,
                    nanos_max: stats.frame_nanos_max,
                    ms_p99: stats.frame_ms_p99,
                    ms_p50: stats.frame_ms_p50,
                    ancestor_stand_ins: stats.ancestor_stand_ins_total,
                    ancestor_zoom_distance_sum: stats.ancestor_stand_in_zoom_distance_sum,
                    ancestor_zoom_distance_max: stats.ancestor_stand_in_zoom_distance_max,
                    descendant_stand_ins: stats.descendant_stand_ins_total,
                }
            },
            seconds: started.elapsed().as_secs_f64(),
        })
    }

    /// What the camera tour did to the heap, for [`emit`] to write into
    /// the sample.
    struct TourReport {
        moves: usize,
        /// Moves that finished arriving inside their own timeout. Below
        /// `moves` means the tour was abandoned partway.
        completed_moves: usize,
        /// The largest `heap_in_use_bytes` seen at any poll during the
        /// tour -- the ceiling this task exists to measure.
        peak_heap_bytes: u64,
        /// The largest seen at a per-move SETTLED instant. Below the poll
        /// peak by however much of the ceiling is transient.
        peak_settled_heap_bytes: u64,
        /// The heap at the fixed camera, in THIS process, immediately
        /// before the first move -- the paired baseline the settled figure
        /// is a ratio of. See `drive_tour`.
        pre_tour_heap_bytes: u64,
        /// Tiles resident at that same instant. The denominator that turns
        /// `pre_tour_heap_bytes` into a figure with a known value -- see
        /// `tour_invalidity`'s per-tile rule.
        pre_tour_tiles_resident: u64,
        /// What the SAME camera, at that SAME instant, actually needs --
        /// `MapView::cover_progress().cover`. Compared against
        /// `pre_tour_tiles_resident` in `tour_invalidity`: a resident count
        /// well beyond what the camera needs means the "steady state" the
        /// tour is about to measure from is carrying surplus from an
        /// abnormal pre-tour settle, not a genuine settled baseline.
        pre_tour_cover: usize,
        /// Moves that never finished arriving. Anything but zero makes the
        /// run's figures a tour that did not happen, and the parent
        /// excludes it.
        unsettled_moves: usize,
        /// Tiles whose fetch or decode failed. Non-zero makes the retained
        /// set a function of the network as well as of the camera, so the
        /// parent excludes the run rather than averaging it in.
        failed_tiles: usize,
        /// The largest number of fetches in flight at any poll -- a
        /// diagnostic for how much the drain condition was actually
        /// waiting on.
        max_pending: usize,
        /// GPU buffers created across the WHOLE tour, and the only
        /// signal that separates a tour of this map from a tour of a
        /// window that stopped painting.
        /// `gpu_buffers_created_total` leaves zero inside `ensure_gpu`,
        /// which runs in `paint` and nowhere else, so a run that ends
        /// with zero here moved a camera nobody was looking at.
        ///
        /// Whole-tour and not per-move, because a per-move version was
        /// written, measured, and thrown away: a move is ~0.37 km at z14
        /// against a ~2.4 km tile, so most moves legitimately bring
        /// nothing, and the new tiles arrive in bursts as the zoom breath
        /// crosses an integer level. A healthy 24-move tour was measured
        /// with 20 of its 24 moves creating no buffer at all, so a count
        /// of those says nothing about liveness. The full circuit creates
        /// ~175 on top of the ~65 a settled fixed camera already has.
        gpu_buffers_created: u64,
        /// Holes (`visible_tiles_without_stand_in_total`) counted DURING
        /// the tour only, i.e. AFTER the pre-tour settle -- a delta
        /// against the cumulative process-global counter, the same way
        /// `gpu_buffers_created` above is a delta rather than the raw
        /// total. The cumulative field (`visible_tiles_without_stand_in_total`
        /// at the top level of the sample) still includes the pre-tour
        /// settle, on purpose: it stays comparable with every prior figure
        /// recorded under that name. `None` without `--features probe`.
        #[cfg(feature = "probe")]
        visible_tiles_without_stand_in: u64,
        /// Tile fetches ISSUED during the tour only -- a delta against the
        /// process-global `tile_fetches_total`, for the same reason
        /// `visible_tiles_without_stand_in` above is a delta: the cold
        /// settle fetches an entire first cover, so the process total is
        /// mostly startup.
        ///
        /// This is the bandwidth half of the coarse-prefetch trade. A
        /// change that buys fewer holes by fetching more tiles has to
        /// state both numbers or it has stated nothing.
        #[cfg(feature = "probe")]
        tile_fetches: u64,
        /// Bytes those fetches returned, before decode -- the same delta,
        /// in bytes rather than requests. Tiles are not equal in size and
        /// a coarse tile is not a fine one, so the request count alone
        /// would understate or overstate the cost depending on which
        /// zooms the extra requests land at.
        #[cfg(feature = "probe")]
        tile_fetch_bytes: u64,
        /// Frame timings and stand-in distances for the pre-tour settle
        /// and for the tour, kept apart rather than differenced -- see
        /// where they are captured for why a maximum cannot be a delta.
        #[cfg(feature = "probe")]
        pre_tour_frames: PhaseFrames,
        #[cfg(feature = "probe")]
        frames: PhaseFrames,
        seconds: f64,
    }

    /// One phase's frame timings and stand-in accounting.
    ///
    /// `nanos_max` is the field to read first. A budget is a ceiling, and
    /// a mean frame time cannot violate one: the cost this measures lands
    /// on the handful of frames that cross an integer zoom, where a
    /// coarse tile's plan goes stale and is re-tessellated.
    ///
    /// The stand-in fields are the successor to
    /// `visible_tiles_without_stand_in_total`, which the coarse prefetch
    /// cover saturates at zero -- see
    /// `render::alloc::ANCESTOR_STAND_INS`. `ancestor_zoom_distance_sum /
    /// ancestor_stand_ins` is the average magnification the user was
    /// actually shown, and it is the quantity a later task that improves
    /// stand-in QUALITY (rather than stand-in presence) has to move.
    #[cfg(feature = "probe")]
    #[derive(Clone, Copy)]
    struct PhaseFrames {
        count: u64,
        nanos_sum: u64,
        nanos_max: u64,
        ms_p99: u64,
        ms_p50: u64,
        ancestor_stand_ins: u64,
        ancestor_zoom_distance_sum: u64,
        ancestor_zoom_distance_max: u64,
        descendant_stand_ins: u64,
    }

    #[cfg(feature = "probe")]
    impl PhaseFrames {
        /// Writes this phase's fields into `object` under `prefix`.
        fn insert_into(&self, prefix: &str, object: &mut serde_json::Map<String, serde_json::Value>) {
            let ms = |nanos: u64| nanos as f64 / 1.0e6;
            object.insert(format!("{prefix}frames"), self.count.into());
            object.insert(format!("{prefix}frame_ms_max"), ms(self.nanos_max).into());
            object.insert(
                format!("{prefix}frame_ms_mean"),
                if self.count == 0 { 0.0 } else { ms(self.nanos_sum) / self.count as f64 }.into(),
            );
            object.insert(format!("{prefix}frame_ms_p99"), self.ms_p99.into());
            object.insert(format!("{prefix}frame_ms_p50"), self.ms_p50.into());
            object.insert(format!("{prefix}ancestor_stand_ins"), self.ancestor_stand_ins.into());
            object.insert(
                format!("{prefix}ancestor_stand_in_zoom_distance_sum"),
                self.ancestor_zoom_distance_sum.into(),
            );
            object.insert(
                format!("{prefix}ancestor_stand_in_zoom_distance_max"),
                self.ancestor_zoom_distance_max.into(),
            );
            object.insert(
                format!("{prefix}ancestor_stand_in_zoom_distance_mean"),
                if self.ancestor_stand_ins == 0 {
                    0.0
                } else {
                    self.ancestor_zoom_distance_sum as f64 / self.ancestor_stand_ins as f64
                }
                .into(),
            );
            object.insert(format!("{prefix}descendant_stand_ins"), self.descendant_stand_ins.into());
        }
    }

    /// What the main thread found, for [`emit`] to write into the sample.
    struct CacheReport {
        counts: [(gpui_maps::map_view::TileCacheKind, usize); 8],
        /// Each per-tile cache's length against its bound. The length
        /// alone cannot say which cache is retaining; only the ratio can.
        occupancy: Vec<(&'static str, usize, usize)>,
        /// Entries the requested drop removed, or `None` if no drop was
        /// requested -- which is not the same as a drop that removed zero.
        dropped: Option<usize>,
        /// `(stripped, shared)` from the requested half-tile free, or
        /// `None` if none was requested. `shared` above zero means some
        /// entry had another live `Arc` owner and kept its bytes, which
        /// makes the freed figure a floor rather than the answer.
        stripped: Option<(usize, usize)>,
        strip_name: &'static str,
        /// What `decoded_cache` held structurally, counted before the
        /// heap was sampled. `None` without the `probe` feature.
        census: Option<Census>,
        heap_before: u64,
        heap_after: u64,
        /// What the camera tour did, or `None` for a fixed-camera run.
        tour: Option<TourReport>,
        /// The viewport the CAMERA actually has, as opposed to the one the
        /// harness asked for. Read from `MapView::viewport_px` in the same
        /// `update` as the occupancy, so a window that did not open at the
        /// requested size is visible in the sample instead of silently
        /// reporting the request back.
        camera_viewport: (f32, f32),
    }

    /// The census fields, flattened to plain numbers so the rest of this
    /// file -- and the JSON it emits -- needs no `cfg` of its own.
    #[derive(Clone, Copy)]
    struct Census {
        tiles: usize,
        layers: usize,
        features: usize,
        tag_entries: usize,
        distinct_keys_per_tile: usize,
        distinct_keys_overall: usize,
        key_bytes: usize,
        distinct_key_bytes_overall: usize,
        string_values: usize,
        distinct_string_values_overall: usize,
        string_value_bytes: usize,
        distinct_string_value_bytes_overall: usize,
        geometry_coords: usize,
        geometry_coord_capacity: usize,
        geometry_vecs: usize,
        geometry_alloc_bytes: usize,
    }

    #[cfg(feature = "probe")]
    fn census_decoded(view: &MapView) -> Option<Census> {
        let c = view.census_decoded_for_measurement();
        Some(Census {
            tiles: c.tiles,
            layers: c.layers,
            features: c.features,
            tag_entries: c.tag_entries,
            distinct_keys_per_tile: c.distinct_keys_per_tile,
            distinct_keys_overall: c.distinct_keys_overall,
            key_bytes: c.key_bytes,
            distinct_key_bytes_overall: c.distinct_key_bytes_overall,
            string_values: c.string_values,
            distinct_string_values_overall: c.distinct_string_values_overall,
            string_value_bytes: c.string_value_bytes,
            distinct_string_value_bytes_overall: c.distinct_string_value_bytes_overall,
            geometry_coords: c.geometry_coords,
            geometry_coord_capacity: c.geometry_coord_capacity,
            geometry_vecs: c.geometry_vecs,
            geometry_alloc_bytes: c.geometry_alloc_bytes,
        })
    }

    #[cfg(not(feature = "probe"))]
    fn census_decoded(_view: &MapView) -> Option<Census> {
        None
    }

    #[cfg(feature = "probe")]
    fn strip_decoded(
        view: &mut MapView,
        part: gpui_maps::mvt::DecodedTilePart,
    ) -> (usize, usize) {
        view.strip_decoded_for_measurement(part)
    }

    #[cfg(not(feature = "probe"))]
    fn strip_decoded(_view: &mut MapView, part: std::convert::Infallible) -> (usize, usize) {
        match part {}
    }

    /// Polls the counters on a thread of its own, prints one sample the
    /// moment the map settles, and ends the process.
    ///
    /// A thread rather than a gpui task because it must keep sampling
    /// through exactly the states the map might be stuck in -- a wedged
    /// main thread, a window that never paints -- and a task scheduled on
    /// that main thread would be stuck alongside it. Everything it reads is
    /// a process-global atomic, so it needs no access to the `MapView`.
    fn watch_for_settle(compiled_layers: usize, width: f64, height: f64) {
        let window = Duration::from_secs(settle_secs());
        let timeout = Duration::from_secs(timeout_secs());
        let floor = zero_tile_floor();
        std::thread::Builder::new()
            .name("memory-probe-settle".to_owned())
            .spawn(move || {
                let started = Instant::now();
                let grace = cache_report_grace();
                let mut settled_at: Option<Instant> = None;
                let mut tiles = SettleDetector::new(window, started);
                let mut floor_detector =
                    FloorSettleDetector::new(window, FLOOR_HEAP_TOLERANCE_BYTES, started);
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    let now = Instant::now();
                    let stats = gpui_maps::render::allocation_stats();
                    let state = if floor {
                        floor_detector.observe(
                            now,
                            stats.tiles_resident,
                            stats.gpu_buffers_created_total,
                            stats.heap_in_use_bytes,
                        )
                    } else {
                        tiles.observe(now, stats.tiles_resident)
                    };
                    let timed_out = now.duration_since(started) >= timeout;
                    if state == Settle::Settled {
                        // Hand off: the main thread reads the caches and
                        // emits (see `report_caches_once_settled`). This
                        // thread stays alive only as the backstop for a
                        // main thread that never gets there.
                        if !SETTLED.swap(true, Ordering::Relaxed) {
                            settled_at = Some(now);
                        }
                    }
                    let gave_up_waiting =
                        settled_at.is_some_and(|at| now.duration_since(at) >= grace);
                    if gave_up_waiting || timed_out {
                        emit(
                            &stats,
                            state == Settle::Settled,
                            started.elapsed(),
                            compiled_layers,
                            width,
                            height,
                            None,
                        );
                        // The map has nothing further to tell us and gpui
                        // owns the main loop, so there is no orderly return
                        // to hand control back to. Exit is the shutdown.
                        std::process::exit(0);
                    }
                }
            })
            .expect("failed to start the settle watcher");
    }

    fn emit(
        stats: &gpui_maps::render::AllocationStats,
        settled: bool,
        elapsed: Duration,
        compiled_layers: usize,
        width: f64,
        height: f64,
        report: Option<CacheReport>,
    ) {
        let mut sample = serde_json::to_value(stats).unwrap_or_default();
        if let Some(object) = sample.as_object_mut() {
            object.insert("settled".to_owned(), settled.into());
            object.insert("seconds_to_sample".to_owned(), elapsed.as_secs_f64().into());
            object.insert("compiled_layers".to_owned(), compiled_layers.into());
            // Carried in the sample rather than only in the harness's
            // header, so the aggregate table proves the viewport was in
            // fact identical across the runs being averaged.
            object.insert("viewport_width_px".to_owned(), width.into());
            object.insert("viewport_height_px".to_owned(), height.into());
            object.insert("drop_cache".to_owned(), drop_target_name(drop_target()).into());
            // False for a run whose main thread never took the sample. The
            // parent excludes those: a run with no cache figures in it is a
            // measurement of a process that stopped serving tasks.
            object.insert("cache_report".to_owned(), report.is_some().into());
            if let Some(report) = report {
                for (kind, count) in report.counts {
                    object.insert(
                        format!("entries_{}", drop_target_name(Some(kind))),
                        count.into(),
                    );
                }
                for (name, len, capacity) in &report.occupancy {
                    object.insert(format!("occupancy_len_{name}"), (*len).into());
                    object.insert(format!("occupancy_cap_{name}"), (*capacity).into());
                }
                // Beside `viewport_width_px`/`viewport_height_px`, which
                // are what the harness ASKED for. Equal in a healthy run;
                // a difference means the sample's cover is not the one the
                // run was configured for.
                object.insert(
                    "camera_viewport_width_px".to_owned(),
                    f64::from(report.camera_viewport.0).into(),
                );
                object.insert(
                    "camera_viewport_height_px".to_owned(),
                    f64::from(report.camera_viewport.1).into(),
                );
                if let Some(tour) = &report.tour {
                    object.insert("tour_moves".to_owned(), tour.moves.into());
                    object.insert("tour_peak_heap_bytes".to_owned(), tour.peak_heap_bytes.into());
                    object.insert(
                        "tour_peak_settled_heap_bytes".to_owned(),
                        tour.peak_settled_heap_bytes.into(),
                    );
                    object.insert(
                        "tour_completed_moves".to_owned(),
                        tour.completed_moves.into(),
                    );
                    object.insert(
                        "tour_unsettled_moves".to_owned(),
                        tour.unsettled_moves.into(),
                    );
                    object.insert("tour_failed_tiles".to_owned(), tour.failed_tiles.into());
                    object.insert("tour_max_pending".to_owned(), tour.max_pending.into());
                    object.insert(
                        "pre_tour_heap_bytes".to_owned(),
                        tour.pre_tour_heap_bytes.into(),
                    );
                    object.insert(
                        "pre_tour_tiles_resident".to_owned(),
                        tour.pre_tour_tiles_resident.into(),
                    );
                    object.insert("pre_tour_cover".to_owned(), tour.pre_tour_cover.into());
                    // The paired ratio, computed where both halves are in
                    // hand, so the aggregate table reports the deviation of
                    // the RATIO rather than of two heaps that would have to
                    // be divided after the fact.
                    if tour.pre_tour_heap_bytes > 0 {
                        let ratio = stats.heap_in_use_bytes as f64
                            / tour.pre_tour_heap_bytes as f64;
                        object.insert("tour_heap_ratio".to_owned(), ratio.into());
                    }
                    object.insert(
                        "tour_gpu_buffers_created".to_owned(),
                        tour.gpu_buffers_created.into(),
                    );
                    #[cfg(feature = "probe")]
                    object.insert(
                        "tour_visible_tiles_without_stand_in".to_owned(),
                        tour.visible_tiles_without_stand_in.into(),
                    );
                    #[cfg(feature = "probe")]
                    object.insert("tour_tile_fetches".to_owned(), tour.tile_fetches.into());
                    #[cfg(feature = "probe")]
                    object.insert(
                        "tour_tile_fetch_bytes".to_owned(),
                        tour.tile_fetch_bytes.into(),
                    );
                    #[cfg(feature = "probe")]
                    tour.frames.insert_into("tour_", object);
                    #[cfg(feature = "probe")]
                    tour.pre_tour_frames.insert_into("pre_tour_", object);
                    object.insert("tour_seconds".to_owned(), tour.seconds.into());
                }
                object.insert("heap_before_drop_bytes".to_owned(), report.heap_before.into());
                object.insert("heap_after_drop_bytes".to_owned(), report.heap_after.into());
                object.insert(
                    "heap_freed_bytes".to_owned(),
                    report.heap_before.saturating_sub(report.heap_after).into(),
                );
                if let Some(dropped) = report.dropped {
                    object.insert("dropped_entries".to_owned(), dropped.into());
                }
                object.insert("strip_decoded".to_owned(), report.strip_name.into());
                if let Some((stripped, shared)) = report.stripped {
                    object.insert("stripped_entries".to_owned(), stripped.into());
                    object.insert("shared_entries".to_owned(), shared.into());
                }
                if let Some(c) = report.census {
                    for (name, value) in [
                        ("census_tiles", c.tiles),
                        ("census_layers", c.layers),
                        ("census_features", c.features),
                        ("census_tag_entries", c.tag_entries),
                        ("census_distinct_keys_per_tile", c.distinct_keys_per_tile),
                        ("census_distinct_keys_overall", c.distinct_keys_overall),
                        ("census_key_bytes", c.key_bytes),
                        ("census_distinct_key_bytes_overall", c.distinct_key_bytes_overall),
                        ("census_string_values", c.string_values),
                        ("census_distinct_string_values_overall", c.distinct_string_values_overall),
                        ("census_string_value_bytes", c.string_value_bytes),
                        (
                            "census_distinct_string_value_bytes_overall",
                            c.distinct_string_value_bytes_overall,
                        ),
                        ("census_geometry_coords", c.geometry_coords),
                        ("census_geometry_coord_capacity", c.geometry_coord_capacity),
                        ("census_geometry_vecs", c.geometry_vecs),
                        ("census_geometry_alloc_bytes", c.geometry_alloc_bytes),
                    ] {
                        object.insert(name.to_owned(), value.into());
                    }
                }
            }
        }
        println!("{STATS_PREFIX}{}", serde_json::to_string(&sample).unwrap_or_default());
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::{tour_moves, zoom_sweep_moves, FloorSettleDetector, Settle, SettleDetector, Summary};
    use std::time::{Duration, Instant};

    /// A synthetic clock. The detector takes the time as an argument rather
    /// than reading it, precisely so its behaviour over a minute can be
    /// tested in microseconds.
    fn at(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }

    #[test]
    fn a_map_with_no_tiles_never_settles_however_long_it_waits() {
        let base = Instant::now();
        let mut detector = SettleDetector::new(Duration::from_secs(4), base);
        for second in 0..600 {
            assert_eq!(
                detector.observe(at(base, second), 0),
                Settle::Empty,
                "zero tiles is unchanged forever; settling on it would turn a window that \
                 never painted into a confident measurement of nothing"
            );
        }
    }

    #[test]
    fn a_still_non_zero_count_settles_when_the_window_is_full_and_not_before() {
        let base = Instant::now();
        let mut detector = SettleDetector::new(Duration::from_secs(4), base);
        assert_eq!(detector.observe(at(base, 0), 12), Settle::Loading);
        assert_eq!(detector.observe(at(base, 3), 12), Settle::Loading, "3s < the 4s window");
        assert_eq!(detector.observe(at(base, 4), 12), Settle::Settled);
    }

    #[test]
    fn a_count_that_keeps_moving_never_settles() {
        let base = Instant::now();
        let mut detector = SettleDetector::new(Duration::from_secs(4), base);
        for second in 0..30 {
            assert_eq!(
                detector.observe(at(base, second), second + 1),
                Settle::Loading,
                "a count still climbing at second {second} is a map still loading"
            );
        }
    }

    #[test]
    fn a_late_arriving_tile_restarts_the_window() {
        let base = Instant::now();
        let mut detector = SettleDetector::new(Duration::from_secs(4), base);
        // Nearly settled on 12 ...
        assert_eq!(detector.observe(at(base, 0), 12), Settle::Loading);
        assert_eq!(detector.observe(at(base, 3), 12), Settle::Loading);
        // ... then a second burst lands. Tiles arrive in bursts and a gap
        // between two of them is not the end of loading.
        assert_eq!(detector.observe(at(base, 4), 20), Settle::Loading);
        assert_eq!(detector.observe(at(base, 7), 20), Settle::Loading, "the window restarted");
        assert_eq!(detector.observe(at(base, 8), 20), Settle::Settled);
    }

    #[test]
    fn tiles_being_evicted_back_to_zero_un_settles_the_run() {
        let base = Instant::now();
        let mut detector = SettleDetector::new(Duration::from_secs(4), base);
        assert_eq!(detector.observe(at(base, 0), 12), Settle::Loading);
        assert_eq!(detector.observe(at(base, 4), 12), Settle::Settled);
        assert_eq!(
            detector.observe(at(base, 5), 0),
            Settle::Empty,
            "an emptied cache is not a settled map, even having once been one"
        );
    }

    /// A floor detector with a 4s window and a 1 MiB tolerance, on a
    /// synthetic clock.
    fn floor(base: Instant) -> FloorSettleDetector {
        FloorSettleDetector::new(Duration::from_secs(4), 1 << 20, base)
    }

    #[test]
    fn the_floor_never_settles_on_a_window_that_never_painted() {
        let base = Instant::now();
        let mut detector = floor(base);
        for second in 0..600 {
            assert_eq!(
                detector.observe(at(base, second), 0, 0, 400_000_000),
                Settle::Empty,
                "no GPU buffer means `ensure_gpu` never ran, which means `paint` never ran -- \
                 a locked screen must not be able to report a floor"
            );
        }
    }

    #[test]
    fn the_floor_settles_once_a_painted_map_stops_allocating() {
        let base = Instant::now();
        let mut detector = floor(base);
        assert_eq!(detector.observe(at(base, 0), 0, 12, 400_000_000), Settle::Loading);
        assert_eq!(
            detector.observe(at(base, 3), 0, 12, 400_100_000),
            Settle::Loading,
            "3s < the 4s window, even though the heap is inside tolerance"
        );
        assert_eq!(detector.observe(at(base, 4), 0, 12, 400_100_000), Settle::Settled);
    }

    #[test]
    fn a_heap_still_climbing_restarts_the_floor_window() {
        let base = Instant::now();
        let mut detector = floor(base);
        for second in 0..30 {
            let heap = 400_000_000 + second * 10_000_000;
            assert_eq!(
                detector.observe(at(base, second), 0, 12, heap),
                Settle::Loading,
                "a heap still growing at second {second} is a process still starting up"
            );
        }
    }

    #[test]
    fn a_slow_creep_inside_the_per_sample_epsilon_still_does_not_settle() {
        let base = Instant::now();
        let mut detector = floor(base);
        // 400 KB per second: never more than the 1 MiB tolerance away from
        // the PREVIOUS sample, but 1.6 MB away from where the window
        // opened by the time the window would otherwise be full. Measured
        // from the anchor, this is not stillness.
        for second in 0..10 {
            let heap = 400_000_000 + second * 400_000;
            assert_eq!(
                detector.observe(at(base, second), 0, 12, heap),
                Settle::Loading,
                "second {second}: a creep that outruns the tolerance across the window is \
                 growth, however small each step is"
            );
        }
    }

    #[test]
    fn a_resident_tile_disqualifies_a_floor_run_rather_than_settling_it() {
        let base = Instant::now();
        let mut detector = floor(base);
        assert_eq!(detector.observe(at(base, 0), 0, 12, 400_000_000), Settle::Loading);
        for second in 1..30 {
            assert_eq!(
                detector.observe(at(base, second), 1, 12, 400_000_000),
                Settle::Loading,
                "this configuration asked for zero tiles; a run that got one is a different \
                 measurement wearing the floor's label, and must run out its timeout"
            );
        }
    }

    /// One sample as the child emits it, with `overrides` applied.
    fn tour_sample(overrides: &[(&str, u64)]) -> serde_json::Value {
        let mut sample = serde_json::json!({
            "tour_moves": 24,
            "tour_completed_moves": 24,
            "tour_unsettled_moves": 0,
            "tour_failed_tiles": 0,
            "tour_gpu_buffers_created": 174,
            // A normal fixed-camera baseline: 92 MB over 20 tiles, which
            // is 4.6 MB/tile. Present in the default sample so every other
            // rule's test keeps testing that rule and not this one.
            "pre_tour_heap_bytes": 92u64 << 20,
            "pre_tour_tiles_resident": 20,
            // A normal pre-tour ratio: resident tiles at (or a hair above)
            // what the camera's own cover needs. Present by default for the
            // same reason `pre_tour_heap_bytes` is: every rule's test other
            // than the surplus rule's own must keep testing that rule, not
            // this one.
            "pre_tour_cover": 20,
        });
        for (name, value) in overrides {
            sample[*name] = (*value).into();
        }
        sample
    }

    #[test]
    fn a_tour_that_toured_is_not_excluded() {
        assert_eq!(super::parent::tour_invalidity(&tour_sample(&[])), None);
    }

    #[test]
    fn a_fixed_camera_run_has_no_tour_to_invalidate() {
        // Every rule below is about a tour. A run without one must pass
        // them all, or the fixed-camera measurement this harness has been
        // reporting for a week would start excluding itself.
        assert_eq!(super::parent::tour_invalidity(&serde_json::json!({})), None);
        assert_eq!(super::parent::tour_invalidity(&tour_sample(&[("tour_moves", 0)])), None);
    }

    #[test]
    fn a_tour_whose_window_never_painted_is_excluded() {
        // The failure that made 6 of 28 runs in the first measurement pass
        // look exactly like successes: an occluded window stops painting,
        // `tiles_resident` freezes non-zero and unchanged, and every move
        // "settles" instantly. `gpu_buffers_created_total` leaves zero
        // only inside `ensure_gpu`, which runs in `paint`.
        let reason = super::parent::tour_invalidity(&tour_sample(&[(
            "tour_gpu_buffers_created",
            0,
        )]))
        .expect("a tour that created no GPU buffer toured nothing");
        assert!(reason.contains("never painted"), "{reason}");
    }

    #[test]
    fn a_tour_with_a_move_that_never_arrived_is_excluded() {
        let reason =
            super::parent::tour_invalidity(&tour_sample(&[("tour_unsettled_moves", 6)]))
                .expect("an abandoned tour is not a tour");
        assert!(reason.contains("6 of 24"), "{reason}");
    }

    #[test]
    fn a_tour_that_lost_a_tile_to_the_network_is_excluded() {
        // Not pedantry: what a tour retains is meant to be a function of
        // the camera path alone, and a failed fetch makes it a function of
        // the network too. Two such runs are not two samples of one thing.
        let reason = super::parent::tour_invalidity(&tour_sample(&[("tour_failed_tiles", 3)]))
            .expect("a tour that lost tiles measured a different tour");
        assert!(reason.contains('3'), "{reason}");
    }

    #[test]
    fn a_tour_whose_baseline_was_not_a_steady_state_is_excluded() {
        // The failure this was written for: one real run reported a
        // FIXED-CAMERA heap of 483 MB over the usual 20 tiles, with normal
        // tile counters, a normal 65 pre-tour GPU buffers, and every other
        // validity rule satisfied. Its `tour_heap_ratio` came out at 1.34
        // -- comfortably INSIDE the budget, because the anomaly is in the
        // denominator. Nothing caught it.
        let reason = super::parent::tour_invalidity(&tour_sample(&[(
            "pre_tour_heap_bytes",
            483u64 << 20,
        )]))
        .expect("a 24 MB/tile baseline is not a steady state");
        assert!(reason.contains("483"), "{reason}");
        assert!(reason.contains("24"), "{reason}");
    }

    #[test]
    fn a_pre_tour_baseline_carrying_5x_its_own_cover_is_excluded() {
        // The failure this was written for: one real run (Task 0's own
        // baseline batch, 2026-08-31) held 108 tiles resident pre-tour
        // against a same-camera cover of ~20 -- 5.4x -- with its per-tile
        // heap (4.09 MiB/tile) actually BELOW its sibling runs' 4.56-4.71
        // MiB/tile band, so `PRE_TOUR_HEAP_PER_TILE_CEILING` (an upper
        // bound only) could not have caught it even in principle, and
        // every other validity rule was satisfied. It went on to report a
        // 0-hole tour against a 362-hole
        // cumulative total: every hole happened during the abnormally long
        // pre-tour settle, and nothing here said so.
        let reason = super::parent::tour_invalidity(&tour_sample(&[
            ("pre_tour_tiles_resident", 108),
            ("pre_tour_cover", 20),
        ]))
        .expect("108 resident tiles against a cover of 20 is not a steady state");
        assert!(reason.contains("108"), "{reason}");
        assert!(reason.contains("20"), "{reason}");
    }

    #[test]
    fn a_pre_tour_surplus_ratio_right_at_the_ceiling_is_not_excluded() {
        // The rule that matters most, as with every other rule here: a
        // healthy run's own generous prefetch must survive it. 3x the
        // cover is the documented ceiling, not the trigger -- only a ratio
        // GREATER than the ceiling excludes.
        assert!(super::parent::tour_invalidity(&tour_sample(&[
            ("pre_tour_tiles_resident", 60),
            ("pre_tour_cover", 20),
        ]))
        .is_none());
    }

    #[test]
    fn a_pre_tour_cover_of_zero_does_not_divide_by_zero() {
        // An older sample, or a tour that never reached the point
        // `pre_tour_cover` is captured, must not be excluded by a
        // comparison it cannot make.
        assert!(super::parent::tour_invalidity(&tour_sample(&[("pre_tour_cover", 0)])).is_none());
    }

    #[test]
    fn a_normal_baseline_is_not_excluded_by_the_per_tile_rule() {
        // The rule that matters most, as with every other rule here: a
        // healthy run must survive it, or the harness starts excluding the
        // measurement it exists to take. 4.6 MB/tile is what every valid
        // run this instrument has produced reports.
        assert!(super::parent::tour_invalidity(&tour_sample(&[])).is_none());
        // And a run with no pre-tour figures at all (an older sample, or a
        // tour that never reached `drive_tour`) must not be excluded by a
        // division it cannot do.
        let mut sample = tour_sample(&[]);
        sample["pre_tour_tiles_resident"] = 0u64.into();
        assert!(super::parent::tour_invalidity(&sample).is_none());
    }

    #[test]
    fn a_zoom_sweep_crosses_four_integer_levels_and_comes_home() {
        let sweep = zoom_sweep_moves(24);
        assert_eq!(sweep.len(), 24);
        // Fixed ground position: a hole counted during this one cannot
        // have come from panning.
        assert!(
            sweep.iter().all(|(lon, lat, _)| *lon == sweep[0].0 && *lat == sweep[0].1),
            "the sweep must not pan"
        );
        let (_, _, last) = *sweep.last().unwrap();
        assert!((last - sweep[0].2).abs() < 1e-9, "the sweep must end where it began");
        let crossings = sweep
            .windows(2)
            .filter(|w| w[0].2.floor() != w[1].2.floor())
            .count();
        assert_eq!(crossings, 4, "14 -> 15 -> 13 -> 14 crosses an integer level four times");
        // And it must actually spend moves in the band where the bound is
        // smallest and prefetch fires, or it is not testing the crossing.
        assert!(
            sweep.iter().any(|(_, _, z)| (0.7..1.0).contains(&z.fract())),
            "the sweep must pass through frac 0.7..1.0"
        );
    }

    #[test]
    fn tour_moves_are_deterministic_and_return_to_the_start() {
        let a = tour_moves(24);
        let b = tour_moves(24);
        assert_eq!(a, b, "two tours of the same length must be identical");
        assert_eq!(a.len(), 24);
        let (last_lon, last_lat, last_zoom) = *a.last().unwrap();
        let (first_lon, first_lat, first_zoom) = a[0];
        // Ending where it started means the settled sample is taken at the
        // SAME camera as the steady-state baseline, so the two are
        // comparable and the only difference is what the tour retained.
        assert!((last_lon - first_lon).abs() < 1e-9, "lon {last_lon} != {first_lon}");
        assert!((last_lat - first_lat).abs() < 1e-9, "lat {last_lat} != {first_lat}");
        assert!((last_zoom - first_zoom).abs() < 1e-9, "zoom {last_zoom} != {first_zoom}");
    }

    #[test]
    fn the_summary_reports_the_sample_deviation_not_the_population_one() {
        // Mean 4, deviations -2,-1,+1,+2, sum of squares 10. Sample: 10/3.
        let summary = Summary::of(&[2.0, 3.0, 5.0, 6.0]).expect("four values summarise");
        assert_eq!(summary.runs, 4);
        assert!((summary.mean - 4.0).abs() < 1e-12);
        assert!((summary.min - 2.0).abs() < 1e-12);
        assert!((summary.max - 6.0).abs() < 1e-12);
        assert!(
            (summary.std_dev - (10.0f64 / 3.0).sqrt()).abs() < 1e-12,
            "n-1 in the divisor: these runs are a sample of the runs that could have \
             happened, not the whole population of them"
        );
    }

    #[test]
    fn a_single_run_has_no_deviation_to_report() {
        let summary = Summary::of(&[100.0]).expect("one value summarises");
        assert!(
            summary.std_dev.is_nan(),
            "0.0 would read as 'measured, and it does not vary' -- which is the false \
             confidence this whole harness exists to stop"
        );
        assert!(summary.relative_std_dev_percent().is_nan());
    }

    #[test]
    fn nothing_summarises_to_nothing() {
        assert!(Summary::of(&[]).is_none(), "there is no mean of no runs");
    }
}
