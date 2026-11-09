//! Every person on Earth as a dot: the GHSL 30″ world (GHS-POP R2023A,
//! 2025) drawn by `MapView::set_dot_density_source` over SWAT Routes Dark
//! (open), the keyless look-alike over OpenFreeMap (`--mapbox-streets` for
//! Mapbox Streets).
//!
//! See `docs/superpowers/specs/2026-09-26-world-dot-density-design.md`.
//!
//! ```text
//! cargo run --release --features probe --example world_dots -- [--grid PATH|URL] [--synthetic]
//!     [--measure] [--at world|india|dhaka|dhaka15] [--pan] [--zoom-sweep]
//!     [--level-switch-ab OUTDIR] [--capture OUTDIR] [--quit-after SECS] [--budget N] [--diag WxH]
//!     [--clickhouse [URL]] [--table ghs_cells_30ss|ghs_cells_3ss] [--measure-clickhouse]
//!     [--view LON,LAT,ZOOM] [--size WxH] [--pixel-ratio R] [--shrink F] [--fade G] [--ink-report]
//!     [--relocate on|off] [--k N] [--placement-probe] [--pitch DEG]
//!     [--worldview IN|US|CN|JP] [--epochs A,B] [--animate-epochs N] [--no-basemap] [--mapbox-streets]
//! ```
//!
//! - `--grid` defaults to `~/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles`
//!   (`scripts/fetch-ghsl.sh 2025 30ss`, then `tools/dotgrid-build`). When
//!   it is absent the demo prints those commands and draws the procedural
//!   `--synthetic` world instead, labelled as such on screen.
//! - Windowed (default): the counter reads `N people in view · D dots · 1
//!   dot = P people` -- the people whose cells are centred on screen and
//!   their dots -- and the time to the first drawn dot is printed.
//!   The basemap is SWAT Routes Dark (open) whether or not a token is set;
//!   `--mapbox-streets` with `MAPBOX_ACCESS_TOKEN` picks Mapbox Streets
//!   (`examples/mapbox-streets-style.json`) instead.
//! - `--measure`: no window. Plans and builds the dot blocks of each preset
//!   camera at 1920x1080 and prints `gate: dots.*` lines when `US_BG_GATE`
//!   is set (the synthetic world unless `--grid` is given).
//! - `--pan` / `--zoom-sweep` (need `--features probe`): MG-11's frame times.
//! - `--level-switch-ab OUTDIR`: MG-9's capture A/B at each level switch.
//! - `--capture OUTDIR`: each preset, headless, over SWAT Routes Dark (open)
//!   (Mapbox Streets with `--mapbox-streets`), to a PNG.
//!   With `--view LON,LAT,ZOOM` that camera alone (`--at` too takes it),
//!   written twice: with the dots and without (`view_nodots.png`), at
//!   `--size WxH`. `--shrink F` sets `DotStyle::density_shrink` (0 = every
//!   dot at the zoom's radius). `--fade G` sets `DotStyle::overlap_gamma`
//!   (0 = no overlap fade), in the window too.
//! - `--relocate on|off` (default on): dasymetric placement -- each dot
//!   moves, inside its own cell, to a building on the basemap (and off
//!   water and parks) among `--k N` candidates (`DotStyle::mask_k`,
//!   default `render::dots::DEFAULT_MASK_K_30SS`); in the window and in
//!   `--capture`. Off draws the uniform placement.
//! - `--placement-probe` (needs `--features probe`, and Mapbox Streets):
//!   no window. DP-4/DP-5 of the dasymetric placement plan: the `--at`
//!   camera captured with relocation on, the mask it drew with read back,
//!   and every live dot inside it placed by the CPU mirror
//!   (`overlay::dotgrid::placement`), relocated and not; prints
//!   `placement[..]: live in_building on_water on_green analytic avoidable
//!   changed` and DP-4's verdict. With `--pan`: a 12-frame steady pan,
//!   the dots whose chosen candidate changed per frame; with
//!   `--zoom-sweep`: an integer-zoom crossing and a tile arrival.
//!   `--capture OUTDIR` writes the probed frame.
//! - `--pitch DEG`: the camera's pitch, in the window and the probe.
//! - `--worldview IN|US|CN|JP`: `style::set_worldview` on the vendored
//!   Mapbox Streets style before it compiles -- switches which side of a
//!   disputed border (e.g. Kashmir, `--at india`) draws solid. Honoured by
//!   windowed, `--capture` and `--placement-probe`, and implies
//!   `--mapbox-streets` (the open style has no worldview); any other value
//!   panics.
//! - `--mapbox-streets`: windowed and `--capture` draw Mapbox Streets
//!   (needs `MAPBOX_ACCESS_TOKEN`) rather than SWAT Routes Dark (open).
//! - `--at` also takes `paris15 amsterdam13 cairo14 sahel12 nizhny14`
//!   and `nizhny16` (the user's street view, 43.995,56.318 z16.5).
//! - `--ink-report`: no window. The `--view` (or `--at`) camera's dots
//!   alone, white on black, no basemap, at `--size` (default 1920x1080),
//!   and prints `ink: blocks N saturated_pct X deciles D total_ink T`
//!   (`overlay::dotgrid::ink::ink_report` over 32 px blocks) with the
//!   frame's `draw_calls_total` -- FO-1's measure of whether a dense view
//!   reads as a gradient or as solid blocks. With `--capture OUTDIR` the
//!   frame is also written to `OUTDIR/<name>_ink.png`.
//! - `--diag WxH`: no window. Per drawn tile of the `--at` preset's settled
//!   view: cells, population, ppd, slots, blocks, slots issued, dots alive
//!   and texture bytes (`DIAG_GLOBE=1` plans on the globe's cover, as the
//!   window does at globe zooms).
//! - `--clickhouse [URL]` (needs `--features clickhouse-demo`): the count
//!   tiles come live from `gpui_map_dots.<--table>` on ClickHouse (URL
//!   defaults to `CLICKHOUSE_URL`, else `http://localhost:8123`) instead of
//!   the archive, in the window and in `--capture`. A server that is down
//!   or slow at startup is reported on screen, and the basemap draws
//!   without dots.
//! - `--epochs A,B` (live epochs, Task 15; needs a source holding both,
//!   i.e. `--clickhouse` after `scripts/load-ghsl-epochs.sh`): the dots
//!   are fetched with both epochs' counts and drawn at epoch A. In the
//!   window, `space` animates A -> B over 2 s (and back from B), `[` / `]`
//!   step epoch B through the source's epochs (the new pair is fetched),
//!   and `shift+D` toggles the difference ramp (blue fewer, red more
//!   people; plain `d` is the engine's tile-debug overlay). Moving between
//!   the epochs writes uniforms only: no fetch, rebuild or upload.
//! - `--animate-epochs N` (needs `--features probe` and `--epochs`): MG-23.
//!   Waits for the view to settle, then animates A -> B N times (2 s each)
//!   and prints the frame times (`-- epochs: ...`) and the upload deltas
//!   over the animations (`epochs uploads: ...`), then quits.
//! - `--no-basemap`: an empty style (no tiles, no network) under the dots
//!   -- for timing the dots alone when no basemap is reachable.
//! - `--measure-clickhouse` (same feature): no window. MG-14's per-batch
//!   query latency at the world, India z5 and Dhaka z8 views, with the
//!   query cache off (cold) and on (warm), and bytes per response.

// The windowed body -- the dot source, the basemap, the window root and
// its counter -- lives in the panel (`kit/src/panels/world_dots.rs`, the
// one copy), which also includes the synthetic world: one include per
// crate. `main`, `Args`, every headless mode and `windowed` stay here.
use swat_maps_kit::panels::world_dots as world_dots_panel;
use world_dots_panel::{
    counter_line, counter_text, default_clickhouse_url, dot_style, mapbox_streets, open_source, people_text, pick_source, preset, routes_open,
    style_of, synthetic, table_arcsec, thousands, DemoRoot, DotSource, Preset, StyleArgs, H, LAYER, PRESETS, SYNTHETIC_LABEL, W,
};
#[cfg(feature = "clickhouse-demo")]
use world_dots_panel::display_url;
#[cfg(feature = "probe")]
use world_dots_panel::{mix_at, EPOCH_ANIMATION};

// The demo keys every example window has: `t` (day/night terminator),
// `g` (globe or flat), `s` (imagery; none configured here). This demo
// draws a counter over the map, so its root is its own and holds the
// shell's `DemoKeys` rather than being a `DemoShell`.
use swat_maps_kit::shell;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{px, size, App, AppContext, Bounds, Entity, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_maps::capture::{capture, fetch_cover, CaptureDots, CaptureRequest, Clear, TileStore};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::dotgrid::blocks::{build_blocks, TileBlocks, OVERLAY_LYON_PER_UNIT_WORLD};
use gpui_maps::overlay::dotgrid::layer::{DotLayers, DotRamp, DotStyle};
#[cfg(feature = "clickhouse-demo")]
use gpui_maps::overlay::dotgrid::clickhouse::ClickHouseGridSource;
use gpui_maps::overlay::dotgrid::{fetch_native, grid, hash, level, GridCountSource, GridTile, GridTileKey};
use gpui_maps::render::allocation_stats;
use gpui_maps::render::dots::DOT_BUCKETS;
use gpui_maps::style::Rgba;
use gpui_maps::tile_source::TileSource;

/// `--worldview`'s fixed vocabulary -- same shape as `--relocate on|off`:
/// validated at parse time, not silently threaded through to
/// `style::set_worldview` as an arbitrary string (which would rewrite the
/// disputed-boundary layers to a `worldview` value no real vector-tile
/// feature carries, dropping the line with no diagnostic).
fn worldview_arg(v: String) -> String {
    match v.as_str() {
        "IN" | "US" | "CN" | "JP" => v,
        other => panic!("--worldview takes IN|US|CN|JP, got {other:?}"),
    }
}

struct Args {
    grid: Option<String>,
    synthetic: bool,
    measure: bool,
    at: Preset,
    pan: bool,
    zoom_sweep: bool,
    level_switch_ab: Option<PathBuf>,
    capture: Option<PathBuf>,
    quit_after: Option<f64>,
    diag: Option<(u32, u32)>,
    /// `--clickhouse [URL]`: the server to read count tiles from.
    clickhouse: Option<String>,
    table: String,
    measure_clickhouse: bool,
    /// `--view LON,LAT,ZOOM`: `at` is this camera, not a preset, and
    /// `--capture` draws it alone (with dots, and again without).
    view: bool,
    /// `--size WxH`: `--capture`'s frame size (default 1920x1080).
    size: Option<(u32, u32)>,
    /// `--pixel-ratio R`: `--capture`'s device pixels per logical pixel
    /// (default 1): `--size 3200x2000 --pixel-ratio 2` is the 1600x1000
    /// view drawn sharp for a Retina screen.
    pixel_ratio: f32,
    /// `--ink-report`: FO-1's ink spread of the `at` camera's dots.
    ink_report: bool,
    /// `--placement-probe`: DP-4/DP-5's placement counts (probe builds).
    placement_probe: bool,
    /// `--pitch P`: the camera's pitch in degrees (window, `--pan` and the probe).
    pitch: f32,
    /// `--worldview IN|US|CN|JP`: the worldview code inside Mapbox Streets'
    /// disputed-border `match` clauses (`style::set_worldview`), applied to
    /// the vendored style before it compiles. `None` draws the style as
    /// vendored (`US`).
    worldview: Option<String>,
    /// `--animate-epochs N`: MG-23, N animations A -> B.
    animate_epochs: Option<u32>,
    /// `--no-basemap`: an empty style under the dots.
    no_basemap: bool,
    /// `--mapbox-streets` (or `--worldview`): Mapbox Streets, not SWAT
    /// Routes Dark (open), in the window and `--capture`.
    mapbox_streets: bool,
    /// `--budget`, `--shrink`, `--fade`, `--relocate`, `--k` and
    /// `--epochs`: what `style_of` reads.
    style: StyleArgs,
}

fn parse_args() -> Args {
    let mut a = Args {
        grid: None,
        synthetic: false,
        measure: false,
        at: PRESETS[0],
        pan: false,
        zoom_sweep: false,
        level_switch_ab: None,
        capture: None,
        quit_after: None,
        diag: None,
        clickhouse: None,
        table: "ghs_cells_30ss".to_string(),
        measure_clickhouse: false,
        view: false,
        size: None,
        pixel_ratio: 1.0,
        ink_report: false,
        placement_probe: false,
        pitch: 0.0,
        worldview: None,
        animate_epochs: None,
        no_basemap: false,
        mapbox_streets: false,
        style: StyleArgs::default(),
    };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--grid" => a.grid = Some(it.next().expect("--grid takes a path or URL")),
            "--synthetic" => a.synthetic = true,
            "--measure" => a.measure = true,
            "--at" => a.at = preset(&it.next().expect("--at takes a preset")),
            "--pan" => a.pan = true,
            "--zoom-sweep" => a.zoom_sweep = true,
            "--level-switch-ab" => a.level_switch_ab = Some(PathBuf::from(it.next().expect("--level-switch-ab takes a dir"))),
            "--capture" => a.capture = Some(PathBuf::from(it.next().expect("--capture takes a dir"))),
            "--quit-after" => a.quit_after = it.next().and_then(|s| s.parse().ok()),
            "--budget" => a.style.budget = Some(it.next().and_then(|s| s.parse().ok()).expect("--budget takes a dot count")),
            "--diag" => {
                let wh = it.next().expect("--diag takes WxH");
                let (w, h) = wh.split_once('x').expect("--diag takes WxH");
                a.diag = Some((w.parse().expect("width"), h.parse().expect("height")));
            }
            "--clickhouse" => {
                let url = it.next_if(|next| !next.starts_with("--"));
                a.clickhouse = Some(url.unwrap_or_else(default_clickhouse_url));
            }
            "--table" => a.table = it.next().expect("--table takes ghs_cells_30ss|ghs_cells_3ss"),
            "--measure-clickhouse" => a.measure_clickhouse = true,
            "--view" => {
                let v: Vec<f64> = it
                    .next()
                    .expect("--view takes LON,LAT,ZOOM")
                    .split(',')
                    .map(|x| x.trim().parse().expect("--view takes LON,LAT,ZOOM"))
                    .collect();
                assert_eq!(v.len(), 3, "--view takes LON,LAT,ZOOM");
                a.at = Preset { name: "view", lon: v[0], lat: v[1], zoom: v[2] };
                a.view = true;
            }
            "--pixel-ratio" => a.pixel_ratio = it.next().and_then(|s| s.parse().ok()).filter(|r: &f32| *r > 0.0).expect("--pixel-ratio takes a positive number"),
            "--size" => {
                let wh = it.next().expect("--size takes WxH");
                let (w, h) = wh.split_once('x').expect("--size takes WxH");
                a.size = Some((w.parse().expect("width"), h.parse().expect("height")));
            }
            "--shrink" => a.style.shrink = Some(it.next().and_then(|s| s.parse().ok()).expect("--shrink takes a fraction")),
            "--fade" => a.style.fade = Some(it.next().and_then(|s| s.parse().ok()).expect("--fade takes an exponent")),
            "--ink-report" => a.ink_report = true,
            "--relocate" => {
                a.style.relocate = Some(match it.next().as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    other => panic!("--relocate takes on|off, got {other:?}"),
                });
            }
            "--placement-probe" => a.placement_probe = true,
            "--pitch" => a.pitch = it.next().and_then(|s| s.parse().ok()).expect("--pitch takes degrees"),
            "--k" => a.style.k = Some(it.next().and_then(|s| s.parse().ok()).expect("--k takes a candidate count")),
            "--worldview" => a.worldview = Some(worldview_arg(it.next().expect("--worldview takes IN|US|CN|JP"))),
            "--epochs" => {
                let v = it.next().expect("--epochs takes A,B");
                let (x, y) = v.split_once(',').expect("--epochs takes A,B");
                a.style.epochs = Some((x.trim().parse().expect("--epochs takes A,B"), y.trim().parse().expect("--epochs takes A,B")));
            }
            "--animate-epochs" => a.animate_epochs = Some(it.next().and_then(|s| s.parse().ok()).expect("--animate-epochs takes a count")),
            "--no-basemap" => a.no_basemap = true,
            "--mapbox-streets" => a.mapbox_streets = true,
            other => panic!("unknown argument {other}"),
        }
    }
    if a.measure_clickhouse && a.clickhouse.is_none() {
        a.clickhouse = Some(default_clickhouse_url());
    }
    if a.clickhouse.is_some() && !cfg!(feature = "clickhouse-demo") {
        eprintln!("world_dots: --clickhouse: built without clickhouse-demo (rebuild with --features clickhouse-demo)");
        std::process::exit(2);
    }
    a
}

/// One `gate:` line in `examples/perf_gate.rs`' format.
fn gate_line(name: &str, value: f64, unit: &str) -> String {
    format!("gate: {name} {value} {unit}")
}

fn gate(name: &str, value: f64, unit: &str) {
    if std::env::var_os("US_BG_GATE").is_some() {
        println!("{}", gate_line(name, value, unit));
    }
}

fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the struct it is given and nothing else.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage.ru_maxrss as u64
}

/// `v`'s `q` quantile (nearest rank on a sorted copy); NaN when empty.
#[cfg_attr(not(feature = "probe"), allow(dead_code))]
fn quantile(v: &[f64], q: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return f64::NAN;
    }
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// A headless mode's source: without one there is nothing to do.
fn required_source(args: &Args) -> Arc<dyn GridCountSource> {
    let picked = pick_source(args.clickhouse.as_deref(), &args.table, args.grid.as_deref(), args.synthetic);
    picked.source.unwrap_or_else(|| {
        eprintln!("world_dots: no dot source: {}", picked.note.unwrap_or_default());
        std::process::exit(1);
    })
}

/// Why a gate run (`US_BG_GATE`) must not go ahead, if it must not: the
/// perf gate builds every demo with `--features probe`, and a binary
/// without it (the `dots.relocate.*` rows are probe-only) is not the one the gate built -- a sibling build
/// re-pointing the shared `release/examples` link, say. Refusing makes
/// that an exit code instead of rows quietly MISSING (see
/// `us_block_groups`'s `gate_refusal`).
fn gate_refusal(gate: bool) -> Option<&'static str> {
    (gate && !cfg!(feature = "probe")).then_some(
        "GATE ERROR: US_BG_GATE is set but this binary was built without --features probe; \
         rebuild with --features probe (scripts/perf-gate.sh does).",
    )
}

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

fn main() {
    let started = Instant::now();
    gpui_maps::render::note_process_start();
    let args = parse_args();
    if let Some(refusal) = gate_refusal(std::env::var_os("US_BG_GATE").is_some()) {
        // stdout: perf_gate discards stderr, and the log is what a human reads.
        println!("{refusal}");
        std::process::exit(2);
    }
    if args.measure_clickhouse {
        measure_clickhouse(args.clickhouse.as_deref().unwrap_or_default(), &args.table, args.at);
    } else if let Some((w, h)) = args.diag {
        diag(&required_source(&args), &dot_style(args.style.budget), args.at, w, h);
    } else if args.measure {
        let (source, synthetic) = if args.grid.is_some() && !args.synthetic {
            open_source(args.grid.as_deref(), false)
        } else {
            (Arc::new(synthetic::source()) as Arc<dyn GridCountSource>, true)
        };
        measure(&source, synthetic, &dot_style(args.style.budget));
    } else if let Some(dir) = &args.level_switch_ab {
        level_switch_ab(&required_source(&args), &dot_style(args.style.budget), dir);
    } else if args.placement_probe {
        placement_probe(&required_source(&args), &style_of(&args.style), &args);
    } else if args.ink_report {
        ink_report(&required_source(&args), &style_of(&args.style), args.at, args.size, args.capture.as_deref());
    } else if let Some(dir) = &args.capture {
        let views: Vec<Preset> = if args.view { vec![args.at] } else { PRESETS.to_vec() };
        let mapbox = args.mapbox_streets || args.worldview.is_some();
        capture_presets(&required_source(&args), &style_of(&args.style), dir, &views, args.size, args.pixel_ratio, args.view, mapbox, args.worldview.as_deref());
    } else {
        windowed(args, started);
    }
}

// ---------------------------------------------------------------- headless

fn request_at(lon: f64, lat: f64, zoom: f64) -> CaptureRequest {
    CaptureRequest { center_lon: lon, center_lat: lat, zoom, width: W, height: H, flat: true, ..CaptureRequest::default() }
}

/// A `DotLayers` for one camera, run until nothing is left to fetch or
/// rebuild. The clock is stepped 100 ms a frame so the ppd controller
/// settles on the loaded view instead of its first estimate.
fn settle(source: &Arc<dyn GridCountSource>, style: &DotStyle, request: &CaptureRequest) -> DotLayers {
    settle_on(source, style, request, gpui_maps::render::globe::GlobeCover::FLAT)
}

/// [`settle`] over `globe`'s cover (the window's, at a globe zoom).
fn settle_on(
    source: &Arc<dyn GridCountSource>,
    style: &DotStyle,
    request: &CaptureRequest,
    globe: gpui_maps::render::globe::GlobeCover,
) -> DotLayers {
    let mut layers = DotLayers::new();
    layers.set_source(LAYER, Arc::clone(source), style.clone());
    let camera = request.camera();
    let t0 = Instant::now();
    let mut quiet = 0;
    for i in 0..2_000u64 {
        let now = gpui_maps::timing::Instant::from(t0 + Duration::from_millis(100 * i));
        let busy = layers.frame(&camera, &globe, now);
        let mut ran = false;
        while let Some(job) = layers.next_job() {
            layers.land(job.run(fetch_native::execute));
            ran = true;
        }
        quiet = if busy || ran { 0 } else { quiet + 1 };
        // Ten quiet frames (1 s of controller time): the ppd has eased
        // onto the full view's population and every rebuild has run.
        if quiet >= 10 {
            break;
        }
    }
    layers
}

/// Dots alive at `ppd` in `blocks`, by `vs_dot`'s rule: dot `i` of a cell
/// is alive when `i + h_t < pop / ppd`, so every index below
/// `floor(pop / ppd)` is, none from `ceil` up is, and the one between is
/// decided by the real `hash::dot_alive`.
fn alive_dots(blocks: &TileBlocks, ppd: f32) -> u64 {
    let mut alive = 0u64;
    for (b, bucket) in blocks.buckets.iter().enumerate() {
        let size = u64::from(DOT_BUCKETS[b]);
        for block in bucket {
            let q = block.pop / ppd;
            let n = q.floor().max(0.0) as u64;
            let first = u64::from(block.first_dot);
            alive += n.saturating_sub(first).min(size);
            if n >= first && n < first + size && hash::dot_alive(block.pop, ppd, n as u32, hash::cell_key(block.origin)) {
                alive += 1;
            }
        }
    }
    alive
}

fn block_count(blocks: &TileBlocks) -> u64 {
    blocks.buckets.iter().map(|b| b.len() as u64).sum()
}

/// One tile, fetched and decoded on this thread.
fn fetch_tile(source: &dyn GridCountSource, key: GridTileKey) -> GridTile {
    let body = fetch_native::execute(&source.request(&[key])).expect("a tile fetch");
    source.decode(&[key], &body).expect("a tile decode").into_iter().next().map(|(_, t)| t).unwrap_or_default()
}

/// Task 2's sampled nesting check, on real cells: a dot alive at `ppd`
/// must be alive at `ppd / 2`.
fn nesting_violations(blocks: &TileBlocks, view_ppd: f32) -> (u64, u64) {
    let (mut checked, mut violations) = (0u64, 0u64);
    for block in blocks.buckets.iter().flatten().step_by(37) {
        let key = hash::cell_key(block.origin);
        for ppd in [1.0f32, 1.7, 2.0, 16.0, 333.0, 2048.0, view_ppd] {
            let top = ((block.pop / ppd).ceil() as u32 + 2).min(4096);
            for i in 0..top {
                checked += 1;
                if hash::dot_alive(block.pop, ppd, i, key) && !hash::dot_alive(block.pop, ppd / 2.0, i, key) {
                    violations += 1;
                }
            }
        }
    }
    (checked, violations)
}

fn measure(source: &Arc<dyn GridCountSource>, synthetic: bool, style: &DotStyle) {
    let d = source.descriptor().clone();
    println!(
        "world_dots --measure: {} grid, {}″, {} levels, budget {} dots, {W}x{H}",
        if synthetic { "synthetic" } else { "archive" },
        d.base_arcsec,
        d.levels,
        thousands(u64::from(style.budget_dots))
    );
    let mut build_ms: Vec<f64> = Vec::new();
    let (mut nest_checked, mut nest_violations) = (0u64, 0u64);
    for p in PRESETS {
        let started = Instant::now();
        let layers = settle(source, style, &request_at(p.lon, p.lat, p.zoom));
        let settle_ms = started.elapsed().as_secs_f64() * 1e3;
        let stats = layers.stats(LAYER).expect("the layer");
        let plan = layers.plan(LAYER).expect("a plan").clone();
        let (mut cells, mut blocks, mut dots, mut slots) = (0u64, 0u64, 0u64, 0u64);
        for draw in &plan.draw {
            let tile = layers.tile(LAYER, &draw.key).expect("a settled tile");
            let ppd = stats.ppd.max(1.0);
            let t = Instant::now();
            let b = build_blocks(&d, &draw.key, &tile, level::ppd_build_for(ppd), OVERLAY_LYON_PER_UNIT_WORLD);
            build_ms.push(t.elapsed().as_secs_f64() * 1e3);
            cells += tile.cells.len() as u64;
            blocks += block_count(&b);
            slots += b.slots;
            dots += alive_dots(&b, ppd);
            let (c, v) = nesting_violations(&b, ppd);
            nest_checked += c;
            nest_violations += v;
        }
        let upload_mb = blocks as f64 * 16.0 / 1e6;
        println!(
            "view {:<10} z{:<4} level {} ppd {:>9.1} | {} tiles ({} stand-ins) | {:>10} cells {:>10} blocks {:>11} slots {:>10} dots | upload {:.1} MB | settle {settle_ms:.0} ms",
            p.name,
            p.zoom,
            stats.level,
            stats.ppd,
            plan.draw.len(),
            stats.standin_tiles,
            thousands(cells),
            thousands(blocks),
            thousands(slots),
            thousands(dots),
            upload_mb
        );
        gate(&format!("dots.view.{}.cells", p.name), cells as f64, "count");
        gate(&format!("dots.view.{}.blocks", p.name), blocks as f64, "count");
        gate(&format!("dots.view.{}.dots", p.name), dots as f64, "count");
        gate(&format!("dots.view.{}.upload_mb", p.name), upload_mb, "MB");
    }
    println!(
        "tile build: {} tiles, p50 {:.2} ms, max {:.2} ms",
        build_ms.len(),
        quantile(&build_ms, 0.5),
        quantile(&build_ms, 1.0)
    );
    gate("dots.tile_build_ms_p50", quantile(&build_ms, 0.5), "ms");
    gate("dots.tile_build_ms_max", quantile(&build_ms, 1.0), "ms");

    // A zoom change's worker cost at Dhaka: plan the new zoom, then build
    // every drawn tile's blocks (the fetch is IO and not counted).
    let dhaka = PRESETS[2];
    let (mut worst, mut worst_z) = (0.0f64, 0u8);
    for z in 0..=16u8 {
        let request = request_at(dhaka.lon, dhaka.lat, f64::from(z));
        let mut layers = settle(source, style, &request);
        let stats = layers.stats(LAYER).expect("the layer");
        let t = Instant::now();
        layers.frame(&request.camera(), &gpui_maps::render::globe::GlobeCover::FLAT, gpui_maps::timing::Instant::now());
        let plan = layers.plan(LAYER).expect("a plan").clone();
        for draw in &plan.draw {
            let tile = layers.tile(LAYER, &draw.key).expect("a settled tile");
            let ppd = stats.ppd.max(1.0);
            std::hint::black_box(build_blocks(&d, &draw.key, &tile, level::ppd_build_for(ppd), OVERLAY_LYON_PER_UNIT_WORLD));
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if ms > worst {
            (worst, worst_z) = (ms, z);
        }
    }
    println!("zoom change z0..z16 at Dhaka: worst plan + block build {worst:.1} ms at z{worst_z}");
    gate("dots.zoom_change_worst_ms", worst, "ms");

    println!("nesting: {nest_violations} violations in {} sampled dot checks", thousands(nest_checked));
    gate("dots.nesting_violations", nest_violations as f64, "count");

    // Every level-0 tile at ppd 1: the dots vs the source's own sum.
    let started = Instant::now();
    let z = grid::data_zoom(&d, 0);
    let n = 1u32 << z;
    let (mut people, mut dots, mut populated) = (0f64, 0u64, 0u64);
    for y in 0..n {
        for x in 0..n {
            let key = GridTileKey { tile: gpui_maps::geo::TileId { z, x, y }, level: 0, epoch: 2025, epoch_b: None };
            let tile = fetch_tile(source.as_ref(), key);
            if tile.cells.is_empty() {
                continue;
            }
            populated += tile.cells.len() as u64;
            people += tile.total;
            let b = build_blocks(&d, &key, &tile, 1.0, OVERLAY_LYON_PER_UNIT_WORLD);
            dots += alive_dots(&b, 1.0);
        }
    }
    let err_pct = (dots as f64 - people) / people * 100.0;
    println!(
        "world at ppd 1: {} dots vs {} people ({err_pct:+.4} %) over {} populated level-0 cells, {:.1} s",
        thousands(dots),
        thousands(people.round() as u64),
        thousands(populated),
        started.elapsed().as_secs_f64()
    );
    gate("dots.world_total_dots", dots as f64, "count");
    gate("dots.world_total_err_pct", err_pct.abs(), "pct");
    measure_relocation_counts(source, style);
    gate("dots.peak_rss_mb", peak_rss_bytes() as f64 / 1e6, "MB");
}

/// DP-1's views: the presets, and Dhaka z14.
const RELOCATE_PRESETS: [Preset; 5] = [
    PRESETS[0],
    PRESETS[1],
    PRESETS[2],
    Preset { name: "dhaka_z14", lon: 90.4125, lat: 23.8103, zoom: 14.0 },
    PRESETS[3],
];

/// **DP-1: relocation changes no count.** Each view settled with
/// relocation on and off: every `DotStats` count, the plan's draws, and
/// every drawn tile's per-cell alive set -- on through the relocating CPU
/// mirror (`placement::alive_digest`) against a synthetic 30 %-building
/// mask, off through `hash::dot_alive` alone -- compared. `count_diff` is
/// the number of differences: 0.
fn measure_relocation_counts(source: &Arc<dyn GridCountSource>, style: &DotStyle) {
    use gpui_maps::overlay::dotgrid::placement::{alive_digest, SyntheticMask};
    use gpui_maps::render::dot_mask::MaskGrid;
    let d = source.descriptor().clone();
    let on = DotStyle { relocate: true, mask_k: style.mask_k.max(2), ..style.clone() };
    let off = DotStyle { relocate: false, ..style.clone() };
    let upl = 1.0 / OVERLAY_LYON_PER_UNIT_WORLD;
    // The CPU mirror's relocation cost per drawn tile (`MaskGrid::plan`,
    // the synthetic texel fill, and `alive_digest`'s relocating branch):
    // `dots.relocate.cpu_mirror_ms`, a harness proxy -- there is no real
    // GPU mask pass headless, and this times none (the window's pass is
    // `dots.pan`, and `MaskPassTimer` in probe builds).
    let mut cpu_mirror_ms: Vec<f64> = Vec::new();
    for p in RELOCATE_PRESETS {
        let request = request_at(p.lon, p.lat, p.zoom);
        // One settled view at a time (the gate's peak RSS): its stats, its
        // draws, and each drawn tile's (live, digest).
        let run = |style: &DotStyle, relocated: bool, cpu_mirror_ms: &mut Vec<f64>| {
            let layers = settle(source, style, &request);
            let stats = layers.stats(LAYER).expect("the layer");
            let plan = layers.plan(LAYER).expect("a plan").clone();
            let ppd = stats.ppd.max(1.0);
            let digests: Vec<(u64, u64)> = plan
                .draw
                .iter()
                .map(|draw| {
                    let tile = layers.tile(LAYER, &draw.key).expect("a settled tile");
                    let blocks = build_blocks(&d, &draw.key, &tile, level::ppd_build_for(ppd), OVERLAY_LYON_PER_UNIT_WORLD);
                    let t = draw.key.tile;
                    let n = f64::from(1u32 << t.z);
                    let origin = (f64::from(t.x) / n, f64::from(t.y) / n);
                    if relocated {
                        let started = Instant::now();
                        let region = [origin.0, origin.1, origin.0 + 1.0 / n, origin.1 + 1.0 / n];
                        let grid = MaskGrid::plan(p.zoom, region, 0.0, 1024).expect("a grid");
                        let texels = SyntheticMask::Checker { tenths: 3 }.texels(&grid);
                        let digest = alive_digest(&blocks, origin, upl, ppd, Some((on.mask_k, &grid, &texels)));
                        cpu_mirror_ms.push(started.elapsed().as_secs_f64() * 1e3);
                        digest
                    } else {
                        alive_digest(&blocks, origin, upl, ppd, None)
                    }
                })
                .collect();
            (stats, plan.draw, digests)
        };
        let (sa, da, ga) = run(&on, true, &mut cpu_mirror_ms);
        let (sb, db, gb) = run(&off, false, &mut cpu_mirror_ms);
        let mut diff = u64::from(sa != sb) + u64::from(da != db) + u64::from(ga.len() != gb.len());
        diff += sa.dots_drawn.abs_diff(sb.dots_drawn) + sa.dots_in_view.abs_diff(sb.dots_in_view) + sa.slots_drawn.abs_diff(sb.slots_drawn);
        diff += u64::from(sa.people_in_view.to_bits() != sb.people_in_view.to_bits()) + u64::from(sa.pop_drawn.to_bits() != sb.pop_drawn.to_bits());
        for (a, b) in ga.iter().zip(&gb) {
            diff += a.0.abs_diff(b.0) + u64::from(a.1 != b.1);
        }
        let live: u64 = ga.iter().map(|g| g.0).sum();
        println!(
            "relocate {:<10} on vs off: {} dots drawn, {} in view, {:.0} people in view, {} live in drawn tiles | {diff} differences",
            p.name,
            thousands(sa.dots_drawn),
            thousands(sa.dots_in_view),
            sa.people_in_view,
            thousands(live)
        );
        gate(&format!("dots.relocate.{}.count_diff", p.name), diff as f64, "count");
    }
    gate("dots.relocate.cpu_mirror_ms", quantile(&cpu_mirror_ms, 0.5), "ms");
}

/// Fix-round diagnostics: per drawn tile of one settled view, what the
/// layer holds and what a draw at the tile's ppd would issue.
/// MG-14's views: the world, India z5 and Dhaka z8 (level 0, overzoomed).
#[cfg(feature = "clickhouse-demo")]
const CH_PRESETS: [Preset; 3] = [
    PRESETS[0],
    PRESETS[1],
    Preset { name: "dhaka_z8", lon: 90.4125, lat: 23.8103, zoom: 8.0 },
];

/// The per-batch cold p95 criterion `--measure-clickhouse` judges `table`
/// by: MG-14 (30″, ≤ 50 ms) or MG-21 (3″, ≤ 80 ms; Task 13).
#[cfg_attr(not(feature = "clickhouse-demo"), allow(dead_code))]
fn ch_criterion(table: &str) -> (&'static str, f64) {
    if table_arcsec(table) == 3.0 { ("MG-21", 80.0) } else { ("MG-14", 50.0) }
}

/// [`CH_PRESETS`], plus `--at`'s camera when it is not one of them.
#[cfg(feature = "clickhouse-demo")]
fn ch_presets(at: Preset) -> Vec<Preset> {
    let mut p = CH_PRESETS.to_vec();
    if !p.iter().any(|q| q.name == at.name) {
        p.push(at);
    }
    p
}

/// Batched tile queries MG-14 times per pass.
#[cfg(feature = "clickhouse-demo")]
const CH_QUERIES: usize = 200;

/// The batches the dot layer would send for `p`'s first frame: its plan's
/// wanted keys in order, split like `DotLayers::fetch_batch` -- one level
/// and epoch per batch, at most `max_batch` keys.
#[cfg(feature = "clickhouse-demo")]
fn preset_batches(source: &Arc<dyn GridCountSource>, p: Preset) -> Vec<Vec<GridTileKey>> {
    let mut layers = DotLayers::new();
    layers.set_source(LAYER, Arc::clone(source), dot_style(None));
    let camera = request_at(p.lon, p.lat, p.zoom).camera();
    layers.frame(&camera, &gpui_maps::render::globe::GlobeCover::FLAT, gpui_maps::timing::Instant::now());
    let mut left: Vec<GridTileKey> = layers.plan(LAYER).map(|plan| plan.wanted.clone()).unwrap_or_default();
    let max = source.max_batch();
    let mut batches = Vec::new();
    while let Some(first) = left.first().copied() {
        let (mut batch, mut rest) = (Vec::new(), Vec::new());
        for k in left {
            if batch.len() < max && k.level == first.level && k.epoch == first.epoch {
                batch.push(k);
            } else {
                rest.push(k);
            }
        }
        batches.push(batch);
        left = rest;
    }
    batches
}

/// One request's response as it crosses the wire (undecoded -- the
/// executor's own client inflates, so it cannot say): bytes, the server's
/// own `elapsed_ns` (X-ClickHouse-Summary) in ms, and the wall ms.
#[cfg(feature = "clickhouse-demo")]
fn wire(
    runtime: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    req: &gpui_maps::overlay::dotgrid::GridRequest,
) -> Result<(u64, f64, f64), String> {
    let gpui_maps::overlay::dotgrid::GridRequest::Post { url, body, headers } = req else { return Ok((0, 0.0, 0.0)) };
    runtime.block_on(async {
        let t = Instant::now();
        let mut r = client.post(url).body(body.clone());
        for (k, v) in headers {
            r = r.header(k.as_str(), v.as_str());
        }
        let resp = r.send().await.map_err(|e| format!("a ClickHouse response from {}: {}", display_url(url), e.without_url()))?;
        let summary = resp.headers().get("x-clickhouse-summary").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let bytes = resp.bytes().await.map_err(|e| format!("a body from {}: {}", display_url(url), e.without_url()))?.len() as u64;
        let wall = t.elapsed().as_secs_f64() * 1e3;
        let server = summary
            .split("\"elapsed_ns\":\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .and_then(|n| n.parse::<f64>().ok())
            .map_or(f64::NAN, |ns| ns / 1e6);
        Ok((bytes, server, wall))
    })
}

/// `--measure-clickhouse`: MG-14. [`CH_QUERIES`] batched queries spread
/// over [`CH_PRESETS`]' batches, timed through the demo's own executor
/// (request, POST, inflate, RowBinary decode) with `use_query_cache = 0`
/// (every query reads the table) -- once gzip-compressed and once plain,
/// since on localhost compression may cost more than it saves -- then
/// warm (cache on, after one priming pass). Then bytes per response, and
/// the densest tiles alone: server time, wire time, executor time.
#[cfg(feature = "clickhouse-demo")]
fn measure_clickhouse(url: &str, table: &str, at: Preset) {
    use gpui_maps::overlay::dotgrid::clickhouse::GZIP_DEFAULT;
    let connect = || {
        ClickHouseGridSource::connect(url, table, table_arcsec(table), 90.0).unwrap_or_else(|e| {
            eprintln!("world_dots --measure-clickhouse: ClickHouse {} unavailable: {e}", display_url(url));
            std::process::exit(1);
        })
    };
    let cold_gz: Arc<dyn GridCountSource> = Arc::new(connect().without_query_cache().with_gzip(true));
    let cold_plain: Arc<dyn GridCountSource> = Arc::new(connect().without_query_cache().with_gzip(false));
    let warm: Arc<dyn GridCountSource> = Arc::new(connect());
    println!(
        "world_dots --measure-clickhouse: {} gpui_map_dots.{table}, {CH_QUERIES} batched queries per pass, default gzip {GZIP_DEFAULT}",
        display_url(url)
    );

    let per_preset: Vec<(Preset, Vec<Vec<GridTileKey>>)> = ch_presets(at).into_iter().map(|p| (p, preset_batches(&warm, p))).collect();
    // Round-robin over the presets, each cycling its own batches.
    let mut queries: Vec<(usize, &Vec<GridTileKey>)> = Vec::with_capacity(CH_QUERIES);
    let mut next = vec![0usize; per_preset.len()];
    while queries.len() < CH_QUERIES {
        for (i, (_, batches)) in per_preset.iter().enumerate() {
            if queries.len() < CH_QUERIES && !batches.is_empty() {
                queries.push((i, &batches[next[i] % batches.len()]));
                next[i] += 1;
            }
        }
    }
    for (p, batches) in &per_preset {
        let keys: usize = batches.iter().map(Vec::len).sum();
        println!("  {}: {} batches, {keys} keys, levels {:?}", p.name, batches.len(), batches.iter().map(|b| b[0].level).collect::<Vec<_>>());
    }

    let time_pass = |source: &Arc<dyn GridCountSource>| -> Vec<(usize, f64, u64)> {
        queries
            .iter()
            .map(|&(i, keys)| {
                let t = Instant::now();
                let body = fetch_native::execute(&source.request(keys)).expect("a batch query");
                let tiles = source.decode(keys, &body).expect("a batch decode");
                let ms = t.elapsed().as_secs_f64() * 1e3;
                (i, ms, tiles.iter().map(|(_, t)| t.cells.len() as u64).sum())
            })
            .collect()
    };
    let report = |pass: &str, samples: &[(usize, f64, u64)]| {
        for (i, (p, _)) in per_preset.iter().enumerate() {
            let ms: Vec<f64> = samples.iter().filter(|s| s.0 == i).map(|s| s.1).collect();
            let rows: Vec<f64> = samples.iter().filter(|s| s.0 == i).map(|s| s.2 as f64).collect();
            println!(
                "dots.ch.tile_query_ms {pass} {}: n {} p50 {:.1} p95 {:.1} max {:.1} ms; rows per batch p50 {:.0} max {:.0}",
                p.name,
                ms.len(),
                quantile(&ms, 0.5),
                quantile(&ms, 0.95),
                quantile(&ms, 1.0),
                quantile(&rows, 0.5),
                quantile(&rows, 1.0)
            );
        }
        let ms: Vec<f64> = samples.iter().map(|s| s.1).collect();
        println!("dots.ch.tile_query_ms {pass} all: n {} p50 {:.1} p95 {:.1} max {:.1} ms", ms.len(), quantile(&ms, 0.5), quantile(&ms, 0.95), quantile(&ms, 1.0));
        gate(&format!("dots.ch.tile_query_ms_p50_{pass}"), quantile(&ms, 0.5), "ms");
        gate(&format!("dots.ch.tile_query_ms_p95_{pass}"), quantile(&ms, 0.95), "ms");
        quantile(&ms, 0.95)
    };

    let cold_gz_p95 = report("cold_gzip", &time_pass(&cold_gz));
    let cold_plain_p95 = report("cold_plain", &time_pass(&cold_plain));
    for (_, batches) in &per_preset {
        for keys in batches {
            fetch_native::execute(&warm.request(keys)).expect("a priming query");
        }
    }
    report("warm", &time_pass(&warm));

    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let client = reqwest::Client::builder().no_gzip().build().expect("a client");
    let mut sizes: Vec<(GridTileKey, usize)> = Vec::new();
    for (p, batches) in &per_preset {
        for (name, source) in [("gzip", &cold_gz), ("plain", &cold_plain)] {
            let mut bytes: Vec<f64> = Vec::new();
            let mut rows: u64 = 0;
            for keys in batches {
                let req = source.request(keys);
                bytes.push(wire(&runtime, &client, &req).unwrap_or_else(|e| panic!("{e}")).0 as f64);
                let body = fetch_native::execute(&req).expect("a batch query");
                for (k, t) in source.decode(keys, &body).expect("a batch decode") {
                    rows += t.cells.len() as u64;
                    if name == "gzip" {
                        sizes.push((k, t.cells.len()));
                    }
                }
            }
            let total: f64 = bytes.iter().sum();
            println!(
                "dots.ch.response_bytes {} {name}: {} batches, p50 {:.0} max {:.0} B on the wire; {} rows, {:.2} B per row",
                p.name,
                bytes.len(),
                quantile(&bytes, 0.5),
                quantile(&bytes, 1.0),
                thousands(rows),
                total / rows.max(1) as f64
            );
        }
    }
    // The densest tiles alone, 5 times each, cold: server, wire, executor.
    sizes.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    sizes.dedup_by_key(|(k, _)| *k);
    // MG-21 (3″): the densest level-0 tile's rows and bytes on the wire.
    match sizes.iter().find(|(k, _)| k.level == 0) {
        Some(&(k, rows)) => {
            let plain = wire(&runtime, &client, &cold_plain.request(&[k])).unwrap_or_else(|e| panic!("{e}")).0;
            let gz = wire(&runtime, &client, &cold_gz.request(&[k])).unwrap_or_else(|e| panic!("{e}")).0;
            println!(
                "dots.ch.densest_level0_tile: z{} {}/{}: {} rows, {} B plain, {} B gzip on the wire",
                k.tile.z,
                k.tile.x,
                k.tile.y,
                thousands(rows as u64),
                thousands(plain),
                thousands(gz)
            );
        }
        None => println!("dots.ch.densest_level0_tile: no level-0 tile in these presets' batches"),
    }
    let dense: Vec<(GridTileKey, usize)> = sizes.into_iter().take(10).collect();
    for (name, source) in [("gzip", &cold_gz), ("plain", &cold_plain)] {
        let (mut server, mut wall, mut exec, mut bytes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for &(k, _) in &dense {
            let req = source.request(&[k]);
            for _ in 0..5 {
                let (b, s, w) = wire(&runtime, &client, &req).unwrap_or_else(|e| panic!("{e}"));
                server.push(s);
                wall.push(w);
                bytes.push(b as f64);
                let t = Instant::now();
                let body = fetch_native::execute(&req).expect("a dense tile");
                source.decode(&[k], &body).expect("a dense tile decode");
                exec.push(t.elapsed().as_secs_f64() * 1e3);
            }
        }
        println!(
            "dots.ch.dense_tile {name}: {} tiles of {}..{} cells x5: server p50 {:.1} p95 {:.1} ms; wire (server + transfer) p50 {:.1} p95 {:.1} ms; executor + decode p50 {:.1} p95 {:.1} ms; {:.0} B p50 on the wire",
            dense.len(),
            thousands(dense.last().map_or(0, |d| d.1) as u64),
            thousands(dense.first().map_or(0, |d| d.1) as u64),
            quantile(&server, 0.5),
            quantile(&server, 0.95),
            quantile(&wall, 0.5),
            quantile(&wall, 0.95),
            quantile(&exec, 0.5),
            quantile(&exec, 0.95),
            quantile(&bytes, 0.5)
        );
        gate(&format!("dots.ch.dense_tile_ms_p95_{name}"), quantile(&exec, 0.95), "ms");
    }
    let cold_p95 = if GZIP_DEFAULT { cold_gz_p95 } else { cold_plain_p95 };
    let (mg, limit) = ch_criterion(table);
    println!(
        "{mg}: cold p95 {cold_p95:.1} ms per batch at the default ({}; gzip {cold_gz_p95:.1}, plain {cold_plain_p95:.1}) (criterion <= {limit} ms): {}",
        if GZIP_DEFAULT { "gzip" } else { "plain" },
        if cold_p95 <= limit { "PASS" } else { "FAIL" }
    );
}

#[cfg(not(feature = "clickhouse-demo"))]
fn measure_clickhouse(_url: &str, _table: &str, _at: Preset) {
    unreachable!("parse_args exits on --clickhouse without clickhouse-demo")
}

fn diag(source: &Arc<dyn GridCountSource>, style: &DotStyle, at: Preset, w: u32, h: u32) {
    let d = source.descriptor().clone();
    let mut request = request_at(at.lon, at.lat, at.zoom);
    request.width = w;
    request.height = h;
    let globe = if std::env::var_os("DIAG_GLOBE").is_some() {
        gpui_maps::render::globe::GlobeCover::new(&gpui_maps::render::globe::globe_frame(&request.camera()))
    } else {
        gpui_maps::render::globe::GlobeCover::FLAT
    };
    let layers = settle_on(source, style, &request, globe);
    let stats = layers.stats(LAYER).expect("the layer");
    let plan = layers.plan(LAYER).expect("a plan").clone();
    println!(
        "diag {} z{} {w}x{h}: level {} view ppd {:.1}, {} wanted, {} drawn, {} stand-ins, {} held",
        at.name,
        at.zoom,
        stats.level,
        stats.ppd,
        plan.wanted.len(),
        plan.draw.len(),
        stats.standin_tiles,
        layers.tiles_held(LAYER)
    );
    let (mut t_cells, mut t_slots, mut t_issued, mut t_alive, mut t_alive_build, mut t_bytes, mut t_first0, mut t_pop) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0f64);
    for draw in &plan.draw {
        let tile = layers.tile(LAYER, &draw.key).expect("settled");
        let ppd = stats.ppd.max(1.0);
        let built = level::ppd_build_for(ppd);
        let b = build_blocks(&d, &draw.key, &tile, built, OVERLAY_LYON_PER_UNIT_WORLD);
        let blocks = block_count(&b);
        let first0: u64 = b.buckets.iter().flatten().filter(|x| x.first_dot == 0).count() as u64;
        let issued: u64 = b
            .buckets
            .iter()
            .enumerate()
            .map(|(i, bk)| {
                u64::from(DOT_BUCKETS[i])
                    * bk.iter().filter(|x| x.first_dot == 0 || x.pop / x.first_dot as f32 > ppd * (1.0 - 1e-5)).count() as u64
            })
            .sum();
        let alive = alive_dots(&b, ppd);
        let alive_build = alive_dots(&b, built);
        let bytes = blocks.div_ceil(2048).max(1) * 2048 * 16;
        println!(
            "  {:?} L{} standin {} | cells {:>9} pop {:>12.0} | tile ppd {:>8.1} (floor {:>8.1}) built {:>8.1} | slots {:>9} blocks {:>9} first0 {:>9} | issued {:>9} alive {:>9} alive@build {:>9} | tex {:.1} MB",
            draw.key.tile,
            draw.key.level,
            draw.standin,
            tile.cells.len(),
            tile.total,
            ppd,
            tile.total / f64::from(style.budget_dots),
            built,
            b.slots,
            blocks,
            first0,
            issued,
            alive,
            alive_build,
            bytes as f64 / 1e6
        );
        t_cells += tile.cells.len() as u64;
        t_slots += b.slots;
        t_issued += issued;
        t_alive += alive;
        t_alive_build += alive_build;
        t_bytes += bytes;
        t_first0 += first0;
        t_pop += tile.total;
    }
    println!(
        "  TOTAL cells {} pop {:.0} slots {} first0 {} issued {} alive {} alive@build {} tex {:.1} MB",
        thousands(t_cells),
        t_pop,
        thousands(t_slots),
        thousands(t_first0),
        thousands(t_issued),
        thousands(t_alive),
        thousands(t_alive_build),
        t_bytes as f64 / 1e6
    );
}

/// A headless dot frame at `zoom`: the settled layer's tiles drawn at a
/// fixed `ppd`, over `basemap` (or black).
fn dot_frame(
    source: &Arc<dyn GridCountSource>,
    style: &DotStyle,
    request: &CaptureRequest,
    ppd: Option<f32>,
    ramp: Vec<Rgba>,
    basemap: Option<(&TileStore, &gpui_maps::style::Style)>,
) -> (gpui_maps::capture::Capture, gpui_maps::overlay::dotgrid::layer::DotStats) {
    dot_frame_from(&settle(source, style, request), source, style, request, ppd, ramp, basemap, None, None)
}

/// [`dot_frame`] over an already settled `layers` (`--placement-probe`
/// walks the same layers' drawn tiles after the capture).
fn dot_frame_from(
    layers: &DotLayers,
    source: &Arc<dyn GridCountSource>,
    style: &DotStyle,
    request: &CaptureRequest,
    ppd: Option<f32>,
    ramp: Vec<Rgba>,
    basemap: Option<(&TileStore, &gpui_maps::style::Style)>,
    mask: Option<(gpui_maps::render::dot_mask::MaskGrid, Vec<u8>)>,
    mask_previous: Option<gpui_maps::render::dot_mask::MaskGrid>,
) -> (gpui_maps::capture::Capture, gpui_maps::overlay::dotgrid::layer::DotStats) {
    let mut stats = layers.stats(LAYER).expect("the layer");
    let plan = layers.plan(LAYER).expect("a plan").clone();
    let ppd = ppd.unwrap_or(stats.ppd);
    // No GPU sync runs headless, so `dots_drawn` is counted here: the
    // dots alive at the frame's ppd in every drawn tile (whole tiles, not
    // just the viewport's share).
    let d = source.descriptor();
    stats.dots_drawn = plan
        .draw
        .iter()
        .map(|x| {
            let tile = layers.tile(LAYER, &x.key).expect("settled");
            alive_dots(&build_blocks(d, &x.key, &tile, level::ppd_build_for(ppd), OVERLAY_LYON_PER_UNIT_WORLD), ppd)
        })
        .sum();
    let mut request = request.clone();
    request.dots = Some(CaptureDots {
        descriptor: source.descriptor().clone(),
        tiles: plan.draw.iter().map(|x| (x.key, (*layers.tile(LAYER, &x.key).expect("settled")).clone())).collect(),
        ppd,
        ppd_build: None,
        radius_px: style.radius_px(request.zoom),
        radius_shrink: style.density_shrink,
        overlap_gamma: style.overlap_gamma,
        ramp,
        epoch_mix: style.epoch_mix,
        difference: style.ramp == DotRamp::Difference,
        exclude: None,
        tile_excludes: plan.draw.iter().map(|x| x.exclude).collect(),
        relocate: style.relocate,
        mask_k: style.mask_k,
        synthetic_mask: mask,
        mask_previous,
        category_mask: None,
    });
    let empty = gpui_maps::style::Style { layers: Vec::new(), ..gpui_maps::style::Style::default() };
    let frame = match basemap {
        Some((tiles, style)) => capture(&request, tiles, style),
        None => {
            request.clear = Clear::Sentinel(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 });
            request.backdrop = false;
            capture(&request, &TileStore::new(), &empty)
        }
    }
    .expect("a headless device");
    (frame, stats)
}

/// FO-1's `--ink-report`: `at`'s dots alone, white on black, no basemap,
/// at `size` (else 1920x1080), summarised by `ink::ink_report` over 32 px
/// blocks, with the frame's draw calls (the proof it painted) and the
/// layer's counts (which the fade must not move). The frame goes to
/// `dir/<name>_ink.png` when `dir` is given.
fn ink_report(source: &Arc<dyn GridCountSource>, style: &DotStyle, at: Preset, size: Option<(u32, u32)>, dir: Option<&Path>) {
    let mut request = request_at(at.lon, at.lat, at.zoom);
    if let Some((w, h)) = size {
        (request.width, request.height) = (w, h);
    }
    let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    let mask = relocation_mask(source, style, &request);
    println!("{}: relocate {} (mask {})", at.name, style.relocates(), if mask.is_some() { "read back from a Mapbox Streets capture" } else { "none" });
    let layers = settle(source, style, &request);
    let before = allocation_stats().draw_calls_total;
    let (frame, stats) = dot_frame_from(&layers, source, style, &request, None, vec![white], None, mask, None);
    let draw_calls = allocation_stats().draw_calls_total - before;
    let r = gpui_maps::overlay::dotgrid::ink::ink_report(&frame.rgba, frame.width, frame.height, 32);
    println!(
        "{}: gamma {} shrink {} ppd {} dots_drawn {} dots_in_view {} people_in_view {}",
        at.name,
        style.overlap_gamma,
        style.density_shrink,
        stats.ppd,
        stats.dots_drawn,
        stats.dots_in_view,
        stats.people_in_view
    );
    println!(
        "ink: blocks {} saturated_pct {:.2} deciles {} total_ink {:.0}",
        r.blocks, r.saturated_pct, r.deciles_occupied, r.total_ink
    );
    println!("draw_calls_total {draw_calls}");
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir).expect("create the capture dir");
        let path = dir.join(format!("{}_ink.png", at.name));
        frame.save_png(&path).expect("write a PNG");
        println!("-> {}", path.display());
    }
}

/// `request`'s basemap cover and, while `style` relocates, the dot mask's
/// ring past it (`render::dot_mask::mask_ring_tiles`, z >= 15): what a
/// capture fetches so its mask has a tile behind every texel an on-screen
/// cell can read.
fn cover_and_ring(source: &Arc<dyn GridCountSource>, style: &DotStyle, request: &CaptureRequest) -> Vec<gpui_maps::geo::TileId> {
    let mut all = request.cover();
    if style.relocates() {
        let cell = source.descriptor().base_arcsec / 3600.0 / 360.0;
        let ring = gpui_maps::render::dot_mask::mask_ring_tiles(&request.camera(), 0.0, cell, &all);
        all.extend(ring);
    }
    all
}

/// FO-3: with relocation on, the mask a Mapbox Streets capture of
/// `request` relocates against, read back -- the ink frame (no basemap)
/// then relocates against it as the window would. `None` without the
/// probe feature, a token, or relocation.
fn relocation_mask(
    source: &Arc<dyn GridCountSource>,
    style: &DotStyle,
    request: &CaptureRequest,
) -> Option<(gpui_maps::render::dot_mask::MaskGrid, Vec<u8>)> {
    if !cfg!(feature = "probe") || !style.relocates() {
        return None;
    }
    let (basemap_style, tiles_source, _, _) = mapbox_streets(None)?;
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let cache = std::env::temp_dir().join("gpui-map-capture-mapbox");
    let mut request = request.clone();
    request.flat_tile_zoom_max = 16;
    let all = cover_and_ring(source, style, &request);
    let tiles = runtime.block_on(fetch_cover(tiles_source.as_ref(), &all, &cache)).ok()?;
    let _ = dot_frame(source, style, &request, None, style.ramp.stops(), Some((&tiles, &basemap_style)));
    take_mask()
}

#[cfg(feature = "probe")]
fn take_mask() -> Option<(gpui_maps::render::dot_mask::MaskGrid, Vec<u8>)> {
    gpui_maps::capture::take_last_dot_mask()
}

#[cfg(not(feature = "probe"))]
fn take_mask() -> Option<(gpui_maps::render::dot_mask::MaskGrid, Vec<u8>)> {
    None
}

/// Mean coverage (red channel / 255 of a white-on-black frame) of each
/// 32x32 pixel block, row-major.
fn block_coverage(frame: &gpui_maps::capture::Capture) -> Vec<f64> {
    let (bw, bh) = (frame.width / 32, frame.height / 32);
    let mut out = Vec::with_capacity((bw * bh) as usize);
    for by in 0..bh {
        for bx in 0..bw {
            let mut sum = 0u64;
            for y in by * 32..by * 32 + 32 {
                for x in bx * 32..bx * 32 + 32 {
                    sum += u64::from(frame.pixel(x, y)[0]);
                }
            }
            out.push(sum as f64 / (1024.0 * 255.0));
        }
    }
    out
}

fn mean_abs_diff(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f64>() / a.len().max(1) as f64
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

/// MG-9's capture half. At each location and each zoom `zs` where
/// `level_for_zoom` changes, the switch pair (`zs ∓ 0.01`) against a
/// control pair the same Δz apart inside one level (`zs - 0.26`,
/// `zs - 0.24`), all four at the ppd the view has at `zs + 0.01`, white
/// dots on black so a pixel's value is its dot coverage.
fn level_switch_ab(source: &Arc<dyn GridCountSource>, style: &DotStyle, dir: &Path) {
    std::fs::create_dir_all(dir).expect("create the A/B dir");
    let d = source.descriptor().clone();
    let zc = grid::crossover_zoom(d.base_arcsec);
    let switches: Vec<f64> = (1..d.levels).map(|k| zc - f64::from(k)).filter(|z| *z - 0.26 >= 0.0).collect();
    let white = vec![Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }];
    let places = [("dhaka", 90.4125, 23.8103), ("india", 78.9, 22.0), ("sahel", 3.0, 14.0)];
    let (mut worst, mut fails) = (0.0f64, 0);
    for (name, lon, lat) in places {
        for &zs in &switches {
            let finer = settle(source, style, &request_at(lon, lat, zs + 0.01));
            let ppd = finer.stats(LAYER).expect("the layer").ppd;
            // The fade's predicted ink per dot at each level: a cell of
            // quota q draws `q - 1/8` dots of ink (q >= 1/4) or `2 q^2`
            // (q < 1/4), the integral of `saturate(4 y)` up to q.
            let fade_ink = |layers: &DotLayers| {
                let (mut ink, mut dots) = (0f64, 0f64);
                for draw in &layers.plan(LAYER).expect("a plan").draw {
                    for &(_, pop, _) in &layers.tile(LAYER, &draw.key).expect("settled").cells {
                        let q = f64::from(pop / ppd);
                        ink += if q >= 0.25 { q - 0.125 } else { 2.0 * q * q };
                        dots += q;
                    }
                }
                ink / dots.max(1e-12)
            };
            let coarser = settle(source, style, &request_at(lon, lat, zs - 0.01));
            let (fine_ink, coarse_ink) = (fade_ink(&finer), fade_ink(&coarser));
            let zc = grid::crossover_zoom(d.base_arcsec);
            let cell_px = |level: u8, z: f64| {
                let arc = d.base_arcsec * f64::from(1u32 << level) / 3600.0;
                arc / 360.0 * 256.0 * 2f64.powf(z) / lat.to_radians().cos()
            };
            let (lf, lc) = (level::level_for_zoom(&d, zs + 0.01), level::level_for_zoom(&d, zs - 0.01));
            println!(
                "{name:<6} zs {zs:.2} (crossover {zc:.3}): cell {:.3} px (level {lf}) / {:.3} px (level {lc}); fade ink per dot fine {fine_ink:.4} coarse {coarse_ink:.4} -> predicted coarse/fine ink {:+.1} %",
                cell_px(lf, zs + 0.01),
                cell_px(lc, zs - 0.01),
                (coarse_ink / fine_ink - 1.0) * 100.0
            );
            let zooms = [zs - 0.01, zs + 0.01, zs - 0.26, zs - 0.24];
            let mut cov = Vec::new();
            let mut levels = Vec::new();
            for (i, z) in zooms.iter().enumerate() {
                let (frame, stats) = dot_frame(source, style, &request_at(lon, lat, *z), Some(ppd), white.clone(), None);
                let tag = ["switch_below", "switch_above", "control_a", "control_b"][i];
                frame.save_png(&dir.join(format!("{name}_zs{zs:.2}_{tag}.png"))).expect("write a PNG");
                levels.push(stats.level);
                cov.push(block_coverage(&frame));
            }
            let switch = mean_abs_diff(&cov[0], &cov[1]);
            let control = mean_abs_diff(&cov[2], &cov[3]);
            // Discriminator: the switch pair with the coarse frame's ink
            // scaled to the fine frame's mean. What is left is the dots
            // moving (a new level hashes new positions), not the fade's
            // ink bias.
            let k = mean(&cov[1]) / mean(&cov[0]).max(1e-12);
            let equalised: Vec<f64> = cov[0].iter().map(|c| c * k).collect();
            let residual = mean_abs_diff(&equalised, &cov[1]);
            let ratio = if control > 0.0 { switch / control } else if switch > 0.0 { f64::INFINITY } else { 1.0 };
            let pass = ratio <= 1.5;
            fails += usize::from(!pass);
            worst = worst.max(ratio);
            println!(
                "{name:<6} zs {zs:.2} (level {} -> {}) ppd {ppd:>9.1} | ink below {:.4} above {:.4} ({:+.1} %) | switch MAD {switch:.5} (ink equalised {residual:.5}) control MAD {control:.5} (levels {}/{}) | ratio {ratio:.2} (equalised {:.2}) {}",
                levels[1],
                levels[0],
                mean(&cov[0]),
                mean(&cov[1]),
                (mean(&cov[0]) / mean(&cov[1]).max(1e-12) - 1.0) * 100.0,
                levels[2],
                levels[3],
                residual / control.max(1e-12),
                if pass { "PASS" } else { "FAIL" }
            );
            gate(&format!("dots.level_switch.{name}.zs{zs:.2}.ratio"), ratio, "ratio");
        }
    }
    println!("level-switch A/B: worst ratio {worst:.2}, {fails} of {} pairs over 1.5", switches.len() * places.len());
    gate("dots.level_switch.worst_ratio", worst, "ratio");
}

/// One probed frame: the capture, its layers' stats, and every live dot
/// of the drawn tiles inside the mask placed by the CPU mirror
/// (`overlay::dotgrid::placement`) against the mask the capture drew with:
/// relocated (the tile's own K, as `DotStyle::relocated` picks it) and
/// not (candidate 0, today's uniform place, K 1).
#[cfg(feature = "probe")]
struct Probed {
    frame: gpui_maps::capture::Capture,
    stats: gpui_maps::overlay::dotgrid::layer::DotStats,
    on: gpui_maps::overlay::dotgrid::placement::PlacementReport,
    off: gpui_maps::overlay::dotgrid::placement::PlacementReport,
    /// (cell key, dot) -> chosen candidate, relocated.
    chosen: std::collections::HashMap<(u32, u32), u8>,
    /// cell key -> the cell's SW corner in unit world.
    cells: std::collections::HashMap<u32, (f64, f64)>,
    /// Drawn tiles that relocated (K > 1) of those drawn.
    relocating: (usize, usize),
    draw_calls: u64,
    grid: gpui_maps::render::dot_mask::MaskGrid,
    texels: Vec<u8>,
}

#[cfg(feature = "probe")]
fn probe_frame(
    source: &Arc<dyn GridCountSource>,
    style: &DotStyle,
    request: &CaptureRequest,
    tiles: &TileStore,
    basemap_style: &gpui_maps::style::Style,
    previous: Option<gpui_maps::render::dot_mask::MaskGrid>,
) -> Option<Probed> {
    use gpui_maps::overlay::dotgrid::placement::{chosen_map, placement_report, PlacementReport};
    use gpui_maps::render::dots::{px_per_lyon, relocation_k, MASK_K_MAX};
    let style = DotStyle { relocate: true, ..style.clone() };
    let layers = settle(source, &style, request);
    let before = allocation_stats().draw_calls_total;
    let (frame, stats) = dot_frame_from(&layers, source, &style, request, None, style.ramp.stops(), Some((tiles, basemap_style)), None, previous);
    let draw_calls = allocation_stats().draw_calls_total - before;
    let Some((grid, texels)) = gpui_maps::capture::take_last_dot_mask() else {
        println!("probe: the capture drew no mask: relocation is off (below z15, pitch above 30 deg, a globe frame, cells under 2 px) or no basemap -- 0 dots move by construction");
        return None;
    };
    let d = source.descriptor();
    let upl = 1.0 / OVERLAY_LYON_PER_UNIT_WORLD;
    let ppd = stats.ppd;
    let (mut on, mut off) = (PlacementReport::default(), PlacementReport::default());
    let mut chosen = std::collections::HashMap::new();
    let mut cells = std::collections::HashMap::new();
    let mut relocating = (0, 0);
    for draw in &layers.plan(LAYER).expect("a plan").draw {
        let tile = layers.tile(LAYER, &draw.key).expect("settled");
        let all = build_blocks(d, &draw.key, &tile, level::ppd_build_for(ppd), OVERLAY_LYON_PER_UNIT_WORLD);
        let t = draw.key.tile;
        let n = f64::from(1u32 << t.z);
        let origin = (f64::from(t.x) / n, f64::from(t.y) / n);
        let world = |o: [f32; 2]| (origin.0 + f64::from(o[0]) * upl, origin.1 + f64::from(o[1]) * upl);
        // The cells on screen (any part of the cell in the frame), what the
        // gates judge: the mask holds every one of them (its margin is one
        // cell), off-screen margin cells can straddle its edge, and a
        // candidate off the mask has no data (its dot stays at candidate 0).
        let cell_unit = f64::from(all.cell_width) * upl;
        let vp = request.camera().view_projection();
        let ws = f64::from(gpui_maps::render::WORLD_SIZE);
        let (w, h) = (request.width as f32, request.height as f32);
        let screen = |x: f64, y: f64| {
            let c = vp * glam::Vec4::new((x * ws) as f32, (y * ws) as f32, 0.0, 1.0);
            (c.w > 0.0).then(|| glam::Vec2::new((c.x / c.w + 1.0) * 0.5 * w, (1.0 - c.y / c.w) * 0.5 * h))
        };
        let on_screen = |o: [f32; 2]| {
            let (wx, wy) = world(o);
            let north = wy - cell_unit * (std::f64::consts::PI * (1.0 - 2.0 * wy)).cosh();
            let corners = [screen(wx, wy), screen(wx + cell_unit, wy), screen(wx, north), screen(wx + cell_unit, north)];
            let Some(c) = corners.into_iter().collect::<Option<Vec<_>>>() else { return false };
            let (x0, x1) = (c.iter().map(|p| p.x).fold(f32::INFINITY, f32::min), c.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max));
            let (y0, y1) = (c.iter().map(|p| p.y).fold(f32::INFINITY, f32::min), c.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max));
            x1 >= 0.0 && x0 <= w && y1 >= 0.0 && y0 <= h
        };
        let buckets = all.buckets.clone().map(|bucket| bucket.into_iter().filter(|x| on_screen(x.origin)).collect());
        let blocks = TileBlocks { buckets, pop_b: Default::default(), ..all };
        for block in blocks.buckets.iter().flatten() {
            cells.insert(hash::cell_key(block.origin), world(block.origin));
        }
        let cell_px = blocks.cell_width * px_per_lyon(request.zoom as f32, upl as f32);
        let k = relocation_k(cell_px, true, style.relocate, style.mask_k.min(MASK_K_MAX)).max(1);
        relocating.1 += 1;
        relocating.0 += usize::from(k > 1);
        on.add(&placement_report(&blocks, origin, upl, ppd, k, &grid, &texels));
        off.add(&placement_report(&blocks, origin, upl, ppd, 1, &grid, &texels));
        chosen.extend(chosen_map(&blocks, origin, upl, ppd, k, &grid, &texels));
    }
    Some(Probed { frame, stats, on, off, chosen, cells, relocating, draw_calls, grid, texels })
}

/// The two frames' masks over the texels both cover (in world texels):
/// how many differ, by (rank a, rank b). A world-anchored mask over the
/// same basemap differs nowhere.
#[cfg(feature = "probe")]
fn mask_diff(a: &Probed, b: &Probed) {
    let (ga, gb) = (&a.grid, &b.grid);
    if ga.zm != gb.zm {
        println!("  mask: zm {} -> {}", ga.zm, gb.zm);
        return;
    }
    let x0 = ga.origin[0].max(gb.origin[0]);
    let y0 = ga.origin[1].max(gb.origin[1]);
    let x1 = (ga.origin[0] + i64::from(ga.size[0])).min(gb.origin[0] + i64::from(gb.size[0]));
    let y1 = (ga.origin[1] + i64::from(ga.size[1])).min(gb.origin[1] + i64::from(gb.size[1]));
    let (mut n, mut diff) = (0u64, std::collections::BTreeMap::<(u8, u8), u64>::new());
    let (mut rows, mut cols) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
    for y in y0..y1 {
        for x in x0..x1 {
            let ta = a.texels[((y - ga.origin[1]) * i64::from(ga.size[0]) + (x - ga.origin[0])) as usize];
            let tb = b.texels[((y - gb.origin[1]) * i64::from(gb.size[0]) + (x - gb.origin[0])) as usize];
            n += 1;
            if ta != tb {
                *diff.entry((ta, tb)).or_default() += 1;
                rows.insert(y);
                cols.insert(x);
            }
        }
    }
    let total: u64 = diff.values().sum();
    println!(
        "  mask: zm {} origin {:?} -> {:?} size {:?} | overlap {n} texels, {total} differ ({:.3} %) {diff:?} | rows {:?}..{:?} cols {:?}..{:?}",
        ga.zm, ga.origin, gb.origin, gb.size, pct(total, n), rows.first(), rows.last(), cols.first(), cols.last()
    );
}

/// DP-5's counts between two probed frames.
#[cfg(feature = "probe")]
#[derive(Default)]
struct ChangeCount {
    common: u64,
    changed: u64,
    interior: u64,
    interior_changed: u64,
    visible: u64,
    visible_changed: u64,
    /// Changed dots whose old or new place is on screen: a visible jump.
    popped: u64,
    /// Each changed on-screen dot's cell width on screen.
    px: Vec<f64>,
}

#[cfg(feature = "probe")]
impl ChangeCount {
    fn add(&mut self, o: &ChangeCount) {
        self.common += o.common;
        self.changed += o.changed;
        self.interior += o.interior;
        self.interior_changed += o.interior_changed;
        self.visible += o.visible;
        self.visible_changed += o.visible_changed;
        self.popped += o.popped;
        self.px.extend_from_slice(&o.px);
    }
}

#[cfg(feature = "probe")]
fn pct(n: u64, of: u64) -> f64 {
    100.0 * n as f64 / of.max(1) as f64
}

/// The `placement:` line for one report.
#[cfg(feature = "probe")]
fn placement_line(tag: &str, r: &gpui_maps::overlay::dotgrid::placement::PlacementReport, changed: Option<(u64, u64)>) {
    println!(
        "placement[{tag}]: live {} in_building {:.2} % on_water {:.2} % on_green {:.2} % analytic {:.2} % avoidable {} changed {}",
        r.live,
        pct(r.in_building, r.live),
        pct(r.on_water, r.live),
        pct(r.on_green, r.live),
        100.0 * r.analytic_in_building / r.live.max(1) as f64,
        r.avoidable_on_water,
        changed.map_or_else(|| "-".to_string(), |(c, of)| format!("{c} of {of} ({:.4} %)", pct(c, of)))
    );
}

/// Unit-world centre of `lon, lat`, and back.
#[cfg(feature = "probe")]
fn to_unit(lon: f64, lat: f64) -> (f64, f64) {
    let y = (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln();
    ((lon + 180.0) / 360.0, 0.5 - y / (2.0 * std::f64::consts::PI))
}

#[cfg(feature = "probe")]
fn from_unit(x: f64, y: f64) -> (f64, f64) {
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
    (x * 360.0 - 180.0, lat)
}

/// `--placement-probe` (headless, Mapbox Streets, relocation forced on):
/// the CPU mirror of `vs_dot` against the mask each capture drew with.
/// - alone: DP-4 at `--at`/`--view` -- the relocated and the uniform
///   placement's shares, and the criteria;
/// - `--pan`: DP-5's steady pan, 12 frames of (97, -31) px (every frame
///   re-plans its mask: a stricter case than the window's, which re-keys
///   only past 256 texels), `--pitch P` tilted; the dots whose chosen
///   candidate changed between frames, with their cells' px on screen;
/// - `--zoom-sweep`: DP-5's events -- the integer crossing above the view's
///   zoom (-0.02 -> +0.02), and a tile arrival (the basemap tile under the
///   centre missing, then present).
#[cfg(feature = "probe")]
fn placement_probe(source: &Arc<dyn GridCountSource>, style: &DotStyle, args: &Args) {
    let (basemap_style, tiles_source, _, _) = mapbox_streets(args.worldview.as_deref()).expect("--placement-probe draws Mapbox Streets: set MAPBOX_ACCESS_TOKEN");
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let cache = std::env::temp_dir().join("gpui-map-capture-mapbox");
    let at = args.at;
    let request_for = |lon: f64, lat: f64, zoom: f64| {
        let mut request = request_at(lon, lat, zoom);
        if let Some((w, h)) = args.size {
            (request.width, request.height) = (w, h);
        }
        request.pitch = args.pitch.to_radians();
        request.flat_tile_zoom_max = 16;
        request
    };
    // The cover, and the mask's ring past it (`dot_mask::mask_ring_tiles`,
    // DP-5 ruling: z >= 15, rasterised into the mask only), whose count and
    // bytes are reported once.
    let ring_reported = std::cell::Cell::new(false);
    let fetch = |request: &CaptureRequest| {
        let cover = request.cover();
        let cell = source.descriptor().base_arcsec / 3600.0 / 360.0;
        let ring = if style.relocates() {
            gpui_maps::render::dot_mask::mask_ring_tiles(&request.camera(), 0.0, cell, &cover)
        } else {
            Vec::new()
        };
        let all: Vec<_> = cover.iter().chain(&ring).copied().collect();
        let tiles = runtime.block_on(fetch_cover(tiles_source.as_ref(), &all, &cache)).expect("basemap tiles");
        if !ring_reported.replace(true) {
            let bytes = |t: &[gpui_maps::geo::TileId]| -> u64 {
                t.iter().filter_map(|t| std::fs::metadata(cache.join(format!("{}-{}-{}.pbf", t.z, t.x, t.y))).ok()).map(|m| m.len()).sum()
            };
            println!(
                "mask ring: {} tiles past the cover's {} ({} vs {} bytes fetched)",
                ring.len(),
                cover.len(),
                bytes(&ring),
                bytes(&cover)
            );
        }
        tiles
    };
    let save = |p: &Probed, tag: &str| {
        if let Some(dir) = &args.capture {
            std::fs::create_dir_all(dir).expect("create the capture dir");
            let path = dir.join(format!("{}_{tag}.png", at.name));
            p.frame.save_png(&path).expect("write a PNG");
            println!("-> {}", path.display());
        }
    };
    let head = |p: &Probed| {
        println!(
            "{}: pitch {} ppd {:.2} dots_in_view {} people_in_view {:.0} | {} of {} drawn tiles relocate (K {}) | draw_calls_total {}",
            at.name,
            args.pitch,
            p.stats.ppd,
            p.stats.dots_in_view,
            p.stats.people_in_view,
            p.relocating.0,
            p.relocating.1,
            style.mask_k,
            p.draw_calls
        );
    };
    // The dots whose chosen candidate differs between two frames: of the
    // dots in both, of those whose whole cell is inside both masks (a
    // cell's candidates off a mask read land, so a mask edge moving over a
    // cell re-places it), and of those whose cell is on `b`'s screen --
    // with each changed on-screen cell's width in px (east edge minus
    // west, at its SW corner).
    let changes = |a: &Probed, b: &Probed, request: &CaptureRequest| {
        let vp = request.camera().view_projection();
        let ws = f64::from(gpui_maps::render::WORLD_SIZE);
        let cell_unit = source.descriptor().base_arcsec / 3600.0 / 360.0;
        let (w, h) = (request.width as f32, request.height as f32);
        let screen = |x: f64, y: f64| {
            let c = vp * glam::Vec4::new((x * ws) as f32, (y * ws) as f32, 0.0, 1.0);
            (c.w > 0.0).then(|| glam::Vec2::new((c.x / c.w + 1.0) * 0.5 * w, (1.0 - c.y / c.w) * 0.5 * h))
        };
        let inside = |g: &gpui_maps::render::dot_mask::MaskGrid, x: f64, y: f64, north: f64| {
            g.texel_of(x, y).is_some() && g.texel_of(x + cell_unit, north).is_some()
        };
        let mut c = ChangeCount::default();
        for (key, j) in &a.chosen {
            let Some(other) = b.chosen.get(key) else { continue };
            let Some(&(x, y)) = b.cells.get(&key.0) else { continue };
            let north = y - cell_unit * (std::f64::consts::PI * (1.0 - 2.0 * y)).cosh();
            let interior = inside(&a.grid, x, y, north) && inside(&b.grid, x, y, north);
            let corners = [screen(x, y), screen(x + cell_unit, north)];
            let on_screen = match corners {
                [Some(p), Some(q)] => p.x.max(q.x) >= 0.0 && p.x.min(q.x) <= w && p.y.max(q.y) >= 0.0 && p.y.min(q.y) <= h,
                _ => false,
            };
            let moved = other != j;
            // The dot's place under candidate `j` (`dot_at` in unit world).
            let place = |j: u8| {
                let k = u32::from(j) + 1;
                let (u, v) = hash::dot_candidates(key.0, key.1, k).nth(usize::from(j)).expect("candidate j");
                let stretch = (std::f64::consts::PI * (1.0 - 2.0 * y)).cosh();
                screen(x + f64::from(u) * cell_unit, y - f64::from(v) * cell_unit * stretch)
            };
            let seen = |p: Option<glam::Vec2>| p.is_some_and(|p| (0.0..=w).contains(&p.x) && (0.0..=h).contains(&p.y));
            if moved && (seen(place(*j)) || seen(place(*other))) {
                c.popped += 1;
            }
            c.common += 1;
            c.changed += u64::from(moved);
            c.interior += u64::from(interior);
            c.interior_changed += u64::from(interior && moved);
            c.visible += u64::from(on_screen);
            c.visible_changed += u64::from(on_screen && moved);
            if on_screen && moved {
                if let (Some(p), Some(q)) = (screen(x, y), screen(x + cell_unit, y)) {
                    c.px.push(f64::from((q - p).length()));
                }
            }
        }
        c
    };
    let change_line = |label: &str, c: &ChangeCount| {
        println!(
            "{label}: changed {} of {} common ({:.4} %) | cells inside both masks {} of {} | cells on screen {} of {} ({:.4} %), changed cells {:.1}..{:.1} px | popped on screen (old or new place) {}",
            c.changed,
            c.common,
            pct(c.changed, c.common),
            c.interior_changed,
            c.interior,
            c.visible_changed,
            c.visible,
            pct(c.visible_changed, c.visible),
            c.px.iter().copied().fold(f64::INFINITY, f64::min),
            c.px.iter().copied().fold(0.0, f64::max),
            c.popped
        );
    };
    if args.pan {
        let (mut x, mut y) = to_unit(at.lon, at.lat);
        let unit_per_px = 1.0 / (gpui_maps::geo::TILE_SIZE * 2f64.powf(at.zoom));
        let mut prev: Option<Probed> = None;
        let mut totals = ChangeCount::default();
        for frame in 0..12 {
            let (lon, lat) = from_unit(x, y);
            let request = request_for(lon, lat, at.zoom);
            let tiles = fetch(&request);
            // The previous frame's grid, as a map's bound mask gives it.
            let Some(p) = probe_frame(source, style, &request, &tiles, &basemap_style, prev.as_ref().map(|p| p.grid)) else { return };
            if frame == 0 {
                head(&p);
                save(&p, &format!("pitch{}_pan0", args.pitch));
            }
            let delta = prev.as_ref().map(|a| changes(a, &p, &request));
            if let Some(c) = &delta {
                change_line(&format!("frame {frame}"), c);
                mask_diff(prev.as_ref().expect("a previous frame"), &p);
                totals.add(c);
            }
            placement_line(&format!("pan {frame}"), &p.on, delta.as_ref().map(|d| (d.changed, d.common)));
            prev = Some(p);
            x += 97.0 * unit_per_px;
            y -= 31.0 * unit_per_px;
        }
        change_line("pan total", &totals);
        let total_changed = totals.visible_changed;
        gate(&format!("dots.placement.{}.pitch{}.pan_changed", at.name, args.pitch), total_changed as f64, "count");
        return;
    }
    if args.zoom_sweep {
        let zc = at.zoom.floor() + 1.0;
        let (below, above) = (request_for(at.lon, at.lat, zc - 0.02), request_for(at.lon, at.lat, zc + 0.02));
        let (a, b) = (probe_frame(source, style, &below, &fetch(&below), &basemap_style, None), probe_frame(source, style, &above, &fetch(&above), &basemap_style, None));
        if let (Some(a), Some(b)) = (a, b) {
            change_line(&format!("crossing z{zc}"), &changes(&a, &b, &above));
        }
        // The 30 deg crossing (relocation is off above it): every dot a
        // 29.5 deg frame relocates (chosen candidate != 0) goes back to its
        // uniform place when the camera tilts past 30.
        let mut tilted = request_for(at.lon, at.lat, at.zoom);
        tilted.pitch = 29.5f32.to_radians();
        if let Some(p) = probe_frame(source, style, &tilted, &fetch(&tilted), &basemap_style, None) {
            let moved = p.chosen.values().filter(|&&j| j != 0).count();
            println!("crossing 30 deg pitch: {moved} of {} on-screen dots move to their uniform place ({:.2} %)", p.chosen.len(), pct(moved as u64, p.chosen.len() as u64));
        }
        let request = request_for(at.lon, at.lat, at.zoom);
        let full = fetch(&request);
        let (cx, cy) = to_unit(at.lon, at.lat);
        let mut holed = full.clone();
        let centre = full.keys().filter(|t| {
            let n = f64::from(1u32 << t.z);
            (f64::from(t.x)..f64::from(t.x + 1)).contains(&(cx * n)) && (f64::from(t.y)..f64::from(t.y + 1)).contains(&(cy * n))
        }).max_by_key(|t| t.z).copied();
        if let Some(t) = centre {
            holed.remove(&t);
            let a = probe_frame(source, style, &request, &holed, &basemap_style, None);
            let b = probe_frame(source, style, &request, &full, &basemap_style, None);
            if let (Some(a), Some(b)) = (a, b) {
                change_line(&format!("arrival of {}/{}/{}", t.z, t.x, t.y), &changes(&a, &b, &request));
            }
        }
        return;
    }
    let request = request_for(at.lon, at.lat, at.zoom);
    let tiles = fetch(&request);
    let Some(p) = probe_frame(source, style, &request, &tiles, &basemap_style, None) else { return };
    head(&p);
    placement_line("relocate on", &p.on, None);
    placement_line("relocate off", &p.off, None);
    let share = |n: u64, r: &gpui_maps::overlay::dotgrid::placement::PlacementReport| pct(n, r.live);
    let analytic = 100.0 * p.on.analytic_in_building / p.on.live.max(1) as f64;
    let building = share(p.on.in_building, &p.on) >= analytic - 2.0;
    let water = share(p.on.on_water, &p.on) <= 0.1 * share(p.off.on_water, &p.off) && p.on.avoidable_on_water == 0;
    let green = share(p.on.on_green, &p.on) <= 0.25 * share(p.off.on_green, &p.off);
    let verdict = |b: bool| if b { "PASS" } else { "FAIL" };
    println!(
        "DP-4 {}: in_building {:.2} % vs analytic {analytic:.2} % - 2 pp {} | water {:.3} % vs off {:.3} % (<= 10 %), avoidable {} {} | green {:.3} % vs off {:.3} % (<= 25 %) {}",
        at.name,
        share(p.on.in_building, &p.on),
        verdict(building),
        share(p.on.on_water, &p.on),
        share(p.off.on_water, &p.off),
        p.on.avoidable_on_water,
        verdict(water),
        share(p.on.on_green, &p.on),
        share(p.off.on_green, &p.off),
        verdict(green)
    );
    save(&p, "probe");
}

#[cfg(not(feature = "probe"))]
fn placement_probe(_: &Arc<dyn GridCountSource>, _: &DotStyle, _: &Args) {
    panic!("--placement-probe needs --features probe");
}

/// Each of `views`, headless, over Mapbox Streets, to `dir/<name>.png`
/// (at `size`, else 1920x1080) -- and, with `no_dots`, the same frame
/// without the dots to `dir/<name>_nodots.png`.
fn capture_presets(
    source: &Arc<dyn GridCountSource>,
    style: &DotStyle,
    dir: &Path,
    views: &[Preset],
    size: Option<(u32, u32)>,
    pixel_ratio: f32,
    no_dots: bool,
    mapbox: bool,
    worldview: Option<&str>,
) {
    std::fs::create_dir_all(dir).expect("create the capture dir");
    let (basemap_style, tiles_source, cache) = if mapbox {
        let (style, tiles, _, _) = mapbox_streets(worldview).expect("--mapbox-streets draws Mapbox Streets: set MAPBOX_ACCESS_TOKEN");
        (style, tiles, "gpui-map-capture-mapbox")
    } else {
        let b = routes_open();
        (b.style, b.source, "gpui-map-capture-routes-open")
    };
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let cache = std::env::temp_dir().join(cache);
    for &p in views {
        let mut request = request_at(p.lon, p.lat, p.zoom);
        if let Some((w, h)) = size {
            (request.width, request.height) = (w, h);
        }
        request.pixel_ratio = pixel_ratio;
        request.flat_tile_zoom_max = 16;
        let all = cover_and_ring(source, style, &request);
        let tiles = runtime.block_on(fetch_cover(tiles_source.as_ref(), &all, &cache)).expect("basemap tiles");
        if no_dots {
            let path = dir.join(format!("{}_nodots.png", p.name));
            capture(&request, &tiles, &basemap_style).expect("a headless device").save_png(&path).expect("write a PNG");
            println!("{}: the basemap alone -> {}", p.name, path.display());
        }
        let (frame, stats) = dot_frame(source, style, &request, None, style.ramp.stops(), Some((&tiles, &basemap_style)));
        let path = dir.join(format!("{}.png", p.name));
        frame.save_png(&path).expect("write a PNG");
        println!(
            "{}: {} people in view · {} dots · 1 dot = {} people ({} dots in the drawn tiles, level {}, {} of {} tiles, {} basemap tiles) -> {}",
            p.name,
            people_text(stats.people_in_view),
            thousands(stats.dots_in_view),
            thousands(stats.ppd.round() as u64),
            thousands(stats.dots_drawn),
            stats.level,
            stats.tiles_ready,
            stats.tiles_wanted,
            tiles.len(),
            path.display()
        );
    }
}

// ---------------------------------------------------------------- windowed

fn windowed(args: Args, process_start: Instant) {
    println!("Drag to pan, scroll to zoom. Every person on Earth as a dot (GHS-POP R2023A, 2025).");
    println!(
        "Keys: {} day/night terminator, {} globe or flat, N north (shift+N also flat), shift+arrows rotate/tilt, C coverage, D tile debug.",
        shell::TERMINATOR_KEY.to_uppercase(),
        shell::GLOBE_KEY.to_uppercase()
    );
    let DotSource { source, synthetic, note } = pick_source(args.clickhouse.as_deref(), &args.table, args.grid.as_deref(), args.synthetic);
    let style = style_of(&args.style);
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime");
    let handle = runtime.handle().clone();
    let mapbox = args.mapbox_streets || args.worldview.is_some();
    let (basemap_style, tiles, sprites) = match if args.no_basemap || !mapbox { None } else { mapbox_streets(args.worldview.as_deref()) } {
        None if args.no_basemap => {
            println!("basemap: none (--no-basemap)");
            let style = gpui_maps::style::Style { layers: Vec::new(), sprite: None, sky: Default::default() };
            (style, Arc::new(gpui_maps::tile_source::EmptyTileSource) as Arc<dyn TileSource>, None)
        }
        Some((style, tiles, sprite, token)) => {
            let sprites = sprite
                .and_then(|s| gpui_maps::sprite::resolve_sprite_url(&s, Some(&token)).ok())
                .and_then(|url| fetch_sprites_blocking(&handle, &url).ok())
                .map(Arc::new);
            println!("basemap: Mapbox Streets");
            (style, tiles, sprites)
        }
        None => {
            if mapbox {
                println!("basemap: SWAT Routes Dark (open) (--mapbox-streets needs MAPBOX_ACCESS_TOKEN)");
            } else {
                println!("basemap: SWAT Routes Dark (open) (--mapbox-streets for Mapbox Streets)");
            }
            let b = routes_open();
            let sprites = b
                .sprite
                .and_then(|s| gpui_maps::sprite::resolve_sprite_url(&s, None).ok())
                .and_then(|url| fetch_sprites_blocking(&handle, &url).ok())
                .map(Arc::new);
            (b.style, b.source, sprites)
        }
    };
    if style.epoch_b.is_some() {
        println!("Epochs: space animates {} -> {} over 2 s (and back), [ ] step the second epoch, shift+D the difference ramp.", style.epoch, style.epoch_b.unwrap_or(style.epoch));
    }
    let epochs: Vec<u16> = source.as_ref().map(|s| s.descriptor().epochs.clone()).unwrap_or_default();
    let animate_epochs = args.animate_epochs;
    if animate_epochs.is_some() && style.epoch_b.is_none() {
        eprintln!("world_dots: --animate-epochs needs --epochs A,B");
        std::process::exit(2);
    }
    let at = args.at;
    let pan = args.pan;
    let sweep = args.zoom_sweep;
    let quit_after = args.quit_after;
    let pitch = args.pitch;
    let mut init = Some((basemap_style, tiles, sprites, source, style));
    let note_for_root = note.clone();

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let mut roots = None;
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("World dots".into()), ..Default::default() }),
                ..Default::default()
            },
            |_window, cx| {
                let (basemap_style, tiles, sprites, source, style) = init.take().expect("one window");
                let map = cx.new(|_| {
                    let mut view = MapView::new(at.lon, at.lat, at.zoom, 1280.0, 800.0, basemap_style, tiles, handle);
                    view.set_chrome_above_map(true);
                    if pitch != 0.0 {
                        view.jump_to(gpui_maps::camera_control::CameraTarget { pitch: Some(pitch.to_radians()), ..Default::default() });
                    }
                    if let Some(s) = sprites {
                        view.set_sprites(s);
                    }
                    view
                });
                if let Some(source) = source {
                    map.update(cx, |view, cx| view.set_dot_density_source(LAYER, source, style, cx));
                }
                let label = synthetic.then_some(SYNTHETIC_LABEL);
                let counter = counter_text(None, note_for_root.as_deref());
                let root = cx.new(|cx| DemoRoot { epochs: epochs.clone(), ..DemoRoot::new(map.clone(), counter, label, true, cx) });
                roots = Some((root.clone(), map));
                root
            },
        )
        .expect("failed to open window");
        // Frontmost, or the window server never asks it to paint.
        cx.activate(true);
        let (root, map) = roots.expect("the window was built");
        let opened = Instant::now();

        if pan {
            let map = map.clone();
            cx.spawn(async move |cx| {
                let started = Instant::now();
                loop {
                    cx.background_executor().timer(Duration::from_millis(16)).await;
                    let t = started.elapsed().as_secs_f64();
                    let lon = at.lon + 6.0 * (t * 0.25).sin();
                    let lat = at.lat + 2.0 * (t * 0.17).sin();
                    map
                        .update(cx, |view, cx| {
                            view.set_camera(lon, lat, at.zoom);
                            cx.notify();
                        });
                }
            })
            .detach();
        }
        if sweep {
            zoom_sweep_task(map.clone(), at, cx);
        }
        if let Some(n) = animate_epochs {
            epochs_task(map.clone(), at, pitch, n, cx);
        }

        cx.spawn(async move |cx| {
            let mut first_dots: Option<Duration> = None;
            let mut last_counter = Instant::now();
            let mut last_report = Instant::now();
            let mut last_paints = gpui_maps::render::paint_census().paints_submitted;
            let (mut gpu_max, mut rss_max) = (0u64, 0u64);
            #[allow(unused_mut, unused_variables)]
            let mut pan_window: Option<(Instant, u64)> = None;
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                let stats = map.read_with(cx, |v, _| v.dot_stats(LAYER));
                if let Some(s) = stats {
                    gpu_max = gpu_max.max(s.gpu_bytes);
                }
                rss_max = rss_max.max(gpui_maps::render::rss_bytes());
                if first_dots.is_none() && stats.is_some_and(|s| s.dots_drawn > 0) {
                    let t = process_start.elapsed();
                    first_dots = Some(t);
                    println!(
                        "first dots: {:.0} ms after process start ({:.0} ms after the window opened)",
                        t.as_secs_f64() * 1e3,
                        opened.elapsed().as_secs_f64() * 1e3
                    );
                    gate("dots.first_dots_ms", t.as_secs_f64() * 1e3, "ms");
                }
                if last_counter.elapsed() >= Duration::from_millis(250) {
                    last_counter = Instant::now();
                    let text = map.read_with(cx, |v, _| counter_line(v, note.as_deref()));
                    root.update(cx, |r, cx| {
                        if r.counter != text {
                            r.counter = text;
                            cx.notify();
                        }
                    });
                }
                if last_report.elapsed() >= Duration::from_secs(5) {
                    let secs = last_report.elapsed().as_secs_f64();
                    last_report = Instant::now();
                    let census = gpui_maps::render::paint_census();
                    let fps = (census.paints_submitted - last_paints) as f64 / secs;
                    last_paints = census.paints_submitted;
                    let a = allocation_stats();
                    if let Some(s) = stats {
                        println!(
                            "[{:>5.1}s] {fps:.1} fps | {} dots, ppd {:.1}, level {}, tiles {}/{} ({} stand-ins), dot gpu {:.1} MB, errors {} {:?} | draws total {} | rss {:.0} MB | drawn {} gpu {} slots {} pop {:.0} M built {:.1}..{:.1} in view {} people {} dots",
                            opened.elapsed().as_secs_f64(),
                            thousands(s.dots_drawn),
                            s.ppd,
                            s.level,
                            s.tiles_ready,
                            s.tiles_wanted,
                            s.standin_tiles,
                            s.gpu_bytes as f64 / 1e6,
                            s.source_errors,
                            s.last_error,
                            a.draw_calls_total,
                            a.rss_bytes as f64 / 1e6,
                            s.tiles_drawn,
                            s.gpu_tiles,
                            thousands(s.slots_drawn),
                            s.pop_drawn / 1e6,
                            s.built_min,
                            s.built_max,
                            people_text(s.people_in_view),
                            thousands(s.dots_in_view)
                        );
                    }
                }
                // MG-11's pan window: from 3 s after the first dots to the quit.
                #[cfg(feature = "probe")]
                if pan && pan_window.is_none() && first_dots.is_some_and(|t| process_start.elapsed() >= t + Duration::from_secs(3)) {
                    pan_window = Some((Instant::now(), allocation_stats().draw_calls_total));
                    (gpu_max, rss_max) = (0, 0);
                    gpui_maps::render::zoom_probe::start("pan steady");
                }
                if quit_after.is_some_and(|q| opened.elapsed().as_secs_f64() >= q) {
                    #[cfg(feature = "probe")]
                    if let Some((opened, draws)) = pan_window.take() {
                        gpui_maps::render::zoom_probe::stop();
                        report_frames("pan", opened.elapsed().as_secs_f64(), allocation_stats().draw_calls_total - draws, rss_max, gpu_max);
                        mask_line();
                    }
                    println!("quit after {:.0}s; peak rss {:.0} MB", opened.elapsed().as_secs_f64(), peak_rss_bytes() as f64 / 1e6);
                    cx.update(|cx| cx.quit());
                    return;
                }
            }
        })
        .detach();
    });
    std::mem::forget(runtime);
}

/// `mask: rekeys N pass_cpu_ms mean M max X`: the mask re-keys the run
/// made and the pass's CPU wall time (encode through submit).
#[cfg(feature = "probe")]
fn mask_line() {
    use gpui_maps::render::dot_mask::{MASK_PASS_NS_MAX, MASK_PASS_NS_TOTAL, MASK_REKEYS_TOTAL};
    use std::sync::atomic::Ordering::Relaxed;
    let n = MASK_REKEYS_TOTAL.load(Relaxed);
    let gpu: Vec<f64> = gpui_maps::render::mask_timer::mask_pass_gpu_ns().iter().map(|&ns| ns as f64 / 1e6).collect();
    if gpu.is_empty() {
        // No `TIMESTAMP_QUERY` (or no pass read back yet): the fallback.
        println!(
            "mask: rekeys {n} pass_gpu_ms n/a (no timestamp query) | FALLBACK pass_cpu_ms mean {:.3} max {:.3} (encode+submit wall time, not GPU execution)",
            MASK_PASS_NS_TOTAL.load(Relaxed) as f64 / n.max(1) as f64 / 1e6,
            MASK_PASS_NS_MAX.load(Relaxed) as f64 / 1e6
        );
    } else {
        println!(
            "mask: rekeys {n} pass_gpu_ms p50 {:.3} p95 {:.3} max {:.3} over {} passes (timestamp query) | pass_cpu_ms mean {:.3}",
            quantile(&gpu, 0.5),
            quantile(&gpu, 0.95),
            quantile(&gpu, 1.0),
            gpu.len(),
            MASK_PASS_NS_TOTAL.load(Relaxed) as f64 / n.max(1) as f64 / 1e6
        );
    }
}

/// MG-11's line: CPU frame p50/p95/max, frames over 16.7 and 33 ms, the
/// GPU pass samples, draw calls in the window (0 = nothing painted, the
/// figures are void), RSS and dot GPU bytes at their maxima.
#[cfg(feature = "probe")]
fn report_frames(prefix: &str, secs: f64, draws: u64, rss_max: u64, gpu_max: u64) {
    let (_, frames) = gpui_maps::render::zoom_probe::snapshot();
    let cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
    let gpu: Vec<f64> = frames.iter().map(|f| f.gpu_ms_last_sample).filter(|&ms| ms > 0.0).collect();
    let over = |ms: f64| cpu.iter().filter(|&&c| c > ms).count();
    println!(
        "-- {prefix}: {} frames in {secs:.1} s ({:.1} fps) | cpu p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms | {} over 16.7 ms, {} over 33 ms | gpu pass p50 {:.2} p95 {:.2} max {:.2} ms | draw calls {draws}{} | rss max {:.0} MB, peak {:.0} MB | dot gpu max {:.1} MB --",
        cpu.len(),
        cpu.len() as f64 / secs.max(1e-9),
        quantile(&cpu, 0.5),
        quantile(&cpu, 0.95),
        quantile(&cpu, 0.99),
        quantile(&cpu, 1.0),
        over(16.7),
        over(33.0),
        quantile(&gpu, 0.5),
        quantile(&gpu, 0.95),
        quantile(&gpu, 1.0),
        if draws == 0 { " (NOTHING PAINTED: void)" } else { "" },
        rss_max as f64 / 1e6,
        peak_rss_bytes() as f64 / 1e6,
        gpu_max as f64 / 1e6
    );
    gate(&format!("dots.{prefix}.frames"), cpu.len() as f64, "frames");
    gate(&format!("dots.{prefix}.cpu_p95_ms"), quantile(&cpu, 0.95), "ms");
    gate(&format!("dots.{prefix}.cpu_max_ms"), quantile(&cpu, 1.0), "ms");
    gate(&format!("dots.{prefix}.frames_over_33ms"), over(33.0) as f64, "frames");
    gate(&format!("dots.{prefix}.gpu_pass_p95_ms"), quantile(&gpu, 0.95), "ms");
    gate(&format!("dots.{prefix}.draw_calls"), draws as f64, "draws");
    gate(&format!("dots.{prefix}.rss_max_mb"), rss_max as f64 / 1e6, "MB");
    gate(&format!("dots.{prefix}.dot_gpu_max_mb"), gpu_max as f64 / 1e6, "MB");
}

/// `--zoom-sweep`: z2 -> z15 -> z2 over the preset's centre, twice slow
/// (8 s, so a person can watch each level switch) and once flicked
/// (0.6 s), each followed by a settle.
#[cfg(feature = "probe")]
fn zoom_sweep_task(map: Entity<MapView>, at: Preset, cx: &mut App) {
    use gpui_maps::render::zoom_probe;
    cx.spawn(async move |cx| {
        let tick = Duration::from_millis(16);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let drawn = map.read_with(cx, |v, _| v.dot_stats(LAYER).is_some_and(|s| s.dots_drawn > 0));
            if drawn {
                break;
            }
            if Instant::now() > deadline {
                println!("zoom sweep: no dots within 60 s; giving up");
                cx.update(|cx| cx.quit());
                return;
            }
            cx.background_executor().timer(Duration::from_millis(50)).await;
        }
        let set = |cx: &mut gpui::AsyncApp, z: f64| {
            zoom_probe::note_zoom(z);
            map.update(cx, |view, cx| {
                view.set_camera(at.lon, at.lat, z);
                cx.notify();
            });
            true
        };
        let park = Instant::now();
        while park.elapsed() < Duration::from_secs(3) {
            if !set(cx, 2.0) {
                return;
            }
            cx.background_executor().timer(tick).await;
        }
        let draws_before = allocation_stats().draw_calls_total;
        let (mut gpu_max, mut rss_max) = (0u64, 0u64);
        zoom_probe::start("parked z2");
        let legs: [(&str, Option<(f64, f64)>, f64); 9] = [
            ("parked z2", None, 1.0),
            ("slow in (z2->z15, 8 s)", Some((2.0, 15.0)), 8.0),
            ("settle z15", None, 2.0),
            ("slow out (z15->z2, 8 s)", Some((15.0, 2.0)), 8.0),
            ("settle z2", None, 2.0),
            ("flick in (0.6 s)", Some((2.0, 15.0)), 0.6),
            ("settle z15 #2", None, 2.0),
            ("flick out (0.6 s)", Some((15.0, 2.0)), 0.6),
            ("settle z2 #2", None, 2.0),
        ];
        let started_all = Instant::now();
        let mut z = 2.0;
        for (i, (label, ramp, secs)) in legs.into_iter().enumerate() {
            if i > 0 {
                zoom_probe::mark(label);
            }
            cx.update(|cx| cx.activate(true));
            let started = Instant::now();
            loop {
                if let Some(s) = map.read_with(cx, |v, _| v.dot_stats(LAYER)) {
                    gpu_max = gpu_max.max(s.gpu_bytes);
                }
                rss_max = rss_max.max(gpui_maps::render::rss_bytes());
                let t = (started.elapsed().as_secs_f64() / secs).min(1.0);
                if let Some((from, to)) = ramp {
                    let e = t * t * (3.0 - 2.0 * t);
                    z = from + (to - from) * e;
                }
                if !set(cx, z) {
                    return;
                }
                if t >= 1.0 {
                    break;
                }
                cx.background_executor().timer(tick).await;
            }
        }
        zoom_probe::stop();
        mask_line();
        report_frames(
            "sweep",
            started_all.elapsed().as_secs_f64(),
            allocation_stats().draw_calls_total - draws_before,
            rss_max,
            gpu_max,
        );
        cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(not(feature = "probe"))]
fn zoom_sweep_task(_: Entity<MapView>, _: Preset, _: &mut App) {
    panic!("--zoom-sweep needs --features probe");
}

/// **MG-23** (`--animate-epochs N`): waits until the view has settled --
/// dots drawn, every wanted tile ready, and no dot upload for 3 s -- then
/// animates the mix A -> B `n` times ([`EPOCH_ANIMATION`] each, a jump back
/// to A between), and prints the frames (`report_frames("epochs")`) and
/// the uploads over the animations: this map's own `MapView::dot_uploads`
/// and the process's `allocation_stats` (`mesh_installs_total` counts a
/// block texture, `gpu_buffers_created_total` any buffer). Criterion: 0
/// block uploads and 0 buffers created, CPU frame p95 <= 16.7 ms, with
/// draw calls > 0 (else the figures are void). The whole camera -- the
/// `--at` centre and zoom, `--pitch`, bearing 0 -- is held with `jump_to`
/// every tick, and any tick where the centre, zoom, pitch, bearing or
/// viewport had moved is counted and the first five printed. Then quits.
#[cfg(feature = "probe")]
fn epochs_task(map: Entity<MapView>, at: Preset, pitch_deg: f32, n: u32, cx: &mut App) {
    use gpui_maps::render::zoom_probe;
    // The whole camera the measurement is taken at (review M-2: centre and
    // zoom were held, pitch and bearing were not, and a stray rotate or
    // tilt gesture would have gone unlogged and stayed).
    let held = gpui_maps::camera_control::CameraTarget {
        center: Some((at.lon, at.lat)),
        zoom: Some(at.zoom),
        pitch: Some(pitch_deg.to_radians()),
        bearing: Some(0.0),
        ..Default::default()
    };
    // What differs from the held camera, or from the viewport the view
    // settled at: `None` when nothing does.
    let drift = move |c: &gpui_maps::render::Camera, viewport: (f32, f32)| -> Option<String> {
        let off = (c.center_lon - at.lon).abs() > 1e-6
            || (c.center_lat - at.lat).abs() > 1e-6
            || (c.zoom - at.zoom).abs() > 1e-6
            || (c.pitch - pitch_deg.to_radians()).abs() > 1e-5
            || c.bearing.abs() > 1e-5
            || (c.viewport_width_px, c.viewport_height_px) != viewport;
        off.then(|| {
            format!(
                "{:.4},{:.4} z{:.3} pitch {:.2} deg bearing {:.2} deg viewport {}x{} (held {}x{})",
                c.center_lon,
                c.center_lat,
                c.zoom,
                c.pitch.to_degrees(),
                c.bearing.to_degrees(),
                c.viewport_width_px,
                c.viewport_height_px,
                viewport.0,
                viewport.1
            )
        })
    };
    cx.spawn(async move |cx| {
        let tick = Duration::from_millis(16);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut quiet_since: Option<(Instant, gpui_maps::overlay::host::DotUploads)> = None;
        loop {
            cx.update(|cx| cx.activate(true));
            let (stats, uploads) = map.read_with(cx, |v, _| (v.dot_stats(LAYER), v.dot_uploads()));
            let settled = stats.is_some_and(|s| s.dots_drawn > 0 && s.tiles_ready == s.tiles_wanted && s.source_errors == 0);
            let same = |u: gpui_maps::overlay::host::DotUploads, v: gpui_maps::overlay::host::DotUploads| {
                u.tile_uploads == v.tile_uploads && u.groups_created == v.groups_created
            };
            match quiet_since {
                Some((_, u)) if settled && same(u, uploads) => {}
                _ => quiet_since = settled.then(|| (Instant::now(), uploads)),
            }
            if quiet_since.is_some_and(|(at, _)| at.elapsed() >= Duration::from_secs(3)) {
                break;
            }
            if Instant::now() > deadline {
                println!("epochs: the view did not settle within 120 s ({stats:?}); giving up");
                cx.update(|cx| cx.quit());
                return;
            }
            // Held at the preset: a stray scroll or drag on the frontmost
            // window (the first run's camera wandered from India z5 to a
            // single tile) must not move the view under the measurement.
            map.update(cx, |v, cx| {
                v.jump_to(held);
                cx.notify();
            });
            cx.background_executor().timer(Duration::from_millis(50)).await;
        }
        let viewport = map.read_with(cx, |v, _| (v.camera().viewport_width_px, v.camera().viewport_height_px));
        let stats = map.read_with(cx, |v, _| v.dot_stats(LAYER));
        println!("epochs: settled ({stats:?}); animating {n} x {:?}", EPOCH_ANIMATION);
        let a0 = allocation_stats();
        let u0 = map.read_with(cx, |v, _| v.dot_uploads());
        let (mut gpu_max, mut rss_max) = (0u64, 0u64);
        zoom_probe::start("epochs");
        let started_all = Instant::now();
        let mut last = None;
        let mut moved = 0u32;
        for i in 0..n {
            if i > 0 {
                zoom_probe::mark(&format!("animation {i}"));
            }
            cx.update(|cx| cx.activate(true));
            let started = Instant::now();
            loop {
                let t = mix_at(started.elapsed(), 0.0, 1.0);
                // Did anything but this task move the camera since the last
                // tick (a scroll or drag on the frontmost window)?
                {
                    let c = map.read_with(cx, |v, _| *v.camera());
                    if let Some(what) = drift(&c, viewport) {
                        moved += 1;
                        if moved <= 5 {
                            println!("epochs: animation {i} t {t:.3}: the camera was moved to {what} (not by this task)");
                        }
                    }
                }
                map
                    .update(cx, |v, cx| {
                        v.jump_to(held);
                        v.set_dot_epoch_mix(LAYER, t, cx);
                    });
                if let (Some(s), u, vp) = map.read_with(cx, |v, _| (v.dot_stats(LAYER), v.dot_uploads(), v.viewport_px())) {
                    gpu_max = gpu_max.max(s.gpu_bytes);
                    // Anything that moves the ppd or uploads mid-animation
                    // is named, with what the frame drew then.
                    if last.is_some_and(|(ppd, up, _): (f32, u64, _)| ppd != s.ppd || up != u.tile_uploads) {
                        println!(
                            "epochs: animation {i} t {t:.3}: ppd {:.1} -> {:.1}, tile uploads {} -> {} | tiles drawn {} wanted {} ready {} standins {} pop {:.0} viewport {vp:?} (was {:?})",
                            last.map_or(0.0, |l| l.0),
                            s.ppd,
                            last.map_or(0, |l| l.1),
                            u.tile_uploads,
                            s.tiles_drawn,
                            s.tiles_wanted,
                            s.tiles_ready,
                            s.standin_tiles,
                            s.pop_drawn,
                            last.map(|l| l.2)
                        );
                    }
                    last = Some((s.ppd, u.tile_uploads, vp));
                }
                rss_max = rss_max.max(gpui_maps::render::rss_bytes());
                if t >= 1.0 {
                    break;
                }
                cx.background_executor().timer(tick).await;
            }
        }
        // One more frame at t = 1 so the last write is painted, then read.
        cx.background_executor().timer(Duration::from_millis(100)).await;
        zoom_probe::stop();
        let a1 = allocation_stats();
        let u1 = map.read_with(cx, |v, _| v.dot_uploads());
        let draws = a1.draw_calls_total - a0.draw_calls_total;
        report_frames("epochs", started_all.elapsed().as_secs_f64(), draws, rss_max, gpu_max);
        println!(
            "epochs uploads: dot tile_uploads +{} groups_created +{} params_writes +{} | mesh_installs_total +{} gpu_buffers_created_total +{} gpu_buffer_bytes_created_total +{} | draw calls +{draws}{}",
            u1.tile_uploads - u0.tile_uploads,
            u1.groups_created - u0.groups_created,
            u1.params_writes - u0.params_writes,
            a1.mesh_installs_total - a0.mesh_installs_total,
            a1.gpu_buffers_created_total - a0.gpu_buffers_created_total,
            a1.gpu_buffer_bytes_created_total - a0.gpu_buffer_bytes_created_total,
            if draws == 0 { " (NOTHING PAINTED: void)" } else { "" }
        );
        gate("dots.epochs.dot_tile_uploads", (u1.tile_uploads - u0.tile_uploads) as f64, "uploads");
        gate("dots.epochs.gpu_buffers_created", (a1.gpu_buffers_created_total - a0.gpu_buffers_created_total) as f64, "buffers");
        println!("epochs: ticks where the camera had been moved by something else: {moved}");
        let end = map.read_with(cx, |v, _| v.dot_style(LAYER));
        println!("epochs: end style mix {:?}", end.map(|s| s.epoch_mix));
        cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(not(feature = "probe"))]
fn epochs_task(_: Entity<MapView>, _: Preset, _: f32, _: u32, _: &mut App) {
    panic!("--animate-epochs needs --features probe");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gate run from a non-probe binary refuses; any other run goes ahead.
    #[test]
    fn a_gate_run_without_the_probe_build_refuses() {
        assert_eq!(gate_refusal(false), None);
        assert_eq!(gate_refusal(true).is_some(), !cfg!(feature = "probe"));
        if let Some(refusal) = gate_refusal(true) {
            assert!(refusal.starts_with("GATE ERROR") && refusal.contains("--features probe"));
        }
    }
    use gpui_maps::geo::TileId;

    fn key(z: u8, x: u32, y: u32, level: u8) -> GridTileKey {
        GridTileKey { tile: TileId { z, x, y }, level, epoch: 2025, epoch_b: None }
    }

    /// Final review I1: `--measure-clickhouse`'s raw wire timing fails with
    /// a message that names the host, not the URL's credentials.
    #[test]
    #[cfg(feature = "clickhouse-demo")]
    fn a_failed_wire_measurement_never_carries_credentials() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap();
        let req = gpui_maps::overlay::dotgrid::GridRequest::Post {
            url: "http://user:SECRET@127.0.0.1:9/?password=SECRET".into(),
            body: "SELECT 1".into(),
            headers: Vec::new(),
        };
        let err = wire(&runtime, &client, &req).expect_err("nothing listens on port 9");
        assert!(!err.contains("SECRET"), "a credential leaked: {err}");
        assert!(err.contains("127.0.0.1:9"), "{err}");
    }

    #[test]
    fn a_table_name_gives_its_cell_size() {
        assert_eq!(table_arcsec("ghs_cells_30ss"), 30.0);
        assert_eq!(table_arcsec("ghs_cells_3ss"), 3.0);
        assert_eq!(ch_criterion("ghs_cells_30ss"), ("MG-14", 50.0));
        assert_eq!(ch_criterion("ghs_cells_3ss"), ("MG-21", 80.0));
        #[cfg(feature = "clickhouse-demo")]
        {
            let names = |at: &str| ch_presets(preset(at)).iter().map(|p| p.name).collect::<Vec<_>>();
            assert_eq!(names("world"), ["world_z1", "india_z5", "dhaka_z8"]);
            assert_eq!(names("dhaka15"), ["world_z1", "india_z5", "dhaka_z8", "dhaka_z15"]);
        }
    }

    #[test]
    fn the_synthetic_source_is_deterministic() {
        let s = synthetic::source();
        // Dhaka's level-0 tile, and a level-2 tile over India.
        for k in [key(6, 48, 27, 0), key(4, 11, 6, 2)] {
            let a = s.decode(&[k], &[]).unwrap();
            let b = s.decode(&[k], &[]).unwrap();
            assert!(!a[0].1.cells.is_empty(), "{k:?} has land");
            assert_eq!(a, b);
        }
    }

    /// A level-1 z5 tile against its four z6 level-0 children. A Mercator
    /// tile's north/south edge is not on the 30″ lattice, so a boundary
    /// row's children can sit in the next tile row -- the sum is exact
    /// over the children that roll up into this tile's window (checked to
    /// 1e-6), and within 0.5 % over the four tiles as they stand.
    #[test]
    fn the_synthetic_pyramid_sums() {
        let d = synthetic::descriptor();
        let s = synthetic::source();
        let parent = key(5, 23, 13, 1); // over Bangladesh and eastern India
        let p = &s.decode(&[parent], &[]).unwrap()[0].1;
        assert!(p.total > 0.0);
        let pw = grid::cell_window(&d, 1, parent.tile);
        let (mut rolled, mut plain) = (0f64, 0f64);
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let k = key(6, parent.tile.x * 2 + dx, parent.tile.y * 2 + dy, 0);
            let c = &s.decode(&[k], &[]).unwrap()[0].1;
            plain += c.total;
            let cw = grid::cell_window(&d, 0, k.tile);
            for &(idx, pop, _) in &c.cells {
                let (r, col) = grid::row_col(&cw, idx);
                let (pr, pc) = (r / 2, col / 2);
                if pr >= pw.row0 && pr < pw.row0 + pw.rows && pc >= pw.col0 && pc < pw.col0 + pw.cols {
                    rolled += f64::from(pop);
                }
            }
        }
        // The rows of this tile's window whose children fall outside the
        // four tiles (the boundary rows): add them from the source itself.
        let rel = |a: f64, b: f64| (a - b).abs() / b.abs().max(1e-12);
        let mut outside = 0f64;
        let child_rows = (grid::cell_window(&d, 0, key(6, parent.tile.x * 2, parent.tile.y * 2, 0).tile).row0,
            grid::cell_window(&d, 0, key(6, parent.tile.x * 2, parent.tile.y * 2 + 1, 0).tile));
        let (top, bottom) = (child_rows.0, child_rows.1.row0 + child_rows.1.rows);
        for r in pw.row0 * 2..(pw.row0 + pw.rows) * 2 {
            if r >= top && r < bottom {
                continue;
            }
            for c in pw.col0 * 2..(pw.col0 + pw.cols) * 2 {
                outside += f64::from(synthetic::cell_pop_at(r, c));
            }
        }
        assert!(rel(p.total, rolled + outside) < 1e-6, "{} vs {} + {}", p.total, rolled, outside);
        assert!(rel(p.total, plain) < 5e-3, "{} vs {}", p.total, plain);
    }

    #[test]
    fn synthetic_cells_are_about_twenty_million() {
        let d = synthetic::descriptor();
        let n = 1u32 << d.max_data_zoom;
        let mut cells = 0usize;
        for y in 0..n {
            for x in 0..n {
                cells += synthetic::tile(&d, &key(d.max_data_zoom, x, y, 0)).cells.len();
            }
        }
        assert!((15_000_000..=25_000_000).contains(&cells), "{cells} populated level-0 cells");
    }

    /// `examples/perf_gate.rs`' `parse_gate_lines`, restated: it is in
    /// another example crate and cannot be imported.
    fn parse(line: &str) -> Option<(String, f64, String)> {
        let rest = line.strip_prefix("gate: ")?;
        let mut it = rest.split_whitespace();
        let (name, value, unit) = (it.next()?, it.next()?, it.next()?);
        Some((name.to_string(), value.parse().ok()?, unit.to_string()))
    }

    /// Fix round 1, MEDIUM: `--worldview` has a fixed vocabulary, same as
    /// `--relocate on|off` -- validated at parse time rather than
    /// threaded through unchecked.
    #[test]
    fn worldview_arg_only_accepts_the_four_codes() {
        for v in ["IN", "US", "CN", "JP"] {
            assert_eq!(worldview_arg(v.to_string()), v);
        }
    }

    #[test]
    #[should_panic(expected = "--worldview takes IN|US|CN|JP, got \"banana\"")]
    fn worldview_arg_rejects_anything_else() {
        worldview_arg("banana".to_string());
    }

    #[test]
    fn gate_lines_parse() {
        for (name, value, unit) in [
            ("dots.view.world_z1.dots", 2_812_345.0, "count"),
            ("dots.tile_build_ms_p50", 1.25, "ms"),
            ("dots.world_total_err_pct", 0.000_123, "pct"),
            ("dots.nesting_violations", 0.0, "count"),
        ] {
            let line = gate_line(name, value, unit);
            assert_eq!(parse(&line), Some((name.to_string(), value, unit.to_string())), "{line}");
        }
    }
}
