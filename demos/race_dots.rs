//! All 331,449,281 people of the 2020 U.S. Census as dots, coloured by Kyle
//! Walker's 8 race and ethnicity groups, over Mapbox Light: the race map
//! panel ([`panel::RaceMapPanel`]) in a window, plus MG-R11 / MG-R12 /
//! MG-R13 / MG-R14's measurement flags. Dots, H3 hexagons and donut
//! clusters.
//!
//! See `docs/superpowers/specs/2026-09-27-us-race-dot-map-design.md`.
//!
//! ```text
//! cargo run --release [--features probe] --example race_dots -- [--dots FILE]
//!     [--at us|manhattan|la|houston|dc] [--view LON,LAT,ZOOM] [--basemap light|positron|none] [--no-basemap]
//!     [--measure] [--pan] [--zoom-sweep] [--quit-after SECS] [--budget N]
//!     [--capture OUTDIR] [--size WxH] [--hide G,G,..]
//!     [--mode dots|hexagons|clusters] [--hexes FILE | --hexes-clickhouse URL | --no-hexes]
//!     [--clusters FILE | --no-clusters] [--state AB]
//! ```
//!
//! - `--dots` defaults to `~/.cache/gpui-map-data/census2020/race-dots-2020.pmtiles`
//!   (`tools/race-build`).
//! - Basemap: Mapbox Light when `MAPBOX_ACCESS_TOKEN` is set, else the
//!   keyless Positron; `--basemap` picks, `--no-basemap` is `--basemap none`.
//! - Windowed (default): the legend (click a group to hide it), the view
//!   switcher and status line, the HUD (`people in view · dots · 1 dot = P
//!   people · level`, "parts" at the part tiles), the About panel. The time
//!   to the first drawn dot is printed (`first dots: N ms`), and a status
//!   line every 5 s with `draw_calls_total` (0 = nothing painted).
//! - `--pan` / `--zoom-sweep` (need `--features probe`): MG-R11's frame
//!   times, as `world_dots`. The pan circles the `--at` camera about 1.5
//!   tiles wide; the sweep runs z3 -> z14 -> z3 slow and flicked, hovers the
//!   parts/cells crossover (the archive's, +- 0.03, 4 s) counting level switches, and prints tile rebuilds by leg
//!   (F8), and prints the zoom at which the ppd first reads 1.00.
//! - `--measure`: no window. Settles each preset headless at 1920x1080 and
//!   prints its stats, then steps a Manhattan camera z11 -> z14 and prints
//!   the first zoom whose ppd reads 1.00 (the headless figure; the
//!   window's is the sweep's).
//! - `--capture OUTDIR`: headless PNGs of us z4, houston z10, manhattan
//!   z13 and z14 (or the `--at` preset alone when given) over the chosen
//!   basemap, at `--size` (default 1920x1080).
//! - `--view LON,LAT,ZOOM`: any camera in place of an `--at` preset.
//! - `--hide G,G,..`: start with these groups (0-7, the legend's order:
//!   Hispanic, White, Black, AIAN, Asian, NHPI, other, two or more) toggled
//!   off, in the window and in `--capture`.
//! - `--quit-after SECS`, `--budget N`: as `world_dots`.
//! - `--mode hexagons`: start in hexagon mode (MG-R12). `--hexes` defaults
//!   to `~/.cache/gpui-map-data/census2020/race-hex-2020.pmtiles`
//!   (`race-build export-hex`); `--hexes-clickhouse URL` reads
//!   `race_hex_2020` live (`--features "clickhouse-demo h3"`). In hexagon
//!   mode `--capture` draws the hexagons (and the dots fading in under them
//!   from z12) at us z5, houston z8 and z11 and manhattan z12.5, and
//!   `--zoom-sweep` runs z3 -> z12.75 (Walker's last hexagon zoom) with no
//!   crossover hover.
//! - `--mode clusters`: start in cluster mode (MG-R13). `--clusters`
//!   defaults to `~/.cache/gpui-map-data/census2020/race-clusters-2020.pmtiles`
//!   (`race-build export-clusters`). In cluster mode `--capture` draws the
//!   donuts (through the capture's donut hook) at us z4, houston z8 and
//!   z11.8 and manhattan z12.2, and the dots from z12; `--zoom-sweep` runs
//!   z3 -> z13 (the handover to the dots and past it) with no crossover
//!   hover. The capture draws the count labels too (the capture's
//!   `overlay_labels`, anchored as the window anchors them).
//! - `--mode states`: start in States mode (a pie per state over the
//!   state outlines, the states chart). `--capture` draws the pies and
//!   outlines at us z4 and the north-east z6, and with `--state AB` (a
//!   postal code) that state selected, framed as its click frames it.

// The panel and its data modules (`demos/race_dots/*.rs`), through the
// gallery panel that includes them: ONE copy, shared with the gallery's
// `race-map` panel.
use swat_maps_kit::panels::race_dots as race_dots_panel;
use race_dots_panel::{about, clusters, hexes, panel, states, walker};

// The demo keys every example window has: `t` (day/night terminator),
// `g` (globe or flat), `s` (imagery; none configured here).
use swat_maps_kit::shell;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{px, size, App, AppContext, Bounds, Entity, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_maps::capture::{capture, fetch_cover, CaptureDots, CaptureRequest, Clear, TileStore};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::dotgrid::layer::{DotLayers, DotStats, DotStyle, RACE_PALETTE};
use gpui_maps::overlay::dotgrid::{fetch_native, GridCountSource, GridTileKey};
use gpui_maps::render::allocation_stats;
use gpui_maps::style::Rgba;

use hexes::HexSourceSpec;
use panel::{Basemap, Mode, PanelOptions, RaceMapPanel, RaceSources, LAYER};

#[derive(Clone, Copy, Debug)]
struct Preset {
    name: &'static str,
    lon: f64,
    lat: f64,
    zoom: f64,
}

fn preset(at: &str) -> Preset {
    match at {
        "us" => Preset { name: "us_z3.5", lon: walker::START.0, lat: walker::START.1, zoom: walker::START.2 },
        "manhattan" => Preset { name: "manhattan_z13", lon: -73.9712, lat: 40.7831, zoom: 13.0 },
        "la" => Preset { name: "la_z10", lon: -118.2437, lat: 34.0522, zoom: 10.0 },
        "houston" => Preset { name: "houston_z10", lon: -95.3698, lat: 29.7604, zoom: 10.0 },
        "dc" => Preset { name: "dc_z12", lon: -77.0365, lat: 38.8977, zoom: 12.0 },
        other => panic!("--at takes us|manhattan|la|houston|dc, got {other}"),
    }
}

/// Step 5's frames: the whole US, Houston at the crossover's far side,
/// Manhattan at one dot per person and at Walker's max zoom.
const CAPTURES: [Preset; 4] = [
    Preset { name: "us_z4", lon: walker::START.0, lat: walker::START.1, zoom: 4.0 },
    Preset { name: "houston_z10", lon: -95.3698, lat: 29.7604, zoom: 10.0 },
    Preset { name: "manhattan_z13", lon: -73.9712, lat: 40.7831, zoom: 13.0 },
    Preset { name: "manhattan_z14", lon: -73.9712, lat: 40.7831, zoom: 14.0 },
];

/// Hexagon mode's frames (T11 step 4): r4/r5 cross-fading over the whole
/// US, r6 on its plateau around Houston, r7/r8 cross-fading, and the r8
/// hexagons fading out over the dots fading in.
const CAPTURES_HEX: [Preset; 4] = [
    Preset { name: "hex_us_z5", lon: walker::START.0, lat: walker::START.1, zoom: 5.0 },
    Preset { name: "hex_houston_z8", lon: -95.3698, lat: 29.7604, zoom: 8.0 },
    Preset { name: "hex_houston_z11", lon: -95.3698, lat: 29.7604, zoom: 11.0 },
    Preset { name: "hex_manhattan_z12.5", lon: -73.9712, lat: 40.7831, zoom: 12.5 },
];

/// Cluster mode's frames (T14 step 4): the whole US, one metro's clusters,
/// the donuts at 0.7 just before the dots take over, and the handover band
/// with both drawn.
const CAPTURES_CLUSTERS: [Preset; 4] = [
    Preset { name: "clusters_us_z4", lon: walker::START.0, lat: walker::START.1, zoom: 4.0 },
    Preset { name: "clusters_houston_z8", lon: -95.3698, lat: 29.7604, zoom: 8.0 },
    Preset { name: "clusters_houston_z11.8", lon: -95.3698, lat: 29.7604, zoom: 11.8 },
    Preset { name: "clusters_manhattan_z12.2", lon: -73.9712, lat: 40.7831, zoom: 12.2 },
];

struct Args {
    dots: PathBuf,
    at: Preset,
    at_given: bool,
    basemap: Basemap,
    measure: bool,
    pan: bool,
    zoom_sweep: bool,
    quit_after: Option<f64>,
    budget: Option<u32>,
    capture: Option<PathBuf>,
    size: Option<(u32, u32)>,
    /// `--hide G,G,..`: groups (0-7, `walker::GROUPS` order) toggled off.
    legend: panel::Legend,
    /// `--hexes` / `--hexes-clickhouse` / `--no-hexes`.
    hexes: Option<HexSourceSpec>,
    /// `--mode`.
    mode: Mode,
    /// `--clusters` / `--no-clusters`.
    clusters: Option<PathBuf>,
    /// `--state AB`: States mode's selection (an index into `states::rows`).
    state: Option<usize>,
}

fn parse_args() -> Args {
    let mut a = Args {
        dots: RaceSources::default_dots(),
        at: preset("us"),
        at_given: false,
        basemap: Basemap::from_env(),
        measure: false,
        pan: false,
        zoom_sweep: false,
        quit_after: None,
        budget: None,
        capture: None,
        size: None,
        legend: panel::Legend::default(),
        hexes: Some(HexSourceSpec::Pmtiles(HexSourceSpec::default_pmtiles())),
        mode: Mode::Dots,
        clusters: Some(clusters::default_pmtiles()),
        state: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dots" => a.dots = PathBuf::from(it.next().expect("--dots takes a file")),
            "--at" => {
                a.at = preset(&it.next().expect("--at takes a preset"));
                a.at_given = true;
            }
            "--basemap" => {
                a.basemap = match it.next().as_deref() {
                    Some("light" | "mapbox") => Basemap::MapboxLight,
                    Some("positron") => Basemap::Positron,
                    Some("none") => Basemap::None,
                    other => panic!("--basemap takes light|positron|none, got {other:?}"),
                }
            }
            "--no-basemap" => a.basemap = Basemap::None,
            "--hide" => {
                for g in it.next().expect("--hide takes group numbers 0-7").split(',') {
                    let g: usize = g.parse().expect("--hide takes group numbers 0-7");
                    assert!(g < 8, "--hide takes group numbers 0-7");
                    a.legend.toggle(g);
                }
            }
            "--view" => {
                let v = it.next().expect("--view takes LON,LAT,ZOOM");
                let n: Vec<f64> = v.split(',').map(|x| x.parse().expect("--view takes LON,LAT,ZOOM")).collect();
                assert_eq!(n.len(), 3, "--view takes LON,LAT,ZOOM");
                // One name per run: leaked once, for the capture's file name.
                let name: &'static str = Box::leak(format!("view_{}_{}_z{}", n[0], n[1], n[2]).into_boxed_str());
                a.at = Preset { name, lon: n[0], lat: n[1], zoom: n[2] };
                a.at_given = true;
            }
            "--hexes" => a.hexes = Some(HexSourceSpec::Pmtiles(PathBuf::from(it.next().expect("--hexes takes a file")))),
            "--hexes-clickhouse" => a.hexes = Some(HexSourceSpec::ClickHouse(it.next().expect("--hexes-clickhouse takes a URL"))),
            "--no-hexes" => a.hexes = None,
            "--clusters" => a.clusters = Some(PathBuf::from(it.next().expect("--clusters takes a file"))),
            "--no-clusters" => a.clusters = None,
            "--mode" => {
                a.mode = match it.next().as_deref() {
                    Some("dots") => Mode::Dots,
                    Some("hexagons" | "hex") => Mode::Hexagons,
                    Some("clusters" | "donuts") => Mode::Clusters,
                    Some("states") => Mode::States,
                    other => panic!("--mode takes dots|hexagons|clusters|states, got {other:?}"),
                }
            }
            "--state" => {
                let ab = it.next().expect("--state takes a postal code").to_uppercase();
                a.state = Some(states::rows().iter().position(|r| r.abbr == ab).unwrap_or_else(|| panic!("--state: no state {ab}")));
            }
            "--measure" => a.measure = true,
            "--pan" => a.pan = true,
            "--zoom-sweep" => a.zoom_sweep = true,
            "--quit-after" => a.quit_after = Some(it.next().and_then(|s| s.parse().ok()).expect("--quit-after takes seconds")),
            "--budget" => a.budget = Some(it.next().and_then(|s| s.parse().ok()).expect("--budget takes a dot count")),
            "--capture" => a.capture = Some(PathBuf::from(it.next().expect("--capture takes a dir"))),
            "--size" => {
                let v = it.next().expect("--size takes WxH");
                let (w, h) = v.split_once('x').expect("--size takes WxH");
                a.size = Some((w.parse().expect("a width"), h.parse().expect("a height")));
            }
            other => panic!("unknown flag {other}"),
        }
    }
    a
}

fn main() {
    let started = Instant::now();
    gpui_maps::render::note_process_start();
    let args = parse_args();
    if args.measure {
        measure(&args);
    } else if let Some(dir) = &args.capture {
        let views: Vec<Preset> = match (args.at_given, args.mode) {
            (true, _) => vec![args.at],
            (false, Mode::Hexagons) => CAPTURES_HEX.to_vec(),
            (false, Mode::Clusters) => CAPTURES_CLUSTERS.to_vec(),
            (false, Mode::Dots) => CAPTURES.to_vec(),
            (false, Mode::States) => states_captures(&args),
        };
        capture_views(&args, dir, &views);
    } else {
        windowed(args, started);
    }
}

/// One `gate:` line in `examples/perf_gate.rs`' format, when `US_BG_GATE`
/// is set.
fn gate(name: &str, value: f64, unit: &str) {
    if std::env::var_os("US_BG_GATE").is_some() {
        println!("gate: {name} {value} {unit}");
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

fn required_source(args: &Args) -> Arc<dyn GridCountSource> {
    panel::open_dots(&args.dots).unwrap_or_else(|why| {
        eprintln!("race_dots: {why}");
        std::process::exit(2);
    })
}

// ---------------------------------------------------------------- headless

fn request_at(p: Preset, size: Option<(u32, u32)>) -> CaptureRequest {
    let (width, height) = size.unwrap_or((1920, 1080));
    CaptureRequest { center_lon: p.lon, center_lat: p.lat, zoom: p.zoom, width, height, flat: true, ..CaptureRequest::default() }
}

/// A `DotLayers` for one camera, run until nothing is left to fetch or
/// rebuild (the clock stepped 100 ms a frame so the ppd controller settles
/// on the loaded view), as `world_dots`' `settle`.
fn settle(source: &Arc<dyn GridCountSource>, style: &DotStyle, request: &CaptureRequest) -> DotLayers {
    let mut layers = DotLayers::new();
    layers.set_source(LAYER, Arc::clone(source), style.clone());
    let camera = request.camera();
    let globe = gpui_maps::render::globe::GlobeCover::FLAT;
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
        if quiet >= 10 {
            break;
        }
    }
    layers
}

/// The 8 group colours at the style's opacity: what the window's paint
/// table holds (C16 scales alpha on the CPU).
fn race_ramp(style: &DotStyle) -> Vec<Rgba> {
    RACE_PALETTE.iter().map(|c| Rgba { a: c.a * style.opacity, ..*c }).collect()
}

/// The settled `layers`' frame over `basemap` (or none, on Walker's pale
/// ground), and the layer's stats.
fn race_frame(
    layers: Option<&DotLayers>,
    source: &Arc<dyn GridCountSource>,
    style: &DotStyle,
    request: &CaptureRequest,
    basemap: Option<(&TileStore, &gpui_maps::style::Style)>,
) -> (gpui_maps::capture::Capture, Option<DotStats>) {
    let mut request = request.clone();
    let stats = layers.map(|layers| race_dots_on(layers, source, style, &mut request));
    let frame = match basemap {
        Some((tiles, basemap_style)) => capture(&request, tiles, basemap_style),
        None => {
            // Walker's ground, rgb(242,243,240).
            request.clear = Clear::Sentinel(Rgba { r: 242.0 / 255.0, g: 243.0 / 255.0, b: 240.0 / 255.0, a: 1.0 });
            request.backdrop = false;
            let empty = gpui_maps::style::Style { layers: Vec::new(), ..gpui_maps::style::Style::default() };
            capture(&request, &TileStore::new(), &empty)
        }
    }
    .expect("a headless device");
    (frame, stats)
}

/// Puts the settled `layers`' dots on `request`; the layer's stats.
fn race_dots_on(layers: &DotLayers, source: &Arc<dyn GridCountSource>, style: &DotStyle, request: &mut CaptureRequest) -> DotStats {
    let stats = layers.stats(LAYER).expect("the layer");
    let plan = layers.plan(LAYER).expect("a plan").clone();
    request.dots = Some(CaptureDots {
        descriptor: source.descriptor().clone(),
        tiles: plan.draw.iter().map(|x| (x.key, (*layers.tile(LAYER, &x.key).expect("settled")).clone())).collect(),
        ppd: stats.ppd,
        ppd_build: None,
        radius_px: style.radius_px(request.zoom),
        radius_shrink: style.density_shrink,
        overlap_gamma: style.overlap_gamma,
        ramp: race_ramp(style),
        epoch_mix: 0.0,
        difference: false,
        exclude: None,
        tile_excludes: plan.draw.iter().map(|x| x.exclude).collect(),
        relocate: style.relocate,
        mask_k: style.mask_k,
        synthetic_mask: None,
        mask_previous: None,
        category_mask: Some(u32::from(style.category_mask)),
    });
    stats
}

/// Hexagon mode's overlay for a headless `request`: every resolution
/// visible at its zoom, the flat cover's tiles at its tile zoom fetched and
/// decoded (unclipped), coarsest first. Returns the hexagons drawn per
/// resolution.
fn hex_overlay_on(source: &dyn gpui_maps::overlay::hexes::HexSource, legend: panel::Legend, request: &mut CaptureRequest) -> Vec<(u8, f32, usize)> {
    let camera = request.camera();
    let mut layers: Vec<(f32, Vec<gpui_maps::overlay::hexes::HexRecord>)> = Vec::new();
    let mut counts = Vec::new();
    // Composited as the window draws them, every resolution loaded.
    let stack = hexes::stack_opacities(request.zoom, |_| 1.0);
    for (i, res) in hexes::RESOLUTIONS.into_iter().enumerate() {
        let opacity = stack[i];
        let Some(tz) = gpui_maps::overlay::hexes::hex_tile_zoom(res) else { continue };
        if opacity <= 0.0 {
            continue;
        }
        let mut tiles: Vec<_> = gpui_maps::render::cover_plan::flat_cover(&camera, tz).tiles.into_iter().filter(|t| t.z == tz).collect();
        tiles.truncate(hexes::MAX_TILES_PER_RES);
        let mut records = Vec::new();
        for tile in tiles {
            let fetch = hexes::Fetch { res, tile, request: source.request(res, tile) };
            match hexes::run_fetch(source, &fetch, fetch_native::execute) {
                Ok(r) => records.extend(r),
                Err(e) => eprintln!("race_dots: hex tile {}", e.why),
            }
        }
        counts.push((res, opacity, records.len()));
        layers.push((opacity, records));
    }
    let refs: Vec<(f32, &[gpui_maps::overlay::hexes::HexRecord])> = layers.iter().map(|(o, r)| (*o, r.as_slice())).collect();
    let (geojson, paint) = hexes::capture_overlay(&refs, legend.mask());
    request.overlay = Some(geojson);
    request.overlay_paint = Some(paint);
    // Tiled, as the window's hex sources are (`tile_source_for_paint_updates`):
    // the tiled path is the one that draws `fill-outline-color`.
    request.overlay_tiling = true;
    counts
}

/// Cluster mode's donuts for a headless `request`: the cluster tiles of
/// the flat cover at the view's cluster zoom, fetched and decoded (corrupt
/// records refused), as the panel's donuts at the view's fade (the same
/// cap and truncation). Returns the HUD figures.
fn cluster_donuts_on(archive: &clusters::ClusterArchive, legend: panel::Legend, request: &mut CaptureRequest) -> panel::ClusterHud {
    let camera = request.camera();
    let tz = clusters::tile_zoom(request.zoom);
    let alpha = clusters::quantise_alpha(walker::cluster_alpha(request.zoom));
    if alpha <= 0.0 {
        return panel::ClusterHud { tile_zoom: tz, ..panel::ClusterHud::default() };
    }
    // The panel's cover (`drive_clusters`): the globe's own below its
    // band, so the capture fetches what the window fetches.
    let globe = gpui_maps::render::globe::GlobeCover::new(&gpui_maps::render::globe::globe_frame(&camera));
    let mut tiles: Vec<_> =
        gpui_maps::render::cover_plan::tile_cover(tz, &globe, || gpui_maps::render::cover_plan::flat_cover(&camera, tz).tiles)
            .into_iter()
            .filter(|t| t.z == tz)
            .collect();
    let mut seen = std::collections::HashSet::new();
    tiles.retain(|t| seen.insert(*t));
    tiles.truncate(clusters::MAX_TILES);
    let (mut records, mut refused, mut errors) = (Vec::new(), 0, 0);
    for tile in tiles {
        let fetch = clusters::Fetch { tile, request: archive.request(tile) };
        match clusters::run_fetch(&fetch, fetch_native::execute) {
            Ok(d) => {
                refused += d.refused;
                records.extend(d.records);
            }
            Err(e) => {
                errors += 1;
                eprintln!("race_dots: cluster tile {}", e.why);
            }
        }
    }
    let d = clusters::drawable(&records, legend.mask(), alpha);
    let hud = panel::ClusterHud { donuts: d.donuts.len(), truncated: d.truncated, tile_zoom: tz, loading: false, errors, refused: refused as u64 };
    // The count labels, the panel's own source document.
    request.overlay_labels = Some(clusters::labels_geojson(&d.records, legend.mask(), alpha));
    request.donuts = vec![(d.donuts, RACE_PALETTE)];
    hud
}

/// The dots the settled `layers` draw at `ppd`: every drawn tile's whole
/// population, on screen or not (what MG-8 costs; headless has no GPU sync
/// to count `dots_drawn`).
fn drawn_dots(layers: &DotLayers, ppd: f32) -> f64 {
    let plan = layers.plan(LAYER).expect("a plan");
    plan.draw.iter().filter_map(|x| layers.tile(LAYER, &x.key)).map(|t| t.total).sum::<f64>() / f64::from(ppd.max(1.0))
}

/// The HUD's words for a headless line.
fn stats_line(s: &DotStats) -> String {
    format!(
        "{} people in view · {} dots · ppd {:.2}{} · {} · tiles {}/{} ({} stand-ins) · dot gpu {:.1} MB · rebuilds {} · errors {}",
        panel::people_text(s.people_in_view),
        panel::thousands(s.dots_in_view),
        s.ppd,
        // Filled by the GPU sync: 0 headless.
        if s.pop_drawn > 0.0 { format!(" (drawn tiles hold {})", panel::people_text(s.pop_drawn)) } else { String::new() },
        panel::level_text(s),
        s.tiles_ready,
        s.tiles_wanted,
        s.standin_tiles,
        s.gpu_bytes as f64 / 1e6,
        s.rebuilds_total,
        s.source_errors
    )
}

/// States mode's frames: the whole US, the crowded north-east, and with
/// `--state` that state framed as a click on it frames it (1920x1080).
const CAPTURES_STATES: [Preset; 2] = [
    Preset { name: "states_us_z4", lon: walker::START.0, lat: walker::START.1, zoom: 4.0 },
    Preset { name: "states_northeast_z6", lon: -73.8, lat: 41.4, zoom: 6.0 },
];

fn states_captures(args: &Args) -> Vec<Preset> {
    let mut v = CAPTURES_STATES.to_vec();
    if let Some(i) = args.state {
        let (w, h) = args.size.unwrap_or((1920, 1080));
        let (lon, lat, zoom) = states::fit(i, f64::from(w), f64::from(h));
        let name: &'static str = Box::leak(format!("states_{}_selected", states::rows()[i].abbr.to_lowercase()).into_boxed_str());
        v.push(Preset { name, lon, lat, zoom });
    }
    v
}

/// States mode for a headless `request`: the outlines (the selected
/// state's darker) and the pies, as the panel pushes them.
fn states_on(legend: panel::Legend, selected: Option<usize>, request: &mut CaptureRequest) -> usize {
    let fips = selected.map(|i| states::rows()[i].fips);
    request.overlay = Some(states::outlines_geojson(fips));
    let pies = states::pies(legend.mask(), states::quantised_scale(request.zoom), selected);
    let n = pies.donuts.len();
    request.donuts = vec![(pies.donuts, RACE_PALETTE)];
    n
}

/// `--capture`: each view headless to `dir/<name>.png`.
fn capture_views(args: &Args, dir: &Path, views: &[Preset]) {
    std::fs::create_dir_all(dir).expect("create the capture dir");
    let source = required_source(args);
    let mut style = walker::dot_style(args.budget);
    style.category_mask = args.legend.mask();
    let spec = (args.basemap != Basemap::None).then(|| panel::basemap(args.basemap));
    println!("basemap: {}", args.basemap.name());
    let hex_source = (args.mode == Mode::Hexagons).then(|| {
        let spec = args.hexes.as_ref().unwrap_or_else(|| {
            eprintln!("race_dots: --mode hexagons needs a hex source");
            std::process::exit(2);
        });
        hexes::open_hexes(spec).unwrap_or_else(|why| {
            eprintln!("race_dots: {why}");
            std::process::exit(2);
        })
    });
    let cluster_archive = (args.mode == Mode::Clusters).then(|| {
        let path = args.clusters.as_ref().unwrap_or_else(|| {
            eprintln!("race_dots: --mode clusters needs a cluster archive");
            std::process::exit(2);
        });
        clusters::open_clusters(path).unwrap_or_else(|why| {
            eprintln!("race_dots: {why}");
            std::process::exit(2);
        })
    });
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let cache = std::env::temp_dir().join(format!("gpui-map-capture-race-{:?}", args.basemap).to_lowercase());
    for &p in views {
        let mut request = request_at(p, args.size);
        request.flat_tile_zoom_max = 16;
        let t = Instant::now();
        // Hexagon mode: the hexagons, and the dots only where they fade in.
        let mut style = style.clone();
        let mut hex_line = String::new();
        if let Some(hex) = &hex_source {
            style.opacity = walker::dot_opacity_in_hex_mode(p.zoom);
            let counts = hex_overlay_on(hex.as_ref(), args.legend, &mut request);
            hex_line = format!(" · {}", panel::hex_hud_text(&counts, false, 0, None));
        }
        // Cluster mode: the donuts, and the dots only from z12, fading in.
        if let Some(archive) = &cluster_archive {
            style.opacity = if p.zoom >= walker::dots_min_zoom(Mode::Clusters) { walker::dot_opacity_in_cluster_mode(p.zoom) } else { 0.0 };
            let hud = cluster_donuts_on(archive, args.legend, &mut request);
            hex_line = format!(
                " · {} at {:.0}%{}",
                panel::cluster_hud_text(hud, None),
                walker::cluster_alpha(p.zoom) * 100.0,
                if hud.refused > 0 { " (REFUSED RECORDS)" } else { "" }
            );
        }
        // States mode: the pies and outlines, no dots.
        if args.mode == Mode::States {
            style.opacity = 0.0;
            // The selection belongs to its own framed view only.
            let selected = args.state.filter(|_| p.name.ends_with("_selected"));
            let n = states_on(args.legend, selected, &mut request);
            hex_line = format!(" · {}", panel::states_hud_text(selected, n));
        }
        let layers = (style.opacity > 0.0).then(|| settle(&source, &style, &request));
        let settle_ms = t.elapsed().as_secs_f64() * 1e3;
        let tiles = spec.as_ref().map(|spec| {
            let cover = request.cover();
            runtime.block_on(fetch_cover(spec.tiles.as_ref(), &cover, &cache)).expect("basemap tiles")
        });
        let basemap = spec.as_ref().zip(tiles.as_ref()).map(|(spec, tiles)| (tiles, &spec.style));
        let (frame, stats) = race_frame(layers.as_ref(), &source, &style, &request, basemap);
        if cluster_archive.is_some() {
            // What the GPU pass drew (on the frame) and what its cap cut.
            hex_line.push_str(&format!(" · {} on the frame, {} labels", frame.donuts_drawn, frame.overlay_labels_drawn));
            if frame.donuts_truncated > 0 {
                hex_line.push_str(&format!(", {} truncated by the capture", frame.donuts_truncated));
            }
        }
        let path = dir.join(format!("{}.png", p.name));
        frame.save_png(&path).expect("write a PNG");
        let dots = stats.map_or_else(|| "no dots (below z12 in this mode)".to_string(), |s| stats_line(&s));
        println!("{}: {dots}{hex_line} (settled in {settle_ms:.0} ms) -> {}", p.name, path.display());
    }
}

/// `--measure`: each preset settled headless, then the headless zoom at
/// which a Manhattan view's ppd first reads 1.00.
fn measure(args: &Args) {
    let source = required_source(args);
    let style = walker::dot_style(args.budget);
    let race = source.race().expect("a race archive").clone();
    println!(
        "race_dots --measure: {} · crossover z{} · part_zoom {} · budget {} dots · attribution {:?}",
        args.dots.display(),
        race.crossover,
        race.part_zoom,
        style.budget_dots,
        source.descriptor().attribution
    );
    for at in ["us", "la", "houston", "dc", "manhattan"] {
        let p = preset(at);
        let t = Instant::now();
        let layers = settle(&source, &style, &request_at(p, args.size));
        let s = layers.stats(LAYER).expect("the layer");
        println!("{}: {} (settled in {:.0} ms)", p.name, stats_line(&s), t.elapsed().as_secs_f64() * 1e3);
    }
    let manhattan = preset("manhattan");
    let budget = f64::from(style.budget_dots);
    let mut first_one = None;
    for step in 0..=30 {
        let z = 11.0 + f64::from(step) * 0.1;
        let p = Preset { zoom: z, ..manhattan };
        let layers = settle(&source, &style, &request_at(p, args.size));
        let s = layers.stats(LAYER).expect("the layer");
        let drawn = drawn_dots(&layers, s.ppd);
        println!(
            "  manhattan z{z:.2}: ppd {:.3} · {} · {} people in view · {} dots drawn ({:.2}x the budget)",
            s.ppd,
            panel::level_text(&s),
            panel::people_text(s.people_in_view),
            panel::thousands(drawn.round() as u64),
            drawn / budget
        );
        if s.ppd <= panel::ONE_PPD && first_one.is_none() {
            first_one = Some(z);
        }
    }
    match first_one {
        Some(z) => {
            println!("ppd reads 1.00 from z{z:.2} over Manhattan (headless {}x{})", args.size.map_or(1920, |s| s.0), args.size.map_or(1080, |s| s.1));
            gate("race.ppd_one_zoom_headless", z, "zoom");
        }
        None => println!("ppd never read 1.00 over Manhattan up to z{}", walker::MAX_ZOOM),
    }
}

// ---------------------------------------------------------------- windowed

fn windowed(args: Args, process_start: Instant) {
    println!("Drag to pan, scroll to zoom. The 2020 Census, one dot per person, by race and ethnicity.");
    println!(
        "Keys: {} day/night terminator, {} globe or flat. Click a legend group to hide or show it.",
        shell::TERMINATOR_KEY.to_uppercase(),
        shell::GLOBE_KEY.to_uppercase()
    );
    println!("basemap: {}", args.basemap.name());
    let at = args.at;
    let (pan, sweep, quit_after) = (args.pan, args.zoom_sweep, args.quit_after);
    let mode = args.mode;
    let options =
        PanelOptions { basemap: args.basemap, at: (at.lon, at.lat, at.zoom), budget: args.budget, size: (1280.0, 800.0), legend: args.legend, mode: args.mode };
    let mut sources = Some(RaceSources { dots: args.dots.clone(), hexes: args.hexes.clone(), clusters: args.clusters.clone() });

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let mut panel_entity = None;
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Race dots".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let sources = sources.take().expect("one window");
                let panel = cx.new(|cx| RaceMapPanel::with_options(window, cx, sources, options));
                panel_entity = Some(panel.clone());
                panel
            },
        )
        .expect("failed to open window");
        // Frontmost, or the window server never asks it to paint.
        cx.activate(true);
        let panel = panel_entity.expect("the window was built");
        let map = panel.read(cx).map().clone();
        let opened = Instant::now();
        if pan {
            pan_task(map.clone(), at, cx);
        }
        if sweep {
            zoom_sweep_task(map.clone(), panel.clone(), at, mode, cx);
        }
        report_task(map, panel, pan, quit_after, process_start, opened, cx);
    });
}

/// The pan: the `--at` centre circled 1.5 tiles east-west and 0.5 tiles
/// north-south (a tile is `360 / 2^z` degrees), so the screen motion is the
/// same at every preset's zoom.
fn pan_task(map: Entity<MapView>, at: Preset, cx: &mut App) {
    let amp = 360.0 / 2f64.powf(at.zoom);
    cx.spawn(async move |cx| {
        let started = Instant::now();
        loop {
            cx.background_executor().timer(Duration::from_millis(16)).await;
            let t = started.elapsed().as_secs_f64();
            let lon = at.lon + 1.5 * amp * (t * 0.25).sin();
            let lat = at.lat + 0.5 * amp * (t * 0.17).sin();
            map
                .update(cx, |view, cx| {
                    view.set_camera(lon, lat, at.zoom);
                    cx.notify();
                });
        }
    })
    .detach();
}

/// Every 16 ms: the first dots (MG-R14), the 5 s status line (with
/// `draw_calls_total`, the proof the window painted), the pan window's
/// frames (probe), and the quit.
fn report_task(
    map: Entity<MapView>,
    panel: Entity<RaceMapPanel>,
    pan: bool,
    quit_after: Option<f64>,
    process_start: Instant,
    opened: Instant,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let mut first_dots: Option<Duration> = None;
        // The archives open off the UI thread: the attribution line carries
        // the dot archive's credit once they have attached.
        let mut credit_checked = false;
        let mut last_report = Instant::now();
        let mut last_paints = gpui_maps::render::paint_census().paints_submitted;
        let (mut gpu_max, mut rss_max) = (0u64, 0u64);
        #[allow(unused_mut, unused_variables)]
        let mut pan_window: Option<(Instant, u64, u64)> = None;
        let _ = pan;
        loop {
            cx.background_executor().timer(Duration::from_millis(16)).await;
            let (stats, zoom) = map.read_with(cx, |v, _| (v.dot_stats(LAYER), v.camera().zoom));
            if let Some(s) = stats {
                gpu_max = gpu_max.max(s.gpu_bytes);
            }
            rss_max = rss_max.max(gpui_maps::render::rss_bytes());
            if !credit_checked && panel.read_with(cx, |p, _| p.loaded()) {
                credit_checked = true;
                let credit = map
                    .read_with(cx, |v, _| v.frame_attribution().map(|c| gpui_maps::tile_source::attribution_text(&c)).unwrap_or_default());
                println!("attribution: {credit}");
                if !credit.contains(about::CENSUS_CREDIT) {
                    eprintln!("race_dots: the attribution line lacks {:?} -- the archive's metadata carries no credit", about::CENSUS_CREDIT);
                }
            }
            if first_dots.is_none() && stats.is_some_and(|s| s.dots_drawn > 0) {
                let t = process_start.elapsed();
                first_dots = Some(t);
                println!(
                    "first dots: {:.0} ms after process start ({:.0} ms after the window opened)",
                    t.as_secs_f64() * 1e3,
                    opened.elapsed().as_secs_f64() * 1e3
                );
                gate("race.first_dots_ms", t.as_secs_f64() * 1e3, "ms");
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                let secs = last_report.elapsed().as_secs_f64();
                last_report = Instant::now();
                let census = gpui_maps::render::paint_census();
                let fps = (census.paints_submitted - last_paints) as f64 / secs;
                last_paints = census.paints_submitted;
                let a = allocation_stats();
                if let (Mode::Clusters, donuts, truncated, refused) =
                    panel.read_with(cx, |p, _| (p.mode(), p.donuts_drawn(), p.donuts_truncated(), p.clusters_refused()))
                {
                    let engine = map.read_with(cx, |v, _| v.donuts_truncated());
                    println!(
                        "[{:>5.1}s] donuts drawn {donuts} | truncated {truncated} (engine {engine}) | refused {refused} | draws total {}",
                        opened.elapsed().as_secs_f64(),
                        allocation_stats().draw_calls_total
                    );
                }
                if let (Mode::Hexagons, drawn, paths) = panel.read_with(cx, |p, _| (p.mode(), p.hexagons_drawn(), p.hex_paint_paths)) {
                    println!(
                        "[{:>5.1}s] hexagons drawn {drawn} | paint updates table-only {} rebake {} replan {} | draws total {}",
                        opened.elapsed().as_secs_f64(),
                        paths[0],
                        paths[1],
                        paths[2],
                        a.draw_calls_total
                    );
                }
                match stats {
                    Some(s) => println!(
                        "[{:>5.1}s] {fps:.1} fps | z{zoom:.2} | {} | draws total {} | rss {:.0} MB",
                        opened.elapsed().as_secs_f64(),
                        stats_line(&s),
                        a.draw_calls_total,
                        a.rss_bytes as f64 / 1e6
                    ),
                    None => println!("[{:>5.1}s] no dot layer | draws total {}", opened.elapsed().as_secs_f64(), a.draw_calls_total),
                }
            }
            // MG-R11's pan window: from 3 s after the first dots to the quit.
            #[cfg(feature = "probe")]
            if pan && pan_window.is_none() && first_dots.is_some_and(|t| process_start.elapsed() >= t + Duration::from_secs(3)) {
                pan_window = Some((Instant::now(), allocation_stats().draw_calls_total, stats.map_or(0, |s| s.rebuilds_total)));
                (gpu_max, rss_max) = (0, 0);
                gpui_maps::render::zoom_probe::start("pan steady");
            }
            if quit_after.is_some_and(|q| opened.elapsed().as_secs_f64() >= q) {
                #[cfg(feature = "probe")]
                if let Some((opened, draws, rebuilt)) = pan_window.take() {
                    gpui_maps::render::zoom_probe::stop();
                    report_frames("pan", opened.elapsed().as_secs_f64(), allocation_stats().draw_calls_total - draws, rss_max, gpu_max);
                    // Review F4: the pan's tile rebuilds.
                    let n = stats.map_or(0, |s| s.rebuilds_total) - rebuilt;
                    println!("rebuilds: {n} over the pan");
                    gate("race.pan.rebuilds", n as f64, "tiles");
                }
                println!(
                    "quit after {:.0}s; peak rss {:.0} MB; dot gpu max {:.1} MB; draws total {}",
                    opened.elapsed().as_secs_f64(),
                    peak_rss_bytes() as f64 / 1e6,
                    gpu_max as f64 / 1e6,
                    allocation_stats().draw_calls_total
                );
                cx.update(|cx| cx.quit());
                return;
            }
        }
    })
    .detach();
}

/// MG-R11's line, as `world_dots`': CPU frame p50/p95/max, frames over
/// 16.7 and 33 ms, GPU pass samples, draw calls in the window (0 = nothing
/// painted, the figures are void), RSS and dot GPU bytes at their maxima.
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
    gate(&format!("race.{prefix}.frames"), cpu.len() as f64, "frames");
    gate(&format!("race.{prefix}.cpu_p95_ms"), quantile(&cpu, 0.95), "ms");
    gate(&format!("race.{prefix}.frames_over_33ms"), over(33.0) as f64, "frames");
    gate(&format!("race.{prefix}.draw_calls"), draws as f64, "draws");
    gate(&format!("race.{prefix}.rss_max_mb"), rss_max as f64 / 1e6, "MB");
    gate(&format!("race.{prefix}.dot_gpu_max_mb"), gpu_max as f64 / 1e6, "MB");
}

/// Counts the layer's switches between parts and cells (one per change of
/// "is the frame's level the part level") and the camera's crossings of
/// the crossover zoom: more switches than crossings is F8's flicker, fewer
/// is hysteresis.
// Read by the probe's zoom sweep and the tests only.
#[cfg_attr(not(any(test, feature = "probe")), allow(dead_code))]
#[derive(Debug, Default)]
struct CrossoverWatch {
    last: Option<bool>,
    switches: u32,
    last_side: Option<bool>,
    crossings: u32,
}

impl CrossoverWatch {
    #[cfg_attr(not(any(test, feature = "probe")), allow(dead_code))]
    fn see(&mut self, level: u8, zoom: f64, crossover: f64) {
        let parts = level == GridTileKey::PART_LEVEL;
        if self.last.is_some_and(|l| l != parts) {
            self.switches += 1;
        }
        self.last = Some(parts);
        let above = zoom >= crossover;
        if self.last_side.is_some_and(|s| s != above) {
            self.crossings += 1;
        }
        self.last_side = Some(above);
    }
}

/// `--zoom-sweep`: z3 -> z14 -> z3 over the preset's centre, slow (8 s)
/// and flicked (0.6 s), each followed by a settle; then 4 s hovering the
/// crossover (F8). Prints the frames, the zoom at which the ppd first reads
/// 1.00 on the slow way in, and the crossover's level switches.
#[cfg(feature = "probe")]
fn zoom_sweep_task(map: Entity<MapView>, panel: Entity<RaceMapPanel>, at: Preset, mode: Mode, cx: &mut App) {
    use gpui_maps::render::zoom_probe;
    // Hexagon mode (MG-R12) sweeps Walker's hexagon range, z3 -> z12.75;
    // cluster mode (MG-R13) z3 -> z13, through the handover to the dots.
    // Both start by waiting for their own layer, and skip the crossover
    // hover.
    let hex = mode != Mode::Dots;
    let hi = match mode {
        Mode::Dots => walker::MAX_ZOOM,
        Mode::Hexagons => walker::HEX_WINDOWS[4].2,
        Mode::Clusters => 13.0,
        Mode::States => states::FIT_MAX_ZOOM,
    };
    let lo = walker::MIN_ZOOM;
    cx.spawn(async move |cx| {
        let tick = Duration::from_millis(16);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            // Hexagon mode waits at the sweep's start, z3: a preset above
            // z12.75 (Manhattan's z13) draws no hexagons at all.
            let drawn = if hex {
                map.update(cx, |view, cx| {
                    view.set_camera(at.lon, at.lat, lo);
                    cx.notify();
                });
                panel.read_with(cx, |p, _| match mode { Mode::Clusters => p.donuts_drawn() > 0, Mode::States => p.state_pies_drawn() > 0, _ => p.hexagons_drawn() > 0 })
            } else {
                map.read_with(cx, |v, _| v.dot_stats(LAYER).is_some_and(|s| s.dots_drawn > 0))
            };
            if drawn {
                break;
            }
            if Instant::now() > deadline {
                println!("zoom sweep: nothing drawn within 60 s; giving up");
                cx.update(|cx| cx.quit());
                return;
            }
            cx.background_executor().timer(Duration::from_millis(50)).await;
        }
        // The archive's own crossover (review F10), not a hardcoded one --
        // read once something drew, i.e. once the archive had opened.
        let crossover = panel.read_with(cx, |p, _| p.crossover()).unwrap_or(f64::NAN);
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
            if !set(cx, lo) {
                return;
            }
            cx.background_executor().timer(tick).await;
        }
        let draws_before = allocation_stats().draw_calls_total;
        let (mut gpu_max, mut rss_max) = (0u64, 0u64);
        // MG-R11's RSS criterion is a delta: from here, parked at z3.
        let rss_start = gpui_maps::render::rss_bytes();
        let mut ppd_one: Option<f64> = None;
        let mut crossing = CrossoverWatch::default();
        let rebuilds = |cx: &mut gpui::AsyncApp| map.read_with(cx, |v, _| v.dot_stats(LAYER).map_or(0, |s| s.rebuilds_total));
        let rebuilds_before = rebuilds(cx);
        let mut rebuilds_leg = Vec::new();
        zoom_probe::start("parked z3");
        // (label, ramp from -> to, seconds, hover amplitude around the crossover)
        type Leg = (String, Option<(f64, f64)>, f64, bool);
        let mut legs: Vec<Leg> = vec![
            ("parked z3".into(), None, 1.0, false),
            (format!("slow in (z3->z{hi}, 8 s)"), Some((lo, hi)), 8.0, false),
            (format!("settle z{hi}"), None, 2.0, false),
            (format!("slow out (z{hi}->z3, 8 s)"), Some((hi, lo)), 8.0, false),
            ("settle z3".into(), None, 2.0, false),
            ("flick in (0.6 s)".into(), Some((lo, hi)), 0.6, false),
            (format!("settle z{hi} #2"), None, 2.0, false),
            ("flick out (0.6 s)".into(), Some((hi, lo)), 0.6, false),
            ("settle z3 #2".into(), None, 2.0, false),
        ];
        if !hex {
            legs.push(("hover crossover (+- 0.03, 4 s)".into(), None, 4.0, true));
        }
        let started_all = Instant::now();
        let mut z = lo;
        for (i, (label, ramp, secs, hover)) in legs.into_iter().enumerate() {
            if i > 0 {
                zoom_probe::mark(&label);
            }
            let leg_start = rebuilds(cx);
            cx.update(|cx| cx.activate(true));
            let started = Instant::now();
            loop {
                if let Some(s) = map.read_with(cx, |v, _| v.dot_stats(LAYER)) {
                    gpu_max = gpu_max.max(s.gpu_bytes);
                    if i == 1 && ppd_one.is_none() && s.dots_drawn > 0 && s.ppd <= panel::ONE_PPD {
                        ppd_one = Some(z);
                    }
                    if hover {
                        crossing.see(s.level, z, crossover);
                    }
                }
                rss_max = rss_max.max(gpui_maps::render::rss_bytes());
                let elapsed = started.elapsed().as_secs_f64();
                let t = (elapsed / secs).min(1.0);
                if let Some((from, to)) = ramp {
                    let e = t * t * (3.0 - 2.0 * t);
                    z = from + (to - from) * e;
                }
                if hover {
                    z = crossover + 0.03 * (elapsed * std::f64::consts::TAU).sin();
                }
                if !set(cx, z) {
                    return;
                }
                if t >= 1.0 {
                    break;
                }
                cx.background_executor().timer(tick).await;
            }
            // Review L4: RSS back at z3 after each in/out pass -- the second
            // reading at or below the first says a pass leaks nothing.
            if label.starts_with("settle z3") {
                println!("rss after {label}: {:.0} MB", gpui_maps::render::rss_bytes() as f64 / 1e6);
            }
            rebuilds_leg.push((label, rebuilds(cx) - leg_start));
        }
        zoom_probe::stop();
        // Review F4: what the sweep cost in tile rebuilds, by leg.
        println!(
            "rebuilds: {} over the sweep | {}",
            rebuilds(cx) - rebuilds_before,
            rebuilds_leg.iter().map(|(l, n)| format!("{l}: {n}")).collect::<Vec<_>>().join(" · ")
        );
        report_frames("sweep", started_all.elapsed().as_secs_f64(), allocation_stats().draw_calls_total - draws_before, rss_max, gpu_max);
        println!(
            "rss at the sweep's start {:.0} MB; max - start = {:.0} MB",
            rss_start as f64 / 1e6,
            rss_max.saturating_sub(rss_start) as f64 / 1e6
        );
        gate("race.sweep.rss_delta_mb", rss_max.saturating_sub(rss_start) as f64 / 1e6, "MB");
        match ppd_one {
            Some(z) => {
                println!("ppd first reads 1.00 at z{z:.2} on the slow way in");
                gate("race.ppd_one_zoom", z, "zoom");
            }
            None => println!("ppd never read 1.00 on the slow way in"),
        }
        if !hex {
            println!(
            "crossover: {} parts<->cells switches over {} crossings of z{crossover} in 4 s hovering +- 0.03 (more switches than crossings is F8's flicker; fewer is hysteresis)",
            crossing.switches, crossing.crossings
        );
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(not(feature = "probe"))]
fn zoom_sweep_task(_: Entity<MapView>, _: Entity<RaceMapPanel>, _: Preset, _: Mode, _: &mut App) {
    panic!("--zoom-sweep needs --features probe");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_parse() {
        for at in ["us", "manhattan", "la", "houston", "dc"] {
            let p = preset(at);
            assert!((walker::MIN_ZOOM..=walker::MAX_ZOOM).contains(&p.zoom), "{at}");
        }
        assert_eq!(preset("us").zoom, walker::START.2);
    }

    #[test]
    fn the_crossover_watch_counts_switches() {
        let mut w = CrossoverWatch::default();
        let p = GridTileKey::PART_LEVEL;
        for (level, z) in [(3, 9.7), (3, 9.71), (p, 9.73), (p, 9.74), (3, 9.70), (p, 9.70)] {
            w.see(level, z, 9.72);
        }
        assert_eq!((w.switches, w.crossings), (3, 2));
    }

    #[test]
    fn the_capture_ramp_carries_walkers_opacity() {
        let ramp = race_ramp(&walker::dot_style(None));
        assert_eq!(ramp.len(), 8);
        assert!(ramp.iter().all(|c| (c.a - walker::DOT_OPACITY).abs() < 1e-6));
    }
}
