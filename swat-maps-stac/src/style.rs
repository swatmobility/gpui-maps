//! A publisher's MapLibre style, made ready for a host's vector overlay.
//!
//! A Portolan PMTiles collection ships MapLibre GL v8 styles whose source
//! is `{"type": "vector", "url": "pmtiles://../x.pmtiles"}`, relative to
//! the style file. [`OverlayStyle::from_style`]:
//!
//! - resolves that URL ([`crate::url::join`]) and accepts any source name
//!   (portolan-nl's `wijken_buurten` names it `buurten`, not `data`);
//! - keeps the layers an overlay that draws fill, line and circle can
//!   draw, in order, with their `source` key dropped;
//! - draws `fill-extrusion` flat, as a `fill` of the extrusion's colour
//!   and opacity (noted in [`OverlayStyle::flattened`]);
//! - skips `symbol`, `raster`, `background`, `heatmap` and `hillshade`
//!   (noted in [`OverlayStyle::skipped`]), and layers on another source;
//! - reads a [`Legend`] from the style's colour expressions.

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::model::Warning;
use crate::url::join;

/// The layer types an overlay draws.
pub const DRAWN: &[&str] = &["fill", "line", "circle"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverlayStyle {
    /// The style's `name`.
    pub name: Option<String>,
    /// The PMTiles archive the layers read, absolute.
    pub pmtiles_url: Option<String>,
    /// The drawable layers, as a JSON array for the overlay compiler.
    pub layers: Value,
    /// `(layer id, type)` of every layer left out.
    pub skipped: Vec<(String, String)>,
    /// Layer ids drawn flat (fill-extrusion as fill).
    pub flattened: Vec<String>,
    pub legend: Option<Legend>,
    pub warnings: Vec<Warning>,
}

/// One key of a legend.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LegendEntry {
    /// The category, or the lower bound of a step class (`"≥ 30"`).
    pub label: String,
    /// `#rrggbb` (or what the style wrote).
    pub color: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegendKind {
    /// One colour for every feature.
    Single,
    /// `match` on a property: categories.
    Categories,
    /// `step` / `interpolate` on a property: numeric classes.
    Classes,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Legend {
    pub kind: LegendKind,
    /// The property the colour reads (`None` for a single colour).
    pub property: Option<String>,
    /// For classes, the class breaks in order (`entries[i]` starts at
    /// `breaks[i-1]`).
    pub breaks: Vec<f64>,
    pub entries: Vec<LegendEntry>,
}

impl OverlayStyle {
    /// Reads the style at `style_url` from its JSON.
    pub fn parse(style_url: &str, bytes: &[u8]) -> Result<OverlayStyle, String> {
        let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("{style_url}: not JSON ({e})"))?;
        Ok(OverlayStyle::from_style(style_url, &v))
    }

    pub fn from_style(style_url: &str, style: &Value) -> OverlayStyle {
        let mut warnings = Vec::new();
        let sources = style.get("sources").and_then(Value::as_object).cloned().unwrap_or_default();
        // The vector source the layers read: the one named `data`, else the
        // first vector source.
        let (source_name, source) = match sources.get("data").map(|s| ("data".to_string(), s.clone())) {
            Some(s) => (Some(s.0), Some(s.1)),
            None => {
                let first = sources.iter().find(|(_, s)| s.get("type").and_then(Value::as_str) == Some("vector"));
                if let Some((n, _)) = first {
                    warnings.push(Warning { code: "style_source", message: format!("the style's source is {n:?}, not \"data\"") });
                }
                (first.map(|(n, _)| n.clone()), first.map(|(_, s)| s.clone()))
            }
        };
        let pmtiles_url = source.as_ref().and_then(|s| s.get("url")).and_then(Value::as_str).map(|u| {
            let rel = u.strip_prefix("pmtiles://").unwrap_or(u);
            join(style_url, rel)
        });
        if pmtiles_url.is_none() {
            warnings.push(Warning { code: "style_source", message: "no vector source with a url".into() });
        }
        let mut layers = Vec::new();
        let mut skipped = Vec::new();
        let mut flattened = Vec::new();
        let all: Vec<Value> = style.get("layers").and_then(Value::as_array).cloned().unwrap_or_default();
        for layer in &all {
            let Some(lo) = layer.as_object() else { continue };
            let id = lo.get("id").and_then(Value::as_str).unwrap_or("?").to_string();
            let kind = lo.get("type").and_then(Value::as_str).unwrap_or("?").to_string();
            let on_source = match (lo.get("source").and_then(Value::as_str), &source_name) {
                (Some(s), Some(n)) => s == n,
                (None, _) => true,
                (Some(_), None) => false,
            };
            if !on_source {
                skipped.push((id, kind));
                continue;
            }
            let mut out: Map<String, Value> = lo.clone();
            out.remove("source");
            if DRAWN.contains(&kind.as_str()) {
                layers.extend(split_classes(out));
            } else if kind == "fill-extrusion" {
                layers.extend(split_classes(flatten_extrusion(&out)));
                flattened.push(id);
            } else {
                skipped.push((id, kind));
            }
        }
        let legend = legend_of(&all);
        OverlayStyle { name: style.get("name").and_then(Value::as_str).map(str::to_string), pmtiles_url, layers: Value::Array(layers), skipped, flattened, legend, warnings }
    }

    /// Whether any layer is drawable.
    pub fn is_drawable(&self) -> bool {
        self.layers.as_array().is_some_and(|a| !a.is_empty())
    }

    /// The `source-layer`s the drawn layers read.
    pub fn source_layers(&self) -> Vec<String> {
        let mut out: Vec<String> = self.layers.as_array().into_iter().flatten().filter_map(|l| l.get("source-layer").and_then(Value::as_str)).map(str::to_string).collect();
        out.dedup();
        out
    }

    /// The ids of the drawn layers.
    pub fn layer_ids(&self) -> Vec<String> {
        self.layers.as_array().into_iter().flatten().filter_map(|l| l.get("id").and_then(Value::as_str)).map(str::to_string).collect()
    }
}

/// The separator of a class layer's id: `buurten-fill#class-3`.
pub const CLASS_SUFFIX: &str = "#class-";

/// The legend class a split layer draws (`Some(3)` for `x#class-3`).
pub fn class_of_layer(id: &str) -> Option<usize> {
    id.rsplit_once(CLASS_SUFFIX)?.1.parse().ok()
}

/// A layer whose colour is a `step` (or `interpolate`) on a feature
/// property, as one layer per class: the class's range as a filter (ANDed
/// with the layer's own) and its colour as a constant. An overlay that
/// colours by zoom only draws them; MapLibre draws the two the same. Any
/// other layer comes back as it is.
fn split_classes(layer: Map<String, Value>) -> Vec<Value> {
    const COLOURS: [&str; 4] = ["fill-color", "line-color", "circle-color", "fill-outline-color"];
    let paint = layer.get("paint").and_then(Value::as_object).cloned().unwrap_or_default();
    let Some((key, expr)) = COLOURS.iter().find_map(|k| paint.get(*k).map(|v| (*k, v.clone()))) else { return vec![Value::Object(layer)] };
    let Some(legend) = expression_legend(&expr).filter(|l| l.kind == LegendKind::Classes) else { return vec![Value::Object(layer)] };
    let Some(prop) = legend.property.clone() else { return vec![Value::Object(layer)] };
    let step = legend.entries.len() == legend.breaks.len() + 1;
    let id = layer.get("id").and_then(Value::as_str).unwrap_or("layer").to_string();
    let get = json!(["get", prop]);
    let mut out = Vec::with_capacity(legend.entries.len());
    for (i, e) in legend.entries.iter().enumerate() {
        // step: entry i is [breaks[i-1], breaks[i]); interpolate stops:
        // entry i is [stop i, stop i+1).
        let (lo, hi) = if step { (i.checked_sub(1).and_then(|j| legend.breaks.get(j)).copied(), legend.breaks.get(i).copied()) } else { (if i == 0 { None } else { legend.breaks.get(i).copied() }, legend.breaks.get(i + 1).copied()) };
        let mut range = Vec::new();
        if let Some(lo) = lo {
            range.push(json!([">=", get, lo]));
        }
        if let Some(hi) = hi {
            range.push(json!(["<", get, hi]));
        }
        let mut l = layer.clone();
        let mut all = vec![json!("all")];
        if let Some(f) = layer.get("filter") {
            all.push(f.clone());
        }
        all.extend(range);
        if all.len() > 1 {
            l.insert("filter".into(), Value::Array(all));
        }
        l.insert("id".into(), json!(format!("{id}{CLASS_SUFFIX}{i}")));
        let mut p = paint.clone();
        p.insert(key.into(), json!(e.color));
        l.insert("paint".into(), Value::Object(p));
        out.push(Value::Object(l));
    }
    out
}

/// A `fill-extrusion` layer as a flat `fill`.
fn flatten_extrusion(layer: &Map<String, Value>) -> Map<String, Value> {
    let mut out = layer.clone();
    out.insert("type".into(), json!("fill"));
    let paint = layer.get("paint").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut flat = Map::new();
    if let Some(c) = paint.get("fill-extrusion-color") {
        flat.insert("fill-color".into(), c.clone());
    }
    if let Some(o) = paint.get("fill-extrusion-opacity") {
        flat.insert("fill-opacity".into(), o.clone());
    }
    out.insert("paint".into(), Value::Object(flat));
    out
}

/// The colour property of a layer's paint.
fn colour_of(layer: &Value) -> Option<&Value> {
    let paint = layer.get("paint")?;
    ["fill-color", "fill-extrusion-color", "line-color", "circle-color"].iter().find_map(|k| paint.get(*k))
}

/// The legend: a layer named `*-legend` (the publishers' convention for
/// the browser's legend) first, then the first layer whose colour is an
/// expression on a property, then the first plain colour.
pub fn legend_of(layers: &[Value]) -> Option<Legend> {
    let id = |l: &Value| l.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let mut ordered: Vec<&Value> = layers.iter().filter(|l| id(l).ends_with("-legend")).collect();
    ordered.extend(layers.iter().filter(|l| !id(l).ends_with("-legend")));
    for l in &ordered {
        if let Some(legend) = colour_of(l).and_then(expression_legend) {
            return Some(legend);
        }
    }
    ordered.iter().filter_map(|l| colour_of(l)).find_map(|c| c.as_str()).map(|c| Legend { kind: LegendKind::Single, property: None, breaks: Vec::new(), entries: vec![LegendEntry { label: String::new(), color: c.to_string() }] })
}

/// The property an input expression reads: `["get", "p"]`, possibly
/// wrapped (`["to-number", ["get", "p"], -1]`, `["coalesce", ...]`).
fn property_of(v: &Value) -> Option<String> {
    let a = v.as_array()?;
    if a.first().and_then(Value::as_str) == Some("get") {
        return a.get(1).and_then(Value::as_str).map(str::to_string);
    }
    a.iter().skip(1).find_map(property_of)
}

fn label_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Array(a) => a.iter().map(label_of).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn expression_legend(c: &Value) -> Option<Legend> {
    let a = c.as_array()?;
    let op = a.first()?.as_str()?;
    match op {
        "match" => {
            // ["match", input, label, colour, ..., fallback]
            if a.len() < 5 {
                return None;
            }
            let property = property_of(a.get(1)?);
            let mut entries = Vec::new();
            let pairs = &a[2..a.len().saturating_sub(1)];
            for pair in pairs.chunks(2) {
                if let [label, Value::String(color)] = pair {
                    entries.push(LegendEntry { label: label_of(label), color: color.clone() });
                }
            }
            (!entries.is_empty()).then_some(Legend { kind: LegendKind::Categories, property, breaks: Vec::new(), entries })
        }
        "step" => {
            let property = property_of(a.get(1)?);
            let first = a.get(2)?.as_str()?.to_string();
            let mut breaks = Vec::new();
            let mut colours = vec![first];
            for pair in a[3..].chunks(2) {
                if let [Value::Number(n), Value::String(color)] = pair {
                    breaks.push(n.as_f64().unwrap_or(0.0));
                    colours.push(color.clone());
                }
            }
            let entries = colours
                .into_iter()
                .enumerate()
                .map(|(i, color)| {
                    let label = match (i.checked_sub(1).and_then(|j| breaks.get(j)), breaks.get(i)) {
                        (None, Some(hi)) => format!("< {}", num(*hi)),
                        (Some(lo), Some(hi)) => format!("{}–{}", num(*lo), num(*hi)),
                        (Some(lo), None) => format!("≥ {}", num(*lo)),
                        (None, None) => String::new(),
                    };
                    LegendEntry { label, color }
                })
                .collect();
            Some(Legend { kind: LegendKind::Classes, property, breaks, entries })
        }
        "interpolate" | "interpolate-hcl" | "interpolate-lab" => {
            let input = a.get(2)?;
            // A zoom ramp is not a legend.
            if input.as_array().and_then(|x| x.first()).and_then(Value::as_str) == Some("zoom") {
                return None;
            }
            let property = property_of(input)?;
            let mut breaks = Vec::new();
            let mut entries = Vec::new();
            for pair in a[3..].chunks(2) {
                if let [Value::Number(n), Value::String(color)] = pair {
                    let x = n.as_f64().unwrap_or(0.0);
                    breaks.push(x);
                    entries.push(LegendEntry { label: num(x), color: color.clone() });
                }
            }
            (!entries.is_empty()).then_some(Legend { kind: LegendKind::Classes, property: Some(property), breaks, entries })
        }
        "case" => None,
        _ => None,
    }
}

fn num(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_named_buurten_still_resolves_and_the_steps_are_the_legend() {
        let url = "https://data.source.coop/cholmes/portolan-nl/cbs/wijken_buurten/styles/by-population-density.json";
        let s = OverlayStyle::parse(url, include_bytes!("../tests/fixtures/styles/wijken_buurten_by-population-density.json")).unwrap();
        assert_eq!(s.pmtiles_url.as_deref(), Some("https://data.source.coop/cholmes/portolan-nl/cbs/wijken_buurten/wijken_buurten.pmtiles"));
        assert!(s.warnings.iter().any(|w| w.code == "style_source"), "{:?}", s.warnings);
        assert_eq!(s.layer_ids().first().map(String::as_str), Some("buurten-water"));
        assert!(s.layers.as_array().unwrap().iter().all(|l| l.get("source").is_none()));
        let legend = s.legend.expect("a legend");
        assert_eq!(legend.kind, LegendKind::Classes);
        assert_eq!(legend.property.as_deref(), Some("bevolkingsdichtheid_inwoners_per_km2"));
        assert_eq!(legend.breaks, [30.0, 130.0, 2100.0, 5200.0, 8100.0]);
        assert_eq!(legend.entries.first().unwrap().label, "< 30");
        assert_eq!(legend.entries.last().unwrap().label, "≥ 8100");
    }

    #[test]
    fn a_colour_step_on_a_property_draws_as_one_layer_per_class() {
        let url = "https://data.source.coop/cholmes/portolan-nl/cbs/wijken_buurten/styles/by-population-density.json";
        let s = OverlayStyle::parse(url, include_bytes!("../tests/fixtures/styles/wijken_buurten_by-population-density.json")).unwrap();
        let ids = s.layer_ids();
        assert_eq!(ids.len(), 1 + 6 + 1, "{ids:?}");
        assert_eq!(class_of_layer(&ids[3]), Some(2));
        let l = &s.layers.as_array().unwrap()[3];
        assert_eq!(l["paint"]["fill-color"], "#feb24c");
        assert_eq!(l["filter"], json!(["all", ["!=", ["get", "water"], "JA"], [">=", ["get", "bevolkingsdichtheid_inwoners_per_km2"], 130.0], ["<", ["get", "bevolkingsdichtheid_inwoners_per_km2"], 2100.0]]));
        let first = &s.layers.as_array().unwrap()[1];
        assert_eq!(first["filter"].as_array().unwrap().len(), 3, "the lowest class has no lower bound");
        assert_eq!(class_of_layer("buurten-outline"), None);
    }

    #[test]
    fn extrusions_draw_flat_and_labels_are_skipped() {
        let url = "https://data.source.coop/cholmes/portolan-nl/tudelft/3dbag/styles/by-height.json";
        let s = OverlayStyle::parse(url, include_bytes!("../tests/fixtures/styles/3dbag_by-height.json")).unwrap();
        assert_eq!(s.pmtiles_url.as_deref(), Some("https://data.source.coop/cholmes/portolan-nl/tudelft/3dbag/3dbag-lod13.pmtiles"));
        assert!(!s.flattened.is_empty(), "the extrusion is drawn flat");
        for l in s.layers.as_array().unwrap() {
            assert!(DRAWN.contains(&l["type"].as_str().unwrap()), "{l}");
        }
        let flat = s.layers.as_array().unwrap().iter().find(|l| l["id"].as_str().is_some_and(|id| id.starts_with(s.flattened[0].as_str()))).unwrap();
        assert!(flat["paint"].get("fill-color").is_some());
        let legend = s.legend.unwrap();
        assert_eq!(legend.property.as_deref(), Some("height"));

        let stops = OverlayStyle::parse("https://data.source.coop/cholmes/trimet/stops/styles/labeled.json", include_bytes!("../tests/fixtures/styles/trimet_stops_labeled.json")).unwrap();
        assert!(stops.skipped.iter().any(|(_, t)| t == "symbol"), "{:?}", stops.skipped);
        let legend = stops.legend.unwrap();
        assert_eq!(legend.kind, LegendKind::Categories);
        assert_eq!(legend.property.as_deref(), Some("type"));
        assert_eq!(legend.entries[0], LegendEntry { label: "BUS".into(), color: "#4679AA".into() });
    }

    #[test]
    fn a_single_colour_is_a_one_entry_legend_and_a_zoom_ramp_is_not_a_legend() {
        let style = json!({"sources": {"data": {"type": "vector", "url": "pmtiles://../a.pmtiles"}}, "layers": [
            {"id": "a", "type": "line", "source": "data", "source-layer": "a", "paint": {"line-color": ["interpolate", ["linear"], ["zoom"], 5, "#000", 10, "#fff"]}},
            {"id": "b", "type": "fill", "source": "data", "source-layer": "a", "paint": {"fill-color": "#2a9d8f"}},
            {"id": "bg", "type": "background"},
            {"id": "other", "type": "fill", "source": "elsewhere", "source-layer": "x"}
        ]});
        let s = OverlayStyle::from_style("https://h.org/c/styles/default.json", &style);
        assert_eq!(s.pmtiles_url.as_deref(), Some("https://h.org/c/a.pmtiles"));
        let legend = s.legend.clone().unwrap();
        assert_eq!(legend.kind, LegendKind::Single);
        assert_eq!(legend.entries[0].color, "#2a9d8f");
        assert_eq!(s.skipped, [("bg".to_string(), "background".to_string()), ("other".to_string(), "fill".to_string())]);
        assert_eq!(s.source_layers(), ["a"]);
    }
}
