//! Arrow arrays to overlay rows: WKB and GeoArrow-native geometry, and
//! property columns mapped to the store's three types.

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Date64Type, Decimal128Type, Float16Type, Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type,
    TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt16Type,
    UInt32Type, UInt64Type, UInt8Type,
};
use arrow_array::Array;
use arrow_schema::{DataType, TimeUnit};

use super::{GeoParquetEncoding, GeoParquetSkip};
use crate::overlay::csv::CsvColumnType;
use crate::overlay::geojson::OverlayGeometry;

/// One decoded geometry: a single point kept flat (the point path's
/// shape), anything else as the store holds it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Decoded {
    Point([f64; 2]),
    Shape(OverlayGeometry),
}

impl Decoded {
    pub(crate) fn bbox(&self) -> [f64; 4] {
        match self {
            Self::Point(p) => [p[0], p[1], p[0], p[1]],
            Self::Shape(g) => crate::overlay::fgb::bbox_of(g),
        }
    }
}

fn checked(g: OverlayGeometry) -> Result<Decoded, GeoParquetSkip> {
    let empty = match &g {
        OverlayGeometry::Point(p) => p.is_empty(),
        OverlayGeometry::Line(l) => l.iter().all(Vec::is_empty),
        OverlayGeometry::Polygon(p) => p.iter().flatten().all(Vec::is_empty),
        _ => true,
    };
    if empty {
        return Err(GeoParquetSkip::NoGeometry);
    }
    if g.positions().any(|(x, y)| !(-180.0..=180.0).contains(&x) || !(-90.0..=90.0).contains(&y)) {
        return Err(GeoParquetSkip::OutOfRange);
    }
    Ok(match g {
        OverlayGeometry::Point(p) if p.len() == 1 => Decoded::Point([p[0].0, p[0].1]),
        g => Decoded::Shape(g),
    })
}

// ---------------------------------------------------------------------------
// WKB
// ---------------------------------------------------------------------------

struct Wkb<'a> {
    b: &'a [u8],
    p: usize,
}

impl Wkb<'_> {
    fn bytes<const N: usize>(&mut self, little: bool) -> Option<[u8; N]> {
        let mut out: [u8; N] = self.b.get(self.p..self.p + N)?.try_into().ok()?;
        self.p += N;
        if !little {
            out.reverse();
        }
        Some(out)
    }
    fn u32(&mut self, little: bool) -> Option<u32> {
        self.bytes::<4>(little).map(u32::from_le_bytes)
    }
    fn f64(&mut self, little: bool) -> Option<f64> {
        self.bytes::<8>(little).map(f64::from_le_bytes)
    }
    /// A count, bounded by the bytes left (a corrupt count would otherwise
    /// ask for a huge allocation).
    fn count(&mut self, little: bool, min_item: usize) -> Option<usize> {
        let n = self.u32(little)? as usize;
        (n.saturating_mul(min_item) <= self.b.len().saturating_sub(self.p)).then_some(n)
    }

    /// A geometry header: (little-endian, base type 1..=7, coordinate count).
    fn header(&mut self) -> Option<(bool, u32, usize)> {
        let little = match *self.b.get(self.p)? {
            0 => false,
            1 => true,
            _ => return None,
        };
        self.p += 1;
        let t = self.u32(little)?;
        // EWKB flags (PostGIS): Z, M, SRID.
        let (mut z, mut m) = (t & 0x8000_0000 != 0, t & 0x4000_0000 != 0);
        if t & 0x2000_0000 != 0 {
            self.u32(little)?;
        }
        let iso = t & 0x0fff_ffff;
        match iso / 1000 {
            1 => z = true,
            2 => m = true,
            3 => (z, m) = (true, true),
            _ => {}
        }
        Some((little, iso % 1000, 2 + usize::from(z) + usize::from(m)))
    }

    fn coords(&mut self, little: bool, dims: usize) -> Option<Vec<(f64, f64)>> {
        let n = self.count(little, dims * 8)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.coord(little, dims)?);
        }
        Some(out)
    }

    fn coord(&mut self, little: bool, dims: usize) -> Option<(f64, f64)> {
        let x = self.f64(little)?;
        let y = self.f64(little)?;
        self.p += (dims - 2) * 8;
        Some((x, y))
    }

    fn rings(&mut self, little: bool, dims: usize) -> Option<Vec<Vec<(f64, f64)>>> {
        let n = self.count(little, 4)?;
        (0..n).map(|_| self.coords(little, dims)).collect()
    }

    /// One part of a multi geometry, which must be of `want` type.
    fn part(&mut self, want: u32) -> Option<(bool, usize)> {
        let (little, kind, dims) = self.header()?;
        (kind == want).then_some((little, dims))
    }
}

/// Decodes one WKB (ISO or EWKB, 2D/Z/M/ZM -- Z and M are dropped).
pub(crate) fn wkb(b: &[u8]) -> Result<Decoded, GeoParquetSkip> {
    let mut r = Wkb { b, p: 0 };
    let malformed = GeoParquetSkip::Malformed;
    let (little, kind, dims) = r.header().ok_or(malformed)?;
    let g = match kind {
        1 => {
            let (x, y) = r.coord(little, dims).ok_or(malformed)?;
            if x.is_nan() && y.is_nan() {
                return Err(GeoParquetSkip::NoGeometry);
            }
            OverlayGeometry::Point(vec![(x, y)])
        }
        2 => OverlayGeometry::Line(vec![r.coords(little, dims).ok_or(malformed)?]),
        3 => OverlayGeometry::Polygon(vec![r.rings(little, dims).ok_or(malformed)?]),
        4 => {
            let n = r.count(little, 21).ok_or(malformed)?;
            let mut points = Vec::with_capacity(n);
            for _ in 0..n {
                let (l, d) = r.part(1).ok_or(malformed)?;
                let (x, y) = r.coord(l, d).ok_or(malformed)?;
                if !(x.is_nan() && y.is_nan()) {
                    points.push((x, y));
                }
            }
            OverlayGeometry::Point(points)
        }
        5 => {
            let n = r.count(little, 9).ok_or(malformed)?;
            let mut lines = Vec::with_capacity(n);
            for _ in 0..n {
                let (l, d) = r.part(2).ok_or(malformed)?;
                lines.push(r.coords(l, d).ok_or(malformed)?);
            }
            OverlayGeometry::Line(lines)
        }
        6 => {
            let n = r.count(little, 9).ok_or(malformed)?;
            let mut polygons = Vec::with_capacity(n);
            for _ in 0..n {
                let (l, d) = r.part(3).ok_or(malformed)?;
                polygons.push(r.rings(l, d).ok_or(malformed)?);
            }
            OverlayGeometry::Polygon(polygons)
        }
        _ => return Err(GeoParquetSkip::UnsupportedGeometry),
    };
    checked(g)
}

// ---------------------------------------------------------------------------
// GeoArrow native
// ---------------------------------------------------------------------------

/// A coordinate array: separated (`struct<x, y, ..>`, what GeoParquet 1.1
/// writes) or interleaved (`fixed_size_list<double>[2..4]`).
enum Coords<'a> {
    Separated(&'a arrow_array::Float64Array, &'a arrow_array::Float64Array),
    Interleaved(&'a arrow_array::Float64Array, usize),
}

impl Coords<'_> {
    fn of(a: &dyn Array) -> Option<Coords<'_>> {
        if let Some(s) = a.as_struct_opt() {
            let x = s.column_by_name("x")?.as_primitive_opt::<Float64Type>()?;
            let y = s.column_by_name("y")?.as_primitive_opt::<Float64Type>()?;
            return Some(Coords::Separated(x, y));
        }
        let l = a.as_fixed_size_list_opt()?;
        let size = usize::try_from(l.value_length()).ok().filter(|&n| n >= 2)?;
        Some(Coords::Interleaved(l.values().as_primitive_opt::<Float64Type>()?, size))
    }
    fn at(&self, i: usize) -> (f64, f64) {
        match self {
            Self::Separated(x, y) => (x.value(i), y.value(i)),
            Self::Interleaved(v, n) => (v.value(i * n), v.value(i * n + 1)),
        }
    }
    fn run(&self, r: std::ops::Range<usize>) -> Vec<(f64, f64)> {
        r.map(|i| self.at(i)).collect()
    }
}

/// A list level: (offsets as usize, the child array).
struct Level<'a> {
    offsets: Vec<usize>,
    child: &'a dyn Array,
}

impl Level<'_> {
    /// Offsets index the (unsliced) child array, so they are used as
    /// they are.
    fn of(a: &dyn Array) -> Option<Level<'_>> {
        if let Some(l) = a.as_list_opt::<i32>() {
            return Some(Level { offsets: l.offsets().iter().map(|&o| o as usize).collect(), child: l.values().as_ref() });
        }
        let l = a.as_list_opt::<i64>()?;
        Some(Level { offsets: l.offsets().iter().map(|&o| o as usize).collect(), child: l.values().as_ref() })
    }
    fn range(&self, i: usize) -> std::ops::Range<usize> {
        self.offsets[i]..self.offsets[i + 1]
    }
}

/// A GeoArrow-native geometry column, read row by row.
pub(crate) struct Native<'a> {
    array: &'a dyn Array,
    encoding: GeoParquetEncoding,
    /// Outermost list level first; the coordinates last.
    levels: Vec<Level<'a>>,
    coords: Coords<'a>,
}

impl<'a> Native<'a> {
    pub(crate) fn of(array: &'a dyn Array, encoding: GeoParquetEncoding) -> Option<Self> {
        let depth = match encoding {
            GeoParquetEncoding::Point => 0,
            GeoParquetEncoding::LineString | GeoParquetEncoding::MultiPoint => 1,
            GeoParquetEncoding::Polygon | GeoParquetEncoding::MultiLineString => 2,
            GeoParquetEncoding::MultiPolygon => 3,
            GeoParquetEncoding::Wkb => return None,
        };
        let mut levels = Vec::with_capacity(depth);
        let mut at: &dyn Array = array;
        for _ in 0..depth {
            let level = Level::of(at)?;
            at = level.child;
            levels.push(level);
        }
        Some(Self { array, encoding, levels, coords: Coords::of(at)? })
    }

    pub(crate) fn at(&self, i: usize) -> Result<Decoded, GeoParquetSkip> {
        if self.array.is_null(i) {
            return Err(GeoParquetSkip::NoGeometry);
        }
        let c = &self.coords;
        let g = match self.encoding {
            GeoParquetEncoding::Point => {
                let (x, y) = c.at(i);
                if x.is_nan() && y.is_nan() {
                    return Err(GeoParquetSkip::NoGeometry);
                }
                OverlayGeometry::Point(vec![(x, y)])
            }
            GeoParquetEncoding::MultiPoint => OverlayGeometry::Point(c.run(self.levels[0].range(i))),
            GeoParquetEncoding::LineString => OverlayGeometry::Line(vec![c.run(self.levels[0].range(i))]),
            GeoParquetEncoding::MultiLineString | GeoParquetEncoding::Polygon => {
                let parts: Vec<Vec<(f64, f64)>> = self.levels[0].range(i).map(|p| c.run(self.levels[1].range(p))).collect();
                if self.encoding == GeoParquetEncoding::Polygon {
                    OverlayGeometry::Polygon(vec![parts])
                } else {
                    OverlayGeometry::Line(parts)
                }
            }
            GeoParquetEncoding::MultiPolygon => OverlayGeometry::Polygon(
                self.levels[0]
                    .range(i)
                    .map(|poly| self.levels[1].range(poly).map(|ring| c.run(self.levels[2].range(ring))).collect())
                    .collect(),
            ),
            GeoParquetEncoding::Wkb => return Err(GeoParquetSkip::UnsupportedGeometry),
        };
        checked(g)
    }
}

/// The geometry column of one batch.
pub(crate) enum Geometry<'a> {
    Binary(Box<dyn Fn(usize) -> Option<&'a [u8]> + 'a>),
    Native(Native<'a>),
}

impl<'a> Geometry<'a> {
    pub(crate) fn of(array: &'a dyn Array, encoding: GeoParquetEncoding) -> Option<Self> {
        if encoding != GeoParquetEncoding::Wkb {
            return Native::of(array, encoding).map(Self::Native);
        }
        if let Some(a) = array.as_binary_opt::<i32>() {
            return Some(Self::Binary(Box::new(move |i| a.is_valid(i).then(|| a.value(i)))));
        }
        if let Some(a) = array.as_binary_opt::<i64>() {
            return Some(Self::Binary(Box::new(move |i| a.is_valid(i).then(|| a.value(i)))));
        }
        let a = array.as_binary_view_opt()?;
        Some(Self::Binary(Box::new(move |i| a.is_valid(i).then(|| a.value(i)))))
    }

    pub(crate) fn at(&self, i: usize) -> Result<Decoded, GeoParquetSkip> {
        match self {
            Self::Binary(get) => wkb(get(i).ok_or(GeoParquetSkip::NoGeometry)?),
            Self::Native(n) => n.at(i),
        }
    }
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

/// The store type an Arrow column becomes; `None`: dropped (binary,
/// nested, intervals).
///
/// Every integer, float and decimal is a number; dates and timestamps are
/// numbers too, in milliseconds since the Unix epoch (so `interpolate` and
/// comparisons work on them); `Boolean` is a bool; `Utf8` (any width,
/// views, dictionary-encoded) is a string.
pub(crate) fn kind_of(dt: &DataType) -> Option<CsvColumnType> {
    Some(match dt {
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Decimal128(_, _)
        | DataType::Date32
        | DataType::Date64
        | DataType::Timestamp(_, _) => CsvColumnType::Number,
        DataType::Boolean => CsvColumnType::Bool,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => CsvColumnType::String,
        DataType::Dictionary(_, v) if matches!(**v, DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View) => {
            CsvColumnType::String
        }
        _ => return None,
    })
}

type Get<'a, T> = Box<dyn Fn(usize) -> Option<T> + 'a>;

macro_rules! prim {
    ($a:expr, $t:ty, $f:expr) => {{
        let p = $a.as_primitive_opt::<$t>()?;
        let f = $f;
        let get: Get<'_, f64> = Box::new(move |i| p.is_valid(i).then(|| f(p.value(i))));
        Some(get)
    }};
}

/// A number getter over one batch's column.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn numbers(a: &dyn Array) -> Option<Get<'_, f64>> {
    match a.data_type() {
        DataType::Int8 => prim!(a, Int8Type, f64::from),
        DataType::Int16 => prim!(a, Int16Type, f64::from),
        DataType::Int32 => prim!(a, Int32Type, f64::from),
        DataType::Int64 => prim!(a, Int64Type, |v: i64| v as f64),
        DataType::UInt8 => prim!(a, UInt8Type, f64::from),
        DataType::UInt16 => prim!(a, UInt16Type, f64::from),
        DataType::UInt32 => prim!(a, UInt32Type, f64::from),
        DataType::UInt64 => prim!(a, UInt64Type, |v: u64| v as f64),
        DataType::Float16 => prim!(a, Float16Type, f64::from),
        DataType::Float32 => prim!(a, Float32Type, f64::from),
        DataType::Float64 => prim!(a, Float64Type, |v: f64| v),
        DataType::Decimal128(_, scale) => {
            let div = 10f64.powi(i32::from(*scale));
            prim!(a, Decimal128Type, move |v: i128| v as f64 / div)
        }
        DataType::Date32 => prim!(a, Date32Type, |v: i32| f64::from(v) * 86_400_000.0),
        DataType::Date64 => prim!(a, Date64Type, |v: i64| v as f64),
        DataType::Timestamp(TimeUnit::Second, _) => prim!(a, TimestampSecondType, |v: i64| v as f64 * 1000.0),
        DataType::Timestamp(TimeUnit::Millisecond, _) => prim!(a, TimestampMillisecondType, |v: i64| v as f64),
        DataType::Timestamp(TimeUnit::Microsecond, _) => prim!(a, TimestampMicrosecondType, |v: i64| v as f64 / 1e3),
        DataType::Timestamp(TimeUnit::Nanosecond, _) => prim!(a, TimestampNanosecondType, |v: i64| v as f64 / 1e6),
        _ => None,
    }
}

/// A bool getter.
pub(crate) fn bools(a: &dyn Array) -> Option<Get<'_, bool>> {
    let b = a.as_boolean_opt()?;
    Some(Box::new(move |i| b.is_valid(i).then(|| b.value(i))))
}

/// A string getter (plain, large, view or dictionary-encoded).
pub(crate) fn strings(a: &dyn Array) -> Option<Get<'_, &str>> {
    if let Some(s) = a.as_string_opt::<i32>() {
        return Some(Box::new(move |i| s.is_valid(i).then(|| s.value(i))));
    }
    if let Some(s) = a.as_string_opt::<i64>() {
        return Some(Box::new(move |i| s.is_valid(i).then(|| s.value(i))));
    }
    if let Some(s) = a.as_string_view_opt() {
        return Some(Box::new(move |i| s.is_valid(i).then(|| s.value(i))));
    }
    let d = a.as_any_dictionary_opt()?;
    let keys = d.normalized_keys();
    let values = strings(d.values().as_ref())?;
    Some(Box::new(move |i| if a.is_valid(i) { keys.get(i).and_then(|&k| values(k)) } else { None }))
}

/// A float getter over a covering field (`Float64` or `Float32`).
pub(crate) fn floats(a: &dyn Array) -> Option<Get<'_, f64>> {
    match a.data_type() {
        DataType::Float64 => prim!(a, Float64Type, |v: f64| v),
        DataType::Float32 => prim!(a, Float32Type, f64::from),
        _ => None,
    }
}
