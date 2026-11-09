//! **S1: story-script types, parsing, structural validation and diagnostics.**
//!
//! A story script is the JSON format an agent (or a person) writes to
//! describe a camera flight that becomes a video: a starting pose, a
//! sequence of camera moves (`camera`), scene setup (`scene`) and timed
//! actions (`events`). This file owns the shared, pure half of validation
//! (see `docs/superpowers/specs/2026-09-29-map-stories-capture-design.md`
//! §2.2): the bytes are parsed, every field name is checked against a known
//! set, and every range rule that can be checked without a GPU, a style
//! document or a clock is checked here.
//!
//! # Two passes, so nothing hides behind something else
//!
//! 1. `serde_json::from_slice::<serde_json::Value>` -- a syntax error here
//!    is the only case that stops early, because there is no tree left to
//!    walk.
//! 2. The `Value` is walked by hand against a table of known field names
//!    per object kind (`unknown_field`, with a "did you mean" hint), and
//!    every range/structural rule below is checked directly against the
//!    `Value` -- not the typed [`Script`] -- so a type error in one field
//!    never hides a range error in another (see [`parse`]). Required
//!    fields are also checked here (`missing_field`/`type_mismatch`, with
//!    the exact JSON pointer), so a missing `duration` names its segment.
//! 3. `serde_json::from_value::<Script>` builds the typed value the rest of
//!    the pipeline uses. Its error is folded in only when step 2 found no
//!    `Severity::Error` diagnostic at all, so a script with a specific
//!    mistake is never ALSO reported as a vague, pathless type mismatch --
//!    and conversely, `parse` can never return `Err` with nothing but a
//!    warning in it (every `Err` carries at least one `Error`).
//!
//! # Spellings
//!
//! Easing keywords are canonically kebab-case (`"ease-in-out"`, matching
//! the spec, CSS and MapLibre); the snake_case spelling
//! (`"ease_in_out"`) is accepted as an alias on read, never written.
//! A `follow` segment's `bearing` keyword is `"track"` (spec §2.1, and
//! plan Task 10's test); `"heading"` is rejected with a "did you mean
//! `track`?" hint.
//!
//! # What this file does not decide
//!
//! `EventTime::After`/`AtStart` need a segment's resolved duration to turn
//! into seconds, and a `fly` only gets one from its `speed` in
//! `CameraPath::new` (a later task). So `parse` checks that a referenced
//! **label exists**; whether the resulting time falls inside the story is
//! `Timeline::new`'s job once durations are known.
//!
//! # No panics, no clock
//!
//! This module reads no clock (`parse_utc` is a hand-rolled, strict RFC
//! 3339 reader with checked arithmetic, not `chrono`) and is
//! `#![deny(clippy::unwrap_used, clippy::expect_used,
//! clippy::indexing_slicing)]` outside tests, so a malformed script can
//! never panic the process that is validating it.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tiers::{default_safe_area, frame_size, pixel_ratio, Aspect, Entitlement, SafeArea, Tier};

// ---------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------

/// How serious a [`Diagnostic`] is. Only `Error` blocks [`parse`] from
/// returning a [`Script`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// One thing wrong (or worth mentioning) about a script, precise enough
/// for an agent to fix without guessing: a JSON pointer to the exact
/// value, a stable machine-checkable `code`, a human `message`, and an
/// optional actionable `hint`.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// A JSON pointer (RFC 6901), e.g. `/camera/2/to/zoom`. The document
    /// root is `""`. A key that itself contains `~` or `/` is escaped
    /// (`~0`/`~1`) as the RFC requires.
    pub path: String,
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

impl Diagnostic {
    fn error(path: &str, code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Error, path: path.to_string(), code, message: message.into(), hint: None }
    }

    fn warning(path: &str, code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Warning, path: path.to_string(), code, message: message.into(), hint: None }
    }

    fn with_hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }
}

// ---------------------------------------------------------------------
// Script types
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub story: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub output: OutputSpec,
    #[serde(default)]
    pub clock: ClockSpec,
    pub scene: SceneSpec,
    pub camera: Vec<Segment>,
    #[serde(default)]
    pub events: Vec<EventSpec>,
    /// The story's own alt text (spec §5): the fallback `alt` for the
    /// implicit chapter a script with no `chapters` gets (spec §2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The language the story's text is written in, as a BCP-47 tag
    /// (`"ja"`, `"pt-BR"`, `"zh-Hant-TW"`): the transcript's `<html lang>`
    /// (spec §5). `None` is `"en"`. Checked loosely ([`is_bcp47_like`],
    /// `bad_lang`): letters for the language, then `-`-separated
    /// alphanumeric subtags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Named segments of the timeline (spec §2.1). Chapters must not
    /// overlap; a script with none gets one implicit chapter covering
    /// the whole story (Task 3's job -- this file only checks the
    /// `from`/`to` each one names, not their overlap, which needs
    /// resolved seconds).
    #[serde(default)]
    pub chapters: Vec<ChapterSpec>,
    /// An explicit caption track (spec §5), alongside any `caption`
    /// events; piece 2 fills this from TTS word timings.
    #[serde(default)]
    pub captions: Vec<CaptionCueSpec>,
    /// Map furniture shown for the whole story unless a chapter
    /// overrides it with its own `furniture` (spec §4.5).
    #[serde(default)]
    pub furniture: FurnitureSpec,
    /// Design-system options for the whole story (spec §10.1): every
    /// field optional, each with its own default decided downstream
    /// (`design.rs`'s `archetype_for`/`theme_for`/`tone_for`, a later
    /// task) -- `None` here just means "decide for me".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub design: Option<DesignSpec>,
}

/// [`Script::design`] (spec §10.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignSpec {
    /// `journey | place | tour | area | timeline | vlog`. Default:
    /// detected (spec §2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archetype: Option<String>,
    /// `ember | ink | ocean | fill`. Default: by archetype (spec §6.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// `#RRGGBB`. Default: the theme's accent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// `auto | paper | ink`. Default: `auto` (spec §4.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// `ltr | rtl`. Default: from `lang` (spec §9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// `auto | compact | side`. Default: `auto`; landscape only (spec
    /// §5.7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// `bottom | above_platform` (spec §3.6, amended 2026-10-05): where
    /// the colophon row's sources and tile credit sit. Default: `bottom`,
    /// beside the bottom-left mark; `above_platform` lifts them above a
    /// platform safe area's bottom band. The mark is bottom-left in both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<String>,
    /// Whether the camera keeps the story clear of the card (spec
    /// §5.3). Default: `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_camera: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_ratio: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// What the author confirms they hold the rights to, e.g.
    /// `"source-licence:tiles.example.com"` or `"mapbox-video"` (spec
    /// §6's Mapbox video acknowledgement): a vector source outside the
    /// licence table (`burn_in`) is publishable only when named here by
    /// its source id (the manifest's `source`) or its URL's host, and
    /// choosing Mapbox for a video is publishable only once
    /// `"mapbox-video"` is named here. Recorded in the manifest's
    /// `licence_acknowledged`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acknowledge: Vec<String>,
    /// The frame's shape (spec §2.4). `None` together with `tier` also
    /// `None` keeps every pre-existing script resolving exactly as
    /// before ([`Script::output`]); either one present switches to the
    /// tier table and makes `width`/`height`/`pixel_ratio`/`preset` the
    /// error `size_and_tier`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect: Option<Aspect>,
    /// The resolution tier (spec §2.4). See `aspect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    /// The short-form app whose UI layout should avoid (spec §2.4).
    /// Defaults to [`tiers::default_safe_area`] for the resolved
    /// `aspect` when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_area: Option<SafeArea>,
    /// `"h264"` (the default) or `"hevc"` (spec §2.4 "Encoding").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Whether captions are burned into the video frame. `None` keeps
    /// today's behaviour for a script with neither `aspect` nor `tier`
    /// (burn-in on); the per-aspect default (spec §5) applies only once
    /// one of them is set -- a later task's `burn_in_default` reads this
    /// alongside `aspect`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_captions: Option<bool>,
    /// The story second the bundle's `poster.png` is rendered at (spec
    /// §5). `None` makes the poster the first chapter's still. A number
    /// `>= 0` here; `Timeline::new` checks it against the duration
    /// (`poster_outside_story`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_at: Option<f64>,
}

/// [`Script::output`]'s resolution of a preset plus field-by-field
/// overrides. `width`/`height` are PHYSICAL pixels (logical size times
/// `pixel_ratio`, rounded).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ResolvedOutput {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixel_ratio: f32,
}

/// [`Script::format`]'s resolution of a script's frame shape,
/// independent of its physical pixel count: the [`Aspect`], the
/// [`Tier`], the tier's [`Entitlement`], the [`SafeArea`] in force, and
/// the frame in 1080-referenced layout units (`physical / pixel_ratio`
/// -- the same picture, panels and safe areas at every tier, spec
/// §2.4's "Scaling").
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct OutputFormat {
    pub aspect: Aspect,
    pub tier: Tier,
    pub entitlement: Entitlement,
    pub safe_area: SafeArea,
    pub layout_size: (f32, f32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockSpec {
    #[serde(default = "default_start_utc")]
    pub start_utc: String,
    #[serde(default = "default_rate")]
    pub rate: f64,
    #[serde(default)]
    pub utc_offset_min: i32,
}

fn default_start_utc() -> String {
    "2026-03-20T10:00:00Z".to_string()
}

fn default_rate() -> f64 {
    1.0
}

fn default_true() -> bool {
    true
}

impl Default for ClockSpec {
    fn default() -> Self {
        ClockSpec { start_utc: default_start_utc(), rate: default_rate(), utc_offset_min: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneSpec {
    pub style: String,
    pub basemap: BasemapSpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raster: Option<String>,
    #[serde(default)]
    pub globe: bool,
    /// Defaults `true`: a story is a showcase, and MapLibre shows
    /// atmosphere by default (only drawn while the globe is enabled).
    #[serde(default = "default_true")]
    pub atmosphere: bool,
    #[serde(default)]
    pub stars: bool,
    /// Defaults `true`: an author who wants a bare frame says so with
    /// `"labels": false`, rather than getting a blank map by omission.
    #[serde(default = "default_true")]
    pub labels: bool,
    /// Day/night shading from the story clock. Defaults `false`: a story
    /// renders in daylight unless it opts in (the plain-defaults rule).
    /// The clock still drives `sun.time` for anything else that reads it.
    #[serde(default)]
    pub terminator: bool,
    /// How 3D buildings are shaded: `"enhanced"` (the default -- a story
    /// is a showcase, and the gallery's 3D buildings panel opens enhanced)
    /// or `"maplibre"` (MapLibre's look, bit for bit). Enhanced also hides
    /// the symbols a building hides from the eye, as that panel does
    /// (`ExtrusionOverrides::label_occlusion`).
    #[serde(default, skip_serializing_if = "BuildingsLook::is_default")]
    pub buildings: BuildingsLook,
    #[serde(default)]
    pub overlays: Vec<OverlaySpec>,
    #[serde(default)]
    pub tracks: Vec<TrackSpec>,
    #[serde(default)]
    pub credits: Vec<String>,
}

/// [`SceneSpec::buildings`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildingsLook {
    #[default]
    Enhanced,
    Maplibre,
}

impl BuildingsLook {
    /// For `skip_serializing_if`: a script that never named it round-trips
    /// without it.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The capture's `(extrusion_look, label_occlusion)` for this look.
    #[must_use]
    pub fn capture(self) -> (crate::render::extrusion::ExtrusionLook, bool) {
        match self {
            Self::Enhanced => (crate::render::extrusion::ExtrusionLook::ENHANCED, true),
            Self::Maplibre => (crate::render::extrusion::ExtrusionLook::MAPLIBRE, false),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum BasemapSpec {
    Named {
        source: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        wordmark: Option<String>,
    },
    Tiles {
        tiles: Vec<String>,
        max_zoom: u8,
        attribution: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlaySpec {
    pub id: String,
    /// A relative path to the overlay's GeoJSON document. `None` when
    /// [`data`](OverlaySpec::data) is used instead -- exactly one of the
    /// two must be present (`overlay_data`, spec §2.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geojson: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paint: Option<serde_json::Value>,
    #[serde(default)]
    pub above_labels: bool,
    /// An inline GeoJSON `FeatureCollection`, or a path relative to the
    /// story file (spec §2.2). Mutually exclusive with `geojson`
    /// (`overlay_data`); a path must not escape the story's folder
    /// (`bad_path`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Enables `line-progress` (spec §3.2) for this source, so a `draw`
    /// effect can trim one of its layers (`draw_needs_metrics`).
    #[serde(default)]
    pub line_metrics: bool,
    /// The overlay's own style layers (spec §2.2), MapLibre style-spec
    /// JSON. Layer ids share one namespace across every overlay
    /// (`duplicate_id`).
    #[serde(default)]
    pub layers: Vec<OverlayLayerSpec>,
    /// The credit this overlay's data owes, if any (spec §6's publish
    /// table). `None` is valid and credits nothing (F16): only images
    /// require a credit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

/// One of an [`OverlaySpec`]'s `layers` (spec §2.2): an ordinary
/// map-style layer, restricted to the three kinds an overlay effect can
/// drive (`line`/`fill`/`circle`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayLayerSpec {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: OverlayLayerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<serde_json::Value>,
    #[serde(default)]
    pub paint: serde_json::Map<String, serde_json::Value>,
    /// Draw this layer over the 3D buildings (story-content 1a, Task 22):
    /// in its own pass after the extrusions, with no depth test against
    /// them, and still under the labels and the story chrome. `None` is
    /// the kind's default ([`Self::above_buildings`]): on for `line` and
    /// `circle` (a route or a port is the story, and a wall must not cut
    /// it into dashes), off for `fill` (an area is ground the buildings
    /// stand on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above_buildings: Option<bool>,
    /// The layer's part in the story's design (spec §6.1): with a
    /// `role`, the theme paints it (`theme_layers`), the layer's own
    /// `paint` merged over the role's core key by key; with no `role` and
    /// an empty `paint`, the role is inferred. A `paint` with no `role` is
    /// drawn exactly as written, unthemed (spec §10.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// A point layer's label text (spec §6.3), e.g. `"ends"` on a
    /// `stops` layer's dots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// An `area` layer's own legend (spec §6.3): the values its fill is
    /// drawn from when no card legend names the layer, and what a card's
    /// `legend: {layers: [..]}` shows. Same shape as a card's legend,
    /// without `layers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legend: Option<Box<CardLegendSpec>>,
}

impl OverlayLayerSpec {
    /// [`Self::above_buildings`] resolved: the author's value, else the
    /// kind's default ([`OverlayLayerKind::above_buildings_by_default`]).
    #[must_use]
    pub fn above_buildings(&self) -> bool {
        self.above_buildings.unwrap_or(self.kind.above_buildings_by_default())
    }
}

impl OverlayLayerKind {
    /// Whether a layer of this kind draws over the 3D buildings when its
    /// spec does not say: `line` and `circle` do, `fill` does not.
    #[must_use]
    pub fn above_buildings_by_default(self) -> bool {
        matches!(self, Self::Line | Self::Circle)
    }
}

/// [`OverlayLayerSpec::kind`]: which effects a layer can take part in
/// (`effect_layer_type` -- `draw` needs `Line`, `drop`/`pulse` need
/// `Circle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlayLayerKind {
    Line,
    Fill,
    Circle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackSpec {
    pub id: String,
    pub file: String,
    pub style: TrackStyle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackStyle {
    pub color: String,
    pub radius_px: f32,
    pub trail_s: f64,
}

/// A visible default for a track drawn without a style (tests, the fuzz
/// harness): a 5 px blue dot with a 30 s trail. A script's `TrackSpec`
/// always names its own style; serde does not fall back to this.
impl Default for TrackStyle {
    fn default() -> Self {
        TrackStyle { color: "#1f77b4".to_string(), radius_px: 5.0, trail_s: 30.0 }
    }
}

// ---------------------------------------------------------------------
// Chapters, captions and map furniture (spec §2.1, §4.5, §5)
// ---------------------------------------------------------------------

/// A segment label or seconds: [`ChapterSpec`]'s `from`/`to` (spec §2.1).
/// Untagged: a bare number is `Seconds`, a bare string is `Label` (a
/// camera segment's `label`, checked the same way an event's `after`/
/// `at_start` is).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChapterTime {
    Seconds(f64),
    Label(String),
}

/// A named segment of the timeline (spec §2.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChapterSpec {
    pub id: String,
    pub title: String,
    pub from: ChapterTime,
    pub to: ChapterTime,
    /// Alt text for this chapter's still (spec §5). Falls back to
    /// [`Script::summary`] when absent -- an `Option` in the type, not a
    /// required field, so its absence is a warning (Task 17), not a
    /// parse error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    /// Overrides the still's capture time (spec §5's "Stills"); defaults
    /// to the chapter's midpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub still_at: Option<f64>,
    /// Overrides [`Script::furniture`] for this chapter only (spec §4.5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub furniture: Option<FurnitureSpec>,
    /// A card that spans the chapter (spec §10.1): an event over
    /// `[from, to]`. When a chapter card and an event card overlap, the
    /// event card wins for its span and the chapter card fills the rest
    /// (checked once durations are known, a later task).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<CardSpec>,
}

/// One cue of the caption track (spec §5): `captions: [{ "from", "to",
/// "text" }]`, filled by hand or, later, by TTS word timings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptionCueSpec {
    pub from: f64,
    pub to: f64,
    pub text: String,
}

/// `metric`, `imperial` or `both` (spec §4.5's scale bar). Defined here
/// (not in `tiers.rs`): it is a document field, not a frame constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScaleBarUnits {
    Metric,
    Imperial,
    Both,
}

/// `auto` (shown once the bearing is more than 2° from north), `always`
/// or `never` (spec §4.5's north arrow).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NorthArrowMode {
    Auto,
    Always,
    Never,
}

/// A legend generated from named overlay layers' paint properties (spec
/// §4.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegendSpec {
    /// Overlay layer ids (spec §2.2's one shared namespace); each must
    /// name a declared layer (`unknown_layer`).
    pub layers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Overrides the label shown for a `match`/`step` class, keyed by
    /// the class's own value.
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

/// Map furniture (spec §4.5): every item is optional, and a story sets
/// as many or as few as it wants. `#[derive(Default)]`: the story-level
/// default (nothing shown beyond the credit plate) and a chapter that
/// never overrides it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FurnitureSpec {
    /// Placed next to the credit plate, never overlapping it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_bar: Option<ScaleBarUnits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legend: Option<LegendSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub north_arrow: Option<NorthArrowMode>,
    /// Design-system fine-print source groups (spec §3.6, §10.1): each
    /// is a labelled line (`"Data"`, `"Imagery"`, or an empty key for
    /// an unlabelled line). 1a's `source_line`/`byline` still work
    /// alongside this (spec §10.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<SourceGroup>>,
}

/// One labelled line of the fine print's sources (spec §3.6, §10.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceGroup {
    pub key: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoseSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoom: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearing: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub globe: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_offset_px: Option<(f32, f32)>,
}

/// An easing curve: a named keyword, or its own cubic-bezier control
/// points `[x1, y1, x2, y2]`. Parses from either a bare string or a
/// 4-element array, so this carries a hand-written `Serialize`/
/// `Deserialize` rather than a derive (see the impls below). The
/// canonical spelling is kebab-case (`"ease-in-out"`, spec §2.1); the
/// snake_case form is accepted on read as an alias and never written.
#[derive(Debug, Clone, PartialEq)]
pub enum Easing {
    Linear,
    Ease,
    EaseIn,
    EaseOut,
    EaseInOut,
    Bezier([f64; 4]),
}

/// The largest |y| a bezier easing may have: overshoot is allowed (CSS
/// allows any y), but ten times the move is already far past any sane one.
pub const MAX_BEZIER_Y: f64 = 10.0;

/// The canonical (kebab-case) easing names, in the order tried for a
/// "did you mean" hint.
const EASING_NAMES: &[&str] = &["linear", "ease", "ease-in", "ease-out", "ease-in-out"];

impl Serialize for Easing {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Easing::Linear => serializer.serialize_str("linear"),
            Easing::Ease => serializer.serialize_str("ease"),
            Easing::EaseIn => serializer.serialize_str("ease-in"),
            Easing::EaseOut => serializer.serialize_str("ease-out"),
            Easing::EaseInOut => serializer.serialize_str("ease-in-out"),
            Easing::Bezier(points) => points.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Easing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value: Value = Deserialize::deserialize(deserializer)?;
        match value {
            Value::String(s) => {
                let kebab = s.replace('_', "-");
                match kebab.as_str() {
                    "linear" => Ok(Easing::Linear),
                    "ease" => Ok(Easing::Ease),
                    "ease-in" => Ok(Easing::EaseIn),
                    "ease-out" => Ok(Easing::EaseOut),
                    "ease-in-out" => Ok(Easing::EaseInOut),
                    _ => Err(serde::de::Error::custom(format!("unknown easing name `{s}`; expected one of {EASING_NAMES:?}"))),
                }
            }
            Value::Array(items) => {
                let nums: Vec<f64> = items
                    .iter()
                    .map(|v| v.as_f64().ok_or_else(|| serde::de::Error::custom("a bezier easing array must contain only numbers")))
                    .collect::<Result<_, _>>()?;
                let points: [f64; 4] = nums
                    .try_into()
                    .map_err(|_| serde::de::Error::custom("a bezier easing array must have exactly 4 numbers"))?;
                Ok(Easing::Bezier(points))
            }
            other => Err(serde::de::Error::custom(format!("easing must be a name or a 4-element array, got {other}"))),
        }
    }
}

impl Easing {
    /// The cubic-bezier curve this easing evaluates through. `Linear` has
    /// no dedicated `Interpolation` variant, so it uses the diagonal
    /// control points `(0,0)-(1,1)`, which is the identity curve (see
    /// `style_spec::expression`'s own `identity curve` test).
    ///
    /// `Interpolation` is `pub(crate)`: this is an intra-crate seam
    /// (`camera_path.rs`, Task 9, is the only other caller), so the type
    /// stays unnameable outside the crate rather than becoming public API.
    #[allow(private_interfaces)]
    pub fn curve(&self) -> crate::style_spec::expression::Interpolation {
        use crate::style_spec::expression::Interpolation;
        match self {
            Easing::Linear => Interpolation::CubicBezier { x1: 0.0, y1: 0.0, x2: 1.0, y2: 1.0 },
            Easing::Ease => Interpolation::CubicBezier { x1: 0.25, y1: 0.1, x2: 0.25, y2: 1.0 },
            Easing::EaseIn => Interpolation::CubicBezier { x1: 0.42, y1: 0.0, x2: 1.0, y2: 1.0 },
            Easing::EaseOut => Interpolation::CubicBezier { x1: 0.0, y1: 0.0, x2: 0.58, y2: 1.0 },
            Easing::EaseInOut => Interpolation::CubicBezier { x1: 0.42, y1: 0.0, x2: 0.58, y2: 1.0 },
            Easing::Bezier([x1, y1, x2, y2]) => Interpolation::CubicBezier { x1: *x1, y1: *y1, x2: *x2, y2: *y2 },
        }
    }

    /// The eased fraction for `t` in `[0, 1]` (clamped). Never non-finite:
    /// a curve that overflows (only a hand-built bezier can) is linear.
    pub fn apply(&self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        let k = self.curve().t_at(t, 0.0, 1.0);
        if k.is_finite() { k } else { t }
    }
}

/// A camera segment. Internally tagged on `type` (`#[serde(tag = "type")]`)
/// so a script reads as a plain list of `{ "type": "...", ... }` objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Segment {
    Jump {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        to: PoseSpec,
    },
    Hold {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        duration: f64,
        #[serde(default)]
        settle: bool,
    },
    Ease {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        to: PoseSpec,
        duration: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        easing: Option<Easing>,
    },
    Fly {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        to: PoseSpec,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        speed: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        screen_speed: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        curve: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        min_zoom: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_duration: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        easing: Option<Easing>,
    },
    Orbit {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        degrees: f64,
        duration: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        easing: Option<Easing>,
        #[serde(skip_serializing_if = "Option::is_none")]
        around: Option<(f64, f64)>,
    },
    Follow {
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The track to follow (spec §2.1/§3.3). Exactly one of `track`
        /// and `follow_draw` must be present (`follow_track_or_draw`);
        /// `None` in the type (rather than a required field) is what
        /// lets a `follow_draw` segment (spec §3, Task 4) omit it
        /// without a type error.
        #[serde(skip_serializing_if = "Option::is_none")]
        track: Option<String>,
        /// Follows a `draw` effect's head along an overlay line layer
        /// instead of a loaded track (spec §3: `"follow_draw":
        /// "route-line"`). The named layer must have a `draw` event
        /// (`follow_track_or_draw`); driving the camera from it is
        /// Task 4's job.
        #[serde(skip_serializing_if = "Option::is_none")]
        follow_draw: Option<String>,
        duration: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        offset_px: Option<(f32, f32)>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bearing: Option<FollowBearing>,
        #[serde(skip_serializing_if = "Option::is_none")]
        zoom: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pitch: Option<f64>,
    },
}

/// A `follow` segment's bearing: the track's own heading (JSON keyword
/// `"track"`, spec §2.1/§3.3, plan Task 10), or a fixed number of
/// degrees. Like [`Easing`], this parses from either a bare string or a
/// bare number, so it carries a hand-written `Serialize`/`Deserialize`.
#[derive(Debug, Clone, PartialEq)]
pub enum FollowBearing {
    Track,
    Fixed(f64),
}

impl Serialize for FollowBearing {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            FollowBearing::Track => serializer.serialize_str("track"),
            FollowBearing::Fixed(v) => serializer.serialize_f64(*v),
        }
    }
}

impl<'de> Deserialize<'de> for FollowBearing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value: Value = Deserialize::deserialize(deserializer)?;
        match value {
            Value::String(ref s) if s == "track" => Ok(FollowBearing::Track),
            Value::String(ref s) if s == "heading" => {
                Err(serde::de::Error::custom("bearing must be \"track\" (not \"heading\") or a number"))
            }
            Value::Number(_) => {
                let v = value.as_f64().ok_or_else(|| serde::de::Error::custom("bearing number must be finite"))?;
                Ok(FollowBearing::Fixed(v))
            }
            other => Err(serde::de::Error::custom(format!("bearing must be \"track\" or a number, got {other}"))),
        }
    }
}

/// When an event fires, or its `until`. Untagged: a bare number is
/// `At(seconds)`, and an object picks `After` or `AtStart` by which key it
/// carries. `offset` defaults to `0`, since the spec shows it in every
/// example but does not require it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum EventTime {
    At(f64),
    After {
        after: String,
        #[serde(default)]
        offset: f64,
    },
    AtStart {
        at_start: String,
        #[serde(default)]
        offset: f64,
    },
}

/// One timed action. `at`/`until` sit alongside the action's own fields
/// (`#[serde(flatten)]`), so `EventAction`'s externally-tagged key (e.g.
/// `"caption"`) appears as a sibling of `"at"` in the JSON object.
///
/// This struct cannot carry `#[serde(deny_unknown_fields)]` itself: serde
/// does not support combining `deny_unknown_fields` with `flatten` on the
/// same struct (the flatten field needs to catch "everything else", which
/// is exactly what `deny_unknown_fields` forbids). Unknown top-level event
/// keys, and an event with zero or more than one action key (`flatten`
/// silently takes the first match and drops the rest), are instead caught
/// by this file's own `Value` walk (`validate_events`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSpec {
    pub at: EventTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<EventTime>,
    #[serde(flatten)]
    pub action: EventAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EventAction {
    SetLayerVisibility { layer: String, visible: bool },
    SetFilter { layer: String, filter: serde_json::Value },
    SetPaintProperty { layer: String, name: String, value: serde_json::Value },
    SetLayoutProperty { layer: String, name: String, value: serde_json::Value },
    SetStyle { style: String },
    ShowOverlay { id: String },
    HideOverlay { id: String },
    SetOverlayData { id: String, geojson: String },
    SetBasemap { source: String },
    SetGlobe { on: bool },
    Tween { param: String, from: f64, to: f64, #[serde(skip_serializing_if = "Option::is_none")] easing: Option<Easing> },
    /// A style paint property eased from `from` to `to` over `[at, until)`:
    /// two numbers, or two colours. The timeline steps it as style edits
    /// (`timeline::PAINT_TWEEN_STEPS_PER_S` generations a second), so it
    /// animates anything a style can paint -- a water colour, a layer's
    /// opacity -- where `tween` drives only the cheap render params.
    TweenPaintProperty {
        layer: String,
        name: String,
        /// Where it starts; `None` is the layer's own value in the style
        /// in force at `at` (it must then be a plain number or colour).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<serde_json::Value>,
        to: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        easing: Option<Easing>,
    },
    SetParam { param: String, value: serde_json::Value },
    Caption(CaptionSpec),
    Settle { #[serde(skip_serializing_if = "Option::is_none")] timeout_s: Option<f64> },
    /// An overlay effect (spec §3): `draw`, `show`/`hide`/`fade`,
    /// `highlight`, `drop` or `pulse` on one declared layer.
    Overlay(OverlayEffectSpec),
    /// A headline/body/source card (spec §4.2).
    Panel(PanelSpec),
    /// A standalone image (spec §4.3).
    Image(ImageSpec),
    /// A leader-lined card pinned to a coordinate (spec §4.4).
    Callout(CalloutSpec),
    /// A design-system card (spec §10.1), over `[at, until]`. One card
    /// at a time: two card spans that overlap are the error
    /// `cards_overlap` ([`crate::story::cards::card_spans`], reported by
    /// validation once times are resolved).
    Card(CardSpec),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptionSpec {
    pub text: String,
    pub anchor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_px: Option<(f32, f32)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_px: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_s: Option<f64>,
}

// ---------------------------------------------------------------------
// Story cards (spec §3, §10.1)
// ---------------------------------------------------------------------

/// The design-system card (spec §3, §10.1): `events[].card` or
/// `chapters[].card`. Every field is optional; the design system
/// decides which supporting item (`quote`/`stats`/`legend`/`photo`)
/// wins when more than one is present (a later task's `layout_card`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<QuoteSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<Vec<StatSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legend: Option<Box<CardLegendSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub photo: Option<PhotoSpec>,
    /// `true` (auto-numbered on a tour) or `{"i", "n"}` (spec §3.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<IndexSpec>,
    /// `auto | off` (spec §3.3). Default: `auto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_s: Option<f64>,
}

/// [`CardSpec::quote`] (spec §3.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteSpec {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One of [`CardSpec::stats`] (spec §3.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatSpec {
    pub label: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// [`CardSpec::legend`] (spec §3.4): a ramp (`stops`/`ticks`/`current`)
/// or a list (`items`), optionally generated from overlay `layers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardLegendSpec {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stops: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<LegendItemSpec>>,
    /// Overlay layer ids (spec §2.2's one shared namespace) the legend
    /// is generated from, when `items` is not written by hand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaks: Option<Vec<f64>>,
    /// The feature property an `area` layer's fill reads (spec §6.3):
    /// a number for a sequential legend (default `"value"`), a class
    /// matched against each item's `label` for a categorical one
    /// (default `"class"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    /// `"sequential"` on a categorical legend: its items are ordered
    /// (decades, ranks), and each item without a colour takes the colour
    /// at its place along `stops` (the default ramp without them), first
    /// item at the first stop, last at the last (spec §6.3, ruling R9-b).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ramp: Option<String>,
}

/// One of [`CardLegendSpec::items`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegendItemSpec {
    /// `#rrggbb`; absent or blank takes the area palette's colour at the
    /// item's position (spec §6.3).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// [`CardSpec::photo`] (spec §3.4), distinct from [`ImageSpec`]: a
/// card's own photo is always required to carry `alt` and `credit` is
/// optional, matching the design system's supporting-item shape rather
/// than 1a's standalone `image` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhotoSpec {
    pub src: String,
    pub alt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit: Option<String>,
}

/// [`CardSpec::index`] (spec §3.3): `true` lets a tour auto-number
/// itself, `{"i", "n"}` names the count explicitly. Untagged: a bare
/// boolean is [`Self::Auto`], an object is [`Self::At`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IndexSpec {
    Auto(bool),
    At { i: u32, n: u32 },
}

// ---------------------------------------------------------------------
// Overlay effects, panels, images and callouts (spec §3, §4.2-§4.4)
// ---------------------------------------------------------------------

/// `overlay`'s `effect` (spec §3's table). Every variant's state is a
/// pure function of `t`: seeking equals playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectKind {
    Draw,
    Show,
    Hide,
    Fade,
    Highlight,
    Drop,
    Pulse,
}

/// `draw`'s optional head: a circle following the line's drawn tip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadSpec {
    pub radius_px: f32,
    pub color: String,
}

/// An `overlay` event (spec §3): `{ "layer", "effect", ... }`, with the
/// rest of the fields read only for the `effect` that uses them
/// (`draw`'s `head`, `highlight`'s `ids`/`where`/`color`/`dim_opacity`,
/// `drop`'s `stagger_s`/`time_property`/`ripple`, `pulse`'s
/// `period_s`/`max_radius_px`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayEffectSpec {
    pub layer: String,
    pub effect: EffectKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub easing: Option<Easing>,
    /// `draw`'s optional head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<HeadSpec>,
    /// `highlight`'s selection: explicit feature ids, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<serde_json::Value>>,
    /// ... or a filter expression (the JSON key is `where`, a Rust
    /// keyword, hence `where_`).
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub where_: Option<serde_json::Value>,
    /// `highlight`'s colour for the selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// `highlight`'s opacity for everything NOT selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim_opacity: Option<f64>,
    /// `drop`'s per-feature stagger, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stagger_s: Option<f64>,
    /// ... or a per-feature reveal time read from this property.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_property: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ripple: Option<bool>,
    /// `pulse`'s ring period.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_s: Option<f64>,
    /// `pulse`'s ring radius at the end of each period.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_radius_px: Option<f32>,
}

/// [`PanelSpec::placement`]/[`ImageSpec::placement`] (spec §4.1's
/// "Placement by intent").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    Title,
    LowerThird,
    Side,
    Corner,
}

/// A headline/body/source card (spec §4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelSpec {
    pub placement: Placement,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
    /// A named theme (`"dark"`, `"light"`, or `"accent:<hex>"`, spec
    /// §4.2); an `accent:` theme's hex must parse (`bad_color`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_s: Option<f64>,
}

/// `fit: cover | contain` (spec §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    Cover,
    Contain,
}

/// A standalone image (spec §4.3), also embeddable in a [`PanelSpec`] or
/// [`CalloutSpec`]. `alt` and `credit` are `Option` in the type so their
/// absence is a diagnostic elsewhere (a missing `alt` warns, Task 17; a
/// missing `credit` makes the render unpublishable, Task 19) rather than
/// a parse error here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSpec {
    /// A local path relative to the story, or an `https://` URL
    /// (`bad_path`: `http://` and an escaping/absolute/`~` local path
    /// are refused).
    pub src: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit: Option<Fit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_s: Option<f64>,
}

/// A small card with a leader line to a geographic coordinate (spec
/// §4.4), re-projected every frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalloutSpec {
    /// `(lon, lat)`: longitude within ±180, latitude within
    /// ±85.0511 (`bad_coord`, the Web Mercator limit this engine
    /// already enforces elsewhere).
    pub at_coord: (f64, f64),
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fade_s: Option<f64>,
}

/// The cheap parameters `tween`/`set_param` may name.
pub const TWEEN_PARAMS: &[&str] =
    &["raster_opacity", "dots.epoch_mix", "dots.ppd", "dots.radius_px", "sun.time", "atmosphere.on", "stars.on", "labels.on", "terminator.on"];

/// `(name, logical_width, logical_height, pixel_ratio, fps)`. `output()`
/// multiplies the logical size by `pixel_ratio` to get physical pixels.
pub const PRESETS: &[(&str, u32, u32, f32, u32)] = &[
    ("1080p30", 1920, 1080, 1.0, 30),
    ("1080p60", 1920, 1080, 1.0, 60),
    ("4k30", 1920, 1080, 2.0, 30),
    ("square1080", 1080, 1080, 1.0, 30),
    ("vertical1080", 1080, 1920, 1.0, 30),
    ("still4k", 1920, 1080, 2.0, 1),
];

const SEGMENT_KINDS: &[&str] = &["jump", "hold", "ease", "fly", "orbit", "follow"];

/// The union of every segment kind's fields, used as the known-field set
/// when a segment's `type` is missing or unrecognized -- so a typo or an
/// absent `type` is reported once, cleanly, instead of cascading into
/// `unknown_field` for every field that happens to belong to whichever
/// kind the author meant (finding 8).
const ALL_SEGMENT_FIELDS: &[&str] = &[
    "type", "label", "to", "duration", "settle", "easing", "speed", "screen_speed", "curve", "min_zoom", "max_duration", "degrees", "around",
    "track", "follow_draw", "offset_px", "bearing", "zoom", "pitch",
];

const ANCHOR_KEYWORDS: &[&str] = &["center", "left", "right", "top", "bottom", "top-left", "top-right", "bottom-left", "bottom-right"];

const EFFECT_KINDS: &[&str] = &["draw", "show", "hide", "fade", "highlight", "drop", "pulse"];
const PLACEMENT_KEYWORDS: &[&str] = &["title", "lower-third", "side", "corner"];
const FIT_KEYWORDS: &[&str] = &["cover", "contain"];
const OVERLAY_LAYER_KIND_KEYWORDS: &[&str] = &["line", "fill", "circle"];

/// Spec §11's `unknown_layer` message for a reference to an overlay
/// layer (`follow_draw`, a legend's `layers`, an event's `overlay.layer`)
/// that names no declared layer -- one template, shared by every such
/// site here and by `timeline.rs`'s own `effect_plan` (R10 review round
/// 1), so the wording cannot drift between them. `timeline.rs`'s OTHER
/// `unknown_layer` (a `set_filter`/`set_paint_property`/... naming no
/// STYLE layer) is a different reference entirely and keeps its own
/// message.
pub(crate) fn unknown_overlay_layer_message(id: &str) -> String {
    format!("There is no overlay layer named \"{id}\". Check the layer's id against the overlays you declared.")
}

/// `kind`'s own JSON `type` word (R10's `role_wrong_type`): the author's
/// text, never the Rust variant name.
fn kind_name(kind: OverlayLayerKind) -> &'static str {
    match kind {
        OverlayLayerKind::Line => "line",
        OverlayLayerKind::Fill => "fill",
        OverlayLayerKind::Circle => "circle",
    }
}

/// Every role the design system's theme understands (spec §6.1's table,
/// R10) and the [`OverlayLayerKind`] it needs: line roles, then point
/// roles, then `area`. THE one list: `theme_layers::role_kind`,
/// `infer_role` and `expand` all check against this -- never their own
/// copy -- so a role can never be added to one without the others
/// noticing (review round 1: a hand-written test list could not catch a
/// role `theme_layers.rs` added without touching `script.rs`, but a
/// shared table cannot drift at all). This is the one copy because
/// `script.rs` is one of the files `web/src/lib.rs` and `ios/src/lib.rs`
/// `#[path]`-include whole (S1), and `theme_layers.rs` is not among them
/// (it draws, which those shells do not do through this crate), so the
/// table has to live where both sides can reach it.
pub(crate) const ROLES: &[(&str, OverlayLayerKind)] = &[
    ("route", OverlayLayerKind::Line),
    ("tour", OverlayLayerKind::Line),
    ("arrow", OverlayLayerKind::Line),
    ("highlight", OverlayLayerKind::Line),
    ("context", OverlayLayerKind::Line),
    ("stops", OverlayLayerKind::Circle),
    ("numbered", OverlayLayerKind::Circle),
    ("event", OverlayLayerKind::Circle),
    ("area", OverlayLayerKind::Fill),
];

/// The [`OverlayLayerKind`] a `role` string needs, from [`ROLES`]: `None`
/// when `role` names none of the nine roles -- `unknown_role`.
pub(crate) fn role_kind(role: &str) -> Option<OverlayLayerKind> {
    ROLES.iter().find(|(r, _)| *r == role).map(|(_, k)| *k)
}

const ACTION_KEYS: &[&str] = &[
    "set_layer_visibility",
    "set_filter",
    "set_paint_property",
    "set_layout_property",
    "set_style",
    "show_overlay",
    "hide_overlay",
    "set_overlay_data",
    "set_basemap",
    "set_globe",
    "tween",
    "tween_paint_property",
    "set_param",
    "caption",
    "settle",
    "overlay",
    "panel",
    "image",
    "callout",
    "card",
];

const TOP_FIELDS: &[&str] = &[
    "story", "title", "output", "clock", "scene", "camera", "events", "summary", "lang", "chapters", "captions", "furniture", "design",
];

/// [`CardSpec`]'s field names, shared by an event's `card` and a
/// chapter's `card` ([`validate_card`]): one list, so a typo is
/// `unknown_field` at its own path whichever one it came from.
const CARD_FIELDS: &[&str] = &["headline", "body", "caption", "quote", "stats", "legend", "photo", "index", "progress", "fade_s"];

/// Whether `tag` looks like a BCP-47 language tag: a primary subtag of
/// 2-8 ASCII letters, then any number of `-` subtags of 1-8 ASCII letters
/// or digits (`ja`, `pt-BR`, `zh-Hant-TW`, `es-419`). Loose on purpose --
/// it does not consult the IANA registry -- but enough that the value is
/// safe and meaningful in `<html lang>`.
#[must_use]
pub fn is_bcp47_like(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let primary_ok = parts.next().is_some_and(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphabetic()));
    primary_ok && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

impl Script {
    /// The output resolution: the named preset (if any), with any explicit
    /// `output` fields overriding it field by field, in physical pixels.
    ///
    /// Once `aspect` or `tier` is set, this branches entirely away from
    /// `preset`/`width`/`height`/`pixel_ratio` (an `unknown_preset`-style
    /// mix is instead the parse-time error `size_and_tier`, so this
    /// never has to reconcile the two): the frame comes from
    /// [`tiers::frame_size`] and the pixel ratio from
    /// [`tiers::pixel_ratio`], with `fps` read from `fps`, else the
    /// named `preset`'s own fps, else 30.
    pub fn output(&self) -> ResolvedOutput {
        if self.output.aspect.is_some() || self.output.tier.is_some() {
            let aspect = self.output.aspect.unwrap_or(Aspect::Landscape);
            let tier = self.output.tier.unwrap_or(Tier::Fullhd);
            let (width, height) = frame_size(aspect, tier);
            let fps = self
                .output
                .fps
                .or_else(|| self.output.preset.as_deref().and_then(|name| PRESETS.iter().find(|(n, ..)| *n == name).map(|(_, _, _, _, f)| *f)))
                .unwrap_or(30);
            ResolvedOutput { width, height, fps, pixel_ratio: pixel_ratio(tier) }
        } else {
            resolve_output(self.output.preset.as_deref(), self.output.width, self.output.height, self.output.fps, self.output.pixel_ratio)
        }
    }

    /// The resolved [`OutputFormat`]: aspect, tier, entitlement and
    /// safe area, plus the frame in 1080-referenced layout units. For a
    /// script with neither `aspect` nor `tier` -- every preset or
    /// `width`/`height` script written before this task -- the aspect
    /// is the closest of the four by ratio to the resolved physical
    /// size, and the tier is whichever one's short edge equals the
    /// resolved physical short edge exactly, else `fullhd`: an old
    /// `4k30` preset is still reported as 4K/pro, not fullhd/free.
    pub fn format(&self) -> OutputFormat {
        let resolved = self.output();
        let (aspect, tier) = if self.output.aspect.is_some() || self.output.tier.is_some() {
            (self.output.aspect.unwrap_or(Aspect::Landscape), self.output.tier.unwrap_or(Tier::Fullhd))
        } else {
            let short_edge = resolved.width.min(resolved.height);
            let tier = [Tier::Hd, Tier::Fullhd, Tier::Qhd, Tier::Uhd].into_iter().find(|t| t.short_edge() == short_edge).unwrap_or(Tier::Fullhd);
            (aspect_from_ratio(resolved.width as f32, resolved.height as f32), tier)
        };
        let safe_area = self.output.safe_area.unwrap_or_else(|| default_safe_area(aspect));
        OutputFormat {
            aspect,
            tier,
            entitlement: tier.entitlement(),
            safe_area,
            layout_size: (resolved.width as f32 / resolved.pixel_ratio, resolved.height as f32 / resolved.pixel_ratio),
        }
    }

    /// The label of the `i`th camera segment, if it has one.
    pub fn segment_label(&self, i: usize) -> Option<&str> {
        self.camera.get(i).and_then(segment_label_of)
    }
}

/// The [`Aspect`] whose ratio is closest to `w / h`, used by
/// [`Script::format`] when a script sets neither `aspect` nor `tier`.
fn aspect_from_ratio(w: f32, h: f32) -> Aspect {
    let ratio = w / h;
    let mut best = Aspect::Landscape;
    let mut best_diff = f32::INFINITY;
    for (a, target) in [(Aspect::Landscape, 16.0 / 9.0), (Aspect::Portrait, 9.0 / 16.0), (Aspect::Feed, 4.0 / 5.0), (Aspect::Square, 1.0)] {
        let diff = (ratio - target).abs();
        if diff < best_diff {
            best_diff = diff;
            best = a;
        }
    }
    best
}

fn segment_label_of(segment: &Segment) -> Option<&str> {
    match segment {
        Segment::Jump { label, .. }
        | Segment::Hold { label, .. }
        | Segment::Ease { label, .. }
        | Segment::Fly { label, .. }
        | Segment::Orbit { label, .. }
        | Segment::Follow { label, .. } => label.as_deref(),
    }
}

fn resolve_output(preset: Option<&str>, width: Option<u32>, height: Option<u32>, fps: Option<u32>, pixel_ratio: Option<f32>) -> ResolvedOutput {
    let (mut logical_w, mut logical_h, mut ratio, mut fps_val) = (1920u32, 1080u32, 1.0f32, 30u32);
    if let Some(name) = preset {
        if let Some((_, w, h, r, f)) = PRESETS.iter().find(|(n, ..)| *n == name) {
            logical_w = *w;
            logical_h = *h;
            ratio = *r;
            fps_val = *f;
        }
    }
    if let Some(w) = width {
        logical_w = w;
    }
    if let Some(h) = height {
        logical_h = h;
    }
    if let Some(f) = fps {
        fps_val = f;
    }
    if let Some(r) = pixel_ratio {
        ratio = r;
    }
    ResolvedOutput {
        width: ((logical_w as f32) * ratio).round() as u32,
        height: ((logical_h as f32) * ratio).round() as u32,
        fps: fps_val,
        pixel_ratio: ratio,
    }
}

// ---------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------

/// Parses and structurally validates a story script, discarding any
/// warnings. See [`parse_with_warnings`] to keep them.
///
/// On success, every structural check passed, with no `Severity::Error`
/// diagnostic (there may still be warnings). On failure, every diagnostic
/// found is returned, not just the first, and at least one is
/// `Severity::Error` (this is an invariant of [`parse_inner`], checked by
/// `err_always_has_at_least_one_error`).
pub fn parse(bytes: &[u8]) -> Result<Script, Vec<Diagnostic>> {
    parse_inner(bytes).map(|(script, _warnings)| script)
}

/// Like [`parse`], but keeps whatever `Severity::Warning`/`Info`
/// diagnostics were found even on success (e.g. a longitude outside
/// ±180 that got wrapped rather than rejected). Shares [`parse_inner`]
/// with `parse`, so the two can never disagree about whether a script is
/// valid.
pub fn parse_with_warnings(bytes: &[u8]) -> Result<(Script, Vec<Diagnostic>), Vec<Diagnostic>> {
    parse_inner(bytes)
}

fn parse_inner(bytes: &[u8]) -> Result<(Script, Vec<Diagnostic>), Vec<Diagnostic>> {
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(err) => return Err(vec![Diagnostic::error("", "invalid_json", format!("invalid JSON: {err}"))]),
    };

    let mut diags = Vec::new();
    validate_structure(&value, &mut diags);
    let has_error = |diags: &[Diagnostic]| diags.iter().any(|d| d.severity == Severity::Error);

    match serde_json::from_value::<Script>(value) {
        Ok(script) => {
            if has_error(&diags) {
                Err(diags)
            } else {
                Ok((script, diags))
            }
        }
        Err(err) => {
            if !has_error(&diags) {
                diags.push(Diagnostic::error("", "type_mismatch", format!("{err}")));
            }
            Err(diags)
        }
    }
}

fn validate_structure(value: &Value, diags: &mut Vec<Diagnostic>) {
    let Some(obj) = value.as_object() else {
        diags.push(Diagnostic::error("", "type_mismatch", "the story document must be a JSON object"));
        return;
    };
    check_known_fields(obj, TOP_FIELDS, "", diags);
    check_optional_type(obj, "title", "", FieldKind::String, diags);
    check_optional_type(obj, "summary", "", FieldKind::String, diags);
    check_optional_type(obj, "lang", "", FieldKind::String, diags);
    if let Some(lang) = obj.get("lang").and_then(Value::as_str) {
        if !is_bcp47_like(lang) {
            diags.push(
                Diagnostic::error("/lang", "bad_lang", format!("`{lang}` is not a BCP-47 language tag"))
                    .with_hint(Some("write a tag like \"en\", \"ja\" or \"pt-BR\"".to_string())),
            );
        }
    }

    // `story` must be a whole-number integer: `serde_json::Value::as_u64`
    // only succeeds for a number written WITHOUT a decimal point, so
    // `1.0` (which the range check below would otherwise accept, since it
    // compares equal to 1.0 as a float) is correctly reported here --
    // serde's typed pass rejects it too, once it reaches a `u32` field.
    match obj.get("story") {
        None | Some(Value::Null) => {
            diags.push(Diagnostic::error("/story", "missing_field", "`story` is required (an integer)"));
        }
        Some(v) => match v.as_u64() {
            Some(story) => {
                if story != 1 {
                    diags.push(Diagnostic::error("/story", "unsupported_version", format!("story version {story} is not supported; only version 1 is understood")));
                }
            }
            None => {
                diags.push(Diagnostic::error("/story", "type_mismatch", format!("`story` must be an integer, got {v}")));
            }
        },
    }

    if let Some(output) = obj.get("output").filter(|v| !v.is_null()) {
        validate_output(output, diags);
    }
    if let Some(clock) = obj.get("clock").filter(|v| !v.is_null()) {
        validate_clock(clock, diags);
    }
    if let Some(design) = obj.get("design").filter(|v| !v.is_null()) {
        validate_design(design, diags);
    }

    let (overlay_ids, track_ids, overlay_layers) = match require_object(obj, "scene", "", diags) {
        Some(scene_obj) => validate_scene(scene_obj, diags),
        None => Default::default(),
    };
    let overlay_layer_ids: Vec<String> = overlay_layers.iter().map(|l| l.id.clone()).collect();
    // A lightweight pre-scan, independent of `validate_camera`'s own
    // signature (it takes `events` too now, only for this):
    // `follow_track_or_draw` needs to know which overlay layers have a
    // `draw` event before `camera` is walked.
    let draw_layer_ids = collect_draw_layers(obj.get("events"));

    // `camera` failing to parse as an array must not be conflated with a
    // validly-parsed, label-free camera: `labels` is `None` here so that
    // event label references are skipped entirely rather than reported as
    // `unknown_label` for every one of them (finding N3).
    let labels: Option<Vec<String>> = require_array(obj, "camera", "", diags)
        .map(|camera_arr| validate_camera(camera_arr, diags, &track_ids, &overlay_layer_ids, &draw_layer_ids).0);

    if let Some(chapters) = obj.get("chapters").filter(|v| !v.is_null()) {
        validate_chapters(chapters, diags, labels.as_deref(), &overlay_layers);
    }
    if let Some(captions) = obj.get("captions").filter(|v| !v.is_null()) {
        validate_captions(captions, diags);
    }
    if let Some(furniture) = obj.get("furniture").filter(|v| !v.is_null()) {
        validate_furniture(furniture, "/furniture", diags, &overlay_layers);
    }

    if let Some(events) = obj.get("events").filter(|v| !v.is_null()) {
        validate_events(events, diags, labels.as_deref(), &overlay_ids, &overlay_layers);
        warn_sun_time_without_terminator(obj, events, diags);
    }
}

/// `sun.time` moves the sun, but only a frame with the terminator on is
/// lit by it: warn at each `sun.time` event when neither the scene nor
/// any `terminator.on` event ever turns it on.
fn warn_sun_time_without_terminator(obj: &serde_json::Map<String, Value>, events: &Value, diags: &mut Vec<Diagnostic>) {
    let Some(events) = events.as_array() else { return };
    let param_of = |e: &Value| -> Option<String> {
        ["set_param", "tween"].iter().find_map(|k| e.get(*k)?.get("param")?.as_str().map(str::to_string))
    };
    let scene_on = obj.get("scene").and_then(|s| s.get("terminator")).and_then(Value::as_bool) == Some(true);
    let event_on = events.iter().any(|e| param_of(e).as_deref() == Some("terminator.on"));
    if scene_on || event_on {
        return;
    }
    for (i, e) in events.iter().enumerate() {
        if param_of(e).as_deref() == Some("sun.time") {
            diags.push(Diagnostic::warning(
                &format!("/events/{i}"),
                "sun_time_without_terminator",
                "sun.time has no visible effect without scene.terminator (or a terminator.on event)",
            ));
        }
    }
}

/// RFC 6901 escaping for a JSON-pointer token: `~` before `/`, so `~`
/// becomes `~0` and `/` becomes `~1` (never the other way around).
fn escape_pointer_token(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn check_known_fields(obj: &serde_json::Map<String, Value>, known: &[&str], path: &str, diags: &mut Vec<Diagnostic>) {
    for key in obj.keys() {
        if !known.contains(&key.as_str()) {
            let hint = hint_for_unknown(key, known);
            diags.push(Diagnostic::error(&format!("{path}/{}", escape_pointer_token(key)), "unknown_field", format!("unknown field `{key}`")).with_hint(hint));
        }
    }
}

/// Checks that `obj[field]` exists, is not JSON `null`, and satisfies
/// `ok`. Reports `missing_field` (absent or null) or `type_mismatch`
/// (present, wrong shape) at the exact pointer, and returns the value
/// only when it passed.
fn check_required<'a>(
    obj: &'a serde_json::Map<String, Value>,
    field: &str,
    path: &str,
    diags: &mut Vec<Diagnostic>,
    kind_name: &str,
    ok: impl Fn(&Value) -> bool,
) -> Option<&'a Value> {
    match obj.get(field) {
        None => {
            diags.push(Diagnostic::error(&format!("{path}/{field}"), "missing_field", format!("`{field}` is required ({kind_name})")));
            None
        }
        Some(v) if v.is_null() => {
            diags.push(Diagnostic::error(&format!("{path}/{field}"), "missing_field", format!("`{field}` is required ({kind_name})")));
            None
        }
        Some(v) if !ok(v) => {
            diags.push(Diagnostic::error(&format!("{path}/{field}"), "type_mismatch", format!("`{field}` must be {kind_name}, got {v}")));
            None
        }
        Some(v) => Some(v),
    }
}

/// The JSON shape an OPTIONAL field is expected to have, for
/// [`check_optional_type`]. Required fields go through [`check_required`]
/// instead, which also handles absence.
#[derive(Clone, Copy)]
enum FieldKind {
    Number,
    Bool,
    String,
    /// A 2-element array of numbers, e.g. `offset_px`/`around`.
    Tuple2,
}

impl FieldKind {
    fn matches(self, v: &Value) -> bool {
        match self {
            FieldKind::Number => v.as_f64().is_some(),
            FieldKind::Bool => v.as_bool().is_some(),
            FieldKind::String => v.as_str().is_some(),
            FieldKind::Tuple2 => v.as_array().is_some_and(|a| a.len() == 2 && a.iter().all(|x| x.as_f64().is_some())),
        }
    }

    fn name(self) -> &'static str {
        match self {
            FieldKind::Number => "a number",
            FieldKind::Bool => "a boolean",
            FieldKind::String => "a string",
            FieldKind::Tuple2 => "a 2-element array of numbers",
        }
    }
}

/// Checks an OPTIONAL field's type when it is present and non-null. This
/// runs independent of every other diagnostic (it never checks whether
/// `diags` is already non-empty), which is what keeps a wrong-typed
/// optional field -- `hold.settle: "yes"` alongside an unrelated
/// `zoom: 25` -- from being silently dropped behind the other error.
fn check_optional_type(obj: &serde_json::Map<String, Value>, field: &str, path: &str, kind: FieldKind, diags: &mut Vec<Diagnostic>) {
    let Some(v) = obj.get(field).filter(|v| !v.is_null()) else { return };
    if !kind.matches(v) {
        diags.push(Diagnostic::error(&format!("{path}/{field}"), "type_mismatch", format!("`{field}` must be {}, got {v}", kind.name())));
    }
}

/// `n` fits a `u32` (a whole number, `0..=u32::MAX`): [`CardSpec::index`]'s
/// `{"i", "n"}` deserialises straight into `u32` fields, so a value this
/// rejects would otherwise surface as an opaque serde error instead of a
/// pathful one (Task 2 minor, R10).
fn check_fits_u32(n: f64, field: &str, path: &str, diags: &mut Vec<Diagnostic>) {
    if n.fract() != 0.0 || n < 0.0 || n > f64::from(u32::MAX) {
        diags.push(Diagnostic::error(
            &format!("{path}/{field}"),
            "type_mismatch",
            format!("`{field}` must be a whole number from 0 to {}, got {n}. Use a smaller index.", u32::MAX),
        ));
    }
}

fn require_number(obj: &serde_json::Map<String, Value>, field: &str, path: &str, diags: &mut Vec<Diagnostic>) -> Option<f64> {
    check_required(obj, field, path, diags, "a number", |v| v.as_f64().is_some()).and_then(Value::as_f64)
}

/// Whether `url` points at Mapbox: a `mapbox://` URL, or an `http(s)`
/// URL whose host is `api.mapbox.com` or any `*.mapbox.com` (the
/// `a.tiles.mapbox.com` family). A `pmtiles:` prefix is looked through.
/// The HOST decides, never a prefix of the text: a `tiles` URL of
/// `https://api.mapbox.com/v4/...` is Mapbox whatever it is labelled.
#[must_use]
pub fn is_mapbox_url(url: &str) -> bool {
    let url = url.trim();
    let url = url.strip_prefix("pmtiles:").unwrap_or(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if scheme.eq_ignore_ascii_case("mapbox") {
        return true;
    }
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_port.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
    host == "mapbox.com" || host.ends_with(".mapbox.com")
}

fn require_string<'a>(obj: &'a serde_json::Map<String, Value>, field: &str, path: &str, diags: &mut Vec<Diagnostic>) -> Option<&'a str> {
    check_required(obj, field, path, diags, "a string", |v| v.as_str().is_some()).and_then(Value::as_str)
}

fn require_object<'a>(
    obj: &'a serde_json::Map<String, Value>,
    field: &str,
    path: &str,
    diags: &mut Vec<Diagnostic>,
) -> Option<&'a serde_json::Map<String, Value>> {
    check_required(obj, field, path, diags, "an object", |v| v.as_object().is_some()).and_then(Value::as_object)
}

fn require_array<'a>(obj: &'a serde_json::Map<String, Value>, field: &str, path: &str, diags: &mut Vec<Diagnostic>) -> Option<&'a Vec<Value>> {
    check_required(obj, field, path, diags, "an array", |v| v.as_array().is_some()).and_then(Value::as_array)
}

/// Reads a JSON field as `u32`, reporting `too_large` (never silently
/// truncating) when the number does not fit. Absent or wrong-typed is
/// `None`, left to the field's own optionality.
fn read_u32(obj: &serde_json::Map<String, Value>, field: &str, path: &str, diags: &mut Vec<Diagnostic>) -> Option<u32> {
    let v = obj.get(field).filter(|v| !v.is_null())?;
    let Some(raw) = v.as_u64() else {
        diags.push(Diagnostic::error(&format!("{path}/{field}"), "type_mismatch", format!("`{field}` must be a non-negative integer, got {v}")));
        return None;
    };
    match u32::try_from(raw) {
        Ok(v) => Some(v),
        Err(_) => {
            diags.push(Diagnostic::error(&format!("{path}/{field}"), "too_large", format!("{raw} exceeds the maximum representable value")));
            None
        }
    }
}

/// The known string values for `/output/aspect`, `/output/tier`,
/// `/output/safe_area` and `/output/codec`, in the order shown in
/// `unknown_value`'s message (matching each type's serde spelling).
const ASPECT_NAMES: &[&str] = &["16:9", "9:16", "4:5", "1:1"];
const TIER_NAMES: &[&str] = &["hd", "fullhd", "qhd", "4k"];
const SAFE_AREA_NAMES: &[&str] = &["tiktok", "reels", "shorts", "youtube", "none", "all"];
const CODEC_NAMES: &[&str] = &["h264", "hevc"];

/// Checks an optional string field's value against a known set,
/// reporting `unknown_value` with a `hint_for_unknown` hint. Silent
/// when the field is absent or already failed its type check (not a
/// string): [`check_optional_type`] already reported that case.
fn validate_enum_field(obj: &serde_json::Map<String, Value>, field: &str, path: &str, known: &[&str], diags: &mut Vec<Diagnostic>) {
    let Some(v) = obj.get(field).and_then(Value::as_str) else { return };
    if !known.contains(&v) {
        let hint = hint_for_unknown(v, known);
        diags.push(Diagnostic::error(&format!("{path}/{field}"), "unknown_value", format!("unknown {field} `{v}`; known values: {known:?}")).with_hint(hint));
    }
}

fn aspect_from_name(s: &str) -> Option<Aspect> {
    match s {
        "16:9" => Some(Aspect::Landscape),
        "9:16" => Some(Aspect::Portrait),
        "4:5" => Some(Aspect::Feed),
        "1:1" => Some(Aspect::Square),
        _ => None,
    }
}

fn tier_from_name(s: &str) -> Option<Tier> {
    match s {
        "hd" => Some(Tier::Hd),
        "fullhd" => Some(Tier::Fullhd),
        "qhd" => Some(Tier::Qhd),
        "4k" => Some(Tier::Uhd),
        _ => None,
    }
}

fn validate_output(output: &Value, diags: &mut Vec<Diagnostic>) {
    let Some(obj) = output.as_object() else { return };
    check_known_fields(
        obj,
        &[
            "preset", "width", "height", "fps", "pixel_ratio", "strict", "acknowledge", "aspect", "tier", "safe_area", "codec", "burn_captions",
            "poster_at",
        ],
        "/output",
        diags,
    );
    check_optional_type(obj, "preset", "/output", FieldKind::String, diags);
    if let Some(acks) = obj.get("acknowledge").filter(|v| !v.is_null()) {
        let ok = acks.as_array().is_some_and(|a| {
            a.iter().all(|v| {
                v.as_str().is_some_and(|s| s == "mapbox-video" || s.strip_prefix("source-licence:").is_some_and(|rest| !rest.trim().is_empty()))
            })
        });
        if !ok {
            diags.push(Diagnostic::error(
                "/output/acknowledge",
                "bad_acknowledge",
                "`acknowledge` is an array of \"mapbox-video\" or \"source-licence:<source id or host>\" strings",
            ));
        }
    }
    check_optional_type(obj, "pixel_ratio", "/output", FieldKind::Number, diags);
    check_optional_type(obj, "strict", "/output", FieldKind::Bool, diags);
    check_optional_type(obj, "aspect", "/output", FieldKind::String, diags);
    check_optional_type(obj, "tier", "/output", FieldKind::String, diags);
    check_optional_type(obj, "safe_area", "/output", FieldKind::String, diags);
    check_optional_type(obj, "codec", "/output", FieldKind::String, diags);
    check_optional_type(obj, "burn_captions", "/output", FieldKind::Bool, diags);
    check_optional_type(obj, "poster_at", "/output", FieldKind::Number, diags);
    // The upper bound needs the duration: `Timeline::new` (`poster_outside_story`).
    if let Some(at) = obj.get("poster_at").and_then(Value::as_f64) {
        if !at.is_finite() || at < 0.0 {
            diags.push(Diagnostic::error(
                "/output/poster_at",
                "poster_outside_story",
                format!("poster_at {at} must be a story second within [0, duration]"),
            ));
        }
    }
    validate_enum_field(obj, "aspect", "/output", ASPECT_NAMES, diags);
    validate_enum_field(obj, "tier", "/output", TIER_NAMES, diags);
    validate_enum_field(obj, "safe_area", "/output", SAFE_AREA_NAMES, diags);
    validate_enum_field(obj, "codec", "/output", CODEC_NAMES, diags);

    let preset = obj.get("preset").and_then(Value::as_str);
    if let Some(name) = preset {
        if !PRESETS.iter().any(|(n, ..)| *n == name) {
            let names: Vec<&str> = PRESETS.iter().map(|(n, ..)| *n).collect();
            let hint = hint_for_unknown(name, &names);
            diags.push(Diagnostic::error("/output/preset", "unknown_preset", format!("unknown preset `{name}`; known presets: {names:?}")).with_hint(hint));
        }
    }

    let width = read_u32(obj, "width", "/output", diags);
    let height = read_u32(obj, "height", "/output", diags);
    let fps = read_u32(obj, "fps", "/output", diags);
    let pixel_ratio_field = obj.get("pixel_ratio").and_then(Value::as_f64).map(|r| r as f32);

    if let Some(r) = pixel_ratio_field {
        let allowed = [1.0f32, 1.5, 2.0, 3.0, 4.0];
        if !allowed.iter().any(|a| (a - r).abs() < 1e-6) {
            diags.push(
                Diagnostic::error("/output/pixel_ratio", "bad_pixel_ratio", format!("pixel_ratio {r} must be one of {allowed:?}"))
                    .with_hint(Some(format!("use one of {allowed:?}"))),
            );
        }
    }
    if let Some(f) = fps {
        if !(1..=120).contains(&f) {
            diags.push(Diagnostic::error("/output/fps", "fps_out_of_range", format!("fps {f} must be within [1, 120]")));
        }
    }

    // `aspect`/`tier` pick the frame from the tier table (spec §2.4);
    // combining either with an explicit size or an old `preset` is
    // ambiguous about which one wins, so it is the error `size_and_tier`
    // rather than a silently-applied precedence rule.
    let has_aspect_or_tier = obj.get("aspect").is_some_and(|v| !v.is_null()) || obj.get("tier").is_some_and(|v| !v.is_null());
    let has_size_fields = width.is_some() || height.is_some() || pixel_ratio_field.is_some() || preset.is_some();
    if has_aspect_or_tier && has_size_fields {
        diags.push(Diagnostic::error(
            "/output",
            "size_and_tier",
            "`aspect`/`tier` cannot be combined with `width`, `height`, `pixel_ratio` or `preset`; they resolve the frame size on their own",
        ));
    }

    if has_aspect_or_tier {
        // The table's sizes are always valid today, but this still runs
        // `check_dimension` over them (rather than assuming so) in case
        // the table ever grows a tier or aspect that isn't.
        let aspect = obj.get("aspect").and_then(Value::as_str).and_then(aspect_from_name).unwrap_or(Aspect::Landscape);
        let tier = obj.get("tier").and_then(Value::as_str).and_then(tier_from_name).unwrap_or(Tier::Fullhd);
        let (w, h) = frame_size(aspect, tier);
        check_dimension(w, "/output/width", diags);
        check_dimension(h, "/output/height", diags);
    } else {
        let resolved = resolve_output(preset, width, height, fps, pixel_ratio_field);
        check_dimension(resolved.width, "/output/width", diags);
        check_dimension(resolved.height, "/output/height", diags);
    }
}

fn check_dimension(physical: u32, path: &str, diags: &mut Vec<Diagnostic>) {
    if physical < 16 {
        diags.push(Diagnostic::error(path, "dimension_too_small", format!("{physical} is below the 16 physical-pixel minimum")));
    } else if !physical.is_multiple_of(2) {
        diags.push(Diagnostic::error(path, "odd_dimension", format!("{physical} must be even (required by yuv420p encoding)")));
    } else if physical > 8192 {
        diags.push(Diagnostic::error(path, "too_large", format!("{physical} exceeds the 8192 physical-pixel limit")));
    }
}

/// [`Script::design`] (spec §10.1): every field is an optional string
/// or boolean here -- which value names a real archetype, theme or
/// card tone is a later task's concern (`design.rs`'s `schema`), so an
/// unrecognised value is not rejected at this layer.
fn validate_design(design: &Value, diags: &mut Vec<Diagnostic>) {
    let Some(obj) = design.as_object() else {
        diags.push(Diagnostic::error("/design", "type_mismatch", "`design` must be an object. Write it as {\"archetype\": \"…\"} or just {}."));
        return;
    };
    check_known_fields(obj, &["archetype", "theme", "accent", "card", "dir", "anchor", "credits", "frame_camera"], "/design", diags);
    for field in ["archetype", "theme", "accent", "card", "dir", "anchor", "credits"] {
        check_optional_type(obj, field, "/design", FieldKind::String, diags);
    }
    check_optional_type(obj, "frame_camera", "/design", FieldKind::Bool, diags);
    if let Some(accent) = obj.get("accent").and_then(Value::as_str) {
        check_bad_color(accent, "/design/accent", diags);
    }
    // Spec §11's `unknown_theme`: an unrecognised `design.theme` would
    // otherwise fall silently back to the archetype's default
    // (`design.rs`'s `theme_named`), which hides a typo from the author.
    if let Some(theme) = obj.get("theme").and_then(Value::as_str) {
        if crate::story::design::theme_named(theme).is_none() {
            diags.push(Diagnostic::error(
                "/design/theme",
                "unknown_theme",
                format!("There is no theme called \"{theme}\". Use ember, ink, ocean or fill."),
            ));
        }
    }
}

fn validate_clock(clock: &Value, diags: &mut Vec<Diagnostic>) {
    let Some(obj) = clock.as_object() else { return };
    check_known_fields(obj, &["start_utc", "rate", "utc_offset_min"], "/clock", diags);
    check_optional_type(obj, "start_utc", "/clock", FieldKind::String, diags);
    check_optional_type(obj, "rate", "/clock", FieldKind::Number, diags);
    check_optional_type(obj, "utc_offset_min", "/clock", FieldKind::Number, diags);
    if let Some(s) = obj.get("start_utc").and_then(Value::as_str) {
        if parse_utc(s).is_none() {
            let hint = if ends_with_utc_offset(s) {
                "write the instant in UTC with `Z`; set local time with `clock.utc_offset_min`".to_string()
            } else {
                "format: YYYY-MM-DDTHH:MM:SS(.fff)?Z, e.g. \"2026-03-20T10:00:00Z\"".to_string()
            };
            diags.push(
                Diagnostic::error("/clock/start_utc", "bad_utc", format!("`{s}` is not a valid RFC 3339 UTC timestamp like \"2026-03-20T10:00:00Z\""))
                    .with_hint(Some(hint)),
            );
        }
    }
}

/// Whether `s` ends in a `±HH:MM` UTC offset (valid RFC 3339, but this
/// format wants a bare `Z` with the offset carried separately in
/// `clock.utc_offset_min`).
fn ends_with_utc_offset(s: &str) -> bool {
    if s.len() < 6 {
        return false;
    }
    let Some(tail) = s.get(s.len() - 6..) else { return false };
    let mut chars = tail.chars();
    let sign = chars.next();
    let d1 = chars.next();
    let d2 = chars.next();
    let colon = chars.next();
    let d3 = chars.next();
    let d4 = chars.next();
    matches!(sign, Some('+') | Some('-'))
        && d1.is_some_and(|c| c.is_ascii_digit())
        && d2.is_some_and(|c| c.is_ascii_digit())
        && colon == Some(':')
        && d3.is_some_and(|c| c.is_ascii_digit())
        && d4.is_some_and(|c| c.is_ascii_digit())
}

/// One `scene.overlays[*].layers[*]` entry (spec §2.2), as seen while
/// walking the `Value` tree: its kind (`None` when `type` was missing or
/// unrecognized -- `effect_layer_type` then stays silent rather than
/// piling a second diagnostic on top of `unknown_field`/`unknown_value`)
/// and whether its owning overlay set `line_metrics: true`
/// (`draw_needs_metrics`). Layer ids share one namespace across every
/// overlay (`duplicate_id`).
struct DeclaredLayer {
    id: String,
    kind: Option<&'static str>,
    line_metrics: bool,
}

/// `bad_path`: a path string (overlay `data`, image `src` when not an
/// `https://` URL) that is absolute, contains a `..` component, or
/// starts with `~`. When `allow_https` is set (image `src`, spec §4.3),
/// `https://` is accepted outright and a bare `http://` is `bad_path`
/// too ("https only"); an overlay's `data`/`geojson` (spec §2.2) takes
/// `allow_https: false`, since it names only a relative path.
fn check_bad_path(value: &str, path: &str, allow_https: bool, diags: &mut Vec<Diagnostic>) {
    if allow_https {
        if value.starts_with("https://") {
            return;
        }
        if value.starts_with("http://") {
            diags.push(Diagnostic::error(path, "bad_path", format!("`{value}` must be `https://`, not `http://`")));
            return;
        }
    }
    let reason = if value.starts_with('/') {
        Some("must be relative, not absolute")
    } else if value.starts_with('~') {
        Some("must not start with `~`")
    } else if value.split('/').any(|segment| segment == "..") {
        Some("must not contain a `..` component (it would escape the story's folder)")
    } else {
        None
    };
    if let Some(reason) = reason {
        diags.push(Diagnostic::error(path, "bad_path", format!("`{value}` is not a valid relative path: {reason}")));
    }
}

/// `bad_color`: a value that must parse with `crate::style_spec::color`
/// (`head.color`, `overlay`'s highlight `color`, and a theme's
/// `accent:<hex>` tail).
fn check_bad_color(s: &str, path: &str, diags: &mut Vec<Diagnostic>) {
    if crate::style_spec::parse_css_color(s).is_err() {
        diags.push(Diagnostic::error(path, "bad_color", format!("`{s}` is not a valid colour")));
    }
}

/// A panel/callout `theme` (spec §4.2): a bare name, or `accent:<hex>`,
/// whose hex must parse (`bad_color`).
fn check_theme(theme: &str, path: &str, diags: &mut Vec<Diagnostic>) {
    if let Some(hex) = theme.strip_prefix("accent:") {
        check_bad_color(hex, path, diags);
    }
}

/// An [`ImageSpec`] (spec §4.3), shared by the `image` event, a panel's
/// `image` and a callout's `image`. `alt`/`credit` are checked only for
/// type (their absence is a later diagnostic, not a parse error here --
/// see [`ImageSpec`]'s doc comment).
fn validate_image_spec(value: &Value, path: &str, diags: &mut Vec<Diagnostic>) {
    let Some(obj) = value.as_object() else { return };
    check_known_fields(obj, &["src", "alt", "credit", "placement", "fit", "fade_s"], path, diags);
    check_optional_type(obj, "alt", path, FieldKind::String, diags);
    check_optional_type(obj, "credit", path, FieldKind::String, diags);
    check_optional_type(obj, "fade_s", path, FieldKind::Number, diags);
    check_optional_type(obj, "placement", path, FieldKind::String, diags);
    check_optional_type(obj, "fit", path, FieldKind::String, diags);
    if let Some(src) = require_string(obj, "src", path, diags) {
        check_bad_path(src, &format!("{path}/src"), true, diags);
    }
    validate_enum_field(obj, "placement", path, PLACEMENT_KEYWORDS, diags);
    validate_enum_field(obj, "fit", path, FIT_KEYWORDS, diags);
}

/// A [`CardSpec`] (spec §3, §10.1), shared by an event's `card` and a
/// chapter's `card`: walked exactly once, so a typo in either is
/// `unknown_field` at its own path (not at the whole `card` object, the
/// way an unrecognised event action is). Which supporting item wins
/// when more than one is present, and whether `index` needs a tour, are
/// layout/semantic questions for a later task; this only checks shape.
fn validate_card(value: &Value, path: &str, diags: &mut Vec<Diagnostic>, overlay_layers: &[DeclaredLayer]) {
    let Some(obj) = value.as_object() else {
        diags.push(Diagnostic::error(path, "type_mismatch", "A card must be an object. Write it as {\"headline\": \"…\"}."));
        return;
    };
    check_known_fields(obj, CARD_FIELDS, path, diags);
    check_optional_type(obj, "headline", path, FieldKind::String, diags);
    check_optional_type(obj, "body", path, FieldKind::String, diags);
    check_optional_type(obj, "caption", path, FieldKind::String, diags);
    check_optional_type(obj, "progress", path, FieldKind::String, diags);
    check_optional_type(obj, "fade_s", path, FieldKind::Number, diags);
    validate_enum_field(obj, "progress", path, &["auto", "off"], diags);

    if let Some(quote) = obj.get("quote").filter(|v| !v.is_null()) {
        let quote_path = format!("{path}/quote");
        if let Some(quote_obj) = quote.as_object() {
            check_known_fields(quote_obj, &["text", "by", "source"], &quote_path, diags);
            require_string(quote_obj, "text", &quote_path, diags);
            check_optional_type(quote_obj, "by", &quote_path, FieldKind::String, diags);
            check_optional_type(quote_obj, "source", &quote_path, FieldKind::String, diags);
        } else {
            diags.push(Diagnostic::error(&quote_path, "type_mismatch", "A quote must be an object with a text. Write it as {\"text\": \"…\"}."));
        }
    }

    if let Some(stats) = obj.get("stats").filter(|v| !v.is_null()) {
        let stats_path = format!("{path}/stats");
        if let Some(arr) = stats.as_array() {
            for (i, stat) in arr.iter().enumerate() {
                let item_path = format!("{stats_path}/{i}");
                let Some(stat_obj) = stat.as_object() else {
                    diags.push(Diagnostic::error(
                        &item_path,
                        "type_mismatch",
                        "A stat must be an object with a label and a value. Write it as {\"label\": \"…\", \"value\": \"…\"}.",
                    ));
                    continue;
                };
                check_known_fields(stat_obj, &["label", "value", "unit"], &item_path, diags);
                require_string(stat_obj, "label", &item_path, diags);
                require_string(stat_obj, "value", &item_path, diags);
                check_optional_type(stat_obj, "unit", &item_path, FieldKind::String, diags);
            }
        } else {
            diags.push(Diagnostic::error(
                &stats_path,
                "type_mismatch",
                "Stats must be a list of label/value objects. Wrap it in an array, e.g. [{\"label\": \"…\", \"value\": \"…\"}].",
            ));
        }
    }

    if let Some(legend) = obj.get("legend").filter(|v| !v.is_null()) {
        validate_legend(legend, &format!("{path}/legend"), diags, Some(overlay_layers));
    }

    if let Some(photo) = obj.get("photo").filter(|v| !v.is_null()) {
        let photo_path = format!("{path}/photo");
        if let Some(photo_obj) = photo.as_object() {
            check_known_fields(photo_obj, &["src", "alt", "credit"], &photo_path, diags);
            if let Some(src) = require_string(photo_obj, "src", &photo_path, diags) {
                check_bad_path(src, &format!("{photo_path}/src"), true, diags);
            }
            require_string(photo_obj, "alt", &photo_path, diags);
            check_optional_type(photo_obj, "credit", &photo_path, FieldKind::String, diags);
        } else {
            diags.push(Diagnostic::error(
                &photo_path,
                "type_mismatch",
                "A photo must be an object with a src and an alt. Write it as {\"src\": \"…\", \"alt\": \"…\"}.",
            ));
        }
    }

    if let Some(index) = obj.get("index").filter(|v| !v.is_null()) {
        let index_path = format!("{path}/index");
        match index {
            Value::Bool(_) => {}
            Value::Object(index_obj) => {
                check_known_fields(index_obj, &["i", "n"], &index_path, diags);
                for field in ["i", "n"] {
                    if let Some(n) = require_number(index_obj, field, &index_path, diags) {
                        check_fits_u32(n, field, &index_path, diags);
                    }
                }
            }
            _ => diags.push(Diagnostic::error(
                &index_path,
                "type_mismatch",
                "Index must be true, or an object naming the position, like {\"i\": 3, \"n\": 5}.",
            )),
        }
    }
}

/// Checks that `obj[field]`, if present and non-null, is an array of
/// strings (a legend's `stops`/`ticks`): reports `message` (already
/// shaped per spec §11) when it is anything else.
fn check_string_array(obj: &serde_json::Map<String, Value>, field: &str, path: &str, message: &'static str, diags: &mut Vec<Diagnostic>) {
    let Some(v) = obj.get(field).filter(|v| !v.is_null()) else { return };
    if !v.as_array().is_some_and(|a| a.iter().all(|s| s.as_str().is_some())) {
        diags.push(Diagnostic::error(&format!("{path}/{field}"), "type_mismatch", message));
    }
}

/// A lightweight, tolerant pre-scan of `events` for every overlay layer
/// named by a `draw` effect, run before `camera` (`follow_track_or_draw`
/// needs it) and independent of `validate_events`'s own full walk (which
/// runs afterwards and reports every other event diagnostic).
fn collect_draw_layers(events: Option<&Value>) -> Vec<String> {
    let mut layers = Vec::new();
    let Some(arr) = events.and_then(Value::as_array) else { return layers };
    for event in arr {
        let Some(overlay) = event.as_object().and_then(|e| e.get("overlay")).and_then(Value::as_object) else { continue };
        if overlay.get("effect").and_then(Value::as_str) != Some("draw") {
            continue;
        }
        if let Some(layer) = overlay.get("layer").and_then(Value::as_str) {
            if !layers.iter().any(|l: &String| l == layer) {
                layers.push(layer.to_string());
            }
        }
    }
    layers
}

fn validate_scene(obj: &serde_json::Map<String, Value>, diags: &mut Vec<Diagnostic>) -> (Vec<String>, Vec<String>, Vec<DeclaredLayer>) {
    let mut overlay_ids = Vec::new();
    let mut track_ids = Vec::new();
    let mut declared_layers: Vec<DeclaredLayer> = Vec::new();
    check_known_fields(
        obj,
        &["style", "basemap", "raster", "globe", "atmosphere", "stars", "labels", "terminator", "buildings", "overlays", "tracks", "credits"],
        "/scene",
        diags,
    );
    check_optional_type(obj, "buildings", "/scene", FieldKind::String, diags);
    if let Some(look) = obj.get("buildings").and_then(Value::as_str) {
        if !["enhanced", "maplibre"].contains(&look) {
            diags.push(Diagnostic::error(
                "/scene/buildings",
                "unknown_value",
                format!("`buildings` is \"enhanced\" (the default) or \"maplibre\", not {look:?}"),
            ));
        }
    }

    require_string(obj, "style", "/scene", diags);
    check_optional_type(obj, "raster", "/scene", FieldKind::String, diags);
    for field in ["globe", "atmosphere", "stars", "labels", "terminator"] {
        check_optional_type(obj, field, "/scene", FieldKind::Bool, diags);
    }

    if let Some(basemap) = require_object(obj, "basemap", "/scene", diags) {
        if basemap.contains_key("tiles") {
            check_known_fields(basemap, &["tiles", "max_zoom", "attribution"], "/scene/basemap", diags);
            require_array(basemap, "tiles", "/scene/basemap", diags);
            require_number(basemap, "max_zoom", "/scene/basemap", diags);
            if require_string(basemap, "attribution", "/scene/basemap", diags).is_some_and(|a| a.trim().is_empty()) {
                diags.push(Diagnostic::warning(
                    "/scene/basemap/attribution",
                    "empty_attribution",
                    "the basemap states no credit: a story always credits its data, so the render is not publishable \
                     until `attribution` names the source's required credit",
                ));
            }
        } else {
            check_known_fields(basemap, &["source", "wordmark"], "/scene/basemap", diags);
            require_string(basemap, "source", "/scene/basemap", diags);
        }
    }

    // Spec §5.4: Mapbox's terms require its wordmark, vector or satellite
    // alike, and the engine ships none -- so the manifest says
    // `publishable: false` for a Mapbox scene unless the user names their
    // own copy in `scene.basemap.wordmark` (drawn beside the SWAT mark),
    // and this says so before the render.
    let basemap = obj.get("basemap").and_then(Value::as_object);
    let mapbox_basemap = basemap.and_then(|b| b.get("source")).and_then(Value::as_str).is_some_and(|s| s.starts_with("mapbox"))
        || basemap
            .and_then(|b| b.get("tiles"))
            .and_then(Value::as_array)
            .is_some_and(|urls| urls.iter().filter_map(Value::as_str).any(is_mapbox_url));
    // `scene.raster` is not drawn yet (the player fetches no raster
    // tiles), so a Mapbox raster owes nothing and is not warned about;
    // the render's gaps say it is not drawn.
    if mapbox_basemap && !basemap.is_some_and(|b| b.contains_key("wordmark")) {
        diags.push(Diagnostic::warning(
            "/scene/basemap",
            "mapbox_wordmark_missing",
            "a Mapbox source needs the Mapbox wordmark: name your copy (PNG or SVG) in scene.basemap.wordmark, \
             or the render is not publishable",
        ));
    }

    if let Some(overlays) = obj.get("overlays").and_then(Value::as_array) {
        for (i, overlay) in overlays.iter().enumerate() {
            let path = format!("/scene/overlays/{i}");
            let Some(overlay_obj) = overlay.as_object() else { continue };
            check_known_fields(
                overlay_obj,
                &["id", "geojson", "paint", "above_labels", "data", "line_metrics", "layers", "attribution"],
                &path,
                diags,
            );
            check_optional_type(overlay_obj, "above_labels", &path, FieldKind::Bool, diags);
            check_optional_type(overlay_obj, "geojson", &path, FieldKind::String, diags);
            check_optional_type(overlay_obj, "line_metrics", &path, FieldKind::Bool, diags);
            check_optional_type(overlay_obj, "attribution", &path, FieldKind::String, diags);
            if let Some(id) = overlay_obj.get("id").and_then(Value::as_str) {
                if overlay_ids.iter().any(|existing: &String| existing == id) {
                    diags.push(Diagnostic::error(&format!("{path}/id"), "duplicate_id", format!("overlay id `{id}` is used more than once")));
                } else {
                    overlay_ids.push(id.to_string());
                }
            }

            // `overlay_data`: exactly one of `geojson`/`data`.
            let has_geojson = overlay_obj.get("geojson").is_some_and(|v| !v.is_null());
            let has_data = overlay_obj.get("data").is_some_and(|v| !v.is_null());
            if has_geojson == has_data {
                diags.push(Diagnostic::error(
                    &path,
                    "overlay_data",
                    format!(
                        "an overlay needs exactly one of `geojson` or `data`; found {}",
                        usize::from(has_geojson) + usize::from(has_data)
                    ),
                ));
            }
            if let Some(geojson) = overlay_obj.get("geojson").and_then(Value::as_str) {
                check_bad_path(geojson, &format!("{path}/geojson"), false, diags);
            }
            // `data` as an inline `FeatureCollection` object needs no
            // path check; only its string (relative-path) form does.
            if let Some(data) = overlay_obj.get("data").and_then(Value::as_str) {
                check_bad_path(data, &format!("{path}/data"), false, diags);
            }

            let line_metrics = overlay_obj.get("line_metrics").and_then(Value::as_bool).unwrap_or(false);
            if let Some(layers) = overlay_obj.get("layers").and_then(Value::as_array) {
                for (j, layer) in layers.iter().enumerate() {
                    let layer_path = format!("{path}/layers/{j}");
                    let Some(layer_obj) = layer.as_object() else { continue };
                    check_known_fields(layer_obj, &["id", "type", "filter", "paint", "above_buildings", "role", "label", "legend"], &layer_path, diags);
                    if let Some(legend) = layer_obj.get("legend").filter(|v| !v.is_null()) {
                        validate_legend(legend, &format!("{layer_path}/legend"), diags, None);
                    }
                    check_optional_type(layer_obj, "above_buildings", &layer_path, FieldKind::Bool, diags);
                    check_optional_type(layer_obj, "role", &layer_path, FieldKind::String, diags);
                    check_optional_type(layer_obj, "label", &layer_path, FieldKind::String, diags);
                    let kind = layer_obj.get("type").and_then(Value::as_str).and_then(|k| {
                        let known = OVERLAY_LAYER_KIND_KEYWORDS.iter().find(|known| **known == k).copied();
                        if known.is_none() {
                            let hint = hint_for_unknown(k, OVERLAY_LAYER_KIND_KEYWORDS);
                            diags.push(
                                Diagnostic::error(
                                    &format!("{layer_path}/type"),
                                    "unknown_value",
                                    format!("unknown layer type `{k}`; known: {OVERLAY_LAYER_KIND_KEYWORDS:?}"),
                                )
                                .with_hint(hint),
                            );
                        }
                        known
                    });
                    if layer_obj.get("type").is_none() {
                        diags.push(Diagnostic::error(&format!("{layer_path}/type"), "missing_field", "`type` is required (line, fill or circle)"));
                    }
                    // Spec §6.1's `role_wrong_type` and R10's `unknown_role`:
                    // a role the theme does not know, or one that needs a
                    // different layer kind than this one.
                    if let Some(role) = layer_obj.get("role").and_then(Value::as_str) {
                        match role_kind(role) {
                            None => diags.push(Diagnostic::error(
                                &format!("{layer_path}/role"),
                                "unknown_role",
                                format!("There is no role called \"{role}\". Use route, tour, arrow, highlight, context, stops, numbered, event or area."),
                            )),
                            Some(expected) => {
                                if let Some(actual) = kind {
                                    let expected = kind_name(expected);
                                    if expected != actual {
                                        diags.push(Diagnostic::error(
                                            &format!("{layer_path}/role"),
                                            "role_wrong_type",
                                            format!(
                                                "Layer \"{}\" has role \"{role}\", which needs a {expected} layer, but it is a {actual} layer.",
                                                layer_obj.get("id").and_then(Value::as_str).unwrap_or(""),
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    if let Some(id) = layer_obj.get("id").and_then(Value::as_str) {
                        if declared_layers.iter().any(|existing| existing.id == id) {
                            diags.push(Diagnostic::error(&format!("{layer_path}/id"), "duplicate_id", format!("overlay layer id `{id}` is used more than once")));
                        } else {
                            declared_layers.push(DeclaredLayer { id: id.to_string(), kind, line_metrics });
                        }
                    }
                }
            }
        }
    }

    if let Some(tracks) = obj.get("tracks").and_then(Value::as_array) {
        for (i, track) in tracks.iter().enumerate() {
            let path = format!("/scene/tracks/{i}");
            if let Some(track_obj) = track.as_object() {
                check_known_fields(track_obj, &["id", "file", "style"], &path, diags);
                if let Some(style) = track_obj.get("style").and_then(Value::as_object) {
                    check_known_fields(style, &["color", "radius_px", "trail_s"], &format!("{path}/style"), diags);
                }
                if let Some(id) = track_obj.get("id").and_then(Value::as_str) {
                    // `draw:` names the synthetic track of a `follow_draw`
                    // (`tracks::DRAW_TRACK_PREFIX`), so a scene track may not.
                    if id.starts_with("draw:") {
                        diags.push(
                            Diagnostic::error(&format!("{path}/id"), "reserved_id", format!("track id `{id}` starts with `draw:`, which is reserved"))
                                .with_hint(Some("`draw:<layer>` is the camera track of a `follow_draw`; rename this track".to_string())),
                        );
                    }
                    if track_ids.iter().any(|existing: &String| existing == id) {
                        diags.push(Diagnostic::error(&format!("{path}/id"), "duplicate_id", format!("track id `{id}` is used more than once")));
                    } else {
                        track_ids.push(id.to_string());
                    }
                }
            }
        }
    }

    (overlay_ids, track_ids, declared_layers)
}

/// Walks `camera`, checking each segment's fields, ranges and the timeline
/// rules that need no more than the segments themselves (first-is-jump,
/// `fly`'s duration/speed choice, total duration). Returns the labels seen
/// (for event `after`/`at_start` references) and the summed `duration`
/// fields present (a conservative stand-in for the real, `fly`-resolved
/// total that `CameraPath::new`, Task 9, computes).
fn validate_camera(
    arr: &[Value],
    diags: &mut Vec<Diagnostic>,
    track_ids: &[String],
    overlay_layer_ids: &[String],
    draw_layer_ids: &[String],
) -> (Vec<String>, f64) {
    let mut labels = Vec::new();
    let mut total_duration = 0.0;

    if arr.is_empty() {
        diags.push(Diagnostic::error("/camera", "empty_camera", "the camera timeline has no segments"));
        return (labels, total_duration);
    }

    for (i, segment) in arr.iter().enumerate() {
        let path = format!("/camera/{i}");
        let Some(obj) = segment.as_object() else {
            diags.push(Diagnostic::error(&path, "type_mismatch", format!("a camera element must be an object, got {segment}")));
            continue;
        };

        let type_value = obj.get("type").filter(|v| !v.is_null());
        let kind = type_value.and_then(Value::as_str).unwrap_or("");
        let kind_known = !kind.is_empty() && SEGMENT_KINDS.contains(&kind);

        match type_value {
            None => {
                diags.push(Diagnostic::error(
                    &format!("{path}/type"),
                    "missing_field",
                    "`type` is required (one of jump, hold, ease, fly, orbit, follow)",
                ));
            }
            Some(v) if v.as_str().is_none() => {
                diags.push(Diagnostic::error(&format!("{path}/type"), "type_mismatch", format!("`type` must be a string, got {v}")));
            }
            Some(_) if !kind_known => {
                let hint = hint_for_unknown(kind, SEGMENT_KINDS);
                diags.push(Diagnostic::error(&format!("{path}/type"), "unsupported_segment_type", format!("unknown segment type `{kind}`")).with_hint(hint));
            }
            Some(_) => {}
        }

        if kind_known && i == 0 && kind != "jump" {
            diags.push(
                Diagnostic::error(&path, "first_segment_not_jump", "the first camera segment must be `jump`, so a story starts from a known pose")
                    .with_hint(Some("insert {\"type\": \"jump\", \"to\": {...}} as the first camera segment".to_string())),
            );
        }

        let known: &[&str] = if kind_known {
            match kind {
                "jump" => &["type", "label", "to"],
                "hold" => &["type", "label", "duration", "settle"],
                "ease" => &["type", "label", "to", "duration", "easing"],
                "fly" => &["type", "label", "to", "duration", "speed", "screen_speed", "curve", "min_zoom", "max_duration", "easing"],
                "orbit" => &["type", "label", "degrees", "duration", "easing", "around"],
                "follow" => &["type", "label", "track", "follow_draw", "duration", "offset_px", "bearing", "zoom", "pitch"],
                _ => ALL_SEGMENT_FIELDS,
            }
        } else {
            ALL_SEGMENT_FIELDS
        };
        check_known_fields(obj, known, &path, diags);

        if kind_known {
            match kind {
                "jump" => {
                    require_object(obj, "to", &path, diags);
                }
                "hold" => {
                    require_number(obj, "duration", &path, diags);
                }
                "ease" => {
                    require_object(obj, "to", &path, diags);
                    require_number(obj, "duration", &path, diags);
                }
                "fly" => {
                    require_object(obj, "to", &path, diags);
                }
                "orbit" => {
                    require_number(obj, "degrees", &path, diags);
                    require_number(obj, "duration", &path, diags);
                }
                "follow" => {
                    // `track`/`follow_draw` is `follow_track_or_draw`
                    // below, not a plain `require_string`: exactly one
                    // of the two must be present, and neither is
                    // unconditionally required on its own.
                    require_number(obj, "duration", &path, diags);
                }
                _ => {}
            }
        }

        check_optional_type(obj, "label", &path, FieldKind::String, diags);
        match kind {
            "hold" => check_optional_type(obj, "settle", &path, FieldKind::Bool, diags),
            "fly" => {
                for field in ["curve", "min_zoom", "max_duration"] {
                    check_optional_type(obj, field, &path, FieldKind::Number, diags);
                }
            }
            "orbit" => check_optional_type(obj, "around", &path, FieldKind::Tuple2, diags),
            "follow" => {
                check_optional_type(obj, "offset_px", &path, FieldKind::Tuple2, diags);
                check_optional_type(obj, "zoom", &path, FieldKind::Number, diags);
                check_optional_type(obj, "pitch", &path, FieldKind::Number, diags);
                check_optional_type(obj, "track", &path, FieldKind::String, diags);
                check_optional_type(obj, "follow_draw", &path, FieldKind::String, diags);
            }
            _ => {}
        }

        if let Some(label) = obj.get("label").and_then(Value::as_str) {
            if labels.iter().any(|l: &String| l == label) {
                diags.push(Diagnostic::error(&format!("{path}/label"), "duplicate_label", format!("label `{label}` is used more than once")));
            } else {
                labels.push(label.to_string());
            }
        }
        if let Some(duration) = obj.get("duration").and_then(Value::as_f64) {
            total_duration += duration;
            if duration <= 0.0 {
                diags.push(Diagnostic::error(&format!("{path}/duration"), "duration_not_positive", format!("duration must be greater than 0, got {duration}")));
            }
        }
        if let Some(to) = obj.get("to") {
            validate_pose(to, &format!("{path}/to"), diags);
        }
        if let Some(zoom) = obj.get("zoom").and_then(Value::as_f64) {
            check_zoom(zoom, &format!("{path}/zoom"), diags);
        }
        if let Some(pitch) = obj.get("pitch").and_then(Value::as_f64) {
            check_pitch(pitch, &format!("{path}/pitch"), diags);
        }

        if kind == "fly" {
            let present: Vec<&str> =
                ["duration", "speed", "screen_speed"].into_iter().filter(|field| obj.get(*field).is_some_and(|v| !v.is_null())).collect();
            for field in &present {
                if let Some(v) = obj.get(*field) {
                    if v.as_f64().is_none() {
                        diags.push(Diagnostic::error(&format!("{path}/{field}"), "type_mismatch", format!("`{field}` must be a number, got {v}")));
                    }
                }
            }
            if present.len() != 1 {
                let hint = if present.is_empty() {
                    Some("add one of `duration`, `speed` (e.g. 1.2) or `screen_speed`".to_string())
                } else {
                    Some(format!("keep only one of {present:?}"))
                };
                diags.push(
                    Diagnostic::error(
                        &path,
                        "fly_needs_one_of_duration_speed_screen_speed",
                        format!("a `fly` segment needs exactly one of `duration`, `speed` or `screen_speed`; found {present:?}"),
                    )
                    .with_hint(hint),
                );
            }
            if let Some(curve) = obj.get("curve").and_then(Value::as_f64) {
                if curve <= 0.0 {
                    diags.push(Diagnostic::error(&format!("{path}/curve"), "curve_not_positive", format!("curve must be greater than 0, got {curve}")));
                }
            }
        }

        if let Some(easing) = obj.get("easing").filter(|v| !v.is_null()) {
            validate_easing_value(easing, &format!("{path}/easing"), diags);
        }

        if kind == "follow" {
            let has_track = obj.get("track").is_some_and(|v| !v.is_null());
            let has_follow_draw = obj.get("follow_draw").is_some_and(|v| !v.is_null());
            if has_track == has_follow_draw {
                diags.push(Diagnostic::error(
                    &path,
                    "follow_track_or_draw",
                    format!("a `follow` segment needs exactly one of `track` or `follow_draw`; found {}", usize::from(has_track) + usize::from(has_follow_draw)),
                ).with_hint(Some("name a loaded track in `track`, or an overlay line layer with a `draw` event in `follow_draw`".to_string())));
            }
            if let Some(track) = obj.get("track").and_then(Value::as_str) {
                if !track_ids.iter().any(|existing| existing == track) {
                    let known_tracks: Vec<&str> = track_ids.iter().map(String::as_str).collect();
                    let hint = hint_for_unknown(track, &known_tracks);
                    diags.push(Diagnostic::error(&format!("{path}/track"), "unknown_track", format!("no track named `{track}`")).with_hint(hint));
                }
            }
            if let Some(follow_draw) = obj.get("follow_draw").and_then(Value::as_str) {
                if !overlay_layer_ids.iter().any(|existing| existing == follow_draw) {
                    let known: Vec<&str> = overlay_layer_ids.iter().map(String::as_str).collect();
                    let hint = hint_for_unknown(follow_draw, &known);
                    diags.push(
                        Diagnostic::error(&format!("{path}/follow_draw"), "unknown_layer", unknown_overlay_layer_message(follow_draw))
                            .with_hint(hint),
                    );
                } else if !draw_layer_ids.iter().any(|existing| existing == follow_draw) {
                    diags.push(Diagnostic::error(
                        &format!("{path}/follow_draw"),
                        "follow_track_or_draw",
                        format!("overlay layer `{follow_draw}` has no `draw` event for the camera to follow"),
                    ));
                }
            }
            if let Some(bearing) = obj.get("bearing").filter(|v| !v.is_null()) {
                validate_bearing_value(bearing, &format!("{path}/bearing"), diags);
            }
        }
    }

    if total_duration > 3600.0 {
        diags.push(Diagnostic::error("/camera", "story_too_long", format!("total camera duration {total_duration}s exceeds the 3600s limit")));
    }

    (labels, total_duration)
}

/// Chapters (spec §2.1): each names a `from`/`to` segment label or
/// seconds, and may override [`Script::furniture`] for its own span.
/// Overlap needs resolved times and is Task 3's job (this only checks
/// each chapter in isolation).
fn validate_chapters(value: &Value, diags: &mut Vec<Diagnostic>, labels: Option<&[String]>, overlay_layers: &[DeclaredLayer]) {
    let Some(arr) = value.as_array() else { return };
    let mut ids: Vec<String> = Vec::new();
    for (i, chapter) in arr.iter().enumerate() {
        let path = format!("/chapters/{i}");
        let Some(obj) = chapter.as_object() else {
            diags.push(Diagnostic::error(&path, "type_mismatch", format!("a chapter must be an object, got {chapter}")));
            continue;
        };
        check_known_fields(obj, &["id", "title", "from", "to", "alt", "still_at", "furniture", "card"], &path, diags);
        check_optional_type(obj, "alt", &path, FieldKind::String, diags);
        check_optional_type(obj, "still_at", &path, FieldKind::Number, diags);
        let id = require_string(obj, "id", &path, diags);
        require_string(obj, "title", &path, diags);
        if let Some(id) = id {
            if ids.iter().any(|existing: &String| existing == id) {
                diags.push(Diagnostic::error(&format!("{path}/id"), "duplicate_id", format!("chapter id `{id}` is used more than once")));
            } else {
                ids.push(id.to_string());
            }
        }
        match obj.get("from") {
            None | Some(Value::Null) => diags.push(Diagnostic::error(&format!("{path}/from"), "missing_field", "`from` is required (a segment label or seconds)")),
            Some(v) => validate_chapter_time(v, &format!("{path}/from"), diags, labels),
        }
        match obj.get("to") {
            None | Some(Value::Null) => diags.push(Diagnostic::error(&format!("{path}/to"), "missing_field", "`to` is required (a segment label or seconds)")),
            Some(v) => validate_chapter_time(v, &format!("{path}/to"), diags, labels),
        }
        if let Some(furniture) = obj.get("furniture").filter(|v| !v.is_null()) {
            validate_furniture(furniture, &format!("{path}/furniture"), diags, overlay_layers);
        }
        if let Some(card) = obj.get("card").filter(|v| !v.is_null()) {
            validate_card(card, &format!("{path}/card"), diags, overlay_layers);
        }
    }
}

/// A [`ChapterTime`] value: a non-negative number of seconds
/// (`bad_chapter_time`), or a known camera segment label
/// (`unknown_label`, the same check an event's `after`/`at_start` gets).
fn validate_chapter_time(value: &Value, path: &str, diags: &mut Vec<Diagnostic>, labels: Option<&[String]>) {
    if let Some(n) = value.as_f64() {
        if n < 0.0 {
            diags.push(Diagnostic::error(path, "bad_chapter_time", format!("time must be >= 0, got {n}")));
        }
        return;
    }
    if let Some(label) = value.as_str() {
        if let Some(labels) = labels {
            if !labels.iter().any(|l| l == label) {
                let known: Vec<&str> = labels.iter().map(String::as_str).collect();
                let hint = hint_for_unknown(label, &known);
                diags.push(
                    Diagnostic::error(path, "unknown_label", format!("no camera segment is labelled `{label}`; known labels: {labels:?}")).with_hint(hint),
                );
            }
        }
        return;
    }
    diags.push(Diagnostic::error(path, "type_mismatch", "must be a number of seconds, or a camera segment label"));
}

/// The caption track (spec §5): `captions: [{ "from", "to", "text" }]`.
fn validate_captions(value: &Value, diags: &mut Vec<Diagnostic>) {
    let Some(arr) = value.as_array() else { return };
    for (i, cue) in arr.iter().enumerate() {
        let path = format!("/captions/{i}");
        let Some(obj) = cue.as_object() else {
            diags.push(Diagnostic::error(&path, "type_mismatch", format!("a caption cue must be an object, got {cue}")));
            continue;
        };
        check_known_fields(obj, &["from", "to", "text"], &path, diags);
        require_number(obj, "from", &path, diags);
        require_number(obj, "to", &path, diags);
        require_string(obj, "text", &path, diags);
    }
}

/// Map furniture (spec §4.5), shared by [`Script::furniture`] and each
/// chapter's override.
fn validate_furniture(value: &Value, path: &str, diags: &mut Vec<Diagnostic>, overlay_layers: &[DeclaredLayer]) {
    let Some(obj) = value.as_object() else { return };
    check_known_fields(obj, &["source_line", "byline", "scale_bar", "legend", "north_arrow", "sources"], path, diags);
    check_optional_type(obj, "source_line", path, FieldKind::String, diags);
    check_optional_type(obj, "byline", path, FieldKind::String, diags);
    check_optional_type(obj, "scale_bar", path, FieldKind::String, diags);
    check_optional_type(obj, "north_arrow", path, FieldKind::String, diags);
    validate_enum_field(obj, "scale_bar", path, &["metric", "imperial", "both"], diags);
    validate_enum_field(obj, "north_arrow", path, &["auto", "always", "never"], diags);

    if let Some(sources) = obj.get("sources").filter(|v| !v.is_null()) {
        let sources_path = format!("{path}/sources");
        if let Some(arr) = sources.as_array() {
            for (i, group) in arr.iter().enumerate() {
                let group_path = format!("{sources_path}/{i}");
                let Some(group_obj) = group.as_object() else {
                    diags.push(Diagnostic::error(
                        &group_path,
                        "type_mismatch",
                        "A source group must be an object with a key and a text. Write it as {\"key\": \"…\", \"text\": \"…\"}.",
                    ));
                    continue;
                };
                check_known_fields(group_obj, &["key", "text"], &group_path, diags);
                require_string(group_obj, "key", &group_path, diags);
                require_string(group_obj, "text", &group_path, diags);
            }
        } else {
            diags.push(Diagnostic::error(
                &sources_path,
                "type_mismatch",
                "Sources must be an array of key/text groups. Write it as [{\"key\": \"…\", \"text\": \"…\"}].",
            ));
        }
    }

    let Some(legend) = obj.get("legend").filter(|v| !v.is_null()) else { return };
    let legend_path = format!("{path}/legend");
    let Some(legend_obj) = legend.as_object() else { return };
    check_known_fields(legend_obj, &["layers", "title", "labels"], &legend_path, diags);
    check_optional_type(legend_obj, "title", &legend_path, FieldKind::String, diags);
    let Some(layers) = require_array(legend_obj, "layers", &legend_path, diags) else { return };
    check_legend_layer_ids(layers, &format!("{legend_path}/layers"), overlay_layers, diags);
}

/// A legend object (spec §3.4, §6.3): a card's (`overlay_layers` is
/// `Some`, and `layers` may name them) or an `area` layer's own (`None`:
/// no `layers`).
fn validate_legend(legend: &Value, legend_path: &str, diags: &mut Vec<Diagnostic>, overlay_layers: Option<&[DeclaredLayer]>) {
    if let Some(legend_obj) = legend.as_object() {
        let known: &[&str] = if overlay_layers.is_some() {
            &["type", "stops", "ticks", "current", "title", "items", "layers", "breaks", "property", "ramp"]
        } else {
            &["type", "stops", "ticks", "current", "title", "items", "breaks", "property", "ramp"]
        };
        check_known_fields(legend_obj, known, legend_path, diags);
        check_optional_type(legend_obj, "property", legend_path, FieldKind::String, diags);
        check_optional_type(legend_obj, "ramp", legend_path, FieldKind::String, diags);
        if let Some(ramp) = legend_obj.get("ramp").and_then(Value::as_str).filter(|r| *r != "sequential") {
            diags.push(Diagnostic::error(
                &format!("{legend_path}/ramp"),
                "unknown_value",
                format!("unknown ramp `{ramp}`. The only ramp is \"sequential\": ordered items coloured along the stops."),
            ));
        }
        check_optional_type(legend_obj, "type", legend_path, FieldKind::String, diags);
        check_optional_type(legend_obj, "title", legend_path, FieldKind::String, diags);
        check_string_array(
            legend_obj,
            "stops",
            legend_path,
            "Stops must be an array of colour strings, e.g. [\"#5C1A1B\", \"#FFE066\"]. List them in order.",
            diags,
        );
        check_string_array(
            legend_obj,
            "ticks",
            legend_path,
            "Ticks must be an array of label strings, e.g. [\"1 Sep\", \"28 Sep\"]. List them in order.",
            diags,
        );
        if let Some(current) = legend_obj.get("current").filter(|v| !v.is_null()) {
            if current.as_u64().is_none() {
                diags.push(Diagnostic::error(
                    &format!("{legend_path}/current"),
                    "type_mismatch",
                    "Current must be a non-negative whole number: the stop's position, starting at 0.",
                ));
            }
        }
        if let Some(breaks) = legend_obj.get("breaks").filter(|v| !v.is_null()) {
            if !breaks.as_array().is_some_and(|a| a.iter().all(|b| b.as_f64().is_some())) {
                diags.push(Diagnostic::error(
                    &format!("{legend_path}/breaks"),
                    "type_mismatch",
                    "Breaks must be an array of numbers, e.g. [10, 20, 30]. List the break values in order.",
                ));
            }
        }
        if let (Some(layers), Some(overlay_layers)) = (legend_obj.get("layers").filter(|v| !v.is_null()), overlay_layers) {
            let layers_path = format!("{legend_path}/layers");
            if let Some(arr) = layers.as_array() {
                check_legend_layer_ids(arr, &layers_path, overlay_layers, diags);
            } else {
                diags.push(Diagnostic::error(
                    &layers_path,
                    "type_mismatch",
                    "Layers must be an array of overlay layer ids. Write it as [\"route\", \"stops\"].",
                ));
            }
        }
        if let Some(items) = legend_obj.get("items").filter(|v| !v.is_null()) {
            let items_path = format!("{legend_path}/items");
            if let Some(arr) = items.as_array() {
                let mut labels: Vec<&str> = Vec::new();
                for (i, item) in arr.iter().enumerate() {
                    let item_path = format!("{items_path}/{i}");
                    // A repeated label: the fill's `match` and the card's grid
                    // could not both list it (spec §6.3's one set of values).
                    if let Some(label) = item.get("label").and_then(Value::as_str).map(str::trim).filter(|l| !l.is_empty()) {
                        if labels.contains(&label) {
                            diags.push(Diagnostic::error(
                                &format!("{item_path}/label"),
                                "duplicate_label",
                                format!("This legend lists \"{label}\" twice. Each item needs its own label; remove or rename one."),
                            ));
                        }
                        labels.push(label);
                    }
                    let Some(item_obj) = item.as_object() else {
                        diags.push(Diagnostic::error(
                            &item_path,
                            "type_mismatch",
                            "A legend item must be an object with a color and a label. Write it as {\"color\": \"#…\", \"label\": \"…\"}.",
                        ));
                        continue;
                    };
                    check_known_fields(item_obj, &["color", "label", "value"], &item_path, diags);
                    check_optional_type(item_obj, "color", &item_path, FieldKind::String, diags);
                    if let Some(color) = item_obj.get("color").and_then(Value::as_str).filter(|c| !c.trim().is_empty()) {
                        check_bad_color(color, &format!("{item_path}/color"), diags);
                    }
                    require_string(item_obj, "label", &item_path, diags);
                    check_optional_type(item_obj, "value", &item_path, FieldKind::String, diags);
                }
            } else {
                diags.push(Diagnostic::error(
                    &items_path,
                    "type_mismatch",
                    "Items must be an array of legend entries. Write it as [{\"color\": \"#…\", \"label\": \"…\"}].",
                ));
            }
        }
    } else {
        diags.push(Diagnostic::error(
            legend_path,
            "type_mismatch",
            "A legend must be an object: either stops and ticks (a ramp) or items (a list). Write one of those shapes.",
        ));
    }
}

/// Checks that every element of a legend's `layers` array is a string
/// naming a declared overlay layer (`unknown_layer`, with a "did you
/// mean" hint): shared by [`FurnitureSpec`]'s 1a legend and the
/// design-system card's [`CardLegendSpec`] ([`validate_card`]), so a
/// typo gets the same diagnostic wherever `layers` is written.
fn check_legend_layer_ids(arr: &[Value], path: &str, overlay_layers: &[DeclaredLayer], diags: &mut Vec<Diagnostic>) {
    for (i, layer) in arr.iter().enumerate() {
        let item_path = format!("{path}/{i}");
        let Some(id) = layer.as_str() else {
            diags.push(Diagnostic::error(
                &item_path,
                "type_mismatch",
                "A legend layer id must be a string naming a declared overlay layer, e.g. \"route\".",
            ));
            continue;
        };
        if !overlay_layers.iter().any(|l| l.id == id) {
            let known: Vec<&str> = overlay_layers.iter().map(|l| l.id.as_str()).collect();
            let hint = hint_for_unknown(id, &known);
            diags.push(Diagnostic::error(&item_path, "unknown_layer", unknown_overlay_layer_message(id)).with_hint(hint));
        }
    }
}

fn check_zoom(zoom: f64, path: &str, diags: &mut Vec<Diagnostic>) {
    if zoom < crate::camera_control::MIN_ZOOM || zoom > crate::camera_control::MAX_ZOOM {
        diags.push(Diagnostic::error(
            path,
            "zoom_out_of_range",
            format!("zoom {zoom} is outside [{}, {}]", crate::camera_control::MIN_ZOOM, crate::camera_control::MAX_ZOOM),
        ));
    }
}

fn check_pitch(pitch: f64, path: &str, diags: &mut Vec<Diagnostic>) {
    let max_pitch_deg = f64::from(crate::camera_control::MAX_PITCH.to_degrees());
    if !(0.0..=max_pitch_deg).contains(&pitch) {
        diags.push(Diagnostic::error(path, "pitch_out_of_range", format!("pitch {pitch} is outside [0, {max_pitch_deg}]")));
    }
}

fn validate_pose(pose: &Value, path: &str, diags: &mut Vec<Diagnostic>) {
    let Some(obj) = pose.as_object() else { return };
    check_known_fields(obj, &["lon", "lat", "zoom", "pitch", "bearing", "globe", "anchor_offset_px"], path, diags);
    for field in ["lon", "lat", "zoom", "pitch", "bearing"] {
        check_optional_type(obj, field, path, FieldKind::Number, diags);
    }
    check_optional_type(obj, "globe", path, FieldKind::Bool, diags);
    check_optional_type(obj, "anchor_offset_px", path, FieldKind::Tuple2, diags);

    if let Some(zoom) = obj.get("zoom").and_then(Value::as_f64) {
        check_zoom(zoom, &format!("{path}/zoom"), diags);
    }
    if let Some(lat) = obj.get("lat").and_then(Value::as_f64) {
        if lat.abs() > crate::camera_control::MAX_MERCATOR_LAT {
            diags.push(Diagnostic::error(
                &format!("{path}/lat"),
                "lat_out_of_range",
                format!("latitude {lat} is outside +/-{}", crate::camera_control::MAX_MERCATOR_LAT),
            ));
        }
    }
    if let Some(pitch) = obj.get("pitch").and_then(Value::as_f64) {
        check_pitch(pitch, &format!("{path}/pitch"), diags);
    }
    if let Some(lon) = obj.get("lon").and_then(Value::as_f64) {
        if lon.abs() > 180.0 {
            diags.push(Diagnostic::warning(&format!("{path}/lon"), "lon_wrapped", format!("longitude {lon} is outside +/-180 and will be wrapped")));
        }
    }
}

fn validate_easing_value(value: &Value, path: &str, diags: &mut Vec<Diagnostic>) {
    match value {
        Value::String(s) => {
            let kebab = s.replace('_', "-");
            if !EASING_NAMES.contains(&kebab.as_str()) {
                let hint = hint_for_unknown(&kebab, EASING_NAMES);
                diags.push(
                    Diagnostic::error(path, "bad_easing", format!("unknown easing `{s}`; expected one of {EASING_NAMES:?}"))
                        .with_hint(hint.or_else(|| Some(format!("use one of {EASING_NAMES:?}")))),
                );
            }
        }
        Value::Array(items) => {
            let nums: Vec<f64> = items.iter().filter_map(Value::as_f64).collect();
            if nums.len() != 4 || items.len() != 4 {
                diags.push(Diagnostic::error(path, "bad_bezier", "a bezier easing array needs exactly 4 numbers [x1, y1, x2, y2]"));
                return;
            }
            let x1 = nums.first().copied().unwrap_or(0.0);
            let x2 = nums.get(2).copied().unwrap_or(0.0);
            if !(0.0..=1.0).contains(&x1) || !(0.0..=1.0).contains(&x2) {
                diags.push(Diagnostic::error(path, "bad_bezier", format!("bezier x-coordinates must be within [0, 1], got x1={x1} x2={x2}")));
            }
            // y may leave [0, 1] (an overshoot, as CSS allows), but not by
            // orders of magnitude: y = 1e308 overflowed a camera ease to a
            // non-finite zoom (found by the fuzz harness).
            let y1 = nums.get(1).copied().unwrap_or(0.0);
            let y2 = nums.get(3).copied().unwrap_or(0.0);
            if !(-MAX_BEZIER_Y..=MAX_BEZIER_Y).contains(&y1) || !(-MAX_BEZIER_Y..=MAX_BEZIER_Y).contains(&y2) {
                diags.push(Diagnostic::error(
                    path,
                    "bad_bezier",
                    format!("bezier y-coordinates must be within [-{MAX_BEZIER_Y}, {MAX_BEZIER_Y}], got y1={y1} y2={y2}"),
                ));
            }
        }
        _ => diags.push(
            Diagnostic::error(path, "bad_easing", "easing must be a name or a 4-element array").with_hint(Some(format!("use one of {EASING_NAMES:?}"))),
        ),
    }
}

fn validate_bearing_value(value: &Value, path: &str, diags: &mut Vec<Diagnostic>) {
    match value {
        Value::String(s) if s == "track" => {}
        Value::String(s) if s == "heading" => {
            diags.push(
                Diagnostic::error(path, "bad_bearing", "bearing must be \"track\" or a fixed number of degrees")
                    .with_hint(Some("did you mean `track`?".to_string())),
            );
        }
        Value::String(s) => {
            diags.push(Diagnostic::error(path, "bad_bearing", format!("unknown bearing `{s}`; expected \"track\" or a number")));
        }
        Value::Number(_) => {}
        _ => diags.push(Diagnostic::error(path, "bad_bearing", "bearing must be \"track\" or a fixed number of degrees")),
    }
}

fn validate_events(events: &Value, diags: &mut Vec<Diagnostic>, labels: Option<&[String]>, overlay_ids: &[String], overlay_layers: &[DeclaredLayer]) {
    let Some(arr) = events.as_array() else { return };

    let mut known: Vec<&str> = vec!["at", "until"];
    known.extend(ACTION_KEYS.iter());

    for (i, event) in arr.iter().enumerate() {
        let path = format!("/events/{i}");
        let Some(obj) = event.as_object() else {
            diags.push(Diagnostic::error(&path, "type_mismatch", format!("an event element must be an object, got {event}")));
            continue;
        };
        check_known_fields(obj, &known, &path, diags);

        let present_actions: Vec<&str> = ACTION_KEYS.iter().copied().filter(|k| obj.get(*k).is_some_and(|v| !v.is_null())).collect();
        if present_actions.len() != 1 {
            diags.push(Diagnostic::error(
                &path,
                "event_needs_one_action",
                format!("an event needs exactly one action; found {}: {present_actions:?}", present_actions.len()),
            ));
        }

        let at_seconds = match obj.get("at").filter(|v| !v.is_null()) {
            Some(v) => validate_event_time(v, &format!("{path}/at"), diags, labels),
            None => {
                diags.push(Diagnostic::error(&format!("{path}/at"), "missing_field", "`at` is required"));
                None
            }
        };
        let until_seconds = obj.get("until").filter(|v| !v.is_null()).and_then(|v| validate_event_time(v, &format!("{path}/until"), diags, labels));
        if let (Some(at), Some(until)) = (at_seconds, until_seconds) {
            if until <= at {
                diags.push(Diagnostic::error(&format!("{path}/until"), "until_before_at", format!("`until` ({until}) is not after `at` ({at})")));
            }
        }

        if let Some(tween) = obj.get("tween").and_then(Value::as_object) {
            check_known_fields(tween, &["param", "from", "to", "easing"], &format!("{path}/tween"), diags);
            check_tweenable_param(tween, &format!("{path}/tween"), diags);
            if let Some(easing) = tween.get("easing").filter(|v| !v.is_null()) {
                validate_easing_value(easing, &format!("{path}/tween/easing"), diags);
            }
        }
        if let Some(tween) = obj.get("tween_paint_property").and_then(Value::as_object) {
            let tween_path = format!("{path}/tween_paint_property");
            check_known_fields(tween, &["layer", "name", "from", "to", "easing"], &tween_path, diags);
            if let Some(easing) = tween.get("easing").filter(|v| !v.is_null()) {
                validate_easing_value(easing, &format!("{tween_path}/easing"), diags);
            }
        }
        if let Some(caption) = obj.get("caption").and_then(Value::as_object) {
            check_known_fields(caption, &["text", "anchor", "offset_px", "size_px", "color", "plate", "fade_s"], &format!("{path}/caption"), diags);
            let caption_path = format!("{path}/caption");
            check_optional_type(caption, "offset_px", &caption_path, FieldKind::Tuple2, diags);
            check_optional_type(caption, "size_px", &caption_path, FieldKind::Number, diags);
            check_optional_type(caption, "color", &caption_path, FieldKind::String, diags);
            check_optional_type(caption, "plate", &caption_path, FieldKind::Bool, diags);
            check_optional_type(caption, "fade_s", &caption_path, FieldKind::Number, diags);
            if let Some(anchor) = caption.get("anchor").and_then(Value::as_str) {
                if crate::style::TextAnchor::from_keyword(anchor).is_none() {
                    let hint = hint_for_unknown(anchor, ANCHOR_KEYWORDS);
                    diags.push(
                        Diagnostic::error(&format!("{path}/caption/anchor"), "bad_anchor", format!("unknown anchor `{anchor}`"))
                            .with_hint(hint.or_else(|| Some(format!("use one of {ANCHOR_KEYWORDS:?}")))),
                    );
                }
            }
        }
        if let Some(show) = obj.get("show_overlay").and_then(Value::as_object) {
            check_known_fields(show, &["id"], &format!("{path}/show_overlay"), diags);
            check_overlay_ref(show, &format!("{path}/show_overlay"), overlay_ids, diags);
        }
        if let Some(hide) = obj.get("hide_overlay").and_then(Value::as_object) {
            check_known_fields(hide, &["id"], &format!("{path}/hide_overlay"), diags);
            check_overlay_ref(hide, &format!("{path}/hide_overlay"), overlay_ids, diags);
        }
        if let Some(set_overlay_data) = obj.get("set_overlay_data").and_then(Value::as_object) {
            check_known_fields(set_overlay_data, &["id", "geojson"], &format!("{path}/set_overlay_data"), diags);
            check_overlay_ref(set_overlay_data, &format!("{path}/set_overlay_data"), overlay_ids, diags);
        }
        if let Some(v) = obj.get("set_layer_visibility").and_then(Value::as_object) {
            check_known_fields(v, &["layer", "visible"], &format!("{path}/set_layer_visibility"), diags);
        }
        if let Some(v) = obj.get("set_filter").and_then(Value::as_object) {
            check_known_fields(v, &["layer", "filter"], &format!("{path}/set_filter"), diags);
        }
        if let Some(v) = obj.get("set_paint_property").and_then(Value::as_object) {
            check_known_fields(v, &["layer", "name", "value"], &format!("{path}/set_paint_property"), diags);
        }
        if let Some(v) = obj.get("set_layout_property").and_then(Value::as_object) {
            check_known_fields(v, &["layer", "name", "value"], &format!("{path}/set_layout_property"), diags);
        }
        if let Some(v) = obj.get("set_style").and_then(Value::as_object) {
            check_known_fields(v, &["style"], &format!("{path}/set_style"), diags);
        }
        if let Some(v) = obj.get("set_basemap").and_then(Value::as_object) {
            check_known_fields(v, &["source"], &format!("{path}/set_basemap"), diags);
        }
        if let Some(v) = obj.get("set_globe").and_then(Value::as_object) {
            check_known_fields(v, &["on"], &format!("{path}/set_globe"), diags);
        }
        if let Some(v) = obj.get("set_param").and_then(Value::as_object) {
            check_known_fields(v, &["param", "value"], &format!("{path}/set_param"), diags);
            check_tweenable_param(v, &format!("{path}/set_param"), diags);
        }
        if let Some(v) = obj.get("settle").and_then(Value::as_object) {
            check_known_fields(v, &["timeout_s"], &format!("{path}/settle"), diags);
        }

        if let Some(overlay) = obj.get("overlay").and_then(Value::as_object) {
            let overlay_path = format!("{path}/overlay");
            check_known_fields(
                overlay,
                &[
                    "layer",
                    "effect",
                    "fade_s",
                    "easing",
                    "head",
                    "ids",
                    "where",
                    "color",
                    "dim_opacity",
                    "stagger_s",
                    "time_property",
                    "ripple",
                    "period_s",
                    "max_radius_px",
                ],
                &overlay_path,
                diags,
            );
            check_optional_type(overlay, "fade_s", &overlay_path, FieldKind::Number, diags);
            check_optional_type(overlay, "dim_opacity", &overlay_path, FieldKind::Number, diags);
            check_optional_type(overlay, "stagger_s", &overlay_path, FieldKind::Number, diags);
            check_optional_type(overlay, "time_property", &overlay_path, FieldKind::String, diags);
            check_optional_type(overlay, "ripple", &overlay_path, FieldKind::Bool, diags);
            check_optional_type(overlay, "period_s", &overlay_path, FieldKind::Number, diags);
            check_optional_type(overlay, "max_radius_px", &overlay_path, FieldKind::Number, diags);
            check_optional_type(overlay, "color", &overlay_path, FieldKind::String, diags);
            if let Some(easing) = overlay.get("easing").filter(|v| !v.is_null()) {
                validate_easing_value(easing, &format!("{overlay_path}/easing"), diags);
            }
            if let Some(color) = overlay.get("color").and_then(Value::as_str) {
                check_bad_color(color, &format!("{overlay_path}/color"), diags);
            }
            if let Some(head) = overlay.get("head").filter(|v| !v.is_null()) {
                let head_path = format!("{overlay_path}/head");
                if let Some(head_obj) = head.as_object() {
                    check_known_fields(head_obj, &["radius_px", "color"], &head_path, diags);
                    require_number(head_obj, "radius_px", &head_path, diags);
                    if let Some(color) = require_string(head_obj, "color", &head_path, diags) {
                        check_bad_color(color, &format!("{head_path}/color"), diags);
                    }
                }
            }

            let layer = require_string(overlay, "layer", &overlay_path, diags);
            let effect = require_string(overlay, "effect", &overlay_path, diags);
            let mut known_effect = None;
            if let Some(e) = effect {
                if EFFECT_KINDS.contains(&e) {
                    known_effect = Some(e);
                } else {
                    let hint = hint_for_unknown(e, EFFECT_KINDS);
                    diags.push(
                        Diagnostic::error(&format!("{overlay_path}/effect"), "unknown_value", format!("unknown effect `{e}`; known: {EFFECT_KINDS:?}"))
                            .with_hint(hint),
                    );
                }
            }
            let declared = layer.and_then(|l| overlay_layers.iter().find(|d| d.id == l));
            if let Some(l) = layer {
                if declared.is_none() {
                    let known: Vec<&str> = overlay_layers.iter().map(|d| d.id.as_str()).collect();
                    let hint = hint_for_unknown(l, &known);
                    diags.push(Diagnostic::error(&format!("{overlay_path}/layer"), "unknown_layer", unknown_overlay_layer_message(l)).with_hint(hint));
                }
            }
            if let (Some(declared), Some(effect)) = (declared, known_effect) {
                if effect == "draw" && declared.kind != Some("line") {
                    diags.push(Diagnostic::error(
                        &format!("{overlay_path}/effect"),
                        "effect_layer_type",
                        format!("`draw` needs a `line` layer, but `{}` is {}", declared.id, declared.kind.unwrap_or("an unknown type")),
                    ));
                }
                if (effect == "drop" || effect == "pulse") && declared.kind != Some("circle") {
                    diags.push(Diagnostic::error(
                        &format!("{overlay_path}/effect"),
                        "effect_layer_type",
                        format!("`{effect}` needs a `circle` layer, but `{}` is {}", declared.id, declared.kind.unwrap_or("an unknown type")),
                    ));
                }
                if effect == "draw" && !declared.line_metrics {
                    diags.push(Diagnostic::error(
                        &format!("{overlay_path}/effect"),
                        "draw_needs_metrics",
                        format!("`draw` needs `line_metrics: true` on layer `{}`'s overlay", declared.id),
                    ));
                }
            }
            // `until_ignored`: a `show`/`hide` ramps over `fade_s`, never
            // over `[at, until]`, so an `until` there changes nothing.
            if matches!(known_effect, Some("show") | Some("hide")) && until_seconds.is_some() {
                diags.push(
                    Diagnostic::warning(
                        &format!("{path}/until"),
                        "until_ignored",
                        format!("`{}` ignores `until`: it ramps over `fade_s` (default 0.4 s) from `at`", known_effect.unwrap_or("this effect")),
                    )
                    .with_hint(Some("use `fade_s` for the ramp's length, or `fade` for an [at, until] fade-out".to_string())),
                );
            }
            // `until_needed`: `draw`/`fade`/`highlight` run over a span;
            // `pulse` without `until` just runs to the end (spec §3.3).
            if matches!(known_effect, Some("draw") | Some("fade") | Some("highlight")) && until_seconds.is_none() {
                diags.push(Diagnostic::error(
                    &format!("{path}/until"),
                    "until_needed",
                    format!("`{}` needs `until`, the effect's span", known_effect.unwrap_or("this effect")),
                ));
            }
        }

        if let Some(panel) = obj.get("panel").and_then(Value::as_object) {
            let panel_path = format!("{path}/panel");
            check_known_fields(panel, &["placement", "headline", "body", "source", "image", "theme", "fade_s"], &panel_path, diags);
            check_optional_type(panel, "headline", &panel_path, FieldKind::String, diags);
            check_optional_type(panel, "body", &panel_path, FieldKind::String, diags);
            check_optional_type(panel, "source", &panel_path, FieldKind::String, diags);
            check_optional_type(panel, "theme", &panel_path, FieldKind::String, diags);
            check_optional_type(panel, "fade_s", &panel_path, FieldKind::Number, diags);
            if let Some(placement) = require_string(panel, "placement", &panel_path, diags) {
                if !PLACEMENT_KEYWORDS.contains(&placement) {
                    let hint = hint_for_unknown(placement, PLACEMENT_KEYWORDS);
                    diags.push(
                        Diagnostic::error(&format!("{panel_path}/placement"), "unknown_value", format!("unknown placement `{placement}`"))
                            .with_hint(hint),
                    );
                }
            }
            if let Some(theme) = panel.get("theme").and_then(Value::as_str) {
                check_theme(theme, &format!("{panel_path}/theme"), diags);
            }
            if let Some(image) = panel.get("image").filter(|v| !v.is_null()) {
                validate_image_spec(image, &format!("{panel_path}/image"), diags);
            }
        }

        if let Some(image) = obj.get("image").filter(|v| !v.is_null()) {
            validate_image_spec(image, &format!("{path}/image"), diags);
            // A title card is a panel (it dims the map and carries text); an
            // image event has no title layout (layout.rs), so it is refused.
            if image.get("placement").and_then(Value::as_str) == Some("title") {
                diags.push(
                    Diagnostic::error(&format!("{path}/image/placement"), "image_title_placement", "an image cannot use placement `title`")
                        .with_hint(Some("use `corner`, `side` or `lower-third`, or put the image in a `title` panel".to_string())),
                );
            }
        }

        if let Some(callout) = obj.get("callout").and_then(Value::as_object) {
            let callout_path = format!("{path}/callout");
            check_known_fields(callout, &["at_coord", "text", "image", "theme", "fade_s"], &callout_path, diags);
            check_optional_type(callout, "theme", &callout_path, FieldKind::String, diags);
            check_optional_type(callout, "fade_s", &callout_path, FieldKind::Number, diags);
            require_string(callout, "text", &callout_path, diags);
            if let Some(theme) = callout.get("theme").and_then(Value::as_str) {
                check_theme(theme, &format!("{callout_path}/theme"), diags);
            }
            if let Some(image) = callout.get("image").filter(|v| !v.is_null()) {
                validate_image_spec(image, &format!("{callout_path}/image"), diags);
            }
            match callout.get("at_coord") {
                None | Some(Value::Null) => {
                    diags.push(Diagnostic::error(&format!("{callout_path}/at_coord"), "missing_field", "`at_coord` is required ([lon, lat])"));
                }
                Some(v) => match v.as_array().map(Vec::as_slice) {
                    Some([lon_v, lat_v]) => match (lon_v.as_f64(), lat_v.as_f64()) {
                        (Some(lon), Some(lat)) => {
                            if lon.abs() > 180.0 || lat.abs() > crate::camera_control::MAX_MERCATOR_LAT {
                                diags.push(Diagnostic::error(
                                    &format!("{callout_path}/at_coord"),
                                    "bad_coord",
                                    format!(
                                        "[{lon}, {lat}] is outside the map: longitude must be within +/-180 and latitude within +/-{}",
                                        crate::camera_control::MAX_MERCATOR_LAT
                                    ),
                                ));
                            }
                        }
                        _ => diags.push(Diagnostic::error(&format!("{callout_path}/at_coord"), "type_mismatch", "`at_coord` must be `[lon, lat]` numbers")),
                    },
                    _ => diags.push(Diagnostic::error(&format!("{callout_path}/at_coord"), "type_mismatch", "`at_coord` must be `[lon, lat]`")),
                },
            }
        }

        if let Some(card) = obj.get("card").filter(|v| !v.is_null()) {
            validate_card(card, &format!("{path}/card"), diags, overlay_layers);
        }
    }
}

fn check_tweenable_param(obj: &serde_json::Map<String, Value>, path: &str, diags: &mut Vec<Diagnostic>) {
    if let Some(param) = obj.get("param").and_then(Value::as_str) {
        if !TWEEN_PARAMS.contains(&param) {
            let hint = hint_for_unknown(param, TWEEN_PARAMS);
            diags.push(
                Diagnostic::error(&format!("{path}/param"), "param_not_tweenable", format!("`{param}` is not tweenable; expected one of {TWEEN_PARAMS:?}"))
                    .with_hint(hint.or_else(|| Some(format!("use one of {TWEEN_PARAMS:?}")))),
            );
        }
    }
}

fn check_overlay_ref(obj: &serde_json::Map<String, Value>, path: &str, overlay_ids: &[String], diags: &mut Vec<Diagnostic>) {
    if let Some(id) = obj.get("id").and_then(Value::as_str) {
        if !overlay_ids.iter().any(|existing| existing == id) {
            let known: Vec<&str> = overlay_ids.iter().map(String::as_str).collect();
            let hint = hint_for_unknown(id, &known);
            diags.push(Diagnostic::error(&format!("{path}/id"), "unknown_overlay", format!("no overlay named `{id}`")).with_hint(hint));
        }
    }
}

/// Checks an `at`/`until` value: a bare number is validated (`>= 0`) and
/// returned for the `until >= at` comparison; a `{after}`/`{at_start}`
/// object is checked only for a KNOWN label (resolving it to seconds
/// needs a segment's resolved duration -- `Timeline::new`'s job).
fn validate_event_time(value: &Value, path: &str, diags: &mut Vec<Diagnostic>, labels: Option<&[String]>) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        if n < 0.0 {
            diags.push(Diagnostic::error(path, "negative_time", format!("time must be >= 0, got {n}")));
        }
        return Some(n);
    }
    if let Some(obj) = value.as_object() {
        check_known_fields(obj, &["after", "at_start", "offset"], path, diags);
        check_optional_type(obj, "offset", path, FieldKind::Number, diags);
        // `labels` is `None` when `camera` itself failed to parse as an
        // array (finding N3): there is nothing to check a label against,
        // and reporting `unknown_label` in that case would just be noise
        // on top of the real `camera` error.
        if let Some(labels) = labels {
            let known_labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            if let Some(label) = obj.get("after").and_then(Value::as_str) {
                if !labels.iter().any(|l| l == label) {
                    let hint = hint_for_unknown(label, &known_labels);
                    diags.push(
                        Diagnostic::error(&format!("{path}/after"), "unknown_label", format!("no camera segment is labelled `{label}`; known labels: {labels:?}"))
                            .with_hint(hint),
                    );
                }
            } else if let Some(label) = obj.get("at_start").and_then(Value::as_str) {
                if !labels.iter().any(|l| l == label) {
                    let hint = hint_for_unknown(label, &known_labels);
                    diags.push(
                        Diagnostic::error(
                            &format!("{path}/at_start"),
                            "unknown_label",
                            format!("no camera segment is labelled `{label}`; known labels: {labels:?}"),
                        )
                        .with_hint(hint),
                    );
                }
            } else if !obj.contains_key("after") && !obj.contains_key("at_start") {
                diags.push(Diagnostic::error(path, "bad_event_time", "an object here must have `after` or `at_start`"));
            }
        } else if !obj.contains_key("after") && !obj.contains_key("at_start") {
            diags.push(Diagnostic::error(path, "bad_event_time", "an object here must have `after` or `at_start`"));
        }
        return None;
    }
    diags.push(Diagnostic::error(path, "type_mismatch", "must be a number of seconds, or an object with `after`/`at_start`"));
    None
}

/// A "did you mean `known_field`?" hint for `name`, when the nearest known
/// name is within edit distance 2.
fn hint_for_unknown(name: &str, known: &[&str]) -> Option<String> {
    known
        .iter()
        .map(|k| (*k, levenshtein(name, k)))
        .min_by_key(|(_, d)| *d)
        .filter(|(_, d)| *d <= 2)
        .map(|(k, _)| format!("did you mean `{k}`?"))
}

/// Plain Levenshtein edit distance, iterative with a rolling row. Written
/// without any `[]` indexing so it satisfies this module's
/// `clippy::indexing_slicing` deny.
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let delete = prev.get(j + 1).copied().unwrap_or(0) + 1;
            let insert = cur.get(j).copied().unwrap_or(0) + 1;
            let substitute = prev.get(j).copied().unwrap_or(0) + usize::from(ca != cb);
            cur.push(delete.min(insert).min(substitute));
        }
        prev = cur;
    }
    prev.last().copied().unwrap_or(0)
}

/// Reads a STRICT RFC 3339 UTC timestamp -- exactly
/// `YYYY-MM-DDTHH:MM:SS(.d+)?Z`: a 4-digit year (no sign), 2-digit month/
/// day/hour/minute/second, a plain decimal fraction (no exponent) and a
/// literal `Z` -- into unix seconds. Every arithmetic step is checked, so
/// this cannot panic regardless of input. Hand-rolled (no `chrono`)
/// because this file is shared with the web and iOS builds
/// (`tests/wasm_portability.rs`'s scanned list) and must read no clock;
/// this reads a STRING, never `SystemTime::now()`.
pub fn parse_utc(s: &str) -> Option<f64> {
    let (year_s, rest) = take_fixed_digits(s, 4)?;
    let rest = rest.strip_prefix('-')?;
    let (month_s, rest) = take_fixed_digits(rest, 2)?;
    let rest = rest.strip_prefix('-')?;
    let (day_s, rest) = take_fixed_digits(rest, 2)?;
    let rest = rest.strip_prefix('T')?;
    let (hour_s, rest) = take_fixed_digits(rest, 2)?;
    let rest = rest.strip_prefix(':')?;
    let (minute_s, rest) = take_fixed_digits(rest, 2)?;
    let rest = rest.strip_prefix(':')?;
    let (second_s, rest) = take_fixed_digits(rest, 2)?;

    let (frac, rest) = match rest.strip_prefix('.') {
        Some(after_dot) => {
            let digits_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
            // Reject rather than accept-and-round: nanosecond precision
            // (9 digits) is already far finer than anything a story needs,
            // and an unbounded fraction is exactly how a hostile input
            // used to reach `Some(NaN)` (`10f64.powi(400)` is `inf`, and
            // `inf / inf` is `NaN`) -- see `parse_utc_is_strict_and_never_panics`.
            if digits_len == 0 || digits_len > 9 {
                return None;
            }
            let (frac_digits, tail) = after_dot.split_at(digits_len);
            (Some(frac_digits), tail)
        }
        None => (None, rest),
    };

    let rest = rest.strip_prefix('Z')?;
    if !rest.is_empty() {
        return None;
    }

    let year: i64 = year_s.parse().ok()?;
    let month: u32 = month_s.parse().ok()?;
    let day: u32 = day_s.parse().ok()?;
    let hour: u32 = hour_s.parse().ok()?;
    let minute: u32 = minute_s.parse().ok()?;
    let second: u32 = second_s.parse().ok()?;
    if hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }

    let frac_seconds: f64 = match frac {
        Some(digits) => {
            let numerator: f64 = digits.parse().ok()?;
            let scale = 10f64.powi(i32::try_from(digits.len()).ok()?);
            numerator / scale
        }
        None => 0.0,
    };

    let days = days_from_civil(year, month, day)?;
    let day_seconds = i64::from(hour).checked_mul(3600)?.checked_add(i64::from(minute).checked_mul(60)?)?.checked_add(i64::from(second))?;
    let total_seconds = days.checked_mul(86_400)?.checked_add(day_seconds)?;
    let result = total_seconds as f64 + frac_seconds;
    // Defense in depth: the 4-digit year and the 9-digit fraction cap
    // above already keep every intermediate value comfortably finite, but
    // this is the one place a NaN or infinity could reach a caller, so it
    // is checked directly rather than trusted.
    result.is_finite().then_some(result)
}

/// Takes exactly `n` bytes from the front of `s`, all ASCII digits, and
/// returns `(digits, rest)`. `None` if there are fewer than `n` bytes left
/// or any of them is not an ASCII digit -- this is what makes the parser
/// strict (no sign, no short/long fields) and safe (no slicing outside a
/// verified ASCII prefix).
fn take_fixed_digits(s: &str, n: usize) -> Option<(&str, &str)> {
    let head = s.get(0..n)?;
    if !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = s.get(n..)?;
    Some((head, rest))
}

/// Days since the unix epoch, Howard Hinnant's `days_from_civil` algorithm
/// (public domain, chrono.hpp), with every multiply/add/subtract checked
/// so a wild input returns `None` instead of panicking. In practice
/// `parse_utc`'s strict 4-digit year already keeps `y` in `0..=9999`,
/// where none of this can overflow -- the checks are defense in depth.
/// Returns `None` for a calendar date that does not exist (e.g. 30
/// February).
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) {
        return None;
    }
    let is_leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month: u32 = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if d < 1 || d > days_in_month {
        return None;
    }

    let y2: i64 = if m <= 2 { y.checked_sub(1)? } else { y };
    let era: i64 = if y2 >= 0 { y2 } else { y2.checked_sub(399)? } / 400;
    let yoe: i64 = y2.checked_sub(era.checked_mul(400)?)?; // [0, 399]
    let mp: i64 = (i64::from(m) + 9) % 12; // [0, 11], March = 0
    let doy: i64 = (153i64.checked_mul(mp)?.checked_add(2)?) / 5 + i64::from(d) - 1; // [0, 365]
    let doe: i64 = yoe.checked_mul(365)?.checked_add(yoe / 4)?.checked_sub(yoe / 100)?.checked_add(doy)?; // [0, 146096]
    era.checked_mul(146_097)?.checked_add(doe)?.checked_sub(719_468)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::test_util::{minimal_json, minimal_script};

    const MIN: &str = include_str!("../../tests/fixtures/stories/minimal.json");
    const SPEC_EXAMPLE: &str = include_str!("../../tests/fixtures/stories/spec-example.json");
    const TASK10_FOLLOW: &str = include_str!("../../tests/fixtures/stories/task10-follow.json");
    const SHOWCASE: &str = include_str!("../../tests/fixtures/stories/showcase.json");

    fn codes(json: &str) -> Vec<(&'static str, String)> {
        match parse(json.as_bytes()) {
            Ok(_) => vec![],
            Err(d) => d.into_iter().filter(|d| d.severity == Severity::Error).map(|d| (d.code, d.path)).collect(),
        }
    }
    fn with(patch: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut v: serde_json::Value = serde_json::from_str(MIN).unwrap();
        patch(&mut v);
        v.to_string()
    }

    /// A minimal valid script (the `test_util::test_script` body) with
    /// its `output` replaced by `o` (raw JSON object text), for the
    /// `output` tests below.
    fn script_with_output(o: &str) -> String {
        serde_json::json!({
            "story": 1,
            "output": serde_json::from_str::<serde_json::Value>(o).expect("valid output json in test"),
            "scene": {
                "style": "x.json",
                "basemap": {"source": "openfreemap"},
                "tracks": [{"id": "bus", "file": "bus.csv", "style": {"color": "#1f77b4", "radius_px": 5, "trail_s": 30}}]
            },
            "camera": [{"type": "jump", "to": {"zoom": 2}}, {"type": "hold", "duration": 10}],
            "events": []
        })
        .to_string()
    }

    fn parse_output(o: &str) -> Script {
        match parse(script_with_output(o).as_bytes()) {
            Ok(script) => script,
            Err(diagnostics) => panic!("test script does not parse: {diagnostics:?}"),
        }
    }

    #[test]
    fn the_minimal_story_parses() {
        assert!(parse(MIN.as_bytes()).is_ok(), "{:?}", codes(MIN));
    }

    #[test]
    fn the_spec_example_parses() {
        assert!(parse(SPEC_EXAMPLE.as_bytes()).is_ok(), "{:?}", codes(SPEC_EXAMPLE));
    }

    #[test]
    fn the_task_10_follow_json_parses() {
        assert!(parse(TASK10_FOLLOW.as_bytes()).is_ok(), "{:?}", codes(TASK10_FOLLOW));
    }

    #[test]
    fn the_showcase_script_parses() {
        assert!(parse(SHOWCASE.as_bytes()).is_ok(), "{:?}", codes(SHOWCASE));
    }

    #[test]
    fn bad_scripts_report_the_exact_code_and_path() {
        let cases: Vec<(String, &str, &str)> = vec![
            (with(|v| v["story"] = 2.into()), "unsupported_version", "/story"),
            (with(|v| v["camra"] = serde_json::json!([])), "unknown_field", "/camra"),
            (with(|v| v["camera"][1]["duration"] = (-1.0).into()), "duration_not_positive", "/camera/1/duration"),
            (with(|v| v["camera"][0]["to"]["zoom"] = 25.into()), "zoom_out_of_range", "/camera/0/to/zoom"),
            (with(|v| v["camera"][0]["to"]["lat"] = 89.into()), "lat_out_of_range", "/camera/0/to/lat"),
            (with(|v| v["camera"][0]["to"]["pitch"] = 75.into()), "pitch_out_of_range", "/camera/0/to/pitch"),
            (with(|v| v["output"] = serde_json::json!({"width": 1921, "height": 1080, "fps": 30})), "odd_dimension", "/output/width"),
            (with(|v| v["output"] = serde_json::json!({"width": 9000, "height": 1080, "fps": 30})), "too_large", "/output/width"),
            (with(|v| v["output"] = serde_json::json!({"preset": "1080p30", "pixel_ratio": 1.7})), "bad_pixel_ratio", "/output/pixel_ratio"),
            (with(|v| v["output"] = serde_json::json!({"preset": "8k"})), "unknown_preset", "/output/preset"),
            (with(|v| v["output"] = serde_json::json!({"preset": "1080p30", "fps": 0})), "fps_out_of_range", "/output/fps"),
            (
                with(|v| v["camera"][2] = serde_json::json!({"type":"fly","to":{"zoom":5},"duration":2,"speed":1.2})),
                "fly_needs_one_of_duration_speed_screen_speed",
                "/camera/2",
            ),
            (with(|v| v["camera"][2] = serde_json::json!({"type":"fly","to":{"zoom":5}})), "fly_needs_one_of_duration_speed_screen_speed", "/camera/2"),
            (with(|v| v["camera"][2] = serde_json::json!({"type":"fly","to":{"zoom":5},"speed":1.2,"curve":0})), "curve_not_positive", "/camera/2/curve"),
            (with(|v| v["camera"][0] = serde_json::json!({"type":"hold","duration":1})), "first_segment_not_jump", "/camera/0"),
            (with(|v| v["camera"] = serde_json::json!([])), "empty_camera", "/camera"),
            (with(|v| v["camera"][1] = serde_json::json!({"type":"jump","to":{},"duration":1})), "unknown_field", "/camera/1/duration"),
            (
                with(|v| v["events"] = serde_json::json!([{"at":{"after":"nope","offset":0},"set_param":{"param":"labels.on","value":false}}])),
                "unknown_label",
                "/events/0/at/after",
            ),
            (with(|v| v["events"] = serde_json::json!([{"at":-1,"set_param":{"param":"labels.on","value":false}}])), "negative_time", "/events/0/at"),
            (with(|v| v["events"] = serde_json::json!([{"at":1,"until":0.5,"caption":{"text":"x","anchor":"top"}}])), "until_before_at", "/events/0/until"),
            (with(|v| v["events"] = serde_json::json!([{"at":1,"until":1,"caption":{"text":"x","anchor":"top"}}])), "until_before_at", "/events/0/until"),
            (with(|v| v["camera"][1] = serde_json::json!({"type":"ease","to":{"zoom":3},"duration":1,"easing":[0.25,0.1,0.25,1e308]})), "bad_bezier", "/camera/1/easing"),
            (with(|v| v["events"] = serde_json::json!([{"at":0,"until":1,"tween":{"param":"fill-color","from":0,"to":1}}])), "param_not_tweenable", "/events/0/tween/param"),
            (with(|v| v["events"] = serde_json::json!([{"at":0,"caption":{"text":"x","anchor":"middle-ish"}}])), "bad_anchor", "/events/0/caption/anchor"),
            (with(|v| v["events"] = serde_json::json!([{"at":0,"show_overlay":{"id":"ghost"}}])), "unknown_overlay", "/events/0/show_overlay/id"),
            (
                with(|v| {
                    v["scene"]["overlays"] = serde_json::json!([{"id":"a","geojson":"a.json"},{"id":"a","geojson":"b.json"}]);
                }),
                "duplicate_id",
                "/scene/overlays/1/id",
            ),
            (with(|v| v["camera"][2] = serde_json::json!({"type":"ease","to":{"zoom":5},"duration":1,"easing":[1.5,0,0.5,1]})), "bad_bezier", "/camera/2/easing"),
            (with(|v| v["camera"][2] = serde_json::json!({"type":"hold","duration":4000})), "story_too_long", "/camera"),
            (with(|v| v["clock"]["start_utc"] = "yesterday".into()), "bad_utc", "/clock/start_utc"),
            (r#"{"story":1,"camera":[{"type":"jump","to":{"zoom":1e999999999}}]}"#.to_string(), "invalid_json", ""),
            ("{".to_string(), "invalid_json", ""),
            // --- fix round 1 additions ---
            (with(|v| v["camera"][2] = serde_json::json!({"type":"eaze","to":{"zoom":5},"duration":1})), "unsupported_segment_type", "/camera/2/type"),
            (
                with(|v| v["events"] = serde_json::json!([{"type":"follow"}])),
                "missing_field",
                "/events/0/at",
            ),
            (with(|v| v["camera"][1] = serde_json::json!({"duration": 1})), "missing_field", "/camera/1/type"),
            (with(|v| v["camera"][1] = serde_json::json!({"type":"hold"})), "missing_field", "/camera/1/duration"),
            (with(|v| v["camera"][1] = serde_json::json!({"type":"hold","duration":"2"})), "type_mismatch", "/camera/1/duration"),
            (
                with(|v| {
                    v["events"] = serde_json::json!([{"at":0,"caption":{"text":"x","anchor":"top"},"settle":{}}]);
                }),
                "event_needs_one_action",
                "/events/0",
            ),
            (
                with(|v| {
                    v["events"] = serde_json::json!([{"at":0}]);
                }),
                "event_needs_one_action",
                "/events/0",
            ),
            (
                with(|v| {
                    v["camera"][3] = serde_json::json!({"type":"follow","track":"nope","duration":5,"bearing":"heading"});
                }),
                "bad_bearing",
                "/camera/3/bearing",
            ),
            (
                with(|v| {
                    v["camera"][0]["label"] = "in".into();
                }),
                "duplicate_label",
                "/camera/2/label",
            ),
            (with(|v| v["output"] = serde_json::json!({"width": 4, "height": 4, "fps": 10})), "dimension_too_small", "/output/width"),
            (with(|v| v["output"] = serde_json::json!({"width": 5_000_000_000_u64, "height": 1080, "fps": 30})), "too_large", "/output/width"),
            (with(|v| v["scene"]["basemap"] = serde_json::json!({"tiles": ["a/{z}/{x}/{y}.pbf"]})), "missing_field", "/scene/basemap/max_zoom"),
            (with(|v| v["clock"]["start_utc"] = "2026-03-20T10:00:00+08:00".into()), "bad_utc", "/clock/start_utc"),
            // --- fix round 2 additions (N2: optional-field type table) ---
            (with(|v| v["camera"][0]["to"]["pitch"] = "40".into()), "type_mismatch", "/camera/0/to/pitch"),
            (with(|v| v["output"] = serde_json::json!({"preset": "1080p30", "fps": "30"})), "type_mismatch", "/output/fps"),
            (with(|v| v["camera"][1]["settle"] = "yes".into()), "type_mismatch", "/camera/1/settle"),
            (with(|v| v["camera"][1] = serde_json::json!("oops")), "type_mismatch", "/camera/1"),
            (with(|v| v["events"] = serde_json::json!(["oops"])), "type_mismatch", "/events/0"),
            (with(|v| v["story"] = 1.0.into()), "type_mismatch", "/story"),
            (with(|v| v["scene"]["globe"] = "yes".into()), "type_mismatch", "/scene/globe"),
            (with(|v| v["clock"]["rate"] = "fast".into()), "type_mismatch", "/clock/rate"),
            (with(|v| v["output"] = serde_json::json!({"preset": 30})), "type_mismatch", "/output/preset"),
            (with(|v| v["title"] = 5.into()), "type_mismatch", "/title"),
            (
                with(|v| v["camera"][2] = serde_json::json!({"type":"ease","to":{"zoom":5},"duration":1,"easing":"linear","label":9})),
                "type_mismatch",
                "/camera/2/label",
            ),
            (
                with(|v| {
                    v["camera"][3] = serde_json::json!({"type":"orbit","degrees":90,"duration":1,"around":"nope"});
                }),
                "type_mismatch",
                "/camera/3/around",
            ),
        ];
        for (json, code, path) in cases {
            let got = codes(&json);
            assert!(got.iter().any(|(c, p)| *c == code && p == path), "want ({code}, {path}) got {got:?} for {json}");
        }
    }

    #[test]
    fn an_unknown_field_gets_a_did_you_mean_hint() {
        let d = parse(with(|v| v["camra"] = serde_json::json!([])).as_bytes()).unwrap_err();
        assert!(d.iter().any(|d| d.hint.as_deref() == Some("did you mean `camera`?")), "{d:?}");
    }

    #[test]
    fn five_independent_errors_give_five_diagnostics() {
        let json = with(|v| {
            v["story"] = 2.into();
            v["camera"][0]["to"]["zoom"] = 25.into();
            v["camera"][1]["duration"] = (-1.0).into();
            v["output"] = serde_json::json!({"width": 1921, "height": 1080, "fps": 30});
            v["events"] = serde_json::json!([{"at":0,"tween":{"param":"nope","from":0,"to":1},"until":1}]);
        });
        assert_eq!(codes(&json).len(), 5, "{:?}", codes(&json));
    }

    #[test]
    fn presets_resolve_to_physical_pixels() {
        let s = parse(with(|v| v["output"] = serde_json::json!({"preset":"4k30"})).as_bytes()).unwrap();
        let o = s.output();
        assert_eq!((o.width, o.height, o.fps, o.pixel_ratio), (3840, 2160, 30, 2.0));
    }

    #[test]
    fn aspect_and_tier_resolve_the_output_and_old_presets_are_unchanged() {
        let s = parse_output(r#"{"aspect":"9:16","tier":"4k","fps":30}"#);
        assert_eq!(s.output(), ResolvedOutput { width: 2160, height: 3840, fps: 30, pixel_ratio: 2.0 });
        let f = s.format();
        assert_eq!((f.aspect, f.tier, f.safe_area, f.entitlement), (Aspect::Portrait, Tier::Uhd, SafeArea::Reels, Entitlement::Pro));
        assert_eq!(f.layout_size, (1080.0, 1920.0));
        let old = parse_output(r#"{"preset":"4k30"}"#);
        assert_eq!(old.output(), ResolvedOutput { width: 3840, height: 2160, fps: 30, pixel_ratio: 2.0 });
        assert_eq!(old.format().layout_size, (1920.0, 1080.0));
    }

    #[test]
    fn tier_with_an_explicit_size_is_an_error_and_unknown_values_are_named() {
        for (output, code) in [
            (r#"{"tier":"hd","width":800}"#, "size_and_tier"),
            (r#"{"aspect":"1:1","preset":"1080p30"}"#, "size_and_tier"),
            (r#"{"aspect":"3:2"}"#, "unknown_value"),
            (r#"{"tier":"8k"}"#, "unknown_value"),
            (r#"{"safe_area":"snapchat"}"#, "unknown_value"),
            (r#"{"codec":"prores"}"#, "unknown_value"),
        ] {
            let err = parse(script_with_output(output).as_bytes()).unwrap_err();
            assert!(err.iter().any(|d| d.code == code && d.path.starts_with("/output")), "{output}: {err:?}");
        }
    }

    #[test]
    fn mapbox_video_is_an_accepted_acknowledgement() {
        assert!(parse(script_with_output(r#"{"acknowledge":["mapbox-video","source-licence:tiles.example.com"]}"#).as_bytes()).is_ok());
        let err = parse(script_with_output(r#"{"acknowledge":["mapbox"]}"#).as_bytes()).unwrap_err();
        assert!(err.iter().any(|d| d.code == "bad_acknowledge"));
    }

    #[test]
    fn parse_utc_reads_rfc3339_z() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_utc("2026-03-20T10:00:00Z"), Some(1_774_000_800.0));
        assert_eq!(parse_utc("2026-03-20T10:00:00.5Z"), Some(1_774_000_800.5));
        assert_eq!(parse_utc("2026-02-30T00:00:00Z"), None);
    }

    #[test]
    fn parse_utc_matches_python_calendar_timegm() {
        // Cross-checked with `python3 -c "import calendar,datetime;
        // print(calendar.timegm(datetime.datetime(Y,M,D,h,mi,s).timetuple()))"`.
        assert_eq!(parse_utc("2024-02-29T00:00:00Z"), Some(1_709_164_800.0));
        assert_eq!(parse_utc("2000-02-29T00:00:00Z"), Some(951_782_400.0));
        assert_eq!(parse_utc("2100-03-01T00:00:00Z"), Some(4_107_542_400.0));
        assert_eq!(parse_utc("1900-01-01T00:00:00Z"), Some(-2_208_988_800.0));
        assert_eq!(parse_utc("1969-12-31T23:59:59Z"), Some(-1.0));
        assert_eq!(parse_utc("2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_utc("2100-02-29T00:00:00Z"), None);
    }

    #[test]
    fn parse_utc_is_strict_and_never_panics() {
        // Rejected: unpadded fields, a leading sign, an exponent in the
        // seconds, lowercase markers, a trailing offset, an out-of-range
        // month/hour/minute/second, and a wildly huge year (would have
        // overflowed the old unchecked arithmetic).
        for bad in [
            "2026-1-1T1:2:3Z",
            "+2026-01-01T00:00:00Z",
            "2026-01-01T00:00:1e1Z",
            "2026-01-01t00:00:00z",
            "2026-01-01T00:00:00+08:00",
            "2026-01-01T00:00:00+00:00",
            "2026-13-01T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "99999999999999999999-01-01T00:00:00Z",
            "9223372036854775807-03-01T00:00:00Z",
        ] {
            assert_eq!(parse_utc(bad), None, "wrongly accepted {bad}");
        }
    }

    #[test]
    fn an_optional_type_error_is_not_hidden_behind_a_range_error() {
        // N2: `hold.settle: "yes"` alongside an unrelated `zoom: 25` used
        // to report only the zoom error; the type error surfaced on the
        // next submit, which is the one-at-a-time loop the two-pass
        // design exists to prevent.
        let json = with(|v| {
            v["camera"][1]["settle"] = "yes".into();
            v["camera"][0]["to"]["zoom"] = 25.into();
        });
        let got = codes(&json);
        assert!(got.iter().any(|(c, p)| *c == "type_mismatch" && p == "/camera/1/settle"), "{got:?}");
        assert!(got.iter().any(|(c, p)| *c == "zoom_out_of_range" && p == "/camera/0/to/zoom"), "{got:?}");
    }

    #[test]
    fn a_wrongly_typed_camera_does_not_cascade_into_unknown_label() {
        // N3: with `camera` not even an array, there is no way to know
        // which labels exist, so an event's `after`/`at_start` reference
        // must be left unchecked rather than reported as `unknown_label`
        // just because the (nonexistent) label list is empty.
        let json = with(|v| {
            v["camera"] = serde_json::json!({});
        });
        let got = codes(&json);
        assert!(got.iter().any(|(c, p)| *c == "type_mismatch" && p == "/camera"), "{got:?}");
        assert!(!got.iter().any(|(c, _)| *c == "unknown_label"), "unknown_label should be skipped, not cascaded: {got:?}");
    }

    #[test]
    fn parse_utc_rejects_a_huge_fraction_instead_of_returning_nan() {
        // The reviewer's exact probe: a 400-digit fraction used to give
        // `Some(NaN)` (`10f64.powi(400)` is `inf`, and `inf / inf` is
        // `NaN`) -- accepted with no `bad_utc`, silently poisoning every
        // downstream sun/clock computation with a NaN epoch.
        let huge_fraction = format!("2026-03-20T10:00:00.{}Z", "9".repeat(400));
        let result = parse_utc(&huge_fraction);
        assert!(result.is_none(), "wrongly accepted a 400-digit fraction: {result:?}");
        assert!(result.is_none_or(f64::is_finite), "returned a non-finite value: {result:?}");

        // A 9-digit (nanosecond) fraction is still accepted; a 10-digit
        // one is not, which is the exact boundary the cap enforces.
        assert!(parse_utc("2026-03-20T10:00:00.123456789Z").is_some());
        assert!(parse_utc("2026-03-20T10:00:00.1234567890Z").is_none());
    }

    #[test]
    fn a_utc_offset_in_start_utc_gets_a_z_hint() {
        let d = parse(with(|v| v["clock"]["start_utc"] = "2026-03-20T10:00:00+08:00".into()).as_bytes()).unwrap_err();
        assert!(
            d.iter().any(|d| d.code == "bad_utc" && d.hint.as_deref().is_some_and(|h| h.contains("utc_offset_min"))),
            "{d:?}"
        );
    }

    #[test]
    fn easing_accepts_kebab_case_and_the_snake_case_alias() {
        let kebab = with(|v| v["camera"][2] = serde_json::json!({"type":"orbit","label":"in","degrees":90,"duration":1,"easing":"ease-in-out"}));
        let snake = with(|v| v["camera"][2] = serde_json::json!({"type":"orbit","label":"in","degrees":90,"duration":1,"easing":"ease_in_out"}));
        assert!(parse(kebab.as_bytes()).is_ok(), "{:?}", codes(&kebab));
        let script = parse(snake.as_bytes()).unwrap_or_else(|d| panic!("{d:?}"));
        // Canonical spelling is kebab-case on the way back out.
        let json = serde_json::to_value(&script).unwrap();
        assert_eq!(json["camera"][2]["easing"], serde_json::json!("ease-in-out"));
    }

    #[test]
    fn follow_bearing_is_track_not_heading() {
        let ok = with(|v| {
            v["scene"]["tracks"] = serde_json::json!([{"id":"bus","file":"fixtures/bus.csv","style":{"color":"#000","radius_px":4,"trail_s":10}}]);
            v["camera"][3] = serde_json::json!({"type":"follow","track":"bus","duration":1,"bearing":"track"});
        });
        assert!(parse(ok.as_bytes()).is_ok(), "{:?}", codes(&ok));

        let bad = with(|v| {
            v["scene"]["tracks"] = serde_json::json!([{"id":"bus","file":"fixtures/bus.csv","style":{"color":"#000","radius_px":4,"trail_s":10}}]);
            v["camera"][3] = serde_json::json!({"type":"follow","track":"bus","duration":1,"bearing":"heading"});
        });
        let d = parse(bad.as_bytes()).unwrap_err();
        assert!(
            d.iter().any(|d| d.code == "bad_bearing" && d.hint.as_deref() == Some("did you mean `track`?")),
            "{d:?}"
        );
    }

    #[test]
    fn a_fly_segment_survives_a_round_trip() {
        let json = with(|v| v["camera"][2] = serde_json::json!({"type":"fly","label":"in","to":{"zoom":5},"speed":1.2}));
        let a = parse(json.as_bytes()).unwrap_or_else(|d| panic!("{d:?}"));
        let bytes = serde_json::to_vec(&a).unwrap();
        let b = parse(&bytes).unwrap_or_else(|d| panic!("round trip failed: {d:?}"));
        assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&b).unwrap());
    }

    #[test]
    fn round_trip_is_stable_over_every_fixture() {
        for fixture in [MIN, SPEC_EXAMPLE, TASK10_FOLLOW, SHOWCASE] {
            let a = parse(fixture.as_bytes()).unwrap_or_else(|d| panic!("{d:?} for {fixture}"));
            let bytes = serde_json::to_vec(&a).unwrap();
            let b = parse(&bytes).unwrap_or_else(|d| panic!("round trip failed: {d:?} for {fixture}"));
            assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&b).unwrap(), "fixture: {fixture}");
        }
    }

    #[test]
    fn err_always_has_at_least_one_error() {
        // The reviewer's exact probes: a type error alongside a warning,
        // and a type error alongside an unrelated range error. Both used
        // to come back with zero Severity::Error diagnostics.
        let wrong_type_plus_warning = with(|v| {
            v["camera"][1]["duration"] = "2".into();
            v["camera"][0]["to"]["lon"] = 200.into();
        });
        let wrong_type_plus_range = with(|v| {
            v["camera"][1]["duration"] = "2".into();
            v["camera"][0]["to"]["zoom"] = 25.into();
        });

        let mut bad_jsons: Vec<String> = vec![wrong_type_plus_warning, wrong_type_plus_range];
        // Reuse every case from the big table too.
        for patch_json in [
            with(|v| v["story"] = 2.into()),
            with(|v| v["camra"] = serde_json::json!([])),
            with(|v| v["camera"] = serde_json::json!([])),
            with(|v| v["camera"][1] = serde_json::json!({"duration": 1})),
            with(|v| v["camera"][1] = serde_json::json!({"type":"hold"})),
            with(|v| v["camera"][1] = serde_json::json!({"type":"hold","duration":"2"})),
            with(|v| v["events"] = serde_json::json!([{"at":0}])),
            with(|v| v["clock"]["start_utc"] = "yesterday".into()),
            "{".to_string(),
            r#"{"story":1,"camera":[{"type":"jump","to":{"zoom":1e999999999}}]}"#.to_string(),
        ] {
            bad_jsons.push(patch_json);
        }

        for json in bad_jsons {
            if let Err(diags) = parse(json.as_bytes()) {
                assert!(diags.iter().any(|d| d.severity == Severity::Error), "no Error in {diags:?} for {json}");
            }
        }
    }

    /// F1: `scene.terminator` parses (default false), `terminator.on` is
    /// a param, and `sun.time` without either warns.
    #[test]
    fn terminator_is_an_opt_in_and_sun_time_alone_warns() {
        let script = parse(MIN.as_bytes()).unwrap();
        assert!(!script.scene.terminator);
        let on = with(|v| v["scene"]["terminator"] = true.into());
        assert!(parse(on.as_bytes()).unwrap().scene.terminator);
        let bad = with(|v| v["scene"]["terminator"] = "yes".into());
        assert!(parse(bad.as_bytes()).is_err());
        let sun = serde_json::json!([{"at": 0, "set_param": {"param": "sun.time", "value": 3600}}]);
        let (_, w) = parse_with_warnings(with(|v| v["events"] = sun.clone()).as_bytes()).unwrap();
        assert!(w.iter().any(|d| d.code == "sun_time_without_terminator" && d.path == "/events/0"), "{w:?}");
        let (_, w) = parse_with_warnings(
            with(|v| {
                v["events"] = sun.clone();
                v["scene"]["terminator"] = true.into();
            })
            .as_bytes(),
        )
        .unwrap();
        assert!(!w.iter().any(|d| d.code == "sun_time_without_terminator"), "{w:?}");
        let flip = serde_json::json!([{"at": 0, "set_param": {"param": "terminator.on", "value": true}}, {"at": 0, "set_param": {"param": "sun.time", "value": 3600}}]);
        let (_, w) = parse_with_warnings(with(|v| v["events"] = flip.clone()).as_bytes()).unwrap();
        assert!(!w.iter().any(|d| d.code == "sun_time_without_terminator"), "{w:?}");
    }

    #[test]
    fn parse_with_warnings_agrees_with_parse() {
        match parse_with_warnings(MIN.as_bytes()) {
            Ok((script, _warnings)) => assert_eq!(script.story, 1),
            Err(d) => panic!("{d:?}"),
        }

        let bad = with(|v| v["story"] = 2.into());
        let a = parse(bad.as_bytes()).unwrap_err();
        let b = parse_with_warnings(bad.as_bytes()).unwrap_err();
        let codes_of = |d: &[Diagnostic]| d.iter().map(|d| (d.code, d.path.clone())).collect::<Vec<_>>();
        assert_eq!(codes_of(&a), codes_of(&b));
    }

    #[test]
    fn scene_buildings_defaults_to_enhanced_and_names_its_values() {
        let with = |f: &dyn Fn(&mut Value)| {
            let mut v: Value = serde_json::from_str(MIN).unwrap();
            f(&mut v);
            parse(&serde_json::to_vec(&v).unwrap())
        };
        let default = with(&|_| {}).unwrap();
        assert_eq!(default.scene.buildings, BuildingsLook::Enhanced);
        assert!(serde_json::to_value(&default).unwrap()["scene"].get("buildings").is_none(), "the default is not written");
        let flat = with(&|v| v["scene"]["buildings"] = "maplibre".into()).unwrap();
        assert_eq!(flat.scene.buildings.capture(), (crate::render::extrusion::ExtrusionLook::MAPLIBRE, false));
        let bad = with(&|v| v["scene"]["buildings"] = "fancy".into()).unwrap_err();
        assert!(bad.iter().any(|d| d.path == "/scene/buildings"), "{bad:?}");
    }

    #[test]
    fn round_trip_is_stable() {
        let a = parse(MIN.as_bytes()).unwrap();
        let b = parse(&serde_json::to_vec(&a).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&b).unwrap());
    }

    // -------------------------------------------------------------
    #[test]
    fn lang_is_a_loose_bcp47_tag() {
        for ok in ["en", "ja", "pt-BR", "zh-Hant-TW", "es-419", "tlh"] {
            assert!(is_bcp47_like(ok), "{ok}");
        }
        for bad in ["", "e", "en_US", "en-", "-en", "en GB", "\"><script>", "toolongprimary", "en-abcdefghi", "12"] {
            assert!(!is_bcp47_like(bad), "{bad}");
        }
        let with = |lang: serde_json::Value| {
            serde_json::json!({"story": 1, "lang": lang, "scene": {"style": "x.json", "basemap": {"source": "openfreemap"}},
                "camera": [{"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 2}}]})
            .to_string()
        };
        assert_eq!(parse(with("pt-BR".into()).as_bytes()).unwrap().lang.as_deref(), Some("pt-BR"));
        let diags = parse(with("en_US".into()).as_bytes()).unwrap_err();
        assert!(diags.iter().any(|d| d.code == "bad_lang" && d.path == "/lang"), "{diags:?}");
        let diags = parse(with(5.into()).as_bytes()).unwrap_err();
        assert!(diags.iter().any(|d| d.path == "/lang"), "{diags:?}");
    }

    // Task 2: chapters, summary, captions, furniture, overlay layers
    // and the overlay/panel/image/callout events.
    // -------------------------------------------------------------

    fn story(extra: serde_json::Value) -> serde_json::Value {
        let mut v = serde_json::json!({
            "story": 1, "output": {"aspect": "16:9"},
            "scene": {"style": "x.json", "basemap": {"source": "openfreemap"},
                "overlays": [{"id": "route", "data": "data/route.geojson", "line_metrics": true,
                    "layers": [{"id": "route-line", "type": "line", "paint": {"line-color": "#e4572e", "line-width": 4}},
                               {"id": "ports", "type": "circle", "filter": ["==", ["geometry-type"], "Point"]}]}]},
            "camera": [{"type": "jump", "label": "start", "to": {"lon": 103.8, "lat": 1.29, "zoom": 10}}, {"type": "hold", "label": "h", "duration": 12}],
            "events": []
        });
        if let (Some(dst), Some(src)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in src { dst.insert(k.clone(), val.clone()); }
        }
        v
    }

    #[test]
    fn the_spec_examples_parse() {
        let v = story(serde_json::json!({
            "summary": "A route from port to port.",
            "chapters": [{"id": "arrival", "title": "Arrival", "from": "start", "to": 10.0, "alt": "The camera arrives."}],
            "captions": [{"from": 1.0, "to": 3.0, "text": "Hello"}],
            "furniture": {"source_line": "Source: port authority", "scale_bar": "metric", "north_arrow": "auto",
                          "legend": {"layers": ["route-line"], "title": "Route"}},
            "events": [
                {"at": 1.0, "until": 6.0, "overlay": {"layer": "route-line", "effect": "draw", "head": {"radius_px": 6, "color": "#fff"}, "easing": "linear"}},
                {"at": 2.0, "until": 5.0, "overlay": {"layer": "ports", "effect": "highlight", "ids": [1], "dim_opacity": 0.3}},
                {"at": 0.0, "until": 3.0, "panel": {"placement": "title", "headline": "Port to port", "theme": "dark"}},
                {"at": 4.0, "until": 8.0, "image": {"src": "img/port.jpg", "alt": "Cranes", "credit": "Photo: A. Tan", "placement": "corner", "fit": "cover"}},
                {"at": 6.0, "until": 9.0, "callout": {"at_coord": [103.85, 1.29], "text": "Marina Bay"}}
            ]
        }));
        let script = parse(v.to_string().as_bytes()).unwrap_or_else(|d| panic!("{d:?}"));
        assert_eq!(script.chapters.len(), 1);
        assert!(matches!(script.events[0].action, EventAction::Overlay(OverlayEffectSpec { effect: EffectKind::Draw, .. })));
    }

    #[test]
    fn until_on_show_or_hide_is_ignored_with_a_warning() {
        for effect in ["show", "hide"] {
            let v = story(serde_json::json!({"events": [{"at": 1.0, "until": 3.0, "overlay": {"layer": "route-line", "effect": effect}}]}));
            let (_, warnings) = parse_with_warnings(v.to_string().as_bytes()).unwrap_or_else(|d| panic!("{d:?}"));
            let w = warnings.iter().find(|d| d.code == "until_ignored").unwrap_or_else(|| panic!("{effect}: {warnings:?}"));
            assert_eq!((w.severity, w.path.as_str()), (Severity::Warning, "/events/0/until"));
            assert!(w.hint.as_deref().unwrap_or_default().contains("fade_s"));
        }
        let v = story(serde_json::json!({"events": [{"at": 1.0, "overlay": {"layer": "route-line", "effect": "show"}}]}));
        let (_, warnings) = parse_with_warnings(v.to_string().as_bytes()).unwrap();
        assert!(warnings.iter().all(|d| d.code != "until_ignored"));
    }

    #[test]
    fn a_scene_track_may_not_use_the_draw_prefix() {
        let mut v = story(serde_json::json!({}));
        v["scene"]["tracks"] = serde_json::json!([{"id": "draw:route-line", "file": "bus.csv", "style": {"color": "#000", "radius_px": 4, "trail_s": 10}}]);
        let err = parse(v.to_string().as_bytes()).unwrap_err();
        assert!(err.iter().any(|d| d.code == "reserved_id" && d.path == "/scene/tracks/0/id"), "{err:?}");
    }

    #[test]
    fn old_overlays_with_geojson_still_parse() {
        let v = story(serde_json::json!({"scene": {"style": "x.json", "basemap": {"source": "openfreemap"},
            "overlays": [{"id": "o", "geojson": "a.geojson", "paint": {"line-color": "#000"}}]}}));
        assert!(parse(v.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn each_structural_rule_reports_its_code_at_its_path() {
        let cases: Vec<(serde_json::Value, &str, &str)> = vec![
            (serde_json::json!({"events": [{"at": 1.0, "until": 2.0, "overlay": {"layer": "ports", "effect": "draw"}}]}), "effect_layer_type", "/events/0/overlay/effect"),
            (serde_json::json!({"events": [{"at": 1.0, "until": 2.0, "overlay": {"layer": "nope", "effect": "show"}}]}), "unknown_layer", "/events/0/overlay/layer"),
            (serde_json::json!({"events": [{"at": 1.0, "image": {"src": "../secret.png", "alt": "a", "credit": "c"}}]}), "bad_path", "/events/0/image/src"),
            (serde_json::json!({"events": [{"at": 1.0, "image": {"src": "http://x.test/a.png", "alt": "a", "credit": "c"}}]}), "bad_path", "/events/0/image/src"),
            (serde_json::json!({"events": [{"at": 1.0, "callout": {"at_coord": [200.0, 1.0], "text": "x"}}]}), "bad_coord", "/events/0/callout/at_coord"),
            (serde_json::json!({"events": [{"at": 1.0, "image": {"src": "a.png", "alt": "a", "credit": "c", "placement": "title"}}]}), "image_title_placement", "/events/0/image/placement"),
            (serde_json::json!({"events": [{"at": 1.0, "overlay": {"layer": "route-line", "effect": "draw"}}]}), "until_needed", "/events/0/until"),
            (serde_json::json!({"chapters": [{"id": "a", "title": "A", "from": 0.0, "to": 1.0}, {"id": "a", "title": "B", "from": 2.0, "to": 3.0}]}), "duplicate_id", "/chapters/1/id"),
            (serde_json::json!({"chapters": [{"id": "a", "title": "A", "from": "nowhere", "to": 1.0}]}), "unknown_label", "/chapters/0/from"),
            (serde_json::json!({"events": [{"at": 0.0, "panel": {"placement": "title", "headline": "x", "theme": "accent:#zz0000"}}]}), "bad_color", "/events/0/panel/theme"),
            // --- fix round 1: rule-table entries the review found untested ---
            // F1: `draw` on a line layer whose overlay has `line_metrics: false`.
            (serde_json::json!({
                "scene": {"style": "x.json", "basemap": {"source": "openfreemap"},
                    "overlays": [{"id": "route", "data": "data/route.geojson", "line_metrics": false,
                        "layers": [{"id": "route-line", "type": "line", "paint": {"line-color": "#e4572e"}}]}]},
                "events": [{"at": 1.0, "until": 6.0, "overlay": {"layer": "route-line", "effect": "draw"}}]
            }), "draw_needs_metrics", "/events/0/overlay/effect"),
            // F2: a `follow` segment with neither `track` nor `follow_draw`.
            (serde_json::json!({
                "camera": [{"type": "jump", "label": "start", "to": {"lon": 103.8, "lat": 1.29, "zoom": 10}},
                           {"type": "follow", "duration": 5.0}]
            }), "follow_track_or_draw", "/camera/1"),
            // F2: a `follow` segment with both `track` and `follow_draw`.
            (serde_json::json!({
                "camera": [{"type": "jump", "label": "start", "to": {"lon": 103.8, "lat": 1.29, "zoom": 10}},
                           {"type": "follow", "track": "bus", "follow_draw": "route-line", "duration": 5.0}]
            }), "follow_track_or_draw", "/camera/1"),
            // F2: `follow_draw` names a declared layer that no event ever draws.
            (serde_json::json!({
                "camera": [{"type": "jump", "label": "start", "to": {"lon": 103.8, "lat": 1.29, "zoom": 10}},
                           {"type": "follow", "follow_draw": "route-line", "duration": 5.0}]
            }), "follow_track_or_draw", "/camera/1/follow_draw"),
            // F3: `unknown_layer` at a `follow_draw` that names no declared layer at all.
            (serde_json::json!({
                "camera": [{"type": "jump", "label": "start", "to": {"lon": 103.8, "lat": 1.29, "zoom": 10}},
                           {"type": "follow", "follow_draw": "nope", "duration": 5.0}]
            }), "unknown_layer", "/camera/1/follow_draw"),
            // F3: `unknown_layer` at a legend's `layers`.
            (serde_json::json!({"furniture": {"legend": {"layers": ["nope"]}}}), "unknown_layer", "/furniture/legend/layers/0"),
            // F4: `effect_layer_type`'s `drop`/`pulse` -> needs-circle branch (only `draw` -> needs-line is tested above).
            (serde_json::json!({"events": [{"at": 1.0, "until": 2.0, "overlay": {"layer": "route-line", "effect": "drop"}}]}), "effect_layer_type", "/events/0/overlay/effect"),
            (serde_json::json!({"events": [{"at": 1.0, "until": 2.0, "overlay": {"layer": "route-line", "effect": "pulse"}}]}), "effect_layer_type", "/events/0/overlay/effect"),
            // F4: `bad_path`'s absolute-path and `~`-prefix branches (only `..` and `http://` are tested above).
            (serde_json::json!({"events": [{"at": 1.0, "image": {"src": "/etc/secret.png", "alt": "a", "credit": "c"}}]}), "bad_path", "/events/0/image/src"),
            (serde_json::json!({"events": [{"at": 1.0, "image": {"src": "~/secret.png", "alt": "a", "credit": "c"}}]}), "bad_path", "/events/0/image/src"),
            // F4: `until_needed`'s `fade`/`highlight` branches (only `draw` is tested above).
            (serde_json::json!({"events": [{"at": 1.0, "overlay": {"layer": "route-line", "effect": "fade"}}]}), "until_needed", "/events/0/until"),
            (serde_json::json!({"events": [{"at": 1.0, "overlay": {"layer": "ports", "effect": "highlight"}}]}), "until_needed", "/events/0/until"),
            // F4: `bad_chapter_time`'s negative-seconds branch (only `unknown_label` is tested above).
            (serde_json::json!({"chapters": [{"id": "a", "title": "A", "from": -1.0, "to": 1.0}]}), "bad_chapter_time", "/chapters/0/from"),
            // F4: overlay-layer `duplicate_id` shared across two different overlays (one namespace).
            (serde_json::json!({"scene": {"style": "x.json", "basemap": {"source": "openfreemap"},
                "overlays": [
                    {"id": "o1", "data": "a.geojson", "layers": [{"id": "shared", "type": "line"}]},
                    {"id": "o2", "data": "b.geojson", "layers": [{"id": "shared", "type": "circle"}]}
                ]}}), "duplicate_id", "/scene/overlays/1/layers/0/id"),
            // R10 Task 2: `role_wrong_type` -- a role naming the wrong
            // layer kind -- and `unknown_role` -- no such role.
            (serde_json::json!({"scene": {"style": "x.json", "basemap": {"source": "openfreemap"},
                "overlays": [{"id": "route", "data": "data/route.geojson", "line_metrics": true,
                    "layers": [{"id": "route-line", "type": "line", "role": "area"}]}]}}),
                "role_wrong_type", "/scene/overlays/0/layers/0/role"),
            (serde_json::json!({"scene": {"style": "x.json", "basemap": {"source": "openfreemap"},
                "overlays": [{"id": "route", "data": "data/route.geojson", "line_metrics": true,
                    "layers": [{"id": "route-line", "type": "line", "role": "nope"}]}]}}),
                "unknown_role", "/scene/overlays/0/layers/0/role"),
            // R10 Task 2: `unknown_theme` -- no such `design.theme` --
            // and a non-object `design` or `card`.
            (serde_json::json!({"design": {"theme": "mauve"}}), "unknown_theme", "/design/theme"),
            (serde_json::json!({"design": "mauve"}), "type_mismatch", "/design"),
            (serde_json::json!({"chapters": [{"id": "a", "title": "A", "from": 0.0, "to": 1.0, "card": "mauve"}]}), "type_mismatch", "/chapters/0/card"),
            // R10 Task 2: `index.i`/`index.n` out of `u32` is a pathful
            // message, not an opaque serde error.
            (serde_json::json!({"chapters": [{"id": "a", "title": "A", "from": 0.0, "to": 1.0, "card": {"index": {"i": -1, "n": 5}}}]}), "type_mismatch", "/chapters/0/card/index/i"),
        ];
        for (extra, code, path) in cases {
            let err = parse(story(extra.clone()).to_string().as_bytes()).expect_err(&extra.to_string());
            assert!(err.iter().any(|d| d.code == code && d.path == path), "{extra}: want {code} at {path}, got {err:?}");
        }
    }

    /// R10: `script.rs`'s own `role_kind` (wasm/iOS cannot name
    /// `theme_layers`, S1) agrees with `theme_layers::role_kind`'s table
    /// for every role either names, so the two copies cannot drift.
    #[test]
    fn script_rs_role_kind_agrees_with_theme_layers() {
        for role in ["route", "tour", "arrow", "highlight", "context", "stops", "numbered", "event", "area", "nope"] {
            assert_eq!(role_kind(role), crate::story::theme_layers::role_kind(role), "{role}");
        }
    }

    #[test]
    fn an_overlay_with_both_or_neither_data_source_is_refused() {
        for overlay in [serde_json::json!({"id": "o", "geojson": "a.geojson", "data": "b.geojson"}), serde_json::json!({"id": "o"})] {
            let v = story(serde_json::json!({"scene": {"style": "x.json", "basemap": {"source": "openfreemap"}, "overlays": [overlay]}}));
            let err = parse(v.to_string().as_bytes()).unwrap_err();
            assert!(err.iter().any(|d| d.code == "overlay_data" && d.path == "/scene/overlays/0"), "{err:?}");
        }
    }

    #[test]
    fn a_missing_alt_or_credit_is_not_a_parse_error() {
        let v = story(serde_json::json!({"events": [{"at": 1.0, "image": {"src": "img/a.png"}}]}));
        assert!(parse(v.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn the_minimal_script_helper_parses_with_no_extra() {
        let s = minimal_script(serde_json::json!({}));
        assert_eq!(s.camera.len(), 2);
    }

    #[test]
    fn design_fields_parse_and_round_trip() {
        let script = minimal_script(serde_json::json!({
            "design": {"archetype": "tour", "theme": "ocean", "accent": "#0B7285", "card": "auto", "dir": "ltr", "anchor": "auto", "credits": "above_platform", "frame_camera": true},
            "output": {"safe_area": "all"},
            "furniture": {"sources": [{"key": "Data", "text": "PUB Singapore"}]},
            "chapters": [{"id": "c1", "title": "Marina", "from": 0, "to": 6, "card": {"headline": "Marina", "index": {"i": 3, "n": 5}}}],
            "events": [{"at": 1.0, "until": 4.0, "card": {"quote": {"text": "q", "by": "b"}, "stats": [{"label": "Distance", "value": "2.11", "unit": "km"}], "index": true}}],
            "scene": {"overlays": [{"id": "t", "data": "x.geojson", "layers": [{"id": "r", "type": "line", "role": "route"}, {"id": "s", "type": "circle", "role": "stops", "label": "ends"}]}]}
        }));
        assert_eq!(script.design.as_ref().unwrap().theme.as_deref(), Some("ocean"));
        assert_eq!(script.output.safe_area, Some(SafeArea::All));
        assert!(matches!(script.events[0].action, EventAction::Card(_)));
        assert!(matches!(script.chapters[0].card.as_ref().unwrap().index, Some(IndexSpec::At { i: 3, n: 5 })));
        assert_eq!(script.scene.overlays[0].layers[1].label.as_deref(), Some("ends"));
        let back = crate::story::parse(serde_json::to_string(&script).unwrap().as_bytes()).expect("round-trips");
        assert_eq!(back.design.as_ref().unwrap().accent.as_deref(), Some("#0B7285"));
        assert_eq!(back.design.as_ref().unwrap().credits.as_deref(), Some("above_platform"));
    }

    #[test]
    fn an_unknown_card_field_is_rejected_by_name() {
        let raw = minimal_json(serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"headlin": "typo"}}]}));
        let diags = crate::story::parse(raw.to_string().as_bytes()).unwrap_err();
        assert!(diags.iter().any(|d| d.code == "unknown_field" && d.path == "/events/0/card/headlin"), "{diags:?}");
    }

    #[test]
    fn every_1a_story_still_parses() {
        for dir in ["tests/fixtures/stories", "examples/stories"] {
            for e in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/").to_string() + dir).unwrap() {
                let p = e.unwrap().path();
                if p.extension().is_some_and(|x| x == "json") {
                    crate::story::parse(&std::fs::read(&p).unwrap()).unwrap_or_else(|d| panic!("{}: {d:?}", p.display()));
                }
            }
        }
    }

    /// Spec §11's copy rule, checked on every new message this task
    /// added (`validate_card`, `check_legend_layer_ids`, the furniture
    /// `sources` check): a capital first letter, a full stop last, at
    /// most 200 chars, and no `{`/`[`...`}`/`]` span longer than 40
    /// chars (a short illustrative example is fine; a raw value dump is
    /// not). Does not touch 1a's own messages, which this task left as
    /// they were (Task 12 owns those).
    #[test]
    fn new_card_and_sources_messages_follow_the_copy_rules() {
        fn bracketed_spans(msg: &str) -> Vec<String> {
            let mut spans = Vec::new();
            for (open, close) in [('{', '}'), ('[', ']')] {
                let mut depth = 0i32;
                let mut start = None;
                for (i, c) in msg.char_indices() {
                    if c == open {
                        if depth == 0 {
                            start = Some(i);
                        }
                        depth += 1;
                    } else if c == close && depth > 0 {
                        depth -= 1;
                        if depth == 0 {
                            if let Some(s) = start {
                                spans.push(msg[s..=i].to_string());
                            }
                        }
                    }
                }
            }
            spans
        }
        fn check(msg: &str) {
            assert!(msg.ends_with('.'), "{msg:?} does not end with a full stop");
            assert!(msg.chars().next().is_some_and(char::is_uppercase), "{msg:?} does not start with a capital");
            assert!(msg.chars().count() <= 200, "{msg:?} is over 200 chars");
            for span in bracketed_spans(msg) {
                assert!(span.chars().count() <= 40, "{msg:?} has a bracketed span over 40 chars: {span:?}");
            }
        }

        let cases: &[(serde_json::Value, &str)] = &[
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"quote": "nope"}}]}), "/events/0/card/quote"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"stats": ["nope"]}}]}), "/events/0/card/stats/0"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"stats": "nope"}}]}), "/events/0/card/stats"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": {"stops": "nope"}}}]}), "/events/0/card/legend/stops"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": {"ticks": "nope"}}}]}), "/events/0/card/legend/ticks"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": {"current": "nope"}}}]}), "/events/0/card/legend/current"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": {"breaks": "nope"}}}]}), "/events/0/card/legend/breaks"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": {"layers": [1]}}}]}), "/events/0/card/legend/layers/0"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": {"layers": "nope"}}}]}), "/events/0/card/legend/layers"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": {"items": ["nope"]}}}]}), "/events/0/card/legend/items/0"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": {"items": "nope"}}}]}), "/events/0/card/legend/items"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"legend": "nope"}}]}), "/events/0/card/legend"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"photo": "nope"}}]}), "/events/0/card/photo"),
            (serde_json::json!({"events": [{"at": 0, "until": 1, "card": {"index": "nope"}}]}), "/events/0/card/index"),
            (serde_json::json!({"furniture": {"sources": ["nope"]}}), "/furniture/sources/0"),
            (serde_json::json!({"furniture": {"sources": "nope"}}), "/furniture/sources"),
        ];
        for (extra, path) in cases {
            let raw = minimal_json(extra.clone());
            let diags = crate::story::parse(raw.to_string().as_bytes()).unwrap_err();
            let d = diags.iter().find(|d| d.path == *path).unwrap_or_else(|| panic!("no diagnostic at {path} in {diags:?}"));
            check(&d.message);
        }
    }
}
