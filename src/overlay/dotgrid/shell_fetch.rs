//! The sans-IO half of a shell's count-tile fetcher (Task 16): what the
//! browser's `fetch` sends for a [`GridRequest`], how its reply is judged,
//! and which dot source the page's query string asks for. The web shell
//! (`web/src/app.rs`) has no test runner, so every decision it makes about
//! a fetch lives here, beside native's `fetch_native`, and is tested here.
//!
//! The rules are native's (`fetch_native::http_range` / `http_bounded`),
//! restated for a caller that reads the status before the body:
//! - a range answered with anything but `206 Partial Content` is refused
//!   before its body is read -- a server that ignores `Range` would
//!   otherwise stream the whole archive into the page;
//! - a range body longer than asked for is refused; a shorter one is kept
//!   (the opener's first read may run past a small archive's end, and
//!   the decoder refuses what it cannot use);
//! - a whole-body GET (the ClickHouse proxy) is bounded by
//!   [`MAX_BODY_BYTES`], and a body carrying a ClickHouse exception is an
//!   error, never rows;
//! - every fetch has native's deadline ([`REQUEST_TIMEOUT_MS`]): past it
//!   the shell aborts it and lands [`timed_out`], a source error that
//!   frees the layer's request slot and backs off like any other;
//! - a body is bounded WHILE it is read ([`BodyCap`]): a declared
//!   `Content-Length` over the bound is refused before the first byte,
//!   and a stream is refused at the chunk that crosses it;
//! - a proxy GET carries [`PROXY_HEADER`], which `web/serve.py` requires
//!   on every `/clickhouse/` request (a cross-site `<img>` or form cannot
//!   send a custom header without a CORS preflight, which the proxy never
//!   answers).

use super::{GridError, GridRequest};

/// The most a whole-body reply may be: native's `MAX_POST_BODY_BYTES`
/// (64 MiB, 10x MG-14's densest 8-tile batch).
pub const MAX_BODY_BYTES: u64 = 64 << 20;

/// A whole count-tile request -- connect, send, body -- that takes longer
/// is aborted: native's `fetch_native::REQUEST_TIMEOUT` (30 s), which
/// `fetch_native`'s own test holds equal to this. Without it four stalled
/// fetches hold all of a layer's request slots and the layer freezes with
/// no error.
pub const REQUEST_TIMEOUT_MS: u32 = 30_000;

/// The header every proxy request carries, and `web/serve.py` requires on
/// `/clickhouse/`: `(name, value)`.
pub const PROXY_HEADER: (&str, &str) = ("X-Gpui-Map-Proxy", "1");

/// The error a fetch aborted at [`REQUEST_TIMEOUT_MS`] lands as. The
/// layer counts it in `source_errors` (`last_error` "timeout") and backs
/// its keys off, as native's timeout.
#[must_use]
pub fn timed_out() -> GridError {
    GridError::Timeout(format!("no reply within {} s", REQUEST_TIMEOUT_MS / 1000))
}

/// The most error text a shell reads from a failed reply's body (it keeps
/// [`super::MAX_ERROR_TEXT`] of it): a bigger or undeclared one is not read.
pub const MAX_ERROR_BODY_BYTES: u64 = 64 << 10;

/// Whether a failed reply's text is worth reading, from its declared
/// `Content-Length`: only when declared and at most [`MAX_ERROR_BODY_BYTES`].
#[must_use]
pub fn read_error_text(content_length: Option<u64>) -> bool {
    content_length.is_some_and(|n| n <= MAX_ERROR_BODY_BYTES)
}

/// A body's bound, checked as it is read: [`BodyCap::declared`] before the
/// first byte, [`BodyCap::take`] per chunk. A refusal is the shell's cue
/// to abort the fetch.
#[derive(Debug, Clone, PartialEq)]
pub struct BodyCap {
    limit: u64,
    got: u64,
    range: bool,
}

impl BodyCap {
    /// The bound for `req`'s reply: a range's length (at least 1 byte,
    /// what [`range_header`] asks for), else [`MAX_BODY_BYTES`].
    #[must_use]
    pub fn for_request(req: &GridRequest) -> Self {
        match req {
            GridRequest::Range { length, .. } => Self { limit: (*length).max(1), got: 0, range: true },
            _ => Self { limit: MAX_BODY_BYTES, got: 0, range: false },
        }
    }

    fn over(&self, got: u64) -> GridError {
        if self.range {
            GridError::Server(format!("a {}-byte range came back with {got} bytes", self.limit))
        } else {
            GridError::Server(format!("a {got}-byte response is over the {} B limit", self.limit))
        }
    }

    /// A declared `Content-Length` over the bound: refused unread.
    pub fn declared(&self, content_length: Option<u64>) -> Result<(), GridError> {
        match content_length {
            Some(n) if n > self.limit => Err(self.over(n)),
            _ => Ok(()),
        }
    }

    /// One more chunk of `len` bytes: refused at the chunk that crosses
    /// the bound, before it is kept.
    pub fn take(&mut self, len: usize) -> Result<(), GridError> {
        let got = self.got + len as u64;
        if got > self.limit {
            return Err(self.over(got));
        }
        self.got = got;
        Ok(())
    }
}

/// The `Range` header value for `length` bytes at `offset` (inclusive end,
/// RFC 9110). A zero length asks for the one byte at `offset`, as native's
/// `saturating_sub` does.
#[must_use]
pub fn range_header(offset: u64, length: u64) -> String {
    format!("bytes={offset}-{}", offset + length.saturating_sub(1))
}

/// What a shell does with a reply once it has its status.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyStatus {
    /// Read the body and hand it to [`range_body`] / [`get_body`].
    ReadBody,
    /// Read the body as text for [`GridError::http`]: a non-2xx status.
    ReadErrorText,
    /// Stop: this reply is refused without reading its body.
    Refuse(GridError),
}

/// The status rule for a [`GridRequest::Range`] reply.
#[must_use]
pub fn range_status(status: u16) -> ReplyStatus {
    match status {
        206 => ReplyStatus::ReadBody,
        200..=299 => ReplyStatus::Refuse(GridError::Server(format!(
            "HTTP {status} for a byte range: the server ignored the Range header"
        ))),
        _ => ReplyStatus::ReadErrorText,
    }
}

/// A range reply's body, checked against the `length` asked for.
pub fn range_body(body: Vec<u8>, length: u64) -> Result<Vec<u8>, GridError> {
    // A zero length asks for one byte (`range_header`), so one is allowed.
    if body.len() as u64 > length.max(1) {
        return Err(GridError::Server(format!("a {length}-byte range came back with {} bytes", body.len())));
    }
    Ok(body)
}

/// The status rule for a [`GridRequest::Get`] reply. `exception_code` is
/// the reply's `X-ClickHouse-Exception-Code` header, if any: the proxy
/// passes it through, and such a body is an exception whatever the status.
#[must_use]
pub fn get_status(status: u16, exception_code: Option<&str>) -> ReplyStatus {
    if exception_code.is_some() || !(200..=299).contains(&status) {
        ReplyStatus::ReadErrorText
    } else {
        ReplyStatus::ReadBody
    }
}

/// A whole-body reply, bounded by [`MAX_BODY_BYTES`].
pub fn get_body(body: Vec<u8>) -> Result<Vec<u8>, GridError> {
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(GridError::Server(format!("a {}-byte response is over the {MAX_BODY_BYTES} B limit", body.len())));
    }
    Ok(body)
}

/// The error for a reply [`range_status`] / [`get_status`] sent to
/// [`ReplyStatus::ReadErrorText`].
#[must_use]
pub fn error_reply(status: u16, exception_code: Option<&str>, text: &str) -> GridError {
    match exception_code {
        Some(code) => GridError::Server(format!("ClickHouse exception {code}: {}", super::bounded_error_text(text))),
        None => GridError::http(status, text),
    }
}

/// How a browser fetches `req`: the URL and its headers -- `Range` for a
/// byte range, [`PROXY_HEADER`] for a proxy GET.
/// `None` for a request a browser never sends -- [`GridRequest::Post`]
/// (raw SQL never leaves the page; the proxy takes parameters) -- and for
/// [`GridRequest::Absent`], which needs no fetch.
#[must_use]
pub fn browser_fetch(req: &GridRequest) -> Option<(String, Vec<(String, String)>)> {
    match req {
        GridRequest::Range { location, offset, length } => {
            Some((location.clone(), vec![("Range".to_string(), range_header(*offset, *length))]))
        }
        GridRequest::Get { url } => Some((url.clone(), vec![(PROXY_HEADER.0.to_string(), PROXY_HEADER.1.to_string())])),
        GridRequest::Post { .. } | GridRequest::Absent => None,
    }
}

/// Which dot source the page's query string asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotSourceParam {
    /// `?grid=<url>`: a `dotgrid` PMTiles archive, read by `Range`.
    Pmtiles(String),
    /// `?clickhouse=<table>`: `gpui_map_dots.<table>` through the page's
    /// own server's `/clickhouse/` proxy.
    ClickHouse { table: String },
}

/// `?grid=` and `?clickhouse=` (either, not both; neither is no dots).
/// `grid` is a same-origin path or an http(s) URL; `clickhouse` a table
/// the proxy serves (`clickhouse::table_grid`).
pub fn dot_source_param(grid: Option<&str>, clickhouse: Option<&str>) -> Result<Option<DotSourceParam>, String> {
    let grid = grid.map(str::trim).filter(|g| !g.is_empty());
    let clickhouse = clickhouse.map(str::trim).filter(|c| !c.is_empty());
    match (grid, clickhouse) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err("?grid= and ?clickhouse= name two dot sources; pick one".to_string()),
        (Some(url), None) => {
            let lower = url.to_ascii_lowercase();
            let scheme = lower.split_once(':').map(|(s, _)| s).filter(|s| !s.contains('/'));
            match scheme {
                None | Some("http" | "https") => Ok(Some(DotSourceParam::Pmtiles(url.to_string()))),
                Some(other) => Err(format!("?grid= takes a path or an http(s) URL, not a {other}: URL")),
            }
        }
        (None, Some(table)) => match super::clickhouse::table_grid(table) {
            Some(_) => Ok(Some(DotSourceParam::ClickHouse { table: table.to_string() })),
            None => Err(format!("?clickhouse= takes {}, not {table:?}", super::clickhouse::PROXY_TABLES.join(" or "))),
        },
    }
}

/// The epoch a page draws: `?dot_epoch=` when the source has it, else
/// 2025 (the archive's and the native demo's) when it has that, else its
/// latest.
#[must_use]
pub fn pick_epoch(epochs: &[u16], asked: Option<f64>) -> Option<u16> {
    let asked = asked.filter(|e| e.fract() == 0.0 && *e >= 0.0 && *e <= f64::from(u16::MAX)).map(|e| e as u16);
    asked
        .filter(|e| epochs.contains(e))
        .or_else(|| epochs.contains(&2025).then_some(2025))
        .or_else(|| epochs.iter().copied().max())
}

/// The browser's dot budget: `?dot_budget=` (MG-24's 1.0 M fallback run)
/// within `1..=NATIVE_DOT_BUDGET`, else [`crate::render::dots::WEB_DOT_BUDGET`].
#[must_use]
pub fn web_dot_budget(asked: Option<f64>) -> u32 {
    use crate::render::dots::{NATIVE_DOT_BUDGET, WEB_DOT_BUDGET};
    asked.filter(|b| b.is_finite() && *b >= 1.0).map_or(WEB_DOT_BUDGET, |b| (b as u32).min(NATIVE_DOT_BUDGET))
}

/// The style a shell draws an opened dot source with (race plan, MG-R15):
/// a race archive (one with `"race"` metadata, `GridCountSource::race`)
/// gets [`super::layer::DotStyle::race`] on the shell's race budget
/// ([`super::layer::race_budget_for`]) -- dot mode only; the hexagons and
/// donuts stay on the native demo (preflight C21) -- and any other source
/// the shell's density style. `budget` overrides the budget (the page's
/// `?dot_budget=`), capped by `render::dots::RACE_DOT_BUDGET` for a race
/// archive; `epoch` is the one to draw ([`pick_epoch`]).
#[must_use]
pub fn shell_dot_style(
    platform: super::layer::Platform,
    race: bool,
    epoch: u16,
    budget: Option<u32>,
) -> super::layer::DotStyle {
    use super::layer::DotStyle;
    let base = if race { DotStyle::race(platform) } else { DotStyle::for_platform(platform) };
    // An override never lifts race dots past `RACE_DOT_BUDGET` (MG-R10's
    // cap, which `race_budget_for` applies to the default budget).
    let budget = budget.map(|b| if race { b.min(crate::render::dots::RACE_DOT_BUDGET) } else { b });
    DotStyle { epoch, budget_dots: budget.unwrap_or(base.budget_dots), ..base }
}

/// The race archive the phone looks for in `Documents` (MG-R15):
/// `ios/run-device.sh`'s `GPUI_MAP_RACE_ARCHIVE` copies it there, and
/// `GPUI_MAP_DOTS=race-dots-2020.pmtiles` draws it.
pub const PHONE_RACE_ARCHIVE: &str = "race-dots-2020.pmtiles";

/// The archive the phone draws dots from when nothing names one: the 30″
/// 2025 archive, copied into the app's `Documents` folder
/// (`ios/run-device.sh`'s `GPUI_MAP_DOTS_ARCHIVE`).
pub const PHONE_DOT_ARCHIVE: &str = "E2025_30ss.pmtiles";

/// Which local archive the phone draws dots from (Task 17), given
/// `GPUI_MAP_DOTS` (the process environment or the bundle's
/// `LSEnvironment`), the app's `Documents` folder and a file test:
/// - unset: [`PHONE_DOT_ARCHIVE`] in `documents` when it is there, else
///   no dots (`Ok(None)`) -- a phone the archive was never copied to runs
///   as before;
/// - `off` (or `0`, or empty): no dots;
/// - a path: that file, relative to `documents` unless absolute; an
///   `Err` naming it when it is not there, so the log says why no dots.
pub fn phone_dot_archive(
    setting: Option<&str>,
    documents: &std::path::Path,
    exists: impl Fn(&std::path::Path) -> bool,
) -> Result<Option<std::path::PathBuf>, String> {
    match setting.map(str::trim) {
        None => {
            let default = documents.join(PHONE_DOT_ARCHIVE);
            Ok(exists(&default).then_some(default))
        }
        Some("" | "off" | "0") => Ok(None),
        Some(named) => {
            let path = documents.join(named);
            if exists(&path) {
                Ok(Some(path))
            } else {
                Err(format!("GPUI_MAP_DOTS names {}, which is not there", path.display()))
            }
        }
    }
}

/// Where the phone starts (`GPUI_MAP_AT`, Task 17's MG-25): `india` and
/// `dhaka` are the native presets MG-11 pans at (`demos/world_dots.rs`:
/// India z5, Dhaka z10), or `lon,lat,zoom`. `(lon, lat, zoom)`; `None` for
/// anything else (the shell keeps its own start).
#[must_use]
pub fn phone_start_view(at: &str) -> Option<(f64, f64, f64)> {
    match at.trim() {
        "india" => Some((78.9, 22.0, 5.0)),
        "dhaka" => Some((90.4125, 23.8103, 10.0)),
        // The race dots' views (MG-R15): the lower 48 whole, and New York
        // at the race archive's densest.
        "us" => Some((-98.58, 39.83, 4.0)),
        "nyc" => Some((-73.95, 40.72, 10.0)),
        // Part tiles (z12+) may draw up to 2x the budget (T8 F1): the
        // phone's real ceiling, for MG-25.
        "nyc13" => Some((-73.95, 40.72, 13.0)),
        other => {
            let v: Vec<f64> = other.split(',').map(|p| p.trim().parse::<f64>()).collect::<Result<_, _>>().ok()?;
            match v[..] {
                [lon, lat, zoom] if lon.abs() <= 180.0 && lat.abs() <= 90.0 && (0.0..=24.0).contains(&zoom) => {
                    Some((lon, lat, zoom))
                }
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_starts_at_mg11s_views_or_a_given_one() {
        assert_eq!(phone_start_view("india"), Some((78.9, 22.0, 5.0)));
        assert_eq!(phone_start_view("dhaka"), Some((90.4125, 23.8103, 10.0)));
        assert_eq!(phone_start_view(" 2.35, 48.85, 15 "), Some((2.35, 48.85, 15.0)));
        for bad in ["", "paris", "1,2", "1,2,3,4", "200,0,5", "0,95,5", "0,0,30", "0,0,NaN", "a,b,c"] {
            assert_eq!(phone_start_view(bad), None, "{bad:?}");
        }
    }

    /// Race Task 15: a race archive draws Walker's categorical dots on the
    /// shell's race budget; any other source keeps its density style.
    #[test]
    fn shells_draw_a_race_archive_in_the_race_style() {
        use crate::overlay::dotgrid::layer::{race_budget_for, DotRamp, DotStyle, Platform};
        for platform in [Platform::Web, Platform::Ios] {
            let race = shell_dot_style(platform, true, 2020, None);
            assert_eq!(race.ramp, DotRamp::Categorical, "{platform:?}");
            assert_eq!(race.budget_dots, race_budget_for(platform));
            assert_eq!(race.epoch, 2020);
            assert_eq!(race, DotStyle { epoch: 2020, ..DotStyle::race(platform) });
            let density = shell_dot_style(platform, false, 2025, None);
            assert_eq!(density, DotStyle { epoch: 2025, ..DotStyle::for_platform(platform) });
            assert_eq!(shell_dot_style(platform, true, 2020, Some(1_000_000)).budget_dots, 1_000_000);
            // Review L4: an override never lifts race dots past the race cap;
            // a density source's override is its shell's own business.
            let over = crate::render::dots::RACE_DOT_BUDGET + 1;
            assert_eq!(shell_dot_style(platform, true, 2020, Some(over)).budget_dots, crate::render::dots::RACE_DOT_BUDGET);
            assert_eq!(shell_dot_style(platform, false, 2025, Some(over)).budget_dots, over);
        }
        assert_eq!(phone_start_view("nyc13"), Some((-73.95, 40.72, 13.0)));
        assert_eq!(phone_start_view("us"), Some((-98.58, 39.83, 4.0)));
        assert_eq!(phone_start_view("nyc"), Some((-73.95, 40.72, 10.0)));
    }

    /// Review N7: the phone's race archive name is the one
    /// `ios/run-device.sh` copies and defaults `GPUI_MAP_DOTS` to.
    #[test]
    fn run_device_copies_the_race_archive_by_its_constant_name() {
        let script = include_str!("../../../ios/run-device.sh");
        assert!(script.contains(&format!("RACE_ARCHIVE_NAME={PHONE_RACE_ARCHIVE}\n")), "run-device.sh names another race archive");
        assert!(script.contains("copy_archive \"$GPUI_MAP_RACE_ARCHIVE\" \"$RACE_ARCHIVE_NAME\""), "run-device.sh copies the race archive elsewhere");
        assert!(script.contains(&format!("copy_archive \"$GPUI_MAP_DOTS_ARCHIVE\" {PHONE_DOT_ARCHIVE}\n")), "run-device.sh copies the dot archive elsewhere");
    }

    mod phone_archive {
        use super::super::*;
        use std::path::{Path, PathBuf};

        const DOCS: &str = "/var/mobile/Containers/Data/Application/X/Documents";

        #[test]
        fn unset_draws_the_default_archive_only_when_it_was_copied() {
            let there = |p: &Path| p == Path::new(DOCS).join(PHONE_DOT_ARCHIVE);
            assert_eq!(phone_dot_archive(None, Path::new(DOCS), there), Ok(Some(Path::new(DOCS).join(PHONE_DOT_ARCHIVE))));
            assert_eq!(phone_dot_archive(None, Path::new(DOCS), |_| false), Ok(None));
        }

        #[test]
        fn off_draws_no_dots_even_with_the_archive_there() {
            for off in ["off", "0", "", "  "] {
                assert_eq!(phone_dot_archive(Some(off), Path::new(DOCS), |_| true), Ok(None), "{off:?}");
            }
        }

        #[test]
        fn a_named_archive_is_read_relative_to_documents_or_absolute() {
            assert_eq!(
                phone_dot_archive(Some("E1975_30ss.pmtiles"), Path::new(DOCS), |_| true),
                Ok(Some(PathBuf::from(DOCS).join("E1975_30ss.pmtiles")))
            );
            assert_eq!(phone_dot_archive(Some("/tmp/w.pmtiles"), Path::new(DOCS), |_| true), Ok(Some(PathBuf::from("/tmp/w.pmtiles"))));
        }

        #[test]
        fn a_named_archive_that_is_missing_says_so() {
            let err = phone_dot_archive(Some("nope.pmtiles"), Path::new(DOCS), |_| false).unwrap_err();
            assert!(err.contains("nope.pmtiles"), "{err}");
        }
    }

    #[test]
    fn a_range_header_is_inclusive() {
        assert_eq!(range_header(0, 16384), "bytes=0-16383");
        assert_eq!(range_header(100, 1), "bytes=100-100");
        assert_eq!(range_header(7, 0), "bytes=7-7");
    }

    #[test]
    fn a_range_must_come_back_partial() {
        assert_eq!(range_status(206), ReplyStatus::ReadBody);
        assert!(matches!(range_status(200), ReplyStatus::Refuse(GridError::Server(t)) if t.contains("ignored the Range")));
        assert_eq!(range_status(404), ReplyStatus::ReadErrorText);
        assert_eq!(range_status(416), ReplyStatus::ReadErrorText);
    }

    #[test]
    fn a_range_body_longer_than_asked_is_refused_and_a_short_one_kept() {
        assert!(range_body(vec![0; 17], 16).is_err());
        assert_eq!(range_body(vec![1; 16], 16).unwrap().len(), 16);
        assert_eq!(range_body(vec![1; 3], 16).unwrap().len(), 3);
        // A zero length asks for one byte and may get it (review F8).
        assert_eq!(range_header(9, 0), "bytes=9-9");
        assert_eq!(range_body(vec![1], 0).unwrap().len(), 1);
        assert!(range_body(vec![1; 2], 0).is_err());
    }

    /// Review F2: the bound is checked while reading -- a declared length
    /// before the first byte, a stream at the chunk that crosses it.
    #[test]
    fn a_body_is_bounded_as_it_is_read() {
        let range = GridRequest::Range { location: "w".into(), offset: 0, length: 16 };
        let mut cap = BodyCap::for_request(&range);
        assert!(cap.declared(Some(17)).is_err());
        assert!(cap.declared(Some(16)).is_ok() && cap.declared(None).is_ok());
        assert!(cap.take(10).is_ok() && cap.take(6).is_ok());
        assert!(matches!(cap.take(1), Err(GridError::Server(t)) if t.contains("16-byte range")));
        let mut zero = BodyCap::for_request(&GridRequest::Range { location: "w".into(), offset: 0, length: 0 });
        assert!(zero.take(1).is_ok() && zero.take(1).is_err());
        let mut get = BodyCap::for_request(&GridRequest::Get { url: "u".into() });
        assert!(get.declared(Some(MAX_BODY_BYTES + 1)).is_err());
        assert!(get.take(MAX_BODY_BYTES as usize).is_ok());
        assert!(get.take(1).is_err());
        assert!(read_error_text(Some(512)) && !read_error_text(None) && !read_error_text(Some(MAX_ERROR_BODY_BYTES + 1)));
    }

    /// Review F1: a fetch aborted at the deadline lands as a source error
    /// -- its request slot is freed, it is counted ("timeout"), and its keys
    /// back off and are asked for again after the backoff, as native's.
    #[test]
    fn a_timed_out_fetch_frees_its_slot_and_backs_off() {
        use crate::overlay::dotgrid::layer::{DotLayers, DotStyle, FAILED_BACKOFF};
        use crate::overlay::dotgrid::{grid, GridDescriptor, GridTile, MemoryGridSource};
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: "t".into(),
        };
        let mut layers = DotLayers::new();
        layers.set_source("pop", std::sync::Arc::new(MemoryGridSource::new(d, |_| GridTile::default())), DotStyle::default());
        let cover = |z: u8| vec![crate::geo::TileId { z, x: 0, y: 0 }];
        let view = ([0.0, 0.0], [0.5, 0.5]);
        let now = crate::timing::Instant::now();
        assert!(layers.frame_with_cover(0.5, cover, view, now), "a tile to fetch");
        let mut landed = 0;
        while let Some(job) = layers.next_job() {
            assert!(job.request().is_some());
            layers.land(job.run(|_| Err(timed_out())));
            landed += 1;
        }
        assert!(landed > 0);
        let stats = layers.stats("pop").unwrap();
        assert_eq!(stats.last_error, Some("timeout"));
        assert_eq!(stats.source_errors, landed);
        // Slots freed, keys backing off: no work until the backoff ends.
        assert!(!layers.frame_with_cover(0.5, cover, view, now));
        assert!(layers.next_retry(now).is_some_and(|w| w <= FAILED_BACKOFF));
        let later = now.after(FAILED_BACKOFF + std::time::Duration::from_millis(10));
        assert!(layers.frame_with_cover(0.5, cover, view, later), "asked for again");
        assert!(layers.next_job().is_some(), "a slot is free for it");
    }

    #[test]
    fn a_proxy_reply_is_an_exception_or_bounded_rows() {
        assert_eq!(get_status(200, None), ReplyStatus::ReadBody);
        assert_eq!(get_status(200, Some("164")), ReplyStatus::ReadErrorText);
        assert_eq!(get_status(400, None), ReplyStatus::ReadErrorText);
        assert!(get_body(vec![0; 8]).is_ok());
        assert_eq!(error_reply(500, Some("60"), "Code: 60. no table"), GridError::Server("ClickHouse exception 60: Code: 60. no table".into()));
        assert_eq!(error_reply(403, None, "same-origin only"), GridError::http(403, "same-origin only"));
    }

    #[test]
    fn a_browser_sends_ranges_and_gets_never_sql() {
        let range = GridRequest::Range { location: "./grid/w.pmtiles".into(), offset: 10, length: 5 };
        assert_eq!(browser_fetch(&range), Some(("./grid/w.pmtiles".into(), vec![("Range".into(), "bytes=10-14".into())])));
        let get = GridRequest::Get { url: "./clickhouse/tile?table=ghs_cells_30ss".into() };
        assert_eq!(
            browser_fetch(&get),
            Some(("./clickhouse/tile?table=ghs_cells_30ss".into(), vec![("X-Gpui-Map-Proxy".into(), "1".into())]))
        );
        let post = GridRequest::Post { url: "http://localhost:8123".into(), body: "SELECT 1".into(), headers: Vec::new() };
        assert_eq!(browser_fetch(&post), None);
        assert_eq!(browser_fetch(&GridRequest::Absent), None);
    }

    #[test]
    fn the_query_string_names_one_dot_source() {
        assert_eq!(dot_source_param(None, None), Ok(None));
        assert_eq!(dot_source_param(Some(""), Some(" ")), Ok(None));
        assert_eq!(dot_source_param(Some("./grid/E2025_30ss.pmtiles"), None), Ok(Some(DotSourceParam::Pmtiles("./grid/E2025_30ss.pmtiles".into()))));
        assert_eq!(dot_source_param(Some("https://h.example/w.pmtiles"), None), Ok(Some(DotSourceParam::Pmtiles("https://h.example/w.pmtiles".into()))));
        assert!(dot_source_param(Some("javascript:alert(1)"), None).is_err());
        assert!(dot_source_param(Some("file:///etc/passwd"), None).is_err());
        assert_eq!(dot_source_param(None, Some("ghs_cells_30ss")), Ok(Some(DotSourceParam::ClickHouse { table: "ghs_cells_30ss".into() })));
        assert!(dot_source_param(None, Some("system.users")).is_err());
        assert!(dot_source_param(None, Some("ghs_cells_30ss; DROP TABLE x")).is_err());
        assert!(dot_source_param(Some("./w.pmtiles"), Some("ghs_cells_30ss")).is_err());
    }

    #[test]
    fn the_epoch_is_the_asked_one_else_2025_else_the_latest() {
        let all = [1975, 2000, 2025, 2030];
        assert_eq!(pick_epoch(&all, Some(2000.0)), Some(2000));
        assert_eq!(pick_epoch(&all, Some(1999.0)), Some(2025));
        assert_eq!(pick_epoch(&all, Some(2000.5)), Some(2025));
        assert_eq!(pick_epoch(&all, None), Some(2025));
        assert_eq!(pick_epoch(&[1975, 2030], None), Some(2030));
        assert_eq!(pick_epoch(&[], None), None);
    }

    #[test]
    fn the_web_budget_is_mg24s_unless_asked() {
        use crate::render::dots::{NATIVE_DOT_BUDGET, WEB_DOT_BUDGET};
        assert_eq!(WEB_DOT_BUDGET, 1_500_000);
        assert_eq!(web_dot_budget(None), WEB_DOT_BUDGET);
        assert_eq!(web_dot_budget(Some(1_000_000.0)), 1_000_000);
        assert_eq!(web_dot_budget(Some(0.0)), WEB_DOT_BUDGET);
        assert_eq!(web_dot_budget(Some(f64::NAN)), WEB_DOT_BUDGET);
        assert_eq!(web_dot_budget(Some(9e9)), NATIVE_DOT_BUDGET);
    }
}
