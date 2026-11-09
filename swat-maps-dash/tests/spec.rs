//! Task 1.2's parsing tests that phase 1's SQL path relies on, with
//! explorer-sql D6/F1 (`postgres`) and D3/F2 (`max_rows`).

use swat_maps_dash::spec::{Localised, Page, SourceKind, SqlText};
use swat_maps_dash::sql::{check_page, CheckOptions};

fn base(extra: &str) -> String {
    format!(r#"{{ "dashboard": 1, "id": "p", "title": "P" {extra} }}"#)
}

fn errors(json: &str) -> Vec<(String, &'static str)> {
    match Page::from_json(json) {
        Ok((p, _)) => p.structural_diagnostics().into_iter().filter(|d| d.is_error()).map(|d| (d.path, d.code)).collect(),
        Err(d) => d.into_iter().filter(|d| d.is_error()).map(|d| (d.path, d.code)).collect(),
    }
}

#[test]
fn a_typo_is_an_error_at_its_json_pointer() {
    let e = errors(&base(r#", "variables": [{ "key": "a", "control": "single" }, { "key": "b", "contorl": "single" }]"#));
    assert_eq!(e, [("/variables/1/contorl".to_string(), "unknown_key")]);
    let e = errors(&base(r#", "widgets": [{ "id": "w", "type": "bar", "sqll": "SELECT 1" }]"#));
    assert_eq!(e, [("/widgets/0/sqll".to_string(), "unknown_key")]);
    let e = errors(&base(r#", "titel": "x""#));
    assert_eq!(e, [("/titel".to_string(), "unknown_key")]);
}

#[test]
fn a_page_without_a_version_is_read_as_version_1() {
    let (_, d) = Page::from_json(r#"{ "id": "p", "title": "P" }"#).unwrap();
    assert!(d.iter().any(|d| d.code == "version_assumed"));
    assert_eq!(errors(r#"{ "dashboard": 2, "id": "p", "title": "P" }"#), [("/dashboard".to_string(), "version_too_new")]);
}

#[test]
fn a_page_without_sources_is_valid() {
    let (p, _) = Page::from_json(&base(r#", "source": "host_registered", "widgets": [{ "id": "w", "type": "bar", "sql": "SELECT a FROM t" }]"#)).unwrap();
    let sql = check_page(&p, &CheckOptions::default());
    assert!(sql.diagnostics.iter().all(|d| !d.is_error()), "{:#?}", sql.diagnostics);
}

#[test]
fn column_not_identifier() {
    for bad in ["a; DROP TABLE t", "a OR 1=1", "a.b.c", "1a", "a b", "\"a\"", "a--", ""] {
        let json = base(&format!(
            r#", "variables": [{{ "key": "k", "control": "single" }}], "filters": [{{ "var": "k", "column": {bad:?} }}]"#
        ));
        assert_eq!(errors(&json), [("/filters/0/column".to_string(), "column_not_identifier")], "{bad}");
    }
    for good in ["a", "_a1", "t.day_type"] {
        let json = base(&format!(
            r#", "variables": [{{ "key": "k", "control": "single" }}], "filters": [{{ "var": "k", "column": {good:?} }}]"#
        ));
        assert!(errors(&json).is_empty(), "{good}");
    }
}

#[test]
fn variable_reserved_and_unknown_references() {
    let e = errors(&base(r#", "variables": [{ "key": "where", "control": "single" }]"#));
    assert_eq!(e, [("/variables/0/key".to_string(), "variable_reserved")]);
    let e = errors(&base(r#", "filters": [{ "var": "nope", "column": "c" }]"#));
    assert_eq!(e, [("/filters/0/var".to_string(), "unknown_variable")]);
    let e = errors(&base(r#", "layout": { "columns": 10, "items": [{ "widget": "ghost" }], "overlay": [] }"#));
    let codes: Vec<&str> = e.iter().map(|(_, c)| *c).collect();
    assert_eq!(codes, ["layout_both", "columns_not_12", "unknown_widget"]);
}

#[test]
fn connections_not_in_json() {
    for key in ["url", "host", "user", "password", "user_env", "password_env"] {
        let json = base(&format!(r#", "sources": {{ "s": {{ "connection": "local", "{key}": "synthetic" }} }}"#));
        assert_eq!(errors(&json), [(format!("/sources/s/{key}"), "connection_details_in_page")], "{key}");
    }
    // A file source may carry a url.
    assert!(errors(&base(r#", "sources": { "s": { "kind": "csv", "url": "data/x.csv" } }"#)).is_empty());
}

#[test]
fn postgres_http_loads_as_postgres() {
    let (p, _) = Page::from_json(&base(r#", "sources": { "s": { "kind": "postgres_http", "connection": "pg" } }"#)).unwrap();
    assert_eq!(p.sources["s"].kind, Some(SourceKind::Postgres));
    assert!(p.to_json().contains("\"kind\": \"postgres\""), "written back normalised");
    let (p, _) = Page::from_json(&base(r#", "sources": { "s": { "kind": "postgres", "connection": "pg" } }"#)).unwrap();
    assert_eq!(p.sources["s"].kind, Some(SourceKind::Postgres));
}

#[test]
fn max_rows_over_ceiling_is_an_error() {
    let (p, _) = Page::from_json(&base(r#", "sources": { "s": { "connection": "local", "max_rows": 500000 } }"#)).unwrap();
    let d = check_page(&p, &CheckOptions::default()).diagnostics;
    assert!(d.iter().any(|d| d.code == "max_rows_over_ceiling" && d.path == "/sources/s/max_rows"), "{d:#?}");
    // A host that registers a higher ceiling (the Explorer: 1,000,000) accepts it.
    let d = check_page(&p, &CheckOptions { max_rows_ceiling: 1_000_000, ..Default::default() }).diagnostics;
    assert!(d.iter().all(|d| d.code != "max_rows_over_ceiling"));
}

#[test]
fn sql_array_equals_string() {
    let a = SqlText::Lines(vec!["SELECT a".into(), "FROM t".into()]);
    let b = SqlText::One("SELECT a\nFROM t".into());
    assert_eq!(a.text(), b.text());
    assert_eq!(a.pointer_of(10), "/1");
    assert_eq!(b.pointer_of(10), "");
}

#[test]
fn a_localised_title_falls_back_to_en_then_the_first_key() {
    let t: Localised = serde_json::from_str(r#"{ "ja": "流入", "en": "Inflow" }"#).unwrap();
    assert_eq!(t.get("ja"), "流入");
    assert_eq!(t.get("de"), "Inflow");
    let t: Localised = serde_json::from_str(r#"{ "ja": "流入" }"#).unwrap();
    assert_eq!(t.get("de"), "流入");
}

#[test]
fn profile_map_loads_with_profile_not_checked() {
    let (_, d) = Page::from_json(&base(r#", "profile": "map""#)).unwrap();
    assert!(d.iter().any(|d| d.code == "profile_not_checked"));
    assert_eq!(errors(&base(r#", "profile": "story""#)), [("/profile".to_string(), "profile_unknown")]);
}

#[test]
fn round_trip_through_json() {
    let (p, _) = Page::from_json(include_str!("fixtures/bikeshare/station_usage.json")).unwrap();
    let (q, _) = Page::from_json(&p.to_json()).unwrap();
    assert_eq!(p, q);
}
