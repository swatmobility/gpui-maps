//! The "Breakdowns" panel: a collapsible card on the window's right with
//! three tabs over [`gpui_maps::census::breakdowns`] --
//!
//! - **States**: one row per state -- postal code, a 20-bar income strip on
//!   the viridis ramp, the weighted median, and the share of its people
//!   inside the slider's range (bar + percent). Sortable by median, by
//!   share, or by population; Puerto Rico apart at the bottom. Hovering a
//!   row dims every other state on the map; clicking flies there.
//! - **Metros**: a ridgeline of the 25 largest metros' income distributions
//!   on the slider's axis, by median, the slider's range a shaded band
//!   across every ridge, each metro's between-block-group Gini beside it.
//! - **In view**: the states and metros with the most people in view, from
//!   the throttled in-view breakdown.
//!
//! Every number a slider move changes is a [`breakdowns::Group::share_in_range`]
//! -- two binary searches a group -- so a drag never walks block groups.
//! The panel only ever notifies the demo's root: nothing here touches the
//! map except a click (a camera ease) and a state hover (a paint update).

use gpui_maps::census::breakdowns::{Breakdowns, Group, InView, InViewGroup, MercatorBounds, GINI_LABEL, TOP_METROS};
// The words, sort and order: shared with the browser's panel, tested there.
pub use gpui_maps::census::panel_json::{common_peak, money, percent, people, short_metro, sort_states, RidgeScale, StateSort};
use super::{income_fraction, thousands, viridis_at, DemoRoot, IncomeRange};
use gpui::prelude::*;
use gpui::{px, App, Context, WeakEntity, Window};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The panel's width, and its gap to the window's edges.
pub const PANEL_W: f32 = 340.0;
pub const PANEL_MARGIN: f32 = crate::linked_chart::CORNER_INSET_PX;

/// The panel's top: the floating cards' one top line, as the chart stack
/// (`linked_chart::stack_top_px`: below the compass where no host places
/// it, the shared line in Studio).
fn panel_top() -> f32 {
    crate::linked_chart::stack_top_px()
}

/// Its right edge's inset: a member of the right card column, so one gap
/// left of Studio's open chat card (`linked_chart::right_card_right_px`).
fn panel_right() -> f32 {
    crate::linked_chart::right_card_right_px()
}

/// Whether the panel is drawn open: the user's choice, unless the open
/// chat leaves too little map beside it (`linked_chart::squeezed_by_chat`),
/// when it shows as its tab until the chat closes.
fn shown_open(state: &PanelState) -> bool {
    state.open && !crate::linked_chart::squeezed_by_chat(PANEL_W)
}
/// The open panel's bottom: clear of the attribution plate in the map's
/// bottom-right corner (a card over it hides a licence credit).
pub const PANEL_BOTTOM: f32 = 48.0;
const PANEL_PAD: f32 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    States,
    Metros,
    InView,
}


/// What the panel remembers between renders.
#[derive(Clone, Debug)]
pub struct PanelState {
    pub open: bool,
    pub tab: Tab,
    pub sort: StateSort,
    /// The state row under the pointer (FIPS).
    pub hovered_state: Option<u32>,
}

impl Default for PanelState {
    fn default() -> Self {
        Self { open: true, tab: Tab::States, sort: StateSort::Median, hovered_state: None }
    }
}

/// The tightest a fly-to frames a state or a metro: the fit decides, this
/// only stops a tiny one (DC, ~120 px at z9) being framed from too far out
/// -- or a point from too close.
const STATE_MAX_ZOOM: f64 = 12.0;
const METRO_MAX_ZOOM: f64 = 12.0;

/// The panel as its own view, drawn by the demo root as a CACHED view: a
/// render of the root that is not about the panel (a tooltip following
/// the cursor) reuses the panel's last layout and paint instead of
/// rebuilding it. The root pushes what the panel shows (`set_range`,
/// `set_breakdowns`, `set_in_view`); the panel calls back into the root
/// for what touches the map (a state's dim, a fly-to).
pub struct PanelView {
    root: WeakEntity<DemoRoot>,
    pub state: PanelState,
    pub range: IncomeRange,
    pub breakdowns: Option<Arc<Breakdowns>>,
    /// Why there are no breakdowns, once that is known.
    pub error: Option<String>,
    pub in_view: Option<InView>,
    pub clock: PaintClock,
    pub stats: PanelStats,
    /// The right-column placement it was last drawn for
    /// ([`Self::sync_column`]): `(right inset, folded for the chat)`.
    column: (f32, bool),
}

impl PanelView {
    /// Run by the root every render: the panel is a CACHED view, so a
    /// change of its place in the right card column (Studio's chat opening
    /// or closing) must re-render it. Also registers the open panel with
    /// the popups' keep-out (`popup::reserve_right`), every frame.
    pub fn sync_column(&mut self, cx: &mut Context<Self>) {
        let open = shown_open(&self.state);
        crate::popup::reserve_right("breakdowns", if open { panel_right() + PANEL_W + PANEL_MARGIN } else { 0.0 });
        let column = (panel_right(), crate::linked_chart::squeezed_by_chat(PANEL_W));
        if column != self.column {
            self.column = column;
            cx.notify();
        }
    }

    pub fn new(root: WeakEntity<DemoRoot>, log: bool) -> Self {
        Self {
            root,
            state: PanelState::default(),
            range: IncomeRange::full(),
            breakdowns: None,
            error: None,
            in_view: None,
            clock: PaintClock::default(),
            stats: PanelStats { log, ..PanelStats::default() },
            column: (PANEL_MARGIN, false),
        }
    }

    /// Whether the in-view breakdown is on screen (the demo skips the
    /// regroup otherwise).
    pub fn shows_in_view(&self) -> bool {
        shown_open(&self.state) && self.state.tab == Tab::InView
    }

    /// Hovers a state row (`None`: none): remembered here, and the map
    /// dims every other state.
    pub fn point_at_state(&mut self, state: Option<u32>, cx: &mut Context<Self>) {
        self.state.hovered_state = state;
        let _ = self.root.update(cx, |root, cx| root.set_state_focus(state, cx));
    }

    fn fly(&mut self, bounds: MercatorBounds, max_zoom: f64, size: gpui::Size<gpui::Pixels>, cx: &mut Context<Self>) {
        let covered = if shown_open(&self.state) { PANEL_W + panel_right() } else { panel_right() - PANEL_MARGIN };
        let _ = self.root.update(cx, |root, cx| root.fly_to_bounds(bounds, max_zoom, size, covered, cx));
    }
}

impl Render for PanelView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::popup::sync_ink(cx);
        // Timed: the build now, and the paint of the canvases in the frame
        // before (they paint after this returns).
        let started = Instant::now();
        let element = panel(self, cx);
        let build = started.elapsed();
        let paint = self.clock.replace(Duration::ZERO);
        self.stats.record(build, paint);
        // The cached view's box is the whole window; the panel sits in it.
        gpui::div().size_full().child(element)
    }
}

/// The panel's cost per render: building its element tree and painting
/// its canvases (the frame before's, read at the next render). Collected
/// only when `log` is on (`--panel-sweep`), which prints one line per
/// phase with [`PanelStats::flush`]; silent otherwise.
#[derive(Default)]
pub struct PanelStats {
    pub log: bool,
    build: Vec<Duration>,
    paint: Vec<Duration>,
    marks: Option<(u64, u64, u64)>,
}

impl PanelStats {
    fn record(&mut self, build: Duration, paint: Duration) {
        if !self.log {
            return;
        }
        self.build.push(build);
        if paint > Duration::ZERO {
            self.paint.push(paint);
        }
    }

    /// One line: renders, build and paint p50/p95/max, and what the map did
    /// since the last line -- GPU passes submitted, window draws it skipped
    /// as unchanged (`MapView::set_skip_unchanged_frames`), and draw calls
    /// (0 draw calls over a phase that should draw: the window never
    /// painted, discard).
    pub fn flush(&mut self, label: &str) {
        let ms = |v: &mut Vec<Duration>| -> String {
            if v.is_empty() {
                return "-".to_string();
            }
            v.sort_unstable();
            let q = |f: f64| v[((v.len() - 1) as f64 * f).round() as usize].as_secs_f64() * 1e3;
            format!("p50 {:.2} p95 {:.2} max {:.2} ms", q(0.5), q(0.95), q(1.0))
        };
        let now = (
            gpui_maps::render::paint_census().paints_submitted,
            gpui_maps::render::alloc::paints_skipped_unchanged(),
            gpui_maps::render::allocation_stats().draw_calls_total,
        );
        let at = self.marks.unwrap_or(now);
        println!(
            "{label}: {} renders; build {}; canvas paint {}; map paints +{} skipped-unchanged +{} draw calls +{}",
            self.build.len(),
            ms(&mut self.build),
            ms(&mut self.paint),
            now.0 - at.0,
            now.1 - at.1,
            now.2 - at.2,
        );
        self.marks = Some(now);
        self.build.clear();
        self.paint.clear();
    }
}

// ---- pure pieces ------------------------------------------------------------

/// The slider's selection along a `width`-wide axis: `(x_lo, x_hi)`.
fn range_x(range: IncomeRange, width: f32) -> (f32, f32) {
    let stops = super::slider_stops();
    (income_fraction(stops[range.lo]) as f32 * width, income_fraction(stops[range.hi]) as f32 * width)
}

// ---- elements ---------------------------------------------------------------

/// A plain text tooltip (gpui's tooltips are views).
pub struct TextTip(pub gpui::SharedString);

impl Render for TextTip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::popup::sync_ink(cx);
        gpui::div()
            .max_w(px(260.0))
            .p(px(6.0))
            .bg(crate::popup::ink(0x1f2937))
            .rounded(px(4.0))
            .text_xs()
            .text_color(crate::popup::ink(0xffffff))
            .child(self.0.clone())
    }
}

fn tip(text: &'static str) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    move |_, cx| cx.new(|_| TextTip(text.into())).into()
}

/// Paint-time accounting for the panel's canvases, per frame.
pub type PaintClock = std::rc::Rc<std::cell::Cell<std::time::Duration>>;

/// A strip's `peak` that means "its own tallest bar".
const OWN_PEAK: f32 = 0.0;

/// A 20-bar strip: each bar the viridis colour of its incomes, dimmed
/// outside the slider's range (a bar cut by a handle is split there), and
/// a dark tick at the median.
fn strip(bins: &[f32], peak: f32, median: f64, range: IncomeRange, w: f32, h: f32, clock: &PaintClock) -> gpui::Canvas<()> {
    let bins = bins.to_vec();
    let peak = if peak > 0.0 { peak } else { common_peak(std::iter::once(bins.as_slice())) };
    let clock = clock.clone();
    let (sel_lo, sel_hi) = range_x(range, w);
    let median_x = income_fraction(median) as f32 * w;
    gpui::canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let started = std::time::Instant::now();
            let (ox, bottom) = (bounds.origin.x, bounds.origin.y + bounds.size.height);
            let n = bins.len().max(1) as f32;
            let edges = gpui_maps::census::breakdowns::default_edges();
            for (k, &b) in bins.iter().enumerate() {
                if b <= 0.0 || peak <= 0.0 {
                    continue;
                }
                let bh = ((b / peak).min(1.0) * h).max(1.0);
                let (x0, x1) = (k as f32 / n * w, (k as f32 + 1.0) / n * w - 0.5);
                let e = edges.edges();
                let color = viridis_at((e[k] + e[k + 1]) / 2.0);
                let cuts = [x0, sel_lo.clamp(x0, x1), sel_hi.clamp(x0, x1), x1];
                for (i, c) in cuts.windows(2).enumerate() {
                    if c[1] > c[0] {
                        let fill = if i == 1 { color } else { gpui::Rgba { a: 0.22, ..color } };
                        window.paint_quad(gpui::fill(
                            gpui::Bounds::new(gpui::point(ox + px(c[0]), bottom - px(bh)), gpui::size(px(c[1] - c[0]), px(bh))),
                            fill,
                        ));
                    }
                }
            }
            if median.is_finite() {
                window.paint_quad(gpui::fill(
                    gpui::Bounds::new(gpui::point(ox + px(median_x - 0.5), bounds.origin.y), gpui::size(px(1.0), bounds.size.height)),
                    crate::popup::ink(0x111827),
                ));
            }
            clock.set(clock.get() + started.elapsed());
        },
    )
}

/// The panel, or its collapsed tab.
fn panel(root: &PanelView, cx: &mut Context<PanelView>) -> gpui::AnyElement {
    let state = &root.state;
    let open = shown_open(state);
    if !open {
        return gpui::div()
            .id("breakdowns-open")
            .debug_selector(|| "breakdowns-open".to_string())
            .absolute()
            .top(px(panel_top()))
            .right(px(panel_right()))
            .px(px(10.0))
            .py(px(6.0))
            .bg(crate::popup::ink(0xffffff))
            .rounded(px(6.0))
            .shadow_md()
            .occlude()
            .cursor_pointer()
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(crate::popup::ink(0x1f2937))
            .child("Breakdowns ◂")
            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                this.state.open = true;
                cx.notify();
            }))
            .into_any_element();
    }
    let range = root.range;
    let tab_button = |tab: Tab, label: &'static str, cx: &mut Context<PanelView>| {
        let on = state.tab == tab;
        gpui::div()
            .id(label)
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_xs()
            .when(on, |d| d.bg(crate::popup::ink(0x1f2937)).text_color(crate::popup::ink(0xffffff)))
            .when(!on, |d| d.text_color(crate::popup::ink(0x374151)).hover(|s| s.bg(crate::popup::ink(0xf3f4f6))))
            .child(label)
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                this.state.tab = tab;
                this.point_at_state(None, cx);
                cx.notify();
            }))
    };
    let header = gpui::div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(gpui::div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(crate::popup::ink(0x1f2937)).child("Breakdowns"))
        .child(
            gpui::div()
                .id("breakdowns-close")
                .px(px(6.0))
                .cursor_pointer()
                .text_xs()
                .text_color(crate::popup::ink(0x6b7280))
                .child("hide ▸")
                .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.state.open = false;
                    this.point_at_state(None, cx);
                    cx.notify();
                })),
        );
    let tabs = gpui::div()
        .flex()
        .flex_row()
        .gap(px(4.0))
        .child(tab_button(Tab::States, "States", cx))
        .child(tab_button(Tab::Metros, "Metros", cx))
        .child(tab_button(Tab::InView, "In view", cx));
    let caption = gpui::div().text_xs().text_color(crate::popup::ink(0x6b7280)).child(if range.is_full() {
        "Slider: all incomes".to_string()
    } else {
        format!("Slider: {}", range.label())
    });

    let body: gpui::AnyElement = match root.breakdowns.as_deref() {
        None => gpui::div()
            .text_xs()
            .text_color(crate::popup::ink(0x6b7280))
            .child(match &root.error {
                Some(err) => format!("No breakdowns: {err}"),
                None => "Loading population and metro tables…".to_string(),
            })
            .into_any_element(),
        Some(b) => match state.tab {
            Tab::States => states_tab(b, state, range, &root.clock, cx).into_any_element(),
            Tab::Metros => metros_tab(b, range, &root.clock, cx).into_any_element(),
            Tab::InView => in_view_tab(b, root.in_view.as_ref(), range, &root.clock, cx).into_any_element(),
        },
    };
    let footnote = root.breakdowns.as_deref().map(footnote);
    gpui::div()
        .id("breakdowns")
        .debug_selector(|| "breakdowns".to_string())
        .absolute()
        .top(px(panel_top()))
        .right(px(panel_right()))
        .w(px(PANEL_W))
        .bottom(px(PANEL_BOTTOM))
        .p(px(PANEL_PAD))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .bg(crate::popup::ink_a(0xffff_fff2))
        .rounded(px(8.0))
        .shadow_md()
        .occlude()
        .child(header)
        .child(tabs)
        .child(caption)
        .child(gpui::div().id("breakdowns-body").flex_1().min_h(px(0.0)).overflow_y_scroll().child(body))
        .children(footnote)
        .into_any_element()
}

const INNER_W: f32 = PANEL_W - 2.0 * PANEL_PAD;
const ROW_H: f32 = 17.0;
const CODE_W: f32 = 24.0;
const STRIP_W: f32 = 118.0;
const MEDIAN_W: f32 = 50.0;
const SHARE_BAR_W: f32 = 58.0;
const PCT_W: f32 = 34.0;

fn states_tab(b: &Breakdowns, state: &PanelState, range: IncomeRange, clock: &PaintClock, cx: &mut Context<PanelView>) -> gpui::Div {
    let (lo, hi) = range.bounds();
    let shares = b.state_shares(lo, hi);
    let (main, apart) = sort_states(&b.states, &shares, state.sort);
    // Each strip to its own peak (as the legend's histograms are): a
    // common one is set by the one state whose mass piles into a single
    // bar, and flattens the other 51.
    let peak = OWN_PEAK;
    let sort_button = |by: StateSort, label: &'static str, cx: &mut Context<PanelView>| {
        let on = state.sort == by;
        gpui::div()
            .id(label)
            .px(px(6.0))
            .py(px(1.0))
            .rounded(px(3.0))
            .cursor_pointer()
            .when(on, |d| d.bg(crate::popup::ink(0xe5e7eb)).text_color(crate::popup::ink(0x111827)))
            .when(!on, |d| d.text_color(crate::popup::ink(0x6b7280)))
            .child(label)
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, cx| {
                this.state.sort = by;
                cx.notify();
            }))
    };
    let sorts = gpui::div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .text_xs()
        .child(gpui::div().text_color(crate::popup::ink(0x9ca3af)).mr(px(4.0)).child("Sort"))
        .child(sort_button(StateSort::Median, "median", cx))
        .child(sort_button(StateSort::Share, "in range", cx))
        .child(sort_button(StateSort::Population, "population", cx));
    let columns = gpui::div()
        .flex()
        .flex_row()
        .gap(px(6.0))
        .text_size(px(10.0))
        .text_color(crate::popup::ink(0x9ca3af))
        .child(gpui::div().w(px(CODE_W)))
        .child(gpui::div().w(px(STRIP_W)).child("income distribution"))
        .child(gpui::div().w(px(MEDIAN_W)).flex().justify_end().child("median"))
        .child(gpui::div().w(px(SHARE_BAR_W + PCT_W + 6.0)).child("people in range"));
    let mut rows = gpui::div().flex().flex_col();
    for &i in &main {
        rows = rows.child(state_row(&b.states[i], shares[i], peak, range, state.hovered_state, clock, cx));
    }
    if !apart.is_empty() {
        rows = rows.child(
            gpui::div()
                .mt(px(6.0))
                .pt(px(4.0))
                .border_t_1()
                .border_color(crate::popup::ink(0xe5e7eb))
                .text_size(px(10.0))
                .text_color(crate::popup::ink(0x9ca3af))
                .child("Territory"),
        );
        for &i in &apart {
            rows = rows.child(state_row(&b.states[i], shares[i], peak, range, state.hovered_state, clock, cx));
        }
    }
    gpui::div().flex().flex_col().gap(px(4.0)).child(sorts).child(columns).child(rows)
}

fn state_row(
    g: &Group,
    share: f64,
    peak: f32,
    range: IncomeRange,
    hovered: Option<u32>,
    clock: &PaintClock,
    cx: &mut Context<PanelView>,
) -> gpui::Stateful<gpui::Div> {
    let code = g.code;
    let bounds = g.bounds;
    gpui::div()
        .id(("state-row", code as usize))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_H))
        .rounded(px(3.0))
        .cursor_pointer()
        .when(hovered == Some(code), |d| d.bg(crate::popup::ink(0xf3f4f6)))
        .text_xs()
        .child(gpui::div().w(px(CODE_W)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(crate::popup::ink(0x1f2937)).child(g.abbrev))
        .child(strip(&g.stats.bins, peak, g.stats.median, range, STRIP_W, ROW_H - 4.0, clock).w(px(STRIP_W)).h(px(ROW_H - 4.0)))
        .child(gpui::div().w(px(MEDIAN_W)).flex().justify_end().text_color(crate::popup::ink(0x374151)).child(money(g.stats.median)))
        .child(share_bar(share))
        .child(gpui::div().w(px(PCT_W)).flex().justify_end().text_color(crate::popup::ink(0x374151)).child(percent(share)))
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            let next = if *hovered { Some(code) } else { this.state.hovered_state.filter(|&c| c != code) };
            if next != this.state.hovered_state {
                this.point_at_state(next, cx);
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
            if let Some(b) = bounds {
                this.fly(b, STATE_MAX_ZOOM, window.viewport_size(), cx);
            }
        }))
}

fn share_bar(share: f64) -> gpui::Div {
    let w = (share.clamp(0.0, 1.0) as f32 * SHARE_BAR_W).round();
    gpui::div()
        .w(px(SHARE_BAR_W))
        .h(px(6.0))
        .rounded(px(2.0))
        .bg(crate::popup::ink(0xe5e7eb))
        .child(gpui::div().w(px(w)).h_full().rounded(px(2.0)).bg(crate::popup::ink(0x1f2937)))
}

const RIDGE_LABEL_W: f32 = 104.0;
const RIDGE_NUM_W: f32 = 70.0;
const RIDGE_W: f32 = INNER_W - RIDGE_LABEL_W - RIDGE_NUM_W - 8.0;
const RIDGE_STEP_TARGET: f32 = gpui_maps::census::panel_json::RIDGE_STEP_PX as f32;
const RIDGE_OVERLAP: f32 = gpui_maps::census::panel_json::RIDGE_OVERLAP as f32;

fn metros_tab(b: &Breakdowns, range: IncomeRange, clock: &PaintClock, cx: &mut Context<PanelView>) -> gpui::Div {
    let mut order: Vec<usize> = (0..b.top_metros(TOP_METROS).len()).collect();
    order.sort_by(|&x, &y| b.metros[y].stats.median.total_cmp(&b.metros[x].stats.median));
    let n = order.len();
    let height = RIDGE_STEP_TARGET * (n as f32 + RIDGE_OVERLAP);
    let scale = RidgeScale::new(n, height, RIDGE_OVERLAP);
    let peak = common_peak(order.iter().map(|&i| b.metros[i].stats.bins.as_slice()));
    let ridges: Vec<(Vec<f32>, gpui::Rgba)> =
        order.iter().map(|&i| (b.metros[i].stats.bins.clone(), viridis_at(b.metros[i].stats.median))).collect();
    let (sel_lo, sel_hi) = range_x(range, RIDGE_W);
    let full = range.is_full();
    let clock = clock.clone();
    let canvas = gpui::canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let started = std::time::Instant::now();
            let o = bounds.origin;
            let at = |(x, y): (f32, f32)| gpui::point(o.x + px(x), o.y + px(y));
            for (i, (bins, color)) in ridges.iter().enumerate() {
                let pts = scale.ridge(i, bins, peak, RIDGE_W);
                let mut fill = gpui::PathBuilder::fill();
                fill.move_to(at(pts[0]));
                for &p in &pts[1..] {
                    fill.line_to(at(p));
                }
                fill.close();
                let mut line = gpui::PathBuilder::stroke(px(1.0));
                line.move_to(at(pts[0]));
                for &p in &pts[1..] {
                    line.line_to(at(p));
                }
                // White first, so a ridge hides the one behind it.
                if let Ok(path) = fill.build() {
                    window.paint_path(path.clone(), crate::popup::ink(0xffffff));
                    window.paint_path(path, gpui::Rgba { a: 0.8, ..*color });
                }
                if let Ok(path) = line.build() {
                    window.paint_path(path, crate::popup::ink(0x1f2937));
                }
            }
            if !full {
                let band = gpui::Bounds::new(at((sel_lo, 0.0)), gpui::size(px((sel_hi - sel_lo).max(1.0)), px(height)));
                window.paint_quad(gpui::fill(band, crate::popup::ink_a(0x1f29371a)));
                for x in [sel_lo, sel_hi] {
                    window.paint_quad(gpui::fill(gpui::Bounds::new(at((x - 0.5, 0.0)), gpui::size(px(1.0), px(height))), crate::popup::ink_a(0x1f293799)));
                }
            }
            clock.set(clock.get() + started.elapsed());
        },
    )
    .absolute()
    .left(px(RIDGE_LABEL_W + 4.0))
    .top_0()
    .w(px(RIDGE_W))
    .h(px(height));

    let mut area = gpui::div().relative().w(px(INNER_W)).h(px(height)).child(canvas);
    for (i, &m) in order.iter().enumerate() {
        let g = &b.metros[m];
        let top = scale.baseline(i) - scale.step;
        let bounds = g.bounds;
        area = area.child(
            gpui::div()
                .id(("metro-row", g.code as usize))
                .absolute()
                .left_0()
                .top(px(top))
                .w(px(INNER_W))
                .h(px(scale.step))
                .flex()
                .flex_row()
                .items_end()
                .cursor_pointer()
                .text_size(px(11.0))
                .hover(|s| s.bg(crate::popup::ink_a(0x1f29370d)))
                .child(
                    gpui::div()
                        .w(px(RIDGE_LABEL_W))
                        .text_color(crate::popup::ink(0x1f2937))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(short_metro(&g.name)),
                )
                .child(gpui::div().w(px(RIDGE_W + 8.0)))
                .child(gpui::div().w(px(40.0)).flex().justify_end().text_color(crate::popup::ink(0x374151)).child(money(g.stats.median)))
                .child(
                    gpui::div()
                        .id(("metro-gini", g.code as usize))
                        .w(px(30.0))
                        .flex()
                        .justify_end()
                        .text_size(px(10.0))
                        .text_color(crate::popup::ink(0x9ca3af))
                        .child(format!("{:.2}", g.stats.gini))
                        .tooltip(tip(GINI_LABEL)),
                )
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                    if let Some(b) = bounds {
                        this.fly(b, METRO_MAX_ZOOM, window.viewport_size(), cx);
                    }
                })),
        );
    }
    let axis = gpui::div()
        .relative()
        .w(px(INNER_W))
        .h(px(14.0))
        .text_size(px(10.0))
        .text_color(crate::popup::ink(0x9ca3af))
        .child(gpui::div().absolute().left(px(RIDGE_LABEL_W + 4.0)).child("$2.5K"))
        .child(gpui::div().absolute().left(px(RIDGE_LABEL_W + 4.0 + RIDGE_W / 2.0 - 16.0)).child("$126K"))
        .child(gpui::div().absolute().left(px(RIDGE_LABEL_W + 4.0 + RIDGE_W - 30.0)).child("$250K"));
    let key = gpui::div()
        .flex()
        .flex_row()
        .justify_between()
        .text_size(px(10.0))
        .text_color(crate::popup::ink(0x9ca3af))
        .child(format!("{n} largest metros, by median; colour = median"))
        .child(gpui::div().id("gini-key").child("median · Gini").tooltip(tip(GINI_LABEL)));
    gpui::div().flex().flex_col().gap(px(4.0)).child(key).child(area).child(axis)
}

fn in_view_tab(
    b: &Breakdowns,
    v: Option<&InView>,
    range: IncomeRange,
    clock: &PaintClock,
    cx: &mut Context<PanelView>,
) -> gpui::Div {
    let Some(v) = v else {
        return gpui::div().text_xs().text_color(crate::popup::ink(0x6b7280)).child("Counting what the view shows…");
    };
    let peak = OWN_PEAK;
    let (lo, hi) = range.bounds();
    let section = |title: &str| {
        gpui::div()
            .mt(px(4.0))
            .flex()
            .flex_row()
            .gap(px(6.0))
            .text_size(px(10.0))
            .text_color(crate::popup::ink(0x9ca3af))
            .child(gpui::div().flex_1().child(title.to_string()))
            .child(gpui::div().w(px(44.0)).flex().justify_end().child("people"))
            .child(gpui::div().w(px(50.0)).flex().justify_end().child("median"))
            .child(gpui::div().w(px(38.0)).flex().justify_end().child("in range"))
    };
    let mut out = gpui::div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(gpui::div().text_xs().text_color(crate::popup::ink(0x374151)).child(format!(
            "{} block groups in view; the {} states and metros with the most people in it:",
            thousands(u64::from(v.block_groups)),
            super::IN_VIEW_GROUPS
        )))
        .child(section("States"));
    let row = |id: (&'static str, usize), name: String, iv: &InViewGroup, share: f64, bounds, max_zoom: f64, cx: &mut Context<PanelView>| {
        gpui::div()
            .id(id)
            .flex()
            .flex_col()
            .cursor_pointer()
            .rounded(px(3.0))
            .hover(|s| s.bg(crate::popup::ink(0xf3f4f6)))
            .child(
                gpui::div()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .text_xs()
                    .child(gpui::div().flex_1().whitespace_nowrap().overflow_hidden().text_ellipsis().text_color(crate::popup::ink(0x1f2937)).child(name))
                    .child(gpui::div().w(px(44.0)).flex().justify_end().text_color(crate::popup::ink(0x374151)).child(people(iv.population_total)))
                    .child(gpui::div().w(px(50.0)).flex().justify_end().text_color(crate::popup::ink(0x374151)).child(money(iv.stats.median)))
                    .child(gpui::div().w(px(38.0)).flex().justify_end().text_color(crate::popup::ink(0x6b7280)).child(percent(share))),
            )
            .child(strip(&iv.stats.bins, peak, iv.stats.median, range, INNER_W - 16.0, 10.0, clock).w(px(INNER_W - 16.0)).h(px(10.0)))
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                if let Some(b) = bounds {
                    this.fly(b, max_zoom, window.viewport_size(), cx);
                }
            }))
    };
    for iv in &v.states {
        let g = &b.states[iv.group];
        let share = iv.share_in_range(lo, hi);
        out = out.child(row(("in-view-state", g.code as usize), g.name.clone(), iv, share, g.bounds, STATE_MAX_ZOOM, cx));
    }
    out = out.child(section("Metros"));
    if v.metros.is_empty() {
        out = out.child(gpui::div().text_xs().text_color(crate::popup::ink(0x9ca3af)).child("No metro area in view"));
    }
    for iv in &v.metros {
        let g = &b.metros[iv.group];
        let share = iv.share_in_range(lo, hi);
        out = out.child(row(("in-view-metro", g.code as usize), short_metro(&g.name), iv, share, g.bounds, METRO_MAX_ZOOM, cx));
    }
    out
}

fn footnote(b: &Breakdowns) -> gpui::Div {
    let (excluded_bg, excluded_people) = b.excluded();
    let (weighted_bg, weighted_people) = b.weighted();
    gpui::div()
        .pt(px(6.0))
        .border_t_1()
        .border_color(crate::popup::ink(0xe5e7eb))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .text_size(px(10.0))
        .text_color(crate::popup::ink(0x6b7280))
        .child(gpui::div().font_weight(gpui::FontWeight::SEMIBOLD).child("About these numbers"))
        .child(
            "Median: the population-weighted median of block-group median household incomes (ACS 2023 5-year) \
             -- the income of the block group the middle resident lives in, not the Census's own household median.",
        )
        .child(
            "Gini: inequality BETWEEN block groups' medians, population-weighted -- not a household Gini, \
             which also counts the spread inside each block group and runs far higher.",
        )
        .child(format!(
            "Weighted: {} block groups, {} people. Excluded for a missing income or population: {} block groups ({} people).",
            thousands(u64::from(weighted_bg)),
            people(weighted_people),
            thousands(u64::from(excluded_bg)),
            people(excluded_people),
        ))
}

