//! **Overlay themes (spec §6.1-§6.4): a layer's `role` expanded into
//! sublayers and paint, under one of the four themes.**
//!
//! [`infer_role`] gives a layer with no `role` and an empty `paint` its
//! role (§6.1); [`expand`] turns a role into its sublayers, bottom to top,
//! with the theme's paint (§6.3) and the author's keys merged over the
//! core sublayer, key by key (§6.1 "Merging"). A layer with a `paint` and
//! no `role` is never themed: the caller draws it exactly as 1a does.
//!
//! Line roles (`route`, `tour`, `arrow`, `highlight`, `context`) take
//! every row of §6.3; a `route` ends in two point sublayers, its `start`
//! (the first vertex, the `stops` dot `done`) and -- with a draw -- its
//! `head` (the themed head at `p`, replacing 1a's head disc).
//!
//! Point roles (`stops`, `numbered`, `event`, R8) expand to `<id>/shadow`,
//! `<id>/halo` (the `now` ring and the outer disc, or a badge) and `<id>`
//! (the core, the numerals and the labels). Their state is a function of
//! `t` ([`point_state`]: the card's index, the route's draw), so the
//! capture draws them as per-frame markers ([`PointLook`], instanced discs
//! with a stroke, blur and translate); each sublayer's GeoJSON carries its
//! discs at the finished frame for the live map, which does not read
//! states per frame yet (R11).
//!
//! `area` (R9) expands to its fill `<id>` and its boundaries `<id>/edge`,
//! both in the area slot -- under every label, over the 3D buildings
//! ([`Sublayer::slot`], ruling R9-a). The fill's
//! colour is the area's [`AreaLegend`] -- the same values every card
//! legend naming the layer shows (`card_layout::area_legend_for`), so map
//! and legend cannot disagree.
//!
//! Every line sublayer carries `line-cap: round` and `line-join: round`
//! (§6.3) except the arrow's core, whose `butt` end meets the arrowhead's
//! base square. The arrowhead itself is not a source: it is a per-frame
//! mark of the arrow's core ([`Sublayer::arrowhead`], drawn by the
//! capture as an SDF icon at the line's last vertex, §6.4), edged in the
//! casing's white and laid over the shaft's last units (R7-a).
//!
//! The ahead line's dots are round: `[0, 2.8]` is read as MapLibre's
//! round-cap zero-length dash (`style::DashPattern::round_dots`).
//!
//! Pure: JSON in, JSON out.

use serde_json::{json, Map, Value};

use crate::story::card_layout::AreaLegend;
use crate::story::design::{Theme, INK};
use crate::story::effects::LayerFx;
use crate::story::overlay_layers::LayerSource;
use crate::story::script::OverlayLayerKind;

/// What a themed sublayer reads from its parent's effect state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trim {
    /// The parent's own trim and opacity (`casing`, the core, `halo`).
    Same,
    /// The part of a drawn line still ahead of its draw: `[p, 1]` (`ahead`).
    /// Only a layer with a `draw` effect has one ([`expand`]'s `draws`).
    Ahead,
}

impl Trim {
    /// The `(trim, opacity)` a sublayer draws at, from its parent's: the
    /// parent's own for [`Trim::Same`]. For [`Trim::Ahead`] it follows the
    /// DRAW (spec §6.3, `p` the draw progress): mid-draw `[p, 1]` at the
    /// parent's opacity; before the draw starts (no trim yet, the route
    /// still hidden) the whole line; nothing once it is drawn, or while
    /// the line shows whole without a draw having started. An `Ahead` is
    /// only made for a layer that has a `draw` ([`expand`]), so a hidden
    /// route that is never drawn shows no ahead dots. One rule for the
    /// capture's draw and the live map's layer effects (spec §13).
    #[must_use]
    pub fn apply(self, trim: Option<[f32; 2]>, opacity: f32) -> (Option<[f32; 2]>, f32) {
        match (self, trim) {
            (Trim::Same, _) => (trim, opacity),
            (Trim::Ahead, Some([_, end])) if end < 1.0 => (Some([end, 1.0]), opacity),
            (Trim::Ahead, None) if opacity <= 0.0 => (None, 1.0),
            (Trim::Ahead, _) => (None, 0.0),
        }
    }
}

/// A themed sublayer's effect state from its parent's (the slice's
/// `fx_id`/[`Trim`] scheme, P5): the parent's state with the sublayer's
/// id and its trim and opacity by [`Trim::apply`]. The live map's layer
/// effects and the capture's draws both fan out through it.
#[must_use]
pub fn fan_out(parent: &LayerFx, sub_id: &str, trim: Trim) -> LayerFx {
    let (t, opacity) = trim.apply(parent.trim, parent.opacity);
    LayerFx { layer_id: sub_id.to_string(), trim: t, opacity, ..parent.clone() }
}

/// One themed sublayer: drawn in order, bottom to top.
#[derive(Clone, Debug, PartialEq)]
pub struct Sublayer {
    pub id: String,
    pub paint: Map<String, Value>,
    /// The sublayer's GeoJSON: the parent's, or the parent's with per-
    /// feature paint written into its properties (`stops`).
    pub geojson: String,
    pub trim: Trim,
    /// Whether the draw head and the rings draw with this sublayer (the
    /// core only).
    pub marks: bool,
    /// Whether an arrowhead is drawn at each line's last vertex (the
    /// `arrow` role's core, spec §6.3): [`line_ends`] of `geojson`.
    pub arrowhead: bool,
    /// A point role's part (spec §6.3), or a route's `start` and `head`:
    /// drawn by the capture as markers ([`PointLook`]) whose state is
    /// read per frame; `geojson` holds the part's discs at the finished
    /// frame (`p = 1`, no card index) for the live map. `None` for a line.
    pub point: Option<PointPart>,
    /// The area slot (spec §6.3, ruling R9-a): drawn below the basemap's
    /// labels and the story's, and above the basemap's 3D buildings,
    /// whatever the overlay's `above_labels` and the layer's
    /// `above_buildings` say. [`Sublayer::slot`] applies it.
    pub area_slot: bool,
}

impl Sublayer {
    /// The `(above_labels, above_buildings)` this sublayer draws at, from
    /// its layer's: as declared, except in the area slot -- below every
    /// label, over the 3D buildings (spec §6.3, ruling R9-a), so at pitch 0
    /// no building punches a grey patch through a fill. The render and the
    /// live map both place sublayers here.
    #[must_use]
    pub fn slot(&self, above_labels: bool, above_buildings: bool) -> (bool, bool) {
        if self.area_slot { (false, true) } else { (above_labels, above_buildings) }
    }
}

/// The ahead line's dash, in line widths: a round 5-unit dot every 14
/// units (spec §6.3, sized for a phone after the slice polish).
pub const AHEAD_DASH: [f64; 2] = [0.0, 2.8];
/// The ahead line's width, units: its dots' diameter.
pub const AHEAD_W: f64 = 5.0;
/// The drawn route's casing and core widths, units (spec §6.3: 1.5x the
/// first slice's 11 and 6, which read thin on a phone).
pub const ROUTE_CASING_W: f64 = 16.5;
pub const ROUTE_CORE_W: f64 = 9.0;
/// `tour`'s casing and core widths, units (spec §6.3).
pub const TOUR_CASING_W: f64 = 9.5;
pub const TOUR_CORE_W: f64 = 4.5;
/// `arrow`'s casing and core widths, units (spec §6.3).
pub const ARROW_CASING_W: f64 = 11.0;
pub const ARROW_CORE_W: f64 = 6.0;
/// `highlight`'s casing and core widths, units (spec §6.3).
pub const HIGHLIGHT_CASING_W: f64 = 13.0;
pub const HIGHLIGHT_CORE_W: f64 = 8.0;
/// An `area`'s fill opacity, and its `edge` line's opacity and width,
/// units (spec §6.3).
pub const AREA_FILL_OPACITY: f64 = 0.85;
pub const AREA_EDGE_OPACITY: f64 = 0.9;
pub const AREA_EDGE_W: f64 = 1.0;
/// `context`'s width, units, and its ink's opacity (spec §6.3).
pub const CONTEXT_W: f64 = 3.0;
pub const CONTEXT_OPACITY: f64 = 0.55;
/// Every casing's opacity (spec §6.3).
pub const CASING_OPACITY: f64 = 0.92;
/// The ahead dots' opacity (spec §6.3).
pub const AHEAD_OPACITY: f64 = 0.38;
/// The arrowhead, units (spec §6.3): a filled triangle this long, tip to
/// base, and this wide at its base, the base on the line's end.
pub const ARROWHEAD_LEN: f32 = 22.0;
pub const ARROWHEAD_W: f32 = 20.0;
/// The arrowhead's white edge, units (R7-a): the casing's own margin,
/// `(ARROW_CASING_W - ARROW_CORE_W) / 2`, at the casing's opacity, so
/// shaft and head read as one shape. With an author's `line-width` on the
/// core, the head's stub takes that width and the edge the casing's
/// margin beside it (none once the core is as wide as the casing); a core
/// wider than 16 units is narrowed under the head (the base keeps 2 units
/// of shoulder each side).
pub const ARROWHEAD_EDGE: f32 = ((ARROW_CASING_W - ARROW_CORE_W) / 2.0) as f32;

/// The colour and width the `arrow` role's core is drawn with, from its
/// merged paint ([`expand`]: the theme's, the author's keys over it): a
/// constant `line-color` (any CSS form the line takes, times a constant
/// `line-opacity`) and a constant `line-width`, falling back to `accent`
/// and [`ARROW_CORE_W`] where the paint has an expression or no value.
/// What the arrowhead and its stub take, so the head is the shaft's own
/// colour and the stub the shaft's own width.
#[must_use]
pub fn arrow_core_stroke(paint: &Map<String, Value>, accent: [u8; 3]) -> ([u8; 4], f64) {
    let parsed = paint.get("line-color").and_then(Value::as_str).and_then(|text| {
        crate::style_spec::color::css_named_color(text).or_else(|| crate::style_spec::parse_css_color(text).ok())
    });
    let [r, g, b, a] = parsed.map_or([accent[0], accent[1], accent[2], 255], crate::story::effects::rgba_bytes);
    let opacity = paint.get("line-opacity").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0);
    let a = (f64::from(a) * opacity).round() as u8;
    let width = paint.get("line-width").and_then(Value::as_f64).filter(|w| w.is_finite() && *w > 0.0).unwrap_or(ARROW_CORE_W);
    ([r, g, b, a], width)
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn paint(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// A line sublayer's paint (spec §6.3): colour, opacity (`None`: opaque),
/// width, `line-join: round` and the given cap.
fn line_paint(colour: [u8; 3], opacity: Option<f64>, width: f64, cap: &str) -> Map<String, Value> {
    let mut p = paint(json!({"line-color": hex(colour), "line-width": width, "line-cap": cap, "line-join": "round"}));
    if let Some(o) = opacity {
        p.insert("line-opacity".into(), json!(o));
    }
    p
}

/// The core colour of `route` and `tour` under `theme` (spec §6.2's
/// table): the accent under `ember` and `ocean`, ink under `ink` and
/// `fill`.
#[must_use]
pub fn line_core(theme: Theme, accent: [u8; 3]) -> [u8; 3] {
    match theme {
        Theme::Ember | Theme::Ocean => accent,
        Theme::Ink | Theme::Fill => INK,
    }
}

/// The role a layer with no `role` and an empty `paint` takes (spec
/// §6.1): a line with a `draw` effect is a `route`, any other line
/// `context`; a circle layer in a tour story is `numbered`, one with a
/// single feature an `event`, any other `stops`; a fill is an `area`.
/// The caller asks only for such a layer: one with `paint` is drawn as
/// 1a draws it, never inferred. Every arm's own string is checked
/// (debug only) against [`crate::story::script::ROLES`] -- the one role
/// list (R10 review round 1) -- so a role added here alone, never there,
/// panics in a debug or test build instead of drifting silently.
#[must_use]
pub fn infer_role(kind: OverlayLayerKind, draws: bool, tour: bool, feature_count: usize) -> &'static str {
    let role = match kind {
        OverlayLayerKind::Line if draws => "route",
        OverlayLayerKind::Line => "context",
        OverlayLayerKind::Circle if tour => "numbered",
        OverlayLayerKind::Circle if feature_count == 1 => "event",
        OverlayLayerKind::Circle => "stops",
        OverlayLayerKind::Fill => "area",
    };
    debug_assert_eq!(crate::story::script::role_kind(role), Some(kind), "`{role}` is not in script::ROLES at its own kind");
    role
}

/// The [`OverlayLayerKind`] a `role` string needs, delegating to
/// [`crate::story::script::role_kind`] -- the one role list (R10 review
/// round 1): `None` when `role` names none of the nine roles --
/// validation's `unknown_role`. A layer whose own kind does not match is
/// validation's `role_wrong_type`.
#[must_use]
pub fn role_kind(role: &str) -> Option<OverlayLayerKind> {
    crate::story::script::role_kind(role)
}

/// `source`'s sublayers for `role` under `theme` (spec §6.3), bottom to
/// top, or `None` when the role is not one [`expand`] themes or is the
/// wrong kind for the layer (it then draws as declared).
///
/// `accent` is the story's (the theme's, or `design.accent`). `author` is
/// the layer's own `paint`: merged over the core sublayer key by key, the
/// theme's paint first (spec §6.1). `draws`: the layer has a `draw`
/// effect, so a `route` or `tour` has an `ahead` line (`p = 1` without
/// one, and the ahead line is absent). `legend` is an `area`'s values
/// (`card_layout::area_legend_for`): its fill, the accent without one.
#[must_use]
pub fn expand(
    source: &LayerSource,
    role: &str,
    theme: Theme,
    accent: [u8; 3],
    author: &Map<String, Value>,
    draws: bool,
    legend: Option<&AreaLegend>,
) -> Option<Vec<Sublayer>> {
    // The one role list (R10 review round 1): a `role` this crate's
    // `role_kind` does not know, or whose kind does not match
    // `source.kind`, is never themed -- the match below never reached
    // past a stale or mismatched role string.
    if role_kind(role) != Some(source.kind) {
        return None;
    }
    let id = &source.layer_id;
    let sub = |suffix: &str, paint: Map<String, Value>, trim: Trim| Sublayer {
        id: if suffix.is_empty() { id.clone() } else { format!("{id}/{suffix}") },
        paint,
        geojson: source.geojson.clone(),
        trim,
        marks: false,
        arrowhead: false,
        point: None,
        area_slot: false,
    };
    let white = [0xFF, 0xFF, 0xFF];
    let mut subs = match (role, source.kind) {
        ("route" | "tour", OverlayLayerKind::Line) => {
            let (casing_w, core_w) = if role == "route" { (ROUTE_CASING_W, ROUTE_CORE_W) } else { (TOUR_CASING_W, TOUR_CORE_W) };
            let mut subs = Vec::with_capacity(3);
            if draws {
                let mut ahead = line_paint(INK, Some(AHEAD_OPACITY), AHEAD_W, "round");
                ahead.insert("line-dasharray".into(), json!(AHEAD_DASH));
                subs.push(sub("ahead", ahead, Trim::Ahead));
            }
            subs.push(sub("casing", line_paint(white, Some(CASING_OPACITY), casing_w, "round"), Trim::Same));
            subs.push(sub("", line_paint(line_core(theme, accent), None, core_w, "round"), Trim::Same));
            subs
        }
        ("arrow", OverlayLayerKind::Line) => {
            let mut core = sub("", line_paint(accent, None, ARROW_CORE_W, "butt"), Trim::Same);
            core.arrowhead = true;
            vec![sub("casing", line_paint(white, Some(CASING_OPACITY), ARROW_CASING_W, "round"), Trim::Same), core]
        }
        ("highlight", OverlayLayerKind::Line) => vec![
            sub("casing", line_paint(white, Some(CASING_OPACITY), HIGHLIGHT_CASING_W, "round"), Trim::Same),
            sub("", line_paint(accent, None, HIGHLIGHT_CORE_W, "round"), Trim::Same),
        ],
        ("context", OverlayLayerKind::Line) => vec![sub("", line_paint(INK, Some(CONTEXT_OPACITY), CONTEXT_W, "round"), Trim::Same)],
        ("stops" | "numbered" | "event", OverlayLayerKind::Circle) => {
            let role = PointRole::named(role)?;
            let looks = finished_looks(source, role, accent);
            let part = |suffix: &str, part: PointPart, paint: Map<String, Value>| Sublayer {
                id: if suffix.is_empty() { id.clone() } else { format!("{id}/{suffix}") },
                paint,
                geojson: part_geojson(&source.geojson, &looks, part),
                trim: Trim::Same,
                marks: false,
                arrowhead: false,
                point: Some(part),
                area_slot: false,
            };
            let shadow_paint = paint(json!({"circle-blur": MARKER_SHADOW_BLUR, "circle-translate": [MARKER_SHADOW_TRANSLATE.0, MARKER_SHADOW_TRANSLATE.1]}));
            vec![part("shadow", PointPart::Shadow, shadow_paint), part("halo", PointPart::Halo, Map::new()), part("", PointPart::Core, Map::new())]
        }
        ("area", OverlayLayerKind::Fill) => {
            let fill_color = legend.map_or_else(|| json!(hex(accent)), |l| l.fill_color(property_range(&source.geojson, &l.property)));
            let fill = paint(json!({"fill-color": fill_color, "fill-opacity": AREA_FILL_OPACITY}));
            let mut edge = sub("edge", line_paint(white, Some(AREA_EDGE_OPACITY), AREA_EDGE_W, "round"), Trim::Same);
            edge.geojson = edges_geojson(&source.geojson);
            let mut subs = vec![sub("", fill, Trim::Same), edge];
            for s in &mut subs {
                s.area_slot = true;
            }
            subs
        }
        _ => return None,
    };
    // The core is the top line sublayer (a point role's `Core`; an area's
    // fill, under its edge): it carries the 1a marks -- the rings; a
    // route's head is its own sublayer -- and the author's keys (spec
    // §6.1 "Merging").
    let core = if role == "area" { Some(0) } else { subs.iter().rposition(|s| s.point.is_none() || s.point == Some(PointPart::Core)) }?;
    let core = &mut subs[core];
    core.marks = role != "route";
    for (k, v) in author {
        core.paint.insert(k.clone(), v.clone());
    }
    // A route's `start` (its first vertex, the `stops` dot `done`) and,
    // with a draw, its `head` (the point at `p`, only while `0 < p < 1`,
    // placed per frame), above the line (spec §6.3).
    if role == "route" && source.kind == OverlayLayerKind::Line {
        let start = source.first_line.first().map(|&(lon, lat)| {
            let look = point_look(PointRole::Stops, PointState::Done, accent);
            let mut props = Map::new();
            if let Some(core) = look.core {
                disc_props(&mut props, core);
            }
            json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": props,
                "geometry": {"type": "Point", "coordinates": [lon, lat]}}]})
        });
        let empty = || json!({"type": "FeatureCollection", "features": []}).to_string();
        let point = |suffix: &str, part: PointPart, geojson: String| Sublayer {
            id: format!("{id}/{suffix}"),
            paint: Map::new(),
            geojson,
            trim: Trim::Same,
            marks: false,
            arrowhead: false,
            point: Some(part),
            area_slot: false,
        };
        subs.push(point("start", PointPart::Start, start.map_or_else(empty, |d| d.to_string())));
        if draws {
            subs.push(point("head", PointPart::Head, empty()));
        }
    }
    Some(subs)
}

/// The `(minimum, maximum)` of numeric property `key` over a GeoJSON
/// FeatureCollection's features, `None` when no feature has a finite one:
/// what a sequential area's equal spacing spreads over (spec §6.3).
#[must_use]
pub fn property_range(geojson: &str, key: &str) -> Option<(f64, f64)> {
    let doc: Value = serde_json::from_str(geojson).ok()?;
    doc.get("features")?
        .as_array()?
        .iter()
        .filter_map(|f| f.get("properties")?.get(key)?.as_f64().filter(|v| v.is_finite()))
        .fold(None, |acc, v| Some(acc.map_or((v, v), |(lo, hi): (f64, f64)| (lo.min(v), hi.max(v)))))
}

/// An area's boundaries (spec §6.3's `<id>/edge`): each Polygon or
/// MultiPolygon feature as a MultiLineString of its rings, its id and
/// properties kept; other features left out.
fn edges_geojson(geojson: &str) -> String {
    let mut doc: Value = serde_json::from_str(geojson).unwrap_or(Value::Null);
    if let Some(features) = doc.get_mut("features").and_then(Value::as_array_mut) {
        let kept: Vec<Value> = features
            .drain(..)
            .filter_map(|mut f| {
                let g = f.get("geometry")?;
                let coords = g.get("coordinates")?;
                let rings: Vec<Value> = match g.get("type").and_then(Value::as_str)? {
                    "Polygon" => coords.as_array()?.clone(),
                    "MultiPolygon" => coords.as_array()?.iter().filter_map(Value::as_array).flatten().cloned().collect(),
                    _ => return None,
                };
                f.as_object_mut()?.insert("geometry".into(), json!({"type": "MultiLineString", "coordinates": rings}));
                Some(f)
            })
            .collect();
        *features = kept;
    }
    doc.to_string()
}

/// A point role (spec §6.1): what its markers mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointRole {
    /// Places on a journey: ringed dots.
    Stops,
    /// Ranked stops: numbered badges.
    Numbered,
    /// Where something happened: always `now`.
    Event,
}

impl PointRole {
    /// The role named `role`, `None` for a line or area role.
    #[must_use]
    pub fn named(role: &str) -> Option<Self> {
        match role {
            "stops" => Some(Self::Stops),
            "numbered" => Some(Self::Numbered),
            "event" => Some(Self::Event),
            _ => None,
        }
    }
}

/// A point marker's state at `t` (spec §6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointState {
    Done,
    Next,
    Now,
    Muted,
}

impl PointState {
    /// This state's drop priority for the story-label 64-cap (Task R13,
    /// spec §7): `now > next > done > muted`, lower drops last. A frame
    /// over the cap keeps the lowest-numbered states first, then (within
    /// one state) the labels nearest the frame's centre
    /// (`capture::story_overlays::place_story_labels`).
    #[must_use]
    pub(crate) fn drop_rank(self) -> u8 {
        match self {
            Self::Now => 0,
            Self::Next => 1,
            Self::Done => 2,
            Self::Muted => 3,
        }
    }
}

/// Which marker sublayer a themed point sublayer is ([`Sublayer::point`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointPart {
    /// Every marker's shadow, under every marker of the layer.
    Shadow,
    /// The `now` ring and the outer disc (a numbered badge's disc).
    Halo,
    /// The core dot, the numerals and the labels.
    Core,
    /// A route's first vertex: the `stops` dot, `done`.
    Start,
    /// A route's draw head, at `p` while `0 < p < 1`.
    Head,
}

/// One disc of a marker, units and straight sRGB bytes: a fill to
/// `radius`, then a stroke `stroke_width` wide OUTSIDE it (MapLibre's
/// `circle-stroke-width`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Disc {
    pub radius: f32,
    pub fill: [u8; 4],
    pub stroke_width: f32,
    pub stroke: [u8; 4],
}

/// A numbered badge's numeral (spec §6.3): size in units, weight, colour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Numeral {
    pub size: f32,
    pub weight: u16,
    pub color: [u8; 4],
}

/// One marker's look in one state (spec §6.3), bottom to top: the ring
/// (`now` only), the outer disc, the core; and a badge's numeral.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointLook {
    pub ring: Option<Disc>,
    pub outer: Option<Disc>,
    pub core: Option<Disc>,
    pub numeral: Option<Numeral>,
}

/// The marker shadow (spec §6.3): ink at this opacity, `circle-blur`
/// this, translated this many units (x, y; y down), 1 unit larger than
/// the marker's outer radius.
pub const MARKER_SHADOW_OPACITY: f32 = 0.28;
pub const MARKER_SHADOW_BLUR: f32 = 0.5;
pub const MARKER_SHADOW_TRANSLATE: (f32, f32) = (0.0, 2.0);
pub const MARKER_SHADOW_GROW: f32 = 1.0;
/// A point label's distance from its marker's centre, units (spec §6.3):
/// 28, or 36 for a `now` marker; and the frame-edge clearance under which
/// it takes the other side.
pub const LABEL_OFFSET: f32 = 28.0;
pub const LABEL_OFFSET_NOW: f32 = 36.0;
pub const LABEL_EDGE: f32 = 24.0;
/// A point label's halo, units (spec §4.1's map label: white halo 6).
pub const LABEL_HALO: f32 = 6.0;
/// The `now` label's size, units (spec §4.1: "26 (current 30)").
pub const LABEL_SIZE_NOW: f32 = 30.0;

impl PointLook {
    /// The marker's outer radius: its widest disc with its stroke (the
    /// `now` ring, a halo of its own, not counted).
    #[must_use]
    pub fn outer_radius(&self) -> f32 {
        self.outer.iter().chain(self.core.iter()).map(|d| d.radius + d.stroke_width).fold(0.0, f32::max)
    }

    /// The marker shadow under this look (spec §6.3).
    #[must_use]
    pub fn shadow(&self) -> Disc {
        let [r, g, b] = INK;
        let a = (MARKER_SHADOW_OPACITY * 255.0).round() as u8;
        Disc { radius: self.outer_radius() + MARKER_SHADOW_GROW, fill: [r, g, b, a], stroke_width: 0.0, stroke: [0; 4] }
    }
}

const fn opaque([r, g, b]: [u8; 3]) -> [u8; 4] {
    [r, g, b, 0xFF]
}

/// `role`'s look in `state` (spec §6.3's point table).
#[must_use]
pub fn point_look(role: PointRole, state: PointState, accent: [u8; 3]) -> PointLook {
    use crate::story::design::MUTED;
    let white = opaque([0xFF, 0xFF, 0xFF]);
    let plain = |radius: f32, fill: [u8; 4]| Some(Disc { radius, fill, stroke_width: 0.0, stroke: [0; 4] });
    let a = opaque(accent);
    match (role, state) {
        (PointRole::Stops | PointRole::Event, PointState::Now) => PointLook {
            ring: Some(Disc { radius: 26.0, fill: [0; 4], stroke_width: 2.0, stroke: [accent[0], accent[1], accent[2], (0.45_f32 * 255.0).round() as u8] }),
            outer: plain(14.0, white),
            core: plain(10.0, a),
            numeral: None,
        },
        (PointRole::Stops | PointRole::Event, s) => PointLook {
            ring: None,
            outer: plain(11.5, white),
            core: plain(7.5, opaque(if s == PointState::Muted { MUTED } else { INK })),
            numeral: None,
        },
        (PointRole::Numbered, PointState::Now) => PointLook {
            ring: None,
            outer: Some(Disc { radius: 19.0, fill: a, stroke_width: 3.5, stroke: white }),
            core: None,
            numeral: Some(Numeral { size: 21.0, weight: 700, color: white }),
        },
        (PointRole::Numbered, s) => PointLook {
            ring: None,
            outer: Some(Disc { radius: 15.0, fill: white, stroke_width: 2.5, stroke: a }),
            core: None,
            numeral: Some(Numeral { size: 17.0, weight: 700, color: opaque(if s == PointState::Muted { MUTED } else { INK }) }),
        },
    }
}

/// The route's draw head (spec §6.3): `r 13` white, then `r 7.5` in the
/// accent, on the marker shadow.
#[must_use]
pub fn head_look(accent: [u8; 3]) -> PointLook {
    let plain = |radius: f32, fill: [u8; 4]| Some(Disc { radius, fill, stroke_width: 0.0, stroke: [0; 4] });
    PointLook { ring: None, outer: plain(13.0, opaque([0xFF, 0xFF, 0xFF])), core: plain(7.5, opaque(accent)), numeral: None }
}

/// A marker's state at `t` (spec §6.3):
/// - `numbered`: `now` when `number` is the card's index `card_index`,
///   `done` below it, `muted` above; every stop `done` with no index;
/// - `stops`: `along` is the stop's fraction along its route (`None`: no
///   route, read as drawn) and `p` the route's draw progress -- while the
///   draw runs, `done` at or behind the head and `next` ahead; once it
///   ends (`p >= 1`) the `last` stop is `now`, the rest `done`;
/// - `event`: always `now`.
#[must_use]
pub fn point_state(role: PointRole, number: u32, card_index: Option<u32>, along: Option<f32>, p: f32, last: bool) -> PointState {
    match role {
        PointRole::Event => PointState::Now,
        PointRole::Numbered => match card_index {
            Some(i) if number == i => PointState::Now,
            Some(i) if number > i => PointState::Muted,
            _ => PointState::Done,
        },
        PointRole::Stops => {
            let drawn = along.is_none() || p >= 1.0;
            if drawn {
                if last { PointState::Now } else { PointState::Done }
            } else if along.is_some_and(|a| a <= p + 1e-4) {
                PointState::Done
            } else {
                PointState::Next
            }
        }
    }
}

/// Whether feature `index` of `count` shows its label (spec §6.3): the
/// layer's `label` (`"all" | "current" | "ends" | "none"`), else the
/// role's default -- `numbered` the `now` stop, `stops` the first and
/// last, `event` all.
#[must_use]
pub fn labels_shown(role: PointRole, mode: Option<&str>, state: PointState, index: usize, count: usize) -> bool {
    let mode = mode.unwrap_or(match role {
        PointRole::Numbered => "current",
        PointRole::Stops => "ends",
        PointRole::Event => "all",
    });
    match mode {
        "all" => true,
        "current" => state == PointState::Now,
        "ends" => index == 0 || index + 1 == count,
        _ => false,
    }
}

/// Which side of its marker a point label sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelSide {
    Left,
    Right,
}

impl LabelSide {
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// A point label's box on `side` of `anchor` (`(w, h)` its text, `offset`
/// from the centre to the text's near edge), vertically centred: `(x0, y0,
/// x1, y1)` in the frame's units.
#[must_use]
pub fn label_box(anchor: (f32, f32), (w, h): (f32, f32), offset: f32, side: LabelSide) -> crate::story::tiers::Rect {
    let (y0, y1) = (anchor.1 - h / 2.0, anchor.1 + h / 2.0);
    match side {
        LabelSide::Right => (anchor.0 + offset, y0, anchor.0 + offset + w, y1),
        LabelSide::Left => (anchor.0 - offset - w, y0, anchor.0 - offset, y1),
    }
}

/// Whether a label box (frame units) keeps [`LABEL_EDGE`] clear of the
/// frame's left and right edges and meets none of the safe area's
/// `rails` (spec §6.3): [`place_label`]'s test for either side, and the
/// capture's when a card pushes a label to its other side.
#[must_use]
pub fn label_clear(r: crate::story::tiers::Rect, frame: (f32, f32), rails: &[crate::story::tiers::Rect]) -> bool {
    r.0 >= LABEL_EDGE && r.2 <= frame.0 - LABEL_EDGE && !rails.iter().any(|q| r.0 < q.2 && q.0 < r.2 && r.1 < q.3 && q.1 < r.3)
}

/// Where a point label goes (spec §6.3): on `preferred`, unless that box
/// ends within [`LABEL_EDGE`] of the frame's left or right edge or meets
/// one of the safe area's `rails` (`tiers::safe_area_rects`) and the other
/// side's box does neither; then on the other side.
#[must_use]
pub fn place_label(
    anchor: (f32, f32),
    size: (f32, f32),
    offset: f32,
    preferred: LabelSide,
    frame: (f32, f32),
    rails: &[crate::story::tiers::Rect],
) -> (LabelSide, crate::story::tiers::Rect) {
    let clear = |r: crate::story::tiers::Rect| label_clear(r, frame, rails);
    let first = label_box(anchor, size, offset, preferred);
    if clear(first) {
        return (preferred, first);
    }
    let other = label_box(anchor, size, offset, preferred.other());
    if clear(other) { (preferred.other(), other) } else { (preferred, first) }
}

/// A stop's number (spec §6.3): its `n` property, else its 1-based
/// position in the layer.
#[must_use]
pub fn stop_number(properties: Option<&Value>, position: usize) -> u32 {
    properties
        .and_then(|p| p.get("n"))
        .and_then(|n| n.as_u64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0).map(|f| f as u64)))
        .map_or(position as u32 + 1, |n| n.min(u64::from(u32::MAX)) as u32)
}

/// `disc` as the circle paint a live overlay feature carries.
pub fn disc_props(props: &mut Map<String, Value>, disc: Disc) {
    let hex4 = |[r, g, b, a]: [u8; 4]| format!("#{r:02x}{g:02x}{b:02x}{a:02x}");
    props.insert("circle-radius".into(), json!(disc.radius));
    props.insert("circle-color".into(), json!(hex4(disc.fill)));
    if disc.stroke_width > 0.0 {
        props.insert("circle-stroke-width".into(), json!(disc.stroke_width));
        props.insert("circle-stroke-color".into(), json!(hex4(disc.stroke)));
    }
}

/// Each feature's look at the finished frame (`p = 1`, no card index):
/// what the live map draws until it reads states per frame.
fn finished_looks(source: &LayerSource, role: PointRole, accent: [u8; 3]) -> Vec<PointLook> {
    let doc: Value = serde_json::from_str(&source.geojson).unwrap_or(Value::Null);
    let features = doc.get("features").and_then(Value::as_array).cloned().unwrap_or_default();
    let n = features.len();
    features
        .iter()
        .enumerate()
        .map(|(k, f)| {
            let number = stop_number(f.get("properties"), k);
            point_look(role, point_state(role, number, None, None, 1.0, k + 1 == n), accent)
        })
        .collect()
}

/// `geojson` with `part`'s disc of each feature's look in its properties
/// (a feature with no such disc left out): the live map's source.
fn part_geojson(geojson: &str, looks: &[PointLook], part: PointPart) -> String {
    let mut doc: Value = serde_json::from_str(geojson).unwrap_or(Value::Null);
    if let Some(features) = doc.get_mut("features").and_then(Value::as_array_mut) {
        let kept: Vec<Value> = features
            .drain(..)
            .zip(looks)
            .filter_map(|(mut f, look)| {
                let disc = match part {
                    PointPart::Shadow => Some(look.shadow()),
                    PointPart::Halo => look.outer,
                    _ => look.core,
                }?;
                let props = f.as_object_mut()?.entry("properties").or_insert_with(|| Value::Object(Map::new()));
                if let Some(p) = props.as_object_mut() {
                    disc_props(p, disc);
                }
                Some(f)
            })
            .collect();
        *features = kept;
    }
    doc.to_string()
}

/// Each line's end in a GeoJSON FeatureCollection, as `(last vertex, the
/// vertex before it)`, `(lon, lat)`: one per LineString (a
/// MultiLineString's parts each count), in document order, skipping a
/// line with no two distinct vertices. Where an arrowhead sits and which
/// way it points (spec §6.3).
#[must_use]
pub fn line_ends(geojson: &str) -> Vec<((f64, f64), (f64, f64))> {
    fn pos(v: &Value) -> Option<(f64, f64)> {
        Some((v.get(0)?.as_f64()?, v.get(1)?.as_f64()?))
    }
    fn end(line: &Value) -> Option<((f64, f64), (f64, f64))> {
        let pts: Vec<(f64, f64)> = line.as_array()?.iter().filter_map(pos).collect();
        let tip = *pts.last()?;
        let from = pts.iter().rev().find(|p| **p != tip)?;
        Some((tip, *from))
    }
    let doc: Value = serde_json::from_str(geojson).unwrap_or(Value::Null);
    let mut out = Vec::new();
    for f in doc.get("features").and_then(Value::as_array).into_iter().flatten() {
        let Some(g) = f.get("geometry") else { continue };
        let coords = g.get("coordinates");
        match g.get("type").and_then(Value::as_str) {
            Some("LineString") => out.extend(coords.and_then(end)),
            Some("MultiLineString") => out.extend(coords.and_then(Value::as_array).into_iter().flatten().filter_map(end)),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §6.3's point table, every role and state: the discs bottom to
    /// top (ring, outer, core) and the numeral.
    #[test]
    fn each_point_role_and_state_matches_the_spec_table() {
        const W: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
        const K: [u8; 4] = [0x0A, 0x0F, 0x1A, 0xFF];
        const M: [u8; 4] = [0x7A, 0x81, 0x8E, 0xFF];
        const A: [u8; 4] = [0xE8, 0x59, 0x0C, 0xFF];
        let disc = |radius: f32, fill: [u8; 4], stroke_width: f32, stroke: [u8; 4]| Disc { radius, fill, stroke_width, stroke };
        let plain = |radius: f32, fill: [u8; 4]| disc(radius, fill, 0.0, [0; 4]);
        let ring = disc(26.0, [0; 4], 2.0, [0xE8, 0x59, 0x0C, 115]);
        for role in [PointRole::Stops, PointRole::Event] {
            for state in [PointState::Done, PointState::Next] {
                let look = point_look(role, state, EMBER);
                assert_eq!((look.ring, look.outer, look.core, look.numeral), (None, Some(plain(11.5, W)), Some(plain(7.5, K)), None), "{role:?} {state:?}");
            }
            let now = point_look(role, PointState::Now, EMBER);
            assert_eq!((now.ring, now.outer, now.core), (Some(ring), Some(plain(14.0, W)), Some(plain(10.0, A))), "{role:?} now");
            let muted = point_look(role, PointState::Muted, EMBER);
            assert_eq!((muted.outer, muted.core), (Some(plain(11.5, W)), Some(plain(7.5, M))), "{role:?} muted");
        }
        for state in [PointState::Done, PointState::Next] {
            let look = point_look(PointRole::Numbered, state, EMBER);
            assert_eq!((look.ring, look.outer, look.core), (None, Some(disc(15.0, W, 2.5, A)), None));
            assert_eq!(look.numeral, Some(Numeral { size: 17.0, weight: 700, color: K }));
        }
        let now = point_look(PointRole::Numbered, PointState::Now, EMBER);
        assert_eq!((now.ring, now.outer, now.core), (None, Some(disc(19.0, A, 3.5, W)), None));
        assert_eq!(now.numeral, Some(Numeral { size: 21.0, weight: 700, color: W }));
        let muted = point_look(PointRole::Numbered, PointState::Muted, EMBER);
        assert_eq!(muted.outer, Some(disc(15.0, W, 2.5, A)));
        assert_eq!(muted.numeral, Some(Numeral { size: 17.0, weight: 700, color: M }));
        // `event` is always `now`.
        assert_eq!(point_state(PointRole::Event, 1, Some(9), Some(0.5), 0.2, false), PointState::Now);
    }

    /// Spec §6.3: a numbered stop is `now` when its number is the card's
    /// `index.i`, `done` below, `muted` above; with no index, `done`.
    #[test]
    fn numbered_states_follow_the_index() {
        let s = |n: u32, i: Option<u32>| point_state(PointRole::Numbered, n, i, None, 1.0, false);
        assert_eq!([s(1, Some(3)), s(3, Some(3)), s(5, Some(3))], [PointState::Done, PointState::Now, PointState::Muted]);
        assert_eq!(s(4, None), PointState::Done);
    }

    #[test]
    fn stops_behind_the_head_are_done_and_the_last_is_now_after_the_draw() {
        let s = |along: f32, p: f32, last: bool| point_state(PointRole::Stops, 1, None, Some(along), p, last);
        // Mid-draw: behind the head done, ahead next, the last still next.
        assert_eq!([s(0.0, 0.62, false), s(0.5, 0.62, false), s(0.8, 0.62, false), s(1.0, 0.62, true)],
            [PointState::Done, PointState::Done, PointState::Next, PointState::Next]);
        // Drawn: the last is now, the rest done.
        assert_eq!([s(0.0, 1.0, false), s(1.0, 1.0, true)], [PointState::Done, PointState::Now]);
        // No route: as drawn.
        assert_eq!(point_state(PointRole::Stops, 2, None, None, 1.0, true), PointState::Now);
        assert_eq!(point_state(PointRole::Stops, 1, None, None, 1.0, false), PointState::Done);
    }

    /// Spec §6.3: every marker's shadow is a circle 1 larger than its
    /// outer radius (stroke included, the `now` ring not), ink at .28,
    /// blur .5, translated (0, 2).
    #[test]
    fn every_marker_sits_on_its_shadow() {
        let ink = [0x0A, 0x0F, 0x1A, (0.28_f32 * 255.0).round() as u8];
        for (role, state, outer) in [
            (PointRole::Stops, PointState::Done, 11.5),
            (PointRole::Stops, PointState::Now, 14.0),
            (PointRole::Event, PointState::Muted, 11.5),
            (PointRole::Numbered, PointState::Done, 17.5),
            (PointRole::Numbered, PointState::Now, 22.5),
        ] {
            let shadow = point_look(role, state, EMBER).shadow();
            assert_eq!((shadow.radius, shadow.fill, shadow.stroke_width), (outer + 1.0, ink, 0.0), "{role:?} {state:?}");
            assert_eq!((MARKER_SHADOW_BLUR, MARKER_SHADOW_TRANSLATE), (0.5, (0.0, 2.0)));
        }
        let head = head_look(EMBER);
        assert_eq!((head.outer.map(|d| d.radius), head.core.map(|d| d.radius), head.shadow().radius), (Some(13.0), Some(7.5), 14.0));
    }

    #[test]
    fn labels_shown_by_default_and_by_override() {
        use PointState::{Done, Muted, Now};
        let shown = |role, mode: Option<&str>, state, i, n| labels_shown(role, mode, state, i, n);
        // Defaults: numbered the `now` stop; stops the first and last; event all.
        assert!(shown(PointRole::Numbered, None, Now, 2, 5) && !shown(PointRole::Numbered, None, Done, 0, 5));
        assert!(shown(PointRole::Stops, None, Done, 0, 3) && shown(PointRole::Stops, None, Done, 2, 3) && !shown(PointRole::Stops, None, Done, 1, 3));
        assert!(shown(PointRole::Event, None, Now, 1, 3));
        // Overrides.
        assert!(shown(PointRole::Stops, Some("all"), Muted, 1, 3));
        assert!(!shown(PointRole::Event, Some("none"), Now, 0, 1));
        assert!(shown(PointRole::Stops, Some("current"), Now, 1, 3) && !shown(PointRole::Stops, Some("current"), Done, 0, 3));
        assert!(shown(PointRole::Numbered, Some("ends"), Muted, 4, 5) && !shown(PointRole::Numbered, Some("ends"), Now, 2, 5));
    }

    /// Spec §6.3: 28 from the centre (36 when `now`) on the preferred
    /// side; a label that would end within 24 of the frame edge -- or on a
    /// platform's rail (`tiers::safe_area_rects`) -- takes the other side.
    #[test]
    fn a_label_near_the_edge_takes_the_other_side() {
        let (frame, size) = ((1080.0, 1920.0), (200.0, 26.0));
        let place = |x: f32, side: LabelSide, offset: f32, rails: &[crate::story::tiers::Rect]| place_label((x, 900.0), size, offset, side, frame, rails);
        let (side, rect) = place(500.0, LabelSide::Right, LABEL_OFFSET, &[]);
        assert_eq!(side, LabelSide::Right);
        assert_eq!(rect, (528.0, 887.0, 728.0, 913.0), "28 from the centre, centred on it vertically");
        assert_eq!(place(500.0, LabelSide::Left, LABEL_OFFSET_NOW, &[]).1 .2, 464.0, "36 from the centre when now");
        // Within 24 of the right edge: the other side.
        assert_eq!(place(850.0, LabelSide::Right, LABEL_OFFSET, &[]).0, LabelSide::Left, "878 + 200 > 1056");
        assert_eq!(place(820.0, LabelSide::Right, LABEL_OFFSET, &[]).0, LabelSide::Right, "848 + 200 = 1048, clear of 1056");
        assert_eq!(place(200.0, LabelSide::Left, LABEL_OFFSET, &[]).0, LabelSide::Right, "172 - 200 < 24");
        // A rail on the right (a platform's buttons): the other side too.
        let rail = [(900.0, 600.0, 1080.0, 1500.0)];
        assert_eq!(place(700.0, LabelSide::Right, LABEL_OFFSET, &rail).0, LabelSide::Left, "728..928 meets the rail at 900");
        assert_eq!(place(700.0, LabelSide::Right, LABEL_OFFSET, &[]).0, LabelSide::Right);
        // Neither side fits: the preferred one stays.
        assert_eq!(place_label((540.0, 900.0), (1000.0, 26.0), LABEL_OFFSET, LabelSide::Right, frame, &[]).0, LabelSide::Right);
    }

    /// Spec §6.3: a route's `start` and `head` are point sublayers above
    /// its core, and only the head carries the draw head (once).
    #[test]
    fn the_head_is_drawn_once_with_its_themed_paint() {
        let subs = expand(&line(), "route", Theme::Ember, EMBER, &Map::new(), true, None).expect("themed");
        let ids: Vec<&str> = subs.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["l/ahead", "l/casing", "l", "l/start", "l/head"]);
        assert_eq!(subs.iter().filter(|s| s.point == Some(PointPart::Head)).count(), 1);
        assert_eq!(subs[3].point, Some(PointPart::Start));
        assert!(subs.iter().all(|s| !s.marks), "the 1a head disc is replaced by the themed head");
        let undrawn = expand(&line(), "route", Theme::Ember, EMBER, &Map::new(), false, None).expect("themed");
        assert!(undrawn.iter().all(|s| s.point != Some(PointPart::Head)), "p = 1 without a draw: no head");
    }

    const EMBER: [u8; 3] = [0xE8, 0x59, 0x0C];

    fn source(kind: OverlayLayerKind, geojson: Value, ids: Vec<u64>) -> LayerSource {
        LayerSource { geojson: geojson.to_string(), feature_ids: ids, ..LayerSource::empty("l", kind) }
    }

    fn line() -> LayerSource {
        source(OverlayLayerKind::Line, json!({"type": "FeatureCollection", "features": []}), vec![])
    }

    #[test]
    fn a_route_is_ahead_casing_core_bottom_to_top() {
        let subs = expand(&line(), "route", Theme::Ember, EMBER, &Map::new(), true, None).expect("themed");
        let ids: Vec<&str> = subs.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["l/ahead", "l/casing", "l", "l/start", "l/head"]);
        assert_eq!(subs[0].trim, Trim::Ahead);
        assert_eq!(subs[2].paint["line-color"], "#e8590c");
        assert_eq!(subs[1].paint["line-width"], 16.5);
        assert_eq!(subs[2].paint["line-width"], 9.0);
        assert_eq!(subs[0].paint["line-dasharray"], json!([0.0, 2.8]), "round dots: a zero-length dash");
        assert!(subs.iter().all(|s| !s.marks), "the themed head replaces the 1a head disc");
    }

    /// Spec §6.2's core colours and §6.3's widths, every line role under
    /// every theme: `(role, theme) -> [(sublayer, colour, opacity, width,
    /// cap)]`, bottom to top.
    #[test]
    fn each_line_role_expands_to_the_spec_table() {
        const INK_HEX: &str = "#0a0f1a";
        const WHITE: &str = "#ffffff";
        let themes = [(Theme::Ember, "#e8590c"), (Theme::Ink, "#e03131"), (Theme::Ocean, "#0b7285"), (Theme::Fill, "#e8590c")];
        for (theme, accent_hex) in themes {
            let accent = crate::story::design::accent(theme);
            assert_eq!(hex(accent), accent_hex);
            let core = if matches!(theme, Theme::Ember | Theme::Ocean) { accent_hex } else { INK_HEX };
            type Row<'a> = (&'a str, &'a str, Option<f64>, f64, &'a str);
            let table: [(&str, Vec<Row>); 5] = [
                ("route", vec![("l/ahead", INK_HEX, Some(0.38), 5.0, "round"), ("l/casing", WHITE, Some(0.92), 16.5, "round"), ("l", core, None, 9.0, "round")]),
                ("tour", vec![("l/ahead", INK_HEX, Some(0.38), 5.0, "round"), ("l/casing", WHITE, Some(0.92), 9.5, "round"), ("l", core, None, 4.5, "round")]),
                ("arrow", vec![("l/casing", WHITE, Some(0.92), 11.0, "round"), ("l", accent_hex, None, 6.0, "butt")]),
                ("highlight", vec![("l/casing", WHITE, Some(0.92), 13.0, "round"), ("l", accent_hex, None, 8.0, "round")]),
                ("context", vec![("l", INK_HEX, Some(0.55), 3.0, "round")]),
            ];
            for (role, want) in table {
                let all = expand(&line(), role, theme, accent, &Map::new(), true, None).unwrap_or_else(|| panic!("{role} is themed"));
                // The route's `start` and `head` are point sublayers (their
                // own test); the line rows are the rest.
                let subs: Vec<&Sublayer> = all.iter().filter(|s| s.point.is_none()).collect();
                let got: Vec<Row> = subs
                    .iter()
                    .map(|s| {
                        (
                            s.id.as_str(),
                            s.paint["line-color"].as_str().unwrap_or(""),
                            s.paint.get("line-opacity").and_then(Value::as_f64),
                            s.paint["line-width"].as_f64().unwrap_or(0.0),
                            s.paint["line-cap"].as_str().unwrap_or(""),
                        )
                    })
                    .collect();
                assert_eq!(got, want, "{role} under {theme:?}");
                assert!(subs.iter().all(|s| s.paint["line-join"] == "round"), "{role}: every join is round");
                let marks = usize::from(role != "route");
                assert_eq!(subs.iter().filter(|s| s.marks).count(), marks, "{role}: one core carries the 1a marks (a route's head is its own)");
                assert_eq!(subs.iter().any(|s| s.arrowhead), role == "arrow", "{role}: only an arrow has an arrowhead");
            }
        }
    }

    #[test]
    fn role_inference_follows_6_1() {
        use OverlayLayerKind::{Circle, Fill, Line};
        assert_eq!(infer_role(Line, true, false, 1), "route");
        assert_eq!(infer_role(Line, false, false, 1), "context");
        assert_eq!(infer_role(Line, true, true, 3), "route", "a drawn line is a route in a tour too");
        assert_eq!(infer_role(Circle, false, true, 1), "numbered", "a tour's circles are numbered, even one");
        assert_eq!(infer_role(Circle, true, false, 1), "event");
        assert_eq!(infer_role(Circle, false, false, 4), "stops");
        assert_eq!(infer_role(Circle, false, false, 0), "stops");
        assert_eq!(infer_role(Fill, false, false, 2), "area");
    }

    #[test]
    fn author_paint_overrides_the_core_key_by_key() {
        let author = paint(json!({"line-color": "#123456", "line-dasharray": [2, 1]}));
        let subs = expand(&line(), "route", Theme::Ember, EMBER, &author, true, None).expect("themed");
        let core = subs.iter().find(|s| s.id == "l").unwrap();
        assert_eq!(core.paint["line-color"], "#123456", "the author's key wins");
        assert_eq!(core.paint["line-width"], 9.0, "the theme's other keys stay");
        assert_eq!(core.paint["line-cap"], "round");
        assert_eq!(core.paint["line-dasharray"], json!([2, 1]), "an author key the theme has not is added");
        assert_eq!(subs[1].paint["line-color"], "#ffffff", "only the core takes the author's paint");
        assert!(!subs[1].paint.contains_key("line-dasharray"));
    }

    /// Spec §6.1 and §10.2: a layer with a `paint` and no `role` is drawn
    /// exactly as 1a draws it -- the caller never asks [`infer_role`] for
    /// it, and [`expand`] themes only roles it knows on the right kind.
    #[test]
    fn explicit_paint_is_never_restyled() {
        assert!(expand(&line(), "stops", Theme::Ember, [0, 0, 0], &Map::new(), false, None).is_none(), "stops needs a circle layer");
        assert!(expand(&line(), "no-such-role", Theme::Ember, [0, 0, 0], &Map::new(), false, None).is_none());
        let circle = source(OverlayLayerKind::Circle, json!({"type": "FeatureCollection", "features": []}), vec![]);
        assert!(expand(&circle, "route", Theme::Ember, [0, 0, 0], &Map::new(), false, None).is_none(), "route needs a line layer");
        assert!(expand(&circle, "area", Theme::Ink, [0, 0, 0], &Map::new(), false, None).is_none(), "area needs a fill layer");
        assert!(expand(&line(), "numbered", Theme::Ink, [0, 0, 0], &Map::new(), false, None).is_none(), "numbered needs a circle layer");
    }

    /// The slice's gap (P5): `ahead` was keyed on the parent's opacity, so
    /// a hidden route that is never drawn showed its ahead dots. The ahead
    /// line now follows the draw: absent without one, `[p, 1]` during it.
    #[test]
    fn ahead_follows_the_draw_not_the_opacity() {
        let hidden = LayerFx { layer_id: "l".into(), opacity: 0.0, ..LayerFx::default() };
        // No draw: no ahead line at all, so nothing shows whatever the opacity.
        let undrawn = expand(&line(), "route", Theme::Ember, EMBER, &Map::new(), false, None).expect("themed");
        assert!(undrawn.iter().all(|s| s.trim != Trim::Ahead && !s.id.ends_with("/ahead")), "a route with no draw has no ahead line");
        for s in &undrawn {
            assert_eq!(fan_out(&hidden, &s.id, s.trim).opacity, 0.0, "{}: a hidden undrawn route draws nothing", s.id);
        }
        // With a draw: the whole line before it starts, [p, 1] during it
        // (at the route's opacity), nothing once it is drawn.
        let ahead = |fx: &LayerFx| {
            let out = fan_out(fx, "l/ahead", Trim::Ahead);
            assert_eq!(out.layer_id, "l/ahead");
            (out.trim, out.opacity)
        };
        assert_eq!(ahead(&hidden), (None, 1.0), "before the draw: the whole route is ahead");
        let mid = LayerFx { trim: Some([0.0, 0.4]), opacity: 1.0, ..hidden.clone() };
        assert_eq!(ahead(&mid), (Some([0.4, 1.0]), 1.0));
        let fading = LayerFx { opacity: 0.5, ..mid.clone() };
        assert_eq!(ahead(&fading).1, 0.5, "the dots fade with their route once the draw runs");
        let done = LayerFx { trim: Some([0.0, 1.0]), ..mid.clone() };
        assert_eq!(ahead(&done).1, 0.0, "a drawn route has nothing ahead");
        let shown = LayerFx { opacity: 1.0, ..hidden.clone() };
        assert_eq!(ahead(&shown).1, 0.0, "a route shown whole before its draw has nothing ahead");
        let same = fan_out(&mid, "l/casing", Trim::Same);
        assert_eq!((same.trim, same.opacity, same.layer_id.as_str()), (Some([0.0, 0.4]), 1.0, "l/casing"));
    }

    #[test]
    fn line_ends_are_each_lines_last_vertex_and_the_one_before() {
        let doc = json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": [[0, 0], [1, 0], [2, 1], [2, 1]]}},
            {"type": "Feature", "properties": {}, "geometry": {"type": "MultiLineString", "coordinates": [[[5, 5], [6, 5]], [[7, 7]]]}},
            {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [9, 9]}}]});
        assert_eq!(line_ends(&doc.to_string()), [((2.0, 1.0), (1.0, 0.0)), ((6.0, 5.0), (5.0, 5.0))]);
    }

    /// A point role's sublayers (spec §6.3): the shadows, the halos (ring
    /// and outer disc), the cores; the live map's GeoJSON carries each
    /// part's disc at the finished frame (`p = 1`, no card index).
    #[test]
    fn a_point_role_is_shadow_halo_core_bottom_to_top() {
        let point = |id: u64| json!({"type": "Feature", "id": id, "properties": {}, "geometry": {"type": "Point", "coordinates": [0, 0]}});
        let s = source(OverlayLayerKind::Circle, json!({"type": "FeatureCollection", "features": [point(0), point(1)]}), vec![0, 1]);
        let subs = expand(&s, "stops", Theme::Ember, EMBER, &Map::new(), false, None).expect("themed");
        let ids: Vec<&str> = subs.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["l/shadow", "l/halo", "l"]);
        assert_eq!(subs.iter().map(|s| s.point).collect::<Vec<_>>(), [Some(PointPart::Shadow), Some(PointPart::Halo), Some(PointPart::Core)]);
        let core: Value = serde_json::from_str(&subs[2].geojson).unwrap();
        assert_eq!(core["features"][0]["properties"]["circle-radius"], 7.5);
        assert_eq!(core["features"][1]["properties"]["circle-radius"], 10.0, "the last stop is now once drawn");
        assert_eq!(core["features"][1]["properties"]["circle-color"], "#e8590cff");
        assert_eq!(subs[0].paint["circle-blur"], 0.5);
        assert!(subs[2].marks && !subs[0].marks);
    }

    // ---- `area` (R9, spec §6.3) ----

    fn polygon(props: Value) -> Value {
        json!({"type": "Feature", "properties": props, "geometry": {"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]}})
    }

    fn area_source(features: Vec<Value>) -> LayerSource {
        let ids = (0..features.len() as u64).collect();
        source(OverlayLayerKind::Fill, json!({"type": "FeatureCollection", "features": features}), ids)
    }

    /// A script with one `area` layer `zones` (its own `layer_legend`, if
    /// any) and one chapter card whose legend is `card_legend`.
    fn area_script(layer_legend: Value, card_legend: Value) -> crate::story::Script {
        let mut layer = json!({"id": "zones", "type": "fill", "role": "area"});
        if !layer_legend.is_null() {
            layer["legend"] = layer_legend;
        }
        let v = crate::story::test_util::minimal_json(json!({
            "scene": {"style": "s.json", "overlays": [{"id": "o", "data": {"type": "FeatureCollection", "features": []}, "layers": [layer]}]},
            "chapters": [{"id": "c", "title": "C", "from": 0.0, "to": 1.0, "card": {"headline": "H", "legend": card_legend}}]
        }));
        crate::story::parse(&serde_json::to_vec(&v).unwrap()).expect("parses")
    }

    fn card_item_of(script: &crate::story::Script) -> crate::story::card_layout::CardItem {
        let spec = script.chapters[0].card.as_ref().expect("a card");
        crate::story::card_layout::CardContent::from_spec_in(spec, script).items.first().cloned().expect("a legend item")
    }

    fn fill_of(script: &crate::story::Script, features: Vec<Value>) -> Value {
        let legend = crate::story::card_layout::area_legend_for(script, "zones");
        let subs = expand(&area_source(features), "area", Theme::Fill, EMBER, &Map::new(), false, legend.as_ref()).expect("themed");
        subs[0].paint["fill-color"].clone()
    }

    /// Spec §6.3 "One source of truth": the fill's colours and the card
    /// legend's are the same values -- from a card legend that names the
    /// layer, or from the layer's own legend shown by a `{layers}` card.
    #[test]
    fn the_area_fill_and_the_legend_come_from_the_same_values() {
        use crate::story::card_layout::CardItem;
        // Categorical, the layer's own legend; the card names the layer only.
        let script = area_script(
            json!({"type": "categorical", "property": "kind", "items": [{"label": "Park", "color": "#2f9e44"}, {"label": "Port"}]}),
            json!({"layers": ["zones"], "title": "Zones"}),
        );
        let CardItem::Legend { rows, title } = card_item_of(&script) else { panic!("categorical") };
        assert_eq!(title.as_deref(), Some("Zones"), "the card's own title");
        let fill = fill_of(&script, vec![polygon(json!({"kind": "Park"}))]);
        let mut want = vec![json!("match"), json!(["get", "kind"])];
        for r in &rows {
            want.push(json!(r.label));
            want.push(json!(format!("#{:02x}{:02x}{:02x}", r.color[0], r.color[1], r.color[2])));
        }
        want.push(json!("#868e96"));
        assert_eq!(fill, Value::Array(want), "every row's label and colour, then Other");
        assert_eq!(rows[1].color, crate::story::design::AREA_PALETTE[1], "a blank colour takes the palette's");
        // A card's hand-written items that disagree never reach the card:
        // the first legend with values (here the card's own) is THE one.
        let script = area_script(Value::Null, json!({"type": "categorical", "property": "kind", "items": [{"label": "Park", "color": "#123456"}], "layers": ["zones"]}));
        let CardItem::Legend { rows, .. } = card_item_of(&script) else { panic!("categorical") };
        assert_eq!(rows[0].color, [0x12, 0x34, 0x56]);
        assert_eq!(fill_of(&script, vec![])[3], "#123456");
        // Sequential: the ramp's stops are the interpolate's outputs.
        let script = area_script(Value::Null, json!({"type": "sequential", "property": "v", "stops": ["#000000", "#ffffff"], "ticks": ["0", "10"], "layers": ["zones"]}));
        let CardItem::Sequential { stops, .. } = card_item_of(&script) else { panic!("sequential") };
        let fill = fill_of(&script, vec![polygon(json!({"v": 0})), polygon(json!({"v": 10}))]);
        assert_eq!(stops, [[0, 0, 0], [0xFF, 0xFF, 0xFF]]);
        assert_eq!(fill, json!(["interpolate", ["linear"], ["get", "v"], 0.0, "#000000", 10.0, "#ffffff"]));
        // No legend anywhere: the accent, and a card that names the layer shows nothing made up.
        let script = area_script(Value::Null, json!({"layers": ["zones"]}));
        assert_eq!(fill_of(&script, vec![]), "#e8590c");
        assert!(crate::story::card_layout::CardContent::from_spec_in(script.chapters[0].card.as_ref().unwrap(), &script).items.is_empty());
    }

    /// Spec §6.3: the stops at equal spacing between the property's
    /// minimum and maximum over the layer's features, or at the author's
    /// `breaks`; the default ramp when no `stops` are given.
    #[test]
    fn a_sequential_area_interpolates_over_equal_or_given_breaks() {
        let features = vec![polygon(json!({"pop": 40})), polygon(json!({"pop": 10})), polygon(json!({"pop": 25})), polygon(json!({"name": "no value"}))];
        let ramp = ["#5c1a1b", "#9b2c20", "#d9480f", "#f59f00", "#ffe066"];
        // Default ramp, equal spacing over 10..40.
        let script = area_script(Value::Null, json!({"type": "sequential", "property": "pop", "layers": ["zones"]}));
        let fill = fill_of(&script, features.clone());
        let mut want = vec![json!("interpolate"), json!(["linear"]), json!(["get", "pop"])];
        for (x, c) in [10.0, 17.5, 25.0, 32.5, 40.0].iter().zip(ramp) {
            want.push(json!(x));
            want.push(json!(c));
        }
        assert_eq!(fill, Value::Array(want));
        // The author's breaks, one per stop.
        let script = area_script(Value::Null, json!({"type": "sequential", "property": "pop", "stops": ["#000000", "#808080", "#ffffff"], "breaks": [0, 5, 100], "layers": ["zones"]}));
        assert_eq!(fill_of(&script, features.clone()), json!(["interpolate", ["linear"], ["get", "pop"], 0.0, "#000000", 5.0, "#808080", 100.0, "#ffffff"]));
        // Breaks that do not fit the stops (count, order) fall back to equal spacing.
        let script = area_script(Value::Null, json!({"type": "sequential", "property": "pop", "stops": ["#000000", "#ffffff"], "breaks": [9, 3], "layers": ["zones"]}));
        assert_eq!(fill_of(&script, features.clone()), json!(["interpolate", ["linear"], ["get", "pop"], 10.0, "#000000", 40.0, "#ffffff"]));
        // One value only: nothing to spread over, the first stop.
        let script = area_script(Value::Null, json!({"type": "sequential", "property": "pop", "stops": ["#000000", "#ffffff"], "layers": ["zones"]}));
        assert_eq!(fill_of(&script, vec![polygon(json!({"pop": 3}))]), "#000000");
        assert_eq!(property_range(&json!({"type": "FeatureCollection", "features": features}).to_string(), "pop"), Some((10.0, 40.0)));
    }

    /// Spec §3.4 and §6.3: past the card's six rows, the first five keep
    /// their colours and the rest fold into one grey "Other" -- on the
    /// card and on the map alike.
    #[test]
    fn categories_past_the_palette_fold_into_other() {
        use crate::story::card_layout::{shown_rows, CardItem};
        let items: Vec<Value> = (0..8).map(|i| json!({"label": format!("c{i}")})).collect();
        let script = area_script(Value::Null, json!({"type": "categorical", "property": "k", "items": items, "layers": ["zones"]}));
        let CardItem::Legend { rows, .. } = card_item_of(&script) else { panic!("categorical") };
        let palette = crate::story::design::AREA_PALETTE;
        assert_eq!(rows.iter().map(|r| r.color).take(6).collect::<Vec<_>>(), palette, "the palette in item order");
        assert_eq!(rows[6].color, crate::story::design::OTHER, "past the palette: Other's grey");
        let (shown, folded) = shown_rows(&rows);
        assert!(folded);
        assert_eq!(shown.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["c0", "c1", "c2", "c3", "c4", "Other"]);
        let fill = fill_of(&script, vec![]);
        let arr = fill.as_array().expect("a match");
        let labels: Vec<&str> = arr[2..arr.len() - 1].iter().step_by(2).filter_map(Value::as_str).collect();
        assert_eq!(labels, ["c0", "c1", "c2", "c3", "c4"], "the card's five, nothing the card folded");
        assert_eq!(arr.last(), Some(&json!("#868e96")), "everything else: Other");
        assert_eq!(arr[3], "#e8590c");
        // Six items fit the card whole: no Other.
        let six: Vec<Value> = (0..6).map(|i| json!({"label": format!("c{i}")})).collect();
        let script = area_script(Value::Null, json!({"type": "categorical", "property": "k", "items": six, "layers": ["zones"]}));
        assert_eq!(fill_of(&script, vec![]).as_array().map(Vec::len), Some(2 + 12 + 1));
    }

    /// Spec §6.3: an area is its fill (`fill-opacity .85`) then its
    /// `edge` (white at .9, width 1, the rings as lines), both below the
    /// labels; the author's paint merges over the fill.
    #[test]
    fn an_area_is_its_fill_then_its_edge_below_the_labels() {
        let author = paint(json!({"fill-opacity": 0.6}));
        let multi = json!({"type": "Feature", "id": 1, "properties": {"k": "a"}, "geometry": {"type": "MultiPolygon",
            "coordinates": [[[[0, 0], [1, 0], [1, 1], [0, 0]]], [[[5, 5], [6, 5], [6, 6], [5, 5]]]]}});
        let subs = expand(&area_source(vec![polygon(json!({})), multi]), "area", Theme::Fill, EMBER, &author, false, None).expect("themed");
        assert_eq!(subs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["l", "l/edge"]);
        assert!(subs.iter().all(|s| s.slot(true, false) == (false, true)), "both under the labels, over the buildings (R9-a)");
        assert_eq!(subs[0].paint["fill-opacity"], 0.6, "the author's key over the theme's .85");
        assert_eq!(subs[0].paint["fill-color"], "#e8590c");
        assert_eq!((subs[1].paint["line-color"].as_str(), subs[1].paint["line-opacity"].as_f64(), subs[1].paint["line-width"].as_f64()), (Some("#ffffff"), Some(0.9), Some(1.0)));
        let edge: Value = serde_json::from_str(&subs[1].geojson).unwrap();
        assert_eq!(edge["features"][0]["geometry"]["type"], "MultiLineString");
        assert_eq!(edge["features"][1]["geometry"]["coordinates"].as_array().map(Vec::len), Some(2), "one line per ring");
        assert_eq!(edge["features"][1]["id"], 1);
        let plain = expand(&area_source(vec![]), "area", Theme::Fill, EMBER, &Map::new(), false, None).unwrap();
        assert_eq!(plain[0].paint["fill-opacity"], 0.85);
        let route = expand(&line(), "route", Theme::Ember, EMBER, &Map::new(), true, None).unwrap();
        assert!(route.iter().all(|s| s.slot(true, false) == (true, false)), "only an area is moved");
    }

    /// Ruling R9-b: ordered categories (decades) take one ramp. With
    /// `ramp: "sequential"`, each item without a colour takes the colour
    /// at its place along `stops`, first to last; the card's dots and the
    /// map's `match` are the same colours.
    #[test]
    fn ordered_categories_take_their_colours_from_one_ramp() {
        use crate::story::card_layout::CardItem;
        let script = area_script(
            json!({"type": "categorical", "ramp": "sequential", "property": "decade", "stops": ["#000000", "#ffffff"],
                "items": [{"label": "1960s"}, {"label": "1970s"}, {"label": "1980s", "color": "#ff0000"}, {"label": "1990s"}]}),
            json!({"layers": ["zones"]}),
        );
        let CardItem::Legend { rows, .. } = card_item_of(&script) else { panic!("categorical") };
        let colours: Vec<[u8; 3]> = rows.iter().map(|r| r.color).collect();
        assert_eq!(colours, [[0, 0, 0], [85, 85, 85], [255, 0, 0], [255, 255, 255]], "a third of the way, two thirds (an explicit colour wins), the end");
        let fill = fill_of(&script, vec![]);
        assert_eq!(fill, json!(["match", ["get", "decade"], "1960s", "#000000", "1970s", "#555555", "1980s", "#ff0000", "1990s", "#ffffff", "#868e96"]));
        // Without stops: the default ramp, first item at its first stop.
        let script = area_script(json!({"ramp": "sequential", "items": [{"label": "a"}, {"label": "b"}]}), json!({"layers": ["zones"]}));
        let CardItem::Legend { rows, .. } = card_item_of(&script) else { panic!("categorical") };
        assert_eq!([rows[0].color, rows[1].color], [crate::story::design::AREA_RAMP[0], crate::story::design::AREA_RAMP[4]]);
        // A bad ramp is refused by validation.
        let v = crate::story::test_util::minimal_json(json!({"scene": {"style": "s.json", "overlays": [{"id": "o", "data": {"type": "FeatureCollection", "features": []},
            "layers": [{"id": "zones", "type": "fill", "role": "area", "legend": {"ramp": "rainbow", "items": [{"label": "a"}]}}]}]}}));
        let err = crate::story::parse(&serde_json::to_vec(&v).unwrap()).expect_err("refused");
        assert!(err.iter().any(|d| d.code == "unknown_value" && d.path.ends_with("/legend/ramp")), "{err:?}");
    }

    /// R9 review: a repeated label is refused (`duplicate_label`), and
    /// even unvalidated the card's rows and the fill's `match` are the
    /// same rows -- `legend_rows` keeps the first and drops the repeat.
    #[test]
    fn a_repeated_label_is_refused_and_never_splits_card_and_fill() {
        use crate::story::card_layout::CardItem;
        let legend = json!({"property": "k", "items": [{"label": "Park", "color": "#00ff00"}, {"label": "Port"}, {"label": " Park ", "color": "#ff0000"}]});
        let v = crate::story::test_util::minimal_json(json!({"scene": {"style": "s.json", "overlays": [{"id": "o", "data": {"type": "FeatureCollection", "features": []},
            "layers": [{"id": "zones", "type": "fill", "role": "area", "legend": legend.clone()}]}]}}));
        let err = crate::story::parse(&serde_json::to_vec(&v).unwrap()).expect_err("refused");
        assert!(err.iter().any(|d| d.code == "duplicate_label" && d.path.ends_with("/legend/items/2/label")), "{err:?}");
        // Unvalidated (built straight from the spec): one row set.
        let spec: crate::story::script::CardLegendSpec = serde_json::from_value(legend).unwrap();
        let area = crate::story::card_layout::AreaLegend::from_spec(&spec).expect("values");
        let CardItem::Legend { rows, .. } = area.card_item(None, None) else { panic!("categorical") };
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["Park", "Port"], "the first Park kept");
        assert_eq!(rows[0].color, [0, 0xFF, 0]);
        assert_eq!(rows[1].color, crate::story::design::AREA_PALETTE[1], "the palette counts the rows kept");
        assert_eq!(area.fill_color(None), json!(["match", ["get", "k"], "Park", "#00ff00", "Port", "#3b5bdb", "#868e96"]));
    }

    /// Spec §6.3 precedence: a card legend with values beats the layer's
    /// own; of two cards naming the layer, the first (chapters, then
    /// events) wins and every card shows it.
    #[test]
    fn the_first_card_legend_with_values_wins_over_later_cards_and_the_layer() {
        use crate::story::card_layout::{area_legend_origin, CardContent, CardItem};
        let layer_legend = json!({"property": "k", "items": [{"label": "from-layer"}]});
        let script = area_script(layer_legend.clone(), json!({"layers": ["zones"], "property": "k", "items": [{"label": "from-card"}]}));
        let (area, path, name) = area_legend_origin(&script, "zones").expect("values");
        assert_eq!((path.as_str(), name.as_str()), ("/chapters/0/card/legend", "chapter \"c\""));
        let CardItem::Legend { rows, .. } = area.card_item(None, None) else { panic!() };
        assert_eq!(rows[0].label, "from-card", "a card's values beat the layer's own");
        // Two chapters: the first wins, and the second card shows it too.
        let v = crate::story::test_util::minimal_json(json!({
            "scene": {"style": "s.json", "overlays": [{"id": "o", "data": {"type": "FeatureCollection", "features": []},
                "layers": [{"id": "zones", "type": "fill", "role": "area", "legend": layer_legend}]}]},
            "chapters": [
                {"id": "a", "title": "A", "from": 0.0, "to": 1.0, "card": {"headline": "A", "legend": {"layers": ["zones"], "property": "k", "items": [{"label": "first"}]}}},
                {"id": "b", "title": "B", "from": 1.0, "to": 2.0, "card": {"headline": "B", "legend": {"layers": ["zones"], "property": "k", "items": [{"label": "second"}]}}}]
        }));
        let script = crate::story::parse(&serde_json::to_vec(&v).unwrap()).expect("parses");
        for c in &script.chapters {
            let content = CardContent::from_spec_in(c.card.as_ref().unwrap(), &script);
            let Some(CardItem::Legend { rows, .. }) = content.items.first() else { panic!("{}", c.id) };
            assert_eq!(rows[0].label, "first", "chapter {}: the first card's values", c.id);
        }
        assert_eq!(area_legend_origin(&script, "zones").map(|(_, p, _)| p).as_deref(), Some("/chapters/0/card/legend"));
    }

    /// Spec §3.4: with no `property`, a categorical fill reads `class` and
    /// a sequential one `value`.
    #[test]
    fn the_default_property_is_class_or_value() {
        let cat = area_script(Value::Null, json!({"layers": ["zones"], "items": [{"label": "a"}]}));
        assert_eq!(fill_of(&cat, vec![])[1], json!(["get", "class"]));
        let seq = area_script(Value::Null, json!({"layers": ["zones"], "type": "sequential", "stops": ["#000000", "#ffffff"]}));
        assert_eq!(fill_of(&seq, vec![polygon(json!({"value": 1})), polygon(json!({"value": 2}))])[2], json!(["get", "value"]));
        // Untyped: `stops` and no `items` is sequential (spec §3.4).
        let untyped = area_script(Value::Null, json!({"layers": ["zones"], "stops": ["#000000", "#ffffff"]}));
        assert_eq!(fill_of(&untyped, vec![polygon(json!({"value": 1})), polygon(json!({"value": 2}))])[0], "interpolate");
    }
}
