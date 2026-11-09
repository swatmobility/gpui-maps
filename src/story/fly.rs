//! A story `fly` segment: the van Wijk & Nuij "optimal path" zoom-and-pan
//! (spec §3.2, plan S2.3), as a pure function of the eased fraction `k`.
//!
//! The paper is van Wijk and Nuij, "Smooth and efficient zooming and
//! panning", InfoVis 2003. The parameterisation follows MapLibre GL JS
//! (BSD-3, Copyright (c) 2023 MapLibre contributors), read at
//! `maplibre-gl-js` (github.com/maplibre/maplibre-gl-js) commit `0d4b6f1`:
//!
//! - `src/ui/camera.ts` `flyTo` (line 1016): `w0` (1074), `w1` (1076),
//!   `u1` (1079), the `minZoom` cap on `rho` (1086), `zoomOutFactor`
//!   (1096), `r0` (1106), `w(s)` (1110), `u(s)` (1116), `S` (1121), the
//!   degenerate zoom-only / same-place case (1124), the duration from
//!   `speed` or `screenSpeed` (1138), `maxDuration` (1142), `s = k * S`
//!   (1157).
//! - `src/geo/projection/mercator_camera_helper.ts` `handleFlyTo` (141):
//!   the flat `pixelPathLength` (160) and `scaleOfZoom` (162).
//! - `src/geo/projection/vertical_perspective_camera_helper.ts`
//!   `handleFlyTo` (358): the globe `pixelPathLength` (389), the
//!   normalised zooms (391-393), `deltaLng` (404) and the per-frame zoom
//!   `interpolatedZoom + getZoomAdjustment(0, newCenter.lat)` (418).
//! - `src/geo/projection/globe_utils.ts`: `getGlobeRadiusPixels` (83),
//!   `getZoomAdjustment` (349), `integrateSecX` (391) and
//!   `interpolateLngLatForGlobe` (406).
//!
//! **Mapbox GL JS 3.x was not read or ported.** The licence notice is in
//! `src/story/PROVENANCE.md`.
//!
//! **Two forms.** The flat form moves the centre along a straight line in
//! Mercator world space. The globe form (the globe is on and any of the
//! start, the end or the arc's peak is in the globe band) moves it with
//! latitude linear in `u` and longitude on MapLibre's integrated `1/cos`
//! curve, and plans in *normalised* zoom, so the drawn planet keeps its
//! size as the centre changes latitude. See [`plan`].
//!
//! **Exact and reproducible.** [`pose`] reads nothing but its plan and
//! `k`. `k <= 0` is `from` and `k >= 1` is `to`, bit for bit.
//!
//! Like `script.rs`, this module denies `unwrap`, `expect` and indexing
//! outside tests (spec §2, "No-panic contract").
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use super::camera_path::Pose;
use super::script::{Diagnostic, Easing, Severity};
use crate::camera_control::{
    ease_pose, lon_lat_at_world, shortest_bearing_delta, wrap_bearing, wrap_lon, MAX_MERCATOR_LAT, MIN_ZOOM,
};
use crate::render::globe::{globe_radius_world, transition_for_zoom};
use crate::render::Camera;

/// MapLibre's default `curve` (rho), `camera.ts` line 1036.
pub const DEFAULT_CURVE: f64 = 1.42;
/// MapLibre's default `speed`, in rho-screenfuls per second, `camera.ts`
/// line 1035.
pub const DEFAULT_SPEED: f64 = 1.2;
/// How long a `fly` to the same place (which MapLibre hands to `easeTo`)
/// eases for when it names no `duration`: `easeTo`'s own default of 500 ms
/// (`camera.ts` line 754).
const SAME_PLACE_EASE_SECONDS: f64 = 0.5;
/// The default easing: MapLibre's `defaultEasing`, the CSS `ease` curve.
const DEFAULT_EASING: Easing = Easing::Ease;

/// The van Wijk & Nuij path for one flight, in MapLibre's form. Spans are
/// in pixels at the start zoom; `s` runs from 0 to [`s_total`](Self::s_total).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VanWijk {
    pub rho: f64,
    pub w0: f64,
    pub w1: f64,
    pub u1: f64,
    /// `r(false)`, the ascent's zoom-out factor. `0.0` on a zoom-only path,
    /// which does not use it.
    pub r0: f64,
    /// `S`, the whole path in rho-screenfuls.
    pub s_total: f64,
    /// `Some(k)` on a zoom-only path (no pan), with `k = sign(w1 - w0)`:
    /// `w(s) = e^(k rho s)` and `u(s) = 0`.
    pub zoom_only: Option<f64>,
}

impl VanWijk {
    /// The path from a visible span `w0` to `w1` (pixels, at the start
    /// scale) over a ground distance `u1` (pixels, at the start scale),
    /// with curvature `rho`. `None` is the degenerate "same place"
    /// (`camera.ts` line 1126: MapLibre hands that to `easeTo`), and also
    /// any input that is not finite and positive where it must be.
    pub fn new(w0: f64, w1: f64, u1: f64, rho: f64) -> Option<Self> {
        if !(w0.is_finite() && w1.is_finite() && u1.is_finite() && rho.is_finite() && w0 > 0.0 && w1 > 0.0 && rho > 0.0) {
            return None;
        }
        if u1.abs() < 2e-6 && (w0 - w1).abs() < 1e-6 {
            return None;
        }
        let rho2 = rho * rho;
        // zoomOutFactor, camera.ts line 1096.
        let r = |descent: bool| {
            let sign = if descent { -1.0 } else { 1.0 };
            let b = (w1 * w1 - w0 * w0 + sign * rho2 * rho2 * u1 * u1) / (2.0 * if descent { w1 } else { w0 } * rho2 * u1);
            ((b * b + 1.0).sqrt() - b).ln()
        };
        let r0 = r(false);
        let s = (r(true) - r0) / rho;
        if u1.abs() < 2e-6 || !s.is_finite() {
            let k = (w1 - w0).signum();
            let s_total = (w1 / w0).ln().abs() / rho;
            return Some(VanWijk { rho, w0, w1, u1, r0: 0.0, s_total, zoom_only: Some(k) });
        }
        Some(VanWijk { rho, w0, w1, u1, r0, s_total: s, zoom_only: None })
    }

    /// The visible span at `s`, as a fraction of `w0`.
    pub fn w(&self, s: f64) -> f64 {
        match self.zoom_only {
            Some(k) => (k * self.rho * s).exp(),
            None => self.r0.cosh() / (self.r0 + self.rho * s).cosh(),
        }
    }

    /// The distance travelled at `s`, as a fraction of the path.
    pub fn u(&self, s: f64) -> f64 {
        match self.zoom_only {
            Some(_) => 0.0,
            None => {
                self.w0 * (self.r0.cosh() * (self.r0 + self.rho * s).tanh() - self.r0.sinh()) / (self.rho * self.rho) / self.u1
            }
        }
    }

    /// The largest `w` on the path (the lowest zoom). On a pan arc it is
    /// at `s = -r0 / rho` when that lies inside `[0, S]`; otherwise, and on
    /// a zoom-only path, at an end.
    fn w_max(&self) -> f64 {
        let ends = 1.0_f64.max(self.w(self.s_total));
        match self.zoom_only {
            Some(_) => ends,
            None => ends.max(self.w((-self.r0 / self.rho).clamp(0.0, self.s_total))),
        }
    }
}

/// Which way the centre travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// A straight line in Mercator world space, the short way across the
    /// antimeridian (`mercator_camera_helper.ts`).
    Flat,
    /// On the sphere, in normalised zoom (`vertical_perspective_camera_helper.ts`).
    Globe,
}

/// A resolved flight: everything [`pose`] needs.
#[derive(Clone, Debug, PartialEq)]
pub struct FlyPlan {
    /// `None` for a flight to the same place, which is an [`ease_pose`].
    pub vw: Option<VanWijk>,
    pub form: Form,
    pub easing: Easing,
    pub from: Pose,
    pub to: Pose,
    pub z0: f64,
    pub z1: f64,
}

/// The `fly` segment's options (script.rs `Segment::Fly`), by value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlySpec {
    pub duration: Option<f64>,
    pub speed: Option<f64>,
    pub screen_speed: Option<f64>,
    pub curve: Option<f64>,
    pub min_zoom: Option<f64>,
    pub max_duration: Option<f64>,
    pub easing: Option<Easing>,
}

/// An error at `field` under the segment. The path is relative
/// (`/speed`); `CameraPath` prefixes `/camera/<i>`.
fn error(field: &str, code: &'static str, message: String) -> Diagnostic {
    Diagnostic { severity: Severity::Error, path: format!("/{field}"), code, message, hint: None }
}

fn positive(v: Option<f64>, field: &str, code: &'static str) -> Result<Option<f64>, Diagnostic> {
    match v {
        Some(x) if !(x.is_finite() && x > 0.0) => Err(error(field, code, format!("`{field}` must be a finite number above 0, got {x}"))),
        other => Ok(other),
    }
}

/// `log2(cos(new) / cos(old))`: what to add to a zoom at latitude `old` to
/// keep the drawn planet the same size at latitude `new`. MapLibre's
/// `getZoomAdjustment` (`globe_utils.ts` line 349), whose
/// `planetScaleAtLatitude` is `cos`. Latitudes are clamped to the Mercator
/// limit before the cosine.
fn zoom_adjustment(old_lat: f64, new_lat: f64) -> f64 {
    let cos = |lat: f64| lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT).to_radians().cos();
    (cos(new_lat) / cos(old_lat)).log2()
}

/// The great-circle angle between two centres, in radians (haversine).
fn great_circle_angle(a: &Camera, b: &Camera) -> f64 {
    let lat = |c: &Camera| c.center_lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT).to_radians();
    let (p1, p2) = (lat(a), lat(b));
    let dl = (b.center_lon - a.center_lon).to_radians();
    let h = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * h.clamp(0.0, 1.0).sqrt().asin()
}

/// `b - a` in degrees, the short way round (MapLibre's
/// `differenceOfAnglesDegrees`): in `[-180, 180)`.
fn angle_difference(a: f64, b: f64) -> f64 {
    (b - a + 180.0).rem_euclid(360.0) - 180.0
}

/// The world-space step from `from`'s centre to `to`'s, x the short way
/// across the antimeridian.
fn world_delta(from: &Camera, to: &Camera) -> glam::DVec3 {
    let world = f64::from(crate::render::WORLD_SIZE);
    let (a, b) = (from.world_center_f64(), to.world_center_f64());
    let mut dx = b.x - a.x;
    if dx > world / 2.0 {
        dx -= world;
    } else if dx < -world / 2.0 {
        dx += world;
    }
    glam::DVec3::new(dx, b.y - a.y, 0.0)
}

/// The VanWijk path for one form. `dz` is the zoom change the path spans
/// (normalised in the globe form) and `min_dz` the lowest the arc may
/// reach, relative to the start (MapLibre's `scaleOfMinZoom`).
fn van_wijk(w0: f64, dz: f64, u1: f64, min_dz: f64, curve: f64) -> Option<VanWijk> {
    let w1 = w0 / dz.exp2();
    // camera.ts line 1086: only ever reduce rho. With u1 == 0 the bound is
    // +inf and rho is unchanged. The bound inverts the far-flight
    // approximation `w_max ~ rho^2 u1 / 2`, so a capped arc reaches just
    // under the floor (a few hundredths of a zoom level), as in MapLibre.
    let w_max = w0 / min_dz.exp2();
    let rho = curve.min((w_max / u1 * 2.0).sqrt());
    VanWijk::new(w0, w1, u1, rho)
}

/// Resolve a flight from `from` to `to` (both already resolved poses; an
/// `anchor_offset_px` on the target is already folded into `to`'s centre,
/// so the arc flies centre to centre) for a `viewport` of logical pixels.
/// Returns the plan and its duration in seconds; a duration of `0` is a
/// jump (`max_duration` exceeded).
///
/// **Form selection** (spec §3.2): plan the flat form first. If the pose
/// is shown on the globe (`to.globe`: the path shows a segment in its
/// target's projection from `k > 0`) and the start, the end or the flat
/// arc's peak zoom is in the globe band (`transition_for_zoom > 0`),
/// re-plan in the globe form.
///
/// **Duration:** `duration`, else `S / speed`, else
/// `S / (screen_speed / rho)`, else `S / DEFAULT_SPEED` (camera.ts line
/// 1138; `screen_speed` wins over `speed` there too). A flight to the same
/// place eases for `duration`, or 0.5 s.
///
/// **min_zoom:** as MapLibre, the arc is capped at
/// `min(max(min_zoom, MIN_ZOOM), z0, z1)`, `MIN_ZOOM` when none is named
/// (MapLibre uses `tr.minZoom`).
pub fn plan(from: Pose, to: Pose, viewport: (f32, f32), spec: &FlySpec) -> Result<(FlyPlan, f64), Diagnostic> {
    let duration = positive(spec.duration, "duration", "duration_not_positive")?;
    let speed = positive(spec.speed, "speed", "speed_not_positive")?;
    let screen_speed = positive(spec.screen_speed, "screen_speed", "speed_not_positive")?;
    let curve = positive(spec.curve, "curve", "curve_not_positive")?.unwrap_or(DEFAULT_CURVE);
    if let Some(m) = spec.max_duration.filter(|m| !m.is_finite() || *m < 0.0) {
        return Err(error("max_duration", "max_duration_invalid", format!("`max_duration` must be a finite number, 0 or above, got {m}")));
    }
    if let Some(m) = spec.min_zoom.filter(|m| !m.is_finite()) {
        return Err(error("min_zoom", "min_zoom_invalid", format!("`min_zoom` must be finite, got {m}")));
    }
    let (a, b) = (&from.camera, &to.camera);
    let (z0, z1) = (a.zoom, b.zoom);
    let w0 = f64::from(viewport.0.max(viewport.1));
    let floor = spec.min_zoom.unwrap_or(MIN_ZOOM).max(MIN_ZOOM);

    // Flat: mercator_camera_helper.ts lines 156-167.
    let flat_u1 = world_delta(a, b).length() * z0.exp2();
    let flat = van_wijk(w0, z1 - z0, flat_u1, floor.min(z0).min(z1) - z0, curve);
    let flat_peak = peak_of(flat.as_ref(), z0, z1);
    let in_band = |z: f64| transition_for_zoom(z) > 0.0;
    let form = if to.globe && (in_band(z0) || in_band(z1) || in_band(flat_peak)) { Form::Globe } else { Form::Flat };

    let vw = match form {
        Form::Flat => flat,
        // vertical_perspective_camera_helper.ts lines 389-399, with zooms
        // relative to z0: the normalised start is z0 + zadj(lat0, 0), the
        // normalised end z1 + zadj(lat1, 0).
        Form::Globe => {
            let u1 = great_circle_angle(a, b) * globe_radius_world(a) * z0.exp2();
            let zn0 = z0 + zoom_adjustment(a.center_lat, 0.0);
            let zn1 = z1 + zoom_adjustment(b.center_lat, 0.0);
            let floor_n = floor + zoom_adjustment(b.center_lat, 0.0);
            van_wijk(w0, zn1 - zn0, u1, floor_n.min(zn0).min(zn1) - zn0, curve)
        }
    };

    let mut seconds = match (&vw, duration) {
        (_, Some(d)) => d,
        (None, None) => SAME_PLACE_EASE_SECONDS,
        (Some(v), None) => {
            let per_second = match (screen_speed, speed) {
                (Some(ss), _) => ss / v.rho,
                (None, Some(sp)) => sp,
                (None, None) => DEFAULT_SPEED,
            };
            v.s_total / per_second
        }
    };
    // camera.ts line 1142: MapLibre's `options.maxDuration &&` skips 0.
    // Only a real flight: a same-place fly returns `easeTo` at line 1126,
    // before that check, and `easeTo` has no `maxDuration`.
    let over_max = vw.is_some() && spec.max_duration.is_some_and(|m| m > 0.0 && seconds > m);
    if over_max || !seconds.is_finite() {
        seconds = 0.0;
    }
    let easing = spec.easing.clone().unwrap_or(DEFAULT_EASING);
    Ok((FlyPlan { vw, form, easing, from, to, z0, z1 }, seconds))
}

/// The lowest zoom on a flat-form arc: `z0 + log2(1 / w_max)`, or the
/// lower end for a flight to the same place.
fn peak_of(vw: Option<&VanWijk>, z0: f64, z1: f64) -> f64 {
    match vw {
        Some(v) => z0 - v.w_max().log2(),
        None => z0.min(z1),
    }
}

/// The lowest zoom the flight reaches, before any latitude adjustment:
/// `z0 + log2(1 / w)` at the widest point of the arc. Spec §3.2 writes the
/// peak as `w(S/2)`, which is the widest point only on a same-zoom arc;
/// this takes the true maximum of `w` (at `s = -r0 / rho`), which is what
/// "the lowest zoom on the arc" means and is the more conservative band
/// test.
pub fn peak_zoom(plan: &FlyPlan) -> f64 {
    peak_of(plan.vw.as_ref(), plan.z0, plan.z1)
}

/// The camera at eased fraction `k` of the flight. `k <= 0` (or NaN) is
/// `from` and `k >= 1` is `to`, exactly. Pitch is linear in `k` and the
/// bearing turns the shortest way round, as `flyTo` does.
pub fn pose(plan: &FlyPlan, k: f64) -> Camera {
    let (a, b) = (&plan.from.camera, &plan.to.camera);
    if k.is_nan() || k <= 0.0 {
        return *a;
    }
    if k >= 1.0 {
        return *b;
    }
    let Some(vw) = plan.vw else {
        return ease_pose(a, b, k);
    };
    let s = k * vw.s_total;
    let (u, w) = (vw.u(s), vw.w(s));
    let kf = k as f32;
    let mut c = Camera {
        pitch: a.pitch + (b.pitch - a.pitch) * kf,
        bearing: wrap_bearing(a.bearing + shortest_bearing_delta(a.bearing, b.bearing) * kf),
        ..*a
    };
    match plan.form {
        Form::Flat => {
            let at = a.world_center_f64() + world_delta(a, b) * u;
            let (lon, lat) = lon_lat_at_world(at).unwrap_or((a.center_lon, a.center_lat));
            c.center_lon = lon;
            c.center_lat = lat;
            c.zoom = plan.z0 - w.log2();
        }
        Form::Globe => {
            let (lon, lat) = interpolate_lon_lat_for_globe(
                (a.center_lon, a.center_lat),
                angle_difference(a.center_lon, b.center_lon),
                b.center_lat - a.center_lat,
                u,
            );
            c.center_lon = wrap_lon(lon);
            c.center_lat = lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
            // vertical_perspective_camera_helper.ts line 418: normalised
            // zoom plus getZoomAdjustment(0, lat); relative to z0 that is
            // zadj(lat0, lat).
            c.zoom = plan.z0 - w.log2() + zoom_adjustment(a.center_lat, c.center_lat);
        }
    }
    c
}

/// The integral of `1 / cos x` (`globe_utils.ts` line 391, `integrateSecX`).
fn integrate_sec(x: f64) -> f64 {
    let (sin, cos) = (0.5 * x).sin_cos();
    (sin + cos).ln() - (cos - sin).ln()
}

/// MapLibre's `interpolateLngLatForGlobe` (`globe_utils.ts` lines 406-440):
/// latitude linear in `t`; longitude re-timed along the integrated `1/cos`
/// curve so the globe's apparent turning speed stays even (its rate of
/// change of longitude is `1/cos(lat)`). For a latitude change of 1 degree
/// or less, longitude is linear too. Latitudes are clamped to the Mercator
/// limit first; if the re-timing is not finite it falls back to linear.
fn interpolate_lon_lat_for_globe(start: (f64, f64), d_lon: f64, d_lat: f64, t: f64) -> (f64, f64) {
    let lat0 = start.1.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
    let lat = lat0 + d_lat * t;
    if d_lat.abs() <= 1.0 {
        return (start.0 + d_lon * t, lat);
    }
    let end = (lat0 + d_lat).clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
    let different_hemispheres = end.signum() != lat0.signum() || end == 0.0 || lat0 == 0.0;
    let p0 = if different_hemispheres { -lat0.abs() } else { lat0.abs() }.to_radians();
    let p1 = end.abs().to_radians();
    let (v0, v1) = (integrate_sec(p0), integrate_sec(p1));
    let new_t = (integrate_sec(p0 + t * (p1 - p0)) - v0) / (v1 - v0);
    let new_t = if new_t.is_finite() { new_t } else { t };
    (start.0 + d_lon * new_t, lat)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn same_zoom_pan_of_four_spans_matches_the_table() {
        let v = VanWijk::new(1000.0, 1000.0, 4000.0, 1.42).unwrap();
        assert!(close(v.r0, -2.102637) && close(v.s_total, 2.961461));
        assert!(close(v.w(v.s_total / 2.0), 4.154934) && close(v.u(v.s_total / 2.0), 0.5));
        assert!(close(-v.w(v.s_total / 2.0).log2(), -2.054826));
        assert!(close(v.s_total / 1.2, 2.467884));
        assert!(close(v.u(v.s_total), 1.0) && close(v.w(v.s_total), 1.0));
    }

    #[test]
    fn zoom_in_three_levels_with_pan_matches_the_table() {
        let v = VanWijk::new(1920.0, 240.0, 10000.0, 1.42).unwrap();
        assert!(close(v.r0, -2.351714) && close(v.s_total, 4.782925));
        assert!(close(v.w(v.s_total / 2.0), 3.319369) && close(v.u(v.s_total / 2.0), 0.888889));
        assert!(close(v.s_total / 1.2, 3.985771));
        assert!(close(v.w(v.s_total), 0.125) && close(v.u(v.s_total), 1.0));
    }

    #[test]
    fn zoom_only_in_two_levels_matches_the_table() {
        let v = VanWijk::new(1000.0, 250.0, 0.0, 1.42).unwrap();
        assert!(v.zoom_only.is_some() && close(v.s_total, 0.976264) && close(v.s_total / 1.2, 0.813553));
        assert_eq!(v.u(0.3), 0.0);
        assert!(close(v.w(v.s_total), 0.25));
    }

    #[test]
    fn a_fly_to_the_same_place_is_an_ease_and_stays_finite() {
        assert!(VanWijk::new(1000.0, 1000.0, 0.0, 1.42).is_none());
        let from = pose(10.0, 20.0, 5.0);
        let to = Pose { camera: Camera { bearing: 1.0, pitch: 0.5, ..from.camera }, ..from };
        let (p, d) = plan(from, to, (800.0, 600.0), &FlySpec { speed: Some(1.2), ..FlySpec::default() }).unwrap();
        assert!(d.is_finite() && d >= 0.0);
        for k in [0.0, 0.25, 0.5, 1.0] {
            let c = super::pose(&p, k);
            assert!(c.zoom.is_finite() && c.bearing.is_finite());
        }
    }

    #[test]
    fn same_zoom_fly_dips_to_its_minimum_at_the_middle() {
        let (p, _) = plan(
            pose(0.0, 0.0, 14.0),
            pose(0.05, 0.0, 14.0),
            (1000.0, 1000.0),
            &FlySpec { speed: Some(1.2), easing: Some(Easing::Linear), ..FlySpec::default() },
        )
        .unwrap();
        let z: Vec<f64> = (0..=100).map(|i| super::pose(&p, i as f64 / 100.0).zoom).collect();
        let min_i = z.iter().enumerate().min_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(min_i, 50);
        assert!(z.windows(2).take(50).all(|w| w[1] <= w[0]) && z.windows(2).skip(50).all(|w| w[1] >= w[0]));
    }

    #[test]
    fn endpoints_are_exact() {
        let (from, to) = (pose(103.82, 1.35, 10.0), pose(139.76, 35.68, 11.0));
        let (p, _) = plan(from, to, (1920.0, 1080.0), &FlySpec { speed: Some(1.2), ..FlySpec::default() }).unwrap();
        assert_eq!(super::pose(&p, 0.0), from.camera);
        assert_eq!(super::pose(&p, 1.0), to.camera);
    }

    #[test]
    fn singapore_to_tokyo_selects_the_globe_form_and_london_to_paris_at_z14_the_flat_form() {
        let (sg, _) = plan(pose(103.82, 1.35, 10.0), pose(139.76, 35.68, 11.0), (1920.0, 1080.0), &FlySpec { speed: Some(1.2), ..FlySpec::default() }).unwrap();
        assert!(matches!(sg.form, Form::Globe), "peak {}", peak_zoom(&sg));
        let (lp, _) = plan(pose(-0.1276, 51.5072, 14.0), pose(-0.1200, 51.5100, 14.0), (1920.0, 1080.0), &FlySpec { speed: Some(1.2), ..FlySpec::default() }).unwrap();
        assert!(peak_zoom(&lp) > crate::render::globe::PROJECTION_BLEND_ZOOM_MAX, "peak {}", peak_zoom(&lp));
        assert!(matches!(lp.form, Form::Flat));
    }

    #[test]
    fn a_globe_fly_keeps_latitude_linear_in_u() {
        let (p, _) = plan(pose(103.82, 1.35, 4.0), pose(139.76, 35.68, 4.0), (1920.0, 1080.0), &FlySpec { speed: Some(1.2), easing: Some(Easing::Linear), ..FlySpec::default() }).unwrap();
        let vw = p.vw.unwrap();
        for i in 1..10 {
            let k = i as f64 / 10.0;
            let u = vw.u(k * vw.s_total);
            let lat = super::pose(&p, k).center_lat;
            assert!((lat - (1.35 + (35.68 - 1.35) * u)).abs() < 1e-6, "k={k} lat={lat}");
        }
    }

    #[test]
    fn a_globe_fly_across_the_antimeridian_goes_the_short_way() {
        let (p, _) = plan(pose(170.0, 0.0, 3.0), pose(-170.0, 0.0, 3.0), (1000.0, 1000.0), &FlySpec { speed: Some(1.2), ..FlySpec::default() }).unwrap();
        for i in 0..=20 {
            let lon = super::pose(&p, i as f64 / 20.0).center_lon;
            let unwrapped = if lon < 0.0 { lon + 360.0 } else { lon };
            assert!((170.0 - 1e-9..=190.0 + 1e-9).contains(&unwrapped), "{lon}");
        }
    }

    #[test]
    fn a_globe_fly_over_the_pole_region_stays_finite() {
        let (p, _) = plan(pose(0.0, 84.0, 3.0), pose(180.0, 84.0, 3.0), (1000.0, 1000.0), &FlySpec { speed: Some(1.2), ..FlySpec::default() }).unwrap();
        for i in 0..=50 {
            let c = super::pose(&p, i as f64 / 50.0);
            assert!(c.center_lat.is_finite() && c.zoom.is_finite() && c.center_lat.abs() <= crate::camera_control::MAX_MERCATOR_LAT);
        }
    }

    // ---- Task 9 review folds ----

    /// Minor 2: MapLibre hands a same-place fly to `easeTo` (camera.ts
    /// line 1126) before its `maxDuration` check (line 1142).
    #[test]
    fn max_duration_does_not_cut_a_same_place_fly() {
        let from = pose(10.0, 20.0, 5.0);
        let to = Pose { camera: Camera { bearing: 1.0, ..from.camera }, ..from };
        let (p, d) = plan(from, to, (800.0, 600.0), &FlySpec { duration: Some(3.0), max_duration: Some(1.0), ..FlySpec::default() }).unwrap();
        assert!(p.vw.is_none());
        assert_eq!(d, 3.0);
    }

    /// Minor 3: Tokyo to San Francisco, both forms. `Δlat > 1`, so the
    /// globe form re-times through `integrate_sec`.
    fn tokyo_to_sf(globe: bool) -> FlyPlan {
        let (mut from, mut to) = (pose(139.76, 35.68, 3.0), pose(-122.42, 37.77, 3.0));
        from.globe = globe;
        to.globe = globe;
        plan(from, to, (1000.0, 1000.0), &FlySpec::default()).unwrap().0
    }

    fn assert_east_across_the_antimeridian(p: &FlyPlan) {
        let mut prev = 139.76 - 1e-9;
        for i in 0..=40 {
            let lon = super::pose(p, f64::from(i) / 40.0).center_lon;
            let unwrapped = if lon < 0.0 { lon + 360.0 } else { lon };
            assert!((139.76 - 1e-9..=237.58 + 1e-9).contains(&unwrapped), "k={i}/40: {lon}");
            assert!(unwrapped >= prev - 1e-9, "monotone east: {prev} -> {unwrapped}");
            prev = unwrapped;
        }
        assert!((super::pose(p, 1.0).center_lon - -122.42).abs() < 1e-9);
    }

    #[test]
    fn tokyo_to_san_francisco_flat_goes_the_short_way() {
        let p = tokyo_to_sf(false);
        assert!(matches!(p.form, Form::Flat));
        assert_east_across_the_antimeridian(&p);
    }

    #[test]
    fn tokyo_to_san_francisco_on_the_globe_goes_the_short_way() {
        let p = tokyo_to_sf(true);
        assert!(matches!(p.form, Form::Globe));
        assert_east_across_the_antimeridian(&p);
    }

    /// Minor 4: MapLibre's `scaleZoom(planetScaleAtLatitude(60) /
    /// planetScaleAtLatitude(0)) = scaleZoom(0.5) = -1`.
    #[test]
    fn zoom_adjustment_matches_maplibre_at_sixty_degrees() {
        assert!((zoom_adjustment(0.0, 60.0) - -1.0).abs() < 1e-12, "{}", zoom_adjustment(0.0, 60.0));
    }

    #[test]
    fn over_max_duration_is_a_jump() {
        let (_, d) = plan(pose(0.0, 0.0, 3.0), pose(100.0, 0.0, 3.0), (1000.0, 1000.0), &FlySpec { speed: Some(0.01), max_duration: Some(1.0), ..FlySpec::default() }).unwrap();
        assert_eq!(d, 0.0);
    }

    fn pose(lon: f64, lat: f64, zoom: f64) -> Pose {
        Pose { camera: Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: 1000.0, viewport_height_px: 1000.0, fov_y: Camera::DEFAULT_FOV_Y }, globe: true }
    }

    // ---- beyond the brief ----

    /// Spec §13's "globe zoom normalisation" risk, checked numerically: on
    /// a globe fly between two latitudes at the same NORMALISED zoom, the
    /// drawn planet's radius in pixels (`globe_radius_world * 2^zoom`)
    /// changes only by the van Wijk zoom-out `1 / w(s)`, never by the
    /// latitude. So `radius * 2^zoom * w(s)` is constant. With the sign of
    /// `zoom_adjustment` flipped it would drift by `1/cos^2(lat)`.
    #[test]
    fn a_pure_globe_pan_at_normalised_zoom_keeps_the_planet_size() {
        let (lat0, lat1) = (5.0_f64, 55.0_f64);
        // Same normalised zoom: z1 + zadj(lat1, 0) == z0 + zadj(lat0, 0).
        let z0 = 4.0;
        let z1 = z0 + zoom_adjustment(lat0, lat1);
        let (p, _) = plan(pose(10.0, lat0, z0), pose(40.0, lat1, z1), (1000.0, 1000.0), &FlySpec { easing: Some(Easing::Linear), ..FlySpec::default() }).unwrap();
        assert!(matches!(p.form, Form::Globe));
        let vw = p.vw.unwrap();
        let size = |k: f64| {
            let c = super::pose(&p, k);
            globe_radius_world(&c) * c.zoom.exp2() * vw.w(k * vw.s_total)
        };
        let reference = size(0.0);
        for i in 1..=10 {
            let k = f64::from(i) / 10.0;
            let s = size(k);
            assert!(((s - reference) / reference).abs() < 1e-6, "k={k}: {s} vs {reference}");
        }
    }

    #[test]
    fn flat_fly_endpoints_meet_the_arc_continuously() {
        let (from, to) = (pose(-0.1276, 51.5072, 14.0), pose(-0.1200, 51.5100, 15.0));
        let (p, _) = plan(from, to, (1920.0, 1080.0), &FlySpec::default()).unwrap();
        assert!(matches!(p.form, Form::Flat));
        let near_end = super::pose(&p, 1.0 - 1e-9);
        assert!((near_end.zoom - 15.0).abs() < 1e-6 && (near_end.center_lon - -0.12).abs() < 1e-6 && (near_end.center_lat - 51.51).abs() < 1e-6);
        let near_start = super::pose(&p, 1e-9);
        assert!((near_start.zoom - 14.0).abs() < 1e-6 && (near_start.center_lon - -0.1276).abs() < 1e-6);
    }

    #[test]
    fn globe_fly_endpoints_meet_the_arc_continuously() {
        let (from, to) = (pose(103.82, 1.35, 10.0), pose(139.76, 35.68, 11.0));
        let (p, _) = plan(from, to, (1920.0, 1080.0), &FlySpec::default()).unwrap();
        assert!(matches!(p.form, Form::Globe));
        let near_end = super::pose(&p, 1.0 - 1e-9);
        assert!((near_end.zoom - 11.0).abs() < 1e-5 && (near_end.center_lon - 139.76).abs() < 1e-5 && (near_end.center_lat - 35.68).abs() < 1e-5, "{near_end:?}");
    }

    #[test]
    fn globe_off_always_flies_flat() {
        let mut from = pose(103.82, 1.35, 10.0);
        let mut to = pose(139.76, 35.68, 11.0);
        from.globe = false;
        to.globe = false;
        let (p, _) = plan(from, to, (1920.0, 1080.0), &FlySpec::default()).unwrap();
        assert!(matches!(p.form, Form::Flat));
    }

    #[test]
    fn duration_comes_from_speed_screen_speed_or_is_explicit() {
        let (from, to) = (pose(0.0, 0.0, 3.0), pose(20.0, 0.0, 3.0));
        let (p, d_speed) = plan(from, to, (1000.0, 1000.0), &FlySpec { speed: Some(2.0), ..FlySpec::default() }).unwrap();
        let vw = p.vw.unwrap();
        assert!(close(d_speed, vw.s_total / 2.0));
        let (_, d_screen) = plan(from, to, (1000.0, 1000.0), &FlySpec { screen_speed: Some(2.0), ..FlySpec::default() }).unwrap();
        assert!(close(d_screen, vw.s_total / (2.0 / vw.rho)));
        let (_, d) = plan(from, to, (1000.0, 1000.0), &FlySpec { duration: Some(7.5), ..FlySpec::default() }).unwrap();
        assert_eq!(d, 7.5);
    }

    #[test]
    fn a_min_zoom_caps_the_arc() {
        let (from, to) = (pose(0.0, 0.0, 8.0), pose(60.0, 0.0, 8.0));
        let (free, _) = plan(from, to, (1000.0, 1000.0), &FlySpec::default()).unwrap();
        let (capped, _) = plan(from, to, (1000.0, 1000.0), &FlySpec { min_zoom: Some(6.0), ..FlySpec::default() }).unwrap();
        assert!(peak_zoom(&free) < 6.0, "free peak {}", peak_zoom(&free));
        // MapLibre's cap `rho = sqrt(2 wMax / u1)` inverts the far-flight
        // approximation `w_max ~ rho^2 u1 / 2` (valid for u1 >> w0), so the
        // arc lands just under the floor, not exactly on it: 5.956 here.
        assert!((peak_zoom(&capped) - 6.0).abs() < 0.1 && peak_zoom(&capped) > peak_zoom(&free) + 1.0, "capped peak {}", peak_zoom(&capped));
    }

    #[test]
    fn bad_options_are_refused_at_their_field() {
        let (from, to) = (pose(0.0, 0.0, 3.0), pose(20.0, 0.0, 3.0));
        let cases = [
            (FlySpec { speed: Some(0.0), ..FlySpec::default() }, "speed_not_positive", "/speed"),
            (FlySpec { screen_speed: Some(f64::NAN), ..FlySpec::default() }, "speed_not_positive", "/screen_speed"),
            (FlySpec { duration: Some(-1.0), ..FlySpec::default() }, "duration_not_positive", "/duration"),
            (FlySpec { curve: Some(0.0), ..FlySpec::default() }, "curve_not_positive", "/curve"),
            (FlySpec { max_duration: Some(-1.0), ..FlySpec::default() }, "max_duration_invalid", "/max_duration"),
            (FlySpec { min_zoom: Some(f64::INFINITY), ..FlySpec::default() }, "min_zoom_invalid", "/min_zoom"),
        ];
        for (spec, code, path) in cases {
            let e = plan(from, to, (1000.0, 1000.0), &spec).unwrap_err();
            assert_eq!((e.code, e.path.as_str()), (code, path));
        }
    }
}
