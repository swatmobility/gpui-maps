//! `FgbSource::from_url` and `FgbRemote` against a local HTTP server that
//! answers `Range` requests: a viewport query reads the index levels and
//! the features in view, not the file; a second query of the same view
//! fetches nothing new; a server that ignores `Range` still works; a
//! non-2xx is an error. Here rather than in `src/` because the library is
//! held to zero listening sockets (`agent::tests`), and a test server is
//! one.
#![cfg(feature = "sprite_http_fetch")]

use std::collections::HashSet;
use std::io::{BufRead as _, BufReader, Write as _};
use std::sync::{Arc, Mutex};

use gpui_maps::overlay::csv::CsvValues;
use gpui_maps::overlay::fgb::write::{write, WriteFeature, WriteType, WriteValue};
use gpui_maps::overlay::fgb::{FgbError, FgbOptions, FgbRemote, FgbSource, FGB_USER_AGENT};
use gpui_maps::overlay::OverlayGeometry;

/// What the server saw: each request's range (`None`: no `Range`) and
/// User-Agent.
#[derive(Default)]
struct Log {
    ranges: Vec<Option<(u64, u64)>>,
    agents: Vec<String>,
    encodings: Vec<String>,
}

/// Serves `file` until the test ends: `Range: bytes=a-b` / `a-` gets a 206
/// with `Content-Range`, unless `honour_range` is off (then a 200 of the
/// whole file). `status` other than 200 answers every request with it.
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
                let mut encoding = String::new();
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
                    if let Some(v) = lower.strip_prefix("accept-encoding:") {
                        encoding = v.trim().to_string();
                    }
                    if lower.starts_with("user-agent:") {
                        agent = line["user-agent:".len()..].trim().to_string();
                    }
                }
                {
                    let mut seen = seen.lock().unwrap();
                    seen.ranges.push(range);
                    seen.agents.push(agent);
                    seen.encodings.push(encoding);
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
    (format!("http://{addr}/data.fgb"), log)
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

/// `n` points spread over the world, each with its `id`.
fn points(n: usize) -> (Vec<[f64; 2]>, Vec<u8>) {
    let mut r = Rng(42);
    let at: Vec<[f64; 2]> = (0..n).map(|_| [-180.0 + r.next() * 360.0, -85.0 + r.next() * 170.0]).collect();
    let features: Vec<WriteFeature> = at
        .iter()
        .enumerate()
        .map(|(i, p)| WriteFeature { geometry: OverlayGeometry::Point(vec![(p[0], p[1])]), properties: vec![WriteValue::Long(i as i64)] })
        .collect();
    let bytes = write(&[("id", WriteType::Long)], &features, 16);
    (at, bytes)
}

fn ids(source: &FgbSource) -> Vec<i64> {
    let Some(CsvValues::Number(ids)) = source.column("id") else { panic!("an id column") };
    let mut ids: Vec<i64> = ids.iter().map(|v| v.unwrap() as i64).collect();
    ids.sort_unstable();
    ids
}

fn inside(at: &[[f64; 2]], b: [f64; 4]) -> Vec<i64> {
    (0..at.len()).filter(|&i| at[i][0] >= b[0] && at[i][0] <= b[2] && at[i][1] >= b[1] && at[i][1] <= b[3]).map(|i| i as i64).collect()
}

#[test]
fn a_viewport_query_reads_the_index_and_the_features_in_view_by_range() {
    let (at, file) = points(200_000);
    let (url, log) = serve(file.clone(), true, 200);
    let mut remote = FgbRemote::open(&url, &FgbOptions::default()).unwrap();
    assert_eq!(remote.file_len(), Some(file.len() as u64));
    assert_eq!(remote.header().features_count, 200_000);

    let view = [2.0, 45.0, 8.0, 50.0];
    let (got, stats) = remote.query(view, &HashSet::new()).unwrap();
    assert_eq!(ids(&got), inside(&at, view));
    assert_eq!(stats.fetched, got.len());
    assert!(stats.bytes * 50 < file.len() as u64, "a query read {} of {} bytes", stats.bytes, file.len());
    // One read per index level (5) plus the features' merged ranges.
    assert!(stats.requests <= 10, "{} requests", stats.requests);

    // The same view again, with those features held: the index is walked,
    // no feature is fetched.
    let loaded: HashSet<u64> = (0..got.len()).filter_map(|i| got.file_index(i)).collect();
    let (again, stats) = remote.query(view, &loaded).unwrap();
    assert!(again.is_empty());
    assert_eq!((stats.hits, stats.already_loaded, stats.fetched), (got.len(), got.len(), 0));

    let log = log.lock().unwrap();
    assert!(log.ranges.iter().all(Option::is_some), "every request is a range request");
    assert!(log.agents.iter().all(|a| a == FGB_USER_AGENT), "{:?}", log.agents.first());
    // A range of the gzipped representation would not line up with the
    // index: every range request asks for the bytes as stored.
    assert!(log.encodings.iter().all(|e| e == "identity"), "{:?}", log.encodings);
}

#[test]
fn a_bbox_from_url_is_exact_and_a_whole_one_matches_the_bytes() {
    let (at, file) = points(5_000);
    let (url, _) = serve(file.clone(), true, 200);
    let bbox = [-20.0, -10.0, 30.0, 20.0];
    let some = FgbSource::from_url(&url, &FgbOptions::default().with_bbox(bbox)).unwrap();
    assert_eq!(ids(&some), inside(&at, bbox));
    let whole = FgbSource::from_url(&url, &FgbOptions::default()).unwrap();
    let bytes = FgbSource::from_bytes(&file, &FgbOptions::default()).unwrap();
    assert_eq!(ids(&whole), ids(&bytes));
    assert_eq!(whole.len(), 5_000);
}

#[test]
fn a_server_that_ignores_range_is_read_whole_once() {
    let (at, file) = points(3_000);
    let (url, log) = serve(file, false, 200);
    let bbox = [0.0, 0.0, 90.0, 45.0];
    let some = FgbSource::from_url(&url, &FgbOptions::default().with_bbox(bbox)).unwrap();
    assert_eq!(ids(&some), inside(&at, bbox));
    assert_eq!(log.lock().unwrap().ranges.len(), 1, "the 200's body serves every later read");
}

#[test]
fn a_missing_file_is_an_http_error_and_a_too_large_one_is_refused() {
    let (url, _) = serve(vec![0; 16], true, 404);
    assert_eq!(FgbSource::from_url(&url, &FgbOptions::default()).unwrap_err(), FgbError::Http("HTTP 404".into()));
    let (_, file) = points(3_000);
    let (url, _) = serve(file, false, 200);
    let small = FgbOptions { max_bytes: 1000, ..FgbOptions::default() };
    assert!(matches!(FgbSource::from_url(&url, &small), Err(FgbError::TooLarge { limit: 1000, .. })));
}
