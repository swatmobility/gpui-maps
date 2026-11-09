//! The fixture database: named tables of rows of [`Val`], held in memory.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::sql::Scalar;

/// One fixture value. Dates are `Str("YYYY-MM-DD")`; `List` appears only
/// for array parameters, `IN` lists and tuples.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Val>),
}

/// A `NULL` to borrow from where a row or an aggregate has no value.
pub(crate) static NULL: Val = Val::Null;

impl Val {
    /// True for [`Val::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }

    /// The value as a number: an `Int`, a `Float`, or a `Str` that reads
    /// as one. `None` for anything else.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Int(i) => Some(*i as f64),
            Val::Float(f) => Some(*f),
            Val::Str(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    }

    /// From a bound parameter's scalar. Booleans read as `0`/`1`, decimals
    /// and `UInt`s past `i64` as floats, and a `Time` as a date
    /// (`YYYY-MM-DD`) or, off midnight, a date-time.
    pub fn from_scalar(s: &Scalar) -> Val {
        match s {
            Scalar::Bool(b) => Val::Int(i64::from(*b)),
            Scalar::Int(i) => Val::Int(*i),
            Scalar::UInt(u) => i64::try_from(*u).map_or(Val::Float(*u as f64), Val::Int),
            Scalar::Float(f) => Val::Float(*f),
            Scalar::Decimal { .. } => s.as_f64().map_or(Val::Null, Val::Float),
            Scalar::Str(t) => Val::Str(t.clone()),
            Scalar::Time(ms) => Val::Str(time_text(*ms)),
        }
    }

    /// To a result scalar; `None` for `Null`. A list becomes its text.
    pub fn to_scalar(&self) -> Option<Scalar> {
        match self {
            Val::Null => None,
            Val::Int(i) => Some(Scalar::Int(*i)),
            Val::Float(f) => Some(Scalar::Float(*f)),
            Val::Str(s) => Some(Scalar::Str(s.clone())),
            Val::List(_) => Some(Scalar::Str(self.text())),
        }
    }

    /// The value as text: `""` for `Null`, numbers as Rust prints them
    /// (`3`, `2.5`), a list as `['a',1]`.
    pub fn text(&self) -> String {
        match self {
            Val::Null => String::new(),
            Val::Int(i) => i.to_string(),
            Val::Float(f) => f.to_string(),
            Val::Str(s) => s.clone(),
            Val::List(xs) => {
                let parts: Vec<String> = xs
                    .iter()
                    .map(|x| match x {
                        Val::Str(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
                        other => other.text(),
                    })
                    .collect();
                format!("[{}]", parts.join(","))
            }
        }
    }
}

/// `ms` since the epoch as `YYYY-MM-DD`, or `YYYY-MM-DD HH:MM:SS` when it
/// is not midnight UTC.
pub(crate) fn time_text(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000) / 1000;
    let (y, m, d) = civil_from_days(days);
    if rem == 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem / 60 % 60, rem % 60)
    }
}

/// Days since 1970-01-01 to `(year, month, day)` (H. Hinnant's algorithm).
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A total order over values: numbers numerically (an `Int` against a
/// `Float` too), a `Str` that reads as a number numerically against a
/// number, strings lexicographically, lists element by element. Across
/// kinds: numbers, then strings, then lists; `Null` sorts last.
pub fn cmp_vals(a: &Val, b: &Val) -> Ordering {
    fn rank(v: &Val) -> u8 {
        match v {
            Val::Int(_) | Val::Float(_) => 0,
            Val::Str(_) => 1,
            Val::List(_) => 2,
            Val::Null => 3,
        }
    }
    fn fcmp(x: f64, y: f64) -> Ordering {
        x.partial_cmp(&y).unwrap_or(Ordering::Equal)
    }
    // A numeric string against a number compares as a number.
    fn str_num(s: &str, n: &Val) -> Option<Ordering> {
        let t = s.trim();
        match n {
            Val::Int(i) => match t.parse::<i64>() {
                Ok(x) => Some(x.cmp(i)),
                Err(_) => t.parse::<f64>().ok().map(|x| fcmp(x, *i as f64)),
            },
            Val::Float(f) => t.parse::<f64>().ok().map(|x| fcmp(x, *f)),
            _ => None,
        }
    }
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => x.cmp(y),
        (Val::Int(x), Val::Float(y)) => fcmp(*x as f64, *y),
        (Val::Float(x), Val::Int(y)) => fcmp(*x, *y as f64),
        (Val::Float(x), Val::Float(y)) => fcmp(*x, *y),
        (Val::Str(x), Val::Str(y)) => x.cmp(y),
        (Val::Str(s), n @ (Val::Int(_) | Val::Float(_))) => str_num(s, n).unwrap_or(Ordering::Greater),
        (n @ (Val::Int(_) | Val::Float(_)), Val::Str(s)) => str_num(s, n).map_or(Ordering::Less, Ordering::reverse),
        (Val::List(x), Val::List(y)) => {
            for (p, q) in x.iter().zip(y) {
                let o = cmp_vals(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        _ => rank(a).cmp(&rank(b)),
    }
}

/// What a column's values are, for typing a result column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Base {
    Int,
    Float,
    Str,
    List,
    /// Every value is `Null` (or there are none).
    Null,
}

/// A column's [`Base`] and whether any value is `Null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnKind {
    pub base: Base,
    pub has_null: bool,
}

impl ColumnKind {
    /// The ClickHouse type text: `Int64`, `Float64`, `String`,
    /// `Array(String)`, wrapped in `Nullable(..)` when a value is null.
    pub(crate) fn sql_type(&self, list_elem: Base) -> String {
        let base = match self.base {
            Base::Int => "Int64",
            Base::Float => "Float64",
            Base::Str | Base::Null => "String",
            Base::List => match list_elem {
                Base::Int => "Array(Int64)",
                Base::Float => "Array(Float64)",
                _ => "Array(String)",
            },
        };
        if self.has_null {
            format!("Nullable({base})")
        } else {
            base.to_string()
        }
    }
}

/// Reads the kind of a column: any list makes it a list column, else any
/// string a string column, else any float a float column.
pub(crate) fn column_kind<'v>(vals: impl Iterator<Item = &'v Val>) -> ColumnKind {
    let (mut i, mut f, mut s, mut l, mut null) = (false, false, false, false, false);
    for v in vals {
        match v {
            Val::Null => null = true,
            Val::Int(_) => i = true,
            Val::Float(_) => f = true,
            Val::Str(_) => s = true,
            Val::List(_) => l = true,
        }
    }
    let base = if l {
        Base::List
    } else if s {
        Base::Str
    } else if f {
        Base::Float
    } else if i {
        Base::Int
    } else {
        Base::Null
    };
    ColumnKind { base, has_null: null }
}

/// One table: its columns (name and ClickHouse type text) and its rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<(String, String)>,
    pub rows: Vec<Vec<Val>>,
}

impl Table {
    /// An empty table with `(name, type)` columns.
    pub fn new(name: &str, columns: &[(&str, &str)]) -> Table {
        Table {
            name: name.to_string(),
            columns: columns.iter().map(|(n, t)| (n.to_string(), t.to_string())).collect(),
            rows: Vec::new(),
        }
    }

    /// Appends a row. A short row is padded with `Null` and a long one cut
    /// to the table's width.
    pub fn push(&mut self, mut row: Vec<Val>) {
        row.resize(self.columns.len(), Val::Null);
        self.rows.push(row);
    }

    /// The position of the column `name`.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(n, _)| n == name)
    }
}

/// Named tables.
#[derive(Debug, Clone, Default)]
pub struct Database {
    tables: BTreeMap<String, Table>,
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `t`, replacing any table of the same name.
    pub fn insert(&mut self, t: Table) {
        self.tables.insert(t.name.clone(), t);
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(name)
    }

    /// Every table, by name.
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_compare_across_kinds_and_null_sorts_last() {
        assert_eq!(cmp_vals(&Val::Int(2), &Val::Float(2.5)), Ordering::Less);
        assert_eq!(cmp_vals(&Val::Str("10".into()), &Val::Int(9)), Ordering::Greater);
        assert_eq!(cmp_vals(&Val::Int(9), &Val::Str("10".into())), Ordering::Less);
        assert_eq!(cmp_vals(&Val::Str("10".into()), &Val::Str("9".into())), Ordering::Less, "strings stay strings");
        assert_eq!(cmp_vals(&Val::Null, &Val::Int(1)), Ordering::Greater);
        assert_eq!(cmp_vals(&Val::Int(1), &Val::Null), Ordering::Less);
    }

    #[test]
    fn a_time_scalar_reads_as_a_date() {
        assert_eq!(Val::from_scalar(&Scalar::Time(0)), Val::Str("1970-01-01".into()));
        assert_eq!(Val::from_scalar(&Scalar::Time(19_000 * 86_400_000)), Val::Str("2022-01-08".into()));
        assert_eq!(Val::from_scalar(&Scalar::Time(3_600_000)), Val::Str("1970-01-01 01:00:00".into()));
    }
}
