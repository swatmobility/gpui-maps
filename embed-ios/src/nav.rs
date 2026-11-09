//! Turn-by-turn navigation on `MapEngine`: the engine's `NavHost` (the
//! drive, the reroute policy, the follow camera, the voice), driven by the
//! app. The engine draws the route line, the puck and the maneuver arrow;
//! with `nav_set_hud_by_host(true)` the app draws the banner, the ETA strip
//! and the drive's controls natively from [`MapEngine::nav_state`], and
//! registers them with [`MapEngine::nav_set_chrome`] so the follow puck
//! rides clear of them. Nothing here decides anything: every call is one
//! `NavHost` call, as `tests/navigation_is_computed_in_nav.rs` holds every
//! shell to.

use gpui_maps_ios::app::host::Host;
use gpui_maps_ios::nav::host::{self as nav, CameraMode, ManeuverGlyph, ManeuverView, NavCameraMode, Rect, SimOptions, TripPhase};

use crate::{MapEngine, MapError};

/// How the camera frames a trip (`NavCameraMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NavCamera {
    /// Tilted, turned to the course, the puck low on the screen.
    Follow3d,
    /// Top-down, turned to the course.
    Follow2d,
    /// Top-down, north up.
    NorthUp,
    /// The whole route.
    Overview,
}

impl NavCamera {
    fn engine(self) -> NavCameraMode {
        match self {
            Self::Follow3d => NavCameraMode::Follow3d,
            Self::Follow2d => NavCameraMode::Follow2d,
            Self::NorthUp => NavCameraMode::NorthUp2d,
            Self::Overview => NavCameraMode::Overview,
        }
    }

    fn of(mode: NavCameraMode) -> Self {
        match mode {
            NavCameraMode::Follow2d => Self::Follow2d,
            NavCameraMode::NorthUp2d => Self::NorthUp,
            NavCameraMode::Overview => Self::Overview,
            // `NavCameraMode` is non-exhaustive: a future mode reads as the
            // default follow.
            _ => Self::Follow3d,
        }
    }
}

/// Where the trip is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NavPhase {
    Navigating,
    OffRoute,
    Rerouting,
    Arrived,
}

/// One instruction: the arrow (`glyph`, a stable name: `straight`, `left`,
/// `slight-right`, `u-turn-right`, `roundabout-cw`, `depart`, `arrive` ...)
/// and its text.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct NavManeuver {
    pub glyph: String,
    pub primary: String,
    pub secondary: Option<String>,
    pub exit_number: Option<u8>,
}

/// A trip as the app's HUD draws it, this frame. Raw figures, not text:
/// the app formats distances, durations and the arrival clock in the
/// phone's own locale.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct NavSnapshot {
    pub phase: NavPhase,
    pub maneuver: Option<NavManeuver>,
    /// The step after, when it comes soon after this one.
    pub then: Option<NavManeuver>,
    pub distance_to_maneuver_m: f64,
    pub road_name: Option<String>,
    pub remaining_distance_m: f64,
    pub remaining_duration_s: f64,
    /// When the trip ends, Unix ms.
    pub eta_unix_ms: f64,
    pub step_index: u32,
    pub step_count: u32,
    /// The camera follows the puck (a pan frees it; Recenter brings it back).
    pub following: bool,
    /// The mode chosen: what following is, or resumes into.
    pub camera: NavCamera,
    /// The follow zoom offset, zoom levels: a pinch while following sets it.
    pub follow_zoom_offset: f64,
    pub voice_muted: bool,
    /// A simulated drive (it has pause and speed); `false` for a live trip.
    pub simulated: bool,
    pub paused: bool,
    /// Stopped by End mid-drive: paused, End's place is Play.
    pub stopped: bool,
    /// 1, 2, 4 or 8.
    pub speed: u8,
    /// The puck on screen, points; `None` off screen.
    pub puck_x: Option<f64>,
    pub puck_y: Option<f64>,
}

/// A rect, points from the map view's top-left.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct NavRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// How a simulated drive plays.
#[derive(Debug, Clone, Copy, PartialEq, Default, uniffi::Record)]
pub struct NavDriveOptions {
    /// 1, 2, 4 or 8; anything else is 1.
    #[uniffi(default = 1)]
    pub speed: u8,
    /// Start this far along the route, metres.
    #[uniffi(default = None)]
    pub start_at_m: Option<f64>,
}

/// The demo drives bundled with the engine, by name.
#[uniffi::export]
pub fn nav_demo_routes() -> Vec<String> {
    Host::nav_routes().into_iter().map(str::to_string).collect()
}

fn glyph_name(glyph: ManeuverGlyph) -> &'static str {
    match glyph {
        ManeuverGlyph::Straight => "straight",
        ManeuverGlyph::SlightLeft => "slight-left",
        ManeuverGlyph::Left => "left",
        ManeuverGlyph::SharpLeft => "sharp-left",
        ManeuverGlyph::SlightRight => "slight-right",
        ManeuverGlyph::Right => "right",
        ManeuverGlyph::SharpRight => "sharp-right",
        ManeuverGlyph::UTurnLeft => "u-turn-left",
        ManeuverGlyph::UTurnRight => "u-turn-right",
        ManeuverGlyph::RoundaboutCounterClockwise => "roundabout-ccw",
        ManeuverGlyph::RoundaboutClockwise => "roundabout-cw",
        ManeuverGlyph::Depart => "depart",
        ManeuverGlyph::Arrive => "arrive",
    }
}

fn maneuver(m: &ManeuverView) -> NavManeuver {
    NavManeuver { glyph: glyph_name(m.glyph).to_string(), primary: m.primary.clone(), secondary: m.secondary.clone(), exit_number: m.exit_number }
}

fn snapshot(host: &Host) -> Option<NavSnapshot> {
    let state: nav::NavigationState = host.nav_state()?;
    let puck = host.nav_puck_on_screen();
    Some(NavSnapshot {
        phase: match state.phase {
            TripPhase::Navigating => NavPhase::Navigating,
            TripPhase::OffRoute => NavPhase::OffRoute,
            TripPhase::Rerouting => NavPhase::Rerouting,
            TripPhase::Arrived => NavPhase::Arrived,
        },
        maneuver: state.maneuver.as_ref().map(maneuver),
        then: state.then.as_ref().map(maneuver),
        distance_to_maneuver_m: state.distance_to_maneuver_m,
        road_name: state.road_name.clone(),
        remaining_distance_m: state.remaining_distance_m,
        remaining_duration_s: state.remaining_duration_s,
        eta_unix_ms: state.eta_unix_ms,
        step_index: state.step_index as u32,
        step_count: state.step_count as u32,
        following: state.camera_mode == CameraMode::Following,
        camera: NavCamera::of(host.nav_camera_mode()),
        follow_zoom_offset: host.nav_follow_zoom_offset(),
        voice_muted: host.nav_voice_muted(),
        simulated: state.sim.is_some(),
        paused: state.sim.is_some_and(|s| s.paused),
        stopped: state.sim.is_some_and(|s| s.stopped),
        speed: state.sim.map_or(1, |s| s.speed),
        puck_x: puck.map(|p| f64::from(p.0)),
        puck_y: puck.map(|p| f64::from(p.1)),
    })
}

#[uniffi::export]
impl MapEngine {
    /// Loads demo drive `name` (see [`nav_demo_routes`]) for the next
    /// [`Self::nav_start`]. `false` for an unknown name.
    pub fn nav_load_route(&self, name: String, options: NavDriveOptions) -> bool {
        let speed = if [1, 2, 4, 8].contains(&options.speed) { options.speed } else { 1 };
        let options = SimOptions { speed, start_at_m: options.start_at_m.filter(|m| m.is_finite() && *m > 0.0), ..SimOptions::default() };
        self.lock().value.host.nav_set_route(&name, options)
    }

    /// The app draws the banner, the ETA strip and the controls itself;
    /// the engine draws the route, the puck and the arrow. Off by default
    /// (the engine draws its own HUD).
    pub fn nav_set_hud_by_host(&self, on: bool) {
        self.lock().value.host.nav_set_hud_by_host(on);
    }

    /// Starts the loaded route as a simulated drive.
    pub fn nav_start(&self) -> Result<(), MapError> {
        self.lock().value.host.nav_start().map_err(MapError::Navigation)
    }

    /// Ends the trip.
    pub fn nav_end(&self) {
        self.lock().value.host.nav_end();
    }

    /// Pause or resume the simulated drive (Play again once stopped).
    pub fn nav_toggle_pause(&self) {
        self.lock().value.host.nav_toggle_pause();
    }

    /// 1x -> 2x -> 4x -> 8x -> 1x.
    pub fn nav_cycle_speed(&self) {
        self.lock().value.host.nav_cycle_speed();
    }

    pub fn nav_set_voice_muted(&self, muted: bool) {
        self.lock().value.host.nav_set_voice_muted(muted);
    }

    /// How the camera frames the trip; the same mode again is Recenter.
    pub fn nav_set_camera(&self, mode: NavCamera) {
        self.lock().value.host.nav_set_camera_mode(mode.engine());
    }

    /// The app's own controls over the map, points: the follow puck and
    /// the overview keep clear of them. Replaces the last set.
    pub fn nav_set_chrome(&self, rects: Vec<NavRect>) {
        let rects = rects
            .into_iter()
            .filter(|r| [r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite()) && r.w > 0.0 && r.h > 0.0)
            .map(|r| Rect { x: r.x as f32, y: r.y as f32, w: r.w as f32, h: r.h as f32 })
            .collect();
        self.lock().value.host.nav_set_host_chrome(rects);
    }

    /// The trip this frame; `None` with no trip.
    pub fn nav_state(&self) -> Option<NavSnapshot> {
        snapshot(&self.lock().value.host)
    }

    /// A notice to show ("Could not start the drive: ..."), if any.
    pub fn nav_notice(&self) -> Option<String> {
        self.lock().value.host.nav_notice()
    }

    /// The map is off screen: a simulated drive pauses, the voice stops.
    pub fn nav_set_suspended(&self, suspended: bool) {
        self.lock().value.host.nav_set_suspended(suspended);
    }
}

/// For host tests: what the engine did, not what it was told.
impl MapEngine {
    /// The route line is on the GPU.
    pub fn nav_route_drawn(&self) -> bool {
        self.lock().value.host.nav_route_drawn()
    }

    /// Voice prompts handed out so far.
    pub fn nav_prompts(&self) -> u64 {
        self.lock().value.host.nav_prompts()
    }

    /// `false`: count prompts, never say them.
    pub fn nav_set_speaker_on(&self, on: bool) {
        self.lock().value.host.nav_set_speaker_on(on);
    }

    /// Marches the engine clock forward (a simulated drive, without
    /// waiting it out).
    #[doc(hidden)]
    pub fn advance_clock_ms(&self, ms: f64) {
        self.lock().value.host.advance_clock_ms(ms);
    }
}
