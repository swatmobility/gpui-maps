//! Task 1.3's `check_collection` (§11.2), over the synthetic bike-share
//! fixtures and copies of them in a temporary directory.

use std::path::PathBuf;

use swat_maps_dash::testing::check_collection;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bikeshare");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gpui-map-dash-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn check_collection_is_clean_on_the_bikeshare_fixtures() {
    let r = check_collection(FIXTURES);
    assert!(!r.pages.is_empty());
    r.assert_clean();
    assert_eq!(r.errors(), 0);
}

#[test]
fn a_bad_page_does_not_hide_the_good_ones_and_names_file_pointer_and_code() {
    let dir = scratch("bad");
    std::fs::copy(format!("{FIXTURES}/station_usage.json"), dir.join("a.json")).unwrap();
    std::fs::write(
        dir.join("b.json"),
        r#"{ "id": "b", "title": "B", "variables": [{ "key": "k", "control": "single" }],
             "filters": [{ "var": "k", "column": "c; DROP TABLE t" }] }"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/ignored.json"), "not json").unwrap();
    let r = check_collection(&dir);
    assert_eq!(r.pages.len(), 2, "subdirectories are ignored");
    assert_eq!(r.pages[0].id.as_deref(), Some("station-usage"));
    assert!(r.pages[0].diagnostics.iter().all(|d| !d.is_error()));
    let bad = &r.pages[1];
    assert!(bad.file.ends_with("b.json"));
    assert!(bad.diagnostics.iter().any(|d| d.code == "column_not_identifier" && d.path == "/filters/0/column"));
    let panic = std::panic::catch_unwind(|| r.assert_clean()).unwrap_err();
    let msg = panic.downcast_ref::<String>().unwrap();
    assert!(msg.contains("b.json: /filters/0/column [column_not_identifier]"), "{msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn duplicate_ids_load_the_first_and_flag_the_second() {
    let dir = scratch("dup");
    for f in ["a.json", "b.json"] {
        std::fs::copy(format!("{FIXTURES}/station_usage.json"), dir.join(f)).unwrap();
    }
    let r = check_collection(&dir);
    assert_eq!(r.duplicates.len(), 1);
    assert_eq!(r.duplicates[0].code, "duplicate_page_id");
    assert!(r.duplicates[0].path.ends_with("b.json"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_collection_takes_under_two_seconds_for_31_pages() {
    let dir = scratch("perf");
    let text = std::fs::read_to_string(format!("{FIXTURES}/station_usage.json")).unwrap();
    for i in 0..31 {
        std::fs::write(dir.join(format!("p{i}.json")), text.replace("\"station-usage\"", &format!("\"p{i}\""))).unwrap();
    }
    let start = std::time::Instant::now();
    let r = check_collection(&dir);
    let took = start.elapsed();
    r.assert_clean();
    assert_eq!(r.pages.len(), 31);
    assert!(took.as_secs_f64() < 2.0, "{took:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
