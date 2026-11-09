//! Altitude maths on the CPU: points above the globe (spec
//! 2026-09-29-satellites-design.md §2, §3.2).
//!
//! Everything here works in the globe's **unit-sphere frame** -- the space
//! [`GlobeFrame::globe_view`] takes to view space, one unit per Earth radius
//! ([`GLOBE_RADIUS_M`]), `+y` north, `+z` through (lon 0, lat 0), `+x`
//! through lon 90 E. Earth-fixed (ECEF) `(x, y, z)` is `(y, z, x)` here.
//!
//! - the frame: [`geo_to_sphere`], [`ecef_to_sphere`], [`teme_to_sphere`]
//!   (with [`gmst_rad`], restated because this module compiles without the
//!   `orbits` feature, on the web and iOS shells, and pinned to sgp4's by a
//!   test in `orbit::propagate`);
//! - the Earth's occlusion of a point: [`closest_approach`] and its soft
//!   [`visibility`] edge, checked against an independent predicate,
//!   [`visible_by_horizon_angle`];
//! - where the point lands on screen: [`project_altitude`], culled by
//!   [`behind_near`] rather than mirrored through `w < 0`;
//! - between keyframes: [`hermite`] over [`keyframe_delta_s`];
//! - the scene clock's arithmetic, [`scene_time_at`], and which keyframes a
//!   frame needs: [`KeyframeScheduler`] (at most three held, one in flight),
//!   filled by [`KeyframeBuilder`] and trimmed by [`hidden_bounds`] where an
//!   object stops (or starts) existing inside an interval.
//!
//! Nothing here reads a clock or spawns a thread: the browser build compiles
//! it through `src/render/mod.rs`.

use glam::{DMat3, DVec3, DVec4};

use super::camera::Camera;
use super::globe::{GlobeFrame, GLOBE_RADIUS_M};

/// J2000.0, 2000-01-01T12:00:00Z, as Unix seconds (the same instant as
/// `orbit::J2000_UNIX_S`, which exists only under `orbits`).
pub const J2000_UNIX_S: f64 = 946_728_000.0;

/// A geocentric position: longitude and latitude in degrees, and altitude in
/// metres above the globe's sphere (spec R2 -- the sphere, not the WGS84
/// ellipsoid, because the sphere is what is drawn).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GeoPoint {
    /// Longitude, degrees east.
    pub lon: f64,
    /// Geocentric latitude, degrees north.
    pub lat: f64,
    /// Height above the globe's sphere (`GLOBE_RADIUS_M`), metres.
    pub alt_m: f64,
}

/// One object's state at a keyframe: position (m) and velocity (m/s) in the
/// source's [`RefFrame`]. `valid == false` hides the object for the whole
/// keyframe interval.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyState {
    /// Position, metres, in the source's [`RefFrame`].
    pub position_m: DVec3,
    /// Velocity, metres per second, in the same frame.
    pub velocity_m_s: DVec3,
    /// Whether the source could place the object at this instant; `false`
    /// hides it (never drawn at the origin).
    pub valid: bool,
}

impl KeyState {
    /// The state of an object with nothing to draw.
    pub const HIDDEN: KeyState = KeyState { position_m: DVec3::ZERO, velocity_m_s: DVec3::ZERO, valid: false };
}

/// Something that can fill keyframe states for a contiguous range of its
/// objects at an instant.
pub trait KeyframeSource: Send + Sync {
    /// How many objects the source has.
    fn len(&self) -> usize;
    /// Whether the source has no objects.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Writes the state of objects `range` at `unix_s` into `out`
    /// (`out.len() == range.len()`); an object without one gets
    /// [`KeyState::HIDDEN`].
    fn states_at(&self, unix_s: f64, range: std::ops::Range<usize>, out: &mut [KeyState]);
}

/// The frame a [`KeyframeSource`]'s states are in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RefFrame {
    /// Earth-fixed (ECEF): turns with the planet.
    #[default]
    EarthFixed,
    /// TEME: rotated into the sphere frame by [`teme_to_sphere`] per frame.
    Inertial,
}

/// The shortest keyframe interval, seconds.
pub const KEYFRAME_DELTA_MIN_S: f64 = 60.0;
/// The longest keyframe interval, seconds. MG-S2
/// (docs/measurements/2026-09-29-satellites.md): the worst Hermite error at
/// this interval is 0.354 km over the stations fixture and the Molniya/GTO
/// set. 300 s also passed the 1 km criterion, at 0.847 km, but with a 15%
/// margin set by synthetic perigees of 250-600 km; real GTO bodies decay to
/// 150 km and below, where perigee is faster and tighter, so 240 s was
/// chosen for a 2.8x margin at +25% keyframe cost.
pub const KEYFRAME_DELTA_MAX_S: f64 = 240.0;

/// The keyframe interval for a scene clock running at `rate` (scene seconds
/// per wall second): a quarter of a wall second's worth, clamped to
/// [`KEYFRAME_DELTA_MIN_S`]..=[`KEYFRAME_DELTA_MAX_S`], then SNAPPED DOWN to
/// a power-of-two multiple of the minimum -- 60, 120 or 240 s. A continuous
/// interval re-gridded (and so dropped every keyframe) on every change of a
/// dragged rate slider; snapped, it changes only when the rate crosses 480
/// or 960. Snapping down only ever shortens the interval, so the Hermite
/// error stays within the capped one's. A NaN rate gets the minimum (never
/// a NaN interval, which would stall a schedule); an infinite one the
/// maximum.
#[must_use]
pub fn keyframe_delta_s(rate: f64) -> f64 {
    if rate.is_nan() {
        return KEYFRAME_DELTA_MIN_S;
    }
    let raw = (rate.abs() * 0.25).clamp(KEYFRAME_DELTA_MIN_S, KEYFRAME_DELTA_MAX_S);
    let mut delta = KEYFRAME_DELTA_MIN_S;
    while delta * 2.0 <= raw {
        delta *= 2.0;
    }
    delta
}

/// A geocentric point to the unit-sphere frame.
#[must_use]
pub fn geo_to_sphere(p: GeoPoint) -> DVec3 {
    let (sl, cl) = p.lat.to_radians().sin_cos();
    let (so, co) = p.lon.to_radians().sin_cos();
    DVec3::new(so * cl, sl, co * cl) * (1.0 + p.alt_m / GLOBE_RADIUS_M)
}

/// Earth-fixed metres to the unit-sphere frame: `(y, z, x) / R`.
#[must_use]
pub fn ecef_to_sphere(ecef_m: DVec3) -> DVec3 {
    DVec3::new(ecef_m.y, ecef_m.z, ecef_m.x) / GLOBE_RADIUS_M
}

/// Greenwich mean sidereal time in radians, `[0, 2π)`, by the IAU-82
/// expression `sgp4::iau_epoch_to_sidereal_time` evaluates (sgp4 2.4.0
/// `model.rs`), in the same operation order so it agrees to the bit.
/// Restated because this module compiles without the `orbits` feature;
/// `orbit::propagate`'s tests pin the two together.
#[must_use]
pub fn gmst_rad(unix_s: f64) -> f64 {
    let c = (unix_s - J2000_UNIX_S) / (86_400.0 * 365.25) / 100.0;
    let theta = (-6.2e-6 * c.powi(3) + 0.093104 * c.powi(2) + (876_600.0 * 3600.0 + 8_640_184.812_866) * c + 67_310.548_41)
        * (std::f64::consts::PI / 180.0)
        / 240.0;
    theta.rem_euclid(std::f64::consts::TAU)
}

/// TEME to the unit-sphere frame at `unix_s`, unscaled (multiply the result
/// by `1 / R` for metres): `ECEF = Rz(-gmst) * TEME` (polar motion ignored;
/// it is metres), then the `(y, z, x)` remap.
#[must_use]
pub fn teme_to_sphere(unix_s: f64) -> DMat3 {
    let (s, c) = gmst_rad(unix_s).sin_cos();
    let rz = DMat3::from_cols(DVec3::new(c, -s, 0.0), DVec3::new(s, c, 0.0), DVec3::Z);
    let remap = DMat3::from_cols(DVec3::new(0.0, 0.0, 1.0), DVec3::new(1.0, 0.0, 0.0), DVec3::new(0.0, 1.0, 0.0));
    remap * rz
}

/// The eye in the unit-sphere frame: the view-space origin taken back
/// through [`GlobeFrame::globe_view`].
#[must_use]
pub fn eye_sphere(frame: &GlobeFrame) -> DVec3 {
    eye_from_inverse_view(&frame.globe_view.inverse())
}

/// [`eye_sphere`] for a caller that already holds `globe_view.inverse()`
/// (the atmosphere uniform), so the frame inverts it once.
#[must_use]
pub fn eye_from_inverse_view(inverse_view: &glam::DMat4) -> DVec3 {
    inverse_view.transform_point3(DVec3::ZERO)
}

/// `m` in spec §2.4: the distance from the planet's centre to the closest
/// point of the segment eye `e` to point `p`. The segment clears the planet
/// when `m > 1`.
#[must_use]
pub fn closest_approach(e: DVec3, p: DVec3) -> f64 {
    let d = p - e;
    let dd = d.dot(d);
    if dd == 0.0 {
        return e.length();
    }
    let t = (-e.dot(d) / dd).clamp(0.0, 1.0);
    (e + d * t).length()
}

/// The soft occlusion edge: `smoothstep(1 - eps, 1 + eps, m)`, `eps` being
/// the half-width in sphere units, which the renderer takes from
/// `altitude_pipeline::occlusion_eps` (`sqrt(|E|² - 1) / focal`), not
/// `1 / px_per_radius`. That holds the ramp at a fixed ANGULAR width, about
/// 2 px on the optical axis; off axis a pixel subtends less angle
/// (`cos²` of the off-axis angle), so the same ramp spans up to about
/// 3.3 px in the frame's corners.
///
/// Never NaN: a non-finite `e` or `p` (so a non-finite `m`) is hidden, 0;
/// an `eps` that is not finite and positive gives the hard edge, `m > 1`.
#[must_use]
pub fn visibility(e: DVec3, p: DVec3, eps: f64) -> f64 {
    let m = closest_approach(e, p);
    if !m.is_finite() {
        return 0.0;
    }
    if !(eps.is_finite() && eps > 0.0) {
        return if m > 1.0 { 1.0 } else { 0.0 };
    }
    let x = ((m - (1.0 - eps)) / (2.0 * eps)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// The independent discriminator for [`visibility`]: `p` is in sight of `e`
/// when the angle between them at the centre is less than the sum of their
/// horizon angles, `acos(1/|E|) + acos(1/|P|)`. A point inside the planet,
/// or an eye inside it, sees nothing; nor does a non-finite input (every
/// comparison with NaN is false).
#[must_use]
pub fn visible_by_horizon_angle(e: DVec3, p: DVec3) -> bool {
    if p.length() < 1.0 || e.length() < 1.0 {
        return false;
    }
    e.angle_between(p) < (1.0 / e.length()).acos() + (1.0 / p.length()).acos()
}

/// Screen pixels per sphere unit at the planet centre's distance:
/// `focal_px / |E|`.
#[must_use]
pub fn px_per_radius(camera: &Camera, e: DVec3) -> f64 {
    let focal = camera.focal_px();
    focal / e.length()
}

/// The depth cue: `sqrt(|E| / |P - E|)` clamped to `0.7..=1.5`, so a point
/// at the planet centre's distance draws at 1.
#[must_use]
pub fn size_factor(e: DVec3, p: DVec3) -> f64 {
    (e.length() / (p - e).length()).sqrt().clamp(0.7, 1.5)
}

/// Cubic Hermite between keyframes `(p0, v0)` and `(p1, v1)` `delta_s`
/// apart, at `s` in `0..=1`. Velocities are per second, so they are scaled
/// by `delta_s` into per-interval tangents. Exact at both ends.
#[must_use]
pub fn hermite(p0: DVec3, v0: DVec3, p1: DVec3, v1: DVec3, delta_s: f64, s: f64) -> DVec3 {
    let (s2, s3) = (s * s, s * s * s);
    p0 * (2.0 * s3 - 3.0 * s2 + 1.0)
        + v0 * delta_s * (s3 - 2.0 * s2 + s)
        + p1 * (-2.0 * s3 + 3.0 * s2)
        + v1 * delta_s * (s3 - s2)
}

/// Earth's rotation rate, rad/s (IERS): how fast an inertial ring turns
/// against the Earth-fixed view per scene second.
pub const EARTH_ROTATION_RAD_S: f64 = 7.292_115_146_7e-5;

/// The velocity of [`hermite`] at `s`, per SECOND (not per interval): the
/// `s`-derivative divided by `delta_s`. Equals `v0` at `s = 0` and `v1` at
/// `s = 1`.
#[must_use]
pub fn hermite_velocity(p0: DVec3, v0: DVec3, p1: DVec3, v1: DVec3, delta_s: f64, s: f64) -> DVec3 {
    let s2 = s * s;
    (p1 - p0) * ((6.0 * s - 6.0 * s2) / delta_s) + v0 * (3.0 * s2 - 4.0 * s + 1.0) + v1 * (3.0 * s2 - 2.0 * s)
}

/// Whether sphere point `p` is at or behind the globe's near plane, where a
/// perspective divide would mirror it back onto the screen.
#[must_use]
pub fn behind_near(frame: &GlobeFrame, camera: &Camera, p: DVec3) -> bool {
    frame.globe_view.transform_point3(p).z > -super::globe::globe_near(camera)
}

/// The CPU mirror of the point shader's position: sphere point `p` to
/// logical screen pixels, `None` when [`behind_near`]. The globe clip is NOT
/// flipped (shader.wgsl: "the flip applies to the Mercator position only")
/// and `project_clip_to_screen` negates x, so x is negated going in.
/// Occlusion by the planet is [`visibility`]'s question, not this one's.
#[must_use]
pub fn project_altitude(camera: &Camera, frame: &GlobeFrame, p: DVec3) -> Option<(f32, f32)> {
    if behind_near(frame, camera, p) {
        return None;
    }
    let mut clip = frame.globe_view_proj * DVec4::new(p.x, p.y, p.z, 1.0);
    clip.x = -clip.x;
    super::project_clip_to_screen(camera, clip.as_vec4())
}

/// A polyline above (or on) the globe: an orbit ring. `points` are metres
/// in the layer's [`RefFrame`], exactly as a keyframed layer's
/// [`KeyState::position_m`] is: TEME for `Inertial` (turned by
/// [`teme_to_sphere`] per frame), the sphere frame's axes for `EarthFixed`.
/// `closed` adds the segment from the last point back to the first.
#[derive(Clone, Debug)]
pub struct AltitudeLine {
    /// The vertices, metres, in the layer's [`RefFrame`].
    pub points: Vec<DVec3>,
    /// The line's colour (alpha included).
    pub color: crate::style::Rgba,
    /// Whether the last point joins back to the first (a ring).
    pub closed: bool,
}

/// The part of segment `a -> b` (view space, the camera looking down `-z`)
/// in front of the near plane, `z <= -near`, as the kept range of `t` along
/// `a + (b - a) t`: `(0, 1)` when both ends are in front, `None` when both
/// are behind, otherwise cut where the segment crosses `z = -near`. The
/// line shader's near clip, restated on the CPU (`vs_line`).
#[must_use]
pub fn clip_segment_to_near(a_view: DVec3, b_view: DVec3, near: f64) -> Option<(f64, f64)> {
    let (fa, fb) = (a_view.z <= -near, b_view.z <= -near);
    match (fa, fb) {
        (true, true) => Some((0.0, 1.0)),
        (false, false) => None,
        _ => {
            let t = (-near - a_view.z) / (b_view.z - a_view.z);
            if !t.is_finite() {
                return None;
            }
            let t = t.clamp(0.0, 1.0);
            Some(if fa { (0.0, t) } else { (t, 1.0) })
        }
    }
}

/// How many objects a browser build propagates per frame when filling a
/// keyframe on the main thread: MG-S1's `wasm_chunk`
/// (docs/measurements/2026-09-29-satellites.md, the MG-S1 row: 150 ns a
/// propagation, `clamp(floor(1 ms / (3 x ns)), 100, 1000)` = 1000), about
/// 0.15 ms of a 16 ms frame at the measured native cost.
pub const WASM_KEYFRAME_CHUNK: usize = 1_000;

/// Scene time, Unix seconds: `anchor_scene_unix` advanced by
/// `elapsed_wall_s` wall seconds at `rate` scene seconds per wall second.
/// Negative `elapsed_wall_s` and negative `rate` are both allowed (a scrub,
/// a clock running backwards). A non-finite `rate` or elapsed time holds
/// the anchor rather than handing a NaN to every consumer.
#[must_use]
pub fn scene_time_at(anchor_scene_unix: f64, elapsed_wall_s: f64, rate: f64) -> f64 {
    let t = anchor_scene_unix + elapsed_wall_s * rate;
    if t.is_finite() { t } else { anchor_scene_unix }
}

/// Every object's state at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct Keyframe {
    /// The instant, Unix seconds (scene time).
    pub t_unix: f64,
    /// One state per object, in the source's object order.
    pub states: Vec<KeyState>,
}

/// What a frame should draw, from [`KeyframeScheduler::tick`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tick {
    /// The two keyframes around the scene time, when both are held.
    pub pair: Option<(f64, f64)>,
    /// The fraction of the interval, `(t - t0) / delta`, in `0..1`.
    pub s: f32,
    /// A keyframe to propagate now: at most one, and never while another
    /// is in flight.
    pub request: Option<f64>,
    /// The pair is not held yet (a scrub, a jump, a changed interval): draw
    /// the last positions, dimmed, until it is.
    pub hold: bool,
}

/// Which keyframes a keyframed layer needs, for a scene clock that may run
/// at any rate, in either direction, and jump. Holds at most three keyframe
/// times -- the pair around the scene time and one prefetched past it in
/// the direction of travel -- and has at most one in flight.
///
/// There is no separate path for a jump: a scene time the held pair does not
/// cover requests `t0` then `t1` and holds until they land. Nothing here
/// reads a clock or is stateful in scene time the way sgp4's cached
/// deep-space integrator is (which panics going backwards, `orbit::propagate`
/// `state_at`'s doc): every request is an independent instant.
///
/// Every time is `k * delta` for an integer `k`, computed the one way, so
/// a prefetched time and the same time reached later compare equal.
#[derive(Debug, Clone)]
pub struct KeyframeScheduler {
    delta: f64,
    held: Vec<f64>,
    in_flight: Option<f64>,
    last_t: f64,
}

impl KeyframeScheduler {
    /// A scheduler at interval `delta_s` (normally
    /// `keyframe_delta_s(rate)`, which `tick` re-derives every frame).
    #[must_use]
    pub fn new(delta_s: f64) -> Self {
        let delta = if delta_s.is_finite() && delta_s > 0.0 { delta_s } else { KEYFRAME_DELTA_MIN_S };
        Self { delta, held: Vec::with_capacity(4), in_flight: None, last_t: 0.0 }
    }

    /// The interval in force.
    #[must_use]
    pub fn delta_s(&self) -> f64 {
        self.delta
    }

    /// Whether keyframe time `t` is held.
    #[must_use]
    pub fn holds(&self, t: f64) -> bool {
        self.held.iter().any(|&h| same_time(h, t, self.delta))
    }

    /// Whether a requested keyframe has not arrived yet.
    #[must_use]
    pub fn waiting(&self) -> bool {
        self.in_flight.is_some()
    }

    /// This frame's pair, `s`, hold state and request at scene time
    /// `scene_unix` and clock `rate`. See the type's doc.
    pub fn tick(&mut self, scene_unix: f64, rate: f64) -> Tick {
        let delta = keyframe_delta_s(rate);
        if delta != self.delta {
            // A new interval is a new grid: nothing held lies on it.
            self.delta = delta;
            self.held.clear();
            self.in_flight = None;
        }
        if !scene_unix.is_finite() {
            return Tick { pair: None, s: 0.0, request: None, hold: true };
        }
        self.last_t = scene_unix;
        let n = (scene_unix / delta).floor();
        let (t0, t1) = (n * delta, (n + 1.0) * delta);
        let s = (((scene_unix - t0) / delta) as f32).clamp(0.0, 1.0);
        let idle = self.in_flight.is_none();
        let tick = if self.holds(t0) && self.holds(t1) {
            // Paused (rate 0): nothing is coming, so nothing is prefetched.
            let want = if rate > 0.0 && s > 0.5 {
                Some((n + 2.0) * delta)
            } else if rate < 0.0 && s < 0.5 {
                Some((n - 1.0) * delta)
            } else {
                None
            };
            let request = want.filter(|&k| idle && !self.holds(k));
            Tick { pair: Some((t0, t1)), s, request, hold: false }
        } else {
            let request = if !idle {
                None
            } else if !self.holds(t0) {
                Some(t0)
            } else {
                Some(t1)
            };
            Tick { pair: None, s, request, hold: true }
        };
        if tick.request.is_some() {
            self.in_flight = tick.request;
        }
        tick
    }

    /// Keyframe `t_unix`, the one in flight, has arrived. Returns whether it
    /// was accepted: a result for a request dropped by a changed interval
    /// is ignored. Holding a fourth time evicts the one farthest from the
    /// last scene time.
    pub fn completed(&mut self, t_unix: f64) -> bool {
        match self.in_flight {
            Some(k) if same_time(k, t_unix, self.delta) => self.in_flight = None,
            _ => return false,
        }
        if !self.holds(t_unix) {
            self.held.push(t_unix);
        }
        while self.held.len() > 3 {
            let t = self.last_t;
            let far = self
                .held
                .iter()
                .enumerate()
                .max_by(|a, b| (a.1 - t).abs().total_cmp(&(b.1 - t).abs()))
                .map(|(i, _)| i)
                .expect("non-empty");
            self.held.swap_remove(far);
        }
        true
    }
}

/// Keyframe-time equality. The tolerance (`delta * 1e-9`, ~6e-8 s) is below
/// the f64 ulp at present-day Unix times (2.4e-7 s at 1.8e9), so in practice
/// this is exact equality -- correct because every time is `n * delta`
/// computed one way and the worker echoes `t` back bit for bit.
fn same_time(a: f64, b: f64, delta: f64) -> bool {
    (a - b).abs() <= delta * 1e-9
}

/// A keyframe filled in steps, so a browser build can spread one over
/// several frames ([`WASM_KEYFRAME_CHUNK`] objects a step); a native worker
/// fills it in one step of `usize::MAX`.
#[derive(Debug, Clone)]
pub struct KeyframeBuilder {
    t_unix: f64,
    next: usize,
    states: Vec<KeyState>,
}

impl KeyframeBuilder {
    #[must_use]
    pub fn new(t_unix: f64, len: usize) -> Self {
        Self { t_unix, next: 0, states: vec![KeyState::HIDDEN; len] }
    }

    /// Fills the next `budget` objects' states. Returns whether the
    /// keyframe is complete.
    pub fn step(&mut self, src: &dyn KeyframeSource, budget: usize) -> bool {
        let len = self.states.len();
        if self.next < len {
            let end = self.next.saturating_add(budget.max(1)).min(len);
            src.states_at(self.t_unix, self.next..end, &mut self.states[self.next..end]);
            self.next = end;
        }
        self.next >= len
    }

    #[must_use]
    pub fn finish(self) -> Keyframe {
        Keyframe { t_unix: self.t_unix, states: self.states }
    }
}

/// The window of `s` over which object `i` exists between keyframes `k0`
/// (at `t0`) and `k1` (at `t0 + delta_s`): `[0, 1]` when both ends are
/// valid, `[1, 0]` (never drawn) when neither is, and otherwise the valid
/// side up to the instant the object stops (or from the instant it starts)
/// having a state, found by bisecting `src` six times (1/64 of the
/// interval). An object that decays mid-interval vanishes there rather than
/// flying along a Hermite curve toward a zero state.
#[must_use]
pub fn hidden_bounds(src: &dyn KeyframeSource, i: usize, k0: &KeyState, k1: &KeyState, t0: f64, delta_s: f64) -> [f32; 2] {
    match (k0.valid, k1.valid) {
        (true, true) => [0.0, 1.0],
        (false, false) => [1.0, 0.0],
        (first, _) => {
            let mut out = [KeyState::HIDDEN];
            let mut valid_at = |s: f64| {
                src.states_at(t0 + s * delta_s, i..i + 1, &mut out);
                out[0].valid
            };
            // `lo` is always on k0's side of the change, `hi` on k1's.
            let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
            for _ in 0..6 {
                let mid = 0.5 * (lo + hi);
                if valid_at(mid) == first { lo = mid } else { hi = mid }
            }
            if first { [0.0, lo as f32] } else { [hi as f32, 1.0] }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::{gmst_rad, J2000_UNIX_S};

    /// The first instant after J2000 at which GMST wraps through zero, by
    /// bisection over the predicate "GMST has not yet wrapped" (true at
    /// J2000, where GMST is ~280 deg; false half a sidereal day later, at
    /// ~100 deg).
    pub fn unix_where_gmst_is_zero() -> f64 {
        let g0 = gmst_rad(J2000_UNIX_S);
        let (mut lo, mut hi) = (J2000_UNIX_S, J2000_UNIX_S + 86_164.090_5 / 2.0);
        assert!(gmst_rad(hi) < g0);
        for _ in 0..200 {
            let mid = 0.5 * (lo + hi);
            if mid <= lo || mid >= hi {
                break;
            }
            if gmst_rad(mid) >= g0 { lo = mid } else { hi = mid }
        }
        hi
    }

    /// The first instant after [`unix_where_gmst_is_zero`] at which GMST
    /// reaches `target` (`0 <= target < π`), by bisection on the half
    /// sidereal day over which GMST climbs from 0 to π.
    pub fn unix_where_gmst_is(target: f64) -> f64 {
        assert!((0.0..std::f64::consts::PI).contains(&target));
        let (mut lo, mut hi) = (unix_where_gmst_is_zero(), unix_where_gmst_is_zero() + 86_164.090_5 / 2.0);
        for _ in 0..200 {
            let mid = 0.5 * (lo + hi);
            if mid <= lo || mid >= hi {
                break;
            }
            if gmst_rad(mid) < target { lo = mid } else { hi = mid }
        }
        hi
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::camera::Camera;
    use crate::render::globe::globe_frame;
    use glam::DVec3;

    fn cam(lon: f64, lat: f64, zoom: f64, w: f32, h: f32) -> Camera {
        Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: w, viewport_height_px: h, fov_y: Camera::DEFAULT_FOV_Y }
    }
    fn at(lon: f64, lat: f64, alt_km: f64) -> DVec3 {
        geo_to_sphere(GeoPoint { lon, lat, alt_m: alt_km * 1000.0 })
    }

    #[test]
    fn a_segment_crossing_the_near_plane_keeps_only_its_front_part() {
        let near = 0.5;
        // View space: the camera looks down -z. a is in front (z = -10), b behind (z = +10).
        let (a, b) = (DVec3::new(0.0, 0.0, -10.0), DVec3::new(0.0, 0.0, 10.0));
        let (t0, t1) = clip_segment_to_near(a, b, near).expect("the front part survives");
        assert_eq!(t0, 0.0);
        let cut = a + (b - a) * t1;
        assert!((cut.z + near).abs() < 1e-12, "cut at z = -near, got {}", cut.z);
        assert!(clip_segment_to_near(DVec3::new(0.0, 0.0, 1.0), DVec3::new(0.0, 0.0, 2.0), near).is_none(), "wholly behind");
        assert_eq!(clip_segment_to_near(DVec3::new(0.0, 0.0, -2.0), DVec3::new(1.0, 0.0, -3.0), near), Some((0.0, 1.0)));
        // Reversed direction: behind -> front keeps the far end.
        let (t0, t1) = clip_segment_to_near(b, a, near).unwrap();
        assert!(t0 > 0.0 && t1 == 1.0);
    }

    #[test]
    fn the_spec_occlusion_table_holds_for_leo_and_geo() {
        let c = cam(0.0, 0.0, 0.0, 800.0, 800.0);
        let e = eye_sphere(&globe_frame(&c));
        // Spec §2.6's formula: R = 512 / 2π world units, d = H / (2 tan 30° 2^z), |E| = (R + d) / R. At
        // H = 800 that is 9.50 R; §2.4's "~10.9" is the same formula at H = 932 (the phone's height), so the
        // test states the formula rather than the misquoted figure. The table below holds either way
        // (acos(1/9.50) = 83.96 deg, so 400 km is in sight out to 103.8 deg).
        let (r, d) = (512.0 / std::f64::consts::TAU, 800.0 / (2.0 * (std::f64::consts::PI / 6.0).tan()));
        let want = (r + d) / r;
        assert!((e.length() - want).abs() < 1e-6, "|E| = {} vs the §2.6 formula's {want}", e.length());
        let eps = 1.0 / px_per_radius(&c, e);
        for (lon, want) in [(0.0, true), (100.0, true), (110.0, false), (180.0, false)] {
            let p = at(lon, 0.0, 400.0);
            assert_eq!(visible_by_horizon_angle(e, p), want, "discriminator, 400 km at lon {lon}");
            assert_eq!(visibility(e, p, eps) > 0.5, want, "shader predicate, 400 km at lon {lon}");
        }
        for (lon, want) in [(90.0, true), (180.0, false)] {
            let p = at(lon, 0.0, 35_786.0);
            assert_eq!(visible_by_horizon_angle(e, p), want, "GEO at lon {lon}");
            assert_eq!(visibility(e, p, eps) > 0.5, want);
        }
    }

    #[test]
    fn a_point_inside_the_planet_is_hidden() {
        let c = cam(0.0, 0.0, 0.0, 800.0, 800.0);
        let e = eye_sphere(&globe_frame(&c));
        assert_eq!(visibility(e, DVec3::new(0.0, 0.0, 0.5), 1e-3), 0.0);
        // Off the eye's axis the segment's closest point is not P itself, so this exercises the clamp.
        for p in [DVec3::new(0.5, 0.0, 0.0), DVec3::new(0.0, -0.9, 0.3), DVec3::new(0.0, 0.0, -0.99)] {
            assert_eq!(visibility(e, p, 1e-3), 0.0, "{p:?}");
            assert!(!visible_by_horizon_angle(e, p), "{p:?}");
        }
    }

    #[test]
    fn non_finite_inputs_are_hidden_or_safe_never_nan() {
        let e = DVec3::new(0.0, 0.0, 10.0);
        let p = DVec3::new(0.0, 0.0, 2.0);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let q = DVec3::new(bad, 0.0, 2.0);
            assert_eq!(visibility(e, q, 1e-3), 0.0, "p with {bad}");
            assert_eq!(visibility(q, p, 1e-3), 0.0, "e with {bad}");
            assert!(!visible_by_horizon_angle(e, q) && !visible_by_horizon_angle(q, p), "{bad}");
            // A bad eps falls back to the hard edge.
            assert_eq!(visibility(e, p, bad), 1.0, "eps {bad}, in sight");
            assert_eq!(visibility(e, DVec3::new(0.0, 0.0, -2.0), bad), 0.0, "eps {bad}, behind");
        }
        for eps in [0.0, -1e-3] {
            assert_eq!(visibility(e, p, eps), 1.0);
            assert_eq!(visibility(e, DVec3::new(0.0, 0.0, -2.0), eps), 0.0);
            // m exactly 1: hidden, not 0/0.
            assert_eq!(visibility(DVec3::new(-10.0, 1.0, 0.0), DVec3::new(10.0, 1.0, 0.0), eps), 0.0);
        }
        assert_eq!(keyframe_delta_s(f64::NAN), KEYFRAME_DELTA_MIN_S);
        assert_eq!(keyframe_delta_s(f64::INFINITY), KEYFRAME_DELTA_MAX_S);
        assert_eq!(keyframe_delta_s(f64::NEG_INFINITY), KEYFRAME_DELTA_MAX_S);
    }

    #[test]
    fn an_eye_inside_the_geo_shell_culls_what_is_behind_it() {
        // Spec §2.6: lat 85, z-2 puts the eye at ~4.3 R, inside GEO's 6.62 R.
        let c = cam(0.0, 85.0, -2.0, 1440.0, 900.0);
        let f = globe_frame(&c);
        let e = eye_sphere(&f);
        assert!(e.length() < 6.6, "|E| = {}", e.length());
        let behind = e + e.normalize() * 2.0; // further out than the eye, along its own line of sight
        assert!(behind_near(&f, &c, behind));
        assert!(project_altitude(&c, &f, behind).is_none(), "culled, never mirrored through w < 0");
    }

    #[test]
    fn the_predicate_agrees_with_the_horizon_angle_on_ten_thousand_pairs() {
        let mut s = 0x5eed_u64;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut disagree = 0;
        for _ in 0..10_000 {
            let dir = |r: &mut dyn FnMut() -> f64| DVec3::new(r() - 0.5, r() - 0.5, r() - 0.5).normalize();
            let e = dir(&mut rnd) * (1.5 + 40.0 * rnd());
            let p = dir(&mut rnd) * (1.01 + 6.0 * rnd());
            let eps = 1e-3;
            let m = closest_approach(e, p);
            if (m - 1.0).abs() < 2.0 * eps {
                continue; // within eps of the boundary: either answer is right
            }
            if (visibility(e, p, eps) > 0.5) != visible_by_horizon_angle(e, p) {
                disagree += 1;
            }
        }
        assert_eq!(disagree, 0);
    }

    #[test]
    fn the_frame_remap_lands_on_the_surface_pixels_the_tiles_use() {
        // ECEF (R,0,0) is (lon 0, lat 0); (0,R,0) is lon 90. The spec's third row, (0,0,R) against the north
        // pole, cannot be asked of WorldProjector (Mercator stops at 85.051), so the z axis is exercised at
        // lat 84.9 instead: ECEF R*(cos 84.9, 0, sin 84.9) against the world point (0, 84.9).
        let r = crate::render::globe::GLOBE_RADIUS_M;
        let (s, c) = 84.9f64.to_radians().sin_cos();
        for (ecef, lon, lat, clon, clat) in [
            (DVec3::new(r, 0.0, 0.0), 0.0, 0.0, 0.0, 0.0),
            (DVec3::new(0.0, r, 0.0), 90.0, 0.0, 60.0, 0.0),
            (DVec3::new(r * c, 0.0, r * s), 0.0, 84.9, 0.0, 60.0),
        ] {
            let c = cam(clon, clat, 1.0, 1024.0, 1024.0);
            let f = globe_frame(&c);
            let got = project_altitude(&c, &f, ecef_to_sphere(ecef)).expect("in front");
            let anchors = crate::render::globe::GlobeAnchors::new(&f);
            let want = crate::render::globe::WorldProjector::new(&c, &anchors)
                .project(crate::overlay::geom::lon_lat_to_world(lon, lat))
                .expect("visible");
            assert!((got.0 - want.0).abs() < 0.5 && (got.1 - want.1).abs() < 0.5, "{ecef:?}: {got:?} vs {want:?}");
        }
    }

    #[test]
    fn geo_projects_where_perspective_says() {
        let c = cam(0.0, 0.0, -1.0, 1024.0, 1024.0);
        let f = globe_frame(&c);
        let centre = project_altitude(&c, &f, at(0.0, 0.0, 35_786.0)).unwrap();
        assert!((centre.0 - 512.0).abs() < 0.5 && (centre.1 - 512.0).abs() < 0.5, "{centre:?}");
        // Lon 90 is at the planet centre's depth, so its offset is 6.618 R x focal / |E|.
        let e = eye_sphere(&f);
        let side = project_altitude(&c, &f, at(90.0, 0.0, 35_786.0)).unwrap();
        let want = at(90.0, 0.0, 35_786.0).length() * px_per_radius(&c, e);
        assert!(((side.0 - 512.0).abs() as f64 - want).abs() < 1.0, "{side:?} vs {want}");
        assert!(side.0 > 512.0, "east is screen right on the globe (shader.wgsl's flip comment)");
    }

    #[test]
    fn hermite_on_a_circle_is_within_the_spec_estimate() {
        // A 6,900 km circular orbit, period 5,550 s: the error is about r*theta^4/384 (spec §3.2).
        let r = 6_900_000.0;
        let w = std::f64::consts::TAU / 5_550.0;
        let pos = |t: f64| DVec3::new(r * (w * t).cos(), r * (w * t).sin(), 0.0);
        let vel = |t: f64| DVec3::new(-r * w * (w * t).sin(), r * w * (w * t).cos(), 0.0);
        for (delta, bound_m) in [(60.0, 1.0), (120.0, 10.0), (240.0, 110.0), (300.0, 400.0)] {
            let mut worst = 0.0f64;
            for i in 1..20 {
                let s = f64::from(i) / 20.0;
                let got = hermite(pos(0.0), vel(0.0), pos(delta), vel(delta), delta, s);
                worst = worst.max((got - pos(s * delta)).length());
            }
            assert!(worst < bound_m, "delta {delta}: {worst} m");
        }
        // The ends are exact.
        assert_eq!(hermite(pos(0.0), vel(0.0), pos(60.0), vel(60.0), 60.0, 0.0), pos(0.0));
    }

    /// `hermite_velocity` is `hermite`'s time derivative: the ends are the
    /// keyframe velocities, and a central difference agrees in between.
    #[test]
    fn hermite_velocity_is_the_derivative_of_hermite() {
        let r = 6_900_000.0;
        let w = std::f64::consts::TAU / 5_550.0;
        let pos = |t: f64| DVec3::new(r * (w * t).cos(), r * (w * t).sin(), 0.0);
        let vel = |t: f64| DVec3::new(-r * w * (w * t).sin(), r * w * (w * t).cos(), 0.0);
        let delta = 120.0;
        let (p0, v0, p1, v1) = (pos(0.0), vel(0.0), pos(delta), vel(delta));
        assert!((hermite_velocity(p0, v0, p1, v1, delta, 0.0) - v0).length() < 1e-9);
        assert!((hermite_velocity(p0, v0, p1, v1, delta, 1.0) - v1).length() < 1e-9);
        for i in 1..10 {
            let s = f64::from(i) / 10.0;
            let h = 1e-4;
            let fd = (hermite(p0, v0, p1, v1, delta, s + h) - hermite(p0, v0, p1, v1, delta, s - h)) / (2.0 * h * delta);
            let got = hermite_velocity(p0, v0, p1, v1, delta, s);
            assert!((got - fd).length() < 1e-3 * v0.length(), "s {s}: {got} vs {fd}");
            // And it tracks the orbit's own speed.
            assert!((got.length() - r * w).abs() < 1e-3 * r * w, "s {s}: {} vs {}", got.length(), r * w);
        }
    }

    #[test]
    fn the_size_factor_is_a_clamped_cue() {
        let e = DVec3::new(0.0, 0.0, 10.0);
        assert!((size_factor(e, DVec3::ZERO) - 1.0).abs() < 1e-12, "at the centre's distance");
        assert_eq!(size_factor(e, DVec3::new(0.0, 0.0, 9.99)), 1.5);
        assert_eq!(size_factor(e, DVec3::new(0.0, 0.0, -6.6)), (10.0f64 / 16.6).sqrt().max(0.7));
    }

    #[test]
    fn keyframe_delta_follows_the_rate() {
        assert_eq!(keyframe_delta_s(1.0), 60.0);
        assert_eq!(keyframe_delta_s(-800.0), 120.0, "200 snaps down to 120");
        assert_eq!(keyframe_delta_s(479.0), 60.0);
        assert_eq!(keyframe_delta_s(480.0), 120.0);
        assert_eq!(keyframe_delta_s(959.0), 120.0);
        assert_eq!(keyframe_delta_s(960.0), 240.0);
        assert_eq!(keyframe_delta_s(100_000.0), KEYFRAME_DELTA_MAX_S);
        assert_eq!(keyframe_delta_s(0.0), 60.0);
    }

    #[test]
    fn teme_to_sphere_turns_by_gmst() {
        // At a time when GMST = 0 the TEME x axis is ECEF x, i.e. sphere +z.
        let m = teme_to_sphere(crate::render::altitude::tests_support::unix_where_gmst_is_zero());
        let x = m * DVec3::X;
        assert!((x - DVec3::Z).length() < 1e-9, "{x:?}");
        // GMST = 0 cannot tell Rz(-g) from Rz(+g). At GMST = π/2 TEME x lands at ECEF (0, -1, 0) -- lon -90,
        // the Earth having turned east under it -- which is sphere -x; the wrong sign would give +x.
        let m = teme_to_sphere(crate::render::altitude::tests_support::unix_where_gmst_is(std::f64::consts::FRAC_PI_2));
        let x = m * DVec3::X;
        assert!((x - DVec3::NEG_X).length() < 1e-9, "{x:?}");
        // And TEME y at GMST = π/2 is ECEF x: sphere +z.
        assert!((m * DVec3::Y - DVec3::Z).length() < 1e-9);
    }

    /// A fake clock and a fake source whose result lands `lag` frames after the request.
    fn run_lagged(s: &mut KeyframeScheduler, times: &[f64], rate: f64, lag: usize) -> Vec<Tick> {
        let mut out = Vec::new();
        let mut pending: Option<(f64, usize)> = None;
        for (frame, &t) in times.iter().enumerate() {
            if let Some((k, due)) = pending {
                if frame >= due {
                    s.completed(k);
                    pending = None;
                }
            }
            let tick = s.tick(t, rate);
            if let Some(k) = tick.request {
                assert!(pending.is_none(), "frame {frame}: requested {k} while another keyframe was in flight");
                pending = Some((k, frame + lag));
            }
            out.push(tick);
        }
        out
    }
    fn run(s: &mut KeyframeScheduler, times: &[f64], rate: f64) -> Vec<Tick> {
        run_lagged(s, times, rate, 1)
    }

    #[test]
    fn keyframes_are_requested_past_the_midpoint_and_never_two_at_once() {
        // A slow source (5 frames per keyframe): the in-flight assertion is inside run_lagged.
        let times: Vec<f64> = (0..600).map(|f| 1_000_020.0 + f64::from(f) * 0.5).collect(); // 300 scene s
        run_lagged(&mut KeyframeScheduler::new(60.0), &times, 1.0, 5);
        // A prompt source: once settled, never held, and every prefetch happens at s > 0.5.
        let ticks = run(&mut KeyframeScheduler::new(60.0), &times, 1.0);
        for t in &ticks[3..] {
            assert!(!t.hold, "held while running forward at 1x");
            if let (Some(k), Some((t0, t1))) = (t.request, t.pair) {
                assert!(k > t1 && t.s > 0.5, "early prefetch {k} in [{t0},{t1}] at s {}", t.s);
            }
        }
    }

    #[test]
    fn a_scrub_recovers_in_three_frames() {
        let mut s = KeyframeScheduler::new(60.0);
        let mut times: Vec<f64> = (0..10).map(|f| 1_000_000.0 + f64::from(f)).collect();
        times.extend((0..5).map(|f| 1_050_000.0 + f64::from(f))); // jump 50,000 s ahead
        let ticks = run(&mut s, &times, 1.0);
        let after = &ticks[10..];
        assert!(after[0].hold, "the jump frame holds the last positions");
        let back = after.iter().position(|t| !t.hold).expect("recovers");
        assert!(back <= 3, "recovered after {back} frames");
    }

    #[test]
    fn a_scrub_backwards_recovers_too() {
        // The carried Task 1 finding: time running backwards must never be a
        // special case (sgp4's cached-state path panics on it).
        let mut s = KeyframeScheduler::new(60.0);
        let mut times: Vec<f64> = (0..10).map(|f| 1_050_000.0 + f64::from(f)).collect();
        times.extend((0..5).map(|f| 1_000_000.0 + f64::from(f)));
        let ticks = run(&mut s, &times, 1.0);
        let back = ticks[10..].iter().position(|t| !t.hold).expect("recovers");
        assert!(back <= 3, "recovered after {back} frames");
    }

    #[test]
    fn a_wall_clock_jump_of_an_hour_recomputes_two_keyframes_not_hundreds() {
        let mut s = KeyframeScheduler::new(60.0);
        let mut times: Vec<f64> = (0..10).map(|f| 2_000_000.0 + f64::from(f) * 0.016).collect();
        times.extend((0..20).map(|f| 2_003_600.0 + f64::from(f) * 0.016));
        let ticks = run(&mut s, &times, 1.0);
        let requests = ticks[10..].iter().filter(|t| t.request.is_some()).count();
        assert!(requests <= 3, "{requests} keyframes requested after a 1 h jump (2 to recover, at most 1 prefetch)");
    }

    #[test]
    fn running_backwards_prefetches_the_earlier_keyframe() {
        let mut s = KeyframeScheduler::new(60.0);
        let times: Vec<f64> = (0..400).map(|f| 3_000_000.0 - f64::from(f) * 0.5).collect();
        let ticks = run(&mut s, &times, -1.0);
        for t in &ticks[3..] {
            assert!(!t.hold);
            if let (Some(k), Some((t0, _))) = (t.request, t.pair) {
                assert!(k < t0 && t.s < 0.5);
            }
        }
    }

    #[test]
    fn a_changed_rate_that_changes_delta_is_a_scrub_not_a_stall() {
        let mut s = KeyframeScheduler::new(keyframe_delta_s(1.0));
        run(&mut s, &(0..10).map(|f| 4_000_000.0 + f64::from(f)).collect::<Vec<_>>(), 1.0);
        let ticks = run(&mut s, &(0..6).map(|f| 4_000_010.0 + f64::from(f) * 250.0 / 60.0).collect::<Vec<_>>(), 1000.0);
        // The brief's 250 predates the 240 s cap (the Task 2 ruling): 1000 x 0.25 clamps to 240.
        assert_eq!(s.delta_s(), KEYFRAME_DELTA_MAX_S);
        assert!(ticks.iter().skip(3).all(|t| !t.hold));
    }

    #[test]
    fn a_result_for_a_dropped_request_is_ignored() {
        let mut s = KeyframeScheduler::new(60.0);
        assert_eq!(s.tick(600.0, 1.0).request, Some(600.0));
        // A new rate, a new interval (240 s): the 600 s request is dropped and 480 s asked for.
        assert_eq!(s.tick(600.0, 1000.0).request, Some(480.0));
        assert!(!s.completed(600.0), "a stale result is ignored");
        assert!(!s.holds(600.0));
        assert!(s.completed(480.0));
    }

    struct DecaysAt(f64);
    impl KeyframeSource for DecaysAt {
        fn len(&self) -> usize {
            1
        }
        fn states_at(&self, t: f64, _r: std::ops::Range<usize>, out: &mut [KeyState]) {
            out[0] = if t <= self.0 {
                KeyState { position_m: glam::DVec3::new(6_800_000.0, 0.0, 0.0), velocity_m_s: glam::DVec3::new(0.0, 7_600.0, 0.0), valid: true }
            } else {
                KeyState::HIDDEN
            };
        }
    }

    #[test]
    fn an_object_that_decays_mid_interval_vanishes_not_flies_to_the_origin() {
        let src = DecaysAt(30.0);
        let mut k = [KeyState::HIDDEN; 2];
        src.states_at(0.0, 0..1, &mut k[0..1]);
        src.states_at(60.0, 0..1, &mut k[1..2]);
        let hs = hidden_bounds(&src, 0, &k[0], &k[1], 0.0, 60.0);
        assert_eq!(hs[0], 0.0);
        assert!((hs[1] - 0.5).abs() <= 1.0 / 64.0, "vanishes at s = {} (want 0.5)", hs[1]);
        let inst = crate::render::altitude_pipeline::keyed_instance(&k[0], &k[1], 60.0, hs, 0);
        let p1 = glam::Vec3::from(inst.p1);
        assert!(p1.length() > 1.0, "p1 = {p1:?}: the missing end must be extrapolated, not zero");
    }

    #[test]
    fn an_object_launched_mid_interval_appears_there() {
        struct LaunchesAt(f64);
        impl KeyframeSource for LaunchesAt {
            fn len(&self) -> usize {
                1
            }
            fn states_at(&self, t: f64, r: std::ops::Range<usize>, out: &mut [KeyState]) {
                DecaysAt(1e12).states_at(t, r, out);
                if t < self.0 {
                    out[0] = KeyState::HIDDEN;
                }
            }
        }
        let src = LaunchesAt(45.0);
        let mut k = [KeyState::HIDDEN; 2];
        src.states_at(0.0, 0..1, &mut k[0..1]);
        src.states_at(60.0, 0..1, &mut k[1..2]);
        let hs = hidden_bounds(&src, 0, &k[0], &k[1], 0.0, 60.0);
        assert_eq!(hs[1], 1.0);
        assert!((hs[0] - 0.75).abs() <= 1.0 / 64.0, "appears at s = {}", hs[0]);
        assert_eq!(hidden_bounds(&src, 0, &KeyState::HIDDEN, &KeyState::HIDDEN, 0.0, 60.0), [1.0, 0.0]);
    }

    #[test]
    fn the_chunked_builder_matches_one_call() {
        let src = DecaysAt(1e12);
        let mut b = KeyframeBuilder::new(10.0, 1);
        assert!(b.step(&src, WASM_KEYFRAME_CHUNK));
        assert_eq!(b.finish().states[0].position_m.x, 6_800_000.0);
        struct Many;
        impl KeyframeSource for Many {
            fn len(&self) -> usize {
                2_500
            }
            fn states_at(&self, t: f64, r: std::ops::Range<usize>, out: &mut [KeyState]) {
                for (o, i) in out.iter_mut().zip(r) {
                    *o = KeyState { position_m: glam::DVec3::splat(i as f64 + t), velocity_m_s: glam::DVec3::ZERO, valid: true };
                }
            }
        }
        let mut chunked = KeyframeBuilder::new(3.0, Many.len());
        let mut steps = 1;
        while !chunked.step(&Many, WASM_KEYFRAME_CHUNK) {
            steps += 1;
        }
        let mut whole = KeyframeBuilder::new(3.0, Many.len());
        assert!(whole.step(&Many, usize::MAX));
        assert_eq!(steps, 3);
        assert_eq!(chunked.finish(), whole.finish());
    }

    #[test]
    fn scene_time_is_the_anchor_advanced_at_the_rate() {
        assert_eq!(scene_time_at(1_000.0, 2.0, 60.0), 1_120.0);
        assert_eq!(scene_time_at(1_000.0, 2.0, -60.0), 880.0);
        assert_eq!(scene_time_at(1_000.0, 2.0, f64::NAN), 1_000.0);
    }

    #[test]
    fn dragging_the_rate_across_240_to_960_keeps_the_keyframes() {
        // A rate slider dragged 240 -> 960 over 60 frames: the interval changes
        // only at the 480 and 960 steps, so the layer holds for a handful of
        // frames, not the whole drag.
        let mut s = KeyframeScheduler::new(keyframe_delta_s(240.0));
        let mut t = 5_000_000.0;
        let mut pending: Option<f64> = None;
        let mut held = 0;
        let mut regrids = 0;
        let mut last_delta = s.delta_s();
        for f in 0..60 {
            let rate = 240.0 + 720.0 * f64::from(f) / 59.0;
            t += rate / 60.0;
            if let Some(k) = pending.take() {
                s.completed(k);
            }
            let tick = s.tick(t, rate);
            if s.delta_s() != last_delta {
                regrids += 1;
                last_delta = s.delta_s();
            }
            pending = tick.request;
            held += usize::from(tick.hold && f >= 2);
        }
        assert_eq!(regrids, 2, "the interval changed {regrids} times");
        assert!(held <= 4, "held on {held} of 58 frames");
    }
}
