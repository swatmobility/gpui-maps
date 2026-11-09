//! A story overlay's GeoJSON split into one source per overlay layer
//! (spec §2.2, §3): each [`LayerSource`] holds exactly the features its
//! layer draws, every one with an integer `id` so a per-feature effect
//! (`highlight`, `drop`, `pulse`, ruling D1) can name it.
//!
//! **Pure.** JSON in, JSON out: no clock, no I/O, no hash-map iteration
//! order. The browser and iOS shells include this file unchanged.
//!
//! **Filters.** A layer's `filter` is compiled by
//! [`crate::style_spec::filter::compile_filter`], the compiler the basemap
//! uses, and evaluated against each feature's properties AND its
//! geometry class, so `["geometry-type"]` works exactly as it does on a
//! vector tile (`Filter::matches_geometry`).
//!
//! **The antimeridian.** Every line's longitudes are unwrapped so
//! consecutive vertices differ by at most 180 degrees: a route from
//! 179.5 to -179.5 is written 179.5 -> 180.5, the short way, so the line,
//! its `line-progress` and [`point_at_fraction`] all agree.

use serde_json::{Map, Value};

use super::script::{Diagnostic, OverlayLayerKind, OverlaySpec, Severity};
use crate::mvt::{TagLookup, TagValue};
use crate::style::GeometryKind;
use crate::style_spec::expression::Scope;

/// The paint keys an overlay layer passes on to `overlay::SourcePaint`.
/// Every other key is listed in [`LayerSource::unsupported_paint`] (a
/// validation warning, not a refusal). `line-cap` and `line-join` (spec
/// §6.4) are layer-wide strokes, as MapLibre's layout keys of the same
/// names; absent, a line keeps the overlay's butt ends and bevel joins.
pub const SUPPORTED_PAINT: &[&str] = &[
    "line-color",
    "line-width",
    "line-opacity",
    "line-gradient",
    "line-cap",
    "line-join",
    "fill-color",
    "fill-opacity",
    "circle-color",
    "circle-radius",
    // Spec §6.4: a story circle layer's marker paint (the capture draws a
    // layer with any of these six as markers, `capture::story_overlays`)
    // and its point labels' text.
    "circle-stroke-width",
    "circle-stroke-color",
    "circle-blur",
    "circle-translate",
    "circle-opacity",
    "circle-stroke-opacity",
    "text-field",
    "text-size",
    "text-color",
    "text-halo-color",
    "text-halo-width",
    "text-offset",
];

/// A story layer's `text-field` for one feature (ruling R8-a): a string
/// as written, or `["get", <property>]` (also inside `["to-string", ..]`)
/// read from the feature's properties -- a string, or a number or bool
/// written out. `None` when it reads nothing (the feature has no label).
/// One reading for the capture's labels and the live map's source.
#[must_use]
pub fn text_field_for(field: &Value, properties: Option<&Map<String, Value>>) -> Option<String> {
    let value = match field {
        Value::String(s) => return (!s.trim().is_empty()).then(|| s.clone()),
        Value::Array(a) => match (a.first().and_then(Value::as_str), a.get(1), a.len()) {
            (Some("get"), Some(Value::String(key)), 2) => properties?.get(key)?,
            (Some("to-string"), Some(inner), 2) => return text_field_for(inner, properties),
            _ => return None,
        },
        _ => return None,
    };
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Whether `field` is a `text-field` [`text_field_for`] reads: a string,
/// or `["get", <property>]` (optionally in `["to-string", ..]`).
#[must_use]
pub fn text_field_supported(field: &Value) -> bool {
    match field {
        Value::String(_) => true,
        Value::Array(a) => match (a.first().and_then(Value::as_str), a.get(1), a.len()) {
            (Some("get"), Some(Value::String(_)), 2) => true,
            (Some("to-string"), Some(inner), 2) => text_field_supported(inner),
            _ => false,
        },
        _ => false,
    }
}

/// The overlay source a story layer is pushed to on a LIVE map (the
/// native preview, `MapView::apply_story_layer_fx`): `story:<layer id>`,
/// a prefix no host source of its own is expected to use.
#[must_use]
pub fn live_source_name(layer_id: &str) -> String {
    format!("story:{layer_id}")
}

/// One story overlay layer, ready to be an overlay source of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerSource {
    pub layer_id: String,
    pub overlay_id: String,
    pub kind: OverlayLayerKind,
    pub line_metrics: bool,
    /// A FeatureCollection of the features this layer draws: geometry type matches `kind`
    /// (line: LineString/MultiLineString, and polygon outlines; fill: Polygon/MultiPolygon;
    /// circle: Point/MultiPoint), `filter` passes, and every feature has an integer `id`:
    /// ALWAYS its index in the source document (review round 1, F2/F3), so ids are dense
    /// and unique and overrides can name it. The author's own GeoJSON `id` is kept in
    /// [`source_ids`](Self::source_ids).
    pub geojson: String,
    /// The layer's paint as `overlay::SourcePaint` declares it (`line-color`, `line-width`,
    /// `line-opacity`, `line-gradient`, `fill-color`, `fill-opacity`, `circle-color`,
    /// `circle-radius`); other keys are listed in `unsupported_paint` for a validation warning.
    pub paint: serde_json::Map<String, serde_json::Value>,
    pub unsupported_paint: Vec<String>,
    pub feature_count: usize,
    /// Feature ids in document order (the drop stagger order): each is the
    /// feature's index in the source document, so strictly increasing.
    pub feature_ids: Vec<u64>,
    /// Aligned with `feature_ids`: the author's GeoJSON `id` (a string or
    /// a number), `None` when the feature had none. What a `highlight`'s
    /// `ids` match against.
    pub source_ids: Vec<Option<Value>>,
    /// Aligned with `feature_ids`: each feature's colour as the renderer
    /// resolves the layer's declared `line-color`/`fill-color`/
    /// `circle-color` (`overlay::SourcePaint`, the same evaluator, so the
    /// two cannot drift), WITHOUT the layer's `*-opacity`. With no colour
    /// declared: the feature's own `<key>` property if it is a colour,
    /// else black (MapLibre's default). `None`: the declared colour does
    /// not resolve for this feature (or is refused), so a highlight has no
    /// base to ramp from. What a `highlight` mixes from.
    pub base_colors: Vec<Option<[u8; 4]>>,
    /// A line layer's FIRST LineString, `(lon, lat)` unwrapped as in
    /// `geojson`: what [`point_at_fraction`] walks, parsed once here so a
    /// draw head (one per frame) never re-reads `geojson`. Empty for a
    /// fill or circle layer, or a line layer with no line.
    pub first_line: Vec<(f64, f64)>,
    /// Aligned with `first_line`: the Web Mercator length (world units)
    /// from its first vertex to each vertex, so [`point_at_fraction`] is a
    /// binary search, not a walk.
    pub first_line_lengths: Vec<f64>,
    /// Draw over the 3D buildings ([`crate::story::script::OverlayLayerSpec::above_buildings`],
    /// resolved): the capture's above-buildings pass and the live map's.
    pub above_buildings: bool,
}

impl LayerSource {
    /// A layer with no features: what an effect on a layer whose overlay
    /// data is not loaded (yet) is evaluated against. Every per-feature
    /// effect is then empty, and a `draw` has no head.
    pub fn empty(layer_id: &str, kind: OverlayLayerKind) -> Self {
        LayerSource {
            layer_id: layer_id.to_string(),
            overlay_id: String::new(),
            kind,
            line_metrics: false,
            geojson: r#"{"type":"FeatureCollection","features":[]}"#.to_string(),
            paint: Map::new(),
            unsupported_paint: Vec::new(),
            feature_count: 0,
            feature_ids: Vec::new(),
            source_ids: Vec::new(),
            base_colors: Vec::new(),
            first_line: Vec::new(),
            first_line_lengths: Vec::new(),
            above_buildings: kind.above_buildings_by_default(),
        }
    }
}

/// A feature's properties as the filter compiler's tag bag. Sorted by key
/// (a `Vec` searched linearly: overlay properties are few), so nothing
/// depends on hash order.
#[derive(Debug)]
pub(crate) struct PropertyBag(Vec<(String, TagValue)>);

impl PropertyBag {
    pub(crate) fn of(properties: Option<&Value>) -> Self {
        let mut tags = Vec::new();
        if let Some(Value::Object(map)) = properties {
            for (k, v) in map {
                let tag = match v {
                    Value::String(s) => TagValue::String(s.clone()),
                    Value::Number(n) => match n.as_f64() {
                        Some(f) => TagValue::Number(f),
                        None => continue,
                    },
                    Value::Bool(b) => TagValue::Bool(*b),
                    // MVT has no null, array or object value: like a tile
                    // feature, such a property is simply absent.
                    _ => continue,
                };
                tags.push((k.clone(), tag));
            }
        }
        tags.sort_by(|a, b| a.0.cmp(&b.0));
        PropertyBag(tags)
    }
}

impl TagLookup for PropertyBag {
    fn get(&self, key: &str) -> Option<&TagValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// A GeoJSON geometry's style-spec class, `None` for anything else.
pub(crate) fn geometry_kind(geometry: &Value) -> Option<GeometryKind> {
    GeometryKind::from_style_name(geometry.get("type")?.as_str()?)
}

/// A compiled layer filter, or a diagnostic at `path`.
pub(crate) fn compile_layer_filter(filter: &Value, path: &str) -> Result<crate::style::Filter, Diagnostic> {
    crate::style_spec::filter::compile_filter(filter, Scope::root()).map_err(|e| Diagnostic {
        severity: Severity::Error,
        path: path.to_string(),
        code: "bad_filter",
        message: format!("the filter does not compile: {e}"),
        hint: None,
    })
}

/// A feature's own GeoJSON `id` when it is a string or a number (the two
/// kinds GeoJSON allows), else `None`.
fn source_id(feature: &Value) -> Option<Value> {
    feature.get("id").filter(|id| id.is_string() || id.is_number()).cloned()
}

/// Whether an `ids` entry names the feature at document index `index`
/// whose own id is `id`: a string matches a string exactly, a number a
/// number numerically (7 matches 7.0), with no coercion between them
/// ("250" never matches 250). `index_fallback`: no feature of the source
/// has an id at all, so a number addresses a feature by its document
/// index (ruling F2b); when any feature has an id, a feature without one
/// cannot be named by `ids` (use `where`).
pub fn id_matches(entry: &Value, id: Option<&Value>, index: u64, index_fallback: bool) -> bool {
    match (entry, id) {
        (Value::String(a), Some(Value::String(b))) => a == b,
        (Value::Number(a), Some(Value::Number(b))) => a.as_f64().is_some() && a.as_f64() == b.as_f64(),
        (Value::Number(a), None) => index_fallback && a.as_f64() == Some(index as f64),
        _ => false,
    }
}

/// Whether a paint key holds a colour (or a colour ramp): where a CSS
/// named colour is accepted.
fn is_colour_key(key: &str) -> bool {
    key.ends_with("-color") || key == "line-gradient"
}

/// The paint key a layer kind colours with.
pub(crate) fn color_key(kind: OverlayLayerKind) -> &'static str {
    match kind {
        OverlayLayerKind::Line => "line-color",
        OverlayLayerKind::Fill => "fill-color",
        OverlayLayerKind::Circle => "circle-color",
    }
}

fn color_slot(kind: OverlayLayerKind) -> crate::overlay::source_paint::ColorSlot {
    use crate::overlay::source_paint::ColorSlot;
    match kind {
        OverlayLayerKind::Line => ColorSlot::Line,
        OverlayLayerKind::Fill => ColorSlot::Fill,
        OverlayLayerKind::Circle => ColorSlot::Circle,
    }
}

fn bytes(c: crate::style::Rgba) -> [u8; 4] {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(c.r), byte(c.g), byte(c.b), byte(c.a)]
}

/// Every feature's base colour ([`LayerSource::base_colors`]), resolved
/// once by the renderer's own `SourcePaint` over the colour key alone.
fn base_colors(kind: OverlayLayerKind, paint: &Map<String, Value>, properties: &[Map<String, Value>]) -> Vec<Option<[u8; 4]>> {
    let key = color_key(kind);
    let Some(declared) = paint.get(key) else {
        // Undeclared: the feature's own `<key>` property, else black.
        return properties
            .iter()
            .map(|p| Some(p.get(key).and_then(Value::as_str).and_then(|s| crate::style_spec::parse_css_color(s).ok()).map_or([0, 0, 0, 255], bytes)))
            .collect();
    };
    let mut only = Map::new();
    only.insert(key.to_string(), declared.clone());
    let Ok(compiled) = crate::overlay::source_paint::SourcePaint::compile(&only) else { return vec![None; properties.len()] };
    let table = crate::overlay::columns::PropertyTable::from_json(properties.iter());
    match compiled.resolve_rows(color_slot(kind), &table) {
        Some(rows) => rows.into_iter().map(|c| c.map(bytes)).collect(),
        None => vec![None; properties.len()],
    }
}

/// Cumulative Web Mercator length at each vertex of `line`.
fn cumulative_lengths(line: &[(f64, f64)]) -> Vec<f64> {
    let mut out = Vec::with_capacity(line.len());
    let mut total = 0.0;
    let mut prev: Option<(f64, f64)> = None;
    for &(lon, lat) in line {
        let w = crate::overlay::geom::lon_lat_to_world(lon, lat);
        if let Some(p) = prev {
            total += ((w.0 - p.0).powi(2) + (w.1 - p.1).powi(2)).sqrt();
        }
        out.push(total);
        prev = Some(w);
    }
    out
}

/// `[lon, lat]` positions, unwrapped so consecutive longitudes differ by
/// at most 180 degrees; `None` if any position is malformed.
fn unwrap_line(coords: &Value) -> Option<Vec<(f64, f64)>> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    for p in coords.as_array()? {
        let p = p.as_array()?;
        let (lon, lat) = (p.first()?.as_f64()?, p.get(1)?.as_f64()?);
        let lon = match out.last() {
            Some(&(prev, _)) => lon + ((prev - lon) / 360.0).round() * 360.0,
            None => lon,
        };
        out.push((lon, lat));
    }
    Some(out)
}

fn line_json(line: &[(f64, f64)]) -> Value {
    Value::Array(line.iter().map(|(lon, lat)| serde_json::json!([lon, lat])).collect())
}

/// A geometry to write, and its first line (unwrapped), if it has one.
type LayerGeometry = (Value, Option<Vec<(f64, f64)>>);

/// The geometry a `kind` layer draws for `geometry`, lines unwrapped and
/// polygons turned into their outlines for a line layer; `None` when this
/// layer does not draw that geometry type (or it is malformed).
fn geometry_for(kind: OverlayLayerKind, geometry: &Value) -> Option<LayerGeometry> {
    let ty = geometry.get("type")?.as_str()?;
    let coords = geometry.get("coordinates")?;
    let lines = |parts: &Value| -> Option<Vec<Vec<(f64, f64)>>> { parts.as_array()?.iter().map(unwrap_line).collect() };
    match (kind, ty) {
        (OverlayLayerKind::Line, "LineString") => {
            let line = unwrap_line(coords)?;
            Some((serde_json::json!({"type": "LineString", "coordinates": line_json(&line)}), Some(line)))
        }
        (OverlayLayerKind::Line, "MultiLineString" | "Polygon") => {
            let parts = lines(coords)?;
            let first = parts.first().cloned();
            let json: Vec<Value> = parts.iter().map(|l| line_json(l)).collect();
            Some((serde_json::json!({"type": "MultiLineString", "coordinates": json}), first))
        }
        (OverlayLayerKind::Line, "MultiPolygon") => {
            let mut parts = Vec::new();
            for polygon in coords.as_array()? {
                parts.extend(lines(polygon)?);
            }
            let first = parts.first().cloned();
            let json: Vec<Value> = parts.iter().map(|l| line_json(l)).collect();
            Some((serde_json::json!({"type": "MultiLineString", "coordinates": json}), first))
        }
        (OverlayLayerKind::Fill, "Polygon" | "MultiPolygon") | (OverlayLayerKind::Circle, "Point" | "MultiPoint") => Some((geometry.clone(), None)),
        _ => None,
    }
}

/// Split `doc` (a parsed FeatureCollection) by `layers`. LineString longitudes are unwrapped
/// so consecutive vertices differ by at most 180 degrees (the short way across the antimeridian).
///
/// A bare `Feature` is read as a collection of one. Anything else, or a
/// layer filter that does not compile, is a diagnostic.
///
/// `overlay_index` is `overlay`'s position in `scene.overlays`: every
/// diagnostic here is a JSON pointer built from it and the layer's own
/// position in `overlay.layers`, like every other diagnostic in the
/// script (review Task 4 #8), never from the author's `id` strings (which
/// need not be unique and are not valid pointer segments).
pub fn layer_sources(overlay_index: usize, overlay: &OverlaySpec, doc: &serde_json::Value) -> Result<Vec<LayerSource>, Diagnostic> {
    let path = format!("/scene/overlays/{overlay_index}");
    let single;
    let features: &[Value] = match doc.get("type").and_then(Value::as_str) {
        Some("FeatureCollection") => doc.get("features").and_then(Value::as_array).map_or(&[], Vec::as_slice),
        Some("Feature") => {
            single = [doc.clone()];
            &single
        }
        _ => {
            return Err(Diagnostic {
                severity: Severity::Error,
                path,
                code: "overlay_not_features",
                message: format!("overlay `{}`'s data is not a GeoJSON FeatureCollection or Feature", overlay.id),
                hint: Some("wrap the geometries in {\"type\": \"FeatureCollection\", \"features\": [...]}".to_string()),
            });
        }
    };
    let mut out = Vec::with_capacity(overlay.layers.len());
    for (layer_index, layer) in overlay.layers.iter().enumerate() {
        let filter = match &layer.filter {
            Some(f) if !f.is_null() => Some(compile_layer_filter(f, &format!("{path}/layers/{layer_index}/filter"))?),
            _ => None,
        };
        let mut kept = Vec::new();
        let mut ids = Vec::new();
        let mut source_ids = Vec::new();
        let mut kept_properties: Vec<Map<String, Value>> = Vec::new();
        let mut first_line: Vec<(f64, f64)> = Vec::new();
        for (index, feature) in features.iter().enumerate() {
            let Some(geometry) = feature.get("geometry").filter(|g| !g.is_null()) else { continue };
            let Some((geometry_out, line)) = geometry_for(layer.kind, geometry) else { continue };
            let properties = feature.get("properties").filter(|p| p.is_object()).cloned().unwrap_or_else(|| Value::Object(Map::new()));
            if let Some(filter) = &filter {
                if !filter.matches_geometry(&PropertyBag::of(Some(&properties)), 0.0, geometry_kind(geometry)) {
                    continue;
                }
            }
            let id = index as u64;
            if first_line.is_empty() {
                if let Some(line) = line {
                    first_line = line;
                }
            }
            ids.push(id);
            source_ids.push(source_id(feature));
            kept_properties.push(properties.as_object().cloned().unwrap_or_default());
            kept.push(serde_json::json!({"type": "Feature", "id": id, "properties": properties, "geometry": geometry_out}));
        }
        // Ruling R8-a: a `text-field` the capture and the live map both
        // read, or refused here for both.
        if let Some(field) = layer.paint.get("text-field").filter(|f| !text_field_supported(f)) {
            return Err(Diagnostic {
                severity: Severity::Error,
                path: format!("{path}/layers/{layer_index}/paint/text-field"),
                code: "text_field_unsupported",
                message: format!(
                    "layer `{}`: its `text-field` {field} is not one a story label can read. Use a plain string, or [\"get\", \"<property>\"] to label each point with one of its properties.",
                    layer.id
                ),
                hint: Some("for example \"text-field\": [\"get\", \"name\"]".to_string()),
            });
        }
        let mut paint = Map::new();
        let mut unsupported_paint = Vec::new();
        for (k, v) in &layer.paint {
            if SUPPORTED_PAINT.contains(&k.as_str()) {
                // Named colours (`"red"`) in EVERY colour key, not only
                // `line-gradient`: spelled out once here, so the renderer,
                // the base colours and the cache key all see one form.
                let v = if is_colour_key(k) { crate::overlay::source_paint::with_named_colours_spelled_out(v) } else { v.clone() };
                paint.insert(k.clone(), v);
            } else {
                unsupported_paint.push(k.clone());
            }
        }
        let base_colors = base_colors(layer.kind, &paint, &kept_properties);
        out.push(LayerSource {
            layer_id: layer.id.clone(),
            overlay_id: overlay.id.clone(),
            kind: layer.kind,
            line_metrics: overlay.line_metrics,
            geojson: serde_json::json!({"type": "FeatureCollection", "features": kept}).to_string(),
            paint,
            unsupported_paint,
            feature_count: ids.len(),
            feature_ids: ids,
            source_ids,
            base_colors,
            first_line_lengths: if layer.kind == OverlayLayerKind::Line { cumulative_lengths(&first_line) } else { Vec::new() },
            first_line: if layer.kind == OverlayLayerKind::Line { first_line } else { Vec::new() },
            above_buildings: layer.above_buildings(),
        });
    }
    Ok(out)
}

/// Every feature in a GeoJSON document, a `FeatureCollection`'s array (a
/// bare `Feature` is one, anything else none): what a `too_many_features`
/// warning (spec §3.3) counts for the WHOLE overlay, not a layer's own
/// (possibly filtered) [`LayerSource::feature_count`], so a layer's filter
/// cannot hide how much data an author shipped.
#[must_use]
pub fn document_feature_count(doc: &Value) -> usize {
    match doc.get("type").and_then(Value::as_str) {
        Some("FeatureCollection") => doc.get("features").and_then(Value::as_array).map_or(0, Vec::len),
        Some("Feature") => 1,
        _ => 0,
    }
}

/// `(lon, lat)` at `fraction` (0..1) of a line layer's FIRST LineString, by geodesic-free
/// Mercator length (the length `line-progress` measures).
///
/// Measured in Web Mercator world units (`overlay::geom::lon_lat_to_world`),
/// the unit the tessellator's line distance is in, so the head sits exactly
/// at the trim end. The longitude is unwrapped like the line's own.
pub fn point_at_fraction(source: &LayerSource, fraction: f64) -> Option<(f64, f64)> {
    if source.kind != OverlayLayerKind::Line || !fraction.is_finite() {
        return None;
    }
    let line = &source.first_line;
    let lengths = &source.first_line_lengths;
    let first = *line.first()?;
    let total = lengths.last().copied().unwrap_or(0.0);
    if lengths.len() != line.len() || total.is_nan() || total <= 0.0 {
        return Some(first);
    }
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction >= 1.0 {
        return line.last().copied();
    }
    let target = fraction * total;
    // The first vertex at or past `target`; the segment ends there. A
    // zero-length segment is never chosen: its end has the same length
    // as its start, which the search finds first.
    let end = lengths.partition_point(|l| *l < target).max(1);
    let (a, b) = (line.get(end - 1)?, line.get(end)?);
    let (la, lb) = (lengths.get(end - 1)?, lengths.get(end)?);
    let f = if lb > la { (target - la) / (lb - la) } else { 0.0 };
    let (wa, wb) = (crate::overlay::geom::lon_lat_to_world(a.0, a.1), crate::overlay::geom::lon_lat_to_world(b.0, b.1));
    let (x, y) = (wa.0 + (wb.0 - wa.0) * f, wa.1 + (wb.1 - wa.1) * f);
    let size = crate::render::WORLD_SIZE as f64;
    Some(crate::geo::tile_xy_to_lon_lat(x / size, y / size, 0.0))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {

    /// Ruling R8-a: a `text-field` both paths read is accepted; any other
    /// expression is refused, for the capture and the live map alike.
    #[test]
    fn a_text_field_expression_is_read_or_refused_on_both_paths() {
        let props: Map<String, Value> = serde_json::from_value(serde_json::json!({"name": "Port", "n": 3})).unwrap();
        assert_eq!(text_field_for(&serde_json::json!(["get", "name"]), Some(&props)).as_deref(), Some("Port"));
        assert_eq!(text_field_for(&serde_json::json!(["to-string", ["get", "n"]]), Some(&props)).as_deref(), Some("3"));
        assert_eq!(text_field_for(&serde_json::json!("Stop"), None).as_deref(), Some("Stop"));
        assert_eq!(text_field_for(&serde_json::json!(["get", "missing"]), Some(&props)), None);
        let overlay: OverlaySpec = serde_json::from_value(serde_json::json!({"id": "o", "data": "o.geojson",
            "layers": [{"id": "dots", "type": "circle", "paint": {"text-field": ["concat", ["get", "name"], "!"]}}]})).unwrap();
        let doc = serde_json::json!({"type": "FeatureCollection", "features": []});
        let err = layer_sources(0, &overlay, &doc).unwrap_err();
        assert_eq!(err.code, "text_field_unsupported");
        assert!(err.message.contains("[\"get\", \"<property>\"]"), "{}", err.message);
    }

    use super::*;

    /// An overlay `routes` with one line layer `route-line` (line metrics on).
    pub(crate) fn overlay_with_line_layer() -> OverlaySpec {
        serde_json::from_value(serde_json::json!({
            "id": "routes", "data": "routes.geojson", "line_metrics": true,
            "layers": [{"id": "route-line", "type": "line", "paint": {"line-color": "#e33", "line-width": 3}}]
        }))
        .unwrap()
    }

    /// Task 22: `line` and `circle` layers draw over the 3D buildings by
    /// default, `fill` under them; `above_buildings` overrides either way.
    #[test]
    fn above_buildings_defaults_by_kind_and_is_overridable() {
        let overlay: OverlaySpec = serde_json::from_value(serde_json::json!({
            "id": "o", "data": "o.geojson",
            "layers": [{"id": "l", "type": "line"}, {"id": "c", "type": "circle"}, {"id": "f", "type": "fill"},
                       {"id": "l-under", "type": "line", "above_buildings": false},
                       {"id": "f-over", "type": "fill", "above_buildings": true}]
        }))
        .unwrap();
        let doc = serde_json::json!({"type": "FeatureCollection", "features": []});
        let got: Vec<(String, bool)> =
            layer_sources(0, &overlay, &doc).unwrap().into_iter().map(|s| (s.layer_id, s.above_buildings)).collect();
        let want = [("l", true), ("c", true), ("f", false), ("l-under", false), ("f-over", true)];
        assert_eq!(got, want.map(|(id, above)| (id.to_string(), above)));
        // A redacted manifest re-serialises the script: an unset value stays unset.
        let json = serde_json::to_value(&overlay.layers[0]).unwrap();
        assert!(json.get("above_buildings").is_none(), "{json}");
    }

    #[test]
    fn a_line_across_the_antimeridian_is_unwrapped_the_short_way() {
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": [[179.5, 0.0], [-179.5, 0.0]]}}]});
        let src = layer_sources(0, &overlay_with_line_layer(), &doc).unwrap().remove(0);
        let v: serde_json::Value = serde_json::from_str(&src.geojson).unwrap();
        let c = &v["features"][0]["geometry"]["coordinates"];
        assert_eq!(c[1][0].as_f64(), Some(180.5));
        let mid = point_at_fraction(&src, 0.5).unwrap();
        assert!((mid.0 - 180.0).abs() < 1e-6 || (mid.0 + 180.0).abs() < 1e-6);
    }

    #[test]
    fn layers_split_by_geometry_type_and_filter_and_every_feature_gets_an_id() {
        let overlay: OverlaySpec = serde_json::from_value(serde_json::json!({
            "id": "trip", "data": "trip.geojson",
            "layers": [
                {"id": "route-line", "type": "line", "paint": {"line-color": "#e33", "line-blur": 2}},
                {"id": "big-ports", "type": "circle", "filter": ["all", ["==", ["geometry-type"], "Point"], [">", ["get", "tons"], 100]],
                 "paint": {"circle-radius": 6}},
                {"id": "zones", "type": "fill", "filter": ["==", ["geometry-type"], "Polygon"]}
            ]
        }))
        .unwrap();
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {"tons": 500}, "geometry": {"type": "Point", "coordinates": [103.8, 1.3]}},
            {"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": [[103.8, 1.3], [139.7, 35.6]]}},
            {"type": "Feature", "properties": {"tons": 50}, "geometry": {"type": "Point", "coordinates": [139.7, 35.6]}},
            {"type": "Feature", "id": 42, "properties": {"tons": 900}, "geometry": {"type": "Point", "coordinates": [121.5, 25.0]}},
            {"type": "Feature", "properties": {}, "geometry": null}
        ]});
        let sources = layer_sources(0, &overlay, &doc).unwrap();
        assert_eq!(sources.len(), 3);
        let (line, ports, zones) = (&sources[0], &sources[1], &sources[2]);
        assert_eq!((line.layer_id.as_str(), line.feature_count, line.feature_ids.clone()), ("route-line", 1, vec![1]));
        assert_eq!(line.first_line, vec![(103.8, 1.3), (139.7, 35.6)]);
        assert_eq!(line.unsupported_paint, vec!["line-blur".to_string()]);
        assert!(line.paint.contains_key("line-color") && !line.paint.contains_key("line-blur"));
        // The filter keeps 2 of 3 points; every id is the document index,
        // and the author's own id is kept beside it.
        assert_eq!(ports.feature_ids, vec![0, 3]);
        assert_eq!(ports.source_ids, vec![None, Some(serde_json::json!(42))]);
        assert_eq!(ports.feature_count, 2);
        let v: serde_json::Value = serde_json::from_str(&ports.geojson).unwrap();
        assert_eq!(v["features"][0]["id"], 0);
        assert_eq!(v["features"][1]["id"], 3);
        assert_eq!(v["features"][1]["properties"]["tons"], 900);
        assert!(ports.first_line.is_empty());
        assert_eq!(zones.feature_count, 0);
    }

    #[test]
    fn a_polygon_on_a_line_layer_is_its_outline() {
        let doc = serde_json::json!({"type": "Feature", "properties": {}, "geometry": {"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]}});
        let src = layer_sources(0, &overlay_with_line_layer(), &doc).unwrap().remove(0);
        let v: serde_json::Value = serde_json::from_str(&src.geojson).unwrap();
        assert_eq!(v["features"][0]["geometry"]["type"], "MultiLineString");
        assert_eq!(src.first_line.len(), 4);
    }

    #[test]
    fn a_bad_filter_or_a_bare_geometry_is_a_diagnostic() {
        let mut overlay = overlay_with_line_layer();
        overlay.layers[0].filter = Some(serde_json::json!(["frobnicate"]));
        let doc = serde_json::json!({"type": "FeatureCollection", "features": []});
        assert_eq!(layer_sources(0, &overlay, &doc).unwrap_err().code, "bad_filter");
        let bare = serde_json::json!({"type": "Point", "coordinates": [0, 0]});
        assert_eq!(layer_sources(0, &overlay_with_line_layer(), &bare).unwrap_err().code, "overlay_not_features");
    }

    #[test]
    fn point_at_fraction_walks_mercator_length() {
        let doc = serde_json::json!({"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": [[0, 0], [10, 0], [10, 0], [20, 0]]}});
        let src = layer_sources(0, &overlay_with_line_layer(), &doc).unwrap().remove(0);
        for (f, lon) in [(0.0, 0.0), (0.25, 5.0), (0.5, 10.0), (0.75, 15.0), (1.0, 20.0), (-1.0, 0.0), (7.0, 20.0)] {
            let p = point_at_fraction(&src, f).unwrap();
            assert!((p.0 - lon).abs() < 1e-9 && p.1.abs() < 1e-9, "{f}: {p:?}");
        }
        assert_eq!(point_at_fraction(&LayerSource::empty("x", OverlayLayerKind::Line), 0.5), None);
    }

    #[test]
    fn ids_match_by_type_strings_exactly_and_numbers_numerically() {
        let (n7, n7f, s7) = (serde_json::json!(7), serde_json::json!(7.0), serde_json::json!("7"));
        assert!(id_matches(&n7, Some(&n7f), 0, false), "7 matches 7.0");
        assert!(id_matches(&n7f, Some(&n7), 0, false));
        assert!(!id_matches(&s7, Some(&n7), 0, false), "no coercion: \"7\" is not 7");
        assert!(!id_matches(&serde_json::json!("250"), Some(&serde_json::json!(250)), 0, false));
        assert!(id_matches(&serde_json::json!("FRA"), Some(&serde_json::json!("FRA")), 0, false));
        assert!(!id_matches(&serde_json::json!("fra"), Some(&serde_json::json!("FRA")), 0, false));
        assert!(id_matches(&serde_json::json!(4), None, 4, true), "an id-less source: the document index");
        assert!(!id_matches(&serde_json::json!(4), None, 4, false), "a source with ids: an id-less feature is not addressable");
        assert!(!id_matches(&serde_json::json!("4"), None, 4, true));
        assert!(!id_matches(&serde_json::json!(4), Some(&serde_json::json!("x")), 4, true), "an explicit id hides the index");
    }

    #[test]
    fn base_colors_resolve_per_feature_once() {
        let overlay: OverlaySpec = serde_json::from_value(serde_json::json!({
            "id": "c", "data": "c.geojson",
            "layers": [
                {"id": "countries", "type": "fill", "paint": {"fill-color": ["match", ["get", "iso"], "FRA", "#0000ff", "#00ff00"], "fill-opacity": 0.5}},
                {"id": "dots", "type": "circle"}
            ]
        }))
        .unwrap();
        let square = serde_json::json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]});
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "id": "FRA", "properties": {"iso": "FRA"}, "geometry": square},
            {"type": "Feature", "id": "DEU", "properties": {"iso": "DEU"}, "geometry": square},
            {"type": "Feature", "properties": {"circle-color": "#ff0000"}, "geometry": {"type": "Point", "coordinates": [0, 0]}},
            {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [0, 0]}}
        ]});
        let sources = layer_sources(0, &overlay, &doc).unwrap();
        assert_eq!(sources[0].base_colors, vec![Some([0, 0, 255, 255]), Some([0, 255, 0, 255])], "the colour, without fill-opacity");
        assert_eq!(sources[1].base_colors, vec![Some([255, 0, 0, 255]), Some([0, 0, 0, 255])], "undeclared: own property, else black");
    }

    /// The story's base colour and the renderer's colour are one
    /// evaluation: `base_colors` equals `SourcePaint::resolve_rows` for the
    /// same paint over the same features (native: the renderer's table).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn base_colors_equal_the_renderers_source_paint() {
        use crate::overlay::columns::PropertyTable;
        use crate::overlay::source_paint::{ColorSlot, SourcePaint};
        let color = serde_json::json!(["case", [">", ["get", "tons"], 100], "#ff8800", ["==", ["get", "kind"], "hub"], "rgba(10, 20, 30, 0.5)", "#123456"]);
        let overlay: OverlaySpec = serde_json::from_value(serde_json::json!({
            "id": "p", "data": "p.geojson", "layers": [{"id": "ports", "type": "circle", "paint": {"circle-color": color.clone()}}]
        }))
        .unwrap();
        let rows: Vec<serde_json::Value> = vec![
            serde_json::json!({"tons": 500}),
            serde_json::json!({"tons": 5, "kind": "hub"}),
            serde_json::json!({"tons": 5}),
            serde_json::json!({}),
        ];
        let features: Vec<serde_json::Value> =
            rows.iter().map(|p| serde_json::json!({"type": "Feature", "properties": p, "geometry": {"type": "Point", "coordinates": [0, 0]}})).collect();
        let src = layer_sources(0, &overlay, &serde_json::json!({"type": "FeatureCollection", "features": features})).unwrap().remove(0);
        let mut paint = serde_json::Map::new();
        paint.insert("circle-color".to_string(), color);
        let maps: Vec<serde_json::Map<String, serde_json::Value>> = rows.iter().map(|r| r.as_object().cloned().unwrap()).collect();
        let renderer = SourcePaint::compile(&paint).unwrap().resolve_rows(ColorSlot::Circle, &PropertyTable::from_json(maps.iter())).unwrap();
        let renderer: Vec<Option<[u8; 4]>> = renderer.into_iter().map(|c| c.map(bytes)).collect();
        assert_eq!(src.base_colors, renderer);
        assert_eq!(src.base_colors[0], Some([255, 136, 0, 255]));
    }

    /// T6 review F6: a named colour works in EVERY colour key of a story
    /// layer's paint, not only `line-gradient`, and resolves to the same
    /// base colour the renderer draws.
    #[test]
    fn named_colours_work_in_every_colour_key() {
        let overlay: OverlaySpec = serde_json::from_value(serde_json::json!({
            "id": "o", "data": "o.geojson",
            "layers": [
                {"id": "l", "type": "line", "paint": {"line-color": "Red"}},
                {"id": "f", "type": "fill", "paint": {"fill-color": ["match", ["get", "k"], "a", "navy", "orange"]}},
                {"id": "c", "type": "circle", "paint": {"circle-color": "teal"}}
            ]
        }))
        .unwrap();
        let square = serde_json::json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]});
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {"k": "a"}, "geometry": square},
            {"type": "Feature", "properties": {"k": "b"}, "geometry": square},
            {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [0, 0]}}]});
        let sources = layer_sources(0, &overlay, &doc).unwrap();
        assert_eq!(sources[0].base_colors, vec![Some([255, 0, 0, 255]); 2], "polygon outlines in red");
        assert_eq!(sources[1].base_colors, vec![Some([0, 0, 128, 255]), Some([255, 165, 0, 255])]);
        assert_eq!(sources[2].base_colors, vec![Some([0, 128, 128, 255])]);
        for s in &sources {
            let paint = serde_json::Value::Object(s.paint.clone()).to_string();
            assert!(!paint.contains("ed\"") && !paint.contains("navy") && !paint.contains("teal"), "names spelled out: {paint}");
            crate::overlay::source_paint::SourcePaint::compile(&s.paint).unwrap_or_else(|e| panic!("{}: {e}", s.layer_id));
        }
        assert!(crate::style_spec::parse_css_color("red").is_err(), "the basemap parser still refuses names (goldens)");
    }

    #[test]
    fn document_feature_count_counts_the_whole_document_not_a_filtered_layer() {
        assert_eq!(document_feature_count(&serde_json::json!({"type": "FeatureCollection", "features": [{}, {}, {}]})), 3);
        assert_eq!(document_feature_count(&serde_json::json!({"type": "Feature"})), 1);
        assert_eq!(document_feature_count(&serde_json::json!({"type": "Point", "coordinates": [0, 0]})), 0);
    }
}
