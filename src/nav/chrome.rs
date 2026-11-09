//! The navigation chrome, laid out once for every shell: the maneuver banner
//! at the top, the ETA strip at the bottom (and, over a simulated drive,
//! the tray above it with pause and speed), the off-route and arrival
//! states, the idle "Simulate" button and a transient notice -- as rounded
//! plates, icons and text runs in LOGICAL points, plus the buttons' hit
//! rectangles. Nothing here draws; `render::nav_layer::NavChromeGpu` turns a
//! [`ChromeScene`] into quads and glyphs, and each shell only scales by its
//! own device pixel ratio and passes its safe-area insets.
//!
//! An empty scene draws nothing at all, which is how a frame with no trip
//! stays pixel-identical to one from before navigation existed.

use crate::nav::format::{arrival_text, chrome_words, clock_text, distance_text, duration_text};
use crate::nav::view_model::{CameraMode, ManeuverGlyph, NavigationState, TripPhase};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    #[must_use]
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// The screen edges the chrome must keep clear of: iOS's status bar and
/// Dynamic Island on top and home indicator below; zero on native and web.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SafeInsets {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeAction {
    Recenter,
    Overview,
    EndTrip,
    Simulate,
    /// The speaker: mute or unmute voice guidance.
    ToggleVoice,
    /// A simulated drive's pause/resume toggle.
    TogglePause,
    /// A simulated drive's speed chip: 1x -> 2x -> 4x -> 8x -> 1x.
    CycleSpeed,
}

/// A press on the chrome: a button, or a plate that must swallow the press
/// so it does not also pan the map underneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeHit {
    Action(ChromeAction),
    Surface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromeGlyph {
    Maneuver(ManeuverGlyph),
    Puck { muted: bool },
    Recenter,
    Overview,
    Close,
    Play,
    /// The voice toggle: a speaker, struck through when muted.
    Speaker { muted: bool },
    /// The sim tray's toggle while the drive runs: two bars.
    Pause,
    /// The sim tray's toggle while the drive is paused: a dark triangle for
    /// a light button ([`Self::Play`] is white, for the green Simulate).
    Resume,
    /// The maneuver arrow's head on the map: a white triangle in the
    /// route's casing colour, with a stub of the shaft's white, centred on
    /// the shaft's end (`RouteLayer::arrow_head_placement`).
    ArrowHead,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromePlate {
    pub rect: Rect,
    pub radius: f32,
    /// Straight sRGB.
    pub color: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeIcon {
    pub rect: Rect,
    pub glyph: ChromeGlyph,
    /// Clockwise, y-down.
    pub rotation_rad: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChromeText {
    pub text: String,
    pub left: f32,
    pub top: f32,
    /// Font size, logical points.
    pub size: f32,
    /// Wraps onto further lines past this width.
    pub wrap_width: Option<f32>,
    pub color: [f32; 4],
    /// Nothing of this run draws outside this rectangle: the plate it sits
    /// on. [`ellipsize`] keeps the text short enough to fit, and this is the
    /// guarantee for when a font's glyphs run wider than its estimate.
    pub clip: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeButton {
    pub rect: Rect,
    pub action: ChromeAction,
}

/// One frame's chrome. Drawn in order: plates, then icons, then text.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChromeScene {
    pub plates: Vec<ChromePlate>,
    pub icons: Vec<ChromeIcon>,
    pub texts: Vec<ChromeText>,
    pub buttons: Vec<ChromeButton>,
}

impl ChromeScene {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plates.is_empty() && self.icons.is_empty() && self.texts.is_empty()
    }
}

/// The corner chrome's one inset (`chrome::CORNER_INSET_PX`), so the
/// navigation chrome lines up with the mark, the credit and the compass.
pub const MARGIN: f32 = crate::chrome::CORNER_INSET_PX;
pub const PANEL_MAX_W: f32 = 560.0;
pub const PANEL_RADIUS: f32 = 16.0;
pub const BANNER_H: f32 = 104.0;
pub const BANNER_SECONDARY_EXTRA_H: f32 = 24.0;
pub const ALERT_H: f32 = 72.0;
pub const THEN_H: f32 = 40.0;
pub const THEN_W: f32 = 124.0;
pub const STRIP_H: f32 = 72.0;
pub const BUTTON: f32 = 44.0;
pub const MANEUVER_ICON: f32 = 64.0;
pub const PUCK_SIZE: f32 = 36.0;
/// The arrowhead icon's square: the head (`route_layer::ARROW_HEAD_WIDTH_PX`
/// across) fills [`ARROW_HEAD_FILL`] of it.
pub const ARROW_HEAD_SIZE: f32 = crate::nav::route_layer::ARROW_HEAD_WIDTH_PX / ARROW_HEAD_FILL;
/// The fraction of [`ARROW_HEAD_SIZE`] the head's base spans.
pub const ARROW_HEAD_FILL: f32 = 0.75;
pub const ARRIVAL_H: f32 = 132.0;
pub const NOTICE_H: f32 = 56.0;
pub const SIMULATE_W: f32 = 140.0;
pub const SIMULATE_H: f32 = 48.0;
/// The simulated drive's tray, above the strip's left end: one row of
/// [`BUTTON`]-high controls with [`SIM_TRAY_PAD`] round them.
pub const SIM_TRAY_PAD: f32 = 6.0;
pub const SIM_TRAY_H: f32 = BUTTON + 2.0 * SIM_TRAY_PAD;
pub const SPEED_CHIP_W: f32 = 56.0;
pub const SIM_TRAY_W: f32 = SIM_TRAY_PAD + BUTTON + SIM_TRAY_PAD + SPEED_CHIP_W + SIM_TRAY_PAD;

pub const BANNER_COLOR: [f32; 4] = [0.090, 0.353, 0.247, 0.97];
pub const THEN_COLOR: [f32; 4] = [0.063, 0.263, 0.184, 0.97];
pub const ALERT_COLOR: [f32; 4] = [0.600, 0.341, 0.039, 0.97];
pub const STRIP_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.97];
pub const BUTTON_COLOR: [f32; 4] = [0.918, 0.925, 0.937, 1.0];
pub const END_COLOR: [f32; 4] = [0.851, 0.200, 0.180, 1.0];
/// The stopped drive's Play, where Stop was: the Simulate button's green.
pub const PLAY_COLOR: [f32; 4] = BANNER_COLOR;
pub const NOTICE_COLOR: [f32; 4] = [0.157, 0.169, 0.188, 0.94];
pub const ON_DARK: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
pub const ON_LIGHT: [f32; 4] = [0.118, 0.122, 0.137, 1.0];
pub const ON_LIGHT_SECONDARY: [f32; 4] = [0.380, 0.400, 0.440, 1.0];

/// Lines the maneuver sentence may take in the banner: two, or one when a
/// secondary line sits beneath it.
pub const PRIMARY_MAX_LINES: usize = 2;
/// A Latin sans glyph's average advance, in ems, taken on the wide side so an
/// estimate that errs, errs short.
pub const AVERAGE_GLYPH_EM: f32 = 0.6;

/// `text` cut to what fits `lines` lines of `width` points at `size`, ending
/// in an ellipsis when anything was cut. An estimate by character count --
/// the chrome's layout is computed before any font is shaped -- with a 15%
/// allowance for the space word wrapping leaves at each line's end. Cuts at
/// a word boundary when one is near.
#[must_use]
pub fn ellipsize(text: &str, size: f32, width: f32, lines: usize) -> String {
    let per_line = (width / (size * AVERAGE_GLYPH_EM)).floor().max(1.0);
    let budget = ((per_line * lines as f32) * 0.85).floor().max(1.0) as usize;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= budget {
        return text.to_string();
    }
    let mut cut: String = chars[..budget - 1].iter().collect();
    if let Some(space) = cut.rfind(' ') {
        if cut[space..].chars().count() <= 12 {
            cut.truncate(space);
        }
    }
    format!("{}\u{2026}", cut.trim_end())
}

/// Clips every text from `from` on to `rect`.
fn clip_texts(scene: &mut ChromeScene, from: usize, rect: Rect) {
    for text in &mut scene.texts[from..] {
        text.clip = Some(rect);
    }
}

/// The column every panel shares: centred, at most [`PANEL_MAX_W`] wide.
fn column(viewport: (f32, f32), insets: SafeInsets) -> (f32, f32) {
    let avail = (viewport.0 - insets.left - insets.right - 2.0 * MARGIN).max(0.0);
    let w = avail.min(PANEL_MAX_W);
    (insets.left + MARGIN + (avail - w) / 2.0, w)
}

/// The chrome for a trip in progress (or just arrived).
#[must_use]
pub fn trip_scene(state: &NavigationState, viewport: (f32, f32), insets: SafeInsets) -> ChromeScene {
    let mut scene = ChromeScene::default();
    let (x, w) = column(viewport, insets);
    let top = insets.top + MARGIN;
    match (state.phase, &state.maneuver) {
        (TripPhase::Arrived, _) => {
            arrival_card(&mut scene, state, x, w, top);
            return scene;
        }
        (TripPhase::Navigating, Some(maneuver)) => {
            let h = BANNER_H + if maneuver.secondary.is_some() { BANNER_SECONDARY_EXTRA_H } else { 0.0 };
            let banner = Rect { x, y: top, w, h };
            scene.plates.push(ChromePlate { rect: banner, radius: PANEL_RADIUS, color: BANNER_COLOR });
            scene.icons.push(ChromeIcon {
                rect: Rect { x: x + 16.0, y: top + (BANNER_H - MANEUVER_ICON) / 2.0, w: MANEUVER_ICON, h: MANEUVER_ICON },
                glyph: ChromeGlyph::Maneuver(maneuver.glyph),
                rotation_rad: 0.0,
            });
            let text_left = x + 16.0 + MANEUVER_ICON + 16.0;
            let text_w = (w - (text_left - x) - 16.0).max(1.0);
            let first_text = scene.texts.len();
            scene.texts.push(text(distance_text(state.distance_to_maneuver_m), text_left, top + 12.0, 30.0, None, ON_DARK));
            let primary_lines = if maneuver.secondary.is_some() { 1 } else { PRIMARY_MAX_LINES };
            let primary = ellipsize(&maneuver.primary, 18.0, text_w, primary_lines);
            scene.texts.push(text(primary, text_left, top + 52.0, 18.0, Some(text_w), ON_DARK));
            if let Some(secondary) = &maneuver.secondary {
                let secondary = ellipsize(secondary, 15.0, text_w, 1);
                scene.texts.push(text(secondary, text_left, top + BANNER_H - 8.0, 15.0, Some(text_w), ON_DARK));
            }
            clip_texts(&mut scene, first_text, banner);
            scene.buttons.push(ChromeButton { rect: banner, action: ChromeAction::Recenter });
            if let Some(then) = &state.then {
                let pill = Rect { x, y: banner.bottom() + 8.0, w: THEN_W, h: THEN_H };
                scene.plates.push(ChromePlate { rect: pill, radius: 12.0, color: THEN_COLOR });
                scene.texts.push(text(chrome_words().then, pill.x + 14.0, pill.y + 9.0, 17.0, None, ON_DARK));
                let last = scene.texts.len() - 1;
                clip_texts(&mut scene, last, pill);
                scene.icons.push(ChromeIcon {
                    rect: Rect { x: pill.x + pill.w - 14.0 - 32.0, y: pill.y + 4.0, w: 32.0, h: 32.0 },
                    glyph: ChromeGlyph::Maneuver(then.glyph),
                    rotation_rad: 0.0,
                });
            }
        }
        // Navigating with no instruction for this step: say nothing rather
        // than "Off route" while on the route. The strip still shows.
        (TripPhase::Navigating, None) => {}
        (phase, _) => {
            let alert = Rect { x, y: top, w, h: ALERT_H };
            scene.plates.push(ChromePlate { rect: alert, radius: PANEL_RADIUS, color: ALERT_COLOR });
            let w = chrome_words();
            let words = if phase == TripPhase::Rerouting { w.rerouting } else { w.off_route };
            scene.texts.push(text(words, x + 20.0, top + (ALERT_H - 30.0) / 2.0, 24.0, None, ON_DARK));
            let last = scene.texts.len() - 1;
            clip_texts(&mut scene, last, alert);
        }
    }
    bottom_strip(&mut scene, state, viewport, insets, x, w);
    scene
}

/// The ETA strip's rect: the shared column `(x, w)`, pinned to the
/// viewport's bottom edge inside `insets.bottom` and [`MARGIN`]. The one
/// formula [`bottom_strip`] and [`hud_rects`] both build from, so a host
/// reading [`hud_rects`] can never drift from what actually draws.
fn strip_rect(viewport: (f32, f32), insets: SafeInsets, x: f32, w: f32) -> Rect {
    Rect { x, y: viewport.1 - insets.bottom - MARGIN - STRIP_H, w, h: STRIP_H }
}

/// The sim tray's rect above `strip`'s left end. The one formula
/// [`sim_tray`] and [`hud_rects`] both build from.
fn tray_rect(strip: Rect) -> Rect {
    Rect { x: strip.x, y: strip.y - 8.0 - SIM_TRAY_H, w: SIM_TRAY_W, h: SIM_TRAY_H }
}

/// The HUD's panels this frame, in the same logical-point rects
/// [`trip_scene`] draws them at -- for a host that lays its own chrome
/// over the map (Studio's drive-scrubber toggle) and must never cover
/// the engine's. Built from the same helpers `trip_scene` itself calls
/// ([`strip_rect`], [`tray_rect`]), so this can never drift from what
/// actually draws; never magic numbers re-derived on the caller's side.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HudRects {
    /// The maneuver banner, the off-route/rerouting alert, or the
    /// arrival card -- whichever top panel this phase draws. `None` only
    /// while `Navigating` with no instruction yet (the quiet frame
    /// [`trip_scene`] draws nothing for).
    pub top: Option<Rect>,
    /// The ETA strip. `None` only once arrived.
    pub strip: Option<Rect>,
    /// The simulated drive's pause/speed tray, above the strip's left
    /// end. `None` for a live trip, with no sim, or once arrived.
    pub tray: Option<Rect>,
}

/// [`HudRects`] for `state` this frame. `None` fields mean exactly what
/// they do in [`trip_scene`] -- see [`HudRects`]'s own doc.
#[must_use]
pub fn hud_rects(state: &NavigationState, viewport: (f32, f32), insets: SafeInsets) -> HudRects {
    let (x, w) = column(viewport, insets);
    let top_y = insets.top + MARGIN;
    match (state.phase, &state.maneuver) {
        (TripPhase::Arrived, _) => {
            let card_w = w.min(360.0);
            let rect = Rect { x: x + (w - card_w) / 2.0, y: top_y, w: card_w, h: ARRIVAL_H };
            HudRects { top: Some(rect), strip: None, tray: None }
        }
        (TripPhase::Navigating, Some(maneuver)) => {
            let h = BANNER_H + if maneuver.secondary.is_some() { BANNER_SECONDARY_EXTRA_H } else { 0.0 };
            let banner = Rect { x, y: top_y, w, h };
            let strip = strip_rect(viewport, insets, x, w);
            HudRects { top: Some(banner), strip: Some(strip), tray: state.sim.map(|_| tray_rect(strip)) }
        }
        (TripPhase::Navigating, None) => {
            let strip = strip_rect(viewport, insets, x, w);
            HudRects { top: None, strip: Some(strip), tray: state.sim.map(|_| tray_rect(strip)) }
        }
        _ => {
            let alert = Rect { x, y: top_y, w, h: ALERT_H };
            let strip = strip_rect(viewport, insets, x, w);
            HudRects { top: Some(alert), strip: Some(strip), tray: state.sim.map(|_| tray_rect(strip)) }
        }
    }
}

fn bottom_strip(scene: &mut ChromeScene, state: &NavigationState, viewport: (f32, f32), insets: SafeInsets, x: f32, w: f32) {
    let strip = strip_rect(viewport, insets, x, w);
    scene.plates.push(ChromePlate { rect: strip, radius: PANEL_RADIUS, color: STRIP_COLOR });
    let first_text = scene.texts.len();
    // The big figure is the time left ("9 min"), the one a driver reads at
    // a glance, as Ferrostar's trip progress and every phone nav app lead
    // with; the arrival clock goes in the line under it, labelled. It used
    // to be the other way round: a bare "00:13" in the big slot read as a
    // countdown timer (the user's "a 00:13 clock label counts down") -- it
    // was the arrival time, just after midnight, which a sped-up simulated
    // drive moves earlier as it outruns the route's own estimate.
    scene.texts.push(text(duration_text(state.remaining_duration_s), strip.x + 20.0, strip.y + 8.0, 26.0, None, ON_LIGHT));
    scene.texts.push(text(
        arrival_text(&distance_text(state.remaining_distance_m), &clock_text(state.eta_unix_ms, state.utc_offset_min)),
        strip.x + 20.0,
        strip.y + 44.0,
        15.0,
        None,
        ON_LIGHT_SECONDARY,
    ));
    clip_texts(scene, first_text, strip);
    let end = Rect { x: strip.x + strip.w - 14.0 - BUTTON, y: strip.y + (STRIP_H - BUTTON) / 2.0, w: BUTTON, h: BUTTON };
    let (camera_action, camera_glyph) = match state.camera_mode {
        CameraMode::Following => (ChromeAction::Overview, ChromeGlyph::Overview),
        CameraMode::Overview | CameraMode::Free => (ChromeAction::Recenter, ChromeGlyph::Recenter),
    };
    let camera_button = Rect { x: end.x - 10.0 - BUTTON, ..end };
    let voice_button = Rect { x: camera_button.x - 10.0 - BUTTON, ..end };
    // A stopped drive: the same button, in the same place, turned into
    // Play -- the strip stays (the user: "why can the red stop button not
    // just become a play button?").
    let (end_color, end_glyph, end_action) = if state.sim.is_some_and(|s| s.stopped) {
        (PLAY_COLOR, ChromeGlyph::Play, ChromeAction::Simulate)
    } else {
        (END_COLOR, ChromeGlyph::Close, ChromeAction::EndTrip)
    };
    for (rect, color, glyph, action) in [
        (voice_button, BUTTON_COLOR, ChromeGlyph::Speaker { muted: state.voice_muted }, ChromeAction::ToggleVoice),
        (camera_button, BUTTON_COLOR, camera_glyph, camera_action),
        (end, end_color, end_glyph, end_action),
    ] {
        scene.plates.push(ChromePlate { rect, radius: BUTTON / 2.0, color });
        scene.icons.push(ChromeIcon { rect: inset(rect, 8.0), glyph, rotation_rad: 0.0 });
        scene.buttons.push(ChromeButton { rect, action });
    }
    if let Some(sim) = state.sim {
        sim_tray(scene, sim, strip);
    }
}

/// The simulated drive's pause toggle and speed chip, on their own plate
/// just above `strip`'s left end: the strip itself has no room for two more
/// buttons beside the ETA on a phone. A live trip has no `sim` and no tray.
fn sim_tray(scene: &mut ChromeScene, sim: crate::nav::view_model::SimView, strip: Rect) {
    let tray = tray_rect(strip);
    scene.plates.push(ChromePlate { rect: tray, radius: SIM_TRAY_H / 2.0, color: STRIP_COLOR });
    let pause = Rect { x: tray.x + SIM_TRAY_PAD, y: tray.y + SIM_TRAY_PAD, w: BUTTON, h: BUTTON };
    scene.plates.push(ChromePlate { rect: pause, radius: BUTTON / 2.0, color: BUTTON_COLOR });
    let glyph = if sim.paused { ChromeGlyph::Resume } else { ChromeGlyph::Pause };
    scene.icons.push(ChromeIcon { rect: inset(pause, 8.0), glyph, rotation_rad: 0.0 });
    scene.buttons.push(ChromeButton { rect: pause, action: ChromeAction::TogglePause });
    let chip = Rect { x: pause.x + BUTTON + SIM_TRAY_PAD, y: pause.y, w: SPEED_CHIP_W, h: BUTTON };
    scene.plates.push(ChromePlate { rect: chip, radius: BUTTON / 2.0, color: BUTTON_COLOR });
    let label = format!("{}\u{d7}", sim.speed);
    const SIZE: f32 = 18.0;
    let estimate = label.chars().count() as f32 * SIZE * AVERAGE_GLYPH_EM;
    scene.texts.push(text(label, chip.x + ((chip.w - estimate) / 2.0).max(4.0), chip.y + (BUTTON - SIZE * 1.25) / 2.0, SIZE, None, ON_LIGHT));
    let last = scene.texts.len() - 1;
    clip_texts(scene, last, chip);
    scene.buttons.push(ChromeButton { rect: chip, action: ChromeAction::CycleSpeed });
}

fn arrival_card(scene: &mut ChromeScene, state: &NavigationState, x: f32, w: f32, top: f32) {
    let card_w = w.min(360.0);
    let card = Rect { x: x + (w - card_w) / 2.0, y: top, w: card_w, h: ARRIVAL_H };
    scene.plates.push(ChromePlate { rect: card, radius: PANEL_RADIUS, color: BANNER_COLOR });
    scene.icons.push(ChromeIcon {
        rect: Rect { x: card.x + 16.0, y: card.y + 16.0, w: 48.0, h: 48.0 },
        glyph: ChromeGlyph::Maneuver(ManeuverGlyph::Arrive),
        rotation_rad: 0.0,
    });
    let first_text = scene.texts.len();
    scene.texts.push(text(chrome_words().arrived, card.x + 76.0, card.y + 16.0, 24.0, None, ON_DARK));
    if let Some(road) = &state.road_name {
        let road = ellipsize(road, 16.0, card.w - 92.0, 1);
        scene.texts.push(text(road, card.x + 76.0, card.y + 50.0, 16.0, Some(card.w - 92.0), ON_DARK));
    }
    clip_texts(scene, first_text, card);
    let done = Rect { x: card.x + card.w - 16.0 - 96.0, y: card.y + card.h - 16.0 - 40.0, w: 96.0, h: 40.0 };
    scene.plates.push(ChromePlate { rect: done, radius: 20.0, color: STRIP_COLOR });
    scene.texts.push(text(chrome_words().done, done.x + 26.0, done.y + 9.0, 18.0, None, ON_LIGHT));
    let last = scene.texts.len() - 1;
    clip_texts(scene, last, done);
    scene.buttons.push(ChromeButton { rect: done, action: ChromeAction::EndTrip });
}

/// The chrome with no trip: nothing, or the "Simulate" button, and a notice
/// when one is live.
#[must_use]
pub fn idle_scene(show_simulate: bool, viewport: (f32, f32), insets: SafeInsets) -> ChromeScene {
    let mut scene = ChromeScene::default();
    if show_simulate {
        let rect = Rect { x: insets.left + MARGIN, y: viewport.1 - insets.bottom - MARGIN - SIMULATE_H, w: SIMULATE_W, h: SIMULATE_H };
        scene.plates.push(ChromePlate { rect, radius: SIMULATE_H / 2.0, color: BANNER_COLOR });
        scene.icons.push(ChromeIcon { rect: Rect { x: rect.x + 10.0, y: rect.y + 8.0, w: 32.0, h: 32.0 }, glyph: ChromeGlyph::Play, rotation_rad: 0.0 });
        scene.texts.push(text(chrome_words().simulate, rect.x + 48.0, rect.y + 13.0, 18.0, None, ON_DARK));
        clip_texts(&mut scene, 0, rect);
        scene.buttons.push(ChromeButton { rect, action: ChromeAction::Simulate });
    }
    scene
}

/// A one-line notice ("Live routing is off ...") over whatever else shows.
pub fn push_notice(scene: &mut ChromeScene, notice: &str, viewport: (f32, f32), insets: SafeInsets) {
    let (x, w) = column(viewport, insets);
    let under_top_panels = scene
        .plates
        .iter()
        .filter(|p| p.rect.y < viewport.1 / 2.0)
        .map(|p| p.rect.bottom() + 8.0)
        .fold(insets.top + MARGIN, f32::max);
    let rect = Rect { x, y: under_top_panels, w, h: NOTICE_H };
    scene.plates.push(ChromePlate { rect, radius: 12.0, color: NOTICE_COLOR });
    scene.texts.push(text(ellipsize(notice, 15.0, w - 32.0, 2), x + 16.0, rect.y + 10.0, 15.0, Some(w - 32.0), ON_DARK));
    let last = scene.texts.len() - 1;
    clip_texts(scene, last, rect);
}

/// Space the scene's top and bottom panels take, for the overview fit.
#[must_use]
pub fn clearance(scene: &ChromeScene, viewport_h: f32) -> crate::nav::follow::ChromeClearance {
    clearance_of(scene.plates.iter().map(|p| p.rect), viewport_h)
}

/// The space `rects` take at the top and bottom: a rect starting in the
/// upper half clears down to its bottom, any other clears up to its top.
#[must_use]
pub fn clearance_of(rects: impl IntoIterator<Item = Rect>, viewport_h: f32) -> crate::nav::follow::ChromeClearance {
    let mut clearance = crate::nav::follow::ChromeClearance::default();
    for rect in rects {
        if rect.y < viewport_h / 2.0 {
            clearance.top = clearance.top.max(rect.bottom());
        } else {
            clearance.bottom = clearance.bottom.max(viewport_h - rect.y);
        }
    }
    clearance
}

/// What a press at `(x, y)` (logical points) lands on. Buttons first, in
/// reverse draw order; then any plate, which swallows the press.
#[must_use]
pub fn hit(scene: &ChromeScene, x: f32, y: f32) -> Option<ChromeHit> {
    if let Some(button) = scene.buttons.iter().rev().find(|b| b.rect.contains(x, y)) {
        return Some(ChromeHit::Action(button.action));
    }
    scene.plates.iter().any(|p| p.rect.contains(x, y)).then_some(ChromeHit::Surface)
}

fn text(text: String, left: f32, top: f32, size: f32, wrap_width: Option<f32>, color: [f32; 4]) -> ChromeText {
    ChromeText { text, left, top, size, wrap_width, color, clip: None }
}

fn inset(rect: Rect, by: f32) -> Rect {
    Rect { x: rect.x + by, y: rect.y + by, w: rect.w - 2.0 * by, h: rect.h - 2.0 * by }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::view_model::{ManeuverView, Puck, SimView};

    fn state(phase: TripPhase) -> NavigationState {
        NavigationState {
            phase,
            camera_mode: CameraMode::Following,
            step_index: 2,
            step_count: 11,
            maneuver: Some(ManeuverView {
                glyph: ManeuverGlyph::SharpRight,
                maneuver_type: None,
                modifier: None,
                exit_number: None,
                primary: "Make a sharp right onto Finance Road (180)".into(),
                secondary: None,
            }),
            distance_to_maneuver_m: 212.0,
            road_name: Some("Taft Avenue".into()),
            then: None,
            remaining_distance_m: 2_550.6,
            remaining_duration_s: 480.7,
            eta_unix_ms: 1_773_755_400_000.0 + 480.7 * 1000.0,
            utc_offset_min: 480,
            trip_elapsed_ms: 62_000.0,
            puck: Some(Puck { lon: 120.98, lat: 14.59, course_deg: 150.0, on_route: true }),
            voice_muted: false,
            sim: None,
        }
    }

    fn simulated(phase: TripPhase, paused: bool, speed: u8) -> NavigationState {
        NavigationState { sim: Some(SimView { paused, speed, stopped: false }), ..state(phase) }
    }

    const PHONE: (f32, f32) = (430.0, 932.0);
    const NOTCH: SafeInsets = SafeInsets { top: 59.0, bottom: 34.0, left: 0.0, right: 0.0 };

    #[test]
    fn the_banner_reads_distance_then_instruction_and_sits_below_the_status_bar() {
        let scene = trip_scene(&state(TripPhase::Navigating), PHONE, NOTCH);
        let banner = scene.plates[0];
        assert_eq!(banner.rect, Rect { x: 12.0, y: 71.0, w: 406.0, h: BANNER_H });
        let texts: Vec<&str> = scene.texts.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["210 m", "Make a sharp right onto Finance Road (180)", "9 min", "2.6 km \u{b7} arrival 21:58"]);
        assert_eq!(scene.icons[0].glyph, ChromeGlyph::Maneuver(ManeuverGlyph::SharpRight));
        let strip = scene.plates[1].rect;
        assert_eq!(strip.bottom(), 932.0 - 34.0 - MARGIN, "the strip clears the home indicator");
    }

    #[test]
    fn a_very_long_street_name_is_cut_to_two_lines_and_clipped_to_the_banner() {
        let mut long = state(TripPhase::Navigating);
        let road = "Jalan Sultan Iskandar Muda Persiaran Kewajipan Utama Selatan Barat Daya Lebuhraya ".repeat(3);
        long.maneuver.as_mut().unwrap().primary = format!("Turn left onto {road}");
        let scene = trip_scene(&long, PHONE, NOTCH);
        let banner = scene.plates[0].rect;
        let primary = &scene.texts[1];
        assert!(primary.text.ends_with('\u{2026}'), "{}", primary.text);
        let per_line = (primary.wrap_width.unwrap() / (primary.size * AVERAGE_GLYPH_EM)).floor() as usize;
        assert!(primary.text.chars().count() <= per_line * PRIMARY_MAX_LINES, "{} chars", primary.text.chars().count());
        let line_height = primary.size * 1.25;
        assert!(primary.top + line_height * PRIMARY_MAX_LINES as f32 <= banner.bottom(), "two lines fit the banner");
        assert!(scene.texts[..2].iter().all(|t| t.clip == Some(banner)), "every banner text is clipped to the banner");
        let short = trip_scene(&state(TripPhase::Navigating), PHONE, NOTCH);
        assert_eq!(short.texts[1].text, "Make a sharp right onto Finance Road (180)", "a short sentence is left alone");
        let mut with_secondary = long.clone();
        with_secondary.maneuver.as_mut().unwrap().secondary = Some(road.clone());
        let scene = trip_scene(&with_secondary, PHONE, NOTCH);
        assert!(scene.texts[1].text.chars().count() <= per_line, "one primary line above a secondary one");
        assert!(scene.texts[2].text.ends_with('\u{2026}') && scene.texts[2].clip == Some(scene.plates[0].rect));
    }

    #[test]
    fn navigating_without_an_instruction_is_not_called_off_route() {
        let mut quiet = state(TripPhase::Navigating);
        quiet.maneuver = None;
        let scene = trip_scene(&quiet, PHONE, NOTCH);
        assert!(scene.plates.iter().all(|p| p.color != ALERT_COLOR));
        assert!(scene.texts.iter().all(|t| t.text != "Off route"));
    }

    #[test]
    fn a_wide_window_centres_a_capped_column() {
        let scene = trip_scene(&state(TripPhase::Navigating), (1_280.0, 800.0), SafeInsets::default());
        assert_eq!(scene.plates[0].rect.w, PANEL_MAX_W);
        assert_eq!(scene.plates[0].rect.x, (1_280.0 - PANEL_MAX_W) / 2.0);
    }

    #[test]
    fn the_buttons_hit_and_the_plates_swallow_the_rest() {
        let scene = trip_scene(&state(TripPhase::Navigating), PHONE, NOTCH);
        let find = |action| scene.buttons.iter().find(|b| b.action == action).unwrap().rect;
        let end = find(ChromeAction::EndTrip);
        assert_eq!(hit(&scene, end.x + 22.0, end.y + 22.0), Some(ChromeHit::Action(ChromeAction::EndTrip)));
        let overview = find(ChromeAction::Overview);
        assert_eq!(hit(&scene, overview.x + 1.0, overview.y + 1.0), Some(ChromeHit::Action(ChromeAction::Overview)));
        let strip = scene.plates[1].rect;
        assert_eq!(hit(&scene, strip.x + 20.0, strip.y + 20.0), Some(ChromeHit::Surface));
        assert_eq!(hit(&scene, 215.0, 500.0), None, "the map between the panels is the map");
    }

    #[test]
    fn the_speaker_toggle_sits_left_of_overview_and_shows_the_mute() {
        let scene = trip_scene(&state(TripPhase::Navigating), PHONE, NOTCH);
        let find = |action| scene.buttons.iter().find(|b| b.action == action).unwrap().rect;
        let (voice, overview) = (find(ChromeAction::ToggleVoice), find(ChromeAction::Overview));
        assert_eq!((voice.y, voice.w, voice.h), (overview.y, BUTTON, BUTTON), "the strip's own button row");
        assert_eq!(voice.x + BUTTON + 10.0, overview.x);
        assert_eq!(hit(&scene, voice.x + 22.0, voice.y + 22.0), Some(ChromeHit::Action(ChromeAction::ToggleVoice)));
        let glyph = |scene: &ChromeScene| scene.icons.iter().find(|i| matches!(i.glyph, ChromeGlyph::Speaker { .. })).map(|i| i.glyph);
        assert_eq!(glyph(&scene), Some(ChromeGlyph::Speaker { muted: false }));
        let mut muted = state(TripPhase::Navigating);
        muted.voice_muted = true;
        assert_eq!(glyph(&trip_scene(&muted, PHONE, NOTCH)), Some(ChromeGlyph::Speaker { muted: true }));
        let eta_right = scene.texts.iter().filter(|t| t.top > PHONE.1 / 2.0).map(|t| t.left + t.text.chars().count() as f32 * t.size * AVERAGE_GLYPH_EM).fold(0.0, f32::max);
        assert!(eta_right < voice.x, "the ETA text ends before the toggle: {eta_right} vs {}", voice.x);
        assert!(idle_scene(false, PHONE, NOTCH).buttons.is_empty(), "no trip, no toggle");
    }

    #[test]
    fn free_mode_offers_recenter_and_off_route_replaces_the_banner() {
        let mut free = state(TripPhase::Navigating);
        free.camera_mode = CameraMode::Free;
        assert!(trip_scene(&free, PHONE, NOTCH).buttons.iter().any(|b| b.action == ChromeAction::Recenter && b.rect.h == BUTTON));
        let mut off = state(TripPhase::Rerouting);
        off.maneuver = None;
        let scene = trip_scene(&off, PHONE, NOTCH);
        assert_eq!(scene.plates[0].color, ALERT_COLOR);
        assert_eq!(scene.texts[0].text, "Rerouting\u{2026}");
    }

    #[test]
    fn arrival_is_a_card_with_done() {
        let scene = trip_scene(&state(TripPhase::Arrived), PHONE, NOTCH);
        assert_eq!(scene.texts[0].text, "You have arrived");
        let done = scene.buttons.iter().find(|b| b.action == ChromeAction::EndTrip).unwrap().rect;
        assert_eq!(hit(&scene, done.x + 5.0, done.y + 5.0), Some(ChromeHit::Action(ChromeAction::EndTrip)));
        assert!(scene.plates.iter().all(|p| p.rect.y < 400.0), "no ETA strip once arrived");
    }

    #[test]
    fn with_no_trip_there_is_nothing_unless_simulate_is_offered() {
        assert!(idle_scene(false, PHONE, NOTCH).is_empty());
        let scene = idle_scene(true, PHONE, NOTCH);
        let button = scene.buttons[0];
        assert_eq!(button.action, ChromeAction::Simulate);
        assert_eq!(button.rect.bottom(), 932.0 - 34.0 - MARGIN);
    }

    #[test]
    fn a_notice_sits_under_the_banner() {
        let mut scene = trip_scene(&state(TripPhase::Navigating), PHONE, NOTCH);
        push_notice(&mut scene, "Live routing is off", PHONE, NOTCH);
        let notice = scene.plates.last().unwrap().rect;
        assert_eq!(notice.y, 71.0 + BANNER_H + 8.0);
        let mut idle = idle_scene(false, PHONE, NOTCH);
        push_notice(&mut idle, "Live routing is off", PHONE, NOTCH);
        assert_eq!(idle.plates[0].rect.y, 71.0);
    }

    #[test]
    fn the_overview_clearance_is_the_banner_and_the_strip() {
        let scene = trip_scene(&state(TripPhase::Navigating), PHONE, NOTCH);
        let c = clearance(&scene, PHONE.1);
        assert_eq!(c.top, 71.0 + BANNER_H);
        assert_eq!(c.bottom, 34.0 + MARGIN + STRIP_H);
    }

    #[test]
    fn hud_rects_matches_what_trip_scene_actually_draws() {
        let scene = trip_scene(&state(TripPhase::Navigating), PHONE, NOTCH);
        let hud = hud_rects(&state(TripPhase::Navigating), PHONE, NOTCH);
        assert_eq!(hud.top, Some(scene.plates[0].rect));
        assert_eq!(hud.strip, Some(scene.plates[1].rect));
        assert_eq!(hud.tray, None, "a live trip has no tray");

        let sim = simulated(TripPhase::Navigating, false, 2);
        let scene = trip_scene(&sim, PHONE, NOTCH);
        let hud = hud_rects(&sim, PHONE, NOTCH);
        let tray_plate = scene.plates.iter().find(|p| p.rect.w == SIM_TRAY_W).expect("the tray drew");
        assert_eq!(hud.tray, Some(tray_plate.rect));

        let arrived = state(TripPhase::Arrived);
        let scene = trip_scene(&arrived, PHONE, NOTCH);
        let hud = hud_rects(&arrived, PHONE, NOTCH);
        assert_eq!(hud.top, Some(scene.plates[0].rect));
        assert_eq!(hud.strip, None, "no ETA strip once arrived");
    }

    #[test]
    fn a_simulated_drive_shows_pause_and_speed_above_the_strip() {
        let scene = trip_scene(&simulated(TripPhase::Navigating, false, 2), PHONE, NOTCH);
        let find = |action| scene.buttons.iter().find(|b| b.action == action).map(|b| b.rect);
        let (pause, speed) = (find(ChromeAction::TogglePause).unwrap(), find(ChromeAction::CycleSpeed).unwrap());
        let strip = scene.plates[1].rect;
        assert_eq!(pause.x, strip.x + SIM_TRAY_PAD, "the tray sits over the strip's left end");
        assert!(pause.bottom() < strip.y && speed.bottom() < strip.y, "above the strip, not over the ETA");
        assert_eq!((pause.w, pause.h, speed.h), (BUTTON, BUTTON, BUTTON), "full-size touch targets");
        assert_eq!(hit(&scene, pause.x + 22.0, pause.y + 22.0), Some(ChromeHit::Action(ChromeAction::TogglePause)));
        assert_eq!(hit(&scene, speed.x + 28.0, speed.y + 22.0), Some(ChromeHit::Action(ChromeAction::CycleSpeed)));
        assert!(scene.texts.iter().any(|t| t.text == "2\u{d7}" && t.clip == Some(speed)), "the chip reads the speed");
        assert!(scene.icons.iter().any(|i| i.glyph == ChromeGlyph::Pause), "a running drive offers pause");
        let paused = trip_scene(&simulated(TripPhase::Navigating, true, 8), PHONE, NOTCH);
        assert!(paused.icons.iter().any(|i| i.glyph == ChromeGlyph::Resume) && paused.icons.iter().all(|i| i.glyph != ChromeGlyph::Pause));
        assert!(paused.texts.iter().any(|t| t.text == "8\u{d7}"));
        // The narrowest phone still fits the tray inside the column.
        let narrow = trip_scene(&simulated(TripPhase::OffRoute, false, 1), (320.0, 568.0), SafeInsets::default());
        let tray = narrow.plates.iter().find(|p| p.rect.w == SIM_TRAY_W).unwrap().rect;
        assert!(tray.x >= MARGIN && tray.x + tray.w <= 320.0 - MARGIN);
        // The overview fit keeps the route clear of the tray too.
        assert_eq!(clearance(&scene, PHONE.1).bottom, 34.0 + MARGIN + STRIP_H + 8.0 + SIM_TRAY_H);
    }

    #[test]
    fn a_live_trip_and_an_arrival_show_no_sim_controls() {
        let sim_actions = |scene: &ChromeScene| {
            scene.buttons.iter().filter(|b| matches!(b.action, ChromeAction::TogglePause | ChromeAction::CycleSpeed)).count()
        };
        assert_eq!(sim_actions(&trip_scene(&state(TripPhase::Navigating), PHONE, NOTCH)), 0, "no simulator, no tray");
        assert_eq!(sim_actions(&trip_scene(&state(TripPhase::OffRoute), PHONE, NOTCH)), 0);
        assert_eq!(sim_actions(&trip_scene(&simulated(TripPhase::Arrived, false, 1), PHONE, NOTCH)), 0, "arrived: the card only");
        assert_eq!(sim_actions(&trip_scene(&simulated(TripPhase::Rerouting, false, 1), PHONE, NOTCH)), 2);
        assert!(idle_scene(false, PHONE, NOTCH).is_empty(), "no trip, nothing drawn");
    }
}
