//! The globe's space scene: objects above it (the Studio app's satellites,
//! drawn by the desktop's altitude layers), the scene clock they run on,
//! and the plain-by-default space effects (stars, terminator).
//!
//! The satellites' catalogue and propagation are the app core's
//! (`studio-mobile`, over `gpui_maps_ios::orbit`); it hands the engine a
//! keyframe source through the Rust-only half below. A tap on a dot comes
//! back through the delegate's `on_tap` as `(layer, index)`.

use std::time::Instant;

use gpui_maps_ios::render::altitude_layers::{AltitudePointState, AltitudePositions, SceneClock};
use gpui_maps_ios::render::altitude::{AltitudeLine, RefFrame};
use gpui_maps_ios::render::altitude_pipeline::{AltitudeLook, AltitudeStyle};

use crate::MapEngine;

/// The space effects as set: both off on every map until switched on.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct SpaceEffects {
    /// The star field behind the globe (drawn below zoom 6).
    pub stars: bool,
    /// Night shading at the scene clock's sun.
    pub terminator: bool,
}

/// What the last frame drew of the altitude layers, and its CPU cost.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct AltitudeFrameStats {
    /// Dots drawn (0 on a flat frame or before the first keyframes land).
    pub dots_drawn: u32,
    /// The frame's altitude advance + prepare, ms (the keyframe
    /// propagation itself runs on the layer's own thread).
    pub cpu_ms: f64,
}

#[uniffi::export]
impl MapEngine {
    pub fn altitude_frame_stats(&self) -> AltitudeFrameStats {
        let (dots, cpu_ms) = self.lock().value.host.altitude_stats();
        AltitudeFrameStats { dots_drawn: dots as u32, cpu_ms }
    }

    /// Turns the globe's star field and day/night terminator on or off.
    /// Both are off by default (plain maps).
    pub fn set_space_effects(&self, effects: SpaceEffects) {
        self.lock().value.host.set_space_effects(effects.stars, effects.terminator);
    }

    pub fn space_effects(&self) -> SpaceEffects {
        let (stars, terminator) = self.lock().value.host.space_effects();
        SpaceEffects { stars, terminator }
    }

    /// The camera's zoom range. A space scene may go below 0 (to -5) to see
    /// the outer shells around the whole Earth.
    pub fn set_zoom_range(&self, min_zoom: f64, max_zoom: f64) {
        self.lock().value.host.set_zoom_range(min_zoom, max_zoom);
    }

    /// A data credit joined to the basemap's in the credit plate and
    /// [`MapEngine::attributions`]; `None` removes it.
    pub fn set_data_credit(&self, credit: Option<String>) {
        self.lock().value.host.set_data_credit(credit);
    }

    /// The scene time the space scene shows, Unix seconds.
    pub fn scene_time(&self) -> f64 {
        self.lock().value.host.scene_time()
    }

    /// The scene clock's rate: scene seconds per wall second (1 real time,
    /// 0 paused, negative backwards). The scene time stays continuous.
    pub fn set_scene_rate(&self, rate: f64) {
        let mut guard = self.lock();
        let host = &mut guard.value.host;
        let clock = host.scene_clock().unwrap_or_else(SceneClock::realtime);
        host.set_scene_clock(Some(clock.with_rate(if rate.is_finite() { rate } else { 1.0 }, Instant::now())));
    }

    pub fn scene_rate(&self) -> f64 {
        self.lock().value.host.scene_clock().map_or(1.0, |c| c.rate)
    }

    /// Back to now, at real time.
    pub fn reset_scene_clock(&self) {
        self.lock().value.host.set_scene_clock(None);
    }
}

/// The Rust-only half, for an app core that builds the objects itself.
impl MapEngine {
    /// Draws `positions` as altitude layer `id` over the globe (native's
    /// `MapView::set_altitude_points`).
    pub fn set_altitude_points(&self, id: &str, positions: AltitudePositions, style: AltitudeStyle) {
        self.lock().value.host.set_altitude_points(id, positions, style);
    }

    pub fn set_altitude_style(&self, id: &str, style: AltitudeStyle) {
        self.lock().value.host.set_altitude_style(id, style);
    }

    pub fn set_altitude_visibility(&self, id: &str, mask: &[bool]) {
        self.lock().value.host.set_altitude_visibility(id, mask);
    }

    /// Up to three emphasised objects of layer `id`: hovered, selected, halo.
    pub fn set_altitude_emphasis(&self, id: &str, hovered: Option<usize>, selected: Option<usize>, halo: Option<usize>) {
        self.lock().value.host.set_altitude_emphasis(id, hovered, selected, halo);
    }

    /// Orbit rings as altitude line layer `id` (an empty list clears it).
    pub fn set_altitude_lines(&self, id: &str, frame: RefFrame, width_px: f32, lines: &[AltitudeLine]) {
        self.lock().value.host.set_altitude_lines(id, frame, width_px, lines);
    }

    pub fn set_altitude_lines_look(&self, id: &str, look: AltitudeLook) {
        self.lock().value.host.set_altitude_lines_look(id, look);
    }

    pub fn remove_altitude(&self, id: &str) {
        self.lock().value.host.remove_altitude(id);
    }

    pub fn set_scene_clock(&self, clock: Option<SceneClock>) {
        self.lock().value.host.set_scene_clock(clock);
    }

    /// Where object `i` of layer `id` was last drawn: sub-point, altitude.
    pub fn altitude_point_state(&self, id: &str, i: usize) -> Option<AltitudePointState> {
        self.lock().value.host.altitude_point_state(id, i)
    }

    /// Where object `i` of layer `id` was last drawn on screen, in points.
    pub fn altitude_point_screen(&self, id: &str, i: usize) -> Option<(f32, f32)> {
        self.lock().value.host.altitude_point_screen(id, i)
    }

    /// The altitude object under a point (in points), within a finger's reach.
    pub fn altitude_point_at(&self, x: f64, y: f64) -> Option<(String, usize)> {
        self.lock().value.host.altitude_point_at(x, y)
    }

    /// Dots the last frame drew, and that frame's altitude CPU time (ms).
    pub fn altitude_stats(&self) -> (usize, f64) {
        self.lock().value.host.altitude_stats()
    }
}

