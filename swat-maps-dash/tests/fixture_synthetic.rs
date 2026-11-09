//! A neutral, synthetic page over a synthetic fixture table: the page
//! passes `check_page_text`, and every query on it -- variable options
//! first, then each widget with every required variable set to its first
//! option -- runs on [`FixtureSource`] through the read guard and the
//! aggregate-only result hook. The same path a real page collection takes,
//! with nothing but generated data.

use std::collections::BTreeMap;
use std::sync::Arc;

use swat_maps_dash::fixture::{Database, FixtureSource, NoTransport, Table, Val};
use swat_maps_dash::hook::{reference::MinGroupHook, run_query, Cell, GuardedSource, NoHook, Privacy, Results, Schema, SourceRegistry};
use swat_maps_dash::spec::{Control, Page, Required};
use swat_maps_dash::sql::{check_page, CheckOptions, QueryKey, VarValue};
use swat_maps_dash::testing::check_page_text;

const TABLE: &str = "synthetic_boardings";

const PAGE: &str = r#"{
  "dashboard": 1,
  "id": "synthetic-boardings",
  "title": { "en": "Boardings (synthetic)" },
  "group": "Synthetic",
  "order": 1,
  "source": "rides",
  "sources": { "rides": { "kind": "clickhouse", "connection": "fixture", "privacy": "aggregate_only" } },
  "variables": [
    { "key": "period", "label": { "en": "Period" }, "control": "date_range", "type": "Date",
      "options_sql": ["SELECT DISTINCT service_date", "FROM synthetic_boardings", "ORDER BY service_date"] },
    { "key": "line", "label": { "en": "Line" }, "control": "single", "required": true,
      "options_sql": "SELECT DISTINCT line_name FROM synthetic_boardings ORDER BY line_name" },
    { "key": "day", "label": { "en": "Day" }, "control": "multi", "default": "all",
      "options_sql": ["SELECT day_label", "FROM (", "  SELECT day_label, min(day_no) AS n", "  FROM synthetic_boardings", "  GROUP BY day_label", ")", "ORDER BY n"] },
    { "key": "hour", "label": { "en": "Hour" }, "control": "range", "type": "Int8",
      "options_sql": ["SELECT DISTINCT hour", "FROM synthetic_boardings", "ORDER BY hour"] },
    { "key": "stop", "label": { "en": "Stop" }, "control": "multi", "default": "all",
      "options_sql": ["SELECT DISTINCT stop_name", "FROM synthetic_boardings", "${where -stop}", "ORDER BY stop_name"] },
    { "key": "sel_stop", "control": "hidden" }
  ],
  "filters": [
    { "var": "period", "column": "service_date" },
    { "var": "line", "column": "line_name" },
    { "var": "day", "column": "day_label" },
    { "var": "hour", "column": "hour" },
    { "var": "stop", "column": "stop_name" }
  ],
  "widgets": [
    { "id": "total", "type": "number", "value": "passengers", "format": { "passengers": "integer" },
      "sql": ["SELECT sum(passengers) AS passengers", "FROM synthetic_boardings", "${where}"] },
    { "id": "top_stops", "type": "bar", "orient": "h", "x": "stop_name", "y": "passengers",
      "format": { "passengers": "integer" }, "click": { "set": "sel_stop", "value": "stop_name" },
      "sql": ["SELECT stop_name, sum(passengers) AS passengers", "FROM synthetic_boardings", "${where}",
              "GROUP BY stop_name", "HAVING passengers > 0", "ORDER BY passengers DESC", "LIMIT 30"] },
    { "id": "by_day", "type": "line", "x": "service_date", "y": "passengers",
      "format": { "service_date": "date", "passengers": "integer" },
      "sql": ["SELECT service_date, sum(passengers) AS passengers", "FROM synthetic_boardings", "${where}",
              "GROUP BY service_date", "ORDER BY service_date"] },
    { "id": "stop_table", "type": "table", "format": { "passengers": "integer", "avg_hour": "decimal:1" },
      "sql": ["SELECT stop_name, sum(passengers) AS passengers, avg(hour) AS avg_hour", "FROM synthetic_boardings",
              "WHERE stop_name <> 'N/A' ${and_where}", "GROUP BY stop_name", "ORDER BY stop_name ASC"] }
  ]
}"#;

/// Two lines, five stops, seven days, five hours; counts from a fixed
/// linear-congruential stream, with a few thin cells so the minimum-group
/// hook has something to suppress.
fn database() -> Database {
    let mut t = Table::new(
        TABLE,
        &[
            ("service_date", "Date"),
            ("day_no", "UInt8"),
            ("day_label", "LowCardinality(String)"),
            ("hour", "Int8"),
            ("line_name", "LowCardinality(String)"),
            ("stop_name", "String"),
            ("passengers", "UInt32"),
        ],
    );
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let mut seed: u64 = 0x5eed;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as i64
    };
    for (d, label) in days.iter().enumerate() {
        for hour in [7_i64, 8, 12, 17, 18] {
            for line in ["Line A", "Line B"] {
                for stop in ["Stop 1", "Stop 2", "Stop 3", "Stop 4", "Stop 5"] {
                    let busy = if matches!(hour, 8 | 18) { 40 } else { 5 };
                    t.push(vec![
                        Val::Str(format!("2026-01-{:02}", 5 + d)),
                        Val::Int(d as i64 + 1),
                        Val::Str((*label).into()),
                        Val::Int(hour),
                        Val::Str(line.into()),
                        Val::Str(stop.into()),
                        Val::Int(next() % busy + 1),
                    ]);
                }
            }
        }
    }
    let mut db = Database::new();
    db.insert(t);
    db
}

fn registry(page: &Page, db: &Arc<Database>) -> SourceRegistry {
    let mut reg = SourceRegistry::new();
    for (name, decl) in &page.sources {
        let schema = Schema { columns: Vec::new(), forbidden_columns: vec!["rider_id".into()] };
        let src = Arc::new(FixtureSource::new(db.clone(), schema));
        let g = match decl.privacy.unwrap_or_default() {
            Privacy::AggregateOnly => GuardedSource::new(src, Arc::new(MinGroupHook::new("passengers", &[], 10)), Privacy::AggregateOnly),
            Privacy::Open => GuardedSource::new(src, Arc::new(NoHook), Privacy::Open),
        };
        reg.register(name, g);
    }
    reg
}

fn run(reg: &SourceRegistry, sql: &swat_maps_dash::sql::PageSql, page: &Page, key: &QueryKey, values: &BTreeMap<String, VarValue>) -> Option<Result<Results, String>> {
    let (src, _) = sql.templates.get(key)?;
    let q = match sql.compile(page, key, values, &BTreeMap::new())? {
        Ok(q) => q,
        Err(p) => return Some(Err(format!("pending {p:?}"))),
    };
    Some(swat_maps_dash::block_on(run_query(reg, src, &q, &NoTransport, "en")).map_err(|e| format!("{e:?}")))
}

#[test]
fn the_synthetic_page_checks_clean() {
    let (id, diagnostics) = check_page_text(PAGE);
    assert_eq!(id.as_deref(), Some("synthetic-boardings"));
    let errors: Vec<_> = diagnostics.iter().filter(|d| d.is_error()).map(|d| format!("{} [{}] {}", d.path, d.code, d.message)).collect();
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn every_query_on_the_synthetic_page_runs_on_the_fixture() {
    let db = Arc::new(database());
    let (page, _) = Page::from_json(PAGE).unwrap_or_else(|d| panic!("{d:?}"));
    let sql = check_page(&page, &CheckOptions::default());
    let reg = registry(&page, &db);
    let mut values: BTreeMap<String, VarValue> =
        page.variables.iter().map(|v| (v.key.clone(), v.default.as_ref().map(VarValue::from_json).unwrap_or_default())).collect();
    let mut failures = Vec::new();
    let mut options = 0;
    for v in &page.variables {
        let key = QueryKey::Options(v.key.clone());
        let Some(r) = run(&reg, &sql, &page, &key, &values) else { continue };
        options += 1;
        match r {
            Err(e) => failures.push(format!("options {}: {e}", v.key)),
            Ok(r) => {
                if r.row_count() == 0 {
                    failures.push(format!("options {}: no options", v.key));
                }
                if v.required != Required::No {
                    if let Some(Cell::Value(first)) = r.columns().next().map(|c| c.cell(0)) {
                        let val = if v.control == Control::Multi { VarValue::Many(vec![first]) } else { VarValue::One(first) };
                        values.insert(v.key.clone(), val);
                    }
                }
            }
        }
    }
    assert_eq!(options, 5, "every variable but the hidden one has options");
    assert!(values.get("line").is_some_and(|v| *v != VarValue::default()), "the required line took its first option");

    let mut ran = BTreeMap::new();
    let keys: Vec<QueryKey> = sql.templates.keys().filter(|k| !matches!(k, QueryKey::Options(_))).cloned().collect();
    for key in keys {
        match run(&reg, &sql, &page, &key, &values) {
            Some(Ok(r)) => {
                ran.insert(format!("{key:?}"), r.row_count());
            }
            Some(Err(e)) => failures.push(format!("{key:?}: {e}")),
            None => {}
        }
    }
    assert!(failures.is_empty(), "{} failure(s):\n{}", failures.len(), failures.join("\n"));
    assert_eq!(ran.len(), 4, "all four widgets ran: {ran:?}");
    assert!(ran.values().all(|&n| n > 0), "every widget has rows: {ran:?}");
}

#[test]
fn the_fixture_source_logs_the_guarded_sql_and_its_parameters() {
    let db = Arc::new(database());
    let (page, _) = Page::from_json(PAGE).unwrap_or_else(|d| panic!("{d:?}"));
    let sql = check_page(&page, &CheckOptions::default());
    let src = Arc::new(FixtureSource::new(db, Schema { columns: Vec::new(), forbidden_columns: Vec::new() }));
    let log = src.log();
    let mut reg = SourceRegistry::new();
    reg.register("rides", GuardedSource::new(src, Arc::new(NoHook), Privacy::Open));
    let mut values: BTreeMap<String, VarValue> = BTreeMap::new();
    values.insert("line".into(), VarValue::from_json(&serde_json::json!("Line B")));
    let r = run(&reg, &sql, &page, &QueryKey::Widget("top_stops".into()), &values).expect("a widget query").expect("it runs");
    assert_eq!(r.row_count(), 5, "five stops on Line B");
    let log = log.lock().unwrap();
    let last = log.last().expect("the source logged the statement");
    assert!(last.sql.contains(TABLE), "{}", last.sql);
    assert!(last.params.iter().any(|(_, v)| v == "Line B"), "{:?}", last.params);
}
