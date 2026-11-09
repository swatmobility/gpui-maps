//! A story's events resolved once into a timeline, and the scene state at
//! any story time as a pure function of it (spec §2.1 events, §4.1, plan
//! S3).
//!
//! **Exact and reproducible.** [`Timeline::state_at`] takes `t` as f64
//! story seconds (and the output fps, only to find the frame a settle
//! point falls on) and reads nothing else: no clock, no state from the
//! previous call. The state at `t` is the fold of every event with
//! `at <= t`, in time order and, at equal times, in document order, plus
//! the tweens active at `t`. So frame `n` is the same whether it is
//! evaluated first, last or alone (`random_access_is_bit_identical`).
//!
//! **Style generations.** Generation 0 is the scene's style document.
//! Every style edit, in time order, derives a new document from the one
//! before it (`set_layer_visibility`, `set_filter`, `set_paint_property`,
//! `set_layout_property` edit one layer; `set_style` replaces the whole
//! document through the caller's loader), and that document is generation
//! `n`. `state_at(t).style_generation` is the number of style edits with
//! `at <= t`. Editing a layer the document does not hold AT THAT TIME
//! (including one an earlier `set_style` removed) is an `unknown_layer`
//! diagnostic listing the nearest ids, never a panic. Compiling a
//! generation is the native `style_cache` module's job; this file only
//! edits JSON, so the browser and iOS shells include it unchanged.
//!
//! **Params.** Defaults come from the scene (`atmosphere`, `stars`,
//! `labels`); `raster_opacity` defaults to 1 and `dots.epoch_mix` to 0;
//! `dots.ppd`/`dots.radius_px` are unset (`None`) until an event sets
//! them. `sun_unix` advances with the story clock, `start_utc + t·rate`,
//! until a `sun.time` event pins it (a number is seconds after
//! `start_utc`, a string a UTC time). A tween over `[at, until)` sets
//! `from + (to − from)·easing(k)` (easing `"ease"` unless given, as
//! everywhere in the spec); from `until` on its `to` holds. Where tweens or
//! sets of one param overlap, the latest-starting event wins, a finished
//! tween's held `to` included. A param with bounds is range-checked when
//! set (`param_out_of_range`) and clamped when a tween's curve overshoots
//! them: `raster_opacity` and `dots.epoch_mix` to `[0, 1]`, `dots.ppd` and
//! `dots.radius_px` to `>= 0`.
//!
//! **Captions** are active on `[at, until)` (to the end of time with no
//! `until`), with `alpha` ramping linearly over `fade_s` (default 0.4 s)
//! at each end.
//!
//! **Track files.** The timeline reads none: tracks reach it already
//! loaded, through the [`CameraPath`]. The loader that reads
//! `scene.tracks[i].file` (a later task) prefixes that file's diagnostics
//! with `/scene/tracks/<i>/file` ([`super::tracks::for_scene_track`]).
//!
//! Like the other story modules, this one denies `unwrap`, `expect` and
//! indexing outside tests (spec §2, "No-panic contract").
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use super::camera_path::CameraPath;
use super::chapters::{self, Chapter};
use super::effects::{self, layer_fx_at, EffectPlan, LayerFx};
use super::overlay_layers::{compile_layer_filter, geometry_kind, id_matches, LayerSource, PropertyBag};
use super::script::{
    levenshtein, parse_utc, CalloutSpec, CaptionSpec, Diagnostic, Easing, EffectKind, EventAction, EventTime, ImageSpec, OverlayEffectSpec,
    PanelSpec, Script, Segment, Severity,
};
use super::tracks::{draw_track, Track, TrackSet, DRAW_TRACK_PREFIX};

/// A caption's fade at each end when `fade_s` is not given, in seconds.
pub const DEFAULT_FADE_S: f64 = 0.4;
/// A caption's text size when `size_px` is not given, in logical pixels.
pub const DEFAULT_CAPTION_SIZE_PX: f32 = 24.0;
/// How long a settle point waits for its whole cover when the `settle`
/// event names no `timeout_s` (and always, for a `hold` with `settle`).
pub const DEFAULT_SETTLE_TIMEOUT_S: f64 = 60.0;
/// The longest a settle point may wait (R1): an hour. Past it a script's
/// `timeout_s` is an error, never a `Duration` overflow in the player.
pub const MAX_SETTLE_TIMEOUT_S: f64 = 3600.0;
/// A caption's colour when `color` is not given: white (on a dark plate).
pub const DEFAULT_CAPTION_COLOR: [u8; 4] = [255, 255, 255, 255];
/// A settle point at `s` falls on the first frame `n` with `n / fps >= s`;
/// `s · fps` is compared with this slack, so a settle point written as
/// `n / fps` in decimal is frame `n`, not `n + 1`.
const SETTLE_FRAME_EPSILON: f64 = 1e-6;

/// Where on the frame a caption sits: the nine compass points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl Anchor {
    /// The style-spec keyword (`"bottom-left"` ...), as `text-anchor` spells it.
    pub fn from_keyword(keyword: &str) -> Option<Self> {
        Some(match keyword {
            "top-left" => Anchor::TopLeft,
            "top" => Anchor::Top,
            "top-right" => Anchor::TopRight,
            "left" => Anchor::Left,
            "center" => Anchor::Center,
            "right" => Anchor::Right,
            "bottom-left" => Anchor::BottomLeft,
            "bottom" => Anchor::Bottom,
            "bottom-right" => Anchor::BottomRight,
            _ => return None,
        })
    }
}

/// A caption on screen at some `t`.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveCaption {
    pub text: String,
    pub anchor: Anchor,
    pub offset_px: (f32, f32),
    pub size_px: f32,
    pub color: [u8; 4],
    pub plate: bool,
    /// In `[0, 1]`: the fade in and out.
    pub alpha: f32,
}

/// A panel, image or callout event's own payload (spec §4.2-§4.4): what
/// [`ActiveElement::kind`] draws. Each variant borrows nothing and carries
/// no computed layout -- `layout.rs` (a later task) reads it alongside
/// `alpha`/`enter` to place it on the frame.
#[derive(Clone, Debug, PartialEq)]
pub enum ElementKind {
    Panel(PanelSpec),
    Image(ImageSpec),
    Callout(CalloutSpec),
}

/// One panel, image or callout on screen at `t` (spec §4.1-§4.4).
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveElement {
    /// `"/events/<i>"`: stable, unique, and what diagnostics and layout
    /// ids use.
    pub id: String,
    pub kind: ElementKind,
    /// `0..1`, fading both IN over `fade_s` after `at` and OUT over the
    /// last `fade_s` before `until` (the caption rule, [`caption_alpha`]:
    /// `min(since, left) / fade_s`, clamped). An element whose whole span
    /// is shorter than `2 * fade_s` never reaches full opacity -- the in
    /// and out ramps meet at their crossing point in the middle, rather
    /// than snapping from one to the other.
    pub alpha: f32,
    /// `0` (just arrived) `..1` (settled) over `fade_s` after `at`; the
    /// slide-in offset is `(1 - enter) * SLIDE_UNITS` (a later task's
    /// job). The slide applies only on the way IN: `enter` depends only
    /// on `since`, never on `until` or how soon the element leaves, so
    /// (controller ruling, Task 3 review round 1) it never eases back
    /// down and there is no slide-out -- only `alpha`'s fade-out plays an
    /// element off screen.
    pub enter: f32,
    /// Seconds since `at` (callout hysteresis and pulse read it).
    pub since: f64,
}

/// The cheap parameters `set_param` and `tween` drive.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub raster_opacity: f32,
    pub epoch_mix: f32,
    pub dots_ppd: Option<f32>,
    pub dots_radius_px: Option<f32>,
    /// The sun's time, unix seconds.
    pub sun_unix: f64,
    pub atmosphere: bool,
    pub stars: bool,
    pub labels: bool,
    /// Day/night shading from the story clock (`scene.terminator`,
    /// `terminator.on`). Off by default: a story renders in daylight.
    pub terminator: bool,
}

/// Everything but the camera at one story time.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneState {
    pub style_generation: u64,
    /// The overlay ids shown, in scene declaration order. Every declared
    /// overlay starts shown.
    pub visible_overlays: Vec<String>,
    /// `(overlay id, geojson path)` for every declared overlay, in scene
    /// order: the scene's file until a `set_overlay_data` replaces it.
    pub overlay_data: Vec<(String, String)>,
    /// The basemap source a `set_basemap` switched to; `None` is the scene's.
    pub basemap: Option<String>,
    /// The globe a `set_globe` set; `None` leaves it to the camera pose.
    pub globe: Option<bool>,
    pub params: Params,
    pub captions: Vec<ActiveCaption>,
    /// Each of [`Self::captions`]' index in the script's `events`, aligned
    /// with it (validation names a caption's layout warning by it).
    pub caption_sources: Vec<usize>,
    /// Every panel, image and callout on screen at `t`, in document order.
    pub elements: Vec<ActiveElement>,
    /// One per overlay layer (`scene.overlays[*].layers[*]`), in
    /// declaration order: what its `overlay` effects make of it at `t`
    /// (spec §3). A layer no effect touches is [`LayerFx::default`].
    pub layer_fx: Vec<LayerFx>,
    /// `Some(timeout_s)` when the frame at `t` is a settle point.
    pub settle: Option<f64>,
}

/// Which parameter an event drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Param {
    RasterOpacity,
    EpochMix,
    DotsPpd,
    DotsRadius,
    SunTime,
    Atmosphere,
    Stars,
    Labels,
    Terminator,
}

impl Param {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "raster_opacity" => Param::RasterOpacity,
            "dots.epoch_mix" => Param::EpochMix,
            "dots.ppd" => Param::DotsPpd,
            "dots.radius_px" => Param::DotsRadius,
            "sun.time" => Param::SunTime,
            "atmosphere.on" => Param::Atmosphere,
            "stars.on" => Param::Stars,
            "labels.on" => Param::Labels,
            "terminator.on" => Param::Terminator,
            _ => return None,
        })
    }

    fn is_bool(self) -> bool {
        matches!(self, Param::Atmosphere | Param::Stars | Param::Labels | Param::Terminator)
    }

    /// The closed range a numeric param lives in; `None` for no bounds.
    fn bounds(self) -> Option<(f64, f64)> {
        match self {
            Param::RasterOpacity | Param::EpochMix => Some((0.0, 1.0)),
            Param::DotsPpd | Param::DotsRadius => Some((0.0, f64::INFINITY)),
            _ => None,
        }
    }

    fn in_range(self, v: f64) -> bool {
        self.bounds().is_none_or(|(lo, hi)| (lo..=hi).contains(&v))
    }

    /// `v` clamped into the bounds: a tween's curve (a CSS bezier's y may
    /// leave `[0, 1]`) never carries a param out of them.
    fn clamp(self, v: f64) -> f64 {
        self.bounds().map_or(v, |(lo, hi)| v.clamp(lo, hi))
    }
}

fn out_of_range(path: &str, name: &str, v: f64, p: Param) -> Diagnostic {
    let range = match p.bounds() {
        Some((lo, hi)) if hi.is_finite() => format!("[{lo}, {hi}]"),
        Some((lo, _)) => format!(">= {lo}"),
        None => "any number".to_string(),
    };
    error(path, "param_out_of_range", format!("`{name}` must be {range}, got {v}"), None)
}

/// A resolved parameter value. A `sun.time` number is already unix seconds.
#[derive(Clone, Copy, Debug)]
enum ParamValue {
    Number(f64),
    Bool(bool),
}

/// An event placed in time: `(at, until, document index, action, eased
/// fraction of a `tween_paint_property` step)`.
type TimedEvent<'a> = (f64, Option<f64>, usize, &'a EventAction, Option<f64>);

#[derive(Clone, Debug)]
struct CaptionPlan {
    until: f64,
    fade_s: f64,
    caption: ActiveCaption,
}

/// A resolved panel, image or callout event: [`Op::Element`]'s payload.
#[derive(Clone, Debug)]
pub struct ElementPlan {
    pub id: String,
    pub kind: ElementKind,
    pub until: f64,
    pub fade_s: f64,
}

/// What one resolved event does to the state.
#[derive(Clone, Debug)]
enum Op {
    /// A style edit: its generation is counted from `generation_times`.
    Style,
    ShowOverlay(usize),
    HideOverlay(usize),
    OverlayData(usize, String),
    Basemap(String),
    Globe(bool),
    Set(Param, ParamValue),
    /// `from`/`to` are unix seconds for `sun.time`.
    Tween { param: Param, from: f64, to: f64, until: f64, easing: Easing },
    Caption(CaptionPlan),
    Settle(f64),
    Element(ElementPlan),
    /// An `overlay` effect (spec §3). Its state at `t` is folded per
    /// layer by [`effects::layer_fx_at`], not in the event walk: a layer's
    /// FIRST effect decides its opacity before it (a shown-later layer
    /// starts hidden), which the walk up to `t` cannot see.
    Effect(EffectPlan),
}

#[derive(Clone, Debug)]
struct Event {
    at: f64,
    /// Its index in the script's `events`.
    source: usize,
    op: Op,
}

/// A script's events, resolved and sorted, with every style generation's
/// document.
#[derive(Clone, Debug)]
pub struct Timeline {
    /// Sorted by `at`, document order at equal times.
    events: Vec<Event>,
    /// `generations[n]` is generation `n`'s style document, shared so a
    /// `Timeline` clone or a player holding a document copies nothing.
    generations: Vec<Arc<Value>>,
    /// `generation_times[n - 1]` is when generation `n` takes effect,
    /// non-decreasing.
    generation_times: Vec<f64>,
    /// `generation_sources[n - 1]` is the index in `script.events` of the
    /// edit that made generation `n` (the last in document order, where
    /// several edits at one instant coalesced into it).
    generation_sources: Vec<usize>,
    base: Params,
    clock_start_unix: f64,
    clock_rate: f64,
    /// The scene's `(id, geojson)` per overlay, in declaration order.
    overlays: Vec<(String, String)>,
    /// `(time, timeout_s)` of every settle point: settle holds and events.
    settles: Vec<(f64, f64)>,
    duration: f64,
    /// Every chapter, resolved once by [`chapters::resolve`] (spec §2.1).
    chapters: Vec<Chapter>,
    /// Every resolved `caption` event's `(at, until-or-+INFINITY, text)`,
    /// in TIME order (the order `events` is sorted into).
    /// [`Timeline::caption_inputs`] appends `caption_cues` after these.
    caption_events: Vec<(f64, f64, String)>,
    /// `script.captions` as plain tuples (spec §5's explicit caption
    /// track), read once here so [`Timeline::caption_inputs`] needs no
    /// `Script` of its own.
    caption_cues: Vec<(f64, f64, String)>,
    /// Every panel/image/callout event's `(at, kind)`, in DOCUMENT order
    /// (F12): [`Timeline::element_plans`].
    element_plans: Vec<(f64, ElementKind)>,
    /// Every declared overlay layer, in declaration order: its source
    /// (an empty one when the caller passed none for it) and its effect
    /// plans in DOCUMENT order, as [`effects::layer_fx_at`] takes them.
    layers: Vec<(Arc<LayerSource>, Vec<EffectPlan>)>,
    /// Every non-`Error` diagnostic found while building this `Timeline`
    /// (today: `chapters::resolve`'s `chapter_past_end`). `Timeline::new`
    /// fails on an `Error`, so whatever reaches here succeeded despite
    /// these -- [`Timeline::warnings`] is the channel a warning survives
    /// through on success (review round 1, finding 1: `chapters::resolve`
    /// itself is not a dead end for a warning; `Timeline` isn't either).
    warnings: Vec<Diagnostic>,
}

fn error(path: impl Into<String>, code: &'static str, message: impl Into<String>, hint: Option<String>) -> Diagnostic {
    Diagnostic { severity: Severity::Error, path: path.into(), code, message: message.into(), hint }
}

/// The JSON key an action is written under, for diagnostic paths.
fn action_key(action: &EventAction) -> &'static str {
    match action {
        EventAction::SetLayerVisibility { .. } => "set_layer_visibility",
        EventAction::SetFilter { .. } => "set_filter",
        EventAction::SetPaintProperty { .. } => "set_paint_property",
        EventAction::SetLayoutProperty { .. } => "set_layout_property",
        EventAction::SetStyle { .. } => "set_style",
        EventAction::ShowOverlay { .. } => "show_overlay",
        EventAction::HideOverlay { .. } => "hide_overlay",
        EventAction::SetOverlayData { .. } => "set_overlay_data",
        EventAction::SetBasemap { .. } => "set_basemap",
        EventAction::SetGlobe { .. } => "set_globe",
        EventAction::Tween { .. } => "tween",
        EventAction::TweenPaintProperty { .. } => "tween_paint_property",
        EventAction::SetParam { .. } => "set_param",
        EventAction::Caption(_) => "caption",
        EventAction::Settle { .. } => "settle",
        EventAction::Overlay(_) => "overlay",
        EventAction::Panel(_) => "panel",
        EventAction::Image(_) => "image",
        EventAction::Callout(_) => "callout",
        EventAction::Card(_) => "card",
    }
}

impl Timeline {
    /// Resolve every event of `script` against `path` (for label times and
    /// the duration) and derive every style generation from `base_style`.
    /// `load_style` returns the document a `set_style` names, or `None`
    /// when it cannot be read. `layers` are the overlay layers' sources
    /// ([`super::overlay_layers::layer_sources`]), which resolve a
    /// `highlight`'s `ids`/`where` and a `drop`'s `time_property` into
    /// feature ids and give a `draw` its head; a declared layer with no
    /// source here gets an empty one (no features, no head). Every problem
    /// found is returned, each a `Severity::Error` with a path into the
    /// script; the `Err` is never empty.
    pub fn new(
        script: &Script,
        path: &CameraPath,
        base_style: Value,
        load_style: &dyn Fn(&str) -> Option<Value>,
        layers: &[LayerSource],
    ) -> Result<Self, Vec<Diagnostic>> {
        Self::build(script, path, base_style, load_style, layers, true)
    }

    /// [`new`](Self::new), with `check_end: false` for
    /// [`path_and_timeline`]'s intermediate rounds (F1b): an event after the
    /// end of a path that is not yet the final one is not an error there,
    /// and is kept, since only the FINAL path's duration is the story's.
    fn build(
        script: &Script,
        path: &CameraPath,
        base_style: Value,
        load_style: &dyn Fn(&str) -> Option<Value>,
        layers: &[LayerSource],
        check_end: bool,
    ) -> Result<Self, Vec<Diagnostic>> {
        let mut diags = Vec::new();
        let duration = path.duration();
        let clock_start_unix = parse_utc(&script.clock.start_utc).unwrap_or_else(|| {
            diags.push(error(
                "/clock/start_utc",
                "bad_utc",
                format!("`{}` is not a valid RFC 3339 UTC timestamp", script.clock.start_utc),
                Some("write it like \"2026-03-20T10:00:00Z\"".to_string()),
            ));
            0.0
        });
        let clock_rate = script.clock.rate;
        if !clock_rate.is_finite() {
            diags.push(error("/clock/rate", "bad_clock_rate", format!("clock rate {clock_rate} is not finite"), None));
        }
        // `geojson` is `None` for an overlay that instead names its
        // document via the new `data` field (spec §2.2, Task 2): this
        // player does not read `data` yet (that is Task 3/8's job), so
        // such an overlay is treated as having no document here, the
        // same as an old script whose `geojson` path never resolved.
        let overlays: Vec<(String, String)> =
            script.scene.overlays.iter().map(|o| (o.id.clone(), o.geojson.clone().unwrap_or_default())).collect();

        // Chapters (spec §2.1): resolved once against `path`, independent
        // of the event walk below. Only an ERROR diagnostic blocks
        // `Timeline::new` (F13): `chapter_past_end`, a warning, never
        // does, and this file has no channel to carry a warning back out
        // on success, so it is not kept past this point.
        let (chapters, chapter_diags) = chapters::resolve(script, path);
        let mut warnings = Vec::new();
        for d in chapter_diags {
            if d.severity == Severity::Error {
                diags.push(d);
            } else {
                warnings.push(d);
            }
        }

        // `output.poster_at` (spec §5, Task 16): the bundle's poster is
        // rendered at this second, so it must be one the story has. Only
        // against the FINAL path (F1b), like `event_after_end`.
        if let Some(at) = script.output.poster_at {
            if check_end && !(at.is_finite() && (0.0..=duration).contains(&at)) {
                diags.push(error(
                    "/output/poster_at",
                    "poster_outside_story",
                    format!("poster_at {at} s is outside the story's [0, {duration}] s"),
                    Some("choose a second within the story, or drop poster_at to use the first chapter's still".to_string()),
                ));
            }
        }

        // `script.captions` (spec §5's explicit caption track), read once
        // as plain tuples for `caption_inputs`.
        let caption_cues: Vec<(f64, f64, String)> = script.captions.iter().map(|c| (c.from, c.to, c.text.clone())).collect();

        let mut element_plans: Vec<(f64, ElementKind)> = Vec::new();
        let mut caption_events: Vec<(f64, f64, String)> = Vec::new();

        // Every declared overlay layer with its source (spec §2.2's one
        // namespace), and each effect plan with its document index, to be
        // handed to its layer in document order after the walk.
        let declared: Vec<Arc<LayerSource>> = script
            .scene
            .overlays
            .iter()
            .flat_map(|o| o.layers.iter())
            .map(|l| Arc::new(layers.iter().find(|s| s.layer_id == l.id).cloned().unwrap_or_else(|| LayerSource::empty(&l.id, l.kind))))
            .collect();
        let mut effect_plans: Vec<(usize, EffectPlan)> = Vec::new();

        // 1. Resolve every event's time, then sort (stable: document order
        //    at equal times).
        //    A `tween_paint_property` becomes its steps here, each an entry
        //    at its own time carrying its eased fraction, so later edits
        //    compose with it in time order like any other. Its values are
        //    made in the walk, where a missing `from` is read off the
        //    document current at `at`.
        let mut timed: Vec<TimedEvent<'_>> = Vec::with_capacity(script.events.len());
        for (i, event) in script.events.iter().enumerate() {
            let at = resolve_time(&event.at, path, &format!("/events/{i}/at"), &mut diags);
            let until = event.until.as_ref().and_then(|u| resolve_time(u, path, &format!("/events/{i}/until"), &mut diags));
            let Some(at) = at else { continue };
            if check_end && at > duration {
                diags.push(error(
                    format!("/events/{i}/at"),
                    "event_after_end",
                    format!("event {i} fires at {at} s, after the camera path ends at {duration} s"),
                    Some("move it earlier, or lengthen the camera path".to_string()),
                ));
                continue;
            }
            if let Some(until) = until {
                if until <= at {
                    diags.push(error(format!("/events/{i}/until"), "until_before_at", format!("`until` ({until}) is not after `at` ({at})"), None));
                    continue;
                }
            }
            if let EventAction::TweenPaintProperty { easing, .. } = &event.action {
                let Some(until) = until else {
                    diags.push(error(format!("/events/{i}/until"), "tween_needs_until", "a tween needs `until`, when it reaches `to`", None));
                    continue;
                };
                for (t, k) in paint_tween_times(easing.as_ref().unwrap_or(&Easing::Ease), at, until.min(duration), until) {
                    timed.push((t, None, i, &event.action, Some(k)));
                }
                continue;
            }
            match &event.action {
                EventAction::Panel(spec) => element_plans.push((at, ElementKind::Panel(spec.clone()))),
                EventAction::Image(spec) => element_plans.push((at, ElementKind::Image(spec.clone()))),
                EventAction::Callout(spec) => element_plans.push((at, ElementKind::Callout(spec.clone()))),
                _ => {}
            }
            timed.push((at, until, i, &event.action, None));
        }
        timed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.cmp(&b.2)));

        // 2. Walk them in time order: derive generations, resolve the rest.
        let mut generations = vec![Arc::new(base_style)];
        let mut generation_times = Vec::new();
        let mut generation_sources = Vec::new();
        let mut events = Vec::with_capacity(timed.len());
        // Each paint tween's ends, resolved at its first step (`None`: it
        // was diagnosed, and its steps are skipped).
        let mut tween_ends: HashMap<usize, Option<PaintEnds>> = HashMap::new();
        for (at, until, i, action, step) in timed {
            let stepped;
            let action: &EventAction = match (action, step) {
                (EventAction::TweenPaintProperty { layer, name, from, to, .. }, Some(k)) => {
                    let ends = tween_ends.entry(i).or_insert_with(|| {
                        let current = generations.last().map(|g| &**g).unwrap_or(&Value::Null);
                        paint_ends(current, layer, name, from.as_ref(), to, &format!("/events/{i}/tween_paint_property"), at, &mut diags)
                    });
                    let Some(ends) = ends.as_ref() else { continue };
                    stepped = EventAction::SetPaintProperty { layer: layer.clone(), name: name.clone(), value: ends.at(k) };
                    &stepped
                }
                (action, _) => action,
            };
            // A tween's steps are reported under the tween's own key.
            let key = script.events.get(i).map_or_else(|| action_key(action), |e| action_key(&e.action));
            let at_path = format!("/events/{i}/{key}");
            let op = match action {
                EventAction::SetStyle { style } => match load_style(style) {
                    Some(doc) => Some((Op::Style, Some(doc))),
                    None => {
                        diags.push(error(format!("{at_path}/style"), "style_not_found", format!("the style `{style}` could not be loaded"), None));
                        None
                    }
                },
                EventAction::SetLayerVisibility { layer, .. }
                | EventAction::SetFilter { layer, .. }
                | EventAction::SetPaintProperty { layer, .. }
                | EventAction::SetLayoutProperty { layer, .. } => {
                    let mut doc = generations.last().map(|g| Value::clone(g)).unwrap_or(Value::Null);
                    let layer_path = format!("{at_path}/layer");
                    if edit_layer(&mut doc, layer, action) {
                        Some((Op::Style, Some(doc)))
                    } else {
                        // Once per event, not once per tween step.
                        if !diags.iter().any(|d| d.path == layer_path) {
                            diags.push(unknown_layer(&doc, layer, &layer_path, at));
                        }
                        None
                    }
                }
                EventAction::Overlay(spec) => {
                    let source = declared.iter().find(|s| s.layer_id == spec.layer);
                    let loaded = layers.iter().any(|s| s.layer_id == spec.layer);
                    effect_plan(spec, at, until, source.map(|s| &**s), loaded, &at_path, &mut diags, &mut warnings).map(|plan| {
                        effect_plans.push((i, plan.clone()));
                        (Op::Effect(plan), None)
                    })
                }
                other => resolve_op(other, until, i, &at_path, &overlays, clock_start_unix, &mut diags).map(|op| (op, None)),
            };
            let Some((op, doc)) = op else { continue };
            if let Op::Caption(plan) = &op {
                // Time order, for free: `timed` is sorted by `at`.
                caption_events.push((at, plan.until, plan.caption.text.clone()));
            }
            if let Some(doc) = doc {
                // Edits at one instant coalesce into one generation (the
                // earlier ones could never be seen), and an edit (or an
                // instant's edits) that leaves the document as it was
                // makes none, so the player neither recompiles nor re-bakes.
                if generation_times.last() == Some(&at) {
                    generations.pop();
                    generation_times.pop();
                    generation_sources.pop();
                }
                if generations.last().is_none_or(|previous| **previous != doc) {
                    generations.push(Arc::new(doc));
                    generation_times.push(at);
                    generation_sources.push(i);
                }
            }
            events.push(Event { at, source: i, op });
        }

        let mut settles: Vec<(f64, f64)> = path.settle_times().into_iter().map(|s| (s, DEFAULT_SETTLE_TIMEOUT_S)).collect();
        settles.extend(events.iter().filter_map(|e| match e.op {
            Op::Settle(timeout) => Some((e.at, timeout)),
            _ => None,
        }));

        if !diags.is_empty() {
            return Err(diags);
        }
        effect_plans.sort_by_key(|(i, _)| *i);
        let layers = declared
            .into_iter()
            .map(|source| {
                let plans = effect_plans.iter().filter(|(_, p)| p.layer_id == source.layer_id).map(|(_, p)| p.clone()).collect();
                (source, plans)
            })
            .collect();
        let base = Params {
            raster_opacity: 1.0,
            epoch_mix: 0.0,
            dots_ppd: None,
            dots_radius_px: None,
            sun_unix: clock_start_unix,
            atmosphere: script.scene.atmosphere,
            stars: script.scene.stars,
            terminator: script.scene.terminator,
            labels: script.scene.labels,
        };
        Ok(Timeline {
            events,
            generations,
            generation_times,
            generation_sources,
            base,
            clock_start_unix,
            clock_rate,
            overlays,
            settles,
            duration,
            chapters,
            caption_events,
            caption_cues,
            element_plans,
            layers,
            warnings,
        })
    }

    /// The scene at story time `t`. `t` below zero (or NaN) is 0 and `+∞`
    /// is the duration. `fps` only places settle points: the frame at `t`
    /// (`round(t · fps)`) is a settle point when it is the first frame at
    /// or after one. Pure: the same `(t, fps)` always gives an identical
    /// state.
    pub fn state_at(&self, t: f64, fps: u32) -> SceneState {
        let t = if t.is_nan() {
            0.0
        } else if t == f64::INFINITY {
            self.duration
        } else {
            t.max(0.0)
        };
        let mut params = self.base.clone();
        params.sun_unix = self.clock_start_unix + t * self.clock_rate;
        let mut visible = vec![true; self.overlays.len()];
        let mut data: Vec<(String, String)> = self.overlays.clone();
        let mut basemap = None;
        let mut globe = None;
        let mut captions = Vec::new();
        let mut caption_sources = Vec::new();
        let mut elements = Vec::new();
        for event in self.events.iter().take_while(|e| e.at <= t) {
            match &event.op {
                Op::Style | Op::Settle(_) | Op::Effect(_) => {}
                Op::ShowOverlay(k) | Op::HideOverlay(k) => {
                    if let Some(v) = visible.get_mut(*k) {
                        *v = matches!(event.op, Op::ShowOverlay(_));
                    }
                }
                Op::OverlayData(k, geojson) => {
                    if let Some(entry) = data.get_mut(*k) {
                        entry.1 = geojson.clone();
                    }
                }
                Op::Basemap(source) => basemap = Some(source.clone()),
                Op::Globe(on) => globe = Some(*on),
                Op::Set(param, value) => apply(&mut params, *param, *value),
                Op::Tween { param, from, to, until, easing } => {
                    let value = if t < *until && *until > event.at {
                        let k = easing.apply((t - event.at) / (until - event.at));
                        let k = if k.is_finite() { k } else { (t - event.at) / (until - event.at) };
                        from + (to - from) * k
                    } else {
                        *to
                    };
                    apply(&mut params, *param, ParamValue::Number(value));
                }
                Op::Caption(plan) => {
                    if t < plan.until {
                        let mut caption = plan.caption.clone();
                        caption.alpha = caption_alpha(t - event.at, plan.until - t, plan.fade_s);
                        captions.push(caption);
                        caption_sources.push(event.source);
                    }
                }
                Op::Element(plan) => {
                    if t < plan.until {
                        let since = t - event.at;
                        let alpha = caption_alpha(since, plan.until - t, plan.fade_s);
                        let enter = if plan.fade_s <= 0.0 { 1.0 } else { (since / plan.fade_s).clamp(0.0, 1.0) as f32 };
                        elements.push(ActiveElement { id: plan.id.clone(), kind: plan.kind.clone(), alpha, enter, since });
                    }
                }
            }
        }
        let style_generation = self.generation_times.partition_point(|&g| g <= t) as u64;
        SceneState {
            style_generation,
            visible_overlays: self.overlays.iter().zip(&visible).filter(|(_, v)| **v).map(|((id, _), _)| id.clone()).collect(),
            overlay_data: data,
            basemap,
            globe,
            params,
            captions,
            caption_sources,
            elements,
            layer_fx: self.layers.iter().map(|(source, plans)| layer_fx_at(&source.layer_id, plans, t, source)).collect(),
            settle: self.settle_at(t, fps),
        }
    }

    /// Every resolved `overlay` effect, in time order (document order at
    /// equal times): what `follow_draw` builds its draw track from.
    pub fn effect_plans(&self) -> Vec<&EffectPlan> {
        self.events
            .iter()
            .filter_map(|e| match &e.op {
                Op::Effect(plan) => Some(plan),
                _ => None,
            })
            .collect()
    }

    /// Every resolved chapter, in document order (spec §2.1).
    pub fn chapters(&self) -> &[Chapter] {
        &self.chapters
    }

    /// Every non-error diagnostic found while building this `Timeline`
    /// (today: a chapter's `chapter_past_end`, clamped rather than
    /// refused). `Timeline::new` already failed on anything worse, so a
    /// caller that only wants to know whether the build succeeded can
    /// ignore this; one that wants to report what was clamped reads it
    /// from the same `Timeline` it already built, rather than re-deriving
    /// chapters a second time.
    pub fn warnings(&self) -> &[Diagnostic] {
        &self.warnings
    }

    /// Every panel, image and callout event's `(at, kind)`, in DOCUMENT
    /// order (the order `script.events` lists them) -- unlike
    /// [`state_at`](Self::state_at)'s per-`t` `elements`, which is in the
    /// order they are active at that instant. Task 16 consumes it for the
    /// transcript.
    pub fn element_plans(&self) -> Vec<(f64, &ElementKind)> {
        self.element_plans.iter().map(|(at, kind)| (*at, kind)).collect()
    }

    /// The globe switch in force at `t` -- `state_at(t, _).globe`, without
    /// building the rest of the state: the last `globe` event at or before
    /// `t` (`t` normalised as `state_at` does). What the player's callout
    /// schedule reads at every grid step (Task 13 review F3).
    pub fn globe_at(&self, t: f64) -> Option<bool> {
        let t = if t.is_nan() {
            0.0
        } else if t == f64::INFINITY {
            self.duration
        } else {
            t.max(0.0)
        };
        self.events.iter().take_while(|e| e.at <= t).filter_map(|e| if let Op::Globe(on) = e.op { Some(on) } else { None }).last()
    }

    /// Every callout event's `(id, at, until, spec)`, in TIME order, with
    /// `until` at most the story's duration (a callout with none runs to
    /// the end): the span the player computes its side schedule over
    /// (`callout::schedule`), keyed by the id its `ActiveElement` carries.
    pub fn callout_plans(&self) -> Vec<(String, f64, f64, &CalloutSpec)> {
        self.events
            .iter()
            .filter_map(|e| match &e.op {
                Op::Element(ElementPlan { id, kind: ElementKind::Callout(spec), until, .. }) => {
                    Some((id.clone(), e.at, until.min(self.duration), spec))
                }
                _ => None,
            })
            .collect()
    }

    /// Every panel, image and callout event's `(id, at, until, fade_s)`, in
    /// TIME order, with `until` at most the story's duration (an element
    /// with none runs to the end): the on-screen windows validation samples
    /// the layout over (Task 19).
    pub fn element_spans(&self) -> Vec<(String, f64, f64, f64)> {
        self.events
            .iter()
            .filter_map(|e| match &e.op {
                Op::Element(ElementPlan { id, until, fade_s, .. }) => Some((id.clone(), e.at, until.min(self.duration), *fade_s)),
                _ => None,
            })
            .collect()
    }

    /// Every caption's `(at, until-or-duration, text)`, in TIME order: a
    /// resolved `caption` event first, then `script.captions` (spec §5).
    /// A caption with no `until` runs to the story's own end, not
    /// `+INFINITY` (Task 15's SRT/VTT need a real number). Task 15
    /// consumes it.
    pub fn caption_inputs(&self) -> Vec<(f64, f64, String)> {
        let mut out: Vec<(f64, f64, String)> = self
            .caption_events
            .iter()
            .map(|(at, until, text)| (*at, if until.is_finite() { *until } else { self.duration }, text.clone()))
            .collect();
        out.extend(self.caption_cues.iter().cloned());
        out
    }

    /// Generation `generation`'s style document, `None` past the last.
    pub fn style_document(&self, generation: u64) -> Option<&Value> {
        self.style_document_shared(generation).map(|doc| &**doc)
    }

    /// [`style_document`](Self::style_document), shared: no copy.
    pub fn style_document_shared(&self, generation: u64) -> Option<&Arc<Value>> {
        usize::try_from(generation).ok().and_then(|g| self.generations.get(g))
    }

    /// How many generations there are: the scene's style, plus one per
    /// instant whose style edits changed the document.
    pub fn generations(&self) -> u64 {
        self.generations.len() as u64
    }

    /// The index in the script's `events` of the edit that made
    /// `generation` (the last one, in document order, of the instant that
    /// made it); `None` for generation 0 (the scene's style) and past the
    /// last.
    pub fn generation_source(&self, generation: u64) -> Option<usize> {
        let g = usize::try_from(generation).ok()?.checked_sub(1)?;
        self.generation_sources.get(g).copied()
    }

    /// `(time, timeout_s)` of every settle point (settle holds and settle
    /// events), in story seconds: what a live preview pauses its clock at.
    pub fn settle_points(&self) -> &[(f64, f64)] {
        &self.settles
    }

    /// Every resolved event as `(index in the script's events, at)`, in
    /// time order. The player uses it to name the events no frame shows.
    pub fn event_times(&self) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.events.iter().map(|e| (e.source, e.at))
    }

    /// The first frame at or after a settle point, if the frame at `t` is
    /// one; the longest timeout wins where two coincide.
    fn settle_at(&self, t: f64, fps: u32) -> Option<f64> {
        let fps = f64::from(fps.max(1));
        let frame = (t * fps).round();
        self.settles
            .iter()
            .filter(|(s, _)| (s * fps - SETTLE_FRAME_EPSILON).ceil().max(0.0) == frame)
            .map(|(_, timeout)| *timeout)
            .reduce(f64::max)
    }
}

/// `at`/`until` in story seconds, or a diagnostic at `path`.
fn resolve_time(time: &EventTime, path: &CameraPath, at: &str, diags: &mut Vec<Diagnostic>) -> Option<f64> {
    let (seconds, label_key) = match time {
        EventTime::At(s) => (Some(*s), None),
        EventTime::After { after, offset } => (path.label_time(after, false).map(|s| s + offset), Some(("after", after))),
        EventTime::AtStart { at_start, offset } => (path.label_time(at_start, true).map(|s| s + offset), Some(("at_start", at_start))),
    };
    match (seconds, label_key) {
        (None, Some((key, label))) => {
            let known: Vec<&str> = path.segments().iter().filter_map(|s| s.label.as_deref()).collect();
            let hint = nearest(label, &known).first().filter(|(_, d)| *d <= 2).map(|(k, _)| format!("did you mean `{k}`?"));
            diags.push(error(format!("{at}/{key}"), "unknown_label", format!("no camera segment is labelled `{label}`; known labels: {known:?}"), hint));
            None
        }
        (Some(s), _) if !s.is_finite() || s < 0.0 => {
            diags.push(error(at, "bad_event_time", format!("the event time {s} is not a finite number of seconds >= 0"), None));
            None
        }
        (s, _) => s,
    }
}

/// `known` sorted by edit distance to `name`, nearest first (ties in
/// their own order).
fn nearest<'a>(name: &str, known: &[&'a str]) -> Vec<(&'a str, usize)> {
    let mut scored: Vec<(&str, usize)> = known.iter().map(|k| (*k, levenshtein(name, k))).collect();
    scored.sort_by_key(|(_, d)| *d);
    scored
}

fn unknown_layer(doc: &Value, layer: &str, path: &str, at: f64) -> Diagnostic {
    let ids: Vec<&str> =
        doc.get("layers").and_then(Value::as_array).map(|a| a.iter().filter_map(|l| l.get("id").and_then(Value::as_str)).collect()).unwrap_or_default();
    let near = nearest(layer, &ids);
    let listed: Vec<&str> = near.iter().take(5).map(|(k, _)| *k).collect();
    let hint = near.first().filter(|(_, d)| *d <= 2).map(|(k, _)| format!("did you mean `{k}`?"));
    error(
        path,
        "unknown_layer",
        format!("at {at:.2} s: The style has no layer \"{layer}\" (after any earlier set_style); nearest: {listed:?}."),
        hint.or_else(|| Some("layer ids are the style document's `layers[].id`".to_string())),
    )
}

/// Applies a one-layer edit to `doc`; `false` when the layer is not there.
fn edit_layer(doc: &mut Value, layer: &str, action: &EventAction) -> bool {
    let Some(target) = doc
        .get_mut("layers")
        .and_then(Value::as_array_mut)
        .and_then(|layers| layers.iter_mut().find(|l| l.get("id").and_then(Value::as_str) == Some(layer)))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let mut member = |section: &str, name: &str, value: Value| {
        // `null` removes the property (back to the spec default), as
        // MapLibre's `setPaintProperty(layer, name, null)` does.
        if value.is_null() {
            if let Some(object) = target.get_mut(section).and_then(Value::as_object_mut) {
                object.remove(name);
            }
            return;
        }
        let slot = target.entry(section.to_string()).or_insert_with(|| Value::Object(Default::default()));
        if !slot.is_object() {
            *slot = Value::Object(Default::default());
        }
        if let Some(object) = slot.as_object_mut() {
            object.insert(name.to_string(), value);
        }
    };
    match action {
        EventAction::SetLayerVisibility { visible, .. } => member("layout", "visibility", Value::from(if *visible { "visible" } else { "none" })),
        EventAction::SetPaintProperty { name, value, .. } => member("paint", name, value.clone()),
        EventAction::SetLayoutProperty { name, value, .. } => member("layout", name, value.clone()),
        EventAction::SetFilter { filter, .. } if filter.is_null() => {
            // As MapLibre's `setFilter(layer, null)`: no filter.
            target.remove("filter");
        }
        EventAction::SetFilter { filter, .. } => {
            target.insert("filter".to_string(), filter.clone());
        }
        _ => return false,
    }
    true
}

/// One `overlay` event resolved into its [`EffectPlan`] (spec §3):
/// defaults filled, `head`/`color` parsed, and a `highlight`'s `ids` or
/// `where` and a `drop`'s reveal order resolved against the layer's
/// features. `loaded`: the caller passed a source for this layer, so an
/// id that matches no feature is worth a warning.
#[allow(clippy::too_many_arguments)]
fn effect_plan(
    spec: &OverlayEffectSpec,
    at: f64,
    until: Option<f64>,
    source: Option<&LayerSource>,
    loaded: bool,
    path: &str,
    diags: &mut Vec<Diagnostic>,
    warnings: &mut Vec<Diagnostic>,
) -> Option<EffectPlan> {
    let Some(source) = source else {
        diags.push(error(format!("{path}/layer"), "unknown_layer", crate::story::script::unknown_overlay_layer_message(&spec.layer), None));
        return None;
    };
    let color = |text: &str, at: &str, diags: &mut Vec<Diagnostic>| match paint_color(&Value::from(text)) {
        Some(c) => Some(effects::rgba_bytes(c)),
        None => {
            diags.push(error(at, "bad_color", format!("`{text}` is not a valid colour"), None));
            None
        }
    };
    let head = match &spec.head {
        Some(h) => Some((h.radius_px, color(&h.color, &format!("{path}/head/color"), diags)?)),
        None => None,
    };
    let highlight_color = match &spec.color {
        Some(c) => Some(color(c, &format!("{path}/color"), diags)?),
        None => None,
    };
    // The layer's features as (id, properties, geometry class), parsed
    // once for the selectors that need them.
    let needs_features = matches!(spec.effect, EffectKind::Highlight) || (spec.effect == EffectKind::Drop && spec.time_property.is_some());
    let features: Vec<(u64, Value, Option<crate::style::GeometryKind>)> = if needs_features {
        let doc: Value = serde_json::from_str(&source.geojson).unwrap_or(Value::Null);
        doc.get("features")
            .and_then(Value::as_array)
            .map(|fs| {
                fs.iter()
                    .filter_map(|f| {
                        let id = f.get("id")?.as_u64()?;
                        Some((id, f.get("properties").cloned().unwrap_or(Value::Null), f.get("geometry").and_then(geometry_kind)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut selected = Vec::new();
    if spec.effect == EffectKind::Highlight {
        if let Some(ids) = &spec.ids {
            // F2b: indices address features only in a source with no ids.
            let without_id = source.source_ids.iter().filter(|id| id.is_none()).count();
            let index_fallback = without_id == source.source_ids.len();
            if loaded && !index_fallback && without_id > 0 {
                warnings.push(Diagnostic {
                    severity: Severity::Warning,
                    path: format!("{path}/ids"),
                    code: "features_without_id",
                    message: format!(
                        "{without_id} feature(s) of `{}` have no GeoJSON `id` while others do, so `ids` cannot name them",
                        spec.layer
                    ),
                    hint: Some("select them with `where` (a filter on their properties), or give every feature an `id`".to_string()),
                });
            }
            for (k, entry) in ids.iter().enumerate() {
                let before = selected.len();
                selected.extend(
                    source
                        .feature_ids
                        .iter()
                        .zip(&source.source_ids)
                        .filter(|(index, own)| id_matches(entry, own.as_ref(), **index, index_fallback))
                        .map(|(index, _)| *index),
                );
                if selected.len() == before && loaded {
                    let known: Vec<String> = source
                        .feature_ids
                        .iter()
                        .zip(&source.source_ids)
                        .take(5)
                        .map(|(index, own)| own.as_ref().map_or_else(|| index.to_string(), Value::to_string))
                        .collect();
                    warnings.push(Diagnostic {
                        severity: Severity::Warning,
                        path: format!("{path}/ids/{k}"),
                        code: "unknown_feature_id",
                        message: format!("layer `{}` has no feature with id {entry}", spec.layer),
                        hint: Some(format!(
                            "an id matches the GeoJSON `id` exactly (\"7\" is not 7), or a feature's document index when no feature has an id; this layer's ids include {}",
                            known.join(", ")
                        )),
                    });
                }
            }
        }
        if let Some(filter) = spec.where_.as_ref().filter(|w| !w.is_null()) {
            match compile_layer_filter(filter, &format!("{path}/where")) {
                Ok(filter) => selected.extend(
                    features.iter().filter(|(_, props, kind)| filter.matches_geometry(&PropertyBag::of(Some(props)), 0.0, *kind)).map(|(id, _, _)| *id),
                ),
                Err(d) => {
                    diags.push(d);
                    return None;
                }
            }
        }
        selected.sort_unstable();
        selected.dedup();
        let no_base = selected
            .iter()
            .filter(|id| source.feature_ids.binary_search(id).ok().and_then(|i| source.base_colors.get(i).copied().flatten()).is_none())
            .count();
        if loaded && highlight_color.is_some() && no_base > 0 {
            warnings.push(Diagnostic {
                severity: Severity::Warning,
                path: format!("{path}/color"),
                code: "highlight_base_color",
                message: format!(
                    "{no_base} selected feature(s) of `{}` have no colour to ramp from, so they switch to the highlight colour at once",
                    spec.layer
                ),
                hint: Some("the layer's colour does not resolve for them; give it a fallback (the last value of a `match`/`case`)".to_string()),
            });
        }
    }
    let reveal = if spec.effect == EffectKind::Drop {
        match &spec.time_property {
            Some(key) => features
                .iter()
                .map(|(id, props, _)| (*id, props.get(key).and_then(Value::as_f64).filter(|s| s.is_finite()).unwrap_or(0.0).max(0.0)))
                .collect(),
            None => {
                let stagger = spec.stagger_s.filter(|s| s.is_finite() && *s >= 0.0).unwrap_or(effects::DEFAULT_STAGGER_S);
                source.feature_ids.iter().enumerate().map(|(i, id)| (*id, i as f64 * stagger)).collect()
            }
        }
    } else {
        Vec::new()
    };
    let radius = spec.max_radius_px.filter(|r| r.is_finite() && *r >= 0.0).unwrap_or(effects::DEFAULT_PULSE_RADIUS_PX);
    Some(EffectPlan {
        layer_id: spec.layer.clone(),
        kind: spec.effect,
        at,
        until: until.unwrap_or(f64::INFINITY),
        fade_s: spec.fade_s.filter(|f| f.is_finite() && *f >= 0.0).unwrap_or(DEFAULT_FADE_S),
        easing: spec.easing.clone().unwrap_or(Easing::Linear),
        head,
        selected,
        color: highlight_color,
        dim_opacity: spec.dim_opacity.filter(|d| d.is_finite()).map_or(effects::DEFAULT_DIM_OPACITY, |d| d.clamp(0.0, 1.0) as f32),
        reveal,
        period_s: spec.period_s.filter(|p| p.is_finite() && *p > 0.0).unwrap_or(effects::DEFAULT_PULSE_PERIOD_S),
        // A drop ripples only with `ripple: true` (`0` is no ripple).
        max_radius_px: match spec.effect {
            EffectKind::Drop if spec.ripple != Some(true) => 0.0,
            _ => radius,
        },
    })
}

/// How many times [`path_and_timeline`] rebuilds the timeline, draw
/// tracks and path before a `follow_draw` story is refused as unstable.
pub const FOLLOW_DRAW_ROUNDS: usize = 3;

/// The camera path and the timeline of `script`, with every
/// `follow_draw` segment on its draw head (spec §3, ruling F7 as amended
/// in review round 1).
///
/// `Timeline::new` needs the path (label times, the duration), and a
/// `follow_draw` path needs the draw's track, made from the timeline's
/// resolved plan. Segment TIMES mostly do not depend on a track, but a
/// `fly` with `speed`/`screen_speed` after a follow takes its duration
/// from where the follow ends, which is where the draw head is then. So:
/// 1. a first path with a placeholder `draw:<layer>` track per followed
///    layer, from the line's first point to its last (the follow ends at
///    the line's end whenever the draw finishes inside it, the usual
///    case); `(0, 0)` for a layer with no line loaded;
/// 2. the timeline over it;
/// 3. each followed layer's draw track ([`draw_track`] of its first `draw`);
/// 4. the path over those. If its segment times equal the path the
///    timeline was built on, the timeline is built once more over this
///    FINAL path, which is the only one event times are validated
///    against (F1b); otherwise repeat 2-4 over the new path,
///    at most [`FOLLOW_DRAW_ROUNDS`] times, then refuse with
///    `follow_draw_unstable` at the first `follow_draw`.
///
/// A script with no `follow_draw` is built exactly as before: one path,
/// one timeline. `Err` carries the stage (`"camera path"` or
/// `"timeline"`) and its diagnostics.
pub fn path_and_timeline(
    viewport: (f32, f32),
    script: &Script,
    tracks: Arc<TrackSet>,
    layers: &[LayerSource],
    base_style: Value,
    load_style: &dyn Fn(&str) -> Option<Value>,
) -> Result<(CameraPath, Timeline), (&'static str, Vec<Diagnostic>)> {
    let mut followed: Vec<&str> = Vec::new();
    let mut first_follow = None;
    for (i, segment) in script.camera.iter().enumerate() {
        if let Segment::Follow { track: None, follow_draw: Some(layer), .. } = segment {
            first_follow.get_or_insert(i);
            if !followed.contains(&layer.as_str()) {
                followed.push(layer);
            }
        }
    }
    let Some(first_follow) = first_follow else {
        let path = CameraPath::with_tracks(viewport, script, tracks).map_err(|d| ("camera path", d))?;
        let timeline = Timeline::new(script, &path, base_style, load_style, layers).map_err(|d| ("timeline", d))?;
        return Ok((path, timeline));
    };
    let source_of = |layer: &str| layers.iter().find(|s| s.layer_id == layer);
    // 1. The placeholder: the line's first point at 0 s, its last at 1 s.
    let mut first_set = tracks.share();
    for layer in &followed {
        let line = source_of(layer).map(|s| s.first_line.as_slice()).unwrap_or_default();
        let (a, b) = match (line.first(), line.last()) {
            (Some(a), Some(b)) => (*a, *b),
            _ => ((0.0, 0.0), (0.0, 0.0)),
        };
        let placeholder =
            Track::from_samples(&format!("{DRAW_TRACK_PREFIX}{layer}"), &[(0.0, a.0, a.1), (1.0, b.0, b.1)]).map_err(|d| ("camera path", vec![d]))?;
        first_set.tracks.push(placeholder);
    }
    let mut built_on = CameraPath::with_tracks(viewport, script, Arc::new(first_set)).map_err(|d| ("camera path", d))?;
    for _ in 0..FOLLOW_DRAW_ROUNDS {
        // 2. Not yet the final path: its duration is not the story's, so
        //    no event is refused for falling after it (F1b).
        let timeline = Timeline::build(script, &built_on, base_style.clone(), load_style, layers, false).map_err(|d| ("timeline", d))?;
        // 3. A layer with no draw track (no source, no line) is left out:
        //    the path then names it at its `follow_draw`.
        let mut set = tracks.share();
        for layer in &followed {
            let track = timeline
                .effect_plans()
                .into_iter()
                .filter(|p| p.kind == EffectKind::Draw && p.layer_id == *layer)
                .find_map(|p| draw_track(p, source_of(layer)?));
            set.tracks.extend(track);
        }
        // 4.
        let path = CameraPath::with_tracks(viewport, script, Arc::new(set)).map_err(|d| ("camera path", d))?;
        let same_times = built_on.segments().len() == path.segments().len()
            && built_on.segments().iter().zip(path.segments()).all(|(a, b)| a.start == b.start && a.duration == b.duration);
        if same_times {
            // The returned timeline is built, and every event time
            // validated, against the FINAL path only.
            let timeline = Timeline::new(script, &path, base_style, load_style, layers).map_err(|d| ("timeline", d))?;
            return Ok((path, timeline));
        }
        built_on = path;
    }
    Err((
        "camera path",
        vec![error(
            format!("/camera/{first_follow}/follow_draw"),
            "follow_draw_unstable",
            "a segment after this follow takes its duration from where the draw head is when the follow ends, and that never settled",
            Some("give the `fly` after this follow an explicit `duration` (instead of `speed`/`screen_speed`)".to_string()),
        )],
    ))
}

/// Every non-style action, resolved; `None` (with a diagnostic) when it
/// cannot be.
#[allow(clippy::too_many_arguments)]
fn resolve_op(
    action: &EventAction,
    until: Option<f64>,
    i: usize,
    path: &str,
    overlays: &[(String, String)],
    clock_start_unix: f64,
    diags: &mut Vec<Diagnostic>,
) -> Option<Op> {
    let overlay = |id: &str, diags: &mut Vec<Diagnostic>| {
        let index = overlays.iter().position(|(o, _)| o == id);
        if index.is_none() {
            let known: Vec<&str> = overlays.iter().map(|(o, _)| o.as_str()).collect();
            diags.push(error(format!("{path}/id"), "unknown_overlay", format!("no overlay named `{id}`; known: {known:?}"), None));
        }
        index
    };
    let param = |name: &str, diags: &mut Vec<Diagnostic>| {
        let p = Param::from_name(name);
        if p.is_none() {
            diags.push(error(
                format!("{path}/param"),
                "param_not_tweenable",
                format!("`{name}` is not a parameter; expected one of {:?}", super::script::TWEEN_PARAMS),
                None,
            ));
        }
        p
    };
    match action {
        EventAction::ShowOverlay { id } => overlay(id, diags).map(Op::ShowOverlay),
        EventAction::HideOverlay { id } => overlay(id, diags).map(Op::HideOverlay),
        EventAction::SetOverlayData { id, geojson } => overlay(id, diags).map(|k| Op::OverlayData(k, geojson.clone())),
        EventAction::SetBasemap { source } => Some(Op::Basemap(source.clone())),
        EventAction::SetGlobe { on } => Some(Op::Globe(*on)),
        EventAction::SetParam { param: name, value } => {
            let p = param(name, diags)?;
            let resolved = match (p, value) {
                (_, Value::Bool(b)) if p.is_bool() => Some(ParamValue::Bool(*b)),
                (Param::SunTime, Value::String(s)) => parse_utc(s).map(ParamValue::Number),
                (Param::SunTime, v) => v.as_f64().map(|s| ParamValue::Number(clock_start_unix + s)),
                (_, v) if !p.is_bool() => v.as_f64().map(ParamValue::Number),
                _ => None,
            };
            let finite = match resolved {
                Some(ParamValue::Number(n)) => n.is_finite() && (p == Param::SunTime || f64::from(n as f32).is_finite()),
                Some(ParamValue::Bool(_)) => true,
                None => false,
            };
            if !finite {
                let expected = if p.is_bool() {
                    "true or false"
                } else if p == Param::SunTime {
                    "seconds after clock.start_utc, or a UTC time string"
                } else {
                    "a finite number"
                };
                diags.push(error(format!("{path}/value"), "bad_param_value", format!("`{name}` takes {expected}, got {value}"), None));
                return None;
            }
            if let Some(ParamValue::Number(n)) = resolved {
                if !p.in_range(n) {
                    diags.push(out_of_range(&format!("{path}/value"), name, n, p));
                    return None;
                }
            }
            resolved.map(|v| Op::Set(p, v))
        }
        EventAction::Tween { param: name, from, to, easing } => {
            let p = param(name, diags)?;
            if p.is_bool() {
                diags.push(error(format!("{path}/param"), "param_not_tweenable", format!("`{name}` is on or off; use set_param"), None));
                return None;
            }
            let Some(until) = until else {
                diags.push(error(format!("/events/{i}/until"), "tween_needs_until", "a tween needs `until`, when it reaches `to`", None));
                return None;
            };
            let (from, to) = if p == Param::SunTime { (clock_start_unix + from, clock_start_unix + to) } else { (*from, *to) };
            let fits = |v: f64| v.is_finite() && (p == Param::SunTime || f64::from(v as f32).is_finite());
            if !(fits(from) && fits(to)) {
                diags.push(error(path.to_string(), "bad_param_value", format!("tween `{name}` from {from} to {to} is not finite"), None));
                return None;
            }
            let mut in_range = true;
            for (key, v) in [("from", from), ("to", to)] {
                if !p.in_range(v) {
                    diags.push(out_of_range(&format!("{path}/{key}"), name, v, p));
                    in_range = false;
                }
            }
            if !in_range {
                return None;
            }
            Some(Op::Tween { param: p, from, to, until, easing: easing.clone().unwrap_or(Easing::Ease) })
        }
        EventAction::Caption(spec) => caption(spec, until, path, diags).map(Op::Caption),
        EventAction::Settle { timeout_s } => {
            let timeout = timeout_s.unwrap_or(DEFAULT_SETTLE_TIMEOUT_S);
            if !(timeout.is_finite() && timeout > 0.0 && timeout <= MAX_SETTLE_TIMEOUT_S) {
                diags.push(error(
                    format!("{path}/timeout_s"),
                    "bad_settle_timeout",
                    format!("timeout_s must be a positive number of seconds, at most {MAX_SETTLE_TIMEOUT_S}, got {timeout}"),
                    None,
                ));
                return None;
            }
            Some(Op::Settle(timeout))
        }
        EventAction::Panel(spec) => Some(element_op(i, until, spec.fade_s, ElementKind::Panel(spec.clone()))),
        EventAction::Image(spec) => Some(element_op(i, until, spec.fade_s, ElementKind::Image(spec.clone()))),
        EventAction::Callout(spec) => Some(element_op(i, until, spec.fade_s, ElementKind::Callout(spec.clone()))),
        EventAction::SetStyle { .. }
        | EventAction::SetLayerVisibility { .. }
        | EventAction::SetFilter { .. }
        | EventAction::SetPaintProperty { .. }
        | EventAction::SetLayoutProperty { .. }
        | EventAction::TweenPaintProperty { .. }
        // Resolved in the walk (`effect_plan`), which knows `at`.
        | EventAction::Overlay(_)
        // The design-system card has no `Op` yet: `cards.rs` (a later
        // task) decides which card is on screen at `t` directly from
        // `Script::events`/`Script::chapters`, not through the timeline.
        | EventAction::Card(_) => None,
    }
}

/// How many style generations a second a `tween_paint_property` makes.
/// Each is a recompile and a re-bake in the player, so it is a rate, not
/// one per frame: 10 steps a second moves a colour in steps no one can see
/// at video frame rates, and a rate (not the fps) keeps the generations
/// the same whatever the output.
pub const PAINT_TWEEN_STEPS_PER_S: f64 = 10.0;
/// At most this many steps per tween, however long.
const PAINT_TWEEN_MAX_STEPS: usize = 600;

/// A tween's value kinds: two numbers, or two colours.
enum PaintEnds {
    Number(f64, f64),
    Color(crate::style::Rgba, crate::style::Rgba),
}

fn paint_color(v: &Value) -> Option<crate::style::Rgba> {
    let text = v.as_str()?;
    crate::style_spec::color::css_named_color(text).or_else(|| crate::style_spec::parse_css_color(text).ok())
}

/// `rgba(r, g, b, a)`: a colour the style compiler parses back exactly
/// (channels rounded to bytes, alpha to thousandths).
fn css_rgba(c: crate::style::Rgba) -> String {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("rgba({}, {}, {}, {})", byte(c.r), byte(c.g), byte(c.b), (f64::from(c.a.clamp(0.0, 1.0)) * 1000.0).round() / 1000.0)
}

impl PaintEnds {
    /// The value at eased fraction `k`.
    fn at(&self, k: f64) -> Value {
        let lerp = |a: f64, b: f64| a + (b - a) * k;
        match self {
            // Rounded to millionths: an eased step reads as the value a
            // person would write, and the generation's document is stable.
            PaintEnds::Number(a, b) => serde_json::json!((lerp(*a, *b) * 1e6).round() / 1e6),
            PaintEnds::Color(a, b) => {
                let ch = |x: f32, y: f32| lerp(f64::from(x), f64::from(y)) as f32;
                Value::from(css_rgba(crate::style::Rgba { r: ch(a.r, b.r), g: ch(a.g, b.g), b: ch(a.b, b.b), a: ch(a.a, b.a) }))
            }
        }
    }
}

/// A paint tween's ends. `from` defaults to the layer's own `name` in
/// `current`, the document in force at the tween's `at` (so a tween picks
/// up from what is on screen, whatever style the story runs on). Diagnoses
/// an unknown layer, a default that is missing or an expression, and ends
/// that are not two numbers or two colours.
#[allow(clippy::too_many_arguments)]
fn paint_ends(
    current: &Value,
    layer: &str,
    name: &str,
    from: Option<&Value>,
    to: &Value,
    path: &str,
    at: f64,
    diags: &mut Vec<Diagnostic>,
) -> Option<PaintEnds> {
    let target = current
        .get("layers")
        .and_then(Value::as_array)
        .and_then(|layers| layers.iter().find(|l| l.get("id").and_then(Value::as_str) == Some(layer)));
    let Some(target) = target else {
        diags.push(unknown_layer(current, layer, &format!("{path}/layer"), at));
        return None;
    };
    let from = match from {
        Some(v) => v.clone(),
        None => match target.get("paint").and_then(|p| p.get(name)) {
            Some(v) if v.is_number() || v.is_string() => v.clone(),
            other => {
                let what = match other {
                    Some(v) => format!("is an expression ({v})"),
                    None => "is not set (the spec default)".to_string(),
                };
                diags.push(error(
                    format!("{path}/from"),
                    "tween_needs_from",
                    format!("`{layer}`'s `{name}` {what}, so the tween has no value to start from"),
                    Some("give `from` explicitly".to_string()),
                ));
                return None;
            }
        },
    };
    let ends = match (from.as_f64(), to.as_f64()) {
        (Some(a), Some(b)) if a.is_finite() && b.is_finite() => Some(PaintEnds::Number(a, b)),
        (None, None) => match (paint_color(&from), paint_color(to)) {
            (Some(a), Some(b)) => Some(PaintEnds::Color(a, b)),
            _ => None,
        },
        _ => None,
    };
    if ends.is_none() {
        diags.push(error(
            path.to_string(),
            "bad_tween_value",
            format!("`from` ({from}) and `to` ({to}) must be two numbers or two colours"),
            Some("a colour is #rrggbb, rgb(), rgba(), hsl(), hsla() or a CSS name".to_string()),
        ));
    }
    ends
}

/// The `(time, eased fraction)` steps of a paint tween from `at`: one every
/// `1 / PAINT_TWEEN_STEPS_PER_S` s up to `last` (the tween's `until`, or
/// the story's end if that is sooner); fraction exactly 1 at `until` when
/// that is within the story.
fn paint_tween_times(easing: &Easing, at: f64, last: f64, until: f64) -> Vec<(f64, f64)> {
    let span = until - at;
    let n = ((span * PAINT_TWEEN_STEPS_PER_S).ceil() as usize).clamp(1, PAINT_TWEEN_MAX_STEPS);
    let mut steps = Vec::with_capacity(n + 1);
    for step in 0..=n {
        // From the index, never summed: the same times on every run.
        let t = if step == n { until } else { at + span * step as f64 / n as f64 };
        if t > last {
            break;
        }
        let x = step as f64 / n as f64;
        let k = if step == n {
            1.0
        } else {
            let k = easing.apply(x);
            if k.is_finite() { k } else { x }
        };
        steps.push((t, k));
    }
    steps
}

/// A panel/image/callout event's `Op`: `until` with no `until` given is
/// `+INFINITY` (the caption rule), and `fade_s` with none given -- or an
/// unusable one, rather than let a bad script produce a NaN alpha forever
/// -- is [`DEFAULT_FADE_S`].
fn element_op(i: usize, until: Option<f64>, fade_s: Option<f64>, kind: ElementKind) -> Op {
    let fade_s = fade_s.filter(|f| f.is_finite() && *f >= 0.0).unwrap_or(DEFAULT_FADE_S);
    Op::Element(ElementPlan { id: format!("/events/{i}"), kind, until: until.unwrap_or(f64::INFINITY), fade_s })
}

fn caption(spec: &CaptionSpec, until: Option<f64>, path: &str, diags: &mut Vec<Diagnostic>) -> Option<CaptionPlan> {
    let before = diags.len();
    let anchor = Anchor::from_keyword(&spec.anchor);
    if anchor.is_none() {
        diags.push(error(format!("{path}/anchor"), "bad_anchor", format!("unknown anchor `{}`", spec.anchor), None));
    }
    let color = match &spec.color {
        None => Some(DEFAULT_CAPTION_COLOR),
        Some(text) => match crate::style_spec::color::css_named_color(text).map(Ok).unwrap_or_else(|| crate::style_spec::parse_css_color(text)) {
            Ok(c) => Some([c.r, c.g, c.b, c.a].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)),
            Err(e) => {
                diags.push(error(format!("{path}/color"), "bad_color", format!("`{text}` is not a colour: {e}"), Some(format!("use #rrggbb, rgb(), rgba(), hsl(), hsla() or one of {:?}", crate::style_spec::color::css_color_names().collect::<Vec<_>>()))));
                None
            }
        },
    };
    let size_px = spec.size_px.unwrap_or(DEFAULT_CAPTION_SIZE_PX);
    if !(size_px.is_finite() && size_px > 0.0) {
        diags.push(error(format!("{path}/size_px"), "bad_caption_size", format!("size_px must be a positive number, got {size_px}"), None));
    }
    let offset_px = spec.offset_px.unwrap_or((0.0, 0.0));
    if !(offset_px.0.is_finite() && offset_px.1.is_finite()) {
        diags.push(error(format!("{path}/offset_px"), "bad_caption_offset", format!("offset_px must be finite, got {offset_px:?}"), None));
    }
    let fade_s = spec.fade_s.unwrap_or(DEFAULT_FADE_S);
    if !(fade_s.is_finite() && fade_s >= 0.0) {
        diags.push(error(format!("{path}/fade_s"), "bad_fade", format!("fade_s must be a number of seconds >= 0, got {fade_s}"), None));
    }
    if diags.len() != before {
        return None;
    }
    Some(CaptionPlan {
        until: until.unwrap_or(f64::INFINITY),
        fade_s,
        caption: ActiveCaption {
            text: spec.text.clone(),
            anchor: anchor?,
            offset_px,
            size_px,
            color: color?,
            plate: spec.plate.unwrap_or(true),
            alpha: 1.0,
        },
    })
}

/// Linear over `fade_s` at each end: `since` seconds after `at`, `left`
/// seconds before `until`.
fn caption_alpha(since: f64, left: f64, fade_s: f64) -> f32 {
    if fade_s <= 0.0 {
        return 1.0;
    }
    (since / fade_s).min(left / fade_s).clamp(0.0, 1.0) as f32
}

fn apply(params: &mut Params, param: Param, value: ParamValue) {
    let value = match value {
        ParamValue::Number(v) => ParamValue::Number(param.clamp(v)),
        other => other,
    };
    match (param, value) {
        (Param::RasterOpacity, ParamValue::Number(v)) => params.raster_opacity = v as f32,
        (Param::EpochMix, ParamValue::Number(v)) => params.epoch_mix = v as f32,
        (Param::DotsPpd, ParamValue::Number(v)) => params.dots_ppd = Some(v as f32),
        (Param::DotsRadius, ParamValue::Number(v)) => params.dots_radius_px = Some(v as f32),
        (Param::SunTime, ParamValue::Number(v)) => params.sun_unix = v,
        (Param::Atmosphere, ParamValue::Bool(b)) => params.atmosphere = b,
        (Param::Stars, ParamValue::Bool(b)) => params.stars = b,
        (Param::Terminator, ParamValue::Bool(b)) => params.terminator = b,
        (Param::Labels, ParamValue::Bool(b)) => params.labels = b,
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::story::style_cache::StyleCache;
    use crate::story::test_util::test_script;
    use crate::story::tracks::TrackSet;

    fn style() -> serde_json::Value {
        serde_json::json!({"version":8,"sources":{},"layers":[{"id":"bg","type":"background","paint":{"background-color":"#fff"}},{"id":"water","type":"fill","source":"s","source-layer":"water","paint":{"fill-color":"#00f"}}]})
    }

    /// A 10 s script: a jump, then a labelled 4 s hold ("a", 0..4) and a
    /// 6 s settle hold ("b", 4..10), with one overlay `routes`.
    fn script_with(events: serde_json::Value) -> Script {
        let mut script = test_script(serde_json::json!([
            {"type":"jump","to":{"lon":0,"lat":0,"zoom":2}},
            {"type":"hold","label":"a","duration":4},
            {"type":"hold","label":"b","duration":6,"settle":true}
        ]));
        script.scene.overlays = serde_json::from_value(serde_json::json!([{"id":"routes","geojson":"routes.geojson"}])).unwrap();
        script.events = serde_json::from_value(events).unwrap();
        script
    }

    fn try_timeline_with_loader(events: serde_json::Value, loader: &dyn Fn(&str) -> Option<serde_json::Value>) -> Result<Timeline, Vec<Diagnostic>> {
        let script = script_with(events);
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        assert_eq!(path.duration(), 10.0);
        Timeline::new(&script, &path, style(), loader, &[])
    }

    fn try_timeline(events: serde_json::Value) -> Result<Timeline, Vec<Diagnostic>> {
        try_timeline_with_loader(events, &|_| None)
    }

    fn timeline(events: serde_json::Value) -> Timeline {
        try_timeline(events).unwrap()
    }

    fn water_at(tl: &Timeline, t: f64) -> serde_json::Value {
        let g = tl.state_at(t, 30).style_generation;
        tl.style_document(g).unwrap()["layers"][1]["paint"]["fill-color"].clone()
    }

    /// A 10 s hold, no overlays, no scene style layers: just enough for a
    /// panel/image/callout test to build a `Timeline` over `events`.
    fn timeline_with_events(events: serde_json::Value) -> Timeline {
        let mut script = test_script(serde_json::json!([{"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 2}}, {"type": "hold", "duration": 10.0}]));
        script.events = serde_json::from_value(events).unwrap();
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        Timeline::new(&script, &path, serde_json::json!({"version": 8, "layers": []}), &|_| None, &[]).unwrap()
    }

    /// Task 16: `output.poster_at` must be a second of the story; at its
    /// end is fine, past it is `poster_outside_story`, and a negative one
    /// is refused at parse (script.rs) with the same code.
    #[test]
    fn a_poster_past_the_end_is_poster_outside_story() {
        let build = |at: f64| {
            let mut script = test_script(serde_json::json!([{"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 2}}, {"type": "hold", "duration": 10.0}]));
            script.output.poster_at = Some(at);
            let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
            Timeline::new(&script, &path, serde_json::json!({"version": 8, "layers": []}), &|_| None, &[]).map(|_| ())
        };
        assert!(build(10.0).is_ok());
        assert!(build(0.0).is_ok());
        let diags = build(10.5).unwrap_err();
        assert!(diags.iter().any(|d| d.code == "poster_outside_story" && d.path == "/output/poster_at"), "{diags:?}");
        let raw = serde_json::json!({"story": 1, "output": {"width": 800, "height": 600, "poster_at": -1.0},
            "scene": {"style": "x.json", "basemap": {"source": "openfreemap"}},
            "camera": [{"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 2}}]});
        let diags = crate::story::parse(raw.to_string().as_bytes()).unwrap_err();
        assert!(diags.iter().any(|d| d.code == "poster_outside_story" && d.path == "/output/poster_at"), "{diags:?}");
    }

    /// Task 13 review F3: `globe_at` is `state_at(..).globe` without the
    /// rest of the state, at every time (and before, between and at the
    /// switches, at NaN and at +inf).
    #[test]
    fn globe_at_agrees_with_the_state() {
        let tl = timeline_with_events(serde_json::json!([
            {"at": 2.0, "set_globe": {"on": true}},
            {"at": 5.0, "set_globe": {"on": false}},
            {"at": 5.0, "set_globe": {"on": true}},
            {"at": 8.0, "set_globe": {"on": false}}
        ]));
        let mut ts: Vec<f64> = (0..=110).map(|k| f64::from(k) / 10.0).collect();
        ts.extend([f64::NAN, f64::INFINITY, -1.0]);
        for t in ts {
            assert_eq!(tl.globe_at(t), tl.state_at(t, 30).globe, "t = {t}");
        }
        assert_eq!((tl.globe_at(1.9), tl.globe_at(5.0), tl.globe_at(9.0)), (None, Some(true), Some(false)));
    }

    /// Pinned by the brief (Task 3, Step 1). Also covers the controller's
    /// exit-ramp ruling (Task 3 review round 1): `alpha` fades out over
    /// the last `fade_s` before `until` (`2.75` s, `0.5` s before `3.0`),
    /// while `enter` keeps climbing independent of `until` -- it is fully
    /// settled by `2.75` s (`since = 1.75 s >> fade_s`), because the
    /// slide applies only on the way in, never back out.
    #[test]
    fn a_panel_fades_in_and_out_and_seeking_equals_playing() {
        let tl = timeline_with_events(serde_json::json!([
            {"at": 1.0, "until": 3.0, "panel": {"placement": "lower-third", "headline": "Hi", "fade_s": 0.5}}]));
        assert!(tl.state_at(0.9, 30).elements.is_empty());
        let mid = tl.state_at(1.25, 30);
        assert_eq!(mid.elements.len(), 1);
        assert!((mid.elements[0].alpha - 0.5).abs() < 1e-6 && (mid.elements[0].enter - 0.5).abs() < 1e-6);
        assert!((tl.state_at(2.0, 30).elements[0].alpha - 1.0).abs() < 1e-6);
        let near_exit = tl.state_at(2.75, 30);
        assert!((near_exit.elements[0].alpha - 0.5).abs() < 1e-6, "the exit ramp: alpha has fallen halfway");
        assert!((near_exit.elements[0].enter - 1.0).abs() < 1e-6, "enter stays settled: no slide-out");
        assert!(tl.state_at(3.0, 30).elements.is_empty());
        // Seeking equals playing: every frame evaluated backwards equals forwards.
        let forward: Vec<_> = (0..120).map(|n| tl.state_at(n as f64 / 30.0, 30)).collect();
        let backward: Vec<_> = (0..120).rev().map(|n| tl.state_at(n as f64 / 30.0, 30)).collect();
        assert!(forward.iter().eq(backward.iter().rev()));
    }

    /// The exit ramp in isolation, far from entry (a long element, so the
    /// in and out ramps never overlap): `alpha` falls from `1` to `0`
    /// linearly over the last `fade_s` before `until`, while `enter` --
    /// already settled long before -- stays at `1`, confirming the slide
    /// applies only on the way in.
    #[test]
    fn the_exit_ramp_is_a_linear_fade_with_no_slide_out() {
        let tl = timeline_with_events(serde_json::json!([
            {"at": 0.0, "until": 10.0, "panel": {"placement": "corner", "headline": "Hi", "fade_s": 1.0}}]));
        assert!((tl.state_at(8.5, 30).elements[0].alpha - 1.0).abs() < 1e-6, "still well before the exit ramp");
        let three_quarters = tl.state_at(9.25, 30);
        assert!((three_quarters.elements[0].alpha - 0.75).abs() < 1e-6, "0.75 s left of a 1 s ramp");
        assert!((three_quarters.elements[0].enter - 1.0).abs() < 1e-6);
        let near_end = tl.state_at(9.99, 30);
        assert!(near_end.elements[0].alpha < 0.02, "almost gone: {}", near_end.elements[0].alpha);
        assert!((near_end.elements[0].enter - 1.0).abs() < 1e-6, "enter never eases back down");
        assert!(tl.state_at(10.0, 30).elements.is_empty());
        // Seek == play over the whole span, including the exit ramp.
        let forward: Vec<_> = (0..300).map(|n| tl.state_at(n as f64 / 30.0, 30)).collect();
        let backward: Vec<_> = (0..300).rev().map(|n| tl.state_at(n as f64 / 30.0, 30)).collect();
        assert!(forward.iter().eq(backward.iter().rev()));
    }

    /// Controller ruling (Task 3 review round 1): an element shorter than
    /// `2 * fade_s` never reaches full opacity -- its in and out ramps
    /// meet at their crossing point instead of each reaching `1`.
    #[test]
    fn a_short_element_never_reaches_full_opacity_the_ramps_meet_in_the_middle() {
        // duration 1.0 s < 2 * fade_s (2.0 s): the in and out ramps cross
        // at the midpoint, 0.5 s in, each only halfway to 1.
        let tl = timeline_with_events(serde_json::json!([
            {"at": 0.0, "until": 1.0, "panel": {"placement": "corner", "headline": "Hi", "fade_s": 1.0}}]));
        let peak = tl.state_at(0.5, 30).elements[0].alpha;
        assert!((peak - 0.5).abs() < 1e-6, "the crossing point, not 1.0: {peak}");
        for n in 0..30 {
            let t = n as f64 / 30.0;
            let alpha = tl.state_at(t, 30).elements[0].alpha;
            assert!(alpha <= peak + 1e-6, "alpha at {t} ({alpha}) exceeds the crossing point {peak}: never reaches full opacity");
        }
        // `enter` is unaffected by how short the element is: it still
        // approaches 1 simply as a function of `since` (until it vanishes
        // at `until`, where there is no frame to query).
        assert!((tl.state_at(0.99, 30).elements[0].enter - 0.99).abs() < 1e-6);
        // Seek == play for the whole (short) span.
        let forward: Vec<_> = (0..30).map(|n| tl.state_at(n as f64 / 30.0, 30)).collect();
        let backward: Vec<_> = (0..30).rev().map(|n| tl.state_at(n as f64 / 30.0, 30)).collect();
        assert!(forward.iter().eq(backward.iter().rev()));
    }

    /// The element's id is stable and points at its own event, and an
    /// image/callout behave the same way a panel does.
    #[test]
    fn an_image_and_a_callout_are_elements_with_a_stable_events_id() {
        let tl = timeline_with_events(serde_json::json!([
            {"at": 0.0, "until": 2.0, "image": {"src": "a.png", "alt": "A", "credit": "Me"}},
            {"at": 0.0, "until": 2.0, "callout": {"at_coord": [1.0, 2.0], "text": "Hi"}}
        ]));
        let els = tl.state_at(1.0, 30).elements;
        assert_eq!(els.len(), 2);
        assert_eq!(els[0].id, "/events/0");
        assert_eq!(els[1].id, "/events/1");
        assert!(matches!(els[0].kind, ElementKind::Image(_)));
        assert!(matches!(els[1].kind, ElementKind::Callout(_)));
    }

    /// No `until`: the element (like a caption) runs to the story's end.
    #[test]
    fn an_element_with_no_until_runs_to_the_end() {
        let tl = timeline_with_events(serde_json::json!([{"at": 0.0, "panel": {"placement": "corner", "headline": "Hi"}}]));
        assert_eq!(tl.state_at(9.999, 30).elements.len(), 1);
    }

    #[test]
    fn element_plans_is_document_order_not_time_order() {
        let tl = timeline_with_events(serde_json::json!([
            {"at": 5.0, "panel": {"placement": "corner", "headline": "Second by time, first in the document"}},
            {"at": 1.0, "panel": {"placement": "corner", "headline": "First by time, second in the document"}}
        ]));
        let plans = tl.element_plans();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].0, 5.0, "document order: event 0 first, even though it fires later");
        assert_eq!(plans[1].0, 1.0);
    }

    #[test]
    fn caption_inputs_covers_events_then_script_captions_with_no_until_as_the_duration() {
        let mut script = test_script(serde_json::json!([{"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 2}}, {"type": "hold", "duration": 10.0}]));
        script.events = serde_json::from_value(serde_json::json!([
            {"at": 1.0, "until": 3.0, "caption": {"text": "First", "anchor": "bottom-left"}},
            {"at": 6.0, "caption": {"text": "Runs to the end", "anchor": "bottom-left"}}
        ]))
        .unwrap();
        script.captions = serde_json::from_value(serde_json::json!([{"from": 4.0, "to": 5.0, "text": "From the track"}])).unwrap();
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        let tl = Timeline::new(&script, &path, style(), &|_| None, &[]).unwrap();
        assert_eq!(
            tl.caption_inputs(),
            vec![
                (1.0, 3.0, "First".to_string()),
                (6.0, 10.0, "Runs to the end".to_string()),
                (4.0, 5.0, "From the track".to_string()),
            ]
        );
    }

    /// Controller ruling (Task 18): the layers-over-time tween must move
    /// something the frame shows. A paint tween steps the water colour as
    /// style generations, ten a second, eased, and holds `to` after.
    #[test]
    fn a_paint_tween_steps_a_visible_colour_through_style_generations() {
        let tl = timeline(serde_json::json!([
            {"at":2,"until":4,"tween_paint_property":{"layer":"water","name":"fill-color","from":"#0000ff","to":"#ff0000","easing":"linear"}}
        ]));
        assert_eq!(water_at(&tl, 1.9), "#00f", "before the tween: the scene's colour");
        assert_eq!(water_at(&tl, 2.0), "rgba(0, 0, 255, 1)");
        assert_eq!(water_at(&tl, 3.0), "rgba(128, 0, 128, 1)", "halfway, linear");
        assert_eq!(water_at(&tl, 3.95), "rgba(242, 0, 13, 1)", "the 3.9 s step");
        assert_eq!(water_at(&tl, 4.0), "rgba(255, 0, 0, 1)");
        assert_eq!(water_at(&tl, 9.0), "rgba(255, 0, 0, 1)", "`to` holds");
        assert_eq!(tl.generations(), 1 + 21, "the scene's, then 2 s x 10 steps + the end");
        assert_eq!(tl.generation_source(5), Some(0), "every step is the tween's");
        // Numbers too, and an edit inside the tween composes with it.
        let tl = timeline(serde_json::json!([
            {"at":0,"until":1,"tween_paint_property":{"layer":"water","name":"fill-opacity","from":0,"to":1,"easing":"linear"}},
            {"at":0.55,"set_paint_property":{"layer":"water","name":"fill-color","value":"#0f0"}}
        ]));
        let doc = |t: f64| tl.style_document(tl.state_at(t, 30).style_generation).unwrap()["layers"][1]["paint"].clone();
        assert_eq!(doc(0.5)["fill-opacity"], 0.5);
        assert_eq!(doc(0.5)["fill-color"], "#00f");
        assert_eq!(doc(0.6)["fill-color"], "#0f0", "the edit");
        assert_eq!(doc(0.6)["fill-opacity"], 0.6, "the tween goes on over it");
        assert_eq!(doc(1.0)["fill-opacity"], 1.0);
        // Every step compiles.
        let mut cache = StyleCache::new();
        for g in 0..tl.generations() {
            cache.get(&tl, g).unwrap();
        }
    }

    /// F2: without `from`, a paint tween starts from the layer's value in
    /// the style in force at `at` -- including an earlier edit -- so it
    /// never jumps on its first step (the keyless twin's water did).
    #[test]
    fn a_paint_tween_without_from_starts_from_what_is_on_screen() {
        let tl = timeline(serde_json::json!([
            {"at":2,"until":4,"tween_paint_property":{"layer":"water","name":"fill-color","to":"#ff0000","easing":"linear"}}
        ]));
        assert_eq!(water_at(&tl, 2.0), "rgba(0, 0, 255, 1)", "the scene's #00f, no jump");
        assert_eq!(water_at(&tl, 3.0), "rgba(128, 0, 128, 1)");
        assert_eq!(water_at(&tl, 4.0), "rgba(255, 0, 0, 1)");
        let tl = timeline(serde_json::json!([
            {"at":2,"until":4,"tween_paint_property":{"layer":"water","name":"fill-color","to":"#ff0000","easing":"linear"}},
            {"at":1,"set_paint_property":{"layer":"water","name":"fill-color","value":"#00ff00"}}
        ]));
        assert_eq!(water_at(&tl, 2.0), "rgba(0, 255, 0, 1)", "from the earlier edit");
        // An unset or expression-valued property needs an explicit `from`.
        let errs = try_timeline(serde_json::json!([
            {"at":2,"until":4,"tween_paint_property":{"layer":"water","name":"fill-opacity","to":0.5}},
            {"at":2,"until":4,"tween_paint_property":{"layer":"bg","name":"background-opacity","from":1,"to":0.5}}
        ]))
        .unwrap_err();
        let got: Vec<(&str, &str)> = errs.iter().map(|d| (d.code, d.path.as_str())).collect();
        assert_eq!(got, vec![("tween_needs_from", "/events/0/tween_paint_property/from")], "the explicit one is fine");
    }

    #[test]
    fn a_paint_tween_needs_until_matching_ends_and_a_known_layer() {
        let errs = try_timeline(serde_json::json!([
            {"at":1,"tween_paint_property":{"layer":"water","name":"fill-color","from":"#000","to":"#fff"}},
            {"at":1,"until":2,"tween_paint_property":{"layer":"water","name":"fill-color","from":"#000","to":3}},
            {"at":1,"until":2,"tween_paint_property":{"layer":"watr","name":"fill-color","from":"#000","to":"#fff"}}
        ]))
        .unwrap_err();
        let got: Vec<(&str, &str)> = errs.iter().map(|d| (d.code, d.path.as_str())).collect();
        assert!(got.contains(&("tween_needs_until", "/events/0/until")), "{got:?}");
        assert!(got.contains(&("bad_tween_value", "/events/1/tween_paint_property")), "{got:?}");
        assert_eq!(got.iter().filter(|(_, p)| *p == "/events/2/tween_paint_property/layer").count(), 1, "once, not per step: {got:?}");
    }

    #[test]
    fn a_style_edit_bumps_the_generation_at_its_time_and_not_before() {
        let tl = timeline(serde_json::json!([{"at":1.0,"set_paint_property":{"layer":"water","name":"fill-color","value":"#f00"}}]));
        assert_eq!(tl.state_at(0.999, 30).style_generation, 0);
        assert_eq!(tl.state_at(1.0, 30).style_generation, 1);
        assert_eq!(tl.style_document(1).unwrap()["layers"][1]["paint"]["fill-color"], "#f00");
        assert_eq!(tl.style_document(0).unwrap()["layers"][1]["paint"]["fill-color"], "#00f", "generation 0 untouched");
    }

    #[test]
    fn an_edit_to_a_missing_layer_is_a_diagnostic_not_a_panic() {
        let err = try_timeline(serde_json::json!([{"at":0,"set_filter":{"layer":"road","filter":["==","class","x"]}}])).unwrap_err();
        assert!(err.iter().any(|d| d.code == "unknown_layer" && d.path == "/events/0/set_filter/layer"), "{err:?}");
    }

    #[test]
    fn a_layer_removed_by_set_style_is_unknown_to_a_later_edit() {
        std::thread_local! { static NEW: serde_json::Value = serde_json::json!({"version":8,"sources":{},"layers":[{"id":"bg","type":"background"}]}); }
        let err = try_timeline_with_loader(
            serde_json::json!([{"at":0.5,"set_style":{"style":"new.json"}},{"at":1.0,"set_layer_visibility":{"layer":"water","visible":false}}]),
            &|_| Some(NEW.with(|v| v.clone())),
        )
        .unwrap_err();
        assert!(err.iter().any(|d| d.code == "unknown_layer"));
    }

    #[test]
    fn a_tween_is_eased_between_at_and_until_and_holds_after() {
        let tl = timeline(serde_json::json!([{"at":2,"until":4,"tween":{"param":"raster_opacity","from":0,"to":1,"easing":"linear"}}]));
        assert_eq!(tl.state_at(1.0, 30).params.raster_opacity, 1.0, "before the tween: the default");
        assert!((tl.state_at(3.0, 30).params.raster_opacity - 0.5).abs() < 1e-6);
        assert_eq!(tl.state_at(5.0, 30).params.raster_opacity, 1.0);
    }

    #[test]
    fn a_caption_fades_in_and_out() {
        let tl = timeline(serde_json::json!([{"at":1,"until":3,"caption":{"text":"Hi","anchor":"bottom-left","fade_s":0.5}}]));
        assert!(tl.state_at(0.9, 30).captions.is_empty());
        assert!((tl.state_at(1.25, 30).captions[0].alpha - 0.5).abs() < 1e-6);
        assert_eq!(tl.state_at(2.0, 30).captions[0].alpha, 1.0);
        assert!((tl.state_at(2.75, 30).captions[0].alpha - 0.5).abs() < 1e-6);
        assert!(tl.state_at(3.0, 30).captions.is_empty());
    }

    #[test]
    fn events_at_the_same_time_apply_in_document_order() {
        let tl = timeline(serde_json::json!([{"at":1,"set_param":{"param":"labels.on","value":false}},{"at":1,"set_param":{"param":"labels.on","value":true}}]));
        assert!(tl.state_at(1.0, 30).params.labels);
    }

    #[test]
    fn the_sun_advances_with_the_story_clock() {
        let tl = timeline(serde_json::json!([]));
        assert_eq!(tl.state_at(10.0, 30).params.sun_unix - tl.state_at(0.0, 30).params.sun_unix, 10.0);
    }

    #[test]
    fn the_style_cache_compiles_each_generation_once() {
        let tl = timeline(serde_json::json!([{"at":1,"set_layer_visibility":{"layer":"water","visible":false}}]));
        let mut cache = StyleCache::new();
        for _ in 0..10 {
            cache.get(&tl, 0).unwrap();
            cache.get(&tl, 1).unwrap();
        }
        assert_eq!(cache.compiles(), 2);
    }

    // ---- beyond the brief's eight ----

    /// The Timing ruling: every state is a pure function of `t`, so
    /// evaluating the frames in any order gives bit-identical states.
    #[test]
    fn random_access_is_bit_identical() {
        let tl = timeline(serde_json::json!([
            {"at":0.5,"set_paint_property":{"layer":"water","name":"fill-color","value":"#f00"}},
            {"at":{"after":"a","offset":-1.5},"until":{"at_start":"b","offset":2},"tween":{"param":"dots.epoch_mix","from":0,"to":1,"easing":"ease-in-out"}},
            {"at":1,"until":7.3,"caption":{"text":"Hi","anchor":"top","fade_s":0.7,"color":"rgba(10,20,30,0.5)"}},
            {"at":2,"until":6,"tween":{"param":"sun.time","from":0,"to":7200,"easing":[0.25,0.1,0.25,1]}},
            {"at":3.1,"hide_overlay":{"id":"routes"}},
            {"at":5,"set_overlay_data":{"id":"routes","geojson":"b.geojson"}},
            {"at":6.5,"set_param":{"param":"sun.time","value":"2026-06-21T12:00:00Z"}},
            {"at":7,"settle":{"timeout_s":5}},
            {"at":8,"set_layer_visibility":{"layer":"bg","visible":false}}
        ]));
        let frames: Vec<f64> = (0..=300).map(|n| f64::from(n) / 30.0).collect();
        let forward: Vec<SceneState> = frames.iter().map(|&t| tl.state_at(t, 30)).collect();
        let bits = |s: &SceneState| {
            let p = &s.params;
            (p.raster_opacity.to_bits(), p.epoch_mix.to_bits(), p.sun_unix.to_bits(), s.captions.iter().map(|c| c.alpha.to_bits()).collect::<Vec<_>>())
        };
        // A fixed scramble of the frame order, each frame alone.
        let mut order: Vec<usize> = (0..frames.len()).collect();
        order.sort_by_key(|&i| (i * 7919) % frames.len());
        for &i in order.iter().rev() {
            let fresh = tl.state_at(frames[i], 30);
            assert_eq!(fresh, forward[i], "frame {i}");
            assert_eq!(bits(&fresh), bits(&forward[i]), "frame {i}");
        }
        // And the schedule really moves: two generations, a tween, a pin.
        assert_eq!(forward[300].style_generation, 2);
        assert!(forward[90].params.epoch_mix > 0.0 && forward[90].params.epoch_mix < 1.0);
        assert_eq!(forward[300].params.sun_unix, parse_utc("2026-06-21T12:00:00Z").unwrap(), "a sun.time set pins the sun");
        assert!(forward[100].visible_overlays.is_empty());
        assert_eq!(forward[150].overlay_data, vec![("routes".to_string(), "b.geojson".to_string())]);
    }

    #[test]
    fn event_times_resolve_through_labels_and_bad_ones_are_diagnostics() {
        let tl = timeline(serde_json::json!([{"at":{"after":"a","offset":0.5},"set_globe":{"on":false}},{"at":{"at_start":"b"},"set_basemap":{"source":"mapbox"}}]));
        assert_eq!(tl.state_at(4.49, 30).globe, None);
        assert_eq!(tl.state_at(4.5, 30).globe, Some(false));
        assert_eq!(tl.state_at(3.99, 30).basemap, None);
        assert_eq!(tl.state_at(4.0, 30).basemap.as_deref(), Some("mapbox"));

        let err = try_timeline(serde_json::json!([
            {"at":{"after":"c"},"set_globe":{"on":true}},
            {"at":10.5,"set_globe":{"on":true}},
            {"at":{"at_start":"x"},"settle":{}},
            {"at":1,"until":0.5,"caption":{"text":"x","anchor":"top"}},
            {"at":1,"tween":{"param":"raster_opacity","from":0,"to":1}}
        ]))
        .unwrap_err();
        let got: Vec<(&str, &str)> = err.iter().map(|d| (d.code, d.path.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("unknown_label", "/events/0/at/after"),
                ("event_after_end", "/events/1/at"),
                ("unknown_label", "/events/2/at/at_start"),
                ("until_before_at", "/events/3/until"),
                ("tween_needs_until", "/events/4/until"),
            ]
        );
        assert_eq!(err[0].hint.as_deref(), Some("did you mean `a`?"));
        assert!(err.iter().all(|d| d.severity == Severity::Error));
    }

    #[test]
    fn unknown_layer_lists_the_nearest_ids() {
        let err = try_timeline(serde_json::json!([{"at":0,"set_paint_property":{"layer":"watr","name":"fill-color","value":"#f00"}}])).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].message.contains(r#"["water", "bg"]"#), "{}", err[0].message);
        assert_eq!(err[0].hint.as_deref(), Some("did you mean `water`?"));
    }

    #[test]
    fn edits_chain_and_set_style_replaces_the_document() {
        let tl = try_timeline_with_loader(
            serde_json::json!([
                {"at":1,"set_filter":{"layer":"water","filter":["==","class","sea"]}},
                {"at":2,"set_layout_property":{"layer":"water","name":"fill-sort-key","value":3}},
                {"at":3,"set_style":{"style":"other.json"}},
                {"at":4,"set_layer_visibility":{"layer":"only","visible":false}}
            ]),
            &|name| (name == "other.json").then(|| serde_json::json!({"version":8,"sources":{},"layers":[{"id":"only","type":"background"}]})),
        )
        .unwrap();
        assert_eq!(tl.generations(), 5);
        let g2 = tl.style_document(2).unwrap();
        assert_eq!(g2["layers"][1]["filter"], serde_json::json!(["==", "class", "sea"]), "generation 2 keeps generation 1's edit");
        assert_eq!(g2["layers"][1]["layout"]["fill-sort-key"], 3);
        assert_eq!(tl.style_document(4).unwrap()["layers"][0]["layout"]["visibility"], "none");
        assert_eq!(tl.style_document(5), None);
        assert_eq!(tl.generation_source(0), None);
        assert_eq!(tl.generation_source(3), Some(2));
        assert_eq!(tl.state_at(3.5, 30).style_generation, 3);
        let missing = try_timeline(serde_json::json!([{"at":1,"set_style":{"style":"gone.json"}}])).unwrap_err();
        assert_eq!((missing[0].code, missing[0].path.as_str()), ("style_not_found", "/events/0/set_style/style"));
    }

    #[test]
    fn settle_points_fall_on_one_frame() {
        let tl = timeline(serde_json::json!([{"at":7.01,"settle":{"timeout_s":5}}]));
        // The settle hold "b" starts at 4.0: frame 120 at 30 fps.
        assert_eq!(tl.state_at(4.0, 30).settle, Some(DEFAULT_SETTLE_TIMEOUT_S));
        assert_eq!(tl.state_at(119.0 / 30.0, 30).settle, None);
        assert_eq!(tl.state_at(121.0 / 30.0, 30).settle, None);
        // 7.01 s is between frames 210 and 211: the first frame after it.
        assert_eq!(tl.state_at(210.0 / 30.0, 30).settle, None);
        assert_eq!(tl.state_at(211.0 / 30.0, 30).settle, Some(5.0));
        assert_eq!(tl.state_at(71.0 / 10.0, 10).settle, Some(5.0), "the frame depends on fps");
    }

    #[test]
    fn params_default_from_the_scene_and_bad_values_are_diagnostics() {
        let s = timeline(serde_json::json!([])).state_at(0.0, 30);
        assert_eq!(
            s.params,
            Params { raster_opacity: 1.0, epoch_mix: 0.0, dots_ppd: None, dots_radius_px: None, sun_unix: parse_utc("2026-03-20T10:00:00Z").unwrap(), atmosphere: true, stars: false, labels: true, terminator: false }
        );
        assert_eq!(s.visible_overlays, vec!["routes".to_string()]);
        let tl = timeline(serde_json::json!([{"at":1,"set_param":{"param":"dots.ppd","value":4}},{"at":1,"set_param":{"param":"sun.time","value":3600}}]));
        assert_eq!(tl.state_at(1.0, 30).params.dots_ppd, Some(4.0));
        assert_eq!(tl.state_at(9.0, 30).params.sun_unix, tl.state_at(0.0, 30).params.sun_unix + 3600.0, "pinned, not advancing");

        let err = try_timeline(serde_json::json!([
            {"at":1,"set_param":{"param":"labels.on","value":1}},
            {"at":1,"set_param":{"param":"raster_opacity","value":"half"}},
            {"at":1,"set_param":{"param":"sun.time","value":"noon"}},
            {"at":1,"until":2,"tween":{"param":"stars.on","from":0,"to":1}},
            {"at":1,"set_param":{"param":"dots.ppd","value":1e300}},
            {"at":1,"hide_overlay":{"id":"roads"}},
            {"at":1,"caption":{"text":"x","anchor":"middle","color":"blue-ish","size_px":0}},
            {"at":1,"settle":{"timeout_s":0}}
        ]))
        .unwrap_err();
        let got: Vec<(&str, &str)> = err.iter().map(|d| (d.code, d.path.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("bad_param_value", "/events/0/set_param/value"),
                ("bad_param_value", "/events/1/set_param/value"),
                ("bad_param_value", "/events/2/set_param/value"),
                ("param_not_tweenable", "/events/3/tween/param"),
                ("bad_param_value", "/events/4/set_param/value"),
                ("unknown_overlay", "/events/5/hide_overlay/id"),
                ("bad_anchor", "/events/6/caption/anchor"),
                ("bad_color", "/events/6/caption/color"),
                ("bad_caption_size", "/events/6/caption/size_px"),
                ("bad_settle_timeout", "/events/7/settle/timeout_s"),
            ]
        );
    }

    /// R1: a settle timeout past an hour (1e12 s would overflow
    /// `Instant + Duration` in the player) is a diagnostic at its path;
    /// an hour exactly is fine.
    #[test]
    fn a_huge_settle_timeout_is_a_diagnostic_not_a_panic() {
        let err = try_timeline(serde_json::json!([{"at":1,"settle":{"timeout_s":1e12}}])).unwrap_err();
        let got: Vec<(&str, &str)> = err.iter().map(|d| (d.code, d.path.as_str())).collect();
        assert_eq!(got, vec![("bad_settle_timeout", "/events/0/settle/timeout_s")]);
        let tl = timeline(serde_json::json!([{"at":1,"settle":{"timeout_s":MAX_SETTLE_TIMEOUT_S}}]));
        assert_eq!(tl.state_at(1.0, 30).settle, Some(MAX_SETTLE_TIMEOUT_S));
    }

    #[test]
    fn a_caption_defaults_to_white_on_a_plate_and_parses_its_colour() {
        let tl = timeline(serde_json::json!([
            {"at":1,"caption":{"text":"A","anchor":"top-right"}},
            {"at":1,"until":9,"caption":{"text":"B","anchor":"center","color":"#ff8000","plate":false,"offset_px":[4,-2],"size_px":30}}
        ]));
        let s = tl.state_at(5.0, 30);
        assert_eq!(
            s.captions[0],
            ActiveCaption { text: "A".into(), anchor: Anchor::TopRight, offset_px: (0.0, 0.0), size_px: DEFAULT_CAPTION_SIZE_PX, color: [255, 255, 255, 255], plate: true, alpha: 1.0 }
        );
        assert_eq!(s.captions[1].color, [255, 128, 0, 255]);
        assert!(!s.captions[1].plate);
        assert_eq!(s.captions[1].offset_px, (4.0, -2.0));
        // No `until`: on to the end, with no fade out.
        assert_eq!(tl.state_at(10.0, 30).captions.len(), 1);
        assert_eq!(tl.state_at(10.0, 30).captions[0].alpha, 1.0);
        assert!((tl.state_at(1.2, 30).captions[0].alpha - 0.5).abs() < 1e-6, "the default fade is 0.4 s");
    }

    #[test]
    fn out_of_range_times_are_clamped_not_panics() {
        let tl = timeline(serde_json::json!([{"at":1,"set_param":{"param":"stars.on","value":true}}]));
        assert_eq!(tl.state_at(f64::NAN, 30), tl.state_at(0.0, 30));
        assert_eq!(tl.state_at(-5.0, 30), tl.state_at(0.0, 30));
        assert_eq!(tl.state_at(f64::INFINITY, 30), tl.state_at(10.0, 30));
        assert!(tl.state_at(1e300, 0).params.stars);
    }

    /// F3: edits at one instant are one generation, and an edit that
    /// changes nothing makes none.
    #[test]
    fn same_instant_edits_coalesce_and_no_op_edits_make_no_generation() {
        let tl = timeline(serde_json::json!([
            {"at":1,"set_paint_property":{"layer":"water","name":"fill-color","value":"#f00"}},
            {"at":1,"set_filter":{"layer":"water","filter":["==","class","sea"]}},
            {"at":2,"set_paint_property":{"layer":"water","name":"fill-color","value":"#f00"}},
            {"at":3,"set_paint_property":{"layer":"bg","name":"background-color","value":"#000"}},
            {"at":3,"set_paint_property":{"layer":"bg","name":"background-color","value":"#fff"}}
        ]));
        assert_eq!(tl.generations(), 2, "t=1 coalesces; t=2 and t=3 change nothing");
        let g1 = tl.style_document(1).unwrap();
        assert_eq!(g1["layers"][1]["paint"]["fill-color"], "#f00");
        assert_eq!(g1["layers"][1]["filter"], serde_json::json!(["==", "class", "sea"]));
        assert_eq!(tl.generation_source(1), Some(1), "the instant's last edit");
        assert_eq!(tl.state_at(0.99, 30).style_generation, 0);
        assert_eq!(tl.state_at(1.0, 30).style_generation, 1);
        assert_eq!(tl.state_at(9.0, 30).style_generation, 1);
        // Shared, not copied.
        assert!(Arc::ptr_eq(tl.style_document_shared(1).unwrap(), tl.clone().style_document_shared(1).unwrap()));
    }

    /// N4: `null` removes the property, as MapLibre does.
    #[test]
    fn a_null_value_removes_the_property() {
        let tl = timeline(serde_json::json!([
            {"at":1,"set_filter":{"layer":"water","filter":["==","class","sea"]}},
            {"at":2,"set_filter":{"layer":"water","filter":null}},
            {"at":3,"set_paint_property":{"layer":"water","name":"fill-color","value":null}},
            {"at":4,"set_layout_property":{"layer":"water","name":"visibility","value":null}}
        ]));
        assert_eq!(tl.generations(), 4, "the layout removal changes nothing");
        assert!(tl.style_document(2).unwrap()["layers"][1].get("filter").is_none());
        assert!(tl.style_document(3).unwrap()["layers"][1]["paint"].get("fill-color").is_none());
    }

    /// F5: bounded params are range-checked when set, and a bezier that
    /// overshoots is clamped.
    #[test]
    fn params_are_range_checked_and_overshoot_is_clamped() {
        let err = try_timeline(serde_json::json!([
            {"at":1,"set_param":{"param":"raster_opacity","value":5}},
            {"at":1,"until":2,"tween":{"param":"dots.epoch_mix","from":-0.5,"to":1}},
            {"at":1,"set_param":{"param":"dots.radius_px","value":-1}}
        ]))
        .unwrap_err();
        let got: Vec<(&str, &str)> = err.iter().map(|d| (d.code, d.path.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("param_out_of_range", "/events/0/set_param/value"),
                ("param_out_of_range", "/events/1/tween/from"),
                ("param_out_of_range", "/events/2/set_param/value"),
            ]
        );
        // y1 = -1, y2 = 2: the curve leaves [0, 1] at both ends.
        let tl = timeline(serde_json::json!([{"at":1,"until":3,"tween":{"param":"raster_opacity","from":0,"to":1,"easing":[0.3,-1,0.7,2]}}]));
        let curve = Easing::Bezier([0.3, -1.0, 0.7, 2.0]);
        assert!(curve.apply(0.1) < 0.0 && curve.apply(0.9) > 1.0, "the premise: the curve overshoots both ends");
        let values: Vec<f32> = (0..=40).map(|k| tl.state_at(1.0 + f64::from(k) * 0.05, 30).params.raster_opacity).collect();
        assert!(values.iter().all(|v| (0.0..=1.0).contains(v)), "{values:?}");
        assert_eq!(tl.state_at(1.2, 30).params.raster_opacity, 0.0);
        assert_eq!(tl.state_at(2.8, 30).params.raster_opacity, 1.0);
    }

    /// F6: a tween with no easing uses "ease", as everywhere in the spec.
    #[test]
    fn a_tween_defaults_to_ease() {
        let tl = timeline(serde_json::json!([{"at":0,"until":4,"tween":{"param":"dots.epoch_mix","from":0,"to":1}}]));
        let expected = Easing::Ease.apply(0.25) as f32;
        assert_eq!(tl.state_at(1.0, 30).params.epoch_mix, expected);
        assert!((expected - 0.25).abs() > 0.05, "ease is not linear at 0.25");
    }

    /// F7: `until` must be after `at`.
    #[test]
    fn until_equal_to_at_is_refused() {
        let err = try_timeline(serde_json::json!([{"at":2,"until":2,"caption":{"text":"x","anchor":"top"}}])).unwrap_err();
        assert_eq!((err[0].code, err[0].path.as_str()), ("until_before_at", "/events/0/until"));
    }

    /// N5: caption colours may be CSS 2.1 names.
    #[test]
    fn a_caption_colour_may_be_a_css_name() {
        let tl = timeline(serde_json::json!([{"at":1,"caption":{"text":"x","anchor":"top","color":"Orange"}},{"at":1,"caption":{"text":"y","anchor":"top","color":"transparent"}}]));
        let s = tl.state_at(2.0, 30);
        assert_eq!(s.captions[0].color, [255, 165, 0, 255]);
        assert_eq!(s.captions[1].color, [0, 0, 0, 0]);
        let err = try_timeline(serde_json::json!([{"at":1,"caption":{"text":"x","anchor":"top","color":"rebeccapurple"}}])).unwrap_err();
        assert!(err[0].hint.as_deref().unwrap().contains("orange"), "{:?}", err[0].hint);
    }

    /// Review round 1, finding 1: `chapter_past_end` is a warning, so
    /// `Timeline::new` still succeeds, and the warning must still be
    /// reachable from the `Timeline` it built -- not silently dropped.
    #[test]
    fn a_past_end_chapters_warning_is_visible_through_timeline_warnings() {
        let mut script = script_with(serde_json::json!([]));
        script.chapters = serde_json::from_value(serde_json::json!([{"id": "a", "title": "A", "from": 0.0, "to": 15.0}])).unwrap();
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        assert_eq!(path.duration(), 10.0);
        let tl = Timeline::new(&script, &path, style(), &|_| None, &[]).unwrap();
        assert_eq!(tl.chapters()[0].to, 10.0, "clamped to the path's duration");
        assert!(
            tl.warnings().iter().any(|d| d.code == "chapter_past_end" && d.severity == Severity::Warning && d.path == "/chapters/0/to"),
            "{:?}",
            tl.warnings()
        );
        // Only the warning, never an error: the build succeeded.
        assert!(tl.warnings().iter().all(|d| d.severity != Severity::Error));
    }

    /// The minimal fixture's caption resolves from its `after` label.
    #[test]
    fn the_minimal_fixture_builds_a_timeline() {
        let script = crate::story::script::parse(include_bytes!("../../tests/fixtures/stories/minimal.json")).unwrap();
        let path = CameraPath::new((480.0, 270.0), &script, &TrackSet::default()).unwrap();
        let tl = Timeline::new(&script, &path, style(), &|_| None, &[]).unwrap();
        assert!(tl.state_at(1.4, 10).captions.is_empty());
        assert_eq!(tl.state_at(1.6, 10).captions[0].text, "Marina Bay");
        assert_eq!(tl.state_at(0.0, 10).settle, Some(DEFAULT_SETTLE_TIMEOUT_S));
    }

    // ---- overlay effects (Task 4) ----

    /// A 10 s hold with overlays `routes` (line layer `route-line`) and
    /// `harbours` (circle layer `ports`, three points with a `tons` and a
    /// `t` property), their sources, and `events`.
    fn effects_timeline(events: serde_json::Value) -> Result<Timeline, Vec<Diagnostic>> {
        let mut script = test_script(serde_json::json!([{"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 2}}, {"type": "hold", "duration": 10.0}]));
        script.scene.overlays = serde_json::from_value(serde_json::json!([
            {"id": "routes", "data": "r.geojson", "line_metrics": true, "layers": [{"id": "route-line", "type": "line"}]},
            {"id": "harbours", "data": "h.geojson", "layers": [{"id": "ports", "type": "circle", "paint": {"circle-color": "#0000ff"}}]}
        ]))
        .unwrap();
        script.events = serde_json::from_value(events).unwrap();
        let line = serde_json::json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": [[0.0, 0.0], [10.0, 0.0]]}}]});
        let points = serde_json::json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {"tons": 10, "t": 2.0}, "geometry": {"type": "Point", "coordinates": [0, 0]}},
            {"type": "Feature", "properties": {"tons": 500, "t": 0.5}, "geometry": {"type": "Point", "coordinates": [1, 0]}},
            {"type": "Feature", "properties": {"tons": 900}, "geometry": {"type": "Point", "coordinates": [2, 0]}}]});
        let mut layers = crate::story::overlay_layers::layer_sources(0, &script.scene.overlays[0], &line).unwrap();
        layers.extend(crate::story::overlay_layers::layer_sources(1, &script.scene.overlays[1], &points).unwrap());
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        Timeline::new(&script, &path, serde_json::json!({"version": 8, "layers": []}), &|_| None, &layers)
    }

    #[test]
    fn scene_state_carries_one_layer_fx_per_declared_layer() {
        let tl = effects_timeline(serde_json::json!([
            {"at": 1.0, "until": 5.0, "overlay": {"layer": "route-line", "effect": "draw", "easing": "linear"}}])).unwrap();
        let before = tl.state_at(0.5, 30);
        assert_eq!(before.layer_fx.iter().map(|f| f.layer_id.as_str()).collect::<Vec<_>>(), ["route-line", "ports"]);
        assert_eq!(before.layer_fx[0].opacity, 0.0, "drawn later: hidden until then");
        assert_eq!(before.layer_fx[1], LayerFx { layer_id: "ports".to_string(), ..LayerFx::default() }, "untouched: as declared");
        assert_eq!(tl.state_at(3.0, 30).layer_fx[0].trim, Some([0.0, 0.5]));
        assert_eq!(tl.effect_plans().len(), 1);
        assert_eq!(tl.effect_plans()[0].until, 5.0);
    }

    #[test]
    fn highlight_ids_and_where_resolve_to_feature_ids() {
        let tl = effects_timeline(serde_json::json!([
            {"at": 0.0, "until": 9.0, "overlay": {"layer": "ports", "effect": "highlight", "ids": [0, 99], "color": "#ff0000"}},
            {"at": 0.0, "until": 9.0, "overlay": {"layer": "ports", "effect": "highlight", "where": [">", ["get", "tons"], 100]}}])).unwrap();
        let plans = tl.effect_plans();
        assert_eq!(plans[0].selected, vec![0]);
        assert_eq!(plans[0].color, Some([255, 0, 0, 255]));
        assert_eq!(plans[1].selected, vec![1, 2], "`where` over properties; ids are document indices");
        let w = tl.warnings();
        assert!(w.iter().any(|d| d.code == "unknown_feature_id" && d.path == "/events/0/overlay/ids/1"), "{w:?}");
        // Colour mixes from the layer's own blue to red over fade_s.
        let fx = &tl.state_at(0.2, 30).layer_fx[1];
        assert_eq!(fx.features.iter().find(|f| f.id == 0).unwrap().color, Some([128, 0, 128, 255]));
    }

    #[test]
    fn drop_reveals_by_stagger_or_by_time_property_and_ripples_only_when_asked() {
        let tl = effects_timeline(serde_json::json!([
            {"at": 1.0, "overlay": {"layer": "ports", "effect": "drop", "stagger_s": 0.5}}])).unwrap();
        assert_eq!(tl.effect_plans()[0].reveal, vec![(0, 0.0), (1, 0.5), (2, 1.0)]);
        assert_eq!(tl.effect_plans()[0].max_radius_px, 0.0);
        let tl = effects_timeline(serde_json::json!([
            {"at": 1.0, "overlay": {"layer": "ports", "effect": "drop", "time_property": "t", "ripple": true}}])).unwrap();
        assert_eq!(tl.effect_plans()[0].reveal, vec![(0, 2.0), (1, 0.5), (2, 0.0)], "a feature without the property reveals at once");
        assert_eq!(tl.effect_plans()[0].max_radius_px, crate::story::effects::DEFAULT_PULSE_RADIUS_PX);
        assert_eq!(tl.effect_plans()[0].until, f64::INFINITY);
    }

    #[test]
    fn effects_in_the_timeline_seek_as_they_play() {
        let tl = effects_timeline(serde_json::json!([
            {"at": 0.5, "until": 4.0, "overlay": {"layer": "route-line", "effect": "draw", "head": {"radius_px": 5, "color": "#fff"}}},
            {"at": 6.0, "until": 8.0, "overlay": {"layer": "route-line", "effect": "fade"}},
            {"at": 1.0, "overlay": {"layer": "ports", "effect": "drop", "ripple": true}},
            {"at": 2.0, "until": 7.0, "overlay": {"layer": "ports", "effect": "highlight", "ids": [1], "color": "#f00"}},
            {"at": 3.0, "overlay": {"layer": "ports", "effect": "pulse", "period_s": 1.3}}])).unwrap();
        let forward: Vec<_> = (0..300).map(|n| tl.state_at(f64::from(n) / 30.0, 30)).collect();
        let mut backward: Vec<_> = (0..300).rev().map(|n| tl.state_at(f64::from(n) / 30.0, 30)).collect();
        backward.reverse();
        assert!(forward == backward);
        // Every kind showed up somewhere, so the comparison had state to compare.
        let fx: Vec<(LayerFx, LayerFx)> = forward.iter().map(|s| (s.layer_fx[0].clone(), s.layer_fx[1].clone())).collect();
        assert!(fx.iter().any(|(l, _)| l.head.is_some() && l.trim.is_some_and(|[_, e]| e > 0.0 && e < 1.0)), "a draw in progress");
        assert!(fx.iter().any(|(l, _)| l.opacity > 0.0 && l.opacity < 1.0), "the fade");
        assert!(fx.iter().any(|(_, p)| p.features.iter().any(|f| f.id == 1 && f.color.is_some_and(|c| c != [255, 0, 0, 255]))), "a highlight mid-ramp");
        assert!(fx.iter().any(|(_, p)| p.features.iter().any(|f| f.radius_scale > 1.0)), "a drop overshoot");
        assert!(fx.iter().any(|(_, p)| p.rings.len() >= 3), "pulse rings on every point");
        assert!(fx.iter().any(|(l, p)| *l == LayerFx { layer_id: "route-line".to_string(), opacity: 0.0, ..LayerFx::default() } && p.features.iter().all(|f| f.alpha == 0.0)), "the hidden start");
    }

    #[test]
    fn an_effect_on_an_undeclared_layer_is_refused() {
        let err = effects_timeline(serde_json::json!([{"at": 1.0, "overlay": {"layer": "nope", "effect": "show"}}])).unwrap_err();
        assert_eq!((err[0].code, err[0].path.as_str()), ("unknown_layer", "/events/0/overlay/layer"));
    }

    /// The `highlight` selection of `ids` over a `ports` circle layer of
    /// `features`, and the Timeline's warnings.
    fn select(features: serde_json::Value, ids: serde_json::Value) -> (Vec<u64>, Vec<Diagnostic>) {
        let mut script = test_script(serde_json::json!([{"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 2}}, {"type": "hold", "duration": 10.0}]));
        script.scene.overlays = serde_json::from_value(serde_json::json!([
            {"id": "harbours", "data": "h.geojson", "layers": [{"id": "ports", "type": "circle", "paint": {"circle-color": "#00f"}}]}])).unwrap();
        script.events = serde_json::from_value(serde_json::json!([
            {"at": 0.0, "until": 5.0, "overlay": {"layer": "ports", "effect": "highlight", "ids": ids, "color": "#f00"}}])).unwrap();
        let doc = serde_json::json!({"type": "FeatureCollection", "features": features});
        let layers = crate::story::overlay_layers::layer_sources(0, &script.scene.overlays[0], &doc).unwrap();
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        let tl = Timeline::new(&script, &path, serde_json::json!({"version": 8, "layers": []}), &|_| None, &layers).unwrap();
        (tl.effect_plans()[0].selected.clone(), tl.warnings().to_vec())
    }

    fn point(id: Option<serde_json::Value>) -> serde_json::Value {
        let mut f = serde_json::json!({"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [0, 0]}});
        if let Some(id) = id {
            f["id"] = id;
        }
        f
    }

    #[test]
    fn string_ids_select_exactly_their_features() {
        let features = serde_json::json!([point(Some("DEU".into())), point(Some("FRA".into())), point(Some("ITA".into()))]);
        let (selected, warnings) = select(features, serde_json::json!(["FRA"]));
        assert_eq!(selected, vec![1]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn duplicate_ids_select_every_feature_that_carries_them() {
        let features = serde_json::json!([point(Some("X".into())), point(Some("Y".into())), point(Some("X".into()))]);
        assert_eq!(select(features, serde_json::json!(["X"])).0, vec![0, 2]);
    }

    #[test]
    fn numbers_match_numerically_and_never_a_string() {
        let features = serde_json::json!([point(Some(serde_json::json!(7.0))), point(Some(serde_json::json!(250))), point(Some(serde_json::json!("9")))]);
        let (selected, warnings) = select(features, serde_json::json!([7, "250", 9]));
        assert_eq!(selected, vec![0], "7 matches 7.0; \"250\" is not 250; 9 is not \"9\"");
        let warned: Vec<&str> = warnings.iter().filter(|d| d.code == "unknown_feature_id").map(|d| d.path.as_str()).collect();
        assert_eq!(warned, ["/events/0/overlay/ids/1", "/events/0/overlay/ids/2"]);
        let hint = warnings[0].hint.as_deref().unwrap();
        assert!(hint.contains("7.0") && hint.contains("250") && hint.contains("\"9\""), "the hint lists the real ids: {hint}");
    }

    #[test]
    fn an_explicit_id_equal_to_another_features_index_does_not_collide() {
        // Feature 0 says `id: 1`; feature 1 has no id, so it is index 1.
        let features = serde_json::json!([point(Some(serde_json::json!(1))), point(None)]);
        let mut script = test_script(serde_json::json!([{"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 2}}, {"type": "hold", "duration": 10.0}]));
        script.scene.overlays = serde_json::from_value(serde_json::json!([
            {"id": "harbours", "data": "h.geojson", "layers": [{"id": "ports", "type": "circle"}]}])).unwrap();
        let layers =
            crate::story::overlay_layers::layer_sources(0, &script.scene.overlays[0], &serde_json::json!({"type": "FeatureCollection", "features": features})).unwrap();
        // Internally two distinct features, whatever the author's ids say.
        assert_eq!(layers[0].feature_ids, vec![0, 1]);
        script.events = serde_json::from_value(serde_json::json!([
            {"at": 1.0, "overlay": {"layer": "ports", "effect": "drop", "stagger_s": 1.0}}])).unwrap();
        let path = CameraPath::new((800.0, 600.0), &script, &TrackSet::default()).unwrap();
        let tl = Timeline::new(&script, &path, serde_json::json!({"version": 8, "layers": []}), &|_| None, &layers).unwrap();
        let fx = &tl.state_at(1.5, 30).layer_fx[0];
        // The first has dropped; the second has not: two overrides, not one merged.
        assert_eq!(fx.features.iter().map(|f| (f.id, f.alpha)).collect::<Vec<_>>(), vec![(0, 1.0), (1, 0.0)]);
        // F2b: the source has ids, so the id-less feature cannot be named
        // by its index: `ids: [1]` is only the feature whose id IS 1, and
        // `ids: [0]` names nothing, with a warning about the id-less one.
        let mixed = || serde_json::json!([point(Some(serde_json::json!(1))), point(None)]);
        assert_eq!(select(mixed(), serde_json::json!([1])).0, vec![0]);
        let (selected, warnings) = select(mixed(), serde_json::json!([0]));
        assert_eq!(selected, Vec::<u64>::new());
        let codes: Vec<&str> = warnings.iter().map(|d| d.code).collect();
        assert_eq!(codes, ["features_without_id", "unknown_feature_id"]);
        assert!(warnings[0].message.starts_with("1 feature(s)") && warnings[0].hint.as_deref().unwrap_or_default().contains("where"), "{warnings:?}");
        // A source with no ids at all: the index addresses a feature.
        let (selected, warnings) = select(serde_json::json!([point(None), point(None)]), serde_json::json!([0]));
        assert_eq!(selected, vec![0]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
