//! The Studio Explorer's filter path at a million rows, headless: the perf
//! gate's `explorer.*` section (plan Task 3.6, spec §11.4).
//!
//! ```text
//! cargo run --release --example explorer_gate -- --measure [--rows N] [--seed S]
//! ```
//!
//! The data is SYNTHETIC and seeded (nothing fetched or committed): a
//! quakes-shaped CSV (`time` as ISO 8601, `latitude`, `longitude`,
//! `depth`, `mag`, `rms`, `magType`, `tsunami`) clustered around ten
//! centres. It runs the same pieces the Explorer runs, in the same order:
//!
//! 1. `overlay::csv` parses it and `swat_maps_doc::fields::refine` types it
//!    (the loader, off the UI thread in the Studio);
//! 2. the table is pushed onto the point path with its typed columns, time
//!    as milliseconds (`bind::with_columns`), and indexed;
//! 3. **`explorer.points_1m.pan_frame_ms_p50`**: the point view a pan step
//!    builds when it needs a new cover (the point path's per-frame CPU
//!    work; no GPU here), p50 over a 24-step pan along the clusters;
//! 4. **`explorer.filter_drag.uploads`**: a GPU range filter declared on
//!    `mag` (`set_source_filter_fields`), then 40 drag steps of its window:
//!    every point-data revision and view rebuild those steps cause. A
//!    window is a uniform, so it must stay 0;
//! 5. `explorer.filter_drag.count_ms_p50`: the drawn-row count each drag
//!    step asks for off the UI thread (`PointFilter::passes_all`, the
//!    engine's CPU twin of the shader);
//! 6. **`explorer.cpu_filter_release_ms_p50`**: a CPU filter's release --
//!    the mask (`swat_maps_doc::filters::cpu_mask`), the passing rows'
//!    table (`Table::take`) and their re-push -- p50 over five windows;
//!    `explorer.cpu_filter_index_ms_p50` is the index the re-push then
//!    builds off the UI thread. The spec's bound for the release is
//!    250 ms (above it, E-mask: hide rows without a re-push).

use std::time::Instant;

use swat_maps_doc::doc::Filter;
use swat_maps_doc::filters::{cpu_mask, passing_rows, FilterKind, FilterValue};
use swat_maps_doc::table::{Column, Field, FieldType, Geometry, Table};
use gpui_maps::overlay::csv::{CsvOptions, CsvSource, CsvValues};
use gpui_maps::overlay::points::{self, ViewKey};
use gpui_maps::overlay::{FeaturePaint, OverlayStore, PropertyColumn};

struct Args {
    rows: usize,
    seed: u64,
    measure: bool,
}

fn parse_args() -> Args {
    let mut a = Args { rows: 1_000_000, seed: 7, measure: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rows" => a.rows = it.next().and_then(|s| s.parse().ok()).expect("--rows takes a count"),
            "--seed" => a.seed = it.next().and_then(|s| s.parse().ok()).expect("--seed takes a number"),
            "--measure" => a.measure = true,
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

const CENTRES: [(f64, f64); 10] = [(139.7, 35.6), (-70.6, -33.4), (-122.4, 37.7), (-149.9, 61.2), (106.8, -6.2), (21.7, 38.3), (-155.3, 19.4), (121.0, 14.6), (174.8, -41.3), (-100.0, 40.0)];

/// `YYYY-MM-DDTHH:MM:SSZ` for seconds since 1970 (UTC).
fn iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

fn generate(rows: usize, seed: u64) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut r = Rng(seed);
    let types = ["ml", "md", "mb", "mww", "mwr"];
    let start = 1_788_220_800_i64; // 2026-09-01
    let mut out = String::with_capacity(rows * 72);
    out.push_str("time,latitude,longitude,depth,mag,rms,magType,tsunami\n");
    for i in 0..rows {
        let (lon, lat) = CENTRES[i % CENTRES.len()];
        let (lon, lat) = (lon + r.gauss() * 2.5, (lat + r.gauss() * 2.5).clamp(-84.0, 84.0));
        let t = start + (r.next() * 30.0 * 86_400.0) as i64;
        let mag = (-r.next().max(1e-12).ln() / 0.9).min(8.0);
        let _ = writeln!(out, "{},{lat:.4},{lon:.4},{:.2},{mag:.2},{:.3},{},{}", iso(t), (r.gauss() * 30.0 + 20.0).abs(), r.next(), types[i % types.len()], mag > 6.0);
    }
    out.into_bytes()
}

fn gate(name: &str, value: f64, unit: &str) {
    if std::env::var_os("US_BG_GATE").is_some() {
        println!("gate: {name} {value} {unit}");
    }
}

fn p50(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// The CSV as the Explorer's loader builds it.
fn table(bytes: &[u8]) -> Table {
    let csv = CsvSource::from_bytes(bytes, &CsvOptions::default()).expect("the synthetic CSV parses");
    let fields = csv
        .columns()
        .iter()
        .map(|(name, values)| {
            let column = match values {
                CsvValues::Number(v) => Column::Number(v.clone()),
                CsvValues::Bool(v) => Column::Bool(v.clone()),
                CsvValues::Str(s) => Column::from_strs((0..s.len()).map(|i| s.get(i))),
            };
            Field { name: name.clone(), ty: FieldType::String, hint: None, column }
        })
        .collect();
    let points = csv.points().expect("lat/lon columns").to_vec();
    let mut t = Table { id: "quakes".into(), label: "quakes".into(), rows: points.len(), geometry: Geometry::Points(points), fields };
    swat_maps_doc::fields::refine(&mut t);
    t
}

/// Pushes `t` as the Explorer's binding does (time as milliseconds, text
/// through a transient `&str` column).
fn push(store: &mut OverlayStore, name: &str, t: &Table) {
    let Geometry::Points(points) = &t.geometry else { unreachable!("a point table") };
    let text: Vec<Option<Vec<Option<&str>>>> = t.fields.iter().map(|f| matches!(f.column, Column::Str { .. }).then(|| (0..f.column.len()).map(|i| f.column.str_at(i)).collect())).collect();
    let columns: Vec<(&str, PropertyColumn<'_>)> = t
        .fields
        .iter()
        .zip(&text)
        .map(|(f, s)| {
            let col = match (s, &f.column) {
                (Some(s), _) => PropertyColumn::Str(s),
                (None, Column::Number(v) | Column::Time { ms: v, .. }) => PropertyColumn::Number(v),
                (None, Column::Bool(v)) => PropertyColumn::Bool(v),
                (None, Column::Str { .. }) => unreachable!("decoded above"),
            };
            (f.name.as_str(), col)
        })
        .collect();
    store.set_source_points(name, points, None, &FeaturePaint::default(), &columns).expect("a valid push");
}

fn reindex(store: &mut OverlayStore, name: &str) -> f64 {
    let data = store.source_point_data(name).expect("a point source");
    let started = Instant::now();
    let idx = std::sync::Arc::new(points::build_index(&data));
    let t = ms(started);
    assert!(store.adopt_point_index(name, data.revision(), idx));
    t
}

fn measure(args: &Args) {
    println!("explorer_gate --measure: {} rows, seed {}", args.rows, args.seed);
    let bytes = generate(args.rows, args.seed);
    let started = Instant::now();
    let t = table(&bytes);
    let load_ms = ms(started);
    drop(bytes);
    println!("load: {} rows typed in {load_ms:.0} ms; fields {:?}", t.rows, t.fields.iter().map(|f| (f.name.as_str(), f.ty.name())).collect::<Vec<_>>());
    assert!(t.field("time").is_some_and(|f| f.ty == FieldType::Timestamp), "the time column is typed as time");
    gate("explorer.load_ms", load_ms, "ms");
    gate("explorer.rows_loaded", t.rows as f64, "count");

    let name = "explorer/quakes-points";
    let mut store = OverlayStore::new();
    let started = Instant::now();
    push(&mut store, name, &t);
    let push_ms = ms(started);
    let index_ms = reindex(&mut store, name);
    println!("push {push_ms:.1} ms | index {index_ms:.1} ms");
    gate("explorer.push_ms", push_ms, "ms");

    // A GPU range filter on `mag`, as the binding declares it.
    let started = Instant::now();
    let domain = store.set_source_filter_fields(name, &["mag"]).expect("mag filters");
    println!("filter field declared in {:.1} ms, domain {:?}", ms(started), domain[0]);
    let data = store.source_point_data(name).expect("data");
    let index = store.source_point_index(name).expect("index");
    let paint = points::SourcePaintHandle::compile(&serde_json::Map::new()).expect("empty paint");
    let source_id: gpui_maps::overlay::SourceId = name.into();

    // 3. A pan along the clusters at z5, 1280 x 800: a new cover per step.
    let mut pan = Vec::new();
    for step in 0..24 {
        let lon = -170.0 + step as f64 * 14.0;
        let (min, max) = points::flat_viewport_box(lon, 20.0, 5.0, 1280.0, 800.0);
        let key = ViewKey::for_box(5, min, max, 0);
        let started = Instant::now();
        let view = points::build_view(&source_id, &data, &index, paint.paint(), key);
        pan.push(ms(started));
        std::hint::black_box(view.len());
    }
    let pan_p50 = p50(pan);
    println!("pan view build p50 {pan_p50:.2} ms over 24 steps");
    gate("explorer.points_1m.pan_frame_ms_p50", pan_p50, "ms");

    // 4-5. Forty drag steps of the `mag` window.
    let revision = data.revision();
    let filter = data.filter().expect("a filter column").clone();
    let mut counts = Vec::new();
    let mut shown = 0;
    for step in 0..40 {
        let lo = domain[0][0] + (domain[0][1] - domain[0][0]) * step as f64 / 80.0;
        let window = [lo, domain[0][1]];
        let started = Instant::now();
        shown = (0..data.len()).filter(|&i| filter.passes_all(i, &[Some(window)])).count();
        counts.push(ms(started));
    }
    let after = store.source_point_data(name).expect("data");
    let uploads = u64::from(after.revision() != revision) + u64::from(store.source_point_index(name).is_none());
    println!("drag: 40 window moves, {uploads} revision changes or rebuilds, count p50 {:.2} ms ({shown} drawn at the last step)", p50(counts.clone()));
    gate("explorer.filter_drag.uploads", uploads as f64, "count");
    gate("explorer.filter_drag.count_ms_p50", p50(counts), "ms");

    // 6. A CPU filter's release, five windows on `rms`.
    let mut releases = Vec::new();
    let mut indexes = Vec::new();
    for k in 0..5 {
        let lo = 0.1 * k as f64;
        let f = Filter::new("cpu", "quakes", "rms", FilterKind::Range, FilterValue::Range([lo, lo + 0.5]));
        let started = Instant::now();
        let rows = passing_rows(&cpu_mask(&t, [&f]));
        let sub = t.take(&rows);
        push(&mut store, name, &sub);
        store.set_source_filter_fields(name, &["mag"]);
        releases.push(ms(started));
        indexes.push(reindex(&mut store, name));
        println!("release {k}: {} of {} rows in {:.1} ms, index {:.1} ms", rows.len(), t.rows, releases[k], indexes[k]);
    }
    let release_p50 = p50(releases);
    gate("explorer.cpu_filter_release_ms_p50", release_p50, "ms");
    gate("explorer.cpu_filter_index_ms_p50", p50(indexes), "ms");
    if release_p50 > 250.0 {
        println!("NOTE: a CPU release over 250 ms at this size: the spec's E-mask follow-up applies");
    }
}

fn main() {
    let args = parse_args();
    if args.measure {
        measure(&args);
    } else {
        println!("explorer_gate: run with --measure (see the module doc)");
    }
}
