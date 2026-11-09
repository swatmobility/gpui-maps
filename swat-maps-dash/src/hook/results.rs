//! `Results`: a hooked result, held in columns (D4). No public constructor,
//! no `Default`, no `Deserialize`; [`super::run_query`] is its only maker.

use serde::ser::{SerializeMap, SerializeSeq};
use serde::Serialize;

use super::table::{Cell, Column, ColumnData};

/// A result that passed its source's hook. Consumers (widgets, exports, the
/// agent, the map binding) take this and nothing else.
///
/// ```compile_fail
/// // No struct literal, no Default, no Deserialize: only run_query makes one.
/// let r = swat_maps_dash::hook::Results::default();
/// ```
///
/// ```compile_fail
/// let r: swat_maps_dash::hook::Results = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug)]
pub struct Results {
    columns: Vec<Column>,
    rows: usize,
    truncated: bool,
    suppressed_label: String,
}

/// A read-only view of one column (D4): its typed values and bitmaps.
#[derive(Debug, Clone, Copy)]
pub struct ColumnRef<'a> {
    col: &'a Column,
}

impl<'a> ColumnRef<'a> {
    pub fn name(&self) -> &'a str {
        self.col.name()
    }
    /// The server type, as returned (`LowCardinality(Nullable(String))`).
    pub fn sql_type(&self) -> &'a str {
        self.col.sql_type()
    }
    /// The typed values. A suppressed or null slot holds the type's zero.
    pub fn values(&self) -> &'a ColumnData {
        self.col.data()
    }
    pub fn len(&self) -> usize {
        self.col.len()
    }
    pub fn is_empty(&self) -> bool {
        self.col.is_empty()
    }
    pub fn is_null(&self, row: usize) -> bool {
        self.col.is_null(row)
    }
    pub fn is_suppressed(&self, row: usize) -> bool {
        self.col.is_suppressed(row)
    }
    pub fn cell(&self, row: usize) -> Cell {
        self.col.cell(row)
    }
}

impl Results {
    pub(crate) fn from_columns(columns: Vec<Column>, rows: usize, truncated: bool, suppressed_label: String) -> Results {
        Results { columns, rows, truncated, suppressed_label }
    }

    /// The columns, moved out (for a host converting to its own table
    /// inside this crate; consumers read [`Results::columns`]).
    #[allow(dead_code)]
    pub(crate) fn into_columns(self) -> Vec<Column> {
        self.columns
    }

    pub fn columns(&self) -> impl Iterator<Item = ColumnRef<'_>> {
        self.columns.iter().map(|col| ColumnRef { col })
    }

    pub fn column(&self, name: &str) -> Option<ColumnRef<'_>> {
        self.columns.iter().find(|c| c.name() == name).map(|col| ColumnRef { col })
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// True when the source stopped at the row cap with more rows left (D3).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// What a suppressed cell reads as ("under 10").
    pub fn suppressed_label(&self) -> &str {
        &self.suppressed_label
    }

    pub fn cell(&self, row: usize, col: &str) -> Option<Cell> {
        self.column(col).map(|c| c.cell(row))
    }

    /// The sum of a numeric column over hooked rows: `Suppressed` if any
    /// input is (§5.2), `Null` when there is nothing to add.
    pub fn sum(&self, col: &str) -> Option<Cell> {
        let c = self.column(col)?;
        let mut total = 0.0;
        let mut any = false;
        for r in 0..c.len() {
            match c.cell(r) {
                Cell::Suppressed => return Some(Cell::Suppressed),
                Cell::Value(v) => {
                    total += v.as_f64()?;
                    any = true;
                }
                Cell::Null => {}
            }
        }
        Some(if any { Cell::Value(crate::sql::Scalar::Float(total)) } else { Cell::Null })
    }

    /// Row order by a column. Values first (ascending or descending), then
    /// nulls, then suppressed cells in either direction, so a ranking never
    /// reveals where a suppressed value would fall (§5.2).
    pub fn order_by(&self, col: &str, descending: bool) -> Option<Vec<usize>> {
        let c = self.column(col)?;
        let mut idx: Vec<usize> = (0..c.len()).collect();
        let rank = |r: usize| match c.cell(r) {
            Cell::Value(_) => 0,
            Cell::Null => 1,
            Cell::Suppressed => 2,
        };
        idx.sort_by(|&a, &b| {
            rank(a).cmp(&rank(b)).then_with(|| {
                let ord = match (c.cell(a), c.cell(b)) {
                    (Cell::Value(x), Cell::Value(y)) => match (x.as_f64(), y.as_f64()) {
                        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                        _ => x.to_text().cmp(&y.to_text()),
                    },
                    _ => std::cmp::Ordering::Equal,
                };
                if descending {
                    ord.reverse()
                } else {
                    ord
                }
            })
        });
        Some(idx)
    }
}

impl Serialize for Results {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        struct Cols<'a>(&'a [Column]);
        impl Serialize for Cols<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut seq = s.serialize_seq(Some(self.0.len()))?;
                for c in self.0 {
                    seq.serialize_element(&serde_json::json!({ "name": c.name(), "type": c.sql_type() }))?;
                }
                seq.end()
            }
        }
        struct Rows<'a>(&'a [Column], usize);
        impl Serialize for Rows<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut seq = s.serialize_seq(Some(self.1))?;
                for r in 0..self.1 {
                    let row: Vec<Cell> = self.0.iter().map(|c| c.cell(r)).collect();
                    seq.serialize_element(&row)?;
                }
                seq.end()
            }
        }
        let mut m = s.serialize_map(Some(4))?;
        m.serialize_entry("columns", &Cols(&self.columns))?;
        m.serialize_entry("rows", &Rows(&self.columns, self.rows))?;
        m.serialize_entry("truncated", &self.truncated)?;
        m.serialize_entry("suppressed_label", &self.suppressed_label)?;
        m.end()
    }
}
