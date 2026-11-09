//! The 3D-building perf-gate sections (spec
//! `docs/superpowers/specs/2026-09-29-3d-building-extrusion-design.md` §5.4),
//! run by `examples/perf_gate.rs` and described in `docs/perf-gate.md`.
//!
//! ```text
//! buildings_gate --measure                         # headless: the `buildings` section
//! buildings_gate --pan [--opaque] --quit-after 25  # windowed: `buildings.pan` / `buildings.pan_opaque`
//! buildings_gate --measure-tile PATH [--style liberty|streets-3d|STYLE.json] [--zoom Z]
//! ```
//!
//! - `--measure`: the three vendored z14 building fixtures
//!   (`benches/fixtures/{shinjuku,manhattan,singapore}_z14.pbf`, ODbL, see
//!   `benches/fixtures/README.md`), each matched against Liberty at z14 and
//!   tessellated by `render::extrusion::tessellate_extrusions`. Prints
//!   `gate: buildings.tess.<city>.{vertices,indices,walls,bake_ms,upload_mb}`.
//!   No window, no network.
//! - `--pan` (needs `--features probe`): Liberty on OpenFreeMap (keyless) over
//!   Shinjuku at z16, pitch 60, bearing 30, a small drifting pan for the run.
//!   Liberty's `building-3d` is opacity 0.8, so this is the TRANSLUCENT path
//!   (a depth prepass plus a stencilled colour pass: the geometry twice).
//!   `--opaque` forces that layer's opacity to 1 -- one opaque pass -- and
//!   prints under `buildings.pan_opaque` instead, so the prepass cost is the
//!   difference between the two sections. The measured window opens 3 s
//!   after the first extrusion triangles are drawn and closes at
//!   `--quit-after`. A run whose window never opens prints no gate line (the
//!   gate refuses it).
//! - `--measure-tile`: one tile file, matched at `--zoom` (16 by default)
//!   against Liberty or Mapbox Streets through `style_spec::with_3d_buildings`
//!   (`streets-3d`), or any style file. The same five numbers, under
//!   `buildings.tile.<file stem>`.
//!   Not part of the gate.
//!
//! Gate lines are printed unconditionally (not only under `US_BG_GATE`): this
//! binary has no other output worth keeping apart from them.

// `crate::kit` for the pan: `PanGate` (occlusion, draw calls, GPU pass mean),
// `OPENFREEMAP_CREDIT_HTML` (the source is built uncached below), `asset`, `peak_rss_bytes`.
use swat_maps_kit::kit;

use std::path::Path;

use gpui_maps::render::extrusion::tessellate_extrusions;

/// The three vendored fixtures: `(gate name, file)`.
const FIXTURES: [(&str, &str); 3] = [
    ("shinjuku_z14", "benches/fixtures/shinjuku_z14.pbf"),
    ("manhattan_z14", "benches/fixtures/manhattan_z14.pbf"),
    ("singapore_z14", "benches/fixtures/singapore_z14.pbf"),
];

/// Shinjuku, the pan's centre: the showcase's own camera (Task 10).
#[cfg(feature = "probe")]
const PAN_AT: (f64, f64) = (139.6995, 35.6905);
#[cfg(feature = "probe")]
const PAN_ZOOM: f64 = 16.0;
#[cfg(feature = "probe")]
const PAN_PITCH_DEG: f32 = 60.0;
#[cfg(feature = "probe")]
const PAN_BEARING_DEG: f32 = 30.0;

fn gate(name: &str, value: f64, unit: &str) {
    println!("gate: {name} {value} {unit}");
}

/// Every `buildings.tess.*` name `--measure` prints, for the band test.
#[cfg_attr(not(test), allow(dead_code))]
fn metric_names() -> Vec<String> {
    FIXTURES
        .iter()
        .flat_map(|(city, _)| {
            ["vertices", "indices", "walls", "bake_ms", "upload_mb"].map(|m| format!("buildings.tess.{city}.{m}"))
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if has("--measure") {
        measure();
    } else if let Some(path) = value("--measure-tile") {
        let style = value("--style").unwrap_or_else(|| "liberty".into());
        let zoom = value("--zoom").and_then(|z| z.parse().ok()).unwrap_or(16u8);
        measure_tile(Path::new(&path), &style, zoom);
    } else if has("--pan") {
        let quit_after = value("--quit-after").and_then(|s| s.parse().ok()).unwrap_or(25.0);
        pan(has("--opaque"), quit_after);
    } else {
        eprintln!("usage: buildings_gate --measure | --pan [--opaque] [--quit-after SECS] | --measure-tile PATH [--style liberty|streets-3d|STYLE.json] [--zoom Z]");
        std::process::exit(2);
    }
}

// ---------------------------------------------------------------- headless

fn liberty() -> gpui_maps::style::Style {
    kit::compile_style("examples/liberty-style.json").expect("compile Liberty")
}

/// The five numbers for one tile: counts from the mesh, `bake_ms` the best of
/// three `match_tile` + `tessellate_extrusions` runs (the work a tile bake
/// adds for buildings, and the match it rides on).
fn report(prefix: &str, tile: &gpui_maps::mvt::DecodedTile, style: &gpui_maps::style::Style, zoom: u8) {
    let mut best = f64::INFINITY;
    let mut mesh = None;
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let plan = gpui_maps::style::match_tile(tile, style, zoom);
        let m = tessellate_extrusions(&plan);
        best = best.min(started.elapsed().as_secs_f64() * 1e3);
        mesh = Some(m);
    }
    let mesh = mesh.expect("three runs");
    gate(&format!("{prefix}.vertices"), mesh.vertex_count() as f64, "count");
    gate(&format!("{prefix}.indices"), mesh.index_count() as f64, "count");
    gate(&format!("{prefix}.walls"), mesh.wall_count() as f64, "count");
    gate(&format!("{prefix}.bake_ms"), (best * 1000.0).round() / 1000.0, "ms");
    gate(&format!("{prefix}.upload_mb"), mesh.upload_bytes() as f64 / 1e6, "MB");
}

fn decode(path: &Path) -> gpui_maps::mvt::DecodedTile {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e} (run from the repo root)", path.display()));
    gpui_maps::mvt::decode(bytes).unwrap_or_else(|e| panic!("decode {}: {e}", path.display()))
}

fn measure() {
    let style = liberty();
    for (city, file) in FIXTURES {
        // Bench fixtures, not shipped assets (kit/build.rs does not embed
        // them): read from the checkout, as db655685 did for the kit test.
        let tile = decode(&kit::repo_root().join(file));
        report(&format!("buildings.tess.{city}"), &tile, &style, 14);
    }
}

fn measure_tile(path: &Path, style: &str, zoom: u8) {
    let style = match style {
        "liberty" => liberty(),
        "streets-3d" => {
            let bytes = kit::asset_bytes("examples/mapbox-streets-style.json").expect("read the Streets style");
            let mut doc: serde_json::Value = serde_json::from_slice(&bytes).expect("parse the Streets style");
            gpui_maps::style_spec::with_3d_buildings(&mut doc).expect("with_3d_buildings");
            gpui_maps::style_spec::parse_and_compile(&serde_json::to_vec(&doc).expect("serialise"))
                .expect("compile Streets with 3D buildings")
        }
        path if path.ends_with(".json") => {
            let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            gpui_maps::style_spec::parse_and_compile(&bytes).unwrap_or_else(|e| panic!("compile {path}: {e}"))
        }
        other => {
            eprintln!("--style {other:?} is not one of: liberty, streets-3d, or a style .json path");
            std::process::exit(2);
        }
    };
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("tile");
    report(&format!("buildings.tile.{stem}"), &decode(path), &style, zoom);
}

// ---------------------------------------------------------------- windowed

#[cfg(not(feature = "probe"))]
fn pan(_opaque: bool, _quit_after: f64) {
    eprintln!("buildings_gate --pan needs --features probe (the frame probe); scripts/perf-gate.sh builds it that way");
    std::process::exit(2);
}

/// Liberty with `building-3d` forced to opacity 1 when `opaque`.
#[cfg(feature = "probe")]
fn pan_style(opaque: bool) -> gpui_maps::style::Style {
    let bytes = kit::asset_bytes("examples/liberty-style.json").expect("read Liberty");
    let mut doc: serde_json::Value = serde_json::from_slice(&bytes).expect("parse Liberty");
    if opaque {
        let layer = doc["layers"]
            .as_array_mut()
            .and_then(|layers| layers.iter_mut().find(|l| l["id"] == "building-3d"))
            .expect("Liberty has a building-3d layer");
        layer["paint"]["fill-extrusion-opacity"] = serde_json::json!(1.0);
    }
    gpui_maps::style_spec::parse_and_compile(&serde_json::to_vec(&doc).expect("serialise")).expect("compile Liberty")
}

#[cfg(feature = "probe")]
fn quantile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * q).round() as usize]
}

#[cfg(feature = "probe")]
fn pan(opaque: bool, quit_after: f64) {
    use std::time::{Duration, Instant};

    use gpui::{px, size, App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
    use gpui_maps::map_view::{extrusion_triangles_last_frame, MapView};
    use gpui_maps::render::zoom_probe;

    let prefix = if opaque { "buildings.pan_opaque" } else { "buildings.pan" };
    println!("{prefix}: Liberty (building-3d {}), Shinjuku z{PAN_ZOOM} pitch {PAN_PITCH_DEG} bearing {PAN_BEARING_DEG}, quit after {quit_after} s", if opaque { "forced opaque" } else { "0.8, translucent" });
    let style = pan_style(opaque);
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime");
    let handle = runtime.handle().clone();
    let mut init = Some(style);

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let mut map = None;
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Buildings gate".into()), ..Default::default() }),
                ..Default::default()
            },
            |_window, cx| {
                let style = init.take().expect("one window");
                let view = cx.new(|_| {
                    let mut view =
                        MapView::new(PAN_AT.0, PAN_AT.1, PAN_ZOOM, 1280.0, 800.0, style, uncached_openfreemap(), handle);
                    view.jump_to(gpui_maps::camera_control::CameraTarget {
                        pitch: Some(PAN_PITCH_DEG.to_radians()),
                        bearing: Some(PAN_BEARING_DEG.to_radians()),
                        ..Default::default()
                    });
                    view
                });
                map = Some(view.clone());
                view
            },
        )
        .expect("failed to open window");
        // Frontmost, or the window server never asks it to paint.
        cx.activate(true);
        let map = map.expect("the window was built");
        let opened = Instant::now();

        // The pan: about two z16 tiles each way, slow enough that the frame
        // cost, not tile loading, dominates once the neighbourhood is baked.
        let panned = map.downgrade();
        cx.spawn(async move |cx| {
            let started = Instant::now();
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                let t = started.elapsed().as_secs_f64();
                let lon = PAN_AT.0 + 0.010 * (t * 0.25).sin();
                let lat = PAN_AT.1 + 0.006 * (t * 0.17).sin();
                if panned
                    .update(cx, |view, cx| {
                        view.set_camera(lon, lat, PAN_ZOOM);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();

        cx.spawn(async move |cx| {
            let _keep = map;
            let mut first_buildings: Option<Instant> = None;
            let mut window: Option<(kit::PanGate, Instant)> = None;
            let (mut triangles_max, mut rss_max) = (0u64, 0u64);
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                let triangles = extrusion_triangles_last_frame();
                if first_buildings.is_none() && triangles > 0 {
                    first_buildings = Some(Instant::now());
                    println!("first buildings: {:.0} ms after the window opened", opened.elapsed().as_secs_f64() * 1e3);
                }
                if window.is_none() && first_buildings.is_some_and(|t| t.elapsed() >= Duration::from_secs(3)) {
                    window = Some((kit::PanGate::open(), Instant::now()));
                    zoom_probe::start("buildings pan");
                }
                if window.is_some() {
                    triangles_max = triangles_max.max(triangles);
                    rss_max = rss_max.max(gpui_maps::render::rss_bytes());
                }
                if opened.elapsed().as_secs_f64() >= quit_after {
                    match window.take() {
                        Some((gate_window, window_opened)) => {
                            zoom_probe::stop();
                            report_pan(prefix, &gate_window, window_opened.elapsed().as_secs_f64(), triangles_max, rss_max);
                        }
                        None => println!("{prefix}: no extrusion drawn 3 s before the quit; nothing measured (no gate lines)"),
                    }
                    println!("quit after {:.0} s; peak rss {:.0} MB", opened.elapsed().as_secs_f64(), kit::peak_rss_bytes() as f64 / 1e6);
                    cx.update(|cx| cx.quit());
                    return;
                }
            }
        })
        .detach();
    });
    std::mem::forget(runtime);
}

/// The section's gate lines. `occluded_paints`, `draw_calls_total` and `window_s` are
/// validity evidence (`perf_gate`'s `invalid_reason`: a covered window or 0
/// draws voids the run) and never graded.
#[cfg(feature = "probe")]
fn report_pan(prefix: &str, window: &kit::PanGate, secs: f64, triangles_max: u64, rss_max: u64) {
    let (_, frames) = gpui_maps::render::zoom_probe::snapshot();
    let cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
    let gpu: Vec<f64> = frames.iter().map(|f| f.gpu_ms_last_sample).filter(|&ms| ms > 0.0).collect();
    let rows = window.rows(prefix, secs, gpui_maps::render::paint_census(), gpui_maps::render::allocation_stats());
    let row = |name: &str| rows.iter().find(|(n, _, _)| n == &format!("{prefix}.{name}")).map(|r| r.1);
    let draws = row("draw_calls").unwrap_or(0.0);
    let over = cpu.iter().filter(|&&c| c > 16.7).count();
    println!(
        "-- {prefix}: {} frames | cpu p50 {:.2} p99 {:.2} max {:.2} ms | {over} over 16.7 ms | gpu pass p95 {:.2} max {:.2} ms | extrusion triangles max {triangles_max} | draw calls {draws}{} --",
        cpu.len(),
        quantile(&cpu, 0.5),
        quantile(&cpu, 0.99),
        quantile(&cpu, 1.0),
        quantile(&gpu, 0.95),
        quantile(&gpu, 1.0),
        if draws == 0.0 { " (NOTHING PAINTED: void)" } else { "" },
    );
    // The mean over every GPU pass sample the window took (`allocation_stats`),
    // falling back to the per-frame samples when there were none.
    let mean = row("gpu_pass_mean_ms").unwrap_or_else(|| gpu.iter().sum::<f64>() / gpu.len().max(1) as f64);
    gate(&format!("{prefix}.gpu_pass_mean_ms"), mean, "ms");
    gate(&format!("{prefix}.gpu_pass_p95_ms"), quantile(&gpu, 0.95), "ms");
    gate(&format!("{prefix}.gpu_pass_max_ms"), quantile(&gpu, 1.0), "ms");
    gate(&format!("{prefix}.cpu_p99_ms"), quantile(&cpu, 0.99), "ms");
    gate(&format!("{prefix}.frames_over_16_7ms"), over as f64, "frames");
    gate(&format!("{prefix}.extrusion_triangles_max"), triangles_max as f64, "triangles");
    gate(&format!("{prefix}.rss_max_mb"), rss_max as f64 / 1e6, "MB");
    gate(&format!("{prefix}.occluded_paints"), row("occluded_paints").unwrap_or(0.0), "paints");
    gate(&format!("{prefix}.draw_calls_total"), draws, "draws");
    // How long the graded window was (info): a run whose first buildings
    // landed late grades only a few seconds of frames.
    gate(&format!("{prefix}.window_s"), secs, "s");
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_buildings_metric_name_lands_in_the_intended_band() {
        let names = super::metric_names();
        for city in ["shinjuku_z14", "manhattan_z14", "singapore_z14"] {
            for exact in ["vertices", "indices", "walls"] {
                assert!(names.contains(&format!("buildings.tess.{city}.{exact}")), "{city}.{exact}");
            }
            assert!(names.contains(&format!("buildings.tess.{city}.bake_ms")));
            assert!(names.contains(&format!("buildings.tess.{city}.upload_mb")));
        }
    }
}

/// OpenFreeMap without the demos' tile disk cache (`kit::with_tile_cache`):
/// a gate times the network path, never a warm cache.
#[cfg(feature = "probe")]
fn uncached_openfreemap() -> std::sync::Arc<dyn gpui_maps::tile_source::TileSource> {
    std::sync::Arc::new(gpui_maps::tile_source::http::HttpTileSource::new(
        gpui_maps::basemap::OPENFREEMAP_PLANET_TILE_URL,
        gpui_maps::basemap::OPENFREEMAP_PLANET_MAX_ZOOM,
        kit::OPENFREEMAP_CREDIT_HTML,
    ))
}
