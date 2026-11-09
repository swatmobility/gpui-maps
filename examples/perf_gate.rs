//! The census regression gate: `demos/us_block_groups.rs`, run as a
//! subprocess, graded against `stress/us_block_groups_baseline.json`.
//!
//! Driven by `scripts/perf-gate.sh`, which builds this runner and the demo
//! (`--features probe`) together and does the preflight; see
//! `docs/perf-gate.md` for what it measures and how to read it.
//!
//! # Why a sibling of `stress_suite`, not a section of it
//!
//! The stress suite is one headless process that says in its own doc
//! comment it "opens no window", and its scenarios share one gpui app. The
//! census benchmark is neither: its windowed tier needs a real, frontmost
//! window, one per mode, and its headless tier loads 70.7 M coordinates and
//! holds ~4.5 GB, which a shared process would carry into every scenario
//! after it. So the gate runs the DEMO -- the exact binary the numbers in
//! `docs/measurements/` came from -- once per mode and parses its `gate:`
//! lines, and it shares everything else with the suite: the baseline file
//! format and grader (`kit/src/stress.rs`: `Bound`, `Baseline`,
//! `grade`, the rationale on every band), the `Env::quiet` definition of a
//! busy machine, and the refusal to invent data (a missing dataset is a
//! skip that exits 2, never a pass).
//!
//! # Exit codes
//!
//! 0 every graded metric passed; 1 a hard regression; 2 nothing could be
//! scored (no data, every run invalid, no baseline); 3 a safety preflight
//! refused to start a run (memory or disk).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use swat_maps_kit::stress;

use stress::{Baseline, Bound};

/// One way of running the demo.
struct Mode {
    /// The `gate:` prefix this mode owns; lines under other prefixes (the
    /// windowed run's own probe report prints `headless.*` rows) are ignored.
    prefix: &'static str,
    tier: Tier,
    args: &'static [&'static str],
    /// Wall-clock bound on one run; the child is killed past it.
    timeout: Duration,
    /// Peak RSS projected for the memory preflight, in MB: the fallback
    /// when the baseline has no `peak_rss_mb`/`rss_leg_max_mb` figure of
    /// its own, AND a cap on that figure when it does. A baseline's
    /// `ceiling()` scales the measured peak by its regression-grading
    /// tolerance (sized to absorb run-to-run noise, e.g. headless's own
    /// 1.36x for a 16.5% spread), which overshoots what the mode actually
    /// needs to run -- `headless.peak_rss_mb`'s ceiling is 6067 MB against
    /// a measured max of 4468.7 MB. This field is a generously-rounded
    /// figure taken directly from measured maxima (see the comment above
    /// `MODES`), so the memory safety check stays mode-aware and matches
    /// `scripts/perf-gate.sh`'s own per-tier threshold instead of quietly
    /// re-inflating past it.
    projected_peak_mb: f64,
    /// The demo binary this mode runs, and the arguments every run of it
    /// takes before `args`.
    demo: &'static str,
    common: &'static [&'static str],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    Headless,
    Windowed,
}

// Measured max peak RSS per mode, from real gate runs (`gate:
// <mode>.peak_rss_mb` / `.rss_leg_max_mb` in `$LOGS/<mode>-<n>.log`, and
// the ranges recorded in `stress/us_block_groups_baseline.json`'s
// rationale strings), as of 2026-09-27. `projected_peak_mb` below rounds
// each of these up generously:
//   headless        4468.7 MB (range 3480.3-4468.7 across runs)
//   headless_states   537.1 MB
//   points            778.9 MB
//   dots              196.2 MB
//   pan              4648.7 MB
//   zoom_dc          3924.8 MB (ceiling; range 3389.5-3534.4 across runs)
//   zoom_la          3505.9 MB (ceiling; range 3168.7-3202.2 across runs)
//   slider           4641.8 MB
//   live             4647.0 MB
//   dots.pan          551.0 MB
// The headless tier's own worst mode (headless, 4468.7 MB) sits far below
// the windowed tier's (pan/live/slider, ~4649 MB) -- both well short of
// the windowed sweep's original 6 GB assumption -- which is why a single
// blanket memory threshold sized for the windowed sweep over-refused
// every headless-only run (see `scripts/perf-gate.sh`).
const MODES: &[Mode] = &[
    Mode {
        prefix: "headless",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(900),
        projected_peak_mb: 5_000.0,
        demo: "us_block_groups",
        common: COMMON_ARGS,
    },
    // The census demo's DEFAULT shape, one source per state, headless: DC
    // (under the tiling threshold), California and Texas. Graded for its
    // slider rows only (`headless_states.table_only.*`): the national
    // one-source run above cannot see a small state that replans on every
    // drag step, which is how that shipped with the slider and stayed
    // green here until 2026-09-27.
    Mode {
        prefix: "headless_states",
        tier: Tier::Headless,
        args: &["--measure", "--states", "11,06,48"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_500.0,
        demo: "us_block_groups",
        common: &["--basemap", "positron"],
    },
    // The second section: `demos/big_points.rs --measure`, 2 M seeded
    // synthetic points on the point path (no data to fetch).
    Mode {
        prefix: "points",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_000.0,
        demo: "big_points",
        common: &[],
    },
    // The third section: `demos/world_dots.rs --measure --synthetic`, the
    // procedural synthetic world (Task 8; no GHSL archive to fetch -- this
    // is the ONLY source the gate ever uses for the dot layer).
    Mode {
        prefix: "dots",
        tier: Tier::Headless,
        args: &["--measure", "--synthetic"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_500.0,
        demo: "world_dots",
        common: &[],
    },
    Mode {
        prefix: "pan",
        tier: Tier::Windowed,
        args: &["--pan", "--quit-after", "25"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 5_500.0,
        demo: "us_block_groups",
        common: COMMON_ARGS,
    },
    Mode {
        prefix: "zoom_dc",
        tier: Tier::Windowed,
        args: &["--zoom-sweep", "--at", "dc"],
        timeout: Duration::from_secs(420),
        projected_peak_mb: 6_000.0,
        demo: "us_block_groups",
        common: COMMON_ARGS,
    },
    Mode {
        prefix: "zoom_la",
        tier: Tier::Windowed,
        args: &["--zoom-sweep", "--at", "la"],
        timeout: Duration::from_secs(420),
        projected_peak_mb: 6_000.0,
        demo: "us_block_groups",
        common: COMMON_ARGS,
    },
    Mode {
        prefix: "slider",
        tier: Tier::Windowed,
        args: &["--slider-sweep"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 5_500.0,
        demo: "us_block_groups",
        common: COMMON_ARGS,
    },
    // Live incremental updates: 1,000 incomes at 10 Hz by a moving wave,
    // 100 removes and re-adds every 2 s, for 120 s at the national view.
    Mode {
        prefix: "live",
        tier: Tier::Windowed,
        args: &["--live", "wave", "--live-secs", "120"],
        timeout: Duration::from_secs(420),
        projected_peak_mb: 5_500.0,
        demo: "us_block_groups",
        common: COMMON_ARGS,
    },
    // The dot layer's own windowed check: a real, frontmost window panning
    // the synthetic world at India z5. `world_dots --pan`'s frame report
    // (`report_frames` in demos/world_dots.rs) prints no `occluded_paints`
    // line -- unlike the census demo's own windowed modes above -- so this
    // mode (and any other `dots.`-prefixed one) is validated on draw calls
    // alone; see `invalid_reason`.
    Mode {
        prefix: "dots.pan",
        tier: Tier::Windowed,
        args: &["--synthetic", "--at", "india", "--pan", "--quit-after", "25"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 2_000.0,
        demo: "world_dots",
        common: &[],
    },
    // Building extrusion (docs/superpowers/specs/2026-09-29-3d-building-extrusion-design.md §5.4):
    // tessellation of the three vendored z14 building fixtures. No data fetch.
    Mode {
        prefix: "buildings",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_000.0,
        demo: "buildings_gate",
        common: &[],
    },
    // A pitched-60° pan over Shinjuku on Liberty (keyless), windowed and
    // frontmost. Liberty's `building-3d` is 0.8, so this is the translucent
    // path Liberty draws by default: a depth prepass plus a stencilled
    // colour pass, the geometry twice.
    Mode {
        prefix: "buildings.pan",
        tier: Tier::Windowed,
        args: &["--pan", "--quit-after", "25"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 2_000.0,
        demo: "buildings_gate",
        common: &[],
    },
    // The same pan with `building-3d` forced opaque: one pass, no prepass.
    // Graded on its own; against `buildings.pan` it is what translucency
    // costs. `buildings.pan_opaque.` does not start with `buildings.pan.`,
    // so neither section claims the other's keys.
    Mode {
        prefix: "buildings.pan_opaque",
        tier: Tier::Windowed,
        args: &["--pan", "--opaque", "--quit-after", "25"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 2_000.0,
        demo: "buildings_gate",
        common: &[],
    },
    // The fourth section: `demos/satellites.rs --measure --synthetic`, the
    // seeded 10k synthetic catalogue at a fixed scene time (no CelesTrak
    // fetch, no data). Built `--features probe,orbits`.
    Mode {
        prefix: "sats",
        tier: Tier::Headless,
        args: &["--measure", "--synthetic"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_000.0,
        demo: "satellites",
        common: &[],
    },
    // Story content 1a (spec §8.9): `examples/story.rs --gate` on the
    // hermetic determinism story (fixture tiles, no network): a 4K frame's
    // time, and a drawn route's frame time and re-tessellations after its
    // first frame (a draw is a uniform: 0, exact). Writes nothing.
    Mode {
        prefix: "story",
        tier: Tier::Headless,
        args: &[concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stories/det.json"), "--gate"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 2_000.0,
        demo: "story",
        common: &[],
    },
    // CSV sources (`overlay::csv`): `demos/csv_points.rs --measure`, a
    // seeded synthetic million-row CSV parsed, pushed onto the point path
    // and indexed. No network, no data.
    Mode {
        prefix: "csv",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_000.0,
        demo: "csv_points",
        common: &[],
    },
    // The Studio Explorer's filter path (`demos/explorer_gate.rs
    // --measure`): a seeded synthetic million-row quakes CSV typed, pushed,
    // panned, GPU-filtered and CPU-filtered. No network, no data.
    Mode {
        prefix: "explorer",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 2_000.0,
        demo: "explorer_gate",
        common: &[],
    },
    // MBTiles reads (`tile_source::mbtiles`): `demos/mbtiles.rs --measure`,
    // a seeded synthetic file of 1024 gzipped z14 tiles (~0.2 GB, written
    // to the temp dir and deleted) read through `TileSource::fetch`. No
    // network, no data.
    Mode {
        prefix: "mbtiles",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 500.0,
        demo: "mbtiles",
        common: &[],
    },
    // FlatGeobuf (`overlay::fgb`): `demos/fgb.rs --measure`, a seeded
    // synthetic million-point file (written to the temp dir and deleted)
    // read whole, pushed, and queried by bbox through its index. No
    // network, no data.
    Mode {
        prefix: "fgb",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_200.0,
        demo: "fgb",
        common: &[],
    },
    // GeoParquet (`overlay::geoparquet`): `demos/geoparquet.rs --measure`,
    // a seeded synthetic million-point file, Z-order sorted into 31 row
    // groups (written to the temp dir and deleted), read whole, pushed, and
    // read by bbox with row groups skipped by their covering statistics.
    // No network, no data.
    Mode {
        prefix: "geoparquet",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_200.0,
        demo: "geoparquet",
        common: &[],
    },
    // Cloud-Optimised GeoTIFF (`tile_source::cog`): `demos/cog.rs
    // --measure`, a seeded synthetic RGB COG and float32 DEM (written to the
    // temp dir and deleted): the header open, one map tile's decode and
    // resample, the range requests one view costs, and that view over a
    // local HTTP server that delays every response 20 ms. No network.
    Mode {
        prefix: "cog",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 800.0,
        demo: "cog",
        common: &[],
    },
    // Origin-destination arcs (`render::arcs`): `demos/arcs.rs --measure`,
    // 680k seeded synthetic commute arcs and 98k taxi arcs at Kepler's own
    // cameras, drawn additive into a 2560x1600 target: submit-to-idle time
    // per frame, and the vertices and draws the length classes plan. No
    // network, no data.
    Mode {
        prefix: "arcs",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 1_000.0,
        demo: "arcs",
        common: &[],
    },
    // Runtime hexagon binning and overlay extrusion (`overlay::hexbin`,
    // `overlay::extrude`): `demos/hexbin.rs --measure`, Kepler's 65,499 SF
    // street trees (read from the Studio's data cache, fetched there once)
    // into 50 m hexagons, extruded, uploaded, and drawn once at Kepler's
    // camera into a 2560x1600 capture; and the Explorer's re-bin at 1M
    // seeded points: hexagons by count and by median (E10), squares
    // (`overlay::gridbin`, E3).
    Mode {
        prefix: "hexbin",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 800.0,
        demo: "hexbin",
        common: &[],
    },
    // Animated trips (`render::trips`): `demos/trips.rs --measure`, 20k
    // seeded synthetic flights (1.1M vertices) on the globe at the
    // world-flights sample's camera, paths whole plus fading trails: build,
    // the first frame's push, a steady frame, and the CPU of a frame where
    // only the clock moved. No network, no data.
    Mode {
        prefix: "trips",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 800.0,
        demo: "trips",
        common: &[],
    },
    // The vector-tile overlay (`overlay::vector_tiles`): `demos/vtoverlay.rs
    // --measure`, seeded synthetic tiles shaped like Kepler's population
    // (4 z10 tiles, 12k blocks) and FSQ (12 z14 tiles, 72k places) views:
    // decode, push and a frame at each sample's camera. No network, no data.
    Mode {
        prefix: "vtoverlay",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 800.0,
        demo: "vtoverlay",
        common: &[],
    },
    // The night race's in-car view (`render::model`'s first-person near
    // pass, the part tables): `demos/night_race_gate.rs --measure`, the own
    // car and the driver at the in-car camera into a 2560x1600 target: the
    // frame's p50/p95, the near pass's GPU time (timestamp queries), its
    // triangles and part-table bytes. No network, no data.
    Mode {
        prefix: "night_race",
        tier: Tier::Headless,
        args: &["--measure"],
        timeout: Duration::from_secs(300),
        projected_peak_mb: 500.0,
        demo: "night_race_gate",
        common: &[],
    },
    // The space look (`render::space_look`; on by default in the satellites
    // scene): its two full-screen passes' GPU ms/frame at 3456x2234 and
    // 5120x2880, the first-use build with the ~37 MB upload, and the
    // per-frame uniform write. Headless, seeded synthetic textures at the
    // real sizes (no NASA download). Its own prefix; `look.draw_calls` is
    // the proof it painted (the demo exits 2 on zero).
    Mode {
        prefix: "look",
        tier: Tier::Headless,
        args: &["--measure-look"],
        timeout: Duration::from_secs(120),
        projected_peak_mb: 500.0,
        demo: "satellites",
        common: &[],
    },
    // The satellites' windowed check: 10k points and two rings at 1000x,
    // frontmost, for 10 s. Its own prefix (like `dots.pan`), validated on
    // draw calls alone; see `invalid_reason`.
    Mode {
        prefix: "sats.run",
        tier: Tier::Windowed,
        args: &["--synthetic", "--measure-frames", "10", "--rate", "1000"],
        timeout: Duration::from_secs(120),
        projected_peak_mb: 1_500.0,
        demo: "satellites",
        common: &[],
    },
];

/// Every run: the national dataset as ONE tiled source (what the user
/// runs), on the keyless basemap so a token in the environment cannot
/// change what is drawn.
const COMMON_ARGS: &[&str] = &["--all", "--one-source", "--basemap", "positron"];

/// The mode a baseline `key` belongs to: the LONGEST `prefix.` it starts
/// with. Prefixes nest (`dots` and `dots.pan`), so a naive "any mode whose
/// prefix matches" would have `--only dots` claim `dots.pan.*` keys as its
/// own and report them MISSING; the longest match picks the more specific
/// mode instead.
fn owning_mode(key: &str) -> Option<&'static Mode> {
    MODES.iter().filter(|m| key.starts_with(&format!("{}.", m.prefix))).max_by_key(|m| m.prefix.len())
}

/// The projected peak RSS of the mode named `prefix`, in MB.
fn projected_peak_mb(prefix: &str) -> Option<f64> {
    MODES.iter().find(|m| m.prefix == prefix).map(|m| m.projected_peak_mb)
}

/// Metrics a run reports that are VALIDITY evidence or context, never
/// graded: a zero or an occlusion refuses the run instead.
fn is_info(metric: &str) -> bool {
    [".occluded_paints", ".draw_calls", ".draw_calls_total", ".frames", ".window_s", ".updates", ".fps"]
        .iter()
        .any(|s| metric.ends_with(s))
}

/// How `--update-baseline` sets a band from the measured spread.
#[derive(Debug, PartialEq)]
enum Policy {
    /// Deterministic count: two-sided, exact (tolerance 1, floor 1).
    Exact,
    /// Must not rise above its baseline, usually 0 (holes, off-path
    /// updates, unbaked tiles): ceiling = baseline, no floor.
    NoMore,
    /// A noisy count of rare events (frames over 16.7 ms): an absolute
    /// slack of max(3, 2 x the worst run) over the p50.
    Spikes,
    /// Bytes: max(x1.05, 1 + 2 x spread + 0.02).
    Bytes,
    /// Time: max(x1.15, 1 + 2 x spread + 0.10).
    Time,
    /// A fixed absolute ceiling that does NOT scale with the baseline's own
    /// drift: `dots.world_total_err_pct` must stay under this cap no matter
    /// how tight or wide the measured spread is. The `f64` is the cap, in
    /// the metric's own unit.
    AtMost(f64),
    /// A near-deterministic count that must not fall OR rise: a band of
    /// max(x1.15, 1 + 2 x spread + 0.10) above the baseline and the mirror
    /// of it below (floor at least x0.5), with the ceiling never above the
    /// `f64` cap. `buildings.pan*.extrusion_triangles_max`: fewer triangles
    /// is lost buildings (culled tiles, a vanished prepass), more is a
    /// heavier frame, and the spec's §5.2 per-frame budget (1.5M) is the cap.
    CountBand(f64),
    /// A timing with an absolute budget (ruling P16): the [`Policy::Time`]
    /// band, its ceiling never above the `f64` budget, so a slow drift is
    /// caught by the band and a frame past the spec's budget fails even
    /// from a baseline that was already near it. `story.card.*`.
    TimeAtMost(f64),
}

/// Spec §14.3's budget for `story.card.frame_ms_p50`: a 9:16 1080p frame
/// with the design card, its fade, fine print and a themed route.
const STORY_CARD_FRAME_MS_BUDGET: f64 = 4.0;
/// Spec §14.3's budget for `story.card.layout_us_p50`: the frame's chrome
/// layout with a warm shape cache.
const STORY_CARD_LAYOUT_US_BUDGET: f64 = 300.0;

/// Spec §5.2's per-frame extrusion triangle budget, the hard ceiling of
/// every `extrusion_triangles_max` band.
const EXTRUSION_TRIANGLES_PER_FRAME_CAP: f64 = 1_500_000.0;

/// A CPU filter's release at a million rows (spec §11.4).
const EXPLORER_CPU_RELEASE_MS_BUDGET: f64 = 250.0;

fn policy(metric: &str, unit: &str) -> Policy {
    let ends = |s: &str| metric.ends_with(s);
    if ends(".vertices")
        || ends(".features")
        || ends(".instances")
        || ends(".indices")
        // Wall quads a fixture tessellates to (`buildings.tess.*.walls`): a
        // deterministic function of the fixture and the style, like its
        // vertices and indices.
        || ends(".walls")
        || ends(".outline_indices")
        || ends(".wanted_tiles")
        || ends(".hit_hits")
        || ends(".cells")
        || ends(".blocks")
        || ends(".dots")
        || ends("world_total_dots")
        // `csv.rows_*`: what a seeded synthetic CSV parses to.
        || ends(".rows_loaded")
        || ends(".rows_skipped")
        // `mbtiles.tiles_read`: every tile of the seeded file, found.
        || ends(".tiles_read")
        // `fgb.features_loaded`: every feature of the seeded file, read.
        || ends(".features_loaded")
        // `geoparquet.query.fixed.row_groups_read`: the groups a fixed box
        // reaches in the seeded file -- pushdown, counted.
        || ends(".row_groups_read")
        // `cog.view.*.range_requests`, `cog.view.tiles_decoded`,
        // `cog.view.map_tiles`: what one fixed view of the seeded COG reads
        // -- coalescing and the overview choice, counted.
        || ends(".range_requests")
        || ends(".tiles_decoded")
        || ends(".map_tiles")
        // `arcs.*`: the seeded set's arcs, and the draws and vertices its
        // length classes plan at a fixed camera.
        || ends(".arcs")
        || ends(".draws")
        || metric == "arcs.upload_kb"
        // `arcs.uk.pick.checksum`: which arc each cursor of the fixed grid
        // picks, folded -- a deterministic function of the set and camera.
        || ends(".checksum")
        // `hexbin.*`: the fixed data's rows, cells, vertices and upload.
        || ends(".rows")
        || ends(".cells")
        || metric == "hexbin.sftrees.upload_kb"
        // `trips.*`: the seeded set's trips, vertices, upload and draws.
        || metric == "trips.flights.trips"
        || metric == "trips.flights.vertices"
        || metric == "trips.upload_kb"
        || metric == "trips.globe.draws"
        // `night_race.*`: the near pass's meshes and part tables are fixed.
        || metric == "night_race.in_car.triangles"
        || metric == "night_race.in_car.parts_upload_bytes"
        // `vtoverlay.*.features`: the seeded tiles' features after clip.
        || (metric.starts_with("vtoverlay.") && ends(".features"))
    {
        Policy::Exact
    } else if metric.starts_with("sats.dots_px_s.") {
        // The dots' speed bound at a fixed camera and scene: deterministic,
        // and an under-estimate (late frames) matters as much as an over-
        // estimate (wasted ones). Its baseline band is +-5%.
        Policy::Exact
    } else if metric.starts_with("sats.dots_redraw_hz.") {
        // How often moving dots ask for a frame at rate 1: deterministic,
        // and any rise is frames (whole-window redraws) nothing needed.
        Policy::NoMore
    } else if metric.starts_with("sats.visible.") || metric == "sats.upload_kb" {
        // A broken occlusion or near cull changes a count, not a time; the
        // upload is instances x 60 B of a fixed synthetic set.
        Policy::Exact
    } else if metric == "story.card.frame_ms_p50" {
        Policy::TimeAtMost(STORY_CARD_FRAME_MS_BUDGET)
    } else if metric == "story.card.layout_us_p50" {
        Policy::TimeAtMost(STORY_CARD_LAYOUT_US_BUDGET)
    } else if ends(".reshapes_after_first") {
        // `story.card.*`: after the first frame no shaping-cache key that was
        // already shaped is shaped again; the baseline is 0 and so is the
        // ceiling.
        Policy::NoMore
    } else if metric == "explorer.filter_drag.uploads" {
        // A filter window is a uniform: a drag step that re-pushes or
        // rebuilds anything is a regression. The baseline is 0.
        Policy::NoMore
    } else if metric == "explorer.cpu_filter_release_ms_p50" {
        // The spec's bound (§11.4): above it, E-mask is owed.
        Policy::TimeAtMost(EXPLORER_CPU_RELEASE_MS_BUDGET)
    } else if metric == "trips.tick_upload_bytes" {
        // A clock tick is a uniform write: its vertex upload is 0, and any
        // byte above it is a re-push per frame.
        Policy::NoMore
    } else if ends(".retessellations_after_first") {
        // `story.draw.*`: a draw is a trim uniform, so no frame after the
        // first may re-tessellate; the baseline is 0 and so is the ceiling.
        Policy::NoMore
    } else if ends("_holes") || ends("other_path") || ends(".unbaked_with_content") || ends("_violations") || ends(".count_diff") {
        Policy::NoMore
    } else if ends(".extrusion_triangles_max") {
        Policy::CountBand(EXTRUSION_TRIANGLES_PER_FRAME_CAP)
    } else if ends(".frames_over_16_7ms") || ends(".frames_over_33ms") {
        Policy::Spikes
    } else if ends("world_total_err_pct") {
        // The dot layer's own accuracy check (dots issued at ppd 1 vs. the
        // source's own population sum): the spec's absolute bound is 0.1%,
        // not a multiple of whatever the measured spread happens to be.
        Policy::AtMost(0.1)
    } else if unit == "MB" {
        Policy::Bytes
    } else {
        Policy::Time
    }
}

/// The band a metric is graded against: the committed one, except that a
/// [`Policy::TimeAtMost`] row's ceiling never passes its budget, whatever
/// the file says (a hand-edited or older band cannot lift it).
fn graded_bound(metric: &str, unit: &str, mut bound: Bound) -> Bound {
    if let Policy::TimeAtMost(cap) = policy(metric, unit) {
        if bound.ceiling() > cap {
            bound.slack = 0.0;
            if bound.baseline > 0.0 {
                bound.tolerance = cap / bound.baseline;
            } else {
                bound.slack = cap;
            }
        }
    }
    bound
}

/// p50, min and max of one metric's valid runs.
#[derive(Debug, Clone, Copy)]
struct Spread {
    p50: f64,
    min: f64,
    max: f64,
    n: usize,
}

fn spread(values: &[f64]) -> Spread {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    Spread { p50: v[(v.len() - 1) / 2], min: v[0], max: v[v.len() - 1], n: v.len() }
}

/// The narrowest a millisecond timing band may be: see `new_bound`.
const MIN_MS_HEADROOM: f64 = 1.0;

/// The band `--update-baseline` writes for a metric measured as `s`.
fn new_bound(metric: &str, unit: &str, s: Spread, when: &str) -> Bound {
    let rel = if s.p50 > 0.0 { (s.max - s.min) / s.p50 } else { 0.0 };
    let observed = format!("{} run(s) {when}: p50 {:.3}, range {:.3}-{:.3} ({:.1}% spread)", s.n, s.p50, s.min, s.max, rel * 100.0);
    let (mut tolerance, mut floor, mut slack, rule) = match policy(metric, unit) {
        Policy::Exact if s.min == s.max => (1.0, Some(1.0), 0.0, "exact across runs, so graded two-sided and exact"),
        Policy::Exact => (1.05, Some(0.95), 0.0, "NOT exact across runs, so +-5% (investigate: this count should be deterministic)"),
        Policy::NoMore => (1.0, None, 0.0, "may not rise above the baseline"),
        Policy::Spikes => (1.0, None, (2.0 * s.max).max(3.0), "rare-event count: slack max(3, 2 x worst run) over the p50"),
        Policy::Bytes => ((1.0 + 2.0 * rel + 0.02).max(1.05), None, 0.0, "bytes: max(x1.05, 1 + 2 x spread + 0.02)"),
        Policy::Time => ((1.0 + 2.0 * rel + 0.10).max(1.15), None, 0.0, "time: max(x1.15, 1 + 2 x spread + 0.10)"),
        Policy::TimeAtMost(_) => {
            ((1.0 + 2.0 * rel + 0.10).max(1.15), None, 0.0, "time: max(x1.15, 1 + 2 x spread + 0.10), the ceiling capped at the spec budget")
        }
        Policy::CountBand(cap) => {
            let tol = (1.0 + 2.0 * rel + 0.10).max(1.15);
            let floor = (2.0 - tol).max(0.5);
            // The ceiling may not pass the cap: a baseline already over it
            // gets a tolerance under 1, so the very next run fails.
            let tol = if s.p50 > 0.0 { tol.min(cap / s.p50) } else { tol };
            (tol, Some(floor), 0.0, "count, two-sided: max(x1.15, 1 + 2 x spread + 0.10) above, its mirror below, ceiling capped at the spec budget")
        }
        Policy::AtMost(cap) => {
            (1.0, None, cap, "absolute cap: the ceiling is fixed regardless of the baseline's own drift (see the rationale for what was measured)")
        }
    };
    // A zero baseline admits nothing under a multiplier; a timing that was
    // 0 in every run (a sub-resolution figure) keeps a floor of one unit.
    if s.p50 == 0.0 && matches!(policy(metric, unit), Policy::Time | Policy::TimeAtMost(_) | Policy::Bytes) {
        slack = 1.0;
    }
    if s.p50 == 0.0 {
        floor = None;
        tolerance = 1.0;
    }
    let mut baseline = (s.p50 * 1000.0).round() / 1000.0;
    // A millisecond timing band is never narrower than 1 ms: the ceiling is
    // at least `baseline + MIN_MS_HEADROOM`. Below ~7 ms the x1.15 rule is
    // under a millisecond, and a sub-ms figure (the per-state table-only
    // update is ~0.2 ms p50, ~0.5 ms max of 40) moves by more than that on
    // one scheduler hiccup. An absolute floor rather than grading a p90 in
    // place of the max: the max is what a dragging hand feels, and the
    // regression that matters at this scale (an update off the table path)
    // is graded exactly by `other_path`, not by the time.
    let mut rule = rule.to_string();
    if matches!(policy(metric, unit), Policy::Time | Policy::TimeAtMost(_)) && unit == "ms" && baseline > 0.0 {
        // Against the tolerance as it is written (rounded up below).
        let written = (tolerance * 100.0_f64).ceil() / 100.0;
        let short = MIN_MS_HEADROOM - baseline * (written - 1.0);
        if short > 0.0 {
            slack = (short * 1000.0).ceil() / 1000.0;
            rule = format!("{rule}; ceiling at least baseline + {MIN_MS_HEADROOM} ms");
        }
    }
    // The rounding above is 3 decimal places, coarser than a percentage
    // metric this small (world_total_err_pct is ~1e-4): force the recorded
    // baseline to 0 so `ceiling() == cap` exactly, never `baseline + cap`.
    if let Policy::AtMost(_) = policy(metric, unit) {
        baseline = 0.0;
    }
    // Rounded UP to whole percent, except under a cap, where rounding up
    // could lift the ceiling past it.
    let mut tolerance = match policy(metric, unit) {
        Policy::CountBand(cap) if baseline * (tolerance * 100.0_f64).ceil() / 100.0 > cap => (tolerance * 100.0_f64).floor() / 100.0,
        _ => (tolerance * 100.0_f64).ceil() / 100.0,
    };
    // A budgeted timing: the band as written, then cut to the budget -- the
    // slack first, then the tolerance (rounded DOWN), so `ceiling() <= cap`.
    // A baseline already over the budget gets a tolerance under 1: the very
    // next run fails.
    if let Policy::TimeAtMost(cap) = policy(metric, unit) {
        if baseline * tolerance + slack > cap {
            slack = (cap - baseline * tolerance).max(0.0);
            slack = (slack * 1000.0).floor() / 1000.0;
            if baseline * tolerance > cap && baseline > 0.0 {
                tolerance = ((cap / baseline) * 100.0_f64).floor() / 100.0;
            }
            rule = format!("{rule}; cut to the {cap} {unit} budget");
        }
    }
    Bound {
        baseline,
        tolerance,
        unit: unit.to_string(),
        rationale: format!("{observed}. Rule: {rule}."),
        floor,
        slack,
    }
}

/// `gate: NAME VALUE UNIT` lines under `prefix.`, in print order.
fn parse_gate_lines(out: &str, prefix: &str) -> BTreeMap<String, (f64, String)> {
    let mut m = BTreeMap::new();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix("gate: ") else { continue };
        let mut it = rest.split_whitespace();
        let (Some(name), Some(value), Some(unit)) = (it.next(), it.next(), it.next()) else { continue };
        if !name.starts_with(&format!("{prefix}.")) {
            continue;
        }
        // Prefixes nest (`dots` / `dots.pan`, `buildings` / `buildings.pan`):
        // a line a more specific mode owns is that mode's, never this one's.
        if owning_mode(name).is_some_and(|m| m.prefix != prefix) {
            continue;
        }
        if let Ok(v) = value.parse::<f64>() {
            m.insert(name.to_string(), (v, unit.to_string()));
        }
    }
    m
}

/// Why a windowed run cannot be scored, or `None` when it can.
fn invalid_reason(prefix: &str, m: &BTreeMap<String, (f64, String)>, load_before: f64, max_load: f64, locked: bool) -> Option<String> {
    if locked {
        return Some("the screen was locked (nothing paints)".into());
    }
    if load_before > max_load {
        return Some(format!("load average {load_before:.2} above the {max_load:.1} threshold"));
    }
    let get = |k: &str| m.get(&format!("{prefix}.{k}")).map(|v| v.0);
    // Every `dots.`-prefixed windowed mode shares `demos/world_dots.rs`'s
    // `report_frames`, which prints no `occluded_paints` line at all (unlike
    // the census demo's own windowed reporting): gated on draw calls alone.
    // So is `sats.run` (`demos/satellites.rs --measure-frames`).
    if !prefix.starts_with("dots.") && !prefix.starts_with("sats.") {
        match get("occluded_paints") {
            None => return Some("no occluded_paints line: the run never reached its measurement".into()),
            Some(n) if n > 0.0 => return Some(format!("{n} occluded paints: the window was not frontmost")),
            _ => {}
        }
    }
    // `buildings_gate` names its count `draw_calls_total`.
    match get("draw_calls").or_else(|| get("draw_calls_total")) {
        None | Some(0.0) => return Some("0 draw calls: nothing painted".into()),
        _ => {}
    }
    None
}

fn load_1m() -> f64 {
    let out = Command::new("sysctl").args(["-n", "vm.loadavg"]).output().ok();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    text.trim().trim_matches(|c| c == '{' || c == '}').split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0.0)
}

/// `Some(true)` when the console session's screen is locked; `None` when
/// it cannot be told (no python3 or no Quartz bridge), which is reported
/// but does not refuse the run -- a locked screen also shows up as 0 draws.
fn screen_locked() -> Option<bool> {
    let out = Command::new("python3")
        .args(["-c", "import Quartz; d=Quartz.CGSessionCopyCurrentDictionary() or {}; print(int(bool(d.get('CGSSessionScreenIsLocked',0))))"])
        .output()
        .ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Free plus inactive memory, in MB, from `vm_stat`.
fn free_plus_inactive_mb() -> f64 {
    let out = Command::new("vm_stat").output().ok();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let mut page = 16384.0;
    let mut pages = 0.0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Mach Virtual Memory Statistics: (page size of ") {
            page = rest.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(page);
        }
        if line.starts_with("Pages free:") || line.starts_with("Pages inactive:") {
            pages += line.rsplit(' ').next().unwrap_or("0").trim_end_matches('.').parse::<f64>().unwrap_or(0.0);
        }
    }
    pages * page / 1e6
}

/// Available MB on the volume holding `path`, from `df -m`.
fn disk_free_mb(path: &Path) -> f64 {
    let out = Command::new("df").arg("-m").arg(path).output().ok();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    text.lines().nth(1).and_then(|l| l.split_whitespace().nth(3)).and_then(|s| s.parse().ok()).unwrap_or(0.0)
}

struct Run {
    metrics: BTreeMap<String, (f64, String)>,
    load_before: f64,
    load_after: f64,
    quiet: bool,
    invalid: Option<String>,
    secs: f64,
}

/// Runs the demo once in `mode`, bounded by the mode's timeout. Kills only
/// the child it started.
fn run_once(bin: &Path, mode: &Mode, log_dir: &Path, rep: usize, max_load: f64) -> Run {
    let locked_before = screen_locked();
    let load_before = load_1m();
    let window = stress::EnvWindow::open();
    let started = Instant::now();
    let log = log_dir.join(format!("{}-{rep}.log", mode.prefix));
    let mut child = Command::new(bin.with_file_name(mode.demo))
        .args(mode.common)
        .args(mode.args)
        .env("US_BG_GATE", "1")
        // Measure the network path, never a warm disk cache
        // (`kit::with_tile_cache`), whatever the caller's environment says.
        .env("GPUI_MAP_TILE_CACHE", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the demo binary");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Some(status);
        }
        if started.elapsed() > mode.timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let out = reader.join().unwrap_or_default();
    let _ = std::fs::write(&log, &out);
    let env = window.close();
    let load_after = load_1m();
    let metrics = parse_gate_lines(&out, mode.prefix);
    let locked = locked_before == Some(true) || screen_locked() == Some(true);
    let invalid = match status {
        None => Some(format!("timed out after {} s (log {})", mode.timeout.as_secs(), log.display())),
        Some(s) if !s.success() => Some(format!("exited {s} (log {})", log.display())),
        _ if metrics.is_empty() => Some(format!("printed no gate lines (log {})", log.display())),
        _ if mode.tier == Tier::Windowed => invalid_reason(mode.prefix, &metrics, load_before, max_load, locked),
        _ => None,
    };
    Run { metrics, load_before, load_after, quiet: env.quiet(), invalid, secs: started.elapsed().as_secs_f64() }
}

struct Args {
    tier: String,
    update: bool,
    reps: usize,
    bin: PathBuf,
    baseline: PathBuf,
    max_load: f64,
    log_dir: PathBuf,
    /// `--only PREFIX`: run (and baseline) only that mode.
    only: Option<String>,
}

fn parse_args() -> Args {
    let exe = std::env::current_exe().expect("current exe");
    let mut a = Args {
        tier: "headless".into(),
        update: false,
        reps: 0,
        bin: exe.with_file_name("us_block_groups"),
        baseline: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/stress/us_block_groups_baseline.json")),
        max_load: std::thread::available_parallelism().map_or(6.0, |n| n.get() as f64 * 0.5),
        log_dir: std::env::temp_dir().join("perf-gate"),
        only: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "headless" | "windowed" | "all" => a.tier = arg,
            "--update-baseline" => a.update = true,
            "--reps" => a.reps = it.next().and_then(|s| s.parse().ok()).expect("--reps takes a count"),
            "--bin" => a.bin = it.next().expect("--bin takes a path").into(),
            "--baseline" => a.baseline = it.next().expect("--baseline takes a path").into(),
            "--max-load" => a.max_load = it.next().and_then(|s| s.parse().ok()).expect("--max-load takes a number"),
            "--log-dir" => a.log_dir = it.next().expect("--log-dir takes a path").into(),
            "--only" => a.only = Some(it.next().expect("--only takes a mode prefix")),
            // For `scripts/perf-gate.sh`'s memory precheck on an `--only`
            // run: that section's own projected peak, from `MODES` (the one
            // place the number lives), then exit.
            "--projected-peak" => {
                let prefix = it.next().expect("--projected-peak takes a mode prefix");
                match projected_peak_mb(&prefix) {
                    Some(mb) => {
                        println!("{mb:.0}");
                        std::process::exit(0);
                    }
                    None => {
                        eprintln!("no mode {prefix:?}; see MODES in examples/perf_gate.rs");
                        std::process::exit(2);
                    }
                }
            }
            other => panic!("unknown argument {other}; see docs/perf-gate.md"),
        }
    }
    if a.reps == 0 {
        a.reps = if a.update { 3 } else { 1 };
    }
    a
}

fn fmt(v: f64) -> String {
    if v.abs() >= 1000.0 || v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

fn main() {
    let args = parse_args();
    std::fs::create_dir_all(&args.log_dir).expect("create the log dir");
    let tiers: Vec<Tier> = match args.tier.as_str() {
        "headless" => vec![Tier::Headless],
        "windowed" => vec![Tier::Windowed],
        _ => vec![Tier::Headless, Tier::Windowed],
    };
    let old = Baseline::load(&args.baseline.to_string_lossy());
    println!("perf gate: tier {} | reps {} | baseline {}", args.tier, args.reps, args.baseline.display());
    if !old.taken_on.is_empty() {
        println!("baseline taken on: {}", old.taken_on);
    }

    // Every valid run's value, per metric; and the run log.
    let mut values: BTreeMap<String, (Vec<f64>, String)> = BTreeMap::new();
    let mut advisory = false;
    let mut scored_modes = 0;
    let mut unscored_modes = 0;
    let mut tier_secs: BTreeMap<&str, f64> = BTreeMap::new();
    let mut loads: Vec<(f64, f64)> = Vec::new();
    let selected = |m: &&Mode| tiers.contains(&m.tier) && args.only.as_deref().is_none_or(|p| p == m.prefix);
    for mode in MODES.iter().filter(selected) {
        let mut valid = 0;
        for rep in 0..args.reps {
            // Safety, before every national run: memory and disk. The
            // baseline's `ceiling()` is capped at `projected_peak_mb`: that
            // ceiling scales the measured peak by a regression-grading
            // tolerance sized for run-to-run noise, not memory headroom,
            // and for `headless` it overshoots (6067 MB) the mode-aware,
            // measured-and-rounded figure (5000 MB) that
            // `scripts/perf-gate.sh`'s own preflight uses -- uncapped, the
            // two would disagree and the script could let a build through
            // that this recheck then refused anyway.
            let peak = old
                .bound(&format!("{}.peak_rss_mb", mode.prefix))
                .or_else(|| old.bound(&format!("{}.rss_leg_max_mb", mode.prefix)))
                .map_or(mode.projected_peak_mb, |b| b.ceiling())
                .min(mode.projected_peak_mb);
            let free = free_plus_inactive_mb();
            if free < peak + 8_000.0 {
                println!("REFUSED: free+inactive {free:.0} MB < projected peak {peak:.0} MB + 8000 MB; close something and rerun");
                std::process::exit(3);
            }
            let disk = disk_free_mb(&args.log_dir);
            if disk < 3_000.0 {
                println!("REFUSED: {disk:.0} MB free on the log volume, under 3 GB");
                std::process::exit(3);
            }
            let run = run_once(&args.bin, mode, &args.log_dir, rep, args.max_load);
            *tier_secs.entry(if mode.tier == Tier::Headless { "headless" } else { "windowed" }).or_default() += run.secs;
            loads.push((run.load_before, run.load_after));
            let head = format!(
                "  run {:<8} #{rep}: {:>5.1} s | load before {:.2} after {:.2}",
                mode.prefix, run.secs, run.load_before, run.load_after
            );
            if let Some(why) = &run.invalid {
                println!("{head} | INVALID, not scored: {why}");
                continue;
            }
            println!("{head} | valid{}", if run.quiet { "" } else { " (machine busy: failures advisory)" });
            advisory |= mode.tier == Tier::Headless && !run.quiet;
            valid += 1;
            for (k, (v, unit)) in run.metrics {
                let e = values.entry(k).or_insert_with(|| (Vec::new(), unit));
                e.0.push(v);
            }
        }
        if valid > 0 {
            scored_modes += 1;
        } else {
            unscored_modes += 1;
            println!("  {}: NO VALID RUN -- nothing from this mode is scored", mode.prefix);
        }
    }

    if args.update {
        std::process::exit(update_baseline(&args, &old, &values, &loads));
    }

    // Grade: the p50 of the valid runs, against the committed band.
    println!("\n{:<48} {:>12} {:>12} {:>8} {:>12}  result", "metric", "observed", "baseline", "delta", "allowed");
    let (mut fails, mut passes, mut slow) = (0, 0, 0);
    let mut ungraded = Vec::new();
    for (metric, (vals, unit)) in &values {
        let observed = spread(vals).p50;
        if is_info(metric) {
            println!("{metric:<48} {:>12} {:>12} {:>8} {:>12}  info", fmt(observed), "-", "-", "-");
            continue;
        }
        let Some(bound) = old.bound(metric).cloned().map(|b| graded_bound(metric, unit, b)) else {
            ungraded.push(metric.clone());
            continue;
        };
        let verdict = stress::grade(metric.clone(), observed, bound.clone(), !advisory);
        let delta = if bound.baseline != 0.0 {
            format!("{:+.1}%", (observed / bound.baseline - 1.0) * 100.0)
        } else {
            format!("{:+}", fmt(observed))
        };
        let allowed = match bound.floor {
            Some(f) if f == bound.tolerance => format!("={}", fmt(bound.baseline)),
            Some(f) => format!("{}-{}", fmt(bound.baseline * f), fmt(bound.ceiling())),
            None => format!("<={}", fmt(bound.ceiling())),
        };
        let state = match (verdict.pass, verdict.advisory) {
            (true, _) => {
                passes += 1;
                "PASS"
            }
            (false, true) => {
                slow += 1;
                "SLOW (advisory: machine busy)"
            }
            (false, false) => {
                fails += 1;
                "FAIL"
            }
        };
        println!("{metric:<48} {:>12} {:>12} {delta:>8} {allowed:>12}  {state} {unit}", fmt(observed), fmt(bound.baseline));
        if !verdict.pass {
            println!("    band: {}", bound.rationale);
        }
    }
    for metric in &ungraded {
        println!("{metric:<48} (no baseline entry: NOT graded)");
    }
    let expected: Vec<&String> = old
        .metrics
        .keys()
        .filter(|k| owning_mode(k).is_some_and(|m| selected(&m)))
        .filter(|k| !values.contains_key(*k))
        .collect();
    for metric in &expected {
        println!("{metric:<48} MISSING: in the baseline, not reported by any valid run");
    }
    println!(
        "\n{} graded: {passes} PASS, {fails} FAIL, {slow} advisory | {} ungraded | {} missing | runtime {}",
        passes + fails + slow,
        ungraded.len(),
        expected.len(),
        tier_secs.iter().map(|(t, s)| format!("{t} {s:.0} s")).collect::<Vec<_>>().join(", ")
    );
    println!("load average (1 m) before/after each run: {}", loads.iter().map(|(a, b)| format!("{a:.2}/{b:.2}")).collect::<Vec<_>>().join(" "));
    println!("logs: {}", args.log_dir.display());
    let (verdict, code) = gate_outcome(fails, passes, scored_modes, unscored_modes, expected.len());
    println!("GATE: {verdict}");
    std::process::exit(code);
}

/// The gate's verdict line and exit code, in priority order:
/// - `FAIL` (1): a graded row is out of its band;
/// - `NOT SCORED` (2): no mode had a valid run, or nothing matched the
///   baseline;
/// - `INCOMPLETE` (5): everything graded passed, but a baseline row was
///   reported by no valid run (MISSING), or a mode had no valid run. A row
///   nobody measured is not a row that passed -- 5239ac7's
///   `headless_states.table_only.*` went MISSING in a gate that said PASS;
/// - `PASS` (0).
fn gate_outcome(
    fails: usize,
    passes: usize,
    scored_modes: usize,
    unscored_modes: usize,
    missing: usize,
) -> (&'static str, i32) {
    if fails > 0 {
        ("FAIL", 1)
    } else if scored_modes == 0 || passes == 0 {
        ("NOT SCORED (no valid run, or no baseline entries matched)", 2)
    } else if missing > 0 || unscored_modes > 0 {
        ("INCOMPLETE (a baseline row was MISSING, or a mode had no valid run)", 5)
    } else {
        ("PASS", 0)
    }
}

/// Writes the baseline from this run's measured spread, printing a diff of
/// every band against the committed one. Graded metrics only.
fn update_baseline(args: &Args, old: &Baseline, values: &BTreeMap<String, (Vec<f64>, String)>, loads: &[(f64, f64)]) -> i32 {
    if args.reps < 3 {
        println!("--update-baseline wants at least 3 runs per mode for a spread; got {}", args.reps);
        return 2;
    }
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let date = Command::new("date").arg("+%Y-%m-%d").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let (lo, hi) = loads.iter().fold((f64::MAX, 0.0f64), |(lo, hi), (a, b)| (lo.min(*a).min(*b), hi.max(*a).max(*b)));
    let when = format!("on {commit}");
    let mut new = old.clone();
    let tiers_run: Vec<&str> = MODES
        .iter()
        .filter(|m| match args.tier.as_str() {
            "headless" => m.tier == Tier::Headless,
            "windowed" => m.tier == Tier::Windowed,
            _ => true,
        })
        .filter(|m| args.only.as_deref().is_none_or(|p| p == m.prefix))
        .map(|m| m.prefix)
        .collect();
    let taken = format!(
        "{date}, {commit}, {} reps per mode ({}); load 1m {lo:.2}-{hi:.2}; {} cpus.",
        args.reps,
        tiers_run.join(", "),
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    // One section re-baselined keeps the others' record: appended, not
    // replaced.
    new.taken_on = match &args.only {
        Some(prefix) if !old.taken_on.is_empty() => {
            let kept: Vec<&str> =
                old.taken_on.split(" | ").filter(|part| !part.starts_with(&format!("{prefix} section: "))).collect();
            format!("{} | {prefix} section: {taken}", kept.join(" | "))
        }
        _ => taken,
    };
    if new.notes.is_empty() {
        new.notes = "Written by scripts/perf-gate.sh --update-baseline from the measured spread; every rationale records its runs and its rule. Graded value is the p50 of the valid runs. See docs/perf-gate.md.".into();
    }
    println!("\n{:<48} {:>22} -> {:<22}", "metric", "old (baseline x tol)", "new");
    for (metric, (vals, unit)) in values {
        if is_info(metric) {
            continue;
        }
        let bound = new_bound(metric, unit, spread(vals), &when);
        let show = |b: &Bound| format!("{} x{:.2}{}", fmt(b.baseline), b.tolerance, if b.slack > 0.0 { format!(" +{}", fmt(b.slack)) } else { String::new() });
        let before = old.bound(metric).map_or("(new)".to_string(), show);
        println!("{metric:<48} {before:>22} -> {}", show(&bound));
        new.metrics.insert(metric.clone(), bound);
    }
    let text = serde_json::to_string_pretty(&new).expect("serialise the baseline") + "\n";
    std::fs::write(&args.baseline, text).expect("write the baseline");
    println!("\nwrote {} -- review `git diff` on it and commit it on purpose.", args.baseline.display());
    0
}

#[cfg(test)]
mod outcome_tests {
    use super::gate_outcome;

    #[test]
    fn every_row_graded_and_passing_is_a_pass() {
        assert_eq!(gate_outcome(0, 91, 4, 0, 0), ("PASS", 0));
    }

    #[test]
    fn a_missing_row_is_incomplete_not_a_pass() {
        assert_eq!(gate_outcome(0, 88, 3, 0, 3).1, 5);
        assert!(gate_outcome(0, 88, 3, 0, 3).0.starts_with("INCOMPLETE"));
    }

    #[test]
    fn a_mode_with_no_valid_run_is_incomplete() {
        assert_eq!(gate_outcome(0, 88, 3, 1, 0).1, 5);
    }

    #[test]
    fn a_failing_row_wins_over_incomplete() {
        assert_eq!(gate_outcome(1, 87, 3, 1, 3), ("FAIL", 1));
    }

    #[test]
    fn nothing_scored_stays_not_scored() {
        assert_eq!(gate_outcome(0, 0, 0, 4, 91).1, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_modes_gate_lines_are_read() {
        let out = "noise\ngate: pan.fps 48.9 fps\ngate: headless.property_mb 1.9 MB\ngate: pan.cpu_p99_ms 4.8 ms\ngate: pan.bad x ms\n";
        let m = parse_gate_lines(out, "pan");
        assert_eq!(m.len(), 2);
        assert_eq!(m["pan.cpu_p99_ms"].0, 4.8);
    }

    #[test]
    fn a_run_that_did_not_paint_or_was_covered_is_refused() {
        let mut m = BTreeMap::new();
        m.insert("pan.occluded_paints".to_string(), (0.0, "paints".into()));
        m.insert("pan.draw_calls".to_string(), (5000.0, "draws".into()));
        assert_eq!(invalid_reason("pan", &m, 2.0, 6.0, false), None);
        assert!(invalid_reason("pan", &m, 7.0, 6.0, false).unwrap().contains("load"));
        assert!(invalid_reason("pan", &m, 2.0, 6.0, true).unwrap().contains("locked"));
        m.insert("pan.occluded_paints".to_string(), (3.0, "paints".into()));
        assert!(invalid_reason("pan", &m, 2.0, 6.0, false).unwrap().contains("frontmost"));
        m.insert("pan.occluded_paints".to_string(), (0.0, "paints".into()));
        m.insert("pan.draw_calls".to_string(), (0.0, "draws".into()));
        assert!(invalid_reason("pan", &m, 2.0, 6.0, false).unwrap().contains("0 draw calls"));
    }

    #[test]
    fn bands_come_from_the_spread_and_never_go_under_the_floor_for_their_kind() {
        let s = spread(&[100.0, 102.0, 101.0]);
        let t = new_bound("headless.push_ms", "ms", s, "t");
        assert_eq!((t.baseline, t.tolerance), (101.0, 1.15), "2% spread stays at the time floor");
        let wide = new_bound("pan.cpu_p99_ms", "ms", spread(&[4.0, 5.0, 6.0]), "t");
        assert_eq!(wide.tolerance, 1.9, "1 + 2 x 40% + 0.10");
        let b = new_bound("headless.index_mb", "MB", spread(&[662.6, 662.6, 662.6]), "t");
        assert_eq!(b.tolerance, 1.05);
        let v = new_bound("headless.cover.dc_z8.vertices", "count", spread(&[358259.0; 3]), "t");
        assert_eq!((v.tolerance, v.floor), (1.0, Some(1.0)));
        let h = new_bound("zoom_dc.overlay_holes", "tile-frames", spread(&[0.0; 3]), "t");
        assert_eq!((h.baseline, h.tolerance, h.slack, h.floor), (0.0, 1.0, 0.0, None));
        let sp = new_bound("zoom_dc.frames_over_16_7ms", "frames", spread(&[0.0, 1.0, 0.0]), "t");
        assert_eq!(sp.ceiling(), 3.0);
        // A sub-ms timing: the ceiling is at least baseline + 1 ms.
        let sub = new_bound("headless_states.table_only.max_ms", "ms", spread(&[0.49, 0.50, 0.54]), "t");
        assert!((sub.ceiling() - 1.5).abs() < 1e-9, "ceiling {}", sub.ceiling());
        assert!(sub.rationale.contains("baseline + 1 ms"));
        // A band already wider than 1 ms is untouched.
        assert_eq!(t.slack, 0.0);
        // Only milliseconds.
        let bytes = new_bound("points.view.city_z8.upload_mb", "MB", spread(&[0.03; 3]), "t");
        assert_eq!(bytes.slack, 0.0);
    }

    #[test]
    fn validity_evidence_is_never_graded() {
        assert!(is_info("zoom_dc.occluded_paints"));
        assert!(is_info("pan.draw_calls"));
        assert!(!is_info("pan.frame_interval_ms"));
        assert!(!is_info("pan.frames_over_16_7ms"));
    }

    /// Task 9: the `dots` section is registered (a headless mode over the
    /// synthetic world, a windowed `dots.pan` mode), its `gate:` lines
    /// parse under the `dots` prefix, and its two tightly-graded bands --
    /// nesting violations (must stay 0) and the world total error (an
    /// absolute cap, not a multiple of the tiny measured baseline) -- both
    /// fail a run that regresses.
    #[test]
    fn the_dots_section_is_registered_and_its_bands_parse() {
        assert!(
            MODES.iter().any(|m| m.prefix == "dots" && m.tier == Tier::Headless && m.demo == "world_dots"),
            "a headless dots mode over world_dots must exist"
        );
        assert!(
            MODES.iter().any(|m| m.prefix == "dots.pan" && m.tier == Tier::Windowed && m.demo == "world_dots"),
            "a windowed dots.pan mode over world_dots must exist"
        );

        let out = "gate: dots.nesting_violations 1 count\ngate: dots.world_total_err_pct 0.2 pct\ngate: dots.view.world_z1.cells 4 count\n";
        let m = parse_gate_lines(out, "dots");
        assert_eq!(m["dots.nesting_violations"], (1.0, "count".to_string()));
        assert_eq!(m["dots.world_total_err_pct"], (0.2, "pct".to_string()));

        // DP-1: relocation on vs off differs in no count; any difference fails.
        let m = parse_gate_lines("gate: dots.relocate.dhaka_z15.count_diff 0 count\n", "dots");
        assert_eq!(m["dots.relocate.dhaka_z15.count_diff"], (0.0, "count".to_string()));
        assert_eq!(policy("dots.relocate.dhaka_z15.count_diff", "count"), Policy::NoMore);
        let diff = new_bound("dots.relocate.dhaka_z15.count_diff", "count", spread(&[0.0, 0.0, 0.0]), "t");
        assert!(stress::grade("dots.relocate.dhaka_z15.count_diff".to_string(), 0.0, diff.clone(), true).pass);
        assert!(!stress::grade("dots.relocate.dhaka_z15.count_diff".to_string(), 1.0, diff, true).pass);

        // nesting_violations: three quiet (0-violation) runs baseline it at
        // exactly 0 with no floor -- an observed 1 must FAIL.
        let nv = new_bound("dots.nesting_violations", "count", spread(&[0.0, 0.0, 0.0]), "t");
        assert_eq!((nv.baseline, nv.tolerance, nv.floor), (0.0, 1.0, None));
        assert!(!stress::grade("dots.nesting_violations".to_string(), 1.0, nv, true).pass);

        // Task 8's five DP-1 views (`RELOCATE_PRESETS` in world_dots.rs;
        // the CLI's `--at dhaka15` maps to the preset named `dhaka_z15`,
        // which is what the gate line actually prints) all parse under
        // `dots` and grade NoMore, not just the one checked above.
        for name in ["world_z1", "india_z5", "dhaka_z10", "dhaka_z14", "dhaka_z15"] {
            let key = format!("dots.relocate.{name}.count_diff");
            assert_eq!(policy(&key, "count"), Policy::NoMore, "{key}");
            let m = owning_mode(&key).expect("some mode owns it");
            assert_eq!(m.prefix, "dots", "{key} must be owned by the headless dots mode");
        }

        // Task 11: the mask/relocation CPU pass time (no real GPU mask
        // pass headless; `demos/world_dots.rs`'s `measure_relocation_counts`
        // times `MaskGrid::plan` + the synthetic texel fill + the
        // relocating branch of `alive_digest`, and gates its median as
        // `dots.relocate.cpu_mirror_ms`) is an ordinary Time band, not
        // swept into NoMore just for sharing the `dots.relocate.` prefix
        // with the count_diff rows above.
        let m = parse_gate_lines("gate: dots.relocate.cpu_mirror_ms 0.4 ms\n", "dots");
        assert_eq!(m["dots.relocate.cpu_mirror_ms"], (0.4, "ms".to_string()));
        assert_eq!(policy("dots.relocate.cpu_mirror_ms", "ms"), Policy::Time);
        let mp = new_bound("dots.relocate.cpu_mirror_ms", "ms", spread(&[0.38, 0.4, 0.42]), "t");
        assert_eq!(mp.baseline, 0.4);
        assert!(stress::grade("dots.relocate.cpu_mirror_ms".to_string(), 0.4, mp.clone(), true).pass);
        assert!(!stress::grade("dots.relocate.cpu_mirror_ms".to_string(), mp.ceiling() + 0.01, mp, true).pass);

        // world_total_err_pct: the baseline's own measured spread is tiny
        // (~0.00008 %), but the band is an absolute cap of 0.1 %, not a
        // multiple of that spread -- 0.2 must FAIL even though it would
        // pass any multiplicative Time/Bytes-style band.
        let err = new_bound("dots.world_total_err_pct", "pct", spread(&[0.00008, 0.00008, 0.00008]), "t");
        assert!(err.ceiling() <= 0.1 + 1e-9, "ceiling {} must not exceed the absolute cap", err.ceiling());
        assert!(!stress::grade("dots.world_total_err_pct".to_string(), 0.2, err, true).pass);

        // dots.view.*.{cells,blocks,dots} are deterministic (a procedural
        // world, no randomness across runs): exact, both sides, like
        // points.view.*.{features,instances}.
        let cells = new_bound("dots.view.world_z1.cells", "count", spread(&[183_872.0; 3]), "t");
        assert_eq!((cells.tolerance, cells.floor), (1.0, Some(1.0)));
    }

    /// Every `dots.`-prefixed windowed mode shares `world_dots`'
    /// `report_frames`, which never prints an `occluded_paints` line (the
    /// census demo's own windowed modes do): validated on draw calls alone.
    #[test]
    fn dots_prefixed_modes_are_gated_on_draw_calls_only() {
        let mut m = BTreeMap::new();
        m.insert("dots.pan.draw_calls".to_string(), (5000.0, "draws".into()));
        assert_eq!(invalid_reason("dots.pan", &m, 2.0, 6.0, false), None, "no occluded_paints line is required");
        m.insert("dots.pan.draw_calls".to_string(), (0.0, "draws".into()));
        assert!(invalid_reason("dots.pan", &m, 2.0, 6.0, false).unwrap().contains("0 draw calls"));
    }

    /// Task 22: the `story` section is headless, owns only `story.` lines,
    /// grades the re-tessellation row as may-not-rise and times as time.
    #[test]
    fn the_mbtiles_section_is_registered_and_its_reads_are_timed() {
        let mode = MODES.iter().find(|m| m.prefix == "mbtiles").expect("the mbtiles section");
        assert_eq!(mode.demo, "mbtiles");
        assert!(matches!(mode.tier, Tier::Headless));
        let m = parse_gate_lines("gate: mbtiles.fetch_p50_us 362 us\ngate: mbtiles.tiles_read 1024 count\ngate: csv.parse_ms 1 ms\n", "mbtiles");
        assert_eq!(m.len(), 2);
        assert_eq!(policy("mbtiles.tiles_read", "count"), Policy::Exact);
        assert_eq!(policy("mbtiles.fetch_p95_us", "us"), Policy::Time);
        assert_eq!(policy("mbtiles.open_ms", "ms"), Policy::Time);
    }

    #[test]
    fn the_geoparquet_section_is_registered_and_its_counts_are_exact() {
        let mode = MODES.iter().find(|m| m.prefix == "geoparquet").expect("the geoparquet section");
        assert_eq!(mode.demo, "geoparquet");
        assert!(matches!(mode.tier, Tier::Headless));
        assert_eq!(policy("geoparquet.rows_loaded", "count"), Policy::Exact);
        assert_eq!(policy("geoparquet.query.fixed.rows_loaded", "count"), Policy::Exact);
        assert_eq!(policy("geoparquet.query.fixed.row_groups_read", "count"), Policy::Exact);
        assert_eq!(policy("geoparquet.query.city.p50_us", "us"), Policy::Time);
        assert_eq!(policy("geoparquet.load_ms", "ms"), Policy::Time);
    }

    #[test]
    fn the_arcs_section_is_registered_and_its_counts_are_exact() {
        let mode = MODES.iter().find(|m| m.prefix == "arcs").expect("the arcs section");
        assert_eq!(mode.demo, "arcs");
        assert!(matches!(mode.tier, Tier::Headless));
        assert_eq!(policy("arcs.uk.arcs", "count"), Policy::Exact);
        assert_eq!(policy("arcs.uk.vertices", "count"), Policy::Exact);
        assert_eq!(policy("arcs.uk.draws", "count"), Policy::Exact);
        assert_eq!(policy("arcs.upload_kb", "KB"), Policy::Exact);
        assert_eq!(policy("arcs.uk.frame_ms", "ms"), Policy::Time);
        assert_eq!(policy("arcs.uk.pick.checksum", "count"), Policy::Exact);
    }

    #[test]
    fn the_trips_section_is_registered_and_a_tick_uploads_nothing() {
        let mode = MODES.iter().find(|m| m.prefix == "trips").expect("the trips section");
        assert_eq!(mode.demo, "trips");
        for exact in ["trips.flights.trips", "trips.flights.vertices", "trips.upload_kb", "trips.globe.draws"] {
            assert_eq!(policy(exact, "count"), Policy::Exact, "{exact}");
        }
        assert_eq!(policy("trips.tick_upload_bytes", "count"), Policy::NoMore);
        for time in ["trips.build_ms", "trips.push_ms", "trips.globe.frame_ms"] {
            assert_eq!(policy(time, "ms"), Policy::Time, "{time}");
        }
        assert_eq!(policy("trips.tick_cpu_us", "us"), Policy::Time);
    }

    #[test]
    fn the_hexbin_section_is_registered_and_its_counts_are_exact() {
        let mode = MODES.iter().find(|m| m.prefix == "hexbin").expect("the hexbin section");
        assert_eq!(mode.demo, "hexbin");
        assert!(matches!(mode.tier, Tier::Headless));
        for exact in ["hexbin.sftrees.rows", "hexbin.sftrees.cells", "hexbin.sftrees.vertices"] {
            assert_eq!(policy(exact, "count"), Policy::Exact, "{exact}");
        }
        assert_eq!(policy("hexbin.sftrees.upload_kb", "KB"), Policy::Exact);
        for exact in ["hexbin.hex1m.cells", "hexbin.grid1m.cells"] {
            assert_eq!(policy(exact, "count"), Policy::Exact, "{exact}");
        }
        for time in ["hexbin.sftrees.bin_ms", "hexbin.sftrees.extrude_ms", "hexbin.sftrees.push_ms", "hexbin.sftrees.frame_ms", "hexbin.hex1m.bin_ms", "hexbin.hex1m.median_ms", "hexbin.grid1m.bin_ms"] {
            assert_eq!(policy(time, "ms"), Policy::Time, "{time}");
        }
    }

    #[test]
    fn the_cog_section_is_registered_and_its_counts_are_exact() {
        let mode = MODES.iter().find(|m| m.prefix == "cog").expect("the cog section");
        assert_eq!(mode.demo, "cog");
        assert_eq!(mode.tier, Tier::Headless);
        assert_eq!(policy("cog.view.coalesced.range_requests", "count"), Policy::Exact);
        assert_eq!(policy("cog.view.tiles_decoded", "count"), Policy::Exact);
        assert_eq!(policy("cog.view.map_tiles", "count"), Policy::Exact);
        assert_eq!(policy("cog.tile.dem.p50_us", "us"), Policy::Time);
        assert_eq!(policy("cog.view.http_8.ms", "ms"), Policy::Time);
    }

    #[test]
    fn the_fgb_section_is_registered_and_its_counts_are_exact() {
        let mode = MODES.iter().find(|m| m.prefix == "fgb").expect("the fgb section");
        assert_eq!(mode.demo, "fgb");
        assert!(matches!(mode.tier, Tier::Headless));
        let m = parse_gate_lines("gate: fgb.load_ms 300 ms\ngate: fgb.features_loaded 1000000 count\ngate: csv.parse_ms 1 ms\n", "fgb");
        assert_eq!(m.len(), 2);
        assert_eq!(policy("fgb.features_loaded", "count"), Policy::Exact);
        assert_eq!(policy("fgb.query.fixed.features", "count"), Policy::Exact);
        assert_eq!(policy("fgb.query.city.p50_us", "us"), Policy::Time);
        assert_eq!(policy("fgb.load_ms", "ms"), Policy::Time);
    }

    #[test]
    fn the_explorer_section_is_registered_with_its_bounds() {
        let mode = MODES.iter().find(|m| m.prefix == "explorer").expect("the explorer section");
        assert_eq!((mode.demo, mode.tier), ("explorer_gate", Tier::Headless));
        assert_eq!(policy("explorer.filter_drag.uploads", "count"), Policy::NoMore);
        assert_eq!(policy("explorer.cpu_filter_release_ms_p50", "ms"), Policy::TimeAtMost(EXPLORER_CPU_RELEASE_MS_BUDGET));
        assert_eq!(policy("explorer.rows_loaded", "count"), Policy::Exact);
        assert_eq!(policy("explorer.points_1m.pan_frame_ms_p50", "ms"), Policy::Time);
    }

    #[test]
    fn the_csv_section_is_registered_and_its_counts_are_exact() {
        let mode = MODES.iter().find(|m| m.prefix == "csv").expect("the csv section");
        assert_eq!(mode.demo, "csv_points");
        assert!(matches!(mode.tier, Tier::Headless));
        let m = parse_gate_lines("gate: csv.parse_ms 188 ms\ngate: csv.rows_loaded 999000 count\ngate: points.push_ms 1 ms\n", "csv");
        assert_eq!(m.len(), 2);
        assert_eq!(policy("csv.rows_loaded", "count"), Policy::Exact);
        assert_eq!(policy("csv.rows_skipped", "count"), Policy::Exact);
        assert_eq!(policy("csv.view.national_z4.features", "count"), Policy::Exact);
        assert_eq!(policy("csv.parse_ms", "ms"), Policy::Time);
    }

    #[test]
    fn the_story_section_is_registered_and_its_bands_parse() {
        let mode = MODES.iter().find(|m| m.prefix == "story").expect("the story section");
        assert_eq!(mode.tier, Tier::Headless);
        assert_eq!(mode.demo, "story");
        assert!(mode.args.contains(&"--gate"));
        assert!(std::path::Path::new(mode.args[0]).is_file(), "the det story exists: {}", mode.args[0]);
        let out = "gate: story.uhd.frame_ms_p50 10.5 ms\ngate: story.draw.retessellations_after_first 0 count\ngate: story.draw.frame_ms_p50 2.9 ms\ngate: sats.keyframe_ms 4 ms\n";
        let m = parse_gate_lines(out, "story");
        assert_eq!(m.len(), 3, "{m:?}");
        assert_eq!(policy("story.draw.retessellations_after_first", "count"), Policy::NoMore);
        assert_eq!(policy("story.uhd.frame_ms_p50", "ms"), Policy::Time);
        assert!(is_info("story.draw.frames"));
        let s = Spread { p50: 0.0, min: 0.0, max: 0.0, n: 3 };
        let b = new_bound("story.draw.retessellations_after_first", "count", s, "now");
        assert_eq!(b.ceiling(), 0.0, "one re-tessellation after the first frame fails the gate");
    }

    /// R11 (spec §14.3, ruling P16): the design card's rows. Its frame time
    /// and layout time are timed bands whose ceiling never passes the
    /// spec's absolute budget (4.0 ms, 300 us); a re-shape after the first
    /// frame may not rise above 0; the frame count is context.
    #[test]
    fn the_story_card_rows_are_graded_against_their_budgets() {
        let out = "gate: story.card.frame_ms_p50 3.1 ms\ngate: story.card.reshapes_after_first 0 count\n\
                   gate: story.card.layout_us_p50 120 us\ngate: story.card.frames 30 count\n";
        let m = parse_gate_lines(out, "story");
        assert_eq!(m.len(), 4, "{m:?}");
        assert_eq!(policy("story.card.frame_ms_p50", "ms"), Policy::TimeAtMost(STORY_CARD_FRAME_MS_BUDGET));
        assert_eq!(policy("story.card.layout_us_p50", "us"), Policy::TimeAtMost(STORY_CARD_LAYOUT_US_BUDGET));
        assert_eq!(policy("story.card.reshapes_after_first", "count"), Policy::NoMore);
        assert!(is_info("story.card.frames"));
        assert_eq!((STORY_CARD_FRAME_MS_BUDGET, STORY_CARD_LAYOUT_US_BUDGET), (4.0, 300.0), "the spec's budgets");

        // A quick, steady frame keeps a band under the budget...
        let quick = new_bound("story.card.frame_ms_p50", "ms", spread(&[2.0, 2.05, 2.1]), "t");
        assert!(quick.ceiling() < 4.0 && quick.ceiling() > 2.1, "{}", quick.ceiling());
        // ...a frame near the budget never gets a ceiling past it...
        let near = new_bound("story.card.frame_ms_p50", "ms", spread(&[3.6, 3.7, 3.8]), "t");
        assert!(near.ceiling() <= 4.0 + 1e-9, "the budget caps the band: {}", near.ceiling());
        assert!(!stress::grade("story.card.frame_ms_p50".to_string(), 4.01, near, true).pass);
        // ...and a baseline already over it fails the very next run.
        let over = new_bound("story.card.frame_ms_p50", "ms", spread(&[4.4, 4.5, 4.6]), "t");
        assert!(!stress::grade("story.card.frame_ms_p50".to_string(), 4.5, over, true).pass);
        let layout = new_bound("story.card.layout_us_p50", "us", spread(&[250.0, 280.0, 290.0]), "t");
        assert!(layout.ceiling() <= 300.0 + 1e-9, "{}", layout.ceiling());
        // Grade time: a committed band past the budget is cut to it.
        let mut loose = new_bound("story.card.frame_ms_p50", "ms", spread(&[2.0, 2.05, 2.1]), "t");
        loose.tolerance = 3.0;
        loose.slack = 1.0;
        let graded = graded_bound("story.card.frame_ms_p50", "ms", loose.clone());
        assert!((graded.ceiling() - 4.0).abs() < 1e-9, "{}", graded.ceiling());
        assert!(!stress::grade("story.card.frame_ms_p50".to_string(), 4.5, graded, true).pass);
        let other = graded_bound("story.draw.frame_ms_p50", "ms", loose.clone());
        assert_eq!(other.ceiling(), loose.ceiling(), "an unbudgeted row is graded as written");
        let reshapes = new_bound("story.card.reshapes_after_first", "count", spread(&[0.0; 3]), "t");
        assert_eq!(reshapes.ceiling(), 0.0, "one re-shape after the first frame fails the gate");
    }

    /// The satellites' sections: headless `sats` on the seeded set, windowed
    /// `sats.run` with its own prefix; the visible counts and the upload
    /// are exact, the times are times.
    #[test]
    fn the_sats_section_is_registered_and_its_bands_parse() {
        assert!(MODES.iter().any(|m| m.prefix == "sats" && m.tier == Tier::Headless && m.demo == "satellites"
            && m.args == ["--measure", "--synthetic"]));
        assert!(MODES.iter().any(|m| m.prefix == "sats.run" && m.tier == Tier::Windowed && m.demo == "satellites"));
        let out = "gate: sats.keyframe_ms 4.1 ms\ngate: sats.visible.z-1_lat0 5123 count\ngate: sats.upload_kb 586 KB\n";
        let m = parse_gate_lines(out, "sats");
        assert_eq!(m["sats.visible.z-1_lat0"], (5123.0, "count".to_string()));
        for key in ["sats.visible.z-1_lat0", "sats.visible.z-1_lat60", "sats.visible.z3_lat0"] {
            assert_eq!(policy(key, "count"), Policy::Exact, "{key}: a broken occlusion changes a count, not a time");
        }
        assert_eq!(policy("sats.upload_kb", "KB"), Policy::Exact);
        assert_eq!(policy("sats.dots_redraw_hz.z3_lat0", "hz"), Policy::NoMore);
        assert_eq!(policy("sats.dots_px_s.z3_lat0", "px/s"), Policy::Exact);
        assert_eq!(policy("sats.dots_speed_us", "us"), Policy::Time);
        assert_eq!(policy("sats.keyframe_ms", "ms"), Policy::Time);
        assert_eq!(owning_mode("sats.run.frame_p95_ms").unwrap().prefix, "sats.run");
        assert_eq!(owning_mode("sats.hit_us").unwrap().prefix, "sats");
        // Validity: gated on draw calls alone (no occluded_paints line).
        assert_eq!(policy("sats.run.frame_p95_ms", "ms"), Policy::Time);
        assert!(is_info("sats.run.draw_calls"));
        let mut m = BTreeMap::new();
        m.insert("sats.run.draw_calls".to_string(), (1200.0, "count".into()));
        assert_eq!(invalid_reason("sats.run", &m, 2.0, 6.0, false), None, "no occluded_paints line is required");
        m.insert("sats.run.draw_calls".to_string(), (0.0, "count".into()));
        assert!(invalid_reason("sats.run", &m, 2.0, 6.0, false).unwrap().contains("0 draw calls"));
    }

    /// The space look section: registered on `satellites --measure-look`,
    /// its GPU and write rows graded as times in us, its draw count info.
    #[test]
    fn the_look_section_is_registered_and_banded() {
        let m = MODES.iter().find(|m| m.prefix == "look").expect("look is registered");
        assert_eq!((m.tier, m.demo, m.args), (Tier::Headless, "satellites", &["--measure-look"][..]));
        for (key, unit) in [("look.gpu_2x_us", "us"), ("look.gpu_5k_us", "us"), ("look.build_ms", "ms"), ("look.write_us", "us")] {
            assert_eq!(policy(key, unit), Policy::Time, "{key}");
            assert_eq!(owning_mode(key).unwrap().prefix, "look");
        }
        assert!(is_info("look.draw_calls"));
    }

    /// The building sections: registered on `buildings_gate`, their names in
    /// the intended bands (counts exact, bytes as bytes, times as times), and
    /// kept apart -- `--only buildings` owns `buildings.tess.*` and not the
    /// windowed sections' keys, which each own their own.
    #[test]
    fn the_buildings_sections_are_registered_and_banded() {
        for (prefix, tier, args) in [
            ("buildings", Tier::Headless, &["--measure"][..]),
            ("buildings.pan", Tier::Windowed, &["--pan", "--quit-after", "25"][..]),
            ("buildings.pan_opaque", Tier::Windowed, &["--pan", "--opaque", "--quit-after", "25"][..]),
        ] {
            let m = MODES.iter().find(|m| m.prefix == prefix).unwrap_or_else(|| panic!("{prefix} is registered"));
            assert_eq!((m.tier, m.demo, m.args), (tier, "buildings_gate", args), "{prefix}");
        }
        for city in ["shinjuku_z14", "manhattan_z14", "singapore_z14"] {
            for exact in ["vertices", "indices", "walls"] {
                let key = format!("buildings.tess.{city}.{exact}");
                assert_eq!(policy(&key, "count"), Policy::Exact, "{key}");
                assert_eq!(owning_mode(&key).unwrap().prefix, "buildings");
            }
            assert_eq!(policy(&format!("buildings.tess.{city}.bake_ms"), "ms"), Policy::Time);
            assert_eq!(policy(&format!("buildings.tess.{city}.upload_mb"), "MB"), Policy::Bytes);
        }
        for prefix in ["buildings.pan", "buildings.pan_opaque"] {
            let key = |k: &str| format!("{prefix}.{k}");
            assert_eq!(owning_mode(&key("cpu_p99_ms")).unwrap().prefix, prefix);
            assert_eq!(policy(&key("frames_over_16_7ms"), "frames"), Policy::Spikes);
            assert_eq!(policy(&key("gpu_pass_p95_ms"), "ms"), Policy::Time);
            assert_eq!(policy(&key("rss_max_mb"), "MB"), Policy::Bytes);
            assert!(is_info(&key("draw_calls_total")) && is_info(&key("occluded_paints")));
            // Validity: a covered window or no draws voids the run.
            let mut m = BTreeMap::new();
            m.insert(key("occluded_paints"), (0.0, "paints".into()));
            m.insert(key("draw_calls_total"), (900.0, "draws".into()));
            assert_eq!(invalid_reason(prefix, &m, 2.0, 6.0, false), None);
            m.insert(key("draw_calls_total"), (0.0, "draws".into()));
            assert!(invalid_reason(prefix, &m, 2.0, 6.0, false).unwrap().contains("0 draw calls"));
            m.insert(key("draw_calls_total"), (900.0, "draws".into()));
            m.insert(key("occluded_paints"), (2.0, "paints".into()));
            assert!(invalid_reason(prefix, &m, 2.0, 6.0, false).unwrap().contains("frontmost"));
        }
        // Each section reads only its own lines, though `buildings.` is a
        // prefix of both windowed sections' names.
        let out = "gate: buildings.tess.shinjuku_z14.walls 47802 count\ngate: buildings.pan.cpu_p99_ms 9 ms\ngate: buildings.pan_opaque.cpu_p99_ms 7 ms\n";
        let only = |prefix: &str| parse_gate_lines(out, prefix).into_keys().collect::<Vec<_>>();
        assert_eq!(only("buildings"), ["buildings.tess.shinjuku_z14.walls"]);
        assert_eq!(only("buildings.pan"), ["buildings.pan.cpu_p99_ms"]);
        assert_eq!(only("buildings.pan_opaque"), ["buildings.pan_opaque.cpu_p99_ms"]);
    }

    /// F1: `extrusion_triangles_max` is a two-sided count band capped at the
    /// spec's 1.5M per-frame budget. Lost buildings (half the triangles, the
    /// prepass gone) FAIL; so does a frame over the budget.
    #[test]
    fn the_extrusion_triangle_band_fails_a_drop_a_rise_and_the_budget() {
        let key = "buildings.pan.extrusion_triangles_max";
        assert_eq!(policy(key, "triangles"), Policy::CountBand(1_500_000.0));
        assert_eq!(policy("buildings.pan_opaque.extrusion_triangles_max", "triangles"), Policy::CountBand(1_500_000.0));
        let b = new_bound(key, "triangles", spread(&[400_000.0, 410_000.0, 420_000.0]), "t");
        let pass = |v: f64| stress::grade(key.to_string(), v, b.clone(), true).pass;
        assert!(pass(410_000.0) && pass(380_000.0) && pass(450_000.0));
        assert!(!pass(205_000.0), "half the triangles (a lost prepass or culled tiles) must fail");
        assert!(!pass(300_000.0), "a quarter lost must fail");
        assert!(!pass(600_000.0), "a heavier frame must fail");
        // Near the budget, the ceiling stops at the cap.
        let near = new_bound(key, "triangles", spread(&[1_400_000.0; 3]), "t");
        assert!(near.ceiling() <= 1_500_000.0, "ceiling {}", near.ceiling());
        assert!(!stress::grade(key.to_string(), 1_550_000.0, near.clone(), true).pass);
        assert!(stress::grade(key.to_string(), 1_400_000.0, near, true).pass);
        // A baseline already over the budget fails its own next run.
        let over = new_bound(key, "triangles", spread(&[1_600_000.0; 3]), "t");
        assert!(!stress::grade(key.to_string(), 1_600_000.0, over, true).pass);
    }

    /// F5: `--projected-peak` answers each section's own peak, so an
    /// `--only` run is prechecked against it, not the tier's worst.
    #[test]
    fn every_section_has_its_own_projected_peak() {
        assert_eq!(projected_peak_mb("buildings"), Some(1_000.0));
        assert_eq!(projected_peak_mb("buildings.pan"), Some(2_000.0));
        assert_eq!(projected_peak_mb("headless"), Some(5_000.0));
        assert_eq!(projected_peak_mb("nope"), None);
    }

    /// `dots` and `dots.pan` are both real `Mode` prefixes, one a literal
    /// prefix of the other's baseline keys: `--only dots` must not treat
    /// `dots.pan.*` keys as its own (they would print as spurious MISSING).
    #[test]
    fn a_hierarchical_prefix_does_not_claim_its_more_specific_sibling() {
        let m = owning_mode("dots.pan.draw_calls").expect("some mode owns it");
        assert_eq!(m.prefix, "dots.pan");
        let m = owning_mode("dots.view.world_z1.cells").expect("some mode owns it");
        assert_eq!(m.prefix, "dots");
    }
}
