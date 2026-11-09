//! A million-row GeoParquet as one overlay source: the whole-file read
//! (`overlay::geoparquet::GeoParquetSource::from_path`), the push onto the
//! point path (`OverlayStore::set_source_geoparquet`), and bbox reads that
//! skip row groups by their 1.1 covering statistics -- timed and sized,
//! headless. The perf gate's `geoparquet.*` section.
//!
//! ```text
//! cargo run --release --example geoparquet [-- PATH|URL|overture]   # a window
//! cargo run --release --example geoparquet -- --measure [--features N] [--seed S]
//! cargo run --release --example geoparquet -- --write /tmp/points.parquet [--features N]
//! ```
//!
//! With no flag it opens the Studio's "GeoParquet" panel in a window: the
//! bundled cities and countries, or the file at `PATH|URL`
//! (`GPUI_MAP_GEOPARQUET` works too; `overture` streams Overture places).
//!
//! The data is SYNTHETIC and seeded (nothing fetched or committed): `id`,
//! `category` (0-4), `value` and `name`, the points clustered around 40
//! cities like `demos/fgb.rs`'s, SORTED along a Z-order curve (as a
//! spatially sorted export -- Overture's, GDAL's `SORT_BY_BBOX` -- is) so
//! that row groups are local, then written with `overlay::geoparquet::write`
//! (snappy, a bbox covering column, 32,768 rows per group) to the system
//! temp directory and deleted afterwards. The bbox reads are warm (the file
//! is in the page cache): the steady state of a panned map reading a local
//! file.

use std::time::Instant;

use gpui_maps::overlay::geoparquet::write::{write, WriteFeature, WriteOptions, WriteType, WriteValue};
use gpui_maps::overlay::geoparquet::{GeoParquetOptions, GeoParquetSource};
use gpui_maps::overlay::{FeaturePaint, OverlayGeometry, OverlayStore};
use gpui_maps::render::allocation_stats;

use swat_maps_kit::kit;
use swat_maps_kit::panels::geoparquet as panel;

/// Rows per row group: Overture writes groups of this order.
const GROUP_ROWS: usize = 32_768;

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

/// Interleaves the bits of two 16-bit cells: a Z-order key.
fn morton(lon: f64, lat: f64) -> u32 {
    let spread = |v: u32| {
        let mut v = v & 0xffff;
        v = (v | (v << 8)) & 0x00ff_00ff;
        v = (v | (v << 4)) & 0x0f0f_0f0f;
        v = (v | (v << 2)) & 0x3333_3333;
        (v | (v << 1)) & 0x5555_5555
    };
    let x = (((lon + 180.0) / 360.0) * 65535.0) as u32;
    let y = (((lat + 90.0) / 180.0) * 65535.0) as u32;
    spread(x) | (spread(y) << 1)
}

const COLUMNS: &[(&str, WriteType)] =
    &[("id", WriteType::Int), ("category", WriteType::Int), ("value", WriteType::Number), ("name", WriteType::Str)];

/// The synthetic file: `n` points over the contiguous US, Z-order sorted.
fn generate(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed);
    let cities: Vec<(f64, f64, f64)> = (0..40)
        .map(|i| (-122.0 + r.next() * 50.0, 27.0 + r.next() * 20.0, 0.04 + 0.25 / (1.0 + i as f64).sqrt()))
        .collect();
    let mut points: Vec<(u32, f64, f64)> = (0..n)
        .map(|_| {
            let (lon, lat) = if r.next() < 0.1 {
                (-124.0 + r.next() * 57.0, 25.0 + r.next() * 24.0)
            } else {
                let (lon, lat, s) = cities[(r.next() * r.next() * cities.len() as f64) as usize];
                (lon + r.gauss() * s / lat.to_radians().cos(), lat + r.gauss() * s)
            };
            let (lon, lat) = (lon.clamp(-180.0, 180.0), lat.clamp(-90.0, 90.0));
            (morton(lon, lat), lon, lat)
        })
        .collect();
    points.sort_unstable_by_key(|p| p.0);
    let features: Vec<WriteFeature> = points
        .iter()
        .enumerate()
        .map(|(i, &(_, lon, lat))| WriteFeature {
            geometry: OverlayGeometry::Point(vec![(lon, lat)]),
            properties: vec![
                WriteValue::Int(Some(i as i64)),
                WriteValue::Int(Some((r.next() * 5.0) as i64)),
                WriteValue::Number(Some((-r.next().max(1e-12).ln() * 100.0).round())),
                WriteValue::Str(Some(format!("site {i}"))),
            ],
        })
        .collect();
    write(COLUMNS, &features, &WriteOptions { row_group_rows: GROUP_ROWS, ..WriteOptions::default() }).expect("writes")
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
    println!("geoparquet --measure: {} features, seed {}", args.features, args.seed);
    let started = Instant::now();
    let bytes = generate(args.features, args.seed);
    println!("generate + write: {:.0} ms, {:.1} MB of GeoParquet", started.elapsed().as_secs_f64() * 1e3, mb(bytes.len()));
    let path = tempfile::Builder::new().prefix("gpui-map-gpq-measure-").suffix(".parquet").tempfile().expect("temp").into_temp_path();
    std::fs::write(&path, &bytes).expect("write the file");
    let file_bytes = bytes.len();
    drop(bytes);
    gate("geoparquet.file_mb", mb(file_bytes), "MB");
    let rss_before = allocation_stats().rss_bytes;

    // Best of 3 (the first run also pays the allocator's page faults).
    let mut best = f64::MAX;
    let mut source = None;
    for _ in 0..3 {
        drop(source.take());
        let started = Instant::now();
        let read = GeoParquetSource::from_path(&path, &GeoParquetOptions::default()).expect("the synthetic file reads");
        best = best.min(started.elapsed().as_secs_f64() * 1e3);
        source = Some(read);
    }
    let source = source.expect("read");
    let report = source.report();
    println!("load: {best:.1} ms best of 3 ({:.0} MB/s) | {report}", mb(file_bytes) / (best / 1e3));
    let resident = source.resident_bytes();
    println!(
        "loaded source: {:.1} MB resident | rss +{:.1} MB",
        mb(resident),
        allocation_stats().rss_bytes.saturating_sub(rss_before) as f64 / 1e6
    );
    gate("geoparquet.load_ms", best, "ms");
    gate("geoparquet.rows_loaded", report.rows_loaded as f64, "count");
    gate("geoparquet.source_mb", mb(resident), "MB");

    // Geometry alone: projection pushdown's saving on the same file.
    let started = Instant::now();
    let bare = GeoParquetSource::from_path(&path, &GeoParquetOptions::default().with_columns(Vec::<String>::new())).expect("reads");
    let bare_ms = started.elapsed().as_secs_f64() * 1e3;
    println!("geometry only: {bare_ms:.1} ms, {:.1} of {:.1} MB read", mb(bare.report().bytes_read as usize), mb(file_bytes));
    gate("geoparquet.load_geometry_only_ms", bare_ms, "ms");
    drop(bare);

    let mut store = OverlayStore::new();
    let started = Instant::now();
    store.set_source_geoparquet("gpq", &source, &FeaturePaint::default(), 0).expect("a valid push");
    let push_ms = started.elapsed().as_secs_f64() * 1e3;
    drop(source);
    println!("push {push_ms:.1} ms | store resident {:.1} MB", mb(store.resident_bytes()));
    gate("geoparquet.push_ms", push_ms, "ms");
    drop(store);

    // Bbox reads by row-group pushdown, by seek: a city view (~1 degree)
    // and a regional one (~8 degrees), 100 of each at seeded centres.
    let mut r = Rng(args.seed ^ 0x5eed);
    for (label, size) in [("city", 1.0), ("region", 8.0)] {
        let mut times = Vec::new();
        let (mut hits, mut groups) = (0usize, 0usize);
        for _ in 0..100 {
            let (lon, lat) = (-120.0 + r.next() * 45.0, 28.0 + r.next() * 18.0);
            let bbox = [lon - size / 2.0, lat - size * 0.3, lon + size / 2.0, lat + size * 0.3];
            let started = Instant::now();
            let got = GeoParquetSource::from_path(&path, &GeoParquetOptions::default().with_bbox(bbox)).expect("a query");
            times.push(started.elapsed().as_secs_f64() * 1e6);
            hits += got.len();
            groups += got.report().row_groups_read;
        }
        times.sort_by(f64::total_cmp);
        let (p50, p95) = (percentile(&times, 0.5), percentile(&times, 0.95));
        println!(
            "bbox read {label}: p50 {p50:.0} us, p95 {p95:.0} us, {:.0} rows and {:.1} row groups per read",
            hits as f64 / 100.0,
            groups as f64 / 100.0
        );
        gate(&format!("geoparquet.query.{label}.p50_us"), p50.round(), "us");
        gate(&format!("geoparquet.query.{label}.p95_us"), p95.round(), "us");
    }
    // One fixed box, counted: exactly these rows, from exactly these groups.
    let fixed = GeoParquetSource::from_path(&path, &GeoParquetOptions::default().with_bbox([-100.0, 35.0, -90.0, 42.0])).expect("a query");
    let fr = fixed.report();
    println!(
        "fixed box: {} rows, {} of {} row groups read ({} skipped), {:.1} MB read",
        fixed.len(),
        fr.row_groups_read,
        fr.row_groups_total,
        fr.row_groups_skipped,
        mb(fr.bytes_read as usize)
    );
    gate("geoparquet.query.fixed.rows_loaded", fixed.len() as f64, "count");
    gate("geoparquet.query.fixed.row_groups_read", fr.row_groups_read as f64, "count");
    println!("peak rss {:.1} MB", peak_rss_bytes() as f64 / 1e6);
    gate("geoparquet.peak_rss_mb", peak_rss_bytes() as f64 / 1e6, "MB");
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
    let env = kit::PanelEnv::from_process(kit::Host::Standalone { title: "GeoParquet".into() }, runtime.handle().clone(), (1280.0, 800.0));
    let choice = open.map(|location| panel::choice_for(&location)).or_else(panel::choice_from_env);
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("GeoParquet".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let built = panel::build_with(&env, choice, window, cx).expect("geoparquet panel");
                built.view.downcast::<panel::GeoParquetRoot>().unwrap_or_else(|_| panic!("the panel root is a GeoParquetRoot"))
            },
        )
        .expect("failed to open window");
    });
    drop(runtime);
}
