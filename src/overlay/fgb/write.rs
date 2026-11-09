//! A small FlatGeobuf WRITER: points, lines and polygons (and their multi
//! forms), a typed column schema, a WGS84 CRS, and a packed Hilbert R-tree
//! -- the subset this crate reads. For tests, demos and small exports; it
//! holds the whole file in memory.
//!
//! The output follows the spec's layout (magic, size-prefixed header,
//! index, size-prefixed features in index order), with every flatbuffer
//! scalar aligned to its size, so the reference readers accept it.

use super::format::{level_bounds, MAGIC};
use super::{FgbColumnType, FgbGeometryType};
use crate::overlay::geojson::OverlayGeometry;

/// A column's type, as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteType {
    Bool,
    Int,
    Long,
    Double,
    String,
}

impl WriteType {
    fn column_type(self) -> FgbColumnType {
        match self {
            Self::Bool => FgbColumnType::Bool,
            Self::Int => FgbColumnType::Int,
            Self::Long => FgbColumnType::Long,
            Self::Double => FgbColumnType::Double,
            Self::String => FgbColumnType::String,
        }
    }
}

/// One property value. `Null` (or a value of the wrong type for its
/// column) is left out of the feature.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteValue {
    Null,
    Bool(bool),
    Int(i32),
    Long(i64),
    Double(f64),
    String(String),
}

/// One feature: a geometry and one value per column, in column order.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteFeature {
    pub geometry: OverlayGeometry,
    pub properties: Vec<WriteValue>,
}

/// What [`write_with`] writes besides the features.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOptions {
    /// The R-tree's node size; 0 writes no index.
    pub node_size: u16,
    /// The CRS as `(authority, code)`; `None` writes no CRS.
    pub crs: Option<(String, i32)>,
    /// The dataset's name.
    pub name: String,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self { node_size: 16, crs: Some(("EPSG".into(), 4326)), name: String::new() }
    }
}

/// Writes `features` with an index of `node_size` (0: none), EPSG:4326.
#[must_use]
pub fn write(columns: &[(&str, WriteType)], features: &[WriteFeature], node_size: u16) -> Vec<u8> {
    write_with(&WriteOptions { node_size, ..WriteOptions::default() }, columns, features)
}

/// Writes `features` per `options`.
#[must_use]
pub fn write_with(options: &WriteOptions, columns: &[(&str, WriteType)], features: &[WriteFeature]) -> Vec<u8> {
    let boxes: Vec<[f64; 4]> = features.iter().map(|f| super::format::bbox_of(&f.geometry)).collect();
    let mut extent = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for b in &boxes {
        extent = [extent[0].min(b[0]), extent[1].min(b[1]), extent[2].max(b[2]), extent[3].max(b[3])];
    }
    // Hilbert order when indexed (the order the index's leaves take, so a
    // bbox's features sit together on disk); input order otherwise.
    let mut order: Vec<usize> = (0..features.len()).collect();
    if options.node_size > 0 && !features.is_empty() {
        let (w, h) = ((extent[2] - extent[0]).max(f64::MIN_POSITIVE), (extent[3] - extent[1]).max(f64::MIN_POSITIVE));
        let keys: Vec<u64> = boxes
            .iter()
            .map(|b| {
                let x = (((b[0] + b[2]) / 2.0 - extent[0]) / w * 65535.0) as u32;
                let y = (((b[1] + b[3]) / 2.0 - extent[1]) / h * 65535.0) as u32;
                hilbert(x.min(65535), y.min(65535))
            })
            .collect();
        order.sort_by_key(|&i| keys[i]);
    }

    let kinds: Vec<FgbGeometryType> = features.iter().map(|f| kind(&f.geometry)).collect();
    let header_kind = match kinds.first() {
        Some(first) if kinds.iter().all(|k| k == first) => *first,
        _ => FgbGeometryType::Unknown,
    };

    // Features, in order, and each one's offset in the features section.
    let mut body = Vec::new();
    let mut offsets = Vec::with_capacity(features.len());
    for &i in &order {
        offsets.push(body.len() as u64);
        body.extend(feature_bytes(&features[i], columns));
    }

    // The header.
    let column_tables: Vec<Vec<(usize, F)>> = columns
        .iter()
        .map(|(name, ty)| vec![(0, F::Str((*name).to_string())), (1, F::U8(ty.column_type().to_u8()))])
        .collect();
    let mut header = vec![
        (0, F::Str(options.name.clone())),
        (2, F::U8(header_kind.to_u8())),
        (8, F::U64(features.len() as u64)),
        (9, F::U16(options.node_size)),
    ];
    if !features.is_empty() {
        header.push((1, F::F64s(extent.to_vec())));
    }
    if !column_tables.is_empty() {
        header.push((7, F::Tables(column_tables)));
    }
    if let Some((org, code)) = &options.crs {
        header.push((10, F::Table(vec![(0, F::Str(org.clone())), (1, F::I32(*code))])));
    }
    let mut out = MAGIC.to_vec();
    out.extend(finish(&header));

    // The index: leaves in feature order, parents bottom-up, written root
    // first.
    if options.node_size > 0 && !features.is_empty() {
        let node_size = usize::from(options.node_size.max(2));
        let levels = level_bounds(features.len() as u64, options.node_size);
        let total = levels.iter().map(|r| r.end - r.start).sum::<u64>() as usize;
        let mut nodes = vec![([0.0f64; 4], 0u64); total];
        for (j, &i) in order.iter().enumerate() {
            nodes[levels[0].start as usize + j] = (boxes[i], offsets[j]);
        }
        for level in 1..levels.len() {
            let (child, parent) = (levels[level - 1].clone(), levels[level].clone());
            for (j, p) in (parent.start as usize..parent.end as usize).enumerate() {
                let c0 = child.start as usize + j * node_size;
                let c1 = (c0 + node_size).min(child.end as usize);
                let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
                for (cb, _) in &nodes[c0..c1] {
                    b = [b[0].min(cb[0]), b[1].min(cb[1]), b[2].max(cb[2]), b[3].max(cb[3])];
                }
                nodes[p] = (b, c0 as u64);
            }
        }
        for (b, offset) in nodes {
            for v in b {
                out.extend(v.to_le_bytes());
            }
            out.extend(offset.to_le_bytes());
        }
    }
    out.extend(body);
    out
}

fn kind(g: &OverlayGeometry) -> FgbGeometryType {
    match g {
        OverlayGeometry::Point(p) if p.len() == 1 => FgbGeometryType::Point,
        OverlayGeometry::Point(_) => FgbGeometryType::MultiPoint,
        OverlayGeometry::Line(l) if l.len() == 1 => FgbGeometryType::LineString,
        OverlayGeometry::Line(_) => FgbGeometryType::MultiLineString,
        OverlayGeometry::Polygon(p) if p.len() == 1 => FgbGeometryType::Polygon,
        OverlayGeometry::Polygon(_) => FgbGeometryType::MultiPolygon,
        _ => FgbGeometryType::Unknown,
    }
}

fn xy(parts: &[&Vec<(f64, f64)>]) -> (Vec<f64>, Vec<u32>) {
    let mut xy = Vec::new();
    let mut ends = Vec::new();
    for part in parts {
        for (x, y) in part.iter() {
            xy.push(*x);
            xy.push(*y);
        }
        ends.push((xy.len() / 2) as u32);
    }
    (xy, ends)
}

fn geometry_fields(g: &OverlayGeometry) -> Vec<(usize, F)> {
    let k = kind(g);
    let mut fields = vec![(6, F::U8(k.to_u8()))];
    let flat = |parts: Vec<&Vec<(f64, f64)>>, fields: &mut Vec<(usize, F)>| {
        let n = parts.len();
        let (xy, ends) = xy(&parts);
        if n > 1 {
            fields.push((0, F::U32s(ends)));
        }
        fields.push((1, F::F64s(xy)));
    };
    match g {
        OverlayGeometry::Point(p) => {
            let all = p.clone();
            flat(vec![&all], &mut fields);
        }
        OverlayGeometry::Line(l) => flat(l.iter().collect(), &mut fields),
        OverlayGeometry::Polygon(p) if p.len() == 1 => flat(p[0].iter().collect(), &mut fields),
        OverlayGeometry::Polygon(p) => {
            let parts = p.iter().map(|poly| geometry_fields(&OverlayGeometry::Polygon(vec![poly.clone()]))).collect();
            fields.push((7, F::Tables(parts)));
        }
        _ => {}
    }
    fields
}

fn feature_bytes(f: &WriteFeature, columns: &[(&str, WriteType)]) -> Vec<u8> {
    let mut props = Vec::new();
    for (i, ((_, ty), value)) in columns.iter().zip(&f.properties).enumerate() {
        let encoded: Option<Vec<u8>> = match (ty, value) {
            (WriteType::Bool, WriteValue::Bool(b)) => Some(vec![u8::from(*b)]),
            (WriteType::Int, WriteValue::Int(v)) => Some(v.to_le_bytes().to_vec()),
            (WriteType::Long, WriteValue::Long(v)) => Some(v.to_le_bytes().to_vec()),
            (WriteType::Double, WriteValue::Double(v)) => Some(v.to_le_bytes().to_vec()),
            (WriteType::String, WriteValue::String(s)) => {
                let mut b = (s.len() as u32).to_le_bytes().to_vec();
                b.extend(s.as_bytes());
                Some(b)
            }
            _ => None,
        };
        if let Some(encoded) = encoded {
            props.extend((i as u16).to_le_bytes());
            props.extend(encoded);
        }
    }
    let mut fields = vec![(0, F::Table(geometry_fields(&f.geometry)))];
    if !props.is_empty() {
        fields.push((1, F::Bytes(props)));
    }
    finish(&fields)
}

/// The Hilbert curve index of `(x, y)` on a 65536 grid.
fn hilbert(mut x: u32, mut y: u32) -> u64 {
    let n: u32 = 1 << 16;
    let mut d: u64 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = u32::from(x & s > 0);
        let ry = u32::from(y & s > 0);
        d += u64::from(s) * u64::from(s) * u64::from((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = n - 1 - x;
                y = n - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}

// ---------------------------------------------------------------------------
// A forward flatbuffer builder: vtable, then table, then children, every
// scalar aligned to its size from the start of the size-prefixed buffer.
// ---------------------------------------------------------------------------

enum F {
    U8(u8),
    U16(u16),
    I32(i32),
    U64(u64),
    Str(String),
    Bytes(Vec<u8>),
    F64s(Vec<f64>),
    U32s(Vec<u32>),
    Table(Vec<(usize, F)>),
    Tables(Vec<Vec<(usize, F)>>),
}

impl F {
    /// The inline size: the scalar's, or a 4-byte offset.
    fn size(&self) -> usize {
        match self {
            Self::U8(_) => 1,
            Self::U16(_) => 2,
            Self::U64(_) => 8,
            _ => 4,
        }
    }
}

/// A size-prefixed flatbuffer with `fields` as its root table.
fn finish(fields: &[(usize, F)]) -> Vec<u8> {
    let mut b = Builder { buf: vec![0; 8] };
    let root = b.table(fields);
    b.put_u32(4, (root - 4) as u32);
    // A whole number of 8-byte words, as the reference builders finish:
    // the next buffer in the file then starts 8-aligned too, which readers
    // that view `f64` vectors in place over the whole file (the JS one's
    // `Float64Array`) require.
    b.pad(8);
    let size = (b.buf.len() - 4) as u32;
    b.put_u32(0, size);
    b.buf
}

struct Builder {
    buf: Vec<u8>,
}

impl Builder {
    fn pad(&mut self, align: usize) {
        while !self.buf.len().is_multiple_of(align) {
            self.buf.push(0);
        }
    }

    fn put_u32(&mut self, at: usize, v: u32) {
        self.buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn table(&mut self, fields: &[(usize, F)]) -> usize {
        let slots = fields.iter().map(|(id, _)| id + 1).max().unwrap_or(0);
        let mut order: Vec<usize> = (0..fields.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(fields[i].1.size()));
        let mut at = vec![0usize; fields.len()];
        let mut vt = vec![0u16; slots];
        let mut off = 4usize;
        for &i in &order {
            let size = fields[i].1.size();
            off = off.next_multiple_of(size);
            at[i] = off;
            vt[fields[i].0] = off as u16;
            off += size;
        }
        let table_size = off;
        self.pad(2);
        let vtable = self.buf.len();
        self.buf.extend(((4 + 2 * slots) as u16).to_le_bytes());
        self.buf.extend((table_size as u16).to_le_bytes());
        for v in vt {
            self.buf.extend(v.to_le_bytes());
        }
        self.pad(8);
        let t = self.buf.len();
        self.buf.extend(((t - vtable) as i32).to_le_bytes());
        self.buf.resize(t + table_size, 0);
        for (i, (_, f)) in fields.iter().enumerate() {
            let p = t + at[i];
            match f {
                F::U8(v) => self.buf[p] = *v,
                F::U16(v) => self.buf[p..p + 2].copy_from_slice(&v.to_le_bytes()),
                F::I32(v) => self.buf[p..p + 4].copy_from_slice(&v.to_le_bytes()),
                F::U64(v) => self.buf[p..p + 8].copy_from_slice(&v.to_le_bytes()),
                _ => {}
            }
        }
        for (i, (_, f)) in fields.iter().enumerate() {
            if f.size() == 4 && !matches!(f, F::I32(_)) {
                let p = t + at[i];
                let child = self.child(f);
                self.put_u32(p, (child - p) as u32);
            }
        }
        t
    }

    fn child(&mut self, f: &F) -> usize {
        match f {
            F::Str(s) => {
                self.pad(4);
                let p = self.buf.len();
                self.buf.extend((s.len() as u32).to_le_bytes());
                self.buf.extend(s.as_bytes());
                self.buf.push(0);
                p
            }
            F::Bytes(bytes) => {
                self.pad(4);
                let p = self.buf.len();
                self.buf.extend((bytes.len() as u32).to_le_bytes());
                self.buf.extend(bytes);
                p
            }
            F::F64s(v) => {
                while !(self.buf.len() + 4).is_multiple_of(8) {
                    self.buf.push(0);
                }
                let p = self.buf.len();
                self.buf.extend((v.len() as u32).to_le_bytes());
                for x in v {
                    self.buf.extend(x.to_le_bytes());
                }
                p
            }
            F::U32s(v) => {
                self.pad(4);
                let p = self.buf.len();
                self.buf.extend((v.len() as u32).to_le_bytes());
                for x in v {
                    self.buf.extend(x.to_le_bytes());
                }
                p
            }
            F::Table(fields) => self.table(fields),
            F::Tables(list) => {
                self.pad(4);
                let p = self.buf.len();
                self.buf.extend((list.len() as u32).to_le_bytes());
                self.buf.resize(p + 4 + 4 * list.len(), 0);
                for (i, fields) in list.iter().enumerate() {
                    let t = self.table(fields);
                    let slot = p + 4 + 4 * i;
                    self.put_u32(slot, (t - slot) as u32);
                }
                p
            }
            F::U8(_) | F::U16(_) | F::I32(_) | F::U64(_) => unreachable!("a scalar is inline"),
        }
    }
}
