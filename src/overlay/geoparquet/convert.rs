//! One-off conversions INTO GeoParquet, for data whose raw form this crate
//! reads slowly or not at all:
//!
//! - [`arrow_ipc_to_geoparquet`]: an Arrow IPC file (Feather v2) with a
//!   GeoArrow-native geometry column, as Kepler.gl and GeoPandas write it.
//!   The record batches are copied as they are (no geometry decode), the
//!   GeoArrow encoding is named in GeoParquet's `geo` metadata, and the
//!   result reads through [`super::GeoParquetSource`] like any other file.
//! - [`geojson_to_geoparquet`]: a GeoJSON `FeatureCollection` to WKB
//!   GeoParquet with typed columns, so a 40 MB document is parsed once
//!   rather than on every open, and loads off the UI thread
//!   (`MapView::load_source_geoparquet`).
//!
//! Native only: the browser has no use for a conversion step.

use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_schema::{DataType, Schema};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use serde_json::{json, Value};

use super::write::{WriteCompression, WriteFeature, WriteOptions, WriteType, WriteValue};

/// What a conversion produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Converted {
    pub bytes: Vec<u8>,
    pub rows: usize,
}

/// The GeoParquet encoding name for a GeoArrow extension name
/// (`geoarrow.multipolygon` -> `multipolygon`), or `None` for one
/// GeoParquet has no native encoding for.
fn geoparquet_encoding(extension: &str) -> Option<&'static str> {
    Some(match extension.strip_prefix("geoarrow.").unwrap_or(extension).to_ascii_lowercase().as_str() {
        "point" => "point",
        "linestring" => "linestring",
        "polygon" => "polygon",
        "multipoint" => "multipoint",
        "multilinestring" => "multilinestring",
        "multipolygon" => "multipolygon",
        "wkb" => "WKB",
        _ => return None,
    })
}

/// Converts an Arrow IPC FILE (`ARROW1` magic) whose geometry column is
/// GeoArrow (named by the schema's `geo` metadata, or by an
/// `ARROW:extension:name` of `geoarrow.*` on a field) into a GeoParquet
/// file: zstd pages, `row_group_rows` rows per group.
///
/// `keep`: the property columns to carry (the geometry column always
/// comes); `None` keeps every column. A named column the file does not
/// have is an error, so a typo is not a silently empty legend.
///
/// The CRS is written as absent (OGC:CRS84): Kepler's files state EPSG:4326
/// as WKT, which GeoParquet does not take, and GeoArrow stores x =
/// longitude whatever the CRS's axis order says.
pub fn arrow_ipc_to_geoparquet(ipc: &[u8], keep: Option<&[&str]>, row_group_rows: usize) -> Result<Converted, String> {
    let reader = arrow_ipc::reader::FileReader::try_new(std::io::Cursor::new(ipc), None)
        .map_err(|e| format!("not an Arrow IPC file: {e}"))?;
    let schema = reader.schema();
    let (geometry, encoding) = find_geometry(&schema)?;
    let mut indices = vec![schema.index_of(&geometry).map_err(|e| e.to_string())?];
    match keep {
        Some(names) => {
            for name in names {
                let at = schema.index_of(name).map_err(|_| format!("the file has no column `{name}`"))?;
                if !indices.contains(&at) {
                    indices.push(at);
                }
            }
        }
        None => {
            let first = indices[0];
            indices.extend((0..schema.fields().len()).filter(|i| *i != first));
        }
    }
    let projected = Arc::new(schema.project(&indices).map_err(|e| e.to_string())?);
    let mut batches: Vec<RecordBatch> = Vec::new();
    let mut rows = 0usize;
    let mut extent = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for batch in reader {
        let batch = batch.map_err(|e| format!("reading a record batch: {e}"))?;
        let batch = batch.project(&indices).map_err(|e| e.to_string())?;
        rows += batch.num_rows();
        grow_extent(&mut extent, batch.column(0).as_ref());
        batches.push(batch);
    }
    let mut column = json!({ "encoding": encoding, "geometry_types": [] });
    if extent[0] <= extent[2] {
        column["bbox"] = json!(extent);
    }
    let geo = json!({ "version": "1.1.0", "primary_column": geometry, "columns": { geometry.clone(): column } });
    // The IPC schema's own metadata (its `geo` with a WKT CRS) is replaced,
    // not merged: the reader takes the key-value `geo` entry.
    let schema = Arc::new(Schema::new(projected.fields().clone()));
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_max_row_group_row_count(Some(row_group_rows.max(1)))
        .set_key_value_metadata(Some(vec![KeyValue::new("geo".into(), geo.to_string())]))
        .build();
    let mut out = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut out, Arc::clone(&schema), Some(props)).map_err(|e| e.to_string())?;
    for batch in batches {
        let batch = RecordBatch::try_new(Arc::clone(&schema), batch.columns().to_vec()).map_err(|e| e.to_string())?;
        writer.write(&batch).map_err(|e| e.to_string())?;
    }
    writer.close().map_err(|e| e.to_string())?;
    Ok(Converted { bytes: out, rows })
}

/// Converts any Arrow IPC FILE (Feather v2) into Parquet that
/// [`super::plain::peek`] and the readers take: GeoParquet through
/// [`arrow_ipc_to_geoparquet`] when a column is GeoArrow, else a plain
/// Parquet file of every column as it is (a table whose rows carry a
/// coordinate pair, or none), zstd pages, `row_group_rows` rows per group.
/// Column types are kept, so a timestamp stays a timestamp.
pub fn arrow_ipc_to_parquet(ipc: &[u8], row_group_rows: usize) -> Result<Converted, String> {
    let reader = arrow_ipc::reader::FileReader::try_new(std::io::Cursor::new(ipc), None)
        .map_err(|e| format!("not an Arrow IPC file: {e}"))?;
    if find_geometry(&reader.schema()).is_ok() {
        return arrow_ipc_to_geoparquet(ipc, None, row_group_rows);
    }
    // No `geo` key carried over: a plain table.
    let schema = Arc::new(Schema::new(reader.schema().fields().clone()));
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_max_row_group_row_count(Some(row_group_rows.max(1)))
        .build();
    let mut out = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut out, Arc::clone(&schema), Some(props)).map_err(|e| e.to_string())?;
    let mut rows = 0usize;
    for batch in reader {
        let batch = batch.map_err(|e| format!("reading a record batch: {e}"))?;
        rows += batch.num_rows();
        let batch = RecordBatch::try_new(Arc::clone(&schema), batch.columns().to_vec()).map_err(|e| e.to_string())?;
        writer.write(&batch).map_err(|e| e.to_string())?;
    }
    writer.close().map_err(|e| e.to_string())?;
    Ok(Converted { bytes: out, rows })
}

/// The geometry column's name and GeoParquet encoding.
fn find_geometry(schema: &Schema) -> Result<(String, &'static str), String> {
    if let Some(geo) = schema.metadata().get("geo").and_then(|g| serde_json::from_str::<Value>(g).ok()) {
        if let Some(primary) = geo["primary_column"].as_str() {
            let encoding = geo["columns"][primary]["encoding"].as_str().unwrap_or_default();
            if let Some(e) = geoparquet_encoding(encoding) {
                return Ok((primary.to_string(), e));
            }
        }
    }
    for field in schema.fields() {
        if let Some(e) = field.metadata().get("ARROW:extension:name").and_then(|n| geoparquet_encoding(n)) {
            return Ok((field.name().clone(), e));
        }
    }
    Err("no GeoArrow geometry column (no `geo` metadata, no geoarrow.* extension field)".into())
}

/// Widens `extent` by every coordinate of a GeoArrow-native column: the
/// innermost struct (`x`, `y`) or fixed-size list (interleaved), however
/// deep the list nesting. WKB columns leave it as it is.
fn grow_extent(extent: &mut [f64; 4], array: &dyn arrow_array::Array) {
    use arrow_array::cast::AsArray as _;
    use arrow_array::types::Float64Type;
    match array.data_type() {
        DataType::List(_) => grow_extent(extent, array.as_list::<i32>().values().as_ref()),
        DataType::LargeList(_) => grow_extent(extent, array.as_list::<i64>().values().as_ref()),
        DataType::Struct(_) => {
            let s = array.as_struct();
            let (Some(x), Some(y)) = (s.column_by_name("x"), s.column_by_name("y")) else { return };
            let (Some(x), Some(y)) = (x.as_primitive_opt::<Float64Type>(), y.as_primitive_opt::<Float64Type>()) else { return };
            for (x, y) in x.values().iter().zip(y.values().iter()) {
                widen(extent, *x, *y);
            }
        }
        DataType::FixedSizeList(_, n) if *n >= 2 => {
            let list = array.as_fixed_size_list();
            let Some(values) = list.values().as_primitive_opt::<Float64Type>() else { return };
            for xy in values.values().chunks(*n as usize) {
                widen(extent, xy[0], xy[1]);
            }
        }
        _ => {}
    }
}

fn widen(extent: &mut [f64; 4], x: f64, y: f64) {
    if x.is_finite() && y.is_finite() {
        *extent = [extent[0].min(x), extent[1].min(y), extent[2].max(x), extent[3].max(y)];
    }
}

/// Converts a GeoJSON `FeatureCollection` into a WKB GeoParquet file
/// (zstd, a bbox covering), carrying the properties named in `columns`
/// with the given types. A value of the wrong JSON type, or a missing
/// one, is a null. Features whose geometry this crate cannot draw are
/// refused by the parse, as `MapView::set_source` refuses them.
pub fn geojson_to_geoparquet(geojson: &str, columns: &[(&str, WriteType)], row_group_rows: usize) -> Result<Converted, String> {
    let parsed = crate::overlay::geojson::parse_feature_collection(geojson).map_err(|e| format!("{e:?}"))?;
    let features: Vec<WriteFeature> = parsed
        .into_iter()
        .map(|f| WriteFeature {
            properties: columns.iter().map(|(name, kind)| write_value(f.properties.get(*name), *kind)).collect(),
            geometry: f.geometry,
        })
        .collect();
    let rows = features.len();
    let options = WriteOptions { row_group_rows: row_group_rows.max(1), compression: WriteCompression::Zstd, ..WriteOptions::default() };
    let bytes = super::write::write(columns, &features, &options)?;
    Ok(Converted { bytes, rows })
}

fn write_value(value: Option<&Value>, kind: WriteType) -> WriteValue {
    match kind {
        WriteType::Number => WriteValue::Number(value.and_then(Value::as_f64).filter(|v| v.is_finite())),
        WriteType::Int => WriteValue::Int(value.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))),
        WriteType::Bool => WriteValue::Bool(value.and_then(Value::as_bool)),
        WriteType::Str => WriteValue::Str(value.and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::geoparquet::{GeoParquetOptions, GeoParquetSource};
    use arrow_array::builder::{Float64Builder, ListBuilder, StructBuilder};
    use arrow_array::{ArrayRef, Int64Array};
    use arrow_schema::{Field, Fields};

    /// A two-row Arrow IPC file with a GeoArrow `polygon` column (one
    /// square each) and an Int64 column, as GeoPandas' `to_feather`
    /// writes it: the encoding in the schema's `geo` metadata.
    fn ipc_fixture() -> Vec<u8> {
        let xy = Fields::from(vec![Field::new("x", DataType::Float64, false), Field::new("y", DataType::Float64, false)]);
        let mut polys = ListBuilder::new(ListBuilder::new(StructBuilder::new(
            xy.clone(),
            vec![Box::new(Float64Builder::new()), Box::new(Float64Builder::new())],
        )));
        for (x0, y0) in [(-74.0, 40.7), (-73.9, 40.8)] {
            let ring = polys.values();
            for (x, y) in [(x0, y0), (x0 + 0.01, y0), (x0 + 0.01, y0 + 0.01), (x0, y0)] {
                let point = ring.values();
                point.field_builder::<Float64Builder>(0).unwrap().append_value(x);
                point.field_builder::<Float64Builder>(1).unwrap().append_value(y);
                point.append(true);
            }
            ring.append(true);
            polys.append(true);
        }
        let geometry: ArrayRef = Arc::new(polys.finish());
        let jobs: ArrayRef = Arc::new(Int64Array::from(vec![12, 40]));
        let other: ArrayRef = Arc::new(Int64Array::from(vec![1, 2]));
        let gisjoin: ArrayRef = Arc::new(arrow_array::StringArray::from(vec!["G36005100", "G36005200"]));
        let geo = json!({"primary_column": "geometry", "columns": {"geometry": {"encoding": "geoarrow.polygon", "crs": "GEOGCRS[...]"}}});
        let schema = Arc::new(
            Schema::new(vec![
                Field::new("geometry", geometry.data_type().clone(), true),
                Field::new("C000_02", DataType::Int64, true),
                Field::new("other", DataType::Int64, true),
                Field::new("GISJOIN", DataType::Utf8, true),
            ])
            .with_metadata(std::collections::HashMap::from([("geo".to_string(), geo.to_string())])),
        );
        let batch = RecordBatch::try_new(Arc::clone(&schema), vec![geometry, jobs, other, gisjoin]).unwrap();
        let mut out = Vec::new();
        let mut w = arrow_ipc::writer::FileWriter::try_new(&mut out, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
        drop(w);
        out
    }

    #[test]
    fn an_arrow_ipc_geoarrow_file_reads_back_as_geoparquet() {
        let converted = arrow_ipc_to_geoparquet(&ipc_fixture(), Some(&["C000_02"]), 1024).expect("converts");
        assert_eq!(converted.rows, 2);
        let gpq = GeoParquetSource::from_bytes(&converted.bytes, &GeoParquetOptions::default()).expect("reads");
        assert_eq!(gpq.len(), 2);
        assert!(gpq.column("C000_02").is_some(), "kept");
        assert!(gpq.column("other").is_none(), "projected away");
        assert_eq!(gpq.metadata().encoding, crate::overlay::geoparquet::GeoParquetEncoding::Polygon);
        assert!(matches!(gpq.geometry(0), Some(crate::overlay::geojson::OverlayGeometry::Polygon(_))));
    }

    #[test]
    fn any_arrow_ipc_file_converts_geoarrow_or_plain() {
        // GeoArrow: GeoParquet, every column kept.
        let geo = arrow_ipc_to_parquet(&ipc_fixture(), 1024).expect("converts");
        let gpq = GeoParquetSource::from_bytes(&geo.bytes, &GeoParquetOptions::default()).expect("reads");
        assert!(gpq.column("C000_02").is_some() && gpq.column("other").is_some(), "every column");
        // A plain table of lat/lon rows: plain Parquet, read by `plain`.
        let schema = Arc::new(Schema::new(vec![Field::new("lat", DataType::Float64, true), Field::new("lon", DataType::Float64, true), Field::new("n", DataType::Int64, true)]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(arrow_array::Float64Array::from(vec![1.3, 1.35])),
            Arc::new(arrow_array::Float64Array::from(vec![103.8, 103.9])),
            Arc::new(Int64Array::from(vec![5, 7])),
        ];
        let batch = RecordBatch::try_new(Arc::clone(&schema), cols).unwrap();
        let mut ipc = Vec::new();
        let mut w = arrow_ipc::writer::FileWriter::try_new(&mut ipc, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
        drop(w);
        let plain = arrow_ipc_to_parquet(&ipc, 1024).expect("converts");
        assert_eq!(plain.rows, 2);
        let peek = crate::overlay::geoparquet::plain::peek(&plain.bytes).expect("a Parquet file");
        assert!(!peek.geo, "no geometry column: plain Parquet");
        assert_eq!(crate::overlay::geoparquet::plain::read_plain(&plain.bytes).expect("reads").rows, 2);
        assert!(arrow_ipc_to_parquet(b"not arrow", 1024).is_err());
    }

    /// Writes the Explorer's Arrow fixtures (`tests/fixtures/arrow/`): the
    /// two GeoArrow squares above, and `tests/fixtures/sg_od_synthetic.csv`
    /// as a plain Arrow table (two coordinate pairs, the departure a
    /// millisecond timestamp in UTC). Run once with
    /// `cargo test --lib write_arrow_fixtures -- --ignored`.
    #[test]
    #[ignore = "writes tests/fixtures/arrow/"]
    fn write_arrow_fixtures() {
        use arrow_array::{Float64Array, StringArray, TimestampMillisecondArray};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/arrow");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("nyc_blocks_geoarrow.arrow"), ipc_fixture()).unwrap();
        let csv = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sg_od_synthetic.csv")).unwrap();
        let rows: Vec<Vec<&str>> = csv.lines().skip(1).map(|l| l.split(',').collect()).collect();
        let num = |i: usize| -> ArrayRef { Arc::new(Float64Array::from(rows.iter().map(|r| r[i].parse::<f64>().unwrap()).collect::<Vec<_>>())) };
        let text = |i: usize| -> ArrayRef { Arc::new(StringArray::from(rows.iter().map(|r| r[i]).collect::<Vec<_>>())) };
        // `2026-10-06T07:33:00+08:00`: 2026-10-06 is day 20_732 since 1970.
        let depart: Vec<i64> = rows
            .iter()
            .map(|r| {
                let (h, m): (i64, i64) = (r[9][11..13].parse().unwrap(), r[9][14..16].parse().unwrap());
                (20_732 * 86_400 + (h - 8) * 3_600 + m * 60) * 1000
            })
            .collect();
        let schema = Arc::new(Schema::new(vec![
            Field::new("trip_id", DataType::Utf8, true),
            Field::new("origin_town", DataType::Utf8, true),
            Field::new("origin_lat", DataType::Float64, true),
            Field::new("origin_lng", DataType::Float64, true),
            Field::new("dest_lat", DataType::Float64, true),
            Field::new("dest_lng", DataType::Float64, true),
            Field::new("fare", DataType::Float64, true),
            Field::new("depart", DataType::Timestamp(arrow_schema::TimeUnit::Millisecond, Some("UTC".into())), true),
        ]));
        let cols: Vec<ArrayRef> = vec![text(0), text(1), num(2), num(3), num(5), num(6), num(8), Arc::new(TimestampMillisecondArray::from(depart).with_timezone("UTC"))];
        let batch = RecordBatch::try_new(Arc::clone(&schema), cols).unwrap();
        let mut out = Vec::new();
        let mut w = arrow_ipc::writer::FileWriter::try_new(&mut out, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
        drop(w);
        std::fs::write(dir.join("sg_od_synthetic.arrow"), out).unwrap();
    }

    #[test]
    fn a_missing_column_or_a_non_ipc_file_is_an_error() {
        assert!(arrow_ipc_to_geoparquet(&ipc_fixture(), Some(&["nope"]), 1024).unwrap_err().contains("nope"));
        assert!(arrow_ipc_to_geoparquet(b"not arrow", None, 1024).is_err());
    }

    #[test]
    fn geojson_converts_with_typed_columns() {
        let doc = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},"properties":{"elevation":100,"name":"a"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[1,1],[2,0]]},"properties":{"elevation":"x"}}
        ]}"#;
        let converted = geojson_to_geoparquet(doc, &[("elevation", WriteType::Number), ("name", WriteType::Str)], 1024).unwrap();
        assert_eq!(converted.rows, 2);
        let gpq = GeoParquetSource::from_bytes(&converted.bytes, &GeoParquetOptions::default()).unwrap();
        assert_eq!(gpq.len(), 2);
        assert!(gpq.column("elevation").is_some() && gpq.column("name").is_some());
    }
}
