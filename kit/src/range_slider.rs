//! The Studio's one from-to slider: two handles on a track with the
//! selected span between them, snapped to a step (an hour, a minute).
//! Every two-handle time window in the Studio (the transit explorer's
//! hours, Vehicle GPS's hours) is one of these; the time slider card's
//! histogram brush (`time_filter.rs`) follows the same rules through
//! [`brush`].
//!
//! - **Drag inside the span**: the whole window moves, its width kept,
//!   clamped at the track's ends (cursor: a grab hand, grabbing while held).
//! - **Drag a handle**: that end moves (cursor: `ew-resize`); the ends never
//!   cross and stay at least `min_span` apart.
//! - **Press the empty track**: the nearer handle jumps there, then drags.
//! - **Double-click the span**: the whole track ("All day").
//! - **Left / Right** (focused, after a press or Tab): the whole window
//!   moves a step; **Home / End** puts it at a track end.
//!
//! The host hears [`RangeSliderEvent::Grabbed`] when a press starts (a
//! playing host pauses), [`RangeSliderEvent::Changed`] with the new window
//! when one lands (on release, a key or a double-click; never per pixel, so
//! a window that triggers a query reads once), and
//! [`RangeSliderEvent::Released`] after the press ends (a host that paused
//! resumes from the new window). The host's own changes come back through
//! [`RangeSlider::set_window`], which emits nothing.
#![allow(dead_code)]

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, div, px, relative, App, Context, CursorStyle, DispatchPhase, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render, SharedString,
    Styled as _, Window,
};
use gpui_component::ActiveTheme as _;

use crate::brush;

/// The track's height: the row it sits in never changes height, held or not.
pub const HEIGHT: f32 = 24.0;
/// A handle's diameter.
pub const THUMB: f32 = 16.0;

/// What the host hears.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RangeSliderEvent {
    /// A press began (on the span, a handle or the track).
    Grabbed,
    /// A new window landed: sorted, snapped, inside the domain.
    Changed([f64; 2]),
    /// The press ended (after its `Changed`, when it changed anything).
    Released,
}

/// What a held press moves.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grab {
    /// The whole window, held `offset` after its low end.
    Move { offset: f64 },
    /// One end.
    Edge(brush::Side),
}

#[derive(Clone, Copy, Debug)]
struct Held {
    grab: Grab,
    /// The window when the press began: the release says whether it moved.
    from: [f64; 2],
}

pub struct RangeSlider {
    id: SharedString,
    domain: [f64; 2],
    step: f64,
    min_span: f64,
    window: [f64; 2],
    held: Option<Held>,
    /// What the pointer is over with no button held: the cursor.
    hover: brush::Hit,
    /// The track's `(x, width)` in window pixels, from the last paint.
    bounds: Rc<Cell<(f32, f32)>>,
    /// The window-level drag listeners are in (a drag faster than a frame
    /// falls back to the element's own).
    listening: Rc<Cell<bool>>,
    focus: FocusHandle,
}

impl EventEmitter<RangeSliderEvent> for RangeSlider {}

impl Focusable for RangeSlider {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl RangeSlider {
    /// A slider over `domain` snapping to `step`, the ends at least one step
    /// apart, showing `window`. `id` names its debug selectors: `id` (the
    /// track), `id-span`, `id-lo`, `id-hi`.
    pub fn new(id: impl Into<SharedString>, domain: [f64; 2], step: f64, window: [f64; 2], cx: &mut Context<Self>) -> Self {
        let step = step.max(f64::EPSILON);
        let mut s = Self {
            id: id.into(),
            domain,
            step,
            min_span: step,
            window,
            held: None,
            hover: brush::Hit::Outside,
            bounds: Rc::new(Cell::new((0.0, 1.0))),
            listening: Rc::default(),
            focus: cx.focus_handle(),
        };
        s.window = s.fit(window);
        s
    }

    /// The narrowest window (default: one step).
    pub fn min_span(mut self, span: f64) -> Self {
        self.min_span = span.max(0.0);
        self.window = self.fit(self.window);
        self
    }

    pub fn window(&self) -> [f64; 2] {
        self.window
    }

    pub fn is_held(&self) -> bool {
        self.held.is_some()
    }

    /// The host's own window (a filter set elsewhere): shown, not emitted.
    /// A held press keeps what the pointer has.
    pub fn set_window(&mut self, w: [f64; 2], cx: &mut Context<Self>) {
        if self.held.is_some() {
            return;
        }
        let w = self.fit(w);
        if w != self.window {
            self.window = w;
            cx.notify();
        }
    }

    fn snap(&self, v: f64) -> f64 {
        let [d0, d1] = self.domain;
        (d0 + ((v - d0) / self.step).round() * self.step).clamp(d0, d1)
    }

    /// Sorted, snapped, inside the domain and at least `min_span` wide.
    fn fit(&self, [a, b]: [f64; 2]) -> [f64; 2] {
        let [d0, d1] = self.domain;
        let (lo, hi) = (self.snap(a.min(b)), self.snap(a.max(b)));
        let min = self.min_span.min(d1 - d0);
        if hi - lo >= min {
            return [lo, hi];
        }
        let lo = lo.min(d1 - min);
        [lo, lo + min]
    }

    /// The window moved by `delta`, its width kept, clamped to the domain.
    pub fn shifted(&self, delta: f64) -> [f64; 2] {
        let [d0, d1] = self.domain;
        let [lo, hi] = self.window;
        let width = hi - lo;
        let lo = self.snap(lo + delta).clamp(d0, (d1 - width).max(d0));
        [lo, lo + width]
    }

    /// The domain value under window x `x`.
    fn value_at(&self, x: f32) -> f64 {
        let (bx, bw) = self.bounds.get();
        let t = f64::from(((x - bx) / bw.max(1.0)).clamp(0.0, 1.0));
        let [d0, d1] = self.domain;
        d0 + t * (d1 - d0)
    }

    /// How near an end, in the domain's units, grabs that end: half a
    /// handle, but never more than a third of the span, so a narrow window
    /// (one hour of 24) still has a middle to drag.
    fn edge_tol(&self) -> f64 {
        let (_, bw) = self.bounds.get();
        let [d0, d1] = self.domain;
        let per_px = (d1 - d0) / f64::from(bw.max(1.0));
        let span_px = (self.window[1] - self.window[0]) / per_px;
        (f64::from(THUMB / 2.0)).min(span_px / 3.0) * per_px
    }

    fn hit(&self, v: f64) -> brush::Hit {
        brush::hit(Some(self.window), v, self.edge_tol())
    }

    fn land(&mut self, w: [f64; 2], cx: &mut Context<Self>) {
        let w = self.fit(w);
        if w != self.window {
            self.window = w;
            cx.emit(RangeSliderEvent::Changed(w));
            cx.notify();
        }
    }

    /// A press at window x `x` (`clicks`: 2 for a double-click).
    pub fn press(&mut self, x: f32, clicks: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let v = self.value_at(x);
        let [lo, hi] = self.window;
        let hit = self.hit(v);
        if clicks >= 2 && hit == brush::Hit::Inside {
            self.land(self.domain, cx);
            return;
        }
        let grab = match hit {
            brush::Hit::Inside => Grab::Move { offset: v - lo },
            brush::Hit::Edge(side) => Grab::Edge(side),
            brush::Hit::Outside => Grab::Edge(if (v - lo).abs() <= (v - hi).abs() { brush::Side::Low } else { brush::Side::High }),
        };
        self.held = Some(Held { grab, from: self.window });
        cx.emit(RangeSliderEvent::Grabbed);
        // The empty track: the nearer handle comes to the press.
        if hit == brush::Hit::Outside {
            self.drag(x, cx);
        }
        cx.notify();
    }

    /// The pointer at window x `x` with the press held: the window shown
    /// follows (it lands on release).
    pub fn drag(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(held) = self.held else { return };
        let v = self.value_at(x);
        let [d0, d1] = self.domain;
        let [lo, hi] = self.window;
        let next = match held.grab {
            Grab::Move { offset } => {
                let width = hi - lo;
                let lo = self.snap(v - offset).clamp(d0, (d1 - width).max(d0));
                [lo, lo + width]
            }
            Grab::Edge(brush::Side::Low) => [self.snap(v).clamp(d0, hi - self.min_span), hi],
            Grab::Edge(brush::Side::High) => [lo, self.snap(v).clamp(lo + self.min_span, d1)],
        };
        if next != self.window {
            self.window = next;
            cx.notify();
        }
    }

    pub fn release(&mut self, cx: &mut Context<Self>) {
        let Some(held) = self.held.take() else { return };
        if self.window != held.from {
            cx.emit(RangeSliderEvent::Changed(self.window));
        }
        cx.emit(RangeSliderEvent::Released);
        cx.notify();
    }

    fn hover_at(&mut self, x: f32, cx: &mut Context<Self>) {
        let hit = self.hit(self.value_at(x));
        if hit != self.hover {
            self.hover = hit;
            cx.notify();
        }
    }

    /// Left / Right move the window a step; Home / End to a track end.
    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let [d0, d1] = self.domain;
        let to = match event.keystroke.key.as_str() {
            "left" => self.shifted(-self.step),
            "right" => self.shifted(self.step),
            "home" => self.shifted(d0 - d1),
            "end" => self.shifted(d1 - d0),
            _ => return false,
        };
        self.land(to, cx);
        true
    }
}

impl Render for RangeSlider {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bar: gpui::Hsla = theme.tokens.slider_bar.into();
        let thumb_bg: gpui::Hsla = theme.tokens.slider_thumb.into();
        let ring = theme.ring;
        let focused = self.focus.is_focused(window);
        let [d0, d1] = self.domain;
        let span = (d1 - d0).max(f64::MIN_POSITIVE);
        let [lo, hi] = self.window;
        let (l, r) = (((lo - d0) / span) as f32, ((hi - d0) / span) as f32);
        let hit = match self.held.map(|h| h.grab) {
            Some(Grab::Move { .. }) => None,
            Some(Grab::Edge(_)) => Some(brush::Hit::Edge(brush::Side::Low)),
            None => Some(self.hover),
        };
        let cursor = match hit {
            None => CursorStyle::ClosedHand,
            Some(brush::Hit::Inside) => CursorStyle::OpenHand,
            Some(brush::Hit::Edge(_)) => CursorStyle::ResizeLeftRight,
            Some(brush::Hit::Outside) => CursorStyle::PointingHand,
        };
        let bounds = self.bounds.clone();
        let probe = canvas(move |b, _, _| bounds.set((f32::from(b.origin.x), f32::from(b.size.width))), |_, _, _, _| {}).absolute().top_0().left_0().size_full();
        let entity = cx.entity().downgrade();
        let listening = self.listening.clone();
        listening.set(false);
        let drag_listeners = self.held.is_some().then(|| {
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
        let id = self.id.clone();
        let thumb = |side: &'static str, at: f32| {
            let sel = format!("{id}-{side}");
            div()
                .debug_selector(move || sel.clone())
                .absolute()
                .top(px((HEIGHT - THUMB) / 2.0))
                .left(relative(at))
                .ml(px(-THUMB / 2.0))
                .size(px(THUMB))
                .rounded_full()
                .p(px(1.0))
                .bg(bar.opacity(0.5))
                .child(div().size_full().rounded_full().bg(thumb_bg))
        };
        let span_sel = format!("{id}-span");
        let track_sel = id.to_string();
        div()
            .id(self.id.clone())
            .debug_selector(move || track_sel.clone())
            .track_focus(&self.focus)
            .relative()
            .w_full()
            .h(px(HEIGHT))
            .flex_none()
            .cursor(cursor)
            .child(probe)
            // The track.
            .child(div().absolute().left_0().right_0().top(px((HEIGHT - 6.0) / 2.0)).h(px(6.0)).rounded_full().bg(bar.opacity(0.2)))
            // The span: the grab area is the track's full height.
            .child(
                div()
                    .debug_selector(move || span_sel.clone())
                    .absolute()
                    .top_0()
                    .h_full()
                    .left(relative(l))
                    .w(relative(r - l))
                    .flex()
                    .items_center()
                    .child(div().w_full().h(px(6.0)).rounded_full().bg(bar).when(focused, |d| d.border_1().border_color(ring))),
            )
            .child(thumb("lo", l))
            .child(thumb("hi", r))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.press(f32::from(event.position.x), event.click_count, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if event.pressed_button.is_none() {
                    this.hover_at(f32::from(event.position.x), cx);
                } else if event.pressed_button == Some(MouseButton::Left) && this.held.is_some() && !this.listening.get() {
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if this.key(event, cx) {
                    cx.stop_propagation();
                }
            }))
            .children(drag_listeners)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, AppContext as _, Modifiers, TestAppContext, VisualTestContext};

    struct Host {
        slider: gpui::Entity<RangeSlider>,
        log: Rc<std::cell::RefCell<Vec<RangeSliderEvent>>>,
        _sub: gpui::Subscription,
    }
    impl Render for Host {
        fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().p(px(20.0)).w(px(280.0)).child(self.slider.clone())
        }
    }

    /// A 240 px slider over 0..24 hours showing `w`.
    fn host(app: &mut TestAppContext, w: [f64; 2]) -> (gpui::Entity<Host>, &mut VisualTestContext) {
        app.update(gpui_component::init);
        let (host, cx) = app.add_window_view(|_, cx| {
            let slider = cx.new(|cx| RangeSlider::new("rs", [0.0, 24.0], 1.0, w, cx));
            let log: Rc<std::cell::RefCell<Vec<RangeSliderEvent>>> = Rc::default();
            let l = log.clone();
            let _sub = cx.subscribe(&slider, move |_, _, ev: &RangeSliderEvent, _| l.borrow_mut().push(*ev));
            Host { slider, log, _sub }
        });
        cx.run_until_parked();
        (host, cx)
    }

    fn at(cx: &mut VisualTestContext, hour: f64) -> gpui::Point<gpui::Pixels> {
        let b = cx.debug_bounds("rs").expect("drawn");
        point(b.origin.x + b.size.width * (hour / 24.0) as f32, b.origin.y + b.size.height / 2.0)
    }

    fn drag(cx: &mut VisualTestContext, from: f64, to: f64) {
        let (a, b) = (at(cx, from), at(cx, to));
        cx.simulate_mouse_down(a, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(b, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(b, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
    }

    fn shown(host: &gpui::Entity<Host>, cx: &mut VisualTestContext) -> [f64; 2] {
        let s = host.read_with(cx, |h, _| h.slider.clone());
        s.read_with(cx, |s, _| s.window())
    }

    fn events(host: &gpui::Entity<Host>, cx: &mut VisualTestContext) -> Vec<RangeSliderEvent> {
        host.read_with(cx, |h, _| h.log.borrow().clone())
    }

    /// The middle drags both ends by the same hours; the width is kept, and
    /// one Changed lands on release between Grabbed and Released.
    #[test]
    fn dragging_the_middle_moves_both_ends() {
        let mut app = TestAppContext::single();
        let (host, cx) = host(&mut app, [7.0, 10.0]);
        drag(cx, 8.5, 12.5);
        assert_eq!(shown(&host, cx), [11.0, 14.0]);
        assert_eq!(events(&host, cx), vec![RangeSliderEvent::Grabbed, RangeSliderEvent::Changed([11.0, 14.0]), RangeSliderEvent::Released]);
    }

    /// A one-hour window still has a middle to drag.
    #[test]
    fn a_one_hour_window_drags_by_its_middle() {
        let mut app = TestAppContext::single();
        let (host, cx) = host(&mut app, [4.0, 5.0]);
        drag(cx, 4.5, 9.5);
        assert_eq!(shown(&host, cx), [9.0, 10.0]);
    }

    /// Dragged past either end, the window stops there, width kept.
    #[test]
    fn the_window_clamps_at_both_ends() {
        let mut app = TestAppContext::single();
        let (host, cx) = host(&mut app, [7.0, 10.0]);
        drag(cx, 8.5, 30.0);
        assert_eq!(shown(&host, cx), [21.0, 24.0]);
        drag(cx, 22.5, -5.0);
        assert_eq!(shown(&host, cx), [0.0, 3.0]);
    }

    /// A handle resizes from its end only; the ends never cross.
    #[test]
    fn a_handle_drag_resizes() {
        let mut app = TestAppContext::single();
        let (host, cx) = host(&mut app, [7.0, 10.0]);
        drag(cx, 10.0, 15.0);
        assert_eq!(shown(&host, cx), [7.0, 15.0]);
        drag(cx, 7.0, 3.0);
        assert_eq!(shown(&host, cx), [3.0, 15.0]);
        drag(cx, 3.0, 20.0);
        assert_eq!(shown(&host, cx), [14.0, 15.0], "stops a step short of the other end");
    }

    /// A press on the empty track brings the nearer handle there.
    #[test]
    fn the_empty_track_moves_the_nearest_handle() {
        let mut app = TestAppContext::single();
        let (host, cx) = host(&mut app, [7.0, 10.0]);
        let p = at(cx, 18.2);
        cx.simulate_click(p, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(shown(&host, cx), [7.0, 18.0]);
        let p = at(cx, 2.0);
        cx.simulate_click(p, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(shown(&host, cx), [2.0, 18.0]);
    }

    /// The arrows move the focused window a step; Home / End to an end;
    /// a double-click on the span is the whole day.
    #[test]
    fn keys_and_double_click() {
        let mut app = TestAppContext::single();
        let (host, cx) = host(&mut app, [7.0, 10.0]);
        drag(cx, 8.5, 8.5); // a press focuses it
        cx.simulate_keystrokes("right right");
        assert_eq!(shown(&host, cx), [9.0, 12.0]);
        cx.simulate_keystrokes("left");
        assert_eq!(shown(&host, cx), [8.0, 11.0]);
        cx.simulate_keystrokes("end");
        assert_eq!(shown(&host, cx), [21.0, 24.0]);
        cx.simulate_keystrokes("home");
        assert_eq!(shown(&host, cx), [0.0, 3.0]);
        let p = at(cx, 1.5);
        cx.simulate_event(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: 2, first_mouse: false });
        cx.simulate_event(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: 2 });
        cx.run_until_parked();
        assert_eq!(shown(&host, cx), [0.0, 24.0]);
    }

    /// Nothing moves the slider's box: held, dragged or released.
    #[test]
    fn the_layout_does_not_shift() {
        let mut app = TestAppContext::single();
        let (_host, cx) = host(&mut app, [7.0, 10.0]);
        let before = cx.debug_bounds("rs").expect("drawn");
        let (a, b) = (at(cx, 8.5), at(cx, 15.0));
        cx.simulate_mouse_down(a, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(b, Some(MouseButton::Left), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("rs"), Some(before), "held");
        cx.simulate_mouse_up(b, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("rs"), Some(before), "released");
        assert_eq!(f32::from(before.size.height), HEIGHT);
    }
}
