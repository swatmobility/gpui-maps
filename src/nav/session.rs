//! One trip through Ferrostar's navigator, plus the parts Ferrostar leaves
//! to the app: when to ask for a new route (the reroute policy), the trip
//! clock, and arrival.
//!
//! Ferrostar decides WHETHER the user is off the route
//! (`RouteDeviationTracking::StaticThreshold`); it deliberately does not
//! decide what to do about it (`deviation_detection.rs`'s module doc: "the
//! decision to reroute ... lies with higher levels"). [`Reroute`] is that
//! decision: off the route entirely, by at least `min_off_route_m`, for at
//! least `min_off_route_ms`, with only one request in flight and a pause
//! after a failure.
//!
//! Time is a parameter everywhere (`now_ms`, the shell's monotonic clock).

use crate::nav::route_source::NavRoute;
use ferrostar::deviation_detection::{DeviationKind, RouteDeviation, RouteDeviationTracking};
use ferrostar::models::UserLocation;
use ferrostar::navigation_controller::models::{
    CourseFiltering, NavState, NavigationControllerConfig, TripState, WaypointAdvanceMode,
};
use ferrostar::navigation_controller::step_advance::conditions::{
    DistanceEntryAndExitCondition, DistanceToEndOfStepCondition,
};
use ferrostar::navigation_controller::{NavigationController, Navigator};
use std::sync::Arc;

/// Ferrostar's "off the route" line: further than this from every remaining
/// step. The reroute policy's own threshold ([`ReroutePolicy::min_off_route_m`])
/// sits above it, so a GPS wobble that crosses this one does not reroute.
pub const OFF_ROUTE_THRESHOLD_M: f64 = 20.0;
/// A fix worse than this is ignored for deviation and step advance alike.
pub const MIN_HORIZONTAL_ACCURACY_M: u16 = 25;
/// The final step completes within this distance of its end.
pub const ARRIVAL_DISTANCE_M: u16 = 10;
/// A waypoint counts as visited within this distance.
pub const WAYPOINT_RANGE_M: f64 = 100.0;

/// The navigator's configuration: Ferrostar's automotive defaults
/// (`DistanceEntryAndExitCondition::default()` -- 20 m in, 5 m out) with a
/// static off-route threshold and course snapped to the route.
#[must_use]
pub fn controller_config() -> NavigationControllerConfig {
    NavigationControllerConfig {
        waypoint_advance: WaypointAdvanceMode::WaypointWithinRange(WAYPOINT_RANGE_M),
        step_advance_condition: Arc::new(DistanceEntryAndExitCondition::default()),
        arrival_step_advance_condition: Arc::new(DistanceToEndOfStepCondition {
            distance: ARRIVAL_DISTANCE_M,
            minimum_horizontal_accuracy: MIN_HORIZONTAL_ACCURACY_M,
        }),
        route_deviation_tracking: RouteDeviationTracking::StaticThreshold {
            minimum_horizontal_accuracy: MIN_HORIZONTAL_ACCURACY_M,
            max_acceptable_deviation: OFF_ROUTE_THRESHOLD_M,
        },
        snapped_location_course_filtering: CourseFiltering::SnapToRoute,
    }
}

/// When a deviation becomes a route request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReroutePolicy {
    /// Off the route by at least this much...
    pub min_off_route_m: f64,
    /// ...continuously for at least this long.
    pub min_off_route_ms: f64,
    /// After a failed request, wait this long before the next.
    pub retry_after_failure_ms: f64,
}

impl Default for ReroutePolicy {
    fn default() -> Self {
        Self { min_off_route_m: 30.0, min_off_route_ms: 3_000.0, retry_after_failure_ms: 5_000.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RerouteState {
    OnRoute,
    OffRoute { since_ms: f64 },
    InFlight,
    Backoff { until_ms: f64 },
}

/// The reroute decision, fed one deviation per location update.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reroute {
    policy: ReroutePolicy,
    state: RerouteState,
}

impl Reroute {
    #[must_use]
    pub fn new(policy: ReroutePolicy) -> Self {
        Self { policy, state: RerouteState::OnRoute }
    }

    /// `true` exactly when a request should go out now.
    pub fn observe(&mut self, deviation: RouteDeviation, now_ms: f64) -> bool {
        let off_by = match deviation {
            RouteDeviation::Deviation { kind: DeviationKind::CompletelyOffRoute { deviation_from_route_line } } => {
                Some(deviation_from_route_line)
            }
            _ => None,
        };
        let far_enough = off_by.is_some_and(|m| m >= self.policy.min_off_route_m);
        match (self.state, off_by) {
            // One request at a time; its answer (`answered`) decides what next.
            (RerouteState::InFlight, _) => false,
            (_, None) => {
                self.state = RerouteState::OnRoute;
                false
            }
            (RerouteState::OnRoute, Some(_)) => {
                self.state = RerouteState::OffRoute { since_ms: now_ms };
                false
            }
            (RerouteState::OffRoute { since_ms }, Some(_)) => {
                let fire = far_enough && now_ms - since_ms >= self.policy.min_off_route_ms;
                if fire {
                    self.state = RerouteState::InFlight;
                }
                fire
            }
            (RerouteState::Backoff { until_ms }, Some(_)) => {
                let fire = far_enough && now_ms >= until_ms;
                if fire {
                    self.state = RerouteState::InFlight;
                }
                fire
            }
        }
    }

    /// The request came back: a new route (`ok`), or a failure that starts
    /// the pause before the next attempt.
    pub fn answered(&mut self, ok: bool, now_ms: f64) {
        self.state = if ok {
            RerouteState::OnRoute
        } else {
            RerouteState::Backoff { until_ms: now_ms + self.policy.retry_after_failure_ms }
        };
    }

    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.state == RerouteState::InFlight
    }

    /// Off the route, whether or not a request has gone out yet.
    #[must_use]
    pub fn off_route(&self) -> bool {
        self.state != RerouteState::OnRoute
    }
}

/// What one location update did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionUpdate {
    pub step_advanced: bool,
    pub reroute_needed: bool,
    pub arrived: bool,
}

/// One trip on one route at a time (a reroute swaps the route in place).
pub struct TripSession {
    route: NavRoute,
    controller: NavigationController,
    state: NavState,
    reroute: Reroute,
    started_ms: f64,
    arrived_ms: Option<f64>,
    route_generation: u64,
}

impl TripSession {
    #[must_use]
    pub fn new(route: NavRoute, first: UserLocation, now_ms: f64, policy: ReroutePolicy) -> Self {
        let controller = NavigationController::new(route.route.clone(), controller_config());
        let state = controller.get_initial_state(first);
        Self { route, controller, state, reroute: Reroute::new(policy), started_ms: now_ms, arrived_ms: None, route_generation: 0 }
    }

    pub fn update(&mut self, location: UserLocation, now_ms: f64) -> SessionUpdate {
        if self.arrived_ms.is_some() {
            return SessionUpdate::default();
        }
        let before = self.step_index();
        self.state = self.controller.update_user_location(location, self.state.clone());
        match self.state.trip_state() {
            TripState::Complete { .. } => {
                self.arrived_ms = Some(now_ms);
                SessionUpdate { step_advanced: true, reroute_needed: false, arrived: true }
            }
            TripState::Navigating { deviation, .. } => SessionUpdate {
                step_advanced: self.step_index() != before,
                reroute_needed: self.reroute.observe(deviation, now_ms),
                arrived: false,
            },
            TripState::Idle { .. } => SessionUpdate::default(),
        }
    }

    /// A reroute answered with `route`; `location` is where the trip is now.
    pub fn replace_route(&mut self, route: NavRoute, location: UserLocation, now_ms: f64) {
        self.controller = NavigationController::new(route.route.clone(), controller_config());
        self.state = self.controller.get_initial_state(location);
        self.route = route;
        self.route_generation += 1;
        self.reroute.answered(true, now_ms);
    }

    /// The position source has run out (a simulated drive reached its last
    /// point) without Ferrostar calling it complete: the trip is over anyway.
    pub fn end_of_drive(&mut self, now_ms: f64) {
        self.arrived_ms.get_or_insert(now_ms);
    }

    pub fn reroute_failed(&mut self, now_ms: f64) {
        self.reroute.answered(false, now_ms);
    }

    #[must_use]
    pub fn trip_state(&self) -> TripState {
        self.state.trip_state()
    }

    #[must_use]
    pub fn route(&self) -> &NavRoute {
        &self.route
    }

    /// Bumped by every `replace_route`, so a drawn route line knows to rebuild.
    #[must_use]
    pub fn route_generation(&self) -> u64 {
        self.route_generation
    }

    /// Index into `route().steps` of the step being driven.
    #[must_use]
    pub fn step_index(&self) -> usize {
        match self.state.trip_state() {
            TripState::Navigating { remaining_steps, .. } => self.route.steps.len().saturating_sub(remaining_steps.len()),
            _ => self.route.steps.len().saturating_sub(1),
        }
    }

    #[must_use]
    pub fn elapsed_ms(&self, now_ms: f64) -> f64 {
        (self.arrived_ms.unwrap_or(now_ms) - self.started_ms).max(0.0)
    }

    #[must_use]
    pub fn arrived(&self) -> bool {
        self.arrived_ms.is_some()
    }

    #[must_use]
    pub fn reroute_in_flight(&self) -> bool {
        self.reroute.in_flight()
    }

    #[must_use]
    pub fn off_route(&self) -> bool {
        self.reroute.off_route()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off(by: f64) -> RouteDeviation {
        RouteDeviation::Deviation { kind: DeviationKind::CompletelyOffRoute { deviation_from_route_line: by } }
    }

    #[test]
    fn a_request_goes_out_only_after_enough_distance_for_enough_time() {
        let mut reroute = Reroute::new(ReroutePolicy::default());
        assert!(!reroute.observe(off(50.0), 0.0), "the first off-route update starts the clock");
        assert!(!reroute.observe(off(50.0), 2_999.0), "2.999 s is not 3 s");
        assert!(reroute.observe(off(50.0), 3_000.0));
        assert!(reroute.in_flight());
    }

    #[test]
    fn a_short_excursion_or_a_near_miss_never_reroutes() {
        let mut reroute = Reroute::new(ReroutePolicy::default());
        assert!(!reroute.observe(off(25.0), 0.0));
        assert!(!reroute.observe(off(29.9), 10_000.0), "long enough but never 30 m off");
        assert!(!reroute.observe(RouteDeviation::NoDeviation, 10_500.0));
        assert!(!reroute.off_route(), "back on the route clears it");
        assert!(!reroute.observe(off(80.0), 11_000.0), "a fresh excursion restarts the clock");
        assert!(!reroute.observe(off(80.0), 13_999.0));
        let on_step = RouteDeviation::Deviation { kind: DeviationKind::OffStepOnRoute { deviation_from_step_line: 90.0 } };
        assert!(!reroute.observe(on_step, 20_000.0), "on a later step of the route is not off it");
    }

    #[test]
    fn only_one_request_is_in_flight_and_a_failure_waits_before_the_next() {
        let mut reroute = Reroute::new(ReroutePolicy::default());
        reroute.observe(off(40.0), 0.0);
        assert!(reroute.observe(off(40.0), 3_000.0));
        for t in [3_500.0, 10_000.0, 60_000.0] {
            assert!(!reroute.observe(off(40.0), t), "still in flight at {t}");
        }
        reroute.answered(false, 60_000.0);
        assert!(!reroute.observe(off(40.0), 64_999.0), "the pause after a failure");
        assert!(reroute.observe(off(40.0), 65_000.0));
        reroute.answered(true, 66_000.0);
        assert!(!reroute.off_route());
    }
}
