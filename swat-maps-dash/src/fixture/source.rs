//! [`FixtureSource`]: a [`Source`] over a fixture [`Database`], so a page's
//! real compiled SQL and bound parameters drive deterministic results with
//! no server.

use std::sync::{Arc, Mutex};

use super::db::{column_kind, Base, Database, Val, NULL};
use super::eval::{execute, ResultSet};
use crate::connect::{HttpRequest, HttpResponse, HttpTransport, SafeError, TransportError};
use crate::hook::{Bitmap, Column, ColumnData, RawTable, Schema, Source, SourceError};
use crate::sql::{CompiledQuery, Dialect};
use crate::BoxFuture;

/// One statement a [`FixtureSource`] ran: the guarded SQL text and each
/// parameter as ClickHouse would receive it (`param_<name>`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutedQuery {
    pub sql: String,
    pub params: Vec<(String, String)>,
}

/// A source that answers from an in-memory [`Database`]. It speaks the
/// ClickHouse dialect: every query passes the read guard first, exactly as
/// it would on its way to a server.
pub struct FixtureSource {
    db: Arc<Database>,
    schema: Schema,
    log: Arc<Mutex<Vec<ExecutedQuery>>>,
}

impl FixtureSource {
    pub fn new(db: Arc<Database>, schema: Schema) -> Self {
        FixtureSource { db, schema, log: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Every statement run so far, in order.
    pub fn log(&self) -> Arc<Mutex<Vec<ExecutedQuery>>> {
        Arc::clone(&self.log)
    }

    fn run(&self, q: &CompiledQuery) -> Result<RawTable, SourceError> {
        let g = q.guarded(Dialect::ClickHouse).map_err(SourceError::Refused)?;
        let entry = ExecutedQuery {
            sql: g.sql().to_string(),
            params: g.params().iter().map(|p| (p.name.clone(), p.value.clickhouse_text())).collect(),
        };
        self.log.lock().unwrap_or_else(|e| e.into_inner()).push(entry);
        let mut rs = execute(&self.db, g.sql(), g.params()).map_err(|e| {
            SourceError::Failed(SafeError {
                connection: "fixture".into(),
                code: "fixture_eval",
                status: None,
                server_code: None,
                message: e.to_string(),
            })
        })?;
        let cap = usize::try_from(q.max_rows()).unwrap_or(usize::MAX);
        let truncated = rs.rows.len() > cap;
        rs.rows.truncate(cap);
        let mut t = to_raw_table(&rs);
        t.set_truncated(truncated);
        Ok(t)
    }
}

impl Source for FixtureSource {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn execute<'a>(&'a self, q: &'a CompiledQuery, _t: &'a dyn HttpTransport) -> BoxFuture<'a, Result<RawTable, SourceError>> {
        Box::pin(std::future::ready(self.run(q)))
    }
}

/// A transport for fixture pages: it never sends anything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTransport;

impl HttpTransport for NoTransport {
    fn send(&self, _req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::Io("fixtures send nothing".into()))))
    }
}

/// A [`ResultSet`] as a column-stored [`RawTable`]. Each column is typed
/// from its values: any list makes `Array(..)`, else any string `String`,
/// else any float `Float64`, else `Int64`; a column of only `NULL`s is
/// `Nullable(String)`, and any `NULL` wraps the type in `Nullable(..)`.
pub fn to_raw_table(rs: &ResultSet) -> RawTable {
    let n = rs.rows.len();
    let columns = rs
        .columns
        .iter()
        .enumerate()
        .map(|(c, (name, _))| {
            let cell = |r: usize| rs.rows[r].get(c).unwrap_or(&NULL);
            let kind = column_kind((0..n).map(cell));
            let mut valid = Bitmap::default();
            for r in 0..n {
                valid.push(!cell(r).is_null());
            }
            let mut elem = Base::Null;
            let data = match kind.base {
                Base::Int => ColumnData::Int((0..n).map(|r| if let Val::Int(i) = cell(r) { *i } else { 0 }).collect()),
                Base::Float => ColumnData::Float((0..n).map(|r| cell(r).as_f64().unwrap_or(0.0)).collect()),
                Base::List => {
                    let elems = (0..n).filter_map(|r| match cell(r) {
                        Val::List(xs) => Some(xs.iter()),
                        _ => None,
                    });
                    elem = column_kind(elems.flatten()).base;
                    ColumnData::List(
                        (0..n)
                            .map(|r| match cell(r) {
                                Val::Null => Vec::new(),
                                Val::List(xs) => xs.iter().filter_map(Val::to_scalar).collect(),
                                other => other.to_scalar().into_iter().collect(),
                            })
                            .collect(),
                    )
                }
                Base::Str | Base::Null => {
                    let mut s = crate::hook::StrColumn::default();
                    for r in 0..n {
                        s.push(&cell(r).text());
                    }
                    ColumnData::Str(s)
                }
            };
            let ty = kind.sql_type(elem);
            Column::with_validity(name.clone(), ty, data, valid)
        })
        .collect();
    RawTable::new(columns).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::fixture::db::Table;
    use crate::hook::{run_query, Cell, GuardedSource, NoHook, Privacy, SourceRegistry};
    use crate::spec::Page;
    use crate::sql::{check_page, CheckOptions, QueryKey, Scalar, VarValue};

    fn db() -> Arc<Database> {
        let mut t = Table::new("t", &[("stop", "String"), ("n", "UInt32")]);
        for (stop, n) in [("A", 3), ("B", 5), ("A", 4), ("C", 1)] {
            t.push(vec![Val::Str(stop.into()), Val::Int(n)]);
        }
        let mut db = Database::new();
        db.insert(t);
        Arc::new(db)
    }

    const PAGE: &str = r#"{ "id": "p", "title": "P", "source": "fixture",
        "variables": [{ "key": "stop", "control": "multi" }],
        "filters": [{ "var": "stop", "column": "stop" }],
        "widgets": [{ "id": "w", "type": "bar",
            "sql": "SELECT stop, sum(n) AS passengers FROM t ${where} GROUP BY stop ORDER BY passengers DESC" }] }"#;

    #[test]
    fn a_page_query_runs_against_the_fixture_with_its_filter() {
        let (page, _) = Page::from_json(PAGE).unwrap();
        let values: BTreeMap<String, VarValue> =
            [("stop".to_string(), VarValue::Many(vec![Scalar::Str("A".into()), Scalar::Str("C".into())]))].into_iter().collect();
        let q = check_page(&page, &CheckOptions::default())
            .compile(&page, &QueryKey::Widget("w".into()), &values, &BTreeMap::new())
            .unwrap()
            .unwrap();
        let src = FixtureSource::new(db(), Schema::default());
        let log = src.log();
        let mut registry = SourceRegistry::new();
        registry.register("fixture", GuardedSource::new(Arc::new(src), Arc::new(NoHook), Privacy::Open));
        let res = crate::block_on(run_query(&registry, "fixture", &q, &NoTransport, "en")).unwrap();
        assert_eq!(res.row_count(), 2);
        assert_eq!(res.cell(0, "stop"), Some(Cell::Value(Scalar::Str("A".into()))));
        assert_eq!(res.cell(0, "passengers"), Some(Cell::Value(Scalar::Int(7))));
        assert_eq!(res.cell(1, "stop"), Some(Cell::Value(Scalar::Str("C".into()))));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].sql.contains("stop IN {f_stop:Array(String)}"), "{}", log[0].sql);
        assert_eq!(log[0].params, vec![("f_stop".to_string(), "['A','C']".to_string())]);
    }

    #[test]
    fn the_row_cap_truncates_and_says_so() {
        let (page, _) = Page::from_json(PAGE).unwrap();
        let q = check_page(&page, &CheckOptions::default())
            .compile(&page, &QueryKey::Widget("w".into()), &BTreeMap::new(), &BTreeMap::new())
            .unwrap()
            .unwrap()
            .with_max_rows(2);
        let t = crate::block_on(FixtureSource::new(db(), Schema::default()).execute(&q, &NoTransport)).unwrap();
        assert_eq!(t.row_count(), 2);
        assert!(t.truncated());
    }

    #[test]
    fn an_eval_error_fails_with_a_fixture_code() {
        let (page, _) = Page::from_json(&PAGE.replace("FROM t", "FROM nowhere")).unwrap();
        let q = check_page(&page, &CheckOptions::default())
            .compile(&page, &QueryKey::Widget("w".into()), &BTreeMap::new(), &BTreeMap::new())
            .unwrap()
            .unwrap();
        match crate::block_on(FixtureSource::new(db(), Schema::default()).execute(&q, &NoTransport)) {
            Err(SourceError::Failed(e)) => {
                assert_eq!(e.code, "fixture_eval");
                assert!(e.message.contains("nowhere"), "{}", e.message);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nulls_and_floats_type_the_raw_columns() {
        let rs = ResultSet {
            columns: vec![("a".into(), "?".into()), ("b".into(), "?".into()), ("c".into(), "?".into())],
            rows: vec![vec![Val::Int(1), Val::Float(0.5), Val::Null], vec![Val::Null, Val::Int(2), Val::Null]],
        };
        let t = to_raw_table(&rs);
        let types: Vec<&str> = t.columns().iter().map(|c| c.sql_type()).collect();
        assert_eq!(types, ["Nullable(Int64)", "Float64", "Nullable(String)"]);
        assert_eq!(t.cell(1, 0), Cell::Null);
        assert_eq!(t.cell(1, 1), Cell::Value(Scalar::Float(2.0)));
    }
}
