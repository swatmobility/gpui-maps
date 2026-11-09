//! A million-row CSV as one overlay source: the parse
//! (`overlay::csv::CsvSource::from_bytes`), the push onto the point path
//! (`OverlayStore::set_source_csv`), and the off-thread index the point path
//! builds -- timed and sized, headless. The perf gate's `csv.*` section.
//!
//! ```text
//! cargo run --release --example csv_points [-- PATH|URL]      # a window
//! cargo run --release --example csv_points -- --measure [--rows N] [--seed S]
//! cargo run --release --example csv_points -- --write /tmp/points.csv [--rows N]
//! ```
//!
//! With no flag it opens the Studio's "CSV points" panel in a window: the
//! bundled world cities, or the CSV at `PATH|URL` (loaded off the UI
//! thread; `--write` a million rows first to watch that one draw).
//!
//! The data is SYNTHETIC and seeded (nothing fetched or committed): `id`,
//! `lat`, `lon`, `category` (0-4), `value`, `name` (a string) and `flag`
//! (a bool), the positions clustered around 40 cities like
//! `demos/big_points.rs`'s, every 1000th row deliberately bad (a missing
//! longitude) so the skip path is on the clock too. `--write` saves the
//! same file for a windowed check (the Studio's "CSV points" panel opens a
//! path or URL).

use std::sync::Arc;
use std::time::Instant;

use gpui_maps::overlay::csv::{CsvOptions, CsvSource};
use gpui_maps::overlay::points::{self, ViewKey};
use gpui_maps::overlay::{FeaturePaint, OverlayStore};
use gpui_maps::render::allocation_stats;

use swat_maps_kit::kit;
use swat_maps_kit::panels::csv_points as panel;

struct Args {
    rows: usize,
    seed: u64,
    measure: bool,
    write: Option<String>,
    open: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args { rows: 1_000_000, seed: 1, measure: false, write: None, open: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rows" => a.rows = it.next().and_then(|s| s.parse().ok()).expect("--rows takes a count"),
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

/// The synthetic CSV: `rows` data rows, every 1000th one bad.
fn generate(rows: usize, seed: u64) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut r = Rng(seed);
    let cities: Vec<(f64, f64, f64)> = (0..40)
        .map(|i| (-122.0 + r.next() * 50.0, 27.0 + r.next() * 20.0, 0.04 + 0.25 / (1.0 + i as f64).sqrt()))
        .collect();
    let mut out = String::with_capacity(rows * 64);
    out.push_str("id,lat,lon,category,value,name,flag\n");
    for i in 0..rows {
        let (lon, lat) = if r.next() < 0.1 {
            (-124.0 + r.next() * 57.0, 25.0 + r.next() * 24.0)
        } else {
            let (lon, lat, s) = cities[(r.next() * r.next() * cities.len() as f64) as usize];
            (lon + r.gauss() * s / lat.to_radians().cos(), lat + r.gauss() * s)
        };
        let category = (r.next() * 5.0).floor();
        let value = (-r.next().max(1e-12).ln() * 100.0).round();
        let flag = r.next() < 0.5;
        if i % 1000 == 999 {
            let _ = writeln!(out, "{i},{lat:.6},,{category},{value},\"site {i}, bad\",{flag}");
        } else {
            let _ = writeln!(out, "{i},{lat:.6},{lon:.6},{category},{value},site {i},{flag}");
        }
    }
    out.into_bytes()
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

fn measure(args: &Args) {
    println!("csv_points --measure: {} rows, seed {}", args.rows, args.seed);
    let started = Instant::now();
    let bytes = generate(args.rows, args.seed);
    println!("generate: {:.0} ms, {:.1} MB of CSV", started.elapsed().as_secs_f64() * 1e3, mb(bytes.len()));
    let rss_before = allocation_stats().rss_bytes;

    // Best of 3: the parse is single-threaded and allocation-heavy, so the
    // first run also pays the allocator's page faults.
    let mut best = f64::MAX;
    let mut csv = None;
    for _ in 0..3 {
        drop(csv.take());
        let started = Instant::now();
        let parsed = CsvSource::from_bytes(&bytes, &CsvOptions::default()).expect("the synthetic CSV parses");
        best = best.min(started.elapsed().as_secs_f64() * 1e3);
        csv = Some(parsed);
    }
    let csv = csv.expect("parsed");
    let report = csv.report();
    println!("parse: {best:.1} ms best of 3 ({:.0} MB/s) | {report}", mb(bytes.len()) / (best / 1e3));
    println!("columns: {:?}", report.columns);
    let resident = csv.resident_bytes();
    let rss_parsed = allocation_stats().rss_bytes;
    println!(
        "parsed source: {:.1} MB resident | rss +{:.1} MB over the input",
        mb(resident),
        rss_parsed.saturating_sub(rss_before) as f64 / 1e6
    );
    gate("csv.parse_ms", best, "ms");
    gate("csv.rows_loaded", report.rows_loaded as f64, "count");
    gate("csv.rows_skipped", report.rows_skipped as f64, "count");
    gate("csv.source_mb", mb(resident), "MB");

    let mut store = OverlayStore::new();
    let started = Instant::now();
    store.set_source_csv("csv", &csv, &FeaturePaint::default(), 0).expect("a valid push");
    let push_ms = started.elapsed().as_secs_f64() * 1e3;
    drop(csv);
    drop(bytes);
    let data = store.source_point_data("csv").expect("a point source");
    let started = Instant::now();
    let index = Arc::new(points::build_index(&data));
    let index_ms = started.elapsed().as_secs_f64() * 1e3;
    assert!(store.adopt_point_index("csv", data.revision(), Arc::clone(&index)));
    println!(
        "push {push_ms:.1} ms | index build {index_ms:.1} ms | point data {:.1} MB, index {:.1} MB | store resident {:.1} MB",
        mb(data.bytes()),
        mb(index.bytes()),
        mb(store.resident_bytes())
    );
    gate("csv.push_ms", push_ms, "ms");
    gate("csv.index_build_ms", index_ms, "ms");
    gate("csv.store_resident_mb", mb(store.resident_bytes()), "MB");

    // One view over the whole spread, as the map would draw it at z4.
    let name: gpui_maps::overlay::SourceId = "csv".into();
    let paint = points::SourcePaintHandle::compile(&serde_json::Map::new()).expect("empty paint");
    let (min, max) = points::flat_viewport_box(-96.0, 38.5, 4.0, 1280.0, 800.0);
    let key = ViewKey::for_box(4, min, max, 0);
    let started = Instant::now();
    let view = points::build_view(&name, &data, &index, paint.paint(), key);
    let view_ms = started.elapsed().as_secs_f64() * 1e3;
    println!("view z4 national: {} features in {view_ms:.1} ms", view.len());
    gate("csv.view.national_z4.features", view.len() as f64, "count");
    println!("peak rss {:.1} MB", peak_rss_bytes() as f64 / 1e6);
    gate("csv.peak_rss_mb", peak_rss_bytes() as f64 / 1e6, "MB");
}

fn main() {
    let args = parse_args();
    if let Some(path) = &args.write {
        let bytes = generate(args.rows, args.seed);
        std::fs::write(path, &bytes).expect("write the CSV");
        println!("wrote {} rows ({:.1} MB) to {path}", args.rows, mb(bytes.len()));
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
    let env = kit::PanelEnv::from_process(kit::Host::Standalone { title: "CSV points".into() }, runtime.handle().clone(), (1280.0, 800.0));
    let input = open.map(|location| {
        if location.starts_with("https://") || location.starts_with("http://") {
            gpui_maps::map_view::CsvInput::Url(location)
        } else {
            gpui_maps::map_view::CsvInput::Path(location.into())
        }
    });
    let input = input.or_else(panel::input_from_env);
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("CSV points".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let built = panel::build_with(&env, input, window, cx).expect("csv_points panel");
                built.view.downcast::<panel::CsvRoot>().unwrap_or_else(|_| panic!("the panel root is a CsvRoot"))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
    drop(runtime);
}
