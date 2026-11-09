//! Task 1.5 with D4: the hook runs on every result, `Results` are columns,
//! and a suppressed value is gone. A stub source stands in for a server.

use std::sync::{Arc, Mutex};

use swat_maps_dash::block_on;
use swat_maps_dash::connect::{HttpRequest, HttpResponse, HttpTransport, TransportError};
use swat_maps_dash::hook::{
    reference::MinGroupHook, run_query, Cell, Column, ColumnData, GuardedSource, NoHook, Predicate, Privacy, QueryError,
    QueryShape, RawTable, Refusal, ResultHook, Schema, Source, SourceError, SourceRegistry, StrColumn,
};
use swat_maps_dash::spec::Page;
use swat_maps_dash::sql::{check_page, CheckOptions, CompiledQuery, QueryKey, Scalar};
use swat_maps_dash::BoxFuture;

/// Counts requests; never answers (the sources below do not use it).
#[derive(Default)]
struct Recorder(Mutex<usize>);

impl HttpTransport for Recorder {
    fn send(&self, _req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        *self.0.lock().unwrap() += 1;
        Box::pin(async { Err(TransportError::Io("stub".into())) })
    }
}

/// A source that returns a fixed table: mesh cells with their riders, one
/// of them a group of 7 holding the sentinel 7,777,777 in its measure.
struct Mesh {
    schema: Schema,
    executed: Mutex<usize>,
    extra_row: bool,
}

impl Mesh {
    fn new() -> Mesh {
        Mesh {
            schema: Schema {
                columns: vec![("mesh".into(), "String".into()), ("riders".into(), "UInt64".into()), ("moves".into(), "UInt64".into())],
                forbidden_columns: vec!["card_id".into()],
            },
            executed: Mutex::new(0),
            extra_row: false,
        }
    }
}

impl Source for Mesh {
    fn schema(&self) -> &Schema {
        &self.schema
    }
    fn execute<'a>(&'a self, _q: &'a CompiledQuery, t: &'a dyn HttpTransport) -> BoxFuture<'a, Result<RawTable, SourceError>> {
        let _ = t;
        *self.executed.lock().unwrap() += 1;
        let extra = self.extra_row;
        Box::pin(async move {
            let mut mesh = StrColumn::default();
            let mut riders = vec![120, 7, 40];
            let mut moves = vec![900, 7_777_777, 300];
            for m in ["5339-a", "5339-b", "5339-c"] {
                mesh.push(m);
            }
            if extra {
                mesh.push("extra");
                riders.push(1);
                moves.push(1);
            }
            Ok(RawTable::new(vec![
                Column::new("mesh", "String", ColumnData::Str(mesh)),
                Column::new("riders", "UInt64", ColumnData::UInt(riders)),
                Column::new("moves", "UInt64", ColumnData::UInt(moves)),
            ])
            .unwrap())
        })
    }
}

fn query() -> CompiledQuery {
    let (p, _) = Page::from_json(
        r#"{ "id": "p", "title": "P", "source": "s",
             "widgets": [{ "id": "w", "type": "bar", "sql": "SELECT mesh, uniqExact(card) AS riders, sum(m) AS moves FROM t GROUP BY mesh" }] }"#,
    )
    .unwrap();
    let sql = check_page(&p, &CheckOptions::default());
    sql.compile(&p, &QueryKey::Widget("w".into()), &Default::default(), &Default::default()).unwrap().unwrap()
}

fn registry(source: Arc<dyn Source>, hook: Arc<dyn ResultHook>) -> SourceRegistry {
    let mut r = SourceRegistry::new();
    r.register("s", GuardedSource::new(source, hook, Privacy::AggregateOnly).threshold("min_group", 10).rule("min_group_10"));
    r
}

fn hooked() -> swat_maps_dash::hook::Results {
    let reg = registry(Arc::new(Mesh::new()), Arc::new(MinGroupHook::new("riders", &["moves"], 10)));
    block_on(run_query(&reg, "s", &query(), &Recorder::default(), "en")).unwrap()
}

#[test]
fn a_small_group_is_suppressed_and_its_value_is_nowhere() {
    let r = hooked();
    assert_eq!(r.cell(1, "moves"), Some(Cell::Suppressed));
    assert_eq!(r.cell(1, "riders"), Some(Cell::Suppressed));
    assert_eq!(r.cell(0, "moves"), Some(Cell::Value(Scalar::UInt(900))));
    assert_eq!(r.suppressed_label(), "under 10");
    // The typed column slot holds zero, not the old value.
    assert_eq!(r.column("moves").unwrap().values(), &ColumnData::UInt(vec![900, 0, 300]));
    let json = serde_json::to_string(&r).unwrap();
    for canary in ["7777777", "7,777,777"] {
        assert!(!json.contains(canary), "{json}");
    }
    assert!(json.contains(r#"{"suppressed":true}"#), "{json}");
}

#[test]
fn a_reducer_over_a_suppressed_cell_is_suppressed() {
    let r = hooked();
    assert_eq!(r.sum("moves"), Some(Cell::Suppressed));
    assert_eq!(r.sum("mesh"), None, "not a number column");
}

#[test]
fn suppressed_sorts_last_in_both_directions() {
    let r = hooked();
    assert_eq!(r.order_by("moves", false).unwrap(), [2, 0, 1]);
    assert_eq!(r.order_by("moves", true).unwrap(), [0, 2, 1]);
}

#[test]
fn a_hook_runs_with_no_hook_too() {
    let reg = registry(Arc::new(Mesh::new()), Arc::new(NoHook));
    let r = block_on(run_query(&reg, "s", &query(), &Recorder::default(), "en")).unwrap();
    assert_eq!(r.row_count(), 3);
    assert!(!r.truncated());
}

/// A hook that records what `QueryShape` lets it name, then refuses.
struct Probe(Mutex<Vec<String>>, Option<Refusal>);

impl ResultHook for Probe {
    fn check(&self, shape: &QueryShape) -> Result<(), Refusal> {
        let mut log = self.0.lock().unwrap();
        log.push(format!("column 7777777: {:?}", shape.column("7777777").is_some()));
        log.push(format!("column riders: {:?}", shape.column("riders").is_some()));
        log.push(format!("undeclared threshold: {:?}", shape.threshold("other").is_some()));
        log.push(format!("bad rule: {:?}", shape.rule("Not A Rule").is_some()));
        match (shape.threshold("min_group"), shape.rule("min_group_10")) {
            (Some(threshold), Some(rule)) if self.1.is_none() => Err(Refusal::BelowThreshold { rule, threshold }),
            _ => Ok(()),
        }
    }
    fn apply(&self, _shape: &QueryShape, raw: RawTable) -> Result<RawTable, Refusal> {
        Ok(raw)
    }
    fn scope(&self, _shape: &QueryShape) -> Result<Option<Predicate>, Refusal> {
        match &self.1 {
            Some(r) => Err(r.clone()),
            None => Ok(None),
        }
    }
    fn suppressed_label(&self, _locale: &str) -> String {
        "under 10".into()
    }
}

#[test]
fn refusal_column_must_be_declared() {
    let probe = Arc::new(Probe(Mutex::new(Vec::new()), None));
    let reg = registry(Arc::new(Mesh::new()), probe.clone());
    let out = block_on(run_query(&reg, "s", &query(), &Recorder::default(), "en"));
    let Err(QueryError::Refused(r)) = out else { panic!("{out:?}") };
    assert_eq!(r.code(), "below_threshold");
    assert_eq!(r.render("en"), "Hidden: fewer than 10 in this selection.");
    assert_eq!(r.render("ja"), "非表示: この選択は 10 未満です。");
    // The six languages the SWAT apps share, by code and by OS-style tag.
    assert_eq!(r.render("vi"), "Đã ẩn: lựa chọn này có ít hơn 10.");
    assert_eq!(r.render("th-TH"), "ซ่อนไว้: รายการที่เลือกนี้มีน้อยกว่า 10");
    assert_eq!(r.render("id"), "Disembunyikan: kurang dari 10 dalam pilihan ini.");
    assert_eq!(r.render("zh-hans"), "已隐藏：此选择少于 10。");
    // Traditional Chinese and unknown locales fall back to English.
    assert_eq!(r.render("zh-Hant-TW"), r.render("en"));
    assert_eq!(r.render("de"), r.render("en"));
    assert_eq!(
        *probe.0.lock().unwrap(),
        ["column 7777777: false", "column riders: true", "undeclared threshold: false", "bad rule: false"]
    );
}

#[test]
fn scope_fails_closed() {
    let mesh = Arc::new(Mesh::new());
    let probe = Arc::new(Probe(Mutex::new(Vec::new()), Some(Refusal::NotAggregated)));
    let reg = registry(mesh.clone(), probe);
    let rec = Recorder::default();
    let out = block_on(run_query(&reg, "s", &query(), &rec, "en"));
    assert_eq!(out.unwrap_err(), QueryError::Refused(Refusal::NotAggregated));
    assert_eq!(*mesh.executed.lock().unwrap(), 0, "the source never ran");
    assert_eq!(*rec.0.lock().unwrap(), 0, "the transport saw no request");
}

#[test]
fn a_hook_that_adds_rows_is_refused() {
    struct Adds;
    impl ResultHook for Adds {
        fn check(&self, _: &QueryShape) -> Result<(), Refusal> {
            Ok(())
        }
        fn apply(&self, _: &QueryShape, _raw: RawTable) -> Result<RawTable, Refusal> {
            let mut s = StrColumn::default();
            for _ in 0..9 {
                s.push("x");
            }
            Ok(RawTable::new(vec![Column::new("mesh", "String", ColumnData::Str(s))]).unwrap())
        }
        fn suppressed_label(&self, _: &str) -> String {
            String::new()
        }
    }
    let reg = registry(Arc::new(Mesh::new()), Arc::new(Adds));
    assert!(matches!(block_on(run_query(&reg, "s", &query(), &Recorder::default(), "en")), Err(QueryError::Refused(_))));
}

#[test]
fn an_unknown_source_is_an_error_not_a_bypass() {
    let reg = SourceRegistry::new();
    assert_eq!(block_on(run_query(&reg, "nope", &query(), &Recorder::default(), "en")).unwrap_err(), QueryError::UnknownSource);
}

#[test]
fn truncated_survives_the_hook() {
    struct Capped(Mesh);
    impl Source for Capped {
        fn schema(&self) -> &Schema {
            &self.0.schema
        }
        fn execute<'a>(&'a self, q: &'a CompiledQuery, t: &'a dyn HttpTransport) -> BoxFuture<'a, Result<RawTable, SourceError>> {
            Box::pin(async move {
                let mut raw = self.0.execute(q, t).await?;
                raw.set_truncated(true);
                Ok(raw)
            })
        }
    }
    let mut m = Mesh::new();
    m.extra_row = false;
    let reg = registry(Arc::new(Capped(m)), Arc::new(MinGroupHook::new("riders", &["moves"], 10)));
    let r = block_on(run_query(&reg, "s", &query(), &Recorder::default(), "en")).unwrap();
    assert!(r.truncated(), "D3: the result reports truncated");
}
