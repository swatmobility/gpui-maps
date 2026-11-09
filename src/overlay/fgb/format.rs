//! The FlatGeobuf wire format, read straight off the bytes: the
//! flatbuffer tables the header and each feature are, the geometry and
//! property encodings, and the packed Hilbert R-tree's layout.
//!
//! No flatbuffers runtime: the two schemas
//! (<https://github.com/flatgeobuf/flatgeobuf/tree/master/src/fbs>) are a
//! dozen fields, and every read here is bounds-checked, so a truncated or
//! hostile file is an error, never a panic or an out-of-bounds read.

use super::{FgbColumn, FgbColumnType, FgbCrs, FgbError, FgbGeometryType, FgbHeader};
use crate::overlay::geojson::OverlayGeometry;

/// The eight magic bytes: `fgb`, the major version (3), `fgb`, a patch
/// byte. The reader checks the first three and the major version.
pub(crate) const MAGIC: [u8; 8] = [b'f', b'g', b'b', 3, b'f', b'g', b'b', 1];

/// One R-tree node on disk: a box of four `f64` and a `u64` offset.
pub(crate) const NODE_BYTES: u64 = 40;

fn u16_at(b: &[u8], p: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(p..p.checked_add(2)?)?.try_into().ok()?))
}
pub(crate) fn u32_at(b: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(p..p.checked_add(4)?)?.try_into().ok()?))
}
fn i32_at(b: &[u8], p: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(p..p.checked_add(4)?)?.try_into().ok()?))
}
pub(crate) fn u64_at(b: &[u8], p: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(p..p.checked_add(8)?)?.try_into().ok()?))
}
pub(crate) fn f64_at(b: &[u8], p: usize) -> Option<f64> {
    Some(f64::from_le_bytes(b.get(p..p.checked_add(8)?)?.try_into().ok()?))
}

/// One flatbuffer table: the buffer and where the table starts.
#[derive(Clone, Copy)]
pub(crate) struct Table<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Table<'a> {
    /// The root table of a flatbuffer that starts at `buf[0]`.
    pub(crate) fn root(buf: &'a [u8]) -> Option<Self> {
        let pos = u32_at(buf, 0)? as usize;
        (pos < buf.len()).then_some(Self { buf, pos })
    }

    fn field(&self, id: usize) -> Option<usize> {
        let vtable = (self.pos as i64).checked_sub(i64::from(i32_at(self.buf, self.pos)?))?;
        let vtable = usize::try_from(vtable).ok()?;
        let vtable_len = usize::from(u16_at(self.buf, vtable)?);
        let entry = 4 + 2 * id;
        if entry + 2 > vtable_len {
            return None;
        }
        let at = usize::from(u16_at(self.buf, vtable + entry)?);
        (at != 0).then(|| self.pos + at)
    }

    fn u8(&self, id: usize, default: u8) -> u8 {
        self.field(id).and_then(|p| self.buf.get(p).copied()).unwrap_or(default)
    }
    fn bool(&self, id: usize, default: bool) -> bool {
        self.u8(id, u8::from(default)) != 0
    }
    fn u16(&self, id: usize, default: u16) -> u16 {
        self.field(id).and_then(|p| u16_at(self.buf, p)).unwrap_or(default)
    }
    fn i32(&self, id: usize, default: i32) -> i32 {
        self.field(id).and_then(|p| i32_at(self.buf, p)).unwrap_or(default)
    }
    fn u64(&self, id: usize, default: u64) -> u64 {
        self.field(id).and_then(|p| u64_at(self.buf, p)).unwrap_or(default)
    }

    /// Where an offset field points.
    fn target(&self, id: usize) -> Option<usize> {
        let p = self.field(id)?;
        p.checked_add(u32_at(self.buf, p)? as usize)
    }

    fn str(&self, id: usize) -> Option<&'a str> {
        let bytes = self.vector(id, 1)?;
        std::str::from_utf8(bytes).ok()
    }

    /// A vector's elements as raw bytes, `elem` bytes each.
    fn vector(&self, id: usize, elem: usize) -> Option<&'a [u8]> {
        let p = self.target(id)?;
        let n = u32_at(self.buf, p)? as usize;
        self.buf.get(p + 4..(p + 4).checked_add(n.checked_mul(elem)?)?)
    }

    fn table(&self, id: usize) -> Option<Table<'a>> {
        let pos = self.target(id)?;
        (pos < self.buf.len()).then_some(Table { buf: self.buf, pos })
    }

    /// A vector of tables.
    fn tables(&self, id: usize) -> Option<Vec<Table<'a>>> {
        let p = self.target(id)?;
        let n = u32_at(self.buf, p)? as usize;
        let mut out = Vec::with_capacity(n.min(4096));
        for i in 0..n {
            let at = p + 4 + 4 * i;
            let pos = at.checked_add(u32_at(self.buf, at)? as usize)?;
            if pos >= self.buf.len() {
                return None;
            }
            out.push(Table { buf: self.buf, pos });
        }
        Some(out)
    }
}

fn malformed(what: &str) -> FgbError {
    FgbError::Malformed(what.to_string())
}

/// Reads the magic and the header's length: the header is
/// `bytes[12..12 + len]`. Needs the first 12 bytes.
pub(crate) fn header_len(prefix: &[u8]) -> Result<usize, FgbError> {
    if prefix.len() < 12 || prefix[0..3] != MAGIC[0..3] || prefix[4..7] != MAGIC[4..7] {
        return Err(FgbError::NotFlatGeobuf);
    }
    if prefix[3] != 3 {
        return Err(FgbError::Unsupported(format!("FlatGeobuf major version {} (this reader reads 3)", prefix[3])));
    }
    let len = u32_at(prefix, 8).ok_or_else(|| malformed("header length"))? as usize;
    // The reference implementation's own bound: a header is names, a
    // schema and a CRS, never megabytes.
    if !(4..=10 << 20).contains(&len) {
        return Err(malformed("header length out of range"));
    }
    Ok(len)
}

/// Parses the header flatbuffer (`bytes[12..12 + header_len]`).
pub(crate) fn parse_header(buf: &[u8]) -> Result<FgbHeader, FgbError> {
    let t = Table::root(buf).ok_or_else(|| malformed("header root"))?;
    let envelope = t.vector(1, 8).and_then(|b| {
        (b.len() >= 32).then(|| [0, 1, 2, 3].map(|i| f64_at(b, i * 8).unwrap_or(f64::NAN)))
    });
    let mut columns = Vec::new();
    for c in t.tables(7).unwrap_or_default() {
        let name = c.str(0).ok_or_else(|| malformed("column name"))?.to_string();
        columns.push(FgbColumn { name, kind: FgbColumnType::from_u8(c.u8(1, 0)) });
    }
    let crs = t.table(10).map(|c| FgbCrs {
        org: c.str(0).map(str::to_string),
        code: c.i32(1, 0),
        name: c.str(2).map(str::to_string),
        wkt: c.str(4).map(str::to_string),
        code_string: c.str(5).map(str::to_string),
    });
    Ok(FgbHeader {
        name: t.str(0).unwrap_or_default().to_string(),
        envelope,
        geometry_type: FgbGeometryType::from_u8(t.u8(2, 0)),
        has_z: t.bool(3, false),
        has_m: t.bool(4, false),
        has_t: t.bool(5, false),
        has_tm: t.bool(6, false),
        columns,
        features_count: t.u64(8, 0),
        index_node_size: t.u16(9, 16),
        crs,
        title: t.str(11).map(str::to_string),
        description: t.str(12).map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// The packed Hilbert R-tree
// ---------------------------------------------------------------------------

/// Each level's node range, LEAVES FIRST: `levels[0]` is the leaves (the
/// end of the node array), the last is the root (`0..1`). As the reference
/// implementation's `generate_level_bounds`.
pub(crate) fn level_bounds(items: u64, node_size: u16) -> Vec<std::ops::Range<u64>> {
    let node_size = u64::from(node_size.max(2));
    let mut counts = vec![items];
    let mut n = items;
    let mut total = items;
    loop {
        n = n.div_ceil(node_size);
        total += n;
        counts.push(n);
        if n <= 1 {
            break;
        }
    }
    let mut offset = total;
    counts
        .into_iter()
        .map(|count| {
            offset -= count;
            offset..offset + count
        })
        .collect()
}

/// The index's size in bytes; 0 when there is none.
pub(crate) fn index_bytes(items: u64, node_size: u16) -> u64 {
    if node_size == 0 || items == 0 {
        return 0;
    }
    level_bounds(items, node_size).iter().map(|r| r.end - r.start).sum::<u64>() * NODE_BYTES
}

/// One R-tree node: `[min_x, min_y, max_x, max_y]` and its offset (a
/// child node's index, or a leaf's feature byte offset).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Node {
    pub(crate) bbox: [f64; 4],
    pub(crate) offset: u64,
}

pub(crate) fn node_at(bytes: &[u8], at: usize) -> Option<Node> {
    Some(Node {
        bbox: [f64_at(bytes, at)?, f64_at(bytes, at + 8)?, f64_at(bytes, at + 16)?, f64_at(bytes, at + 24)?],
        offset: u64_at(bytes, at + 32)?,
    })
}

pub(crate) fn intersects(a: &[f64; 4], b: &[f64; 4]) -> bool {
    a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1]
}

// ---------------------------------------------------------------------------
// Features
// ---------------------------------------------------------------------------

/// Why one feature was not loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FgbSkip {
    /// The feature's flatbuffer does not parse.
    Malformed,
    /// No geometry, or an empty one.
    NoGeometry,
    /// A type with no overlay shape (collections, curves, surfaces).
    UnsupportedGeometry,
    /// A coordinate outside -180..180 / -90..90.
    OutOfRange,
}

impl FgbSkip {
    /// A short phrase for a report line.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Malformed => "malformed feature",
            Self::NoGeometry => "no geometry",
            Self::UnsupportedGeometry => "unsupported geometry type",
            Self::OutOfRange => "coordinate out of range",
        }
    }
}

fn pairs(xy: &[u8]) -> Vec<(f64, f64)> {
    xy.as_chunks::<16>()
        .0
        .iter()
        .map(|c| (f64_at(c, 0).unwrap_or(f64::NAN), f64_at(c, 8).unwrap_or(f64::NAN)))
        .collect()
}

/// Splits `points` at `ends` (each the exclusive end of a part, in
/// positions). No ends: one part.
fn split(points: Vec<(f64, f64)>, ends: Option<&[u8]>) -> Vec<Vec<(f64, f64)>> {
    let ends: Vec<usize> = ends.map(|e| e.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c) as usize).collect()).unwrap_or_default();
    if ends.len() <= 1 {
        return vec![points];
    }
    let mut parts = Vec::with_capacity(ends.len());
    let mut start = 0;
    for end in ends {
        let end = end.min(points.len());
        if end > start {
            parts.push(points[start..end].to_vec());
        }
        start = end;
    }
    parts
}

fn geometry(g: Table<'_>, kind: FgbGeometryType) -> Result<OverlayGeometry, FgbSkip> {
    let kind = match FgbGeometryType::from_u8(g.u8(6, 0)) {
        FgbGeometryType::Unknown => kind,
        own => own,
    };
    let xy = || g.vector(1, 8).map(pairs).unwrap_or_default();
    let geometry = match kind {
        FgbGeometryType::Point | FgbGeometryType::MultiPoint => OverlayGeometry::Point(xy()),
        FgbGeometryType::LineString => OverlayGeometry::Line(vec![xy()]),
        FgbGeometryType::MultiLineString => OverlayGeometry::Line(split(xy(), g.vector(0, 4))),
        FgbGeometryType::Polygon => OverlayGeometry::Polygon(vec![split(xy(), g.vector(0, 4))]),
        FgbGeometryType::MultiPolygon => {
            let mut polygons = Vec::new();
            for part in g.tables(7).unwrap_or_default() {
                match geometry(part, FgbGeometryType::Polygon)? {
                    OverlayGeometry::Polygon(p) => polygons.extend(p),
                    _ => return Err(FgbSkip::Malformed),
                }
            }
            // A writer that put a single polygon's rings straight on the
            // multipolygon (no parts).
            if polygons.is_empty() {
                polygons.push(split(xy(), g.vector(0, 4)));
            }
            OverlayGeometry::Polygon(polygons)
        }
        _ => return Err(FgbSkip::UnsupportedGeometry),
    };
    let empty = match &geometry {
        OverlayGeometry::Point(p) => p.is_empty(),
        OverlayGeometry::Line(l) => l.iter().all(Vec::is_empty),
        OverlayGeometry::Polygon(p) => p.iter().flatten().all(Vec::is_empty),
        _ => false,
    };
    if empty {
        return Err(FgbSkip::NoGeometry);
    }
    if geometry.positions().any(|(x, y)| !(-180.0..=180.0).contains(&x) || !(-90.0..=90.0).contains(&y)) {
        return Err(FgbSkip::OutOfRange);
    }
    Ok(geometry)
}

/// One property value, as the store's typed columns take it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value<'a> {
    Number(f64),
    Bool(bool),
    Str(&'a str),
}

/// A decoded feature: its geometry and its property bytes (decoded into
/// the columns by the caller, which owns the column builders).
pub(crate) struct Feature<'a> {
    pub(crate) geometry: OverlayGeometry,
    pub(crate) properties: Option<&'a [u8]>,
}

/// Decodes one feature from its flatbuffer (the bytes AFTER the `u32`
/// size prefix).
pub(crate) fn parse_feature(buf: &[u8], kind: FgbGeometryType) -> Result<Feature<'_>, FgbSkip> {
    let t = Table::root(buf).ok_or(FgbSkip::Malformed)?;
    let g = t.table(0).ok_or(FgbSkip::NoGeometry)?;
    Ok(Feature { geometry: geometry(g, kind)?, properties: t.vector(1, 1) })
}

/// Walks a feature's property bytes: `(column index, value)` pairs.
/// Binary values are passed over. Stops at the first malformed value.
pub(crate) fn properties<'a>(bytes: &'a [u8], columns: &[FgbColumn], mut each: impl FnMut(usize, Value<'a>)) {
    let mut p = 0usize;
    while p + 2 <= bytes.len() {
        let Some(col) = u16_at(bytes, p).map(usize::from) else { return };
        p += 2;
        let Some(column) = columns.get(col) else { return };
        let b = bytes;
        let fixed = |n: usize| b.get(p..p + n);
        let (value, len) = match column.kind {
            FgbColumnType::Byte => (fixed(1).map(|v| Value::Number(f64::from(v[0] as i8))), 1),
            FgbColumnType::UByte => (fixed(1).map(|v| Value::Number(f64::from(v[0]))), 1),
            FgbColumnType::Bool => (fixed(1).map(|v| Value::Bool(v[0] != 0)), 1),
            FgbColumnType::Short => (fixed(2).map(|v| Value::Number(f64::from(i16::from_le_bytes([v[0], v[1]])))), 2),
            FgbColumnType::UShort => (u16_at(b, p).map(|v| Value::Number(f64::from(v))), 2),
            FgbColumnType::Int => (i32_at(b, p).map(|v| Value::Number(f64::from(v))), 4),
            FgbColumnType::UInt => (u32_at(b, p).map(|v| Value::Number(f64::from(v))), 4),
            FgbColumnType::Long => (u64_at(b, p).map(|v| Value::Number(v as i64 as f64)), 8),
            FgbColumnType::ULong => (u64_at(b, p).map(|v| Value::Number(v as f64)), 8),
            FgbColumnType::Float => (u32_at(b, p).map(|v| Value::Number(f64::from(f32::from_bits(v)))), 4),
            FgbColumnType::Double => (f64_at(b, p).map(Value::Number), 8),
            FgbColumnType::String | FgbColumnType::Json | FgbColumnType::DateTime | FgbColumnType::Binary => {
                let Some(n) = u32_at(b, p).map(|n| n as usize) else { return };
                let Some(text) = b.get(p + 4..(p + 4).saturating_add(n)) else { return };
                p += 4 + n;
                if column.kind != FgbColumnType::Binary {
                    if let Ok(text) = std::str::from_utf8(text) {
                        each(col, Value::Str(text));
                    }
                }
                continue;
            }
        };
        let Some(value) = value else { return };
        p += len;
        each(col, value);
    }
}

/// A geometry's bounding box `[min_x, min_y, max_x, max_y]`.
pub(crate) fn bbox_of(geometry: &OverlayGeometry) -> [f64; 4] {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for (x, y) in geometry.positions() {
        b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
    }
    b
}
