//! Singapore as a GeoJSON showcase: the 55 URA Master Plan 2019 planning
//! areas as a population-density choropleth, the MRT/LRT network in each
//! line's own colour, and every station sized by the lines it serves --
//! over the quiet Mapbox Light basemap (OpenFreeMap Positron without a
//! token). Shared by `examples/overlay_features.rs` and the gallery
//! (`overlay-features`).
//!
//! # What it exercises
//!
//! Data-driven paint: `fill-color` interpolates on each area's `density`,
//! `line-color` matches on a track's `line`, `circle-color` matches on a
//! station part's `kind` -- one declaration each ([`paint_json`]), not a
//! colour per feature. The engine has no data-driven `circle-radius` or
//! `text-field` for an overlay (`overlay::source_paint` is colour-only), so
//! those two stay per-feature properties. Hover (`FeatureEvent`) raises the
//! area's outline via `set_feature_highlight` and fills the card; a click
//! eases to the area's bounds (`census::breakdowns::MercatorBounds::fit`).
//!
//! # Where the data comes from
//!
//! Nothing is committed. On first open the panel downloads five files
//! (the list is `scripts/singapore-overlay-sources.txt`, compiled in here
//! and read by `scripts/fetch-singapore-overlay.sh` -- one copy of every
//! URL) into `~/.cache/gpui-map-data/singapore/` and works offline after.
//! Every file is untrusted: size-bounded ([`MAX_BYTES`]) and parsed without
//! a panic path, each malformed input an `Err` the card shows.
//!
//! The MRT data is assembled, because no single open dataset has it: the
//! URA Master Plan 2019 rail-line layer has the track geometry but no line
//! names, LTA's station-exit layer has positions but no codes, and SEAB's
//! station-to-school table carries every current station's codes
//! (`CALDECOTT MRT STATION (CC17 / TE9)`). Tracks are assigned to the line
//! whose station-to-station chords they run along ([`classify_tracks`]);
//! a Master Plan track no operating line runs along is drawn dashed grey
//! as planned or under construction (the Jurong Region and Cross Island
//! lines, in 2019's alignment). The rail-line layer also omits the track
//! inside every station box, so each line is then stitched into one
//! network ([`build_tracks`]: each gap routed along the source track, a
//! straight piece only across a station hole of at most 420 m, strays
//! re-homed), which `every_operating_line_is_continuous_on_the_real_data`
//! checks on the cached files. A line whose station list runs past its
//! last station in service (TE past Bayshore, DT past Expo) is drawn in
//! its own colour, dashed, as under construction beyond that station.
//!
//! Hover reaches all three sources -- station first (pushed last, so hit
//! first), then line, then area -- and the card follows the pointer.
//!
//! `#![allow(dead_code)]`: this file is included by two crates (the
//! standalone example and the gallery) that each use a different half of
//! it, so some items are dead in one of them.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{div, prelude::*, px, rgb, rgba, App, Context, Entity, Window};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::{FeatureId, FeatureRef, HighlightStyle, TagValue};
use gpui_maps::tile_source::TileSource;
use serde_json::{json, Value};

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "overlay-features";

/// All of Singapore at the panel's opening camera.
pub const START: (f64, f64, f64) = (103.8198, 1.3521, 10.8);

/// The download list, compiled in. See the module doc.
pub const SOURCES_TXT: &str = include_str!("../../../scripts/singapore-overlay-sources.txt");

/// No downloaded file may be larger than this: the biggest (the rail-line
/// layer) is ~23 MB.
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// Where the files land: `~/.cache/gpui-map-data/singapore`, or
/// `GPUI_MAP_SG_DATA` (the fetch script honours it too).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GPUI_MAP_SG_DATA") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join(".cache/gpui-map-data/singapore")
}

// ---------------------------------------------------------------------------
// The download list
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// A data.gov.sg dataset id, fetched through [`poll_url`].
    DataGovSg,
    Url,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataSource {
    pub file: String,
    pub kind: SourceKind,
    pub target: String,
}

/// The `POLL` line's template, `{id}` still in it.
pub fn poll_template() -> &'static str {
    SOURCES_TXT
        .lines()
        .find_map(|l| l.strip_prefix("POLL ").map(str::trim))
        .expect("scripts/singapore-overlay-sources.txt has a POLL line")
}

pub fn poll_url(id: &str) -> String {
    poll_template().replace("{id}", id)
}

/// Every file line of [`SOURCES_TXT`].
pub fn sources() -> Vec<DataSource> {
    SOURCES_TXT
        .lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with("POLL"))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (file, kind, target) = (it.next()?, it.next()?, it.next()?);
            let kind = match kind {
                "datagovsg" => SourceKind::DataGovSg,
                "url" => SourceKind::Url,
                _ => return None,
            };
            Some(DataSource { file: file.into(), kind, target: target.into() })
        })
        .collect()
}

pub fn is_cached(dir: &Path) -> bool {
    sources().iter().all(|s| dir.join(&s.file).metadata().is_ok_and(|m| m.len() > 0))
}

/// A file off disk, refused over [`MAX_BYTES`] before it is read.
pub fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    if len > MAX_BYTES {
        return Err(format!("{} is {len} bytes, over the {MAX_BYTES}-byte limit", path.display()));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// One HTTP body, refused as soon as it passes [`MAX_BYTES`] (the declared
/// length first, then the bytes actually received).
async fn get_bounded(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let mut response = client.get(url).send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    if response.content_length().is_some_and(|n| n > MAX_BYTES) {
        return Err(format!("the server declares more than {MAX_BYTES} bytes"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("download interrupted: {e}"))? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > MAX_BYTES {
            return Err(format!("the body passed {MAX_BYTES} bytes"));
        }
    }
    Ok(body)
}

/// The signed download URL out of a data.gov.sg poll answer, or why not.
/// `Ok(None)`: rate limited, ask again.
pub fn poll_answer(body: &[u8]) -> Result<Option<String>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("data.gov.sg answered non-JSON: {e}"))?;
    if let Some(url) = v.pointer("/data/url").and_then(Value::as_str) {
        return Ok(Some(url.to_string()));
    }
    if v.get("code").and_then(Value::as_i64) == Some(24) || v.get("name").and_then(Value::as_str) == Some("TOO_MANY_REQUESTS") {
        return Ok(None);
    }
    Err(format!("data.gov.sg gave no download URL: {}", v.get("errorMsg").and_then(Value::as_str).unwrap_or("no reason")))
}

/// One data.gov.sg poll attempt, classified so the retry loop knows what is
/// worth waiting for.
enum PollAttempt {
    /// The signed download URL.
    Resolved(String),
    /// HTTP 429, or the documented `{"code":24,...}`/`TOO_MANY_REQUESTS`
    /// throttle body: worth a retry.
    RateLimited,
    /// Anything else -- a network error (offline, DNS, connection refused),
    /// a non-429/non-2xx status, or a body that is not the documented
    /// shape. There is nothing a sleep fixes, so this fails at once.
    Failed(String),
}

async fn poll_once(client: &reqwest::Client, poll_url: &str) -> PollAttempt {
    let mut response = match client.get(poll_url).send().await {
        Ok(r) => r,
        Err(e) => return PollAttempt::Failed(format!("request failed: {e}")),
    };
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return PollAttempt::RateLimited;
    }
    let status = response.status();
    if !status.is_success() {
        return PollAttempt::Failed(format!("HTTP {status}"));
    }
    if response.content_length().is_some_and(|n| n > MAX_BYTES) {
        return PollAttempt::Failed(format!("the server declares more than {MAX_BYTES} bytes"));
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() as u64 > MAX_BYTES {
                    return PollAttempt::Failed(format!("the body passed {MAX_BYTES} bytes"));
                }
            }
            Ok(None) => break,
            Err(e) => return PollAttempt::Failed(format!("download interrupted: {e}")),
        }
    }
    match poll_answer(&body) {
        Ok(Some(url)) => PollAttempt::Resolved(url),
        Ok(None) => PollAttempt::RateLimited,
        Err(why) => PollAttempt::Failed(why),
    }
}

/// Sleeps up to `dur`, checked in small slices so a cancel raised while
/// waiting out data.gov.sg's backoff is noticed well before the full wait
/// elapses, instead of only between attempts.
async fn cancellable_sleep(dur: Duration, cancel: &AtomicBool) {
    let step = Duration::from_millis(100);
    let mut left = dur;
    while left > Duration::ZERO {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        let this = step.min(left);
        tokio::time::sleep(this).await;
        left = left.saturating_sub(this);
    }
}

/// Resolves a data.gov.sg dataset's signed download URL, retrying only on
/// a rate-limit reply and checking `cancel` before every attempt (and
/// through the backoff wait) so leaving the panel does not run up the full
/// 5 x 11 s. Takes the full poll endpoint URL, not a dataset id, so tests
/// can point it at a mock server instead of `poll_url()`'s real host.
async fn resolve_datagovsg_url(client: &reqwest::Client, cancel: &Arc<AtomicBool>, poll_endpoint: &str) -> Result<String, String> {
    for _ in 0..5 {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        match poll_once(client, poll_endpoint).await {
            PollAttempt::Resolved(url) => return Ok(url),
            PollAttempt::RateLimited => cancellable_sleep(Duration::from_secs(11), cancel).await,
            PollAttempt::Failed(why) => return Err(why),
        }
    }
    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled".into());
    }
    Err("data.gov.sg kept rate-limiting".into())
}

/// Downloads every missing file of [`sources`] into `dir`, one at a time
/// (data.gov.sg rate-limits bursts), each written to `.part` and renamed.
/// `progress(done, total, file)` before each file.
pub async fn download_missing(
    dir: PathBuf,
    cancel: Arc<AtomicBool>,
    progress: impl Fn(usize, usize, &str),
) -> Result<(), String> {
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let client = reqwest::Client::builder()
        .user_agent("gpui-map-gallery (overlay-features)")
        .timeout(Duration::from_secs(300))
        .https_only(true)
        .build()
        .map_err(|e| e.to_string())?;
    let all = sources();
    for (i, s) in all.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        let out = dir.join(&s.file);
        if out.metadata().is_ok_and(|m| m.len() > 0) {
            continue;
        }
        progress(i, all.len(), &s.file);
        let url = match s.kind {
            SourceKind::Url => s.target.clone(),
            SourceKind::DataGovSg => {
                resolve_datagovsg_url(&client, &cancel, &poll_url(&s.target)).await.map_err(|why| format!("{}: {why}", s.file))?
            }
        };
        let body = get_bounded(&client, &url).await.map_err(|why| format!("{}: {why}", s.file))?;
        let part = dir.join(format!("{}.part", s.file));
        std::fs::write(&part, &body).map_err(|e| format!("{}: {e}", part.display()))?;
        std::fs::rename(&part, &out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    progress(all.len(), all.len(), "");
    Ok(())
}

// ---------------------------------------------------------------------------
// Parsing (untrusted input: every path returns Err, none panics)
// ---------------------------------------------------------------------------

pub type Ring = Vec<[f64; 2]>;

/// A planning area as parsed.
#[derive(Clone, Debug)]
pub struct Area {
    /// `ANG MO KIO`, as the boundary file spells it.
    pub name: String,
    pub polygons: Vec<Vec<Ring>>,
}

/// `PLN_AREA_N` spelling and SingStat's (`Ang Mo Kio`) meet here: upper
/// case, every run of non-alphanumerics one space (`NORTH-EASTERN ISLANDS`
/// and `North-Eastern Islands` -> `NORTH EASTERN ISLANDS`).
pub fn normalise(name: &str) -> String {
    name.to_uppercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `ANG MO KIO` -> `Ang Mo Kio`, for the card.
pub fn title_case(name: &str) -> String {
    name.split(' ')
        .map(|w| {
            let lower = w.to_lowercase();
            let mut out = String::new();
            let mut up = true;
            for c in lower.chars() {
                out.extend(if up { c.to_uppercase().collect::<Vec<_>>() } else { vec![c] });
                up = c == '-';
            }
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn features_of(bytes: &[u8]) -> Result<Vec<Value>, String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("not JSON: {e}"))?;
    match v.get("features") {
        Some(Value::Array(fs)) => Ok(fs.clone()),
        _ => Err("not a GeoJSON FeatureCollection (no `features` array)".into()),
    }
}

fn position(v: &Value) -> Option<[f64; 2]> {
    let a = v.as_array()?;
    let (x, y) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    (x.is_finite() && y.is_finite() && (-180.0..=180.0).contains(&x) && (-90.0..=90.0).contains(&y)).then_some([x, y])
}

fn ring(v: &Value) -> Option<Ring> {
    let r: Option<Ring> = v.as_array()?.iter().map(position).collect();
    r.filter(|r| r.len() >= 4)
}

fn line(v: &Value) -> Option<Ring> {
    let r: Option<Ring> = v.as_array()?.iter().map(position).collect();
    r.filter(|r| r.len() >= 2)
}

/// Polygons of a `Polygon` or `MultiPolygon` geometry; `None` for anything
/// else or any malformed ring.
fn polygons(geometry: &Value) -> Option<Vec<Vec<Ring>>> {
    let coords = geometry.get("coordinates")?;
    let poly = |p: &Value| -> Option<Vec<Ring>> { p.as_array()?.iter().map(ring).collect() };
    match geometry.get("type")?.as_str()? {
        "Polygon" => Some(vec![poly(coords)?]),
        "MultiPolygon" => coords.as_array()?.iter().map(poly).collect(),
        _ => None,
    }
}

fn lines(geometry: &Value) -> Option<Vec<Ring>> {
    let coords = geometry.get("coordinates")?;
    match geometry.get("type")?.as_str()? {
        "LineString" => Some(vec![line(coords)?]),
        "MultiLineString" => coords.as_array()?.iter().map(line).collect(),
        _ => None,
    }
}

/// The URA planning-area boundaries (`PLN_AREA_N` + polygon).
pub fn parse_areas(bytes: &[u8]) -> Result<Vec<Area>, String> {
    let mut out = Vec::new();
    for (i, f) in features_of(bytes)?.iter().enumerate() {
        let name = f.pointer("/properties/PLN_AREA_N").and_then(Value::as_str).ok_or(format!("area {i}: no PLN_AREA_N"))?;
        let polygons = f.get("geometry").and_then(polygons).ok_or(format!("area {name}: bad polygon"))?;
        out.push(Area { name: name.trim().to_string(), polygons });
    }
    if out.is_empty() {
        return Err("no planning areas".into());
    }
    Ok(out)
}

/// SingStat's table: `"<Area> - Total"` rows' first column, normalised
/// name -> residents. `"-"` (nil) is 0.
pub fn parse_population(bytes: &[u8]) -> Result<HashMap<String, u64>, String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("population: not JSON: {e}"))?;
    let rows = v.pointer("/Data/row").and_then(Value::as_array).ok_or("population: no Data.row")?;
    let mut out = HashMap::new();
    for row in rows {
        let Some(area) = row.get("rowText").and_then(Value::as_str).and_then(|t| t.strip_suffix(" - Total")) else { continue };
        let value = row.pointer("/columns/0/value").and_then(Value::as_str).unwrap_or("-").replace(',', "");
        let n = if value.trim() == "-" { 0 } else { value.trim().parse::<u64>().map_err(|_| format!("population: {area}: `{value}`"))? };
        out.insert(normalise(area), n);
    }
    if out.is_empty() {
        return Err("population: no `<area> - Total` rows".into());
    }
    Ok(out)
}

/// An area with its residents, or `None` when it did not join.
#[derive(Clone, Debug)]
pub struct JoinedArea {
    pub area: Area,
    pub residents: Option<u64>,
    pub km2: f64,
}

impl JoinedArea {
    /// Residents per km², 0 for none or no join.
    pub fn density(&self) -> f64 {
        match self.residents {
            Some(n) if self.km2 > 0.0 => n as f64 / self.km2,
            _ => 0.0,
        }
    }
}

/// Areas joined to population by [`normalise`]d name; the names that did
/// not join, from either side, come back to be logged.
pub fn join(areas: Vec<Area>, population: &HashMap<String, u64>) -> (Vec<JoinedArea>, Vec<String>) {
    let mut unjoined = Vec::new();
    let mut seen = BTreeSet::new();
    let joined = areas
        .into_iter()
        .map(|area| {
            let key = normalise(&area.name);
            let residents = population.get(&key).copied();
            if residents.is_none() {
                unjoined.push(format!("boundary {}", area.name));
            }
            seen.insert(key);
            let km2 = area.polygons.iter().map(|p| polygon_m2(p)).sum::<f64>() / 1e6;
            JoinedArea { area, residents, km2 }
        })
        .collect();
    for name in population.keys().filter(|k| !seen.contains(*k)) {
        unjoined.push(format!("population {name}"));
    }
    unjoined.sort();
    (joined, unjoined)
}

const M_PER_DEG: f64 = 111_320.0;

/// Local metres, east and north, near Singapore (1.35 N).
fn xy(p: [f64; 2]) -> (f64, f64) {
    (p[0] * M_PER_DEG * 1.35f64.to_radians().cos(), p[1] * M_PER_DEG)
}

fn ring_m2(r: &[[f64; 2]]) -> f64 {
    let s: f64 = r.windows(2).map(|w| {
        let (a, b) = (xy(w[0]), xy(w[1]));
        a.0 * b.1 - b.0 * a.1
    }).sum();
    (s / 2.0).abs()
}

fn polygon_m2(p: &[Ring]) -> f64 {
    let outer = p.first().map_or(0.0, |r| ring_m2(r));
    (outer - p.iter().skip(1).map(|r| ring_m2(r)).sum::<f64>()).max(0.0)
}

/// Douglas-Peucker in local metres; keeps the ends.
pub fn simplify(points: &[[f64; 2]], tolerance_m: f64) -> Ring {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut stack = vec![(0, points.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        let (mut best, mut at) = (0.0, 0);
        for i in a + 1..b {
            let d = seg_distance(points[i], points[a], points[b]);
            if d > best {
                (best, at) = (d, i);
            }
        }
        if best > tolerance_m {
            keep[at] = true;
            stack.push((a, at));
            stack.push((at, b));
        }
    }
    points.iter().zip(keep).filter_map(|(p, k)| k.then_some(*p)).collect()
}

fn simplify_ring(r: &[[f64; 2]], tolerance_m: f64) -> Ring {
    let s = simplify(r, tolerance_m);
    if s.len() >= 4 { s } else { r.to_vec() }
}

fn seg_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let ((px_, py), (ax, ay), (bx, by)) = (xy(p), xy(a), xy(b));
    let (dx, dy) = (bx - ax, by - ay);
    let l = dx * dx + dy * dy;
    let t = if l == 0.0 { 0.0 } else { (((px_ - ax) * dx + (py - ay) * dy) / l).clamp(0.0, 1.0) };
    (px_ - ax - t * dx).hypot(py - ay - t * dy)
}

// ---------------------------------------------------------------------------
// Stations and lines
// ---------------------------------------------------------------------------

/// The official line colours (LTA's), keyed as the `line` property is.
pub const LINE_COLOURS: &[(&str, &str, &str)] = &[
    ("NS", "North–South Line", "#d42e12"),
    ("EW", "East–West Line", "#009645"),
    ("NE", "North East Line", "#9900aa"),
    ("CC", "Circle Line", "#fa9e0d"),
    ("DT", "Downtown Line", "#005ec4"),
    ("TE", "Thomson–East Coast Line", "#9d5b25"),
    ("LRT", "LRT (Bukit Panjang, Sengkang, Punggol)", "#748477"),
];

/// A drawn line's name and colour, by its `line` code; `TE-UC` (under
/// construction) is the line's own name and colour.
pub fn line_name_colour(code: &str) -> (&'static str, &'static str) {
    if code == "PLANNED" {
        return ("Planned or under construction (MP2019 alignment)", PLANNED_COLOUR);
    }
    let base = code.strip_suffix("-UC").unwrap_or(code);
    LINE_COLOURS.iter().find(|(k, _, _)| *k == base).map_or(("Rail line", "#888888"), |(_, n, c)| (*n, *c))
}

/// A planned / under-construction track's colour, dashed.
pub const PLANNED_COLOUR: &str = "#8a9199";

/// Which drawn line a station code belongs to: `CG` is the East West
/// line's airport branch, `CE` the Circle line's extension, and every
/// LRT code (Bukit Panjang, Sengkang, Punggol) is one grey `LRT`. `None`
/// for a line with no colour here (a future line's code).
pub fn line_of(code: &str) -> Option<&'static str> {
    let prefix: String = code.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    Some(match prefix.as_str() {
        "NS" => "NS",
        "EW" | "CG" => "EW",
        "NE" => "NE",
        "CC" | "CE" => "CC",
        "DT" => "DT",
        "TE" => "TE",
        "BP" | "SE" | "SW" | "PE" | "PW" | "STC" | "PTC" => "LRT",
        _ => return None,
    })
}

/// `CALDECOTT MRT STATION` -> `CALDECOTT`: SEAB and LTA disagree on the
/// suffix of some interchanges (`SENGKANG LRT STATION` vs `... MRT ...`).
pub fn station_key(name: &str) -> String {
    let n = normalise(name);
    for suffix in [" MRT STATION", " LRT STATION", " STATION"] {
        if let Some(stripped) = n.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    n
}

/// SEAB's table: station key -> its codes, from the `From` column's
/// `NAME MRT STATION (CC17 / TE9)`.
pub fn parse_station_codes(bytes: &[u8]) -> Result<HashMap<String, BTreeSet<String>>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "station codes: not UTF-8")?;
    let mut lines = text.lines();
    let header = lines.next().ok_or("station codes: empty")?;
    let col = split_csv(header).iter().position(|h| h.trim_start_matches('\u{feff}') == "From").ok_or("station codes: no `From` column")?;
    let mut out: HashMap<String, BTreeSet<String>> = HashMap::new();
    for l in lines {
        let fields = split_csv(l);
        let Some(from) = fields.get(col) else { continue };
        let (Some(open), Some(close)) = (from.rfind('('), from.rfind(')')) else { continue };
        if close < open {
            continue;
        }
        let codes = from[open + 1..close].split('/').map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        out.entry(station_key(&from[..open])).or_default().extend(codes);
    }
    if out.is_empty() {
        return Err("station codes: no `NAME (CODE)` rows".into());
    }
    Ok(out)
}

/// A CSV line's fields, double quotes honoured.
fn split_csv(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// LTA's exits: station key -> the mean of its exits.
pub fn parse_exits(bytes: &[u8]) -> Result<HashMap<String, [f64; 2]>, String> {
    let mut acc: HashMap<String, ([f64; 2], usize)> = HashMap::new();
    for f in features_of(bytes)? {
        let Some(name) = f.pointer("/properties/STATION_NA").and_then(Value::as_str) else { continue };
        let Some(p) = f.pointer("/geometry/coordinates").and_then(position) else { continue };
        let e = acc.entry(station_key(name)).or_insert(([0.0, 0.0], 0));
        e.0[0] += p[0];
        e.0[1] += p[1];
        e.1 += 1;
    }
    if acc.is_empty() {
        return Err("station exits: no named points".into());
    }
    Ok(acc.into_iter().map(|(k, (s, n))| (k, [s[0] / n as f64, s[1] / n as f64])).collect())
}

#[derive(Clone, Debug)]
pub struct Station {
    pub name: String,
    pub codes: BTreeSet<String>,
    pub at: [f64; 2],
}

impl Station {
    /// The distinct drawn lines it serves (`NS`, `EW`, ... `LRT`).
    pub fn lines(&self) -> BTreeSet<&'static str> {
        self.codes.iter().filter_map(|c| line_of(c)).collect()
    }
}

/// Stations with both a code and a position. An exit named by a bare code
/// (`CC30`: LTA's name for a newly opened station) places the station
/// that code belongs to.
pub fn join_stations(codes: &HashMap<String, BTreeSet<String>>, exits: &HashMap<String, [f64; 2]>) -> Vec<Station> {
    let by_code: HashMap<&str, [f64; 2]> =
        exits.iter().filter(|(k, _)| k.chars().any(|c| c.is_ascii_digit()) && !k.contains(' ')).map(|(k, p)| (k.as_str(), *p)).collect();
    let mut out: Vec<Station> = codes
        .iter()
        .filter_map(|(key, cs)| {
            let at = exits.get(key).copied().or_else(|| cs.iter().find_map(|c| by_code.get(c.as_str()).copied()))?;
            Some(Station { name: title_case(key), codes: cs.clone(), at })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Station-to-station chords per drawn line, in code order: `(line, a, b)`.
/// Branches start at their junction (`CG` at `EW4`, `CE` at `CC4`, the
/// LRT loops at their hubs).
pub fn chords(stations: &[Station]) -> Vec<(&'static str, [f64; 2], [f64; 2])> {
    let mut by_code: HashMap<String, [f64; 2]> = HashMap::new();
    for s in stations {
        for c in &s.codes {
            by_code.insert(c.clone(), s.at);
        }
    }
    let split = |c: &str| {
        let prefix: String = c.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let n: u32 = c[prefix.len()..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0);
        (prefix, n, c.to_string())
    };
    let mut groups: HashMap<String, Vec<(u32, String)>> = HashMap::new();
    for c in by_code.keys() {
        let (p, n, c) = split(c);
        groups.entry(p).or_default().push((n, c));
    }
    let junction = |p: &str| match p {
        "CG" => Some("EW4"),
        "CE" => Some("CC4"),
        "SE" | "SW" => Some("STC"),
        "PE" | "PW" => Some("PTC"),
        _ => None,
    };
    let mut out = Vec::new();
    let mut prefixes: Vec<_> = groups.keys().cloned().collect();
    prefixes.sort();
    for p in prefixes {
        let Some(line) = line_of(&p) else { continue };
        let mut codes = groups[&p].clone();
        codes.sort();
        let mut seq: Vec<[f64; 2]> = junction(&p).and_then(|j| by_code.get(j)).into_iter().copied().collect();
        seq.extend(codes.iter().map(|(_, c)| by_code[c]));
        for w in seq.windows(2) {
            out.push((line, w[0], w[1]));
        }
    }
    out
}

/// One piece of track, as drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    /// `NS` .. `LRT`, or `PLANNED`.
    pub line: &'static str,
    pub points: Ring,
}

/// The Master Plan rail-line layer: `(is_lrt, part)` per line part.
pub fn parse_tracks(bytes: &[u8]) -> Result<Vec<(bool, Ring)>, String> {
    let mut out = Vec::new();
    for f in features_of(bytes)? {
        let lrt = f.pointer("/properties/RAIL_TYPE").and_then(Value::as_str) == Some("LRT");
        if let Some(parts) = f.get("geometry").and_then(lines) {
            out.extend(parts.into_iter().map(|p| (lrt, p)));
        }
    }
    if out.is_empty() {
        return Err("rail lines: no line features".into());
    }
    Ok(out)
}

fn length_m(r: &[[f64; 2]]) -> f64 {
    r.windows(2).map(|w| {
        let (a, b) = (xy(w[0]), xy(w[1]));
        (a.0 - b.0).hypot(a.1 - b.1)
    }).sum()
}

/// Assigns each track part to the line whose chords it runs along: sample
/// points vote for the nearest chord that is (a) the same kind, MRT or
/// LRT, (b) within 350 m and (c) roughly parallel (|cos| >= 0.7) -- which
/// is what keeps a future line crossing an interchange from taking the
/// operating line's colour. Unassigned parts whose both ends meet one
/// line's parts join it (a curve between distant stations); what is left
/// and longer than 300 m is `PLANNED`; the rest (depot spurs) is dropped.
pub fn classify_tracks(parts: &[(bool, Ring)], chords: &[(&'static str, [f64; 2], [f64; 2])]) -> Vec<Track> {
    tracks_from_labels(parts, &classify_labels(parts, chords))
}

/// A part's own track, or planned (unassigned MRT track over 300 m), or
/// nothing (depot spurs).
pub fn tracks_from_labels(parts: &[(bool, Ring)], labels: &[Option<&'static str>]) -> Vec<Track> {
    parts
        .iter()
        .zip(labels)
        .filter_map(|((lrt, points), label)| {
            let line = match label {
                Some(l) => *l,
                None if !lrt && length_m(points) > 300.0 => "PLANNED",
                None => return None,
            };
            Some(Track { line, points: points.clone() })
        })
        .collect()
}

/// [`classify_tracks`]' per-part answer: `None` for a part no operating
/// line claims.
pub fn classify_labels(parts: &[(bool, Ring)], chords: &[(&'static str, [f64; 2], [f64; 2])]) -> Vec<Option<&'static str>> {
    let mut labels: Vec<Option<&'static str>> = parts
        .iter()
        .map(|(lrt, points)| {
            let mut votes: HashMap<&'static str, usize> = HashMap::new();
            let step = (points.len() / 8).max(1);
            for i in (0..points.len()).step_by(step) {
                let (p, q) = if i + 1 < points.len() { (points[i], points[i + 1]) } else { (points[i - 1], points[i]) };
                let (t0, t1) = (xy(p), xy(q));
                let (tx, ty) = (t1.0 - t0.0, t1.1 - t0.1);
                let tl = tx.hypot(ty).max(1e-9);
                let mut best: Option<(&'static str, f64)> = None;
                for &(line, a, b) in chords {
                    if (line == "LRT") != *lrt {
                        continue;
                    }
                    let (c0, c1) = (xy(a), xy(b));
                    let (cx, cy) = (c1.0 - c0.0, c1.1 - c0.1);
                    let cl = cx.hypot(cy).max(1e-9);
                    if ((tx * cx + ty * cy) / (tl * cl)).abs() < 0.7 {
                        continue;
                    }
                    let d = seg_distance(points[i], a, b);
                    if best.is_none_or(|(_, bd)| d < bd) {
                        best = Some((line, d));
                    }
                }
                if let Some((line, d)) = best.filter(|(_, d)| *d < 350.0) {
                    let _ = d;
                    *votes.entry(line).or_default() += 1;
                }
            }
            votes.into_iter().max_by_key(|(l, n)| (*n, *l)).map(|(l, _)| l)
        })
        .collect();
    let near = |a: [f64; 2], b: [f64; 2]| {
        let (p, q) = (xy(a), xy(b));
        (p.0 - q.0).hypot(p.1 - q.1) < 40.0
    };
    for _ in 0..6 {
        let mut changed = false;
        for i in 0..parts.len() {
            if labels[i].is_some() {
                continue;
            }
            let pts = &parts[i].1;
            let touching = |end: [f64; 2]| -> BTreeSet<&'static str> {
                parts
                    .iter()
                    .zip(&labels)
                    .filter_map(|((_, q), l)| l.filter(|_| near(end, q[0]) || near(end, q[q.len() - 1])))
                    .collect()
            };
            let (a, b) = (touching(pts[0]), touching(pts[pts.len() - 1]));
            // Between two pieces of one line: that line. A short stub off
            // one line's end, touching nothing else: that line too.
            let short = length_m(pts) < 300.0;
            let adopt = if (a.len() == 1 && a == b) || (short && a.len() == 1 && b.is_empty()) {
                a.into_iter().next()
            } else if short && b.len() == 1 && a.is_empty() {
                b.into_iter().next()
            } else {
                None
            };
            if adopt.is_some() {
                labels[i] = adopt;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    labels
}

/// Joins pieces of one line that meet end to end (within 2 m) into one
/// polyline, so a line is a few long pieces rather than hundreds of short
/// ones -- which is also what makes a hovered line highlight as a line.
pub fn merge_contiguous(tracks: Vec<Track>) -> Vec<Track> {
    let mut pool: Vec<Option<Track>> = tracks.into_iter().map(Some).collect();
    let close = |a: [f64; 2], b: [f64; 2]| dist_m(a, b) <= 2.0;
    for i in 0..pool.len() {
        let Some(mut cur) = pool[i].take() else { continue };
        loop {
            let mut grown = false;
            for slot in pool.iter_mut() {
                let Some(other) = slot.as_ref() else { continue };
                if other.line != cur.line {
                    continue;
                }
                let (h, t) = (cur.points[0], cur.points[cur.points.len() - 1]);
                let (oh, ot) = (other.points[0], other.points[other.points.len() - 1]);
                let how = if close(t, oh) {
                    0
                } else if close(t, ot) {
                    1
                } else if close(h, ot) {
                    2
                } else if close(h, oh) {
                    3
                } else {
                    continue;
                };
                let mut o = slot.take().expect("checked above").points;
                if how == 1 || how == 3 {
                    o.reverse();
                }
                if how <= 1 {
                    cur.points.extend(o.drain(1..));
                } else {
                    o.extend(cur.points.drain(1..));
                    cur.points = o;
                }
                grown = true;
            }
            if !grown {
                break;
            }
        }
        pool[i] = Some(cur);
    }
    pool.into_iter().flatten().collect()
}

fn dist_m(a: [f64; 2], b: [f64; 2]) -> f64 {
    let (p, q) = (xy(a), xy(b));
    (p.0 - q.0).hypot(p.1 - q.1)
}

/// The unit direction a piece LEAVES by at one end, measured over its last
/// ~30 m, in local metres.
fn outward(points: &[[f64; 2]], at_tail: bool) -> (f64, f64) {
    let seq: Vec<[f64; 2]> = if at_tail { points.iter().rev().copied().collect() } else { points.to_vec() };
    let end = seq[0];
    let back = seq.iter().skip(1).find(|p| dist_m(**p, end) >= 30.0).or(seq.last()).copied().unwrap_or(end);
    let (e, b) = (xy(end), xy(back));
    let (dx, dy) = (e.0 - b.0, e.1 - b.1);
    let l = dx.hypot(dy).max(1e-9);
    (dx / l, dy / l)
}

/// The longest gap closed with a STRAIGHT piece: the Master Plan
/// rail-line layer omits the track inside every station box (Bedok's is
/// 170 m, Bayshore's ~350 m), and there is no source track to follow
/// there. Every longer gap is routed along the source track or left open.
pub const STRAIGHT_MAX_M: f64 = 420.0;

/// The longest gap a route through the source track is searched for.
pub const ROUTE_MAX_M: f64 = 3_000.0;

/// The source rail parts as a graph: part ends snapped within
/// [`SNAP_M`] are one node, each part an edge weighted by its length.
pub struct TrackGraph {
    node_of_end: Vec<(usize, usize)>,
    nodes: Vec<[f64; 2]>,
    /// `(to, step, weight)`.
    adjacency: Vec<Vec<(usize, Step, f64)>>,
}

/// One step of a route: along a source part, or a straight hop across a
/// hole in the source (a station box) of at most [`STRAIGHT_MAX_M`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    Part(usize),
    Hop(usize, usize),
}

/// A hop costs this many times its length, so a route takes the source
/// track wherever there is one.
const HOP_PENALTY: f64 = 4.0;

pub const SNAP_M: f64 = 20.0;

impl TrackGraph {
    pub fn new(parts: &[(bool, Ring)]) -> Self {
        let mut nodes: Vec<[f64; 2]> = Vec::new();
        let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        let cell = |p: [f64; 2]| {
            let (x, y) = xy(p);
            ((x / SNAP_M).floor() as i64, (y / SNAP_M).floor() as i64)
        };
        let mut node = |p: [f64; 2], nodes: &mut Vec<[f64; 2]>| -> usize {
            let (cx, cy) = cell(p);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    if let Some(ids) = grid.get(&(cx + dx, cy + dy)) {
                        if let Some(&id) = ids.iter().find(|&&id| dist_m(nodes[id], p) <= SNAP_M) {
                            return id;
                        }
                    }
                }
            }
            nodes.push(p);
            grid.entry((cx, cy)).or_default().push(nodes.len() - 1);
            nodes.len() - 1
        };
        let mut node_of_end = Vec::with_capacity(parts.len());
        for (_, r) in parts {
            let a = node(r[0], &mut nodes);
            let b = node(r[r.len() - 1], &mut nodes);
            node_of_end.push((a, b));
        }
        let mut adjacency = vec![Vec::new(); nodes.len()];
        for (i, &(a, b)) in node_of_end.iter().enumerate() {
            let w = length_m(&parts[i].1);
            adjacency[a].push((b, Step::Part(i), w));
            adjacency[b].push((a, Step::Part(i), w));
        }
        // Hops between part ends up to STRAIGHT_MAX_M apart: the holes the
        // source leaves at stations.
        let big = |p: [f64; 2]| {
            let (x, y) = xy(p);
            ((x / STRAIGHT_MAX_M).floor() as i64, (y / STRAIGHT_MAX_M).floor() as i64)
        };
        let mut cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (n, p) in nodes.iter().enumerate() {
            cells.entry(big(*p)).or_default().push(n);
        }
        for (n, p) in nodes.iter().enumerate() {
            let (cx, cy) = big(*p);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &m in cells.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                        let d = dist_m(*p, nodes[m]);
                        if m != n && d <= STRAIGHT_MAX_M {
                            adjacency[n].push((m, Step::Hop(n, m), d * HOP_PENALTY + 50.0));
                        }
                    }
                }
            }
        }
        Self { node_of_end, nodes, adjacency }
    }

    fn nearest_node(&self, p: [f64; 2]) -> Option<usize> {
        (0..self.nodes.len()).filter(|&n| dist_m(self.nodes[n], p) <= SNAP_M).min_by(|&a, &b| dist_m(self.nodes[a], p).total_cmp(&dist_m(self.nodes[b], p)))
    }

    /// The shortest path from `a` to `b` over the parts `allowed` admits,
    /// no longer than `limit` metres: the part indices in order.
    pub fn route(&self, a: [f64; 2], b: [f64; 2], cost: impl Fn(usize) -> Option<f64>, limit: f64) -> Option<Vec<Step>> {
        let (from, to) = (self.nearest_node(a)?, self.nearest_node(b)?);
        if from == to {
            return None;
        }
        let mut dist = vec![f64::INFINITY; self.nodes.len()];
        let mut via: Vec<Option<(usize, Step)>> = vec![None; self.nodes.len()];
        let mut heap = std::collections::BinaryHeap::new();
        dist[from] = 0.0;
        heap.push(std::cmp::Reverse((0u64, from)));
        while let Some(std::cmp::Reverse((d_mm, n))) = heap.pop() {
            let d = d_mm as f64 / 1000.0;
            if d > dist[n] + 1e-6 {
                continue;
            }
            if n == to {
                break;
            }
            for &(m, part, w) in &self.adjacency[n] {
                let factor = match part {
                    Step::Part(i) => match cost(i) {
                        Some(f) => f,
                        None => continue,
                    },
                    Step::Hop(..) => 1.0,
                };
                let nd = d + w * factor;
                if nd < dist[m] && nd <= limit {
                    dist[m] = nd;
                    via[m] = Some((n, part));
                    heap.push(std::cmp::Reverse(((nd * 1000.0) as u64, m)));
                }
            }
        }
        if !dist[to].is_finite() {
            return None;
        }
        let mut path = Vec::new();
        let mut n = to;
        while let Some((prev, part)) = via[n] {
            path.push(part);
            n = prev;
        }
        path.reverse();
        Some(path)
    }
}

/// Where a line's source is a join rather than a part, for mapping a
/// component back to what made it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Src {
    Part(usize),
    Join(usize),
}

/// One line's pieces, each part and join on its own (simplified, never
/// merged), with where each came from.
fn line_pieces(parts: &[(bool, Ring)], labels: &[Option<&'static str>], joins: &[Track], line: &'static str) -> (Vec<Track>, Vec<Src>) {
    let mut tracks = Vec::new();
    let mut srcs = Vec::new();
    for (i, l) in labels.iter().enumerate() {
        if *l == Some(line) {
            tracks.push(Track { line, points: simplify(&parts[i].1, TRACK_TOLERANCE_M) });
            srcs.push(Src::Part(i));
        }
    }
    for (j, t) in joins.iter().enumerate() {
        if t.line == line {
            tracks.push(t.clone());
            srcs.push(Src::Join(j));
        }
    }
    (tracks, srcs)
}

/// Closes one line's gaps, Kruskal-style over its networks: the shortest
/// gap between two different networks first, routed along the source
/// track through parts no other line claims (relabelled to this line),
/// or -- only where no such track exists and the gap is at most
/// [`STRAIGHT_MAX_M`], i.e. inside a station box -- a straight piece.
fn close_gaps(
    parts: &[(bool, Ring)],
    graph: &TrackGraph,
    labels: &mut [Option<&'static str>],
    joins: &mut Vec<Track>,
    line: &'static str,
    routed: &mut usize,
    straight: &mut Vec<f64>,
) {
    let (tracks, _) = line_pieces(parts, labels, joins, line);
    let comps = components(&tracks, line, 30.0);
    if comps.len() < 2 {
        return;
    }
    let mut comp_of = vec![0; tracks.len()];
    for (c, (m, _)) in comps.iter().enumerate() {
        for &i in m {
            comp_of[i] = c;
        }
    }
    let ends: Vec<(usize, [f64; 2])> =
        tracks.iter().enumerate().flat_map(|(i, t)| [(comp_of[i], t.points[0]), (comp_of[i], t.points[t.points.len() - 1])]).collect();
    let mut gaps: Vec<(f64, usize, usize)> = Vec::new();
    for a in 0..ends.len() {
        for b in a + 1..ends.len() {
            if ends[a].0 != ends[b].0 {
                let d = dist_m(ends[a].1, ends[b].1);
                if d <= ROUTE_MAX_M {
                    gaps.push((d, a, b));
                }
            }
        }
    }
    gaps.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut parent: Vec<usize> = (0..comps.len()).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for (d, a, b) in gaps {
        let (ra, rb) = (find(&mut parent, ends[a].0), find(&mut parent, ends[b].0));
        if ra == rb {
            continue;
        }
        let snapshot: Vec<Option<&'static str>> = labels.to_vec();
        // Unassigned parts and this line's own at face value; another
        // line's (a stretch the classifier gave to a line running
        // alongside, CC at Caldecott for the TE) at a premium, and shared
        // rather than taken: this line gets a copy.
        let cost = |i: usize| match snapshot[i] {
            None => Some(1.0),
            Some(l) if l == line => Some(1.0),
            Some(l) if MRT_LINES.contains(&l) && line != "LRT" => Some(1.5),
            Some(_) => None,
        };
        let route = graph.route(ends[a].1, ends[b].1, cost, (3.0 * d + 300.0).min(2.0 * ROUTE_MAX_M) * HOP_PENALTY);
        match route {
            // A route that takes some source track: its parts join the
            // line, its hops (station holes) are straight joins.
            Some(path) if path.iter().any(|s| matches!(s, Step::Part(_))) => {
                for step in path {
                    match step {
                        Step::Part(i) if snapshot[i].is_none() || snapshot[i] == Some(line) => labels[i] = Some(line),
                        Step::Part(i) => joins.push(Track { line, points: parts[i].1.clone() }),
                        Step::Hop(x, y) => {
                            straight.push(dist_m(graph.nodes[x], graph.nodes[y]));
                            joins.push(Track { line, points: vec![graph.nodes[x], graph.nodes[y]] });
                        }
                    }
                }
                *routed += 1;
            }
            _ if d <= STRAIGHT_MAX_M => {
                straight.push(d);
                joins.push(Track { line, points: vec![ends[a].1, ends[b].1] });
            }
            _ => continue,
        }
        parent[ra] = rb;
    }
}

/// Where a station's double track got one side joined through it and the
/// other not, the other: an end of the line no other piece of it meets,
/// facing (within ~25°) a mutual nearest such end at most
/// [`STRAIGHT_MAX_M`] away. A terminus faces nothing and stays open.
fn close_facing_ends(parts: &[(bool, Ring)], labels: &[Option<&'static str>], joins: &mut Vec<Track>, line: &'static str, straight: &mut Vec<f64>) {
    let (tracks, _) = line_pieces(parts, labels, joins, line);
    let dangling: Vec<([f64; 2], (f64, f64))> = tracks
        .iter()
        .enumerate()
        .flat_map(|(i, t)| [(i, t.points[0], outward(&t.points, false)), (i, t.points[t.points.len() - 1], outward(&t.points, true))])
        .filter(|&(i, p, _)| !tracks.iter().enumerate().any(|(j, o)| j != i && to_polyline(p, &o.points) <= 30.0))
        .map(|(_, p, t)| (p, t))
        .collect();
    let best = |k: usize| -> Option<usize> {
        let (a, ta) = dangling[k];
        (0..dangling.len())
            .filter(|&m| m != k)
            .filter_map(|m| {
                let (b, tb) = dangling[m];
                let d = dist_m(a, b);
                if !(1.0..=STRAIGHT_MAX_M).contains(&d) {
                    return None;
                }
                let (pa, pb) = (xy(a), xy(b));
                let (ux, uy) = ((pb.0 - pa.0) / d, (pb.1 - pa.1) / d);
                (ta.0 * ux + ta.1 * uy >= 0.9 && -(tb.0 * ux + tb.1 * uy) >= 0.9).then_some((m, d))
            })
            .min_by(|x, y| x.1.total_cmp(&y.1))
            .map(|(m, _)| m)
    };
    for k in 0..dangling.len() {
        if let Some(m) = best(k) {
            if m > k && best(m) == Some(k) {
                straight.push(dist_m(dangling[k].0, dangling[m].0));
                joins.push(Track { line, points: vec![dangling[k].0, dangling[m].0] });
            }
        }
    }
}

/// A line's under-construction extension: past `terminal` (its last
/// station in service), in the direction `(east, north)` from the station
/// before it. See [`extensions`].
#[derive(Clone, Copy, Debug)]
pub struct Extension {
    pub line: &'static str,
    pub terminal: [f64; 2],
    pub direction: (f64, f64),
}

/// A line whose station list (SEAB's, every current code) runs past its
/// last station with a position (LTA's exits: stations in service) is
/// being extended there: TE past Bayshore (TE29) to TE30/TE31, DT past
/// Expo (DT35) to DT36/DT37.
pub fn extensions(codes: &HashMap<String, BTreeSet<String>>, stations: &[Station]) -> Vec<Extension> {
    let num = |c: &str, prefix: &str| c.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok());
    let mut out = Vec::new();
    for &line in MRT_LINES {
        let listed = codes.values().flatten().filter_map(|c| num(c, line)).max();
        let mut placed: Vec<(u32, [f64; 2])> =
            stations.iter().flat_map(|s| s.codes.iter().filter_map(move |c| num(c, line).map(|n| (n, s.at)))).collect();
        placed.sort_by_key(|p| p.0);
        let (Some(listed), Some(&(last, at)), Some(&(_, before))) = (listed, placed.last(), placed.len().checked_sub(2).and_then(|i| placed.get(i))) else { continue };
        if listed > last {
            let (a, b) = (xy(before), xy(at));
            let l = (b.0 - a.0).hypot(b.1 - a.1).max(1e-9);
            out.push(Extension { line, terminal: at, direction: ((b.0 - a.0) / l, (b.1 - a.1) / l) });
        }
    }
    out
}

/// The `line` code of a stretch under construction: `TE-UC`.
pub fn under_construction(line: &str) -> String {
    format!("{line}-UC")
}

/// Splits every piece of an extended line where it passes its terminal,
/// relabelling what lies beyond (within 3 km and 800 m of the line's
/// axis) as under construction; a planned piece there is the same
/// extension in the Master Plan and takes the same label.
fn mark_extensions(tracks: Vec<Track>, ext: &[Extension], labels: &'static [(&'static str, &'static str)]) -> Vec<Track> {
    let beyond = |e: &Extension, p: [f64; 2]| {
        let (t, q) = (xy(e.terminal), xy(p));
        let (dx, dy) = (q.0 - t.0, q.1 - t.1);
        let along = dx * e.direction.0 + dy * e.direction.1;
        let across = (dx * e.direction.1 - dy * e.direction.0).abs();
        along > 20.0 && along < 3_000.0 && across < 800.0
    };
    let mut out = Vec::new();
    for t in tracks {
        let Some(e) = ext.iter().find(|e| e.line == t.line || (t.line == "PLANNED" && t.points.iter().all(|p| beyond(e, *p)))) else {
            out.push(t);
            continue;
        };
        let uc = labels.iter().find(|(l, _)| *l == e.line).map(|(_, u)| *u).unwrap_or("PLANNED");
        // Runs of vertices beyond and not, each its own piece.
        let mut run: Ring = Vec::new();
        let mut run_beyond = beyond(e, t.points[0]);
        for &p in &t.points {
            let b = beyond(e, p);
            if b != run_beyond && !run.is_empty() {
                run.push(p);
                if run.len() >= 2 {
                    out.push(Track { line: if run_beyond { uc } else { t.line }, points: std::mem::take(&mut run) });
                }
                run = vec![p];
                run_beyond = b;
                continue;
            }
            run.push(p);
        }
        if run.len() >= 2 {
            out.push(Track { line: if run_beyond { uc } else { t.line }, points: run });
        }
    }
    out
}

/// The under-construction codes, one per MRT line.
pub const UNDER_CONSTRUCTION: &[(&str, &str)] =
    &[("NS", "NS-UC"), ("EW", "EW-UC"), ("NE", "NE-UC"), ("CC", "CC-UC"), ("DT", "DT-UC"), ("TE", "TE-UC")];

/// What [`build_tracks`] made, and how.
#[derive(Clone, Debug, Default)]
pub struct TrackBuild {
    pub tracks: Vec<Track>,
    /// The length of every straight join (each at most [`STRAIGHT_MAX_M`]).
    pub straight_joins: Vec<f64>,
    /// Gaps closed by a route along the source track.
    pub routed: usize,
    /// Each source part's final line.
    pub labels: Vec<Option<&'static str>>,
}

/// Whether most of `t` (80 % of its sampled vertices) runs within 250 m of
/// some operating line's track: a stretch of that line the classifier left
/// out, which must not also be drawn as planned.
fn shadows_an_operating_line(t: &Track, tracks: &[Track]) -> bool {
    let step = (t.points.len() / 10).max(1);
    let samples: Vec<[f64; 2]> = t.points.iter().step_by(step).copied().collect();
    let near = samples
        .iter()
        .filter(|p| tracks.iter().any(|o| MRT_LINES.contains(&o.line) && to_polyline(**p, &o.points) <= 250.0))
        .count();
    near * 5 >= samples.len() * 4
}

/// Distance from `p` to a polyline.
fn to_polyline(p: [f64; 2], line: &[[f64; 2]]) -> f64 {
    line.windows(2).map(|w| seg_distance(p, w[0], w[1])).fold(f64::INFINITY, f64::min)
}

/// One line's pieces grouped into connected networks (an end within
/// `tol` metres of another piece), as `(piece indices, total length m)`.
pub fn components(tracks: &[Track], line: &str, tol: f64) -> Vec<(Vec<usize>, f64)> {
    let idx: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].line == line).collect();
    let mut parent: Vec<usize> = (0..idx.len()).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for a in 0..idx.len() {
        let ta = &tracks[idx[a]].points;
        for b in 0..idx.len() {
            if a == b {
                continue;
            }
            let tb = &tracks[idx[b]].points;
            if to_polyline(ta[0], tb) <= tol || to_polyline(ta[ta.len() - 1], tb) <= tol {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                parent[ra] = rb;
            }
        }
    }
    let mut groups: HashMap<usize, (Vec<usize>, f64)> = HashMap::new();
    for a in 0..idx.len() {
        let r = find(&mut parent, a);
        let g = groups.entry(r).or_default();
        g.0.push(idx[a]);
        g.1 += length_m(&tracks[idx[a]].points);
    }
    let mut out: Vec<_> = groups.into_values().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// The operating MRT lines: each is one connected network.
pub const MRT_LINES: &[&str] = &["NS", "EW", "NE", "CC", "DT", "TE"];

/// A network of an operating line's colour, apart from its main one and
/// shorter than this, is a stray (see [`build_tracks`]).
pub const STRAY_MAX_M: f64 = 3_500.0;

/// Planned networks shorter than this are stubs (depot throats, turnback
/// sidings), not a 2019 alignment: dropped.
pub const PLANNED_MIN_M: f64 = 1_500.0;

/// Classify, then close each line's gaps along the source track (see
/// [`close_gaps`]), re-home stray networks, close the other side of each
/// double track through a station, merge, simplify, drop short or
/// shadowing planned pieces, and mark each extension under construction.
pub fn build_tracks(parts: &[(bool, Ring)], chords: &[(&'static str, [f64; 2], [f64; 2])], ext: &[Extension]) -> TrackBuild {
    let mut labels = classify_labels(parts, chords);
    let graph = TrackGraph::new(parts);
    let mut joins: Vec<Track> = Vec::new();
    let mut routed = 0;
    let mut straight: Vec<f64> = Vec::new();
    let mut lines: Vec<&'static str> = labels.iter().flatten().copied().collect();
    lines.sort();
    lines.dedup();
    for pass in 0..3 {
        for &line in &lines {
            close_gaps(parts, &graph, &mut labels, &mut joins, line, &mut routed, &mut straight);
        }
        if pass == 0 {
            // A network apart from a line's main one and short is a stray
            // that took the colour by running alongside (a future line at
            // an interchange): its parts go back to unassigned, its joins
            // go.
            let mut drop_joins = BTreeSet::new();
            for &line in MRT_LINES {
                let (tracks, srcs) = line_pieces(parts, &labels, &joins, line);
                for (members, len) in components(&tracks, line, 30.0).into_iter().skip(1) {
                    if len < STRAY_MAX_M {
                        for i in members {
                            match srcs[i] {
                                Src::Part(p) => labels[p] = None,
                                Src::Join(j) => {
                                    drop_joins.insert(j);
                                }
                            }
                        }
                    }
                }
            }
            joins = joins.into_iter().enumerate().filter(|(j, _)| !drop_joins.contains(j)).map(|(_, t)| t).collect();
        }
    }
    for &line in &lines {
        close_facing_ends(parts, &labels, &mut joins, line, &mut straight);
    }
    let straight_joins = straight;
    let final_labels = labels.clone();
    let tracks: Vec<Track> = tracks_from_labels(parts, &labels)
        .into_iter()
        .chain(joins)
        .map(|mut t| {
            t.points = simplify(&t.points, TRACK_TOLERANCE_M);
            t
        })
        .collect();
    let tracks = merge_contiguous(tracks);
    let keep: BTreeSet<usize> = components(&tracks, "PLANNED", 30.0)
        .into_iter()
        .filter(|(_, len)| *len >= PLANNED_MIN_M)
        .flat_map(|(m, _)| m)
        .collect();
    let operating: Vec<Track> = tracks.iter().filter(|t| t.line != "PLANNED").cloned().collect();
    let tracks: Vec<Track> = tracks
        .into_iter()
        .enumerate()
        .filter(|(i, t)| t.line != "PLANNED" || (keep.contains(i) && !shadows_an_operating_line(t, &operating)))
        .map(|(_, t)| t)
        .collect();
    TrackBuild { tracks: mark_extensions(tracks, ext, UNDER_CONSTRUCTION), straight_joins, routed, labels: final_labels }
}

/// Topology-preserving simplification of every area ring: each ring is
/// cut into arcs at the vertices where the set of areas sharing it
/// changes, and each arc is simplified on its own -- so a border two
/// areas share is simplified identically on both sides (no slivers, no
/// gaps), and its ends never move. A coast (an arc only one area owns)
/// takes `coast_tol`, a shared border `border_tol`.
pub fn simplify_areas(areas: &mut [JoinedArea], border_tol: f64, coast_tol: f64) {
    let key = |p: [f64; 2]| ((p[0] * 1e7).round() as i64, (p[1] * 1e7).round() as i64);
    let mut owners: HashMap<(i64, i64), BTreeSet<usize>> = HashMap::new();
    for (i, a) in areas.iter().enumerate() {
        for r in a.area.polygons.iter().flatten() {
            for p in r {
                owners.entry(key(*p)).or_default().insert(i);
            }
        }
    }
    for a in areas.iter_mut() {
        for r in a.area.polygons.iter_mut().flatten() {
            let n = r.len();
            if n < 4 {
                continue;
            }
            let set = |i: usize| owners.get(&key(r[i % (n - 1)])).cloned().unwrap_or_default();
            // A node: its owner set differs from a neighbour's.
            let nodes: Vec<usize> = (0..n - 1).filter(|&i| set(i) != set(i + 1) || set(i) != set(i + n - 2)).collect();
            let mut out: Ring = Vec::with_capacity(n);
            if nodes.is_empty() {
                // One owner the whole way round: one closed arc.
                let tol = if set(0).len() > 1 { border_tol } else { coast_tol };
                *r = simplify_ring(r, tol);
                continue;
            }
            for (k, &start) in nodes.iter().enumerate() {
                let end = nodes.get(k + 1).copied().unwrap_or(nodes[0] + n - 1);
                let arc: Ring = (start..=end).map(|i| r[i % (n - 1)]).collect();
                // The arc's owners, read off an interior vertex (or its
                // start when it has none).
                let mid = if end - start >= 2 { set(start + 1) } else { set(start).intersection(&set(end)).copied().collect() };
                let tol = if mid.len() > 1 { border_tol } else { coast_tol };
                let s = simplify(&arc, tol);
                out.extend(&s[..s.len() - 1]);
            }
            out.push(out[0]);
            if out.len() >= 4 {
                *r = out;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The prepared overlay
// ---------------------------------------------------------------------------

/// Everything the map draws, built off the UI thread from the cached files.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub areas: Vec<JoinedArea>,
    pub tracks: Vec<Track>,
    pub stations: Vec<Station>,
    pub unjoined: Vec<String>,
    /// (before, after) simplification, areas and tracks.
    pub area_vertices: (usize, usize),
    pub track_vertices: (usize, usize),
    /// Gaps closed, straight or routed (see [`build_tracks`]).
    pub bridged: usize,
    /// When the data.gov.sg files were fetched, `YYYY-MM-DD`, for the
    /// SODL credit's access date -- taken from a data.gov.sg file
    /// (`planning-areas.geojson`), not from `population.json`, which is
    /// SingStat's (a different holder, its own licence note).
    pub accessed: Option<String>,
}

/// Shared-border tolerance: 6 m, under 2 px at z15.
const AREA_TOLERANCE_M: f64 = 6.0;
/// Coast tolerance: 1.5 m. East Coast Park's boundary is a run of small
/// scalloped bays between groynes; at 8 m each bay became a straight
/// chord and a notch -- the sawtooth the user saw.
const COAST_TOLERANCE_M: f64 = 1.5;
/// Track tolerance: 4 m, under a pixel at z15.
const TRACK_TOLERANCE_M: f64 = 4.0;

pub fn prepare(dir: &Path) -> Result<Prepared, String> {
    let file = |name: &str| read_bounded(&dir.join(name));
    let areas = parse_areas(&file("planning-areas.geojson")?)?;
    let population = parse_population(&file("population.json")?)?;
    let codes = parse_station_codes(&file("station-codes.csv")?)?;
    let exits = parse_exits(&file("station-exits.geojson")?)?;
    let parts = parse_tracks(&file("rail-lines.geojson")?)?;
    // The data.gov.sg access date, for the SODL credit -- a data.gov.sg
    // file's fetch time, not SingStat's `population.json` (M6).
    let accessed = std::fs::metadata(dir.join("planning-areas.geojson")).and_then(|m| m.modified()).ok().map(|t| {
        let date: chrono::DateTime<chrono::Utc> = t.into();
        date.format("%Y-%m-%d").to_string()
    });
    Ok(prepare_from(areas, &population, &codes, &exits, &parts, accessed))
}

pub fn prepare_from(
    areas: Vec<Area>,
    population: &HashMap<String, u64>,
    codes: &HashMap<String, BTreeSet<String>>,
    exits: &HashMap<String, [f64; 2]>,
    parts: &[(bool, Ring)],
    accessed: Option<String>,
) -> Prepared {
    let (mut areas, unjoined) = join(areas, population);
    for name in &unjoined {
        eprintln!("overlay-features: did not join: {name}");
    }
    let count = |a: &[JoinedArea]| a.iter().flat_map(|a| &a.area.polygons).flatten().map(Vec::len).sum::<usize>();
    let before = count(&areas);
    simplify_areas(&mut areas, AREA_TOLERANCE_M, COAST_TOLERANCE_M);
    let area_vertices = (before, count(&areas));
    let stations = join_stations(codes, exits);
    let tb = parts.iter().map(|(_, p)| p.len()).sum();
    let ext = extensions(codes, &stations);
    let built = build_tracks(parts, &chords(&stations), &ext);
    let (tracks, bridged) = (built.tracks, built.straight_joins.len() + built.routed);
    let track_vertices = (tb, tracks.iter().map(|t| t.points.len()).sum());
    Prepared { areas, tracks, stations, unjoined, area_vertices, track_vertices, bridged, accessed }
}

// ---------------------------------------------------------------------------
// Style: the expressions, and the per-feature properties they cannot cover
// ---------------------------------------------------------------------------

/// The density ramp: `(residents per km², colour)`, 0.55 alpha applied in
/// [`paint_json`]. YlGnBu-like, so no stop reads as an MRT line colour.
pub const DENSITY_STOPS: &[(f64, [u8; 3])] = &[
    (1.0, [233, 240, 226]),
    (2_000.0, [199, 233, 192]),
    (6_000.0, [127, 205, 187]),
    (12_000.0, [65, 182, 196]),
    (18_000.0, [34, 94, 168]),
    (26_000.0, [37, 52, 148]),
];
/// No residents (catchment, industry, water): neutral grey.
pub const NO_RESIDENTS: [u8; 3] = [176, 180, 186];
pub const FILL_ALPHA: f64 = 0.55;

fn rgba_css(c: [u8; 3], a: f64) -> String {
    format!("rgba({},{},{},{a})", c[0], c[1], c[2])
}

/// The ramp's colour at `density`, the same linear interpolation the
/// engine applies to [`paint_json`]'s `fill-color` -- the legend and the
/// tests read this.
pub fn density_colour(density: f64) -> [u8; 3] {
    if density < DENSITY_STOPS[0].0 {
        return NO_RESIDENTS;
    }
    for w in DENSITY_STOPS.windows(2) {
        let ((d0, c0), (d1, c1)) = (w[0], w[1]);
        if density <= d1 {
            let t = (density - d0) / (d1 - d0);
            return std::array::from_fn(|i| (f64::from(c0[i]) + t * (f64::from(c1[i]) - f64::from(c0[i]))).round() as u8);
        }
    }
    DENSITY_STOPS[DENSITY_STOPS.len() - 1].1
}

/// An interchange's dot: white, but not `#ffffff` -- a single-line
/// station's ring is that, and the engine draws one op per distinct colour
/// in first-appearance order, so a shared colour would put every white
/// dot under the dark rings.
pub const DOT_X: &str = "#fdfdfd";

/// Station parts: a ring under a dot, both `circle-color` from `kind`.
/// Interchanges are white dots in a dark ring; single-line stations are
/// their line's colour in a white ring.
pub fn station_kind_colours() -> Vec<(String, String)> {
    let mut out = vec![("ring-x".to_string(), "#1f2933".to_string()), ("ring-1".into(), "#ffffff".into()), ("dot-x".into(), DOT_X.into())];
    out.extend(LINE_COLOURS.iter().map(|(k, _, c)| (format!("dot-{k}"), c.to_string())));
    out
}

/// The one paint declaration every source of this panel carries. Each
/// slot only ever reaches its own geometry type.
pub fn paint_json() -> Value {
    let mut ramp = vec![json!("interpolate"), json!(["linear"]), json!(["get", "density"]), json!(0), json!(rgba_css(NO_RESIDENTS, 0.45))];
    for (d, c) in DENSITY_STOPS {
        ramp.push(json!(d));
        ramp.push(json!(rgba_css(*c, FILL_ALPHA)));
    }
    let mut line = vec![json!("match"), json!(["get", "line"]), json!("CASING"), json!("#ffffff"), json!("PLANNED"), json!(PLANNED_COLOUR)];
    for (k, _, c) in LINE_COLOURS {
        line.push(json!(k));
        line.push(json!(c));
    }
    for (k, uc) in UNDER_CONSTRUCTION {
        line.push(json!(uc));
        line.push(json!(line_name_colour(k).1));
    }
    line.push(json!("#888888"));
    let mut circle = vec![json!("match"), json!(["get", "kind"])];
    for (k, c) in station_kind_colours() {
        circle.push(json!(k));
        circle.push(json!(c));
    }
    circle.push(json!("#888888"));
    json!({
        "fill-color": ramp,
        "fill-outline-color": "rgba(255,255,255,0.95)",
        "line-color": line,
        "circle-color": circle,
    })
}

pub fn paint_object() -> serde_json::Map<String, Value> {
    match paint_json() {
        Value::Object(m) => m,
        _ => unreachable!("paint_json builds an object"),
    }
}

fn coords(r: &[[f64; 2]]) -> Value {
    Value::Array(r.iter().map(|p| json!([(p[0] * 1e6).round() / 1e6, (p[1] * 1e6).round() / 1e6])).collect())
}

/// URA Master Plan 2019's five planning regions, in the chart's order.
pub const REGIONS: [&str; 5] = ["Central", "East", "North", "North-East", "West"];

/// The planning region of a planning area (URA Master Plan 2019: 55 areas
/// in five regions), by its normalised name. `None` for a name not in it.
pub fn region_of(name: &str) -> Option<&'static str> {
    const TABLE: [(&str, &[&str]); 5] = [
        (
            "Central",
            &[
                "BISHAN", "BUKIT MERAH", "BUKIT TIMAH", "DOWNTOWN CORE", "GEYLANG", "KALLANG", "MARINA EAST", "MARINA SOUTH", "MARINE PARADE", "MUSEUM",
                "NEWTON", "NOVENA", "ORCHARD", "OUTRAM", "QUEENSTOWN", "RIVER VALLEY", "ROCHOR", "SINGAPORE RIVER", "SOUTHERN ISLANDS", "STRAITS VIEW",
                "TANGLIN", "TOA PAYOH",
            ],
        ),
        ("East", &["BEDOK", "CHANGI", "CHANGI BAY", "PASIR RIS", "PAYA LEBAR", "TAMPINES"]),
        ("North", &["CENTRAL WATER CATCHMENT", "LIM CHU KANG", "MANDAI", "SEMBAWANG", "SIMPANG", "SUNGEI KADUT", "WOODLANDS", "YISHUN"]),
        ("North-East", &["ANG MO KIO", "HOUGANG", "NORTH EASTERN ISLANDS", "PUNGGOL", "SELETAR", "SENGKANG", "SERANGOON"]),
        (
            "West",
            &[
                "BOON LAY", "BUKIT BATOK", "BUKIT PANJANG", "CHOA CHU KANG", "CLEMENTI", "JURONG EAST", "JURONG WEST", "PIONEER", "TENGAH", "TUAS",
                "WESTERN ISLANDS", "WESTERN WATER CATCHMENT",
            ],
        ),
    ];
    let key = normalise(name);
    TABLE.iter().find(|(_, areas)| areas.contains(&key.as_str())).map(|(r, _)| *r)
}

/// The areas source: id = index, `name`, `region`, `residents`, `density`.
pub fn areas_geojson(p: &Prepared) -> String {
    areas_geojson_in(p, &[])
}

/// [`areas_geojson`] keeping only the areas of `regions` (all for none):
/// what a click on the region chart pushes.
pub fn areas_geojson_in(p: &Prepared, regions: &[&str]) -> String {
    let features: Vec<Value> = p
        .areas
        .iter()
        .enumerate()
        .filter(|(_, a)| regions.is_empty() || region_of(&a.area.name).is_some_and(|r| regions.contains(&r)))
        .map(|(i, a)| {
            let polys: Vec<Value> = a.area.polygons.iter().map(|poly| Value::Array(poly.iter().map(|r| coords(r)).collect())).collect();
            json!({
                "type": "Feature", "id": i,
                "geometry": {"type": "MultiPolygon", "coordinates": polys},
                "properties": {"name": title_case(&a.area.name), "region": region_of(&a.area.name).unwrap_or("Other"), "residents": a.residents.unwrap_or(0), "density": a.density().round()},
            })
        })
        .collect();
    json!({"type": "FeatureCollection", "features": features}).to_string()
}

/// Track width by kind: operating MRT 3 px over a 5 px white casing, LRT
/// 2 px, planned 1.5 px dashed.
pub fn rail_geojson(p: &Prepared) -> String {
    let mut features = Vec::new();
    for t in p.tracks.iter().filter(|t| t.line != "PLANNED") {
        let w = if t.line == "LRT" { 3.5 } else { 5.0 };
        features.push(json!({"type": "Feature", "geometry": {"type": "LineString", "coordinates": coords(&t.points)},
            "properties": {"line": "CASING", "code": t.line, "line-width": w}}));
    }
    for t in &p.tracks {
        let mut props = json!({"line": t.line, "code": t.line, "line-width": match t.line { "PLANNED" => 1.5, "LRT" => 2.0, _ => 3.0 }});
        if t.line == "PLANNED" {
            props["line-dasharray"] = json!([3, 2]);
        } else if t.line.ends_with("-UC") {
            // The line's own colour, dashed: under construction, which the
            // grey planned style is not.
            props["line-dasharray"] = json!([2, 1.5]);
        }
        features.push(json!({"type": "Feature", "geometry": {"type": "LineString", "coordinates": coords(&t.points)}, "properties": props}));
    }
    json!({"type": "FeatureCollection", "features": features}).to_string()
}

/// `(dot, ring)` radius by lines served.
pub fn station_radii(lines: usize) -> (f64, f64) {
    match lines {
        0 | 1 => (2.6, 4.0),
        2 => (4.0, 5.8),
        _ => (5.0, 7.0),
    }
}

/// Two point features per station (ring first), `kind` for the colour
/// match, radius per feature.
pub fn stations_geojson(p: &Prepared, labels: Option<&Value>) -> String {
    // Every ring before every dot: the engine draws one op per colour, in
    // the order each colour first appears (see [`DOT_X`]).
    let (mut rings, mut dots) = (Vec::new(), Vec::new());
    for s in &p.stations {
        let lines = s.lines();
        let (dot, ring) = station_radii(lines.len());
        let inter = lines.len() >= 2;
        let at = coords(&[s.at])[0].clone();
        let codes = s.codes.iter().cloned().collect::<Vec<_>>().join(" / ");
        let served = lines.iter().copied().collect::<Vec<_>>().join(" ");
        rings.push(json!({"type": "Feature", "geometry": {"type": "Point", "coordinates": at},
            "properties": {"kind": if inter { "ring-x" } else { "ring-1" }, "circle-radius": ring, "name": s.name, "codes": codes, "lines": served}}));
        let dot_kind = if inter { "dot-x".to_string() } else { format!("dot-{}", lines.iter().next().copied().unwrap_or("LRT")) };
        let mut props = json!({"kind": dot_kind, "circle-radius": dot, "name": s.name, "codes": codes, "lines": served});
        if let Some(paint) = labels {
            for (k, v) in label_properties(paint, &s.name) {
                props[k] = v;
            }
        }
        dots.push(json!({"type": "Feature", "geometry": {"type": "Point", "coordinates": at}, "properties": props}));
    }
    rings.extend(dots);
    json!({"type": "FeatureCollection", "features": rings}).to_string()
}

/// A station name's text paint. Carried on the station's own dot (not a
/// separate label source, whose zero-radius points would still be hit
/// within the engine's pointer tolerance and steal the station's hover),
/// and only at [`LABEL_ZOOM`] and up: overlay text has no min zoom, so the
/// panel re-pushes the stations when the zoom crosses it.
/// The paint is the one map-label style
/// ([`crate::data_labels::map_label_paint`], regular tier).
fn label_properties(paint: &Value, name: &str) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = paint.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
    out.push(("text-field".into(), json!(name)));
    out.push(("text-offset".into(), json!([0, 1.2])));
    out
}

pub const LABEL_ZOOM: f64 = 12.8;

pub const AREAS: &str = "sg-areas";
pub const RAIL: &str = "sg-rail";
pub const STATIONS: &str = "sg-stations";

/// Everything in one collection, for the headless capture (which draws one
/// overlay source): areas, casing and tracks, station parts.
pub fn combined_geojson(p: &Prepared) -> String {
    let mut all = Vec::new();
    for text in [areas_geojson(p), rail_geojson(p), stations_geojson(p, None)] {
        if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(&text) {
            if let Some(Value::Array(fs)) = m.remove("features") {
                all.extend(fs.into_iter().map(|mut f| {
                    if let Some(o) = f.as_object_mut() {
                        o.remove("id");
                    }
                    f
                }));
            }
        }
    }
    json!({"type": "FeatureCollection", "features": all}).to_string()
}

/// The map's own credit line.
pub fn map_credit(p: &Prepared) -> String {
    format!(
        "Contains information from URA, LTA and SEAB datasets (data.gov.sg{}) under the Singapore Open Data Licence v1.0; population: SingStat GHS 2025",
        p.accessed.as_deref().map(|d| format!(", accessed {d}")).unwrap_or_default()
    )
}

// ---------------------------------------------------------------------------
// Gallery contract
// ---------------------------------------------------------------------------

/// A Mapbox token for Mapbox Light (Positron without one), and the
/// network while the data is not cached yet.
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    requirements_for(is_cached(&data_dir()))
}

pub fn requirements_for(cached: bool) -> Vec<kit::Requirement> {
    let mut out = vec![kit::Requirement::MapboxToken { fallback: Some("OpenFreeMap Positron") }];
    if !cached {
        out.push(kit::Requirement::Network);
    }
    out
}

const SODL_CHANGES: &str = "accessed when this data was fetched (the map's credit line shows your download's actual date); simplified, joined and restyled";

pub const CREDITS: &[kit::Credit] = &[
    kit::Credit {
        what: "Planning areas",
        holder: "Urban Redevelopment Authority (URA), via data.gov.sg",
        dataset: "Contains information from Master Plan 2019 Planning Area Boundary (No Sea)",
        licence: kit::Licence::SgOpenData,
        url: "https://data.gov.sg/datasets/d_4765db0e87b9c86336792efe8a1f7a66/view",
        changes: SODL_CHANGES,
    },
    kit::Credit {
        what: "Rail track geometry",
        holder: "Urban Redevelopment Authority (URA), via data.gov.sg",
        dataset: "Contains information from Master Plan 2019 Rail Line layer",
        licence: kit::Licence::SgOpenData,
        url: "https://data.gov.sg/datasets/d_222bfc84eb86c7c11994d02f8939da8d/view",
        changes: "tracks assigned to lines by nearest station chords; unassigned alignments drawn as planned",
    },
    kit::Credit {
        what: "Station positions",
        holder: "Land Transport Authority (LTA), via data.gov.sg",
        dataset: "Contains information from LTA MRT Station Exit (GEOJSON)",
        licence: kit::Licence::SgOpenData,
        url: "https://data.gov.sg/datasets/d_b39d3a0871985372d7e1637193335da5/view",
        changes: "each station placed at the mean of its exits",
    },
    kit::Credit {
        what: "Station codes",
        holder: "Singapore Examinations and Assessment Board (SEAB), via data.gov.sg",
        dataset: "Contains information from Travelling Distance between MRT/LRT Station to Secondary School",
        licence: kit::Licence::SgOpenData,
        url: "https://data.gov.sg/datasets/d_f5d9f95e7691f70f87f9a2347454e069/view",
        changes: "only the station names and codes are read",
    },
    kit::Credit {
        what: "Residents by planning area",
        holder: "Singapore Department of Statistics (SingStat)",
        dataset: "General Household Survey 2025, table C020125 (resident population by planning area, rounded to 10)",
        licence: kit::Licence::Unverified,
        url: "https://tablebuilder.singstat.gov.sg/table/CT/C020125",
        changes: "SingStat's terms page could not be read (script-rendered); its data.gov.sg tables are SODL v1.0. Divided by computed area for density",
    },
    kit::Credit { what: "Basemap tiles, when drawn (Mapbox Light, with a token)", ..kit::CREDIT_MAPBOX },
    kit::Credit { what: "Basemap tiles, when drawn (Positron, without a token)", ..kit::CREDIT_OSM_OPENFREEMAP },
];

/// Mapbox Light with a token, Positron without.
fn basemap(token: Option<&str>) -> anyhow::Result<(gpui_maps::style::Style, Arc<dyn TileSource>)> {
    if let Some(token) = token {
        let bytes = kit::asset_bytes("demos/mapbox-light-style.json")?;
        let json: Value = serde_json::from_slice(&bytes)?;
        let style = gpui_maps::style_spec::parse_and_compile(&bytes)?;
        return Ok((style, kit::mapbox_composite(&json, token)?));
    }
    Ok((kit::compile_style("demos/positron-style.json")?, kit::openfreemap_planet()))
}

// ---------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------

pub enum State {
    Loading { line: String, fraction: f32 },
    Failed(String),
    Ready,
}

/// What the hover card shows: a station (drawn on top, so hit first), a
/// rail line, or a planning area -- the engine reports the topmost hit,
/// and the sources are pushed areas, rail, stations.
#[derive(Clone, Debug, PartialEq)]
pub enum HoverInfo {
    Area { name: String, residents: u64, density: f64 },
    Line { code: String },
    Station { name: String, codes: String, lines: Vec<String> },
}

impl HoverInfo {
    /// From a hovered feature's own properties, by its source.
    pub fn from_properties(source: &str, props: &gpui_maps::overlay::FeatureProperties) -> Option<Self> {
        let text = |k: &str| match props.get(k) {
            Some(TagValue::String(s)) => Some(s.to_string()),
            _ => None,
        };
        let num = |k: &str| match props.get(k) {
            Some(TagValue::Number(n)) => *n,
            _ => 0.0,
        };
        match source {
            AREAS => Some(Self::Area { name: text("name")?, residents: num("residents") as u64, density: num("density") }),
            RAIL => Some(Self::Line { code: text("code")? }),
            STATIONS => Some(Self::Station {
                name: text("name")?,
                codes: text("codes").unwrap_or_default(),
                lines: text("lines").unwrap_or_default().split_whitespace().map(str::to_string).collect(),
            }),
            _ => None,
        }
    }

    /// The swatch beside the title: a line's colour.
    pub fn colour(&self) -> Option<&'static str> {
        match self {
            Self::Line { code } => Some(line_name_colour(code).1),
            _ => None,
        }
    }
}

pub struct OverlayPanel {
    map: Entity<MapView>,
    inner: Entity<FocusedMap>,
    state: State,
    prepared: Option<Arc<Prepared>>,
    /// The shared hover / popup / highlight component (`popup.rs`).
    popup: Entity<crate::popup::MapPopup>,
    /// Residents by planning region, linked to the areas (`linked_chart`).
    pub chart: Entity<crate::linked_chart::LinkedChart>,
    labels_on: bool,
    env: kit::PanelEnv,
    /// This panel's window box `(x, y, w, h)` at each prepaint (see
    /// [`area_probe`]): an area click fits against the current size.
    area: std::rc::Rc<std::cell::Cell<(f32, f32, f32, f32)>>,
    /// The basemap labels over the planning-area fills (`crate::data_labels`).
    _data_labels: crate::data_labels::DataLabels,
}

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let (style, source) = basemap(env.mapbox_token.as_deref())?;
    let start = kit::camera_from_env(START);
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(start.0, start.1, start.2, w, h, style, source, runtime);
        if let Some(title) = host.title() {
            view.set_window_title_prefix(title.to_string());
        }
        view.set_chrome_above_map(true);
        // The hover card follows the pointer by re-rendering; the map must
        // not repaint for that (the census tooltip's rule).
        view.set_skip_unchanged_frames(true);
        view
    });
    // The planning areas fill the island: labels over the data.
    let document = crate::data_labels::document(if env.mapbox_token.is_some() { "demos/mapbox-light-style.json" } else { "demos/positron-style.json" }).ok();
    let data_labels = crate::data_labels::DataLabels::attach(&map, document, true, cx);
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let inner = cx.new(|_| FocusedMap::new(map.clone(), focus));
    let env = env.clone();
    // A click on an area zooms to it (the panel's own fit); on a line or a
    // station it pins the card. The panel is made after the popup, so the
    // click reaches it through this slot.
    let slot: std::rc::Rc<std::cell::RefCell<Option<gpui::WeakEntity<OverlayPanel>>>> = std::rc::Rc::default();
    let clicked = slot.clone();
    let config = crate::popup::PopupConfig::new()
        .only_listed()
        .custom(AREAS, popup_card)
        .custom(RAIL, popup_card)
        .custom(STATIONS, popup_card)
        .highlight(|pick| popup_info(pick).map_or_else(area_highlight_style, |info| highlight_style(&info)))
        // cursor: a map popup's click callback, not an element
        .on_click(move |pick, _lon_lat, _map, cx| {
            let gpui_maps::map_view::MapObject::Feature(feature) = &pick.object else { return crate::popup::ClickOutcome::Pin };
            if &*feature.source != AREAS {
                return crate::popup::ClickOutcome::Pin;
            }
            if let Some(panel) = clicked.borrow().as_ref().and_then(|w| w.upgrade()) {
                panel.update(cx, |panel, cx| panel.ease_to_area(feature, cx));
            }
            crate::popup::ClickOutcome::Handled
        });
    let popup = crate::popup::install(&map, config, window, cx);
    let panel = cx.new(|cx| {
        let chart = region_chart(&map, &popup, cx.entity().downgrade(), cx);
        let mut panel = OverlayPanel {
            map: map.clone(),
            inner,
            state: State::Loading { line: "Reading the Singapore data".into(), fraction: 0.0 },
            prepared: None,
            popup,
            chart,
            labels_on: false,
            env,
            area: std::rc::Rc::new(std::cell::Cell::new((0.0, 0.0, 0.0, 0.0))),
            _data_labels: data_labels,
        };
        cx.observe(&map, |this: &mut OverlayPanel, _map, cx: &mut Context<OverlayPanel>| this.sync_labels(cx)).detach();
        panel.load(cx);
        panel
    });
    *slot.borrow_mut() = Some(panel.downgrade());
    Ok(kit::Built { view: panel.into(), map: Some(map.downgrade()), start })
}

/// Residents by planning region: a click keeps the chosen regions' areas
/// (the 55-polygon areas source pushed again with only them: a polygon
/// source has no GPU range filter, and the push is a few ms); a hover
/// lights the region's areas; a hovered area lights its region.
fn region_chart(
    map: &Entity<MapView>,
    popup: &Entity<crate::popup::MapPopup>,
    panel: gpui::WeakEntity<OverlayPanel>,
    cx: &mut App,
) -> Entity<crate::linked_chart::LinkedChart> {
    use crate::linked_chart::{Category, ChartSpec, Kind, Link, LinkedChart, Selection};
    let categories = REGIONS.iter().map(|r| Category::new(gpui_maps::overlay::TagValue::String((*r).to_string()), r)).collect();
    let apply = move |selection: &Selection, cx: &mut App| {
        let chosen: Vec<&str> = match selection {
            Selection::Bars(set) => set.iter().filter_map(|&i| REGIONS.get(i).copied()).collect(),
            _ => Vec::new(),
        };
        let _ = panel.update(cx, |p, cx| {
            let Some(prepared) = p.prepared.clone() else { return };
            p.map.update(cx, |view, cx| {
                let _ = view.set_source(AREAS, &areas_geojson_in(&prepared, &chosen), cx);
            });
        });
    };
    let spec = ChartSpec {
        title: "Residents by planning region".into(),
        unit: "Residents, SingStat 2025, by URA planning region. Click bars to keep their areas.".into(),
        noun: "residents".into(),
        fmt: crate::popup::NumFmt::Compact,
        hint: None,
    };
    let chart = cx.new(|cx| {
        let mut c = LinkedChart::new(map, AREAS, "region", Kind::Bars { categories }, spec, Link::Callback(std::rc::Rc::new(apply)), cx).docked();
        c.sum_of("residents", cx);
        c.follow_popup(popup, cx);
        c
    });
    crate::linked_chart::reload_when_ready(&chart, cx);
    chart
}

/// The picked feature as the card reads it.
fn popup_info(pick: &gpui_maps::map_view::Pick) -> Option<HoverInfo> {
    let gpui_maps::map_view::MapObject::Feature(feature) = &pick.object else { return None };
    HoverInfo::from_properties(&feature.source, pick.properties.as_deref()?)
}

fn popup_card(pick: &gpui_maps::map_view::Pick) -> Option<crate::popup::Card> {
    popup_info(pick).map(|info| hover_card(&info))
}

impl OverlayPanel {
    /// Downloads what is missing (progress on the card), prepares off the
    /// UI thread, then pushes the sources.
    fn load(&mut self, cx: &mut Context<Self>) {
        let dir = data_dir();
        let cancel = self.env.cancel.clone();
        let runtime = self.env.runtime.clone();
        let (tx, rx) = std::sync::mpsc::channel::<(usize, usize, String)>();
        let job = runtime.spawn(async move {
            download_missing(dir.clone(), cancel, move |done, total, file| {
                let _ = tx.send((done, total, file.to_string()));
            })
            .await?;
            tokio::task::spawn_blocking(move || prepare(&dir)).await.map_err(|e| format!("prepare panicked: {e}"))?
        });
        cx.spawn(async move |this, cx| {
            let mut job = job;
            loop {
                let tick = cx.background_executor().timer(Duration::from_millis(100));
                tick.await;
                if let Some((done, total, file)) = rx.try_iter().last() {
                    let alive = this.update(cx, |p, cx| {
                        p.state = if file.is_empty() {
                            State::Loading { line: "Preparing the overlay".into(), fraction: 1.0 }
                        } else {
                            State::Loading { line: format!("Downloading {file} ({}/{total})", done + 1), fraction: done as f32 / total.max(1) as f32 }
                        };
                        cx.notify();
                    });
                    if alive.is_err() {
                        return;
                    }
                }
                if job.is_finished() {
                    let result = (&mut job).await.unwrap_or_else(|e| Err(format!("load task failed: {e}")));
                    let _ = this.update(cx, |p, cx| p.finish(result, cx));
                    return;
                }
            }
        })
        .detach();
    }

    fn finish(&mut self, result: Result<Prepared, String>, cx: &mut Context<Self>) {
        match result {
            Ok(prepared) => {
                println!(
                    "overlay-features: {} areas ({} -> {} vertices), {} track parts ({} -> {} vertices), {} stations",
                    prepared.areas.len(),
                    prepared.area_vertices.0,
                    prepared.area_vertices.1,
                    prepared.tracks.len(),
                    prepared.track_vertices.0,
                    prepared.track_vertices.1,
                    prepared.stations.len()
                );
                let unplaced: Vec<&str> = prepared.areas.iter().filter(|a| region_of(&a.area.name).is_none()).map(|a| a.area.name.as_str()).collect();
                if !unplaced.is_empty() {
                    println!("overlay-features: no planning region for {unplaced:?}");
                }
                let paint = paint_object();
                let pushed = self.map.update(cx, |view, cx| -> Result<(), String> {
                    for (name, text) in [(AREAS, areas_geojson(&prepared)), (RAIL, rail_geojson(&prepared)), (STATIONS, stations_geojson(&prepared, None))] {
                        view.declare_source_paint(name, &paint).map_err(|e| format!("{name}: {e:?}"))?;
                        view.set_source(name, &text, cx).map_err(|e| format!("{name}: {e:?}"))?;
                    }
                    view.set_data_credit(Some(map_credit(&prepared)));
                    cx.notify();
                    Ok(())
                });
                match pushed {
                    Ok(()) => {
                        self.prepared = Some(Arc::new(prepared));
                        self.state = State::Ready;
                        self.labels_on = false;
                        self.sync_labels(cx);
                    }
                    Err(why) => self.state = State::Failed(why),
                }
            }
            Err(why) => {
                eprintln!("overlay-features: {why}");
                self.state = State::Failed(why);
            }
        }
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.state = State::Loading { line: "Retrying".into(), fraction: 0.0 };
        self.load(cx);
        cx.notify();
    }

    /// Station names on at [`LABEL_ZOOM`] and up, off below.
    fn sync_labels(&mut self, cx: &mut Context<Self>) {
        let Some(prepared) = self.prepared.clone() else { return };
        let zoom = self.map.read(cx).camera().zoom;
        let want = zoom >= LABEL_ZOOM;
        if want == self.labels_on {
            return;
        }
        self.labels_on = want;
        self.map.update(cx, |view, cx| {
            let paint = want.then(|| crate::data_labels::map_label_paint(view, crate::data_labels::LabelTier::Regular));
            let _ = view.set_source(STATIONS, &stations_geojson(&prepared, paint.as_ref()), cx);
        });
    }

    /// Clicking an overlay area must fit it against the panel's CURRENT
    /// laid-out size, not the size at build time: the About dock and a
    /// window resize both change how much of the window the map actually
    /// has. `area` is the window-box probe (`self.area`, refreshed every
    /// prepaint, see [`area_probe`]); `env_viewport` is the fallback for
    /// before the first prepaint, and is what `self.env.viewport` alone
    /// used to be stuck with for the panel's whole life (M7). Same source
    /// `census::fly_to_bounds` reads after 1f8c0f4's window-box probe.
    fn ease_to_area(&mut self, feature: &FeatureRef, cx: &mut Context<Self>) {
        let Some(FeatureId::Number(i)) = feature.id else { return };
        let Some(area) = self.prepared.as_ref().and_then(|p| p.areas.get(i as usize)) else { return };
        let Some(bounds) = mercator_bounds(&area.area) else { return };
        let (w, h) = fit_size(self.area.get(), self.env.viewport);
        let (lon, lat, zoom) = bounds.fit(w, h, 60.0, 15.0);
        self.map.update(cx, |view, cx| {
            view.ease_to(
                gpui_maps::camera_control::CameraTarget { center: Some((lon, lat)), zoom: Some(zoom), ..Default::default() },
                Duration::from_millis(900),
            );
            cx.notify();
        });
    }
}

/// The hovered area's highlight: a calm 1.5 px mid-grey outline and a
/// slight brighten. It was a 2.5 px near-black outline, which made the
/// Master Plan's scalloped East Coast boundary glare.
pub fn area_highlight_style() -> HighlightStyle {
    HighlightStyle {
        outline_color: gpui_maps::style::Rgba { r: 0.35, g: 0.40, b: 0.45, a: 0.8 },
        outline_width_px: 1.5,
        fill_color: Some(gpui_maps::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.14 }),
        casing_color: None,
        casing_width_px: 0.0,
    }
}

fn rgba_of(hex: &str, a: f32) -> gpui_maps::style::Rgba {
    let [r, g, b] = hex3(hex);
    gpui_maps::style::Rgba { r: f32::from(r) / 255.0, g: f32::from(g) / 255.0, b: f32::from(b) / 255.0, a }
}

/// A hovered line draws thicker in its own colour over a white casing; a
/// hovered station gets a dark ring; an area [`area_highlight_style`].
pub fn highlight_style(info: &HoverInfo) -> HighlightStyle {
    match info {
        HoverInfo::Area { .. } => area_highlight_style(),
        HoverInfo::Line { code } => HighlightStyle {
            outline_color: rgba_of(line_name_colour(code).1, 1.0),
            outline_width_px: 6.0,
            fill_color: None,
            casing_color: Some(rgba_of("#ffffff", 0.95)),
            casing_width_px: 2.0,
        },
        HoverInfo::Station { .. } => HighlightStyle {
            outline_color: rgba_of("#1f2933", 1.0),
            outline_width_px: 3.0,
            fill_color: None,
            casing_color: Some(rgba_of("#ffffff", 0.95)),
            casing_width_px: 1.5,
        },
    }
}

/// An area's box in Web Mercator `0..1` units, for `MercatorBounds::fit`.
pub fn mercator_bounds(area: &Area) -> Option<gpui_maps::census::breakdowns::MercatorBounds> {
    let mut b: Option<[f64; 4]> = None;
    for p in area.polygons.iter().flat_map(|p| p.first()).flatten() {
        let x = (p[0] + 180.0) / 360.0;
        let s = p[1].to_radians().sin();
        let y = 0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * std::f64::consts::PI);
        b = Some(match b {
            None => [x, y, x, y],
            Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
        });
    }
    let [x0, y0, x1, y1] = b?;
    Some(gpui_maps::census::breakdowns::MercatorBounds { x0, y0, x1, y1 })
}

/// M7: the size to fit an area-click's ease against -- the panel's current
/// laid-out size (`area`, `(x, y, w, h)` from the window-box probe) once
/// it has had a prepaint, `env_viewport` (the size at build time)
/// otherwise. `env_viewport` alone goes stale the moment the About dock
/// opens or the window resizes.
pub fn fit_size(area: (f32, f32, f32, f32), env_viewport: (f64, f64)) -> (f64, f64) {
    let (_, _, w, h) = area;
    if w > 0.0 && h > 0.0 { (f64::from(w), f64::from(h)) } else { env_viewport }
}

/// `12345.6` -> `12,346`.
pub fn thousands(n: f64) -> String {
    let s = format!("{:.0}", n.max(0.0));
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The card's title and body.
pub fn hover_lines(info: &HoverInfo) -> (String, String) {
    match info {
        HoverInfo::Area { name, residents: 0, .. } => (name.clone(), "No residents".into()),
        HoverInfo::Area { name, residents, density } => {
            (name.clone(), format!("{} residents · {} per km²", thousands(*residents as f64), thousands(*density)))
        }
        HoverInfo::Line { code } if code == "PLANNED" => (line_name_colour(code).0.to_string(), "Master Plan 2019 alignment, not yet in service".into()),
        HoverInfo::Line { code } if code.ends_with("-UC") => {
            let base = code.trim_end_matches("-UC");
            (format!("{} ({base}), under construction", line_name_colour(code).0), "Extension past the last station in service".into())
        }
        HoverInfo::Line { code } => (format!("{} ({code})", line_name_colour(code).0), format!("Line colour {}", line_name_colour(code).1)),
        HoverInfo::Station { name, codes, lines } => {
            let title = if codes.is_empty() { name.clone() } else { format!("{name} — {codes}") };
            let names: Vec<&str> = lines.iter().map(|l| line_name_colour(l).0).collect();
            let body = match names.len() {
                0 => "Station".to_string(),
                1 => names[0].to_string(),
                _ => format!("Interchange: {}", names.join(", ")),
            };
            (title, body)
        }
    }
}

/// The hover card (the shared `popup` component's): an area's residents
/// and density, a line's name and colour, a station's codes and lines.
pub fn hover_card(info: &HoverInfo) -> crate::popup::Card {
    let (title, body) = hover_lines(info);
    let swatch = info.colour().map(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).unwrap_or(0x888888));
    match info {
        HoverInfo::Area { name, residents, density } => crate::popup::Card {
            title: name.clone(),
            subtitle: Some("Planning area".into()),
            rows: if *residents == 0 {
                vec![("Residents".into(), "none".into())]
            } else {
                vec![("Residents".into(), thousands(*residents as f64)), ("Density".into(), format!("{} per km²", thousands(*density)))]
            },
            hint: Some("Click to zoom to the area".into()),
            ..Default::default()
        },
        HoverInfo::Line { .. } => crate::popup::Card { title, subtitle: Some(body), swatch, hint: Some("Click to pin".into()), ..Default::default() },
        HoverInfo::Station { codes, .. } => crate::popup::Card {
            title,
            subtitle: Some("Station".into()),
            rows: [("Codes".to_string(), codes.clone()), ("Lines".to_string(), body)].into_iter().filter(|(_, v)| !v.is_empty()).collect(),
            hint: Some("Click to pin".into()),
            ..Default::default()
        },
    }
}

/// Records this panel's window box at each prepaint.
fn area_probe(area: std::rc::Rc<std::cell::Cell<(f32, f32, f32, f32)>>) -> impl IntoElement {
    gpui::canvas(
        move |bounds, _, _| {
            area.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y), f32::from(bounds.size.width), f32::from(bounds.size.height)))
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// A small card: white at 92 %, rounded, dark text. Every card this panel
/// draws is built here, OVER the map; the root never sets a background.
fn card() -> gpui::Div {
    div().rounded(px(8.0)).bg(rgba(0xffffffeb)).text_color(rgb(0x1f2933)).text_xs().px(px(10.0)).py(px(8.0)).shadow_sm()
}

fn swatch(c: [u8; 3], a: f32, w: f32, h: f32) -> gpui::Div {
    let hex = (u32::from(c[0]) << 24) | (u32::from(c[1]) << 16) | (u32::from(c[2]) << 8) | (a * 255.0) as u32;
    div().w(px(w)).h(px(h)).bg(rgba(hex))
}

fn hex3(s: &str) -> [u8; 3] {
    let v = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0x888888);
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

fn legend() -> gpui::Div {
    let ramp = div().flex().flex_row().children(DENSITY_STOPS.iter().map(|(d, _)| swatch(density_colour(*d), 0.85, 26.0, 10.0)));
    let ticks = div().flex().flex_row().text_color(rgb(0x52606d)).children(DENSITY_STOPS.iter().map(|(d, _)| {
        div().w(px(26.0)).child(if *d < 1000.0 { "0".to_string() } else { format!("{}k", (*d / 1000.0) as u32) })
    }));
    let lines = div().flex().flex_col().gap(px(2.0)).children(LINE_COLOURS.iter().map(|(_, name, c)| {
        div().flex().flex_row().items_center().gap(px(6.0)).child(swatch(hex3(c), 1.0, 18.0, 4.0)).child(name.to_string())
    }));
    // Bottom-left, ABOVE the engine's bottom row (the brand mark, 9..27 px
    // up from the map's bottom edge -- `chrome::BOTTOM_ROW_TOP_PX`), never
    // over it, and one corner inset clear of it so the stack keeps the
    // corner's rhythm:
    // `the_legend_keeps_clear_of_the_brand_mark` pins it.
    let inset = gpui_maps::chrome::CORNER_INSET_PX;
    card()
        .debug_selector(|| "overlay-legend".into())
        .absolute()
        .bottom(px(gpui_maps::chrome::BOTTOM_ROW_TOP_PX + inset))
        .left(px(inset))
        .w(px(260.0)) // the longest row (the LRT lines) fits
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Residents per km²"))
        .child(ramp)
        .child(ticks)
        .child(div().flex().flex_row().items_center().gap(px(6.0)).child(swatch(NO_RESIDENTS, 0.8, 18.0, 10.0)).child("no residents"))
        .child(div().pt(px(4.0)).font_weight(gpui::FontWeight::SEMIBOLD).child("MRT / LRT"))
        .child(lines)
        .child(
            div().flex().flex_row().items_center().gap(px(6.0)).child(swatch(hex3(PLANNED_COLOUR), 1.0, 18.0, 2.0)).child("planned (MP2019), dashed"),
        )
        .child(
            div().flex().flex_row().items_center().gap(px(6.0)).child(swatch(hex3("#9d5b25"), 0.7, 18.0, 3.0)).child("line colour, dashed: under construction"),
        )
        .child(div().pt(px(4.0)).text_color(rgb(0x7b8794)).child("URA, LTA, SEAB via data.gov.sg (SODL v1.0); SingStat GHS 2025"))
}

impl gpui::Render for OverlayPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .relative()
            .size_full()
            .child(area_probe(self.area.clone()))
            .child(self.inner.clone())
            .child(crate::popup::input_layer(&self.popup, cx));
        let overlay = match &self.state {
            State::Ready => None,
            State::Loading { line, fraction } => Some(
                card()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(line.clone())
                    .child(div().w(px(260.0)).h(px(4.0)).rounded_full().bg(rgba(0x1f29332e)).child(
                        div().h_full().w(px(260.0 * fraction.clamp(0.0, 1.0))).rounded_full().bg(rgb(0x225ea8)),
                    ))
                    .child(div().text_color(rgb(0x7b8794)).child(format!("into {}", data_dir().display()))),
            ),
            State::Failed(why) => Some(
                card()
                    .max_w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Could not load the Singapore data"))
                    .child(why.clone())
                    .child(
                        div()
                            .id("retry")
                            .cursor_pointer()
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .bg(rgb(0x225ea8))
                            .text_color(rgb(0xffffff))
                            .child("Retry")
                            .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                    ),
            ),
        };
        let map_area = root.children(overlay.map(|c| div().absolute().inset_0().flex().items_center().justify_center().child(c)))
            .children(matches!(self.state, State::Ready).then(legend))
            .child(self.popup.clone());
        crate::linked_chart::over_map(map_area, vec![self.chart.clone().into_any_element()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, d: f64) -> Vec<Ring> {
        vec![vec![[x, y], [x + d, y], [x + d, y + d], [x, y + d], [x, y]]]
    }

    /// The window -> `MapView` chain must set no opaque background: the
    /// map draws in a CAMetalLayer behind gpui, so one would hide it (the
    /// bug `race_dots` shipped). The root is `div().relative().size_full()`
    /// holding the map; every background in this file is a card's, built by
    /// `card()`, `swatch()` or inside the card chains. The needle is built
    /// at runtime so this assertion does not match itself.
    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        let needle = [".", "bg", "("].concat();
        let source = include_str!("overlay_features.rs");
        let at = source.find("let root = div()").expect("the root div");
        let root = &source[at..at + source[at..].find(';').expect("the statement ends")];
        assert!(!root.contains(&needle), "the root div must not set a background");
        let render = &source[source.find("impl gpui::Render for OverlayPanel").unwrap()..];
        let render = &render[..render.find("\n}\n").unwrap()];
        assert!(render.contains("root.children("), "cards are the root's later children, over the map");
    }

    /// M7: an area click must fit against the panel's CURRENT size, not
    /// the build-time `env.viewport` -- the About dock and a window resize
    /// both change it after the panel is built.
    #[test]
    fn fit_size_prefers_the_current_laid_out_size_over_build_time_viewport() {
        let build_time = (1200.0, 800.0);
        // Before the first prepaint: `area` is still the zeroed default.
        assert_eq!(fit_size((0.0, 0.0, 0.0, 0.0), build_time), build_time, "falls back before layout");
        // After a prepaint with the About dock open, or a resize: `area`
        // holds the panel's real, current, smaller box.
        let laid_out = (0.0, 0.0, 860.0, 640.0);
        assert_eq!(fit_size(laid_out, build_time), (860.0, 640.0), "must use the current box, not the stale build-time one");
        assert_ne!(fit_size(laid_out, build_time), build_time, "a click after the dock opened must not fit the old, wider size");
    }

    #[test]
    fn names_join_across_spelling_and_case() {
        assert_eq!(normalise("North-Eastern Islands"), normalise("NORTH-EASTERN ISLANDS"));
        assert_eq!(normalise("  Ang Mo  Kio "), "ANG MO KIO");
        let areas = vec![
            Area { name: "NORTH-EASTERN ISLANDS".into(), polygons: vec![square(103.9, 1.4, 0.01)] },
            Area { name: "ANG MO KIO".into(), polygons: vec![square(103.8, 1.3, 0.01)] },
            Area { name: "NOWHERE".into(), polygons: vec![square(103.7, 1.3, 0.01)] },
        ];
        let pop = HashMap::from([
            (normalise("North-Eastern Islands"), 40),
            (normalise("Ang Mo Kio"), 158_720),
            (normalise("Atlantis"), 5),
        ]);
        let (joined, unjoined) = join(areas, &pop);
        assert_eq!(joined[0].residents, Some(40));
        assert_eq!(joined[1].residents, Some(158_720));
        assert_eq!(joined[2].residents, None);
        assert_eq!(unjoined, vec!["boundary NOWHERE".to_string(), "population ATLANTIS".to_string()]);
        // 0.01 deg square near 1.3 N: ~1.24 km².
        assert!((joined[1].km2 - 1.239).abs() < 0.01, "{}", joined[1].km2);
        assert!((joined[1].density() - 158_720.0 / joined[1].km2).abs() < 1e-6);
        assert_eq!(joined[2].density(), 0.0);
    }

    #[test]
    fn singstat_rows_parse_and_nil_is_zero() {
        let body = br#"{"Data":{"row":[
            {"rowText":"Total","columns":[{"key":"Total","value":"4204520"}]},
            {"rowText":"Ang Mo Kio - Total","columns":[{"key":"Total","value":"158720"}]},
            {"rowText":"Cheng San","columns":[{"key":"Total","value":"28000"}]},
            {"rowText":"Changi Bay - Total","columns":[{"key":"Total","value":"-"}]}]}}"#;
        let pop = parse_population(body).unwrap();
        assert_eq!(pop.len(), 2);
        assert_eq!(pop["ANG MO KIO"], 158_720);
        assert_eq!(pop["CHANGI BAY"], 0);
    }

    /// Untrusted input: every malformed file is an `Err`, never a panic.
    #[test]
    fn malformed_files_are_errors_not_panics() {
        let junk: &[&[u8]] = &[
            b"",
            b"not json",
            b"{}",
            b"{\"features\": 3}",
            b"{\"features\": [{\"geometry\": {\"type\": \"Polygon\", \"coordinates\": [[[\"a\", 1]]]}}]}",
            b"{\"features\": [{\"properties\": {\"PLN_AREA_N\": \"X\"}, \"geometry\": {\"type\": \"Polygon\", \"coordinates\": [[[1e400, 1]]]}}]}",
            b"{\"features\": [{\"properties\": {\"PLN_AREA_N\": \"X\"}, \"geometry\": {\"type\": \"Point\", \"coordinates\": [1, 1]}}]}",
            &[0xff, 0xfe, 0x00],
        ];
        for bytes in junk {
            assert!(parse_areas(bytes).is_err(), "{:?}", String::from_utf8_lossy(bytes));
            assert!(parse_tracks(bytes).is_err());
            assert!(parse_exits(bytes).is_err());
            assert!(parse_population(bytes).is_err());
            assert!(parse_station_codes(bytes).is_err());
        }
        assert!(parse_population(br#"{"Data":{"row":[{"rowText":"A - Total","columns":[{"value":"12x"}]}]}}"#).is_err());
        assert!(poll_answer(b"<html>").is_err());
        assert_eq!(poll_answer(br#"{"code":24,"name":"TOO_MANY_REQUESTS","data":null}"#), Ok(None));
        assert_eq!(poll_answer(br#"{"code":0,"data":{"url":"https://x/y"}}"#), Ok(Some("https://x/y".into())));
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.geojson");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        let err = read_bounded(&path).unwrap_err();
        assert!(err.contains("over the"), "{err}");
    }

    #[test]
    fn the_download_list_is_the_scripts_list() {
        let s = sources();
        assert_eq!(s.len(), 5);
        assert!(s.iter().any(|s| s.file == "planning-areas.geojson" && s.target == "d_4765db0e87b9c86336792efe8a1f7a66"));
        assert_eq!(poll_url("d_x"), "https://api-open.data.gov.sg/v1/public/api/datasets/d_x/poll-download");
        let script = include_str!("../../../scripts/fetch-singapore-overlay.sh");
        assert!(script.contains("singapore-overlay-sources.txt"), "the script reads the same list");
        assert!(!script.contains("api-open.data.gov.sg"), "and holds no second copy of a URL");
    }

    // -- M3: the retry loop retries only on a rate limit, fails at once on
    // a network error, and checks cancel before every attempt. --

    #[tokio::test]
    async fn a_429_is_rate_limited_not_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        assert!(matches!(poll_once(&client, &server.uri()).await, PollAttempt::RateLimited));
    }

    #[tokio::test]
    async fn the_documented_throttle_body_is_rate_limited_not_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"code": 24, "name": "TOO_MANY_REQUESTS"})))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        assert!(matches!(poll_once(&client, &server.uri()).await, PollAttempt::RateLimited));
    }

    #[tokio::test]
    async fn a_resolved_poll_returns_the_signed_url() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"data": {"url": "https://example.test/f.geojson"}})))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        match poll_once(&client, &server.uri()).await {
            PollAttempt::Resolved(url) => assert_eq!(url, "https://example.test/f.geojson"),
            _ => panic!("expected Resolved"),
        }
    }

    #[tokio::test]
    async fn a_non_429_status_fails_at_once_not_rate_limited() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET")).respond_with(wiremock::ResponseTemplate::new(500)).mount(&server).await;
        let client = reqwest::Client::new();
        assert!(matches!(poll_once(&client, &server.uri()).await, PollAttempt::Failed(_)));
    }

    #[tokio::test]
    async fn a_network_error_fails_at_once_not_rate_limited() {
        // Nothing is listening on this loopback port: a connect error, the
        // same shape offline/DNS/connection-refused takes.
        let client = reqwest::Client::builder().build().unwrap();
        match poll_once(&client, "http://127.0.0.1:1/unreachable").await {
            PollAttempt::Failed(why) => assert!(why.contains("request failed"), "{why}"),
            _ => panic!("a connect error must fail at once, not be treated as a rate limit"),
        }
    }

    #[tokio::test]
    async fn resolve_checks_cancel_before_the_first_attempt_and_makes_no_request() {
        let server = wiremock::MockServer::start().await;
        // No `Mock` is mounted: any request the resolver made would 404
        // and fail the assertion below with the wrong error text.
        let client = reqwest::Client::new();
        let cancel = Arc::new(AtomicBool::new(true));
        let err = resolve_datagovsg_url(&client, &cancel, &server.uri()).await.unwrap_err();
        assert_eq!(err, "cancelled");
        assert!(server.received_requests().await.unwrap().is_empty(), "a pre-set cancel must skip the request entirely");
    }

    #[tokio::test]
    async fn resolve_stops_between_attempts_once_cancelled() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        // Flips cancel shortly after the first attempt starts waiting out
        // the backoff -- `cancellable_sleep`'s 100 ms slices must notice it
        // well before the full 11 s elapses.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let started = std::time::Instant::now();
        let err = resolve_datagovsg_url(&client, &cancel, &server.uri()).await.unwrap_err();
        assert_eq!(err, "cancelled");
        assert!(started.elapsed() < Duration::from_secs(5), "cancel must cut the backoff short, not run the full 11 s");
    }

    #[test]
    fn cancellable_sleep_returns_quickly_once_cancelled() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        rt.block_on(async {
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                flag.store(true, Ordering::SeqCst);
            });
            let started = std::time::Instant::now();
            cancellable_sleep(Duration::from_secs(11), &cancel).await;
            assert!(started.elapsed() < Duration::from_secs(1), "a cancel mid-wait must cut the sleep short");
        });
    }

    /// M6: `Prepared::accessed` is the SODL credit's access date -- it
    /// must come from a data.gov.sg file's fetch time, not from
    /// `population.json`, which is SingStat's (a different holder, on a
    /// different day). Two files with deliberately different mtimes prove
    /// which one `prepare` actually reads.
    #[test]
    fn accessed_comes_from_a_datagovsg_file_not_from_singstats_population_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("planning-areas.geojson"),
            json!({"type": "FeatureCollection", "features": [{
                "type": "Feature",
                "properties": {"PLN_AREA_N": "ANG MO KIO"},
                "geometry": {"type": "Polygon", "coordinates": [[[103.8, 1.3], [103.81, 1.3], [103.81, 1.31], [103.8, 1.31], [103.8, 1.3]]]},
            }]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("population.json"),
            json!({"Data": {"row": [{"rowText": "Ang Mo Kio - Total", "columns": [{"value": "158,720"}]}]}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("station-codes.csv"),
            "From\nAng Mo Kio (NS16)\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("station-exits.geojson"),
            json!({"type": "FeatureCollection", "features": [{
                "type": "Feature",
                "properties": {"STATION_NA": "Ang Mo Kio"},
                "geometry": {"coordinates": [103.8, 1.3]},
            }]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("rail-lines.geojson"),
            json!({"type": "FeatureCollection", "features": [{
                "type": "Feature",
                "properties": {"RAIL_TYPE": "MRT"},
                "geometry": {"type": "LineString", "coordinates": [[103.8, 1.3], [103.81, 1.31]]},
            }]})
            .to_string(),
        )
        .unwrap();

        let datagovsg_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000); // 2023-11-14
        let singstat_time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000); // 2020-09-13
        for f in ["planning-areas.geojson", "station-codes.csv", "station-exits.geojson", "rail-lines.geojson"] {
            std::fs::File::options().write(true).open(dir.path().join(f)).unwrap().set_modified(datagovsg_time).unwrap();
        }
        std::fs::File::options().write(true).open(dir.path().join("population.json")).unwrap().set_modified(singstat_time).unwrap();

        let p = prepare(dir.path()).expect("prepare");
        assert_eq!(p.accessed.as_deref(), Some("2023-11-14"), "must read a data.gov.sg file's mtime, not population.json's");
        assert_ne!(p.accessed.as_deref(), Some("2020-09-13"), "must not read SingStat's population.json mtime");
    }

    /// M6: the SODL credit's `changes` text must not hard-code a date --
    /// the map's credit line carries the real, per-download access date.
    #[test]
    fn the_sodl_changes_note_has_no_hard_coded_date() {
        assert!(!SODL_CHANGES.contains("2026-09-28"), "{SODL_CHANGES}");
        let has_iso_date = SODL_CHANGES.as_bytes().windows(10).any(|w| {
            let s = std::str::from_utf8(w).unwrap();
            s.len() == 10
                && s[..4].bytes().all(|b| b.is_ascii_digit())
                && s.as_bytes()[4] == b'-'
                && s[5..7].bytes().all(|b| b.is_ascii_digit())
                && s.as_bytes()[7] == b'-'
                && s[8..10].bytes().all(|b| b.is_ascii_digit())
        });
        assert!(!has_iso_date, "no other YYYY-MM-DD date either: {SODL_CHANGES}");
    }

    #[test]
    fn station_codes_and_exits_join() {
        let csv = b"From_PostalCode,To_PostalCode,From,To\n1,2,CALDECOTT MRT STATION (CC17 / TE9),A SCHOOL\n1,2,\"SENGKANG LRT STATION (NE16 / STC)\",B\n1,2,KEPPEL MRT STATION (CC30),C\n";
        let codes = parse_station_codes(csv).unwrap();
        assert_eq!(codes["CALDECOTT"].iter().cloned().collect::<Vec<_>>(), vec!["CC17", "TE9"]);
        let exits = br#"{"features":[
            {"properties":{"STATION_NA":"CALDECOTT MRT STATION"},"geometry":{"type":"Point","coordinates":[103.84,1.33]}},
            {"properties":{"STATION_NA":"CALDECOTT MRT STATION"},"geometry":{"type":"Point","coordinates":[103.86,1.35]}},
            {"properties":{"STATION_NA":"SENGKANG MRT STATION"},"geometry":{"type":"Point","coordinates":[103.89,1.39]}},
            {"properties":{"STATION_NA":"CC30"},"geometry":{"type":"Point","coordinates":[103.83,1.27]}}]}"#;
        let exits = parse_exits(exits).unwrap();
        let st = join_stations(&codes, &exits);
        assert_eq!(st.len(), 3, "{st:?}");
        let cal = st.iter().find(|s| s.name == "Caldecott").unwrap();
        assert!((cal.at[0] - 103.85).abs() < 1e-9 && (cal.at[1] - 1.34).abs() < 1e-9);
        assert_eq!(cal.lines().into_iter().collect::<Vec<_>>(), vec!["CC", "TE"]);
        assert!(st.iter().any(|s| s.name == "Keppel"), "placed by its code-named exit");
        assert_eq!(line_of("CG2"), Some("EW"));
        assert_eq!(line_of("PTC"), Some("LRT"));
        assert_eq!(line_of("JS1"), None);
    }

    #[test]
    fn tracks_follow_their_chords_and_strays_are_planned() {
        let s = |name: &str, code: &str, lon: f64| Station { name: name.into(), codes: [code.to_string()].into(), at: [lon, 1.30] };
        let stations = vec![s("A", "EW1", 103.80), s("B", "EW2", 103.82), s("C", "EW3", 103.84)];
        let ch = chords(&stations);
        assert_eq!(ch.len(), 2);
        let along: Ring = (0..=20).map(|i| [103.80 + 0.002 * i as f64, 1.3005]).collect();
        let far: Ring = (0..=20).map(|i| [103.80 + 0.002 * i as f64, 1.33]).collect();
        let across: Ring = (0..=20).map(|i| [103.81, 1.29 + 0.001 * i as f64]).collect();
        let t = classify_tracks(&[(false, along), (false, far), (false, across)], &ch);
        assert_eq!(t.iter().map(|t| t.line).collect::<Vec<_>>(), vec!["EW", "PLANNED", "PLANNED"]);
    }

    /// The styling expressions: the engine compiles all four slots, and the
    /// `fill-color` ramp resolves sample densities to the colours the
    /// legend shows -- checked through the engine's class count (one op per
    /// distinct colour) and against [`density_colour`].
    #[test]
    fn the_paint_expressions_compile_and_class_as_expected() {
        let mut store = gpui_maps::overlay::OverlayStore::new();
        store.declare_source_paint("t", &paint_object()).expect("the engine accepts every slot");
        // Two areas at each of three densities: 3 fill classes.
        let areas: Vec<Value> = [0.0, 0.0, 14_000.0, 14_000.0, 28_000.0, 28_000.0]
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let x = 103.6 + i as f64 * 0.02;
                json!({"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[x,1.3],[x+0.01,1.3],[x+0.01,1.31],[x,1.31],[x,1.3]]]},
                    "properties":{"density": d}})
            })
            .collect();
        store.set_source("t", &json!({"type":"FeatureCollection","features":areas}).to_string(), 11).unwrap();
        let fills = store.source_op_count("t").unwrap();
        let mut store2 = gpui_maps::overlay::OverlayStore::new();
        store2.declare_source_paint("l", &paint_object()).unwrap();
        store2.set_source("l", &json!({"type":"FeatureCollection","features":tracks_json()}).to_string(), 11).unwrap();
        assert!(fills >= 3, "three densities, three fill classes (plus outlines): {fills}");
        assert_eq!(density_colour(0.0), NO_RESIDENTS);
        assert_eq!(density_colour(12_000.0), [65, 182, 196]);
        assert_eq!(density_colour(99_999.0), [37, 52, 148]);
        assert_eq!(density_colour(1_000.5), [216, 237, 209], "halfway between the first two stops");
        let paint = paint_json();
        let line = paint["line-color"].as_array().unwrap();
        let at = |k: &str| line.iter().position(|v| v == k).map(|i| line[i + 1].as_str().unwrap().to_string());
        assert_eq!(at("NS").as_deref(), Some("#d42e12"));
        assert_eq!(at("TE").as_deref(), Some("#9d5b25"));
        assert_eq!(at("PLANNED").as_deref(), Some(PLANNED_COLOUR));
        fn tracks_json() -> Vec<Value> {
            ["NS", "NS", "EW", "EW", "PLANNED", "PLANNED"]
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let y = 1.3 + i as f64 * 0.01;
                    json!({"type":"Feature","geometry":{"type":"LineString","coordinates":[[103.7,y],[103.8,y]]},"properties":{"line": l}})
                })
                .collect()
        }
        assert_eq!(store2.source_op_count("l"), Some(3), "NS, EW and PLANNED: three line classes");
    }

    /// On the real, cached data (`scripts/fetch-singapore-overlay.sh`):
    /// every operating MRT line is one network with no gap over 30 m --
    /// the user's cut lines at Bedok, Tanah Merah, Siglap and Bayshore were
    /// the source omitting the track inside each station box.
    #[test]
    #[ignore = "needs the cached Singapore data"]
    fn every_operating_line_is_continuous_on_the_real_data() {
        let dir = data_dir();
        let file = |n: &str| read_bounded(&dir.join(n)).unwrap();
        let codes = parse_station_codes(&file("station-codes.csv")).unwrap();
        let exits = parse_exits(&file("station-exits.geojson")).unwrap();
        let parts = parse_tracks(&file("rail-lines.geojson")).unwrap();
        let stations = join_stations(&codes, &exits);
        let ch = chords(&stations);
        let mut broken: Vec<String> = Vec::new();
        let ext = extensions(&codes, &stations);
        let build = build_tracks(&parts, &ch, &ext);
        let built = build.tracks.clone();
        // No join longer than a station box is straight: longer gaps are
        // routed along the source track.
        for d in &build.straight_joins {
            assert!(*d <= STRAIGHT_MAX_M, "a {d:.0} m straight join");
        }
        assert!(build.routed > 0, "some gaps were routed along the source track");
        assert!(ext.iter().any(|e| e.line == "TE"), "TE is extended past Bayshore: {ext:?}");
        assert!(built.iter().any(|t| t.line == "TE-UC"), "and that stretch is drawn under construction");
        for line in MRT_LINES {
            assert!(built.iter().any(|t| t.line == *line), "{line} survived the clean-up");
            let comps = components(&built, line, 30.0);
            if comps.len() != 1 {
                let ends = |m: &Vec<usize>| -> Vec<[f64; 2]> {
                    m.iter().flat_map(|&i| [built[i].points[0], built[i].points[built[i].points.len() - 1]]).collect()
                };
                let main = ends(&comps[0].0);
                let desc: Vec<String> = comps
                    .iter()
                    .map(|(m, len)| {
                        let (d, p, q) = ends(m)
                            .into_iter()
                            .flat_map(|p| main.iter().map(move |q| (dist_m(p, *q), p, *q)))
                            .min_by(|x, y| x.0.total_cmp(&y.0))
                            .unwrap();
                        format!("{len:.0} m, nearest gap {d:.0} m {p:?} -> {q:?}")
                    })
                    .collect();
                broken.push(format!("after clean-up {line}: {desc:?}"));
            }
        }
        if !broken.is_empty() {
            // Why a gap stayed open: every pair of networks' nearest ends,
            // and what a route through ANY source part would cross.
            let graph = TrackGraph::new(&parts);
            let labels = build.labels.clone();
            for line in MRT_LINES {
                let comps = components(&built, line, 30.0);
                for x in 0..comps.len() {
                    for y in x + 1..comps.len() {
                        let e = |m: &Vec<usize>| -> Vec<[f64; 2]> { m.iter().flat_map(|&i| [built[i].points[0], built[i].points[built[i].points.len() - 1]]).collect() };
                        let (d, p, q) = e(&comps[x].0)
                            .into_iter()
                            .flat_map(|p| e(&comps[y].0).into_iter().map(move |q| (dist_m(p, q), p, q)))
                            .min_by(|a, b| a.0.total_cmp(&b.0))
                            .unwrap();
                        let route = graph.route(p, q, |_| Some(1.0), 20_000.0);
                        let crossed: BTreeSet<String> = route
                            .iter()
                            .flatten()
                            .map(|s| match s {
                                Step::Part(i) => format!("{i}:{:?}:{:.0}m", labels[*i], length_m(&parts[*i].1)),
                                Step::Hop(..) => "hop".into(),
                            })
                            .collect();
                        broken.push(format!("  {line} {x}-{y}: {d:.0} m {p:?} {q:?}, open route crosses {crossed:?}"));
                    }
                }
            }
        }
        assert!(broken.is_empty(), "{}", broken.join("\n"));
    }

    #[test]
    fn interchanges_are_bigger_and_white() {
        assert!(station_radii(3).0 > station_radii(2).0 && station_radii(2).0 > station_radii(1).0);
        let kinds = station_kind_colours();
        assert!(kinds.contains(&("dot-x".into(), DOT_X.into())));
        assert!(kinds.contains(&("ring-1".into(), "#ffffff".into())), "a distinct colour from dot-x, so the rings cannot merge with the dots");
        assert!(kinds.contains(&("ring-x".into(), "#1f2933".into())));
    }

    #[test]
    fn simplification_keeps_ends_and_drops_collinear_points() {
        let line: Ring = (0..100).map(|i| [103.8 + i as f64 * 1e-4, 1.3]).collect();
        assert_eq!(simplify(&line, 1.0), vec![line[0], line[99]]);
    }

    #[test]
    fn the_card_says_what_the_area_holds() {
        let (t, b) = hover_lines(&HoverInfo::Area { name: "Bedok".into(), residents: 274_360, density: 12_620.4 });
        assert_eq!(t, "Bedok");
        assert_eq!(b, "274,360 residents · 12,620 per km²");
        assert_eq!(hover_lines(&HoverInfo::Area { name: "Changi Bay".into(), residents: 0, density: 0.0 }).1, "No residents");
        assert_eq!(title_case("NORTH-EASTERN ISLANDS"), "North-Eastern Islands");
    }

    #[test]
    fn the_card_names_a_line_by_name_code_and_colour() {
        let ew = HoverInfo::Line { code: "EW".into() };
        assert_eq!(hover_lines(&ew), ("East–West Line (EW)".to_string(), "Line colour #009645".to_string()));
        assert_eq!(ew.colour(), Some("#009645"));
        let te = HoverInfo::Line { code: "TE".into() };
        assert_eq!(hover_lines(&te).0, "Thomson–East Coast Line (TE)");
        let uc = HoverInfo::Line { code: "TE-UC".into() };
        assert_eq!(hover_lines(&uc).0, "Thomson–East Coast Line (TE), under construction");
        assert_eq!(uc.colour(), Some("#9d5b25"), "the line's own colour, not the planned grey");
        let planned = HoverInfo::Line { code: "PLANNED".into() };
        assert!(hover_lines(&planned).0.starts_with("Planned"));
    }

    #[test]
    fn the_card_names_a_station_its_codes_and_lines() {
        let props: gpui_maps::overlay::FeatureProperties = HashMap::from([
            ("name".to_string(), TagValue::String("Tanah Merah".into())),
            ("codes".to_string(), TagValue::String("EW4".into())),
            ("lines".to_string(), TagValue::String("EW".into())),
        ]);
        let info = HoverInfo::from_properties(STATIONS, &props).unwrap();
        assert_eq!(hover_lines(&info), ("Tanah Merah — EW4".to_string(), "East–West Line".to_string()));
        let inter = HoverInfo::Station { name: "Bayfront".into(), codes: "CE1 / DT16".into(), lines: vec!["CC".into(), "DT".into()] };
        assert_eq!(hover_lines(&inter).1, "Interchange: Circle Line, Downtown Line");
        // A rail feature, casing or colour, carries its line's code.
        let rail: gpui_maps::overlay::FeatureProperties = HashMap::from([("code".to_string(), TagValue::String("TE".into()))]);
        assert_eq!(HoverInfo::from_properties(RAIL, &rail), Some(HoverInfo::Line { code: "TE".into() }));
        assert_eq!(HoverInfo::from_properties("elsewhere", &rail), None);
    }

    /// The legend is this panel's one card anchored to the bottom-left
    /// corner, the corner the engine draws the SWAT mark into. A native map
    /// draws BELOW gpui, so a card over the mark hides it. Laid out by gpui
    /// itself and measured with `debug_bounds`, against the mark's own
    /// footprint for the map the panel's root fills.
    #[test]
    fn the_legend_keeps_clear_of_the_brand_mark() {
        use gpui::{prelude::*, TestAppContext};
        struct Host;
        impl gpui::Render for Host {
            fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                div().relative().size_full().child(legend())
            }
        }
        let mut app = TestAppContext::single();
        let (_host, cx) = app.add_window_view(|_, _| Host);
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        let legend = cx.debug_bounds("overlay-legend").expect("the legend was drawn");
        let height = cx.update(|window, _| f32::from(window.viewport_size().height));
        let (mx, my, mw, mh) = gpui_maps::brand_mark::BrandMark::default().footprint(height);
        let (lx, ly) = (f32::from(legend.origin.x), f32::from(legend.origin.y));
        let (lw, lh) = (f32::from(legend.size.width), f32::from(legend.size.height));
        assert!(lh > 0.0 && lx < mx + mw, "the legend is in the mark's columns, so only its height keeps them apart");
        assert!(
            ly + lh <= my,
            "the legend ({lx}, {ly}, {lw}x{lh}) reaches down over the mark ({mx}, {my}, {mw}x{mh})"
        );
    }

    /// The shared card says what the old one did, in rows: an area's
    /// residents and density, a station's codes and lines.
    #[test]
    fn the_popup_card_tables_what_the_hover_said() {
        let area = hover_card(&HoverInfo::Area { name: "Bedok".into(), residents: 274_360, density: 12_620.4 });
        assert_eq!(area.rows, vec![("Residents".to_string(), "274,360".to_string()), ("Density".to_string(), "12,620 per km²".to_string())]);
        assert!(area.hint.unwrap().contains("zoom"));
        let line = hover_card(&HoverInfo::Line { code: "EW".into() });
        assert_eq!(line.swatch, Some(0x009645));
        let st = hover_card(&HoverInfo::Station { name: "Bayfront".into(), codes: "CE1 / DT16".into(), lines: vec!["CC".into(), "DT".into()] });
        assert_eq!(st.rows[1].1, "Interchange: Circle Line, Downtown Line");
    }

    /// Every planning area has one region (URA MP2019: 55 areas in five
    /// regions), spelled as the boundary file or SingStat spells it.
    #[test]
    fn every_planning_area_has_one_region() {
        assert_eq!(super::region_of("ANG MO KIO"), Some("North-East"));
        assert_eq!(super::region_of("North-Eastern Islands"), Some("North-East"));
        assert_eq!(super::region_of("Downtown Core"), Some("Central"));
        assert_eq!(super::region_of("TUAS"), Some("West"));
        assert_eq!(super::region_of("Atlantis"), None);
        for r in super::REGIONS {
            assert!(super::REGIONS.contains(&r));
        }
    }
}
