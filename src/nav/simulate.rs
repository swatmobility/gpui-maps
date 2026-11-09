//! A drive along a route, one location per tick: Ferrostar's
//! `location_simulation_from_route` resampled every [`SIM_RESAMPLE_M`], with a
//! speed factor, pause, a seek, and an off-route mode that drifts off the
//! route from a chosen step so rerouting can be exercised.
//!
//! Ferrostar's own contract is "intended to be called once/second"
//! (`simulation.rs`, `advance_location_simulation`), so one tick is one
//! second of DRIVING: [`SIM_RESAMPLE_M`] metres at 36 km/h. The speed factor
//! only changes how much wall time a tick takes. Every location's timestamp
//! is the trip clock ([`crate::nav::time::timestamp_at`]), replacing the
//! `SystemTime::now()` Ferrostar stamps it with, so a drive is identical
//! every time it is run.

use crate::nav::route_source::NavRoute;
use crate::nav::time::timestamp_at;
use ferrostar::models::{Speed, UserLocation};
use ferrostar::simulation::{
    advance_location_simulation, location_simulation_from_route, LocationBias, LocationSimulationState,
};

/// At most this many metres between simulated fixes (Ferrostar's densify
/// splits each segment evenly, so the real spacing is a little under).
pub const SIM_RESAMPLE_M: f64 = 10.0;
/// Driving time per tick, whatever the speed factor.
pub const SIM_TICK_DRIVE_MS: f64 = 1_000.0;
/// The speed factors a shell cycles through.
pub const SPEED_FACTORS: [u8; 4] = [1, 2, 4, 8];
/// A stall (a hidden window, a backgrounded tab or app, App Nap) catches
/// up every tick it missed, so the drive is where wall-clock time says it
/// is when the map comes back -- a real navigation app keeps driving while
/// another app is in front. Each missed tick is fed to the navigator in
/// order (a fix far down the route with no fixes between would read as
/// off-route), but only the last state is shown and spoken, so nothing is
/// replayed on screen or aloud. Bounded so a pathological stall cannot hold
/// a frame for long: an hour of driving at 1x, far more than any demo route
/// (Manila's is ~320 ticks); past it the rest is dropped and the clock
/// re-arms from now.
pub const MAX_CATCH_UP_TICKS: usize = 3_600;
/// How far each tick drifts off the route once off-route mode starts.
pub const OFF_ROUTE_DRIFT_M_PER_TICK: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimOptions {
    /// One of [`SPEED_FACTORS`]; anything else is clamped into 1..=8.
    pub speed: u8,
    pub paused: bool,
    /// Start drifting off the route once the trip reaches this step index.
    pub off_route_after_step: Option<usize>,
    /// Begin this far along the route (metres), for captures.
    pub start_at_m: Option<f64>,
}

impl Default for SimOptions {
    fn default() -> Self {
        Self { speed: 1, paused: false, off_route_after_step: None, start_at_m: None }
    }
}

pub struct SimulatedDrive {
    state: LocationSimulationState,
    speed: u8,
    paused: bool,
    off_route_after_step: Option<usize>,
    /// Ticks since the drift began; `None` until it does.
    drift: Option<u32>,
    next_tick_ms: Option<f64>,
    ticks: u64,
    finished: bool,
}

impl SimulatedDrive {
    pub fn new(route: &NavRoute, options: SimOptions) -> Result<Self, String> {
        let state = location_simulation_from_route(&route.route, Some(SIM_RESAMPLE_M), LocationBias::None)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            state,
            speed: options.speed.clamp(1, 8),
            paused: options.paused,
            off_route_after_step: options.off_route_after_step,
            drift: None,
            next_tick_ms: None,
            ticks: 0,
            finished: false,
        })
    }

    /// Where the drive starts, stamped at trip time zero.
    #[must_use]
    pub fn first_location(&self) -> UserLocation {
        self.stamp(self.state.current_location)
    }

    /// The locations due by `now_ms`. `current_step` is the navigator's step
    /// index before these are fed to it; off-route mode keys on it.
    pub fn poll(&mut self, now_ms: f64, current_step: usize) -> Vec<UserLocation> {
        if self.finished || self.paused {
            self.next_tick_ms = None;
            return Vec::new();
        }
        let interval = self.tick_interval_ms();
        let mut next = *self.next_tick_ms.get_or_insert(now_ms + interval);
        let mut out = Vec::new();
        while now_ms >= next && out.len() < MAX_CATCH_UP_TICKS {
            match self.tick(current_step) {
                Some(location) => out.push(location),
                None => {
                    self.finished = true;
                    break;
                }
            }
            next += interval;
        }
        if now_ms >= next {
            next = now_ms + interval;
        }
        self.next_tick_ms = Some(next);
        out
    }

    /// One tick, now, whatever the clock says: how a seek fast-forwards.
    pub fn tick(&mut self, current_step: usize) -> Option<UserLocation> {
        let next = advance_location_simulation(&self.state);
        if next == self.state {
            self.finished = true;
            return None;
        }
        self.state = next;
        self.ticks += 1;
        let mut location = self.stamp(self.state.current_location);
        if self.drift.is_some() || self.off_route_after_step.is_some_and(|after| current_step >= after) {
            let n = self.drift.unwrap_or(0);
            location = displaced(location, OFF_ROUTE_DRIFT_M_PER_TICK * f64::from(n));
            self.drift = Some(n + 1);
        }
        Some(location)
    }

    /// Fraction of the current tick's wall time that has passed, for the puck.
    #[must_use]
    pub fn tick_fraction(&self, now_ms: f64) -> f64 {
        match self.next_tick_ms {
            Some(next) if !self.paused && !self.finished => (1.0 - (next - now_ms) / self.tick_interval_ms()).clamp(0.0, 1.0),
            _ => 1.0,
        }
    }

    /// Drive the new route from its start; off-route mode is one-shot.
    pub fn restart_on(&mut self, route: &NavRoute) -> Result<(), String> {
        self.state = location_simulation_from_route(&route.route, Some(SIM_RESAMPLE_M), LocationBias::None)
            .map_err(|e| e.to_string())?;
        self.off_route_after_step = None;
        self.drift = None;
        self.finished = false;
        self.next_tick_ms = None;
        Ok(())
    }

    #[must_use]
    pub fn tick_interval_ms(&self) -> f64 {
        SIM_TICK_DRIVE_MS / f64::from(self.speed)
    }

    #[must_use]
    pub fn speed(&self) -> u8 {
        self.speed
    }

    /// 1 -> 2 -> 4 -> 8 -> 1.
    pub fn cycle_speed(&mut self) {
        let at = SPEED_FACTORS.iter().position(|s| *s == self.speed).unwrap_or(0);
        self.speed = SPEED_FACTORS[(at + 1) % SPEED_FACTORS.len()];
        self.next_tick_ms = None;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.next_tick_ms = None;
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    fn stamp(&self, mut location: UserLocation) -> UserLocation {
        location.timestamp = timestamp_at(self.ticks as f64 * SIM_TICK_DRIVE_MS);
        location.speed = Some(Speed { value: SIM_RESAMPLE_M / (SIM_TICK_DRIVE_MS / 1000.0), accuracy: None });
        location
    }
}

/// Great-circle metres between two `(lon, lat)` points.
#[must_use]
pub fn metres_between(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lat2) = (a.1.to_radians(), b.1.to_radians());
    let (dlat, dlon) = (lat2 - lat1, (b.0 - a.0).to_radians());
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * h.sqrt().asin()
}

/// `location` moved `metres` to the right of its course.
fn displaced(location: UserLocation, metres: f64) -> UserLocation {
    let course = location.course_over_ground.map_or(0.0, |c| f64::from(c.degrees));
    let bearing = (course + 90.0).to_radians();
    let mut moved = location;
    moved.coordinates.lat += metres * bearing.cos() / 111_111.0;
    moved.coordinates.lng += metres * bearing.sin() / (111_111.0 * moved.coordinates.lat.to_radians().cos());
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::route_source::parse_route;
    use crate::nav::session::{ReroutePolicy, TripSession};

    const MANILA: &[u8] = include_bytes!("../../tests/fixtures/routes/manila.json");

    #[test]
    fn a_manila_drive_at_1x_arrives_in_about_six_minutes_of_ticks() {
        let route = parse_route(MANILA).unwrap();
        let mut drive = SimulatedDrive::new(&route, SimOptions::default()).unwrap();
        let mut session = TripSession::new(route, drive.first_location(), 0.0, ReroutePolicy::default());
        let mut arrived_at = None;
        for second in 1..=600u32 {
            let now = f64::from(second) * 1000.0;
            for location in drive.poll(now, session.step_index()) {
                if session.update(location, now).arrived {
                    arrived_at = Some(second);
                }
            }
            if arrived_at.is_some() {
                break;
            }
        }
        let arrived_at = arrived_at.expect("the drive arrives");
        assert!((360..=370).contains(&arrived_at), "3.2 km at 10 m/s is ~320 s of road plus the arrival: {arrived_at}");
        assert!(!session.off_route(), "a faithful drive never leaves the route");
    }

    #[test]
    fn each_tick_is_one_second_of_driving_whatever_the_speed() {
        let route = parse_route(MANILA).unwrap();
        let mut drive = SimulatedDrive::new(&route, SimOptions { speed: 4, ..SimOptions::default() }).unwrap();
        assert!(drive.poll(0.0, 0).is_empty(), "the first poll arms the clock");
        let ticks = drive.poll(1000.0, 0);
        assert_eq!(ticks.len(), 4, "4x is four ticks per wall second");
        let stamps: Vec<_> = ticks.iter().map(|l| l.timestamp).collect();
        assert_eq!(stamps[3].duration_since(stamps[0]).unwrap().as_millis(), 3_000, "stamped on the trip clock");
    }

    #[test]
    fn a_paused_drive_emits_nothing_and_a_stall_catches_up_boundedly() {
        let route = parse_route(MANILA).unwrap();
        let mut drive = SimulatedDrive::new(&route, SimOptions { paused: true, ..SimOptions::default() }).unwrap();
        assert!(drive.poll(0.0, 0).is_empty() && drive.poll(60_000.0, 0).is_empty());
        drive.set_paused(false);
        assert!(drive.poll(60_000.0, 0).is_empty());
        // The user's "the drive pauses when Studio loses focus": a
        // 30-second stall (another app in front, the window not painting)
        // is 30 seconds of driving when the map comes back, not 8.
        assert_eq!(drive.poll(90_000.0, 0).len(), 30, "a stall catches up to wall-clock time");
        assert_eq!(drive.poll(90_500.0, 0).len(), 0);
        // Past the bound the rest is dropped (Manila's route ends first).
        let mut long = SimulatedDrive::new(&route, SimOptions::default()).unwrap();
        assert!(long.poll(0.0, 0).is_empty());
        let caught_up = long.poll(10_000_000.0, 0).len();
        assert!(caught_up <= MAX_CATCH_UP_TICKS && long.is_finished(), "{caught_up} ticks, finished {}", long.is_finished());
    }

    #[test]
    fn off_route_mode_triggers_exactly_one_reroute_about_three_seconds_after_leaving() {
        let route = parse_route(MANILA).unwrap();
        let options = SimOptions { off_route_after_step: Some(5), ..SimOptions::default() };
        let mut drive = SimulatedDrive::new(&route, options).unwrap();
        let mut session = TripSession::new(route.clone(), drive.first_location(), 0.0, ReroutePolicy::default());
        let mut requests = Vec::new();
        for second in 1..=140u32 {
            let now = f64::from(second) * 1000.0;
            for location in drive.poll(now, session.step_index()) {
                if session.update(location, now).reroute_needed {
                    requests.push(second);
                }
            }
        }
        // Measured: the drift starts on step 5 at tick 83, Ferrostar calls it
        // off the route at 20 m (drift tick 7) and it is 32 m off three
        // seconds later.
        assert_eq!(requests.len(), 1, "one request while it is in flight: {requests:?}");
        assert!((90..=96).contains(&requests[0]), "{requests:?}");
        assert!(session.reroute_in_flight());

        drive.restart_on(&route).unwrap();
        session.replace_route(route.clone(), drive.first_location(), 141_000.0);
        assert!(!session.off_route() && session.step_index() == 0);
        assert_eq!(session.route_generation(), 1);
    }
}
