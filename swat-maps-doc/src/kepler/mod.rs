//! Kepler.gl config import and export (spec §2.9, §7).
//!
//! [`import`] reads a Kepler.gl `v1` or `v0` config, or a full map export
//! with its datasets, into a [`Doc`], and a [`Report`] of every item it
//! dropped or approximated. [`export_config`] and [`export_map`] write a
//! `v1` config (and, for a map, its rows) back, with Kepler.gl's required
//! defaults filled so its own loader accepts the file.
//!
//! The input is untrusted: its size and nesting are bounded before it is
//! parsed, every value is read by checked access, and a wrong type
//! becomes a report line or a [`KeplerError`], never a panic. No URL is
//! fetched, no style URL or token is copied into the document.
//!
//! Kepler.gl is MIT; its schema code was read for key names, defaults and
//! the `v0` migration. No code or JSON is copied from it.
//!
//! Layers and filters the Explorer cannot draw are kept verbatim in
//! `explorer.unsupported_layers` / `explorer.unsupported_filters`
//! (`[{"index": n, "layer"|"filter": {...}}]`, `n` the position in the
//! Kepler.gl list) and written back where they were. Keys of a Kepler.gl
//! layer the document does not model are kept in the layer's
//! `extra["kepler"]` (`{"config": {...}, "visualChannels": {...}}`) and
//! written back too. Draw order: Kepler.gl lists its top layer first, the
//! document draws `layers[0]` first, so the order flips both ways.

mod export;
mod import;
mod report;

use serde_json::{Map, Value};

use crate::doc::Doc;

pub use export::{export_config, export_map};
pub use import::import;
pub use report::Report;

/// Bounds for untrusted input: the whole text.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
/// JSON nesting.
pub const MAX_DEPTH: usize = 64;
/// Layers read; past this the rest are dropped with a report line.
pub const MAX_LAYERS: usize = 200;

/// Kepler.gl's `colorAggregation` names the Explorer draws (all of them,
/// with engine gap E10 closed).
pub const AGGREGATIONS: [&str; 10] = [
    "count",
    "sum",
    "average",
    "maximum",
    "minimum",
    "median",
    "stdev",
    "variance",
    "mode",
    "countUnique",
];
/// Filters read; past this the rest are dropped with a report line.
pub const MAX_FILTERS: usize = 200;

/// Why a file could not be imported at all.
#[derive(Debug, thiserror::Error)]
pub enum KeplerError {
    #[error("This file is {mb} MB; the importer reads up to 64 MB. Export the config without its data, or load the data separately.")]
    TooLarge { mb: usize },
    #[error("This file nests deeper than 64 levels, so it is not a Kepler.gl config. Check that you picked the right file.")]
    TooDeep,
    #[error("This file is not valid JSON (line {line}, column {column}). Export it from Kepler.gl again.")]
    NotJson { line: usize, column: usize },
    #[error("This JSON is not a Kepler.gl config: it has no config with a version. Export the map from Kepler.gl again.")]
    NotKepler,
    #[error("This Kepler.gl config is version '{0}'; the Explorer reads v0 and v1. Save it again from a current Kepler.gl.")]
    Version(String),
}

/// A finished import.
#[derive(Clone, Debug, PartialEq)]
pub struct Imported {
    pub doc: Doc,
    pub report: Report,
    /// dataIds referenced by layers/filters that the file carries no
    /// dataset for (config-only import): the host matches them to loaded
    /// datasets by id then label. Sorted, no repeats.
    pub missing: Vec<String>,
}

/// Cheap sniff on parsed JSON: the top level has `config.version` `"v0"` or
/// `"v1"` (a full map export), or `version` beside `config` (a config), or
/// `datasets` beside `config`.
pub fn looks_like_kepler(value: &Value) -> bool {
    let Some(top) = value.as_object() else {
        return false;
    };
    let Some(config) = top.get("config").and_then(Value::as_object) else {
        return false;
    };
    let versioned = |v: Option<&Value>| matches!(v.and_then(Value::as_str), Some("v0" | "v1"));
    versioned(config.get("version"))
        || versioned(top.get("version"))
        || top.get("datasets").is_some_and(Value::is_array)
}

/// Renames a source id across layers, filters, the kept unsupported
/// layers and filters, and the source table itself (when `to` is not
/// taken). Used when a config-only import's dataId matches a loaded
/// dataset with another id. Hover cards belong to layers, so they move
/// with them.
pub fn rebind_source(doc: &mut Doc, from: &str, to: &str) {
    if from == to {
        return;
    }
    for layer in doc.map.layers.iter_mut().filter(|l| l.source == from) {
        layer.source = to.to_string();
    }
    for filter in doc.filters.iter_mut().filter(|f| f.source == from) {
        filter.source = to.to_string();
    }
    if !doc.sources.contains_key(to) {
        if let Some(source) = doc.sources.remove(from) {
            doc.sources.insert(to.to_string(), source);
        }
    }
    let Some(explorer) = doc.explorer.as_object_mut() else {
        return;
    };
    if let Some(items) = explorer
        .get_mut("unsupported_layers")
        .and_then(Value::as_array_mut)
    {
        for item in items {
            if let Some(config) = item
                .get_mut("layer")
                .and_then(|l| l.get_mut("config"))
                .and_then(Value::as_object_mut)
            {
                rename_data_id(config, from, to);
            }
        }
    }
    if let Some(items) = explorer
        .get_mut("unsupported_filters")
        .and_then(Value::as_array_mut)
    {
        for item in items {
            if let Some(filter) = item.get_mut("filter").and_then(Value::as_object_mut) {
                rename_data_id(filter, from, to);
            }
        }
    }
    if let Some(types) = explorer
        .get_mut("field_types")
        .and_then(Value::as_object_mut)
    {
        if !types.contains_key(to) {
            if let Some(v) = types.remove(from) {
                types.insert(to.to_string(), v);
            }
        }
    }
    doc.place_filters();
}

fn rename_data_id(object: &mut Map<String, Value>, from: &str, to: &str) {
    match object.get_mut("dataId") {
        Some(Value::String(s)) if s == from => *s = to.to_string(),
        Some(Value::Array(ids)) => {
            for id in ids {
                if id.as_str() == Some(from) {
                    *id = Value::String(to.to_string());
                }
            }
        }
        _ => {}
    }
}

/// Whether the bracket nesting of `text` passes `max` (strings skipped).
/// Runs before parsing, so a hostile file never reaches the parser's
/// recursion.
pub(crate) fn nests_deeper_than(text: &str, max: usize) -> bool {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for b in text.bytes() {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > max {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// Whether a string looks like an access token or a URL carrying one.
pub(crate) fn looks_like_token(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("access_token")
        || lower.contains("accesstoken")
        || ["pk.", "sk.", "tk."]
            .iter()
            .any(|p| s.starts_with(p) && s.len() > 20)
        || s.contains("pk.ey")
        || s.contains("sk.ey")
}

/// Removes token keys (`access_token`, `accessToken`, ...) and blanks
/// token-like strings anywhere in `value`. Returns whether it removed
/// anything.
pub(crate) fn strip_tokens(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let before = map.len();
            map.retain(|k, _| {
                let k = k.to_ascii_lowercase();
                !(k == "access_token"
                    || k == "accesstoken"
                    || k == "token"
                    || k == "apikey"
                    || k == "api_key")
            });
            let mut removed = map.len() != before;
            for v in map.values_mut() {
                removed |= strip_tokens(v);
            }
            removed
        }
        Value::Array(items) => {
            // Every item, not the first hit only.
            let mut removed = false;
            for v in items {
                removed |= strip_tokens(v);
            }
            removed
        }
        Value::String(s) if looks_like_token(s) => {
            s.clear();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defaults::default_layers;
    use crate::doc::{DataRef, LayerType, Source};
    use crate::filters::{FilterKind, FilterValue};
    use crate::table::{Column, Field, FieldType, Geometry, Table};

    const POINTS: &str = include_str!("../../tests/fixtures/kepler/points_quantize_sqrt.json");
    const CHORO: &str = include_str!("../../tests/fixtures/kepler/choropleth_quantile.json");
    const TIME: &str = include_str!("../../tests/fixtures/kepler/time_range.json");
    const HEX: &str = include_str!("../../tests/fixtures/kepler/hexagon.json");
    const ARC: &str = include_str!("../../tests/fixtures/kepler/arc.json");
    const V0: &str = include_str!("../../tests/fixtures/kepler/v0_points.json");
    const MAP: &str = include_str!("../../tests/fixtures/kepler/map_with_data.json");
    const LOSS: &str = include_str!("../../tests/fixtures/kepler/loss.json");
    const FOCUS4: &str = include_str!("../../tests/fixtures/kepler/review_focus_4.json");
    const HEAT: &str = include_str!("../../tests/fixtures/kepler/heatmap.json");
    const TRIP: &str = include_str!("../../tests/fixtures/kepler/trip.json");

    const SHAPES: [(&str, &str); 7] = [
        ("points_quantize_sqrt", POINTS),
        ("choropleth_quantile", CHORO),
        ("time_range", TIME),
        ("hexagon", HEX),
        ("arc", ARC),
        ("heatmap", HEAT),
        ("trip", TRIP),
    ];
    const ALL: [(&str, &str); 9] = [
        ("points_quantize_sqrt", POINTS),
        ("choropleth_quantile", CHORO),
        ("time_range", TIME),
        ("hexagon", HEX),
        ("arc", ARC),
        ("v0_points", V0),
        ("map_with_data", MAP),
        ("loss", LOSS),
        ("review_focus_4", FOCUS4),
    ];

    fn no_palettes(_: &str, _: bool) -> Option<Vec<String>> {
        None
    }

    fn json(text: &str) -> Value {
        serde_json::from_str(text).expect("the export is JSON")
    }

    #[test]
    fn kepler_round_trip() {
        for (name, text) in SHAPES {
            let first = import(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            let (out, _) = export_config(&first.doc, &no_palettes);
            assert!(looks_like_kepler(&json(&out)), "{name}");
            let second = import(&out).unwrap_or_else(|e| panic!("{name}: {e}\n{out}"));
            assert_eq!(first.doc, second.doc, "{name}\n{out}");
            assert_eq!(first.report, second.report, "{name}");
            assert_eq!(first.missing, second.missing, "{name}");
            // And once more: the export is stable.
            assert_eq!(export_config(&second.doc, &no_palettes).0, out, "{name}");
        }
        for (name, text) in [("points", POINTS), ("choropleth", CHORO), ("time", TIME)] {
            let r = import(text).unwrap().report;
            assert!(r.is_empty(), "{name}: {}", r.to_text());
        }
        let points = import(POINTS).unwrap().doc;
        let l = &points.map.layers[0];
        assert_eq!(l.columns.get("lon").map(String::as_str), Some("Longitude"));
        let c = l.color.as_ref().unwrap();
        assert_eq!(
            (
                c.field.as_str(),
                c.scale.as_str(),
                c.palette.as_deref(),
                c.steps
            ),
            ("Magnitude", "quantize", Some("Sunrise 5"), Some(5))
        );
        assert_eq!(c.extra["colors"].as_array().map(Vec::len), Some(5));
        let r = l.radius.as_ref().unwrap();
        assert_eq!(
            (r.field.as_str(), r.scale.as_str(), r.range),
            ("Depth", "sqrt", Some([2.0, 40.0]))
        );
        assert_eq!(
            (
                l.vis.radius,
                l.vis.opacity,
                l.vis.outline,
                l.vis.thickness,
                l.vis.stroke_color
            ),
            (12.0, 0.7, true, 1.5, Some([20, 20, 20]))
        );
        assert_eq!(
            l.hover
                .card
                .iter()
                .map(|h| (h.field.as_str(), h.format.as_deref()))
                .collect::<Vec<_>>(),
            [("Magnitude", Some(".1f")), ("Place", None)]
        );
        assert_eq!(
            (points.map.view.basemap.as_str(), points.map.view.zoom),
            ("dark", 6.2)
        );
        let time = import(TIME).unwrap().doc;
        assert_eq!(time.filters.len(), 3);
        assert_eq!(
            (
                time.filters[0].kind,
                &time.filters[0].value,
                time.filters[0].speed
            ),
            (
                FilterKind::TimeRange,
                &FilterValue::Range([1704067200000.0, 1704153600000.0]),
                Some(2.0)
            )
        );
        assert_eq!(
            time.filters[1].value,
            FilterValue::Values(vec!["bus".into(), "van".into()])
        );
        assert!(!time.filters[1].enabled);
        assert_eq!(time.filters[2].value, FilterValue::Bool(true));
        let arc = import(ARC).unwrap().doc;
        assert_eq!(arc.map.view.projection, "globe");
        assert!(!arc.map.layers[0].visible);
    }

    /// P8: a heatmap imports drawn, with its radius, intensity, threshold,
    /// palette and weight field, and goes back out as a `heatmap`.
    #[test]
    fn a_heatmap_imports_as_a_drawn_layer_with_its_settings() {
        let got = import(HEAT).unwrap();
        assert!(got.report.is_empty(), "{}", got.report.to_text());
        let l = &got.doc.map.layers[0];
        assert_eq!(l.kind, LayerType::Heatmap);
        assert_eq!(
            (
                l.vis.radius,
                l.vis.opacity,
                l.vis.intensity(),
                l.vis.threshold()
            ),
            (32.0, 0.7, 2.5, 0.1)
        );
        assert_eq!(
            (l.columns["lat"].as_str(), l.columns["lon"].as_str()),
            ("Latitude", "Longitude")
        );
        let c = l.color.as_ref().unwrap();
        assert_eq!(
            (c.field.as_str(), c.palette.as_deref()),
            ("", Some("Ember 4"))
        );
        assert_eq!(
            l.radius.as_ref().map(|b| (b.field.as_str(), b.range)),
            Some(("Magnitude", None)),
            "the weight rides in the size channel"
        );
        let (out, _) = export_config(&got.doc, &no_palettes);
        let v = json(&out);
        let layer = &v["config"]["visState"]["layers"][0];
        assert_eq!(layer["type"], "heatmap");
        assert_eq!(layer["visualChannels"]["weightField"]["name"], "Magnitude");
        assert_eq!(layer["config"]["visConfig"]["intensity"], 2.5);
        assert!(
            layer["config"]["visConfig"].get("radiusRange").is_none(),
            "no point radius range on a heatmap"
        );
    }

    /// P8: trips import drawn: GeoJSON timed lines and rows grouped by id,
    /// with the trail length, width and colour by field.
    #[test]
    fn trips_import_as_drawn_layers_with_their_settings() {
        let got = import(TRIP).unwrap();
        assert!(got.report.is_empty(), "{}", got.report.to_text());
        let geo = got.doc.layer("flights-trip").unwrap();
        assert_eq!(
            (geo.kind, geo.vis.trail_s(), geo.vis.thickness),
            (LayerType::Trip, 600.0, 3.0)
        );
        assert_eq!(
            geo.columns.get("geojson").map(String::as_str),
            Some("_geojson")
        );
        let rows = got.doc.layer("buses-trip").unwrap();
        assert_eq!(rows.vis.trail_s(), crate::doc::DEFAULT_TRAIL_S);
        assert_eq!(
            rows.columns
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            [
                ("id", "bus"),
                ("lat", "lat"),
                ("lon", "lon"),
                ("timestamp", "time")
            ]
        );
        assert_eq!(
            rows.color
                .as_ref()
                .map(|b| (b.field.as_str(), b.scale.as_str())),
            Some(("route", "ordinal"))
        );
        let (out, _) = export_config(&got.doc, &no_palettes);
        let v = json(&out);
        let layers = v["config"]["visState"]["layers"].as_array().unwrap();
        assert!(layers.iter().all(|l| l["type"] == "trip"));
        let buses = layers.iter().find(|l| l["id"] == "buses-trip").unwrap();
        assert_eq!(buses["config"]["columns"]["lng"], "lon");
    }

    /// P6: hexagon and arc layers are drawn, not kept: their settings land
    /// on the layer, nothing says "not drawn", and the export writes them
    /// back as the same Kepler types.
    #[test]
    fn hexagon_and_arc_layers_import_as_drawn_layers() {
        let hex = import(HEX).unwrap();
        assert!(
            !hex.report.to_text().contains("not drawn"),
            "{}",
            hex.report.to_text()
        );
        assert!(hex.doc.explorer.get("unsupported_layers").is_none());
        let h = hex
            .doc
            .map
            .layers
            .iter()
            .find(|l| l.kind == LayerType::Hexbin)
            .expect("a hexbin layer");
        assert_eq!(
            (h.vis.cell_km, h.vis.enable3d, h.vis.opacity),
            (Some(0.5), true, 0.75)
        );
        assert_eq!(h.columns.get("lat").map(String::as_str), Some("lat"));
        assert_eq!(h.columns.get("lon").map(String::as_str), Some("lon"));
        let c = h.color.as_ref().expect("a count colour");
        assert!(c.field.is_empty(), "no field: the point count");
        assert_eq!(c.palette.as_deref(), Some("Ochre 4"));
        let arc = import(ARC).unwrap();
        let a = arc
            .doc
            .map
            .layers
            .iter()
            .find(|l| l.kind == LayerType::Arc)
            .expect("an arc layer");
        assert_eq!(
            a.columns.get("lon0").map(String::as_str),
            Some("origin_lon")
        );
        assert_eq!(a.columns.get("lat1").map(String::as_str), Some("dest_lat"));
        assert_eq!(a.vis.target_color, Some([255, 203, 153]));
        assert!(
            arc.report
                .to_text()
                .contains("not applied: size by 'trips'"),
            "arc width by field is gap E8: {}",
            arc.report.to_text()
        );
        let (out, _) = export_config(&arc.doc, &no_palettes);
        let v = json(&out);
        let types: Vec<&str> = v["config"]["visState"]["layers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|l| l["type"].as_str())
            .collect();
        assert!(
            types.contains(&"arc") && types.contains(&"point"),
            "{types:?}"
        );
        let (out, _) = export_config(&hex.doc, &no_palettes);
        let v = json(&out);
        let l = v["config"]["visState"]["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["type"] == "hexagon")
            .expect("a hexagon");
        assert_eq!(l["config"]["visConfig"]["worldUnitSize"], 0.5);
        assert_eq!(l["config"]["visConfig"]["enable3d"], true);
        assert_eq!(l["config"]["visConfig"]["colorRange"]["name"], "Ochre 4");
        assert!(l["visualChannels"]["colorField"].is_null());
    }

    /// `visState.layerOrder` (top first) decides the draw order over the
    /// `layers` array's: kepler.gl 3's ids and 2.x's indices both; a layer
    /// it does not name goes under the named ones. Export writes it back,
    /// so a reorder round-trips.
    #[test]
    fn layer_order_imports_by_id_or_index_and_round_trips() {
        let layers = r#"[
            {"id":"a","type":"point","config":{"dataId":"d","label":"A","columns":{"lat":"lat","lng":"lon"}}},
            {"id":"b","type":"point","config":{"dataId":"d","label":"B","columns":{"lat":"lat","lng":"lon"}}},
            {"id":"c","type":"point","config":{"dataId":"d","label":"C","columns":{"lat":"lat","lng":"lon"}}}
        ]"#;
        let with = |order: &str| {
            format!(r#"{{"version":"v1","config":{{"visState":{{"layers":{layers}{order}}}}}}}"#)
        };
        let ids = |text: &str| -> Vec<String> {
            import(text)
                .unwrap()
                .doc
                .map
                .layers
                .iter()
                .map(|l| l.id.clone())
                .collect()
        };
        // No layerOrder: `layers` is top first, the document bottom first.
        assert_eq!(ids(&with("")), ["c", "b", "a"]);
        assert_eq!(
            ids(&with(r#","layerOrder":["b","c","a"]"#)),
            ["a", "c", "b"],
            "ids: b on top"
        );
        assert_eq!(
            ids(&with(r#","layerOrder":[2,0,1]"#)),
            ["b", "a", "c"],
            "indices: c on top"
        );
        assert_eq!(
            ids(&with(r#","layerOrder":["c"]"#)),
            ["b", "a", "c"],
            "an unnamed layer goes under the named"
        );
        // Reordered here, exported, read back: the same order.
        let mut doc = import(&with("")).unwrap().doc;
        assert_eq!(
            doc.apply(crate::action::DocAction::MoveLayer {
                layer: "c".into(),
                to: 2
            }),
            vec![crate::action::Effect::Reorder]
        );
        let (out, _) = export_config(&doc, &|_, _| None);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["config"]["visState"]["layerOrder"],
            serde_json::json!(["c", "a", "b"])
        );
        assert_eq!(ids(&out), ["b", "a", "c"]);
    }

    /// Grid, line, cluster, 3D GeoJSON, H3, heatmap and trip layers come in
    /// as their own types with their settings; icon, text and S2 stay kept
    /// and not drawn.
    #[test]
    fn grid_line_cluster_3d_and_h3_import_and_the_rest_stay_kept() {
        let text = r#"{"version":"v1","config":{"visState":{"layers":[
            {"id":"g","type":"grid","config":{"dataId":"d","label":"G","columns":{"lat":"lat","lng":"lon"},"visConfig":{"worldUnitSize":2,"colorAggregation":"median","coverage":0.8,"percentile":[5,95]}},"visualChannels":{"colorField":{"name":"mag","type":"real"},"colorScale":"quantile"}},
            {"id":"l","type":"line","config":{"dataId":"d","label":"L","columns":{"lat0":"a_lat","lng0":"a_lon","lat1":"b_lat","lng1":"b_lon"},"visConfig":{"targetColor":[1,2,3]}}},
            {"id":"c","type":"cluster","config":{"dataId":"d","label":"C","columns":{"lat":"lat","lng":"lon"},"visConfig":{"clusterRadius":60}}},
            {"id":"p","type":"geojson","config":{"dataId":"d","label":"P","columns":{"geojson":"_geojson"},"visConfig":{"enable3d":true,"elevationScale":2,"heightRange":[0,300]}},"visualChannels":{"heightField":{"name":"pop","type":"integer"},"heightScale":"linear"}},
            {"id":"h","type":"hexagonId","config":{"dataId":"d","label":"H","columns":{"hex_id":"h3"}}},
            {"id":"hm","type":"heatmap","config":{"dataId":"d","label":"Heat","columns":{"lat":"lat","lng":"lon"}}},
            {"id":"ic","type":"icon","config":{"dataId":"d","label":"Icons"}},
            {"id":"tr","type":"trip","config":{"dataId":"d","label":"Trips"}},
            {"id":"s2","type":"s2","config":{"dataId":"d","label":"Cells"}}
        ]}}}"#;
        let got = import(text).unwrap();
        let kinds: Vec<LayerType> = got.doc.map.layers.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LayerType::Trip,
                LayerType::Heatmap,
                LayerType::H3,
                LayerType::GeoJson,
                LayerType::Cluster,
                LayerType::Line,
                LayerType::Grid
            ]
        );
        let l = |id: &str| got.doc.layer(id).unwrap();
        assert_eq!(
            (
                l("g").vis.cell_km,
                l("g").vis.aggregation.as_deref(),
                l("g").vis.coverage,
                l("g").vis.percentile
            ),
            (Some(2.0), Some("median"), Some(0.8), Some([5.0, 95.0]))
        );
        assert_eq!(l("g").color.as_ref().map(|b| b.field.as_str()), Some("mag"));
        assert_eq!(l("l").vis.target_color, Some([1, 2, 3]));
        assert_eq!(l("c").vis.cluster_radius, Some(60.0));
        assert!(l("p").vis.enable3d);
        assert_eq!(
            l("p").height.as_ref().map(|b| (b.field.as_str(), b.range)),
            Some(("pop", Some([0.0, 300.0])))
        );
        assert_eq!(l("h").columns.get("hex_id").map(String::as_str), Some("h3"));
        let kept = got.doc.explorer["unsupported_layers"].as_array().unwrap();
        assert_eq!(kept.len(), 2, "icon and s2 stay kept");
        let text = got.report.to_text();
        for name in ["'Icons'", "'Cells'"] {
            assert!(
                got.report
                    .lines()
                    .any(|line| line.contains("not drawn") && line.contains(name)),
                "{name}: {text}"
            );
        }
        // And back: every type keeps its Kepler name.
        let (out, _) = export_config(&got.doc, &no_palettes);
        let again = import(&out).unwrap();
        assert_eq!(again.doc.map.layers, got.doc.map.layers);
    }

    #[test]
    fn every_dropped_item_names_itself() {
        let r = import(LOSS).unwrap().report;
        let text = r.to_text();
        let has = |words: &[&str]| r.lines().any(|l| words.iter().all(|w| l.contains(w)));
        assert!(has(&["not drawn", "icon", "'Density'"]), "{text}");
        assert!(has(&["not applied", "polygon filter", "'f3'"]), "{text}");
        assert!(has(&["approximated", "jenks", "'Quakes'"]), "{text}");
        assert!(has(&["split map"]), "{text}");
        assert!(has(&["lightAndShadow", "'glow-1'"]), "{text}");
        assert!(has(&["custom map style", "'Night Ops'"]), "{text}");
        assert!(
            !text.contains("mapbox://") && !text.contains("pk."),
            "no style URL or token in the report: {text}"
        );
        let doc = import(LOSS).unwrap().doc.to_json();
        assert!(
            !doc.contains("mapbox://") && !doc.contains("pk.ey") && !doc.contains("accessToken"),
            "{doc}"
        );
    }

    #[test]
    fn v0_configs_migrate_like_kepler() {
        let i = import(V0).unwrap();
        assert!(i.report.is_empty(), "{}", i.report.to_text());
        assert_eq!(i.missing, ["faults", "quakes"]);
        let doc = i.doc;
        // Kepler.gl lists the top layer first; the document draws first.
        assert_eq!(
            doc.map
                .layers
                .iter()
                .map(|l| l.id.as_str())
                .collect::<Vec<_>>(),
            ["old-shapes", "old-points"]
        );
        let p = doc.layer("old-points").unwrap();
        assert_eq!(p.kind, LayerType::Points);
        assert_eq!(
            (p.columns["lat"].as_str(), p.columns["lon"].as_str()),
            ("Latitude", "Longitude")
        );
        let c = p.color.as_ref().unwrap();
        assert_eq!(
            (
                c.field.as_str(),
                c.scale.as_str(),
                c.palette.as_deref(),
                c.steps
            ),
            ("Magnitude", "quantize", Some("Ember 3"), Some(3))
        );
        let r = p.radius.as_ref().unwrap();
        assert_eq!(
            (r.field.as_str(), r.scale.as_str(), r.range),
            ("Depth", "sqrt", Some([1.0, 30.0]))
        );
        // An old outlined point is stroked in its own colour, unfilled.
        assert_eq!(
            (
                p.vis.outline,
                p.vis.filled,
                p.vis.stroke_color,
                p.vis.radius,
                p.vis.thickness
            ),
            (true, false, Some([200, 60, 60]), 15.0, 3.0)
        );
        assert_eq!(
            p.hover
                .card
                .iter()
                .map(|h| h.field.as_str())
                .collect::<Vec<_>>(),
            ["Magnitude", "Place"]
        );
        // A geojson sizeField with a non-default radius meant the radius.
        let g = doc.layer("old-shapes").unwrap();
        assert_eq!(g.columns["geojson"], "_geojson");
        let r = g.radius.as_ref().unwrap();
        assert_eq!(
            (r.field.as_str(), r.scale.as_str()),
            ("slip_rate", "linear")
        );
        assert!(!g.vis.outline);
        let f = &doc.filters[0];
        assert_eq!(
            (f.id.as_str(), f.source.as_str(), f.field.as_str(), &f.value),
            (
                "mag-filter",
                "quakes",
                "Magnitude",
                &FilterValue::Range([3.5, 7.2])
            )
        );
        assert_eq!(
            f.extra.get("view"),
            Some(&Value::String("side".into())),
            "enlarged false becomes the side view"
        );
    }

    #[test]
    fn an_unsupported_icon_layer_round_trips_verbatim() {
        let input = json(LOSS);
        let heat_in = &input["config"]["visState"]["layers"][0];
        assert_eq!(heat_in["type"], "icon");
        let doc = import(LOSS).unwrap().doc;
        assert!(
            doc.map.layers.iter().all(|l| l.id != "quakes-heat"),
            "not drawn"
        );
        let (out, _) = export_config(&doc, &no_palettes);
        let out = json(&out);
        let layers = out["config"]["visState"]["layers"].as_array().unwrap();
        assert_eq!(&layers[0], heat_in, "same JSON, same place");
        assert_eq!(
            out["config"]["visState"]["filters"][0], input["config"]["visState"]["filters"][0],
            "the polygon filter too"
        );
    }

    #[test]
    fn a_jenks_scale_imports_as_quantile_with_a_warning() {
        let i = import(FOCUS4).unwrap();
        assert_eq!(
            i.doc
                .layer("quakes-pts")
                .unwrap()
                .color
                .as_ref()
                .unwrap()
                .scale,
            "quantile"
        );
        assert_eq!(
            i.report.approximated,
            ["approximated: jenks colour scale on layer 'Quakes' drawn as quantile"]
        );
    }

    #[test]
    fn the_icon_layer_polygon_filter_and_jenks_give_three_named_warnings() {
        let r = import(FOCUS4).unwrap().report;
        let lines: Vec<&str> = r.lines().collect();
        assert_eq!(lines.len(), 3, "{}", r.to_text());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("icon") && l.contains("'Density'")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("polygon") && l.contains("'f3'")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("jenks") && l.contains("'Quakes'")),
            "{lines:?}"
        );
    }

    #[test]
    fn full_map_datasets_become_embedded_sources() {
        let i = import(MAP).unwrap();
        assert!(i.missing.is_empty());
        assert_eq!(i.doc.title, "Five cities");
        assert_eq!(i.doc.map.view.basemap, "light");
        let s = &i.doc.sources["cities"];
        assert_eq!((s.kind.as_str(), s.label.as_str()), ("csv", "Five cities"));
        assert_eq!(
            (
                s.data.kind.as_str(),
                s.data.encoding.as_deref(),
                s.data.name.as_deref()
            ),
            ("embedded", Some("text"), Some("Five cities.csv"))
        );
        assert_eq!(s.columns["riders"], "integer");
        assert_eq!(s.columns["opened"], "timestamp");
        assert_eq!(s.columns["tags"], "string");
        assert_eq!(
            s.geometry,
            serde_json::json!({ "lat": "lat", "lon": "lon" })
        );
        let csv = s.data.text.as_deref().unwrap();
        let lines: Vec<&str> = csv.split("\r\n").collect();
        assert_eq!(lines.len(), 7, "header, five rows, a final break: {csv}");
        assert_eq!(lines[0], "name,lat,lon,riders,opened,night_service,tags");
        assert_eq!(
            lines[1],
            "Alpha Town,1.3,103.85,1200,2021-03-01 08:00:00,true,\"[\"\"a\"\",\"\"b\"\"]\""
        );
        assert_eq!(
            lines[2],
            "\"Beta, Upper\",35.69,139.7,5400,1617235200000,false,[]"
        );
        assert_eq!(
            lines[3],
            "\"Gamma \"\"Old\"\" Port\",52.37,4.9,,2022-11-15 17:30:00,,"
        );
        assert_eq!(
            i.report.lines().collect::<Vec<_>>(),
            ["approximated: array field 'tags' of dataset 'Five cities' read as text"]
        );
        assert!(
            i.report.lines().all(|l| !l.contains("Alpha")),
            "no cell value in a report line"
        );
    }

    #[test]
    fn a_config_only_import_lists_its_missing_datasets() {
        assert_eq!(import(POINTS).unwrap().missing, ["quakes"]);
        assert_eq!(import(TIME).unwrap().missing, ["trips"]);
        assert_eq!(import(LOSS).unwrap().missing, ["quakes"]);
        assert!(import(MAP).unwrap().missing.is_empty());
    }

    #[test]
    fn rebind_source_moves_layers_filters_and_hover() {
        let mut doc = import(TIME).unwrap().doc;
        let card = doc.map.layers[0].hover.card.clone();
        rebind_source(&mut doc, "trips", "trips_2024");
        assert!(doc.map.layers.iter().all(|l| l.source == "trips_2024"));
        assert!(doc.filters.iter().all(|f| f.source == "trips_2024"));
        assert_eq!(
            doc.map.layers[0].hover.card, card,
            "the card moves with its layer"
        );
        let (out, _) = export_config(&doc, &no_palettes);
        let out = json(&out);
        assert_eq!(
            out["config"]["visState"]["interactionConfig"]["tooltip"]["fieldsToShow"]["trips_2024"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );

        let mut loss = import(LOSS).unwrap().doc;
        rebind_source(&mut loss, "quakes", "eq");
        assert!(loss.map.layers.iter().all(|l| l.source == "eq"));
        assert_eq!(
            loss.explorer["unsupported_layers"][0]["layer"]["config"]["dataId"],
            "eq"
        );
        assert_eq!(
            loss.explorer["unsupported_filters"][0]["filter"]["dataId"],
            serde_json::json!(["eq"])
        );
    }

    fn table() -> Table {
        let f = |name: &str, ty: FieldType, column: Column| Field {
            name: name.into(),
            ty,
            hint: None,
            column,
        };
        Table {
            id: "stops".into(),
            label: "stops.csv".into(),
            rows: 3,
            geometry: Geometry::Points(vec![[103.8, 1.3], [139.7, 35.7], [4.9, 52.4]]),
            fields: vec![
                f(
                    "name",
                    FieldType::String,
                    Column::from_strs([Some("A, one"), Some("B"), None]),
                ),
                f(
                    "lat",
                    FieldType::Real,
                    Column::Number(vec![Some(1.3), Some(35.7), Some(52.4)]),
                ),
                f(
                    "lon",
                    FieldType::Real,
                    Column::Number(vec![Some(103.8), Some(139.7), Some(4.9)]),
                ),
                f(
                    "riders",
                    FieldType::Integer,
                    Column::Number(vec![Some(12.0), None, Some(7.0)]),
                ),
                f(
                    "open",
                    FieldType::Boolean,
                    Column::Bool(vec![Some(true), Some(false), None]),
                ),
                f(
                    "since",
                    FieldType::Timestamp,
                    Column::Time {
                        ms: vec![Some(0.0), Some(1.0), None],
                        raw: Some(Box::new(Column::from_strs([
                            Some("1970-01-01"),
                            Some("1970-01-01 00:00:00.001"),
                            None,
                        ]))),
                    },
                ),
            ],
        }
    }

    #[test]
    fn export_map_writes_all_data_rows() {
        let t = table();
        let mut doc = Doc::default();
        let source = |label: &str| Source {
            kind: "csv".into(),
            data: DataRef::file("stops.csv"),
            label: label.into(),
            columns: Default::default(),
            geometry: serde_json::json!({ "lat": "lat", "lon": "lon" }),
            attribution: None,
            extra: Default::default(),
        };
        doc.sources.insert("stops".into(), source("Stops"));
        doc.sources.insert("gone".into(), source("Gone"));
        doc.map.layers = default_layers(&t, 0);
        let (out, report) = export_map(
            &doc,
            &|id: &str| (id == "stops").then_some(&t),
            &no_palettes,
        );
        assert_eq!(
            report.lines().collect::<Vec<_>>(),
            ["dropped: the rows of dataset 'Gone' (not loaded; written as config only)"]
        );
        let v = json(&out);
        assert!(looks_like_kepler(&v));
        let data = &v["datasets"][0]["data"];
        assert_eq!(data["id"], "stops");
        let rows = data["allData"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            serde_json::json!(["A, one", 1.3, 103.8, 12, true, "1970-01-01"])
        );
        assert_eq!(rows[1][3], Value::Null);
        assert_eq!(rows[2], serde_json::json!([null, 52.4, 4.9, 7, null, null]));
        assert_eq!(
            data["fields"][3],
            serde_json::json!({ "name": "riders", "type": "integer", "format": "", "analyzerType": "INT" })
        );
        // It reads back as an embedded source with the same rows.
        let back = import(&out).unwrap();
        let csv = back.doc.sources["stops"].data.text.clone().unwrap();
        assert_eq!(
            csv.split("\r\n").filter(|l| !l.is_empty()).count(),
            4,
            "{csv}"
        );
        assert_eq!(back.doc.map.layers[0].columns["lat"], "lat");
        // A feature table without its shapes is written config only.
        let mut shapes = table();
        shapes.geometry = Geometry::Features {
            count: 3,
            bounds: None,
            all_points: false,
        };
        let (out, report) = export_map(
            &doc,
            &|id: &str| (id == "stops").then_some(&shapes),
            &no_palettes,
        );
        assert!(
            report
                .lines()
                .any(|l| l.contains("'Stops'") && l.contains("shapes")),
            "{}",
            report.to_text()
        );
        assert_eq!(json(&out)["datasets"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn an_explorer_only_palette_is_resolved_or_reported() {
        let t = table();
        let mut doc = Doc::default();
        doc.map.layers = default_layers(&t, 0);
        let mut b = crate::doc::ChannelBinding::new("riders", "quantile");
        b.palette = Some("Teal 3".into());
        doc.map.layers[0].color = Some(b);
        let palettes = |name: &str, _: bool| {
            (name == "Teal 3")
                .then(|| vec!["#000000".to_string(), "#777777".into(), "#FFFFFF".into()])
        };
        let (out, report) = export_config(&doc, &palettes);
        assert!(report.is_empty(), "{}", report.to_text());
        assert_eq!(
            json(&out)["config"]["visState"]["layers"][0]["config"]["visConfig"]["colorRange"]
                ["colors"][1],
            "#777777"
        );
        let (_, report) = export_config(&doc, &no_palettes);
        assert!(
            report.lines().any(|l| l.contains("'Teal 3'")),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn malformed_configs_never_panic() {
        let mut inputs: Vec<String> = [
            "",
            "[]",
            "{}",
            "null",
            "1",
            "\"x\"",
            "\u{feff}{}",
            "{\"config\":1}",
            "{\"config\":{}}",
            "{\"version\":\"v1\",\"config\":[]}",
            "{\"version\":\"v9\",\"config\":{}}",
            "{\"version\":2,\"config\":{}}",
            "{\"version\":null,\"config\":{\"version\":\"v1\",\"config\":5}}",
            "{\"version\":\"v1\",\"config\":{\"visState\":1,\"mapState\":[],\"mapStyle\":\"dark\"}}",
            "{\"version\":\"v1\",\"config\":{\"visState\":{\"layers\":1,\"filters\":\"x\",\"interactionConfig\":[],\"splitMaps\":5,\"effects\":{},\"charts\":[1]}}}",
            r#"{"version":"v1","config":{"visState":{"layers":[1,null,"x",{},{"type":"point"},{"type":"point","config":1},{"type":"point","config":{"dataId":5}},{"type":7,"config":{"label":[]}}]}}}"#,
            r#"{"version":"v1","config":{"visState":{"layers":[{"id":5,"type":"point","config":{"dataId":"d","color":"red","columns":[1],"visConfig":[],"textLabel":5,"isVisible":"yes"},"visualChannels":{"colorField":5,"sizeField":{"name":1},"colorScale":[]}}]}}}"#,
            r#"{"version":"v1","config":{"visState":{"layers":[{"type":"point","config":{"dataId":"d","color":[1e308,-1e308,"x"],"visConfig":{"radius":1e308,"opacity":-5,"thickness":"NaN","strokeColor":[1],"colorRange":{"colors":[1,null],"colorMap":5,"name":3},"radiusRange":["a",2]}},"visualChannels":{"colorField":{"name":"m"},"colorScale":"custom","sizeField":{"name":"s"},"sizeScale":"point"}}]}}}"#,
            r##"{"version":"v1","config":{"visState":{"layers":[{"type":"geojson","config":{"dataId":"d","visConfig":{"colorRange":{"colorMap":[[1],[null,"#fff"],"x",[[1,2],5]]}},"textLabel":[{"field":{"name":"n"}},5]},"visualChannels":{"colorField":{"name":"m"},"colorScale":"customOrdinal","radiusField":{"name":"r"},"heightField":{"name":"h"}}}]}}}"##,
            r#"{"version":"v1","config":{"visState":{"filters":[1,{},{"type":"range","dataId":5},{"type":"range","dataId":["a","b"],"name":"x","value":"NaN"},{"type":"timeRange","dataId":"a","name":["x"],"value":[1e308,"Infinity"]},{"type":"multiSelect","dataId":"a","name":"x","value":[{},1,true]},{"type":"select","dataId":["a"],"name":[5],"value":"true","plotType":5,"yAxis":{},"animationWindow":7,"speed":"fast"},{"type":"polygon","id":["x"]}]}}}"#,
            r#"{"version":"v1","config":{"visState":{"interactionConfig":{"tooltip":{"fieldsToShow":{"a":5,"b":[1,{"name":2},{"name":"x","format":7},{"name":"y","format":"%Y"}]},"enabled":"no"},"brush":5,"geocoder":{"enabled":true}}}}}"#,
            r#"{"version":"v1","config":{"visState":{"interactionConfig":{"tooltip":5}},"mapState":{"latitude":"NaN","zoom":1e308,"bearing":1e308,"pitch":-1e308,"mapViewMode":5,"globe":[]},"mapStyle":{"styleType":5,"mapStyles":7,"visibleLayerGroups":[]}}}"#,
            r#"{"version":"v1","config":{"mapStyle":{"styleType":"x","mapStyles":{"x":5}}}}"#,
            r#"{"version":"v1","config":{"mapStyle":{"styleType":"https://example.com/style.json?access_token=abc"}}}"#,
            r#"{"datasets":5,"config":{}}"#,
            r#"{"datasets":[1,{},{"data":{"fields":5,"allData":5}},{"data":{"id":"a","fields":[{"name":1,"type":2}],"allData":[1,[1,2,3],null,[{"a":[1]}]]}},{"data":{"id":"a"}},{"data":{"id":"u","metadata":{"source":5}}}],"config":{}}"#,
            r#"{"version":"v0","config":{"visState":{"layers":[1,{"type":"geojson","sizeField":{"name":"x"},"visConfig":5,"columns":{"geojson":5}},{"type":"point","columns":[],"colorField":5}],"filters":[1,{"enlarged":true}],"interactionConfig":{"tooltip":5,"brush":[]}}}}"#,
            "{\"version\":\"v1\",\"config\":{\"visState\":{\"layers\":[{\"type\":\"point\",\"config\":{\"dataId\":\"\u{0}\",\"label\":\"地図 🗺️ \\u0000\"}}]}}}",
            "{\"a\":\"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[\"}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        inputs.push("[".repeat(MAX_DEPTH + 1));
        inputs.push(format!("{}{}", "{\"a\":".repeat(200), "}".repeat(200)));
        inputs.push("[".repeat(100_000));
        for (_, text) in ALL {
            for k in 1..8 {
                let mut cut = text.len() * k / 8;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                inputs.push(text[..cut].to_string());
            }
        }
        for text in &inputs {
            if let Ok(i) = import(text) {
                // Whatever came in exports and re-imports.
                let (out, _) = export_config(&i.doc, &no_palettes);
                import(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
            }
        }
        assert!(matches!(
            import(&"[".repeat(MAX_DEPTH + 1)),
            Err(KeplerError::TooDeep)
        ));
        assert!(matches!(import(""), Err(KeplerError::NotJson { .. })));
        assert!(matches!(import("[]"), Err(KeplerError::NotKepler)));
        assert!(matches!(
            import("{\"version\":\"v9\",\"config\":{}}"),
            Err(KeplerError::Version(_))
        ));
        let huge = " ".repeat(MAX_BYTES + 1);
        assert!(matches!(
            import(&huge),
            Err(KeplerError::TooLarge { mb: 65 })
        ));
        for e in [
            KeplerError::TooLarge { mb: 70 },
            KeplerError::TooDeep,
            KeplerError::NotJson { line: 1, column: 2 },
            KeplerError::NotKepler,
            KeplerError::Version("v9".into()),
        ] {
            let m = e.to_string();
            assert!(
                m.len() <= 200 && m.ends_with('.') && !m.contains("Value") && !m.contains("serde"),
                "{m}"
            );
        }
    }

    #[test]
    fn a_non_https_data_url_is_rejected() {
        let text = r#"{"datasets":[
            {"version":"v1","data":{"id":"a","label":"Local","fields":[],"allData":[],"metadata":{"source":"file:///etc/passwd"}}},
            {"version":"v1","data":{"id":"b","label":"Script","fields":[],"allData":[],"metadata":{"source":"javascript:alert(1)"}}},
            {"version":"v1","data":{"id":"c","label":"Inline","fields":[],"allData":[],"metadata":{"source":"data:text/csv,a"}}},
            {"version":"v1","data":{"id":"d","label":"Web","fields":[{"name":"x","type":"real"}],"allData":[],"metadata":{"source":"https://example.com/d.csv"}}},
            {"version":"v1","data":{"id":"e","label":"Tokened","fields":[],"allData":[],"metadata":{"source":"https://example.com/e.csv?access_token=secret"}}}
        ],"config":{"version":"v1","config":{}}}"#;
        let i = import(text).unwrap();
        assert_eq!(i.doc.sources.keys().collect::<Vec<_>>(), ["d"]);
        assert_eq!(
            i.doc.sources["d"].data.url.as_deref(),
            Some("https://example.com/d.csv")
        );
        assert_eq!(i.missing, ["a", "b", "c", "e"]);
        for label in ["'Local'", "'Script'", "'Inline'", "'Tokened'"] {
            assert!(
                i.report.lines().any(|l| l.contains(label)),
                "{label}: {}",
                i.report.to_text()
            );
        }
        let text = i.report.to_text();
        assert!(
            !text.contains("passwd") && !text.contains("javascript") && !text.contains("secret"),
            "{text}"
        );
    }

    #[test]
    fn charts_are_a_named_loss_on_export() {
        let mut doc = import(POINTS).unwrap().doc;
        doc.charts.push(crate::doc::Chart::new(
            "chart-1",
            "s",
            crate::doc::ChartKind::Histogram,
            "mag",
        ));
        let (text, report) = export_config(&doc, &no_palettes);
        assert!(
            report.dropped.iter().any(|l| l.contains("chart 'chart-1'")),
            "{:?}",
            report.dropped
        );
        assert!(!text.contains("chart-1"));
    }

    #[test]
    fn no_report_line_says_kepler() {
        let check = |r: &Report, name: &str| {
            for l in r.lines() {
                assert!(
                    !l.replace("Kepler.gl config", "")
                        .to_lowercase()
                        .contains("kepler"),
                    "{name}: {l}"
                );
            }
        };
        for (name, text) in ALL {
            let i = import(text).unwrap();
            check(&i.report, name);
            check(&export_config(&i.doc, &no_palettes).1, name);
            check(&export_map(&i.doc, &|_: &str| None, &no_palettes).1, name);
        }
    }

    #[test]
    fn export_never_writes_a_token() {
        let mut doc = import(LOSS).unwrap().doc;
        // Fake, but token-shaped: `pk.` and longer than 20 characters.
        let token = "pk.FAKE_TEST_TOKEN_kepler_export";
        let l = &mut doc.map.layers[0];
        l.vis
            .extra
            .insert("accessToken".into(), Value::String(token.into()));
        l.vis.extra.insert(
            "tiles".into(),
            Value::String("https://tiles.example.com/{z}/{x}/{y}.pbf?access_token=abc".into()),
        );
        l.extra.insert(
            "kepler".into(),
            serde_json::json!({ "config": { "token": token, "note": token } }),
        );
        doc.filters.push(crate::doc::Filter::new(
            "f",
            "quakes",
            "Magnitude",
            FilterKind::Range,
            FilterValue::All,
        ));
        doc.filters[0]
            .extra
            .insert("access_token".into(), Value::String("abc".into()));
        if let Some(items) = doc
            .explorer
            .get_mut("unsupported_layers")
            .and_then(Value::as_array_mut)
        {
            items[0]["layer"]["config"]["visConfig"]["accessToken"] = Value::String(token.into());
        }
        doc.map.view.basemap = "dark".into();
        for (out, report) in [
            export_config(&doc, &no_palettes),
            export_map(&doc, &|_: &str| None, &no_palettes),
        ] {
            for bad in ["access_token", "accessToken", token, "pk.", "mapbox://"] {
                assert!(!out.contains(bad), "{bad} in {out}");
            }
            assert!(
                report.lines().any(|l| l.contains("access token")),
                "{}",
                report.to_text()
            );
        }
    }
}
