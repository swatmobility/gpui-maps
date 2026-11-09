//! **The frame around the card** (spec §3.6, §5.1-§5.3, §5.6-§5.7): a
//! frame's platform bands, the fine print (the mark bottom-left on every
//! frame; the credits on the bottom row, or lifted above a platform's
//! band with `design.credits: "above_platform"`, D2), the floor and top limit, the landscape
//! compact/side anchor, the map share and the camera box.
//!
//! [`layout_card`] is pure: a [`FrameSpec`], the card on screen, the
//! fine print and a [`CardMeasure`] in; a [`CardLayout`] (1080 units)
//! out. The card's body is [`crate::story::card_layout`]'s height-capped
//! measure (ruling P13): this module only decides the room it gets, where
//! it hangs, and what the frame leaves the map.

use crate::story::card_layout::{
    measure_card_at, CardContent, CardLayout, CardMeasure, Ink, TextRun, BAND_GAP, GROUP_GAP, HAIRLINE_H, KEY_GAP, LIFTED_FADE_HOLD, LIFTED_FADE_LEAD, LIFTED_FADE_TAIL, LOGO_H,
    MARK_GAP,
};
use crate::story::design::{Tone, CAMERA_CLEARANCE, CARD_INSET, CARD_MAX_W, CARD_RADIUS, COMPACT_MAX_H, CREDIT, FINE_PRINT, KEYLINE, SIDE_MAX_W};
use crate::story::script::SourceGroup;
use crate::story::tiers::{frame_size, safe_area_rects, Aspect, Rect, SafeArea, Tier};

/// The script's reading direction (spec §9, R14): `lang`, or
/// `design.dir` over it ([`crate::story::cards::dir_for`]). Under
/// [`Dir::Rtl`], [`layout_card`] mirrors the card and the colophon row's text
/// (never the mark, bottom-left on every frame).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dir {
    #[default]
    Ltr,
    Rtl,
}

/// `design.anchor` (spec §5.7): which landscape card the author asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnchorPref {
    /// Compact when the card is at most 200 tall, else side unless that
    /// truncates more.
    #[default]
    Auto,
    Compact,
    Side,
}

impl AnchorPref {
    /// `design.anchor` by its script spelling; absent or unknown is `auto`
    /// (validation reports an unknown value, R10).
    #[must_use]
    pub fn named(s: Option<&str>) -> Self {
        match s.map(str::trim) {
            Some("compact") => AnchorPref::Compact,
            Some("side") => AnchorPref::Side,
            _ => AnchorPref::Auto,
        }
    }
}

/// `design.credits` (spec §3.6, amended 2026-10-05): where the colophon
/// row's sources and tile credit sit. The mark is bottom-left in both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CreditsPlacement {
    /// On the bottom row, beside the bottom-left mark, on every frame.
    #[default]
    Bottom,
    /// Under a platform safe area (`tiktok`, `reels`, `shorts`, `all`),
    /// lifted to `bottom band top - 12` (the old D2 lift); elsewhere as
    /// [`CreditsPlacement::Bottom`].
    AbovePlatform,
}

impl CreditsPlacement {
    /// `design.credits` by its script spelling (any case, trimmed);
    /// `None` for a value that names neither.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bottom" => Some(CreditsPlacement::Bottom),
            "above_platform" => Some(CreditsPlacement::AbovePlatform),
            _ => None,
        }
    }

    /// `design.credits`; absent or unknown is `bottom` (validation reports
    /// an unknown value, `unknown_credits`).
    #[must_use]
    pub fn named(s: Option<&str>) -> Self {
        s.and_then(Self::parse).unwrap_or_default()
    }
}

/// Where the card hangs (spec §5.2, §5.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Anchor {
    /// 9:16, 4:5 and 1:1: bottom-left on the floor.
    #[default]
    Bottom,
    /// Landscape, a lower third at most 900 wide.
    Compact,
    /// Landscape, a column at most 720 wide, still on the floor.
    Side,
}

/// One frame as the design system lays it out, 1080 units (a 4K frame
/// is the same `FrameSpec` as its 1080p twin: see [`units`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameSpec {
    pub w: f32,
    pub h: f32,
    pub aspect: Aspect,
    /// `output.safe_area` as the design system reads it (D1: absent is `none`).
    pub safe_area: SafeArea,
    pub dir: Dir,
    pub anchor: AnchorPref,
    pub tone: Tone,
    /// The width, units, of the mark's INK, bottom-left (spec §3.6, P3):
    /// the SWAT logo, or the Mapbox wordmark when the policy drops the
    /// logo; `None` with no mark. See [`slot_mark_w`].
    pub mark_w: Option<f32>,
    /// `design.credits` (spec §3.6): the sources and credit on the bottom
    /// row, or lifted above a platform's band.
    pub credits: CreditsPlacement,
}

/// The SWAT logo's ink width in the slot: [`LOGO_H`] (28) tall at its
/// artwork's aspect, less the artwork's side margins (150 of its 174
/// columns are ink): `slot_mark_w(Some(artwork_size()), None)`.
pub const DEFAULT_MARK_W: f32 = LOGO_H * 174.0 / 36.0 - 2.0 * (LOGO_H * crate::brand_mark::ARTWORK_SIDE_MARGIN_FRACTION);

impl FrameSpec {
    /// `frame` (units) on `aspect` under `safe_area`: LTR, `auto`, paper,
    /// the SWAT logo in the slot.
    #[must_use]
    pub fn new(frame: (f32, f32), safe_area: SafeArea, aspect: Aspect) -> Self {
        FrameSpec { w: frame.0, h: frame.1, aspect, safe_area, dir: Dir::Ltr, anchor: AnchorPref::Auto, tone: Tone::Paper, mark_w: Some(DEFAULT_MARK_W), credits: CreditsPlacement::Bottom }
    }

    /// Spec §5.7: a frame is landscape when `w / h > 1.2` (16:9).
    #[must_use]
    pub fn is_landscape(&self) -> bool {
        self.w > 1.2 * self.h
    }
}

/// A frame of `size` physical pixels at pixel ratio `ratio`, in 1080
/// units, rounded to the whole unit: what every layout here takes, so an
/// `hd`, `qhd` or `4k` frame lays out exactly as its 1080p twin (720 /
/// (720 / 1080) is 1079.9999 in `f32`, which would move a wrap).
#[must_use]
pub fn units(size: (u32, u32), ratio: f32) -> (f32, f32) {
    ((size.0 as f32 / ratio).round(), (size.1 as f32 / ratio).round())
}

/// The width, units, of the mark's INK the design frame draws at the head
/// of its colophon row (spec §3.6, P3), as `capture::chrome` places it:
/// the SWAT mark (its artwork size) when the brand-mark policy draws it,
/// [`LOGO_H`] tall, less the artwork's transparent side margins
/// ([`slot_mark_margin`]); else the Mapbox wordmark (artwork size, no
/// margin) at the same height; `None` with neither. The capture and the
/// player's camera pass both call this, so they lay out on one spec.
#[must_use]
pub fn slot_mark_w(mark: Option<(u32, u32)>, wordmark: Option<(u32, u32)>) -> Option<f32> {
    let width = |(aw, ah): (u32, u32)| (aw > 0 && ah > 0).then(|| LOGO_H * aw as f32 / ah as f32);
    match mark {
        Some(art) => width(art).map(|w| w - 2.0 * slot_mark_margin(true)),
        None => wordmark.and_then(width),
    }
}

/// The transparent margin, units, left (and right) of the slot mark's ink
/// at [`LOGO_H`]: the SWAT artwork's (`brand_mark::ARTWORK_SIDE_MARGIN_FRACTION`
/// of its height) when `swat`, none for a wordmark. The drawn quad stands
/// this much left of the 64 keyline, so the INK is on it (the corner
/// inset's rule, `fix/chrome-inset-grid`, applied to the design keyline).
#[must_use]
pub fn slot_mark_margin(swat: bool) -> f32 {
    if swat {
        LOGO_H * crate::brand_mark::ARTWORK_SIDE_MARGIN_FRACTION
    } else {
        0.0
    }
}

/// Words that never name an attribution on their own (spec §3.6): a
/// source part made only of these, or of too little else, is never
/// dropped as a duplicate of the tile credit.
const GENERIC_WORDS: &[&str] = &["data", "map", "maps", "source", "sources", "tiles", "tile", "from", "by", "and", "the", "of"];

/// A part's words, lowercased, without `©` and `(c)`.
fn credit_words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .replace('©', " ")
        .replace("(c)", " ")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The source groups less every attribution the tile credit already
/// names (spec §3.6, amended 2026-10-05): the colophon row credits each
/// name once, and the credit's licence-required wording wins.
/// Conservative: a group's text is split only at `;`, `·` and `|` (a name
/// may hold a comma); a part is dropped only when its whole name -- its
/// words without `©`, `(c)` and a trailing "contributors" -- is a
/// contiguous run of the credit's words AND it is specific enough: at
/// least 2 words outside [`GENERIC_WORDS`], or such words of at least 8
/// characters. The kept parts keep the group's own separators; a group
/// left with no text is dropped. Pure: `credit` `None` (or blank) returns
/// `sources` unchanged.
#[must_use]
pub fn dedupe_sources(sources: &[SourceGroup], credit: Option<&str>) -> Vec<SourceGroup> {
    let Some(credit) = credit.map(str::trim).filter(|c| !c.is_empty()) else { return sources.to_vec() };
    let credit = credit_words(credit);
    let named = |part: &str| {
        let mut name = credit_words(part);
        if name.len() > 1 && name.last().is_some_and(|w| w == "contributors") {
            name.pop();
        }
        let significant: Vec<&String> = name.iter().filter(|w| !GENERIC_WORDS.contains(&w.as_str())).collect();
        let specific = significant.len() >= 2 || significant.iter().map(|w| w.chars().count()).sum::<usize>() >= 8;
        specific && credit.windows(name.len()).any(|w| w == name.as_slice())
    };
    sources
        .iter()
        .filter_map(|g| {
            // The text as alternating parts and the separators between them.
            let mut parts: Vec<(&str, &str)> = Vec::new(); // (part, separator before it)
            let mut start = 0;
            let mut sep_before = "";
            for (i, c) in g.text.char_indices() {
                if matches!(c, ';' | '·' | '|') {
                    parts.push((&g.text[start..i], sep_before));
                    // The separator with the spaces around it, as written.
                    let end = i + c.len_utf8();
                    let lead = g.text[..i].len() - g.text[..i].trim_end().len();
                    let trail = g.text[end..].len() - g.text[end..].trim_start().len();
                    sep_before = &g.text[i - lead..end + trail];
                    start = end;
                }
            }
            parts.push((&g.text[start..], sep_before));
            let kept: Vec<(&str, &str)> = parts.iter().map(|(p, s)| (p.trim(), *s)).filter(|(p, _)| !p.is_empty() && !named(p)).collect();
            if kept.len() == parts.iter().filter(|(p, _)| !p.trim().is_empty()).count() {
                return Some(g.clone());
            }
            let mut text = String::new();
            for (i, (p, sep)) in kept.iter().enumerate() {
                if i > 0 {
                    text.push_str(if sep.is_empty() { "; " } else { sep });
                }
                text.push_str(p);
            }
            (!text.is_empty()).then(|| SourceGroup { key: g.key.clone(), text })
        })
        .collect()
}

/// A platform's bands on a frame (spec §5.1), 1080 units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bands {
    pub top_band_bottom: Option<f32>,
    pub bottom_band_top: Option<f32>,
    /// The right rail's left edge, where one applies.
    pub rail_x: Option<f32>,
    /// The rail's y range `(top, bottom)`.
    pub rail_y: Option<(f32, f32)>,
    /// A platform's bottom band (`tiktok`, `reels`, `shorts`, `all`): the
    /// visible region ends at it (§5.6), and `design.credits:
    /// "above_platform"` lifts the sources and credit above it (D2).
    pub lifted: bool,
}

/// `safe_area`'s bands on an `aspect` frame, read off
/// [`safe_area_rects`] (spec §5.1): the top band's bottom is the lowest
/// bottom of a full-width rectangle at y 0 (0 when the frame clips every
/// one away, as 4:5 does under `reels`), the bottom band's top the highest
/// top of a full-width rectangle reaching the bottom edge, and the rail
/// the rectangle on the right edge that is neither. On 9:16 these are the
/// spec table's numbers; on 4:5 the 9:16 rectangles shifted up 285 and
/// clipped. A pair 1a never measured (1:1 with a portrait app) has none.
#[must_use]
pub fn bands(safe_area: SafeArea, aspect: Aspect) -> Bands {
    let none = Bands { top_band_bottom: None, bottom_band_top: None, rail_x: None, rail_y: None, lifted: false };
    let rects = safe_area_rects(safe_area, aspect);
    if rects.is_empty() {
        return none;
    }
    let (w, h) = frame_size(aspect, Tier::Fullhd);
    let (w, h) = (w as f32, h as f32);
    let full = |r: &Rect| r.0 <= 0.0 && r.2 >= w;
    let top = rects.iter().filter(|r| full(r) && r.1 <= 0.0).map(|r| r.3).reduce(f32::max);
    let bottom = rects.iter().filter(|r| full(r) && r.3 >= h).map(|r| r.1).reduce(f32::min);
    let rail = rects.iter().find(|r| !full(r) && r.2 >= w);
    let platform = matches!(safe_area, SafeArea::Tiktok | SafeArea::Reels | SafeArea::Shorts | SafeArea::All);
    Bands {
        top_band_bottom: top.or(platform.then_some(0.0)),
        bottom_band_top: bottom,
        rail_x: rail.map(|r| r.0),
        rail_y: rail.map(|r| (r.1, r.3)),
        lifted: platform && bottom.is_some(),
    }
}

/// The card's (and a lifted fine print's) right limit (spec §3.1).
#[must_use]
pub fn right_limit(frame_w: f32, b: &Bands) -> f32 {
    match b.rail_x {
        Some(rail) => rail - 8.0,
        None => frame_w - CARD_INSET,
    }
}

/// [`right_limit`] for a span `(top, bottom)`: the rail counts only where
/// the span meets its y range (spec §5.1).
fn right_limit_for(frame_w: f32, b: &Bands, span: (f32, f32)) -> f32 {
    match b.rail_y {
        Some((y0, y1)) if span.1 > y0 && span.0 < y1 => right_limit(frame_w, b),
        Some(_) => frame_w - CARD_INSET,
        None => right_limit(frame_w, b),
    }
}

/// The colophon row laid out (spec §3.6): its text runs, its top (the
/// mark's box included when the mark shares the row), the mark's ink
/// top-left (always bottom-left, [`mark_origin`]) and the text column the
/// row's lines start on.
struct FinePrint {
    runs: Vec<TextRun>,
    top: f32,
    mark: (f32, f32),
    /// The text column's left edge: right of the mark on the bottom row,
    /// the 64 keyline when lifted.
    text_x: f32,
    /// The row's right limit: `w - 64`, or the rail's where the row meets it.
    right: f32,
    /// The row sits above a platform's band (`design.credits:
    /// "above_platform"` under a platform safe area).
    lifted: bool,
    warnings: Vec<&'static str>,
}

/// Where the design frame's mark sits (spec §3.6, amended 2026-10-05 by
/// the user: "Logo should always be in bottom left"): its INK's top-left,
/// on the 64 keyline, its bottom on the bottom keyline `h - 64`, on every
/// frame, preset, direction and `design.credits` mode. The one place the
/// mark's position is decided: [`layout_card`]'s `logo` is this.
#[must_use]
pub fn mark_origin(frame_h: f32) -> (f32, f32) {
    (KEYLINE, frame_h - KEYLINE - LOGO_H)
}

/// The colophon row's geometry on `frame` (spec §3.6): its bottom edge,
/// its right limit, its text column and whether it is lifted. On the
/// bottom row (the default, and every frame with no platform band): the
/// bottom keyline `h - 64`, right limit `w - 64` on every preset, the
/// text column 24 clear of the mark's ink.
/// Lifted (`above_platform` under a platform, the old D2): `bottom band
/// top - 12`, from the 64 keyline -- the mark stays bottom-left, below.
fn row_geometry(frame: &FrameSpec, b: &Bands) -> (f32, f32, f32, bool) {
    let (w, h) = (frame.w, frame.h);
    let lh = FINE_PRINT.line_height;
    match b.bottom_band_top.filter(|_| b.lifted && frame.credits == CreditsPlacement::AbovePlatform) {
        Some(band_top) => {
            let bottom = band_top - BAND_GAP;
            (bottom, right_limit_for(w, b, (bottom - 2.0 * lh, bottom)), KEYLINE, true)
        }
        None => {
            // The bottom row is the same on every preset (the platform's
            // UI may cover it in-app; post-description.txt carries the
            // credits): `w - 64`, no rail.
            let text_x = frame.mark_w.map_or(KEYLINE, |mw| KEYLINE + mw + MARK_GAP);
            (h - KEYLINE, w - KEYLINE, text_x, false)
        }
    }
}
/// One piece of a colophon line before it is placed: text, weight, ink
/// and its x offset from the line's start.
type Piece = (String, u16, Ink, f32);

/// The colophon row's lines (spec §3.6, amended 2026-10-05), each at most
/// `max_w` wide: the source groups first (each key in 600, its text in
/// 400; a group never breaks, and one that does not fit on line 2 is not
/// drawn, `sources_too_long`), then the tile credit -- on the same line
/// when it fits whole, else from a new line, broken at words, never
/// dropped (it is the licence credit).
fn colophon_lines(m: &dyn CardMeasure, groups: &[SourceGroup], credit: Option<&str>, max_w: f32) -> (Vec<Vec<Piece>>, Option<String>) {
    let size = FINE_PRINT.size;
    let mut lines: Vec<Vec<Piece>> = vec![Vec::new()];
    let mut x = 0.0_f32;
    let mut dropped: Option<String> = None;
    for g in groups {
        let (key, text) = (g.key.trim(), g.text.trim());
        if text.is_empty() {
            continue;
        }
        let kw = if key.is_empty() { 0.0 } else { m.width(key, size, fine_style(600)) + KEY_GAP };
        let tw = m.width(text, size, fine_style(FINE_PRINT.weight));
        if x > 0.0 && x + GROUP_GAP + kw + tw > max_w {
            if lines.len() == 2 {
                dropped.get_or_insert_with(|| if key.is_empty() { text.to_string() } else { key.to_string() });
                continue;
            }
            lines.push(Vec::new());
            x = 0.0;
        }
        let gap = if x > 0.0 { GROUP_GAP } else { 0.0 };
        let Some(line) = lines.last_mut() else { continue };
        x += gap;
        if !key.is_empty() {
            line.push((key.to_string(), 600, Ink::Ink, x));
            x += kw;
        }
        line.push((text.to_string(), FINE_PRINT.weight, Ink::Ink2, x));
        x += tw;
    }
    if let Some(credit) = credit {
        let whole = m.width(credit, CREDIT.size, fine_style(CREDIT.weight));
        let gap = if x > 0.0 { GROUP_GAP } else { 0.0 };
        if x + gap + whole <= max_w {
            if let Some(line) = lines.last_mut() {
                line.push((credit.to_string(), CREDIT.weight, Ink::Ink2, x + gap));
            }
        } else {
            if x > 0.0 {
                lines.push(Vec::new());
            }
            for (i, text) in crate::story::card_layout::wrap(m, credit, CREDIT.size, fine_style(CREDIT.weight), max_w).into_iter().enumerate() {
                if i > 0 {
                    lines.push(Vec::new());
                }
                if let Some(line) = lines.last_mut() {
                    line.push((text, CREDIT.weight, Ink::Ink2, 0.0));
                }
            }
        }
    }
    lines.retain(|l| !l.is_empty());
    (lines, dropped)
}

/// A fine-print run's style at `weight` (spec §8): tabular, lining
/// figures, so the dates in the sources and the credit (`© 2024`,
/// `Census 2020`) set on one advance. The features touch figures only;
/// the words around them shape as before.
fn fine_style(weight: u16) -> crate::story::design::TextStyle {
    crate::story::design::TextStyle::of_weight(weight).with_features(crate::story::design::TNUM | crate::story::design::LNUM)
}

/// The colophon row (spec §3.6, amended 2026-10-05): the mark
/// bottom-left, then the sources and the tile credit, one type size.
///
/// - **Bottom** (the default; every frame with no platform band): the
///   row's bottom edge is the bottom keyline `h - 64`; the text column is
///   24 clear of the mark's ink, every line on it (a hanging indent); the
///   lines stack upward and the mark is vertically centred on the bottom
///   line's box, its 1-unit overhang past that box reserved at the row's
///   top and bottom, so a one-line row ends on the mark's bottom.
/// - **Lifted** (`design.credits: "above_platform"` under a platform):
///   the sources and credit only, from the 64 keyline, bottom edge `bottom
///   band top - 12` (D2); the mark stays bottom-left, under the band.
fn fine_print(m: &dyn CardMeasure, frame: &FrameSpec, b: &Bands, sources: &[SourceGroup], credit: Option<&str>) -> FinePrint {
    let credit = credit.map(str::trim).filter(|c| !c.is_empty());
    let sources = dedupe_sources(sources, credit);
    let lh = FINE_PRINT.line_height;
    let (row_bottom, right, text_x, lifted) = row_geometry(frame, b);
    let (lines, dropped) = colophon_lines(m, &sources, credit, (right - text_x).max(1.0));
    let n = lines.len();
    // The mark shares the bottom row: its 28 centred on the bottom line's
    // 26 overhangs it by 1 above and below, reserved at both ends.
    let with_mark = !lifted && frame.mark_w.is_some();
    let overhang = if with_mark { ((LOGO_H - lh) / 2.0).max(0.0) } else { 0.0 };
    let text_bottom = row_bottom - overhang;
    let first_top = text_bottom - n as f32 * lh;
    let top = match n {
        0 if with_mark => row_bottom - LOGO_H,
        0 => row_bottom,
        _ => first_top - overhang,
    };
    let mut runs = Vec::new();
    for (i, line) in lines.into_iter().enumerate() {
        let line_top = first_top + i as f32 * lh;
        for (text, weight, ink, dx) in line {
            let style = fine_style(weight);
            let width = m.width(&text, FINE_PRINT.size, style);
            runs.push(TextRun { text, x: text_x + dx, top: line_top, size: FINE_PRINT.size, weight, style, line_height: lh, ink, width });
        }
    }
    let warnings = if dropped.is_some() { vec!["sources_too_long"] } else { Vec::new() };
    FinePrint { runs, top, mark: mark_origin(frame.h), text_x, right, lifted, warnings }
}

/// The first source group [`colophon_lines`] drops for `sources_too_long`
/// (spec §11, R10): `None` when every group (and the tile credit) fits
/// on the colophon row's two lines. [`layout_card`]'s own
/// `sources_too_long` warning only says a drop happened; validation
/// needs the dropped group's own key (or text, with none) to name it in
/// its message, so this reads [`fine_print`]'s own geometry
/// ([`row_geometry`]) rather than widen [`CardLayout`]'s public shape for
/// one caller.
#[must_use]
pub(crate) fn sources_too_long_group(frame: &FrameSpec, sources: &[SourceGroup], credit: Option<&str>, m: &dyn CardMeasure) -> Option<String> {
    let b = bands(frame.safe_area, frame.aspect);
    let credit = credit.map(str::trim).filter(|c| !c.is_empty());
    let sources = dedupe_sources(sources, credit);
    let (_, right, text_x, _) = row_geometry(frame, &b);
    colophon_lines(m, &sources, credit, (right - text_x).max(1.0)).1
}

/// How many text blocks a body truncated (spec §5.7's side-card test).
fn truncations(warnings: &[&'static str]) -> usize {
    warnings.iter().filter(|w| **w == "text_truncated").count()
}

/// The map share (spec §5.6): the share of the region the viewer sees
/// (between the bands under a platform, the whole frame otherwise) where
/// the fade is under half its peak, less the card inside it. Exact: the
/// ramp crosses half at its midpoint.
fn map_share(frame: &FrameSpec, b: &Bands, l: &CardLayout) -> f32 {
    let (vt, vb) = if b.lifted { (b.top_band_bottom.unwrap_or(0.0), b.bottom_band_top.unwrap_or(frame.h)) } else { (0.0, frame.h) };
    if vb <= vt || frame.w <= 0.0 {
        return 1.0;
    }
    let (y0, y1) = l.fade;
    let half = if y1 > y0 { (y0 + y1) / 2.0 } else { y1 };
    // A lifted fade's taper starts 60 below the row, inside the bottom
    // band, so it never reaches the visible region: the ramp alone counts.
    let (a, z) = (vt, half.min(vb).max(vt));
    let card = l.card.map_or(0.0, |c| (c.2 - c.0) * (z.min(c.3) - a.max(c.1)).max(0.0));
    (((z - a) * frame.w - card) / ((vb - vt) * frame.w)).clamp(0.0, 1.0)
}

/// The box the camera centres the story in (spec §5.3, §5.7): *above*
/// the card (or the fine print, with no card) for a bottom or compact
/// card, *beside* a side card. Never inverted.
fn camera_box(frame: &FrameSpec, b: &Bands, anchor: Anchor, card: Option<Rect>, fine_top: f32, floor: f32) -> Rect {
    let top = b.top_band_bottom.map_or(KEYLINE, |t| t + 40.0);
    let (left, right, bottom) = match (anchor, card) {
        // Spec §9: an RTL side card hangs on the right, the box beside it.
        (Anchor::Side, Some(c)) if frame.dir == Dir::Rtl => (KEYLINE, c.0 - 96.0, floor - 24.0),
        (Anchor::Side, Some(c)) => (c.2 + 96.0, frame.w - KEYLINE, floor - 24.0),
        _ => {
            // Spec §5.3: 64, plus the rail's width on the right.
            let right = b.rail_x.map_or(frame.w - KEYLINE, |x| x - KEYLINE);
            (KEYLINE, right, card.map_or(fine_top, |c| c.1) - CAMERA_CLEARANCE)
        }
    };
    (left, top, right.max(left + 1.0), bottom.max(top + 1.0))
}

/// Spec §9 (R14): the LTR layout `l` mirrored for RTL. The card and
/// everything on it mirror about `card_axis` (its LTR left edge plus its
/// right limit, so it hangs from `w - 32`, or 8 left of a rail, with its
/// text right-aligned): the photo lands right of the headline, the counter
/// at the left end, the stats read from the right, the categorical
/// legend's dots on the right and the ramp reversed, so each tick keeps
/// its colour. The colophon row's text mirrors about `fine_axis` (its text
/// column's left edge plus its right limit): the sources start on the
/// right limit, then the credit toward the left, never left of the text
/// column -- so on the bottom row never within 24 of the mark. The mark
/// itself does not mirror: it is bottom-left on every frame (spec §3.6,
/// amended 2026-10-05). Each run keeps its width and is shaped as before;
/// only where it starts moves. Nothing moves vertically, so the floor,
/// the fade and the map share are the LTR ones.
fn mirror(l: &mut CardLayout, card_axis: Option<f32>, fine_axis: f32) {
    let run = |r: &mut TextRun, axis: f32| r.x = axis - r.x - r.width;
    let rect = |r: Rect, axis: f32| (axis - r.2, r.1, axis - r.0, r.3);
    if let Some(axis) = card_axis {
        l.card = l.card.map(|c| rect(c, axis));
        l.hairline = l.hairline.map(|h| rect(h, axis));
        l.runs.iter_mut().chain(l.counter.iter_mut()).for_each(|r| run(r, axis));
        l.swatches.iter_mut().for_each(|(r, _)| *r = rect(*r, axis));
        if let Some((r, _)) = l.photo.as_mut() {
            *r = rect(*r, axis);
        }
        if let Some((r, stops)) = l.ramp.as_mut() {
            *r = rect(*r, axis);
            stops.reverse();
        }
    }
    l.fine.iter_mut().for_each(|r| run(r, fine_axis));
}

/// The pose offset that centres the story in a camera box (spec §5.7),
/// where the camera path solves it (the shells include that module).
pub use crate::story::camera_path::anchor_offset_units;

/// Lays out the frame's design part (spec §3, §5): fine print first, then
/// the card on the floor it leaves (compact or side in landscape, §5.7),
/// then the fade, the map share and the camera box. Pure: identical
/// inputs give an identical layout.
#[must_use]
pub fn layout_card(frame: &FrameSpec, card: Option<&CardContent>, sources: &[SourceGroup], credit: Option<&str>, m: &dyn CardMeasure) -> CardLayout {
    let (w, h) = (frame.w, frame.h);
    let b = bands(frame.safe_area, frame.aspect);
    let fp = fine_print(m, frame, &b, sources, credit);
    // Spec §5.2: the fine print's top is the colophon row's top (the
    // mark's box included on the bottom row), lifted or not; under a
    // platform the band caps the floor too.
    let fine_top = fp.top;
    let mut floor = fine_top - 32.0;
    if let Some(band_top) = b.bottom_band_top {
        floor = floor.min(band_top - BAND_GAP);
    }
    // Lifted (spec §3.7, `above_platform`): a short ramp up to the row,
    // the peak under it and a little below, then a taper, so the map under
    // the band stays visible. On the bottom row: up to the row, held to `h`.
    let (fade, fade_tail) = match b.bottom_band_top.filter(|_| fp.lifted) {
        Some(band_top) => {
            let row_bottom = band_top - BAND_GAP;
            ((fine_top - LIFTED_FADE_LEAD, fine_top), Some(((row_bottom + LIFTED_FADE_HOLD).min(h), LIFTED_FADE_TAIL)))
        }
        None => {
            let lead = if h >= 1350.0 { 160.0 } else { 96.0 };
            ((fine_top - lead, h), None)
        }
    };
    let logo = fp.mark;
    // Spec §9 (R14): the axis the colophon row's text mirrors about under
    // RTL -- its text column to its right limit, clear of the mark.
    let fine_axis = fp.text_x + fp.right;
    let mut card_axis = None;
    let mut out = CardLayout { fine: fp.runs, fine_top, logo, fade, fade_tail, floor, warnings: fp.warnings, ..CardLayout::default() };
    let content = card.filter(|c| !c.is_empty());
    if let Some(content) = content {
        let top_limit = b.top_band_bottom.map_or(264.0, |t| t + 200.0);
        let room = (floor - top_limit).max(0.0);
        let card_right = right_limit_for(w, &b, (top_limit, floor));
        let avail = card_right - CARD_INSET;
        // Too tall for the room above the floor (spec §5.2's top limit):
        // the body drops its item, then its rows (P13).
        let measure = |max_w: f32| measure_card_at(content, max_w, room, CARD_INSET, m);
        let compact = measure(CARD_MAX_W.min(avail));
        let (anchor, body) = if !frame.is_landscape() {
            (Anchor::Bottom, compact)
        } else {
            match frame.anchor {
                AnchorPref::Compact => (Anchor::Compact, compact),
                AnchorPref::Side => (Anchor::Side, measure(SIDE_MAX_W.min(avail))),
                // Spec §5.7: compact up to 200 tall; else side, unless the
                // side card truncates a block the compact one did not.
                AnchorPref::Auto if compact.h <= COMPACT_MAX_H => (Anchor::Compact, compact),
                AnchorPref::Auto => {
                    let side = measure(SIDE_MAX_W.min(avail));
                    if truncations(&side.warnings) > truncations(&compact.warnings) {
                        (Anchor::Compact, compact)
                    } else {
                        (Anchor::Side, side)
                    }
                }
            }
        };
        out.warnings.extend(body.warnings.iter().copied());
        if body.h > 0.0 {
            let left = CARD_INSET;
            let top = floor - body.h;
            out.anchor = anchor;
            out.card = Some((left, top, left + body.w, floor));
            out.hairline = Some((left + CARD_RADIUS * 0.6, top, left + body.w - CARD_RADIUS * 0.6, top + HAIRLINE_H));
            let down = |r: &TextRun| TextRun { top: r.top + top, ..r.clone() };
            let down_rect = |r: Rect| (r.0, r.1 + top, r.2, r.3 + top);
            out.runs = body.runs.iter().map(down).collect();
            out.counter = body.counter.iter().map(down).collect();
            out.swatches = body.swatches.iter().map(|(r, c)| (down_rect(*r), *c)).collect();
            out.photo = body.photo.map(|(r, src)| (down_rect(r), src));
            out.ramp = body.ramp.map(|(r, stops)| (down_rect(r), stops));
            // Spec §3.1: the RTL card's right edge is the right limit.
            card_axis = Some(CARD_INSET + card_right);
        }
    }
    if frame.dir == Dir::Rtl {
        mirror(&mut out, card_axis, fine_axis);
    }
    out.map_share = map_share(frame, &b, &out);
    // Spec §5.6: under 0.75 of the visible map, the card is too tall.
    if out.card.is_some() && out.map_share < 0.75 && !out.warnings.contains(&"card_too_tall") {
        out.warnings.push("card_too_tall");
    }
    // With no card the box stands above the fine print -- and above a
    // platform's band, which may cover a bottom row (spec §5.3).
    let fine_ref = b.bottom_band_top.filter(|_| b.lifted).map_or(fine_top, |t| fine_top.min(t - BAND_GAP));
    out.camera_box = camera_box(frame, &b, out.anchor, out.card, fine_ref, floor);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::story::card_layout::{CardItem, LegendRow, Stat};
    use crate::story::tiers::pixel_ratio;

    /// Every glyph is `0.5 * size` wide: whole, predictable numbers.
    struct Half;
    impl CardMeasure for Half {
        fn width(&self, text: &str, size: f32, _style: crate::story::design::TextStyle) -> f32 {
            text.chars().count() as f32 * size * 0.5
        }
    }

    fn portrait(sa: SafeArea) -> FrameSpec {
        FrameSpec::new((1080.0, 1920.0), sa, Aspect::Portrait)
    }

    fn feed(sa: SafeArea) -> FrameSpec {
        FrameSpec::new((1080.0, 1350.0), sa, Aspect::Feed)
    }

    fn youtube() -> FrameSpec {
        FrameSpec::new((1920.0, 1080.0), SafeArea::Youtube, Aspect::Landscape)
    }

    fn src(key: &str, text: &str) -> SourceGroup {
        SourceGroup { key: key.into(), text: text.into() }
    }

    /// One source line, or (`2`) two groups that cannot share a line on
    /// any frame here (646 + 22 + 456 units > 952).
    fn sources(lines: usize) -> Vec<SourceGroup> {
        match lines {
            0 => Vec::new(),
            1 => vec![src("Data", "PUB Singapore")],
            _ => vec![src("Data", &"x".repeat(60)), src("Route", &"y".repeat(40))],
        }
    }

    const CREDIT_TEXT: &str = "© OpenStreetMap";

    fn card() -> CardContent {
        CardContent { headline: Some("Marina Barrage".into()), ..CardContent::default() }
    }

    fn line_rect(r: &TextRun) -> Rect {
        (r.x, r.top, r.x + r.width, r.top + r.line_height)
    }

    fn intersects(a: Rect, b: Rect) -> bool {
        a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
    }

    #[test]
    fn bands_on_4x5_are_the_9x16_rectangles_shifted_and_clipped() {
        // 9:16 keeps the spec table's numbers.
        let p = bands(SafeArea::All, Aspect::Portrait);
        assert_eq!((p.top_band_bottom, p.bottom_band_top, p.rail_x, p.rail_y, p.lifted), (Some(220.0), Some(1440.0), Some(930.0), Some((700.0, 1540.0)), true));
        // 4:5 shifts every 9:16 rectangle up 285: the top band is clipped
        // away (its bottom is 0), the bottom band starts 285 higher.
        for (sa, band_top, rail) in [(SafeArea::Reels, 1215.0, (940.0, 415.0, 1215.0)), (SafeArea::Tiktok, 1155.0, (930.0, 515.0, 1155.0)), (SafeArea::Shorts, 1255.0, (940.0, 415.0, 1255.0)), (SafeArea::All, 1155.0, (930.0, 415.0, 1255.0))] {
            let b = bands(sa, Aspect::Feed);
            assert_eq!((b.top_band_bottom, b.bottom_band_top, b.rail_x, b.rail_y, b.lifted), (Some(0.0), Some(band_top), Some(rail.0), Some((rail.1, rail.2)), true), "{sa:?}");
        }
        // 1:1 was never measured for a portrait app: no bands, as `none`.
        assert_eq!(bands(SafeArea::Reels, Aspect::Square), bands(SafeArea::None, Aspect::Square));
        assert!(!bands(SafeArea::Youtube, Aspect::Landscape).lifted);
    }

    /// A 173-char credit (1730 units at 20): wider than any row.
    const LONG_CREDIT: &str = "© Mapbox © OpenStreetMap contributors © Maxar Technologies © Esri Community Maps contributors © GEBCO Compilation Group 2025 © Copernicus Sentinel data 2026 Improve this map";

    /// The mark's ink rect, units, on a frame with the SWAT mark.
    fn mark_rect(l: &CardLayout) -> Rect {
        (l.logo.0, l.logo.1, l.logo.0 + DEFAULT_MARK_W, l.logo.1 + LOGO_H)
    }

    /// The colophon row's bottom edge: its lowest text line or the mark.
    fn row_bottom(l: &CardLayout) -> f32 {
        l.fine.iter().map(|r| r.top + r.line_height).fold(mark_rect(l).3, f32::max)
    }

    const PRESETS: [SafeArea; 6] = [SafeArea::None, SafeArea::Youtube, SafeArea::Reels, SafeArea::Tiktok, SafeArea::Shorts, SafeArea::All];
    const PLATFORMS: [SafeArea; 4] = [SafeArea::Reels, SafeArea::Tiktok, SafeArea::Shorts, SafeArea::All];
    const MODES: [CreditsPlacement; 2] = [CreditsPlacement::Bottom, CreditsPlacement::AbovePlatform];

    /// Every aspect's frame, 1080 units.
    fn aspects() -> [(Aspect, (f32, f32)); 4] {
        [(Aspect::Portrait, (1080.0, 1920.0)), (Aspect::Feed, (1080.0, 1350.0)), (Aspect::Square, (1080.0, 1080.0)), (Aspect::Landscape, (1920.0, 1080.0))]
    }

    /// Every preset x aspect x direction x `design.credits`, with a tag.
    fn every_frame() -> Vec<(String, FrameSpec)> {
        let mut out = Vec::new();
        for (aspect, size) in aspects() {
            for sa in PRESETS {
                for dir in [Dir::Ltr, Dir::Rtl] {
                    for credits in MODES {
                        let f = FrameSpec { dir, credits, ..FrameSpec::new(size, sa, aspect) };
                        out.push((format!("{sa:?} {aspect:?} {dir:?} {credits:?}"), f));
                    }
                }
            }
        }
        out
    }

    /// The user's rule (2026-10-05): "Logo should always be in bottom
    /// left". The mark's ink rect is the same on every preset, aspect,
    /// direction and `design.credits` mode -- the non-platform LTR one,
    /// its ink on the 64 keyline, its bottom on `h - 64` -- whatever the
    /// row holds (one line, a wrapped one, none).
    #[test]
    fn the_mark_is_bottom_left_on_every_frame() {
        for (tag, frame) in every_frame() {
            let plain = FrameSpec::new((frame.w, frame.h), SafeArea::None, frame.aspect);
            for (lines, credit) in [(0, None), (1, Some(CREDIT_TEXT)), (2, Some(LONG_CREDIT))] {
                let want = mark_rect(&layout_card(&plain, Some(&card()), &sources(lines), credit, &Half));
                let l = layout_card(&frame, Some(&card()), &sources(lines), credit, &Half);
                assert_eq!(mark_rect(&l), want, "{tag} {lines}: the mark moved");
                assert_eq!(want, (64.0, frame.h - 64.0 - LOGO_H, 64.0 + DEFAULT_MARK_W, frame.h - 64.0), "{tag}: bottom-left on the keylines");
                assert_eq!(l.logo, mark_origin(frame.h), "{tag}: one source of truth");
            }
        }
    }

    /// Default `design.credits` (`bottom`, amended 2026-10-05 by the user):
    /// on every preset the whole row, mark included, sits on the bottom
    /// row exactly as a `none` frame's -- the same runs, the same top --
    /// with the fade held to the bottom edge. Only the card keeps clear of
    /// the platform (its floor is the band's).
    #[test]
    fn by_default_the_row_sits_on_the_bottom_row_on_every_preset() {
        for (aspect, size) in aspects() {
            let plain = layout_card(&FrameSpec::new(size, SafeArea::None, aspect), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
            for sa in PRESETS {
                let tag = format!("{sa:?} {aspect:?}");
                let l = layout_card(&FrameSpec::new(size, sa, aspect), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
                assert_eq!(l.fine, plain.fine, "{tag}: the row is the none frame's");
                assert_eq!((l.fine_top, row_bottom(&l)), (plain.fine_top, size.1 - 64.0), "{tag}");
                assert_eq!(l.fade_tail, None, "{tag}: no lifted taper");
                assert_eq!(l.fade, (l.fine_top - if size.1 >= 1350.0 { 160.0 } else { 96.0 }, size.1), "{tag}: the bottom-edge fade");
                let b = bands(sa, aspect);
                if let (true, Some(band_top)) = (b.lifted, b.bottom_band_top) {
                    assert_eq!(l.floor, (band_top - 12.0).min(l.fine_top - 32.0), "{tag}: the floor follows the band");
                    assert!(l.floor < l.fine_top - 32.0 || sa == SafeArea::Shorts, "{tag}: the band, not the row, sets the floor");
                }
            }
        }
    }

    /// `design.credits: "above_platform"` (the old D2, now opt-in): under
    /// a platform the sources and credit lift to `bottom band top - 12`,
    /// clear of every safe-area rectangle, from the 64 keyline; the mark
    /// stays bottom-left; the fade tapers and the floor follows the row.
    /// With no platform it is `bottom`.
    #[test]
    fn above_platform_lifts_the_credits_and_leaves_the_mark() {
        for sa in PLATFORMS {
            for frame in [portrait(sa), feed(sa)] {
                for credit in [CREDIT_TEXT, LONG_CREDIT] {
                    let frame = FrameSpec { credits: CreditsPlacement::AbovePlatform, ..frame };
                    let tag = format!("{sa:?} {:?} {}-char credit", frame.aspect, credit.chars().count());
                    let l = layout_card(&frame, Some(&card()), &sources(1), Some(credit), &Half);
                    let b = bands(sa, frame.aspect);
                    let band_top = b.bottom_band_top.expect("a bottom band");
                    let rb = l.fine.iter().map(|r| r.top + r.line_height).fold(f32::MIN, f32::max);
                    assert_eq!(rb, band_top - 12.0, "{tag}: the row bottom");
                    assert_eq!(l.fine.iter().map(|r| r.x).fold(f32::MAX, f32::min), 64.0, "{tag}: from the keyline, no mark in the row");
                    assert!(l.fade_tail.is_some(), "{tag}: lifted, the fade tapers");
                    assert_eq!(l.logo, mark_origin(frame.h), "{tag}: the mark stays bottom-left");
                    for r in safe_area_rects(sa, frame.aspect) {
                        for run in &l.fine {
                            assert!(!intersects(line_rect(run), r), "{tag}: {run:?} inside {r:?}");
                        }
                    }
                    let card_rect = l.card.expect("a card");
                    assert_eq!(card_rect.3, l.fine_top - 32.0, "{tag}: the floor follows the row");
                    assert!(card_rect.2 <= right_limit(frame.w, &b), "{tag}: inside the rail");
                }
            }
        }
        // No platform: the option is `bottom`.
        for f in [portrait(SafeArea::None), youtube(), FrameSpec::new((1080.0, 1080.0), SafeArea::Reels, Aspect::Square)] {
            let lifted = layout_card(&FrameSpec { credits: CreditsPlacement::AbovePlatform, ..f }, Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
            assert_eq!(lifted, layout_card(&f, Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half), "{:?} {:?}", f.safe_area, f.aspect);
        }
        assert_eq!(CreditsPlacement::named(Some(" Above_Platform ")), CreditsPlacement::AbovePlatform);
        assert_eq!(CreditsPlacement::named(Some("floating")), CreditsPlacement::Bottom);
        assert_eq!(CreditsPlacement::parse("floating"), None);
        assert_eq!(CreditsPlacement::named(None), CreditsPlacement::Bottom);
    }

    /// Under a platform the CARD clears every safe-area rectangle (bands
    /// and rails) in both modes and directions: only the colophon row may
    /// sit under the band (the default), where post-description.txt
    /// carries the full credits.
    #[test]
    fn the_card_clears_the_platform_band() {
        for (tag, frame) in every_frame() {
            for content in [card(), legend_card()] {
                let l = layout_card(&frame, Some(&content), &sources(2), Some(LONG_CREDIT), &Half);
                let c = l.card.expect("a card");
                let rects = if bands(frame.safe_area, frame.aspect).lifted { safe_area_rects(frame.safe_area, frame.aspect) } else { Vec::new() };
                for r in rects {
                    assert!(!intersects(c, r), "{tag}: the card {c:?} inside {r:?}");
                }
            }
        }
    }

    /// In RTL the row's text mirrors in either mode but never reaches the
    /// fixed bottom-left mark: on the bottom row every run stops 24 or
    /// more right of the mark's ink; lifted, it is above it.
    #[test]
    fn the_rtl_credits_never_overlap_the_mark() {
        for (tag, frame) in every_frame().into_iter().filter(|(_, f)| f.dir == Dir::Rtl) {
            for (lines, credit) in [(1, CREDIT_TEXT), (2, LONG_CREDIT)] {
                let l = layout_card(&frame, Some(&card()), &sources(lines), Some(credit), &Half);
                let m = mark_rect(&l);
                assert!(!l.fine.is_empty(), "{tag}");
                for run in &l.fine {
                    assert!(!intersects(line_rect(run), m), "{tag}: {run:?} over the mark {m:?}");
                    if run.top + run.line_height > m.1 {
                        assert!(run.x >= m.2 + 24.0 - 1e-3, "{tag}: {run:?} within 24 of the mark {m:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn without_a_platform_the_colophon_sits_on_the_bottom_keyline() {
        let square = FrameSpec::new((1080.0, 1080.0), SafeArea::None, Aspect::Square);
        let landscape_none = FrameSpec::new((1920.0, 1080.0), SafeArea::None, Aspect::Landscape);
        for frame in [youtube(), landscape_none, square, portrait(SafeArea::None)] {
            let tag = format!("{:?} {:?}", frame.safe_area, frame.aspect);
            let l = layout_card(&frame, Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
            assert_eq!(row_bottom(&l), frame.h - 64.0, "{tag}: on the bottom keyline");
            assert_eq!(l.fade_tail, None, "{tag}: the fade holds to the bottom edge");
            assert_eq!(l.fade.1, frame.h, "{tag}");
            assert_eq!(mark_rect(&l).0, 64.0, "{tag}");
            assert!(l.fine.iter().all(|r| r.x >= 64.0 + DEFAULT_MARK_W + 24.0 - 1e-3), "{tag}: the text right of the mark");
        }
        // youtube's floor is still the controls band's (960 - 12).
        assert_eq!(layout_card(&youtube(), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half).floor, 948.0);
    }

    #[test]
    fn one_baseline_one_size() {
        for frame in [portrait(SafeArea::Reels), youtube(), feed(SafeArea::All)] {
            let l = layout_card(&frame, Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
            let first = l.fine.iter().map(|r| r.top).fold(f32::MAX, f32::min);
            let line: Vec<&TextRun> = l.fine.iter().filter(|r| r.top == first).collect();
            assert!(line.len() >= 3, "{:?}: key, source and credit share the line: {:?}", frame.aspect, l.fine);
            assert!(l.fine.iter().all(|r| r.size == FINE_PRINT.size && r.line_height == FINE_PRINT.line_height), "{:?}: one size", frame.aspect);
            assert!(line.iter().any(|r| r.text == CREDIT_TEXT), "{:?}: the credit on the sources' line", frame.aspect);
            let m = mark_rect(&l);
            let text_mid = first + FINE_PRINT.line_height / 2.0;
            assert!(((m.1 + m.3) / 2.0 - text_mid).abs() < 1e-3, "{:?}: the mark centred on the line box", frame.aspect);
        }
    }

    #[test]
    fn a_shared_attribution_is_credited_once() {
        let ofm = "OpenFreeMap © OpenMapTiles Data from OpenStreetMap";
        // The source's duplicate goes; the credit's wording stays.
        assert_eq!(dedupe_sources(&[src("Map data", "OpenStreetMap")], Some(ofm)), vec![]);
        assert_eq!(dedupe_sources(&[src("Map data", "OpenStreetMap contributors")], Some("© OpenStreetMap")), vec![]);
        assert_eq!(dedupe_sources(&[src("Map data", "© OpenStreetMap contributors")], Some("Protomaps © OpenStreetMap contributors")), vec![]);
        // A generic word is never a name: "Data" stays beside "Data from OpenStreetMap".
        assert_eq!(dedupe_sources(&[src("Route", "Data")], Some("Data from OpenStreetMap")), vec![src("Route", "Data")]);
        assert_eq!(dedupe_sources(&[src("Data", "Map data")], Some("Map data © OpenStreetMap")), vec![src("Data", "Map data")]);
        // A short specific word is not enough either ("OSM" < 8 chars, one word).
        assert_eq!(dedupe_sources(&[src("Data", "OSM")], Some("© OSM")), vec![src("Data", "OSM")]);
        // A mixed group keeps its own separator, and a comma never splits.
        assert_eq!(dedupe_sources(&[src("Data", "OpenStreetMap · GEBCO 2025")], Some(ofm)), vec![src("Data", "GEBCO 2025")]);
        assert_eq!(dedupe_sources(&[src("Data", "GEBCO | OpenStreetMap | Natural Earth")], Some(ofm)), vec![src("Data", "GEBCO | Natural Earth")]);
        let comma = vec![src("Data", "Land Transport Authority, Singapore")];
        assert_eq!(dedupe_sources(&comma, Some("© Land Transport Authority")), comma, "a name holding a comma is one name");
        // Only whole names: a different name, or no credit, is kept as written.
        let keep = vec![src("Data", "OpenStreetMapper Atlas"), src("Route", "Hand-drawn, illustrative")];
        assert_eq!(dedupe_sources(&keep, Some(ofm)), keep);
        assert_eq!(dedupe_sources(&[src("Map data", "OpenStreetMap")], None), vec![src("Map data", "OpenStreetMap")]);
        assert_eq!(dedupe_sources(&[src("Map data", "OpenStreetMap")], Some("  ")), vec![src("Map data", "OpenStreetMap")]);
        // Through the layout: one "OpenStreetMap" on the row, in the credit.
        let l = layout_card(&portrait(SafeArea::Reels), None, &[src("Map data", "OpenStreetMap"), src("Data", "PUB")], Some(ofm), &Half);
        let osm: Vec<&str> = l.fine.iter().filter(|r| r.text.contains("OpenStreetMap")).map(|r| r.text.as_str()).collect();
        assert_eq!(osm, vec![ofm]);
        assert!(!l.fine.iter().any(|r| r.text == "Map data"));
    }

    #[test]
    fn a_long_colophon_wraps_with_the_mark_on_the_bottom_line() {
        let credit = LONG_CREDIT;
        let text_x = 64.0 + DEFAULT_MARK_W + 24.0;
        let lifted = |f: FrameSpec| FrameSpec { credits: CreditsPlacement::AbovePlatform, ..f };
        // (frame, the text column, the right limit): the bottom row is
        // `w - 64` on every preset; a lifted row starts on the keyline and
        // stops at the rail.
        for (frame, left, right) in [
            (portrait(SafeArea::All), text_x, 1016.0),
            (portrait(SafeArea::Reels), text_x, 1016.0),
            (feed(SafeArea::Tiktok), text_x, 1016.0),
            (portrait(SafeArea::None), text_x, 1016.0),
            (youtube(), text_x, 1856.0),
            (lifted(portrait(SafeArea::All)), 64.0, 922.0),
            (lifted(portrait(SafeArea::Reels)), 64.0, 932.0),
            (lifted(feed(SafeArea::Tiktok)), 64.0, 922.0),
        ] {
            let tag = format!("{:?} {:?} {:?}", frame.safe_area, frame.aspect, frame.credits);
            let l = layout_card(&frame, Some(&card()), &sources(2), Some(credit), &Half);
            let mut tops: Vec<f32> = l.fine.iter().map(|r| r.top).collect();
            tops.sort_by(f32::total_cmp);
            tops.dedup();
            assert!(tops.len() >= 3, "{tag}: wrapped {tops:?}");
            // Every line in the text column, inside the right limit.
            for r in &l.fine {
                assert!(r.x >= left - 1e-3 && r.x + r.width <= right + 1e-3, "{tag}: {r:?}");
            }
            let m = mark_rect(&l);
            assert_eq!(l.logo, mark_origin(frame.h), "{tag}: the mark bottom-left");
            if frame.credits == CreditsPlacement::Bottom {
                // The mark is centred on the bottom line's box.
                let last = tops[tops.len() - 1];
                assert!(((m.1 + m.3) / 2.0 - (last + FINE_PRINT.line_height / 2.0)).abs() < 1e-3, "{tag}: {m:?} vs {tops:?}");
                assert_eq!(l.fine_top, tops[0] - 1.0, "{tag}: the row's top is its first line, less the reserved overhang");
            } else {
                assert_eq!(l.fine_top, tops[0], "{tag}: the lifted row's top is its first line");
                for r in safe_area_rects(frame.safe_area, frame.aspect) {
                    for run in &l.fine {
                        assert!(!intersects(line_rect(run), r), "{tag}: {run:?} inside {r:?}");
                    }
                }
            }
            // Every line pitch is 26; the credit never drops.
            for pair in tops.windows(2) {
                assert_eq!(pair[1] - pair[0], 26.0, "{tag}: {tops:?}");
            }
            let words: usize = l.fine.iter().filter(|r| r.ink == Ink::Ink2 && r.weight == 400).map(|r| r.text.split_whitespace().count()).sum();
            assert!(words >= credit.split_whitespace().count(), "{tag}: every credit word drawn");
            assert_eq!(l.floor.min(l.fine_top - 32.0), l.floor, "{tag}: the floor follows");
        }
        // The slot width as the capture draws it: the SWAT mark's ink at 28
        // tall (its artwork's 12-of-36 side margins off), else the wordmark.
        assert_eq!(slot_mark_w(Some(crate::brand_mark::artwork_size()), Some((100, 20))), Some(DEFAULT_MARK_W));
        assert_eq!(slot_mark_w(None, Some((100, 20))), Some(140.0));
        assert_eq!(slot_mark_w(None, None), None);
        // A wider wordmark in the slot (P3) pushes the text column right.
        let wordmark = FrameSpec { mark_w: Some(400.0), ..portrait(SafeArea::None) };
        let l = layout_card(&wordmark, None, &sources(1), Some(CREDIT_TEXT), &Half);
        assert!(l.fine.iter().all(|r| r.x >= 64.0 + 400.0 + 24.0 - 1e-3));
        // No mark at all: the row starts on the 64 keyline.
        let bare = FrameSpec { mark_w: None, ..portrait(SafeArea::None) };
        let l = layout_card(&bare, None, &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(l.fine.iter().map(|r| r.x).fold(f32::MAX, f32::min), 64.0);
    }

    /// A truncated 3-line headline (48 x 3), a truncated 2-line body
    /// (36 x 2) and a 4-row legend with a title.
    fn too_tall() -> CardContent {
        CardContent {
            headline: Some("word ".repeat(30)),
            body: Some("body ".repeat(40)),
            items: vec![CardItem::Legend { rows: (0..4).map(|i| LegendRow { color: [0, 0, 0], label: format!("c{i}"), value: Some("1".into()) }).collect(), title: Some("t".into()) }],
            ..CardContent::default()
        }
    }

    #[test]
    fn a_card_on_a_small_frame_drops_then_truncates_and_never_inverts() {
        // Review Focus 2. 1:1 with `none`: top limit 264, right limit 1048.
        // 4:5 with `reels`: the top band is clipped away (top limit 200),
        // the bottom band starts at 1215 and the rail (940, 415-1215) caps
        // the card at 932. The card is 324 tall and the map share falls
        // under 0.75 on both (0.66 on 1:1, 0.70 on 4:5 with its credits lifted).
        let square = FrameSpec::new((1080.0, 1080.0), SafeArea::None, Aspect::Square);
        // (4:5 with its credits lifted, `above_platform`, as D2 had them.)
        let feed_lifted = FrameSpec { credits: CreditsPlacement::AbovePlatform, ..feed(SafeArea::Reels) };
        for (f, top_limit, right_limit, share) in [(square, 264.0, 1048.0, 0.6565), (feed_lifted, 200.0, 932.0, 0.7013)] {
            let l = layout_card(&f, Some(&too_tall()), &sources(2), Some(CREDIT_TEXT), &Half);
            let r = l.card.expect("a card");
            assert!(r.0.is_finite() && r.1.is_finite() && r.2.is_finite() && r.3.is_finite(), "{:?}: {r:?}", f.aspect);
            assert!(r.1 < r.3 && r.0 < r.2, "{:?}: inverted {r:?}", f.aspect);
            assert!(r.1 >= top_limit && r.3 <= l.fine_top - 32.0, "{:?}: {r:?} vs fine print top {}", f.aspect, l.fine_top);
            assert!(r.2 <= right_limit, "{:?}: {r:?}", f.aspect);
            assert_eq!(r.3 - r.1, 324.0, "{:?}", f.aspect);
            assert!(l.swatches.is_empty() && !l.runs.iter().any(|t| t.text == "c0"), "{:?}: the legend is dropped", f.aspect);
            assert!(l.warnings.contains(&"card_too_tall"), "{:?}: {:?}", f.aspect, l.warnings);
            assert!(l.runs.iter().any(|t| t.text.ends_with('…')), "{:?}: then the text truncates", f.aspect);
            assert!((l.map_share - share).abs() < 1e-3, "{:?}: {}", f.aspect, l.map_share);
            let b = l.camera_box;
            assert!(b.0 < b.2 && b.1 < b.3, "{:?}: {b:?}", f.aspect);
        }
        // So little room that even row 1 overflows the top limit: the body
        // still comes out finite and the right way up.
        let tiny = FrameSpec::new((1080.0, 480.0), SafeArea::None, Aspect::Square);
        let l = layout_card(&tiny, Some(&too_tall()), &sources(2), Some(CREDIT_TEXT), &Half);
        let r = l.card.expect("a card");
        assert!(r.1 < r.3 && r.0 < r.2 && [r.0, r.1, r.2, r.3, l.map_share].iter().all(|v| v.is_finite()), "{r:?}");
        assert!(l.warnings.contains(&"card_too_tall") && l.warnings.contains(&"text_truncated"), "{:?}", l.warnings);
    }

    #[test]
    fn map_share_counts_only_the_visible_region_on_platform_frames() {
        let l = layout_card(&portrait(SafeArea::All), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
        let c = l.card.unwrap();
        let visible_h = 1440.0 - 220.0;
        // The bottom row's fade reaches half its peak below the band, so
        // the whole visible region counts, less the card.
        let half = (l.fade.0 + 0.5 * (l.fade.1 - l.fade.0)).min(1440.0);
        let want = ((half - 220.0) * 1080.0 - (c.2 - c.0) * (half.min(c.3) - c.1).max(0.0)) / (visible_h * 1080.0);
        assert!((l.map_share - want).abs() < 1e-4, "{} vs {want}", l.map_share);
        assert!(!l.warnings.contains(&"card_too_tall"), "a one-line card leaves the map most of the frame");
        // `none` and `youtube` count the whole frame, the fade to its
        // midpoint; with no card, only the fade.
        for f in [portrait(SafeArea::None), youtube()] {
            let l = layout_card(&f, None, &sources(1), Some(CREDIT_TEXT), &Half);
            let half = (l.fade.0 + l.fade.1) / 2.0;
            assert!((l.map_share - half / f.h).abs() < 1e-4, "{:?}: {}", f.aspect, l.map_share);
        }
    }

    fn legend_card() -> CardContent {
        CardContent {
            headline: Some("Fire".into()),
            items: vec![CardItem::Legend { rows: (0..4).map(|i| LegendRow { color: [0, 0, 0], label: format!("c{i}"), value: None }).collect(), title: None }],
            ..CardContent::default()
        }
    }

    #[test]
    fn landscape_is_compact_up_to_200_then_a_720_side_card() {
        let small = layout_card(&youtube(), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(small.anchor, Anchor::Compact);
        let c = small.card.unwrap();
        assert!(c.3 - c.1 <= 200.0);
        // 30 + 66 + 24 + 80 (a two-row grid) + 32 = 232 tall: side.
        let l = layout_card(&youtube(), Some(&legend_card()), &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(l.anchor, Anchor::Side);
        let c = l.card.unwrap();
        assert!(c.3 - c.1 > 200.0);
        assert_eq!((c.0, c.3), (32.0, l.floor), "still bottom-left on the floor");
        assert!(c.2 - c.0 <= 720.0);
        // A long headline that fits 720 without truncating: the side card
        // is narrower than the compact one would have been.
        let wide = CardContent { headline: Some("Fire season across the northern plains".into()), ..legend_card() };
        let l = layout_card(&youtube(), Some(&wide), &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(l.anchor, Anchor::Side);
        assert!(l.card.unwrap().2 - 32.0 <= 720.0, "{:?}", l.card);
        // The author's choice wins in landscape; portrait is always bottom.
        let compact = FrameSpec { anchor: AnchorPref::Compact, ..youtube() };
        assert_eq!(layout_card(&compact, Some(&legend_card()), &sources(1), None, &Half).anchor, Anchor::Compact);
        let side = FrameSpec { anchor: AnchorPref::Side, ..youtube() };
        assert_eq!(layout_card(&side, Some(&card()), &sources(1), None, &Half).anchor, Anchor::Side);
        let tall = FrameSpec { anchor: AnchorPref::Side, ..portrait(SafeArea::Reels) };
        assert_eq!(layout_card(&tall, Some(&legend_card()), &sources(1), None, &Half).anchor, Anchor::Bottom);
        assert_eq!(AnchorPref::named(Some("side")), AnchorPref::Side);
        assert_eq!(AnchorPref::named(None), AnchorPref::Auto);
    }

    #[test]
    fn landscape_stays_compact_when_side_would_truncate_more() {
        // 84 chars of ordinary words. Compact text width 836: 60 x 2 and
        // 54 x 2 do not fit, 54 x 3 does -> 240 tall, over 200, so the side
        // card is tried. Side text width 656: 48 x 3 holds 27 chars a line
        // and the headline needs 4 lines -> it would truncate a block the
        // compact card did not -> compact.
        let de = CardContent {
            headline: Some("Die Witwe des Kapitäns kehrt nach vierzig Jahren auf der Donau heim nach Wien zurück".into()),
            items: vec![CardItem::Stats(vec![Stat { value: "2.11".into(), unit: Some("km".into()), label: "Strecke".into() }])],
            ..CardContent::default()
        };
        let l = layout_card(&youtube(), Some(&de), &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(l.anchor, Anchor::Compact);
        let heads: Vec<&TextRun> = l.runs.iter().filter(|r| r.weight == 700).collect();
        assert!(heads.iter().all(|r| r.size == 54.0) && heads.len() == 3, "{heads:?}");
        assert!(!heads.iter().any(|r| r.text.ends_with('…')), "compact does not truncate");
        assert!(!l.warnings.contains(&"text_truncated"), "{:?}", l.warnings);
        let c = l.card.unwrap();
        assert!(c.3 - c.1 > 200.0, "taller than 200, so the side card was tried");
        assert!(c.2 - c.0 > 720.0, "the compact width");
    }

    #[test]
    fn the_camera_box_is_above_or_beside_the_card() {
        // Portrait (spec §5.3): top band + 40, 64 in from the left, 64 in
        // from the rail, 120 above the card.
        let l = layout_card(&portrait(SafeArea::Tiktok), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(l.camera_box, (64.0, 160.0 + 40.0, 930.0 - 64.0, l.card.unwrap().1 - 120.0));
        // `none`: 64 from the top; no card: 120 above the fine print.
        let n = layout_card(&portrait(SafeArea::None), None, &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(n.camera_box, (64.0, 64.0, 1080.0 - 64.0, n.fine_top - 120.0));
        // Landscape compact: above (spec §5.7).
        let a = layout_card(&youtube(), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(a.camera_box, (64.0, 140.0, 1920.0 - 64.0, a.card.unwrap().1 - 120.0));
        // Landscape side: beside, 96 right of the card, to 24 above the floor.
        let s = layout_card(&youtube(), Some(&legend_card()), &sources(1), Some(CREDIT_TEXT), &Half);
        let c = s.card.unwrap();
        assert_eq!(s.camera_box, (c.2 + 96.0, 140.0, 1920.0 - 64.0, s.floor - 24.0));
        let (dx, dy) = anchor_offset_units(s.camera_box, (1920.0, 1080.0));
        assert!(dx > 0.0, "a side card moves the subject right: {dx}");
        assert_eq!((dx, dy), ((c.2 + 96.0 + 1856.0) / 2.0 - 960.0, (140.0 + s.floor - 24.0) / 2.0 - 540.0));
        // 4:5 under reels: the clipped top band leaves 0 + 40.
        let f = layout_card(&feed(SafeArea::Reels), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
        assert_eq!(f.camera_box.1, 40.0);
    }

    fn rtl(f: FrameSpec) -> FrameSpec {
        FrameSpec { dir: Dir::Rtl, ..f }
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    /// Headline + body, a counter and a stats pair; and headline + photo.
    fn rtl_cards() -> [CardContent; 2] {
        let stats = vec![Stat { value: "3.4".into(), unit: Some("km".into()), label: "Distance".into() }, Stat { value: "18".into(), unit: None, label: "Minutes".into() }];
        [
            CardContent { headline: Some("Marina Barrage".into()), body: Some("A dam across the channel".into()), index: Some((3, 5)), items: vec![CardItem::Stats(stats)], ..CardContent::default() },
            CardContent {
                headline: Some("The Helix Bridge".into()),
                photo: Some(crate::story::card_layout::Photo { src: "p.png".into(), alt: "a".into(), credit: Some("c".into()) }),
                ..CardContent::default()
            },
        ]
    }

    /// Spec §3.1, §9, §14.2 item 10: with no rail, the RTL card is the LTR
    /// card mirrored about the frame (`w -` its rectangle): the card on the
    /// right, its text right-aligned on `w - 64`, the photo right of the
    /// headline, the counter at the left end, the stats read from the
    /// right, the colophon row's text mirrored on its text column (the mark
    /// stays bottom-left). Nothing moves vertically.
    #[test]
    fn the_rtl_card_is_the_mirror_of_the_ltr_card() {
        let square = FrameSpec::new((1080.0, 1080.0), SafeArea::None, Aspect::Square);
        for frame in [portrait(SafeArea::None), square, youtube()] {
            for card in rtl_cards() {
                let tag = format!("{:?} {:?}", frame.aspect, card.headline);
                let (l, r) = (layout_card(&frame, Some(&card), &sources(1), Some(CREDIT_TEXT), &Half), layout_card(&rtl(frame), Some(&card), &sources(1), Some(CREDIT_TEXT), &Half));
                let w = frame.w;
                let c = l.card.expect("a card");
                assert_eq!(r.card, Some((w - c.2, c.1, w - c.0, c.3)), "{tag}");
                assert_eq!((r.anchor, r.floor, r.fade, r.fade_tail, r.fine_top, &r.warnings), (l.anchor, l.floor, l.fade, l.fade_tail, l.fine_top, &l.warnings), "{tag}");
                assert!(near(r.map_share, l.map_share), "{tag}");
                let flipped = |a: &[TextRun], b: &[TextRun]| {
                    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.text == b.text && a.top == b.top && near(b.x, w - a.x - a.width) && a.width == b.width)
                };
                assert!(flipped(&l.runs, &r.runs), "{tag}: {:?} vs {:?}", l.runs, r.runs);
                assert!(flipped(&l.counter, &r.counter), "{tag}");
                // The row's text mirrors about its text column plus its
                // right limit, so it reads from `w - 64` toward the mark.
                let fine_axis = 64.0 + DEFAULT_MARK_W + 24.0 + (w - 64.0);
                let fine_ok = l.fine.len() == r.fine.len() && l.fine.iter().zip(&r.fine).all(|(a, b)| a.text == b.text && a.top == b.top && near(b.x, fine_axis - a.x - a.width));
                assert!(fine_ok, "{tag}: the colophon row's text mirrors: {:?} vs {:?}", l.fine, r.fine);
                // The text right-aligned on w - 64 (the card's padding).
                let head = r.runs.iter().filter(|t| t.weight == 700 && t.size >= 48.0).collect::<Vec<_>>();
                let rc = r.card.unwrap();
                if card.photo.is_none() {
                    assert!(head.iter().all(|t| near(t.x + t.width, rc.2 - 32.0)), "{tag}: {head:?} on {}", rc.2 - 32.0);
                } else {
                    let p = r.photo.as_ref().expect("a photo").0;
                    assert_eq!(p, (w - l.photo.as_ref().unwrap().0 .2, p.1, w - l.photo.as_ref().unwrap().0 .0, p.3), "{tag}");
                    assert!(near(p.2, rc.2 - 32.0) && head.iter().all(|t| t.x + t.width <= p.0 - 28.0 + 1e-3), "{tag}: the photo right of the headline");
                }
                if !r.counter.is_empty() {
                    assert!(r.counter.iter().all(|k| k.x + k.width <= head.iter().map(|t| t.x).fold(f32::MAX, f32::min)), "{tag}: the counter at the left end");
                    assert!(near(r.counter.iter().map(|k| k.x).fold(f32::MAX, f32::min), rc.0 + 32.0), "{tag}: on the card's left padding");
                }
                if card.items.iter().any(|i| matches!(i, CardItem::Stats(_))) {
                    let x_of = |t: &str| r.runs.iter().find(|x| x.text == t).map(|x| x.x).unwrap_or_else(|| panic!("{tag}: no {t}"));
                    assert!(x_of("3.4") > x_of("18"), "{tag}: the first stat is read first, from the right");
                    assert!(x_of("km") < x_of("3.4"), "{tag}: the unit follows its value, to its left");
                }
                let (lh, rh) = (l.hairline.unwrap(), r.hairline.unwrap());
                assert!(near(rh.0, w - lh.2) && near(rh.2, w - lh.0), "{tag}: {lh:?} {rh:?}");
                // The mark does not mirror: bottom-left, as in LTR.
                assert_eq!(r.logo, l.logo, "{tag}: the mark stays bottom-left");
            }
        }
        // A rail: the card's right edge 8 left of it (spec §3.1).
        let tiktok = layout_card(&rtl(portrait(SafeArea::Tiktok)), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
        let ltr = layout_card(&portrait(SafeArea::Tiktok), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half).card.unwrap();
        let c = tiktok.card.unwrap();
        assert_eq!((c.2, c.2 - c.0, c.1, c.3), (922.0, ltr.2 - ltr.0, ltr.1, ltr.3));
        // A landscape side card hangs on the right, the camera box beside it on the left.
        let side = layout_card(&rtl(youtube()), Some(&legend_card()), &sources(1), Some(CREDIT_TEXT), &Half);
        let c = side.card.unwrap();
        assert_eq!((side.anchor, c.2), (Anchor::Side, 1920.0 - 32.0));
        assert_eq!(side.camera_box, (64.0, 140.0, c.0 - 96.0, side.floor - 24.0));
        assert!(anchor_offset_units(side.camera_box, (1920.0, 1080.0)).0 < 0.0, "the subject moves left");
        // LTR is untouched by the direction field's default.
        assert_eq!(FrameSpec::new((1080.0, 1920.0), SafeArea::None, Aspect::Portrait).dir, Dir::Ltr);
    }

    /// Spec §3.6, §9 (amended 2026-10-05): in RTL the colophon row's text
    /// mirrors in either `design.credits` mode -- the sources start on the
    /// right limit, reading from the right, the tile credit toward the left
    /// end -- while the mark stays bottom-left: on the bottom row the text
    /// stops 24 right of the mark's ink; lifted, it spans the 64 keyline to
    /// the rail.
    #[test]
    fn the_rtl_row_swaps_sources_and_credit_beside_the_fixed_mark() {
        let col = 64.0 + DEFAULT_MARK_W + 24.0;
        let lifted = |f: FrameSpec| FrameSpec { credits: CreditsPlacement::AbovePlatform, ..f };
        for (frame, left, right) in [
            (portrait(SafeArea::Tiktok), col, 1016.0),
            (feed(SafeArea::Reels), col, 1016.0),
            (portrait(SafeArea::None), col, 1016.0),
            (lifted(portrait(SafeArea::Tiktok)), 64.0, 922.0),
            (lifted(feed(SafeArea::Reels)), 64.0, 932.0),
        ] {
            let tag = format!("{:?} {:?} {:?}", frame.safe_area, frame.aspect, frame.credits);
            let ltr = layout_card(&frame, Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
            let l = layout_card(&rtl(frame), Some(&card()), &sources(1), Some(CREDIT_TEXT), &Half);
            let x_of = |t: &str| l.fine.iter().find(|r| r.text == t).unwrap_or_else(|| panic!("{tag}: no {t} in {:?}", l.fine));
            let (key, text, credit) = (x_of("Data"), x_of("PUB Singapore"), x_of(CREDIT_TEXT));
            assert!(near(key.x + key.width, right), "{tag}: the sources start on the right limit");
            assert!(key.x > text.x && text.x > credit.x, "{tag}: key, text, then the credit, read from the right");
            assert!(l.fine.iter().all(|r| r.x >= credit.x), "{tag}: the credit at the left end");
            assert!(l.fine.iter().all(|r| r.x >= left - 1e-3 && r.x + r.width <= right + 1e-3), "{tag}");
            assert_eq!(l.logo, mark_origin(frame.h), "{tag}: the mark bottom-left");
            // One row, one baseline: the vertical layout is the LTR row's.
            assert_eq!((l.fine_top, l.logo, l.floor), (ltr.fine_top, ltr.logo, ltr.floor), "{tag}");
        }
        // A wrapped RTL row: every line right-aligned on the right limit.
        for (frame, right) in [(rtl(portrait(SafeArea::All)), 1016.0), (rtl(lifted(portrait(SafeArea::All))), 922.0)] {
            let l = layout_card(&frame, Some(&card()), &sources(2), Some(LONG_CREDIT), &Half);
            let mut tops: Vec<f32> = l.fine.iter().map(|r| r.top).collect();
            tops.dedup();
            assert!(tops.len() >= 3);
            for t in tops {
                let end = l.fine.iter().filter(|r| r.top == t).map(|r| r.x + r.width).fold(f32::MIN, f32::max);
                assert!(near(end, right), "{:?}: line at {t} ends at {end}, not {right}", frame.credits);
            }
        }
    }

    #[test]
    fn the_1080p_and_4k_layouts_are_equal_in_units() {
        for (aspect, sa) in [(Aspect::Landscape, SafeArea::Youtube), (Aspect::Portrait, SafeArea::Reels), (Aspect::Feed, SafeArea::All), (Aspect::Square, SafeArea::None)] {
            let at = |tier: Tier| {
                let spec = FrameSpec::new(units(frame_size(aspect, tier), pixel_ratio(tier)), sa, aspect);
                layout_card(&spec, Some(&legend_card()), &sources(2), Some(CREDIT_TEXT), &Half)
            };
            let base = at(Tier::Fullhd);
            assert!(base.card.is_some());
            for tier in [Tier::Hd, Tier::Qhd, Tier::Uhd] {
                assert_eq!(at(tier), base, "{aspect:?} {tier:?}");
            }
        }
    }
}
