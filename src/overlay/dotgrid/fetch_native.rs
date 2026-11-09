//! The native (non-wasm) [`GridRequest`] executor: turns a request into
//! bytes, blocking, so it can be called from an ordinary worker thread
//! rather than driven through an async runtime.
//!
//! `#[cfg(not(target_arch = "wasm32"))]` at the `mod fetch_native;`
//! declaration in `overlay::dotgrid::mod.rs` -- not `target_os = "macos"`
//! -- because `ios/Cargo.toml` builds this whole crate's `src/` by
//! `#[path]` include (see `overlay::dotgrid`'s own module doc) both for
//! the real `--target aarch64-apple-ios` device build AND for a plain
//! host-side `cargo build --manifest-path ios/Cargo.toml` sanity check,
//! and `target_os` on THAT build reads `"macos"`, not `"ios"` -- exactly
//! the reasoning `sprite.rs`'s own `fetch_sprite_atlas` gating documents.
//! `target_arch` is the one thing that reads the same regardless of which
//! of those two builds compiles it.
//!
//! Reading a local file works on every non-wasm shell (native and iOS
//! alike -- both link `std::fs`), so that half of [`execute`] has no
//! further gate. The http(s) half needs `reqwest`, which `ios/Cargo.toml`
//! and `web/Cargo.toml` both deliberately do NOT declare (iOS fetches over
//! `ureq` on its own worker pool instead -- see `ios/src/net.rs`), so it
//! is behind `feature = "sprite_http_fetch"` too -- the same feature (on
//! by default here, declared and left off in both shells' manifests) that
//! already gates `sprite.rs`'s own `reqwest`-touching functions, reused
//! rather than duplicated because it is exactly the same constraint.

use std::io::{Read, Seek, SeekFrom};

use super::pmtiles_source::{PmtilesGridSource, PmtilesOpener};
use super::{GridError, GridRequest};

/// Executes one [`GridRequest`], blocking. Call this from a worker thread,
/// never from a frame callback.
pub fn execute(req: &GridRequest) -> Result<Vec<u8>, GridError> {
    execute_within(req, REQUEST_TIMEOUT)
}

/// [`execute`] with the http(s) request's whole-request timeout stated: a
/// test can prove a server that never answers is [`GridError::Timeout`]
/// without waiting the default out, and a ClickHouse source's startup
/// reads give up in seconds rather than the tile default.
pub fn execute_within(req: &GridRequest, timeout: std::time::Duration) -> Result<Vec<u8>, GridError> {
    match req {
        GridRequest::Absent => Ok(Vec::new()),
        GridRequest::Range { location, offset, length } => {
            if is_url(location) {
                http_range(location, *offset, *length, timeout)
            } else {
                read_range(location, *offset, *length)
            }
        }
        GridRequest::Post { url, body, headers } => http_post(url, body, headers, timeout),
        GridRequest::Get { url } => http_get(url, timeout),
    }
}

/// Opens a `dotgrid` PMTiles archive at `location` (a local path or an
/// http(s) URL), blocking. Drives [`PmtilesOpener`] to completion by
/// [`execute`]-ing whatever it asks for -- the first fetch, then up to two
/// more, independent follow-up requests (never one spanning both), see
/// `pmtiles_source`'s own module doc.
pub fn open_pmtiles_grid(location: &str) -> Result<PmtilesGridSource, GridError> {
    let mut opener = PmtilesOpener::new(location);
    let first = execute(&opener.first_request())?;
    let requests = opener.after_first(&first)?;
    let mut bodies = Vec::with_capacity(requests.len());
    for req in &requests {
        bodies.push(execute(req)?);
    }
    opener.finish(&bodies)
}

/// Opens the race map's hex archive (`race-hex-2020.pmtiles`) at
/// `location`, blocking: the same three steps as [`open_pmtiles_grid`],
/// each request run by [`execute`].
pub fn open_pmtiles_hex(location: &str) -> Result<crate::overlay::hexes::source::PmtilesHexSource, GridError> {
    let mut opener = crate::overlay::hexes::source::PmtilesHexOpener::new(location);
    let first = execute(&opener.first_request())?;
    let requests = opener.after_first(&first)?;
    let mut bodies = Vec::with_capacity(requests.len());
    for req in &requests {
        bodies.push(execute(req)?);
    }
    opener.finish(&bodies)
}

fn is_url(location: &str) -> bool {
    location.starts_with("http://") || location.starts_with("https://")
}

/// Reads exactly `length` bytes starting at `offset` from a local file.
/// Anything short of `length` bytes -- the range reached past the file's
/// end -- is [`GridError::Truncated`], never a padded/short buffer handed
/// back as if it were whole.
fn read_range(path: &str, offset: u64, length: u64) -> Result<Vec<u8>, GridError> {
    let mut file = std::fs::File::open(path).map_err(|e| GridError::Io(e.to_string()))?;
    // The claimed length is untrusted (an archive's directory says it): a
    // range the file cannot hold is Truncated before a buffer is sized.
    let file_len = file.metadata().map_err(|e| GridError::Io(e.to_string()))?.len();
    if offset.checked_add(length).is_none_or(|end| end > file_len) {
        return Err(GridError::Truncated);
    }
    file.seek(SeekFrom::Start(offset)).map_err(|e| GridError::Io(e.to_string()))?;
    let mut buf = vec![0u8; length as usize];
    match file.read_exact(&mut buf) {
        Ok(()) => Ok(buf),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(GridError::Truncated),
        Err(e) => Err(GridError::Io(e.to_string())),
    }
}

/// The shared blocking runtime `http_range`/`http_post` drive `reqwest`'s
/// async client through. A fresh `tokio::runtime::Runtime` per call would
/// work too, but this is called once per tile on a fetch worker, and a
/// runtime's own worker threads are not free to stand up repeatedly.
#[cfg(feature = "sprite_http_fetch")]
fn http_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Runtime::new().expect("failed to start the blocking count-tile fetch runtime")
    })
}

/// A whole request -- connect, send, and the body -- that takes longer
/// than this is [`GridError::Timeout`]: a server that accepts and never
/// answers would otherwise hold one of the layer's four request slots
/// forever, and the map would say nothing about why no dots appear.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Connecting alone.
#[cfg(feature = "sprite_http_fetch")]
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(feature = "sprite_http_fetch")]
fn http_client(timeout: std::time::Duration) -> Result<reqwest::Client, GridError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .build()
        .map_err(|e| GridError::Server(e.to_string()))
}

/// A `reqwest` failure: [`GridError::Timeout`] when it timed out.
///
/// Its text is redacted (`sprite::redact_url_credentials`): `reqwest::Error`
/// prints the request URL, and an archive URL may carry an access token, a
/// `password=` or `user:pass@`.
///
/// Its causes are appended (`reqwest::Error` alone prints "error sending
/// request", not "Connection refused"), so the screen can say why.
#[cfg(feature = "sprite_http_fetch")]
fn http_error(e: reqwest::Error) -> GridError {
    let mut text = e.to_string();
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        text = format!("{text}: {c}");
        cause = c.source();
    }
    let text = crate::sprite::redact_url_credentials(&text);
    if e.is_timeout() {
        GridError::Timeout(text)
    } else {
        GridError::Server(text)
    }
}

#[cfg(feature = "sprite_http_fetch")]
fn http_range(url: &str, offset: u64, length: u64, timeout: std::time::Duration) -> Result<Vec<u8>, GridError> {
    let client = http_client(timeout)?;
    http_runtime().block_on(async {
        let end = offset + length.saturating_sub(1);
        let response = client
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes={offset}-{end}"))
            .send()
            .await
            .map_err(http_error)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(GridError::http(status, &body));
        }
        // A server that ignores `Range` answers 200 with the whole
        // archive: refuse it rather than stream it into memory.
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(GridError::Server(format!(
                "HTTP {} for a byte range: the server ignored the Range header",
                response.status().as_u16()
            )));
        }
        let too_long = |got: u64| GridError::Server(format!("a {length}-byte range came back with {got} bytes"));
        if let Some(got) = response.content_length().filter(|&got| got > length) {
            return Err(too_long(got));
        }
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(http_error)? {
            if (body.len() + chunk.len()) as u64 > length {
                return Err(too_long((body.len() + chunk.len()) as u64));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    })
}

#[cfg(not(feature = "sprite_http_fetch"))]
fn http_range(url: &str, _offset: u64, _length: u64, _timeout: std::time::Duration) -> Result<Vec<u8>, GridError> {
    Err(GridError::Server(format!("no HTTP client compiled in for {}", crate::sprite::redact_url_credentials(url))))
}

/// The most a POST (ClickHouse) response body may be: 64 MiB. MG-14's
/// densest 8-tile batch is about 6.2 MB (8 B rows), so this is 10x
/// headroom -- and a bound on what one untrusted server can make a worker
/// allocate, as `http_range` bounds a range by its requested length.
pub const MAX_POST_BODY_BYTES: u64 = super::shell_fetch::MAX_BODY_BYTES;

/// A POST (ClickHouse). A response carrying `X-ClickHouse-Exception-Code`
/// is [`GridError::Server`] with the server's text, whatever its status:
/// with `wait_end_of_query=1` that is how ClickHouse reports a failed
/// query, and its body is an exception, never data.
#[cfg(feature = "sprite_http_fetch")]
fn http_post(url: &str, body: &str, headers: &[(String, String)], timeout: std::time::Duration) -> Result<Vec<u8>, GridError> {
    let client = http_client(timeout)?;
    let mut request = client.post(url).body(body.to_string());
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    http_bounded(request)
}

/// A whole-body GET (the browser's ClickHouse proxy, `GridRequest::Get`):
/// the same exception check and [`MAX_POST_BODY_BYTES`] bound as a POST,
/// and the proxy's required header (`shell_fetch::PROXY_HEADER`).
#[cfg(feature = "sprite_http_fetch")]
fn http_get(url: &str, timeout: std::time::Duration) -> Result<Vec<u8>, GridError> {
    let client = http_client(timeout)?;
    let (name, value) = super::shell_fetch::PROXY_HEADER;
    http_bounded(client.get(url).header(name, value))
}

/// Sends `request` and reads its body, bounded by [`MAX_POST_BODY_BYTES`].
#[cfg(feature = "sprite_http_fetch")]
fn http_bounded(request: reqwest::RequestBuilder) -> Result<Vec<u8>, GridError> {
    http_runtime().block_on(async {
        // `without_url`: a ClickHouse URL can carry `user=`/`password=`
        // arguments or `user:pass@`, which `redact_access_token` does not know.
        let response = request.send().await.map_err(|e| http_error(e.without_url()))?;
        if let Some(code) = response.headers().get("x-clickhouse-exception-code") {
            let code = code.to_str().unwrap_or("?").to_string();
            let text = response.text().await.unwrap_or_default();
            return Err(GridError::Server(format!("ClickHouse exception {code}: {}", super::bounded_error_text(&text))));
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(GridError::http(status, &body));
        }
        let too_long = |got: u64| GridError::Server(format!("a {got}-byte response is over the {MAX_POST_BODY_BYTES} B limit"));
        if let Some(got) = response.content_length().filter(|&got| got > MAX_POST_BODY_BYTES) {
            return Err(too_long(got));
        }
        let mut response = response;
        let mut out = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| http_error(e.without_url()))? {
            if (out.len() + chunk.len()) as u64 > MAX_POST_BODY_BYTES {
                return Err(too_long((out.len() + chunk.len()) as u64));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    })
}

#[cfg(not(feature = "sprite_http_fetch"))]
fn http_get(url: &str, _timeout: std::time::Duration) -> Result<Vec<u8>, GridError> {
    Err(GridError::Server(format!("no HTTP client compiled in for {}", crate::sprite::redact_url_credentials(url))))
}

#[cfg(not(feature = "sprite_http_fetch"))]
fn http_post(url: &str, _body: &str, _headers: &[(String, String)], _timeout: std::time::Duration) -> Result<Vec<u8>, GridError> {
    Err(GridError::Server(format!("no HTTP client compiled in for {}", crate::sprite::redact_url_credentials(url))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_of_a_local_file_reads_exactly_those_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"0123456789").unwrap();

        let bytes = execute(&GridRequest::Range {
            location: path.to_string_lossy().to_string(),
            offset: 3,
            length: 4,
        })
        .unwrap();
        assert_eq!(bytes, b"3456");
    }

    #[test]
    fn a_short_file_is_truncated_not_padded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"short").unwrap();

        let result = execute(&GridRequest::Range {
            location: path.to_string_lossy().to_string(),
            offset: 0,
            length: 100,
        });
        assert!(matches!(result, Err(GridError::Truncated)), "expected Truncated, got {result:?}");
    }

    /// Fix round 3: a failed range request from a tokenised archive URL
    /// does not print the token (`reqwest::Error` prints the URL).
    #[test]
    fn a_failed_http_range_never_carries_the_access_token() {
        let req = GridRequest::Range { location: "http://127.0.0.1:9/w.pmtiles?access_token=SECRET".into(), offset: 0, length: 16 };
        let err = execute_within(&req, std::time::Duration::from_secs(5)).expect_err("nothing listens on port 9");
        let text = format!("{err} {err:?}");
        assert!(!text.contains("SECRET"), "the token leaked: {text}");
    }

    /// Task 11: a ClickHouse URL's `user:pass@` or `password=` never
    /// reaches the error text (the POST drops the URL entirely).
    #[test]
    fn a_failed_post_never_carries_credentials() {
        let req = GridRequest::Post { url: "http://user:pw1@127.0.0.1:9/?password=pw2".into(), body: "SELECT 1".into(), headers: Vec::new() };
        let err = execute_within(&req, std::time::Duration::from_secs(5)).expect_err("nothing listens on port 9");
        let text = format!("{err} {err:?}");
        assert!(!text.contains("pw1") && !text.contains("pw2"), "a credential leaked: {text}");
    }

    /// Final review I1: an archive URL's `user:pass@` never reaches a
    /// failed range's error either.
    #[test]
    fn a_failed_http_range_never_carries_userinfo() {
        let req = GridRequest::Range { location: "http://user:SECRET@127.0.0.1:9/w.pmtiles?password=SECRET".into(), offset: 0, length: 16 };
        let err = execute_within(&req, std::time::Duration::from_secs(5)).expect_err("nothing listens on port 9");
        let text = format!("{err} {err:?}");
        assert!(!text.contains("SECRET"), "a credential leaked: {text}");
    }

    /// Final review I4: a local range reaching past the file's end is
    /// Truncated before a buffer is sized from the claimed length (it was
    /// `vec![0u8; length as usize]`).
    #[test]
    fn a_local_range_claiming_more_than_the_file_is_truncated_without_allocating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"0123456789").unwrap();
        for (offset, length) in [(0, u64::MAX), (4, u64::MAX - 2), (u64::MAX, 1), (9, 2)] {
            let result = execute(&GridRequest::Range { location: path.to_string_lossy().to_string(), offset, length });
            assert!(matches!(result, Err(GridError::Truncated)), "offset {offset} length {length}: {result:?}");
        }
    }

    #[test]
    fn an_absent_request_reads_nothing() {
        assert_eq!(execute(&GridRequest::Absent).unwrap(), Vec::<u8>::new());
    }

    /// Review F1: the browser's fetch deadline is native's.
    #[test]
    fn the_browser_fetch_deadline_is_natives() {
        assert_eq!(REQUEST_TIMEOUT, std::time::Duration::from_millis(u64::from(super::super::shell_fetch::REQUEST_TIMEOUT_MS)));
    }

    /// The real race archive (Task 5's 786 MB national build) through the
    /// native fetcher: its metadata opens as C7 says, and a part tile over
    /// the Chicago Loop decodes with parts. Needs
    /// `~/.cache/gpui-map-data/census2020/race-dots-2020.pmtiles`.
    fn race_archive() -> String {
        std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(".cache/gpui-map-data/census2020/race-dots-2020.pmtiles")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    #[ignore = "reads the national race archive"]
    fn the_real_race_archive_opens_and_serves_part_tiles() {
        use crate::geo::TileId;
        use crate::overlay::dotgrid::{GridCountSource, GridTileKey};
        let source = open_pmtiles_grid(&race_archive()).expect("the race archive opens");
        let race = source.race().expect("C7's race key");
        assert_eq!((race.part_zoom, race.crossover, race.groups), (12, 9.72, 8));
        let (x, y) = crate::geo::lon_lat_to_tile_xy(-87.63, 41.88, 12.0);
        let key = GridTileKey::part(TileId { z: 12, x: x as u32, y: y as u32 }, 2020);
        let body = execute(&source.request(&[key])).unwrap();
        let tile = source.decode(&[key], &body).unwrap().remove(0).1;
        eprintln!("{key:?}: {} parts, {} people", tile.parts.len(), tile.total);
        assert!(tile.parts.len() > 100 && tile.total > 1000.0);
    }

    /// The real race archive through `DotLayers`: over the Chicago Loop at
    /// z12.5 the layer plans part tiles, loads them, draws categorical
    /// triangles with the cell terms off and counts the people in view; a
    /// zero mask keeps the ppd; at z9 it draws cells.
    #[test]
    #[ignore = "reads the national race archive"]
    fn the_real_race_archive_draws_parts_above_the_crossover() {
        use crate::geo::TileId;
        use crate::overlay::dotgrid::layer::{DotLayers, DotStyle, Platform};
        let source = open_pmtiles_grid(&race_archive()).expect("the race archive");
        let mut layers = DotLayers::new();
        layers.set_source("race", std::sync::Arc::new(source), DotStyle::race(Platform::Native));
        let (cx, cy) = crate::geo::lon_lat_to_tile_xy(-87.63, 41.88, 0.0);
        let run = |layers: &mut DotLayers, zoom: f64| {
            let half = 0.5 / f64::from(1u32 << 12);
            let view = ([cx - half, cy - half], [cx + half, cy + half]);
            let cover_at = |z: u8| -> Vec<TileId> {
                let n = f64::from(1u32 << z);
                let (x0, y0, x1, y1) = ((view.0[0] * n) as u32, (view.0[1] * n) as u32, (view.1[0] * n) as u32, (view.1[1] * n) as u32);
                (y0..=y1).flat_map(|y| (x0..=x1).map(move |x| TileId { z, x, y })).collect()
            };
            for _ in 0..6 {
                layers.frame_with_cover(zoom, cover_at, view, crate::timing::Instant::now());
                while let Some(job) = layers.next_job() {
                    layers.land(job.run(execute));
                }
            }
            (layers.stats("race").unwrap(), layers.frame_params("race", zoom, 0.0, 0.0, None))
        };
        let (stats, params) = run(&mut layers, 12.5);
        eprintln!("z12.5: {} drawn tiles, ppd {}, {:.0} people in view, errors {}", params.len(), stats.ppd, stats.people_in_view, stats.source_errors);
        assert_eq!(stats.source_errors, 0);
        assert!(layers.plan("race").unwrap().draw.iter().any(|d| d.key.is_part()));
        assert!(!params.is_empty() && params.iter().all(|p| p.shape == crate::render::dots::SHAPE_TRIANGLE && p.cell_terms_off()));
        assert!(stats.people_in_view > 10_000.0, "{stats:?}");
        layers.set_category_mask("race", 0);
        let (masked, _) = run(&mut layers, 12.5);
        assert_eq!(masked.ppd, stats.ppd);
        let (cells, params) = run(&mut layers, 9.0);
        eprintln!("z9: {} drawn tiles, ppd {}, level {}", params.len(), cells.ppd, cells.level);
        assert!(!params.is_empty() && params.iter().all(|p| p.shape == crate::render::dots::SHAPE_CELL));
    }
}
