//! `CompiledQuery::plain` (explorer-sql P3): the Explorer's SQL as written.

use swat_maps_dash::sql::{CompiledQuery, Dialect, GuardOptions};

fn plain(sql: &str) -> Result<CompiledQuery, Vec<&'static str>> {
    CompiledQuery::plain(sql, Dialect::ClickHouse, &GuardOptions::default(), 1_000).map(|(q, _)| q).map_err(|e| e.iter().map(|i| i.code).collect())
}

#[test]
fn a_select_compiles_with_its_text_behind_the_guard() {
    let q = plain("SELECT trip_id, fare FROM trips_synthetic").unwrap();
    assert_eq!(q.guarded(Dialect::ClickHouse).unwrap().sql(), "SELECT trip_id, fare FROM trips_synthetic");
    assert_eq!(q.max_rows(), 1_000);
    assert!(q.params().is_empty());
}

#[test]
fn one_trailing_semicolon_is_a_habit_not_a_statement() {
    let q = plain("SELECT 1 AS n;  \n").unwrap();
    assert_eq!(q.guarded(Dialect::ClickHouse).unwrap().sql(), "SELECT 1 AS n");
    assert!(plain("SELECT 1; DROP TABLE t").is_err());
}

#[test]
fn writes_and_templates_and_empty_text_are_refused() {
    assert!(plain("DROP TABLE trips").is_err());
    assert!(plain("/*x*/DROP TABLE t").is_err());
    assert_eq!(plain("SELECT * FROM t WHERE city = ${city}").unwrap_err(), ["template_needs_parameters"]);
    assert_eq!(plain("   ").unwrap_err(), ["sql_empty"]);
    assert!(plain("SELECT 'unterminated").is_err());
}

#[test]
fn a_comment_with_a_semicolon_is_one_statement() {
    let q = plain("SELECT 1 -- what; ever\nFROM system.one").unwrap();
    assert!(q.guarded(Dialect::ClickHouse).unwrap().sql().ends_with("FROM system.one"));
}
