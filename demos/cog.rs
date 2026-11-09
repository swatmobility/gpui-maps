//! Cloud-Optimised GeoTIFF through `tile_source::cog`: the Studio's "Cloud
//! GeoTIFF" panel in a window, a headless `--measure` (the perf gate's
//! `cog.*` section), and two tools.
//!
//! ```text
//! cargo run --release --example cog [-- PATH|URL|usgs]          # a window
//! cargo run --release --example cog -- --measure
//! cargo run --release --example cog -- --info PATH|URL
//! cargo run --release --example cog -- --png PATH|URL Z X Y OUT.png
//! cargo run --release --example cog -- --make-fixture OUT.tif
//! ```
//!
//! `--measure` is SYNTHETIC and seeded (nothing fetched or committed): a
//! 4096 x 4096 RGB COG (deflate, horizontal predictor) and a 4096 x 4096
//! float32 DEM (deflate, floating-point predictor, nodata), 256 px internal
//! tiles with overviews, written by `tile_source::cog::write` to the temp
//! directory and deleted. It times the open, one map tile's decode and
//! resample (cold: every internal tile decoded), counts the range requests
//! one 1280 x 800 view costs with and without coalescing, and times that
//! view over a local HTTP server that delays every response 20 ms, with one
//! request in flight and with eight.
//!
//! `--make-fixture` writes `tests/fixtures/cog/rainier_dem.tif`: Mount
//! Rainier from USGS 3DEP's 1 arc-second tile `n47w122` (public domain),
//! read by range request and resampled onto 2 x 2 z11 map tiles at 192 px (384 x 384
//! px, EPSG:3857), as whole metres in int16.

use std::io::{BufRead as _, BufReader, Write as _};
use std::time::{Duration, Instant};

use gpui_maps::geo::TileId;
use gpui_maps::tile_source::cog::write::{write, WriteCompression, WriteImage, WriteSamples};
use gpui_maps::tile_source::cog::{CogOptions, CogSource};

use swat_maps_kit::kit;
use swat_maps_kit::panels::cog as panel;

const HALF_WORLD: f64 = std::f64::consts::PI * 6_378_137.0;

fn gate(name: &str, value: f64, unit: &str) {
    if std::env::var_os("US_BG_GATE").is_some() {
        println!("gate: {name} {value} {unit}");
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a tokio runtime")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--measure") => measure(),
        Some("--info") => info(args.get(1).expect("--info takes a path or URL")),
        Some("--png") => {
            let loc = args.get(1).expect("--png LOC Z X Y OUT");
            let n = |i: usize| args.get(i).and_then(|s| s.parse::<u32>().ok()).expect("--png LOC Z X Y OUT");
            let tile = TileId { z: n(2) as u8, x: n(3), y: n(4) };
            png(loc, tile, args.get(5).expect("--png LOC Z X Y OUT"));
        }
        Some("--make-fixture") => make_fixture(args.get(1).map_or("tests/fixtures/cog/rainier_dem.tif", String::as_str)),
        Some(other) if other.starts_with("--") => panic!("unknown argument {other}"),
        other => windowed(other.map(panel::choice_for)),
    }
}

fn open(loc: &str) -> CogSource {
    let options = if loc.eq_ignore_ascii_case("usgs") { panel::dem_options() } else { CogOptions::default() };
    let loc = if loc.eq_ignore_ascii_case("usgs") { panel::USGS_RAINIER_URL } else { loc };
    let started = Instant::now();
    let cog = CogSource::open_blocking(loc, options).unwrap_or_else(|e| panic!("{e}"));
    println!("opened in {:.0} ms", started.elapsed().as_secs_f64() * 1e3);
    cog
}

fn info(loc: &str) {
    let cog = open(loc);
    let m = cog.metadata();
    println!("{}", m.summary());
    println!("bounds {:?}  max zoom {}  rendering {:?}  nodata {:?}  stats {:?}", m.bounds, m.max_zoom, m.rendering, m.nodata, m.stats);
    for (i, l) in m.levels.iter().enumerate() {
        println!("  level {i}: {} x {} at {:?}", l.width, l.height, l.resolution);
    }
    println!("header {} bytes in {} requests; file {:?} bytes", m.header_bytes, m.header_requests, m.file_bytes);
    for w in &m.warnings {
        println!("warning: {w}");
    }
}

fn png(loc: &str, tile: TileId, out: &str) {
    let cog = open(loc);
    let rt = runtime();
    let started = Instant::now();
    let image = rt.block_on(cog.render_tile(tile)).unwrap_or_else(|e| panic!("{e}"));
    println!("{tile:?} in {:.0} ms: {:?}", started.elapsed().as_secs_f64() * 1e3, cog.stats());
    match image {
        Some(image) => image.save(out).expect("write the PNG"),
        None => println!("outside the image"),
    }
}

fn make_fixture(out: &str) {
    let cog = CogSource::open_blocking(panel::USGS_RAINIER_URL, CogOptions::default().with_range(0.0, 4400.0)).unwrap_or_else(|e| panic!("{e}"));
    let rt = runtime();
    let (x0, y0, z) = (330u32, 721u32, 11u8);
    let size = 192usize;
    let mut values = vec![0i16; 4 * size * size];
    for dy in 0..2u32 {
        for dx in 0..2u32 {
            let tile = TileId { z, x: x0 + dx, y: y0 + dy };
            let v = rt.block_on(cog.values_tile(tile, size as u32)).expect("reads").expect("inside");
            for j in 0..size {
                for i in 0..size {
                    let at = (dy as usize * size + j) * 2 * size + dx as usize * size + i;
                    values[at] = v[j * size + i].round() as i16;
                }
            }
        }
    }
    let world = 2.0 * HALF_WORLD;
    let stats = (f64::from(*values.iter().min().expect("values")), f64::from(*values.iter().max().expect("values")));
    let tiles = f64::from(1u32 << z);
    let bytes = write(&WriteImage {
        width: 2 * size as u32,
        height: 2 * size as u32,
        samples: WriteSamples::I16(values, 1),
        tile: 128,
        compression: WriteCompression::Deflate,
        predictor: 2,
        stats: Some(stats),
        ..WriteImage::web_mercator_square(-HALF_WORLD + f64::from(x0) / tiles * world, HALF_WORLD - f64::from(y0) / tiles * world, world / tiles / size as f64)
    });
    std::fs::write(out, &bytes).expect("write the fixture");
    println!("wrote {out}: {} bytes; {:?}", bytes.len(), cog.stats());
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
}

/// The synthetic pair: an RGB image and a DEM over the same 3857 square
/// (z6 tile (33, 22) at 4096 px, so z9 is full resolution).
fn synthetic(seed: u64) -> (Vec<u8>, Vec<u8>) {
    let n = 4096u32;
    let mut r = Rng(seed);
    let bumps: Vec<(f64, f64, f64, f64)> = (0..40).map(|_| (r.next() * 4096.0, r.next() * 4096.0, 80.0 + r.next() * 600.0, r.next() * 2000.0)).collect();
    let mut dem = Vec::with_capacity((n * n) as usize);
    let mut rgb = Vec::with_capacity((n * n * 3) as usize);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (f64::from(x), f64::from(y));
            let mut h = 100.0 + 0.05 * fx;
            for &(bx, by, s, a) in &bumps {
                let d2 = (fx - bx).powi(2) + (fy - by).powi(2);
                if d2 < 9.0 * s * s {
                    h += a * (-d2 / (2.0 * s * s)).exp();
                }
            }
            let hole = (1800..2000).contains(&x) && (1800..2000).contains(&y);
            dem.push(if hole { -9999.0 } else { h.round() as f32 });
            rgb.extend_from_slice(&[(h / 12.0).min(255.0) as u8, ((fx / 16.0) as u32 % 256) as u8, ((fy * 7.0 / 16.0) as u32 % 256) as u8]);
        }
    }
    let span = 2.0 * HALF_WORLD / 64.0;
    let (x0, y0) = (-HALF_WORLD + 33.0 * span, HALF_WORLD - 22.0 * span);
    let base = WriteImage { width: n, height: n, tile: 256, ..WriteImage::web_mercator_square(x0, y0, span / f64::from(n)) };
    let rgb = write(&WriteImage { samples: WriteSamples::U8(rgb, 3), predictor: 2, ..base.clone() });
    let dem = write(&WriteImage { samples: WriteSamples::F32(dem, 1), predictor: 3, nodata: Some(-9999.0), stats: Some((100.0, 2500.0)), ..base });
    (rgb, dem)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// A local HTTP server answering `Range` with a fixed delay per response.
fn serve(file: Vec<u8>, delay: Duration) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let file = std::sync::Arc::new(file);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let file = file.clone();
            std::thread::spawn(move || loop {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut range = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        let (a, b) = v.trim().split_once('-').expect("a-b");
                        let a: usize = a.parse().expect("start");
                        let b: usize = b.parse::<usize>().unwrap_or(file.len() - 1).min(file.len() - 1);
                        range = Some((a, b));
                    }
                }
                std::thread::sleep(delay);
                let (a, b) = range.unwrap_or((0, file.len() - 1));
                let body = &file[a..=b];
                let head = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/{}\r\nContent-Length: {}\r\n\r\n", file.len(), body.len());
                if stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body)).is_err() {
                    return;
                }
            });
        }
    });
    format!("http://{addr}/synthetic.tif")
}

fn measure() {
    println!("cog --measure: synthetic 4096 x 4096 RGB and float32 DEM, 256 px tiles");
    let started = Instant::now();
    let (rgb, dem) = synthetic(1);
    println!("generate + write: {:.0} ms; RGB {:.1} MB, DEM {:.1} MB", started.elapsed().as_secs_f64() * 1e3, rgb.len() as f64 / 1e6, dem.len() as f64 / 1e6);
    let dir = tempfile::tempdir().expect("a temp dir");
    let rgb_path = dir.path().join("rgb.tif");
    let dem_path = dir.path().join("dem.tif");
    std::fs::write(&rgb_path, &rgb).expect("write");
    std::fs::write(&dem_path, &dem).expect("write");
    let rt = runtime();

    // Open: the header, best of 5.
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let cog = rt.block_on(CogSource::open(rgb_path.to_str().expect("utf-8"), CogOptions::default())).expect("opens");
        best = best.min(t.elapsed().as_secs_f64() * 1e6);
        drop(cog);
    }
    println!("open (header): {best:.0} us best of 5");
    gate("cog.open_us", best.round(), "us");

    // One map tile, cold: a fresh source each time so every internal tile
    // is decoded. 40 seeded tiles at z7-z9.
    let dem_options = || CogOptions::default().with_hillshade(1.0);
    for (label, path, options) in
        [("rgb", &rgb_path, CogOptions::default()), ("dem", &dem_path, dem_options()), ("dem_flat", &dem_path, CogOptions::default())]
    {
        let mut times = Vec::new();
        let mut r = Rng(7);
        let mut decoded = 0;
        for _ in 0..40 {
            let z = 7 + (r.next() * 3.0) as u8;
            let k = 1u32 << (z - 6);
            let tile = TileId { z, x: 33 * k + (r.next() * f64::from(k)) as u32, y: 22 * k + (r.next() * f64::from(k)) as u32 };
            let cog = rt.block_on(CogSource::open(path.to_str().expect("utf-8"), options.clone())).expect("opens");
            let t = Instant::now();
            rt.block_on(cog.render_tile(tile)).expect("renders").expect("inside");
            times.push(t.elapsed().as_secs_f64() * 1e6);
            decoded += cog.stats().tiles_decoded;
        }
        times.sort_by(f64::total_cmp);
        let (p50, p95) = (percentile(&times, 0.5), percentile(&times, 0.95));
        println!("map tile {label} (512 px, cold): p50 {p50:.0} us, p95 {p95:.0} us, {:.1} internal tiles decoded per map tile", decoded as f64 / 40.0);
        gate(&format!("cog.tile.{label}.p50_us"), p50.round(), "us");
        gate(&format!("cog.tile.{label}.p95_us"), p95.round(), "us");
    }

    // One 1280 x 800 view at z8 over the DEM: the map tiles a map asks for,
    // each made once, sequentially (the cache shares internal tiles between
    // neighbours), with and without coalescing.
    let view = gpui_maps::geo::visible_tiles(gpui_maps::geo::tile_xy_to_lon_lat(33.5 * 4.0, 22.5 * 4.0, 8.0), 8, (1280.0, 800.0));
    for (label, gap) in [("coalesced", CogOptions::default().merge_gap), ("uncoalesced", 0)] {
        let options = CogOptions { merge_gap: gap, max_request_bytes: if gap == 0 { 1 } else { CogOptions::default().max_request_bytes }, ..dem_options() };
        let cog = rt.block_on(CogSource::open(dem_path.to_str().expect("utf-8"), options)).expect("opens");
        for &tile in &view {
            let _ = rt.block_on(cog.render_tile(tile)).expect("renders");
        }
        let s = cog.stats();
        println!("view z8 ({} map tiles, {label}): {} range requests, {} internal tiles, {:.2} MB", view.len(), s.requests, s.tiles_decoded, s.bytes_read as f64 / 1e6);
        gate(&format!("cog.view.{label}.range_requests"), s.requests as f64, "count");
        if gap != 0 {
            gate("cog.view.tiles_decoded", s.tiles_decoded as f64, "count");
            gate("cog.view.map_tiles", view.len() as f64, "count");
        }
    }

    // The same view over HTTP with 20 ms per response: requests one at a
    // time versus eight in flight, every map tile of the view requested at
    // once as the map does.
    let url = serve(dem.clone(), Duration::from_millis(20));
    for limit in [1usize, 8] {
        let options = CogOptions { max_concurrent: limit, ..dem_options() };
        let cog = rt.block_on(CogSource::open(&url, options)).expect("opens over HTTP");
        let t = Instant::now();
        rt.block_on(async {
            let tasks: Vec<_> = view
                .iter()
                .map(|&tile| {
                    let cog = cog.clone();
                    tokio::spawn(async move { cog.render_tile(tile).await })
                })
                .collect();
            for task in tasks {
                task.await.expect("joins").expect("renders");
            }
        });
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let s = cog.stats();
        println!("view over HTTP (20 ms/response), {limit} in flight per tile: {ms:.0} ms, {} requests, {} internal tiles", s.requests, s.tiles_decoded);
        gate(&format!("cog.view.http_{limit}.ms"), ms.round(), "ms");
    }
}

fn windowed(choice: Option<panel::Choice>) {
    use gpui::{px, size, App, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
    let runtime = runtime();
    let env = kit::PanelEnv::from_process(kit::Host::Standalone { title: "Cloud GeoTIFF".into() }, runtime.handle().clone(), (1280.0, 800.0));
    let choice = choice.or_else(panel::choice_from_env).unwrap_or(panel::Choice::Bundled);
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Cloud GeoTIFF".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let built = panel::build_with(&env, choice, window, cx).expect("cog panel");
                built.view.downcast::<panel::CogRoot>().unwrap_or_else(|_| panic!("the panel root is a CogRoot"))
            },
        )
        .expect("failed to open window");
    });
    drop(runtime);
}
