//! The story's part of the chrome pass (story-content 1a, spec §4.1-§4.2):
//! a frame's panels, its title card and its captions, laid out by
//! [`crate::story::layout::layout`] and turned into plates and text in
//! PHYSICAL pixels.
//!
//! Text is measured for the layout ([`GlyphonMeasure`]) and drawn through
//! the same `LabelShapeCache` entries, so what is drawn is what was
//! measured: the measure shapes with the chrome face, left-justified, at
//! `units * pixel_ratio`, wrapped at the width the layout passed; the draw
//! asks for that size at that wrap again.
//!
//! [`build_story_chrome`] takes no [`CaptureRequest`] (pre-flight ruling
//! F32): the live map (Task 20) and the GPU-free validator (Task 19) call it
//! with their own font state. [`story_chrome`] is the capture's door to it.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

use std::cell::RefCell;

use super::session::FontState;
use super::{CaptureRequest, StoryFrame};
use crate::collision::CollisionBox;
use crate::overlay::paint::TextJustify;
use crate::render::{IconInstance, PlateInstance};
use crate::story::layout::{LayoutItem, LayoutWarning, PanelTheme, Placed, Rect, TextMeasure, TextRole};
use crate::story::script::{Fit, Placement};
use crate::symbol_pass::{chrome_text_face, LabelDraw, LabelShapeCache};

/// An image's texture key: the address of the `Arc<RgbaImage>` it was
/// decoded into (`Arc::as_ptr as usize`; pre-flight ruling F10). Task 12
/// uploads one texture per key.
pub(crate) type ImageKey = usize;

/// The font state the story chrome measures and shapes with, borrowed for
/// one frame. A `RefCell` because [`TextMeasure::measure`] takes `&self`
/// while shaping mutates the cache (ruling F32).
pub(crate) type StoryFonts<'a> = RefCell<(&'a mut glyphon::FontSystem, &'a mut LabelShapeCache)>;

/// A [`TextMeasure`] over the session's glyphon `FontSystem` and
/// `LabelShapeCache`: physical size = `units * pixel_ratio`, measured with
/// `LabelShapeCache::get_or_shape` (the chrome face, left-justified) and
/// divided back by `pixel_ratio`.
pub(crate) struct GlyphonMeasure<'f, 'a> {
    fonts: &'f StoryFonts<'a>,
    pixel_ratio: f32,
}

impl<'f, 'a> GlyphonMeasure<'f, 'a> {
    pub(crate) fn new(fonts: &'f StoryFonts<'a>, pixel_ratio: f32) -> Self {
        Self { fonts, pixel_ratio: sane_ratio(pixel_ratio) }
    }
}

impl TextMeasure for GlyphonMeasure<'_, '_> {
    fn measure(&self, text: &str, size: f32, wrap: Option<f32>) -> (f32, f32) {
        let r = self.pixel_ratio;
        let physical = size * r;
        if !(physical.is_finite() && physical > 0.0) {
            return (0.0, 0.0);
        }
        let wrap = wrap.filter(|w| w.is_finite() && *w >= 0.0).map(|w| w * r);
        let mut fonts = self.fonts.borrow_mut();
        let (system, shapes) = &mut *fonts;
        let shaped = shapes.get_or_shape(system, text, physical, wrap, TextJustify::Left, &chrome_text_face());
        (shaped.measured_width / r, shaped.measured_height / r)
    }
}

/// Runs `f` with a [`GlyphonMeasure`] over `session`'s font state for
/// `label_fonts` at `ratio`: how the player measures a callout's card
/// width (`layout::callout_width`) before any frame, with the faces the
/// frames are drawn with.
pub(crate) fn with_session_measure<R>(
    session: &mut super::CaptureSession,
    label_fonts: &Option<super::LabelFonts>,
    ratio: f32,
    f: impl FnOnce(&dyn TextMeasure) -> R,
) -> R {
    let FontState { system, shapes, .. } = super::session::fonts_for(&mut session.fonts, label_fonts);
    let cell: StoryFonts<'_> = RefCell::new((system, shapes));
    let measure = GlyphonMeasure::new(&cell, ratio);
    f(&measure)
}

/// A font state of its own, with no GPU and no capture session: what
/// validation (Task 19) measures callouts with and lays every sampled
/// frame's chrome out through, with the faces a render draws.
pub(crate) struct LayoutFonts {
    label_fonts: Option<super::LabelFonts>,
    state: Option<FontState>,
}

impl LayoutFonts {
    pub(crate) fn new(label_fonts: Option<super::LabelFonts>) -> Self {
        Self { label_fonts, state: None }
    }

    /// [`with_session_measure`] over this state.
    pub(crate) fn with_measure<R>(&mut self, ratio: f32, f: impl FnOnce(&dyn TextMeasure) -> R) -> R {
        let FontState { system, shapes, .. } = super::session::fonts_for(&mut self.state, &self.label_fonts);
        let cell: StoryFonts<'_> = RefCell::new((system, shapes));
        let measure = GlyphonMeasure::new(&cell, ratio);
        f(&measure)
    }

    /// [`Self::with_measure`]'s design-system twin: a
    /// [`crate::story::card_layout::CardMeasure`] over this state.
    pub(crate) fn with_card_measure<R>(&mut self, ratio: f32, f: impl FnOnce(&dyn crate::story::card_layout::CardMeasure) -> R) -> R {
        let FontState { system, shapes, .. } = super::session::fonts_for(&mut self.state, &self.label_fonts);
        let cell: StoryFonts<'_> = RefCell::new((system, shapes));
        let measure = GlyphonCardMeasure { fonts: &cell, ratio };
        f(&measure)
    }

    /// The design card's footprint (its plate grown by its soft shadow,
    /// [`crate::story::card_layout::card_shadow`]) and every fine-print
    /// line's rect for `request`, physical pixels, exactly as the chrome
    /// pass lays them out -- twice: with every run's `TextStyle`, and with
    /// the weight alone (the measure before R12). What a re-bless's diff
    /// mask (`scripts/story-golden-proof.sh --mask auto`, P14) allows to
    /// move: a card that tracking narrows uncovers its old plate and
    /// shadow, which lie outside the new one.
    #[cfg(test)]
    pub(crate) fn design_rects(&mut self, request: &CaptureRequest) -> Vec<Rect> {
        if request.story.is_none() {
            return Vec::new();
        }
        // The shadow's reach past the plate, per side, 1080 units.
        let unit = (0.0, 0.0, 1000.0, 1000.0);
        let shadow = crate::story::card_layout::card_shadow(unit, crate::story::design::CARD_RADIUS);
        let grow = shadow.iter().fold((0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32), |g, (r, ..)| {
            (g.0.max(-r.0), g.1.max(-r.1), g.2.max(r.2 - 1000.0), g.3.max(r.3 - 1000.0))
        });
        let ratio = sane_ratio(request.pixel_ratio);
        let mut out = Vec::new();
        for unstyled in [false, true] {
            let _guard = UnstyledMeasure::set(unstyled);
            let mut fonts = LayoutFonts::new(self.label_fonts.clone());
            let state = super::session::fonts_for(&mut fonts.state, &fonts.label_fonts);
            let mark = request.effective_brand_mark().map(|_| crate::brand_mark::artwork_size());
            let blockers = super::chrome::layout(request, state, mark, Default::default()).story.map(|s| s.label_blockers).unwrap_or_default();
            for (r, card) in blockers {
                out.push(if card { (r.0 - grow.0 * ratio, r.1 - grow.1 * ratio, r.2 + grow.2 * ratio, r.3 + grow.3 * ratio) } else { r });
            }
        }
        out.dedup();
        out
    }

    /// The design card's own warning codes for `request` (spec §11: the
    /// card layout's `supporting_item_deferred`, `text_shrunk`, ...), laid
    /// out as [`build_story_chrome`] lays it out: what the story golden
    /// test adds to the capture's layout warnings, so an entry's
    /// `EXPECTED_STORY_WARNINGS` row covers its card too (R14). None
    /// without a design part.
    #[cfg(test)]
    pub(crate) fn design_warnings(&mut self, request: &CaptureRequest) -> Vec<&'static str> {
        let Some(d) = request.story.as_deref().and_then(|f| f.design.as_ref()) else { return Vec::new() };
        let state = super::session::fonts_for(&mut self.state, &self.label_fonts);
        let slot = request.slot_mark_w(Some(crate::brand_mark::artwork_size()));
        let fonts = RefCell::new((&mut state.system, &mut state.shapes));
        design_card_layout(d, (request.width, request.height), request.pixel_ratio, slot, &fonts).warnings
    }

    /// The story layout's warnings for `request`, exactly as the chrome
    /// pass lays it out (`chrome::layout`: the credit plate, the SWAT mark
    /// at its vendored artwork's size, the wordmark, then the story clear
    /// of all three) -- no device, no draw.
    pub(crate) fn warnings(&mut self, request: &CaptureRequest) -> Vec<LayoutWarning> {
        if request.story.is_none() {
            return Vec::new();
        }
        let fonts = super::session::fonts_for(&mut self.state, &self.label_fonts);
        let mark = request.effective_brand_mark().map(|_| crate::brand_mark::artwork_size());
        super::chrome::layout(request, fonts, mark, Default::default()).story.map(|s| s.warnings).unwrap_or_default()
    }
}

fn sane_ratio(r: f32) -> f32 {
    if r.is_finite() && r > 0.0 {
        r
    } else {
        1.0
    }
}

/// One frame's story drawing, physical pixels.
#[derive(Default)]
pub(crate) struct StoryChromeDraw {
    /// The title card's full-frame dim (radius 0, black at
    /// `TITLE_DIM * alpha`), drawn first.
    pub dim: Option<PlateInstance>,
    /// One plate per panel card, in placement order.
    pub plates: Vec<PlateInstance>,
    /// The panels' text, through the chrome text renderer.
    pub text: Vec<(LabelDraw, Option<CollisionBox>)>,
    /// One quad per placed image, in placement order, keyed by its
    /// `Arc`'s address (drawn over the panel plates).
    pub quads: Vec<(ImageKey, IconInstance)>,
    /// Each image credit's plate (black at [`IMAGE_CREDIT_PLATE_ALPHA`]),
    /// drawn over the images; its text is in [`Self::text`].
    pub credit_plates: Vec<PlateInstance>,
    /// Each callout's leader line, as two quads of the chrome atlas's line
    /// sprite (`chrome::STORY_LINE`) turned to the leader's angle: first
    /// its underlay in the theme's fill (wider and longer by
    /// [`CALLOUT_HALO_UNITS`] each side), then the line in the theme's
    /// text colour, so it reads on light and dark maps alike. Drawn over
    /// the panel plates, under [`Self::callout_plates`].
    pub leaders: Vec<IconInstance>,
    /// The callouts' own plates, drawn after every leader so a card hides
    /// any other callout's leader that crosses it: each anchor dot's
    /// underlay then the dot, then every card. Their text and images draw
    /// over them.
    pub callout_plates: Vec<PlateInstance>,
    /// The compass needle (spec §4.5), one quad of the chrome atlas's
    /// needle sprite ([`super::chrome::furniture_arrow_sdf`]) turned by
    /// `-bearing`, dark with a light SDF halo; drawn with the leaders. Its
    /// plate is in [`Self::plates`] and its upright "N" in [`Self::text`];
    /// the other furniture is plates and text.
    pub north: Vec<IconInstance>,
    /// The needle's light south half, drawn over [`Self::north`] with the
    /// chrome atlas's south sprite ([`super::chrome::furniture_arrow_south_sdf`]).
    pub north_south: Vec<IconInstance>,
    pub warnings: Vec<LayoutWarning>,
    /// The captions as the layout placed them, 1080 units: the chrome pass
    /// draws them itself, over everything (ids from [`caption_id`]).
    pub captions: Vec<Placed>,
    /// Every placed element's footprint, physical pixels, for basemap label
    /// placement to keep clear of (final review V1): each panel's card (a
    /// title's card, not its dim; a sliding panel's card where it is this
    /// frame as well as where it settles), image, callout card, furniture
    /// piece and caption plate. Not filtered by alpha: a plate fading in or
    /// out is still drawn, so no label may sit under it.
    pub obstacles: Vec<Rect>,
    /// What a story point label may not sit under (spec §7), physical
    /// pixels: the card (`true`) and each colophon-row run (`false`).
    pub label_blockers: Vec<(Rect, bool)>,
}

/// The layout id of `CaptureRequest::captions[i]`: what the player names
/// its `LayoutItem::Caption` items, and what the chrome pass looks a
/// caption's placement up by.
pub(crate) fn caption_id(i: usize) -> String {
    format!("caption/{i}")
}

/// An image credit's text size, 1080 units.
pub(crate) const IMAGE_CREDIT_UNITS: f32 = 28.0;
/// An image credit's plate: black at this alpha (times the element's).
pub(crate) const IMAGE_CREDIT_PLATE_ALPHA: f32 = 0.55;
/// Room between an image credit's plate and its text, 1080 units.
const IMAGE_CREDIT_PAD: (f32, f32) = (8.0, 4.0);

/// Where an `image` (pixels) draws in `rect` under `fit`, and which part
/// of it: `(rect drawn, uv_min, uv_max)`. `Cover` fills `rect`, cropping
/// the image's centre to the rect's aspect; `Contain` shows all of it,
/// shrinking the rect about its centre to the image's aspect (no bars).
pub(crate) fn fit_image(rect: Rect, image: (u32, u32), fit: Fit) -> (Rect, (f32, f32), (f32, f32)) {
    let (rw, rh) = (rect.2 - rect.0, rect.3 - rect.1);
    let (iw, ih) = (image.0 as f32, image.1 as f32);
    if !(rw > 0.0 && rh > 0.0 && iw > 0.0 && ih > 0.0) {
        return (rect, (0.0, 0.0), (1.0, 1.0));
    }
    let (ra, ia) = (rw / rh, iw / ih);
    match fit {
        Fit::Cover if ia > ra => {
            let f = ra / ia;
            (rect, ((1.0 - f) / 2.0, 0.0), ((1.0 + f) / 2.0, 1.0))
        }
        Fit::Cover => {
            let f = ia / ra;
            (rect, (0.0, (1.0 - f) / 2.0), (1.0, (1.0 + f) / 2.0))
        }
        Fit::Contain if ia > ra => {
            let h = rw / ia;
            let top = rect.1 + (rh - h) / 2.0;
            ((rect.0, top, rect.2, top + h), (0.0, 0.0), (1.0, 1.0))
        }
        Fit::Contain => {
            let w = rh * ia;
            let left = rect.0 + (rw - w) / 2.0;
            ((left, rect.1, left + w, rect.3), (0.0, 0.0), (1.0, 1.0))
        }
    }
}

/// `text` shaped on one line at `physical` px, cut at grapheme boundaries
/// and ended with an ellipsis until it is at most `max_w` wide: `(text as
/// drawn, width, height)`, or `None` when not even "…" fits.
fn one_line_within(
    system: &mut glyphon::FontSystem,
    shapes: &mut LabelShapeCache,
    text: &str,
    physical: f32,
    max_w: f32,
) -> Option<(String, f32, f32)> {
    use unicode_segmentation::UnicodeSegmentation;
    let face = chrome_text_face();
    let mut measure = |t: &str| {
        let s = shapes.get_or_shape(system, t, physical, None, TextJustify::Left, &face);
        (s.measured_width, s.measured_height)
    };
    let (w, h) = measure(text);
    if w <= max_w {
        return Some((text.to_string(), w, h));
    }
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    // The longest prefix (trailing spaces dropped) that fits with "…": a
    // binary search, as the width only grows with the prefix.
    let (mut lo, mut hi) = (0usize, graphemes.len());
    let mut best: Option<(String, f32, f32)> = None;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let candidate = format!("{}…", graphemes.iter().take(mid).copied().collect::<String>().trim_end());
        let (cw, ch) = measure(&candidate);
        if cw <= max_w {
            best = Some((candidate, cw, ch));
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    best
}

fn rgba(c: [u8; 4], alpha: f32) -> crate::style::Rgba {
    let [r, g, b, a] = c;
    crate::style::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: f32::from(a) / 255.0 * alpha,
    }
}

/// A title's card: the text stack grown by the theme's padding, kept inside
/// the title's band rectangle. The dim covers the rest of the frame, so the
/// map stays visible (dimmed) around the card (spec §4.1).
fn title_card(placed: &Placed, theme: &PanelTheme) -> Option<Rect> {
    let stack = placed
        .text
        .iter()
        .filter(|(.., text)| !text.is_empty())
        .map(|(_, r, ..)| *r)
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))?;
    let p = theme.padding;
    let band = placed.rect;
    Some(((stack.0 - p).max(band.0), (stack.1 - p).max(band.1), (stack.2 + p).min(band.2), (stack.3 + p).min(band.3)))
}

/// [`StoryChromeDraw::obstacles`]: `placed`'s footprints (1080 units) in
/// physical pixels at `ratio`.
fn footprints(frame: &StoryFrame, placed: &[Placed], ratio: f32, motion: &dyn Fn(&str) -> (f32, f32)) -> Vec<Rect> {
    let physical = |r: Rect| (r.0 * ratio, r.1 * ratio, r.2 * ratio, r.3 * ratio);
    let mut out = Vec::with_capacity(placed.len());
    for p in placed {
        let item = frame.items.iter().find(|i| i.id() == p.id);
        let (rect, placement) = match item {
            Some(LayoutItem::Panel { placement: Placement::Title, theme, .. }) => match title_card(p, theme) {
                Some(card) => (card, None),
                None => continue,
            },
            Some(LayoutItem::Panel { placement, .. } | LayoutItem::Image { placement, .. }) => (p.rect, Some(*placement)),
            _ => (p.rect, None),
        };
        if ![rect.0, rect.1, rect.2, rect.3].iter().all(|v| v.is_finite()) {
            continue;
        }
        out.push(physical(rect));
        // Mid-slide, the card is also where it is drawn this frame.
        if let Some(placement) = placement {
            let (sx, sy) = crate::story::panel::slide_offset(placement, motion(&p.id).1, false);
            if sx != 0.0 || sy != 0.0 {
                out.push(physical((rect.0 + sx, rect.1 + sy, rect.2 + sx, rect.3 + sy)));
            }
        }
    }
    out
}

/// Where the design frame's mark sits (spec §3.6, amended 2026-10-05):
/// its INK top-left, 1080 units, bottom-left on every frame --
/// [`crate::story::frame_layout::layout_card`]'s own `logo`
/// ([`crate::story::frame_layout::mark_origin`]), from the very layout
/// [`build_story_chrome`] draws (the mark depends on neither the card nor
/// the row's text). `credit` is the view's tile credit, used only when the frame
/// carries none (`chrome::LiveCorner::credit`), as the live builder fills
/// it. `None` without a design part. The capture's chrome pass and the
/// live corner both place the mark from here, so they cannot disagree.
pub(crate) fn design_mark_origin(
    frame: &StoryFrame,
    size: (u32, u32),
    ratio: f32,
    slot_mark: Option<f32>,
    credit: Option<&str>,
    fonts: &StoryFonts<'_>,
) -> Option<(f32, f32)> {
    let d = frame.design.as_ref()?;
    let ratio = sane_ratio(ratio);
    let measure = GlyphonCardMeasure { fonts, ratio };
    let spec = d.frame_spec(crate::story::frame_layout::units(size, ratio), slot_mark);
    let credit = d.credit.as_deref().or(credit.filter(|c| !c.is_empty()));
    Some(crate::story::frame_layout::layout_card(&spec, None, &d.sources, credit, &measure).logo)
}

/// Lays `frame` out on a `size` (physical pixels) frame at `ratio`, clear
/// of `obstacles` (the credit plate, the mark and the wordmark, 1080
/// units), and turns it into plates and text in physical pixels.
///
/// - The title card's dim is one full-frame plate, black at
///   `layout.dim * alpha` (the title's alpha);
/// - each panel is one plate in its theme's fill (alpha multiplied by the
///   element's), radius times `ratio`, and its text in the theme's colours
///   at that alpha, moved by `panel::slide_offset` (enter only; a title
///   never slides);
/// - captions are returned placed, for the chrome pass to draw on top.
///
/// Items with no `motion` entry draw at alpha 1, settled. Pure but for the
/// shape cache it fills: identical inputs give identical draws.
pub(crate) fn build_story_chrome(
    frame: &StoryFrame,
    size: (u32, u32),
    ratio: f32,
    obstacles: &[Rect],
    slot_mark: Option<f32>,
    fonts: &StoryFonts<'_>,
) -> StoryChromeDraw {
    let ratio = sane_ratio(ratio);
    // The design system's part first (the slice): its card and fine print
    // are obstacles the 1a items keep clear of.
    let design = frame.design.as_ref().map(|d| (d, design_card_layout(d, size, ratio, slot_mark, fonts)));
    let with_design: Vec<Rect>;
    let obstacles = match &design {
        Some((_, l)) => {
            with_design = obstacles.iter().copied().chain(design_rects(l)).collect();
            &with_design[..]
        }
        None => obstacles,
    };
    let laid = {
        let measure = GlyphonMeasure::new(fonts, ratio);
        crate::story::layout::layout(size, ratio, &frame.safe_area, &frame.items, obstacles, &measure)
    };
    let motion = |id: &str| {
        frame
            .motion
            .iter()
            .find(|(m, ..)| m == id)
            .map_or((1.0, 1.0), |(_, alpha, enter)| (alpha.clamp(0.0, 1.0), enter.clamp(0.0, 1.0)))
    };
    let mut out = StoryChromeDraw { warnings: laid.warnings, ..StoryChromeDraw::default() };
    out.obstacles = footprints(frame, &laid.placed, ratio, &motion);
    if let Some((d, l)) = &design {
        draw_design(d, l, &frame.images, size, ratio, fonts, &mut out);
    }
    if let Some(dim) = laid.dim {
        let title_alpha = frame
            .items
            .iter()
            .filter(|i| matches!(i, LayoutItem::Panel { placement: Placement::Title, .. }))
            .map(|i| motion(i.id()).0)
            .fold(0.0f32, f32::max);
        if title_alpha > 0.0 {
            out.dim = Some(PlateInstance {
                screen_x: 0.0,
                screen_y: 0.0,
                width: size.0 as f32,
                height: size.1 as f32,
                radius_px: 0.0,
                color: [0.0, 0.0, 0.0, dim * title_alpha],
            });
        }
    }
    let face = chrome_text_face();
    // Callout cards, appended after their dots once every item is drawn.
    let mut cards: Vec<PlateInstance> = Vec::new();
    let mut guard = fonts.borrow_mut();
    let (system, shapes) = &mut *guard;
    for placed in &laid.placed {
        if let Some((furniture, piece)) = furniture_of(&frame.items, &placed.id) {
            draw_furniture(placed, piece, furniture, ratio, &mut out, system, shapes);
            continue;
        }
        let Some(item) = frame.items.iter().find(|i| i.id() == placed.id) else { continue };
        match item {
            LayoutItem::Panel { placement, theme, .. } => {
                let (alpha, enter) = motion(&placed.id);
                if alpha <= 0.0 {
                    continue;
                }
                let title = *placement == Placement::Title;
                let (sx, sy) = crate::story::panel::slide_offset(*placement, enter, false);
                let card = if title { title_card(placed, theme) } else { Some(placed.rect) };
                if let Some(c) = card {
                    let [r, g, b, a] = theme.fill;
                    out.plates.push(PlateInstance {
                        screen_x: (c.0 + sx) * ratio,
                        screen_y: (c.1 + sy) * ratio,
                        width: (c.2 - c.0) * ratio,
                        height: (c.3 - c.1) * ratio,
                        radius_px: theme.radius * ratio,
                        color: [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, f32::from(a) / 255.0 * alpha],
                    });
                }
                // The wrap every block was measured at (`Content::place`'s
                // text width: the inner width, or the column beside an image).
                let inner_w = placed.text_wrap.unwrap_or_else(|| (placed.rect.2 - placed.rect.0 - 2.0 * theme.padding).max(0.0));
                let justify = if title { TextJustify::Center } else { TextJustify::Left };
                for (role, rect, units, text) in &placed.text {
                    if text.is_empty() {
                        continue;
                    }
                    let physical = units * ratio;
                    let wrap = Some(inner_w * ratio);
                    shapes.get_or_shape(system, text, physical, wrap, justify, &face);
                    let colour = rgba(if *role == TextRole::Source { theme.secondary } else { theme.text }, alpha);
                    let key = LabelShapeCache::key(text, physical, wrap, justify, &face);
                    let left = ((rect.0 + sx) * ratio).round();
                    let top = ((rect.1 + sy) * ratio).round();
                    out.text.push(((key, left, top, colour, 0.0, colour, 0.0), None));
                }
            }
            LayoutItem::Callout { theme, .. } => {
                let (alpha, _) = motion(&placed.id);
                // A faded callout, or one whose placement is not finite
                // (never expected: the layout skips a non-finite anchor),
                // draws nothing at all -- no plate, no text, no leader.
                if !(alpha.is_finite() && alpha > 0.0 && callout_is_finite(placed)) {
                    continue;
                }
                let c = placed.rect;
                let [r, g, b, a] = theme.fill;
                cards.push(PlateInstance {
                    screen_x: c.0 * ratio,
                    screen_y: c.1 * ratio,
                    width: (c.2 - c.0) * ratio,
                    height: (c.3 - c.1) * ratio,
                    radius_px: theme.radius * ratio,
                    color: [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, f32::from(a) / 255.0 * alpha],
                });
                let ink = rgba(theme.text, alpha);
                // The opposite tone: the card's fill, opaque, at the
                // callout's alpha (review F1).
                let under = rgba([r, g, b, 255], alpha);
                if let Some((anchor, end)) = placed.leader {
                    // The anchor's dot: a round plate (radius half its
                    // side) over a wider one in the opposite tone.
                    for (units, colour) in [(CALLOUT_DOT_UNITS + 2.0 * CALLOUT_HALO_UNITS, under), (CALLOUT_DOT_UNITS, ink)] {
                        let d = units * ratio;
                        out.callout_plates.push(PlateInstance {
                            screen_x: anchor.0 * ratio - d / 2.0,
                            screen_y: anchor.1 * ratio - d / 2.0,
                            width: d,
                            height: d,
                            radius_px: d / 2.0,
                            color: [colour.r, colour.g, colour.b, colour.a],
                        });
                    }
                    out.leaders.extend(leader_quad(anchor, end, ratio, CALLOUT_HALO_UNITS, under, alpha));
                    out.leaders.extend(leader_quad(anchor, end, ratio, 0.0, ink, alpha));
                }
                let inner_w = placed
                    .text_wrap
                    .unwrap_or_else(|| (c.2 - c.0 - 2.0 * crate::story::layout::CALLOUT_PADDING).max(0.0));
                for (_, rect, units, text) in &placed.text {
                    if text.is_empty() {
                        continue;
                    }
                    let physical = units * ratio;
                    let wrap = Some(inner_w * ratio);
                    shapes.get_or_shape(system, text, physical, wrap, TextJustify::Left, &face);
                    let key = LabelShapeCache::key(text, physical, wrap, TextJustify::Left, &face);
                    out.text.push(((key, (rect.0 * ratio).round(), (rect.1 * ratio).round(), ink, 0.0, ink, 0.0), None));
                }
            }
            LayoutItem::Caption { .. } => out.captions.push(placed.clone()),
            // Furniture is drawn piece by piece above (`draw_furniture`).
            LayoutItem::Image { .. } | LayoutItem::Furniture { .. } => {}
        }
        // The image (a standalone one, a panel's or a callout's), its
        // credit over it.
        let (Some(rect), Some(img)) = (placed.image, frame.images.iter().find(|i| i.id == placed.id)) else { continue };
        let (alpha, enter) = motion(&placed.id);
        if !(alpha.is_finite() && alpha > 0.0 && [rect.0, rect.1, rect.2, rect.3].iter().all(|v| v.is_finite())) {
            continue;
        }
        // A callout never slides: it is pinned to its anchor.
        let (sx, sy) = match item {
            LayoutItem::Panel { placement, .. } | LayoutItem::Image { placement, .. } => {
                crate::story::panel::slide_offset(*placement, enter, false)
            }
            _ => (0.0, 0.0),
        };
        let (drawn, uv_min, uv_max) = fit_image(rect, img.image.dimensions(), img.fit);
        let drawn = (drawn.0 + sx, drawn.1 + sy, drawn.2 + sx, drawn.3 + sy);
        out.quads.push((
            std::sync::Arc::as_ptr(&img.image) as ImageKey,
            IconInstance {
                screen_x: drawn.0 * ratio,
                screen_y: drawn.1 * ratio,
                width: (drawn.2 - drawn.0) * ratio,
                height: (drawn.3 - drawn.1) * ratio,
                uv_min,
                uv_max,
                rotation_rad: 0.0,
                sdf: None,
                opacity: alpha,
            },
        ));
        let Some(credit) = img.credit.as_deref().map(str::trim).filter(|c| !c.is_empty()) else { continue };
        // One line, ellipsized to the image's width, its plate inside the
        // image: a long credit on a short image never spills out of it
        // (review F15). An image too small for even that draws no credit
        // on it (the credit plate still names it).
        let (px, py) = IMAGE_CREDIT_PAD;
        let physical = IMAGE_CREDIT_UNITS * ratio;
        let (img_w, img_h) = ((drawn.2 - drawn.0) * ratio, (drawn.3 - drawn.1) * ratio);
        let max_text_w = img_w - 2.0 * px * ratio;
        let Some((text, tw, th)) = one_line_within(system, shapes, credit, physical, max_text_w) else { continue };
        let (pw, ph) = (tw + 2.0 * px * ratio, th + 2.0 * py * ratio);
        if ph > img_h || pw > img_w + 0.5 {
            continue;
        }
        let left = (drawn.0 * ratio).round();
        let top = (drawn.3 * ratio - ph).round().max((drawn.1 * ratio).ceil());
        out.credit_plates.push(PlateInstance {
            screen_x: left,
            screen_y: top,
            width: pw,
            height: ph,
            radius_px: 0.0,
            color: [0.0, 0.0, 0.0, IMAGE_CREDIT_PLATE_ALPHA * alpha],
        });
        let colour = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: alpha };
        let key = LabelShapeCache::key(&text, physical, None, TextJustify::Left, &face);
        out.text.push(((key, left + (px * ratio).round(), top + (py * ratio).round(), colour, 0.0, colour, 0.0), None));
    }
    out.callout_plates.extend(cards);
    out
}

/// A [`crate::story::card_layout::CardMeasure`] over the session's glyphon
/// state: one line, the design face at the run's weight, shaped with the
/// run's whole `TextStyle` (spec §8) -- the very call `draw_design` makes,
/// so the measured run is the drawn one.
pub(crate) struct GlyphonCardMeasure<'f, 'a> {
    pub(crate) fonts: &'f StoryFonts<'a>,
    pub(crate) ratio: f32,
}

/// The design system's face at `weight`: `STORY_FAMILY` (Source Sans 3),
/// bundled native-only by `font_fallback::STORY_FONTS` (spec §4.1, D5).
pub(crate) fn design_face(weight: u16) -> crate::font_fallback::ResolvedFace {
    crate::font_fallback::ResolvedFace { family: crate::font_fallback::STORY_FAMILY.into(), weight, italic: false }
}

/// The design face for `text` at `weight` (spec §4.1, §9): IBM Plex Sans
/// Arabic (`STORY_ARABIC_FAMILY`, bundled beside Source Sans 3) for a run
/// with any Arabic-script letter -- its joining forms, and its own digits
/// and Latin for the isolated runs inside it -- else [`design_face`].
/// Hebrew, Persian-only and Urdu-only letters outside the Arabic block go
/// through the engine's font fallback (§9). Text with no Arabic letter
/// gets exactly `design_face(weight)`, so every LTR run shapes as before.
pub(crate) fn design_face_for(text: &str, weight: u16) -> crate::font_fallback::ResolvedFace {
    use unicode_script::{Script, UnicodeScript as _};
    if text.chars().any(|c| c.script() == Script::Arabic) {
        crate::font_fallback::ResolvedFace { family: crate::font_fallback::STORY_ARABIC_FAMILY.into(), weight, italic: false }
    } else {
        design_face(weight)
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: [`GlyphonCardMeasure`] measures with the
    /// weight alone, as before R12 -- for [`LayoutFonts::design_rects`]'
    /// "before" footprint. R12-specific: R12's base did not print mask
    /// rects, so the head prints the pre-R12 footprint itself; later
    /// tasks get the base's own rects from `story-golden-proof.sh old`.
    /// Set only through [`UnstyledMeasure`].
    static UNSTYLED_MEASURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Sets [`UNSTYLED_MEASURE`] for its lifetime and puts the previous value
/// back on drop -- a panicking layout cannot leave every later measure on
/// this thread unstyled.
#[cfg(test)]
struct UnstyledMeasure(bool);

#[cfg(test)]
impl UnstyledMeasure {
    fn set(on: bool) -> Self {
        Self(UNSTYLED_MEASURE.with(|u| u.replace(on)))
    }
}

#[cfg(test)]
impl Drop for UnstyledMeasure {
    fn drop(&mut self) {
        let previous = self.0;
        UNSTYLED_MEASURE.with(|u| u.set(previous));
    }
}

impl crate::story::card_layout::CardMeasure for GlyphonCardMeasure<'_, '_> {
    fn width(&self, text: &str, size: f32, style: crate::story::design::TextStyle) -> f32 {
        #[cfg(test)]
        let style = if UNSTYLED_MEASURE.with(std::cell::Cell::get) { crate::story::design::TextStyle::of_weight(style.weight) } else { style };
        let r = sane_ratio(self.ratio);
        let physical = size * r;
        if !(physical.is_finite() && physical > 0.0) || text.is_empty() {
            return 0.0;
        }
        let mut fonts = self.fonts.borrow_mut();
        let (system, shapes) = &mut *fonts;
        shapes.get_or_shape_styled(system, text, physical, None, TextJustify::Left, &design_face_for(text, style.weight), style).measured_width / r
    }
}

/// The design layout's footprints (1080 units): the card and every fine
/// print line, for the 1a items and the labels to keep clear of.
pub(crate) fn design_rects(l: &crate::story::card_layout::CardLayout) -> impl Iterator<Item = Rect> + '_ {
    l.card.into_iter().chain(l.fine.iter().map(|r| (r.x, r.top, r.x + r.width, r.top + r.line_height)))
}

/// `d`'s own card layout (spec §3-§3.6, Task R13), at `size` physical
/// pixels and `ratio`: the title card against `d.card` (or `d.from_card`
/// eased by `d.ease` mid card-to-card, spec §3.1), and the colophon row's
/// fine print.
///
/// Factored out of [`build_story_chrome`], which uses it for its own
/// obstacles and label blockers (`design_rects`, [`draw_design`]), so the
/// parity test that checks the obstacle order (spec §7: card, then fine
/// print, grown by 8 each) can read the card's and the fine print's own
/// rectangles directly, independent of how [`StoryChromeDraw::obstacles`]
/// assembles them. Takes the already-unwrapped [`DesignFrame`] (not the
/// `StoryFrame`, which may carry none) so a caller that already knows it
/// has one -- [`build_story_chrome`]'s own `frame.design.as_ref().map`,
/// and the test's -- never needs an `Option` it would just `expect`.
pub(crate) fn design_card_layout(
    d: &crate::story::card_layout::DesignFrame,
    size: (u32, u32),
    ratio: f32,
    slot_mark: Option<f32>,
    fonts: &StoryFonts<'_>,
) -> crate::story::card_layout::CardLayout {
    let ratio = sane_ratio(ratio);
    let measure = GlyphonCardMeasure { fonts, ratio };
    let spec = d.frame_spec(crate::story::frame_layout::units(size, ratio), slot_mark);
    let lay = |card: Option<&crate::story::card_layout::CardContent>| {
        crate::story::frame_layout::layout_card(&spec, card, &d.sources, d.credit.as_deref(), &measure)
    };
    let laid = lay(d.card.as_ref());
    // Card to card (spec §3.1): the plate eases, the content cross-fades.
    match d.from_card.as_ref().filter(|_| d.ease < 1.0) {
        Some(from) => crate::story::card_layout::ease_cards(&lay(Some(from)), &laid, d.ease),
        None => laid,
    }
}

fn rgb_f(c: [u8; 3], alpha: f32) -> [f32; 4] {
    [f32::from(c[0]) / 255.0, f32::from(c[1]) / 255.0, f32::from(c[2]) / 255.0, alpha.clamp(0.0, 1.0)]
}

/// The fade (spec §3.7) as stacked full-width plates of one colour, so the
/// composite is exact: a "layer cake" in optical depth. The profile's
/// `-ln(1 - alpha)` is sliced into levels; each level is one plate spanning
/// every `y` whose depth reaches it, from where the ramp first reaches it
/// to where the taper falls below it (or past the bottom edge, when the
/// held part reaches it there). Levels are taken at the ramp's 2-unit
/// steps, so the rise steps as finely as before, and no two plates share
/// an edge inside the frame (side-by-side strips would seam).
fn fade_plates(l: &crate::story::card_layout::CardLayout, size: (u32, u32), ratio: f32, colour: [u8; 3], peak: f32) -> Vec<PlateInstance> {
    let (y0, y1) = l.fade;
    let (w, h) = (size.0 as f32, size.1 as f32);
    let h_units = h / ratio;
    if !(y0.is_finite() && y1.is_finite()) || y1 <= y0 || !(peak > 0.0) {
        return Vec::new();
    }
    let peak = peak.min(0.999);
    let depth = |y: f32| -(1.0 - crate::story::card_layout::fade_profile(l.fade, l.fade_tail, h_units, y) * peak).ln();
    // The taper side, sampled every 2 units from the tail's start down.
    let tail: Vec<(f32, f32)> = match l.fade_tail {
        Some((from, _)) if from < h_units => {
            let n = (((h_units - from) / 2.0).ceil() as usize).clamp(1, 1024);
            (0..=n).map(|i| from + (h_units - from) * i as f32 / n as f32).map(|y| (y, depth(y))).collect()
        }
        _ => Vec::new(),
    };
    // Where the taper first falls below `level` (interpolated), or `None`
    // when it never does inside the frame.
    let falls_below = |level: f32| -> Option<f32> {
        tail.iter().zip(tail.iter().skip(1)).find(|(_, b)| b.1 < level).map(|(&(ya, da), &(yb, db))| {
            if da > db { ya + (yb - ya) * ((da - level) / (da - db)).clamp(0.0, 1.0) } else { yb }
        })
    };
    let steps = ((y1 - y0) / 2.0).ceil().clamp(1.0, 128.0) as usize;
    let mut out = Vec::with_capacity(steps);
    let mut below = 0.0_f32;
    for i in 0..steps {
        let top = y0 + (y1 - y0) * i as f32 / steps as f32;
        let mid = y0 + (y1 - y0) * (i as f32 + 0.5) / steps as f32;
        let level = depth(mid);
        let slice = level - below;
        if slice <= 0.0 {
            continue;
        }
        let alpha = 1.0 - (-slice).exp();
        let y = top * ratio;
        let bottom = falls_below(level).map_or(h + 2.0, |b| b * ratio);
        if bottom > y {
            out.push(PlateInstance { screen_x: -2.0, screen_y: y, width: w + 4.0, height: bottom - y, radius_px: 0.0, color: rgb_f(colour, alpha) });
        }
        below = level;
    }
    out
}

/// Draws the design layout `l` of `d` into `out` (physical pixels): the
/// fade, the card plate, its hairline, its swatches and ramp, the photo
/// (the frame's image under [`crate::story::card_layout::card_photo_id`],
/// through the 1a image quads; a placeholder plate when it did not load),
/// then its text, the counter and the fine print. Plates go BEFORE the 1a
/// plates, so callouts stay on top.
fn draw_design(
    d: &crate::story::card_layout::DesignFrame,
    l: &crate::story::card_layout::CardLayout,
    images: &[super::StoryImage],
    size: (u32, u32),
    ratio: f32,
    fonts: &StoryFonts<'_>,
    out: &mut StoryChromeDraw,
) {
    use crate::story::card_layout::Ink;
    use crate::story::design::{Tone, FADE_INK_PEAK, FADE_PAPER_PEAK, INK, INK_2, INK_3, INK_CARD_ALPHA, PAPER_ALPHA};
    const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
    let (fade_colour, peak, fill, fill_alpha, track) = match d.tone {
        Tone::Paper => (WHITE, FADE_PAPER_PEAK, WHITE, PAPER_ALPHA, (INK, 0.07)),
        Tone::Ink => (INK, FADE_INK_PEAK, INK, INK_CARD_ALPHA, (WHITE, 0.12)),
    };
    let ink = |i: Ink| -> ([u8; 3], f32) {
        match (d.tone, i) {
            (Tone::Paper, Ink::Ink) => (INK, 1.0),
            (Tone::Paper, Ink::Ink2) => (INK_2, 1.0),
            (Tone::Paper, Ink::Ink3) => (INK_3, 1.0),
            (Tone::Ink, Ink::Ink) => (WHITE, 1.0),
            (Tone::Ink, Ink::Ink2) => (WHITE, 0.80),
            (Tone::Ink, Ink::Ink3) => (WHITE, 0.60),
            (Tone::Paper, Ink::Accent) => (d.accent, 1.0),
            (Tone::Ink, Ink::Accent) => (crate::story::design::lift_accent(d.accent), 1.0),
        }
    };
    let extras = a_of(l.extras_alpha);
    let px = |r: Rect| (r.0 * ratio, r.1 * ratio, (r.2 - r.0) * ratio, (r.3 - r.1) * ratio);
    let mut plates = fade_plates(l, size, ratio, fade_colour, peak);
    let a = d.card_alpha.clamp(0.0, 1.0);
    if let (Some(card), true) = (l.card, a > 0.0) {
        // The soft shadow (spec §3.1), under the plate, faded with it.
        for (r, radius, alpha) in crate::story::card_layout::card_shadow(card, crate::story::design::CARD_RADIUS) {
            let (x, y, w, h) = px(r);
            plates.push(PlateInstance { screen_x: x, screen_y: y, width: w, height: h, radius_px: radius * ratio, color: rgb_f(INK, alpha * a) });
        }
        let (x, y, w, h) = px(card);
        plates.push(PlateInstance { screen_x: x, screen_y: y, width: w, height: h, radius_px: crate::story::design::CARD_RADIUS * ratio, color: rgb_f(fill, fill_alpha * a) });
        if let (Some(hair), Some(p)) = (l.hairline, d.progress) {
            let (x, y, w, h) = px(hair);
            let r = h / 2.0;
            plates.push(PlateInstance { screen_x: x, screen_y: y, width: w, height: h, radius_px: r, color: rgb_f(track.0, track.1 * a) });
            // From the start edge (spec §3.3, §9): the right in RTL.
            let (fx, filled) = crate::story::card_layout::hairline_fill(x, w, p, d.dir);
            if filled > 0.5 {
                let accent = if d.tone == Tone::Ink { crate::story::design::lift_accent(d.accent) } else { d.accent };
                plates.push(PlateInstance { screen_x: fx, screen_y: y, width: filled, height: h, radius_px: r.min(filled / 2.0), color: rgb_f(accent, a) });
            }
        }
        for (i, (rect, colour)) in l.swatches.iter().enumerate() {
            let k = a * l.swatch_alpha.get(i).copied().unwrap_or(1.0);
            if k <= 0.0 {
                continue;
            }
            let (x, y, w, h) = px(*rect);
            plates.push(PlateInstance { screen_x: x, screen_y: y, width: w, height: h, radius_px: w / 2.0, color: rgb_f(*colour, k) });
        }
        if let Some((rect, stops)) = &l.ramp {
            plates.extend(ramp_plates(*rect, stops, ratio, a * extras));
        }
        if let Some((rect, src)) = &l.photo {
            let id = crate::story::card_layout::card_photo_id(src);
            let k = a * extras;
            match images.iter().find(|i| i.id == id) {
                Some(img) if k > 0.0 => {
                    let (drawn, uv_min, uv_max) = fit_image(*rect, img.image.dimensions(), crate::story::script::Fit::Cover);
                    let (x, y, w, h) = px(drawn);
                    out.quads.push((
                        std::sync::Arc::as_ptr(&img.image) as ImageKey,
                        IconInstance { screen_x: x, screen_y: y, width: w, height: h, uv_min, uv_max, rotation_rad: 0.0, sdf: None, opacity: k },
                    ));
                }
                // Not loaded (yet): the photo's square, so the card keeps its shape.
                None if k > 0.0 => {
                    let (x, y, w, h) = px(*rect);
                    plates.push(PlateInstance { screen_x: x, screen_y: y, width: w, height: h, radius_px: crate::story::design::PHOTO_RADIUS * ratio, color: rgb_f(track.0, track.1 * k) });
                }
                _ => {}
            }
        }
    }
    plates.append(&mut out.plates);
    out.plates = plates;
    let mut guard = fonts.borrow_mut();
    let (system, shapes) = &mut *guard;
    let card_runs = l
        .runs
        .iter()
        .enumerate()
        .map(|(i, r)| (r, a * l.run_alpha.get(i).copied().unwrap_or(1.0)))
        .chain(l.counter.iter().map(|r| (r, a * extras)))
        .filter(|(_, a)| *a > 0.0 && l.card.is_some());
    for (run, alpha) in card_runs.chain(l.fine.iter().map(|r| (r, 1.0))) {
        let physical = run.size * ratio;
        if run.text.is_empty() || !(physical.is_finite() && physical > 0.0) {
            continue;
        }
        let face = design_face_for(&run.text, run.style.weight);
        shapes.get_or_shape_styled(system, &run.text, physical, None, TextJustify::Left, &face, run.style);
        let key = LabelShapeCache::key_styled(&run.text, physical, None, TextJustify::Left, &face, run.style);
        let (c, k) = ink(run.ink);
        let colour = crate::style::Rgba { r: f32::from(c[0]) / 255.0, g: f32::from(c[1]) / 255.0, b: f32::from(c[2]) / 255.0, a: k * alpha };
        // The shaped block is `size * LABEL_LINE_HEIGHT_RATIO` tall: centred in the run's line box.
        let top = run.top + (run.line_height - run.size * crate::symbol_pass::LABEL_LINE_HEIGHT_RATIO) / 2.0;
        out.text.push(((key, (run.x * ratio).round(), (top * ratio).round(), colour, 0.0, colour, 0.0), None));
    }
    out.obstacles.extend(design_rects(l).map(|r| (r.0 * ratio, r.1 * ratio, r.2 * ratio, r.3 * ratio)));
    let phys = |r: Rect| (r.0 * ratio, r.1 * ratio, r.2 * ratio, r.3 * ratio);
    out.label_blockers.extend(l.card.map(|c| (phys(c), true)));
    out.label_blockers.extend(l.fine.iter().map(|r| (phys((r.x, r.top, r.x + r.width, r.top + r.line_height)), false)));
}

fn a_of(k: Option<f32>) -> f32 {
    k.map_or(1.0, |k| k.clamp(0.0, 1.0))
}

/// A ramp step's width, 1080 units: the gradient is drawn as plates this
/// wide, each overlapping the next by [`RAMP_OVERLAP_PX`] so no seam shows.
const RAMP_STEP: f32 = 4.0;
const RAMP_OVERLAP_PX: f32 = 2.0;

/// `stops` interpolated (sRGB, as a CSS `linear-gradient`) at `t` in 0..1,
/// the stops at equal spacing (spec §3.4).
fn ramp_colour(stops: &[[u8; 3]], t: f32) -> [u8; 3] {
    let (Some(first), Some(last)) = (stops.first(), stops.last()) else { return [0; 3] };
    if stops.len() == 1 || t <= 0.0 {
        return *first;
    }
    if t >= 1.0 {
        return *last;
    }
    let pos = t * (stops.len() - 1) as f32;
    let i = (pos.floor() as usize).min(stops.len() - 2);
    let f = pos - i as f32;
    let (a, b) = (stops.get(i).copied().unwrap_or(*first), stops.get(i + 1).copied().unwrap_or(*last));
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * f).round().clamp(0.0, 255.0) as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2])]
}

/// The sequential ramp (spec §3.4) as plates, physical pixels: a rounded
/// cap in each end stop (radius 6), then [`RAMP_STEP`]-wide steps between
/// the caps' centres, each in the gradient's colour at its middle. The
/// plate pipeline has no gradient quad yet (spec §6.4, R15).
fn ramp_plates(rect: Rect, stops: &[[u8; 3]], ratio: f32, alpha: f32) -> Vec<PlateInstance> {
    use crate::story::card_layout::RAMP_RADIUS;
    let (Some(first), Some(last)) = (stops.first(), stops.last()) else { return Vec::new() };
    let (x0, x1) = (rect.0, rect.2);
    if !(alpha > 0.0 && x1 - x0 > 2.0 * RAMP_RADIUS && [rect.0, rect.1, rect.2, rect.3].iter().all(|v| v.is_finite())) {
        return Vec::new();
    }
    let (y, h) = (rect.1 * ratio, (rect.3 - rect.1) * ratio);
    let cap = 4.0 * RAMP_RADIUS;
    let mut out = vec![
        PlateInstance { screen_x: x0 * ratio, screen_y: y, width: cap.min(x1 - x0) * ratio, height: h, radius_px: RAMP_RADIUS * ratio, color: rgb_f(*first, alpha) },
        PlateInstance { screen_x: (x1 - cap.min(x1 - x0)) * ratio, screen_y: y, width: cap.min(x1 - x0) * ratio, height: h, radius_px: RAMP_RADIUS * ratio, color: rgb_f(*last, alpha) },
    ];
    let (a, b) = (x0 + RAMP_RADIUS, x1 - RAMP_RADIUS);
    let n = ((b - a) / RAMP_STEP).ceil().clamp(1.0, 512.0) as usize;
    for i in 0..n {
        let (sa, sb) = (a + (b - a) * i as f32 / n as f32, a + (b - a) * (i + 1) as f32 / n as f32);
        let t = ((sa + sb) / 2.0 - x0) / (x1 - x0);
        let overlap = if i + 1 < n { RAMP_OVERLAP_PX } else { 0.0 };
        out.push(PlateInstance { screen_x: sa * ratio, screen_y: y, width: (sb - sa) * ratio + overlap, height: h, radius_px: 0.0, color: rgb_f(ramp_colour(stops, t), alpha) });
    }
    out
}

/// A furniture plate: white at this alpha under [`FURNITURE_INK`], so the
/// source line, the scale bar and the legend read on any map.
pub(crate) const FURNITURE_PLATE: [f32; 4] = [1.0, 1.0, 1.0, 0.85];
/// A furniture plate's corner radius, 1080 units.
pub(crate) const FURNITURE_PLATE_RADIUS: f32 = 4.0;
/// Furniture text, scale bars and the north arrow.
pub(crate) const FURNITURE_INK: [f32; 4] = [0.13, 0.13, 0.13, 1.0];
/// The north arrow's halo (the opposite tone, as a callout leader's
/// underlay), and its width, 1080 units.
pub(crate) const NORTH_HALO: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
pub(crate) const NORTH_HALO_UNITS: f32 = 2.0;
/// The needle's south half: light, rimmed by the dark diamond under it.
pub(crate) const NEEDLE_SOUTH_FILL: [f32; 4] = [0.96, 0.96, 0.96, 1.0];
/// A line swatch's height and a circle swatch's diameter, 1080 units.
pub(crate) const LEGEND_LINE_H: f32 = 6.0;
pub(crate) const LEGEND_CIRCLE: f32 = 16.0;

/// The furniture item `placed` is a piece of, and which piece, when it is one.
fn furniture_of<'a>(items: &'a [LayoutItem], placed_id: &str) -> Option<(&'a crate::story::furniture::FurnitureFrame, &'static str)> {
    items.iter().find_map(|i| {
        let LayoutItem::Furniture { id, frame } = i else { return None };
        let rest = placed_id.strip_prefix(id.as_str())?.strip_prefix('/')?;
        crate::story::layout::FURNITURE_PIECES.iter().find(|p| **p == rest).map(|p| (frame, *p))
    })
}

/// A plate over `r` (1080 units) in `colour`, physical pixels.
fn plate(r: Rect, ratio: f32, radius: f32, colour: [f32; 4]) -> PlateInstance {
    PlateInstance {
        screen_x: r.0 * ratio,
        screen_y: r.1 * ratio,
        width: (r.2 - r.0) * ratio,
        height: (r.3 - r.1) * ratio,
        radius_px: radius * ratio,
        color: colour,
    }
}

fn bytes_colour(c: [u8; 4]) -> [f32; 4] {
    c.map(|v| f32::from(v) / 255.0)
}

/// The compass needle's two quads for a compass box `rect` (1080 units)
/// turned `degrees` clockwise: the whole diamond
/// ([`super::chrome::furniture_arrow_sdf`]) dark with a light halo, then
/// its south half ([`super::chrome::furniture_arrow_south_sdf`], inset)
/// light, so the north half stays dark. Both turn about the needle's pivot,
/// the box's centre.
pub(crate) fn needle_quads(rect: Rect, degrees: f32, ratio: f32) -> Option<(IconInstance, IconInstance)> {
    use super::chrome::{NORTH_ARROW_SDF_ARROW, NORTH_ARROW_SDF_SIZE};
    let side = crate::story::layout::NORTH_ARROW * ratio * NORTH_ARROW_SDF_SIZE as f32 / NORTH_ARROW_SDF_ARROW as f32;
    let (cx, cy) = ((rect.0 + rect.2) / 2.0 * ratio, (rect.1 + rect.3) / 2.0 * ratio);
    if !(side.is_finite() && cx.is_finite() && cy.is_finite() && degrees.is_finite()) {
        return None;
    }
    let quad = |fill: [f32; 4], halo_units: f32| IconInstance {
        screen_x: cx - side / 2.0,
        screen_y: cy - side / 2.0,
        width: side,
        height: side,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        rotation_rad: degrees.to_radians(),
        sdf: Some(crate::render::SdfPaint {
            fill,
            halo: NORTH_HALO,
            halo_width: halo_units * ratio,
            font_scale: side / NORTH_ARROW_SDF_SIZE as f32,
        }),
        opacity: 1.0,
    };
    Some((quad(FURNITURE_INK, NORTH_HALO_UNITS), quad(NEEDLE_SOUTH_FILL, 0.0)))
}

/// Where the compass's upright "N" is centred, 1080 units: on the needle's
/// turned north tip's line, [`crate::story::layout::NORTH_N_RADIUS`] from
/// the box's centre (outside the plate). Only its position turns.
pub(crate) fn north_n_centre(rect: Rect, degrees: f32) -> (f32, f32) {
    let (cx, cy) = ((rect.0 + rect.2) / 2.0, (rect.1 + rect.3) / 2.0);
    let (s, c) = degrees.to_radians().sin_cos();
    let r = crate::story::layout::NORTH_N_RADIUS;
    (cx + r * s, cy - r * c)
}

/// One furniture piece (`placed`, 1080 units) as plates, text and the
/// north arrow's quad, physical pixels: its plate, then (the scale bar) a
/// bar and two end ticks per row under each label, or (the legend) a
/// swatch left of each entry's label; (the compass) a round plate, the
/// two-tone needle and an upright "N" at its turned north tip.
fn draw_furniture(
    placed: &Placed,
    piece: &str,
    furniture: &crate::story::furniture::FurnitureFrame,
    ratio: f32,
    out: &mut StoryChromeDraw,
    system: &mut glyphon::FontSystem,
    shapes: &mut LabelShapeCache,
) {
    use crate::story::furniture::LegendSwatch;
    use crate::story::layout::{FURNITURE_PAD, LEGEND_SWATCH, SCALE_BAR, SCALE_LABEL_GAP, SCALE_TICK, SCALE_TICK_W};
    let r = placed.rect;
    if ![r.0, r.1, r.2, r.3].iter().all(|v| v.is_finite()) {
        return;
    }
    if piece == "north" {
        // On a round plate, like a compass rose: a bare arrowhead turned
        // a few degrees reads as a mouse pointer.
        let Some(deg) = furniture.north_arrow else { return };
        let Some((needle, south)) = needle_quads(r, deg, ratio) else { return };
        let (cx, cy) = ((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0);
        let d = crate::story::layout::NORTH_PLATE / 2.0;
        out.plates.push(plate((cx - d, cy - d, cx + d, cy + d), ratio, d, FURNITURE_PLATE));
        out.north.push(needle);
        out.north_south.push(south);
        // The "N", upright, centred at the turned north tip, outside the
        // plate, dark on a small light halo.
        let face = chrome_text_face();
        let physical = crate::story::layout::NORTH_N_UNITS * ratio;
        let shaped = shapes.get_or_shape(system, "N", physical, None, TextJustify::Left, &face);
        let (w, h) = (shaped.measured_width, shaped.measured_height);
        let (nx, ny) = north_n_centre(r, deg);
        let ink = crate::style::Rgba { r: FURNITURE_INK[0], g: FURNITURE_INK[1], b: FURNITURE_INK[2], a: FURNITURE_INK[3] };
        let halo = crate::style::Rgba { r: NORTH_HALO[0], g: NORTH_HALO[1], b: NORTH_HALO[2], a: NORTH_HALO[3] };
        let key = LabelShapeCache::key("N", physical, None, TextJustify::Left, &face);
        out.text.push(((key, (nx * ratio - w / 2.0).round(), (ny * ratio - h / 2.0).round(), ink, NORTH_HALO_UNITS * ratio, halo, 0.0), None));
        return;
    }
    out.plates.push(plate(r, ratio, FURNITURE_PLATE_RADIUS, FURNITURE_PLATE));
    let x0 = r.0 + FURNITURE_PAD;
    match piece {
        "scale" => {
            let bars = furniture.scale_bar.as_ref().map(|s| s.bars.as_slice()).unwrap_or(&[]);
            for ((_, len), (_, label, ..)) in bars.iter().zip(&placed.text) {
                let len = len.clamp(0.0, (r.2 - r.0 - 2.0 * FURNITURE_PAD - SCALE_TICK_W).max(0.0));
                let top = label.3 + SCALE_LABEL_GAP;
                let bottom = top + SCALE_TICK;
                for rect in [
                    (x0, bottom - SCALE_BAR, x0 + len + SCALE_TICK_W, bottom),
                    (x0, top, x0 + SCALE_TICK_W, bottom),
                    (x0 + len, top, x0 + len + SCALE_TICK_W, bottom),
                ] {
                    out.plates.push(plate(rect, ratio, 0.0, FURNITURE_INK));
                }
            }
        }
        "legend" => {
            let Some(legend) = furniture.legend.as_ref() else { return };
            let titled = legend.title.as_ref().is_some_and(|t| !t.trim().is_empty());
            for ((swatch, _), (_, label, ..)) in legend.entries.iter().zip(placed.text.iter().skip(usize::from(titled))) {
                let cy = (label.1 + label.3) / 2.0;
                let cx = x0 + LEGEND_SWATCH / 2.0;
                let (rect, radius, colour) = match swatch {
                    LegendSwatch::Line(c, _) => ((x0, cy - LEGEND_LINE_H / 2.0, x0 + LEGEND_SWATCH, cy + LEGEND_LINE_H / 2.0), 0.0, *c),
                    LegendSwatch::Fill(c) => ((x0, cy - LEGEND_SWATCH / 2.0, x0 + LEGEND_SWATCH, cy + LEGEND_SWATCH / 2.0), 2.0, *c),
                    LegendSwatch::Circle(c, _) => {
                        let d = LEGEND_CIRCLE / 2.0;
                        ((cx - d, cy - d, cx + d, cy + d), d, *c)
                    }
                };
                out.plates.push(plate(rect, ratio, radius, bytes_colour(colour)));
            }
        }
        _ => {}
    }
    let face = chrome_text_face();
    let ink = crate::style::Rgba { r: FURNITURE_INK[0], g: FURNITURE_INK[1], b: FURNITURE_INK[2], a: FURNITURE_INK[3] };
    for (_, rect, units, text) in &placed.text {
        if text.is_empty() {
            continue;
        }
        let physical = units * ratio;
        shapes.get_or_shape(system, text, physical, None, TextJustify::Left, &face);
        let key = LabelShapeCache::key(text, physical, None, TextJustify::Left, &face);
        out.text.push(((key, (rect.0 * ratio).round(), (rect.1 * ratio).round(), ink, 0.0, ink, 0.0), None));
    }
}

/// A callout's anchor dot, 1080 units across.
pub(crate) const CALLOUT_DOT_UNITS: f32 = 6.0;
/// A callout's leader line, 1080 units thick.
pub(crate) const LEADER_UNITS: f32 = 2.0;
/// How far the opposite-tone underlay of a callout's leader and dot
/// reaches past them on every side, 1080 units (review F1).
pub(crate) const CALLOUT_HALO_UNITS: f32 = 1.5;

/// Whether everything a placed callout draws from is finite.
fn callout_is_finite(placed: &Placed) -> bool {
    let r = placed.rect;
    let leader_ok = placed.leader.is_none_or(|(a, b)| [a.0, a.1, b.0, b.1].iter().all(|v| v.is_finite()));
    [r.0, r.1, r.2, r.3].iter().all(|v| v.is_finite()) && leader_ok
}

/// The leader from `anchor` to `end` (1080 units), grown by `halo` units
/// on every side (0 for the line itself, [`CALLOUT_HALO_UNITS`] for its
/// underlay), as one quad of the line sprite: `LEADER_UNITS + 2 * halo`
/// thick and the leader's length `+ 2 * halo`, centred on its midpoint and
/// turned to its angle (clockwise, y down). The sprite's line fills only
/// its middle rows ([`super::chrome::STORY_LINE_INNER`]), its long edges
/// ramp so a slanted leader is antialiased (review F5), so the quad is
/// that much taller than the line. `None` for a leader too short to see.
pub(crate) fn leader_quad(anchor: (f32, f32), end: (f32, f32), ratio: f32, halo: f32, fill: crate::style::Rgba, alpha: f32) -> Option<IconInstance> {
    let (dx, dy) = ((end.0 - anchor.0) * ratio, (end.1 - anchor.1) * ratio);
    let line = dx.hypot(dy);
    if !(line.is_finite() && line >= 0.5) {
        return None;
    }
    let length = line + 2.0 * halo * ratio;
    let (cx, cy) = ((anchor.0 + end.0) / 2.0 * ratio, (anchor.1 + end.1) / 2.0 * ratio);
    let thick = (LEADER_UNITS + 2.0 * halo) * ratio;
    let sprite_h = super::chrome::STORY_LINE_SIZE as f32;
    let quad_h = thick * sprite_h / super::chrome::STORY_LINE_INNER as f32;
    Some(IconInstance {
        screen_x: cx - length / 2.0,
        screen_y: cy - quad_h / 2.0,
        width: length,
        height: quad_h,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        rotation_rad: dy.atan2(dx),
        sdf: Some(crate::render::SdfPaint {
            // Straight colour: the alpha rides in `opacity`.
            fill: [fill.r, fill.g, fill.b, 1.0],
            halo: [0.0; 4],
            halo_width: 0.0,
            font_scale: quad_h / sprite_h,
        }),
        opacity: alpha.clamp(0.0, 1.0),
    })
}

/// [`build_story_chrome`] for a capture request: nothing without
/// `request.story`. (`chrome::layout` calls the builder itself, with the
/// font state it already holds borrowed apart.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn story_chrome(request: &CaptureRequest, fonts: &mut FontState, chrome_obstacles: &[Rect]) -> StoryChromeDraw {
    let Some(frame) = request.story.as_deref() else { return StoryChromeDraw::default() };
    let FontState { system, shapes, .. } = fonts;
    let cell: StoryFonts<'_> = RefCell::new((system, shapes));
    let slot = request.slot_mark_w(Some(crate::brand_mark::artwork_size()));
    build_story_chrome(frame, (request.width, request.height), request.pixel_ratio, chrome_obstacles, slot, &cell)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noto() -> super::super::session::FontState {
        let f = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/NotoSans-Regular.latin-subset.ttf")).unwrap();
        let mut state = None;
        let fonts: Option<crate::capture::LabelFonts> = Some((vec![f].into(), "Noto Sans".to_string()));
        super::super::session::fonts_for(&mut state, &fonts);
        state.unwrap()
    }

    /// Review F15: a long credit on a short image is one ellipsized line
    /// whose plate stays inside the image; an image too short for a line
    /// carries no credit on it.
    #[test]
    fn a_long_credit_on_a_short_image_stays_inside_it() {
        let long = "Photo: A. Photographer for The Very Long Agency Name, licensed under CC BY-SA 4.0 International".to_string();
        for (aspect, ratio) in [(8.0f32, 1.0f32), (4.0, 2.0), (1.5, 1.0)] {
            let image = std::sync::Arc::new(image::RgbaImage::from_pixel((aspect * 100.0) as u32, 100, image::Rgba([9, 9, 9, 255])));
            let frame = StoryFrame {
                items: vec![LayoutItem::Image { id: "i".into(), placement: Placement::Corner, aspect }],
                images: vec![crate::capture::StoryImage { id: "i".into(), image, fit: Fit::Cover, credit: Some(long.clone()) }],
                ..StoryFrame::default()
            };
            let mut fonts = noto();
            let super::super::session::FontState { system, shapes, .. } = &mut fonts;
            let cell: StoryFonts<'_> = RefCell::new((system, shapes));
            let size = ((1920.0 * ratio) as u32, (1080.0 * ratio) as u32);
            let draw = build_story_chrome(&frame, size, ratio, &[], None, &cell);
            let (_, q) = draw.quads[0];
            let plate = draw.credit_plates[0];
            assert!(plate.screen_x >= q.screen_x && plate.screen_x + plate.width <= q.screen_x + q.width + 0.5, "{aspect}: {plate:?} in {q:?}");
            assert!(plate.screen_y >= q.screen_y && plate.screen_y + plate.height <= q.screen_y + q.height + 0.5, "{aspect}: {plate:?} in {q:?}");
            let ((key, ..), _) = &draw.text[0];
            assert!(key.0.ends_with('…'), "{aspect}: cut to fit: {:?}", key.0);
        }
        // Too short for a line of credit: none on the image.
        let image = std::sync::Arc::new(image::RgbaImage::from_pixel(2000, 100, image::Rgba([9, 9, 9, 255])));
        let frame = StoryFrame {
            items: vec![LayoutItem::Image { id: "i".into(), placement: Placement::Corner, aspect: 20.0 }],
            images: vec![crate::capture::StoryImage { id: "i".into(), image, fit: Fit::Cover, credit: Some(long) }],
            ..StoryFrame::default()
        };
        let mut fonts = noto();
        let super::super::session::FontState { system, shapes, .. } = &mut fonts;
        let cell: StoryFonts<'_> = RefCell::new((system, shapes));
        let draw = build_story_chrome(&frame, (1920, 1080), 1.0, &[], None, &cell);
        assert_eq!(draw.quads.len(), 1);
        assert!(draw.credit_plates.is_empty() && draw.text.is_empty());
    }

    /// Task 19 review F3: a one-line callout's text is the same size at
    /// every anchor. The card was exactly as wide as its text, and the
    /// inner width re-derived from the placed rect (`(x + w) - x`) rounds
    /// an ulp either way with `x`, so glyphon wrapped the last word at
    /// some anchors: `text_shrunk` and a size that popped as the camera
    /// moved. Swept over the shipped faces at 1080p and at hd's ratio.
    #[test]
    fn a_one_line_callout_keeps_its_size_at_every_anchor() {
        use crate::story::layout::{layout, Side};
        let mut fonts = LayoutFonts::new(Some(crate::story::validate::shipped_label_fonts()));
        for ((w, h), ratio) in [((1080u32, 1920u32), 1.0f32), ((720, 1280), 720.0 / 1080.0)] {
            for text in ["Marina Bay Sands", "Gardens by the Bay", "Raffles Place"] {
                let mut sizes = Vec::new();
                let mut x = 100.0f32;
                while x <= 980.0 {
                    let side = if x < 540.0 { Side::Right } else { Side::Left };
                    let item = LayoutItem::Callout {
                        id: "c".into(),
                        anchor: (x, 960.0),
                        text: text.into(),
                        side,
                        image_aspect: None,
                        theme: crate::story::panel::theme(None).unwrap(),
                    };
                    let laid = fonts.with_measure(ratio, |m| layout((w, h), ratio, &[], &[item], &[], m));
                    assert!(laid.warnings.iter().all(|w| w.code != "text_shrunk"), "{text} at x={x} ratio {ratio}: {:?}", laid.warnings);
                    let size = laid.placed[0].text[0].2;
                    sizes.push(size);
                    x += 1.7;
                }
                assert!(sizes.windows(2).all(|p| p[0] == p[1]), "{text} at ratio {ratio}: sizes {sizes:?}");
            }
        }
    }

    /// The frame a player builds for a callout at `t` (`Player::story_frame`'s
    /// callout arm, without the player): nothing when `callout_at` is `None`.
    fn callout_frame(at: Option<crate::story::callout::CalloutAt>, image: Option<std::sync::Arc<image::RgbaImage>>) -> StoryFrame {
        callout_frame_themed(at, image, None)
    }

    fn callout_frame_themed(
        at: Option<crate::story::callout::CalloutAt>,
        image: Option<std::sync::Arc<image::RgbaImage>>,
        theme: Option<&str>,
    ) -> StoryFrame {
        let mut frame = StoryFrame::default();
        let Some(at) = at else { return frame };
        let theme = crate::story::panel::theme(theme).unwrap();
        frame.items.push(LayoutItem::Callout {
            id: "/events/0".into(),
            anchor: at.anchor,
            text: "Marina Bay Sands".into(),
            side: at.side,
            image_aspect: image.as_ref().map(|i| i.width() as f32 / i.height() as f32),
            theme,
        });
        frame.motion.push(("/events/0".into(), at.alpha, 1.0));
        if let Some(image) = image {
            frame.images.push(crate::capture::StoryImage { id: "/events/0".into(), image, fit: Fit::Cover, credit: Some("Photo: Test".into()) });
        }
        frame
    }

    fn draw(frame: &StoryFrame) -> StoryChromeDraw {
        let mut fonts = noto();
        let super::super::session::FontState { system, shapes, .. } = &mut fonts;
        let cell: StoryFonts<'_> = RefCell::new((system, shapes));
        build_story_chrome(frame, (1920, 1080), 1.0, &[], None, &cell)
    }

    fn all_finite(d: &StoryChromeDraw) -> bool {
        let plate = |p: &PlateInstance| [p.screen_x, p.screen_y, p.width, p.height, p.radius_px].iter().chain(&p.color).all(|v| v.is_finite());
        let quad = |q: &IconInstance| [q.screen_x, q.screen_y, q.width, q.height, q.rotation_rad, q.opacity].iter().all(|v| v.is_finite());
        d.plates.iter().chain(&d.credit_plates).chain(&d.callout_plates).chain(&d.dim).all(plate)
            && d.leaders.iter().chain(d.quads.iter().map(|(_, q)| q)).all(quad)
            && d.text.iter().all(|((_, x, y, ..), _)| x.is_finite() && y.is_finite())
    }

    /// Nothing at all is drawn for the callout.
    fn empty(d: &StoryChromeDraw) -> bool {
        d.plates.is_empty() && d.callout_plates.is_empty() && d.leaders.is_empty() && d.text.is_empty() && d.quads.is_empty()
    }

    /// Review Focus 2, the drawing half (the schedule half is
    /// `story::callout::tests`): a callout whose point goes behind the
    /// globe at t = 2 draws its card, dot and leader (each with its
    /// underlay) before, fades them at 2.15, and at t = 3 draws no plate,
    /// no text and no leader. No rect is ever NaN -- also not for a NaN
    /// anchor handed straight to the layout.
    #[test]
    fn a_callout_behind_the_globe_fades_out_and_draws_nothing() {
        use crate::story::callout::{callout_at, schedule};
        let project = |t: f64| if t < 2.0 { Some((500.0, 500.0)) } else { None };
        let s = schedule(0.0, 5.0, 420.0, (1920.0, 1080.0), &project);
        let before = draw(&callout_frame(callout_at(&s, 1.0, 1.9, &project), None));
        assert!(before.plates.is_empty(), "a callout's plates draw after the leaders, not with the panels");
        assert_eq!(before.callout_plates.len(), 3, "the dot's underlay, the dot, the card");
        assert_eq!(before.leaders.len(), 2, "the leader's underlay and the leader");
        assert_eq!(before.text.len(), 1);
        assert_eq!(before.callout_plates[2].color[3], crate::story::panel::theme(None).unwrap().fill[3] as f32 / 255.0);
        assert!(all_finite(&before));
        let fading = draw(&callout_frame(callout_at(&s, 1.0, 2.15, &project), None));
        assert_eq!((fading.callout_plates.len(), fading.leaders.len(), fading.text.len()), (3, 2, 1));
        assert!(fading.leaders.iter().all(|l| (l.opacity - 0.5).abs() <= 0.05), "{:?}", fading.leaders);
        assert!((fading.callout_plates[0].color[3] - 0.5).abs() <= 0.05, "the underlay fades too");
        assert!(all_finite(&fading));
        let gone = draw(&callout_frame(callout_at(&s, 1.0, 3.0, &project), None));
        assert!(empty(&gone));
        // A NaN anchor reaching the layout anyway: skipped with a warning, nothing drawn.
        let nan = callout_frame(Some(crate::story::callout::CalloutAt { anchor: (f32::NAN, 500.0), side: crate::story::layout::Side::Right, alpha: 1.0 }), None);
        let d = draw(&nan);
        assert!(empty(&d));
        assert_eq!(d.warnings.iter().map(|w| w.code).collect::<Vec<_>>(), ["callout_offscreen"]);
    }

    /// Review F1: under the leader and under the dot, drawn first, an
    /// underlay in the opposite tone -- the card's fill, opaque -- wider
    /// by CALLOUT_HALO_UNITS each side, for a dark and a light theme.
    #[test]
    fn a_callouts_leader_and_dot_sit_on_a_wider_underlay_in_the_opposite_tone() {
        let luma = |c: [f32; 4]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        for name in ["dark", "light"] {
            let theme = crate::story::panel::theme(Some(name)).unwrap();
            let ink = rgba(theme.text, 1.0);
            let ink = [ink.r, ink.g, ink.b, ink.a];
            let at = crate::story::callout::CalloutAt { anchor: (900.0, 500.0), side: crate::story::layout::Side::Right, alpha: 1.0 };
            let d = draw(&callout_frame_themed(Some(at), None, Some(name)));
            let (under, line) = (d.leaders[0], d.leaders[1]);
            let (under_paint, line_paint) = (under.sdf.unwrap(), line.sdf.unwrap());
            assert_eq!(line_paint.fill, ink, "{name}: the line in the text colour");
            assert!((luma(under_paint.fill) - luma(ink)).abs() > 0.8, "{name}: opposite tone {:?} vs {ink:?}", under_paint.fill);
            assert_eq!(under_paint.fill[3], 1.0, "{name}: opaque");
            let grow = 2.0 * CALLOUT_HALO_UNITS;
            assert!((under.width - line.width - grow).abs() < 1e-3, "{name}: longer by the halo at both ends");
            let inner = |q: IconInstance| q.height * super::super::chrome::STORY_LINE_INNER as f32 / super::super::chrome::STORY_LINE_SIZE as f32;
            assert!((inner(line) - LEADER_UNITS).abs() < 1e-4 && (inner(under) - LEADER_UNITS - grow).abs() < 1e-4, "{name}: thicker");
            assert_eq!((under.rotation_rad, under.opacity), (line.rotation_rad, line.opacity));
            // The dot: its underlay first, wider, the same centre, the opposite tone.
            let (u, dot) = (d.callout_plates[0], d.callout_plates[1]);
            assert_eq!(dot.color, ink, "{name}");
            assert!((u.width - dot.width - grow).abs() < 1e-4 && (u.height - dot.height - grow).abs() < 1e-4);
            assert!((u.screen_x + u.width / 2.0 - 900.0).abs() < 1e-4 && (dot.screen_x + dot.width / 2.0 - 900.0).abs() < 1e-4);
            assert!((luma(u.color) - luma(ink)).abs() > 0.8 && u.color[3] == 1.0, "{name}: {:?}", u.color);
        }
    }

    /// The line sprite's field crosses the SDF edge one texel in from the
    /// top and the bottom (its inner rows), ramping across the short axis
    /// only (review F5), symmetric about the middle.
    #[test]
    fn the_line_sprite_ramps_across_its_short_axis() {
        use super::super::chrome::{story_line_sprite, STORY_LINE_SIZE};
        let img = story_line_sprite();
        let edge = crate::render::icon_pipeline::SDF_EDGE * 255.0;
        let col: Vec<f32> = (0..STORY_LINE_SIZE).map(|y| f32::from(img.get_pixel(0, y)[3])).collect();
        for x in 0..STORY_LINE_SIZE {
            assert!((0..STORY_LINE_SIZE).all(|y| f32::from(img.get_pixel(x, y)[3]) == col[y as usize]), "the same along every column");
        }
        assert!(col[0] < edge && col[1] > edge, "{col:?}");
        assert!((((col[0] + col[1]) / 2.0) - edge).abs() <= 1.0, "the edge falls between rows 0 and 1: {col:?}");
        assert_eq!(col.iter().rev().copied().collect::<Vec<_>>(), col);
        assert!(col[3] > col[1], "deeper inside towards the middle");
    }

    /// Task 12 review F11: a callout's image draws inside its card, with its
    /// credit on it; the leader runs from the anchor to the card's edge
    /// along the leader's angle, in the theme's text colour.
    #[test]
    fn a_callout_draws_its_card_its_image_with_its_credit_and_a_leader_to_the_card() {
        use crate::story::layout::Side;
        let image = std::sync::Arc::new(image::RgbaImage::from_pixel(300, 200, image::Rgba([9, 99, 9, 255])));
        for side in [Side::Right, Side::Left] {
            let at = crate::story::callout::CalloutAt { anchor: (900.0, 500.0), side, alpha: 1.0 };
            let d = draw(&callout_frame(Some(at), Some(std::sync::Arc::clone(&image))));
            let card = *d.callout_plates.last().unwrap();
            let (_, q) = d.quads[0];
            assert!(q.screen_x >= card.screen_x && q.screen_x + q.width <= card.screen_x + card.width + 0.5, "{q:?} in {card:?}");
            assert!(q.screen_y >= card.screen_y && q.screen_y + q.height <= card.screen_y + card.height + 0.5, "{q:?} in {card:?}");
            assert_eq!(d.credit_plates.len(), 1, "the image's credit");
            assert_eq!(d.text.len(), 2, "the label and the credit");
            let leader = d.leaders[1];
            let paint = leader.sdf.expect("an SDF quad");
            assert_eq!(paint.fill, [1.0, 1.0, 1.0, 1.0], "the dark theme's text colour");
            // The leader's midpoint is halfway to the card edge it meets.
            let (mx, my) = (leader.screen_x + leader.width / 2.0, leader.screen_y + leader.height / 2.0);
            let edge = if side == Side::Right { card.screen_x } else { card.screen_x + card.width };
            assert!((mx - (900.0 + edge) / 2.0).abs() < 0.5 && (my - 500.0).abs() < 0.5, "{side:?}: {mx},{my} edge {edge}");
            assert!((leader.width - (edge - 900.0).abs()).abs() < 0.5);
            assert!(all_finite(&d));
        }
        // A leader at an angle is turned to it: from (0, 0) to (30, 40), 50 long, at atan2(40, 30).
        let white = rgba([255; 4], 1.0);
        let q = leader_quad((0.0, 0.0), (30.0, 40.0), 2.0, 0.0, white, 1.0).unwrap();
        assert!((q.width - 100.0).abs() < 1e-4 && (q.rotation_rad - 40f32.atan2(30.0)).abs() < 1e-6);
        assert!((q.screen_x + q.width / 2.0 - 30.0).abs() < 1e-4 && (q.screen_y + q.height / 2.0 - 40.0).abs() < 1e-4);
        assert!(leader_quad((1.0, 1.0), (1.0, 1.0), 1.0, 0.0, white, 1.0).is_none(), "too short to see");
        assert!(leader_quad((f32::NAN, 1.0), (1.0, 1.0), 1.0, 0.0, white, 1.0).is_none());
    }

    /// Spec §4.5: every furniture piece draws at any tier the same in
    /// 1080 units -- the scale bar's ticks exactly its length apart, one
    /// swatch per legend entry in its colour, the north arrow turned by
    /// `-bearing` with a light halo under dark ink -- and nothing meets the
    /// credit plate.
    #[test]
    fn furniture_draws_its_plates_bars_swatches_text_and_a_turned_arrow() {
        use crate::story::furniture::{Legend, LegendSwatch, ScaleBar};
        let furniture = crate::story::furniture::FurnitureFrame {
            source_line: Some("Source: Port authority".into()),
            byline: Some("By A. Reporter".into()),
            scale_bar: Some(ScaleBar { bars: vec![("10 km".into(), 130.83), ("5 mi".into(), 105.3)] }),
            legend: Some(Legend {
                title: Some("Vessel".into()),
                entries: vec![
                    (LegendSwatch::Line([31, 119, 180, 255], 3.0), "ferry".into()),
                    (LegendSwatch::Fill([228, 87, 46, 255]), "cargo".into()),
                    (LegendSwatch::Circle([153, 153, 153, 255], 5.0), "Other".into()),
                ],
            }),
            north_arrow: Some(30.0),
            scale_bar_hidden_pitch: None,
        };
        let frame = StoryFrame { items: vec![LayoutItem::Furniture { id: "furniture".into(), frame: furniture }], ..StoryFrame::default() };
        for ratio in [1.0f32, 2.0] {
            let (w, h) = (1920.0, 1080.0);
            let credit = (w - 416.0, h - 36.0, w - 16.0, h - 16.0);
            let mut fonts = noto();
            let super::super::session::FontState { system, shapes, .. } = &mut fonts;
            let cell: StoryFonts<'_> = RefCell::new((system, shapes));
            let d = build_story_chrome(&frame, ((w * ratio) as u32, (h * ratio) as u32), ratio, &[credit], None, &cell);
            assert!(d.warnings.is_empty(), "{:?}", d.warnings);
            // 3 piece plates and the arrow's disc; 2 bars x (bar + 2 ticks); 3 swatches.
            assert_eq!(d.plates.len(), 4 + 6 + 3, "ratio {ratio}");
            // 2 source lines, 2 scale labels, the legend's title and 3 entries, the compass's "N".
            assert_eq!(d.text.len(), 9);
            let units = |p: &PlateInstance| (p.screen_x / ratio, p.screen_y / ratio, (p.screen_x + p.width) / ratio, (p.screen_y + p.height) / ratio);
            for p in &d.plates {
                assert!(!crate::story::layout::test_support::intersects(units(p), credit), "{p:?}");
            }
            // The first bar's ticks (plates 2 and 3 after the source and scale plates' order: source, scale, bar, tick, tick).
            let ticks: Vec<(f32, f32, f32, f32)> = d.plates.iter().filter(|p| (p.width / ratio - crate::story::layout::SCALE_TICK_W).abs() < 1e-3).map(units).collect();
            assert_eq!(ticks.len(), 4);
            assert!((ticks[1].0 - ticks[0].0 - 130.83).abs() < 1e-3, "{ticks:?}");
            assert!((ticks[3].0 - ticks[2].0 - 105.3).abs() < 1e-3, "{ticks:?}");
            // Each swatch in its own colour: a 24 x 6 line, a 24 x 24 fill, a 16-unit circle.
            let swatch = |c: [u8; 4]| d.plates.iter().find(|p| p.color == c.map(|v| f32::from(v) / 255.0)).map(units).unwrap();
            let line = swatch([31, 119, 180, 255]);
            assert!(((line.2 - line.0) - 24.0).abs() < 1e-3 && ((line.3 - line.1) - 6.0).abs() < 1e-3);
            let fill = swatch([228, 87, 46, 255]);
            assert!(((fill.2 - fill.0) - 24.0).abs() < 1e-3 && ((fill.3 - fill.1) - 24.0).abs() < 1e-3);
            let circle = d.plates.iter().find(|p| p.color == [153.0 / 255.0; 3].into_iter().chain([1.0]).collect::<Vec<_>>()[..]).unwrap();
            assert!((circle.width / ratio - 16.0).abs() < 1e-3 && (circle.radius_px - circle.width / 2.0).abs() < 1e-3);
            // The needle: turned 30 degrees clockwise, dark with a light halo, its light south half over it.
            assert_eq!((d.north.len(), d.north_south.len()), (1, 1));
            let (q, south) = (d.north[0], d.north_south[0]);
            assert!((q.rotation_rad - 30f32.to_radians()).abs() < 1e-6 && south.rotation_rad == q.rotation_rad);
            let sdf = q.sdf.unwrap();
            assert!(sdf.halo_width > 0.0 && sdf.halo[0] > 0.9 && sdf.fill[0] < 0.2);
            assert!(south.sdf.unwrap().fill[0] > 0.9, "the south half is light");
            assert!((south.screen_x, south.screen_y, south.width) == (q.screen_x, q.screen_y, q.width), "one pivot");
            assert!((q.width / ratio - crate::story::layout::NORTH_ARROW * 64.0 / 52.0).abs() < 1e-3);
        }
    }

    /// Controller ruling: the compass's "N" sits outside the plate at the
    /// needle's turned north tip and circles the plate as the bearing turns,
    /// while the glyph itself stays upright (a text draw, never turned; the
    /// same shaped "N" at every bearing).
    #[test]
    fn the_compass_n_circles_the_plate_with_the_bearing_and_stays_upright() {
        use crate::story::layout::{NORTH_N_RADIUS, NORTH_PLATE};
        let mut keys = Vec::new();
        for (deg, dir) in [(0.0f32, (0.0f32, -1.0f32)), (90.0, (1.0, 0.0)), (-90.0, (-1.0, 0.0)), (180.0, (0.0, 1.0)), (30.0, (0.5, -0.866))] {
            let furniture = crate::story::furniture::FurnitureFrame { north_arrow: Some(deg), ..Default::default() };
            let frame = StoryFrame { items: vec![LayoutItem::Furniture { id: "furniture".into(), frame: furniture }], ..StoryFrame::default() };
            let d = draw(&frame);
            assert_eq!(d.text.len(), 1, "{deg}: only the N");
            let ((key, left, top, ..), _) = &d.text[0];
            assert_eq!(key.0, "N");
            keys.push(key.clone());
            // The plate is the compass's centre.
            let p = d.plates[0];
            assert!((p.width - NORTH_PLATE).abs() < 1e-3 && (p.radius_px - NORTH_PLATE / 2.0).abs() < 1e-3);
            let (cx, cy) = (p.screen_x + p.width / 2.0, p.screen_y + p.height / 2.0);
            // The N's centre, from its shaped box.
            let mut fonts = noto();
            let shaped = fonts.shapes.get_or_shape(&mut fonts.system, "N", f32::from_bits(key.1), None, TextJustify::Left, &chrome_text_face());
            let (nx, ny) = (left + shaped.measured_width / 2.0, top + shaped.measured_height / 2.0);
            let (dx, dy) = (nx - cx, ny - cy);
            assert!((dx.hypot(dy) - NORTH_N_RADIUS).abs() < 1.5, "{deg}: N {:.1} from the centre", dx.hypot(dy));
            assert!(dx.hypot(dy) > NORTH_PLATE / 2.0 + shaped.measured_width / 2.0, "{deg}: outside the plate");
            assert!((dx / NORTH_N_RADIUS - dir.0).abs() < 0.04 && (dy / NORTH_N_RADIUS - dir.1).abs() < 0.04, "{deg}: N at ({dx:.1}, {dy:.1})");
            // The needle turns; the N's draw has no rotation at all, and a halo.
            assert!((d.north[0].rotation_rad - deg.to_radians()).abs() < 1e-6);
            assert!(d.text[0].0 .4 > 0.0, "{deg}: the N has a halo");
        }
        assert!(keys.windows(2).all(|k| k[0] == k[1]), "one upright glyph at every bearing");
    }

    #[test]
    fn cover_and_contain_uvs() {
        // A 2:1 image into a 1:1 rect.
        let rect = (10.0, 20.0, 110.0, 120.0);
        let (r, a, b) = fit_image(rect, (200, 100), Fit::Cover);
        assert_eq!((r, a, b), (rect, (0.25, 0.0), (0.75, 1.0)));
        let (r, a, b) = fit_image(rect, (200, 100), Fit::Contain);
        assert_eq!((a, b), ((0.0, 0.0), (1.0, 1.0)));
        assert_eq!(r, (10.0, 45.0, 110.0, 95.0), "the height halves, centred");
        // A 1:2 image into a 2:1 rect: cover crops rows, contain narrows.
        let wide = (0.0, 0.0, 200.0, 100.0);
        let (_, a, b) = fit_image(wide, (100, 200), Fit::Cover);
        assert_eq!((a, b), ((0.0, 0.375), (1.0, 0.625)));
        let (r, ..) = fit_image(wide, (100, 200), Fit::Contain);
        assert_eq!(r, (75.0, 0.0, 125.0, 100.0));
        // Degenerate input draws the whole image into the rect.
        assert_eq!(fit_image(wide, (0, 5), Fit::Cover), (wide, (0.0, 0.0), (1.0, 1.0)));
    }

    /// Task R1, spec §4.1/D5: the card measures in Source Sans 3, not the
    /// generic chrome family `design_face` fell back to before the faces
    /// were bundled. A bolder run measures wider than the same text at
    /// 400, and the face `design_face` hands the shaper is `STORY_FAMILY`.
    #[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
    #[test]
    fn the_card_measures_in_source_sans_at_the_token_weight() {
        use crate::story::card_layout::CardMeasure;
        let face = design_face(700);
        assert_eq!(face.family.as_ref(), crate::font_fallback::STORY_FAMILY, "{face:?}");

        let mut db = glyphon::fontdb::Database::new();
        for bytes in crate::font_fallback::STORY_FONTS {
            db.load_font_data(bytes.to_vec());
        }
        let mut system = crate::font_fallback::new_font_system_from_db(String::from("en-US"), db);
        let mut shapes = crate::symbol_pass::LabelShapeCache::new();
        let cell: StoryFonts<'_> = RefCell::new((&mut system, &mut shapes));
        let measure = GlyphonCardMeasure { fonts: &cell, ratio: 1.0 };
        let text = "A headline the design system sets";
        let w400 = measure.width(text, 60.0, crate::story::design::TextStyle::of_weight(400));
        let w700 = measure.width(text, 60.0, crate::story::design::TextStyle::of_weight(700));
        assert!(w400 > 0.0 && w700 > w400, "400: {w400}, 700: {w700}");
    }

    /// Task R2 step 4: one card per new item (quote, photo, counter,
    /// sequential legend, legend values), rendered headless at 9:16 on a
    /// plain background, each against the same frame with no card. Every
    /// item's rectangle has painted pixels; the photo is the image, not
    /// its placeholder. `GPUI_MAP_IOU_DUMP` writes each PNG for the eyes.
    #[test]
    fn every_new_card_item_paints_inside_its_rect() {
        use crate::story::card_layout::{card_photo_id, layout_card, CardContent, CardItem, DesignFrame, LegendRow, Photo, Quote};
        let style = crate::style_spec::parse_and_compile(br##"{"version": 8, "sources": {}, "layers": [{"id": "bg", "type": "background", "paint": {"background-color": "#c8d8e0"}}]}"##)
            .expect("a background style");
        let label_fonts = || -> Option<crate::capture::LabelFonts> {
            let f = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/NotoSans-Regular.latin-subset.ttf")).unwrap();
            Some((vec![f].into(), "Noto Sans".to_string()))
        };
        let magenta = std::sync::Arc::new(image::RgbaImage::from_pixel(64, 48, image::Rgba([230, 0, 200, 255])));
        let src = "img/test.png";
        let card = |c: CardContent| CardContent { headline: Some("Marina Bay".into()), ..c };
        let cases: Vec<(&str, CardContent)> = vec![
            ("quote", CardContent { quote: Some(Quote { text: "We built it to last".into(), by: Some("Lee".into()), source: Some("1987".into()) }), items: vec![CardItem::Attribution], ..CardContent::default() }),
            ("photo", card(CardContent { photo: Some(Photo { src: src.into(), alt: "Test".into(), credit: None }), ..CardContent::default() })),
            ("counter", card(CardContent { index: Some((3, 5)), ..CardContent::default() })),
            ("sequential", card(CardContent { items: vec![CardItem::Sequential { stops: vec![[0x5C, 0x1A, 0x1B], [0xE8, 0x59, 0x0C], [0xFF, 0xE0, 0x66]], ticks: vec!["1 Sep".into(), "14 Sep".into(), "28 Sep".into()], current: Some(2), title: Some("Haze days".into()) }], ..CardContent::default() })),
            ("legend", card(CardContent { items: vec![CardItem::Legend { rows: vec![LegendRow { color: [0xE8, 0x59, 0x0C], label: "Ferry".into(), value: Some("12".into()) }, LegendRow { color: [0x0B, 0x72, 0x85], label: "Cargo".into(), value: Some("40".into()) }], title: Some("Vessels".into()) }], ..CardContent::default() })),
        ];
        let frame_for = |c: Option<CardContent>| StoryFrame {
            images: vec![crate::capture::StoryImage { id: card_photo_id(src), image: std::sync::Arc::clone(&magenta), fit: Fit::Cover, credit: None }],
            design: Some(DesignFrame {
                card: c,
                from_card: None,
                ease: 1.0,
                card_alpha: 1.0,
                progress: Some(0.5),
                accent: [0xE8, 0x59, 0x0C],
                tone: crate::story::design::Tone::Paper,
                safe_area: crate::story::tiers::SafeArea::Reels,
                aspect: crate::story::tiers::Aspect::Portrait,
                sources: Vec::new(),
                credit: None,
                anchor: crate::story::frame_layout::AnchorPref::Auto,
                dir: crate::story::frame_layout::Dir::Ltr,
                credits: crate::story::frame_layout::CreditsPlacement::Bottom,
            }),
            ..StoryFrame::default()
        };
        let request = |frame: StoryFrame| crate::capture::CaptureRequest {
            width: 1080,
            height: 1920,
            pixel_ratio: 1.0,
            label_fonts: label_fonts(),
            story: Some(std::sync::Arc::new(frame)),
            ..crate::capture::CaptureRequest::default()
        };
        let tiles = crate::capture::TileStore::new();
        let shoot = |frame: StoryFrame, name: &str| {
            let before = crate::render::allocation_stats().draw_calls_total;
            let shot = crate::capture::capture(&request(frame), &tiles, &style).expect("a headless device");
            assert!(crate::render::allocation_stats().draw_calls_total > before, "{name}: draw_calls_total did not move -- nothing was drawn");
            if let Ok(dir) = std::env::var("GPUI_MAP_IOU_DUMP") {
                let _ = std::fs::create_dir_all(&dir);
                if let Some(img) = image::RgbaImage::from_raw(shot.width, shot.height, shot.rgba.clone()) {
                    let _ = img.save(format!("{dir}/{name}.png"));
                }
            }
            shot
        };
        let bare = shoot(frame_for(None), "r2_no_card");
        let mut fonts = LayoutFonts::new(label_fonts());
        for (name, content) in cases {
            let l = fonts.with_card_measure(1.0, |m| {
                // The slot as the capture draws it: the bottom row reserves
                // the mark's height only when a mark draws.
                let spec = crate::story::frame_layout::FrameSpec {
                    mark_w: request(frame_for(None)).slot_mark_w(Some(crate::brand_mark::artwork_size())),
                    ..crate::story::frame_layout::FrameSpec::new((1080.0, 1920.0), crate::story::tiers::SafeArea::Reels, crate::story::tiers::Aspect::Portrait)
                };
                layout_card(&spec, Some(&content), &[], None, m)
            });
            let rects: Vec<(&str, Rect)> = match name {
                "quote" => l.runs.iter().filter(|r| r.ink == crate::story::card_layout::Ink::Accent || r.text == "Lee").map(|r| ("run", (r.x, r.top, r.x + r.width, r.top + r.line_height))).collect(),
                "photo" => l.photo.iter().map(|(r, _)| ("photo", *r)).collect(),
                "counter" => l.counter.iter().map(|r| ("counter", (r.x, r.top, r.x + r.width, r.top + r.line_height))).collect(),
                "sequential" => l.ramp.iter().map(|(r, _)| ("ramp", *r)).chain(l.runs.iter().filter(|r| r.text.ends_with("Sep")).map(|r| ("tick", (r.x, r.top, r.x + r.width, r.top + r.line_height)))).collect(),
                _ => l.runs.iter().filter(|r| r.text == "12" || r.text == "Vessels").map(|r| ("run", (r.x, r.top, r.x + r.width, r.top + r.line_height))).chain(l.swatches.iter().map(|(r, _)| ("dot", *r))).collect(),
            };
            assert!(!rects.is_empty(), "{name}: the layout placed the item");
            let shot = shoot(frame_for(Some(content)), &format!("r2_{name}"));
            for (what, r) in rects {
                let (mut changed, mut total) = (0u32, 0u32);
                for y in (r.1.ceil() as u32)..(r.3.floor() as u32) {
                    for x in (r.0.ceil() as u32)..(r.2.floor() as u32) {
                        total += 1;
                        let (a, b) = (shot.pixel(x, y), bare.pixel(x, y));
                        changed += u32::from((0..3).any(|c| a[c].abs_diff(b[c]) > 8));
                    }
                }
                // The card plate alone changes every pixel, so measure the
                // item against the plate: a sample inside the rect differs
                // from the card's own fill.
                let fill = shot.pixel((l.card.unwrap().0 + 8.0) as u32, (l.card.unwrap().1 + 40.0) as u32);
                let inked = (r.1.ceil() as u32..r.3.floor() as u32)
                    .flat_map(|y| (r.0.ceil() as u32..r.2.floor() as u32).map(move |x| (x, y)))
                    .filter(|&(x, y)| (0..3).any(|c| shot.pixel(x, y)[c].abs_diff(fill[c]) > 24))
                    .count();
                assert!(total > 0 && changed > 0, "{name} {what} {r:?}: nothing painted");
                assert!(inked as f32 >= 0.05 * total as f32, "{name} {what} {r:?}: {inked} of {total} pixels inked over the card's fill");
            }
            if name == "photo" {
                let (r, _) = l.photo.clone().unwrap();
                let p = shot.pixel(((r.0 + r.2) / 2.0) as u32, ((r.1 + r.3) / 2.0) as u32);
                assert!(p[0] > 200 && p[1] < 40 && p[2] > 170, "the photo is the image: {p:?}");
            }
        }
    }

    /// A font state with the bundled story faces (Source Sans 3, IBM Plex
    /// Sans Arabic) and nothing else: R12's shaping tests.
    #[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
    fn story_font_system() -> glyphon::FontSystem {
        let mut db = glyphon::fontdb::Database::new();
        for bytes in crate::font_fallback::STORY_FONTS {
            db.load_font_data(bytes.to_vec());
        }
        crate::font_fallback::new_font_system_from_db(String::from("en-US"), db)
    }

    /// Task R12, spec §8: with `tnum` the bundled Source Sans 3 sets every
    /// figure on one advance, so a counter or a stat does not jitter as its
    /// digits change -- and the features reach the shaper.
    ///
    /// Measured, not assumed: the bundled latin subset carries no `tnum`,
    /// `pnum` or `lnum` lookup at all (its GSUB is `liga` and `locl`) and
    /// its DEFAULT figures are already tabular lining (every digit 513/1000
    /// em in the Semibold). So the request is a no-op on these faces and
    /// the plain run is equal too; it stays in the style (and the key) so a
    /// face with proportional default figures gets them tabular.
    #[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
    #[test]
    fn tnum_makes_1111_and_8888_equal_in_source_sans() {
        use crate::story::card_layout::CardMeasure;
        use crate::story::design::{TextStyle, LNUM, STAT_L, TNUM};
        let mut system = story_font_system();
        let mut shapes = LabelShapeCache::new();
        let cell: StoryFonts<'_> = RefCell::new((&mut system, &mut shapes));
        let measure = GlyphonCardMeasure { fonts: &cell, ratio: 1.0 };
        let tabular = TextStyle::of_weight(600).with_features(TNUM | LNUM);
        assert_eq!(STAT_L.style().features, TNUM | LNUM, "the stat token asks for both");
        let (t1, t8) = (measure.width("1111", 60.0, tabular), measure.width("8888", 60.0, tabular));
        assert!(t1 > 0.0 && (t1 - t8).abs() < 0.01, "tabular figures are one advance: 1111 {t1}, 8888 {t8}");
        let (p1, p8) = (measure.width("1111", 60.0, TextStyle::of_weight(600)), measure.width("8888", 60.0, TextStyle::of_weight(600)));
        assert!((p1 - p8).abs() < 0.01 && (p1 - t1).abs() < 0.01, "the subset's default figures are tabular too: {p1} {p8} {t1}");
        let face = design_face(600);
        let attrs = tabular.apply(face.attrs());
        let tags: Vec<[u8; 4]> = attrs.font_features.features.iter().filter(|f| f.value == 1).map(|f| *f.tag.as_bytes()).collect();
        assert_eq!(tags, [*b"tnum", *b"lnum"], "the features reach cosmic-text");
        assert!(TextStyle::default().apply(face.attrs()) == face.attrs(), "the default style adds nothing");
    }

    /// Task R12, spec §4.1/§8: tracking is added BETWEEN glyphs, so a
    /// ten-glyph run grows by nine steps, not ten -- the measured width
    /// ends at the last glyph, where a right-aligned run must end.
    #[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
    #[test]
    fn tracking_changes_a_ten_glyph_run_by_nine_steps() {
        use crate::story::card_layout::CardMeasure;
        use crate::story::design::TextStyle;
        let mut system = story_font_system();
        let mut shapes = LabelShapeCache::new();
        let cell: StoryFonts<'_> = RefCell::new((&mut system, &mut shapes));
        let measure = GlyphonCardMeasure { fonts: &cell, ratio: 1.0 };
        let text = "ABCDEFGHIJ";
        let plain = TextStyle::of_weight(700);
        let tracked = TextStyle { letter_spacing_milli_em: 20, ..plain };
        let tight = TextStyle { letter_spacing_milli_em: -18, ..plain };
        let w = measure.width(text, 60.0, plain);
        let step = 0.020 * 60.0;
        assert!((measure.width(text, 60.0, tracked) - w - 9.0 * step).abs() < 0.01, "+20: {} vs {w}", measure.width(text, 60.0, tracked));
        assert!((measure.width(text, 60.0, tight) - w + 9.0 * 0.018 * 60.0).abs() < 0.01, "-18: {} vs {w}", measure.width(text, 60.0, tight));
    }

    /// Task R12, spec §4.1: tracking breaks Arabic joining, so an Arabic
    /// run shapes as if untracked whatever its token says -- and so do
    /// Hebrew and CJK runs, which the key normalises the same way.
    #[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
    #[test]
    fn arabic_runs_take_no_tracking() {
        use crate::story::card_layout::CardMeasure;
        use crate::story::design::TextStyle;
        let mut system = story_font_system();
        let mut shapes = LabelShapeCache::new();
        let cell: StoryFonts<'_> = RefCell::new((&mut system, &mut shapes));
        let measure = GlyphonCardMeasure { fonts: &cell, ratio: 1.0 };
        let arabic = "\u{0627}\u{0644}\u{0645}\u{064A}\u{0646}\u{0627}\u{0621}";
        let plain = TextStyle::of_weight(700);
        let tracked = TextStyle { letter_spacing_milli_em: -18, ..plain };
        let (w, wt) = (measure.width(arabic, 60.0, plain), measure.width(arabic, 60.0, tracked));
        assert!(w > 0.0 && (w - wt).abs() < 1e-3, "untracked {w}, tracked {wt}");
        // Arabic, Hebrew, Han, kana, Hangul; Syriac, N'Ko, Mongolian,
        // Thaana; Devanagari, Bengali, Tamil, Sinhala (R12 review).
        for text in [
            arabic,
            "\u{05E9}\u{05DC}\u{05D5}\u{05DD}",
            "\u{6771}\u{4EAC}",
            "\u{3068}\u{3046}",
            "\u{D55C}",
            "\u{0710}\u{0712}",
            "\u{07CA}\u{07CB}",
            "\u{1820}\u{1821}",
            "\u{0780}\u{0781}",
            "\u{0939}\u{093F}",
            "\u{0995}",
            "\u{0BA4}",
            "\u{0DC3}",
        ] {
            assert_eq!(tracked.for_text(text).letter_spacing_milli_em, 0, "{text}");
        }
        assert_eq!(tracked.for_text("Marina Bay").letter_spacing_milli_em, -18);
    }

    /// Task R12, spec §8/§14.1: `TextStyle::default()` shapes exactly as
    /// the unstyled call always has (one key, one entry, the same glyphs),
    /// so map labels and `capture_golden.txt` cannot move; and a styled
    /// run of the same text never shares that entry.
    #[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
    #[test]
    fn the_default_style_shapes_as_before() {
        use crate::story::design::{TextStyle, TNUM};
        let mut system = story_font_system();
        let mut shapes = LabelShapeCache::new();
        let face = design_face(600);
        let text = "1 / 18";
        let glyphs = |s: &crate::symbol_pass::ShapedLabel| -> Vec<(u16, u32, u32)> {
            s.buffer.layout_runs().flat_map(|r| r.glyphs.iter().map(|g| (g.glyph_id, g.x.to_bits(), g.w.to_bits())).collect::<Vec<_>>()).collect()
        };
        let before = glyphs(shapes.get_or_shape(&mut system, text, 26.0, None, TextJustify::Left, &face));
        let after = glyphs(shapes.get_or_shape_styled(&mut system, text, 26.0, None, TextJustify::Left, &face, TextStyle::default()));
        assert_eq!(before, after);
        assert_eq!(
            LabelShapeCache::key(text, 26.0, None, TextJustify::Left, &face),
            LabelShapeCache::key_styled(text, 26.0, None, TextJustify::Left, &face, TextStyle::default())
        );
        assert_eq!((shapes.entries.len(), shapes.shape_count), (1, 1), "one entry for both calls");
        let styled = TextStyle { features: TNUM, ..TextStyle::default() };
        shapes.get_or_shape_styled(&mut system, text, 26.0, None, TextJustify::Left, &face, styled);
        assert_eq!(shapes.entries.len(), 2, "a styled run is its own entry");
        assert_ne!(
            LabelShapeCache::key(text, 26.0, None, TextJustify::Left, &face),
            LabelShapeCache::key_styled(text, 26.0, None, TextJustify::Left, &face, styled)
        );
    }
}
