//! The story's GPS tracks, sampled by time (spec §3.3, plan S2.4).
//!
//! A track is a list of (story seconds, lon, lat) samples, read from either
//! a GeoJSON `Feature<LineString>` whose `properties.times` gives one time
//! per vertex, or a CSV of `t,lon,lat[,bearing]` rows. [`parse_track`]
//! tells them apart by the first non-space byte (`{` is GeoJSON).
//!
//! **Times.** A number, or a string holding a number, is story seconds
//! (the same in GeoJSON and CSV). Any other string is an RFC 3339 UTC time
//! (`script::parse_utc`) and becomes `utc - clock_start_unix`. Times must be
//! strictly increasing: a duplicate or a step back is refused, never sorted
//! silently, because a GPS log out of order is a broken log.
//!
//! **Space.** Positions are interpolated linearly in Web Mercator world
//! space (`geo::lon_lat_to_tile_xy(.., 0.0) * TILE_SIZE`), not in degrees.
//! Consecutive vertices are unwrapped to the copy of the world nearest the
//! previous one, so a track across the antimeridian takes the short way.
//! Latitudes are clamped to `geo`'s Mercator limit (85.05 degrees).
//!
//! **Heading.** The direction of travel over a centred one-second window,
//! `position(t - 0.5)` to `position(t + 0.5)` clamped to the span, as a
//! bearing clockwise from north. Whether the track is MOVING at `t` is
//! decided over a wider window: it must cover at least 1 m/s over both
//! `[t - 2.5, t]` and `[t, t + 2.5]` (each clamped to the span), and the
//! one-second chord must too. Where it is stopped (a stop with 1 Hz GPS
//! jitter, whose fixes a second apart can be metres apart but never
//! wander 2.5 m in 2.5 s, or `t` outside the span), the heading is that
//! of the nearest earlier moving stretch (else the first later one), so a
//! follow camera does not spin when the bus stops -- including the
//! seconds of arrival, when a single centred window would still see the
//! approach. Every answer is a pure function of `t`.
//!
//! **Hostile input.** Memory is bounded by more than the byte cap: a CSV
//! line over 4 KiB or with a fifth column is refused before it is split,
//! and GeoJSON is deserialised into typed, capped arrays of just the
//! coordinates and times (no `serde_json::Value` tree), so a refused file
//! allocates at most the capped arrays (see
//! `tests/story_track_peak_memory.rs`). Nothing here panics on any bytes (the module denies
//! `unwrap`, `expect` and indexing outside tests). Every refusal is a
//! [`Diagnostic`] whose `path` is a JSON pointer INTO THE TRACK FILE:
//! `/properties/times/3` or `/geometry/coordinates/3/1` in GeoJSON, and
//! `/lines/<n>` or `/lines/<n>/<column>` (1-based line numbers) in a CSV.
//! A problem with the whole file (too large, empty, not UTF-8, not JSON)
//! is at the root, `""`. The caller that knows which `scene.tracks[i]` it
//! loaded prefixes `/scene/tracks/<i>/file`.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::sync::Arc;

use glam::DVec2;

use super::script::{parse_utc, Diagnostic, Severity, TrackStyle};
use crate::camera_control::wrap_lon;
use crate::geo::{lon_lat_to_tile_xy, tile_xy_to_lon_lat, TILE_SIZE};

/// The largest track file [`parse_track`] reads: 64 MiB. A day of 1 Hz GPS
/// as CSV is about 3 MB.
pub const MAX_TRACK_BYTES: usize = 64 * 1024 * 1024;
/// The most samples one track may hold: a million (11 days at 1 Hz).
pub const MAX_TRACK_POINTS: usize = 1_000_000;
/// Half the heading window, in seconds.
const HEADING_HALF_WINDOW_S: f64 = 0.5;
/// Each half of the window that decides "stopped", in seconds: moving at
/// `t` needs [`MIN_HEADING_SPEED_M_PER_S`] over both `[t - 2.5, t]` and
/// `[t, t + 2.5]`, so 2.5 m each way.
const STOP_HALF_WINDOW_S: f64 = 2.5;
/// The slowest motion that has a heading: 1 m/s (3.6 km/h), so 1 m over
/// the one-second chord and 2.5 m over each half of the stop window.
/// Slower than this, `t` (or a segment, for the fallback) counts as
/// stopped and the heading falls back to the last moving stretch, so GPS
/// jitter at a stop does not spin the camera. Jitter of metres between
/// fixes a second apart is caught by the 2.5 s halves (it stays within a
/// couple of metres of the stop); jitter wandering over 2.5 m in 2.5 s
/// still reads as motion.
///
/// **Known limit.** Urban 1 Hz phone GPS at a stop often wanders 3-5 m
/// (a 6-10 m spread), which is over that, so a `bearing: "track"` camera
/// still turns at such a stop (`realistic_gps_jitter_at_a_stop_is_the_known_limit`).
/// A displacement floor (say `max(1 m/s · Δt, 5 m)`) would hold it, at the
/// cost of no heading for the first seconds after a slow start; smoothing
/// the log before it reaches the story is the author-side fix.
const MIN_HEADING_SPEED_M_PER_S: f64 = 1.0;
/// Web Mercator's equator, in metres: one world width at latitude 0.
const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.686;
/// geo's Mercator latitude clamp (`lon_lat_to_tile_xy`).
const TRACK_MAX_LAT: f64 = 85.05;

/// One track: samples strictly increasing in time. Cloning is cheap: the
/// samples are shared through `Arc`, so a [`super::CameraPath`] keeps the
/// tracks without copying them.
#[derive(Debug, Clone)]
pub struct Track {
    pub id: String,
    /// Story seconds, strictly increasing, at least two.
    times: Arc<[f64]>,
    /// World-space positions (TILE_SIZE units at zoom 0), x unwrapped so
    /// consecutive vertices are never more than half a world apart.
    world: Arc<[DVec2]>,
    /// The vertices as given (lon wrapped, lat clamped), returned exactly
    /// at a vertex's own time.
    lon_lat: Arc<[(f64, f64)]>,
    /// Per segment `i` (vertex `i` to `i + 1`): the heading of the last
    /// moving segment at or before `i`, else the first moving one after.
    /// All `None` for a track that never moves.
    fallback_heading: Arc<[Option<f32>]>,
    pub style: TrackStyle,
    /// A camera-only track ([`draw_track`]'s `draw:<layer>`): followable,
    /// never drawn. [`TrackSet::feature_collection_at`] and
    /// [`TrackSet::any_live_at`] skip it -- the overlay layer it follows
    /// draws itself.
    pub(crate) camera_only: bool,
}

/// Every track a story names.
#[derive(Debug, Default)]
pub struct TrackSet {
    pub tracks: Vec<Track>,
}

impl TrackSet {
    pub fn get(&self, id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    /// The index of the track named `id`, for plans that keep an index.
    pub(crate) fn index_of(&self, id: &str) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    /// A shallow copy: the tracks' samples are shared, not copied.
    pub(crate) fn share(&self) -> TrackSet {
        TrackSet { tracks: self.tracks.clone() }
    }

    /// Points and trails at `t`, as one GeoJSON FeatureCollection string
    /// for `CaptureRequest::overlay`, painted by per-feature properties.
    ///
    /// For each track with a position at `t` (from its first time on), one
    /// `Point` with `circle-color` and `circle-radius`. When at least two
    /// vertices lie in `[t - trail_s, t]`, also a `LineString` trail
    /// through them to the current position, with `line-color` and
    /// `line-width: 3`. Trails come first so the points draw over them.
    /// A trail across the antimeridian keeps its longitudes continuous
    /// (some past ±180), shifted so the point's own longitude is wrapped.
    pub fn feature_collection_at(&self, t: f64) -> String {
        self.feature_collection_at_inner(t)
    }

    /// Whether [`Self::feature_collection_at`] has any feature at `t`: a
    /// track whose span holds `t` (every live track draws its point).
    pub fn any_live_at(&self, t: f64) -> bool {
        t.is_finite() && self.drawn().any(|track| track.world_at_in_span(t).is_some())
    }

    /// The tracks the story draws: every one but a camera-only track.
    pub(crate) fn drawn(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|track| !track.camera_only)
    }

    fn feature_collection_at_inner(&self, t: f64) -> String {
        let mut trails = Vec::new();
        let mut points = Vec::new();
        if t.is_finite() {
            for track in self.drawn() {
                let Some(head) = track.world_at_in_span(t) else { continue };
                let (head_lon, head_lat) = world_to_lon_lat(head);
                let shift = wrap_lon(head_lon) - head_lon;
                points.push(serde_json::json!({
                    "type": "Feature",
                    "properties": {"track": track.id, "circle-color": track.style.color, "circle-radius": track.style.radius_px},
                    "geometry": {"type": "Point", "coordinates": [head_lon + shift, head_lat]},
                }));
                if let Some(line) = track.trail(t, head, shift) {
                    trails.push(serde_json::json!({
                        "type": "Feature",
                        "properties": {"track": track.id, "line-color": track.style.color, "line-width": 3},
                        "geometry": {"type": "LineString", "coordinates": line},
                    }));
                }
            }
        }
        trails.extend(points);
        serde_json::json!({"type": "FeatureCollection", "features": trails}).to_string()
    }
}

impl Track {
    /// `(first time, last time)` in story seconds.
    pub fn span(&self) -> (f64, f64) {
        (self.times.first().copied().unwrap_or(0.0), self.times.last().copied().unwrap_or(0.0))
    }

    /// (lon, lat) at `t`: `None` before the first time (or for a
    /// non-finite `t`), the last vertex from the last time on, and
    /// linear in world space between.
    pub fn position_at(&self, t: f64) -> Option<(f64, f64)> {
        if !t.is_finite() || t < self.span().0 {
            return None;
        }
        Some(self.position_clamped(t))
    }

    /// [`position_at`](Self::position_at) with `t` clamped into the span:
    /// the first vertex before it, the last after. A non-finite `t` is the
    /// first vertex.
    pub fn position_clamped(&self, t: f64) -> (f64, f64) {
        match self.locate(t) {
            Locate::Vertex(i) => self.lon_lat.get(i).copied().unwrap_or((0.0, 0.0)),
            Locate::Between(i, f) => {
                let (lon, lat) = world_to_lon_lat(self.lerp(i, f));
                (wrap_lon(lon), lat)
            }
        }
    }

    /// The heading at `t`, in radians clockwise from north (MapLibre's
    /// bearing sense), over a centred one-second window clamped to the
    /// span. `None` for a non-finite `t` or a track that never moves.
    pub fn heading_at(&self, t: f64) -> Option<f32> {
        if !t.is_finite() {
            return None;
        }
        let (first, last) = self.span();
        let ta = (t - HEADING_HALF_WINDOW_S).clamp(first, last);
        let tb = (t + HEADING_HALF_WINDOW_S).clamp(first, last);
        if self.moving_at(t) {
            if let Some(h) = heading_between(self.world_clamped(ta), self.world_clamped(tb), tb - ta) {
                return Some(h);
            }
        }
        let segment = match self.locate(t) {
            Locate::Vertex(i) | Locate::Between(i, _) => i.min(self.fallback_heading.len().saturating_sub(1)),
        };
        self.fallback_heading.get(segment).copied().flatten()
    }

    /// Whether the track moves at `t` (clamped into the span): at least
    /// [`MIN_HEADING_SPEED_M_PER_S`] over each of `[t - 2.5, t]` and
    /// `[t, t + 2.5]`, clamped to the span. A half the span clamps to
    /// nothing (at an end) is no evidence either way and passes.
    fn moving_at(&self, t: f64) -> bool {
        let (first, last) = self.span();
        let t = if t.is_nan() { first } else { t.clamp(first, last) };
        let here = self.world_clamped(t);
        [t - STOP_HALF_WINDOW_S, t + STOP_HALF_WINDOW_S].into_iter().all(|edge| {
            let edge = edge.clamp(first, last);
            let seconds = (edge - t).abs();
            seconds <= 0.0 || metres_between(here, self.world_clamped(edge)) >= MIN_HEADING_SPEED_M_PER_S * seconds
        })
    }

    /// Where `t` falls: exactly on vertex `i` (including before the first
    /// and after the last), or between vertex `i` and `i + 1` at fraction
    /// `f` in `(0, 1)`.
    fn locate(&self, t: f64) -> Locate {
        let (first, last) = self.span();
        let last_index = self.times.len().saturating_sub(1);
        if t.is_nan() || t <= first {
            return Locate::Vertex(0);
        }
        if t >= last {
            return Locate::Vertex(last_index);
        }
        // The last vertex with time <= t; `t > first` makes it >= 0.
        let i = self.times.partition_point(|&x| x <= t).saturating_sub(1);
        let (Some(&t0), Some(&t1)) = (self.times.get(i), self.times.get(i + 1)) else {
            return Locate::Vertex(last_index);
        };
        if t == t0 {
            return Locate::Vertex(i);
        }
        Locate::Between(i, (t - t0) / (t1 - t0))
    }

    fn lerp(&self, i: usize, f: f64) -> DVec2 {
        match (self.world.get(i), self.world.get(i + 1)) {
            (Some(&a), Some(&b)) => a + (b - a) * f,
            (Some(&a), None) => a,
            _ => DVec2::ZERO,
        }
    }

    /// World position at `t`, clamped into the span.
    fn world_clamped(&self, t: f64) -> DVec2 {
        match self.locate(t) {
            Locate::Vertex(i) => self.world.get(i).copied().unwrap_or(DVec2::ZERO),
            Locate::Between(i, f) => self.lerp(i, f),
        }
    }

    /// World position at `t`, or `None` before the span (as `position_at`).
    fn world_at_in_span(&self, t: f64) -> Option<DVec2> {
        (t.is_finite() && t >= self.span().0).then(|| self.world_clamped(t))
    }

    /// The trail's coordinates at `t` ending at `head`, longitudes shifted
    /// by `shift`; `None` with fewer than two vertices in the window.
    fn trail(&self, t: f64, head: DVec2, shift: f64) -> Option<Vec<[f64; 2]>> {
        let trail_s = self.style.trail_s;
        if !(trail_s.is_finite() && trail_s > 0.0) {
            return None;
        }
        let lo = self.times.partition_point(|&x| x < t - trail_s);
        let hi = self.times.partition_point(|&x| x <= t);
        let vertices = self.world.get(lo..hi)?;
        if vertices.len() < 2 {
            return None;
        }
        let mut line: Vec<[f64; 2]> = vertices
            .iter()
            .map(|&w| {
                let (lon, lat) = world_to_lon_lat(w);
                [lon + shift, lat]
            })
            .collect();
        if vertices.last() != Some(&head) {
            let (lon, lat) = world_to_lon_lat(head);
            line.push([lon + shift, lat]);
        }
        Some(line)
    }
}

enum Locate {
    Vertex(usize),
    Between(usize, f64),
}

/// World (TILE_SIZE units at zoom 0, x possibly unwrapped) to (lon, lat),
/// the lon NOT wrapped.
fn world_to_lon_lat(w: DVec2) -> (f64, f64) {
    tile_xy_to_lon_lat(w.x / TILE_SIZE, w.y / TILE_SIZE, 0.0)
}

/// The bearing from `a` to `b` in world space (y grows south):
/// `atan2(dx, -dy)`, clockwise from north. `None` when the move over
/// `seconds` is slower than [`MIN_HEADING_SPEED_M_PER_S`] (or `seconds`
/// is not positive).
fn heading_between(a: DVec2, b: DVec2, seconds: f64) -> Option<f32> {
    let d = b - a;
    if !(d.is_finite() && seconds > 0.0) {
        return None;
    }
    let metres = metres_between(a, b);
    (metres >= MIN_HEADING_SPEED_M_PER_S * seconds && metres > 0.0).then(|| d.x.atan2(-d.y) as f32)
}

/// The ground distance from `a` to `b` in metres: a world unit is
/// C cos(lat) / TILE_SIZE metres at the chord's latitude. NaN for a
/// non-finite input, which compares false against any threshold.
fn metres_between(a: DVec2, b: DVec2) -> f64 {
    let (_, lat) = world_to_lon_lat((a + b) / 2.0);
    (b - a).length() * EARTH_CIRCUMFERENCE_M * lat.to_radians().cos() / TILE_SIZE
}

/// Where a sample came from, for its diagnostic's path.
#[derive(Clone, Copy)]
enum At {
    /// A CSV line, 1-based.
    Line(usize),
    /// A GeoJSON vertex index.
    Vertex(usize),
}

impl At {
    fn time_path(self) -> String {
        match self {
            At::Line(n) => format!("/lines/{n}/t"),
            At::Vertex(i) => format!("/properties/times/{i}"),
        }
    }
}

struct Sample {
    t: f64,
    lon: f64,
    lat: f64,
    at: At,
}

fn error(path: impl Into<String>, code: &'static str, message: impl Into<String>, hint: Option<&str>) -> Diagnostic {
    Diagnostic { severity: Severity::Error, path: path.into(), code, message: message.into(), hint: hint.map(str::to_string) }
}

/// A track file's diagnostic as seen from the script: its path (a pointer
/// INTO the file) is prefixed with `/scene/tracks/<i>/file`, so the root
/// `""` becomes `/scene/tracks/<i>/file` and `/lines/3/t` becomes
/// `/scene/tracks/<i>/file/lines/3/t`. For the loader that reads
/// `scene.tracks[i].file`.
pub fn for_scene_track(mut diagnostic: Diagnostic, i: usize) -> Diagnostic {
    diagnostic.path = format!("/scene/tracks/{i}/file{}", diagnostic.path);
    diagnostic
}

const FORMAT_HINT: &str = "a track is a GeoJSON Feature<LineString> with properties.times (one per vertex), or a CSV with the header t,lon,lat[,bearing]";

/// Read one track from `bytes`: GeoJSON when the first non-space byte is
/// `{`, else CSV. `id` and `style` are the script's; `clock_start_unix`
/// turns UTC time strings into story seconds. Never panics; see the module
/// docs for the diagnostic paths.
pub fn parse_track(bytes: &[u8], id: &str, style: TrackStyle, clock_start_unix: f64) -> Result<Track, Diagnostic> {
    if bytes.len() > MAX_TRACK_BYTES {
        return Err(error(
            "",
            "track_too_large",
            format!("track `{id}` is {} bytes, over the {MAX_TRACK_BYTES}-byte limit", bytes.len()),
            Some("thin the track (one sample a second is plenty for a camera)"),
        ));
    }
    let samples = match bytes.iter().find(|b| !b.is_ascii_whitespace()) {
        None => return Err(error("", "track_empty", format!("track `{id}` is empty"), Some(FORMAT_HINT))),
        Some(b'{') => parse_geojson(bytes, id, clock_start_unix)?,
        Some(_) => parse_csv(bytes, id, clock_start_unix)?,
    };
    build(samples, id, style)
}

/// The prefix of a draw head's synthetic track id: `draw:<layer id>`,
/// what `CameraPath::new` looks a `follow_draw` segment's track up by.
pub const DRAW_TRACK_PREFIX: &str = "draw:";

/// A draw head's track is sampled on this fixed grid (per second) over
/// the draw, whatever the output fps, so the follow camera is the same
/// at every fps.
pub const DRAW_TRACK_HZ: f64 = 30.0;

impl Track {
    /// A camera-only track from `(t, lon, lat)` samples: the same checks
    /// and world-space unwrapping as a loaded track (so it can cross the
    /// antimeridian), the default style, and never drawn
    /// ([`TrackSet::feature_collection_at`] skips it).
    pub(crate) fn from_samples(id: &str, samples: &[(f64, f64, f64)] /* (t, lon, lat) */) -> Result<Track, Diagnostic> {
        let mut checked = Vec::with_capacity(samples.len());
        for (i, &(t, lon, lat)) in samples.iter().enumerate() {
            let at = At::Vertex(i);
            check_sample(t, lon, lat, id, || at.time_path(), |name| format!("/coordinates/{i}/{name}"))?;
            checked.push(Sample { t, lon, lat, at });
        }
        let mut track = build(checked, id, TrackStyle::default())?;
        track.camera_only = true;
        Ok(track)
    }
}

/// The synthetic track of a draw head: sampled every 1/30 s over the draw's [at, until], plus
/// its final point at `until + 0.001`. Its id is `"draw:<layer id>"`.
///
/// `None` for a plan that is not a `draw`, has no finite span, or whose
/// source has no line to walk. The head position at each sample is
/// exactly [`super::effects::layer_fx_at`]'s: `point_at_fraction` of the
/// plan's eased `k`.
pub fn draw_track(plan: &super::effects::EffectPlan, source: &super::overlay_layers::LayerSource) -> Option<Track> {
    use super::overlay_layers::point_at_fraction;
    if plan.kind != super::script::EffectKind::Draw || !(plan.at.is_finite() && plan.until.is_finite() && plan.until > plan.at) {
        return None;
    }
    let span = plan.until - plan.at;
    let k = |t: f64| plan.easing.apply(((t - plan.at) / span).clamp(0.0, 1.0));
    let steps = (span * DRAW_TRACK_HZ + 1e-9).floor() as u64;
    let mut samples = Vec::with_capacity(steps as usize + 2);
    for n in 0..=steps {
        let t = plan.at + n as f64 / DRAW_TRACK_HZ;
        let (lon, lat) = point_at_fraction(source, k(t))?;
        samples.push((t, lon, lat));
    }
    let (lon, lat) = point_at_fraction(source, 1.0)?;
    samples.push((plan.until + 0.001, lon, lat));
    Track::from_samples(&format!("{DRAW_TRACK_PREFIX}{}", plan.layer_id), &samples).ok()
}

/// A time value: a number (story seconds) or a UTC string.
fn time_from_str(s: &str, clock_start_unix: f64) -> Option<f64> {
    s.parse::<f64>().ok().or_else(|| parse_utc(s).map(|utc| utc - clock_start_unix))
}

/// The longest CSV line read (4 KiB). A real `t,lon,lat,bearing` row is
/// under 100 bytes; a longer one is refused before it is split.
pub const MAX_CSV_LINE_BYTES: usize = 4096;

/// A CSV line split into at most four fields, without allocating. `None`
/// for a fifth field (more columns than `t,lon,lat,bearing`).
fn split_fields(line: &str) -> Option<([&str; 4], usize)> {
    let mut fields = [""; 4];
    let mut count = 0;
    for field in line.splitn(5, ',') {
        *fields.get_mut(count)? = field.trim();
        count += 1;
    }
    Some((fields, count))
}

fn parse_csv(bytes: &[u8], id: &str, clock_start_unix: f64) -> Result<Vec<Sample>, Diagnostic> {
    let text = std::str::from_utf8(bytes).map_err(|e| {
        error("", "track_not_utf8", format!("track `{id}` is not UTF-8 text (byte {})", e.valid_up_to()), Some(FORMAT_HINT))
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut columns: Option<usize> = None;
    let mut samples = Vec::new();
    for (index, raw) in text.split('\n').enumerate() {
        let n = index + 1;
        if raw.len() > MAX_CSV_LINE_BYTES {
            return Err(error(
                format!("/lines/{n}"),
                "track_bad_row",
                format!("track `{id}` line {n} is {} bytes, over the {MAX_CSV_LINE_BYTES}-byte line limit", raw.len()),
                Some(FORMAT_HINT),
            ));
        }
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some((fields, count)) = split_fields(line) else {
            return Err(error(format!("/lines/{n}"), "track_bad_row", format!("track `{id}` line {n} has more than 4 columns (t,lon,lat[,bearing])"), None));
        };
        let [first, second, third, fourth] = fields;
        let first_row = columns.is_none() && samples.is_empty();
        // A header is a first line whose first field is not a time and
        // which is not a data row with a bad time (its lon and lat parse).
        let looks_like_data = (3..=4).contains(&count) && second.parse::<f64>().is_ok() && third.parse::<f64>().is_ok();
        if first_row && !looks_like_data && time_from_str(first, clock_start_unix).is_none() {
            let named = |got: &str, want: &str| got.eq_ignore_ascii_case(want);
            let ok = (3..=4).contains(&count)
                && named(first, "t")
                && named(second, "lon")
                && named(third, "lat")
                && (count == 3 || named(fourth, "bearing"));
            if !ok {
                let shown: String = line.chars().take(80).collect();
                return Err(error(format!("/lines/{n}"), "track_bad_row", format!("track `{id}`: header `{shown}` is not t,lon,lat[,bearing]"), Some(FORMAT_HINT)));
            }
            columns = Some(count);
            continue;
        }
        let expected = columns.unwrap_or(if count == 4 { 4 } else { 3 });
        if count != expected {
            return Err(error(
                format!("/lines/{n}"),
                "track_bad_row",
                format!("track `{id}` line {n} has {count} columns, expected {expected} (t,lon,lat[,bearing])"),
                None,
            ));
        }
        if samples.len() >= MAX_TRACK_POINTS {
            return Err(error(format!("/lines/{n}"), "track_too_large", format!("track `{id}` has more than {MAX_TRACK_POINTS} samples"), None));
        }
        let bad = |name: &str, s: &str| {
            let shown: String = s.chars().take(40).collect();
            error(format!("/lines/{n}/{name}"), "track_bad_row", format!("track `{id}` line {n}: `{shown}` is not a valid {name}"), None)
        };
        let t = time_from_str(first, clock_start_unix).ok_or_else(|| bad("t", first))?;
        let lon = second.parse::<f64>().map_err(|_| bad("lon", second))?;
        let lat = third.parse::<f64>().map_err(|_| bad("lat", third))?;
        // The optional bearing column is accepted and ignored: the heading
        // comes from the motion, so a camera cannot disagree with the dot.
        check_sample(t, lon, lat, id, || format!("/lines/{n}/t"), |c| format!("/lines/{n}/{c}"))?;
        samples.push(Sample { t, lon, lat, at: At::Line(n) });
    }
    Ok(samples)
}

// ---- GeoJSON, deserialised into just what a track needs ----
//
// Not a `serde_json::Value` tree: that costs ~100 bytes a vertex, so a
// 64 MiB file could allocate gigabytes before any check ran. These types
// keep 24 bytes a vertex and 16 a time, stop storing past
// MAX_TRACK_POINTS (the rest is skipped with `IgnoredAny`), ignore every
// other member, and never hold a time string: it is read to a number
// while it is borrowed.

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

#[derive(serde::Deserialize)]
struct GeoFeature {
    #[serde(rename = "type")]
    kind: Option<Str>,
    geometry: Option<Obj<GeoGeometry>>,
    properties: Option<Obj<GeoProperties>>,
}

#[derive(serde::Deserialize)]
struct GeoGeometry {
    #[serde(rename = "type")]
    kind: Option<Str>,
    coordinates: Option<Capped<Coord>>,
}

#[derive(serde::Deserialize)]
struct GeoProperties {
    times: Option<Capped<Time>>,
}

/// A string member, or `None` for any other JSON value (skipped).
struct Str(Option<String>);

/// A member that must be a JSON object: `Some(T)` for an object, `None`
/// for any other value (consumed), so a `"properties": 5` or `[]` is
/// reported at its own path rather than as a pathless parse failure.
struct Obj<T>(Option<T>);

/// An array of at most MAX_TRACK_POINTS elements; `None` for any other
/// JSON value. `overflow` is set (and the rest skipped) past the cap.
struct Capped<T>(Option<(Vec<T>, bool)>);

/// A vertex: `Some((lon, lat))` for an array starting with two numbers
/// (any more are ignored, e.g. elevation), else `None`.
struct Coord(Option<(f64, f64)>);

/// A time as read: a number, or a string that is a number (story
/// seconds, as in CSV) or a UTC time (unix seconds, the clock start is
/// subtracted later). `Bad` for anything else.
#[derive(Clone, Copy)]
enum Time {
    Seconds(f64),
    Unix(f64),
    Bad,
}

/// Drains any JSON value; returns `None` from every `visit_*`.
fn skip_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}
fn skip_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

/// Implements the non-matching `visit_*` of a visitor whose value is
/// `$none`: every other JSON value is consumed and gives `$none`.
macro_rules! otherwise {
    ($none:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> { Ok($none) }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok($none) }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> { Ok($none) }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> { skip_map(map).map(|()| $none) }
    };
}

impl<'de, T: de::Deserialize<'de>> de::Deserialize<'de> for Obj<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(std::marker::PhantomData<T>);
        impl<'de, T: de::Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Obj<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("any JSON value") }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Obj<T>, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(|t| Obj(Some(t)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Obj<T>, A::Error> { skip_seq(seq).map(|()| Obj(None)) }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Obj<T>, E> { Ok(Obj(None)) }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Obj<T>, E> { Ok(Obj(None)) }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Obj<T>, E> { Ok(Obj(None)) }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Obj<T>, E> { Ok(Obj(None)) }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Obj<T>, E> { Ok(Obj(None)) }
            fn visit_unit<E: de::Error>(self) -> Result<Obj<T>, E> { Ok(Obj(None)) }
        }
        d.deserialize_any(V(std::marker::PhantomData))
    }
}

impl<'de> de::Deserialize<'de> for Str {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Str;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("any JSON value") }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Str, E> { Ok(Str(Some(s.to_string()))) }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Str, E> { Ok(Str(None)) }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Str, E> { Ok(Str(None)) }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Str, E> { Ok(Str(None)) }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Str, A::Error> { skip_seq(seq).map(|()| Str(None)) }
            otherwise!(Str(None));
        }
        d.deserialize_any(V)
    }
}

impl<'de, T: de::Deserialize<'de>> de::Deserialize<'de> for Capped<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(std::marker::PhantomData<T>);
        impl<'de, T: de::Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Capped<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("any JSON value") }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Capped<T>, A::Error> {
                let mut items = Vec::new();
                while items.len() < MAX_TRACK_POINTS {
                    match seq.next_element::<T>()? {
                        Some(item) => items.push(item),
                        None => return Ok(Capped(Some((items, false)))),
                    }
                }
                let overflow = seq.next_element::<IgnoredAny>()?.is_some();
                skip_seq(seq)?;
                Ok(Capped(Some((items, overflow))))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Capped<T>, E> { Ok(Capped(None)) }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Capped<T>, E> { Ok(Capped(None)) }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Capped<T>, E> { Ok(Capped(None)) }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Capped<T>, E> { Ok(Capped(None)) }
            otherwise!(Capped(None));
        }
        d.deserialize_any(V(std::marker::PhantomData))
    }
}

impl<'de> de::Deserialize<'de> for Coord {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// One array element: a number, or `None` for anything else.
        struct Num(Option<f64>);
        impl<'de> de::Deserialize<'de> for Num {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = Num;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("any JSON value") }
                    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Num, E> { Ok(Num(Some(v))) }
                    #[allow(clippy::cast_precision_loss)]
                    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Num, E> { Ok(Num(Some(v as f64))) }
                    #[allow(clippy::cast_precision_loss)]
                    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Num, E> { Ok(Num(Some(v as f64))) }
                    fn visit_str<E: de::Error>(self, _: &str) -> Result<Num, E> { Ok(Num(None)) }
                    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Num, A::Error> { skip_seq(seq).map(|()| Num(None)) }
                    otherwise!(Num(None));
                }
                d.deserialize_any(V)
            }
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Coord;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("any JSON value") }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Coord, A::Error> {
                let lon = seq.next_element::<Num>()?.and_then(|n| n.0);
                let lat = seq.next_element::<Num>()?.and_then(|n| n.0);
                skip_seq(seq)?;
                Ok(Coord(lon.zip(lat)))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Coord, E> { Ok(Coord(None)) }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Coord, E> { Ok(Coord(None)) }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Coord, E> { Ok(Coord(None)) }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Coord, E> { Ok(Coord(None)) }
            otherwise!(Coord(None));
        }
        d.deserialize_any(V)
    }
}

impl<'de> de::Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Time;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("any JSON value") }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Time, E> { Ok(Time::Seconds(v)) }
            #[allow(clippy::cast_precision_loss)]
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Time, E> { Ok(Time::Seconds(v as f64)) }
            #[allow(clippy::cast_precision_loss)]
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Time, E> { Ok(Time::Seconds(v as f64)) }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Time, E> {
                let s = s.trim();
                Ok(match s.parse::<f64>() {
                    Ok(v) => Time::Seconds(v),
                    Err(_) => parse_utc(s).map_or(Time::Bad, Time::Unix),
                })
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Time, A::Error> { skip_seq(seq).map(|()| Time::Bad) }
            otherwise!(Time::Bad);
        }
        d.deserialize_any(V)
    }
}

fn parse_geojson(bytes: &[u8], id: &str, clock_start_unix: f64) -> Result<Vec<Sample>, Diagnostic> {
    let doc: GeoFeature = serde_json::from_slice(bytes)
        .map_err(|e| error("", "track_bad_geojson", format!("track `{id}` is not a valid JSON object: {e}"), Some(FORMAT_HINT)))?;
    if doc.kind.and_then(|k| k.0).as_deref() != Some("Feature") {
        return Err(error("/type", "track_bad_geojson", format!("track `{id}` must be a GeoJSON Feature"), Some(FORMAT_HINT)));
    }
    if matches!(doc.geometry, Some(Obj(None))) {
        return Err(error("/geometry", "track_bad_geojson", format!("track `{id}`'s geometry must be an object"), Some(FORMAT_HINT)));
    }
    if matches!(doc.properties, Some(Obj(None))) {
        return Err(error(
            "/properties",
            "track_bad_geojson",
            format!("track `{id}`'s properties must be an object"),
            Some("give properties as an object holding `times`, one per vertex"),
        ));
    }
    let geometry = doc.geometry.and_then(|g| g.0).filter(|g| g.kind.as_ref().and_then(|k| k.0.as_deref()) == Some("LineString"));
    let Some(geometry) = geometry else {
        return Err(error("/geometry/type", "track_bad_geojson", format!("track `{id}`'s geometry must be a LineString"), Some(FORMAT_HINT)));
    };
    let (coords, coords_overflow) = geometry
        .coordinates
        .and_then(|c| c.0)
        .ok_or_else(|| error("/geometry/coordinates", "track_bad_geojson", format!("track `{id}` has no coordinates array"), None))?;
    let (times, times_overflow) = doc.properties.and_then(|p| p.0).and_then(|p| p.times).and_then(|t| t.0).ok_or_else(|| {
        error("/properties/times", "track_times_missing", format!("track `{id}` has no properties.times array"), Some("give one time per vertex: story seconds, or UTC strings"))
    })?;
    if coords_overflow {
        return Err(error("/geometry/coordinates", "track_too_large", format!("track `{id}` has more than {MAX_TRACK_POINTS} vertices"), None));
    }
    if times_overflow || times.len() != coords.len() {
        let count = if times_overflow { format!("more than {MAX_TRACK_POINTS}") } else { times.len().to_string() };
        return Err(error(
            "/properties/times",
            "track_times_mismatch",
            format!("track `{id}` has {count} times for {} vertices", coords.len()),
            Some("give exactly one time per vertex"),
        ));
    }
    let mut samples = Vec::with_capacity(coords.len());
    for (i, (coord, time)) in coords.iter().zip(&times).enumerate() {
        let t = match *time {
            Time::Seconds(v) => v,
            Time::Unix(utc) => utc - clock_start_unix,
            Time::Bad => {
                return Err(error(format!("/properties/times/{i}"), "track_bad_time", format!("track `{id}` time {i} is not story seconds or a UTC time"), None));
            }
        };
        let (lon, lat) = coord.0.ok_or_else(|| {
            error(format!("/geometry/coordinates/{i}"), "track_bad_row", format!("track `{id}` vertex {i} is not [lon, lat]"), None)
        })?;
        check_sample(t, lon, lat, id, || format!("/properties/times/{i}"), |c| {
            format!("/geometry/coordinates/{i}/{}", if c == "lon" { 0 } else { 1 })
        })?;
        samples.push(Sample { t, lon, lat, at: At::Vertex(i) });
    }
    Ok(samples)
}

/// Finite time and position, and a latitude on the planet.
fn check_sample(
    t: f64,
    lon: f64,
    lat: f64,
    id: &str,
    time_path: impl Fn() -> String,
    coord_path: impl Fn(&str) -> String,
) -> Result<(), Diagnostic> {
    if !t.is_finite() {
        return Err(error(time_path(), "track_not_finite", format!("track `{id}` has a time of {t}"), None));
    }
    for (name, v) in [("lon", lon), ("lat", lat)] {
        if !v.is_finite() {
            return Err(error(coord_path(name), "track_not_finite", format!("track `{id}` has a {name} of {v}"), None));
        }
    }
    if lat.abs() > 90.0 {
        return Err(error(coord_path("lat"), "track_bad_coordinate", format!("track `{id}` has latitude {lat}, outside -90..90"), Some("coordinates are lon, lat in that order")));
    }
    Ok(())
}

fn build(samples: Vec<Sample>, id: &str, style: TrackStyle) -> Result<Track, Diagnostic> {
    match samples.len() {
        0 => return Err(error("", "track_empty", format!("track `{id}` has no samples"), Some(FORMAT_HINT))),
        1 => {
            let path = samples.first().map(|s| s.at.time_path()).unwrap_or_default();
            return Err(error(path, "track_too_short", format!("track `{id}` has one sample; a track needs two"), None));
        }
        _ => {}
    }
    for pair in samples.windows(2) {
        if let [a, b] = pair {
            if b.t <= a.t {
                let what = if b.t == a.t { "repeats" } else { "goes back from" };
                return Err(error(
                    b.at.time_path(),
                    "track_times_not_increasing",
                    format!("track `{id}`: time {} {what} {}", b.t, a.t),
                    Some("times must be strictly increasing; sort the log and drop duplicate fixes"),
                ));
            }
        }
    }
    let mut times = Vec::with_capacity(samples.len());
    let mut world: Vec<DVec2> = Vec::with_capacity(samples.len());
    let mut lon_lat = Vec::with_capacity(samples.len());
    for s in &samples {
        let lon = wrap_lon(s.lon);
        let lat = s.lat.clamp(-TRACK_MAX_LAT, TRACK_MAX_LAT);
        let (x, y) = lon_lat_to_tile_xy(lon, lat, 0.0);
        let mut w = DVec2::new(x * TILE_SIZE, y * TILE_SIZE);
        if let Some(prev) = world.last() {
            w.x += ((prev.x - w.x) / TILE_SIZE).round() * TILE_SIZE;
        }
        times.push(s.t);
        world.push(w);
        lon_lat.push((lon, lat));
    }
    let mut track = Track {
        id: id.to_string(),
        times: times.into(),
        world: world.into(),
        lon_lat: lon_lat.into(),
        fallback_heading: Arc::from(Vec::new()),
        style,
        camera_only: false,
    };
    track.fallback_heading = fallback_headings(&track).into();
    Ok(track)
}

/// Per segment: the last moving segment's heading at or before it, else
/// the first moving one after it. A segment moves when it covers 1 m/s
/// itself AND the track is moving at its midpoint time (the same 2.5 s
/// halves as [`Track::heading_at`]), so a jittery second inside a stop is
/// never a fallback heading.
fn fallback_headings(track: &Track) -> Vec<Option<f32>> {
    let own: Vec<Option<f32>> = track
        .world
        .windows(2)
        .zip(track.times.windows(2))
        .map(|(w, t)| match (w, t) {
            ([a, b], [t0, t1]) => heading_between(*a, *b, t1 - t0).filter(|_| track.moving_at(t0 + (t1 - t0) / 2.0)),
            _ => None,
        })
        .collect();
    let first_moving = own.iter().find_map(|h| *h);
    let mut last = None;
    own.iter()
        .map(|h| {
            if h.is_some() {
                last = *h;
            }
            last.or(first_moving)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::Value;
    use crate::story::camera_path::CameraPath;
    use crate::story::test_util::test_script;

    fn csv_track(csv: &[u8]) -> Track {
        parse_track(csv, "bus", TrackStyle::default(), 0.0).unwrap()
    }

    fn err(bytes: &[u8]) -> Diagnostic {
        parse_track(bytes, "x", TrackStyle::default(), 0.0).unwrap_err()
    }

    #[test]
    fn a_straight_track_at_constant_speed_has_a_constant_heading_and_the_centre_on_the_track() {
        let csv = b"t,lon,lat\n0,103.80,1.30\n10,103.81,1.30\n20,103.82,1.30\n";
        let track = parse_track(csv, "bus", TrackStyle::default(), 0.0).unwrap();
        let set = TrackSet { tracks: vec![track] };
        let camera = serde_json::json!([{"type":"jump","to":{"lon":103.8,"lat":1.3,"zoom":15}},{"type":"follow","track":"bus","duration":20,"bearing":"track"}]);
        let p = CameraPath::new((800.0, 600.0), &test_script(camera), &set).unwrap();
        let h0 = p.pose_at(2.0).camera.bearing;
        for t in [3.0, 7.5, 12.0, 17.0] {
            let c = p.pose_at(t).camera;
            assert!((c.bearing - h0).abs() < 1e-6, "heading drifted at {t}");
            assert!((c.center_lat - 1.30).abs() < 1e-9, "off the track at {t}: {}", c.center_lat);
            let want_lon = 103.80 + 0.001 * t;
            // Spec §8: the centre on the track to 1e-9.
            assert!((c.center_lon - want_lon).abs() < 1e-9, "{t}: {} vs {want_lon}", c.center_lon);
        }
        // Due east: MapLibre bearing is clockwise from north, so the camera turns to +90 degrees
        // for the track to run up the screen.
        assert!((f64::from(h0).to_degrees() - 90.0).abs() < 1e-3);
    }

    #[test]
    fn before_the_track_it_is_hidden_and_after_it_holds() {
        let t = parse_track(b"t,lon,lat\n5,1,1\n6,2,1\n", "x", TrackStyle::default(), 0.0).unwrap();
        assert_eq!(t.position_at(4.9), None);
        assert_eq!(t.position_at(99.0), Some((2.0, 1.0)));
    }

    #[test]
    fn geojson_times_as_utc_strings_become_story_seconds() {
        let g = br#"{"type":"Feature","properties":{"times":["2026-03-20T10:00:00Z","2026-03-20T10:00:10Z"]},"geometry":{"type":"LineString","coordinates":[[0,0],[1,0]]}}"#;
        let start = crate::story::script::parse_utc("2026-03-20T10:00:00Z").unwrap();
        let t = parse_track(g, "g", TrackStyle::default(), start).unwrap();
        assert_eq!(t.span(), (0.0, 10.0));
    }

    #[test]
    fn bad_tracks_are_diagnostics() {
        for (bytes, code) in [
            (&b"t,lon,lat\n1,0,0\n1,1,1\n"[..], "track_times_not_increasing"),
            (&b"t,lon,lat\n0,0,0\n"[..], "track_too_short"),
            (&b"t,lon\n0,0\n1,1\n"[..], "track_bad_row"),
            (&br#"{"type":"Feature","properties":{"times":[0]},"geometry":{"type":"LineString","coordinates":[[0,0],[1,0]]}}"#[..], "track_times_mismatch"),
        ] {
            assert_eq!(parse_track(bytes, "x", TrackStyle::default(), 0.0).unwrap_err().code, code);
        }
    }

    // ---- hostile input (progress.md ruling): a diagnostic with a path each ----

    #[test]
    fn hostile_tracks_are_diagnostics_with_paths() {
        let cases: [(&[u8], &str, &str); 14] = [
            (b"t,lon,lat\n0,0,0\n2,1,0\n1,2,0\n", "track_times_not_increasing", "/lines/4/t"),
            (b"t,lon,lat\n0,0,0\n1,1,0\n1,2,0\n", "track_times_not_increasing", "/lines/4/t"),
            (b"t,lon,lat\nNaN,0,0\n1,1,0\n", "track_not_finite", "/lines/2/t"),
            (b"t,lon,lat\n0,NaN,0\n1,1,0\n", "track_not_finite", "/lines/2/lon"),
            (b"t,lon,lat\n0,0,inf\n1,1,0\n", "track_not_finite", "/lines/2/lat"),
            (b"", "track_empty", ""),
            (b"  \n\t ", "track_empty", ""),
            (b"t,lon,lat\n", "track_empty", ""),
            (b"t,lon,lat\n\n7,1,1\n", "track_too_short", "/lines/3/t"),
            (b"t,lon,lat\n0,0,95\n1,1,0\n", "track_bad_coordinate", "/lines/2/lat"),
            (b"t,lon,lat\n0,zero,0\n1,1,0\n", "track_bad_row", "/lines/2/lon"),
            (b"\xff\xfe,1,2\n", "track_not_utf8", ""),
            (b"{\"type\":\"Feature\"", "track_bad_geojson", ""),
            (br#"{"type":"Feature","properties":{"times":[0,"soon"]},"geometry":{"type":"LineString","coordinates":[[0,0],[1,0]]}}"#, "track_bad_time", "/properties/times/1"),
        ];
        for (bytes, code, path) in cases {
            let e = err(bytes);
            assert_eq!((e.code, e.path.as_str()), (code, path), "{:?}", String::from_utf8_lossy(bytes));
            assert!(!e.message.is_empty());
        }
        let geojson_cases: [(&str, &str, &str); 6] = [
            (r#"{"type":"FeatureCollection","features":[]}"#, "track_bad_geojson", "/type"),
            (r#"{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0]}}"#, "track_bad_geojson", "/geometry/type"),
            (r#"{"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[0,0],[1,0]]}}"#, "track_times_missing", "/properties/times"),
            (r#"{"type":"Feature","properties":{"times":[0,1]},"geometry":{"type":"LineString","coordinates":[[0,0],[1]]}}"#, "track_bad_row", "/geometry/coordinates/1"),
            (r#"{"type":"Feature","properties":{"times":[3,2]},"geometry":{"type":"LineString","coordinates":[[0,0],[1,0]]}}"#, "track_times_not_increasing", "/properties/times/1"),
            (r#"{"type":"Feature","properties":{"times":[]},"geometry":{"type":"LineString","coordinates":[]}}"#, "track_empty", ""),
        ];
        for (json, code, path) in geojson_cases {
            let e = err(json.as_bytes());
            assert_eq!((e.code, e.path.as_str()), (code, path), "{json}");
        }
    }

    #[test]
    fn a_track_diagnostic_is_prefixed_with_its_scene_track() {
        assert_eq!(for_scene_track(err(b"t,lon,lat\nNaN,0,0\n1,1,0\n"), 2).path, "/scene/tracks/2/file/lines/2/t");
        assert_eq!(for_scene_track(err(b""), 0).path, "/scene/tracks/0/file");
    }

    /// Re-review N1: a `properties` (or `geometry`) that is not an object
    /// is reported at its own member, not as a pathless parse failure --
    /// and the array form of either is no longer taken positionally.
    #[test]
    fn a_non_object_properties_or_geometry_has_its_own_path() {
        let line = r#""geometry":{"type":"LineString","coordinates":[[0,0],[1,0]]}"#;
        for props in ["5", "[]", "\"times\"", "true", "[[0,1]]"] {
            let json = format!(r#"{{"type":"Feature","properties":{props},{line}}}"#);
            let e = err(json.as_bytes());
            assert_eq!((e.code, e.path.as_str()), ("track_bad_geojson", "/properties"), "{json}");
        }
        for geometry in ["5", r#"["LineString",[[0,0],[1,0]]]"#] {
            let json = format!(r#"{{"type":"Feature","properties":{{"times":[0,1]}},"geometry":{geometry}}}"#);
            let e = err(json.as_bytes());
            assert_eq!((e.code, e.path.as_str()), ("track_bad_geojson", "/geometry"), "{json}");
        }
        // null is still "missing", as before.
        let json = format!(r#"{{"type":"Feature","properties":null,{line}}}"#);
        assert_eq!(err(json.as_bytes()).path, "/properties/times");
    }

    #[test]
    fn a_huge_file_is_refused_before_it_is_read() {
        let huge = vec![b'0'; MAX_TRACK_BYTES + 1];
        let e = err(&huge);
        assert_eq!((e.code, e.path.as_str()), ("track_too_large", ""));
    }

    #[test]
    fn too_many_samples_are_refused_at_the_row_that_overflows() {
        let mut csv = String::from("t,lon,lat\n");
        for i in 0..=MAX_TRACK_POINTS {
            csv.push_str(&format!("{i},0,0\n"));
        }
        assert!(csv.len() <= MAX_TRACK_BYTES);
        let e = err(csv.as_bytes());
        assert_eq!((e.code, e.path.as_str()), ("track_too_large", format!("/lines/{}", MAX_TRACK_POINTS + 2).as_str()));
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // A tiny deterministic generator: garbage, near-CSV and near-JSON.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet = b"0123456789.,-eE\n\r {}[]\":tlonaNifTZ";
        for round in 0..2000 {
            let len = (next() % 64) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| if round % 3 == 0 { next() as u8 } else { alphabet[(next() % alphabet.len() as u64) as usize] })
                .collect();
            if let Err(e) = parse_track(&bytes, "f", TrackStyle::default(), 0.0) {
                assert!(!e.code.is_empty());
            }
        }
        // Deep nesting stays inside serde_json's recursion limit.
        let deep = format!("{}{}", "{\"a\":[".repeat(10_000), "]}".repeat(10_000));
        assert_eq!(err(deep.as_bytes()).code, "track_bad_geojson");
    }

    // ---- sampling ----

    #[test]
    fn a_vertex_time_returns_the_vertex_exactly_and_the_middle_is_linear_in_world_space() {
        let t = csv_track(b"t,lon,lat\n0,10,0\n10,20,40\n");
        assert_eq!(t.position_at(0.0), Some((10.0, 0.0)));
        assert_eq!(t.position_at(10.0), Some((20.0, 40.0)));
        let (lon, lat) = t.position_at(5.0).unwrap();
        assert!((lon - 15.0).abs() < 1e-9);
        // Halfway in Mercator y, not halfway in degrees.
        let (_, y0) = lon_lat_to_tile_xy(10.0, 0.0, 0.0);
        let (_, y1) = lon_lat_to_tile_xy(20.0, 40.0, 0.0);
        let (_, mid) = tile_xy_to_lon_lat(0.0, (y0 + y1) / 2.0, 0.0);
        assert!((lat - mid).abs() < 1e-9 && (lat - 20.0).abs() > 0.5, "{lat} vs {mid}");
    }

    #[test]
    fn a_track_across_the_antimeridian_goes_the_short_way() {
        let t = csv_track(b"t,lon,lat\n0,179.5,0\n10,-179.5,0\n20,-178.5,0\n");
        let (lon, _) = t.position_at(5.0).unwrap();
        assert!((lon.abs() - 180.0).abs() < 1e-9, "{lon}");
        let (lon, _) = t.position_at(2.5).unwrap();
        assert!((lon - 179.75).abs() < 1e-9, "{lon}");
        // Heading due east.
        assert!((f64::from(t.heading_at(5.0).unwrap()).to_degrees() - 90.0).abs() < 1e-3);
        // The trail keeps its longitudes continuous.
        let fc: Value = serde_json::from_str(&TrackSet { tracks: vec![t] }.feature_collection_at(15.0)).unwrap();
        assert_eq!(fc["features"][0]["geometry"]["type"], "LineString");
        let line = fc["features"][0]["geometry"]["coordinates"].as_array().unwrap();
        assert_eq!(line.len(), 3);
        let lons: Vec<f64> = line.iter().map(|c| c[0].as_f64().unwrap()).collect();
        assert!(lons.windows(2).all(|w| (w[1] - w[0]).abs() < 2.0), "{lons:?}");
    }

    #[test]
    fn heading_is_clockwise_from_north_and_holds_through_a_stop() {
        // North for 10 s, stopped for 10 s, then west.
        let t = csv_track(b"t,lon,lat\n0,0,0\n10,0,1\n20,0,1\n30,-1,1\n");
        assert!(f64::from(t.heading_at(5.0).unwrap()).to_degrees().abs() < 1e-3);
        assert!(f64::from(t.heading_at(15.0).unwrap()).to_degrees().abs() < 1e-3, "a stop keeps the last heading");
        assert!((f64::from(t.heading_at(25.0).unwrap()).to_degrees() + 90.0).abs() < 1e-3);
        // Before the span: the first moving heading; after it: the last.
        assert!(f64::from(t.heading_at(-100.0).unwrap()).to_degrees().abs() < 1e-3);
        assert!((f64::from(t.heading_at(100.0).unwrap()).to_degrees() + 90.0).abs() < 1e-3);
        // A track that never moves has no heading.
        assert_eq!(csv_track(b"t,lon,lat\n0,5,5\n1,5,5\n").heading_at(0.5), None);
        assert_eq!(t.heading_at(f64::NAN), None);
    }

    #[test]
    fn csv_accepts_no_header_a_bearing_column_crlf_and_utc_times() {
        let t = csv_track(b"0,1,2\r\n1,1.5,2\r\n");
        assert_eq!(t.span(), (0.0, 1.0));
        let t = csv_track(b"t,lon,lat,bearing\n0,1,2,90\n1,1.5,2,\n");
        assert_eq!(t.position_at(1.0), Some((1.5, 2.0)));
        let start = parse_utc("2026-03-20T10:00:00Z").unwrap();
        let t = parse_track(b"t,lon,lat\n2026-03-20T10:00:05Z,0,0\n2026-03-20T10:00:07Z,1,0\n", "u", TrackStyle::default(), start).unwrap();
        assert_eq!(t.span(), (5.0, 7.0));
    }

    #[test]
    fn the_feature_collection_has_a_point_and_a_trail_once_two_vertices_are_in_the_window() {
        let style = TrackStyle { color: "#ff0000".into(), radius_px: 7.0, trail_s: 15.0 };
        let track = parse_track(b"t,lon,lat\n0,0,0\n10,1,0\n20,2,0\n", "bus", style, 0.0).unwrap();
        let set = TrackSet { tracks: vec![track] };
        let at = |t: f64| -> Value { serde_json::from_str(&set.feature_collection_at(t)).unwrap() };
        assert_eq!(at(-1.0)["features"].as_array().unwrap().len(), 0, "hidden before the track");
        let early = at(5.0);
        let features = early["features"].as_array().unwrap();
        assert_eq!(features.len(), 1, "one vertex in the window: no trail");
        assert_eq!(features[0]["geometry"]["type"], "Point");
        assert_eq!(features[0]["properties"]["circle-color"], "#ff0000");
        assert_eq!(features[0]["properties"]["circle-radius"], 7.0);
        let later = at(15.0);
        let features = later["features"].as_array().unwrap();
        assert_eq!(features.len(), 2);
        assert_eq!(features[0]["geometry"]["type"], "LineString");
        assert_eq!(features[0]["properties"]["line-width"], 3);
        assert_eq!(features[0]["properties"]["line-color"], "#ff0000");
        // Vertices at 0 and 10, then the head at 15 (lon 1.5).
        let line = features[0]["geometry"]["coordinates"].as_array().unwrap();
        assert_eq!(line.len(), 3);
        assert!((line[2][0].as_f64().unwrap() - 1.5).abs() < 1e-9);
        assert_eq!(features[1]["geometry"]["coordinates"], line[2]);
        // After the end the point holds, and the trail drains to the last vertex.
        let after = at(40.0);
        assert_eq!(after["features"].as_array().unwrap().len(), 1);
        assert_eq!(at(f64::NAN)["features"].as_array().unwrap().len(), 0);
    }

    // ---- fix round 1 ----

    /// F2: a bus stopped for a minute with GPS fixes every 5 s scattered
    /// by up to 2.5 m (a typical fleet tracker) keeps the heading it
    /// arrived with; the jitter never turns a `bearing: "track"` camera.
    #[test]
    fn gps_jitter_at_a_stop_does_not_turn_the_heading() {
        // One metre in degrees of latitude and of longitude at lat 1.3.
        let m_lat = 1.0 / 111_320.0;
        let m_lon = m_lat / 1.3_f64.to_radians().cos();
        let mut csv = String::from("t,lon,lat\n0,103.800,1.300\n");
        // East at ~11 m/s for 20 s, then parked at (103.802, 1.300).
        csv.push_str("20,103.802,1.300\n");
        let jitter = [(2.0, -1.0), (-1.5, 2.5), (0.5, -2.0), (-2.5, 0.0), (1.0, 1.5), (-0.5, -2.5), (2.5, 1.0), (-2.0, -0.5), (0.0, 2.0), (1.5, -1.5), (-1.0, 0.5), (2.0, 2.0)];
        for (k, (dx, dy)) in jitter.iter().enumerate() {
            let t = 25.0 + 5.0 * k as f64;
            csv.push_str(&format!("{t},{},{}\n", 103.802 + dx * m_lon, 1.300 + dy * m_lat));
        }
        let track = csv_track(csv.as_bytes());
        // Once the one-second window lies wholly in the stop (t >= 20.5),
        // the heading is exactly the arrival's; at t = 20 the window still
        // holds half a second of driving and reads within a degree of it.
        assert!((f64::from(track.heading_at(20.0).unwrap()).to_degrees() - 90.0).abs() < 1.0);
        for i in 2..=160 {
            let t = 20.0 + f64::from(i) * 0.25;
            let h = f64::from(track.heading_at(t).unwrap()).to_degrees();
            assert!((h - 90.0).abs() < 1e-3, "t={t}: the stop turned the heading to {h}");
        }
        // And the same camera path: a follow with bearing "track" holds east.
        let set = TrackSet { tracks: vec![track] };
        let camera = serde_json::json!([{"type":"jump","to":{"zoom":16}},{"type":"follow","track":"bus","duration":80,"bearing":"track"}]);
        let p = CameraPath::new((800.0, 600.0), &test_script(camera), &set).unwrap();
        for i in 0..=59 {
            let b = f64::from(p.pose_at(21.0 + f64::from(i)).camera.bearing).to_degrees();
            assert!((b - 90.0).abs() < 1e-3, "{b}");
        }
    }

    /// Re-review F2: a 1 Hz log parked for a minute, each fix up to 1.2 m
    /// from the stop and consecutive fixes about 2 m apart, so every
    /// one-second chord reads as 2 m/s of "motion" in a new direction.
    /// "Stopped" is decided over 2.5 s each way, so the heading holds the
    /// arrival's from the moment the bus stops, through the arrival
    /// seconds a single centred window would still see, to departure.
    #[test]
    fn one_hertz_gps_jitter_at_a_stop_does_not_turn_the_heading() {
        let m_lat = 1.0 / 111_320.0;
        let m_lon = m_lat / 1.3_f64.to_radians().cos();
        let mut csv = String::from("t,lon,lat\n");
        // East at ~11 m/s, 1 Hz, for 20 s, arriving at (103.802, 1.300).
        for k in 0..=20 {
            csv.push_str(&format!("{k},{},1.300\n", 103.800 + 0.0001 * f64::from(k)));
        }
        // Parked 60 s: fixes on a 1.2 m circle, stepping 110 degrees.
        for k in 1..=60 {
            let a = f64::from(k) * 110f64.to_radians();
            csv.push_str(&format!("{},{},{}\n", 20 + k, 103.802 + 1.2 * a.sin() * m_lon, 1.300 + 1.2 * a.cos() * m_lat));
        }
        // Then north at ~11 m/s for 20 s.
        for k in 1..=20 {
            let lat = 1.300 + 0.0001 * f64::from(k);
            csv.push_str(&format!("{},103.802,{lat}\n", 80 + k));
        }
        let track = csv_track(csv.as_bytes());
        // The premise: consecutive parked fixes a second apart are > 1 m
        // apart, so the one-second chord alone would spin.
        let chord = heading_between(track.world[30], track.world[31], 1.0);
        assert!(chord.is_some(), "the jitter must beat the one-second threshold for this test to mean anything");
        let degrees = |t: f64| f64::from(track.heading_at(t).unwrap()).to_degrees();
        for i in 0..240 {
            let t = 20.0 + f64::from(i) * 0.25;
            let h = degrees(t);
            assert!((h - 90.0).abs() < 1e-3, "t={t}: the stop turned the heading to {h}");
        }
        // Moving again, it follows the new direction.
        assert!(degrees(90.0).abs() < 1e-3, "{}", degrees(90.0));
        assert!((degrees(10.0) - 90.0).abs() < 1e-3);
    }

    /// Task 11 review F2, the companion: the same 1 Hz stop at a realistic
    /// urban jitter of 3 m and 5 m radius. Fixes that wander up to 6-10 m
    /// read as motion over the 2.5 s halves, so the heading DOES turn at
    /// such a stop -- the documented limit on `MIN_HEADING_SPEED_M_PER_S`.
    /// This pins the limit (and how much of the stop it spoils) so a
    /// later fix flips it deliberately.
    #[test]
    fn realistic_gps_jitter_at_a_stop_is_the_known_limit() {
        for radius in [3.0, 5.0] {
            let track = csv_track(parked_track_csv(radius).as_bytes());
            let turned = (0..240)
                .filter(|&i| {
                    let t = 20.0 + f64::from(i) * 0.25;
                    (f64::from(track.heading_at(t).unwrap()).to_degrees() - 90.0).abs() > 1.0
                })
                .count();
            eprintln!("jitter radius {radius} m: the heading turns on {turned} of 240 samples of the stop");
            assert!(turned > 0, "radius {radius} m: the limit has moved; update the docs and this test");
        }
    }

    /// A 1 Hz track: east at ~11 m/s for 20 s, parked 60 s with fixes on a
    /// `radius`-metre circle stepping 110 degrees, then north for 20 s.
    fn parked_track_csv(radius: f64) -> String {
        let m_lat = 1.0 / 111_320.0;
        let m_lon = m_lat / 1.3_f64.to_radians().cos();
        let mut csv = String::from("t,lon,lat\n");
        for k in 0..=20 {
            csv.push_str(&format!("{k},{},1.300\n", 103.800 + 0.0001 * f64::from(k)));
        }
        for k in 1..=60 {
            let a = f64::from(k) * 110f64.to_radians();
            csv.push_str(&format!("{},{},{}\n", 20 + k, 103.802 + radius * a.sin() * m_lon, 1.300 + radius * a.cos() * m_lat));
        }
        for k in 1..=20 {
            let lat = 1.300 + 0.0001 * f64::from(k);
            csv.push_str(&format!("{},103.802,{lat}\n", 80 + k));
        }
        csv
    }

    /// F2: the threshold is 1 m/s, so a walk (1.4 m/s) still has a heading.
    #[test]
    fn a_walking_pace_still_has_a_heading() {
        let m_lat = 1.0 / 111_320.0;
        let csv = format!("t,lon,lat\n0,0,0\n10,0,{}\n", 14.0 * m_lat);
        let h = csv_track(csv.as_bytes()).heading_at(5.0).expect("1.4 m/s moves");
        assert!(f64::from(h).to_degrees().abs() < 1e-3);
    }

    /// F3: a first line whose lon and lat are numbers is a data row with a
    /// bad time, not a bad header.
    #[test]
    fn a_bad_first_time_is_a_data_error_not_a_header_error() {
        let e = err(b"abc,1,2\n1,1,2\n");
        assert_eq!((e.code, e.path.as_str()), ("track_bad_row", "/lines/1/t"));
        assert!(e.message.contains("not a valid t"), "{}", e.message);
        let e = err(b"time,lon,lat\n1,1,2\n2,1,3\n");
        assert_eq!((e.code, e.path.as_str()), ("track_bad_row", "/lines/1"));
    }

    /// F4: GeoJSON times that are numeric strings are story seconds, as in
    /// CSV.
    #[test]
    fn geojson_accepts_numeric_time_strings_like_csv() {
        let g = br#"{"type":"Feature","properties":{"times":["0"," 2.5 "]},"geometry":{"type":"LineString","coordinates":[[0,0],[1,0,12.0]]}}"#;
        assert_eq!(parse_track(g, "g", TrackStyle::default(), 0.0).unwrap().span(), (0.0, 2.5));
        let csv = csv_track(b"t,lon,lat\n0,0,0\n2.5,1,0\n");
        assert_eq!(csv.span(), (0.0, 2.5));
    }

    /// F1: lines and columns are capped before anything is allocated per
    /// field.
    #[test]
    fn a_long_line_or_extra_columns_are_refused() {
        let mut long = b"t,lon,lat\n".to_vec();
        long.extend(std::iter::repeat_n(b',', MAX_CSV_LINE_BYTES + 1));
        assert_eq!(err(&long).path, "/lines/2");
        let e = err(b"t,lon,lat\n0,0,0,0,0\n1,1,1\n");
        assert_eq!((e.code, e.path.as_str()), ("track_bad_row", "/lines/2"));
        let big_json = format!(r#"{{"type":"Feature","properties":{{"times":[{}]}},"geometry":{{"type":"LineString","coordinates":[[0,0]]}}}}"#, vec!["0"; MAX_TRACK_POINTS + 1].join(","));
        assert_eq!(err(big_json.as_bytes()).code, "track_times_mismatch");
    }

    #[test]
    fn cloning_a_track_shares_its_samples() {
        let a = csv_track(b"t,lon,lat\n0,0,0\n1,1,0\n");
        let b = a.clone();
        assert!(Arc::ptr_eq(&a.times, &b.times) && Arc::ptr_eq(&a.world, &b.world));
        let set = TrackSet { tracks: vec![a] };
        assert!(Arc::ptr_eq(&set.share().tracks[0].world, &set.tracks[0].world));
    }
}
