//! Runtime hexagon binning and overlay extrusion at Kepler's scale,
//! headless: the perf gate's `hexbin.*` section.
//!
//! ```text
//! cargo run --release --example hexbin -- --measure
//! ```
//!
//! Reads the SF street trees exactly as the Kepler sample does (Kepler's
//! `sftrees/data.csv`, 65,499 rows), from the Studio's data cache
//! `~/.cache/gpui-map-data/kepler/sftrees/data.csv`, fetching it there
//! once (with the project User-Agent) when it is missing. Then, as the
//! "San Francisco Street Tree Map" page does:
//!
//! - **bin**: 50 m hexagons, count (`overlay::hexbin`), deck's 99.94th
//!   percentile window;
//! - **extrude**: Kepler's height (count onto 0-500 m, x 5) through
//!   `overlay::extrude` into the building mesh;
//! - **push**: the meshes uploaded (`render::overlay_extrusion`), submit to
//!   idle;
//! - **frame**: one 2560 x 1600 capture (1280 x 800 at 2x) at Kepler's
//!   camera, translucent at 0.8, over an empty dark ground: encode, draw
//!   and read back, the median of a few.
//!
//! Then the Explorer's re-bin at scale: 1M seeded synthetic points (a
//! city's worth of pickups, no data read) binned into 500 m hexagons and
//! 500 m squares (`overlay::gridbin`) by count, and into hexagons by the
//! median of a value (engine gap E10's statistics keep every value).
//!
//! Counts (rows, cells, vertices, upload) are exact for the fixed data;
//! the timings are graded as times.

use std::time::Instant;

use gpui_maps::overlay::extrude::{extrude, ExtrudeFeature, ExtrusionStyle};
use gpui_maps::overlay::gridbin::GridOptions;
use gpui_maps::overlay::hexbin::{bin_points, HexAggregation, HexbinOptions};
use gpui_maps::style::Rgba;

const URL: &str = "https://raw.githubusercontent.com/keplergl/kepler.gl-data/master/sftrees/data.csv";
/// Kepler's `mapState` for the sample.
const CAMERA: (f64, f64, f64, f32, f32) = (-122.44780065592347, 37.75898064019907, 12.509585911862231, 1.794_413_5, -0.606_741_6);
const PALETTE: [[f32; 3]; 8] = [
    [0.169, 0.118, 0.243],
    [0.220, 0.235, 0.396],
    [0.243, 0.373, 0.494],
    [0.286, 0.514, 0.541],
    [0.369, 0.635, 0.553],
    [0.510, 0.733, 0.573],
    [0.682, 0.808, 0.631],
    [0.839, 0.871, 0.749],
];

fn gate(name: &str, value: f64, unit: &str) {
    println!("gate: hexbin.{name} {value} {unit}");
}

fn round(v: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (v * k).round() / k
}

fn data() -> Vec<u8> {
    let home = std::env::var_os("HOME").expect("$HOME");
    let path = std::path::PathBuf::from(home).join(".cache/gpui-map-data/kepler/sftrees/data.csv");
    if let Ok(bytes) = std::fs::read(&path) {
        return bytes;
    }
    eprintln!("hexbin: fetching {URL} into {}", path.display());
    let bytes = tokio::runtime::Runtime::new()
        .expect("a runtime")
        .block_on(async {
            let client = reqwest::Client::builder().user_agent(gpui_maps::tile_source::http::TILE_USER_AGENT).build()?;
            client.get(URL).send().await?.error_for_status()?.bytes().await
        })
        .expect("the SF trees CSV");
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the cache dir");
    std::fs::write(&path, &bytes).expect("cached");
    bytes.to_vec()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// 1M seeded points round twelve centres near Singapore, with a value:
/// the same set every run (a 64-bit LCG, no data read).
fn seeded_million() -> (Vec<[f64; 2]>, Vec<f64>) {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let centres: Vec<[f64; 2]> = (0..12).map(|k| [103.65 + 0.33 * f64::from(k % 4) / 3.0 + 0.02 * f64::from(k), 1.27 + 0.16 * f64::from(k / 4) / 2.0]).collect();
    let mut points = Vec::with_capacity(1_000_000);
    let mut values = Vec::with_capacity(1_000_000);
    for _ in 0..1_000_000 {
        let c = centres[(next() * 12.0) as usize % 12];
        // A rough normal (the sum of three uniforms), about 2 km wide.
        let g = |n: &mut dyn FnMut() -> f64| (n() + n() + n() - 1.5) * 0.025;
        points.push([c[0] + g(&mut next), c[1] + g(&mut next)]);
        values.push((next() * 40.0).round() / 2.0);
    }
    (points, values)
}

/// The best of five runs of `f`, ms, and its last result.
fn best_of_five<T>(mut f: impl FnMut() -> T) -> (f64, T) {
    let mut out = None;
    let mut ms = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        out = Some(f());
        ms.push(started.elapsed().as_secs_f64() * 1e3);
    }
    (median(ms), out.expect("ran"))
}

fn million() {
    let (points, values) = seeded_million();
    let (hex_ms, hex) = best_of_five(|| bin_points(&points, None, &HexbinOptions { radius_m: 500.0, aggregation: HexAggregation::Count, reference_lat: None }));
    gate("hex1m.bin_ms", round(hex_ms, 2), "ms");
    gate("hex1m.cells", hex.bins.len() as f64, "count");
    let (median_ms, med) = best_of_five(|| bin_points(&points, Some(&values), &HexbinOptions { radius_m: 500.0, aggregation: HexAggregation::Median, reference_lat: None }));
    gate("hex1m.median_ms", round(median_ms, 2), "ms");
    let (grid_ms, grid) = best_of_five(|| gpui_maps::overlay::gridbin::bin_points(&points, None, &GridOptions { cell_m: 500.0, aggregation: HexAggregation::Count, reference_lat: None }));
    gate("grid1m.bin_ms", round(grid_ms, 2), "ms");
    gate("grid1m.cells", grid.cells.len() as f64, "count");
    println!(
        "hexbin 1M seeded: {} hexagons in {hex_ms:.1} ms ({} by median in {median_ms:.1} ms), {} squares in {grid_ms:.1} ms",
        hex.bins.len(),
        med.bins.len(),
        grid.cells.len()
    );
}

fn measure() {
    million();
    let bytes = data();
    let options = gpui_maps::overlay::csv::CsvOptions { lon: Some("longitude".into()), lat: Some("latitude".into()), ..Default::default() };
    let csv = gpui_maps::overlay::csv::CsvSource::from_bytes(&bytes, &options).expect("the CSV parses");
    let points = csv.points().expect("points").to_vec();
    gate("sftrees.rows", points.len() as f64, "count");

    // Bin: the best of five (the binning is microseconds per point; the
    // first run also pays the allocator).
    let mut bin_ms = Vec::new();
    let mut grid = None;
    for _ in 0..5 {
        let started = Instant::now();
        let mut g = bin_points(&points, None, &HexbinOptions { radius_m: 50.0, aggregation: HexAggregation::Count, reference_lat: None });
        g.retain_percentile(0.0, 99.94);
        bin_ms.push(started.elapsed().as_secs_f64() * 1e3);
        grid = Some(g);
    }
    let grid = grid.expect("binned");
    gate("sftrees.bin_ms", round(median(bin_ms), 2), "ms");
    gate("sftrees.cells", grid.bins.len() as f64, "count");

    let sorted = grid.sorted_values();
    let (lo, hi) = (sorted[0], sorted[sorted.len() - 1]);
    let features: Vec<ExtrudeFeature> = grid
        .bins
        .iter()
        .map(|b| {
            let t = if hi > lo { (b.value - lo) / (hi - lo) } else { 1.0 };
            let k = ((t * 8.0) as usize).min(7);
            let [r, g, bl] = PALETTE[k];
            ExtrudeFeature { polygons: vec![vec![grid.hexagon(b, 1.0)]], height_m: (t * 500.0 * 5.0) as f32, base_m: 0.0, color: Rgba { r, g, b: bl, a: 1.0 } }
        })
        .collect();
    let mut extrude_ms = Vec::new();
    let mut set = None;
    for _ in 0..5 {
        let started = Instant::now();
        set = Some(extrude(&features));
        extrude_ms.push(started.elapsed().as_secs_f64() * 1e3);
    }
    let set = std::sync::Arc::new(set.expect("extruded"));
    gate("sftrees.extrude_ms", round(median(extrude_ms), 2), "ms");
    gate("sftrees.vertices", set.vertex_count() as f64, "count");
    gate("sftrees.upload_kb", (set.upload_bytes() / 1024) as f64, "KB");
    println!(
        "hexbin sftrees: {} rows -> {} cells, {} tiles at z{}, {} vertices, {} indices, tallest {} m",
        points.len(),
        grid.bins.len(),
        set.tiles.len(),
        set.tile_zoom,
        set.vertex_count(),
        set.index_count(),
        set.max_height_m
    );

    // Push: the upload alone, submit to idle.
    let ctx = pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })
            .await
            .expect("an adapter");
        let (device, queue) =
            adapter.request_device(&wgpu::DeviceDescriptor { required_limits: adapter.limits(), ..Default::default() }).await.expect("a device");
        let max_buffer_size = device.limits().max_buffer_size;
        gpui_maps::render::GpuContext { device, queue, max_buffer_size }
    });
    let pipeline = gpui_maps::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
    let mut push_ms = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let gpu = gpui_maps::render::overlay_extrusion::OverlayExtrusionGpu::upload(&ctx, &pipeline, &set);
        ctx.queue.submit([]);
        let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
        push_ms.push(started.elapsed().as_secs_f64() * 1e3);
        drop(gpu);
    }
    gate("sftrees.push_ms", round(median(push_ms), 2), "ms");

    // Frame: a capture at Kepler's camera.
    let style = gpui_maps::style::Style {
        layers: vec![gpui_maps::style::StyleLayer::Background {
            id: "ground".into(),
            color: gpui_maps::style::ZoomValue::constant(Rgba { r: 0.08, g: 0.09, b: 0.11, a: 1.0 }),
            min_zoom: None,
            max_zoom: None,
        }],
        ..Default::default()
    };
    let request = |on: bool| gpui_maps::capture::CaptureRequest {
        center_lon: CAMERA.0,
        center_lat: CAMERA.1,
        zoom: CAMERA.2,
        pitch: CAMERA.3.to_radians(),
        bearing: CAMERA.4.to_radians(),
        width: 2560,
        height: 1600,
        pixel_ratio: 2.0,
        globe: false,
        flat: true,
        extrusions: if on { vec![gpui_maps::capture::CaptureExtrusion { set: set.clone(), style: ExtrusionStyle { opacity: 0.8, ..Default::default() } }] } else { Vec::new() },
        ..Default::default()
    };
    let mut session = gpui_maps::capture::CaptureSession::new_blocking(None).expect("a headless device");
    let tiles = gpui_maps::capture::TileStore::new();
    let time = |session: &mut gpui_maps::capture::CaptureSession, on: bool| {
        let r = request(on);
        let _ = session.render(&r, &tiles, &style).expect("a frame");
        let before = gpui_maps::render::allocation_stats().draw_calls_total;
        let ms: Vec<f64> = (0..7)
            .map(|_| {
                let started = Instant::now();
                let _ = session.render(&r, &tiles, &style).expect("a frame");
                started.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        let draws = gpui_maps::render::allocation_stats().draw_calls_total - before;
        (median(ms), draws)
    };
    let (empty_ms, _) = time(&mut session, false);
    let (frame_ms, draws) = time(&mut session, true);
    assert!(draws > 0, "draw_calls_total did not move -- nothing was drawn");
    println!("hexbin frame: {frame_ms:.2} ms with the columns, {empty_ms:.2} ms without (2560 x 1600, render + readback); {draws} draws over 7 frames");
    gate("sftrees.frame_ms", round(frame_ms, 2), "ms");
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--measure") => measure(),
        _ => eprintln!("usage: hexbin --measure"),
    }
}
