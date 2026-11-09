//! Every US Census block group (~242k polygons, ~70.7 M coordinates) as ONE
//! runtime overlay, drawn over the basemap -- and what that costs.
//!
//! The "no preprocessing" case: no tiling, no simplification, no
//! tippecanoe. The Census TIGER/Line 2023 shapefiles are read as published
//! and handed to the engine's ordinary public overlay API
//! (`MapView::set_source_columns`: the features plus one typed `income`
//! column, with no per-feature property map), minus the GeoJSON string round
//! trip `examples/overlay_features.rs` takes. `--geojson FILE` takes the
//! string door instead (`MapView::set_source`), for comparison.
//!
//! Fill: median household income (ACS 5-year 2023, B19013_001E), joined by
//! GEOID as a numeric feature property `income`, on a continuous viridis
//! `interpolate` from $2,500 to $250,000 at `fill-opacity` 0.75, grey
//! `#9E9E9E` where the Census has no estimate -- Kyle Walker's mapgl block
//! group demo, restyled to read the way his does: a pale basemap so the
//! ramp carries the colour, a thin darker hairline between block groups,
//! and a white legend card (top-left, drawn as ordinary gpui elements over
//! the map -- not an engine feature) with the ramp and its tick values. The
//! engine quantises the ramp to `RAMP_STEPS` (256) colours, so ~224k
//! distinct incomes are a few hundred paint classes.
//!
//! Basemap: Mapbox Light (`mapbox://styles/mapbox/light-v11`, vendored at
//! `demos/mapbox-light-style.json`) when `MAPBOX_ACCESS_TOKEN` is set,
//! otherwise OpenFreeMap Positron (`demos/positron-style.json`, fetched from
//! `https://tiles.openfreemap.org/styles/positron`) -- both pale greys so
//! the income choropleth is the only saturated colour on screen. `--basemap
//! mapbox` or `--basemap positron` overrides the auto-pick either way (the
//! former needs the token, the latter never does). As in Kyle Walker's
//! reference, basemap place names draw on top of the choropleth: every
//! block group source is below the labels (`MapView::set_source_below_labels`,
//! the engine's default, called explicitly). Hovering a block group shows
//! a card with its GEOID, median household income and land area, from the
//! engine's overlay hit-test (`FeatureEvent::HoverEnter`/`HoverLeave`),
//! and outlines it (`MapView::set_feature_highlight`: 2 px white over a
//! dark casing, a slight brighten). Both go away while the camera moves --
//! the engine ends the hover -- and come back once it settles. Light's
//! place names are darkened and haloed for the ramp (`readable_place_labels`).
//!
//! Data (public domain, never committed): `demos/data/fetch_us_block_groups.sh`
//! (shapefiles, and `acs/income_bg_2023.csv` for the income join).
//!
//! ```text
//! cargo run --release --example us_block_groups -- [--all | --states 44 | --states 06,41,53]
//!     [--measure] [--no-outline] [--geojson FILE] [--data DIR] [--quit-after SECS] [--pan]
//!     [--one-source] [--index-only] [--zoom-sweep [--at dc|la]] [--slider-sweep] [--hover-sweep] [--panel-sweep]
//!     [--basemap mapbox|positron]
//!     [--camera LON,LAT,ZOOM] [--range LO,HI] [--snapshot PNG [--snapshot-at SECS]]
//! ```
//!
//! The legend card carries a two-handle income range slider: only block
//! groups whose median household income lies inside the range are drawn
//! (no-data block groups are hidden while a range is set, shown in grey at
//! the full span). The handles snap to the engine's own quantised ramp
//! steps, so a drag repaints through `MapView::update_source_paint`'s
//! table-only path -- a paint-table rewrite, no tile re-bake. The hover
//! card ignores hidden block groups.
//!
//! `--slider-sweep` (needs `--features probe`) is the slider's harness:
//! once the first overlay frame is up it drags a 40-step window across the
//! whole range and back for ~10 s (an update every 2 ms tick, snapped: the
//! table-only path), then forces ten re-bakes by alternating an unsnapped
//! range edge with a snapped one, waiting for each to settle. It prints
//! update latency (p50/max), the fps during the drag, which engine path
//! each update took, and the zoom probe's hole counts, then quits.
//!
//! `--hover-sweep` (needs `--features probe`) is the hover tooltip's
//! harness: 120 Hz synthetic mouse moves over New York at z4 and z10,
//! reporting how far the tooltip falls behind and what each move costs the
//! map (see `hover_sweep_task`). `US_BG_REDRAW_ALWAYS=1` turns off
//! `MapView::set_skip_unchanged_frames` for an A/B in the same binary.
//!
//! The "Breakdowns" panel on the right (`panel.rs`, over `breakdowns.rs`)
//! ranks the states, draws a ridgeline of the 25 largest metros and lists
//! what the view shows, all following the slider. It needs the population
//! and metro tables the fetch script writes; without them it says so.
//! `--panel-sweep` scripts it -- a slider drag on each tab, a state hover
//! (every other state dims), the three sorts, two fly-tos -- and prints the
//! panel's per-frame build and canvas paint cost per phase, with the map's
//! draw calls beside each (0 means the window never painted: discard).
//!
//! `--zoom-sweep` (needs `--features probe`) is the zoom-continuity
//! harness: once the first overlay frame is up it parks the camera over
//! `--at` (Washington DC by default) at z3, then scripts two slow sweeps
//! z3 -> z12 -> z3 (4 s each way, eased, ~2.25 zoom levels a second) and two
//! fast flicks (0.6 s each way), with a 3 s settle at each end, while
//! `render::zoom_probe` logs every frame. It prints the per-segment report,
//! writes the per-frame CSV to `$US_BG_ZOOM_CSV` when set, and quits.
//!
//! `--measure` opens no window: it loads, pushes through the same calls the
//! window makes, waits for the off-thread tessellation to land on a headless
//! gpui `App`, prints the numbers, and exits. There is no GPU in that mode,
//! so there is no upload; the windowed run reports the upload as the GPU
//! buffer bytes that appear once the mesh is drawn. gpui repaints only on
//! change, so an untouched window reports idle frames; `--pan` drifts the
//! camera west to east every 16 ms so the frame counters measure drawing.
//!
//! `--measure` also prints one `tile index:` row: phase B's internal tile
//! index (`overlay::tile_index`) built from each pushed fill source's
//! features. `--measure --index-only` pushes into a bare `OverlayStore`
//! instead of a `MapView`, so nothing is tessellated, prints that row and
//! exits: the national index build without the national mesh beside it.
//!
//! `US_BG_GATE=1` adds machine-readable `gate: NAME VALUE UNIT` lines to
//! `--measure`, `--pan --quit-after`, `--zoom-sweep` and `--slider-sweep`
//! (and, headless, the gate's fixed-camera cover bakes and a table-only
//! update run). `scripts/perf-gate.sh` grades them; see `docs/perf-gate.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{px, size, App, AppContext, Bounds, Entity, TitlebarOptions, WindowBounds, WindowOptions};
#[cfg(feature = "probe")]
use gpui::Window;
use gpui_maps::census::ramp::*;
use gpui_maps::census::view::ViewItem;
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::{FeatureId, FeatureInput, FeaturePaint, GeometryInput, PropertyColumn};
use gpui_maps::render::{allocation_stats, AllocationStats};
use gpui_maps::tile_source::TileSource;

use swat_maps_kit::shell;
// The panel contract (`kit::PanelEnv`, `kit::PanGate`, the credits); the
// panel names it `crate::kit`, as every includer of a panel declares it.
use swat_maps_kit::kit;

// The windowed body -- the block groups and their push, the basemaps, the
// window root with its tooltip, slider, histograms and Breakdowns panel --
// lives in the panel (`kit/src/panels/census.rs`, the one copy), which
// also includes `us_block_groups/{breakdowns,panel}.rs`: one include per
// crate. `main`, `Args`, `load`, every headless mode and `windowed` stay
// here.
use swat_maps_kit::panels::census;
use census::*;
// The continental US, framed the way Kyle Walker's mapgl reference opens:
// one value for both shells (`census::START`), so the MG-C11 parity camera
// cannot drift between them.
use gpui_maps::census::START;

struct Args {
    states: Vec<String>,
    measure: bool,
    outline: bool,
    geojson: Option<PathBuf>,
    data: PathBuf,
    quit_after: Option<f64>,
    pan: bool,
    one_source: bool,
    index_only: bool,
    zoom_sweep: bool,
    slider_sweep: bool,
    /// `--hover-sweep`: see [`hover_sweep_task`].
    hover_sweep: bool,
    panel_sweep: bool,
    at: (f64, f64),
    basemap: Option<Basemap>,
    /// `--live [wave|random]`: see [`live_task`].
    live: Option<LiveMode>,
    /// `--live-secs`: how long the live phase runs (default 120).
    live_secs: f64,
    /// `--live-zoom Z`: run the live phase with the camera at `--at`, zoom Z
    /// (default: the national view).
    live_zoom: Option<f64>,
    /// `--snapshot PNG`: write the window (map AND the gpui legend) to a
    /// PNG `--snapshot-at` seconds in (default 15). See [`snapshot_window`].
    snapshot: Option<PathBuf>,
    snapshot_at: f64,
    /// `--range LO,HI`: open with the slider at these stop indices.
    range: Option<(usize, usize)>,
    /// `--camera LON,LAT,ZOOM`: open there instead of the national view.
    camera: Option<(f64, f64, f64)>,
}

fn parse_args() -> Args {
    let mut args = Args {
        states: ALL_STATES.iter().map(|s| s.to_string()).collect(),
        measure: false,
        outline: true,
        geojson: None,
        data: census::default_data(),
        quit_after: None,
        pan: false,
        one_source: false,
        index_only: false,
        zoom_sweep: false,
        slider_sweep: false,
        hover_sweep: false,
        panel_sweep: false,
        at: (-77.03, 38.90),
        basemap: None,
        live: None,
        live_secs: 120.0,
        live_zoom: None,
        snapshot: None,
        snapshot_at: 15.0,
        range: None,
        camera: None,
    };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--all" => args.states = ALL_STATES.iter().map(|s| s.to_string()).collect(),
            "--states" => {
                let list = it.next().expect("--states takes a comma-separated FIPS list, e.g. 06,41,53");
                args.states = list.split(',').map(|s| format!("{:0>2}", s.trim())).collect();
            }
            "--measure" => args.measure = true,
            "--no-outline" => args.outline = false,
            "--pan" => args.pan = true,
            "--one-source" => args.one_source = true,
            "--index-only" => args.index_only = true,
            "--zoom-sweep" => args.zoom_sweep = true,
            "--slider-sweep" => args.slider_sweep = true,
            "--hover-sweep" => args.hover_sweep = true,
            "--panel-sweep" => args.panel_sweep = true,
            "--live" => {
                args.live = Some(match it.peek().map(String::as_str) {
                    Some("random") => {
                        it.next();
                        LiveMode::Random
                    }
                    Some("wave") => {
                        it.next();
                        LiveMode::Wave
                    }
                    _ => LiveMode::Wave,
                })
            }
            "--live-zoom" => {
                args.live_zoom = Some(it.next().and_then(|s| s.parse().ok()).expect("--live-zoom takes a zoom"))
            }
            "--live-secs" => {
                args.live_secs = it.next().and_then(|s| s.parse().ok()).expect("--live-secs takes seconds")
            }
            "--at" => {
                args.at = match it.next().as_deref() {
                    Some("dc") => (-77.03, 38.90),
                    Some("la") => (-118.25, 34.05),
                    other => panic!("--at takes dc or la, not {other:?}"),
                }
            }
            "--basemap" => {
                args.basemap = match it.next().as_deref() {
                    Some("mapbox") => Some(Basemap::Mapbox),
                    Some("positron") => Some(Basemap::Positron),
                    other => panic!("--basemap takes mapbox or positron, not {other:?}"),
                }
            }
            "--geojson" => args.geojson = Some(it.next().expect("--geojson takes a file").into()),
            "--data" => args.data = it.next().expect("--data takes a directory").into(),
            "--snapshot" => args.snapshot = Some(it.next().expect("--snapshot takes a PNG path").into()),
            "--snapshot-at" => {
                args.snapshot_at = it.next().and_then(|s| s.parse().ok()).expect("--snapshot-at takes seconds")
            }
            "--range" => {
                let v: Vec<usize> = it
                    .next()
                    .map(|s| s.split(',').filter_map(|p| p.parse().ok()).collect())
                    .unwrap_or_default();
                assert!(v.len() == 2 && v[0] < v[1] && v[1] < slider_stops().len(), "--range takes LO,HI stop indices, LO < HI <= 249");
                args.range = Some((v[0], v[1]));
            }
            "--camera" => {
                let v: Vec<f64> = it
                    .next()
                    .map(|s| s.split(',').filter_map(|p| p.parse().ok()).collect())
                    .unwrap_or_default();
                assert!(v.len() == 3, "--camera takes LON,LAT,ZOOM");
                args.camera = Some((v[0], v[1], v[2]));
            }
            "--quit-after" => {
                args.quit_after = Some(it.next().and_then(|s| s.parse().ok()).expect("--quit-after takes seconds"))
            }
            other => panic!("unknown argument {other}; see the doc comment at the top of demos/us_block_groups.rs"),
        }
    }
    args
}

fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the struct it is given and nothing else.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage.ru_maxrss as u64 // bytes on macOS
}

fn gb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// One machine-readable metric for `examples/perf_gate.rs` (driven by
/// `scripts/perf-gate.sh`), printed only when `US_BG_GATE` is set: `gate:
/// NAME VALUE UNIT`. The human-readable rows around it are unchanged; the
/// gate parses these and nothing else, so a reworded row cannot silently
/// change what is graded.
fn gate(name: &str, value: f64, unit: &str) {
    if std::env::var_os("US_BG_GATE").is_some() {
        println!("gate: {name} {value} {unit}");
    }
}

/// `v`'s `q` quantile (nearest rank on the sorted copy); NaN when empty.
#[cfg_attr(not(feature = "probe"), allow(dead_code))]
fn quantile(v: &[f64], q: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return f64::NAN;
    }
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// The gate lines every windowed mode shares, over the frames the zoom
/// probe recorded: holes (overlay and basemap tile-frames), CPU frame
/// p50/p99/max, frames over 16.7 ms, the worst GPU tile-pass sample, and
/// the worst overlay time to full detail over the segments named `settle*`
/// (never reaching full detail scores as the whole segment's length).
#[cfg(feature = "probe")]
fn gate_frames(prefix: &str) {
    let (segments, frames) = gpui_maps::render::zoom_probe::snapshot();
    let cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
    let sum = |f: &dyn Fn(&gpui_maps::render::zoom_probe::FrameRow) -> u32| frames.iter().map(f).map(u64::from).sum::<u64>();
    gate(&format!("{prefix}.frames"), frames.len() as f64, "frames");
    gate(&format!("{prefix}.overlay_holes"), sum(&|f| f.overlay.hole) as f64, "tile-frames");
    gate(&format!("{prefix}.basemap_holes"), sum(&|f| f.basemap.hole) as f64, "tile-frames");
    gate(&format!("{prefix}.cpu_p50_ms"), quantile(&cpu, 0.5), "ms");
    gate(&format!("{prefix}.cpu_p99_ms"), quantile(&cpu, 0.99), "ms");
    gate(&format!("{prefix}.cpu_max_ms"), quantile(&cpu, 1.0), "ms");
    gate(&format!("{prefix}.frames_over_16_7ms"), cpu.iter().filter(|&&ms| ms > 16.7).count() as f64, "frames");
    let gpu: Vec<f64> = frames.iter().map(|f| f.gpu_ms_last_sample).collect();
    gate(&format!("{prefix}.gpu_pass_p50_ms"), quantile(&gpu, 0.5), "ms");
    gate(&format!("{prefix}.gpu_pass_max_ms"), quantile(&gpu, 1.0), "ms");
    let mut detail_max: Option<f64> = None;
    for (i, (label, start)) in segments.iter().enumerate() {
        if !label.starts_with("settle") {
            continue;
        }
        let seg: Vec<_> = frames.iter().filter(|f| f.segment == i).collect();
        let Some(last) = seg.last() else { continue };
        let full = seg.iter().find(|f| {
            let o = &f.overlay;
            o.hole == 0 && o.ancestor == 0 && o.descendants == 0 && o.own + o.empty > 0
        });
        let ms = full.map_or(last.t_ms - start, |f| f.t_ms - start);
        detail_max = Some(detail_max.map_or(ms, |d: f64| d.max(ms)));
    }
    if let Some(ms) = detail_max {
        gate(&format!("{prefix}.detail_after_settle_max_ms"), ms, "ms");
    }
}

fn mem_line(label: &str) -> AllocationStats {
    let s = allocation_stats();
    println!(
        "  mem {label:<22} rss {:>9}  heap {:>9}  peak rss {:>9}",
        gb(s.rss_bytes),
        gb(s.heap_in_use_bytes),
        gb(peak_rss_bytes())
    );
    s
}

/// What `args` asks [`census::load_cancellable`] to read.
fn options(args: &Args) -> census::Options {
    census::Options {
        states: args.states.clone(),
        data: args.data.clone(),
        outline: args.outline,
        basemap: args.basemap,
        camera: args.camera,
        // The standalone is the unsimplified 242k-polygon benchmark: never
        // the gallery's simplified pack.
        pack: None,
    }
}

/// `--geojson FILE`'s text, or the block groups through the ONE load
/// (`census::load_cancellable`) with a flag nobody raises. A state whose
/// shapefile is missing panics here, before anything is read, with the
/// fetch script to run -- the gallery's load skips it instead.
fn load(args: &Args) -> (Payload, Duration) {
    let started = Instant::now();
    let payload = match &args.geojson {
        Some(path) => Payload::GeoJson(std::fs::read_to_string(path).expect("read --geojson file")),
        None => {
            for fips in &args.states {
                let path = census::shapefile_path(&args.data, fips);
                if !path.exists() {
                    panic!("{}: not found -- run demos/data/fetch_us_block_groups.sh first", path.display());
                }
            }
            let never = std::sync::atomic::AtomicBool::new(false);
            census::load_cancellable(&options(args), &never, &|_, _| {}).expect("never cancelled")
        }
    };
    let elapsed = started.elapsed();
    if let Payload::GeoJson(t) = &payload {
        println!("read {} bytes of GeoJSON in {:.2}s", t.len(), elapsed.as_secs_f64());
    }
    (payload, elapsed)
}

fn main() {
    let args = parse_args();
    if let Some(refusal) = gate_refusal(std::env::var_os("US_BG_GATE").is_some()) {
        // stdout: perf_gate discards stderr, and the log is what a human reads.
        println!("{refusal}");
        std::process::exit(2);
    }
    gpui_maps::render::note_process_start();
    if args.measure && args.index_only {
        measure_index_only(args);
    } else if args.measure {
        measure(args);
    } else {
        windowed(args);
    }
}

/// Why a gate run (`US_BG_GATE`) must not go ahead, if it must not. Every
/// row the gate grades beyond the push and memory ones is behind
/// `feature = "probe"`: a binary built without it prints a well-formed run
/// with those rows simply absent, which the gate reads as MISSING rather
/// than as a failure. That is how a plain build re-pointing the shared
/// `release/examples/us_block_groups` link mid-gate cost 5239ac7 its
/// `headless_states.table_only.*` rows. Refusing makes it an exit code.
fn gate_refusal(gate: bool) -> Option<&'static str> {
    (gate && !cfg!(feature = "probe")).then_some(
        "GATE ERROR: US_BG_GATE is set but this binary was built without --features probe; \
         the probe-only rows (table_only, hit, classify, frames) would be missing. \
         Rebuild with --features probe (scripts/perf-gate.sh does).",
    )
}

/// Builds phase B's tile index from every fill source in `store` and prints
/// one row. Every feature gets class 0: the paint classes are task 2's.
fn tile_index_row(store: &gpui_maps::overlay::OverlayStore) {
    use gpui_maps::overlay::tile_index::{TileIndex, TileIndexOptions};
    let names: Vec<_> = store.source_names().filter(|n| n.starts_with(FILL_SOURCE)).cloned().collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let (mut build, mut bytes, mut indexed, mut features) = (Duration::ZERO, 0usize, 0usize, 0usize);
    let mut indices = Vec::new();
    for name in &names {
        let f = store.source_features(name).expect("listed by source_names");
        let classes = vec![0u16; f.len()];
        let started = Instant::now();
        let index = TileIndex::build(f, &classes, TileIndexOptions::default(), &|| false).expect("never cancelled");
        build += started.elapsed();
        bytes += index.bytes();
        indexed += index.indexed_tiles();
        features += f.len();
        indices.push(index);
    }
    println!(
        "tile index: build {:.1} ms on {threads} threads, {:.1} MB, {indexed} index tiles ({} sources, {features} features)",
        build.as_secs_f64() * 1e3,
        bytes as f64 / 1e6,
        names.len()
    );
    mem_line("with tile index");
    drop(indices);
    mem_line("tile index dropped");
}

/// The flat cover `MapView` draws at `START` in a `w` x `h` window: every
/// tile at the display zoom (`floor(zoom)`) the viewport overlaps.
fn start_cover(w: f64, h: f64) -> Vec<gpui_maps::geo::TileId> {
    cover_at(START, w, h)
}

/// [`start_cover`] for any `(lon, lat, zoom)` camera.
fn cover_at((lon, lat, zoom): (f64, f64, f64), w: f64, h: f64) -> Vec<gpui_maps::geo::TileId> {
    let z = zoom.floor() as u8;
    let world = 512.0 * 2f64.powf(zoom);
    let cx = (lon + 180.0) / 360.0 * world;
    let s = lat.to_radians().sin();
    let cy = (0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * std::f64::consts::PI)) * world;
    let tile_px = world / f64::from(1u32 << z);
    let n = 1i64 << z;
    let x0 = ((cx - w / 2.0) / tile_px).floor() as i64;
    let x1 = ((cx + w / 2.0) / tile_px).floor() as i64;
    let y0 = (((cy - h / 2.0) / tile_px).floor() as i64).max(0);
    let y1 = (((cy + h / 2.0) / tile_px).floor() as i64).min(n - 1);
    let mut out = Vec::new();
    for y in y0..=y1 {
        for x in x0..=x1 {
            out.push(gpui_maps::geo::TileId { z, x: x.rem_euclid(n) as u32, y: y as u32 });
        }
    }
    out
}

/// Phase B task 3: the tiled path's own numbers for every tiled fill
/// source -- classify + index build, and the `START` cover (1280x800, the
/// windowed run's) cut, planned and tessellated one thread per tile.
fn cover_bake_row(store: &gpui_maps::overlay::OverlayStore) {
    let cover = start_cover(1280.0, 800.0);
    let names: Vec<_> = store.source_names().filter(|n| n.starts_with(FILL_SOURCE)).cloned().collect();
    for name in &names {
        let Some(m) = gpui_maps::overlay::tile_plan::measure_cover_bake(store, name, &cover, START.2.floor() as u8)
        else {
            println!("cover bake: {name} not classable");
            continue;
        };
        println!(
            "cover bake ({name}): classes {} | index build {:.1} ms, {:.1} MB | {} cover tiles, {} baked ({} compact): wall {:.1} ms, slowest tile {:.1} ms | {} vertices, {} indices, {} outline indices | GPU {:.1} MB (88 B vertex + u32: {:.1} MB) | feature spans {:.1} KB over {} features ({:.1} KB a compact tile, {:.1} B a feature)",
            m.classes,
            m.index_build.as_secs_f64() * 1e3,
            m.index_bytes as f64 / 1e6,
            cover.len(),
            m.tiles_baked,
            m.compact_tiles,
            m.cover_bake_wall.as_secs_f64() * 1e3,
            m.cover_bake_max_tile.as_secs_f64() * 1e3,
            m.vertices,
            m.indices,
            m.outline_indices,
            m.device_bytes as f64 / 1e6,
            m.legacy_device_bytes as f64 / 1e6,
            m.span_bytes as f64 / 1e3,
            m.span_features,
            m.span_bytes as f64 / 1e3 / m.compact_tiles.max(1) as f64,
            m.span_bytes as f64 / m.span_features.max(1) as f64,
        );
    }
    mem_line("after cover bake");
}

/// The gate's fixed cameras: `(name, (lon, lat, zoom))`, each baked at a
/// 1280x800 viewport like [`cover_bake_row`]'s `START` cover.
const GATE_CAMERAS: [(&str, (f64, f64, f64)); 3] =
    [("national", START), ("dc_z8", (-77.03, 38.90, 8.0)), ("la_z11", (-118.25, 34.05, 11.0))];

/// The gate's headless half: the cover bake at each of [`GATE_CAMERAS`]
/// (wall time, and vertex / index / outline-index counts, which are exact),
/// and the bake half of the hole check along the zoom sweep's scripted path
/// -- every integer zoom z3..=z12 over DC and over LA. A hole itself is a
/// paint-time fact (the pick found nothing to draw) that no headless run
/// can see; what it CAN see is a wanted tile whose cut has features and
/// whose bake produced nothing, which would be a hole on every frame.
fn gate_covers(store: &gpui_maps::overlay::OverlayStore) {
    let Some(name) = store.source_names().find(|n| n.starts_with(FILL_SOURCE)).cloned() else { return };
    for (label, camera) in GATE_CAMERAS {
        let cover = cover_at(camera, 1280.0, 800.0);
        let Some(m) = gpui_maps::overlay::tile_plan::measure_cover_bake(store, &name, &cover, camera.2.floor() as u8)
        else {
            continue;
        };
        gate(&format!("headless.cover.{label}.bake_wall_ms"), m.cover_bake_wall.as_secs_f64() * 1e3, "ms");
        gate(&format!("headless.cover.{label}.vertices"), m.vertices as f64, "count");
        gate(&format!("headless.cover.{label}.indices"), m.indices as f64, "count");
        gate(&format!("headless.cover.{label}.outline_indices"), m.outline_indices as f64, "count");
    }
    let (mut wanted, mut unbaked) = (0usize, 0usize);
    for at in [(-77.03, 38.90), (-118.25, 34.05)] {
        for z in 3..=12 {
            let camera = (at.0, at.1, f64::from(z));
            let cover = cover_at(camera, 1280.0, 800.0);
            if let Some(m) = gpui_maps::overlay::tile_plan::measure_cover_bake(store, &name, &cover, z as u8) {
                wanted += m.wanted_tiles;
                unbaked += m.unbaked_with_content;
            }
        }
    }
    gate("headless.path.wanted_tiles", wanted as f64, "count");
    gate("headless.path.unbaked_with_content", unbaked as f64, "count");
    // Past the index's old z14 ceiling (not graded): what a cover costs to
    // cut and bake at z14, z16 and z18 over DC, where tiles above the
    // index's `max_zoom` would be its overzoomed ancestors.
    for z in [14u8, 16, 18] {
        let cover = cover_at((-77.03, 38.90, f64::from(z)), 1280.0, 800.0);
        if let Some(m) = gpui_maps::overlay::tile_plan::measure_cover_bake(store, &name, &cover, z) {
            println!(
                "deep cover dc_z{z}: {} tiles baked, wall {:.2} ms, worst tile {:.2} ms, \
                 {} vertices, {} indices, {} outline indices, device {}, index {}",
                m.tiles_baked,
                m.cover_bake_wall.as_secs_f64() * 1e3,
                m.cover_bake_max_tile.as_secs_f64() * 1e3,
                m.vertices,
                m.indices,
                m.outline_indices,
                gb(m.device_bytes as u64),
                gb(m.index_bytes as u64)
            );
        }
    }
}

/// The table-only update the legend's slider drives, headless: 40 snapped
/// ranges walked across the income scale, each one `update_source_paint`
/// on every fill source, timed synchronously. Every one must take the
/// table-only path; one that does not is counted and graded as exact 0.
///
/// `prefix` is the gate section: `headless` for the national one-source
/// run, `headless_states` for the per-state run (the demo's default
/// shape), whose small states (DC) were whole-source meshes that replanned
/// on every drag step while the one-source row stayed green.
#[cfg(feature = "probe")]
fn gate_table_only(view: &Entity<MapView>, outline: bool, prefix: &str, cx: &mut App) {
    use gpui_maps::map_view::SourcePaintPath;
    let names: Vec<String> =
        view.read(cx).overlay().source_names().filter(|n| n.starts_with(FILL_SOURCE)).map(|n| n.to_string()).collect();
    assert!(!names.is_empty(), "{prefix}.table_only: no fill source to update, so no rows to report");
    const WINDOW: usize = 40;
    let last = slider_stops().len() - 1;
    let (mut sync, mut other) = (Vec::new(), 0usize);
    for step in 0..40usize {
        let lo = (last - WINDOW) * step / 39;
        let paint = paint_object(&IncomeRange { lo, hi: lo + WINDOW }.paint_json(outline));
        let started = Instant::now();
        let paths: Vec<SourcePaintPath> = view.update(cx, |view, cx| {
            names.iter().map(|n| view.update_source_paint(n, &paint, cx).expect("the demo's paint is valid")).collect()
        });
        sync.push(started.elapsed().as_secs_f64() * 1e3);
        other += paths.iter().filter(|p| **p != SourcePaintPath::TableOnly).count();
    }
    println!(
        "table-only update (headless, {} snapped ranges): p50 {:.2} ms max {:.2} ms, {other} took another path",
        sync.len(),
        quantile(&sync, 0.5),
        quantile(&sync, 1.0)
    );
    gate(&format!("{prefix}.table_only.p50_ms"), quantile(&sync, 0.5), "ms");
    gate(&format!("{prefix}.table_only.max_ms"), quantile(&sync, 1.0), "ms");
    gate(&format!("{prefix}.table_only.other_path"), other as f64, "count");
}

/// `--measure --index-only`: the same features, pushed into a bare
/// `OverlayStore` through the same `set_source_columns` door, so the
/// index is built without a tessellation running beside it.
fn measure_index_only(args: Args) {
    println!("us_block_groups --measure --index-only: states {}", args.states.join(","));
    mem_line("start");
    let (payload, _) = load(&args);
    mem_line("after read");
    let Payload::Shapes(data) = payload else { panic!("--index-only reads the shapefiles, not --geojson") };
    let incomes = income_column(&data);
    let rings: Vec<&[[f64; 2]]> = data.rings.iter().map(|&(a, b)| &data.coords[a..b]).collect();
    let polygons: Vec<&[&[[f64; 2]]]> = data.polygons.iter().map(|&(a, b)| &rings[a..b]).collect();
    let groups: Vec<(String, usize, usize)> = if args.one_source {
        vec![(FILL_SOURCE.to_string(), 0, data.features.len())]
    } else {
        data.states.iter().map(|(fips, a, b)| (state_source(fips), *a, *b)).collect()
    };
    let mut store = gpui_maps::overlay::OverlayStore::new();
    let fill_paint = fill_paint_json(false);
    let started = Instant::now();
    for (name, first, last) in &groups {
        store.declare_source_paint(name, &paint_object(&fill_paint)).expect("fill paint must be accepted");
        let fills: Vec<FeatureInput<'_>> = data.features[*first..*last]
            .iter()
            .map(|bg| FeatureInput {
                id: Some(FeatureId::Number(bg.geoid)),
                geometry: GeometryInput::Polygon(&polygons[bg.polygons.0..bg.polygons.1]),
                paint: FeaturePaint::default(),
            })
            .collect();
        let columns = [("income", PropertyColumn::Number(&incomes[*first..*last]))];
        store.set_source_columns(name, &fills, &columns, START.2 as u8).expect("block groups must be accepted");
    }
    println!("push into a bare store: {:.1} ms", started.elapsed().as_secs_f64() * 1e3);
    drop(polygons);
    drop(rings);
    drop(data);
    mem_line("after push (input freed)");
    tile_index_row(&store);
    println!("peak RSS (getrusage ru_maxrss): {}", gb(peak_rss_bytes()));
}

fn measure(args: Args) {
    println!("us_block_groups --measure: states {}", args.states.join(","));
    mem_line("start");
    let (payload, _) = load(&args);
    let loaded = mem_line("after read");
    let runtime = runtime();
    let handle = runtime.handle().clone();
    let outline = args.outline && matches!(payload, Payload::Shapes(_));
    let one_source = args.one_source;
    let mut payload = Some(payload);

    gpui_platform::headless().run(move |cx: &mut App| {
        let view = cx.new(|_| {
            MapView::new(
                START.0,
                START.1,
                START.2,
                1024.0,
                768.0,
                gpui_maps::style::Style::default(),
                Arc::new(gpui_maps::tile_source::EmptyTileSource) as Arc<dyn TileSource>,
                handle,
            )
        });
        let before = allocation_stats();
        let tiles_before = gpui_maps::render::alloc::overlay_tile_census();
        let payload = payload.take().expect("run once");
        let pushed_at = Instant::now();
        let (fill_ms, outline_ms, expected) = view.update(cx, |view, cx| match &payload {
            Payload::Shapes(data) => push(view, data, outline, one_source, cx),
            Payload::GeoJson(text) => (push_geojson(view, text, cx), None, 1),
        });
        let push_total = pushed_at.elapsed();
        drop(payload);
        let after_push = mem_line("after push (input freed)");
        tile_index_row(view.read(cx).overlay());

        cx.spawn(async move |cx| {
            // Holding the entity is what keeps the view alive: a bake that
            // lands on a released `MapView` is dropped, never installed.
            let _view = view;
            let deadline = Instant::now() + Duration::from_secs(1800);
            let mut peak_heap = after_push.heap_in_use_bytes;
            let landed = loop {
                let s = allocation_stats();
                peak_heap = peak_heap.max(s.heap_in_use_bytes);
                // A tiled source (phase B) installs no whole-source mesh
                // unless it has points: its index landing is what counts.
                let tiled = gpui_maps::render::alloc::overlay_tile_census().index_builds_total
                    - tiles_before.index_builds_total;
                if s.overlay_mesh_installs_total - before.overlay_mesh_installs_total + tiled >= expected {
                    break Some(s);
                }
                if Instant::now() > deadline {
                    break None;
                }
                cx.background_executor().timer(Duration::from_millis(5)).await;
            };
            let tessellate = pushed_at.elapsed() - push_total;
            let Some(landed) = landed else {
                println!("TIMEOUT: the overlay did not land within 30 minutes");
                cx.update(|cx| cx.quit());
                return;
            };
            let installed = mem_line("after tessellation");
            let vertices = landed.overlay_bake_vertices_total - before.overlay_bake_vertices_total;
            let ops = landed.overlay_bake_ops_total - before.overlay_bake_ops_total;
            let vertex_size = std::mem::size_of::<gpui_maps::render::Vertex>() as u64;
            let vertex_bytes = vertices * vertex_size;
            let mesh_heap = installed.heap_in_use_bytes.saturating_sub(after_push.heap_in_use_bytes);
            let index_estimate = mesh_heap.saturating_sub(vertex_bytes) / 4;
            println!("-- result --");
            println!(
                "push (sync, UI thread): fill {:.1} ms{}",
                fill_ms.as_secs_f64() * 1e3,
                outline_ms.map(|d| format!(", outline {:.1} ms", d.as_secs_f64() * 1e3)).unwrap_or_default()
            );
            println!(
                "tessellate (off-thread, push end -> last install): {:.1} ms over {} bakes",
                tessellate.as_secs_f64() * 1e3,
                landed.overlay_bakes_total - before.overlay_bakes_total
            );
            println!("upload: n/a headless (no GPU); see the windowed run's gpu bytes");
            println!("output vertices {vertices} ({vertex_size} B each) = {}", gb(vertex_bytes));
            println!(
                "paint ops {ops}; mesh heap delta {} -> indices ~{index_estimate} (~{}) [ESTIMATE: heap delta minus vertex bytes]",
                gb(mesh_heap),
                gb(index_estimate * 4)
            );
            println!(
                "heap: read {} | after push {} | peak seen {} | resident mesh {}",
                gb(loaded.heap_in_use_bytes),
                gb(after_push.heap_in_use_bytes),
                gb(peak_heap),
                gb(installed.heap_in_use_bytes)
            );
            println!("peak RSS (getrusage ru_maxrss): {}", gb(peak_rss_bytes()));
            gate("headless.push_ms", fill_ms.as_secs_f64() * 1e3, "ms");
            gate("headless.rss_at_rest_mb", installed.rss_bytes as f64 / 1e6, "MB");
            gate("headless.heap_at_rest_mb", installed.heap_in_use_bytes as f64 / 1e6, "MB");
            gate("headless.heap_peak_mb", peak_heap as f64 / 1e6, "MB");
            let census = gpui_maps::render::alloc::overlay_tile_census();
            if census.index_builds_total > tiles_before.index_builds_total {
                println!(
                    "tiled: {} index builds landed, last {:.1} ms, {:.1} MB",
                    census.index_builds_total - tiles_before.index_builds_total,
                    census.index_build_nanos_last as f64 / 1e6,
                    census.index_bytes_last as f64 / 1e6
                );
                gate("headless.index_build_ms", census.index_build_nanos_last as f64 / 1e6, "ms");
                gate("headless.index_mb", census.index_bytes_last as f64 / 1e6, "MB");
                cx.update(|cx| cover_bake_row(_view.read(cx).overlay()));
                // The gate grades only the slider rows of a per-state run
                // (`headless_states.*`): its cover bakes would be discarded.
                if std::env::var_os("US_BG_GATE").is_some() && one_source {
                    cx.update(|cx| gate_covers(_view.read(cx).overlay()));
                }
            }
            #[cfg(feature = "probe")]
            cx.update(|cx| probe_report(_view.read(cx)));
            #[cfg(feature = "probe")]
            if one_source || std::env::var_os("US_BG_GATE").is_none() {
                cx.update(|cx| paint_and_hit_row(_view.read(cx)));
            }
            #[cfg(feature = "probe")]
            if std::env::var_os("US_BG_GATE").is_some() {
                let prefix = if one_source { "headless" } else { "headless_states" };
                // gpui-pre's `AsyncApp::update` is infallible (it panics
                // if the app is gone), so a table-only section cannot be
                // skipped silently and leave its rows MISSING in a pass.
                cx.update(|cx| gate_table_only(&_view, outline, prefix, cx));
            }
            // Peak last: the gate's own cover bakes are part of the run.
            gate("headless.peak_rss_mb", peak_rss_bytes() as f64 / 1e6, "MB");
            // What the gallery's switch away from a loaded census costs its
            // UI thread: the MapView's release (in the effect flush that
            // ends this update) and the allocator relief its slot runs
            // next. A human row, not a gate row.
            let (heap0, rss0, fp0) = (allocation_stats().heap_in_use_bytes, allocation_stats().rss_bytes, kit::phys_footprint_bytes());
            let started = Instant::now();
            cx.update(|_| drop(_view));
            let drop_ms = started.elapsed().as_secs_f64() * 1e3;
            let started = Instant::now();
            let relieved = gpui_maps::render::alloc::release_cached_free_memory();
            let relief_ms = started.elapsed().as_secs_f64() * 1e3;
            let after = allocation_stats();
            println!(
                "close: MapView drop {drop_ms:.1} ms (heap {} -> {}), relief {} in {relief_ms:.1} ms, rss {} -> {}, phys_footprint {} -> {}",
                gb(heap0),
                gb(after.heap_in_use_bytes),
                gb(relieved),
                gb(rss0),
                gb(after.rss_bytes),
                gb(fp0),
                gb(kit::phys_footprint_bytes())
            );
            // `US_BG_CLOSE_VMMAP=FILE`: where that footprint sits, for the
            // gallery's closed-census RSS question.
            if let Some(path) = std::env::var_os("US_BG_CLOSE_VMMAP") {
                if let Ok(out) = std::process::Command::new("vmmap").args(["--summary", &std::process::id().to_string()]).output() {
                    let _ = std::fs::write(path, out.stdout);
                }
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    drop(runtime);
}

/// A hovered feature's properties, the way an application reads them.
#[cfg(feature = "probe")]
fn properties_of(
    view: &MapView,
    feature: &gpui_maps::overlay::FeatureRef,
) -> Option<Arc<gpui_maps::overlay::FeatureProperties>> {
    view.feature_properties(feature)
}

/// B9: the paint-change re-class, and tooltip hit latency (`feature_at`
/// plus reading the hovered feature's properties) over a 64 x 48 grid of
/// the 1024 x 768 headless viewport.
#[cfg(feature = "probe")]
fn paint_and_hit_row(view: &MapView) {
    let names: Vec<_> = view.overlay().source_names().filter(|n| n.starts_with(FILL_SOURCE)).cloned().collect();
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let mut total = Duration::ZERO;
        for name in &names {
            total += view.overlay().probe_classify(name).unwrap_or_default();
        }
        best = best.min(total);
    }
    println!("classify (paint-change re-class), best of 3: {:.1} ms", best.as_secs_f64() * 1e3);
    gate("headless.classify_ms", best.as_secs_f64() * 1e3, "ms");
    let (mut hits, mut values, mut worst, mut total, mut probes) = (0usize, 0usize, Duration::ZERO, Duration::ZERO, 0u32);
    let mut fetch_total = Duration::ZERO;
    for gy in 0..48 {
        for gx in 0..64 {
            let (x, y) = (gx as f32 * 16.0 + 8.0, gy as f32 * 16.0 + 8.0);
            let started = Instant::now();
            let hit = view.feature_at(x, y);
            let fetch = Instant::now();
            let props = hit.and_then(|f| properties_of(view, &f));
            fetch_total += fetch.elapsed();
            let took = started.elapsed();
            if let Some(props) = props {
                hits += 1;
                values += props.len();
            }
            total += took;
            worst = worst.max(took);
            probes += 1;
        }
    }
    println!(
        "hit + properties: {probes} probes, {hits} hits ({values} values), mean {:.1} us, worst {:.1} us; properties alone {:.3} us per hit",
        total.as_secs_f64() * 1e6 / f64::from(probes),
        worst.as_secs_f64() * 1e6,
        fetch_total.as_secs_f64() * 1e6 / hits.max(1) as f64
    );
    gate("headless.hit_mean_us", total.as_secs_f64() * 1e6 / f64::from(probes), "us");
    gate("headless.hit_hits", hits as f64, "count");
}

/// The owner-by-owner split of what the overlay holds, for
/// `docs/measurements/2026-09-25-us-block-groups-breakdown.md`.
/// `US_BG_PROBE_SUBDIV=1` also re-tessellates every source with and without
/// `Subdivision::OVERLAY` (transient: one source's mesh, twice).
/// `US_BG_VMMAP=1` appends `vmmap --summary` of this process.
#[cfg(feature = "probe")]
fn probe_report(view: &MapView) {
    let c = view.overlay().probe_census();
    println!("-- probe: overlay store --");
    println!(
        "sources {} features {} rings {} coordinates {}",
        c.sources, c.features, c.rings, c.coordinates
    );
    println!(
        "features vec {} | geometry {} (coords {} at 16 B, ring/poly headers {}) | by_id {} | hit index {} | adopted tile index {}",
        gb(c.feature_vec_bytes as u64),
        gb(c.geometry_bytes as u64),
        gb(c.coordinate_bytes as u64),
        gb((c.geometry_bytes - c.coordinate_bytes) as u64),
        gb(c.by_id_bytes as u64),
        gb(c.hit_index_bytes as u64),
        gb(c.tile_index_bytes as u64)
    );
    println!("properties {}", gb(c.property_bytes as u64));
    gate("headless.property_mb", c.property_bytes as f64 / 1e6, "MB");
    gate("headless.store_features_mb", c.feature_vec_bytes as f64 / 1e6, "MB");
    println!(
        "plan: {} ops, {} (Geometry<i32> copy) | feature_ops {}",
        c.plan_ops,
        gb(c.plan_bytes as u64),
        gb(c.feature_ops_bytes as u64)
    );
    let [vl, vc, il, ic, spans, arena] = view.probe_overlay_mesh_census();
    let vsize = std::mem::size_of::<gpui_maps::render::Vertex>() as u64;
    println!("-- probe: retained CPU mesh (DrawnOverlayMesh) --");
    println!(
        "vertices len {vl} cap {vc} ({} len, {} cap) | indices len {il} cap {ic} ({} len, {} cap) | spans {} | arena {}",
        gb(vl * vsize),
        gb(vc * vsize),
        gb(il * 4),
        gb(ic * 4),
        gb(spans),
        gb(arena)
    );
    if std::env::var_os("US_BG_PROBE_SUBDIV").is_some() {
        let started = Instant::now();
        let [fv, fi, sv, si, max_op] = view.probe_overlay_subdivision_counts();
        println!(
            "-- probe: subdivision -- NONE {fv} vertices {fi} indices | OVERLAY {sv} vertices {si} indices \
             (+{:.2}% v, +{:.2}% i) | largest op {max_op} vertices | {:.1} s",
            (sv as f64 / fv as f64 - 1.0) * 100.0,
            (si as f64 / fi as f64 - 1.0) * 100.0,
            started.elapsed().as_secs_f64()
        );
        mem_line("after subdivision probe");
    }
    if std::env::var_os("US_BG_VMMAP").is_some() {
        vmmap_summary("");
        // `US_BG_VMMAP=release`: the same summary again after handing the
        // allocator's cached free pages back, to split retention from live.
        if std::env::var("US_BG_VMMAP").as_deref() == Ok("release") {
            let released = gpui_maps::render::alloc::release_cached_free_memory();
            println!("release_cached_free_memory() released {}", gb(released));
            mem_line("after release");
            vmmap_summary(" (after release)");
        }
    }
}

#[cfg(feature = "probe")]
fn vmmap_summary(label: &str) {
    let out = std::process::Command::new("vmmap")
        .args(["--summary", &std::process::id().to_string()])
        .output()
        .expect("vmmap");
    let text = String::from_utf8_lossy(&out.stdout);
    println!("-- probe: vmmap --summary{label} --");
    for line in text.lines().filter(|l| {
        l.contains("MALLOC")
            || l.contains("IOAccelerator")
            || l.contains("IOSurface")
            || l.contains("TOTAL")
            || l.contains("Physical footprint")
            || l.contains("REGION TYPE")
    }) {
        println!("{line}");
    }
}

/// The engine's frame timer over one reporting window. Only compiled into
/// the crate with `--features probe`; without it the fps from
/// `paint_census` is all there is.
#[cfg(feature = "probe")]
fn frame_times(s: &AllocationStats, last: &AllocationStats) -> String {
    let frames = s.frame_count - last.frame_count;
    let mean = if frames > 0 { (s.frame_nanos_sum - last.frame_nanos_sum) as f64 / frames as f64 / 1e6 } else { 0.0 };
    format!(
        " | cpu frame: +{frames} mean {mean:.2} ms, run p50 {} p99 {} max {:.1} ms",
        s.frame_ms_p50,
        s.frame_ms_p99,
        s.frame_nanos_max as f64 / 1e6
    )
}

#[cfg(not(feature = "probe"))]
fn frame_times(_: &AllocationStats, _: &AllocationStats) -> String {
    String::new()
}

/// Writes this process's first window -- the map's Metal layer and the gpui
/// legend over it, as composited -- to a PNG, through CoreGraphics'
/// `CGWindowListCreateImage` for the window's own number. A process may
/// image its own windows without the Screen Recording permission, so this
/// needs no helper tool and no focus games. The symbol is looked up at run
/// time: it is deprecated (ScreenCaptureKit replaces it) and a future
/// macOS may drop it, which this reports rather than failing to launch.
#[cfg(target_os = "macos")]
fn snapshot_window(path: &std::path::Path) -> Result<(u32, u32), String> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect([f64; 4]);
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGImageGetWidth(image: *const c_void) -> usize;
        fn CGImageGetHeight(image: *const c_void) -> usize;
        fn CGImageGetBytesPerRow(image: *const c_void) -> usize;
        fn CGImageGetBitsPerPixel(image: *const c_void) -> usize;
        fn CGImageGetDataProvider(image: *const c_void) -> *const c_void;
        fn CGDataProviderCopyData(provider: *const c_void) -> *const c_void;
        fn CGImageRelease(image: *const c_void);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFDataGetLength(data: *const c_void) -> isize;
        fn CFRelease(cf: *const c_void);
    }
    type CreateImage = unsafe extern "C" fn(CGRect, u32, u32, u32) -> *const c_void;
    let mtm = objc2::MainThreadMarker::new().ok_or("not on the main thread")?;
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    let window = app.windows().firstObject().ok_or("no window")?;
    let number = u32::try_from(window.windowNumber()).map_err(|_| "no window number")?;
    // SAFETY: dlsym on the process's own image list with a NUL-terminated
    // name; the result is checked before use and has the documented C
    // signature `CGImageRef CGWindowListCreateImage(CGRect, CGWindowListOption, CGWindowID, CGWindowImageOption)`.
    let create: CreateImage = unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"CGWindowListCreateImage".as_ptr());
        if sym.is_null() {
            return Err("CGWindowListCreateImage is not available on this macOS".into());
        }
        std::mem::transmute::<*mut c_void, CreateImage>(sym)
    };
    // CGRectNull; kCGWindowListOptionIncludingWindow (8);
    // kCGWindowImageBoundsIgnoreFraming (1).
    let null = CGRect([f64::INFINITY, f64::INFINITY, 0.0, 0.0]);
    // SAFETY: every CF/CG object is checked for null, read within the
    // lengths CoreGraphics reports, and released exactly once.
    unsafe {
        let image = create(null, 8, number, 1);
        if image.is_null() {
            return Err("CGWindowListCreateImage returned no image".into());
        }
        let (w, h, stride, bpp) =
            (CGImageGetWidth(image), CGImageGetHeight(image), CGImageGetBytesPerRow(image), CGImageGetBitsPerPixel(image));
        let data = CGDataProviderCopyData(CGImageGetDataProvider(image));
        CGImageRelease(image);
        if data.is_null() || bpp != 32 {
            return Err(format!("unexpected image format ({bpp} bits per pixel)"));
        }
        let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize);
        // BGRA, premultiplied, rows `stride` apart -> RGBA.
        let mut rgba = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            for px in bytes[row * stride..row * stride + w * 4].as_chunks::<4>().0.iter() {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        CFRelease(data);
        let buffer = image::RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or("buffer size")?;
        buffer.save(path).map_err(|e| e.to_string())?;
        Ok((w as u32, h as u32))
    }
}

#[cfg(not(target_os = "macos"))]
fn snapshot_window(_: &std::path::Path) -> Result<(u32, u32), String> {
    Err("--snapshot is macOS only".into())
}

fn windowed(args: Args) {
    println!("Drag to pan, scroll to zoom, Shift+drag to tilt/rotate. US Census block groups, TIGER/Line 2023.");
    let (payload, _) = load(&args);
    mem_line("after read");
    // Each block group's Mercator box and histogram bar, for the in-view
    // histogram (`in_view_histogram`); 16 bytes a block group.
    let view_items: Option<Arc<[ViewItem]>> = match &payload {
        Payload::Shapes(data) => {
            let started = Instant::now();
            let items: Arc<[ViewItem]> = view_items(data).into();
            println!("histogram boxes: {} block groups in {:.1} ms", items.len(), started.elapsed().as_secs_f64() * 1e3);
            Some(items)
        }
        Payload::GeoJson(_) => None,
    };
    let counties: Arc<HashMap<u32, String>> = Arc::new(load_county_names(&args.data));
    let (style, source) = basemap(args.basemap);
    let (imagery_style, satellite) = satellite();
    let runtime = runtime();
    let handle = runtime.handle().clone();
    let outline = args.outline && matches!(payload, Payload::Shapes(_));
    let quit_after = args.quit_after;
    let camera = args.camera;
    let range = args.range;
    let snapshot = args.snapshot.clone().map(|path| (path, args.snapshot_at));
    let pan = args.pan;
    let zoom_sweep = args.zoom_sweep.then_some(args.at);
    let slider_sweep = args.slider_sweep;
    let hover_sweep = args.hover_sweep;
    let panel_sweep = args.panel_sweep;
    let live = args.live.map(|mode| (mode, args.live_secs, args.live_zoom.map(|z| (args.at.0, args.at.1, z))));
    let one_source = args.one_source;
    let expected = std::rc::Rc::new(std::cell::Cell::new(1u64));
    let expected_in_window = expected.clone();
    let mut payload = Some(payload);
    let mut view_items = view_items;
    // `(GEOID, income)` per block group, for the state and metro
    // breakdowns (`DemoRoot::start_breakdowns`).
    let mut breakdown_rows: Option<Vec<(u64, Option<f64>)>> = match payload.as_ref() {
        Some(Payload::Shapes(data)) => Some(breakdown_rows_of(data)),
        _ => None,
    };
    let data_dir = args.data.clone();
    let mut satellite = satellite;
    let mut imagery_style = Some(imagery_style);

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let mut map_entity = None;
        let mut root_entity = None;
        let mut live_data: Option<Arc<LiveData>> = None;
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("US Block Groups".into()), ..Default::default() }),
                ..Default::default()
            },
            |_window, cx| {
                let map = cx.new(|_| {
                    let (lon, lat, zoom) = camera.unwrap_or(START);
                    let mut view = MapView::new(lon, lat, zoom, 1280.0, 800.0, style, source, handle);
                    // The legend card must draw over the map's own GPU
                    // layer, not under it -- see `DemoRoot`.
                    view.set_chrome_above_map(true);
                    // The hover tooltip follows every mouse move; the map
                    // under it only redraws when IT changed.
                    view.set_skip_unchanged_frames(std::env::var_os("US_BG_REDRAW_ALWAYS").is_none());
                    view
                });
                let payload = payload.take().expect("one window");
                let lookup: Arc<HashMap<u64, (Option<f64>, f64)>> = Arc::new(match &payload {
                    Payload::Shapes(data) => lookup_of(data),
                    Payload::GeoJson(_) => HashMap::new(),
                });
                map.update(cx, |view, cx| {
                    let (fill, line, sources) = match &payload {
                        Payload::Shapes(data) => push(view, data, outline, one_source, cx),
                        Payload::GeoJson(text) => (push_geojson(view, text, cx), None, 1),
                    };
                    expected_in_window.set(sources);
                    println!(
                        "push (sync): fill {:.1} ms{}",
                        fill.as_secs_f64() * 1e3,
                        line.map(|d| format!(", outline {:.1} ms", d.as_secs_f64() * 1e3)).unwrap_or_default()
                    );
                });
                let sources: Arc<[String]> = match &payload {
                    Payload::Shapes(data) if !one_source => {
                        data.states.iter().map(|(fips, _, _)| state_source(fips)).collect()
                    }
                    _ => Arc::new([FILL_SOURCE.to_string()]),
                };
                if live.is_some() {
                    if let Payload::Shapes(data) = &payload {
                        live_data = Some(Arc::new(LiveData::of(data)));
                    }
                }
                drop(payload);
                map_entity = Some(map.clone());
                // The other demos' keys -- t terminator, g globe, s
                // satellite -- from their shared shell, which owns the map
                // and is drawn in its place (see `kit/src/shell.rs`).
                let vector_style = map.read(cx).style().clone();
                let shell = cx.new(|cx| {
                    shell::DemoShell::with_satellite(
                        map.clone(),
                        "US Block Groups",
                        satellite.take().map(|(source, tile_size, name)| {
                            shell::Satellite::new(source, tile_size, name, false)
                                .with_styles(imagery_style.take().expect("one window"), vector_style)
                        }),
                        cx,
                    )
                });
                let root = cx.new(|cx| {
                    let mut root = DemoRoot::new(map, lookup, Arc::clone(&counties), sources, outline, cx);
                    root.shell = Some(shell);
                    if let Some((lo, hi)) = range {
                        root.request_range(IncomeRange { lo, hi }, cx);
                    }
                    root.panel_view.update(cx, |p, _| p.stats.log = panel_sweep);
                    match breakdown_rows.take() {
                        Some(features) => root.start_breakdowns(data_dir.clone(), features, view_items.clone(), cx),
                        None => root.set_breakdowns_error(
                            "the --geojson path reads no GEOIDs to join the population and metro tables to; run without it".into(),
                            cx,
                        ),
                    }
                    if let Some(items) = view_items.take() {
                        root.start_in_view(items, cx);
                    }
                    root
                });
                root_entity = Some(root.clone());
                root
            },
        )
        .expect("failed to open window");
        let root_entity = root_entity.expect("the window was built");
        // Frontmost, or the window server never asks it to paint.
        cx.activate(true);
        let map_entity = map_entity.expect("the window was built");

        if pan {
            // `START + 12 sin(0.2 t)` in longitude every 16 ms, as before.
            kit::pan_task(map_entity.downgrade(), START, cx);
        }

        if let Some(at) = zoom_sweep {
            zoom_sweep_task(map_entity.clone(), at, cx);
        }
        if slider_sweep {
            slider_sweep_task(root_entity.clone(), cx);
        }
        if hover_sweep {
            hover_sweep_task(root_entity.clone(), cx);
        }
        if panel_sweep {
            panel_sweep_task(root_entity.clone(), cx);
        }
        if let Some((path, at)) = snapshot {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs_f64(at)).await;
                cx.update(|_| match snapshot_window(&path) {
                    Ok((w, h)) => println!("snapshot: {} ({w} x {h} px)", path.display()),
                    Err(err) => eprintln!("snapshot failed: {err}"),
                });
            })
            .detach();
        }
        if let (Some((mode, secs, camera)), Some(data)) = (live, live_data.take()) {
            live_task(map_entity.clone(), data, mode, secs, camera, cx);
        }
        let _ = &root_entity;

        // Every 5 s: the engine's own frame and memory counters.
        cx.spawn(async move |cx| {
            let _map = map_entity;
            let started = Instant::now();
            let mut last = allocation_stats();
            let mut landed_at: Option<Duration> = None;
            let mut uploaded_at: Option<Duration> = None;
            #[cfg(feature = "probe")]
            let mut probed = false;
            let gpu_before = last.gpu_buffer_bytes_live;
            let mut last_paints = gpui_maps::render::paint_census().paints_submitted;
            let mut first_tile_frame_printed = false;
            let mut last_tiles = gpui_maps::render::alloc::overlay_tile_census();
            let mut pan_window: Option<kit::PanGate> = None;
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let s = allocation_stats();
                let t = started.elapsed();
                let tiles = gpui_maps::render::alloc::overlay_tile_census();
                if !first_tile_frame_printed && tiles.first_frame_nanos_last > 0 {
                    first_tile_frame_printed = true;
                    // A tiled source makes no +100 MB upload: its first
                    // overlay frame is what starts the steady-state probe.
                    uploaded_at = uploaded_at.or(Some(t));
                    println!(
                        "[{:>5.1}s] first overlay frame: {:.0} ms after push (tiles drawn: {}; index build {:.0} ms, {:.1} MB; {} tile bakes, {:.0} ms summed)",
                        t.as_secs_f64(),
                        tiles.first_frame_nanos_last as f64 / 1e6,
                        tiles.tile_draws_last_frame,
                        tiles.index_build_nanos_last as f64 / 1e6,
                        tiles.index_bytes_last as f64 / 1e6,
                        tiles.tile_bakes_total,
                        tiles.tile_bake_nanos_total as f64 / 1e6,
                    );
                }
                if landed_at.is_none() && s.overlay_mesh_installs_total >= expected.get() {
                    landed_at = Some(t);
                    println!("[{:>5.1}s] overlay tessellated and installed ({} vertices)", t.as_secs_f64(), s.overlay_bake_vertices_total);
                }
                if landed_at.is_some() && uploaded_at.is_none() && s.gpu_buffer_bytes_live > gpu_before + 100_000_000 {
                    uploaded_at = Some(t);
                    // Exact, unlike `--measure`'s heap-delta estimate: the
                    // GPU holds exactly the vertex and index buffers.
                    let uploaded = s.gpu_buffer_bytes_live - gpu_before;
                    let vertex_bytes =
                        s.overlay_bake_vertices_total * std::mem::size_of::<gpui_maps::render::Vertex>() as u64;
                    println!(
                        "[{:>5.1}s] uploaded: gpu +{} -> ~{} indices ({})",
                        t.as_secs_f64(),
                        gb(uploaded),
                        uploaded.saturating_sub(vertex_bytes) / 4,
                        gb(uploaded.saturating_sub(vertex_bytes))
                    );
                }
                // Once, 4 s after the upload, so the census reads the
                // steady state rather than the upload's staging copy.
                #[cfg(feature = "probe")]
                if let Some(at) = uploaded_at {
                    // Once: a late tick used to skip a 250 ms window.
                    if !probed && t >= at + Duration::from_secs(4) {
                        probed = true;
                        mem_line("probe (steady, windowed)");
                        cx.update(|cx| probe_report(_map.read(cx)));
                    }
                }
                if t.as_secs() / 5 != (t - Duration::from_millis(250)).as_secs() / 5 {
                    let census = gpui_maps::render::paint_census();
                    let fps = (census.paints_submitted - last_paints) as f64 / 5.0;
                    last_paints = census.paints_submitted;
                    println!(
                        "[{:>5.1}s] {fps:.1} fps (paints {} occluded {}){} | draws last {} | overlay tiles drawn {} baked +{} (+{} vertices) held {} | gpu live {} | rss {} peak {} | heap {}",
                        t.as_secs_f64(),
                        census.paints_submitted,
                        census.paints_skipped_occluded,
                        frame_times(&s, &last),
                        s.draw_calls_last_frame,
                        tiles.tile_draws_last_frame,
                        tiles.tile_bakes_total - last_tiles.tile_bakes_total,
                        tiles.tile_bake_vertices_total - last_tiles.tile_bake_vertices_total,
                        gb(tiles.tile_gpu_bytes),
                        gb(s.gpu_buffer_bytes_live),
                        gb(s.rss_bytes),
                        gb(peak_rss_bytes()),
                        gb(s.heap_in_use_bytes),
                    );
                    last = s;
                    last_tiles = tiles;
                }
                // The gate's pan window: from 5 s after the first overlay
                // frame (load and first bakes excluded) to the quit.
                if pan && pan_window.is_none() && uploaded_at.is_some_and(|at| t >= at + Duration::from_secs(5)) {
                    pan_window = Some(kit::PanGate::open());
                    #[cfg(feature = "probe")]
                    gpui_maps::render::zoom_probe::start("pan steady");
                }
                if let Some(limit) = quit_after {
                    if t.as_secs_f64() >= limit {
                        if let Some(pan_gate) = pan_window.take() {
                            pan_gate.report("pan");
                            #[cfg(feature = "probe")]
                            {
                                gpui_maps::render::zoom_probe::stop();
                                gate_frames("pan");
                            }
                        }
                        println!("quit after {limit}s; peak rss {}", gb(peak_rss_bytes()));
                        cx.update(|cx| cx.quit());
                        return;
                    }
                }
            }
        })
        .detach();
    });
    std::mem::forget(runtime);
}

/// `--panel-sweep`: see the doc comment at the top. Each phase ends with
/// one `panel[phase]:` line (`PanelStats::log`).
fn panel_sweep_task(root: Entity<DemoRoot>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let wait = |ms: u64| Duration::from_millis(ms);
        let deadline = Instant::now() + Duration::from_secs(300);
        while root.read_with(cx, |r, _| r.breakdowns.is_none()) {
            if Instant::now() > deadline {
                println!("panel sweep: no breakdowns within 300 s (fetch the population and metro tables); giving up");
                return;
            }
            cx.background_executor().timer(wait(100)).await;
        }
        // Let the first cover bake and the first in-view count land.
        cx.background_executor().timer(wait(6_000)).await;
        cx.update(|cx| cx.activate(true));
        let panel = root.read_with(cx, |r, _| r.panel_view.clone());
        // Each phase line, then the window brought back to the front: a
        // window that is not frontmost does not paint, and its phase would
        // read as "nothing redrew" when nothing was drawn at all (a phase
        // with `renders` 0 is void).
        let log = |label: &str, cx: &mut gpui::AsyncApp| {
            panel.update(cx, |p, _| p.stats.flush(&format!("panel[{label}]")));
            cx.update(|cx| cx.activate(true));
        };
        log("idle", cx);
        // The baseline for the next two: the same 2.4 s with no input (tile
        // loads and bakes still landing are the map's own paints).
        cx.background_executor().timer(wait(2_400)).await;
        log("quiet, no input", cx);
        // Panel-only changes: every one re-renders the window, and none may
        // cost the map a GPU pass -- `map paints +0` is the check.
        let set = |t: panel::Tab, sort: panel::StateSort, cx: &mut gpui::AsyncApp| {
            panel
                .update(cx, |p, cx| {
                    p.state.tab = t;
                    p.state.sort = sort;
                    cx.notify();
                });
        };
        for sort in [panel::StateSort::Share, panel::StateSort::Population, panel::StateSort::Median].repeat(2) {
            set(panel::Tab::States, sort, cx);
            cx.background_executor().timer(wait(400)).await;
        }
        log("sort clicks", cx);
        for tab in [panel::Tab::Metros, panel::Tab::InView, panel::Tab::States].repeat(2) {
            set(tab, panel::StateSort::Median, cx);
            cx.background_executor().timer(wait(400)).await;
        }
        log("tab clicks", cx);
        // A 40-step window dragged across the range and back, one move a
        // frame, as `--slider-sweep` does.
        async fn drag(root: &Entity<DemoRoot>, secs: f64, cx: &mut gpui::AsyncApp) {
            const WINDOW: usize = 40;
            let last = slider_stops().len() - 1;
            let started = Instant::now();
            while started.elapsed().as_secs_f64() < secs {
                let u = started.elapsed().as_secs_f64() / secs * 2.0;
                let phase = if u < 1.0 { u } else { 2.0 - u };
                let lo = ((last - WINDOW) as f64 * phase).round() as usize;
                root.update(cx, |r, cx| r.request_range(IncomeRange { lo, hi: lo + WINDOW }, cx));
                cx.background_executor().timer(Duration::from_millis(16)).await;
            }
        }
        set(panel::Tab::States, panel::StateSort::Median, cx);
        drag(&root, 5.0, cx).await;
        log("states drag", cx);
        // Let the drag's last paint updates land before counting the hover's.
        cx.background_executor().timer(wait(1_000)).await;
        root.update(cx, |r, _| r.stats = SliderStats::default());
        for state in [Some(6), Some(48), Some(36), None] {
            panel.update(cx, |p, cx| {
                p.point_at_state(state, cx);
                cx.notify();
            });
            cx.background_executor().timer(wait(1_500)).await;
        }
        {
            let s = root.update(cx, |r, _| std::mem::take(&mut r.stats));
            let mut sync = s.sync.clone();
            sync.sort_unstable();
            println!(
                "panel state hover: {} paint updates ({} table-only, {} re-bake, {} replan), sync p50 {:.2} ms max {:.2} ms",
                sync.len(),
                s.table_only,
                s.rebake,
                s.replan,
                sync.get(sync.len() / 2).map_or(0.0, |d| d.as_secs_f64() * 1e3),
                sync.last().map_or(0.0, |d| d.as_secs_f64() * 1e3),
            );
        }
        log("state hover (dims the map)", cx);
        set(panel::Tab::States, panel::StateSort::Share, cx);
        drag(&root, 3.0, cx).await;
        log("states by share drag", cx);
        set(panel::Tab::Metros, panel::StateSort::Share, cx);
        drag(&root, 5.0, cx).await;
        log("metros drag", cx);
        set(panel::Tab::InView, panel::StateSort::Share, cx);
        drag(&root, 3.0, cx).await;
        log("in view drag", cx);
        let fly = |code: u32, metro: bool, cx: &mut gpui::AsyncApp| {
            root.update(cx, |r, cx| {
                let b = r.breakdowns.clone().expect("waited for it");
                let groups = if metro { &b.metros } else { &b.states };
                if let Some(bounds) = groups.iter().find(|g| g.code == code).and_then(|g| g.bounds) {
                    let fit = bounds.fit(1280.0 - f64::from(panel::PANEL_W + panel::PANEL_MARGIN), 800.0, 40.0, 12.0);
                    println!("panel sweep: fly to {code}: bounds {bounds:?} -> centre ({:.2}, {:.2}) z{:.2}", fit.0, fit.1, fit.2);
                    r.fly_to_bounds(bounds, 12.0, size(px(1280.0), px(800.0)), panel::PANEL_W + panel::PANEL_MARGIN, cx);
                }
            });
        };
        let camera = |label: &str, cx: &mut gpui::AsyncApp| {
            root.read_with(cx, |r, cx| {
                let c = *r.map.read(cx).camera();
                println!("panel sweep: after the fly to {label}, camera at ({:.2}, {:.2}) z{:.2}", c.center_lon, c.center_lat, c.zoom);
            });
        };
        for (label, code, metro) in [("California", 6, false), ("Alaska", 2, false), ("Houston", 26420, true)] {
            fly(code, metro, cx);
            cx.background_executor().timer(wait(4_000)).await;
            camera(label, cx);
            log(&format!("fly to {label}"), cx);
        }
        panel
            .update(cx, |p, _| {
                if let (Some(v), Some(b)) = (&p.in_view, p.breakdowns.as_deref()) {
                    let names: Vec<String> = v.metros.iter().take(3).map(|m| panel::short_metro(&b.metros[m.group].name)).collect();
                    println!("panel sweep: in view now {} block groups; top metros {names:?}", v.block_groups);
                }
            });
        root.update(cx, |r, cx| r.request_range(IncomeRange::full(), cx));
        println!("panel sweep: done");
    })
    .detach();
}

/// `--zoom-sweep`: see the doc comment at the top. One segment per leg, so
/// the report separates the zoom itself from the settle after it.
#[cfg(feature = "probe")]
fn zoom_sweep_task(map: Entity<MapView>, at: (f64, f64), cx: &mut App) {
    use gpui_maps::render::zoom_probe;
    cx.spawn(async move |cx| {
        let tick = Duration::from_millis(16);
        // Bounded: the national load and index build take well under this.
        let deadline = Instant::now() + Duration::from_secs(300);
        while gpui_maps::render::alloc::overlay_tile_census().first_frame_nanos_last == 0 {
            if Instant::now() > deadline {
                println!("zoom sweep: no overlay frame within 300 s; giving up");
                return;
            }
            cx.background_executor().timer(Duration::from_millis(100)).await;
        }
        let set = |cx: &mut gpui::AsyncApp, z: f64| {
            zoom_probe::note_zoom(z);
            map.update(cx, |view, cx| {
                view.set_camera(at.0, at.1, z);
                cx.notify();
            });
            true
        };
        // Park at z3 over the target and let it settle, unrecorded.
        let park = Instant::now();
        while park.elapsed() < Duration::from_secs(4) {
            if !set(cx, 3.0) {
                return;
            }
            cx.background_executor().timer(tick).await;
        }
        let occluded_before = gpui_maps::render::paint_census().paints_skipped_occluded;
        zoom_probe::start("parked z3");
        let hold = |secs: f64| (None::<(f64, f64)>, secs);
        let legs: Vec<(&str, (Option<(f64, f64)>, f64))> = vec![
            ("parked z3", hold(1.0)),
            ("slow in #1 (z3->z12, 4 s)", (Some((3.0, 12.0)), 4.0)),
            ("settle z12 #1", hold(3.0)),
            ("slow out #1 (z12->z3, 4 s)", (Some((12.0, 3.0)), 4.0)),
            ("settle z3 #1", hold(3.0)),
            ("slow in #2", (Some((3.0, 12.0)), 4.0)),
            ("settle z12 #2", hold(3.0)),
            ("slow out #2", (Some((12.0, 3.0)), 4.0)),
            ("settle z3 #2", hold(3.0)),
            ("flick in #1 (z3->z12, 0.6 s)", (Some((3.0, 12.0)), 0.6)),
            ("settle z12 #3", hold(3.0)),
            ("flick out #1 (z12->z3, 0.6 s)", (Some((12.0, 3.0)), 0.6)),
            ("settle z3 #3", hold(3.0)),
            ("flick in #2", (Some((3.0, 12.0)), 0.6)),
            ("settle z12 #4", hold(3.0)),
            ("flick out #2", (Some((12.0, 3.0)), 0.6)),
            ("settle z3 #4", hold(3.0)),
        ];
        let mut z = 3.0;
        // Per leg: (label, rss max, heap max, rss at end, heap at end).
        let mut mem_rows: Vec<(&str, u64, u64, u64, u64)> = Vec::new();
        let at_rest = allocation_stats();
        let (mut basemap_gpu_max, mut gpu_live_max) = ((0usize, 0usize), 0u64);
        for (i, (label, (ramp, secs))) in legs.into_iter().enumerate() {
            if i > 0 {
                zoom_probe::mark(label);
            }
            // Frontmost again at every leg: a window another app covers
            // mid-sweep stops painting, and the leg would measure nothing.
            cx.update(|cx| cx.activate(true));
            let started = Instant::now();
            let (mut rss_max, mut heap_max) = (0u64, 0u64);
            loop {
                let (rss, heap) =
                    (gpui_maps::render::alloc::rss_bytes(), gpui_maps::render::alloc::heap_in_use_bytes());
                rss_max = rss_max.max(rss);
                heap_max = heap_max.max(heap);
                // The basemap's GPU meshes (`gpu_meshes`, device bytes) and
                // the whole live GPU buffer figure, at their sweep maxima.
                let (bytes, tiles) = map
                    .read_with(cx, |view, _| {
                        let bytes = view.cache_occupancy_bytes().iter().find(|r| r.0 == "gpu_meshes").map_or(0, |r| r.1);
                        let tiles = view.cache_occupancy().iter().find(|r| r.0 == "gpu_meshes").map_or(0, |r| r.1);
                        (bytes, tiles)
                    });
                if bytes > basemap_gpu_max.0 {
                    basemap_gpu_max = (bytes, tiles);
                }
                gpu_live_max = gpu_live_max.max(allocation_stats().gpu_buffer_bytes_live);
                let t = (started.elapsed().as_secs_f64() / secs).min(1.0);
                if let Some((from, to)) = ramp {
                    // Smoothstep: a gesture accelerates and decelerates.
                    let e = t * t * (3.0 - 2.0 * t);
                    z = from + (to - from) * e;
                }
                if !set(cx, z) {
                    return;
                }
                if t >= 1.0 {
                    mem_rows.push((
                        label,
                        rss_max,
                        heap_max,
                        gpui_maps::render::alloc::rss_bytes(),
                        gpui_maps::render::alloc::heap_in_use_bytes(),
                    ));
                    break;
                }
                cx.background_executor().timer(tick).await;
            }
        }
        zoom_probe::stop();
        println!(
            "-- sweep basemap GPU meshes: max {} over {} tiles ({} per tile) | GPU buffers live max {} --",
            gb(basemap_gpu_max.0 as u64),
            basemap_gpu_max.1,
            gb((basemap_gpu_max.0 / basemap_gpu_max.1.max(1)) as u64),
            gb(gpu_live_max)
        );
        println!(
            "-- sweep memory: at rest before the sweep rss {} heap {} --",
            gb(at_rest.rss_bytes),
            gb(at_rest.heap_in_use_bytes)
        );
        for (label, rss_max, heap_max, rss_end, heap_end) in &mem_rows {
            println!(
                "  {label:<32} rss max {:>10} end {:>10} | heap max {:>10} end {:>10}",
                gb(*rss_max),
                gb(*rss_end),
                gb(*heap_max),
                gb(*heap_end)
            );
        }
        // Discriminator: what the allocator hands back is freed memory it
        // was caching, not live data.
        let heap_before = gpui_maps::render::alloc::heap_in_use_bytes();
        let released = gpui_maps::render::alloc::release_cached_free_memory();
        println!(
            "  after the sweep: heap {} -> release_cached_free_memory() released {} -> heap {} rss {} | peak rss {}",
            gb(heap_before),
            gb(released),
            gb(gpui_maps::render::alloc::heap_in_use_bytes()),
            gb(gpui_maps::render::alloc::rss_bytes()),
            gb(peak_rss_bytes())
        );
        let stats = allocation_stats();
        println!(
            "-- zoom sweep over ({:.2}, {:.2}); draw_calls_total {}; occluded paints during the sweep {} --",
            at.0,
            at.1,
            stats.draw_calls_total,
            gpui_maps::render::paint_census().paints_skipped_occluded - occluded_before
        );
        // Every basemap fetch is a decode: a decoded-cache eviction shows
        // up here as a re-fetch of a tile the sweep already visited.
        println!(
            "-- basemap during the sweep: tile fetches (each one decode) {} | fetched {} --",
            stats.tile_fetches_total - at_rest.tile_fetches_total,
            gb(stats.tile_fetch_bytes_total - at_rest.tile_fetch_bytes_total)
        );
        println!("{}", zoom_probe::report());
        let prefix = if at.0 < -100.0 { "zoom_la" } else { "zoom_dc" };
        gate(
            &format!("{prefix}.occluded_paints"),
            (gpui_maps::render::paint_census().paints_skipped_occluded - occluded_before) as f64,
            "paints",
        );
        gate(&format!("{prefix}.draw_calls"), (stats.draw_calls_total - at_rest.draw_calls_total) as f64, "draws");
        gate(&format!("{prefix}.rss_at_rest_mb"), at_rest.rss_bytes as f64 / 1e6, "MB");
        gate(
            &format!("{prefix}.rss_leg_max_mb"),
            mem_rows.iter().map(|r| r.1).max().unwrap_or(0) as f64 / 1e6,
            "MB",
        );
        gate_frames(prefix);
        if let Some(path) = std::env::var_os("US_BG_ZOOM_CSV") {
            std::fs::write(&path, zoom_probe::frames_csv()).expect("write the zoom CSV");
            println!("frames csv: {}", PathBuf::from(path).display());
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(not(feature = "probe"))]
fn zoom_sweep_task(_: Entity<MapView>, _: (f64, f64), _: &mut App) {
    panic!("--zoom-sweep needs --features probe");
}

/// `--slider-sweep`: see the doc comment at the top.
#[cfg(feature = "probe")]
fn slider_sweep_task(root: Entity<DemoRoot>, cx: &mut App) {
    use gpui_maps::render::zoom_probe;
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + Duration::from_secs(300);
        while gpui_maps::render::alloc::overlay_tile_census().first_frame_nanos_last == 0 {
            if Instant::now() > deadline {
                println!("slider sweep: no overlay frame within 300 s; giving up");
                return;
            }
            cx.background_executor().timer(Duration::from_millis(100)).await;
        }
        // Let the first cover finish baking, unrecorded.
        cx.background_executor().timer(Duration::from_secs(6)).await;
        cx.update(|cx| cx.activate(true));
        // Settled AND checked: an unsnapped edge is written as a table at
        // once and only found to split a class by the full check behind it.
        let settled = |cx: &mut gpui::AsyncApp| {
            cx.update(|cx| {
                let root = root.read(cx);
                let view = root.map.read(cx);
                root.settled(cx) && root.sources.iter().all(|name| view.source_paint_verified(name))
            })
        };
        let epochs = |cx: &mut gpui::AsyncApp| {
            cx.update(|cx| {
                let root = root.read(cx);
                let view = root.map.read(cx);
                root.sources.iter().filter_map(|name| view.source_paint_epoch(name)).sum::<u64>()
            })
        };
        let epochs_at_start = epochs(cx);

        // Phase 1: drag a 40-step window across the range and back, one
        // update every 2 ms tick (the loop runs at the frame rate), every edge snapped.
        const WINDOW: usize = 40;
        let last = slider_stops().len() - 1;
        root.update(cx, |r, _| r.stats = SliderStats::default());
        let (paints_before, draws_before) =
            (gpui_maps::render::paint_census().paints_submitted, allocation_stats().draw_calls_total);
        let occluded_before = gpui_maps::render::paint_census().paints_skipped_occluded;
        zoom_probe::start("slider drag (snapped)");
        let started = Instant::now();
        let secs = 10.0;
        loop {
            let t = started.elapsed().as_secs_f64();
            if t >= secs {
                break;
            }
            let u = t / secs * 2.0;
            let phase = if u < 1.0 { u } else { 2.0 - u };
            let lo = ((last - WINDOW) as f64 * phase).round() as usize;
            root.update(cx, |r, cx| r.request_range(IncomeRange { lo, hi: lo + WINDOW }, cx));
            cx.background_executor().timer(Duration::from_millis(2)).await;
        }
        let drag = started.elapsed();
        let paints = gpui_maps::render::paint_census().paints_submitted - paints_before;
        let draws = allocation_stats().draw_calls_total - draws_before;
        let drag_stats = root.update(cx, |r, _| std::mem::take(&mut r.stats));

        // Phase 2: forced re-bakes -- an unsnapped lower edge splits a
        // class, the snapped range after it merges it back. Each waits for
        // the previous picture to settle, then times its own.
        let epochs_after_drag = epochs(cx);
        zoom_probe::mark("forced re-bakes");
        let outline = root.read_with(cx, |r, _| r.outline);
        let mut settle: Vec<Duration> = Vec::new();
        let mut rebake_sync: Vec<Duration> = Vec::new();
        for i in 0..10usize {
            let wait = Instant::now();
            while !settled(cx) && wait.elapsed() < Duration::from_secs(5) {
                cx.background_executor().timer(Duration::from_millis(1)).await;
            }
            let lo = 30 + 17 * (i / 2);
            let range = IncomeRange { lo, hi: lo + WINDOW };
            let json = if i % 2 == 0 {
                let stops = slider_stops();
                range_paint_json(outline, Some((stops[lo] + stops[lo + 1]) / 2.0), range.bounds().1)
            } else {
                range.paint_json(outline)
            };
            let paint = paint_object(&json);
            let t0 = Instant::now();
            root.update(cx, |r, cx| {
                r.apply_paint(&paint, cx);
                r.wanted = range;
                r.applied = range;
                cx.notify();
            });
            rebake_sync.push(t0.elapsed());
            while !settled(cx) {
                if t0.elapsed() > Duration::from_secs(10) {
                    println!("slider sweep: re-bake {i} did not settle within 10 s");
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(1)).await;
            }
            settle.push(t0.elapsed());
        }
        let rebake_stats = root.update(cx, |r, _| std::mem::take(&mut r.stats));
        let epochs_after_rebakes = epochs(cx);
        zoom_probe::stop();

        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let pct = |v: &[Duration], q: f64| -> f64 {
            let mut v: Vec<f64> = v.iter().map(|d| ms(*d)).collect();
            v.sort_by(f64::total_cmp);
            if v.is_empty() {
                return f64::NAN;
            }
            v[((v.len() - 1) as f64 * q).round() as usize]
        };
        println!("-- slider sweep --");
        println!(
            "drag (snapped): {} updates in {:.1} s | paths: table-only {} re-bake {} replan {} | update (sync) p50 {:.2} ms max {:.2} ms | {:.1} fps ({} paints, {} draw calls)",
            drag_stats.sync.len(),
            drag.as_secs_f64(),
            drag_stats.table_only,
            drag_stats.rebake,
            drag_stats.replan,
            pct(&drag_stats.sync, 0.5),
            pct(&drag_stats.sync, 1.0),
            paints as f64 / drag.as_secs_f64(),
            paints,
            draws,
        );
        println!(
            "drag re-bakes (tile epochs advanced): {}",
            epochs_after_drag - epochs_at_start
        );
        println!(
            "forced re-bakes: {} (tile epochs advanced {}) | paths at the call: table-only {} re-bake {} replan {} | update (sync) p50 {:.2} ms max {:.2} ms | to a whole, checked new frame p50 {:.1} ms max {:.1} ms",
            settle.len(),
            epochs_after_rebakes - epochs_after_drag,
            rebake_stats.table_only,
            rebake_stats.rebake,
            rebake_stats.replan,
            pct(&rebake_sync, 0.5),
            pct(&rebake_sync, 1.0),
            pct(&settle, 0.5),
            pct(&settle, 1.0),
        );
        println!("forced re-bakes, each (ms, even = unsnapped edge): {:?}", settle.iter().map(|d| ms(*d).round()).collect::<Vec<_>>());
        println!("{}", zoom_probe::report());
        gate(
            "slider.occluded_paints",
            (gpui_maps::render::paint_census().paints_skipped_occluded - occluded_before) as f64,
            "paints",
        );
        gate("slider.draw_calls", draws as f64, "draws");
        gate("slider.updates", drag_stats.sync.len() as f64, "count");
        gate("slider.drag_p50_ms", pct(&drag_stats.sync, 0.5), "ms");
        gate("slider.drag_max_ms", pct(&drag_stats.sync, 1.0), "ms");
        gate("slider.drag_other_path", (drag_stats.rebake + drag_stats.replan) as f64, "count");
        gate("slider.drag_frame_interval_ms", drag.as_secs_f64() * 1e3 / paints.max(1) as f64, "ms");
        gate("slider.rebake_settle_p50_ms", pct(&settle, 0.5), "ms");
        gate("slider.rebake_settle_max_ms", pct(&settle, 1.0), "ms");
        gate_frames("slider");
        cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(not(feature = "probe"))]
fn slider_sweep_task(_: Entity<DemoRoot>, _: &mut App) {
    panic!("--slider-sweep needs --features probe");
}

/// `--hover-sweep`: how the hover tooltip keeps up with the cursor.
///
/// Over New York at z4 and at z10, sends synthetic `MouseMoveEvent`s
/// through `Window::dispatch_event` -- the path a real move takes -- at
/// 120 Hz for 5 s along a Lissajous curve, and records every `DemoRoot`
/// render (the cursor the tooltip was drawn at, and whether the MAP had
/// asked to be redrawn) and every `MapView::paint` (its duration and the
/// time spent in `get_current_texture`). Reports, per view: how late each
/// move was dispatched (a stalled main thread shows up here first), how
/// long from a move to the render that shows it, the longest gap between
/// renders while the cursor was moving, the map's paint and acquire
/// times, and how many window draws repainted the map only because the
/// tooltip moved. Every figure is gated on the map having drawn.
#[cfg(feature = "probe")]
fn hover_sweep_task(root: Entity<DemoRoot>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + Duration::from_secs(300);
        while gpui_maps::render::alloc::overlay_tile_census().first_frame_nanos_last == 0 {
            if Instant::now() > deadline {
                println!("hover sweep: no overlay frame within 300 s; giving up");
                return;
            }
            cx.background_executor().timer(Duration::from_millis(100)).await;
        }
        cx.background_executor().timer(Duration::from_secs(6)).await;
        let Some(window) = cx.update(|cx| cx.windows().first().copied()) else {
            println!("hover sweep: no window");
            return;
        };
        let map = root.read_with(cx, |r, _| r.map.clone());
        let Some(target) = window.update(cx, |_, window, _| MouseTarget::of(window)).ok().flatten() else {
            println!("hover sweep: no AppKit view to deliver moves to");
            return;
        };
        let ms = |nanos: f64| nanos / 1e6;
        let pct = |v: &[f64], q: f64| -> f64 {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            if v.is_empty() {
                return f64::NAN;
            }
            v[((v.len() - 1) as f64 * q).round() as usize]
        };
        for (label, zoom) in [("z4", 4.0), ("z10", 10.0)] {
            cx.update(|cx| cx.activate(true));
            map.update(cx, |v, cx| {
                v.set_camera(-73.95, 40.72, zoom);
                cx.notify();
            });
            // Park the cursor, then let the cover bake and land.
            let centre = gpui::point(px(640.0), px(400.0));
            send_mouse_moved(target, centre);
            let wait = Instant::now();
            cx.background_executor().timer(Duration::from_secs(4)).await;
            while !cx.update(|cx| root.read(cx).settled(cx)) && wait.elapsed() < Duration::from_secs(30) {
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;

            let census0 = gpui_maps::render::paint_census();
            let skipped0 = gpui_maps::render::paints_skipped_unchanged();
            let draws0 = allocation_stats().draw_calls_total;
            gpui_maps::render::frame_log_start();
            root.update(cx, |r, cx| r.popup.update(cx, |p, _| p.hover_log = Some(Vec::new())));
            let started = Instant::now();
            let started_engine = gpui_maps::timing::Instant::now();
            let tick = Duration::from_secs_f64(1.0 / 120.0);
            let secs = 5.0;
            // (scheduled, dispatched, position)
            let mut moves: Vec<(Instant, Instant, gpui::Point<gpui::Pixels>)> = Vec::new();
            let mut handled: Vec<f64> = Vec::new();
            let mut i = 0u32;
            loop {
                let scheduled = started + tick * i;
                let now = Instant::now();
                if now < scheduled {
                    cx.background_executor().timer(scheduled - now).await;
                }
                let t = started.elapsed().as_secs_f64();
                if t >= secs {
                    break;
                }
                let at = gpui::point(
                    px(640.0 + 300.0 * (t * 2.0 * std::f64::consts::PI / 2.3).sin() as f32),
                    px(400.0 + 180.0 * (t * 2.0 * std::f64::consts::PI / 1.7).sin() as f32),
                );
                let sent = Instant::now();
                send_mouse_moved(target, at);
                handled.push(ms(sent.elapsed().as_nanos() as f64));
                moves.push((scheduled, sent, at));
                i += 1;
            }
            // One more frame to land the last move.
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let renders = root.update(cx, |r, cx| r.popup.update(cx, |p, _| p.hover_log.take().unwrap_or_default()));
            let paints = gpui_maps::render::frame_log_take();
            let census = gpui_maps::render::paint_census();
            let draws = allocation_stats().draw_calls_total - draws0;
            let occluded = census.paints_skipped_occluded - census0.paints_skipped_occluded;
            let submitted = census.paints_submitted - census0.paints_submitted;
            let skipped = gpui_maps::render::paints_skipped_unchanged() - skipped0;

            let late: Vec<f64> = moves.iter().map(|(s, d, _)| ms(d.saturating_duration_since(*s).as_nanos() as f64)).collect();
            // Move -> the first render at or after it, and whether that
            // render drew the tooltip at that very move's position (or a
            // later one's).
            let mut to_render = Vec::new();
            let mut unmatched = 0usize;
            for (idx, (_, sent, _)) in moves.iter().enumerate() {
                match renders.iter().find(|r| r.at >= *sent) {
                    Some(r) => {
                        to_render.push(ms((r.at - *sent).as_nanos() as f64));
                        // AppKit's bottom-left round trip moves a position by
                        // a few ULPs, so "the same position" is within 0.01 px.
                        let same = |p: &gpui::Point<gpui::Pixels>| {
                            (f32::from(p.x) - f32::from(r.cursor.x)).abs() < 0.01
                                && (f32::from(p.y) - f32::from(r.cursor.y)).abs() < 0.01
                        };
                        if !moves[idx..].iter().take_while(|(_, s, _)| *s <= r.at).any(|(_, _, p)| same(p)) {
                            unmatched += 1;
                            if unmatched <= 5 {
                                let shown = moves.iter().position(|(_, _, p)| same(p));
                                println!(
                                    "  stale render: move {idx} sent, render {:.2} ms later shows move {shown:?} (cursor {:?})",
                                    ms((r.at - *sent).as_nanos() as f64),
                                    r.cursor
                                );
                            }
                        }
                    }
                    None => unmatched += 1,
                }
            }
            let gaps: Vec<f64> = renders.windows(2).map(|w| ms((w[1].at - w[0].at).as_nanos() as f64)).collect();
            let mut worst: Vec<(f64, f64)> = renders
                .windows(2)
                .map(|w| (ms((w[1].at - w[0].at).as_nanos() as f64), ms(w[0].at.saturating_duration_since(started).as_nanos() as f64)))
                .collect();
            worst.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (gap, at) in worst.iter().take(3) {
                let during = paints
                    .iter()
                    .filter(|p| {
                        let end = ms(p.ended_at.duration_since(started_engine).as_nanos() as f64);
                        end > *at && end <= at + gap + 0.5
                    })
                    .map(|p| ms(p.paint_nanos as f64))
                    .collect::<Vec<_>>();
                println!("  render gap {gap:.1} ms starting {at:.0} ms into the sweep; map paints ending inside it: {during:.1?}");
            }
            let paint_ms: Vec<f64> = paints.iter().map(|p| ms(p.paint_nanos as f64)).collect();
            let acquire_ms: Vec<f64> = paints.iter().map(|p| ms(p.acquire_nanos as f64)).collect();
            let host_only = renders.iter().filter(|r| !r.map_notified).count();
            let tooltips = renders.iter().filter(|r| r.tooltip).count();
            println!("-- hover sweep {label} (NYC, zoom {zoom}) --");
            println!(
                "moves {} in {:.1} s | dispatch late p50 {:.2} p99 {:.2} max {:.2} ms",
                moves.len(),
                secs,
                pct(&late, 0.5),
                pct(&late, 0.99),
                pct(&late, 1.0)
            );
            println!(
                "move handled (synchronous: hit test, listeners, highlight) p50 {:.2} p99 {:.2} max {:.2} ms, total {:.0} ms",
                pct(&handled, 0.5),
                pct(&handled, 0.99),
                pct(&handled, 1.0),
                handled.iter().sum::<f64>()
            );
            println!(
                "renders {} ({} with a tooltip) | move->render p50 {:.2} p99 {:.2} max {:.2} ms | render gap p50 {:.2} p99 {:.2} max {:.2} ms | renders not at the latest cursor {}",
                renders.len(),
                tooltips,
                pct(&to_render, 0.5),
                pct(&to_render, 0.99),
                pct(&to_render, 1.0),
                pct(&gaps, 0.5),
                pct(&gaps, 0.99),
                pct(&gaps, 1.0),
                unmatched
            );
            println!(
                "map paints {} (submitted {}, occluded {}, draw calls {}) | paint p50 {:.2} p99 {:.2} max {:.2} ms | get_current_texture p50 {:.2} p99 {:.2} max {:.2} ms, total {:.0} ms",
                paints.len(),
                submitted,
                occluded,
                draws,
                pct(&paint_ms, 0.5),
                pct(&paint_ms, 0.99),
                pct(&paint_ms, 1.0),
                pct(&acquire_ms, 0.5),
                pct(&acquire_ms, 0.99),
                pct(&acquire_ms, 1.0),
                acquire_ms.iter().sum::<f64>()
            );
            println!(
                "window draws where the map had not asked to redraw (tooltip-only): {host_only} of {} | map paints that skipped the GPU pass: {skipped}",
                renders.len()
            );
            if draws == 0 || submitted == 0 {
                println!("hover sweep {label}: INVALID -- the map drew nothing (window not frontmost, or the screen is locked)");
            }
            gate(&format!("hover.{label}.draw_calls"), draws as f64, "draws");
            gate(&format!("hover.{label}.occluded_paints"), occluded as f64, "paints");
            gate(&format!("hover.{label}.move_to_render_p99_ms"), pct(&to_render, 0.99), "ms");
            gate(&format!("hover.{label}.move_to_render_max_ms"), pct(&to_render, 1.0), "ms");
            gate(&format!("hover.{label}.render_gap_max_ms"), pct(&gaps, 1.0), "ms");
            gate(&format!("hover.{label}.move_handled_max_ms"), pct(&handled, 1.0), "ms");
            gate(&format!("hover.{label}.acquire_max_ms"), pct(&acquire_ms, 1.0), "ms");
            gate(&format!("hover.{label}.map_paints"), paints.len() as f64, "count");
            gate(&format!("hover.{label}.tooltip_only_draws"), host_only as f64, "count");
            gate(&format!("hover.{label}.gpu_passes_skipped"), skipped as f64, "count");
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

/// gpui's AppKit view and its content height, for [`send_mouse_moved`].
/// `Window::dispatch_event` cannot be called from outside gpui (its return
/// type is private), so a move is built the way AppKit builds one and
/// handed to the view's own `mouseMoved:` -- gpui's `from_native`, the
/// hit test and every mouse listener, exactly as a real move runs them
/// (`kit/src/agentic.rs` drives drags the same way).
#[cfg(feature = "probe")]
#[derive(Clone, Copy)]
struct MouseTarget {
    ns_view: usize,
    content_height: f64,
}

#[cfg(feature = "probe")]
impl MouseTarget {
    #[cfg(target_os = "macos")]
    fn of(window: &Window) -> Option<Self> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return None };
        Some(Self { ns_view: appkit.ns_view.as_ptr() as usize, content_height: f64::from(window.viewport_size().height) })
    }

    #[cfg(not(target_os = "macos"))]
    fn of(_: &Window) -> Option<Self> {
        None
    }
}

/// One mouse move at `at` (gpui window coordinates), no button held.
#[cfg(all(feature = "probe", target_os = "macos"))]
fn send_mouse_moved(target: MouseTarget, at: gpui::Point<gpui::Pixels>) {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSView};
    use objc2_core_foundation::CGPoint;
    // SAFETY: gpui documents the view as valid for the window's lifetime,
    // and this runs on the main thread (a `cx.spawn` task).
    let ns_view: &NSView = unsafe { &*(target.ns_view as *const NSView) };
    let Some(ns_window) = ns_view.window() else { return };
    let point = CGPoint::new(f64::from(f32::from(at.x)), target.content_height - f64::from(f32::from(at.y)));
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        NSEventType::MouseMoved,
        point,
        NSEventModifierFlags::empty(),
        timestamp,
        ns_window.windowNumber(),
        None,
        0,
        0,
        0.0,
    );
    if let Some(event) = event {
        ns_view.mouseMoved(&event);
    }
}

#[cfg(all(feature = "probe", not(target_os = "macos")))]
fn send_mouse_moved(_: MouseTarget, _: gpui::Point<gpui::Pixels>) {}

#[cfg(not(feature = "probe"))]
fn hover_sweep_task(_: Entity<DemoRoot>, _: &mut App) {
    panic!("--hover-sweep needs --features probe");
}

/// `--live`'s update pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
enum LiveMode {
    /// The 1,000 block groups nearest a meridian that sweeps west to east.
    Wave,
    /// 1,000 block groups picked at random each tick.
    Random,
}

/// What `--live` keeps of the pushed data: each block group's id and a
/// representative longitude (its first vertex), and the geometry of the
/// 200 block groups it removes and re-adds.
// Read only by the probe-gated `live_task`.
#[cfg_attr(not(feature = "probe"), allow(dead_code))]
struct LiveData {
    geoids: Vec<f64>,
    /// Feature positions sorted by longitude: the wave walks this.
    by_lon: Vec<u32>,
    /// `(geoid, income, polygons -> rings -> coordinates)`.
    pool: Vec<(f64, Option<f64>, Vec<Vec<Vec<[f64; 2]>>>)>,
}

impl LiveData {
    fn of(data: &BlockGroups) -> Self {
        let lon = |bg: &Bg| {
            let ring = data.polygons[bg.polygons.0].0;
            data.coords[data.rings[ring].0][0]
        };
        let lons: Vec<f64> = data.features.iter().map(lon).collect();
        let mut by_lon: Vec<u32> = (0..data.features.len() as u32).collect();
        by_lon.sort_by(|&a, &b| lons[a as usize].total_cmp(&lons[b as usize]));
        let step = (data.features.len() / 200).max(1);
        let pool = data
            .features
            .iter()
            .step_by(step)
            .take(200)
            .map(|bg| {
                let polys = data.polygons[bg.polygons.0..bg.polygons.1]
                    .iter()
                    .map(|&(a, b)| data.rings[a..b].iter().map(|&(c, d)| data.coords[c..d].to_vec()).collect())
                    .collect();
                (bg.geoid, bg.income, polys)
            })
            .collect();
        Self { geoids: data.features.iter().map(|bg| bg.geoid).collect(), by_lon, pool }
    }
}

/// `--live [wave|random]` (needs `--features probe`): once the first
/// overlay frame is up and settled, ten times a second for `--live-secs`
/// (default 120): 1,000 block groups get a new income through
/// `MapView::update_source_features`, and every 20th tick 100 block groups
/// are removed and the 100 removed the time before are added back. Prints
/// the engine's own per-update measurement (`take_live_samples`: submit ->
/// a whole frame with no tile the update dirtied still showing its old
/// picture), tiles re-baked against cached, frame times, holes and RSS
/// drift, as `gate: live.*` lines too.
#[cfg(feature = "probe")]
fn live_task(map: Entity<MapView>, data: Arc<LiveData>, mode: LiveMode, secs: f64, camera: Option<(f64, f64, f64)>, cx: &mut App) {
    use gpui_maps::overlay::{AddFeature, FeatureUpdate, SourceDiff, TagValue};
    use gpui_maps::render::zoom_probe;
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + Duration::from_secs(300);
        while gpui_maps::render::alloc::overlay_tile_census().first_frame_nanos_last == 0 {
            if Instant::now() > deadline {
                println!("live: no overlay frame within 300 s; giving up");
                return;
            }
            cx.background_executor().timer(Duration::from_millis(100)).await;
        }
        if let Some((lon, lat, zoom)) = camera {
            map.update(cx, |view, cx| {
                view.set_camera(lon, lat, zoom);
                cx.notify();
            });
        }
        // Let the first cover finish baking, unrecorded.
        cx.background_executor().timer(Duration::from_secs(6)).await;
        cx.update(|cx| cx.activate(true));
        let (paints_before, draws_before) =
            (gpui_maps::render::paint_census().paints_submitted, allocation_stats().draw_calls_total);
        let occluded_before = gpui_maps::render::paint_census().paints_skipped_occluded;
        zoom_probe::start("live");
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (rng >> 33) as usize
        };
        let n = data.geoids.len();
        let mut removed_half: Option<usize> = None;
        let mut sync: Vec<f64> = Vec::new();
        let mut samples: Vec<gpui_maps::map_view::LiveSample> = Vec::new();
        let mut skipped = 0usize;
        let mut rss_10s: Option<u64> = None;
        let started = Instant::now();
        let mut tick = 0u64;
        loop {
            let t = started.elapsed().as_secs_f64();
            if t >= secs {
                break;
            }
            if rss_10s.is_none() && t >= 10.0 {
                rss_10s = Some(allocation_stats().rss_bytes);
            }
            let picks: Vec<usize> = match mode {
                LiveMode::Wave => {
                    // Once across the country every 20 s.
                    let centre = ((t / 20.0).fract() * n as f64) as usize;
                    (0..1000).map(|k| data.by_lon[(centre + k) % n] as usize).collect()
                }
                LiveMode::Random => (0..1000).map(|_| next() % n).collect(),
            };
            let update: Vec<FeatureUpdate<'_>> = picks
                .iter()
                .enumerate()
                .map(|(k, &at)| {
                    let phase = tick as f64 * 0.7 + k as f64 * 0.013;
                    let income = 2_500.0 + 247_500.0 * (0.5 + 0.5 * phase.sin());
                    FeatureUpdate {
                        id: FeatureId::Number(data.geoids[at]),
                        geometry: None,
                        properties: vec![("income".into(), Some(TagValue::Number(income.round())))],
                    }
                })
                .collect();
            let mut diff = SourceDiff { update, ..SourceDiff::default() };
            // Borrowed geometry for the re-adds, built per tick.
            let back = removed_half.filter(|_| tick % 20 == 19);
            let rings: Vec<Vec<Vec<&[[f64; 2]]>>> = match back {
                Some(h) => data.pool[h * 100..h * 100 + 100]
                    .iter()
                    .map(|(_, _, polys)| polys.iter().map(|p| p.iter().map(Vec::as_slice).collect()).collect())
                    .collect(),
                None => Vec::new(),
            };
            let polys: Vec<Vec<&[&[[f64; 2]]]>> =
                rings.iter().map(|f| f.iter().map(Vec::as_slice).collect()).collect();
            if tick % 20 == 19 {
                let half = removed_half.map_or(0, |h| 1 - h);
                diff.remove_ids = data.pool[half * 100..half * 100 + 100].iter().map(|p| FeatureId::Number(p.0)).collect();
                if let Some(h) = back {
                    diff.add = data.pool[h * 100..h * 100 + 100]
                        .iter()
                        .zip(&polys)
                        .map(|((geoid, income, _), polys)| AddFeature {
                            feature: FeatureInput {
                                id: Some(FeatureId::Number(*geoid)),
                                geometry: GeometryInput::Polygon(polys),
                                paint: FeaturePaint::default(),
                            },
                            properties: income.map(|i| ("income".to_string(), TagValue::Number(i))).into_iter().collect(),
                        })
                        .collect();
                }
                removed_half = Some(half);
            }
            let t0 = Instant::now();
            let result = map.update(cx, |view, cx| {
                let r = view.update_source_features(FILL_SOURCE, diff, cx).expect("the live diff is valid");
                (r, view.take_live_samples(FILL_SOURCE))
            });
            sync.push(t0.elapsed().as_secs_f64() * 1e3);
            skipped += result.0.skipped;
            samples.extend(result.1);
            tick += 1;
            let due = started + Duration::from_millis(100 * tick);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                cx.background_executor().timer(wait).await;
            }
        }
        let live_s = started.elapsed().as_secs_f64();
        // The last updates' frames.
        let drain = Instant::now();
        while drain.elapsed() < Duration::from_secs(3) {
            let (backlog, more) =
                map.update(cx, |view, _| (view.live_backlog(FILL_SOURCE), view.take_live_samples(FILL_SOURCE)));
            samples.extend(more);
            if backlog == 0 {
                break;
            }
            cx.background_executor().timer(Duration::from_millis(16)).await;
        }
        zoom_probe::stop();
        let rss_end = allocation_stats().rss_bytes;
        let paints = gpui_maps::render::paint_census().paints_submitted - paints_before;
        let draws = allocation_stats().draw_calls_total - draws_before;
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let lat: Vec<f64> = samples.iter().map(|s| ms(s.latency)).collect();
        // Jobs that dirtied at least one cached tile: the ones a re-bake stands between.
        let lat_dirty: Vec<f64> = samples.iter().filter(|s| s.tiles_rebaked > 0).map(|s| ms(s.latency)).collect();
        // Jobs that re-baked nothing: class-only changes, repainted in place.
        let lat_class: Vec<f64> = samples.iter().filter(|s| s.tiles_rebaked == 0).map(|s| ms(s.latency)).collect();
        let repainted: Vec<f64> = samples.iter().map(|s| s.tiles_repainted as f64).collect();
        let lat_new: Vec<f64> = samples.iter().map(|s| ms(s.latency_newest)).collect();
        let apply: Vec<f64> = samples.iter().map(|s| ms(s.apply)).collect();
        let land: Vec<f64> = samples.iter().map(|s| ms(s.land_to_frame)).collect();
        let rebaked: Vec<f64> = samples.iter().map(|s| s.tiles_rebaked as f64).collect();
        let cached: Vec<f64> = samples.iter().map(|s| s.tiles_cached as f64).collect();
        let frac: Vec<f64> = samples.iter().map(|s| s.tiles_rebaked as f64 / s.tiles_cached.max(1) as f64).collect();
        let merged: usize = samples.iter().map(|s| s.diffs).sum();
        let rss0 = rss_10s.unwrap_or(rss_end);
        println!("-- live ({mode:?}, {live_s:.0} s) --");
        println!(
            "updates: {tick} submitted ({skipped} ids skipped), {} jobs landed whole ({merged} diffs; {:.2} diffs/job) | update call (sync) p50 {:.2} ms max {:.2} ms",
            samples.len(),
            merged as f64 / samples.len().max(1) as f64,
            quantile(&sync, 0.5),
            quantile(&sync, 1.0),
        );
        println!(
            "submit -> whole frame: p50 {:.1} ms p99 {:.1} ms max {:.1} ms (newest diff of each job: p50 {:.1} ms) | apply (worker) p50 {:.1} ms max {:.1} ms | landed -> whole frame p50 {:.1} ms max {:.1} ms",
            quantile(&lat, 0.5),
            quantile(&lat, 0.99),
            quantile(&lat, 1.0),
            quantile(&lat_new, 0.5),
            quantile(&apply, 0.5),
            quantile(&apply, 1.0),
            quantile(&land, 0.5),
            quantile(&land, 1.0),
        );
        println!(
            "jobs that dirtied a tile: {} -- submit -> whole frame p50 {:.1} ms p99 {:.1} ms max {:.1} ms",
            lat_dirty.len(),
            quantile(&lat_dirty, 0.5),
            quantile(&lat_dirty, 0.99),
            quantile(&lat_dirty, 1.0),
        );
        println!(
            "class-only jobs (repainted in place, nothing re-baked): {} -- submit -> whole frame p50 {:.1} ms p99 {:.1} ms max {:.1} ms | tiles repainted per job p50 {:.0} max {:.0}",
            lat_class.len(),
            quantile(&lat_class, 0.5),
            quantile(&lat_class, 0.99),
            quantile(&lat_class, 1.0),
            quantile(&repainted, 0.5),
            quantile(&repainted, 1.0),
        );
        println!(
            "tiles re-baked per job: p50 {:.0} max {:.0} of {:.0} cached (p50 fraction {:.2}, max {:.2})",
            quantile(&rebaked, 0.5),
            quantile(&rebaked, 1.0),
            quantile(&cached, 0.5),
            quantile(&frac, 0.5),
            quantile(&frac, 1.0),
        );
        println!(
            "rss at 10 s {} -> end {} (drift {:+.1} MB) | {:.1} fps ({paints} paints, {draws} draw calls)",
            gb(rss0),
            gb(rss_end),
            (rss_end as f64 - rss0 as f64) / 1e6,
            paints as f64 / live_s,
        );
        println!("{}", zoom_probe::report());
        // Where the frames over 16.7 ms went: phases and off-paint work, summed.
        {
            let (_, frames) = zoom_probe::snapshot();
            let slow: Vec<_> = frames.iter().filter(|f| f.cpu_ms > 16.7).collect();
            let mut by: HashMap<&str, f64> = HashMap::new();
            for f in &slow {
                for (name, ms) in f.phase_ms.iter().chain(&f.extra_ms) {
                    *by.entry(name).or_default() += ms;
                }
                *by.entry("overlay_upload").or_default() += f.overlay_upload_ms;
            }
            let mut by: Vec<_> = by.into_iter().collect();
            by.sort_by(|a, b| b.1.total_cmp(&a.1));
            println!(
                "frames over 16.7 ms: {} -- summed ms by phase: {}",
                slow.len(),
                by.iter().take(8).map(|(n, ms)| format!("{n} {ms:.0}")).collect::<Vec<_>>().join(", ")
            );
        }
        gate("live.occluded_paints", (gpui_maps::render::paint_census().paints_skipped_occluded - occluded_before) as f64, "paints");
        gate("live.draw_calls", draws as f64, "draws");
        gate("live.updates", tick as f64, "count");
        gate("live.latency_p50_ms", quantile(&lat, 0.5), "ms");
        gate("live.latency_p99_ms", quantile(&lat, 0.99), "ms");
        gate("live.latency_max_ms", quantile(&lat, 1.0), "ms");
        gate("live.class_latency_p50_ms", quantile(&lat_class, 0.5), "ms");
        gate("live.class_latency_p99_ms", quantile(&lat_class, 0.99), "ms");
        gate("live.dirty_latency_p50_ms", quantile(&lat_dirty, 0.5), "ms");
        gate("live.dirty_latency_p99_ms", quantile(&lat_dirty, 0.99), "ms");
        gate("live.apply_p50_ms", quantile(&apply, 0.5), "ms");
        gate("live.update_call_max_ms", quantile(&sync, 1.0), "ms");
        gate("live.rebaked_tiles_p50", quantile(&rebaked, 0.5), "tiles");
        gate("live.rebaked_fraction_p50", quantile(&frac, 0.5), "ratio");
        gate("live.rss_start_mb", rss0 as f64 / 1e6, "MB");
        gate("live.rss_end_mb", rss_end as f64 / 1e6, "MB");
        gate("live.frame_interval_ms", live_s * 1e3 / paints.max(1) as f64, "ms");
        gate_frames("live");
        cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(not(feature = "probe"))]
fn live_task(_: Entity<MapView>, _: Arc<LiveData>, _: LiveMode, _: f64, _: Option<(f64, f64, f64)>, _: &mut App) {
    panic!("--live needs --features probe");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gate run from a binary without the probe rows refuses instead of
    /// passing with them MISSING; a gate run from a probe build, or any
    /// run without `US_BG_GATE`, goes ahead.
    #[test]
    fn a_gate_run_without_the_probe_rows_refuses() {
        assert_eq!(gate_refusal(false), None);
        assert_eq!(gate_refusal(true).is_some(), !cfg!(feature = "probe"));
        if let Some(refusal) = gate_refusal(true) {
            assert!(refusal.starts_with("GATE ERROR") && refusal.contains("--features probe"));
        }
    }

    /// `--pan --quit-after` reports through `kit::PanGate` (gallery Task
    /// 12): the same `pan.*` rows, in the same order, as the inline block
    /// it replaced (`windowed` at c08eb3e), the GPU pass row only when a
    /// sample was taken. The perf gate grades these names.
    #[test]
    fn the_pan_gate_rows_are_the_inline_ones() {
        let old_inline = [
            "pan.window_s",
            "pan.occluded_paints",
            "pan.draw_calls",
            "pan.fps",
            "pan.frame_interval_ms",
            "pan.gpu_pass_mean_ms",
            "pan.rss_mb",
        ];
        let units = ["s", "paints", "draws", "fps", "ms", "ms", "MB"];
        let gate = kit::PanGate::open();
        let census = gpui_maps::render::paint_census();
        let mut now = allocation_stats();
        for sampled in [true, false] {
            // The window's samples: one more than at open, or none.
            now.gpu_pass_samples = gpui_maps::render::allocation_stats().gpu_pass_samples + u64::from(sampled);
            let rows = gate.rows("pan", 5.0, census, now);
            let names: Vec<&str> = rows.iter().map(|(n, _, _)| n.as_str()).collect();
            let want: Vec<&str> = old_inline.iter().copied().filter(|n| sampled || *n != "pan.gpu_pass_mean_ms").collect();
            assert_eq!(names, want, "sampled: {sampled}");
            let got_units: Vec<&str> = rows.iter().map(|(_, _, u)| *u).collect();
            let want_units: Vec<&str> =
                old_inline.iter().zip(units).filter(|(n, _)| sampled || **n != "pan.gpu_pass_mean_ms").map(|(_, u)| u).collect();
            assert_eq!(got_units, want_units);
        }
    }
}
