//! `GeoParquetSource::from_url` and `GeoParquetRemote` against a local HTTP
//! server that answers `Range` requests: opening reads the magic and the
//! footer; a viewport query reads only the column chunks of the row groups
//! the box reaches; a second query of the same view fetches nothing; a
//! server that ignores `Range` still works; a non-2xx is an error. Here
//! rather than in `src/` because the library is held to zero listening
//! sockets (`agent::tests`), and a test server is one.
#![cfg(all(feature = "sprite_http_fetch", feature = "geoparquet"))]

use std::collections::HashSet;
use std::io::{BufRead as _, BufReader, Write as _};
use std::sync::{Arc, Mutex};

use gpui_maps::overlay::csv::CsvValues;
use gpui_maps::overlay::geoparquet::write::{write, WriteFeature, WriteOptions, WriteType, WriteValue};
use gpui_maps::overlay::geoparquet::{
    GeoParquetError, GeoParquetOptions, GeoParquetPushdown, GeoParquetRemote, GeoParquetSource, GEOPARQUET_USER_AGENT,
};
use gpui_maps::overlay::OverlayGeometry;

#[derive(Default)]
struct Log {
    ranges: Vec<Option<(u64, u64)>>,
    agents: Vec<String>,
    /// Requests being answered now, and the most there ever were at once.
    in_flight: usize,
    max_in_flight: usize,
}

/// Serves `file`: `Range: bytes=a-b` gets a 206 with `Content-Range`,
/// unless `honour_range` is off (then a 200 of the whole file). `status`
/// other than 200 answers every request with it.
fn serve(file: Vec<u8>, honour_range: bool, status: u16) -> (String, Arc<Mutex<Log>>) {
    serve_with(file, honour_range, status, std::time::Duration::ZERO)
}

/// [`serve`], each request held `latency` before it is answered (a remote
/// bucket's round trip), counting how many are in flight at once.
fn serve_with(file: Vec<u8>, honour_range: bool, status: u16, latency: std::time::Duration) -> (String, Arc<Mutex<Log>>) {
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
                    seen.in_flight += 1;
                    seen.max_in_flight = seen.max_in_flight.max(seen.in_flight);
                }
                std::thread::sleep(latency);
                seen.lock().unwrap().in_flight -= 1;
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
    (format!("http://{addr}/data.parquet"), log)
}

/// `n` points sorted by longitude into row groups of 2000 (as a spatially
/// sorted export is), each with its `id` and a text column.
fn points(n: usize) -> (Vec<[f64; 2]>, Vec<u8>) {
    let at: Vec<[f64; 2]> = (0..n)
        .map(|i| {
            let t = i as f64 / n as f64;
            [-180.0 + t * 360.0, -60.0 + ((i * 7919) % 1200) as f64 / 10.0]
        })
        .collect();
    let features: Vec<WriteFeature> = at
        .iter()
        .enumerate()
        .map(|(i, p)| WriteFeature {
            geometry: OverlayGeometry::Point(vec![(p[0], p[1])]),
            properties: vec![WriteValue::Int(Some(i as i64)), WriteValue::Str(Some(format!("place {i}")))],
        })
        .collect();
    let bytes = write(&[("id", WriteType::Int), ("name", WriteType::Str)], &features, &WriteOptions { row_group_rows: 2000, ..WriteOptions::default() })
        .unwrap();
    (at, bytes)
}

fn ids(source: &GeoParquetSource) -> Vec<i64> {
    let Some(CsvValues::Number(ids)) = source.column("id") else { panic!("an id column") };
    let mut ids: Vec<i64> = ids.iter().map(|v| v.unwrap() as i64).collect();
    ids.sort_unstable();
    ids
}

fn inside(at: &[[f64; 2]], b: [f64; 4]) -> Vec<i64> {
    (0..at.len()).filter(|&i| at[i][0] >= b[0] && at[i][0] <= b[2] && at[i][1] >= b[1] && at[i][1] <= b[3]).map(|i| i as i64).collect()
}

#[test]
fn a_viewport_query_reads_the_footer_and_the_row_groups_in_view_by_range() {
    let (at, file) = points(200_000);
    let (url, log) = serve(file.clone(), true, 200);
    let mut remote = GeoParquetRemote::open(&url, &GeoParquetOptions::default()).unwrap();
    assert_eq!(remote.file_len(), file.len() as u64);
    assert_eq!(remote.rows_total(), 200_000);
    assert_eq!(remote.row_groups(), 100);
    assert_eq!(remote.pushdown(), GeoParquetPushdown::Covering);
    let (open_bytes, open_requests) = remote.counters();
    // The magic (which also gives the length), the last 64 KiB, and the
    // rest of a footer that is larger (100 row groups' statistics).
    assert_eq!(open_requests, 3, "magic, tail, rest of the footer");

    let view = [2.0, 45.0, 8.0, 50.0];
    let (got, stats) = remote.query(view, &HashSet::new()).unwrap();
    assert_eq!(ids(&got), inside(&at, view));
    // 6 degrees of 360 over 100 groups of 3.6 degrees: 2 or 3 groups.
    assert!((2..=3).contains(&stats.row_groups_hit), "{stats:?}");
    assert_eq!(stats.row_groups_skipped, 100 - stats.row_groups_hit);
    assert!(stats.requests <= 3, "{stats:?}");
    assert!(stats.bytes * 20 < file.len() as u64, "{} of {}", stats.bytes, file.len());
    assert!(open_bytes < 512 << 10, "{open_bytes}");

    // The same view again, its rows held: nothing to fetch.
    let loaded: HashSet<u64> = (0..got.len()).filter_map(|i| got.file_row(i)).collect();
    let (again, stats) = remote.query(view, &loaded).unwrap();
    assert!(again.is_empty());
    assert_eq!(stats.fetched, 0);
    // Groups are only partly in view, so they are read again but their
    // rows in view are passed over.
    assert!(stats.already_loaded >= got.len(), "{stats:?}");

    // Whole groups held are not even requested.
    let all: HashSet<u64> = (0..200_000).collect();
    let (none, stats) = remote.query(view, &all).unwrap();
    assert!(none.is_empty());
    assert_eq!((stats.requests, stats.row_groups_resident), (0, stats.row_groups_hit));

    let log = log.lock().unwrap();
    assert!(log.ranges.iter().all(Option::is_some), "every request is a range");
    assert!(log.agents.iter().all(|a| a == GEOPARQUET_USER_AGENT), "{:?}", log.agents);
}

#[test]
fn from_url_matches_from_bytes_and_projects_columns() {
    let (_, file) = points(20_000);
    let (url, _) = serve(file.clone(), true, 200);
    let whole = GeoParquetSource::from_url(&url, &GeoParquetOptions::default()).unwrap();
    let local = GeoParquetSource::from_bytes(&file, &GeoParquetOptions::default()).unwrap();
    assert_eq!(ids(&whole), ids(&local));
    assert_eq!(whole.columns().len(), 2);
    let only_id = GeoParquetSource::from_url(&url, &GeoParquetOptions::default().with_columns(["id"])).unwrap();
    assert_eq!(only_id.columns().len(), 1);
    assert!(only_id.report().bytes_read < whole.report().bytes_read, "{} vs {}", only_id.report().bytes_read, whole.report().bytes_read);
}

#[test]
fn a_server_that_ignores_range_is_read_whole() {
    let (at, file) = points(5_000);
    let (url, log) = serve(file, false, 200);
    let view = [-10.0, -60.0, 10.0, 60.0];
    let got = GeoParquetSource::from_url(&url, &GeoParquetOptions::default().with_bbox(view)).unwrap();
    assert_eq!(ids(&got), inside(&at, view));
    assert_eq!(log.lock().unwrap().ranges.len(), 1, "one 200, sliced from then on");
}

#[test]
fn a_non_2xx_is_an_error() {
    let (url, _) = serve(b"PAR1".to_vec(), true, 404);
    let e = GeoParquetSource::from_url(&url, &GeoParquetOptions::default()).unwrap_err();
    assert!(matches!(&e, GeoParquetError::Http(m) if m.contains("404")), "{e}");
}

/// A file whose projected columns are far apart: `id` beside a wide,
/// incompressible `pad` column the read does not project, 2,000 rows a
/// group -- so each group's `id` and geometry chunks are separate ranges
/// (the gap is past the merge distance), as an Overture read of a few
/// columns of many is.
fn sparse_columns(n: usize) -> Vec<u8> {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let features: Vec<WriteFeature> = (0..n)
        .map(|i| {
            let t = i as f64 / n as f64;
            let pad: String = (0..6).map(|_| format!("{:016x}", noise())).collect();
            WriteFeature {
                geometry: OverlayGeometry::Point(vec![(-180.0 + t * 360.0, -60.0 + ((i * 7919) % 1200) as f64 / 10.0)]),
                properties: vec![WriteValue::Int(Some(i as i64)), WriteValue::Str(Some(pad))],
            }
        })
        .collect();
    write(&[("id", WriteType::Int), ("pad", WriteType::Str)], &features, &WriteOptions { row_group_rows: 2000, ..WriteOptions::default() })
        .unwrap()
}

/// **CONCURRENT RANGES.** A whole read of a sparse projection at 20 ms a
/// request: the column chunks are coalesced and fetched several at once,
/// not one after another (before: every request alone, ~N x 20 ms).
#[test]
fn column_chunks_are_fetched_concurrently_at_a_round_trip_each() {
    let file = sparse_columns(40_000);
    let (url, log) = serve_with(file.clone(), true, 200, std::time::Duration::from_millis(20));
    let options = GeoParquetOptions::default().with_columns(["id"]);
    let mut remote = GeoParquetRemote::open(&url, &options).unwrap();
    let (_, open_requests) = remote.counters();
    log.lock().unwrap().max_in_flight = 0;
    let started = std::time::Instant::now();
    let got = remote.read(None).unwrap();
    let elapsed = started.elapsed();
    let (bytes, requests) = remote.counters();
    let requests = requests - open_requests;
    let max_in_flight = log.lock().unwrap().max_in_flight;
    eprintln!(
        "geoparquet sparse read: {} rows, {requests} requests, {:.2} MB, {:.0} ms, at most {max_in_flight} in flight",
        got.len(),
        bytes as f64 / 1e6,
        elapsed.as_secs_f64() * 1e3
    );
    assert_eq!(got.len(), 40_000);
    let local = GeoParquetSource::from_bytes(&file, &options).unwrap();
    assert_eq!(ids(&got), ids(&local));
    assert!(requests >= 8, "the fixture must need several requests: {requests}");
    assert!(max_in_flight > 1, "{requests} requests went one at a time");
    assert!(max_in_flight <= 8, "at most 8 at once: {max_in_flight}");
}
