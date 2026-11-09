//! Column-stored tables (explorer-sql D4, D7).
//!
//! Each column is one typed vector plus a validity bitmap (null) and a
//! suppressed bitmap. [`Cell`] stays the per-cell accessor. Strings live in
//! one byte buffer with offsets, so decoding a million rows allocates per
//! column, not per row. Wide numbers stay exact (D7): `Int64`/`UInt64` are
//! integers, `Decimal32/64/128` a scaled `i128`, and `Decimal256`,
//! `Int128/256`, `UInt128/256` exact text.

use serde::Serialize;

use crate::sql::Scalar;

/// A bitmap, one bit per row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    pub fn push(&mut self, bit: bool) {
        if self.len.is_multiple_of(64) {
            self.words.push(0);
        }
        if bit {
            self.words[self.len / 64] |= 1 << (self.len % 64);
        }
        self.len += 1;
    }

    pub fn get(&self, i: usize) -> bool {
        i < self.len && self.words[i / 64] & (1 << (i % 64)) != 0
    }

    pub fn set(&mut self, i: usize, bit: bool) {
        if i >= self.len {
            return;
        }
        if bit {
            self.words[i / 64] |= 1 << (i % 64);
        } else {
            self.words[i / 64] &= !(1 << (i % 64));
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn any(&self) -> bool {
        self.words.iter().any(|w| *w != 0)
    }

    fn with_len(len: usize, bit: bool) -> Bitmap {
        let mut b = Bitmap::default();
        for _ in 0..len {
            b.push(bit);
        }
        b
    }
}

/// Strings in one buffer: row `i` is `bytes[offsets[i]..offsets[i + 1]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct StrColumn {
    bytes: Vec<u8>,
    offsets: Vec<usize>,
}

impl Default for StrColumn {
    fn default() -> Self {
        StrColumn { bytes: Vec::new(), offsets: vec![0] }
    }
}

impl StrColumn {
    pub fn push(&mut self, s: &str) {
        self.bytes.extend_from_slice(s.as_bytes());
        self.offsets.push(self.bytes.len());
    }

    /// Appends raw bytes, which must be UTF-8 (the decoder checks).
    pub(crate) fn push_bytes(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
        self.offsets.push(self.bytes.len());
    }

    pub fn get(&self, i: usize) -> Option<&str> {
        let (a, b) = (*self.offsets.get(i)?, *self.offsets.get(i + 1)?);
        std::str::from_utf8(&self.bytes[a..b]).ok()
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Overwrites row `i`'s bytes with zeros in place: the old value is gone
    /// from the column's memory, not just hidden.
    fn wipe(&mut self, i: usize) {
        if let (Some(&a), Some(&b)) = (self.offsets.get(i), self.offsets.get(i + 1)) {
            self.bytes[a..b].fill(0);
        }
    }

    /// The raw bytes of row `i`, wiped or not (for tests that read the slot).
    #[doc(hidden)]
    pub fn raw_slot(&self, i: usize) -> &[u8] {
        &self.bytes[self.offsets[i]..self.offsets[i + 1]]
    }
}

/// A column's values, one typed vector.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    Bool(Vec<bool>),
    Int(Vec<i64>),
    UInt(Vec<u64>),
    Float(Vec<f64>),
    /// `Decimal32/64/128`: unscaled integers and the scale, exact (D7).
    Decimal { scale: u8, values: Vec<i128> },
    /// Strings, UUIDs, and exact text for the widest numbers (D7).
    Str(StrColumn),
    /// Dates and date-times as milliseconds since the epoch, UTC.
    Time(Vec<i64>),
    /// `Array(T)`, one list per row (arrays are rare in results).
    List(Vec<Vec<Scalar>>),
}

impl ColumnData {
    pub fn len(&self) -> usize {
        match self {
            ColumnData::Bool(v) => v.len(),
            ColumnData::Int(v) | ColumnData::Time(v) => v.len(),
            ColumnData::UInt(v) => v.len(),
            ColumnData::Float(v) => v.len(),
            ColumnData::Decimal { values, .. } => values.len(),
            ColumnData::Str(s) => s.len(),
            ColumnData::List(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn scalar(&self, i: usize) -> Option<Scalar> {
        Some(match self {
            ColumnData::Bool(v) => Scalar::Bool(*v.get(i)?),
            ColumnData::Int(v) => Scalar::Int(*v.get(i)?),
            ColumnData::UInt(v) => Scalar::UInt(*v.get(i)?),
            ColumnData::Float(v) => Scalar::Float(*v.get(i)?),
            ColumnData::Decimal { scale, values } => Scalar::Decimal { value: *values.get(i)?, scale: *scale },
            ColumnData::Str(s) => Scalar::Str(s.get(i)?.to_string()),
            ColumnData::Time(v) => Scalar::Time(*v.get(i)?),
            ColumnData::List(v) => Scalar::Str(format!("{:?}", v.get(i)?)),
        })
    }

    /// Replaces row `i`'s value with the type's zero, in place.
    fn erase(&mut self, i: usize) {
        match self {
            ColumnData::Bool(v) => v[i] = false,
            ColumnData::Int(v) | ColumnData::Time(v) => v[i] = 0,
            ColumnData::UInt(v) => v[i] = 0,
            ColumnData::Float(v) => v[i] = 0.0,
            ColumnData::Decimal { values, .. } => values[i] = 0,
            ColumnData::Str(s) => s.wipe(i),
            ColumnData::List(v) => v[i] = Vec::new(),
        }
    }
}

/// One cell, as read: a value, a null, or a value the hook suppressed.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Value(Scalar),
    Null,
    /// Withheld by the hook. Renderers show the hook's label ("under 10"),
    /// never 0 and never blank (§5.2).
    Suppressed,
}

impl Serialize for Cell {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Cell::Value(v) => v.serialize(s),
            Cell::Null => s.serialize_none(),
            Cell::Suppressed => {
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("suppressed", &true)?;
                m.end()
            }
        }
    }
}

/// One column: its name, its server type as returned, its values and the
/// two bitmaps.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    name: String,
    sql_type: String,
    data: ColumnData,
    valid: Bitmap,
    suppressed: Bitmap,
}

impl Column {
    /// A column whose rows are all valid and none suppressed.
    pub fn new(name: impl Into<String>, sql_type: impl Into<String>, data: ColumnData) -> Column {
        let n = data.len();
        Column { name: name.into(), sql_type: sql_type.into(), data, valid: Bitmap::with_len(n, true), suppressed: Bitmap::with_len(n, false) }
    }

    /// A column with an explicit validity bitmap (`false` = null).
    pub fn with_validity(name: impl Into<String>, sql_type: impl Into<String>, data: ColumnData, valid: Bitmap) -> Column {
        let n = data.len();
        Column { name: name.into(), sql_type: sql_type.into(), data, valid, suppressed: Bitmap::with_len(n, false) }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn sql_type(&self) -> &str {
        &self.sql_type
    }
    pub fn data(&self) -> &ColumnData {
        &self.data
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub fn is_null(&self, row: usize) -> bool {
        !self.valid.get(row)
    }
    pub fn is_suppressed(&self, row: usize) -> bool {
        self.suppressed.get(row)
    }

    pub fn cell(&self, row: usize) -> Cell {
        if self.is_suppressed(row) {
            Cell::Suppressed
        } else if self.is_null(row) {
            Cell::Null
        } else {
            self.data.scalar(row).map_or(Cell::Null, Cell::Value)
        }
    }
}

/// A source's raw result. Public and inert: sources and hooks build and edit
/// it, and nothing a consumer takes accepts it (§5.1). Only
/// [`super::run_query`] turns one into [`super::Results`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawTable {
    columns: Vec<Column>,
    rows: usize,
    truncated: bool,
}

impl RawTable {
    /// A table from columns of equal length; `None` if they differ.
    pub fn new(columns: Vec<Column>) -> Option<RawTable> {
        let rows = columns.first().map_or(0, Column::len);
        columns.iter().all(|c| c.len() == rows && c.valid.len() == rows).then_some(RawTable { columns, rows, truncated: false })
    }

    /// Marks that the source stopped at its row cap with more rows left (D3).
    pub fn set_truncated(&mut self, t: bool) {
        self.truncated = t;
    }
    pub fn truncated(&self) -> bool {
        self.truncated
    }
    pub fn row_count(&self) -> usize {
        self.rows
    }
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.columns.get(col).map_or(Cell::Null, |c| c.cell(row))
    }

    /// Suppresses one cell. The value is replaced in the column's memory
    /// (zeroed in place), so it is gone, not hidden (D4).
    pub fn set_suppressed(&mut self, row: usize, col: usize) {
        if let Some(c) = self.columns.get_mut(col) {
            if row < c.len() {
                c.data.erase(row);
                c.suppressed.set(row, true);
            }
        }
    }

    /// Drops a column (a hook removing what a consumer may not see).
    pub fn drop_column(&mut self, name: &str) {
        self.columns.retain(|c| c.name != name);
    }

    pub(crate) fn into_columns(self) -> (Vec<Column>, usize, bool) {
        (self.columns, self.rows, self.truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> RawTable {
        let mut s = StrColumn::default();
        s.push("mesh-a");
        s.push("SECRET-7777777");
        RawTable::new(vec![
            Column::new("k", "String", ColumnData::Str(s)),
            Column::new("n", "UInt64", ColumnData::UInt(vec![12, 7_777_777])),
            Column::new("d", "Decimal(18, 2)", ColumnData::Decimal { scale: 2, values: vec![150, 7_777_777] }),
        ])
        .unwrap()
    }

    #[test]
    fn a_suppressed_cell_holds_no_value() {
        let mut t = table();
        t.set_suppressed(1, 0);
        t.set_suppressed(1, 1);
        t.set_suppressed(1, 2);
        assert_eq!(t.cell(1, 0), Cell::Suppressed);
        assert_eq!(t.cell(1, 1), Cell::Suppressed);
        // Read the slots themselves: the old values are gone from memory.
        let ColumnData::Str(s) = t.columns()[0].data() else { panic!() };
        assert!(s.raw_slot(1).iter().all(|b| *b == 0), "string bytes wiped");
        assert_eq!(s.get(0), Some("mesh-a"), "neighbours untouched");
        assert_eq!(t.columns()[1].data(), &ColumnData::UInt(vec![12, 0]));
        assert_eq!(t.columns()[2].data(), &ColumnData::Decimal { scale: 2, values: vec![150, 0] });
        assert_eq!(t.cell(0, 1), Cell::Value(Scalar::UInt(12)));
    }

    #[test]
    fn columnar_round_trip_every_type() {
        let mut s = StrColumn::default();
        s.push("a");
        let mut wide = StrColumn::default();
        wide.push("170141183460469231731687303715884105727");
        let mut valid = Bitmap::default();
        valid.push(false);
        let cols = vec![
            Column::new("b", "Bool", ColumnData::Bool(vec![true])),
            Column::new("i", "Int64", ColumnData::Int(vec![i64::MIN])),
            Column::new("u", "UInt64", ColumnData::UInt(vec![u64::MAX])),
            Column::new("f", "Float64", ColumnData::Float(vec![1.5])),
            Column::new("d", "Decimal(38, 4)", ColumnData::Decimal { scale: 4, values: vec![-12345] }),
            Column::new("s", "String", ColumnData::Str(s)),
            Column::new("w", "Int128", ColumnData::Str(wide)),
            Column::new("t", "DateTime", ColumnData::Time(vec![86_400_000])),
            Column::new("l", "Array(Int32)", ColumnData::List(vec![vec![Scalar::Int(1)]])),
            Column::with_validity("z", "Nullable(String)", ColumnData::Str(StrColumn { bytes: vec![], offsets: vec![0, 0] }), valid),
        ];
        let t = RawTable::new(cols).unwrap();
        let cells: Vec<Cell> = (0..t.columns().len()).map(|c| t.cell(0, c)).collect();
        assert_eq!(
            cells,
            vec![
                Cell::Value(Scalar::Bool(true)),
                Cell::Value(Scalar::Int(i64::MIN)),
                Cell::Value(Scalar::UInt(u64::MAX)),
                Cell::Value(Scalar::Float(1.5)),
                Cell::Value(Scalar::Decimal { value: -12345, scale: 4 }),
                Cell::Value(Scalar::Str("a".into())),
                Cell::Value(Scalar::Str("170141183460469231731687303715884105727".into())),
                Cell::Value(Scalar::Time(86_400_000)),
                Cell::Value(Scalar::Str("[Int(1)]".into())),
                Cell::Null,
            ]
        );
        assert_eq!(serde_json::to_string(&cells[4]).unwrap(), "\"-1.2345\"", "decimals serialise exactly");
        assert_eq!(serde_json::to_string(&cells[2]).unwrap(), "18446744073709551615");
    }

    #[test]
    fn columns_of_unequal_length_are_refused() {
        assert!(RawTable::new(vec![
            Column::new("a", "Int64", ColumnData::Int(vec![1])),
            Column::new("b", "Int64", ColumnData::Int(vec![])),
        ])
        .is_none());
    }
}
