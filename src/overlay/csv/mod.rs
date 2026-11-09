//! CSV and TSV as an overlay SOURCE: rows of points (or WKT / GeoJSON
//! geometries) and their other columns as typed feature properties, pushed
//! into the same overlay store a GeoJSON source is. There is no CSV
//! renderer: a CSV of points goes down the point path
//! ([`super::OverlayStore::set_source_points`] -- clustering, level of
//! detail, the off-thread index) and a CSV of lines or polygons goes down
//! the feature path, tiled when large, exactly as the same data handed in
//! as GeoJSON would.
//!
//! # Reading
//!
//! [`CsvSource::from_bytes`] parses an in-memory file; [`CsvSource::from_path`]
//! and [`CsvSource::from_url`] read one first (both block -- call them off
//! the UI thread, or let `MapView::load_source_csv` do it). In the browser
//! there is no filesystem: `from_bytes`, or the async `from_url` the web
//! build compiles in its place.
//!
//! - **Delimiter**: comma, tab or semicolon, sniffed from the first rows
//!   (the candidate that splits the header widest and the following rows
//!   most consistently), or fixed with [`CsvOptions::delimiter`].
//! - **Quoting**: RFC 4180 -- a quoted field may hold the delimiter, a line
//!   break, or a doubled `""`. CRLF, LF and lone CR all end a row.
//! - **Encoding**: UTF-8, with or without a byte-order mark.
//! - **Geometry**: a WKT column (`wkt`, `geometry`, `geom`, `the_geom`,
//!   `wkt_geom`), a GeoJSON-geometry column (a `geometry`/`geom` column
//!   whose cells are JSON objects, or one named with
//!   [`CsvOptions::geojson`]), or a latitude/longitude pair (`lat`,
//!   `latitude`, `y` / `lng`, `lon`, `long`, `longitude`, `x`). Names are
//!   matched case-insensitively; a geometry column, when there is one, wins
//!   over a lat/lon pair (it is the richer of the two). Any of them can be
//!   named explicitly, which also turns auto-detection off for that role.
//! - **Properties**: every other column, typed by inference over the whole
//!   column -- `bool` if every non-empty cell is `true`/`false`, number if
//!   every one parses as a finite number (a leading zero such as `02134`
//!   keeps the column a string: it is an identifier, not a quantity),
//!   string otherwise. An empty cell is a missing value. The types are what
//!   the paint expressions see, so `["get", "population"]` is a number.
//! - **Bad rows are skipped, not fatal**: [`CsvReport`] counts them by
//!   reason and keeps the first few line numbers.
//!
//! # Example
//!
//! ```
//! use gpui_maps::overlay::csv::{CsvOptions, CsvSource};
//!
//! let text = "name;lat;lon;population\n\
//!             Singapore;1,29;103,85;5900000\n\
//!             Tokyo;35,68;139,69;14000000\n\
//!             Nowhere;;;0\n";
//! let csv = CsvSource::from_bytes(text.as_bytes(), &CsvOptions::default()).unwrap();
//! assert_eq!(csv.len(), 2);
//! assert_eq!(csv.report().delimiter, b';');
//! assert_eq!(csv.report().rows_skipped, 1); // no coordinates
//! assert_eq!(csv.points().unwrap()[1], [139.69, 35.68]); // [lon, lat]
//! ```
//!
//! Then, on a `MapView` (`cx` is its `Context`):
//!
//! ```ignore
//! map.set_source_csv("cities", &csv, &FeaturePaint::default(), cx)?;
//! map.set_source_credit("cities", Some("Cities: Natural Earth"));
//! ```

pub mod wkt;

#[cfg(not(target_arch = "wasm32"))]
mod fetch_native;
#[cfg(target_arch = "wasm32")]
mod fetch_web;

use std::time::Duration;

use super::columns::{PropertyColumn, PropertyTable};
use super::geojson::OverlayGeometry;
use super::paint::FeaturePaint;
use super::{FeatureError, OverlayFeature};

/// The User-Agent a CSV fetch sends: the project and its repository,
/// nothing about the user.
pub const CSV_USER_AGENT: &str = concat!("gpui-maps/", env!("CARGO_PKG_VERSION"), " (+https://github.com/swatmobility/gpui-maps)");

/// [`CsvOptions::max_bytes`]' default: 512 MiB. A million rows of a dozen
/// columns is about 100 MB.
pub const DEFAULT_MAX_BYTES: u64 = 512 << 20;

/// [`CsvOptions::timeout`]'s default for a URL: the whole request, body
/// included.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// How many skipped rows [`CsvReport::first_skipped`] keeps.
pub const FIRST_SKIPPED_KEPT: usize = 10;

/// How a CSV is read. `Default` sniffs the delimiter and finds the
/// geometry columns by name.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvOptions {
    /// The field delimiter (`b','`, `b'\t'`, `b';'`, or any other single
    /// ASCII byte). `None` sniffs comma, tab and semicolon.
    pub delimiter: Option<u8>,
    /// The latitude column's header, matched case-insensitively. `None`
    /// looks for `lat`, `latitude`, `y`.
    pub lat: Option<String>,
    /// The longitude column's header. `None` looks for `lng`, `lon`,
    /// `long`, `longitude`, `x`.
    pub lon: Option<String>,
    /// A WKT geometry column's header. `None` looks for `wkt`, `geometry`,
    /// `geom`, `the_geom`, `wkt_geom`.
    pub wkt: Option<String>,
    /// A GeoJSON-geometry column's header (cells such as
    /// `{"type":"Point","coordinates":[1,2]}`). `None`: a `geometry`/`geom`
    /// column is taken as GeoJSON when its first non-empty cell is a JSON
    /// object.
    pub geojson: Option<String>,
    /// The most bytes read from a file or URL, and parsed from bytes.
    pub max_bytes: u64,
    /// A URL fetch's whole-request deadline.
    pub timeout: Duration,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self { delimiter: None, lat: None, lon: None, wkt: None, geojson: None, max_bytes: DEFAULT_MAX_BYTES, timeout: DEFAULT_TIMEOUT }
    }
}

impl CsvOptions {
    /// These options with a fixed delimiter.
    #[must_use]
    pub fn with_delimiter(mut self, delimiter: u8) -> Self {
        self.delimiter = Some(delimiter);
        self
    }

    /// These options with the latitude and longitude columns named.
    #[must_use]
    pub fn with_lat_lon(mut self, lat: impl Into<String>, lon: impl Into<String>) -> Self {
        self.lat = Some(lat.into());
        self.lon = Some(lon.into());
        self
    }

    /// These options with the WKT column named.
    #[must_use]
    pub fn with_wkt(mut self, column: impl Into<String>) -> Self {
        self.wkt = Some(column.into());
        self
    }

    /// These options with the GeoJSON-geometry column named.
    #[must_use]
    pub fn with_geojson(mut self, column: impl Into<String>) -> Self {
        self.geojson = Some(column.into());
        self
    }
}

/// Why a whole CSV was refused. A bad ROW is never this -- see
/// [`CsvReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvError {
    /// No header row: the input is empty or blank.
    Empty,
    /// No column the geometry can come from. Carries the header.
    NoGeometry(Vec<String>),
    /// A column named in [`CsvOptions`] is not in the header.
    MissingColumn(String),
    /// Larger than [`CsvOptions::max_bytes`].
    TooLarge { limit: u64, got: u64 },
    /// Reading a file failed.
    Io(String),
    /// A URL fetch failed (status, transport, or no HTTP client compiled
    /// into this build).
    Http(String),
    /// A URL fetch passed [`CsvOptions::timeout`].
    Timeout,
    /// The parsed rows could not be pushed into the overlay store.
    Push(String),
}

impl std::fmt::Display for CsvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the CSV has no header row"),
            Self::NoGeometry(header) => write!(
                f,
                "no geometry column: expected lat/lon (lat, latitude, y / lng, lon, long, longitude, x) or a WKT/GeoJSON column (wkt, geometry, geom), found {}",
                header.join(", ")
            ),
            Self::MissingColumn(name) => write!(f, "no column named {name:?}"),
            Self::TooLarge { limit, got } => write!(f, "{got} bytes is over the {limit}-byte limit"),
            Self::Io(e) => write!(f, "reading the CSV: {e}"),
            Self::Http(e) => write!(f, "fetching the CSV: {e}"),
            Self::Timeout => write!(f, "fetching the CSV timed out"),
            Self::Push(e) => write!(f, "pushing the CSV: {e}"),
        }
    }
}

impl std::error::Error for CsvError {}

/// Why one row was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CsvSkip {
    /// More or fewer fields than the header.
    FieldCount,
    /// A field that is not UTF-8.
    InvalidUtf8,
    /// A quoted field still open at the end of the input.
    UnterminatedQuote,
    /// Text after a closing quote (`"a"b`).
    StrayQuote,
    /// An empty latitude, longitude or geometry cell.
    MissingGeometry,
    /// A coordinate that is not a number.
    BadCoordinate,
    /// A latitude outside -90..90 or a longitude outside -180..180 (often
    /// projected metres, or lat/lon swapped).
    OutOfRange,
    /// A WKT cell that does not parse.
    BadWkt,
    /// A GeoJSON cell that does not parse.
    BadGeoJson,
    /// A geometry type with no overlay shape (`GEOMETRYCOLLECTION`, ...).
    UnsupportedGeometry,
}

impl CsvSkip {
    /// A short phrase for a report line.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::FieldCount => "wrong number of fields",
            Self::InvalidUtf8 => "not UTF-8",
            Self::UnterminatedQuote => "unterminated quote",
            Self::StrayQuote => "text after a closing quote",
            Self::MissingGeometry => "missing geometry",
            Self::BadCoordinate => "coordinate is not a number",
            Self::OutOfRange => "coordinate out of range",
            Self::BadWkt => "malformed WKT",
            Self::BadGeoJson => "malformed GeoJSON geometry",
            Self::UnsupportedGeometry => "unsupported geometry type",
        }
    }
}

/// Where a source's geometry came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvGeometryColumns {
    LatLon { lat: String, lon: String },
    Wkt(String),
    GeoJson(String),
}

/// A property column's inferred type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvColumnType {
    Number,
    Bool,
    String,
}

/// What a parse did: rows read, loaded and skipped (by reason, with the
/// first few line numbers), and what it decided about the file.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvReport {
    /// Data rows seen: every non-blank record after the header.
    pub rows_read: usize,
    /// Rows that became features.
    pub rows_loaded: usize,
    /// Rows skipped (`rows_read - rows_loaded`).
    pub rows_skipped: usize,
    /// Skipped rows by reason, most frequent first.
    pub skipped_by_reason: Vec<(CsvSkip, usize)>,
    /// The first [`FIRST_SKIPPED_KEPT`] skipped rows: the 1-based LINE each
    /// starts on (the header is line 1 of a file with no blank lead-in),
    /// and why.
    pub first_skipped: Vec<(usize, CsvSkip)>,
    /// The delimiter used (sniffed or given).
    pub delimiter: u8,
    /// The geometry's column(s).
    pub geometry: CsvGeometryColumns,
    /// Each property column and its inferred type, in header order.
    pub columns: Vec<(String, CsvColumnType)>,
}

impl std::fmt::Display for CsvReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "loaded {} of {} rows", self.rows_loaded, self.rows_read)?;
        if self.rows_skipped > 0 {
            write!(f, "; skipped {}", self.rows_skipped)?;
            let reasons: Vec<String> =
                self.skipped_by_reason.iter().map(|(why, n)| format!("{n} {}", why.describe())).collect();
            write!(f, " ({})", reasons.join(", "))?;
            let lines: Vec<String> = self.first_skipped.iter().map(|(line, _)| line.to_string()).collect();
            write!(f, ", first at line {}", lines.join(", "))?;
        }
        Ok(())
    }
}

/// A string column: one text buffer and each row's end offset (a missing
/// value is an empty string).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CsvStrings {
    text: String,
    ends: Vec<u32>,
}

impl CsvStrings {
    /// Row `i`'s value; `None` when empty or past the end.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&str> {
        let end = *self.ends.get(i)? as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        (end > start).then(|| &self.text[start..end])
    }

    /// Rows held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether no row is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Appends one row (`None` and `""` both read back as `None`). Shared
    /// with `overlay::fgb`, whose string columns are the same shape.
    pub(crate) fn push(&mut self, value: Option<&str>) {
        if let Some(value) = value {
            self.text.push_str(value);
        }
        self.ends.push(u32::try_from(self.text.len()).unwrap_or(u32::MAX));
    }

    pub(crate) fn bytes(&self) -> usize {
        self.text.capacity() + self.ends.capacity() * 4
    }
}

/// One property column's values, one per loaded row.
#[derive(Debug, Clone, PartialEq)]
pub enum CsvValues {
    Number(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    Str(CsvStrings),
}

impl CsvValues {
    fn kind(&self) -> CsvColumnType {
        match self {
            Self::Number(_) => CsvColumnType::Number,
            Self::Bool(_) => CsvColumnType::Bool,
            Self::Str(_) => CsvColumnType::String,
        }
    }
}

/// The loaded rows' geometry: a flat position list when every row is one
/// point (the common case, and the one the point path takes), else one
/// geometry per row.
#[derive(Debug, Clone, PartialEq)]
enum Geometries {
    Points(Vec<[f64; 2]>),
    Mixed(Vec<OverlayGeometry>),
}

/// A parsed CSV: geometry, typed property columns, and the parse report.
/// Push it with `MapView::set_source_csv` (or
/// [`super::OverlayStore::set_source_csv`] / `ShellOverlays::set_source_csv`).
#[derive(Debug, Clone, PartialEq)]
pub struct CsvSource {
    geometry: Geometries,
    columns: Vec<(String, CsvValues)>,
    report: CsvReport,
}

impl CsvSource {
    /// Parses a whole CSV held in memory. Fails only for a file-level
    /// problem ([`CsvError`]); bad rows are skipped and reported.
    pub fn from_bytes(bytes: &[u8], options: &CsvOptions) -> Result<Self, CsvError> {
        if bytes.len() as u64 > options.max_bytes {
            return Err(CsvError::TooLarge { limit: options.max_bytes, got: bytes.len() as u64 });
        }
        if bytes.len() as u64 >= u64::from(u32::MAX) {
            return Err(CsvError::TooLarge { limit: u64::from(u32::MAX) - 1, got: bytes.len() as u64 });
        }
        parse(bytes, options)
    }

    /// Features loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.report.rows_loaded
    }

    /// Whether no row loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.report.rows_loaded == 0
    }

    /// What the parse did.
    #[must_use]
    pub fn report(&self) -> &CsvReport {
        &self.report
    }

    /// Every feature's `[lon, lat]`, when every loaded row is a single
    /// point; `None` when any row is a line, a polygon or a multipoint.
    #[must_use]
    pub fn points(&self) -> Option<&[[f64; 2]]> {
        match &self.geometry {
            Geometries::Points(points) => Some(points),
            Geometries::Mixed(_) => None,
        }
    }

    /// Row `i`'s geometry, as the store holds it.
    #[must_use]
    pub fn geometry(&self, i: usize) -> Option<OverlayGeometry> {
        match &self.geometry {
            Geometries::Points(points) => points.get(i).map(|p| OverlayGeometry::Point(vec![(p[0], p[1])])),
            Geometries::Mixed(all) => all.get(i).cloned(),
        }
    }

    /// The property columns, in header order.
    #[must_use]
    pub fn columns(&self) -> &[(String, CsvValues)] {
        &self.columns
    }

    /// One property column by header name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&CsvValues> {
        self.columns.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Heap bytes held (geometry and columns), for a memory budget.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        let geometry = match &self.geometry {
            Geometries::Points(p) => p.capacity() * 16,
            Geometries::Mixed(g) => g.iter().map(|g| g.positions().count() * 16 + 32).sum::<usize>(),
        };
        geometry
            + self
                .columns
                .iter()
                .map(|(name, v)| {
                    name.capacity()
                        + match v {
                            CsvValues::Number(v) => v.capacity() * 16,
                            CsvValues::Bool(v) => v.capacity(),
                            CsvValues::Str(s) => s.bytes(),
                        }
                })
                .sum::<usize>()
    }

    /// Calls `f` with the property columns in the shape the store's typed
    /// doors take (`set_source_points`, `set_source_columns`). String
    /// columns are borrowed into a transient `Option<&str>` list.
    pub fn with_property_columns<R>(&self, f: impl FnOnce(&[(&str, PropertyColumn<'_>)]) -> R) -> R {
        let strings: Vec<Vec<Option<&str>>> = self
            .columns
            .iter()
            .filter_map(|(_, v)| match v {
                CsvValues::Str(s) => Some((0..s.len()).map(|i| s.get(i)).collect()),
                _ => None,
            })
            .collect();
        let mut strings = strings.iter();
        let columns: Vec<(&str, PropertyColumn<'_>)> = self
            .columns
            .iter()
            .map(|(name, values)| {
                let column = match values {
                    CsvValues::Number(v) => PropertyColumn::Number(v),
                    CsvValues::Bool(v) => PropertyColumn::Bool(v),
                    CsvValues::Str(_) => PropertyColumn::Str(strings.next().expect("one list per string column")),
                };
                (name.as_str(), column)
            })
            .collect();
        f(&columns)
    }
}

impl super::OverlayStore {
    /// Replaces source `name` with a parsed CSV.
    ///
    /// All points: the point path ([`Self::set_source_points`]), declaring
    /// [`super::cluster::ClusterOptions::points_only`] unless the caller
    /// declared clustering first. Anything else: the feature path, every
    /// row painted `paint` (a source paint declared with
    /// [`Self::declare_source_paint`] reads the CSV's typed columns).
    pub fn set_source_csv(
        &mut self,
        name: &str,
        csv: &CsvSource,
        paint: &FeaturePaint,
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        match &csv.geometry {
            Geometries::Points(points) => {
                csv.with_property_columns(|columns| self.set_source_points(name, points, None, paint, columns))
            }
            Geometries::Mixed(geometries) => {
                if self.source_cluster_options(name).is_some() {
                    return Err(FeatureError::SourcePaint {
                        source: name.to_string(),
                        reason: "a clustered or point-indexed source takes single-position Point features only".into(),
                    });
                }
                paint.validate().map_err(|reason| FeatureError::Feature { index: 0, id: None, reason })?;
                let properties = csv
                    .with_property_columns(|columns| PropertyTable::from_columns(geometries.len(), columns))
                    .map_err(|reason| FeatureError::SourcePaint { source: name.to_string(), reason })?;
                let features = geometries
                    .iter()
                    .enumerate()
                    .map(|(index, geometry)| OverlayFeature { index, id: None, geometry: geometry.clone(), paint: paint.clone() })
                    .collect();
                self.install(name, features, properties, display_zoom)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The reader
// ---------------------------------------------------------------------------

/// One field of the current record: a range of the input, or of the
/// scratch buffer when unescaping `""` forced a copy.
#[derive(Debug, Clone, Copy)]
enum Field {
    Input(u32, u32),
    Scratch(u32, u32),
}

/// An RFC 4180 record reader over the whole input.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    /// 1-based line of `pos`.
    line: usize,
    delim: u8,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], delim: u8) -> Self {
        Self { data, pos: 0, line: 1, delim }
    }

    /// Skips blank lines, then reads one record into `fields`. `None` at the
    /// end of the input; otherwise the record's first line and whether it
    /// read cleanly.
    fn record(&mut self, fields: &mut Vec<Field>, scratch: &mut Vec<u8>) -> Option<(usize, Result<(), CsvSkip>)> {
        let d = self.data;
        let n = d.len();
        // Blank lines.
        loop {
            match d.get(self.pos) {
                None => return None,
                Some(b'\n') => {
                    self.pos += 1;
                    self.line += 1;
                }
                Some(b'\r') => {
                    self.pos += 1;
                    if d.get(self.pos) == Some(&b'\n') {
                        self.pos += 1;
                    }
                    self.line += 1;
                }
                Some(_) => break,
            }
        }
        fields.clear();
        scratch.clear();
        let start_line = self.line;
        let mut error = None;
        let mut i = self.pos;
        loop {
            // One field, starting at `i`.
            if i < n && d[i] == b'"' {
                i += 1;
                let begin = i;
                let mut copied: Option<usize> = None;
                let mut closed = false;
                while i < n {
                    let b = d[i];
                    if b == b'"' {
                        if i + 1 < n && d[i + 1] == b'"' {
                            if copied.is_none() {
                                copied = Some(scratch.len());
                                scratch.extend_from_slice(&d[begin..i]);
                            }
                            scratch.push(b'"');
                            i += 2;
                            continue;
                        }
                        closed = true;
                        break;
                    }
                    // A line break inside quotes: `\n`, or a lone `\r`.
                    if b == b'\n' || (b == b'\r' && d.get(i + 1) != Some(&b'\n')) {
                        self.line += 1;
                    }
                    if copied.is_some() {
                        scratch.push(b);
                    }
                    i += 1;
                }
                if !closed {
                    self.pos = n;
                    return Some((start_line, Err(CsvSkip::UnterminatedQuote)));
                }
                fields.push(match copied {
                    Some(from) => Field::Scratch(from as u32, scratch.len() as u32),
                    None => Field::Input(begin as u32, i as u32),
                });
                i += 1; // the closing quote
                // Anything but a delimiter or a line end after it is a
                // stray: skipped over to resynchronise, and the row refused.
                while i < n && d[i] != self.delim && d[i] != b'\n' && d[i] != b'\r' {
                    error.get_or_insert(CsvSkip::StrayQuote);
                    i += 1;
                }
            } else {
                let begin = i;
                while i < n && d[i] != self.delim && d[i] != b'\n' && d[i] != b'\r' {
                    i += 1;
                }
                fields.push(Field::Input(begin as u32, i as u32));
            }
            // After a field: a delimiter, a line end, or the end.
            if i < n && d[i] == self.delim {
                i += 1;
                continue;
            }
            if i < n && d[i] == b'\r' {
                i += 1;
                if i < n && d[i] == b'\n' {
                    i += 1;
                }
                self.line += 1;
            } else if i < n && d[i] == b'\n' {
                i += 1;
                self.line += 1;
            }
            break;
        }
        self.pos = i;
        Some((start_line, error.map_or(Ok(()), Err)))
    }
}

fn field_bytes<'s>(data: &'s [u8], scratch: &'s [u8], field: Field) -> &'s [u8] {
    match field {
        Field::Input(a, b) => &data[a as usize..b as usize],
        Field::Scratch(a, b) => &scratch[a as usize..b as usize],
    }
}

/// The delimiter that splits the first rows widest and most consistently.
fn sniff_delimiter(data: &[u8]) -> u8 {
    // The first 64 KiB is plenty to judge from; cut at a line end so the
    // last record is whole.
    let window = &data[..data.len().min(64 << 10)];
    let mut best = (b',', 0usize, 0usize);
    for delim in *b",\t;" {
        let mut reader = Reader::new(window, delim);
        let (mut fields, mut scratch) = (Vec::new(), Vec::new());
        let Some(_) = reader.record(&mut fields, &mut scratch) else { return b',' };
        let width = fields.len();
        if width < 2 {
            continue;
        }
        let mut consistent = 0;
        for _ in 0..50 {
            match reader.record(&mut fields, &mut scratch) {
                Some((_, Ok(()))) if fields.len() == width => consistent += 1,
                Some(_) => {}
                None => break,
            }
        }
        if (consistent, width) > (best.1, best.2) {
            best = (delim, consistent, width);
        }
    }
    best.0
}

const LAT_NAMES: [&str; 3] = ["lat", "latitude", "y"];
const LON_NAMES: [&str; 5] = ["lng", "lon", "long", "longitude", "x"];
const WKT_NAMES: [&str; 5] = ["wkt", "geometry", "geom", "the_geom", "wkt_geom"];

/// The first header (in `names`' priority order) matching one of `names`.
fn find(header: &[String], names: &[&str]) -> Option<usize> {
    names.iter().find_map(|want| header.iter().position(|h| h.trim().eq_ignore_ascii_case(want)))
}

fn find_named(header: &[String], name: &str) -> Result<usize, CsvError> {
    header
        .iter()
        .position(|h| h.trim().eq_ignore_ascii_case(name.trim()))
        .ok_or_else(|| CsvError::MissingColumn(name.to_string()))
}

#[derive(Debug, Clone, Copy)]
enum GeometryAt {
    LatLon { lat: usize, lon: usize },
    Wkt(usize),
    GeoJson(usize),
    /// No place column, read as a plain table ([`CsvTable`] only).
    None,
}

/// A number cell: trimmed, finite, and -- when the delimiter is `;` -- a
/// decimal comma (`52,5`) read as a point.
fn parse_number(text: &str, decimal_comma: bool) -> Option<f64> {
    let text = text.trim();
    let first = *text.as_bytes().first()?;
    // `inf`, `NaN`, `infinity` parse as f64 and are not data.
    if !(first.is_ascii_digit() || matches!(first, b'-' | b'+' | b'.')) {
        return None;
    }
    let value = match text.parse::<f64>() {
        Ok(v) => v,
        Err(_) if decimal_comma && text.bytes().filter(|&b| b == b',').count() == 1 && !text.contains('.') => {
            text.replace(',', ".").parse::<f64>().ok()?
        }
        Err(_) => return None,
    };
    value.is_finite().then_some(value)
}

/// Whether a numeric-looking cell is really an identifier: `0123`, `-007`.
fn has_leading_zero(text: &str) -> bool {
    let digits = text.trim().trim_start_matches(['-', '+']).as_bytes();
    digits.len() > 1 && digits[0] == b'0' && digits[1].is_ascii_digit()
}

fn parse_bool(text: &str) -> Option<bool> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("true") {
        Some(true)
    } else if text.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

fn in_range(lon: f64, lat: f64) -> bool {
    (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat)
}

/// A property column while rows are read: the raw text of every loaded
/// row's cell.
#[derive(Default)]
struct RawColumn {
    text: String,
    ends: Vec<u32>,
}

impl RawColumn {
    fn push(&mut self, value: &str) {
        self.text.push_str(value);
        self.ends.push(self.text.len() as u32);
    }

    fn get(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.text[start..self.ends[i] as usize]
    }

    /// The column's type over every row, and its values in that type.
    fn infer(self, decimal_comma: bool) -> CsvValues {
        let rows = self.ends.len();
        let cells = || (0..rows).map(|i| self.get(i)).filter(|t| !t.trim().is_empty());
        let mut any = false;
        if cells().all(|t| {
            any = true;
            parse_bool(t).is_some()
        }) && any
        {
            return CsvValues::Bool(
                (0..rows).map(|i| parse_bool(self.get(i))).collect(),
            );
        }
        if any {
            let mut numbers = Vec::with_capacity(rows);
            let mut all = true;
            for i in 0..rows {
                let t = self.get(i);
                if t.trim().is_empty() {
                    numbers.push(None);
                    continue;
                }
                match parse_number(t, decimal_comma) {
                    Some(v) if !has_leading_zero(t) => numbers.push(Some(v)),
                    _ => {
                        all = false;
                        break;
                    }
                }
            }
            if all {
                return CsvValues::Number(numbers);
            }
        }
        let mut text = self.text;
        text.shrink_to_fit();
        let mut ends = self.ends;
        ends.shrink_to_fit();
        CsvValues::Str(CsvStrings { text, ends })
    }
}

/// The header, deduplicated (`name`, `name_2`) and with a name for every
/// blank column, so every property key is unique.
fn header_names(raw: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for (k, name) in raw.into_iter().enumerate() {
        let base = if name.trim().is_empty() { format!("column_{}", k + 1) } else { name.trim().to_string() };
        let mut candidate = base.clone();
        let mut n = 2;
        while out.contains(&candidate) {
            candidate = format!("{base}_{n}");
            n += 1;
        }
        out.push(candidate);
    }
    out
}

fn parse(bytes: &[u8], options: &CsvOptions) -> Result<CsvSource, CsvError> {
    parse_with(bytes, options, false)
}

/// `table_only`: no column is read as geometry; every column is a
/// property and every well-formed row is kept.
fn parse_with(bytes: &[u8], options: &CsvOptions, table_only: bool) -> Result<CsvSource, CsvError> {
    let data = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let delim = options.delimiter.unwrap_or_else(|| sniff_delimiter(data));
    let decimal_comma = delim == b';';
    let mut reader = Reader::new(data, delim);
    let (mut fields, mut scratch) = (Vec::new(), Vec::new());

    let Some((_, header_read)) = reader.record(&mut fields, &mut scratch) else { return Err(CsvError::Empty) };
    if header_read.is_err() {
        return Err(CsvError::Empty);
    }
    let header = header_names(
        fields.iter().map(|&f| String::from_utf8_lossy(field_bytes(data, &scratch, f)).into_owned()).collect(),
    );
    let width = header.len();

    // Which column(s) the geometry comes from.
    let named = |name: &Option<String>| name.as_deref().map(|n| find_named(&header, n)).transpose();
    let (lat_named, lon_named, wkt_named, geojson_named) =
        (named(&options.lat)?, named(&options.lon)?, named(&options.wkt)?, named(&options.geojson)?);
    let geometry_at = if table_only {
        GeometryAt::None
    } else if let Some(at) = geojson_named {
        GeometryAt::GeoJson(at)
    } else if let Some(at) = wkt_named {
        GeometryAt::Wkt(at)
    } else if lat_named.is_some() || lon_named.is_some() {
        let lat = lat_named.or_else(|| find(&header, &LAT_NAMES)).ok_or_else(|| CsvError::MissingColumn("lat".into()))?;
        let lon = lon_named.or_else(|| find(&header, &LON_NAMES)).ok_or_else(|| CsvError::MissingColumn("lon".into()))?;
        GeometryAt::LatLon { lat, lon }
    } else if let Some(at) = find(&header, &WKT_NAMES) {
        // GeoJSON or WKT: decided by the first non-empty cell, read ahead
        // on a throwaway reader.
        let mut ahead = Reader { data, pos: reader.pos, line: reader.line, delim };
        let (mut f2, mut s2) = (Vec::new(), Vec::new());
        let mut json = false;
        for _ in 0..1000 {
            match ahead.record(&mut f2, &mut s2) {
                Some((_, Ok(()))) if f2.len() == width => {
                    let cell = field_bytes(data, &s2, f2[at]);
                    if let Some(&b) = cell.iter().find(|b| !b.is_ascii_whitespace()) {
                        json = b == b'{';
                        break;
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        if json {
            GeometryAt::GeoJson(at)
        } else {
            GeometryAt::Wkt(at)
        }
    } else {
        match (find(&header, &LAT_NAMES), find(&header, &LON_NAMES)) {
            (Some(lat), Some(lon)) if lat != lon => GeometryAt::LatLon { lat, lon },
            _ => return Err(CsvError::NoGeometry(header)),
        }
    };
    let geometry_columns: Vec<usize> = match geometry_at {
        GeometryAt::LatLon { lat, lon } => vec![lat, lon],
        GeometryAt::Wkt(at) | GeometryAt::GeoJson(at) => vec![at],
        GeometryAt::None => Vec::new(),
    };
    let property_at: Vec<usize> = (0..width).filter(|k| !geometry_columns.contains(k)).collect();

    let mut raw: Vec<RawColumn> = property_at.iter().map(|_| RawColumn::default()).collect();
    let mut points: Vec<[f64; 2]> = Vec::new();
    let mut mixed: Option<Vec<OverlayGeometry>> = None;
    let mut rows_read = 0usize;
    let mut placeless = 0usize;
    let mut skipped: std::collections::BTreeMap<CsvSkip, usize> = std::collections::BTreeMap::new();
    let mut first_skipped = Vec::new();

    while let Some((line, read)) = reader.record(&mut fields, &mut scratch) {
        rows_read += 1;
        let mut cells: Vec<&str> = Vec::with_capacity(width);
        let outcome = read.and_then(|()| {
            if fields.len() != width {
                return Err(CsvSkip::FieldCount);
            }
            for &f in &fields {
                let bytes = field_bytes(data, &scratch, f);
                // Checked per field, so an invalid byte costs its row only.
                let text = std::str::from_utf8(bytes).map_err(|_| CsvSkip::InvalidUtf8)?;
                cells.push(text);
            }
            let geometry = read_geometry(&cells, geometry_at, decimal_comma)?;
            Ok(geometry)
        });
        match outcome {
            Ok(geometry) => {
                match (geometry, &mut mixed) {
                    (RowGeometry::None, _) => placeless += 1,
                    (RowGeometry::Point(p), None) => points.push(p),
                    (RowGeometry::Point(p), Some(all)) => all.push(OverlayGeometry::Point(vec![(p[0], p[1])])),
                    (RowGeometry::Other(g), None) => {
                        let mut all: Vec<OverlayGeometry> = std::mem::take(&mut points)
                            .into_iter()
                            .map(|p| OverlayGeometry::Point(vec![(p[0], p[1])]))
                            .collect();
                        all.push(g);
                        mixed = Some(all);
                    }
                    (RowGeometry::Other(g), Some(all)) => all.push(g),
                }
                for (column, &k) in raw.iter_mut().zip(&property_at) {
                    column.push(cells[k]);
                }
            }
            Err(why) => {
                *skipped.entry(why).or_default() += 1;
                if first_skipped.len() < FIRST_SKIPPED_KEPT {
                    first_skipped.push((line, why));
                }
            }
        }
    }

    let columns: Vec<(String, CsvValues)> = property_at
        .iter()
        .zip(raw)
        .map(|(&k, column)| (header[k].clone(), column.infer(decimal_comma)))
        .collect();
    let geometry = match mixed {
        Some(all) => Geometries::Mixed(all),
        None => {
            points.shrink_to_fit();
            Geometries::Points(points)
        }
    };
    let rows_loaded = placeless
        + match &geometry {
            Geometries::Points(p) => p.len(),
            Geometries::Mixed(g) => g.len(),
        };
    let mut skipped_by_reason: Vec<(CsvSkip, usize)> = skipped.into_iter().collect();
    skipped_by_reason.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let report = CsvReport {
        rows_read,
        rows_loaded,
        rows_skipped: rows_read - rows_loaded,
        skipped_by_reason,
        first_skipped,
        delimiter: delim,
        geometry: match geometry_at {
            GeometryAt::LatLon { lat, lon } => CsvGeometryColumns::LatLon { lat: header[lat].clone(), lon: header[lon].clone() },
            GeometryAt::Wkt(at) => CsvGeometryColumns::Wkt(header[at].clone()),
            GeometryAt::GeoJson(at) => CsvGeometryColumns::GeoJson(header[at].clone()),
            // Never read back: a table-only parse is seen through `CsvTable`.
            GeometryAt::None => CsvGeometryColumns::LatLon { lat: String::new(), lon: String::new() },
        },
        columns: columns.iter().map(|(name, values)| (name.clone(), values.kind())).collect(),
    };
    Ok(CsvSource { geometry, columns, report })
}

/// A CSV read as a plain table: every column typed as [`CsvSource`] types
/// them, no geometry. For a file with no place column (national monthly
/// figures, daily cash flows, page views) that feeds charts, KPIs and a
/// table rather than a map layer. Place columns, if any, are plain
/// columns here.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvTable {
    inner: CsvSource,
}

impl CsvTable {
    /// Parses a whole CSV held in memory as a table. Fails only for a
    /// file-level problem; a row of the wrong width is skipped and counted.
    pub fn from_bytes(bytes: &[u8], options: &CsvOptions) -> Result<Self, CsvError> {
        if bytes.len() as u64 > options.max_bytes {
            return Err(CsvError::TooLarge { limit: options.max_bytes, got: bytes.len() as u64 });
        }
        if bytes.len() as u64 >= u64::from(u32::MAX) {
            return Err(CsvError::TooLarge { limit: u64::from(u32::MAX) - 1, got: bytes.len() as u64 });
        }
        // Place columns are properties here: nothing is read as geometry.
        let options = CsvOptions { lat: None, lon: None, wkt: None, geojson: None, ..options.clone() };
        Ok(Self { inner: parse_with(bytes, &options, true)? })
    }

    /// Rows loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether no row loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The columns, in header order.
    #[must_use]
    pub fn columns(&self) -> &[(String, CsvValues)] {
        self.inner.columns()
    }

    /// One column by header name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&CsvValues> {
        self.inner.column(name)
    }

    /// Records read after the header.
    #[must_use]
    pub fn rows_read(&self) -> usize {
        self.inner.report.rows_read
    }

    /// Records skipped, by reason (most first).
    #[must_use]
    pub fn skipped_by_reason(&self) -> &[(CsvSkip, usize)] {
        &self.inner.report.skipped_by_reason
    }

    /// The delimiter used.
    #[must_use]
    pub fn delimiter(&self) -> u8 {
        self.inner.report.delimiter
    }
}

enum RowGeometry {
    Point([f64; 2]),
    Other(OverlayGeometry),
    None,
}

fn read_geometry(cells: &[&str], at: GeometryAt, decimal_comma: bool) -> Result<RowGeometry, CsvSkip> {
    match at {
        GeometryAt::None => Ok(RowGeometry::None),
        GeometryAt::LatLon { lat, lon } => {
            let (lat, lon) = (cells[lat].trim(), cells[lon].trim());
            if lat.is_empty() || lon.is_empty() {
                return Err(CsvSkip::MissingGeometry);
            }
            let lat = parse_number(lat, decimal_comma).ok_or(CsvSkip::BadCoordinate)?;
            let lon = parse_number(lon, decimal_comma).ok_or(CsvSkip::BadCoordinate)?;
            if !in_range(lon, lat) {
                return Err(CsvSkip::OutOfRange);
            }
            Ok(RowGeometry::Point([lon, lat]))
        }
        GeometryAt::Wkt(at) => {
            let text = cells[at].trim();
            if text.is_empty() {
                return Err(CsvSkip::MissingGeometry);
            }
            let geometry = wkt::parse(text).map_err(|e| match e {
                wkt::WktError::Malformed => CsvSkip::BadWkt,
                wkt::WktError::Empty => CsvSkip::MissingGeometry,
                wkt::WktError::Unsupported => CsvSkip::UnsupportedGeometry,
            })?;
            checked(geometry)
        }
        GeometryAt::GeoJson(at) => {
            let text = cells[at].trim();
            if text.is_empty() {
                return Err(CsvSkip::MissingGeometry);
            }
            let value: serde_json::Value = serde_json::from_str(text).map_err(|_| CsvSkip::BadGeoJson)?;
            if value.is_null() {
                return Err(CsvSkip::MissingGeometry);
            }
            let geometry = super::geojson::parse_geometry(Some(&value)).map_err(|reason| {
                if reason.starts_with("unsupported geometry type") {
                    CsvSkip::UnsupportedGeometry
                } else {
                    CsvSkip::BadGeoJson
                }
            })?;
            checked(geometry)
        }
    }
}

/// A geometry with at least one position, every one finite and in range;
/// a lone point as the flat form.
fn checked(geometry: OverlayGeometry) -> Result<RowGeometry, CsvSkip> {
    let mut any = false;
    for (lon, lat) in geometry.positions() {
        any = true;
        if !lon.is_finite() || !lat.is_finite() {
            return Err(CsvSkip::BadCoordinate);
        }
        if !in_range(lon, lat) {
            return Err(CsvSkip::OutOfRange);
        }
    }
    if !any {
        return Err(CsvSkip::MissingGeometry);
    }
    match geometry {
        OverlayGeometry::Point(p) if p.len() == 1 => Ok(RowGeometry::Point([p[0].0, p[0].1])),
        other => Ok(RowGeometry::Other(other)),
    }
}

#[cfg(test)]
mod tests;
