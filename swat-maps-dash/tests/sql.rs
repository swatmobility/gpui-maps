//! Task 1.4 with explorer-sql D1, D2, D5, D6: the template compiler, both
//! dialects, the guard at load and at send, and the page checks the SQL
//! path relies on. Every test drives the real compiler and asserts the SQL
//! text and parameters it produced.

use std::collections::BTreeMap;

use swat_maps_dash::spec::Page;
use swat_maps_dash::sql::{
    check_page, CheckOptions, CompiledQuery, Dialect, GuardCode, Param, ParamValue, Pending, QueryKey,
    Scalar, VarValue,
};
use swat_maps_dash::hook::Refusal;

const CANARY: &str = "x' OR 1=1 -- \\'; DROP TABLE t; --";

fn station_usage() -> Page {
    Page::from_json(include_str!("fixtures/bikeshare/station_usage.json")).expect("the station-usage fixture loads").0
}

fn s(v: &str) -> Scalar {
    Scalar::Str(v.into())
}

/// A one-filter page on a source of the given kind, with a widget whose SQL
/// is `sql`.
fn page(kind: &str, vars: &str, filters: &str, sql: &str) -> Page {
    let json = format!(
        r#"{{ "dashboard": 1, "id": "p", "title": "P", "source": "src",
             "sources": {{ "src": {{ "kind": "{kind}", "connection": "local" }} }},
             "variables": [{vars}], "filters": [{filters}],
             "widgets": [{{ "id": "w", "type": "bar", "sql": {sql:?} }}] }}"#
    );
    match Page::from_json(&json) {
        Ok((p, _)) => p,
        Err(d) => panic!("{d:#?}"),
    }
}

fn compile(p: &Page, values: &[(&str, VarValue)]) -> Result<CompiledQuery, Pending> {
    let sql = check_page(p, &CheckOptions::default());
    assert!(sql.diagnostics.iter().all(|d| !d.is_error()), "{:#?}", sql.diagnostics);
    let values: BTreeMap<String, VarValue> = values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    sql.compile(p, &QueryKey::Widget("w".into()), &values, &BTreeMap::new()).expect("the widget has a template")
}

fn text(q: &CompiledQuery) -> String {
    q.guarded(q.dialect()).expect("passes the guard").sql().to_string()
}

#[test]
fn the_station_usage_fixture_parses_and_checks_clean() {
    let p = station_usage();
    let sql = check_page(&p, &CheckOptions::default());
    let errors: Vec<_> = sql.diagnostics.iter().filter(|d| d.is_error()).collect();
    assert!(errors.is_empty(), "{errors:#?}");
    assert_eq!(sql.templates.len(), 5, "three widgets' and named queries' SQL plus two options queries");
}

#[test]
fn binds_never_pastes() {
    let p = station_usage();
    let sql = check_page(&p, &CheckOptions::default());
    let values: BTreeMap<String, VarValue> = [
        ("day_type", VarValue::One(s(CANARY))),
        ("zone", VarValue::Many(vec![s(CANARY), s("b")])),
        ("cell", VarValue::One(s(CANARY))),
        ("min_rides", VarValue::One(Scalar::Int(10))),
        ("station", VarValue::One(s(CANARY))),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let mut compiled = 0;
    for key in sql.templates.keys() {
        let q = sql.compile(&p, key, &values, &BTreeMap::new()).unwrap().unwrap();
        let t = text(&q);
        assert!(!t.contains("OR 1=1") && !t.contains("DROP"), "{key:?}: {t}");
        compiled += 1;
        if !q.params().is_empty() {
            assert!(q.params().iter().any(|p| match &p.value {
                ParamValue::One(Scalar::Str(v)) => v.contains("OR 1=1"),
                ParamValue::Array(a) => a.contains(&s(CANARY)),
                _ => false,
            }), "{key:?}: the canary is in params");
        }
    }
    assert_eq!(compiled, 5);
    // The same over the Postgres dialect.
    let pg = page("postgres", r#"{ "key": "k", "control": "single" }"#, r#"{ "var": "k", "column": "c" }"#, "SELECT c FROM t ${where}");
    let q = compile(&pg, &[("k", VarValue::One(s(CANARY)))]).unwrap();
    assert_eq!(text(&q), "SELECT c FROM t WHERE c = $1");
    assert_eq!(q.params()[0].value, ParamValue::One(s(CANARY)));
}

fn one_op(op: &str, control: &str, value: VarValue) -> (String, Vec<Param>, String, Vec<Param>) {
    let var = format!(r#"{{ "key": "k", "control": "{control}", "type": "Int32" }}"#);
    let filter = format!(r#"{{ "var": "k", "column": "c", "op": "{op}" }}"#);
    let run = |kind| {
        let p = page(kind, &var, &filter, "SELECT c FROM t ${where}");
        let q = compile(&p, &[("k", value.clone())]).unwrap();
        (text(&q), q.params().to_vec())
    };
    let (ch, chp) = run("clickhouse");
    let (pg, pgp) = run("postgres");
    (ch, chp, pg, pgp)
}

#[test]
fn each_operator_has_a_golden_in_both_dialects() {
    let i = |v| Scalar::Int(v);
    let (ch, chp, pg, pgp) = one_op("eq", "single", VarValue::One(i(3)));
    assert_eq!(ch, "SELECT c FROM t WHERE c = {f_k:Int32}");
    assert_eq!(pg, "SELECT c FROM t WHERE c = $1");
    assert_eq!(chp[0].ty, "Int32");
    assert_eq!(pgp[0].ty, "int4");

    let (ch, chp, pg, pgp) = one_op("in", "multi", VarValue::Many(vec![i(1), i(2)]));
    assert_eq!(ch, "SELECT c FROM t WHERE c IN {f_k:Array(Int32)}");
    assert_eq!(pg, "SELECT c FROM t WHERE c = ANY($1)");
    assert_eq!(chp[0].value, ParamValue::Array(vec![i(1), i(2)]));
    assert_eq!(pgp[0].ty, "int4[]");

    let range = VarValue::Range { lo: Some(i(1)), hi: Some(i(9)) };
    let (ch, _, pg, _) = one_op("between", "range", range.clone());
    assert_eq!(ch, "SELECT c FROM t WHERE c BETWEEN {f_k_lo:Int32} AND {f_k_hi:Int32}");
    assert_eq!(pg, "SELECT c FROM t WHERE c BETWEEN $1 AND $2");
    let (ch, _, _, _) = one_op("between", "range", VarValue::Range { lo: None, hi: Some(i(9)) });
    assert_eq!(ch, "SELECT c FROM t WHERE c <= {f_k_hi:Int32}", "an open end drops its half");

    let (ch, _, pg, _) = one_op("overlaps", "range", range);
    assert_eq!(ch, "SELECT c FROM t WHERE c_lo <= {f_k_hi:Int32} AND c_hi >= {f_k_lo:Int32}");
    assert_eq!(pg, "SELECT c FROM t WHERE c_lo <= $1 AND c_hi >= $2");

    let (ch, _, pg, _) = one_op("ge", "slider", VarValue::One(i(5)));
    assert_eq!(ch, "SELECT c FROM t WHERE c >= {f_k:Int32}");
    assert_eq!(pg, "SELECT c FROM t WHERE c >= $1");
    let (ch, _, _, _) = one_op("le", "slider", VarValue::One(i(5)));
    assert_eq!(ch, "SELECT c FROM t WHERE c <= {f_k:Int32}");

    let (ch, chp, pg, pgp) = one_op("contains", "search", VarValue::One(s("ｱｵﾓﾘ")));
    assert_eq!(ch, "SELECT c FROM t WHERE position(normalizeUTF8NFKC(c), {f_k:String}) > 0");
    assert_eq!(pg, "SELECT c FROM t WHERE strpos(normalize(c, NFKC), $1) > 0");
    assert_eq!(chp[0].value, ParamValue::One(s("アオモリ")), "search_nfkc: the bound input is NFKC");
    assert_eq!(pgp[0].ty, "text");
}

#[test]
fn search_on_a_normalized_column_reads_it_bare() {
    let p = page(
        "clickhouse",
        r#"{ "key": "k", "control": "search" }"#,
        r#"{ "var": "k", "column": "c_nfkc", "normalized": true }"#,
        "SELECT c FROM t ${where}",
    );
    let q = compile(&p, &[("k", VarValue::One(s("  ＡＢＣ ")))]).unwrap();
    assert_eq!(text(&q), "SELECT c FROM t WHERE position(c_nfkc, {f_k:String}) > 0");
    assert_eq!(q.params()[0].value, ParamValue::One(s("ABC")));
    // An input that is empty after normalisation is unset.
    let q = compile(&p, &[("k", VarValue::One(s("   ")))]).unwrap();
    assert_eq!(text(&q), "SELECT c FROM t ");
}

#[test]
fn placeholders_bind_with_their_type() {
    let p = page(
        "clickhouse",
        r#"{ "key": "day", "control": "single", "type": "Int8" }, { "key": "r", "control": "range", "type": "Date" }"#,
        "",
        "SELECT c FROM t WHERE d = ${day} AND x BETWEEN ${r.lo} AND ${r.hi} AND y <> ${day}",
    );
    let q = compile(
        &p,
        &[("day", VarValue::One(Scalar::Int(1))), ("r", VarValue::Range { lo: Some(s("2026-01-01")), hi: Some(s("2026-02-01")) })],
    )
    .unwrap();
    assert_eq!(text(&q), "SELECT c FROM t WHERE d = {p_day:Int8} AND x BETWEEN {p_r_lo:Date} AND {p_r_hi:Date} AND y <> {p_day:Int8}");
    assert_eq!(q.params().len(), 3, "a repeated placeholder binds once");
    assert_eq!(q.deps(), ["day", "r"]);
    // An unset placeholder cannot bind: the query waits.
    assert_eq!(compile(&p, &[]).unwrap_err(), Pending::Unbound { var: "day".into() });
}

#[test]
fn and_where_appends_to_an_existing_where() {
    let p = page("clickhouse", r#"{ "key": "k", "control": "single" }"#, r#"{ "var": "k", "column": "c" }"#, "SELECT c FROM t WHERE hour <> -1 ${and_where}");
    assert_eq!(text(&compile(&p, &[("k", VarValue::One(s("a")))]).unwrap()), "SELECT c FROM t WHERE hour <> -1 AND c = {f_k:String}");
    assert_eq!(text(&compile(&p, &[]).unwrap()), "SELECT c FROM t WHERE hour <> -1 ", "nothing set renders as empty text");
}

#[test]
fn an_exclusion_drops_only_its_variable() {
    let p = page(
        "clickhouse",
        r#"{ "key": "a", "control": "single" }, { "key": "b", "control": "single" }"#,
        r#"{ "var": "a", "column": "ca" }, { "var": "b", "column": "cb" }"#,
        "SELECT ca FROM t ${where -b}",
    );
    let q = compile(&p, &[("a", VarValue::One(s("1"))), ("b", VarValue::One(s("2")))]).unwrap();
    assert_eq!(text(&q), "SELECT ca FROM t WHERE ca = {f_a:String}");
    assert_eq!(q.deps(), ["a"], "an excluded variable is not a dependency");
}

#[test]
fn all_emits_no_clause() {
    let p = page("clickhouse", r#"{ "key": "k", "control": "multi" }"#, r#"{ "var": "k", "column": "c" }"#, "SELECT c FROM t ${where}");
    for v in [VarValue::All, VarValue::Many(vec![]), VarValue::Unset] {
        let q = compile(&p, &[("k", v.clone())]).unwrap();
        assert_eq!(text(&q), "SELECT c FROM t ", "{v:?}: never `c IN ()`");
        assert!(q.params().is_empty());
    }
}

#[test]
fn a_having_filter_never_appears_in_where() {
    let p = page(
        "clickhouse",
        r#"{ "key": "n", "control": "slider", "type": "UInt32" }"#,
        r#"{ "var": "n", "column": "moves", "op": "ge", "clause": "having" }"#,
        "SELECT g, sum(x) AS moves FROM t ${where} GROUP BY g ${having}",
    );
    let q = compile(&p, &[("n", VarValue::One(Scalar::Int(10)))]).unwrap();
    assert_eq!(text(&q), "SELECT g, sum(x) AS moves FROM t  GROUP BY g HAVING moves >= {f_n:UInt32}");
}

#[test]
fn having_filter_without_slot_is_an_error() {
    let json = r#"{ "id": "p", "title": "P", "source": "src", "sources": { "src": { "connection": "local" } },
        "variables": [{ "key": "n", "control": "slider" }],
        "filters": [{ "var": "n", "column": "moves", "op": "ge", "clause": "having" }],
        "widgets": [{ "id": "w", "type": "bar", "sql": "SELECT g, sum(x) AS moves FROM t ${where} GROUP BY g" }] }"#;
    let (p, _) = Page::from_json(json).unwrap();
    let d = check_page(&p, &CheckOptions::default()).diagnostics;
    assert!(d.iter().any(|d| d.code == "having_filter_without_slot" && d.path == "/widgets/0/sql"), "{d:#?}");
}

#[test]
fn having_alias_is_unsupported_in_postgres() {
    let json = r#"{ "id": "p", "title": "P", "source": "src", "sources": { "src": { "kind": "postgres", "connection": "pg" } },
        "variables": [{ "key": "n", "control": "slider" }],
        "filters": [{ "var": "n", "column": "moves", "op": "ge", "clause": "having" }],
        "widgets": [{ "id": "w", "type": "bar", "sql": "SELECT g, sum(x) AS moves FROM t GROUP BY g ${having}" }] }"#;
    let (p, _) = Page::from_json(json).unwrap();
    let d = check_page(&p, &CheckOptions::default()).diagnostics;
    assert!(d.iter().any(|d| d.code == "having_alias_unsupported" && d.path == "/filters/0/clause"), "{d:#?}");
}

#[test]
fn required_strict() {
    let p = page(
        "clickhouse",
        r#"{ "key": "k", "control": "single", "required": true }"#,
        r#"{ "var": "k", "column": "c" }"#,
        "SELECT c FROM t ${where}",
    );
    assert!(compile(&p, &[("k", VarValue::One(s("x")))]).is_ok());
    assert_eq!(compile(&p, &[("k", VarValue::Many(vec![s("x")]))]).unwrap_err(), Pending::ChooseOne { var: "k".into() });
    assert_eq!(compile(&p, &[("k", VarValue::Many(vec![s("x"), s("y")]))]).unwrap_err(), Pending::TooMany { var: "k".into() });
    assert_eq!(compile(&p, &[]).unwrap_err(), Pending::ChooseOne { var: "k".into() });
}

#[test]
fn required_one_value() {
    let p = page(
        "clickhouse",
        r#"{ "key": "k", "control": "multi", "required": "one_value" }"#,
        r#"{ "var": "k", "column": "c" }"#,
        "SELECT c FROM t ${where}",
    );
    let q = compile(&p, &[("k", VarValue::Many(vec![s("x")]))]).unwrap();
    assert_eq!(text(&q), "SELECT c FROM t WHERE c IN {f_k:Array(String)}", "the one-element IN");
    assert_eq!(compile(&p, &[("k", VarValue::Many(vec![s("x"), s("y")]))]).unwrap_err(), Pending::TooMany { var: "k".into() });
    assert!(compile(&p, &[("k", VarValue::All)]).is_err());
    assert!(compile(&p, &[("k", VarValue::Many(vec![]))]).is_err());
}

#[test]
fn dependencies_are_derived_from_placeholders_and_slots() {
    let p = station_usage();
    let sql = check_page(&p, &CheckOptions::default());
    let values: BTreeMap<String, VarValue> = [("day_type".to_string(), VarValue::One(s("weekday")))].into_iter().collect();
    let deps = |k: QueryKey| sql.compile(&p, &k, &values, &BTreeMap::new()).unwrap().unwrap().deps().to_vec();
    assert_eq!(deps(QueryKey::Widget("rides_bar".into())), ["day_type", "zone", "cell", "station", "min_rides"]);
    assert_eq!(deps(QueryKey::Widget("rides_map".into())), ["day_type", "zone", "station", "min_rides"]);
    assert_eq!(deps(QueryKey::Options("zone".into())), ["day_type", "station"]);
    assert!(deps(QueryKey::Options("day_type".into())).is_empty());
}

#[test]
fn a_cycle_names_both_keys() {
    let json = r#"{ "id": "p", "title": "P", "source": "src", "sources": { "src": { "connection": "local" } },
        "variables": [
          { "key": "a", "control": "single", "options_sql": "SELECT a FROM t ${where}" },
          { "key": "b", "control": "single", "options_sql": "SELECT b FROM t ${where}" } ],
        "filters": [{ "var": "a", "column": "a" }, { "var": "b", "column": "b" }] }"#;
    let (p, _) = Page::from_json(json).unwrap();
    let d = check_page(&p, &CheckOptions::default()).diagnostics;
    let cycle: Vec<_> = d.iter().filter(|d| d.code == "dependency_cycle").collect();
    assert_eq!(cycle.len(), 1, "{d:#?}");
    assert!(cycle[0].message.contains(" a ") && cycle[0].message.contains(" b "), "{}", cycle[0].message);
}

#[test]
fn the_lexer_table() {
    // One row per rule of §4.2.1, each its own code at its own line.
    let cases = [
        ("SELECT c FROM t WHERE d = {day}", "bare_brace_placeholder"),
        ("SELECT c FROM t WHERE d = {day:String}", "native_param_in_template"),
        ("SELECT c FROM t WHERE d = '${k}'", "placeholder_in_string"),
        ("SELECT c FROM t WHERE d = ${nope}", "unknown_variable"),
        ("SELECT c FROM t WHERE d = ${k.lo}", "range_part_on_scalar"),
        ("DELETE FROM t", "statement_not_read"),
        ("SHOW TABLES", "statement_not_read"),
        ("SELECT 1; SELECT 2", "statement_not_read"),
        ("SELECT c FROM t; DROP TABLE t", "statement_not_read"),
        ("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x", "statement_not_read"),
        ("SELECT c FROM url('http://127.0.0.1/x', CSV)", "table_function_not_allowed"),
        ("SELECT c FROM t WHERE d = 'x", "sql_unterminated"),
    ];
    for (sql, code) in cases {
        let json = format!(
            r#"{{ "id": "p", "title": "P", "source": "src", "sources": {{ "src": {{ "connection": "local" }} }},
                 "variables": [{{ "key": "k", "control": "single" }}],
                 "widgets": [{{ "id": "w", "type": "bar", "sql": {sql:?} }}] }}"#
        );
        let (p, _) = Page::from_json(&json).unwrap();
        let d = check_page(&p, &CheckOptions::default()).diagnostics;
        let hit = d.iter().find(|d| d.code == code);
        assert!(hit.is_some(), "{sql}: expected {code}, got {d:#?}");
        assert_eq!(hit.unwrap().path, "/widgets/0/sql", "{sql}");
    }
    // In the array form, a diagnostic points at its line.
    let json = r#"{ "id": "p", "title": "P", "source": "src", "sources": { "src": { "connection": "local" } },
        "widgets": [{ "id": "w", "type": "bar", "sql": ["SELECT c", "FROM t", "WHERE d = {day}"] }] }"#;
    let (p, _) = Page::from_json(json).unwrap();
    let d = check_page(&p, &CheckOptions::default()).diagnostics;
    assert_eq!(d[0].path, "/widgets/0/sql/2", "{d:#?}");
    // A placeholder inside a comment is ignored and is not a dependency.
    let p = page("clickhouse", r#"{ "key": "k", "control": "single" }"#, "", "SELECT c FROM t -- ${k}\n/* ${nope} */");
    let q = compile(&p, &[]).unwrap();
    assert!(q.deps().is_empty());
}

#[test]
fn select_star_is_an_error_on_aggregate_only_and_a_warning_on_open() {
    for (privacy, severity_error) in [("aggregate_only", true), ("open", false)] {
        let json = format!(
            r#"{{ "id": "p", "title": "P", "source": "src",
                 "sources": {{ "src": {{ "connection": "local", "privacy": "{privacy}" }} }},
                 "widgets": [{{ "id": "w", "type": "bar", "sql": "SELECT * FROM t" }}] }}"#
        );
        let (p, _) = Page::from_json(&json).unwrap();
        let sql = check_page(&p, &CheckOptions::default());
        let d = sql.diagnostics.iter().find(|d| d.code == "select_star").expect("select_star reported");
        assert_eq!(d.is_error(), severity_error, "{privacy}");
        assert_eq!(sql.templates.contains_key(&QueryKey::Widget("w".into())), !severity_error, "an open source still compiles");
    }
    // `count(*)` and `a * b` are not star selects.
    let p = page("clickhouse", "", "", "SELECT count(*) AS n, a * b AS m FROM t");
    assert!(compile(&p, &[]).is_ok());
}

#[test]
fn a_connection_can_allow_a_table_function() {
    let p = page("clickhouse", "", "", "SELECT a FROM url('http://127.0.0.1/x.csv', CSV, 'a String')");
    let opts = CheckOptions { guard: swat_maps_dash::sql::GuardOptions { allowed_table_functions: vec!["url".into()] }, ..Default::default() };
    let sql = check_page(&p, &opts);
    assert!(sql.diagnostics.iter().all(|d| !d.is_error()), "{:#?}", sql.diagnostics);
    let q = sql.compile(&p, &QueryKey::Widget("w".into()), &BTreeMap::new(), &BTreeMap::new()).unwrap().unwrap();
    // The send-time guard needs the same allowance.
    assert_eq!(q.guarded(Dialect::ClickHouse).unwrap_err(), Refusal::Guard { code: GuardCode::TableFunction("url") });
    assert!(q.guarded_with(Dialect::ClickHouse, &opts.guard).is_ok());
}

#[test]
fn guarded_refuses_another_dialect() {
    let p = page("postgres", "", "", "SELECT 1 AS a");
    let q = compile(&p, &[]).unwrap();
    assert_eq!(q.guarded(Dialect::ClickHouse).unwrap_err(), Refusal::Guard { code: GuardCode::DialectMismatch });
    assert_eq!(q.guarded(Dialect::Postgres).unwrap().sql(), "SELECT 1 AS a");
}

#[test]
fn generated_queries_compile_per_dialect() {
    let p = station_usage();
    let sql = check_page(&p, &CheckOptions::default());
    let values: BTreeMap<String, VarValue> =
        [("day_type".to_string(), VarValue::One(s("weekday"))), ("min_rides".to_string(), VarValue::One(Scalar::Int(10)))]
            .into_iter()
            .collect();
    let q = sql.compile(&p, &QueryKey::Query("total".into()), &values, &BTreeMap::new()).unwrap().unwrap();
    assert_eq!(
        text(&q),
        "SELECT sum(ride_count) AS rides FROM bike_trips WHERE day_kind = {f_day_type:String} HAVING rides >= {f_min_rides:UInt32}"
    );
    let q = swat_maps_dash::spec::QueryDecl {
        table: Some("trips".into()),
        group: vec!["district".into()],
        agg: [("n".to_string(), "count()".to_string()), ("u".to_string(), "count_distinct(rider)".to_string())].into_iter().collect(),
        order: Some("n desc".into()),
        top: Some(10),
        ..Default::default()
    };
    assert_eq!(
        swat_maps_dash::sql::generated_text(&q, Dialect::Postgres).unwrap(),
        "SELECT district, count(*) AS n, count(DISTINCT rider) AS u FROM trips ${where} GROUP BY district ${having} ORDER BY n DESC LIMIT 10"
    );
    let bad = swat_maps_dash::spec::QueryDecl { table: Some("t; DROP TABLE t".into()), agg: q.agg.clone(), ..Default::default() };
    assert_eq!(swat_maps_dash::sql::generated_text(&bad, Dialect::ClickHouse).unwrap_err().code, "column_not_identifier");
}

#[test]
fn compile_is_under_200us_per_template() {
    let p = station_usage();
    let sql = check_page(&p, &CheckOptions::default());
    let values: BTreeMap<String, VarValue> =
        [("day_type".to_string(), VarValue::One(s("weekday"))), ("zone".to_string(), VarValue::Many(vec![s("a"), s("b")]))]
            .into_iter()
            .collect();
    let key = QueryKey::Widget("rides_bar".into());
    let n = 2000;
    let start = std::time::Instant::now();
    for _ in 0..n {
        std::hint::black_box(sql.compile(&p, &key, &values, &BTreeMap::new()));
    }
    let per = start.elapsed() / n;
    // Debug builds are slower; the budget is for release, so allow 5x here.
    assert!(per.as_micros() < 1000, "dash.sql.compile_us: {per:?} per template");
}
