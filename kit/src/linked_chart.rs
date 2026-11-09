//! The Studio's one linked chart: a histogram of a numeric field, or a
//! horizontal bar chart by category, tied to a map both ways.
//!
//! - **Chart -> map, select.** Brush a range on the histogram (or click a
//!   bin), or click bars: the map shows only what is selected. How depends
//!   on the source ([`Link`]): a point source's GPU range filter (one
//!   uniform write, nothing re-pushed), or a page's own callback (a
//!   re-declared paint, a re-push, a dim).
//! - **Chart -> map, hover.** Hover a bin or a bar: every map feature in
//!   it lights at once (`MapView::set_highlight_where`).
//! - **Map -> chart.** Hover a map object (through the shared popup's
//!   pick, [`LinkedChart::follow_popup`]): its bin or bar lights.
//!
//! Studio-side only: the library answers "which features are in this bar"
//! and "show these"; the bars, their words and their layout are the host's.
//! Generalises the satellites page's cross-filter charts and the census
//! income histogram; Kepler's time histogram is `time_filter.rs`.
//!
//! The maths (binning, scales, brush to range, category totals) is plain
//! functions at the top, unit-tested; the component is below.
//!
//! Screenshot probes (dispatched through the window's own event path, no
//! OS pointer moved, bounded at 40 s):
//! - `GPUI_MAP_STUDIO_PROBE_BRUSH=a,b`: a drag from fraction `a` to `b` of
//!   the histogram's width; on a bar chart, clicks bars `a` (and `b`).
//! - `GPUI_MAP_STUDIO_PROBE_CHART_HOVER=f`: the pointer over fraction `f`
//!   of the histogram's width, or over bar `f` (an index).
//! - `GPUI_MAP_STUDIO_PROBE_BRUSH_DRAG=from,to`: after the brush probe, a
//!   press at fraction `from` dragged to `to` -- inside the window it
//!   moves, on an edge it resizes ([`brush`]).
#![allow(dead_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, div, point, px, relative, rgba, AnyElement, App, AppContext as _, Context, DispatchPhase, Entity, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window,
};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::{HighlightStyle, HighlightWhere, TagValue};

use crate::popup::{number, NumFmt};

/// The shared brush state machine (move inside, resize at an edge, draw
/// outside): `brush.rs`, declared once by each binary.
use crate::brush;

// ---------------------------------------------------------------------------
// The maths
// ---------------------------------------------------------------------------

/// How values map onto the histogram's x axis.
// The quantile edges are inline so the axis stays `Copy`: one axis per
// chart, a few hundred bytes, never in a hot collection.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scale {
    Linear,
    /// Log base 10: populations, incomes -- data that spans decades. The
    /// domain must be positive.
    Log10,
    /// Quantile bins (`bins_scale: "quantile"`): every bin the same width
    /// on the axis, each holding about as many values; linear inside one.
    Quantile(QuantileEdges),
}

/// The edges of quantile bins (or of any uneven bins, such as calendar
/// months), at most [`MAX_EDGE_BINS`] bins: a fixed array so the axis
/// stays `Copy`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuantileEdges {
    n: usize,
    e: [f64; MAX_EDGE_BINS + 1],
}

/// The most quantile bins an axis holds.
pub const MAX_QUANTILE_BINS: usize = 80;
/// The most bins an axis of given edges holds ([`Axis::edges`]): a time
/// series' buckets, e.g. 140 months.
pub const MAX_EDGE_BINS: usize = 160;

impl QuantileEdges {
    /// About `bins` quantile bins of `values` (repeated edges merged, the
    /// web viewer's `quantileEdges`); `None` when the data cannot take them
    /// (under two distinct values, or under two bins after merging).
    pub fn of(values: &[f64], bins: usize) -> Option<Self> {
        let mut xs: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
        xs.sort_by(f64::total_cmp);
        let (first, last) = (*xs.first()?, *xs.last()?);
        if xs.len() < 2 || first == last {
            return None;
        }
        let bins = bins.clamp(2, MAX_QUANTILE_BINS);
        let mut out = vec![first];
        for j in 1..bins {
            let q = xs[((j * xs.len()) as f64 / bins as f64).round().min((xs.len() - 1) as f64) as usize];
            if q > *out.last()? {
                out.push(q);
            }
        }
        if last > *out.last()? {
            out.push(last);
        }
        if out.len() < 3 {
            return None;
        }
        Self::from_edges(&out)
    }

    /// Bins between `edges` as given (increasing, two to
    /// [`MAX_EDGE_BINS`] + 1 of them); `None` otherwise.
    pub fn from_edges(edges: &[f64]) -> Option<Self> {
        if edges.len() < 2 || edges.len() > MAX_EDGE_BINS + 1 || edges.windows(2).any(|w| !(w[1] > w[0])) {
            return None;
        }
        let mut e = [0.0; MAX_EDGE_BINS + 1];
        e[..edges.len()].copy_from_slice(edges);
        Some(Self { n: edges.len() - 1, e })
    }

    /// How many bins.
    pub fn bins(&self) -> usize {
        self.n
    }

    /// The edges, `bins() + 1` of them.
    pub fn edges(&self) -> &[f64] {
        &self.e[..=self.n]
    }

    /// `v` in bin units: 0 at the first edge, `bins()` at the last.
    fn index(&self, v: f64) -> f64 {
        let e = self.edges();
        if v <= e[0] {
            return 0.0;
        }
        if v >= e[self.n] {
            return self.n as f64;
        }
        let b = e.partition_point(|x| *x <= v).saturating_sub(1).min(self.n - 1);
        b as f64 + (v - e[b]) / (e[b + 1] - e[b]).max(f64::MIN_POSITIVE)
    }

    fn value(&self, u: f64) -> f64 {
        let c = u.clamp(0.0, self.n as f64);
        let b = (c.floor() as usize).min(self.n - 1);
        self.e[b] + (c - b as f64) * (self.e[b + 1] - self.e[b])
    }
}

/// A histogram's x axis: a domain and a scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Axis {
    pub domain: [f64; 2],
    pub scale: Scale,
}

impl Axis {
    pub fn linear(domain: [f64; 2]) -> Self {
        Self { domain, scale: Scale::Linear }
    }

    pub fn log10(domain: [f64; 2]) -> Self {
        Self { domain: [domain[0].max(f64::MIN_POSITIVE), domain[1].max(f64::MIN_POSITIVE)], scale: Scale::Log10 }
    }

    /// Quantile bins of `values` (see [`QuantileEdges::of`]).
    pub fn quantile(values: &[f64], bins: usize) -> Option<Self> {
        let q = QuantileEdges::of(values, bins)?;
        let e = q.edges();
        Some(Self { domain: [e[0], e[e.len() - 1]], scale: Scale::Quantile(q) })
    }

    /// Bins between `edges` as given, each the same width on the axis (a
    /// time series' calendar buckets: every month one bar).
    pub fn edges(edges: &[f64]) -> Option<Self> {
        let q = QuantileEdges::from_edges(edges)?;
        Some(Self { domain: [edges[0], edges[edges.len() - 1]], scale: Scale::Quantile(q) })
    }

    /// Log bins when the data can take them (every value above 0, a span),
    /// else `None`.
    pub fn log_of(domain: [f64; 2]) -> Option<Self> {
        (domain[0] > 0.0 && domain[1] > domain[0]).then(|| Self::log10(domain))
    }

    fn map(&self, v: f64) -> f64 {
        match &self.scale {
            Scale::Linear => v,
            Scale::Log10 => v.max(f64::MIN_POSITIVE).log10(),
            Scale::Quantile(q) => q.index(v),
        }
    }

    fn unmap(&self, u: f64) -> f64 {
        match &self.scale {
            Scale::Linear => u,
            Scale::Log10 => 10f64.powf(u),
            Scale::Quantile(q) => q.value(u),
        }
    }

    /// Where `v` sits along the axis, `0..=1` inside the domain.
    pub fn fraction(&self, v: f64) -> f64 {
        let (a, b) = (self.map(self.domain[0]), self.map(self.domain[1]));
        if !(b > a) {
            return 0.0;
        }
        (self.map(v) - a) / (b - a)
    }

    /// The value at fraction `t` of the axis.
    pub fn value(&self, t: f64) -> f64 {
        let (a, b) = (self.map(self.domain[0]), self.map(self.domain[1]));
        self.unmap(a + t * (b - a))
    }
}

/// The bin of `v` among `bins` equal bins of `axis` (equal in the axis's
/// scale); `None` outside the domain or for NaN. The domain's top edge is
/// in the last bin.
pub fn bin_of(axis: &Axis, bins: usize, v: f64) -> Option<usize> {
    if bins == 0 || !v.is_finite() || v < axis.domain[0] || v > axis.domain[1] {
        return None;
    }
    let t = axis.fraction(v);
    Some(((t * bins as f64).floor().max(0.0) as usize).min(bins - 1))
}

/// Bin `i`'s closed `[lo, hi]`.
pub fn bin_range(axis: &Axis, bins: usize, i: usize) -> [f64; 2] {
    let n = bins.max(1) as f64;
    [axis.value(i as f64 / n), axis.value((i + 1) as f64 / n)]
}

/// Per-bin totals: a count of `values`, or the sum of `weights`.
pub fn histogram(values: &[f64], weights: Option<&[f64]>, axis: &Axis, bins: usize) -> Vec<f64> {
    let mut out = vec![0.0; bins.max(1)];
    for (i, &v) in values.iter().enumerate() {
        if let Some(b) = bin_of(axis, bins, v) {
            out[b] += weights.and_then(|w| w.get(i)).copied().unwrap_or(1.0);
        }
    }
    out
}

/// A brush from fraction `a` to `b` of the chart's width as a value range,
/// snapped OUT to whole bins (a brush that touches a bin takes all of it,
/// so the selection is exactly the bars drawn lit). Either order.
pub fn brush_to_range(axis: &Axis, bins: usize, a: f64, b: f64) -> [f64; 2] {
    let n = bins.max(1) as f64;
    let (lo, hi) = (a.min(b).clamp(0.0, 1.0), a.max(b).clamp(0.0, 1.0));
    let first = (lo * n).floor().min(n - 1.0);
    let last = ((hi * n).ceil()).max(first + 1.0).min(n);
    [axis.value(first / n), axis.value(last / n)]
}

/// A moved or resized window `a..b` (fractions) snapped to the NEAREST bin
/// edges, at least one bin wide: a dragged window keeps its width in bins
/// (snapping out would grow it by one on every move).
pub fn snap_nearest(axis: &Axis, bins: usize, a: f64, b: f64) -> [f64; 2] {
    let n = bins.max(1) as f64;
    let mut lo = (a.min(b).clamp(0.0, 1.0) * n).round();
    let mut hi = (a.max(b).clamp(0.0, 1.0) * n).round();
    if hi <= lo {
        hi = lo + 1.0;
    }
    if hi > n {
        (lo, hi) = (n - 1.0, n);
    }
    [axis.value(lo / n), axis.value(hi / n)]
}

/// The bins a range covers, `first..=last` -- what draws lit.
pub fn range_bins(axis: &Axis, bins: usize, range: [f64; 2]) -> (usize, usize) {
    let n = bins.max(1);
    let f = |v: f64| (axis.fraction(v) * n as f64).clamp(0.0, n as f64);
    let first = (f(range[0]) + 1e-9).floor() as usize;
    let last = ((f(range[1]) - 1e-9).ceil() as usize).max(first + 1);
    (first.min(n - 1), last.min(n) - 1)
}

/// Axis ticks: the domain's ends and a few round values between, as
/// fractions of the axis. Powers of ten on a log axis.
pub fn ticks(axis: &Axis, want: usize) -> Vec<(f64, f64)> {
    let [lo, hi] = axis.domain;
    let mut out = Vec::new();
    match axis.scale {
        Scale::Log10 => {
            // The powers of ten (1k, 10k, 100k, 1M); 1-2-5 steps when
            // fewer than two fall inside (the web viewer's `logTicks`).
            let mut p = 10f64.powf((lo.log10() - 1e-9).ceil());
            while p <= hi * (1.0 + 1e-9) {
                out.push((axis.fraction(p), p));
                p *= 10.0;
            }
            if out.len() < 2 {
                out.clear();
                let mut e = lo.log10().floor() as i32;
                while 10f64.powi(e) <= hi {
                    for m in [1.0, 2.0, 5.0] {
                        let t = m * 10f64.powi(e);
                        if t >= lo && t <= hi {
                            out.push((axis.fraction(t), t));
                        }
                    }
                    e += 1;
                }
            }
        }
        Scale::Quantile(q) => {
            // A few edges, evenly along the axis (the bins are equal there).
            let n = q.bins();
            let mut last = usize::MAX;
            for j in 0..=want.max(1) {
                let i = ((j * n) as f64 / want.max(1) as f64).round() as usize;
                if i != last {
                    out.push((axis.fraction(q.edges()[i]), q.edges()[i]));
                    last = i;
                }
            }
        }
        Scale::Linear => {
            let span = hi - lo;
            if span > 0.0 {
                let raw = span / want.max(1) as f64;
                let mag = 10f64.powf(raw.log10().floor());
                // The round step whose tick count is nearest the want.
                let count = |s: f64| (span / s - want as f64).abs();
                let step = [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).min_by(|a, b| count(*a).total_cmp(&count(*b))).unwrap_or(mag);
                let mut v = (lo / step).ceil() * step;
                while v <= hi + step * 1e-9 {
                    out.push((axis.fraction(v), v));
                    v += step;
                }
            }
        }
    }
    out
}

/// A tick's text: 1k, 10k, 100k, 1M, 2.5M, 500 (the web viewer's
/// `tickText`).
pub fn tick_text(x: f64) -> String {
    let a = x.abs();
    let t = |v: f64| {
        let s = format!("{v:.1}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    if a >= 1e12 {
        format!("{}T", t(x / 1e12))
    } else if a >= 1e9 {
        format!("{}B", t(x / 1e9))
    } else if a >= 1e6 {
        format!("{}M", t(x / 1e6))
    } else if a >= 1e3 {
        format!("{}k", t(x / 1e3))
    } else if a >= 100.0 || x == x.round() {
        format!("{}", x.round())
    } else {
        format!("{}", (x * 100.0).round() / 100.0)
    }
}

/// `text` (a formatted number) with `unit` written the way it is: "12%",
/// "21°C", "¥1,200", "12.4k passengers/day" (the web viewer's `attach`).
pub fn attach_unit(text: &str, unit: &str) -> String {
    const TIGHT: [&str; 6] = ["%", "\u{2030}", "\u{b0}", "\u{b0}C", "\u{b0}F", "\u{2032}"];
    const PREFIX: [&str; 8] = ["\u{a5}", "$", "\u{20ac}", "\u{a3}", "\u{20a9}", "\u{20b9}", "\u{e3f}", "\u{20ab}"];
    if unit.is_empty() || text.is_empty() {
        return text.to_string();
    }
    if PREFIX.contains(&unit) {
        return match text.strip_prefix('-') {
            Some(rest) => format!("-{unit}{rest}"),
            None => format!("{unit}{text}"),
        };
    }
    if TIGHT.contains(&unit) {
        return format!("{text}{unit}");
    }
    format!("{text} {unit}")
}

/// A range with its unit once: "10k\u{2013}70k passengers/day".
pub fn range_text(r: [f64; 2], unit: &str) -> String {
    attach_unit(&format!("{}\u{2013}{}", tick_text(r[0]), tick_text(r[1])), unit)
}

/// Totals per category, in the order `categories` lists them: a count of
/// the rows whose key is that category, or the sum of their `weights`.
pub fn category_totals(keys: &[Option<TagValue>], weights: Option<&[f64]>, categories: &[Category]) -> Vec<f64> {
    let mut out = vec![0.0; categories.len()];
    for (i, key) in keys.iter().enumerate() {
        let Some(key) = key else { continue };
        if let Some(c) = categories.iter().position(|c| same_key(&c.key, key)) {
            out[c] += weights.and_then(|w| w.get(i)).copied().unwrap_or(1.0);
        }
    }
    out
}

fn same_key(a: &TagValue, b: &TagValue) -> bool {
    match (a, b) {
        (TagValue::Number(x), TagValue::Number(y)) => x == y,
        _ => a == b,
    }
}

/// The finite numbers of a column (what a histogram bins).
pub fn numbers(column: &[Option<TagValue>]) -> Vec<f64> {
    column.iter().map(|v| match v {
        Some(TagValue::Number(n)) => *n,
        _ => f64::NAN,
    }).collect()
}

// ---------------------------------------------------------------------------
// The component
// ---------------------------------------------------------------------------

/// One bar of a bar chart.
#[derive(Clone, Debug)]
pub struct Category {
    /// The property value the bar stands for.
    pub key: TagValue,
    pub label: String,
    /// `0xRRGGBB`, else the theme's bar colour.
    pub color: Option<u32>,
}

impl Category {
    pub fn new(key: TagValue, label: &str) -> Self {
        Self { key, label: label.to_string(), color: None }
    }

    #[must_use]
    pub fn color(mut self, rgb: u32) -> Self {
        self.color = Some(rgb);
        self
    }
}

/// What the chart draws.
// One per chart: the inline axis (see `Scale`) costs nothing here.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Kind {
    Histogram { axis: Axis, bins: usize },
    Bars { categories: Vec<Category> },
}

/// What is selected.
#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    All,
    /// A brushed value range (a histogram).
    Range([f64; 2]),
    /// Chosen bars, by index (a bar chart).
    Bars(Vec<usize>),
}

/// How a selection reaches the map.
#[derive(Clone)]
pub enum Link {
    /// A point source's GPU range filter on `field` (declared by
    /// [`LinkedChart::new`]): a brush is one uniform write. Histograms
    /// only (a bar chart over numeric codes may use it too: bars must
    /// then be one contiguous run, else the hull is filtered).
    GpuRange,
    /// The page applies it (a re-declared paint, a re-push, a dim).
    Callback(Rc<dyn Fn(&Selection, &mut App)>),
    /// Nothing: the chart only highlights.
    None,
}

/// A chart's words and numbers.
#[derive(Clone)]
pub struct ChartSpec {
    pub title: String,
    /// The x axis' (histogram) or the values' (bars) unit, e.g.
    /// "population, log scale" or "residents".
    pub unit: String,
    /// What is counted: "cities", "points".
    pub noun: String,
    /// How tick values and bar totals print.
    pub fmt: NumFmt,
    /// A muted line under the chart, e.g. what a click does.
    pub hint: Option<String>,
}

/// A histogram or bar chart over one field of one map source. See the
/// module doc.
pub struct LinkedChart {
    map: WeakEntity<MapView>,
    source: SharedString,
    field: SharedString,
    pub spec: ChartSpec,
    pub kind: Kind,
    link: Link,
    /// Per bin or bar: every row (the grey bars) and the selected rows.
    totals: Vec<f64>,
    /// The rows' keys, kept to recount after a selection.
    keys: Vec<Option<TagValue>>,
    weights: Option<Vec<f64>>,
    /// A numeric field summed per bin or bar instead of counting rows.
    weight_field: Option<SharedString>,
    pub selection: Selection,
    /// The bin or bar under the pointer on the chart.
    pub hover: Option<usize>,
    /// The bin or bar of the object hovered ON THE MAP.
    pub map_hover: Option<usize>,
    /// The plot's window box `(x, y, w, h)` at the last prepaint.
    bounds: Rc<Cell<(f32, f32, f32, f32)>>,
    /// A held brush ([`brush`]), in fractions of the plot's width.
    brush: Option<brush::Drag>,
    /// What the pointer is over on the histogram: the cursor and the
    /// edge grips.
    brush_hit: brush::Hit,
    /// In a card stack ([`over_map`]): the stack's width, not its own
    /// absolute corner.
    pub docked: bool,
    /// Folded to its title row (the card's hide control), so a reader can
    /// clear the map under it.
    pub collapsed: bool,
    /// The host's card layout's say: `Some(true)`
    /// folds the card for want of room, `Some(false)` holds it open (a
    /// phone sheet); `None` leaves it to [`Self::collapsed`].
    pub layout_folded: Option<bool>,
    /// The reader unfolded it: the layout never folds it again (until the
    /// reader folds it).
    pub pinned_open: bool,
    /// A histogram bin's own colour (`0xRRGGBB`), the map's class colour
    /// for its values; `None`: the theme's bar colour.
    pub bin_colors: Vec<Option<u32>>,
    /// How hovered features light.
    pub highlight: HighlightStyle,
    /// What `set_highlight_where` was last asked for.
    lit: Option<HighlightWhere>,
    /// How many `set_source_filter_range` writes the chart made.
    pub filter_writes: u64,
    /// One bar at a time ([`Self::single_select`]).
    single: bool,
    /// More sources a hovered bar lights (a layer split by resolution).
    also: Vec<SharedString>,
    /// Called with the selection when the brush is let go (after the
    /// release's snap): a page whose filter applies on release only.
    pub on_release: Option<Rc<dyn Fn(&Selection, &mut App)>>,
    /// Inside a host's own card: no surface, no title row (the host draws
    /// both).
    pub embedded: bool,
    /// The held brush's window listeners are installed (a frame was
    /// painted since the press); until then the plot's own handlers follow
    /// the drag.
    listening: Rc<Cell<bool>>,
    /// The free brush: the range is exactly what the
    /// pointer marked, never snapped to bins; a click outside clears it.
    /// Bins partly inside draw lit. Frameless: a tint and two grips.
    pub free: bool,
    /// The x axis' unit (a histogram's field's, "passengers/day"): ticks'
    /// caption, the hovered bin and the brushed range carry it.
    pub axis_unit: Option<String>,
    /// What a bar's value is in when it is not a count ("passengers/day"
    /// for a sum): the hover and the summary say it instead of the noun.
    pub value_unit: Option<String>,
    /// A time axis' words for a value ("Jan 2015"): ticks, the hovered
    /// bin and the brushed range read as dates, not epoch numbers.
    pub time_text: Option<Rc<dyn Fn(f64) -> String>>,
    /// Per bin or bar, over EVERY row (the host's unfiltered totals): the
    /// grey ghost behind each coloured bar, and the scale every bar shares,
    /// so a filter elsewhere shrinks the colour against fixed ghosts and
    /// never rescales or reflows the card. Empty: no ghosts, the bars scale
    /// to their own largest.
    pub ghost: Vec<f64>,
    /// The hint the pointer shows over the bars until the reader's first
    /// press ([`HINT_SEEN`]): "Click to filter · click again to remove".
    pub hint_once: Option<String>,
    _subs: Vec<Subscription>,
}

/// Whether a reader pressed a chart's bars yet in this session: the
/// first-time hint ([`LinkedChart::hint_once`]) shows until then.
pub static HINT_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The width of a bar row's check-mark column (reserved on every row, so
/// a pick never moves a label).
pub const CHECK_W: f32 = 12.0;
/// The bar rows' value column: right-aligned tabular figures.
pub const VALUE_W: f32 = 56.0;

/// A bar's length as a fraction of its track: `v` over `max`, a sliver at
/// least when above 0, exactly 0 at 0 (an empty bar over its ghost).
pub fn bar_fraction(v: f64, max: f64) -> f32 {
    if v <= 0.0 || !v.is_finite() {
        return 0.0;
    }
    (v / max.max(1e-9)).clamp(0.01, 1.0) as f32
}

/// Tabular (equal-width) figures: a changing count never changes width.
pub fn tabular() -> gpui::FontFeatures {
    gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]))
}

/// Bars in a histogram unless a page asks otherwise.
pub const BINS: usize = 24;
/// The plot's height.
pub const PLOT_H: f32 = 84.0;
/// A bar row's height.
pub const ROW_H: f32 = 20.0;
/// A bar row's label column.
pub const LABEL_W: f32 = 120.0;

impl LinkedChart {
    /// A chart over `field` of `source`, its rows read from the map
    /// (`MapView::source_column`) -- empty until the source has landed;
    /// [`Self::reload`] reads again. With [`Link::GpuRange`] the field is
    /// declared as the source's filter column.
    pub fn new(
        map: &Entity<MapView>,
        source: &str,
        field: &str,
        kind: Kind,
        spec: ChartSpec,
        link: Link,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut chart = Self::bare(map, source, field, kind, spec, link, cx);
        chart.reload(cx);
        chart
    }

    /// A chart over rows the page holds itself (`keys`, one per pushed
    /// feature, in push order): nothing is read from the map. The field
    /// still names what hover lights; with [`Link::GpuRange`] the page
    /// declares the filter column.
    #[allow(clippy::too_many_arguments)]
    pub fn with_rows(
        map: &Entity<MapView>,
        source: &str,
        field: &str,
        kind: Kind,
        spec: ChartSpec,
        link: Link,
        keys: Vec<Option<TagValue>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut chart = Self::bare(map, source, field, kind, spec, link, cx);
        chart.keys = keys;
        chart.recount();
        chart
    }

    fn bare(map: &Entity<MapView>, source: &str, field: &str, kind: Kind, spec: ChartSpec, link: Link, cx: &mut Context<Self>) -> Self {
        let chart = Self {
            map: map.downgrade(),
            source: source.to_string().into(),
            field: field.to_string().into(),
            spec,
            kind,
            link,
            totals: Vec::new(),
            keys: Vec::new(),
            weights: None,
            weight_field: None,
            selection: Selection::All,
            hover: None,
            map_hover: None,
            bounds: Rc::new(Cell::new((0.0, 0.0, 1.0, 1.0))),
            brush: None,
            brush_hit: brush::Hit::Outside,
            docked: false,
            collapsed: false,
            layout_folded: None,
            pinned_open: false,
            bin_colors: Vec::new(),
            highlight: chart_highlight(),
            lit: None,
            filter_writes: 0,
            single: false,
            also: Vec::new(),
            on_release: None,
            embedded: false,
            listening: Rc::new(Cell::new(false)),
            free: false,
            axis_unit: None,
            time_text: None,
            value_unit: None,
            ghost: Vec::new(),
            hint_once: None,
            _subs: Vec::new(),
        };
        start_probes(cx.entity().downgrade(), cx);
        chart
    }

    /// A hovered bar lights `sources` too (the same field in each).
    pub fn also_light(&mut self, sources: &[String]) {
        self.also = sources.iter().map(|s| s.clone().into()).collect();
    }

    /// Bar or bin totals the page computed itself (no rows): what the
    /// chart draws until the next [`Self::set_totals`] or reload.
    pub fn set_totals(&mut self, totals: Vec<f64>, cx: &mut Context<Self>) {
        if self.totals != totals {
            self.totals = totals;
            cx.notify();
        }
    }

    /// One bar at a time: a click selects that bar alone (again: all).
    #[must_use]
    pub fn single_select(mut self) -> Self {
        self.single = true;
        self
    }

    /// For a card stack ([`over_map`]): the stack's width.
    #[must_use]
    pub fn docked(mut self) -> Self {
        self.docked = true;
        self
    }

    /// Sums numeric `field` per bin or bar (read with the keys) instead
    /// of counting rows: population by continent.
    pub fn sum_of(&mut self, field: &str, cx: &mut Context<Self>) {
        self.weight_field = Some(field.to_string().into());
        self.reload(cx);
    }

    /// Counts `keys` (one per row) instead of reading the map: a page that
    /// holds its own column (the synthetic points).
    pub fn set_rows(&mut self, keys: Vec<Option<TagValue>>, weights: Option<Vec<f64>>, cx: &mut Context<Self>) {
        self.keys = keys;
        self.weights = weights;
        self.recount();
        cx.notify();
    }

    /// Reads the column from the map again (after the source landed or
    /// changed). Rows of `weight_field`, when given, are summed instead of
    /// counted.
    pub fn reload(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(map) = self.map.upgrade() else { return false };
        let Some(keys) = map.read(cx).source_column(&self.source, &self.field) else { return false };
        if let Some(w) = &self.weight_field {
            let Some(weights) = map.read(cx).source_column(&self.source, w) else { return false };
            self.weights = Some(numbers(&weights).into_iter().map(|v| if v.is_finite() { v } else { 0.0 }).collect());
        }
        if matches!(self.link, Link::GpuRange) {
            let (source, field) = (self.source.clone(), self.field.clone());
            // A page that declared several fields (a time slider's and
            // this chart's, packed) keeps its declaration.
            let declared = map.read(cx).source_filter_fields(&source).is_some_and(|f| f.iter().any(|f| *f == *field));
            if !declared {
                let domain = map.update(cx, |m, cx| m.set_source_filter_field(&source, Some(&field), cx));
                eprintln!("linked chart: {source}.{field} filter column, domain {domain:?}");
            }
        }
        self.keys = keys;
        self.recount();
        cx.notify();
        true
    }

    /// Sums `weights` per bin or bar instead of counting rows.
    pub fn set_weights(&mut self, weights: Option<Vec<f64>>, cx: &mut Context<Self>) {
        self.weights = weights;
        self.recount();
        cx.notify();
    }

    pub fn rows(&self) -> usize {
        self.keys.len().max(usize::from(self.totals.iter().any(|v| *v > 0.0)))
    }

    pub fn totals(&self) -> &[f64] {
        &self.totals
    }

    /// The unfiltered totals per bin or bar ([`Self::ghost`]).
    pub fn set_ghost(&mut self, ghost: Vec<f64>, cx: &mut Context<Self>) {
        if self.ghost != ghost {
            self.ghost = ghost;
            cx.notify();
        }
    }

    /// The scale every bar shares: the largest ghost when the host gave
    /// them (one per slot), else the largest total.
    pub fn scale_max(&self) -> f64 {
        let of = if self.ghost.len() == self.totals.len() && !self.ghost.is_empty() { &self.ghost } else { &self.totals };
        of.iter().copied().filter(|v| v.is_finite()).fold(0.0, f64::max).max(1e-9)
    }

    /// The bars picked by this chart's own filter (empty: none).
    pub fn picked(&self) -> usize {
        match &self.selection {
            Selection::Bars(v) => v.len(),
            _ => 0,
        }
    }

    fn recount(&mut self) {
        self.totals = match &self.kind {
            Kind::Histogram { axis, bins } => histogram(&numbers(&self.keys), self.weights.as_deref(), axis, *bins),
            Kind::Bars { categories } => category_totals(&self.keys, self.weights.as_deref(), categories),
        };
    }

    /// The bin or bar a property value falls in.
    pub fn slot_of(&self, value: Option<&TagValue>) -> Option<usize> {
        let value = value?;
        match (&self.kind, value) {
            (Kind::Histogram { axis, bins }, TagValue::Number(v)) => bin_of(axis, *bins, *v),
            (Kind::Histogram { .. }, _) => None,
            (Kind::Bars { categories }, v) => categories.iter().position(|c| same_key(&c.key, v)),
        }
    }

    /// The predicate that lights slot `i`'s features.
    pub fn slot_predicate(&self, i: usize) -> Option<HighlightWhere> {
        match &self.kind {
            Kind::Histogram { axis, bins } => Some(HighlightWhere::range(&self.field, bin_range(axis, *bins, i))),
            Kind::Bars { categories } => categories.get(i).map(|c| HighlightWhere::one_of(&self.field, vec![c.key.clone()])),
        }
    }

    /// Whether slot `i` is inside the selection.
    pub fn selected(&self, i: usize) -> bool {
        match (&self.selection, &self.kind) {
            (Selection::All, _) => true,
            (Selection::Range(r), Kind::Histogram { axis, bins }) if self.free => {
                // A bin partly inside draws lit.
                let [a, b] = bin_range(axis, *bins, i);
                b > r[0] && a < r[1]
            }
            (Selection::Range(r), Kind::Histogram { axis, bins }) => {
                let (a, b) = range_bins(axis, *bins, *r);
                (a..=b).contains(&i)
            }
            (Selection::Bars(set), _) => set.contains(&i),
            _ => true,
        }
    }

    /// The total of the selected rows.
    pub fn selected_total(&self) -> f64 {
        // A free range cuts bins: count the rows themselves.
        if let (true, Selection::Range([lo, hi]), Kind::Histogram { .. }) = (self.free, &self.selection, &self.kind) {
            if !self.keys.is_empty() {
                return self
                    .keys
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| matches!(k, Some(TagValue::Number(v)) if *v >= *lo && *v <= *hi))
                    .map(|(i, _)| self.weights.as_ref().and_then(|w| w.get(i)).copied().unwrap_or(1.0))
                    .sum();
            }
        }
        self.totals.iter().enumerate().filter(|(i, _)| self.selected(*i)).map(|(_, v)| v).sum()
    }

    /// Selects, and sends it to the map.
    pub fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        let selection = match selection {
            Selection::Bars(v) if v.is_empty() => Selection::All,
            s => s,
        };
        self.selection = selection.clone();
        match self.link.clone() {
            Link::GpuRange => {
                let range = match (&selection, &self.kind) {
                    (Selection::Range(r), _) => Some(*r),
                    (Selection::Bars(set), Kind::Bars { categories }) => {
                        let codes: Vec<f64> = set.iter().filter_map(|&i| match categories.get(i).map(|c| &c.key) {
                            Some(TagValue::Number(n)) => Some(*n),
                            _ => None,
                        }).collect();
                        let lo = codes.iter().copied().fold(f64::INFINITY, f64::min);
                        let hi = codes.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                        (lo <= hi).then_some([lo, hi])
                    }
                    _ => None,
                };
                let (source, field) = (self.source.clone(), self.field.clone());
                self.filter_writes += 1;
                // Its own field's window: another control's field on the
                // same packed filter (a time slider) keeps its window.
                let _ = self.map.update(cx, |m, cx| {
                    if !m.set_source_filter_field_range(&source, &field, range, cx) {
                        m.set_source_filter_range(&source, range, cx);
                    }
                });
            }
            Link::Callback(f) => f(&selection, cx),
            Link::None => {}
        }
        cx.notify();
    }

    /// The pointer over slot `i` of the chart (or off it): its features
    /// light on the map.
    pub fn set_hover(&mut self, i: Option<usize>, cx: &mut Context<Self>) {
        if self.hover == i {
            return;
        }
        self.hover = i;
        let want = i.and_then(|i| self.slot_predicate(i));
        if want != self.lit {
            let mine = std::mem::replace(&mut self.lit, want.clone());
            let (style, sources) = (self.highlight, std::iter::once(self.source.clone()).chain(self.also.iter().cloned()).collect::<Vec<_>>());
            let _ = self.map.update(cx, |m, cx| {
                for source in &sources {
                    // Clearing: only this chart's own highlight. Two charts
                    // over one source (taxi fares and passengers): the
                    // pointer leaving one must not clear what the other
                    // just lit.
                    if want.is_none() && m.highlight_where(source).map(|(p, _)| p) != mine.as_ref() {
                        continue;
                    }
                    m.set_highlight_where(source, want.clone(), style, cx);
                }
            });
        }
        cx.notify();
    }

    /// The bin or bar of the object hovered on the map, from a page that
    /// picks its objects itself (the race map's hexagons, which keep their
    /// own click popup): what [`Self::follow_popup`] sets.
    pub fn set_map_hover(&mut self, slot: Option<usize>, cx: &mut Context<Self>) {
        if slot != self.map_hover {
            self.map_hover = slot;
            cx.notify();
        }
    }

    /// Follows `popup`'s hovered (or pinned) object: the bar holding it
    /// lights. Objects of other sources (than the chart's and the ones it
    /// [`Self::also_light`]s) are ignored.
    pub fn follow_popup(&mut self, popup: &Entity<crate::popup::MapPopup>, cx: &mut Context<Self>) {
        let sub = cx.observe(popup, |this: &mut Self, popup, cx| {
            let slot = {
                let p = popup.read(cx);
                p.hovered_pick().or(p.pinned_pick()).and_then(|pick| {
                    let gpui_maps::map_view::MapObject::Feature(f) = &pick.object else { return None };
                    if *f.source != *this.source && !this.also.iter().any(|a| **a == *f.source) {
                        return None;
                    }
                    let props = pick.properties.as_deref()?;
                    this.slot_of(props.get(this.field.as_ref()))
                })
            };
            if slot != this.map_hover {
                this.map_hover = slot;
                cx.notify();
            }
        });
        self._subs.push(sub);
    }

    fn fraction_at(&self, x: f32) -> f64 {
        let (bx, _, bw, _) = self.bounds.get();
        f64::from(((x - bx) / bw.max(1.0)).clamp(0.0, 1.0))
    }

    fn row_at(&self, y: f32) -> Option<usize> {
        let (_, by, _, bh) = self.bounds.get();
        let n = self.totals.len();
        if n == 0 || y < by || y > by + bh {
            return None;
        }
        Some((((y - by) / bh) * n as f32).floor().max(0.0) as usize).filter(|i| *i < n)
    }

    /// The selected range as fractions of the plot's width, if a range.
    fn window_frac(&self) -> Option<[f64; 2]> {
        match (&self.selection, &self.kind) {
            (Selection::Range(r), Kind::Histogram { axis, .. }) => Some([axis.fraction(r[0]).clamp(0.0, 1.0), axis.fraction(r[1]).clamp(0.0, 1.0)]),
            _ => None,
        }
    }

    /// `px` logical pixels as a fraction of the plot's width.
    fn px_frac(&self, px: f32) -> f64 {
        f64::from(px / self.bounds.get().2.max(1.0))
    }

    /// A press at window `(x, y)` on the plot.
    pub fn press(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        match &self.kind {
            Kind::Histogram { .. } => {
                self.brush = Some(brush::press(self.window_frac(), self.fraction_at(x), self.px_frac(brush::EDGE_PX)));
                cx.notify();
            }
            Kind::Bars { .. } => {
                let Some(i) = self.row_at(y) else { return };
                HINT_SEEN.store(true, std::sync::atomic::Ordering::Relaxed);
                let mut set = match &self.selection {
                    Selection::Bars(v) => v.clone(),
                    _ => Vec::new(),
                };
                if let Some(at) = set.iter().position(|&j| j == i) {
                    set.remove(at);
                } else if self.single {
                    set = vec![i];
                } else {
                    set.push(i);
                    set.sort_unstable();
                }
                self.select(Selection::Bars(set), cx);
            }
        }
    }

    /// The pointer at window `x` with the button held.
    /// Moves, resizes or draws the window (the [`brush`] rules), the map
    /// following live through the filter uniform; bins snap on release.
    pub fn drag(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(mut d) = self.brush else { return };
        let Kind::Histogram { axis, .. } = self.kind.clone() else { return };
        let window = brush::drag(&mut d, self.fraction_at(x), [0.0, 1.0], self.px_frac(3.0));
        self.brush = Some(d);
        if let Some([a, b]) = window {
            self.select(Selection::Range([axis.value(a), axis.value(b)]), cx);
        }
    }

    /// The button up at window `x`. A drag snaps: a drawn window OUT to
    /// whole bins, a moved or resized one to the nearest bin edges (its
    /// width in bins kept). A click outside the selection selects that bin;
    /// a click inside changes nothing.
    pub fn release(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(d) = self.brush.take() else { return };
        let Kind::Histogram { axis, bins } = self.kind.clone() else { return };
        self.released(d, axis, bins, x, cx);
        if let Some(f) = self.on_release.clone() {
            f(&self.selection.clone(), cx);
        }
    }

    fn released(&mut self, d: brush::Drag, axis: Axis, bins: usize, x: f32, cx: &mut Context<Self>) {
        match brush::release(d) {
            // The free brush keeps what was drawn; a click outside clears.
            brush::Release::Dragged if self.free => cx.notify(),
            brush::Release::ClickOutside if self.free => self.select(Selection::All, cx),
            brush::Release::Dragged => {
                if let Some([a, b]) = self.window_frac() {
                    let snapped = match d.grab {
                        brush::Grab::New { .. } => brush_to_range(&axis, bins, a, b),
                        _ => snap_nearest(&axis, bins, a, b),
                    };
                    self.select(Selection::Range(snapped), cx);
                }
                cx.notify();
            }
            brush::Release::ClickOutside => {
                let bin = ((self.fraction_at(x) * bins as f64).floor() as usize).min(bins - 1);
                self.select(Selection::Range(bin_range(&axis, bins, bin)), cx);
            }
            brush::Release::ClickInside => cx.notify(),
        }
    }

    /// The pointer over the plot at window `(x, y)`.
    fn pointer(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        if matches!(self.kind, Kind::Histogram { .. }) && self.brush.is_none() {
            let hit = brush::hit(self.window_frac(), self.fraction_at(x), self.px_frac(brush::EDGE_PX));
            if hit != self.brush_hit {
                self.brush_hit = hit;
                cx.notify();
            }
        }
        let slot = match &self.kind {
            Kind::Histogram { bins, .. } => {
                let (bx, by, bw, bh) = self.bounds.get();
                (x >= bx && x <= bx + bw && y >= by && y <= by + bh)
                    .then(|| ((self.fraction_at(x) * *bins as f64).floor() as usize).min(bins - 1))
            }
            Kind::Bars { .. } => self.row_at(y),
        };
        self.set_hover(slot, cx);
    }

    fn colours(cx: &App) -> Palette {
        if cx.try_global::<gpui_component::Theme>().is_some_and(|t| t.mode.is_dark()) {
            Palette::DARK
        } else {
            Palette::LIGHT
        }
    }
}

/// Reads the column once the source has landed (a file loaded off the UI
/// thread), polling every 250 ms for at most 60 s.
pub fn reload_when_ready(chart: &Entity<LinkedChart>, cx: &mut App) {
    let weak = chart.downgrade();
    cx.spawn(async move |cx| {
        for _ in 0..240 {
            let Ok(done) = weak.update(cx, |c, cx| c.reload(cx) && c.rows() > 0) else { return };
            if done {
                return;
            }
            cx.background_executor().timer(Duration::from_millis(250)).await;
        }
    })
    .detach();
}

/// Every map corner's inset: the engine's `chrome::CORNER_INSET_PX`, the
/// one the compass, the brand mark and the credits stand on.
pub const CORNER_INSET_PX: f32 = gpui_maps::chrome::CORNER_INSET_PX;

/// A floating card's width: compact enough to leave the map the page.
pub const CARD_W: f32 = 280.0;

/// **The one floating-card layout.** Every card floated over a map --
/// the Explorer's left card, the top-right chart stack, the agent chat
/// card -- starts on ONE top line, [`CARD_TOP_PX`], one
/// [`CORNER_INSET_PX`] in from the map's edges, with one [`CARD_GAP_PX`]
/// between adjacent cards. The compass does not push the cards down: the
/// host stands it beside them ([`compass_offset`], applied through
/// `MapView::set_compass_offset`). Left and right cards share one top
/// edge.
pub const CARD_TOP_PX: f32 = CORNER_INSET_PX;

/// The gap between two adjacent floating cards (the chat card and the chart
/// stack beside it, stacked cards): the corner inset again.
pub const CARD_GAP_PX: f32 = CORNER_INSET_PX;

/// The top of the top-right card stack. On a host that places the compass
/// ([`set_compass_host`], Studio) it is the cards' one top line; on a
/// standalone example, which leaves the compass in its corner, it stays one
/// inset below the compass's disc so a rotated map's compass is never
/// under a chart.
pub fn stack_top_px() -> f32 {
    if COMPASS_HOST.with(|h| h.borrow().is_some()) {
        CARD_TOP_PX
    } else {
        CORNER_INSET_PX + gpui_maps::compass::COMPASS_SIZE_PX + CORNER_INSET_PX
    }
}

/// Where the compass stands, as `MapView::set_compass_offset`'s
/// `(left, down)`, on a `map_w` wide map whose top-right column of cards
/// reaches `column_right_px` in from the map's right edge (0: no column)
/// and runs down to `column_bottom_px`; `left_reserved_px` is how far the
/// left column reaches in from the left edge (0: none).
///
/// - No column: its own corner.
/// - Room on the top line: just left of the column, one [`CARD_GAP_PX`]
///   off it, on the cards' top line.
/// - No room: in its corner's column, one gap below the column's bottom.
#[must_use]
pub fn compass_offset(map_w: f32, column_right_px: f32, column_bottom_px: f32, left_reserved_px: f32) -> (f32, f32) {
    use gpui_maps::compass::{COMPASS_MARGIN_PX, COMPASS_SIZE_PX};
    if column_right_px <= 0.0 {
        return (0.0, 0.0);
    }
    // The compass's corner spot is COMPASS_MARGIN_PX in from the right; it
    // moves left until its right edge is one gap clear of the column.
    let left = column_right_px + CARD_GAP_PX - COMPASS_MARGIN_PX;
    let x = map_w - COMPASS_MARGIN_PX - COMPASS_SIZE_PX - left;
    let floor = if left_reserved_px > 0.0 { left_reserved_px + CARD_GAP_PX } else { CORNER_INSET_PX };
    if x >= floor {
        (left, CARD_TOP_PX - COMPASS_MARGIN_PX)
    } else {
        (0.0, (column_bottom_px + CARD_GAP_PX - COMPASS_MARGIN_PX).max(0.0))
    }
}

/// The map whose compass the floating cards place, and how far its left
/// column reaches in (`left_reserved_px` of [`compass_offset`]). Set by the
/// host every render (Studio's gallery; `None` on a page with no map);
/// never set by a standalone example. Per thread, like [`CHAT_INSET`].
#[allow(dead_code)] // Studio only; the standalone examples include this file too
pub fn set_compass_host(host: Option<(WeakEntity<MapView>, f32)>) {
    COMPASS_HOST.with(|h| *h.borrow_mut() = host);
}

thread_local! {
    static COMPASS_HOST: std::cell::RefCell<Option<(WeakEntity<MapView>, f32)>> = const { std::cell::RefCell::new(None) };
    /// This frame's chart stack height, recorded by its prepaint.
    static STACK_H: std::cell::Cell<Option<f32>> = const { std::cell::Cell::new(None) };
    /// The open chat card's bottom, map-local (0 when closed).
    static CHAT_BOTTOM: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

/// Records this frame's top-right column height for [`place_compass`],
/// for a host that lays its cards out itself instead of
/// through [`card_stack`]; `None`: no column.
#[allow(dead_code)] // Studio only
pub fn record_stack_height(h: Option<f32>) {
    STACK_H.with(|c| c.set(h));
}

/// Sets the open chat card's bottom (Studio's chat, every render).
#[allow(dead_code)] // Studio only
pub fn set_chat_bottom(px: f32) {
    CHAT_BOTTOM.with(|c| c.set(px));
}

/// Places the compass for the column as it stands this frame: the chat
/// card (if open) plus the chart stack, if one was prepainted
/// ([`card_stack`] records its height), applied to the host's map. The
/// host runs it from a canvas prepainted AFTER the panels (the last child
/// of its root), so it sees this frame's stack, and before any map paints.
#[allow(dead_code)] // Studio only
pub fn place_compass(cx: &mut App) {
    let stack_h = STACK_H.with(std::cell::Cell::take);
    let Some((map, left_reserved)) = COMPASS_HOST.with(|h| h.borrow().clone()) else { return };
    let Some(map) = map.upgrade() else { return };
    let chat = chat_inset();
    let chat_bottom = CHAT_BOTTOM.with(std::cell::Cell::get);
    let (right, bottom) = match stack_h {
        Some(h) => (CORNER_INSET_PX + chat + CARD_W, (CARD_TOP_PX + h).max(chat_bottom)),
        None if chat > 0.0 => (chat, chat_bottom),
        None => (0.0, 0.0),
    };
    let map_w = map.read(cx).camera().viewport_width_px;
    let want = compass_offset(map_w, right, bottom, left_reserved);
    if map.read(cx).compass_offset() != want {
        map.update(cx, |m, _| m.set_compass_offset(want.0, want.1));
    }
}

/// The map area (the map and everything over it) full-bleed, with
/// `children` -- charts, cards -- floating as a top-right stack over it
/// ([`card_stack`]). Nothing here paints a background behind the map (an
/// opaque root hides its layer); each card brings its own translucent
/// surface, and each [`LinkedChart`] folds to its title row.
pub fn over_map(map_area: impl IntoElement, children: Vec<AnyElement>) -> gpui::Div {
    div().relative().size_full().child(div().absolute().top_0().left_0().size_full().child(map_area)).child(card_stack(children))
}

thread_local! {
    /// How far Studio's agent chat card reaches in from the map's right
    /// edge while it is open (0 otherwise): the stack moves left by this
    /// much (spec 2026-10-07-agent-chat §6.1 `OverlayInsets`). Per thread,
    /// so parallel window tests never see each other's.
    static CHAT_INSET: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

/// Sets [`CHAT_INSET`] (Studio's chat, every render). The chat card is the
/// right column's rightmost member: every page card anchored at
/// [`right_card_right_px`] moves left of it, and the popups keep clear
/// of it (`popup::reserve_right`).
#[allow(dead_code)] // Studio only; the standalone examples include this file too
pub fn set_chat_inset(px: f32) {
    CHAT_INSET.with(|c| c.set(px));
    crate::popup::reserve_right("chat", px);
}

/// The narrowest strip of map worth keeping between the left edge and the
/// right column while the chat is open: 320 logical px. That is a phone's
/// width -- the narrowest map any shell is designed for -- and still more
/// than one 256 px tile plus the corner insets, so the strip pans, zooms
/// and shows a hover card (264 px) beside the pointer. Below it a page's
/// right card folds to its header instead of squeezing the map to a
/// sliver or being covered by the chat.
pub const MIN_MAP_W: f32 = 320.0;

thread_local! {
    /// The host map's width, logical px (0: unknown, e.g. standalone).
    static MAP_W: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

/// Sets the host map's width (Studio's gallery, every render).
#[allow(dead_code)] // Studio only
pub fn set_map_width(px: f32) {
    MAP_W.with(|c| c.set(px));
}

/// Where a page's right-side card stands: its right edge this far in from
/// the map's right edge -- the corner inset, or one gap left of the open
/// chat card. Every right-anchored floating card reads it.
pub fn right_card_right_px() -> f32 {
    CORNER_INSET_PX + chat_inset()
}

/// Whether a `card_w` wide page card beside the open chat would leave less
/// than [`MIN_MAP_W`] of map: it then folds to its header (the page's own
/// fold), and unfolds when the chat closes. `false` with the chat closed.
/// A docked chat ([`set_chat_docked`]) sets no inset: the page, and its
/// map, narrow instead, so the map's own width decides.
pub fn squeezed_by_chat(card_w: f32) -> bool {
    let map_w = MAP_W.with(std::cell::Cell::get);
    (chat_inset() > 0.0 || CHAT_DOCKED.with(std::cell::Cell::get)) && map_w > 0.0 && map_w - right_card_right_px() - card_w < MIN_MAP_W
}

thread_local! {
    /// Whether the chat is open as a docked column (ADR 0020).
    static CHAT_DOCKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Sets [`CHAT_DOCKED`] (Studio's chat, every render).
#[allow(dead_code)] // Studio only
pub fn set_chat_docked(open: bool) {
    CHAT_DOCKED.with(|c| c.set(open));
}

fn chat_inset() -> f32 {
    CHAT_INSET.with(std::cell::Cell::get)
}

/// The top-right card stack alone, for a page that lays out its own map
/// area: absolute, so put it in a `relative` parent over the map.
/// [`CARD_W`] wide, one [`CORNER_INSET_PX`] in from the right, starting
/// on the cards' top line ([`stack_top_px`]); its height is its cards', so the
/// map stays live (wheel, drag) everywhere around them.
pub fn card_stack(children: Vec<AnyElement>) -> gpui::Stateful<gpui::Div> {
    reserve_stack(!children.is_empty());
    div()
        .id("chart-stack")
        .debug_selector(|| "chart-stack".to_string())
        .absolute()
        .top(px(stack_top_px()))
        .right(px(right_card_right_px()))
        .w(px(CARD_W))
        .flex()
        .flex_col()
        .gap(px(CARD_GAP_PX))
        .children(children)
        // This frame's height, for [`place_compass`] (below the column
        // when there is no room beside it).
        .child(canvas(|bounds, _, _| STACK_H.with(|c| c.set(Some(f32::from(bounds.size.height)))), |_, _, _, _| {}).absolute().top_0().left_0().size_full())
}

/// [`card_stack`], registered with the popups' keep-out
/// (`popup::reserve_right`) when it holds any card.
fn reserve_stack(shown: bool) {
    crate::popup::reserve_right("chart-stack", if shown { right_card_right_px() + CARD_W + CARD_GAP_PX } else { 0.0 });
}

/// The surface every floating card shares (the shared popup's, in the app
/// theme): the Explorer's editor stack and rail, the agent chat, a page's
/// own legend card. OPAQUE: even at 95 % alpha, map detail showing
/// through a card reads as noise. The border and shadow still float it over
/// the full-bleed map.
pub fn card_surface<E: Styled>(card: E, cx: &App) -> E {
    let p = LinkedChart::colours(cx);
    card.rounded(px(8.0)).bg(rgba(p.panel)).border_1().border_color(rgba(p.grid)).shadow_md().text_color(rgba(p.text))
}

/// A card's small hide / show control: `−` open, `+` folded.
pub fn fold_button(id: &'static str, folded: bool, cx: &App, on: impl Fn(&mut Window, &mut App) + 'static) -> gpui::Stateful<gpui::Div> {
    let p = LinkedChart::colours(cx);
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex_none()
        .w(px(18.0))
        .h(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .cursor_pointer()
        .text_color(rgba(p.muted))
        .hover(|s| s.bg(rgba(p.grid)))
        .child(if folded { "+" } else { "\u{2212}" })
        .on_click(move |_, window, cx| on(window, cx))
}

/// The bundled world cities' population (CSV, FlatGeobuf and GeoParquet
/// pages): a log-scale histogram, brushed through the point source's GPU
/// range filter, its bins lit by the popup's hover.
pub fn city_population_chart(map: &Entity<MapView>, source: &str, popup: &Entity<crate::popup::MapPopup>, cx: &mut App) -> Entity<LinkedChart> {
    let spec = ChartSpec {
        title: "Population".into(),
        unit: "People per city, log scale. Drag to filter the map; click a bar for one bin.".into(),
        noun: "cities".into(),
        fmt: NumFmt::Compact,
        hint: None,
    };
    let kind = Kind::Histogram { axis: Axis::log10([1e5, 4e7]), bins: 12 };
    let chart = cx.new(|cx| {
        let mut c = LinkedChart::new(map, source, "population", kind, spec, Link::GpuRange, cx).docked();
        c.follow_popup(popup, cx);
        c
    });
    reload_when_ready(&chart, cx);
    chart
}

/// How chart-hovered features light: a bright outline, no casing clutter.
pub fn chart_highlight() -> HighlightStyle {
    let c = |r: u8, g: u8, b: u8, a: f32| gpui_maps::style::Rgba { r: f32::from(r) / 255.0, g: f32::from(g) / 255.0, b: f32::from(b) / 255.0, a };
    HighlightStyle {
        outline_color: c(255, 196, 0, 1.0),
        outline_width_px: 2.0,
        fill_color: Some(c(255, 196, 0, 0.18)),
        casing_color: Some(c(20, 20, 26, 0.8)),
        casing_width_px: 1.0,
    }
}

/// The chart's colours, by the app theme.
#[derive(Clone, Copy)]
struct Palette {
    bg: u32,
    panel: u32,
    text: u32,
    muted: u32,
    grid: u32,
    bar: u32,
    bar_in: u32,
}

impl Palette {
    const LIGHT: Self =
        Self { bg: 0xffff_ffff, panel: 0xffff_ffff, text: 0x1f29_33ff, muted: 0x5f6b_76ff, grid: 0x1f29_331a, bar: 0xc5cc_d3ff, bar_in: 0x2f6f_b0ff };
    const DARK: Self =
        Self { bg: 0x1d21_27ff, panel: 0x1d21_27ff, text: 0xe6e9_edff, muted: 0xa3ad_b8ff, grid: 0xffff_ff1a, bar: 0x4a50_58ff, bar_in: 0x6ba8_e5ff };
}

/// A bar's own colour (`0xRRGGBB`) made readable on the panel: a ramp's
/// darkest class (#3b0f4f) is lifted toward white on a dark panel, its
/// palest darkened on a light one, by how far it falls short; mid colours
/// are kept as they are.
pub fn readable_on(rgb: u32, dark: bool) -> u32 {
    let ch = |shift: u32| ((rgb >> shift) & 0xff) as f64;
    let (r, g, b) = (ch(16), ch(8), ch(0));
    let luma = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
    let (target, t) = if dark { (255.0, ((0.32 - luma) / 0.32).clamp(0.0, 1.0) * 0.55) } else { (0.0, ((luma - 0.82) / 0.18).clamp(0.0, 1.0) * 0.35) };
    let mix = |c: f64| (c + (target - c) * t).round().clamp(0.0, 255.0) as u32;
    (mix(r) << 16) | (mix(g) << 8) | mix(b)
}

fn short(v: f64, fmt: NumFmt) -> String {
    number(v, fmt)
}

impl Render for LinkedChart {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Self::colours(cx);
        let bounds = self.bounds.clone();
        let probe = canvas(
            move |b, _, _| bounds.set((f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height))),
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        // One scale for every bar: the ghosts' (the unfiltered totals), so
        // a filter shrinks the colour and never rescales the card.
        let max = self.scale_max();
        let ghosted = self.ghost.len() == self.totals.len() && !self.ghost.is_empty();
        let any = !matches!(self.selection, Selection::All);
        let dark = p.bg == Palette::DARK.bg;
        // Colour says one thing only: what passes. The bar's own colour (or
        // the accent), faded where this chart's own filter leaves it out.
        // The pointer adds no colour: a hovered row or bin gets a faint
        // background (`p.grid`) instead.
        let colour_of = |i: usize, own: Option<u32>| -> u32 {
            let base = own.map(|rgb| readable_on(rgb, dark)).map_or(p.bar_in, |rgb| (rgb << 8) | 0xff);
            if any && !self.selected(i) {
                (base & 0xffff_ff00) | 0x59
            } else {
                base
            }
        };
        // A held brush follows the pointer anywhere until the release.
        let entity = cx.entity().downgrade();
        let listening = self.listening.clone();
        listening.set(false);
        let drag_listeners = self.brush.is_some().then(|| {
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    listening.set(true);
                    let moved = entity.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        let _ = moved.update(cx, |this, cx| {
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.drag(f32::from(event.position.x), cx);
                            } else {
                                this.release(f32::from(event.position.x), cx);
                            }
                        });
                    });
                    let released = entity.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                            let _ = released.update(cx, |this, cx| this.release(f32::from(event.position.x), cx));
                        }
                    });
                },
            )
            .absolute()
            .size_0()
        });
        let plot: AnyElement = match &self.kind {
            Kind::Histogram { axis, .. } => {
                // Each bin: its ghost (every row) in grey, what passes in
                // colour over it; the bin under the pointer (or the map's
                // hovered object's) gets a faint column, never a colour.
                let bars = self.totals.iter().enumerate().map(|(i, &c)| {
                    let lit = self.hover == Some(i) || self.map_hover == Some(i);
                    let g = self.ghost.get(i).copied().filter(|_| ghosted).unwrap_or(0.0);
                    div()
                        .flex_1()
                        .h_full()
                        .relative()
                        .when(lit, |d| d.bg(rgba(p.grid)))
                        .when(g > 0.0, |d| {
                            // Top-anchored at 1 - h: a bar inside the chart, not a
                            // card at the map's foot (the brand-mark audit).
                            let h = bar_fraction(g, max).max(0.04);
                            d.child(div().absolute().top(relative(1.0 - h)).left_0().right_0().h(relative(h)).bg(rgba(p.bar)))
                        })
                        .child({
                            let h = if c > 0.0 { bar_fraction(c, max).max(0.04) } else { 0.0 };
                            div().absolute().top(relative(1.0 - h)).left_0().right_0().h(relative(h)).bg(rgba(colour_of(i, self.bin_colors.get(i).copied().flatten())))
                        })
                });
                // The held grab, or what the pointer is over: the cursor
                // and which edge shows its grip.
                let hit = match self.brush.map(|d| d.grab) {
                    Some(brush::Grab::Move { .. }) => brush::Hit::Inside,
                    Some(brush::Grab::Edge { .. }) => brush::Hit::Edge(brush::Side::Low),
                    Some(brush::Grab::New { .. }) => brush::Hit::Outside,
                    None => self.brush_hit,
                };
                let brush_box = match (&self.selection, &self.kind) {
                    (Selection::Range(r), Kind::Histogram { axis, .. }) => {
                        let (a, b) = (axis.fraction(r[0]).clamp(0.0, 1.0), axis.fraction(r[1]).clamp(0.0, 1.0));
                        let grip = |side: brush::Side| {
                            let lit = matches!(hit, brush::Hit::Edge(s) if s == side) || matches!(self.brush.map(|d| d.grab), Some(brush::Grab::Edge { .. }));
                            div()
                                .absolute()
                                .top_0()
                                .h_full()
                                .w(px(2.0))
                                .when(side == brush::Side::Low, |d| d.left(px(-1.0)))
                                .when(side == brush::Side::High, |d| d.right(px(-1.0)))
                                .when(lit, |d| d.bg(rgba(p.text)))
                        };
                        let free = self.free;
                        Some(
                            div()
                                .absolute()
                                .top_0()
                                .h_full()
                                .left(relative(a as f32))
                                .w(relative((b - a) as f32))
                                // The free brush is frameless: a tint and
                                // two grips that are always there.
                                .when(!free, |d| d.border_1().border_color(rgba(p.bar_in)))
                                .bg(Hsla::from(rgba(p.bar_in)).opacity(if free { 0.16 } else { 0.10 }))
                                .when(free, |d| {
                                    let edge = |side: brush::Side| {
                                        div()
                                            .absolute()
                                            .top_0()
                                            .h_full()
                                            .w(px(2.0))
                                            .when(side == brush::Side::Low, |d| d.left(px(-1.0)))
                                            .when(side == brush::Side::High, |d| d.right(px(-1.0)))
                                            .bg(Hsla::from(rgba(p.bar_in)).opacity(0.7))
                                    };
                                    d.child(edge(brush::Side::Low)).child(edge(brush::Side::High))
                                })
                                .child(grip(brush::Side::Low))
                                .child(grip(brush::Side::High)),
                        )
                    }
                    _ => None,
                };
                let tick_marks = ticks(axis, 4);
                let labels = tick_marks.iter().map(|&(t, v)| {
                    // The end ticks' words stay inside the plot: the first
                    // starts at its tick, the last ends at it.
                    let (ml, edge) = if t <= 0.02 { (0.0, -1) } else if t >= 0.98 { (-60.0, 1) } else { (-30.0, 0) };
                    div()
                        .absolute()
                        .left(relative(t as f32))
                        .ml(px(ml))
                        .w(px(60.0))
                        .flex()
                        .map(|d| match edge {
                            -1 => d.justify_start(),
                            1 => d.justify_end(),
                            _ => d.justify_center(),
                        })
                        .child(match (&self.time_text, self.spec.fmt) {
                            (Some(f), _) => f(v),
                            (None, NumFmt::Auto | NumFmt::Int | NumFmt::Compact) => tick_text(v),
                            (None, fmt) => short(v, fmt),
                        })
                });
                let grid = tick_marks.iter().map(|&(t, _)| div().absolute().top_0().h_full().left(relative(t as f32)).w(px(1.0)).bg(rgba(p.grid)));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .id("linked-chart-plot")
                            .debug_selector(|| "linked-chart-plot".to_string())
                            .relative()
                            .h(px(PLOT_H))
                            .w_full()
                            .map(|d| match hit {
                                brush::Hit::Inside => d.cursor(gpui::CursorStyle::OpenHand),
                                brush::Hit::Edge(_) => d.cursor(gpui::CursorStyle::ResizeLeftRight),
                                brush::Hit::Outside => d.cursor_crosshair(),
                            })
                            .border_b_1()
                            .border_color(rgba(p.muted))
                            .child(probe)
                            .children(grid)
                            .child(div().absolute().size_full().flex().gap(px(1.0)).children(bars))
                            .children(brush_box)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                                    this.press(f32::from(e.position.x), f32::from(e.position.y), cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                                if e.pressed_button.is_none() {
                                    this.pointer(f32::from(e.position.x), f32::from(e.position.y), cx);
                                } else if e.pressed_button == Some(MouseButton::Left) && this.brush.is_some() && !this.listening.get() {
                                    // A drag faster than a frame: the
                                    // window listeners are not in yet.
                                    this.drag(f32::from(e.position.x), cx);
                                }
                            }))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, e: &MouseUpEvent, _, cx| {
                                    if !this.listening.get() {
                                        this.release(f32::from(e.position.x), cx);
                                    }
                                }),
                            )
                            // A press and a release OUTSIDE the plot with no
                            // frame between them (a flick, or a synthetic
                            // burst): the window listeners never went in, and
                            // the brush stayed held until the next press.
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(|this, e: &MouseUpEvent, _, cx| {
                                    if !this.listening.get() && this.brush.is_some() {
                                        this.release(f32::from(e.position.x), cx);
                                    }
                                }),
                            )
                            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                if !*hovered {
                                    this.set_hover(None, cx);
                                }
                            }))
                            .children(drag_listeners),
                    )
                    .child(div().relative().h(px(14.0)).w_full().text_color(rgba(p.muted)).children(labels))
                    .into_any_element()
            }
            Kind::Bars { categories } => {
                // Each row, in a set and order that never change while
                // filtering: a check column (reserved on every row), the
                // label, the ghost (every row) in grey with what passes in
                // colour over it, and the value in tabular figures in a
                // fixed column. A pick wears the check; a row this chart's
                // own filter leaves out fades but keeps its value. The
                // pointer (on the card or the map) lights the row's
                // background only: never a colour, never bold.
                let rows = categories.iter().enumerate().map(|(i, c)| {
                    let v = self.totals.get(i).copied().unwrap_or(0.0);
                    let g = self.ghost.get(i).copied().filter(|_| ghosted).unwrap_or(0.0);
                    let lit = self.hover == Some(i) || self.map_hover == Some(i);
                    let picked = matches!(self.selection, Selection::Bars(_)) && self.selected(i);
                    let out = any && !self.selected(i);
                    let colour = colour_of(i, c.color);
                    // By field and row: a page with several cards finds each row.
                    let sel = format!("linked-chart-row-{}-{i}", self.field);
                    div()
                        .debug_selector(move || sel.clone())
                        .h(px(ROW_H))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .rounded(px(3.0))
                        .when(lit, |d| d.bg(rgba(p.grid)))
                        .child(
                            div()
                                .w(px(CHECK_W))
                                .flex_none()
                                .flex()
                                .justify_center()
                                .text_color(rgba(p.bar_in))
                                .when(picked, |d| d.child("\u{2713}")),
                        )
                        .child(
                            div()
                                .w(px(LABEL_W))
                                .flex_none()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(rgba(if out { p.muted } else { p.text }))
                                .child(c.label.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .h(px(12.0))
                                .relative()
                                .when(g > 0.0, |d| d.child(div().absolute().top_0().left_0().h_full().w(relative(bar_fraction(g, max))).rounded(px(2.0)).bg(rgba(p.bar))))
                                .child(div().absolute().top_0().left_0().h_full().w(relative(bar_fraction(v, max))).rounded(px(2.0)).bg(rgba(colour))),
                        )
                        .child(
                            div()
                                .w(px(VALUE_W))
                                .flex_none()
                                .flex()
                                .justify_end()
                                .font_features(tabular())
                                .text_color(rgba(if out { p.muted } else { p.text }))
                                .child(short(v, self.spec.fmt)),
                        )
                });
                div()
                    .id("linked-chart-plot")
                    .debug_selector(|| "linked-chart-plot".to_string())
                    .relative()
                    .w_full()
                    .cursor_pointer()
                    .child(probe)
                    .child(div().flex().flex_col().children(rows))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _, cx| {
                            this.press(f32::from(e.position.x), f32::from(e.position.y), cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| this.pointer(f32::from(e.position.x), f32::from(e.position.y), cx)))
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered {
                            this.set_hover(None, cx);
                        }
                    }))
                    .into_any_element()
            }
        };
        let total: f64 = self.totals.iter().sum();
        // A value with what it is in: a count's noun, or a sum's unit.
        let with_unit = |v: f64| match &self.value_unit {
            Some(u) => attach_unit(&short(v, self.spec.fmt), u),
            None => format!("{} {}", short(v, self.spec.fmt), self.spec.noun),
        };
        let axis_unit = self.axis_unit.clone().unwrap_or_default();
        let summary = match (&self.selection, &self.kind) {
            // The brushed range, with its unit, and what it keeps.
            (Selection::Range(r), Kind::Histogram { .. }) => {
                let range = match &self.time_text {
                    Some(f) => format!("{}\u{2013}{}", f(r[0]), f(r[1])),
                    None => range_text(*r, &axis_unit),
                };
                format!("{range} \u{b7} {} of {}", short(self.selected_total(), self.spec.fmt), with_unit(total))
            }
            _ if any => format!("{} of {}", short(self.selected_total(), self.spec.fmt), with_unit(total)),
            // Another chart's filter: what passes of every row (the ghosts).
            _ if ghosted && (self.ghost.iter().sum::<f64>() - total).abs() > 1e-9 => format!("{} of {}", short(total, self.spec.fmt), with_unit(self.ghost.iter().sum())),
            _ => with_unit(total),
        };
        let readout = self.hover.or(self.map_hover).map(|i| {
            let v = self.totals.get(i).copied().unwrap_or(0.0);
            let what = match &self.kind {
                Kind::Histogram { axis, bins } if self.time_text.is_some() => self.time_text.as_ref().map(|f| f(bin_range(axis, *bins, i)[0])).unwrap_or_default(),
                Kind::Histogram { axis, bins } => match self.spec.fmt {
                    NumFmt::Auto | NumFmt::Int | NumFmt::Compact => range_text(bin_range(axis, *bins, i), &axis_unit),
                    fmt => {
                        let [a, b] = bin_range(axis, *bins, i);
                        format!("{} to {}", short(a, fmt), short(b, fmt))
                    }
                },
                Kind::Bars { categories } => categories.get(i).map(|c| c.label.clone()).unwrap_or_default(),
            };
            format!("{what}: {}", with_unit(v))
        });
        // While a brush is held the range it marks reads live.
        let readout = if self.brush.is_some() { None } else { readout };
        // Over the bars, until the reader's first press: what a click does.
        let readout = match (&self.kind, readout, &self.hint_once) {
            (Kind::Bars { .. }, Some(r), Some(h)) if !HINT_SEEN.load(std::sync::atomic::Ordering::Relaxed) => Some(format!("{r} \u{b7} {h}")),
            (_, r, _) => r,
        };
        // Beside the open chat on a narrow map, folded (unfolds when the
        // chat closes; the user's own fold state is untouched).
        let collapsed = self.layout_folded.unwrap_or(self.collapsed) || (!self.docked && squeezed_by_chat(CARD_W));
        let entity = cx.entity().downgrade();
        let fold = fold_button("linked-chart-fold", collapsed, cx, move |_, cx| {
            let _ = entity.update(cx, |this, cx| {
                // Unfolding a card the layout folded pins it open; folding
                // one is the reader's own fold.
                if collapsed {
                    this.collapsed = false;
                    this.pinned_open = true;
                    this.layout_folded = None;
                } else {
                    this.collapsed = true;
                    this.pinned_open = false;
                }
                cx.notify();
            });
        });
        let entity = cx.entity().downgrade();
        // "Show all" keeps its slot whether or not anything is selected
        // (transparent and inert when nothing is), so selecting a bar
        // never squeezes the title beside it.
        let clear = (!collapsed).then(|| {
            div()
                .id("linked-chart-clear")
                .debug_selector(|| "linked-chart-clear".to_string())
                .flex_none()
                .px(px(6.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(rgba(p.grid))
                .text_color(rgba(p.text))
                .when(!any, |d| d.opacity(0.0))
                .child("Show all")
                .when(any, |d| {
                    d.cursor_pointer().on_click(move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| this.select(Selection::All, cx));
                    })
                })
        });
        if self.embedded {
            return div()
                .id("linked-chart")
                .debug_selector(|| "linked-chart".to_string())
                .w_full()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .text_color(rgba(p.text))
                .text_xs()
                .child(
                    div()
                        .debug_selector(|| "linked-chart-readout".to_string())
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(rgba(p.muted))
                        // The slot is always there, always one line: the
                        // summary, or what the pointer is over.
                        .child(readout.unwrap_or_else(|| summary.clone()))
                        .h(px(14.0)),
                )
                .child(plot)
                .when(!self.spec.unit.is_empty(), |c| c.child(div().text_color(rgba(p.muted)).child(self.spec.unit.clone())));
        }
        div()
            .id("linked-chart")
            .debug_selector(|| "linked-chart".to_string())
            .occlude()
            .when(!self.docked, |d| d.absolute().top(px(stack_top_px())).right(px(right_card_right_px())).w(px(CARD_W)))
            .when(self.docked, |d| d.w_full())
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(10.0))
            .rounded(px(8.0))
            .bg(rgba(p.bg))
            .border_1()
            .border_color(rgba(p.grid))
            .shadow_md()
            .text_color(rgba(p.text))
            .text_xs()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    // The title (and, folded, the summary) take the room
                    // left and cut with an ellipsis; the buttons keep their
                    // width inside the card.
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .overflow_hidden()
                            .child(div().flex_none().font_weight(gpui::FontWeight::SEMIBOLD).child(self.spec.title.clone()))
                            .when(collapsed, |r| {
                                r.child(div().min_w(px(0.0)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgba(p.muted)).child(summary.clone()))
                            }),
                    )
                    .children(clear)
                    .child(fold),
            )
            .when(!collapsed, |c| {
                // One line, cut rather than wrapped: a selection's longer
                // "N of M" never adds a row and pushes the plot down.
                c.child(
                    div()
                        .debug_selector(|| "linked-chart-summary".to_string())
                        .w_full()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(rgba(p.muted))
                        .child(readout.unwrap_or(summary)),
                )
                    .child(plot)
                    // No unit, no blank line under the plot.
                    .when(!self.spec.unit.is_empty(), |c| c.child(div().text_color(rgba(p.muted)).child(self.spec.unit.clone())))
                    .children(self.spec.hint.clone().map(|h| div().text_color(rgba(p.muted)).child(h)))
            })
    }
}

// ---------------------------------------------------------------------------
// The screenshot probes
// ---------------------------------------------------------------------------

fn env_pair(name: &str) -> Option<(f64, Option<f64>)> {
    let v = std::env::var(name).ok()?;
    let mut it = v.split(',').map(|s| s.trim().parse::<f64>().ok());
    let a = it.next()??;
    Some((a, it.next().flatten()))
}

/// Starts the brush and hover probes, if set, once the chart has rows and
/// has been laid out.
fn start_probes(chart: WeakEntity<LinkedChart>, cx: &mut App) {
    let brush = env_pair("GPUI_MAP_STUDIO_PROBE_BRUSH");
    let hover = env_pair("GPUI_MAP_STUDIO_PROBE_CHART_HOVER").map(|(a, _)| a);
    let brush_drag = env_pair("GPUI_MAP_STUDIO_PROBE_BRUSH_DRAG").and_then(|(a, b)| Some((a, b?)));
    if brush.is_none() && hover.is_none() && brush_drag.is_none() {
        return;
    }
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            if Instant::now() > deadline {
                eprintln!("chart probe: no laid-out chart with rows after 40 s");
                return;
            }
            let Ok((ready, (bx, by, bw, bh), bars, n)) =
                chart.update(cx, |c, _| (c.rows() > 0 && c.bounds.get().2 > 1.0, c.bounds.get(), matches!(c.kind, Kind::Bars { .. }), c.totals.len()))
            else {
                return;
            };
            if !ready {
                continue;
            }
            // One more beat: the page lays out once more with its data.
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            let Some(handle) = cx.update(|cx| cx.active_window().or_else(|| cx.windows().first().copied())) else { return };
            let at_frac = |f: f64| point(px(bx + bw * f as f32), px(by + bh * 0.6));
            let at_row = |i: f64| point(px(bx + bw * 0.5), px(by + bh * ((i as f32 + 0.5) / n.max(1) as f32)));
            let (b, m) = (MouseButton::Left, gpui::Modifiers::default());
            if let Some((a, second)) = brush {
                let _ = handle.update(cx, |_, window, cx| {
                    if bars {
                        for i in std::iter::once(a).chain(second) {
                            let pos = at_row(i);
                            window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { button: b, position: pos, modifiers: m, click_count: 1, first_mouse: false }), cx);
                            window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { button: b, position: pos, modifiers: m, click_count: 1 }), cx);
                        }
                    } else {
                        let from = at_frac(a);
                        window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { button: b, position: from, modifiers: m, click_count: 1, first_mouse: false }), cx);
                        window.refresh();
                    }
                });
                if !bars {
                    // The drag listeners install on the next frame.
                    cx.background_executor().timer(Duration::from_millis(100)).await;
                    let to = at_frac(second.unwrap_or(a));
                    let _ = handle.update(cx, |_, window, cx| {
                        window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: to, pressed_button: Some(b), modifiers: m }), cx);
                        window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { button: b, position: to, modifiers: m, click_count: 1 }), cx);
                    });
                }
                let _ = chart.update(cx, |c, cx| {
                    let window = c.map.upgrade().and_then(|m| {
                        let m = m.read(cx);
                        m.source_filter_ranges(&c.source).map(|r| format!("{r:?}")).or_else(|| m.source_filter_range(&c.source).map(|r| format!("{r:?}")))
                    });
                    eprintln!("chart probe: brushed {:?}, {} writes, map window {window:?}", c.selection, c.filter_writes);
                });
            }
            // A second gesture on the brushed histogram: press at `from`,
            // drag in steps to `to`, release -- inside the window moves it,
            // at an edge resizes it (the brush rules).
            if let (Some((from, to)), false) = (brush_drag, bars) {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                let before = chart.update(cx, |c, _| c.selection.clone()).ok();
                let _ = handle.update(cx, |_, window, cx| {
                    window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { button: b, position: at_frac(from), modifiers: m, click_count: 1, first_mouse: false }), cx);
                    window.refresh();
                });
                for k in 1..=6 {
                    cx.background_executor().timer(Duration::from_millis(60)).await;
                    let at = at_frac(from + (to - from) * f64::from(k) / 6.0);
                    let _ = handle.update(cx, |_, window, cx| {
                        window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: at, pressed_button: Some(b), modifiers: m }), cx);
                    });
                }
                let _ = handle.update(cx, |_, window, cx| {
                    window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { button: b, position: at_frac(to), modifiers: m, click_count: 1 }), cx);
                });
                let _ = chart.update(cx, |c, _| eprintln!("chart probe: brush drag {from} -> {to}: {before:?} -> {:?}, {} writes", c.selection, c.filter_writes));
            }
            if let Some(f) = hover {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                let pos = if bars { at_row(f) } else { at_frac(f) };
                let _ = handle.update(cx, |_, window, cx| {
                    window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: pos, pressed_button: None, modifiers: m }), cx);
                });
                let _ = chart.update(cx, |c, cx| {
                    let lit = c.map.upgrade().and_then(|m| m.read(cx).highlight_where(&c.source).map(|(_, n)| n));
                    eprintln!("chart probe: hover slot {:?}, {:?} features lit", c.hover, lit);
                });
            }
            return;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dark_ramp_class_is_lifted_on_a_dark_panel() {
        let lifted = readable_on(0x3b0f4f, true);
        let luma = |c: u32| 0.2126 * ((c >> 16) & 0xff) as f64 + 0.7152 * ((c >> 8) & 0xff) as f64 + 0.0722 * (c & 0xff) as f64;
        assert!(luma(lifted) > luma(0x3b0f4f) * 2.0, "{lifted:06x}");
        assert_eq!(readable_on(0x3b0f4f, false), 0x3b0f4f, "dark on light reads");
        assert_eq!(readable_on(0xe55c30, true), 0xe55c30, "a mid colour is kept");
        assert_eq!(tick_text(14.8e12), "14.8T");
        let a = Axis::edges(&[0.0, 31.0, 59.0, 90.0]).expect("edges");
        assert_eq!(bin_of(&a, 3, 31.0), Some(1));
        assert_eq!(bin_of(&a, 3, 58.9), Some(1));
        assert_eq!(bin_range(&a, 3, 2), [59.0, 90.0]);
    }

    #[test]
    fn a_log_axis_ticks_at_powers_of_ten_and_reads_them_short() {
        let axis = Axis::log_of([473.0, 1_770_430.0]).expect("positive data takes a log axis");
        let t: Vec<f64> = ticks(&axis, 4).into_iter().map(|(_, v)| v).collect();
        assert_eq!(t, vec![1e3, 1e4, 1e5, 1e6]);
        let text: Vec<String> = t.iter().map(|v| tick_text(*v)).collect();
        assert_eq!(text, ["1k", "10k", "100k", "1M"]);
        assert!(Axis::log_of([0.0, 10.0]).is_none(), "a zero cannot sit on a log axis");
        // Under two powers inside: 1-2-5 steps.
        let narrow = Axis::log_of([120.0, 900.0]).unwrap();
        let t: Vec<f64> = ticks(&narrow, 4).into_iter().map(|(_, v)| v).collect();
        assert_eq!(t, vec![200.0, 500.0]);
    }

    #[test]
    fn quantile_bins_hold_about_as_many_values_each() {
        let values: Vec<f64> = (1..=100).map(|i| f64::from(i * i)).collect();
        let axis = Axis::quantile(&values, 4).expect("enough values");
        let Scale::Quantile(q) = axis.scale else { panic!("a quantile axis") };
        assert_eq!(q.bins(), 4);
        let h = histogram(&values, None, &axis, q.bins());
        assert!(h.iter().all(|n| (20.0..=30.0).contains(n)), "{h:?}");
        // The axis maps an edge to its index and back.
        let e = q.edges().to_vec();
        assert!((axis.fraction(e[2]) - 0.5).abs() < 1e-9);
        assert!((axis.value(0.5) - e[2]).abs() < 1e-9);
        assert!(Axis::quantile(&[3.0, 3.0, 3.0], 4).is_none(), "one value has no quantiles");
    }

    #[test]
    fn units_attach_the_way_they_are_written() {
        assert_eq!(attach_unit("12.4k", "passengers/day"), "12.4k passengers/day");
        assert_eq!(attach_unit("12", "%"), "12%");
        assert_eq!(attach_unit("1,200", "\u{a5}"), "\u{a5}1,200");
        assert_eq!(attach_unit("-3", "$"), "-$3");
        assert_eq!(attach_unit("3", ""), "3");
        assert_eq!(range_text([10_000.0, 70_000.0], "passengers/day"), "10k\u{2013}70k passengers/day");
    }

    #[test]
    fn values_bin_evenly_and_the_top_edge_is_in_the_last_bin() {
        let axis = Axis::linear([0.0, 100.0]);
        assert_eq!(bin_of(&axis, 10, 0.0), Some(0));
        assert_eq!(bin_of(&axis, 10, 9.99), Some(0));
        assert_eq!(bin_of(&axis, 10, 10.0), Some(1));
        assert_eq!(bin_of(&axis, 10, 100.0), Some(9), "the domain's top is in the last bin");
        assert_eq!(bin_of(&axis, 10, 100.1), None);
        assert_eq!(bin_of(&axis, 10, f64::NAN), None);
        let h = histogram(&[1.0, 2.0, 55.0, 100.0, f64::NAN, -3.0], None, &axis, 10);
        assert_eq!(h, vec![2.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
        let w = histogram(&[1.0, 2.0, 55.0], Some(&[10.0, 5.0, 1.0]), &axis, 2);
        assert_eq!(w, vec![15.0, 1.0], "weights sum");
    }

    #[test]
    fn a_log_axis_bins_by_decade() {
        let axis = Axis::log10([1e5, 1e8]);
        assert_eq!(bin_of(&axis, 3, 2e5), Some(0));
        assert_eq!(bin_of(&axis, 3, 1e6), Some(1));
        assert_eq!(bin_of(&axis, 3, 5e7), Some(2));
        let [lo, hi] = bin_range(&axis, 3, 1);
        assert!((lo - 1e6).abs() < 1e-3 && (hi - 1e7).abs() < 1e-2, "{lo} {hi}");
        let t: Vec<f64> = ticks(&axis, 4).into_iter().map(|(_, v)| v).collect();
        assert_eq!(t.len(), 4);
        assert!((t[0] - 1e5).abs() < 1e-6 && (t[3] - 1e8).abs() < 1.0, "{t:?}");
    }

    #[test]
    fn a_brush_snaps_out_to_whole_bins_in_either_direction() {
        let axis = Axis::linear([0.0, 100.0]);
        assert_eq!(brush_to_range(&axis, 10, 0.25, 0.42), [20.0, 50.0]);
        assert_eq!(brush_to_range(&axis, 10, 0.42, 0.25), [20.0, 50.0], "a leftward drag is the same range");
        assert_eq!(brush_to_range(&axis, 10, -0.5, 2.0), [0.0, 100.0], "clamped into the axis");
        assert_eq!(brush_to_range(&axis, 10, 0.3, 0.3), [30.0, 40.0], "a zero-width brush is its bin");
        assert_eq!(brush_to_range(&axis, 10, 1.0, 1.0), [90.0, 100.0], "the right edge is the last bin");
        // And back: the bins drawn lit are exactly the brushed ones.
        assert_eq!(range_bins(&axis, 10, [20.0, 50.0]), (2, 4));
        assert_eq!(range_bins(&axis, 10, [90.0, 100.0]), (9, 9));
        let log = Axis::log10([1e5, 1e8]);
        let r = brush_to_range(&log, 6, 0.4, 0.6);
        assert_eq!(range_bins(&log, 6, r), (2, 3));
    }

    #[test]
    fn linear_ticks_are_round() {
        let t: Vec<f64> = ticks(&Axis::linear([0.0, 250_000.0]), 4).into_iter().map(|(_, v)| v).collect();
        assert_eq!(t, vec![0.0, 50_000.0, 100_000.0, 150_000.0, 200_000.0, 250_000.0]);
        let t: Vec<f64> = ticks(&Axis::linear([3.0, 47.0]), 4).into_iter().map(|(_, v)| v).collect();
        assert_eq!(t, vec![10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    fn categories_count_or_sum_in_their_order() {
        let cats = vec![Category::new(TagValue::String("East".into()), "East"), Category::new(TagValue::Number(2.0), "Two")];
        let keys = vec![Some(TagValue::String("East".into())), Some(TagValue::Number(2.0)), Some(TagValue::String("East".into())), None, Some(TagValue::String("West".into()))];
        assert_eq!(category_totals(&keys, None, &cats), vec![2.0, 1.0]);
        assert_eq!(category_totals(&keys, Some(&[10.0, 3.0, 5.0, 7.0, 9.0]), &cats), vec![15.0, 3.0]);
    }

    // -- the component in a window ------------------------------------------

    fn window_with_chart(app: &mut gpui::TestAppContext, kind: Kind, link: Link) -> (Entity<MapView>, Entity<LinkedChart>, &mut gpui::VisualTestContext) {
        struct Host {
            chart: Entity<LinkedChart>,
            map: Entity<MapView>,
        }
        impl Render for Host {
            fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                // As a page lays it out: in the floating top-right stack.
                div().relative().size(px(800.0)).child(card_stack(vec![self.chart.clone().into_any_element()]))
            }
        }
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let handle = rt.handle().clone();
        std::mem::forget(rt);
        let (host, cx) = app.add_window_view(|_, cx| {
            let style = gpui_maps::style::Style { layers: vec![], sprite: None, sky: Default::default() };
            let map = cx.new(|_| MapView::new(0.0, 0.0, 2.0, 800.0, 600.0, style, std::sync::Arc::new(gpui_maps::tile_source::EmptyTileSource), handle));
            let lon_lat: Vec<[f64; 2]> = (0..100).map(|i| [f64::from(i) * 0.1, 0.0]).collect();
            let pop: Vec<Option<f64>> = (0..100).map(|i| Some(f64::from(i))).collect();
            let cat: Vec<Option<f64>> = (0..100).map(|i| Some(f64::from(i % 4))).collect();
            map.update(cx, |m, cx| {
                m.set_source_points(
                    "pts",
                    &lon_lat,
                    None,
                    &gpui_maps::overlay::FeaturePaint::default(),
                    &[("pop", gpui_maps::overlay::PropertyColumn::Number(&pop)), ("cat", gpui_maps::overlay::PropertyColumn::Number(&cat))],
                    cx,
                )
                .expect("pushed");
            });
            let field = if matches!(kind, Kind::Bars { .. }) { "cat" } else { "pop" };
            let spec = ChartSpec { title: "T".into(), unit: "u".into(), noun: "points".into(), fmt: NumFmt::Int, hint: None };
            let chart = cx.new(|cx| LinkedChart::new(&map, "pts", field, kind, spec, link, cx).docked());
            Host { chart, map }
        });
        let (map, chart) = host.read_with(cx, |h, _| (h.map.clone(), h.chart.clone()));
        cx.run_until_parked();
        (map, chart, cx)
    }

    /// A brush on the plot reaches the map as a filter WINDOW -- one
    /// uniform write per move -- and never as a push: the source's data
    /// stamp does not move.
    #[test]
    fn a_brush_writes_the_filter_window_and_never_re_pushes() {
        let mut app = gpui::TestAppContext::single();
        let (map, chart, cx) = window_with_chart(&mut app, Kind::Histogram { axis: Axis::linear([0.0, 100.0]), bins: 10 }, Link::GpuRange);
        assert_eq!(chart.read_with(cx, |c, _| c.totals().to_vec()), vec![10.0; 10], "100 rows read from the map");
        // The filter column's view rebuild lands off the UI thread: wait
        // for the stamp to hold still before taking it.
        let mut generation = map.read_with(cx, |m, _| m.source_generation("pts"));
        let mut still = 0;
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(10));
            cx.run_until_parked();
            let now = map.read_with(cx, |m, _| m.source_generation("pts"));
            still = if now == generation && map.read_with(cx, |m, _| m.source_point_index_ready("pts")) { still + 1 } else { 0 };
            generation = now;
            if still >= 20 {
                break;
            }
        }
        let writes = map.read_with(cx, |m, _| m.source_filter_range_writes("pts"));
        let plot = cx.debug_bounds("linked-chart-plot").expect("the plot was drawn");
        let at = |f: f32| point(plot.origin.x + plot.size.width * f, plot.origin.y + plot.size.height / 2.0);
        cx.simulate_mouse_down(at(0.22), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(at(0.35), Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_move(at(0.48), Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(at(0.48), MouseButton::Left, gpui::Modifiers::default());
        let window = map.read_with(cx, |m, _| m.source_filter_range("pts"));
        assert_eq!(window, Some([20.0, 50.0]), "snapped out to whole bins");
        assert_eq!(chart.read_with(cx, |c, _| c.selected_total()), 30.0);
        assert!(map.read_with(cx, |m, _| m.source_filter_range_writes("pts")) > writes, "the window moved");
        assert_eq!(map.read_with(cx, |m, _| m.source_generation("pts")), generation, "and nothing was pushed again");
        // A click inside the selection changes nothing (d3-brush; "Show
        // all" clears).
        cx.simulate_mouse_down(at(0.3), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(at(0.3), MouseButton::Left, gpui::Modifiers::default());
        assert_eq!(map.read_with(cx, |m, _| m.source_filter_range("pts")), Some([20.0, 50.0]));
        // A drag from inside moves the window, its width kept; snapped to
        // the nearest bin edges on release.
        cx.simulate_mouse_down(at(0.35), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(at(0.45), Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_move(at(0.55), Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(at(0.55), MouseButton::Left, gpui::Modifiers::default());
        assert_eq!(map.read_with(cx, |m, _| m.source_filter_range("pts")), Some([40.0, 70.0]), "moved 20, still 30 wide");
        assert_eq!(map.read_with(cx, |m, _| m.source_generation("pts")), generation);
    }

    /// A press, a drag and a release past the plot's end in one burst (a
    /// flick, or a debug-port drag): the brush lets go, keeping the window
    /// the drag reached. (The test platform draws between simulated
    /// events, so this guards the release-outside path rather than
    /// reproducing a frameless burst.)
    #[test]
    fn a_release_outside_the_plot_before_a_frame_lets_the_brush_go() {
        let mut app = gpui::TestAppContext::single();
        let (map, chart, cx) = window_with_chart(&mut app, Kind::Histogram { axis: Axis::linear([0.0, 100.0]), bins: 10 }, Link::GpuRange);
        let plot = cx.debug_bounds("linked-chart-plot").expect("the plot was drawn");
        let at = |f: f32| point(plot.origin.x + plot.size.width * f, plot.origin.y + plot.size.height / 2.0);
        cx.simulate_mouse_down(at(0.42), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(at(0.7), Some(MouseButton::Left), gpui::Modifiers::default());
        let past = point(plot.origin.x + plot.size.width * 1.4, plot.origin.y + plot.size.height * 3.0);
        cx.simulate_mouse_up(past, MouseButton::Left, gpui::Modifiers::default());
        assert!(chart.read_with(cx, |c, _| c.brush.is_none()), "the brush is not left held");
        assert_eq!(map.read_with(cx, |m, _| m.source_filter_range("pts")), Some([40.0, 70.0]), "the window the drag reached");
        // The next press starts a new brush rather than continuing a held one.
        cx.simulate_mouse_down(at(0.05), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(at(0.25), Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(at(0.25), MouseButton::Left, gpui::Modifiers::default());
        assert_eq!(map.read_with(cx, |m, _| m.source_filter_range("pts")), Some([0.0, 30.0]));
    }

    /// Hovering a bar asks the map to light every feature in it (the
    /// predicate highlight); leaving the chart clears it.
    #[test]
    fn hovering_a_bar_lights_its_features_on_the_map() {
        let mut app = gpui::TestAppContext::single();
        let cats = (0..4).map(|i| Category::new(TagValue::Number(f64::from(i)), &format!("Cat {i}"))).collect();
        let (map, chart, cx) = window_with_chart(&mut app, Kind::Bars { categories: cats }, Link::GpuRange);
        assert_eq!(chart.read_with(cx, |c, _| c.totals().to_vec()), vec![25.0; 4]);
        let plot = cx.debug_bounds("linked-chart-plot").expect("the plot was drawn");
        let row = |i: f32| point(plot.origin.x + plot.size.width / 2.0, plot.origin.y + px(ROW_H * (i + 0.5)));
        cx.simulate_mouse_move(row(2.0), None, gpui::Modifiers::default());
        assert_eq!(chart.read_with(cx, |c, _| c.hover), Some(2));
        let asked = map.read_with(cx, |m, _| m.highlight_where("pts").map(|(p, _)| p.clone()));
        assert_eq!(asked, Some(HighlightWhere::one_of("cat", vec![TagValue::Number(2.0)])), "bar 2's predicate");
        cx.simulate_mouse_move(point(px(790.0), px(790.0)), None, gpui::Modifiers::default());
        assert_eq!(chart.read_with(cx, |c, _| c.hover), None);
        assert!(map.read_with(cx, |m, _| m.highlight_where("pts").is_none()), "off the chart, nothing lit");
        // A click on a bar filters to it (a numeric code: the GPU window).
        cx.simulate_mouse_down(row(1.0), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(row(1.0), MouseButton::Left, gpui::Modifiers::default());
        assert_eq!(map.read_with(cx, |m, _| m.source_filter_range("pts")), Some([1.0, 1.0]));
    }

    /// A map hover (the popup's pick) lights the bin holding the object.
    #[test]
    fn a_map_hover_lights_its_bin() {
        let mut app = gpui::TestAppContext::single();
        let (map, chart, cx) = window_with_chart(&mut app, Kind::Histogram { axis: Axis::linear([0.0, 100.0]), bins: 10 }, Link::None);
        let popup = cx.update(|_, cx| cx.new(|cx| crate::popup::MapPopup::new(&map, crate::popup::PopupConfig::new(), cx)));
        chart.update(cx, |c, cx| c.follow_popup(&popup, cx));
        let props: gpui_maps::overlay::FeatureProperties = [("pop".to_string(), TagValue::Number(57.0))].into_iter().collect();
        let pick = gpui_maps::map_view::Pick {
            object: gpui_maps::map_view::MapObject::Feature(gpui_maps::overlay::FeatureRef { source: "pts".into(), index: 57, id: None }),
            properties: Some(std::sync::Arc::new(props)),
        };
        popup.update(cx, |p, cx| p.set_hover(Some(pick), cx));
        cx.run_until_parked();
        assert_eq!(chart.read_with(cx, |c, _| c.map_hover), Some(5));
        popup.update(cx, |p, cx| p.set_hover(None, cx));
        cx.run_until_parked();
        assert_eq!(chart.read_with(cx, |c, _| c.map_hover), None);
    }

    /// The user: buttons "are going outside divs where they are located".
    /// With a long title and a selection, the header's Show all and fold
    /// buttons lie inside the chart card, and the card inside the stack's
    /// width; folded, the card is its header alone and the buttons still
    /// sit inside it.
    #[test]
    fn the_header_buttons_stay_inside_the_card() {
        let mut app = gpui::TestAppContext::single();
        let (_map, chart, cx) = window_with_chart(&mut app, Kind::Bars { categories: (0..4).map(|i| Category::new(TagValue::Number(f64::from(i)), &format!("c{i}"))).collect() }, Link::GpuRange);
        chart.update(cx, |c, cx| {
            c.spec.title = "Passengers per trip, every borough, the whole of January".into();
            cx.notify();
        });
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
        };
        draw(cx);
        // Click bar 1, as a reader would: Show all appears.
        let plot = cx.debug_bounds("linked-chart-plot").expect("the plot");
        let row = point(plot.origin.x + plot.size.width / 2.0, plot.origin.y + px(ROW_H * 1.5));
        cx.simulate_click(row, gpui::Modifiers::default());
        draw(cx);
        assert!(!matches!(chart.read_with(cx, |c, _| c.selection.clone()), Selection::All), "the click selected a bar");
        let inside = |outer: gpui::Bounds<gpui::Pixels>, inner: gpui::Bounds<gpui::Pixels>| {
            inner.origin.x >= outer.origin.x
                && inner.origin.y >= outer.origin.y
                && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width
                && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height
        };
        for folded in [false, true] {
            chart.update(cx, |c, cx| {
                c.collapsed = folded;
                cx.notify();
            });
            draw(cx);
            let card = cx.debug_bounds("linked-chart").expect("the card");
            let stack = cx.debug_bounds("chart-stack").expect("the stack");
            assert!((f32::from(card.size.width) - CARD_W).abs() < 0.5, "the card is the stack's width: {card:?}");
            assert!(inside(stack, card), "folded {folded}: card {card:?} outside the stack {stack:?}");
            // Folded, Show all goes with the plot; the fold stays.
            let ids: &[&str] = if folded { &["linked-chart-fold"] } else { &["linked-chart-clear", "linked-chart-fold"] };
            for &id in ids {
                let b = cx.debug_bounds(id).unwrap_or_else(|| panic!("folded {folded}: {id} drawn"));
                assert!(inside(card, b), "folded {folded}: {id} {b:?} outside the card {card:?}");
            }
            let plot = cx.debug_bounds("linked-chart-plot");
            assert_eq!(plot.is_none(), folded, "folded hides the plot");
        }
    }

    /// Pressing never changes the card's layout: a bar click (the
    /// summary becomes "N of M", "Show all" goes live) and "Show all"
    /// leave the card, the plot, the summary line and both header buttons
    /// exactly where they were.
    #[test]
    fn pressing_a_bar_or_show_all_never_moves_the_layout() {
        let mut app = gpui::TestAppContext::single();
        let (_map, chart, cx) = window_with_chart(&mut app, Kind::Bars { categories: (0..4).map(|i| Category::new(TagValue::Number(f64::from(i)), &format!("c{i}"))).collect() }, Link::GpuRange);
        chart.update(cx, |c, cx| {
            c.spec.title = "Passengers per trip, every borough, the whole of January".into();
            c.spec.noun = "passengers counted on every trip this month".into();
            cx.notify();
        });
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
        };
        draw(cx);
        let ids = ["linked-chart", "linked-chart-plot", "linked-chart-summary", "linked-chart-clear", "linked-chart-fold"];
        let measure = |cx: &mut gpui::VisualTestContext| ids.map(|id| cx.debug_bounds(id).unwrap_or_else(|| panic!("{id} drawn")));
        let before = measure(cx);
        let plot = before[1];
        let row = point(plot.origin.x + plot.size.width / 2.0, plot.origin.y + px(ROW_H * 1.5));
        cx.simulate_click(row, gpui::Modifiers::default());
        draw(cx);
        assert!(!matches!(chart.read_with(cx, |c, _| c.selection.clone()), Selection::All), "the click selected a bar");
        assert_eq!(measure(cx), before, "selecting a bar moved the layout");
        let clear = before[3];
        cx.simulate_click(point(clear.origin.x + clear.size.width / 2.0, clear.origin.y + clear.size.height / 2.0), gpui::Modifiers::default());
        draw(cx);
        assert!(matches!(chart.read_with(cx, |c, _| c.selection.clone()), Selection::All), "Show all cleared it");
        assert_eq!(measure(cx), before, "Show all moved the layout");
    }

    /// The stack sits one corner inset in from the right, its top one
    /// inset below the compass's disc.
    #[test]
    fn the_stack_clears_the_compass() {
        let mut app = gpui::TestAppContext::single();
        let (_map, _chart, cx) = window_with_chart(&mut app, Kind::Histogram { axis: Axis::linear([0.0, 100.0]), bins: 10 }, Link::GpuRange);
        let stack = cx.debug_bounds("chart-stack").expect("the stack");
        let (cl, ct, cs) = gpui_maps::compass::compass_rect(800.0, 0.0);
        assert!(f32::from(stack.origin.y) >= ct + cs, "the stack's top {:?} is under the compass (bottom {})", stack.origin.y, ct + cs);
        assert!((800.0 - (f32::from(stack.origin.x) + f32::from(stack.size.width)) - CORNER_INSET_PX).abs() < 0.5);
        assert!(f32::from(stack.origin.x) < cl + cs, "the stack is the compass's column");
    }
}
