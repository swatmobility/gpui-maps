//! **Task 3: tokens, archetype, theme, tone and colour maths.**
//!
//! Everything here is a pure function of a [`Script`] or of plain colour
//! values: no clock, no GPU, no I/O. [`archetype_for`] and [`theme_for`]
//! are the deterministic rules an AI author (or the CLI, or a host app)
//! gets for free (spec §2.1, §6.2); [`tone_for`] decides whether a card
//! sits on `paper` or `ink` (spec §4.3); [`relative_luminance`],
//! [`contrast`], [`composite`] and [`lift_accent`] are the WCAG colour
//! maths the rest of the design system (and its tests) share (spec
//! §4.2, §6.2).
//!
//! `TextStyle` lives in [`crate::font_fallback`], an engine module, and is
//! re-exported here (ruling P6): `symbol_pass.rs` is compiled into the
//! iOS and browser shells and must never depend on a story module, but
//! it needs `TextStyle` on `LabelKey` (Task 15). `design.rs` is the only
//! place a story caller should name it.
//!
//! `STORY_FAMILY` and `STORY_ARABIC_FAMILY` (Task R1, spec §4.1, D5) are
//! re-exported the same way: they are plain names, not `cfg`-gated (unlike
//! the bundled bytes in `font_fallback::STORY_FONTS`, which are native
//! only), so re-exporting them here costs this module nothing on wasm32
//! or iOS even though neither shell ever loads the faces.
//!
//! This module is `#[path]`-included by both `web/src/lib.rs` and
//! `ios/src/lib.rs`, so it compiles on wasm32 and iOS exactly as written
//! here.

use serde::Serialize;

use crate::story::script::{CardSpec, EffectKind, EventAction, OverlayLayerKind, OverlayLayerSpec, Script};

pub use crate::font_fallback::{TextStyle, LNUM, STORY_ARABIC_FAMILY, STORY_FAMILY, TNUM};

// ---------------------------------------------------------------------
// Archetype (spec §2, §2.1)
// ---------------------------------------------------------------------

/// Which kind of story this is: sets the card's default fields, its
/// theme and what the map shows (spec §2's table). Every default can be
/// overridden by the author; nothing here forces a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Archetype {
    Journey,
    Place,
    Tour,
    Area,
    Timeline,
    Vlog,
}

impl Archetype {
    /// `s` as one of the serde names (`"journey"`, `"place"`, …), or
    /// `None` for anything else -- including the wrong case, since an
    /// author's typo should fall through to detection rather than be
    /// silently accepted.
    fn named(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
    }

    /// Its spec §2.1 name, exactly as `design.archetype` or the
    /// validation report's `design.archetype` writes it (R10, P15: a
    /// JSON author string, never the Rust variant name).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Archetype::Journey => "journey",
            Archetype::Place => "place",
            Archetype::Tour => "tour",
            Archetype::Area => "area",
            Archetype::Timeline => "timeline",
            Archetype::Vlog => "vlog",
        }
    }
}

/// `script`'s archetype, and which rule of spec §2.1 fired (1-7): first
/// match wins. Reported in the validation report as `design.archetype`
/// together with the rule number, so an author or an agent can see why.
#[must_use]
pub fn archetype_for(script: &Script) -> (Archetype, u8) {
    if let Some(name) = script.design.as_ref().and_then(|d| d.archetype.as_deref()) {
        if let Some(a) = Archetype::named(name) {
            return (a, 1);
        }
    }
    if every_chapter_has_a_photo_card(script) && no_card_has_stats(script) {
        return (Archetype::Vlog, 2);
    }
    if any_card_has_quote(script) || any_layer_has_role(script, "arrow") {
        return (Archetype::Timeline, 3);
    }
    if any_layer_is_kind(script, OverlayLayerKind::Fill) || any_layer_has_role(script, "area") {
        return (Archetype::Area, 4);
    }
    if any_line_layer_role_route(script) || any_draw_effect_targets_a_line_layer(script) {
        return (Archetype::Journey, 5);
    }
    if any_card_has_index(script) || any_circle_layer_role_numbered(script) || any_inline_circle_layer_has_3_or_more_points(script) {
        return (Archetype::Tour, 6);
    }
    (Archetype::Place, 7)
}

/// Every [`CardSpec`] the script carries: a chapter's own card and
/// every event's `card` action.
pub(crate) fn all_cards(script: &Script) -> impl Iterator<Item = &CardSpec> {
    let chapter_cards = script.chapters.iter().filter_map(|c| c.card.as_ref());
    let event_cards = script.events.iter().filter_map(|e| match &e.action {
        EventAction::Card(card) => Some(card),
        _ => None,
    });
    chapter_cards.chain(event_cards)
}

fn every_chapter_has_a_photo_card(script: &Script) -> bool {
    !script.chapters.is_empty() && script.chapters.iter().all(|c| c.card.as_ref().and_then(|card| card.photo.as_ref()).is_some())
}

fn no_card_has_stats(script: &Script) -> bool {
    all_cards(script).all(|c| c.stats.is_none())
}

fn any_card_has_quote(script: &Script) -> bool {
    all_cards(script).any(|c| c.quote.is_some())
}

fn any_card_has_index(script: &Script) -> bool {
    all_cards(script).any(|c| c.index.is_some())
}

/// Every overlay layer the script declares, across every overlay (spec
/// §2.2's one shared namespace).
fn all_layers(script: &Script) -> impl Iterator<Item = &OverlayLayerSpec> {
    script.scene.overlays.iter().flat_map(|o| o.layers.iter())
}

fn any_layer_has_role(script: &Script, role: &str) -> bool {
    all_layers(script).any(|l| l.role.as_deref() == Some(role))
}

fn any_layer_is_kind(script: &Script, kind: OverlayLayerKind) -> bool {
    all_layers(script).any(|l| l.kind == kind)
}

fn any_line_layer_role_route(script: &Script) -> bool {
    all_layers(script).any(|l| l.kind == OverlayLayerKind::Line && l.role.as_deref() == Some("route"))
}

/// Whether any `overlay` event draws (spec §3's `draw` effect) a layer
/// that is declared `type: "line"` somewhere in the script.
fn any_draw_effect_targets_a_line_layer(script: &Script) -> bool {
    script.events.iter().any(|e| match &e.action {
        EventAction::Overlay(spec) if spec.effect == EffectKind::Draw => {
            all_layers(script).any(|l| l.kind == OverlayLayerKind::Line && l.id == spec.layer)
        }
        _ => false,
    })
}

fn any_circle_layer_role_numbered(script: &Script) -> bool {
    all_layers(script).any(|l| l.kind == OverlayLayerKind::Circle && l.role.as_deref() == Some("numbered"))
}

/// Rule 6's third clause: an overlay with a circle layer whose `data` is
/// given inline (not a path to a file) and holds 3 or more point
/// features. Only inline `data` can be counted in a pure function -- a
/// file reference needs I/O to resolve, which this module never does.
fn any_inline_circle_layer_has_3_or_more_points(script: &Script) -> bool {
    script.scene.overlays.iter().any(|o| {
        o.layers.iter().any(|l| l.kind == OverlayLayerKind::Circle)
            && o.data.as_ref().is_some_and(|d| !d.is_string() && count_point_features(d) >= 3)
    })
}

fn is_point_feature(feature: &serde_json::Value) -> bool {
    matches!(feature.get("geometry").and_then(|g| g.get("type")).and_then(serde_json::Value::as_str), Some("Point" | "MultiPoint"))
}

/// The point (or multipoint) features in a GeoJSON document given
/// inline: a `FeatureCollection`'s array, or a bare `Feature` (one, if
/// it is itself a point).
fn count_point_features(doc: &serde_json::Value) -> usize {
    match doc.get("type").and_then(serde_json::Value::as_str) {
        Some("FeatureCollection") => doc.get("features").and_then(serde_json::Value::as_array).map_or(0, |features| features.iter().filter(|f| is_point_feature(f)).count()),
        Some("Feature") if is_point_feature(doc) => 1,
        _ => 0,
    }
}

// ---------------------------------------------------------------------
// Theme (spec §4.2, §6.2)
// ---------------------------------------------------------------------

/// The story's one accent and the roles it plays on overlay layers (spec
/// §6.2's table). `design.theme` overrides the archetype's default;
/// `design.accent` replaces a theme's accent everywhere without
/// changing anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Ember,
    Ink,
    Ocean,
    Fill,
}

impl Theme {
    /// Its spec §6.2 name, as `design.theme` writes it (R10).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Theme::Ember => "ember",
            Theme::Ink => "ink",
            Theme::Ocean => "ocean",
            Theme::Fill => "fill",
        }
    }
}

/// The archetype's default theme (spec §6.2 rule 2): `journey` ->
/// `ember`, `place` -> `ink`, `tour` -> `ocean`, `area` -> `fill`,
/// `timeline` -> `ink`, `vlog` -> `ember`.
#[must_use]
pub fn theme_for(archetype: Archetype) -> Theme {
    match archetype {
        Archetype::Journey | Archetype::Vlog => Theme::Ember,
        Archetype::Place | Archetype::Timeline => Theme::Ink,
        Archetype::Tour => Theme::Ocean,
        Archetype::Area => Theme::Fill,
    }
}

/// `s` as a [`Theme`] by its spec §6.2 name (`"ember"`, `"ink"`,
/// `"ocean"`, `"fill"`), or `None` for anything else.
#[must_use]
pub fn theme_named(s: &str) -> Option<Theme> {
    match s {
        "ember" => Some(Theme::Ember),
        "ink" => Some(Theme::Ink),
        "ocean" => Some(Theme::Ocean),
        "fill" => Some(Theme::Fill),
        _ => None,
    }
}

/// `theme`'s accent colour (spec §6.2's table). Used unlifted on the
/// map; [`lift_accent`] gives the version an `ink` card uses.
#[must_use]
pub fn accent(theme: Theme) -> [u8; 3] {
    match theme {
        Theme::Ember | Theme::Fill => [0xE8, 0x59, 0x0C],
        Theme::Ink => [0xE0, 0x31, 0x31],
        Theme::Ocean => [0x0B, 0x72, 0x85],
    }
}

// ---------------------------------------------------------------------
// Tone (spec §4.3)
// ---------------------------------------------------------------------

/// Which fill the card and the fade use (spec §3.1, §3.7): `paper`
/// (white) or `ink` (dark). The tone sets both, so they always agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Paper,
    Ink,
}

impl Tone {
    /// Its spec §4.3 name, as `design.card` writes it (R10).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Tone::Paper => "paper",
            Tone::Ink => "ink",
        }
    }
}

/// `design.card` (`"auto" | "paper" | "ink"`, `card`) resolved against
/// whether the scene carries a raster basemap (`has_raster`) and the
/// style's `background` layer colour, when known (spec §4.3). An
/// explicit `"paper"` or `"ink"` always wins; `"auto"` (or anything
/// else, including `None`) picks `ink` when the scene is a raster or
/// the background's WCAG relative luminance is below 0.2, and `paper`
/// otherwise.
#[must_use]
pub fn tone_for(card: Option<&str>, has_raster: bool, background: Option<[u8; 3]>) -> Tone {
    match card {
        Some("paper") => return Tone::Paper,
        Some("ink") => return Tone::Ink,
        _ => {}
    }
    if has_raster {
        return Tone::Ink;
    }
    if background.is_some_and(|bg| relative_luminance(bg) < 0.2) {
        return Tone::Ink;
    }
    Tone::Paper
}

// ---------------------------------------------------------------------
// Colour maths (spec §4.2, §6.2's accent lift)
// ---------------------------------------------------------------------

/// `c`'s WCAG relative luminance: sRGB channels linearised, then
/// `0.2126R + 0.7152G + 0.0722B`.
#[must_use]
pub fn relative_luminance(c: [u8; 3]) -> f32 {
    fn linear(channel: u8) -> f32 {
        let c = f32::from(channel) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linear(c[0]) + 0.7152 * linear(c[1]) + 0.0722 * linear(c[2])
}

/// The WCAG contrast ratio between `a` and `b`: the lighter relative
/// luminance plus 0.05, over the darker plus 0.05. Symmetric in its two
/// arguments, as the WCAG definition is.
#[must_use]
pub fn contrast(a: [u8; 3], b: [u8; 3]) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// `fg` at `alpha` over `bg`: ordinary alpha compositing, each channel
/// rounded to the nearest `u8`.
#[must_use]
pub fn composite(fg: [u8; 3], alpha: f32, bg: [u8; 3]) -> [u8; 3] {
    let alpha = alpha.clamp(0.0, 1.0);
    let mix = |f: u8, b: u8| (f32::from(f) * alpha + f32::from(b) * (1.0 - alpha)).round().clamp(0.0, 255.0) as u8;
    [mix(fg[0], bg[0]), mix(fg[1], bg[1]), mix(fg[2], bg[2])]
}

/// `accent`'s colour mixed toward white in steps of 0.05 until its
/// contrast against `INK` (the `ink` card's fill colour) is at least
/// 4.5:1 (spec §4.2, §6.2): the colour an `ink` card's hairline, counter
/// and quote marks use. The map keeps the unlifted accent.
#[must_use]
pub fn lift_accent(accent: [u8; 3]) -> [u8; 3] {
    const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
    let mut lifted = accent;
    for step in 1_u8..=20 {
        if contrast(lifted, INK) >= 4.5 {
            break;
        }
        let alpha = f32::from(step) * 0.05;
        lifted = composite(WHITE, alpha, accent);
    }
    lifted
}

// ---------------------------------------------------------------------
// Colour tokens (spec §4.2)
// ---------------------------------------------------------------------

/// Headline, values, stops, the ahead line.
pub const INK: [u8; 3] = [0x0A, 0x0F, 0x1A];
/// Body, units, legend labels, fine print on paper.
pub const INK_2: [u8; 3] = [0x4A, 0x52, 0x62];
/// Labels, ticks, the counter's `/ n` -- darkened from the round-4
/// mockup's `#7A818E` (3.9:1 on white) to clear 4.5:1 (spec §4.2, D6).
pub const INK_3: [u8; 3] = [0x6B, 0x72, 0x80];
/// Muted stops and numerals, graphics only -- never body text.
pub const MUTED: [u8; 3] = [0x7A, 0x81, 0x8E];
/// The legend's "Other" bucket (5+ categorical items folded together).
pub const OTHER: [u8; 3] = [0x86, 0x8E, 0x96];
/// An `area`'s default sequential ramp, low to high (spec §6.3).
pub const AREA_RAMP: [[u8; 3]; 5] = [[0x5C, 0x1A, 0x1B], [0x9B, 0x2C, 0x20], [0xD9, 0x48, 0x0F], [0xF5, 0x9F, 0x00], [0xFF, 0xE0, 0x66]];
/// An `area`'s default categorical palette, in item order (spec §6.3);
/// past it, [`OTHER`].
pub const AREA_PALETTE: [[u8; 3]; 6] =
    [[0xE8, 0x59, 0x0C], [0x3B, 0x5B, 0xDB], [0x2F, 0x9E, 0x44], [0xAE, 0x3E, 0xC9], [0xF5, 0x9F, 0x00], [0x10, 0x98, 0xAD]];
/// The `paper`-tone card fill's alpha, over `#FFFFFF`.
pub const PAPER_ALPHA: f32 = 0.94;
/// The `ink`-tone card fill's alpha, over `#0A0F1A`.
pub const INK_CARD_ALPHA: f32 = 0.82;
/// The `paper`-tone fade's peak alpha, at the bottom edge.
pub const FADE_PAPER_PEAK: f32 = 0.88;
/// The `ink`-tone fade's peak alpha, at the bottom edge.
pub const FADE_INK_PEAK: f32 = 0.40;

// ---------------------------------------------------------------------
// Spacing tokens (spec §4.4)
// ---------------------------------------------------------------------

/// The left text edge (the right text edge under RTL).
pub const KEYLINE: f32 = 64.0;
/// The card's inset from the frame edge.
pub const CARD_INSET: f32 = 32.0;
/// Headline/quote row to the body row.
pub const HEADLINE_BODY_GAP: f32 = 10.0;
/// The last text row to the supporting item.
pub const ITEM_GAP: f32 = 24.0;
/// Between two stats.
pub const STAT_GAP: f32 = 64.0;
/// Between a categorical legend's two columns.
pub const LEGEND_COL_GAP: f32 = 48.0;
/// The photo to row 1's text.
pub const PHOTO_GAP: f32 = 28.0;
/// A card photo's side (spec §3.4): 208 x 208, cover fit.
pub const PHOTO_SIZE: f32 = 208.0;
/// A card photo's corner radius (spec §3.4).
pub const PHOTO_RADIUS: f32 = 14.0;
/// The counter to row 1's text.
pub const COUNTER_GAP: f32 = 28.0;
/// The card to the floor obstacle below it.
pub const FLOOR_GAP: f32 = 12.0;
/// The map's clearance above the card, for camera padding.
pub const CAMERA_CLEARANCE: f32 = 120.0;
/// The card's padding: (top, sides, bottom).
pub const CARD_PAD: (f32, f32, f32) = (30.0, 32.0, 32.0);
pub const CARD_RADIUS: f32 = 24.0;
/// The card's maximum width.
pub const CARD_MAX_W: f32 = 900.0;
/// A landscape side card's maximum width.
pub const SIDE_MAX_W: f32 = 720.0;
/// A landscape compact card's maximum height.
pub const COMPACT_MAX_H: f32 = 200.0;

// ---------------------------------------------------------------------
// Type tokens (spec §4.1)
// ---------------------------------------------------------------------

/// One text style: size, line height, weight, tracking and the steps it
/// shrinks through when its content does not fit at the first step
/// (spec §5.4). `tracking_milli_em` is the tracking in thousandths of an
/// em (`-0.018 em` is `-18`); `steps` is `(size, max lines)` pairs, in
/// the order they are tried, or empty when the token never steps down.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TextToken {
    pub size: f32,
    pub line_height: f32,
    pub weight: u16,
    pub tracking_milli_em: i16,
    pub tabular: bool,
    pub steps: &'static [(f32, usize)],
}

impl TextToken {
    /// How this token shapes (spec §8): its weight, its tracking, and
    /// `tnum | lnum` when it is tabular. `TextStyle::for_text` then drops
    /// the tracking on an Arabic, Hebrew or CJK run.
    #[must_use]
    pub const fn style(&self) -> TextStyle {
        TextStyle { weight: self.weight, letter_spacing_milli_em: self.tracking_milli_em, features: if self.tabular { TNUM | LNUM } else { 0 } }
    }
}

/// Row 1's headline (spec §4.1).
pub const HEADLINE_L: TextToken = TextToken { size: 60.0, line_height: 66.0, weight: 700, tracking_milli_em: -18, tabular: false, steps: &[(60.0, 2), (54.0, 2), (54.0, 3), (48.0, 3)] };
/// Row 1's quote, when the card has one instead of a headline.
pub const QUOTE: TextToken = TextToken { size: 52.0, line_height: 57.0, weight: 600, tracking_milli_em: -12, tabular: false, steps: &[(52.0, 3), (48.0, 3), (44.0, 4)] };
/// A stat's value.
pub const STAT_L: TextToken = TextToken { size: 60.0, line_height: 64.0, weight: 600, tracking_milli_em: -10, tabular: true, steps: &[] };
/// Row 2's body text.
pub const BODY_M: TextToken = TextToken { size: 40.0, line_height: 52.0, weight: 400, tracking_milli_em: 0, tabular: false, steps: &[(40.0, 2), (36.0, 2)] };
/// Row 2's caption, in place of body when there is no `body`; also the
/// moving-camera caption row's headline (spec §3.2).
pub const CAPTION_M: TextToken = TextToken { size: 44.0, line_height: 56.0, weight: 600, tracking_milli_em: 0, tabular: false, steps: &[(44.0, 2), (40.0, 2)] };
/// The spec's "S" token: stat labels, legend ticks and labels; the
/// counter's two runs are [`COUNTER`] and [`COUNTER_OF`], a legend
/// item's value [`LEGEND_VALUE`].
pub const SMALL: TextToken = TextToken { size: 26.0, line_height: 32.0, weight: 600, tracking_milli_em: 0, tabular: false, steps: &[] };
/// The counter's numeral `i` (spec §3.3, §4.1): the S token at 700,
/// tabular, tracked +0.02 em (the counter is the only S run tracked).
pub const COUNTER: TextToken = TextToken { size: 26.0, line_height: 32.0, weight: 700, tracking_milli_em: 20, tabular: true, steps: &[] };
/// The counter's ` / n` (spec §3.3): S at 600, tabular, tracked as
/// [`COUNTER`], in ink-3.
pub const COUNTER_OF: TextToken = TextToken { size: 26.0, line_height: 32.0, weight: 600, tracking_milli_em: 20, tabular: true, steps: &[] };
/// A legend item's value (spec §3.4): S at 700, tabular, in ink.
pub const LEGEND_VALUE: TextToken = TextToken { size: 26.0, line_height: 32.0, weight: 700, tracking_milli_em: 0, tabular: true, steps: &[] };
/// The fine print: the colophon row's sources and tile credit (spec §3.6).
pub const FINE_PRINT: TextToken = TextToken { size: 20.0, line_height: 26.0, weight: 400, tracking_milli_em: 0, tabular: false, steps: &[] };
/// The tile credit, on the colophon row beside the sources (spec §3.6, as
/// amended 2026-10-05): the sources' size, never smaller -- one row, one
/// type size.
pub const CREDIT: TextToken = FINE_PRINT;
/// A map label (spec §4.1's target size; the map currently draws them
/// at 30, changed by a later task).
pub const MAP_LABEL: TextToken = TextToken { size: 26.0, line_height: 26.0, weight: 700, tracking_milli_em: 0, tabular: false, steps: &[] };
/// A 1a `panel` with `placement: "title"`, migrated to a card (spec
/// §10.2, D9): a bigger headline than [`HEADLINE_L`], with its own steps.
pub const TITLE: TextToken = TextToken { size: 72.0, line_height: 78.0, weight: 700, tracking_milli_em: -18, tabular: false, steps: &[(72.0, 2), (64.0, 2), (60.0, 3)] };

// ---------------------------------------------------------------------
// tokens(): every token as data (spec §12)
// ---------------------------------------------------------------------

/// `c` as an upper-case `#RRGGBB` string, for [`tokens`]'s JSON: the
/// consts themselves stay `[u8; 3]`, since that is what the colour
/// maths above takes and returns.
fn hex(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

/// [`tokens`]'s colour section: every `[u8; 3]`-typed const as a hex
/// string, and every alpha as a plain number.
#[derive(Serialize)]
pub struct ColorTokens {
    pub ink: String,
    pub ink_2: String,
    pub ink_3: String,
    pub muted: String,
    pub other: String,
    pub paper_alpha: f32,
    pub ink_card_alpha: f32,
    pub fade_paper_peak: f32,
    pub fade_ink_peak: f32,
}

/// [`tokens`]'s spacing section: every spacing const of spec §4.4.
#[derive(Serialize)]
pub struct SpacingTokens {
    pub keyline: f32,
    pub card_inset: f32,
    pub headline_body_gap: f32,
    pub item_gap: f32,
    pub stat_gap: f32,
    pub legend_col_gap: f32,
    pub photo_gap: f32,
    pub counter_gap: f32,
    pub floor_gap: f32,
    pub camera_clearance: f32,
    pub card_pad: (f32, f32, f32),
    pub card_radius: f32,
    pub card_max_w: f32,
    pub side_max_w: f32,
    pub compact_max_h: f32,
}

/// Every token this module defines, as one serialisable value: spec
/// §12's `design::tokens()`, so a host app's own UI (and an agent) can
/// show the same colours and sizes the engine draws with.
#[derive(Serialize)]
pub struct Tokens {
    pub headline_l: TextToken,
    pub quote: TextToken,
    pub stat_l: TextToken,
    pub body_m: TextToken,
    pub caption_m: TextToken,
    pub small: TextToken,
    pub counter: TextToken,
    pub counter_of: TextToken,
    pub legend_value: TextToken,
    pub fine_print: TextToken,
    pub credit: TextToken,
    pub map_label: TextToken,
    pub title: TextToken,
    pub colors: ColorTokens,
    pub spacing: SpacingTokens,
}

/// The current tokens, as data (spec §12).
#[must_use]
pub fn tokens() -> Tokens {
    Tokens {
        headline_l: HEADLINE_L,
        quote: QUOTE,
        stat_l: STAT_L,
        body_m: BODY_M,
        caption_m: CAPTION_M,
        small: SMALL,
        counter: COUNTER,
        counter_of: COUNTER_OF,
        legend_value: LEGEND_VALUE,
        fine_print: FINE_PRINT,
        credit: CREDIT,
        map_label: MAP_LABEL,
        title: TITLE,
        colors: ColorTokens {
            ink: hex(INK),
            ink_2: hex(INK_2),
            ink_3: hex(INK_3),
            muted: hex(MUTED),
            other: hex(OTHER),
            paper_alpha: PAPER_ALPHA,
            ink_card_alpha: INK_CARD_ALPHA,
            fade_paper_peak: FADE_PAPER_PEAK,
            fade_ink_peak: FADE_INK_PEAK,
        },
        spacing: SpacingTokens {
            keyline: KEYLINE,
            card_inset: CARD_INSET,
            headline_body_gap: HEADLINE_BODY_GAP,
            item_gap: ITEM_GAP,
            stat_gap: STAT_GAP,
            legend_col_gap: LEGEND_COL_GAP,
            photo_gap: PHOTO_GAP,
            counter_gap: COUNTER_GAP,
            floor_gap: FLOOR_GAP,
            camera_clearance: CAMERA_CLEARANCE,
            card_pad: CARD_PAD,
            card_radius: CARD_RADIUS,
            card_max_w: CARD_MAX_W,
            side_max_w: SIDE_MAX_W,
            compact_max_h: COMPACT_MAX_H,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::story::test_util::minimal_script as script;
    use serde_json::json;

    #[test]
    fn detection_rules_fire_in_order() {
        let quote = json!([{"at": 0, "until": 1, "card": {"quote": {"text": "q"}}}]);
        assert_eq!(archetype_for(&script(json!({"design": {"archetype": "area"}, "events": quote.clone()}))), (Archetype::Area, 1));
        assert_eq!(archetype_for(&script(json!({"design": {}, "events": quote}))), (Archetype::Timeline, 3));
        let vlog = json!({"chapters": [{"id": "a", "title": "A", "from": 0, "to": 2,
            "card": {"caption": "x", "photo": {"src": "a.jpg", "alt": "a", "credit": "c"}}}]});
        assert_eq!(archetype_for(&script(vlog)), (Archetype::Vlog, 2));
        assert_eq!(archetype_for(&script(json!({}))), (Archetype::Place, 7));
    }

    #[test]
    fn theme_table_matches_the_spec() {
        use Archetype::*;
        let want = [(Journey, Theme::Ember), (Place, Theme::Ink), (Tour, Theme::Ocean), (Area, Theme::Fill), (Timeline, Theme::Ink), (Vlog, Theme::Ember)];
        for (a, t) in want {
            assert_eq!(theme_for(a), t);
        }
        assert_eq!(accent(Theme::Ember), [0xE8, 0x59, 0x0C]);
        assert_eq!(accent(Theme::Ink), [0xE0, 0x31, 0x31]);
        assert_eq!(accent(Theme::Ocean), [0x0B, 0x72, 0x85]);
    }

    #[test]
    fn ink_3_clears_4_5_on_white_and_muted_does_not() {
        assert!(contrast(INK_3, [255, 255, 255]) >= 4.5);
        assert!(contrast(MUTED, [255, 255, 255]) < 4.5);
    }

    #[test]
    fn the_accent_lift_reaches_4_5_on_the_ink_card() {
        for t in [Theme::Ember, Theme::Ink, Theme::Ocean] {
            assert!(contrast(lift_accent(accent(t)), INK) >= 4.5, "{t:?}");
        }
    }

    #[test]
    fn tone_is_ink_on_rasters_and_dark_backgrounds() {
        assert_eq!(tone_for(None, true, None), Tone::Ink);
        assert_eq!(tone_for(Some("auto"), false, Some([0x10, 0x10, 0x10])), Tone::Ink);
        assert_eq!(tone_for(None, false, Some([0xF8, 0xF4, 0xF0])), Tone::Paper);
        assert_eq!(tone_for(Some("paper"), true, None), Tone::Paper);
    }

    #[test]
    fn tokens_serialise_with_every_size() {
        let v = serde_json::to_value(tokens()).unwrap();
        assert_eq!(v["headline_l"]["size"], 60.0);
        assert_eq!(v["colors"]["ink_3"], "#6B7280");
        assert_eq!((v["counter"]["weight"].as_u64(), v["counter"]["tracking_milli_em"].as_i64()), (Some(700), Some(20)), "the counter's S variant");
    }

    #[test]
    fn text_style_default_is_the_map_label_style() {
        assert_eq!(TextStyle::default(), TextStyle { weight: 0, letter_spacing_milli_em: 0, features: 0 });
    }
}
