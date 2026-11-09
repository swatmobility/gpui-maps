//! `MapView`'s altitude layers: objects above the globe (satellites; spec
//! 2026-09-29-satellites-design.md §2.8). The public API and what a frame
//! draws.
//!
//! Drawn by `render::altitude_pipeline::AltitudePipeline` in the symbol
//! pass, right after the donuts and BEFORE the icons and labels: over the
//! tiles and the atmosphere, under the chrome. The brand mark and compass
//! ride in the same icon draw as the basemap icons, and the attribution
//! credit in the same text draw as the labels, so drawing after the labels
//! (spec R6, literally) would put dots over the SWAT mark, the compass and
//! the credit. The cost of this order: a country label draws over a dot
//! rather than under it. A plan ruling (Task 4); splitting the chrome out of
//! those draws is separate work.
//!
//! Only on a globe frame: at `transition == 0` the layer is skipped
//! outright, so a flat frame is bit-identical with and without it (R3).
use super::MapView;
pub use crate::render::altitude_layers::{AltitudePointState, AltitudePositions, SceneClock};
pub(crate) use crate::render::altitude_layers::*;
use crate::render::altitude::{AltitudeLine, RefFrame};
use crate::render::altitude_pipeline::{dot_redraw_after_s, AltitudeLook, AltitudeStyle};

/// Asks the host for the altitude layers' next frame, `wait_s` wall
/// seconds on ([`MapView::altitude_redraw_wait`]): at once when it is due
/// within a frame, otherwise by one delayed wake notifying `handle`'s map
/// (`wake_at` remembers it; a sooner need replaces a later one, and a
/// fired one is forgotten). Takes the fields, not the map, so `paint` can
/// call it while it holds its GPU borrows.
pub(super) fn schedule_altitude_redraw(
    wait_s: Option<f64>,
    wake_at: &mut Option<std::time::Instant>,
    handle: Option<&gpui::WeakEntity<MapView>>,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) -> AltitudeRedraw {
    let Some(wait_s) = wait_s else { return AltitudeRedraw::Idle };
    if wait_s <= 1.0 / 60.0 {
        window.request_animation_frame();
        return AltitudeRedraw::NextFrame;
    }
    let now = std::time::Instant::now();
    let due = now + std::time::Duration::from_secs_f64(wait_s.min(3_600.0));
    if wake_at.is_none_or(|t| t <= now || due < t) {
        *wake_at = Some(due);
        let entity = super::delayed_wake_target(handle, || window.current_view());
        window
            .spawn(cx, async move |cx| {
                cx.background_executor().timer(due.saturating_duration_since(std::time::Instant::now())).await;
                let _ = cx.update(|win, _cx| {
                    win.on_next_frame(move |_, app_cx| app_cx.notify(entity));
                });
            })
            .detach();
    }
    AltitudeRedraw::Wake(wait_s)
}

/// What a paint asked of the host for the altitude layers
/// ([`schedule_altitude_redraw`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum AltitudeRedraw {
    /// Nothing moves: no frame asked for.
    Idle,
    /// Due within a frame: `request_animation_frame`.
    NextFrame,
    /// A delayed wake this many wall seconds on (queued, or one sooner
    /// already is).
    Wake(f64),
}

impl MapView {
    /// Draws `positions` as altitude layer `id` -- dots above the globe,
    /// hidden by the planet where it is in the way -- replacing that
    /// layer's objects if it exists (it keeps its draw position) or adding
    /// it over the others. `style.paint[i]` picks object `i`'s colour and
    /// size; paint indices 15 and up are clamped to 14 (15 holds the
    /// outline). The layer draws on the globe only (it fades in with the
    /// projection's `transition` and is skipped on a flat frame), over the
    /// tiles and the atmosphere, under labels and map chrome. Clears the
    /// layer's visibility mask and emphasis.
    pub fn set_altitude_points(
        &mut self,
        id: &str,
        positions: AltitudePositions,
        style: AltitudeStyle,
        cx: &mut gpui::Context<Self>,
    ) {
        self.altitude_layers.set(id, positions, style);
        cx.notify();
    }

    /// Restyles altitude layer `id` -- colours, sizes,
    /// [`AltitudeLook`], trails -- without touching its positions,
    /// keyframes, mask or emphasis. Nothing happens for an unknown `id`.
    /// The caller is responsible for `cx.notify()`.
    pub fn set_altitude_style(&mut self, id: &str, style: AltitudeStyle) {
        self.altitude_layers.set_style(id, style);
    }

    /// Shows object `i` of layer `id` only where `mask[i]` (group toggles).
    /// Flips flag bits; never rebuilds positions. A mask of the wrong length
    /// is ignored (logged). The caller is responsible for `cx.notify()`.
    pub fn set_altitude_visibility(&mut self, id: &str, mask: &[bool]) {
        self.altitude_layers.set_visibility(id, mask);
    }

    /// Emphasises up to three objects of layer `id`, drawn last and larger
    /// (`render::altitude_pipeline::EMPHASIS_PX`): the hovered one, the
    /// selected one, and `halo` -- an object shown with a white halo
    /// independently of hover and selection (the demo's ISS). An index past
    /// the layer's end is ignored. The caller is responsible for
    /// `cx.notify()`.
    pub fn set_altitude_emphasis(&mut self, id: &str, hovered: Option<usize>, selected: Option<usize>, halo: Option<usize>) {
        self.altitude_layers.set_emphasis(id, (hovered, selected, halo));
    }

    /// Sets the scene clock: what instant keyframed altitude layers (and a
    /// [`Sun::Now`](crate::render::atmosphere::Sun::Now) sun) show, and how
    /// fast it runs. `None`, the default, is real time from the wall clock.
    /// Read once per frame, at the top of `paint`.
    pub fn set_scene_clock(&mut self, clock: Option<SceneClock>) {
        self.scene_clock = clock;
        self.frame_dirty = true;
    }

    /// The scene clock set by [`Self::set_scene_clock`].
    #[must_use]
    pub fn scene_clock(&self) -> Option<SceneClock> {
        self.scene_clock
    }

    /// This frame's scene time, Unix seconds: the value `paint` read once
    /// for the frame, or, before the first paint, the clock (or the wall
    /// clock when there is none) read now.
    #[must_use]
    pub fn scene_time(&self) -> f64 {
        self.frame_scene_unix.unwrap_or_else(|| self.read_scene_clock())
    }

    /// The scene clock read now: the clock's time, or wall UTC.
    pub(super) fn read_scene_clock(&self) -> f64 {
        match self.scene_clock {
            Some(clock) => clock.at(std::time::Instant::now()),
            None => SceneClock::realtime().anchor_scene_unix,
        }
    }

    /// The scene clock's rate: 1 (real time) when there is no clock.
    pub(super) fn scene_rate(&self) -> f64 {
        self.scene_clock.map_or(1.0, |c| c.rate)
    }

    /// Whether the altitude layers need the next frame ([`wants_frames`]):
    /// only for a keyframed layer with something shown. Inertial lines
    /// alone do not: see [`Self::inertial_redraw_after_s`].
    pub(super) fn altitude_animating(&self) -> bool {
        wants_frames(self.scene_rate(), self.altitude_layers.has_shown_keyframed(), self.altitude_layers.waiting)
    }

    /// Wall seconds until the altitude layers need their next frame: now
    /// (`Some(0.0)`) while a layer holds for a keyframe, otherwise
    /// [`dot_redraw_after_s`] of the last drawn motion -- the fastest shown
    /// dot moving `DOT_REDRAW_PX`, an off-screen one entering, an object
    /// appearing or vanishing, the prefetch trigger or the pair end,
    /// whichever is first. `None` when [`Self::altitude_animating`] is
    /// false. A moving dot no longer asks for every display refresh: at
    /// rate 1 on the globe it moves a few px a second, and each frame it
    /// did ask for redrew the host's whole gpui window. A prefetch in
    /// flight asks for nothing more: the pair-end event paints in time to
    /// take it, and a keyframe that is late turns into a hold.
    pub(super) fn altitude_redraw_after_s(&self) -> Option<f64> {
        if !self.altitude_animating() {
            return None;
        }
        if self.altitude_layers.holding {
            return Some(0.0);
        }
        dot_redraw_after_s(self.scene_rate(), self.altitude_layers.dot_motion)
    }

    /// After a paint: wall seconds until the altitude layers need their
    /// next frame, for [`schedule_altitude_redraw`]. `on_globe` is this
    /// frame's `transition > 0` (the dots are drawn only there),
    /// `has_altitude` whether anything was prepared (the rings' speed is
    /// of this frame).
    ///
    /// Keep painting while the clock moves them, or a keyframe is owed,
    /// but only as often as they move a visible amount or something is
    /// due ([`Self::altitude_redraw_after_s`]; an inertial orbit ring turns
    /// with the planet far below a pixel a frame,
    /// [`Self::inertial_redraw_after_s`]). Each frame asked for redraws the
    /// host's WHOLE gpui window (layout, prepaint and paint of every
    /// element around the map), so asking at 120 Hz for dots that move a
    /// few px a second cost a third of a core while nothing visibly
    /// changed. The next frame at once when it is due within a frame,
    /// otherwise by one delayed wake (the fade's pattern; a sooner need
    /// replaces a later one, and a fired one is forgotten).
    pub(super) fn altitude_redraw_wait(&self, on_globe: bool, has_altitude: bool) -> Option<f64> {
        let dots_wait = if on_globe { self.altitude_redraw_after_s() } else { None };
        let rings_wait = if has_altitude { self.inertial_redraw_after_s() } else { None };
        match (dots_wait, rings_wait) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Wall seconds until the inertial orbit rings, as last drawn, have
    /// turned [`INERTIAL_REDRAW_PX`] with the planet; `None` when there are
    /// none or they do not move. Anything else that repaints (the camera,
    /// moving dots) redraws them sooner at no extra cost.
    pub(super) fn inertial_redraw_after_s(&self) -> Option<f64> {
        if !self.altitude_layers.has_inertial_lines() {
            return None;
        }
        inertial_redraw_after_s(self.scene_rate(), self.altitude_layers.inertial_px_per_rad)
    }

    /// The altitude object under `(x, y)` (logical pixels, as
    /// [`Self::feature_at`] takes them): its layer id and index.
    ///
    /// **Layer precedence beats distance.** Layers are searched topmost
    /// first (the last one first set), and the first layer with ANY dot
    /// within `render::altitude_pipeline::HIT_RADIUS_PX` (8 px) wins: a top
    /// layer's dot 7.9 px away is returned over a lower layer's dot exactly
    /// under the pointer. Within a layer the nearest dot centre wins, and an
    /// exact tie goes to the higher index (the dot drawn on top).
    ///
    /// Placed and culled exactly as the LAST DRAWN frame placed them -- the
    /// same Hermite time, the same soft-edge occlusion -- so an object behind
    /// the planet, masked off, or decayed is never hit. `None` on a flat
    /// frame, and between a `set_altitude_points` / `remove_altitude` and
    /// the next paint. Orbit lines are never hit.
    #[must_use]
    pub fn altitude_point_at(&self, x: f32, y: f32) -> Option<(String, usize)> {
        self.altitude_layers.hit(x, y)
    }

    /// Where object `i` of layer `id` was last drawn, in logical pixels;
    /// `None` when it was not drawn (hidden by the planet, masked off, not
    /// existing, or a flat frame).
    #[must_use]
    pub fn altitude_point_screen(&self, id: &str, i: usize) -> Option<(f32, f32)> {
        self.altitude_layers.screen(id, i)
    }

    /// Where object `i` of layer `id` was last drawn, as a geocentric
    /// sub-point and altitude (what follow mode and an info card want).
    /// Still answers for an object hidden behind the planet or behind the
    /// eye (past the near plane), unlike [`Self::altitude_point_screen`];
    /// `None` for one masked off or not existing at this frame's time, or
    /// before a globe frame.
    #[must_use]
    pub fn altitude_point_state(&self, id: &str, i: usize) -> Option<AltitudePointState> {
        self.altitude_layers.state(id, i)
    }

    /// Draws `lines` (orbit rings) as altitude line layer `id`, replacing
    /// that line layer if it exists or adding it over the others. Every
    /// line of the call is `width_px` logical pixels wide. `frame` says what
    /// the points' metres are in: `Inertial` (TEME) lines are turned by the
    /// scene time's `teme_to_sphere` each frame, so a ring stays fixed in
    /// the sky while the planet turns under it; `EarthFixed` lines are in
    /// the globe's sphere-frame axes. Near-clipped, and hidden by the planet
    /// per fragment (the same soft edge as the dots). Drawn on the globe
    /// only, before every point layer (dots sit on their rings), over the
    /// tiles and atmosphere, under labels and chrome. Shares `id`s with
    /// [`Self::set_altitude_points`]: [`Self::remove_altitude`] removes both.
    pub fn set_altitude_lines(
        &mut self,
        id: &str,
        frame: RefFrame,
        width_px: f32,
        lines: Vec<AltitudeLine>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.altitude_layers.set_lines(id, frame, width_px, &lines);
        cx.notify();
    }

    /// Draws line layer `id` (now, and whenever it is set again) in
    /// `look`: [`AltitudeLook::Glow`] gives the rings a bright core in a
    /// soft, screen-blended falloff. Flat unless asked.
    pub fn set_altitude_lines_look(&mut self, id: &str, look: AltitudeLook, cx: &mut gpui::Context<Self>) {
        self.altitude_layers.set_line_look(id, look);
        cx.notify();
    }

    /// Stops drawing altitude layer `id`: its points and its lines.
    pub fn remove_altitude(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        if self.altitude_layers.remove(id) {
            cx.notify();
        }
    }
}

#[cfg(test)]
impl MapView {
    /// The altitude half of `paint` a headless test window can run -- the
    /// keyframe advance, the motion `prepare` records (without the GPU
    /// pipeline), and the redraw request, on the paint's own code -- so a
    /// test sees what a real paint would ask the host for. Here, not in
    /// `map_view.rs`, whose `globe_frame(` call sites
    /// `render::globe`'s `paint_calls_globe_frame_exactly_once...` counts.
    pub(super) fn headless_altitude_redraw(&mut self, window: &mut gpui::Window, cx: &mut gpui::App) -> AltitudeRedraw {
        if self.altitude_layers.has_keyframed() {
            let scene = self.frame_scene_unix.unwrap_or_else(|| self.read_scene_clock());
            self.altitude_layers.advance(scene, self.scene_rate());
        }
        let camera = *self.camera_control.camera();
        let globe_frame = crate::render::globe::globe_frame(&camera).with_globe_enabled(self.globe_enabled);
        let has_altitude = !self.altitude_layers.is_empty()
            && match crate::render::altitude_pipeline::AltitudeFrame::new(&camera, &globe_frame, glam::DMat3::IDENTITY, 0.0, 60.0, 1.0) {
                Some(mut frame) => {
                    let scale = window.scale_factor();
                    frame.px_scale = scale;
                    frame.viewport = (camera.viewport_width_px * scale, camera.viewport_height_px * scale);
                    self.altitude_layers.note_motion(&frame);
                    true
                }
                None => {
                    self.altitude_layers.not_drawn();
                    false
                }
            };
        let wait = self.altitude_redraw_wait(globe_frame.transition > 0.0, has_altitude);
        schedule_altitude_redraw(wait, &mut self.altitude_layers.redraw_wake_at, self.self_handle.as_ref(), window, cx)
    }
}

