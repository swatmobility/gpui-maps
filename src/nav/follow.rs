//! The navigation camera: following the puck in one of three framings
//! ([`NavCameraMode::Follow3d`], [`NavCameraMode::Follow2d`],
//! [`NavCameraMode::NorthUp2d`]), the overview (the whole route, flat
//! and north-up), or free (the user moved the map; Recenter, or
//! [`FREE_RESUME_MS`] without a touch, brings it back).
//!
//! Built on `camera_control::CameraController` and nothing else. It writes
//! the camera only through `jump_to`, once per frame, on a pose it computes
//! itself -- so the motion is continuous rather than one ease per fix: an
//! `ease_to` toward where the puck WAS would be overtaken by the next fix
//! before it landed. A change of mode glides for [`TRANSITION_MS`] by
//! blending from the pose the camera had to the live target, so the glide
//! chases a moving puck instead of a stale one. Between glides the zoom and
//! the bearing ease toward the target ([`ZOOM_EASE_MS`],
//! [`BEARING_EASE_MS`]), so a speed change or a bend in the road turns the
//! view smoothly rather than in steps.
//!
//! Free is detected, not reported: `CameraController::input_generation`
//! counts every camera-moving `apply` (not a bare tap), and every `apply` is
//! the user. Except a zoom: while following, a wheel, trackpad or pinch zoom
//! (`CameraController::zoom_input`) keeps following and becomes the follow
//! zoom offset ([`FollowCamera::set_zoom_offset`]), about the puck rather
//! than the pointer, for the rest of the trip. Organic Maps keeps its follow
//! mode through a scale gesture in routing the same way
//! (`MyPositionController::ScaleEnded`); MapLibre GL JS's
//! `GeolocateControl` drops its lock on any `movestart`, a zoom included,
//! which is the "zoom breaks the drive" behaviour this module avoids.
//!
//! # Where the numbers come from
//!
//! Zooms are this engine's (and MapLibre's) 512-px-tile zoom in logical
//! pixels, so MapLibre Native / Ferrostar zoom values compare directly.
//!
//! - Ferrostar (BSD-3), `android/ui-maplibre/.../runtime/NavigationCamera.kt`:
//!   `NavigationActivity.Automotive = (zoom 16, tilt 45)` at every speed; the
//!   follow padding is `top = 50%` of the screen, which centres the puck in
//!   the lower half -- 75% of the way down -- and `withNavigationBottomInset`
//!   lifts it to stay 24 dp clear of the bottom chrome.
//! - MapLibre Navigation iOS (MIT/ISC), `NavigationMapView.swift`: pitch 45,
//!   `defaultAltitude` 1000 m, `zoomedOutMotorwayAltitude` 2000 m (one zoom
//!   level out) on a motorway with >= `longManeuverDistance` 1000 m to go;
//!   edge padding `top = 40%` puts the puck 70% down; 1 s linear animation
//!   per fix.
//! - MapLibre Navigation Android (MIT), `DynamicCamera.java`: zoom 12..16
//!   fitted to (puck, next maneuver), so the view tightens as the maneuver
//!   nears; tilt `distance / 5` clamped 45..60 deg; zoom animations
//!   300..1500 ms (`NavigationConstants.kt`).
//! - Organic Maps (Apache-2.0), `drape_frontend/my_position_controller.cpp`
//!   `CalculateZoomBySpeed`: metres per pixel by speed, 3D 0.25 @ 20 km/h ..
//!   1.5 @ 60 .. 6.0 @ 95+, 2D about 0.6 zoom levels further out at city
//!   speeds; follow resumes after `kMaxNotFollowRoutingTimeSec` = 20 s.
//! - Google Navigation SDK docs (camera page): `TILTED`,
//!   `TOP_DOWN_HEADING_UP`, `TOP_DOWN_NORTH_UP` perspectives; Re-center
//!   restores following; `showRouteOverview` fits the untravelled route (this overview fits the whole route: the Studio button reads "Whole route").
//!
//! The 3D city zoom here is closer than Ferrostar's flat 16 (the user's
//! report: "too far from the car"), and it opens out with speed less
//! steeply than Organic Maps, which reaches z13.7 at motorway speed.

use crate::camera_control::{CameraController, CameraTarget, MAX_PITCH, MAX_ZOOM};
#[cfg(test)]
use crate::camera_control::MIN_ZOOM;
use crate::nav::view_model::{CameraMode, Puck};

/// How the camera frames a trip. The three follow modes share the
/// speed-and-maneuver zoom; they differ in tilt and in which way is up.
/// `Follow3d` is the default.
///
/// Google's Navigation SDK offers the same three follow perspectives
/// (`TILTED`, `TOP_DOWN_HEADING_UP`, `TOP_DOWN_NORTH_UP`). The flat ones are
/// the driver-app (taxi, delivery) pattern: a top-down plan reads street
/// names and the next pickup at a glance, nothing is foreshortened, and
/// north-up keeps the city's layout fixed for a driver who knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum NavCameraMode {
    /// Tilted [`FOLLOW_PITCH_RAD`], turned to the course, puck in the lower
    /// quarter.
    #[default]
    Follow3d,
    /// Top-down, turned to the course, puck in the lower quarter.
    Follow2d,
    /// Top-down, north up, puck in the middle of the clear map.
    NorthUp2d,
    /// The whole route (driven part included), flat and north-up, between the banner and the
    /// bottom strip.
    Overview,
}

impl NavCameraMode {
    /// Every mode, in the order a switch cycles them.
    pub const ALL: [Self; 4] = [Self::Follow3d, Self::Follow2d, Self::NorthUp2d, Self::Overview];

    /// The mode's stable name: `follow3d`, `follow2d`, `northup2d`, `overview`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Follow3d => "follow3d",
            Self::Follow2d => "follow2d",
            Self::NorthUp2d => "northup2d",
            Self::Overview => "overview",
        }
    }

    /// The mode a [`Self::name`] names, ignoring case, `-` and `_`
    /// (`follow-3d`, `north_up_2d`, ...).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let key: String = name.chars().filter(|c| !matches!(c, '-' | '_' | ' ')).collect::<String>().to_ascii_lowercase();
        Self::ALL.into_iter().find(|m| m.name() == key)
    }

    /// One of the three follow modes (not the overview).
    #[must_use]
    pub fn follows(self) -> bool {
        !matches!(self, Self::Overview)
    }
}

/// The follow zoom offset never leaves this range, zoom levels: +1 is half
/// the altitude. The old `Close` / `Far` presets were +1 / -1 (MapLibre
/// Navigation iOS's `defaultAltitude` 1000 m halved, and its
/// `zoomedOutMotorwayAltitude` 2000 m); the range is that span widened by
/// half a level each way.
pub const FOLLOW_ZOOM_OFFSET_MIN: f64 = -1.5;
pub const FOLLOW_ZOOM_OFFSET_MAX: f64 = 1.5;
/// [`NavCameraMode::Follow3d`]'s tilt once the offset is +1 or more,
/// 50 deg: nearer the driver's eye line than [`FOLLOW_PITCH_RAD`]
/// (MapLibre Navigation Android tilts 45..60), and no further -- at this
/// engine's 60 deg field of view a pitch past 50 deg reaches the horizon
/// and the tile cover is no longer bounded (`flat_cover`). Between 0 and
/// +1 the tilt runs linearly from [`FOLLOW_PITCH_RAD`] to this; zoomed
/// out, it stays at [`FOLLOW_PITCH_RAD`].
pub const CLOSE_PITCH_RAD: f32 = 50.0 * std::f32::consts::PI / 180.0;
/// While the user is zooming, the follow zoom tracks the new offset with
/// this time constant, ms, rather than [`ZOOM_EASE_MS`]: a wheel notch or a
/// pinch must answer at once, not over a second.
pub const USER_ZOOM_EASE_MS: f64 = 80.0;
/// ... for this long after the last zoom input, ms; then the slow ease
/// that smooths speed changes is back.
pub const USER_ZOOM_FAST_MS: f64 = 600.0;

/// The startup preset names (`--nav-proximity close|standard|far`) as
/// zoom offsets: `close` +1, `standard` 0, `far` -1. `None` for any other
/// name. Case is ignored.
#[must_use]
pub fn zoom_offset_preset(name: &str) -> Option<f64> {
    match name.trim().to_ascii_lowercase().as_str() {
        "close" => Some(1.0),
        "standard" => Some(0.0),
        "far" => Some(-1.0),
        _ => None,
    }
}

/// [`NavCameraMode::Follow3d`]'s tilt at follow zoom `offset`: see
/// [`CLOSE_PITCH_RAD`].
#[must_use]
pub fn follow_pitch_3d(offset: f64) -> f32 {
    let t = offset.clamp(0.0, 1.0) as f32;
    FOLLOW_PITCH_RAD + (CLOSE_PITCH_RAD - FOLLOW_PITCH_RAD) * t
}

/// [`NavCameraMode::Follow3d`]'s tilt. Ferrostar `Automotive` tilt 45;
/// MapLibre Navigation iOS pitch 45; MapLibre Navigation Android 45..60.
pub const FOLLOW_PITCH_RAD: f32 = std::f32::consts::FRAC_PI_4;
/// The 3D follow zoom at [`DEFAULT_SPEED_KMH`] -- and so whenever the
/// speed is unknown. Ferrostar's `Automotive` is a flat 16 at every speed;
/// this is the [`SPEED_ZOOM_3D`] curve's 50 km/h point.
pub const FOLLOW_ZOOM: f64 = 17.0;
/// The puck sits this fraction of the viewport height below the centre:
/// 0.5 + 0.25 = 75% of the way down. Ferrostar's follow padding (`top =
/// 50%`) centres the puck in the lower half, i.e. 75%; MapLibre Navigation
/// iOS puts it at 70%.
pub const FOLLOW_ANCHOR_FRACTION: f32 = 0.25;
/// The puck stays this far above the bottom chrome, logical pixels, past its
/// own half-size: Ferrostar `withNavigationBottomInset`'s 24 dp clearance.
pub const FOLLOW_BOTTOM_CLEARANCE_PX: f32 = 24.0;
/// The speed assumed when a fix carries none, km/h: urban driving.
pub const DEFAULT_SPEED_KMH: f64 = 50.0;
/// 3D follow zoom by speed, `(km/h, zoom)`, linear between points and flat
/// beyond the ends. Never zooms IN with speed. City speeds sit about one
/// level closer than Ferrostar's flat 16; the motorway end is one level out
/// from the 50 km/h point, as MapLibre Navigation iOS's motorway altitude
/// (2000 m against 1000 m) is; Organic Maps' table spans further (z18.3 at
/// 20 km/h to z13.7 at 95), which reads as too far at speed on a phone.
pub const SPEED_ZOOM_3D: [(f64, f64); 4] = [(20.0, 17.5), (50.0, 17.0), (80.0, 16.25), (110.0, 15.75)];
/// The flat modes sit this many zoom levels further out than
/// [`SPEED_ZOOM_3D`]: with no tilt, the road ahead is not stretched out
/// toward a horizon, so the plan needs a wider scale to show as much of it.
/// Organic Maps' 2D table is 0.6..0.75 levels out from its 3D at 40-60 km/h.
pub const FLAT_ZOOM_OFFSET: f64 = -0.6;
/// The follow zoom never leaves this range, whatever the speed or maneuver
/// boost. MapLibre Navigation Android's `DynamicCamera` clamps to 12..16
/// (its 16 is the closest it ever gets, which the report found too far).
/// [`FOLLOW_ZOOM_OFFSET_MIN`] at motorway speed, flat, is z13.65;
/// [`FOLLOW_ZOOM_OFFSET_MAX`] at a crawl with the turn boost is z19.75.
pub const FOLLOW_MIN_ZOOM: f64 = 13.5;
pub const FOLLOW_MAX_ZOOM: f64 = 19.75;
/// The zoom-in before a turn starts this far from the maneuver, metres.
/// MapLibre Navigation Android's `DynamicCamera` tilt (`distance / 5`)
/// begins to flatten at 300 m, and its zoom fits the maneuver from wherever
/// the step begins.
pub const MANEUVER_ZOOM_START_M: f64 = 300.0;
/// ... and is complete this far from it.
pub const MANEUVER_ZOOM_FULL_M: f64 = 75.0;
/// How many zoom levels closer the camera is at [`MANEUVER_ZOOM_FULL_M`].
/// Google Maps' auto-zoom (driver reports) tightens on each turn; 0.75 of a
/// level is enough to see the junction's lanes without losing the exit.
pub const MANEUVER_ZOOM_BOOST: f64 = 0.75;
/// Time constant of the follow zoom's ease toward its target, ms: a speed
/// change or a maneuver boost settles in about three of these. MapLibre
/// Navigation Android eases zoom over 300..1500 ms.
pub const ZOOM_EASE_MS: f64 = 1_000.0;
/// Time constant of the heading-up bearing's ease toward the course, ms.
/// MapLibre Navigation iOS animates each fix's heading over 1 s; this is
/// the same lag, frame-rate independent.
pub const BEARING_EASE_MS: f64 = 500.0;
/// After a gesture, following resumes by itself after this long without
/// another one, ms. Organic Maps' `kMaxNotFollowRoutingTimeSec` (20 s).
pub const FREE_RESUME_MS: f64 = 20_000.0;
/// A mode change glides this long.
pub const TRANSITION_MS: f64 = 800.0;
/// A glide whose two ends are further apart than this many viewports, at
/// the farther-out of their two zooms, is a cut instead. The glide blends
/// the centre linearly, so a long one sweeps every frame's tile cover over
/// ground that is on screen at neither end: a drive started from a camera
/// on another city (the web demo opens on Tokyo and drives in Singapore)
/// asked for some 200 tiles across the sea in 800 ms, and the drive's own
/// tiles queued behind them. MapLibre's `flyTo` zooms out to make such a
/// move readable; a cut is the honest version without that curve.
pub const GLIDE_MAX_VIEWPORTS: f64 = 4.0;
/// Overview keeps this much clear space around the route, logical pixels.
pub const OVERVIEW_PADDING_PX: f32 = 48.0;
/// Overview never zooms in past this, however short the route.
pub const OVERVIEW_MAX_ZOOM: f64 = 17.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Pose {
    lon: f64,
    lat: f64,
    zoom: f64,
    pitch: f32,
    bearing: f32,
    anchor_y: f32,
}

impl Pose {
    fn of(camera: &CameraController) -> Self {
        let c = camera.camera();
        Self { lon: c.center_lon, lat: c.center_lat, zoom: c.zoom, pitch: c.pitch, bearing: c.bearing, anchor_y: 0.0 }
    }

    fn lerp(self, to: Self, t: f64) -> Self {
        let tf = t as f32;
        Self {
            lon: self.lon + crate::camera_control::shortest_lon_delta(self.lon, to.lon) * t,
            lat: self.lat + (to.lat - self.lat) * t,
            zoom: self.zoom + (to.zoom - self.zoom) * t,
            pitch: self.pitch + (to.pitch - self.pitch) * tf,
            bearing: self.bearing + crate::camera_control::shortest_bearing_delta(self.bearing, to.bearing) * tf,
            anchor_y: self.anchor_y + (to.anchor_y - self.anchor_y) * tf,
        }
    }

    fn target(self) -> CameraTarget {
        CameraTarget {
            center: Some((self.lon, self.lat)),
            zoom: Some(self.zoom),
            pitch: Some(self.pitch),
            bearing: Some(self.bearing),
            anchor_offset_px: (0.0, self.anchor_y),
        }
    }
}

/// Space the chrome takes at the top and bottom, logical pixels, so the
/// overview fits the route between the banner and the strip, and the
/// follow puck stays above the strip.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChromeClearance {
    pub top: f32,
    pub bottom: f32,
}

/// What the follow zoom reads off the trip each frame: how fast, and how
/// far to the next turn.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Motion {
    speed_mps: Option<f64>,
    maneuver_m: Option<f64>,
}

/// The eased zoom and bearing, and when they were last stepped.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Eased {
    zoom: f64,
    bearing: f32,
    at_ms: f64,
}

pub struct FollowCamera {
    mode: CameraMode,
    /// The mode chosen: what following or the overview looks like, and
    /// what a resume after a gesture goes back to.
    chosen: NavCameraMode,
    /// The follow mode Recenter returns to: the last follow mode chosen.
    follow_style: NavCameraMode,
    /// Zoom levels added to the follow zoom: the user's own, from a wheel,
    /// trackpad or pinch zoom while following, or [`Self::set_zoom_offset`].
    zoom_offset: f64,
    seen_input: u64,
    /// `CameraController::zoom_input` when last looked at.
    seen_zoom: (u64, f64),
    /// Until when the follow zoom tracks the offset fast
    /// ([`USER_ZOOM_EASE_MS`]): the user is zooming.
    fast_zoom_until_ms: f64,
    glide: Option<(Pose, f64)>,
    eased: Option<Eased>,
    motion: Motion,
    /// When the last gesture was seen, while free.
    free_since_ms: Option<f64>,
}

impl FollowCamera {
    /// Starts following in [`NavCameraMode::Follow3d`], gliding in from
    /// wherever `camera` is now.
    #[must_use]
    pub fn new(camera: &CameraController, now_ms: f64) -> Self {
        Self::with_mode(NavCameraMode::default(), camera, now_ms)
    }

    /// Starts in `mode`, gliding in from wherever `camera` is now.
    #[must_use]
    pub fn with_mode(mode: NavCameraMode, camera: &CameraController, now_ms: f64) -> Self {
        let follow_style = if mode.follows() { mode } else { NavCameraMode::default() };
        let mut follow = Self {
            mode: CameraMode::Following,
            chosen: mode,
            follow_style,
            zoom_offset: 0.0,
            seen_input: camera.input_generation(),
            seen_zoom: camera.zoom_input(),
            fast_zoom_until_ms: f64::NEG_INFINITY,
            glide: None,
            eased: None,
            motion: Motion::default(),
            free_since_ms: None,
        };
        follow.set_nav_camera_mode(mode, camera, now_ms);
        follow
    }

    /// Following, overview or free.
    #[must_use]
    pub fn mode(&self) -> CameraMode {
        self.mode
    }

    /// The mode chosen -- still the chosen one while a gesture has the
    /// camera free; following resumes into it.
    #[must_use]
    pub fn nav_camera_mode(&self) -> NavCameraMode {
        self.chosen
    }

    /// Switches to `mode`, gliding, and takes the camera back from a
    /// gesture if one had it.
    pub fn set_nav_camera_mode(&mut self, mode: NavCameraMode, camera: &CameraController, now_ms: f64) {
        self.chosen = mode;
        if mode.follows() {
            self.follow_style = mode;
            self.switch(CameraMode::Following, camera, now_ms);
        } else {
            self.switch(CameraMode::Overview, camera, now_ms);
        }
    }

    /// The follow zoom offset, zoom levels (+1 is half the altitude),
    /// clamped to [`FOLLOW_ZOOM_OFFSET_MIN`]..[`FOLLOW_ZOOM_OFFSET_MAX`].
    /// It moves the whole speed-and-maneuver zoom curve -- the camera still
    /// opens out on a motorway and tightens before a turn, around the
    /// user's own distance -- and, in [`NavCameraMode::Follow3d`], the tilt
    /// along [`follow_pitch_3d`]. A wheel, trackpad or pinch zoom while
    /// following sets it too, without letting go of the puck. Following
    /// eases to a new value; free and the overview keep it for when
    /// following resumes.
    pub fn set_zoom_offset(&mut self, offset: f64, now_ms: f64) {
        if offset.is_finite() {
            self.zoom_offset = offset.clamp(FOLLOW_ZOOM_OFFSET_MIN, FOLLOW_ZOOM_OFFSET_MAX);
            self.fast_zoom_until_ms = now_ms + USER_ZOOM_FAST_MS;
        }
    }

    /// See [`Self::set_zoom_offset`].
    #[must_use]
    pub fn zoom_offset(&self) -> f64 {
        self.zoom_offset
    }

    /// How fast the puck is moving and how far the next turn is (`None`:
    /// no turn worth zooming in for). Read by the follow zoom on the next
    /// [`Self::drive`].
    pub fn set_motion(&mut self, speed_mps: Option<f64>, maneuver_m: Option<f64>) {
        self.motion = Motion { speed_mps, maneuver_m };
    }

    /// Recenter: back to the last follow mode chosen, gliding.
    pub fn recenter(&mut self, camera: &CameraController, now_ms: f64) {
        self.set_nav_camera_mode(self.follow_style, camera, now_ms);
    }

    /// Overview: the whole route, flat and north-up, gliding.
    pub fn overview(&mut self, camera: &CameraController, now_ms: f64) {
        self.set_nav_camera_mode(NavCameraMode::Overview, camera, now_ms);
    }

    fn switch(&mut self, mode: CameraMode, camera: &CameraController, now_ms: f64) {
        self.mode = mode;
        self.seen_input = camera.input_generation();
        self.seen_zoom = camera.zoom_input();
        self.glide = Some((Pose::of(camera), now_ms));
        self.eased = None;
        self.free_since_ms = None;
    }

    /// Puts the camera where this mode wants it for this frame. Returns
    /// whether a glide is still running (the caller keeps drawing while it
    /// is). Does nothing -- and switches to free -- once the user has moved
    /// the camera since the last call; [`FREE_RESUME_MS`] after the last
    /// gesture, it glides back into the chosen mode.
    pub fn drive(
        &mut self,
        camera: &mut CameraController,
        puck: Option<Puck>,
        remaining_bounds_world: Option<(f64, f64, f64, f64)>,
        clearance: ChromeClearance,
        now_ms: f64,
    ) -> bool {
        let viewport = (camera.camera().viewport_width_px, camera.camera().viewport_height_px);
        if camera.input_generation() != self.seen_input {
            let inputs = camera.input_generation().wrapping_sub(self.seen_input);
            let zoom = camera.zoom_input();
            let (zooms, levels) = (zoom.0.wrapping_sub(self.seen_zoom.0), zoom.1 - self.seen_zoom.1);
            self.seen_input = camera.input_generation();
            self.seen_zoom = zoom;
            if self.mode == CameraMode::Following && zooms == inputs {
                // Only zooms since the last frame: keep following, at the
                // user's new distance. Organic Maps does the same in
                // routing (`MyPositionController::ScaleEnded` keeps the
                // follow mode; only a scroll or a twist leaves it).
                self.zoom_by(levels, now_ms);
            } else {
                self.mode = CameraMode::Free;
                self.glide = None;
                self.eased = None;
                self.free_since_ms = Some(now_ms);
            }
        }
        if self.mode == CameraMode::Free {
            match self.free_since_ms {
                Some(since) if now_ms - since >= FREE_RESUME_MS => self.set_nav_camera_mode(self.chosen, camera, now_ms),
                Some(_) => return false,
                None => {
                    self.free_since_ms = Some(now_ms);
                    return false;
                }
            }
        }
        let target = match self.mode {
            CameraMode::Free => return false,
            CameraMode::Following => puck.map(|p| self.eased_follow(follow_pose(self.follow_style, self.zoom_offset, p, self.motion, viewport, clearance), now_ms)),
            CameraMode::Overview => remaining_bounds_world.and_then(|b| overview_pose(b, viewport, clearance, camera.zoom_range().0)),
        };
        let Some(target) = target else { return false };
        if self.glide.is_some_and(|(from, _)| too_far_to_glide(from, target, viewport)) {
            self.glide = None;
        }
        let (pose, gliding) = match self.glide {
            Some((from, started_ms)) => {
                let t = ((now_ms - started_ms) / TRANSITION_MS).clamp(0.0, 1.0);
                let eased = t * t * (3.0 - 2.0 * t);
                if t >= 1.0 {
                    self.glide = None;
                }
                (from.lerp(target, eased), t < 1.0)
            }
            None => (target, false),
        };
        camera.jump_to(pose.target());
        gliding
    }

    /// A user zoom of `levels` while following: the offset moves by as much
    /// of it as the follow zoom can still show -- past
    /// [`FOLLOW_MIN_ZOOM`]..[`FOLLOW_MAX_ZOOM`] a zoom would only bank an
    /// offset the next zoom the other way must first spend -- and the
    /// follow zoom tracks it fast for a moment.
    fn zoom_by(&mut self, levels: f64, now_ms: f64) {
        if !levels.is_finite() || levels == 0.0 {
            return;
        }
        let raw = follow_zoom_unclamped(self.follow_style, 0.0, self.motion);
        let lo = FOLLOW_ZOOM_OFFSET_MIN.max(FOLLOW_MIN_ZOOM - raw).min(self.zoom_offset);
        let hi = FOLLOW_ZOOM_OFFSET_MAX.min(FOLLOW_MAX_ZOOM.min(MAX_ZOOM) - raw).max(self.zoom_offset);
        self.zoom_offset = (self.zoom_offset + levels).clamp(lo, hi);
        self.fast_zoom_until_ms = now_ms + USER_ZOOM_FAST_MS;
    }

    /// `target` with its zoom and bearing eased from last frame's. While a
    /// glide runs the target is taken as it is (the glide is the ease), so
    /// the ease starts from where the glide lands.
    fn eased_follow(&mut self, mut target: Pose, now_ms: f64) -> Pose {
        if let Some(prev) = self.eased.filter(|_| self.glide.is_none()) {
            let dt = (now_ms - prev.at_ms).max(0.0);
            let tau = if now_ms < self.fast_zoom_until_ms { USER_ZOOM_EASE_MS } else { ZOOM_EASE_MS };
            let k_zoom = 1.0 - (-dt / tau).exp();
            let k_bearing = (1.0 - (-dt / BEARING_EASE_MS).exp()) as f32;
            target.zoom = prev.zoom + (target.zoom - prev.zoom) * k_zoom;
            target.bearing = prev.bearing + crate::camera_control::shortest_bearing_delta(prev.bearing, target.bearing) * k_bearing;
        }
        self.eased = Some(Eased { zoom: target.zoom, bearing: target.bearing, at_ms: now_ms });
        target
    }
}

/// [`SPEED_ZOOM_3D`] at `speed_mps` ([`DEFAULT_SPEED_KMH`] when unknown).
fn speed_zoom_3d(speed_mps: Option<f64>) -> f64 {
    let kmh = speed_mps.filter(|s| s.is_finite() && *s >= 0.0).map_or(DEFAULT_SPEED_KMH, |s| s * 3.6);
    let table = SPEED_ZOOM_3D;
    if kmh <= table[0].0 {
        return table[0].1;
    }
    for pair in table.windows(2) {
        let ((s0, z0), (s1, z1)) = (pair[0], pair[1]);
        if kmh <= s1 {
            return z0 + (z1 - z0) * (kmh - s0) / (s1 - s0);
        }
    }
    table[table.len() - 1].1
}

/// Levels closer for a turn `maneuver_m` ahead: none beyond
/// [`MANEUVER_ZOOM_START_M`], all of [`MANEUVER_ZOOM_BOOST`] inside
/// [`MANEUVER_ZOOM_FULL_M`], smoothstepped between.
fn maneuver_boost(maneuver_m: Option<f64>) -> f64 {
    let Some(d) = maneuver_m.filter(|d| d.is_finite() && *d >= 0.0) else { return 0.0 };
    let t = ((MANEUVER_ZOOM_START_M - d) / (MANEUVER_ZOOM_START_M - MANEUVER_ZOOM_FULL_M)).clamp(0.0, 1.0);
    MANEUVER_ZOOM_BOOST * t * t * (3.0 - 2.0 * t)
}

/// The follow zoom for `mode` at zoom `offset`, before easing and before
/// the [`FOLLOW_MIN_ZOOM`]..[`FOLLOW_MAX_ZOOM`] clamp. The maneuver boost
/// adds to the user's offset: a turn tightens the view from wherever the
/// user put it, and lets go back to it after.
fn follow_zoom_unclamped(mode: NavCameraMode, offset: f64, motion: Motion) -> f64 {
    let flat = if mode == NavCameraMode::Follow3d { 0.0 } else { FLAT_ZOOM_OFFSET };
    speed_zoom_3d(motion.speed_mps) + flat + offset + maneuver_boost(motion.maneuver_m)
}

/// [`follow_zoom_unclamped`], clamped.
fn follow_zoom(mode: NavCameraMode, offset: f64, motion: Motion) -> f64 {
    follow_zoom_unclamped(mode, offset, motion).clamp(FOLLOW_MIN_ZOOM, FOLLOW_MAX_ZOOM.min(MAX_ZOOM))
}

/// How far below the centre the heading-up puck sits: [`FOLLOW_ANCHOR_FRACTION`]
/// of the height, lifted to stay [`FOLLOW_BOTTOM_CLEARANCE_PX`] clear of the
/// bottom chrome, never above the centre.
fn heading_up_anchor(viewport_h: f32, clearance: ChromeClearance) -> f32 {
    let wanted = viewport_h * FOLLOW_ANCHOR_FRACTION;
    let lowest = viewport_h / 2.0 - clearance.bottom - FOLLOW_BOTTOM_CLEARANCE_PX - crate::nav::chrome::PUCK_SIZE / 2.0;
    wanted.min(lowest).max(0.0)
}

/// Whether `from` and `to` are more than [`GLIDE_MAX_VIEWPORTS`] apart,
/// measured in logical pixels at the lower of their two zooms: the most of
/// the ground between them any frame of the glide could show.
fn too_far_to_glide(from: Pose, to: Pose, viewport: (f32, f32)) -> bool {
    let (ax, ay) = crate::overlay::geom::lon_lat_to_world(from.lon, from.lat);
    let (bx, by) = crate::overlay::geom::lon_lat_to_world(from.lon + crate::camera_control::shortest_lon_delta(from.lon, to.lon), to.lat);
    // One world unit is 2^zoom logical pixels (`render::camera`).
    let px = (bx - ax).hypot(by - ay) * from.zoom.min(to.zoom).exp2();
    let span = f64::from(viewport.0.max(viewport.1).max(1.0));
    px.is_nan() || px > GLIDE_MAX_VIEWPORTS * span
}

fn follow_pose(mode: NavCameraMode, offset: f64, puck: Puck, motion: Motion, viewport: (f32, f32), clearance: ChromeClearance) -> Pose {
    let (pitch, bearing, anchor_y) = match mode {
        NavCameraMode::NorthUp2d => (0.0, 0.0, (clearance.top - clearance.bottom) / 2.0),
        NavCameraMode::Follow2d => (0.0, (puck.course_deg as f32).to_radians(), heading_up_anchor(viewport.1, clearance)),
        _ => (follow_pitch_3d(offset).min(MAX_PITCH), (puck.course_deg as f32).to_radians(), heading_up_anchor(viewport.1, clearance)),
    };
    Pose { lon: puck.lon, lat: puck.lat, zoom: follow_zoom(mode, offset, motion), pitch, bearing, anchor_y }
}

fn overview_pose(bounds: (f64, f64, f64, f64), viewport: (f32, f32), clearance: ChromeClearance, min_zoom: f64) -> Option<Pose> {
    let (min_x, min_y, max_x, max_y) = bounds;
    let avail_w = f64::from(viewport.0 - 2.0 * OVERVIEW_PADDING_PX).max(1.0);
    let avail_h = f64::from(viewport.1 - clearance.top - clearance.bottom - 2.0 * OVERVIEW_PADDING_PX).max(1.0);
    let (span_x, span_y) = ((max_x - min_x).max(1e-9), (max_y - min_y).max(1e-9));
    // One world unit is 2^zoom logical pixels (`render::camera`). The floor is the CALLER's own
    // `zoom_range().0`, not the module's `MIN_ZOOM`: a hardcoded floor here would silently ignore
    // a wider `set_zoom_range` (e.g. the opt-in `SPACE_MIN_ZOOM`) on this one path.
    let zoom = (avail_w / span_x).min(avail_h / span_y).log2().clamp(min_zoom, OVERVIEW_MAX_ZOOM.min(MAX_ZOOM));
    let (lon, lat) = crate::camera_control::lon_lat_at_world(glam::DVec3::new((min_x + max_x) / 2.0, (min_y + max_y) / 2.0, 0.0))?;
    Some(Pose { lon, lat, zoom, pitch: 0.0, bearing: 0.0, anchor_y: (clearance.top - clearance.bottom) / 2.0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_control::CameraIntent;
    use crate::overlay::geom::lon_lat_to_world;
    use crate::render::Camera;

    fn controller() -> CameraController {
        CameraController::new(
            Camera { center_lon: 120.9, center_lat: 14.5, zoom: 12.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 800.0, viewport_height_px: 600.0, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        )
    }

    const PUCK: Puck = Puck { lon: 120.9875, lat: 14.5925, course_deg: 30.0, on_route: true };
    /// The simulated drive's speed: 10 m/s, 36 km/h.
    const SIM_SPEED: Option<f64> = Some(10.0);

    fn screen_of(camera: &CameraController, lon: f64, lat: f64) -> (f32, f32) {
        crate::render::project_world_to_screen_f64(camera.camera(), lon_lat_to_world(lon, lat)).unwrap()
    }

    /// A camera settled in `mode` on `PUCK` at `speed`, `maneuver_m` from a turn.
    fn settled(mode: NavCameraMode, speed: Option<f64>, maneuver_m: Option<f64>, clearance: ChromeClearance) -> CameraController {
        let mut camera = controller();
        let mut follow = FollowCamera::with_mode(mode, &camera, 0.0);
        follow.set_motion(speed, maneuver_m);
        // Past the glide, then long enough for the ease to converge.
        for t in [0.0, 400.0, 800.0, 20_000.0] {
            follow.drive(&mut camera, Some(PUCK), None, clearance, t);
        }
        camera
    }

    #[test]
    fn follow3d_tilts_turns_and_puts_the_puck_three_quarters_down() {
        let mut camera = controller();
        let mut follow = FollowCamera::new(&camera, 0.0);
        assert_eq!(follow.nav_camera_mode(), NavCameraMode::Follow3d, "the default");
        assert!(follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 400.0), "still gliding at 400 ms");
        assert!(!follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 800.0), "landed at 800 ms");
        let c = *camera.camera();
        assert!((c.pitch - FOLLOW_PITCH_RAD).abs() < 1e-6 && (c.zoom - FOLLOW_ZOOM).abs() < 1e-9, "{c:?}");
        assert!((c.bearing - 30f32.to_radians()).abs() < 1e-5, "{}", c.bearing);
        let (x, y) = screen_of(&camera, PUCK.lon, PUCK.lat);
        assert!((x - 400.0).abs() < 1.0 && (y - 450.0).abs() < 1.0, "the puck at 75% height: ({x}, {y})");
        assert_eq!(follow.mode(), CameraMode::Following);
    }

    #[test]
    fn each_mode_has_its_own_pose() {
        let clear = ChromeClearance { top: 100.0, bottom: 60.0 };
        // 600 px: 75% down is 450; the strip's top is 540, so 540 - 24 - 18 = 498 is the floor.
        let three = settled(NavCameraMode::Follow3d, SIM_SPEED, None, clear);
        let two = settled(NavCameraMode::Follow2d, SIM_SPEED, None, clear);
        let north = settled(NavCameraMode::NorthUp2d, SIM_SPEED, None, clear);
        let (c3, c2, cn) = (*three.camera(), *two.camera(), *north.camera());
        // 36 km/h: between the 20 and 50 km/h points.
        assert!(c3.zoom > 17.0 && c3.zoom < 17.5, "3D at 36 km/h: {}", c3.zoom);
        assert!((c3.pitch - FOLLOW_PITCH_RAD).abs() < 1e-6 && (c3.bearing - 30f32.to_radians()).abs() < 1e-4);
        assert!((c2.zoom - (c3.zoom + FLAT_ZOOM_OFFSET)).abs() < 1e-6, "2D is further out: {} vs {}", c2.zoom, c3.zoom);
        assert!(c2.pitch.abs() < 1e-6 && (c2.bearing - 30f32.to_radians()).abs() < 1e-4, "2D: flat, heading-up");
        assert!((cn.zoom - c2.zoom).abs() < 1e-6 && cn.pitch.abs() < 1e-6 && cn.bearing.abs() < 1e-6, "north-up: flat, north");
        for cam in [&three, &two] {
            let (x, y) = screen_of(cam, PUCK.lon, PUCK.lat);
            assert!((x - 400.0).abs() < 1.0 && (y - 450.0).abs() < 1.0, "heading-up puck 75% down: ({x}, {y})");
        }
        let (x, y) = screen_of(&north, PUCK.lon, PUCK.lat);
        // The middle of the clear band: (100 + 540) / 2.
        assert!((x - 400.0).abs() < 1.0 && (y - 320.0).abs() < 1.0, "north-up puck in the clear middle: ({x}, {y})");
    }

    /// The user: "tilted should have one more option, closer to the
    /// ground; flat is a very generic zoom". The zoom offset moves the whole
    /// speed curve in every follow mode; in 3D the tilt follows it up to
    /// 50 deg and never further, and the closest camera's tile cover stays
    /// inside the budget.
    #[test]
    fn the_zoom_offset_moves_every_follow_mode_and_the_closest_stays_bounded() {
        let settle = |mode: NavCameraMode, offset: f64, viewport: (f32, f32)| {
            let mut camera = CameraController::new(
                Camera { center_lon: 120.9, center_lat: 14.5, zoom: 12.0, pitch: 0.0, bearing: 0.0, viewport_width_px: viewport.0, viewport_height_px: viewport.1, fov_y: Camera::DEFAULT_FOV_Y },
                MIN_ZOOM,
                MAX_ZOOM,
            );
            let mut follow = FollowCamera::with_mode(mode, &camera, 0.0);
            follow.set_zoom_offset(offset, 0.0);
            assert_eq!(follow.zoom_offset(), offset.clamp(FOLLOW_ZOOM_OFFSET_MIN, FOLLOW_ZOOM_OFFSET_MAX));
            follow.set_motion(SIM_SPEED, None);
            for t in [0.0, 400.0, 800.0, 20_000.0] {
                follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), t);
            }
            *camera.camera()
        };
        for mode in [NavCameraMode::Follow3d, NavCameraMode::Follow2d, NavCameraMode::NorthUp2d] {
            let [close, standard, far] = [1.0, 0.0, -1.0].map(|o| settle(mode, o, (800.0, 600.0)));
            assert!((close.zoom - standard.zoom - 1.0).abs() < 1e-6, "{mode:?}: +1 {} vs 0 {}", close.zoom, standard.zoom);
            assert!((far.zoom - standard.zoom + 1.0).abs() < 1e-6, "{mode:?}: -1 {} vs 0 {}", far.zoom, standard.zoom);
            if mode == NavCameraMode::Follow3d {
                assert!((close.pitch - CLOSE_PITCH_RAD).abs() < 1e-6 && (standard.pitch - FOLLOW_PITCH_RAD).abs() < 1e-6 && (far.pitch - FOLLOW_PITCH_RAD).abs() < 1e-6);
                let half = settle(mode, 0.5, (800.0, 600.0));
                assert!(half.pitch > FOLLOW_PITCH_RAD && half.pitch < CLOSE_PITCH_RAD, "the tilt follows the offset smoothly: {}", half.pitch);
            } else {
                assert!(close.pitch.abs() < 1e-6 && far.pitch.abs() < 1e-6, "{mode:?} stays flat");
            }
        }
        assert!(CLOSE_PITCH_RAD <= 50f32.to_radians() + 1e-6, "past 50 deg the cover reaches the horizon");
        for viewport in [(800.0, 600.0), (1300.0, 760.0), (430.0, 932.0)] {
            let close = settle(NavCameraMode::Follow3d, FOLLOW_ZOOM_OFFSET_MAX, viewport);
            let tiles = crate::render::cover_plan::flat_cover(&close, close.zoom.floor() as u8).tiles.len();
            assert!(tiles <= crate::render::cover_plan::BUILDING_COVER_BUDGET, "{viewport:?}: the closest camera covers {tiles} tiles");
        }
        assert_eq!(settle(NavCameraMode::Follow3d, 9.0, (800.0, 600.0)).zoom, settle(NavCameraMode::Follow3d, FOLLOW_ZOOM_OFFSET_MAX, (800.0, 600.0)).zoom, "clamped");
        for (name, want) in [("close", 1.0), ("Standard", 0.0), ("far", -1.0)] {
            assert_eq!(zoom_offset_preset(name), Some(want));
        }
        assert_eq!(zoom_offset_preset("closer"), None);
    }

    /// The user: "instead of clicks closer-standard-far we can have just
    /// continuous current zoom and what user changes it remains". A wheel or
    /// pinch zoom while following keeps following, moves the camera by the
    /// levels asked for, keeps the puck where follow puts it (zoomed about
    /// the puck, not the pointer), and becomes the offset.
    #[test]
    fn a_zoom_while_following_keeps_following_about_the_puck() {
        for intent in [
            CameraIntent::ZoomAbout { levels: 0.8, anchor_px: (100.0, 100.0), immediate: true },
            CameraIntent::PinchZoomBy { levels: 0.8, anchor_px: (700.0, 120.0) },
        ] {
            let mut camera = controller();
            let mut follow = FollowCamera::new(&camera, 0.0);
            follow.set_motion(SIM_SPEED, None);
            for t in [0.0, 400.0, 800.0, 20_000.0] {
                follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), t);
            }
            let before = camera.camera().zoom;
            camera.apply(intent);
            let mut t = 20_000.0;
            for _ in 0..40 {
                t += 16.0;
                follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), t);
                camera.advance(std::time::Duration::from_millis(16));
            }
            assert_eq!(follow.mode(), CameraMode::Following, "{intent:?} kept following");
            assert!((follow.zoom_offset() - 0.8).abs() < 1e-9, "{intent:?}: offset {}", follow.zoom_offset());
            let after = camera.camera().zoom;
            assert!((after - before - 0.8).abs() < 0.01, "{intent:?}: zoom {before} -> {after} within 640 ms");
            let (x, y) = screen_of(&camera, PUCK.lon, PUCK.lat);
            assert!((x - 400.0).abs() < 1.0 && (y - 450.0).abs() < 1.0, "{intent:?}: the puck stayed put: ({x}, {y})");
            // Zooming out past the range stops at its end, banking nothing.
            camera.apply(CameraIntent::ZoomAbout { levels: -10.0, anchor_px: (400.0, 300.0), immediate: true });
            follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), t + 16.0);
            assert_eq!(follow.zoom_offset(), FOLLOW_ZOOM_OFFSET_MIN);
            camera.apply(CameraIntent::ZoomAbout { levels: 0.5, anchor_px: (400.0, 300.0), immediate: true });
            follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), t + 32.0);
            assert!((follow.zoom_offset() - (FOLLOW_ZOOM_OFFSET_MIN + 0.5)).abs() < 1e-9, "the way back in starts at once");
        }
    }

    /// A zoom mixed with a pan in the same frame is a pan: free. A zoom in
    /// the overview frees too (the overview has no offset to move), and the
    /// offset outlives both.
    #[test]
    fn a_pan_with_a_zoom_or_a_zoom_in_the_overview_still_frees() {
        let mut camera = controller();
        let mut follow = FollowCamera::new(&camera, 0.0);
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 1_000.0);
        follow.set_zoom_offset(0.5, 1_000.0);
        camera.apply(CameraIntent::PinchZoomBy { levels: 0.3, anchor_px: (400.0, 300.0) });
        camera.apply(CameraIntent::PanBy { dx_px: 20.0, dy_px: 0.0, anchor_px: (400.0, 300.0) });
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 1_016.0);
        assert_eq!(follow.mode(), CameraMode::Free);
        assert_eq!(follow.zoom_offset(), 0.5, "a pan's pinch is not the offset");
        follow.overview(&camera, 2_000.0);
        camera.apply(CameraIntent::ZoomAbout { levels: 1.0, anchor_px: (400.0, 300.0), immediate: true });
        follow.drive(&mut camera, Some(PUCK), Some((200.0, 100.0, 200.1, 100.1)), ChromeClearance::default(), 2_016.0);
        assert_eq!(follow.mode(), CameraMode::Free);
        follow.recenter(&camera, 3_000.0);
        assert_eq!(follow.zoom_offset(), 0.5);
    }

    #[test]
    fn the_puck_stays_above_a_tall_bottom_strip() {
        let clear = ChromeClearance { top: 100.0, bottom: 140.0 };
        let camera = settled(NavCameraMode::Follow3d, SIM_SPEED, None, clear);
        let (_, y) = screen_of(&camera, PUCK.lon, PUCK.lat);
        let floor = 600.0 - 140.0 - FOLLOW_BOTTOM_CLEARANCE_PX - crate::nav::chrome::PUCK_SIZE / 2.0;
        assert!((y - floor).abs() < 1.0, "lifted to {floor}: {y}");
    }

    #[test]
    fn faster_never_zooms_in_and_the_ends_are_flat() {
        let mut last = f64::INFINITY;
        for kmh in 0..=160 {
            let z = speed_zoom_3d(Some(f64::from(kmh) / 3.6));
            assert!(z <= last + 1e-12, "{kmh} km/h zoomed in: {z} after {last}");
            last = z;
        }
        assert_eq!(speed_zoom_3d(Some(0.0)), SPEED_ZOOM_3D[0].1);
        assert_eq!(speed_zoom_3d(Some(200.0)), SPEED_ZOOM_3D[3].1);
        assert_eq!(speed_zoom_3d(None), FOLLOW_ZOOM, "unknown speed is the 50 km/h point");
        assert_eq!(speed_zoom_3d(Some(f64::NAN)), FOLLOW_ZOOM);
        // And the camera, settled at each speed, agrees.
        let zooms: Vec<f64> =
            [5.0, 10.0, 15.0, 20.0, 25.0, 30.0].iter().map(|s| settled(NavCameraMode::Follow3d, Some(*s), None, ChromeClearance::default()).camera().zoom).collect();
        assert!(zooms.windows(2).all(|w| w[1] <= w[0]), "{zooms:?}");
        assert!(zooms[0] - zooms[5] > 1.0, "a motorway is more than a level out from a crawl: {zooms:?}");
    }

    #[test]
    fn the_camera_zooms_in_before_a_turn() {
        let at = |m: Option<f64>| settled(NavCameraMode::Follow3d, SIM_SPEED, m, ChromeClearance::default()).camera().zoom;
        let (none, far, mid, near) = (at(None), at(Some(1_000.0)), at(Some(180.0)), at(Some(50.0)));
        assert!((far - none).abs() < 1e-9, "a turn 1 km off changes nothing");
        assert!(mid > far && near > mid, "closer in as the turn nears: {far} {mid} {near}");
        assert!((near - none - MANEUVER_ZOOM_BOOST).abs() < 1e-6, "the full boost inside {MANEUVER_ZOOM_FULL_M} m");
        assert!(near <= FOLLOW_MAX_ZOOM);
    }

    #[test]
    fn a_speed_change_eases_rather_than_jumps() {
        let mut camera = controller();
        let mut follow = FollowCamera::new(&camera, 0.0);
        follow.set_motion(Some(5.0), None);
        for t in [0.0, 800.0, 10_000.0] {
            follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), t);
        }
        let slow = camera.camera().zoom;
        follow.set_motion(Some(30.0), None);
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 10_016.0);
        let one_frame = camera.camera().zoom;
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 20_000.0);
        let fast = camera.camera().zoom;
        assert!(fast < slow - 1.0);
        assert!(one_frame < slow && slow - one_frame < 0.05 * (slow - fast), "one frame moves a little: {slow} -> {one_frame} -> {fast}");
    }

    #[test]
    fn switching_modes_glides_and_reports_the_mode() {
        let mut camera = controller();
        let mut follow = FollowCamera::new(&camera, 0.0);
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 1_000.0);
        follow.set_nav_camera_mode(NavCameraMode::NorthUp2d, &camera, 1_000.0);
        assert_eq!(follow.nav_camera_mode(), NavCameraMode::NorthUp2d);
        assert!(follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 1_400.0), "gliding");
        let mid = *camera.camera();
        assert!(mid.pitch > 0.0 && mid.pitch < FOLLOW_PITCH_RAD, "halfway flat: {}", mid.pitch);
        assert!(!follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 1_800.0));
        assert_eq!((camera.camera().pitch, camera.camera().bearing), (0.0, 0.0));
        assert_eq!(follow.mode(), CameraMode::Following);
        follow.set_nav_camera_mode(NavCameraMode::Overview, &camera, 2_000.0);
        assert_eq!(follow.mode(), CameraMode::Overview);
        // Recenter goes back to the last FOLLOW mode, not the default.
        follow.recenter(&camera, 3_000.0);
        assert_eq!(follow.nav_camera_mode(), NavCameraMode::NorthUp2d);
    }

    #[test]
    fn a_drive_started_over_another_city_cuts_rather_than_glides() {
        // The web demo's camera opens on Tokyo; its drive is in Singapore.
        let mut camera = CameraController::new(
            Camera { center_lon: 139.7671, center_lat: 35.6812, zoom: 14.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 1200.0, viewport_height_px: 607.0, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        let puck = Puck { lon: 103.8565, lat: 1.3005, course_deg: 200.0, on_route: true };
        let mut follow = FollowCamera::new(&camera, 0.0);
        assert!(!follow.drive(&mut camera, Some(puck), None, ChromeClearance::default(), 16.0), "no glide across the sea");
        let c = *camera.camera();
        assert!((c.center_lat - 1.3).abs() < 0.05 && (c.center_lon - 103.86).abs() < 0.05, "on the puck at once: {c:?}");
        // A near one still glides (the other tests' camera is 500 px from `PUCK`).
        let mut near = controller();
        let mut follow = FollowCamera::new(&near, 0.0);
        assert!(follow.drive(&mut near, Some(PUCK), None, ChromeClearance::default(), 400.0), "gliding");
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in NavCameraMode::ALL {
            assert_eq!(NavCameraMode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(NavCameraMode::from_name("North-Up_2D"), Some(NavCameraMode::NorthUp2d));
        assert_eq!(NavCameraMode::from_name("follow-3d"), Some(NavCameraMode::Follow3d));
        assert_eq!(NavCameraMode::from_name("sideways"), None);
    }

    #[test]
    fn a_user_gesture_frees_the_camera_and_recenter_takes_it_back() {
        let mut camera = controller();
        let mut follow = FollowCamera::new(&camera, 0.0);
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 1_000.0);
        camera.apply(CameraIntent::PanBy { dx_px: 50.0, dy_px: 0.0, anchor_px: (400.0, 300.0) });
        let panned = *camera.camera();
        assert!(!follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 1_016.0));
        assert_eq!(follow.mode(), CameraMode::Free);
        assert_eq!(follow.nav_camera_mode(), NavCameraMode::Follow3d, "the chosen mode outlives the gesture");
        assert_eq!(*camera.camera(), panned, "free means the camera is left alone");
        follow.recenter(&camera, 2_000.0);
        assert!(follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 2_100.0));
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 2_800.0);
        assert_eq!(follow.mode(), CameraMode::Following);
        let (x, y) = screen_of(&camera, PUCK.lon, PUCK.lat);
        assert!((x - 400.0).abs() < 1.0 && (y - 450.0).abs() < 1.0);
    }

    #[test]
    fn following_resumes_by_itself_after_the_user_leaves_the_map_alone() {
        let mut camera = controller();
        let mut follow = FollowCamera::with_mode(NavCameraMode::Follow2d, &camera, 0.0);
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 1_000.0);
        camera.apply(CameraIntent::PanBy { dx_px: 50.0, dy_px: 0.0, anchor_px: (400.0, 300.0) });
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 2_000.0);
        // Another gesture restarts the wait.
        camera.apply(CameraIntent::PanBy { dx_px: 10.0, dy_px: 0.0, anchor_px: (400.0, 300.0) });
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 15_000.0);
        follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 15_000.0 + FREE_RESUME_MS - 1.0);
        assert_eq!(follow.mode(), CameraMode::Free, "still within the wait of the second gesture");
        assert!(follow.drive(&mut camera, Some(PUCK), None, ChromeClearance::default(), 15_000.0 + FREE_RESUME_MS), "gliding back");
        assert_eq!((follow.mode(), follow.nav_camera_mode()), (CameraMode::Following, NavCameraMode::Follow2d));
    }

    #[test]
    fn overview_fits_the_rest_of_the_route_flat_and_north_up_between_the_chrome() {
        let mut camera = controller();
        let (a, b) = (lon_lat_to_world(120.980214, 14.591513), lon_lat_to_world(120.993246, 14.604056));
        let bounds = (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1));
        let clearance = ChromeClearance { top: 130.0, bottom: 100.0 };
        let mut follow = FollowCamera::new(&camera, 0.0);
        follow.set_nav_camera_mode(NavCameraMode::Overview, &camera, 0.0);
        follow.drive(&mut camera, Some(PUCK), Some(bounds), clearance, 800.0);
        let c = *camera.camera();
        assert_eq!((c.pitch, c.bearing), (0.0, 0.0));
        for (lon, lat) in [(120.980214, 14.591513), (120.993246, 14.604056)] {
            let (x, y) = screen_of(&camera, lon, lat);
            assert!((OVERVIEW_PADDING_PX - 1.0..=800.0 - OVERVIEW_PADDING_PX + 1.0).contains(&x), "x {x}");
            assert!((130.0 + OVERVIEW_PADDING_PX - 1.0..=600.0 - 100.0 - OVERVIEW_PADDING_PX + 1.0).contains(&y), "y {y}");
        }
    }

    /// Task 3's nav fix, exercised: the overview's zoom floor is the
    /// controller's own range, so a view that opted into
    /// `SPACE_MIN_ZOOM` fits a world-wide route below `MIN_ZOOM`, and one
    /// that did not stops at `MIN_ZOOM`.
    #[test]
    fn overview_floors_at_the_controllers_own_range_including_the_space_floor() {
        use crate::camera_control::SPACE_MIN_ZOOM;
        // 150 px: 54 px across after padding, for a 512-unit world: log2(54/512) = -3.2.
        let small = |min: f64| {
            let mut c = CameraController::new(
                Camera { center_lon: 0.0, center_lat: 0.0, zoom: 3.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 150.0, viewport_height_px: 150.0, fov_y: Camera::DEFAULT_FOV_Y },
                MIN_ZOOM,
                MAX_ZOOM,
            );
            c.set_zoom_range(min, MAX_ZOOM);
            c
        };
        let (a, b) = (lon_lat_to_world(-179.0, -80.0), lon_lat_to_world(179.0, 80.0));
        let bounds = (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1));
        let want = (54.0 / (bounds.2 - bounds.0).max(bounds.3 - bounds.1)).log2();
        assert!(want < MIN_ZOOM && want > SPACE_MIN_ZOOM, "the route must need the wider floor: {want}");
        for (min, expect) in [(MIN_ZOOM, MIN_ZOOM), (SPACE_MIN_ZOOM, want)] {
            let mut camera = small(min);
            let mut follow = FollowCamera::new(&camera, 0.0);
            follow.overview(&camera, 0.0);
            follow.drive(&mut camera, None, Some(bounds), ChromeClearance::default(), 10_000.0);
            let z = camera.camera().zoom;
            assert!((z - expect).abs() < 1e-6, "floor {min}: zoom {z}, want {expect}");
        }
    }

    #[test]
    fn without_a_puck_following_leaves_the_camera_alone() {
        let mut camera = controller();
        let before = *camera.camera();
        let mut follow = FollowCamera::new(&camera, 0.0);
        assert!(!follow.drive(&mut camera, None, None, ChromeClearance::default(), 100.0));
        assert_eq!(*camera.camera(), before);
    }

    #[test]
    fn a_zero_sized_viewport_never_puts_nan_into_the_camera() {
        let mut camera = CameraController::new(
            Camera { center_lon: 120.9, center_lat: 14.5, zoom: 12.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 0.0, viewport_height_px: 0.0, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        );
        let bounds = Some((200.0, 100.0, 200.1, 100.1));
        for mode in NavCameraMode::ALL {
            let mut follow = FollowCamera::with_mode(mode, &camera, 0.0);
            follow.set_motion(Some(f64::NAN), Some(f64::INFINITY));
            follow.drive(&mut camera, Some(PUCK), bounds, ChromeClearance::default(), 900.0);
            follow.drive(&mut camera, Some(PUCK), bounds, ChromeClearance::default(), 1_900.0);
            let c = camera.camera();
            assert!(
                c.center_lon.is_finite() && c.center_lat.is_finite() && c.zoom.is_finite() && c.pitch.is_finite() && c.bearing.is_finite(),
                "{mode:?}: {c:?}"
            );
        }
    }
}
