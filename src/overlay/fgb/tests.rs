use super::write::{write, write_with, WriteFeature, WriteOptions, WriteType, WriteValue};
use super::*;
use crate::mvt::TagValue;

/// One of the reference implementation's own test files (see
/// `tests/fixtures/fgb/LICENSE-flatgeobuf.txt`): written by its writer, not
/// this crate's, so they check the reader against the spec independently.
fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/fgb/{name}.fgb", env!("CARGO_MANIFEST_DIR"))).expect("fixture")
}

fn read(bytes: &[u8]) -> FgbSource {
    FgbSource::from_bytes(bytes, &FgbOptions::default()).expect("reads")
}

/// splitmix64 -> uniform in [0, 1).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

const COLUMNS: &[(&str, WriteType)] =
    &[("id", WriteType::Long), ("name", WriteType::String), ("flag", WriteType::Bool), ("value", WriteType::Double)];

/// `n` features: points, or small squares, with an `id` per feature.
fn generated(n: usize, polygons: bool, seed: u64) -> Vec<WriteFeature> {
    let mut r = Rng(seed);
    (0..n)
        .map(|i| {
            let (x, y) = (-170.0 + r.next() * 340.0, -80.0 + r.next() * 160.0);
            let geometry = if polygons {
                let s = 0.1 + r.next() * 2.0;
                OverlayGeometry::Polygon(vec![vec![vec![(x, y), (x + s, y), (x + s, y + s), (x, y + s), (x, y)]]])
            } else {
                OverlayGeometry::Point(vec![(x, y)])
            };
            let name = if i.is_multiple_of(7) { WriteValue::Null } else { WriteValue::String(format!("f{i}")) };
            WriteFeature {
                geometry,
                properties: vec![WriteValue::Long(i as i64), name, WriteValue::Bool(i.is_multiple_of(2)), WriteValue::Double(i as f64 * 0.5)],
            }
        })
        .collect()
}

fn ids(source: &FgbSource) -> Vec<i64> {
    let Some(CsvValues::Number(ids)) = source.column("id") else { panic!("an id column") };
    let mut ids: Vec<i64> = ids.iter().map(|v| v.expect("every feature has an id") as i64).collect();
    ids.sort_unstable();
    ids
}

/// Every feature whose box meets `bbox`, by brute force.
fn expected(features: &[WriteFeature], bbox: &[f64; 4]) -> Vec<i64> {
    features
        .iter()
        .enumerate()
        .filter(|(_, f)| intersects(&format::bbox_of(&f.geometry), bbox))
        .map(|(i, _)| i as i64)
        .collect()
}

#[test]
fn the_reference_all_types_file_reads_every_column_type() {
    let fgb = read(&fixture("alldatatypes"));
    assert_eq!(fgb.len(), 1, "{}", fgb.report());
    assert_eq!(fgb.points().unwrap(), [[0.0, 0.0]]);
    let number = |name: &str| match fgb.column(name) {
        Some(CsvValues::Number(v)) => v[0],
        other => panic!("{name}: {other:?}"),
    };
    assert_eq!(number("byte"), Some(-1.0));
    assert_eq!(number("ubyte"), Some(255.0));
    assert_eq!(number("short"), Some(-1.0));
    assert_eq!(number("ushort"), Some(65535.0));
    assert_eq!(number("int"), Some(-1.0));
    assert_eq!(number("uint"), Some(4_294_967_295.0));
    assert_eq!(number("long"), Some(-1.0));
    assert_eq!(fgb.column("bool"), Some(&CsvValues::Bool(vec![Some(true)])));
    let string = |name: &str| match fgb.column(name) {
        Some(CsvValues::Str(s)) => s.get(0).map(str::to_string),
        other => panic!("{name}: {other:?}"),
    };
    assert_eq!(string("string").as_deref(), Some("X"));
    assert_eq!(string("datetime").as_deref(), Some("2020-02-29T12:34:56Z"));
    // No CRS in this file: read as WGS84, said so.
    assert!(fgb.report().warnings.iter().any(|w| w.contains("no CRS")), "{}", fgb.report());
}

#[test]
fn the_reference_geometry_files_read_every_shape() {
    let mixed = read(&fixture("heterogeneous"));
    assert_eq!(mixed.len(), 3, "{}", mixed.report());
    assert_eq!(mixed.header().geometry_type, FgbGeometryType::Unknown);
    assert_eq!(mixed.geometry(0), Some(OverlayGeometry::Point(vec![(1.2, -2.1)])));
    assert!(mixed.report().warnings.iter().any(|w| w.contains("no spatial index")), "{}", mixed.report());

    let multi = read(&fixture("mp_overlapping"));
    assert_eq!(multi.len(), 2);
    assert_eq!(multi.report().crs, "EPSG:4326 (WGS 84)");
    let Some(OverlayGeometry::Polygon(polygons)) = multi.geometry(0) else { panic!("a multipolygon") };
    assert_eq!(polygons.len(), 2);
    assert_eq!(polygons[0][0].len(), 7);
    assert_eq!(polygons[0][0][0], (14.746050442938735, 55.27163965741248));

    let point = read(&fixture("no_properties"));
    assert_eq!(point.points().unwrap(), [[-123.1874, 48.7902]]);

    // A streamed writer's file: no count, no index -- a full scan.
    let unknown = read(&fixture("unknown_feature_count"));
    assert_eq!((unknown.header().features_count, unknown.len()), (0, 1));
    assert_eq!(unknown.column("devices"), Some(&CsvValues::Number(vec![Some(49.0)])));

    let empty = read(&fixture("empty"));
    assert!(empty.is_empty());
}

#[test]
fn a_projected_crs_is_refused_by_name() {
    let err = FgbSource::from_bytes(&fixture("poly00"), &FgbOptions::default()).unwrap_err();
    assert_eq!(err, FgbError::Crs("EPSG:27700 (OSGB 1936 / British National Grid)".into()));
    assert!(err.to_string().contains("accept_any_crs"), "{err}");
    // Accepted on request: the metres then fail the range check per feature.
    let forced = FgbSource::from_bytes(&fixture("poly00"), &FgbOptions { accept_any_crs: true, ..FgbOptions::default() }).unwrap();
    assert_eq!(forced.report().skipped_by_reason, [(FgbSkip::OutOfRange, 10)]);
}

#[test]
fn crs84_and_a_geographic_crs_on_request_are_read() {
    let features = generated(10, false, 1);
    let crs84 = write_with(&WriteOptions { crs: Some(("OGC".into(), 84)), ..WriteOptions::default() }, COLUMNS, &features);
    assert_eq!(read(&crs84).len(), 10);
    let nad83 = write_with(&WriteOptions { crs: Some(("EPSG".into(), 4269)), ..WriteOptions::default() }, COLUMNS, &features);
    assert_eq!(FgbSource::from_bytes(&nad83, &FgbOptions::default()).unwrap_err(), FgbError::Crs("EPSG:4269".into()));
    let ok = FgbSource::from_bytes(&nad83, &FgbOptions { accept_any_crs: true, ..FgbOptions::default() }).unwrap();
    assert_eq!(ok.len(), 10);
    assert!(ok.report().warnings[0].contains("EPSG:4269"), "{}", ok.report());
    // No CRS and an envelope in metres: refused rather than guessed.
    let metres = vec![WriteFeature { geometry: OverlayGeometry::Point(vec![(530_000.0, 180_000.0)]), properties: vec![] }];
    let none = write_with(&WriteOptions { crs: None, ..WriteOptions::default() }, &[], &metres);
    assert!(matches!(FgbSource::from_bytes(&none, &FgbOptions::default()), Err(FgbError::Crs(_))));
}

#[test]
fn a_written_file_reads_back_whole_with_typed_columns() {
    let features = generated(1000, false, 7);
    let bytes = write(COLUMNS, &features, 16);
    let fgb = read(&bytes);
    assert_eq!(fgb.len(), 1000, "{}", fgb.report());
    assert_eq!(fgb.report().features_total, 1000);
    assert!(fgb.report().indexed);
    assert!(fgb.report().warnings.is_empty(), "{}", fgb.report());
    assert_eq!(ids(&fgb), (0..1000).collect::<Vec<_>>());
    // Feature by feature: the file index maps back to the id, and the
    // position and every column agree with what was written.
    let Some(CsvValues::Number(id)) = fgb.column("id") else { panic!() };
    let Some(CsvValues::Str(name)) = fgb.column("name") else { panic!() };
    let Some(CsvValues::Bool(flag)) = fgb.column("flag") else { panic!() };
    for i in 0..fgb.len() {
        let k = id[i].unwrap() as usize;
        assert_eq!(fgb.geometry(i).as_ref(), Some(&features[k].geometry));
        assert_eq!(name.get(i), (!k.is_multiple_of(7)).then(|| format!("f{k}")).as_deref());
        assert_eq!(flag[i], Some(k.is_multiple_of(2)));
    }
    let bbox = fgb.report().bbox.unwrap();
    assert!(bbox[0] >= -170.0 && bbox[2] <= 170.0 && bbox[0] < bbox[2], "{bbox:?}");
}

#[test]
fn an_index_query_returns_exactly_the_features_the_box_meets() {
    for (polygons, node_size) in [(false, 16), (true, 16), (false, 2), (true, 7)] {
        let features = generated(5000, polygons, 11);
        let bytes = write(COLUMNS, &features, node_size);
        let mut r = Rng(3);
        for _ in 0..40 {
            let (x, y) = (-180.0 + r.next() * 360.0, -90.0 + r.next() * 180.0);
            let (w, h) = (r.next() * 60.0, r.next() * 30.0);
            let bbox = [x, y, x + w, y + h];
            let fgb = FgbSource::from_bytes(&bytes, &FgbOptions::default().with_bbox(bbox)).unwrap();
            assert_eq!(ids(&fgb), expected(&features, &bbox), "polygons {polygons} node {node_size} bbox {bbox:?}");
        }
        // A box that meets nothing, and one over everything.
        let none = FgbSource::from_bytes(&bytes, &FgbOptions::default().with_bbox([179.5, 89.5, 179.6, 89.6])).unwrap();
        assert!(none.is_empty());
        let all = FgbSource::from_bytes(&bytes, &FgbOptions::default().with_bbox([-180.0, -90.0, 180.0, 90.0])).unwrap();
        assert_eq!(all.len(), 5000);
    }
}

#[test]
fn a_bbox_query_reads_a_fraction_of_the_file() {
    let features = generated(50_000, false, 5);
    let bytes = write(COLUMNS, &features, 16);
    let bbox = [10.0, 10.0, 20.0, 20.0];
    let mut reader = SliceReader { bytes: &bytes, read: 0, requests: 0 };
    let fgb = block_ready(read_source(&mut reader, &FgbOptions::default().with_bbox(bbox))).unwrap();
    assert_eq!(ids(&fgb), expected(&features, &bbox));
    let (read, requests) = reader.counters();
    assert!(read * 20 < bytes.len() as u64, "read {read} of {} bytes", bytes.len());
    // Header, one read per index level (5 levels), then the features.
    assert!(requests <= 12, "{requests} requests");
}

#[test]
fn a_file_query_matches_the_bytes_query() {
    let features = generated(3000, true, 9);
    let bytes = write(COLUMNS, &features, 16);
    let dir = std::env::temp_dir().join(format!("gpui-maps-fgb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("squares.fgb");
    std::fs::write(&path, &bytes).unwrap();
    let bbox = [-30.0, -10.0, 15.0, 25.0];
    let from_file = FgbSource::from_path(&path, &FgbOptions::default().with_bbox(bbox)).unwrap();
    assert_eq!(ids(&from_file), expected(&features, &bbox));
    assert!(from_file.report().bytes_read < bytes.len() as u64 / 2, "{}", from_file.report().bytes_read);
    assert_eq!(FgbSource::from_path(&path, &FgbOptions::default()).unwrap().len(), 3000);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unindexed_file_is_scanned_and_filtered_with_a_warning() {
    let features = generated(2000, false, 13);
    let bytes = write(COLUMNS, &features, 0);
    let bbox = [0.0, 0.0, 40.0, 40.0];
    let fgb = FgbSource::from_bytes(&bytes, &FgbOptions::default().with_bbox(bbox)).unwrap();
    assert!(!fgb.report().indexed);
    assert_eq!(ids(&fgb), expected(&features, &bbox));
    assert_eq!(fgb.report().features_read, fgb.len());
    assert!(fgb.report().warnings.iter().any(|w| w.contains("no spatial index")), "{}", fgb.report());
}

#[test]
fn bad_bytes_are_errors_never_panics() {
    assert_eq!(FgbSource::from_bytes(b"", &FgbOptions::default()).unwrap_err(), FgbError::NotFlatGeobuf);
    assert_eq!(FgbSource::from_bytes(b"PK\x03\x04 not an fgb", &FgbOptions::default()).unwrap_err(), FgbError::NotFlatGeobuf);
    let mut v4 = fixture("no_properties");
    v4[3] = 4;
    assert!(matches!(FgbSource::from_bytes(&v4, &FgbOptions::default()), Err(FgbError::Unsupported(_))));
    let bytes = write(COLUMNS, &generated(300, true, 2), 16);
    // Every truncation, and a sweep of corrupted bytes: an error or a
    // partial read, but never a panic.
    for cut in (0..bytes.len()).step_by(97) {
        let _ = FgbSource::from_bytes(&bytes[..cut], &FgbOptions::default());
        let _ = FgbSource::from_bytes(&bytes[..cut], &FgbOptions::default().with_bbox([0.0, 0.0, 10.0, 10.0]));
    }
    let mut r = Rng(17);
    for _ in 0..300 {
        let mut bad = bytes.clone();
        for _ in 0..8 {
            let at = 8 + (r.next() * (bad.len() - 8) as f64) as usize;
            bad[at] = (r.next() * 256.0) as u8;
        }
        let _ = FgbSource::from_bytes(&bad, &FgbOptions::default());
        let _ = FgbSource::from_bytes(&bad, &FgbOptions::default().with_bbox([-50.0, -50.0, 50.0, 50.0]));
    }
    // A file over the byte cap is refused unread.
    let small = FgbOptions { max_bytes: 100, ..FgbOptions::default() };
    assert!(matches!(FgbSource::from_bytes(&bytes, &small), Err(FgbError::TooLarge { limit: 100, .. })));
}

#[test]
fn unsupported_and_out_of_range_features_are_skipped_and_counted() {
    let features = vec![
        WriteFeature { geometry: OverlayGeometry::Point(vec![(1.0, 2.0)]), properties: vec![] },
        WriteFeature { geometry: OverlayGeometry::Point(vec![(200.0, 2.0)]), properties: vec![] },
        WriteFeature { geometry: OverlayGeometry::Line(vec![vec![(0.0, 0.0), (1.0, 1.0)]]), properties: vec![] },
    ];
    let fgb = read(&write(&[], &features, 16));
    assert_eq!(fgb.len(), 2);
    assert_eq!(fgb.report().skipped_by_reason, [(FgbSkip::OutOfRange, 1)]);
    assert_eq!(fgb.report().to_string(), "loaded 2 of 3 features; skipped 1 (1 coordinate out of range)");
    // A line among points: the geometry is mixed, the point path is off.
    assert!(fgb.points().is_none());
}

#[test]
fn a_stream_merge_dedups_evicts_outside_the_view_and_stays_bounded() {
    let features = generated(4000, false, 21);
    let bytes = write(COLUMNS, &features, 16);
    let query = |bbox: [f64; 4]| FgbSource::from_bytes(&bytes, &FgbOptions::default().with_bbox(bbox)).unwrap();
    let header = read(&bytes).header().clone();
    let mut stream = FgbSource::empty_like(&header);
    let west = [-170.0, -80.0, 0.0, 80.0];
    let first = query(west);
    let n_west = first.len();
    assert!(stream.merge_stream(first, west, 10_000).is_empty());
    assert_eq!(stream.len(), n_west);
    // Room for everything: the east half joins, nothing is dropped.
    let east = [0.0, -80.0, 170.0, 80.0];
    let second = query(east);
    let n_east = second.len();
    assert!(stream.merge_stream(second, east, 10_000).is_empty());
    assert_eq!(stream.len(), n_west + n_east);
    // Over the bound: what is outside the new view goes first, then the
    // rest is thinned evenly to the bound.
    let mut stream = FgbSource::empty_like(&header);
    stream.merge_stream(query(west), west, 2500);
    let dropped = stream.merge_stream(query(east), east, 2500);
    assert!(stream.len() <= 2500);
    assert!(dropped.len() >= n_west);
    for i in 0..stream.len() {
        let g = stream.geometry(i).unwrap();
        assert!(intersects(&format::bbox_of(&g), &east), "a west feature survived");
    }
    assert!(stream.report().streaming);
    assert!(stream.report().to_string().contains("streaming by viewport"));
}

/// A streamed source under a long pan: a 60-degree view slides east round
/// the world three times, each step's query merged under a cap of 600. The
/// resident set never passes the cap.
#[test]
fn a_stream_stays_bounded_over_a_long_pan() {
    let features = generated(4000, false, 22);
    let bytes = write(COLUMNS, &features, 16);
    let header = read(&bytes).header().clone();
    let mut stream = FgbSource::empty_like(&header);
    let mut merged = 0;
    for step in 0..3 * 34 {
        let west = -170.0 + f64::from(step % 34) * 10.0;
        let view = [west, -80.0, (west + 60.0).min(180.0), 80.0];
        let more = FgbSource::from_bytes(&bytes, &FgbOptions::default().with_bbox(view)).unwrap();
        merged += more.len();
        stream.merge_stream(more, view, 600);
        assert!(stream.len() <= 600, "step {step}: {} resident", stream.len());
    }
    assert!(merged > 20 * 600, "the pan must read far more than the cap: {merged}");
}

#[test]
fn points_take_the_point_path_and_polygons_the_feature_path_with_properties() {
    let points = read(&write(COLUMNS, &generated(50, false, 1), 16));
    let mut store = crate::overlay::OverlayStore::new();
    store.set_source_fgb("pts", &points, &FeaturePaint::default(), 3).unwrap();
    let data = store.source_point_data("pts").expect("a point source");
    assert_eq!(data.len(), 50);
    let row = data.properties.row_map(0);
    assert!(matches!(row.get("value"), Some(TagValue::Number(_))));
    assert!(matches!(row.get("flag"), Some(TagValue::Bool(_))));

    let squares = read(&write(COLUMNS, &generated(20, true, 1), 16));
    store.set_source_fgb("squares", &squares, &FeaturePaint::default(), 3).unwrap();
    assert_eq!(store.source_features("squares").unwrap().len(), 20);
    let props = store.feature_properties("squares", 0).unwrap();
    assert!(matches!(props.get("id"), Some(TagValue::Number(_))));
    // A paint expression over a typed column compiles against it.
    let paint = serde_json::json!({ "fill-color": ["case", ["==", ["get", "flag"], true], "#ff0000", "#0000ff"] });
    store.declare_source_paint("squares", paint.as_object().unwrap()).expect("compiles");
}

#[test]
fn the_viewport_box_is_degrees_and_clamped() {
    let cam = |lon: f64, lat: f64, zoom: f64| crate::render::Camera {
        center_lon: lon,
        center_lat: lat,
        zoom,
        pitch: 0.0,
        bearing: 0.0,
        viewport_width_px: 1280.0,
        viewport_height_px: 800.0,
        fov_y: crate::render::Camera::DEFAULT_FOV_Y,
    };
    let camera = cam(2.0, 50.0, 6.0);
    let b = viewport_bbox(&camera, 0.0);
    assert!(b[0] < 2.0 && b[2] > 2.0 && b[1] < 50.0 && b[3] > 50.0, "{b:?}");
    assert!(b[2] - b[0] < 20.0, "{b:?}");
    let world = viewport_bbox(&cam(0.0, 0.0, 0.0), 0.5);
    assert_eq!((world[0], world[2]), (-180.0, 180.0));
}

#[test]
fn the_user_agent_names_the_project_and_nothing_personal() {
    assert!(FGB_USER_AGENT.contains("github.com/swatmobility/gpui-maps"));
    assert!(!FGB_USER_AGENT.contains('@'));
}


/// The Studio's bundled sample (`tests/fixtures/fgb/world_cities.fgb`) is
/// this writer's output for the public-domain cities CSV beside it, so it
/// can be rebuilt (`FGB_BLESS=1`) rather than trusted as an opaque blob.
#[test]
fn the_bundled_cities_file_is_the_cities_csv_written() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let csv = std::fs::read(format!("{dir}/tests/fixtures/world_cities.csv")).unwrap();
    let csv = crate::overlay::csv::CsvSource::from_bytes(&csv, &Default::default()).unwrap();
    let text = |name: &str, i: usize| match csv.column(name) {
        Some(CsvValues::Str(s)) => s.get(i).map_or(WriteValue::Null, |v| WriteValue::String(v.to_string())),
        _ => WriteValue::Null,
    };
    let features: Vec<WriteFeature> = (0..csv.len())
        .map(|i| {
            let [lon, lat] = csv.points().unwrap()[i];
            let population = match csv.column("population") {
                Some(CsvValues::Number(v)) => v[i].map_or(WriteValue::Null, |p| WriteValue::Long(p as i64)),
                _ => WriteValue::Null,
            };
            let capital = match csv.column("capital") {
                Some(CsvValues::Bool(v)) => v[i].map_or(WriteValue::Null, WriteValue::Bool),
                _ => WriteValue::Null,
            };
            WriteFeature {
                geometry: OverlayGeometry::Point(vec![(lon, lat)]),
                properties: vec![text("name", i), text("country", i), population, capital],
            }
        })
        .collect();
    let columns =
        [("name", WriteType::String), ("country", WriteType::String), ("population", WriteType::Long), ("capital", WriteType::Bool)];
    let options = WriteOptions { name: "world_cities".into(), ..WriteOptions::default() };
    let bytes = write_with(&options, &columns, &features);
    let path = format!("{dir}/tests/fixtures/fgb/world_cities.fgb");
    if std::env::var_os("FGB_BLESS").is_some() {
        std::fs::write(&path, &bytes).unwrap();
    }
    assert_eq!(std::fs::read(&path).expect("the bundled file (FGB_BLESS=1 writes it)"), bytes);
    let fgb = read(&bytes);
    assert_eq!(fgb.len(), 46);
    assert_eq!(fgb.report().columns.len(), 4);
}

#[test]
fn the_header_reads_alone_from_the_file_head() {
    let bytes = fixture("alldatatypes");
    let h = read_header(&bytes).unwrap();
    assert_eq!(h.features_count, 1);
    assert!(h.columns.iter().any(|c| c.name == "datetime" && c.kind == FgbColumnType::DateTime));
    assert_eq!(read_header(b"not a flatgeobuf file").unwrap_err(), FgbError::NotFlatGeobuf);
}
