use super::*;

fn parse_str(text: &str) -> CsvSource {
    CsvSource::from_bytes(text.as_bytes(), &CsvOptions::default()).expect("parses")
}

fn strings(csv: &CsvSource, name: &str) -> Vec<Option<String>> {
    match csv.column(name).expect("column") {
        CsvValues::Str(s) => (0..s.len()).map(|i| s.get(i).map(str::to_string)).collect(),
        other => panic!("{name} is {:?}, not a string column", other.kind()),
    }
}

#[test]
fn quoted_fields_hold_delimiters_line_breaks_and_doubled_quotes() {
    let csv = parse_str(
        "name,lat,lon,note\r\n\
         \"Smith, John\",1,2,\"said \"\"hi\"\"\"\r\n\
         plain,3,4,\"two\nlines\"\r\n\
         \"\",5,6,\r\n",
    );
    assert_eq!(csv.report().rows_skipped, 0, "{}", csv.report());
    assert_eq!(strings(&csv, "name"), [Some("Smith, John".into()), Some("plain".into()), None]);
    assert_eq!(strings(&csv, "note"), [Some("said \"hi\"".into()), Some("two\nlines".into()), None]);
    assert_eq!(csv.points().unwrap(), [[2.0, 1.0], [4.0, 3.0], [6.0, 5.0]]);
}

#[test]
fn a_line_break_inside_quotes_does_not_throw_off_the_line_numbers() {
    let csv = parse_str("name,lat,lon\n\"a\nb\",1,2\nbad,x,2\n");
    assert_eq!(csv.report().first_skipped, [(4, CsvSkip::BadCoordinate)]);
}

#[test]
fn the_delimiter_is_sniffed_or_given() {
    for (text, want) in [
        ("lat,lon,name\n1,2,a\n3,4,b\n", b','),
        ("lat\tlon\tname\n1\t2\ta, b\n3\t4\tc\n", b'\t'),
        ("lat;lon;name\n1;2;a,b\n3;4;c\n", b';'),
        // A comma inside every name must not win over the real delimiter.
        ("name;lat;lon\n\"x, y\";1;2\n\"z, w\";3;4\n", b';'),
    ] {
        let csv = parse_str(text);
        assert_eq!(csv.report().delimiter, want, "{text:?}");
        assert_eq!(csv.len(), 2, "{text:?}: {}", csv.report());
    }
    let pipe = CsvSource::from_bytes(b"lat|lon\n1|2\n", &CsvOptions::default().with_delimiter(b'|')).unwrap();
    assert_eq!(pipe.points().unwrap(), [[2.0, 1.0]]);
}

#[test]
fn a_semicolon_file_reads_decimal_commas() {
    let csv = parse_str("lat;lon;value\n52,52;13,40;1,5\n");
    assert_eq!(csv.points().unwrap(), [[13.40, 52.52]]);
    assert_eq!(csv.column("value"), Some(&CsvValues::Number(vec![Some(1.5)])));
}

#[test]
fn a_byte_order_mark_is_not_part_of_the_first_header() {
    let csv = CsvSource::from_bytes(b"\xEF\xBB\xBFlat,lon,id\n1,2,a\n", &CsvOptions::default()).unwrap();
    assert_eq!(csv.report().geometry, CsvGeometryColumns::LatLon { lat: "lat".into(), lon: "lon".into() });
    assert!(csv.column("id").is_some());
}

#[test]
fn invalid_utf8_skips_its_row_only() {
    let csv = CsvSource::from_bytes(b"lat,lon,name\n1,2,ok\n3,4,\xFF\xFE\n5,6,fine\n", &CsvOptions::default()).unwrap();
    assert_eq!(csv.len(), 2);
    assert_eq!(csv.report().first_skipped, [(3, CsvSkip::InvalidUtf8)]);
}

#[test]
fn lat_lon_columns_are_found_by_every_common_name() {
    for (lat, lon) in [("lat", "lon"), ("Latitude", "Longitude"), ("LAT", "LNG"), ("y", "x"), ("lat", "long")] {
        let csv = parse_str(&format!("id,{lon},{lat}\n1,103.8,1.35\n"));
        assert_eq!(csv.points().unwrap(), [[103.8, 1.35]], "{lat}/{lon}");
        assert_eq!(csv.report().columns, [("id".to_string(), CsvColumnType::Number)]);
    }
}

#[test]
fn explicit_columns_override_detection() {
    let options = CsvOptions::default().with_lat_lon("northing_deg", "easting_deg");
    let csv = CsvSource::from_bytes(b"lat,lon,northing_deg,easting_deg\n0,0,10,20\n", &options).unwrap();
    assert_eq!(csv.points().unwrap(), [[20.0, 10.0]]);
    // The unused auto-detectable names are ordinary properties.
    assert_eq!(csv.report().columns.len(), 2);
    let missing = CsvSource::from_bytes(b"a,b\n1,2\n", &CsvOptions::default().with_wkt("shape"));
    assert_eq!(missing.unwrap_err(), CsvError::MissingColumn("shape".into()));
}

#[test]
fn no_geometry_column_is_a_file_error_naming_the_header() {
    let err = CsvSource::from_bytes(b"name,population\na,1\n", &CsvOptions::default()).unwrap_err();
    assert_eq!(err, CsvError::NoGeometry(vec!["name".into(), "population".into()]));
    assert!(err.to_string().contains("name, population"));
    assert_eq!(CsvSource::from_bytes(b"", &CsvOptions::default()).unwrap_err(), CsvError::Empty);
    assert_eq!(CsvSource::from_bytes(b"\n\r\n", &CsvOptions::default()).unwrap_err(), CsvError::Empty);
}

#[test]
fn wkt_columns_take_every_geometry_type() {
    let csv = parse_str(
        "id,wkt\n\
         1,POINT (1 2)\n\
         2,\"LINESTRING (0 0, 1 1)\"\n\
         3,\"POLYGON ((0 0, 1 0, 1 1, 0 0))\"\n\
         4,\"MULTIPOINT ((1 2), (3 4))\"\n\
         5,\"MULTILINESTRING ((0 0, 1 1), (2 2, 3 3))\"\n\
         6,\"MULTIPOLYGON (((0 0, 1 0, 1 1, 0 0)))\"\n\
         7,\"GEOMETRYCOLLECTION (POINT (1 2))\"\n\
         8,POINT EMPTY\n\
         9,POINT (1\n\
         10,POINT (500000 4000000)\n",
    );
    assert_eq!(csv.report().geometry, CsvGeometryColumns::Wkt("wkt".into()));
    assert_eq!(csv.len(), 6, "{}", csv.report());
    assert!(csv.points().is_none(), "a line makes it a mixed source");
    assert_eq!(csv.geometry(0), Some(OverlayGeometry::Point(vec![(1.0, 2.0)])));
    assert!(matches!(csv.geometry(1), Some(OverlayGeometry::Line(_))));
    assert!(matches!(csv.geometry(5), Some(OverlayGeometry::Polygon(_))));
    let reasons: Vec<CsvSkip> = csv.report().first_skipped.iter().map(|(_, why)| *why).collect();
    assert_eq!(
        reasons,
        [CsvSkip::UnsupportedGeometry, CsvSkip::MissingGeometry, CsvSkip::BadWkt, CsvSkip::OutOfRange]
    );
}

#[test]
fn a_wkt_column_of_points_is_a_point_source() {
    let csv = parse_str("geometry,name\nPOINT(103.8 1.35),sg\n\"SRID=4326;POINT(139.7 35.7)\",tk\n");
    assert_eq!(csv.points().unwrap(), [[103.8, 1.35], [139.7, 35.7]]);
}

#[test]
fn a_geometry_column_of_json_objects_is_geojson() {
    let csv = parse_str(
        "name,geometry\n\
         a,\"{\"\"type\"\":\"\"Point\"\",\"\"coordinates\"\":[1,2]}\"\n\
         b,\"{\"\"type\"\":\"\"LineString\"\",\"\"coordinates\"\":[[0,0],[1,1]]}\"\n\
         c,\"{\"\"type\"\":\"\"Point\"\"}\"\n\
         d,not json\n",
    );
    assert_eq!(csv.report().geometry, CsvGeometryColumns::GeoJson("geometry".into()));
    assert_eq!(csv.len(), 2, "{}", csv.report());
    assert_eq!(csv.report().skipped_by_reason, [(CsvSkip::BadGeoJson, 2)]);
    let named = CsvSource::from_bytes(
        b"shape\n\"{\"\"type\"\":\"\"Point\"\",\"\"coordinates\"\":[5,6]}\"\n",
        &CsvOptions::default().with_geojson("SHAPE"),
    )
    .unwrap();
    assert_eq!(named.points().unwrap(), [[5.0, 6.0]]);
}

#[test]
fn properties_are_typed_by_their_whole_column() {
    let csv = parse_str(
        "lat,lon,pop,capital,zip,name,mixed,blank\n\
         1,2,100,true,02134,a,1,\n\
         3,4,2.5e3,FALSE,10001,b,x,\n\
         5,6,,True,,c,2,\n",
    );
    assert_eq!(csv.column("pop"), Some(&CsvValues::Number(vec![Some(100.0), Some(2500.0), None])));
    assert_eq!(csv.column("capital"), Some(&CsvValues::Bool(vec![Some(true), Some(false), Some(true)])));
    assert_eq!(strings(&csv, "zip"), [Some("02134".into()), Some("10001".into()), None], "a leading zero is an id");
    assert_eq!(strings(&csv, "mixed"), [Some("1".into()), Some("x".into()), Some("2".into())]);
    assert_eq!(strings(&csv, "blank"), [None, None, None]);
    let types: Vec<CsvColumnType> = csv.report().columns.iter().map(|(_, t)| *t).collect();
    use CsvColumnType::{Bool, Number, String as Str};
    assert_eq!(types, [Number, Bool, Str, Str, Str, Str]);
}

#[test]
fn nan_and_infinity_are_strings_not_numbers() {
    let csv = parse_str("lat,lon,v\n1,2,NaN\n3,4,inf\n");
    assert!(matches!(csv.column("v"), Some(CsvValues::Str(_))));
    let csv = parse_str("lat,lon\nNaN,2\n1,inf\n");
    assert_eq!(csv.report().skipped_by_reason, [(CsvSkip::BadCoordinate, 2)]);
}

#[test]
fn bad_rows_are_counted_by_reason_with_their_first_lines() {
    let csv = parse_str(
        "name,lat,lon\n\
         ok,1,2\n\
         short,1\n\
         nolat,,2\n\
         far,95,2\n\
         word,north,2\n\
         \"stray\"x,1,2\n\
         long,1,2,3\n\
         \n\
         ok2,3,4\n\
         \"open,1,2\n",
    );
    let report = csv.report();
    assert_eq!((report.rows_read, report.rows_loaded, report.rows_skipped), (9, 2, 7), "{report}");
    assert_eq!(
        report.first_skipped,
        [
            (3, CsvSkip::FieldCount),
            (4, CsvSkip::MissingGeometry),
            (5, CsvSkip::OutOfRange),
            (6, CsvSkip::BadCoordinate),
            (7, CsvSkip::StrayQuote),
            (8, CsvSkip::FieldCount),
            (11, CsvSkip::UnterminatedQuote),
        ]
    );
    assert_eq!(report.skipped_by_reason[0], (CsvSkip::FieldCount, 2));
    let text = report.to_string();
    assert!(text.starts_with("loaded 2 of 9 rows; skipped 7"), "{text}");
    assert!(text.contains("first at line 3, 4, 5"), "{text}");
}

#[test]
fn only_the_first_few_skipped_lines_are_kept() {
    let mut text = String::from("lat,lon\n");
    for _ in 0..100 {
        text.push_str("x,y\n");
    }
    let csv = parse_str(&text);
    assert_eq!(csv.report().rows_skipped, 100);
    assert_eq!(csv.report().first_skipped.len(), FIRST_SKIPPED_KEPT);
    assert_eq!(csv.report().first_skipped[0].0, 2);
}

#[test]
fn duplicate_and_blank_headers_get_unique_names() {
    let csv = parse_str("lat,lon,name,name,\n1,2,a,b,c\n");
    let names: Vec<&str> = csv.report().columns.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["name", "name_2", "column_5"]);
}

#[test]
fn a_file_over_the_byte_limit_is_refused() {
    let options = CsvOptions { max_bytes: 8, ..CsvOptions::default() };
    assert_eq!(
        CsvSource::from_bytes(b"lat,lon\n1,2\n", &options).unwrap_err(),
        CsvError::TooLarge { limit: 8, got: 12 }
    );
}

#[test]
fn a_file_path_reads_and_a_missing_file_is_an_io_error() {
    let dir = std::env::temp_dir().join(format!("gpui-maps-csv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.tsv");
    std::fs::write(&path, "lat\tlon\n1\t2\n").unwrap();
    let csv = CsvSource::from_path(&path, &CsvOptions::default()).unwrap();
    assert_eq!(csv.points().unwrap(), [[2.0, 1.0]]);
    let too_small = CsvOptions { max_bytes: 3, ..CsvOptions::default() };
    assert!(matches!(CsvSource::from_path(&path, &too_small), Err(CsvError::TooLarge { .. })));
    assert!(matches!(CsvSource::from_path(dir.join("nope.csv"), &CsvOptions::default()), Err(CsvError::Io(_))));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_user_agent_names_the_project_and_nothing_personal() {
    assert!(CSV_USER_AGENT.contains("github.com/swatmobility/gpui-maps"));
    assert!(!CSV_USER_AGENT.contains('@'));
}

#[test]
fn a_point_csv_installs_on_the_point_path_with_typed_properties() {
    let csv = parse_str("name,lat,lon,pop,capital\nSingapore,1.29,103.85,5.9e6,true\nTokyo,35.68,139.69,1.4e7,true\n");
    let mut store = crate::overlay::OverlayStore::new();
    store.set_source_csv("cities", &csv, &FeaturePaint::default(), 3).unwrap();
    let data = store.source_point_data("cities").expect("a point source");
    assert_eq!(data.len(), 2);
    assert_eq!(data.lon_lat[1], [139.69, 35.68]);
    let props = data.properties.row_map(1);
    assert_eq!(props.get("pop"), Some(&crate::mvt::TagValue::Number(1.4e7)));
    assert_eq!(props.get("capital"), Some(&crate::mvt::TagValue::Bool(true)));
    assert_eq!(props.get("name"), Some(&crate::mvt::TagValue::String("Tokyo".into())));
}

#[test]
fn a_mixed_csv_installs_as_features_with_properties() {
    let csv = parse_str("id,wkt\n1,\"LINESTRING (0 0, 1 1)\"\n2,POINT (5 5)\n");
    let mut store = crate::overlay::OverlayStore::new();
    store.set_source_csv("shapes", &csv, &FeaturePaint::default(), 3).unwrap();
    let features = store.source_features("shapes").unwrap();
    assert_eq!(features.len(), 2);
    assert!(matches!(features[0].geometry, OverlayGeometry::Line(_)));
    let props = store.feature_properties("shapes", 1).unwrap();
    assert_eq!(props.get("id"), Some(&crate::mvt::TagValue::Number(2.0)));
    // A declared point source refuses lines, and is left as it was.
    store.set_source_cluster_options("pts", Some(crate::overlay::ClusterOptions::points_only()), 3).unwrap();
    assert!(store.set_source_csv("pts", &csv, &FeaturePaint::default(), 3).is_err());
}

#[test]
fn a_placeless_csv_reads_as_a_table_with_typed_columns() {
    let text = "month,category,spending\n2015-01-01,Food,72000\n2015-02-01,\"Food, other\",68000\n2015-03-01,Housing,\n";
    // A source refuses it: there is nothing to place.
    assert!(matches!(CsvSource::from_bytes(text.as_bytes(), &CsvOptions::default()), Err(CsvError::NoGeometry(_))));
    let t = CsvTable::from_bytes(text.as_bytes(), &CsvOptions::default()).unwrap();
    assert_eq!(t.len(), 3);
    assert_eq!(t.rows_read(), 3);
    assert_eq!(t.columns().iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["month", "category", "spending"]);
    assert_eq!(t.column("spending"), Some(&CsvValues::Number(vec![Some(72000.0), Some(68000.0), None])));
    match t.column("category") {
        Some(CsvValues::Str(s)) => assert_eq!(s.get(1), Some("Food, other")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_table_keeps_place_columns_as_columns_and_skips_ragged_rows() {
    let text = "name;lat;lon\nA;1,5;2\nB;3\nC;4;5\n";
    let t = CsvTable::from_bytes(text.as_bytes(), &CsvOptions::default().with_lat_lon("lat", "lon")).unwrap();
    assert_eq!(t.delimiter(), b';');
    assert_eq!(t.len(), 2);
    assert_eq!(t.skipped_by_reason(), &[(CsvSkip::FieldCount, 1)]);
    assert_eq!(t.column("lat"), Some(&CsvValues::Number(vec![Some(1.5), Some(4.0)])));
}
