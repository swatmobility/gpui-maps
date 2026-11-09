//! One trip: a session, the drive that feeds it, the puck between fixes,
//! and the `NavigationState` all of it adds up to. Time comes in through
//! [`TripClock`]; nothing here reads a clock.

use crate::nav::format::WallClock;
use crate::nav::route_source::NavRoute;
use crate::nav::session::{ReroutePolicy, TripSession};
use crate::nav::simulate::{metres_between, SimOptions, SimulatedDrive};
use crate::nav::view_model::{self, CameraMode, NavigationState, Puck, SimView};
use ferrostar::models::UserLocation;
use ferrostar::navigation_controller::models::TripState;

/// The shell's two clocks for this frame: monotonic `now_ms` for motion, and
/// the wall clock for the ETA.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TripClock {
    pub now_ms: f64,
    pub wall: WallClock,
}

/// A reroute to request: from here to where the trip is still going.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerouteNeed {
    pub origin: (f64, f64),
    pub destination: (f64, f64),
}

/// What one `advance` did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TripTick {
    /// Ask for another frame. `false` once paused (or arrived) and the puck
    /// has finished gliding -- the redraw contract the iOS heat fix needs.
    pub redraw: bool,
    pub step_advanced: bool,
    pub arrived_now: bool,
    pub reroute: Option<RerouteNeed>,
}

pub struct NavTrip {
    session: TripSession,
    drive: Option<SimulatedDrive>,
    puck_from: Option<Puck>,
    puck_to: Option<Puck>,
    camera_mode: CameraMode,
    state: NavigationState,
    voice_muted: bool,
    /// [`Self::stop`]ped: paused for good; only a restart drives again.
    stopped: bool,
}

fn puck_of(session: &TripSession) -> Option<Puck> {
    let at = |location: &UserLocation, on_route: bool| Puck {
        lon: location.coordinates.lng,
        lat: location.coordinates.lat,
        course_deg: location.course_over_ground.map_or(0.0, |c| f64::from(c.degrees)),
        on_route,
    };
    match session.trip_state() {
        TripState::Navigating { user_location, snapped_user_location, deviation, .. } => Some(if deviation.is_completely_off_route() {
            at(&user_location, false)
        } else {
            at(&snapped_user_location, true)
        }),
        TripState::Complete { user_location, .. } => Some(at(&user_location, true)),
        TripState::Idle { .. } => None,
    }
}

impl NavTrip {
    /// A simulated drive over `route`, seeked to `options.start_at_m`.
    pub fn simulated(route: NavRoute, options: SimOptions, clock: TripClock) -> Result<Self, String> {
        let mut drive = SimulatedDrive::new(&route, options)?;
        let mut session = TripSession::new(route, drive.first_location(), clock.now_ms, ReroutePolicy::default());
        if let Some(at_m) = options.start_at_m.filter(|m| m.is_finite() && *m > 0.0) {
            let mut travelled = 0.0;
            let mut last = drive.first_location();
            while travelled < at_m {
                let Some(location) = drive.tick(session.step_index()) else { break };
                travelled += metres_between(
                    (last.coordinates.lng, last.coordinates.lat),
                    (location.coordinates.lng, location.coordinates.lat),
                );
                last = location;
                if session.update(location, clock.now_ms).arrived {
                    break;
                }
            }
        }
        let puck = puck_of(&session);
        let state = view_model::build(&session, CameraMode::Following, puck, clock.wall, clock.now_ms);
        let mut trip =
            Self { session, drive: Some(drive), puck_from: puck, puck_to: puck, camera_mode: CameraMode::Following, state, voice_muted: false, stopped: false };
        trip.refresh_overlays();
        Ok(trip)
    }

    pub fn advance(&mut self, clock: TripClock) -> TripTick {
        let mut tick = TripTick::default();
        if let Some(drive) = self.drive.as_mut() {
            let locations = drive.poll(clock.now_ms, self.session.step_index());
            let moved = !locations.is_empty();
            for location in locations {
                let update = self.session.update(location, clock.now_ms);
                tick.step_advanced |= update.step_advanced;
                tick.arrived_now |= update.arrived;
                if update.reroute_needed {
                    tick.reroute = self.session.route().destination().map(|destination| RerouteNeed {
                        origin: (location.coordinates.lng, location.coordinates.lat),
                        destination,
                    });
                }
            }
            if drive.is_finished() && !self.session.arrived() {
                self.session.end_of_drive(clock.now_ms);
                tick.arrived_now = true;
            }
            if moved {
                self.puck_from = self.puck_to;
                self.puck_to = puck_of(&self.session);
            }
        }
        let fraction = self.drive.as_ref().map_or(1.0, |d| d.tick_fraction(clock.now_ms));
        let driving = self.drive.as_ref().is_some_and(|d| !d.is_paused() && !d.is_finished());
        tick.redraw = (driving && !self.session.arrived()) || fraction < 1.0;
        self.state = view_model::build(&self.session, self.camera_mode, self.puck(fraction), clock.wall, clock.now_ms);
        self.refresh_overlays();
        tick
    }

    fn puck(&self, fraction: f64) -> Option<Puck> {
        match (self.puck_from, self.puck_to) {
            (Some(from), Some(to)) => Some(from.lerp(to, fraction)),
            (_, to) => to,
        }
    }

    /// A reroute came back. `Ok` swaps the route in (a simulated drive
    /// starts again from the new route's first point); `Err` waits the
    /// policy's pause and tries again.
    pub fn reroute_arrived(&mut self, result: Result<NavRoute, String>, clock: TripClock) {
        match result {
            Ok(route) => {
                let restarted = self.drive.as_mut().and_then(|drive| drive.restart_on(&route).ok().map(|()| drive.first_location()));
                let location = restarted.or_else(|| match self.session.trip_state() {
                    TripState::Navigating { user_location, .. } => Some(user_location),
                    _ => None,
                });
                if let Some(location) = location {
                    self.session.replace_route(route, location, clock.now_ms);
                    self.puck_from = puck_of(&self.session);
                    self.puck_to = self.puck_from;
                }
            }
            Err(_) => self.session.reroute_failed(clock.now_ms),
        }
        self.state = view_model::build(&self.session, self.camera_mode, self.puck_to, clock.wall, clock.now_ms);
        self.refresh_overlays();
    }

    #[must_use]
    pub fn state(&self) -> &NavigationState {
        &self.state
    }

    #[must_use]
    pub fn session(&self) -> &TripSession {
        &self.session
    }

    #[must_use]
    pub fn drive(&self) -> Option<&SimulatedDrive> {
        self.drive.as_ref()
    }

    pub fn drive_mut(&mut self) -> Option<&mut SimulatedDrive> {
        self.drive.as_mut()
    }

    /// What the state carries beyond the session: the mute and the drive's
    /// controls. Rebuilt with the state, and at once when either changes,
    /// so a tap shows on the very next scene.
    fn refresh_overlays(&mut self) {
        self.state.voice_muted = self.voice_muted;
        self.state.sim = self.drive.as_ref().map(|d| SimView { paused: d.is_paused(), speed: d.speed(), stopped: self.stopped });
    }

    /// Stop a simulated drive: paused, and flagged so the chrome offers
    /// Play (a restart) where Stop was. Nothing for a live trip.
    pub fn stop(&mut self) {
        if let Some(drive) = self.drive.as_mut() {
            drive.set_paused(true);
            self.stopped = true;
        }
        self.refresh_overlays();
    }

    /// Whether [`Self::stop`] stopped the drive.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Pause or resume the simulated drive; nothing for a live trip, nor
    /// for a stopped drive (only a restart drives it again).
    pub fn toggle_pause(&mut self) {
        if self.stopped {
            return;
        }
        if let Some(drive) = self.drive.as_mut() {
            let paused = drive.is_paused();
            drive.set_paused(!paused);
        }
        self.refresh_overlays();
    }

    /// 1x -> 2x -> 4x -> 8x -> 1x; nothing for a live trip.
    pub fn cycle_speed(&mut self) {
        if let Some(drive) = self.drive.as_mut() {
            drive.cycle_speed();
        }
        self.refresh_overlays();
    }

    /// What the chrome's speaker toggle shows; the announcer owns the mute itself.
    pub fn set_voice_muted(&mut self, muted: bool) {
        self.voice_muted = muted;
        self.state.voice_muted = muted;
    }

    pub fn set_camera_mode(&mut self, mode: CameraMode) {
        self.camera_mode = mode;
        self.state.camera_mode = mode;
    }

    #[must_use]
    pub fn camera_mode(&self) -> CameraMode {
        self.camera_mode
    }

    /// The latest fix's ground speed, m/s, when it carries one: what the
    /// follow camera's zoom reads. A simulated drive reports its driving
    /// speed (36 km/h), whatever the playback factor.
    #[must_use]
    pub fn speed_mps(&self) -> Option<f64> {
        match self.session.trip_state() {
            TripState::Navigating { user_location, .. } | TripState::Complete { user_location, .. } => {
                user_location.speed.map(|s| s.value).filter(|v| v.is_finite() && *v >= 0.0)
            }
            TripState::Idle { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::route_source::parse_route;
    use crate::nav::view_model::TripPhase;

    const MANILA: &[u8] = include_bytes!("../../tests/fixtures/routes/manila.json");
    /// 2026-03-17T13:50:00Z, the Manila leg's own `start_time` (21:50 local).
    const WALL: WallClock = WallClock { unix_ms: 1_773_755_400_000.0, utc_offset_min: 480 };

    fn clock(now_ms: f64) -> TripClock {
        TripClock { now_ms, wall: WallClock { unix_ms: WALL.unix_ms + now_ms, ..WALL } }
    }

    fn manila_trip(options: SimOptions) -> NavTrip {
        NavTrip::simulated(parse_route(MANILA).unwrap(), options, clock(0.0)).unwrap()
    }

    #[test]
    fn the_banner_changes_at_every_step_and_the_drive_arrives() {
        let mut trip = manila_trip(SimOptions::default());
        let mut banners: Vec<String> = Vec::new();
        let mut arrived_at = None;
        for second in 0..=600u32 {
            let tick = trip.advance(clock(f64::from(second) * 1000.0));
            let state = trip.state();
            assert_ne!(state.phase, TripPhase::OffRoute, "second {second}");
            assert_eq!(state.camera_mode, CameraMode::Following);
            if let Some(maneuver) = &state.maneuver {
                if banners.last() != Some(&maneuver.primary) {
                    banners.push(maneuver.primary.clone());
                }
            }
            if tick.arrived_now {
                arrived_at = Some(second);
                break;
            }
        }
        assert_eq!(
            banners,
            [
                "Make a slight left to stay on Padre Burgos Avenue (170)",
                "Continue onto Taft Avenue (170)",
                "Make a sharp right onto Finance Road (180)",
                "Make a U-turn and continue on Finance Road (180)",
                "Turn left onto Ayala Boulevard (180)",
                "Continue onto P. Casal Street (180)",
                "Continue onto Legarda Street (180)",
                "Turn left onto J. Figueras Street",
                "Turn right onto F. Jhocson Street",
                "You have arrived at your destination, on the right",
            ]
        );
        assert!((360..=370).contains(&arrived_at.expect("arrives")), "{arrived_at:?}");
        let state = trip.state();
        assert_eq!(state.phase, TripPhase::Arrived);
        assert!(state.maneuver.is_none() && state.puck.is_some());
        assert!(!trip.advance(clock(700_000.0)).redraw, "an arrived trip stops asking for frames");
    }

    #[test]
    fn the_first_banner_and_the_eta_come_from_the_route() {
        let mut trip = manila_trip(SimOptions::default());
        trip.advance(clock(0.0));
        let state = trip.state();
        assert_eq!(state.step_index, 0);
        assert!((state.distance_to_maneuver_m - 171.7).abs() < 1.0, "{}", state.distance_to_maneuver_m);
        assert!((state.remaining_distance_m - 3211.7).abs() < 1.0);
        assert!((state.eta_unix_ms - (WALL.unix_ms + 565.7 * 1000.0)).abs() < 1.0);
        assert_eq!(state.road_name.as_deref(), Some("Padre Burgos Avenue"));
        assert!(state.then.is_none(), "step 1 is 150.7 m long, just past the hint's 150 m");
    }

    #[test]
    fn a_close_follow_up_maneuver_is_hinted() {
        // Step 2 (Taft Avenue) ends in the sharp right; step 3 is 81.7 m, then the U-turn.
        let mut trip = manila_trip(SimOptions { start_at_m: Some(400.0), ..SimOptions::default() });
        trip.advance(clock(0.0));
        let state = trip.state();
        assert_eq!(state.step_index, 2);
        assert_eq!(state.maneuver.as_ref().map(|m| m.glyph), Some(view_model::ManeuverGlyph::SharpRight));
        let then = state.then.as_ref().expect("the U-turn follows 82 m later");
        assert_eq!(then.glyph, view_model::ManeuverGlyph::UTurnLeft, "Manila drives on the right");
    }

    #[test]
    fn a_paused_drive_settles_and_stops_asking_for_frames() {
        let mut trip = manila_trip(SimOptions::default());
        for second in 0..20u32 {
            assert!(trip.advance(clock(f64::from(second) * 1000.0)).redraw);
        }
        trip.drive_mut().unwrap().set_paused(true);
        assert!(!trip.advance(clock(20_000.0)).redraw, "the first paused frame already settles");
        let settled = trip.state().clone();
        assert!(!trip.advance(clock(90_000.0)).redraw);
        assert_eq!(trip.state().step_index, settled.step_index);
        assert_eq!(trip.state().puck, settled.puck, "nothing moves while paused");
    }

    #[test]
    fn a_seek_starts_the_drive_part_way_along() {
        let mut trip = manila_trip(SimOptions { start_at_m: Some(1_200.0), paused: true, ..SimOptions::default() });
        trip.advance(clock(0.0));
        assert_eq!(trip.state().step_index, 5, "1.2 km in is on Ayala Boulevard");
        assert_eq!(trip.state().road_name.as_deref(), Some("Ayala Boulevard"));
    }

    #[test]
    fn leaving_the_route_asks_for_one_reroute_and_the_answer_resumes_the_trip() {
        let mut trip = manila_trip(SimOptions { off_route_after_step: Some(5), ..SimOptions::default() });
        let mut needs = Vec::new();
        let mut phases = Vec::new();
        for second in 0..=140u32 {
            let tick = trip.advance(clock(f64::from(second) * 1000.0));
            if let Some(need) = tick.reroute {
                needs.push((second, need));
            }
            if phases.last() != Some(&trip.state().phase) {
                phases.push(trip.state().phase);
            }
        }
        assert_eq!(needs.len(), 1, "{needs:?}");
        assert_eq!(needs[0].1.destination, (120.993246, 14.604056));
        assert_eq!(phases, [TripPhase::Navigating, TripPhase::OffRoute, TripPhase::Rerouting]);
        assert!(trip.state().maneuver.is_none(), "no instruction is shown while off the route");
        assert!(!trip.state().puck.unwrap().on_route, "the puck leaves the line");

        trip.reroute_arrived(Err("timed out".into()), clock(141_000.0));
        assert_eq!(trip.state().phase, TripPhase::OffRoute, "a failure waits, then tries again");
        let retry = (142..=150u32).find_map(|s| trip.advance(clock(f64::from(s) * 1000.0)).reroute.map(|_| s));
        assert_eq!(retry, Some(146), "five seconds after the failure");

        trip.reroute_arrived(Ok(parse_route(MANILA).unwrap()), clock(147_000.0));
        assert_eq!(trip.state().phase, TripPhase::Navigating);
        assert_eq!(trip.state().step_index, 0);
        assert_eq!(trip.session().route_generation(), 1);
    }

    const BUGIS: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-bugis-marina-bay.json");
    const SELETAR: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-seletar-rotary-uturn.json");

    /// Every distinct banner and its glyph, the arrival second, and the route length.
    fn drive_to_the_end(bytes: &[u8]) -> (Vec<String>, Vec<view_model::ManeuverGlyph>, Option<u32>, f64) {
        let route = parse_route(bytes).unwrap();
        let distance = route.route.distance;
        let mut trip = NavTrip::simulated(route, SimOptions::default(), clock(0.0)).unwrap();
        let (mut banners, mut glyphs, mut arrived) = (Vec::new(), Vec::new(), None);
        for second in 0..=1_200u32 {
            let tick = trip.advance(clock(f64::from(second) * 1000.0));
            assert_ne!(trip.state().phase, TripPhase::OffRoute, "second {second}: a faithful drive never leaves the route");
            if let Some(maneuver) = &trip.state().maneuver {
                if banners.last() != Some(&maneuver.primary) {
                    banners.push(maneuver.primary.clone());
                    glyphs.push(maneuver.glyph);
                }
            }
            if tick.arrived_now {
                arrived = Some(second);
                break;
            }
        }
        (banners, glyphs, arrived, distance)
    }

    #[test]
    fn the_bugis_drive_keeps_right_at_the_fork_and_arrives() {
        let (banners, _, arrived, distance) = drive_to_the_end(BUGIS);
        assert!(banners.iter().any(|b| b == "Keep right onto Ophir Road"), "{banners:#?}");
        let arrived = f64::from(arrived.expect("the drive arrives"));
        // One fix per tick, at most 10 m apart (Ferrostar's densify), and
        // closer where the geometry's own vertices are: 449 s for 3697 m,
        // about 8.2 m/s, measured. Never faster than 10 m/s.
        assert!(arrived >= distance / 10.0, "{arrived} s for {distance} m");
        assert!(arrived < distance / 8.0 + 30.0, "{arrived} s for {distance} m");
    }

    #[test]
    fn the_seletar_drive_shows_the_rotary_and_a_right_hand_u_turn() {
        let (banners, glyphs, arrived, _) = drive_to_the_end(SELETAR);
        assert!(arrived.is_some());
        assert!(banners.iter().any(|b| b.starts_with("Enter ") && b.contains("1st exit")), "{banners:#?}");
        assert!(banners.iter().any(|b| b.starts_with("Make a U-turn")), "{banners:#?}");
        assert!(glyphs.contains(&view_model::ManeuverGlyph::RoundaboutClockwise), "left-hand traffic: {glyphs:?}");
        assert!(glyphs.contains(&view_model::ManeuverGlyph::UTurnRight), "left-hand traffic: {glyphs:?}");
    }
}
