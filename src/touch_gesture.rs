//! Pure gesture math for multitouch pointer input.
//!
//! Deliberately shared rather than living in `web/src/app.rs`: the browser
//! shell has no working `#[cfg(test)]` gate (`web/` is its own workspace and
//! `scripts/check-wasm.sh` only runs `cargo check --lib`), so anything that
//! needs a unit test to be trustworthy has to live somewhere `cargo test`
//! actually runs it. This module is that somewhere -- it names nothing but
//! plain floats and tuples, no `web_sys`, no `Camera`, so it compiles and
//! tests identically on the host and on `wasm32-unknown-unknown`.
//!
//! Two-pointer ROTATE and TILT are here too, as of the 3D camera work
//! (`docs/superpowers/specs/2026-09-24-3d-camera-parity-design.md`): the
//! reason this file once gave for leaving rotation out -- the browser's
//! `pan_by_pixels` ignoring bearing -- is gone, because every shell's pan now
//! comes from `camera_control`. [`TwoFingerGesture`] decides, once per
//! gesture, whether two fingers are pinching-and-rotating or tilting; the
//! decision follows MapLibre GL JS's `TwoFingersTouchPitchHandler`
//! (`src/ui/handler/two_fingers_touch.ts`, BSD-3-Clause).

/// Euclidean distance between two pointer positions, in CSS pixels.
pub fn pointer_distance(a: (f32, f32), b: (f32, f32)) -> f64 {
    let dx = (a.0 - b.0) as f64;
    let dy = (a.1 - b.1) as f64;
    (dx * dx + dy * dy).sqrt()
}

/// The midpoint between two pointer positions, in CSS pixels -- the anchor a
/// pinch zooms about, matching every touch map's own convention (the
/// gesture's centre, not either finger).
pub fn pointer_midpoint(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
}

/// Zoom delta, in zoom LEVELS (not the `amount` a caller like
/// `zoom_towards_point` expects, which is levels divided by however many
/// levels one wheel line is worth -- that conversion is the caller's, since
/// this module has no opinion on wheel sensitivity).
///
/// Defined as `log2(new_distance / old_distance)`: two fingers doubling
/// their separation is one full zoom level in, halving is one level out.
/// This is the same curve MapLibre GL JS's own
/// `TouchZoomRotateHandler`/`_computeZoom` and Mapbox's touch handler use
/// (both are `log2` of the pinch ratio), not an independently invented one.
///
/// Guards the degenerate case -- a pinch that starts, or briefly passes
/// through, near-zero finger separation (two fingers landing on almost the
/// same point) -- by returning `0.0` rather than `log2` of something near
/// zero, which would blow up to a huge or infinite/NaN "zoom".
pub fn pinch_zoom_delta_levels(old_distance: f64, new_distance: f64) -> f64 {
    const MIN_DISTANCE_PX: f64 = 1.0;
    if !old_distance.is_finite()
        || !new_distance.is_finite()
        || old_distance < MIN_DISTANCE_PX
        || new_distance < MIN_DISTANCE_PX
    {
        return 0.0;
    }
    (new_distance / old_distance).log2()
}

/// Net twist, in radians, a two-finger gesture must make before it rotates
/// the map (10 degrees). Moved from `ios/src/app.rs`'s
/// `ROTATION_DEADZONE_RADIANS`, whose doc comment carries the reasoning (13 pt
/// of tangential travel per finger at a 150 pt span, just past the tap slop).
pub const TWIST_DEADZONE_RADIANS: f64 = std::f64::consts::PI / 18.0;
/// Below this separation the pair's angle is contact noise and rotation is
/// suppressed. Moved from `ios/src/app.rs`'s `MIN_ROTATION_SPAN_PT`.
pub const MIN_TWIST_SPAN_PX: f64 = 40.0;
/// Below this separation the pinch ratio is numerically worthless. Moved from
/// `ios/src/app.rs`'s `MIN_PINCH_SPAN_PT`.
pub const MIN_PINCH_SPAN_PX: f64 = 20.0;
/// How far a finger must move before it counts toward the pinch-or-tilt
/// decision: MapLibre's `gestureBeginsVertically` `threshold = 2`.
pub const TILT_MOVE_THRESHOLD_PX: f32 = 2.0;
/// How long one moving finger waits for the other before the gesture is
/// decided NOT to be a tilt: MapLibre's `ALLOWED_SINGLE_TOUCH_TIME = 100`.
pub const TILT_DECIDE_WINDOW_MS: f64 = 100.0;

/// MapLibre's `isVertical` (`two_fingers_touch.ts:277`).
fn is_vertical(v: (f32, f32)) -> bool {
    v.1.abs() > v.0.abs()
}

fn sub(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    (a.0 - b.0, a.1 - b.1)
}

fn length(v: (f32, f32)) -> f32 {
    (v.0 * v.0 + v.1 * v.1).sqrt()
}

/// The change in angle of the line from `a` to `b`, the short way round, in
/// radians. Screen y points DOWN, so a positive value is a CLOCKWISE turn.
/// `atan2` wraps at +/-PI, so the raw difference of two angles can be off by
/// a whole turn; `rem_euclid` brings it back.
pub fn twist_radians(old_a: (f32, f32), old_b: (f32, f32), new_a: (f32, f32), new_b: (f32, f32)) -> f64 {
    let angle = |a: (f32, f32), b: (f32, f32)| f64::from(b.1 - a.1).atan2(f64::from(b.0 - a.0));
    let d = (angle(new_a, new_b) - angle(old_a, old_b)).rem_euclid(std::f64::consts::TAU);
    if d > std::f64::consts::PI {
        d - std::f64::consts::TAU
    } else {
        d
    }
}

/// `Some(mean dy)` when both pointers moved at least
/// [`TILT_MOVE_THRESHOLD_PX`], both mostly vertically, both the same way, and
/// the pair started roughly level. `None` otherwise. MapLibre's
/// `_start` + `gestureBeginsVertically`, as one pure function.
pub fn two_finger_tilt_px(old_a: (f32, f32), old_b: (f32, f32), new_a: (f32, f32), new_b: (f32, f32)) -> Option<f32> {
    let (va, vb) = (sub(new_a, old_a), sub(new_b, old_b));
    if length(va) < TILT_MOVE_THRESHOLD_PX || length(vb) < TILT_MOVE_THRESHOLD_PX {
        return None;
    }
    if is_vertical(sub(old_b, old_a)) {
        return None;
    }
    let same_direction = (va.1 > 0.0) == (vb.1 > 0.0);
    (is_vertical(va) && is_vertical(vb) && same_direction).then_some((va.1 + vb.1) / 2.0)
}

/// What a two-finger gesture is, once it has said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwoFingerMode {
    /// Neither finger has moved far enough, or one has and the other has not
    /// yet (inside [`TILT_DECIDE_WINDOW_MS`]). Nothing moves the map.
    Undecided,
    /// Pan by the midpoint, zoom by the span ratio, and rotate by the twist
    /// once it clears [`TWIST_DEADZONE_RADIANS`] -- all together, as MapLibre
    /// runs its zoom and rotate handlers together.
    PinchRotate,
    /// Both fingers dragging vertically the same way: pitch only.
    Tilt,
}

/// One update's worth of camera input, in the units `camera_control` takes.
/// A shell turns each non-zero field into one `CameraIntent`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TwoFingerStep {
    /// Midpoint motion (a `PanBy`), and the midpoint BEFORE it (its anchor).
    pub pan_px: (f32, f32),
    pub pan_anchor_px: (f32, f32),
    /// Zoom delta in LEVELS, `log2` of the span ratio (a `ZoomAbout`).
    pub zoom_levels: f64,
    /// Bearing change (a `RotateBy`): the twist NEGATED, because screen angle
    /// grows clockwise while bearing grows the other way, and turning the
    /// fingers clockwise must turn the map clockwise with them.
    pub rotate_radians: f32,
    /// Mean vertical motion of the pair, y down (a `TiltBy` through
    /// `camera_control::tilt_radians_for_drag_px`).
    pub tilt_px: f32,
    /// Where zoom and rotation are anchored: the midpoint AFTER this update.
    pub anchor_px: (f32, f32),
}

/// One two-finger gesture, from both fingers down to either lifting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwoFingerGesture {
    mode: TwoFingerMode,
    /// Where the fingers were when the gesture began. While undecided, every
    /// update is measured from HERE rather than from the previous update:
    /// iOS and the browser both deliver one finger per move event, so a
    /// per-event "did both fingers move?" is never true. MapLibre's
    /// `_lastPoints` behaves the same way until the gesture is valid.
    start: [(f32, f32); 2],
    first_single_move_ms: Option<f64>,
    net_twist: f64,
    rotating: bool,
}

impl TwoFingerGesture {
    #[must_use]
    pub fn begin(a: (f32, f32), b: (f32, f32)) -> Self {
        Self { mode: TwoFingerMode::Undecided, start: [a, b], first_single_move_ms: None, net_twist: 0.0, rotating: false }
    }

    #[must_use]
    pub fn mode(&self) -> TwoFingerMode {
        self.mode
    }

    /// Whether the twist has cleared the deadzone. Latches for the rest of
    /// the gesture: a hand that turns back through its own start angle must
    /// keep tracking, not disengage and stutter.
    #[must_use]
    pub fn is_rotating(&self) -> bool {
        self.rotating
    }

    /// The fingers moved from `old` to `new` at `at_ms` (the shell's clock).
    pub fn update(&mut self, old: [(f32, f32); 2], new: [(f32, f32); 2], at_ms: f64) -> TwoFingerStep {
        if self.mode == TwoFingerMode::Undecided {
            self.mode = self.classify(new, at_ms);
            if self.mode == TwoFingerMode::Undecided {
                return TwoFingerStep {
                    anchor_px: pointer_midpoint(new[0], new[1]),
                    pan_anchor_px: pointer_midpoint(old[0], old[1]),
                    ..TwoFingerStep::default()
                };
            }
            // Just committed: replay everything since the gesture began, so
            // the few pixels spent deciding are not lost.
            let start = self.start;
            return self.step(start, new);
        }
        self.step(old, new)
    }

    fn classify(&mut self, new: [(f32, f32); 2], at_ms: f64) -> TwoFingerMode {
        let moved_a = length(sub(new[0], self.start[0])) >= TILT_MOVE_THRESHOLD_PX;
        let moved_b = length(sub(new[1], self.start[1])) >= TILT_MOVE_THRESHOLD_PX;
        if !moved_a && !moved_b {
            return TwoFingerMode::Undecided;
        }
        if !(moved_a && moved_b) {
            let first = *self.first_single_move_ms.get_or_insert(at_ms);
            return if at_ms - first < TILT_DECIDE_WINDOW_MS { TwoFingerMode::Undecided } else { TwoFingerMode::PinchRotate };
        }
        if two_finger_tilt_px(self.start[0], self.start[1], new[0], new[1]).is_some() {
            TwoFingerMode::Tilt
        } else {
            TwoFingerMode::PinchRotate
        }
    }

    fn step(&mut self, old: [(f32, f32); 2], new: [(f32, f32); 2]) -> TwoFingerStep {
        let (from_mid, to_mid) = (pointer_midpoint(old[0], old[1]), pointer_midpoint(new[0], new[1]));
        let mut out = TwoFingerStep { anchor_px: to_mid, pan_anchor_px: from_mid, ..TwoFingerStep::default() };
        match self.mode {
            TwoFingerMode::Undecided => {}
            TwoFingerMode::Tilt => {
                out.tilt_px = ((new[0].1 - old[0].1) + (new[1].1 - old[1].1)) / 2.0;
            }
            TwoFingerMode::PinchRotate => {
                out.pan_px = (to_mid.0 - from_mid.0, to_mid.1 - from_mid.1);
                let (span_before, span) = (pointer_distance(old[0], old[1]), pointer_distance(new[0], new[1]));
                if span_before >= MIN_PINCH_SPAN_PX && span >= MIN_PINCH_SPAN_PX {
                    out.zoom_levels = pinch_zoom_delta_levels(span_before, span);
                }
                let twist = twist_radians(old[0], old[1], new[0], new[1]);
                self.net_twist += twist;
                if !self.rotating && span >= MIN_TWIST_SPAN_PX && self.net_twist.abs() >= TWIST_DEADZONE_RADIANS {
                    // The deadzone is CONSUMED, not repaid: only this
                    // update's twist lands (ios/ROTATION.md).
                    self.rotating = true;
                }
                if self.rotating && span >= MIN_TWIST_SPAN_PX {
                    out.rotate_radians = -twist as f32;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_distance_is_the_3_4_5_triangle() {
        assert!((pointer_distance((0.0, 0.0), (3.0, 4.0)) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn pointer_distance_same_point_is_zero() {
        assert_eq!(pointer_distance((12.0, 8.0), (12.0, 8.0)), 0.0);
    }

    #[test]
    fn pointer_midpoint_averages_each_axis() {
        assert_eq!(pointer_midpoint((0.0, 0.0), (10.0, 20.0)), (5.0, 10.0));
        assert_eq!(pointer_midpoint((-10.0, 5.0), (10.0, -5.0)), (0.0, 0.0));
    }

    #[test]
    fn pinch_doubling_distance_is_one_zoom_level_in() {
        let delta = pinch_zoom_delta_levels(100.0, 200.0);
        assert!((delta - 1.0).abs() < 1e-9, "got {delta}");
    }

    #[test]
    fn pinch_halving_distance_is_one_zoom_level_out() {
        let delta = pinch_zoom_delta_levels(200.0, 100.0);
        assert!((delta - (-1.0)).abs() < 1e-9, "got {delta}");
    }

    #[test]
    fn pinch_unchanged_distance_is_zero() {
        assert_eq!(pinch_zoom_delta_levels(150.0, 150.0), 0.0);
    }

    #[test]
    fn pinch_guards_near_zero_old_distance() {
        // Two fingers that landed almost on top of each other: no sane
        // finite zoom delta exists, and this must not return +/- infinity
        // or NaN.
        assert_eq!(pinch_zoom_delta_levels(0.0, 200.0), 0.0);
        assert_eq!(pinch_zoom_delta_levels(0.4, 200.0), 0.0);
    }

    #[test]
    fn pinch_guards_near_zero_new_distance() {
        assert_eq!(pinch_zoom_delta_levels(200.0, 0.0), 0.0);
    }

    #[test]
    fn pinch_guards_non_finite_input() {
        assert_eq!(pinch_zoom_delta_levels(f64::NAN, 200.0), 0.0);
        assert_eq!(pinch_zoom_delta_levels(100.0, f64::INFINITY), 0.0);
    }

    /// Two fingers diametrically opposite on a circle of radius `r` about
    /// `c`, at screen angle `phi` (y down, so increasing `phi` is clockwise).
    fn pair_at(c: (f32, f32), r: f32, phi: f32) -> [(f32, f32); 2] {
        [(c.0 + r * phi.cos(), c.1 + r * phi.sin()), (c.0 - r * phi.cos(), c.1 - r * phi.sin())]
    }

    /// Feeds consecutive pairs through `update`, 8 ms apart.
    fn drive(g: &mut TwoFingerGesture, frames: &[[(f32, f32); 2]]) -> Vec<TwoFingerStep> {
        frames.windows(2).enumerate().map(|(i, w)| g.update(w[0], w[1], i as f64 * 8.0)).collect()
    }

    #[test]
    fn twist_takes_the_short_way_round_and_is_clockwise_positive() {
        let c = (200.0, 400.0);
        let [a0, b0] = pair_at(c, 90.0, 179.0_f32.to_radians());
        let [a1, b1] = pair_at(c, 90.0, (-179.0_f32).to_radians());
        assert!((twist_radians(a0, b0, a1, b1).to_degrees() - 2.0).abs() < 1e-3);
        let [a2, b2] = pair_at(c, 90.0, 10.0_f32.to_radians());
        let [a3, b3] = pair_at(c, 90.0, 0.0);
        assert!((twist_radians(a2, b2, a3, b3).to_degrees() + 10.0).abs() < 1e-3, "anticlockwise is negative");
    }

    #[test]
    fn a_tilt_needs_both_fingers_moving_vertically_the_same_way_from_a_level_pair() {
        let (a, b) = ((150.0, 400.0), (250.0, 400.0));
        assert_eq!(two_finger_tilt_px(a, b, (150.0, 410.0), (250.0, 410.0)), Some(10.0));
        assert_eq!(two_finger_tilt_px(a, b, (150.0, 410.0), (250.0, 390.0)), None, "opposite directions is a twist");
        assert_eq!(two_finger_tilt_px(a, b, (160.0, 401.0), (260.0, 401.0)), None, "sideways is a pan");
        assert_eq!(two_finger_tilt_px(a, b, (150.0, 410.0), (250.0, 400.5)), None, "one finger still");
        let (top, bottom) = ((200.0, 300.0), (200.0, 500.0));
        assert_eq!(two_finger_tilt_px(top, bottom, (200.0, 310.0), (200.0, 510.0)), None, "stacked fingers never tilt");
    }

    #[test]
    fn one_finger_per_event_still_classifies_a_two_finger_tilt() {
        // iOS delivers ONE finger per `Moved` event. Measured per event,
        // "both fingers moved" is never true; measured from the gesture's
        // start (MapLibre's `_lastPoints` while undecided) it is.
        let mut pair = [(150.0f32, 400.0f32), (250.0, 400.0)];
        let mut frames = vec![pair];
        for i in 0..10 {
            pair[i % 2].1 += 3.0;
            frames.push(pair);
        }
        let mut g = TwoFingerGesture::begin(frames[0][0], frames[0][1]);
        let steps = drive(&mut g, &frames);
        assert_eq!(g.mode(), TwoFingerMode::Tilt);
        let tilt: f32 = steps.iter().map(|s| s.tilt_px).sum();
        assert!((tilt - 15.0).abs() < 1e-4, "each finger went 15 px down, the pair tilts by their mean: {tilt}");
        assert!(steps.iter().all(|s| s.rotate_radians == 0.0 && s.zoom_levels == 0.0 && s.pan_px == (0.0, 0.0)));
    }

    #[test]
    fn a_twist_that_turns_vertical_later_never_becomes_a_tilt() {
        let c = (195.0, 422.0);
        let mut frames: Vec<[(f32, f32); 2]> = (0..=10).map(|i| pair_at(c, 90.0, (2.0 * i as f32).to_radians())).collect();
        let last = *frames.last().unwrap();
        for i in 1..=10 {
            frames.push([(last[0].0, last[0].1 + 4.0 * i as f32), (last[1].0, last[1].1 + 4.0 * i as f32)]);
        }
        let mut g = TwoFingerGesture::begin(frames[0][0], frames[0][1]);
        let steps = drive(&mut g, &frames);
        assert_eq!(g.mode(), TwoFingerMode::PinchRotate, "committed on its first movement");
        assert!(g.is_rotating(), "20 degrees clears the 10-degree deadzone");
        assert!(steps.iter().all(|s| s.tilt_px == 0.0), "a gesture committed to rotating must never tilt");
        let pan_y: f32 = steps.iter().map(|s| s.pan_px.1).sum();
        assert!((pan_y - 40.0).abs() < 1e-3, "the later vertical drag pans instead: {pan_y}");
    }

    #[test]
    fn a_tilt_with_a_slight_twist_does_not_rotate() {
        let mut frames = vec![[(150.0f32, 400.0f32), (250.0, 400.0)]];
        for i in 1..=10 {
            frames.push([(150.0, 400.0 + 4.0 * i as f32), (250.0, 400.0 + 3.6 * i as f32)]);
        }
        let mut g = TwoFingerGesture::begin(frames[0][0], frames[0][1]);
        let steps = drive(&mut g, &frames);
        assert_eq!(g.mode(), TwoFingerMode::Tilt);
        assert!(steps.iter().all(|s| s.rotate_radians == 0.0));
        let tilt: f32 = steps.iter().map(|s| s.tilt_px).sum();
        assert!((tilt - 38.0).abs() < 1e-3, "{tilt}");
    }

    #[test]
    fn an_incidental_twist_below_the_deadzone_does_not_rotate_a_pinch() {
        // ios/src/app.rs's `a_straight_pinch_with_incidental_rotation_does_not_rotate`,
        // at the gesture layer: 100 -> 240 pt apart, drifting 5 degrees.
        let c = (195.0, 422.0);
        let frames: Vec<[(f32, f32); 2]> = (0..=20)
            .map(|i| {
                let t = i as f32 / 20.0;
                pair_at(c, 50.0 + 70.0 * t, 5.0_f32.to_radians() * t)
            })
            .collect();
        let mut g = TwoFingerGesture::begin(frames[0][0], frames[0][1]);
        let steps = drive(&mut g, &frames);
        assert_eq!(g.mode(), TwoFingerMode::PinchRotate);
        assert!(!g.is_rotating());
        assert!(steps.iter().all(|s| s.rotate_radians == 0.0));
        let zoom: f64 = steps.iter().map(|s| s.zoom_levels).sum();
        assert!(zoom > 1.0, "the pinch itself still zooms: {zoom}");
    }

    #[test]
    fn fingers_too_close_together_never_rotate() {
        let c = (195.0, 422.0);
        let frames: Vec<[(f32, f32); 2]> = (0..=20).map(|i| pair_at(c, 15.0, (4.5 * i as f32).to_radians())).collect();
        let mut g = TwoFingerGesture::begin(frames[0][0], frames[0][1]);
        let steps = drive(&mut g, &frames);
        assert!(!g.is_rotating(), "30 pt apart is under MIN_TWIST_SPAN_PX");
        assert!(steps.iter().all(|s| s.rotate_radians == 0.0));
    }

    #[test]
    fn a_deliberate_twist_rotates_about_the_midpoint_with_the_bearing_sign() {
        let c = (195.0, 422.0);
        let frames: Vec<[(f32, f32); 2]> = (0..=20).map(|i| pair_at(c, 90.0, (2.0 * i as f32).to_radians())).collect();
        let mut g = TwoFingerGesture::begin(frames[0][0], frames[0][1]);
        let steps = drive(&mut g, &frames);
        let turned: f32 = steps.iter().map(|s| s.rotate_radians).sum();
        // Clockwise on screen is a NEGATIVE bearing change; the deadzone is
        // consumed, not repaid (ios/ROTATION.md), so 30..=32 of the 40.
        assert!(turned < 0.0 && (30.0..=32.1).contains(&turned.abs().to_degrees()), "{}", turned.to_degrees());
        assert!(steps.iter().all(|s| (s.anchor_px.0 - 195.0).abs() < 1e-3 && (s.anchor_px.1 - 422.0).abs() < 1e-3));
    }

    #[test]
    fn one_finger_moving_alone_decides_pinch_after_the_window() {
        let mut g = TwoFingerGesture::begin((150.0, 400.0), (250.0, 400.0));
        let old = [(150.0, 400.0), (250.0, 400.0)];
        let _ = g.update(old, [(150.0, 405.0), (250.0, 400.0)], 0.0);
        assert_eq!(g.mode(), TwoFingerMode::Undecided, "still waiting for the second finger");
        let _ = g.update([(150.0, 405.0), (250.0, 400.0)], [(150.0, 410.0), (250.0, 400.0)], TILT_DECIDE_WINDOW_MS);
        assert_eq!(g.mode(), TwoFingerMode::PinchRotate);
    }
}
