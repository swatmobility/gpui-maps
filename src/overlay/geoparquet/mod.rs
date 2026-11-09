//! GeoParquet as an overlay SOURCE: Parquet with `geo` metadata, read into
//! the same overlay store a GeoJSON, CSV or FlatGeobuf source is. There is
//! no GeoParquet renderer: a file of points goes down the point path
//! ([`super::OverlayStore::set_source_points`]) and a file of lines or
//! polygons down the tiled feature path.
//!
//! # Reading
//!
//! - [`GeoParquetSource::from_bytes`] reads an in-memory file (every
//!   target), [`GeoParquetSource::from_path`] a local file by seek, and
//!   [`GeoParquetSource::from_url`] an http(s) one by `Range` requests
//!   (native: blocking, call off the UI thread or let
//!   `MapView::load_source_geoparquet` do it; the browser build compiles an
//!   async `from_url` over `fetch`).
//! - Only the footer, then the column chunks of the geometry column and
//!   the properties asked for ([`GeoParquetOptions::columns`]; `None` is
//!   every column the store can hold) are read: projection pushdown.
//! - [`GeoParquetOptions::bbox`] skips whole ROW GROUPS whose extent misses
//!   the box, by the first of: the 1.1 bbox covering column's row-group
//!   statistics; a GeoArrow-native geometry's `x`/`y` statistics; Parquet's
//!   own geospatial statistics on the geometry column. Rows inside a read
//!   group are then filtered by the covering column (before their geometry
//!   is decoded) or by their geometry's box. [`GeoParquetReport`] counts
//!   both: row groups read and skipped, rows read, filtered and loaded.
//! - [`GeoParquetRemote`] holds a remote file open (its footer) for repeated
//!   bbox queries: `MapView::load_source_geoparquet` streams a large URL by
//!   viewport through it ([`GeoParquetOptions::stream_above_bytes`]).
//!
//! The bytes move through this crate's own readers (the FlatGeobuf
//! source's HTTP client, User-Agent and timeout; the browser's `fetch`);
//! the `parquet` crate's sans-IO push decoder decodes them, so no second
//! HTTP stack or async runtime is linked.
//!
//! # What is read
//!
//! - GeoParquet 1.0 and 1.1 (`geo` metadata `version` 1.0.x / 1.1.x); any
//!   other version is refused by number.
//! - Geometry: WKB (ISO and EWKB; Point, LineString, Polygon and their
//!   Multi forms; Z/M dropped) and the 1.1 GeoArrow native encodings
//!   (`point` ... `multipolygon`, separated `struct<x, y>` coordinates, or
//!   interleaved). A GeometryCollection is skipped and counted.
//! - Properties: every integer, float and decimal column is a number,
//!   dates and timestamps are numbers (milliseconds since the epoch),
//!   `Boolean` a bool, `Utf8` (any width, views, dictionary-encoded) a
//!   string. Binary and nested columns (structs, lists, maps) are dropped
//!   and named in [`GeoParquetReport::dropped_columns`].
//! - Codecs: uncompressed, snappy, gzip and lz4 everywhere; zstd natively
//!   (it is C). The browser build refuses a zstd file by codec name
//!   ([`GeoParquetError::Unsupported`]); brotli is refused everywhere.
//! - CRS: EPSG:4326 / OGC:CRS84 (or none: CRS84 by the spec). Anything
//!   else is refused ([`GeoParquetError::Crs`]) unless
//!   [`GeoParquetOptions::accept_any_crs`].
//!
//! # Example
//!
//! ```
//! use gpui_maps::overlay::geoparquet::{write, GeoParquetOptions, GeoParquetSource};
//! use gpui_maps::overlay::OverlayGeometry;
//!
//! let features: Vec<write::WriteFeature> = (0..100)
//!     .map(|i| write::WriteFeature {
//!         geometry: OverlayGeometry::Point(vec![(f64::from(i) - 50.0, 10.0)]),
//!         properties: vec![write::WriteValue::Number(Some(f64::from(i)))],
//!     })
//!     .collect();
//! let bytes = write::write(&[("value", write::WriteType::Number)], &features, &write::WriteOptions::default()).unwrap();
//!
//! let all = GeoParquetSource::from_bytes(&bytes, &GeoParquetOptions::default()).unwrap();
//! assert_eq!(all.len(), 100);
//! let some = GeoParquetSource::from_bytes(&bytes, &GeoParquetOptions::default().with_bbox([-10.0, 0.0, 9.5, 20.0])).unwrap();
//! assert_eq!(some.len(), 20); // longitudes -10..=9
//! ```

mod decode;
mod geo;
pub mod write;

/// A Parquet file's footer, and a plain (non-geo) Parquet file's columns.
pub mod plain;
/// Arrow IPC (GeoArrow) and GeoJSON into GeoParquet. Native only.
#[cfg(not(target_arch = "wasm32"))]
pub mod convert;

#[cfg(not(target_arch = "wasm32"))]
mod fetch_native;
#[cfg(target_arch = "wasm32")]
mod fetch_web;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use arrow_array::cast::AsArray;
use arrow_array::{Array, RecordBatch};
use bytes::Bytes;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
use parquet::arrow::push_decoder::ParquetPushDecoderBuilder;
use parquet::arrow::ProjectionMask;
use parquet::basic::Compression;
use parquet::errors::ParquetError;
use parquet::file::metadata::{PageIndexPolicy, ParquetMetaData, ParquetMetaDataPushDecoder};
use parquet::file::statistics::Statistics;
use parquet::DecodeResult;

pub use geo::{GeoParquetCovering, GeoParquetEncoding, GeoParquetMetadata};

use decode::{Decoded, Geometry};
use super::columns::{PropertyColumn, PropertyTable};
use super::csv::{CsvColumnType, CsvStrings, CsvValues};
use super::fgb::{intersects, FgbError, FgbOptions, HttpReader, RangeRead};
use crate::range_fetch::{coalesce, ByteRange};
use super::geojson::OverlayGeometry;
use super::paint::FeaturePaint;
use super::{FeatureError, OverlayFeature};

/// The User-Agent a GeoParquet fetch sends (the FlatGeobuf reader's: one
/// client for both).
pub const GEOPARQUET_USER_AGENT: &str = super::fgb::FGB_USER_AGENT;

/// [`GeoParquetOptions::max_bytes`]' default: 512 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 512 << 20;

/// [`GeoParquetOptions::stream_above_bytes`]' default: 8 MiB, the
/// FlatGeobuf source's threshold. Parquet compresses better, so the same
/// size is more rows (roughly 150k-500k points); a whole-file read of it
/// is still one to three seconds on a 25-50 Mbit/s link.
pub const DEFAULT_STREAM_ABOVE_BYTES: u64 = 8 << 20;

/// [`GeoParquetOptions::stream_max_features`]' default (the FlatGeobuf
/// source's, for its reason).
pub const DEFAULT_STREAM_MAX_FEATURES: usize = 250_000;

/// [`GeoParquetOptions::timeout`]'s default: one request, body included.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The footer read: the whole metadata of most files in one request.
const TAIL_PREFETCH: u64 = 64 << 10;

/// Row groups whose column chunks add up to this are fetched together
/// (ranges merged across them), then decoded, then released.
const WINDOW_BYTES: u64 = 32 << 20;

/// Two ranges closer than this are fetched as one (the FlatGeobuf
/// reader's gap).
const MERGE_GAP: u64 = 64 << 10;

/// A merged request is not grown past this, so a window of adjacent
/// chunks still fans out into requests that run concurrently
/// ([`crate::range_fetch::MAX_CONCURRENT_RANGES`] at once). The COG
/// reader's cap.
const MAX_REQUEST: u64 = 8 << 20;

/// Rows per decoded Arrow batch.
const BATCH_ROWS: usize = 8192;

/// How a GeoParquet file is read.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoParquetOptions {
    /// The property columns to read, by name. `None` reads every column the
    /// store can hold (fine for a small file; name the ones a layer paints
    /// by for a wide one, such as Overture's). The geometry column (and a
    /// bbox covering column, for a bbox read) is always read.
    pub columns: Option<Vec<String>>,
    /// Load only rows whose boxes intersect `[west, south, east, north]`
    /// (degrees), reading only the row groups that can hold them. `None`
    /// loads everything.
    pub bbox: Option<[f64; 4]>,
    /// The most bytes one read (a load, or one viewport query) fetches.
    pub max_bytes: u64,
    /// One HTTP request's deadline.
    pub timeout: Duration,
    /// Draw a file whose CRS is not WGS84 as longitude/latitude anyway
    /// (for a geographic datum such as NAD83 or ETRS89). Off: refused.
    pub accept_any_crs: bool,
    /// `MapView::load_source_geoparquet` with a URL: a file larger than
    /// this is streamed by viewport rather than read whole. See
    /// [`DEFAULT_STREAM_ABOVE_BYTES`].
    pub stream_above_bytes: u64,
    /// A streamed source's resident bound: the most rows held at once.
    /// Rows leaving the view are evicted first, then the rest are thinned
    /// evenly.
    pub stream_max_features: usize,
}

impl Default for GeoParquetOptions {
    fn default() -> Self {
        Self {
            columns: None,
            bbox: None,
            max_bytes: DEFAULT_MAX_BYTES,
            timeout: DEFAULT_TIMEOUT,
            accept_any_crs: false,
            stream_above_bytes: DEFAULT_STREAM_ABOVE_BYTES,
            stream_max_features: DEFAULT_STREAM_MAX_FEATURES,
        }
    }
}

impl GeoParquetOptions {
    /// These options with a bbox filter, `[west, south, east, north]`.
    #[must_use]
    pub fn with_bbox(mut self, bbox: [f64; 4]) -> Self {
        self.bbox = Some(bbox);
        self
    }

    /// These options reading only the named property columns.
    #[must_use]
    pub fn with_columns<S: Into<String>>(mut self, columns: impl IntoIterator<Item = S>) -> Self {
        self.columns = Some(columns.into_iter().map(Into::into).collect());
        self
    }

    /// The FlatGeobuf options its HTTP reader takes.
    fn http(&self) -> FgbOptions {
        FgbOptions { max_bytes: self.max_bytes, timeout: self.timeout, ..FgbOptions::default() }
    }
}

/// Why a whole file was refused. A bad ROW is never this -- see
/// [`GeoParquetReport::skipped_by_reason`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeoParquetError {
    /// Not a Parquet file (no `PAR1` magic).
    NotParquet,
    /// A Parquet file without usable `geo` metadata; says what is missing.
    NotGeoParquet(String),
    /// A GeoParquet this build does not read: another version, a geometry
    /// encoding it does not know, or a compression codec not compiled in.
    Unsupported(String),
    /// The footer or a column chunk does not parse, or is truncated.
    Malformed(String),
    /// The geometry column's CRS is not WGS84; carries a description.
    Crs(String),
    /// A read would fetch more than [`GeoParquetOptions::max_bytes`].
    TooLarge { limit: u64, got: u64 },
    /// Reading a file failed.
    Io(String),
    /// A URL fetch failed (status, transport, or no HTTP client compiled
    /// into this build).
    Http(String),
    /// A URL fetch passed [`GeoParquetOptions::timeout`].
    Timeout,
    /// The rows could not be pushed into the overlay store.
    Push(String),
}

impl std::fmt::Display for GeoParquetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotParquet => write!(f, "not a Parquet file"),
            Self::NotGeoParquet(e) => write!(f, "not GeoParquet: {e}"),
            Self::Unsupported(e) => write!(f, "unsupported GeoParquet: {e}"),
            Self::Malformed(e) => write!(f, "malformed Parquet: {e}"),
            Self::Crs(crs) => write!(
                f,
                "the geometry's CRS is {crs}, not WGS84 (EPSG:4326 / OGC:CRS84); reproject it, \
                 or set GeoParquetOptions::accept_any_crs to draw its coordinates as longitude/latitude anyway"
            ),
            Self::TooLarge { limit, got } => write!(f, "{got} bytes is over the {limit}-byte limit"),
            Self::Io(e) => write!(f, "reading the GeoParquet: {e}"),
            Self::Http(e) => write!(f, "fetching the GeoParquet: {e}"),
            Self::Timeout => write!(f, "fetching the GeoParquet timed out"),
            Self::Push(e) => write!(f, "pushing the GeoParquet: {e}"),
        }
    }
}

impl std::error::Error for GeoParquetError {}

impl From<FgbError> for GeoParquetError {
    /// The shared HTTP reader's errors.
    fn from(e: FgbError) -> Self {
        match e {
            FgbError::TooLarge { limit, got } => Self::TooLarge { limit, got },
            FgbError::Io(e) => Self::Io(e),
            FgbError::Http(e) => Self::Http(e),
            FgbError::Timeout => Self::Timeout,
            other => Self::Malformed(other.to_string()),
        }
    }
}

/// A `parquet` decode error.
fn pq(e: ParquetError) -> GeoParquetError {
    match e {
        ParquetError::NYI(e) => GeoParquetError::Unsupported(e),
        ParquetError::General(e) if e.contains("Disabled feature") => GeoParquetError::Unsupported(e),
        other => GeoParquetError::Malformed(other.to_string()),
    }
}

/// Why one row was not loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GeoParquetSkip {
    /// The geometry's bytes do not parse.
    Malformed,
    /// A null or empty geometry.
    NoGeometry,
    /// A type with no overlay shape (a GeometryCollection, a curve).
    UnsupportedGeometry,
    /// A coordinate outside -180..180 / -90..90.
    OutOfRange,
}

impl GeoParquetSkip {
    /// A short phrase for a report line.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Malformed => "malformed geometry",
            Self::NoGeometry => "no geometry",
            Self::UnsupportedGeometry => "unsupported geometry type",
            Self::OutOfRange => "coordinate out of range",
        }
    }
}

/// Which statistics told row groups apart for bbox pushdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoParquetPushdown {
    /// The 1.1 bbox covering column's row-group min/max.
    Covering,
    /// A GeoArrow-native geometry's `x`/`y` min/max.
    GeometryStats,
    /// Parquet's geospatial statistics (a `GEOMETRY` logical column).
    GeospatialStats,
    /// No row-group extents: every row group is read and filtered.
    None,
}

/// What a read did.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoParquetReport {
    /// The file's GeoParquet version (`1.1.0`).
    pub version: String,
    /// The primary geometry column and its encoding.
    pub geometry_column: String,
    pub encoding: GeoParquetEncoding,
    /// The writer's declared geometry types (empty: any).
    pub geometry_types: Vec<String>,
    /// The CRS as written (`OGC:CRS84 (WGS 84 (CRS84))`).
    pub crs: String,
    /// Rows and row groups in the file.
    pub rows_total: u64,
    pub row_groups_total: usize,
    /// Row groups read, and skipped by their statistics (a bbox read).
    pub row_groups_read: usize,
    pub row_groups_skipped: usize,
    /// What the row-group skipping used.
    pub pushdown: GeoParquetPushdown,
    /// Rows decoded from the groups read.
    pub rows_read: u64,
    /// Of those, rows outside the bbox (dropped).
    pub rows_filtered: u64,
    /// Rows that became overlay features.
    pub rows_loaded: usize,
    /// Rows passed over undecoded because a streamed source already holds
    /// them.
    pub rows_already_loaded: u64,
    /// Rows skipped for their geometry, most frequent first.
    pub skipped_by_reason: Vec<(GeoParquetSkip, usize)>,
    /// The property columns kept, and their store types.
    pub columns: Vec<(String, CsvColumnType)>,
    /// Columns not kept: their types have no store shape (binary, nested),
    /// or they were named in [`GeoParquetOptions::columns`] and are absent.
    pub dropped_columns: Vec<String>,
    /// The extent: the metadata's `bbox`, else the loaded rows' box.
    pub bbox: Option<[f64; 4]>,
    /// Things that loaded but deserve a look.
    pub warnings: Vec<String>,
    /// Whether this source streams by viewport.
    pub streaming: bool,
    /// Bytes read (from the file, or over HTTP) and requests made.
    pub bytes_read: u64,
    pub requests: u32,
}

impl std::fmt::Display for GeoParquetReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "loaded {} of {} rows", self.rows_loaded, self.rows_total)?;
        write!(f, "; {} of {} row groups read", self.row_groups_read, self.row_groups_total)?;
        if self.row_groups_skipped > 0 {
            write!(f, " ({} skipped)", self.row_groups_skipped)?;
        }
        if self.rows_filtered > 0 {
            write!(f, "; {} rows outside the box", self.rows_filtered)?;
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

impl GeoParquetReport {
    fn new(layout: &Layout) -> Self {
        Self {
            version: layout.geo.version.clone(),
            geometry_column: layout.geo.primary_column.clone(),
            encoding: layout.geo.encoding,
            geometry_types: layout.geo.geometry_types.clone(),
            crs: layout.crs.clone(),
            rows_total: layout.group_rows.iter().sum(),
            row_groups_total: layout.group_rows.len(),
            row_groups_read: 0,
            row_groups_skipped: 0,
            pushdown: layout.pushdown,
            rows_read: 0,
            rows_filtered: 0,
            rows_loaded: 0,
            rows_already_loaded: 0,
            skipped_by_reason: Vec::new(),
            columns: layout.props.iter().map(|p| (p.name.clone(), p.kind)).collect(),
            dropped_columns: layout.dropped.clone(),
            bbox: layout.geo.bbox.filter(|e| e.iter().all(|v| v.is_finite()) && e[0] <= e[2]),
            warnings: layout.warnings.clone(),
            streaming: false,
            bytes_read: 0,
            requests: 0,
        }
    }

    fn skip(&mut self, why: GeoParquetSkip, n: usize) {
        match self.skipped_by_reason.iter_mut().find(|(w, _)| *w == why) {
            Some((_, m)) => *m += n,
            None => self.skipped_by_reason.push((why, n)),
        }
    }

    fn finish(&mut self, rows: &Rows) {
        self.rows_loaded = rows.len();
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

fn point(p: [f64; 2]) -> OverlayGeometry {
    OverlayGeometry::Point(vec![(p[0], p[1])])
}

/// The loaded rows: geometry, typed columns, and each row's position in
/// the file (row-group start + offset), which a streamed source
/// de-duplicates by.
#[derive(Debug, Clone, PartialEq)]
struct Rows {
    ids: Vec<u64>,
    geometry: Geometries,
    columns: Vec<(String, CsvValues)>,
}

/// Keeps `v`'s items where `keep` says to.
fn retain_by<T>(v: &mut Vec<T>, keep: &[bool]) {
    let mut it = keep.iter();
    v.retain(|_| *it.next().unwrap_or(&true));
}

impl Rows {
    fn new(props: &[Prop]) -> Self {
        let columns = props
            .iter()
            .map(|p| {
                let values = match p.kind {
                    CsvColumnType::Number => CsvValues::Number(Vec::new()),
                    CsvColumnType::Bool => CsvValues::Bool(Vec::new()),
                    CsvColumnType::String => CsvValues::Str(CsvStrings::default()),
                };
                (p.name.clone(), values)
            })
            .collect();
        Self { ids: Vec::new(), geometry: Geometries::Points(Vec::new()), columns }
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    fn push_geometry(&mut self, id: u64, geometry: Decoded) {
        match (&mut self.geometry, geometry) {
            (Geometries::Points(points), Decoded::Point(p)) => points.push(p),
            (Geometries::Points(points), Decoded::Shape(other)) => {
                let mut all: Vec<OverlayGeometry> = points.iter().map(|p| point(*p)).collect();
                all.push(other);
                self.geometry = Geometries::Mixed(all);
            }
            (Geometries::Mixed(all), Decoded::Point(p)) => all.push(point(p)),
            (Geometries::Mixed(all), Decoded::Shape(other)) => all.push(other),
        }
        self.ids.push(id);
    }

    fn bbox(&self, i: usize) -> [f64; 4] {
        match &self.geometry {
            Geometries::Points(p) => [p[i][0], p[i][1], p[i][0], p[i][1]],
            Geometries::Mixed(g) => super::fgb::bbox_of(&g[i]),
        }
    }

    fn retain(&mut self, keep: &[bool]) {
        retain_by(&mut self.ids, keep);
        match &mut self.geometry {
            Geometries::Points(p) => retain_by(p, keep),
            Geometries::Mixed(g) => retain_by(g, keep),
        }
        for (_, values) in &mut self.columns {
            match values {
                CsvValues::Number(v) => retain_by(v, keep),
                CsvValues::Bool(v) => retain_by(v, keep),
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

    fn append(&mut self, other: Rows) {
        self.ids.extend(other.ids);
        match (&mut self.geometry, other.geometry) {
            (Geometries::Points(a), Geometries::Points(b)) => a.extend(b),
            (Geometries::Mixed(a), Geometries::Mixed(b)) => a.extend(b),
            (Geometries::Mixed(a), Geometries::Points(b)) => a.extend(b.into_iter().map(point)),
            (mine @ Geometries::Points(_), Geometries::Mixed(b)) => {
                let Geometries::Points(a) = std::mem::replace(mine, Geometries::Mixed(Vec::new())) else { unreachable!() };
                let mut all: Vec<OverlayGeometry> = a.into_iter().map(point).collect();
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

// ---------------------------------------------------------------------------
// The source
// ---------------------------------------------------------------------------

/// A read GeoParquet file: geometry, typed property columns, the `geo`
/// metadata and the report. Push it with `MapView::set_source_geoparquet`
/// (or [`super::OverlayStore::set_source_geoparquet`] /
/// `ShellOverlays::set_source_geoparquet`).
#[derive(Debug, Clone, PartialEq)]
pub struct GeoParquetSource {
    rows: Rows,
    metadata: GeoParquetMetadata,
    report: GeoParquetReport,
}

impl GeoParquetSource {
    /// Reads a GeoParquet file held in memory (with
    /// [`GeoParquetOptions::bbox`], only the row groups and rows that box
    /// reaches). Fails only for a file-level problem; bad rows are skipped
    /// and reported.
    pub fn from_bytes(bytes: &[u8], options: &GeoParquetOptions) -> Result<Self, GeoParquetError> {
        if bytes.len() as u64 > options.max_bytes {
            return Err(GeoParquetError::TooLarge { limit: options.max_bytes, got: bytes.len() as u64 });
        }
        let mut reader = SliceReader { bytes, read: 0, requests: 0 };
        let mut source = super::fgb::block_ready(read_source(&mut reader, options))?;
        source.report.requests = 0;
        Ok(source)
    }

    /// Rows loaded.
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
    pub fn report(&self) -> &GeoParquetReport {
        &self.report
    }

    /// The file's `geo` metadata.
    #[must_use]
    pub fn metadata(&self) -> &GeoParquetMetadata {
        &self.metadata
    }

    /// Every row's `[lon, lat]`, when every loaded row is a single point.
    #[must_use]
    pub fn points(&self) -> Option<&[[f64; 2]]> {
        match &self.rows.geometry {
            Geometries::Points(p) => Some(p),
            Geometries::Mixed(_) => None,
        }
    }

    /// Row `i`'s geometry, as the store holds it.
    #[must_use]
    pub fn geometry(&self, i: usize) -> Option<OverlayGeometry> {
        match &self.rows.geometry {
            Geometries::Points(p) => p.get(i).map(|p| point(*p)),
            Geometries::Mixed(g) => g.get(i).cloned(),
        }
    }

    /// Row `i`'s position in the file (0-based across row groups): stable
    /// across reads, so a bbox read's rows can be matched to a whole read's.
    #[must_use]
    pub fn file_row(&self, i: usize) -> Option<u64> {
        self.rows.ids.get(i).copied()
    }

    /// The property columns, in file order.
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
    /// `max`. Returns the file rows dropped.
    pub(crate) fn merge_stream(&mut self, more: GeoParquetSource, keep: [f64; 4], max: usize) -> Vec<u64> {
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
        let r = &mut self.report;
        r.rows_loaded = self.rows.len();
        r.rows_read += more.report.rows_read;
        r.rows_filtered += more.report.rows_filtered;
        r.rows_already_loaded += more.report.rows_already_loaded;
        r.row_groups_read += more.report.row_groups_read;
        r.bytes_read += more.report.bytes_read;
        r.requests += more.report.requests;
        for (why, n) in more.report.skipped_by_reason {
            r.skip(why, n);
        }
        dropped
    }

    /// An empty source with `remote`'s schema (a streamed source's start).
    pub(crate) fn empty_like(remote: &GeoParquetRemote) -> Self {
        let mut report = GeoParquetReport::new(&remote.layout);
        report.streaming = true;
        Self { rows: Rows::new(&remote.layout.props), metadata: remote.layout.geo.clone(), report }
    }
}

impl super::OverlayStore {
    /// Replaces source `name` with a read GeoParquet file.
    ///
    /// All single points: the point path ([`Self::set_source_points`]).
    /// Anything else: the feature path, every feature painted `paint` (a
    /// source paint declared with [`Self::declare_source_paint`] reads the
    /// file's typed columns).
    pub fn set_source_geoparquet(
        &mut self,
        name: &str,
        source: &GeoParquetSource,
        paint: &FeaturePaint,
        display_zoom: u8,
    ) -> Result<(), FeatureError> {
        match &source.rows.geometry {
            Geometries::Points(points) => {
                source.with_property_columns(|columns| self.set_source_points(name, points, None, paint, columns))
            }
            Geometries::Mixed(geometries) => {
                if self.source_cluster_options(name).is_some() {
                    return Err(FeatureError::SourcePaint {
                        source: name.to_string(),
                        reason: "a clustered or point-indexed source takes single-position Point features only".into(),
                    });
                }
                paint.validate().map_err(|reason| FeatureError::Feature { index: 0, id: None, reason })?;
                let properties = source
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

/// Bytes already in memory.
struct SliceReader<'a> {
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

/// One property column to read.
#[derive(Debug, Clone, PartialEq)]
struct Prop {
    /// Its root index in the Arrow schema.
    root: usize,
    /// For a struct field (`names.primary`): the field, read alone.
    child: Option<String>,
    /// The property name: the column's, or `root.field`.
    name: String,
    kind: CsvColumnType,
}

/// An open file: its footer, `geo` metadata, row-group extents and the
/// columns a read projects.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    metadata: Arc<ParquetMetaData>,
    arrow: ArrowReaderMetadata,
    file_len: u64,
    geo: GeoParquetMetadata,
    geom_root: usize,
    covering_root: Option<usize>,
    /// Each row group's extent (`None`: no statistics; always read).
    group_boxes: Vec<Option<[f64; 4]>>,
    /// Each row group's first row and row count.
    group_starts: Vec<u64>,
    group_rows: Vec<u64>,
    pushdown: GeoParquetPushdown,
    props: Vec<Prop>,
    dropped: Vec<String>,
    crs: String,
    warnings: Vec<String>,
}

/// A row group statistic as `(min, max)` doubles.
fn min_max(s: &Statistics) -> Option<(f64, f64)> {
    match s {
        Statistics::Double(v) => Some((*v.min_opt()?, *v.max_opt()?)),
        Statistics::Float(v) => Some((f64::from(*v.min_opt()?), f64::from(*v.max_opt()?))),
        _ => None,
    }
}

/// Reads the footer and works out what a read needs.
async fn open<R: RangeRead>(r: &mut R, options: &GeoParquetOptions) -> Result<Layout, GeoParquetError> {
    let magic = r.read(0, 4).await?;
    if magic != b"PAR1" {
        return Err(GeoParquetError::NotParquet);
    }
    let file_len = r.file_len().ok_or_else(|| GeoParquetError::Http("the server did not say how long the file is".into()))?;
    if file_len < 12 {
        return Err(GeoParquetError::NotParquet);
    }
    let mut footer = ParquetMetaDataPushDecoder::try_new(file_len).map_err(pq)?.with_page_index_policy(PageIndexPolicy::Skip);
    let tail_start = file_len.saturating_sub(TAIL_PREFETCH);
    let tail = r.read(tail_start, file_len - tail_start).await?;
    footer.push_range(tail_start..file_len, Bytes::from(tail)).map_err(pq)?;
    let metadata = loop {
        match footer.try_decode().map_err(pq)? {
            DecodeResult::NeedsData(ranges) => {
                for range in ranges {
                    let bytes = r.read(range.start, range.end - range.start).await?;
                    footer.push_range(range, Bytes::from(bytes)).map_err(pq)?;
                }
            }
            DecodeResult::Data(metadata) => break metadata,
            DecodeResult::Finished => return Err(GeoParquetError::Malformed("no footer".into())),
        }
    };
    let json = metadata
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|k| k.key == "geo"))
        .and_then(|k| k.value.clone())
        .ok_or_else(|| GeoParquetError::NotGeoParquet("no `geo` key in the file metadata (a plain Parquet file)".into()))?;
    let geo = GeoParquetMetadata::parse(&json)?;
    let (crs, crs_warning) = geo.check_crs(options.accept_any_crs)?;
    let metadata = Arc::new(metadata);
    let arrow = ArrowReaderMetadata::try_new(Arc::clone(&metadata), ArrowReaderOptions::new()).map_err(pq)?;
    let schema = arrow.schema().clone();
    let mut warnings: Vec<String> = crs_warning.into_iter().collect();

    let geom_root = schema
        .index_of(&geo.primary_column)
        .map_err(|_| GeoParquetError::NotGeoParquet(format!("the primary column {:?} is not in the file", geo.primary_column)))?;
    let geom_type = schema.field(geom_root).data_type();
    if Geometry::of(arrow_array::new_empty_array(geom_type).as_ref(), geo.encoding).is_none() {
        return Err(GeoParquetError::Unsupported(format!(
            "the geometry column {:?} is {geom_type}, which is not a {} encoding this reader reads",
            geo.primary_column,
            geo.encoding.name()
        )));
    }

    // The covering column, when it is the struct of floats it says it is.
    let covering_root = geo.covering.as_ref().and_then(|c| {
        let root = schema.index_of(&c.column).ok();
        let ok = root.is_some_and(|root| {
            let empty = arrow_array::new_empty_array(schema.field(root).data_type());
            empty.as_struct_opt().is_some_and(|s| c.fields.iter().all(|f| s.column_by_name(f).is_some_and(|a| decode::floats(a.as_ref()).is_some())))
        });
        if !ok {
            warnings.push(format!("the bbox covering column {:?} is missing or not a struct of floats: ignored", c.column));
        }
        root.filter(|_| ok)
    });

    // Row-group extents, by the first statistics that give them.
    let schema_descr = metadata.file_metadata().schema_descr();
    let leaf = |path: &[&str]| (0..schema_descr.num_columns()).find(|&i| schema_descr.column(i).path().parts() == path);
    let groups = metadata.row_groups();
    let stat = |g: usize, leaf: usize| groups[g].column(leaf).statistics().and_then(min_max);
    let covering_leaves: Option<[usize; 4]> = covering_root.and(geo.covering.as_ref()).and_then(|c| {
        let l = |f: &str| leaf(&[c.column.as_str(), f]);
        Some([l(&c.fields[0])?, l(&c.fields[1])?, l(&c.fields[2])?, l(&c.fields[3])?])
    });
    let native_leaves: Option<[usize; 2]> = (geo.encoding != GeoParquetEncoding::Wkb).then(|| {
        let find = |axis: &str| {
            (0..schema_descr.num_columns()).find(|&i| {
                let column = schema_descr.column(i);
                let p = column.path().parts();
                p.first() == Some(&geo.primary_column) && p.last().map(String::as_str) == Some(axis)
            })
        };
        Some([find("x")?, find("y")?])
    }).flatten();
    let geom_leaf = (0..schema_descr.num_columns()).find(|&i| schema_descr.get_column_root_idx(i) == geom_root);
    let by_covering = |g: usize| {
        let [x0, y0, x1, y1] = covering_leaves?;
        Some([stat(g, x0)?.0, stat(g, y0)?.0, stat(g, x1)?.1, stat(g, y1)?.1])
    };
    let by_native = |g: usize| {
        let [x, y] = native_leaves?;
        let (x, y) = (stat(g, x)?, stat(g, y)?);
        Some([x.0, y.0, x.1, y.1])
    };
    let by_geospatial = |g: usize| {
        let b = groups[g].column(geom_leaf?).geo_statistics()?.bounding_box()?.clone();
        let (x0, x1) = (b.get_xmin(), b.get_xmax());
        // xmin > xmax: the box wraps the antimeridian.
        let (x0, x1) = if x0 > x1 { (-180.0, 180.0) } else { (x0, x1) };
        Some([x0, b.get_ymin(), x1, b.get_ymax()])
    };
    let n = groups.len();
    let all = |f: &dyn Fn(usize) -> Option<[f64; 4]>| -> Option<Vec<Option<[f64; 4]>>> {
        let boxes: Vec<Option<[f64; 4]>> = (0..n).map(f).collect();
        (n > 0 && boxes.iter().all(Option::is_some)).then_some(boxes)
    };
    let (pushdown, group_boxes) = if let Some(b) = all(&by_covering) {
        (GeoParquetPushdown::Covering, b)
    } else if let Some(b) = all(&by_native) {
        (GeoParquetPushdown::GeometryStats, b)
    } else if let Some(b) = all(&by_geospatial) {
        (GeoParquetPushdown::GeospatialStats, b)
    } else {
        (GeoParquetPushdown::None, vec![None; n])
    };
    let group_rows: Vec<u64> = groups.iter().map(|g| u64::try_from(g.num_rows()).unwrap_or(0)).collect();
    let group_starts: Vec<u64> = group_rows.iter().scan(0u64, |at, &n| { let start = *at; *at += n; Some(start) }).collect();

    // The property columns.
    let mut props = Vec::new();
    let mut dropped = Vec::new();
    let skip = |i: usize| i == geom_root || Some(i) == covering_root;
    match &options.columns {
        None => {
            for (i, field) in schema.fields().iter().enumerate() {
                if skip(i) {
                    continue;
                }
                match decode::kind_of(field.data_type()) {
                    Some(kind) => props.push(Prop { root: i, child: None, name: field.name().clone(), kind }),
                    None => dropped.push(field.name().clone()),
                }
            }
        }
        Some(names) => {
            for name in names {
                if props.iter().any(|p: &Prop| &p.name == name) {
                    continue;
                }
                // `root.field`: one scalar field of a struct column (Overture's
                // `names.primary`), when no column has the dotted name.
                let nested = name.split_once('.').filter(|_| schema.index_of(name).is_err()).and_then(|(root, field)| {
                    let i = schema.index_of(root).ok().filter(|&i| !skip(i))?;
                    let arrow_schema::DataType::Struct(fields) = schema.field(i).data_type() else { return None };
                    let f = fields.iter().find(|f| f.name() == field)?;
                    Some((i, field.to_string(), decode::kind_of(f.data_type())))
                });
                if let Some((i, field, kind)) = nested {
                    match kind {
                        Some(kind) => props.push(Prop { root: i, child: Some(field), name: name.clone(), kind }),
                        None => dropped.push(name.clone()),
                    }
                    continue;
                }
                match schema.index_of(name) {
                    Ok(i) if !skip(i) => match decode::kind_of(schema.field(i).data_type()) {
                        Some(kind) => props.push(Prop { root: i, child: None, name: name.clone(), kind }),
                        None => dropped.push(name.clone()),
                    },
                    Ok(_) => {}
                    Err(_) => {
                        warnings.push(format!("no column {name:?} in the file"));
                        dropped.push(name.clone());
                    }
                }
            }
        }
    }

    Ok(Layout {
        metadata,
        arrow,
        file_len,
        geo,
        geom_root,
        covering_root,
        group_boxes,
        group_starts,
        group_rows,
        pushdown,
        props,
        dropped,
        crs,
        warnings,
    })
}

impl Layout {
    /// The row groups `bbox` can reach (all of them for `None`).
    fn groups_for(&self, bbox: Option<&[f64; 4]>) -> Vec<usize> {
        (0..self.group_boxes.len())
            .filter(|&g| match (bbox, self.group_boxes[g]) {
                (Some(b), Some(extent)) => intersects(&extent, b),
                _ => true,
            })
            .collect()
    }

    /// The Parquet leaf columns a read projects: every leaf of the
    /// geometry, of the covering column (for a bbox read) and of each whole
    /// property column; just the one field of a `root.field` property.
    fn leaves(&self, bbox: bool) -> Vec<usize> {
        let schema = self.metadata.file_metadata().schema_descr();
        let mut roots = vec![self.geom_root];
        if bbox {
            roots.extend(self.covering_root);
        }
        roots.extend(self.props.iter().filter(|p| p.child.is_none()).map(|p| p.root));
        (0..schema.num_columns())
            .filter(|&l| {
                let root = schema.get_column_root_idx(l);
                let column = schema.column(l);
                let field = column.path().parts().get(1).map(String::as_str);
                roots.contains(&root) || self.props.iter().any(|p| p.root == root && p.child.is_some() && p.child.as_deref() == field)
            })
            .collect()
    }
}

/// `spans` (`(start, len)`), fetched together -- concurrently by an HTTP
/// reader -- and counted against `max_bytes` before any is requested.
async fn fetch_spans<R: RangeRead>(
    r: &mut R,
    spans: &[(u64, u64)],
    fetched: &mut u64,
    max_bytes: u64,
) -> Result<Vec<Bytes>, GeoParquetError> {
    *fetched += spans.iter().map(|&(_, len)| len).sum::<u64>();
    if *fetched > max_bytes {
        return Err(GeoParquetError::TooLarge { limit: max_bytes, got: *fetched });
    }
    let bodies = r.read_many(spans).await?;
    spans
        .iter()
        .zip(bodies)
        .map(|(&(_, len), bytes)| {
            if (bytes.len() as u64) < len {
                return Err(GeoParquetError::Malformed("truncated column chunk".into()));
            }
            Ok(Bytes::from(bytes))
        })
        .collect()
}

/// The codecs this build decodes.
fn codec_supported(c: Compression) -> bool {
    let pure_rust =
        matches!(c, Compression::UNCOMPRESSED | Compression::SNAPPY | Compression::GZIP(_) | Compression::LZ4 | Compression::LZ4_RAW);
    pure_rust || (matches!(c, Compression::ZSTD(_)) && cfg!(not(target_arch = "wasm32")))
}

/// Reads row groups `groups` (ascending) into `rows`: their projected
/// column chunks, fetched a window of groups at a time with nearby ranges
/// merged, decoded, and filtered by `bbox`. Rows in `skip` are not loaded
/// (a streamed source's residents).
async fn read_groups<R: RangeRead>(
    r: &mut R,
    layout: &Layout,
    groups: &[usize],
    bbox: Option<&[f64; 4]>,
    skip: Option<&HashSet<u64>>,
    max_bytes: u64,
    rows: &mut Rows,
    report: &mut GeoParquetReport,
) -> Result<(), GeoParquetError> {
    report.row_groups_read += groups.len();
    if groups.is_empty() {
        return Ok(());
    }
    let schema_descr = layout.metadata.file_metadata().schema_descr();
    let leaves = layout.leaves(bbox.is_some());
    for &g in groups {
        for &l in &leaves {
            let codec = layout.metadata.row_group(g).column(l).compression();
            if !codec_supported(codec) {
                return Err(GeoParquetError::Unsupported(format!(
                    "{codec} compression (column {}) is not compiled into this build{}",
                    schema_descr.column(l).path(),
                    if cfg!(target_arch = "wasm32") { "; the browser build reads uncompressed, snappy, gzip and lz4" } else { "" }
                )));
            }
        }
    }
    let mut decoder = ParquetPushDecoderBuilder::new_with_metadata(layout.arrow.clone())
        .with_projection(ProjectionMask::leaves(schema_descr, leaves.iter().copied()))
        .with_row_groups(groups.to_vec())
        .with_batch_size(BATCH_ROWS)
        .build()
        .map_err(pq)?;
    let mut fetched = 0u64;
    let mut i = 0;
    while i < groups.len() {
        // A window of groups, its chunks' ranges coalesced (within and
        // across the groups) and fetched together.
        let mut j = i;
        let mut window = 0u64;
        let mut wanted = Vec::new();
        while j < groups.len() && (j == i || window < WINDOW_BYTES) {
            for &l in &leaves {
                let (start, len) = layout.metadata.row_group(groups[j]).column(l).byte_range();
                wanted.push(ByteRange { key: (groups[j], l), start, len });
                window += len;
            }
            j += 1;
        }
        let spans: Vec<(u64, u64)> = coalesce(&wanted, MERGE_GAP, MAX_REQUEST).iter().map(|g| (g.start, g.len)).collect();
        let bodies = fetch_spans(r, &spans, &mut fetched, max_bytes).await?;
        let starts = spans.iter().map(|&(start, len)| start..start + len).collect();
        decoder.push_ranges(starts, bodies).map_err(pq)?;
        for &g in &groups[i..j] {
            loop {
                match decoder.try_next_reader().map_err(pq)? {
                    DecodeResult::NeedsData(needed) => {
                        let spans: Vec<(u64, u64)> = needed.iter().map(|range| (range.start, range.end - range.start)).collect();
                        let got = fetch_spans(r, &spans, &mut fetched, max_bytes).await?;
                        decoder.push_ranges(needed, got).map_err(pq)?;
                    }
                    DecodeResult::Data(reader) => {
                        let mut at = layout.group_starts[g];
                        for batch in reader {
                            let batch = batch.map_err(|e| GeoParquetError::Malformed(e.to_string()))?;
                            let n = batch.num_rows() as u64;
                            decode_batch(&batch, at, layout, bbox, skip, rows, report)?;
                            at += n;
                        }
                        break;
                    }
                    DecodeResult::Finished => break,
                }
            }
        }
        decoder.clear_all_ranges();
        i = j;
    }
    Ok(())
}

/// One batch's rows into `rows`: the bbox filter (by the covering column
/// before decoding, else by the decoded geometry's box), the geometry,
/// then the kept rows' properties column by column.
fn decode_batch(
    batch: &RecordBatch,
    first_row: u64,
    layout: &Layout,
    bbox: Option<&[f64; 4]>,
    skip: Option<&HashSet<u64>>,
    rows: &mut Rows,
    report: &mut GeoParquetReport,
) -> Result<(), GeoParquetError> {
    let geo = &layout.geo;
    let geom_array = batch
        .column_by_name(&geo.primary_column)
        .ok_or_else(|| GeoParquetError::Malformed("the geometry column did not decode".into()))?;
    let geometry = Geometry::of(geom_array.as_ref(), geo.encoding)
        .ok_or_else(|| GeoParquetError::Unsupported(format!("geometry column type {}", geom_array.data_type())))?;
    let covering = match (bbox, &geo.covering) {
        (Some(_), Some(c)) => batch.column_by_name(&c.column).and_then(|a| a.as_struct_opt()).and_then(|s| {
            let f = |name: &str| s.column_by_name(name).and_then(|a| decode::floats(a.as_ref()));
            Some([f(&c.fields[0])?, f(&c.fields[1])?, f(&c.fields[2])?, f(&c.fields[3])?])
        }),
        _ => None,
    };
    let n = batch.num_rows();
    report.rows_read += n as u64;
    let mut keep: Vec<usize> = Vec::with_capacity(n);
    let mut filtered = 0u64;
    for i in 0..n {
        let id = first_row + i as u64;
        if skip.is_some_and(|s| s.contains(&id)) {
            report.rows_already_loaded += 1;
            continue;
        }
        let row_box = covering.as_ref().and_then(|c| Some([c[0](i)?, c[1](i)?, c[2](i)?, c[3](i)?]));
        if let (Some(b), Some(row_box)) = (bbox, row_box) {
            if !intersects(&row_box, b) {
                filtered += 1;
                continue;
            }
        }
        match geometry.at(i) {
            Ok(g) => {
                if let (Some(b), None) = (bbox, row_box) {
                    if !intersects(&g.bbox(), b) {
                        filtered += 1;
                        continue;
                    }
                }
                rows.push_geometry(id, g);
                keep.push(i);
            }
            Err(why) => report.skip(why, 1),
        }
    }
    report.rows_filtered += filtered;
    for (slot, prop) in layout.props.iter().enumerate() {
        let schema = layout.arrow.schema();
        let array = batch.column_by_name(schema.field(prop.root).name());
        let array = match &prop.child {
            Some(field) => array.and_then(|a| a.as_struct_opt()).and_then(|s| s.column_by_name(field)),
            None => array,
        };
        let values = &mut rows.columns[slot].1;
        match values {
            CsvValues::Number(v) => match array.and_then(|a| decode::numbers(a.as_ref())) {
                Some(get) => v.extend(keep.iter().map(|&i| get(i).filter(|x| x.is_finite()))),
                None => v.resize(v.len() + keep.len(), None),
            },
            CsvValues::Bool(v) => match array.and_then(|a| decode::bools(a.as_ref())) {
                Some(get) => v.extend(keep.iter().map(|&i| get(i))),
                None => v.resize(v.len() + keep.len(), None),
            },
            CsvValues::Str(s) => match array.and_then(|a| decode::strings(a.as_ref())) {
                Some(get) => keep.iter().for_each(|&i| s.push(get(i))),
                None => keep.iter().for_each(|_| s.push(None)),
            },
        }
    }
    Ok(())
}

/// Opens, then reads the whole file or the bbox's row groups.
async fn read_source<R: RangeRead>(r: &mut R, options: &GeoParquetOptions) -> Result<GeoParquetSource, GeoParquetError> {
    let layout = open(r, options).await?;
    read_with(r, &layout, options.bbox, options.max_bytes).await
}

/// A read of an open file.
async fn read_with<R: RangeRead>(
    r: &mut R,
    layout: &Layout,
    bbox: Option<[f64; 4]>,
    max_bytes: u64,
) -> Result<GeoParquetSource, GeoParquetError> {
    let (bytes0, requests0) = r.counters();
    let mut rows = Rows::new(&layout.props);
    let mut report = GeoParquetReport::new(layout);
    let groups = layout.groups_for(bbox.as_ref());
    report.row_groups_skipped = layout.group_boxes.len() - groups.len();
    if bbox.is_some() && layout.pushdown == GeoParquetPushdown::None && layout.group_boxes.len() > 1 {
        report.warnings.push("no row-group statistics for the geometry: every row group was read and filtered".into());
    }
    read_groups(r, layout, &groups, bbox.as_ref(), None, max_bytes, &mut rows, &mut report).await?;
    report.finish(&rows);
    let (bytes1, requests1) = r.counters();
    report.bytes_read = bytes1 - bytes0;
    report.requests = requests1 - requests0;
    Ok(GeoParquetSource { rows, metadata: layout.geo.clone(), report })
}

// ---------------------------------------------------------------------------
// A remote file held open for viewport queries
// ---------------------------------------------------------------------------

/// What one viewport query did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GeoParquetQueryStats {
    /// Row groups whose extent meets the box.
    pub row_groups_hit: usize,
    /// Row groups whose extent misses it (not read).
    pub row_groups_skipped: usize,
    /// Of the hits, groups every row of which is already held (not read).
    pub row_groups_resident: usize,
    /// Rows of the row groups read that were already held: passed over
    /// before decoding, not loaded again.
    pub already_loaded: usize,
    /// Rows decoded and loaded.
    pub fetched: usize,
    /// HTTP requests and bytes for this query.
    pub requests: u32,
    pub bytes: u64,
}

/// A GeoParquet file over HTTP, opened once (its footer) and queried by
/// bbox: the row groups the box reaches, by `Range` requests for their
/// projected column chunks. `MapView::load_source_geoparquet` holds one per
/// streamed source and queries it on each camera settle.
pub struct GeoParquetRemote {
    reader: HttpReader,
    layout: Layout,
    max_bytes: u64,
}

impl std::fmt::Debug for GeoParquetRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeoParquetRemote")
            .field("metadata", &self.layout.geo)
            .field("file_len", &self.layout.file_len)
            .field("row_groups", &self.layout.group_rows.len())
            .finish()
    }
}

impl GeoParquetRemote {
    /// The file's `geo` metadata.
    #[must_use]
    pub fn metadata(&self) -> &GeoParquetMetadata {
        &self.layout.geo
    }

    /// The file's size in bytes.
    #[must_use]
    pub fn file_len(&self) -> u64 {
        self.layout.file_len
    }

    /// Rows in the file.
    #[must_use]
    pub fn rows_total(&self) -> u64 {
        self.layout.group_rows.iter().sum()
    }

    /// The file's date and timestamp columns from its schema, `true` for a
    /// calendar date: [`plain::ParquetPeek::temporal`] for a remote file,
    /// which the read turns into epoch milliseconds and so forgets.
    #[must_use]
    pub fn temporal(&self) -> Vec<(String, bool)> {
        plain::temporal_of(self.layout.arrow.schema())
    }

    /// Row groups in the file.
    #[must_use]
    pub fn row_groups(&self) -> usize {
        self.layout.group_rows.len()
    }

    /// Whether the row groups have extents a viewport query can skip by.
    #[must_use]
    pub fn pushdown(&self) -> GeoParquetPushdown {
        self.layout.pushdown
    }

    /// Bytes read and requests made since opening.
    #[must_use]
    pub fn counters(&self) -> (u64, u32) {
        self.reader.counters()
    }

    /// The warnings the open produced (CRS, columns).
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.layout.warnings.clone()
    }

    async fn query_async(
        &mut self,
        bbox: [f64; 4],
        loaded: &HashSet<u64>,
    ) -> Result<(GeoParquetSource, GeoParquetQueryStats), GeoParquetError> {
        let (bytes0, requests0) = self.reader.counters();
        let layout = &self.layout;
        let mut rows = Rows::new(&layout.props);
        let mut report = GeoParquetReport::new(layout);
        let hit = layout.groups_for(Some(&bbox));
        let mut stats = GeoParquetQueryStats {
            row_groups_hit: hit.len(),
            row_groups_skipped: layout.group_boxes.len() - hit.len(),
            ..GeoParquetQueryStats::default()
        };
        // A group every row of which is held: nothing to fetch.
        let fresh: Vec<usize> = hit
            .into_iter()
            .filter(|&g| {
                let start = layout.group_starts[g];
                !(start..start + layout.group_rows[g]).all(|id| loaded.contains(&id))
            })
            .collect();
        stats.row_groups_resident = stats.row_groups_hit - fresh.len();
        report.row_groups_skipped = stats.row_groups_skipped + stats.row_groups_resident;
        read_groups(&mut self.reader, layout, &fresh, Some(&bbox), Some(loaded), self.max_bytes, &mut rows, &mut report).await?;
        stats.fetched = rows.len();
        stats.already_loaded = usize::try_from(report.rows_already_loaded).unwrap_or(usize::MAX);
        let (bytes1, requests1) = self.reader.counters();
        stats.bytes = bytes1 - bytes0;
        stats.requests = requests1 - requests0;
        report.bytes_read = stats.bytes;
        report.requests = stats.requests;
        report.finish(&rows);
        Ok((GeoParquetSource { rows, metadata: layout.geo.clone(), report }, stats))
    }
}

/// The viewport of `camera` as `[west, south, east, north]` degrees, grown
/// by `grow` of its size on each side (the FlatGeobuf source's).
#[must_use]
pub fn viewport_bbox(camera: &crate::render::Camera, grow: f64) -> [f64; 4] {
    super::fgb::viewport_bbox(camera, grow)
}
