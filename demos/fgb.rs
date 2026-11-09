//! A million-feature FlatGeobuf as one overlay source: the whole-file read
//! (`overlay::fgb::FgbSource::from_path`), the push onto the point path
//! (`OverlayStore::set_source_fgb`), and bbox queries through the file's
//! packed Hilbert R-tree -- timed and sized, headless. The perf gate's
//! `fgb.*` section.
//!
//! ```text
//! cargo run --release --example fgb [-- PATH|URL]          # a window
//! cargo run --release --example fgb -- --measure [--features N] [--seed S]
//! cargo run --release --example fgb -- --write /tmp/points.fgb [--features N]
//! ```
//!
//! With no flag it opens the Studio's "FlatGeobuf" panel in a window: the
//! bundled world cities and the remote US counties (streamed by viewport),
//! or the file at `PATH|URL` (`GPUI_MAP_FGB` works too).
//!
//! The data is SYNTHETIC and seeded (nothing fetched or committed): `id`,
//! `category` (0-4), `value` and `name` (a string), the points clustered
//! around 40 cities like `demos/csv_points.rs`'s, written with
//! `overlay::fgb::write` (node size 16) to the system temp directory and
//! deleted afterwards. The bbox queries are warm (the file was just
//! written, so it is in the page cache): the steady state of a panned map
//! reading a local file.

use std::time::Instant;

use gpui_maps::overlay::fgb::write::{write, WriteFeature, WriteType, WriteValue};
use gpui_maps::overlay::fgb::{FgbOptions, FgbSource};
use gpui_maps::overlay::{FeaturePaint, OverlayGeometry, OverlayStore};
use gpui_maps::render::allocation_stats;

use swat_maps_kit::kit;
use swat_maps_kit::panels::fgb as panel;

struct Args {
    features: usize,
    seed: u64,
    measure: bool,
    write: Option<String>,
    open: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args { features: 1_000_000, seed: 1, measure: false, write: None, open: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--features" => a.features = it.next().and_then(|s| s.parse().ok()).expect("--features takes a count"),
            "--seed" => a.seed = it.next().and_then(|s| s.parse().ok()).expect("--seed takes a number"),
            "--measure" => a.measure = true,
            "--write" => a.write = Some(it.next().expect("--write takes a path")),
            other if !other.starts_with("--") && a.open.is_none() => a.open = Some(other.to_string()),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// splitmix64 -> uniform in [0, 1).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.next().max(1e-12), self.next());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

const COLUMNS: &[(&str, WriteType)] =
    &[("id", WriteType::Long), ("category", WriteType::Int), ("value", WriteType::Double), ("name", WriteType::String)];

/// The synthetic file: `n` points over the contiguous US.
fn generate(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed);
    let cities: Vec<(f64, f64, f64)> = (0..40)
        .map(|i| (-122.0 + r.next() * 50.0, 27.0 + r.next() * 20.0, 0.04 + 0.25 / (1.0 + i as f64).sqrt()))
        .collect();
    let features: Vec<WriteFeature> = (0..n)
        .map(|i| {
            let (lon, lat) = if r.next() < 0.1 {
                (-124.0 + r.next() * 57.0, 25.0 + r.next() * 24.0)
            } else {
                let (lon, lat, s) = cities[(r.next() * r.next() * cities.len() as f64) as usize];
                (lon + r.gauss() * s / lat.to_radians().cos(), lat + r.gauss() * s)
            };
            WriteFeature {
                geometry: OverlayGeometry::Point(vec![(lon.clamp(-180.0, 180.0), lat.clamp(-90.0, 90.0))]),
                properties: vec![
                    WriteValue::Long(i as i64),
                    WriteValue::Int((r.next() * 5.0) as i32),
                    WriteValue::Double((-r.next().max(1e-12).ln() * 100.0).round()),
                    WriteValue::String(format!("site {i}")),
                ],
            }
        })
        .collect();
    write(COLUMNS, &features, 16)
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

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn measure(args: &Args) {
    println!("fgb --measure: {} features, seed {}", args.features, args.seed);
    let started = Instant::now();
    let bytes = generate(args.features, args.seed);
    println!("generate + write: {:.0} ms, {:.1} MB of FlatGeobuf", started.elapsed().as_secs_f64() * 1e3, mb(bytes.len()));
    let path = tempfile::Builder::new().prefix("gpui-map-fgb-measure-").suffix(".fgb").tempfile().expect("temp").into_temp_path();
    std::fs::write(&path, &bytes).expect("write the file");
    let file_bytes = bytes.len();
    drop(bytes);
    gate("fgb.file_mb", mb(file_bytes), "MB");
    let rss_before = allocation_stats().rss_bytes;

    // Best of 3: the read is single-threaded and allocation-heavy, so the
    // first run also pays the allocator's page faults.
    let mut best = f64::MAX;
    let mut fgb = None;
    for _ in 0..3 {
        drop(fgb.take());
        let started = Instant::now();
        let read = FgbSource::from_path(&path, &FgbOptions::default()).expect("the synthetic file reads");
        best = best.min(started.elapsed().as_secs_f64() * 1e3);
        fgb = Some(read);
    }
    let fgb = fgb.expect("read");
    let report = fgb.report();
    println!("load: {best:.1} ms best of 3 ({:.0} MB/s) | {report}", mb(file_bytes) / (best / 1e3));
    let resident = fgb.resident_bytes();
    println!(
        "loaded source: {:.1} MB resident | rss +{:.1} MB",
        mb(resident),
        allocation_stats().rss_bytes.saturating_sub(rss_before) as f64 / 1e6
    );
    gate("fgb.load_ms", best, "ms");
    gate("fgb.features_loaded", report.features_loaded as f64, "count");
    gate("fgb.source_mb", mb(resident), "MB");

    let mut store = OverlayStore::new();
    let started = Instant::now();
    store.set_source_fgb("fgb", &fgb, &FeaturePaint::default(), 0).expect("a valid push");
    let push_ms = started.elapsed().as_secs_f64() * 1e3;
    drop(fgb);
    println!("push {push_ms:.1} ms | store resident {:.1} MB", mb(store.resident_bytes()));
    gate("fgb.push_ms", push_ms, "ms");
    drop(store);

    // Viewport queries through the index, by seek: a city view (~1 degree)
    // and a regional one (~8 degrees), 100 of each at seeded centres.
    let mut r = Rng(args.seed ^ 0x5eed);
    for (label, size) in [("city", 1.0), ("region", 8.0)] {
        let mut times = Vec::new();
        let mut hits = 0usize;
        for _ in 0..100 {
            let (lon, lat) = (-120.0 + r.next() * 45.0, 28.0 + r.next() * 18.0);
            let bbox = [lon - size / 2.0, lat - size * 0.3, lon + size / 2.0, lat + size * 0.3];
            let started = Instant::now();
            let got = FgbSource::from_path(&path, &FgbOptions::default().with_bbox(bbox)).expect("a query");
            times.push(started.elapsed().as_secs_f64() * 1e6);
            hits += got.len();
        }
        times.sort_by(f64::total_cmp);
        let (p50, p95) = (percentile(&times, 0.5), percentile(&times, 0.95));
        println!("bbox query {label}: p50 {p50:.0} us, p95 {p95:.0} us, {:.0} features per query", hits as f64 / 100.0);
        gate(&format!("fgb.query.{label}.p50_us"), p50.round(), "us");
        gate(&format!("fgb.query.{label}.p95_us"), p95.round(), "us");
    }
    // One fixed box, counted: the index must find exactly this many.
    let fixed = FgbSource::from_path(&path, &FgbOptions::default().with_bbox([-100.0, 35.0, -90.0, 42.0])).expect("a query");
    println!("fixed box: {} features, {} bytes read in {} reads", fixed.len(), fixed.report().bytes_read, fixed.report().requests);
    gate("fgb.query.fixed.features", fixed.len() as f64, "count");
    println!("peak rss {:.1} MB", peak_rss_bytes() as f64 / 1e6);
    gate("fgb.peak_rss_mb", peak_rss_bytes() as f64 / 1e6, "MB");
}

fn main() {
    let args = parse_args();
    if let Some(path) = &args.write {
        let bytes = generate(args.features, args.seed);
        std::fs::write(path, &bytes).expect("write the file");
        println!("wrote {} features ({:.1} MB) to {path}", args.features, mb(bytes.len()));
        return;
    }
    if args.measure {
        measure(&args);
        return;
    }
    windowed(args.open);
}

fn windowed(open: Option<String>) {
    use gpui::{px, size, App, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime");
    let env = kit::PanelEnv::from_process(kit::Host::Standalone { title: "FlatGeobuf".into() }, runtime.handle().clone(), (1280.0, 800.0));
    let input = open.map(|location| panel::input_for(&location)).or_else(panel::input_from_env);
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("FlatGeobuf".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let built = panel::build_with(&env, input, window, cx).expect("fgb panel");
                built.view.downcast::<panel::FgbRoot>().unwrap_or_else(|_| panic!("the panel root is a FgbRoot"))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
    drop(runtime);
}
