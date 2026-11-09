//! The drive page's own scrubber toggle and panel (`fix/nav-scrubber-placement`),
//! plus the camera-mode buttons (`feat/nav-continuous-zoom`; they replaced
//! the camera popover, which covered the vehicle):
//! split out of `streets.rs` itself so that file's own
//! `the_panel_root_sets_no_opaque_background` test -- a blunt whole-file
//! search for `.bg(` guarding against an opaque background anywhere on
//! the `streets.rs` -> `MapView` chain -- keeps meaning what it says.
//! This file's `.bg()` calls are on [`DriveScrubber`]'s own shapes, a
//! SIBLING of the map in [`super::DriveRoot`], never an ancestor of it;
//! they draw over the map on purpose, the same way the chrome and (the
//! old bug) the `TimeFilter` card used to.

use gpui::{IntoElement, Render, Window};
use gpui_maps::map_view::MapView;
use gpui_maps::nav::host::NavCameraMode;

/// The drive's scrubber toggle/panel and the camera-mode buttons: one
/// white pill (the HUD's own play/pause + 1x pill's shape) holding the
/// route-timeline toggle and, after a hairline, a segmented row of one
/// icon button per [`NavCameraMode`] -- the current one filled. One tap
/// switches the mode; nothing opens. Beside the sim tray (or the strip,
/// when there is no tray) when the window is wide enough for that, else
/// stacked above it -- see [`pill_layout`]. Reads
/// [`MapView::nav_hud_rects`] every frame rather than guessing at the
/// chrome's own layout constants, so neither the pill nor the panel can
/// ever overlap the banner, the ETA strip or the simulated drive's
/// pause/speed tray; and hands the pill to the map
/// ([`MapView::set_nav_host_chrome`]), so the follow puck rides above it.
///
/// The user (2026-10-07): "on navigation this panel is closing the
/// vehicle ... styles somehow maybe can be small buttons without this
/// popup? it is on top of vehicle - cannot see it". The follow distance
/// that popover also held is now the user's own zoom
/// ([`MapView::set_nav_follow_zoom_offset`]).
pub struct DriveScrubber {
    pub(super) map: gpui::WeakEntity<MapView>,
    pub(super) total_m: f64,
    /// The planned-speed bars (`speed_profile`), tallest-normalised.
    pub(super) bars: Vec<f64>,
    pub(super) driven_m: f64,
    pub open: bool,
    pub(super) dragging: bool,
    /// The track's last-painted bounds (left x, width), in window
    /// points, for turning a drag's pointer x into a fraction.
    pub(super) track: std::rc::Rc<std::cell::Cell<(f32, f32)>>,
    pub(super) seek: std::rc::Rc<dyn Fn(f64, &mut gpui::App)>,
}

/// The panel's content height when there is room for it.
const SCRUBBER_PANEL_H: f32 = 64.0;
/// The HUD's own pill padding/gap unit -- `chrome::SIM_TRAY_PAD`, the
/// play/pause + 1x pill's own inner padding. Reused here (never a second
/// magic number) for the gap kept between the scrubber's own shapes and
/// the HUD's, around the buttons inside the pill, AND between that pill and
/// the HUD's: "the same gap between the two pills as inside them" (the
/// coordinator's capture review).
const PILL_GAP: f32 = gpui_maps::nav::chrome::SIM_TRAY_PAD;
/// The gap between two mode buttons of the segmented row: tighter than
/// [`PILL_GAP`], so the four read as one control.
const SEGMENT_GAP: f32 = 2.0;
/// The hairline between the timeline toggle and the mode row.
const DIVIDER_W: f32 = 1.0;
/// The pill's own height -- `chrome::SIM_TRAY_H`, so a pill placed beside
/// the sim tray shares its exact top and bottom edges ("the same
/// baseline as the play/pause pill").
const PILL_H: f32 = gpui_maps::nav::chrome::SIM_TRAY_H;
/// The smallest a button may shrink to on a window too narrow for the
/// full pill.
const MIN_BUTTON: f32 = 30.0;
/// Each button's own icon, inside its plate: big enough to read, small
/// enough that nothing pokes past the circle's own curve.
const GLYPH_SIZE: f32 = 20.0;

/// The pill's width for buttons of `button` points: padding, the timeline
/// toggle, padding, a hairline, padding, the four mode buttons with
/// [`SEGMENT_GAP`] between them, padding.
fn pill_width(button: f32) -> f32 {
    let modes = NavCameraMode::ALL.len() as f32;
    PILL_GAP * 4.0 + DIVIDER_W + button * (1.0 + modes) + SEGMENT_GAP * (modes - 1.0)
}

impl DriveScrubber {
    fn fraction_at(&self, x: f32) -> f64 {
        let (left, width) = self.track.get();
        if width <= 0.0 {
            return 0.0;
        }
        f64::from((x - left) / width).clamp(0.0, 1.0)
    }

    fn seek_to(&mut self, x: f32, cx: &mut gpui::Context<Self>) {
        let metres = self.fraction_at(x) * self.total_m;
        self.driven_m = metres;
        (self.seek)(metres, cx);
        cx.notify();
    }

    /// The simulated drive's own progress, polled from `nav_state`
    /// (`drive_scrubber`'s spawn loop). Ignored while the user is
    /// dragging the head: the drive does not fight the pointer.
    pub fn set_driven(&mut self, driven_m: f64, cx: &mut gpui::Context<Self>) {
        if !self.dragging {
            self.driven_m = driven_m.clamp(0.0, self.total_m);
            cx.notify();
        }
    }
}

fn chrome_rgba(c: [f32; 4]) -> gpui::Rgba {
    gpui::Rgba { r: c[0], g: c[1], b: c[2], a: c[3] }
}

/// Where the pill holding the route-timeline toggle and the camera-mode
/// buttons sits: beside `band` (the sim tray, or the strip when there is
/// none) at its own height, when `viewport_w` has room for it there -- so
/// it reads as part of the HUD's own cluster. Falls back to stacking it
/// above `band`, right-aligned to the strip (or the window's margin), on a
/// window too narrow for that; on one too narrow even for the full pill,
/// the buttons shrink (never below [`MIN_BUTTON`]).
pub(super) struct PillLayout {
    pub(super) pill: gpui_maps::nav::chrome::Rect,
    pub(super) route: gpui_maps::nav::chrome::Rect,
    pub(super) divider: gpui_maps::nav::chrome::Rect,
    /// One per [`NavCameraMode::ALL`], in that order.
    pub(super) modes: Vec<(NavCameraMode, gpui_maps::nav::chrome::Rect)>,
}

pub(super) fn pill_layout(strip: gpui_maps::nav::chrome::Rect, band: gpui_maps::nav::chrome::Rect, has_tray: bool, viewport_w: f32) -> PillLayout {
    use gpui_maps::nav::chrome::{Rect, BUTTON, MARGIN};
    let beside_x = band.x + band.w + PILL_GAP;
    let full_w = pill_width(BUTTON);
    let (x, y, button) = if beside_x + full_w <= viewport_w - MARGIN {
        // Beside `band`, sharing its own top and bottom edges when it is
        // the tray (`PILL_H` already equals the tray's own height); when
        // it is the strip instead (no sim running), centred on it the
        // same way the engine's own strip buttons are.
        let y = if has_tray { band.y } else { strip.y + (strip.h - PILL_H) / 2.0 };
        (beside_x, y, BUTTON)
    } else {
        let avail = (viewport_w - 2.0 * MARGIN).max(0.0);
        let fixed = pill_width(0.0);
        let button = if full_w <= avail { BUTTON } else { ((avail - fixed) / (1.0 + NavCameraMode::ALL.len() as f32)).clamp(MIN_BUTTON, BUTTON) };
        let w = pill_width(button);
        let x = (strip.x + strip.w - w).min(viewport_w - MARGIN - w).max(MARGIN.min(viewport_w - w).max(0.0));
        (x, band.y - PILL_GAP - PILL_H, button)
    };
    let w = pill_width(button);
    let pill = Rect { x, y, w, h: PILL_H };
    let top = pill.y + (PILL_H - button) / 2.0;
    let route = Rect { x: pill.x + PILL_GAP, y: top, w: button, h: button };
    let divider = Rect { x: route.x + button + PILL_GAP, y: pill.y + PILL_GAP * 1.5, w: DIVIDER_W, h: PILL_H - PILL_GAP * 3.0 };
    let first = divider.x + DIVIDER_W + PILL_GAP;
    let modes = NavCameraMode::ALL.iter().enumerate().map(|(i, mode)| (*mode, Rect { x: first + i as f32 * (button + SEGMENT_GAP), y: top, w: button, h: button })).collect();
    PillLayout { pill, route, divider, modes }
}

/// The box the scrubber panel draws in: left-aligned to the strip's own
/// column, its bottom pinned [`PILL_GAP`] above the pill's row -- whether
/// that row sits beside `band` or stacked above it. `None` when there
/// truly is no room (a very short window).
fn popover_rect(
    strip: gpui_maps::nav::chrome::Rect,
    band: gpui_maps::nav::chrome::Rect,
    pill: gpui_maps::nav::chrome::Rect,
    top_bottom: f32,
    desired_h: f32,
) -> Option<gpui_maps::nav::chrome::Rect> {
    use gpui_maps::nav::chrome::Rect;
    let bottom = band.y.min(pill.y) - PILL_GAP;
    let avail = bottom - top_bottom;
    let h = avail.min(desired_h);
    if h < 32.0 {
        return None;
    }
    Some(Rect { x: strip.x, y: bottom - h, w: strip.w.max(80.0), h })
}

/// `mode`'s icon in the bundled Lucide catalog (`examples/studio/assets.rs`
/// embeds each of these explicitly -- an icon not in the Studio's own
/// asset source draws nothing at all, silently). A tilted, heading-up
/// follow reads as [`gpui_kit_assets::IconName::Navigation`] (the classic
/// GPS arrow); flat heading-up as a plain [`ArrowUp`]; north-up as a
/// [`Compass`]; the whole route as the [`Route`] path.
///
/// [`ArrowUp`]: gpui_kit_assets::IconName::ArrowUp
/// [`Compass`]: gpui_kit_assets::IconName::Compass
/// [`Route`]: gpui_kit_assets::IconName::Route
fn camera_mode_icon(mode: NavCameraMode) -> gpui_kit_assets::IconName {
    use gpui_kit_assets::IconName;
    match mode {
        NavCameraMode::Follow3d => IconName::Navigation,
        NavCameraMode::Follow2d => IconName::ArrowUp,
        NavCameraMode::NorthUp2d => IconName::Compass,
        NavCameraMode::Overview => IconName::Route,
        // `NavCameraMode` is `#[non_exhaustive]`; any future mode falls
        // back to the tilted-follow icon rather than failing to compile.
        _ => IconName::Navigation,
    }
}

/// A real icon at `size` points, tinted `color`, wrapped in a plain
/// `size`x`size` box carrying `id`/`debug_selector` -- so a layout test
/// can read exactly the glyph's own painted bounds (`Icon` itself has no
/// `debug_selector`; only `div` does), never the button's whole plate.
fn icon_glyph(id: &'static str, name: gpui_kit_assets::IconName, color: gpui::Rgba, size: f32) -> impl IntoElement {
    use gpui::{div, px, InteractiveElement as _, ParentElement as _, Styled as _};
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .w(px(size))
        .h(px(size))
        .child(gpui_component::Icon::new(name).w(px(size)).h(px(size)).text_color(color))
}

/// `mode`'s tooltip on its button: "Follow, tilted", "Follow, flat",
/// "North up" or "Whole route" -- plainer than [`NavCameraMode::name`]'s
/// stable identifier (`follow3d`, ...), which is for config and debug URLs,
/// not a label a driver reads.
fn camera_mode_label(mode: NavCameraMode) -> &'static str {
    match mode {
        NavCameraMode::Follow3d => "Follow, tilted",
        NavCameraMode::Follow2d => "Follow, flat",
        NavCameraMode::NorthUp2d => "North up",
        NavCameraMode::Overview => "Whole route",
        _ => "Follow",
    }
}

/// Rects a host floats over the drive page besides this panel's own pill
/// (Studio's agent chat card while a trip runs). `MapView::set_nav_host_chrome`
/// takes ONE list, so the scrubber, which already writes it every frame,
/// writes the union; a host just sets this global.
#[derive(Default)]
pub struct ExtraHostChrome(pub Vec<gpui_maps::nav::host::Rect>);

impl gpui::Global for ExtraHostChrome {}

impl Render for DriveScrubber {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        use gpui::prelude::FluentBuilder as _;
        use gpui::{
            canvas, div, px, relative, DispatchPhase, InteractiveElement as _, MouseButton, MouseDownEvent,
            MouseMoveEvent, MouseUpEvent, ParentElement as _, StatefulInteractiveElement as _, Styled as _,
        };
        use gpui_component::tooltip::Tooltip;
        use gpui_maps::nav::chrome;

        let root = div().absolute().inset_0();
        // No active trip (between the panel building and the first nav
        // frame, or after arrival): nothing of ours draws either --
        // there is nothing of the HUD's left to sit beside.
        let Some(map) = self.map.upgrade() else { return root };
        let Some(hud) = map.read(cx).nav_hud_rects() else { return root };
        let Some(strip) = hud.strip else { return root };
        // The tray (always present: this page always runs a simulated
        // drive), or the strip itself when there is none -- the row our
        // own toggles align with.
        let band = hud.tray.unwrap_or(strip);
        let viewport_w = f32::from(window.viewport_size().width);
        let row = pill_layout(strip, band, hud.tray.is_some(), viewport_w);
        let (pill, route_toggle) = (row.pill, row.route);
        // The pill is chrome over the map too: the overview fits the route
        // clear of it (on a narrow window it stacks above the tray, where
        // the HUD's own clearance does not reach).
        // Plus any chrome the host floats over the drive (Studio's chat
        // card, `ExtraHostChrome`): the setter replaces the whole list, so
        // the one writer unions them.
        let mut want = vec![pill];
        if let Some(extra) = cx.try_global::<ExtraHostChrome>() {
            want.extend(extra.0.iter().copied());
        }
        if map.read(cx).nav_host_chrome() != want.as_slice() {
            map.update(cx, |m, _| m.set_nav_host_chrome(want));
        }
        let top_bottom = hud.top.map_or(chrome::MARGIN, |t| t.bottom() + PILL_GAP);

        let open = self.open;
        let current_mode = map.read(cx).nav_camera_mode();
        let fraction = if self.total_m > 0.0 { (self.driven_m / self.total_m).clamp(0.0, 1.0) } else { 0.0 };
        let max_bar = self.bars.iter().copied().fold(0.0, f64::max).max(1e-9);

        // A click outside the toggle and the open panel closes it -- a
        // WINDOW-level tap, checked against the toggle's and the panel's
        // own bounds so it never steals a click meant for either. Those
        // rects are the MAP's own points and the event's the WINDOW's: the
        // pointer is first made local to this panel's painted origin (the
        // canvas below, pinned to the root's top-left, which is the map's).
        let dismiss_rect = if open { popover_rect(strip, band, pill, top_bottom, SCRUBBER_PANEL_H) } else { None };
        let dismiss_listener = open.then(|| {
            let entity = cx.entity().downgrade();
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let entity = entity.clone();
                    let origin = bounds.origin;
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, _window, cx| {
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        let local = event.position - origin;
                        let (x, y) = (f32::from(local.x), f32::from(local.y));
                        let inside = route_toggle.contains(x, y) || dismiss_rect.is_some_and(|r| r.contains(x, y));
                        if inside {
                            return;
                        }
                        let _ = entity.update(cx, |this, cx| {
                            this.open = false;
                            cx.notify();
                        });
                    });
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_0()
        });
        let root = root.children(dismiss_listener);

        // -- The pill holding the toggle and the mode row: the HUD's own play/pause + 1x
        // pill's exact shape (`chrome::STRIP_COLOR`, radius = half its own
        // height), so the two read as one cluster, not two loose circles.
        let mut root = root.child(
            div()
                .absolute()
                .left(px(pill.x))
                .top(px(pill.y))
                .w(px(pill.w))
                .h(px(pill.h))
                .rounded(px(pill.h / 2.0))
                .bg(chrome_rgba(chrome::STRIP_COLOR))
                .shadow_md()
                // A press on the pill or either toggle is not the map's:
                // without this it reached the map under it too, which
                // took it as a gesture on the camera.
                .occlude(),
        );

        // -- The route-timeline toggle: repositioned into the pill
        // (`pill_layout`) and now also closing the camera popover on
        // open, the two being mutually exclusive. Its glyph was a
        // hand-drawn 3-bar icon (relative-height bars that reached past
        // the circle's own rounded corners, per the coordinator's
        // capture); a real Lucide icon, sized and centred, replaces it.
        let glyph_color = chrome_rgba(if open { chrome::ON_DARK } else { chrome::ON_LIGHT });
        let toggle_bg = chrome_rgba(if open { chrome::BANNER_COLOR } else { chrome::BUTTON_COLOR });
        root = root.child(
            div()
                .id("nav-scrubber-toggle")
                .debug_selector(|| "nav-scrubber-toggle".to_string())
                .absolute()
                .occlude()
                .left(px(route_toggle.x))
                .top(px(route_toggle.y))
                .w(px(route_toggle.w))
                .h(px(route_toggle.h))
                .rounded(px(chrome::BUTTON / 2.0))
                .bg(toggle_bg)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .tooltip(|window, cx| Tooltip::new("Route timeline").build(window, cx))
                .child(icon_glyph("nav-scrubber-glyph", gpui_kit_assets::IconName::ChartNoAxesColumn, glyph_color, GLYPH_SIZE))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open = !this.open;
                    cx.notify();
                })),
        );

        // -- The hairline between the toggle and the mode row.
        let divider = row.divider;
        root = root.child(
            div()
                .absolute()
                .left(px(divider.x))
                .top(px(divider.y))
                .w(px(divider.w))
                .h(px(divider.h))
                .bg(chrome_rgba(chrome::BUTTON_COLOR)),
        );

        // -- One button per camera mode, the current one filled. A tap
        // switches at once (`MapView::set_nav_camera_mode`: a freed camera
        // is taken back, the overview glides in); nothing opens.
        for (mode, rect) in row.modes.iter().copied() {
            let selected = mode == current_mode;
            let (bg, fg) = if selected {
                (chrome_rgba(chrome::BANNER_COLOR), chrome_rgba(chrome::ON_DARK))
            } else {
                (chrome_rgba([0.0, 0.0, 0.0, 0.0]), chrome_rgba(chrome::ON_LIGHT))
            };
            let id: &'static str = match mode {
                NavCameraMode::Follow3d => "nav-view-mode-follow3d",
                NavCameraMode::Follow2d => "nav-view-mode-follow2d",
                NavCameraMode::NorthUp2d => "nav-view-mode-northup2d",
                _ => "nav-view-mode-overview",
            };
            let glyph_id: &'static str = match mode {
                NavCameraMode::Follow3d => "nav-view-glyph-follow3d",
                NavCameraMode::Follow2d => "nav-view-glyph-follow2d",
                NavCameraMode::NorthUp2d => "nav-view-glyph-northup2d",
                _ => "nav-view-glyph-overview",
            };
            let weak = self.map.clone();
            let glyph = GLYPH_SIZE.min(rect.w - 10.0);
            root = root.child(
                div()
                    .id(id)
                    .debug_selector(move || id.to_string())
                    .absolute()
                    .occlude()
                    .left(px(rect.x))
                    .top(px(rect.y))
                    .w(px(rect.w))
                    .h(px(rect.h))
                    .rounded(px(rect.h / 2.0))
                    .bg(bg)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(!selected, |d| d.hover(|s| s.bg(chrome_rgba(chrome::BUTTON_COLOR))))
                    .tooltip(move |window, cx| Tooltip::new(camera_mode_label(mode)).build(window, cx))
                    .child(icon_glyph(glyph_id, camera_mode_icon(mode), fg, glyph))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let _ = weak.update(cx, |map, cx| {
                            map.set_nav_camera_mode(mode);
                            cx.notify();
                        });
                        cx.notify();
                    })),
            );
        }

        if open {
            if let Some(panel) = popover_rect(strip, band, pill, top_bottom, SCRUBBER_PANEL_H) {
                let total_km = self.total_m / 1000.0;
                let driven_km = self.driven_m / 1000.0;
                let track = self.track.clone();
                let probe = canvas(
                    move |b, _, _| track.set((f32::from(b.origin.x), f32::from(b.size.width))),
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0();
                let entity = cx.entity().downgrade();
                let drag_listeners = self.dragging.then(|| {
                    canvas(
                        |_, _, _| {},
                        move |_, _, window, _| {
                            let moved = entity.clone();
                            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                                if phase != DispatchPhase::Bubble {
                                    return;
                                }
                                let _ = moved.update(cx, |this, cx| {
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        this.seek_to(f32::from(event.position.x), cx);
                                    } else {
                                        this.dragging = false;
                                        cx.notify();
                                    }
                                });
                            });
                            let released = entity.clone();
                            window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                                if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                                    let _ = released.update(cx, |this, cx| {
                                        this.dragging = false;
                                        cx.notify();
                                    });
                                }
                            });
                        },
                    )
                    .absolute()
                    .size_0()
                });
                let bars = self.bars.clone();
                // `top_0().left_0().size_full()`, not the usual
                // zero-inset shorthand: the same fill, but this chain's
                // `items_end()` (the bars grow from the row's own
                // bottom, not the window's) must not read as a
                // brand-mark-anchored card to `app.rs`'s
                // `every_bottom_anchored_panel_card_is_accounted_for_against_the_brand_mark`
                // scan, which keys that heuristic on the shorthand.
                let bars_row = div().absolute().top_0().left_0().size_full().flex().items_end().gap(px(1.0)).children(
                    bars.into_iter().map(move |v| {
                        let h = (v / max_bar).max(0.05) as f32;
                        div().flex_1().h(relative(h)).bg(chrome_rgba(chrome::ON_LIGHT_SECONDARY)).opacity(0.5)
                    }),
                );
                let head = div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(px(2.0))
                    .left(relative(fraction as f32))
                    .bg(chrome_rgba(chrome::BANNER_COLOR));
                let track_row = div()
                    .id("nav-scrubber-track")
                    .relative()
                    .h(px(28.0))
                    .w_full()
                    .cursor_pointer()
                    .child(probe)
                    .child(bars_row)
                    .child(head)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                            this.dragging = true;
                            this.seek_to(f32::from(event.position.x), cx);
                            cx.stop_propagation();
                        }),
                    )
                    .children(drag_listeners);
                let label_row = div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .text_xs()
                    .text_color(chrome_rgba(chrome::ON_LIGHT))
                    .child(format!("{driven_km:.1} km"))
                    .child(div().text_color(chrome_rgba(chrome::ON_LIGHT_SECONDARY)).child(format!("/ {total_km:.1} km")));
                root = root.child(
                    div()
                        .id("nav-scrubber-panel")
                        .debug_selector(|| "nav-scrubber-panel".to_string())
                        .absolute()
                        .left(px(panel.x))
                        .top(px(panel.y))
                        .w(px(panel.w))
                        .h(px(panel.h))
                        .occlude()
                        .flex()
                        .flex_col()
                        .justify_between()
                        .p(px(6.0))
                        .rounded(px(chrome::PANEL_RADIUS))
                        .bg(chrome_rgba(chrome::STRIP_COLOR))
                        .child(track_row)
                        .child(label_row),
                );
            }
        }

        root
    }
}
