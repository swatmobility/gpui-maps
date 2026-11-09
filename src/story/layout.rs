//! The layout function (spec §4.1): where every story element on screen at
//! `t` goes -- title cards, lower-thirds, side cards (bottom sheets on a
//! portrait frame), corner cards and images, pinned callouts and captions
//! -- for any aspect and resolution.
//!
//! **Units.** `layout` takes the frame in PHYSICAL pixels and returns
//! every rectangle in 1080-referenced units (`physical / pixel_ratio`), so a
//! 4K frame and a 1080p frame of the same aspect lay out identically.
//!
//! **Pure.** It reads its arguments and nothing else: no clock, no font
//! system (text is measured through the caller's [`TextMeasure`], plan
//! ruling D4), no map or set anywhere, so the same inputs always give the
//! same [`Layout`]. Compiled on web and iOS through `#[path]` includes.
//!
//! **Algorithm** (the Task 9 brief; no search, no randomness):
//! 1. Obstacles start as the safe-area rectangles then the chrome corners
//!    (the SWAT mark, the credit plate), each grown by [`GAP`].
//! 2. Items are placed in `(Priority, input index)` order; every placed
//!    rectangle joins the obstacles, grown by [`GAP`].
//! 3. Bands: an obstacle wider than half the frame whose top is below the
//!    middle is a bottom band, one whose bottom is above the middle a top
//!    band, every other one a side obstacle. The free band runs from
//!    `max(MARGIN, top bands' bottoms)` to `min(h - MARGIN, bottom bands'
//!    tops)`.
//! 4. Each placement has its rule (see the `place_*` functions).
//! 5. A rectangle that still meets an obstacle moves by the smallest
//!    vertical shift that clears every obstacle inside the frame; when none
//!    exists it stays and an `in_safe_zone` or `overlap` warning names both.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

pub use super::furniture::FurnitureFrame;
pub use super::panel::PanelTheme;
use super::script::Placement;
pub use super::tiers::Rect;
use super::timeline::Anchor;

/// The order in which items claim space: a lower value is placed first and
/// keeps its rule rectangle; later items move around it (spec §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Title = 0,
    Panel = 1,
    Image = 2,
    Furniture = 3,
    Callout = 4,
    Caption = 5,
}

/// Which side of its anchor a callout card sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// What the layout places. Text is measured through [`TextMeasure`]; sizes
/// are 1080 units.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutItem {
    Panel {
        id: String,
        placement: Placement,
        headline: Option<String>,
        body: Option<String>,
        source: Option<String>,
        /// The panel's image, width / height, drawn above the text.
        image_aspect: Option<f32>,
        theme: PanelTheme,
    },
    Image {
        id: String,
        placement: Placement,
        /// Width / height.
        aspect: f32,
    },
    /// Map furniture (spec §4.5, pre-flight ruling F29): up to four pieces,
    /// each placed as its own [`Placed`] with the id `<id>/<piece>`
    /// ([`FURNITURE_PIECES`]): the source line and byline above the credit
    /// plate, the scale bar above the SWAT mark, the legend top-right and
    /// the north arrow top-left.
    Furniture { id: String, frame: FurnitureFrame },
    /// `anchor` is the projected coordinate in 1080 units; `side` comes from
    /// `callout::side_at`. `theme` colours the card, its text and its
    /// leader (the layout reads none of it: a callout's padding is
    /// [`CALLOUT_PADDING`]).
    Callout { id: String, anchor: (f32, f32), text: String, side: Side, image_aspect: Option<f32>, theme: PanelTheme },
    /// `offset` moves the caption from its anchor position before it is
    /// pushed clear of obstacles, as `capture::chrome::caption_origin` applies
    /// `offset_px` (1080 units). `plate` is the caption's own: without one
    /// its box has no plate padding, as `caption_origin` places a plate-less
    /// caption.
    Caption { id: String, text: String, size: f32, anchor: Anchor, offset: (f32, f32), plate: bool },
}

impl LayoutItem {
    pub fn id(&self) -> &str {
        match self {
            LayoutItem::Panel { id, .. }
            | LayoutItem::Image { id, .. }
            | LayoutItem::Furniture { id, .. }
            | LayoutItem::Callout { id, .. }
            | LayoutItem::Caption { id, .. } => id,
        }
    }

    pub fn priority(&self) -> Priority {
        match self {
            LayoutItem::Panel { placement: Placement::Title, .. } => Priority::Title,
            LayoutItem::Panel { .. } => Priority::Panel,
            LayoutItem::Image { .. } => Priority::Image,
            LayoutItem::Furniture { .. } => Priority::Furniture,
            LayoutItem::Callout { .. } => Priority::Callout,
            LayoutItem::Caption { .. } => Priority::Caption,
        }
    }
}

/// Measures text for the layout, so the layout needs no font system (D4).
pub trait TextMeasure {
    /// `(width, height)` of `text` at `size`, wrapped at `wrap` (1080 units).
    fn measure(&self, text: &str, size: f32, wrap: Option<f32>) -> (f32, f32);
}

/// What a text block is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRole {
    Headline,
    Body,
    Source,
    Callout,
    Caption,
    /// A furniture label: a scale bar's, a legend's title or entry.
    Furniture,
}

/// One placed item, in 1080 units.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub id: String,
    /// The card (or image, or caption plate) rectangle.
    pub rect: Rect,
    /// Text blocks inside `rect`: (role, rect, size, text as drawn).
    pub text: Vec<(TextRole, Rect, f32, String)>,
    pub image: Option<Rect>,
    /// The width this card's text was wrapped at (1080 units), when it is
    /// not the card's inner width: a wide card with an image beside its
    /// text, or a caption narrowed beside a side rail (otherwise a caption
    /// wraps at [`caption_wrap`]). The draw wraps at the same width.
    pub text_wrap: Option<f32>,
    /// A callout's leader line: from the anchor to the card's nearest edge
    /// midpoint.
    pub leader: Option<((f32, f32), (f32, f32))>,
}

/// Something the layout could not do cleanly. Serialised into the render
/// manifest (pre-flight ruling F11).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LayoutWarning {
    pub id: String,
    /// `in_safe_zone` | `overlap` | `bad_measure` | `bad_input` |
    /// `callout_offscreen` | `frame_too_small` | `text_shrunk` |
    /// `text_truncated` | `text_dropped` (the last three, Task 10's
    /// `fit_text`) | `scale_bar_hidden_pitch` (a note: Task 14's scale bar
    /// was asked for but the camera is pitched too far). At most one per `(id, code)` per layout.
    pub code: &'static str,
    pub message: String,
}

/// Everything placed at one `t`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Layout {
    /// In placement order: `(Priority, input index)`.
    pub placed: Vec<Placed>,
    pub warnings: Vec<LayoutWarning>,
    /// `Some(TITLE_DIM)` while a title card is placed; the caller multiplies
    /// it by the title element's alpha.
    pub dim: Option<f32>,
}

/// Distance from the frame's edges to any card, 1080 units.
pub const MARGIN: f32 = 48.0;
/// Room kept around every obstacle and every placed card, 1080 units.
pub const GAP: f32 = 16.0;
/// No text is drawn smaller than this, 1080 units (spec §4.1).
pub const TEXT_FLOOR: f32 = 28.0;
/// How much a title card dims the map, before the element's alpha.
pub const TITLE_DIM: f32 = 0.55;

/// Text sizes, 1080 units.
const TITLE_HEADLINE: f32 = 88.0;
const TITLE_BODY: f32 = 40.0;
const CARD_HEADLINE: f32 = 56.0;
const CARD_BODY: f32 = 36.0;
const SOURCE_SIZE: f32 = 28.0;
/// Callout text: the floor plus a little, so a callout never needs to shrink
/// on a short label.
const CALLOUT_SIZE: f32 = 32.0;
/// Space between stacked blocks (image, headline, body, source).
const BLOCK_GAP: f32 = 12.0;
/// A card's image keeps at least this share of the card's inner height
/// when the text would otherwise take it all.
const IMAGE_MIN_SHARE: f32 = 0.4;
/// A card whose inner width is more than this many times its inner height
/// puts its image in a left column beside the text (a stacked strip that
/// wide would keep a sliver of the photo; Task 12 review F3).
const SIDE_IMAGE_ASPECT: f32 = 2.5;
/// That column's share of the inner width, and the gap after it.
const SIDE_IMAGE_SHARE: f32 = 0.4;
const SIDE_IMAGE_GAP: f32 = 24.0;

const LOWER_THIRD_MAX_W: f32 = 1200.0;
const SIDE_W: f32 = 560.0;
const CORNER_W: f32 = 480.0;
const CORNER_IMAGE_MAX_H: f32 = 360.0;
/// Lower-third and corner cards are at most this fraction of the frame's
/// height (pre-flight ruling F2); a bottom sheet may take more.
const CARD_HEIGHT_CAP: f32 = 0.30;
const SHEET_HEIGHT_CAP: f32 = 0.45;
/// A side trim that leaves less than this fraction of the ideal width is
/// undone, and the card moves up instead.
const MIN_TRIMMED_FRACTION: f32 = 0.6;

/// A callout card's widest, 1080 units (and its width with an image).
pub const CALLOUT_MAX_W: f32 = 420.0;
/// A callout card's padding around its text and image, 1080 units.
pub const CALLOUT_PADDING: f32 = 16.0;
/// Room a callout's one-line text gets beyond its measured width, 1080
/// units ([`callout_width`]).
pub const CALLOUT_TEXT_SLACK: f32 = 1.0;
/// How far a callout card sits from its anchor, 1080 units.
pub const CALLOUT_OFFSET: f32 = 24.0;

/// A callout card's width: its one-line text rounded up to a whole unit
/// plus [`CALLOUT_TEXT_SLACK`], plus padding, capped at
/// [`CALLOUT_MAX_W`]; always the cap with an image. What the layout
/// places, and what `callout::schedule` decides the side from.
///
/// The slack is not cosmetic (Task 19 review F3): `Content::place` and
/// the draw re-derive the text width from the placed rect as
/// `(x + w) - x - 2 * padding`, which f32 rounds an ulp either way of
/// `w` depending on `x`. A card exactly as wide as its text then wrapped
/// the last word at some anchors and not others, so a moving callout's
/// text popped between its size and a shrunk one frame to frame.
pub fn callout_width(text: &str, has_image: bool, measure: &dyn TextMeasure) -> f32 {
    if has_image {
        return CALLOUT_MAX_W;
    }
    let (text_w, _) = measure.measure(text, CALLOUT_SIZE, None);
    let w = (text_w.ceil() + CALLOUT_TEXT_SLACK + 2.0 * CALLOUT_PADDING).min(CALLOUT_MAX_W);
    if w.is_finite() {
        w.max(0.0)
    } else {
        CALLOUT_MAX_W
    }
}

/// `capture::chrome`'s caption margin and plate padding, in units (the
/// same numbers it uses in logical pixels).
const CAPTION_MARGIN: f32 = 24.0;
const CAPTION_PAD_X: f32 = 12.0;
const CAPTION_PAD_Y: f32 = 6.0;

/// The width a caption's text wraps at on a frame `frame_w` units wide:
/// the frame between the caption margins, less the plate's padding when it
/// has a plate. The capture draws a laid-out caption at this same wrap, so
/// the text it shapes is the text this layout measured.
pub fn caption_wrap(frame_w: f32, plate: bool) -> f32 {
    (frame_w - 2.0 * CAPTION_MARGIN - 2.0 * caption_pad(plate).0).max(0.0)
}

/// A caption's plate padding `(x, y)`: none without a plate.
fn caption_pad(plate: bool) -> (f32, f32) {
    if plate {
        (CAPTION_PAD_X, CAPTION_PAD_Y)
    } else {
        (0.0, 0.0)
    }
}

/// How far (1080 units) the bottom band of `safe_area` reaches up from the
/// bottom of a frame `frame_units` big: the frame's height less the
/// highest top of any bottom band (a rectangle wider than half the frame
/// in the frame's lower half, as the layout's step 3 reads it), or 0 with
/// none. The corner chrome sits this far up (final review F3: the licence
/// credit must stay visible on the platform), so it never draws under the
/// platform's own UI ([`corner_insets`]).
pub fn bottom_band_inset(frame_units: (f32, f32), safe_area: &[Rect]) -> f32 {
    let (w, h) = frame_units;
    safe_area
        .iter()
        .filter(|r| width(**r).is_finite() && width(**r) > w / 2.0 && r.1 > h / 2.0 && r.1.is_finite())
        .map(|r| h - r.1)
        .fold(0.0f32, f32::max)
        .clamp(0.0, h.max(0.0))
}

/// The corner chrome's insets `(right, bottom)`, 1080 units, on a frame
/// `frame_units` big under `safe_area`: the bottom band
/// ([`bottom_band_inset`]), and the width of a right-hand rail that comes
/// down to that band (so it covers the corner the credit sits in above
/// it), or 0. With both, the credit plate sits clear of the band and left
/// of the rail -- outside every safe-area rectangle (final review F3).
pub fn corner_insets(frame_units: (f32, f32), safe_area: &[Rect]) -> (f32, f32) {
    let (w, h) = frame_units;
    let bottom = bottom_band_inset(frame_units, safe_area);
    let floor = h - bottom;
    let right = safe_area
        .iter()
        .filter(|r| [r.0, r.1, r.2, r.3].iter().all(|v| v.is_finite()))
        .filter(|r| width(**r) <= w / 2.0 && r.0 > w / 2.0 && r.3 >= floor - 1.0 && r.1 < floor)
        .map(|r| w - r.0)
        .fold(0.0f32, f32::max)
        .clamp(0.0, w.max(0.0));
    (right, bottom)
}

/// Two rectangles share area (touching edges, within float noise, do not).
fn intersects(a: Rect, b: Rect) -> bool {
    const EPS: f32 = 1e-3;
    a.0 < b.2 - EPS && b.0 < a.2 - EPS && a.1 < b.3 - EPS && b.1 < a.3 - EPS
}

fn grow(r: Rect, by: f32) -> Rect {
    (r.0 - by, r.1 - by, r.2 + by, r.3 + by)
}

fn width(r: Rect) -> f32 {
    r.2 - r.0
}

/// What an obstacle is, for warnings.
#[derive(Clone, Debug)]
enum Source {
    SafeArea(usize),
    Chrome(usize),
    Placed(String),
    /// A placed title card: captions draw over it (controller ruling, Task 9
    /// fix round 1), like subtitles over a title in edited video.
    Title(String),
}

impl Source {
    fn name(&self) -> String {
        match self {
            Source::SafeArea(i) => format!("safe_area[{i}]"),
            Source::Chrome(i) => format!("chrome[{i}]"),
            Source::Placed(id) | Source::Title(id) => id.clone(),
        }
    }
}

/// A rectangle nothing else may meet, already grown by [`GAP`].
#[derive(Clone, Debug)]
struct Obstacle {
    rect: Rect,
    source: Source,
}

/// The frame in units and the obstacles placed so far.
struct Frame {
    w: f32,
    h: f32,
    obstacles: Vec<Obstacle>,
}

impl Frame {
    /// Step 3: the free band `(top, bottom)` between the top and bottom
    /// bands.
    ///
    /// The corner chrome, lifted onto a platform's bottom band (final review
    /// F3: `corner_insets`), sits on the band's edge: there it is part of the
    /// band, so a card sits above it rather than being trimmed to a sliver
    /// between the mark and the credit. Without a platform band the chrome
    /// is in the frame's corner and stays a side obstacle.
    fn band(&self) -> (f32, f32) {
        let (mut top, mut bottom) = (MARGIN, self.h - MARGIN);
        let mut platform_band = false;
        for o in &self.obstacles {
            match self.band_kind(o.rect) {
                Some(BandKind::Top) => top = top.max(o.rect.3),
                Some(BandKind::Bottom) => {
                    bottom = bottom.min(o.rect.1);
                    platform_band |= matches!(o.source, Source::SafeArea(_));
                }
                None => {}
            }
        }
        if platform_band {
            let edge = bottom;
            for o in self.obstacles.iter().filter(|o| matches!(o.source, Source::Chrome(_))) {
                // Never past the band's top: chrome that would close the
                // band is left a side obstacle.
                if o.rect.1 < edge && o.rect.3 >= edge && o.rect.1 > top {
                    bottom = bottom.min(o.rect.1);
                }
            }
        }
        (top, bottom)
    }

    fn band_kind(&self, r: Rect) -> Option<BandKind> {
        let mid = self.h / 2.0;
        // A non-finite width (an inf or NaN edge) is never a band: it would
        // shrink the free band for everything after it.
        if !width(r).is_finite() || width(r) <= self.w / 2.0 {
            None
        } else if r.1 > mid {
            Some(BandKind::Bottom)
        } else if r.3 < mid {
            Some(BandKind::Top)
        } else {
            None
        }
    }

    fn meets_any(&self, r: Rect) -> bool {
        self.meets(r, &all)
    }

    fn meets(&self, r: Rect, counts: &dyn Fn(&Obstacle) -> bool) -> bool {
        self.obstacles.iter().any(|o| counts(o) && intersects(r, o.rect))
    }

    /// Step 5 (rule 5's trim): every side obstacle that overlaps `rect`
    /// (vertically, and horizontally so a trim can do something) cuts it
    /// from the right when its centre is right of the frame's centre, from
    /// the left otherwise. Returns the trimmed rectangle and the tops of the
    /// obstacles that cut it.
    fn trim(&self, rect: Rect) -> (Rect, Vec<f32>) {
        self.trim_by(rect, &all)
    }

    /// [`Self::trim`] by only the side obstacles `counts` keeps.
    fn trim_by(&self, rect: Rect, counts: &dyn Fn(&Obstacle) -> bool) -> (Rect, Vec<f32>) {
        let centre = self.w / 2.0;
        let mut out = rect;
        let mut tops = Vec::new();
        let sides = self.obstacles.iter().filter(|o| counts(o)).map(|o| o.rect).filter(|r| self.band_kind(*r).is_none());
        for o in sides {
            let overlaps = o.1 < rect.3 && rect.1 < o.3 && o.0 < rect.2 && rect.0 < o.2;
            if !overlaps {
                continue;
            }
            tops.push(o.1);
            if (o.0 + o.2) / 2.0 > centre {
                out.2 = out.2.min(o.0);
            } else {
                out.0 = out.0.max(o.2);
            }
        }
        // Never inverted: a card trimmed from both sides past each other is
        // zero wide (and then undone by the 60% rule).
        out.2 = out.2.max(out.0);
        (out, tops)
    }

    /// Step 9: the smallest vertical shift that moves `rect` clear of every
    /// obstacle `counts` keeps, while keeping it inside the frame (upwards on
    /// a tie), or `None`. A clear position, if any exists, either is the
    /// rule's own (clamped into the frame) or touches a kept obstacle's top
    /// or bottom edge, so those are the only candidates.
    fn clear_shift(&self, rect: Rect, counts: &dyn Fn(&Obstacle) -> bool) -> Option<f32> {
        let h = rect.3 - rect.1;
        let max_top = self.h - h;
        if max_top < 0.0 {
            return None;
        }
        let kept: Vec<Rect> = self.obstacles.iter().filter(|o| counts(o)).map(|o| o.rect).collect();
        let candidates = std::iter::once(rect.1.clamp(0.0, max_top)).chain(kept.iter().flat_map(|o| [o.1 - h, o.3]));
        let mut best: Option<f32> = None;
        for top in candidates {
            let moved = (rect.0, top, rect.2, top + h);
            if !(0.0..=max_top).contains(&top) || kept.iter().any(|o| intersects(moved, *o)) {
                continue;
            }
            let dy = top - rect.1;
            let better = match best {
                None => true,
                Some(b) => dy.abs() < b.abs() || (dy.abs() == b.abs() && dy < b),
            };
            if better {
                best = Some(dy);
            }
        }
        best
    }
}

/// Every obstacle counts.
fn all(_: &Obstacle) -> bool {
    true
}

/// Everything but a placed title card counts (for captions).
fn not_title(o: &Obstacle) -> bool {
    !matches!(o.source, Source::Title(_))
}

/// Only the platform's safe-area rectangles count.
fn safe_areas_only(o: &Obstacle) -> bool {
    matches!(o.source, Source::SafeArea(_))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BandKind {
    Top,
    Bottom,
}

/// One text block before it is positioned.
struct Block {
    role: TextRole,
    size: f32,
    text: String,
}

/// A card's content: an optional image above stacked text blocks.
struct Content<'a> {
    blocks: Vec<Block>,
    image_aspect: Option<f32>,
    padding: f32,
    measure: &'a dyn TextMeasure,
}

impl Content<'_> {
    /// Each element's `(w, h)` at an inner width: the image first, if any,
    /// then each text block wrapped at `inner_w`.
    fn sizes(&self, inner_w: f32) -> (Option<f32>, Vec<(f32, f32)>) {
        let image_h = self.image_aspect.map(|a| inner_w / sane_aspect(a));
        let blocks = self.blocks.iter().map(|b| self.measure.measure(&b.text, b.size, Some(inner_w))).collect();
        (image_h, blocks)
    }

    /// The card's height (padding included) at card width `w`.
    fn height(&self, w: f32) -> f32 {
        let (image_h, blocks) = self.sizes(w - 2.0 * self.padding);
        let heights: Vec<f32> = image_h.into_iter().chain(blocks.iter().map(|b| b.1)).collect();
        let gaps = heights.len().saturating_sub(1) as f32 * BLOCK_GAP;
        2.0 * self.padding + heights.iter().sum::<f32>() + gaps
    }

    /// Positions the content inside `rect`: left-aligned from the top, or
    /// (for a title) centred both ways. Each text block is fit
    /// ([`super::panel::fit_text`], Task 10) to whatever height `rect`
    /// actually leaves once the image and the earlier blocks have taken
    /// their share -- the larger of the remaining room split evenly among
    /// the blocks still to place, and the remaining room less what the
    /// later blocks need at their nominal size (and the gaps between
    /// them), so a block may take room the later ones do not need and
    /// content that fits the card at its nominal size is never shrunk
    /// (Task 11 review) -- so no block's rect can land outside `rect`
    /// (pre-flight review F5). The height `fit_text` reports is also
    /// clamped to that block's own share before it is accepted (Task 10
    /// review F2): even an adversarial or non-deterministic `measure`
    /// cannot make one block's real height eat into room promised to a
    /// later one. A shrink, truncation or drop warns `id` once per
    /// `(id, code)` ([`warn`]).
    fn place(self, id: &str, rect: Rect, centred: bool, warnings: &mut Vec<LayoutWarning>) -> Placed {
        let card_inner_w = (width(rect) - 2.0 * self.padding).max(0.0);
        let inner_h = (rect.3 - rect.1 - 2.0 * self.padding).max(0.0);
        // A wide card puts its image in a left column, full inner height,
        // and the text beside it.
        let beside = self.image_aspect.is_some() && !centred && inner_h > 0.0 && card_inner_w / inner_h > SIDE_IMAGE_ASPECT;
        let column = if beside { card_inner_w * SIDE_IMAGE_SHARE } else { 0.0 };
        let inner_w = if beside { (card_inner_w - column - SIDE_IMAGE_GAP).max(0.0) } else { card_inner_w };
        let text_wrap = beside.then_some(inner_w);
        // Each block's height at its nominal size, wrapped at the text width.
        let natural: Vec<f32> = self.blocks.iter().map(|b| self.measure.measure(&b.text, b.size, Some(inner_w)).1.max(0.0)).collect();
        // The image at the inner width, but never taller than the card
        // leaves once the text has its nominal room (Task 12: a capped
        // lower third would otherwise push the text out of the card) --
        // and never squeezed below IMAGE_MIN_SHARE of the inner height;
        // the text shrinks first. The rect's aspect then differs from the
        // image's, which the draw's `fit` crops or letterboxes.
        let text_need = natural.iter().sum::<f32>() + natural.len() as f32 * BLOCK_GAP;
        let image_h = self
            .image_aspect
            .filter(|_| !beside)
            .map(|a| (inner_w / sane_aspect(a)).min((inner_h - text_need).max(inner_h * IMAGE_MIN_SHARE)).max(0.0));
        let reserved = image_h.map_or(0.0, |h| h + BLOCK_GAP);
        let available_h = (inner_h - reserved).max(0.0);
        let n = self.blocks.len();
        let mut remaining_h = available_h;
        let mut fitted: Vec<(TextRole, f32, String, f32, f32)> = Vec::with_capacity(n);
        for (i, block) in self.blocks.iter().enumerate() {
            let left_count = (n - i) as f32;
            let later = natural.iter().skip(i + 1);
            let later_need = later.clone().sum::<f32>() + later.count() as f32 * BLOCK_GAP;
            let budget_h = (remaining_h / left_count).max(remaining_h - later_need).max(0.0);
            let (size, text, code) = super::panel::fit_text(&block.text, block.size, (inner_w, budget_h), self.measure);
            if let Some(code) = code {
                let verb = match code {
                    "text_shrunk" => "shrank",
                    "text_truncated" => "shrank to the floor and was truncated with an ellipsis",
                    _ => "shrank to the floor, and even an ellipsis did not fit, so it was dropped",
                };
                warn(warnings, id, code, format!("`{id}`'s {:?} text did not fit at its nominal size and {verb}.", block.role));
            }
            let (w, h) = self.measure.measure(&text, size, Some(inner_w));
            // Never trust a measurement past the share this block was given (review F2): clamping here, not just
            // at `fit_text`'s own fit check, means even a measure that answers `fits` one way and a direct
            // `measure` call another cannot make this block's rect -- or the next block's budget -- larger than
            // promised.
            let h = h.min(budget_h.max(0.0));
            remaining_h = (remaining_h - h - BLOCK_GAP).max(0.0);
            fitted.push((block.role, size, text, w, h));
        }
        let stack_h = image_h.into_iter().chain(fitted.iter().map(|f| f.4)).sum::<f32>()
            + (image_h.iter().count() + fitted.len()).saturating_sub(1) as f32 * BLOCK_GAP;
        let mut y = if centred { (rect.1 + rect.3 - stack_h) / 2.0 } else { rect.1 + self.padding };
        let card_left = rect.0 + self.padding;
        let image = if beside {
            Some((card_left, y, card_left + column, y + inner_h))
        } else {
            image_h.map(|ih| {
                let r = (card_left, y, card_left + inner_w, y + ih);
                y += ih + BLOCK_GAP;
                r
            })
        };
        let left = if beside { card_left + column + SIDE_IMAGE_GAP } else { card_left };
        let mut text = Vec::with_capacity(fitted.len());
        for (role, size, txt, w, h) in fitted {
            let w = w.min(inner_w);
            let x = if centred { (rect.0 + rect.2 - w) / 2.0 } else { left };
            text.push((role, (x, y, x + w, y + h), size, txt));
            y += h + BLOCK_GAP;
        }
        Placed { id: id.to_string(), rect, text, image, text_wrap, leader: None }
    }
}

/// A usable width / height: anything non-positive or non-finite is 16:9.
fn sane_aspect(a: f32) -> f32 {
    if a.is_finite() && a > 0.0 {
        a
    } else {
        16.0 / 9.0
    }
}

/// The text blocks of a panel, at the sizes of its placement.
fn panel_blocks(placement: Placement, headline: &Option<String>, body: &Option<String>, source: &Option<String>) -> Vec<Block> {
    let (head, body_size) = if placement == Placement::Title { (TITLE_HEADLINE, TITLE_BODY) } else { (CARD_HEADLINE, CARD_BODY) };
    [(TextRole::Headline, head, headline), (TextRole::Body, body_size, body), (TextRole::Source, SOURCE_SIZE, source)]
        .into_iter()
        .filter_map(|(role, size, text)| text.as_ref().filter(|t| !t.is_empty()).map(|t| Block { role, size, text: t.clone() }))
        .collect()
}

/// Rule 4: the free band inset by [`MARGIN`], kept off side obstacles by
/// whichever costs less area -- trimming its sides (rule 5's trim) or
/// lifting its bottom above them. (The brief's rule 4 names only the band;
/// without this a title would sit on a short-form app's right-hand rail.)
fn title_rect(f: &Frame) -> Rect {
    let (top, bottom) = f.band();
    let rect = (MARGIN, top, f.w - MARGIN, bottom);
    let (trimmed, tops) = f.trim(rect);
    let Some(lift) = tops.iter().copied().reduce(f32::min) else { return rect };
    let lifted = (rect.0, rect.1, rect.2, lift.max(rect.1));
    let area = |r: Rect| width(r).max(0.0) * (r.3 - r.1).max(0.0);
    if area(lifted) > area(trimmed) {
        lifted
    } else {
        trimmed
    }
}

/// Rule 5 (lower-third) and rule 6's portrait bottom sheet: `ideal_w` wide
/// from the left margin, sitting on the free band's bottom, trimmed by side
/// obstacles; a trim under 60% of the ideal width is undone and the card
/// moves up above the obstacles instead. `height` gives the card's height
/// at a width (already capped by placement; this also caps it at the free
/// band's height).
fn bottom_card(f: &Frame, ideal_w: f32, height: &dyn Fn(f32) -> f32) -> Rect {
    let (top, bottom) = f.band();
    // Never taller than the free band: a taller card could not clear it.
    let height = |w: f32| height(w).min((bottom - top).max(0.0));
    let h = height(ideal_w);
    let rect = (MARGIN, bottom - h, MARGIN + ideal_w, bottom);
    let (trimmed, tops) = f.trim(rect);
    if width(trimmed) >= MIN_TRIMMED_FRACTION * ideal_w {
        let h = height(width(trimmed));
        (trimmed.0, bottom - h, trimmed.2, bottom)
    } else {
        let lift = tops.iter().copied().fold(bottom, f32::min);
        (MARGIN, lift - h, MARGIN + ideal_w, lift)
    }
}

/// Rule 6 in landscape: [`SIDE_W`] wide against the right margin,
/// vertically centred in the free band, trimmed as in rule 5.
fn side_card(f: &Frame, height: &dyn Fn(f32) -> f32) -> Rect {
    let (top, bottom) = f.band();
    let centre = (top + bottom) / 2.0;
    let at = |w: f32, h: f32, right: f32| (right - w, centre - h / 2.0, right, centre + h / 2.0);
    let h = height(SIDE_W).min(bottom - top);
    let rect = at(SIDE_W, h, f.w - MARGIN);
    let (trimmed, tops) = f.trim(rect);
    if width(trimmed) >= MIN_TRIMMED_FRACTION * SIDE_W {
        let h = height(width(trimmed)).min(bottom - top);
        (trimmed.0, centre - h / 2.0, trimmed.2, centre + h / 2.0)
    } else {
        let lift = tops.iter().copied().fold(rect.3, f32::min);
        (rect.0, lift - h, rect.2, lift)
    }
}

/// Rule 6b: [`CORNER_W`] wide at the top-left of the free band, or the
/// top-right when the top-left meets an obstacle. When both meet one, the
/// corner that step 9 can clear with the smaller shift (the left on a tie,
/// or when neither clears).
fn corner_card(f: &Frame, h: f32) -> Rect {
    let (top, _) = f.band();
    let left = (MARGIN, top, MARGIN + CORNER_W, top + h);
    let right = (f.w - MARGIN - CORNER_W, top, f.w - MARGIN, top + h);
    if !f.meets_any(left) {
        return left;
    }
    if !f.meets_any(right) {
        return right;
    }
    match (f.clear_shift(left, &all), f.clear_shift(right, &all)) {
        (None, Some(_)) => right,
        (Some(l), Some(r)) if r.abs() < l.abs() => right,
        _ => left,
    }
}

/// A card's rectangle by placement, from its height at a width. `cap` is
/// the lower-third/corner cap; a bottom sheet uses [`SHEET_HEIGHT_CAP`].
fn card_rect(f: &Frame, placement: Placement, height: &dyn Fn(f32) -> f32, corner_cap: f32) -> Rect {
    let card_cap = CARD_HEIGHT_CAP * f.h;
    match placement {
        Placement::Title => title_rect(f),
        Placement::LowerThird => bottom_card(f, (f.w - 2.0 * MARGIN).min(LOWER_THIRD_MAX_W), &|w| height(w).min(card_cap)),
        Placement::Side if f.w > f.h => side_card(f, height),
        Placement::Side => {
            let sheet_cap = SHEET_HEIGHT_CAP * f.h;
            bottom_card(f, f.w - 2.0 * MARGIN, &|w| height(w).min(sheet_cap))
        }
        Placement::Corner => corner_card(f, height(CORNER_W).min(corner_cap)),
    }
}

/// Rule 7: a callout card beside its anchor, clamped into the free band,
/// with a leader to the card's nearest edge midpoint. Its height is
/// capped at the free band's height (pre-flight review F9's rule,
/// generalised beyond the side card), so `fit_text` always has a real
/// budget to fit the label into, however short the band is.
#[allow(clippy::too_many_arguments)]
fn place_callout(
    f: &Frame,
    id: &str,
    anchor: (f32, f32),
    text: &str,
    side: Side,
    image_aspect: Option<f32>,
    measure: &dyn TextMeasure,
    warnings: &mut Vec<LayoutWarning>,
) -> Placed {
    let w = callout_width(text, image_aspect.is_some(), measure);
    let content = Content {
        blocks: vec![Block { role: TextRole::Callout, size: CALLOUT_SIZE, text: text.to_string() }],
        image_aspect,
        padding: CALLOUT_PADDING,
        measure,
    };
    let (top, bottom) = f.band();
    let h = content.height(w).min((bottom - top).max(0.0));
    let y = (anchor.1 - h / 2.0).clamp(top, (bottom - h).max(top));
    let x = match side {
        Side::Right => anchor.0 + CALLOUT_OFFSET,
        Side::Left => anchor.0 - CALLOUT_OFFSET - w,
    };
    let mut placed = content.place(id, (x, y, x + w, y + h), false, warnings);
    placed.leader = Some((anchor, nearest_edge_midpoint(anchor, placed.rect)));
    placed
}

/// The midpoint of `r`'s edge nearest to `p` (left, right, top, bottom on a
/// tie, in that order).
fn nearest_edge_midpoint(p: (f32, f32), r: Rect) -> (f32, f32) {
    let (cx, cy) = ((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0);
    let mids = [(r.0, cy), (r.2, cy), (cx, r.1), (cx, r.3)];
    let d2 = |m: (f32, f32)| (m.0 - p.0).powi(2) + (m.1 - p.1).powi(2);
    mids.into_iter().reduce(|best, m| if d2(m) < d2(best) { m } else { best }).unwrap_or((r.0, cy))
}

/// Rule 8: `capture::chrome::caption_origin`'s arithmetic (ported, since
/// that function is native-only; ruling F28), in units, against every
/// obstacle so far. The caption is fit (Task 10's `fit_text`) to the
/// frame's width between margins and, for height, the room the frame
/// itself allows between the top and bottom margins -- generous, since a
/// caption is rarely more than a couple of lines, but still a real cap so
/// a pathological caption shrinks and ellipsizes rather than running off
/// the frame. Its box includes the plate padding.
///
/// A platform's side rail (a safe-area rectangle that is not a top or
/// bottom band) is the one obstacle a caption narrows for rather than
/// climbs over (final review V2): the rail spans most of a portrait frame,
/// so climbing it put 9:16 captions mid-frame. In the row the caption
/// would sit in -- its rule row, brought inside the free band -- a rail
/// the full-width caption meets trims its span ([`Frame::trim`], as a
/// card is trimmed), the text is re-fit to the narrower width and the
/// caption is anchored in the trimmed span. Only then does the vertical
/// walk move it, for what it still meets (the band, the source line, a
/// card). Short obstacles -- the corner chrome, furniture, cards -- are
/// stepped over as before, so a frame with no safe area places every
/// caption exactly where it always did.
#[allow(clippy::too_many_arguments)]
fn place_caption(
    f: &Frame,
    id: &str,
    text: &str,
    size: f32,
    anchor: Anchor,
    offset: (f32, f32),
    plate: bool,
    measure: &dyn TextMeasure,
    warnings: &mut Vec<LayoutWarning>,
) -> (Placed, Placed) {
    let (pad_x, pad_y) = caption_pad(plate);
    let full_wrap = caption_wrap(f.w, plate);
    let max_h = (f.h - 2.0 * CAPTION_MARGIN - 2.0 * pad_y).max(0.0);
    let gap = CAPTION_MARGIN / 2.0;
    let downward = matches!(anchor, Anchor::TopLeft | Anchor::Top | Anchor::TopRight);
    let (band_top, band_bottom) = f.band();
    // The text fit at `wrap`, its size and its box `(w, h)`.
    let fit = |wrap: f32| {
        let (size, text, code) = super::panel::fit_text(text, size, (wrap, max_h), measure);
        let (tw, th) = measure.measure(&text, size, Some(wrap));
        (size, text, code, (tw, th), (tw + 2.0 * pad_x, th + 2.0 * pad_y))
    };
    // `(left, right)`: the span a caption is anchored in horizontally.
    let rule_x = |span: (f32, f32), w: f32| match anchor {
        Anchor::TopLeft | Anchor::Left | Anchor::BottomLeft => span.0,
        Anchor::Top | Anchor::Center | Anchor::Bottom => (span.0 + span.1 - w) / 2.0,
        Anchor::TopRight | Anchor::Right | Anchor::BottomRight => span.1 - w,
    };
    let rule_y = |h: f32| match anchor {
        Anchor::TopLeft | Anchor::Top | Anchor::TopRight => CAPTION_MARGIN,
        Anchor::Left | Anchor::Center | Anchor::Right => (f.h - h) / 2.0,
        Anchor::BottomLeft | Anchor::Bottom | Anchor::BottomRight => f.h - CAPTION_MARGIN - h,
    } + offset.1;
    // The row the walk would bring a caption `h` tall into: its rule row,
    // inside the free band.
    let row = |h: f32| {
        let y = rule_y(h);
        if downward {
            y.max(band_top + gap)
        } else if matches!(anchor, Anchor::Left | Anchor::Center | Anchor::Right) {
            y
        } else {
            y.min(band_bottom - gap - h)
        }
    };
    let full = (CAPTION_MARGIN, f.w - CAPTION_MARGIN);
    let mut span = full;
    let mut fitted = fit(full_wrap);
    let mut wrap = full_wrap;
    // Twice: a re-fit can add a line, which moves the row.
    for _ in 0..2 {
        let (.., (w, h)) = fitted;
        let y = row(h);
        let x = rule_x(span, w) + offset.0;
        let rails_met = f.meets((x, y, x + w, y + h), &|o| is_rail(f, o));
        if !rails_met {
            break;
        }
        let (trimmed, _) = f.trim_by((span.0, y, span.1, y + h), &|o| is_rail(f, o));
        let trimmed_wrap = (width(trimmed) - 2.0 * pad_x).max(0.0);
        // Never a sliver: under 60% of the full span the caption climbs
        // instead, as a trimmed card would.
        if width(trimmed) < MIN_TRIMMED_FRACTION * (full.1 - full.0) || trimmed_wrap >= wrap {
            break;
        }
        span = (trimmed.0, trimmed.2);
        wrap = trimmed_wrap;
        fitted = fit(wrap);
    }
    let (size, text, code, (tw, th), (w, h)) = fitted;
    if let Some(code) = code {
        let verb = match code {
            "text_shrunk" => "shrank",
            "text_truncated" => "shrank to the floor and was truncated with an ellipsis",
            _ => "shrank to the floor, and even an ellipsis did not fit, so it was dropped",
        };
        warn(warnings, id, code, format!("`{id}`'s caption text did not fit and {verb}."));
    }
    let text = text.as_str();
    let text_wrap = (wrap < full_wrap).then_some(wrap);
    let x = rule_x(span, w) + offset.0;
    let mut y = rule_y(h);
    let first_y = y;
    for _ in 0..f.obstacles.len() {
        let mut moved = false;
        for o in f.obstacles.iter().filter(|o| not_title(o)) {
            if intersects((x, y, x + w, y + h), o.rect) {
                y = if downward { o.rect.3 + gap } else { o.rect.1 - gap - h };
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    let x = x.clamp(0.0, (f.w - w).max(0.0));
    let at = |y: f32| {
        let y = y.clamp(0.0, (f.h - h).max(0.0));
        let inner = (x + pad_x, y + pad_y, x + pad_x + tw, y + pad_y + th);
        Placed { id: id.to_string(), rect: (x, y, x + w, y + h), text: vec![(TextRole::Caption, inner, size, text.to_string())], image: None, text_wrap, leader: None }
    };
    (at(y), at(first_y))
}

/// A platform side rail: a safe-area rectangle that is neither a top nor
/// a bottom band (what a caption narrows for: [`place_caption`]).
fn is_rail(f: &Frame, o: &Obstacle) -> bool {
    safe_areas_only(o) && f.band_kind(o.rect).is_none()
}

/// The rule placement for one item, or `None` for an item placed elsewhere
/// (furniture: [`place_furniture`]) or one that cannot be placed (a callout whose
/// anchor is not a finite point). The second value is where step 9's
/// safe-area fallback starts: the caption's rule position before its
/// push-away walked it (`None`: the first value is the rule position).
fn place(f: &Frame, item: &LayoutItem, measure: &dyn TextMeasure, warnings: &mut Vec<LayoutWarning>) -> Option<(Placed, Option<Placed>)> {
    Some(match item {
        LayoutItem::Panel { id, placement, headline, body, source, image_aspect, theme } => {
            let content = Content { blocks: panel_blocks(*placement, headline, body, source), image_aspect: *image_aspect, padding: theme.padding, measure };
            let rect = card_rect(f, *placement, &|w| content.height(w), CARD_HEIGHT_CAP * f.h);
            (content.place(id, rect, *placement == Placement::Title, warnings), None)
        }
        LayoutItem::Image { id, placement, aspect } => {
            let aspect = sane_aspect(*aspect);
            let rect = card_rect(f, *placement, &|w| w / aspect, CORNER_IMAGE_MAX_H);
            (Placed { id: id.clone(), rect, text: Vec::new(), image: Some(rect), text_wrap: None, leader: None }, None)
        }
        LayoutItem::Furniture { .. } => return None,
        LayoutItem::Callout { id, anchor, text, side, image_aspect, .. } => {
            if !(anchor.0.is_finite() && anchor.1.is_finite()) {
                warn(warnings, id, "callout_offscreen", format!("`{id}` has no finite anchor ({anchor:?}); it is not drawn"));
                return None;
            }
            (place_callout(f, id, *anchor, text, *side, *image_aspect, measure, warnings), None)
        }
        LayoutItem::Caption { id, text, size, anchor, offset, plate } => {
            let size = if size.is_finite() && *size > 0.0 {
                *size
            } else {
                warn(warnings, id, "bad_input", format!("`{id}` has text size {size}; drawn at {TEXT_FLOOR}"));
                TEXT_FLOOR
            };
            let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
            if !(offset.0.is_finite() && offset.1.is_finite()) {
                warn(warnings, id, "bad_input", format!("`{id}` has offset {offset:?}; non-finite parts are 0"));
            }
            let (walked, rule) = place_caption(f, id, text, size, *anchor, (finite(offset.0), finite(offset.1)), *plate, measure, warnings);
            (walked, Some(rule))
        }
    })
}

/// Map furniture's text size, 1080 units (the floor: it truncates rather
/// than shrinks).
pub const FURNITURE_TEXT: f32 = 28.0;
/// A furniture plate's padding around its content, 1080 units.
pub const FURNITURE_PAD: f32 = 8.0;
/// The piece ids' suffixes, in placement order: `<item id>/<suffix>`.
pub const FURNITURE_PIECES: [&str; 4] = ["source", "scale", "legend", "north"];
/// Space between a scale bar's label and its bar, and between two bars.
pub const SCALE_LABEL_GAP: f32 = 2.0;
pub const SCALE_ROW_GAP: f32 = 6.0;
/// A scale bar's end ticks (their height) and the bar (its thickness),
/// the bar along the ticks' bottom.
pub const SCALE_TICK: f32 = 10.0;
pub const SCALE_BAR: f32 = 4.0;
/// A tick's width.
pub const SCALE_TICK_W: f32 = 2.0;
/// A legend swatch's column (the widest swatch), the gap after it, and the
/// space between rows.
pub const LEGEND_SWATCH: f32 = 24.0;
pub const LEGEND_SWATCH_GAP: f32 = 8.0;
pub const LEGEND_ROW_GAP: f32 = 4.0;
/// A legend's widest, before the frame's own cap (half its width less the
/// margin).
pub const LEGEND_MAX_W: f32 = 420.0;
/// The source block's widest, 1080 units, before the frame's own cap.
pub const SOURCE_MAX_W: f32 = 960.0;
/// What the source block leaves beside it for the scale bar (its widest
/// box and a gap each side), so the two can share the bottom row.
const SCALE_RESERVE: f32 = super::furniture::SCALE_BAR_MAX_UNITS + 2.0 * FURNITURE_PAD + 2.0 * GAP;
/// The compass (spec §4.5's north arrow): a round plate, the two-tone
/// needle on it (its length, tip to tip), and an upright "N" outside the
/// plate at the needle's turned north tip -- its centre [`NORTH_N_RADIUS`]
/// from the plate's, so the "N" circles the plate as the bearing turns.
pub const NORTH_PLATE: f32 = 48.0;
pub const NORTH_ARROW: f32 = 40.0;
/// The "N"'s size (the text floor) and the half-extent its box is given.
pub const NORTH_N_UNITS: f32 = TEXT_FLOOR;
const NORTH_N_HALF: f32 = 0.6 * NORTH_N_UNITS;
/// The plate's radius, a 4-unit gap, then the "N"'s half-extent.
pub const NORTH_N_RADIUS: f32 = NORTH_PLATE / 2.0 + 4.0 + NORTH_N_HALF;
/// The compass's box: room for the "N" at any bearing, the plate centred.
pub const NORTH_BOX: f32 = 2.0 * (NORTH_N_RADIUS + NORTH_N_HALF);

/// `p` moved by `(dx, dy)`.
fn translate(p: &mut Placed, dx: f32, dy: f32) {
    let by = |r: Rect| (r.0 + dx, r.1 + dy, r.2 + dx, r.3 + dy);
    p.rect = by(p.rect);
    p.image = p.image.map(by);
    for t in &mut p.text {
        t.1 = by(t.1);
    }
}

/// One line of furniture text fit (Task 10's `fit_text`) to `max_w`: never
/// shrunk below [`FURNITURE_TEXT`] (the floor), truncated with an ellipsis
/// instead. `(text as drawn, w, h)`; empty when even "…" does not fit.
fn furniture_line(id: &str, text: &str, max_w: f32, measure: &dyn TextMeasure, warnings: &mut Vec<LayoutWarning>) -> (String, f32, f32) {
    let one_line_h = measure.measure(text, FURNITURE_TEXT, None).1.max(measure.measure("Ag", FURNITURE_TEXT, None).1);
    let (size, fitted, code) = super::panel::fit_text(text, FURNITURE_TEXT, (max_w.max(0.0), one_line_h), measure);
    if let Some(code) = code {
        warn(warnings, id, code, format!("`{id}`'s text `{text}` did not fit {max_w} units on one line and was cut"));
    }
    if fitted.is_empty() {
        return (fitted, 0.0, 0.0);
    }
    let (w, h) = measure.measure(&fitted, size, None);
    (fitted, w.min(max_w.max(0.0)), h.min(one_line_h))
}

/// A furniture piece's content at the origin: its box and its text blocks
/// (each one line, at [`FURNITURE_TEXT`]).
fn furniture_piece(
    id: &str,
    piece: &str,
    frame: &FurnitureFrame,
    max_w: f32,
    measure: &dyn TextMeasure,
    warnings: &mut Vec<LayoutWarning>,
) -> Option<Placed> {
    let pad = FURNITURE_PAD;
    let inner_max = (max_w - 2.0 * pad).max(0.0);
    let mut text: Vec<(TextRole, Rect, f32, String)> = Vec::new();
    let (w, h) = match piece {
        "source" => {
            let lines: Vec<&String> = frame.source_line.iter().chain(frame.byline.iter()).filter(|t| !t.trim().is_empty()).collect();
            if lines.is_empty() {
                return None;
            }
            let mut y = pad;
            let mut widest: f32 = 0.0;
            for (k, line) in lines.into_iter().enumerate() {
                if k > 0 {
                    y += SCALE_LABEL_GAP;
                }
                let (t, tw, th) = furniture_line(id, line, inner_max, measure, warnings);
                text.push((TextRole::Source, (pad, y, pad + tw, y + th), FURNITURE_TEXT, t));
                widest = widest.max(tw);
                y += th;
            }
            // Right-aligned: every line ends at the box's right padding.
            let w = widest + 2.0 * pad;
            for t in &mut text {
                let tw = t.1 .2 - t.1 .0;
                t.1 = (w - pad - tw, t.1 .1, w - pad, t.1 .3);
            }
            (w, y + pad)
        }
        "scale" => {
            let bars = &frame.scale_bar.as_ref()?.bars;
            if bars.is_empty() {
                return None;
            }
            let mut y = pad;
            let mut widest: f32 = 0.0;
            for (k, (label, len)) in bars.iter().enumerate() {
                if k > 0 {
                    y += SCALE_ROW_GAP;
                }
                let (t, tw, th) = furniture_line(id, label, inner_max, measure, warnings);
                text.push((TextRole::Furniture, (pad, y, pad + tw, y + th), FURNITURE_TEXT, t));
                let len = if len.is_finite() { len.clamp(0.0, inner_max) } else { 0.0 };
                widest = widest.max(tw).max(len + SCALE_TICK_W);
                y += th + SCALE_LABEL_GAP + SCALE_TICK;
            }
            (widest + 2.0 * pad, y + pad)
        }
        "legend" => {
            let legend = frame.legend.as_ref()?;
            if legend.entries.is_empty() {
                return None;
            }
            let mut y = pad;
            let mut widest: f32 = 0.0;
            if let Some(title) = legend.title.as_ref().filter(|t| !t.trim().is_empty()) {
                let (t, tw, th) = furniture_line(id, title, inner_max, measure, warnings);
                text.push((TextRole::Furniture, (pad, y, pad + tw, y + th), FURNITURE_TEXT, t));
                widest = widest.max(tw);
                y += th + LEGEND_ROW_GAP;
            }
            let column = LEGEND_SWATCH + LEGEND_SWATCH_GAP;
            for (k, (_, label)) in legend.entries.iter().enumerate() {
                if k > 0 {
                    y += LEGEND_ROW_GAP;
                }
                let (t, tw, th) = furniture_line(id, label, (inner_max - column).max(0.0), measure, warnings);
                // The row is as tall as the swatch or the text, the text centred in it.
                let row = th.max(LEGEND_SWATCH);
                let top = y + (row - th) / 2.0;
                text.push((TextRole::Furniture, (pad + column, top, pad + column + tw, top + th), FURNITURE_TEXT, t));
                widest = widest.max(column + tw);
                y += row;
            }
            (widest + 2.0 * pad, y + pad)
        }
        "north" => {
            frame.north_arrow?;
            (NORTH_BOX, NORTH_BOX)
        }
        _ => return None,
    };
    Some(Placed { id: id.to_string(), rect: (0.0, 0.0, w, h), text, image: None, text_wrap: None, leader: None })
}

/// The bottom-right chrome (the credit plate: the rightmost corner whose
/// centre is in the frame's bottom-right quarter) and the top of the
/// bottom-left chrome (the mark and the wordmark), 1080 units, raw.
fn chrome_anchors(f: &Frame, chrome: &[Rect]) -> (Option<Rect>, Option<f32>) {
    let centre = |r: &Rect| ((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0);
    let finite = |r: &Rect| [r.0, r.1, r.2, r.3].iter().all(|v| v.is_finite());
    let credit = chrome
        .iter()
        .filter(|r| finite(r) && centre(r).0 >= f.w / 2.0 && centre(r).1 > f.h / 2.0)
        .copied()
        .reduce(|a, b| if b.2 > a.2 { b } else { a });
    let mark_top = chrome.iter().filter(|r| finite(r) && centre(r).0 < f.w / 2.0 && centre(r).1 > f.h / 2.0).map(|r| r.1).reduce(f32::min);
    (credit, mark_top)
}

/// Where a furniture piece's rule rectangle goes when it meets something:
/// the clear position with the least movement (`|dx| + |dy|`, the smaller
/// `|dy|` then upwards on a tie) among every top step 9 would try (the
/// rule's own, and against each obstacle's top and bottom), each kept
/// where it is or slid sideways off what it meets (left for a
/// right-aligned piece) -- a short-form app's side rail would otherwise
/// send a corner piece the whole rail's height. `None` when nothing
/// inside the frame clears.
fn furniture_shift(f: &Frame, rect: Rect, slide_left: bool) -> Option<(f32, f32)> {
    let h = rect.3 - rect.1;
    let max_top = f.h - h;
    if max_top < 0.0 {
        return None;
    }
    let tops = std::iter::once(rect.1.clamp(0.0, max_top)).chain(f.obstacles.iter().flat_map(|o| [o.rect.1 - h, o.rect.3]));
    let mut best: Option<(f32, f32)> = None;
    let key = |c: (f32, f32)| (c.0.abs() + c.1.abs(), c.1.abs(), c.1);
    for top in tops.filter(|t| (0.0..=max_top).contains(t)) {
        let mut moved = (rect.0, top, rect.2, top + h);
        // Each step clears the obstacle it slides past; as many steps as
        // there are obstacles settle it.
        for _ in 0..=f.obstacles.len() {
            let hit = f.obstacles.iter().map(|o| o.rect).filter(|o| intersects(moved, *o)).reduce(|a, b| if slide_left == (b.0 < a.0) { b } else { a });
            let Some(o) = hit else { break };
            let dx = if slide_left { o.0 - moved.2 } else { o.2 - moved.0 };
            moved = (moved.0 + dx, moved.1, moved.2 + dx, moved.3);
        }
        if moved.0 < 0.0 || moved.2 > f.w || f.meets_any(moved) {
            continue;
        }
        let candidate = (moved.0 - rect.0, top - rect.1);
        if best.is_none_or(|b| key(candidate) < key(b)) {
            best = Some(candidate);
        }
    }
    best
}

/// Places a furniture item's pieces ([`FURNITURE_PIECES`], in that order),
/// each joining the obstacles as it is placed:
/// - the source line and byline stacked, right-aligned to the credit
///   plate and sitting [`GAP`] above it (it is an obstacle, so they never
///   meet it), at most [`SOURCE_MAX_W`] wide and leaving the scale bar its
///   room (never less than half the frame less the margin);
/// - the scale bar bottom-left, above the SWAT mark and the wordmark;
/// - the legend top-right of the free band;
/// - the north arrow top-left of the free band (under a corner card there,
///   which was placed first).
///
/// Each moves off what it meets as [`furniture_shift`] says, or warns like
/// any other item.
fn place_furniture(f: &mut Frame, chrome: &[Rect], id: &str, frame: &FurnitureFrame, measure: &Checked<'_>, out: &mut Layout) {
    let (credit, mark_top) = chrome_anchors(f, chrome);
    let half = (f.w / 2.0 - MARGIN).max(0.0);
    for piece in FURNITURE_PIECES {
        let piece_id = format!("{id}/{piece}");
        measure.bad.set(false);
        let max_w = match piece {
            "legend" => LEGEND_MAX_W.min(half),
            // Up to the frame less its margins and the scale bar's room,
            // never under half the frame (a portrait frame's one line).
            "source" => SOURCE_MAX_W.min(f.w - 2.0 * MARGIN - SCALE_RESERVE).max(half),
            _ => half,
        };
        if piece == "scale" {
            if let Some(pitch) = frame.scale_bar_hidden_pitch {
                let message = format!("the scale bar is hidden: the camera is pitched {pitch:.1} degrees (over {}), so no one length holds across the frame", super::furniture::SCALE_BAR_MAX_PITCH_DEG);
                warn(&mut out.warnings, &piece_id, "scale_bar_hidden_pitch", message);
            }
        }
        let Some(mut placed) = furniture_piece(&piece_id, piece, frame, max_w, measure, &mut out.warnings) else { continue };
        if measure.bad.get() {
            warn(&mut out.warnings, &piece_id, "bad_measure", format!("measuring `{piece_id}`'s text gave a NaN, infinite or negative size; it was taken as 0"));
        }
        let (w, h) = (width(placed.rect), placed.rect.3 - placed.rect.1);
        let (band_top, _) = f.band();
        let (x, y) = match piece {
            "source" => match credit {
                Some(c) => (c.2 - w, c.1 - GAP - h),
                None => (f.w - MARGIN - w, f.h - MARGIN - h),
            },
            "scale" => (MARGIN, mark_top.map_or(f.h - MARGIN, |t| t - GAP) - h),
            "legend" => (f.w - MARGIN - w, band_top),
            _ => (MARGIN, band_top),
        };
        translate(&mut placed, x, y);
        placed.rect = upright(placed.rect);
        if f.meets_any(placed.rect) {
            match furniture_shift(f, placed.rect, matches!(piece, "source" | "legend")) {
                Some((dx, dy)) => translate(&mut placed, dx, dy),
                None => {
                    if let Some(dy) = f.clear_shift(placed.rect, &safe_areas_only) {
                        translate(&mut placed, 0.0, dy);
                    }
                }
            }
            for code in ["in_safe_zone", "overlap"] {
                let names: Vec<String> = f
                    .obstacles
                    .iter()
                    .filter(|o| (safe_areas_only(o) == (code == "in_safe_zone")) && intersects(placed.rect, o.rect))
                    .map(|o| format!("`{}`", o.source.name()))
                    .collect();
                if !names.is_empty() {
                    let message = format!("`{}` could not be placed clear of {}", placed.id, names.join(", "));
                    warn(&mut out.warnings, &placed.id, code, message);
                }
            }
        }
        f.obstacles.push(Obstacle { rect: grow(placed.rect, GAP), source: Source::Placed(placed.id.clone()) });
        out.placed.push(placed);
    }
}

/// Pushes a warning unless one with the same `(id, code)` is already there.
fn warn(warnings: &mut Vec<LayoutWarning>, id: &str, code: &'static str, message: String) {
    if !warnings.iter().any(|w| w.id == id && w.code == code) {
        warnings.push(LayoutWarning { id: id.to_string(), code, message });
    }
}

/// A [`TextMeasure`] that never returns NaN, infinity or a negative size:
/// such a result becomes `(0, 0)` and is remembered, so the item it was
/// measured for gets a `bad_measure` warning.
struct Checked<'a> {
    inner: &'a dyn TextMeasure,
    bad: std::cell::Cell<bool>,
}

impl TextMeasure for Checked<'_> {
    fn measure(&self, text: &str, size: f32, wrap: Option<f32>) -> (f32, f32) {
        let (w, h) = self.inner.measure(text, size, wrap);
        if w.is_finite() && h.is_finite() && w >= 0.0 && h >= 0.0 {
            (w, h)
        } else {
            self.bad.set(true);
            (0.0, 0.0)
        }
    }
}

/// `r` with its right edge not left of its left edge and its bottom not
/// above its top.
fn upright(r: Rect) -> Rect {
    (r.0, r.1, r.2.max(r.0), r.3.max(r.1))
}

/// Moves everything in `p` down by `dy` (the leader's anchor end stays put).
fn shift(p: &mut Placed, dy: f32) {
    let down = |r: Rect| (r.0, r.1 + dy, r.2, r.3 + dy);
    p.rect = down(p.rect);
    p.image = p.image.map(down);
    for t in &mut p.text {
        t.1 = down(t.1);
    }
    if let Some((anchor, _)) = p.leader {
        p.leader = Some((anchor, nearest_edge_midpoint(anchor, p.rect)));
    }
}

/// Places `items` on a frame of `frame` PHYSICAL pixels at `pixel_ratio`,
/// clear of `safe_area` and `chrome_corners` (1080 units) and of each
/// other. Everything returned is in 1080 units (multiply by `pixel_ratio`
/// to draw). The spec's `layout(frame, pixel_ratio, safe_area,
/// elements_at_t, chrome_corners)`, plus `measure` (plan ruling D4).
pub fn layout(
    frame: (u32, u32),
    pixel_ratio: f32,
    safe_area: &[Rect],
    items: &[LayoutItem],
    chrome_corners: &[Rect],
    measure: &dyn TextMeasure,
) -> Layout {
    let ratio = if pixel_ratio.is_finite() && pixel_ratio > 0.0 { pixel_ratio } else { 1.0 };
    let obstacles = (safe_area.iter().enumerate().map(|(i, r)| (*r, Source::SafeArea(i))))
        .chain(chrome_corners.iter().enumerate().map(|(i, r)| (*r, Source::Chrome(i))))
        .map(|(r, source)| Obstacle { rect: grow(r, GAP), source })
        .collect();
    let mut f = Frame { w: frame.0 as f32 / ratio, h: frame.1 as f32 / ratio, obstacles };
    let mut out = Layout::default();
    // A frame with no room inside its margins, or whose room is all platform
    // UI, places nothing (a live map can be 0x0 mid-resize).
    let min_side = 2.0 * MARGIN + 1.0;
    let inner = (MARGIN, MARGIN, f.w - MARGIN, f.h - MARGIN);
    let covered = safe_area.iter().any(|r| r.0 <= inner.0 && r.1 <= inner.1 && r.2 >= inner.2 && r.3 >= inner.3);
    let (band_top, band_bottom) = f.band();
    if f.w < min_side || f.h < min_side || covered || band_bottom - band_top < 1.0 {
        let message = format!("the frame ({} x {} units) has no room for story elements inside its margins and safe areas", f.w, f.h);
        warn(&mut out.warnings, "frame", "frame_too_small", message);
        return out;
    }
    let measure = Checked { inner: measure, bad: std::cell::Cell::new(false) };
    let mut order: Vec<(Priority, usize, &LayoutItem)> = items.iter().enumerate().map(|(i, item)| (item.priority(), i, item)).collect();
    order.sort_by_key(|(priority, i, _)| (*priority, *i));
    for (_, _, item) in order {
        if let LayoutItem::Furniture { id, frame } = item {
            place_furniture(&mut f, chrome_corners, id, frame, &measure, &mut out);
            continue;
        }
        measure.bad.set(false);
        let placed = place(&f, item, &measure, &mut out.warnings);
        if measure.bad.get() {
            let id = item.id();
            warn(&mut out.warnings, id, "bad_measure", format!("measuring `{id}`'s text gave a NaN, infinite or negative size; it was taken as 0"));
        }
        let Some((mut placed, rule)) = placed else { continue };
        placed.rect = upright(placed.rect);
        let is_title = matches!(item, LayoutItem::Panel { placement: Placement::Title, .. });
        // Captions draw over a title card (controller ruling): it is not an
        // obstacle to them.
        let counts: &dyn Fn(&Obstacle) -> bool = if matches!(item, LayoutItem::Caption { .. }) { &not_title } else { &all };
        if f.meets(placed.rect, counts) {
            // Step 9. Safe areas are the hard constraint: when nothing clears
            // every obstacle, the item goes back to its rule position and
            // takes the smallest shift that clears the safe areas alone; what
            // still overlaps is warned about.
            match f.clear_shift(placed.rect, counts) {
                Some(dy) => shift(&mut placed, dy),
                None => {
                    if let Some(rule) = rule {
                        placed = rule;
                    }
                    if let Some(dy) = f.clear_shift(placed.rect, &safe_areas_only) {
                        shift(&mut placed, dy);
                    }
                }
            }
            for code in ["in_safe_zone", "overlap"] {
                let names: Vec<String> = f
                    .obstacles
                    .iter()
                    .filter(|o| counts(o) && (safe_areas_only(o) == (code == "in_safe_zone")) && intersects(placed.rect, o.rect))
                    .map(|o| format!("`{}`", o.source.name()))
                    .collect();
                if !names.is_empty() {
                    let message = format!("`{}` could not be placed clear of {}", placed.id, names.join(", "));
                    warn(&mut out.warnings, &placed.id, code, message);
                }
            }
        }
        if is_title {
            out.dim = Some(TITLE_DIM);
        }
        let source = if is_title { Source::Title(placed.id.clone()) } else { Source::Placed(placed.id.clone()) };
        f.obstacles.push(Obstacle { rect: grow(placed.rect, GAP), source });
        out.placed.push(placed);
    }
    out
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Helpers shared by layout's tests and Task 10's (pre-flight ruling F1).
    use super::*;
    use crate::story::tiers::{frame_size, Aspect, Tier};

    pub(crate) const ALL_ASPECTS: [Aspect; 4] = [Aspect::Landscape, Aspect::Portrait, Aspect::Feed, Aspect::Square];

    /// A monospace measure: every character is `0.5 * size` wide, words wrap
    /// greedily at `wrap`, every line is `1.2 * size` tall.
    pub(crate) fn mono(text: &str, size: f32, wrap: Option<f32>) -> (f32, f32) {
        let char_w = 0.5 * size;
        let mut lines: Vec<usize> = Vec::new();
        for word in text.split(' ').filter(|w| !w.is_empty()) {
            let n = word.chars().count();
            match lines.last_mut() {
                Some(line) if wrap.is_none_or(|wrap| (*line + 1 + n) as f32 * char_w <= wrap) => *line += 1 + n,
                _ => lines.push(n),
            }
        }
        let widest = lines.iter().copied().max().unwrap_or(0);
        (widest as f32 * char_w, lines.len() as f32 * 1.2 * size)
    }

    pub(crate) struct Mono;
    impl TextMeasure for Mono {
        fn measure(&self, text: &str, size: f32, wrap: Option<f32>) -> (f32, f32) {
            mono(text, size, wrap)
        }
    }

    /// A dark, headline-only panel.
    pub(crate) fn panel(id: &str, placement: Placement, headline: &str) -> LayoutItem {
        LayoutItem::Panel {
            id: id.to_string(),
            placement,
            headline: Some(headline.to_string()),
            body: None,
            source: None,
            image_aspect: None,
            theme: crate::story::panel::theme(Some("dark")).unwrap(),
        }
    }

    /// The SWAT mark and the credit plate as `chrome::layout` places them at
    /// ratio 1, in 1080 units.
    pub(crate) fn corners(aspect: Aspect) -> Vec<Rect> {
        let (w, h) = frame_size(aspect, Tier::Fullhd);
        let (w, h) = (w as f32, h as f32);
        vec![(16.0, h - 56.0, 136.0, h - 16.0), (w - 416.0, h - 36.0, w - 16.0, h - 16.0)]
    }

    pub(crate) fn intersects(a: Rect, b: Rect) -> bool {
        super::intersects(a, b)
    }

    /// Every furniture piece: a source line and byline, a metric and an
    /// imperial scale bar, a titled three-class legend and a north arrow.
    pub(crate) fn furniture_frame(source_line: &str) -> FurnitureFrame {
        use crate::story::furniture::{Legend, LegendSwatch, ScaleBar};
        FurnitureFrame {
            source_line: Some(source_line.to_string()),
            byline: Some("By A. Reporter".into()),
            scale_bar: Some(ScaleBar { bars: vec![("10 km".into(), 130.8), ("5 mi".into(), 105.3)] }),
            legend: Some(Legend {
                title: Some("Vessel".into()),
                entries: vec![
                    (LegendSwatch::Line([31, 119, 180, 255], 3.0), "ferry".into()),
                    (LegendSwatch::Line([228, 87, 46, 255], 3.0), "cargo".into()),
                    (LegendSwatch::Line([153, 153, 153, 255], 3.0), "Other".into()),
                ],
            }),
            north_arrow: Some(30.0),
            scale_bar_hidden_pitch: None,
        }
    }

    /// One of every kind that claims space (no title: a title card fills
    /// the free band, so everything else would only overlap it).
    pub(crate) fn all_kinds() -> Vec<LayoutItem> {
        let dark = crate::story::panel::theme(Some("dark")).unwrap();
        vec![
            LayoutItem::Panel {
                id: "/events/0".into(),
                placement: Placement::LowerThird,
                headline: Some("Harbour".into()),
                body: Some("The busiest port in the region by container volume".into()),
                source: Some("Port authority, 2025".into()),
                image_aspect: None,
                theme: dark.clone(),
            },
            LayoutItem::Panel {
                id: "/events/1".into(),
                placement: Placement::Side,
                headline: Some("Strait".into()),
                body: Some("A narrow channel".into()),
                source: None,
                image_aspect: None,
                theme: dark.clone(),
            },
            panel("/events/2", Placement::Corner, "Day 3"),
            LayoutItem::Image { id: "/events/3".into(), placement: Placement::Corner, aspect: 16.0 / 9.0 },
            LayoutItem::Furniture { id: "furniture".into(), frame: furniture_frame("Source: Port authority, 2025") },
            LayoutItem::Callout {
                id: "/events/4".into(),
                anchor: (400.0, 500.0),
                text: "Terminal 4".into(),
                side: Side::Right,
                image_aspect: None,
                theme: dark.clone(),
            },
            LayoutItem::Caption {
                id: "caption:0".into(),
                text: "Container traffic doubled since 2019".into(),
                size: 32.0,
                anchor: Anchor::Bottom,
                offset: (0.0, 0.0),
                plate: true,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{all_kinds, corners, intersects, mono, panel, Mono, ALL_ASPECTS};
    use super::*;
    use crate::story::tiers::{frame_size, pixel_ratio, safe_area_rects, Aspect, SafeArea, Tier};

    fn dark() -> PanelTheme {
        crate::story::panel::theme(Some("dark")).unwrap()
    }

    fn assert_rect(got: Rect, want: Rect, what: &str) {
        for (g, w) in [got.0, got.1, got.2, got.3].into_iter().zip([want.0, want.1, want.2, want.3]) {
            assert!((g - w).abs() < 0.01, "{what}: got {got:?}, want {want:?}");
        }
    }

    /// Task 11 review: a card sized for its content at nominal size draws
    /// that content at nominal size. A body that wraps to two lines once
    /// took only an even share of the room left after the headline and
    /// shrank (16:9) or was truncated (9:16) while the card's bottom stayed
    /// empty.
    #[test]
    fn a_two_line_body_in_a_card_with_room_is_not_shrunk_or_truncated() {
        let body = "Container berths moved west to Pasir Panjang in the 2000s, freeing the waterfront for the new downtown.";
        for (aspect, area) in [(Aspect::Landscape, SafeArea::None), (Aspect::Portrait, SafeArea::Reels), (Aspect::Portrait, SafeArea::None)] {
            let (w, h) = frame_size(aspect, Tier::Fullhd);
            let item = LayoutItem::Panel {
                id: "/events/0".into(),
                placement: Placement::LowerThird,
                headline: Some("Harbour".into()),
                body: Some(body.into()),
                source: Some("Source: MPA".into()),
                image_aspect: None,
                theme: crate::story::panel::theme(Some("dark")).unwrap(),
            };
            let l = layout((w, h), 1.0, &safe_area_rects(area, aspect), &[item], &corners(aspect), &Mono);
            assert!(l.warnings.iter().all(|w| !w.code.starts_with("text_")), "{aspect:?}/{area:?}: {:?}", l.warnings);
            let p = &l.placed[0];
            let body_block = p.text.iter().find(|t| t.0 == TextRole::Body).unwrap();
            assert_eq!(body_block.3, body, "{aspect:?}: the body is whole");
            assert_eq!(body_block.2, CARD_BODY, "{aspect:?}: at its nominal size");
            assert!(body_block.1 .3 - body_block.1 .1 > 1.5 * 1.2 * CARD_BODY, "{aspect:?}: two lines, {:?}", body_block.1);
            for t in &p.text {
                assert!(t.1 .0 >= p.rect.0 && t.1 .2 <= p.rect.2 + 1e-3 && t.1 .1 >= p.rect.1 && t.1 .3 <= p.rect.3 + 1e-3, "{aspect:?}: {t:?} leaves {:?}", p.rect);
            }
        }
    }

    #[test]
    fn the_mono_measure_wraps_greedily_by_words() {
        let (w, h) = mono("Harbour", 56.0, None);
        assert_eq!(w, 196.0);
        assert!((h - 67.2).abs() < 1e-4);
        // 10 units a char at 20: "aa bb" is 50 wide, "cc" goes to line 2.
        let (w, h) = mono("aa bb cc", 20.0, Some(55.0));
        assert_eq!(w, 50.0);
        assert!((h - 48.0).abs() < 1e-4);
        assert_eq!(mono("", 20.0, None), (0.0, 0.0));
    }

    #[test]
    fn the_table_of_placements_matches_hand_computed_rectangles() {
        // A headline-only dark panel is 24 + 1.2 * 56 + 24 = 115.2 units tall under Mono.
        let rows: Vec<(Aspect, SafeArea, Placement, Rect)> = vec![
            // 16:9, no safe area: free band bottom = 1080 - 48 = 1032. The mark (grown: 0..152 x 1008..1080) overlaps
            // vertically and sits left of centre: trim the left to 152; width 1248 - 152 = 1096 >= 0.6 * 1200.
            (Aspect::Landscape, SafeArea::None, Placement::LowerThird, (152.0, 916.8, 1248.0, 1032.0)),
            // 9:16 reels: bottom band top 1500 - 16 = 1484; right rail grown to x >= 924 overlaps: trim right to 924;
            // width 876 >= 0.6 * 984.
            (Aspect::Portrait, SafeArea::Reels, Placement::LowerThird, (48.0, 1368.8, 924.0, 1484.0)),
            // 16:9 side: x from 1920 - 48 - 560 = 1312, centred in the free band [48, 1032]: top 540 - 57.6.
            (Aspect::Landscape, SafeArea::None, Placement::Side, (1312.0, 482.4, 1872.0, 597.6)),
            // 9:16 reels side: a bottom sheet, same band and trim as the lower-third.
            (Aspect::Portrait, SafeArea::Reels, Placement::Side, (48.0, 1368.8, 924.0, 1484.0)),
            // 4:5 reels (1080 x 1350): the 9:16 rects shift up 285 and clip; the top bar drops out, the bottom bar
            // is 1215..1350 (grown top 1199, a bottom band), the rail 940..1080 x 415..1215 (grown x >= 924). The mark
            // (grown 0..152 x 1278..1350) and plate (grown 648..1080 x 1298..1350) sit below the band [48, 1199].
            // Bottom 1199, top 1199 - 115.2 = 1083.8; ideal width min(984, 1200) = 984; the rail trims the right to
            // 924; width 876 >= 0.6 * 984 = 590.4.
            (Aspect::Feed, SafeArea::Reels, Placement::LowerThird, (48.0, 1083.8, 924.0, 1199.0)),
            // 4:5 side: 1080 < 1350, so a bottom sheet with ideal width 1080 - 96 = 984: same as the lower-third.
            (Aspect::Feed, SafeArea::Reels, Placement::Side, (48.0, 1083.8, 924.0, 1199.0)),
            // 1:1 (1080 x 1080), no safe area: band [48, 1032]; the card is 48..1032 x 916.8..1032. The mark (grown
            // 0..152 x 1008..1080, left of centre) trims the left to 152; the plate (grown 648..1080 x 1028..1080,
            // right of centre) trims the right to 648: width 496 < 0.6 * 984 = 590.4, so the trim is undone and the
            // bottom moves to the higher of their tops, 1008: 48..1032 x 892.8..1008.
            (Aspect::Square, SafeArea::None, Placement::LowerThird, (48.0, 892.8, 1032.0, 1008.0)),
            // 1:1 side: 1080 is not > 1080, so a bottom sheet with the same ideal width 984: the same rectangle.
            (Aspect::Square, SafeArea::None, Placement::Side, (48.0, 892.8, 1032.0, 1008.0)),
        ];
        for (aspect, area, placement, want) in rows {
            let l = layout(
                frame_size(aspect, Tier::Fullhd),
                1.0,
                &safe_area_rects(area, aspect),
                &[panel("p", placement, "Harbour")],
                &corners(aspect),
                &Mono,
            );
            assert_rect(l.placed[0].rect, want, &format!("{aspect:?} {area:?} {placement:?}"));
            assert!(l.warnings.is_empty(), "{aspect:?} {area:?} {placement:?}: {:?}", l.warnings);
        }
    }

    /// The corner chrome as `corners` places it, lifted by the safe area's
    /// bottom band (final review F3).
    fn corners_above_band(aspect: Aspect, rects: &[Rect]) -> Vec<Rect> {
        let (w, h) = frame_size(aspect, Tier::Fullhd);
        let lift = bottom_band_inset((w as f32, h as f32), rects);
        corners(aspect).into_iter().map(|r| (r.0, r.1 - lift, r.2, r.3 - lift)).collect()
    }

    /// The showcase's furniture (port-to-port): a source line, a metric
    /// scale bar and a north arrow, and a two-line Bottom caption.
    fn showcase_items(caption: &str) -> Vec<LayoutItem> {
        let mut frame = super::test_support::furniture_frame("Sources: OpenStreetMap; route hand-drawn");
        frame.byline = None;
        frame.legend = None;
        vec![
            LayoutItem::Furniture { id: "furniture".into(), frame },
            LayoutItem::Caption {
                id: "caption/0".into(),
                text: caption.into(),
                size: 40.0,
                anchor: Anchor::Bottom,
                offset: (0.0, 0.0),
                plate: true,
            },
        ]
    }

    const SHOWCASE_CAPTION: &str = "Every day, hundreds of container ships leave Singapore for Port Klang, up the strait";

    /// Final review V2: at 9:16 (and 4:5) a Bottom caption is as wide as
    /// the frame, so it always met the platform's side rail and climbed
    /// over all of it, to mid-frame. It now narrows beside the rail and
    /// sits just above the source line or the bottom band.
    #[test]
    fn a_bottom_caption_narrows_beside_a_rail_and_sits_just_above_what_is_below_it() {
        for (aspect, area) in [(Aspect::Portrait, SafeArea::Reels), (Aspect::Portrait, SafeArea::Tiktok), (Aspect::Feed, SafeArea::Reels)] {
            let (w, h) = frame_size(aspect, Tier::Fullhd);
            let rects = safe_area_rects(area, aspect);
            let chrome = corners_above_band(aspect, &rects);
            let l = layout((w, h), 1.0, &rects, &showcase_items(SHOWCASE_CAPTION), &chrome, &Mono);
            let c = l.placed.iter().find(|p| p.id == "caption/0").unwrap_or_else(|| panic!("{aspect:?} {area:?}: {l:?}"));
            for r in &rects {
                assert!(!intersects(c.rect, *r), "{aspect:?} {area:?}: the caption {:?} meets {r:?}", c.rect);
            }
            assert!(l.warnings.is_empty(), "{aspect:?} {area:?}: {:?}", l.warnings);
            // What lies below the caption in its columns: the bands, the
            // corner chrome, the furniture.
            let below = rects
                .iter()
                .chain(&chrome)
                .copied()
                .chain(l.placed.iter().filter(|p| p.id != c.id).map(|p| p.rect))
                .filter(|r| r.0 < c.rect.2 && c.rect.0 < r.2 && r.1 >= c.rect.3 - 0.01)
                .map(|r| r.1)
                .fold(f32::INFINITY, f32::min);
            let gap = below - c.rect.3;
            assert!(
                (0.0..=2.0 * GAP + CAPTION_MARGIN / 2.0 + 0.01).contains(&gap),
                "{aspect:?} {area:?}: the caption {:?} sits {gap} above the highest obstacle below it ({below})",
                c.rect
            );
            assert!(c.rect.1 > h as f32 * 0.5, "{aspect:?} {area:?}: not mid-frame, {:?}", c.rect);
            assert!(c.text_wrap.is_some(), "{aspect:?} {area:?}: narrowed beside the rail");
        }
    }

    /// Final review F3's consequence: corner chrome lifted onto a platform's
    /// bottom band is part of that band, so a lower third sits whole above
    /// it (not trimmed to a sliver between the mark and the credit, nor
    /// lifted over the rail into the top band).
    #[test]
    fn a_lower_third_sits_above_corner_chrome_lifted_onto_the_band() {
        for (aspect, area) in [(Aspect::Portrait, SafeArea::Reels), (Aspect::Portrait, SafeArea::Tiktok), (Aspect::Feed, SafeArea::Reels)] {
            let (w, h) = frame_size(aspect, Tier::Fullhd);
            let rects = safe_area_rects(area, aspect);
            let (right, bottom) = corner_insets((w as f32, h as f32), &rects);
            let (fw, fh) = (w as f32 - right, h as f32 - bottom);
            let chrome = vec![(16.0, fh - 56.0, 136.0, fh - 16.0), (fw - 416.0, fh - 36.0, fw - 16.0, fh - 16.0)];
            let item = LayoutItem::Panel {
                id: "/events/0".into(),
                placement: Placement::LowerThird,
                headline: Some("Harbour".into()),
                body: Some("The busiest port in the region by container volume".into()),
                source: Some("Port authority, 2025".into()),
                image_aspect: Some(16.0 / 9.0),
                theme: dark(),
            };
            let l = layout((w, h), 1.0, &rects, &[item], &chrome, &Mono);
            // A 4:5 band is short, so its capped card may shrink its text; nothing may overlap.
            assert!(l.warnings.iter().all(|w| w.code.starts_with("text_")), "{aspect:?} {area:?}: {:?}", l.warnings);
            let card = l.placed[0].rect;
            let chrome_top = chrome.iter().map(|c| c.1).fold(f32::INFINITY, f32::min);
            assert!(card.3 <= chrome_top - GAP + 0.01, "{aspect:?} {area:?}: the card {card:?} above the chrome (top {chrome_top})");
            assert!(width(card) >= MIN_TRIMMED_FRACTION * (w as f32 - 2.0 * MARGIN).min(LOWER_THIRD_MAX_W), "{aspect:?} {area:?}: {card:?}");
            for c in chrome.iter().chain(&rects) {
                assert!(!intersects(card, *c), "{aspect:?} {area:?}: {card:?} meets {c:?}");
            }
        }
    }

    /// With no safe area, nothing narrows a caption: it is centred on the
    /// full frame at its full wrap and steps over the corner chrome, as it
    /// always did.
    #[test]
    fn a_caption_with_no_safe_area_is_placed_as_before() {
        for aspect in ALL_ASPECTS {
            let (w, h) = frame_size(aspect, Tier::Fullhd);
            let (fw, fh) = (w as f32, h as f32);
            let chrome = corners(aspect);
            let item = LayoutItem::Caption {
                id: "caption/0".into(),
                text: SHOWCASE_CAPTION.into(),
                size: 40.0,
                anchor: Anchor::Bottom,
                offset: (0.0, 0.0),
                plate: true,
            };
            let l = layout((w, h), 1.0, &[], &[item], &chrome, &Mono);
            let c = &l.placed[0];
            assert_eq!(c.text_wrap, None, "{aspect:?}");
            let wrap = caption_wrap(fw, true);
            let (tw, th) = mono(SHOWCASE_CAPTION, 40.0, Some(wrap));
            let (cw, ch) = (tw + 2.0 * CAPTION_PAD_X, th + 2.0 * CAPTION_PAD_Y);
            let x = (fw - cw) / 2.0;
            // The legacy walk: up from the rule row over every grown corner it meets.
            let mut y = fh - CAPTION_MARGIN - ch;
            for _ in 0..chrome.len() {
                for o in chrome.iter().map(|r| grow(*r, GAP)) {
                    if intersects((x, y, x + cw, y + ch), o) {
                        y = o.1 - CAPTION_MARGIN / 2.0 - ch;
                    }
                }
            }
            assert_rect(c.rect, (x, y, x + cw, y + ch), &format!("{aspect:?}"));
        }
    }

    #[test]
    fn the_bottom_band_inset_is_the_highest_bottom_band() {
        let reels = safe_area_rects(SafeArea::Reels, Aspect::Portrait);
        assert_eq!(bottom_band_inset((1080.0, 1920.0), &reels), 420.0);
        let tiktok = safe_area_rects(SafeArea::Tiktok, Aspect::Portrait);
        assert_eq!(bottom_band_inset((1080.0, 1920.0), &tiktok), 480.0);
        let feed = safe_area_rects(SafeArea::Reels, Aspect::Feed);
        assert_eq!(bottom_band_inset((1080.0, 1350.0), &feed), 1350.0 - (1500.0 - 285.0));
        assert_eq!(bottom_band_inset((1920.0, 1080.0), &[]), 0.0);
        // A rail is not a band.
        assert_eq!(bottom_band_inset((1080.0, 1920.0), &[(940.0, 700.0, 1080.0, 1500.0)]), 0.0);
        // The rail that comes down to the band is the right inset.
        assert_eq!(corner_insets((1080.0, 1920.0), &reels), (140.0, 420.0));
        assert_eq!(corner_insets((1080.0, 1920.0), &tiktok), (150.0, 480.0));
        assert_eq!(corner_insets((1080.0, 1350.0), &feed), (140.0, 1350.0 - (1500.0 - 285.0)));
        assert_eq!(corner_insets((1920.0, 1080.0), &safe_area_rects(SafeArea::Youtube, Aspect::Landscape)), (0.0, 120.0));
        // A rail that stops short of the band leaves the corner alone.
        assert_eq!(corner_insets((1080.0, 1920.0), &[(0.0, 1500.0, 1080.0, 1920.0), (940.0, 700.0, 1080.0, 1200.0)]), (0.0, 420.0));
    }

    #[test]
    fn no_element_lands_in_a_safe_zone_on_any_aspect() {
        for aspect in ALL_ASPECTS {
            for area in [SafeArea::Reels, SafeArea::Tiktok, SafeArea::Shorts] {
                let rects = safe_area_rects(area, aspect);
                let l = layout(frame_size(aspect, Tier::Fullhd), 1.0, &rects, &all_kinds(), &corners(aspect), &Mono);
                assert!(l.placed.len() >= 6, "{aspect:?} {area:?}: {l:?}");
                for p in &l.placed {
                    for r in &rects {
                        assert!(!intersects(p.rect, *r), "{aspect:?} {area:?} {} in {r:?}", p.id);
                    }
                }
                assert!(l.warnings.iter().all(|w| w.code != "in_safe_zone"), "{aspect:?} {area:?}: {:?}", l.warnings);
            }
        }
    }

    #[test]
    fn placed_elements_keep_clear_of_each_other_and_of_the_chrome() {
        for aspect in ALL_ASPECTS {
            for area in [SafeArea::None, SafeArea::Reels, SafeArea::Youtube] {
                let chrome = corners(aspect);
                let l = layout(frame_size(aspect, Tier::Fullhd), 1.0, &safe_area_rects(area, aspect), &all_kinds(), &chrome, &Mono);
                // No element meets a safe area, chrome or another element (checked below); a card capped under
                // CARD_HEIGHT_CAP may still need to shrink its text to stay inside it (Task 10), and that is fine.
                assert!(
                    l.warnings.iter().all(|w| matches!(w.code, "text_shrunk" | "text_truncated")),
                    "{aspect:?} {area:?}: {:?}",
                    l.warnings
                );
                for (i, a) in l.placed.iter().enumerate() {
                    for c in &chrome {
                        assert!(!intersects(a.rect, *c), "{aspect:?} {area:?} {} on the chrome {c:?}", a.id);
                    }
                    for b in l.placed.iter().skip(i + 1) {
                        assert!(!intersects(a.rect, b.rect), "{aspect:?} {area:?} {} meets {}", a.id, b.id);
                    }
                }
            }
        }
    }

    #[test]
    fn every_tier_lays_out_identically_in_1080_units() {
        for aspect in ALL_ASPECTS {
            for area in [SafeArea::None, SafeArea::Reels, SafeArea::Tiktok, SafeArea::Shorts, SafeArea::Youtube] {
                let rects = safe_area_rects(area, aspect);
                let fullhd = layout(frame_size(aspect, Tier::Fullhd), 1.0, &rects, &all_kinds(), &corners(aspect), &Mono);
                assert!(!fullhd.placed.is_empty());
                for tier in [Tier::Hd, Tier::Qhd, Tier::Uhd] {
                    let other = layout(frame_size(aspect, tier), pixel_ratio(tier), &rects, &all_kinds(), &corners(aspect), &Mono);
                    assert_eq!(fullhd, other, "{aspect:?} {area:?} {tier:?}");
                }
            }
        }
    }

    #[test]
    fn priority_decides_who_moves() {
        // A title-free 16:9 frame with a caption at bottom-left and a lower-third panel. The caption comes first in
        // the input, but panels outrank captions: the panel keeps its rule rect (the table's first row) and the
        // caption moves above it.
        let caption = LayoutItem::Caption {
            id: "caption:0".into(),
            // 35 chars at 0.5 * 32 = 560 wide, plus 2 * 12 padding: x 24..608 crosses the panel's 136..1264 (grown).
            text: "Container traffic doubled since 19".into(),
            size: 32.0,
            anchor: Anchor::BottomLeft,
            offset: (0.0, 0.0),
            plate: true,
        };
        let items = [caption, panel("/events/0", Placement::LowerThird, "Harbour")];
        let frame = frame_size(Aspect::Landscape, Tier::Fullhd);
        let l = layout(frame, 1.0, &[], &items, &corners(Aspect::Landscape), &Mono);
        assert_eq!(l.placed.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["/events/0", "caption:0"]);
        let alone = layout(frame, 1.0, &[], &items[1..], &corners(Aspect::Landscape), &Mono);
        assert_eq!(l.placed[0], alone.placed[0]);
        assert_rect(l.placed[0].rect, (152.0, 916.8, 1248.0, 1032.0), "panel");
        let cap = l.placed[1].rect;
        assert!(cap.3 <= l.placed[0].rect.1 - GAP + 0.01, "caption {cap:?} is not above the panel");
        assert!((cap.0 - 24.0).abs() < 0.01, "caption kept its left margin: {cap:?}");
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        // Swapping the input order changes nothing.
        let swapped = [items[1].clone(), items[0].clone()];
        assert_eq!(layout(frame, 1.0, &[], &swapped, &corners(Aspect::Landscape), &Mono), l);
    }

    #[test]
    fn a_title_fills_the_free_band_dims_the_map_and_avoids_the_safe_area() {
        let title = |headline: &str| LayoutItem::Panel {
            id: "/events/0".into(),
            placement: Placement::Title,
            headline: Some(headline.into()),
            body: Some("A season at sea".into()),
            source: Some("Port authority".into()),
            image_aspect: None,
            theme: crate::story::panel::theme(Some("dark")).unwrap(),
        };
        // 16:9, no safe area: the band is [48, 1032]; the mark and plate reach 24 units into it, so the cheaper fix
        // is to lift the bottom to the mark's grown top, 1008 (trimming both sides would cost far more area).
        let l = layout(frame_size(Aspect::Landscape, Tier::Fullhd), 1.0, &[], &[title("Port to port")], &corners(Aspect::Landscape), &Mono);
        assert_eq!(l.dim, Some(TITLE_DIM));
        assert_rect(l.placed[0].rect, (48.0, 48.0, 1872.0, 1008.0), "16:9 title");
        let roles: Vec<TextRole> = l.placed[0].text.iter().map(|t| t.0).collect();
        assert_eq!(roles, [TextRole::Headline, TextRole::Body, TextRole::Source]);
        assert_eq!(l.placed[0].text[0].2, 88.0);
        // The headline is centred horizontally in the card.
        let h = l.placed[0].text[0].1;
        assert!(((h.0 + h.2) / 2.0 - 960.0).abs() < 0.01, "{h:?}");
        for aspect in ALL_ASPECTS {
            for area in [SafeArea::Reels, SafeArea::Tiktok, SafeArea::Shorts, SafeArea::Youtube] {
                let rects = safe_area_rects(area, aspect);
                let l = layout(frame_size(aspect, Tier::Fullhd), 1.0, &rects, &[title("Port to port")], &corners(aspect), &Mono);
                assert!(rects.iter().all(|r| !intersects(l.placed[0].rect, *r)), "{aspect:?} {area:?}: {:?}", l.placed[0].rect);
                assert!(l.warnings.is_empty(), "{aspect:?} {area:?}: {:?}", l.warnings);
            }
        }
        // 9:16 reels: the band is [236, 1484]; the rail (grown x >= 924) is cheaper to trim than to lift above.
        let rects = safe_area_rects(SafeArea::Reels, Aspect::Portrait);
        let l = layout(frame_size(Aspect::Portrait, Tier::Fullhd), 1.0, &rects, &[title("Port")], &corners(Aspect::Portrait), &Mono);
        assert_rect(l.placed[0].rect, (48.0, 236.0, 924.0, 1484.0), "9:16 title");
    }

    #[test]
    fn a_corner_card_takes_the_top_left_and_the_next_one_the_top_right() {
        let items = [
            panel("/events/0", Placement::Corner, "Day 3"),
            LayoutItem::Image { id: "/events/1".into(), placement: Placement::Corner, aspect: 1.0 },
        ];
        let l = layout(frame_size(Aspect::Landscape, Tier::Fullhd), 1.0, &[], &items, &corners(Aspect::Landscape), &Mono);
        assert_rect(l.placed[0].rect, (48.0, 48.0, 528.0, 163.2), "corner panel");
        // A square image is 480 x 480, capped at 360 tall; the top-left meets the panel, so it goes top-right.
        assert_rect(l.placed[1].rect, (1392.0, 48.0, 1872.0, 408.0), "corner image");
        assert_eq!(l.placed[1].image, Some(l.placed[1].rect));
    }

    #[test]
    fn a_callout_sits_beside_its_anchor_with_a_leader_to_the_nearest_edge() {
        let callout = |side| LayoutItem::Callout { id: "c".into(), anchor: (900.0, 500.0), text: "Terminal 4".into(), side, image_aspect: None, theme: dark() };
        let frame = frame_size(Aspect::Landscape, Tier::Fullhd);
        // "Terminal 4" is 10 chars at 16 = 160 wide: card 160 + CALLOUT_TEXT_SLACK (1) + 32 = 193 wide,
        // 1.2 * 32 + 32 = 70.4 tall.
        let right = layout(frame, 1.0, &[], &[callout(Side::Right)], &[], &Mono);
        assert_rect(right.placed[0].rect, (924.0, 464.8, 1117.0, 535.2), "right");
        assert_eq!(right.placed[0].leader, Some(((900.0, 500.0), (924.0, 500.0))));
        let left = layout(frame, 1.0, &[], &[callout(Side::Left)], &[], &Mono);
        assert_rect(left.placed[0].rect, (683.0, 464.8, 876.0, 535.2), "left");
        assert_eq!(left.placed[0].leader, Some(((900.0, 500.0), (876.0, 500.0))));
        assert_eq!(left.placed[0].text[0].2, 32.0);
        // An anchor near the top: the card is clamped into the free band, the leader still starts at the anchor.
        let high = LayoutItem::Callout { id: "c".into(), anchor: (900.0, 10.0), text: "Terminal 4".into(), side: Side::Right, image_aspect: None, theme: dark() };
        let l = layout(frame, 1.0, &[], &[high], &[], &Mono);
        assert!((l.placed[0].rect.1 - MARGIN).abs() < 0.01, "{:?}", l.placed[0].rect);
        assert_eq!(l.placed[0].leader.map(|(a, _)| a), Some((900.0, 10.0)));
    }

    #[test]
    fn an_element_that_cannot_be_cleared_warns_once_per_code_naming_every_obstacle() {
        // A 1920 x 300 frame with platform bars above 120 and below 180: the free band (grown) is 136..164, and a
        // corner card capped at 0.3 * 300 = 90 cannot fit between the bars. It stays at the band's top (136..226),
        // in the lower bar (grown from 164) and on two chrome rects: one warning per code, naming every obstacle.
        // The card's 42-unit inner height (90 - 2 * padding) also forces its headline to shrink (Task 10).
        let bars = [(0.0, 0.0, 1920.0, 120.0), (0.0, 180.0, 1920.0, 300.0)];
        let chrome = [(0.0, 150.0, 300.0, 300.0), (500.0, 150.0, 700.0, 300.0)];
        let l = layout((1920, 300), 1.0, &bars, &[panel("/events/0", Placement::Corner, "Day 3")], &chrome, &Mono);
        assert_eq!(l.placed.len(), 1);
        let got: Vec<(&str, &str)> = l.warnings.iter().map(|w| (w.id.as_str(), w.code)).collect();
        assert_eq!(got, [("/events/0", "text_shrunk"), ("/events/0", "in_safe_zone"), ("/events/0", "overlap")], "{:?}", l.warnings);
        assert!(l.warnings[1].message.contains("`safe_area[1]`"), "{}", l.warnings[1].message);
        assert!(l.warnings[2].message.contains("`chrome[0]`") && l.warnings[2].message.contains("`chrome[1]`"), "{}", l.warnings[2].message);
        // Three corner cards in a frame 140 tall: each card is capped at 0.3 * 140 = 42; the third one's top-right
        // slot meets the second card (grown 32..106), and neither above (32 - 42 < 0) nor below (106 + 42 > 140)
        // fits inside the frame: `overlap`.
        let l = layout(
            (1920, 140),
            1.0,
            &[],
            &[panel("a", Placement::Corner, "One"), panel("b", Placement::Corner, "Two"), panel("c", Placement::Corner, "Three")],
            &[],
            &Mono,
        );
        // Neither corner clears, so `c` stays top-left on `a` and says so, once (each card's headline also
        // truncates: a 42-unit cap leaves no room for the 2 * 24 padding alone -- Task 10's `fit_text` still
        // returns something finite, just an ellipsis).
        assert!(l.warnings.iter().any(|w| w.id == "c" && w.code == "overlap" && w.message.contains("`a`")), "{:?}", l.warnings);
        assert_eq!(l.warnings.iter().filter(|w| w.id == "c" && w.code == "overlap").count(), 1, "{:?}", l.warnings);
    }

    fn caption(anchor: Anchor, offset: (f32, f32)) -> LayoutItem {
        LayoutItem::Caption { id: "caption:0".into(), text: "Container traffic doubled".into(), size: 32.0, anchor, offset, plate: true }
    }

    fn title() -> LayoutItem {
        LayoutItem::Panel {
            id: "/events/0".into(),
            placement: Placement::Title,
            headline: Some("Port to port".into()),
            body: Some("A season at sea".into()),
            source: None,
            image_aspect: None,
            theme: crate::story::panel::theme(Some("dark")).unwrap(),
        }
    }

    #[test]
    fn a_caption_draws_over_a_title_card_at_its_own_position() {
        for aspect in ALL_ASPECTS {
            let rects = safe_area_rects(crate::story::tiers::default_safe_area(aspect), aspect);
            let frame = frame_size(aspect, Tier::Fullhd);
            for anchor in [Anchor::Bottom, Anchor::BottomLeft, Anchor::Top] {
                let alone = layout(frame, 1.0, &rects, &[caption(anchor, (0.0, 0.0))], &corners(aspect), &Mono);
                let with_title = layout(frame, 1.0, &rects, &[caption(anchor, (0.0, 0.0)), title()], &corners(aspect), &Mono);
                assert_eq!(with_title.placed.len(), 2);
                assert_eq!(with_title.placed[1], alone.placed[0], "{aspect:?} {anchor:?}");
                assert!(with_title.warnings.is_empty(), "{aspect:?} {anchor:?}: {:?}", with_title.warnings);
                assert!(rects.iter().all(|r| !intersects(with_title.placed[1].rect, *r)), "{aspect:?} {anchor:?}");
            }
        }
        // A caption is still pushed off any other card.
        let l = layout((1920, 1080), 1.0, &[], &[caption(Anchor::Bottom, (0.0, 0.0)), panel("p", Placement::LowerThird, "Harbour")], &[], &Mono);
        assert!(!intersects(l.placed[0].rect, l.placed[1].rect));
    }

    #[test]
    fn a_caption_offset_moves_it_as_caption_origin_does() {
        // "Container traffic doubled" is 25 chars at 16 = 400 + 24 = 424 wide, 1.2 * 32 + 12 = 50.4 tall. Centred on
        // 1920 x 1080: (748, 514.8); offset (10, -5) moves it to (758, 509.8).
        let l = layout((1920, 1080), 1.0, &[], &[caption(Anchor::Center, (10.0, -5.0))], &[], &Mono);
        assert_rect(l.placed[0].rect, (758.0, 509.8, 1182.0, 560.2), "offset caption");
        // The push-away runs after the offset: an offset into the mark still clears it.
        let l = layout((1920, 1080), 1.0, &[], &[caption(Anchor::BottomLeft, (0.0, 0.0))], &corners(Aspect::Landscape), &Mono);
        assert!(l.placed[0].rect.3 <= 1008.0 + 0.01, "{:?}", l.placed[0].rect);
        let l = layout((1920, 1080), 1.0, &[], &[caption(Anchor::Center, (f32::NAN, 5.0))], &[], &Mono);
        assert_rect(l.placed[0].rect, (748.0, 519.8, 1172.0, 570.2), "a NaN offset part is 0");
        assert!(l.warnings.iter().any(|w| w.code == "bad_input"), "{:?}", l.warnings);
    }

    struct Broken(f32);
    impl TextMeasure for Broken {
        fn measure(&self, _: &str, _: f32, _: Option<f32>) -> (f32, f32) {
            (self.0, self.0)
        }
    }

    fn finite(r: Rect) -> bool {
        [r.0, r.1, r.2, r.3].iter().all(|v| v.is_finite()) && r.2 >= r.0 && r.3 >= r.1
    }

    #[test]
    fn bad_measures_and_anchors_are_sanitised_and_warned() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -5.0] {
            let l = layout((1920, 1080), 1.0, &[], &all_kinds(), &corners(Aspect::Landscape), &Broken(bad));
            assert!(!l.placed.is_empty());
            for p in &l.placed {
                assert!(finite(p.rect), "{bad}: {} {:?}", p.id, p.rect);
                assert!(p.text.iter().all(|t| finite(t.1)), "{bad}: {} {:?}", p.id, p.text);
                assert!(l.warnings.iter().any(|w| w.id == p.id && w.code == "bad_measure") || p.text.is_empty(), "{bad}: {}", p.id);
            }
        }
        for anchor in [(f32::NAN, 500.0), (f32::INFINITY, 500.0), (400.0, f32::NEG_INFINITY)] {
            let item = LayoutItem::Callout { id: "c".into(), anchor, text: "x".into(), side: Side::Right, image_aspect: None, theme: dark() };
            let l = layout((1920, 1080), 1.0, &[], &[item], &[], &Mono);
            assert!(l.placed.is_empty(), "{anchor:?}");
            assert_eq!(l.warnings.iter().map(|w| (w.id.as_str(), w.code)).collect::<Vec<_>>(), [("c", "callout_offscreen")]);
        }
    }

    #[test]
    fn an_obstacle_of_infinite_width_is_not_a_band() {
        let f = Frame { w: 1920.0, h: 1080.0, obstacles: Vec::new() };
        assert_eq!(f.band_kind((f32::NEG_INFINITY, 900.0, f32::INFINITY, 1080.0)), None);
        assert_eq!(f.band_kind((f32::NAN, 900.0, 1920.0, 1080.0)), None);
        assert_eq!(f.band_kind((0.0, 900.0, 1920.0, 1080.0)), Some(BandKind::Bottom));
        // Through `layout`: such a safe area does not lift the lower-third off the band's bottom.
        let l = layout((1920, 1080), 1.0, &[(f32::NEG_INFINITY, 1060.0, f32::INFINITY, 1080.0)], &[panel("p", Placement::LowerThird, "Harbour")], &[], &Mono);
        assert!((l.placed[0].rect.3 - 1032.0).abs() < 0.01, "{:?}", l.placed[0].rect);
    }

    #[test]
    fn a_frame_with_no_room_places_nothing_and_says_so() {
        let full = [(0.0, 0.0, 1080.0, 1920.0)];
        for (frame, safe) in [((0, 0), &[][..]), ((1, 1), &[][..]), ((100, 50), &[][..]), ((50, 4000), &[][..]), ((1080, 1920), &full[..])] {
            let l = layout(frame, 1.0, safe, &all_kinds(), &[], &Mono);
            assert!(l.placed.is_empty(), "{frame:?}");
            assert_eq!(l.warnings.iter().map(|w| (w.id.as_str(), w.code)).collect::<Vec<_>>(), [("frame", "frame_too_small")], "{frame:?}");
        }
    }

    #[test]
    fn no_rect_is_ever_inverted() {
        // Small but legal frames, crowded with every kind and a title.
        for frame in [(97, 97), (200, 120), (120, 900), (1920, 140), (300, 3000)] {
            let mut items = all_kinds();
            items.push(title());
            let l = layout(frame, 1.0, &[], &items, &[], &Mono);
            for p in &l.placed {
                assert!(finite(p.rect), "{frame:?} {} {:?}", p.id, p.rect);
            }
        }
    }

    #[test]
    fn an_overfull_frame_overlaps_its_cards_before_it_enters_a_safe_zone() {
        // Every kind, the side sheet carrying a 3:2 image, on 9:16 reels: not everything fits, and the caption's push-away walks
        // it up past the top bar before clamping. Step 9 then takes the smallest shift clear of the safe areas.
        let mut items = all_kinds();
        if let Some(LayoutItem::Panel { image_aspect, .. }) = items.get_mut(1) {
            *image_aspect = Some(1.5);
        }
        let rects = safe_area_rects(SafeArea::Reels, Aspect::Portrait);
        let l = layout(frame_size(Aspect::Portrait, Tier::Fullhd), 1.0, &rects, &items, &corners(Aspect::Portrait), &Mono);
        assert!(l.warnings.iter().any(|w| w.code == "overlap"), "the fixture is meant to overfill: {:?}", l.warnings);
        assert!(l.warnings.iter().all(|w| w.code != "in_safe_zone"), "{:?}", l.warnings);
        for p in &l.placed {
            assert!(rects.iter().all(|r| !intersects(p.rect, *r)), "{} at {:?}", p.id, p.rect);
        }
    }

    #[test]
    fn a_panel_image_sits_above_its_text_at_the_card_width() {
        let item = LayoutItem::Panel {
            id: "p".into(),
            placement: Placement::Side,
            headline: Some("Strait".into()),
            body: None,
            source: None,
            image_aspect: Some(2.0),
            theme: crate::story::panel::theme(Some("dark")).unwrap(),
        };
        // 16:9 side card, 560 wide: the image is 512 x 256 inside the padding, then 12, then the headline (67.2).
        let l = layout((1920, 1080), 1.0, &[], &[item], &[], &Mono);
        let p = &l.placed[0];
        let h = 24.0 + 256.0 + 12.0 + 67.2 + 24.0;
        assert_rect(p.rect, (1312.0, 540.0 - h / 2.0, 1872.0, 540.0 + h / 2.0), "card");
        assert_rect(p.image.unwrap(), (1336.0, p.rect.1 + 24.0, 1848.0, p.rect.1 + 280.0), "image");
        assert!((p.text[0].1 .1 - (p.rect.1 + 292.0)).abs() < 0.01, "{:?}", p.text);
    }

    /// Task 12: a lower third embedding an image keeps the image and every
    /// text block inside its capped card, the text below the image.
    #[test]
    fn a_lower_third_image_is_capped_inside_its_card() {
        // (frame, image beside the text?) -- 16:9 is wide enough for a column (review F3).
        for (size, expect) in [((1920, 1080), Some(true)), ((1080, 1920), Some(false)), ((1080, 1080), None), ((1080, 1350), None)] {
            let item = LayoutItem::Panel {
                id: "p".into(),
                placement: Placement::LowerThird,
                headline: Some("Harbour".into()),
                body: Some("Container berths moved west to Pasir Panjang in the 2000s.".into()),
                source: Some("Source: MPA".into()),
                image_aspect: Some(1.5),
                theme: crate::story::panel::theme(Some("dark")).unwrap(),
            };
            let l = layout(size, 1.0, &[], &[item], &[], &Mono);
            let p = &l.placed[0];
            let img = p.image.unwrap();
            let inside = |r: Rect| r.0 >= p.rect.0 - 0.01 && r.1 >= p.rect.1 - 0.01 && r.2 <= p.rect.2 + 0.01 && r.3 <= p.rect.3 + 0.01;
            assert!(inside(img), "{size:?}: image {img:?} leaves card {:?}", p.rect);
            assert!(img.3 - img.1 > 0.0 && img.2 - img.0 > 0.0);
            let pad = 24.0;
            let (iw, ih) = (p.rect.2 - p.rect.0 - 2.0 * pad, p.rect.3 - p.rect.1 - 2.0 * pad);
            // The rule: beside when the inner box is wider than 2.5 : 1.
            let beside = iw / ih > SIDE_IMAGE_ASPECT;
            if let Some(e) = expect {
                assert_eq!(beside, e, "{size:?}: inner {iw} x {ih}");
            }
            for (_, r, ..) in &p.text {
                assert!(inside(*r), "{size:?}: text {r:?} leaves card {:?}", p.rect);
                if beside {
                    assert!(r.0 >= img.2 + SIDE_IMAGE_GAP - 0.01, "{size:?}: text {r:?} right of the image {img:?}");
                } else {
                    assert!(r.1 >= img.3, "{size:?}: text {r:?} under the image {img:?}");
                }
            }
            if beside {
                assert!((img.2 - img.0 - iw * SIDE_IMAGE_SHARE).abs() < 0.01, "{size:?}: a 40% column {img:?}");
                assert!((img.3 - img.1 - ih).abs() < 0.01, "{size:?}: the column is the inner height");
                assert_eq!(p.text_wrap, Some(iw - iw * SIDE_IMAGE_SHARE - SIDE_IMAGE_GAP));
            } else {
                assert!((img.2 - img.0 - iw).abs() < 0.01, "{size:?}: a full-width image");
                assert_eq!(p.text_wrap, None);
            }
        }
    }

    #[test]
    fn furniture_places_every_piece_in_its_corner_after_images_and_before_callouts() {
        assert!(Priority::Image < Priority::Furniture && Priority::Furniture < Priority::Callout);
        let item = LayoutItem::Furniture { id: "furniture".into(), frame: super::test_support::furniture_frame("Source: Port authority") };
        let chrome = corners(Aspect::Landscape);
        let l = layout((1920, 1080), 1.0, &[], &[item], &chrome, &Mono);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        let ids: Vec<&str> = l.placed.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["furniture/source", "furniture/scale", "furniture/legend", "furniture/north"]);
        let rect = |id: &str| l.placed.iter().find(|p| p.id == id).unwrap().rect;
        let (credit, mark) = (chrome[1], chrome[0]);
        // The source block: right-aligned to the credit plate, GAP above it.
        let source = rect("furniture/source");
        assert_eq!((source.2, source.3), (credit.2, credit.1 - GAP));
        // Its two lines, stacked and right-aligned inside it.
        let p = l.placed.iter().find(|p| p.id == "furniture/source").unwrap();
        assert_eq!(p.text.len(), 2);
        assert!(p.text.iter().all(|t| (t.1 .2 - (source.2 - FURNITURE_PAD)).abs() < 1e-3 && t.2 == FURNITURE_TEXT));
        assert!(p.text[0].1 .3 <= p.text[1].1 .1);
        // The scale bar above the mark, at the left margin.
        let scale = rect("furniture/scale");
        assert_eq!((scale.0, scale.3), (MARGIN, mark.1 - GAP));
        assert!(width(scale) >= 130.8, "as long as the longest bar");
        // The legend top-right, the arrow top-left, both at the top margin.
        let legend = rect("furniture/legend");
        assert_eq!((legend.2, legend.1), (1920.0 - MARGIN, MARGIN));
        assert_eq!(l.placed[2].text.len(), 4, "title and three entries");
        assert_eq!(rect("furniture/north"), (MARGIN, MARGIN, MARGIN + NORTH_BOX, MARGIN + NORTH_BOX));
        // A piece with nothing to show places nothing.
        let empty = LayoutItem::Furniture { id: "furniture".into(), frame: FurnitureFrame::default() };
        assert!(layout((1920, 1080), 1.0, &[], &[empty], &chrome, &Mono).placed.is_empty());
    }

    #[test]
    fn a_scale_bar_hidden_by_pitch_is_a_layout_note_and_places_nothing() {
        let frame = FurnitureFrame { scale_bar_hidden_pitch: Some(60.0), ..FurnitureFrame::default() };
        let l = layout((1920, 1080), 1.0, &[], &[LayoutItem::Furniture { id: "furniture".into(), frame }], &[], &Mono);
        assert!(l.placed.is_empty());
        assert_eq!(l.warnings.iter().map(|w| (w.id.as_str(), w.code)).collect::<Vec<_>>(), [("furniture/scale", "scale_bar_hidden_pitch")]);
        // Without the note, none.
        let shown = LayoutItem::Furniture { id: "furniture".into(), frame: super::test_support::furniture_frame("Source") };
        assert!(layout((1920, 1080), 1.0, &[], &[shown], &[], &Mono).warnings.iter().all(|w| w.code != "scale_bar_hidden_pitch"));
    }

    #[test]
    fn the_north_arrow_sits_under_a_corner_card() {
        let frame = FurnitureFrame { north_arrow: Some(-12.0), ..FurnitureFrame::default() };
        let items = [LayoutItem::Furniture { id: "f".into(), frame }, panel("/events/0", Placement::Corner, "Day 3")];
        let l = layout((1920, 1080), 1.0, &[], &items, &corners(Aspect::Landscape), &Mono);
        let card = l.placed.iter().find(|p| p.id == "/events/0").unwrap().rect;
        let arrow = l.placed.iter().find(|p| p.id == "f/north").unwrap().rect;
        assert_eq!(card.0, MARGIN, "the card took the top-left slot");
        assert_eq!((arrow.0, arrow.1), (MARGIN, card.3 + GAP), "the arrow is right under it");
    }

    #[test]
    fn the_source_line_never_overlaps_the_credit_plate() {
        // layout with a credit obstacle at the bottom-right and a long source line at 16:9 and 9:16: no intersection.
        let long = "Source: Maritime and Port Authority vessel arrival records, 2019 to 2025, compiled by the newsroom data desk \
                    from monthly bulletins and cross-checked against the port operator's own published annual reports";
        for aspect in [Aspect::Landscape, Aspect::Portrait] {
            for area in [SafeArea::None, SafeArea::Reels, SafeArea::Youtube] {
                let (w, h) = frame_size(aspect, Tier::Fullhd);
                let chrome = corners(aspect);
                let credit = chrome[1];
                let item = LayoutItem::Furniture { id: "furniture".into(), frame: super::test_support::furniture_frame(long) };
                let l = layout((w, h), 1.0, &safe_area_rects(area, aspect), &[item], &chrome, &Mono);
                let p = l.placed.iter().find(|p| p.id == "furniture/source").unwrap();
                assert!(!intersects(p.rect, credit), "{aspect:?} {area:?}: {:?} meets the credit {credit:?}", p.rect);
                for t in &p.text {
                    assert!(!intersects(t.1, credit), "{aspect:?} {area:?}: text {:?}", t.1);
                    assert!(t.1 .0 >= p.rect.0 - 1e-3 && t.1 .2 <= p.rect.2 + 1e-3, "{aspect:?} {area:?}: text inside its box");
                }
                // Truncated gracefully: one line at the floor, with an ellipsis, inside its cap.
                let line = &p.text[0];
                assert!(line.3.ends_with('…'), "{aspect:?} {area:?}: {:?}", line.3);
                assert!((line.1 .3 - line.1 .1 - 1.2 * FURNITURE_TEXT).abs() < 1e-3, "one line");
                let cap = SOURCE_MAX_W.min(w as f32 - 2.0 * MARGIN - SCALE_RESERVE).max(w as f32 / 2.0 - MARGIN);
                assert!(width(p.rect) <= cap + 1e-3);
                assert!(l.warnings.iter().any(|x| x.id == "furniture/source" && x.code == "text_truncated"));
                // Every piece clear of the safe areas, the chrome and each other.
                for (i, a) in l.placed.iter().enumerate() {
                    assert!(a.rect.0 >= 0.0 && a.rect.1 >= 0.0 && a.rect.2 <= w as f32 && a.rect.3 <= h as f32, "{aspect:?} {area:?} {}", a.id);
                    for r in safe_area_rects(area, aspect).iter().chain(&chrome) {
                        assert!(!intersects(a.rect, *r), "{aspect:?} {area:?} {} meets {r:?}", a.id);
                    }
                    for b in l.placed.iter().skip(i + 1) {
                        assert!(!intersects(a.rect, b.rect), "{aspect:?} {area:?} {} meets {}", a.id, b.id);
                    }
                }
            }
        }
    }

    #[test]
    fn a_very_long_headline_at_9x16_hd_shrinks_to_the_floor_then_ellipsizes_and_warns() {
        // The brief's sentence, repeated enough times that even the 28-unit floor cannot wrap it whole into the
        // lower-third's capped height: at `repeat(3)` the floor still fits (a shrink alone is enough, no
        // ellipsis); `repeat(10)` needs more height than 30% of the 9:16 frame leaves, even at the floor, so it
        // genuinely needs the ellipsis this test is pinning (confirmed against `Mono`, not hand-counted, since
        // the wrap is the measure's to decide).
        let headline = "The longest container route ever sailed between two ports in a single season, and why it matters for the city "
            .repeat(10);
        let (w, h) = frame_size(Aspect::Portrait, Tier::Hd);
        let rects = safe_area_rects(SafeArea::Reels, Aspect::Portrait);
        let l = layout((w, h), pixel_ratio(Tier::Hd), &rects, &[panel("/events/0", Placement::LowerThird, &headline)], &corners(Aspect::Portrait), &Mono);
        let p = &l.placed[0];
        let (_, _, size, text) = p.text.iter().find(|t| t.0 == TextRole::Headline).unwrap();
        assert_eq!(*size, TEXT_FLOOR);
        assert!(text.ends_with('…') && text.len() < headline.len());
        assert!(rects.iter().all(|r| !intersects(p.rect, *r)));
        assert!(l.warnings.iter().any(|w| w.id == "/events/0" && w.code == "text_truncated"));
    }

    #[test]
    fn text_is_never_below_the_floor() {
        // Every row of `the_table_of_placements_matches_hand_computed_rectangles` x every aspect (through its own
        // row) x every tier: all placed text sizes are >= TEXT_FLOOR, whatever `fit_text` had to do to get there.
        let rows: [(Aspect, SafeArea, Placement); 8] = [
            (Aspect::Landscape, SafeArea::None, Placement::LowerThird),
            (Aspect::Portrait, SafeArea::Reels, Placement::LowerThird),
            (Aspect::Landscape, SafeArea::None, Placement::Side),
            (Aspect::Portrait, SafeArea::Reels, Placement::Side),
            (Aspect::Feed, SafeArea::Reels, Placement::LowerThird),
            (Aspect::Feed, SafeArea::Reels, Placement::Side),
            (Aspect::Square, SafeArea::None, Placement::LowerThird),
            (Aspect::Square, SafeArea::None, Placement::Side),
        ];
        for (aspect, area, placement) in rows {
            for tier in [Tier::Hd, Tier::Fullhd, Tier::Qhd, Tier::Uhd] {
                let l = layout(
                    frame_size(aspect, tier),
                    pixel_ratio(tier),
                    &safe_area_rects(area, aspect),
                    &[panel("p", placement, "Harbour")],
                    &corners(aspect),
                    &Mono,
                );
                for p in &l.placed {
                    for (role, _, size, _) in &p.text {
                        assert!(*size >= TEXT_FLOOR - 1e-3, "{aspect:?} {area:?} {placement:?} {tier:?} {role:?}: size {size} below the floor");
                    }
                }
            }
        }
    }

    #[test]
    fn every_text_rect_stays_inside_its_capped_card_at_every_aspect_and_tier() {
        // Task 9 review F5: a long lower-third's text used to land at y 1416..2237, outside both the card and the
        // frame. Long text in every role, on every placement that carries text, every aspect, every tier: every
        // text rect still lies inside its own card rect.
        let word = |n: usize| "word ".repeat(n);
        let long_panel = |placement: Placement| LayoutItem::Panel {
            id: "/events/0".into(),
            placement,
            headline: Some(word(40)),
            body: Some(word(80)),
            source: Some(word(20)),
            image_aspect: None,
            theme: crate::story::panel::theme(Some("dark")).unwrap(),
        };
        for placement in [Placement::Title, Placement::LowerThird, Placement::Side, Placement::Corner] {
            for aspect in ALL_ASPECTS {
                for tier in [Tier::Hd, Tier::Fullhd, Tier::Qhd, Tier::Uhd] {
                    let rects = safe_area_rects(crate::story::tiers::default_safe_area(aspect), aspect);
                    let l = layout(frame_size(aspect, tier), pixel_ratio(tier), &rects, &[long_panel(placement)], &corners(aspect), &Mono);
                    let Some(p) = l.placed.first() else { continue };
                    for (role, r, size, text) in &p.text {
                        assert!(*size >= TEXT_FLOOR - 1e-3, "{placement:?} {aspect:?} {tier:?} {role:?}: size {size} below the floor");
                        const EPS: f32 = 0.1;
                        assert!(
                            r.0 >= p.rect.0 - EPS && r.1 >= p.rect.1 - EPS && r.2 <= p.rect.2 + EPS && r.3 <= p.rect.3 + EPS,
                            "{placement:?} {aspect:?} {tier:?} {role:?}: text rect {r:?} escapes card {:?} ({text:?})",
                            p.rect
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_side_cards_text_is_capped_by_the_free_bands_height_not_a_fixed_fraction() {
        // Pre-flight review F9: on landscape, the side card's own cap is the free band's height (there is no 30%
        // or 45% fraction for `Side` in landscape at all -- see `card_rect`'s `Placement::Side if f.w > f.h`
        // arm). Bars that squeeze the band to 88 units, well under any of the other placements' caps, force the
        // side card's height to 88 and its text to shrink or truncate to stay inside it.
        let bars = [(0.0, 0.0, 1920.0, 480.0), (0.0, 600.0, 1920.0, 1080.0)];
        let item = LayoutItem::Panel {
            id: "p".into(),
            placement: Placement::Side,
            headline: None,
            body: Some("word ".repeat(60)),
            source: None,
            image_aspect: None,
            theme: crate::story::panel::theme(Some("dark")).unwrap(),
        };
        let l = layout((1920, 1080), 1.0, &bars, &[item], &[], &Mono);
        let p = &l.placed[0];
        assert!((p.rect.3 - p.rect.1 - 88.0).abs() < 0.5, "the side card's height should be the free band's: {:?}", p.rect);
        for (_, r, size, _) in &p.text {
            assert!(*size >= TEXT_FLOOR - 1e-3);
            assert!(r.1 >= p.rect.1 - 0.1 && r.3 <= p.rect.3 + 0.1, "{r:?} escapes {:?}", p.rect);
        }
        assert!(l.warnings.iter().any(|w| w.id == "p" && matches!(w.code, "text_shrunk" | "text_truncated")), "{:?}", l.warnings);
    }

    #[test]
    fn a_long_callout_label_stays_inside_its_capped_box_at_every_aspect_and_tier() {
        // Task 10 review F1 (containment) plus F9 generalised to callouts: a callout's box is capped at the
        // free band's height, the same principle as the side card. Bars that squeeze the band to a fixed 100
        // units, regardless of aspect or tier, with a long label: the box's height tracks the band (this fails
        // if `place_callout`'s `.min((bottom - top).max(0.0))` is removed, since the box would then grow to the
        // label's full natural height instead of 100), and the drawn text still lies inside it.
        let long = "word ".repeat(80);
        for aspect in ALL_ASPECTS {
            for tier in [Tier::Hd, Tier::Fullhd, Tier::Qhd, Tier::Uhd] {
                let (pw, ph) = frame_size(aspect, tier);
                let ratio = pixel_ratio(tier);
                let (w, h) = (pw as f32 / ratio, ph as f32 / ratio);
                let bars = [(0.0, 0.0, w, h / 2.0 - 50.0), (0.0, h / 2.0 + 50.0, w, h)];
                let item = LayoutItem::Callout { id: "c".into(), anchor: (w / 2.0, h / 2.0), text: long.clone(), side: Side::Right, image_aspect: None, theme: dark() };
                let l = layout((pw, ph), ratio, &bars, &[item], &[], &Mono);
                let p = &l.placed[0];
                let f = Frame { w, h, obstacles: bars.iter().map(|r| Obstacle { rect: grow(*r, GAP), source: Source::SafeArea(0) }).collect() };
                let (top, bottom) = f.band();
                let band_h = bottom - top;
                assert!(
                    (p.rect.3 - p.rect.1 - band_h).abs() < 0.5,
                    "{aspect:?} {tier:?}: callout height {:?} should track the band {band_h}",
                    p.rect
                );
                for (_, r, size, text) in &p.text {
                    assert!(*size >= TEXT_FLOOR - 1e-3, "{aspect:?} {tier:?}: size {size}");
                    assert!(
                        r.0 >= p.rect.0 - 0.1 && r.1 >= p.rect.1 - 0.1 && r.2 <= p.rect.2 + 0.1 && r.3 <= p.rect.3 + 0.1,
                        "{aspect:?} {tier:?}: {r:?} escapes {:?} ({text:?})",
                        p.rect
                    );
                }
            }
        }
    }

    #[test]
    fn a_long_caption_stays_inside_its_frame_sized_box_at_every_aspect_and_tier() {
        // Task 10 review F1: a caption is fit to the room the frame allows between its margins
        // (`place_caption`'s `max_h`). A caption long enough to need more than any aspect's cap, even at the
        // floor, on every aspect and tier: the box's height never exceeds `frame height - 2 * CAPTION_MARGIN`
        // (this fails if `place_caption`'s cap is removed, since an uncapped caption would just grow past it),
        // and the drawn text still lies inside the box.
        let long = "word ".repeat(400);
        for aspect in ALL_ASPECTS {
            for tier in [Tier::Hd, Tier::Fullhd, Tier::Qhd, Tier::Uhd] {
                let (pw, ph) = frame_size(aspect, tier);
                let ratio = pixel_ratio(tier);
                let frame_h = ph as f32 / ratio;
                let item = LayoutItem::Caption { id: "cap".into(), text: long.clone(), size: 40.0, anchor: Anchor::Center, offset: (0.0, 0.0), plate: true };
                let l = layout((pw, ph), ratio, &[], &[item], &[], &Mono);
                let p = &l.placed[0];
                assert!(
                    p.rect.3 - p.rect.1 <= frame_h - 2.0 * CAPTION_MARGIN + 0.5,
                    "{aspect:?} {tier:?}: caption height {:?} exceeds the frame's own cap ({})",
                    p.rect,
                    frame_h - 2.0 * CAPTION_MARGIN
                );
                for (_, r, size, text) in &p.text {
                    assert!(*size >= TEXT_FLOOR - 1e-3, "{aspect:?} {tier:?}: size {size}");
                    assert!(
                        r.0 >= p.rect.0 - 0.1 && r.1 >= p.rect.1 - 0.1 && r.2 <= p.rect.2 + 0.1 && r.3 <= p.rect.3 + 0.1,
                        "{aspect:?} {tier:?}: {r:?} escapes {:?} ({text:?})",
                        p.rect
                    );
                }
            }
        }
    }

    #[test]
    fn a_tight_three_block_card_truncates_or_drops_but_never_overflows_its_share() {
        // Task 10 review F2: headline, body and source together, in a corner card capped so small (0.3 * 400 =
        // 120, minus 2 * 24 padding = 72 units for all three) that an even three-way split starves the earlier
        // blocks: 72 / 3 = 24 units is under a single floor-sized line (1.2 * 28 = 33.6), so the headline drops;
        // what it frees lets the body's share grow, but 60 / 2 = 30 is still under a line, so the body drops too;
        // the source then inherits everything left (48 units) and survives truncated. Every block's text rect
        // still lies inside the card, and nothing is silently zeroed without its own warning.
        let long = |n: usize| "word ".repeat(n);
        let item = LayoutItem::Panel {
            id: "/events/0".into(),
            placement: Placement::Corner,
            headline: Some(long(30)),
            body: Some(long(40)),
            source: Some(long(10)),
            image_aspect: None,
            theme: crate::story::panel::theme(Some("dark")).unwrap(),
        };
        let l = layout((1920, 400), 1.0, &[], &[item], &[], &Mono);
        let p = &l.placed[0];
        assert_eq!(p.text.len(), 3);
        let mut saw_drop = false;
        let mut saw_truncated = false;
        for (role, r, size, text) in &p.text {
            assert!(*size >= TEXT_FLOOR - 1e-3, "{role:?}: size {size}");
            const EPS: f32 = 0.1;
            assert!(
                r.0 >= p.rect.0 - EPS && r.1 >= p.rect.1 - EPS && r.2 <= p.rect.2 + EPS && r.3 <= p.rect.3 + EPS,
                "{role:?}: {r:?} escapes {:?}",
                p.rect
            );
            if text.is_empty() {
                saw_drop = true;
                assert!(
                    l.warnings.iter().any(|w| w.id == "/events/0" && w.code == "text_dropped"),
                    "{role:?} came back empty with no `text_dropped` warning"
                );
            } else {
                // Never the whole, un-truncated text at this budget, and never non-empty garbage: either whole
                // (it isn't, here) or an ellipsis survived.
                assert!(text.ends_with('…'), "{role:?}: non-empty but not an ellipsis: {text:?}");
                saw_truncated = true;
            }
        }
        // The fixture is built to exercise both paths, not just one.
        assert!(saw_drop, "nothing dropped: {:?}", p.text);
        assert!(saw_truncated, "nothing truncated: {:?}", p.text);
        assert!(l.warnings.iter().any(|w| w.id == "/events/0" && w.code == "text_truncated"), "{:?}", l.warnings);
    }
}
