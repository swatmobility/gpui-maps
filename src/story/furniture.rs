//! Map furniture (spec §4.5): the source line and byline, the scale bar,
//! the legend and the north arrow -- what a frame shows, computed from the
//! story's [`FurnitureSpec`] and the camera at the frame's centre.
//!
//! **Pure.** Numbers and JSON in, values out: no clock, no font system, no
//! map iteration order. The layout ([`super::layout`]) places a
//! [`FurnitureFrame`] and `capture::story_chrome` draws it. The browser
//! and iOS shells include this file unchanged.
//!
//! **Units.** Every length is in 1080 units, which are the capture's
//! logical pixels (`physical / pixel_ratio`), so a scale bar is the same
//! length in units at every output tier and the drawing multiplies by the
//! ratio like everything else.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

use serde_json::Value;

use super::overlay_layers::LayerSource;
use super::script::{Diagnostic, FurnitureSpec, LegendSpec, NorthArrowMode, OverlayLayerKind, ScaleBarUnits, Severity};

/// One frame's furniture, ready to lay out and draw.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct FurnitureFrame {
    pub source_line: Option<String>,
    pub byline: Option<String>,
    pub scale_bar: Option<ScaleBar>,
    pub legend: Option<Legend>,
    /// The north arrow's rotation, degrees clockwise (`-bearing`).
    pub north_arrow: Option<f32>,
    /// `Some(pitch)` when a scale bar was asked for but hidden because the
    /// camera is pitched past [`SCALE_BAR_MAX_PITCH_DEG`]: the layout
    /// notes it (`scale_bar_hidden_pitch`).
    pub scale_bar_hidden_pitch: Option<f32>,
}

impl FurnitureFrame {
    /// Whether there is anything to place.
    pub fn is_empty(&self) -> bool {
        self.source_line.is_none() && self.byline.is_none() && self.scale_bar.is_none() && self.legend.is_none() && self.north_arrow.is_none()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScaleBar {
    /// (label, length in 1080 units): metric first, then imperial when asked.
    pub bars: Vec<(String, f32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Legend {
    pub title: Option<String>,
    pub entries: Vec<(LegendSwatch, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LegendSwatch {
    /// A line layer's colour and its constant `line-width` (2 when it has none).
    Line([u8; 4], f32),
    Fill([u8; 4]),
    /// A circle layer's colour and its constant `circle-radius` (5 when it has none).
    Circle([u8; 4], f32),
}

/// The widest a scale bar may be, 1080 units.
pub const SCALE_BAR_MAX_UNITS: f32 = 200.0;
/// Below this zoom a globe frame shows no scale bar: the scale changes too
/// much across the frame for one bar to be true.
pub const SCALE_BAR_GLOBE_MIN_ZOOM: f64 = 3.0;
/// Above this pitch (degrees) the scale bar is hidden: the ground scale
/// changes too much between the frame's foot and its horizon for one bar
/// to be true. Up to it the bar is labelled for the frame centre.
pub const SCALE_BAR_MAX_PITCH_DEG: f64 = 45.0;
/// The equator's length, metres (WGS 84), as MapLibre's scale control uses.
const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.686;
const METRES_PER_FOOT: f64 = 0.3048;
const METRES_PER_MILE: f64 = 1609.344;
/// The label of a `match` expression's fallback class.
pub const LEGEND_OTHER: &str = "Other";

/// The largest `1, 2, 5 x 10^n` not above `max` (`max > 0`).
fn nice_below(max: f64) -> Option<f64> {
    if !(max.is_finite() && max > 0.0) {
        return None;
    }
    let pow = 10f64.powi(max.log10().floor() as i32);
    let nice = [5.0, 2.0, 1.0].into_iter().map(|m| m * pow).find(|v| *v <= max * (1.0 + 1e-12))?;
    Some(nice)
}

/// A number with no trailing `.0` and at most six decimals.
fn number(v: f64) -> String {
    let r = (v * 1e6).round() / 1e6;
    if r == r.trunc() && r.abs() < 1e15 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// One bar of `nice` (in `unit`s of `metres_each` metres) at `mpu` metres
/// per 1080 unit.
fn bar(nice: f64, unit: &str, metres_each: f64, mpu: f64) -> (String, f32) {
    (format!("{} {unit}", number(nice)), (nice * metres_each / mpu) as f32)
}

/// The largest "nice" length (1, 2, 5 x 10^n m; 1, 2, 5 x 10^n ft or mi) not longer than `max_units`, from the ground
/// resolution at the frame centre: `metres_per_unit = 40_075_016.686 * cos(lat) / (512 * 2^zoom)` (512 = TILE_SIZE at
/// ratio 1). `None` on a globe frame below zoom 3.
///
/// Metric reads in metres below 1 km and in kilometres from there; imperial
/// in feet below a mile and in miles from there.
pub fn scale_bar(lat: f64, zoom: f64, globe_frame: bool, units: ScaleBarUnits, max_units: f32) -> Option<ScaleBar> {
    if globe_frame && zoom < SCALE_BAR_GLOBE_MIN_ZOOM {
        return None;
    }
    let mpu = EARTH_CIRCUMFERENCE_M * lat.clamp(-85.051_129, 85.051_129).to_radians().cos() / (crate::geo::TILE_SIZE * 2f64.powf(zoom));
    if !(mpu.is_finite() && mpu > 0.0 && max_units.is_finite() && max_units > 0.0) {
        return None;
    }
    let max_m = f64::from(max_units) * mpu;
    let metric = || -> Option<(String, f32)> {
        let m = nice_below(max_m)?;
        Some(if m >= 1000.0 { bar(m / 1000.0, "km", 1000.0, mpu) } else { bar(m, "m", 1.0, mpu) })
    };
    let imperial = || -> Option<(String, f32)> {
        let max_mi = max_m / METRES_PER_MILE;
        if max_mi >= 1.0 {
            Some(bar(nice_below(max_mi)?, "mi", METRES_PER_MILE, mpu))
        } else {
            Some(bar(nice_below(max_m / METRES_PER_FOOT)?, "ft", METRES_PER_FOOT, mpu))
        }
    };
    let bars: Vec<(String, f32)> = match units {
        ScaleBarUnits::Metric => metric().into_iter().collect(),
        ScaleBarUnits::Imperial => imperial().into_iter().collect(),
        ScaleBarUnits::Both => metric().into_iter().chain(imperial()).collect(),
    };
    (!bars.is_empty()).then_some(ScaleBar { bars })
}

/// How far from north the bearing must be before `auto` shows the arrow.
pub const NORTH_ARROW_AUTO_DEG: f64 = 2.0;

/// Shown when `|bearing| > 2` degrees (auto), always, or never.
/// The bearing is read the short way round (359 is 1 degree off north).
/// The value is the arrow's rotation, degrees clockwise: `-bearing`.
pub fn north_arrow(bearing_deg: f64, mode: NorthArrowMode) -> Option<f32> {
    if !bearing_deg.is_finite() {
        return None;
    }
    let b = (bearing_deg + 180.0).rem_euclid(360.0) - 180.0;
    let shown = match mode {
        NorthArrowMode::Never => false,
        NorthArrowMode::Always => true,
        NorthArrowMode::Auto => b.abs() > NORTH_ARROW_AUTO_DEG,
    };
    // `0.0 - b`, not `-b`: north is `0.0`, never `-0.0`.
    shown.then_some((0.0 - b) as f32)
}

fn colour_bytes(c: crate::style::Rgba) -> [u8; 4] {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(c.r), byte(c.g), byte(c.b), byte(c.a)]
}

/// A colour literal: a CSS colour or a CSS name.
fn colour(v: &Value) -> Option<[u8; 4]> {
    let s = v.as_str()?;
    crate::style_spec::parse_css_color(s).ok().or_else(|| crate::style_spec::color::css_named_color(s.trim())).map(colour_bytes)
}

/// A `match` label as text: a string or number as itself, a list of them
/// joined with ", ".
fn label_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), number),
        Value::Bool(b) => b.to_string(),
        Value::Array(a) => a.iter().map(label_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn stop_text(v: &Value) -> String {
    v.as_f64().map_or_else(|| label_text(v), number)
}

fn color_key(kind: OverlayLayerKind) -> &'static str {
    match kind {
        OverlayLayerKind::Line => "line-color",
        OverlayLayerKind::Fill => "fill-color",
        OverlayLayerKind::Circle => "circle-color",
    }
}

fn opacity_key(kind: OverlayLayerKind) -> Option<&'static str> {
    match kind {
        OverlayLayerKind::Line => Some("line-opacity"),
        OverlayLayerKind::Fill => Some("fill-opacity"),
        OverlayLayerKind::Circle => None,
    }
}

/// The last colour literal anywhere in `v`, depth first: an unsupported
/// expression's fallback (`case`, `coalesce` and friends end with it).
fn last_colour(v: &Value) -> Option<[u8; 4]> {
    match v {
        Value::Array(a) => a.iter().rev().find_map(last_colour),
        other => colour(other),
    }
}

/// From each named layer's paint: a plain colour gives one swatch labelled with the layer id (or labels[id]); a
/// `match` gives one entry per label value (in expression order, default last); a `step` gives one per threshold
/// labelled "< a", "a–b", ">= b". Anything else: one swatch of the expression's default and the warning
/// `legend_unsupported`.
///
/// An `interpolate` ramp gives one entry per stop, labelled with the stop's
/// input value. Every label is text from the story (a layer id, a class
/// value, a stop, or `spec.labels`), except [`LEGEND_OTHER`] for a
/// `match`'s fallback; `spec.labels` overrides any of them, keyed by that
/// text. A constant `*-opacity` scales the swatch's alpha. A layer with
/// no loaded source is left out. Diagnostics point at
/// `/furniture/legend/layers/<i>` (a chapter's caller re-roots them).
pub fn legend(spec: &LegendSpec, layers: &[LayerSource]) -> (Legend, Vec<Diagnostic>) {
    let mut entries = Vec::new();
    let mut diags = Vec::new();
    let label = |text: String| spec.labels.get(&text).cloned().unwrap_or(text);
    for (i, id) in spec.layers.iter().enumerate() {
        let Some(source) = layers.iter().find(|l| &l.layer_id == id) else { continue };
        let kind = source.kind;
        let alpha = opacity_key(kind).and_then(|k| source.paint.get(k)).and_then(Value::as_f64).map_or(1.0, |o| o.clamp(0.0, 1.0));
        let number_paint = |k: &str, d: f32| source.paint.get(k).and_then(Value::as_f64).map_or(d, |v| v as f32);
        let swatch = |c: [u8; 4]| {
            let [r, g, b, a] = c;
            let c = [r, g, b, (f64::from(a) * alpha).round() as u8];
            match kind {
                OverlayLayerKind::Line => LegendSwatch::Line(c, number_paint("line-width", 2.0)),
                OverlayLayerKind::Fill => LegendSwatch::Fill(c),
                OverlayLayerKind::Circle => LegendSwatch::Circle(c, number_paint("circle-radius", 5.0)),
            }
        };
        let path = format!("/furniture/legend/layers/{i}");
        let unsupported = |diags: &mut Vec<Diagnostic>, what: &str| {
            diags.push(Diagnostic {
                severity: Severity::Warning,
                path: path.clone(),
                code: "legend_unsupported",
                message: format!("layer `{id}`'s {} is {what}; the legend shows one swatch of its fallback colour", color_key(kind)),
                hint: Some("use a constant colour, a `match`, a `step` or an `interpolate` on one property".to_string()),
            })
        };
        let paint = source.paint.get(color_key(kind));
        let Some(expr) = paint else {
            // No colour declared: MapLibre's default, black.
            entries.push((swatch([0, 0, 0, 255]), label(id.clone())));
            continue;
        };
        if let Some(c) = colour(expr) {
            entries.push((swatch(c), label(id.clone())));
            continue;
        }
        let parts = expr.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let op = parts.first().and_then(Value::as_str).unwrap_or("");
        let mut classes: Vec<(LegendSwatch, String)> = Vec::new();
        let mut ok = true;
        match op {
            // ["match", input, label, out, label, out, ..., fallback]
            "match" if parts.len() >= 3 && parts.len() % 2 == 1 => {
                let rest = parts.get(2..).unwrap_or(&[]);
                let (pairs, fallback) = rest.split_at(rest.len() - 1);
                for [l, c] in pairs.as_chunks::<2>().0 {
                    let Some(c) = colour(c) else {
                        ok = false;
                        continue;
                    };
                    classes.push((swatch(c), label(label_text(l))));
                }
                match fallback.first().and_then(colour) {
                    Some(c) => classes.push((swatch(c), label(LEGEND_OTHER.to_string()))),
                    None => ok = false,
                }
            }
            // ["step", input, out0, stop1, out1, stop2, out2, ...]
            "step" if parts.len() >= 3 && parts.len() % 2 == 1 => {
                let stops: Vec<&Value> = parts.iter().skip(3).step_by(2).collect();
                let outs: Vec<&Value> = parts.iter().skip(2).step_by(2).collect();
                for (k, out) in outs.iter().enumerate() {
                    let Some(c) = colour(out) else {
                        ok = false;
                        continue;
                    };
                    let text = match (k.checked_sub(1).and_then(|p| stops.get(p)), stops.get(k)) {
                        (None, Some(a)) => format!("< {}", stop_text(a)),
                        (Some(a), Some(b)) => format!("{}–{}", stop_text(a), stop_text(b)),
                        (Some(b), None) => format!(">= {}", stop_text(b)),
                        (None, None) => label_text(&Value::Null),
                    };
                    classes.push((swatch(c), label(text)));
                }
            }
            // ["interpolate", ["linear"], input, stop1, out1, ...]
            "interpolate" | "interpolate-hcl" | "interpolate-lab" if parts.len() >= 5 && parts.len() % 2 == 1 => {
                for [s, c] in parts.get(3..).unwrap_or(&[]).as_chunks::<2>().0 {
                    let Some(c) = colour(c) else {
                        ok = false;
                        continue;
                    };
                    classes.push((swatch(c), label(stop_text(s))));
                }
            }
            _ => ok = false,
        }
        if ok && !classes.is_empty() {
            entries.extend(classes);
        } else {
            unsupported(&mut diags, if op.is_empty() { "not a colour" } else { "an expression the legend cannot list" });
            entries.push((swatch(last_colour(expr).unwrap_or([0, 0, 0, 255])), label(id.clone())));
        }
    }
    (Legend { title: spec.title.clone(), entries }, diags)
}

/// The chapter's furniture over the story's, field by field.
pub fn furniture_at(story: &FurnitureSpec, chapter: Option<&FurnitureSpec>) -> FurnitureSpec {
    let Some(c) = chapter else { return story.clone() };
    FurnitureSpec {
        source_line: c.source_line.clone().or_else(|| story.source_line.clone()),
        byline: c.byline.clone().or_else(|| story.byline.clone()),
        scale_bar: c.scale_bar.or(story.scale_bar),
        legend: c.legend.clone().or_else(|| story.legend.clone()),
        north_arrow: c.north_arrow.or(story.north_arrow),
        sources: c.sources.clone().or_else(|| story.sources.clone()),
    }
}

/// The camera at a frame's centre, as furniture reads it: degrees for the
/// angles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameCamera {
    pub lat: f64,
    pub zoom: f64,
    pub bearing_deg: f64,
    pub pitch_deg: f64,
    pub globe: bool,
}

/// One frame's furniture from its spec, the legend built for that spec, and
/// the camera at the frame's centre. `None` when nothing shows (a scale bar
/// hidden by pitch alone still gives a frame, carrying the note).
pub fn frame_furniture(spec: &FurnitureSpec, legend: Option<&Legend>, camera: FrameCamera) -> Option<FurnitureFrame> {
    let text = |s: &Option<String>| s.as_ref().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let pitched = camera.pitch_deg > SCALE_BAR_MAX_PITCH_DEG;
    let frame = FurnitureFrame {
        source_line: text(&spec.source_line),
        byline: text(&spec.byline),
        scale_bar: spec.scale_bar.filter(|_| !pitched).and_then(|u| scale_bar(camera.lat, camera.zoom, camera.globe, u, SCALE_BAR_MAX_UNITS)),
        legend: legend.filter(|l| !l.entries.is_empty()).cloned(),
        north_arrow: spec.north_arrow.and_then(|m| north_arrow(camera.bearing_deg, m)),
        scale_bar_hidden_pitch: (spec.scale_bar.is_some() && pitched).then_some(camera.pitch_deg as f32),
    };
    (!frame.is_empty() || frame.scale_bar_hidden_pitch.is_some()).then_some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, kind: OverlayLayerKind, paint: Value) -> LayerSource {
        let mut s = LayerSource::empty(id, kind);
        s.paint = paint.as_object().cloned().unwrap();
        s
    }

    fn spec(layers: &[&str]) -> LegendSpec {
        LegendSpec { layers: layers.iter().map(|s| s.to_string()).collect(), title: None, labels: Default::default() }
    }

    #[test]
    fn the_scale_bar_is_right_at_the_equator_and_at_60_degrees() {
        // zoom 10, lat 0: metres per unit = 40075016.686 / (512 * 1024) = 76.437; max 200 units -> 10 km = 130.8 units.
        let s = scale_bar(0.0, 10.0, false, ScaleBarUnits::Metric, 200.0).unwrap();
        assert_eq!(s.bars[0].0, "10 km");
        assert!((s.bars[0].1 - 130.83).abs() < 0.05);
        // lat 60: 38.2185 m per unit, so 10 km would be 261.7 units (> 200): 5 km = 130.83 units.
        let s60 = scale_bar(60.0, 10.0, false, ScaleBarUnits::Metric, 200.0).unwrap();
        assert_eq!(s60.bars[0].0, "5 km");
        assert!((s60.bars[0].1 - 130.83).abs() < 0.05);
        assert!(scale_bar(0.0, 2.5, true, ScaleBarUnits::Metric, 200.0).is_none());
    }

    #[test]
    fn the_scale_bar_reads_metres_feet_and_miles_and_both_in_order() {
        // zoom 16, lat 0: 1.19 m per unit; 200 units = 238.9 m -> 200 m; 783.8 ft -> 500 ft.
        let s = scale_bar(0.0, 16.0, false, ScaleBarUnits::Both, 200.0).unwrap();
        assert_eq!(s.bars.iter().map(|b| b.0.as_str()).collect::<Vec<_>>(), ["200 m", "500 ft"]);
        let mpu = 40_075_016.686 / (512.0 * 65536.0);
        assert!((f64::from(s.bars[0].1) - 200.0 / mpu).abs() < 1e-3);
        assert!((f64::from(s.bars[1].1) - 500.0 * 0.3048 / mpu).abs() < 1e-3);
        // zoom 10: 15.29 km = 9.5 mi -> 5 mi.
        let s = scale_bar(0.0, 10.0, false, ScaleBarUnits::Imperial, 200.0).unwrap();
        assert_eq!(s.bars[0].0, "5 mi");
        // Every bar is at most the cap and at least 40% of it (1-2-5 steps never skip more than 2.5x).
        for zoom in [0.5, 3.0, 7.3, 12.0, 18.9, 22.0] {
            for lat in [-70.0, 0.0, 35.0, 80.0] {
                for b in scale_bar(lat, zoom, false, ScaleBarUnits::Both, 200.0).unwrap().bars {
                    assert!(b.1 <= 200.0 + 1e-3 && b.1 >= 200.0 / 2.5 - 1e-3, "{zoom} {lat}: {b:?}");
                }
            }
        }
        // A globe at zoom 3 or more shows it; a flat map at any zoom does.
        assert!(scale_bar(0.0, 3.0, true, ScaleBarUnits::Metric, 200.0).is_some());
        assert!(scale_bar(0.0, 1.0, false, ScaleBarUnits::Metric, 200.0).is_some());
    }

    #[test]
    fn the_north_arrow_shows_past_two_degrees() {
        assert_eq!(north_arrow(1.9, NorthArrowMode::Auto), None);
        assert_eq!(north_arrow(-30.0, NorthArrowMode::Auto), Some(30.0));
        assert_eq!(north_arrow(0.0, NorthArrowMode::Always), Some(0.0));
        assert_eq!(north_arrow(359.0, NorthArrowMode::Auto), None, "359 is 1 degree off north");
        assert_eq!(north_arrow(330.0, NorthArrowMode::Auto), Some(30.0));
        assert_eq!(north_arrow(90.0, NorthArrowMode::Never), None);
    }

    #[test]
    fn a_match_expression_legend_has_one_entry_per_class() {
        // line-color ["match", ["get", "kind"], "ferry", "#1f77b4", "cargo", "#e4572e", "#999999"]:
        // entries ferry, cargo, "Other" in that order with those colours.
        let layer = source(
            "routes",
            OverlayLayerKind::Line,
            serde_json::json!({"line-color": ["match", ["get", "kind"], "ferry", "#1f77b4", "cargo", "#e4572e", "#999999"], "line-width": 3}),
        );
        let (legend, diags) = legend(&spec(&["routes"]), std::slice::from_ref(&layer));
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            legend.entries,
            vec![
                (LegendSwatch::Line([0x1f, 0x77, 0xb4, 255], 3.0), "ferry".to_string()),
                (LegendSwatch::Line([0xe4, 0x57, 0x2e, 255], 3.0), "cargo".to_string()),
                (LegendSwatch::Line([0x99, 0x99, 0x99, 255], 3.0), "Other".to_string()),
            ]
        );
        // Labels and the title are the author's when given.
        let mut s = spec(&["routes"]);
        s.title = Some("Vessel".into());
        s.labels.insert("ferry".into(), "Ferries".into());
        s.labels.insert("Other".into(), "Everything else".into());
        let (legend, _) = super::legend(&s, &[layer]);
        assert_eq!(legend.title.as_deref(), Some("Vessel"));
        let labels: Vec<&str> = legend.entries.iter().map(|e| e.1.as_str()).collect();
        assert_eq!(labels, ["Ferries", "cargo", "Everything else"]);
    }

    #[test]
    fn a_step_a_constant_an_interpolate_and_an_unsupported_expression() {
        let step = source(
            "density",
            OverlayLayerKind::Fill,
            serde_json::json!({"fill-color": ["step", ["get", "n"], "#eeeeee", 10, "#999999", 100, "#333333"], "fill-opacity": 0.5}),
        );
        let plain = source("ports", OverlayLayerKind::Circle, serde_json::json!({"circle-color": "red", "circle-radius": 6}));
        let ramp =
            source("heat", OverlayLayerKind::Fill, serde_json::json!({"fill-color": ["interpolate", ["linear"], ["get", "v"], 0, "#000000", 1.5, "#ffffff"]}));
        let odd = source("odd", OverlayLayerKind::Line, serde_json::json!({"line-color": ["case", ["has", "x"], "#ff0000", "#00ff00"]}));
        let (l, diags) = legend(&spec(&["density", "ports", "heat", "odd", "not-loaded"]), &[step, plain, ramp, odd]);
        let got: Vec<(LegendSwatch, &str)> = l.entries.iter().map(|(s, t)| (s.clone(), t.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (LegendSwatch::Fill([0xee, 0xee, 0xee, 128]), "< 10"),
                (LegendSwatch::Fill([0x99, 0x99, 0x99, 128]), "10–100"),
                (LegendSwatch::Fill([0x33, 0x33, 0x33, 128]), ">= 100"),
                (LegendSwatch::Circle([255, 0, 0, 255], 6.0), "ports"),
                (LegendSwatch::Fill([0, 0, 0, 255]), "0"),
                (LegendSwatch::Fill([255, 255, 255, 255]), "1.5"),
                (LegendSwatch::Line([0, 255, 0, 255], 2.0), "odd"),
            ]
        );
        assert_eq!(diags.iter().map(|d| (d.code, d.path.as_str(), d.severity)).collect::<Vec<_>>(), [(
            "legend_unsupported",
            "/furniture/legend/layers/3",
            Severity::Warning
        )]);
    }

    /// Controller ruling: up to 45 degrees of pitch the bar stays,
    /// labelled for the frame centre; past it the bar is hidden and the
    /// frame carries the note.
    #[test]
    fn the_scale_bar_hides_past_45_degrees_of_pitch() {
        let spec = FurnitureSpec { scale_bar: Some(ScaleBarUnits::Metric), ..Default::default() };
        let cam = |pitch_deg: f64| FrameCamera { lat: 0.0, zoom: 10.0, bearing_deg: 0.0, pitch_deg, globe: false };
        for pitch in [0.0, 20.0, 44.9, 45.0] {
            let f = frame_furniture(&spec, None, cam(pitch)).unwrap();
            assert_eq!(f.scale_bar, scale_bar(0.0, 10.0, false, ScaleBarUnits::Metric, SCALE_BAR_MAX_UNITS), "{pitch}");
            assert_eq!(f.scale_bar_hidden_pitch, None);
        }
        for pitch in [45.1, 60.0] {
            let f = frame_furniture(&spec, None, cam(pitch)).expect("the note is a frame");
            assert_eq!(f.scale_bar, None, "{pitch}");
            assert_eq!(f.scale_bar_hidden_pitch, Some(pitch as f32));
        }
        // Not asked for: no note however pitched.
        assert_eq!(frame_furniture(&FurnitureSpec::default(), None, cam(60.0)), None);
    }

    #[test]
    fn a_chapter_overrides_the_story_field_by_field() {
        let story = FurnitureSpec { source_line: Some("Source: A".into()), scale_bar: Some(ScaleBarUnits::Metric), ..Default::default() };
        let chapter = FurnitureSpec { byline: Some("By B".into()), scale_bar: Some(ScaleBarUnits::Imperial), ..Default::default() };
        let got = furniture_at(&story, Some(&chapter));
        assert_eq!(got.source_line.as_deref(), Some("Source: A"));
        assert_eq!(got.byline.as_deref(), Some("By B"));
        assert_eq!(got.scale_bar, Some(ScaleBarUnits::Imperial));
        assert_eq!(furniture_at(&story, None), story);
        // Nothing to show is no frame.
        let cam = |bearing_deg: f64, pitch_deg: f64| FrameCamera { lat: 0.0, zoom: 5.0, bearing_deg, pitch_deg, globe: false };
        assert_eq!(frame_furniture(&FurnitureSpec::default(), None, cam(0.0, 0.0)), None);
        let f = frame_furniture(&got, None, cam(45.0, 0.0)).unwrap();
        assert_eq!(f.north_arrow, None, "not asked for");
        assert!(f.scale_bar.is_some());
    }
}
