//! The camera's rules, in one place every shell shares.
//!
//! Three shells used to own three copies of these rules, and the copies
//! disagreed. Native kept bearing in `[-PI, PI)` and snapped to north on
//! release. iOS kept it in `[0, 2*PI)` and snapped live, mid-gesture. The
//! browser had neither. See
//! `docs/superpowers/specs/2026-09-24-3d-camera-parity-design.md`. Every
//! constant and function below moved here from `src/map_view.rs` with its
//! doc comment, because the doc comments record why each rule exists.
//!
//! No `gpui`, `web_sys`, winit or UIKit type, and no clock read: time
//! arrives as a `Duration` argument. That is what lets `web/src/lib.rs` and
//! `ios/src/lib.rs` `#[path]`-include this file unchanged, and it is what
//! `tests/wasm_portability.rs` checks.

use std::time::Duration;

use crate::render::{Camera, WORLD_SIZE};

/// Web Mercator's latitude limit, in degrees: `atan(sinh(PI))`, where the
/// square world ends (`render/mod.rs` calls it +/-85.051). A camera target
/// is clamped to it, because a centre beyond it has no tile to stand on.
pub const MAX_MERCATOR_LAT: f64 = 85.051_128_779_806_59;

/// The zoom floor every shell clamps to. Native, iOS and web each used to
/// hardcode their OWN number at their own `CameraController::new` call site
/// -- native `0.0`, iOS and web `1.0` -- disagreeing with each other and, at
/// `1.0`, with what the globe camera can actually put on screen: a camera
/// parked at [`MAX_MERCATOR_LAT`] renders a sphere whose
/// [`crate::render::globe::globe_radius_world`] has grown by
/// `1/cos(85.0511\u{b0}) \u{2248} 11.5\u{d7}` over its equatorial size (that
/// function's `1/cos(lat)` term), and at z1 the resulting sphere is wide
/// enough to overflow even a 430-logical-px-wide phone viewport. That is
/// the exact shape of the bug this constant fixed: "in some positions I
/// cannot zoom out from the Earth" -- reported at z1, lat 85.05, tilted --
/// because `1.0` was already the floor, so scrolling out further hit the
/// clamp while the globe still filled the screen.
///
/// `-2.0` is the lowest zoom
/// `the_minimum_zoom_globe_fits_the_phone_viewport` (in
/// `src/render/globe.rs`) confirms keeps that worst-case sphere inside a
/// phone-width window with margin, pitch 0 -- and it is independently
/// MapLibre GL JS's own default (`defaultMinZoom = -2`,
/// `maplibre-gl-js/src/ui/map.ts:482`, BSD-3, Copyright (c)
/// 2023 MapLibre contributors). The tile cover clamps to z0 regardless
/// (`MapView::display_zoom`'s `.clamp(0.0, u8::MAX as f64)` and
/// `fetch_queue`'s saturating float-to-`u8` cast both floor a negative
/// camera zoom at the tile layer), so a negative camera zoom only ever
/// widens the globe view -- it never asks for a tile below z0.
pub const MIN_ZOOM: f64 = -2.0;

/// The lowest zoom a view may opt into with [`CameraController::set_zoom_range`] -- far enough out that the
/// geostationary shell fits on a phone at latitude 85 (spec 2026-09-29-satellites §2.6, R5). [`MIN_ZOOM`]
/// stays the DEFAULT floor: no view that does not ask goes below it.
pub const SPACE_MIN_ZOOM: f64 = -5.0;

/// The zoom ceiling every shell clamps to. `20.0` was always native's own
/// number (the old scroll-handler clamp its `CameraController::new` call
/// sites hardcoded); iOS and web used `19.0`, one level short, with a doc
/// comment that said "That is exactly what native does" -- which was true
/// of the OVERZOOM behaviour past a source's ceiling, just not of the exact
/// number. Unifying onto native's `20.0` closes that one-level gap rather
/// than opening a new one.
pub const MAX_ZOOM: f64 = 20.0;

/// The longest `ease_to` honoured. Longer (or non-finite, or negative, via
/// [`ease_duration_from_ms`]) becomes a `jump_to`: a camera that eases for
/// a day is a caller's bug, and the spec's rule is that every motion
/// reaches `advance() == false` in finite time.
pub const MAX_EASE_DURATION: Duration = Duration::from_secs(60);

/// A caller's duration in milliseconds -- a JS number, so possibly NaN
/// (an `undefined` argument), infinite or negative -- as a `Duration`
/// `ease_to` can use. Anything unusable, including more than
/// [`MAX_EASE_DURATION`], is `Duration::ZERO`: a jump. Never panics, which
/// `Duration::from_secs_f64` does for infinity and anything above ~1.8e22.
#[must_use]
pub fn ease_duration_from_ms(ms: f64) -> Duration {
    let secs = ms / 1000.0;
    if secs.is_finite() && secs > 0.0 && secs <= MAX_EASE_DURATION.as_secs_f64() {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

/// Degrees (a URL parameter or a JS number) as `f32` radians, converted in
/// f64 FIRST so a large finite value does not overflow `f32` as degrees,
/// and `None` if the result is not finite -- which [`CameraTarget`]'s
/// `None` then reads as "keep the current value".
#[must_use]
pub fn radians_from_degrees(deg: f64) -> Option<f32> {
    let radians = deg.to_radians() as f32;
    radians.is_finite().then_some(radians)
}

/// The camera's maximum pitch: 60 degrees.
///
/// MapLibre's long-standing default (`maxPitch ?? 60`,
/// `src/geo/transform_helper.ts:169`) and typical of navigation cameras.
/// Ferrostar's automotive camera sits at 45, below it (see the 3D camera
/// spec). One constant, cheap to change.
///
/// **What 60 costs in this renderer.** Our vertical FOV is 60 degrees
/// (MapLibre's is 36.87), so at this pitch the top frustum ray is exactly
/// horizontal: `far_distance` saturates to `MAX_FAR` and the old
/// `ground_radius` circle covered the whole world. The old 0.9 clamp existed
/// to stay under that point. What makes 60 safe now is not the clamp but the
/// cover: `cover_plan::flat_cover` walks the frustum nearest-first and stops
/// at `MAX_COVER_TILES`, and `horizon_fog` fades the ground out just inside
/// where it stopped.
///
/// The pitch ease clamps to exactly this value too, for the reason the old
/// comment gave: a target above what the camera can reach would never
/// converge and would hold the frame loop open forever.
pub const MAX_PITCH: f32 = std::f32::consts::FRAC_PI_3;

/// **The eye camera's pitch ceiling**: MapLibre GL JS's own `maxPitch`
/// limit (85 degrees, `src/geo/transform_helper.ts`). Only a map that
/// opts in ([`EyeCamera`], `MapView::set_eye_camera`) reaches past
/// [`MAX_PITCH`]; with this renderer's 60-degree FOV the horizon then
/// sits `tan(5 deg) / tan(30 deg)` = 15% of the half-height above the
/// centre, 42% from the top.
pub const EYE_MAX_PITCH: f32 = 85.0 * std::f32::consts::PI / 180.0;

/// The eye camera's zoom ceiling. An eye `h` metres over the ground at
/// pitch `p` is `h / cos p` from its look-at point, which is the camera
/// distance only at a zoom past [`MAX_ZOOM`]: 1.1 m at 85 degrees in a
/// 768-px-high view is z22.0 at Singapore's latitude, z22.6 at 1200 px;
/// a race car's in-car camera, 0.86 m up at 75 degrees with a 58-degree
/// FOV, is z24.7 at 1263 px and z25.0 at 1600 px. The tiles are
/// overzoomed from the source's own ceiling, and each tile's matrix is
/// composed in f64 (`camera::tile_view_projection`), so the extra zoom
/// costs no precision.
pub const EYE_MAX_ZOOM: f64 = 26.0;

/// **The low eye camera**: an eye a metre or two over the road, looking
/// nearly level, as from a driver's seat. A map opts in with
/// `MapView::set_eye_camera`; every other map keeps [`MAX_PITCH`] and
/// [`MAX_ZOOM`] and draws exactly what it drew.
///
/// Looking level, the ground runs to the horizon, so the cover is bounded
/// by DISTANCE instead (MapLibre bounds a high-pitch cover the same way,
/// with its far plane at `maxMercatorHorizonAngle`): no tile whose
/// nearest ground is farther than [`Self::fog_m`] from the eye is ever
/// requested (`cover_plan::CoverZoom::cutoff`), the distance LOD coarsens
/// what is nearer, and the ground fogs into the sky's horizon colour
/// before the cut (`cover_plan::eye_fog`). Above the horizon the sky pass
/// draws a gradient from `horizon` up to `sky`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EyeCamera {
    /// Radians, held to `MAX_PITCH..=EYE_MAX_PITCH`.
    pub max_pitch: f32,
    /// Held to `MAX_ZOOM..=EYE_MAX_ZOOM`.
    pub max_zoom: f64,
    /// The far cutoff, metres of ground from the eye: the fog is thick by
    /// here and nothing farther is fetched.
    pub fog_m: f64,
    /// The sky overhead.
    pub sky: crate::style::Rgba,
    /// The sky at the horizon, and the fog's colour.
    pub horizon: crate::style::Rgba,
    /// The camera's vertical field of view, degrees: 60 (the shared
    /// default) unless widened with [`Self::with_fov_deg`]. A cockpit
    /// camera reads wide at 75-80. `MapView::set_eye_camera` puts it on
    /// the map's camera (`Camera::fov_y`), and turning the eye camera off
    /// puts the default back.
    pub fov_y_deg: f64,
    /// The fog's reach as the eye climbs ([`AerialFog`]); `None` (the
    /// default) keeps [`Self::fog_m`] at every height.
    pub aerial: Option<AerialFog>,
}

/// **The eye camera's fog from altitude**, opted into with
/// [`EyeCamera::with_aerial_fog`]. [`EyeCamera::fog_m`] is tuned for an
/// eye at street level, where every distance is a level path through the
/// densest air; from a kilometre up the whole frame is already a
/// kilometre or more away, so a fixed reach fogs the entire view and the
/// cut ([`EyeCamera::cutoff_world`]) shows as a flat band under the sky.
/// Looking down a slant path through thinner air, the city reads clear
/// much farther: the reach and the clear distance grow with the eye's
/// height ([`crate::render::Camera::eye_height_m`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AerialFog {
    /// The eye height, metres, below which nothing changes: the street
    /// keeps [`EyeCamera::fog_m`] and its own fog start exactly.
    pub above_m: f64,
    /// Metres of cutoff added per metre of eye height over
    /// [`Self::above_m`], on top of [`EyeCamera::fog_m`].
    pub reach_per_altitude: f64,
    /// The view depth, metres per metre of eye height over
    /// [`Self::above_m`], before which the fog does not start
    /// (`cover_plan::eye_fog_clear`; never past
    /// [`crate::render::cover_plan::EYE_FOG_MAX_CLEAR_FRACTION`] of its end).
    pub clear_per_altitude: f64,
    /// The cutoff never grows past this, metres (the tile budget; the
    /// distance LOD keeps a far cutoff to a few dozen coarse tiles).
    pub max_reach_m: f64,
    /// Nor the clear distance past this, metres: from very high the far
    /// ground still fades toward the horizon.
    pub max_clear_m: f64,
}

impl AerialFog {
    /// How far `camera`'s eye is over [`Self::above_m`], metres; 0 at or
    /// under it, or for a camera whose height is not finite.
    #[must_use]
    pub fn climb_m(&self, camera: &Camera) -> f64 {
        let h = camera.eye_height_m() - self.above_m;
        if h.is_finite() { h.max(0.0) } else { 0.0 }
    }
}

impl EyeCamera {
    /// An eye camera with the full [`EYE_MAX_PITCH`] and [`EYE_MAX_ZOOM`]
    /// and the default 60-degree FOV.
    #[must_use]
    pub fn new(fog_m: f64, sky: crate::style::Rgba, horizon: crate::style::Rgba) -> Self {
        Self { max_pitch: EYE_MAX_PITCH, max_zoom: EYE_MAX_ZOOM, fog_m, sky, horizon, fov_y_deg: Camera::DEFAULT_FOV_Y.to_degrees(), aerial: None }
    }

    /// This eye camera with its fog reaching farther as it climbs
    /// ([`AerialFog`]).
    #[must_use]
    pub fn with_aerial_fog(self, aerial: AerialFog) -> Self {
        Self { aerial: Some(aerial), ..self }
    }

    /// The far cutoff at `camera`'s height, metres of ground from the eye:
    /// [`Self::fog_m`], plus [`AerialFog::reach_per_altitude`] per metre of
    /// eye height up to [`AerialFog::max_reach_m`] when the eye camera has
    /// one.
    #[must_use]
    pub fn reach_m(&self, camera: &Camera) -> f64 {
        match self.aerial {
            Some(a) => (self.fog_m + a.reach_per_altitude * a.climb_m(camera)).min(a.max_reach_m).max(self.fog_m),
            None => self.fog_m,
        }
    }

    /// The view depth, world units, the fog stays clear to at `camera`'s
    /// height ([`AerialFog::clear_per_altitude`]); 0 without one.
    #[must_use]
    pub fn clear_world(&self, camera: &Camera) -> f64 {
        let Some(a) = self.aerial else { return 0.0 };
        (a.clear_per_altitude * a.climb_m(camera)).min(a.max_clear_m.max(0.0)) / metres_per_world_unit(camera.center_lat)
    }

    /// This eye camera with a vertical FOV of `deg` degrees (held to
    /// `Camera::MIN_FOV_Y..=MAX_FOV_Y`).
    #[must_use]
    pub fn with_fov_deg(self, deg: f64) -> Self {
        Self { fov_y_deg: Camera::clamped_fov_y(deg.to_radians()).to_degrees(), ..self }
    }

    /// [`Self::fov_y_deg`] in radians, exactly the default when it is 60.
    #[must_use]
    pub fn fov_y_rad(&self) -> f64 {
        if (self.fov_y_deg - Camera::DEFAULT_FOV_Y.to_degrees()).abs() < 1e-12 {
            Camera::DEFAULT_FOV_Y
        } else {
            Camera::clamped_fov_y(self.fov_y_deg.to_radians())
        }
    }

    /// [`Self::reach_m`] (just [`Self::fog_m`] without an [`AerialFog`])
    /// in world units at `camera`'s latitude: the ground distance
    /// `cover_plan` cuts at.
    #[must_use]
    pub fn cutoff_world(&self, camera: &Camera) -> f64 {
        self.reach_m(camera) / metres_per_world_unit(camera.center_lat)
    }
}

/// Metres per world unit (`render::WORLD_SIZE` spans the world) at `lat`.
#[must_use]
pub fn metres_per_world_unit(lat: f64) -> f64 {
    crate::geo::EARTH_CIRCUMFERENCE_M * lat.to_radians().cos() / crate::geo::TILE_SIZE
}

/// **Where to put the camera for an eye** at `eye` (lon, lat),
/// `height_m` over the ground, facing `bearing` (radians, clockwise from
/// north) and pitched `pitch` (radians from straight down), in a view
/// `viewport_height_px` tall: the look-at point `height_m * tan(pitch)`
/// ahead along the bearing, and the zoom whose camera distance puts the
/// eye at that height. Hand it to `jump_to` on a map with an
/// [`EyeCamera`]; on any other the zoom and pitch are clamped, and the
/// eye sits higher.
#[must_use]
pub fn eye_target(eye: (f64, f64), height_m: f64, bearing: f32, pitch: f32, viewport_height_px: f32) -> CameraTarget {
    eye_target_fov(eye, height_m, bearing, pitch, viewport_height_px, Camera::DEFAULT_FOV_Y)
}

/// [`eye_target`] for a camera of vertical FOV `fov_y` radians (the
/// map's `Camera::fov_y`, which `MapView::set_eye_camera` sets from
/// [`EyeCamera::fov_y_deg`]): the camera distance is `H / (2 tan(fov/2)
/// 2^z)`, so the zoom that puts the eye at `height_m` depends on it.
#[must_use]
pub fn eye_target_fov(eye: (f64, f64), height_m: f64, bearing: f32, pitch: f32, viewport_height_px: f32, fov_y: f64) -> CameraTarget {
    let p = f64::from(pitch).clamp(0.0, f64::from(EYE_MAX_PITCH));
    let ahead_m = height_m * p.tan();
    let b = f64::from(bearing);
    let m_per_deg_lat = crate::geo::EARTH_CIRCUMFERENCE_M / 360.0;
    let m_per_deg_lon = m_per_deg_lat * eye.1.to_radians().cos();
    let center = (eye.0 + ahead_m * b.sin() / m_per_deg_lon, eye.1 + ahead_m * b.cos() / m_per_deg_lat);
    // distance() = H / (2 tan(fov/2) 2^z) world units must equal the
    // eye's slant range to the look-at point, h / cos(p) metres.
    let slant_world = height_m / p.cos() / metres_per_world_unit(center.1);
    let tan_half = (Camera::clamped_fov_y(fov_y) / 2.0).tan();
    let zoom = (f64::from(viewport_height_px) / (2.0 * tan_half * slant_world)).log2();
    CameraTarget { center: Some(center), zoom: Some(zoom), pitch: Some(p as f32), bearing: Some(bearing), anchor_offset_px: (0.0, 0.0) }
}

/// Zoom at and above which the globe allows the full [`MAX_PITCH`]. See
/// [`max_pitch_at_zoom`].
pub const PITCH_LIMIT_FULL_ZOOM: f64 = 5.0;

/// Zoom at and below which the globe is drawn face-on (pitch 0). See
/// [`max_pitch_at_zoom`].
pub const PITCH_LIMIT_FLAT_ZOOM: f64 = 2.5;

/// **The pitch the globe allows at `zoom`**: [`MAX_PITCH`] at and above
/// [`PITCH_LIMIT_FULL_ZOOM`] (z5), `0` at and below
/// [`PITCH_LIMIT_FLAT_ZOOM`] (z2.5), a smoothstep between. A non-finite zoom
/// gets the full range.
///
/// **Why.** The camera orbits a point on the surface, so a pitched camera
/// zoomed out to the whole planet still looks at that point from the side:
/// the globe's centre is drawn `R sin(pitch)` below it and the planet sits
/// in the bottom of the frame (the 2026-10-04 report: "zoom out to the globe
/// level, globe is not in the center of the screen", Tilt 60). Google Earth
/// eases the tilt off as you leave for space and gives it back as you come
/// down; this is that.
///
/// **Neither reference engine does it.** MapLibre GL JS's globe has one
/// `maxPitch` for every zoom (`transform_helper.ts:169`, `setPitch` clamps
/// to it at :352) and its vertical-perspective camera orbits the centre the
/// same way ours does, so a pitched MapLibre globe zoomed out to z1 is off
/// centre too. Mapbox GL JS's behaviour was checked from the outside only
/// (licence gate: no 3.x source read).
///
/// **The numbers.** z2.5 is where the whole planet is in the frame:
/// [`fit_shell_zoom`] puts the earth's disc exactly across the short side of
/// a 1000x750 window at z3.0 and of a 430x932 phone at z1.8, so by z2.5 a
/// desktop frame shows all of it and a pitch there only pushes it off
/// centre. z5 is two levels above the desktop fit: the planet overfills the
/// frame four times over, a pitched camera sees a horizon and not a ball,
/// and the space effects are mostly faded (`render::globe::GLOBE_ZOOM_MIN`
/// .. `GLOBE_ZOOM_MAX`, z3..z6). The smoothstep has zero slope at both ends,
/// so a zoom crossing either one has no kink in its pitch.
///
/// The camera keeps the user's pitch as an INTENT and draws
/// `min(intent, max_pitch_at_zoom(zoom))` -- see
/// [`CameraController::pitch_intent`] and
/// [`CameraController::set_low_zoom_pitch_limit`].
#[must_use]
pub fn max_pitch_at_zoom(zoom: f64) -> f32 {
    if !zoom.is_finite() {
        return MAX_PITCH;
    }
    let t = ((zoom - PITCH_LIMIT_FLAT_ZOOM) / (PITCH_LIMIT_FULL_ZOOM - PITCH_LIMIT_FLAT_ZOOM)).clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    if s >= 1.0 {
        return MAX_PITCH;
    }
    MAX_PITCH * s as f32
}

/// [`max_pitch_at_zoom`] for a globe camera at `zoom` over latitude
/// `center_lat`: the zoom the curve reads is the planet's SIZE on screen,
/// `zoom + log2(1 / cos(lat))` -- MapLibre's `normalizedGlobeZoom`
/// (`globe_utils.ts:374`, `tr.zoom + getZoomAdjustment(tr.center.lat, 0)`).
/// The globe's radius goes as `2^zoom / cos(lat)`, so z1 over the pole
/// draws the planet as large as z4.5 over the equator: overfilling the
/// frame, with room to tilt. It is also what a globe drag holds constant
/// (`set_center_keeping_globe_size`), so dragging never moves the pitch.
#[must_use]
pub fn max_pitch_on_globe(zoom: f64, center_lat: f64) -> f32 {
    let lat = if center_lat.is_finite() { center_lat } else { 0.0 };
    max_pitch_at_zoom(zoom + crate::render::globe::globe_size_zoom_adjustment(lat, 0.0))
}

/// How far one keyboard rotate step turns the map, in radians (15 degrees).
///
/// This is MapLibre/Mapbox GL JS's own keyboard step (`bearingStep = 15` in
/// `src/ui/handler/keyboard.ts`, bound to Shift+Left/Right there too), so
/// muscle memory carries over from the web maps this renderer is a native
/// counterpart to. It also divides the circle exactly 24 times, which is
/// what makes keyboard-only rotation land back on true north rather than
/// accumulating a residual the user cannot clear.
pub const KEYBOARD_ROTATE_STEP: f32 = std::f32::consts::TAU / 24.0;

/// How far one keyboard tilt step pitches the camera, in radians (10
/// degrees) -- MapLibre's `pitchStep`, bound to Shift+Up/Down there as well.
pub const KEYBOARD_PITCH_STEP: f32 = std::f32::consts::PI / 18.0;

/// A bearing within this many radians of north (7 degrees) snaps to exactly
/// north when a rotation gesture SETTLES.
///
/// MapLibre's `bearingSnap` map option defaults to the same 7 degrees, and
/// it exists because 0.4 degrees off north is visually indistinguishable
/// from north while still rotating every label and leaving a compass needle
/// off-zero: aiming for north with a drag essentially never hits it exactly.
///
/// Deliberately applied only at the END of a gesture, never continuously. A
/// snap that fires mid-drag makes the map fight the cursor inside the
/// window, which reads as the gesture breaking rather than as an assist.
pub const NORTH_SNAP_WINDOW: f32 = 7.0 * std::f32::consts::PI / 180.0;

/// Radians of bearing per pixel of a Shift+drag rotation (0.8 degrees/px),
/// and radians of pitch per pixel of a Shift+drag tilt (0.5 degrees/px).
///
/// Both are MapLibre's drag-rotate rates (`bearingDegreesPerPixelMoved` and
/// `pitchDegreesPerPixelMoved`). They replace the single `0.01` rad/px that
/// both axes used to share, which was wrong for at least one of them by
/// construction: pitch has only `MAX_PITCH` (60 degrees) of total travel
/// against bearing's full 6.28 rad circle, so one shared rate necessarily
/// saturates the tilt axis long before the rotate axis has gone anywhere.
pub const DRAG_BEARING_RADIANS_PER_PIXEL: f32 = 0.8 * std::f32::consts::PI / 180.0;
pub const DRAG_PITCH_RADIANS_PER_PIXEL: f32 = 0.5 * std::f32::consts::PI / 180.0;

/// Convergence rate (1/seconds) for the bearing/pitch ease, matching the
/// discrete-input zoom rate ([`ZOOM_EASE_RATE_DISCRETE`], 20.0) closely enough
/// that a keyboard rotate and a wheel-notch zoom read as the same machine.
/// ~90% converged in ~130ms.
pub const ORIENTATION_EASE_RATE: f32 = 18.0;

/// Wraps an accumulated turn into `(-PI, PI]`: the shortest way back.
fn wrap_turn(turn: f64) -> f64 {
    use std::f64::consts::{PI, TAU};
    if turn > -PI && turn <= PI {
        return turn;
    }
    -((PI - turn).rem_euclid(TAU) - PI)
}

/// Wraps a bearing into `[-PI, PI)`.
///
/// Every write to `camera.bearing` goes through this. Without it the field
/// simply accumulated (`self.camera.bearing += delta_bearing` was its only
/// mutation), so repeated rotation grew it without bound: the rendering math
/// is periodic and does not care, but an `f32` holding 5000 rad has ~5e-4
/// rad of representable precision left where a fresh one has ~1e-7, so a
/// long-lived map's rotation would visibly quantize. It also left every
/// derived question ("how far from north is this?", "which way round is
/// shorter?") unanswerable without wrapping first.
///
/// `[-PI, PI)` rather than `[0, TAU)` so the sign carries meaning and
/// distance-from-north is just `.abs()`; it is also the range MapLibre's own
/// `transform.bearing` keeps.
pub fn wrap_bearing(bearing: f32) -> f32 {
    (bearing + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

/// The signed shortest way round from `from` to `to`, in `[-PI, PI)`.
///
/// This is what keeps the orientation ease from taking the long way round:
/// easing from 179 degrees to -179 degrees is a 2-degree nudge, but the raw
/// difference is -358 degrees, and an ease driven by the raw difference
/// spins the map almost the whole way around to arrive somewhere next door.
pub fn shortest_bearing_delta(from: f32, to: f32) -> f32 {
    wrap_bearing(to - from)
}

/// Wraps a longitude into `[-180, 180)`.
///
/// The longitude twin of [`wrap_bearing`], and deliberately
/// the same shape -- `(x + half).rem_euclid(full) - half` -- in degrees.
/// Every write to `camera.center_lon`/`zoom_target.center_lon` goes through it.
/// Without it the field simply accumulated: the six assignment sites all took
/// whatever `tile_xy_to_lon_lat`/`center_anchored_at` handed back, so panning
/// east never stopped adding -- measured at 216.12 degrees after four 200px
/// drags at zoom 1.38 (see
/// `panning_east_past_the_antimeridian_wraps_instead_of_accumulating`), with
/// rendering entirely unbothered, since `lon_lat_to_tile_xy` is a periodic
/// map onto the tile grid and `visible_tiles` clamps what comes out of it.
/// The damage is to everyone who READS the camera back: a saved position, a
/// log line, or a comparison against a track coordinate. A consumer's track player
/// sits at lon 170.92 by design and had to carry this exact expression as a
/// workaround.
///
/// SIGNED, for the same reason `wrap_bearing` signs its range: a bare
/// `lon.rem_euclid(360.0)` is just as bounded but normalises into `[0, 360)`,
/// so it would persist Honolulu as 202.14 instead of -157.86 -- a hemisphere
/// off in any saved position, log line or comparison against a track
/// coordinate, and the opposite of what the consumer's own workaround
/// (`((lon + 180.0).rem_euclid(360.0)) - 180.0`) already assumes. MapLibre
/// signs it too (`LngLat.wrap()` -> `util.wrap(lng, -180, 180)`, documented
/// as 286.0251 -> -73.9749), though with the opposite endpoint: theirs is
/// `(-180, 180]` and maps -180 to +180, where this is `[-180, 180)` to match
/// `wrap_bearing`'s `[-PI, PI)` -- the antimeridian itself reads -180 here.
///
/// The in-range fast path is not an optimisation, it is exactness: the
/// expression is not the identity in f64 even where it should be
/// (`(103.85 + 180.0).rem_euclid(360.0) - 180.0` is 103.85000000000002,
/// 2.84e-14 degrees out -- 3 nanometres on the ground, but not the number
/// the caller passed in, and this file's camera tests compare centres with
/// `assert_eq!`). A wrap is a repair for a longitude that left the range,
/// not a rounding pass over every coordinate a caller hands in.
pub fn wrap_lon(lon: f64) -> f64 {
    if (-180.0..180.0).contains(&lon) {
        lon
    } else {
        (lon + 180.0).rem_euclid(360.0) - 180.0
    }
}

/// The signed shortest way round from `from` to `to`, in `[-180, 180)`.
///
/// `shortest_bearing_delta`'s longitude twin, and it exists for the same
/// failure: once both ends are wrapped, easing from 179.9 to -179.9 is a
/// 0.2-degree nudge east, but the raw subtraction says -359.8 and an ease
/// driven by that crawls the camera west across the whole globe to arrive
/// next door. Wrapping `center_lon` without this would have BOUGHT that bug.
pub fn shortest_lon_delta(from: f64, to: f64) -> f64 {
    wrap_lon(to - from)
}

/// The zoom at which a shell of `radius_m` spans `(1 - margin)` of the viewport's short side, at `lat_deg`.
/// It inverts spec §2.6's `R = 512/(2*pi*cos lat)`, `d = H/(2*tan 30*2^z)`. Clamped to
/// `SPACE_MIN_ZOOM..=MAX_ZOOM`; `None` for a non-finite input or a margin outside `[0, 1)`.
#[must_use]
pub fn fit_shell_zoom(radius_m: f64, margin: f32, lat_deg: f64, width_px: f32, height_px: f32) -> Option<f64> {
    if !(radius_m.is_finite() && radius_m > 0.0 && (0.0..1.0).contains(&margin) && width_px > 0.0 && height_px > 0.0) {
        return None;
    }
    let tan_half = (crate::render::FOV_Y_RADIANS_F64 / 2.0).tan();
    let half_short = f64::from(width_px.min(height_px)) * f64::from(1.0 - margin) / 2.0;
    let k = half_short * tan_half / (f64::from(height_px) / 2.0); // tan of the shell's angular radius
    let r = radius_m / crate::render::globe::GLOBE_RADIUS_M;
    let eye = r / k.atan().sin(); // radii from the centre
    let r_world = 512.0 / (std::f64::consts::TAU * lat_deg.to_radians().cos().max(1e-6));
    let d = (eye - 1.0) * r_world;
    let z = (f64::from(height_px) / (2.0 * tan_half * d)).log2();
    z.is_finite().then(|| z.clamp(SPACE_MIN_ZOOM, MAX_ZOOM))
}

/// `bearing`, wrapped, and collapsed to exactly north if it is within
/// `NORTH_SNAP_WINDOW` of it. See that constant for why.
pub fn bearing_after_north_snap(bearing: f32) -> f32 {
    let wrapped = wrap_bearing(bearing);
    if wrapped.abs() <= NORTH_SNAP_WINDOW {
        0.0
    } else {
        wrapped
    }
}

/// The flat-map "zoom about a point" solution, as `(lon, lat)`: the center a
/// camera must move to so that the world point `anchor` stays under the same
/// screen pixel while the world-to-screen scale grows by `scale_factor`
/// (`2^Δzoom`), starting from center `center_before`.
///
/// Both callers (`zoom_towards_point`, computing the ease target at the target
/// zoom, and `advance_zoom_animation`, recomputing the live center at the
/// zoom it eased to this frame) need exactly this formula, and they must agree
/// to the last bit or the ease would chase a target it can never reach. All
/// f64: see `zoom_towards_point` for why f32 world coordinates are not
/// remotely good enough here.
pub fn center_anchored_at(
    anchor: glam::DVec3,
    center_before: glam::DVec3,
    scale_factor: f64,
) -> (f64, f64) {
    let w = world_anchored_at(anchor, center_before, scale_factor);
    crate::geo::tile_xy_to_lon_lat(w.x / WORLD_SIZE as f64, w.y / WORLD_SIZE as f64, 0.0)
}

/// [`center_anchored_at`] in world units, before the lon/lat conversion:
/// the centre that keeps `anchor` fixed while the scale grows by
/// `scale_factor`. `z` is carried from `center_before`.
fn world_anchored_at(anchor: glam::DVec3, center_before: glam::DVec3, scale_factor: f64) -> glam::DVec3 {
    glam::DVec3::new(
        anchor.x + (center_before.x - anchor.x) / scale_factor,
        anchor.y + (center_before.y - anchor.y) / scale_factor,
        center_before.z,
    )
}

/// The camera centre `(lon, lat)` that puts the look-at point on world
/// point `world`, longitude wrapped, or `None` if either is not finite.
/// Every "move the centre to this world position" path converts through
/// here (`CameraController::shift_centre_by_world`, and `resolve`'s
/// anchor-offset solve), so the conversion, the wrap and the finiteness
/// guard exist once.
pub(crate) fn lon_lat_at_world(world: glam::DVec3) -> Option<(f64, f64)> {
    let size = f64::from(WORLD_SIZE);
    let (lon, lat) = crate::geo::tile_xy_to_lon_lat(world.x / size, world.y / size, 0.0);
    (lon.is_finite() && lat.is_finite()).then(|| (wrap_lon(lon), lat))
}

/// `true` when `camera`'s viewport is too small to unproject through (a
/// zero-size window during startup, or one dimension not yet laid out).
/// Every anchor-solving path (`resolve_pose`'s offset solve, a live rotate's
/// or pan's anchor, a click-to-zoom's anchor) checks this first, because
/// unprojecting through a degenerate viewport is meaningless.
fn viewport_is_degenerate(camera: &Camera) -> bool {
    !(camera.viewport_width_px >= 1.0 && camera.viewport_height_px >= 1.0)
}

/// The centre that draws `world` at `pixel` on `camera`'s frame: solved
/// on the globe, one exact step on the flat plane (translation
/// equivariance). `None` when there is none. `globe_enabled` is the
/// controller's `globe_enabled` flag (or the story path's own): whether
/// `camera` itself is drawn on the globe is `render::globe::globe_active`'s
/// call, made once here.
fn centre_placing(camera: &Camera, world: glam::DVec3, pixel: (f32, f32), globe_enabled: bool) -> Option<(f64, f64)> {
    if crate::render::globe::globe_active(camera, globe_enabled) {
        let (lon, lat) = crate::render::globe::center_placing_world_at(camera, world.truncate(), pixel)?;
        return Some((wrap_lon(lon), lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT)));
    }
    let shown = crate::render::unproject_to_ground_f64(camera, pixel.0, pixel.1)?;
    let (lon, lat) = lon_lat_at_world(camera.world_center_f64() + (world - shown))?;
    Some((lon, lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT)))
}

/// `CameraController::resolve` as a pure function: clamps, wraps, ignores
/// non-finite fields, solves `anchor_offset_px`. `current` stands in for
/// `self.camera`, `zoom_range` for `self.zoom_range`, and `globe_enabled` for
/// `self.globe_enabled`; a non-finite `target` field is dropped rather than
/// propagated -- see `CameraController::resolve`'s doc comment for why.
pub fn resolve_pose(current: &Camera, target: &CameraTarget, zoom_range: (f64, f64), globe_enabled: bool) -> Camera {
    resolve_pose_capped(current, target, zoom_range, globe_enabled, MAX_PITCH)
}

/// [`resolve_pose`] with the pitch held to `max_pitch` rather than
/// [`MAX_PITCH`]: an eye camera's ([`EyeCamera`]) controller resolves
/// through it.
pub fn resolve_pose_capped(current: &Camera, target: &CameraTarget, zoom_range: (f64, f64), globe_enabled: bool, max_pitch: f32) -> Camera {
    let mut end = *current;
    if let Some(zoom) = target.zoom.filter(|z| z.is_finite()) {
        end.zoom = zoom.clamp(zoom_range.0, zoom_range.1);
    }
    if let Some(pitch) = target.pitch.filter(|p| p.is_finite()) {
        end.pitch = pitch.clamp(0.0, max_pitch);
    }
    if let Some(bearing) = target.bearing.filter(|b| b.is_finite()) {
        end.bearing = wrap_bearing(bearing);
    }
    if let Some((lon, lat)) = target.center {
        if lon.is_finite() {
            end.center_lon = wrap_lon(lon);
        }
        if lat.is_finite() {
            end.center_lat = lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
        }
    }
    // `anchor_offset_px`: `center` must appear at `viewport centre +
    // offset`. With the centre at T, that pixel shows ground S; moving
    // the centre to 2T - S makes it show T. Translation equivariance
    // again, so one step is exact.
    let (ox, oy) = target.anchor_offset_px;
    let offset_usable = ox.is_finite() && oy.is_finite() && (ox != 0.0 || oy != 0.0);
    if target.center.is_some() && offset_usable && !viewport_is_degenerate(&end) {
        let at = (end.viewport_width_px / 2.0 + ox, end.viewport_height_px / 2.0 + oy);
        if crate::render::globe::globe_active(&end, globe_enabled) {
            // On the globe the centre is solved so `center` is DRAWN
            // at the offset pixel (MapLibre's `handleEaseTo`, which
            // calls `setLocationAtPoint` the same way). An unsolvable
            // offset (the offset pixel off a small planet) is dropped:
            // `center` lands at the viewport centre, and the flat
            // formula below is NOT a fallback for a globe frame.
            if let Some((lon, lat)) = centre_placing(&end, end.world_center_f64(), at, globe_enabled) {
                end.center_lon = lon;
                end.center_lat = lat;
            }
        } else if let Some(shown) = crate::render::unproject_to_ground_f64(&end, at.0, at.1) {
            let want = end.world_center_f64();
            if let Some((lon, lat)) = lon_lat_at_world(want + (want - shown)) {
                end.center_lon = lon;
                end.center_lat = lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
            }
        }
    }
    end
}

/// The world-space centre `ease_pose`'s curve is at eased progress `k`:
/// `from`'s and `to`'s world centres, interpolated the short way across the
/// antimeridian. Factored out of `ease_pose` so `CameraController`'s
/// `advance_motion` can ask the same question `ease_pose` asks internally,
/// to fall back to `self.camera`'s centre (rather than `ease_pose`'s own
/// fallback to `from`'s) on failure -- see `advance_motion`.
fn eased_world_center(from: &Camera, to: &Camera, k: f64) -> Option<(f64, f64)> {
    let world = f64::from(WORLD_SIZE);
    let (a, b) = (from.world_center_f64(), to.world_center_f64());
    let mut dx = b.x - a.x;
    if dx > world / 2.0 {
        dx -= world;
    } else if dx < -world / 2.0 {
        dx += world;
    }
    let at = glam::DVec3::new(a.x + dx * k, a.y + (b.y - a.y) * k, a.z);
    lon_lat_at_world(at)
}

/// `ease_to`'s arithmetic at eased progress `k` (already through the easing
/// curve): the centre interpolated in world space the shortest way across
/// the antimeridian, zoom and pitch linear, bearing the shortest way round.
/// Returns a camera with `from`'s viewport -- a pose has no viewport of its
/// own to interpolate. On `eased_world_center` failure (a non-finite world
/// point) it keeps `from`'s centre; `CameraController::advance_motion`
/// instead keeps `self.camera`'s (the last eased pose) to match its
/// pre-refactor behaviour exactly -- see that method.
pub fn ease_pose(from: &Camera, to: &Camera, k: f64) -> Camera {
    let (lon, lat) = eased_world_center(from, to, k).unwrap_or((from.center_lon, from.center_lat));
    let kf = k as f32;
    Camera {
        center_lon: lon,
        center_lat: lat,
        zoom: from.zoom + (to.zoom - from.zoom) * k,
        // Between two clamped poses; only the eye camera's ceiling bounds it.
        pitch: (from.pitch + (to.pitch - from.pitch) * kf).clamp(0.0, from.pitch.max(to.pitch).min(EYE_MAX_PITCH)),
        bearing: wrap_bearing(from.bearing + shortest_bearing_delta(from.bearing, to.bearing) * kf),
        viewport_width_px: from.viewport_width_px,
        viewport_height_px: from.viewport_height_px,
        fov_y: from.fov_y + (to.fov_y - from.fov_y) * k,
    }
}

/// Bearing change for a horizontal drag of `dx_px` logical pixels: MapLibre's
/// `rotateSpeed = 0.8` degrees per pixel (`src/ui/handler/mouse.ts`,
/// `generateMouseRotationHandler`), positive for a drag to the right. One
/// function so that native Shift+drag, the browser's right-drag and any
/// later shell convert with the same number.
pub fn rotate_radians_for_drag_px(dx_px: f32) -> f32 {
    dx_px * DRAG_BEARING_RADIANS_PER_PIXEL
}

/// Pitch change for a vertical drag of `dy_px` logical pixels (y down):
/// MapLibre's `pitchSpeed = -0.5` degrees per pixel
/// (`generateMousePitchHandler`) and its two-finger touch pitch
/// (`degreesPerPixelMoved = -0.5`, `TwoFingersTouchPitchHandler._move`).
/// NEGATED so that dragging UP tilts into 3D; see
/// `dragging_up_tilts_into_3d_rather_than_doing_nothing` for why.
pub fn tilt_radians_for_drag_px(dy_px: f32) -> f32 {
    -dy_px * DRAG_PITCH_RADIANS_PER_PIXEL
}

/// Radians of pitch per pixel of a Shift+two-finger trackpad scroll (0.25
/// degrees/px): half the drag rate, because a trackpad swipe reports far
/// more travel than the fingers cover (macOS accelerates it), and at the
/// drag rate one ordinary swipe slammed from flat to `MAX_PITCH`. At this
/// rate the whole 60-degree range is a ~240px swipe -- one deliberate
/// stroke, as in a drag.
pub const SCROLL_PITCH_RADIANS_PER_PIXEL: f32 = 0.25 * std::f32::consts::PI / 180.0;

/// Radians of bearing per pixel of a horizontal Shift+two-finger scroll (0.3
/// degrees/px) -- the rate Option+scroll already rotates at (30 degrees per
/// 100px), so the two rotate gestures feel like one machine.
pub const SCROLL_BEARING_RADIANS_PER_PIXEL: f32 = 0.3 * std::f32::consts::PI / 180.0;

/// Travel, in scroll pixels, a Shift+scroll gesture covers before it commits
/// to tilting OR rotating. The scroll counterpart of the Shift+drag axis
/// lock: a two-finger swipe is never perfectly vertical, and a tilt that
/// also wobbled the bearing would read as the map slipping.
pub const SCROLL_AXIS_LOCK_PX: f32 = 6.0;

/// Which modifier-bound orientation gestures a desktop map answers. All on
/// by default; a host that binds one of these chords itself turns that one
/// off with `MapView::set_orientation_gestures`. Touch gestures (two-finger
/// twist and tilt on iOS and the browser) are not modifier chords and are
/// not gated here.
///
/// The binding table (MapLibre GL JS's where it has one):
///
/// | gesture                   | what it does                         |
/// |---------------------------|--------------------------------------|
/// | Shift + scroll            | vertical tilts, horizontal rotates   |
/// | Option + scroll           | rotates                              |
/// | right-drag                | vertical tilts, horizontal rotates   |
/// | Ctrl + drag               | the same, for a one-button trackpad  |
/// | Shift + drag              | tilts or rotates, one axis at a time |
///
/// Shift, not Ctrl, for the scroll: Ctrl+scroll is ZOOM in two places
/// this engine runs -- a browser reports a trackpad pinch as a wheel event
/// with `ctrlKey` set (MapLibre's `scroll_zoom` relies on exactly that),
/// and macOS's Accessibility zoom takes Ctrl+scroll system-wide when it is
/// switched on. Shift collides with nothing: it is already this map's
/// orientation modifier for drags and arrow keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrientationGestures {
    /// Shift + two-finger scroll (or Shift + wheel): tilt and rotate.
    pub shift_scroll: bool,
    /// Option + scroll: rotate.
    pub option_scroll: bool,
    /// Right-button drag: tilt and rotate together. Off, a right press is
    /// only ever a click (the nav destination, when nav input is on).
    pub right_drag: bool,
    /// Ctrl + left drag: tilt and rotate together. Off, a Ctrl+drag pans.
    /// On macOS gpui reports a Ctrl+click as a RIGHT press (the platform
    /// convention), so there a Ctrl+drag is a right-drag and follows
    /// `right_drag` instead; this flag governs the other platforms.
    pub ctrl_drag: bool,
    /// Shift + left drag: tilt or rotate, axis-locked. Off, it pans.
    pub shift_drag: bool,
}

impl Default for OrientationGestures {
    fn default() -> Self {
        Self { shift_scroll: true, option_scroll: true, right_drag: true, ctrl_drag: true, shift_drag: true }
    }
}

impl OrientationGestures {
    /// Every orientation chord off: drags pan, scrolls zoom.
    pub const NONE: Self =
        Self { shift_scroll: false, option_scroll: false, right_drag: false, ctrl_drag: false, shift_drag: false };
}

/// One Shift+scroll event's travel, as the platform reported it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollTravel {
    /// A trackpad (or Magic Mouse): continuous pixels.
    Pixels { dx: f32, dy: f32 },
    /// A wheel: discrete notches.
    Lines { dx: f32, dy: f32 },
}

/// The axis an in-progress Shift+scroll committed to. Reset by the caller
/// when a new gesture starts (`TouchPhase::Started`, or a pause).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScrollAxisLock {
    travel_x: f32,
    travel_y: f32,
    tilt: Option<bool>,
}

/// What one Shift+scroll event asks of the camera, or `None` while the
/// gesture is still inside its axis dead zone.
///
/// Signs follow the content, as a drag's do: on macOS (natural scrolling,
/// the default) two fingers moving UP report a negative `dy`, and they tilt
/// INTO 3D exactly as dragging up does; fingers moving right rotate the way
/// dragging right does. A wheel notch goes through the eased `TiltStep`, one
/// `KEYBOARD_PITCH_STEP` per line -- and macOS, holding Shift, reports a
/// plain wheel's turn on the HORIZONTAL axis, so a wheel with no vertical
/// travel tilts by its horizontal one: Shift+wheel always tilts.
#[must_use]
pub fn shift_scroll_intent(travel: ScrollTravel, lock: &mut ScrollAxisLock) -> Option<CameraIntent> {
    match travel {
        ScrollTravel::Lines { dx, dy } => {
            let lines = if dy != 0.0 { dy } else { dx };
            (lines != 0.0 && lines.is_finite()).then(|| CameraIntent::TiltStep { radians: -lines * KEYBOARD_PITCH_STEP })
        }
        ScrollTravel::Pixels { dx, dy } => {
            if !(dx.is_finite() && dy.is_finite()) {
                return None;
            }
            lock.travel_x += dx.abs();
            lock.travel_y += dy.abs();
            if lock.tilt.is_none() && lock.travel_x.max(lock.travel_y) >= SCROLL_AXIS_LOCK_PX {
                lock.tilt = Some(lock.travel_y >= lock.travel_x);
            }
            match lock.tilt? {
                true if dy != 0.0 => Some(CameraIntent::TiltBy { radians: -dy * SCROLL_PITCH_RADIANS_PER_PIXEL }),
                false if dx != 0.0 => {
                    Some(CameraIntent::RotateBy { radians: dx * SCROLL_BEARING_RADIANS_PER_PIXEL, anchor_px: None })
                }
                _ => None,
            }
        }
    }
}

/// One move of a right-drag or Ctrl+drag orbit: horizontal turns, vertical
/// tilts, both at once -- MapLibre's `DragRotateHandler` and
/// `DragPitchHandler`, which both answer the same button. Rotate first, as
/// the touch shells order it.
#[must_use]
pub fn orbit_drag_intents(dx_px: f32, dy_px: f32) -> Vec<CameraIntent> {
    let mut out = Vec::with_capacity(2);
    let rotate = rotate_radians_for_drag_px(dx_px);
    if rotate != 0.0 && rotate.is_finite() {
        out.push(CameraIntent::RotateBy { radians: rotate, anchor_px: None });
    }
    let tilt = tilt_radians_for_drag_px(dy_px);
    if tilt != 0.0 && tilt.is_finite() {
        out.push(CameraIntent::TiltBy { radians: tilt });
    }
    out
}

/// The intents one two-finger update asks for, in the order every touch
/// shell applies them: pan, rotate, zoom, tilt -- all about the same sample,
/// so a hand doing several at once does not fight itself. Shared by iOS and
/// the browser so the translation from gesture to camera is written once.
#[must_use]
pub fn two_finger_intents(step: &crate::touch_gesture::TwoFingerStep) -> Vec<CameraIntent> {
    let mut out = Vec::with_capacity(4);
    if step.pan_px != (0.0, 0.0) {
        out.push(CameraIntent::PanBy { dx_px: step.pan_px.0, dy_px: step.pan_px.1, anchor_px: step.pan_anchor_px });
    }
    if step.rotate_radians != 0.0 {
        out.push(CameraIntent::RotateBy { radians: step.rotate_radians, anchor_px: Some(step.anchor_px) });
    }
    if step.zoom_levels != 0.0 && step.zoom_levels.is_finite() {
        out.push(CameraIntent::PinchZoomBy { levels: step.zoom_levels, anchor_px: step.anchor_px });
    }
    if step.tilt_px != 0.0 {
        out.push(CameraIntent::TiltBy { radians: tilt_radians_for_drag_px(step.tilt_px) });
    }
    out
}

/// `zoom_towards_point`'s rate for trackpad-sourced (`immediate`) input:
/// ~90% converged in ~26 ms. See `zoom_towards_point`'s doc comment.
pub const ZOOM_EASE_RATE_IMMEDIATE: f64 = 60.0;
/// The rate for discrete wheel notches and double-click/double-tap: ~90% in
/// ~115 ms.
pub const ZOOM_EASE_RATE_DISCRETE: f64 = 20.0;
/// The longest frame step the eases integrate at once, applied inside
/// [`CameraController::advance`] so that no shell can forget it.
///
/// Replaces `MapView::paint`'s `dt.min(Duration::from_millis(100))`: a
/// multi-second gap (window occluded, app backgrounded, an iPhone left idle)
/// must not replay the curve in one jump. 50 ms rather than paint's old
/// 100 ms, so the first frame after idle moves an ease by no more than about
/// three 60 Hz frames' worth.
pub const MAX_ADVANCE_STEP: Duration = Duration::from_millis(50);

/// How much more ground one pixel may cover at a grab point than at the
/// look-at point (`pitch_scale_factor_at_ground`) before a pan stops trusting
/// the unprojection there and moves at the clamp row's rate instead.
///
/// The failure it exists for is the spec's first risk: a ray just below the
/// horizon unprojects to ground arbitrarily far away, so a pan that followed
/// it would fling the map a city's width per pixel. At 4.0 the whole viewport
/// is inside the limit up to ~55 degrees of pitch (the clamp does nothing
/// there). Above that, the rows it clamps cover ~20-30 centre-pixels of
/// ground per pixel, where unclamped rows cover hundreds (a vertical drag in
/// the band at 60 degrees moves 31.7 per pixel, measured while writing
/// `a_drag_near_the_horizon_is_clamped_to_the_grab_scale_limit`).
///
/// It clamps the RATE, not the row (ruling F2): a grab point above the clamp
/// row is extrapolated from the clamp row at that row's ground-per-pixel
/// rate. Pulling the point down onto the clamp row instead would put both
/// ends of a drag that starts and ends in the band on the same row, and the
/// drag would move nothing. See `CameraController::grab_point`.
pub const GRAB_MAX_PITCH_SCALE: f64 = 4.0;

/// Fastest rotation a release may throw, radians/second: MapLibre's
/// `defaultBearingInertiaOptions.maxSpeed = 360` degrees/second
/// (`src/ui/handler_inertia.ts`).
pub const ROTATE_INERTIA_MAX_SPEED: f32 = std::f32::consts::TAU;
/// How far below the latitude clamp a globe drag starts turning the
/// planet as a dial about the pole ([`CameraController::polar_dial_longitude`]):
/// MapLibre's 12 degrees (`fixedBearingLongitude`, `globe_utils.ts`).
const POLAR_DIAL_BAND_DEG: f64 = 12.0;

/// Inside this many pixels of the pole's screen point the dial's sweep is
/// damped to nothing instead of running away: MapLibre's
/// `DIAL_MIN_RADIUS_PIXELS`.
const POLAR_DIAL_MIN_RADIUS_PX: f64 = 20.0;

/// The shortest lever a drag pinned on the latitude limit turns the view
/// by ([`CameraController::turn_at_the_latitude_limit`]): the view turns at
/// most one radian per this many pixels of drag.
const POLAR_TURN_MIN_LEVER_PX: f64 = 80.0;

/// The most latitude a globe drag's exact solve may move the centre per
/// 10 px of drag (or 1.5 times the planet's turn, if more) before the grab is
/// let slip
/// ([`CameraController::pan_on_globe`]).
const POLAR_STEP_MAX_LAT_DEG: f64 = 1.5;

/// How near the latitude limit a centre must already be for a drag to pin
/// it there ([`CameraController::pan_on_globe`]'s pinned path).
const POLAR_PIN_BAND_DEG: f64 = 1.0;

/// The most a single gesture may turn the view while pinned on the latitude
/// limit, either way: 45 degrees (review ruling). Reset by `GestureBegan`.
const POLAR_TURN_MAX_TWIST: f64 = std::f64::consts::FRAC_PI_4;

/// How long the view takes to give back, after release, the bearing a drag
/// added by turning on the latitude limit (user ruling: the globe eases
/// back to north-up after a pole drag). See [`PoleUnwind`].
///
/// MapLibre has no counterpart to unwind (its globe drag never turns the
/// bearing; at the pole it just stops), so the shape follows its nearest
/// relative, `snapToNorth` -> `rotateTo` after a drag-rotate: an eased
/// return to north, here shortened from its 1000 ms to fit the 300-500 ms
/// the ruling asked for.
pub const POLE_TURN_UNWIND_DURATION: Duration = Duration::from_millis(450);

/// Largest total turn one throw may add: MapLibre clamps the inertial
/// bearing to `clamp(result.amount, -179, 179)` (same file). Under half a
/// turn, so the ease's shortest-way crossing always turns the way it was
/// thrown.
pub const ROTATE_INERTIA_MAX_THROW: f32 = 179.0 * std::f32::consts::PI / 180.0;
/// Samples older than this are dropped: `map_view`'s `INERTIA_BUFFER_CUTOFF`.
pub const ROTATE_SAMPLE_CUTOFF_MS: f64 = 160.0;
/// Release velocity is measured across this window: `map_view`'s
/// `INERTIA_VELOCITY_WINDOW`.
pub const ROTATE_VELOCITY_WINDOW_MS: f64 = 60.0;
/// A velocity window shorter than this measures nothing and throws nothing.
/// Touch shells get one event per finger, handled back to back and stamped
/// microseconds apart, so a still hand's last tiny twist would otherwise
/// divide a few milliradians by microseconds and throw the maximum spin.
/// Half a ProMotion frame (120 Hz is 8.33 ms): far above one frame's
/// event spread, and below a real two-frame flick, which must still throw.
pub const MIN_ROTATE_SAMPLE_SPAN_MS: f64 = 4.0;

/// `ease_to`'s curve: MapLibre's `defaultEasing = bezier(0.25, 0.1, 0.25, 1)`
/// (`src/util/util.ts:384`), evaluated by the style-spec solver `map_view`'s
/// pan inertia already uses.
///
/// `pub(crate)`, like `Interpolation` itself: this is an intra-crate seam
/// (`ease_pose`, the story camera path), not public API -- see
/// `story::script::Easing::curve`'s doc comment for the same call.
pub(crate) const EASE_TO_EASING: crate::style_spec::expression::Interpolation =
    crate::style_spec::expression::Interpolation::CubicBezier { x1: 0.25, y1: 0.1, x2: 0.25, y2: 1.0 };

/// The recent history of a live rotation, for its release velocity. Shells
/// push `(timestamp, bearing delta)` beside every `RotateBy` they send, with
/// the timestamp from their own clock: the controller has none.
#[derive(Debug, Clone, Default)]
pub struct RotateSamples {
    samples: std::collections::VecDeque<(f64, f32)>,
}

impl RotateSamples {
    pub fn push(&mut self, at_ms: f64, radians: f32) {
        self.samples.push_back((at_ms, radians));
        while self.samples.front().is_some_and(|&(t, _)| at_ms - t > ROTATE_SAMPLE_CUTOFF_MS) {
            self.samples.pop_front();
        }
    }

    /// Radians per second over the samples inside [`ROTATE_VELOCITY_WINDOW_MS`]
    /// of `released_at_ms`. The first sample in the window only marks the
    /// start time: its own delta happened before it. `0.0` when fewer than
    /// two samples remain, which is what a finger that stopped before
    /// lifting leaves behind, and when they span less than
    /// [`MIN_ROTATE_SAMPLE_SPAN_MS`].
    #[must_use]
    pub fn velocity(&self, released_at_ms: f64) -> f32 {
        let recent: Vec<(f64, f32)> = self
            .samples
            .iter()
            .copied()
            .filter(|&(t, _)| released_at_ms - t <= ROTATE_VELOCITY_WINDOW_MS)
            .collect();
        let (Some(first), Some(last)) = (recent.first(), recent.last()) else { return 0.0 };
        let span_ms = last.0 - first.0;
        if recent.len() < 2 || !(span_ms >= MIN_ROTATE_SAMPLE_SPAN_MS) {
            return 0.0;
        }
        let turned: f32 = recent.iter().skip(1).map(|&(_, r)| r).sum();
        turned / (span_ms / 1000.0) as f32
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

/// A programmatic ease in flight: `from` and `to` are full poses, and
/// `elapsed` accumulates `advance`'s `dt`, clamped to [`MAX_ADVANCE_STEP`]
/// like every other ease (ruling F6): native measures `dt` from the last
/// frame it drew, so an `ease_to` started on an idle map would otherwise
/// finish on its first frame.
#[derive(Debug, Clone, PartialEq)]
struct CameraMotion {
    from: Camera,
    to: Camera,
    elapsed: Duration,
    duration: Duration,
    /// `Some` for a [`CameraController::fly_to`]: the centre and zoom follow
    /// the story's van Wijk path (`story::fly`) instead of the straight
    /// lines `ease_pose` draws. A `to` whose zoom the range moves mid-flight
    /// still lands exactly (the difference is eased in linearly); one whose
    /// centre moves (the plain map's bounds) drops back to an ease.
    flight: Option<crate::story::fly::FlyPlan>,
}

/// The release-time return of the bearing the latitude-limit turn added
/// ([`CameraController::turn_at_the_latitude_limit`]). It gives back
/// `from` radians over [`POLE_TURN_UNWIND_DURATION`] on a quadratic
/// ease-out (`1 - (1 - t)^2`): fastest at release, where the drag was
/// still turning the view, and arriving with zero speed. It is RELATIVE --
/// each frame shifts `camera.bearing` and the orientation target by the
/// frame's share -- so a bearing the user set (Shift-drag, Option-scroll,
/// keys, a rotation throw) rides along untouched and only the pole turn is
/// taken out.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PoleUnwind {
    from: f64,
    elapsed: Duration,
}

/// One thing a shell's input asks the camera to do. Shells translate their
/// platform events into these and nothing else; the rules live here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CameraIntent {
    /// A live drag. `anchor_px` is the pointer position BEFORE this move, in
    /// logical viewport pixels: the grabbed point that must stay under the
    /// finger.
    PanBy { dx_px: f32, dy_px: f32, anchor_px: (f32, f32) },
    /// Zoom by `levels` about `anchor_px`. `immediate` selects
    /// [`ZOOM_EASE_RATE_IMMEDIATE`] (continuous input) over
    /// [`ZOOM_EASE_RATE_DISCRETE`].
    ZoomAbout { levels: f64, anchor_px: (f32, f32), immediate: bool },
    /// A live pinch: zoom by `levels` about `anchor_px`, applied AT ONCE.
    /// Not eased, because touch shells deliver several pinch updates per
    /// frame (one per finger) and an eased zoom rebased on the rendered
    /// zoom would keep only the last of them; and because the next update's
    /// pan would drop an eased zoom's anchor. MapLibre applies touch zoom
    /// per frame too. Native wheel and trackpad keep `ZoomAbout`.
    PinchZoomBy { levels: f64, anchor_px: (f32, f32) },
    /// A live rotation, applied at once. `anchor_px: Some(p)` keeps the
    /// ground under `p` in place (a two-finger twist turns about the
    /// fingers). `None` turns about the screen centre.
    RotateBy { radians: f32, anchor_px: Option<(f32, f32)> },
    /// A live tilt, applied at once.
    TiltBy { radians: f32 },
    /// A discrete rotation (key, wheel notch), eased.
    RotateStep { radians: f32 },
    /// A discrete tilt, eased.
    TiltStep { radians: f32 },
    /// A finger or button went down.
    GestureBegan,
    /// The last finger or button came up. The north snap and rotation
    /// inertia happen here. `rotate_velocity` is in radians per second of
    /// bearing, from `RotateSamples::velocity`.
    GestureEnded { rotate_velocity: f32 },
    ResetNorth,
    ResetNorthAndPitch,
}

/// A programmatic camera move. Every field is optional; `None` keeps the
/// current value.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CameraTarget {
    pub center: Option<(f64, f64)>,
    pub zoom: Option<f64>,
    /// Radians.
    pub pitch: Option<f32>,
    /// Radians.
    pub bearing: Option<f32>,
    /// Where `center` lands on screen, as an offset from the viewport centre
    /// in logical pixels. A follow camera puts the puck low: `(0.0, h / 4.0)`.
    /// Solved for [`CameraController::jump_to`] and
    /// [`CameraController::ease_to`] by unprojecting through the
    /// camera; `Camera` itself gets no padding field.
    pub anchor_offset_px: (f32, f32),
}

/// Where the orientation ease is steering `camera.bearing`/`camera.pitch`.
/// `MapView`'s old `target_bearing`/`target_pitch`; see
/// [`CameraController::advance_orientation_animation`].
///
/// Keyboard rotation, reset-to-north, wheel-notch rotation and the
/// post-gesture north snap steer these, and the camera eases toward them
/// instead of jumping. Continuous, direct-manipulation input (Shift+drag,
/// trackpad Option+scroll) deliberately does NOT go through the ease: it
/// writes `camera` immediately and collapses these onto it, because a
/// gesture tracking the hand must not lag it. That is the same split the
/// zoom path makes via `ZoomTarget::ease_rate`.
///
/// `bearing` is kept wrapped to `[-PI, PI)` like `camera.bearing`; the ease
/// crosses between them with `shortest_bearing_delta` so it never takes the
/// long way round.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrientationTarget {
    bearing: f32,
    pitch: f32,
}

/// Where the zoom ease is steering zoom and centre: the fields that were
/// `MapView`'s own before the controller existed, now `zoom`,
/// `center_lon`/`center_lat`, `ease_rate` and `anchor` here.
///
/// `camera` itself is what's drawn each frame; it eases toward this (see
/// `advance_zoom_animation`) rather than jumping to it, which is what made
/// scroll-wheel zoom feel like a series of snaps instead of a smooth motion.
/// Panning stays immediate: only zoom, and the centre adjustment that keeps
/// zoom anchored under the cursor, is eased.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ZoomTarget {
    zoom: f64,
    center_lon: f64,
    center_lat: f64,
    /// Convergence rate (1/seconds) `advance_zoom_animation` uses. Set
    /// per-call by `zoom_towards_point`: [`ZOOM_EASE_RATE_IMMEDIATE`] for
    /// trackpad-sourced input, [`ZOOM_EASE_RATE_DISCRETE`] for wheel notches
    /// and double-click. See `zoom_towards_point`'s doc comment for why
    /// trackpad input needs a fast-but-finite rate instead of an instant
    /// snap.
    ease_rate: f64,
    /// World-space ground point the in-flight zoom must hold visually fixed:
    /// the point that was under the cursor when the zoom input arrived.
    /// `Some` while an anchored zoom is easing; `None` for a plain
    /// centre-anchored zoom (cursor ray missed the ground) or after a pan,
    /// which overrides the centre outright. `advance_zoom_animation` derives
    /// the centre from the zoom it actually eased to each frame, instead of
    /// interpolating it independently -- see that method for why an
    /// independent lerp leaks a small, always-outward drift.
    anchor: Option<glam::DVec3>,
    /// The pixel `anchor` is held under. The flat arithmetic never needs
    /// it (zoom scales the ground about the look-at point), the globe does:
    /// there the centre is SOLVED so the anchor is drawn at this pixel
    /// (`render::globe::center_placing_world_at`).
    anchor_px: (f32, f32),
}

/// The one writer of pitch, bearing, zoom and centre.
#[derive(Debug, Clone)]
pub struct CameraController {
    camera: Camera,
    target: OrientationTarget,
    zoom_target: ZoomTarget,
    zoom_range: (f64, f64),
    /// Whether a `RotateBy` has landed since the last `GestureBegan` or
    /// `GestureEnded`, which is when the north snap applies: native only
    /// ever snapped a drag that actually committed to rotating (see
    /// `NORTH_SNAP_WINDOW`). `GestureBegan` clearing it is load-bearing: a
    /// rotate whose end never arrived (an Option+trackpad rotate whose
    /// `TouchPhase::Ended` went elsewhere) must not make the NEXT gesture's
    /// release snap.
    rotated_in_gesture: bool,
    /// Radians the view has turned this gesture while pinned on the
    /// latitude limit ([`Self::turn_at_the_latitude_limit`]), held under
    /// [`POLAR_TURN_MAX_TWIST`]. Reset by `GestureBegan` and `GestureEnded`.
    pinned_twist: f64,
    /// Radians of bearing the latitude-limit turn has added and not yet
    /// given back: the drag's own turn, plus any its inertial glide adds.
    /// Unlike `pinned_twist` it survives the release -- it is what
    /// [`PoleUnwind`] eases back out. A deliberate absolute bearing
    /// (`ResetNorth`, a `jump_to`/`ease_to` that sets one) zeroes it:
    /// that bearing is the user's, not the pole's.
    pole_turn: f64,
    /// Whether the last gesture has ended, so `pole_turn` may be given
    /// back: set by `GestureEnded`, cleared by `GestureBegan` (a grab
    /// freezes the return where it stands; the next release resumes it).
    pole_unwind_armed: bool,
    /// The return in flight. `None` while held: before release, and on any
    /// frame after a `PanBy` (an inertial glide is still pushing the view,
    /// and may still be turning it), so the return starts once the glide
    /// ends and runs its whole curve over whatever the glide left.
    pole_unwind: Option<PoleUnwind>,
    /// A `PanBy` has landed since the last `advance`.
    panned_since_advance: bool,
    /// A globe drag's zoom before the zoom-range clamp, and the zoom it
    /// applied: while the camera still shows that applied zoom, the next
    /// drag step adds its size adjustment to the UNCLAMPED one, so a drag
    /// that meets the floor going north comes back to its start zoom going
    /// south instead of ratcheting (review). Any other zoom change leaves
    /// the camera at a different zoom, and the next drag starts afresh.
    globe_pan_zoom: Option<(f64, f64)>,
    /// The `ease_to` in flight, if any. Any `apply` cancels it where it
    /// stands, except `GestureEnded`: a release moves nothing, so a motion
    /// still here at release was started DURING the press (a press-time
    /// `FeatureEvent::Click` handler) and is left to finish -- see `apply`.
    motion: Option<CameraMotion>,
    /// How many camera-moving intents `apply` has taken. Every `apply` is a
    /// person moving the map -- programmatic moves are `jump_to`/`ease_to`
    /// -- so a follow camera that sees this change knows to let go. See
    /// [`CameraController::input_generation`].
    input_generation: u64,
    /// How many of those intents were a zoom (`ZoomAbout`, `PinchZoomBy`)
    /// and nothing else, and the levels they asked for, summed. See
    /// [`CameraController::zoom_input`].
    zoom_input_generation: u64,
    zoom_input_levels: f64,
    /// See [`Self::set_globe_enabled`].
    globe_enabled: bool,
    /// The user's pitch while the low-zoom limit holds the camera below it
    /// ([`max_pitch_at_zoom`]); `None` whenever `camera.pitch` IS the
    /// user's pitch. Read through [`Self::pitch_intent`], written through
    /// `set_pitch_intent`. An `Option` rather than a second copy of the
    /// pitch so that a camera whose pitch was written directly (the tests'
    /// `camera_mut`) still has an intent equal to what it draws.
    pitch_intent: Option<f32>,
    /// See [`Self::set_low_zoom_pitch_limit`].
    low_zoom_pitch_limit: bool,
    /// A switch to Plain is easing the camera onto [`plain_constrain`]'s
    /// pose ([`Self::set_globe_enabled`]): the per-step constraint waits
    /// for that ease rather than snapping to its end.
    plain_settling: bool,
    /// Whether [`Self::constrain_plain`] holds a Plain camera inside the
    /// world. Always `true` outside this module's tests, which turn it off
    /// to exercise the flat arithmetic past the antimeridian that the
    /// globe's flat regime (z12 and up) still relies on.
    plain_bounds: bool,
    /// See [`Self::set_eye_limits`].
    eye_limits: Option<(f32, f64)>,
}

/// How long a switch to Plain takes to ease a camera that shows more than
/// the world (a globe zoomed out to the planet) onto the covering pose.
pub const PLAIN_SETTLE_DURATION: Duration = Duration::from_millis(350);

/// **The Plain (flat Mercator) camera constraint**: MapLibre's
/// `MercatorTransform.defaultConstrain` with `renderWorldCopies: false`
/// (`src/geo/projection/mercator_transform.ts`), the world's longitude
/// range `-180..180` and latitude range the Mercator square's. A viewport
/// `width_px` x `height_px` (logical) never shows past the world's edge:
/// the zoom rises until the world (`TILE_SIZE * 2^zoom` px square) covers
/// both sides, then the centre is pulled in until no band shows above,
/// below or beside it. Returns `(lon, lat, zoom)`, unchanged when the
/// pose already fits (and for a degenerate viewport).
#[must_use]
pub fn plain_constrain(lon: f64, lat: f64, zoom: f64, width_px: f64, height_px: f64) -> (f64, f64, f64) {
    if !(width_px >= 1.0 && height_px >= 1.0) || !lon.is_finite() || !lat.is_finite() || !zoom.is_finite() {
        return (lon, lat, zoom);
    }
    let zoom = zoom.max(plain_cover_zoom(width_px, height_px));
    let world = crate::geo::TILE_SIZE * 2f64.powf(zoom);
    let (fx, fy) = crate::geo::lon_lat_to_tile_xy(lon, lat, 0.0);
    // The Mercator square's own top and bottom (`lon_lat_to_tile_xy`
    // clamps latitude to +-85.05, a hair inside the square's edge).
    let (_, top) = crate::geo::lon_lat_to_tile_xy(0.0, 90.0, 0.0);
    let (_, bottom) = crate::geo::lon_lat_to_tile_xy(0.0, -90.0, 0.0);
    let (min_y, max_y) = (top * world, bottom * world);
    let (x, y) = (fx * world, fy * world);
    let (w2, h2) = (width_px / 2.0, height_px / 2.0);
    let clamp_axis = |v: f64, lo: f64, hi: f64, half: f64| if hi - lo <= 2.0 * half { 0.5 * (lo + hi) } else { v.clamp(lo + half, hi - half) };
    let nx = clamp_axis(x, 0.0, world, w2);
    let ny = clamp_axis(y, min_y, max_y, h2);
    if nx == x && ny == y {
        return (lon, lat, zoom);
    }
    let (nlon, nlat) = crate::geo::tile_xy_to_lon_lat(nx / world, ny / world, 0.0);
    (if nx == x { lon } else { nlon }, if ny == y { lat } else { nlat }, zoom)
}

/// The lowest zoom at which the Plain world covers a `width_px` x
/// `height_px` viewport on both axes ([`plain_constrain`]'s floor).
#[must_use]
pub fn plain_cover_zoom(width_px: f64, height_px: f64) -> f64 {
    let (_, top) = crate::geo::lon_lat_to_tile_xy(0.0, 90.0, 0.0);
    let (_, bottom) = crate::geo::lon_lat_to_tile_xy(0.0, -90.0, 0.0);
    let world_h = bottom - top;
    let need = (width_px / crate::geo::TILE_SIZE).max(height_px / (crate::geo::TILE_SIZE * world_h));
    need.max(f64::MIN_POSITIVE).log2()
}

impl CameraController {
    /// `min_zoom..=max_zoom` is this shell's zoom range. Every shell (native,
    /// iOS, web) passes this module's own [`MIN_ZOOM`]/[`MAX_ZOOM`] -- see
    /// their doc comments for why those two numbers, not a shell-picked
    /// pair, are what every `CameraController::new` call site should reach
    /// for.
    #[must_use]
    pub fn new(camera: Camera, min_zoom: f64, max_zoom: f64) -> Self {
        let camera = Camera {
            center_lon: wrap_lon(camera.center_lon),
            bearing: wrap_bearing(camera.bearing),
            ..camera
        };
        Self {
            camera,
            target: OrientationTarget { bearing: camera.bearing, pitch: camera.pitch },
            zoom_target: ZoomTarget {
                zoom: camera.zoom,
                center_lon: camera.center_lon,
                center_lat: camera.center_lat,
                ease_rate: ZOOM_EASE_RATE_DISCRETE,
                anchor: None,
                anchor_px: (0.0, 0.0),
            },
            zoom_range: (min_zoom, max_zoom),
            rotated_in_gesture: false,
            pinned_twist: 0.0,
            pole_turn: 0.0,
            pole_unwind_armed: false,
            pole_unwind: None,
            panned_since_advance: false,
            globe_pan_zoom: None,
            motion: None,
            input_generation: 0,
            zoom_input_generation: 0,
            zoom_input_levels: 0.0,
            globe_enabled: true,
            pitch_intent: None,
            low_zoom_pitch_limit: true,
            plain_settling: false,
            plain_bounds: true,
            eye_limits: None,
        }
        .with_pitch_limited()
    }

    fn with_pitch_limited(mut self) -> Self {
        self.limit_pitch();
        self.target.pitch = self.pitch_intent();
        self
    }

    /// Whether a globe zoomed out past z5 eases its pitch off
    /// ([`max_pitch_at_zoom`]), so the planet is drawn face-on and centred
    /// by z2.5, the user's pitch coming back as they zoom in. On by
    /// default. A host whose camera is authored rather than steered (a
    /// story preview, which must match the render that has no such limit)
    /// turns it off; switching re-applies it at once. No effect with the
    /// globe off: a flat map has no planet to centre.
    pub fn set_low_zoom_pitch_limit(&mut self, enabled: bool) {
        self.low_zoom_pitch_limit = enabled;
        self.limit_pitch();
    }

    /// See [`Self::set_low_zoom_pitch_limit`].
    #[must_use]
    pub fn low_zoom_pitch_limit(&self) -> bool {
        self.low_zoom_pitch_limit
    }

    /// The pitch the user chose, in radians: what [`Self::camera`] draws
    /// once the zoom allows it. Equal to `camera().pitch` except while the
    /// low-zoom limit holds the camera below it ([`max_pitch_at_zoom`]).
    #[must_use]
    pub fn pitch_intent(&self) -> f32 {
        self.pitch_intent.unwrap_or(self.camera.pitch)
    }

    /// The highest pitch this controller draws at `zoom` over `center_lat`.
    fn pitch_ceiling(&self, zoom: f64, center_lat: f64) -> f32 {
        if self.low_zoom_pitch_limit && self.globe_enabled {
            let globe = max_pitch_on_globe(zoom, center_lat);
            // The eye camera's ceiling wherever the globe allows its full
            // pitch; the globe's own limit below that.
            if globe >= MAX_PITCH {
                self.max_pitch()
            } else {
                globe
            }
        } else {
            self.max_pitch()
        }
    }

    /// The steepest pitch this controller draws: [`MAX_PITCH`], or the eye
    /// camera's ([`Self::set_eye_limits`]).
    #[must_use]
    pub fn max_pitch(&self) -> f32 {
        self.eye_limits.map_or(MAX_PITCH, |(p, _)| p)
    }

    /// The highest zoom any range may reach: [`MAX_ZOOM`], or the eye
    /// camera's ([`Self::set_eye_limits`]).
    #[must_use]
    pub fn zoom_ceiling(&self) -> f64 {
        self.eye_limits.map_or(MAX_ZOOM, |(_, z)| z)
    }

    /// **The eye camera's limits**: `Some((max_pitch, max_zoom))` lets the
    /// camera tilt to `max_pitch` (at most [`EYE_MAX_PITCH`], at least
    /// [`MAX_PITCH`]) and zoom to `max_zoom` (at most [`EYE_MAX_ZOOM`]) --
    /// an eye a metre over the road. The zoom RANGE is widened to the new
    /// ceiling. `None` restores the defaults and brings the camera back
    /// inside them at once.
    /// The camera's vertical field of view, radians (`Camera::fov_y`):
    /// the camera now and any motion in flight take it at once, so every
    /// pose this controller resolves, eases or reports carries it.
    /// [`Camera::DEFAULT_FOV_Y`] restores the shared default.
    pub fn set_fov_y(&mut self, fov_y: f64) {
        let fov_y = Camera::clamped_fov_y(fov_y);
        self.camera.fov_y = fov_y;
        if let Some(motion) = &mut self.motion {
            motion.from.fov_y = fov_y;
            motion.to.fov_y = fov_y;
        }
    }

    pub fn set_eye_limits(&mut self, limits: Option<(f32, f64)>) {
        let limits = limits
            .filter(|(p, z)| p.is_finite() && z.is_finite())
            .map(|(p, z)| (p.clamp(MAX_PITCH, EYE_MAX_PITCH), z.clamp(MAX_ZOOM, EYE_MAX_ZOOM)));
        let was_ceiling = self.zoom_ceiling();
        self.eye_limits = limits;
        let ceiling = self.zoom_ceiling();
        let hi = if (self.zoom_range.1 - was_ceiling).abs() < 1e-9 || self.zoom_range.1 > ceiling { ceiling } else { self.zoom_range.1 };
        self.zoom_range = (self.zoom_range.0.min(hi), hi);
        if self.camera.zoom > hi {
            self.camera.zoom = hi;
            self.zoom_target.zoom = hi;
        }
        let max_pitch = self.max_pitch();
        if let Some(motion) = &mut self.motion {
            motion.to.zoom = motion.to.zoom.min(hi);
            motion.to.pitch = motion.to.pitch.min(max_pitch);
        }
        self.target.pitch = self.target.pitch.min(self.max_pitch());
        if let Some(intent) = self.pitch_intent {
            self.pitch_intent = Some(intent.min(self.max_pitch()));
        }
        self.camera.pitch = self.camera.pitch.min(self.max_pitch());
        self.limit_pitch();
    }

    /// Makes `intent` the user's pitch and draws as much of it as the
    /// current zoom allows. A non-finite intent keeps the current one.
    fn set_pitch_intent(&mut self, intent: f32) {
        let intent = if intent.is_finite() { intent.clamp(0.0, self.max_pitch()) } else { self.pitch_intent() };
        let ceiling = self.pitch_ceiling(self.camera.zoom, self.camera.center_lat);
        if intent > ceiling {
            self.camera.pitch = ceiling;
            self.pitch_intent = Some(intent);
        } else {
            self.camera.pitch = intent;
            self.pitch_intent = None;
        }
    }

    /// Re-draws the user's pitch at the current zoom. Every zoom write
    /// calls it BEFORE it solves a centre, so an anchored zoom holds its
    /// point under the pitch actually drawn. A globe drag's own zoom
    /// adjustment does not: it keeps the planet's size, which is what the
    /// ceiling reads ([`max_pitch_on_globe`]).
    fn limit_pitch(&mut self) {
        self.set_pitch_intent(self.pitch_intent());
    }

    /// `target` resolved with its pitch replaced by what the camera will
    /// DRAW at the target zoom (an `anchor_offset_px` solve must see the
    /// drawn pitch), and the user's pitch that comes from.
    fn resolve_with_intent(&self, target: &CameraTarget) -> (Camera, f32) {
        let intent = target.pitch.filter(|p| p.is_finite()).map_or_else(|| self.pitch_intent(), |p| p.clamp(0.0, self.max_pitch()));
        let zoom = target.zoom.filter(|z| z.is_finite()).map_or(self.camera.zoom, |z| z.clamp(self.zoom_range.0, self.zoom_range.1));
        let lat = target.center.map(|c| c.1).filter(|l| l.is_finite()).unwrap_or(self.camera.center_lat);
        let drawn = intent.min(self.pitch_ceiling(zoom, lat));
        (self.resolve(&CameraTarget { pitch: Some(drawn), ..*target }), intent)
    }

    /// Narrows (or restores) the zoom range every input and every
    /// `jump_to` / `ease_to` clamps to: a map whose data stops at a zoom
    /// (the race dots' z3-14, Walker's `minZoom`/`maxZoom`) holds the camera
    /// there, a wheel past the end doing nothing rather than overshooting and
    /// bouncing back. The range is intersected with `SPACE_MIN_ZOOM..=MAX_ZOOM`;
    /// the default range a controller is built with is `MIN_ZOOM..=MAX_ZOOM`.
    /// A non-finite or inverted pair is ignored. A camera (or an ease in
    /// flight) outside the new range EASES onto it -- `advance` then
    /// returns `true` until it arrives.
    pub fn set_zoom_range(&mut self, min_zoom: f64, max_zoom: f64) {
        self.globe_pan_zoom = None;
        if !min_zoom.is_finite() || !max_zoom.is_finite() || min_zoom > max_zoom {
            return;
        }
        let (lo, hi) = (min_zoom.max(SPACE_MIN_ZOOM), max_zoom.min(self.zoom_ceiling()));
        if lo > hi {
            return;
        }
        self.zoom_range = (lo, hi);
        if let Some(motion) = &mut self.motion {
            motion.to.zoom = motion.to.zoom.clamp(lo, hi);
        }
        if self.camera.zoom < lo || self.camera.zoom > hi {
            // Outside: ease onto the range's nearer end, about the centre.
            self.zoom_target.zoom = self.camera.zoom.clamp(lo, hi);
            self.zoom_target.center_lon = self.camera.center_lon;
            self.zoom_target.center_lat = self.camera.center_lat;
            self.zoom_target.anchor = None;
            self.zoom_target.ease_rate = ZOOM_EASE_RATE_DISCRETE;
        } else {
            // Inside, with a wheel ease in flight past an end (review F9):
            // it goes on to the end, anchor and all, rather than stopping.
            self.zoom_target.zoom = self.zoom_target.zoom.clamp(lo, hi);
        }
        self.constrain_plain();
    }

    /// The zoom range inputs clamp to ([`Self::set_zoom_range`]).
    #[must_use]
    pub fn zoom_range(&self) -> (f64, f64) {
        self.zoom_range
    }

    /// Whether the frames this camera drives are drawn on the globe below
    /// z12 -- `MapView::set_globe_enabled`'s switch, forwarded here so
    /// that every cursor-anchored gesture (drag, wheel and pinch zoom,
    /// anchored rotation, an offset ease) holds its point where it is
    /// DRAWN. `true` by default, as on every map.
    ///
    /// Switching to Plain with a camera that shows past the world's edge
    /// (the planet seen whole from z1) EASES onto [`plain_constrain`]'s pose
    /// over [`PLAIN_SETTLE_DURATION`]: the flat world zooms in to fill the
    /// view rather than snapping. `advance` returns `true` until it lands.
    pub fn set_globe_enabled(&mut self, enabled: bool) {
        let was = self.globe_enabled;
        self.globe_enabled = enabled;
        self.limit_pitch();
        self.plain_settling = false;
        if was && !enabled {
            let c = self.camera;
            let (lon, lat, zoom) = plain_constrain(c.center_lon, c.center_lat, c.zoom, f64::from(c.viewport_width_px), f64::from(c.viewport_height_px));
            if (lon, lat, zoom) != (c.center_lon, c.center_lat, c.zoom) {
                self.plain_settling = true;
                self.ease_to(CameraTarget { center: Some((lon, lat)), zoom: Some(zoom), ..CameraTarget::default() }, PLAIN_SETTLE_DURATION);
                self.plain_settling = self.motion.is_some();
            }
        }
        self.constrain_plain();
    }

    /// Holds a Plain camera inside the world ([`plain_constrain`]): the
    /// camera, the zoom ease's target and an ease's destination alike, so
    /// no ease chases a pose the constraint keeps taking back. Run after
    /// every input, jump and frame. Nothing on the globe (the space around
    /// the planet is the view), and nothing while the switch's own settle
    /// ease runs.
    fn constrain_plain(&mut self) {
        if self.plain_settling && self.motion.is_none() {
            self.plain_settling = false;
        }
        if self.globe_enabled || self.plain_settling || !self.plain_bounds || self.viewport_is_degenerate() {
            return;
        }
        let (w, h) = (f64::from(self.camera.viewport_width_px), f64::from(self.camera.viewport_height_px));
        let c = self.camera;
        let (lon, lat, zoom) = plain_constrain(c.center_lon, c.center_lat, c.zoom, w, h);
        if (lon, lat, zoom) != (c.center_lon, c.center_lat, c.zoom) {
            self.camera.center_lon = lon;
            self.camera.center_lat = lat;
            if zoom != c.zoom {
                self.camera.zoom = zoom;
                self.limit_pitch();
            }
        }
        let t = self.zoom_target;
        let (tlon, tlat, tzoom) = plain_constrain(t.center_lon, t.center_lat, t.zoom, w, h);
        if (tlon, tlat, tzoom) != (t.center_lon, t.center_lat, t.zoom) {
            // An anchored zoom held at the edge cannot keep its anchor (the
            // anchor's centre is past the edge): it eases to the clamped
            // pose instead, which converges.
            self.zoom_target.anchor = None;
        }
        self.zoom_target.center_lon = tlon;
        self.zoom_target.center_lat = tlat;
        self.zoom_target.zoom = tzoom;
        if let Some(motion) = &mut self.motion {
            let to = motion.to;
            let (mlon, mlat, mzoom) = plain_constrain(to.center_lon, to.center_lat, to.zoom, w, h);
            motion.to.center_lon = mlon;
            motion.to.center_lat = mlat;
            motion.to.zoom = mzoom;
        }
    }

    /// Whether `camera`'s frame is on the globe, for this controller.
    fn globe_at(&self, camera: &Camera) -> bool {
        crate::render::globe::globe_active(camera, self.globe_enabled)
    }

    /// The world point DRAWN under screen point `p` on the current frame,
    /// globe included (`render::globe::unproject_to_world_on_globe`);
    /// `None` off the planet or above the horizon.
    fn pick(&self, p: (f32, f32)) -> Option<glam::DVec3> {
        crate::render::globe::unproject_to_world_on_globe(&self.camera, self.globe_enabled, p.0, p.1)
            .filter(|w| w.is_finite())
            .map(|w| w.extend(0.0))
    }

    /// The centre that draws `world` at `pixel` on `camera`'s frame: solved
    /// on the globe, one exact step on the flat plane (translation
    /// equivariance). `None` when there is none.
    fn centre_placing(&self, camera: &Camera, world: glam::DVec3, pixel: (f32, f32)) -> Option<(f64, f64)> {
        centre_placing(camera, world, pixel, self.globe_enabled)
    }

    /// Bumped by every [`Self::apply`] that can move the camera -- every
    /// intent but `GestureBegan` and `GestureEnded`, which a plain tap sends
    /// (a release that throws a rotation was already counted by its
    /// `RotateBy`). `nav::follow::FollowCamera` compares it frame to frame:
    /// a change means a pan, pinch, rotate, tilt, key or compass tap, and the
    /// follow camera switches to free.
    #[must_use]
    pub fn input_generation(&self) -> u64 {
        self.input_generation
    }

    /// The zoom-only share of [`Self::input_generation`]: how many of the
    /// counted intents were a wheel, trackpad or pinch zoom
    /// (`ZoomAbout`, `PinchZoomBy`), and the zoom levels they asked for,
    /// summed (requested, before any clamp to the zoom range). A follow
    /// camera that sees `input_generation` move by exactly as much as this
    /// count knows the user only zoomed -- and keeps following, taking the
    /// levels as its own zoom offset (`nav::follow::FollowCamera`).
    #[must_use]
    pub fn zoom_input(&self) -> (u64, f64) {
        (self.zoom_input_generation, self.zoom_input_levels)
    }

    /// The camera as it is RIGHT NOW, mid-ease included.
    #[must_use]
    pub fn camera(&self) -> &Camera {
        &self.camera
    }

    /// Where the camera is going: an ease's end while one runs, else the
    /// camera now. A window that draws no frames (covered, on another
    /// Space, a locked screen) never advances an ease, so a caller that
    /// reads the view right after asking for a move reads this.
    pub fn destination(&self) -> Camera {
        self.motion.as_ref().map_or(self.camera, |m| m.to)
    }

    /// Logical viewport size. The only camera field a shell sets directly,
    /// because it is a fact about the window, not a camera move.
    /// In Plain a viewport grown past the world re-applies
    /// [`plain_constrain`].
    pub fn set_viewport(&mut self, width_px: f32, height_px: f32) {
        self.camera.viewport_width_px = width_px;
        self.camera.viewport_height_px = height_px;
        self.constrain_plain();
    }

    /// `true` when nothing is easing, so a shell may stop asking for frames.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.motion.is_none()
            && self.zoom_target.anchor.is_none()
            && self.zoom_target.zoom == self.camera.zoom
            && self.zoom_target.center_lon == self.camera.center_lon
            && self.zoom_target.center_lat == self.camera.center_lat
            && self.target.bearing == self.camera.bearing
            && self.target.pitch == self.pitch_intent()
            && !self.pole_unwind_pending()
    }

    /// Whether a released pole turn is still to be given back.
    fn pole_unwind_pending(&self) -> bool {
        self.pole_unwind_armed && self.pole_turn != 0.0
    }

    /// Forgets the pole turn: the bearing is now the user's own.
    fn forget_pole_turn(&mut self) {
        self.pole_turn = 0.0;
        self.pole_unwind = None;
        self.pole_unwind_armed = false;
    }

    pub fn apply(&mut self, intent: CameraIntent) {
        self.apply_unconstrained(intent);
        self.constrain_plain();
    }

    fn apply_unconstrained(&mut self, intent: CameraIntent) {
        // Any intent (a live gesture, a key, a reset) cancels a programmatic
        // move where it stands. `advance_motion` collapses the eases'
        // targets onto the camera every frame, so after this nothing chases
        // the old destination.
        //
        // Except a RELEASE. `GestureBegan` already stopped whatever was
        // moving when the button went down, so a motion still here at
        // `GestureEnded` was started DURING the press -- by a host's
        // `FeatureEvent::Click` handler, which fires on the press. A release
        // moves nothing, and cancelling here killed every click-to-zoom a
        // few milliseconds after it began (the gallery's big-points "click
        // a cluster does nothing"). A press that became a drag already
        // cancelled it with its own `PanBy`.
        if !matches!(intent, CameraIntent::GestureEnded { .. }) {
            self.motion = None;
        }
        if !matches!(intent, CameraIntent::GestureBegan | CameraIntent::GestureEnded { .. }) {
            self.input_generation = self.input_generation.wrapping_add(1);
        }
        // Every other input may write the zoom (a pinch, a wheel, a tilt
        // that re-solves, a new gesture): the drag's unclamped zoom
        // ([`Self::globe_pan_zoom`]) is only good between drag steps. The
        // release keeps it, so the fling's steps carry it on.
        if !matches!(intent, CameraIntent::PanBy { .. } | CameraIntent::GestureEnded { .. }) {
            self.globe_pan_zoom = None;
        }
        if let CameraIntent::ZoomAbout { levels, .. } | CameraIntent::PinchZoomBy { levels, .. } = intent {
            self.zoom_input_generation = self.zoom_input_generation.wrapping_add(1);
            if levels.is_finite() {
                self.zoom_input_levels += levels;
            }
        }
        match intent {
            CameraIntent::PanBy { dx_px, dy_px, anchor_px } => {
                // A non-finite drag or anchor moves nothing: one NaN
                // through the solves would poison the camera for good.
                if dx_px.is_finite() && dy_px.is_finite() && anchor_px.0.is_finite() && anchor_px.1.is_finite() {
                    self.panned_since_advance = true;
                    self.pan_by_pixels(dx_px, dy_px, anchor_px);
                }
            }
            CameraIntent::ZoomAbout { levels, anchor_px, immediate } => {
                self.zoom_towards_point(levels, anchor_px.0, anchor_px.1, immediate);
            }
            CameraIntent::PinchZoomBy { levels, anchor_px } => self.pinch_zoom_now(levels, anchor_px),
            CameraIntent::RotateBy { radians, anchor_px } => {
                self.rotated_in_gesture = true;
                self.rotate_now(radians, anchor_px);
            }
            CameraIntent::TiltBy { radians } => self.tilt_by(radians, 0.0),
            CameraIntent::RotateStep { radians } => self.rotate_target_by(radians),
            CameraIntent::TiltStep { radians } => self.pitch_target_by(radians),
            CameraIntent::GestureBegan => {
                self.rotated_in_gesture = false;
                self.pinned_twist = 0.0;
                self.pole_unwind_armed = false;
                self.pole_unwind = None;
                // Grabbing the map stops it dead, the way catching a spinning
                // wheel does: an inertial throw (or a keyboard step still
                // gliding) must not keep turning the map under a new gesture.
                self.target = OrientationTarget { bearing: self.camera.bearing, pitch: self.pitch_intent() };
            }
            CameraIntent::GestureEnded { rotate_velocity } => {
                self.pinned_twist = 0.0;
                // A release with no press since the last one (a stray end,
                // a duplicate from a second finger) must not restart the
                // curve already running.
                if !self.pole_unwind_armed {
                    // Several quick grabs can stack turns past half a
                    // revolution; given back unwrapped, 270 degrees would
                    // spin the long way round instead of 90 the short way.
                    self.pole_turn = wrap_turn(self.pole_turn);
                    self.pole_unwind = None;
                }
                self.pole_unwind_armed = self.pole_turn != 0.0;
                if std::mem::take(&mut self.rotated_in_gesture) {
                    // Inertia as a projected TARGET for the orientation ease:
                    // an exponential that starts at velocity `v` with rate
                    // `k` covers `v / k` in total, and its initial speed
                    // `k * (v / k)` equals the release speed, so the spin
                    // carries on without a jolt and decays on exactly the
                    // ease's curve, stopping at its epsilon. The north snap
                    // (moved from `map_view.rs`'s mouse-up handler) applies
                    // to where the throw lands, and is written to the
                    // TARGET, so the last few degrees glide.
                    let v = if rotate_velocity.is_finite() {
                        rotate_velocity.clamp(-ROTATE_INERTIA_MAX_SPEED, ROTATE_INERTIA_MAX_SPEED)
                    } else {
                        0.0
                    };
                    let throw = (v / ORIENTATION_EASE_RATE).clamp(-ROTATE_INERTIA_MAX_THROW, ROTATE_INERTIA_MAX_THROW);
                    // The snap judges the bearing the view will settle on
                    // once the pole turn is given back, not today's: a
                    // user-set 3 degrees under a 40 degree pole turn
                    // snaps; the turn is then added back for the
                    // unwind to remove.
                    let pole = self.pole_turn as f32;
                    self.target.bearing = wrap_bearing(bearing_after_north_snap(self.camera.bearing + throw - pole) + pole);
                }
            }
            CameraIntent::ResetNorth => {
                self.forget_pole_turn();
                self.reset_north();
            }
            CameraIntent::ResetNorthAndPitch => {
                self.forget_pole_turn();
                self.reset_north_and_pitch();
            }
        }
    }

    /// Puts the camera at `target` at once, leaving nothing chasing it.
    /// `MapView::set_camera`'s reset rules, moved: `zoom` clamps to this
    /// controller's range, `center` longitude wraps, the zoom ease and its
    /// anchor are collapsed onto the new pose, and an orientation target is
    /// collapsed only for the axes `target` sets. Cancels any `ease_to`.
    pub fn jump_to(&mut self, target: CameraTarget) {
        self.globe_pan_zoom = None;
        self.motion = None;
        let (end, intent) = self.resolve_with_intent(&target);
        self.camera.center_lon = end.center_lon;
        self.camera.center_lat = end.center_lat;
        self.camera.zoom = end.zoom;
        self.set_pitch_intent(intent);
        self.camera.bearing = end.bearing;
        self.zoom_target.zoom = end.zoom;
        self.zoom_target.center_lon = end.center_lon;
        self.zoom_target.center_lat = end.center_lat;
        self.zoom_target.anchor = None;
        if target.bearing.is_some() {
            self.target.bearing = end.bearing;
            self.forget_pole_turn();
        }
        if target.pitch.is_some() {
            self.target.pitch = intent;
        }
        self.constrain_plain();
    }

    /// Advances every ease by `dt`, clamped to [`MAX_ADVANCE_STEP`], and
    /// returns whether anything is still moving. A shell asks for another
    /// frame ONLY while this is `true`.
    pub fn advance(&mut self, dt: Duration) -> bool {
        let moving = self.advance_unconstrained(dt);
        self.constrain_plain();
        moving
    }

    fn advance_unconstrained(&mut self, dt: Duration) -> bool {
        let step = dt.min(MAX_ADVANCE_STEP);
        let panned = std::mem::take(&mut self.panned_since_advance);
        if self.motion.is_some() {
            return self.advance_motion(step) || self.pole_unwind_pending();
        }
        let unwinding = self.advance_pole_unwind(step, panned);
        let zooming = self.advance_zoom_animation(step);
        let orienting = self.advance_orientation_animation(step);
        zooming || orienting || unwinding
    }

    /// Gives back the released pole turn ([`PoleUnwind`]); `true` while
    /// any is left. `panned`: a `PanBy` landed since the last frame, so an
    /// inertial glide is still running -- the return waits for it, and then
    /// starts afresh from whatever is left.
    fn advance_pole_unwind(&mut self, dt: Duration, panned: bool) -> bool {
        if !self.pole_unwind_pending() {
            return false;
        }
        if panned {
            self.pole_unwind = None;
            return true;
        }
        let mut unwind = self.pole_unwind.unwrap_or(PoleUnwind { from: self.pole_turn, elapsed: Duration::ZERO });
        unwind.elapsed = (unwind.elapsed + dt).min(POLE_TURN_UNWIND_DURATION);
        let t = unwind.elapsed.as_secs_f64() / POLE_TURN_UNWIND_DURATION.as_secs_f64();
        let left = if t >= 1.0 { 0.0 } else { unwind.from * (1.0 - t) * (1.0 - t) };
        let give_back = (self.pole_turn - left) as f32;
        self.camera.bearing = wrap_bearing(self.camera.bearing - give_back);
        self.target.bearing = wrap_bearing(self.target.bearing - give_back);
        if left == 0.0 {
            // The last frame gives back exactly what is left, so nothing of
            // the turn outlives the curve.
            self.forget_pole_turn();
            return false;
        }
        self.pole_turn = left;
        self.pole_unwind = Some(unwind);
        true
    }

    /// The pose `target` asks for, clamped and wrapped. A NON-FINITE field
    /// is ignored -- it keeps the current value, like `None` -- because
    /// `clamp`, `wrap_bearing` and `wrap_lon` all pass NaN through, and one
    /// NaN in an ease target leaves `advance` returning `true` forever.
    fn resolve(&self, target: &CameraTarget) -> Camera {
        resolve_pose_capped(&self.camera, target, self.zoom_range, self.globe_enabled, self.max_pitch())
    }

    /// A live rotation about `anchor_px` (or the centre). Measures the slide
    /// instead of deriving it: unproject the anchor before and after the
    /// turn and translate the centre by the difference. Translation
    /// equivariance makes that exact under any pitch, in one step. Replaces
    /// iOS's `rotate_by` plus `center_after_rotating_about`.
    fn rotate_now(&mut self, radians: f32, anchor_px: Option<(f32, f32)>) {
        let anchor = anchor_px.filter(|_| !self.viewport_is_degenerate());
        if self.globe_at(&self.camera) {
            // On the globe the slide is not a translation: hold the drawn
            // point under the anchor by solving for the centre.
            let before = anchor.and_then(|p| self.pick(p));
            self.tilt_by(0.0, radians);
            if let (Some(p), Some(before)) = (anchor, before) {
                if let Some((lon, lat)) = self.centre_placing(&self.camera, before, p) {
                    self.set_center_now(lon, lat);
                }
            }
            return;
        }
        let before = anchor.and_then(|p| crate::render::unproject_to_ground_f64(&self.camera, p.0, p.1));
        self.tilt_by(0.0, radians);
        let (Some(p), Some(before)) = (anchor, before) else { return };
        let Some(after) = crate::render::unproject_to_ground_f64(&self.camera, p.0, p.1) else { return };
        self.shift_centre_by_world(before - after);
    }

    /// A live zoom about `anchor_px`, at once (`CameraIntent::PinchZoomBy`).
    /// The flat "zoom about a point" centre (`world_anchored_at`, the
    /// arithmetic behind [`center_anchored_at`]), which holds under pitch too:
    /// zooming scales the whole view about the look-at point, so the ground
    /// under any pixel scales about the centre by the same factor. The zoom
    /// ease's target is collapsed onto the result, zoom and centre, and its
    /// anchor dropped, so `advance` has nothing to chase.
    fn pinch_zoom_now(&mut self, levels: f64, anchor_px: (f32, f32)) {
        if !levels.is_finite() {
            return;
        }
        let old_zoom = self.camera.zoom;
        let new_zoom = (old_zoom + levels).clamp(self.zoom_range.0, self.zoom_range.1);
        if new_zoom == old_zoom {
            return;
        }
        let globe_before = self.globe_at(&self.camera);
        let anchor = if self.viewport_is_degenerate() {
            None
        } else if globe_before {
            self.pick(anchor_px)
        } else {
            crate::render::unproject_to_ground_f64(&self.camera, anchor_px.0, anchor_px.1).filter(|g| g.is_finite())
        };
        let center_before = self.camera.world_center_f64();
        self.camera.zoom = new_zoom;
        self.limit_pitch();
        self.zoom_target.zoom = new_zoom;
        self.zoom_target.anchor = None;
        if globe_before || self.globe_at(&self.camera) {
            // A globe frame on either side: solve for the centre that
            // draws the pinched point back under the fingers.
            if let Some((lon, lat)) = anchor.and_then(|a| self.centre_placing(&self.camera, a, anchor_px)) {
                self.set_center_now(lon, lat);
                return;
            }
            self.zoom_target.center_lon = self.camera.center_lon;
            self.zoom_target.center_lat = self.camera.center_lat;
            return;
        }
        let scale_factor = 2f64.powf(new_zoom - old_zoom);
        if let Some(anchor) = anchor.filter(|_| scale_factor > 0.0 && scale_factor.is_finite()) {
            // Moves the centre and lines the zoom target's centre up with it.
            if self.shift_centre_by_world(world_anchored_at(anchor, center_before, scale_factor) - center_before) {
                return;
            }
        }
        // No ground under the fingers: a centre-anchored zoom. The target
        // centre must still agree with the camera's.
        self.zoom_target.center_lon = self.camera.center_lon;
        self.zoom_target.center_lat = self.camera.center_lat;
    }

    /// Moves to `target` over `duration` on MapLibre's default ease-to curve.
    /// Zoom and pitch interpolate linearly, bearing the shortest way round,
    /// and the centre in world space the shortest way across the
    /// antimeridian. Any `apply` cancels it where it stands, except a
    /// `GestureEnded` that ends the same press this was started on -- see
    /// `motion`. A zero duration is a `jump_to`, and so is one longer than
    /// [`MAX_EASE_DURATION`].
    pub fn ease_to(&mut self, target: CameraTarget, duration: Duration) {
        self.globe_pan_zoom = None;
        if duration.is_zero() || duration > MAX_EASE_DURATION {
            self.jump_to(target);
            return;
        }
        // The motion eases the user's pitch, not the drawn one: each frame
        // draws what that frame's zoom allows of it (`set_pose`), so an
        // ease out to the globe holds its tilt down to z5 and lets it go on
        // `max_pitch_at_zoom`'s curve, not on a straight line to zero.
        let (to, intent) = self.resolve_with_intent(&target);
        let to = Camera { pitch: intent, ..to };
        let from = Camera { pitch: self.pitch_intent(), ..self.camera };
        if target.bearing.is_some_and(f32::is_finite) {
            self.forget_pole_turn();
        }
        self.zoom_target.anchor = None;
        self.motion = Some(CameraMotion { from, to, elapsed: Duration::ZERO, duration, flight: None });
        self.constrain_plain();
    }

    /// Flies to `target`: MapLibre's `flyTo`, the van Wijk and Nuij path
    /// the story player already flies (`story::fly`, flat and globe forms),
    /// rising off the ground over a long move and coming down at the far
    /// end. Pitch and bearing ease as in [`Self::ease_to`], whose rules (any
    /// `apply` cancels it, a zero or over-long duration jumps) all hold.
    ///
    /// `duration: None` is MapLibre's own: the path's length over its
    /// default speed, or `easeTo`'s 500 ms when centre and zoom already
    /// match. The path rises no higher than this controller's zoom floor.
    pub fn fly_to(&mut self, target: CameraTarget, duration: Option<Duration>) {
        let spec = crate::story::fly::FlySpec { min_zoom: Some(self.zoom_range.0), ..Default::default() };
        let plan = |from: Camera, to: Camera, globe: bool| {
            use crate::story::camera_path::Pose;
            crate::story::fly::plan(
                Pose { camera: from, globe },
                Pose { camera: to, globe },
                (from.viewport_width_px, from.viewport_height_px),
                &spec,
            )
            .ok()
        };
        // Planned twice: from the resolved target for the duration
        // `ease_to` needs up front, then from the endpoints `ease_to` built
        // (constrained to the plain map's bounds), which the motion runs.
        let duration = duration.unwrap_or_else(|| {
            let (to, _) = self.resolve_with_intent(&target);
            plan(self.camera, to, self.globe_enabled)
                .map_or(Duration::ZERO, |(_, seconds)| ease_duration_from_ms(seconds * 1000.0))
        });
        self.ease_to(target, duration);
        let globe = self.globe_enabled;
        if let Some(motion) = &mut self.motion {
            motion.flight = plan(motion.from, motion.to, globe).map(|(flight, _)| flight).filter(|f| f.vw.is_some());
        }
    }

    fn advance_motion(&mut self, dt: Duration) -> bool {
        let Some(mut motion) = self.motion.take() else { return false };
        let (from, to) = (motion.from, motion.to);
        motion.elapsed = (motion.elapsed + dt).min(motion.duration);
        if motion.elapsed >= motion.duration {
            self.set_pose(to.center_lon, to.center_lat, to.zoom, to.pitch, to.bearing);
            self.motion = None;
            return false;
        }
        let t = motion.elapsed.as_secs_f64() / motion.duration.as_secs_f64();
        let k = EASE_TO_EASING.t_at(t, 0.0, 1.0);
        let pose = ease_pose(&from, &to, k);
        // A flight's centre and zoom ride its arc; pitch and bearing stay
        // `ease_pose`'s. A centre moved under it since planning (the plain
        // map's bounds) drops it back to an ease.
        if let Some(flight) = &motion.flight {
            let planned = &flight.to.camera;
            if (planned.center_lon, planned.center_lat) == (to.center_lon, to.center_lat) {
                let at = crate::story::fly::pose(flight, k);
                let zoom = at.zoom + (to.zoom - planned.zoom) * k;
                self.set_pose(at.center_lon, at.center_lat, zoom, pose.pitch, pose.bearing);
                self.motion = Some(motion);
                return true;
            }
            motion.flight = None;
        }
        // `ease_pose` alone falls back to `from`'s centre on a non-finite
        // world point (its only option, a pure function); the controller
        // instead falls back to `self.camera`'s -- the last eased pose --
        // matching this method's pre-refactor behaviour exactly.
        let (lon, lat) = eased_world_center(&from, &to, k).unwrap_or((self.camera.center_lon, self.camera.center_lat));
        self.set_pose(lon, lat, pose.zoom, pose.pitch, pose.bearing);
        self.motion = Some(motion);
        true
    }

    /// Writes a whole pose and collapses every ease onto it, so that when a
    /// motion ends -- or is cancelled -- nothing is left chasing an older
    /// target. The viewport is deliberately NOT part of a pose: a resize
    /// mid-motion must survive the motion's end. `pitch` is the user's
    /// (the intent): the camera draws what `zoom` allows of it.
    fn set_pose(&mut self, lon: f64, lat: f64, zoom: f64, pitch: f32, bearing: f32) {
        self.globe_pan_zoom = None;
        self.camera.center_lon = lon;
        self.camera.center_lat = lat;
        self.camera.zoom = zoom;
        self.set_pitch_intent(pitch);
        self.camera.bearing = bearing;
        self.zoom_target = ZoomTarget { zoom, center_lon: lon, center_lat: lat, anchor: None, ..self.zoom_target };
        self.target = OrientationTarget { bearing, pitch };
    }
}

impl CameraController {
    /// A live drag by `(dx, dy)` logical pixels, grabbed at `anchor_px`.
    /// Flat (`pan_flat`, native's formula) at pitch 0, where it is exact and
    /// where every pre-existing pan test runs. Grab-point (`pan_by_grab`)
    /// under pitch, where the same finger movement near the horizon crosses
    /// far more ground than near the bottom of the screen.
    fn pan_by_pixels(&mut self, dx: f32, dy: f32, anchor_px: (f32, f32)) {
        if !self.viewport_is_degenerate() && self.globe_at(&self.camera) && self.pan_on_globe(dx, dy, anchor_px) {
            return;
        }
        if self.camera.pitch == 0.0 || self.viewport_is_degenerate() || !self.pan_by_grab(dx, dy, anchor_px) {
            self.pan_flat(dx, dy);
        }
    }

    /// A drag on the globe: the point DRAWN under `anchor_px` is solved
    /// back under `anchor_px + (dx, dy)`, bearing fixed -- MapLibre's
    /// `handleMapControlsPan` / `versorSetLocationAtPoint` semantics, the
    /// grabbed place stays under the finger. A grab off the planet grabs
    /// the viewport centre instead, as MapLibre's does (`isPointOnMapSurface`
    /// else `centerPoint`).
    ///
    /// Near the silhouette of a pitched view exact tracking would fling the
    /// map (a ray grazing the sphere lands thousands of pixels away), so
    /// either end past `render::globe::pan_surface_point`'s handover turns
    /// the planet by the rate-limited surface points instead -- the globe's
    /// form of `grab_point`'s `GRAB_MAX_PITCH_SCALE` clamp.
    ///
    /// `false` (nothing moved) when no surface point can be formed; the
    /// caller then falls back to the FLAT pan, which on a globe frame is an
    /// approximation of the drag (rates agree near the centre), not the
    /// flat-map path.
    fn pan_on_globe(&mut self, dx: f32, dy: f32, anchor_px: (f32, f32)) -> bool {
        let centre = (self.camera.viewport_width_px / 2.0, self.camera.viewport_height_px / 2.0);
        let at = if self.pick(anchor_px).is_some() { anchor_px } else { centre };
        let to_px = (at.0 + dx, at.1 + dy);
        let surface = |p| crate::render::globe::pan_surface_point(&self.camera, GRAB_MAX_PITCH_SCALE, p);
        let (Some((from, from_exact)), Some((to, to_exact))) = (surface(at), surface(to_px)) else {
            return false;
        };
        if from_exact && to_exact {
            if let Some(world) = self.pick(at) {
                let exact = crate::render::globe::center_placing_world_at_keeping_size(
                    &self.globe_pan_solve_camera(),
                    world.truncate(),
                    to_px,
                    self.zoom_range,
                );
                // Near a pole the fixed-bearing map from centre to screen
                // is ill-conditioned, and the solve can put a 10 px drag's
                // centre 3.5 degrees north (re-review, lat 72 SE): there the
                // grab is let slip, as MapLibre's does, and the planet's own
                // turn below moves it instead. The exact answer is kept when
                // its latitude moves no more than 1.5 times that turn, or
                // POLAR_STEP_MAX_LAT_DEG per 10 px of drag, whichever is more.
                let turn = from.angle_between(to).to_degrees();
                let step_cap = (1.5 * turn).max(POLAR_STEP_MAX_LAT_DEG * f64::from(dx.hypot(dy)) / 10.0);
                let near_limit = self.camera.center_lat.abs() > MAX_MERCATOR_LAT - POLAR_PIN_BAND_DEG;
                let solved = exact.is_some();
                let exact = exact.filter(|&(_, lat)| (lat - self.camera.center_lat).abs() <= step_cap);
                if let Some((lon, lat)) = exact {
                    self.set_center_keeping_globe_size(wrap_lon(lon), lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT));
                    return true;
                }
                // No centre draws the grabbed place under the cursor
                // because the one that would lies past the latitude limit:
                // take the nearest centre ON the limit, and let the view
                // turn for the rest.
                let nearest = crate::render::globe::center_placing_world_at_nearest(
                    &self.globe_pan_solve_camera(),
                    world.truncate(),
                    to_px,
                    Some(self.zoom_range),
                );
                // Only from a centre already within a degree of the limit
                // (re-review): from further out the solve's pinned answer
                // can sit several degrees up, and taking it would jump the
                // centre there in one step; the swing below moves it only
                // as far as the drag does.
                let pinned = nearest.filter(|((_, lat), _, past)| {
                    near_limit
                        && (lat.abs() >= MAX_MERCATOR_LAT - 1e-9 || (*past && lat.abs() > MAX_MERCATOR_LAT - 1.0))
                });
                if let Some(((lon, lat), _, _)) = pinned {
                    self.set_center_keeping_globe_size(wrap_lon(lon), MAX_MERCATOR_LAT.copysign(lat));
                    self.turn_at_the_latitude_limit(world, to_px, f64::from(dx.hypot(dy)));
                    return true;
                }
                // An exact answer thrown out as a jump, from a centre ON
                // the limit: the centre stays pinned where it is and the
                // turn takes the drag (the swing would carry the pinned
                // centre off round the fold).
                if solved && self.camera.center_lat.abs() >= MAX_MERCATOR_LAT - 1e-9 {
                    self.turn_at_the_latitude_limit(world, to_px, f64::from(dx.hypot(dy)));
                    return true;
                }
            }
        }
        let Some((lon, lat)) = crate::render::globe::center_after_turning(&self.camera, from, to) else {
            return false;
        };
        let grabbed = if from_exact && to_exact { self.pick(at) } else { None };
        let lon = self.polar_dial_longitude(to_px, (dx, dy), lon);
        let pinned = lat.abs() >= MAX_MERCATOR_LAT;
        let lat = lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
        self.set_center_keeping_globe_size(lon, lat);
        // Only a centre the swing pinned ON the limit turns the view;
        // anywhere else the bearing holds and the grab may slip, as
        // MapLibre's does (review ruling).
        if let Some(world) = grabbed.filter(|_| pinned) {
            self.turn_at_the_latitude_limit(world, to_px, f64::from(dx.hypot(dy)));
        }
        true
    }

    /// A globe drag's centre move, with MapLibre's zoom adjustment
    /// (`handleMapControlsPan` -> `versorSetLocationAtPoint`, which ends
    /// `setZoom(oldZoom + getZoomAdjustment(oldLat, newLat))`): the sphere's
    /// radius goes as `2^zoom / cos(lat)`, so without it the planet grew
    /// 11.5x on screen as a drag carried the centre from the equator to the
    /// latitude limit. The zoom ease's target shifts by the same amount, so
    /// a settled camera stays settled and a gliding zoom glides on from
    /// where the drag left it. Clamped to the zoom range: at the floor the
    /// planet grows again, as it always did.
    fn set_center_keeping_globe_size(&mut self, lon: f64, lat: f64) {
        let unclamped =
            self.globe_pan_base_zoom() + crate::render::globe::globe_size_zoom_adjustment(self.camera.center_lat, lat);
        let (lo, hi) = self.zoom_range;
        let zoom = unclamped.clamp(lo, hi);
        let moved = zoom - self.camera.zoom;
        self.camera.zoom = zoom;
        self.zoom_target.zoom = (self.zoom_target.zoom + moved).clamp(lo, hi);
        self.globe_pan_zoom = Some((unclamped, zoom));
        self.set_center_now(lon, lat);
    }

    /// The zoom a globe drag step starts from: the previous step's
    /// unclamped zoom while the camera still shows what that step applied
    /// ([`Self::globe_pan_zoom`]), else the camera's own.
    fn globe_pan_base_zoom(&self) -> f64 {
        match self.globe_pan_zoom {
            Some((unclamped, applied)) if applied == self.camera.zoom => unclamped,
            _ => self.camera.zoom,
        }
    }

    /// The camera with its zoom at [`Self::globe_pan_base_zoom`], for the
    /// size-keeping solves, which add the adjustment and clamp themselves.
    fn globe_pan_solve_camera(&self) -> Camera {
        Camera { zoom: self.globe_pan_base_zoom(), ..self.camera }
    }

    /// A drag the latitude limit stopped: the centre is pinned on the
    /// clamp, so the rest of the drag is taken up by turning the view
    /// (bearing) with the planet about its axis (longitude) -- the only two
    /// freedoms left there -- solved so the grabbed `world` point lands as
    /// near `to_px` as they allow (Gauss-Newton on the renderer's own
    /// forward projection, every step made only if it gets nearer).
    ///
    /// Without it a drag pulling the view further over the pole moved
    /// nothing: the planet stopped dead in that direction. With it, a grab
    /// beside the pole's screen line follows the cursor, the view turning
    /// as it goes. A grab ON that line pulled straight away from the pole
    /// is the one drag nothing can realise (a turn about any point of the
    /// line moves a point on it across the line, never along it); there the
    /// bearing would have to spin without bound, so the turn is capped at
    /// one radian per [`POLAR_TURN_MIN_LEVER_PX`] of drag and the drag
    /// fades to what the cap allows rather than spinning the map.
    fn turn_at_the_latitude_limit(&mut self, world: glam::DVec3, to_px: (f32, f32), drag_px: f64) {
        let size = f64::from(crate::render::WORLD_SIZE);
        let (lon0, lat, bearing0) = (self.camera.center_lon, self.camera.center_lat, f64::from(self.camera.bearing));
        let base = self.camera;
        let drawn = |dl: f64, db: f64| {
            let cam = Camera { center_lon: wrap_lon(lon0 + dl), center_lat: lat, bearing: (bearing0 + db) as f32, ..base };
            let x = world.x + ((cam.world_center_f64().x - world.x) / size).round() * size;
            crate::render::globe::drawn_pixel_on_globe(&cam, (x, world.y))
                .map(|(x, y)| glam::DVec2::new(f64::from(x), f64::from(y)))
                .filter(|p| p.is_finite())
        };
        let target = glam::DVec2::new(f64::from(to_px.0), f64::from(to_px.1));
        let rate = drag_px / POLAR_TURN_MIN_LEVER_PX;
        let twist = self.pinned_twist;
        let (turn_lo, turn_hi) = ((-rate).max(-POLAR_TURN_MAX_TWIST - twist), rate.min(POLAR_TURN_MAX_TWIST - twist));
        if !(turn_lo <= turn_hi) {
            return;
        }
        let (mut dl, mut db) = (0.0f64, 0.0f64);
        let Some(mut s) = drawn(dl, db) else { return };
        let mut miss = (target - s).length();
        for _ in 0..8 {
            if miss < 0.05 {
                break;
            }
            let (hl, hb) = (1e-3, 1e-5);
            let (Some(pl), Some(pb)) = (drawn(dl + hl, db), drawn(dl, db + hb)) else { break };
            let (jl, jb) = ((pl - s) / hl, (pb - s) / hb);
            let jacobian = glam::DMat2::from_cols(jl, jb);
            if jacobian.determinant().abs() < 1e-12 || jl.length_squared() < 1e-12 {
                break;
            }
            let r = target - s;
            let mut step = jacobian.inverse() * r;
            let capped = (db + step.y).clamp(turn_lo, turn_hi) - db;
            if capped != step.y {
                // The turn is spent: the longitude alone does what it can.
                step.y = capped;
                step.x = jl.dot(r - jb * step.y) / jl.length_squared();
            }
            let Some(next) = drawn(dl + step.x, db + step.y) else { break };
            let next_miss = (target - next).length();
            if !(next_miss < miss) {
                break;
            }
            (dl, db, s, miss) = (dl + step.x, db + step.y, next, next_miss);
        }
        if dl != 0.0 || db != 0.0 {
            self.set_center_now(wrap_lon(lon0 + dl), lat);
            self.tilt_by(0.0, db as f32);
            self.pinned_twist += db;
            self.pole_turn += db;
        }
    }

    /// The centre longitude of a turned-planet drag near a pole: MapLibre's
    /// `fixedBearingLongitude` (`globe_utils.ts`, BSD-3).
    ///
    /// The swing's longitude (`swing_lon`, from `center_after_turning`) is
    /// ill-conditioned near the pole: a turn whose arc passes close to it
    /// swings the centre's longitude by tens of degrees for a pixel, and
    /// one passing OVER it flips it by 180, which the latitude clamp then
    /// pins back beside the pole -- the drag went sideways or backwards and
    /// flung the map. So within [`POLAR_DIAL_BAND_DEG`] of the clamp the
    /// cursor is treated as turning a dial about the pole's screen point,
    /// blended in by a smoothstep that is fully on at the clamp: the
    /// cursor's sweep angle about the pole is the planet's turn about its
    /// axis, which is exactly what a longitude change at fixed bearing is.
    /// Within [`POLAR_DIAL_MIN_RADIUS_PX`] of the pole the sweep is damped
    /// rather than dropped, so no spot there is dead.
    fn polar_dial_longitude(&self, cursor: (f32, f32), delta: (f32, f32), swing_lon: f64) -> f64 {
        let (old_lon, old_lat) = (self.camera.center_lon, self.camera.center_lat);
        let swing = shortest_lon_delta(old_lon, swing_lon);
        let ramp = (1.0 - (MAX_MERCATOR_LAT - old_lat.abs()) / POLAR_DIAL_BAND_DEG).clamp(0.0, 1.0);
        let dial = ramp * ramp * (3.0 - 2.0 * ramp);
        let north = old_lat >= 0.0;
        let pole = crate::render::globe::pole_screen_point(&self.camera, north);
        let Some(pole) = pole.filter(|_| dial > 0.0) else {
            return wrap_lon(old_lon + swing);
        };
        let (rx, ry) = (f64::from(cursor.0) - pole.0, f64::from(cursor.1) - pole.1);
        let r2 = (rx * rx + ry * ry).max(POLAR_DIAL_MIN_RADIUS_PX * POLAR_DIAL_MIN_RADIUS_PX);
        // Screen y points down, so a positive sweep is clockwise on screen.
        let sweep = (rx * f64::from(delta.1) - ry * f64::from(delta.0)) / r2;
        let turn = if north { sweep } else { -sweep }.to_degrees();
        wrap_lon(old_lon + (1.0 - dial) * swing + dial * turn)
    }

    /// Translates the camera so the ground under `anchor_px` ends up under
    /// `anchor_px + (dx, dy)`. Exact for any pitch and bearing, in one step:
    /// the camera's view of the ground plane is translation-equivariant, so
    /// moving the centre by `from - to` moves what every pixel shows by
    /// exactly that much. Near the horizon both ends go through
    /// `grab_point`'s rate clamp, so there it is the clamp row's rate rather
    /// than exact. Returns `false` (and moves nothing) when no grab point
    /// reaches the ground.
    ///
    /// f64 throughout, for `zoom_towards_point`'s reason: this subtracts two
    /// world positions of magnitude ~256 to recover a few pixels over
    /// `2^zoom`.
    fn pan_by_grab(&mut self, dx: f32, dy: f32, anchor_px: (f32, f32)) -> bool {
        if self.viewport_is_degenerate() {
            return false;
        }
        let (Some(from), Some(to)) = (self.grab_point(anchor_px), self.grab_point((anchor_px.0 + dx, anchor_px.1 + dy)))
        else {
            return false;
        };
        self.shift_centre_by_world(from - to)
    }

    /// The ground under screen point `p`, rate-clamped to
    /// [`GRAB_MAX_PITCH_SCALE`].
    ///
    /// A point whose ray misses the ground (above the horizon), or lands
    /// where one pixel covers more than the limit, is not trusted. Instead
    /// the clamp row (the topmost usable row of its column, found by
    /// bisection toward the centre row) is found, and the point is
    /// EXTRAPOLATED from the ground there, at that row's ground-per-pixel
    /// rate, for as many pixels as `p` sits above it. The result is
    /// continuous at the clamp row, finite everywhere, and moves the same
    /// way the real ground does, so a drag that starts, ends, or stays in the
    /// band still moves the map in the drag's direction (ruling F2). Pulling
    /// the point down ONTO the clamp row instead would put both ends of an
    /// in-band drag on one row, and the drag would move nothing.
    ///
    /// The centre row is always usable: every ground point on it has the
    /// look-at point's depth, so its scale is exactly 1. Below the centre row
    /// the ground is nearer and the scale is below 1, so a point there never
    /// needs clamping.
    fn grab_point(&self, p: (f32, f32)) -> Option<glam::DVec3> {
        let usable = |x: f32, y: f32| {
            crate::render::unproject_to_ground_f64(&self.camera, x, y)
                .filter(|g| g.is_finite())
                .filter(|g| crate::render::pitch_scale_factor_at_ground(&self.camera, *g) <= GRAB_MAX_PITCH_SCALE)
        };
        if let Some(ground) = usable(p.0, p.1) {
            return Some(ground);
        }
        let centre_row = self.camera.viewport_height_px / 2.0;
        if p.1.is_nan() || p.1 >= centre_row {
            return None;
        }
        let (mut good, mut bad) = (centre_row, p.1);
        for _ in 0..32 {
            let mid = 0.5 * (good + bad);
            if usable(p.0, mid).is_some() {
                good = mid;
            } else {
                bad = mid;
            }
        }
        // `at_clamp - one_below` is the ground one pixel UP the column
        // covers at the clamp row; `p` is `good - p.1` pixels further up.
        let at_clamp = usable(p.0, good)?;
        let one_below = usable(p.0, good + 1.0)?;
        Some(at_clamp + (at_clamp - one_below) * f64::from(good - p.1))
    }

    /// Moves the centre by `delta` world units, at once: THE centre-moving
    /// helper every measured-slide move shares (grab pan, and anchored
    /// rotation in `rotate_now`). Returns `false`, moving nothing, when the moved
    /// centre is not a finite position ([`lon_lat_at_world`]).
    pub(crate) fn shift_centre_by_world(&mut self, delta: glam::DVec3) -> bool {
        let Some((lon, lat)) = lon_lat_at_world(self.camera.world_center_f64() + delta) else {
            return false;
        };
        self.set_center_now(lon, lat);
        true
    }

    /// An immediate centre move. The zoom ease's target follows it (so the
    /// ease does not pull the camera back) and its anchor is dropped (an
    /// anchor describes a centre this move has just overridden) -- the same
    /// three writes `pan_flat` ends with.
    fn set_center_now(&mut self, lon: f64, lat: f64) {
        self.camera.center_lon = lon;
        self.camera.center_lat = lat;
        self.zoom_target.center_lon = lon;
        self.zoom_target.center_lat = lat;
        self.zoom_target.anchor = None;
    }

    /// A viewport under one pixel on either axis, or NaN. Every unprojection
    /// divides by the viewport, so each path that unprojects checks this
    /// first and falls back to its centre-anchored form. The browser's
    /// canvas can report 0x0 CSS pixels while hidden or before layout.
    fn viewport_is_degenerate(&self) -> bool {
        viewport_is_degenerate(&self.camera)
    }

    /// The flat-map drag: the whole of what `MapView::pan_by_pixels` was,
    /// moved. Task 3 routes a pitched drag elsewhere.
    fn pan_flat(&mut self, dx: f32, dy: f32) {
        let zoom = self.camera.zoom;
        let (x, y) = crate::geo::lon_lat_to_tile_xy(self.camera.center_lon, self.camera.center_lat, zoom);
        // The centre moves opposite the drag: dragging right pulls the map
        // right, which walks the centre left.
        let base_x = -dx as f64 / crate::geo::TILE_SIZE;
        let base_y = -dy as f64 / crate::geo::TILE_SIZE;
        // ...but "left" is a SCREEN direction, and the screen is rotated by
        // `bearing` relative to tile space, so the screen delta has to be
        // rotated into tile space before it can be added to a tile
        // coordinate. Without this the map moves off by exactly the bearing
        // angle whenever it is rotated -- a bug that predates rotation
        // controls (Shift+drag rotate has always existed) and stayed
        // invisible only because the correction is the exact identity at
        // bearing 0, which is where a map that is never rotated spends its
        // whole life.
        //
        // Tile space is x=east, y=SOUTH, so this is the rotation that takes
        // screen axes to tile axes for a camera whose heading is `bearing`.
        // Worked example, matching the iOS shell's independently measured
        // result: at bearing 90 degrees (facing east) screen-left is due
        // north, so a rightward drag walks the centre due north with
        // longitude unchanged -- `base = (-1, 0)` maps to `(0, -1)`, and
        // tile -y is north.
        let (sin_bearing, cos_bearing) = (self.camera.bearing as f64).sin_cos();
        let new_x = x + cos_bearing * base_x - sin_bearing * base_y;
        let new_y = y + sin_bearing * base_x + cos_bearing * base_y;
        // `tile_xy_to_lon_lat` is a straight linear map off the tile grid: a
        // tile x past the east edge comes back as a longitude past +180, and
        // nothing downstream of here ever brought it home. This is where the
        // unbounded growth came from -- a drag is the only input that can
        // walk the centre any distance at all. Wrapped as the LAST step so the
        // tile arithmetic above still sees a continuous coordinate; see
        // `wrap_lon`, and `wrap_bearing` for the bearing precedent.
        let (lon, lat) = crate::geo::tile_xy_to_lon_lat(new_x, new_y, zoom);
        let lon = wrap_lon(lon);
        self.camera.center_lon = lon;
        self.camera.center_lat = lat;
        // Panning is immediate, not eased — keep the zoom-ease target in
        // lockstep so `advance_zoom_animation` doesn't fight the pan by
        // trying to pull the center back toward a now-stale pre-pan target.
        self.zoom_target.center_lon = lon;
        self.zoom_target.center_lat = lat;
        // Same reason, for the anchored branch of the ease: an anchor from an
        // earlier zoom gesture describes a center this pan has just
        // deliberately overridden, so holding it would undo the drag.
        self.zoom_target.anchor = None;
    }

    /// Nudges the zoom target by `levels`, from the RENDERED zoom rather than
    /// the (possibly still-easing) previous target -- so each new scroll
    /// input starts its delta from what the user actually sees right now,
    /// instead of compounding against an in-flight animation.
    /// Takes LEVELS; `map_view` converts scroll lines before calling in.
    fn zoom_by(&mut self, levels: f64) {
        self.zoom_target.zoom = (self.camera.zoom + levels).clamp(self.zoom_range.0, self.zoom_range.1);
    }

    /// Zooms by `amount` (same units as `zoom_by`), keeping the world point
    /// under `(screen_x, screen_y)` visually anchored in place. Uses the
    /// standard flat-map "zoom about a point" formula — exact at pitch=0,
    /// a reasonable approximation at nonzero pitch (a fully exact solution
    /// for a tilted perspective camera would need iterative solving; not
    /// worth it for this pass). Falls back to a plain center-anchored zoom
    /// if the given point doesn't hit the ground (e.g. looking above the
    /// horizon at a steep pitch).
    ///
    /// Writes the result into `zoom_target` (`zoom`, `center_lon`/`center_lat`), not directly
    /// into `camera` — `camera` (what's actually drawn) eases toward that
    /// target over subsequent frames via `advance_zoom_animation`, which is
    /// what makes scroll-wheel zoom feel smooth instead of snapping. The
    /// anchor math itself is computed from `self.camera` (the currently
    /// *rendered* position), since that's what's actually on screen under
    /// the cursor right now.
    ///
    /// `immediate` controls how fast `advance_zoom_animation`'s ease chases
    /// the new target: `true` (trackpad continuous scroll, already smooth at
    /// the source) selects a much faster rate ([`ZOOM_EASE_RATE_IMMEDIATE`]) than
    /// `false` (discrete wheel notches and double-click, needing manufactured
    /// smoothness).
    ///
    /// This used to hard-snap `camera` straight to the target for `immediate`
    /// input, bypassing the ease entirely. That produced a visible left-right
    /// oscillation at zoom ~13+, growing with zoom level: real trackpad
    /// scroll input isn't perfectly monotonic (small back-and-forth noise in
    /// the raw delta stream and cursor position between successive ticks),
    /// and with zero damping that noise passed straight through as
    /// per-tick position ping-pong. The old eased path absorbed this noise
    /// as a side effect of its low-pass filtering; a literal snap has no
    /// such filter. Because `screen_pixels_per_world_unit = 2^zoom` (see
    /// `render::camera`'s module docs), the same absolute world-space noise
    /// covers more screen pixels at higher zoom, which is exactly why the
    /// oscillation was invisible at z13 and grew from there. Using a fast
    /// but finite rate instead keeps the feel immediate while restoring a
    /// real (if very short) low-pass filter against that noise.
    fn zoom_towards_point(&mut self, amount: f64, screen_x: f32, screen_y: f32, immediate: bool) {
        // 60.0 gives a ~90%-converged time constant of ~26ms -- a couple of
        // frames at 60fps, imperceptibly fast to the user -- but still damps
        // out noise between successive scroll ticks, which typically arrive
        // every ~8-16ms during continuous trackpad scrolling. 20.0 is the
        // existing wheel/double-click rate, unchanged.
        self.zoom_target.ease_rate = if immediate { ZOOM_EASE_RATE_IMMEDIATE } else { ZOOM_EASE_RATE_DISCRETE };
        if self.viewport_is_degenerate() {
            self.zoom_target.anchor = None;
            self.zoom_by(amount);
            return;
        }

        // Both of these are f64 (`unproject_to_ground_f64` /
        // `world_center_f64`), and that is load-bearing, not incidental
        // tidiness. This formula subtracts two world positions of magnitude
        // ~128-256 to recover a difference that is only
        // `cursor_offset_px / 2^zoom` world units across -- 0.0036 world
        // units for a cursor 240px off-center at z16. In f32 (one ulp
        // ~1.5e-5 world units, i.e. `1.5e-5 * 2^zoom` screen pixels) that
        // subtraction is catastrophic cancellation: the rounding error in
        // `old_center` alone passes almost undamped into the new target
        // (`err(target) ~= err(old_center) / scale_factor`, and
        // `scale_factor` is ~1.008 for one trackpad tick), so every frame
        // the target center got yanked toward the nearest f32 grid node --
        // a 4px-spaced grid at z18, 16px at z20. Measured with the f32
        // version, over a 30-frame trackpad gesture at z18: 9.56px of drift
        // in the point under the cursor, with up to 1.42px of it landing in a
        // single frame (~85px/s of visible slide), versus 0.148px total in
        // f64. See `continuous_trackpad_zoom_keeps_the_cursor_anchored_at_high_zoom`.
        if self.globe_at(&self.camera) || self.globe_at(&Camera { zoom: self.camera.zoom + amount, ..self.camera }) {
            self.zoom_towards_point_on_globe(amount, (screen_x, screen_y));
            return;
        }
        let Some(click_world) = crate::render::unproject_to_ground_f64(&self.camera, screen_x, screen_y) else {
            // No ground under the cursor: fall back to a plain center-anchored
            // zoom, and drop any anchor a previous call left behind so the
            // ease doesn't keep correcting toward a point this input never
            // asked to hold.
            self.zoom_target.anchor = None;
            self.zoom_by(amount);
            return;
        };
        let old_center = self.camera.world_center_f64();
        let old_zoom = self.camera.zoom;
        self.zoom_by(amount);
        let scale_factor = 2f64.powf(self.zoom_target.zoom - old_zoom);
        if scale_factor <= 0.0 || !scale_factor.is_finite() {
            return; // amount was zero or degenerate; zoom_by already clamped it
        }

        // An anchor east of a centre already near +180 puts the target past
        // it, so this wraps like every other write (`wrap_lon`). It is safe
        // to wrap here and NOT to wrap `click_world` with it: the anchored
        // branch of `advance_zoom_animation` derives the centre from the
        // anchor and never reads `zoom_target.center_lon`, and the one place that
        // does read it -- the settle test -- goes through
        // `shortest_lon_delta`, so a target on the far side of the wrap from
        // the camera is still 0.05 degrees away there, not 359.95.
        let (lon, lat) = center_anchored_at(click_world, old_center, scale_factor);
        self.zoom_target.center_lon = wrap_lon(lon);
        self.zoom_target.center_lat = lat;
        self.zoom_target.anchor = Some(click_world);
    }

    /// `zoom_towards_point` when a globe frame is involved (now or at the
    /// target zoom): the point DRAWN under the cursor is the anchor, and the
    /// target centre is solved at the target zoom so the anchor is drawn
    /// back under the cursor there. `advance_zoom_animation` solves the same
    /// way at every eased zoom (`anchor_px`).
    fn zoom_towards_point_on_globe(&mut self, amount: f64, px: (f32, f32)) {
        let Some(click_world) = self.pick(px) else {
            self.zoom_target.anchor = None;
            self.zoom_by(amount);
            return;
        };
        self.zoom_by(amount);
        let at_target = Camera {
            zoom: self.zoom_target.zoom,
            pitch: self.pitch_intent().min(self.pitch_ceiling(self.zoom_target.zoom, self.zoom_target.center_lat)),
            ..self.camera
        };
        let Some((lon, lat)) = self.centre_placing(&at_target, click_world, px) else {
            self.zoom_target.anchor = None;
            return;
        };
        self.zoom_target.center_lon = lon;
        self.zoom_target.center_lat = lat;
        self.zoom_target.anchor = Some(click_world);
        self.zoom_target.anchor_px = px;
    }

    /// Eases `camera.zoom`/`camera.center_{lon,lat}` toward
    /// `zoom_target.zoom`/`zoom_target.center_{lon,lat}` — an exponential decay (a
    /// standard, frame-rate-independent "ease toward target" — the same
    /// shape as a critically-damped spring's approach curve) rather than a
    /// fixed-duration tween, so it composes cleanly with new input arriving
    /// mid-animation (a fresh `zoom_towards_point` call just redirects the
    /// target; this keeps chasing it from wherever `camera` currently is,
    /// no restart or discontinuity). Returns `true` while the camera is
    /// still visibly catching up, so the caller knows whether to keep
    /// requesting animation frames.
    ///
    /// Longitude is crossed with `shortest_lon_delta` rather than a raw
    /// subtraction, for the reason `advance_orientation_animation` crosses
    /// bearing with `shortest_bearing_delta`: both ends are wrapped
    /// (`wrap_lon`), so a camera at 179.9 chasing a target at -179.9 is
    /// 0.2 degrees from it, and the raw -359.8 would drag it west across the
    /// whole globe to arrive next door.
    fn advance_zoom_animation(&mut self, dt: Duration) -> bool {
        const EPSILON_ZOOM: f64 = 1e-4;
        const EPSILON_DEGREES: f64 = 1e-9;
        let zoom_diff = self.zoom_target.zoom - self.camera.zoom;
        if zoom_diff != 0.0 {
            // This ease is about to write the zoom: a drag's unclamped
            // zoom no longer describes the camera.
            self.globe_pan_zoom = None;
        }
        let lon_diff = shortest_lon_delta(self.camera.center_lon, self.zoom_target.center_lon);
        let lat_diff = self.zoom_target.center_lat - self.camera.center_lat;
        // A globe-anchored zoom's centre is SOLVED each frame to within a
        // hundredth of a pixel, and the target centre was solved the same
        // way at the target zoom, so the two agree to that and not to
        // `EPSILON_DEGREES`: once the zoom has arrived it has arrived.
        let globe_anchored = self.zoom_target.anchor.is_some() && self.globe_at(&self.camera);
        if zoom_diff.abs() < EPSILON_ZOOM
            && (globe_anchored || (lon_diff.abs() < EPSILON_DEGREES && lat_diff.abs() < EPSILON_DEGREES))
        {
            // Snap exactly onto the target rather than leaving a
            // never-quite-zero residual (exponential decay never reaches
            // zero in finite time) so the camera settles precisely instead
            // of drifting by a sub-pixel amount forever.
            self.camera.zoom = self.zoom_target.zoom;
            self.limit_pitch();
            self.camera.center_lon = self.zoom_target.center_lon;
            self.camera.center_lat = self.zoom_target.center_lat;
            // The gesture is over; nothing left to hold anchored.
            self.zoom_target.anchor = None;
            return false;
        }
        // Convergence rate in 1/seconds, set by the most recent
        // `zoom_towards_point` call (see `ZoomTarget::ease_rate`'s doc comment):
        // 20.0 for discrete wheel notches/double-click (~90% of the way to
        // target in ~115ms — a deliberate ease), or 60.0 for trackpad-sourced
        // `immediate` input (~90% converged in ~26ms — fast enough to read as
        // instant, but not a true zero-damping snap, which let real input
        // noise through as visible position oscillation; see
        // `zoom_towards_point`'s doc comment for the full story).
        let rate = self.zoom_target.ease_rate;
        let t = 1.0 - (-rate * dt.as_secs_f64()).exp();
        let zoom_before = self.camera.zoom;
        self.camera.zoom += zoom_diff * t;
        self.limit_pitch();
        match self.zoom_target.anchor {
            // Anchored zoom: derive the center from the zoom we actually
            // eased to this frame, so the anchor is held EXACTLY at every
            // intermediate zoom rather than only at the target.
            //
            // Lerping lon/lat by the same `t` used for zoom (what this used
            // to do) does not do that: the center a fixed anchor demands is
            // `anchor + (center - anchor) * 2^(-Δzoom)`, an exponential in
            // zoom, and a straight line between its endpoints always cuts the
            // corner. The error is second order in the per-frame zoom step
            // (`≈ offset_px * (Δ·ln2)² · t(1-t)/2`) and always has the same
            // sign, so it accumulated: measured 2.96px of outward slide over
            // a 30-frame gesture covering ~1.2 zoom levels (identically at
            // z14 and z18 -- unlike the f32 bug this is zoom-independent), so
            // whatever the user aimed at slid steadily away from the cursor.
            // Recomputing from the eased zoom instead makes the anchor exact
            // and leaves nothing to accumulate: 0.034px over the same
            // gesture, which is just its synthetic cursor jitter.
            //
            // This is the same model MapLibre GL JS uses: its scroll handler
            // eases only the zoom scalar and applies it with
            // `tr.zoomTo(zoom, aroundPoint)` per frame (see
            // `src/ui/handler/scroll_zoom.ts`), never interpolating a center
            // independently of the zoom it belongs to.
            //
            // The anchor has to travel with the camera when the centre wraps.
            // `zoom_anchor` is a WORLD point (`unproject_to_ground_f64` at
            // gesture start), `world_center_f64` is a linear map from
            // longitude to world x, and `center_anchored_at` works on the
            // DIFFERENCE between the two -- so wrapping `center_lon` by 360
            // degrees moves the camera's world x by a whole `WORLD_SIZE`
            // while leaving the anchor in the world copy the gesture started
            // in. The next frame would then read an offset a world wide and
            // fling the camera half way round it. Shifting the stored anchor
            // by the same turn keeps the pair in one copy, which also keeps
            // the anchor projectable: it is still a point the current camera
            // can put back under the cursor. Measured across a z4 zoom-in
            // that crosses the antimeridian (see
            // `an_anchored_zoom_across_the_antimeridian_holds_its_anchor`):
            // 3.14e-11 px of worst-frame anchor drift, against 1.23e-10 px
            // for the same gesture run from lon 100. Crossing costs the
            // anchor nothing.
            Some(anchor)
                if self.globe_at(&self.camera) || self.globe_at(&Camera { zoom: zoom_before, ..self.camera }) =>
            {
                // On a globe frame: solve the centre that draws the anchor
                // under the cursor at the zoom eased to; if the solve fails,
                // lerp like an unanchored zoom.
                match self.centre_placing(&self.camera, anchor, self.zoom_target.anchor_px) {
                    Some((lon, lat)) => {
                        self.camera.center_lon = lon;
                        self.camera.center_lat = lat;
                    }
                    None => {
                        self.camera.center_lon = wrap_lon(self.camera.center_lon + lon_diff * t);
                        self.camera.center_lat += lat_diff * t;
                    }
                }
            }
            Some(anchor) => {
                let center_before = self.camera.world_center_f64();
                let scale = 2f64.powf(self.camera.zoom - zoom_before);
                if scale > 0.0 && scale.is_finite() {
                    let (lon, lat) = center_anchored_at(anchor, center_before, scale);
                    let wrapped = wrap_lon(lon);
                    let turns = ((lon - wrapped) / 360.0).round();
                    if turns != 0.0 {
                        let shifted_x = anchor.x - turns * WORLD_SIZE as f64;
                        self.zoom_target.anchor = Some(glam::DVec3::new(shifted_x, anchor.y, anchor.z));
                    }
                    self.camera.center_lon = wrapped;
                    self.camera.center_lat = lat;
                }
            }
            // Unanchored (pan-synced target, or a cursor ray that missed the
            // ground): plain lerp, as before -- but along `lon_diff`, which is
            // now the SHORTEST way round (see this function's doc comment),
            // and wrapped after the step because a lerp that starts at 179.9
            // and heads east lands outside the range on its way.
            None => {
                self.camera.center_lon = wrap_lon(self.camera.center_lon + lon_diff * t);
                self.camera.center_lat += lat_diff * t;
            }
        }
        true
    }

    /// Applies an orientation delta IMMEDIATELY -- the direct-manipulation
    /// path, used by Shift+drag and trackpad Option+scroll, where the camera
    /// must track the hand with no lag.
    ///
    /// Collapses `target_bearing`/`target_pitch` onto the new camera values
    /// as it goes, which is what lets a live gesture override an in-flight
    /// ease (a reset-to-north, say) instead of fighting it: after this the
    /// ease sees a zero difference and stops.
    ///
    /// See `MAX_PITCH` for the pitch clamp, and `wrap_bearing` for why the
    /// bearing write is not the bare `+=` it used to be.
    ///
    /// Under the low-zoom limit ([`max_pitch_at_zoom`]) the delta moves the
    /// DRAWN pitch: tilting down from a limited view lowers the user's pitch
    /// to what they now see; tilting up raises it no further than the limit
    /// and never lowers a higher pitch the limit is holding back.
    fn tilt_by(&mut self, delta_pitch: f32, delta_bearing: f32) {
        if delta_pitch != 0.0 && delta_pitch.is_finite() {
            let ceiling = self.pitch_ceiling(self.camera.zoom, self.camera.center_lat);
            let drawn = (self.camera.pitch + delta_pitch).clamp(0.0, self.max_pitch());
            let intent = if delta_pitch > 0.0 { self.pitch_intent().max(drawn.min(ceiling)) } else { drawn };
            self.set_pitch_intent(intent);
        }
        self.camera.bearing = wrap_bearing(self.camera.bearing + delta_bearing);
        self.target.pitch = self.pitch_intent();
        self.target.bearing = self.camera.bearing;
    }

    /// Steers the EASED orientation target by `delta_bearing` radians -- the
    /// discrete-input path, used by keyboard rotation and wheel-notch
    /// rotation, where a step should glide rather than snap.
    ///
    /// Accumulates on `target_bearing`, not on `camera.bearing`: pressing
    /// the key three times quickly must queue three steps, and reading the
    /// live (mid-ease) camera would silently swallow whatever of the
    /// previous step had not been travelled yet.
    ///
    /// Snaps the result to north when it lands inside `NORTH_SNAP_WINDOW`.
    /// A discrete step is a settling point by definition, so this is the
    /// "gesture ended" moment for the keyboard -- and with a 15-degree step
    /// against a 7-degree window it only ever fires for a user who reached
    /// an odd bearing by dragging and is now walking it back with the keys.
    fn rotate_target_by(&mut self, delta_bearing: f32) {
        self.target.bearing = bearing_after_north_snap(self.target.bearing + delta_bearing);
    }

    /// Steers the eased pitch target by `delta_pitch` radians. The tilt
    /// counterpart of `rotate_target_by`, clamped to the same `MAX_PITCH`
    /// the camera itself is clamped to so the ease always has a reachable
    /// target.
    ///
    /// The target is the user's pitch; under the low-zoom limit a step
    /// moves it the way [`Self::tilt_by`] moves a live tilt.
    fn pitch_target_by(&mut self, delta_pitch: f32) {
        if !delta_pitch.is_finite() {
            return;
        }
        let ceiling = self.pitch_ceiling(self.camera.zoom, self.camera.center_lat);
        let drawn = (self.target.pitch.min(ceiling) + delta_pitch).clamp(0.0, self.max_pitch());
        self.target.pitch = if delta_pitch > 0.0 { self.target.pitch.max(drawn.min(ceiling)) } else { drawn };
    }

    /// Eases the map back to true north, leaving pitch alone -- the
    /// equivalent of clicking a compass needle, and of MapLibre's
    /// `map.resetNorth()`.
    fn reset_north(&mut self) {
        self.target.bearing = 0.0;
    }

    /// Eases the map back to true north AND flat on its back: the "put it
    /// back the way it was" escape hatch. MapLibre spells this
    /// `map.resetNorthPitch()`.
    ///
    /// Worth binding separately because pitch, unlike bearing, has no
    /// landmark a user can steer back to by eye -- a map left at 4 degrees
    /// of tilt looks subtly wrong with nothing on screen explaining why.
    fn reset_north_and_pitch(&mut self) {
        self.target.bearing = 0.0;
        self.target.pitch = 0.0;
    }

    /// Eases `camera.bearing`/`camera.pitch` toward
    /// `target_bearing`/`target_pitch`, returning whether either is still in
    /// flight (so `paint` knows to ask for another frame).
    ///
    /// Same exponential-decay shape as `advance_zoom_animation`, and snaps
    /// exactly onto the target inside an epsilon for the same reason: an
    /// exponential never reaches its target in finite time, so without the
    /// snap the map would hold the frame loop open forever chasing a
    /// sub-microradian residual.
    ///
    /// Bearing is crossed with `shortest_bearing_delta` rather than a raw
    /// subtraction -- see that function for the 358-degrees-the-wrong-way
    /// failure it exists to prevent.
    fn advance_orientation_animation(&mut self, dt: Duration) -> bool {
        const EPSILON_RADIANS: f32 = 1e-4;
        // The ease steers the user's pitch (the intent); the camera draws
        // what the zoom allows of it.
        let bearing_diff = shortest_bearing_delta(self.camera.bearing, self.target.bearing);
        let intent = self.pitch_intent();
        let pitch_diff = self.target.pitch - intent;
        if bearing_diff.abs() < EPSILON_RADIANS && pitch_diff.abs() < EPSILON_RADIANS {
            self.camera.bearing = self.target.bearing;
            self.set_pitch_intent(self.target.pitch);
            return false;
        }
        let t = 1.0 - (-ORIENTATION_EASE_RATE * dt.as_secs_f32()).exp();
        self.camera.bearing = wrap_bearing(self.camera.bearing + bearing_diff * t);
        self.set_pitch_intent((intent + pitch_diff * t).clamp(0.0, self.max_pitch()));
        true
    }
}

/// Read-only windows onto the eases, for `map_view`'s own tests of its input
/// path. `#[cfg(test)]`: production code reads [`CameraController::camera`]
/// and [`CameraController::is_settled`] and nothing else.
#[cfg(test)]
impl CameraController {
    pub(crate) fn camera_mut(&mut self) -> &mut Camera {
        &mut self.camera
    }
    pub(crate) fn target_zoom(&self) -> f64 {
        self.zoom_target.zoom
    }
    pub(crate) fn target_center(&self) -> (f64, f64) {
        (self.zoom_target.center_lon, self.zoom_target.center_lat)
    }
    pub(crate) fn zoom_anchor(&self) -> Option<glam::DVec3> {
        self.zoom_target.anchor
    }
    pub(crate) fn set_zoom_anchor(&mut self, anchor: Option<glam::DVec3>) {
        self.zoom_target.anchor = anchor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The on-screen diameter of a shell of `radius_m` from the camera, in px (spec §2.6 formula).
    fn shell_diameter_px(radius_m: f64, lat: f64, zoom: f64, _w: f32, h: f32) -> Option<f64> {
        let r_world = 512.0 / (std::f64::consts::TAU * lat.to_radians().cos());
        let tan_half = (crate::render::FOV_Y_RADIANS_F64 / 2.0).tan();
        let d = f64::from(h) / (2.0 * tan_half * 2f64.powf(zoom));
        let eye = (r_world + d) / r_world;
        let r = radius_m / crate::render::globe::GLOBE_RADIUS_M;
        (eye > r).then(|| 2.0 * (r / eye).asin().tan() * (f64::from(h) / 2.0) / tan_half)
    }

    #[test]
    fn geo_fits_on_phone_and_desktop_at_every_latitude_from_the_space_floor() {
        for (w, h) in [(430.0, 932.0), (1440.0, 900.0)] {
            for lat in [0.0, 60.0, 85.0] {
                let z = fit_shell_zoom(42_164_000.0, 0.1, lat, w, h).expect("fits");
                assert!(z >= SPACE_MIN_ZOOM - 1e-9, "{w}x{h} lat {lat}: z {z} is below the floor");
                let dia = shell_diameter_px(42_164_000.0, lat, z, w, h).expect("eye outside GEO");
                let short = f64::from(w.min(h));
                assert!(dia <= 0.9 * short + 1.0 && dia >= 0.85 * short, "{w}x{h} lat {lat}: {dia} px of {short}");
            }
        }
    }

    /// The eye camera is opt-in: a controller holds pitch to 60 degrees
    /// and zoom to 20 until `set_eye_limits`, then reaches an eye 1.1 m up
    /// at 85 degrees (`eye_target`), and `None` brings it straight back.
    #[test]
    fn the_eye_limits_are_opt_in_and_reach_a_driver_eye() {
        let camera = Camera { center_lon: 103.859, center_lat: 1.29, zoom: 16.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 1024.0, viewport_height_px: 768.0, fov_y: Camera::DEFAULT_FOV_Y };
        let mut c = CameraController::new(camera, MIN_ZOOM, MAX_ZOOM);
        let target = eye_target((103.859, 1.29), 1.1, 0.4, 84f32.to_radians(), 768.0);
        let z = target.zoom.unwrap();
        assert!((21.5..23.0).contains(&z), "1.1 m up at 84 degrees in 768 px: z{z}");
        c.jump_to(target);
        assert_eq!(c.camera().pitch, MAX_PITCH, "no eye limits: the default ceiling");
        assert_eq!(c.camera().zoom, MAX_ZOOM);
        c.set_eye_limits(Some((EYE_MAX_PITCH, EYE_MAX_ZOOM)));
        c.jump_to(target);
        assert!((c.camera().pitch - 84f32.to_radians()).abs() < 1e-6, "{}", c.camera().pitch.to_degrees());
        assert!((c.camera().zoom - z).abs() < 1e-9);
        let h = c.camera().distance() * f64::from(c.camera().pitch.cos()) * metres_per_world_unit(c.camera().center_lat);
        assert!((h - 1.1).abs() < 1e-3, "the eye is {h} m up");
        // The eye ground point is where the eye was asked to be.
        let (ex, ey) = crate::geo::lon_lat_to_tile_xy(103.859, 1.29, 0.0);
        let (eye, _, _) = c.camera().eye_target_up();
        let eye = eye.truncate();
        let off_m = (eye - glam::DVec2::new(ex * WORLD_SIZE as f64, ey * WORLD_SIZE as f64)).length() * metres_per_world_unit(1.29);
        assert!(off_m < 0.01, "the eye is {off_m} m from where it was put");
        c.jump_to(CameraTarget { pitch: Some(1.6), ..Default::default() });
        assert_eq!(c.camera().pitch, EYE_MAX_PITCH, "never past 85");
        c.set_eye_limits(None);
        assert_eq!(c.camera().pitch, MAX_PITCH);
        assert_eq!(c.camera().zoom, MAX_ZOOM);
        assert_eq!(c.zoom_range(), (MIN_ZOOM, MAX_ZOOM));
    }

    #[test]
    fn the_default_range_is_unchanged_and_the_space_floor_is_opt_in() {
        let camera = Camera { center_lon: 0.0, center_lat: 0.0, zoom: 1.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 800.0, viewport_height_px: 600.0, fov_y: Camera::DEFAULT_FOV_Y };
        let mut c = CameraController::new(camera, MIN_ZOOM, MAX_ZOOM);
        assert_eq!(c.zoom_range(), (MIN_ZOOM, MAX_ZOOM));
        assert_eq!(MIN_ZOOM, -2.0, "the default floor did not move");
        c.set_zoom_range(-5.0, 20.0);
        assert_eq!(c.zoom_range(), (-5.0, 20.0));
        c.set_zoom_range(-9.0, 20.0);
        assert_eq!(c.zoom_range(), (SPACE_MIN_ZOOM, 20.0), "intersected with the space floor, not below it");
    }

    #[test]
    fn an_opted_in_view_reaches_minus_five_and_stays() {
        let camera = Camera { center_lon: 0.0, center_lat: 85.0, zoom: 1.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 430.0, viewport_height_px: 932.0, fov_y: Camera::DEFAULT_FOV_Y };
        let mut c = CameraController::new(camera, MIN_ZOOM, MAX_ZOOM);
        c.set_zoom_range(SPACE_MIN_ZOOM, MAX_ZOOM);
        c.jump_to(CameraTarget { zoom: Some(-5.0), ..Default::default() });
        assert!((c.camera().zoom + 5.0).abs() < 1e-9, "zoom {}", c.camera().zoom);
    }

    /// A host that eases on `FeatureEvent::Click` starts the ease between
    /// the press and the release; the release must not stop it.
    #[test]
    fn an_ease_started_during_a_press_survives_the_release() {
        let mut view = CameraController::new(
            Camera {
                center_lon: -96.0,
                center_lat: 38.5,
                zoom: 3.7,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1024.0,
                viewport_height_px: 768.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        view.apply(CameraIntent::GestureBegan);
        view.ease_to(CameraTarget { zoom: Some(6.0), ..CameraTarget::default() }, std::time::Duration::from_millis(500));
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        assert!(!view.is_settled(), "the release left the ease running");
        view.apply(CameraIntent::GestureBegan);
        assert!(view.is_settled() || view.motion.is_none(), "a new press still stops it dead");
    }

    #[test]
    fn a_drag_that_begins_during_the_press_still_cancels_an_ease_started_on_that_press() {
        let mut view = CameraController::new(
            Camera {
                center_lon: -96.0,
                center_lat: 38.5,
                zoom: 3.7,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1024.0,
                viewport_height_px: 768.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        view.apply(CameraIntent::GestureBegan);
        view.ease_to(CameraTarget { zoom: Some(6.0), ..CameraTarget::default() }, std::time::Duration::from_millis(500));
        assert!(view.motion.is_some(), "the ease is running before the drag");
        // The press turned into a drag before it was released -- MapView
        // delivers this as a `PanBy` while the gesture is still down.
        view.apply(CameraIntent::PanBy { dx_px: 12.0, dy_px: 0.0, anchor_px: (512.0, 384.0) });
        assert!(view.motion.is_none(), "a drag beginning during the press cancels the press-started ease");
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        assert!(view.motion.is_none(), "the release must not resurrect a motion the drag already cancelled");
    }

    #[test]
    fn a_tap_is_not_camera_input_but_a_pan_is() {
        let mut view = CameraController::new(
            Camera {
                center_lon: 103.8198,
                center_lat: 1.3521,
                zoom: 14.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1024.0,
                viewport_height_px: 768.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        let before = view.input_generation();
        view.apply(CameraIntent::GestureBegan);
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        assert_eq!(view.input_generation(), before, "a tap -- on the map or on a chrome button -- moved nothing");
        view.apply(CameraIntent::PanBy { dx_px: 1.0, dy_px: 0.0, anchor_px: (512.0, 384.0) });
        assert_eq!(view.input_generation(), before + 1);
        view.jump_to(CameraTarget { zoom: Some(10.0), ..CameraTarget::default() });
        assert_eq!(view.input_generation(), before + 1, "a programmatic move is not input");
    }

    use std::time::Duration;

    /// `src/map_view.rs`'s `ZOOM_LEVELS_PER_SCROLL_LINE`. Native converts
    /// scroll lines to zoom LEVELS before it reaches the controller; the
    /// moved tests were written in lines.
    const LINE: f64 = 0.8;

    /// The Shift+scroll axis lock: nothing inside the dead zone, then the
    /// dominant axis for the rest of the gesture -- a vertical swipe's
    /// sideways drift never rotates.
    #[test]
    fn shift_scroll_commits_to_one_axis_after_the_dead_zone() {
        let mut lock = ScrollAxisLock::default();
        assert_eq!(shift_scroll_intent(ScrollTravel::Pixels { dx: 1.0, dy: -4.0 }, &mut lock), None, "inside the dead zone");
        assert_eq!(
            shift_scroll_intent(ScrollTravel::Pixels { dx: 1.0, dy: -4.0 }, &mut lock),
            Some(CameraIntent::TiltBy { radians: 4.0 * SCROLL_PITCH_RADIANS_PER_PIXEL }),
            "committed to tilt; up (negative dy) tilts in"
        );
        assert_eq!(shift_scroll_intent(ScrollTravel::Pixels { dx: 30.0, dy: 0.0 }, &mut lock), None, "locked: no rotate");
        let mut sideways = ScrollAxisLock::default();
        assert_eq!(
            shift_scroll_intent(ScrollTravel::Pixels { dx: 10.0, dy: 1.0 }, &mut sideways),
            Some(CameraIntent::RotateBy { radians: 10.0 * SCROLL_BEARING_RADIANS_PER_PIXEL, anchor_px: None })
        );
        assert_eq!(
            shift_scroll_intent(ScrollTravel::Lines { dx: 0.0, dy: -2.0 }, &mut ScrollAxisLock::default()),
            Some(CameraIntent::TiltStep { radians: 2.0 * KEYBOARD_PITCH_STEP })
        );
        assert_eq!(shift_scroll_intent(ScrollTravel::Pixels { dx: f32::NAN, dy: 0.0 }, &mut ScrollAxisLock::default()), None);
    }

    #[test]
    fn an_orbit_drag_rotates_and_tilts_together_at_maplibre_rates() {
        assert_eq!(
            orbit_drag_intents(10.0, -20.0),
            vec![
                CameraIntent::RotateBy { radians: 10.0 * DRAG_BEARING_RADIANS_PER_PIXEL, anchor_px: None },
                CameraIntent::TiltBy { radians: 20.0 * DRAG_PITCH_RADIANS_PER_PIXEL },
            ]
        );
        assert!(orbit_drag_intents(0.0, 0.0).is_empty());
    }

    #[test]
    fn two_finger_intents_turns_each_nonzero_part_into_one_intent_in_order() {
        let step = crate::touch_gesture::TwoFingerStep {
            pan_px: (3.0, -2.0),
            pan_anchor_px: (100.0, 200.0),
            zoom_levels: 0.25,
            rotate_radians: -0.1,
            tilt_px: 0.0,
            anchor_px: (103.0, 198.0),
        };
        assert_eq!(
            two_finger_intents(&step),
            vec![
                CameraIntent::PanBy { dx_px: 3.0, dy_px: -2.0, anchor_px: (100.0, 200.0) },
                CameraIntent::RotateBy { radians: -0.1, anchor_px: Some((103.0, 198.0)) },
                CameraIntent::PinchZoomBy { levels: 0.25, anchor_px: (103.0, 198.0) },
            ]
        );
        let tilt = crate::touch_gesture::TwoFingerStep { tilt_px: -10.0, ..Default::default() };
        assert_eq!(two_finger_intents(&tilt), vec![CameraIntent::TiltBy { radians: tilt_radians_for_drag_px(-10.0) }]);
        assert!(two_finger_intents(&crate::touch_gesture::TwoFingerStep::default()).is_empty());
    }

    /// Singapore at z14, 1024x768, pitched through `jump_to` so the
    /// orientation target agrees and `advance` does not ease it back.
    fn pinch_controller(pitch: f32) -> CameraController {
        let mut view = zoom_anchor_test_controller(14.0);
        view.jump_to(CameraTarget { pitch: Some(pitch), ..CameraTarget::default() });
        assert!(view.is_settled());
        view
    }

    /// T8 fix round 0: a narrowed zoom range holds every input at its end
    /// (no overshoot, so nothing to bounce back from), and a camera outside
    /// the new range eases onto it rather than jumping.
    #[test]
    fn a_narrowed_zoom_range_clamps_input_and_eases_the_camera_in() {
        let mut view = test_controller(16.0, 0.0, (1024.0, 768.0));
        view.set_zoom_range(3.0, 14.0);
        assert_eq!(view.zoom_range(), (3.0, 14.0));
        assert_eq!(view.camera().zoom, 16.0, "not a jump");
        assert!(view.advance(Duration::from_millis(16)), "easing in");
        let first = view.camera().zoom;
        assert!(first < 16.0 && first > 14.0, "part of the way after one frame: {first}");
        for _ in 0..200 {
            view.advance(Duration::from_millis(16));
        }
        assert!((view.camera().zoom - 14.0).abs() < 1e-9, "{}", view.camera().zoom);
        // A wheel past the end: the target never leaves the range.
        view.zoom_by(3.0);
        assert_eq!(view.zoom_target.zoom, 14.0);
        for _ in 0..50 {
            view.advance(Duration::from_millis(16));
            assert!(view.camera().zoom <= 14.0 + 1e-12, "overshoot {}", view.camera().zoom);
        }
        view.apply(CameraIntent::PinchZoomBy { levels: -100.0, anchor_px: (300.0, 200.0) });
        assert_eq!(view.camera().zoom, 3.0);
        view.jump_to(CameraTarget { zoom: Some(18.0), ..Default::default() });
        assert_eq!(view.camera().zoom, 14.0);
        // A wheel ease in flight past the new end goes on to the end.
        view.set_zoom_range(MIN_ZOOM, MAX_ZOOM);
        view.jump_to(CameraTarget { zoom: Some(12.0), ..Default::default() });
        view.zoom_by(4.0);
        view.advance(Duration::from_millis(16));
        let mid = view.camera().zoom;
        assert!(mid > 12.0 && mid < 14.0, "part way: {mid}");
        view.set_zoom_range(3.0, 14.0);
        assert_eq!(view.zoom_target.zoom, 14.0, "the ease's target clamps to the end");
        for _ in 0..200 {
            view.advance(Duration::from_millis(16));
        }
        assert!((view.camera().zoom - 14.0).abs() < 1e-9, "{}", view.camera().zoom);
        // Ignored: inverted or non-finite.
        view.set_zoom_range(10.0, 5.0);
        view.set_zoom_range(f64::NAN, 5.0);
        assert_eq!(view.zoom_range(), (3.0, 14.0));
        // Restored to the full range.
        view.set_zoom_range(MIN_ZOOM, MAX_ZOOM);
        view.jump_to(CameraTarget { zoom: Some(18.0), ..Default::default() });
        assert_eq!(view.camera().zoom, 18.0);
    }

    #[test]
    fn pinch_zooms_in_one_frame_accumulate_to_the_sum_of_their_levels() {
        // iOS and the browser deliver one finger per event, so a frame
        // carries several pinch updates before the next `advance`. Each must
        // land in full -- an eased zoom rebased on the rendered zoom would
        // keep only the last.
        let mut view = pinch_controller(0.0);
        view.apply(CameraIntent::PinchZoomBy { levels: 0.25, anchor_px: (300.0, 200.0) });
        view.apply(CameraIntent::PinchZoomBy { levels: 0.125, anchor_px: (310.0, 205.0) });
        assert!((view.camera().zoom - 14.375).abs() < 1e-12, "{}", view.camera().zoom);
        // Clamped to the controller's range like every other zoom.
        view.apply(CameraIntent::PinchZoomBy { levels: 100.0, anchor_px: (300.0, 200.0) });
        assert_eq!(view.camera().zoom, view.zoom_range.1);
    }

    #[test]
    fn a_pinch_zoom_holds_the_ground_under_the_fingers_flat_and_tilted() {
        for pitch in [0.0f32, 0.7] {
            let mut view = pinch_controller(pitch);
            let anchor = (250.0f32, 520.0f32);
            let held = crate::render::unproject_to_ground_f64(view.camera(), anchor.0, anchor.1).expect("ground");
            for levels in [0.3, 0.2, -0.15, 0.4] {
                view.apply(CameraIntent::PinchZoomBy { levels, anchor_px: anchor });
            }
            let (x, y) = crate::render::project_world_to_screen_f64(view.camera(), (held.x, held.y)).expect("visible");
            let slip = (f64::from(x) - f64::from(anchor.0)).hypot(f64::from(y) - f64::from(anchor.1));
            assert!(slip < 0.5, "pitch {pitch}: the ground under the pinch slipped {slip:.4} px");
            assert!((view.camera().zoom - 14.75).abs() < 1e-9, "{}", view.camera().zoom);
        }
    }

    #[test]
    fn a_pinch_zoom_leaves_nothing_for_advance_to_chase() {
        let mut view = pinch_controller(0.7);
        view.ease_to(CameraTarget { zoom: Some(10.0), ..CameraTarget::default() }, Duration::from_millis(500));
        view.apply(CameraIntent::PinchZoomBy { levels: 0.5, anchor_px: (700.0, 300.0) });
        let after = *view.camera();
        assert!(view.is_settled(), "a live pinch cancels the ease and leaves no zoom target behind");
        assert!(!view.advance(Duration::from_millis(16)));
        assert_eq!(*view.camera(), after, "advance moved a camera that had nothing to do");
    }

    /// `map_view`'s `test_map_view`, minus the tokio runtime and the tile
    /// source a controller has no use for. Sets `camera.pitch` WITHOUT the
    /// target, exactly as the original did, so the moved tests see the same
    /// state they always saw.
    pub(super) fn test_controller(zoom: f64, pitch: f32, viewport_px: (f64, f64)) -> CameraController {
        let mut view = CameraController::new(
            Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: viewport_px.0 as f32,
                viewport_height_px: viewport_px.1 as f32,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            0.0,
            20.0,
        );
        view.camera.pitch = pitch;
        view
    }

    /// `map_view`'s `zoom_anchor_test_view`: Singapore, 1024x768.
    fn zoom_anchor_test_controller(zoom: f64) -> CameraController {
        CameraController::new(
            Camera {
                center_lon: 103.8198,
                center_lat: 1.3521,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1024.0,
                viewport_height_px: 768.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            0.0,
            20.0,
        )
    }

    #[test]
    fn a_wrapped_centre_reads_as_the_western_hemisphere_not_as_past_180() {
        // The choice of range, pinned. `lon.rem_euclid(360.0)` also stops the
        // unbounded growth, but it normalises into [0, 360) -- so a camera
        // over Honolulu would be persisted, logged and compared as 202.14
        // instead of -157.86, a hemisphere off in every one of those. An
        // in-range longitude must come back bit-identical: the wrap is a
        // repair for out-of-range values, not a rounding pass.
        let mut view = test_controller(4.0, 0.0, (1024.0, 768.0));
        view.jump_to(CameraTarget { center: Some((202.14, 21.30)), zoom: Some(4.0), ..CameraTarget::default() });
        assert!((view.camera.center_lon - (-157.86)).abs() < 1e-9, "got {}", view.camera.center_lon);
        assert_eq!(view.zoom_target.center_lon, view.camera.center_lon, "jump_to writes both");
        view.jump_to(CameraTarget { center: Some((103.85, 1.29)), zoom: Some(4.0), ..CameraTarget::default() });
        assert_eq!((view.camera.center_lon, view.zoom_target.center_lon), (103.85, 103.85));
    }

    #[test]
    fn apply_routes_every_intent_to_the_rule_it_names() {
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.apply(CameraIntent::TiltBy { radians: 0.3 });
        assert_eq!(view.camera.pitch, 0.3);
        view.apply(CameraIntent::RotateBy { radians: 0.5, anchor_px: None });
        assert_eq!(view.camera.bearing, 0.5);
        assert_eq!(view.target.bearing, 0.5, "a live rotate collapses the ease target onto the camera");
        view.apply(CameraIntent::RotateStep { radians: KEYBOARD_ROTATE_STEP });
        assert!(view.target.bearing > view.camera.bearing, "a step eases; it does not jump");
        view.apply(CameraIntent::ResetNorthAndPitch);
        assert_eq!((view.target.bearing, view.target.pitch), (0.0, 0.0));
        view.apply(CameraIntent::ZoomAbout { levels: 1.0, anchor_px: (512.0, 384.0), immediate: false });
        assert_eq!(view.zoom_target.zoom, 11.0);
        assert_eq!(view.zoom_target.ease_rate, ZOOM_EASE_RATE_DISCRETE);
    }

    #[test]
    fn release_snaps_to_north_only_after_a_gesture_that_rotated() {
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.apply(CameraIntent::GestureBegan);
        view.apply(CameraIntent::RotateBy { radians: 3.0_f32.to_radians(), anchor_px: None });
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        assert_eq!(view.target.bearing, 0.0, "a rotation that ends 3 degrees off north settles on north");

        // A pan-only gesture at the same bearing must not rotate the map.
        let mut panned = test_controller(10.0, 0.0, (1024.0, 768.0));
        panned.apply(CameraIntent::RotateBy { radians: 3.0_f32.to_radians(), anchor_px: None });
        panned.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 }); // consumes that rotation
        panned.apply(CameraIntent::GestureBegan);
        panned.apply(CameraIntent::PanBy { dx_px: 40.0, dy_px: 0.0, anchor_px: (512.0, 384.0) });
        // That first release snapped the TARGET to north. Put it back on the
        // camera's 3 degrees, so a wrongful snap below has something to undo:
        // left at 0.0, a snap and a no-snap would read the same.
        panned.target.bearing = panned.camera.bearing;
        let before = panned.target.bearing;
        assert!(before > 2.0_f32.to_radians(), "sanity: the target is off north, at {before}");
        panned.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        assert_eq!(panned.target.bearing, before, "a gesture that never rotated has nothing to snap");
        assert_ne!(panned.target.bearing, 0.0, "the no-rotate release must NOT snap to north");
    }

    #[test]
    fn advance_runs_both_eases_and_reports_settled_only_when_both_are() {
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.apply(CameraIntent::RotateStep { radians: KEYBOARD_ROTATE_STEP });
        view.apply(CameraIntent::ZoomAbout { levels: 1.0, anchor_px: (512.0, 384.0), immediate: false });
        assert!(!view.is_settled());
        let mut frames = 0;
        while view.advance(Duration::from_millis(16)) {
            frames += 1;
            assert!(frames < 600, "advance never settled");
        }
        assert!(view.is_settled());
        assert_eq!(view.camera.zoom, 11.0);
    }

    #[test]
    fn a_ten_second_gap_moves_an_ease_no_further_than_one_max_advance_step() {
        // The first frame after idle (window occluded, iPhone asleep) must
        // not replay the curve in one jump. Without the clamp the 10 s frame
        // converges both eases outright, and neither equality below holds.
        let mut a = test_controller(10.0, 0.0, (1024.0, 768.0));
        let mut b = test_controller(10.0, 0.0, (1024.0, 768.0));
        a.apply(CameraIntent::RotateStep { radians: 1.0 });
        b.apply(CameraIntent::RotateStep { radians: 1.0 });
        a.apply(CameraIntent::ZoomAbout { levels: 1.0, anchor_px: (512.0, 384.0), immediate: false });
        b.apply(CameraIntent::ZoomAbout { levels: 1.0, anchor_px: (512.0, 384.0), immediate: false });
        assert_eq!(MAX_ADVANCE_STEP, Duration::from_millis(50));
        assert!(a.advance(Duration::from_secs(10)), "one clamped step must leave the eases still moving");
        b.advance(Duration::from_millis(50));
        assert_eq!(a.camera.bearing, b.camera.bearing, "a 10 s gap must move the ease exactly one 50 ms step");
        assert_eq!(a.camera.zoom, b.camera.zoom, "and the zoom ease the same");
    }

    #[test]
    fn a_rotation_settling_near_north_snaps_to_it_but_a_deliberate_one_does_not() {
        // Moved verbatim from `src/map_view.rs` (was :14974).
        assert_eq!(bearing_after_north_snap(3.0_f32.to_radians()), 0.0);
        assert_eq!(bearing_after_north_snap((-3.0_f32).to_radians()), 0.0);
        let deliberate = 20.0_f32.to_radians();
        assert!(
            (bearing_after_north_snap(deliberate) - deliberate).abs() < 1e-6,
            "a deliberate 20-degree bearing must not be snapped away"
        );
        assert_eq!(bearing_after_north_snap(359.0_f32.to_radians()), 0.0);
    }

    #[test]
    fn wrap_bearing_is_half_open_and_signed() {
        let pi = std::f32::consts::PI;
        assert_eq!(wrap_bearing(0.0), 0.0);
        assert!((wrap_bearing(pi) - (-pi)).abs() < 1e-6, "+PI is outside [-PI, PI) and must fold to -PI");
        for b in [-1000.0f32, -3.2, -0.001, 3.1, 6.3, 1000.0] {
            let w = wrap_bearing(b);
            assert!((-pi..pi).contains(&w), "{b} wrapped to {w}");
        }
    }

    #[test]
    fn wrap_lon_passes_an_in_range_longitude_through_bit_identical() {
        assert_eq!(wrap_lon(103.85), 103.85);
        assert!((wrap_lon(202.14) - (-157.86)).abs() < 1e-9);
        assert!((shortest_lon_delta(179.9, -179.9) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn drag_conversions_match_maplibre_rates_and_signs() {
        // MapLibre: bearing +0.8 deg per px right, pitch -0.5 deg per px down
        // (`src/ui/handler/mouse.ts`, `rotateSpeed = 0.8`, `pitchSpeed = -0.5`).
        assert!((rotate_radians_for_drag_px(10.0) - 8.0_f32.to_radians()).abs() < 1e-6);
        assert!((tilt_radians_for_drag_px(-10.0) - 5.0_f32.to_radians()).abs() < 1e-6, "dragging UP tilts into 3D");
    }

    #[test]
    fn repeated_rotation_keeps_bearing_bounded_instead_of_accumulating() {
        // The latent bug this fixes: `tilt_by` used to do a bare
        // `self.camera.bearing += delta_bearing` with no wrap anywhere in
        // the file, so bearing grew without bound across repeated
        // rotations. The render math is periodic and never complained,
        // which is exactly why it went unnoticed -- but an `f32` holding a
        // few thousand radians has lost most of its precision, so a
        // long-lived map's rotation would visibly quantize.
        //
        // Forty full turns is roughly a minute of enthusiastic dragging.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        for _ in 0..40 {
            for _ in 0..8 {
                view.tilt_by(0.0, std::f32::consts::FRAC_PI_4);
            }
        }
        assert!(
            view.camera.bearing.abs() <= std::f32::consts::PI,
            "bearing accumulated to {} instead of staying wrapped",
            view.camera.bearing
        );
        // Forty whole turns should also land back where it started, which
        // it only does if precision was preserved along the way.
        assert!(
            view.camera.bearing.abs() < 1e-3,
            "40 full turns should return to north, got {}",
            view.camera.bearing
        );
    }

    /// The longitude of the ground point under `cursor`, read back out of the
    /// live camera. Deliberately geographic rather than world-space: an
    /// anchored zoom stores its anchor as a world point, so comparing world
    /// x against world x cannot tell a correctly-held anchor from one the
    /// wrap has quietly moved a whole world copy. Longitude, compared with
    /// `shortest_lon_delta`, is the same question asked where the wrap is
    /// visible.
    fn ground_lon_under_cursor(view: &CameraController, cursor: (f32, f32)) -> f64 {
        let p = crate::render::unproject_to_ground_f64(&view.camera, cursor.0, cursor.1)
            .expect("an unpitched camera always has ground under the cursor");
        let (lon, _) = crate::geo::tile_xy_to_lon_lat(
            p.x / crate::render::WORLD_SIZE as f64,
            p.y / crate::render::WORLD_SIZE as f64,
            0.0,
        );
        lon
    }

    #[test]
    fn panning_east_past_the_antimeridian_wraps_instead_of_accumulating() {
        // The longitude twin of the bearing bug above: every write to
        // `camera.center_lon` used to be a raw assignment, so a drag that
        // kept going east just kept adding: the four drags below used to
        // leave the centre at 216.12 degrees, and eight at 432.25. Rendering
        // never complained -- `lon_lat_to_tile_xy` is happy to hand back an
        // off-the-map tile x and the visible-tile clamp absorbs it -- but
        // every consumer that
        // reads the camera back (a consumer's track player saves its position on a
        // timer) gets a number it has to repair itself.
        let mut view = test_controller(1.38, 0.0, (1024.0, 768.0));
        // The yardstick below (54 degrees per 200 px) is FLAT arithmetic. At
        // z1.38 the planet is ~210 px in radius, so on the globe a 200 px
        // drag from the centre runs off its limb into
        // `pan_surface_point`'s falloff, a trackball turn of ~79 degrees.
        // The wrap under test is the same either way; it is pinned flat.
        view.set_globe_enabled(false);
        assert_eq!(view.camera.center_lon, 0.0, "sanity: this view starts on the prime meridian");

        // Driven through `pan_by_pixels`, which is exactly what the
        // mouse-move handler calls with the cursor delta -- not by assigning
        // the field, which would prove nothing about the real path. A
        // NEGATIVE dx drags the map west and so walks the centre EAST.
        let mut after_each = Vec::new();
        for drag in 1..=8 {
            view.pan_by_pixels(-200.0, 0.0, (512.0, 384.0));
            let lon = view.camera.center_lon;
            assert!(
                (-180.0..180.0).contains(&lon),
                "after {drag} eastward drags the centre read {lon}, outside [-180, 180)"
            );
            after_each.push(lon);
        }

        // 200px at zoom 1.38 is ~54 degrees (360 degrees spans
        // TILE_SIZE * 2^1.38 = 1332px), so the first drag lands well inside
        // range and gives the yardstick the rest are measured against. Four
        // drags pass +180; eight pass it by 72 degrees more. Wrapping must
        // subtract exactly one turn, not clamp and not lose travel.
        let per_drag = after_each[0];
        assert!(
            (50.0..60.0).contains(&per_drag),
            "expected ~54 degrees per drag at zoom 1.38, got {per_drag}"
        );
        assert!(
            (after_each[3] - (4.0 * per_drag - 360.0)).abs() < 1e-9,
            "four drags east should read {} (that is 4 x {per_drag}, one turn taken off), got {}",
            4.0 * per_drag - 360.0,
            after_each[3]
        );
        assert!(after_each[3] < 0.0, "just past the antimeridian is the WESTERN hemisphere");
        assert!(
            (after_each[7] - (8.0 * per_drag - 360.0)).abs() < 1e-9,
            "eight drags east should read {} , got {}",
            8.0 * per_drag - 360.0,
            after_each[7]
        );
        assert_eq!(
            view.zoom_target.center_lon, view.camera.center_lon,
            "pan keeps the ease target in lockstep with the centre; wrapping one and not the \
             other would leave `advance_zoom_animation` a whole turn to unwind"
        );
    }

    #[test]
    fn the_zoom_ease_crosses_the_antimeridian_the_short_way_round() {
        // The hazard wrapping introduces, and the reason `shortest_lon_delta`
        // exists: once both ends are wrapped, easing from 179.9 to -179.9 is
        // a 0.2-degree nudge east, but the raw difference is -359.8, and an
        // ease driven by the raw difference drags the camera west across the
        // entire globe to arrive next door. Exactly the failure
        // `shortest_bearing_delta` was written for, one field over.
        let mut view = test_controller(4.0, 0.0, (1024.0, 768.0));
        view.jump_to(CameraTarget { center: Some((179.9, 1.35)), zoom: Some(4.0), ..CameraTarget::default() });
        // The unanchored branch is the one that interpolates lon/lat, so the
        // anchor has to be clear for this to be the path under test.
        assert!(view.zoom_target.anchor.is_none(), "`set_camera` drops the anchor");
        view.zoom_target.center_lon = -179.9;

        for _ in 0..120 {
            if !view.advance_zoom_animation(Duration::from_millis(16)) {
                break;
            }
            assert!(
                view.camera.center_lon.abs() > 170.0,
                "the ease took the long way round, reaching {}",
                view.camera.center_lon
            );
        }
        assert!(
            shortest_lon_delta(view.camera.center_lon, -179.9).abs() < 1e-6,
            "the ease did not converge, stopping at {}",
            view.camera.center_lon
        );
        assert!(
            (-180.0..180.0).contains(&view.camera.center_lon),
            "the eased centre left the range, at {}",
            view.camera.center_lon
        );
    }

    #[test]
    fn an_anchored_zoom_across_the_antimeridian_holds_its_anchor() {
        // The other hazard, and the one wrapping creates rather than
        // inherits. An anchored zoom's `zoom_anchor` is a WORLD point
        // unprojected from the camera at gesture start, and
        // `center_anchored_at` works on the difference between it and the
        // camera's own world position. Wrapping the camera's longitude moves
        // that world position by a whole WORLD_SIZE; if the anchor does not
        // travel with it, the offset the formula sees is off by the width of
        // the world and the camera is flung half way round it.
        //
        // Zoom in at a cursor near the right edge, from a camera 0.05 degrees
        // west of the antimeridian: at zoom 4 the cursor sits ~21 degrees
        // east of centre, and 1.6 zoom levels pulls the centre ~two thirds of
        // the way there, straight across 180.
        let mut view = zoom_anchor_test_controller(4.0);
        // The FLAT anchor bookkeeping this test is about runs only on a flat
        // frame; at z4 that is a map with the globe off. The globe solves
        // its centre in lon/lat and has no world copy to carry
        // (`globe_gesture_tests`). Plain's world bounds would stop the
        // camera at the edge (`plain_constrain`): this is the arithmetic
        // the globe's own flat regime uses across 180, so they are off.
        view.plain_bounds = false;
        view.set_globe_enabled(false);
        view.jump_to(CameraTarget { center: Some((179.95, 1.3521)), zoom: Some(4.0), ..CameraTarget::default() });
        let cursor = (1000.0f32, 384.0f32);
        let anchor_lon_before = ground_lon_under_cursor(&view, cursor);
        assert!(
            anchor_lon_before > 180.0,
            "this test is only meaningful with the anchor beyond the antimeridian, at {anchor_lon_before}"
        );

        view.zoom_towards_point(2.0 * LINE, cursor.0, cursor.1, true);
        assert!(view.zoom_target.anchor.is_some(), "an anchored zoom should be in flight");

        let mut worst_drift_px = 0.0f64;
        for _ in 0..120 {
            let still_easing = view.advance_zoom_animation(Duration::from_millis(16));
            assert!(
                (-180.0..180.0).contains(&view.camera.center_lon),
                "mid-ease the centre read {}, outside [-180, 180)",
                view.camera.center_lon
            );
            // Degrees of drift, converted to the pixels the user would
            // actually see it as at the zoom the camera has reached.
            let drift_deg = shortest_lon_delta(anchor_lon_before, ground_lon_under_cursor(&view, cursor)).abs();
            let px_per_degree = crate::geo::TILE_SIZE * 2f64.powf(view.camera.zoom) / 360.0;
            worst_drift_px = worst_drift_px.max(drift_deg * px_per_degree);
            if !still_easing {
                break;
            }
        }
        // The tolerance the other anchored-zoom tests in this file use. The
        // measured worst frame is 3.14e-11 px, and the same gesture run from
        // lon 100, nowhere near the wrap, measures 1.23e-10 px -- so crossing
        // the antimeridian costs the anchor nothing, and both numbers are
        // just f64 noise.
        assert!(
            worst_drift_px < 0.01,
            "the anchored point slid {worst_drift_px:e}px away from the cursor across the wrap"
        );
        assert!(
            view.camera.center_lon < 0.0,
            "the gesture was meant to cross the antimeridian; it ended at {}",
            view.camera.center_lon
        );
    }

    #[test]
    fn the_orientation_ease_crosses_the_wrap_the_short_way_round() {
        // Easing from 179 to -179 degrees is a 2-degree nudge, but the raw
        // difference is -358 degrees. An ease driven by the raw difference
        // spins the map almost the whole way around to arrive next door --
        // the reason `shortest_bearing_delta` exists.
        let from = 179.0_f32.to_radians();
        let to = (-179.0_f32).to_radians();
        let delta = shortest_bearing_delta(from, to);
        assert!(
            delta.abs() < 3.0_f32.to_radians(),
            "expected a ~2 degree crossing, got {} degrees",
            delta.to_degrees()
        );

        // And the same thing through the real ease, one frame at a time:
        // the camera must never wander far from the 180-degree line.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.camera.bearing = from;
        view.target.bearing = to;
        for _ in 0..120 {
            if !view.advance_orientation_animation(Duration::from_millis(16)) {
                break;
            }
            assert!(
                view.camera.bearing.abs() > 170.0_f32.to_radians(),
                "the ease took the long way round, reaching {} degrees",
                view.camera.bearing.to_degrees()
            );
        }
        assert!(
            shortest_bearing_delta(view.camera.bearing, to).abs() < 1e-3,
            "ease did not converge, at {} degrees",
            view.camera.bearing.to_degrees()
        );
    }

    #[test]
    fn the_orientation_ease_settles_and_stops_asking_for_frames() {
        // An exponential decay never reaches its target in finite time, so
        // without the epsilon snap this would hold the frame loop open
        // forever chasing a sub-microradian residual.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.rotate_target_by(KEYBOARD_ROTATE_STEP);
        let mut frames = 0;
        while view.advance_orientation_animation(Duration::from_millis(16)) {
            frames += 1;
            assert!(frames < 600, "orientation ease never settled");
        }
        assert!(frames > 1, "a keyboard step should glide, not snap in one frame");
        assert_eq!(
            view.camera.bearing, view.target.bearing,
            "a settled ease must land exactly on the target"
        );
    }

    #[test]
    fn twentyfour_keyboard_steps_return_to_exactly_north() {
        // `KEYBOARD_ROTATE_STEP` divides the circle exactly 24 times, which
        // is what stops keyboard-only rotation from accumulating a residual
        // the user has no way to clear by eye.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        for _ in 0..24 {
            view.rotate_target_by(KEYBOARD_ROTATE_STEP);
        }
        assert!(
            view.target.bearing.abs() < 1e-4,
            "24 keyboard steps should close the circle, got {} degrees",
            view.target.bearing.to_degrees()
        );
    }

    #[test]
    fn panning_a_rotated_map_moves_it_in_the_direction_the_screen_says() {
        // `pan_by_pixels` used to push the screen delta straight into tile
        // space with no rotation, so a rotated map panned off by exactly the
        // bearing angle. Reachable on master through the existing Shift+drag
        // rotate, and invisible only because the correction is the identity
        // at bearing 0.
        //
        // The load-bearing case, and the one that pins the SIGN: at bearing
        // 90 degrees the camera faces east, so screen-left is due north and
        // a rightward drag must walk the centre due north with longitude
        // unchanged. Cross-checked against the iOS shell's independently
        // measured result for the same gesture.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        // Screen-direction semantics of the FLAT drag. On the globe (z10
        // with it on) a drag holds the grabbed place under the cursor
        // instead, which moves latitude on a horizontal drag off the
        // centre's parallel, exactly as MapLibre's globe does.
        view.set_globe_enabled(false);
        view.camera.center_lon = 103.8198;
        view.camera.center_lat = 1.3521;
        let (lon_before, lat_before) = (view.camera.center_lon, view.camera.center_lat);
        view.camera.bearing = std::f32::consts::FRAC_PI_2;
        view.pan_by_pixels(200.0, 0.0, (512.0, 384.0));
        // Not an exact zero: `bearing` is an `f32`, so `cos(PI/2)` is about
        // -4.4e-8 rather than 0, and that leaks a few nanodegrees of
        // longitude — roughly a micrometre on the ground. The tolerance
        // measures the sign convention, which is what this test is for, not
        // the float type.
        assert!(
            (view.camera.center_lon - lon_before).abs() < 1e-7,
            "a rightward drag at bearing 90 must not change longitude, moved {} degrees",
            view.camera.center_lon - lon_before
        );
        assert!(
            view.camera.center_lat > lat_before,
            "a rightward drag at bearing 90 must move the centre NORTH, went from {lat_before} to {}",
            view.camera.center_lat
        );

        // And the correction must be the exact identity at north, so this
        // changes nothing at all for a map that is never rotated.
        let mut flat = test_controller(10.0, 0.0, (1024.0, 768.0));
        flat.set_globe_enabled(false);
        flat.camera.center_lon = 103.8198;
        flat.camera.center_lat = 1.3521;
        let lat_at_north = flat.camera.center_lat;
        flat.pan_by_pixels(200.0, 0.0, (512.0, 384.0));
        assert!(
            (flat.camera.center_lat - lat_at_north).abs() < 1e-12,
            "at bearing 0 a horizontal drag must leave latitude untouched"
        );
        assert!(
            flat.camera.center_lon < 103.8198,
            "at bearing 0 a rightward drag must move the centre west"
        );

        // A full turn is the identity too -- which is only true if bearing
        // stays wrapped, tying this to `wrap_bearing`.
        let mut turned = test_controller(10.0, 0.0, (1024.0, 768.0));
        turned.set_globe_enabled(false);
        turned.camera.center_lon = 103.8198;
        turned.camera.center_lat = 1.3521;
        turned.camera.bearing = wrap_bearing(std::f32::consts::TAU);
        turned.pan_by_pixels(200.0, 0.0, (512.0, 384.0));
        // Same f32 caveat as above: `wrap_bearing(TAU)` lands on about
        // -2e-7 rather than a clean zero, so this asserts "indistinguishable
        // on the ground", not bit equality.
        assert!(
            (turned.camera.center_lon - flat.camera.center_lon).abs() < 1e-6
                && (turned.camera.center_lat - flat.camera.center_lat).abs() < 1e-6,
            "a full turn must pan identically to no turn at all, differed by ({}, {})",
            turned.camera.center_lon - flat.camera.center_lon,
            turned.camera.center_lat - flat.camera.center_lat
        );
    }

    #[test]
    fn dragging_up_tilts_into_3d_rather_than_doing_nothing() {
        // The map starts flat at pitch 0, so a tilt gesture whose sign is
        // inverted is not merely unconventional -- it is a silent no-op
        // against the clamp, and the first thing a user reaches for gives
        // no feedback at all. That was the previous behaviour, observed in
        // the running app: an upward Shift+drag changed nothing.
        //
        // Upward drag means NEGATIVE dy in screen coordinates, which is why
        // the handler negates it. MapLibre's drag-pitch is likewise
        // `-deltaY * 0.5`, and Google Maps and Apple Maps both tilt on an
        // upward drag.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        assert_eq!(view.camera.pitch, 0.0, "the map must start flat for this to mean anything");
        let dy_upward = -100.0_f32;
        view.tilt_by(-dy_upward * DRAG_PITCH_RADIANS_PER_PIXEL, 0.0);
        assert!(
            view.camera.pitch > 0.5,
            "a 100px upward drag should tilt the map into 3D, got pitch {}",
            view.camera.pitch
        );
        // And the keyboard must agree with the drag, or the two gestures
        // teach the user opposite things.
        let mut keyboard_view = test_controller(10.0, 0.0, (1024.0, 768.0));
        keyboard_view.pitch_target_by(KEYBOARD_PITCH_STEP);
        assert!(
            keyboard_view.target.pitch > 0.0,
            "Shift+Up must increase pitch, the same direction an upward drag does"
        );
    }

    #[test]
    fn a_live_drag_overrides_an_in_flight_reset_instead_of_fighting_it() {
        // `tilt_by` collapses the ease targets onto the camera, so grabbing
        // the map mid-reset hands control straight back to the hand rather
        // than leaving the ease pulling the other way.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.camera.bearing = 1.0;
        view.target.bearing = 1.0;
        view.reset_north();
        assert_eq!(view.target.bearing, 0.0);
        view.advance_orientation_animation(Duration::from_millis(16));
        // The user grabs it mid-glide.
        view.tilt_by(0.0, 0.2);
        assert_eq!(
            view.target.bearing, view.camera.bearing,
            "a direct-manipulation rotation must cancel the in-flight ease"
        );
        assert!(
            !view.advance_orientation_animation(Duration::from_millis(16)),
            "with the target collapsed onto the camera the ease must report itself finished"
        );
    }

    #[test]
    fn reset_north_and_pitch_levels_the_map_as_well_as_facing_it_north() {
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.tilt_by(0.6, 1.2);
        view.reset_north_and_pitch();
        for _ in 0..600 {
            if !view.advance_orientation_animation(Duration::from_millis(16)) {
                break;
            }
        }
        assert!(view.camera.bearing.abs() < 1e-4, "bearing {}", view.camera.bearing);
        assert!(view.camera.pitch.abs() < 1e-4, "pitch {}", view.camera.pitch);
    }

    #[test]
    fn the_pitch_ease_target_is_clamped_to_the_pitch_the_camera_can_reach() {
        // If `target_pitch` could exceed `MAX_PITCH`, the camera would clamp
        // where the target did not, leaving a permanent non-zero difference
        // -- an ease that never converges and so never stops requesting
        // animation frames.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        for _ in 0..100 {
            view.pitch_target_by(KEYBOARD_PITCH_STEP);
        }
        assert_eq!(view.target.pitch, MAX_PITCH);
        let mut frames = 0;
        while view.advance_orientation_animation(Duration::from_millis(16)) {
            frames += 1;
            assert!(frames < 600, "pitch ease never converged against its clamp");
        }
        assert_eq!(view.camera.pitch, MAX_PITCH);
    }

    #[test]
    fn the_pitch_clamp_is_sixty_degrees() {
        // MapLibre's long-standing default maximum (`transform_helper.ts:169`,
        // `maxPitch ?? 60`), and a typical navigation-camera ceiling. With
        // this renderer's 60-degree FOV it puts the horizon exactly on the
        // top edge, which is why `cover_plan::flat_cover` bounds the cover
        // and `horizon_fog` (Task 11) hides where it stops.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.tilt_by(100.0, 0.0);
        assert_eq!(view.camera.pitch, std::f32::consts::FRAC_PI_3);
        assert!((MAX_PITCH - 60.0_f32.to_radians()).abs() < 1e-6);
    }

    #[test]
    fn scroll_zoom_does_not_snap_the_rendered_camera_immediately() {
        // Before this fix, `zoom_by`/`zoom_towards_point` wrote straight
        // into `camera.zoom`, so a single scroll tick moved the rendered
        // camera all the way to the new zoom in one frame -- a snap, not an
        // animation. Now the input should only move `target_zoom`; the
        // rendered `camera.zoom` must stay put until a frame actually
        // advances the ease.
        let mut view = test_controller(4.0, 0.0, (1024.0, 768.0));
        view.zoom_by(1.0 * LINE);
        assert_eq!(view.camera.zoom, 4.0, "camera.zoom must not move before any animation frame runs");
        assert!(view.zoom_target.zoom > 4.0, "zoom_by must move the target, got target_zoom={}", view.zoom_target.zoom);
    }

    #[test]
    fn advance_zoom_animation_eases_camera_zoom_toward_the_target_over_several_frames() {
        let mut view = test_controller(4.0, 0.0, (1024.0, 768.0));
        view.zoom_by(4.0 * LINE); // target_zoom = 5.0
        let target = view.zoom_target.zoom;

        // One small frame: camera.zoom should have moved partway toward the
        // target, but not reached it -- this is what "eased" means, as
        // opposed to snapping to the target on the very first frame.
        let still_animating = view.advance_zoom_animation(Duration::from_millis(16));
        assert!(still_animating, "a single 16ms frame should not finish the ease");
        assert!(view.camera.zoom > 4.0, "camera.zoom should have started moving toward the target");
        assert!(
            view.camera.zoom < target,
            "camera.zoom ({}) should not reach target ({target}) after just one frame",
            view.camera.zoom
        );

        // Enough further frames (a couple of seconds of simulated time) must
        // fully converge and report the animation as finished.
        let mut finished = false;
        for _ in 0..200 {
            if !view.advance_zoom_animation(Duration::from_millis(16)) {
                finished = true;
                break;
            }
        }
        assert!(finished, "advance_zoom_animation should eventually report convergence");
        assert_eq!(view.camera.zoom, target, "camera.zoom should snap exactly onto the target once converged");
    }

    #[test]
    fn pan_keeps_the_zoom_ease_target_in_sync_so_it_does_not_fight_the_pan() {
        // `pan_by_pixels` is immediate (unanimated). If it only updated
        // `camera.center_*` and left `zoom_target.center_*` behind, the very next
        // `advance_zoom_animation` call would start easing the camera back
        // toward the stale pre-pan target, undoing the pan.
        let mut view = test_controller(4.0, 0.0, (1024.0, 768.0));
        view.pan_by_pixels(50.0, -30.0, (512.0, 384.0));
        let panned_lon = view.camera.center_lon;
        let panned_lat = view.camera.center_lat;

        // Whether or not the (already-converged) zoom ease reports "still
        // animating", the pan must not have been reverted.
        view.advance_zoom_animation(Duration::from_millis(16));
        assert_eq!(view.camera.center_lon, panned_lon, "pan must survive an animation-frame tick");
        assert_eq!(view.camera.center_lat, panned_lat, "pan must survive an animation-frame tick");
    }

    #[test]
    fn zoom_towards_point_still_anchors_the_cursor_once_the_ease_converges() {
        // The cursor-anchored zoom invariant (the world point under the
        // cursor stays under the cursor) must still hold once the eased
        // camera actually reaches the target -- this is the property the
        // smoothing must not break, just delay by a few frames.
        let mut view = test_controller(4.0, 0.0, (1024.0, 768.0));
        let screen_x = view.camera.viewport_width_px / 2.0 + 100.0;
        let screen_y = view.camera.viewport_height_px / 2.0 + 40.0;

        let before = crate::render::unproject_to_ground(&view.camera, screen_x, screen_y)
            .expect("center-ish point should hit the ground at pitch 0");

        view.zoom_towards_point(2.0 * LINE, screen_x, screen_y, false);
        // Converge the animation fully.
        for _ in 0..200 {
            if !view.advance_zoom_animation(Duration::from_millis(16)) {
                break;
            }
        }
        assert_eq!(view.camera.zoom, view.zoom_target.zoom, "animation should have converged for this assertion to be meaningful");

        let after = crate::render::unproject_to_ground(&view.camera, screen_x, screen_y)
            .expect("same screen point should still hit the ground");
        assert!(
            (before.x - after.x).abs() < 0.5 && (before.y - after.y).abs() < 0.5,
            "expected the world point under the cursor to stay anchored after the ease converges, \
             before={before:?} after={after:?}"
        );
    }

    #[test]
    fn zoom_towards_point_immediate_uses_a_much_faster_ease_than_the_default() {
        // `immediate` no longer hard-snaps `camera` straight to the target --
        // that turned out to let real trackpad input noise through as
        // visible position oscillation at high zoom (see `zoom_towards_point`'s
        // doc comment). It should still leave `camera.zoom` short of
        // `target_zoom` immediately after the call (an animation is still
        // needed), but converge far faster than the default eased rate once
        // frames advance.
        let mut immediate_view = test_controller(10.0, 0.0, (1024.0, 768.0));
        let screen_x = immediate_view.camera.viewport_width_px / 2.0;
        let screen_y = immediate_view.camera.viewport_height_px / 2.0;
        let before_zoom = immediate_view.camera.zoom;

        immediate_view.zoom_towards_point(2.0 * LINE, screen_x, screen_y, true);
        assert_eq!(immediate_view.camera.zoom, before_zoom, "camera.zoom must not move before any animation frame runs, immediate or not");
        assert_ne!(
            immediate_view.camera.zoom, immediate_view.zoom_target.zoom,
            "camera.zoom must not equal target_zoom yet -- immediate mode eases quickly, it doesn't snap"
        );
        assert_eq!(immediate_view.zoom_target.ease_rate, 60.0, "immediate zoom_towards_point should select the fast ease rate");

        // A matching eased (non-immediate) call, for comparison.
        let mut eased_view = test_controller(10.0, 0.0, (1024.0, 768.0));
        eased_view.zoom_towards_point(2.0 * LINE, screen_x, screen_y, false);
        assert_eq!(eased_view.zoom_target.ease_rate, 20.0, "non-immediate zoom_towards_point should select the default ease rate");

        // Advance both by the same short frame time; the immediate-mode
        // camera should have covered much more of the remaining distance to
        // its target than the eased-mode camera covered of its own.
        let dt = std::time::Duration::from_millis(20);
        immediate_view.advance_zoom_animation(dt);
        eased_view.advance_zoom_animation(dt);

        let immediate_remaining = (immediate_view.zoom_target.zoom - immediate_view.camera.zoom).abs();
        let eased_remaining = (eased_view.zoom_target.zoom - eased_view.camera.zoom).abs();

        // Both views started from the same zoom/amount, so they share the
        // same total distance-to-target; the fraction remaining after `dt`
        // is exactly `exp(-rate * dt)` from the exponential-decay formula in
        // `advance_zoom_animation`. Compute the expected fractions directly
        // rather than guessing a ratio, for a tight, self-documenting bound.
        let dt_secs = dt.as_secs_f64();
        let expected_immediate_fraction = (-60.0 * dt_secs).exp(); // ≈ 0.301
        let expected_eased_fraction = (-20.0 * dt_secs).exp(); // ≈ 0.670
        let immediate_total_distance = (immediate_view.zoom_target.zoom - before_zoom).abs();
        let eased_total_distance = (eased_view.zoom_target.zoom - before_zoom).abs();

        assert!(
            (immediate_remaining - immediate_total_distance * expected_immediate_fraction).abs() < 1e-6,
            "immediate remaining distance should match the rate=60 exponential-decay formula: \
             got {immediate_remaining}, expected ~{}",
            immediate_total_distance * expected_immediate_fraction
        );
        assert!(
            (eased_remaining - eased_total_distance * expected_eased_fraction).abs() < 1e-6,
            "eased remaining distance should match the rate=20 exponential-decay formula: \
             got {eased_remaining}, expected ~{}",
            eased_total_distance * expected_eased_fraction
        );

        // The headline behavior this test protects: after the same dt, the
        // fast (immediate) ease has converged much further than the default
        // ease -- roughly 70% vs roughly 33% of the way to target.
        assert!(
            immediate_remaining < eased_remaining * 0.5,
            "immediate mode should converge much faster than the default ease after the same dt: \
             immediate_remaining={immediate_remaining}, eased_remaining={eased_remaining}"
        );
    }

    /// World units per logical pixel at the look-at point: `1 / 2^zoom`
    /// (`render::camera`'s module docs).
    fn world_per_px(view: &CameraController) -> f64 {
        1.0 / 2f64.powf(view.camera.zoom)
    }

    fn singapore(zoom: f64, pitch: f32, bearing: f32, w: f32, h: f32) -> CameraController {
        let mut view = CameraController::new(
            Camera {
                center_lon: 103.8198,
                center_lat: 1.3521,
                zoom,
                pitch: 0.0,
                bearing,
                viewport_width_px: w,
                viewport_height_px: h,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            0.0,
            20.0,
        );
        // Direct, so a test can reach pitches past today's clamp.
        view.camera.pitch = pitch;
        view.target.pitch = pitch;
        view
    }

    /// Pans `view` by `(dx, dy)` grabbed at `anchor`, and returns the world
    /// displacement of the centre together with the displacement the FLAT
    /// formula gives for the same drag. The flat one is the drag's direction
    /// on the ground (exact at pitch 0, and the screen direction rotated by
    /// bearing under pitch), so a grab pan that moves "the right way" has a
    /// positive dot product with it.
    fn pan_and_flat_direction(
        view: &mut CameraController,
        dx: f32,
        dy: f32,
        anchor: (f32, f32),
    ) -> (glam::DVec3, glam::DVec3) {
        let before = view.camera.world_center_f64();
        let mut flat = view.clone();
        flat.pan_flat(dx, dy);
        view.apply(CameraIntent::PanBy { dx_px: dx, dy_px: dy, anchor_px: anchor });
        (view.camera.world_center_f64() - before, flat.camera.world_center_f64() - before)
    }

    #[test]
    fn a_tilted_drag_keeps_the_grabbed_ground_under_the_finger() {
        let mut view = singapore(16.0, 0.8, 0.3, 900.0, 900.0);
        let anchor = (300.0f32, 500.0f32);
        let grabbed = crate::render::unproject_to_ground_f64(&view.camera, anchor.0, anchor.1).unwrap();
        view.apply(CameraIntent::PanBy { dx_px: 40.0, dy_px: -60.0, anchor_px: anchor });
        let (x, y) = crate::render::project_world_to_screen_f64(&view.camera, (grabbed.x, grabbed.y)).unwrap();
        let slip = ((x - 340.0).powi(2) + (y - 440.0).powi(2)).sqrt();
        assert!(slip < 0.05, "the grabbed ground slipped {slip} px out from under the finger");
    }

    #[test]
    fn the_grab_pan_agrees_with_the_flat_pan_at_pitch_zero() {
        // The spec's "at pitch 0 the result must match today's to within
        // float noise", pinned by running BOTH formulas on the same input.
        let mut flat = singapore(16.0, 0.0, 0.7, 900.0, 900.0);
        let mut grab = flat.clone();
        flat.pan_flat(37.0, -21.0);
        assert!(grab.pan_by_grab(37.0, -21.0, (200.0, 650.0)));
        assert!((flat.camera.center_lon - grab.camera.center_lon).abs() < 1e-9);
        assert!((flat.camera.center_lat - grab.camera.center_lat).abs() < 1e-9);

        // And the pitch-0 production path IS the flat formula, bit for bit.
        let mut routed = singapore(16.0, 0.0, 0.7, 900.0, 900.0);
        let mut direct = routed.clone();
        routed.apply(CameraIntent::PanBy { dx_px: 37.0, dy_px: -21.0, anchor_px: (200.0, 650.0) });
        direct.pan_flat(37.0, -21.0);
        assert_eq!(routed.camera, direct.camera);
    }

    #[test]
    fn a_drag_near_the_horizon_is_clamped_to_the_grab_scale_limit() {
        // Pitch 60 with this renderer's 60-degree FOV: the top screen edge
        // IS the horizon, and the first rows under it unproject hundreds to
        // thousands of centre-pixels of ground per screen pixel.
        let mut view = singapore(16.0, std::f32::consts::FRAC_PI_3, 0.0, 900.0, 900.0);
        let (moved, flat) = pan_and_flat_direction(&mut view, 0.0, 10.0, (450.0, 1.0));
        let moved_px = moved.length() / world_per_px(&view);
        assert!(moved_px.is_finite());
        // At the clamp row one vertical pixel covers ~30 centre-pixels of
        // ground (measured while writing this plan: 29.6 at pitch 57.3).
        // 40 is the ceiling, against >1000 unclamped.
        assert!(moved_px <= 40.0 * 10.0, "a 10 px drag at the horizon moved {moved_px} centre-px of ground");
        // And the floor (ruling F2): the drag still moves the map, at no
        // less than the clamp row's horizontal rate, the right way.
        assert!(moved_px >= GRAB_MAX_PITCH_SCALE * 10.0, "a 10 px drag at the horizon moved only {moved_px} centre-px");
        assert!(moved.dot(flat) > 0.0, "the map moved against the drag: {moved:?} vs {flat:?}");
    }

    #[test]
    fn a_drag_that_starts_above_the_horizon_does_not_fling_the_map() {
        let mut view = singapore(16.0, std::f32::consts::FRAC_PI_3, 0.4, 900.0, 900.0);
        // (450, -40) is above the viewport, so its ray never reaches the
        // ground; the drag ends just under the horizon.
        let (moved, flat) = pan_and_flat_direction(&mut view, 0.0, 100.0, (450.0, -40.0));
        let moved_px = moved.length() / world_per_px(&view);
        assert!(moved_px.is_finite(), "the centre went non-finite: {:?}", view.camera);
        assert!(moved_px <= 40.0 * 100.0, "moved {moved_px} centre-px");
        assert!(moved_px >= GRAB_MAX_PITCH_SCALE * 100.0, "moved only {moved_px} centre-px");
        assert!(moved.dot(flat) > 0.0, "the map moved against the drag: {moved:?} vs {flat:?}");
    }

    #[test]
    fn a_drag_inside_the_horizon_band_still_moves_the_map() {
        // Ruling F2. Both ends of each drag sit in the band above the clamp
        // row (the top ~12% of the screen at 60 degrees). Clamping the ROW
        // would put both ends on the same row and move nothing; clamping
        // the RATE moves the map at the clamp row's rate, the drag's way.
        for (dx, dy, anchor) in [
            (0.0, 15.0, (450.0, 20.0)),
            (0.0, -15.0, (450.0, 40.0)),
            (30.0, 0.0, (450.0, 20.0)),
            (-30.0, 0.0, (200.0, 5.0)),
            (25.0, 15.0, (300.0, 30.0)),
        ] {
            let mut view = singapore(16.0, std::f32::consts::FRAC_PI_3, 0.4, 900.0, 900.0);
            let (moved, flat) = pan_and_flat_direction(&mut view, dx, dy, anchor);
            let moved_px = moved.length() / world_per_px(&view);
            let drag_px = f64::from(dx.hypot(dy));
            assert!(
                moved_px >= GRAB_MAX_PITCH_SCALE * drag_px * 0.99,
                "a ({dx}, {dy}) drag at {anchor:?} moved only {moved_px} centre-px"
            );
            assert!(moved_px <= 40.0 * drag_px, "a ({dx}, {dy}) drag at {anchor:?} moved {moved_px} centre-px");
            let cos = moved.dot(flat) / (moved.length() * flat.length());
            assert!(cos > 0.5, "a ({dx}, {dy}) drag at {anchor:?} moved at cos {cos} to the drag's direction");
        }
    }

    #[test]
    fn a_release_without_a_rotation_since_the_press_never_snaps() {
        // Task 2 finding 1(b): an earlier gesture rotated (an Option+trackpad
        // rotate whose `Ended` never arrived), leaving `rotated_in_gesture`
        // set. The next press must clear it, so its release does not snap.
        let mut view = test_controller(10.0, 0.0, (1024.0, 768.0));
        view.apply(CameraIntent::RotateBy { radians: 3.0_f32.to_radians(), anchor_px: None });
        view.apply(CameraIntent::GestureBegan);
        view.apply(CameraIntent::PanBy { dx_px: 40.0, dy_px: 0.0, anchor_px: (512.0, 384.0) });
        let before = view.target.bearing;
        assert!(before > 2.0_f32.to_radians(), "sanity: the target is off north, at {before}");
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        assert_eq!(view.target.bearing, before, "the press cleared the stale rotation, so nothing snaps");
    }

    #[test]
    fn a_zero_sized_viewport_never_puts_nan_into_the_camera() {
        fn assert_finite(view: &CameraController, after: &str) {
            let c = view.camera;
            let t = view.zoom_target;
            for v in [c.center_lon, c.center_lat, c.zoom, f64::from(c.pitch), f64::from(c.bearing), t.center_lon, t.center_lat, t.zoom] {
                assert!(v.is_finite(), "a 0x0 viewport put a non-finite value into {c:?} / {t:?} after {after}");
            }
            if let Some(a) = t.anchor {
                assert!(a.is_finite(), "a 0x0 viewport put a non-finite zoom anchor {a:?} after {after}");
            }
        }
        let mut view = singapore(14.0, 0.5, 0.0, 0.0, 0.0);
        view.apply(CameraIntent::PanBy { dx_px: 10.0, dy_px: 5.0, anchor_px: (0.0, 0.0) });
        assert_finite(&view, "PanBy");
        // Advanced before `jump_to` too: `jump_to` overwrites the centre and
        // drops the anchor, so checking only after it would hide a NaN the
        // zoom put there.
        view.apply(CameraIntent::ZoomAbout { levels: 1.0, anchor_px: (0.0, 0.0), immediate: true });
        assert_finite(&view, "ZoomAbout");
        view.advance(Duration::from_millis(16));
        assert_finite(&view, "ZoomAbout + advance");
        view.apply(CameraIntent::RotateBy { radians: 0.3, anchor_px: Some((0.0, 0.0)) });
        assert_finite(&view, "RotateBy");
        view.jump_to(CameraTarget { center: Some((103.8, 1.35)), anchor_offset_px: (0.0, 100.0), ..CameraTarget::default() });
        for _ in 0..400 {
            if !view.advance(Duration::from_millis(16)) {
                break;
            }
        }
        assert_finite(&view, "jump_to + advance");
        // And the viewport arriving later works normally.
        view.set_viewport(900.0, 900.0);
        view.apply(CameraIntent::PanBy { dx_px: 10.0, dy_px: 0.0, anchor_px: (450.0, 600.0) });
        assert!(view.camera.center_lon.is_finite());
    }

    #[test]
    fn an_anchored_rotation_holds_the_ground_under_the_anchor_even_tilted() {
        // iOS's `the_ground_under_the_pinch_midpoint_stays_there`, moved to
        // where the rule now lives, and now also under pitch: translation
        // equivariance makes the correction exact for any pitch.
        for pitch in [0.0f32, 0.7] {
            let mut view = singapore(15.0, pitch, 0.0, 390.0, 844.0);
            let anchor = (120.0f32, 560.0f32);
            let held = crate::render::unproject_to_ground_f64(&view.camera, anchor.0, anchor.1).unwrap();
            for _ in 0..35 {
                view.apply(CameraIntent::RotateBy { radians: 2.0_f32.to_radians(), anchor_px: Some(anchor) });
            }
            let (x, y) = crate::render::project_world_to_screen_f64(&view.camera, (held.x, held.y)).unwrap();
            let slip = ((x - anchor.0).powi(2) + (y - anchor.1).powi(2)).sqrt();
            assert!(slip < 0.05, "pitch {pitch}: the held ground slipped {slip} px");
        }
    }

    #[test]
    fn a_release_mid_spin_keeps_turning_and_eases_to_a_stop() {
        let mut view = singapore(15.0, 0.0, 0.0, 900.0, 900.0);
        view.apply(CameraIntent::GestureBegan);
        view.apply(CameraIntent::RotateBy { radians: 0.5, anchor_px: None });
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 1.8 }); // rad/s
        let released = view.camera.bearing;
        let mut last = released;
        let mut frames = 0;
        while view.advance(Duration::from_millis(16)) {
            assert!(view.camera.bearing >= last, "inertia reversed");
            last = view.camera.bearing;
            frames += 1;
            assert!(frames < 600, "rotation inertia never settled");
        }
        let travelled = view.camera.bearing - released;
        assert!((travelled - 1.8 / ORIENTATION_EASE_RATE).abs() < 1e-3, "an exponential throw covers v/k, got {travelled}");
    }

    #[test]
    fn rotation_inertia_crosses_the_antimeridian_of_bearing_the_short_way() {
        let mut view = singapore(15.0, 0.0, 170.0_f32.to_radians(), 900.0, 900.0);
        view.apply(CameraIntent::GestureBegan);
        view.apply(CameraIntent::RotateBy { radians: 0.0, anchor_px: None });
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 4.0 }); // throws ~12.7 degrees
        while view.advance(Duration::from_millis(16)) {
            let b = view.camera.bearing;
            assert!((-std::f32::consts::PI..std::f32::consts::PI).contains(&b), "bearing left [-PI, PI): {b}");
            assert!(b.abs() > 160.0_f32.to_radians(), "inertia took the long way round, at {} deg", b.to_degrees());
        }
        let expected = wrap_bearing(170.0_f32.to_radians() + 4.0 / ORIENTATION_EASE_RATE);
        assert!(shortest_bearing_delta(view.camera.bearing, expected).abs() < 1e-3);
    }

    #[test]
    fn a_throw_is_capped_at_maplibres_ceiling_and_can_still_snap_north() {
        let mut view = singapore(15.0, 0.0, 0.0, 900.0, 900.0);
        view.apply(CameraIntent::RotateBy { radians: 0.0, anchor_px: None });
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 1.0e6 });
        let throw = shortest_bearing_delta(view.camera.bearing, view.target.bearing).abs();
        // The speed cap bites first: 2*PI rad/s over rate 18 is a 20-degree
        // throw, inside MapLibre's 179-degree total cap.
        assert!((throw - ROTATE_INERTIA_MAX_SPEED / ORIENTATION_EASE_RATE).abs() < 1e-5, "throw {throw}");
        assert!(throw <= ROTATE_INERTIA_MAX_THROW);
        let mut near = singapore(15.0, 0.0, 0.2, 900.0, 900.0);
        near.apply(CameraIntent::RotateBy { radians: 0.0, anchor_px: None });
        near.apply(CameraIntent::GestureEnded { rotate_velocity: -3.0 }); // lands 0.2 - 0.167 = 1.9 deg off north
        assert_eq!(near.target.bearing, 0.0, "a throw that lands inside the snap window lands ON north");
    }

    #[test]
    fn grabbing_the_map_stops_a_spin_dead() {
        let mut view = singapore(15.0, 0.0, 0.0, 900.0, 900.0);
        view.apply(CameraIntent::RotateBy { radians: 0.1, anchor_px: None });
        view.apply(CameraIntent::GestureEnded { rotate_velocity: 3.0 });
        view.advance(Duration::from_millis(16));
        view.apply(CameraIntent::GestureBegan);
        assert!(!view.advance(Duration::from_millis(16)), "a finger down catches the spin");
    }

    #[test]
    fn rotate_samples_measure_the_last_sixty_milliseconds_only() {
        let mut samples = RotateSamples::default();
        samples.push(0.0, 0.5); // ages out
        samples.push(200.0, 0.0);
        samples.push(220.0, 0.02);
        samples.push(240.0, 0.02);
        samples.push(260.0, 0.02);
        let v = samples.velocity(260.0);
        // Window is t >= 200: four samples, 60 ms apart end to end; the 200 ms
        // sample only marks the start, so 0.06 rad over 60 ms = 1 rad/s.
        assert!((v - 1.0).abs() < 1e-4, "got {v}");
        assert_eq!(samples.velocity(400.0), 0.0, "a finger that stopped before release throws nothing");
    }

    #[test]
    fn samples_from_one_frame_throw_nothing() {
        // Both fingers' events of one frame, handled back to back: 0.002 rad
        // over 0.01 ms would read as 200 rad/s, the maximum throw.
        let mut samples = RotateSamples::default();
        samples.push(1_000.0, 0.001);
        samples.push(1_000.01, 0.001);
        assert_eq!(samples.velocity(1_000.02), 0.0);
    }

    #[test]
    fn a_two_frame_flick_at_120_hz_still_measures_the_spin() {
        let mut samples = RotateSamples::default();
        samples.push(1_000.0, 0.01);
        samples.push(1_008.33, 0.01);
        let v = samples.velocity(1_008.33);
        assert!(v > 1.0, "0.01 rad over one 120 Hz frame is ~1.2 rad/s: {v}");
    }

    #[test]
    fn samples_spread_over_forty_milliseconds_still_measure_the_spin() {
        let mut samples = RotateSamples::default();
        for i in 0..=4 {
            samples.push(1_000.0 + 10.0 * f64::from(i), 0.01);
        }
        // The first sample marks the start: 0.04 rad over 40 ms = 1 rad/s.
        let v = samples.velocity(1_040.0);
        assert!((v - 1.0).abs() < 1e-4, "got {v}");
    }

    #[test]
    fn ease_to_arrives_exactly_and_settles_in_its_duration() {
        let mut view = singapore(14.0, 0.0, 0.0, 900.0, 900.0);
        let target = CameraTarget {
            center: Some((103.86, 1.29)),
            zoom: Some(16.0),
            pitch: Some(0.6),
            bearing: Some(-2.5),
            anchor_offset_px: (0.0, 0.0),
        };
        view.ease_to(target, Duration::from_millis(500));
        let mut frames = 0;
        while view.advance(Duration::from_millis(16)) {
            frames += 1;
            assert!(frames <= 32, "a 500 ms ease must finish within 32 frames of 16 ms");
        }
        assert!((view.camera.center_lon - 103.86).abs() < 1e-12 && (view.camera.center_lat - 1.29).abs() < 1e-12);
        assert_eq!((view.camera.zoom, view.camera.pitch), (16.0, 0.6));
        assert!((view.camera.bearing - (-2.5)).abs() < 1e-6);
        assert!(view.is_settled());
    }

    #[test]
    fn ease_to_turns_the_short_way_across_the_bearing_wrap() {
        let mut view = singapore(14.0, 0.0, 175.0_f32.to_radians(), 900.0, 900.0);
        view.ease_to(CameraTarget { bearing: Some((-175.0_f32).to_radians()), ..CameraTarget::default() }, Duration::from_millis(300));
        while view.advance(Duration::from_millis(16)) {
            assert!(view.camera.bearing.abs() > 170.0_f32.to_radians(), "the long way round");
        }
    }

    // ---- fly_to: MapLibre's van Wijk flight ----

    /// Samples a motion to its end at 16 ms frames: `(elapsed ms, camera)`.
    fn fly_frames(view: &mut CameraController) -> Vec<(u32, Camera)> {
        let mut out = Vec::new();
        let mut t = 0;
        while view.advance(Duration::from_millis(16)) {
            t += 16;
            out.push((t, view.camera));
            assert!(t < 120_000, "a flight must end");
        }
        out
    }

    /// Singapore to Tokyo at z10, ~5,300 km: the camera rises well above
    /// both ends (more than three zoom levels), peaks in the first half of
    /// the TIME (MapLibre's easing front-loads the path), and lands
    /// exactly. An `ease_to` over the same move never leaves z10.
    #[test]
    fn fly_to_rises_over_a_long_move_and_lands_exactly() {
        let tokyo = (139.767, 35.681);
        let mut view = singapore(10.0, 0.0, 0.0, 1024.0, 768.0);
        view.fly_to(CameraTarget { center: Some(tokyo), zoom: Some(10.0), ..CameraTarget::default() }, None);
        let frames = fly_frames(&mut view);
        let (apex_t, apex) = frames.iter().min_by(|a, b| a.1.zoom.total_cmp(&b.1.zoom)).copied().unwrap();
        let total = frames.last().unwrap().0;
        assert!(apex.zoom < 7.0, "the flight should rise more than three levels, apex z{}", apex.zoom);
        assert!(apex_t < total / 2, "apex at {apex_t} ms of {total} ms");
        assert!((view.camera.center_lon - tokyo.0).abs() < 1e-9 && (view.camera.center_lat - tokyo.1).abs() < 1e-9);
        assert_eq!(view.camera.zoom, 10.0);
        assert!(view.is_settled());

        let mut eased = singapore(10.0, 0.0, 0.0, 1024.0, 768.0);
        eased.ease_to(CameraTarget { center: Some(tokyo), zoom: Some(10.0), ..CameraTarget::default() }, Duration::from_secs(2));
        assert!(fly_frames(&mut eased).iter().all(|(_, c)| (c.zoom - 10.0).abs() < 1e-9), "an ease keeps its zoom");
    }

    /// With no duration given the flight takes MapLibre's: the path's
    /// length over its speed. Across 5,300 km that is seconds; a short hop
    /// is far quicker.
    #[test]
    fn fly_to_times_itself_by_path_length() {
        let flight_ms = |to: (f64, f64)| {
            let mut view = singapore(10.0, 0.0, 0.0, 1024.0, 768.0);
            view.fly_to(CameraTarget { center: Some(to), ..CameraTarget::default() }, None);
            fly_frames(&mut view).last().map_or(0, |(t, _)| *t)
        };
        let long = flight_ms((139.767, 35.681));
        let short = flight_ms((103.9, 1.30));
        assert!(long > 3_000, "a 5,300 km flight took {long} ms");
        assert!(short < long / 3, "a hop took {short} ms against {long}");
    }

    /// Only pitch or bearing changing is no flight: it eases, for
    /// `easeTo`'s 500 ms, and the zoom never moves.
    #[test]
    fn fly_to_the_same_place_eases_the_orientation() {
        let mut view = singapore(12.0, 0.0, 0.0, 900.0, 900.0);
        view.fly_to(CameraTarget { bearing: Some(1.0), ..CameraTarget::default() }, None);
        let frames = fly_frames(&mut view);
        assert!((28..=33).contains(&frames.len()), "{} frames", frames.len());
        assert!(frames.iter().all(|(_, c)| c.zoom == 12.0));
        assert!((view.camera.bearing - 1.0).abs() < 1e-6);
    }

    /// Never higher than the zoom range allows: MapLibre's `minZoom` caps
    /// the path's rho, and the controller's floor is passed for it.
    #[test]
    fn a_flight_stays_inside_the_zoom_range() {
        let mut view = singapore(6.0, 0.0, 0.0, 1024.0, 768.0);
        view.set_zoom_range(5.0, MAX_ZOOM);
        view.fly_to(CameraTarget { center: Some((-74.0, 40.7)), zoom: Some(6.0), ..CameraTarget::default() }, None);
        for (_, c) in fly_frames(&mut view) {
            assert!(c.zoom >= 5.0 - 1e-6, "z{} under the floor", c.zoom);
        }
    }

    /// An explicit duration is honoured, as `ease_to`'s is.
    #[test]
    fn fly_to_honours_an_explicit_duration() {
        let mut view = singapore(10.0, 0.0, 0.0, 1024.0, 768.0);
        view.fly_to(CameraTarget { center: Some((139.767, 35.681)), ..CameraTarget::default() }, Some(Duration::from_millis(800)));
        let frames = fly_frames(&mut view);
        assert!(frames.len() <= 51, "an 800 ms flight ends within 51 frames, took {}", frames.len());
    }

    #[test]
    fn a_live_pan_cancels_an_ease_in_flight_and_nothing_resumes_it() {
        let mut view = singapore(14.0, 0.0, 0.0, 900.0, 900.0);
        view.ease_to(CameraTarget { center: Some((104.2, 1.10)), zoom: Some(17.0), ..CameraTarget::default() }, Duration::from_secs(1));
        for _ in 0..10 {
            view.advance(Duration::from_millis(16));
        }
        let interrupted = view.camera;
        view.apply(CameraIntent::PanBy { dx_px: 30.0, dy_px: 0.0, anchor_px: (450.0, 450.0) });
        assert_eq!(view.camera.zoom, interrupted.zoom, "the pan applies to the pose the ease had reached");
        assert!(view.camera.center_lon < interrupted.center_lon, "and the pan itself happened");
        let after_pan = view.camera;
        assert!(!view.advance(Duration::from_millis(16)), "nothing is left chasing the old target");
        assert_eq!(view.camera, after_pan);
    }

    #[test]
    fn jump_to_puts_the_centre_at_the_anchor_offset() {
        let mut view = singapore(16.0, 0.8, 0.9, 900.0, 900.0);
        let puck = (103.851, 1.2901);
        view.jump_to(CameraTarget { center: Some(puck), anchor_offset_px: (0.0, 225.0), ..CameraTarget::default() });
        let (x, y) = crate::geo::lon_lat_to_tile_xy(puck.0, puck.1, 0.0);
        let world = f64::from(WORLD_SIZE);
        let (sx, sy) = crate::render::project_world_to_screen_f64(&view.camera, (x * world, y * world)).unwrap();
        assert!((sx - 450.0).abs() < 0.05 && (sy - 675.0).abs() < 0.05, "the puck landed at ({sx}, {sy}), wanted (450, 675)");
        assert!(view.is_settled());
    }

    #[test]
    fn every_motion_settles_in_finite_time() {
        // The on-demand redraw contract: `advance` must reach `false`.
        let cases: Vec<Box<dyn Fn(&mut CameraController)>> = vec![
            Box::new(|v| v.apply(CameraIntent::RotateStep { radians: 1.0 })),
            Box::new(|v| v.apply(CameraIntent::TiltStep { radians: 10.0 })),
            Box::new(|v| v.apply(CameraIntent::ZoomAbout { levels: 3.0, anchor_px: (100.0, 800.0), immediate: false })),
            Box::new(|v| v.apply(CameraIntent::ResetNorthAndPitch)),
            Box::new(|v| {
                v.apply(CameraIntent::RotateBy { radians: 0.2, anchor_px: None });
                v.apply(CameraIntent::GestureEnded { rotate_velocity: 1.0e9 });
            }),
            Box::new(|v| v.ease_to(CameraTarget { zoom: Some(19.0), bearing: Some(3.0), ..CameraTarget::default() }, Duration::from_secs(2))),
            // An ease interrupted mid-flight, by each kind of input that can
            // interrupt it: a spin thrown over it, a wheel zoom, a keyboard
            // step, and a press that is never released.
            Box::new(|v| {
                v.ease_to(CameraTarget { zoom: Some(18.0), bearing: Some(-2.0), ..CameraTarget::default() }, Duration::from_secs(2));
                for _ in 0..10 {
                    v.advance(Duration::from_millis(16));
                }
                v.apply(CameraIntent::GestureBegan);
                v.apply(CameraIntent::RotateBy { radians: 0.3, anchor_px: Some((200.0, 700.0)) });
                v.apply(CameraIntent::GestureEnded { rotate_velocity: -5.0 });
            }),
            Box::new(|v| {
                v.ease_to(CameraTarget { center: Some((104.1, 1.2)), zoom: Some(12.0), ..CameraTarget::default() }, Duration::from_secs(2));
                for _ in 0..10 {
                    v.advance(Duration::from_millis(16));
                }
                v.apply(CameraIntent::ZoomAbout { levels: -2.0, anchor_px: (700.0, 200.0), immediate: false });
            }),
            Box::new(|v| {
                v.ease_to(CameraTarget { pitch: Some(0.0), bearing: Some(2.0), ..CameraTarget::default() }, Duration::from_secs(2));
                for _ in 0..10 {
                    v.advance(Duration::from_millis(16));
                }
                v.apply(CameraIntent::TiltStep { radians: 0.3 });
            }),
            Box::new(|v| {
                v.ease_to(CameraTarget { zoom: Some(17.0), ..CameraTarget::default() }, Duration::from_secs(2));
                for _ in 0..10 {
                    v.advance(Duration::from_millis(16));
                }
                v.apply(CameraIntent::GestureBegan);
            }),
        ];
        for (i, start) in cases.iter().enumerate() {
            let mut view = singapore(14.0, 0.5, 1.0, 900.0, 900.0);
            start(&mut view);
            let mut frames = 0;
            while view.advance(Duration::from_millis(16)) {
                frames += 1;
                assert!(frames < 2_000, "case {i} never settled: {:?}", view.camera);
            }
            assert!(view.is_settled(), "case {i} reported false while not settled");
        }
    }

    #[test]
    fn an_ease_started_after_an_idle_gap_does_not_finish_on_its_first_frame() {
        // Native's `paint` measures `dt` from the last frame it drew, which
        // after an idle map is however long the map sat still. `advance`
        // clamps that (ruling F6) for the ease a programmatic move starts
        // too, or `ease_to` called on an idle map would arrive in one jump.
        let mut view = singapore(14.0, 0.0, 0.0, 900.0, 900.0);
        view.ease_to(CameraTarget { zoom: Some(16.0), ..CameraTarget::default() }, Duration::from_millis(500));
        assert!(view.advance(Duration::from_secs(10)), "a 10 s first frame finished a 500 ms ease");
        assert!(view.camera.zoom < 15.0, "the first frame after idle moved the ease to zoom {}", view.camera.zoom);
    }

    /// Spec S8.2: "ease matches ease_to stepped at 1/60 s for 120 frames".
    /// The free `ease_pose`/`resolve_pose` are exactly the controller's own
    /// `ease_to` arithmetic, so the story camera path can reuse it without
    /// drifting from the interactive ease frame by frame.
    #[test]
    fn ease_pose_is_the_controllers_formula() {
        let cam = |lon, lat, zoom, pitch: f32, bearing: f32| Camera {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch,
            bearing,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let from = cam(170.0, 10.0, 4.0, 0.0, 3.0);
        let to_target = CameraTarget { center: Some((-170.0, 20.0)), zoom: Some(6.0), pitch: Some(0.5), bearing: Some(-3.0), ..Default::default() };
        let mut c = CameraController::new(from, MIN_ZOOM, MAX_ZOOM);
        let to = resolve_pose(&from, &to_target, (MIN_ZOOM, MAX_ZOOM), true);
        c.ease_to(to_target, Duration::from_secs(2));
        for i in 1..=120 {
            c.advance(Duration::from_secs_f64(1.0 / 60.0));
            let k = EASE_TO_EASING.t_at((i as f64 / 120.0).min(1.0), 0.0, 1.0);
            let want = if i == 120 { to } else { ease_pose(&from, &to, k) };
            let got = *c.camera();
            // `k` here is recomputed from the frame index (i/120), while the
            // controller's own `k` comes from real `Duration` arithmetic:
            // 120 steps of `Duration::from_secs_f64(1.0 / 60.0)` (not exactly
            // representable in whole nanoseconds) accumulated against a
            // 2-second `Duration`. That quantization -- not a formula
            // difference -- is worth up to ~2.4e-7 mid-motion (measured),
            // collapsing back to exactly 0 at frame 120 where `elapsed` clamps
            // to `duration` and the motion lands on `to` exactly. 1e-6 clears
            // that with headroom while still catching a real formula drift.
            assert!(
                (got.center_lon - want.center_lon).abs() < 1e-6
                    && (got.center_lat - want.center_lat).abs() < 1e-6
                    && (got.zoom - want.zoom).abs() < 1e-6,
                "frame {i}: {got:?} vs {want:?}"
            );
            assert!((got.pitch - want.pitch).abs() < 1e-6 && (got.bearing - want.bearing).abs() < 1e-6);
        }
    }

    /// Every way a caller can hand `jump_to`/`ease_to` a non-finite field:
    /// a JS `undefined` argument arrives as NaN through wasm-bindgen.
    fn non_finite_targets() -> Vec<(&'static str, CameraTarget)> {
        let base = CameraTarget { center: Some((103.86, 1.29)), ..CameraTarget::default() };
        vec![
            ("center lon", CameraTarget { center: Some((f64::NAN, 1.29)), ..base }),
            ("center lat", CameraTarget { center: Some((103.86, f64::NAN)), ..base }),
            ("center lon inf", CameraTarget { center: Some((f64::INFINITY, 1.29)), ..base }),
            ("zoom", CameraTarget { zoom: Some(f64::NAN), ..base }),
            ("zoom inf", CameraTarget { zoom: Some(f64::INFINITY), ..base }),
            ("pitch", CameraTarget { pitch: Some(f32::NAN), ..base }),
            ("bearing", CameraTarget { bearing: Some(f32::NAN), ..base }),
            ("bearing inf", CameraTarget { bearing: Some(f32::INFINITY), ..base }),
            ("anchor offset", CameraTarget { anchor_offset_px: (f32::NAN, 10.0), ..base }),
        ]
    }

    fn assert_finite_and_settles(view: &mut CameraController, label: &str) {
        for frame in 0.. {
            let c = view.camera;
            assert!(
                c.center_lon.is_finite() && c.center_lat.is_finite() && c.zoom.is_finite() && c.pitch.is_finite() && c.bearing.is_finite(),
                "{label}: non-finite camera {c:?} at frame {frame}"
            );
            if !view.advance(Duration::from_millis(16)) {
                break;
            }
            assert!(frame < 64, "{label}: still moving after 64 frames");
        }
        assert!(view.is_settled(), "{label}: advance said done but the eases are not settled");
    }

    #[test]
    fn a_non_finite_target_field_is_ignored_and_the_camera_settles() {
        for (label, target) in non_finite_targets() {
            let mut view = singapore(14.0, 0.3, 0.5, 900.0, 900.0);
            let before = view.camera;
            view.jump_to(target);
            assert_finite_and_settles(&mut view, &format!("jump_to {label}"));
            if label.starts_with("zoom") {
                assert_eq!(view.camera.zoom, before.zoom, "{label}: zoom kept");
            }
            if label == "pitch" {
                assert_eq!(view.camera.pitch, before.pitch, "{label}: pitch kept");
            }
            if label.starts_with("bearing") {
                assert_eq!(view.camera.bearing, before.bearing, "{label}: bearing kept");
            }
            if label.starts_with("center lon") {
                assert_eq!(view.camera.center_lon, before.center_lon, "{label}: lon kept");
                assert!((view.camera.center_lat - 1.29).abs() < 1e-12, "{label}: the finite lat still applies");
            }
            if label == "center lat" {
                assert_eq!(view.camera.center_lat, before.center_lat, "{label}: lat kept");
                assert!((view.camera.center_lon - 103.86).abs() < 1e-12, "{label}: the finite lon still applies");
            }
            if label == "anchor offset" {
                assert!((view.camera.center_lon - 103.86).abs() < 1e-12 && (view.camera.center_lat - 1.29).abs() < 1e-12, "{label}: the centre lands unoffset");
            }

            let mut view = singapore(14.0, 0.3, 0.5, 900.0, 900.0);
            view.ease_to(target, Duration::from_millis(300));
            assert_finite_and_settles(&mut view, &format!("ease_to {label}"));
        }
    }

    #[test]
    fn a_target_latitude_beyond_mercator_is_clamped_to_its_limit() {
        let mut view = singapore(14.0, 0.0, 0.0, 900.0, 900.0);
        view.jump_to(CameraTarget { center: Some((10.0, 89.9)), ..CameraTarget::default() });
        assert_eq!(view.camera.center_lat, MAX_MERCATOR_LAT);
        view.jump_to(CameraTarget { center: Some((10.0, -1e9)), ..CameraTarget::default() });
        assert_eq!(view.camera.center_lat, -MAX_MERCATOR_LAT);
        assert_finite_and_settles(&mut view, "clamped lat");
    }

    #[test]
    fn an_unusable_ease_duration_becomes_a_jump() {
        for ms in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -5.0, 1e30] {
            assert_eq!(ease_duration_from_ms(ms), Duration::ZERO, "{ms} ms");
        }
        assert_eq!(ease_duration_from_ms(250.0), Duration::from_millis(250));
        assert_eq!(ease_duration_from_ms(MAX_EASE_DURATION.as_secs_f64() * 1000.0), MAX_EASE_DURATION);

        let target = CameraTarget { center: Some((103.86, 1.29)), zoom: Some(16.0), ..CameraTarget::default() };
        for duration in [ease_duration_from_ms(f64::INFINITY), ease_duration_from_ms(f64::NAN), Duration::MAX] {
            let mut view = singapore(14.0, 0.0, 0.0, 900.0, 900.0);
            view.ease_to(target, duration);
            assert_eq!(view.camera.zoom, 16.0, "{duration:?}: landed at once");
            assert!((view.camera.center_lon - 103.86).abs() < 1e-12);
            assert!(!view.advance(Duration::from_millis(16)), "{duration:?}: nothing left moving");
        }
    }

    #[test]
    fn degrees_become_radians_only_when_the_result_is_finite() {
        assert_eq!(radians_from_degrees(90.0), Some(std::f32::consts::FRAC_PI_2));
        assert_eq!(radians_from_degrees(f64::NAN), None);
        assert_eq!(radians_from_degrees(f64::INFINITY), None);
        assert_eq!(radians_from_degrees(1e300), None, "narrows to f32 infinity");
        // 1e39 overflows f32 as DEGREES but not as radians: the conversion
        // happens in f64 first, so the result is finite.
        assert!(radians_from_degrees(1e39).is_some_and(f32::is_finite));
    }

    /// The bug report this whole fix responds to: "in some positions I
    /// cannot zoom out from the Earth", reproduced at z1, lat 85.05, pitch
    /// tilted, anchored zoom (scroll-wheel zoom always is). Repeatedly
    /// zooming out, anchored near the TOP of the viewport (the screen
    /// region most likely to ray-cast to an extreme latitude, and hence
    /// most likely to trip any lat-clamp interaction with the zoom ease),
    /// must walk `camera.zoom` down to [`MIN_ZOOM`] and settle there --
    /// not stall partway because each step's centre move gets undone by
    /// `resolve`/`lon_lat_to_tile_xy`'s latitude clamp. Checked at the
    /// north pole limit, the south pole limit, and the equator, at pitch 0
    /// and pitch 60 (about [`MAX_PITCH`]'s ballpark), because the user's
    /// report said "in SOME positions" -- one working case would have
    /// hidden a bug that only bites at certain centres.
    #[test]
    fn zoom_out_anchored_near_the_pole_reaches_the_shared_minimum_and_does_not_stall() {
        for (lat, pitch_deg) in [(85.0, 60.0f32), (85.0, 0.0), (-85.0, 60.0), (0.0, 60.0)] {
            let mut view = CameraController::new(
                Camera {
                    center_lon: -108.1529,
                    center_lat: lat,
                    zoom: 1.0,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: 1024.0,
                    viewport_height_px: 768.0,
                    fov_y: Camera::DEFAULT_FOV_Y,
                },
                MIN_ZOOM,
                MAX_ZOOM,
            );
            view.camera.pitch = pitch_deg.to_radians();
            view.target.pitch = view.camera.pitch;
            // Near the top of the viewport: the anchor a scroll-wheel zoom
            // uses is wherever the cursor sits, and the top edge is exactly
            // where a tilted camera's ray runs longest before it reaches
            // the (infinite, flat) Mercator ground plane.
            let anchor = (512.0, 40.0);
            let mut last_zoom = view.camera.zoom;
            let mut stalled_steps = 0;
            for _ in 0..400 {
                view.apply(CameraIntent::ZoomAbout { levels: -0.1, anchor_px: anchor, immediate: false });
                // Let the ease fully catch up to this step's target before
                // the next one lands, so a per-step stall can't hide behind
                // an in-flight ease.
                for _ in 0..64 {
                    if !view.advance(Duration::from_millis(16)) {
                        break;
                    }
                }
                if view.camera.zoom >= last_zoom - 1e-9 && view.camera.zoom > MIN_ZOOM + 1e-6 {
                    stalled_steps += 1;
                }
                last_zoom = view.camera.zoom;
            }
            assert_eq!(
                stalled_steps, 0,
                "lat={lat} pitch={pitch_deg}: a zoom-out step must never leave zoom unchanged \
                 short of the minimum -- that is exactly the stuck state the bug report described"
            );
            assert!(
                (view.camera.zoom - MIN_ZOOM).abs() < 1e-6,
                "lat={lat} pitch={pitch_deg}: 400 zoom-out steps must reach the shared minimum, got {}",
                view.camera.zoom
            );
            assert!(view.camera.zoom.is_finite() && view.camera.center_lat.is_finite());
        }
    }
}

/// Cursor-anchored gestures on the GLOBE frame (below z12): the point drawn
/// under the cursor is held there, measured through the renderer's own
/// forward projection. Before this, every one of them unprojected onto the
/// flat plane, and the point slid by tens to hundreds of pixels.
#[cfg(test)]
mod globe_gesture_tests {
    use super::*;

    fn over_the_us(zoom: f64) -> CameraController {
        CameraController::new(
            Camera {
                center_lon: -98.0,
                center_lat: 39.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1400.0,
                viewport_height_px: 900.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            MIN_ZOOM,
            MAX_ZOOM,
        )
    }

    fn picked(c: &CameraController, p: (f32, f32)) -> glam::DVec2 {
        crate::render::globe::unproject_to_world_on_globe(c.camera(), true, p.0, p.1).expect("on the planet")
    }

    fn slip(c: &CameraController, world: glam::DVec2, p: (f32, f32)) -> f64 {
        // In the world copy the camera is in: what the frame draws there.
        let size = f64::from(crate::render::WORLD_SIZE);
        let x = world.x + ((c.camera().world_center_f64().x - world.x) / size).round() * size;
        let (x, y) = crate::render::globe::drawn_pixel_on_globe(c.camera(), (x, world.y)).expect("visible");
        f64::from(x - p.0).hypot(f64::from(y - p.1))
    }

    #[test]
    fn a_wheel_zoom_on_the_globe_holds_the_point_under_the_cursor_every_frame() {
        for (zoom, levels, cursor) in [(4.0, 1.0, (1100.0, 250.0)), (5.0, -1.0, (300.0, 700.0)), (10.6, 1.0, (1200.0, 150.0))] {
            let mut c = over_the_us(zoom);
            let held = picked(&c, cursor);
            c.apply(CameraIntent::ZoomAbout { levels, anchor_px: cursor, immediate: false });
            let mut worst = 0f64;
            for _ in 0..200 {
                let moving = c.advance(Duration::from_millis(16));
                worst = worst.max(slip(&c, held, cursor));
                if !moving {
                    break;
                }
            }
            assert!((c.camera().zoom - (zoom + levels)).abs() < 1e-3, "it arrived");
            assert!(worst < 0.5, "z{zoom} {levels:+}: the point under the cursor slid {worst:.3} px");
        }
    }

    #[test]
    fn a_pinch_and_a_twist_on_the_globe_hold_the_point_under_the_fingers() {
        let mut c = over_the_us(4.0);
        let fingers = (1000.0, 300.0);
        let held = picked(&c, fingers);
        for _ in 0..10 {
            c.apply(CameraIntent::PinchZoomBy { levels: 0.1, anchor_px: fingers });
            assert!(slip(&c, held, fingers) < 0.5, "pinch slip {:.3} px", slip(&c, held, fingers));
        }
        for _ in 0..10 {
            c.apply(CameraIntent::RotateBy { radians: 0.05, anchor_px: Some(fingers) });
            assert!(slip(&c, held, fingers) < 0.5, "twist slip {:.3} px", slip(&c, held, fingers));
        }
    }

    #[test]
    fn a_drag_on_the_globe_moves_the_grabbed_place_with_the_cursor() {
        for zoom in [3.0, 4.5, 6.0, 11.5] {
            let mut c = over_the_us(zoom);
            let mut at = (1000.0f32, 300.0f32);
            let grabbed = picked(&c, at);
            for _ in 0..12 {
                let (dx, dy) = (-40.0, 25.0);
                c.apply(CameraIntent::PanBy { dx_px: dx, dy_px: dy, anchor_px: at });
                at = (at.0 + dx, at.1 + dy);
                let s = slip(&c, grabbed, at);
                assert!(s < 0.5, "z{zoom}: the grabbed place is {s:.3} px from the cursor");
            }
        }
    }

    #[test]
    fn an_offset_ease_on_the_globe_draws_the_centre_at_the_offset() {
        let mut c = over_the_us(4.0);
        let target = (-80.0, 35.0);
        let offset = (0.0f32, 225.0f32);
        c.jump_to(CameraTarget { center: Some(target), anchor_offset_px: offset, ..CameraTarget::default() });
        let world = crate::overlay::geom::lon_lat_to_world(target.0, target.1);
        let s = slip(&c, glam::DVec2::new(world.0, world.1), (700.0 + offset.0, 450.0 + offset.1));
        assert!(s < 0.5, "the eased-to centre is drawn {s:.3} px from its offset");
    }

    /// Centre movement of one drag, in the frame's own centre pixels.
    fn centre_moved_px(before: &Camera, after: &Camera) -> f64 {
        let a = crate::overlay::geom::lon_lat_to_world(before.center_lon, before.center_lat);
        let b = crate::overlay::geom::lon_lat_to_world(after.center_lon, after.center_lat);
        (b.0 - a.0).hypot(b.1 - a.1) * 2f64.powf(before.zoom)
    }

    /// The first row (from the top, at column `x`) that picks the planet.
    fn limb_row(c: &CameraController, x: f32) -> f32 {
        (0..c.camera().viewport_height_px as i32)
            .map(|y| y as f32)
            .find(|&y| crate::render::globe::unproject_to_world_on_globe(c.camera(), true, x, y).is_some())
            .expect("the planet is on screen")
    }

    /// **A pitched globe drag near the silhouette does not fling the map**
    /// (review IM1 of 7d95600). z10.5 at pitch 60 puts the planet's limb
    /// ~70 px under the top edge; exact tracking there carried a 25 px drag
    /// 2,500-4,300 centre px (the ray grazes the sphere), where the flat
    /// grab caps the rate at `GRAB_MAX_PITCH_SCALE` times the centre's. The
    /// globe drag now obeys the same cap (`pan_surface_point`'s handover),
    /// grabbed anywhere from the limb down, and a drag carried up past the
    /// limb keeps moving the map the same way at a bounded rate.
    #[test]
    fn a_pitched_globe_drag_near_the_limb_is_rate_clamped() {
        let mut c = over_the_us(10.5);
        c.jump_to(CameraTarget { pitch: Some(MAX_PITCH), ..CameraTarget::default() });
        let yl = limb_row(&c, 700.0);
        assert!(yl > 10.0 && yl < 200.0, "the fixture must show the limb near the top: row {yl}");
        let drag = |anchor: (f32, f32), dy: f32| {
            let mut d = c.clone();
            d.apply(CameraIntent::PanBy { dx_px: 0.0, dy_px: dy, anchor_px: anchor });
            (centre_moved_px(c.camera(), d.camera()), d.camera().center_lat - c.camera().center_lat)
        };
        let (at_centre, _) = drag((700.0, 450.0), 25.0);
        let cap = GRAB_MAX_PITCH_SCALE * at_centre * 1.1;
        for off in [1.0, 3.0, 8.0, 20.0, 60.0, 150.0] {
            let (moved, dlat) = drag((700.0, yl + off), 25.0);
            assert!(moved <= cap, "grabbed {off} px under the limb: a 25 px drag moved {moved:.0} centre px (cap {cap:.0})");
            assert!(dlat > 0.0, "a downward drag moves the centre north");
        }
        // Up and over the limb in 5 px steps: always the same direction,
        // never faster than the cap.
        let mut d = c.clone();
        let mut y = yl + 40.0;
        for _ in 0..16 {
            let before = *d.camera();
            d.apply(CameraIntent::PanBy { dx_px: 0.0, dy_px: -5.0, anchor_px: (700.0, y) });
            let moved = centre_moved_px(&before, d.camera());
            assert!(moved > 0.0 && moved <= cap / 5.0, "row {y}: a 5 px step moved {moved:.1} centre px");
            assert!(d.camera().center_lat < before.center_lat, "row {y}: an upward drag moves the centre south");
            y -= 5.0;
        }
    }

    /// The globe gestures with the camera pitched AND turned: a sign error
    /// in the pick or the solve for bearing or pitch would pass every
    /// north-up test (review M2).
    #[test]
    fn pitched_and_turned_globe_gestures_hold_the_point_under_the_cursor() {
        for zoom in [4.0, 11.5] {
            let mut c = over_the_us(zoom);
            c.jump_to(CameraTarget { pitch: Some(0.7), bearing: Some(2.1), ..CameraTarget::default() });
            // A drag.
            let mut at = (820.0f32, 560.0f32);
            let grabbed = picked(&c, at);
            for _ in 0..8 {
                c.apply(CameraIntent::PanBy { dx_px: -30.0, dy_px: -20.0, anchor_px: at });
                at = (at.0 - 30.0, at.1 - 20.0);
                let s = slip(&c, grabbed, at);
                assert!(s < 0.5, "z{zoom} pitched+turned drag: {s:.3} px off the cursor");
            }
            // A wheel zoom, every frame.
            let cursor = (900.0, 600.0);
            let held = picked(&c, cursor);
            c.apply(CameraIntent::ZoomAbout { levels: 0.6, anchor_px: cursor, immediate: false });
            for _ in 0..200 {
                let moving = c.advance(Duration::from_millis(16));
                let s = slip(&c, held, cursor);
                assert!(s < 0.5, "z{zoom} pitched+turned zoom: {s:.3} px off the cursor");
                if !moving {
                    break;
                }
            }
        }
    }

    /// Review M1: a wheel zoom in the z11-z12 blend whose centre crosses
    /// the antimeridian keeps its anchor under the cursor (the blend's flat
    /// half is not periodic, so the anchor must follow the centre's world
    /// copy).
    #[test]
    fn a_blend_band_zoom_across_the_antimeridian_holds_its_anchor() {
        let mut c = over_the_us(11.2);
        c.jump_to(CameraTarget { center: Some((179.97, 10.0)), ..CameraTarget::default() });
        let cursor = (1300.0, 450.0);
        let held = picked(&c, cursor);
        c.apply(CameraIntent::ZoomAbout { levels: 0.7, anchor_px: cursor, immediate: false });
        let mut crossed = false;
        for _ in 0..200 {
            let moving = c.advance(Duration::from_millis(16));
            crossed |= c.camera().center_lon < 0.0;
            let s = slip(&c, held, cursor);
            assert!(s < 0.5, "centre {:.4}: the anchor is {s:.3} px off the cursor", c.camera().center_lon);
            if !moving {
                break;
            }
        }
        assert!(crossed, "the fixture must carry the centre across 180");
    }

    #[test]
    fn with_the_globe_off_a_low_zoom_drag_is_the_flat_one() {
        let (mut on, mut off) = (over_the_us(4.0), over_the_us(4.0));
        off.set_globe_enabled(false);
        let pan = CameraIntent::PanBy { dx_px: -200.0, dy_px: 0.0, anchor_px: (1100.0, 250.0) };
        on.apply(pan);
        off.apply(pan);
        let flat = over_the_us(4.0).camera().center_lon + 200.0 * 360.0 / (512.0 * 16.0);
        assert!((off.camera().center_lon - flat).abs() < 1e-9, "flat pan unchanged");
        assert!((on.camera().center_lon - off.camera().center_lon).abs() > 0.5, "the globe drag differs");
    }
}

/// **Dragging the globe near a pole moves it every way** (the report: "when
/// the globe is looking from the north, hard to rotate it in some
/// directions"). With the centre near or on the latitude limit, a drag used
/// to fall off the exact grab onto a swing whose longitude is ill-conditioned
/// at the pole and whose latitude the clamp then cut: some directions barely
/// moved, some went sideways or backwards. Measured as the motion of the place
/// under the cursor, summed over a 100 px drag in ten steps: along the drag,
/// and across it.
#[cfg(test)]
mod pole_drag_tests {
    use super::*;

    fn at(lat: f64, zoom: f64, pitch: f32, bearing: f32) -> CameraController {
        let mut c = CameraController::new(
            Camera {
                center_lon: 20.0,
                center_lat: lat,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1400.0,
                viewport_height_px: 900.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        c.jump_to(CameraTarget { pitch: Some(pitch), bearing: Some(bearing), ..CameraTarget::default() });
        c
    }

    /// Ten 10 px steps from `start` along unit `dir`: the place under the
    /// cursor before each step, where it is drawn after, summed as (along,
    /// across) the drag.
    fn drag(c: &mut CameraController, start: (f32, f32), dir: (f32, f32)) -> (f64, f64) {
        let size = f64::from(crate::render::WORLD_SIZE);
        let mut cur = start;
        let (mut along, mut across) = (0.0, 0.0);
        for _ in 0..10 {
            let (dx, dy) = (dir.0 * 10.0, dir.1 * 10.0);
            let w = crate::render::globe::unproject_to_world_on_globe(c.camera(), true, cur.0, cur.1).expect("on the planet");
            c.apply(CameraIntent::PanBy { dx_px: dx, dy_px: dy, anchor_px: cur });
            let x = w.x + ((c.camera().world_center_f64().x - w.x) / size).round() * size;
            let p = crate::render::globe::drawn_pixel_on_globe(c.camera(), (x, w.y)).expect("still drawn");
            let m = (f64::from(p.0 - cur.0), f64::from(p.1 - cur.1));
            along += m.0 * f64::from(dir.0) + m.1 * f64::from(dir.1);
            across += m.1 * f64::from(dir.0) - m.0 * f64::from(dir.1);
            cur = (cur.0 + dx, cur.1 + dy);
        }
        (along, across)
    }

    const S: f32 = std::f32::consts::FRAC_1_SQRT_2;
    const DIRECTIONS: [(&str, f32, f32); 8] =
        [("N", 0.0, -1.0), ("NE", S, -S), ("E", 1.0, 0.0), ("SE", S, S), ("S", 0.0, 1.0), ("SW", -S, S), ("W", -1.0, 0.0), ("NW", -S, -S)];

    #[test]
    fn a_globe_drag_near_the_pole_moves_the_view_every_way() {
        let top = MAX_MERCATOR_LAT;
        let cases = [
            (85.0, 1.5, 0.0, 0.0),
            (top, 2.0, 0.0, 0.0),
            (top, 2.0, 0.6, 0.0),
            (84.0, 1.5, 0.0, 1.2),
            (top, 2.0, 0.5, -2.0),
            (-top, 1.5, 0.0, 0.0),
            (top, 0.0, 0.0, 0.0),
        ];
        let mut failures = Vec::new();
        for (lat, zoom, pitch, bearing) in cases {
            for anchor in [(900.0f32, 300.0f32), (500.0, 600.0)] {
                if zoom == 0.0 {
                    // At z0 the pole sits ~80 px from the centre: every grab
                    // is near its line, and following would need more than
                    // the gesture's POLAR_TURN_MAX_TWIST of turn.
                    continue;
                }
                for (name, dx, dy) in DIRECTIONS {
                    if (lat, bearing, anchor, name) == (84.0, 1.2, (500.0, 600.0), "SW") {
                        // The exact centre lies past the limit only part of
                        // the way: off the clamp the bearing holds (review
                        // ruling) and the grab slips ~28 px, as MapLibre's.
                        continue;
                    }
                    let mut c = at(lat, zoom, pitch, bearing);
                    let (along, across) = drag(&mut c, anchor, (dx, dy));
                    // Pitched on the limit, grabbed near the pole's side,
                    // the exact answers move the centre over 1.5 degrees a
                    // 10 px step (the fold the step cap refuses); the pinned
                    // turn takes those drags and the grab slips.
                    let folded = lat == top && pitch > 0.0 && anchor == (900.0, 300.0);
                    let ok = if folded {
                        (55.0..=125.0).contains(&along) && across.abs() <= 35.0
                    } else {
                        (85.0..=115.0).contains(&along) && across.abs() <= 15.0
                    };
                    if !ok {
                        failures.push(format!(
                            "lat {lat:.2} z{zoom} pitch {pitch} bearing {bearing} grab {anchor:?} {name}: {along:.0} along, {across:.0} across"
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "a 100 px drag moved the grabbed place:\n{}", failures.join("\n"));
    }

    /// Grabbed at the screen centre, straight under the pole: every
    /// direction but one follows the cursor. The one is the drag pulling
    /// the view further over the pole along the pole's own screen line
    /// (S here), which the latitude limit forbids and no turn of the view
    /// can stand in for; it must stay put -- not slide sideways, not jump.
    #[test]
    fn a_drag_grabbed_under_the_pole_follows_but_for_the_limit() {
        for (zoom, pitch) in [(1.5, 0.0), (2.0, 0.6)] {
            for (name, dx, dy) in DIRECTIONS {
                let mut c = at(MAX_MERCATOR_LAT, zoom, pitch, 0.0);
                let (along, across) = drag(&mut c, (700.0, 450.0), (dx, dy));
                if name == "S" {
                    assert!(along.abs() < 5.0 && across.abs() < 5.0, "z{zoom} S: {along:.0} along, {across:.0} across");
                } else {
                    assert!(
                        (75.0..=110.0).contains(&along) && across.abs() <= 25.0,
                        "z{zoom} pitch {pitch} {name}: {along:.0} along, {across:.0} across"
                    );
                }
            }
        }
    }

    /// The planet's radius on screen, in pixels.
    fn globe_radius_px(c: &CameraController) -> f64 {
        crate::render::globe::globe_radius_world(c.camera()) * 2f64.powf(c.camera().zoom)
    }

    /// Drags down at the centre in 4 px steps (the centre goes north)
    /// until it passes `lat`; returns the steps taken.
    fn drag_north_to(c: &mut CameraController, lat: f64) -> usize {
        let mut steps = 0;
        while c.camera().center_lat < lat && steps < 2000 {
            c.apply(CameraIntent::PanBy { dx_px: 0.0, dy_px: 4.0, anchor_px: (700.0, 450.0) });
            steps += 1;
        }
        steps
    }

    /// MapLibre's `getZoomAdjustment` on globe drags: the planet keeps its
    /// size on screen while a drag carries the centre from the equator to
    /// lat 80 (it grew 1/cos(80) = 5.8x before), and the grabbed place
    /// still follows the cursor exactly.
    #[test]
    fn a_globe_drag_north_keeps_the_planet_its_size() {
        for (zoom, pitch) in [(1.0, 0.0), (1.0, 0.5), (4.0, 0.0)] {
            let mut c = at(0.0, zoom, pitch, 0.0);
            let before = globe_radius_px(&c);
            let steps = drag_north_to(&mut c, 80.0);
            let after = globe_radius_px(&c);
            assert!(steps < 2000, "z{zoom}: the drag never reached lat 80");
            assert!(
                (after / before - 1.0).abs() < 0.01,
                "z{zoom} pitch {pitch}: radius {before:.1} px at the equator, {after:.1} px at lat {:.1}",
                c.camera().center_lat
            );
            let want = zoom + c.camera().center_lat.to_radians().cos().log2();
            assert!((c.camera().zoom - want).abs() < 1e-6, "z{zoom}: zoom {} not {want}", c.camera().zoom);
        }
        // And the drag is still an exact grab with the zoom moving under it.
        let mut c = at(40.0, 3.0, 0.0, 0.0);
        let (along, across) = drag(&mut c, (900.0, 300.0), (0.0, 1.0));
        assert!((along - 100.0).abs() < 1.0 && across.abs() < 1.0, "{along:.2} along, {across:.2} across");
    }

    /// The adjustment never takes the zoom out of the controller's range:
    /// at the floor the planet grows again, as it always did.
    #[test]
    fn the_globe_size_adjustment_stays_in_the_zoom_range() {
        let mut c = at(0.0, -1.0, 0.0, 0.0);
        drag_north_to(&mut c, 80.0);
        assert!(c.camera().zoom >= MIN_ZOOM, "zoom {} under the floor", c.camera().zoom);
        assert!((c.camera().zoom - MIN_ZOOM).abs() < 1e-9, "it rests on the floor");
        let mut space = at(0.0, -1.0, 0.0, 0.0);
        space.set_zoom_range(SPACE_MIN_ZOOM, MAX_ZOOM);
        drag_north_to(&mut space, 80.0);
        let want = -1.0 + space.camera().center_lat.to_radians().cos().log2();
        assert!(space.camera().zoom >= SPACE_MIN_ZOOM && (space.camera().zoom - want).abs() < 1e-6);
        // Near the ceiling (z11.9, inside the blend) the zoom only falls
        // going north, and comes back going south, never above MAX_ZOOM.
        let mut high = at(0.0, 11.9, 0.0, 0.0);
        for _ in 0..50 {
            high.apply(CameraIntent::PanBy { dx_px: 0.0, dy_px: -40.0, anchor_px: (700.0, 450.0) });
            assert!(high.camera().zoom <= MAX_ZOOM && high.camera().zoom >= MIN_ZOOM);
        }
    }

    /// The flat map keeps its zoom through any pan: globe off at z2 (z1
    /// is below Plain's cover zoom for this viewport, `plain_cover_zoom`),
    /// and the flat regime (z14) with the globe on.
    #[test]
    fn a_flat_pan_leaves_the_zoom_alone() {
        let mut off = at(0.0, 2.0, 0.0, 0.0);
        off.set_globe_enabled(false);
        drag_north_to(&mut off, 60.0);
        assert_eq!(off.camera().zoom, 2.0);
        let mut flat = at(50.0, 14.0, 0.0, 0.0);
        for _ in 0..20 {
            flat.apply(CameraIntent::PanBy { dx_px: 0.0, dy_px: 300.0, anchor_px: (700.0, 450.0) });
        }
        assert!(flat.camera().center_lat > 50.0);
        assert_eq!(flat.camera().zoom, 14.0);
    }

    /// **Review ruling: a drag turns the bearing only while the centre is
    /// pinned ON the latitude limit**, and never more than
    /// [`POLAR_TURN_MAX_TWIST`] a gesture. The review measured 20-30 degrees
    /// of drift on E/SE drags at z1.5 from lat 72-85 when the fallback turned
    /// the view anywhere in the polar band.
    #[test]
    fn a_globe_drag_turns_the_bearing_only_on_the_limit() {
        let mut report = Vec::new();
        for lat in [72.0, 75.0, 78.0, 80.0, 82.0, 84.0] {
            for (name, dx, dy) in DIRECTIONS {
                let mut c = at(lat, 1.5, 0.0, 0.0);
                c.apply(CameraIntent::GestureBegan);
                let mut cur = (900.0f32, 300.0f32);
                let mut hit_clamp = false;
                for _ in 0..10 {
                    c.apply(CameraIntent::PanBy { dx_px: dx * 10.0, dy_px: dy * 10.0, anchor_px: cur });
                    hit_clamp |= c.camera().center_lat >= MAX_MERCATOR_LAT - 1e-9;
                    cur = (cur.0 + dx * 10.0, cur.1 + dy * 10.0);
                }
                let turned = f64::from(shortest_bearing_delta(0.0, c.camera().bearing)).to_degrees().abs();
                report.push(format!("lat {lat} {name}: {turned:.2} deg{}", if hit_clamp { " (pinned)" } else { "" }));
                assert!(turned <= 45.0 + 1e-6, "lat {lat} {name}: turned {turned:.2} deg, over the cap");
                assert!(hit_clamp || turned <= 1e-4, "lat {lat} {name}: turned {turned:.6} deg off the limit");
            }
        }
        println!("{}", report.join("\n"));
    }

    /// Review: the floor clamp does not ratchet. A drag from the equator to
    /// lat 80 and back returns to its start zoom, starting on the floor too.
    #[test]
    fn a_globe_drag_there_and_back_returns_to_its_zoom() {
        for zoom in [1.0, MIN_ZOOM, -1.0] {
            let mut c = at(0.0, zoom, 0.0, 0.0);
            drag_north_to(&mut c, 80.0);
            let mut steps = 0;
            while c.camera().center_lat > 0.0 && steps < 4000 {
                c.apply(CameraIntent::PanBy { dx_px: 0.0, dy_px: -4.0, anchor_px: (700.0, 450.0) });
                steps += 1;
            }
            // Back on the equator (within a step): the size term there is ~0.
            let back = zoom + c.camera().center_lat.to_radians().cos().log2();
            assert!((c.camera().zoom - back.max(MIN_ZOOM)).abs() < 0.01, "z{zoom}: came back at z{}", c.camera().zoom);
            assert!((c.camera().zoom - zoom).abs() < 0.01, "z{zoom}: came back at z{}", c.camera().zoom);
        }
    }

    /// Review: a drag south out of the z11-z12 blend flips the frame to flat
    /// once and stays there (the flat pan does not adjust the zoom, so it
    /// cannot carry the camera back into the blend).
    #[test]
    fn a_drag_out_of_the_blend_flips_to_flat_once() {
        // z11.99: the drag south needs only ~0.06 deg to lift the zoom past
        // the blend's z12 top (log2(cos 59.9 / cos 60) = 0.0145 per 0.1 deg).
        let mut c = at(60.0, 11.99, 0.0, 0.0);
        let mut was_globe = c.globe_at(c.camera());
        assert!(was_globe, "the fixture starts inside the blend");
        let mut flips = 0;
        for _ in 0..400 {
            c.apply(CameraIntent::PanBy { dx_px: 0.0, dy_px: -40.0, anchor_px: (700.0, 450.0) });
            let globe = c.globe_at(c.camera());
            flips += usize::from(globe != was_globe);
            was_globe = globe;
            assert!(c.camera().zoom <= MAX_ZOOM);
        }
        assert!(c.camera().center_lat < 59.0, "the drag went south: lat {}", c.camera().center_lat);
        assert_eq!(flips, 1, "globe/flat flips: {flips}");
        assert!(!was_globe);
    }

    /// Review: a non-finite drag or anchor is a no-op.
    #[test]
    fn a_non_finite_pan_moves_nothing() {
        for (dx, dy, anchor) in [
            (f32::NAN, 0.0, (700.0, 450.0)),
            (0.0, f32::INFINITY, (700.0, 450.0)),
            (5.0, 5.0, (f32::NAN, 450.0)),
            (5.0, 5.0, (700.0, f32::NEG_INFINITY)),
        ] {
            for zoom in [2.0, 14.0] {
                let mut c = at(40.0, zoom, 0.3, 0.2);
                let before = *c.camera();
                c.apply(CameraIntent::PanBy { dx_px: dx, dy_px: dy, anchor_px: anchor });
                assert_eq!(*c.camera(), before, "z{zoom} ({dx}, {dy}) at {anchor:?} moved the camera");
            }
        }
    }

    /// Re-review MAJOR 1: the pinned path jumped the centre 80.89 -> 85.05 in
    /// one 10 px step (lat 75, z1.5, grab (900,300), SE). No 10 px step
    /// moves the centre's latitude more than 1.5 degrees while the centre
    /// is north of lat 70, in any direction.
    #[test]
    fn a_polar_drag_step_never_jumps_the_latitude() {
        let mut worst = (0.0f64, String::new());
        for lat in [72.0, 75.0, 78.0, 80.0, 82.0, 84.0] {
            for (name, dx, dy) in DIRECTIONS {
                let mut c = at(lat, 1.5, 0.0, 0.0);
                let mut cur = (900.0f32, 300.0f32);
                for step in 0..40 {
                    if crate::render::globe::unproject_to_world_on_globe(c.camera(), true, cur.0, cur.1).is_none() {
                        break;
                    }
                    let before = c.camera().center_lat;
                    c.apply(CameraIntent::PanBy { dx_px: dx * 10.0, dy_px: dy * 10.0, anchor_px: cur });
                    let moved = (c.camera().center_lat - before).abs();
                    // Scored while the centre is in or near the polar band
                    // (a long N drag carries the grab up to the limb, well
                    // south, where the limb's rate legitimately runs higher).
                    if before >= 70.0 && moved > worst.0 {
                        worst = (moved, format!("lat {lat} {name} step {step}: {before:.3} -> {:.3}", c.camera().center_lat));
                    }
                    cur = (cur.0 + dx * 10.0, cur.1 + dy * 10.0);
                }
            }
        }
        println!("largest latitude step: {:.3} deg ({})", worst.0, worst.1);
        assert!(worst.0 <= 1.5, "a 10 px step moved the latitude {:.3} deg ({})", worst.0, worst.1);
    }

    /// Re-review MAJOR 2: the drag's unclamped zoom is dropped by every other
    /// zoom writer. Probe A: at the floor at lat 80, pinch in and back out
    /// to the floor, then drag to the equator -- the planet keeps its size
    /// (it stayed at the floor, and shrank 5.8x, while the stale value held).
    #[test]
    fn a_pinch_or_jump_to_the_floor_then_a_drag_south_keeps_the_size() {
        let mut pinched = at(0.0, 0.0, 0.0, 0.0);
        drag_north_to(&mut pinched, 80.0);
        assert_eq!(pinched.camera().zoom, MIN_ZOOM, "the fixture reaches the floor");
        pinched.apply(CameraIntent::PinchZoomBy { levels: 1.0, anchor_px: (700.0, 450.0) });
        pinched.apply(CameraIntent::PinchZoomBy { levels: -1.0, anchor_px: (700.0, 450.0) });
        assert_eq!(pinched.camera().zoom, MIN_ZOOM);
        let mut jumped = at(0.0, 3.0, 0.0, 0.0);
        jumped.jump_to(CameraTarget { center: Some((20.0, 80.0)), zoom: Some(MIN_ZOOM), ..CameraTarget::default() });
        for (name, mut c) in [("pinch", pinched), ("jump_to", jumped)] {
            let before = globe_radius_px(&c);
            let mut steps = 0;
            while c.camera().center_lat > 1.0 && steps < 4000 {
                c.apply(CameraIntent::PanBy { dx_px: 0.0, dy_px: -4.0, anchor_px: (700.0, 450.0) });
                steps += 1;
            }
            let after = globe_radius_px(&c);
            assert!((after / before - 1.0).abs() < 0.01, "{name}: radius {before:.1} px -> {after:.1} px");
        }
    }

    /// z0 on the limit: the pole sits ~80 px from the centre, so a grab
    /// cannot follow in full, but every drag still moves the place toward
    /// the cursor and turns the view at most the gesture's cap.
    #[test]
    fn a_z0_drag_on_the_limit_moves_toward_the_cursor() {
        for anchor in [(900.0f32, 300.0f32), (500.0, 600.0)] {
            for (name, dx, dy) in DIRECTIONS {
                let mut c = at(MAX_MERCATOR_LAT, 0.0, 0.0, 0.0);
                c.apply(CameraIntent::GestureBegan);
                let (along, _) = drag(&mut c, anchor, (dx, dy));
                let turned = f64::from(shortest_bearing_delta(0.0, c.camera().bearing)).to_degrees().abs();
                assert!(along > 0.0, "{anchor:?} {name}: {along:.1} along");
                assert!(turned <= 45.0 + 1e-3, "{anchor:?} {name}: turned {turned:.4}");
            }
        }
    }

    /// A long pinned drag turns exactly the cap and no more; the next
    /// gesture may turn again.
    #[test]
    fn the_pinned_twist_cap_is_per_gesture() {
        let mut c = at(MAX_MERCATOR_LAT, 1.5, 0.0, 0.0);
        let pinned_drag = |c: &mut CameraController| {
            c.apply(CameraIntent::GestureBegan);
            let start = c.camera().bearing;
            let mut cur = (900.0f32, 300.0f32);
            for _ in 0..60 {
                c.apply(CameraIntent::PanBy { dx_px: 7.0, dy_px: 7.0, anchor_px: cur });
                cur = (cur.0 + 7.0, cur.1 + 7.0);
            }
            c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
            f64::from(shortest_bearing_delta(start, c.camera().bearing)).to_degrees().abs()
        };
        let first = pinned_drag(&mut c);
        assert!((first - 45.0).abs() < 1e-3, "the first gesture turned {first:.4} deg, not the cap");
        let second = pinned_drag(&mut c);
        assert!(second > 1.0 && second <= 45.0 + 1e-3, "the second gesture turned {second:.4} deg");
    }

    /// Away from the pole the drag is the exact, bearing-fixed grab it was.
    #[test]
    fn a_globe_drag_clear_of_the_limit_keeps_the_bearing() {
        for (name, dx, dy) in DIRECTIONS {
            let mut c = at(70.0, 2.0, 0.3, 0.4);
            let (along, across) = drag(&mut c, (800.0, 400.0), (dx, dy));
            assert!((along - 100.0).abs() < 1.0 && across.abs() < 1.0, "{name}: {along:.2} along, {across:.2} across");
            assert_eq!(c.camera().bearing, at(70.0, 2.0, 0.3, 0.4).camera().bearing, "{name}: the bearing moved");
        }
    }
}

/// The defect "drag right-left on the globe feels hard", at the
/// camera it was seen at (lon 78.28, lat 18.85, z2.79, bearing 22.79, 1422x801):
/// a horizontal drag holds the grabbed place under the cursor, never
/// turns the view, and changes the zoom only by the planet-size
/// adjustment (`globe_size_zoom_adjustment`). Measured on master
/// 9e1c1b43 and on 689324a8 (before the pole-drag merge): identical, slip
/// under 0.15 px, bearing change 0.0000 deg.
#[cfg(test)]
mod horizontal_drag_feel_tests {
    use super::*;

    fn at(lat: f64, bearing_deg: f32) -> CameraController {
        let mut c = CameraController::new(
            Camera { center_lon: 78.28, center_lat: lat, zoom: 2.79, pitch: 0.0, bearing: 0.0, viewport_width_px: 1422.0, viewport_height_px: 801.0, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        c.jump_to(CameraTarget { bearing: Some(bearing_deg.to_radians()), ..CameraTarget::default() });
        c
    }

    /// Where the place under `p` is drawn after `c` moves.
    fn follow(c: &CameraController, w: glam::DVec2) -> (f32, f32) {
        let size = f64::from(crate::render::WORLD_SIZE);
        let x = w.x + ((c.camera().world_center_f64().x - w.x) / size).round() * size;
        crate::render::globe::drawn_pixel_on_globe(c.camera(), (x, w.y)).expect("still drawn")
    }

    #[test]
    fn a_horizontal_globe_drag_follows_the_cursor_without_turning_the_view() {
        for (lat, bearing) in [(18.85, 22.79f32), (18.85, 0.0), (0.0, 22.79), (60.0, 0.0), (60.0, 22.79)] {
            for anchor in [(711.0f32, 400.5f32), (600.0, 330.0)] {
                for (dist, sign) in [(100.0f32, 1.0f32), (100.0, -1.0), (200.0, 1.0), (200.0, -1.0)] {
                    // Inside 80% of the ~360 px disc: past it the
                    // silhouette falloff slows the drag on purpose (below).
                    if (anchor.0 + sign * dist - 711.0).hypot(anchor.1 - 400.5) > 280.0 {
                        continue;
                    }
                    let mut c = at(lat, bearing);
                    let (b0, z0, lat0) = (c.camera().bearing, c.camera().zoom, c.camera().center_lat);
                    c.apply(CameraIntent::GestureBegan);
                    let w = crate::render::globe::unproject_to_world_on_globe(c.camera(), true, anchor.0, anchor.1).expect("on the planet");
                    let mut cur = anchor;
                    for _ in 0..(dist / 4.0) as usize {
                        c.apply(CameraIntent::PanBy { dx_px: sign * 4.0, dy_px: 0.0, anchor_px: cur });
                        cur.0 += sign * 4.0;
                    }
                    let p = follow(&c, w);
                    let case = format!("lat {lat} bearing {bearing} grab {anchor:?} drag {}", sign * dist);
                    assert!((p.0 - cur.0).hypot(p.1 - cur.1) < 0.5, "{case}: the grabbed place slipped to {p:?}, cursor {cur:?}");
                    assert_eq!(c.camera().bearing, b0, "{case}: the drag turned the view");
                    let expected = crate::render::globe::globe_size_zoom_adjustment(lat0, c.camera().center_lat);
                    assert!((c.camera().zoom - z0 - expected).abs() < 1e-6, "{case}: zoom moved {} not {expected}", c.camera().zoom - z0);
                }
            }
        }
    }

    /// One 4 px step anywhere inside 80% of the drawn disc moves the
    /// grabbed place with the cursor (gain 1). Past that the silhouette
    /// falloff (`pan_surface_point`, MapLibre's `PAN_FALLOFF_BAND`) slows it
    /// on purpose -- gain 0.4-0.9 at 90%, as before the pole-drag merge.
    #[test]
    fn inside_the_disc_a_drag_step_moves_the_place_as_far_as_the_cursor() {
        let c0 = at(18.85, 22.79);
        let mut r = 0.0f32;
        while crate::render::globe::unproject_to_world_on_globe(c0.camera(), true, 711.0 + r, 400.5).is_some() {
            r += 1.0;
        }
        assert!((340.0..380.0).contains(&r), "drawn radius {r}");
        for frac in [0.0f32, 0.3, 0.5, 0.7, 0.8] {
            for ang in (0..8).map(|i| (i as f32 * 45.0).to_radians()) {
                let p = (711.0 + frac * r * ang.cos(), 400.5 - frac * r * ang.sin());
                for dx in [4.0f32, -4.0] {
                    let mut c = at(18.85, 22.79);
                    let w = crate::render::globe::unproject_to_world_on_globe(c.camera(), true, p.0, p.1).expect("on the planet");
                    c.apply(CameraIntent::PanBy { dx_px: dx, dy_px: 0.0, anchor_px: p });
                    let q = follow(&c, w);
                    let (gain, across) = ((q.0 - p.0) / dx, (q.1 - p.1) / dx.abs());
                    assert!((gain - 1.0).abs() < 0.05 && across.abs() < 0.05, "{frac} R at {:.0} deg, dx {dx}: gain {gain:.2}, across {across:.2}", ang.to_degrees());
                }
            }
        }
    }
}

/// User ruling: a drag pinned on the latitude limit turns the view
/// (`turn_at_the_latitude_limit`), and on release the view eases that turn
/// back out over [`POLE_TURN_UNWIND_DURATION`] -- but only that turn. A
/// bearing the user set stays.
#[cfg(test)]
mod pole_unwind_tests {
    use super::*;

    const FRAME: Duration = Duration::from_micros(16_667);

    fn on_the_limit() -> CameraController {
        CameraController::new(
            Camera {
                center_lon: 20.0,
                center_lat: MAX_MERCATOR_LAT,
                zoom: 1.5,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1400.0,
                viewport_height_px: 900.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            MIN_ZOOM,
            MAX_ZOOM,
        )
    }

    /// The long pinned drag of `the_pinned_twist_cap_is_per_gesture`: turns
    /// the full 45 degree cap.
    fn pinned_drag(c: &mut CameraController) {
        c.apply(CameraIntent::GestureBegan);
        let mut cur = (900.0f32, 300.0f32);
        for _ in 0..60 {
            c.apply(CameraIntent::PanBy { dx_px: 7.0, dy_px: 7.0, anchor_px: cur });
            cur = (cur.0 + 7.0, cur.1 + 7.0);
        }
        c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
    }

    /// Frames until settled, with the largest per-frame bearing change.
    fn run_out(c: &mut CameraController) -> (usize, f32) {
        let mut frames = 0;
        let mut largest = 0.0f32;
        let mut last = c.camera().bearing;
        while c.advance(FRAME) {
            frames += 1;
            largest = largest.max(shortest_bearing_delta(last, c.camera().bearing).abs());
            last = c.camera().bearing;
            assert!(frames < 600, "the return never settled");
        }
        largest = largest.max(shortest_bearing_delta(last, c.camera().bearing).abs());
        (frames, largest)
    }

    fn degrees_between(a: f32, b: f32) -> f32 {
        shortest_bearing_delta(a, b).abs().to_degrees()
    }

    #[test]
    fn a_pole_pinned_drag_returns_to_its_starting_bearing_after_release() {
        let mut c = on_the_limit();
        pinned_drag(&mut c);
        let turned = degrees_between(0.0, c.camera().bearing);
        assert!((turned - 45.0).abs() < 1e-3, "the drag turned {turned} deg, not the cap");
        assert!(!c.is_settled(), "a released pole turn is still to be given back");
        let centre = (c.camera().center_lon, c.camera().center_lat, c.camera().zoom);
        let (frames, _) = run_out(&mut c);
        let secs = frames as f64 * FRAME.as_secs_f64();
        assert!((0.3..=0.55).contains(&secs), "the return took {secs:.3} s");
        assert!(degrees_between(0.0, c.camera().bearing) < 1e-3, "landed at {} deg", c.camera().bearing.to_degrees());
        assert!(c.is_settled());
        // Only the bearing moves: the centre stays where the drag left it.
        assert_eq!((c.camera().center_lon, c.camera().center_lat, c.camera().zoom), centre);
    }

    #[test]
    fn a_rotated_view_returns_to_the_users_bearing_not_to_north() {
        let mut c = on_the_limit();
        c.apply(CameraIntent::GestureBegan);
        // Shift-drag: 100 px of rotate.
        for _ in 0..10 {
            c.apply(CameraIntent::RotateBy { radians: rotate_radians_for_drag_px(10.0), anchor_px: None });
        }
        c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        run_out(&mut c);
        let users = c.camera().bearing;
        assert!(users.to_degrees().abs() > 30.0, "the rotate set {} deg", users.to_degrees());
        pinned_drag(&mut c);
        assert!(degrees_between(users, c.camera().bearing) > 40.0, "the pole drag turned the view");
        run_out(&mut c);
        assert!(
            degrees_between(users, c.camera().bearing) < 1e-3,
            "returned to {} deg, not the user's {} deg",
            c.camera().bearing.to_degrees(),
            users.to_degrees()
        );
    }

    #[test]
    fn the_return_never_lurches() {
        let mut c = on_the_limit();
        pinned_drag(&mut c);
        let (_, largest) = run_out(&mut c);
        // A quadratic ease-out's fastest frame is its first: 2 * 45 deg / 450 ms
        // = 200 deg/s, 3.33 deg per 60 Hz frame.
        let bound = 2.0 * 45.0 * FRAME.as_secs_f32() / POLE_TURN_UNWIND_DURATION.as_secs_f32();
        assert!(largest.to_degrees() <= bound * 1.01, "a frame turned {} deg, bound {bound}", largest.to_degrees());
        // A long stall clamps to MAX_ADVANCE_STEP like every other ease.
        let mut c = on_the_limit();
        pinned_drag(&mut c);
        let before = c.camera().bearing;
        c.advance(Duration::from_secs(5));
        let jumped = degrees_between(before, c.camera().bearing);
        let cap = MAX_ADVANCE_STEP.as_secs_f32() / POLE_TURN_UNWIND_DURATION.as_secs_f32() * 2.0 * 45.0;
        assert!(jumped <= cap * 1.01, "a stalled frame turned {jumped} deg, cap {cap}");
    }

    /// An inertial glide's pans hold the return; it starts once they stop
    /// and gives back what the glide added too.
    #[test]
    fn the_return_waits_for_the_glide() {
        let mut c = on_the_limit();
        pinned_drag(&mut c);
        for _ in 0..10 {
            let before = c.camera().bearing;
            assert!(c.advance(FRAME));
            assert_eq!(c.camera().bearing, before, "the unwind turned the view during the glide");
            c.apply(CameraIntent::PanBy { dx_px: 3.0, dy_px: 3.0, anchor_px: (700.0, 450.0) });
        }
        let before = c.camera().bearing;
        assert!(c.advance(FRAME), "held for the last glide step");
        assert_eq!(c.camera().bearing, before, "the unwind turned the view on the last glide frame");
        assert!(degrees_between(0.0, c.camera().bearing) > 1.0, "the glide frames gave nothing back yet");
        run_out(&mut c);
        assert!(degrees_between(0.0, c.camera().bearing) < 1e-3, "landed at {} deg", c.camera().bearing.to_degrees());
    }

    /// Grabbing the map mid-return freezes it; the next release resumes.
    /// A deliberate bearing (reset, a camera set) drops the pole account.
    #[test]
    fn a_grab_freezes_and_a_set_bearing_forgets() {
        let mut c = on_the_limit();
        pinned_drag(&mut c);
        for _ in 0..5 {
            c.advance(FRAME);
        }
        c.apply(CameraIntent::GestureBegan);
        let held = c.camera().bearing;
        assert!(!c.advance(FRAME) && c.camera().bearing == held, "a held map turned");
        c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        run_out(&mut c);
        assert!(degrees_between(0.0, c.camera().bearing) < 1e-3);

        let mut c = on_the_limit();
        pinned_drag(&mut c);
        c.jump_to(CameraTarget { bearing: Some(0.5), ..CameraTarget::default() });
        assert!(!c.advance(FRAME));
        assert_eq!(c.camera().bearing, 0.5, "set_camera's bearing must stay");

        let mut c = on_the_limit();
        pinned_drag(&mut c);
        c.ease_to(CameraTarget { bearing: Some(-0.3), ..CameraTarget::default() }, Duration::from_millis(200));
        run_out(&mut c);
        assert!((c.camera().bearing + 0.3).abs() < 1e-5, "ease_to's bearing must stay: {}", c.camera().bearing);
    }

    #[test]
    fn wrap_turn_is_the_short_way_round() {
        use std::f64::consts::PI;
        assert!((wrap_turn(1.5 * PI) + 0.5 * PI).abs() < 1e-12);
        assert!((wrap_turn(-1.5 * PI) - 0.5 * PI).abs() < 1e-12);
        assert!((wrap_turn(PI) - PI).abs() < 1e-12, "PI stays PI");
        assert!((wrap_turn(-PI) - PI).abs() < 1e-12, "-PI becomes PI");
        assert_eq!(wrap_turn(0.3), 0.3);
    }

    /// Quick grab / re-drag cycles stack the pole turn past 180 degrees
    /// before any frame gives it back (five pinned drags, 45 degrees each,
    /// found by search at z1.5): the account is wrapped when armed, so the
    /// return takes the short way -- 135 degrees, not 225 -- and lands on
    /// the starting bearing.
    #[test]
    fn stacked_pole_turns_past_half_a_turn_return_the_short_way() {
        let mut c = on_the_limit();
        let mut stacked = 0.0f64;
        for (dx, dy, x, y) in [
            (0.0f32, 10.0f32, 500.0f32, 600.0f32),
            (-7.0, 7.0, 700.0, 600.0),
            (10.0, 0.0, 900.0, 600.0),
            (10.0, 0.0, 700.0, 600.0),
            (-7.0, -7.0, 900.0, 600.0),
        ] {
            let before = c.camera().bearing;
            c.apply(CameraIntent::GestureBegan);
            let mut cur = (x, y);
            for _ in 0..60 {
                c.apply(CameraIntent::PanBy { dx_px: dx, dy_px: dy, anchor_px: cur });
                cur = (cur.0 + dx, cur.1 + dy);
            }
            c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
            // A frame or two between grabs: quick, but not instant.
            c.advance(FRAME);
            stacked += f64::from(shortest_bearing_delta(before, c.camera().bearing));
        }
        assert!(stacked.abs() > 200.0f64.to_radians(), "the cycles stacked only {} deg", stacked.to_degrees());
        assert!(c.pole_turn.abs() <= std::f64::consts::PI, "armed unwrapped: {} deg", c.pole_turn.to_degrees());
        let mut travelled = 0.0f32;
        let mut last = c.camera().bearing;
        while c.advance(FRAME) {
            travelled += shortest_bearing_delta(last, c.camera().bearing).abs();
            last = c.camera().bearing;
        }
        travelled += shortest_bearing_delta(last, c.camera().bearing).abs();
        assert!(degrees_between(0.0, c.camera().bearing) < 1e-2, "landed at {} deg", c.camera().bearing.to_degrees());
        assert!(travelled.to_degrees() < 180.0, "the return went the long way: {} deg", travelled.to_degrees());
    }

    /// A release that arrives mid-unwind without a press before it does not
    /// restart the curve.
    #[test]
    fn a_stray_release_does_not_restart_the_return() {
        let mut a = on_the_limit();
        pinned_drag(&mut a);
        let mut b = a.clone();
        for _ in 0..6 {
            a.advance(FRAME);
            b.advance(FRAME);
        }
        b.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        for _ in 0..30 {
            a.advance(FRAME);
            b.advance(FRAME);
            assert_eq!(a.camera().bearing, b.camera().bearing, "the stray release changed the curve");
        }
    }

    /// The north snap judges the bearing the user will be left with once
    /// the pole turn is given back: a deliberate 3 degree rotate inside a
    /// gesture that also turned 45 at the pole snaps to north.
    #[test]
    fn the_north_snap_ignores_the_pole_turn() {
        let mut c = on_the_limit();
        c.apply(CameraIntent::GestureBegan);
        let mut cur = (900.0f32, 300.0f32);
        for _ in 0..60 {
            c.apply(CameraIntent::PanBy { dx_px: 7.0, dy_px: 7.0, anchor_px: cur });
            cur = (cur.0 + 7.0, cur.1 + 7.0);
        }
        c.apply(CameraIntent::RotateBy { radians: 3f32.to_radians(), anchor_px: None });
        assert!(degrees_between(0.0, c.camera().bearing) > 40.0);
        c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        run_out(&mut c);
        assert!(degrees_between(0.0, c.camera().bearing) < 1e-3, "landed at {} deg, not north", c.camera().bearing.to_degrees());

        // And 20 degrees, past the snap window, is kept.
        let mut c = on_the_limit();
        c.apply(CameraIntent::GestureBegan);
        let mut cur = (900.0f32, 300.0f32);
        for _ in 0..60 {
            c.apply(CameraIntent::PanBy { dx_px: 7.0, dy_px: 7.0, anchor_px: cur });
            cur = (cur.0 + 7.0, cur.1 + 7.0);
        }
        c.apply(CameraIntent::RotateBy { radians: 20f32.to_radians(), anchor_px: None });
        c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        run_out(&mut c);
        assert!((degrees_between(0.0, c.camera().bearing) - 20.0).abs() < 1e-2, "landed at {} deg", c.camera().bearing.to_degrees());
    }
}

/// The 2026-10-04 report: "in 3d buildings, when zoom out, to the globe
/// level, globe is not in the center of the screen" (Tilt 60). The camera
/// keeps the user's pitch as an intent and draws `min(intent,
/// max_pitch_at_zoom(zoom))`, so the planet is face-on and centred by z2.5
/// and the tilt comes back on the way in.
#[cfg(test)]
mod low_zoom_pitch_tests {
    use super::*;

    const FRAME: Duration = Duration::from_micros(16_667);
    const W: f32 = 1000.0;
    const H: f32 = 744.0;

    /// Shinjuku at z16, tilted to 60 and turned 30, in a Studio-sized
    /// window -- the buildings panel's pose.
    fn shinjuku_pitched() -> CameraController {
        let mut c = CameraController::new(
            Camera {
                center_lon: 139.7005,
                center_lat: 35.6905,
                zoom: 16.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: W,
                viewport_height_px: H,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        c.jump_to(CameraTarget { pitch: Some(MAX_PITCH), bearing: Some(30f32.to_radians()), ..CameraTarget::default() });
        c
    }

    /// Where the planet's centre is drawn, as a fraction of the frame from
    /// its centre: `(dx / width, dy / height)`.
    fn globe_centre_offset(camera: &Camera) -> (f64, f64) {
        let frame = crate::render::globe::globe_frame(camera);
        let clip = frame.globe_view_proj * glam::DVec4::new(0.0, 0.0, 0.0, 1.0);
        (clip.x / clip.w / 2.0, clip.y / clip.w / 2.0)
    }

    /// Runs frames until settled; returns the largest per-frame pitch
    /// change in degrees.
    fn settle(c: &mut CameraController) -> f32 {
        let mut largest = 0.0f32;
        let mut last = c.camera().pitch;
        let mut frames = 0;
        while c.advance(FRAME) {
            largest = largest.max((c.camera().pitch - last).abs().to_degrees());
            last = c.camera().pitch;
            frames += 1;
            assert!(frames < 2000, "never settled");
        }
        largest.max((c.camera().pitch - last).abs().to_degrees())
    }

    #[test]
    fn the_curve_is_full_above_z5_flat_below_z2_5_and_smooth_between() {
        for z in [5.0, 5.01, 8.0, 16.0, MAX_ZOOM] {
            assert_eq!(max_pitch_at_zoom(z), MAX_PITCH, "z{z}");
        }
        for z in [2.5, 2.0, 1.0, MIN_ZOOM, SPACE_MIN_ZOOM] {
            assert_eq!(max_pitch_at_zoom(z), 0.0, "z{z}");
        }
        assert!((max_pitch_at_zoom(3.75) - MAX_PITCH / 2.0).abs() < 1e-6, "the smoothstep's midpoint");
        assert_eq!(max_pitch_at_zoom(f64::NAN), MAX_PITCH);
        assert_eq!(max_pitch_at_zoom(f64::INFINITY), MAX_PITCH);
        // Monotonic, and no step anywhere: 1/1000 of a zoom level moves it
        // by well under a tenth of a degree, ends included.
        let mut last = max_pitch_at_zoom(1.0);
        let mut z = 1.0;
        while z < 6.0 {
            z += 0.001;
            let p = max_pitch_at_zoom(z);
            assert!(p >= last, "falls at z{z}");
            assert!((p - last).to_degrees() < 0.05, "a step at z{z}: {} deg", (p - last).to_degrees());
            last = p;
        }
        // Zero slope at both ends: the last 1/100 of the band moves it by a
        // vanishing amount.
        assert!((MAX_PITCH - max_pitch_at_zoom(4.99)).to_degrees() < 0.01);
        assert!(max_pitch_at_zoom(2.51).to_degrees() < 0.01);
        // On the globe the curve reads the planet's size: at the equator
        // the zoom itself, over 60 degrees one level more.
        assert_eq!(max_pitch_on_globe(3.0, 0.0), max_pitch_at_zoom(3.0));
        assert!((max_pitch_on_globe(3.0, 60.0) - max_pitch_at_zoom(4.0)).abs() < 1e-5);
        assert_eq!(max_pitch_on_globe(3.0, f64::NAN), max_pitch_at_zoom(3.0));
    }

    /// The headline: wheel out from z16 at 60 to z1 and the planet is
    /// centred at z<=2; wheel back in and 60 comes back.
    #[test]
    fn wheeling_out_to_the_globe_centres_it_and_wheeling_in_gives_the_tilt_back() {
        let mut c = shinjuku_pitched();
        let mid = (W / 2.0, H / 2.0);
        let mut worst_step = 0.0f32;
        let mut seen_low = false;
        while c.camera().zoom > 1.0 + 1e-6 {
            c.apply(CameraIntent::ZoomAbout { levels: (-0.25_f64).max(1.0 - c.camera().zoom), anchor_px: mid, immediate: false });
            let mut last = c.camera().pitch;
            while c.advance(FRAME) {
                worst_step = worst_step.max((c.camera().pitch - last).abs().to_degrees());
                last = c.camera().pitch;
                let cam = *c.camera();
                assert!(cam.pitch <= max_pitch_on_globe(cam.zoom, cam.center_lat) + 1e-4, "z{} draws {} deg", cam.zoom, cam.pitch.to_degrees());
                if cam.zoom <= 2.0 {
                    seen_low = true;
                    let (dx, dy) = globe_centre_offset(&cam);
                    assert!(dx.abs() < 0.02 && dy.abs() < 0.02, "z{:.2}: the globe's centre is ({dx:.3}, {dy:.3}) of the frame off", cam.zoom);
                }
            }
            assert_eq!(c.pitch_intent(), MAX_PITCH, "the chosen tilt survives at z{}", c.camera().zoom);
        }
        assert!(seen_low);
        assert!((c.camera().zoom - 1.0).abs() < 1e-6);
        assert_eq!(c.camera().pitch, 0.0);
        let (dx, dy) = globe_centre_offset(c.camera());
        assert!(dx.abs() < 1e-6 && dy.abs() < 1e-6, "face-on at z1: ({dx}, {dy})");
        assert!(worst_step < 3.0, "the pitch jumped {worst_step} deg in one frame");
        assert!(c.is_settled());

        while c.camera().zoom < 16.0 - 1e-6 {
            c.apply(CameraIntent::ZoomAbout { levels: 0.25_f64.min(16.0 - c.camera().zoom), anchor_px: mid, immediate: false });
            worst_step = worst_step.max(settle(&mut c));
        }
        assert_eq!(c.camera().pitch, MAX_PITCH, "60 again at z{}", c.camera().zoom);
        assert!(worst_step < 3.0, "the pitch jumped {worst_step} deg in one frame");
    }

    /// The same round trip through `ease_to` (the debug API's `/map/ease`,
    /// a host's fly-out): the pitch follows the curve, not a straight line
    /// to zero, and nothing is left chasing a pitch it cannot reach.
    #[test]
    fn an_ease_out_to_z1_and_back_follows_the_curve() {
        let mut c = shinjuku_pitched();
        c.ease_to(CameraTarget { zoom: Some(1.0), ..CameraTarget::default() }, Duration::from_millis(2000));
        // The ease crosses the 2.5-level band in a few frames, so a step
        // is bounded by the curve's steepest slope (1.5 / 2.5 of 60 deg
        // per level, the smoothstep's midpoint) times the zoom it moved.
        let mut last = *c.camera();
        let mut worst_step = 0.0f32;
        while c.advance(FRAME) {
            let cam = *c.camera();
            assert!((cam.pitch - max_pitch_on_globe(cam.zoom, cam.center_lat)).abs() < 1e-5, "z{} draws {} deg", cam.zoom, cam.pitch.to_degrees());
            let bound = 36.0 * (cam.zoom - last.zoom).abs() as f32 + 0.01;
            worst_step = worst_step.max((cam.pitch - last.pitch).abs().to_degrees() / bound);
            last = cam;
        }
        assert!(c.is_settled());
        assert_eq!(c.camera().pitch, 0.0);
        assert_eq!(c.pitch_intent(), MAX_PITCH);
        let (dx, dy) = globe_centre_offset(c.camera());
        assert!(dx.abs() < 0.02 && dy.abs() < 0.02, "({dx}, {dy})");
        assert!(worst_step <= 1.0, "a step {worst_step}x the curve's own");
        c.ease_to(CameraTarget { zoom: Some(16.0), ..CameraTarget::default() }, Duration::from_millis(2000));
        settle(&mut c);
        assert_eq!(c.camera().pitch, MAX_PITCH);
    }

    /// A jump straight to the globe with an explicit pitch keeps it as the
    /// intent; one back down draws it.
    #[test]
    fn a_jump_keeps_an_explicit_pitch_as_the_intent() {
        let mut c = shinjuku_pitched();
        c.jump_to(CameraTarget { zoom: Some(1.0), pitch: Some(0.5), ..CameraTarget::default() });
        assert_eq!(c.camera().pitch, 0.0);
        assert_eq!(c.pitch_intent(), 0.5);
        assert!(c.is_settled(), "nothing chases the held-back pitch");
        assert!(!c.advance(FRAME));
        c.jump_to(CameraTarget { zoom: Some(4.0), ..CameraTarget::default() });
        assert_eq!(c.camera().pitch, 0.5f32.min(max_pitch_on_globe(4.0, c.camera().center_lat)));
        c.jump_to(CameraTarget { zoom: Some(12.0), ..CameraTarget::default() });
        assert_eq!(c.camera().pitch, 0.5);
    }

    /// Tilting while held back: down lowers the choice to what is now
    /// drawn; up never lowers a choice the limit is holding back nor raises
    /// one past what can be seen.
    #[test]
    fn tilting_under_the_limit_moves_what_is_drawn() {
        let mut c = shinjuku_pitched();
        c.jump_to(CameraTarget { zoom: Some(4.0), ..CameraTarget::default() });
        let ceiling = max_pitch_on_globe(4.0, c.camera().center_lat);
        assert_eq!(c.camera().pitch, ceiling);
        c.apply(CameraIntent::TiltBy { radians: 0.1 });
        assert_eq!(c.pitch_intent(), MAX_PITCH, "up keeps the higher choice");
        assert_eq!(c.camera().pitch, ceiling);
        c.apply(CameraIntent::TiltBy { radians: -0.1 });
        assert!((c.pitch_intent() - (ceiling - 0.1)).abs() < 1e-6, "down from what is drawn");
        assert_eq!(c.camera().pitch, c.pitch_intent());
        // At z1 a tilt up from flat changes nothing, seen or unseen.
        c.jump_to(CameraTarget { zoom: Some(1.0), pitch: Some(0.0), ..CameraTarget::default() });
        c.apply(CameraIntent::TiltBy { radians: 0.5 });
        assert_eq!(c.pitch_intent(), 0.0);
        // The keyboard step the same way, through the ease.
        c.jump_to(CameraTarget { zoom: Some(4.0), pitch: Some(MAX_PITCH), ..CameraTarget::default() });
        c.apply(CameraIntent::TiltStep { radians: KEYBOARD_PITCH_STEP });
        settle(&mut c);
        assert_eq!(c.pitch_intent(), MAX_PITCH);
        c.apply(CameraIntent::TiltStep { radians: -KEYBOARD_PITCH_STEP });
        settle(&mut c);
        assert!((c.camera().pitch - (ceiling - KEYBOARD_PITCH_STEP)).abs() < 1e-4, "{}", c.camera().pitch);
        assert!(c.is_settled());
    }

    /// A gesture starting while held back does not throw the choice away.
    #[test]
    fn a_grab_keeps_the_held_back_pitch() {
        let mut c = shinjuku_pitched();
        c.jump_to(CameraTarget { zoom: Some(1.5), ..CameraTarget::default() });
        c.apply(CameraIntent::GestureBegan);
        c.apply(CameraIntent::PanBy { dx_px: 40.0, dy_px: 10.0, anchor_px: (500.0, 372.0) });
        c.apply(CameraIntent::RotateBy { radians: 0.2, anchor_px: Some((500.0, 372.0)) });
        c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        settle(&mut c);
        assert_eq!(c.pitch_intent(), MAX_PITCH);
        assert_eq!(c.camera().pitch, 0.0);
    }

    /// The opt-out, and a flat map: no planet to centre, no limit.
    #[test]
    fn the_limit_is_off_when_the_host_turns_it_off_or_the_globe_is_off() {
        let mut c = shinjuku_pitched();
        c.set_low_zoom_pitch_limit(false);
        c.jump_to(CameraTarget { zoom: Some(1.0), ..CameraTarget::default() });
        assert_eq!(c.camera().pitch, MAX_PITCH);
        c.set_low_zoom_pitch_limit(true);
        assert_eq!(c.camera().pitch, 0.0, "on again, applied at once");
        assert_eq!(c.pitch_intent(), MAX_PITCH);
        c.set_globe_enabled(false);
        assert_eq!(c.camera().pitch, MAX_PITCH, "a flat map keeps its tilt");
    }

    /// The pole's released turn unwinds as before with a held-back pitch.
    #[test]
    fn the_pole_unwind_runs_out_under_a_held_back_pitch() {
        let mut c = CameraController::new(
            Camera {
                center_lon: 20.0,
                center_lat: MAX_MERCATOR_LAT,
                zoom: -1.5, // the planet as large as z2.2 over the equator: held flat
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1400.0,
                viewport_height_px: 900.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        c.jump_to(CameraTarget { pitch: Some(MAX_PITCH), ..CameraTarget::default() });
        assert_eq!(c.camera().pitch, 0.0);
        c.apply(CameraIntent::GestureBegan);
        let mut cur = (900.0f32, 300.0f32);
        for _ in 0..60 {
            c.apply(CameraIntent::PanBy { dx_px: 7.0, dy_px: 7.0, anchor_px: cur });
            cur = (cur.0 + 7.0, cur.1 + 7.0);
        }
        c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
        settle(&mut c);
        assert!(c.is_settled());
        assert!(c.camera().bearing.abs() < 1e-3, "the turn given back: {}", c.camera().bearing);
        assert_eq!(c.pitch_intent(), MAX_PITCH);
        assert!(c.camera().pitch <= max_pitch_on_globe(c.camera().zoom, c.camera().center_lat) + 1e-4);
    }
}

/// The Plain camera constraint (a Plain map smaller than the window must
/// not show grey areas around the map): MapLibre's `defaultConstrain` with `renderWorldCopies: false`.
#[cfg(test)]
mod plain_constrain_tests {
    use super::*;

    /// The world's screen rect (x0, y0, x1, y1) in viewport px for `camera`
    /// at pitch 0, bearing 0.
    fn world_rect(c: &Camera) -> (f64, f64, f64, f64) {
        let world = crate::geo::TILE_SIZE * 2f64.powf(c.zoom);
        let (fx, fy) = crate::geo::lon_lat_to_tile_xy(c.center_lon, c.center_lat, 0.0);
        let (_, top) = crate::geo::lon_lat_to_tile_xy(0.0, 90.0, 0.0);
        let (_, bottom) = crate::geo::lon_lat_to_tile_xy(0.0, -90.0, 0.0);
        let (w2, h2) = (f64::from(c.viewport_width_px) / 2.0, f64::from(c.viewport_height_px) / 2.0);
        (w2 - fx * world, h2 - (fy - top) * world, w2 + (1.0 - fx) * world, h2 + (bottom - fy) * world)
    }

    fn assert_covers(c: &Camera, what: &str) {
        let (x0, y0, x1, y1) = world_rect(c);
        let (w, h) = (f64::from(c.viewport_width_px), f64::from(c.viewport_height_px));
        let tol = 1e-6;
        assert!(x0 <= tol && y0 <= tol && x1 >= w - tol && y1 >= h - tol, "{what}: the world {:?} leaves a band in {w}x{h} at {c:?}", (x0, y0, x1, y1));
    }

    fn plain(zoom: f64, lon: f64, lat: f64, w: f32, h: f32) -> CameraController {
        let mut c = CameraController::new(
            Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: w, viewport_height_px: h, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        // Built on the globe (the default); switched with nothing easing.
        c.set_globe_enabled(false);
        for _ in 0..100 {
            if !c.advance(Duration::from_millis(16)) {
                break;
            }
        }
        c
    }

    #[test]
    fn the_cover_zoom_fills_the_wider_and_the_taller_side() {
        // A 1024-wide viewport needs a 1024-px world: z1, exactly.
        assert!((plain_cover_zoom(1024.0, 300.0) - 1.0).abs() < 1e-9);
        // A tall one: the Mercator square's height (the full world, to
        // +-85.05) covers 2048 px at a hair over z2.
        let z = plain_cover_zoom(300.0, 2048.0);
        assert!(z > 2.0 && z < 2.001, "{z}");
    }

    #[test]
    fn a_pose_inside_the_world_is_untouched() {
        assert_eq!(plain_constrain(103.8, 1.3, 10.0, 1280.0, 800.0), (103.8, 1.3, 10.0));
        assert_eq!(plain_constrain(-74.0, 40.7, 3.0, 1280.0, 800.0), (-74.0, 40.7, 3.0));
    }

    #[test]
    fn a_zoomed_out_plain_camera_fills_the_viewport_on_every_side() {
        for (w, h) in [(1280.0_f32, 800.0_f32), (800.0, 1280.0), (390.0, 844.0), (2560.0, 1440.0), (512.0, 512.0)] {
            for (lon, lat) in [(0.0, 0.0), (179.9, 0.0), (-179.9, 80.0), (30.0, -84.0), (-120.0, 60.0)] {
                for zoom in [0.0, 0.5, 1.0, 2.0, 3.0] {
                    let c = plain(zoom, lon, lat, w, h);
                    assert_covers(c.camera(), &format!("z{zoom} ({lon}, {lat})"));
                    assert!(c.is_settled(), "the constrained camera rests");
                }
            }
        }
    }

    #[test]
    fn the_globe_keeps_its_space_around_the_planet() {
        let mut c = CameraController::new(
            Camera { center_lon: 0.0, center_lat: 0.0, zoom: 0.5, pitch: 0.0, bearing: 0.0, viewport_width_px: 1280.0, viewport_height_px: 800.0, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        c.apply(CameraIntent::ZoomAbout { levels: -0.3, anchor_px: (640.0, 400.0), immediate: true });
        for _ in 0..200 {
            if !c.advance(Duration::from_millis(16)) {
                break;
            }
        }
        assert!(c.camera().zoom < 0.5, "the globe zooms out past the cover zoom: {}", c.camera().zoom);
    }

    #[test]
    fn switching_to_plain_eases_onto_the_cover_zoom_rather_than_snapping() {
        let mut c = CameraController::new(
            Camera { center_lon: 20.0, center_lat: 50.0, zoom: 0.8, pitch: 0.0, bearing: 0.0, viewport_width_px: 1280.0, viewport_height_px: 800.0, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        c.set_globe_enabled(false);
        assert_eq!(c.camera().zoom, 0.8, "the switch frame is the camera the user had");
        assert!(!c.is_settled(), "an ease to the covering pose is running");
        let mut zooms = vec![c.camera().zoom];
        for _ in 0..100 {
            let moving = c.advance(Duration::from_millis(16));
            zooms.push(c.camera().zoom);
            if !moving {
                break;
            }
        }
        assert!(zooms.windows(2).all(|p| p[1] >= p[0]), "the zoom only rises: {zooms:?}");
        assert!(zooms.len() > 5, "over several frames, not one: {}", zooms.len());
        assert!((c.camera().zoom - plain_cover_zoom(1280.0, 800.0)).abs() < 1e-9);
        assert_covers(c.camera(), "after the settle");
        assert!(c.is_settled());
    }

    #[test]
    fn wheel_and_drag_past_the_edge_stop_at_it() {
        let mut c = plain(3.0, 0.0, 0.0, 1280.0, 800.0);
        // Out past the floor: the wheel stops at the cover zoom and rests.
        for _ in 0..10 {
            c.apply(CameraIntent::ZoomAbout { levels: -1.0, anchor_px: (100.0, 100.0), immediate: false });
            for _ in 0..30 {
                c.advance(Duration::from_millis(16));
                assert_covers(c.camera(), "mid zoom-out");
            }
        }
        for _ in 0..300 {
            if !c.advance(Duration::from_millis(16)) {
                break;
            }
        }
        assert!(c.is_settled(), "a zoom-out held at the floor converges");
        assert!((c.camera().zoom - plain_cover_zoom(1280.0, 800.0)).abs() < 1e-6, "{}", c.camera().zoom);
        // Drags north, south, east and west far past the edge.
        let mut c = plain(3.0, 0.0, 0.0, 1280.0, 800.0);
        for (dx, dy) in [(0.0, 5000.0), (0.0, -9000.0), (6000.0, 0.0), (-12000.0, 0.0)] {
            c.apply(CameraIntent::GestureBegan);
            c.apply(CameraIntent::PanBy { dx_px: dx, dy_px: dy, anchor_px: (640.0, 400.0) });
            c.apply(CameraIntent::GestureEnded { rotate_velocity: 0.0 });
            c.advance(Duration::from_millis(16));
            assert_covers(c.camera(), &format!("after a drag of ({dx}, {dy})"));
        }
        // A jump outside the world lands inside it.
        c.jump_to(CameraTarget { center: Some((179.0, 85.0)), zoom: Some(0.0), ..CameraTarget::default() });
        assert_covers(c.camera(), "after a jump");
        // A window made wider than the world re-applies it.
        c.set_viewport(3000.0, 800.0);
        assert_covers(c.camera(), "after a resize");
    }

    /// The browser's trackpad pinch, before and after: Chrome sends a pinch
    /// that doubles the content (one zoom level) as ctrl-wheel events with
    /// `deltaY = -100 ln(scale)` in total, here 36 events at 120 Hz over
    /// 300 ms, drawn at 60 Hz. The OLD web path sent each as an eased
    /// `ZoomAbout` (`-deltaY / 100 * 0.8` levels); every call re-targets from
    /// the drawn zoom, so only the last event of each frame counted and the
    /// ease's undelivered remainder was dropped. The NEW path
    /// (`trackpad_pinch::BrowserPinch::wheel` -> `PinchZoomBy`) applies each
    /// at once. Prints the measurement (`--nocapture`).
    #[test]
    fn a_browser_ctrl_wheel_pinch_tracks_the_fingers_where_the_eased_path_lost_most_of_it() {
        let events = 36;
        let delta_y = -100.0 * 2f64.ln() / f64::from(events);
        let event_dt_ms = 1000.0 / 120.0;
        let frame = Duration::from_micros(16_667);
        let anchor = (640.0, 300.0);
        let run = |new: bool| -> (f64, f64, f64, usize) {
            let mut c = super::tests::test_controller(12.0, 0.0, (1024.0, 768.0));
            let z0 = c.camera().zoom;
            let pinch = crate::trackpad_pinch::BrowserPinch::default();
            let mut first_frame = None;
            let mut next_frame_ms = 16.667;
            for i in 0..events {
                let t = f64::from(i) * event_dt_ms;
                while t >= next_frame_ms {
                    c.advance(frame);
                    next_frame_ms += 16.667;
                    first_frame.get_or_insert(c.camera().zoom - z0);
                }
                if new {
                    let levels = pinch.wheel(true, delta_y, false).expect("a pinch");
                    c.apply(CameraIntent::PinchZoomBy { levels, anchor_px: anchor });
                } else {
                    c.apply(CameraIntent::ZoomAbout { levels: -(delta_y / 100.0) * 0.8, anchor_px: anchor, immediate: true });
                }
            }
            c.advance(frame);
            let at_lift = c.camera().zoom - z0;
            let mut frames_to_settle = 0;
            while !c.is_settled() && frames_to_settle < 1000 {
                c.advance(frame);
                frames_to_settle += 1;
            }
            (first_frame.unwrap_or(0.0), at_lift, c.camera().zoom - z0, frames_to_settle)
        };
        let (old_first, old_lift, old_total, old_settle) = run(false);
        let (new_first, new_lift, new_total, new_settle) = run(true);
        // Three events (t = 0, 8.3, 16.66 ms) land before the first frame.
        let asked_first = 3.0 * -delta_y / (100.0 * std::f64::consts::LN_2);
        eprintln!(
            "pinch of 1.000 level: OLD first frame {old_first:.4} (fingers {asked_first:.4}), at lift {old_lift:.4}, total {old_total:.4}, \
             {old_settle} frames to settle | NEW first frame {new_first:.4}, at lift {new_lift:.4}, total {new_total:.4}, {new_settle} frames to settle"
        );
        assert!((new_total - 1.0).abs() < 1e-9, "the new path is 1:1: {new_total}");
        assert!((new_lift - 1.0).abs() < 1e-9 && new_settle == 0, "and is already there when the fingers lift");
        assert!((new_first - asked_first).abs() < 1e-9, "and the first frame draws what the first events asked");
        assert!(old_total < 0.5, "the old path lost most of the pinch: {old_total}");
    }
}

#[cfg(test)]
mod aerial_fog_tests {
    use super::*;

    fn at_height(h_m: f64) -> Camera {
        let t = eye_target_fov((-73.99, 40.747), h_m, 0.4, 60f32.to_radians(), 887.0, 46f64.to_radians());
        let (lon, lat) = t.center.unwrap();
        Camera { center_lon: lon, center_lat: lat, zoom: t.zoom.unwrap(), pitch: t.pitch.unwrap(), bearing: t.bearing.unwrap(), viewport_width_px: 1401.0, viewport_height_px: 887.0, fov_y: 46f64.to_radians() }
    }

    const AERIAL: AerialFog = AerialFog { above_m: 100.0, reach_per_altitude: 12.0, clear_per_altitude: 6.5, max_reach_m: 200_000.0, max_clear_m: 20_000.0 };

    /// Without an [`AerialFog`] the reach is `fog_m` at every height and
    /// nothing is held clear; with one, nothing changes at or under
    /// `above_m`, the reach and clear distance grow with the climb, and
    /// both stop at their caps.
    #[test]
    fn the_aerial_fog_grows_with_the_eye_and_stops_at_its_caps() {
        let sky = crate::style::Rgba { r: 0.4, g: 0.6, b: 0.8, a: 1.0 };
        let plain = EyeCamera::new(8_000.0, sky, sky);
        let aerial = plain.with_aerial_fog(AERIAL);
        for h in [48.0, 100.0, 1_500.0, 30_000.0] {
            let cam = at_height(h);
            assert!((cam.eye_height_m() - h).abs() < 1e-6 * h.max(1.0));
            assert_eq!(plain.reach_m(&cam), 8_000.0);
            assert_eq!(plain.clear_world(&cam), 0.0);
            assert_eq!(plain.cutoff_world(&cam), 8_000.0 / metres_per_world_unit(cam.center_lat));
        }
        let street = at_height(48.0);
        assert_eq!(aerial.reach_m(&street), 8_000.0);
        assert_eq!(aerial.clear_world(&street), 0.0);
        assert_eq!(aerial.cutoff_world(&street), plain.cutoff_world(&street), "the street's cut is unchanged");
        let high = at_height(1_500.0);
        assert!((aerial.reach_m(&high) - 24_800.0).abs() < 1e-3, "{}", aerial.reach_m(&high));
        assert!((aerial.clear_world(&high) * metres_per_world_unit(high.center_lat) - 9_100.0).abs() < 1e-3);
        let very_high = at_height(30_000.0);
        assert_eq!(aerial.reach_m(&very_high), 200_000.0);
        assert!((aerial.clear_world(&very_high) * metres_per_world_unit(very_high.center_lat) - 20_000.0).abs() < 1e-3);
    }
}
