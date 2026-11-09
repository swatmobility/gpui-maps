//! MBTiles tile reads, timed: `tile_source::mbtiles::MbtilesSource` on a
//! seeded synthetic file of realistic size, read through
//! `TileSource::fetch` exactly as `MapView` reads it (tokio blocking pool,
//! SQLite, gunzip). The perf gate's `mbtiles.*` section.
//!
//! ```text
//! cargo run --release --example mbtiles [-- PATH]                # a window
//! cargo run --release --example mbtiles -- --measure [--tiles N]  # headless
//! cargo run --release --example mbtiles -- --measure PATH         # a real file
//! cargo run --release --example mbtiles -- --write /tmp/s.mbtiles [--tiles N]
//! ```
//!
//! With no flag it opens the Studio's "MBTiles" panel in a window: the
//! bundled three-city sample, or the file at `PATH` (`GPUI_MAP_MBTILES`
//! works too).
//!
//! The synthetic file is `N` (default 1024) z14 tiles in a square block,
//! each one of the three committed OpenFreeMap z14 tiles (`benches/fixtures/`,
//! 120-690 KB raw, ~60-330 KB gzipped) picked by a seeded generator and
//! stored gzipped, as Planetiler and tippecanoe store them -- so the blob
//! sizes, the B-tree depth and the overflow-page chains SQLite walks are
//! those of a dense city-scale vector file (~0.2 GB at the default). It is
//! written to the system temp directory and deleted afterwards; the reads
//! are warm (the file was just written, so it is in the page cache), which
//! is the steady state of a panned map.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui_maps::geo::TileId;
use gpui_maps::tile_source::mbtiles::MbtilesSource;
use gpui_maps::tile_source::TileSource;

use swat_maps_kit::kit;
use swat_maps_kit::panels::mbtiles as panel;

struct Args {
    tiles: u32,
    seed: u64,
    measure: bool,
    write: Option<String>,
    open: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args { tiles: 1024, seed: 1, measure: false, write: None, open: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--tiles" => a.tiles = it.next().and_then(|s| s.parse().ok()).expect("--tiles takes a count"),
            "--seed" => a.seed = it.next().and_then(|s| s.parse().ok()).expect("--seed takes a number"),
            "--measure" => a.measure = true,
            "--write" => a.write = Some(it.next().expect("--write takes a path")),
            other if !other.starts_with("--") && a.open.is_none() => a.open = Some(other.to_string()),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(bytes).expect("gzip into a Vec");
    gz.finish().expect("gzip into a Vec")
}

/// The square block of z14 tiles the synthetic file holds, west of
/// Shinjuku's tile so it is real ground.
fn grid(tiles: u32) -> Vec<TileId> {
    let side = (f64::from(tiles)).sqrt().ceil() as u32;
    let (x0, y0) = (14549 - side / 2, 6451 - side / 2);
    (0..tiles).map(|i| TileId { z: 14, x: x0 + i % side, y: y0 + i / side }).collect()
}

fn write_synthetic(path: &Path, tiles: u32, seed: u64) -> Vec<TileId> {
    let blobs: Vec<Vec<u8>> = panel::SAMPLE_TILES.iter().map(|(_, _, b)| gzip(b)).collect();
    let ids = grid(tiles);
    let mut r = Rng(seed);
    // Weighted towards the smaller tiles, as a real file is: most of a
    // city's tiles are lighter than its densest block.
    let pick: Vec<usize> = ids.iter().map(|_| [0, 1, 1, 1, 2][(r.next() % 5) as usize]).collect();
    let bytes: u64 = pick.iter().map(|&i| blobs[i].len() as u64).sum();
    let (w, south, e, n) = ids.iter().map(|t| gpui_maps::geo::tile_bounds_lonlat(*t)).fold(
        (180.0f64, 90.0f64, -180.0f64, -90.0f64),
        |(w, s, e, n), b| (w.min(b.min_lon), s.min(b.min_lat), e.max(b.max_lon), n.max(b.max_lat)),
    );
    let metadata = [
        ("name", "synthetic z14 block".to_string()),
        ("format", "pbf".to_string()),
        ("minzoom", "14".to_string()),
        ("maxzoom", "14".to_string()),
        ("bounds", format!("{w},{south},{e},{n}")),
        ("attribution", "\u{a9} OpenStreetMap contributors".to_string()),
    ];
    let started = Instant::now();
    panel::write_mbtiles(path, &metadata, ids.iter().zip(&pick).map(|(t, &i)| (*t, blobs[i].clone()))).expect("write the synthetic file");
    println!(
        "wrote {} tiles ({:.0} MB of gzipped MVT, file {:.0} MB) in {:.1} s",
        ids.len(),
        bytes as f64 / 1e6,
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );
    ids
}

fn gate(name: &str, value: f64, unit: &str) {
    if std::env::var_os("US_BG_GATE").is_some() {
        println!("gate: {name} {value} {unit}");
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Every tile id the file holds, in XYZ (for `--measure PATH`).
fn ids_in(path: &Path) -> Vec<TileId> {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open");
    let xyz = conn
        .query_row("SELECT value FROM metadata WHERE name = 'scheme'", [], |r| r.get::<_, String>(0))
        .is_ok_and(|s| s.eq_ignore_ascii_case("xyz"));
    let mut stmt = conn.prepare("SELECT zoom_level, tile_column, tile_row FROM tiles").expect("tiles");
    stmt.query_map([], |r| Ok((r.get::<_, u8>(0)?, r.get::<_, u32>(1)?, r.get::<_, u32>(2)?)))
        .expect("query")
        .filter_map(Result::ok)
        .map(|(z, x, row)| TileId { z, x, y: if xyz { row } else { (1u32 << z) - 1 - row } })
        .collect()
}

fn measure(args: &Args) {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    let (path, ids, _guard): (PathBuf, Vec<TileId>, Option<tempfile::TempPath>) = match &args.open {
        Some(p) => {
            let path = PathBuf::from(p);
            let ids = ids_in(&path);
            println!("mbtiles --measure {}: {} tiles", path.display(), ids.len());
            (path, ids, None)
        }
        None => {
            println!("mbtiles --measure: {} synthetic z14 tiles, seed {}", args.tiles, args.seed);
            let tmp = tempfile::Builder::new().prefix("gpui-map-mbtiles-measure-").suffix(".mbtiles").tempfile().expect("temp").into_temp_path();
            let ids = write_synthetic(&tmp, args.tiles, args.seed);
            (tmp.to_path_buf(), ids, Some(tmp))
        }
    };

    // Open: best of 3 (the first pays the page faults of a cold process).
    let mut open_ms = f64::MAX;
    let mut source = None;
    for _ in 0..3 {
        let started = Instant::now();
        let s = rt.block_on(MbtilesSource::open(&path)).expect("opens");
        open_ms = open_ms.min(started.elapsed().as_secs_f64() * 1e3);
        source = Some(s);
    }
    let source = Arc::new(source.expect("opened"));
    let m = source.metadata();
    println!("open: {open_ms:.2} ms best of 3 | {} {} z{}-{}", m.name.as_deref().unwrap_or("-"), m.format.as_str(), m.min_zoom, m.max_zoom);
    gate("mbtiles.open_ms", open_ms, "ms");

    // A seeded shuffle, so reads are not in the file's insertion order.
    let mut order = ids.clone();
    let mut r = Rng(args.seed ^ 0x5eed);
    for i in (1..order.len()).rev() {
        order.swap(i, (r.next() % (i as u64 + 1)) as usize);
    }

    // Warm-up pass: brings the file into the page cache for every row.
    let mut bytes_out = 0usize;
    for t in &order {
        bytes_out += rt.block_on(source.fetch(*t)).expect("fetch").map_or(0, |b| b.len());
    }

    // Sequential: one fetch at a time, the latency a lone tile request sees.
    let mut seq: Vec<f64> = order
        .iter()
        .map(|t| {
            let started = Instant::now();
            let got = rt.block_on(source.fetch(*t)).expect("fetch");
            let us = started.elapsed().as_secs_f64() * 1e6;
            assert!(got.is_some(), "tile {t:?} missing");
            us
        })
        .collect();
    seq.sort_by(f64::total_cmp);
    let (p50, p95) = (percentile(&seq, 0.5), percentile(&seq, 0.95));
    println!(
        "fetch (sequential, warm, {} tiles, {:.0} MB decompressed): p50 {p50:.0} us, p95 {p95:.0} us, max {:.0} us",
        seq.len(),
        bytes_out as f64 / 1e6,
        seq[seq.len() - 1]
    );
    gate("mbtiles.fetch_p50_us", p50, "us");
    gate("mbtiles.fetch_p95_us", p95, "us");

    // The gunzip's share, measured apart: one blob per sample tile.
    for (name, _, raw) in panel::SAMPLE_TILES {
        let gz = gzip(raw);
        let started = Instant::now();
        for _ in 0..20 {
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut out).expect("gunzip");
        }
        println!(
            "  gunzip {name}: {:.0} KB -> {:.0} KB in {:.0} us",
            gz.len() as f64 / 1e3,
            raw.len() as f64 / 1e3,
            started.elapsed().as_secs_f64() * 1e6 / 20.0
        );
    }

    // Concurrent: 32 in flight at once (a screenful plus prefetch), per-fetch
    // latency and throughput.
    let inflight = 32usize;
    let started = Instant::now();
    let mut conc: Vec<f64> = rt.block_on(async {
        let mut all = Vec::with_capacity(order.len());
        for chunk in order.chunks(inflight) {
            let handles: Vec<_> = chunk
                .iter()
                .map(|t| {
                    let (source, t) = (source.clone(), *t);
                    tokio::spawn(async move {
                        let started = Instant::now();
                        let got = source.fetch(t).await.expect("fetch");
                        assert!(got.is_some());
                        started.elapsed().as_secs_f64() * 1e6
                    })
                })
                .collect();
            for h in handles {
                all.push(h.await.expect("task"));
            }
        }
        all
    });
    let wall = started.elapsed().as_secs_f64();
    conc.sort_by(f64::total_cmp);
    let (cp50, cp95) = (percentile(&conc, 0.5), percentile(&conc, 0.95));
    let rate = conc.len() as f64 / wall;
    println!("fetch ({inflight} in flight): p50 {cp50:.0} us, p95 {cp95:.0} us | {rate:.0} tiles/s ({:.1}x sequential)", rate / (seq.len() as f64 / (seq.iter().sum::<f64>() / 1e6)));
    gate("mbtiles.concurrent_p50_us", cp50, "us");
    gate("mbtiles.concurrent_p95_us", cp95, "us");
    gate("mbtiles.tiles_read", conc.len() as f64, "count");
}

fn main() {
    let args = parse_args();
    if let Some(path) = &args.write {
        write_synthetic(Path::new(path), args.tiles, args.seed);
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
    let env = kit::PanelEnv::from_process(kit::Host::Standalone { title: "MBTiles".into() }, runtime.handle().clone(), (1280.0, 800.0));
    let path = open.map(PathBuf::from).or_else(panel::path_from_env);
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("MBTiles".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let built = panel::build_with(&env, path, window, cx).expect("mbtiles panel");
                built.view.downcast::<panel::MbtilesRoot>().unwrap_or_else(|_| panic!("the panel root is an MbtilesRoot"))
            },
        )
        .expect("failed to open window");
    });
    drop(runtime);
}
