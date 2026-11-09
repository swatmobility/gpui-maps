//! [`Doc`] -> Kepler.gl v1 JSON (spec §7.3).

use serde_json::{json, Map, Number, Value};

use super::{strip_tokens, Report};
use crate::doc::{ChannelBinding, Doc, Layer, LayerType};
use crate::filters::FilterValue;
use crate::table::{Column, FieldType, Geometry, Table};
use crate::tooltip::Format;

/// The colours of Kepler.gl's default colour range (Global Warming, six
/// steps), written when a palette cannot be resolved.
pub(crate) const DEFAULT_COLORS: [&str; 6] = [
    "#5A1846", "#900C3F", "#C70039", "#E3611C", "#F1920E", "#FFC300",
];

/// Kepler.gl's default colour range.
pub(crate) fn default_color_range() -> Value {
    json!({ "name": "Global Warming", "type": "sequential", "category": "Uber", "colors": DEFAULT_COLORS })
}

/// Kepler.gl's default text label list (one label, no field).
pub(crate) fn default_text_label() -> Value {
    json!([{
        "field": null, "color": [255, 255, 255], "size": 18, "offset": [0, 0], "anchor": "start", "alignment": "center",
        "outlineWidth": 0, "outlineColor": [255, 0, 0, 255], "background": false, "backgroundColor": [0, 0, 200, 255]
    }])
}

/// The `visConfig` keys the exporter fills when the document has none,
/// besides the ones the document models. An import drops a key equal to
/// its default here, so a round trip adds nothing.
pub(crate) fn default_vis_config(kind: LayerType) -> Map<String, Value> {
    let v = match kind {
        LayerType::Points => json!({
            "fixedRadius": false, "colorRange": default_color_range(), "strokeColorRange": default_color_range(),
            "radiusRange": [0, 50]
        }),
        LayerType::GeoJson => json!({
            "strokeOpacity": 0.8, "colorRange": default_color_range(), "strokeColorRange": default_color_range(),
            "radiusRange": [0, 50], "sizeRange": [0, 10], "heightRange": [0, 500], "elevationScale": 5,
            "enableElevationZoomFactor": true, "enable3d": false, "wireframe": false
        }),
        LayerType::Hexbin | LayerType::Grid => json!({
            "colorRange": default_color_range(), "worldUnitSize": 1, "resolution": 8, "coverage": 1, "sizeRange": [0, 500],
            "percentile": [0, 100], "elevationPercentile": [0, 100], "elevationScale": 5, "enableElevationZoomFactor": true,
            "fixedHeight": false, "colorAggregation": "count", "sizeAggregation": "count", "enable3d": false
        }),
        LayerType::Arc | LayerType::Line | LayerType::Flow | LayerType::HexDemand => {
            json!({ "colorRange": default_color_range(), "sizeRange": [0, 10], "targetColor": null })
        }
        LayerType::Cluster => {
            json!({ "clusterRadius": 40, "colorRange": default_color_range(), "radiusRange": [1, 40], "colorAggregation": "count" })
        }
        LayerType::H3 => json!({
            "colorRange": default_color_range(), "coverage": 1, "enable3d": false, "sizeRange": [0, 500], "coverageRange": [0, 1],
            "elevationScale": 5, "enableElevationZoomFactor": true, "filled": true
        }),
        LayerType::Heatmap => {
            json!({ "colorRange": default_color_range(), "intensity": 1, "threshold": 0.18 })
        }
        LayerType::Trip => {
            json!({ "colorRange": default_color_range(), "trailLength": 180, "sizeRange": [0, 10] })
        }
    };
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// The visual channels of a layer type, unbound.
pub(crate) fn default_channels(kind: LayerType) -> Map<String, Value> {
    let v = match kind {
        LayerType::Points => json!({
            "colorField": null, "colorScale": "quantile", "strokeColorField": null, "strokeColorScale": "quantile",
            "sizeField": null, "sizeScale": "linear"
        }),
        LayerType::GeoJson => json!({
            "colorField": null, "colorScale": "quantile", "strokeColorField": null, "strokeColorScale": "quantile",
            "sizeField": null, "sizeScale": "linear", "heightField": null, "heightScale": "linear",
            "radiusField": null, "radiusScale": "linear"
        }),
        LayerType::Hexbin | LayerType::Grid | LayerType::Cluster => json!({
            "colorField": null, "colorScale": "quantize", "sizeField": null, "sizeScale": "linear"
        }),
        LayerType::Arc | LayerType::Line | LayerType::Flow | LayerType::HexDemand => json!({
            "colorField": null, "colorScale": "quantile", "sizeField": null, "sizeScale": "linear"
        }),
        LayerType::H3 => json!({
            "colorField": null, "colorScale": "quantile", "sizeField": null, "sizeScale": "linear", "coverageField": null, "coverageScale": "linear"
        }),
        LayerType::Heatmap => json!({ "weightField": null, "weightScale": "linear" }),
        LayerType::Trip => {
            json!({ "colorField": null, "colorScale": "quantile", "sizeField": null, "sizeScale": "linear" })
        }
    };
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Resolves a palette name (and whether it is reversed) to hex colours.
pub type Palettes<'a> = &'a dyn Fn(&str, bool) -> Option<Vec<String>>;

/// The document as a Kepler.gl v1 config, and what it could not write.
pub fn export_config(
    doc: &Doc,
    palettes: &dyn Fn(&str, bool) -> Option<Vec<String>>,
) -> (String, Report) {
    let mut report = Report::default();
    let config = config_value(doc, palettes, &mut report);
    (pretty(json!({ "version": "v1", "config": config })), report)
}

/// The document as a Kepler.gl map with its rows: `data(source_id)` gives
/// a loaded table. A source with no table, or whose shapes the table does
/// not hold, is written config only and reported.
pub fn export_map<'t>(
    doc: &Doc,
    data: &dyn Fn(&str) -> Option<&'t Table>,
    palettes: &dyn Fn(&str, bool) -> Option<Vec<String>>,
) -> (String, Report) {
    let mut report = Report::default();
    let config = config_value(doc, palettes, &mut report);
    let mut datasets = Vec::new();
    for (id, source) in &doc.sources {
        let label = if source.label.is_empty() {
            id.as_str()
        } else {
            source.label.as_str()
        };
        let Some(table) = data(id) else {
            report.drop(format!(
                "dropped: the rows of dataset '{label}' (not loaded; written as config only)"
            ));
            continue;
        };
        if matches!(table.geometry, Geometry::Features { .. })
            && !table.fields.iter().any(|f| f.ty == FieldType::GeoJson)
        {
            report.drop(format!("dropped: the rows of dataset '{label}' (its shapes are not in the table; written as config only)"));
            continue;
        }
        let color = doc
            .layers_of(id)
            .next()
            .map_or([46, 134, 171], |l| l.vis.color);
        datasets.push(json!({ "version": "v1", "data": dataset(id, label, color, table) }));
    }
    let info = json!({ "app": "kepler.gl", "title": doc.title, "description": "" });
    let out = json!({ "datasets": datasets, "config": { "version": "v1", "config": config }, "info": info });
    (pretty(out), report)
}

fn dataset(id: &str, label: &str, color: [u8; 3], table: &Table) -> Value {
    let fields: Vec<Value> = table
        .fields
        .iter()
        .map(|f| {
            let analyzer = match f.ty {
                FieldType::Boolean => "BOOLEAN",
                FieldType::Integer => "INT",
                FieldType::Real => "FLOAT",
                FieldType::String => "STRING",
                FieldType::Timestamp => "DATETIME",
                FieldType::Date => "DATE",
                FieldType::GeoJson | FieldType::Point => "GEOMETRY",
            };
            json!({ "name": f.name, "type": f.ty.name(), "format": "", "analyzerType": analyzer })
        })
        .collect();
    let rows: Vec<Value> = (0..table.rows)
        .map(|i| {
            Value::Array(
                table
                    .fields
                    .iter()
                    .map(|f| cell(&f.column, f.ty, i))
                    .collect(),
            )
        })
        .collect();
    json!({ "id": id, "label": label, "color": color, "fields": fields, "allData": rows })
}

fn cell(column: &Column, ty: FieldType, i: usize) -> Value {
    let number = |x: Option<f64>| match x {
        Some(x) if ty == FieldType::Integer && x.fract() == 0.0 && x.abs() < 9.0e15 => {
            Value::Number((x as i64).into())
        }
        Some(x) => Number::from_f64(x).map_or(Value::Null, Value::Number),
        None => Value::Null,
    };
    match column {
        Column::Number(v) => number(v.get(i).copied().flatten()),
        Column::Bool(v) => v.get(i).copied().flatten().map_or(Value::Null, Value::Bool),
        Column::Str { .. } => {
            let s = column.str_at(i);
            match (ty, s) {
                // A geojson cell holds a geometry object.
                (FieldType::GeoJson, Some(s)) => serde_json::from_str::<Value>(s)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| Value::String(s.to_string())),
                (_, Some(s)) => Value::String(s.to_string()),
                (_, None) => Value::Null,
            }
        }
        Column::Time { ms, raw } => match raw {
            Some(raw) => raw.text(i).map_or(Value::Null, Value::String),
            None => number(ms.get(i).copied().flatten()),
        },
    }
}

/// `{visState, mapState, mapStyle}`.
fn config_value(doc: &Doc, palettes: Palettes, report: &mut Report) -> Value {
    let explorer = doc.explorer.as_object();
    if let Some(e) = explorer {
        let own: Vec<&str> = e
            .keys()
            .map(String::as_str)
            .filter(|k| {
                !matches!(
                    *k,
                    "unsupported_layers" | "unsupported_filters" | "layer_blending" | "field_types"
                )
            })
            .collect();
        if !own.is_empty() {
            report.drop(format!(
                "dropped: the Explorer's own settings ({})",
                own.join(", ")
            ));
        }
    }
    // Kepler.gl has no floating chart cards (its histograms live in the
    // filters, which are written).
    for c in &doc.charts {
        report.drop(format!(
            "dropped: chart '{}' (a {} of {}; the format has no chart cards)",
            c.id,
            c.kind.name(),
            c.field
        ));
    }
    let kept = |key: &str, inner: &str| -> Vec<(usize, Value)> {
        let mut items: Vec<(usize, Value)> = explorer
            .and_then(|e| e.get(key))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|i| {
                        Some((
                            i.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize,
                            i.get(inner)?.clone(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        items.sort_by_key(|(i, _)| *i);
        items
    };

    // Layers, top first.
    let mut layers: Vec<Value> = doc
        .map
        .layers
        .iter()
        .rev()
        .map(|l| layer_value(doc, l, palettes, report))
        .collect();
    for (index, layer) in kept("unsupported_layers", "layer") {
        layers.insert(index.min(layers.len()), layer);
    }
    let layer_order: Vec<Value> = layers.iter().filter_map(|l| l.get("id").cloned()).collect();

    let mut filters: Vec<Value> = doc
        .filters
        .iter()
        .map(|f| {
            let mut out = f.extra.clone();
            let value = match &f.value {
                FilterValue::Range(r) => json!(r),
                FilterValue::Bool(b) => json!(b),
                FilterValue::Values(v) => json!(v),
                // Kepler has no substring filter: it opens showing everything.
                FilterValue::Contains(_) | FilterValue::All => Value::Null,
            };
            for (k, v) in [
                ("dataId", json!([f.source])),
                ("id", json!(f.id)),
                ("name", json!([f.field])),
                ("type", serde_json::to_value(f.kind).unwrap_or(Value::Null)),
                ("value", value),
                ("enabled", json!(f.enabled)),
                ("plotType", json!({ "type": "histogram" })),
                ("animationWindow", json!("free")),
                ("yAxis", Value::Null),
            ] {
                out.insert(k.into(), v);
            }
            // Kepler.gl plays at 1x when no speed is saved.
            if let Some(speed) = f.speed {
                out.insert("speed".into(), json!(speed));
            }
            Value::Object(out)
        })
        .collect();
    for (index, filter) in kept("unsupported_filters", "filter") {
        filters.insert(index.min(filters.len()), filter);
    }

    let blending = explorer
        .and_then(|e| e.get("layer_blending"))
        .and_then(Value::as_str)
        .unwrap_or("normal");

    let mut vis_state = json!({
        "filters": filters,
        "layers": layers,
        "layerOrder": layer_order,
        "interactionConfig": interaction(doc, report),
        "layerBlending": blending,
        "overlayBlending": "normal",
        "splitMaps": [],
        "animationConfig": { "currentTime": null, "speed": 1 },
        "editor": { "features": [], "visible": true },
    });
    if strip_tokens(&mut vis_state) {
        report.drop("dropped: an access token found in the layers or filters");
    }

    let v = &doc.map.view;
    let globe = v.projection == "globe";
    if globe {
        report.approx("approximated: globe projection (older versions open it flat)");
    }
    let mode = if globe {
        "MODE_GLOBE"
    } else if v.pitch > 0.0 {
        "MODE_3D"
    } else {
        "MODE_2D"
    };
    let map_state = json!({
        "bearing": v.bearing, "dragRotate": v.pitch > 0.0 || v.bearing != 0.0, "latitude": v.lat, "longitude": v.lon,
        "pitch": v.pitch, "zoom": v.zoom, "isSplit": false, "mapViewMode": mode
    });
    let style = match v.basemap.as_str() {
        "dark"
        | "swat-routes-dark"
        | "mapbox-dark"
        | "mapbox-standard-dark"
        | "mapbox-swat-routes-dark" => "dark",
        "light" | "mapbox-light" => "light",
        "streets" | "open-streets" | "liberty" | "mapbox-streets" => "muted",
        other => {
            report.approx(format!(
                "approximated: basemap '{}' written as the muted style",
                other.chars().take(32).collect::<String>()
            ));
            "muted"
        }
    };
    let map_style = json!({
        "styleType": style,
        "topLayerGroups": {},
        "visibleLayerGroups": { "label": true, "road": true, "border": false, "building": true, "water": true, "land": true, "3d building": false },
        "threeDBuildingColor": [9, 17, 31],
        "mapStyles": {}
    });
    json!({ "visState": vis_state, "mapState": map_state, "mapStyle": map_style })
}

fn layer_name(l: &Layer) -> &str {
    if l.label.is_empty() {
        &l.id
    } else {
        &l.label
    }
}

fn layer_value(doc: &Doc, l: &Layer, palettes: Palettes, report: &mut Report) -> Value {
    let name = layer_name(l);
    let kepler = l.extra.get("kepler");
    let mut config = kepler
        .and_then(|k| k.get("config"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let source = doc.sources.get(&l.source);
    let geometry = source.map(|s| &s.geometry);
    let geom = |key: &str| {
        geometry
            .and_then(|g| g.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    let mut columns = Map::new();
    match l.kind {
        k if k.reads_points() => {
            let lat = l
                .columns
                .get("lat")
                .cloned()
                .or_else(|| geom("lat"))
                .unwrap_or_else(|| "lat".into());
            let lon = l
                .columns
                .get("lon")
                .cloned()
                .or_else(|| geom("lon"))
                .unwrap_or_else(|| "lon".into());
            columns.insert("lat".into(), json!(lat));
            columns.insert("lng".into(), json!(lon));
            for (k, v) in l
                .columns
                .iter()
                .filter(|(k, _)| !matches!(k.as_str(), "lat" | "lon"))
            {
                columns.insert(k.clone(), json!(v));
            }
        }
        LayerType::Arc | LayerType::Line | LayerType::Flow | LayerType::HexDemand => {
            for (ours, theirs) in [
                ("lat0", "lat0"),
                ("lon0", "lng0"),
                ("lat1", "lat1"),
                ("lon1", "lng1"),
            ] {
                columns.insert(
                    theirs.into(),
                    l.columns.get(ours).map_or(Value::Null, |c| json!(c)),
                );
            }
        }
        LayerType::H3 => {
            columns.insert(
                "hex_id".into(),
                l.columns.get("hex_id").map_or(Value::Null, |c| json!(c)),
            );
        }
        // A trip from rows: Kepler's table columns (`lng`, not `lon`).
        LayerType::Trip if l.columns.contains_key("timestamp") => {
            for (k, v) in &l.columns {
                columns.insert(if k == "lon" { "lng".into() } else { k.clone() }, json!(v));
            }
        }
        _ => {
            let g = l
                .columns
                .get("geojson")
                .cloned()
                .or_else(|| geom("geojson"))
                .unwrap_or_else(|| "_geojson".into());
            columns.insert("geojson".into(), json!(g));
            for (k, v) in l.columns.iter().filter(|(k, _)| k.as_str() != "geojson") {
                columns.insert(k.clone(), json!(v));
            }
        }
    }

    let mut vis = default_vis_config(l.kind);
    for (k, v) in &l.vis.extra {
        vis.insert(k.clone(), v.clone());
    }
    vis.insert("radius".into(), json!(l.vis.radius));
    vis.insert("opacity".into(), json!(l.vis.opacity));
    vis.insert("filled".into(), json!(l.vis.filled));
    vis.insert(
        if l.kind == LayerType::Points {
            "outline"
        } else {
            "stroked"
        }
        .into(),
        json!(l.vis.outline),
    );
    vis.insert("thickness".into(), json!(l.vis.thickness));
    vis.insert(
        "strokeColor".into(),
        l.vis.stroke_color.map_or(Value::Null, |c| json!(c)),
    );
    let v = &l.vis;
    if l.kind.is_binned() {
        vis.insert("worldUnitSize".into(), json!(v.cell_km()));
        vis.insert("coverage".into(), json!(v.coverage()));
        vis.insert("percentile".into(), json!(v.percentile()));
        let agg = if l.color.as_ref().is_some_and(|b| !b.field.is_empty()) {
            v.aggregation()
        } else {
            "count"
        };
        vis.insert("colorAggregation".into(), json!(agg));
        vis.insert("sizeAggregation".into(), json!(agg));
    }
    if matches!(
        l.kind,
        LayerType::Hexbin | LayerType::Grid | LayerType::GeoJson | LayerType::H3
    ) {
        vis.insert("enable3d".into(), json!(v.enable3d));
        vis.insert("elevationScale".into(), json!(v.elevation_scale()));
    }
    if l.kind.is_od() {
        vis.insert(
            "targetColor".into(),
            json!(v.target_color.unwrap_or(v.color)),
        );
    }
    if l.kind == LayerType::Cluster {
        vis.insert("clusterRadius".into(), json!(v.cluster_radius()));
    }

    let mut channels = default_channels(l.kind);
    if let Some(kept) = kepler
        .and_then(|k| k.get("visualChannels"))
        .and_then(Value::as_object)
    {
        for (k, v) in kept {
            channels.insert(k.clone(), v.clone());
        }
    }
    let field_type = |field: &str, fallback: &str| {
        source
            .and_then(|s| s.columns.get(field))
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    };
    if let Some(b) = l.color.as_ref().filter(|b| b.field.is_empty()) {
        // A binned or cluster layer's count: no field, its colour range.
        vis.insert("colorRange".into(), color_range(b, name, palettes, report));
        channels.insert("colorScale".into(), json!(b.scale));
    }
    if let Some(b) = l.color.as_ref().filter(|b| !b.field.is_empty()) {
        let fallback = if matches!(b.scale.as_str(), "ordinal" | "customOrdinal") {
            "string"
        } else {
            "real"
        };
        channels.insert(
            "colorField".into(),
            json!({ "name": b.field, "type": field_type(&b.field, fallback) }),
        );
        channels.insert("colorScale".into(), json!(b.scale));
        vis.insert("colorRange".into(), color_range(b, name, palettes, report));
    }
    if let Some(b) = &l.radius {
        let (field_key, scale_key) = match l.kind {
            LayerType::GeoJson => ("radiusField", "radiusScale"),
            LayerType::Heatmap => ("weightField", "weightScale"),
            _ => ("sizeField", "sizeScale"),
        };
        channels.insert(
            field_key.into(),
            json!({ "name": b.field, "type": field_type(&b.field, "real") }),
        );
        channels.insert(scale_key.into(), json!(b.scale));
        if let Some(r) = b.range {
            vis.insert("radiusRange".into(), json!(r));
        }
    }

    if let Some(b) = &l.height {
        channels.insert(
            "heightField".into(),
            json!({ "name": b.field, "type": field_type(&b.field, "real") }),
        );
        channels.insert("heightScale".into(), json!(b.scale));
        if let Some(r) = b.range {
            vis.insert("heightRange".into(), json!(r));
        }
    }
    config.insert(
        "highlightColor".into(),
        json!(l
            .hover
            .highlight_color
            .map_or([252, 242, 26, 255], |c| [c[0], c[1], c[2], 255])),
    );
    config.insert("dataId".into(), json!(l.source));
    config.insert("label".into(), json!(l.label));
    config.insert("color".into(), json!(l.vis.color));
    config.insert("columns".into(), Value::Object(columns));
    config.insert("isVisible".into(), json!(l.visible));
    config.insert("visConfig".into(), Value::Object(vis));
    config.entry("hidden").or_insert(Value::Bool(false));
    config.entry("textLabel").or_insert_with(default_text_label);

    // A flow layer is not a Kepler type: it goes out as the arc it reads.
    let ty = if l.kind.is_flow() {
        "arc"
    } else {
        l.kind.kepler_name()
    };
    json!({ "id": l.id, "type": ty, "config": config, "visualChannels": channels })
}

fn color_range(b: &ChannelBinding, name: &str, palettes: Palettes, report: &mut Report) -> Value {
    let mut colors: Vec<String> = b
        .extra
        .get("colors")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if colors.is_empty() {
        if let Some(p) = &b.palette {
            colors = palettes(p, b.reversed).unwrap_or_default();
        }
    }
    if colors.is_empty() {
        let palette = b.palette.as_deref().unwrap_or("unnamed");
        report.approx(format!(
            "approximated: palette '{palette}' on layer '{name}' written as the default colours"
        ));
        colors = DEFAULT_COLORS.iter().map(|c| c.to_string()).collect();
    }
    let qualitative = matches!(b.scale.as_str(), "ordinal" | "customOrdinal");
    let ty = b
        .extra
        .get("palette_type")
        .and_then(Value::as_str)
        .unwrap_or(if qualitative {
            "qualitative"
        } else {
            "sequential"
        });
    let category = b
        .extra
        .get("palette_category")
        .and_then(Value::as_str)
        .unwrap_or(if b.palette.is_some() {
            "Uber"
        } else {
            "Custom"
        });
    let mut range = json!({
        "name": b.palette.clone().unwrap_or_else(|| "Custom Palette".into()),
        "type": ty,
        "category": category,
        "colors": colors,
    });
    if b.reversed {
        range["reversed"] = Value::Bool(true);
    }
    let keys = match b.scale.as_str() {
        "custom" => b.extra.get("breaks"),
        "customOrdinal" => b.extra.get("categories"),
        _ => None,
    };
    if let Some(keys) = keys.and_then(Value::as_array) {
        let map: Vec<Value> = keys
            .iter()
            .zip(&colors)
            .map(|(k, c)| json!([k, c]))
            .collect();
        range["colorMap"] = Value::Array(map);
    }
    range
}

/// `interactionConfig`, every key present; one tooltip card per dataset.
fn interaction(doc: &Doc, report: &mut Report) -> Value {
    let mut fields = Map::new();
    let mut card_owner: Vec<(&str, &Layer)> = Vec::new();
    for l in &doc.map.layers {
        if let Some((_, first)) = card_owner.iter().find(|(s, _)| *s == l.source) {
            if first.hover.card != l.hover.card {
                report.approx(format!("approximated: the hover card of layer '{}' written as that of layer '{}' (one card per dataset)", layer_name(l), layer_name(first)));
            }
            continue;
        }
        card_owner.push((l.source.as_str(), l));
        let card: Vec<Value> = l
            .hover
            .card
            .iter()
            .map(|h| {
                let format = h.format.as_deref().filter(|f| {
                    let ok = Format::parse(Some(f)) != Format::None;
                    if !ok {
                        report.approx(format!(
                            "approximated: the format of '{}' on layer '{}' written as the default",
                            h.field,
                            layer_name(l)
                        ));
                    }
                    ok
                });
                json!({ "name": h.field, "format": format })
            })
            .collect();
        fields.insert(l.source.clone(), Value::Array(card));
    }
    let enabled = !doc
        .map
        .layers
        .iter()
        .any(|l| l.hover.extra.get("enabled").and_then(Value::as_bool) == Some(false));
    json!({
        "tooltip": { "fieldsToShow": fields, "compareMode": false, "compareType": "absolute", "enabled": enabled },
        "brush": { "size": 0.5, "enabled": false },
        "geocoder": { "enabled": false },
        "coordinate": { "enabled": false }
    })
}

/// Pretty JSON, every object's keys sorted, with a final newline.
fn pretty(value: Value) -> String {
    fn sorted(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut entries: Vec<(String, Value)> = map.into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
            }
            Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
            other => other,
        }
    }
    let mut out = serde_json::to_string_pretty(&sorted(value)).unwrap_or_default();
    out.push('\n');
    out
}
