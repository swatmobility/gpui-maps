//! FlatGeobuf (`.fgb`) as an overlay SOURCE: a binary vector file with a
//! typed column schema and a packed Hilbert R-tree, read into the same
//! overlay store a GeoJSON or CSV source is. There is no FlatGeobuf
//! renderer: a file of points goes down the point path
//! ([`super::OverlayStore::set_source_points`]) and a file of lines or
//! polygons down the tiled feature path, exactly as the same data handed
//! in as GeoJSON would. Properties keep their schema's types (every integer
//! and float type is a number, `Bool` a bool, `String`/`Json`/`DateTime` a
//! string; `Binary` columns are dropped), so paint expressions read them.
//!
//! # Reading
//!
//! - [`FgbSource::from_bytes`] parses an in-memory file (every target).
//! - [`FgbSource::from_path`] reads a local file and [`FgbSource::from_url`]
//!   an http(s) one (native; both block -- call them off the UI thread, or
//!   let `MapView::load_source_fgb` do it). The browser build compiles an
//!   async `from_url` in its place, over `fetch` range requests.
//! - [`FgbOptions::bbox`] loads only the features whose boxes intersect
//!   `[west, south, east, north]`. With an index this reads the index
//!   nodes on the way down and then just those features -- from a file, by
//!   seek; from a URL, by HTTP `Range` requests (adjacent ranges merged);
//!   from bytes, by slicing.
//! - [`FgbRemote`] holds an open remote file (its header and index layout)
//!   for repeated bbox queries: `MapView::load_source_fgb` streams a large
//!   URL by viewport through it (see [`FgbOptions::stream_above_bytes`]).
//!
//! # Refusals and warnings
//!
//! - A header CRS other than EPSG:4326 or OGC CRS84 is refused
//!   ([`FgbError::Crs`]), naming the CRS, unless
//!   [`FgbOptions::accept_any_crs`] says to draw the coordinates as
//!   longitude/latitude anyway (right for a geographic datum such as NAD83,
//!   metres off; wrong for any projected CRS). A header with no CRS is read
//!   as WGS84 with a warning, and refused if its envelope is plainly not
//!   longitude/latitude.
//! - A file with no index is scanned whole (a `bbox` filters after the
//!   read), with a warning in the report.
//! - A bad FEATURE is skipped and counted ([`FgbReport::skipped_by_reason`]):
//!   unsupported geometry (collections, curves), empty geometry, a
//!   coordinate out of range, a malformed flatbuffer.
//!
//! # Example
//!
//! ```
//! use gpui_maps::overlay::fgb::{write, FgbOptions, FgbSource};
//! use gpui_maps::overlay::OverlayGeometry;
//!
//! let features: Vec<write::WriteFeature> = (0..100)
//!     .map(|i| write::WriteFeature {
//!         geometry: OverlayGeometry::Point(vec![(f64::from(i) - 50.0, 10.0)]),
//!         properties: vec![write::WriteValue::Double(f64::from(i))],
//!     })
//!     .collect();
//! let bytes = write::write(&[("value", write::WriteType::Double)], &features, 16);
//!
//! let all = FgbSource::from_bytes(&bytes, &FgbOptions::default()).unwrap();
//! assert_eq!(all.len(), 100);
//! let some = FgbSource::from_bytes(&bytes, &FgbOptions { bbox: Some([-10.0, 0.0, 9.5, 20.0]), ..FgbOptions::default() }).unwrap();
//! assert_eq!(some.len(), 20); // longitudes -10..=9
//! ```

mod format;
pub mod write;

#[cfg(not(target_arch = "wasm32"))]
mod fetch_native;
#[cfg(target_arch = "wasm32")]
mod fetch_web;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::time::Duration;

pub use format::FgbSkip;
#[cfg(feature = "geoparquet")]
pub(crate) use format::bbox_of;
pub(crate) use format::intersects;
use format::{node_at, u32_at, Value, NODE_BYTES};

use super::columns::{PropertyColumn, PropertyTable};
use super::csv::{CsvStrings, CsvValues};
use super::geojson::OverlayGeometry;
use super::paint::FeaturePaint;
use super::{FeatureError, OverlayFeature};

/// The User-Agent a FlatGeobuf fetch sends: the project and its
/// repository, nothing about the user.
pub const FGB_USER_AGENT: &str = concat!("gpui-maps/", env!("CARGO_PKG_VERSION"), " (+https://github.com/swatmobility/gpui-maps)");

/// [`FgbOptions::max_bytes`]' default: 512 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 512 << 20;

/// [`FgbOptions::stream_above_bytes`]' default: 8 MiB.
///
/// A file this size is about 50k-150k simple features: one request that
/// lands in one to three seconds on a 25-50 Mbit/s link and a whole-file
/// parse in tens of milliseconds (`fgb.load_ms` reads a million points in
/// a few hundred), after which pans and zooms cost nothing. Past it, the
/// download grows linearly while a viewport query stays a handful of
/// range requests (the index levels, then the features in view), so the
/// first draw is faster by streaming and memory stays bounded by
/// [`FgbOptions::stream_max_features`] instead of the file.
pub const DEFAULT_STREAM_ABOVE_BYTES: u64 = 8 << 20;

/// [`FgbOptions::stream_max_features`]' default. The point path draws a
/// quarter million points interactively, and the feature path tiles a
/// quarter million polygons off the UI thread; the re-push on each settle
/// is the cost this bounds.
pub const DEFAULT_STREAM_MAX_FEATURES: usize = 250_000;

/// [`FgbOptions::timeout`]'s default: one request, body included.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// How a FlatGeobuf file is read.
#[derive(Debug, Clone, PartialEq)]
pub struct FgbOptions {
    /// Load only features whose bounding boxes intersect
    /// `[west, south, east, north]` (degrees). `None` loads everything.
    pub bbox: Option<[f64; 4]>,
    /// The most bytes read in one go: a whole file, or one merged range.
    pub max_bytes: u64,
    /// One HTTP request's deadline.
    pub timeout: Duration,
    /// Draw a file whose CRS is not WGS84 as longitude/latitude anyway
    /// (for a geographic datum such as NAD83 or ETRS89). Off: refused.
    pub accept_any_crs: bool,
    /// `MapView::load_source_fgb` with a URL: a file larger than this
    /// (by its `Content-Range` total) is streamed by viewport rather than
    /// downloaded whole. See [`DEFAULT_STREAM_ABOVE_BYTES`]. A file with
    /// no index is always read whole.
    pub stream_above_bytes: u64,
    /// A streamed source's resident bound: the most features held at once.
    /// A viewport that hits more is thinned evenly along the index's
    /// Hilbert order; features leaving the view are evicted first.
    pub stream_max_features: usize,
}

impl Default for FgbOptions {
    fn default() -> Self {
        Self {
            bbox: None,
            max_bytes: DEFAULT_MAX_BYTES,
            timeout: DEFAULT_TIMEOUT,
            accept_any_crs: false,
            stream_above_bytes: DEFAULT_STREAM_ABOVE_BYTES,
            stream_max_features: DEFAULT_STREAM_MAX_FEATURES,
        }
    }
}

impl FgbOptions {
    /// These options with a bbox filter, `[west, south, east, north]`.
    #[must_use]
    pub fn with_bbox(mut self, bbox: [f64; 4]) -> Self {
        self.bbox = Some(bbox);
        self
    }
}

/// Why a whole file was refused. A bad FEATURE is never this -- see
/// [`FgbReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FgbError {
    /// Not a FlatGeobuf file (the magic bytes are wrong).
    NotFlatGeobuf,
    /// A FlatGeobuf this reader does not read (another major version).
    Unsupported(String),
    /// The header or index does not parse, or is truncated.
    Malformed(String),
    /// The file's CRS is not WGS84; carries a description of it.
    Crs(String),
    /// Larger than [`FgbOptions::max_bytes`].
    TooLarge { limit: u64, got: u64 },
    /// Reading a file failed.
    Io(String),
    /// A URL fetch failed (status, transport, no range support where one
    /// is needed, or no HTTP client compiled into this build).
    Http(String),
    /// A URL fetch passed [`FgbOptions::timeout`].
    Timeout,
    /// The features could not be pushed into the overlay store.
    Push(String),
}

impl std::fmt::Display for FgbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFlatGeobuf => write!(f, "not a FlatGeobuf file"),
            Self::Unsupported(e) => write!(f, "unsupported FlatGeobuf: {e}"),
            Self::Malformed(e) => write!(f, "malformed FlatGeobuf: {e}"),
            Self::Crs(crs) => write!(
                f,
                "the file's CRS is {crs}, not WGS84 (EPSG:4326 / CRS84); reproject it (ogr2ogr -t_srs EPSG:4326), \
                 or set FgbOptions::accept_any_crs to draw its coordinates as longitude/latitude anyway"
            ),
            Self::TooLarge { limit, got } => write!(f, "{got} bytes is over the {limit}-byte limit"),
            Self::Io(e) => write!(f, "reading the FlatGeobuf: {e}"),
            Self::Http(e) => write!(f, "fetching the FlatGeobuf: {e}"),
            Self::Timeout => write!(f, "fetching the FlatGeobuf timed out"),
            Self::Push(e) => write!(f, "pushing the FlatGeobuf: {e}"),
        }
    }
}

impl std::error::Error for FgbError {}

/// The header's geometry type (the spec's `GeometryType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FgbGeometryType {
    /// Mixed: each feature names its own.
    Unknown,
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    GeometryCollection,
    /// A curve, surface, TIN or another type this reader skips.
    Other(u8),
}

impl FgbGeometryType {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Unknown,
            1 => Self::Point,
            2 => Self::LineString,
            3 => Self::Polygon,
            4 => Self::MultiPoint,
            5 => Self::MultiLineString,
            6 => Self::MultiPolygon,
            7 => Self::GeometryCollection,
            other => Self::Other(other),
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Point => 1,
            Self::LineString => 2,
            Self::Polygon => 3,
            Self::MultiPoint => 4,
            Self::MultiLineString => 5,
            Self::MultiPolygon => 6,
            Self::GeometryCollection => 7,
            Self::Other(v) => v,
        }
    }
}

/// A column's declared type (the spec's `ColumnType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FgbColumnType {
    Byte,
    UByte,
    Bool,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    Float,
    Double,
    String,
    Json,
    DateTime,
    Binary,
}

impl FgbColumnType {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Byte,
            1 => Self::UByte,
            2 => Self::Bool,
            3 => Self::Short,
            4 => Self::UShort,
            5 => Self::Int,
            6 => Self::UInt,
            7 => Self::Long,
            8 => Self::ULong,
            9 => Self::Float,
            10 => Self::Double,
            11 => Self::String,
            12 => Self::Json,
            13 => Self::DateTime,
            // 14 is Binary; anything newer is read as bytes and dropped.
            _ => Self::Binary,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        self as u8
    }

    /// The store type this column becomes: `None` for `Binary`, dropped.
    fn values(self) -> Option<CsvValues> {
        match self {
            Self::Bool => Some(CsvValues::Bool(Vec::new())),
            Self::String | Self::Json | Self::DateTime => Some(CsvValues::Str(CsvStrings::default())),
            Self::Binary => None,
            _ => Some(CsvValues::Number(Vec::new())),
        }
    }
}

/// One column of the header's schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FgbColumn {
    pub name: String,
    pub kind: FgbColumnType,
}

/// The header's CRS, as written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FgbCrs {
    /// The authority (`EPSG`, `OGC`), when given.
    pub org: Option<String>,
    /// The authority's numeric code (0 when not numeric).
    pub code: i32,
    /// A human name (`WGS 84`).
    pub name: Option<String>,
    /// The WKT, when given.
    pub wkt: Option<String>,
    /// A non-numeric code (`CRS84`).
    pub code_string: Option<String>,
}

impl FgbCrs {
    fn is_empty(&self) -> bool {
        self.org.is_none() && self.code == 0 && self.code_string.is_none() && self.wkt.is_none()
    }

    /// EPSG:4326 or OGC CRS84.
    #[must_use]
    pub fn is_wgs84(&self) -> bool {
        let org = self.org.as_deref().unwrap_or("EPSG");
        let crs84 = |s: &str| {
            let s = s.trim().to_ascii_uppercase();
            s == "CRS84" || s == "OGC:CRS84" || s.ends_with("/OGC/1.3/CRS84")
        };
        (self.code == 4326 && org.eq_ignore_ascii_case("EPSG"))
            || self.code_string.as_deref().is_some_and(crs84)
            || (self.code == 84 && org.eq_ignore_ascii_case("OGC"))
            || (self.code == 0
                && self.code_string.is_none()
                && self.wkt.as_deref().is_some_and(|w| {
                    let w = w.replace(' ', "");
                    w.ends_with("ID[\"EPSG\",4326]]") || w.ends_with("AUTHORITY[\"EPSG\",\"4326\"]]")
                }))
    }

    /// `EPSG:4269 (NAD83)`.
    #[must_use]
    pub fn describe(&self) -> String {
        let org = self.org.as_deref().unwrap_or("EPSG");
        let code = match (&self.code_string, self.code) {
            (Some(s), _) => format!("{org}:{s}"),
            (None, 0) => "an unnamed CRS".to_string(),
            (None, c) => format!("{org}:{c}"),
        };
        match &self.name {
            Some(name) => format!("{code} ({name})"),
            None => code,
        }
    }
}

/// A file's header: the schema, the CRS, the counts.
#[derive(Debug, Clone, PartialEq)]
pub struct FgbHeader {
    pub name: String,
    /// `[min_x, min_y, max_x, max_y]`, when the writer gave one.
    pub envelope: Option<[f64; 4]>,
    pub geometry_type: FgbGeometryType,
    pub has_z: bool,
    pub has_m: bool,
    pub has_t: bool,
    pub has_tm: bool,
    pub columns: Vec<FgbColumn>,
    /// Features in the file; 0 when the writer did not know (streamed).
    pub features_count: u64,
    /// The R-tree's node size; 0 means the file has no index.
    pub index_node_size: u16,
    pub crs: Option<FgbCrs>,
    pub title: Option<String>,
    pub description: Option<String>,
}

impl FgbHeader {
    /// Whether the file carries a spatial index.
    #[must_use]
    pub fn indexed(&self) -> bool {
        self.index_node_size > 0 && self.features_count > 0
    }

    /// The CRS check: `Ok(warning)` to read, `Err` to refuse.
    fn check_crs(&self, options: &FgbOptions) -> Result<Option<String>, FgbError> {
        match self.crs.as_ref().filter(|c| !c.is_empty()) {
            Some(crs) if crs.is_wgs84() => Ok(None),
            Some(crs) if options.accept_any_crs => {
                Ok(Some(format!("CRS {} drawn as longitude/latitude (accept_any_crs)", crs.describe())))
            }
            Some(crs) => Err(FgbError::Crs(crs.describe())),
            None => {
                let projected = self.envelope.is_some_and(|e| {
                    e.iter().all(|v| v.is_finite()) && (e[0] < -180.5 || e[2] > 180.5 || e[1] < -90.5 || e[3] > 90.5)
                });
                if projected && !options.accept_any_crs {
                    return Err(FgbError::Crs(format!(
                        "not stated, and the envelope {:?} is not longitude/latitude",
                        self.envelope.unwrap_or_default()
                    )));
                }
                Ok(Some("no CRS in the header: coordinates read as WGS84 longitude/latitude".into()))
            }
        }
    }
}

/// The header of a FlatGeobuf file in memory (`bytes` from its start; the
/// header alone is enough): its feature count, columns and CRS, without
/// reading a feature.
pub fn read_header(bytes: &[u8]) -> Result<FgbHeader, FgbError> {
    let len = format::header_len(bytes)?;
    let header = bytes.get(12..12 + len).ok_or_else(|| FgbError::Malformed("header past the end".into()))?;
    format::parse_header(header)
}

/// What a read did.
#[derive(Debug, Clone, PartialEq)]
pub struct FgbReport {
    /// Features in the file by its header (0: the writer did not say).
    pub features_total: u64,
    /// Features read off the file (all of them, or the bbox's hits).
    pub features_read: usize,
    /// Features that became overlay features.
    pub features_loaded: usize,
    /// Skipped features by reason, most frequent first.
    pub skipped_by_reason: Vec<(FgbSkip, usize)>,
    /// The file's extent: the header's envelope, else the loaded
    /// features' box. `[west, south, east, north]`.
    pub bbox: Option<[f64; 4]>,
    pub geometry_type: FgbGeometryType,
    /// The property columns kept, and their declared types.
    pub columns: Vec<(String, FgbColumnType)>,
    /// Whether the file has a spatial index.
    pub indexed: bool,
    /// The CRS as written (`EPSG:4326 (WGS 84)`), or `none`.
    pub crs: String,
    /// Things that loaded but deserve a look (no index, no CRS, thinned).
    pub warnings: Vec<String>,
    /// Whether this source streams by viewport (`MapView::load_source_fgb`
    /// on a large URL).
    pub streaming: bool,
    /// Bytes read (from the file, or over HTTP) and requests made.
    pub bytes_read: u64,
    pub requests: u32,
}

impl std::fmt::Display for FgbReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.features_total > 0 {
            write!(f, "loaded {} of {} features", self.features_loaded, self.features_total)?;
        } else {
            write!(f, "loaded {} features", self.features_loaded)?;
        }
        if self.streaming {
            write!(f, " (streaming by viewport)")?;
        }
        let skipped: usize = self.skipped_by_reason.iter().map(|(_, n)| n).sum();
        if skipped > 0 {
            let reasons: Vec<String> = self.skipped_by_reason.iter().map(|(why, n)| format!("{n} {}", why.describe())).collect();
            write!(f, "; skipped {skipped} ({})", reasons.join(", "))?;
        }
        for warning in &self.warnings {
            write!(f, "; {warning}")?;
        }
        Ok(())
    }
}

impl FgbReport {
    fn new(header: &FgbHeader) -> Self {
        Self {
            features_total: header.features_count,
            features_read: 0,
            features_loaded: 0,
            skipped_by_reason: Vec::new(),
            bbox: header.envelope.filter(|e| e.iter().all(|v| v.is_finite()) && e[0] <= e[2]),
            geometry_type: header.geometry_type,
            columns: header.columns.iter().filter(|c| c.kind != FgbColumnType::Binary).map(|c| (c.name.clone(), c.kind)).collect(),
            indexed: header.indexed(),
            crs: header.crs.as_ref().filter(|c| !c.is_empty()).map_or_else(|| "none".to_string(), FgbCrs::describe),
            warnings: Vec::new(),
            streaming: false,
            bytes_read: 0,
            requests: 0,
        }
    }

    fn skip(&mut self, why: FgbSkip) {
        match self.skipped_by_reason.iter_mut().find(|(w, _)| *w == why) {
            Some((_, n)) => *n += 1,
            None => self.skipped_by_reason.push((why, 1)),
        }
    }

    fn finish(&mut self, rows: &Rows) {
        self.features_loaded = rows.len();
        self.skipped_by_reason.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if self.bbox.is_none() && !rows.is_empty() {
            let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
            for i in 0..rows.len() {
                let g = rows.bbox(i);
                b = [b[0].min(g[0]), b[1].min(g[1]), b[2].max(g[2]), b[3].max(g[3])];
            }
            self.bbox = Some(b);
        }
    }
}

// ---------------------------------------------------------------------------
// Rows: the loaded features, columnar
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Geometries {
    Points(Vec<[f64; 2]>),
    Mixed(Vec<OverlayGeometry>),
}

/// The loaded features: geometry, typed columns, and each feature's
/// position in the file (its index-leaf order), which a streamed source
/// de-duplicates by.
#[derive(Debug, Clone, PartialEq)]
struct Rows {
    ids: Vec<u64>,
    geometry: Geometries,
    /// Header column index -> slot in `columns` (`None`: dropped).
    slots: Vec<Option<usize>>,
    columns: Vec<(String, CsvValues)>,
}

impl Rows {
    fn new(header: &FgbHeader) -> Self {
        let mut slots = Vec::with_capacity(header.columns.len());
        let mut columns = Vec::new();
        for c in &header.columns {
            match c.kind.values() {
                // A repeated name keeps its first column: the store's
                // property table takes each key once.
                Some(values) if !columns.iter().any(|(n, _)| n == &c.name) => {
                    slots.push(Some(columns.len()));
                    columns.push((c.name.clone(), values));
                }
                _ => slots.push(None),
            }
        }
        Self { ids: Vec::new(), geometry: Geometries::Points(Vec::new()), slots, columns }
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    fn push(&mut self, id: u64, geometry: OverlayGeometry, properties: Option<&[u8]>, header: &FgbHeader) {
        match (&mut self.geometry, geometry) {
            (Geometries::Points(points), OverlayGeometry::Point(p)) if p.len() == 1 => points.push([p[0].0, p[0].1]),
            (Geometries::Points(points), other) => {
                let mut all: Vec<OverlayGeometry> =
                    points.iter().map(|p| OverlayGeometry::Point(vec![(p[0], p[1])])).collect();
                all.push(other);
                self.geometry = Geometries::Mixed(all);
            }
            (Geometries::Mixed(all), other) => all.push(other),
        }
        self.ids.push(id);
        let row = self.ids.len();
        if let Some(bytes) = properties {
            let (slots, columns) = (&self.slots, &mut self.columns);
            format::properties(bytes, &header.columns, |col, value| {
                let Some(Some(slot)) = slots.get(col) else { return };
                let values = &mut columns[*slot].1;
                // A column repeated within one feature: the first wins.
                let len = match values {
                    CsvValues::Number(v) => v.len(),
                    CsvValues::Bool(v) => v.len(),
                    CsvValues::Str(s) => s.len(),
                };
                if len >= row {
                    return;
                }
                pad(values, row - 1);
                match (values, value) {
                    (CsvValues::Number(v), Value::Number(n)) => v.push(Some(n)),
                    (CsvValues::Bool(v), Value::Bool(b)) => v.push(Some(b)),
                    (CsvValues::Str(s), Value::Str(t)) => s.push(Some(t)),
                    _ => {}
                }
            });
        }
        for (_, values) in &mut self.columns {
            pad(values, row);
        }
    }

    fn bbox(&self, i: usize) -> [f64; 4] {
        match &self.geometry {
            Geometries::Points(p) => [p[i][0], p[i][1], p[i][0], p[i][1]],
            Geometries::Mixed(g) => format::bbox_of(&g[i]),
        }
    }

    /// Keeps the rows `keep` says to, in order.
    fn retain(&mut self, keep: &[bool]) {
        let mut it = keep.iter();
        self.ids.retain(|_| *it.next().unwrap_or(&true));
        match &mut self.geometry {
            Geometries::Points(p) => {
                let mut it = keep.iter();
                p.retain(|_| *it.next().unwrap_or(&true));
            }
            Geometries::Mixed(g) => {
                let mut it = keep.iter();
                g.retain(|_| *it.next().unwrap_or(&true));
            }
        }
        for (_, values) in &mut self.columns {
            match values {
                CsvValues::Number(v) => {
                    let mut it = keep.iter();
                    v.retain(|_| *it.next().unwrap_or(&true));
                }
                CsvValues::Bool(v) => {
                    let mut it = keep.iter();
                    v.retain(|_| *it.next().unwrap_or(&true));
                }
                CsvValues::Str(s) => {
                    let mut kept = CsvStrings::default();
                    for (i, k) in keep.iter().enumerate().take(s.len()) {
                        if *k {
                            kept.push(s.get(i));
                        }
                    }
                    *s = kept;
                }
            }
        }
    }

    /// Appends `other`'s rows (same schema).
    fn append(&mut self, other: Rows) {
        self.ids.extend(other.ids);
        match (&mut self.geometry, other.geometry) {
            (Geometries::Points(a), Geometries::Points(b)) => a.extend(b),
            (Geometries::Mixed(a), Geometries::Mixed(b)) => a.extend(b),
            (Geometries::Mixed(a), Geometries::Points(b)) => {
                a.extend(b.into_iter().map(|p| OverlayGeometry::Point(vec![(p[0], p[1])])));
            }
            (mine @ Geometries::Points(_), Geometries::Mixed(b)) => {
                let Geometries::Points(a) = std::mem::replace(mine, Geometries::Mixed(Vec::new())) else { unreachable!() };
                let mut all: Vec<OverlayGeometry> = a.into_iter().map(|p| OverlayGeometry::Point(vec![(p[0], p[1])])).collect();
                all.extend(b);
                *mine = Geometries::Mixed(all);
            }
        }
        for ((_, a), (_, b)) in self.columns.iter_mut().zip(other.columns) {
            match (a, b) {
                (CsvValues::Number(a), CsvValues::Number(b)) => a.extend(b),
                (CsvValues::Bool(a), CsvValues::Bool(b)) => a.extend(b),
                (CsvValues::Str(a), CsvValues::Str(b)) => (0..b.len()).for_each(|i| a.push(b.get(i))),
                _ => {}
            }
        }
    }

    fn resident_bytes(&self) -> usize {
        let geometry = match &self.geometry {
            Geometries::Points(p) => p.capacity() * 16,
            Geometries::Mixed(g) => g.iter().map(|g| g.positions().count() * 16 + 32).sum::<usize>(),
        };
        geometry
            + self.ids.capacity() * 8
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
}

/// Pads a column with missing values up to `len` rows.
fn pad(values: &mut CsvValues, len: usize) {
    match values {
        CsvValues::Number(v) => v.resize(len.max(v.len()), None),
        CsvValues::Bool(v) => v.resize(len.max(v.len()), None),
        CsvValues::Str(s) => {
            while s.len() < len {
                s.push(None);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The source
// ---------------------------------------------------------------------------

/// A read FlatGeobuf: geometry, typed property columns, the header and the
/// report. Push it with `MapView::set_source_fgb` (or
/// [`super::OverlayStore::set_source_fgb`] / `ShellOverlays::set_source_fgb`).
#[derive(Debug, Clone, PartialEq)]
pub struct FgbSource {
    rows: Rows,
    header: FgbHeader,
    report: FgbReport,
}

impl FgbSource {
    /// Reads a whole FlatGeobuf held in memory (or, with
    /// [`FgbOptions::bbox`], the features that box hits). Fails only for a
    /// file-level problem; bad features are skipped and reported.
    pub fn from_bytes(bytes: &[u8], options: &FgbOptions) -> Result<Self, FgbError> {
        if bytes.len() as u64 > options.max_bytes {
            return Err(FgbError::TooLarge { limit: options.max_bytes, got: bytes.len() as u64 });
        }
        let mut reader = SliceReader { bytes, read: 0, requests: 0 };
        let mut source = block_ready(read_source(&mut reader, options))?;
        source.report.bytes_read = bytes.len() as u64;
        source.report.requests = 0;
        Ok(source)
    }

    /// Features loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether nothing loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// What the read did.
    #[must_use]
    pub fn report(&self) -> &FgbReport {
        &self.report
    }

    /// The file's header.
    #[must_use]
    pub fn header(&self) -> &FgbHeader {
        &self.header
    }

    /// Every feature's `[lon, lat]`, when every loaded feature is a single
    /// point; `None` otherwise.
    #[must_use]
    pub fn points(&self) -> Option<&[[f64; 2]]> {
        match &self.rows.geometry {
            Geometries::Points(p) => Some(p),
            Geometries::Mixed(_) => None,
        }
    }

    /// Feature `i`'s geometry, as the store holds it.
    #[must_use]
    pub fn geometry(&self, i: usize) -> Option<OverlayGeometry> {
        match &self.rows.geometry {
            Geometries::Points(p) => p.get(i).map(|p| OverlayGeometry::Point(vec![(p[0], p[1])])),
            Geometries::Mixed(g) => g.get(i).cloned(),
        }
    }

    /// Feature `i`'s position in the file (its leaf in the index's order):
    /// stable across reads, so a bbox query's features can be matched to
    /// a whole read's.
    #[must_use]
    pub fn file_index(&self, i: usize) -> Option<u64> {
        self.rows.ids.get(i).copied()
    }

    /// The property columns, in schema order (`Binary` ones dropped):
    /// numbers, bools and strings, as a CSV's are.
    #[must_use]
    pub fn columns(&self) -> &[(String, CsvValues)] {
        &self.rows.columns
    }

    /// One property column by name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&CsvValues> {
        self.rows.columns.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Heap bytes held, for a memory budget.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.rows.resident_bytes()
    }

    /// Calls `f` with the property columns in the store's typed shape.
    pub fn with_property_columns<R>(&self, f: impl FnOnce(&[(&str, PropertyColumn<'_>)]) -> R) -> R {
        let strings: Vec<Vec<Option<&str>>> = self
            .rows
            .columns
            .iter()
            .filter_map(|(_, v)| match v {
                CsvValues::Str(s) => Some((0..s.len()).map(|i| s.get(i)).collect()),
                _ => None,
            })
            .collect();
        let mut strings = strings.iter();
        let columns: Vec<(&str, PropertyColumn<'_>)> = self
            .rows
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

    /// Merges a viewport query into a streamed source: drops the rows
    /// outside `keep` (when over `max`), appends `more`, and thins to
    /// `max`. Returns the file indices dropped.
    pub(crate) fn merge_stream(&mut self, more: FgbSource, keep: [f64; 4], max: usize) -> Vec<u64> {
        let mut dropped = Vec::new();
        if self.rows.len() + more.rows.len() > max {
            let keep_mask: Vec<bool> = (0..self.rows.len()).map(|i| intersects(&self.rows.bbox(i), &keep)).collect();
            dropped.extend(self.rows.ids.iter().zip(&keep_mask).filter(|(_, k)| !**k).map(|(id, _)| *id));
            self.rows.retain(&keep_mask);
        }
        self.rows.append(more.rows);
        if self.rows.len() > max {
            let n = self.rows.len();
            let keep_mask: Vec<bool> = (0..n).map(|i| (i * max) / n != ((i + 1) * max) / n).collect();
            dropped.extend(self.rows.ids.iter().zip(&keep_mask).filter(|(_, k)| !**k).map(|(id, _)| *id));
            self.rows.retain(&keep_mask);
        }
        self.report.features_loaded = self.rows.len();
        self.report.features_read += more.report.features_read;
        self.report.bytes_read += more.report.bytes_read;
        self.report.requests += more.report.requests;
        for (why, n) in more.report.skipped_by_reason {
            for _ in 0..n {
                self.report.skip(why);
            }
        }
        dropped
    }

    /// An empty source with this one's schema (a streamed source's start).
    pub(crate) fn empty_like(header: &FgbHeader) -> Self {
        let mut report = FgbReport::new(header);
        report.streaming = true;
        Self { rows: Rows::new(header), header: header.clone(), report }
    }
}

impl super::OverlayStore {
    /// Replaces source `name` with a read FlatGeobuf.
    ///
    /// All single points: the point path ([`Self::set_source_points`]).
    /// Anything else: the feature path, every feature painted `paint` (a
    /// source paint declared with [`Self::declare_source_paint`] reads the
    /// file's typed columns).
    pub fn set_source_fgb(
        &mut self,
        name: &str,
        fgb: &FgbSource,
        paint: &FeaturePaint,
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        match &fgb.rows.geometry {
            Geometries::Points(points) => {
                fgb.with_property_columns(|columns| self.set_source_points(name, points, None, paint, columns))
            }
            Geometries::Mixed(geometries) => {
                if self.source_cluster_options(name).is_some() {
                    return Err(FeatureError::SourcePaint {
                        source: name.to_string(),
                        reason: "a clustered or point-indexed source takes single-position Point features only".into(),
                    });
                }
                paint.validate().map_err(|reason| FeatureError::Feature { index: 0, id: None, reason })?;
                let properties = fgb
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
// The read core: one async path over any range reader
// ---------------------------------------------------------------------------

/// Bytes by range: a slice, a file, or HTTP `Range` requests. `len ==
/// u64::MAX` reads to the end. A short read means the end of the file.
pub(crate) trait RangeRead {
    async fn read(&mut self, start: u64, len: u64) -> Result<Vec<u8>, FgbError>;
    /// Several `(start, len)` ranges, the bytes in the same order. One by
    /// one here; the HTTP readers override it to keep up to
    /// [`crate::range_fetch::MAX_CONCURRENT_RANGES`] requests in flight.
    async fn read_many(&mut self, ranges: &[(u64, u64)]) -> Result<Vec<Vec<u8>>, FgbError> {
        let mut out = Vec::with_capacity(ranges.len());
        for &(start, len) in ranges {
            out.push(self.read(start, len).await?);
        }
        Ok(out)
    }
    /// The file's length, once known.
    fn file_len(&self) -> Option<u64>;
    /// Bytes read and requests made so far.
    fn counters(&self) -> (u64, u32);
}

/// Bytes already in memory.
pub(crate) struct SliceReader<'a> {
    bytes: &'a [u8],
    read: u64,
    requests: u32,
}

impl RangeRead for SliceReader<'_> {
    async fn read(&mut self, start: u64, len: u64) -> Result<Vec<u8>, FgbError> {
        let start = usize::try_from(start).unwrap_or(usize::MAX).min(self.bytes.len());
        let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(self.bytes.len());
        self.read += (end - start) as u64;
        self.requests += 1;
        Ok(self.bytes[start..end].to_vec())
    }
    fn file_len(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }
    fn counters(&self) -> (u64, u32) {
        (self.read, self.requests)
    }
}

/// Polls a future that never waits (a slice or file reader's) to its end.
pub(crate) fn block_ready<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => unreachable!("a synchronous range reader never waits"),
    }
}

/// Where a file's parts are, from its header.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Layout {
    pub(crate) header: FgbHeader,
    index_start: u64,
    features_start: u64,
    crs_warning: Option<String>,
}

/// The first read: enough for most headers in one request.
const HEADER_PREFETCH: u64 = 16 << 10;

async fn open<R: RangeRead>(r: &mut R, options: &FgbOptions) -> Result<Layout, FgbError> {
    let mut prefix = r.read(0, HEADER_PREFETCH).await?;
    let header_len = format::header_len(&prefix)? as u64;
    let header_end = 12 + header_len;
    if (prefix.len() as u64) < header_end {
        let rest = r.read(prefix.len() as u64, header_end - prefix.len() as u64).await?;
        prefix.extend(rest);
        if (prefix.len() as u64) < header_end {
            return Err(FgbError::Malformed("truncated header".into()));
        }
    }
    let header = format::parse_header(&prefix[12..header_end as usize])?;
    let crs_warning = header.check_crs(options)?;
    let index = if header.indexed() { format::index_bytes(header.features_count, header.index_node_size) } else { 0 };
    Ok(Layout { header, index_start: header_end, features_start: header_end + index, crs_warning })
}

/// One index hit: the feature's position in the file and its byte range
/// relative to the features section (`end` unknown for the file's last).
#[derive(Debug, Clone, Copy)]
struct Hit {
    index: u64,
    offset: u64,
    end: Option<u64>,
}

/// Merges sorted ranges whose gap is under `gap`.
fn merge_ranges(mut ranges: Vec<(u64, u64)>, gap: u64) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match out.last_mut() {
            Some(last) if a <= last.1.saturating_add(gap) => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Two ranges closer than this are fetched as one: a round trip costs
/// more than 64 KiB on any link worth streaming over.
const MERGE_GAP: u64 = 64 << 10;

/// Walks the index level by level, reading only the node groups the box
/// reaches (adjacent groups merged into one read per level).
async fn search<R: RangeRead>(r: &mut R, layout: &Layout, bbox: &[f64; 4]) -> Result<Vec<Hit>, FgbError> {
    let h = &layout.header;
    let node_size = u64::from(h.index_node_size.max(2));
    let levels = format::level_bounds(h.features_count, h.index_node_size);
    let leaves = levels[0].clone();
    let mut frontier: Vec<u64> = vec![0];
    let mut hits = Vec::new();
    for level in (0..levels.len()).rev() {
        let bounds = &levels[level];
        // Each group: first node .. end (a leaf group reads one node more,
        // for the next feature's offset, which ends this one's bytes).
        let groups: Vec<(u64, u64)> = frontier
            .iter()
            .filter(|&&n| bounds.contains(&n))
            .map(|&n| {
                let end = (n + node_size).min(bounds.end);
                (n, if level == 0 { (end + 1).min(bounds.end) } else { end })
            })
            .collect();
        let mut next = Vec::new();
        let gap_nodes = MERGE_GAP / NODE_BYTES;
        // `groups` is sorted (the frontier is), so a cursor walks it once.
        let mut cursor = 0;
        for (a, b) in merge_ranges(groups.clone(), gap_nodes) {
            let bytes = r.read(layout.index_start + a * NODE_BYTES, (b - a) * NODE_BYTES).await?;
            if (bytes.len() as u64) < (b - a) * NODE_BYTES {
                return Err(FgbError::Malformed("truncated index".into()));
            }
            let first = cursor;
            while cursor < groups.len() && groups[cursor].0 < b {
                cursor += 1;
            }
            for &(g0, g1) in &groups[first..cursor] {
                let g_end = if level == 0 { (g0 + node_size).min(bounds.end) } else { g1 };
                for pos in g0..g_end {
                    let node = node_at(&bytes, ((pos - a) * NODE_BYTES) as usize).ok_or_else(|| FgbError::Malformed("index node".into()))?;
                    if !intersects(&node.bbox, bbox) {
                        continue;
                    }
                    if level == 0 {
                        let end = (pos + 1 < leaves.end)
                            .then(|| node_at(&bytes, ((pos + 1 - a) * NODE_BYTES) as usize).map(|n| n.offset))
                            .flatten();
                        hits.push(Hit { index: pos - leaves.start, offset: node.offset, end });
                    } else {
                        next.push(node.offset);
                    }
                }
            }
        }
        next.sort_unstable();
        next.dedup();
        frontier = next;
    }
    hits.sort_unstable_by_key(|h| h.offset);
    Ok(hits)
}

/// Reads the hits' features into `rows`.
async fn read_hits<R: RangeRead>(
    r: &mut R,
    layout: &Layout,
    hits: &[Hit],
    rows: &mut Rows,
    report: &mut FgbReport,
    max_bytes: u64,
) -> Result<(), FgbError> {
    let file_end = r.file_len().map(|len| len.saturating_sub(layout.features_start));
    let ranges: Vec<(u64, u64)> = hits
        .iter()
        // The last feature: to the file's end, or its size prefix's word.
        // A corrupt index's end before its start reads just the prefix.
        .map(|h| (h.offset, h.end.or(file_end).unwrap_or(u64::MAX).max(h.offset.saturating_add(4))))
        .collect();
    // `hits` is sorted by offset, so a cursor walks it once.
    let mut cursor = 0;
    let merged = merge_ranges(ranges, MERGE_GAP);
    if let Some(&(a, b)) = merged.iter().find(|&&(a, b)| b != u64::MAX && b - a > max_bytes) {
        return Err(FgbError::TooLarge { limit: max_bytes, got: b - a });
    }
    // A handful of requests at a time, concurrently by an HTTP reader
    // (`RangeRead::read_many`), each batch decoded and released before
    // the next is fetched.
    for batch in merged.chunks(crate::range_fetch::MAX_CONCURRENT_RANGES) {
        let spans: Vec<(u64, u64)> =
            batch.iter().map(|&(a, b)| (layout.features_start + a, if b == u64::MAX { u64::MAX } else { b - a })).collect();
        let bodies = r.read_many(&spans).await?;
        for (&(a, b), bytes) in batch.iter().zip(bodies) {
            let first = cursor;
            while cursor < hits.len() && hits[cursor].offset < b {
                cursor += 1;
            }
            for hit in &hits[first..cursor] {
                let at = (hit.offset - a) as usize;
                read_feature(&bytes, at, hit.index, layout, rows, report);
            }
        }
    }
    Ok(())
}

/// Decodes the size-prefixed feature at `bytes[at..]` into `rows`; returns
/// the byte after it, or `None` when it does not fit.
fn read_feature(bytes: &[u8], at: usize, index: u64, layout: &Layout, rows: &mut Rows, report: &mut FgbReport) -> Option<usize> {
    report.features_read += 1;
    let Some(size) = u32_at(bytes, at) else {
        report.skip(FgbSkip::Malformed);
        return None;
    };
    let end = at.checked_add(4)?.checked_add(size as usize)?;
    let Some(buf) = bytes.get(at + 4..end) else {
        report.skip(FgbSkip::Malformed);
        return None;
    };
    match format::parse_feature(buf, layout.header.geometry_type) {
        Ok(feature) => rows.push(index, feature.geometry, feature.properties, &layout.header),
        Err(why) => report.skip(why),
    }
    Some(end)
}

/// Every feature in file order, from `bytes` (the features section),
/// keeping those whose box meets `bbox` when one is given.
fn scan(bytes: &[u8], layout: &Layout, bbox: Option<&[f64; 4]>, rows: &mut Rows, report: &mut FgbReport) {
    let mut at = 0usize;
    let mut index = 0u64;
    while at + 4 <= bytes.len() {
        let before = rows.len();
        let Some(next) = read_feature(bytes, at, index, layout, rows, report) else { break };
        if let Some(bbox) = bbox {
            if rows.len() > before && !intersects(&rows.bbox(before), bbox) {
                let mut keep = vec![true; rows.len()];
                keep[before] = false;
                rows.retain(&keep);
                report.features_read -= 1;
            }
        }
        at = next;
        index += 1;
    }
}

/// Opens, then reads the whole file or the bbox's features.
async fn read_source<R: RangeRead>(r: &mut R, options: &FgbOptions) -> Result<FgbSource, FgbError> {
    let layout = open(r, options).await?;
    let mut rows = Rows::new(&layout.header);
    let mut report = FgbReport::new(&layout.header);
    report.warnings.extend(layout.crs_warning.clone());
    read_into(r, &layout, options.bbox.as_ref(), options.max_bytes, &mut rows, &mut report).await?;
    report.finish(&rows);
    let (bytes_read, requests) = r.counters();
    report.bytes_read = bytes_read;
    report.requests = requests;
    Ok(FgbSource { rows, header: layout.header, report })
}

async fn read_into<R: RangeRead>(
    r: &mut R,
    layout: &Layout,
    bbox: Option<&[f64; 4]>,
    max_bytes: u64,
    rows: &mut Rows,
    report: &mut FgbReport,
) -> Result<(), FgbError> {
    match bbox {
        Some(bbox) if layout.header.indexed() => {
            let hits = search(r, layout, bbox).await?;
            read_hits(r, layout, &hits, rows, report, max_bytes).await
        }
        _ => {
            if !layout.header.indexed() {
                report.warnings.push("no spatial index: the whole file was scanned".into());
            }
            let len = r.file_len().map_or(u64::MAX, |len| len.saturating_sub(layout.features_start));
            if len != u64::MAX && len > max_bytes {
                return Err(FgbError::TooLarge { limit: max_bytes, got: len });
            }
            let bytes = r.read(layout.features_start, len).await?;
            scan(&bytes, layout, bbox, rows, report);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// A remote file held open for viewport queries
// ---------------------------------------------------------------------------

/// What one viewport query did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FgbQueryStats {
    /// Features the index returned for the box.
    pub hits: usize,
    /// Of those, already loaded (not fetched again).
    pub already_loaded: usize,
    /// Features fetched and decoded.
    pub fetched: usize,
    /// HTTP requests and bytes for this query.
    pub requests: u32,
    pub bytes: u64,
}

/// A FlatGeobuf over HTTP, opened once (header + index layout) and
/// queried by bbox: the index levels the box reaches, then its features,
/// by `Range` requests. `MapView::load_source_fgb` holds one per streamed
/// source and queries it on each camera settle.
pub struct FgbRemote {
    reader: HttpReader,
    layout: Layout,
    max_bytes: u64,
}

impl std::fmt::Debug for FgbRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FgbRemote").field("header", &self.layout.header).field("file_len", &self.reader.file_len()).finish()
    }
}

impl FgbRemote {
    /// The header.
    #[must_use]
    pub fn header(&self) -> &FgbHeader {
        &self.layout.header
    }

    /// The file's size in bytes (from the first range response).
    #[must_use]
    pub fn file_len(&self) -> Option<u64> {
        self.reader.file_len()
    }

    /// Bytes read and requests made since opening.
    #[must_use]
    pub fn counters(&self) -> (u64, u32) {
        self.reader.counters()
    }

    /// The CRS warning the open produced, if any.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.layout.crs_warning.iter().cloned().collect()
    }

    async fn query_async(&mut self, bbox: [f64; 4], loaded: &HashSet<u64>) -> Result<(FgbSource, FgbQueryStats), FgbError> {
        let (bytes0, requests0) = self.reader.counters();
        let mut rows = Rows::new(&self.layout.header);
        let mut report = FgbReport::new(&self.layout.header);
        let mut stats = FgbQueryStats::default();
        if self.layout.header.indexed() {
            let hits = search(&mut self.reader, &self.layout, &bbox).await?;
            stats.hits = hits.len();
            let fresh: Vec<Hit> = hits.into_iter().filter(|h| !loaded.contains(&h.index)).collect();
            stats.already_loaded = stats.hits - fresh.len();
            read_hits(&mut self.reader, &self.layout, &fresh, &mut rows, &mut report, self.max_bytes).await?;
        } else {
            read_into(&mut self.reader, &self.layout, Some(&bbox), self.max_bytes, &mut rows, &mut report).await?;
            stats.hits = rows.len();
            let keep: Vec<bool> = rows.ids.iter().map(|id| !loaded.contains(id)).collect();
            rows.retain(&keep);
            stats.already_loaded = stats.hits - rows.len();
        }
        stats.fetched = rows.len();
        let (bytes1, requests1) = self.reader.counters();
        stats.bytes = bytes1 - bytes0;
        stats.requests = requests1 - requests0;
        report.bytes_read = stats.bytes;
        report.requests = stats.requests;
        report.finish(&rows);
        Ok((FgbSource { rows, header: self.layout.header.clone(), report }, stats))
    }
}

pub(crate) use platform::HttpReader;
/// Drives an async range read to its end on the fetch runtime (blocking),
/// for `overlay::geoparquet`'s native reads over the same client.
#[cfg(all(not(target_arch = "wasm32"), feature = "geoparquet"))]
pub(crate) use fetch_native::run as run_blocking;

#[cfg(not(target_arch = "wasm32"))]
mod platform {
    pub(crate) use super::fetch_native::HttpReader;
}
#[cfg(target_arch = "wasm32")]
mod platform {
    pub(crate) use super::fetch_web::HttpReader;
}

/// The viewport of `camera` as `[west, south, east, north]` degrees,
/// grown by `grow` of its size on each side and clamped to the world.
#[must_use]
pub fn viewport_bbox(camera: &crate::render::Camera, grow: f64) -> [f64; 4] {
    let (min, max) = super::host::point_viewport_box(camera, grow);
    let lon = |x: f64| (x * 360.0 - 180.0).clamp(-180.0, 180.0);
    let lat = |y: f64| {
        let y = y.clamp(0.0, 1.0);
        (std::f64::consts::PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees()
    };
    if max[0] - min[0] >= 1.0 {
        return [-180.0, lat(max[1]), 180.0, lat(min[1])];
    }
    [lon(min[0]), lat(max[1]), lon(max[0]), lat(min[1])]
}

