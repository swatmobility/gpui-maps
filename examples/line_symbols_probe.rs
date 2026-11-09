//! **Route chevrons: engine-placed line symbols vs one rotated point per
//! arrow.** The A/B behind `MapView::declare_source_symbols`.
//!
//! A planning map draws 30 vehicle routes of ~150 km each with a direction
//! chevron every 200 m. Before line symbols the only way to say that was
//! one rotated `Point` feature per chevron -- ~22,500 of them, generated
//! on the UI thread at every push and walked by the overlay symbol loop
//! on every frame. This probe runs the three shapes over the SAME
//! synthetic routes, in a real window (so `paint` runs; every number is
//! gated on `draw_calls_total > 0`):
//!
//! | `--arm`    | what is pushed |
//! |------------|----------------|
//! | `none`     | the 30 routes as lines, no arrows (the floor) |
//! | `declared` | the same lines, with `declare_source_symbols` (`symbol-spacing` = `--spacing`) |
//! | `points`   | the same lines, plus one rotated point feature per 200 m in a second source |
//!
//! ```text
//! cargo run --release --features probe --example line_symbols_probe -- --arm declared --km 150
//! cargo run --release --features probe --example line_symbols_probe -- --bake-bench
//! ```
//!
//! Flags: `--km N` route length (default 150), `--zoom Z` (15), `--spacing
//! PX` (42: 200 m at z15 near the equator, so `declared` draws the arrows
//! `points` does), `--secs S` pan length (10), `--bearing DEG`, `--pitch
//! DEG`, `--hold S` (no pan: keep the camera still S seconds and print the
//! window's pid, for a screenshot), `--bake-bench` (no window: time the
//! anchor bake and the point generation alone), `--frame-bench` (no window:
//! the push and the per-frame symbol loop of each arm, headless).
//!
//! What it prints, per run: `push_ms` (the synchronous UI-thread push,
//! points generation included for `points` as `gen_ms`), `anchors` and
//! `anchor_bake_ms` (off-thread, push to landed), and over the pan window
//! frame CPU p50/p99, the overlay-symbol phase p50/p99, and draw calls.
//!
//! Synthetic routes, seeded: each a meandering walk at 50 m per vertex
//! inside a 40 x 18 km box over Singapore (3,000 vertices per 150 km, so
//! 30 routes stay under the tiling threshold; `GPUI_MAP_OVERLAY_TILING=always`
//! runs the tiled path).

// Without `probe` only `--bake-bench` is built; the window half is not.
#![cfg_attr(not(feature = "probe"), allow(dead_code, unused_imports))]

use std::time::{Duration, Instant};

use gpui_maps::overlay::{FeatureInput, FeaturePaint, GeometryInput};
use swat_maps_kit::kit;

const ROUTES: usize = 30;
const STEP_M: f64 = 50.0;
const ARROW_EVERY_M: f64 = 200.0;
const CENTER: (f64, f64) = (103.82, 1.35);
const BOX: (f64, f64, f64, f64) = (103.62, 1.27, 104.02, 1.43);
const METRES_PER_DEG: f64 = 111_320.0;

struct Args {
    arm: String,
    km: f64,
    zoom: f64,
    spacing: f64,
    secs: f64,
    bearing: f32,
    pitch: f32,
    hold: Option<f64>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let num = |flag: &str, default: f64| value(flag).and_then(|v| v.parse().ok()).unwrap_or(default);
    if args.iter().any(|a| a == "--bake-bench") {
        bake_bench();
        return;
    }
    if args.iter().any(|a| a == "--frame-bench") {
        frame_bench(num("--zoom", 15.0), num("--spacing", 42.0), num("--bearing", 0.0) as f32, num("--pitch", 0.0) as f32);
        return;
    }
    let parsed = Args {
        arm: value("--arm").unwrap_or_else(|| "declared".into()),
        km: num("--km", 150.0),
        zoom: num("--zoom", 15.0),
        spacing: num("--spacing", 42.0),
        secs: num("--secs", 10.0),
        bearing: num("--bearing", 0.0) as f32,
        pitch: num("--pitch", 0.0) as f32,
        hold: value("--hold").and_then(|v| v.parse().ok()),
    };
    if !["none", "declared", "points"].contains(&parsed.arm.as_str()) {
        eprintln!("--arm must be none, declared or points");
        std::process::exit(2);
    }
    window(parsed);
}

/// A tiny seeded generator: the routes are the same on every run and arm.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// 30 meandering routes of `km` each, `[lon, lat]` every `STEP_M`.
fn routes(km: f64) -> Vec<Vec<[f64; 2]>> {
    let mut rng = Lcg(0x5eed);
    let steps = (km * 1000.0 / STEP_M) as usize;
    (0..ROUTES)
        .map(|_| {
            let mut p = [BOX.0 + (BOX.2 - BOX.0) * rng.next(), BOX.1 + (BOX.3 - BOX.1) * rng.next()];
            let mut heading = rng.next() * std::f64::consts::TAU;
            let mut line = Vec::with_capacity(steps + 1);
            line.push(p);
            for _ in 0..steps {
                heading += (rng.next() - 0.5) * 0.35;
                let d = STEP_M / METRES_PER_DEG;
                let mut next = [p[0] + d * heading.cos(), p[1] + d * heading.sin()];
                if next[0] < BOX.0 || next[0] > BOX.2 {
                    heading = std::f64::consts::PI - heading;
                    next[0] = p[0] + d * heading.cos();
                }
                if next[1] < BOX.1 || next[1] > BOX.3 {
                    heading = -heading;
                    next[1] = p[1] + d * heading.sin();
                }
                p = next;
                line.push(p);
            }
            line
        })
        .collect()
}

/// The consumer's old shape: a point every `ARROW_EVERY_M` along each
/// route, with its screen rotation (`icon-rotate`, clockwise from east,
/// y down) taken from the segment it sits on.
fn arrow_points(routes: &[Vec<[f64; 2]>]) -> Vec<([f64; 2], f32)> {
    let mut out = Vec::new();
    for line in routes {
        let mut next_at = ARROW_EVERY_M / 2.0;
        let mut walked = 0.0;
        for w in line.windows(2) {
            let (dx, dy) = ((w[1][0] - w[0][0]) * METRES_PER_DEG, (w[1][1] - w[0][1]) * METRES_PER_DEG);
            let len = dx.hypot(dy);
            while next_at <= walked + len && len > 0.0 {
                let t = (next_at - walked) / len;
                let at = [w[0][0] + (w[1][0] - w[0][0]) * t, w[0][1] + (w[1][1] - w[0][1]) * t];
                out.push((at, (-dy).atan2(dx).to_degrees() as f32));
                next_at += ARROW_EVERY_M;
            }
            walked += len;
        }
    }
    out
}

fn line_paint() -> FeaturePaint {
    FeaturePaint {
        line_color: gpui_maps::style::Rgba { r: 0.16, g: 0.38, b: 0.86, a: 1.0 },
        line_width: gpui_maps::style::ZoomValue::constant(4.0),
        ..FeaturePaint::default()
    }
}

/// A 16 pt chevron pointing EAST (rotation 0 runs along +x), white with a
/// dark rim, at pixel ratio 2.
fn chevron() -> image::RgbaImage {
    let n = 32u32;
    image::RgbaImage::from_fn(n, n, |x, y| {
        let (px, py) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
        // Two strokes meeting at the tip (8, 0): from (-6, -10) and (-6, 10).
        let seg = |ax: f32, ay: f32, bx: f32, by: f32| {
            let (vx, vy) = (bx - ax, by - ay);
            let t = (((px - ax) * vx + (py - ay) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
            ((px - ax - vx * t).powi(2) + (py - ay - vy * t).powi(2)).sqrt()
        };
        let d = seg(-6.0, -10.0, 8.0, 0.0).min(seg(-6.0, 10.0, 8.0, 0.0));
        if d < 3.0 {
            image::Rgba([255, 255, 255, 255])
        } else if d < 5.0 {
            image::Rgba([20, 30, 60, 255])
        } else {
            image::Rgba([0, 0, 0, 0])
        }
    })
}

fn declaration(spacing: f64) -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({
        "symbol-placement": "line",
        "symbol-spacing": spacing,
        "icon-image": "chevron",
        "icon-allow-overlap": true,
        "icon-ignore-placement": true,
    })
    .as_object()
    .expect("an object")
    .clone()
}

fn quantile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// `--frame-bench`: the push and the per-frame overlay symbol loop of each
/// arm, headless, through `overlay::line_symbols::bench` (no basemap, no
/// GPU: see that module for what it leaves out). 600 frames per arm.
#[cfg(feature = "probe")]
fn frame_bench(zoom: f64, spacing: f64, bearing: f32, pitch: f32) {
    use gpui_maps::overlay::line_symbols::bench;
    let layout = declaration(spacing);
    bench::check_layout(&layout).expect("the declaration compiles");
    println!("frame bench: z{zoom} bearing {bearing} pitch {pitch}, spacing {spacing} px, 600 frames per arm");
    println!("| km | arm | features | push ms | (gen ms) | anchor bake ms | symbol loop p50 us | p99 us | icons/frame |");
    for km in [50.0, 150.0] {
        let lines = routes(km);
        for arm in ["none", "declared", "points"] {
            let cost = bench::run_arm(arm, &lines, &|| arrow_points(&lines), &layout, CENTER, zoom, bearing, pitch, 600);
            println!(
                "| {km} | {arm} | {} | {:.2} | {:.2} | {:.2} | {:.0} | {:.0} | {:.0} |",
                cost.features, cost.push_ms, cost.gen_ms, cost.bake_ms, cost.frame_us_p50, cost.frame_us_p99, cost.icons_per_frame
            );
        }
    }
}

#[cfg(not(feature = "probe"))]
fn frame_bench(_: f64, _: f64, _: f32, _: f32) {
    eprintln!("--frame-bench needs --features probe");
    std::process::exit(2);
}

/// `--bake-bench`: the off-thread anchor bake and the old shape's point
/// generation, timed alone, best of 5.
fn bake_bench() {
    use gpui_maps::overlay::line_symbols::{bake_anchors, LineSymbolLayout, WorldLines};
    let layout = LineSymbolLayout::compile(&declaration(42.0)).expect("compiles");
    for km in [50.0, 150.0] {
        let lines = routes(km);
        let best = |f: &mut dyn FnMut()| {
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    f();
                    t.elapsed().as_secs_f64() * 1e3
                })
                .fold(f64::INFINITY, f64::min)
        };
        let mut points = 0;
        let gen_ms = best(&mut || points = arrow_points(&lines).len());
        let world = WorldLines {
            features: lines
                .iter()
                .map(|l| {
                    vec![l
                        .iter()
                        .map(|p| {
                            let (x, y) = gpui_maps::geo::lon_lat_to_tile_xy(p[0], p[1], 0.0);
                            (x * gpui_maps::geo::TILE_SIZE, y * gpui_maps::geo::TILE_SIZE)
                        })
                        .collect()]
                })
                .collect(),
            vertices: lines.iter().map(Vec::len).sum(),
        };
        println!("km {km}: {} vertices, {points} arrow points (gen {gen_ms:.2} ms)", world.vertices);
        for zoom in [12u8, 14, 15, 16, 18] {
            let mut total = 0;
            let ms = best(&mut || total = bake_anchors(&world, &layout, zoom, &|| false).expect("not cancelled").total);
            println!("  z{zoom}: {total} anchors in {ms:.2} ms");
        }
    }
}

#[cfg(not(feature = "probe"))]
fn window(_: Args) {
    eprintln!("line_symbols_probe's window needs --features probe (the frame probe); --bake-bench does not");
    std::process::exit(2);
}

#[cfg(feature = "probe")]
fn window(args: Args) {
    use gpui::{px, size, App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
    use gpui_maps::map_view::MapView;
    use gpui_maps::render::zoom_probe;

    let style = kit::compile_style("examples/liberty-style.json").expect("compile Liberty");
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime");
    let handle = runtime.handle().clone();
    let mut init = Some(style);
    let prefix = format!("line_symbols.{}.{}km", args.arm, args.km);
    println!(
        "{prefix}: {ROUTES} routes x {} km, z{} bearing {} pitch {}, spacing {} px, tiling {}",
        args.km,
        args.zoom,
        args.bearing,
        args.pitch,
        args.spacing,
        std::env::var("GPUI_MAP_OVERLAY_TILING").unwrap_or_else(|_| "auto".into())
    );

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let mut map = None;
        let (zoom, bearing, pitch) = (args.zoom, args.bearing, args.pitch);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Line symbols probe".into()), ..Default::default() }),
                ..Default::default()
            },
            |_window, cx| {
                let style = init.take().expect("one window");
                let view = cx.new(|_| {
                    let mut view =
                        MapView::new(CENTER.0, CENTER.1, zoom, 1280.0, 800.0, style, kit::openfreemap_planet(), handle);
                    view.jump_to(gpui_maps::camera_control::CameraTarget {
                        pitch: Some(pitch.to_radians()),
                        bearing: Some(bearing.to_radians()),
                        ..Default::default()
                    });
                    view.add_image("chevron", chevron(), 2.0, false);
                    view
                });
                map = Some(view.clone());
                view
            },
        )
        .expect("failed to open window");
        cx.activate(true);
        let map = map.expect("the window was built");

        cx.spawn(async move |cx| {
            let draws = || gpui_maps::render::allocation_stats().draw_calls_total;
            let deadline = Instant::now() + Duration::from_secs(30);
            while draws() == 0 {
                if Instant::now() > deadline {
                    println!("{prefix}: NOTHING PAINTED in 30 s (locked screen or a background window?); void");
                    cx.update(|cx| cx.quit());
                    return;
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
            // Let the basemap's first cover settle before the push.
            cx.background_executor().timer(Duration::from_secs(2)).await;

            let lines = routes(args.km);
            let vertices: usize = lines.iter().map(Vec::len).sum();
            let (push_ms, gen_ms, arrows) = map.update(cx, |view, cx| {
                let started = Instant::now();
                let parts: Vec<[&[[f64; 2]]; 1]> = lines.iter().map(|l| [l.as_slice()]).collect();
                let line_features: Vec<FeatureInput<'_>> = parts
                    .iter()
                    .map(|p| FeatureInput { id: None, geometry: GeometryInput::Line(p), paint: line_paint() })
                    .collect();
                if args.arm == "declared" {
                    view.declare_source_symbols("routes", Some(&declaration(args.spacing))).expect("declares");
                }
                view.set_source_from("routes", &line_features, cx).expect("push routes");
                let mut gen_ms = 0.0;
                let mut arrows = 0;
                if args.arm == "points" {
                    let generated = Instant::now();
                    let points = arrow_points(&lines);
                    let coords: Vec<[[f64; 2]; 1]> = points.iter().map(|(p, _)| [*p]).collect();
                    let features: Vec<FeatureInput<'_>> = points
                        .iter()
                        .zip(&coords)
                        .map(|((_, rotate), c)| FeatureInput {
                            id: None,
                            geometry: GeometryInput::Point(c),
                            paint: FeaturePaint {
                                icon_image: Some("chevron".into()),
                                icon_rotate_deg: *rotate,
                                icon_allow_overlap: true,
                                circle_radius: 0.0,
                                ..FeaturePaint::default()
                            },
                        })
                        .collect();
                    gen_ms = generated.elapsed().as_secs_f64() * 1e3;
                    arrows = features.len();
                    view.set_source_from("arrows", &features, cx).expect("push arrows");
                }
                cx.notify();
                (started.elapsed().as_secs_f64() * 1e3, gen_ms, arrows)
            });
            let pushed = Instant::now();
            println!("{prefix}: {vertices} route vertices; push_ms {push_ms:.2} (of which arrow gen_ms {gen_ms:.2}, {arrows} arrow features)");
            if args.arm == "declared" {
                loop {
                    let landed = map.read_with(cx, |view, _| view.source_line_symbol_anchors("routes"));
                    if let Some((zoom, total)) = landed {
                        println!(
                            "{prefix}: anchors {total} at z{zoom}, landed {:.1} ms after the push (worker + next frame)",
                            pushed.elapsed().as_secs_f64() * 1e3
                        );
                        break;
                    }
                    if pushed.elapsed() > Duration::from_secs(20) {
                        println!("{prefix}: anchors never landed");
                        break;
                    }
                    cx.background_executor().timer(Duration::from_millis(1)).await;
                }
            }
            cx.background_executor().timer(Duration::from_secs(3)).await;

            if let Some(hold) = args.hold {
                println!("{prefix}: HOLD pid {} for {hold} s", std::process::id());
                cx.background_executor().timer(Duration::from_secs_f64(hold)).await;
                println!("{prefix}: draw_calls_total {}", draws());
                cx.update(|cx| cx.quit());
                return;
            }

            // The pan: a slow ellipse about the centre, every 16 ms.
            let gate = kit::PanGate::open();
            zoom_probe::start("line symbols pan");
            let started = Instant::now();
            while started.elapsed().as_secs_f64() < args.secs {
                let t = started.elapsed().as_secs_f64();
                let (lon, lat) = (CENTER.0 + 0.04 * (t * 0.6).sin(), CENTER.1 + 0.02 * (t * 0.45).sin());
                map.update(cx, |view, cx| {
                    view.set_camera(lon, lat, zoom);
                    cx.notify();
                });
                cx.background_executor().timer(Duration::from_millis(16)).await;
            }
            zoom_probe::stop();
            let secs = started.elapsed().as_secs_f64();
            let (_, frames) = zoom_probe::snapshot();
            let cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
            let phase = |name: &str| -> Vec<f64> {
                frames.iter().map(|f| f.phase_ms.iter().filter(|(n, _)| *n == name).map(|(_, ms)| ms).sum()).collect()
            };
            let symbols = phase("overlay_symbols");
            let rows = gate.rows(&prefix, secs, gpui_maps::render::paint_census(), gpui_maps::render::allocation_stats());
            let row = |name: &str| rows.iter().find(|(n, _, _)| n == &format!("{prefix}.{name}")).map_or(0.0, |r| r.1);
            let drawn = row("draw_calls");
            let occluded = row("occluded_paints");
            println!(
                "RESULT {prefix} | push_ms {push_ms:.2} | gen_ms {gen_ms:.2} | frames {} | cpu p50 {:.2} p99 {:.2} ms | overlay_symbols p50 {:.3} p99 {:.3} ms | draw_calls {drawn} | occluded_paints {occluded}{}",
                cpu.len(),
                quantile(&cpu, 0.5),
                quantile(&cpu, 0.99),
                quantile(&symbols, 0.5),
                quantile(&symbols, 0.99),
                if drawn == 0.0 || occluded > 0.0 { " (VOID: nothing painted, or the window was covered)" } else { "" },
            );
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    drop(runtime);
}
