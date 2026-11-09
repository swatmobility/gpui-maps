//! Range and time filters on a point source, evaluated on the GPU, and the
//! playback clock that animates one (Kepler.gl's time-range filter).
//!
//! # How a filter costs nothing per frame
//!
//! deck.gl's `DataFilterExtension` shape: each instanced circle carries its
//! row's filter value as one more `f32` attribute
//! (`render::circles::CircleInstance::filter`), and the draw's uniform
//! (`render::circles::CircleParams::filter`) carries the window. `vs_circle`
//! collapses an instance outside the window to a degenerate quad. Moving
//! the window is therefore ONE 48-byte uniform write per draw per frame:
//! no re-push, no re-plan, no re-upload of a single instance.
//!
//! Declaring WHICH column filters ([`crate::map_view::MapView::set_source_filter_field`])
//! reads the column once into a [`PointFilter`] and rebuilds the source's
//! views from the index it already has (a few ms off the UI thread, once);
//! setting the window ([`crate::map_view::MapView::set_source_filter_range`])
//! is the uniform write.
//!
//! Values are stored relative to the column's minimum (`base`), so a
//! timestamp in milliseconds since 1970 keeps f32 precision: half a century
//! of milliseconds relative to its own start resolves to about two minutes.
//!
//! # Time columns
//!
//! A numeric column filters by its numbers. A text column filters by
//! [`parse_time`]'s reading of it (milliseconds since 1970, UTC), which
//! takes the forms Kepler's samples and ISO 8601 use: `2011-03-30
//! 21:10:05.47`, `2011/03/30 21:10:05`, `2011-03-30T21:10:05Z`, a bare date.

use super::columns::{Cell, PropertyTable};

/// A source's filter column, read once: one value per pushed point,
/// relative to `base`, `NaN` where the row has none (a NaN is outside every
/// window, as Kepler hides a row whose filter field is null).
#[derive(Debug, Clone, PartialEq)]
pub struct PointFilter {
    pub field: String,
    /// The column's minimum: what every stored value is relative to.
    pub base: f64,
    /// `[min, max]` of the column's finite values, absolute.
    pub domain: [f64; 2],
    pub values: Vec<f32>,
    /// Whether the column was text read as times.
    pub is_time: bool,
    /// Several fields packed into each value's bits
    /// ([`PointFilter::read_many`]): `values` are then code words (as
    /// `f32::from_bits`), `base` is 0, and `domain`/`is_time` are the first
    /// field's.
    pub packed: Option<PackedLayout>,
}

impl PointFilter {
    /// Reads `field` out of `properties` for `rows` points. `None` when the
    /// column does not exist or has no finite value.
    pub(crate) fn read(properties: &PropertyTable, rows: usize, field: &str) -> Option<Self> {
        let column = properties.column(field)?;
        let mut is_time = false;
        let raw: Vec<f64> = (0..rows)
            .map(|row| match properties.cell(column, row) {
                Some(Cell::Number(v)) => v,
                Some(Cell::Str(s)) => {
                    is_time = true;
                    parse_time(s).unwrap_or(f64::NAN)
                }
                _ => f64::NAN,
            })
            .collect();
        Self::from_values(field, &raw, is_time)
    }

    /// From absolute values (`NaN` for none).
    pub fn from_values(field: &str, raw: &[f64], is_time: bool) -> Option<Self> {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for &v in raw.iter().filter(|v| v.is_finite()) {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        if !lo.is_finite() {
            return None;
        }
        let values = raw.iter().map(|&v| if v.is_finite() { (v - lo) as f32 } else { f32::NAN }).collect();
        Some(Self { field: field.to_string(), base: lo, domain: [lo, hi], values, is_time, packed: None })
    }

    /// Point `i`'s value relative to `base` (`NaN` for none).
    pub fn value(&self, i: usize) -> f32 {
        self.values.get(i).copied().unwrap_or(f32::NAN)
    }

    /// Whether point `i` lies in the absolute window `range` -- the CPU
    /// statement of what `vs_circle` decides, for tests and hit filtering.
    pub fn passes(&self, i: usize, range: [f64; 2]) -> bool {
        let [lo, hi] = relative_window(self.base, range);
        let v = self.value(i);
        v >= lo && v <= hi
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        self.values.capacity() * 4 + self.field.capacity() + self.packed.as_ref().map_or(0, PackedLayout::bytes)
    }

    /// Reads up to [`MAX_FILTER_FIELDS`] columns into ONE packed filter
    /// (deck.gl's `DataFilterExtension` with `filterSize` > 1): each row's
    /// value is a code word holding every field's level, so the instance
    /// layout keeps its one `f32` and moving any field's window is still a
    /// uniform write. One field reads as [`Self::read`] does (exact, f32
    /// relative values). `None` when a column is missing or empty, or for
    /// no field or too many.
    pub(crate) fn read_many(properties: &PropertyTable, rows: usize, fields: &[&str]) -> Option<Self> {
        match fields {
            [] => None,
            [one] => Self::read(properties, rows, one),
            _ if fields.len() > MAX_FILTER_FIELDS => None,
            _ => {
                let columns: Vec<(String, Vec<f64>, bool)> = fields
                    .iter()
                    .map(|f| Self::read(properties, rows, f).map(|p| (p.field.clone(), p.absolute(), p.is_time)))
                    .collect::<Option<_>>()?;
                Self::packed_from_values(&columns)
            }
        }
    }

    /// A packed filter over `columns` (`(field, absolute values with NaN for
    /// none, is_time)`, all the same length). See [`PackedLayout`].
    pub fn packed_from_values(columns: &[(String, Vec<f64>, bool)]) -> Option<Self> {
        if columns.len() < 2 || columns.len() > MAX_FILTER_FIELDS {
            return None;
        }
        let rows = columns[0].1.len();
        if columns.iter().any(|c| c.1.len() != rows) {
            return None;
        }
        let layout = PackedLayout::new(columns)?;
        let values = (0..rows).map(|i| f32::from_bits(layout.code(columns.iter().map(|c| c.1[i])))).collect();
        let first = &layout.fields[0];
        Some(Self {
            field: columns.iter().map(|c| c.0.as_str()).collect::<Vec<_>>().join(","),
            base: 0.0,
            domain: first.domain,
            values,
            is_time: first.is_time,
            packed: Some(layout),
        })
    }

    /// Every value back as absolute (`NaN` for none). Scalar filters only.
    fn absolute(&self) -> Vec<f64> {
        self.values.iter().map(|&v| if v.is_nan() { f64::NAN } else { f64::from(v) + self.base }).collect()
    }

    /// Whether point `i` passes every field's window (`None`: that field is
    /// unfiltered) -- the CPU statement of what `vs_circle` decides for a
    /// packed filter. A scalar filter reads `ranges[0]`.
    pub fn passes_all(&self, i: usize, ranges: &[Option<[f64; 2]>]) -> bool {
        match &self.packed {
            None => ranges.first().copied().flatten().is_none_or(|r| self.passes(i, r)),
            Some(layout) => {
                let code = self.value(i).to_bits();
                layout.fields.iter().zip(layout.windows(ranges)).all(|(f, [lo, hi])| {
                    let level = (code >> f.shift) & f.mask();
                    level >= lo && level <= hi
                })
            }
        }
    }

    /// The fields' domains, in declaration order (one for a scalar filter).
    pub fn domains(&self) -> Vec<[f64; 2]> {
        match &self.packed {
            None => vec![self.domain],
            Some(layout) => layout.fields.iter().map(|f| f.domain).collect(),
        }
    }
}

/// The most fields one packed filter holds (deck.gl's `filterSize` limit,
/// and the uniform's four lanes).
pub const MAX_FILTER_FIELDS: usize = 4;

/// Bits of a packed code the levels may use. The word's bit 29 is always
/// set and bits 30-31 always clear, so as an `f32` it is a NORMAL number
/// (exponent neither 0 nor 255): no NaN or denormal a GPU might flush or
/// canonicalise on the way into `vs_circle`, which reads it back with
/// `bitcast<u32>`.
pub const PACKED_PAYLOAD_BITS: u32 = 29;
const PACKED_MARKER: u32 = 1 << PACKED_PAYLOAD_BITS;

/// One field of a packed filter: its levels and where they sit in the code.
#[derive(Debug, Clone, PartialEq)]
pub struct PackedField {
    pub field: String,
    pub domain: [f64; 2],
    pub is_time: bool,
    /// Sorted, distinct. EXACT: every distinct value of the column, and a
    /// row's level is its value's index. Otherwise each level is the lower
    /// edge of an equal-count bin (`exact` false).
    pub levels: Vec<f64>,
    pub exact: bool,
    pub shift: u32,
    pub bits: u32,
}

impl PackedField {
    fn mask(&self) -> u32 {
        (1u32 << self.bits) - 1
    }

    /// The code a row with no value gets: above every level, so outside
    /// every window.
    fn none_level(&self) -> u32 {
        self.mask()
    }

    fn level(&self, v: f64) -> u32 {
        if !v.is_finite() {
            return self.none_level();
        }
        // The last level <= v (exact: v's own index).
        let at = self.levels.partition_point(|&l| l <= v);
        at.saturating_sub(1) as u32
    }

    /// The closed window `range` as an inclusive level range. Exact: the
    /// levels inside it, so the GPU's test is the row's own `lo <= v <= hi`.
    /// Binned: every bin the window overlaps (a few rows either side of a
    /// window end that cuts a bin pass with it). An empty window is
    /// `[1, 0]`.
    fn window(&self, range: [f64; 2]) -> [u32; 2] {
        let (lo, hi) = (range[0].min(range[1]), range[0].max(range[1]));
        let first = if self.exact { self.levels.partition_point(|&l| l < lo) } else { self.levels.partition_point(|&l| l <= lo).saturating_sub(1) };
        let end = self.levels.partition_point(|&l| l <= hi);
        if end == 0 || first >= end {
            return [1, 0];
        }
        [first as u32, (end - 1) as u32]
    }
}

/// How a packed filter's fields share the code's [`PACKED_PAYLOAD_BITS`].
///
/// Each field asks for the bits its distinct values need (plus one code
/// for "no value"). When every field's ask fits, every field is EXACT: a
/// row's level is its value's rank among the column's distinct values, and
/// a window maps to the levels inside it -- the GPU decides exactly what
/// `lo <= v <= hi` would. LA's assessor rows (12 years built, 33 bedroom
/// counts, ~60k values) need 4 + 6 + 16 bits: exact. When the asks do not
/// fit, the smallest asks are granted first and the rest share what is
/// left, binned into equal-count levels (`PackedField::exact` false;
/// [`PointFilter::passes_all`] states the result either way).
#[derive(Debug, Clone, PartialEq)]
pub struct PackedLayout {
    pub fields: Vec<PackedField>,
}

impl PackedLayout {
    fn new(columns: &[(String, Vec<f64>, bool)]) -> Option<Self> {
        let mut sorted: Vec<Vec<f64>> = Vec::with_capacity(columns.len());
        for (_, values, _) in columns {
            let mut s: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
            if s.is_empty() {
                return None;
            }
            s.sort_by(f64::total_cmp);
            sorted.push(s);
        }
        let distinct: Vec<Vec<f64>> = sorted
            .iter()
            .map(|s| {
                let mut d = s.clone();
                d.dedup();
                d
            })
            .collect();
        let need = |n: usize| (usize::BITS - n.leading_zeros()).max(1);
        // Grant asks smallest first; whoever is left shares the rest.
        let mut order: Vec<usize> = (0..columns.len()).collect();
        order.sort_by_key(|&k| distinct[k].len());
        let mut bits = vec![0u32; columns.len()];
        let mut left = PACKED_PAYLOAD_BITS;
        for (at, &k) in order.iter().enumerate() {
            let fair = left / (order.len() - at) as u32;
            let ask = need(distinct[k].len());
            bits[k] = if ask <= fair { ask } else { fair };
            left -= bits[k];
        }
        let mut shift = 0;
        let mut fields = Vec::with_capacity(columns.len());
        for (k, (field, _, is_time)) in columns.iter().enumerate() {
            let b = bits[k];
            let capacity = (1usize << b) - 1;
            let exact = distinct[k].len() <= capacity;
            let levels = if exact {
                distinct[k].clone()
            } else {
                let s = &sorted[k];
                let mut edges: Vec<f64> = (0..capacity).map(|j| s[j * s.len() / capacity]).collect();
                edges.dedup();
                edges
            };
            let domain = [sorted[k][0], *sorted[k].last().expect("non-empty")];
            fields.push(PackedField { field: field.clone(), domain, is_time: *is_time, levels, exact, shift, bits: b });
            shift += b;
        }
        Some(Self { fields })
    }

    /// One row's code word, from its absolute values in field order.
    fn code(&self, values: impl Iterator<Item = f64>) -> u32 {
        self.fields.iter().zip(values).fold(PACKED_MARKER, |code, (f, v)| code | (f.level(v) << f.shift))
    }

    /// Every field's inclusive level window: `ranges[k]`, or all levels
    /// (no-value rows excluded, as Kepler hides a null) when `None`.
    pub fn windows(&self, ranges: &[Option<[f64; 2]>]) -> Vec<[u32; 2]> {
        self.fields
            .iter()
            .enumerate()
            .map(|(k, f)| match ranges.get(k).copied().flatten() {
                Some(r) => f.window(r),
                None => [0, f.none_level().saturating_sub(1)],
            })
            .collect()
    }

    /// The draw's uniform lanes for `ranges`: shift, mask, lo and hi per
    /// field (an unused lane has mask 0 and passes).
    #[must_use]
    pub fn uniform(&self, ranges: &[Option<[f64; 2]>]) -> PackedUniform {
        let mut u = PackedUniform::default();
        for (k, (f, w)) in self.fields.iter().zip(self.windows(ranges)).enumerate() {
            u.shift[k] = f.shift;
            u.mask[k] = f.mask();
            u.lo[k] = w[0];
            u.hi[k] = w[1];
        }
        u
    }

    fn bytes(&self) -> usize {
        self.fields.iter().map(|f| f.levels.capacity() * 8 + f.field.capacity()).sum()
    }
}

/// [`PackedLayout::uniform`]: `render::circles::CircleParams`' packed lanes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackedUniform {
    pub shift: [u32; 4],
    pub mask: [u32; 4],
    pub lo: [u32; 4],
    pub hi: [u32; 4],
}

/// The absolute window `range` as the f32 pair a draw's uniform carries,
/// relative to `base`. Widened by one f32 ulp each side so a value equal
/// to an end, rounded independently, still passes (the window is closed).
#[must_use]
pub fn relative_window(base: f64, range: [f64; 2]) -> [f32; 2] {
    let lo = (range[0] - base) as f32;
    let hi = (range[1] - base) as f32;
    [next_down(lo), next_up(hi)]
}

fn next_up(v: f32) -> f32 {
    if v.is_nan() || v == f32::INFINITY {
        return v;
    }
    if v == 0.0 {
        return f32::from_bits(1);
    }
    let bits = v.to_bits();
    f32::from_bits(if v > 0.0 { bits + 1 } else { bits - 1 })
}

fn next_down(v: f32) -> f32 {
    -next_up(-v)
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian; Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `y-m-d` of the day `days` after 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Milliseconds since 1970 as `YYYY-MM-DD` (UTC): the label a time filter
/// prints for a window's ends.
#[must_use]
pub fn format_date(ms: f64) -> String {
    if !ms.is_finite() {
        return String::new();
    }
    let (y, m, d) = civil_from_days((ms / 86_400_000.0).floor() as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A date-time as milliseconds since 1970-01-01T00:00:00Z, or `None`.
///
/// `YYYY-MM-DD` or `YYYY/MM/DD`, optionally followed by `T` or a space and
/// `hh:mm[:ss[.fff]]`, optionally followed by `Z` or `+hh:mm`/`-hh:mm`. No
/// zone means UTC.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let b = s.as_bytes();
    let digits = |from: usize, n: usize| -> Option<i64> {
        let part = s.get(from..from + n)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok())?
    };
    let year = digits(0, 4)?;
    let sep = *b.get(4)?;
    if sep != b'-' && sep != b'/' {
        return None;
    }
    let month = digits(5, 2)? as u32;
    if *b.get(7)? != sep {
        return None;
    }
    let day = digits(8, 2)? as u32;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut ms = days_from_civil(year, month, day) as f64 * 86_400_000.0;
    let mut at = 10;
    if at == b.len() {
        return Some(ms);
    }
    if b[at] != b'T' && b[at] != b' ' {
        return None;
    }
    at += 1;
    let hour = digits(at, 2)?;
    if b.get(at + 2) != Some(&b':') {
        return None;
    }
    let minute = digits(at + 3, 2)?;
    at += 5;
    let mut seconds = 0.0;
    if b.get(at) == Some(&b':') {
        let start = at + 1;
        let mut end = start;
        while end < b.len() && (b[end].is_ascii_digit() || b[end] == b'.') {
            end += 1;
        }
        seconds = s.get(start..end)?.parse().ok()?;
        at = end;
    }
    if hour > 24 || minute > 59 || !(0.0..61.0).contains(&seconds) {
        return None;
    }
    ms += (hour * 3_600_000 + minute * 60_000) as f64 + seconds * 1000.0;
    // A space before the zone, as NYC TLC's `2015-01-15 19:05:39 +00:00`.
    if b.get(at) == Some(&b' ') && matches!(b.get(at + 1), Some(b'+' | b'-' | b'Z')) {
        at += 1;
    }
    match b.get(at) {
        None => Some(ms),
        Some(b'Z') if at + 1 == b.len() => Some(ms),
        Some(&sign) if sign == b'+' || sign == b'-' => {
            let oh = digits(at + 1, 2)?;
            let om = if b.get(at + 3) == Some(&b':') { digits(at + 4, 2)? } else { digits(at + 3, 2).unwrap_or(0) };
            let offset = (oh * 3_600_000 + om * 60_000) as f64;
            Some(if sign == b'+' { ms - offset } else { ms + offset })
        }
        _ => None,
    }
}

/// Kepler.gl's playback speed: one frame moves the window by the domain's
/// span over `BASE_SPEED` (kepler.gl `default-settings`: `BASE_SPEED =
/// 600`, `FPS = 60`), so at speed 1 the window crosses the whole domain in
/// ten seconds.
pub const KEPLER_BASE_SPEED: f64 = 600.0;
pub const KEPLER_FPS: f64 = 60.0;

/// The playback clock of a time-range filter: a window of fixed width that
/// slides across `domain` while playing and wraps to the start at the end
/// (Kepler's `free` animation window). Pure: the caller asks
/// [`Self::tick`] on each frame it draws, with the time now, and draws
/// another frame only when [`Self::is_playing`] -- so a paused map asks
/// for no frames at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Playback {
    domain: [f64; 2],
    window: [f64; 2],
    /// Kepler's speed multiplier.
    pub speed: f64,
    playing: bool,
    /// The clock reading of the last tick, in seconds.
    last: Option<f64>,
}

impl Playback {
    /// A paused clock over `domain`, with `window` (clamped into it).
    pub fn new(domain: [f64; 2], window: [f64; 2]) -> Self {
        let mut p = Self { domain, window: domain, speed: 1.0, playing: false, last: None };
        p.set_window(window);
        p
    }

    pub fn domain(&self) -> [f64; 2] {
        self.domain
    }

    pub fn window(&self) -> [f64; 2] {
        self.window
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// The window, clamped into the domain (a brush on the histogram).
    pub fn set_window(&mut self, window: [f64; 2]) {
        let [d0, d1] = self.domain;
        let lo = window[0].min(window[1]).clamp(d0, d1);
        let hi = window[0].max(window[1]).clamp(d0, d1);
        self.window = [lo, hi];
    }

    /// Starts playing. A window already at the end starts over.
    pub fn play(&mut self) {
        if self.window[1] >= self.domain[1] {
            let width = self.window[1] - self.window[0];
            self.window = [self.domain[0], self.domain[0] + width];
        }
        self.playing = true;
        self.last = None;
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.last = None;
    }

    pub fn toggle(&mut self) {
        if self.playing {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Domain units per second at the current speed.
    pub fn rate(&self) -> f64 {
        (self.domain[1] - self.domain[0]) / KEPLER_BASE_SPEED * KEPLER_FPS * self.speed
    }

    /// Advances to clock reading `now_s` (seconds, any epoch) and returns
    /// the new window, or `None` when paused (nothing moved, draw nothing).
    /// The first tick after `play` only starts the clock.
    pub fn tick(&mut self, now_s: f64) -> Option<[f64; 2]> {
        if !self.playing {
            return None;
        }
        let dt = self.last.map_or(0.0, |last| (now_s - last).clamp(0.0, 0.25));
        self.last = Some(now_s);
        let width = self.window[1] - self.window[0];
        let mut lo = self.window[0] + dt * self.rate();
        if lo + width > self.domain[1] {
            // Wrap: the window starts over at the domain's start.
            lo = self.domain[0];
        }
        self.window = [lo, lo + width];
        Some(self.window)
    }
}

/// Counts of `values` in `bins` equal bins over `domain` (a histogram for
/// the filter's brush). NaNs and values outside the domain are skipped.
#[must_use]
pub fn histogram(values: impl IntoIterator<Item = f64>, domain: [f64; 2], bins: usize) -> Vec<u32> {
    let mut out = vec![0u32; bins.max(1)];
    let span = domain[1] - domain[0];
    if !(span > 0.0) {
        return out;
    }
    let n = out.len();
    for v in values {
        if !(v >= domain[0] && v <= domain[1]) {
            continue;
        }
        let at = (((v - domain[0]) / span) * n as f64) as usize;
        out[at.min(n - 1)] += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse_in_kepler_and_iso_forms() {
        assert_eq!(parse_time("1970-01-01"), Some(0.0));
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), Some(86_400_000.0));
        // Kepler's earthquake sample's form, with hundredths.
        let t = parse_time("2011/03/30 21:10:05.47").unwrap();
        assert_eq!(t, 1_301_519_405_470.0);
        assert_eq!(parse_time("2011-03-30T21:10:05.47Z"), Some(t));
        assert_eq!(parse_time("2011-03-30T23:10:05.47+02:00"), Some(t));
        assert_eq!(parse_time("2015-01-15 19:05:39 +00:00"), parse_time("2015-01-15T19:05:39Z"));
        assert_eq!(parse_time("2015-01-15 21:05:39 +02:00"), parse_time("2015-01-15T19:05:39Z"));
        // Kepler's own filter start for that sample (`cfg_earthquakes`).
        assert_eq!(parse_time("1991/01/05 03:38:42.47"), Some(663_046_722_470.0));
        assert_eq!(parse_time("1960-02-29"), Some(-310_521_600_000.0));
        assert_eq!(format_date(t), "2011-03-30");
        assert_eq!(format_date(0.0), "1970-01-01");
        assert_eq!(format_date(-310_521_600_000.0), "1960-02-29");
        assert_eq!(format_date(-1.0), "1969-12-31");
        for bad in ["", "yesterday", "2011-3-30", "2011-13-01", "2011-03-30X", "2011-03-30 25:00"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_filter_holds_values_relative_to_its_minimum_and_tests_closed_windows() {
        let raw = [1_000_000_000_000.0, 1_000_000_060_000.0, f64::NAN, 1_500_000_000_000.0];
        let f = PointFilter::from_values("t", &raw, true).unwrap();
        assert_eq!(f.base, 1_000_000_000_000.0);
        assert_eq!(f.domain, [1_000_000_000_000.0, 1_500_000_000_000.0]);
        assert_eq!(f.value(0), 0.0);
        // Inside, on both closed ends, and outside.
        assert!(f.passes(1, [1_000_000_060_000.0, 1_000_000_060_000.0]));
        assert!(f.passes(0, [1_000_000_000_000.0, 1_100_000_000_000.0]));
        assert!(!f.passes(3, [1_000_000_000_000.0, 1_100_000_000_000.0]));
        assert!(f.passes(3, [1_400_000_000_000.0, 1_500_000_000_000.0]));
        // A row without a value is outside every window.
        assert!(!f.passes(2, [f64::MIN, f64::MAX]));
        assert!(PointFilter::from_values("t", &[f64::NAN], false).is_none());
    }

    /// A packed filter is exact while the fields' distinct values fit the
    /// code; past that the big field bins by equal counts and a window
    /// passes every bin it overlaps (never fewer rows than exact). The code
    /// word is always a normal f32.
    #[test]
    fn a_packed_filter_is_exact_when_it_fits_and_permissive_when_binned() {
        let n = 50_000usize;
        let a: Vec<f64> = (0..n).map(|i| (i % 12) as f64).collect();
        let b: Vec<f64> = (0..n).map(|i| if i == 7 { f64::NAN } else { i as f64 * 0.5 }).collect();
        let f = PointFilter::packed_from_values(&[("a".into(), a.clone(), false), ("b".into(), b.clone(), false)]).unwrap();
        let layout = f.packed.as_ref().unwrap();
        assert!(layout.fields[0].exact && layout.fields[0].bits == 4);
        assert!(layout.fields[1].exact, "50k distinct values fit in the 25 bits left");
        assert!(f.values.iter().all(|v| v.is_normal()), "every code is a normal float");
        let ranges = [Some([3.0, 5.0]), Some([100.0, 20_000.25])];
        let exact = |i: usize| a[i] >= 3.0 && a[i] <= 5.0 && b[i] >= 100.0 && b[i] <= 20_000.25;
        assert!((0..n).all(|i| f.passes_all(i, &ranges) == exact(i)));
        assert!(!f.passes_all(7, &[None, None]), "no value: hidden");
        assert!((0..n).filter(|&i| i != 7).all(|i| f.passes_all(i, &[None, None])));

        // Four wide fields: each gets 7 bits, so the binned ones err wide.
        let cols: Vec<(String, Vec<f64>, bool)> = (0..4).map(|k| (format!("f{k}"), (0..n).map(|i| ((i * (k + 3)) % n) as f64).collect(), false)).collect();
        let f = PointFilter::packed_from_values(&cols).unwrap();
        assert!(f.packed.as_ref().unwrap().fields.iter().all(|f| !f.exact));
        let ranges = [Some([1000.0, 20_000.0]), None, Some([0.0, 30_000.0]), None];
        let exact = (0..n).filter(|&i| cols[0].1[i] >= 1000.0 && cols[0].1[i] <= 20_000.0 && cols[2].1[i] <= 30_000.0).count();
        let packed = (0..n).filter(|&i| f.passes_all(i, &ranges)).count();
        assert!(packed >= exact && packed <= exact + exact / 10, "{packed} vs {exact}");
        assert!(PointFilter::packed_from_values(&cols[..1]).is_none(), "one field is the scalar filter");
    }

    #[test]
    fn playback_moves_at_kepler_speed_wraps_and_is_silent_when_paused() {
        let mut p = Playback::new([0.0, 600.0], [0.0, 100.0]);
        assert_eq!(p.tick(0.0), None, "paused: nothing moves");
        p.play();
        assert_eq!(p.tick(10.0), Some([0.0, 100.0]), "the first tick starts the clock");
        // 600 units over BASE_SPEED 600 at 60 fps: 60 units a second.
        let w = p.tick(10.1).unwrap();
        assert!((w[0] - 6.0).abs() < 1e-9, "{w:?}");
        assert!((w[1] - w[0] - 100.0).abs() < 1e-9, "the width holds");
        // A long stall moves at most a quarter second's worth.
        let w = p.tick(100.0).unwrap();
        assert!((w[0] - 21.0).abs() < 1e-9, "{w:?}");
        // At the end it wraps to the start.
        p.set_window([495.0, 595.0]);
        let w = p.tick(100.25).unwrap();
        assert_eq!(w, [0.0, 100.0]);
        p.pause();
        assert_eq!(p.tick(101.0), None);
        assert_eq!(p.window(), [0.0, 100.0]);
        // The whole domain takes ten seconds at speed 1.
        assert!((600.0 / p.rate() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_window_brushed_outside_the_domain_is_clamped_and_play_restarts_at_the_end() {
        let mut p = Playback::new([10.0, 20.0], [5.0, 12.0]);
        assert_eq!(p.window(), [10.0, 12.0]);
        p.set_window([25.0, 15.0]);
        assert_eq!(p.window(), [15.0, 20.0]);
        p.play();
        assert_eq!(p.window(), [10.0, 15.0], "a window at the end starts over");
    }

    #[test]
    fn histogram_bins_inclusive_of_the_top() {
        assert_eq!(histogram([0.0, 0.5, 1.0, 9.99, 10.0, 11.0, f64::NAN], [0.0, 10.0], 10), vec![2, 1, 0, 0, 0, 0, 0, 0, 0, 2]);
        assert_eq!(histogram([1.0], [1.0, 1.0], 4), vec![0, 0, 0, 0]);
    }
}
