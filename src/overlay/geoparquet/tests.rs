use super::write::{wkb, write, write_streamed, StreamedWriteError, WriteCompression, WriteFeature, WriteOptions, WriteType, WriteValue};
use super::*;

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

fn read(bytes: &[u8], options: &GeoParquetOptions) -> GeoParquetSource {
    GeoParquetSource::from_bytes(bytes, options).expect("reads")
}

fn numbers(source: &GeoParquetSource, name: &str) -> Vec<Option<f64>> {
    match source.column(name) {
        Some(CsvValues::Number(v)) => v.clone(),
        other => panic!("{name}: {other:?}"),
    }
}

fn point(x: f64, y: f64, id: i64) -> WriteFeature {
    WriteFeature { geometry: OverlayGeometry::Point(vec![(x, y)]), properties: vec![WriteValue::Int(Some(id))] }
}

const ID: [(&str, WriteType); 1] = [("id", WriteType::Int)];

/// 10 row groups of 100 points; group `g` spans longitudes `g*10 .. g*10+9.9`.
fn banded(options: &WriteOptions) -> Vec<u8> {
    let features: Vec<WriteFeature> =
        (0..1000).map(|i| point(f64::from(i / 100) * 10.0 + f64::from(i % 100) * 0.099, f64::from(i % 50) - 25.0, i64::from(i))).collect();
    write(&ID, &features, &WriteOptions { row_group_rows: 100, ..options.clone() }).unwrap()
}

#[test]
fn points_round_trip_with_typed_columns_and_nulls() {
    let columns = [("n", WriteType::Number), ("i", WriteType::Int), ("b", WriteType::Bool), ("s", WriteType::Str)];
    let features: Vec<WriteFeature> = (0..5)
        .map(|i| WriteFeature {
            geometry: OverlayGeometry::Point(vec![(f64::from(i), -f64::from(i))]),
            properties: vec![
                WriteValue::Number((i != 1).then_some(f64::from(i) * 1.5)),
                WriteValue::Int(Some(i64::from(i) * 10)),
                WriteValue::Bool((i != 2).then_some(i % 2 == 0)),
                WriteValue::Str((i != 3).then(|| format!("row {i}"))),
            ],
        })
        .collect();
    let bytes = write(&columns, &features, &WriteOptions::default()).unwrap();
    let source = read(&bytes, &GeoParquetOptions::default());
    assert_eq!(source.len(), 5);
    assert_eq!(source.points().unwrap()[3], [3.0, -3.0]);
    assert_eq!(numbers(&source, "n"), vec![Some(0.0), None, Some(3.0), Some(4.5), Some(6.0)]);
    assert_eq!(numbers(&source, "i"), vec![Some(0.0), Some(10.0), Some(20.0), Some(30.0), Some(40.0)]);
    let Some(CsvValues::Bool(b)) = source.column("b") else { panic!() };
    assert_eq!(b, &vec![Some(true), Some(false), None, Some(false), Some(true)]);
    let Some(CsvValues::Str(s)) = source.column("s") else { panic!() };
    assert_eq!((s.get(0), s.get(3), s.get(4)), (Some("row 0"), None, Some("row 4")));
    let r = source.report();
    assert_eq!(r.version, "1.1.0");
    assert_eq!(r.encoding, GeoParquetEncoding::Wkb);
    assert_eq!(r.columns.len(), 4);
    // The covering column is read for pushdown, not loaded as a property.
    assert!(source.column("bbox").is_none());
    assert_eq!(r.crs, "OGC:CRS84 (by default)");
}

#[test]
fn lines_polygons_and_multis_round_trip_through_wkb() {
    let ring = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 0.0)];
    let hole = vec![(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 2.0)];
    let geometries = [
        OverlayGeometry::Line(vec![vec![(1.0, 1.0), (2.0, 2.0), (3.0, 1.0)]]),
        OverlayGeometry::Line(vec![vec![(1.0, 1.0), (2.0, 2.0)], vec![(5.0, 5.0), (6.0, 6.0)]]),
        OverlayGeometry::Polygon(vec![vec![ring.clone(), hole]]),
        OverlayGeometry::Polygon(vec![vec![ring.clone()], vec![ring.iter().map(|(x, y)| (x + 20.0, *y)).collect()]]),
        OverlayGeometry::Point(vec![(1.0, 2.0), (3.0, 4.0)]),
    ];
    let features: Vec<WriteFeature> =
        geometries.iter().map(|g| WriteFeature { geometry: g.clone(), properties: vec![] }).collect();
    let bytes = write(&[], &features, &WriteOptions::default()).unwrap();
    let source = read(&bytes, &GeoParquetOptions::default());
    assert_eq!(source.len(), 5);
    assert!(source.points().is_none());
    for (i, g) in geometries.iter().enumerate() {
        assert_eq!(source.geometry(i).as_ref(), Some(g), "row {i}");
    }
    assert_eq!(
        source.report().geometry_types,
        vec!["LineString", "MultiLineString", "Polygon", "MultiPolygon", "MultiPoint"]
    );
}

#[test]
fn wkb_reads_big_endian_ewkb_and_z_and_skips_collections() {
    // Big-endian ISO Point Z (1001).
    let mut be = vec![0u8];
    be.extend_from_slice(&1001u32.to_be_bytes());
    for v in [5.0f64, 6.0, 7.0] {
        be.extend_from_slice(&v.to_be_bytes());
    }
    assert_eq!(decode::wkb(&be), Ok(Decoded::Point([5.0, 6.0])));
    // EWKB LineString with SRID 4326.
    let mut ewkb = vec![1u8];
    ewkb.extend_from_slice(&(2u32 | 0x2000_0000).to_le_bytes());
    ewkb.extend_from_slice(&4326u32.to_le_bytes());
    ewkb.extend_from_slice(&2u32.to_le_bytes());
    for v in [1.0f64, 2.0, 3.0, 4.0] {
        ewkb.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(decode::wkb(&ewkb), Ok(Decoded::Shape(OverlayGeometry::Line(vec![vec![(1.0, 2.0), (3.0, 4.0)]]))));
    assert_eq!(decode::wkb(&wkb(&OverlayGeometry::Empty)), Err(GeoParquetSkip::UnsupportedGeometry));
    assert_eq!(decode::wkb(&[1, 2, 0]), Err(GeoParquetSkip::Malformed));
    // A count past the bytes left is malformed, not an allocation.
    let mut huge = vec![1u8];
    huge.extend_from_slice(&2u32.to_le_bytes());
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decode::wkb(&huge), Err(GeoParquetSkip::Malformed));
    let out = wkb(&OverlayGeometry::Point(vec![(200.0, 0.0)]));
    assert_eq!(decode::wkb(&out), Err(GeoParquetSkip::OutOfRange));
}

#[test]
fn bad_rows_are_skipped_and_counted() {
    let features = vec![
        point(1.0, 1.0, 0),
        WriteFeature { geometry: OverlayGeometry::Empty, properties: vec![WriteValue::Int(Some(1))] },
        point(500.0, 1.0, 2),
        point(2.0, 2.0, 3),
    ];
    let bytes = write(&ID, &features, &WriteOptions { covering: false, ..WriteOptions::default() }).unwrap();
    let source = read(&bytes, &GeoParquetOptions::default());
    assert_eq!(numbers(&source, "id"), vec![Some(0.0), Some(3.0)]);
    let r = source.report();
    assert_eq!(r.rows_read, 4);
    assert_eq!(r.rows_loaded, 2);
    assert_eq!(r.skipped_by_reason.len(), 2);
    assert!(r.to_string().contains("skipped 2"), "{r}");
}

#[test]
fn native_points_and_lines_read_and_push_down_by_their_xy_statistics() {
    let bytes = banded(&WriteOptions { encoding: GeoParquetEncoding::Point, covering: false, ..WriteOptions::default() });
    let all = read(&bytes, &GeoParquetOptions::default());
    assert_eq!(all.len(), 1000);
    assert_eq!(all.report().encoding, GeoParquetEncoding::Point);
    assert_eq!(all.report().pushdown, GeoParquetPushdown::GeometryStats);
    let some = read(&bytes, &GeoParquetOptions::default().with_bbox([30.0, -90.0, 49.9, 90.0]));
    assert_eq!((some.report().row_groups_read, some.report().row_groups_skipped), (2, 8));
    assert_eq!(some.len(), 200);

    let lines: Vec<WriteFeature> = (0..10)
        .map(|i| WriteFeature {
            geometry: OverlayGeometry::Line(vec![vec![(f64::from(i), 0.0), (f64::from(i), 5.0), (f64::from(i) + 1.0, 5.0)]]),
            properties: vec![],
        })
        .collect();
    let bytes =
        write(&[], &lines, &WriteOptions { encoding: GeoParquetEncoding::LineString, covering: false, ..WriteOptions::default() })
            .unwrap();
    let source = read(&bytes, &GeoParquetOptions::default());
    assert_eq!(source.len(), 10);
    assert_eq!(source.geometry(3), Some(lines[3].geometry.clone()));
}

#[test]
fn row_groups_outside_the_box_are_skipped_exactly() {
    let bytes = banded(&WriteOptions::default());
    let all = read(&bytes, &GeoParquetOptions::default());
    assert_eq!(all.report().row_groups_total, 10);
    assert_eq!(all.report().pushdown, GeoParquetPushdown::Covering);
    // Longitudes 25..44: groups 2, 3 and 4 (20..29.9, 30..39.9, 40..49.9).
    let b = [25.0, -90.0, 44.0, 90.0];
    let some = read(&bytes, &GeoParquetOptions::default().with_bbox(b));
    let r = some.report();
    assert_eq!((r.row_groups_read, r.row_groups_skipped), (3, 7));
    assert_eq!(r.rows_read, 300);
    let inside = (0..1000).filter(|&i| {
        let x = f64::from(i / 100) * 10.0 + f64::from(i % 100) * 0.099;
        (b[0]..=b[2]).contains(&x)
    });
    assert_eq!(some.len(), inside.clone().count());
    assert_eq!(r.rows_filtered, 300 - some.len() as u64);
    let ids: Vec<u64> = (0..some.len()).map(|i| some.file_row(i).unwrap()).collect();
    assert_eq!(ids, inside.map(|i| i as u64).collect::<Vec<_>>());
    // Fewer bytes than the whole read: only three groups' chunks.
    assert!(r.bytes_read * 2 < all.report().bytes_read, "{} vs {}", r.bytes_read, all.report().bytes_read);
    // A box no group reaches reads none.
    let none = read(&bytes, &GeoParquetOptions::default().with_bbox([120.0, -90.0, 130.0, 90.0]));
    assert_eq!((none.report().row_groups_read, none.len()), (0, 0));
}

#[test]
fn without_statistics_every_group_is_read_and_filtered_with_a_warning() {
    let bytes = banded(&WriteOptions { statistics: false, ..WriteOptions::default() });
    let some = read(&bytes, &GeoParquetOptions::default().with_bbox([25.0, -90.0, 44.0, 90.0]));
    let r = some.report();
    assert_eq!(r.pushdown, GeoParquetPushdown::None);
    assert_eq!((r.row_groups_read, r.row_groups_skipped), (10, 0));
    assert_eq!(some.len(), read(&banded(&WriteOptions::default()), &GeoParquetOptions::default().with_bbox([25.0, -90.0, 44.0, 90.0])).len());
    assert!(r.warnings.iter().any(|w| w.contains("no row-group statistics")), "{r}");
}

/// Random points and polygons in random order (no useful row-group
/// pruning): the bbox read must equal a brute-force filter of the whole.
#[test]
fn the_bbox_filter_matches_brute_force() {
    let mut r = Rng(7);
    let features: Vec<WriteFeature> = (0..3000)
        .map(|i| {
            let (x, y) = (-170.0 + r.next() * 340.0, -80.0 + r.next() * 160.0);
            let geometry = if i % 3 == 0 {
                let (w, h) = (r.next() * 5.0, r.next() * 5.0);
                OverlayGeometry::Polygon(vec![vec![vec![(x, y), (x + w, y), (x + w, y + h), (x, y)]]])
            } else {
                OverlayGeometry::Point(vec![(x, y)])
            };
            WriteFeature { geometry, properties: vec![WriteValue::Int(Some(i))] }
        })
        .collect();
    for covering in [true, false] {
        let bytes = write(&ID, &features, &WriteOptions { row_group_rows: 256, covering, ..WriteOptions::default() }).unwrap();
        for b in [[-10.0, -10.0, 30.0, 25.0], [100.0, 40.0, 101.0, 41.0], [-180.0, -90.0, 180.0, 90.0]] {
            let got = read(&bytes, &GeoParquetOptions::default().with_bbox(b));
            let mut ids: Vec<i64> = numbers(&got, "id").iter().map(|v| v.unwrap() as i64).collect();
            ids.sort_unstable();
            let want: Vec<i64> = features
                .iter()
                .enumerate()
                .filter(|(_, f)| intersects(&crate::overlay::fgb::bbox_of(&f.geometry), &b))
                .map(|(i, _)| i as i64)
                .collect();
            assert_eq!(ids, want, "covering {covering}, box {b:?}");
            assert_eq!(got.report().rows_read, got.report().rows_filtered + got.len() as u64);
        }
    }
}

#[test]
fn only_the_named_columns_are_read() {
    let columns = [("a", WriteType::Number), ("b", WriteType::Str)];
    let features: Vec<WriteFeature> = (0..2000)
        .map(|i| WriteFeature {
            geometry: OverlayGeometry::Point(vec![(0.0, 0.0)]),
            properties: vec![WriteValue::Number(Some(f64::from(i))), WriteValue::Str(Some(format!("a fairly long string {i}")))],
        })
        .collect();
    let bytes = write(&columns, &features, &WriteOptions { compression: WriteCompression::Uncompressed, ..WriteOptions::default() }).unwrap();
    let all = read(&bytes, &GeoParquetOptions::default());
    let a = read(&bytes, &GeoParquetOptions::default().with_columns(["a", "missing"]));
    assert_eq!(a.columns().len(), 1);
    assert_eq!(a.columns()[0].0, "a");
    assert!(a.report().bytes_read < all.report().bytes_read / 2, "{} vs {}", a.report().bytes_read, all.report().bytes_read);
    assert_eq!(a.report().dropped_columns, vec!["missing".to_string()]);
    assert!(a.report().warnings.iter().any(|w| w.contains("missing")));
}

#[test]
fn versions_crs_and_non_geoparquet_files_are_refused_by_name() {
    let features = vec![point(1.0, 2.0, 0)];
    let with = |o: WriteOptions| write(&ID, &features, &o).unwrap();
    let opts = GeoParquetOptions::default();
    for v in ["1.0.0", "1.1.0", "1.0.0-beta.1"] {
        assert!(GeoParquetSource::from_bytes(&with(WriteOptions { version: v.into(), ..WriteOptions::default() }), &opts).is_ok(), "{v}");
    }
    for v in ["2.0.0", "0.4.0"] {
        let e = GeoParquetSource::from_bytes(&with(WriteOptions { version: v.into(), ..WriteOptions::default() }), &opts).unwrap_err();
        assert!(matches!(&e, GeoParquetError::Unsupported(m) if m.contains(v)), "{e}");
    }
    let mercator = serde_json::json!({"type": "ProjectedCRS", "name": "WGS 84 / Pseudo-Mercator", "id": {"authority": "EPSG", "code": 3857}});
    let file = with(WriteOptions { crs: Some(mercator), ..WriteOptions::default() });
    let e = GeoParquetSource::from_bytes(&file, &opts).unwrap_err();
    assert_eq!(e, GeoParquetError::Crs("EPSG:3857 (WGS 84 / Pseudo-Mercator)".into()));
    let anyway = GeoParquetSource::from_bytes(&file, &GeoParquetOptions { accept_any_crs: true, ..opts.clone() }).unwrap();
    assert!(anyway.report().warnings[0].contains("accept_any_crs"));
    for crs in [
        serde_json::json!({"id": {"authority": "OGC", "code": "CRS84"}}),
        serde_json::json!({"id": {"authority": "EPSG", "code": 4326}}),
        serde_json::json!("OGC:CRS84"),
    ] {
        assert!(GeoParquetSource::from_bytes(&with(WriteOptions { crs: Some(crs.clone()), ..WriteOptions::default() }), &opts).is_ok(), "{crs}");
    }
    let unknown = GeoParquetSource::from_bytes(&with(WriteOptions { crs: Some(serde_json::Value::Null), ..WriteOptions::default() }), &opts).unwrap();
    assert!(unknown.report().warnings[0].contains("unknown"));

    assert_eq!(GeoParquetSource::from_bytes(b"not a parquet file", &opts).unwrap_err(), GeoParquetError::NotParquet);
    // A plain Parquet file: no `geo` key.
    let schema = Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new("x", arrow_schema::DataType::Int32, false)]));
    let mut plain = Vec::new();
    let mut w = parquet::arrow::ArrowWriter::try_new(&mut plain, Arc::clone(&schema), None).unwrap();
    w.write(&RecordBatch::try_new(schema, vec![Arc::new(arrow_array::Int32Array::from(vec![1]))]).unwrap()).unwrap();
    w.close().unwrap();
    assert!(matches!(GeoParquetSource::from_bytes(&plain, &opts), Err(GeoParquetError::NotGeoParquet(_))));
}

#[test]
fn zstd_and_gzip_files_read_natively() {
    for compression in [WriteCompression::Zstd, WriteCompression::Gzip, WriteCompression::Uncompressed] {
        let bytes = banded(&WriteOptions { compression, ..WriteOptions::default() });
        assert_eq!(read(&bytes, &GeoParquetOptions::default()).len(), 1000, "{compression:?}");
    }
}

#[test]
fn a_file_over_max_bytes_is_refused_unread() {
    let bytes = banded(&WriteOptions::default());
    let e = GeoParquetSource::from_bytes(&bytes, &GeoParquetOptions { max_bytes: 100, ..GeoParquetOptions::default() }).unwrap_err();
    assert!(matches!(e, GeoParquetError::TooLarge { limit: 100, .. }));
}

/// The GeoParquet spec's own 1.1.0 example (`examples/example.parquet`,
/// written by pyarrow/GeoPandas; Natural Earth countries): a PROJJSON
/// OGC:CRS84, a `bbox` covering, WKB polygons and multipolygons, pandas
/// metadata alongside.
#[test]
fn the_spec_example_file_reads() {
    let bytes = std::fs::read(format!("{}/tests/fixtures/geoparquet/naturalearth_countries.parquet", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let source = read(&bytes, &GeoParquetOptions::default());
    let r = source.report();
    assert_eq!(r.version, "1.1.0");
    assert_eq!(r.rows_total, 5);
    assert_eq!(source.len(), 5);
    assert_eq!(r.pushdown, GeoParquetPushdown::Covering);
    assert!(r.crs.starts_with("OGC:CRS84"), "{}", r.crs);
    assert!(r.skipped_by_reason.is_empty());
    let Some(CsvValues::Str(names)) = source.column("name") else { panic!("{:?}", r.columns) };
    assert_eq!(names.get(3), Some("Canada"));
    assert!(numbers(&source, "pop_est")[4].unwrap() > 3e8);
    assert!(matches!(source.geometry(0), Some(OverlayGeometry::Polygon(_))));
    // Canada's box only.
    let canada = read(&bytes, &GeoParquetOptions::default().with_bbox([-100.0, 75.0, -90.0, 80.0]));
    let Some(CsvValues::Str(names)) = canada.column("name") else { panic!() };
    assert_eq!((canada.len(), names.get(0)), (1, Some("Canada")));
}

/// The Studio's bundled sample is this writer's output for the cities CSV,
/// so it can be rebuilt (`GEOPARQUET_BLESS=1`) rather than trusted.
#[test]
fn the_bundled_cities_file_is_the_cities_csv_written() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let csv = std::fs::read(format!("{dir}/tests/fixtures/world_cities.csv")).unwrap();
    let csv = crate::overlay::csv::CsvSource::from_bytes(&csv, &Default::default()).unwrap();
    let text = |name: &str, i: usize| match csv.column(name) {
        Some(CsvValues::Str(s)) => WriteValue::Str(s.get(i).map(str::to_string)),
        _ => WriteValue::Str(None),
    };
    let features: Vec<WriteFeature> = (0..csv.len())
        .map(|i| {
            let [lon, lat] = csv.points().unwrap()[i];
            let population = match csv.column("population") {
                Some(CsvValues::Number(v)) => WriteValue::Int(v[i].map(|p| p as i64)),
                _ => WriteValue::Int(None),
            };
            let capital = match csv.column("capital") {
                Some(CsvValues::Bool(v)) => WriteValue::Bool(v[i]),
                _ => WriteValue::Bool(None),
            };
            WriteFeature { geometry: OverlayGeometry::Point(vec![(lon, lat)]), properties: vec![text("name", i), text("country", i), population, capital] }
        })
        .collect();
    let columns = [("name", WriteType::Str), ("country", WriteType::Str), ("population", WriteType::Int), ("capital", WriteType::Bool)];
    let bytes = write(&columns, &features, &WriteOptions { row_group_rows: 16, ..WriteOptions::default() }).unwrap();
    let path = format!("{dir}/tests/fixtures/geoparquet/world_cities.parquet");
    if std::env::var_os("GEOPARQUET_BLESS").is_some() {
        std::fs::write(&path, &bytes).unwrap();
    }
    assert_eq!(std::fs::read(&path).expect("the bundled file (GEOPARQUET_BLESS=1 writes it)"), bytes);
    let source = read(&bytes, &GeoParquetOptions::default());
    assert_eq!(source.len(), 46);
    assert_eq!(source.report().row_groups_total, 3);
}

#[test]
fn a_streamed_merge_dedupes_and_evicts_to_the_cap() {
    let bytes = banded(&WriteOptions::default());
    let first = read(&bytes, &GeoParquetOptions::default().with_bbox([0.0, -90.0, 19.9, 90.0]));
    assert_eq!(first.len(), 200);
    let mut held = first.clone();
    // A neighbouring view, merged under a cap of 250: the rows outside it
    // go first, then the rest is thinned.
    let view = [20.0, -90.0, 39.9, 90.0];
    let more = read(&bytes, &GeoParquetOptions::default().with_bbox(view));
    let dropped = held.merge_stream(more, view, 250);
    assert_eq!(held.len(), 200);
    assert_eq!(dropped.len(), 200);
    assert!((0..held.len()).all(|i| held.file_row(i).unwrap() >= 200));
}

/// A streamed source under a long pan: a 20-degree view slides across the
/// banded file and back, three times, merged under a cap of 150. The
/// resident set never passes the cap.
#[test]
fn a_stream_stays_bounded_over_a_long_pan() {
    let bytes = banded(&WriteOptions::default());
    let mut held = read(&bytes, &GeoParquetOptions::default().with_bbox([0.0, -90.0, 9.9, 90.0]));
    let mut merged = 0;
    for step in 0..3 * 32 {
        let k = step % 32;
        let west = f64::from(if k < 16 { k } else { 31 - k }) * 5.0;
        let view = [west, -90.0, west + 19.9, 90.0];
        let more = read(&bytes, &GeoParquetOptions::default().with_bbox(view));
        merged += more.len();
        held.merge_stream(more, view, 150);
        assert!(held.len() <= 150, "step {step}: {} resident", held.len());
    }
    assert!(merged > 20 * 150, "the pan must read far more than the cap: {merged}");
}

#[test]
fn the_store_takes_points_and_polygons() {
    let mut store = crate::overlay::OverlayStore::default();
    let points = read(&banded(&WriteOptions::default()), &GeoParquetOptions::default());
    store.set_source_geoparquet("p", &points, &FeaturePaint::default(), 3).unwrap();
    let bytes = std::fs::read(format!("{}/tests/fixtures/geoparquet/naturalearth_countries.parquet", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let countries = read(&bytes, &GeoParquetOptions::default());
    store.set_source_geoparquet("c", &countries, &FeaturePaint::default(), 3).unwrap();
}

#[test]
fn a_plain_parquet_reads_as_columns_and_the_footer_names_its_times() {
    use arrow_array::{Float64Array, StringArray, TimestampMillisecondArray};
    use arrow_schema::{DataType, Field, Schema, TimeUnit};
    let schema = Arc::new(Schema::new(vec![
        Field::new("lat", DataType::Float64, true),
        Field::new("lon", DataType::Float64, true),
        Field::new("name", DataType::Utf8, true),
        Field::new("seen", DataType::Timestamp(TimeUnit::Millisecond, None), true),
        Field::new("blob", DataType::Binary, true),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Float64Array::from(vec![Some(1.30), Some(1.35), None])),
            Arc::new(Float64Array::from(vec![Some(103.8), Some(103.9), Some(104.0)])),
            Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])),
            Arc::new(TimestampMillisecondArray::from(vec![Some(1_700_000_000_000), None, Some(1_700_000_060_000)])),
            Arc::new(arrow_array::BinaryArray::from(vec![Some(&b"x"[..]), None, None])),
        ],
    )
    .unwrap();
    let mut bytes = Vec::new();
    let mut w = parquet::arrow::ArrowWriter::try_new(&mut bytes, schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let p = plain::peek(&bytes).unwrap();
    assert_eq!((p.rows, p.geo), (3, false));
    assert_eq!(p.temporal, vec![("seen".to_string(), false)]);
    let t = plain::read_plain(&bytes).unwrap();
    assert_eq!(t.rows, 3);
    let names: Vec<&str> = t.columns.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["lat", "lon", "name", "seen"], "binary is left out");
    assert_eq!(t.columns[0].1, CsvValues::Number(vec![Some(1.30), Some(1.35), None]));
    assert_eq!(t.columns[3].1, CsvValues::Number(vec![Some(1.7e12), None, Some(1_700_000_060_000.0)]));
    match &t.columns[2].1 {
        CsvValues::Str(s) => assert_eq!((s.get(0), s.get(1), s.get(2)), (Some("a"), None, Some("c"))),
        other => panic!("{other:?}"),
    }
    // A GeoParquet's footer says so.
    let geo = write(&ID, &[point(1.0, 2.0, 0)], &WriteOptions::default()).unwrap();
    assert!(plain::peek(&geo).unwrap().geo);
    assert_eq!(plain::peek(b"not parquet at all").unwrap_err(), GeoParquetError::NotParquet);
}

/// `write_streamed` writes the same file as `write` from rows built on
/// demand, and with `max_bytes` stops at the first row group past it
/// instead of writing the rest.
#[test]
fn a_streamed_write_is_the_same_file_and_stops_at_its_byte_limit() {
    let feature = |i: usize| point(f64::from((i / 100) as u32) * 10.0 + f64::from((i % 100) as u32) * 0.099, f64::from((i % 50) as u32) - 25.0, i as i64);
    let options = WriteOptions { row_group_rows: 100, ..WriteOptions::default() };
    let whole: Vec<WriteFeature> = (0..1000).map(feature).collect();
    let streamed = write_streamed(&ID, 1000, &|i| feature(i).geometry, &feature, &options, None).unwrap();
    assert_eq!(streamed, write(&ID, &whole, &options).unwrap(), "byte for byte");
    let limit = streamed.len() as u64 / 3;
    match write_streamed(&ID, 1000, &|i| feature(i).geometry, &feature, &options, Some(limit)) {
        Err(StreamedWriteError::OverLimit { rows_written }) => assert!(rows_written < 1000 && rows_written % 100 == 0, "stopped part-way, at a group: {rows_written}"),
        other => panic!("{other:?}"),
    }
    let empty = write_streamed(&ID, 0, &|i| feature(i).geometry, &feature, &options, None).unwrap();
    assert_eq!(empty, write(&ID, &[], &options).unwrap());
}
