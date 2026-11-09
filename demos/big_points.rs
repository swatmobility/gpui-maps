//! Millions of points as one overlay source: clusters at low zoom, every
//! point at high zoom.
//!
//! See `docs/superpowers/specs/2026-09-26-big-point-sources-design.md`.
//!
//! The data is SYNTHETIC and seeded, so every run draws the same points and
//! nothing is fetched or committed: 40 US "cities" (Gaussian cores whose
//! sizes follow a Zipf curve) plus a sparse rural spread over the
//! contiguous 48, each point carrying a `category` (0-4) and a `value`.
//!
//! ```text
//! cargo run --release --example big_points -- [--points N] [--seed S] [--no-cluster]
//!     [--grid | --h3] [--measure] [--zoom-sweep] [--quit-after SECS]
//! ```
//!
//! - Windowed (default): clusters coloured and sized by `point_count`; a
//!   click on a cluster eases to its expansion zoom; a click on a point
//!   prints its properties.
//! - `--measure`: no window. Pushes into a bare `OverlayStore`, builds the
//!   index, builds and tessellates the view at fixed cameras and at every
//!   integer zoom over one city, times the cluster queries, and prints
//!   `gate: points.*` lines when `US_BG_GATE` is set (the perf gate's second
//!   section, `examples/perf_gate.rs`).
//! - `--zoom-sweep` (needs `--features probe`): z3 -> z16 -> z3 over the
//!   biggest city, twice slow and twice flicked, with CPU frame times and
//!   the point path's hole frames.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{px, size, App, Bounds, Entity, TitlebarOptions, WeakEntity, WindowBounds, WindowOptions};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::points::{self, ViewKey};
use gpui_maps::overlay::{ClusterMode, ClusterOptions, OverlayStore, PropertyColumn};
use gpui_maps::render::allocation_stats;

// The windowed root, point generation, clustering options and both paints
// all live in this panel (`kit/src/panels/big_points.rs`), the one copy;
// `windowed` and `measure` both reach it from this crate root.
use swat_maps_kit::kit;
use swat_maps_kit::shell;
use swat_maps_kit::panels::big_points as panel;

use panel::{SOURCE, START};

struct Args {
    points: usize,
    seed: u64,
    cluster: bool,
    grid: bool,
    h3: bool,
    measure: bool,
    zoom_sweep: bool,
    quit_after: Option<f64>,
}

fn parse_args() -> Args {
    let mut a = Args {
        points: 2_000_000,
        seed: 1,
        cluster: true,
        grid: false,
        h3: false,
        measure: false,
        zoom_sweep: false,
        quit_after: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--points" => a.points = it.next().and_then(|s| s.parse().ok()).expect("--points takes a count"),
            "--seed" => a.seed = it.next().and_then(|s| s.parse().ok()).expect("--seed takes a number"),
            "--no-cluster" => a.cluster = false,
            "--grid" => a.grid = true,
            "--h3" => a.h3 = true,
            "--measure" => a.measure = true,
            "--zoom-sweep" => a.zoom_sweep = true,
            "--quit-after" => a.quit_after = it.next().and_then(|s| s.parse().ok()),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// Point generation, clustering options and both paints all live in
/// `kit/src/panels/big_points.rs` (`generate`, `cluster_options`,
/// `point_paint`, `cluster_paint`, `biggest_city`, marked `pub(crate)`
/// there): ONE copy, reached from this crate root the same way `windowed`
/// already reaches `panel::Options`/`panel::build_with`, so `measure`
/// below and the windowed/gallery path can never silently draw or measure
/// different clustering or paint.
use panel::{biggest_city, cluster_options, cluster_paint, generate, point_paint};

/// `windowed`'s point generation and clustering now live in
/// `kit/src/panels/big_points.rs`, self-contained (its own `Options`
/// carries no `grid`/`h3` booleans, just the `ClusterMode` they pick).
impl From<&Args> for panel::Options {
    fn from(args: &Args) -> Self {
        panel::Options {
            points: args.points,
            seed: args.seed,
            cluster: args.cluster,
            mode: if args.h3 {
                ClusterMode::h3_for(50.0, 14)
            } else if args.grid {
                ClusterMode::Grid
            } else {
                ClusterMode::Radius
            },
        }
    }
}

fn gate(name: &str, value: f64, unit: &str) {
    if std::env::var_os("US_BG_GATE").is_some() {
        println!("gate: {name} {value} {unit}");
    }
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1e6
}

fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the struct it is given and nothing else.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage.ru_maxrss as u64
}

/// Why a gate run (`US_BG_GATE`) must not go ahead, if it must not: the
/// perf gate builds every demo with `--features probe`, and a binary
/// without it (its `points.*` rows need no probe, but its `--zoom-sweep` does) is not the one the gate built -- a sibling build
/// re-pointing the shared `release/examples` link, say. Refusing makes
/// that an exit code instead of rows quietly MISSING (see
/// `us_block_groups`'s `gate_refusal`).
fn gate_refusal(gate: bool) -> Option<&'static str> {
    (gate && !cfg!(feature = "probe")).then_some(
        "GATE ERROR: US_BG_GATE is set but this binary was built without --features probe; \
         rebuild with --features probe (scripts/perf-gate.sh does).",
    )
}

fn main() {
    let args = parse_args();
    if let Some(refusal) = gate_refusal(std::env::var_os("US_BG_GATE").is_some()) {
        // stdout: perf_gate discards stderr, and the log is what a human reads.
        println!("{refusal}");
        std::process::exit(2);
    }
    gpui_maps::render::note_process_start();
    if args.measure {
        measure(&args);
    } else {
        windowed(args);
    }
}

/// The fixed cameras `--measure` builds a view at, 1280x800.
fn cameras(biggest: (f64, f64)) -> Vec<(&'static str, (f64, f64, f64))> {
    vec![
        ("national_z3", START),
        ("city_z8", (biggest.0, biggest.1, 8.0)),
        ("city_z11", (biggest.0, biggest.1, 11.0)),
        ("city_z14", (biggest.0, biggest.1, 14.0)),
        ("city_z17", (biggest.0, biggest.1, 17.0)),
    ]
}

fn view_key(camera: (f64, f64, f64), grow: f64) -> ViewKey {
    let (min, max) = points::flat_viewport_box(camera.0, camera.1, camera.2, 1280.0, 800.0);
    let (gw, gh) = ((max[0] - min[0]) * grow, (max[1] - min[1]) * grow);
    ViewKey::for_box(camera.2.floor() as u8, [min[0] - gw, min[1] - gh], [max[0] + gw, max[1] + gh], 0)
}

fn measure(args: &Args) {
    let mode = match (args.cluster, args.grid, args.h3) {
        (false, _, _) => "points (grid level of detail)",
        (true, _, true) => "cluster (h3)",
        (true, true, false) => "cluster (grid)",
        (true, false, false) => "cluster (radius)",
    };
    println!("big_points --measure: {} points, seed {}, mode {mode}", args.points, args.seed);
    let rss0 = allocation_stats().rss_bytes;
    let started = Instant::now();
    let data = generate(args.points, args.seed);
    println!("generate: {:.0} ms", started.elapsed().as_secs_f64() * 1e3);

    let mut store = OverlayStore::new();
    store
        .set_source_cluster_options(SOURCE, Some(cluster_options(args.cluster, panel::Options::from(args).mode)), 0)
        .expect("valid options");
    store.declare_source_paint(SOURCE, &cluster_paint()).expect("valid paint");
    let paint = point_paint();
    let started = Instant::now();
    store
        .set_source_points(
            SOURCE,
            &data.lon_lat,
            None,
            &paint,
            &[("category", PropertyColumn::Number(&data.category)), ("value", PropertyColumn::Number(&data.value))],
        )
        .expect("a valid push");
    let push_ms = started.elapsed().as_secs_f64() * 1e3;
    let biggest = biggest_city(args.seed);
    drop(data);

    let point_data = store.source_point_data(SOURCE).expect("pushed");
    let started = Instant::now();
    let (index, phases) = points::build_index_timed(&point_data);
    let index = Arc::new(index);
    let build_ms = started.elapsed().as_secs_f64() * 1e3;
    println!(
        "index phases: project {:.1} ms | sort {:.1} ms | levels {:.1} ms | trees {:.1} ms ({} threads)",
        phases.project_ms,
        phases.sort_ms,
        phases.levels_ms,
        phases.trees_ms,
        std::thread::available_parallelism().map_or(1, |n| n.get())
    );
    let adopted = store.adopt_point_index(SOURCE, point_data.revision(), Arc::clone(&index));
    assert!(adopted, "the index was built for the current push");
    let levels = index.level_sizes();
    println!(
        "push {push_ms:.1} ms | index build {build_ms:.1} ms | data {:.1} MB, index {:.1} MB | levels {}",
        mb(point_data.bytes()),
        mb(index.bytes()),
        levels.iter().map(|(z, n)| format!("z{z}:{n}")).collect::<Vec<_>>().join(" ")
    );
    let rss_at_rest = allocation_stats().rss_bytes;
    println!(
        "memory: store resident {:.1} MB | rss {:.1} MB (+{:.1} MB since start)",
        mb(store.resident_bytes()),
        rss_at_rest as f64 / 1e6,
        rss_at_rest.saturating_sub(rss0) as f64 / 1e6
    );
    gate("points.push_ms", push_ms, "ms");
    gate("points.index_build_ms", build_ms, "ms");
    gate("points.data_mb", mb(point_data.bytes()), "MB");
    gate("points.index_mb", mb(index.bytes()), "MB");
    gate("points.store_resident_mb", mb(store.resident_bytes()), "MB");
    gate("points.rss_at_rest_mb", rss_at_rest as f64 / 1e6, "MB");
    gate("points.index_project_ms", phases.project_ms, "ms");
    gate("points.index_sort_ms", phases.sort_ms, "ms");
    gate("points.index_levels_ms", phases.levels_ms, "ms");

    // The other two index builds on the same 2 M points, for the table the
    // gate keeps: the grid roll-up and the non-clustered level of detail.
    #[allow(unused_mut)]
    let mut others = vec![
        ("grid", ClusterOptions { mode: ClusterMode::Grid, ..cluster_options(true, ClusterMode::Radius) }),
        ("lod", ClusterOptions::points_only()),
    ];
    // Built with `--features h3`: the H3 roll-up on the same points too
    // (reported, not graded -- the gate builds without the feature).
    #[cfg(feature = "h3")]
    others.push(("h3", cluster_options(true, ClusterMode::h3_for(50.0, 14))));
    for (label, other) in others {
        let started = Instant::now();
        let (built, phases) = points::build_index_with(&point_data, &other);
        let ms = started.elapsed().as_secs_f64() * 1e3;
        println!(
            "index build ({label}): {ms:.1} ms (project {:.1}, sort {:.1}, levels {:.1}, trees {:.1}) | {:.1} MB",
            phases.project_ms,
            phases.sort_ms,
            phases.levels_ms,
            phases.trees_ms,
            mb(built.bytes())
        );
        if label != "h3" {
            gate(&format!("points.{label}_index_build_ms"), ms, "ms");
            gate(&format!("points.{label}_index_mb"), mb(built.bytes()), "MB");
        }
    }

    let name: gpui_maps::overlay::SourceId = SOURCE.into();
    let paint_decl = points::SourcePaintHandle::compile(&cluster_paint()).expect("valid paint");
    for (label, camera) in cameras(biggest) {
        let key = view_key(camera, 0.5);
        let started = Instant::now();
        let view = points::build_view(&name, &point_data, &index, paint_decl.paint(), key);
        let build = started.elapsed().as_secs_f64() * 1e3;
        let started = Instant::now();
        let (instances, upload) = points::probe_upload(&view);
        let tess = started.elapsed().as_secs_f64() * 1e3;
        println!(
            "view {label:<12} z{:<2} {:>7} features ({:>5} clusters) | build {build:>6.1} ms, mesh {tess:>6.1} ms | {instances} circle instances, upload {:.2} MB",
            key.z,
            view.len(),
            view.clusters,
            mb(upload)
        );
        gate(&format!("points.view.{label}.features"), view.len() as f64, "count");
        gate(&format!("points.view.{label}.instances"), instances as f64, "count");
        gate(&format!("points.view.{label}.upload_mb"), mb(upload), "MB");
    }
    // Every integer zoom over the biggest city: what a zoom change costs
    // the worker (the UI thread only swaps the result in).
    let (mut worst, mut worst_z, mut worst_len) = (0.0f64, 0u8, 0usize);
    for z in 0..=18u8 {
        let key = view_key((biggest.0, biggest.1, f64::from(z) + 0.5), 0.5);
        let started = Instant::now();
        let view = points::build_view(&name, &point_data, &index, paint_decl.paint(), key);
        let (_, _) = points::probe_upload(&view);
        let ms = started.elapsed().as_secs_f64() * 1e3;
        if ms > worst {
            (worst, worst_z, worst_len) = (ms, z, view.len());
        }
    }
    println!("zoom change z0..z18 over the biggest city: worst view build (instances included) {worst:.1} ms at z{worst_z} ({worst_len} features)");
    gate("points.zoom_change_worst_ms", worst, "ms");

    if args.cluster {
        // The biggest cluster at z5: its expansion zoom and its leaves.
        let clusters = index.clusters([-125.0, 24.0, -66.0, 50.0], 5.0);
        let biggest_cluster = clusters
            .iter()
            .filter_map(|c| match c {
                gpui_maps::overlay::ClusterItem::Cluster { id, count, .. } => Some((*id, *count)),
                _ => None,
            })
            .max_by_key(|c| c.1);
        if let Some((id, count)) = biggest_cluster {
            let started = Instant::now();
            let expansion = store.cluster_expansion_zoom(SOURCE, id);
            let expansion_us = started.elapsed().as_secs_f64() * 1e6;
            let started = Instant::now();
            let leaves = store.cluster_leaves(SOURCE, id, 100, 0).map_or(0, |l| l.len());
            let leaves_us = started.elapsed().as_secs_f64() * 1e6;
            println!(
                "cluster queries: biggest z5 cluster {id} ({count} points) expands at z{} ({expansion_us:.0} us), 100 leaves {leaves} ({leaves_us:.0} us)",
                expansion.unwrap_or(0)
            );
        }
    }
    println!("peak rss {:.1} MB", peak_rss_bytes() as f64 / 1e6);
    gate("points.peak_rss_mb", peak_rss_bytes() as f64 / 1e6, "MB");
}

fn windowed(args: Args) {
    println!("Drag to pan, scroll to zoom; click a cluster to zoom into it. {} synthetic points.", args.points);
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime");
    let handle = runtime.handle().clone();
    let quit_after = args.quit_after;
    let sweep = args.zoom_sweep.then(|| biggest_city(args.seed));
    // The panel draws keyless Graphite, token or not (the `--basemap`
    // flag that picked Mapbox Light or Positron went with it, 2026-10-22).
    let env = kit::PanelEnv::from_process(kit::Host::Standalone { title: "Big points".into() }, handle, (1280.0, 800.0));
    let opts = panel::Options::from(&args);

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let mut map_entity: Option<WeakEntity<MapView>> = None;
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Big points".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let built = panel::build_with(&env, opts, window, cx).expect("big_points panel");
                map_entity = built.map.clone();
                built
                    .view
                    .downcast::<shell::FocusedMap>()
                    .unwrap_or_else(|_| panic!("big_points's panel root is a FocusedMap"))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
        let map = map_entity.expect("the window was built").upgrade().expect("the window is still open");
        if let Some(at) = sweep {
            zoom_sweep_task(map.clone(), at, cx);
        }
        cx.spawn(async move |cx| {
            let started = Instant::now();
            let mut reported_index = false;
            let mut last_paints = gpui_maps::render::paint_census().paints_submitted;
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let t = started.elapsed();
                let stats = map.read_with(cx, |view, _| view.point_source_stats(SOURCE));
                if let Some(s) = stats.filter(|s| s.index_bytes > 0 && !reported_index) {
                    reported_index = true;
                    println!(
                        "[{:>5.1}s] index landed: {} points, build {:.0} ms off-thread, data {:.1} MB, index {:.1} MB",
                        t.as_secs_f64(),
                        s.points,
                        s.index_build_ms,
                        mb(s.data_bytes),
                        mb(s.index_bytes)
                    );
                }
                if t.as_secs() / 5 != (t - Duration::from_millis(250)).as_secs() / 5 {
                    let census = gpui_maps::render::paint_census();
                    let fps = (census.paints_submitted - last_paints) as f64 / 5.0;
                    last_paints = census.paints_submitted;
                    let a = allocation_stats();
                    if let Some(s) = stats {
                        println!(
                            "[{:>5.1}s] {fps:.1} fps | view {} features ({} clusters), {} views, last build {:.1} ms, max {:.1} ms | hole frames {} | rss {:.0} MB, gpu {:.0} MB",
                            t.as_secs_f64(),
                            s.view_features,
                            s.view_clusters,
                            s.views,
                            s.last_view_build_ms,
                            s.max_view_build_ms,
                            s.hole_frames,
                            a.rss_bytes as f64 / 1e6,
                            a.gpu_buffer_bytes_live as f64 / 1e6
                        );
                    }
                }
                if quit_after.is_some_and(|q| t.as_secs_f64() >= q) {
                    println!("quit after {:.0}s; peak rss {:.0} MB", t.as_secs_f64(), peak_rss_bytes() as f64 / 1e6);
                    cx.update(|cx| cx.quit());
                    return;
                }
            }
        })
        .detach();
    });
    std::mem::forget(runtime);
}

/// `--zoom-sweep`: z3 -> z16 -> z3 over `at`, twice slow (4 s) and twice
/// flicked (0.6 s), each followed by a 2 s settle; CPU frame times from the
/// zoom probe, and the point path's hole frames and view builds.
#[cfg(feature = "probe")]
fn zoom_sweep_task(map: Entity<MapView>, at: (f64, f64), cx: &mut App) {
    use gpui_maps::render::zoom_probe;
    cx.spawn(async move |cx| {
        let tick = Duration::from_millis(16);
        // Bounded wait for the index and the first view.
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let ready = map.read_with(cx, |v, _| v.point_source_stats(SOURCE).is_some_and(|s| s.views > 0));
            if ready {
                break;
            }
            if Instant::now() > deadline {
                println!("zoom sweep: no point view within 120 s; giving up");
                cx.update(|cx| cx.quit());
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
        let park = Instant::now();
        while park.elapsed() < Duration::from_secs(3) {
            if !set(cx, 3.0) {
                return;
            }
            cx.background_executor().timer(tick).await;
        }
        let before = map.read_with(cx, |v, _| v.point_source_stats(SOURCE)).unwrap_or_default();
        let occluded_before = gpui_maps::render::paint_census().paints_skipped_occluded;
        let draws_before = allocation_stats().draw_calls_total;
        zoom_probe::start("parked z3");
        let legs: [(&str, Option<(f64, f64)>, f64); 9] = [
            ("parked z3", None, 1.0),
            ("slow in (z3->z16, 4 s)", Some((3.0, 16.0)), 4.0),
            ("settle z16", None, 2.0),
            ("slow out (z16->z3, 4 s)", Some((16.0, 3.0)), 4.0),
            ("settle z3", None, 2.0),
            ("flick in (0.6 s)", Some((3.0, 16.0)), 0.6),
            ("settle z16 #2", None, 2.0),
            ("flick out (0.6 s)", Some((16.0, 3.0)), 0.6),
            ("settle z3 #2", None, 2.0),
        ];
        let mut z = 3.0;
        for (i, (label, ramp, secs)) in legs.into_iter().enumerate() {
            if i > 0 {
                zoom_probe::mark(label);
            }
            cx.update(|cx| cx.activate(true));
            let started = Instant::now();
            loop {
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
        let (_, frames) = zoom_probe::snapshot();
        let mut cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
        cpu.sort_by(f64::total_cmp);
        let q = |p: f64| if cpu.is_empty() { f64::NAN } else { cpu[((cpu.len() - 1) as f64 * p).round() as usize] };
        let over = cpu.iter().filter(|&&ms| ms > 16.7).count();
        let after = map.read_with(cx, |v, _| v.point_source_stats(SOURCE)).unwrap_or_default();
        let a = allocation_stats();
        println!("{}", zoom_probe::report());
        println!(
            "-- point sweep over ({:.2}, {:.2}): {} frames, cpu p50 {:.2} p99 {:.2} max {:.2} ms, {over} over 16.7 ms | point hole frames {} stale frames {} stand-in frames {} floor frames {} | views built {} (max build {:.1} ms, max upload {:.2} MB), stand-ins {} | draws {} | occluded {} | rss {:.0} MB peak {:.0} MB --",
            at.0,
            at.1,
            cpu.len(),
            q(0.5),
            q(0.99),
            q(1.0),
            after.hole_frames - before.hole_frames,
            after.stale_frames - before.stale_frames,
            after.standin_frames - before.standin_frames,
            after.floor_frames - before.floor_frames,
            after.views - before.views,
            after.max_view_build_ms,
            mb(after.max_view_upload_bytes),
            after.standins - before.standins,
            a.draw_calls_total - draws_before,
            gpui_maps::render::paint_census().paints_skipped_occluded - occluded_before,
            a.rss_bytes as f64 / 1e6,
            peak_rss_bytes() as f64 / 1e6
        );
        cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(not(feature = "probe"))]
fn zoom_sweep_task(_: Entity<MapView>, _: (f64, f64), _: &mut App) {
    panic!("--zoom-sweep needs --features probe");
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
}
