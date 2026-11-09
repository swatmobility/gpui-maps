//! A connection's catalogue (the database and table pickers of a SQL
//! editor): the fixed statements that list databases, tables and columns, the
//! readers of their results, and the `SELECT` a picked table generates.
//!
//! - The statements are constants. The database and table names a user
//!   picks are **bound parameters** (`{db:String}` for ClickHouse, `$1`
//!   for Postgres), never spliced into the text; each statement still
//!   passes the read guard on its way out ([`super::CompiledQuery::guarded`]).
//! - Postgres lists *schemas* where ClickHouse lists databases: the
//!   picker's "database" is the namespace a table is qualified with. Row
//!   counts come from `pg_class.reltuples` (an estimate; `-1` before the
//!   first `ANALYZE` reads as unknown).
//! - [`select_table`] lists every column by name: ClickHouse's `SELECT *`
//!   leaves out `MATERIALIZED` and `ALIAS` columns, which the catalogue
//!   does list. Identifiers are bare when plain and quoted per dialect
//!   otherwise.

use super::compile::{CompiledQuery, Param, ParamValue};
use super::value::Scalar;
use super::Dialect;
use crate::hook::{Cell, RawTable};

/// Databases (ClickHouse) and schemas (Postgres) the picker hides unless
/// the connection itself names one.
pub const HIDDEN_DATABASES: [&str; 5] = ["system", "INFORMATION_SCHEMA", "information_schema", "pg_catalog", "pg_toast"];

/// The most rows a catalogue statement reads.
pub const CATALOGUE_ROWS: u64 = 10_000;

/// One catalogue request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalogue<'a> {
    Databases,
    Tables { database: &'a str },
    Columns { database: &'a str, table: &'a str },
}

const CH_DATABASES: &str = "SELECT name FROM system.databases ORDER BY name";
const CH_TABLES: &str = "SELECT name, total_rows, total_bytes, engine FROM system.tables WHERE database = {db:String} ORDER BY name";
const CH_COLUMNS: &str = "SELECT name, type, default_kind FROM system.columns WHERE database = {db:String} AND table = {tbl:String} ORDER BY position";
const PG_DATABASES: &str = "SELECT schema_name AS name FROM information_schema.schemata WHERE schema_name NOT LIKE 'pg\\_temp%' AND schema_name NOT LIKE 'pg\\_toast\\_temp%' ORDER BY schema_name";
const PG_TABLES: &str = "SELECT t.table_name AS name, CASE WHEN c.reltuples < 0 THEN NULL ELSE CAST(c.reltuples AS bigint) END AS total_rows, \
     CASE WHEN c.oid IS NULL THEN NULL ELSE pg_total_relation_size(c.oid) END AS total_bytes, t.table_type AS engine \
     FROM information_schema.tables t \
     LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = t.table_schema \
     LEFT JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = t.table_name \
     WHERE t.table_schema = $1 ORDER BY t.table_name";
const PG_COLUMNS: &str = "SELECT column_name AS name, CASE WHEN data_type = 'USER-DEFINED' THEN udt_name ELSE data_type END AS type, \
     CASE WHEN is_generated = 'ALWAYS' THEN 'GENERATED' ELSE '' END AS default_kind \
     FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position";

impl Catalogue<'_> {
    /// The statement's text (constant per dialect and kind).
    pub fn text(&self, dialect: Dialect) -> &'static str {
        match (self, dialect) {
            (Catalogue::Databases, Dialect::ClickHouse) => CH_DATABASES,
            (Catalogue::Tables { .. }, Dialect::ClickHouse) => CH_TABLES,
            (Catalogue::Columns { .. }, Dialect::ClickHouse) => CH_COLUMNS,
            (Catalogue::Databases, Dialect::Postgres) => PG_DATABASES,
            (Catalogue::Tables { .. }, Dialect::Postgres) => PG_TABLES,
            (Catalogue::Columns { .. }, Dialect::Postgres) => PG_COLUMNS,
        }
    }

    /// The statement with its names bound as parameters, capped at
    /// [`CATALOGUE_ROWS`].
    pub fn compile(&self, dialect: Dialect) -> CompiledQuery {
        let (sql_ty, pg_ty) = ("String", "text");
        let bind = |name: &str, v: &str| Param {
            name: name.to_string(),
            ty: if dialect == Dialect::ClickHouse { sql_ty.into() } else { pg_ty.into() },
            value: ParamValue::One(Scalar::Str(v.to_string())),
        };
        let params = match *self {
            Catalogue::Databases => Vec::new(),
            Catalogue::Tables { database } => vec![bind("db", database)],
            Catalogue::Columns { database, table } => vec![bind("db", database), bind("tbl", table)],
        };
        CompiledQuery::new(self.text(dialect).to_string(), params, dialect, CATALOGUE_ROWS, Vec::new())
    }
}

/// A table as the catalogue lists it. `rows` and `bytes` are `None` when
/// the server does not know them (a view, a `Merge` table, an unanalysed
/// Postgres table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub name: String,
    pub rows: Option<u64>,
    pub bytes: Option<u64>,
    /// The ClickHouse engine, or the Postgres `table_type`.
    pub engine: String,
}

/// A column as the catalogue lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// The server's type text (`Nullable(Float64)`, `geometry`, …).
    pub ty: String,
    /// `MATERIALIZED`, `ALIAS`, `DEFAULT`, `EPHEMERAL`, `GENERATED` or empty.
    pub default_kind: String,
}

fn text_at(t: &RawTable, row: usize, col: Option<usize>) -> Option<String> {
    match t.cell(row, col?) {
        Cell::Value(Scalar::Str(s)) => Some(s),
        Cell::Value(v) => Some(v.to_text()),
        _ => None,
    }
}

fn count_at(t: &RawTable, row: usize, col: Option<usize>) -> Option<u64> {
    match t.cell(row, col?) {
        Cell::Value(Scalar::UInt(n)) => Some(n),
        Cell::Value(Scalar::Int(n)) => u64::try_from(n).ok(),
        Cell::Value(Scalar::Float(f)) if f.is_finite() && f >= 0.0 => Some(f as u64),
        Cell::Value(Scalar::Decimal { value, scale }) if value >= 0 => u64::try_from(value / 10i128.pow(scale as u32)).ok(),
        Cell::Value(Scalar::Str(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The databases in a [`Catalogue::Databases`] result, the hidden ones
/// left out unless `keep` names one (the connection's own database).
pub fn databases(t: &RawTable, keep: Option<&str>) -> Vec<String> {
    database_names(t).into_iter().filter(|n| !HIDDEN_DATABASES.contains(&n.as_str()) || keep == Some(n.as_str())).collect()
}

/// Every database in a [`Catalogue::Databases`] result, the hidden ones
/// included (the Connections form's "Show system").
pub fn database_names(t: &RawTable) -> Vec<String> {
    let c = t.column_index("name").or(Some(0));
    (0..t.row_count()).filter_map(|r| text_at(t, r, c)).collect()
}

/// The tables in a [`Catalogue::Tables`] result.
pub fn tables(t: &RawTable) -> Vec<TableInfo> {
    let (n, rows, bytes, engine) = (t.column_index("name"), t.column_index("total_rows"), t.column_index("total_bytes"), t.column_index("engine"));
    (0..t.row_count())
        .filter_map(|r| {
            Some(TableInfo { name: text_at(t, r, n)?, rows: count_at(t, r, rows), bytes: count_at(t, r, bytes), engine: text_at(t, r, engine).unwrap_or_default() })
        })
        .collect()
}

/// The columns in a [`Catalogue::Columns`] result.
pub fn columns(t: &RawTable) -> Vec<ColumnInfo> {
    let (n, ty, kind) = (t.column_index("name"), t.column_index("type"), t.column_index("default_kind"));
    (0..t.row_count())
        .filter_map(|r| Some(ColumnInfo { name: text_at(t, r, n)?, ty: text_at(t, r, ty).unwrap_or_default(), default_kind: text_at(t, r, kind).unwrap_or_default() }))
        .collect()
}

/// Words a bare identifier may not be (the clauses a generated `SELECT`
/// could be misread with, in either dialect).
const RESERVED: [&str; 40] = [
    "all", "and", "any", "array", "as", "asc", "between", "by", "case", "cast", "cross", "default", "desc", "distinct", "else", "end", "except", "false",
    "final", "from", "full", "group", "having", "in", "inner", "interval", "into", "is", "join", "left", "like", "limit", "not", "null", "offset", "on",
    "or", "order", "select", "where",
];

/// `s` as an identifier in `dialect`: bare when it is plain (a letter or
/// `_`, then letters, digits and `_`; lower case only for Postgres, which
/// folds bare names) and not a reserved word, quoted otherwise.
pub fn ident(dialect: Dialect, s: &str) -> String {
    let mut chars = s.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !(dialect == Dialect::Postgres && s.chars().any(|c| c.is_ascii_uppercase()))
        && !RESERVED.contains(&s.to_ascii_lowercase().as_str());
    if plain {
        s.to_string()
    } else {
        dialect.quote_ident(s)
    }
}

/// The `SELECT` for a picked table: every column by name, one per line,
/// the table qualified with its database, and `LIMIT limit`. With no
/// columns known yet it reads `*`.
pub fn select_table(dialect: Dialect, database: &str, table: &str, columns: &[ColumnInfo], limit: u64) -> String {
    let list = if columns.is_empty() { "    *".to_string() } else { columns.iter().map(|c| format!("    {}", ident(dialect, &c.name))).collect::<Vec<_>>().join(",\n") };
    let from = if database.is_empty() { ident(dialect, table) } else { format!("{}.{}", ident(dialect, database), ident(dialect, table)) };
    format!("SELECT\n{list}\nFROM {from}\nLIMIT {limit}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::rowbinary::{decode, encode};

    fn col(name: &str, ty: &str, kind: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), ty: ty.into(), default_kind: kind.into() }
    }

    #[test]
    fn statements_bind_names_as_parameters_and_pass_the_guard() {
        let evil = "x' OR 1=1; DROP TABLE t; --";
        for dialect in [Dialect::ClickHouse, Dialect::Postgres] {
            for c in [Catalogue::Databases, Catalogue::Tables { database: evil }, Catalogue::Columns { database: evil, table: evil }] {
                let q = c.compile(dialect);
                let g = q.guarded(dialect).unwrap_or_else(|e| panic!("{c:?} {dialect:?}: {e:?}"));
                assert!(!g.sql().contains(evil), "a name is never in the text");
                assert_eq!(g.sql(), c.text(dialect));
                assert!(q.params().iter().all(|p| p.value == ParamValue::One(Scalar::Str(evil.into()))));
                assert_eq!(q.max_rows(), CATALOGUE_ROWS);
            }
        }
        let q = Catalogue::Columns { database: "d", table: "t" }.compile(Dialect::ClickHouse);
        assert_eq!(q.params().iter().map(|p| (p.name.as_str(), p.ty.as_str())).collect::<Vec<_>>(), [("db", "String"), ("tbl", "String")]);
        assert!(CH_COLUMNS.contains("{db:String}") && CH_COLUMNS.contains("{tbl:String}"));
        assert!(PG_COLUMNS.contains("$1") && PG_COLUMNS.contains("$2"));
        assert_eq!(Catalogue::Tables { database: "d" }.compile(Dialect::Postgres).params()[0].ty, "text");
    }

    #[test]
    fn identifiers_are_bare_when_plain_and_quoted_per_dialect() {
        assert_eq!(ident(Dialect::ClickHouse, "pickup_lat"), "pickup_lat");
        assert_eq!(ident(Dialect::ClickHouse, "PickupLat"), "PickupLat");
        assert_eq!(ident(Dialect::Postgres, "PickupLat"), "\"PickupLat\"", "Postgres folds bare names");
        assert_eq!(ident(Dialect::ClickHouse, "from"), "`from`");
        assert_eq!(ident(Dialect::Postgres, "order"), "\"order\"");
        assert_eq!(ident(Dialect::ClickHouse, "trip id"), "`trip id`");
        assert_eq!(ident(Dialect::ClickHouse, "a`b"), "`a\\`b`");
        assert_eq!(ident(Dialect::Postgres, "a\"b"), "\"a\"\"b\"");
        assert_eq!(ident(Dialect::ClickHouse, "1st"), "`1st`");
        assert_eq!(ident(Dialect::ClickHouse, "名前"), "`名前`");
    }

    #[test]
    fn a_picked_table_enumerates_every_column_with_the_limit() {
        let cols = [col("trip_id", "UInt64", ""), col("pickup_lat", "Float64", ""), col("h3_cell", "UInt64", "MATERIALIZED"), col("day", "Date", "ALIAS"), col("select", "String", "")];
        let sql = select_table(Dialect::ClickHouse, "synthetic_db", "trips", &cols, 10_000);
        assert_eq!(sql, "SELECT\n    trip_id,\n    pickup_lat,\n    h3_cell,\n    day,\n    `select`\nFROM synthetic_db.trips\nLIMIT 10000");
        assert!(!sql.contains('*'), "MATERIALIZED and ALIAS columns are listed by name");
        let pg = select_table(Dialect::Postgres, "Public Data", "Trips", &cols[..2], 1_000);
        assert_eq!(pg, "SELECT\n    trip_id,\n    pickup_lat\nFROM \"Public Data\".\"Trips\"\nLIMIT 1000");
        assert_eq!(select_table(Dialect::ClickHouse, "", "t", &[], 5), "SELECT\n    *\nFROM t\nLIMIT 5");
        // The generated text is one guarded read.
        let (q, _) = CompiledQuery::plain(&sql, Dialect::ClickHouse, &Default::default(), 10_000).unwrap();
        assert!(q.guarded(Dialect::ClickHouse).is_ok());
    }

    #[test]
    fn catalogue_results_are_read_by_column_name() {
        let mut b = encode::header(&[("name", "String")]);
        for n in ["default", "INFORMATION_SCHEMA", "synthetic_db", "system", "information_schema"] {
            encode::string(n.as_bytes(), &mut b);
        }
        let t = decode(&b, 100).unwrap();
        assert_eq!(databases(&t, None), ["default", "synthetic_db"]);
        assert_eq!(databases(&t, Some("system")), ["default", "synthetic_db", "system"], "the connection's own database stays");

        let mut b = encode::header(&[("name", "String"), ("total_rows", "Nullable(UInt64)"), ("total_bytes", "Nullable(UInt64)"), ("engine", "String")]);
        for (n, rows, bytes, e) in [("trips", Some(1_200u64), Some(48_000u64), "MergeTree"), ("trips_view", None, None, "View")] {
            encode::string(n.as_bytes(), &mut b);
            for v in [rows, bytes] {
                match v {
                    Some(v) => {
                        b.push(0);
                        b.extend(v.to_le_bytes());
                    }
                    None => b.push(1),
                }
            }
            encode::string(e.as_bytes(), &mut b);
        }
        let t = decode(&b, 100).unwrap();
        assert_eq!(
            tables(&t),
            [
                TableInfo { name: "trips".into(), rows: Some(1_200), bytes: Some(48_000), engine: "MergeTree".into() },
                TableInfo { name: "trips_view".into(), rows: None, bytes: None, engine: "View".into() },
            ]
        );

        let mut b = encode::header(&[("name", "String"), ("type", "String"), ("default_kind", "String")]);
        for (n, ty, k) in [("trip_id", "UInt64", ""), ("h3", "UInt64", "MATERIALIZED")] {
            for s in [n, ty, k] {
                encode::string(s.as_bytes(), &mut b);
            }
        }
        let t = decode(&b, 100).unwrap();
        assert_eq!(columns(&t), [col("trip_id", "UInt64", ""), col("h3", "UInt64", "MATERIALIZED")]);
    }

    #[test]
    fn postgres_counts_read_from_signed_and_text_cells() {
        let mut b = encode::header(&[("name", "String"), ("total_rows", "Int64"), ("total_bytes", "String")]);
        encode::string(b"t", &mut b);
        b.extend((-1i64).to_le_bytes());
        encode::string(b"8192", &mut b);
        let t = decode(&b, 10).unwrap();
        assert_eq!(tables(&t)[0], TableInfo { name: "t".into(), rows: None, bytes: Some(8_192), engine: String::new() });
    }
}
