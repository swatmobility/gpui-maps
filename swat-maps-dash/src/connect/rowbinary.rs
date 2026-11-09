//! The `RowBinaryWithNamesAndTypes` decoder (§8.2), into column storage
//! (D4) with wide numbers exact (D7).
//!
//! Types read: `(U)Int8–256`, `Float32/64`, `Bool`, `String`,
//! `FixedString`, `Date`, `Date32`, `DateTime`, `DateTime64`, `Decimal*`,
//! `UUID`, `Enum8/16`, `Nullable(T)`, `LowCardinality(T)` and `Array(T)` of
//! those; `Map(K, V)` and `Tuple(…)` of those as ClickHouse's own text
//! (`{'k':'v'}`, `(1,'a')`), so one such column never fails a whole query.
//! Anything else is `column_type_unsupported`, naming the column.
//! Type strings are parsed by [`super::chtypes`] (ported from clickpad).

use super::chtypes::{parse, ChType, SimpleType};
use crate::hook::{Bitmap, Column, ColumnData, RawTable, StrColumn};
use crate::sql::Scalar;

/// Why a body did not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The body ended inside a value.
    UnexpectedEnd,
    /// A string was not UTF-8.
    NotUtf8 { column: String },
    /// A column type this decoder does not read; suggest a cast.
    ColumnTypeUnsupported { column: String, ty: String },
}

impl DecodeError {
    pub fn code(&self) -> &'static str {
        match self {
            DecodeError::UnexpectedEnd => "decode_unexpected_end",
            DecodeError::NotUtf8 { .. } => "decode_not_utf8",
            DecodeError::ColumnTypeUnsupported { .. } => "column_type_unsupported",
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.at.checked_add(n).filter(|e| *e <= self.b.len()).ok_or(DecodeError::UnexpectedEnd)?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn leb(&mut self) -> Result<u64, DecodeError> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)?[0];
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError::UnexpectedEnd)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    fn string(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.leb()? as usize;
        self.take(n)
    }
    fn done(&self) -> bool {
        self.at >= self.b.len()
    }
}

/// How one column is stored, decided from its type once.
#[derive(Debug, Clone)]
enum Plan {
    Int(u8),        // signed, byte width 1/2/4/8
    UInt(u8),       // unsigned, byte width 1/2/4/8
    WideText(u8, bool), // 16/32 bytes, signed: exact text (D7)
    Float(u8),
    Bool,
    Str,
    Fixed(usize),
    Uuid,
    Date,
    Date32,
    DateTime,
    DateTime64(u8),
    Decimal { width: u8, scale: u8 }, // 4/8/16 → i128; 32 → text
    Enum(u8, Vec<(String, i16)>),
    Nullable(Box<Plan>),
    Array(Box<Plan>),
    /// Read as text: `{'k':v,…}`.
    Map(Box<Plan>, Box<Plan>),
    /// Read as text: `(a,b,…)`.
    Tuple(Vec<Plan>),
}

fn plan(t: &ChType) -> Option<Plan> {
    use SimpleType as S;
    Some(match t {
        ChType::Nullable(i) => Plan::Nullable(Box::new(plan(i)?)),
        ChType::LowCardinality(i) => plan(i)?,
        ChType::Array(i) => Plan::Array(Box::new(plan(i)?)),
        ChType::Map(k, v) => Plan::Map(Box::new(plan(k)?), Box::new(plan(v)?)),
        ChType::Tuple(fields) => Plan::Tuple(fields.iter().map(|f| plan(&f.ty)).collect::<Option<Vec<_>>>()?),
        ChType::Enum8(v) => Plan::Enum(1, v.clone()),
        ChType::Enum16(v) => Plan::Enum(2, v.clone()),
        ChType::Decimal { precision, scale } => {
            let width = match precision {
                0..=9 => 4,
                10..=18 => 8,
                19..=38 => 16,
                _ => 32,
            };
            Plan::Decimal { width, scale: *scale }
        }
        ChType::DateTime64 { precision, .. } => Plan::DateTime64(*precision),
        ChType::DateTime { .. } => Plan::DateTime,
        ChType::FixedString(n) => Plan::Fixed(*n),
        ChType::Simple(s) => match s {
            S::Bool => Plan::Bool,
            S::Int8 => Plan::Int(1),
            S::Int16 => Plan::Int(2),
            S::Int32 => Plan::Int(4),
            S::Int64 => Plan::Int(8),
            S::UInt8 => Plan::UInt(1),
            S::UInt16 => Plan::UInt(2),
            S::UInt32 => Plan::UInt(4),
            S::UInt64 => Plan::UInt(8),
            S::Int128 => Plan::WideText(16, true),
            S::Int256 => Plan::WideText(32, true),
            S::UInt128 => Plan::WideText(16, false),
            S::UInt256 => Plan::WideText(32, false),
            S::Float32 => Plan::Float(4),
            S::Float64 => Plan::Float(8),
            S::String => Plan::Str,
            S::Uuid => Plan::Uuid,
            S::Date => Plan::Date,
            S::Date32 => Plan::Date32,
            _ => return None,
        },
        _ => return None,
    })
}

fn base(p: &Plan) -> &Plan {
    match p {
        Plan::Nullable(i) => base(i),
        other => other,
    }
}

fn empty_data(p: &Plan) -> ColumnData {
    match base(p) {
        Plan::Int(_) => ColumnData::Int(Vec::new()),
        Plan::UInt(_) => ColumnData::UInt(Vec::new()),
        Plan::Float(_) => ColumnData::Float(Vec::new()),
        Plan::Bool => ColumnData::Bool(Vec::new()),
        Plan::Date | Plan::Date32 | Plan::DateTime | Plan::DateTime64(_) => ColumnData::Time(Vec::new()),
        Plan::Decimal { width, scale } if *width <= 16 => ColumnData::Decimal { scale: *scale, values: Vec::new() },
        Plan::Array(_) => ColumnData::List(Vec::new()),
        _ => ColumnData::Str(StrColumn::default()),
    }
}

/// Twos-complement little-endian bytes as exact decimal text.
fn wide_text(bytes: &[u8], signed: bool) -> String {
    let neg = signed && bytes.last().is_some_and(|b| b & 0x80 != 0);
    let mut mag: Vec<u8> = bytes.to_vec();
    if neg {
        // magnitude = !x + 1
        let mut carry = 1u16;
        for b in mag.iter_mut() {
            let v = u16::from(!*b) + carry;
            *b = v as u8;
            carry = v >> 8;
        }
    }
    // Repeated division by 10 over the little-endian magnitude.
    let mut digits = Vec::new();
    while mag.iter().any(|b| *b != 0) {
        let mut rem = 0u16;
        for b in mag.iter_mut().rev() {
            let cur = (rem << 8) | u16::from(*b);
            *b = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    if neg {
        digits.push(b'-');
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

fn uuid_text(b: &[u8]) -> String {
    // Two little-endian u64 halves, high first.
    let hi = u64::from_le_bytes(b[0..8].try_into().unwrap_or_default());
    let lo = u64::from_le_bytes(b[8..16].try_into().unwrap_or_default());
    let h = format!("{hi:016x}{lo:016x}");
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// A value inside a map, tuple or array, as ClickHouse writes it: text
/// quoted, a null `NULL`.
fn nested_text(v: Option<Scalar>) -> String {
    match v {
        None => "NULL".into(),
        Some(Scalar::Str(s)) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Some(x) => x.to_text(),
    }
}

/// One value inside a map, tuple or array as text: a nested array, map or
/// tuple as its own text, unquoted.
fn read_nested(r: &mut Reader<'_>, p: &Plan, col: &str) -> Result<String, DecodeError> {
    let v = read_scalar(r, p, col)?;
    Ok(match (base(p), v) {
        (Plan::Array(_) | Plan::Map(..) | Plan::Tuple(_), Some(Scalar::Str(s))) => s,
        (_, v) => nested_text(v),
    })
}

/// One value as a scalar (for array elements), or `None` for a null.
fn read_scalar(r: &mut Reader<'_>, p: &Plan, col: &str) -> Result<Option<Scalar>, DecodeError> {
    let utf8 = |b: &[u8]| std::str::from_utf8(b).map(str::to_string).map_err(|_| DecodeError::NotUtf8 { column: col.to_string() });
    Ok(Some(match p {
        Plan::Nullable(i) => {
            if r.take(1)?[0] != 0 {
                return Ok(None);
            }
            return read_scalar(r, i, col);
        }
        Plan::Int(1) => Scalar::Int(i8::from_le_bytes(r.arr()?) as i64),
        Plan::Int(2) => Scalar::Int(i16::from_le_bytes(r.arr()?) as i64),
        Plan::Int(4) => Scalar::Int(i32::from_le_bytes(r.arr()?) as i64),
        Plan::Int(_) => Scalar::Int(i64::from_le_bytes(r.arr()?)),
        Plan::UInt(1) => Scalar::UInt(r.take(1)?[0] as u64),
        Plan::UInt(2) => Scalar::UInt(u16::from_le_bytes(r.arr()?) as u64),
        Plan::UInt(4) => Scalar::UInt(u32::from_le_bytes(r.arr()?) as u64),
        Plan::UInt(_) => Scalar::UInt(u64::from_le_bytes(r.arr()?)),
        Plan::WideText(w, s) => Scalar::Str(wide_text(r.take(*w as usize)?, *s)),
        Plan::Float(4) => Scalar::Float(f32::from_le_bytes(r.arr()?) as f64),
        Plan::Float(_) => Scalar::Float(f64::from_le_bytes(r.arr()?)),
        Plan::Bool => Scalar::Bool(r.take(1)?[0] != 0),
        Plan::Str => Scalar::Str(utf8(r.string()?)?),
        Plan::Fixed(n) => Scalar::Str(utf8(r.take(*n)?)?.trim_end_matches('\0').to_string()),
        Plan::Uuid => Scalar::Str(uuid_text(r.take(16)?)),
        Plan::Date => Scalar::Time(u16::from_le_bytes(r.arr()?) as i64 * 86_400_000),
        Plan::Date32 => Scalar::Time(i32::from_le_bytes(r.arr()?) as i64 * 86_400_000),
        Plan::DateTime => Scalar::Time(u32::from_le_bytes(r.arr()?) as i64 * 1000),
        Plan::DateTime64(p) => {
            let ticks = i64::from_le_bytes(r.arr()?);
            let p = *p as i32;
            Scalar::Time(if p >= 3 { ticks / 10i64.pow((p - 3) as u32) } else { ticks * 10i64.pow((3 - p) as u32) })
        }
        Plan::Decimal { width, scale } => match width {
            4 => Scalar::Decimal { value: i32::from_le_bytes(r.arr()?) as i128, scale: *scale },
            8 => Scalar::Decimal { value: i64::from_le_bytes(r.arr()?) as i128, scale: *scale },
            16 => Scalar::Decimal { value: i128::from_le_bytes(r.arr()?), scale: *scale },
            _ => {
                let t = wide_text(r.take(32)?, true);
                Scalar::Str(crate::sql::decimal_text_from_str(&t, *scale))
            }
        },
        Plan::Enum(w, names) => {
            let v = if *w == 1 { i8::from_le_bytes(r.arr()?) as i16 } else { i16::from_le_bytes(r.arr()?) };
            Scalar::Str(names.iter().find(|(_, n)| *n == v).map(|(s, _)| s.clone()).unwrap_or_else(|| v.to_string()))
        }
        Plan::Array(i) => {
            let n = r.leb()? as usize;
            let mut items = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                items.push(read_nested(r, i, col)?);
            }
            // An array inside a map or tuple; a column's goes through `push`.
            Scalar::Str(format!("[{}]", items.join(",")))
        }
        Plan::Map(k, v) => {
            let n = r.leb()? as usize;
            let mut parts = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                let key = read_nested(r, k, col)?;
                let value = read_nested(r, v, col)?;
                parts.push(format!("{key}:{value}"));
            }
            Scalar::Str(format!("{{{}}}", parts.join(",")))
        }
        Plan::Tuple(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for p in items {
                parts.push(read_nested(r, p, col)?);
            }
            Scalar::Str(format!("({})", parts.join(",")))
        }
    }))
}

/// Reads one value into its column.
fn push(r: &mut Reader<'_>, p: &Plan, data: &mut ColumnData, valid: &mut Bitmap, col: &str) -> Result<(), DecodeError> {
    let inner = match p {
        Plan::Nullable(i) => {
            if r.take(1)?[0] != 0 {
                push_default(data);
                valid.push(false);
                return Ok(());
            }
            i.as_ref()
        }
        other => other,
    };
    valid.push(true);
    match (inner, data) {
        (Plan::Str, ColumnData::Str(s)) => {
            let b = r.string()?;
            if std::str::from_utf8(b).is_err() {
                return Err(DecodeError::NotUtf8 { column: col.to_string() });
            }
            s.push_bytes(b);
        }
        (Plan::Int(8), ColumnData::Int(v)) => v.push(i64::from_le_bytes(r.arr()?)),
        (Plan::UInt(8), ColumnData::UInt(v)) => v.push(u64::from_le_bytes(r.arr()?)),
        (Plan::Float(8), ColumnData::Float(v)) => v.push(f64::from_le_bytes(r.arr()?)),
        (Plan::Array(i), ColumnData::List(v)) => {
            let n = r.leb()? as usize;
            let mut items = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                if let Some(s) = read_scalar(r, i, col)? {
                    items.push(s);
                }
            }
            v.push(items);
        }
        (p, d) => match (read_scalar(r, p, col)?, d) {
            (Some(Scalar::Int(x)), ColumnData::Int(v)) => v.push(x),
            (Some(Scalar::UInt(x)), ColumnData::UInt(v)) => v.push(x),
            (Some(Scalar::Float(x)), ColumnData::Float(v)) => v.push(x),
            (Some(Scalar::Bool(x)), ColumnData::Bool(v)) => v.push(x),
            (Some(Scalar::Time(x)), ColumnData::Time(v)) => v.push(x),
            (Some(Scalar::Decimal { value, .. }), ColumnData::Decimal { values, .. }) => values.push(value),
            (Some(Scalar::Str(x)), ColumnData::Str(s)) => s.push(&x),
            _ => return Err(DecodeError::UnexpectedEnd),
        },
    }
    Ok(())
}

fn push_default(data: &mut ColumnData) {
    match data {
        ColumnData::Bool(v) => v.push(false),
        ColumnData::Int(v) | ColumnData::Time(v) => v.push(0),
        ColumnData::UInt(v) => v.push(0),
        ColumnData::Float(v) => v.push(0.0),
        ColumnData::Decimal { values, .. } => values.push(0),
        ColumnData::Str(s) => s.push(""),
        ColumnData::List(v) => v.push(Vec::new()),
    }
}

/// The decoded table and whether rows were left after the cap.
pub fn decode(body: &[u8], max_rows: u64) -> Result<RawTable, DecodeError> {
    let mut r = Reader { b: body, at: 0 };
    if r.done() {
        return Ok(RawTable::default());
    }
    let n = r.leb()? as usize;
    let mut names = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        names.push(String::from_utf8_lossy(r.string()?).into_owned());
    }
    let mut plans = Vec::with_capacity(n.min(4096));
    let mut types = Vec::with_capacity(n.min(4096));
    for name in &names {
        let ty = String::from_utf8_lossy(r.string()?).into_owned();
        let p = plan(&parse(&ty)).ok_or_else(|| DecodeError::ColumnTypeUnsupported { column: name.clone(), ty: ty.clone() })?;
        plans.push(p);
        types.push(ty);
    }
    let mut data: Vec<ColumnData> = plans.iter().map(empty_data).collect();
    let mut valid: Vec<Bitmap> = (0..n).map(|_| Bitmap::default()).collect();
    let mut rows = 0u64;
    while !r.done() && rows < max_rows {
        for c in 0..n {
            push(&mut r, &plans[c], &mut data[c], &mut valid[c], &names[c])?;
        }
        rows += 1;
    }
    let truncated = !r.done();
    let columns = names
        .into_iter()
        .zip(types)
        .zip(data.into_iter().zip(valid))
        .map(|((name, ty), (d, v))| Column::with_validity(name, ty, d, v))
        .collect();
    let mut t = RawTable::new(columns).ok_or(DecodeError::UnexpectedEnd)?;
    t.set_truncated(truncated);
    Ok(t)
}

/// Encoding helpers for tests and stub servers.
#[doc(hidden)]
pub mod encode {
    pub fn leb(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }
    pub fn string(s: &[u8], out: &mut Vec<u8>) {
        leb(s.len() as u64, out);
        out.extend_from_slice(s);
    }
    /// The header for `(name, type)` columns.
    pub fn header(cols: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        leb(cols.len() as u64, &mut out);
        for (n, _) in cols {
            string(n.as_bytes(), &mut out);
        }
        for (_, t) in cols {
            string(t.as_bytes(), &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::encode::*;
    use super::*;
    use crate::hook::Cell;

    #[test]
    fn decoder_round_trips_every_supported_type() {
        let cols = [
            ("i8", "Int8"),
            ("u64", "UInt64"),
            ("i64", "Int64"),
            ("i128", "Int128"),
            ("u256", "UInt256"),
            ("f32", "Float32"),
            ("b", "Bool"),
            ("s", "String"),
            ("fs", "FixedString(4)"),
            ("d", "Date"),
            ("d32", "Date32"),
            ("dt", "DateTime('UTC')"),
            ("dt64", "DateTime64(6)"),
            ("dec", "Decimal(18, 2)"),
            ("dec128", "Decimal(38, 4)"),
            ("dec256", "Decimal(76, 2)"),
            ("uuid", "UUID"),
            ("e", "Enum8('a' = 1, 'b' = 2)"),
            ("lc", "LowCardinality(Nullable(String))"),
            ("arr", "Array(Int32)"),
        ];
        let mut b = header(&cols);
        b.push(0xff); // Int8 -1
        b.extend(u64::MAX.to_le_bytes());
        b.extend(i64::MIN.to_le_bytes());
        b.extend((-170141183460469231731687303715884105728i128).to_le_bytes());
        let mut u256 = [0xffu8; 32];
        u256[31] = 0x7f;
        b.extend(u256);
        b.extend(1.5f32.to_le_bytes());
        b.push(1);
        string("アオモリ".as_bytes(), &mut b);
        b.extend(b"ab\0\0");
        b.extend(1u16.to_le_bytes());
        b.extend((-1i32).to_le_bytes());
        b.extend(60u32.to_le_bytes());
        b.extend(1_500_000i64.to_le_bytes());
        b.extend(12345i64.to_le_bytes());
        b.extend((-123456789012345678901234567890i128).to_le_bytes());
        let mut d256 = [0u8; 32];
        d256[0] = 150; // 1.50
        b.extend(d256);
        b.extend(0x0011223344556677u64.to_le_bytes());
        b.extend(0x8899aabbccddeeffu64.to_le_bytes());
        b.push(2);
        b.push(1); // lc: NULL
        leb(2, &mut b);
        b.extend(7i32.to_le_bytes());
        b.extend((-7i32).to_le_bytes());

        let t = decode(&b, 100).unwrap();
        assert_eq!(t.row_count(), 1);
        assert!(!t.truncated());
        let c = |n: &str| t.cell(0, t.column_index(n).unwrap());
        let s = |v: &str| Cell::Value(Scalar::Str(v.into()));
        assert_eq!(c("i8"), Cell::Value(Scalar::Int(-1)));
        assert_eq!(c("u64"), Cell::Value(Scalar::UInt(u64::MAX)), "UInt64 stays exact (D7)");
        assert_eq!(c("i64"), Cell::Value(Scalar::Int(i64::MIN)));
        assert_eq!(c("i128"), s("-170141183460469231731687303715884105728"), "Int128 is exact text (D7)");
        assert_eq!(c("u256"), s("57896044618658097711785492504343953926634992332820282019728792003956564819967"));
        assert_eq!(c("f32"), Cell::Value(Scalar::Float(1.5)));
        assert_eq!(c("b"), Cell::Value(Scalar::Bool(true)));
        assert_eq!(c("s"), s("アオモリ"));
        assert_eq!(c("fs"), s("ab"));
        assert_eq!(c("d"), Cell::Value(Scalar::Time(86_400_000)));
        assert_eq!(c("d32"), Cell::Value(Scalar::Time(-86_400_000)));
        assert_eq!(c("dt"), Cell::Value(Scalar::Time(60_000)));
        assert_eq!(c("dt64"), Cell::Value(Scalar::Time(1_500)));
        assert_eq!(c("dec"), Cell::Value(Scalar::Decimal { value: 12345, scale: 2 }), "Decimal is a scaled integer (D7)");
        assert_eq!(c("dec128"), Cell::Value(Scalar::Decimal { value: -123456789012345678901234567890, scale: 4 }));
        assert_eq!(c("dec256"), s("1.50"), "Decimal256 is exact text (D7)");
        assert_eq!(c("uuid"), s("00112233-4455-6677-8899-aabbccddeeff"));
        assert_eq!(c("e"), s("b"));
        assert_eq!(c("lc"), Cell::Null);
        assert_eq!(c("arr"), Cell::Value(Scalar::Str("[Int(7), Int(-7)]".into())));
    }

    #[test]
    fn lowcardinality_nullable_string_decodes_with_nulls() {
        let mut b = header(&[("k", "LowCardinality(Nullable(String))")]);
        for v in [Some("a'b"), None, Some("c")] {
            match v {
                Some(s) => {
                    b.push(0);
                    string(s.as_bytes(), &mut b);
                }
                None => b.push(1),
            }
        }
        let t = decode(&b, 100).unwrap();
        assert_eq!(t.row_count(), 3);
        assert_eq!(t.cell(0, 0), Cell::Value(Scalar::Str("a'b".into())));
        assert_eq!(t.cell(1, 0), Cell::Null);
        assert_eq!(t.cell(2, 0), Cell::Value(Scalar::Str("c".into())));
    }

    #[test]
    fn an_unsupported_type_names_its_column() {
        let b = header(&[("j", "Nothing")]);
        assert_eq!(decode(&b, 10).unwrap_err(), DecodeError::ColumnTypeUnsupported { column: "j".into(), ty: "Nothing".into() });
    }

    #[test]
    fn maps_and_tuples_read_as_clickhouse_text() {
        let mut b = header(&[("m", "Map(String, Nullable(UInt8))"), ("t", "Tuple(Float64, String, Array(UInt8))"), ("n", "UInt8")]);
        leb(2, &mut b);
        string(b"door", &mut b);
        b.extend([0, 7]);
        string(b"it's", &mut b);
        b.push(1);
        b.extend(0.25f64.to_le_bytes());
        string(b"ok", &mut b);
        leb(2, &mut b);
        b.extend([1, 2]);
        b.push(9);
        let t = decode(&b, 10).unwrap();
        assert_eq!(t.cell(0, 0), Cell::Value(Scalar::Str("{'door':7,'it\\'s':NULL}".into())));
        assert_eq!(t.cell(0, 1), Cell::Value(Scalar::Str("(0.25,'ok',[1,2])".into())));
        assert_eq!(t.cell(0, 2), Cell::Value(Scalar::UInt(9)), "the columns after them still line up");
    }

    #[test]
    fn cap_plus_one_rows_report_truncated() {
        let mut b = header(&[("n", "UInt64")]);
        for i in 0..4u64 {
            b.extend(i.to_le_bytes());
        }
        let t = decode(&b, 3).unwrap();
        assert_eq!(t.row_count(), 3);
        assert!(t.truncated());
        let t = decode(&b, 4).unwrap();
        assert!(!t.truncated());
    }

    #[test]
    fn a_short_body_is_an_error() {
        let mut b = header(&[("n", "UInt64")]);
        b.extend([1, 2, 3]);
        assert_eq!(decode(&b, 10).unwrap_err(), DecodeError::UnexpectedEnd);
    }
}
