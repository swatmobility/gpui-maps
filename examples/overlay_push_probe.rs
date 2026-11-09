//! Does an overlay push get slower over a session with IDENTICAL content?
//!
//! `docs/superpowers/backlog.md` records "the overlay push degrades 8.6x
//! over 30 seconds" against a 3,000-vehicle scene, and
//! `docs/superpowers/specs/2026-09-02-performance-investigation.md` records
//! that the number was never re-measured after the caches it was taken
//! against were bounded. This is the re-measurement, and it is built so
//! that the two ways such a number goes wrong cannot happen here:
//!
//! - **Per-window means, never cumulative.** A running mean over a session
//!   whose workload grows climbs monotonically for ever with no defect
//!   anywhere. Every figure printed below is one window's own pushes
//!   divided by that window's own count, and the windows are disjoint.
//! - **A payload asserted constant, not assumed constant.** The features
//!   are built ONCE, leaked, and re-pushed by reference; the probe also
//!   fingerprints them and re-checks the fingerprint every window, so a
//!   collection that somehow changed is a failed run rather than a
//!   mysterious trend.
//!
//! It reports the LANDING RATIO alongside the timings, because that is the
//! metric no averaging artifact can produce: `install_overlay_mesh`
//! discards a bake older than the store's generation, so an application
//! pushing faster than it can tessellate has an `overlay_generation` that
//! races and an `overlay_mesh_installs_total` that crawls. The backlog's
//! "bakes landing 100% -> 2%" is that ratio, and it is a count over a
//! count.
//!
//! # What it controls for
//!
//! - **No window, no paint, no GPU.** `Application::headless` gives a real
//!   gpui `App` with a real foreground executor -- so the `cx.spawn`'d
//!   bakes are genuinely polled and `install_overlay_mesh` genuinely runs
//!   -- while `Render::render` is never called. If a degradation shows up
//!   here it is upstream of the GPU entirely; if it does not, the GPU side
//!   is where to look next.
//! - **`EmptyTileSource`.** No network, no tiles, no decode. The overlay
//!   path is the only thing running.
//! - **A pause phase.** Pushing stops for `PROBE_PAUSE_SECS` and resumes.
//!   If the cost resumes where it left off the driver is PUSH COUNT; if it
//!   resumes at the first window's level it is ELAPSED TIME.
//!
//! Run with:
//!   cargo run --release --example overlay_push_probe
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `PROBE_FEATURES` | 3000 | features per push, shaped after the vehicle spike |
//! | `PROBE_POINTS` | 8 | coordinates per feature |
//! | `PROBE_HZ` | 20 | pushes per second |
//! | `PROBE_WINDOW_SECS` | 5 | seconds per reported window |
//! | `PROBE_WINDOWS` | 8 | windows before the pause |
//! | `PROBE_PAUSE_SECS` | 15 | idle seconds, zero pushes |
//! | `PROBE_WINDOWS_AFTER` | 3 | windows after the pause |

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::{FeatureId, FeatureInput, FeaturePaint, GeometryInput};
use gpui_maps::render::AllocationStats;
use gpui_maps::style::Style;
use gpui_maps::tile_source::EmptyTileSource;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// One route: a run of coordinates wandering north-east from a seeded
/// start. Deterministic -- an LCG rather than `rand`, so the probe has no
/// dependency and no seed to forget, and two runs push the same bytes.
fn route(index: usize, points: usize) -> Vec<[f64; 2]> {
    let mut lon = 103.60 + (index % 64) as f64 * 0.005;
    let mut lat = 1.22 + (index / 64) as f64 * 0.004;
    let mut out = Vec::with_capacity(points);
    let mut state = (index as u64).wrapping_mul(6364136223846793005).wrapping_add(1);
    for _ in 0..points {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let jitter = ((state >> 33) as f64 / (1u64 << 31) as f64) - 0.5;
        lon += 0.00035 + jitter * 0.0002;
        lat += 0.00028 + jitter * 0.0002;
        out.push([lon, lat]);
    }
    out
}

/// A cheap order-sensitive digest of every coordinate the probe pushes.
///
/// The whole argument of this harness is "identical content", and the
/// retracted consumer report failed on exactly that point -- their content
/// grew between the samples they compared. Asserting it costs one pass per
/// window over data already in cache, and turns an assumption into a
/// checked fact.
fn fingerprint(coords: &[Vec<[f64; 2]>]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for route in coords {
        for point in route {
            for value in point {
                hash ^= value.to_bits();
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
    }
    hash
}

/// One window's own pushes, divided by that window's own count. Never a
/// running total: see the module doc comment.
struct Window {
    label: String,
    pushes: usize,
    mean_ms: f64,
    p50_ms: f64,
    max_ms: f64,
    /// Generations the store accepted in this window, and overlay meshes
    /// that survived the staleness check in it. Their ratio is the landing
    /// rate.
    generations: u64,
    landed: u64,
    cancelled: u64,
    bakes_in_flight: u64,
    bakes_in_flight_max: u64,
    rss_mb: f64,
    heap_mb: f64,
}

impl Window {
    fn landing_pct(&self) -> f64 {
        if self.generations == 0 {
            0.0
        } else {
            100.0 * self.landed as f64 / self.generations as f64
        }
    }
}

fn main() {
    let features_count = env_usize("PROBE_FEATURES", 3000);
    let points_per_feature = env_usize("PROBE_POINTS", 8);
    let push_hz = env_f64("PROBE_HZ", 20.0);
    let window_secs = env_f64("PROBE_WINDOW_SECS", 5.0);
    let windows_before = env_usize("PROBE_WINDOWS", 8);
    let pause_secs = env_f64("PROBE_PAUSE_SECS", 15.0);
    let windows_after = env_usize("PROBE_WINDOWS_AFTER", 3);

    // Leaked so the borrowed `FeatureInput`s can be `'static` and live
    // inside the spawned loop. A probe process that exits when it is done
    // has nothing to give back, and the leak is the point twice over: the
    // SAME allocation is pushed every time, so nothing about the payload
    // can drift.
    let coords: &'static Vec<Vec<[f64; 2]>> = Box::leak(Box::new(
        (0..features_count).map(|i| route(i, points_per_feature)).collect::<Vec<_>>(),
    ));
    let parts: &'static Vec<[&'static [[f64; 2]]; 1]> =
        Box::leak(Box::new(coords.iter().map(|c| [c.as_slice()]).collect::<Vec<_>>()));
    let features: &'static Vec<FeatureInput<'static>> = Box::leak(Box::new(
        parts
            .iter()
            .enumerate()
            .map(|(i, part)| FeatureInput {
                id: Some(FeatureId::Number(i as f64)),
                geometry: GeometryInput::Line(part),
                paint: FeaturePaint::default(),
            })
            .collect::<Vec<_>>(),
    ));
    let expected = fingerprint(coords);

    let total_coords: usize = coords.iter().map(Vec::len).sum();
    println!(
        "overlay_push_probe: {features_count} features, {total_coords} coordinates, \
         {push_hz} Hz, {windows_before} x {window_secs}s, pause {pause_secs}s, \
         {windows_after} x {window_secs}s after"
    );
    println!("payload fingerprint {expected:#018x} (re-checked every window)\n");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let handle = runtime.handle().clone();

    gpui_platform::headless().run(move |cx: &mut App| {
        let view = cx.new(|_| {
            MapView::new(
                103.86,
                1.29,
                13.0,
                1024.0,
                768.0,
                Style::default(),
                Arc::new(EmptyTileSource) as Arc<dyn gpui_maps::tile_source::TileSource>,
                handle,
            )
        });

        cx.spawn(async move |cx| {
            let interval = Duration::from_secs_f64(1.0 / push_hz);
            let mut report: Vec<Window> = Vec::new();

            for index in 0..windows_before {
                let w =
                    push_for(&view, cx, features, coords, expected, interval, window_secs, index)
                        .await;
                println!("{}", line(&w));
                report.push(w);
            }

            if pause_secs > 0.0 {
                println!("-- pausing {pause_secs}s, zero pushes --");
                let before = gpui_maps::render::allocation_stats();
                cx.background_executor().timer(Duration::from_secs_f64(pause_secs)).await;
                let after = gpui_maps::render::allocation_stats();
                println!(
                    "   across the pause: rss {:.1} -> {:.1} MB, heap {:.1} -> {:.1} MB, \
                     in-flight {} -> {}",
                    before.rss_bytes as f64 / 1e6,
                    after.rss_bytes as f64 / 1e6,
                    before.heap_in_use_bytes as f64 / 1e6,
                    after.heap_in_use_bytes as f64 / 1e6,
                    before.overlay_bakes_in_flight,
                    after.overlay_bakes_in_flight,
                );
            }

            for index in 0..windows_after {
                let w = push_for(
                    &view,
                    cx,
                    features,
                    coords,
                    expected,
                    interval,
                    window_secs,
                    windows_before + index,
                )
                .await;
                println!("{}", line(&w));
                report.push(w);
            }

            summarise(&report, windows_before);
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

fn summarise(report: &[Window], windows_before: usize) {
    let Some(first) = report.first() else { return };
    let Some(last_before) = report.get(windows_before.saturating_sub(1)) else { return };
    println!(
        "\nlast window before the pause / first window: {:.2}x  ({:.3} -> {:.3} ms)",
        if first.mean_ms > 0.0 { last_before.mean_ms / first.mean_ms } else { 0.0 },
        first.mean_ms,
        last_before.mean_ms,
    );
    println!(
        "landing rate first -> last before the pause: {:.1}% -> {:.1}%",
        first.landing_pct(),
        last_before.landing_pct(),
    );
    if let Some(resumed) = report.get(windows_before) {
        let verdict = if resumed.mean_ms >= last_before.mean_ms * 0.9 {
            "resumed where it left off -> the driver is PUSH COUNT, not elapsed time"
        } else if resumed.mean_ms <= first.mean_ms * 1.1 {
            "resumed at the first window's level -> the driver is ELAPSED TIME, and it decays"
        } else {
            "resumed part-way down -> mixed, or the trend is inside the noise"
        };
        println!("first window after the pause: {:.3} ms -- {verdict}", resumed.mean_ms);
    }
}

fn line(w: &Window) -> String {
    format!(
        "{:<10} n {:>5}  mean {:>7.3}  p50 {:>7.3}  max {:>8.3} ms | \
         landed {:>5}/{:<5} {:>5.1}%  cancelled {:>5}  in-flight {:>2} (max {:>2}) | \
         rss {:>7.1}  heap {:>7.1} MB",
        w.label,
        w.pushes,
        w.mean_ms,
        w.p50_ms,
        w.max_ms,
        w.landed,
        w.generations,
        w.landing_pct(),
        w.cancelled,
        w.bakes_in_flight,
        w.bakes_in_flight_max,
        w.rss_mb,
        w.heap_mb,
    )
}

/// Pushes at `interval` for `secs`, timing every call, and returns THAT
/// window's mean -- the counters are read as a delta across the window for
/// the same reason.
#[allow(clippy::too_many_arguments)]
async fn push_for(
    view: &gpui::Entity<MapView>,
    cx: &mut gpui::AsyncApp,
    features: &'static [FeatureInput<'static>],
    coords: &'static [Vec<[f64; 2]>],
    expected: u64,
    interval: Duration,
    secs: f64,
    index: usize,
) -> Window {
    assert_eq!(
        fingerprint(coords),
        expected,
        "the payload changed between windows -- every comparison in this run is void"
    );
    let opened: AllocationStats = gpui_maps::render::allocation_stats();
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let mut samples: Vec<f64> = Vec::new();
    while Instant::now() < deadline {
        let started = Instant::now();
        let outcome = view.update(cx, |view, cx| view.set_features_from(features, cx));
        let elapsed = started.elapsed();
        match outcome {
            Ok(()) => {}
            Err(error) => panic!("the probe's own features must be valid: {error}"),
        }
        samples.push(elapsed.as_secs_f64() * 1e3);
        cx.background_executor().timer(interval).await;
    }
    let closed: AllocationStats = gpui_maps::render::allocation_stats();

    let pushes = samples.len();
    let mean = if pushes == 0 { 0.0 } else { samples.iter().sum::<f64>() / pushes as f64 };
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    let p50 = sorted.get(pushes / 2).copied().unwrap_or(0.0);
    let max = sorted.last().copied().unwrap_or(0.0);

    Window {
        label: format!("window {index}"),
        pushes,
        mean_ms: mean,
        p50_ms: p50,
        max_ms: max,
        generations: closed.overlay_generation.saturating_sub(opened.overlay_generation),
        landed: closed
            .overlay_mesh_installs_total
            .saturating_sub(opened.overlay_mesh_installs_total),
        cancelled: closed
            .overlay_bakes_cancelled_total
            .saturating_sub(opened.overlay_bakes_cancelled_total),
        bakes_in_flight: closed.overlay_bakes_in_flight,
        bakes_in_flight_max: closed.overlay_bakes_in_flight_max,
        rss_mb: closed.rss_bytes as f64 / 1e6,
        heap_mb: closed.heap_in_use_bytes as f64 / 1e6,
    }
}
