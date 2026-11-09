//! Trackpad pinch-to-zoom, in every form a platform delivers it, turned into
//! zoom LEVELS that track the fingers 1:1.
//!
//! Shared rather than living in a shell for the same reason as
//! `touch_gesture`: `web/` cannot run tests, so the arithmetic that decides
//! how far a pinch zooms lives where `cargo test` reaches it. Plain floats
//! only -- no gpui, no `web_sys` -- so it compiles identically on the host,
//! on `wasm32-unknown-unknown` and on iOS.
//!
//! # The three forms
//!
//! * **macOS, native.** AppKit sends `magnifyWithEvent:`; gpui-pre (0.3.7)
//!   forwards it as `gpui::PinchEvent`, whose `delta` is
//!   `NSEvent.magnification`: the content should be scaled by `1 + delta`.
//!   So one event is `log2(1 + delta)` zoom levels.
//! * **Chrome / Edge / Firefox.** A trackpad pinch is a `wheel` event with
//!   `ctrlKey` set (the browsers synthesise the modifier) and a small pixel
//!   `deltaY`. Chromium builds that `deltaY` as `-100 * ln(scale)`, so one
//!   event is `-deltaY / (100 * ln 2)` levels. Its sign follows a scroll:
//!   negative is fingers apart, zoom IN.
//! * **Safari.** WebKit's non-standard `gesturestart` / `gesturechange` /
//!   `gestureend` with a CUMULATIVE `scale` since the gesture began, so a
//!   step is `log2(scale / previous_scale)`.
//!
//! In every case the result is applied at once (`CameraIntent::PinchZoomBy`),
//! not eased: the fingers are already a smooth continuous signal, and an
//! ease on top of one only adds lag. That is what MapLibre GL JS does for a
//! trackpad (`ScrollZoomHandler.renderFrame` jumps straight to the target
//! zoom unless the device is a `wheel`).

/// One trackpad pinch step never moves the camera further than this many
/// zoom levels. A real pinch is a few hundredths of a level per event; this
/// only catches a corrupt delta (or a mouse wheel notch held with Ctrl) so it
/// cannot throw the map across the world in one frame. MapLibre bounds its
/// trackpad step the same way (`maxScalePerFrame = 2`, one level).
pub const MAX_PINCH_LEVELS_PER_EVENT: f64 = 1.0;

/// Chromium's pixels of ctrl-wheel `deltaY` per unit of `ln(scale)`
/// (`-100 * ln(scale)` in its pinch-to-wheel conversion).
pub const CTRL_WHEEL_PX_PER_LN_SCALE: f64 = 100.0;

/// MapLibre's `wheelZoomDelta`: macOS browsers report one physical wheel
/// notch as an exact multiple of this many pixels. A ctrl-wheel delta that is
/// one is a mouse wheel with Ctrl held, not a pinch.
pub const WHEEL_NOTCH_PX: f64 = 4.000_244_140_625;

fn bounded(levels: f64) -> f64 {
    if levels.is_finite() {
        levels.clamp(-MAX_PINCH_LEVELS_PER_EVENT, MAX_PINCH_LEVELS_PER_EVENT)
    } else {
        0.0
    }
}

/// Zoom levels for one native magnify event (`gpui::PinchEvent::delta`,
/// i.e. `NSEvent.magnification`). Positive zooms in. A magnification at or
/// below -1 would scale by zero or less: no zoom.
#[must_use]
pub fn magnify_zoom_levels(magnification: f32) -> f64 {
    let m = f64::from(magnification);
    if !m.is_finite() || m <= -1.0 {
        return 0.0;
    }
    bounded((1.0 + m).log2())
}

/// Zoom levels for one browser ctrl-wheel pinch event, `None` when the event
/// is not a pinch (so the caller treats it as an ordinary wheel zoom): a
/// line-mode delta, or a pixel delta that is a whole number of wheel notches
/// (`WHEEL_NOTCH_PX`), is a mouse wheel with Ctrl held.
#[must_use]
pub fn ctrl_wheel_zoom_levels(delta_y: f64, line_mode: bool) -> Option<f64> {
    if line_mode || !delta_y.is_finite() {
        return None;
    }
    if delta_y != 0.0 && (delta_y % WHEEL_NOTCH_PX) == 0.0 {
        return None;
    }
    Some(bounded(-delta_y / (CTRL_WHEEL_PX_PER_LN_SCALE * std::f64::consts::LN_2)))
}

/// One browser's trackpad pinch, across its events. Safari sends BOTH a
/// `gesturechange` and (in some versions) a ctrl-wheel for the same pinch;
/// while a WebKit gesture is open the wheel copies are ignored so the map
/// does not zoom twice.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BrowserPinch {
    /// The cumulative `scale` of the open WebKit gesture at its last event.
    gesture_scale: Option<f64>,
}

impl BrowserPinch {
    /// `gesturestart`: the cumulative scale restarts at 1.
    pub fn gesture_start(&mut self) {
        self.gesture_scale = Some(1.0);
    }

    /// `gesturechange` with its cumulative `scale`: the zoom levels since the
    /// previous event. 0 for a degenerate scale.
    pub fn gesture_change(&mut self, scale: f64) -> f64 {
        let previous = self.gesture_scale.unwrap_or(1.0);
        if !(scale.is_finite() && scale > 0.0) {
            return 0.0;
        }
        self.gesture_scale = Some(scale);
        if !(previous.is_finite() && previous > 0.0) {
            return 0.0;
        }
        bounded((scale / previous).log2())
    }

    /// `gestureend`.
    pub fn gesture_end(&mut self) {
        self.gesture_scale = None;
    }

    /// Whether a WebKit gesture is open.
    #[must_use]
    pub fn in_gesture(&self) -> bool {
        self.gesture_scale.is_some()
    }

    /// A `wheel` event: `Some(levels)` when it is a pinch this shell should
    /// apply (ctrl held, pixel mode, no WebKit gesture open), `Some(0.0)` when
    /// it is a pinch already counted by the open gesture, `None` when it is
    /// not a pinch at all.
    #[must_use]
    pub fn wheel(&self, ctrl: bool, delta_y: f64, line_mode: bool) -> Option<f64> {
        if !ctrl {
            return None;
        }
        let levels = ctrl_wheel_zoom_levels(delta_y, line_mode)?;
        Some(if self.in_gesture() { 0.0 } else { levels })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fingers' total scale and the map's total zoom agree: a pinch
    /// that doubles the content (in any number of steps) is one level.
    #[test]
    fn a_native_magnify_sequence_zooms_by_log2_of_its_total_scale() {
        // 20 events of +3.5%: total scale 1.035^20 = 1.99.
        let steps = [0.035_f32; 20];
        let levels: f64 = steps.iter().map(|&m| magnify_zoom_levels(m)).sum();
        let scale: f64 = steps.iter().map(|&m| 1.0 + f64::from(m)).product();
        assert!((levels - scale.log2()).abs() < 1e-6, "{levels} vs {}", scale.log2());
        assert!(levels > 0.98 && levels < 1.0, "{levels}");
        assert!(magnify_zoom_levels(-0.1) < 0.0, "fingers together zooms out");
        assert_eq!(magnify_zoom_levels(-1.0), 0.0);
        assert_eq!(magnify_zoom_levels(f32::NAN), 0.0);
        assert_eq!(magnify_zoom_levels(50.0), MAX_PINCH_LEVELS_PER_EVENT);
    }

    /// Chromium's `deltaY = -100 ln(scale)`: summing the per-event levels
    /// recovers `log2` of the pinch's total scale.
    #[test]
    fn a_chrome_ctrl_wheel_pinch_tracks_the_fingers() {
        let scales = [1.02_f64, 1.05, 1.03, 1.08, 0.97];
        let mut levels = 0.0;
        for s in scales {
            levels += ctrl_wheel_zoom_levels(-100.0 * s.ln(), false).expect("a pinch");
        }
        let total: f64 = scales.iter().product();
        assert!((levels - total.log2()).abs() < 1e-9);
        assert!(ctrl_wheel_zoom_levels(-3.0, false).unwrap() > 0.0, "negative deltaY zooms in");
    }

    #[test]
    fn a_ctrl_mouse_wheel_is_not_a_pinch() {
        assert_eq!(ctrl_wheel_zoom_levels(3.0, true), None, "line mode");
        assert_eq!(ctrl_wheel_zoom_levels(WHEEL_NOTCH_PX, false), None, "one notch");
        assert_eq!(ctrl_wheel_zoom_levels(-2.0 * WHEEL_NOTCH_PX, false), None, "two notches");
        assert_eq!(BrowserPinch::default().wheel(false, -3.0, false), None, "no ctrl");
        assert_eq!(ctrl_wheel_zoom_levels(-10_000.0, false), Some(MAX_PINCH_LEVELS_PER_EVENT));
    }

    /// Safari's cumulative `scale`: steps sum to `log2(final scale)`, and the
    /// ctrl-wheel copies inside the gesture count for nothing.
    #[test]
    fn a_safari_gesture_counts_once() {
        let mut p = BrowserPinch::default();
        p.gesture_start();
        let mut levels = 0.0;
        for s in [1.1, 1.4, 1.9, 2.0] {
            levels += p.gesture_change(s);
            assert_eq!(p.wheel(true, -2.0, false), Some(0.0), "a wheel copy of the same pinch");
        }
        assert!((levels - 1.0).abs() < 1e-9, "{levels}");
        p.gesture_end();
        assert!(p.wheel(true, -2.0, false).unwrap() > 0.0, "after the gesture a ctrl-wheel is a pinch again");
        assert_eq!(p.gesture_change(0.0), 0.0);
    }
}
