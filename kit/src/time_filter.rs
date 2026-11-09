//! The Studio's one time slider: a histogram of the rows over time, a
//! brushable window (or a play head) on it, and Play / speed buttons that
//! slide it across the domain. Kepler.gl's time-range filter, generalised
//! from `examples/panels/kepler.rs`'s card so every page with a time
//! dimension gets the same control.
//!
//! What the window drives ([`TimeTarget`]):
//! - a point source's GPU range filter (`MapView::set_source_filter_range`,
//!   a uniform write; an arc layer of the same id follows);
//! - a trips layer's clock (`MapView::set_trip_clock`, the window's end is
//!   the head and its width the trail);
//! - anything else, through a callback: a dot layer's epochs (a discrete
//!   time axis), a raster's WMS `TIME`, a simulated drive's position.
//!
//! The loop that moves the window runs only while playing; a paused card
//! schedules nothing. Pure maths (snapping to stops, counting rows in the
//! window) is at the bottom with its tests.
//!
//! `GPUI_MAP_STUDIO_PROBE_TIME=a[,b]` (fractions of the histogram's width)
//! presses at `a` and drags to `b` through the window's own event path, for
//! screenshots; `GPUI_MAP_STUDIO_TIME_PLAY=1` starts playing on open.
#![allow(dead_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, div, point, px, relative, rgba, App, Context, DispatchPhase, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render, SharedString,
    Styled as _, WeakEntity, Window,
};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::filter::Playback;

/// The shared brush state machine (move inside, resize at an edge, draw
/// outside): `brush.rs`, declared once by each binary.
use crate::brush;

/// Bars in the time histogram.
pub const HIST_BINS: usize = 80;
/// The playback loop's frame interval.
const FRAME: Duration = Duration::from_millis(16);
/// The Play/Pause button's width: room for "Pause", the wider label.
const PLAY_W: f32 = 56.0;
/// The speed button's width: room for "0.25x", the widest of [`SPEEDS`].
const SPEED_W: f32 = 48.0;

/// Playback speeds the card cycles through (Kepler's multiplier).
pub const SPEEDS: [f64; 6] = [0.1, 0.25, 0.5, 1.0, 2.0, 4.0];

/// What a [`TimeFilter`]'s window drives.
#[derive(Clone)]
pub enum TimeTarget {
    /// A point source's GPU filter (and an arc layer of the same id).
    Source { source: SharedString },
    /// One field of a point source's packed filter
    /// (`MapView::set_source_filter_field_range`): the other fields'
    /// windows -- a linked chart's -- are kept. An arc layer of the same
    /// id follows the window too.
    Field { source: SharedString, field: SharedString },
    /// A trip layer's clock: the window's end is the head, its width the
    /// trail.
    Trips { layer: SharedString, trail: f64 },
    /// The page's own: called with the window on every move.
    Custom(Rc<dyn Fn([f64; 2], &mut App)>),
}

/// How the window moves on the histogram.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowMode {
    /// A brushable range (a press outside draws a new one; inside drags it).
    Range,
    /// A play head with a trail behind it: a press puts the head there.
    Head { trail: f64 },
    /// A head that rests on these stops (sorted): a press or a release
    /// snaps to the nearest, playback glides between them.
    Stops(Vec<f64>),
}

/// What the card says the window holds.
#[derive(Clone)]
pub enum Counter {
    /// Rows with these times (sorted) inside the window.
    Rows { sorted: Rc<Vec<f64>>, noun: SharedString },
    /// Spans `[start, end]` (sorted starts and ends) live at the head.
    Live { starts: Rc<Vec<f64>>, ends: Rc<Vec<f64>>, noun: SharedString },
    /// The page's own words for a window.
    Text(Rc<dyn Fn([f64; 2]) -> String>),
}

/// How a time prints.
#[derive(Clone)]
pub struct TimeFormat {
    /// A window end, in the heading.
    pub stamp: Rc<dyn Fn(f64) -> String>,
    /// The domain's ends, under the histogram.
    pub edge: Rc<dyn Fn(f64) -> String>,
}

impl TimeFormat {
    /// Dates (or `HH:MM ZONE` for a domain of a day or two), years at the
    /// edges -- Kepler's card -- in the machine's local time with its zone
    /// ("08:00 SGT"); the document and the filter keep UTC.
    pub fn dates(domain: [f64; 2]) -> Self {
        let span = domain[1] - domain[0];
        // The edges name what tells them apart: the hour within a few
        // days, the day within two years, else the year.
        Self {
            stamp: Rc::new(move |ms| stamp(ms, span)),
            edge: Rc::new(move |ms| {
                if span < 3.0 * 86_400_000.0 {
                    local_datetime(ms)
                } else if span < 2.0 * 365.25 * 86_400_000.0 {
                    local_date(ms)
                } else {
                    year(ms + f64::from(local_zone(ms).0) * 60_000.0)
                }
            }),
        }
    }

    /// Plain numbers through `f`: years, metres.
    pub fn plain(f: impl Fn(f64) -> String + 'static) -> Self {
        let f: Rc<dyn Fn(f64) -> String> = Rc::new(f);
        Self { stamp: f.clone(), edge: f }
    }
}

/// The time slider. See the module doc.
pub struct TimeFilter {
    map: WeakEntity<MapView>,
    pub target: TimeTarget,
    pub mode: WindowMode,
    /// The field's name in the heading ("Time", "Epoch", "Clock").
    pub label: SharedString,
    pub playback: Playback,
    /// Bar heights, any unit (counts, people, turns).
    pub hist: Vec<f64>,
    counter: Counter,
    format: TimeFormat,
    /// The histogram's window box `(x, y, w, h)` at the last prepaint.
    bounds: Rc<Cell<(f32, f32, f32, f32)>>,
    /// A held press ([`brush`]), in the domain's own values. A head mode
    /// holds one too, only to say the user has the head.
    brush: Option<brush::Drag>,
    /// What the pointer is over on the histogram (Range mode): the cursor
    /// and the edge grips.
    brush_hit: brush::Hit,
    /// A press paused playback: it plays on from the new window on release.
    resume: bool,
    looping: bool,
    epoch: Instant,
    /// Frames the playback loop moved the window: a paused filter adds
    /// none.
    pub frames: u64,
    /// Windows sent to the target.
    pub applied: u64,
    /// Laid out in a side panel (full width, compact) rather than
    /// floating over the map's bottom-left.
    pub docked: bool,
    /// Whether the window reaches the target only on release (a drive
    /// seek restarts the trip: once, not per pointer move).
    pub apply_on_release: bool,
    /// A line under the card (what the bars are).
    pub caption: Option<SharedString>,
    /// Show the Play / speed buttons.
    pub controls: bool,
    /// Called with the window when the brush is let go or playback pauses:
    /// a page whose filter applies on release only.
    pub on_release: Option<Rc<dyn Fn([f64; 2], &mut App)>>,
    /// The speeds the speed button cycles through ([`SPEEDS`] by default).
    pub speeds: &'static [f64],
    /// The held brush's window listeners are installed; until then the
    /// histogram's own handlers follow the drag.
    listening: Rc<Cell<bool>>,
}

impl TimeFilter {
    /// The general constructor. `hist` is drawn as is; the window is
    /// applied at once.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        map: WeakEntity<MapView>,
        target: TimeTarget,
        mode: WindowMode,
        label: &str,
        domain: [f64; 2],
        window: [f64; 2],
        hist: Vec<f64>,
        counter: Counter,
        format: TimeFormat,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut tf = Self {
            map,
            target,
            mode,
            label: label.to_string().into(),
            playback: Playback::new(domain, window),
            hist,
            counter,
            format,
            bounds: Rc::new(Cell::new((0.0, 0.0, 1.0, 1.0))),
            brush: None,
            brush_hit: brush::Hit::Outside,
            resume: false,
            looping: false,
            epoch: cx.background_executor().now(),
            frames: 0,
            applied: 0,
            docked: false,
            apply_on_release: false,
            caption: None,
            controls: true,
            on_release: None,
            speeds: &SPEEDS,
            listening: Rc::new(Cell::new(false)),
        };
        tf.apply(cx);
        if std::env::var("GPUI_MAP_STUDIO_TIME_PLAY").is_ok_and(|v| v == "1") {
            tf.toggle(cx);
        }
        start_probe(cx.entity().downgrade(), cx);
        tf
    }

    /// A point source's time column: the histogram of `times` (ms since
    /// 1970, any order), the window a GPU range filter on `source` (whose
    /// filter column the caller declared).
    pub fn for_points(map: WeakEntity<MapView>, source: &str, label: &str, domain: [f64; 2], window: [f64; 2], mut times: Vec<f64>, noun: &str, cx: &mut Context<Self>) -> Self {
        times.retain(|t| t.is_finite());
        times.sort_by(f64::total_cmp);
        let hist = gpui_maps::overlay::filter::histogram(times.iter().copied(), domain, HIST_BINS).into_iter().map(f64::from).collect();
        let counter = Counter::Rows { sorted: Rc::new(times), noun: noun.to_string().into() };
        Self::new(map, TimeTarget::Source { source: source.to_string().into() }, WindowMode::Range, label, domain, window, hist, counter, TimeFormat::dates(domain), cx)
    }

    /// A trips layer's clock: the histogram counts the trips live per bin
    /// (`spans`: each trip's `[first, last]`), the window is
    /// `[clock - trail, clock]`.
    #[allow(clippy::too_many_arguments)]
    pub fn for_trips(map: WeakEntity<MapView>, layer: &str, domain: [f64; 2], clock: f64, trail: f64, spans: &[[f64; 2]], noun: &str, cx: &mut Context<Self>) -> Self {
        let mut starts: Vec<f64> = spans.iter().map(|s| s[0]).collect();
        let mut ends: Vec<f64> = spans.iter().map(|s| s[1]).collect();
        starts.sort_by(f64::total_cmp);
        ends.sort_by(f64::total_cmp);
        let hist = coverage_histogram(spans, domain, HIST_BINS).into_iter().map(f64::from).collect();
        let counter = Counter::Live { starts: Rc::new(starts), ends: Rc::new(ends), noun: noun.to_string().into() };
        let target = TimeTarget::Trips { layer: layer.to_string().into(), trail };
        Self::new(map, target, WindowMode::Head { trail }, "Clock", domain, [clock - trail, clock], hist, counter, TimeFormat::dates(domain), cx)
    }

    /// For a side panel: full width, compact text, not over the map.
    #[must_use]
    pub fn docked(mut self) -> Self {
        self.docked = true;
        self
    }

    /// Drives `field` of the source's packed filter instead of its whole
    /// filter: a time slider and a `linked_chart::LinkedChart`
    /// over one source each keep their own window. The window is applied
    /// again at once.
    #[must_use]
    pub fn on_field(mut self, field: &str, cx: &mut Context<Self>) -> Self {
        if let TimeTarget::Source { source } | TimeTarget::Field { source, .. } = self.target.clone() {
            self.target = TimeTarget::Field { source, field: field.to_string().into() };
            self.apply(cx);
        }
        self
    }

    /// How times print (the heading, the domain's ends).
    #[must_use]
    pub fn format(mut self, format: TimeFormat) -> Self {
        self.format = format;
        self
    }

    #[must_use]
    pub fn caption(mut self, text: &str) -> Self {
        self.caption = Some(text.to_string().into());
        self
    }

    /// Whether the playback loop is scheduled (a paused card's ends the
    /// frame after the pause).
    pub fn is_looping(&self) -> bool {
        self.looping
    }

    /// The head (the window's end).
    pub fn head(&self) -> f64 {
        self.playback.window()[1]
    }

    /// Moves the head from outside (a drive that advanced on its own),
    /// unless the user holds the brush. Sends nothing to the target.
    pub fn follow(&mut self, head: f64, cx: &mut Context<Self>) {
        if self.brush.is_some() {
            return;
        }
        let trail = match &self.mode {
            WindowMode::Head { trail } => *trail,
            _ => 0.0,
        };
        let w = [head - trail, head];
        if self.playback.window() != w {
            self.playback.set_window(w);
            cx.notify();
        }
    }

    /// The next speed in [`SPEEDS`].
    pub fn cycle_speed(&mut self, cx: &mut Context<Self>) {
        let speeds = self.speeds;
        let at = speeds.iter().position(|s| (*s - self.playback.speed).abs() < 1e-9).map_or(0, |i| (i + 1) % speeds.len());
        self.playback.speed = speeds[at];
        cx.notify();
    }

    /// Play (true) or Pause, through the Play button's own path.
    pub fn set_playing(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.playback.is_playing() != on {
            self.toggle(cx);
        }
    }

    /// The speed button's choice nearest `speed`; the speed set.
    pub fn set_speed(&mut self, speed: f64, cx: &mut Context<Self>) -> f64 {
        let s = self.speeds.iter().copied().min_by(|a, b| (a - speed).abs().total_cmp(&(b - speed).abs())).unwrap_or(speed);
        self.playback.speed = s;
        cx.notify();
        s
    }

    /// A window brushed on the histogram (a head mode takes its end),
    /// clamped to the domain and sent to the target as a release would.
    pub fn seek(&mut self, window: [f64; 2], cx: &mut Context<Self>) -> [f64; 2] {
        let [d0, d1] = self.playback.domain();
        let lo = window[0].min(window[1]).clamp(d0, d1);
        let hi = window[0].max(window[1]).clamp(d0, d1);
        if self.mode == WindowMode::Range {
            self.playback.set_window([lo, hi]);
            self.apply(cx);
        } else {
            self.put_head(hi, cx);
            if self.apply_on_release {
                self.apply(cx);
            }
        }
        if let Some(f) = self.on_release.clone() {
            f(self.playback.window(), cx);
        }
        cx.notify();
        self.playback.window()
    }

    /// The card as the agent reads it: the domain, the window, playing,
    /// the speed and its choices, what the window holds.
    pub fn agent_state(&self) -> serde_json::Value {
        serde_json::json!({
            "label": self.label.to_string(),
            "domain": self.playback.domain(),
            "window": self.playback.window(),
            "playing": self.playback.is_playing(),
            "speed": self.playback.speed,
            "speeds": self.speeds,
            "holds": self.status(),
            "controls": self.controls,
        })
    }

    /// The window, sent to the target.
    fn apply(&mut self, cx: &mut Context<Self>) {
        let window = self.playback.window();        self.applied += 1;
        match self.target.clone() {
            TimeTarget::Source { source } => {
                let _ = self.map.update(cx, |m, cx| {
                    m.set_source_filter_range(&source, Some(window), cx);
                    m.set_arc_filter_range(&source, Some(window), cx);
                });
            }
            TimeTarget::Field { source, field } => {
                let _ = self.map.update(cx, |m, cx| {
                    m.set_source_filter_field_range(&source, &field, Some(window), cx);
                    m.set_arc_filter_range(&source, Some(window), cx);
                });
            }
            TimeTarget::Trips { layer, .. } => {
                let _ = self.map.update(cx, |m, cx| m.set_trip_clock(&layer, Some(window[1]), cx));
            }
            TimeTarget::Custom(f) => f(window, cx),
        }
        cx.notify();
    }

    /// What the window holds, in words.
    pub fn status(&self) -> String {
        let w = self.playback.window();
        match &self.counter {
            Counter::Rows { sorted, noun } => format!("{} {noun}", group(rows_in(sorted, w) as f64)),
            Counter::Live { starts, ends, noun } => format!("{} {noun}", group(live_at(starts, ends, w[1]) as f64)),
            Counter::Text(f) => f(w),
        }
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        // A window that spans (nearly) the whole domain has nowhere to
        // slide: Play starts from the domain's first tenth instead.
        let [d0, d1] = self.playback.domain();
        let [lo, hi] = self.playback.window();
        if !self.playback.is_playing() && self.mode == WindowMode::Range && hi - lo >= 0.9 * (d1 - d0) && d1 > d0 {
            self.playback.set_window([d0, d0 + (d1 - d0) / 10.0]);
            self.apply(cx);
        }
        self.playback.toggle();
        if self.playback.is_playing() {
            self.ensure_loop(cx);
        } else if let Some(f) = self.on_release.clone() {
            f(self.playback.window(), cx);
        }
        cx.notify();
    }

    /// One step per frame while playing, gone the first frame after a
    /// pause.
    fn ensure_loop(&mut self, cx: &mut Context<Self>) {
        if self.looping {
            return;
        }
        self.looping = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(FRAME).await;
            let Ok(go) = this.update(cx, |tf, cx| tf.step(cx)) else { break };
            if !go {
                let _ = this.update(cx, |tf, _| tf.looping = false);
                break;
            }
        })
        .detach();
    }

    /// One playback frame: `false` when paused (the loop then ends).
    fn step(&mut self, cx: &mut Context<Self>) -> bool {
        let now = cx.background_executor().now().saturating_duration_since(self.epoch).as_secs_f64();
        if self.playback.tick(now).is_none() {
            return false;
        }
        self.frames += 1;
        self.apply(cx);
        true
    }

    /// The domain value under window x `x`.
    fn value_at(&self, x: f32) -> f64 {
        let (bx, _, bw, _) = self.bounds.get();
        let t = f64::from(((x - bx) / bw.max(1.0)).clamp(0.0, 1.0));
        let [d0, d1] = self.playback.domain();
        d0 + t * (d1 - d0)
    }

    /// Sets the window from a pointer value, by mode.
    fn put_head(&mut self, v: f64, cx: &mut Context<Self>) {
        let w = match &self.mode {
            WindowMode::Head { trail } => [v - trail, v],
            WindowMode::Stops(stops) => {
                let s = snap(stops, v);
                [s, s]
            }
            WindowMode::Range => return,
        };
        self.playback.set_window(w);
        if self.apply_on_release {
            cx.notify();
        } else {
            self.apply(cx);
        }
    }

    /// The edge tolerance ([`brush::EDGE_PX`]) in the domain's units.
    fn edge_tol(&self) -> f64 {
        let [d0, d1] = self.playback.domain();
        f64::from(brush::EDGE_PX / self.bounds.get().2.max(1.0)) * (d1 - d0)
    }

    /// A press on the histogram: pauses a playing card (the user has the
    /// window now; it plays on from the new window on release), then the
    /// [`brush`] rules -- inside moves the window,
    /// an edge resizes it, outside draws a new one from here.
    pub fn press(&mut self, x: f32, cx: &mut Context<Self>) {
        let v = self.value_at(x);
        if self.playback.is_playing() {
            self.resume = true;
            self.toggle(cx);
        }
        if self.mode != WindowMode::Range {
            self.brush = Some(brush::press(None, v, 0.0));
            self.put_head(v, cx);
            return;
        }
        let d = brush::press(Some(self.playback.window()), v, self.edge_tol());
        self.brush = Some(d);
        if let brush::Grab::New { anchor } = d.grab {
            self.playback.set_window([anchor, anchor]);
            self.apply(cx);
        }
        cx.notify();
    }

    pub fn drag(&mut self, x: f32, cx: &mut Context<Self>) {
        let v = self.value_at(x);
        if self.mode != WindowMode::Range {
            if self.brush.is_some() {
                self.put_head(v, cx);
            }
            return;
        }
        let Some(mut d) = self.brush else { return };
        let window = brush::drag(&mut d, v, self.playback.domain(), 0.0);
        self.brush = Some(d);
        if let Some(w) = window {
            self.playback.set_window(w);
            self.apply(cx);
        }
    }

    /// The pointer over the histogram with no button held (Range mode):
    /// what it is over, for the cursor and the grips.
    fn hover_at(&mut self, x: f32, cx: &mut Context<Self>) {
        if self.mode != WindowMode::Range || self.brush.is_some() {
            return;
        }
        let hit = brush::hit(Some(self.playback.window()), self.value_at(x), self.edge_tol());
        if hit != self.brush_hit {
            self.brush_hit = hit;
            cx.notify();
        }
    }

    pub fn release(&mut self, cx: &mut Context<Self>) {
        let held = self.brush.take().is_some();
        if held && self.apply_on_release {
            self.apply(cx);
        }
        if held {
            if let Some(f) = self.on_release.clone() {
                f(self.playback.window(), cx);
            }
            if std::mem::take(&mut self.resume) && !self.playback.is_playing() {
                self.toggle(cx);
            }
        }
        cx.notify();
    }

    fn dark(cx: &App) -> bool {
        cx.try_global::<gpui_component::Theme>().is_some_and(|t| t.mode.is_dark())
    }
}

/// A floating card's bottom: one corner inset above the bottom row (the
/// brand mark and the credit, `chrome::BOTTOM_ROW_TOP_PX`), so the stack
/// keeps the corner's rhythm.
const FLOAT_BOTTOM_PX: f32 = gpui_maps::chrome::BOTTOM_ROW_TOP_PX + gpui_maps::chrome::CORNER_INSET_PX;

impl Render for TimeFilter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_component::button::{Button, ButtonVariants as _};
        use gpui_component::Sizable as _;
        let (bg, fg, muted, bar, bar_in) = if Self::dark(cx) {
            (0x1d20_25ffu32, 0xe8ea_edffu32, 0xa0a6_adffu32, 0x4a50_58ffu32, 0xd6b4_4affu32)
        } else {
            (0xffff_ffff, 0x1f29_33ff, 0x5f6b_76ff, 0xc5cc_d3ff, 0x2f6f_b0ff)
        };
        let [d0, d1] = self.playback.domain();
        let [lo, hi] = self.playback.window();
        let span = (d1 - d0).max(f64::MIN_POSITIVE);
        let max = self.hist.iter().copied().fold(0.0, f64::max).max(1e-9);
        let n = self.hist.len().max(1);
        let point_like = lo == hi;
        let bars = self.hist.iter().enumerate().map(|(i, &c)| {
            let (a, b) = (d0 + i as f64 / n as f64 * span, d0 + (i + 1) as f64 / n as f64 * span);
            let inside = if point_like { lo >= a && lo <= b } else { b > lo && a < hi };
            div().flex_1().h(relative(((c / max) as f32).max(if c > 0.0 { 0.02 } else { 0.0 }))).bg(rgba(if inside { bar_in } else { bar }))
        });
        let left = ((lo - d0) / span) as f32;
        let width = ((hi - lo) / span) as f32;
        let bounds = self.bounds.clone();
        let probe = canvas(
            move |b, _, _| bounds.set((f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height))),
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
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
                                this.release(cx);
                            }
                        });
                    });
                    let released = entity.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                            let _ = released.update(cx, |this, cx| this.release(cx));
                        }
                    });
                },
            )
            .absolute()
            .size_0()
        });
        let heading = if point_like || matches!(self.mode, WindowMode::Head { .. }) {
            format!("{} {}", self.label, (self.format.stamp)(hi))
        } else {
            format!("{}: {} to {}", self.label, (self.format.stamp)(lo), (self.format.stamp)(hi))
        };
        let playing = self.playback.is_playing();
        let entity = cx.entity().downgrade();
        // Fixed widths: "Play" / "Pause" and "0.1x" .. "0.25x" .. "4x"
        // swap on a press, and a label-sized button moved its neighbours.
        let play = Button::new("time-filter-play").small().w(px(PLAY_W)).when(!playing, |b| b.primary()).label(if playing { "Pause" } else { "Play" }).cursor_pointer().on_click(move |_, _, cx| {
            let _ = entity.update(cx, |tf, cx| tf.toggle(cx));
        });
        let entity = cx.entity().downgrade();
        let speed = Button::new("time-filter-speed")
            .small()
            .w(px(SPEED_W))
            .label(format!("{}x", gpui_maps::overlay::color_scale::format_number(self.playback.speed)))
            .cursor_pointer().on_click(move |_, _, cx| {
                let _ = entity.update(cx, |tf, cx| tf.cycle_speed(cx));
            });
        // A window (or head) box over the bars.
        // What the pointer holds or is over (Range mode): the cursor, and
        // the 2 px grip on the edge it would resize.
        let range = self.mode == WindowMode::Range;
        let moving = matches!(self.brush.map(|b| b.grab), Some(brush::Grab::Move { .. }));
        let hit = match self.brush.map(|d| d.grab) {
            Some(brush::Grab::Move { .. }) => brush::Hit::Inside,
            Some(brush::Grab::Edge { fixed }) => brush::Hit::Edge(if fixed >= hi { brush::Side::Low } else { brush::Side::High }),
            Some(brush::Grab::New { .. }) => brush::Hit::Outside,
            None => self.brush_hit,
        };
        let grip = |side: brush::Side| {
            div()
                .absolute()
                .top_0()
                .h_full()
                .w(px(2.0))
                .when(side == brush::Side::Low, |d| d.left(px(-1.0)))
                .when(side == brush::Side::High, |d| d.right(px(-1.0)))
                .when(range && hit == brush::Hit::Edge(side), |d| d.bg(rgba(fg)))
        };
        let window_box = div()
            .absolute()
            .top_0()
            .h_full()
            .left(relative(left))
            .when(width > 0.0, |d| {
                d.w(relative(width))
                    .border_1()
                    .border_color(rgba(bar_in))
                    .bg(Hsla::from(rgba(bar_in)).opacity(0.12))
                    .child(grip(brush::Side::Low))
                    .child(grip(brush::Side::High))
            })
            .when(width <= 0.0, |d| d.w(px(2.0)).ml(px(-1.0)).bg(rgba(bar_in)));
        let ticks: Vec<f64> = match &self.mode {
            WindowMode::Stops(stops) => stops.clone(),
            _ => Vec::new(),
        };
        // At most seven labels: many stops (a radar's frames every two
        // minutes) would print over each other.
        let every = ticks.len().div_ceil(7).max(1);
        let stop_labels = (!ticks.is_empty()).then(|| {
            div().relative().h(px(14.0)).w_full().text_color(rgba(muted)).children(ticks.iter().step_by(every).map(|&s| {
                let t = ((s - d0) / span) as f32;
                div().absolute().left(relative(t)).ml(px(-18.0)).w(px(36.0)).flex().justify_center().child((self.format.edge)(s))
            }))
        });
        let docked = self.docked;
        div()
            .id("time-filter")
            .debug_selector(|| "time-filter".to_string())
            .occlude()
            // One corner inset above the bottom row (`FLOAT_BOTTOM_PX`).
            .when(!docked, |d| d.absolute().bottom(px(FLOAT_BOTTOM_PX)).left(px(gpui_maps::chrome::CORNER_INSET_PX)).w(px(560.)))
            .when(docked, |d| d.w_full())
            .flex()
            .flex_col()
            .p(px(10.))
            .gap(px(6.))
            .rounded(px(6.))
            .bg(rgba(bg))
            .text_color(rgba(fg))
            .text_xs()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .items_center()
                    .when(self.controls, |r| r.child(div().debug_selector(|| "time-filter-play".to_string()).child(play)).child(speed))
                    .when(!docked, |r| r.child(div().font_weight(gpui::FontWeight::MEDIUM).child(heading.clone())))
                    .child(div().flex_1())
                    .child(div().text_color(rgba(muted)).child(self.status())),
            )
            .when(docked, |c| c.child(div().font_weight(gpui::FontWeight::MEDIUM).child(heading)))
            .child(
                div()
                    .id("time-filter-hist")
                    .debug_selector(|| "time-filter-hist".to_string())
                    .relative()
                    .h(px(56.))
                    .w_full()
                    .map(|d| match (range, hit) {
                        // Grabbing while the window is held.
                        (true, brush::Hit::Inside) if moving => d.cursor(gpui::CursorStyle::ClosedHand),
                        (true, brush::Hit::Inside) => d.cursor(gpui::CursorStyle::OpenHand),
                        (true, brush::Hit::Edge(_)) => d.cursor(gpui::CursorStyle::ResizeLeftRight),
                        (true, brush::Hit::Outside) => d.cursor_crosshair(),
                        (false, _) => d.cursor_pointer(),
                    })
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                        if event.pressed_button.is_none() {
                            this.hover_at(f32::from(event.position.x), cx);
                        } else if event.pressed_button == Some(MouseButton::Left) && this.brush.is_some() && !this.listening.get() {
                            // A drag faster than a frame: the window
                            // listeners are not in yet.
                            this.drag(f32::from(event.position.x), cx);
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _window, cx| {
                            if !this.listening.get() {
                                this.release(cx);
                            }
                        }),
                    )
                    .child(probe)
                    .child(div().absolute().size_full().flex().items_end().gap(px(1.)).children(bars))
                    .child(window_box)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                            this.press(f32::from(event.position.x), cx);
                            cx.stop_propagation();
                        }),
                    )
                    .children(drag_listeners),
            )
            .children(stop_labels)
            .when(ticks.is_empty(), |c| {
                c.child(
                    div()
                        .flex()
                        .flex_row()
                        .text_color(rgba(muted))
                        .child(div().child((self.format.edge)(d0)))
                        .child(div().flex_1())
                        .when(self.controls, |r| r.child(div().child(format!("{:.0} s a run at {}x", span / self.playback.rate(), gpui_maps::overlay::color_scale::format_number(self.playback.speed)))))
                        .child(div().flex_1())
                        .child(div().child((self.format.edge)(d1))),
                )
            })
            .children(self.caption.clone().map(|c| div().text_color(rgba(muted)).child(c)))
    }
}

// ---------------------------------------------------------------------------
// The maths
// ---------------------------------------------------------------------------

/// The stop nearest `v` (the stops sorted); `v` itself when there are none.
pub fn snap(stops: &[f64], v: f64) -> f64 {
    stops.iter().copied().min_by(|a, b| (a - v).abs().total_cmp(&(b - v).abs())).unwrap_or(v)
}

/// The two stops either side of `v` and how far between them it is:
/// `(a, b, t)`, `t` in `0..1` -- how a playing head between epochs blends
/// them. At or past the last stop, `(last, last, 0)`.
pub fn between(stops: &[f64], v: f64) -> Option<(f64, f64, f64)> {
    let first = *stops.first()?;
    let last = *stops.last()?;
    if v <= first {
        return Some((first, first, 0.0));
    }
    if v >= last {
        return Some((last, last, 0.0));
    }
    let i = stops.partition_point(|&s| s <= v);
    let (a, b) = (stops[i - 1], stops[i]);
    Some((a, b, ((v - a) / (b - a)).clamp(0.0, 1.0)))
}

/// Rows whose time lies in the closed `window` (times sorted).
pub fn rows_in(sorted: &[f64], window: [f64; 2]) -> usize {
    sorted.partition_point(|&t| t <= window[1]) - sorted.partition_point(|&t| t < window[0])
}

/// Spans live at `head`: started by it, not yet ended.
pub fn live_at(starts: &[f64], ends: &[f64], head: f64) -> usize {
    starts.partition_point(|&t| t <= head).saturating_sub(ends.partition_point(|&t| t < head))
}

/// Trips in the air per bin: each `[first, last]` span counted in every bin
/// it overlaps.
pub fn coverage_histogram(spans: &[[f64; 2]], domain: [f64; 2], bins: usize) -> Vec<u32> {
    let mut out = vec![0u32; bins.max(1)];
    let span = domain[1] - domain[0];
    if !(span > 0.0) {
        return out;
    }
    let n = out.len();
    let bin = |t: f64| (((t - domain[0]) / span * n as f64).floor().max(0.0) as usize).min(n - 1);
    for s in spans {
        if !(s[0].is_finite() && s[1].is_finite()) || s[1] < domain[0] || s[0] > domain[1] {
            continue;
        }
        for b in &mut out[bin(s[0])..=bin(s[1])] {
            *b += 1;
        }
    }
    out
}

fn group(v: f64) -> String {
    gpui_maps::overlay::color_scale::format_number(v.round())
}

fn date(ms: f64) -> String {
    gpui_maps::overlay::filter::format_date(ms)
}

fn year(ms: f64) -> String {
    date(ms).chars().take(4).collect()
}

// ---------------------------------------------------------------------------
// Local time: what the cards show (the document keeps UTC milliseconds)
// ---------------------------------------------------------------------------

/// The machine's zone at `ms`: minutes east of UTC and its abbreviation
/// ("SGT", "JST", "CEST"; "UTC+05:30" where the zone database has none).
pub fn local_zone(ms: f64) -> (i32, String) {
    use chrono::TimeZone as _;
    let offset_min = if ms.is_finite() {
        chrono::Local.timestamp_millis_opt(ms as i64).single().map_or_else(|| chrono::Local::now().offset().local_minus_utc(), |t| t.offset().local_minus_utc()) / 60
    } else {
        0
    };
    let abbr = zone_abbreviation(ms).filter(|a| a.chars().any(|c| c.is_ascii_alphabetic())).or_else(|| named_zone(offset_min)).unwrap_or_else(|| utc_offset_name(offset_min));
    (offset_min, abbr)
}

/// The zone database's abbreviation for the local zone at `ms`.
#[cfg(target_os = "macos")]
fn zone_abbreviation(ms: f64) -> Option<String> {
    if !ms.is_finite() {
        return None;
    }
    let t: libc::time_t = (ms / 1000.0).floor() as libc::time_t;
    // SAFETY: `localtime_r` writes into the zeroed `tm` we own; `tm_zone`
    // points at a static string in the zone database.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() || tm.tm_zone.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(tm.tm_zone).to_string_lossy().into_owned())
    }
}

#[cfg(not(target_os = "macos"))]
fn zone_abbreviation(_ms: f64) -> Option<String> {
    None
}

/// Zones the database names only by offset ("+08"), by their usual name,
/// where the machine's zone says which: Singapore is "+08" there.
fn named_zone(offset_min: i32) -> Option<String> {
    let tz = std::env::var("TZ").ok().filter(|t| !t.is_empty()).or_else(|| std::fs::read_link("/etc/localtime").ok().map(|p| p.to_string_lossy().into_owned()))?;
    let name = [("Singapore", 480, "SGT"), ("Kuala_Lumpur", 480, "MYT"), ("Ho_Chi_Minh", 420, "ICT"), ("Bangkok", 420, "ICT")]
        .iter()
        .find(|(city, off, _)| tz.ends_with(city) && *off == offset_min)?
        .2;
    Some(name.to_string())
}

/// "UTC", "UTC+08", "UTC+05:30", "UTC-03".
pub fn utc_offset_name(offset_min: i32) -> String {
    if offset_min == 0 {
        return "UTC".into();
    }
    let sign = if offset_min < 0 { '-' } else { '+' };
    let (h, m) = (offset_min.abs() / 60, offset_min.abs() % 60);
    if m == 0 {
        format!("UTC{sign}{h:02}")
    } else {
        format!("UTC{sign}{h:02}:{m:02}")
    }
}

/// `HH:MM ZONE` of `ms` in a zone `offset_min` east of UTC.
pub fn clock_in(ms: f64, offset_min: i32, abbr: &str) -> String {
    format!("{} {abbr}", clock(ms + f64::from(offset_min) * 60_000.0))
}

/// `YYYY-MM-DD` of `ms` in a zone `offset_min` east of UTC.
pub fn date_in(ms: f64, offset_min: i32) -> String {
    date(ms + f64::from(offset_min) * 60_000.0)
}

/// `HH:MM ZONE` in local time ("08:00 SGT").
pub fn local_clock(ms: f64) -> String {
    let (off, abbr) = local_zone(ms);
    clock_in(ms, off, &abbr)
}

/// The local date.
pub fn local_date(ms: f64) -> String {
    date_in(ms, local_zone(ms).0)
}

/// `YYYY-MM-DD HH:MM ZONE` in local time.
pub fn local_datetime(ms: f64) -> String {
    let (off, abbr) = local_zone(ms);
    format!("{} {}", date_in(ms, off), clock_in(ms, off, &abbr))
}

/// `HH:MM` (UTC) of `ms`.
pub fn clock(ms: f64) -> String {
    let m = (ms.rem_euclid(86_400_000.0) / 60_000.0) as u64;
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// A window end in local time: the date, or the time of day with its zone
/// when the domain is a day or two.
fn stamp(ms: f64, span: f64) -> String {
    if span < 3.0 * 86_400_000.0 {
        local_clock(ms)
    } else {
        local_date(ms)
    }
}

// ---------------------------------------------------------------------------
// The screenshot probe
// ---------------------------------------------------------------------------

fn start_probe(tf: WeakEntity<TimeFilter>, cx: &mut App) {
    let Some((a, b)) = std::env::var("GPUI_MAP_STUDIO_PROBE_TIME").ok().and_then(|v| {
        let mut it = v.split(',').map(|s| s.trim().parse::<f32>().ok());
        let a = it.next()??;
        Some((a, it.next().flatten()))
    }) else {
        return;
    };
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            if Instant::now() > deadline {
                eprintln!("time probe: no laid-out card after 40 s");
                return;
            }
            let Ok((bx, by, bw, bh)) = tf.update(cx, |t, _| t.bounds.get()) else { return };
            if bw <= 1.0 {
                continue;
            }
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            let Some(handle) = cx.update(|cx| cx.active_window().or_else(|| cx.windows().first().copied())) else { return };
            let at = |f: f32| point(px(bx + bw * f), px(by + bh * 0.5));
            let (btn, m) = (MouseButton::Left, gpui::Modifiers::default());
            let _ = handle.update(cx, |_, window, cx| {
                window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { button: btn, position: at(a), modifiers: m, click_count: 1, first_mouse: false }), cx);
                window.refresh();
            });
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let to = at(b.unwrap_or(a));
            let _ = handle.update(cx, |_, window, cx| {
                window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: to, pressed_button: Some(btn), modifiers: m }), cx);
                window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { button: btn, position: to, modifiers: m, click_count: 1 }), cx);
            });
            let _ = tf.update(cx, |t, _| eprintln!("time probe: window {:?}, {}", t.playback.window(), t.status()));
            return;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_in_local_time_with_the_zone() {
        // 2026-10-06T00:00:00Z is 08:00 in Singapore, 09:00 in Tokyo.
        let ms = 1_791_244_800_000.0;
        assert_eq!(clock(ms), "00:00", "the UTC clock is unchanged");
        assert_eq!(clock_in(ms, 480, "SGT"), "08:00 SGT");
        assert_eq!(clock_in(ms, 540, "JST"), "09:00 JST");
        assert_eq!(date_in(ms - 3_600_000.0, 480), "2026-10-06", "23:00Z the day before is the 6th in Singapore");
        assert_eq!(date_in(ms - 3_600_000.0, 0), "2026-10-05");
        assert_eq!(utc_offset_name(480), "UTC+08");
        assert_eq!(utc_offset_name(330), "UTC+05:30");
        assert_eq!(utc_offset_name(-180), "UTC-03");
        assert_eq!(utc_offset_name(0), "UTC");
        // The machine's own zone: a name with letters, and the clock moved
        // by its offset.
        let (off, abbr) = local_zone(ms);
        assert!(abbr.chars().any(|c| c.is_ascii_alphabetic()), "{abbr}");
        assert_eq!(local_clock(ms), clock_in(ms, off, &abbr));
        assert!(local_datetime(ms).ends_with(&abbr));
    }

    #[test]
    fn a_head_snaps_to_the_nearest_stop_and_blends_between() {
        let stops = [1975.0, 1980.0, 2025.0, 2030.0];
        assert_eq!(snap(&stops, 1977.0), 1975.0);
        assert_eq!(snap(&stops, 1978.0), 1980.0);
        assert_eq!(snap(&stops, 2100.0), 2030.0);
        assert_eq!(snap(&[], 5.0), 5.0);
        assert_eq!(between(&stops, 1977.5), Some((1975.0, 1980.0, 0.5)));
        assert_eq!(between(&stops, 1980.0), Some((1980.0, 2025.0, 0.0)), "on a stop: that stop, nothing of the next");
        assert_eq!(between(&stops, 2030.0), Some((2030.0, 2030.0, 0.0)));
        assert_eq!(between(&stops, 1900.0), Some((1975.0, 1975.0, 0.0)));
    }

    #[test]
    fn the_counter_counts_the_window_and_the_live_spans() {
        let sorted = [1.0, 2.0, 2.0, 5.0, 9.0];
        assert_eq!(rows_in(&sorted, [2.0, 5.0]), 3, "closed at both ends");
        assert_eq!(rows_in(&sorted, [6.0, 8.0]), 0);
        let (starts, ends) = ([0.0, 1.0, 4.0], [2.0, 6.0, 5.0]);
        let mut ends_sorted = ends;
        ends_sorted.sort_by(f64::total_cmp);
        assert_eq!(live_at(&starts, &ends_sorted, 1.5), 2);
        assert_eq!(live_at(&starts, &ends_sorted, 4.5), 2);
        assert_eq!(live_at(&starts, &ends_sorted, 7.0), 0);
    }

    #[test]
    fn coverage_counts_each_span_in_every_bin_it_touches() {
        assert_eq!(coverage_histogram(&[[0.0, 4.9], [5.0, 10.0]], [0.0, 10.0], 2), vec![1, 1]);
        assert_eq!(coverage_histogram(&[[2.0, 7.0]], [0.0, 10.0], 5), vec![0, 1, 1, 1, 0]);
    }

    /// Floating (not docked), the card sits above the engine's brand mark
    /// at the map's bottom-left.
    #[test]
    fn the_card_keeps_clear_of_the_brand_mark() {
        use gpui::{AppContext as _, TestAppContext};
        struct Host(gpui::Entity<TimeFilter>);
        impl gpui::Render for Host {
            fn render(&mut self, _w: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
                use gpui::{ParentElement as _, Styled as _};
                gpui::div().relative().size_full().child(self.0.clone())
            }
        }
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let mut app = TestAppContext::single();
        app.update(gpui_component::init);
        let map = app.new(|_| {
            MapView::new(
                0.0,
                0.0,
                2.0,
                800.0,
                600.0,
                gpui_maps::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                std::sync::Arc::new(gpui_maps::tile_source::EmptyTileSource),
                rt.handle().clone(),
            )
        });
        let weak = map.downgrade();
        let (_host, cx) = app.add_window_view(|_, cx| {
            let tf = cx.new(|cx| TimeFilter::for_points(weak, "pts", "Time", [0.0, 1.0e12], [0.0, 1.0e11], vec![5.0e11], "rows", cx));
            Host(tf)
        });
        cx.run_until_parked();
        let height = cx.update(|w, _| f32::from(w.viewport_size().height));
        let b = cx.debug_bounds("time-filter").expect("the card is drawn");
        let (_mx, my, _mw, _mh) = gpui_maps::brand_mark::BrandMark::default().footprint(height);
        let bottom = f32::from(b.origin.y) + f32::from(b.size.height);
        assert!(bottom <= my, "the card reaches down to {bottom}, over the mark's top at {my}");
    }

    /// A drag on the histogram moves the window and sends it to the
    /// target, once per move; a paused card runs no loop.
    #[test]
    fn a_brush_on_the_histogram_sends_the_window() {
        use gpui::{AppContext as _, TestAppContext};
        struct Host(gpui::Entity<TimeFilter>);
        impl gpui::Render for Host {
            fn render(&mut self, _w: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
                use gpui::{ParentElement as _, Styled as _};
                gpui::div().relative().size_full().child(self.0.clone())
            }
        }
        let mut app = TestAppContext::single();
        app.update(gpui_component::init);
        let sent: Rc<std::cell::RefCell<Vec<[f64; 2]>>> = Rc::default();
        let log = sent.clone();
        let (host, cx) = app.add_window_view(|_, cx| {
            let target = TimeTarget::Custom(Rc::new(move |w, _| log.borrow_mut().push(w)));
            let counter = Counter::Text(Rc::new(|w| format!("{:.0}", w[1])));
            let tf = cx.new(|cx| {
                TimeFilter::new(WeakEntity::new_invalid(), target, WindowMode::Range, "T", [0.0, 100.0], [0.0, 10.0], vec![1.0; 10], counter, TimeFormat::plain(|v| format!("{v:.0}")), cx)
            });
            Host(tf)
        });
        cx.run_until_parked();
        let before = sent.borrow().len();
        let h = cx.debug_bounds("time-filter-hist").expect("the histogram is drawn");
        let at = |f: f32| point(h.origin.x + h.size.width * f, h.origin.y + h.size.height / 2.0);
        cx.simulate_mouse_down(at(0.5), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(at(0.7), Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(at(0.7), MouseButton::Left, gpui::Modifiers::default());
        let last = *sent.borrow().last().expect("sent");
        assert!((last[0] - 50.0).abs() < 1.5 && (last[1] - 70.0).abs() < 1.5, "{last:?}");
        assert!(sent.borrow().len() > before);
        let tf = host.read_with(cx, |h, _| h.0.clone());
        assert_eq!(tf.read_with(cx, |t, _| t.frames), 0, "paused: no playback frames");
        // While playing: the press pauses, and the release plays on from
        // the window it left, width kept.
        tf.update(cx, |t, cx| t.toggle(cx));
        cx.simulate_mouse_down(at(0.6), MouseButton::Left, gpui::Modifiers::default());
        assert!(!tf.read_with(cx, |t, _| t.playback.is_playing()), "the press paused");
        cx.simulate_mouse_move(at(0.3), Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(at(0.3), MouseButton::Left, gpui::Modifiers::default());
        let w = tf.read_with(cx, |t, _| t.playback.window());
        assert!(tf.read_with(cx, |t, _| t.playback.is_playing()), "plays on after the release");
        assert!((w[0] - 20.0).abs() < 1.5 && (w[1] - w[0] - 20.0).abs() < 1e-3, "{w:?}");
        tf.update(cx, |t, cx| t.toggle(cx));
    }
}
