//! `CogSource` over HTTP against a local server that answers `Range`:
//! opening reads only the header; a map tile reads only the internal tiles
//! it needs, adjacent ones coalesced into one request; a second render of
//! the same tile reads nothing; a whole view requested at once fetches
//! every internal tile at most once; a server that ignores `Range` still
//! works; a non-2xx is an error. Here rather than in `src/` because the
//! library is held to zero listening sockets (`agent::tests`), and a test
//! server is one.
#![cfg(feature = "sprite_http_fetch")]

use std::io::{BufRead as _, BufReader, Write as _};
use std::sync::{Arc, Mutex};

use gpui_maps::geo::TileId;
use gpui_maps::tile_source::cog::write::{write, WriteImage, WriteSamples};
use gpui_maps::tile_source::cog::{CogError, CogOptions, CogSource, COG_USER_AGENT};
use gpui_maps::tile_source::TileSource as _;

#[derive(Default)]
struct Log {
    ranges: Vec<Option<(u64, u64)>>,
    agents: Vec<String>,
}

/// Serves `file`: `Range: bytes=a-b` gets a 206 with `Content-Range`,
/// unless `honour_range` is off (then a 200 of the whole file). `status`
/// other than 200 answers every request with it.
fn serve(file: Vec<u8>, honour_range: bool, status: u16) -> (String, Arc<Mutex<Log>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Log::default()));
    let file = Arc::new(file);
    let seen = Arc::clone(&log);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (file, seen) = (Arc::clone(&file), Arc::clone(&seen));
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut range = None;
                let mut agent = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("range: bytes=") {
                        let (a, b) = v.trim().split_once('-').unwrap();
                        let a: u64 = a.parse().unwrap();
                        let b: u64 = if b.is_empty() { file.len() as u64 - 1 } else { b.parse::<u64>().unwrap().min(file.len() as u64 - 1) };
                        range = Some((a, b));
                    }
                    if lower.starts_with("user-agent:") {
                        agent = line["user-agent:".len()..].trim().to_string();
                    }
                }
                {
                    let mut seen = seen.lock().unwrap();
                    seen.ranges.push(range);
                    seen.agents.push(agent);
                }
                let reply = if status != 200 {
                    format!("HTTP/1.1 {status} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
                } else if let (Some((a, b)), true) = (range, honour_range) {
                    let body = &file[a as usize..=b as usize];
                    let mut r = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        file.len(),
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(body);
                    r
                } else {
                    let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", file.len()).into_bytes();
                    r.extend_from_slice(&file);
                    r
                };
                let _ = stream.write_all(&reply);
            });
        }
    });
    (format!("http://{addr}/dem.tif"), log)
}

const HALF_WORLD: f64 = std::f64::consts::PI * 6_378_137.0;

/// A 2048 x 2048 float32 DEM exactly covering z5 tile (16, 10), 256 px
/// internal tiles, three overviews; values ramp left to right.
fn dem() -> Vec<u8> {
    let n = 2048u32;
    let values: Vec<f32> = (0..n * n).map(|i| (i % n) as f32).collect();
    let span = 2.0 * HALF_WORLD / 32.0;
    write(&WriteImage {
        width: n,
        height: n,
        samples: WriteSamples::F32(values, 1),
        tile: 256,
        predictor: 3,
        stats: Some((0.0, 2047.0)),
        ..WriteImage::web_mercator_square(-HALF_WORLD + 16.0 * span, HALF_WORLD - 10.0 * span, span / f64::from(n))
    })
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap()
}

#[test]
fn opening_reads_the_header_and_a_map_tile_reads_only_its_internal_tiles_coalesced() {
    let file = dem();
    let (url, log) = serve(file.clone(), true, 200);
    let rt = runtime();
    let cog = rt.block_on(CogSource::open(&url, CogOptions::default())).unwrap();
    {
        let log = log.lock().unwrap();
        assert_eq!(log.ranges, vec![Some((0, 65_535))], "one header read");
        assert_eq!(log.agents, vec![COG_USER_AGENT.to_string()]);
    }
    let m = cog.metadata();
    assert_eq!(m.file_bytes, Some(file.len() as u64));
    assert_eq!(m.max_zoom, 7, "2048 px over one z5 tile: z7 at 512 px");
    // z7 (64, 40): the top-left 512 x 512 px of the full image, 2 x 2
    // internal tiles (+1 px margin: a third row and column). The file
    // stores them row by row: three runs of three adjacent tiles, a few
    // KiB apart -- one request within the default 32 KiB gap.
    let image = rt.block_on(cog.render_tile(TileId { z: 7, x: 64, y: 40 })).unwrap().expect("inside");
    assert_eq!(image.dimensions(), (512, 512));
    let s = cog.stats();
    assert_eq!(s.tiles_decoded, 9, "{s:?}");
    assert_eq!(s.requests, 1, "nine tiles, one coalesced request: {s:?}");
    assert_eq!(log.lock().unwrap().ranges.len(), 2);
    // Again: all from the cache.
    rt.block_on(cog.render_tile(TileId { z: 7, x: 64, y: 40 })).unwrap();
    assert_eq!(log.lock().unwrap().ranges.len(), 2, "nothing re-read");
    assert_eq!(cog.stats().requests, 1);
    // Outside the image: absent, and no request.
    assert!(rt.block_on(cog.render_tile(TileId { z: 7, x: 0, y: 0 })).unwrap().is_none());
    assert_eq!(log.lock().unwrap().ranges.len(), 2);

    // With no gap allowed, the runs are three requests (each run's
    // tiles are adjacent bytes) and never a byte more than the tiles.
    let strict = rt.block_on(CogSource::open(&url, CogOptions { merge_gap: 0, ..CogOptions::default() })).unwrap();
    rt.block_on(strict.render_tile(TileId { z: 7, x: 64, y: 40 })).unwrap();
    assert_eq!(strict.stats().requests, 3, "{:?}", strict.stats());
}

#[test]
fn a_view_requested_at_once_fetches_each_internal_tile_once() {
    let file = dem();
    let (url, log) = serve(file, true, 200);
    let rt = runtime();
    let cog = rt.block_on(CogSource::open(&url, CogOptions::default())).unwrap();
    // A 4 x 4 block of z7 map tiles, all at once, as the map asks.
    let tiles: Vec<TileId> = (64..68).flat_map(|x| (40..44).map(move |y| TileId { z: 7, x, y })).collect();
    rt.block_on(async {
        let tasks: Vec<_> = tiles
            .iter()
            .map(|&t| {
                let cog = cog.clone();
                tokio::spawn(async move { cog.fetch_rgba(t, 512).await })
            })
            .collect();
        for task in tasks {
            assert!(task.await.unwrap().unwrap().is_some());
        }
    });
    // 4 x 4 map tiles of 512 px = 2048 px = the whole level 0: 8 x 8
    // internal tiles, each decoded exactly once despite 16 map tiles
    // sharing their edges.
    assert_eq!(cog.stats().tiles_decoded, 64, "{:?}", cog.stats());
    let requests = log.lock().unwrap().ranges.len() - 1;
    // Coalescing needs the 16 requests to overlap in time; on a loaded
    // runner (GPUI_MAP_SKIP_TIMING=1) they may not.
    if !std::env::var("GPUI_MAP_SKIP_TIMING").is_ok_and(|v| v == "1") {
        assert!(requests < 16, "coalesced: {requests} requests for 64 tiles over 16 map tiles");
    }

    // Uncoalesced, the ranges are exactly the tiles: none fetched twice.
    let (url, log) = serve(dem(), true, 200);
    let cog = rt.block_on(CogSource::open(&url, CogOptions { merge_gap: 0, max_request_bytes: 1, ..CogOptions::default() })).unwrap();
    rt.block_on(async {
        let tasks: Vec<_> = tiles
            .iter()
            .map(|&t| {
                let cog = cog.clone();
                tokio::spawn(async move { cog.render_tile(t).await })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
    });
    let mut ranges: Vec<(u64, u64)> = log.lock().unwrap().ranges.iter().skip(1).map(|r| r.expect("a range")).collect();
    assert_eq!(ranges.len(), 64);
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        assert!(pair[0].1 < pair[1].0, "a tile fetched twice: {pair:?}");
    }
}

#[test]
fn a_server_that_ignores_range_is_read_whole_once() {
    let file = dem();
    let (url, log) = serve(file, false, 200);
    let rt = runtime();
    let cog = rt.block_on(CogSource::open(&url, CogOptions::default())).unwrap();
    rt.block_on(cog.render_tile(TileId { z: 6, x: 32, y: 20 })).unwrap().expect("inside");
    rt.block_on(cog.render_tile(TileId { z: 7, x: 65, y: 41 })).unwrap().expect("inside");
    assert_eq!(log.lock().unwrap().ranges.len(), 1, "the 200 is kept and sliced");
    // Refused past the whole-file limit.
    let (url, _) = serve(dem(), false, 200);
    let err = rt.block_on(CogSource::open(&url, CogOptions { max_whole_file_bytes: 1000, ..CogOptions::default() })).unwrap_err();
    assert!(matches!(err, CogError::TooLarge { limit: 1000, .. }), "{err}");
}

#[test]
fn a_non_2xx_is_an_error_that_names_the_status_not_the_url() {
    let (url, _) = serve(vec![0; 16], true, 404);
    let err = runtime().block_on(CogSource::open(&url, CogOptions::default())).unwrap_err();
    assert_eq!(err, CogError::Http("HTTP 404".into()));
    assert!(!err.to_string().contains("127.0.0.1"));
}
