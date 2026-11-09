//! "Labels over data": the basemap label treatment a page uses while it
//! fills large polygon areas over the basemap (a choropleth, hexagons,
//! census tracts), the way Kepler draws labels on its data maps -- light
//! text on a firm dark halo, so a street or district name stays readable
//! over teal, green and crimson alike.
//!
//! The basemaps' own label paint is tuned for the bare basemap: OpenFreeMap
//! Dark writes its place names in `rgb(101,101,101)` with a 1 px soft halo,
//! which all but vanishes over a 0.8-opacity data fill. Here every symbol
//! layer that draws text gets
//!
//! - on a dark basemap: `#F1F3F5` text, a `rgba(17,17,17,0.85)` halo,
//!   1.75 px wide with a 0.25 px blur;
//! - on a light basemap (inverted): `#1B1E22` text, a white halo at 0.9;
//! - place and district names (the `place` / `place_label` source layers,
//!   countries and states aside) floored at 12 px, so they are never
//!   smaller than the street names beside them.
//!
//! Only the style's SYMBOL layers change; fills, lines and icons do not.
//! It is a style edit, applied through the host-level
//! `MapView::set_style_json` (what the story timeline's
//! `set_paint_property` does too: edit the document, compile it, set it),
//! so it needs the map's style document. [`DataLabels::attach`] records
//! one when the page built its map from a compiled style. No library
//! primitive is needed: the restyle happens on a page open, a mode switch
//! or a basemap switch, never per frame.
//!
//! [`DataLabels`] keeps the treatment on while the page says its fill is
//! shown ([`DataLabels::set_active`]) and puts the untreated style back
//! when it is not. It watches the map, so a basemap picked from the map
//! options (a new style) is treated again rather than silently losing the
//! treatment. Leaving the page drops the map with it.
#![allow(dead_code)] // not every binary that includes the helper uses all of it

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, Entity, Subscription};
use gpui_maps::map_view::MapView;
use serde_json::{json, Value};

/// Set in a treated document's root `metadata`, so a style is never
/// treated twice.
pub const MARK: &str = "gpui-map:labels-over-data";

/// The near-white text on a dark basemap.
pub const DARK_GROUND_TEXT: &str = "#F1F3F5";
/// Its halo: `#111` at 0.85.
pub const DARK_GROUND_HALO: &str = "rgba(17, 17, 17, 0.85)";
/// The inverted pair on a light basemap.
pub const LIGHT_GROUND_TEXT: &str = "#1B1E22";
pub const LIGHT_GROUND_HALO: &str = "rgba(255, 255, 255, 0.9)";
/// The halo's width and blur, in px.
pub const HALO_WIDTH: f64 = 1.75;
pub const HALO_BLUR: f64 = 0.25;
/// The smallest a place or district name is drawn, in px (the vendored
/// basemaps' street names are 10-13 px).
pub const PLACE_MIN_SIZE: f64 = 12.0;

/// Which way round the treatment goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ground {
    Dark,
    Light,
}

/// The first colour found in `v` (a literal or any expression's
/// arguments), parsed.
fn first_color(v: &Value) -> Option<gpui_maps::style::Rgba> {
    match v {
        Value::String(s) => gpui_maps::style_spec::color::parse_css_color(s).ok(),
        Value::Array(items) => items.iter().find_map(first_color),
        _ => None,
    }
}

/// Dark or light, from the style's `background` layer (its first colour
/// when the colour is an expression). A style with no background draws on
/// the map's clear colour, which is dark.
#[must_use]
pub fn ground_of(doc: &Value) -> Ground {
    let background = doc
        .get("layers")
        .and_then(Value::as_array)
        .and_then(|layers| layers.iter().find(|l| l.get("type").and_then(Value::as_str) == Some("background")))
        .and_then(|l| l.get("paint"))
        .and_then(|p| p.get("background-color"))
        .and_then(first_color);
    match background {
        Some(c) if 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b >= 0.5 => Ground::Light,
        _ => Ground::Dark,
    }
}

/// The ONE style of every label a page or the agent puts on the map
/// (agent annotations, result labels, a page's area, hub, stop and
/// station names): the basemap's own main place-name label, so an added
/// name reads as one of the map's and never as a headline over it.
///
/// Two tiers only: [`LabelTier::Regular`] for a point's name and value,
/// [`LabelTier::Major`] for a territory or area name, as the basemap's
/// city names sit above its town names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelTier {
    Regular,
    Major,
}

impl LabelTier {
    /// The tier's text size, px.
    #[must_use]
    pub const fn size(self) -> f64 {
        match self {
            Self::Regular => LABEL_SIZE,
            Self::Major => MAJOR_LABEL_SIZE,
        }
    }
}

/// A regular label's size, px (the basemaps' town names are 12-14 px).
pub const LABEL_SIZE: f64 = 13.0;
/// A territory or area name's size, px.
pub const MAJOR_LABEL_SIZE: f64 = 15.0;
/// The widest halo a label of ours gets, px: thin, as the basemaps' own
/// (1 px). A 2 px halo at 2x drew the engine's halo copies 4 px out and
/// read as doubled glyphs (the delivery page's agent labels, 2026-10-12
/// and -13).
pub const LABEL_HALO_MAX: f64 = 1.5;
/// The halo a label gets when the style's place labels have none, or the
/// map has no style document.
pub const LABEL_HALO_FALLBACK: f64 = 1.0;

/// The ground of the map's current style (dark when it has none).
#[must_use]
pub fn ground_of_map(map: &MapView) -> Ground {
    map.style_document().map_or(Ground::Dark, ground_of)
}

/// The first number in `v`: a literal, or an `interpolate` / `step`'s
/// first output.
fn first_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Array(items) => match items.first().and_then(Value::as_str) {
            // ["interpolate", curve, input, z0, v0, ...]
            Some("interpolate") => items.get(4).and_then(first_number),
            // ["step", input, v0, ...]
            Some("step") => items.get(2).and_then(first_number),
            _ => None,
        },
        _ => None,
    }
}

fn css(c: gpui_maps::style::Rgba) -> String {
    let u = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = (f64::from(c.a.clamp(0.0, 1.0)) * 1000.0).round() / 1000.0;
    format!("rgba({}, {}, {}, {a})", u(c.r), u(c.g), u(c.b))
}

/// The style's main place-name label layer: its city names (not a
/// capital's), else its major settlements (Mapbox Streets), else its town
/// names, else the first place label it has.
fn main_place_layer(doc: &Value) -> Option<&Value> {
    let layers: Vec<&Value> = doc
        .get("layers")?
        .as_array()?
        .iter()
        .filter(|l| l.get("type").and_then(Value::as_str) == Some("symbol") && is_place_layer(l) && l.get("layout").and_then(|x| x.get("text-field")).is_some())
        .collect();
    let id = |l: &Value| l.get("id").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    let pick = |w: &str| layers.iter().copied().find(|l| id(l).contains(w) && !id(l).contains("capital"));
    pick("city").or_else(|| pick("major")).or_else(|| pick("town")).or_else(|| layers.first().copied())
}

/// The paint of every label of ours, in `tier`, from `doc`'s main
/// place-name label: its `text-color`, `text-halo-color` and
/// `text-halo-width` (capped at [`LABEL_HALO_MAX`]), so it matches light,
/// dark and every keyless style without a table of them. What the style
/// does not say (or no style at all) falls back to the ground's
/// treatment colours and a 1 px halo. No `text-allow-overlap`: a label
/// of ours sits on its point and collides with the basemap's.
///
/// The font is the engine's overlay label font: overlay features carry
/// no `text-font` (`gpui_maps::overlay::paint`), so the family cannot
/// follow the style's yet; the size, colours and halo do.
#[must_use]
pub fn label_paint(doc: Option<&Value>, tier: LabelTier) -> Value {
    let ground = doc.map_or(Ground::Dark, ground_of);
    let (fallback_text, fallback_halo) = match ground {
        Ground::Dark => (DARK_GROUND_TEXT, DARK_GROUND_HALO),
        Ground::Light => (LIGHT_GROUND_TEXT, LIGHT_GROUND_HALO),
    };
    let paint = doc.and_then(main_place_layer).and_then(|l| l.get("paint"));
    let get = |k: &str| paint.and_then(|p| p.get(k));
    let text = get("text-color").and_then(first_color).map_or_else(|| fallback_text.to_string(), css);
    let halo = get("text-halo-color").and_then(first_color).map_or_else(|| fallback_halo.to_string(), css);
    let width = get("text-halo-width").and_then(first_number).filter(|w| *w > 0.0).unwrap_or(LABEL_HALO_FALLBACK).min(LABEL_HALO_MAX);
    json!({"text-color": text, "text-halo-color": halo, "text-halo-width": width, "text-size": tier.size()})
}

/// A followed vehicle's label (the Transit page's GTFS-Realtime follow
/// mode): the name style's type, white on a halo in the vehicle's route
/// colour, so the label reads as a pill the colour of its line. The one
/// label style's variant for it, kept here with the others.
#[must_use]
pub fn route_pill_paint(base: &Value, route_hex: &str) -> serde_json::Map<String, Value> {
    let mut props = base.as_object().cloned().unwrap_or_default();
    props.insert("text-color".into(), json!("#ffffff"));
    props.insert("text-halo-color".into(), json!(route_hex));
    props.insert("text-halo-width".into(), json!(2.5));
    props
}

/// An object's name beside its marker (a station, a bus stop): the
/// transit desktop's stop-name style -- 11 px, the label's left edge a
/// little right of the point, in [`label_paint`]'s colours and halo (so the
/// halo palette follows the basemap). `highlighted`: the object picked on
/// the map, in [`HIGHLIGHT_TEXT`], drawn last so it wins its place.
#[must_use]
pub fn name_label_paint(doc: Option<&Value>, highlighted: bool) -> Value {
    let mut paint = label_paint(doc, LabelTier::Regular);
    if let Some(o) = paint.as_object_mut() {
        o.insert("text-size".into(), json!(NAME_LABEL_SIZE));
        o.insert("text-anchor".into(), json!("left"));
        o.insert("text-justify".into(), json!("left"));
        if highlighted {
            o.insert("text-color".into(), json!(HIGHLIGHT_TEXT));
        }
    }
    paint
}

/// `paint` (a [`name_label_paint`]) in the engine's on-data label paint
/// (`gpui_maps::style::LabelPaint::ON_DATA`, what the basemap's labels draw
/// with while the document has fills under them), so an object's name
/// reads like the place names beside it. A highlighted name keeps its
/// colour and takes the halo.
pub fn on_data_name_paint(paint: &mut Value) {
    let on = gpui_maps::style::LabelPaint::ON_DATA;
    let rgba = |c: gpui_maps::style::Rgba| {
        let b = |v: f32| (v * 255.0).round() as u8;
        format!("rgba({}, {}, {}, {})", b(c.r), b(c.g), b(c.b), (c.a * 100.0).round() / 100.0)
    };
    let Some(o) = paint.as_object_mut() else { return };
    if o.get("text-color").and_then(Value::as_str) != Some(HIGHLIGHT_TEXT) {
        o.insert("text-color".into(), json!(rgba(on.text_color)));
    }
    o.insert("text-halo-color".into(), json!(rgba(on.halo_color)));
    o.insert("text-halo-width".into(), json!(on.halo_width));
}

/// A name label's size, px (the transit desktop's stop names).
pub const NAME_LABEL_SIZE: f64 = 11.0;
/// A picked object's name colour (the transit desktop's selection blue).
pub const HIGHLIGHT_TEXT: &str = "#389EFF";

/// [`label_paint`] for `map`'s current style.
#[must_use]
pub fn map_label_paint(map: &MapView, tier: LabelTier) -> Value {
    label_paint(map.style_document(), tier)
}

/// Whether `doc` already carries the treatment.
#[must_use]
pub fn is_treated(doc: &Value) -> bool {
    doc.get("metadata").and_then(|m| m.get(MARK)).and_then(Value::as_bool) == Some(true)
}

/// A place or district label layer (OpenMapTiles' `place`, Mapbox's
/// `place_label`), not a country, state or continent name.
fn is_place_layer(layer: &Value) -> bool {
    let source_layer = layer.get("source-layer").and_then(Value::as_str).unwrap_or_default();
    let id = layer.get("id").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    matches!(source_layer, "place" | "place_label") && !["country", "state", "continent"].iter().any(|w| id.contains(w))
}

/// `text-size` with every literal size (a constant, or an `interpolate` /
/// `step`'s stop outputs) raised to at least `floor`. Anything else (a
/// data-driven size) is left as it is.
fn floor_size(size: &Value, floor: f64) -> Value {
    match size {
        Value::Number(n) if n.as_f64().is_some_and(|v| v < floor) => json!(floor),
        Value::Array(items) => match items.first().and_then(Value::as_str) {
            // ["interpolate", curve, input, z0, v0, z1, v1, ...]
            Some("interpolate") => Value::Array(
                items.iter().enumerate().map(|(i, v)| if i >= 4 && i % 2 == 0 { floor_size(v, floor) } else { v.clone() }).collect(),
            ),
            // ["step", input, v0, z1, v1, ...]
            Some("step") => Value::Array(items.iter().enumerate().map(|(i, v)| if i >= 2 && i % 2 == 0 { floor_size(v, floor) } else { v.clone() }).collect()),
            _ => size.clone(),
        },
        // Legacy `{"stops": [[z, v], ...]}`.
        Value::Object(o) if o.contains_key("stops") => {
            let mut o = o.clone();
            if let Some(stops) = o.get_mut("stops").and_then(Value::as_array_mut) {
                for stop in stops.iter_mut() {
                    if let Some(v) = stop.get_mut(1) {
                        *v = floor_size(v, floor);
                    }
                }
            }
            Value::Object(o)
        }
        _ => size.clone(),
    }
}

/// Applies the treatment to `doc` in place; `false` when it was already
/// treated (nothing changed).
pub fn treat(doc: &mut Value) -> bool {
    if is_treated(doc) {
        return false;
    }
    let (text, halo) = match ground_of(doc) {
        Ground::Dark => (DARK_GROUND_TEXT, DARK_GROUND_HALO),
        Ground::Light => (LIGHT_GROUND_TEXT, LIGHT_GROUND_HALO),
    };
    if let Some(layers) = doc.get_mut("layers").and_then(Value::as_array_mut) {
        for layer in layers.iter_mut() {
            if layer.get("type").and_then(Value::as_str) != Some("symbol") {
                continue;
            }
            // Text only: an icon-only layer (one-way arrows, shields with
            // no text) keeps its paint.
            if layer.get("layout").and_then(|l| l.get("text-field")).is_none() {
                continue;
            }
            let place = is_place_layer(layer);
            let Some(obj) = layer.as_object_mut() else { continue };
            let paint = obj.entry("paint").or_insert_with(|| json!({}));
            if !paint.is_object() {
                *paint = json!({});
            }
            if let Some(p) = paint.as_object_mut() {
                p.insert("text-color".into(), json!(text));
                p.insert("text-halo-color".into(), json!(halo));
                p.insert("text-halo-width".into(), json!(HALO_WIDTH));
                p.insert("text-halo-blur".into(), json!(HALO_BLUR));
                // A basemap that fades its labels (`text-opacity` < 1) would
                // fade the halo with them.
                p.remove("text-opacity");
            }
            if place {
                // No `text-size` is the spec default, 16: already above it.
                if let Some(size) = obj.get_mut("layout").and_then(|l| l.get_mut("text-size")) {
                    *size = floor_size(size, PLACE_MIN_SIZE);
                }
            }
        }
    }
    let Some(root) = doc.as_object_mut() else { return false };
    let metadata = root.entry("metadata").or_insert_with(|| json!({}));
    if !metadata.is_object() {
        *metadata = json!({});
    }
    if let Some(m) = metadata.as_object_mut() {
        m.insert(MARK.into(), json!(true));
    }
    true
}

/// A vendored style (`kit::asset` path) as its JSON document.
pub fn document(rel: &str) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&crate::kit::asset_bytes(rel)?)?)
}

#[derive(Default)]
struct State {
    active: bool,
    /// The untreated document the treatment replaced, put back on
    /// deactivation.
    original: Option<Value>,
    /// The `MapView::style_epoch` of the style this helper last set or
    /// checked; another epoch means someone set a new style since.
    seen_epoch: Option<u64>,
    /// The epoch of the treated style this helper set, while it is up.
    treated_epoch: Option<u64>,
    warned: bool,
}

/// Keeps a map's basemap labels in the "labels over data" treatment while
/// the page's fill is shown. Hold it for as long as the page lives.
pub struct DataLabels {
    state: Rc<RefCell<State>>,
    _watch: Subscription,
}

impl DataLabels {
    /// Starts watching `map`. `document` is the JSON the map's CURRENT
    /// style was compiled from, when the page built the map from a
    /// compiled style (recorded with `MapView::set_style_document`);
    /// `None` when the map already has it. `active`: whether the page's
    /// fill shows from the start (treated before the first paint, so the
    /// page never opens with a restyle).
    pub fn attach(map: &Entity<MapView>, document: Option<Value>, active: bool, cx: &mut App) -> Self {
        if let Some(doc) = document {
            map.update(cx, |m, _| m.set_style_document(doc));
        }
        let state = Rc::new(RefCell::new(State { active, ..State::default() }));
        // No notify for this first treatment: the map has not painted yet,
        // and its first paint draws the treated style anyway. With one, a
        // page whose data loads slowly opened on a blank (white) map until
        // something else notified it -- the population page sat 20-60 s
        // that way on a cold cache; the basemap now paints at once.
        sync(&state, map, false, cx);
        let watched = state.clone();
        let watch = cx.observe(map, move |map, cx| sync(&watched, &map, true, cx));
        Self { state, _watch: watch }
    }

    /// The page's fill shown (`true`) or hidden.
    pub fn set_active(&self, map: &Entity<MapView>, active: bool, cx: &mut App) {
        if self.state.borrow().active == active {
            return;
        }
        self.state.borrow_mut().active = active;
        sync(&self.state, map, true, cx);
    }

    /// Whether the treatment is wanted now.
    #[must_use]
    pub fn active(&self) -> bool {
        self.state.borrow().active
    }
}

/// Brings `map`'s style in line with `state.active`. Cheap when nothing
/// changed (one epoch comparison), which is every notify but the ones a
/// new style or a mode switch causes.
fn sync(state: &Rc<RefCell<State>>, map: &Entity<MapView>, notify: bool, cx: &mut App) {
    let epoch = map.read(cx).style_epoch();
    let (active, seen, treated) = {
        let s = state.borrow();
        (s.active, s.seen_epoch, s.treated_epoch)
    };
    if active {
        if seen == Some(epoch) {
            return;
        }
        let Some(doc) = map.read(cx).style_document().cloned() else {
            let mut s = state.borrow_mut();
            s.seen_epoch = Some(epoch);
            if !s.warned {
                s.warned = true;
                eprintln!("data labels: the map's style has no JSON document; labels keep the basemap's own paint");
            }
            return;
        };
        if is_treated(&doc) {
            let mut s = state.borrow_mut();
            s.seen_epoch = Some(epoch);
            s.treated_epoch = Some(epoch);
            return;
        }
        let mut treated_doc = doc.clone();
        treat(&mut treated_doc);
        let result = map.update(cx, |m, cx| {
            let r = serde_json::to_vec(&treated_doc).map_err(anyhow::Error::from).and_then(|bytes| m.set_style_json(&bytes));
            if notify {
                cx.notify();
            }
            r.map(|()| m.style_epoch())
        });
        let mut s = state.borrow_mut();
        match result {
            Ok(new_epoch) => {
                s.original = Some(doc);
                s.seen_epoch = Some(new_epoch);
                s.treated_epoch = Some(new_epoch);
            }
            Err(e) => {
                s.seen_epoch = Some(epoch);
                eprintln!("data labels: the treated style did not compile ({e:#}); labels keep the basemap's own paint");
            }
        }
    } else {
        if treated != Some(epoch) {
            // Never treated, or someone replaced the treated style since:
            // what is up now is not ours to undo.
            let mut s = state.borrow_mut();
            s.treated_epoch = None;
            s.seen_epoch = Some(epoch);
            return;
        }
        let original = state.borrow_mut().original.take();
        let Some(original) = original else { return };
        let result = map.update(cx, |m, cx| {
            let r = serde_json::to_vec(&original).map_err(anyhow::Error::from).and_then(|bytes| m.set_style_json(&bytes));
            if notify {
                cx.notify();
            }
            r.map(|()| m.style_epoch())
        });
        let mut s = state.borrow_mut();
        s.treated_epoch = None;
        match result {
            Ok(new_epoch) => s.seen_epoch = Some(new_epoch),
            Err(e) => eprintln!("data labels: the basemap's own style did not compile back ({e:#})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol_paint<'a>(doc: &'a Value, id: &str) -> &'a Value {
        let layer = doc["layers"].as_array().unwrap().iter().find(|l| l["id"] == id).unwrap();
        &layer["paint"]
    }

    fn layer<'a>(doc: &'a Value, id: &str) -> &'a Value {
        doc["layers"].as_array().unwrap().iter().find(|l| l["id"] == id).unwrap()
    }

    #[test]
    fn the_dark_basemap_gets_light_text_on_a_dark_halo() {
        let mut doc = document("demos/openfreemap-dark-style.json").unwrap();
        assert_eq!(ground_of(&doc), Ground::Dark);
        assert!(treat(&mut doc));
        let p = symbol_paint(&doc, "place_suburb");
        assert_eq!(p["text-color"], DARK_GROUND_TEXT);
        assert_eq!(p["text-halo-color"], DARK_GROUND_HALO);
        assert_eq!(p["text-halo-width"], HALO_WIDTH);
        assert_eq!(symbol_paint(&doc, "highway_name_other")["text-color"], DARK_GROUND_TEXT);
        // The one-way arrows (icons, no text) keep their paint.
        assert_eq!(symbol_paint(&doc, "road_oneway"), &json!({"icon-opacity": 0.5}));
        // Fills untouched.
        let water = layer(&doc, "water");
        assert_eq!(water, layer(&document("demos/openfreemap-dark-style.json").unwrap(), "water"));
    }

    #[test]
    fn district_names_are_never_smaller_than_street_names() {
        let mut doc = document("demos/openfreemap-dark-style.json").unwrap();
        treat(&mut doc);
        assert_eq!(layer(&doc, "place_suburb")["layout"]["text-size"], json!(PLACE_MIN_SIZE));
        assert_eq!(layer(&doc, "place_other")["layout"]["text-size"], json!(PLACE_MIN_SIZE));
        // Larger stays larger; countries keep their own curve.
        assert_eq!(layer(&doc, "place_city_large")["layout"]["text-size"], json!(14));
        let original = document("demos/openfreemap-dark-style.json").unwrap();
        assert_eq!(layer(&doc, "place_country_major")["layout"]["text-size"], layer(&original, "place_country_major")["layout"]["text-size"]);
        // Streets keep their size.
        assert_eq!(layer(&doc, "highway_name_other")["layout"]["text-size"], json!(10));
    }

    #[test]
    fn the_light_basemap_is_inverted() {
        let mut doc = document("demos/positron-style.json").unwrap();
        assert_eq!(ground_of(&doc), Ground::Light);
        treat(&mut doc);
        let p = symbol_paint(&doc, "label_other");
        assert_eq!(p["text-color"], LIGHT_GROUND_TEXT);
        assert_eq!(p["text-halo-color"], LIGHT_GROUND_HALO);
        // An interpolated place size is floored stop by stop.
        assert_eq!(layer(&doc, "label_other")["layout"]["text-size"], json!(["interpolate", ["linear"], ["zoom"], 8, 12.0, 12, 12.0]));
    }

    #[test]
    fn a_treated_style_compiles_and_is_not_treated_twice() {
        for rel in ["demos/openfreemap-dark-style.json", "demos/positron-style.json", "examples/liberty-style.json", "demos/mapbox-light-style.json"] {
            // Mapbox Light is not in the open library tree (split-manifest X).
            if rel.contains("mapbox") && crate::kit::asset_bytes(rel).is_err() {
                eprintln!("skipped: {rel} is a Mapbox fixture and this checkout has none");
                continue;
            }
            let mut doc = document(rel).unwrap();
            assert!(treat(&mut doc), "{rel}");
            assert!(is_treated(&doc));
            let once = doc.clone();
            assert!(!treat(&mut doc), "{rel}: a second treat is a no-op");
            assert_eq!(doc, once);
            gpui_maps::style_spec::parse_and_compile(&serde_json::to_vec(&doc).unwrap()).unwrap_or_else(|e| panic!("{rel}: {e:#}"));
        }
    }

    #[test]
    fn floor_size_reads_steps_and_legacy_stops() {
        assert_eq!(floor_size(&json!(["step", ["zoom"], 9, 10, 14]), 12.0), json!(["step", ["zoom"], 12.0, 10, 14]));
        assert_eq!(floor_size(&json!({"stops": [[4, 9], [10, 16]]}), 12.0), json!({"stops": [[4, 12.0], [10, 16]]}));
        // A data-driven size is left alone.
        assert_eq!(floor_size(&json!(["get", "size"]), 12.0), json!(["get", "size"]));
    }
}
