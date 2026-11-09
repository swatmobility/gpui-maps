//! `NavigationState`: everything the chrome and the map layer show, as plain
//! data, one per frame. It is the ONLY thing they read -- the seam where a
//! native UIKit/HTML layer, or Ferrostar's own SwiftUI views, can replace the
//! engine-drawn chrome later without touching navigation.

use crate::nav::format::WallClock;
use crate::nav::instructions::{maneuver_modifier, maneuver_type};
use crate::nav::route_source::{NavRoute, OsrmStep};
use crate::nav::session::TripSession;
use ferrostar::models::{ManeuverModifier, ManeuverType};
use ferrostar::navigation_controller::models::TripState;

/// "Then ..." is shown only when the maneuver after the next one is this close
/// behind it.
pub const THEN_HINT_MAX_M: f64 = 150.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripPhase {
    Navigating,
    /// Off the route, no request out yet (or waiting after a failed one).
    OffRoute,
    /// Off the route, a new route requested.
    Rerouting,
    Arrived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraMode {
    Following,
    Overview,
    Free,
}

/// Which arrow the banner draws. The OSRM type/modifier set collapsed onto
/// the shapes a driver can tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManeuverGlyph {
    Straight,
    SlightLeft,
    Left,
    SharpLeft,
    SlightRight,
    Right,
    SharpRight,
    /// A U-turn that swings left: right-hand traffic.
    UTurnLeft,
    /// A U-turn that swings right: left-hand traffic (Singapore).
    UTurnRight,
    /// Counter-clockwise: right-hand traffic.
    RoundaboutCounterClockwise,
    /// Clockwise: left-hand traffic (Singapore).
    RoundaboutClockwise,
    Depart,
    Arrive,
}

/// The arrow for OSRM `step`'s maneuver.
#[must_use]
pub fn glyph_for(step: &OsrmStep) -> ManeuverGlyph {
    let left_hand_traffic = step.driving_side.as_deref() == Some("left");
    match step.maneuver.kind.as_str() {
        "arrive" => return ManeuverGlyph::Arrive,
        "depart" => return ManeuverGlyph::Depart,
        "roundabout" | "rotary" => {
            return if left_hand_traffic {
                ManeuverGlyph::RoundaboutClockwise
            } else {
                ManeuverGlyph::RoundaboutCounterClockwise
            };
        }
        _ => {}
    }
    match step.maneuver.modifier.as_deref() {
        Some("uturn") if left_hand_traffic => ManeuverGlyph::UTurnRight,
        Some("uturn") => ManeuverGlyph::UTurnLeft,
        Some("sharp left") => ManeuverGlyph::SharpLeft,
        Some("left") => ManeuverGlyph::Left,
        Some("slight left") => ManeuverGlyph::SlightLeft,
        Some("slight right") => ManeuverGlyph::SlightRight,
        Some("right") => ManeuverGlyph::Right,
        Some("sharp right") => ManeuverGlyph::SharpRight,
        _ => ManeuverGlyph::Straight,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManeuverView {
    pub glyph: ManeuverGlyph,
    pub maneuver_type: Option<ManeuverType>,
    pub modifier: Option<ManeuverModifier>,
    /// The roundabout exit, when there is one.
    pub exit_number: Option<u8>,
    pub primary: String,
    pub secondary: Option<String>,
}

/// Where the puck is. `on_route` is false while off the route, when the puck
/// sits at the raw position in a muted colour instead of on the line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Puck {
    pub lon: f64,
    pub lat: f64,
    /// Clockwise from north.
    pub course_deg: f64,
    pub on_route: bool,
}

impl Puck {
    /// Linear in position, shortest way round in course.
    #[must_use]
    pub fn lerp(self, to: Self, t: f64) -> Self {
        let t = t.clamp(0.0, 1.0);
        let turn = ((to.course_deg - self.course_deg + 540.0) % 360.0) - 180.0;
        Self {
            lon: self.lon + (to.lon - self.lon) * t,
            lat: self.lat + (to.lat - self.lat) * t,
            course_deg: (self.course_deg + turn * t).rem_euclid(360.0),
            on_route: if t < 1.0 { self.on_route && to.on_route } else { to.on_route },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NavigationState {
    pub phase: TripPhase,
    pub camera_mode: CameraMode,
    /// The step being driven, and how many there are.
    pub step_index: usize,
    pub step_count: usize,
    /// The next maneuver: what the banner shows. `None` while off the route
    /// (Ferrostar withholds instructions there) and once arrived.
    pub maneuver: Option<ManeuverView>,
    pub distance_to_maneuver_m: f64,
    /// The road being driven.
    pub road_name: Option<String>,
    /// The maneuver after the next one, when it follows closely.
    pub then: Option<ManeuverView>,
    pub remaining_distance_m: f64,
    pub remaining_duration_s: f64,
    /// Wall-clock arrival, Unix ms.
    pub eta_unix_ms: f64,
    pub utc_offset_min: i32,
    pub trip_elapsed_ms: f64,
    pub puck: Option<Puck>,
    /// Voice guidance is muted: the chrome's speaker toggle shows it. What
    /// to say is `nav::voice`'s, handed out by `NavHost::take_voice`.
    pub voice_muted: bool,
    /// The simulated drive's controls, for the chrome's pause toggle and
    /// speed chip. `None` for a trip no simulator drives (a live trip).
    pub sim: Option<SimView>,
}

/// A simulated drive as the chrome shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimView {
    pub paused: bool,
    /// 1, 2, 4 or 8.
    pub speed: u8,
    /// Stopped with the strip's stop button: paused, and the button in
    /// its place is Play (start the drive again), not Stop.
    pub stopped: bool,
}

fn maneuver_view(route: &NavRoute, index: usize, primary: String, secondary: Option<String>) -> ManeuverView {
    let step = &route.steps[index];
    ManeuverView {
        glyph: glyph_for(step),
        maneuver_type: maneuver_type(&step.maneuver.kind),
        modifier: maneuver_modifier(step.maneuver.modifier.as_deref()),
        exit_number: step.maneuver.exit.map(|e| e.min(u32::from(u8::MAX)) as u8),
        primary,
        secondary,
    }
}

/// The state for this instant.
#[must_use]
pub fn build(session: &TripSession, camera_mode: CameraMode, puck: Option<Puck>, wall: WallClock, now_ms: f64) -> NavigationState {
    let route = session.route();
    let count = route.steps.len();
    let step_index = session.step_index();
    let phase = if session.arrived() {
        TripPhase::Arrived
    } else if session.reroute_in_flight() {
        TripPhase::Rerouting
    } else if session.off_route() {
        TripPhase::OffRoute
    } else {
        TripPhase::Navigating
    };
    let mut state = NavigationState {
        phase,
        camera_mode,
        step_index,
        step_count: count,
        maneuver: None,
        distance_to_maneuver_m: 0.0,
        road_name: route.route.steps.get(step_index).and_then(|s| s.road_name.clone()),
        then: None,
        remaining_distance_m: 0.0,
        remaining_duration_s: 0.0,
        eta_unix_ms: wall.unix_ms,
        utc_offset_min: wall.utc_offset_min,
        trip_elapsed_ms: session.elapsed_ms(now_ms),
        puck,
        voice_muted: false,
        sim: None,
    };
    if phase == TripPhase::Arrived {
        return state;
    }
    if let TripState::Navigating { progress, visual_instruction, .. } = session.trip_state() {
        let upcoming = (step_index + 1).min(count.saturating_sub(1));
        state.maneuver = visual_instruction.map(|visual| {
            maneuver_view(route, upcoming, visual.primary_content.text, visual.secondary_content.map(|s| s.text))
        });
        if state.maneuver.is_some() && step_index + 2 < count && route.steps[step_index + 1].distance <= THEN_HINT_MAX_M {
            let after = step_index + 2;
            state.then = Some(maneuver_view(route, after, route.route.steps[after].instruction.clone(), None));
        }
        state.distance_to_maneuver_m = progress.distance_to_next_maneuver;
        state.remaining_distance_m = progress.distance_remaining;
        state.remaining_duration_s = progress.duration_remaining;
        state.eta_unix_ms = wall.unix_ms + progress.duration_remaining * 1000.0;
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(json: &str) -> OsrmStep {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_glyph_follows_the_maneuver_and_the_side_of_the_road() {
        assert_eq!(glyph_for(&step(r#"{"maneuver":{"type":"turn","modifier":"sharp right"}}"#)), ManeuverGlyph::SharpRight);
        assert_eq!(glyph_for(&step(r#"{"maneuver":{"type":"continue","modifier":"uturn"},"driving_side":"right"}"#)), ManeuverGlyph::UTurnLeft);
        assert_eq!(glyph_for(&step(r#"{"maneuver":{"type":"continue","modifier":"uturn"},"driving_side":"left"}"#)), ManeuverGlyph::UTurnRight);
        assert_eq!(glyph_for(&step(r#"{"maneuver":{"type":"roundabout","modifier":"left","exit":2},"driving_side":"left"}"#)), ManeuverGlyph::RoundaboutClockwise);
        assert_eq!(glyph_for(&step(r#"{"maneuver":{"type":"roundabout turn","modifier":"left"}}"#)), ManeuverGlyph::Left, "a mini-roundabout is drawn as the turn it is");
        assert_eq!(glyph_for(&step(r#"{"maneuver":{"type":"new name","modifier":"straight"}}"#)), ManeuverGlyph::Straight);
        assert_eq!(glyph_for(&step(r#"{"maneuver":{"type":"arrive","modifier":"right"}}"#)), ManeuverGlyph::Arrive);
    }

    #[test]
    fn a_puck_turns_the_short_way_round() {
        let a = Puck { lon: 0.0, lat: 0.0, course_deg: 350.0, on_route: true };
        let b = Puck { lon: 2.0, lat: 2.0, course_deg: 10.0, on_route: true };
        let mid = a.lerp(b, 0.5);
        assert!((mid.lon - 1.0).abs() < 1e-12 && (mid.course_deg - 0.0).abs() < 1e-9, "{mid:?}");
    }
}
