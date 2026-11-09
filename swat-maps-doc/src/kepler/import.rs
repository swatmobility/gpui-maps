//! Kepler.gl JSON -> [`Doc`] (spec §7.1, §7.2).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use super::{
    nests_deeper_than, strip_tokens, Imported, KeplerError, Report, MAX_BYTES, MAX_DEPTH,
    MAX_FILTERS, MAX_LAYERS,
};
use crate::doc::{
    ChannelBinding, DataRef, Doc, Filter, Hover, HoverField, Layer, LayerType, Source, View, Vis,
};
use crate::filters::{FilterKind, FilterValue};
use crate::tooltip::Format;

/// Kepler.gl's default `highlightColor` (its yellow): read as the
/// Studio's own default.
pub(crate) const KEPLER_HIGHLIGHT: [u8; 3] = [252, 242, 26];

/// The size range Kepler.gl gives a point radius when none is saved.
pub(crate) const KEPLER_RADIUS_RANGE: [f64; 2] = [0.0, 50.0];

/// Reads a Kepler.gl config or map export.
pub fn import(text: &str) -> Result<Imported, KeplerError> {
    if text.len() > MAX_BYTES {
        return Err(KeplerError::TooLarge {
            mb: text.len().div_ceil(1024 * 1024),
        });
    }
    if nests_deeper_than(text, MAX_DEPTH) {
        return Err(KeplerError::TooDeep);
    }
    let value: Value = serde_json::from_str(text).map_err(|e| KeplerError::NotJson {
        line: e.line(),
        column: e.column(),
    })?;
    import_value(&value)
}

fn import_value(top: &Value) -> Result<Imported, KeplerError> {
    let top_obj = top.as_object().ok_or(KeplerError::NotKepler)?;
    let config_v = top_obj
        .get("config")
        .and_then(Value::as_object)
        .ok_or(KeplerError::NotKepler)?;
    // A map export nests `{version, config}` under `config`; a config is
    // `{version, config}` itself; datasets beside an unversioned config
    // read as v1.
    let (version, config) = if config_v.contains_key("version")
        && config_v.get("config").is_some_and(Value::is_object)
    {
        (
            config_v.get("version"),
            config_v.get("config").and_then(Value::as_object),
        )
    } else if top_obj.contains_key("version") {
        (top_obj.get("version"), Some(config_v))
    } else if top_obj.get("datasets").is_some_and(Value::is_array) {
        (None, Some(config_v))
    } else {
        return Err(KeplerError::NotKepler);
    };
    let version = match version {
        None => "v1".to_string(),
        Some(Value::String(v)) => v.clone(),
        Some(other) => return Err(KeplerError::Version(short(&other.to_string()))),
    };
    if version != "v0" && version != "v1" {
        return Err(KeplerError::Version(short(&version)));
    }
    let empty = Map::new();
    let config = config.unwrap_or(&empty);

    let mut ctx = Ctx::default();
    let mut doc = Doc::default();
    if let Some(title) = top_obj
        .get("info")
        .and_then(|i| i.get("title"))
        .and_then(Value::as_str)
    {
        doc.title = title.to_string();
    }

    let vis_state = match config.get("visState") {
        Some(Value::Object(v)) => {
            if version == "v0" {
                migrate_v0(v)
            } else {
                v.clone()
            }
        }
        Some(Value::Null) | None => Map::new(),
        Some(_) => {
            ctx.report
                .drop("dropped: the layers, filters and tooltips (their block is unreadable)");
            Map::new()
        }
    };

    ctx.datasets(top_obj.get("datasets"), &mut doc);
    ctx.layers(
        vis_state.get("layers"),
        vis_state.get("layerOrder"),
        &mut doc,
    );
    ctx.filters(vis_state.get("filters"), &mut doc);
    ctx.interaction(vis_state.get("interactionConfig"), &mut doc);
    ctx.vis_state_rest(&vis_state);
    ctx.map_state(config.get("mapState"), &mut doc.map.view);
    ctx.map_style(config.get("mapStyle"), &mut doc.map.view);

    // The geometry of each embedded source, from the layers that draw it.
    for (id, source) in doc.sources.iter_mut() {
        if !source.geometry.is_null() {
            continue;
        }
        if let Some(layer) = doc.map.layers.iter().find(|l| &l.source == id) {
            source.geometry = match layer.kind {
                k if k.reads_points() => match (layer.columns.get("lat"), layer.columns.get("lon"))
                {
                    (Some(lat), Some(lon)) => json!({ "lat": lat, "lon": lon }),
                    _ => Value::Null,
                },
                LayerType::Arc | LayerType::Line => {
                    match (layer.columns.get("lat0"), layer.columns.get("lon0")) {
                        (Some(lat), Some(lon)) => json!({ "lat": lat, "lon": lon }),
                        _ => Value::Null,
                    }
                }
                LayerType::GeoJson => layer
                    .columns
                    .get("geojson")
                    .map_or(Value::Null, |g| json!({ "geojson": g })),
                _ => Value::Null,
            };
        }
        if source.geometry.is_null() {
            if let Some((name, _)) = source.columns.iter().find(|(_, t)| t.as_str() == "geojson") {
                source.geometry = json!({ "geojson": name });
            }
        }
    }

    let mut explorer = Map::new();
    if !ctx.unsupported_layers.is_empty() {
        explorer.insert(
            "unsupported_layers".into(),
            Value::Array(std::mem::take(&mut ctx.unsupported_layers)),
        );
    }
    if !ctx.unsupported_filters.is_empty() {
        explorer.insert(
            "unsupported_filters".into(),
            Value::Array(std::mem::take(&mut ctx.unsupported_filters)),
        );
    }
    if let Some(blend) = ctx.blending.take() {
        explorer.insert("layer_blending".into(), Value::String(blend));
    }
    if !explorer.is_empty() {
        doc.explorer = Value::Object(explorer);
    }

    let mut missing: BTreeSet<String> = ctx
        .referenced
        .iter()
        .filter(|id| !doc.sources.contains_key(*id))
        .cloned()
        .collect();
    missing.extend(ctx.unloaded);
    doc.place_filters();
    Ok(Imported {
        doc,
        report: ctx.report,
        missing: missing.into_iter().collect(),
    })
}

/// A layer's sort key from Kepler.gl's `visState.layerOrder`, top first:
/// kepler.gl 3 writes layer ids, 2.x the indices into `layers`. A layer it
/// does not name (or a file without one) ranks after every named layer, in
/// its `layers` position.
fn layer_order_rank(order: Option<&Value>) -> impl Fn(usize, Option<&str>) -> usize {
    let list: Vec<Value> = order.and_then(Value::as_array).cloned().unwrap_or_default();
    move |index, id| {
        let at = list.iter().position(|v| match v {
            Value::String(s) => Some(s.as_str()) == id,
            Value::Number(n) => n.as_u64() == Some(index as u64),
            _ => false,
        });
        at.unwrap_or(list.len() + index)
    }
}

fn short(s: &str) -> String {
    s.chars().take(16).collect()
}

#[derive(Default)]
struct Ctx {
    report: Report,
    unsupported_layers: Vec<Value>,
    unsupported_filters: Vec<Value>,
    blending: Option<String>,
    /// Every dataId a layer, filter or tooltip names.
    referenced: BTreeSet<String>,
    /// Datasets the file names but whose data was refused.
    unloaded: BTreeSet<String>,
}

/// A layer's name in a report line: its label, else its id.
fn layer_name(layer: &Value) -> String {
    let config = layer.get("config");
    let label = config
        .and_then(|c| c.get("label"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let id = layer.get("id").and_then(Value::as_str);
    label.or(id).unwrap_or("unnamed").to_string()
}

pub(crate) fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}

pub(crate) fn rgb(v: Option<&Value>) -> Option<[u8; 3]> {
    let a = v?.as_array()?;
    if a.len() < 3 {
        return None;
    }
    let mut out = [0u8; 3];
    for (o, x) in out.iter_mut().zip(a) {
        let x = x.as_f64().filter(|x| x.is_finite())?;
        *o = x.round().clamp(0.0, 255.0) as u8;
    }
    Some(out)
}

/// A dataId as one id or a list of them.
fn data_ids(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// The scales a colour binding keeps as they are.
const COLOR_SCALES: [&str; 4] = ["quantile", "quantize", "ordinal", "linear"];
const SIZE_SCALES: [&str; 3] = ["linear", "sqrt", "log"];

impl Ctx {
    fn datasets(&mut self, datasets: Option<&Value>, doc: &mut Doc) {
        let Some(list) = datasets else { return };
        let Some(list) = list.as_array() else {
            self.report
                .drop("dropped: the datasets (their block is unreadable)");
            return;
        };
        for (i, entry) in list.iter().enumerate() {
            // `{version, data: {...}}` (v1) or the data itself.
            let data = match entry.get("data") {
                Some(d) if d.is_object() => d,
                _ => entry,
            };
            let Some(data) = data.as_object() else {
                self.report
                    .drop(format!("dropped: dataset {} (unreadable)", i + 1));
                continue;
            };
            let id = data
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map_or_else(|| format!("dataset-{}", i + 1), str::to_string);
            let label = data
                .get("label")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&id)
                .to_string();
            if doc.sources.contains_key(&id) {
                self.report.drop(format!(
                    "dropped: dataset '{label}' (its id '{id}' repeats an earlier one)"
                ));
                continue;
            }
            let fields: Vec<(String, String)> = match data.get("fields").and_then(Value::as_array) {
                Some(fields) => fields
                    .iter()
                    .enumerate()
                    .map(|(j, f)| {
                        let name = f.get("name").and_then(Value::as_str).map_or_else(|| format!("field_{}", j + 1), str::to_string);
                        let ty = f.get("type").and_then(Value::as_str).unwrap_or("");
                        let ours = match ty {
                            "boolean" | "integer" | "real" | "string" | "timestamp" | "date" | "geojson" | "point" => ty.to_string(),
                            "geoarrow" => "geojson".to_string(),
                            "h3" => "string".to_string(),
                            "array" | "object" => {
                                self.report.approx(format!("approximated: {ty} field '{name}' of dataset '{label}' read as text"));
                                "string".to_string()
                            }
                            _ => {
                                self.report.approx(format!("approximated: field '{name}' of dataset '{label}' has an unknown type, read as text"));
                                "string".to_string()
                            }
                        };
                        (name, ours)
                    })
                    .collect(),
                None => Vec::new(),
            };
            let rows = data.get("allData").and_then(Value::as_array);
            let remote = data
                .get("metadata")
                .and_then(|m| m.get("source"))
                .and_then(Value::as_str);
            let data_ref = if rows.is_some_and(|r| !r.is_empty()) || remote.is_none() {
                let (csv, bad) = write_csv(&fields, rows.map(Vec::as_slice).unwrap_or(&[]));
                if bad > 0 {
                    self.report.drop(format!(
                        "dropped: {bad} unreadable rows of dataset '{label}'"
                    ));
                }
                DataRef::embedded_text(format!("{label}.csv"), csv)
            } else {
                let url = remote.unwrap_or("");
                let lower = url.to_ascii_lowercase();
                if lower.starts_with("https://") || lower.starts_with("http://") {
                    if super::looks_like_token(url) {
                        self.report.drop(format!("not loaded: dataset '{label}' (its link carries an access token; load the data yourself)"));
                        self.unloaded.insert(id);
                        continue;
                    }
                    DataRef::url(url)
                } else {
                    self.report.drop(format!("not loaded: dataset '{label}' (its link is not a web address; only https and http links are read)"));
                    self.unloaded.insert(id);
                    continue;
                }
            };
            let kind = match data_ref.url.as_deref().map(str::to_ascii_lowercase) {
                Some(u) if u.contains(".geojson") || u.contains(".json") => "geojson",
                Some(u) if u.contains(".parquet") => "geoparquet",
                Some(u) if u.contains(".fgb") => "fgb",
                _ => "csv",
            };
            let columns: BTreeMap<String, String> = fields.into_iter().collect();
            doc.sources.insert(
                id,
                Source {
                    kind: kind.into(),
                    data: data_ref,
                    label,
                    columns,
                    geometry: Value::Null,
                    attribution: None,
                    extra: Map::new(),
                },
            );
        }
    }

    fn layers(&mut self, layers: Option<&Value>, order: Option<&Value>, doc: &mut Doc) {
        let Some(layers) = layers else { return };
        let Some(list) = layers.as_array() else {
            self.report
                .drop("dropped: the layers (their block is unreadable)");
            return;
        };
        if list.len() > MAX_LAYERS {
            self.report.drop(format!(
                "dropped: {} layers past the first {MAX_LAYERS}",
                list.len() - MAX_LAYERS
            ));
        }
        let mut ours = Vec::new();
        // Each drawn layer's place in `layerOrder` (top first), when the
        // file has one.
        let mut ranks = Vec::new();
        let rank = layer_order_rank(order);
        let mut ids = BTreeSet::new();
        for (index, raw) in list.iter().take(MAX_LAYERS).enumerate() {
            let ty = raw.get("type").and_then(Value::as_str).unwrap_or("");
            let name = layer_name(raw);
            if let Some(id) = raw
                .get("config")
                .and_then(|c| c.get("dataId"))
                .and_then(Value::as_str)
            {
                self.referenced.insert(id.to_string());
            }
            let drawn = LayerType::from_kepler(ty).filter(|_| raw.is_object());
            match drawn {
                Some(kind) => match self.layer(raw, kind, &name) {
                    Some(mut layer) => {
                        while !ids.insert(layer.id.clone()) {
                            layer.id.push_str("-2");
                        }
                        ranks.push(rank(index, raw.get("id").and_then(Value::as_str)));
                        ours.push(layer);
                    }
                    None => self.keep_layer(
                        index,
                        raw,
                        &format!("not drawn: layer '{name}' (it names no dataset)"),
                        &name,
                    ),
                },
                None => {
                    if raw.is_object() {
                        let what = match ty {
                            "heatmap" => "heatmap".to_string(),
                            "" => "layer of this type".to_string(),
                            t => format!("{} layer", short(t)),
                        };
                        let shown = if ty.is_empty() {
                            "unknown".to_string()
                        } else {
                            short(ty)
                        };
                        self.keep_layer(index, raw, &format!("not drawn: {shown} layer '{name}' (the Explorer has no {what} yet)"), &name);
                    } else {
                        self.report
                            .drop(format!("dropped: layer {} (unreadable)", index + 1));
                    }
                }
            }
        }
        // Kepler.gl's `layerOrder` (top first) decides; a layer it does
        // not name follows the named ones in `layers` order. Without one,
        // `layers` is top first. The document draws its first layer first.
        let mut ranked: Vec<(usize, Layer)> = ranks.into_iter().zip(ours).collect();
        ranked.sort_by_key(|(r, _)| *r);
        let mut ours: Vec<Layer> = ranked.into_iter().map(|(_, l)| l).collect();
        ours.reverse();
        doc.map.layers = ours;
    }

    fn keep_layer(&mut self, index: usize, raw: &Value, line: &str, name: &str) {
        let mut layer = raw.clone();
        if strip_tokens(&mut layer) {
            self.report
                .drop(format!("dropped: an access token in layer '{name}'"));
        }
        self.report.drop(line);
        self.unsupported_layers
            .push(json!({ "index": index, "layer": layer }));
    }

    /// A layer the Explorer draws, or `None` when it names no dataset.
    fn layer(&mut self, raw: &Value, kind: LayerType, name: &str) -> Option<Layer> {
        let empty = Map::new();
        let config = raw
            .get("config")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let source = config.get("dataId").and_then(Value::as_str)?.to_string();
        let id = raw
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("layer")
            .to_string();
        let label = config
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let visible = config
            .get("isVisible")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let mut columns = BTreeMap::new();
        if let Some(cols) = config.get("columns").and_then(Value::as_object) {
            for (k, v) in cols {
                // v1 saves names; a v0-shaped `{value}` is read too.
                let v = v
                    .as_str()
                    .or_else(|| v.get("value").and_then(Value::as_str));
                if let Some(v) = v {
                    let k = match k.as_str() {
                        "lng" if kind != LayerType::GeoJson => "lon",
                        "lng0" => "lon0",
                        "lng1" => "lon1",
                        other => other,
                    };
                    columns.insert(k.to_string(), v.to_string());
                }
            }
        }

        // A trip with no row columns reads GeoJSON lines: named, so the
        // export's default comes back as it went.
        if kind == LayerType::Trip && !columns.contains_key("timestamp") {
            let g = columns
                .remove("geojson")
                .unwrap_or_else(|| "_geojson".into());
            columns = BTreeMap::from([("geojson".to_string(), g)]);
        }
        let mut vis_config = config
            .get("visConfig")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let color = rgb(config.get("color"));
        // Kepler.gl's v1 load: an old point layer with no `filled` and no
        // stroke colour strokes in its own colour, and an outlined one is
        // not filled.
        if kind == LayerType::Points
            && !vis_config.contains_key("filled")
            && vis_config.get("strokeColor").is_none_or(Value::is_null)
            && vis_config
                .get("strokeColorRange")
                .is_none_or(Value::is_null)
        {
            if let Some(c) = color {
                vis_config.insert("strokeColor".into(), json!(c));
            }
            if vis_config.get("outline").and_then(Value::as_bool) == Some(true) {
                vis_config.insert("filled".into(), Value::Bool(false));
            }
        }
        let defaults = Vis::default();
        let mut take_num = |key: &str, d: f64| match vis_config.remove(key) {
            Some(v) => v.as_f64().filter(|x| x.is_finite()).unwrap_or(d),
            None => d,
        };
        let radius_default = if kind == LayerType::Heatmap {
            crate::doc::DEFAULT_HEATMAP_RADIUS
        } else {
            10.0
        };
        let radius = take_num("radius", radius_default).clamp(0.0, 1000.0);
        let opacity = take_num("opacity", defaults.opacity).clamp(0.0, 1.0);
        let thickness = take_num("thickness", 2.0).clamp(0.0, 100.0);
        let outline_key = if kind == LayerType::Points {
            "outline"
        } else {
            "stroked"
        };
        let mut take_bool = |key: &str, d: bool| match vis_config.remove(key) {
            Some(v) => v.as_bool().unwrap_or(d),
            None => d,
        };
        let filled = take_bool("filled", true);
        let outline = take_bool(outline_key, kind == LayerType::GeoJson);
        let stroke_color = match vis_config.remove("strokeColor") {
            Some(v) => rgb(Some(&v)),
            None => None,
        };
        if kind == LayerType::Points
            && vis_config.get("fixedRadius").and_then(Value::as_bool) == Some(true)
        {
            self.report.drop(format!(
                "dropped: fixed radius in metres on layer '{name}' (drawn in pixels)"
            ));
        }
        if kind == LayerType::GeoJson
            && vis_config.get("wireframe").and_then(Value::as_bool) == Some(true)
        {
            self.report
                .drop(format!("not drawn: wireframe on layer '{name}'"));
        }
        // The binned, arc, cluster and 3D settings (P6).
        let pair = |v: Option<Value>| {
            v.and_then(|r| Some([num(r.as_array()?.first())?, num(r.as_array()?.get(1))?]))
        };
        let positive = |v: Option<Value>| {
            v.and_then(|x| x.as_f64())
                .filter(|x| x.is_finite() && *x > 0.0)
        };
        let binned = kind.is_binned();
        let cell_km = if binned {
            positive(vis_config.remove("worldUnitSize")).map(|v| v.clamp(0.001, 500.0))
        } else {
            None
        };
        let coverage = if binned {
            vis_config
                .remove("coverage")
                .and_then(|v| v.as_f64())
                .map(|v| v.clamp(0.0, 1.0))
        } else {
            None
        };
        let percentile = if binned {
            pair(vis_config.remove("percentile"))
                .map(|[a, b]| [a.clamp(0.0, 100.0), b.clamp(0.0, 100.0)])
        } else {
            None
        };
        let three_d = matches!(
            kind,
            LayerType::Hexbin | LayerType::Grid | LayerType::GeoJson | LayerType::H3
        );
        let enable3d = three_d
            && vis_config
                .remove("enable3d")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        let elevation_scale = if three_d {
            vis_config
                .remove("elevationScale")
                .and_then(|v| v.as_f64())
                .filter(|v| v.is_finite() && *v >= 0.0)
        } else {
            None
        };
        let aggregation = if binned {
            vis_config
                .remove("colorAggregation")
                .and_then(|v| v.as_str().map(str::to_string))
        } else {
            None
        };
        let aggregation =
            aggregation.map(
                |a| match crate::kepler::AGGREGATIONS.contains(&a.as_str()) {
                    true => a,
                    false => {
                        self.report.approx(format!(
                            "approximated: {} aggregation on layer '{name}' drawn as a count",
                            short(&a)
                        ));
                        "count".into()
                    }
                },
            );
        let target_color = if kind.is_od() {
            vis_config.remove("targetColor").and_then(|v| rgb(Some(&v)))
        } else {
            None
        };
        let cluster_radius = if kind == LayerType::Cluster {
            positive(vis_config.remove("clusterRadius"))
        } else {
            None
        };
        if binned {
            for key in ["sizeAggregation", "elevationPercentile"] {
                if let Some(v) = vis_config.get(key) {
                    let default = if key == "sizeAggregation" {
                        json!("count")
                    } else {
                        json!([0, 100])
                    };
                    if *v != default
                        && v.as_str() != vis_config.get("colorAggregation").and_then(Value::as_str)
                    {
                        self.report.approx(format!(
                            "approximated: the height of layer '{name}' follows its colour value"
                        ));
                        break;
                    }
                }
            }
            vis_config.remove("sizeAggregation");
        }
        let highlight_color = rgb(config.get("highlightColor")).filter(|c| *c != KEPLER_HIGHLIGHT);
        // A value at Kepler.gl's default is left unset (what the export
        // writes back for unset), so a round trip is stable.
        let cell_km = cell_km.filter(|v| *v != crate::doc::DEFAULT_CELL_KM);
        let coverage = coverage.filter(|v| *v != crate::doc::DEFAULT_COVERAGE);
        let percentile = percentile.filter(|v| *v != [0.0, 100.0]);
        let elevation_scale = elevation_scale.filter(|v| *v != crate::doc::DEFAULT_ELEVATION_SCALE);
        let aggregation = aggregation.filter(|a| a != "count");
        let cluster_radius = cluster_radius.filter(|v| *v != crate::doc::DEFAULT_CLUSTER_RADIUS);
        if let Some(labels) = config.get("textLabel") {
            let labels: Vec<&Value> = match labels {
                Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };
            for l in labels {
                if let Some(field) = l
                    .get("field")
                    .and_then(|f| f.get("name").or(Some(f)))
                    .and_then(Value::as_str)
                {
                    self.report
                        .drop(format!("not drawn: text label '{field}' on layer '{name}'"));
                }
            }
        }

        let channels = raw
            .get("visualChannels")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let field_of = |key: &str| {
            channels
                .get(key)
                .and_then(|f| f.get("name").or(Some(f)))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let mut kept_channels = channels.clone();

        // Colour. A binned or cluster layer with no field colours by the
        // point count, through its colour range.
        let counted = binned || matches!(kind, LayerType::Cluster | LayerType::Heatmap);
        let color_binding = field_of("colorField")
            .filter(|_| kind != LayerType::Cluster)
            .or_else(|| counted.then(String::new))
            .map(|field| {
                kept_channels.remove("colorField");
                let default = if counted { "quantize" } else { "quantile" };
                let scale = kept_channels
                    .remove("colorScale")
                    .and_then(|s| s.as_str().map(str::to_string))
                    .unwrap_or_else(|| default.into());
                // A count always has a colour range: Kepler's default when none
                // is saved, so the export writes back what was read.
                let range = vis_config
                    .get("colorRange")
                    .cloned()
                    .or_else(|| counted.then(super::export::default_color_range));
                self.color_binding(field, &scale, range.as_ref(), name)
            });
        if color_binding.is_some() {
            vis_config.remove("colorRange");
        } else {
            kept_channels.remove("colorField");
            kept_channels.remove("colorScale");
        }

        // Size: a point's radius by `sizeField`, a geojson point's by
        // `radiusField`.
        let (size_key, scale_key) = match kind {
            LayerType::Points => ("sizeField", "sizeScale"),
            LayerType::GeoJson => ("radiusField", "radiusScale"),
            // A heatmap's weight rides in the size channel.
            LayerType::Heatmap => ("weightField", "weightScale"),
            // No size channel: a binned layer's height follows its colour
            // value; arc widths by field are engine gap E8.
            _ => ("", ""),
        };
        let radius_binding = field_of(size_key)
            .filter(|_| !size_key.is_empty())
            .map(|field| {
                kept_channels.remove(size_key);
                let scale = kept_channels
                    .remove(scale_key)
                    .and_then(|s| s.as_str().map(str::to_string))
                    .unwrap_or_else(|| "linear".into());
                let scale = if SIZE_SCALES.contains(&scale.as_str()) {
                    scale
                } else {
                    self.report.approx(format!(
                        "approximated: {} size scale on layer '{name}' drawn as linear",
                        short(&scale)
                    ));
                    "linear".into()
                };
                let range = vis_config.remove("radiusRange").and_then(|r| {
                    let a = r.as_array()?;
                    Some([num(a.first())?, num(a.get(1))?])
                });
                let mut b = ChannelBinding::new(field, scale);
                // A weight is always onto 0..=1: no range.
                b.range =
                    (kind != LayerType::Heatmap).then(|| range.unwrap_or(KEPLER_RADIUS_RANGE));
                b
            });
        if radius_binding.is_none() && !size_key.is_empty() {
            kept_channels.remove(size_key);
            kept_channels.remove(scale_key);
        }
        // Height (a 3D GeoJSON layer): `heightField` through `heightScale`
        // onto `heightRange`.
        let height_binding = (kind == LayerType::GeoJson)
            .then(|| field_of("heightField"))
            .flatten()
            .map(|field| {
                kept_channels.remove("heightField");
                let scale = kept_channels
                    .remove("heightScale")
                    .and_then(|s| s.as_str().map(str::to_string))
                    .unwrap_or_else(|| "linear".into());
                let scale = if SIZE_SCALES.contains(&scale.as_str()) {
                    scale
                } else {
                    "linear".into()
                };
                let mut b = ChannelBinding::new(field, scale);
                b.range = pair(vis_config.remove("heightRange"));
                b
            });
        if height_binding.is_none() {
            kept_channels.remove("heightField");
            kept_channels.remove("heightScale");
        }

        // Every other channel bound to a field is kept but not drawn; an
        // unbound channel (and its scale) is a default the export writes.
        for (key, v) in &channels {
            if let Some(channel) = key.strip_suffix("Field") {
                if key == "colorField"
                    || key == size_key
                    || (key == "heightField" && height_binding.is_some())
                {
                    continue;
                }
                match v.get("name").or(Some(v)).and_then(Value::as_str) {
                    Some(field) => self.report.drop(format!(
                        "not applied: {channel} by '{field}' on layer '{name}'"
                    )),
                    None => {
                        kept_channels.remove(key);
                        kept_channels.remove(&format!("{channel}Scale"));
                    }
                }
            }
        }
        kept_channels.retain(|k, _| match k.strip_suffix("Scale") {
            Some(channel) => kept_channels_has_field(&channels, channel),
            None => true,
        });

        let mut kept_config = config.clone();
        for key in [
            "dataId",
            "label",
            "color",
            "columns",
            "isVisible",
            "visConfig",
            "highlightColor",
        ] {
            kept_config.remove(key);
        }
        if kept_config.get("hidden").and_then(Value::as_bool) == Some(false) {
            kept_config.remove("hidden");
        }
        let unlabelled = |t: &Value| match t {
            Value::Array(a) => a.iter().all(|l| l.get("field").is_none_or(Value::is_null)),
            Value::Object(_) => t.get("field").is_none_or(Value::is_null),
            _ => true,
        };
        if kept_config.get("textLabel").is_some_and(unlabelled) {
            kept_config.remove("textLabel");
        }
        // A visConfig key at the export's default adds nothing.
        for (k, d) in super::export::default_vis_config(kind) {
            if vis_config.get(&k) == Some(&d) {
                vis_config.remove(&k);
            }
        }
        let mut kepler = Map::new();
        if !kept_config.is_empty() {
            kepler.insert("config".into(), Value::Object(kept_config));
        }
        if !kept_channels.is_empty() {
            kepler.insert("visualChannels".into(), Value::Object(kept_channels));
        }
        let mut extra = Map::new();
        if !kepler.is_empty() {
            let mut kepler = Value::Object(kepler);
            strip_tokens(&mut kepler);
            extra.insert("kepler".into(), kepler);
        }
        let mut vis_extra = Value::Object(vis_config);
        strip_tokens(&mut vis_extra);
        let vis_extra = match vis_extra {
            Value::Object(m) => m,
            _ => Map::new(),
        };

        Some(Layer {
            id,
            kind,
            source,
            label,
            visible,
            columns,
            vis: Vis {
                radius,
                opacity,
                filled,
                outline,
                thickness,
                color: color.unwrap_or(defaults.color),
                stroke_color,
                cell_km,
                coverage,
                percentile,
                enable3d,
                elevation_scale,
                aggregation,
                target_color,
                cluster_radius,
                extra: vis_extra,
            },
            color: color_binding,
            radius: radius_binding,
            height: height_binding,
            hover: Hover {
                card: Vec::new(),
                highlight: true,
                highlight_color,
                extra: Map::new(),
            },
            extra,
        })
    }

    fn color_binding(
        &mut self,
        field: String,
        scale: &str,
        range: Option<&Value>,
        name: &str,
    ) -> ChannelBinding {
        let scale = match scale {
            s if COLOR_SCALES.contains(&s) => s.to_string(),
            "jenks" => {
                self.report.approx(format!(
                    "approximated: jenks colour scale on layer '{name}' drawn as quantile"
                ));
                "quantile".into()
            }
            "custom" | "threshold" => "custom".into(),
            "customOrdinal" => "customOrdinal".into(),
            other => {
                self.report.approx(format!(
                    "approximated: {} colour scale on layer '{name}' drawn as quantile",
                    short(other)
                ));
                "quantile".into()
            }
        };
        let mut b = ChannelBinding::new(field, scale.clone());
        let range = range.and_then(Value::as_object);
        let colors: Vec<Value> = range
            .and_then(|r| r.get("colors"))
            .and_then(Value::as_array)
            .map(|c| c.iter().filter(|c| c.is_string()).cloned().collect())
            .unwrap_or_default();
        if let Some(range) = range {
            b.palette = range
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string);
            b.reversed = range
                .get("reversed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            for (key, ours) in [("type", "palette_type"), ("category", "palette_category")] {
                if let Some(v) = range.get(key).and_then(Value::as_str) {
                    b.extra.insert(ours.into(), Value::String(v.to_string()));
                }
            }
        }
        if !colors.is_empty() {
            b.steps = Some(colors.len() as u32);
            b.extra.insert("colors".into(), Value::Array(colors));
        }
        if scale == "custom" || scale == "customOrdinal" {
            let map = range
                .and_then(|r| r.get("colorMap"))
                .and_then(Value::as_array);
            let (mut keys, mut colors) = (Vec::new(), Vec::new());
            for pair in map.into_iter().flatten() {
                if let (Some(k), Some(c)) = (pair.get(0), pair.get(1).and_then(Value::as_str)) {
                    keys.push(k.clone());
                    colors.push(Value::String(c.to_string()));
                }
            }
            if map.is_none() {
                self.report.approx(format!("approximated: custom colour scale on layer '{name}' has no breaks, drawn with its colours in order"));
            }
            let key = if scale == "custom" {
                "breaks"
            } else {
                "categories"
            };
            b.extra.insert(key.into(), Value::Array(keys));
            if !colors.is_empty() {
                b.steps = Some(colors.len() as u32);
                b.extra.insert("colors".into(), Value::Array(colors));
            }
        }
        b
    }

    fn filters(&mut self, filters: Option<&Value>, doc: &mut Doc) {
        let Some(filters) = filters else { return };
        let Some(list) = filters.as_array() else {
            self.report
                .drop("dropped: the filters (their block is unreadable)");
            return;
        };
        if list.len() > MAX_FILTERS {
            self.report.drop(format!(
                "dropped: {} filters past the first {MAX_FILTERS}",
                list.len() - MAX_FILTERS
            ));
        }
        let mut ids = BTreeSet::new();
        for (index, raw) in list.iter().take(MAX_FILTERS).enumerate() {
            let Some(obj) = raw.as_object() else {
                self.report
                    .drop(format!("dropped: filter {} (unreadable)", index + 1));
                continue;
            };
            let id = obj
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map_or_else(|| format!("filter-{}", index + 1), str::to_string);
            let datasets = data_ids(obj.get("dataId"));
            self.referenced.extend(datasets.iter().cloned());
            let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");
            let kind: Option<FilterKind> =
                serde_json::from_value(Value::String(ty.to_string())).ok();
            let Some(kind) = kind else {
                let mut kept = raw.clone();
                strip_tokens(&mut kept);
                let ty = if ty.is_empty() {
                    "unknown".to_string()
                } else {
                    short(ty)
                };
                self.report.drop(format!("not applied: {ty} filter '{id}'"));
                self.unsupported_filters
                    .push(json!({ "index": index, "filter": kept }));
                continue;
            };
            if datasets.is_empty() {
                self.report
                    .drop(format!("not applied: filter '{id}' (it names no dataset)"));
                continue;
            }
            let names: Vec<Option<String>> = match obj.get("name") {
                Some(Value::String(s)) => vec![Some(s.clone()); datasets.len()],
                Some(Value::Array(a)) => (0..datasets.len())
                    .map(|i| a.get(i).and_then(Value::as_str).map(str::to_string))
                    .collect(),
                _ => vec![None; datasets.len()],
            };
            // Dropped, warned.
            let plot = obj.get("plotType").map(|p| p.get("type").unwrap_or(p));
            if plot.is_some_and(|p| !p.is_null() && p.as_str() != Some("histogram")) {
                self.report.drop(format!(
                    "dropped: the chart in filter '{id}' (shown as a histogram)"
                ));
            }
            if obj.get("yAxis").is_some_and(|y| !y.is_null()) {
                self.report
                    .drop(format!("dropped: the y axis of filter '{id}'"));
            }
            if let Some(w) = obj.get("animationWindow").and_then(Value::as_str) {
                if w != "free" {
                    self.report.approx(format!(
                        "approximated: {} animation window of filter '{id}' plays as free",
                        short(w)
                    ));
                }
            }
            let value = self.filter_value(kind, obj.get("value"), &id);
            let enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let speed = num(obj.get("speed"));
            let mut extra = obj.clone();
            for key in [
                "id",
                "dataId",
                "name",
                "type",
                "value",
                "enabled",
                "speed",
                "plotType",
                "yAxis",
                "animationWindow",
            ] {
                extra.remove(key);
            }
            let mut extra_v = Value::Object(extra);
            strip_tokens(&mut extra_v);
            let extra = match extra_v {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            for (i, (source, field)) in datasets.iter().zip(names).enumerate() {
                let Some(field) = field else {
                    self.report
                        .drop(format!("not applied: filter '{id}' (it names no field)"));
                    continue;
                };
                let mut fid = if datasets.len() > 1 {
                    format!("{id}-{}", i + 1)
                } else {
                    id.clone()
                };
                while !ids.insert(fid.clone()) {
                    fid.push_str("-2");
                }
                let mut f = Filter::new(fid, source.clone(), field, kind, value.clone());
                f.enabled = enabled;
                f.speed = speed;
                f.extra = extra.clone();
                doc.filters.push(f);
            }
        }
    }

    fn filter_value(&mut self, kind: FilterKind, v: Option<&Value>, id: &str) -> FilterValue {
        let v = match v {
            None | Some(Value::Null) => return FilterValue::All,
            Some(v) => v,
        };
        let read = match kind {
            FilterKind::Range | FilterKind::TimeRange => v
                .as_array()
                .filter(|a| a.len() == 2)
                .and_then(|a| Some(FilterValue::Range([num(a.first())?, num(a.get(1))?]))),
            FilterKind::Select => v.as_bool().map(FilterValue::Bool),
            FilterKind::MultiSelect => v.as_array().map(|a| {
                FilterValue::Values(
                    a.iter()
                        .filter_map(|x| match x {
                            Value::String(s) => Some(s.clone()),
                            Value::Number(n) => Some(n.to_string()),
                            Value::Bool(b) => Some(b.to_string()),
                            _ => None,
                        })
                        .collect(),
                )
            }),
        };
        read.unwrap_or_else(|| {
            self.report.approx(format!(
                "approximated: filter '{id}' has an unreadable value and shows everything"
            ));
            FilterValue::All
        })
    }

    fn interaction(&mut self, config: Option<&Value>, doc: &mut Doc) {
        let Some(config) = config.and_then(Value::as_object) else {
            return;
        };
        if let Some(tooltip) = config.get("tooltip").and_then(Value::as_object) {
            let enabled = tooltip
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if let Some(fields) = tooltip.get("fieldsToShow").and_then(Value::as_object) {
                for (data_id, list) in fields {
                    self.referenced.insert(data_id.clone());
                    let card: Vec<HoverField> = list
                        .as_array()
                        .map(|l| {
                            l.iter()
                                .take(64)
                                .filter_map(|f| self.hover_field(f))
                                .collect()
                        })
                        .unwrap_or_default();
                    for layer in doc.map.layers.iter_mut().filter(|l| &l.source == data_id) {
                        layer.hover.card = card.clone();
                    }
                }
            }
            if !enabled {
                for layer in &mut doc.map.layers {
                    layer
                        .hover
                        .extra
                        .insert("enabled".into(), Value::Bool(false));
                }
            }
        }
        for (key, what) in [
            ("brush", "brush"),
            ("geocoder", "geocoder"),
            ("coordinate", "coordinate readout"),
            ("annotation", "annotation tool"),
            ("zoomOpacity", "zoom fade"),
        ] {
            if config
                .get(key)
                .and_then(|c| c.get("enabled"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                self.report
                    .drop(format!("dropped: the {what} (the Explorer has none yet)"));
            }
        }
    }

    fn hover_field(&mut self, f: &Value) -> Option<HoverField> {
        // v0 lists names; v1 `{name, format}`.
        let (name, format) = match f {
            Value::String(s) => (s.clone(), None),
            Value::Object(o) => (
                o.get("name").and_then(Value::as_str)?.to_string(),
                o.get("format")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty()),
            ),
            _ => return None,
        };
        let format = format.and_then(|fmt| {
            if Format::parse(Some(fmt)) == Format::None {
                self.report.approx(format!(
                    "approximated: the format of '{name}' in the hover card shows as Auto"
                ));
                None
            } else {
                Some(fmt.to_string())
            }
        });
        Some(HoverField {
            field: name,
            format,
            extra: Map::new(),
        })
    }

    fn vis_state_rest(&mut self, vis: &Map<String, Value>) {
        match vis.get("layerBlending").and_then(Value::as_str) {
            None | Some("normal") => {}
            Some(other) => {
                let other = short(other);
                self.report.approx(format!(
                    "approximated: {other} layer blending drawn as normal"
                ));
                self.blending = Some(other);
            }
        }
        if vis
            .get("splitMaps")
            .and_then(Value::as_array)
            .is_some_and(|s| !s.is_empty())
        {
            self.report
                .drop("dropped: split map (the Explorer shows one map)");
        }
        if let Some(effects) = vis.get("effects").and_then(Value::as_array) {
            for (i, e) in effects.iter().enumerate() {
                let ty = e
                    .get("type")
                    .or_else(|| e.get("config").and_then(|c| c.get("type")))
                    .and_then(Value::as_str)
                    .map_or_else(|| "visual".to_string(), short);
                let id = e
                    .get("id")
                    .and_then(Value::as_str)
                    .map_or_else(|| (i + 1).to_string(), str::to_string);
                self.report.drop(format!(
                    "dropped: {ty} effect '{id}' (the Explorer has no effects yet)"
                ));
            }
        }
        if vis
            .get("annotations")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            self.report
                .drop("dropped: annotations (the Explorer has none yet)");
        }
        if let Some(charts) = vis.get("charts").and_then(Value::as_array) {
            for (i, c) in charts.iter().enumerate() {
                let title = c
                    .get("title")
                    .and_then(Value::as_str)
                    .or_else(|| c.get("id").and_then(Value::as_str))
                    .map_or_else(|| (i + 1).to_string(), str::to_string);
                self.report.drop(format!(
                    "dropped: chart '{title}' (the Explorer has no charts panel yet)"
                ));
            }
        }
    }

    fn map_state(&mut self, state: Option<&Value>, view: &mut View) {
        let Some(s) = state.and_then(Value::as_object) else {
            return;
        };
        let d = View::default();
        view.lat = num(s.get("latitude")).unwrap_or(d.lat).clamp(-90.0, 90.0);
        view.lon = num(s.get("longitude"))
            .unwrap_or(d.lon)
            .clamp(-180.0, 180.0);
        view.zoom = num(s.get("zoom")).unwrap_or(d.zoom).clamp(0.0, 24.0);
        view.pitch = num(s.get("pitch")).unwrap_or(0.0).clamp(0.0, 85.0);
        view.bearing = num(s.get("bearing")).unwrap_or(0.0);
        if !(-360.0..=360.0).contains(&view.bearing) {
            view.bearing = view.bearing.rem_euclid(360.0);
        }
        if matches!(
            s.get("mapViewMode").and_then(Value::as_str),
            Some("MODE_GLOBE" | "globe")
        ) {
            view.projection = "globe".into();
            if s.get("globe")
                .and_then(Value::as_object)
                .is_some_and(|g| !g.is_empty())
            {
                self.report
                    .drop("dropped: the globe's colours (the Explorer draws its own)");
            }
        }
        if s.get("isSplit").and_then(Value::as_bool) == Some(true) {
            self.report
                .drop("dropped: split map (the Explorer shows one map)");
        }
    }

    fn map_style(&mut self, style: Option<&Value>, view: &mut View) {
        let Some(s) = style.and_then(Value::as_object) else {
            return;
        };
        let ty = s.get("styleType").and_then(Value::as_str).unwrap_or("");
        let custom = s
            .get("mapStyles")
            .and_then(|m| m.get(ty))
            .filter(|m| m.is_object());
        view.basemap = match ty {
            _ if custom.is_some() => {
                let c = custom.unwrap_or(&Value::Null);
                let label = c
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|l| !super::looks_like_token(l))
                    .or_else(|| c.get("id").and_then(Value::as_str))
                    .unwrap_or(ty);
                let label = if super::looks_like_token(label) || label.contains("://") {
                    "custom"
                } else {
                    label
                };
                self.report.approx(format!(
                    "approximated: custom map style '{}' drawn as streets",
                    short_label(label)
                ));
                "streets"
            }
            "dark" | "dark-matter" | "muted_night" | "dark_matter" => "dark",
            "light" | "voyager" | "positron" => "light",
            "muted" | "streets" | "" => "streets",
            t if t.starts_with("satellite") => {
                self.report
                    .approx("approximated: satellite map style drawn as streets");
                "streets"
            }
            other => {
                let other = if super::looks_like_token(other) || other.contains("://") {
                    "custom".to_string()
                } else {
                    short_label(other)
                };
                self.report.approx(format!(
                    "approximated: map style '{other}' drawn as streets"
                ));
                "streets"
            }
        }
        .into();
        // Borders and 3D buildings are off by default; another hidden
        // group is a choice the Explorer cannot keep.
        if let Some(groups) = s.get("visibleLayerGroups").and_then(Value::as_object) {
            let hidden: Vec<&str> = groups
                .iter()
                .filter(|(k, v)| {
                    v.as_bool() == Some(false) && !matches!(k.as_str(), "border" | "3d building")
                })
                .map(|(k, _)| k.as_str())
                .take(12)
                .collect();
            if !hidden.is_empty() {
                self.report.drop(format!(
                    "dropped: hidden basemap groups ({}); the Explorer shows the whole basemap",
                    hidden.join(", ")
                ));
            }
        }
    }
}

/// Whether `channel`'s field is bound in `channels` (a scale beside an
/// unbound field is a default).
fn kept_channels_has_field(channels: &Map<String, Value>, channel: &str) -> bool {
    channels
        .get(&format!("{channel}Field"))
        .is_some_and(|f| !f.is_null())
}

fn short_label(s: &str) -> String {
    s.chars().take(48).collect()
}

/// RFC 4180 CSV of `fields` and `rows`; returns the text and how many
/// rows were not arrays (skipped). A row shorter than the header is
/// padded with empty cells, a longer one cut.
pub(crate) fn write_csv(fields: &[(String, String)], rows: &[Value]) -> (String, usize) {
    let mut out = String::new();
    let header: Vec<String> = fields.iter().map(|(n, _)| csv_cell(n)).collect();
    out.push_str(&header.join(","));
    out.push_str("\r\n");
    let mut bad = 0;
    for row in rows {
        let Some(cells) = row.as_array() else {
            bad += 1;
            continue;
        };
        let line: Vec<String> = (0..fields.len())
            .map(|i| match cells.get(i) {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => csv_cell(s),
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::Bool(b)) => b.to_string(),
                Some(v) => csv_cell(&v.to_string()),
            })
            .collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    (out, bad)
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Kepler.gl's `visStateSchemaV0 -> V1`, on raw JSON: each layer's flat
/// v0 keys fold into `config`, `visConfig` and `visualChannels`; a
/// tooltip or brush that is present is enabled.
fn migrate_v0(v0: &Map<String, Value>) -> Map<String, Value> {
    let mut out = v0.clone();
    if let Some(layers) = v0.get("layers").and_then(Value::as_array) {
        out.insert(
            "layers".into(),
            Value::Array(layers.iter().map(migrate_layer_v0).collect()),
        );
    }
    if let Some(filters) = v0.get("filters").and_then(Value::as_array) {
        let migrated = filters
            .iter()
            .map(|f| {
                let mut f = f.clone();
                if let Some(o) = f.as_object_mut() {
                    // `enlarged` becomes a view.
                    if let Some(e) = o.remove("enlarged") {
                        o.insert(
                            "view".into(),
                            Value::String(
                                if e.as_bool() == Some(true) {
                                    "enlarged"
                                } else {
                                    "side"
                                }
                                .into(),
                            ),
                        );
                    }
                }
                f
            })
            .collect();
        out.insert("filters".into(), Value::Array(migrated));
    }
    if let Some(ic) = v0.get("interactionConfig").and_then(Value::as_object) {
        let mut migrated = Map::new();
        for key in ["tooltip", "brush"] {
            let present = ic.get(key).filter(|v| !v.is_null());
            let mut entry = present
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            entry.insert("enabled".into(), Value::Bool(present.is_some()));
            migrated.insert(key.into(), Value::Object(entry));
        }
        out.insert("interactionConfig".into(), Value::Object(migrated));
    }
    out
}

fn migrate_layer_v0(layer: &Value) -> Value {
    let Some(l) = layer.as_object() else {
        return layer.clone();
    };
    let ty = l.get("type").and_then(Value::as_str).unwrap_or("");
    let mut config = Map::new();
    for key in ["dataId", "label", "color", "isVisible", "hidden"] {
        if let Some(v) = l.get(key) {
            config.insert(key.into(), v.clone());
        }
    }
    let v0_vis = l
        .get("visConfig")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut vis = Map::new();
    for (k, v) in &v0_vis {
        let k = match (ty, k.as_str()) {
            ("geojson", "extruded") => "enable3d",
            ("geojson", "elevationRange") => "heightRange",
            (_, k) => k,
        };
        vis.insert(k.to_string(), v.clone());
    }
    for key in ["enable3d", "colorAggregation", "sizeAggregation"] {
        if let Some(v) = l.get(key) {
            vis.insert(key.into(), v.clone());
        }
    }
    if let Some(cols) = l.get("columns").and_then(Value::as_object) {
        let cols: Map<String, Value> = cols
            .iter()
            .map(|(k, v)| (k.clone(), v.get("value").cloned().unwrap_or(Value::Null)))
            .collect();
        config.insert("columns".into(), Value::Object(cols));
    }
    let mut channels = Map::new();
    let dim = |v: &Value| match v {
        Value::Object(o) => {
            json!({ "name": o.get("name").cloned().unwrap_or(Value::Null), "type": o.get("type").cloned().unwrap_or(Value::Null) })
        }
        _ => Value::Null,
    };
    if let Some(f) = l.get("colorField") {
        channels.insert("colorField".into(), dim(f));
    }
    if let Some(s) = l.get("colorScale") {
        channels.insert("colorScale".into(), s.clone());
    }
    if let Some(f) = l.get("sizeField") {
        let key = if ty == "geojson" && !f.is_null() {
            geojson_size_key_v0(&v0_vis)
        } else {
            "sizeField"
        };
        channels.insert(key.into(), dim(f));
    }
    if let Some(s) = l.get("sizeScale") {
        if ty != "geojson" {
            channels.insert("sizeScale".into(), s.clone());
        }
    }
    config.insert("visConfig".into(), Value::Object(vis));
    json!({
        "id": l.get("id").cloned().unwrap_or(Value::Null),
        "type": ty,
        "config": config,
        "visualChannels": channels,
    })
}

/// Which v1 channel a v0 geojson layer's `sizeField` meant.
fn geojson_size_key_v0(vis: &Map<String, Value>) -> &'static str {
    if vis.get("extruded").and_then(Value::as_bool) == Some(true) {
        return "heightField";
    }
    if vis.get("stroked").and_then(Value::as_bool) == Some(true) {
        return "sizeField";
    }
    let radius = num(vis.get("radius")).unwrap_or(10.0);
    let range = vis.get("radiusRange").and_then(Value::as_array);
    let range_changed = range.is_some_and(|r| {
        r.iter()
            .zip(KEPLER_RADIUS_RANGE)
            .any(|(a, b)| a.as_f64() != Some(b))
    });
    if radius != 10.0 || range_changed {
        "radiusField"
    } else {
        "sizeField"
    }
}
