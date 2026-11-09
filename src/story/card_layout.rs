//! **The slice (ruling S1): the design system's card, laid out** (spec
//! §3.1-§3.5, §3.7, §5.4-§5.5).
//!
//! The frame around the card -- platform bands, the fine print, the floor
//! and top limit, the landscape anchor, the map share and the camera box
//! (spec §3.6, §5.1-§5.3, §5.6-§5.7) -- is [`crate::story::frame_layout`]
//! (Task R3); [`layout_card`] is re-exported from there. Everything here
//! is pure: a [`CardMeasure`] in, rectangles and text runs (1080 units)
//! out. It wraps text itself, line by line, so the drawer places every
//! line at the token's own line height and what is drawn is what was
//! measured. Nothing here draws: `capture::story_chrome` turns a
//! [`CardLayout`] into plates and text for the capture and the live map.
//!
//! The card's body is [`measure_card`] (Task R2, ruling P13): its rows,
//! steps, balanced wrap, quote, photo, counter and one supporting item,
//! under a height cap, relative to the card's top-left. The frame layout
//! hangs it from the floor and calls it with its own caps (the top limit,
//! the 720 side card) instead of walking the steps again.
//!
//! Every run carries its token's [`TextStyle`] (R12, spec §8): the weight,
//! the tracking and `tnum`/`lnum` for stat values and units, the counter,
//! legend ticks and values. It is measured with that style and drawn with
//! it (`story_chrome::draw_design`), so what is measured is what is drawn.
//!
//! RTL (§9, R14) is [`layout_card`]'s mirror of this LTR body (see
//! `frame_layout::mirror`). What is still owed (and said so): no rounded
//! photo corners (the 1a image quad is square; R15's feathered plate). The moving-camera rule's timing (§3.2)
//! is [`crate::story::cards`]'s: [`CardContent::compact`] is set there and
//! honoured here.

use crate::story::design::{
    Tone, AREA_PALETTE, AREA_RAMP, BODY_M, CAPTION_M, CARD_PAD, COUNTER, COUNTER_GAP, COUNTER_OF, HEADLINE_BODY_GAP, HEADLINE_L, ITEM_GAP, LEGEND_COL_GAP,
    LEGEND_VALUE, LNUM, OTHER, PHOTO_GAP, PHOTO_SIZE, QUOTE, SMALL, STAT_GAP, STAT_L, TITLE, TNUM,
};
pub use crate::story::design::TextStyle;
use crate::story::script::{CardLegendSpec, CardSpec, EventAction, IndexSpec, OverlayLayerKind, OverlayLayerSpec, Script, SourceGroup};
use crate::story::tiers::{Aspect, Rect, SafeArea};

pub use crate::story::frame_layout::{bands, layout_card, right_limit, Anchor, AnchorPref, Bands, CreditsPlacement, Dir, FrameSpec};

/// Text widths for the layout, in 1080 units: `text` on ONE line at `size`
/// and `weight`. The layout breaks lines itself, so it needs nothing else.
pub trait CardMeasure {
    /// `text`'s width on one line at `size` (1080 units), shaped with
    /// `style` (spec §8): its weight, tracking and features. A tracked
    /// run's width ends at its last glyph (no trailing step).
    fn width(&self, text: &str, size: f32, style: TextStyle) -> f32;
}

/// One stat of a card's stats item (spec §3.4).
#[derive(Clone, Debug, PartialEq)]
pub struct Stat {
    pub value: String,
    pub unit: Option<String>,
    pub label: String,
}

/// One row of a card's categorical legend (spec §3.4).
#[derive(Clone, Debug, PartialEq)]
pub struct LegendRow {
    pub color: [u8; 3],
    pub label: String,
    /// Pushed to the column's end, S 700 ink.
    pub value: Option<String>,
}

/// A card's quote (spec §3.2): row 1 in place of the headline; `by` and
/// `source` make the attribution item.
#[derive(Clone, Debug, PartialEq)]
pub struct Quote {
    pub text: String,
    pub by: Option<String>,
    pub source: Option<String>,
}

/// A card's photo (spec §3.4): beside row 1, and the card's one
/// supporting item. Its credit joins the sources (R6), never the image.
#[derive(Clone, Debug, PartialEq)]
pub struct Photo {
    pub src: String,
    pub alt: String,
    /// Joins the sources as `Photo` while the card is on screen
    /// ([`crate::story::cards::sources_at`]).
    pub credit: Option<String>,
}

/// A card's supporting item (spec §3.4). The photo is
/// [`CardContent::photo`]: it is the item, but it sits beside row 1.
#[derive(Clone, Debug, PartialEq)]
pub enum CardItem {
    /// 1 or 2 stats; a third or later is `too_many_stats`.
    Stats(Vec<Stat>),
    /// The categorical grid: 2 columns up to 4 rows, 3 columns for 5-6
    /// (`legend_crowded`), more fold into "Other" (`legend_too_many_items`).
    Legend { rows: Vec<LegendRow>, title: Option<String> },
    /// The ramp through `stops`, its ticks under it, the `current` one in ink.
    Sequential { stops: Vec<[u8; 3]>, ticks: Vec<String>, current: Option<usize>, title: Option<String> },
    /// The quote's `<by>, <source>` line.
    Attribution,
}

impl CardItem {
    /// Spec §3.2's order: legend > stats > photo (2) > attribution.
    fn rank(&self) -> u8 {
        match self {
            CardItem::Legend { .. } | CardItem::Sequential { .. } => 0,
            CardItem::Stats(_) => 1,
            CardItem::Attribution => 3,
        }
    }
}

/// What a card shows, empty fields already dropped (spec §3.2, Review
/// Focus 5): a whitespace-only field is absent, an empty stats or legend
/// list is no item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CardContent {
    pub headline: Option<String>,
    pub body: Option<String>,
    pub caption: Option<String>,
    /// Row 1 when given: wins over `headline` (`headline_ignored`).
    pub quote: Option<Quote>,
    pub photo: Option<Photo>,
    /// The counter `(i, n)` (spec §3.3).
    pub index: Option<(u32, u32)>,
    /// A migrated 1a title panel (spec §10.2, D9): row 1 takes [`TITLE`]'s steps.
    pub title: bool,
    /// The moving-camera rule (spec §3.2): only the caption row (the
    /// headline in the caption style when there is no caption). Set by
    /// [`crate::story::cards::card_at`] (R6).
    pub compact: bool,
    /// Every non-photo supporting item the author gave (a legend, the
    /// stats, the quote's attribution); [`measure_card`] draws the first
    /// in spec order and warns about the rest (`extra_supporting_item`).
    pub items: Vec<CardItem>,
}

fn present(s: Option<&String>) -> Option<String> {
    s.map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// `#rgb` or `#rrggbb` as bytes; anything else `None`.
#[must_use]
pub fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let hex = s.trim().strip_prefix('#')?;
    let digit = |i: usize, n: usize| u8::from_str_radix(hex.get(i..i + n)?, 16).ok();
    match hex.len() {
        6 => Some([digit(0, 2)?, digit(2, 2)?, digit(4, 2)?]),
        3 => Some([digit(0, 1)? * 17, digit(1, 1)? * 17, digit(2, 1)? * 17]),
        _ => None,
    }
}

/// A categorical legend's rows (spec §3.4, §6.3), blank labels dropped
/// and a repeated label kept once (the first): THE one place repeats go,
/// so the card's grid and an area's `match` ([`AreaLegend::fill_color`])
/// list the same rows even for a script validation never saw (which
/// refuses repeats, `duplicate_label`). Each row's colour is its item's `#rrggbb` (not a colour: [`OTHER`]);
/// a blank one is, with `ramp: "sequential"` (ruling R9-b: ordered
/// items), the colour at the row's place along `stops` (the default
/// ramp without them, first row at the first stop and last at the last);
/// else the area palette's at its position ([`OTHER`] past it).
fn legend_rows(spec: &CardLegendSpec) -> Vec<LegendRow> {
    let mut seen = std::collections::HashSet::new();
    let items: Vec<_> = spec.items.iter().flatten().filter(|i| !i.label.trim().is_empty() && seen.insert(i.label.trim().to_string())).collect();
    let ordered = spec.ramp.as_deref().map(str::trim) == Some("sequential");
    let stops: Vec<[u8; 3]> = match &spec.stops {
        Some(stops) if ordered => stops.iter().filter_map(|s| parse_hex(s)).collect(),
        _ => AREA_RAMP.to_vec(),
    };
    let n = items.len();
    items
        .iter()
        .enumerate()
        .map(|(k, i)| {
            let color = if !i.color.trim().is_empty() {
                parse_hex(&i.color).unwrap_or(OTHER)
            } else if ordered {
                ramp_at(&stops, if n > 1 { k as f32 / (n - 1) as f32 } else { 0.0 })
            } else {
                AREA_PALETTE.get(k).copied().unwrap_or(OTHER)
            };
            LegendRow { color, label: i.label.trim().to_string(), value: present(i.value.as_ref()) }
        })
        .collect()
}

/// The colour at `t` (0..1) along `stops` placed at equal spacing,
/// linear in sRGB (as the card's ramp draws them); [`OTHER`] without stops.
#[must_use]
pub fn ramp_at(stops: &[[u8; 3]], t: f32) -> [u8; 3] {
    match stops {
        [] => OTHER,
        [only] => *only,
        _ => {
            let x = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
            let k = (x.floor() as usize).min(stops.len() - 2);
            let f = x - k as f32;
            let (a, b) = (stops[k], stops[k + 1]);
            std::array::from_fn(|c| (f32::from(a[c]) + (f32::from(b[c]) - f32::from(a[c])) * f).round() as u8)
        }
    }
}

/// The rows a categorical legend shows (spec §3.4), and whether some were
/// folded: every row up to [`LEGEND_MAX`]; past it the first
/// `LEGEND_MAX - 1` and one grey "Other". The card's grid and an area's
/// fill ([`AreaLegend::fill_color`]) both fold here, so a category the
/// card shows as "Other" is drawn "Other" on the map too.
#[must_use]
pub fn shown_rows(rows: &[LegendRow]) -> (Vec<LegendRow>, bool) {
    if rows.len() <= LEGEND_MAX {
        return (rows.to_vec(), false);
    }
    let mut shown = rows[..LEGEND_MAX - 1].to_vec();
    shown.push(LegendRow { color: OTHER, label: OTHER_LABEL.into(), value: None });
    (shown, true)
}

/// The folded rows' label (spec §3.4).
pub const OTHER_LABEL: &str = "Other";
/// The property an area's fill reads when its legend names none: a
/// sequential legend's number, a categorical legend's class.
pub const AREA_VALUE_PROPERTY: &str = "value";
pub const AREA_CLASS_PROPERTY: &str = "class";

/// What an [`AreaLegend`] maps a feature's property to.
#[derive(Clone, Debug, PartialEq)]
pub enum AreaScale {
    /// A ramp: `stops` low to high, at equal spacing over the property's
    /// range or at `breaks` (one per stop, ascending); `ticks` and
    /// `current` are the card's.
    Sequential { stops: Vec<[u8; 3]>, ticks: Vec<String>, current: Option<usize>, breaks: Option<Vec<f64>> },
    /// Classes: a feature whose property equals a row's `label` takes its
    /// colour; any other, and a row folded past the card's grid, "Other".
    /// Labels are unique (`legend_rows` keeps the first of a repeat).
    Categorical { rows: Vec<LegendRow> },
}

/// **An `area` layer's legend (spec §6.3): the one set of values its fill
/// and every card legend naming it are drawn from**, so the two cannot
/// disagree. [`area_legend_for`] finds it; [`AreaLegend::fill_color`] is
/// the fill and [`AreaLegend::card_item`] the card's supporting item.
#[derive(Clone, Debug, PartialEq)]
pub struct AreaLegend {
    /// The feature property the fill reads.
    pub property: String,
    pub title: Option<String>,
    pub scale: AreaScale,
}

fn hex6([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

impl AreaLegend {
    /// A legend spec with values (spec §3.4's shapes), `None` without:
    /// `type: "sequential"` (or `stops` and no `items`) is a ramp through
    /// `stops`, the default ramp ([`AREA_RAMP`]) when `stops` is absent;
    /// otherwise `items` are the classes, a blank colour taking the
    /// palette's ([`AREA_PALETTE`]). A legend of `layers` only has none.
    #[must_use]
    pub fn from_spec(spec: &CardLegendSpec) -> Option<Self> {
        let title = present(spec.title.as_ref());
        let sequential = spec.kind.as_deref().map_or(spec.stops.is_some() && spec.items.is_none(), |k| k.trim() == "sequential");
        let property = |default: &str| present(spec.property.as_ref()).unwrap_or_else(|| default.to_string());
        if sequential {
            let stops: Vec<[u8; 3]> = match &spec.stops {
                Some(stops) => stops.iter().filter_map(|s| parse_hex(s)).collect(),
                None => AREA_RAMP.to_vec(),
            };
            if stops.is_empty() {
                return None;
            }
            let mut ticks: Vec<String> = spec.ticks.iter().flatten().map(|t| t.trim().to_string()).collect();
            if ticks.iter().all(String::is_empty) {
                ticks.clear();
            }
            let scale = AreaScale::Sequential { stops, ticks, current: spec.current, breaks: spec.breaks.clone() };
            return Some(Self { property: property(AREA_VALUE_PROPERTY), title, scale });
        }
        let rows = legend_rows(spec);
        (!rows.is_empty()).then(|| Self { property: property(AREA_CLASS_PROPERTY), title, scale: AreaScale::Categorical { rows } })
    }

    /// The card's supporting item from these values (spec §3.4): `title`
    /// and `current` are the card's own when it gives them.
    #[must_use]
    pub fn card_item(&self, title: Option<String>, current: Option<usize>) -> CardItem {
        let title = title.or_else(|| self.title.clone());
        match &self.scale {
            AreaScale::Sequential { stops, ticks, current: c, .. } => {
                CardItem::Sequential { stops: stops.clone(), ticks: ticks.clone(), current: current.or(*c), title }
            }
            AreaScale::Categorical { rows } => CardItem::Legend { rows: rows.clone(), title },
        }
    }

    /// The area's `fill-color` (spec §6.3), a MapLibre expression:
    /// - sequential: `["interpolate", ["linear"], ["get", property], ..]`
    ///   over the stops at `breaks` (one per stop, ascending), else at
    ///   equal spacing over `range` (the property's minimum and maximum
    ///   over the layer's features); one stop, or no range to spread
    ///   over, is the first stop alone;
    /// - categorical: `["match", ["get", property], label, colour, ..,
    ///   "Other"]` over the rows the card shows ([`shown_rows`]).
    #[must_use]
    pub fn fill_color(&self, range: Option<(f64, f64)>) -> serde_json::Value {
        use serde_json::{json, Value};
        let get = json!(["get", self.property]);
        match &self.scale {
            AreaScale::Sequential { stops, breaks, .. } => {
                let n = stops.len();
                let ascending = |b: &[f64]| b.iter().all(|v| v.is_finite()) && b.windows(2).all(|w| w[0] < w[1]);
                let inputs: Option<Vec<f64>> = match (breaks, range) {
                    (Some(b), _) if b.len() == n && n > 1 && ascending(b) => Some(b.clone()),
                    (_, Some((lo, hi))) if n > 1 && lo.is_finite() && hi.is_finite() && lo < hi => {
                        Some((0..n).map(|k| lo + (hi - lo) * k as f64 / (n - 1) as f64).collect())
                    }
                    _ => None,
                };
                let Some(inputs) = inputs else { return json!(hex6(stops[0])) };
                let mut e = vec![json!("interpolate"), json!(["linear"]), get];
                for (x, c) in inputs.iter().zip(stops) {
                    e.push(json!(x));
                    e.push(json!(hex6(*c)));
                }
                Value::Array(e)
            }
            AreaScale::Categorical { rows } => {
                let (shown, folded) = shown_rows(rows);
                let classes = if folded { &shown[..shown.len() - 1] } else { &shown[..] };
                let mut e = vec![json!("match"), get];
                // Labels are unique: `legend_rows` drops repeats.
                for r in classes {
                    e.push(json!(r.label));
                    e.push(json!(hex6(r.color)));
                }
                if e.len() == 2 {
                    return json!(hex6(OTHER));
                }
                e.push(json!(hex6(OTHER)));
                Value::Array(e)
            }
        }
    }
}

/// Whether `layer` is drawn as an `area` in a design story (spec §6.1):
/// `role: "area"`, or a fill with neither a role nor a paint.
#[must_use]
pub fn is_area_layer(layer: &OverlayLayerSpec) -> bool {
    match layer.role.as_deref() {
        Some(role) => role == "area",
        None => layer.paint.is_empty() && layer.kind == OverlayLayerKind::Fill,
    }
}

/// Every card legend whose `layers` name `layer_id`, in the order
/// [`area_legend_for`] reads them (chapters, then events), each with its
/// JSON pointer (`/chapters/<i>/card/legend`, `/events/<i>/card/legend`)
/// and the card's name for a message (`chapter "<id>"`, `the card at
/// events[<i>]`).
#[must_use]
pub fn card_legends_naming<'s>(script: &'s Script, layer_id: &str) -> Vec<(String, String, &'s CardLegendSpec)> {
    let chapters = script.chapters.iter().enumerate().filter_map(|(i, c)| {
        let l = c.card.as_ref()?.legend.as_deref()?;
        Some((format!("/chapters/{i}/card/legend"), format!("chapter \"{}\"", c.id), l))
    });
    let events = script.events.iter().enumerate().filter_map(|(i, e)| match &e.action {
        EventAction::Card(spec) => Some((format!("/events/{i}/card/legend"), format!("the card at events[{i}]"), spec.legend.as_deref()?)),
        _ => None,
    });
    chapters.chain(events).filter(|(_, _, l)| l.layers.iter().flatten().any(|id| id == layer_id)).collect()
}

/// [`area_legend_for`] with where the values came from: the JSON pointer
/// of the legend that holds them and its name for a message (a card's,
/// as [`card_legends_naming`], or `layer "<id>"`).
#[must_use]
pub fn area_legend_origin(script: &Script, layer_id: &str) -> Option<(AreaLegend, String, String)> {
    let (oi, li, layer) = script
        .scene
        .overlays
        .iter()
        .enumerate()
        .find_map(|(oi, o)| o.layers.iter().position(|l| l.id == layer_id).map(|li| (oi, li, &o.layers[li])))?;
    if !is_area_layer(layer) {
        return None;
    }
    card_legends_naming(script, layer_id)
        .into_iter()
        .find_map(|(path, name, l)| AreaLegend::from_spec(l).map(|a| (a, path, name)))
        .or_else(|| {
            let own = AreaLegend::from_spec(layer.legend.as_deref()?)?;
            Some((own, format!("/scene/overlays/{oi}/layers/{li}/legend"), format!("layer \"{layer_id}\"")))
        })
}

/// The values area layer `layer_id` is drawn from (spec §6.3): the first
/// card legend naming it that has values (chapters in order, then
/// events), else the layer's own `legend`; `None` when neither has any
/// (the fill then takes the accent) or the layer is not an area.
#[must_use]
pub fn area_legend_for(script: &Script, layer_id: &str) -> Option<AreaLegend> {
    area_legend_origin(script, layer_id).map(|(a, _, _)| a)
}

impl CardContent {
    /// [`Self::from_spec`] in `script`: a legend whose `layers` name an
    /// `area` layer shows that layer's [`AreaLegend`] -- the values its
    /// fill is drawn from (spec §6.3) -- with this card's `title` and
    /// `current`, whatever the card's own `stops` or `items` say
    /// (validation warns `legend_overridden` when they differ). The cards
    /// pass ([`crate::story::cards`]) builds every card here.
    #[must_use]
    pub fn from_spec_in(spec: &CardSpec, script: &Script) -> Self {
        let mut content = Self::from_spec(spec);
        let Some(legend) = spec.legend.as_ref() else { return content };
        let area = legend.layers.iter().flatten().find_map(|id| area_legend_for(script, id));
        if let Some(area) = area {
            content.items.retain(|i| !matches!(i, CardItem::Legend { .. } | CardItem::Sequential { .. }));
            content.items.insert(0, area.card_item(present(legend.title.as_ref()), legend.current));
        }
        content
    }

    /// A [`CardSpec`] as content on its own, every field trimmed and an
    /// empty one absent. A legend is sequential when its `type` says so
    /// or, with no `type`, when it has `stops` and no `items` (spec
    /// §3.4); otherwise its `items` are the categorical rows. A legend of
    /// `layers` only gives no item here: it takes an area's values in
    /// [`Self::from_spec_in`]. `index: true` needs the tour's numbered
    /// layer and is resolved by the cards pass (R6).
    #[must_use]
    pub fn from_spec(spec: &CardSpec) -> Self {
        let mut items = Vec::new();
        if let Some(l) = spec.legend.as_ref() {
            let title = present(l.title.as_ref());
            let sequential = l.kind.as_deref().map_or(l.stops.is_some() && l.items.is_none(), |k| k.trim() == "sequential");
            if sequential {
                let stops: Vec<[u8; 3]> = l.stops.iter().flatten().filter_map(|s| parse_hex(s)).collect();
                // Blank ticks keep their place on the ramp (a tick's position
                // is its index) but draw nothing; all blank is no tick row.
                let mut ticks: Vec<String> = l.ticks.iter().flatten().map(|t| t.trim().to_string()).collect();
                if ticks.iter().all(String::is_empty) {
                    ticks.clear();
                }
                if !stops.is_empty() {
                    items.push(CardItem::Sequential { stops, ticks, current: l.current, title });
                }
            } else {
                let rows = legend_rows(l);
                if !rows.is_empty() {
                    items.push(CardItem::Legend { rows, title });
                }
            }
        }
        let stats: Vec<Stat> = spec
            .stats
            .iter()
            .flatten()
            .filter(|s| !s.value.trim().is_empty())
            .map(|s| Stat { value: s.value.trim().to_string(), unit: present(s.unit.as_ref()), label: s.label.trim().to_string() })
            .collect();
        if !stats.is_empty() {
            items.push(CardItem::Stats(stats));
        }
        let quote = spec.quote.as_ref().and_then(|q| {
            let text = q.text.trim();
            (!text.is_empty()).then(|| Quote { text: text.to_string(), by: present(q.by.as_ref()), source: present(q.source.as_ref()) })
        });
        if quote.as_ref().is_some_and(|q| q.by.is_some() || q.source.is_some()) {
            items.push(CardItem::Attribution);
        }
        let photo = spec.photo.as_ref().filter(|p| !p.src.trim().is_empty()).map(|p| Photo { src: p.src.trim().to_string(), alt: p.alt.trim().to_string(), credit: present(p.credit.as_ref()) });
        let index = match spec.index {
            Some(IndexSpec::At { i, n }) if n > 0 => Some((i, n)),
            _ => None,
        };
        CardContent {
            headline: present(spec.headline.as_ref()),
            body: present(spec.body.as_ref()),
            caption: present(spec.caption.as_ref()),
            quote,
            photo,
            index,
            title: false,
            compact: false,
            items,
        }
    }

    /// Whether nothing would be drawn: no plate for an empty card. A
    /// counter alone is not a card.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.headline.is_none() && self.body.is_none() && self.caption.is_none() && self.quote.is_none() && self.photo.is_none() && self.items.is_empty()
    }
}

/// A frame's design-system part ([`crate::capture::StoryFrame::design`]):
/// what the player decided at `t`, laid out and drawn by the chrome pass.
#[derive(Clone, Debug, PartialEq)]
pub struct DesignFrame {
    pub card: Option<CardContent>,
    /// The card being replaced while a card-to-card ease runs (spec §3.1):
    /// `Some` only inside the ease, with [`DesignFrame::ease`] below 1.
    pub from_card: Option<CardContent>,
    /// The card-to-card ease's linear progress, 0..1; 1 when settled.
    pub ease: f32,
    /// The card's fade, 0..1 (spec §3.1's `fade_s`).
    pub card_alpha: f32,
    /// The hairline's filled fraction (spec §3.3), `None` for no hairline.
    pub progress: Option<f32>,
    pub accent: [u8; 3],
    pub tone: Tone,
    /// `output.safe_area` as the design system reads it (D1: absent is `none`).
    pub safe_area: SafeArea,
    pub aspect: Aspect,
    /// The sources line's groups, in order, deduplicated (spec §3.6).
    pub sources: Vec<SourceGroup>,
    /// The tile credit (spec §3.6): the basemap's, the imagery's.
    pub credit: Option<String>,
    /// `design.anchor` (spec §5.7): landscape compact, side, or the rule.
    pub anchor: AnchorPref,
    /// The reading direction (spec §9): RTL mirrors the card and the
    /// colophon row's text and fills the hairline from the right.
    pub dir: Dir,
    /// `design.credits` (spec §3.6): the bottom row, or lifted above a
    /// platform's band.
    pub credits: CreditsPlacement,
}

impl DesignFrame {
    /// The frame this design part lays out on, `units` (1080 units) wide
    /// and tall, with `mark_w` the slot mark's width
    /// ([`crate::story::frame_layout::slot_mark_w`]).
    #[must_use]
    pub fn frame_spec(&self, units: (f32, f32), mark_w: Option<f32>) -> FrameSpec {
        FrameSpec { w: units.0, h: units.1, aspect: self.aspect, safe_area: self.safe_area, dir: self.dir, anchor: self.anchor, tone: self.tone, mark_w, credits: self.credits }
    }
}

/// What colour a run takes (by the tone, in the drawer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    /// ink / white.
    Ink,
    /// ink-2 / white .80.
    Ink2,
    /// ink-3 / white .60.
    Ink3,
    /// The story's accent (lifted on an `ink` card, spec §4.2): the
    /// counter's numeral and the quote marks.
    Accent,
}

/// One line of text, placed: its line box's top-left, 1080 units.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub text: String,
    pub x: f32,
    pub top: f32,
    pub size: f32,
    /// `style.weight`, kept as its own field for the many readers that
    /// only ask for the weight.
    pub weight: u16,
    /// How the run is shaped (spec §8): the weight again, the tracking,
    /// `tnum`/`lnum`. Measured and drawn with this.
    pub style: TextStyle,
    pub line_height: f32,
    pub ink: Ink,
    /// The run's width as measured.
    pub width: f32,
}

/// [`layout_card`]'s result, 1080 units.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CardLayout {
    /// The card plate, `None` without a card.
    pub card: Option<Rect>,
    /// The card's text, every line its own run.
    pub runs: Vec<TextRun>,
    /// A legend's swatches: centre-free rectangles and colours.
    pub swatches: Vec<(Rect, [u8; 3])>,
    /// The hairline's track along the card's top edge (spec §3.3).
    pub hairline: Option<Rect>,
    /// The sources and the tile credit, every line its own run.
    pub fine: Vec<TextRun>,
    /// The fine print's top (spec §3.6): the colophon row's top, the
    /// mark's box included when it shares the bottom row.
    pub fine_top: f32,
    /// The mark's INK top-left (spec §3.6, D7 as amended 2026-10-05 by
    /// the user): bottom-left on every frame, its ink on the 64 keyline and
    /// its bottom on `h - 64` ([`crate::story::frame_layout::mark_origin`]). The drawn quad stands its artwork's side margin further left.
    pub logo: (f32, f32),
    /// The fade's ramp `(y0, y1)` (spec §3.7): 0 at `y0`, the peak at `y1`.
    pub fade: (f32, f32),
    /// The lifted fade's taper (spec §3.7), `None` where the fade holds its
    /// peak to the bottom edge: `(y, k)`, the peak held from `y1` to `y`,
    /// then easing down to `k` times the peak at the bottom edge.
    pub fade_tail: Option<(f32, f32)>,
    /// The card's bottom edge (spec §5.2).
    pub floor: f32,
    /// Warning codes (spec §11's names).
    pub warnings: Vec<&'static str>,
    /// Each of `runs`' alpha factor during a card-to-card ease; empty
    /// (every run at 1) otherwise.
    pub run_alpha: Vec<f32>,
    /// Each of `swatches`' alpha factor, as `run_alpha`.
    pub swatch_alpha: Vec<f32>,
    /// The photo's square and its `src` (spec §3.4).
    pub photo: Option<(Rect, String)>,
    /// The sequential legend's ramp and its stops (spec §3.4).
    pub ramp: Option<(Rect, Vec<[u8; 3]>)>,
    /// The counter's two runs (spec §3.3).
    pub counter: Vec<TextRun>,
    /// The photo's, the ramp's and the counter's alpha factor during a
    /// card-to-card ease; `None` (1) otherwise.
    pub extras_alpha: Option<f32>,
    /// Where the card hangs (spec §5.2, §5.7).
    pub anchor: Anchor,
    /// The share of the visible frame left to the map (spec §5.6): where
    /// the fade is under half its peak, less the card.
    pub map_share: f32,
    /// The box the camera centres the story in (spec §5.3, §5.7): above a
    /// bottom or compact card (or the fine print), beside a side card.
    pub camera_box: Rect,
}

/// The design mark's height (spec §3.6, D7 as amended 2026-10-05): 28
/// units, its width from the artwork's aspect (about 135 for the SWAT
/// lockup, 117 of it ink). Every consumer -- the colophon row's layout,
/// the slot width, the drawn quad -- reads this one token.
pub const LOGO_H: f32 = 28.0;
/// The gap between a lifted row and the bottom band (D2).
pub const BAND_GAP: f32 = 12.0;
/// The mark's ink to the colophon row's first text run (spec §3.6).
pub const MARK_GAP: f32 = 24.0;
/// Between a source group's key and its text.
pub const KEY_GAP: f32 = 6.0;
/// Between two source groups.
pub const GROUP_GAP: f32 = 22.0;
/// The hairline's height (spec §3.3).
pub const HAIRLINE_H: f32 = 5.0;
/// The fade's rise above a lifted row (spec §3.7).
pub const LIFTED_FADE_LEAD: f32 = 96.0;
/// How far below a lifted row the fade holds its peak (spec §3.7).
pub const LIFTED_FADE_HOLD: f32 = 60.0;
/// What is left of a lifted fade at the bottom edge, as a fraction of its
/// peak (spec §3.7): .25 alpha on the paper tone's .88.
pub const LIFTED_FADE_TAIL: f32 = 0.25 / 0.88;
/// The share of the way from the held peak to the bottom edge over which
/// a lifted fade tapers to [`LIFTED_FADE_TAIL`] (spec §3.7).
pub const LIFTED_FADE_TAPER: f32 = 0.5;

/// `text` broken greedily at spaces so no line is wider than `max_w`
/// (a single word wider than that keeps its own line).
pub(crate) fn wrap(m: &dyn CardMeasure, text: &str, size: f32, style: TextStyle, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() { word.to_string() } else { format!("{current} {word}") };
        if current.is_empty() || m.width(&candidate, size, style) <= max_w {
            current = candidate;
        } else {
            lines.push(std::mem::replace(&mut current, word.to_string()));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    // Spec §9 (R14 review): every line its own paragraph, isolates closed.
    crate::story::cards::balance_isolates(lines)
}

/// A balanced wrap (spec §5.5): the narrowest width, found by at most 8
/// halvings to 1 unit, at which `text` still wraps into `n` lines or
/// fewer. The search starts at the greedy wrap's widest line (capped at
/// `max_w`), so it never adds a line or widens the block; if the
/// narrower wrap would (an ulp either way), the greedy wrap stands.
fn balanced(m: &dyn CardMeasure, text: &str, size: f32, style: TextStyle, max_w: f32, n: usize) -> Vec<String> {
    let greedy = wrap(m, text, size, style, max_w);
    let widest = |lines: &[String]| lines.iter().map(|l| m.width(l, size, style)).fold(0.0_f32, f32::max);
    let greedy_w = widest(&greedy);
    let (mut lo, mut hi) = (0.0_f32, greedy_w.min(max_w));
    for _ in 0..8 {
        if hi - lo <= 1.0 {
            break;
        }
        let mid = (lo + hi) / 2.0;
        if wrap(m, text, size, style, mid).len() <= n {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let lines = wrap(m, text, size, style, hi);
    if lines.len() <= n && lines.len() == greedy.len() && widest(&lines) <= greedy_w {
        lines
    } else {
        greedy
    }
}

/// `lines` cut to `n`, the last ending in an ellipsis that fits `max_w`.
fn truncate(m: &dyn CardMeasure, mut lines: Vec<String>, n: usize, size: f32, style: TextStyle, max_w: f32) -> Vec<String> {
    lines.truncate(n.max(1));
    let lines = truncate_last(m, lines, size, style, max_w);
    // Words dropped for the ellipsis may take a PDI with them.
    crate::story::cards::balance_isolates(lines)
}

/// [`truncate`]'s ellipsis on the last line.
fn truncate_last(m: &dyn CardMeasure, mut lines: Vec<String>, size: f32, style: TextStyle, max_w: f32) -> Vec<String> {
    if let Some(last) = lines.last_mut() {
        let mut words: Vec<&str> = last.split_whitespace().collect();
        loop {
            let candidate = format!("{}…", words.join(" "));
            if words.len() <= 1 || m.width(&candidate, size, style) <= max_w {
                *last = candidate;
                break;
            }
            words.pop();
        }
    }
    lines
}

/// A text block set through its token's steps (spec §5.4): `(lines,
/// size, line height, step index)`, and whether it was truncated. Each
/// step is broken at its own size and carries its own line height
/// (`size * token.line_height / token.size`): a 48 x 3 headline is three
/// lines of 52.8, never measured against the 60/66 token.
fn stepped(m: &dyn CardMeasure, text: &str, token: &crate::story::design::TextToken, max_w: f32, balance: bool) -> (Vec<String>, f32, f32, usize, bool) {
    let ratio = token.line_height / token.size;
    let steps: &[(f32, usize)] = if token.steps.is_empty() { &[(token.size, 1)] } else { token.steps };
    for (i, &(size, max_lines)) in steps.iter().enumerate() {
        let lines = wrap(m, text, size, token.style(), max_w);
        if lines.len() <= max_lines {
            let lines = if balance && lines.len() > 1 { balanced(m, text, size, token.style(), max_w, lines.len()) } else { lines };
            return (lines, size, size * ratio, i, false);
        }
    }
    let (size, max_lines) = steps.last().copied().unwrap_or((token.size, 1));
    let lines = truncate(m, wrap(m, text, size, token.style(), max_w), max_lines, size, token.style(), max_w);
    (lines, size, size * ratio, steps.len().saturating_sub(1), true)
}

/// One text row of the card, before placement.
struct Row {
    lines: Vec<String>,
    size: f32,
    line_height: f32,
    style: TextStyle,
    ink: Ink,
    /// A quote: its first line opens and its last closes with a mark in
    /// the accent (spec §4.1).
    quoted: bool,
    /// `text_shrunk` or `text_truncated`, when its steps said so.
    note: Option<&'static str>,
}

impl Row {
    fn height(&self) -> f32 {
        self.lines.len() as f32 * self.line_height
    }
}

/// A quote's marks, drawn in the accent.
pub const QUOTE_OPEN: &str = "\u{201C}";
pub const QUOTE_CLOSE: &str = "\u{201D}";
/// The sequential ramp (spec §3.4): 12 high, radius 6, at most 560 wide.
pub const RAMP_H: f32 = 12.0;
pub const RAMP_RADIUS: f32 = 6.0;
pub const RAMP_MAX_W: f32 = 560.0;
/// The ramp to its tick row, and the tick row's height.
const TICK_GAP: f32 = 10.0;
const TICK_ROW_H: f32 = 34.0;
/// A legend's title, under the grid or the ticks.
const TITLE_GAP: f32 = 4.0;
/// The categorical grid's rows and their gap (spec §3.4).
const LEGEND_ROW_H: f32 = 36.0;
const LEGEND_ROW_GAP: f32 = 8.0;
/// A legend dot, and the dot to its label.
const DOT: f32 = 16.0;
const DOT_GAP: f32 = 12.0;
/// The least room between a legend label and its value.
const VALUE_GAP: f32 = 16.0;
/// The attribution's line height (spec §3.4).
const ATTRIBUTION_LH: f32 = 34.0;
/// A stat's unit after its value, and its label under it.
const UNIT_GAP: f32 = 6.0;
const STAT_LABEL_GAP: f32 = 4.0;
/// The most stats a card draws (`too_many_stats` past it).
pub(crate) const MAX_STATS: usize = 2;
/// A categorical legend at full size; past it, 3 columns
/// (`legend_crowded`); past [`LEGEND_MAX`], "Other" (`legend_too_many_items`).
pub(crate) const LEGEND_FULL: usize = 4;
pub(crate) const LEGEND_MAX: usize = 6;
/// Where the baseline sits in a shaped block, as a share of the size
/// (Source Sans 3: ascender 1000 over a 1.2-size block holding 1.326).
const BASELINE_IN_BLOCK: f32 = 0.937;

/// A run's baseline below its line box's top: the drawer centres the
/// shaped block (`size * LABEL_LINE_HEIGHT_RATIO`) in the box.
fn baseline_offset(size: f32, line_height: f32) -> f32 {
    (line_height - size * crate::symbol_pass::LABEL_LINE_HEIGHT_RATIO) / 2.0 + BASELINE_IN_BLOCK * size
}

/// [`measure_card`]'s result: the card's size and everything on it, 1080
/// units, relative to the card's top-left.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CardBody {
    /// 0 x 0 for a card that draws nothing (no plate).
    pub w: f32,
    pub h: f32,
    pub runs: Vec<TextRun>,
    pub swatches: Vec<(Rect, [u8; 3])>,
    /// The photo's square and its `src`.
    pub photo: Option<(Rect, String)>,
    /// The sequential ramp and its stops.
    pub ramp: Option<(Rect, Vec<[u8; 3]>)>,
    /// The counter: `i` in the accent, then ` / n` in ink-3.
    pub counter: Vec<TextRun>,
    /// Spec §11's codes, in the order they arose.
    pub warnings: Vec<&'static str>,
}

/// The supporting item a card draws.
#[derive(Clone, Copy)]
pub(crate) enum Chosen<'a> {
    Item(&'a CardItem),
    Photo(&'a Photo),
}

impl Chosen<'_> {
    /// Its plain-English name (spec §11's `extra_supporting_item`,
    /// `supporting_item_deferred` and `card_too_tall` templates, R10):
    /// a word the author recognises, never a Rust type name.
    #[must_use]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Chosen::Photo(_) => "photo",
            Chosen::Item(CardItem::Stats(_)) => "stats",
            Chosen::Item(CardItem::Legend { .. } | CardItem::Sequential { .. }) => "legend",
            Chosen::Item(CardItem::Attribution) => "quote's attribution",
        }
    }
}

/// Every non-empty supporting item `c` could draw, in spec §3.2's order
/// (legend > stats > photo > attribution): [`choose`]'s first is the one
/// drawn, the rest are `extra_supporting_item` (R10).
pub(crate) fn ranked_items(c: &CardContent) -> Vec<Chosen<'_>> {
    let attributed = c.quote.as_ref().is_some_and(|q| q.by.is_some() || q.source.is_some());
    let mut all: Vec<(u8, Chosen<'_>)> = c.items.iter().filter(|i| attributed || !matches!(i, CardItem::Attribution)).map(|i| (i.rank(), Chosen::Item(i))).collect();
    if let Some(p) = &c.photo {
        all.push((2, Chosen::Photo(p)));
    }
    all.sort_by_key(|(rank, _)| *rank);
    all.into_iter().map(|(_, ch)| ch).collect()
}

/// Spec §3.2: the first item in the order legend > stats > photo >
/// attribution, and how many others were given (each one
/// `extra_supporting_item`).
fn choose(c: &CardContent) -> (Option<Chosen<'_>>, usize) {
    let all = ranked_items(c);
    let extra = all.len().saturating_sub(1);
    (all.first().copied(), extra)
}

/// `text` on one line within `max_w`, cut a character at a time and ended
/// with an ellipsis when it does not fit: `(text, width)`.
fn fit_chars(m: &dyn CardMeasure, text: &str, size: f32, style: TextStyle, max_w: f32) -> (String, f32) {
    let w = m.width(text, size, style);
    if w <= max_w {
        return (text.to_string(), w);
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = format!("{}…", chars.iter().collect::<String>().trim_end());
        let cw = m.width(&candidate, size, style);
        if cw <= max_w || chars.is_empty() {
            return (candidate, cw);
        }
    }
    (String::new(), 0.0)
}

#[allow(clippy::too_many_arguments)]
fn run(m: &dyn CardMeasure, text: &str, x: f32, top: f32, size: f32, style: TextStyle, line_height: f32, ink: Ink) -> TextRun {
    TextRun { text: text.to_string(), x, top, size, weight: style.weight, style, line_height, ink, width: m.width(text, size, style) }
}

/// The advance from the end of a tracked run's last glyph to where the
/// next run of the same line starts: one tracking step (0 untracked, and
/// 0 on a script that takes none). `CardMeasure` widths end at the ink,
/// so two runs set side by side need it between them.
fn track_step(text: &str, size: f32, style: TextStyle) -> f32 {
    style.for_text(text).letter_spacing_em() * size
}

/// The supporting item `item` placed at `(x, y)` within `avail` wide:
/// its runs, swatches or ramp into `body`; its `(width, height)`.
fn place_item(item: &CardItem, quote: Option<&Quote>, x: f32, y: f32, avail: f32, m: &dyn CardMeasure, body: &mut CardBody) -> (f32, f32) {
    match item {
        CardItem::Stats(stats) => {
            if stats.len() > MAX_STATS {
                body.warnings.push("too_many_stats");
            }
            let mut sx = x;
            let widths: Vec<f32> = stats.iter().take(MAX_STATS).map(|s| stat_width(m, s)).collect();
            for (s, sw) in stats.iter().zip(&widths) {
                let vw = m.width(&s.value, STAT_L.size, STAT_L.style());
                body.runs.push(TextRun { text: s.value.clone(), x: sx, top: y, size: STAT_L.size, weight: STAT_L.weight, style: STAT_L.style(), line_height: STAT_L.line_height, ink: Ink::Ink, width: vw });
                if let Some(unit) = &s.unit {
                    let uw = m.width(unit, SMALL.size, unit_style());
                    // On the value's baseline: the line boxes' bottoms, roughly.
                    let utop = y + STAT_L.line_height - SMALL.line_height - 6.0;
                    body.runs.push(TextRun { text: unit.clone(), x: sx + vw + UNIT_GAP, top: utop, size: SMALL.size, weight: SMALL.weight, style: unit_style(), line_height: SMALL.line_height, ink: Ink::Ink2, width: uw });
                }
                let lw = m.width(&s.label, SMALL.size, SMALL.style());
                body.runs.push(TextRun {
                    text: s.label.clone(),
                    x: sx,
                    top: y + STAT_L.line_height + STAT_LABEL_GAP,
                    size: SMALL.size,
                    weight: SMALL.weight,
                    style: SMALL.style(),
                    line_height: SMALL.line_height,
                    ink: Ink::Ink3,
                    width: lw,
                });
                sx += sw + STAT_GAP;
            }
            let w = widths.iter().sum::<f32>() + STAT_GAP * widths.len().saturating_sub(1) as f32;
            (w, STAT_L.line_height + STAT_LABEL_GAP + SMALL.line_height)
        }
        CardItem::Legend { rows, title } => {
            let (shown, folded) = shown_rows(rows);
            if folded {
                body.warnings.push("legend_too_many_items");
            }
            if shown.len() > LEGEND_FULL {
                body.warnings.push("legend_crowded");
            }
            let cols = if shown.len() > LEGEND_FULL { 3 } else { shown.len().clamp(1, 2) };
            let n_rows = shown.len().div_ceil(cols);
            let value_w = |r: &LegendRow| r.value.as_deref().map_or(0.0, |v| VALUE_GAP + m.width(v, LEGEND_VALUE.size, LEGEND_VALUE.style()));
            let natural = shown.iter().map(|r| DOT + DOT_GAP + m.width(&r.label, SMALL.size, SMALL.style()) + value_w(r)).fold(0.0_f32, f32::max);
            let gaps = LEGEND_COL_GAP * cols.saturating_sub(1) as f32;
            let col_w = natural.min(((avail - gaps) / cols as f32).max(0.0));
            for (i, r) in shown.iter().enumerate() {
                let (c, rr) = (i % cols, i / cols);
                let rx = x + c as f32 * (col_w + LEGEND_COL_GAP);
                let ry = y + rr as f32 * (LEGEND_ROW_H + LEGEND_ROW_GAP);
                body.swatches.push(((rx, ry + 10.0, rx + DOT, ry + 26.0), r.color));
                let vw = value_w(r);
                let (label, lw) = fit_chars(m, &r.label, SMALL.size, SMALL.style(), (col_w - DOT - DOT_GAP - vw).max(0.0));
                body.runs.push(TextRun { text: label, x: rx + DOT + DOT_GAP, top: ry + 2.0, size: SMALL.size, weight: SMALL.weight, style: SMALL.style(), line_height: SMALL.line_height, ink: Ink::Ink2, width: lw });
                if let Some(v) = &r.value {
                    let w = m.width(v, LEGEND_VALUE.size, LEGEND_VALUE.style());
                    body.runs.push(TextRun { text: v.clone(), x: rx + col_w - w, top: ry + 2.0, size: LEGEND_VALUE.size, weight: LEGEND_VALUE.weight, style: LEGEND_VALUE.style(), line_height: LEGEND_VALUE.line_height, ink: Ink::Ink, width: w });
                }
            }
            let grid_w = cols as f32 * col_w + gaps;
            let grid_h = n_rows as f32 * LEGEND_ROW_H + n_rows.saturating_sub(1) as f32 * LEGEND_ROW_GAP;
            let title_w = legend_title(m, title.as_deref(), x, y + grid_h + TITLE_GAP, avail, body);
            (grid_w.max(title_w), grid_h + if title_w > 0.0 { TITLE_GAP + SMALL.line_height } else { 0.0 })
        }
        CardItem::Sequential { stops, ticks, current, title } => {
            let rw = RAMP_MAX_W.min(avail).max(0.0);
            body.ramp = Some(((x, y, x + rw, y + RAMP_H), stops.clone()));
            let mut h = RAMP_H;
            if ticks.iter().any(|t| !t.trim().is_empty()) {
                let top = y + RAMP_H + TICK_GAP;
                let n = ticks.len();
                for (k, t) in ticks.iter().enumerate().filter(|(_, t)| !t.trim().is_empty()) {
                    let (t, tw) = fit_chars(m, t.trim(), SMALL.size, tick_style(), rw);
                    let tx = if k == 0 {
                        x
                    } else if k + 1 == n {
                        x + rw - tw
                    } else {
                        x + rw * k as f32 / (n - 1) as f32 - tw / 2.0
                    };
                    let ink = if *current == Some(k) { Ink::Ink } else { Ink::Ink3 };
                    body.runs.push(TextRun { text: t, x: tx, top, size: SMALL.size, weight: SMALL.weight, style: tick_style(), line_height: TICK_ROW_H, ink, width: tw });
                }
                h += TICK_GAP + TICK_ROW_H;
            }
            let title_w = legend_title(m, title.as_deref(), x, y + h + TITLE_GAP, avail, body);
            if title_w > 0.0 {
                h += TITLE_GAP + SMALL.line_height;
            }
            (rw.max(title_w), h)
        }
        CardItem::Attribution => {
            let Some(q) = quote else { return (0.0, 0.0) };
            let (by, rest) = match (&q.by, &q.source) {
                (Some(b), Some(s)) => (Some(b.clone()), Some(format!(", {s}"))),
                (Some(b), None) => (Some(b.clone()), None),
                (None, Some(s)) => (None, Some(s.clone())),
                (None, None) => return (0.0, 0.0),
            };
            let mut w = 0.0_f32;
            if let Some(by) = by {
                let (t, bw) = fit_chars(m, &by, SMALL.size, TextStyle::of_weight(600), avail);
                body.runs.push(TextRun { text: t, x, top: y, size: SMALL.size, weight: 600, style: TextStyle::of_weight(600), line_height: ATTRIBUTION_LH, ink: Ink::Ink, width: bw });
                w = bw;
            }
            if let Some(rest) = rest {
                let (t, rw) = fit_chars(m, &rest, SMALL.size, TextStyle::of_weight(400), (avail - w).max(0.0));
                if !t.is_empty() {
                    body.runs.push(TextRun { text: t, x: x + w, top: y, size: SMALL.size, weight: 400, style: TextStyle::of_weight(400), line_height: ATTRIBUTION_LH, ink: Ink::Ink2, width: rw });
                    w += rw;
                }
            }
            (w, ATTRIBUTION_LH)
        }
    }
}

/// A legend's optional title at `(x, top)`, S 600 ink-3, cut to `avail`:
/// its width, 0 without one.
fn legend_title(m: &dyn CardMeasure, title: Option<&str>, x: f32, top: f32, avail: f32, body: &mut CardBody) -> f32 {
    let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) else { return 0.0 };
    let (t, w) = fit_chars(m, title, SMALL.size, SMALL.style(), avail);
    body.runs.push(TextRun { text: t, x, top, size: SMALL.size, weight: SMALL.weight, style: SMALL.style(), line_height: SMALL.line_height, ink: Ink::Ink3, width: w });
    w
}

fn stat_width(m: &dyn CardMeasure, s: &Stat) -> f32 {
    let value = m.width(&s.value, STAT_L.size, STAT_L.style());
    let unit = s.unit.as_deref().map_or(0.0, |u| UNIT_GAP + m.width(u, SMALL.size, unit_style()));
    (value + unit).max(m.width(&s.label, SMALL.size, SMALL.style()))
}

/// A stat's unit (spec §8): the S token, tabular and lining like its value.
fn unit_style() -> TextStyle {
    SMALL.style().with_features(TNUM | LNUM)
}

/// A sequential legend's tick (spec §8): the S token, tabular and lining.
fn tick_style() -> TextStyle {
    SMALL.style().with_features(TNUM | LNUM)
}

/// One attempt at a card body: `keep` text rows at most, `chosen` as the
/// item. `rows` is how many rows it set; `deferred` says the three-line
/// rule dropped the item.
struct Built {
    body: CardBody,
    rows: usize,
    deferred: bool,
}

fn build(c: &CardContent, chosen: Option<Chosen<'_>>, keep: usize, max_w: f32, x0: f32, m: &dyn CardMeasure) -> Built {
    let (pad_top, pad_side, pad_bottom) = CARD_PAD;
    let mut body = CardBody::default();
    // The counter (spec §3.3), never on a compact card.
    let counter = c.index.filter(|_| !c.compact).map(|(i, n)| {
        let (a, b) = (i.to_string(), format!(" / {n}"));
        let (wa, wb) = (m.width(&a, COUNTER.size, COUNTER.style()), m.width(&b, COUNTER_OF.size, COUNTER_OF.style()));
        // `wa` is the advance to ` / n`: the numeral's ink and one
        // tracking step, as the two runs read as one tracked line.
        let step = track_step(&a, COUNTER.size, COUNTER.style());
        (a, wa + step, b, wb)
    });
    let counter_w = counter.as_ref().map_or(0.0, |(_, wa, _, wb)| wa + wb);
    let photo_off = if matches!(chosen, Some(Chosen::Photo(_))) { PHOTO_SIZE + PHOTO_GAP } else { 0.0 };
    let counter_off = if counter.is_some() { counter_w + COUNTER_GAP } else { 0.0 };
    // Spec §5.4: the text width less the photo's 236 and the counter's room.
    let text_w = (max_w - 2.0 * pad_side - photo_off - counter_off).max(0.0);
    let set = |text: &str, token: &crate::story::design::TextToken, balance: bool, ink: Ink, quoted: bool| -> Row {
        let (lines, size, line_height, step, cut) = stepped(m, text, token, text_w, balance);
        let note = if cut {
            Some("text_truncated")
        } else if step > 0 {
            Some("text_shrunk")
        } else {
            None
        };
        Row { lines, size, line_height, style: token.style(), ink, quoted, note }
    };
    let mut rows: Vec<Row> = Vec::new();
    let mut has_row1 = false;
    if c.compact {
        // The moving-camera rule (spec §3.2): the caption row only, else
        // the headline (or the quote) set in the caption style.
        let text = c.caption.as_deref().or(c.headline.as_deref()).or(c.quote.as_ref().map(|q| q.text.as_str()));
        rows.extend(text.map(|t| set(t, &CAPTION_M, false, Ink::Ink, false)));
    } else {
        if let Some(q) = &c.quote {
            rows.push(set(&format!("{QUOTE_OPEN}{}{QUOTE_CLOSE}", q.text), &QUOTE, true, Ink::Ink, true));
            has_row1 = true;
        } else if let Some(h) = &c.headline {
            rows.push(set(h, if c.title { &TITLE } else { &HEADLINE_L }, true, Ink::Ink, false));
            has_row1 = true;
        }
        match (&c.body, &c.caption) {
            (Some(t), _) => rows.push(set(t, &BODY_M, false, Ink::Ink2, false)),
            (None, Some(t)) => rows.push(set(t, &CAPTION_M, false, Ink::Ink, false)),
            _ => {}
        }
    }
    rows.truncate(keep.max(1));
    // The three-line rule (spec §3.2): row 1 on 3 lines defers a stats or legend item.
    let mut chosen = chosen;
    let deferred = has_row1
        && rows.first().is_some_and(|r| r.lines.len() >= 3)
        && matches!(chosen, Some(Chosen::Item(CardItem::Stats(_) | CardItem::Legend { .. } | CardItem::Sequential { .. })));
    if deferred {
        chosen = None;
    }
    if rows.is_empty() && chosen.is_none() {
        return Built { body, rows: 0, deferred };
    }
    body.warnings.extend(rows.iter().filter_map(|r| r.note));
    let x = x0 + pad_side;
    let col_x = x + photo_off;
    let col_h = rows.iter().map(Row::height).sum::<f32>() + HEADLINE_BODY_GAP * rows.len().saturating_sub(1) as f32;
    // With a photo, the text column is centred against it (spec §3.2).
    let block_h = if photo_off > 0.0 { col_h.max(PHOTO_SIZE) } else { col_h };
    if let Some(Chosen::Photo(p)) = chosen {
        let top = pad_top + (block_h - PHOTO_SIZE) / 2.0;
        body.photo = Some(((x, top, x + PHOTO_SIZE, top + PHOTO_SIZE), p.src.clone()));
    }
    let mut y = pad_top + (block_h - col_h) / 2.0;
    let (mut widest, mut row1_w) = (0.0_f32, 0.0_f32);
    let mut first_baseline = None;
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            y += HEADLINE_BODY_GAP;
        }
        let n = row.lines.len();
        let mut row_w = 0.0_f32;
        for (k, line) in row.lines.iter().enumerate() {
            let mut rest = line.as_str();
            let mut lx = col_x;
            if row.quoted && k == 0 {
                if let Some(r) = rest.strip_prefix(QUOTE_OPEN) {
                    let mark = run(m, QUOTE_OPEN, lx, y, row.size, row.style, row.line_height, Ink::Accent);
                    lx += mark.width + track_step(line, row.size, row.style);
                    body.runs.push(mark);
                    rest = r;
                }
            }
            let close = if row.quoted && k + 1 == n { rest.strip_suffix(QUOTE_CLOSE) } else { None };
            if let Some(r) = close {
                rest = r;
            }
            if !rest.is_empty() {
                let text = run(m, rest, lx, y, row.size, row.style, row.line_height, row.ink);
                lx += text.width + track_step(line, row.size, row.style);
                body.runs.push(text);
            }
            if close.is_some() {
                body.runs.push(run(m, QUOTE_CLOSE, lx, y, row.size, row.style, row.line_height, Ink::Accent));
            }
            row_w = row_w.max(m.width(line, row.size, row.style));
            first_baseline = first_baseline.or(Some(y + baseline_offset(row.size, row.line_height)));
            y += row.line_height;
        }
        if i == 0 {
            row1_w = row_w;
        }
        widest = widest.max(row_w);
    }
    let mut y = pad_top + block_h;
    let mut item_w = 0.0_f32;
    if let Some(Chosen::Item(item)) = chosen {
        if !rows.is_empty() {
            y += ITEM_GAP;
        }
        let (w, h) = place_item(item, c.quote.as_ref(), x, y, text_w, m, &mut body);
        item_w = w;
        y += h;
    }
    // Spec §5.5: the widest of the text, the item, photo + 28 + text, and
    // text + 28 + counter; capped at the maximum.
    let text_part = if rows.is_empty() { photo_off.min(PHOTO_SIZE) } else { photo_off + widest };
    let counter_part = if counter.is_some() { photo_off + row1_w + COUNTER_GAP + counter_w } else { 0.0 };
    let inner = text_part.max(item_w).max(counter_part).min((max_w - 2.0 * pad_side).max(0.0));
    body.w = 2.0 * pad_side + inner;
    body.h = y + pad_bottom;
    if let (Some((a, wa, b, wb)), Some(base)) = (counter, first_baseline.filter(|_| !rows.is_empty())) {
        // At the row's end, on row 1's first baseline (spec §3.2).
        let right = x + inner;
        let top = base - baseline_offset(COUNTER.size, COUNTER.line_height);
        let ink_a = wa - track_step(&a, COUNTER.size, COUNTER.style());
        body.counter.push(TextRun { text: a, x: right - wa - wb, top, size: COUNTER.size, weight: COUNTER.weight, style: COUNTER.style(), line_height: COUNTER.line_height, ink: Ink::Accent, width: ink_a });
        body.counter.push(TextRun { text: b, x: right - wb, top, size: COUNTER_OF.size, weight: COUNTER_OF.weight, style: COUNTER_OF.style(), line_height: COUNTER_OF.line_height, ink: Ink::Ink3, width: wb });
    }
    Built { rows: rows.len(), body, deferred }
}

/// The card's body (spec §3.2-§3.4, §5.4, §5.5), 1080 units, relative to
/// its top-left: rows set through their steps (balanced for row 1), the
/// quote with its marks, the photo beside row 1, the counter at row 1's
/// end and the one supporting item, at most `max_w` wide. Taller than
/// `max_h`, it drops its item (`card_too_tall`), then its last text row
/// (`text_truncated`), down to row 1 (ruling P13: R3's small frames and
/// side cards call this with their own caps). An empty card is 0 x 0.
#[must_use]
pub fn measure_card(content: &CardContent, max_w: f32, max_h: f32, m: &dyn CardMeasure) -> CardBody {
    measure_card_at(content, max_w, max_h, 0.0, m)
}

/// [`measure_card`] with its left edge at `x0`, so the layout's runs are
/// computed where they are drawn (bit-for-bit what the slice computed).
pub(crate) fn measure_card_at(content: &CardContent, max_w: f32, max_h: f32, x0: f32, m: &dyn CardMeasure) -> CardBody {
    if content.is_empty() {
        return CardBody::default();
    }
    let mut warnings = Vec::new();
    let (mut chosen, extra) = if content.compact { (None, 0) } else { choose(content) };
    warnings.extend(std::iter::repeat_n("extra_supporting_item", extra));
    if !content.compact && content.quote.is_some() && content.headline.is_some() {
        warnings.push("headline_ignored");
    }
    let mut built = build(content, chosen, usize::MAX, max_w, x0, m);
    if built.deferred {
        warnings.push("supporting_item_deferred");
        chosen = None;
    }
    if built.body.h > max_h && chosen.is_some() {
        chosen = None;
        warnings.push("card_too_tall");
        built = build(content, None, usize::MAX, max_w, x0, m);
    }
    // Bounded: at most one try per row, so a body that will not shrink
    // ends here (over the cap) instead of looping.
    for _ in 0..built.rows {
        if built.body.h <= max_h || built.rows <= 1 {
            break;
        }
        warnings.push("text_truncated");
        built = build(content, chosen, built.rows - 1, max_w, x0, m);
    }
    let mut body = built.body;
    warnings.append(&mut body.warnings);
    body.warnings = warnings;
    body
}

/// The id a card photo's decoded image carries in
/// [`crate::capture::StoryFrame::images`]: the drawer finds the photo
/// there, and the 1a image path uploads it like any story image.
#[must_use]
pub fn card_photo_id(src: &str) -> String {
    format!("card-photo/{src}")
}

/// The card-to-card ease's length, seconds (spec §3.1).
pub const CARD_EASE_S: f64 = 0.3;

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let s = if e1 > e0 { ((x - e0) / (e1 - e0)).clamp(0.0, 1.0) } else if x >= e1 { 1.0 } else { 0.0 };
    s * s * (3.0 - 2.0 * s)
}

/// The card mid-ease from `from` to `to` at linear progress `s` (spec
/// §3.1): the plate's rectangle eases (smoothstep) from `from`'s to `to`'s
/// -- both sit on the same floor at x 32, so only the width and the height
/// move -- while `from`'s content fades out over the first half and `to`'s
/// fades in over the second, each kept at its own place under the eased
/// top edge; a line both cards draw holds at full alpha and glides. The
/// fine print and the fade are `to`'s. Pure in `s`, so a
/// seek lands on the frame playing reaches.
#[must_use]
pub fn ease_cards(from: &CardLayout, to: &CardLayout, s: f32) -> CardLayout {
    let mut out = to.clone();
    let (Some(a), Some(b)) = (from.card, to.card) else { return out };
    let s = s.clamp(0.0, 1.0);
    let k = smoothstep(0.0, 1.0, s);
    let lerp = |x: f32, y: f32| x + (y - x) * k;
    let card = (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2), lerp(a.3, b.3));
    let (fade_out, fade_in) = (1.0 - smoothstep(0.0, 0.5, s), smoothstep(0.5, 1.0, s));
    let shift = |r: &TextRun, dy: f32| TextRun { top: r.top + dy, ..r.clone() };
    let (dy_a, dy_b) = (card.1 - a.1, card.1 - b.1);
    out.card = Some(card);
    // A line both cards draw -- the same text in the same style (a
    // headline kept while a cue swaps row 2, the caption row kept through
    // the moving-camera rule's compact <-> full, R6) -- holds: drawn once
    // at full alpha, gliding from its place under the old top edge to its
    // place under the new one, rather than fading out and back in. Each
    // line pairs with at most one line of the other card.
    let same = |r: &TextRun, o: &TextRun| {
        r.text == o.text && r.size == o.size && r.style == o.style && r.line_height == o.line_height && r.ink == o.ink
    };
    let mut taken = vec![false; to.runs.len()];
    let pair: Vec<Option<usize>> = from
        .runs
        .iter()
        .map(|r| {
            let j = (0..to.runs.len()).find(|j| !taken[*j] && same(r, &to.runs[*j]))?;
            taken[j] = true;
            Some(j)
        })
        .collect();
    out.runs = from
        .runs
        .iter()
        .zip(&pair)
        .map(|(r, j)| match j {
            Some(j) => {
                let o = &to.runs[*j];
                TextRun { x: lerp(r.x, o.x), top: card.1 + lerp(r.top - a.1, o.top - b.1), ..r.clone() }
            }
            None => shift(r, dy_a),
        })
        .chain(to.runs.iter().map(|r| shift(r, dy_b)))
        .collect();
    out.run_alpha = pair
        .iter()
        .map(|j| if j.is_some() { 1.0 } else { fade_out })
        .chain(taken.iter().map(|t| if *t { 0.0 } else { fade_in }))
        .collect();
    let move_rect = |r: &Rect, dy: f32| (r.0, r.1 + dy, r.2, r.3 + dy);
    out.swatches = from.swatches.iter().map(|(r, c)| (move_rect(r, dy_a), *c)).chain(to.swatches.iter().map(|(r, c)| (move_rect(r, dy_b), *c))).collect();
    out.swatch_alpha = std::iter::repeat_n(fade_out, from.swatches.len()).chain(std::iter::repeat_n(fade_in, to.swatches.len())).collect();
    // The photo, the ramp and the counter: the outgoing card's over the
    // first half, the incoming card's over the second (the two fades
    // never overlap, so one slot each is exact).
    let (src, dy, k) = if s < 0.5 { (from, dy_a, fade_out) } else { (to, dy_b, fade_in) };
    out.photo = src.photo.as_ref().map(|(r, p)| (move_rect(r, dy), p.clone()));
    out.ramp = src.ramp.as_ref().map(|(r, stops)| (move_rect(r, dy), stops.clone()));
    out.counter = src.counter.iter().map(|r| shift(r, dy)).collect();
    out.extras_alpha = Some(k);
    let inset = crate::story::design::CARD_RADIUS * 0.6;
    out.hairline = to.hairline.or(from.hairline).map(|_| (card.0 + inset, card.1, card.2 - inset, card.1 + HAIRLINE_H));
    out.floor = card.3;
    out
}

/// The hairline's filled part (spec §3.3, §9) on a track `x`..`x + w`
/// (any unit) at progress `p`: `(start, width)`, filled from the start
/// edge -- the left in LTR, the right in RTL. `p` is clamped to 0..1.
#[must_use]
pub fn hairline_fill(x: f32, w: f32, p: f32, dir: Dir) -> (f32, f32) {
    let filled = w * p.clamp(0.0, 1.0);
    match dir {
        Dir::Ltr => (x, filled),
        Dir::Rtl => (x + w - filled, filled),
    }
}

/// One layer of the card's shadow (spec §3.1): an offset, a CSS blur
/// radius and an ink alpha.
pub const CARD_SHADOW: [((f32, f32), f32, f32); 2] = [((0.0, 10.0), 30.0, 0.14), ((0.0, 1.0), 2.0, 0.10)];

/// The standard normal CDF (Abramowitz and Stegun 7.1.26, |error| < 1.5e-7).
fn normal_cdf(x: f32) -> f32 {
    let z = f64::from(x) / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * z.abs());
    let poly = t * (0.254_829_592 + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-z * z).exp();
    (0.5 * (1.0 + if z < 0.0 { -erf } else { erf })) as f32
}

/// The card's soft shadow under `card` (spec §3.1), as nested rounded
/// plates of ink, `(rect, radius, alpha)` in units, drawn bottom to top:
/// each [`CARD_SHADOW`] layer's Gaussian edge (sigma = blur / 2) sliced in
/// optical depth over +-2.5 sigma, so the stack composites to the blurred
/// edge's profile within a step. The plate pipeline has no feathered plate
/// yet (spec §6.4); this is the cheap stand-in, a pure function of `card`.
#[must_use]
pub fn card_shadow(card: Rect, radius: f32) -> Vec<(Rect, f32, f32)> {
    let mut out = Vec::new();
    for ((dx, dy), blur, peak) in CARD_SHADOW {
        let sigma = (blur / 2.0).max(0.5);
        let n: usize = if blur >= 8.0 { 14 } else { 3 };
        let span = 2.5 * sigma;
        let step = 2.0 * span / n as f32;
        let depth = |d: f32| -(1.0 - peak * normal_cdf(-d / sigma)).ln();
        for k in 0..n {
            let (outer, inner) = (span - k as f32 * step, span - (k + 1) as f32 * step);
            let alpha = 1.0 - (-(depth(inner) - depth(outer))).exp();
            let g = (outer + inner) / 2.0;
            let r = (card.0 - g + dx, card.1 - g + dy, card.2 + g + dx, card.3 + g + dy);
            if alpha > 0.0 && r.2 > r.0 && r.3 > r.1 {
                out.push((r, (radius + g).max(0.0), alpha));
            }
        }
    }
    out
}

/// The whole fade's alpha over its peak at `y` (spec §3.7): the ramp
/// ([`fade_alpha`]), then, on a lifted fade, the held peak and the taper:
/// smoothstep from 1 at the tail's start down to its fraction, reached
/// [`LIFTED_FADE_TAPER`] of the way to `h` and held to `h`.
#[must_use]
pub fn fade_profile(fade: (f32, f32), tail: Option<(f32, f32)>, h: f32, y: f32) -> f32 {
    let ramp = fade_alpha(fade, y);
    match tail {
        Some((from, k)) if y > from => {
            // Over the first half of the way down, then held: the band's
            // lower half reads as map, not as a white wash.
            let span = (h - from) * LIFTED_FADE_TAPER;
            let s = if span > 0.0 { ((y - from) / span).clamp(0.0, 1.0) } else { 1.0 };
            ramp * (1.0 - (1.0 - k) * s * s * (3.0 - 2.0 * s))
        }
        _ => ramp,
    }
}

/// The fade's alpha over its peak at `y` (spec §3.7): smoothstep of
/// `(y - y0) / (y1 - y0)`, held at 1 below `y1`.
#[must_use]
pub fn fade_alpha(fade: (f32, f32), y: f32) -> f32 {
    let (y0, y1) = fade;
    let s = if y1 > y0 { ((y - y0) / (y1 - y0)).clamp(0.0, 1.0) } else if y >= y1 { 1.0 } else { 0.0 };
    s * s * (3.0 - 2.0 * s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::story::design::CARD_MAX_W;

    /// The slice's positional call, kept so its D2 tests read as they did:
    /// a default (LTR, `auto`, paper) [`FrameSpec`].
    fn layout_card(frame: (f32, f32), safe_area: SafeArea, aspect: Aspect, card: Option<&CardContent>, sources: &[SourceGroup], credit: Option<&str>, m: &dyn CardMeasure) -> CardLayout {
        crate::story::frame_layout::layout_card(&FrameSpec::new(frame, safe_area, aspect), card, sources, credit, m)
    }

    /// Every glyph is `0.5 * size` wide: whole, predictable numbers.
    struct Half;
    impl CardMeasure for Half {
        fn width(&self, text: &str, size: f32, _style: TextStyle) -> f32 {
            text.chars().count() as f32 * size * 0.5
        }
    }

    fn src(key: &str, text: &str) -> SourceGroup {
        SourceGroup { key: key.into(), text: text.into() }
    }

    fn headline(s: &str) -> CardContent {
        CardContent { headline: Some(s.into()), ..CardContent::default() }
    }

    const PORTRAIT: (f32, f32) = (1080.0, 1920.0);
    const LANDSCAPE: (f32, f32) = (1920.0, 1080.0);

    /// The paragraph direction a line shapes with (UAX #9 P2/P3, as
    /// cosmic-text resolves it): its first strong character, skipping
    /// every isolate's content; `None` with none.
    fn first_strong_rtl(line: &str) -> Option<bool> {
        use unicode_script::{Script as S, UnicodeScript as _};
        let mut depth = 0usize;
        for c in line.chars() {
            match c {
                '\u{2066}' | '\u{2067}' | '\u{2068}' => depth += 1,
                '\u{2069}' => depth = depth.saturating_sub(1),
                c if depth == 0 && c.is_alphabetic() => return Some(matches!(c.script(), S::Arabic | S::Hebrew)),
                _ => {}
            }
        }
        None
    }

    /// Whether every isolate `line` opens it also closes, and no PDI
    /// closes one it did not open.
    fn balanced_line(line: &str) -> bool {
        let mut depth = 0i32;
        for c in line.chars() {
            match c {
                '\u{2066}' | '\u{2067}' | '\u{2068}' => depth += 1,
                '\u{2069}' => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        depth == 0
    }

    /// R14 review (Important): an Arabic headline whose isolated `Marina
    /// Bay` falls on a line break. Each drawn line is its own paragraph:
    /// unbalanced, the second would start on Latin `Bay` and flip to LTR.
    /// Balanced, every line opens and closes its own isolates, starts on
    /// an Arabic letter, and keeps every word in order; in an RTL frame
    /// every line is right-aligned.
    #[test]
    fn a_wrapped_line_keeps_its_isolates_balanced() {
        let text = crate::story::cards::isolate_ltr_runs("يقع سد Marina Bay عند مصب الخليج");
        // At size 2 every char is 1 unit (`Half`): "يقع سد", FSI, "Marina" is 14.
        let lines = wrap(&Half, &text, 2.0, TextStyle::default(), 15.0);
        assert!(lines.len() >= 3 && lines[0].ends_with("Marina\u{2069}") && lines[1].starts_with("\u{2068}Bay\u{2069}"), "the break splits the run: {lines:?}");
        for l in &lines {
            assert!(balanced_line(l), "{l:?}");
            assert_eq!(first_strong_rtl(l), Some(true), "{l:?} starts RTL");
        }
        let strip = |s: &str| s.chars().filter(|c| !matches!(c, '\u{2066}'..='\u{2069}')).collect::<String>();
        assert_eq!(lines.iter().map(|l| strip(l)).collect::<Vec<_>>().join(" "), strip(&text), "no word lost or moved");
        // Truncation keeps the last line balanced too.
        let cut = truncate(&Half, lines.clone(), 2, 2.0, TextStyle::default(), 15.0);
        assert!(cut.iter().all(|l| balanced_line(l)), "{cut:?}");
        // Through the layout: an RTL card sets the headline in balanced,
        // RTL-first lines, each right-aligned on the card's padding.
        let card = CardContent { headline: Some(crate::story::cards::isolate_ltr_runs("في وسط المدينة القديمة Marina Bay سد يحجز مياه الخليج كلها")), ..CardContent::default() };
        let frame = FrameSpec { dir: Dir::Rtl, ..FrameSpec::new(PORTRAIT, SafeArea::None, Aspect::Portrait) };
        let l = crate::story::frame_layout::layout_card(&frame, Some(&card), &[], None, &Half);
        let c = l.card.expect("a card");
        let head: Vec<&TextRun> = l.runs.iter().filter(|r| r.weight == HEADLINE_L.weight).collect();
        assert!(head.len() >= 2, "{head:?}");
        assert!(head.iter().any(|r| r.text.contains("Marina") && !r.text.contains("Bay")), "the run is split here too: {head:?}");
        for r in head {
            assert!(balanced_line(&r.text) && first_strong_rtl(&r.text) == Some(true), "{:?}", r.text);
            assert!((r.x + r.width - (c.2 - 32.0)).abs() < 1e-3, "{:?} right-aligned", r.text);
        }
    }

    /// Spec §3.3, §9: the hairline fills from the start edge -- the right
    /// in RTL -- by the same amount, and never past the track.
    #[test]
    fn the_hairline_fills_from_the_right() {
        let (x, w) = (47.0, 400.0);
        assert_eq!(hairline_fill(x, w, 0.25, Dir::Ltr), (47.0, 100.0));
        assert_eq!(hairline_fill(x, w, 0.25, Dir::Rtl), (347.0, 100.0), "ends on the track's right edge");
        for p in [-1.0, 0.0, 0.4, 1.0, 3.0] {
            let (l, r) = (hairline_fill(x, w, p, Dir::Ltr), hairline_fill(x, w, p, Dir::Rtl));
            assert_eq!(l.1, r.1, "{p}: the same fill");
            assert_eq!(l.0, x, "{p}: LTR from the left");
            assert_eq!(r.0 + r.1, x + w, "{p}: RTL to the right edge");
            assert!(r.0 >= x && r.1 <= w, "{p}: inside the track");
        }
    }

    #[test]
    fn the_floors_hold_for_reels_all_and_none() {
        // Spec §3.6, §5.2 (amended 2026-10-05): by default the colophon row
        // sits on the bottom row, so under a platform the band sets the
        // floor (band top - 12); with `design.credits: "above_platform"`
        // the lifted row (no mark in it: one line 26, a second 26) does,
        // at its top - 32.
        let one = [src("Data", "OSM")];
        let two = [src("Data", &"x".repeat(60)), src("Route", &"y".repeat(40))];
        let credit = Some("OpenFreeMap");
        let floor_in = |credits, sa, sources: &[SourceGroup]| {
            let spec = FrameSpec { credits, ..FrameSpec::new(PORTRAIT, sa, Aspect::Portrait) };
            crate::story::frame_layout::layout_card(&spec, None, sources, credit, &Half).floor
        };
        let floor = |sa, sources: &[SourceGroup]| floor_in(CreditsPlacement::Bottom, sa, sources);
        let lifted = |sa, sources: &[SourceGroup]| floor_in(CreditsPlacement::AbovePlatform, sa, sources);
        for sources in [&one[..], &two[..], &[]] {
            assert_eq!(floor(SafeArea::Reels, sources), 1488.0);
            assert_eq!(floor(SafeArea::All, sources), 1428.0);
            assert_eq!(floor(SafeArea::Tiktok, sources), 1428.0);
            assert_eq!(floor(SafeArea::Shorts, sources), 1528.0);
        }
        assert_eq!(lifted(SafeArea::Reels, &one), 1430.0);
        assert_eq!(lifted(SafeArea::Reels, &two), 1404.0);
        assert_eq!(lifted(SafeArea::Reels, &[]), 1430.0, "the credit alone is one line");
        assert_eq!(lifted(SafeArea::All, &one), 1370.0);
        assert_eq!(lifted(SafeArea::All, &two), 1344.0);
        assert_eq!(lifted(SafeArea::Tiktok, &one), 1370.0);
        assert_eq!(lifted(SafeArea::Shorts, &one), 1470.0);
        for mode in [CreditsPlacement::Bottom, CreditsPlacement::AbovePlatform] {
            assert_eq!(floor_in(mode, SafeArea::None, &one), 1796.0);
            assert_eq!(floor_in(mode, SafeArea::None, &two), 1770.0);
            assert_eq!(floor_in(mode, SafeArea::None, &[]), 1796.0);
        }
    }

    #[test]
    fn youtube_floors_are_948_and_930() {
        let floor = |sources: &[SourceGroup]| layout_card(LANDSCAPE, SafeArea::Youtube, Aspect::Landscape, None, sources, Some("OpenFreeMap"), &Half).floor;
        assert_eq!(floor(&[src("Data", "OSM")]), 948.0);
        assert_eq!(floor(&[src("Data", &"x".repeat(100)), src("Route", &"y".repeat(100))]), 930.0);
    }

    #[test]
    fn the_lifted_row_sits_above_the_band_and_the_mark_stays_bottom_left() {
        let spec = FrameSpec { credits: CreditsPlacement::AbovePlatform, ..FrameSpec::new(PORTRAIT, SafeArea::Reels, Aspect::Portrait) };
        let l = crate::story::frame_layout::layout_card(&spec, Some(&headline("Port to port")), &[src("Data", "OSM")], Some("OpenFreeMap"), &Half);
        for run in &l.fine {
            assert!(run.top + run.line_height <= 1488.0 + 1e-3, "{run:?} above the band top - 12");
            assert!(run.x + run.width <= 932.0 + 1e-3, "{run:?} inside the right limit");
        }
        // One line, no mark in it: the text ends on the row's bottom edge;
        // the mark stays bottom-left (the user's rule, 2026-10-05).
        let credit = l.fine.iter().find(|r| r.text == "OpenFreeMap").expect("the credit");
        assert_eq!(credit.top + credit.line_height, 1488.0);
        assert_eq!(l.logo, (64.0, 1920.0 - 64.0 - 28.0), "the mark bottom-left, not lifted");
        assert_eq!(l.fine_top, 1462.0);
        let card = l.card.expect("a card");
        assert_eq!(card.3, l.floor);
        assert!(card.3 <= l.fine_top - 32.0);
        assert_eq!(card.0, 32.0);
        assert!(card.1 >= 220.0, "nothing in the top band");
        assert_eq!(l.fade, (l.fine_top - 96.0, l.fine_top));
        assert_eq!(l.fade_tail, Some((1488.0 + 60.0, 0.25 / 0.88)));
        let at = |y| fade_profile(l.fade, l.fade_tail, 1920.0, y);
        assert_eq!(at(l.fine_top), 1.0, "the row sits on the peak");
        assert_eq!(at(1548.0), 1.0, "held 60 below the row");
        assert!((at(1920.0) - 0.25 / 0.88).abs() < 1e-6, "tapers to .25 alpha at the bottom edge");
        assert!(at(1700.0) < 1.0 && at(1700.0) > at(1900.0));
    }

    #[test]
    fn landscape_and_none_put_the_card_bottom_left_above_the_fine_print() {
        for (frame, sa, aspect) in [(LANDSCAPE, SafeArea::Youtube, Aspect::Landscape), (PORTRAIT, SafeArea::None, Aspect::Portrait)] {
            let l = layout_card(frame, sa, aspect, Some(&headline("Arrival")), &[src("Data", "OSM")], Some("OpenFreeMap"), &Half);
            let card = l.card.expect("a card");
            assert_eq!(card.0, 32.0);
            assert!(card.3 <= l.fine_top - 32.0, "{sa:?}");
            assert_eq!(l.fade.1, frame.1, "{sa:?}: the fade runs to the bottom edge");
            assert_eq!(l.fade_tail, None, "{sa:?}: held at the peak");
        }
    }

    #[test]
    fn empty_fields_are_absent() {
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"headline": "  ", "body": "", "stats": [], "legend": {"items": []}})).unwrap();
        let content = CardContent::from_spec(&spec);
        assert!(content.is_empty(), "{content:?}");
        // Task R2: a blank quote, a photo with no `src`, a ramp with no stops,
        // a stat with no value and a legend row with no label are absent too.
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"quote": {"text": " ", "by": "Lee"}, "photo": {"src": " ", "alt": "x"},
            "legend": {"type": "sequential", "stops": [], "ticks": ["a"]}, "stats": [{"label": "x", "value": " "}]})).unwrap();
        let content = CardContent::from_spec(&spec);
        assert!(content.is_empty(), "{content:?}");
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"legend": {"items": [{"color": "#000", "label": "  "}]}})).unwrap();
        assert!(CardContent::from_spec(&spec).is_empty());
        // Every tick blank: no tick row, the card ends under the ramp.
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"headline": "Haze", "legend": {"type": "sequential", "stops": ["#000", "#fff"], "ticks": [" ", ""]}})).unwrap();
        let c = CardContent::from_spec(&spec);
        assert!(matches!(&c.items[..], [CardItem::Sequential { ticks, .. }] if ticks.is_empty()), "{c:?}");
        let l = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&c), &[], None, &Half);
        let ramp = l.ramp.expect("the ramp").0;
        assert_eq!((l.runs.len(), l.card.unwrap().3), (1, ramp.3 + 32.0), "no empty 44-unit tick row");
        // A blank tick among others keeps its slot and draws nothing.
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"legend": {"type": "sequential", "stops": ["#000", "#fff"], "ticks": ["1 Sep", " ", "28 Sep"]}})).unwrap();
        let l = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&CardContent::from_spec(&spec)), &[], None, &Half);
        assert_eq!(l.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["1 Sep", "28 Sep"]);
        assert_eq!(measure_card(&CardContent::default(), 900.0, 1e9, &Half), CardBody::default(), "0 x 0, nothing in it");
        let l = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&content), &[], None, &Half);
        assert!(l.card.is_none() && l.runs.is_empty(), "no plate for an empty card");
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"headline": "Port Klang", "body": " "})).unwrap();
        let l = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&CardContent::from_spec(&spec)), &[], None, &Half);
        let card = l.card.expect("a card");
        assert_eq!(l.runs.len(), 1, "the headline alone, no empty body row");
        assert_eq!(card.3 - card.1, 30.0 + 66.0 + 32.0, "padding plus one headline line");
    }

    #[test]
    fn an_overflowing_headline_steps_down_then_drops_the_item() {
        let stats = vec![CardItem::Stats(vec![Stat { value: "350".into(), unit: Some("km".into()), label: "Distance".into() }])];
        // 30 glyphs at 60 is 900 wide: two lines at 60 in 826 units.
        let two = CardContent { headline: Some("Through the straits of Malacca".repeat(2)), items: stats.clone(), ..CardContent::default() };
        let l = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&two), &[], None, &Half);
        let sizes: Vec<f32> = l.runs.iter().filter(|r| r.weight == 700).map(|r| r.size).collect();
        assert!(sizes.iter().all(|s| *s < 60.0), "stepped down: {sizes:?}");
        assert!(l.warnings.contains(&"text_shrunk"), "{:?}", l.warnings);
        // So long it needs three lines: the item is deferred.
        let three = CardContent { headline: Some("Through the straits ".repeat(4)), items: stats, ..CardContent::default() };
        let l = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&three), &[], None, &Half);
        assert!(l.warnings.contains(&"supporting_item_deferred"), "{:?}", l.warnings);
        assert!(l.runs.iter().all(|r| r.size != STAT_L.size || r.weight == 700), "no stat drawn");
        // Spec §3.2: "the stats or legend item" -- both legend kinds defer
        // too; the attribution and the photo do not.
        let ramp = CardItem::Sequential { stops: vec![[0; 3], [255; 3]], ticks: vec!["a".into(), "b".into()], current: None, title: None };
        let categorical = CardItem::Legend { rows: vec![LegendRow { color: [0; 3], label: "x".into(), value: None }], title: None };
        for item in [ramp, categorical] {
            let l = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&CardContent { items: vec![item.clone()], ..three.clone() }), &[], None, &Half);
            assert!(l.warnings.contains(&"supporting_item_deferred") && l.ramp.is_none() && l.swatches.is_empty(), "{item:?}: {:?}", l.warnings);
            assert_eq!(l.runs.len(), 3, "{item:?}: the three headline lines only");
        }
        for run in &l.runs {
            assert!(run.x + run.width <= 32.0 + CARD_MAX_W.min(932.0 - 32.0) - 32.0 + 1e-3, "{run:?} inside the card");
        }
    }

    #[test]
    fn text_wraps_within_the_card_and_the_card_hugs_it() {
        let c = CardContent { headline: Some("Port Klang".into()), body: Some("Malaysia's busiest port, at the end of a passage that hugs the coast.".into()), ..CardContent::default() };
        let l = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&c), &[], None, &Half);
        let card = l.card.unwrap();
        assert!(card.2 - card.0 <= 900.0);
        for run in &l.runs {
            assert!(run.x >= card.0 + 32.0 && run.x + run.width <= card.2 - 32.0 + 1e-3, "{run:?} in {card:?}");
        }
        let short = layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(&headline("Hi")), &[], None, &Half).card.unwrap();
        assert_eq!(short.2 - short.0, 64.0 + 60.0, "shrink-wrapped to its text");
    }

    /// Spec §3.6: the fine print clears 4.5:1 on the fade at its position,
    /// over the light basemaps' water (`#AAD3DF`, OpenFreeMap's `#9EBDFF`)
    /// and land, on the bottom row's fade and on the tapered fade of a row
    /// lifted with `design.credits: "above_platform"`.
    #[test]
    fn the_fine_print_clears_aa_on_either_fade() {
        use crate::story::design::{composite, contrast, FADE_PAPER_PEAK, INK, INK_2};
        for (sa, credits) in [SafeArea::Reels, SafeArea::All, SafeArea::Tiktok, SafeArea::Shorts].into_iter().flat_map(|sa| [(sa, CreditsPlacement::Bottom), (sa, CreditsPlacement::AbovePlatform)]) {
            let sources = [src("Map data", "OpenStreetMap"), src("Data", "Hand-drawn route, illustrative")];
            let spec = FrameSpec { credits, ..FrameSpec::new(PORTRAIT, sa, Aspect::Portrait) };
            let l = crate::story::frame_layout::layout_card(&spec, Some(&headline("Port to port")), &sources, Some("OpenFreeMap © OpenMapTiles Data from OpenStreetMap"), &Half);
            assert!(!l.fine.is_empty());
            for run in &l.fine {
                let lowest = (0..=8).map(|i| fade_profile(l.fade, l.fade_tail, PORTRAIT.1, run.top + run.line_height * i as f32 / 8.0)).fold(1.0_f32, f32::min);
                for bg in [[0xAA, 0xD3, 0xDF], [0x9E, 0xBD, 0xFF], [0xD3, 0xEA, 0xC0], [0xF8, 0xF4, 0xF0]] {
                    let ground = composite([0xFF; 3], lowest * FADE_PAPER_PEAK, bg);
                    for ink in [INK, INK_2] {
                        let c = contrast(ink, ground);
                        assert!(c >= 4.5, "{sa:?} {:?} on {bg:?}: {c}:1", run.text);
                    }
                }
            }
        }
    }

    #[test]
    fn a_card_to_card_ease_moves_the_plate_and_crossfades_the_content() {
        let lay = |c: &CardContent| layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(c), &[], None, &Half);
        let a = lay(&headline("Port to port"));
        let long = CardContent { body: Some("Malaysia's busiest port, at the end of a passage that hugs the coast.".into()), ..headline("Port Klang, the destination") };
        let b = lay(&long);
        assert_ne!(a.card, b.card, "the two cards differ in size");
        let at = |s: f32| ease_cards(&a, &b, s);
        assert_eq!(at(0.0).card, a.card, "starts on the old plate");
        assert_eq!(at(1.0).card, b.card, "ends on the new one");
        let mid = at(0.5).card.expect("a plate");
        let (ra, rb) = (a.card.unwrap(), b.card.unwrap());
        assert_eq!(mid.0, 32.0);
        assert_eq!(mid.3, ra.3, "the floor holds");
        assert!(mid.1 < ra.1.max(rb.1) && mid.1 > ra.1.min(rb.1), "the top edge is between");
        let n = a.runs.len();
        assert_eq!(at(0.0).run_alpha[..n], vec![1.0; n][..], "old content shown at the start");
        assert!(at(0.0).run_alpha[n..].iter().all(|k| *k == 0.0));
        assert!(at(1.0).run_alpha[..n].iter().all(|k| *k == 0.0));
        assert!(at(1.0).run_alpha[n..].iter().all(|k| *k == 1.0), "new content shown at the end");
        let top = at(0.3).card.unwrap().1;
        assert!(at(0.3).runs.iter().all(|r| r.top >= top - 1e-3), "text hangs from the eased top");
        assert_eq!(at(0.37), at(0.37), "a pure function of s");
    }

    /// R6: a line both cards draw at the same place (the headline, while a
    /// caption cue replaces the caption) holds at full alpha through the
    /// ease; only what changes cross-fades.
    #[test]
    fn a_line_both_cards_draw_stays_through_the_ease() {
        let a = lay(&CardContent { caption: Some("Own caption".into()), ..headline("The bay") });
        let b = lay(&CardContent { caption: Some("A cue from the caption track, longer".into()), ..headline("The bay") });
        let mid = ease_cards(&a, &b, 0.5);
        let n = a.runs.len();
        let alpha_of = |text: &str| -> Vec<f32> { mid.runs.iter().zip(&mid.run_alpha).filter(|(r, _)| r.text == text).map(|(_, k)| *k).collect() };
        assert_eq!(alpha_of("The bay"), vec![1.0, 0.0], "the headline: the old run kept, the new one not drawn twice");
        assert!(mid.run_alpha[..n].iter().zip(&a.runs).filter(|(_, r)| r.text != "The bay").all(|(k, _)| *k < 1e-6), "the old caption fades out");
    }

    #[test]
    fn the_card_shadow_is_soft_and_offset_down() {
        let card = (32.0, 1200.0, 600.0, 1430.0);
        let layers = card_shadow(card, 24.0);
        assert!(layers.len() >= 10);
        let coverage = |x: f32, y: f32| 1.0 - layers.iter().filter(|(r, ..)| x >= r.0 && x <= r.2 && y >= r.1 && y <= r.3).map(|(_, _, a)| 1.0 - a).product::<f32>();
        let under = coverage(300.0, 1430.0 + 10.0);
        assert!(under > 0.06 && under < 0.2, "about half the .14 peak (plus the contact layer) at the offset edge: {under}");
        assert!(coverage(300.0, 1430.0 + 50.0) < 0.01, "gone well past the blur");
        assert!(coverage(300.0, 1200.0 - 20.0) < coverage(300.0, 1430.0 + 20.0), "offset downward");
        assert!(layers.iter().all(|(r, rad, a)| r.2 > r.0 && r.3 > r.1 && *rad >= 0.0 && *a > 0.0 && *a < 0.1));
    }

    #[test]
    fn the_fade_is_the_spec_table() {
        for (s, want) in [(0.0, 0.0), (0.25, 0.15625), (0.5, 0.5), (0.75, 0.84375), (1.0, 1.0)] {
            assert!((fade_alpha((0.0, 100.0), s * 100.0) - want).abs() < 1e-6);
        }
        assert_eq!(fade_alpha((0.0, 100.0), 150.0), 1.0, "held below y1");
    }

    fn lay(c: &CardContent) -> CardLayout {
        layout_card(PORTRAIT, SafeArea::Reels, Aspect::Portrait, Some(c), &[], None, &Half)
    }

    fn quote(text: &str, by: Option<&str>, source: Option<&str>) -> Quote {
        Quote { text: text.into(), by: by.map(Into::into), source: source.map(Into::into) }
    }

    fn photo() -> Photo {
        Photo { src: "img/barrage.jpg".into(), alt: "The barrage".into(), credit: None }
    }

    fn legend(n: usize) -> CardItem {
        CardItem::Legend { rows: (0..n).map(|i| LegendRow { color: [i as u8, 0, 0], label: format!("c{i}"), value: None }).collect(), title: None }
    }

    fn stats(n: usize) -> CardItem {
        CardItem::Stats((0..n).map(|i| Stat { value: format!("{i}"), unit: None, label: format!("l{i}") }).collect())
    }

    #[test]
    fn a_quote_wins_over_a_headline_and_warns() {
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"headline": "Ignored", "quote": {"text": "We built it to last", "by": "Lee", "source": "1987"}})).unwrap();
        let c = CardContent::from_spec(&spec);
        assert_eq!(c.items, vec![CardItem::Attribution], "the quote's by/source make the attribution item");
        let l = lay(&c);
        assert!(l.warnings.contains(&"headline_ignored"), "{:?}", l.warnings);
        assert!(l.runs.iter().all(|r| r.text != "Ignored"), "the headline is not drawn");
        let q: Vec<&TextRun> = l.runs.iter().filter(|r| r.size == QUOTE.size).collect();
        assert_eq!(q.iter().map(|r| r.text.as_str()).collect::<String>(), "\u{201C}We built it to last\u{201D}");
        assert_eq!((q[0].ink, q[2].ink), (Ink::Accent, Ink::Accent), "the marks in the accent");
        assert_eq!(q[1].ink, Ink::Ink);
        // One line, in order, a tracking step apart (R12): the measured
        // widths end at the ink, the quote's -0.012 em sits between runs.
        let step = QUOTE.style().letter_spacing_em() * QUOTE.size;
        assert!((q[1].x - (q[0].x + q[0].width + step)).abs() < 1e-3 && (q[2].x - (q[1].x + q[1].width + step)).abs() < 1e-3, "on one line, in order");
        assert!(q.iter().all(|r| r.weight == QUOTE.weight));
        // The attribution: `by` in 600 ink, ", source" in 400 ink-2, on 34.
        let by = l.runs.iter().find(|r| r.text == "Lee").expect("the attribution");
        let rest = l.runs.iter().find(|r| r.text == ", 1987").expect("its source");
        assert_eq!((by.weight, by.ink, by.line_height), (600, Ink::Ink, 34.0));
        assert_eq!((rest.weight, rest.ink, rest.x), (400, Ink::Ink2, by.x + by.width));
        assert_eq!(by.top, q[0].top + QUOTE.line_height + ITEM_GAP, "24 below the quote");
        // A quote over several lines opens on the first and closes on the last.
        let long = CardContent { quote: Some(quote(&"word ".repeat(12), None, None)), ..CardContent::default() };
        let l = lay(&long);
        let marks: Vec<&TextRun> = l.runs.iter().filter(|r| r.ink == Ink::Accent).collect();
        assert_eq!(marks.len(), 2);
        assert!(marks[0].top < marks[1].top && marks[0].text == QUOTE_OPEN && marks[1].text == QUOTE_CLOSE);
    }

    #[test]
    fn the_photo_sits_beside_row_one_and_is_the_item() {
        let c = CardContent { headline: Some("Morning run".into()), photo: Some(photo()), ..CardContent::default() };
        let l = lay(&c);
        let card = l.card.expect("a card");
        let (rect, src) = l.photo.clone().expect("the photo");
        assert_eq!(src, "img/barrage.jpg");
        assert_eq!((rect.0, rect.2 - rect.0, rect.3 - rect.1), (64.0, 208.0, 208.0), "208 x 208 on the keyline");
        assert_eq!(rect.1, card.1 + 30.0, "taller than the text: it sets the row");
        let h = &l.runs[0];
        assert_eq!(h.x, 64.0 + 208.0 + 28.0, "the text 28 right of the photo");
        assert!(((h.top + h.line_height / 2.0) - (rect.1 + rect.3) / 2.0).abs() < 1e-3, "centred against the photo");
        assert_eq!(card.2 - card.0, 64.0 + 236.0 + 11.0 * 30.0, "the photo, its gap and the text");
        assert_eq!(card.3 - card.1, 30.0 + 208.0 + 32.0);
        // Stats outrank the photo: the photo is not drawn and warns.
        let l = lay(&CardContent { items: vec![stats(1)], ..c.clone() });
        assert!(l.photo.is_none());
        assert_eq!(l.warnings.iter().filter(|w| **w == "extra_supporting_item").count(), 1);
        assert_eq!(l.runs[0].x, 64.0, "no photo, no indent");
        // Too tall for a small frame: the photo is the item, so it goes first.
        let body = measure_card(&c, 900.0, 200.0, &Half);
        assert!(body.photo.is_none() && body.warnings.contains(&"card_too_tall"), "{:?}", body.warnings);
        assert_eq!(body.runs[0].x, 32.0);
    }

    #[test]
    fn the_counter_ends_row_one_28_from_the_text() {
        let c = CardContent { headline: Some("Port Klang".into()), body: Some("A long body line that is wider than the headline".into()), index: Some((3, 5)), ..CardContent::default() };
        let l = lay(&c);
        let card = l.card.expect("a card");
        assert_eq!(l.counter.len(), 2);
        let (i, n) = (&l.counter[0], &l.counter[1]);
        assert_eq!((i.text.as_str(), i.weight, i.ink), ("3", 700, Ink::Accent));
        assert_eq!((n.text.as_str(), n.weight, n.ink), (" / 5", 600, Ink::Ink3));
        // One tracking step (+0.02 em, R12) between the numeral's ink and ` / n`.
        assert!((n.x - (i.x + i.width + COUNTER.style().letter_spacing_em() * COUNTER.size)).abs() < 1e-3, "{i:?} {n:?}");
        assert!((n.x + n.width - (card.2 - 32.0)).abs() < 1e-3, "at the row's end");
        let head = &l.runs[0];
        assert!(i.x >= head.x + head.width + 28.0 - 1e-3, "28 from the text: {i:?} {head:?}");
        let base = |r: &TextRun| r.top + baseline_offset(r.size, r.line_height);
        assert!((base(i) - base(head)).abs() < 1e-3, "on row 1's first baseline");
        assert!((base(n) - base(head)).abs() < 1e-3);
        // The text width gives the counter its room (spec §5.4).
        for r in &l.runs {
            assert!(r.x + r.width <= 32.0 + 900.0 - 32.0 - (n.x + n.width - i.x) - 28.0 + 1e-3, "{r:?}");
        }
        // A short headline alone: the card is the text, 28, the counter
        // (its two runs and the tracking step between them).
        let l = lay(&CardContent { headline: Some("Port Klang".into()), index: Some((3, 5)), ..CardContent::default() });
        let card = l.card.unwrap();
        let step = COUNTER.style().letter_spacing_em() * COUNTER.size;
        assert!((card.2 - card.0 - (64.0 + 300.0 + 28.0 + 13.0 + step + 52.0)).abs() < 1e-3, "{card:?}");
        assert!(lay(&CardContent { compact: true, ..c }).counter.is_empty(), "no counter on a compact card");
    }

    #[test]
    fn one_supporting_item_wins_in_order_legend_stats_photo_attribution() {
        let all = CardContent { quote: Some(quote("Q", Some("Lee"), None)), photo: Some(photo()), items: vec![stats(1), CardItem::Attribution, legend(2)], ..CardContent::default() };
        let l = lay(&all);
        assert_eq!(l.swatches.len(), 2, "the legend is drawn");
        assert!(l.photo.is_none() && l.runs.iter().all(|r| r.text != "Lee" && !(r.size == STAT_L.size && r.weight == STAT_L.weight)));
        assert_eq!(l.warnings.iter().filter(|w| **w == "extra_supporting_item").count(), 3, "{:?}", l.warnings);
        let without_legend = CardContent { items: vec![stats(1), CardItem::Attribution], ..all.clone() };
        let l = lay(&without_legend);
        assert!(l.runs.iter().any(|r| r.size == STAT_L.size && r.weight == STAT_L.weight) && l.photo.is_none());
        assert_eq!(l.warnings.iter().filter(|w| **w == "extra_supporting_item").count(), 2);
        let photo_or_quote = CardContent { items: vec![CardItem::Attribution], ..all.clone() };
        let l = lay(&photo_or_quote);
        assert!(l.photo.is_some() && l.runs.iter().all(|r| r.text != "Lee"));
        let l = lay(&CardContent { photo: None, ..photo_or_quote });
        assert!(l.runs.iter().any(|r| r.text == "Lee") && !l.warnings.contains(&"extra_supporting_item"));
        // From a script, a legend and stats on one card: the legend.
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"headline": "H", "stats": [{"label": "a", "value": "1"}],
            "legend": {"items": [{"color": "#e8590c", "label": "Ferry", "value": "12"}], "title": "Vessels"}})).unwrap();
        let l = lay(&CardContent::from_spec(&spec));
        assert_eq!(l.swatches, vec![((64.0, l.runs[0].top + 66.0 + 24.0 + 10.0, 80.0, l.runs[0].top + 66.0 + 24.0 + 26.0), [0xE8, 0x59, 0x0C])]);
        let value = l.runs.iter().find(|r| r.text == "12").expect("the legend value");
        assert_eq!((value.weight, value.ink), (700, Ink::Ink));
        let label = l.runs.iter().find(|r| r.text == "Ferry").unwrap();
        assert!((value.x + value.width - (label.x - 28.0 + 28.0 + 5.0 * 13.0 + 16.0 + 26.0)).abs() < 1e-3, "pushed to the column's end");
        let title = l.runs.iter().find(|r| r.text == "Vessels").expect("the title");
        assert_eq!((title.top, title.ink), (label.top - 2.0 + 36.0 + 4.0, Ink::Ink3), "4 below the grid");
    }

    #[test]
    fn the_headline_truncates_with_an_ellipsis_only_after_the_last_step() {
        // 826 units of text width; at 54 a glyph is 27 wide: 30 per line.
        // Three lines at 54 fit: shrunk, never cut.
        let three = "abcdefghi ".repeat(8);
        let l = lay(&headline(three.trim()));
        let h: Vec<&TextRun> = l.runs.iter().filter(|r| r.weight == 700).collect();
        assert_eq!((h.len(), h[0].size), (3, 54.0), "{h:?}");
        assert!(h.iter().all(|r| !r.text.ends_with('…')));
        assert!(l.warnings.contains(&"text_shrunk") && !l.warnings.contains(&"text_truncated"), "{:?}", l.warnings);
        assert!((h[1].top - h[0].top - 54.0 * 66.0 / 60.0).abs() < 1e-3, "each step at its own line height");
        // Past the last step (48 x 3): cut at 48 with an ellipsis.
        let l = lay(&headline(&"abcdefghi ".repeat(30)));
        let h: Vec<&TextRun> = l.runs.iter().filter(|r| r.weight == 700).collect();
        assert_eq!((h.len(), h[0].size), (3, 48.0));
        assert!(h[2].text.ends_with('…') && h[..2].iter().all(|r| !r.text.ends_with('…')));
        assert!(l.warnings.contains(&"text_truncated"));
        for r in &h {
            assert!(r.x + r.width <= 32.0 + 900.0 - 32.0 + 1e-3, "{r:?}");
        }
        // A body past its last step (36 x 2) is cut the same way.
        let l = lay(&CardContent { body: Some("abcdefghi ".repeat(30)), ..CardContent::default() });
        assert!(l.runs.last().unwrap().text.ends_with('…') && l.runs.iter().all(|r| r.size == 36.0));
    }

    #[test]
    fn balanced_wrap_never_adds_a_line_or_widens_the_card() {
        let text_w = 900.0 - 64.0;
        for text in ["Barrage closes the Marina channel for good, and the bay becomes a reservoir", "A B C D E F G H I J K L M N O P Q R S T U V W X Y Z a b c d e f", &"word ".repeat(9)] {
            for max_w in [400.0_f32, 600.0, 900.0] {
                let body = measure_card(&headline(text), max_w, 1e9, &Half);
                if body.warnings.contains(&"text_truncated") {
                    continue;
                }
                let h: Vec<&TextRun> = body.runs.iter().filter(|r| r.weight == 700).collect();
                let greedy = wrap(&Half, text, h[0].size, TextStyle::of_weight(700), (max_w - 64.0).min(text_w));
                let greedy_w = greedy.iter().map(|l| Half.width(l, h[0].size, TextStyle::of_weight(700))).fold(0.0_f32, f32::max);
                assert_eq!(h.len(), greedy.len(), "{text:?} at {max_w}: the same line count");
                assert!(body.w - 64.0 <= greedy_w + 1e-3, "{text:?} at {max_w}: {} > {greedy_w}", body.w - 64.0);
            }
        }
        // Spec §5.5 says "keeps the same line count", and shaping is not
        // additive: under a measure where "a b" is 800 wide but "b c d"
        // only 10, the greedy wrap is 3 lines and narrower widths give 2.
        // The balanced wrap must stay at 3 (the 54 x 3 step, the three-line
        // rule's trigger), never fall to 2.
        struct Shaped;
        impl CardMeasure for Shaped {
            fn width(&self, text: &str, _size: f32, _style: TextStyle) -> f32 {
                match text {
                    "a b" => 800.0,
                    "a b c" | "c d" => 2000.0,
                    _ => 10.0,
                }
            }
        }
        let body = measure_card(&CardContent { headline: Some("a b c d".into()), items: vec![stats(1)], ..CardContent::default() }, 900.0, 1e9, &Shaped);
        let h: Vec<&str> = body.runs.iter().filter(|r| r.weight == 700).map(|r| r.text.as_str()).collect();
        assert_eq!(h, ["a b", "c", "d"], "the greedy three lines stand");
        assert!(body.warnings.contains(&"supporting_item_deferred"), "{:?}", body.warnings);
        // And it does balance: greedy is 25 + 16 glyphs, balanced 18 + 23,
        // the narrowest two-line split.
        let body = measure_card(&headline("Barrage closes the Marina channel for good"), 900.0, 1e9, &Half);
        let widths: Vec<f32> = body.runs.iter().map(|r| r.width).collect();
        assert_eq!(widths, [18.0 * 30.0, 23.0 * 30.0]);
        assert_eq!(body.w, 64.0 + 23.0 * 30.0, "the card hugs the balanced lines");
    }

    #[test]
    fn a_sequential_legend_places_first_last_and_middle_ticks() {
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"headline": "Haze",
            "legend": {"type": "sequential", "stops": ["#5C1A1B", "#FFE066"], "ticks": ["1 Sep", "8 Sep", "15 Sep", "28 Sep"], "current": 1, "title": "Days"}})).unwrap();
        let l = lay(&CardContent::from_spec(&spec));
        let (ramp, stops) = l.ramp.clone().expect("a ramp");
        assert_eq!(stops, vec![[0x5C, 0x1A, 0x1B], [0xFF, 0xE0, 0x66]]);
        assert_eq!((ramp.0, ramp.2 - ramp.0, ramp.3 - ramp.1), (64.0, 560.0, 12.0), "min(560, text width), 12 high");
        assert_eq!(ramp.1, l.runs[0].top + 66.0 + 24.0);
        let ticks: Vec<&TextRun> = l.runs.iter().filter(|r| r.text.ends_with("Sep")).collect();
        assert_eq!(ticks.len(), 4);
        assert_eq!(ticks[0].x, ramp.0, "the first left-aligned");
        assert!((ticks[3].x + ticks[3].width - ramp.2).abs() < 1e-3, "the last right-aligned");
        for (k, t) in ticks.iter().enumerate().skip(1).take(2) {
            assert!((t.x + t.width / 2.0 - (ramp.0 + 560.0 * k as f32 / 3.0)).abs() < 1e-3, "tick {k} centred on k / (n - 1)");
        }
        assert!(ticks.iter().all(|t| t.top == ramp.3 + 10.0 && t.line_height == 34.0));
        assert_eq!(ticks.iter().map(|t| t.ink).collect::<Vec<_>>(), [Ink::Ink3, Ink::Ink, Ink::Ink3, Ink::Ink3], "the current tick in ink");
        let title = l.runs.iter().find(|r| r.text == "Days").unwrap();
        assert_eq!((title.top, title.ink), (ramp.3 + 10.0 + 34.0 + 4.0, Ink::Ink3));
        let card = l.card.unwrap();
        assert_eq!(card.3, title.top + 32.0 + 32.0, "the card ends 32 under the title");
        assert_eq!(card.2 - card.0, 64.0 + 560.0, "the ramp sets the card's width");
        // A narrow card: the ramp is the text width.
        let body = measure_card(&CardContent::from_spec(&spec), 400.0, 1e9, &Half);
        assert_eq!(body.ramp.unwrap().0 .2 - 32.0, 400.0 - 64.0);
    }

    #[test]
    fn a_third_stat_warns_and_a_crowded_legend_shrinks() {
        let l = lay(&CardContent { headline: Some("H".into()), items: vec![stats(3)], ..CardContent::default() });
        assert!(l.warnings.contains(&"too_many_stats"));
        assert_eq!(l.runs.iter().filter(|r| r.size == STAT_L.size && r.weight == STAT_L.weight).count(), 2, "two stats drawn");
        let columns = |l: &CardLayout| l.swatches.iter().map(|(r, _)| r.0.to_bits()).collect::<std::collections::BTreeSet<_>>().len();
        let four = lay(&CardContent { items: vec![legend(4)], ..CardContent::default() });
        assert_eq!((columns(&four), four.warnings.len()), (2, 0), "{:?}", four.warnings);
        let five = lay(&CardContent { items: vec![legend(5)], ..CardContent::default() });
        assert_eq!(columns(&five), 3);
        assert_eq!(five.warnings, ["legend_crowded"]);
        let seven = lay(&CardContent { items: vec![legend(7)], ..CardContent::default() });
        assert_eq!(seven.swatches.len(), 6, "5 and Other");
        assert_eq!(seven.swatches[5].1, OTHER);
        assert_eq!(seven.runs.last().unwrap().text, "Other");
        assert!(seven.warnings.contains(&"legend_too_many_items") && seven.warnings.contains(&"legend_crowded"), "{:?}", seven.warnings);
        assert_eq!(columns(&seven), 3);
        // A grid wider than the card: the labels are cut, the grid fits.
        let wide = CardItem::Legend { rows: (0..6).map(|i| LegendRow { color: [0; 3], label: format!("{i} {}", "long label ".repeat(3)), value: Some("100".into()) }).collect(), title: None };
        let l = lay(&CardContent { items: vec![wide], ..CardContent::default() });
        let card = l.card.unwrap();
        assert!(l.runs.iter().all(|r| r.x + r.width <= card.2 - 32.0 + 1e-3), "inside the card");
        assert!(l.runs.iter().any(|r| r.text.ends_with('…')));
    }

    #[test]
    fn title_cards_use_title_steps() {
        let c = CardContent { headline: Some("Marina Bay".into()), title: true, ..CardContent::default() };
        let l = lay(&c);
        assert_eq!((l.runs[0].size, l.runs[0].line_height, l.runs[0].weight), (72.0, 78.0, 700));
        // 70 glyphs: two lines at 72 do not fit (23 per line), at 64 (25) neither; 60 x 3 does.
        let l = lay(&CardContent { headline: Some("abcdefghi ".repeat(7)), title: true, ..CardContent::default() });
        assert!(l.runs.iter().all(|r| r.size == 60.0) && l.runs.len() == 3, "{:?}", l.runs);
    }

    #[test]
    fn the_moving_camera_rule_keeps_only_the_caption() {
        let c = CardContent { headline: Some("Head".into()), body: Some("Body".into()), items: vec![stats(1)], index: Some((1, 3)), photo: Some(photo()), compact: true, ..CardContent::default() };
        let l = lay(&c);
        assert_eq!(l.runs.len(), 1);
        assert_eq!((l.runs[0].text.as_str(), l.runs[0].size, l.runs[0].weight), ("Head", 44.0, 600), "the headline set in the caption style");
        assert!(l.photo.is_none() && l.counter.is_empty() && l.swatches.is_empty() && l.warnings.is_empty());
        let l = lay(&CardContent { caption: Some("Leaving port".into()), ..c });
        assert_eq!(l.runs[0].text, "Leaving port");
    }

    /// Ruling P13: one height cap, shared with R3's small frames. The
    /// item goes first, then the last row, never below row 1; finite.
    #[test]
    fn the_height_cap_drops_the_item_then_rows() {
        let c = CardContent { headline: Some("Port Klang".into()), body: Some("The body".into()), items: vec![stats(2)], ..CardContent::default() };
        let full = measure_card(&c, 900.0, 1e9, &Half);
        assert_eq!(full.h, 30.0 + 66.0 + 10.0 + 52.0 + 24.0 + 100.0 + 32.0);
        let no_item = measure_card(&c, 900.0, full.h - 1.0, &Half);
        assert_eq!(no_item.warnings, ["card_too_tall"]);
        assert_eq!(no_item.h, 30.0 + 66.0 + 10.0 + 52.0 + 32.0);
        let one_row = measure_card(&c, 900.0, 10.0, &Half);
        assert_eq!(one_row.warnings, ["card_too_tall", "text_truncated"]);
        assert_eq!((one_row.runs.len(), one_row.h), (1, 30.0 + 66.0 + 32.0), "row 1 stays");
        assert!(one_row.w.is_finite() && one_row.w > 0.0);
    }

    #[test]
    fn an_ease_hands_the_photo_ramp_and_counter_over_at_the_midpoint() {
        let a = lay(&CardContent { headline: Some("A".into()), photo: Some(photo()), index: Some((1, 2)), ..CardContent::default() });
        let spec: CardSpec = serde_json::from_value(serde_json::json!({"headline": "B", "index": {"i": 2, "n": 2}, "legend": {"type": "sequential", "stops": ["#000", "#fff"]}})).unwrap();
        let b = lay(&CardContent::from_spec(&spec));
        let early = ease_cards(&a, &b, 0.25);
        assert!(early.photo.is_some() && early.ramp.is_none() && early.counter[0].text == "1");
        assert!(early.extras_alpha.unwrap() > 0.0 && early.extras_alpha.unwrap() < 1.0);
        let late = ease_cards(&a, &b, 0.75);
        assert!(late.photo.is_none() && late.ramp.is_some() && late.counter[0].text == "2");
        let top = late.card.unwrap().1;
        assert!(late.ramp.unwrap().0 .1 > top && late.counter.iter().all(|r| r.top >= top));
        assert_eq!(ease_cards(&a, &b, 0.5).extras_alpha, Some(0.0), "nothing of either at the swap");
        assert_eq!(b.extras_alpha, None, "settled: drawn at 1");
    }
}
