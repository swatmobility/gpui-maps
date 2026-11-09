//! `NavHost`: the one object a shell holds for navigation, and the whole of
//! the surface it calls. A shell turns its own events into these calls and
//! its own HTTP into [`NavHost::route_fetched`]; every decision -- what a tap
//! on the chrome does, when to reroute, where the camera goes, what the
//! banner says -- is made here, which is what
//! `tests/navigation_is_computed_in_nav.rs` holds every shell to.

use crate::camera_control::CameraController;
use crate::collision::CollisionBox;
use crate::nav::chrome::{self, ChromeAction, ChromeGlyph, ChromeHit, ChromeIcon, PUCK_SIZE};
use crate::nav::route_layer::puck_placement;
use crate::nav::route_source::{describe_fetch_error, parse_route, request_url, RouteError, UrlOptions};
use crate::nav::trip::{NavTrip, TripClock};
pub use crate::nav::view_model::{ManeuverGlyph, ManeuverView, SimView, TripPhase};

// Every type a shell names crosses this module, so a shell's only `nav::`
// path is `nav::host` (`tests/navigation_is_computed_in_nav.rs`). These are
// the same types, not copies: re-exported, never wrapped.
pub use crate::nav::chrome::{hud_rects, ChromeScene, HudRects, Rect, SafeInsets};
pub use crate::nav::config::{RoutingConfig, CONFIG_FILE_RELATIVE};
pub use crate::nav::demo::{is_destination_click, valid_route_name, DemoRequest, DEFAULT_ROUTE};
pub use crate::nav::follow::{ChromeClearance, FollowCamera, NavCameraMode};
pub use crate::nav::format::WallClock;
pub use crate::nav::route_layer::RouteLayer;
pub use crate::nav::route_source::RouteRequest;
pub use crate::nav::simulate::SimOptions;
pub use crate::nav::view_model::{CameraMode, NavigationState, Puck};
pub use crate::nav::voice::{pick_voice, pick_voice_for, Utterance, VoiceCommand, VoiceInfo};
use crate::nav::voice::{VoiceAnnouncer, VoiceView};
use crate::render::Camera;
use std::sync::Arc;

/// How long a notice stays up.
pub const NOTICE_MS: f64 = 4_000.0;
/// A replayed reroute (no live routing) "arrives" after this long, so the
/// Rerouting state is visible in a demo.
pub const REPLAY_REROUTE_MS: f64 = 600.0;
/// A route request the shell has not answered by then is treated as failed,
/// so a fetch that never returns cannot hold a trip in "Rerouting..." or the
/// map in "Finding a route..." for good.
/// A shell's transport timeout must be shorter (native's reqwest and
/// `web/serve.py` both use 15 s), so an answer never lands after the host
/// has already given up and said so.
pub const REQUEST_TIMEOUT_MS: f64 = 20_000.0;
/// The OSRM-style `code` a routing proxy answers when it holds no key
/// (`web/serve.py`'s 503): live routing is off, not a failed route.
pub const PROXY_NOT_CONFIGURED: &str = "NoKey";

/// One frame's inputs from the shell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavFrame {
    /// The shell's monotonic clock, ms.
    pub now_ms: f64,
    pub wall: WallClock,
    /// Logical viewport size.
    pub viewport: (f32, f32),
    pub insets: SafeInsets,
}

impl NavFrame {
    fn clock(&self) -> TripClock {
        TripClock { now_ms: self.now_ms, wall: self.wall }
    }
}

/// What the shell must do after [`NavHost::advance`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NavTick {
    /// Ask for another frame.
    pub redraw: bool,
    /// Fetch this and hand the bytes to [`NavHost::route_fetched`].
    pub fetch: Option<RouteRequest>,
}

/// A key press as the shell saw it, for [`NavHost::key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyPress<'a> {
    /// The key's character, as the browser's `KeyboardEvent.key` spells it
    /// (`"g"`, `"G"`, `"p"`).
    pub key: &'a str,
    pub shift: bool,
    /// Ctrl, Cmd or Alt held: the browser's shortcut, never navigation's.
    pub other_modifier: bool,
    /// An auto-repeat from a held key.
    pub repeat: bool,
    /// The event target's tag (`"INPUT"`, `"TEXTAREA"`, ...), if any.
    pub target_tag: Option<&'a str>,
    /// The event target is `contenteditable`.
    pub target_editable: bool,
}

impl KeyPress<'_> {
    /// Typed into a field: the letter is text, not a command.
    #[must_use]
    pub fn typing(&self) -> bool {
        self.target_editable
            || self.target_tag.is_some_and(|t| ["INPUT", "TEXTAREA", "SELECT"].iter().any(|f| t.eq_ignore_ascii_case(f)))
    }
}

/// A request in flight: its id, and when it is given up on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pending {
    id: u64,
    deadline_ms: f64,
}

struct ActiveTrip {
    trip: NavTrip,
    follow: FollowCamera,
    layer: RouteLayer,
}

#[derive(Default)]
pub struct NavHost {
    active: Option<ActiveTrip>,
    demo_route: Option<Arc<[u8]>>,
    sim_options: SimOptions,
    routing: Option<RoutingConfig>,
    routing_off_reason: Option<String>,
    show_simulate: bool,
    /// A demo trip was ended (End, or Done on arrival): Simulate is offered
    /// again, whatever the launch mode, so a `?nav=sim` / `--nav-sim` drive
    /// is not the only one a session gets.
    offer_restart: bool,
    notice: Option<(String, f64)>,
    next_id: u64,
    /// A destination the user picked, and the reroute the trip asked for:
    /// tracked apart, so neither can strand the other. A new destination
    /// supersedes an older destination, never the trip's reroute.
    pending_trip: Option<Pending>,
    pending_reroute: Option<Pending>,
    replay_due_ms: Option<f64>,
    /// What to say and when; its mute outlives a trip.
    voice: VoiceAnnouncer,
    /// For the shell's speaker: the latest command not yet taken.
    voice_out: Option<VoiceCommand>,
    /// A `Speak` has gone out since the last `Stop`: something may be playing.
    voice_playing: bool,
    /// How the camera frames a trip; outlives a trip, so the next one
    /// starts in it.
    camera_mode: NavCameraMode,
    /// The follow zoom offset (the user's own zoom while following);
    /// outlives a trip too.
    zoom_offset: f64,
    /// The map is off screen (`set_suspended`): no drive, no voice.
    suspended: bool,
    /// The shell's own controls over the map (`set_host_chrome`), kept
    /// clear by the overview and the follow puck like the HUD's panels.
    host_chrome: Vec<Rect>,
    /// The suspension paused the simulated drive, so resuming unpauses it
    /// -- and a drive the user had paused stays paused.
    paused_by_suspend: bool,
    /// The shell draws the whole HUD itself, natively (iOS Studio's
    /// SwiftUI banner and strip, read from [`Self::state`]):
    /// [`Self::scene`] is the puck and the maneuver arrow's head only, a
    /// tap is never the chrome's, and the follow camera clears the
    /// shell's [`Self::set_host_chrome`] rects alone. See
    /// [`Self::set_hud_by_host`].
    hud_by_host: bool,
}

impl NavHost {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The route a simulated drive (G, `?nav=sim`, Simulate) plays, and how.
    /// With no live routing, a reroute is answered by this same route.
    pub fn set_demo_route(&mut self, bytes: Arc<[u8]>, options: SimOptions) {
        self.demo_route = Some(bytes);
        self.sim_options = options;
    }

    /// Live routing: a config, or why there is none (shown when asked for).
    pub fn set_routing(&mut self, routing: Result<RoutingConfig, String>) {
        match routing {
            Ok(config) => {
                self.routing = Some(config);
                self.routing_off_reason = None;
            }
            Err(reason) => {
                self.routing = None;
                self.routing_off_reason = Some(reason);
            }
        }
    }

    pub fn set_show_simulate(&mut self, show: bool) {
        self.show_simulate = show;
    }

    /// Anything to draw or drive at all. `false` means the shell may skip
    /// every navigation call this frame, and its frame is byte-for-byte the
    /// frame it drew before navigation existed.
    #[must_use]
    pub fn is_active(&self) -> bool {
        // A pending destination request always carries its notice; it is
        // named too so its deadline is seen even with no trip running.
        self.active.is_some() || self.notice.is_some() || self.simulate_offered() || self.pending_trip.is_some()
    }

    /// The idle chrome shows Simulate: the shell asked for it, or a demo
    /// trip has ended and the same drive can be started again.
    fn simulate_offered(&self) -> bool {
        self.show_simulate || (self.offer_restart && self.demo_route.is_some())
    }

    #[must_use]
    pub fn trip_active(&self) -> bool {
        self.active.is_some()
    }

    #[must_use]
    pub fn state(&self) -> Option<&NavigationState> {
        self.active.as_ref().map(|a| a.trip.state())
    }

    #[must_use]
    pub fn route_layer(&self) -> Option<&RouteLayer> {
        self.active.as_ref().map(|a| &a.layer)
    }

    /// Start the demo drive. A notice explains any failure.
    pub fn start_simulation(&mut self, camera: &CameraController, frame: &NavFrame) -> Result<(), String> {
        let result = self
            .demo_route
            .clone()
            .ok_or_else(|| "no demo route is loaded".to_string())
            .and_then(|bytes| parse_route(&bytes).map_err(|e| e.to_string()))
            .and_then(|route| self.begin(route, camera, frame));
        if let Err(reason) = &result {
            self.notify(format!("Could not start the drive: {reason}"), frame.now_ms);
        }
        result
    }

    fn begin(&mut self, route: crate::nav::route_source::NavRoute, camera: &CameraController, frame: &NavFrame) -> Result<(), String> {
        let layer = RouteLayer::new(&route, 0);
        let trip = NavTrip::simulated(route, self.sim_options, frame.clock())?;
        if self.active.is_some() {
            // A trip replaced mid-prompt: the old trip's prompt stops.
            self.stop_voice();
        }
        let mut follow = FollowCamera::with_mode(self.camera_mode, camera, frame.now_ms);
        follow.set_zoom_offset(self.zoom_offset, frame.now_ms);
        self.active = Some(ActiveTrip { trip, follow, layer });
        self.voice.reset();
        if let Some(active) = self.active.as_mut() {
            active.trip.set_voice_muted(self.voice.muted());
        }
        self.replay_due_ms = None;
        // The old trip's reroute, if any, answers a trip that is gone.
        self.pending_reroute = None;
        if self.suspended {
            self.pause_for_suspension();
        }
        Ok(())
    }

    /// The map went off screen (`true`: another page covers it, the app
    /// hid it) or came back. Off screen, a simulated drive pauses, a
    /// prompt in progress stops and nothing more is said; back on screen,
    /// a drive this paused resumes (one the user paused stays paused). A
    /// live trip keeps tracking, silently. The shell's own page switch
    /// calls this -- the host cannot see whether its map is visible.
    pub fn set_suspended(&mut self, suspended: bool) {
        if suspended == self.suspended {
            return;
        }
        self.suspended = suspended;
        if suspended {
            self.stop_voice();
            self.pause_for_suspension();
        } else if std::mem::take(&mut self.paused_by_suspend) {
            if let Some(active) = self.active.as_mut() {
                if active.trip.drive().is_some_and(|d| d.is_paused()) {
                    active.trip.toggle_pause();
                }
            }
        }
    }

    /// The shell's own controls drawn over the map, logical points -- a
    /// pill of buttons beside the HUD, say. The overview fits the route
    /// clear of them and the follow puck stays above them, exactly as for
    /// the HUD's own panels: one above the middle clears down to its
    /// bottom, any other up to its top. Replaces the last set; empty for
    /// none.
    pub fn set_host_chrome(&mut self, rects: Vec<Rect>) {
        self.host_chrome = rects;
    }

    #[must_use]
    pub fn host_chrome(&self) -> &[Rect] {
        &self.host_chrome
    }

    /// The shell draws the banner, the ETA strip, the sim tray, the
    /// arrival card and the notice itself, natively, from [`Self::state`]
    /// and [`Self::notice`] -- and registers what it draws with
    /// [`Self::set_host_chrome`], which then is the ONLY chrome the follow
    /// puck and the overview keep clear of. The engine still draws the
    /// route, the puck and the maneuver arrow. Every decision (the drive,
    /// the camera, the voice) stays here.
    pub fn set_hud_by_host(&mut self, on: bool) {
        self.hud_by_host = on;
    }

    #[must_use]
    pub fn hud_by_host(&self) -> bool {
        self.hud_by_host
    }

    /// The notice on show ("Could not start the drive: ..."), for a shell
    /// that draws the HUD itself ([`Self::set_hud_by_host`]).
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_ref().map(|(text, _)| text.as_str())
    }

    /// Where the puck is on screen this frame, logical points: what a
    /// shell's own chrome must never cover.
    #[must_use]
    pub fn puck_on_screen(&self, camera: &Camera) -> Option<(f32, f32)> {
        let state = self.active.as_ref()?.trip.state();
        state.puck.as_ref().and_then(|p| puck_placement(camera, p)).map(|placed| (placed.x, placed.y))
    }

    /// Whether [`Self::set_suspended`] has the trip off screen.
    #[must_use]
    pub fn suspended(&self) -> bool {
        self.suspended
    }

    /// The simulated drive is held by the user (Pause, or the strip's
    /// Stop), not by [`Self::set_suspended`]: what a shell keeps when it
    /// drops the page and starts the drive again from where it stood.
    #[must_use]
    pub fn drive_paused_by_user(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.trip.drive().is_some_and(|d| d.is_paused())) && !self.paused_by_suspend
    }

    fn pause_for_suspension(&mut self) {
        if let Some(active) = self.active.as_mut() {
            if active.trip.drive().is_some_and(|d| !d.is_paused()) {
                active.trip.toggle_pause();
                self.paused_by_suspend = true;
            }
        }
    }

    /// How the camera frames the trip: [`NavCameraMode::Follow3d`] (the
    /// default), [`NavCameraMode::Follow2d`], [`NavCameraMode::NorthUp2d`]
    /// or [`NavCameraMode::Overview`]. A trip in progress glides into it --
    /// taking the camera back from a gesture if one had it -- and the next
    /// trip starts in it. The chrome's Overview and Recenter buttons still
    /// switch between the overview and the last follow mode chosen.
    pub fn set_nav_camera_mode(&mut self, mode: NavCameraMode, camera: &CameraController, frame: &NavFrame) {
        self.camera_mode = mode;
        if let Some(active) = self.active.as_mut() {
            active.follow.set_nav_camera_mode(mode, camera, frame.now_ms);
            active.trip.set_camera_mode(active.follow.mode());
        }
    }

    /// The mode chosen by [`Self::set_nav_camera_mode`] or the chrome's
    /// Overview / Recenter. While a gesture holds the camera, this is still
    /// the mode following resumes into; `NavigationState::camera_mode` says
    /// `Free`.
    #[must_use]
    pub fn nav_camera_mode(&self) -> NavCameraMode {
        self.active.as_ref().map_or(self.camera_mode, |a| a.follow.nav_camera_mode())
    }

    /// The follow camera's zoom offset, zoom levels (+1 is half the
    /// altitude), clamped to `follow::FOLLOW_ZOOM_OFFSET_MIN..MAX`: the
    /// speed-and-maneuver zoom of every follow mode moves by it. A wheel,
    /// trackpad or pinch zoom while following sets it as well (and keeps
    /// following). A trip in progress eases to it when following; the next
    /// trip starts at it.
    pub fn set_nav_follow_zoom_offset(&mut self, offset: f64, frame: &NavFrame) {
        if !offset.is_finite() {
            return;
        }
        self.zoom_offset = offset.clamp(crate::nav::follow::FOLLOW_ZOOM_OFFSET_MIN, crate::nav::follow::FOLLOW_ZOOM_OFFSET_MAX);
        if let Some(active) = self.active.as_mut() {
            active.follow.set_zoom_offset(self.zoom_offset, frame.now_ms);
        }
    }

    /// See [`Self::set_nav_follow_zoom_offset`]: the user's last zoom while
    /// following, or the value set.
    #[must_use]
    pub fn nav_follow_zoom_offset(&self) -> f64 {
        self.active.as_ref().map_or(self.zoom_offset, |a| a.follow.zoom_offset())
    }

    /// `G`: start the demo drive, or end the trip in progress.
    pub fn toggle_simulation(&mut self, camera: &CameraController, frame: &NavFrame) {
        if self.active.is_some() {
            self.end_trip();
        } else {
            let _ = self.start_simulation(camera, frame);
        }
    }

    pub fn end_trip(&mut self) {
        if self.active.take().is_some() {
            self.stop_voice();
            self.offer_restart = self.demo_route.is_some();
        }
        self.voice.reset();
        self.paused_by_suspend = false;
        self.pending_trip = None;
        self.pending_reroute = None;
        self.replay_due_ms = None;
    }

    /// Puts the demo away: the trip in progress ends, and nothing offers to
    /// start it again (no Simulate in the idle chrome). For a shell that
    /// drives one map through several demos and is leaving this one.
    pub fn clear_demo(&mut self) {
        self.end_trip();
        self.demo_route = None;
        self.show_simulate = false;
        self.offer_restart = false;
    }

    /// The strip's stop button. A simulated drive STOPS in place -- paused,
    /// the strip and everything on it kept, the stop button turned into
    /// Play, which starts the drive again (the user: "why can the red stop
    /// button not just become a play button?"). A live trip, and Done on
    /// arrival, end the trip as [`Self::end_trip`] does.
    pub fn stop_or_end(&mut self) {
        let stoppable = self.demo_route.is_some()
            && self.active.as_ref().is_some_and(|a| a.trip.drive().is_some() && a.trip.state().phase != TripPhase::Arrived);
        if !stoppable {
            self.end_trip();
            return;
        }
        self.stop_voice();
        if let Some(active) = self.active.as_mut() {
            active.trip.stop();
        }
        self.paused_by_suspend = false;
    }

    /// A simulated drive [`Self::stop_or_end`] stopped.
    #[must_use]
    pub fn drive_stopped(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.trip.stopped())
    }

    /// 1x -> 2x -> 4x -> 8x -> 1x.
    /// Also the chrome's speed chip. Nothing for a live trip.
    pub fn cycle_speed(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.trip.cycle_speed();
        }
    }

    /// `P`, and the chrome's pause toggle. Nothing for a live trip.
    pub fn toggle_pause(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.trip.toggle_pause();
        }
    }

    /// A key press, for a shell with no navigation bindings of its own (the
    /// browser): native's `G` / `Shift+G` / `P`. `true` when it was taken.
    /// A key typed into a text field, or held with Ctrl/Cmd/Alt, is never
    /// taken; nor is `G` on a page with no demo route and no trip, so a
    /// page without `?nav=` behaves as it did before navigation existed.
    pub fn key(&mut self, press: &KeyPress<'_>, camera: &CameraController, frame: &NavFrame) -> bool {
        if press.other_modifier || press.repeat || press.typing() {
            return false;
        }
        let simulated = self.active.as_ref().is_some_and(|a| a.trip.drive().is_some());
        match (press.key.to_ascii_lowercase().as_str(), press.shift) {
            ("g", false) if self.active.is_some() || self.demo_route.is_some() => self.toggle_simulation(camera, frame),
            ("g", true) if simulated => self.cycle_speed(),
            ("p", false) if simulated => self.toggle_pause(),
            _ => return false,
        }
        true
    }

    /// A destination was picked (right-click, long-press). With live routing
    /// this is the request to make; without, a notice says why not.
    pub fn request_route(&mut self, from: (f64, f64), to: (f64, f64), frame: &NavFrame) -> Option<RouteRequest> {
        let Some(config) = &self.routing else {
            let reason = self.routing_off_reason.clone().unwrap_or_else(|| "live routing is off".to_string());
            self.notify(reason, frame.now_ms);
            return None;
        };
        self.next_id += 1;
        let waypoints = vec![from, to];
        let request = RouteRequest { id: self.next_id, url: request_url(config, &waypoints, &UrlOptions::navigation()), waypoints };
        self.pending_trip = Some(Pending { id: request.id, deadline_ms: frame.now_ms + REQUEST_TIMEOUT_MS });
        self.notify("Finding a route\u{2026}".to_string(), frame.now_ms);
        Some(request)
    }

    /// A destination picked at screen point `(x, y)` (logical px) -- a
    /// right-click, a still right press, a long-press: from the camera
    /// centre to the ground under the point, through [`Self::request_route`].
    /// A point off the ground (above the horizon) picks nothing and says
    /// nothing. The one place a shell's click becomes a route, so native and
    /// the browser cannot unproject it two ways.
    ///
    /// Picked on the frame as DRAWN (`globe_enabled`: the map's globe
    /// switch), so on the z<12 globe the destination is the place under
    /// the pointer, not the flat plane's point 40-450 px away.
    pub fn request_route_at(
        &mut self,
        camera: &Camera,
        globe_enabled: bool,
        x: f32,
        y: f32,
        frame: &NavFrame,
    ) -> Option<RouteRequest> {
        let world = crate::render::globe::unproject_to_world_on_globe(camera, globe_enabled, x, y)?;
        let to = crate::camera_control::lon_lat_at_world(world.extend(0.0))?;
        self.request_route((camera.center_lon, camera.center_lat), to, frame)
    }

    /// The shell's answer to a [`RouteRequest`]: the body, or an error
    /// string (which is redacted before it is shown). An answer to anything
    /// but the latest request is dropped.
    ///
    /// A proxy that answers [`PROXY_NOT_CONFIGURED`] (`web/serve.py` with no
    /// key) turns live routing off, exactly as a missing key does natively:
    /// see [`Self::routing_unavailable`].
    pub fn route_fetched(&mut self, id: u64, result: Result<Vec<u8>, String>, camera: &CameraController, frame: &NavFrame) {
        if let Ok(bytes) = &result {
            if let Err(RouteError::Service { code, message }) = parse_route(bytes) {
                if code == PROXY_NOT_CONFIGURED {
                    let reason = message.unwrap_or_else(|| "the routing proxy has no key".to_string());
                    return self.routing_unavailable(id, &reason, frame);
                }
            }
        }
        let parse = |result: Result<Vec<u8>, String>| {
            result.map_err(|e| describe_fetch_error(&e)).and_then(|bytes| parse_route(&bytes).map_err(|e| e.to_string()))
        };
        if self.pending_trip.is_some_and(|p| p.id == id) {
            self.pending_trip = None;
            match parse(result) {
                Ok(route) => {
                    self.notice = None;
                    if let Err(reason) = self.begin(route, camera, frame) {
                        self.notify(format!("Could not start the trip: {reason}"), frame.now_ms);
                    }
                }
                Err(reason) => self.notify(format!("No route: {reason}"), frame.now_ms),
            }
        } else if self.pending_reroute.is_some_and(|p| p.id == id) {
            self.pending_reroute = None;
            self.finish_reroute(parse(result), frame);
        }
    }

    /// The answer to request `id` is that live routing is not configured
    /// after all: the browser's proxy has no key (`web/serve.py`'s
    /// [`PROXY_NOT_CONFIGURED`]), or the page is not served by a proxy at
    /// all. From here on the host behaves as a keyless native one: a
    /// destination explains why not, and a demo drive's reroute replays the
    /// demo route -- this one included, so `?nav=sim&offroute=` completes
    /// offline. `reason` names a variable, never a value.
    pub fn routing_unavailable(&mut self, id: u64, reason: &str, frame: &NavFrame) {
        let reason = describe_fetch_error(&format!("live routing is off: {reason}"));
        let was_trip = self.pending_trip.is_some_and(|p| p.id == id);
        let was_reroute = self.pending_reroute.is_some_and(|p| p.id == id);
        self.set_routing(Err(reason.clone()));
        if was_trip {
            self.pending_trip = None;
            self.notify(reason, frame.now_ms);
        } else if was_reroute {
            self.pending_reroute = None;
            if self.demo_route.is_some() {
                self.replay_due_ms = Some(frame.now_ms + REPLAY_REROUTE_MS);
            } else {
                self.finish_reroute(Err(reason), frame);
            }
        }
    }

    /// Hands a reroute's outcome to the trip, which leaves "Rerouting..."
    /// either way.
    fn finish_reroute(&mut self, result: Result<crate::nav::route_source::NavRoute, String>, frame: &NavFrame) {
        if let Err(reason) = &result {
            self.notify(format!("Reroute failed: {reason}"), frame.now_ms);
        }
        if let Some(active) = self.active.as_mut() {
            active.trip.reroute_arrived(result, frame.clock());
        }
    }

    /// One frame of navigation: the drive, the reroute policy, the route
    /// line and the camera. Call it before the camera's own `advance`.
    pub fn advance(&mut self, camera: &mut CameraController, frame: &NavFrame) -> NavTick {
        let mut tick = NavTick::default();
        // "Finding a route..." stays up for as long as the request is out
        // (its deadline ends it); any other notice expires on time.
        if self.notice.as_ref().is_some_and(|(_, until)| frame.now_ms >= *until) && self.pending_trip.is_none() {
            self.notice = None;
        }
        if self.pending_trip.is_some_and(|p| frame.now_ms >= p.deadline_ms) {
            self.pending_trip = None;
            self.notify("No route: the routing service did not answer".to_string(), frame.now_ms);
        }
        if self.pending_reroute.is_some_and(|p| frame.now_ms >= p.deadline_ms) {
            self.pending_reroute = None;
            self.finish_reroute(Err("the routing service did not answer".to_string()), frame);
        }
        // While a request is out, keep frames coming so its deadline is seen.
        let waiting = self.pending_trip.is_some() || self.pending_reroute.is_some();
        let Some(active) = self.active.as_mut() else {
            tick.redraw = self.notice.is_some() || waiting;
            return tick;
        };
        let trip_tick = active.trip.advance(frame.clock());
        if trip_tick.reroute.is_some() {
            // The prompt playing is for a route being replaced.
            self.voice_out = Some(VoiceCommand::Stop);
            self.voice_playing = false;
        }
        if let Some(need) = trip_tick.reroute {
            if let Some(config) = &self.routing {
                self.next_id += 1;
                let waypoints = vec![need.origin, need.destination];
                let request = RouteRequest { id: self.next_id, url: request_url(config, &waypoints, &UrlOptions::navigation()), waypoints };
                self.pending_reroute = Some(Pending { id: request.id, deadline_ms: frame.now_ms + REQUEST_TIMEOUT_MS });
                tick.fetch = Some(request);
            } else if self.demo_route.is_some() {
                self.replay_due_ms = Some(frame.now_ms + REPLAY_REROUTE_MS);
            } else {
                active.trip.reroute_arrived(Err("live routing is off".to_string()), frame.clock());
            }
        }
        if self.replay_due_ms.is_some_and(|due| frame.now_ms >= due) {
            self.replay_due_ms = None;
            let route = self.demo_route.as_ref().ok_or_else(|| "no demo route".to_string()).and_then(|b| parse_route(b).map_err(|e| e.to_string()));
            active.trip.reroute_arrived(route, frame.clock());
        }
        let session = active.trip.session();
        active.layer.update(session.route(), session.route_generation(), active.trip.state());
        let clearance = if self.hud_by_host {
            chrome::clearance_of(self.host_chrome.iter().copied(), frame.viewport.1)
        } else {
            let scene = chrome::trip_scene(active.trip.state(), frame.viewport, frame.insets);
            chrome::clearance_of(scene.plates.iter().map(|p| p.rect).chain(self.host_chrome.iter().copied()), frame.viewport.1)
        };
        // A turn worth zooming in for: anything but "keep going".
        let state = active.trip.state();
        let turn_m = state.maneuver.as_ref().filter(|m| m.glyph != ManeuverGlyph::Straight).map(|_| state.distance_to_maneuver_m);
        active.follow.set_motion(active.trip.speed_mps(), turn_m);
        let gliding =
            active.follow.drive(camera, active.trip.state().puck, active.layer.bounds_world(), clearance, frame.now_ms);
        active.trip.set_camera_mode(active.follow.mode());
        self.zoom_offset = active.follow.zoom_offset();
        let utterance = if self.suspended { None } else { self.voice.update(&VoiceView::of(&active.trip)) };
        if let Some(utterance) = utterance {
            // A newer prompt replaces anything not taken yet, a Stop
            // included: a Speak interrupts whatever is playing anyway.
            self.voice_out = Some(VoiceCommand::Speak(utterance));
            self.voice_playing = true;
        }
        tick.redraw = trip_tick.redraw || gliding || waiting || self.replay_due_ms.is_some() || self.notice.is_some();
        tick
    }

    /// This frame's chrome, puck included. Empty when there is nothing to show.
    #[must_use]
    pub fn scene(&self, camera: &Camera, frame: &NavFrame) -> ChromeScene {
        let mut scene = match &self.active {
            Some(active) => {
                let state = active.trip.state();
                let mut scene = if self.hud_by_host {
                    ChromeScene::default()
                } else {
                    chrome::trip_scene(state, frame.viewport, frame.insets)
                };
                if let Some(placed) = state.puck.as_ref().and_then(|p| puck_placement(camera, p)) {
                    scene.icons.insert(
                        0,
                        ChromeIcon {
                            rect: Rect { x: placed.x - PUCK_SIZE / 2.0, y: placed.y - PUCK_SIZE / 2.0, w: PUCK_SIZE, h: PUCK_SIZE },
                            glyph: ChromeGlyph::Puck { muted: placed.muted },
                            rotation_rad: placed.rotation_rad,
                        },
                    );
                }
                // The maneuver arrow's head, under the puck and never over
                // a panel (icons draw after every plate).
                if let Some(head) = active.layer.arrow_head_placement(camera) {
                    let side = chrome::ARROW_HEAD_SIZE;
                    let rect = Rect { x: head.x - side / 2.0, y: head.y - side / 2.0, w: side, h: side };
                    let under_a_panel = scene.plates.iter().map(|p| p.rect).chain(self.host_chrome.iter().copied()).any(|p| {
                        rect.x < p.x + p.w && p.x < rect.x + rect.w && rect.y < p.bottom() && p.y < rect.bottom()
                    });
                    if !under_a_panel {
                        scene.icons.insert(0, ChromeIcon { rect, glyph: ChromeGlyph::ArrowHead, rotation_rad: head.rotation_rad });
                    }
                }
                scene
            }
            None if self.hud_by_host => ChromeScene::default(),
            None => chrome::idle_scene(self.simulate_offered(), frame.viewport, frame.insets),
        };
        if let (Some((text, _)), false) = (&self.notice, self.hud_by_host) {
            chrome::push_notice(&mut scene, text, frame.viewport, frame.insets);
        }
        scene
    }

    /// Physical-pixel boxes to put in the label collision index before
    /// placement, so no label is drawn over the route.
    #[must_use]
    pub fn collision_boxes(&self, camera: &Camera, scale_factor: f32) -> Vec<CollisionBox> {
        self.active.as_ref().map_or_else(Vec::new, |a| a.layer.collision_boxes(camera, scale_factor))
    }

    /// A press at `(x, y)` (logical points). `true` when the chrome took it,
    /// in which case the shell must not also start a map gesture.
    pub fn tap(&mut self, x: f32, y: f32, camera: &mut CameraController, frame: &NavFrame) -> bool {
        let scene = self.scene(camera.camera(), frame);
        match chrome::hit(&scene, x, y) {
            None => false,
            Some(ChromeHit::Surface) => true,
            Some(ChromeHit::Action(action)) => {
                match action {
                    ChromeAction::Recenter => {
                        if let Some(active) = self.active.as_mut() {
                            active.follow.recenter(camera, frame.now_ms);
                            self.camera_mode = active.follow.nav_camera_mode();
                        }
                    }
                    ChromeAction::Overview => {
                        if let Some(active) = self.active.as_mut() {
                            active.follow.overview(camera, frame.now_ms);
                            self.camera_mode = active.follow.nav_camera_mode();
                        }
                    }
                    ChromeAction::EndTrip => self.stop_or_end(),
                    ChromeAction::Simulate => {
                        let _ = self.start_simulation(camera, frame);
                    }
                    ChromeAction::ToggleVoice => {
                        let muted = self.voice_muted();
                        self.set_voice_muted(!muted);
                    }
                    // A stopped drive's paused tray plays it again, as the
                    // Play in the strip does.
                    ChromeAction::TogglePause if self.drive_stopped() => {
                        let _ = self.start_simulation(camera, frame);
                    }
                    ChromeAction::TogglePause => self.toggle_pause(),
                    ChromeAction::CycleSpeed => self.cycle_speed(),
                }
                true
            }
        }
    }

    /// What the platform's speaker must do, once: call it every frame
    /// after [`Self::advance`] (and after a tap). `Speak` says the next prompt
    /// (after the one playing, or interrupting it: see `speech::speak_queues`);
    /// `Stop` silences everything --
    /// sent when mute turns on, when the trip ends or is dismissed, and when
    /// a reroute starts. `None` almost always.
    pub fn take_voice(&mut self) -> Option<VoiceCommand> {
        self.voice_out.take()
    }

    /// A `Stop`, if anything may be playing; it replaces an untaken `Speak`.
    fn stop_voice(&mut self) {
        // An untaken Speak set `voice_playing` too, so it is replaced here.
        if self.voice_playing {
            self.voice_out = Some(VoiceCommand::Stop);
            self.voice_playing = false;
        }
    }

    /// Mute or unmute voice guidance, for this trip and the next. Muting
    /// stops a prompt in progress.
    pub fn set_voice_muted(&mut self, muted: bool) {
        if muted && !self.voice.muted() {
            self.stop_voice();
        }
        self.voice.set_muted(muted);
        if let Some(active) = self.active.as_mut() {
            active.trip.set_voice_muted(muted);
        }
    }

    #[must_use]
    pub fn voice_muted(&self) -> bool {
        self.voice.muted()
    }

    fn notify(&mut self, text: String, now_ms: f64) {
        self.notice = Some((text, now_ms + NOTICE_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_control::{CameraIntent, MAX_ZOOM, MIN_ZOOM};
    use crate::nav::config::RoutingKey;
    use crate::nav::view_model::{CameraMode, TripPhase};

    const MANILA: &[u8] = include_bytes!("../../tests/fixtures/routes/manila.json");

    fn frame(now_ms: f64) -> NavFrame {
        NavFrame {
            now_ms,
            wall: WallClock { unix_ms: 1_773_755_400_000.0 + now_ms, utc_offset_min: 480 },
            viewport: (800.0, 600.0),
            insets: SafeInsets::default(),
        }
    }

    fn camera() -> CameraController {
        CameraController::new(
            Camera { center_lon: 120.98, center_lat: 14.59, zoom: 13.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 800.0, viewport_height_px: 600.0, fov_y: Camera::DEFAULT_FOV_Y },
            MIN_ZOOM,
            MAX_ZOOM,
        )
    }

    fn demo_host(options: SimOptions) -> NavHost {
        let mut host = NavHost::new();
        host.set_demo_route(Arc::from(MANILA), options);
        host
    }

    #[test]
    fn a_shell_that_names_only_nav_host_can_launch_a_demo_drive() {
        // Everything a native shell spells: the request from its launch
        // arguments, the route name default, the options, and the frame
        // types -- all through `nav::host` (the parity gate allows no other
        // `nav::` path in a shell).
        let args: Vec<String> = ["demo", "--nav-sim", "--nav-speed", "4", "--nav-paused"].iter().map(ToString::to_string).collect();
        let request: super::DemoRequest = super::DemoRequest::from_args(&args).expect("--nav-sim asks for a drive");
        assert_eq!(request.route, super::DEFAULT_ROUTE);
        assert!(request.autostart);
        assert_eq!(request.options, super::SimOptions { speed: 4, paused: true, off_route_after_step: None, start_at_m: None });
        let mut host = demo_host(request.options);
        let mut camera = camera();
        let f = super::NavFrame {
            now_ms: 0.0,
            wall: super::WallClock { unix_ms: 1_773_755_400_000.0, utc_offset_min: 480 },
            viewport: (800.0, 600.0),
            insets: super::SafeInsets::default(),
        };
        host.start_simulation(&camera, &f).expect("the Manila fixture drives");
        let _ = host.advance(&mut camera, &f);
        let state: Option<&super::NavigationState> = host.state();
        assert!(state.is_some());
        let scene: super::ChromeScene = host.scene(camera.camera(), &f);
        assert!(!scene.is_empty(), "a trip draws its chrome");
        let layer: Option<&super::RouteLayer> = host.route_layer();
        assert!(layer.is_some());
        assert!(super::DemoRequest::from_args(&["demo".to_string()]).is_none(), "no flag, no drive");
        // The browser's half: a query string, and the name check a fixture
        // name passes before it reaches a fetch path.
        let query: Vec<(String, String)> = [("nav", "sim"), ("route", "singapore-bugis-marina-bay")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let from_query = super::DemoRequest::from_query(&query).expect("?nav=sim asks for a drive");
        assert!(super::valid_route_name(&from_query.route));
        assert!(!super::valid_route_name("../secrets"), "a name that climbs out of routes/ is refused");
        // The live-routing half a shell names (Task 14): the config it hands
        // over and the request it fetches.
        let keyless: fn(&str, Option<&str>) -> super::RoutingConfig = super::RoutingConfig::keyless;
        host.set_routing(Ok(keyless("https://r.example.com", None)));
        let fetch: Option<super::RouteRequest> = host.advance(&mut camera, &f).fetch;
        assert!(fetch.is_none(), "a paused drive on its route asks for nothing");
        // The destination click both shells tell from an orbit drag, and the
        // config file native reads the key from.
        let click: fn((f32, f32), (f32, f32), f64) -> bool = super::is_destination_click;
        assert!(click((10.0, 10.0), (11.0, 10.0), 120.0) && !click((10.0, 10.0), (40.0, 10.0), 120.0));
        assert!(super::CONFIG_FILE_RELATIVE.ends_with("swat-routing.env"));
        // Voice guidance: what to say this frame, and the mute.
        let command: Option<super::VoiceCommand> = host.take_voice();
        assert!(command.is_none(), "a paused drive says nothing");
        host.toggle_pause();
        let _ = host.advance(&mut camera, &f);
        let utterance: Option<super::Utterance> = match host.take_voice() {
            Some(super::VoiceCommand::Speak(utterance)) => Some(utterance),
            _ => None,
        };
        assert!(utterance.is_some_and(|u| !u.text.is_empty() && !u.id.is_empty()), "the drive's first prompt");
        assert!(host.take_voice().is_none(), "taken once");
        host.set_voice_muted(true);
        assert!(host.voice_muted());
        assert_eq!(host.take_voice(), Some(super::VoiceCommand::Stop), "muting stops the prompt in progress");
    }

    /// The user's "I moved away from the navigation screen but it
    /// continues to run, I hear voice": off screen, the drive must not
    /// advance and nothing may be said; back on screen it carries on.
    #[test]
    fn a_suspended_host_neither_drives_nor_speaks_and_resumes_where_it_was() {
        let mut host = demo_host(SimOptions::default());
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        host.advance(&mut camera, &frame(16.0));
        assert!(matches!(host.take_voice(), Some(VoiceCommand::Speak(_))), "the opening prompt");
        host.advance(&mut camera, &frame(5_000.0));
        let _ = host.take_voice();
        let before = host.state().unwrap().remaining_distance_m;
        host.set_suspended(true);
        assert_eq!(host.take_voice(), Some(VoiceCommand::Stop), "the prompt in progress stops");
        for t in [10_000.0, 60_000.0, 300_000.0] {
            host.advance(&mut camera, &frame(t));
            assert_eq!(host.take_voice(), None, "nothing is said off screen ({t} ms)");
        }
        assert_eq!(host.state().unwrap().remaining_distance_m, before, "the drive did not advance off screen");
        host.set_suspended(false);
        host.advance(&mut camera, &frame(310_000.0));
        host.advance(&mut camera, &frame(320_000.0));
        assert!(host.state().unwrap().remaining_distance_m < before, "back on screen the drive carries on");
        // A drive the USER paused stays paused through a suspension.
        host.toggle_pause();
        host.set_suspended(true);
        host.set_suspended(false);
        assert!(host.state().unwrap().sim.is_some_and(|s| s.paused), "the user's pause is kept");
    }

    /// The user's "Whole route is not showing full route": the overview
    /// must put EVERY vertex of the route -- the part already driven too --
    /// inside the map left clear by the HUD (below the banner, above the
    /// strip and the sim tray), on a desktop window and a phone, wherever
    /// along the route the drive is.
    #[test]
    fn the_overview_shows_every_vertex_of_the_route_clear_of_the_hud() {
        let line = crate::nav::route_source::parse_route(MANILA).unwrap().line();
        // The last two: a narrow window where the shell's own pill (Studio's
        // camera / timeline toggles, 106 x 56) no longer fits beside the
        // sim tray and stacks above it at the right -- the overview must
        // clear it too.
        for (viewport, stacked_pill) in [((800.0_f32, 600.0_f32), false), ((430.0, 932.0), false), ((1300.0, 760.0), false), ((360.0, 740.0), true), ((430.0, 932.0), true)] {
            for start_at_m in [None, Some(1_500.0), Some(3_200.0)] {
                let mut host = demo_host(SimOptions { start_at_m, paused: true, ..SimOptions::default() });
                let mut camera = CameraController::new(
                    Camera { center_lon: 120.98, center_lat: 14.59, zoom: 13.0, pitch: 0.0, bearing: 0.0, viewport_width_px: viewport.0, viewport_height_px: viewport.1, fov_y: Camera::DEFAULT_FOV_Y },
                    MIN_ZOOM,
                    MAX_ZOOM,
                );
                let at = |t: f64| NavFrame { viewport, ..frame(t) };
                host.start_simulation(&camera, &at(0.0)).unwrap();
                host.advance(&mut camera, &at(0.0));
                let pill = stacked_pill.then(|| {
                    let hud = chrome::hud_rects(host.state().unwrap(), viewport, SafeInsets::default());
                    let (strip, tray) = (hud.strip.unwrap(), hud.tray.unwrap());
                    Rect { x: strip.x + strip.w - 106.0, y: tray.y - 6.0 - 56.0, w: 106.0, h: 56.0 }
                });
                host.set_host_chrome(pill.into_iter().collect());
                host.set_nav_camera_mode(NavCameraMode::Overview, &camera, &at(0.0));
                for t in [16.0, 400.0, 900.0, 1_000.0] {
                    host.advance(&mut camera, &at(t));
                }
                let hud = chrome::hud_rects(host.state().unwrap(), viewport, SafeInsets::default());
                let top = hud.top.map_or(0.0, |r| r.bottom());
                let bottom = [hud.strip, hud.tray, pill].iter().flatten().map(|r| r.y).fold(viewport.1, f32::min);
                for (i, &(lon, lat)) in line.iter().enumerate() {
                    let (x, y) = crate::render::project_world_to_screen_f64(camera.camera(), crate::overlay::geom::lon_lat_to_world(lon, lat)).unwrap();
                    assert!(
                        (0.0..=viewport.0).contains(&x) && (top..=bottom).contains(&y),
                        "{viewport:?} (pill stacked: {stacked_pill}) from {start_at_m:?} m: vertex {i} of {} at ({x:.0}, {y:.0}) is outside the clear map x 0..{}, y {top}..{bottom}",
                        line.len(),
                        viewport.0
                    );
                }
            }
        }
    }

    /// Two copies of the same running Manila drive, stepped with the same
    /// 16 ms frames: `zoomed` gets the user's input, `plain` none -- so
    /// their zoom difference is exactly what the input did, whatever the
    /// speed and the maneuver boost are doing at that moment.
    struct Twin {
        zoomed: NavHost,
        plain: NavHost,
        zcam: CameraController,
        pcam: CameraController,
        t: f64,
    }

    impl Twin {
        fn new(mode: NavCameraMode) -> Self {
            let mut twin = Self { zoomed: demo_host(SimOptions::default()), plain: demo_host(SimOptions::default()), zcam: camera(), pcam: camera(), t: 0.0 };
            for (host, cam) in [(&mut twin.zoomed, &twin.zcam), (&mut twin.plain, &twin.pcam)] {
                host.set_nav_camera_mode(mode, cam, &frame(0.0));
                host.start_simulation(cam, &frame(0.0)).unwrap();
            }
            twin
        }

        /// Steps both to `until`; returns the zoom difference at every frame.
        fn step_to(&mut self, until: f64) -> Vec<(f64, f64)> {
            let mut diffs = Vec::new();
            while self.t < until {
                self.t += 16.0;
                self.zoomed.advance(&mut self.zcam, &frame(self.t));
                self.plain.advance(&mut self.pcam, &frame(self.t));
                self.zcam.advance(std::time::Duration::from_millis(16));
                self.pcam.advance(std::time::Duration::from_millis(16));
                diffs.push((self.pcam.camera().zoom, self.zcam.camera().zoom - self.pcam.camera().zoom));
            }
            diffs
        }

        fn diff(&self) -> f64 {
            self.zcam.camera().zoom - self.pcam.camera().zoom
        }

        fn modes(&self) -> (Option<CameraMode>, Option<CameraMode>) {
            (self.zoomed.state().map(|s| s.camera_mode), self.plain.state().map(|s| s.camera_mode))
        }
    }

    /// The user's "maybe instead of clicks closer-standard-far we can have
    /// just continuous current zoom and what user changes it remains": a
    /// wheel zoom mid-drive (three notches, at a pointer nowhere near the
    /// puck) keeps the camera following in every follow mode, moves it by
    /// the levels asked for within a few frames, and the offset then
    /// holds -- through the speed changes and maneuver boosts of the rest
    /// of a running drive.
    #[test]
    fn a_zoom_mid_drive_keeps_following_and_the_offset_holds_through_the_turns() {
        for mode in [NavCameraMode::Follow3d, NavCameraMode::Follow2d, NavCameraMode::NorthUp2d] {
            let mut twin = Twin::new(mode);
            twin.step_to(4_000.0);
            assert_eq!(twin.modes().0, Some(CameraMode::Following));
            let before = twin.diff();
            assert!(before.abs() < 1e-9, "{mode:?}: the twins agree before any input");
            for _ in 0..3 {
                twin.zcam.apply(CameraIntent::ZoomAbout { levels: 0.25, anchor_px: (120.0, 90.0), immediate: false });
                twin.step_to(twin.t + 16.0);
            }
            twin.step_to(twin.t + 500.0);
            assert_eq!(twin.modes(), (Some(CameraMode::Following), Some(CameraMode::Following)), "{mode:?}: the zoom did not free the camera");
            assert!((twin.diff() - 0.75).abs() < 0.02, "{mode:?}: zoomed in by {:+.3}, wanted +0.75", twin.diff());
            assert!((twin.zoomed.nav_follow_zoom_offset() - 0.75).abs() < 1e-9);
            // The rest of the drive: a minute of turns at 1x.
            let rest = twin.step_to(twin.t + 60_000.0);
            let (lo, hi) = rest.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (z, _)| (lo.min(*z), hi.max(*z)));
            assert!(hi - lo > 0.3, "{mode:?}: the plain drive's zoom moved with speed and turns ({lo:.2}..{hi:.2}), so the check below means something");
            let worst = rest.iter().map(|(_, d)| (d - 0.75).abs()).fold(0.0, f64::max);
            assert!(worst < 0.02, "{mode:?}: the offset drifted by {worst:.3} over the turns");
            assert_eq!(twin.modes().0, Some(CameraMode::Following));
        }
    }

    /// The offset survives the whole-route overview and back, a pan and
    /// Recenter, and a restarted drive.
    #[test]
    fn the_zoom_offset_survives_overview_recenter_and_the_next_drive() {
        let mut twin = Twin::new(NavCameraMode::Follow3d);
        twin.step_to(3_000.0);
        twin.zcam.apply(CameraIntent::PinchZoomBy { levels: -0.6, anchor_px: (700.0, 500.0) });
        twin.step_to(4_000.0);
        assert!((twin.diff() + 0.6).abs() < 0.02, "zoomed out: {:+.3}", twin.diff());
        // Whole route, then back to following.
        for (host, cam) in [(&mut twin.zoomed, &mut twin.zcam), (&mut twin.plain, &mut twin.pcam)] {
            press_action(host, cam, ChromeAction::Overview, 4_000.0);
        }
        twin.step_to(6_000.0);
        assert_eq!(twin.modes().0, Some(CameraMode::Overview));
        for (host, cam) in [(&mut twin.zoomed, &mut twin.zcam), (&mut twin.plain, &mut twin.pcam)] {
            press_action(host, cam, ChromeAction::Recenter, 6_000.0);
        }
        twin.step_to(9_000.0);
        assert_eq!(twin.modes().0, Some(CameraMode::Following));
        assert!((twin.diff() + 0.6).abs() < 0.02, "after the overview: {:+.3}", twin.diff());
        // A pan frees both; Recenter brings both back, the offset intact.
        for cam in [&mut twin.zcam, &mut twin.pcam] {
            cam.apply(CameraIntent::PanBy { dx_px: 80.0, dy_px: 0.0, anchor_px: (400.0, 300.0) });
        }
        twin.step_to(9_500.0);
        assert_eq!(twin.modes(), (Some(CameraMode::Free), Some(CameraMode::Free)), "a pan still frees");
        for (host, cam) in [(&mut twin.zoomed, &mut twin.zcam), (&mut twin.plain, &mut twin.pcam)] {
            press_action(host, cam, ChromeAction::Recenter, 9_500.0);
        }
        twin.step_to(12_000.0);
        assert!((twin.diff() + 0.6).abs() < 0.02, "after pan + Recenter: {:+.3}", twin.diff());
        // The next drive starts at it.
        for (host, cam) in [(&mut twin.zoomed, &twin.zcam), (&mut twin.plain, &twin.pcam)] {
            host.end_trip();
            host.start_simulation(cam, &frame(12_000.0)).unwrap();
        }
        twin.step_to(16_000.0);
        assert!((twin.zoomed.nav_follow_zoom_offset() + 0.6).abs() < 1e-9);
        assert!((twin.diff() + 0.6).abs() < 0.02, "the next drive: {:+.3}", twin.diff());
    }

    /// `set_nav_follow_zoom_offset` (the `--nav-proximity` / `--nav-zoom-offset`
    /// startup path) moves a following camera and is clamped.
    #[test]
    fn the_follow_zoom_offset_setter_moves_the_camera_and_clamps() {
        let mut twin = Twin::new(NavCameraMode::Follow3d);
        twin.step_to(3_000.0);
        twin.zoomed.set_nav_follow_zoom_offset(1.0, &frame(twin.t));
        twin.step_to(4_000.0);
        assert!((twin.diff() - 1.0).abs() < 0.02, "{:+.3}", twin.diff());
        assert!((twin.zcam.camera().pitch - crate::nav::follow::CLOSE_PITCH_RAD).abs() < 1e-3, "+1 tilts to 50 deg");
        twin.zoomed.set_nav_follow_zoom_offset(40.0, &frame(twin.t));
        assert_eq!(twin.zoomed.nav_follow_zoom_offset(), crate::nav::follow::FOLLOW_ZOOM_OFFSET_MAX);
        twin.zoomed.set_nav_follow_zoom_offset(f64::NAN, &frame(twin.t));
        assert_eq!(twin.zoomed.nav_follow_zoom_offset(), crate::nav::follow::FOLLOW_ZOOM_OFFSET_MAX, "NaN is ignored");
    }

    #[test]
    fn an_idle_host_draws_nothing_and_asks_for_no_frames() {
        let mut host = demo_host(SimOptions::default());
        let mut camera = camera();
        let before = *camera.camera();
        assert!(!host.is_active());
        assert!(host.scene(camera.camera(), &frame(0.0)).is_empty());
        assert!(host.route_layer().is_none() && host.collision_boxes(camera.camera(), 2.0).is_empty());
        assert_eq!(host.advance(&mut camera, &frame(16.0)), NavTick::default());
        assert_eq!(*camera.camera(), before, "an idle host never touches the camera");
        assert!(!host.tap(400.0, 300.0, &mut camera, &frame(20.0)));
    }

    #[test]
    fn g_starts_a_drive_that_follows_and_g_again_ends_it() {
        let mut host = demo_host(SimOptions::default());
        let mut camera = camera();
        host.toggle_simulation(&camera, &frame(0.0));
        assert!(host.trip_active());
        for t in [16.0, 400.0, 900.0] {
            assert!(host.advance(&mut camera, &frame(t)).redraw);
        }
        assert!((camera.camera().pitch - crate::nav::follow::FOLLOW_PITCH_RAD).abs() < 1e-6, "the follow camera has tilted");
        let scene = host.scene(camera.camera(), &frame(900.0));
        assert!(matches!(scene.icons[0].glyph, ChromeGlyph::Puck { muted: false }));
        assert!(!host.collision_boxes(camera.camera(), 2.0).is_empty());
        host.toggle_simulation(&camera, &frame(1_000.0));
        assert!(!host.trip_active());
        let after: Vec<ChromeAction> = host.scene(camera.camera(), &frame(1_000.0)).buttons.iter().map(|b| b.action).collect();
        assert_eq!(after, [ChromeAction::Simulate], "the trip's chrome is gone; Simulate offers the drive again");
    }

    #[test]
    fn the_chrome_buttons_do_what_they_say() {
        let mut host = demo_host(SimOptions::default());
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        host.advance(&mut camera, &frame(900.0));
        let press = |host: &mut NavHost, camera: &mut CameraController, action: ChromeAction, now: f64| {
            let scene = host.scene(camera.camera(), &frame(now));
            let rect = scene.buttons.iter().find(|b| b.action == action).unwrap().rect;
            assert!(host.tap(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, camera, &frame(now)));
        };
        press(&mut host, &mut camera, ChromeAction::Overview, 1_000.0);
        host.advance(&mut camera, &frame(2_000.0));
        assert_eq!(host.state().unwrap().camera_mode, CameraMode::Overview);
        assert_eq!(camera.camera().pitch, 0.0);
        camera.apply(CameraIntent::PanBy { dx_px: 30.0, dy_px: 0.0, anchor_px: (400.0, 300.0) });
        host.advance(&mut camera, &frame(2_100.0));
        assert_eq!(host.state().unwrap().camera_mode, CameraMode::Free);
        press(&mut host, &mut camera, ChromeAction::Recenter, 2_200.0);
        host.advance(&mut camera, &frame(3_100.0));
        assert_eq!(host.state().unwrap().camera_mode, CameraMode::Following);
        press(&mut host, &mut camera, ChromeAction::EndTrip, 3_200.0);
        assert!(host.trip_active() && host.drive_stopped(), "a demo drive stops in place");
        host.end_trip();
        assert!(!host.trip_active());
    }

    #[test]
    fn the_camera_mode_switches_a_trip_and_carries_to_the_next() {
        let mut host = demo_host(SimOptions::default());
        let mut camera = camera();
        assert_eq!(host.nav_camera_mode(), NavCameraMode::Follow3d, "the default");
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        host.advance(&mut camera, &frame(900.0));
        assert!((camera.camera().pitch - crate::nav::follow::FOLLOW_PITCH_RAD).abs() < 1e-6);
        host.set_nav_camera_mode(NavCameraMode::NorthUp2d, &camera, &frame(1_000.0));
        host.advance(&mut camera, &frame(2_000.0));
        assert_eq!((camera.camera().pitch, camera.camera().bearing), (0.0, 0.0), "flat and north-up");
        assert_eq!(host.state().unwrap().camera_mode, CameraMode::Following);
        // A gesture frees it; the chosen mode stands, and a switch takes the camera back.
        camera.apply(CameraIntent::PanBy { dx_px: 30.0, dy_px: 0.0, anchor_px: (400.0, 300.0) });
        host.advance(&mut camera, &frame(2_100.0));
        assert_eq!(host.state().unwrap().camera_mode, CameraMode::Free);
        assert_eq!(host.nav_camera_mode(), NavCameraMode::NorthUp2d);
        host.set_nav_camera_mode(NavCameraMode::Overview, &camera, &frame(2_200.0));
        assert_eq!(host.state().unwrap().camera_mode, CameraMode::Overview);
        host.set_nav_camera_mode(NavCameraMode::Follow2d, &camera, &frame(2_300.0));
        host.end_trip();
        host.start_simulation(&camera, &frame(3_000.0)).unwrap();
        host.advance(&mut camera, &frame(4_000.0));
        assert_eq!(host.nav_camera_mode(), NavCameraMode::Follow2d, "the next trip starts in it");
        assert_eq!(camera.camera().pitch, 0.0);
        assert!(camera.camera().bearing.abs() > 1e-3 || host.state().unwrap().puck.is_some_and(|p| p.course_deg.abs() < 0.1), "heading-up");
    }

    #[test]
    fn without_a_key_a_destination_explains_itself_and_the_notice_expires() {
        let mut host = demo_host(SimOptions::default());
        host.set_routing(Err(crate::nav::config::ConfigError::MissingKey.to_string()));
        let mut camera = camera();
        assert!(host.request_route((120.98, 14.59), (120.99, 14.60), &frame(0.0)).is_none());
        let scene = host.scene(camera.camera(), &frame(10.0));
        assert!(scene.texts.iter().any(|t| t.text.contains("SWAT_ROUTING_KEY")), "{:?}", scene.texts);
        assert!(host.advance(&mut camera, &frame(100.0)).redraw);
        host.advance(&mut camera, &frame(NOTICE_MS + 1.0));
        assert!(host.scene(camera.camera(), &frame(NOTICE_MS + 2.0)).is_empty(), "the notice is gone");
    }

    #[test]
    fn a_live_route_starts_a_trip_and_a_stale_answer_is_ignored() {
        let mut host = NavHost::new();
        host.set_routing(Ok(RoutingConfig { url: "https://r.example.com".into(), road_network: "van".into(), key: RoutingKey::new("sk-SECRET") }));
        let camera = camera();
        let first = host.request_route((120.98, 14.59), (120.99, 14.60), &frame(0.0)).unwrap();
        let second = host.request_route((120.98, 14.59), (120.993246, 14.604056), &frame(10.0)).unwrap();
        assert!(second.url.starts_with("https://r.example.com/route/v1/van/120.98,14.59;120.993246,14.604056?"), "{}", second.url);
        assert!(!format!("{second:?}").contains("sk-SECRET"));
        host.route_fetched(first.id, Ok(MANILA.to_vec()), &camera, &frame(20.0));
        assert!(!host.trip_active(), "an answer to a superseded request is dropped");
        host.route_fetched(second.id, Ok(MANILA.to_vec()), &camera, &frame(30.0));
        assert!(host.trip_active());
    }

    #[test]
    fn a_failed_fetch_shows_a_redacted_reason() {
        let mut host = NavHost::new();
        host.set_routing(Ok(RoutingConfig { url: "https://r.example.com".into(), road_network: "van".into(), key: RoutingKey::new("sk-SECRET") }));
        let camera = camera();
        let request = host.request_route((120.98, 14.59), (120.99, 14.60), &frame(0.0)).unwrap();
        host.route_fetched(request.id, Err(format!("GET {} timed out", request.url)), &camera, &frame(10.0));
        let scene = host.scene(camera.camera(), &frame(20.0));
        let shown: Vec<&str> = scene.texts.iter().map(|t| t.text.as_str()).collect();
        assert!(shown.iter().any(|t| t.starts_with("No route:")), "{shown:?}");
        assert!(shown.iter().all(|t| !t.contains("sk-SECRET")), "{shown:?}");
    }

    #[test]
    fn with_no_live_routing_a_demo_reroute_is_answered_by_the_demo_route() {
        let mut host = demo_host(SimOptions { off_route_after_step: Some(5), speed: 8, ..SimOptions::default() });
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        let mut phases = Vec::new();
        for i in 0..400 {
            let tick = host.advance(&mut camera, &frame(f64::from(i) * 50.0));
            assert!(tick.fetch.is_none(), "no network without routing");
            let phase = host.state().unwrap().phase;
            if phases.last() != Some(&phase) {
                phases.push(phase);
            }
        }
        assert_eq!(phases, [TripPhase::Navigating, TripPhase::OffRoute, TripPhase::Rerouting, TripPhase::Navigating]);
    }

    /// The browser's config: `web/serve.py`'s `/swat-route/`, which holds
    /// the key, so the page's URL never carries one.
    fn browser_host() -> NavHost {
        let mut host = demo_host(SimOptions::default());
        host.set_routing(Ok(RoutingConfig::keyless("./swat-route", None)));
        host
    }

    fn destination_of(request: &RouteRequest) -> (f64, f64) {
        request.waypoints[1]
    }

    /// On the globe (z<12) the destination is the place DRAWN under the
    /// pointer: a flat-plane pick put it 40-450 px away at z3-z6.
    #[test]
    fn a_destination_click_on_the_globe_lands_where_the_map_is_drawn() {
        let mut host = browser_host();
        let camera = Camera {
            center_lon: 120.98,
            center_lat: 14.59,
            zoom: 4.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let pointer = (720.0f32, 90.0f32);
        let request = host.request_route_at(&camera, true, pointer.0, pointer.1, &frame(0.0)).expect("on the planet");
        let (lon, lat) = destination_of(&request);
        let world = crate::overlay::geom::lon_lat_to_world(lon, lat);
        let (x, y) = crate::render::globe::drawn_pixel_on_globe(&camera, world).expect("visible");
        let off = f64::from(x - pointer.0).hypot(f64::from(y - pointer.1));
        assert!(off < 0.5, "the destination is drawn {off:.3} px from the pointer");
    }

    #[test]
    fn a_destination_click_routes_from_the_camera_centre_to_the_ground_under_it() {
        let mut host = browser_host();
        let camera = camera();
        let centre = host.request_route_at(camera.camera(), true, 400.0, 300.0, &frame(0.0)).expect("the centre is on the ground");
        assert_eq!(centre.waypoints[0], (120.98, 14.59), "from the camera centre");
        let (lon, lat) = destination_of(&centre);
        assert!((lon - 120.98).abs() < 1e-6 && (lat - 14.59).abs() < 1e-6, "the screen centre is the camera centre: {lon},{lat}");
        let east = host.request_route_at(camera.camera(), true, 700.0, 300.0, &frame(10.0)).unwrap();
        let north = host.request_route_at(camera.camera(), true, 400.0, 50.0, &frame(20.0)).unwrap();
        assert!(destination_of(&east).0 > 120.98 && (destination_of(&east).1 - 14.59).abs() < 1e-6, "{:?}", east.waypoints);
        assert!(destination_of(&north).1 > 14.59, "{:?}", north.waypoints);
        assert!(north.url.starts_with("./swat-route/route/v1/van/120.98,14.59;"), "{}", north.url);
        assert!(!north.url.contains("access_token"), "the page never sends a key: {}", north.url);
        assert!(north.url.contains("geometries=polyline6") && north.url.contains("steps=true"), "{}", north.url);
    }

    #[test]
    fn a_click_above_the_horizon_picks_nothing_and_says_nothing() {
        let mut host = browser_host();
        let camera = Camera {
            center_lon: 120.98,
            center_lat: 14.59,
            zoom: 13.0,
            pitch: crate::camera_control::MAX_PITCH,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let sky = (0..=300).step_by(10).map(|y| y as f32).find(|&y| crate::render::unproject_to_ground_f64(&camera, 400.0, y).is_none());
        let Some(y) = sky else { return };
        assert!(host.request_route_at(&camera, true, 400.0, y, &frame(0.0)).is_none());
        assert!(host.scene(&camera, &frame(10.0)).is_empty(), "no notice, no pending request");
    }

    #[test]
    fn the_browser_proxy_without_a_key_explains_itself_and_the_demo_still_drives() {
        // `web/serve.py`'s 503 body when it has no key, verbatim.
        const NO_KEY: &str = r#"{"code": "NoKey", "message": "SWAT_ROUTING_KEY not set in serve.py"}"#;
        let mut host = browser_host();
        let mut camera = camera();
        let request = host.request_route_at(camera.camera(), true, 500.0, 250.0, &frame(0.0)).unwrap();
        host.route_fetched(request.id, Ok(NO_KEY.as_bytes().to_vec()), &camera, &frame(50.0));
        assert!(!host.trip_active());
        let shown: Vec<String> = host.scene(camera.camera(), &frame(60.0)).texts.into_iter().map(|t| t.text).collect();
        assert!(shown.iter().any(|t| t.starts_with("live routing is off:") && t.contains("SWAT_ROUTING_KEY")), "{shown:?}");
        assert!(host.request_route_at(camera.camera(), true, 500.0, 250.0, &frame(70.0)).is_none(), "the next click asks nothing");
        host.toggle_simulation(&camera, &frame(100.0));
        assert!(host.trip_active(), "G still drives the demo route");
        assert!(host.advance(&mut camera, &frame(500.0)).redraw);
    }

    /// The phases an off-route demo drive passes through, answering any
    /// reroute request with `answer` -- `None` means no request may go out.
    fn off_route_phases(mut host: NavHost, answer: Option<&dyn Fn(&mut NavHost, u64, &CameraController, f64)>) -> (Vec<TripPhase>, usize) {
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        let (mut phases, mut fetches) = (Vec::new(), 0);
        for i in 0..400 {
            let now = f64::from(i) * 50.0;
            let tick = host.advance(&mut camera, &frame(now));
            if let Some(request) = tick.fetch {
                fetches += 1;
                answer.expect("no request may go out")(&mut host, request.id, &camera, now + 10.0);
            }
            let phase = host.state().unwrap().phase;
            if phases.last() != Some(&phase) {
                phases.push(phase);
            }
        }
        (phases, fetches)
    }

    #[test]
    fn a_browser_demo_reroute_without_a_proxy_key_replays_the_demo_route() {
        const NO_KEY: &[u8] = br#"{"code": "NoKey", "message": "SWAT_ROUTING_KEY not set in serve.py"}"#;
        let off_route = SimOptions { off_route_after_step: Some(5), speed: 8, ..SimOptions::default() };
        let mut before = demo_host(off_route);
        before.set_routing(Err("live routing is off".into()));
        let (expected, none) = off_route_phases(before, None);
        assert_eq!(none, 0);
        let mut browser = demo_host(off_route);
        browser.set_routing(Ok(RoutingConfig::keyless("./swat-route", None)));
        let no_key = |host: &mut NavHost, id: u64, camera: &CameraController, now: f64| host.route_fetched(id, Ok(NO_KEY.to_vec()), camera, &frame(now));
        let (phases, fetches) = off_route_phases(browser, Some(&no_key));
        assert_eq!(phases, expected, "the same drive as a keyless native demo");
        assert_eq!(phases, [TripPhase::Navigating, TripPhase::OffRoute, TripPhase::Rerouting, TripPhase::Navigating]);
        assert_eq!(fetches, 1, "one probe of the proxy, then replays");
        // A page served without the proxy at all (a plain static server).
        let mut plain = demo_host(off_route);
        plain.set_routing(Ok(RoutingConfig::keyless("./swat-route", None)));
        let missing = |host: &mut NavHost, id: u64, _: &CameraController, now: f64| host.routing_unavailable(id, "HTTP 404: the page is not served by web/serve.py", &frame(now));
        assert_eq!(off_route_phases(plain, Some(&missing)).0, expected);
    }

    #[test]
    fn a_pending_destination_keeps_its_notice_and_times_out_with_no_trip() {
        let mut host = browser_host();
        let mut camera = camera();
        host.request_route_at(camera.camera(), true, 500.0, 250.0, &frame(0.0)).unwrap();
        let later = NOTICE_MS + 1_000.0;
        assert!(host.is_active(), "a pending request keeps the shell calling advance");
        assert!(host.advance(&mut camera, &frame(later)).redraw);
        let shown: Vec<String> = host.scene(camera.camera(), &frame(later)).texts.into_iter().map(|t| t.text).collect();
        assert!(shown.iter().any(|t| t.starts_with("Finding a route")), "still finding at {later} ms: {shown:?}");
        host.advance(&mut camera, &frame(REQUEST_TIMEOUT_MS + 1.0));
        let shown: Vec<String> = host.scene(camera.camera(), &frame(REQUEST_TIMEOUT_MS + 2.0)).texts.into_iter().map(|t| t.text).collect();
        assert!(shown.iter().any(|t| t.contains("did not answer")), "{shown:?}");
        host.advance(&mut camera, &frame(REQUEST_TIMEOUT_MS + NOTICE_MS + 10.0));
        assert!(!host.is_active(), "and then the host is idle again");
    }

    #[test]
    fn native_without_a_key_explains_itself_names_no_value_and_the_demo_still_drives() {
        // What `mapbox_streets_demo` hands over with the variables unset and
        // no config file, or with the key blanked: the reason, never a value.
        let empty = |_: &str| -> Option<String> { None };
        let blank_key = |name: &str| (name == "SWAT_ROUTING_KEY").then(String::new);
        let cases: [(&dyn Fn(&str) -> Option<String>, Option<&str>); 2] =
            [(&empty, None), (&blank_key, Some("SWAT_ROUTING_URL=https://r.example.com\nSWAT_ROUTING_KEY=\n"))];
        for (env, file) in cases {
            let mut host = demo_host(SimOptions::default());
            host.set_routing(RoutingConfig::resolve(env, file).map_err(|e| e.to_string()));
            let mut camera = camera();
            assert!(host.request_route_at(camera.camera(), true, 500.0, 250.0, &frame(0.0)).is_none(), "no key, no request");
            let shown: Vec<String> = host.scene(camera.camera(), &frame(10.0)).texts.into_iter().map(|t| t.text).collect();
            assert!(shown.iter().any(|t| t.contains("live routing is off: SWAT_ROUTING_")), "{shown:?}");
            if file.is_some() {
                assert!(shown.iter().any(|t| t.contains("SWAT_ROUTING_KEY")), "{shown:?}");
            }
            host.toggle_simulation(&camera, &frame(100.0));
            assert!(host.trip_active() && host.advance(&mut camera, &frame(500.0)).redraw, "G still drives");
        }
    }

    fn live_host() -> NavHost {
        let mut host = demo_host(SimOptions { off_route_after_step: Some(5), speed: 8, ..SimOptions::default() });
        host.set_routing(Ok(RoutingConfig { url: "https://r.example.com".into(), road_network: "van".into(), key: RoutingKey::new("k") }));
        host
    }

    /// Drives until the trip asks for a reroute; returns the request and
    /// the frame time it went out at.
    fn drive_to_reroute(host: &mut NavHost, camera: &mut CameraController) -> (RouteRequest, f64) {
        host.start_simulation(camera, &frame(0.0)).unwrap();
        (0..400)
            .find_map(|i| {
                let now = f64::from(i) * 50.0;
                host.advance(camera, &frame(now)).fetch.map(|f| (f, now))
            })
            .expect("a reroute request")
    }

    #[test]
    fn a_destination_picked_during_a_reroute_does_not_strand_the_trip_in_rerouting() {
        let mut host = live_host();
        let mut camera = camera();
        let (reroute, at) = drive_to_reroute(&mut host, &mut camera);
        assert_eq!(host.state().unwrap().phase, TripPhase::Rerouting);
        // Request B, a new destination, goes out while reroute A is in flight...
        let destination = host.request_route((120.98, 14.59), (120.99, 14.60), &frame(at + 10.0)).unwrap();
        // ...then A resolves. It is still A's answer: the trip hears it.
        host.route_fetched(reroute.id, Err("offline".into()), &camera, &frame(at + 20.0));
        assert_eq!(host.state().unwrap().phase, TripPhase::OffRoute, "not stuck at Rerouting");
        // And B failing leaves the trip where it was, not Rerouting either.
        host.route_fetched(destination.id, Err("offline".into()), &camera, &frame(at + 30.0));
        assert!(host.trip_active());
        assert_ne!(host.state().unwrap().phase, TripPhase::Rerouting);
        let shown: Vec<String> = host.scene(camera.camera(), &frame(at + 40.0)).texts.into_iter().map(|t| t.text).collect();
        assert!(shown.iter().any(|t| t.starts_with("No route:")), "{shown:?}");
    }

    #[test]
    fn a_reroute_fired_while_a_destination_is_pending_does_not_swallow_the_destination() {
        let mut host = live_host();
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        let destination = host.request_route((120.98, 14.59), (120.993246, 14.604056), &frame(0.0)).unwrap();
        let reroute = (1..400).find_map(|i| host.advance(&mut camera, &frame(f64::from(i) * 50.0)).fetch).expect("a reroute request");
        assert_ne!(reroute.id, destination.id);
        host.route_fetched(destination.id, Ok(MANILA.to_vec()), &camera, &frame(20_100.0));
        let state = host.state().unwrap();
        assert_eq!(state.phase, TripPhase::Navigating, "the picked destination started a fresh trip");
        assert!(state.remaining_distance_m > 3_000.0, "{}", state.remaining_distance_m);
        host.route_fetched(reroute.id, Err("offline".into()), &camera, &frame(20_200.0));
        assert_eq!(host.state().unwrap().phase, TripPhase::Navigating, "the old trip's reroute answer is dropped");
    }

    #[test]
    fn a_reroute_that_never_answers_times_out() {
        let mut host = live_host();
        let mut camera = camera();
        let (_, at) = drive_to_reroute(&mut host, &mut camera);
        assert!(host.advance(&mut camera, &frame(at + REQUEST_TIMEOUT_MS - 1.0)).redraw, "frames keep coming while waiting");
        assert_eq!(host.state().unwrap().phase, TripPhase::Rerouting);
        host.advance(&mut camera, &frame(at + REQUEST_TIMEOUT_MS));
        assert_ne!(host.state().unwrap().phase, TripPhase::Rerouting);
    }

    #[test]
    fn with_live_routing_a_reroute_is_a_fetch_to_the_destination() {
        let mut host = demo_host(SimOptions { off_route_after_step: Some(5), speed: 8, ..SimOptions::default() });
        host.set_routing(Ok(RoutingConfig { url: "https://r.example.com".into(), road_network: "van".into(), key: RoutingKey::new("k") }));
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        let fetch = (0..400).find_map(|i| host.advance(&mut camera, &frame(f64::from(i) * 50.0)).fetch).expect("a reroute request");
        assert!(fetch.url.contains(";120.993246,14.604056?"), "to the destination: {:?}", fetch);
        assert_eq!(host.state().unwrap().phase, TripPhase::Rerouting);
        host.route_fetched(fetch.id, Err("offline".into()), &camera, &frame(20_050.0));
        assert_eq!(host.state().unwrap().phase, TripPhase::OffRoute);
    }

    /// The first frame, stepping 16 ms from `from_ms`, that asks for no
    /// redraw -- and every frame for a further 2 s after it must agree.
    fn settles(host: &mut NavHost, camera: &mut CameraController, from_ms: f64, what: &str) -> f64 {
        let mut t = from_ms;
        while host.advance(camera, &frame(t)).redraw {
            t += 16.0;
            assert!(t < from_ms + NOTICE_MS + 10_000.0, "{what}: still asking for frames {} ms later", t - from_ms);
        }
        for i in 1..=125 {
            let at = t + f64::from(i) * 16.0;
            assert!(!host.advance(camera, &frame(at)).redraw, "{what}: asked for a frame again at {at} ms");
        }
        t
    }

    #[test]
    fn a_paused_an_arrived_and_an_ended_drive_stop_asking_for_frames() {
        // Paused mid-drive, with a notice up too: once the glide and the
        // notice are over, the host asks for nothing (the iOS heat fix
        // rests on this).
        let mut host = demo_host(SimOptions::default());
        let mut cam = camera();
        host.start_simulation(&cam, &frame(0.0)).unwrap();
        let mut t = 0.0;
        while t < 5_000.0 {
            assert!(host.advance(&mut cam, &frame(t)).redraw, "a moving drive draws every frame");
            t += 16.0;
        }
        host.toggle_pause();
        host.notify("paused".to_string(), t);
        let settled = settles(&mut host, &mut cam, t, "paused");
        assert!(settled >= t + NOTICE_MS, "the notice kept frames coming until it expired");
        assert!(host.trip_active() && !host.scene(cam.camera(), &frame(settled)).is_empty(), "still drawn, just not redrawn");

        // Arrived: started 30 m short of the Manila destination.
        let mut host = demo_host(SimOptions { start_at_m: Some(3_180.0), ..SimOptions::default() });
        let mut cam = camera();
        host.start_simulation(&cam, &frame(0.0)).unwrap();
        let settled = settles(&mut host, &mut cam, 0.0, "arrived");
        assert_eq!(host.state().map(|s| s.phase), Some(TripPhase::Arrived), "it settled because it arrived");

        // Ended: only the Simulate button that restarts the demo, which
        // asks for no frames.
        host.end_trip();
        assert_eq!(host.advance(&mut cam, &frame(settled + 16.0)), NavTick::default());
        assert!(!host.trip_active() && host.route_layer().is_none());
    }

    fn press_action(host: &mut NavHost, camera: &mut CameraController, action: ChromeAction, now: f64) {
        let scene = host.scene(camera.camera(), &frame(now));
        let rect = scene.buttons.iter().find(|b| b.action == action).unwrap_or_else(|| panic!("no {action:?} button")).rect;
        assert!(host.tap(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, camera, &frame(now)));
    }

    fn sim(host: &NavHost) -> Option<crate::nav::view_model::SimView> {
        host.state().and_then(|s| s.sim)
    }

    #[test]
    fn the_sim_tray_pauses_resumes_and_cycles_the_speed_at_once() {
        let mut host = demo_host(SimOptions::default());
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        host.advance(&mut camera, &frame(16.0));
        assert_eq!(sim(&host), Some(crate::nav::view_model::SimView { paused: false, speed: 1, stopped: false }), "a demo trip shows its controls");
        press_action(&mut host, &mut camera, ChromeAction::TogglePause, 100.0);
        assert!(sim(&host).unwrap().paused, "the very next scene shows it paused, before any advance");
        assert!(host.scene(camera.camera(), &frame(100.0)).icons.iter().any(|i| i.glyph == ChromeGlyph::Resume));
        assert!(host.active.as_ref().unwrap().trip.drive().unwrap().is_paused());
        press_action(&mut host, &mut camera, ChromeAction::TogglePause, 200.0);
        assert!(!sim(&host).unwrap().paused, "and resumes");
        let mut speeds = Vec::new();
        for i in 0..4 {
            press_action(&mut host, &mut camera, ChromeAction::CycleSpeed, 300.0 + f64::from(i));
            speeds.push(sim(&host).unwrap().speed);
        }
        assert_eq!(speeds, [2, 4, 8, 1]);
        assert!(host.scene(camera.camera(), &frame(400.0)).texts.iter().any(|t| t.text == "1\u{d7}"));
    }

    #[test]
    fn a_paused_drive_from_the_tray_stops_asking_for_frames() {
        let mut host = demo_host(SimOptions::default());
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        for i in 0..100 {
            host.advance(&mut camera, &frame(f64::from(i) * 16.0));
        }
        press_action(&mut host, &mut camera, ChromeAction::TogglePause, 1_600.0);
        settles(&mut host, &mut camera, 1_616.0, "paused from the tray");
        press_action(&mut host, &mut camera, ChromeAction::TogglePause, 20_000.0);
        assert!(host.advance(&mut camera, &frame(20_016.0)).redraw, "resumed, it drives again");
    }

    /// The user: "when I press stop, maybe no need to hide the full block
    /// and create a green button on the left. Why can the red stop button
    /// not just become a play button?" Stop keeps the strip -- and every
    /// button on it, in place -- and turns itself into Play.
    #[test]
    fn stop_turns_into_play_in_place_and_play_restarts_the_same_drive() {
        // `?nav=sim` / `--nav-sim`: autostarted, no Simulate button asked for.
        let mut host = demo_host(SimOptions::default());
        let mut camera = camera();
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        host.advance(&mut camera, &frame(16.0));
        let first_distance = host.state().unwrap().remaining_distance_m;
        host.advance(&mut camera, &frame(20_000.0));
        let _ = host.take_voice();
        let before = host.scene(camera.camera(), &frame(20_000.0));
        let stop = before.buttons.iter().find(|b| b.action == ChromeAction::EndTrip).unwrap().rect;
        press_action(&mut host, &mut camera, ChromeAction::EndTrip, 20_100.0);
        assert!(host.trip_active() && host.drive_stopped(), "stopped, not ended");
        assert_eq!(host.take_voice(), Some(VoiceCommand::Stop), "the prompt in progress stops");
        let after = host.scene(camera.camera(), &frame(20_200.0));
        let actions = |scene: &ChromeScene| scene.buttons.iter().map(|b| (b.action, b.rect)).collect::<Vec<_>>();
        let mut want = actions(&before);
        for (action, rect) in &mut want {
            if *rect == stop {
                *action = ChromeAction::Simulate;
            }
        }
        assert_eq!(actions(&after), want, "the same buttons in the same places; only Stop became Play");
        let play = after.icons.iter().find(|i| i.rect == Rect { x: stop.x + 8.0, y: stop.y + 8.0, w: stop.w - 16.0, h: stop.h - 16.0 }).unwrap();
        assert_eq!(play.glyph, ChromeGlyph::Play);
        let plate = after.plates.iter().find(|p| p.rect == stop).unwrap();
        assert_eq!(plate.color, chrome::PLAY_COLOR, "green, not red");
        assert!(after.plates.iter().any(|p| p.rect.y > 300.0 && p.rect.w > 300.0), "the strip is still there");
        let distance = host.state().unwrap().remaining_distance_m;
        assert!(!host.advance(&mut camera, &frame(40_000.0)).redraw, "a stopped drive asks for no frames");
        assert_eq!(host.state().unwrap().remaining_distance_m, distance, "and does not move");
        assert_eq!(host.take_voice(), None, "nor talk");
        host.toggle_pause();
        assert!(host.drive_stopped(), "P does not un-stop it");
        press_action(&mut host, &mut camera, ChromeAction::Simulate, 40_100.0);
        assert!(host.trip_active() && !host.drive_stopped(), "Play drives again");
        host.advance(&mut camera, &frame(40_116.0));
        assert!((host.state().unwrap().remaining_distance_m - first_distance).abs() < 50.0, "the same demo route, from its start");
        assert!(host.advance(&mut camera, &frame(40_200.0)).redraw, "and it moves");
        // Done on arrival ends the trip; Simulate is offered again.
        let mut host = demo_host(SimOptions { start_at_m: Some(3_180.0), ..SimOptions::default() });
        host.start_simulation(&camera, &frame(0.0)).unwrap();
        settles(&mut host, &mut camera, 0.0, "arrived");
        assert_eq!(host.state().unwrap().phase, TripPhase::Arrived);
        press_action(&mut host, &mut camera, ChromeAction::EndTrip, 50_000.0);
        assert!(host.scene(camera.camera(), &frame(50_001.0)).buttons.iter().any(|b| b.action == ChromeAction::Simulate));
        // A live trip with no demo route loaded offers nothing after End.
        let mut live = NavHost::new();
        live.set_routing(Ok(RoutingConfig::keyless("./swat-route", None)));
        let request = live.request_route((120.98, 14.59), (120.99, 14.60), &frame(0.0)).unwrap();
        live.route_fetched(request.id, Ok(MANILA.to_vec()), &camera, &frame(10.0));
        assert!(live.trip_active());
        live.end_trip();
        assert!(live.scene(camera.camera(), &frame(20.0)).is_empty() && !live.is_active(), "no demo, no Simulate");
    }

    #[test]
    fn the_browser_keys_are_natives_g_shift_g_and_p_but_never_in_a_text_field() {
        let mut host = demo_host(SimOptions::default());
        let camera = camera();
        let key = |key| KeyPress { key, ..KeyPress::default() };
        assert!(!host.key(&KeyPress { target_tag: Some("input"), ..key("g") }, &camera, &frame(0.0)), "typed, not a command");
        assert!(!host.key(&KeyPress { target_editable: true, ..key("g") }, &camera, &frame(0.0)));
        assert!(!host.key(&KeyPress { other_modifier: true, ..key("p") }, &camera, &frame(0.0)), "Cmd+P prints");
        assert!(!host.key(&key("p"), &camera, &frame(0.0)), "no trip, nothing to pause");
        assert!(!host.trip_active());
        assert!(host.key(&key("g"), &camera, &frame(0.0)));
        assert!(host.trip_active());
        assert!(host.key(&key("p"), &camera, &frame(10.0)));
        assert!(sim(&host).unwrap().paused);
        assert!(host.key(&KeyPress { shift: true, ..key("G") }, &camera, &frame(20.0)));
        assert_eq!(sim(&host).unwrap().speed, 2);
        assert!(!host.key(&KeyPress { repeat: true, ..key("g") }, &camera, &frame(25.0)), "a held G does not flicker the trip");
        assert!(host.key(&key("g"), &camera, &frame(30.0)));
        assert!(!host.trip_active(), "G again ends it");
        let mut bare = NavHost::new();
        assert!(!bare.key(&key("g"), &camera, &frame(0.0)) && !bare.is_active(), "a page without ?nav= is untouched");
    }
}
