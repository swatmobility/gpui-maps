//! The examples' agentic HTTP surface: what this map can be asked, and
//! what it will tell you about itself, over `127.0.0.1` and nothing else.
//!
//! # Why this exists, in one paragraph
//!
//! Because the alternative was screenshots. Debugging this renderer by
//! launching a window and capturing it took over the user's display, cost
//! 1-4 GB of RSS per window, and twice helped push the machine into swap.
//! It also produced a WRONG ANSWER: a black capture was read as a
//! rendering failure when the run had already printed `frames 0` -- the
//! window had never been foreground, macOS never composited it, and
//! `Element::paint` had never run. There was nothing wrong with the
//! renderer and nothing to see in the picture. Reading one counter would
//! have settled it in a second. So [`ACTIONS`]'s `state` leads with the
//! paint census, and every field below was chosen by asking "would this
//! have prevented a wrong conclusion someone actually reached".
//!
//! # The transport is not ours
//!
//! Routing, the accept loop, the `tiny_http` plumbing and the thread
//! crossing all live in `gpui-agentic-http`, shared with a downstream app. This
//! file owns only what is specific to a map: which actions exist, and how
//! to turn one into an answer. `GET /` on the port lists every action with
//! its method, path and params, plus `"app": "gpui-map"` -- that registry,
//! not this comment, is the contract a caller reads.
//!
//! # Safety posture, unchanged from the harness this replaces
//!
//! - **Studio: always on** ([`spawn_always`]: an agent can always find
//!   a running Studio), on `GPUI_MAP_AGENTIC_PORT`
//!   or the first free port of 7334-7339. **Other examples: off unless
//!   asked for** ([`spawn`]): no `GPUI_MAP_AGENTIC_PORT` in the
//!   environment, no socket, no thread. `GPUI_MAP_DEBUG_PORT` is the old
//!   name, still read.
//! - **`127.0.0.1` only**, never `0.0.0.0`, guaranteed by
//!   `agentic_gate::spawn` itself.
//! - **Unauthenticated and mutating.** Whoever reaches the port can move
//!   this map's camera and replace its overlay features. The only thing
//!   between these calls and the network is that they never leave the
//!   machine.
//! - **An example, not the library.** `gpui-agentic-http` is a
//!   `[dev-dependency]`; a consumer linking `gpui-maps` gains no listener.
//!   `the_http_server_never_reaches_a_consumers_dependency_tree` in
//!   `src/agent.rs` fails the build if that ever stops being true.
//!
//! # CSRF: this port is NOT protected against a cross-site request
//!
//! `127.0.0.1`-only does not mean same-origin-only. Any web page open in
//! the SAME BROWSER as a running example can still reach this port: a
//! `fetch` with `mode: "no-cors"`, a `Content-Type` of `text/plain` (one of
//! the Fetch spec's own "CORS-safelisted" values) and an arbitrary body is
//! a **simple request** -- no preflight, and the browser sends it
//! cross-site with no permission asked. `gpui-agentic-http`'s own
//! `server.rs` never reads request headers before routing (confirmed by
//! reading it directly -- `AgenticCall`/`ModuleRegistration::dispatch`
//! carry no header field at all), so nothing on this side can even see
//! that request coming from elsewhere, let alone refuse it.
//!
//! **`gallery_open`'s strict-JSON-body requirement (below) does NOT close
//! this.** A `fetch` body is an exact string the caller controls byte for
//! byte -- unlike an HTML `<form>`, which can only ever produce
//! `key=value` pairs, `fetch` can set the body to `{"id":"census"}`
//! verbatim, with `Content-Type: text/plain`, and this parses that exactly
//! as if it had arrived from the gallery's own `main.rs`. The JSON-body
//! rule only rules out one narrow thing: a literal `<form>` element (whose
//! encoding always appends `=` and a value, which a full
//! `serde_json::from_str` parse refuses) -- real, but not a security
//! boundary, and NOT sufficient against the `fetch`-based attack above.
//!
//! **The residual risk**, plainly: any tab open in the same browser as a
//! running example with `GPUI_MAP_AGENTIC_PORT` set can silently switch the
//! gallery's open panel (`POST /gallery/open`), move its camera, or fire a
//! gesture, without the user clicking anything and without any visible
//! response (`no-cors` hides the reply from the page, but the request
//! still runs server-side). This is not new to `gallery_open` -- every
//! mutating `/map/*` action already had it -- but `gallery_open` is the
//! first to reach above the map into which panel is even showing.
//!
//! **It also reaches the file system.** `POST /story/render` and
//! `POST /capture/image` (`story_agentic.rs`) WRITE files -- a video, a
//! still or a PNG sequence, plus a `<stem>.story.json` manifest and a
//! `<stem>-extras/` folder beside it -- and the script they take names
//! local files the render READS (style, tracks, overlays, wordmark) and
//! URLs it fetches. So those writes are confined: every `out` must resolve
//! (canonicalised, symlinks followed, no `..`) to inside
//! `~/Movies/gpui-map`, a symlink at the output is refused, and an
//! existing file is never replaced unless the body says
//! `"overwrite": true`. A cross-site page can still fill that folder,
//! read a local file into a render, or make the process fetch a URL.
//!
//! The fix is [`crate::agentic_gate`], kit's own accept loop in front of
//! every dispatch (the crate's `AgenticServer::spawn` reads no header): a
//! browser `Origin` (including `null`), a cross-site `Sec-Fetch-Site` or a
//! `Host` that is not the loopback (DNS rebinding) is refused with 403
//! before routing, so none of the above reaches a module. (`OPTIONS` has
//! no action, so a preflight gets 405 and the browser sends nothing.)

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

use gpui::AppContext as _;
use gpui_agentic_http::{Action, AgenticCall, ModuleRegistration, Param, ParamLocation};
use gpui_maps::map_view::MapView;
use serde_json::{json, Value};

/// The name a port-range scanner reads out of `GET /`'s `"app"` field to
/// know which app it found. The shared discovery range is 7330-7339 and
/// one downstream app takes 7330; see `DEFAULT_AGENTIC_PORT`.
pub const APP_NAME: &str = "gpui-map";

/// This module's namespace. Every action is `map.<name>` in `GET /` and
/// `/map/<path>` on the wire.
pub const PREFIX: &str = "map";

/// The port used when `GPUI_MAP_AGENTIC_PORT` is set but empty, and the
/// first one Studio tries.
///
/// 7334, inside the shared 7330-7339 discovery range the bootstrap skill
/// scans, and past every port another app on this machine binds by default:
/// one downstream app's 7330, another's 7332 (a FIXED port: if it is
/// taken, that app starts with no agentic surface at all) and its GTFS audit
/// server 7333. Studio used to start at 7332 and, launched first, silently
/// took that app's port (2026-10-15). Scanners identify apps by `"app"` in
/// `GET /`, never by port number.
pub const DEFAULT_AGENTIC_PORT: u16 = 7334;

/// How often the gpui side drains the channel. Under a frame at 60 Hz, so
/// a call is answered within one frame of arriving -- and, more to the
/// point, it is answered by a `cx.update_window` that runs whether or not
/// the window was ever going to paint.
const POLL: std::time::Duration = std::time::Duration::from_millis(15);

// ---------------------------------------------------------------------------
// What the map cannot tell you about itself
// ---------------------------------------------------------------------------
//
// Three facts in `state` are not knowable from `&mut MapView`, which is
// all a dispatch fn gets:
//
//   - the style's name and the path it was read from -- the library
//     discards both (`style_spec::parse_and_compile` drops the document's
//     top-level `"name"`, and `Style` has no url/id/path field at all), so
//     the example that read the file is the only thing that knows;
//   - the tile endpoint -- `MapView` holds an `Arc<dyn TileSource>` with
//     no accessor, and the URL template is the example's;
//   - whether the window is foreground -- that is `Window`'s to answer,
//     and the poll loop is the only place with one.
//
// So the example hands the first two over at startup and the poll loop
// publishes the third every tick. Process-global rather than threaded
// through, because there is exactly one map per example process and
// pretending otherwise would be ceremony over a fact.

static PROVENANCE: Mutex<Option<Provenance>> = Mutex::new(None);

thread_local! {
    /// Set while the port plays a synthetic gesture (`POST /map/drag`,
    /// `/map/wheel`, ...) into the window as real AppKit events. Those run
    /// synchronously on this (the main) thread, so a thread-local sees
    /// exactly them (and a test's stand-in never leaks into another test).
    static SYNTHETIC_INPUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the press being handled came from the port's synthetic input,
/// not the user's hand. A control only the user may press (a consent
/// card's Allow) refuses it.
pub fn synthetic_input() -> bool {
    SYNTHETIC_INPUT.with(std::cell::Cell::get)
}

/// Marks the input played inside `f` as synthetic (the port's gestures;
/// the tests' stand-in for one).
pub fn with_synthetic_input<R>(f: impl FnOnce() -> R) -> R {
    let before = SYNTHETIC_INPUT.with(|c| c.replace(true));
    let out = f();
    SYNTHETIC_INPUT.with(|c| c.set(before));
    out
}

/// The live map behind the debug port, if one is showing right now. A
/// weak handle so this never keeps a closed panel's `MapView` (and its GPU
/// buffers, tiles) alive -- see the gallery's own `PanelSlot::map`. `None`
/// covers every phase but `Live` (`Closing`, `Blocked`, `Failed`, `Empty`),
/// and before anything has ever opened.
///
/// `Rc<RefCell<..>>`, not an `Entity` or a channel: written from the
/// gallery's own `Render::render` (`self.slot.map()`, every frame) and read
/// from [`pump_slot`]'s poll loop, both on gpui's single foreground thread
/// -- there is no cross-thread access to arbitrate here, only ordinary
/// aliasing gpui's own borrow rules already forbid overlapping. `pump`'s
/// single-map examples get one that never changes after construction.
pub type ViewSlot = std::rc::Rc<std::cell::RefCell<Option<gpui::WeakEntity<MapView>>>>;

/// The slot's current map, if it can still be upgraded. `None` covers both
/// "nothing there" and "there was something, but it died" -- [`pump_slot`]
/// treats those identically (see its own doc comment).
fn resolve_slot(slot: &ViewSlot) -> Option<gpui::Entity<MapView>> {
    slot.borrow().as_ref().and_then(|weak| weak.upgrade())
}

/// The gallery's own hook, installed once via [`set_gallery_open_hook`]:
/// how [`pump_slot`] turns a queued `POST /gallery/open` id into an actual
/// panel switch. `None` in every other example -- which is what lets
/// `answer`'s `GalleryOpen` arm tell a real gallery process from any other
/// one.
///
/// `+ Send` -- not in the signature this module's own doc names the hook
/// by -- purely so this can be a `static`; the closure the `gallery`
/// example installs here only ever runs on gpui's single foreground
/// thread, the same one that calls it.
static ON_GALLERY_OPEN: Mutex<Option<Box<dyn Fn(&str, &mut gpui::App) + Send>>> = Mutex::new(None);

/// Ids `POST /gallery/open` queued, waiting for [`pump_slot`]'s own loop --
/// which alone has an `App` to hand [`ON_GALLERY_OPEN`] -- to drain them.
static PENDING_GALLERY_OPEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Installs the gallery's own switch as `POST /gallery/open`'s handler.
/// Every other example never calls this, which is what makes that route
/// answer `"not a gallery"` there instead of silently doing nothing --
/// see `answer`'s `GalleryOpen` arm.
#[allow(dead_code)] // gallery-only: the standalone examples never install it
pub fn set_gallery_open_hook<F>(hook: F)
where
    F: Fn(&str, &mut gpui::App) + Send + 'static,
{
    if let Ok(mut slot) = ON_GALLERY_OPEN.lock() {
        *slot = Some(Box::new(hook));
    }
}

/// A pure `id -> is this a registered demo` check, installed once
/// alongside [`ON_GALLERY_OPEN`]. Separate from that hook because this one
/// needs no `App` and answers synchronously, inside `answer` itself, before
/// the HTTP reply is sent -- `POST /gallery/open` with an id the gallery's
/// own registry does not know answers `"unknown id"` right there, rather
/// than an `"ok": true` that silently never switches anything (the id
/// validation `Gallery::open` already does happens later, asynchronously,
/// with no way back to this call's own reply channel).
static GALLERY_KNOWN_ID: Mutex<Option<Box<dyn Fn(&str) -> bool + Send>>> = Mutex::new(None);

/// Installs the gallery's own `registry::entry(id).is_some()` as `POST
/// /gallery/open`'s id check. Every other example never calls this either;
/// with no hook installed, [`gallery_open_response`] treats every id as
/// known, since [`ON_GALLERY_OPEN`]'s own absence already answers
/// `"not a gallery"` first in that case.
#[allow(dead_code)] // gallery-only: the standalone examples never install it
pub fn set_gallery_known_id_hook<F>(known: F)
where
    F: Fn(&str) -> bool + Send + 'static,
{
    if let Ok(mut slot) = GALLERY_KNOWN_ID.lock() {
        *slot = Some(Box::new(known));
    }
}

/// Every id `POST /gallery/open` takes, in the sidebar's order, installed
/// by the gallery: an unknown id answers with the closest ones
/// (`did_you_mean`) and this whole list, so a client that guessed wrong
/// gets the right id back in the same reply instead of guessing again.
static GALLERY_IDS: Mutex<Option<Box<dyn Fn() -> Vec<String> + Send>>> = Mutex::new(None);

/// Installs [`GALLERY_IDS`].
#[allow(dead_code)] // gallery-only: the standalone examples never install it
pub fn set_gallery_ids_hook<F>(ids: F)
where
    F: Fn() -> Vec<String> + Send + 'static,
{
    if let Ok(mut slot) = GALLERY_IDS.lock() {
        *slot = Some(Box::new(ids));
    }
}

/// Edit distance between two short ids (Levenshtein, by char).
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(row[j + 1] + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// The `n` ids closest to `wanted`: words shared (split on `-`, `_`,
/// space; any order), then one containing the other, then edit distance.
/// "singapore-smart-city" finds "smart-city-singapore"; ids sharing
/// nothing and far in spelling are left out.
pub fn did_you_mean(wanted: &str, ids: &[String], n: usize) -> Vec<String> {
    let wanted = wanted.trim().to_ascii_lowercase();
    let words = |s: &str| s.split(['-', '_', ' ', '.']).filter(|w| w.len() > 1).map(str::to_string).collect::<Vec<_>>();
    let mine = words(&wanted);
    let mut scored: Vec<(i64, &String)> = ids
        .iter()
        .filter_map(|id| {
            let theirs = words(id);
            // A prefix counts only between words of 4+ letters: "tool" is
            // not "to".
            let shared = mine.iter().filter(|w| theirs.iter().any(|t| t == *w || (w.len() > 3 && t.len() > 3 && (t.starts_with(w.as_str()) || w.starts_with(t.as_str()))))).count() as i64;
            let contains = i64::from(id.contains(wanted.as_str()) || (wanted.len() > 3 && wanted.contains(id.as_str())));
            let dist = edit_distance(&wanted, id) as i64;
            let close = dist <= (wanted.len().max(3) as i64) / 3;
            (shared > 0 || contains > 0 || close).then_some((shared * 100 + contains * 50 - dist, id))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(n).map(|(_, id)| id.clone()).collect()
}

/// A pure `id -> is this a KNOWN id the sidebar is currently hiding behind
/// a reachability probe` check (`kit::Availability::Probe`, the
/// gallery's own `registry::DemoEntry::availability`),
/// installed alongside [`GALLERY_KNOWN_ID`]. Separate from that hook for
/// the same reason it is separate from [`ON_GALLERY_OPEN`]: this answers
/// synchronously, inside `answer` itself, with no `App` -- `POST
/// /gallery/open` for a probe-gated id the probe has not (yet) found
/// reachable answers `"not available"` right there, rather than a lucky
/// `"ok": true` that queues a switch to a panel the sidebar is not even
/// showing.
static GALLERY_UNAVAILABLE_ID: Mutex<Option<Box<dyn Fn(&str) -> bool + Send>>> = Mutex::new(None);

/// Installs the gallery's own availability check (`registry::entry(id)`'s
/// `Probe`-gate, against this session's probe mirror) as `POST
/// /gallery/open`'s second check, after [`GALLERY_KNOWN_ID`]. Every other
/// example never calls this either; with no hook installed, every known
/// id is treated as available.
#[allow(dead_code)] // gallery-only: the standalone examples never install it
pub fn set_gallery_unavailable_id_hook<F>(unavailable: F)
where
    F: Fn(&str) -> bool + Send + 'static,
{
    if let Ok(mut slot) = GALLERY_UNAVAILABLE_ID.lock() {
        *slot = Some(Box::new(unavailable));
    }
}

/// The gallery's own answer for its page actions (`POST /gallery/scroll_to`,
/// `GET /gallery/state`), installed once by the gallery: `(action, body)`
/// in, the reply out, answered synchronously on the HTTP thread (the
/// gallery queues the scroll for its next tick). `None` everywhere else,
/// which answers `"not a gallery"`.
static GALLERY_PAGE: Mutex<Option<Box<dyn Fn(&str, &str) -> Value + Send>>> = Mutex::new(None);

/// Installs [`GALLERY_PAGE`].
#[allow(dead_code)] // gallery-only: the standalone examples never install it
pub fn set_gallery_page_hook<F>(hook: F)
where
    F: Fn(&str, &str) -> Value + Send + 'static,
{
    if let Ok(mut slot) = GALLERY_PAGE.lock() {
        *slot = Some(Box::new(hook));
    }
}

/// Modules an app adds to its port before [`spawn_always`]/[`spawn`]
/// (Studio's `voice`), as constructors, since a registration is not
/// `Clone` and a second port try needs another.
static EXTRA_MODULES: Mutex<Vec<fn() -> ModuleRegistration<MapView>>> = Mutex::new(Vec::new());

/// Adds a module to the port this process serves. Call before spawning.
#[allow(dead_code)] // Studio-only: the standalone examples add none
pub fn add_module(registration: fn() -> ModuleRegistration<MapView>) {
    if let Ok(mut m) = EXTRA_MODULES.lock() {
        m.push(registration);
    }
}

/// Sections an app adds to `GET /map/state`, by key.
static EXTRA_STATE: Mutex<Vec<(&'static str, fn() -> Value)>> = Mutex::new(Vec::new());

/// Adds `key` to `GET /map/state`, filled by `section` on every call.
#[allow(dead_code)] // Studio-only
pub fn add_state_section(key: &'static str, section: fn() -> Value) {
    if let Ok(mut m) = EXTRA_STATE.lock() {
        m.push((key, section));
    }
}

/// Run once per [`pump_slot`] tick with a fresh, non-nested `App`, for an
/// app's own queued work (Studio's `POST /voice/inject`). `Send` for the
/// `static`; it only ever runs on gpui's foreground thread.
static ON_TICK: Mutex<Option<Box<dyn Fn(&mut gpui::App) + Send>>> = Mutex::new(None);

/// Run by each [`pump_slot`] tick INSIDE its window update, before the
/// slot is read: the host brings the slot up to the page on screen (Studio
/// finishes a panel switch its window, not drawing, left undone). Without
/// it the slot is only as current as the host's last render, and a window
/// that is covered or minimised does not render.
static ON_SLOT_SYNC: Mutex<Option<Box<dyn Fn(&mut gpui::Window, &mut gpui::App) + Send>>> = Mutex::new(None);

#[allow(dead_code)] // Studio-only
pub fn set_slot_sync_hook<F>(hook: F)
where
    F: Fn(&mut gpui::Window, &mut gpui::App) + Send + 'static,
{
    if let Ok(mut slot) = ON_SLOT_SYNC.lock() {
        *slot = Some(Box::new(hook));
    }
}

#[allow(dead_code)] // Studio-only
pub fn set_tick_hook<F>(hook: F)
where
    F: Fn(&mut gpui::App) + Send + 'static,
{
    if let Ok(mut slot) = ON_TICK.lock() {
        *slot = Some(Box::new(hook));
    }
}

/// What the example knows about where this map's style and tiles came
/// from, and the library does not.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    /// The style document's own top-level `"name"`, or the file stem.
    pub style_name: String,
    /// The path or URL the style was read from.
    pub style_source: String,
    /// The tile URL template, verbatim.
    pub tile_url: String,
    /// The example binary's own name, so two windows are never confused.
    pub example: &'static str,
    /// The binary's build counter (its own `GPUI_MAPS_BUILD_NUMBER`):
    /// this crate is compiled once for every binary, so the binary passes it.
    pub build: &'static str,
}

/// `1` foreground, `0` background, `2` not yet sampled. An `AtomicU64`
/// tri-state rather than an `AtomicBool` because "we have not looked yet"
/// and "it is in the background" are different claims and the first frame
/// of a run is exactly when someone is reading this.
static WINDOW_ACTIVE: AtomicU64 = AtomicU64::new(2);
/// The window's drawable size in logical pixels, packed `(w << 32) | h`,
/// sampled by the poll loop. Compare against the camera's own viewport:
/// they disagree when the window opened at a size nobody asked for.
static WINDOW_SIZE: AtomicU64 = AtomicU64::new(0);
/// Where gpui thinks the pointer is, packed as two `f32` bit patterns,
/// sampled by the poll loop.
///
/// This is the one number that answers "is the operating system delivering
/// my mouse to this window at all". gpui updates it inside
/// `Window::dispatch_event`, before any listener runs, so it moves if and
/// only if a real event arrived. Move the mouse across the window, read it
/// twice: unchanged means nothing is reaching gpui and no amount of looking
/// at the renderer will explain it.
static POINTER: AtomicU64 = AtomicU64::new(0);
/// Set once the poll loop has run at least once, so `state` can say
/// whether the gpui side is alive at all rather than implying it.
static POLLED: AtomicBool = AtomicBool::new(false);
/// Poll ticks completed. A `state` that answers at all proves the HTTP
/// thread is up; this is what proves the *gpui* thread is.
static POLL_TICKS: AtomicU64 = AtomicU64::new(0);

/// Queued work that needs a `Window`, which a dispatch fn has no way to
/// reach. Same "fire now, poll `state` afterward" shape a downstream app's own
/// `pending_agentic_requests` uses, and for the same reason.
static PENDING_WINDOW_WORK: Mutex<Vec<WindowWork>> = Mutex::new(Vec::new());

#[derive(Debug, Clone, PartialEq)]
enum WindowWork {
    /// `Window::refresh()` -- mark the window dirty and ask for a redraw.
    Refresh,
    /// A synthetic mouse gesture, pushed through `Window::dispatch_event`
    /// exactly as the platform would push a real one. See [`Gesture`].
    Gesture(Gesture),
}

/// A synthetic pointer gesture, in window-logical pixels.
///
/// # Why this is here rather than in a screenshot
///
/// "Maps not dragging" is not answerable from `GET /map/state`: the camera
/// reads identically whether the drag handler never ran or ran and did
/// nothing. It was also not answerable without taking over the user's
/// display and pushing real CoreGraphics events at their pointer -- the
/// exact thing this whole surface exists to avoid.
///
/// `Window::dispatch_event` is public in gpui 0.2.2, and it is the SAME
/// entry point the macOS platform layer calls: it walks the last rendered
/// frame's hitboxes, computes the hit test, and runs every registered
/// `on_mouse_event` listener in both phases. So a gesture pushed here
/// exercises hit testing, occlusion, z-order and the map's own handlers
/// exactly as a real mouse does.
///
/// The one thing it does NOT exercise is AppKit's own delivery of the event
/// to the window. A gesture that moves the camera here therefore proves the
/// gpui side is sound; it does not prove a human's mouse reaches the window
/// at all. Said again in the action descriptions, because that is the
/// difference between "the wrapper swallowed it" and "the window is
/// transparent to clicks", and those have different fixes.
#[derive(Debug, Clone, PartialEq)]
pub enum Gesture {
    /// Press at `(x, y)`, move to `(x + dx, y + dy)` in `steps` equal
    /// increments, release there. `shift` holds Shift for the whole
    /// gesture, which is the rotate/tilt modifier rather than pan.
    /// `right` drags with the right button and `ctrl` holds Control: both
    /// are the orbit (tilt and rotate together).
    Drag { x: f32, y: f32, dx: f32, dy: f32, steps: u32, shift: bool, ctrl: bool, right: bool },
    /// A wheel scroll of `lines` at `(x, y)`; positive zooms in. `alt`
    /// holds Option, which rotates instead of zooming; `shift` holds Shift,
    /// which tilts (vertical) and rotates (horizontal). `pixels` sends a
    /// continuous (trackpad-like) scroll of `lines * 100` px and `dx_lines`
    /// of horizontal travel, in `steps` events.
    Scroll { x: f32, y: f32, lines: f32, dx_lines: f32, alt: bool, shift: bool, pixels: bool, steps: u32 },
    /// One key press and release. See [`key_code`] for the names.
    ///
    /// Here because the search box is driven by the keyboard and by nothing
    /// else: `/` focuses it, characters query it, arrows walk the results,
    /// Enter flies the camera, Esc collapses it. None of that is reachable
    /// from a mouse, so without this the whole headline feature could only
    /// be verified by a human at the machine.
    Key { key: String, shift: bool, cmd: bool },
    /// A plain pointer move to `(x, y)`, no button held: what drives the
    /// map's hover hit-test (`FeatureEvent::HoverEnter`/`HoverLeave`),
    /// which no drag can reach -- a held button is a pan, and the engine
    /// does not hover while the camera moves.
    Move { x: f32, y: f32 },
}

/// The macOS virtual key code for a key name, and the characters it types.
///
/// Only what this demo's keyboard path actually needs. gpui's
/// `parse_keystroke` reads BOTH `charactersIgnoringModifiers` and the key
/// code, so both have to be right or the keystroke it builds does not match
/// the binding it should fire.
fn key_code(name: &str) -> Option<(u16, &'static str)> {
    Some(match name {
        "escape" | "esc" => (53, "\u{1b}"),
        "enter" | "return" => (36, "\r"),
        "tab" => (48, "\t"),
        "space" => (49, " "),
        "backspace" => (51, "\u{8}"),
        "up" => (126, "\u{f700}"),
        "down" => (125, "\u{f701}"),
        "left" => (123, "\u{f702}"),
        "right" => (124, "\u{f703}"),
        "slash" | "/" => (44, "/"),
        "f" => (3, "f"),
        "n" => (45, "n"),
        "c" => (8, "c"),
        "d" => (2, "d"),
        // The Studio's map-style cycle (`examples/studio/map_options.rs`).
        // 1 is `kVK_ANSI_S`.
        "s" => (1, "s"),
        // The Explorer's undo (cmd-Z, shift-cmd-Z redo) and open (cmd-O).
        "z" => (6, "z"),
        "o" => (31, "o"),
        // The demo's own terminator toggle (`kit/src/shell.rs`), not
        // a library binding. 17 is `kVK_ANSI_T`.
        "t" => (17, "t"),
        // The rest of the ANSI letters, so a text field (the agent chat's
        // draft line) can be typed into a letter at a time.
        "a" => (0, "a"),
        "h" => (4, "h"),
        "g" => (5, "g"),
        "x" => (7, "x"),
        "v" => (9, "v"),
        "b" => (11, "b"),
        "q" => (12, "q"),
        "w" => (13, "w"),
        "e" => (14, "e"),
        "r" => (15, "r"),
        "y" => (16, "y"),
        "u" => (32, "u"),
        "i" => (34, "i"),
        "p" => (35, "p"),
        "l" => (37, "l"),
        "j" => (38, "j"),
        "k" => (40, "k"),
        "m" => (46, "m"),
        _ => return None,
    })
}

/// What the last synthetic gesture did to the camera, reported in
/// `GET /map/state` under `"last_gesture"`.
///
/// The gesture is queued, not run inline -- it needs a `Window`, and a
/// dispatch fn has none -- so the answer cannot ride in the POST's own
/// response. Publishing it into `state` rather than inventing a second
/// polling route keeps the "fire, then read state" shape every other
/// mutating action here already has.
static LAST_GESTURE: Mutex<Option<Value>> = Mutex::new(None);

/// The previous `state` call's `(paints_submitted, wall clock)`, so the
/// next one can report the frame rate over the interval BETWEEN them
/// rather than only the average since launch. Two `state` calls a few
/// seconds apart is how you find out whether a map that says it has drawn
/// 400 frames is still drawing any.
static LAST_STATE_SAMPLE: Mutex<Option<(u64, std::time::Instant)>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Every action this map exposes, in the order `GET /` lists them.
///
/// Reads first, then mutations. Add one here first: `registry_drift` fails
/// if [`parse_action`] does not also recognise it, so a capability an agent
/// is told exists cannot be one that 404s.
pub const ACTIONS: &[Action] = &[
    Action {
        name: "state",
        method: "GET",
        path: "/state",
        description: "\
Everything about what this map is doing, in one call, designed so a renderer \
can be diagnosed without looking at the screen. Read `paint.paints_entered` \
FIRST: zero means gpui has never asked this view to draw (the window is \
occluded, minimised, behind another window, or on another Space), so no tile \
has ever been requested and the map is not broken, it is not running. Then \
`tiles.data` (loaded/decoded/pending/absent/failed over the current cover) \
and `tiles.draw` (how many visible tiles are drawing an ancestor or \
descendant stand-in rather than their own data). `absent` is not a fault: it \
is the source stating it holds nothing there. `?fields=camera,ui` answers \
only those top-level keys (the whole state is tens of KB).",
        params: &[Param {
            name: "fields",
            kind: "string",
            location: ParamLocation::Query,
            description: "Optional: comma-separated top-level keys to return (e.g. camera,ui,gallery); everything when absent.",
        }],
    },
    Action {
        name: "tiles",
        method: "GET",
        path: "/tiles",
        description: "\
One row per visible tile: z/x/y, what is known about its own data, and where \
its pixels actually came from on the last painted frame (own mesh, ancestor \
magnified with the zoom distance, a full descendant set, or nothing at all). \
This is the same record the on-screen `D` overlay draws, recorded at the very \
branches of the stand-in loop that made each decision -- not a second guess \
at it. Needs `tile_status_recording` on and at least one painted frame since; \
the harness turns it on at startup, and the response says so if it is off.",
        params: &[Param {
            name: "state",
            kind: "string",
            location: ParamLocation::Query,
            description: "Optional filter on the tile's own data state: loaded, decoded, pending, absent, failed, unknown.",
        }],
    },
    Action {
        name: "layers",
        method: "GET",
        path: "/layers",
        description: "\
Every layer that compiled, with its kind and zoom range, plus the layers that \
did not: skipped (this renderer has no equivalent) and hidden (the style said \
visibility: none). A map that looks wrong because a whole road class is \
missing looks identical to one that looks wrong for any other reason, until \
you read this.",
        params: &[],
    },
    Action {
        name: "diagnostics",
        method: "GET",
        path: "/diagnostics",
        description: "\
Everything the last style compile had to do to the document that the document \
did not ask for: layers dropped, layers hidden, and properties degraded to a \
literal because the constant folder could not reduce them. A degraded \
property still draws -- with one value replaced -- which is how a colour ends \
up wrong rather than a whole layer ending up missing.",
        params: &[],
    },
    Action {
        name: "stats",
        method: "GET",
        path: "/stats",
        description: "\
The GPU buffer allocation counters, process heap-in-use and RSS, and the \
paint census. `gpu_buffers_created_total` climbing while \
`gpu_buffer_bytes_live` stays flat means buffers are being recreated rather \
than reused. Heap flat while RSS climbs is allocator page retention, not a \
leak.",
        params: &[],
    },
    Action {
        name: "frame_log",
        method: "POST",
        path: "/frame-log/{op}",
        description: "\
Per-paint timing for a measurement window. `start` (re)starts the log; \
`take` returns every `MapView::paint` since, as `[{t_ms, paint_ms, \
acquire_ms}]` (t_ms is when the paint ended, relative to the first one), \
and stops it. Interval p50/p95 between successive `t_ms` is the frame rate \
the map actually achieved; `paint_ms` is what the paint itself cost. Needs \
the `probe` feature -- without it `take` answers an error, never an empty \
log that would read as \"no frames\".",
        params: &[Param {
            name: "op",
            kind: "string",
            location: ParamLocation::Path,
            description: "start or take.",
        }],
    },
    Action {
        name: "functions",
        method: "GET",
        path: "/functions",
        description: "\
The `map.*` agent function declarations from `gpui_maps::agent` -- name, \
JSON-Schema params and returns, and a read/write/destructive safety mark for \
each. These are what POST /map/call dispatches; this registry is what routes \
to it.",
        params: &[],
    },
    Action {
        name: "call",
        method: "POST",
        path: "/call",
        description: "\
Dispatch one `gpui_maps::agent` function by name. Body: {\"name\": \
\"map.get_camera\", \"args\": {}}. `args` may be omitted for a function that \
takes none. This is the whole of the pre-existing harness's surface, kept \
verbatim so nothing that drove the old endpoint has to be rewritten; GET \
/map/functions lists what can be named here.",
        params: &[
            Param {
                name: "name",
                kind: "string",
                location: ParamLocation::Body,
                description: "A function name from GET /map/functions, e.g. map.get_camera.",
            },
            Param {
                name: "args",
                kind: "object",
                location: ParamLocation::Body,
                description: "That function's arguments. Optional; an absent args is an empty object.",
            },
        ],
    },
    Action {
        name: "camera",
        method: "POST",
        path: "/camera",
        description: "\
Move the camera immediately, no animation. Body: {\"lon\": .., \"lat\": .., \
\"zoom\": ..}. The same call as POST /map/call with map.set_camera, given its \
own route because moving the camera is what a diagnosis actually does -- \
point it at a region and read GET /map/state back.",
        params: &[
            Param { name: "lon", kind: "number", location: ParamLocation::Body, description: "Degrees east, -180..180." },
            Param { name: "lat", kind: "number", location: ParamLocation::Body, description: "Degrees north, -90..90." },
            Param { name: "zoom", kind: "number", location: ParamLocation::Body, description: "0..20." },
        ],
    },
    Action {
        name: "ease",
        method: "POST",
        path: "/ease",
        description: "\
Ease the camera to a centre and zoom over `ms` milliseconds with the engine's \
own `MapView::ease_to` (MapLibre's easeTo), so the motion runs at the \
display's frame rate rather than at the rate requests arrive. Body: {\"lon\": \
.., \"lat\": .., \"zoom\": .., \"ms\": ..}; `ms` defaults to 2000. What a \
frame-time measurement wants: a stream of POST /map/camera calls can only \
produce frames as fast as this port answers, which is not the renderer's \
limit. Any input cancels it, as any ease.",
        params: &[
            Param { name: "lon", kind: "number", location: ParamLocation::Body, description: "Degrees east, -180..180." },
            Param { name: "lat", kind: "number", location: ParamLocation::Body, description: "Degrees north, -90..90." },
            Param { name: "zoom", kind: "number", location: ParamLocation::Body, description: "0..20." },
            Param { name: "ms", kind: "number", location: ParamLocation::Body, description: "Duration in milliseconds. Optional, default 2000." },
        ],
    },
    Action {
        name: "fly_to",
        method: "POST",
        path: "/fly_to",
        description: "\
Move the open map's camera to a place: {\"lon\": .., \"lat\": ..} and \
optionally \"zoom\" (default 12; a city is 11-13, a street 16), \"pitch\" \
(degrees, 0 looks straight down; 50-60 shows 3D buildings) and \"bearing\" \
(degrees). At once by default, so it lands even while Studio is behind \
another window; \"ms\" eases instead (that needs Studio on screen). Read \
GET /map/state?fields=camera to confirm.",
        params: &[
            Param { name: "lon", kind: "number", location: ParamLocation::Body, description: "Degrees east, -180..180." },
            Param { name: "lat", kind: "number", location: ParamLocation::Body, description: "Degrees north, -90..90." },
            Param { name: "zoom", kind: "number", location: ParamLocation::Body, description: "0..22, default 12." },
            Param { name: "pitch", kind: "number", location: ParamLocation::Body, description: "Tilt in degrees, 0..85 (optional)." },
            Param { name: "bearing", kind: "number", location: ParamLocation::Body, description: "Rotation in degrees, 0 is north up (optional)." },
            Param { name: "ms", kind: "number", location: ParamLocation::Body, description: "Ease over this many milliseconds instead (Studio on screen only)." },
        ],
    },
    Action {
        name: "tile_status_recording",
        method: "POST",
        path: "/tile-status-recording/{on}",
        description: "\
Turn the per-frame tile-status record on or off. On costs one small push per \
visible tile per painted frame and draws NOTHING on screen -- it is the \
switch an agent wants, as against `debug_tile_overlay`, which is the one a \
human wants. The harness turns this on at startup; this exists to turn it \
back off.",
        params: &[Param {
            name: "on",
            kind: "boolean",
            location: ParamLocation::Path,
            description: "true/1/on, or false/0/off.",
        }],
    },
    Action {
        name: "debug_tile_overlay",
        method: "POST",
        path: "/debug-tile-overlay/{on}",
        description: "\
Draw the tile-status overlay ON SCREEN -- the same thing the `D` key does. \
This changes what the user sees; GET /map/tiles gives the same information \
without touching their display, and is what you want unless a human is \
looking.",
        params: &[Param {
            name: "on",
            kind: "boolean",
            location: ParamLocation::Path,
            description: "true/1/on, or false/0/off.",
        }],
    },
    Action {
        name: "nav_camera",
        method: "POST",
        path: "/nav/camera/{mode}",
        description: "\
Switch the navigation camera: `follow3d` (tilted, heading-up, the default), \
`follow2d` (top-down, heading-up), `northup2d` (top-down, north-up) or \
`overview` (the rest of the route). A trip in progress glides into it, taking \
the camera back from a pan; the next trip starts in it. Answers the mode now \
chosen and, with a trip, its `camera_mode` (Following/Overview/Free).",
        params: &[Param {
            name: "mode",
            kind: "string",
            location: ParamLocation::Path,
            description: "follow3d, follow2d, northup2d or overview.",
        }],
    },
    Action {
        name: "refresh",
        method: "POST",
        path: "/refresh",
        description: "\
Mark the window dirty and ask for a redraw. Answers immediately with an ack; \
poll GET /map/state and watch `paint.paints_entered` to find out whether one \
actually happened. It often will not: macOS does not composite an occluded \
window however politely it is asked, which is precisely the condition this \
harness exists to make visible rather than to paper over. There is \
deliberately no action that raises or focuses the window -- taking over the \
user's screen is the thing this whole surface replaces.",
        params: &[],
    },
    Action {
        name: "drag",
        method: "POST",
        path: "/drag",
        description: "\
Push a synthetic left-button drag through the window's own event dispatch: \
press at (x, y), move by (dx, dy) over `steps` increments, release. Body: \
{\"x\": .., \"y\": .., \"dx\": .., \"dy\": .., \"steps\": .., \"shift\": .., \"ctrl\": .., \"button\": ..}; \
window-logical pixels, origin top-left, `steps` defaults to 8 and `shift` to \
false (shift rotates/tilts instead of panning). `button` \"right\" (or `ctrl` \
true) is the orbit: vertical tilts, horizontal rotates, both at once. \
\
This is how you answer \"is dragging broken\" without touching the user's \
pointer. It runs gpui's real hit test against the last rendered frame, so an \
element painted over the map that swallows the press shows up here exactly as \
it would under a real mouse. What it does NOT prove is that AppKit delivers a \
real click to this window in the first place -- if this moves the camera and a \
human's mouse still does not, the fault is in the window, not in the element \
tree. Read the outcome from `last_gesture` in GET /map/state.",
        params: &[
            Param { name: "x", kind: "number", location: ParamLocation::Body, description: "Press point, logical px from the window's left edge." },
            Param { name: "y", kind: "number", location: ParamLocation::Body, description: "Press point, logical px from the window's top edge." },
            Param { name: "dx", kind: "number", location: ParamLocation::Body, description: "Total horizontal travel, logical px." },
            Param { name: "dy", kind: "number", location: ParamLocation::Body, description: "Total vertical travel, logical px." },
            Param { name: "steps", kind: "number", location: ParamLocation::Body, description: "How many move events to split the travel into. Optional, default 8." },
            Param { name: "shift", kind: "boolean", location: ParamLocation::Body, description: "Hold Shift, making this a rotate/tilt gesture. Optional, default false." },
            Param { name: "ctrl", kind: "boolean", location: ParamLocation::Body, description: "Hold Control, making this an orbit (tilt + rotate). Optional, default false." },
            Param { name: "button", kind: "string", location: ParamLocation::Body, description: "\"left\" (default) or \"right\"; a right-drag orbits." },
        ],
    },
    Action {
        name: "key",
        method: "POST",
        path: "/key",
        description: "\
Press and release one key, as a real AppKit key event into the window's own \
view. Body: {\"key\": \"escape\", \"shift\": false, \"cmd\": false}. Known \
names: escape, enter, tab, space, backspace, up, down, left, right, slash, \
and the letters a-z. \
\
`t` is the mapbox_standard_dark demo's sun-terminator toggle, and its effect \
is in GET /map/state under `space`: `space.terminator` flips and \
`space.night_brightness` moves between 1.0 (off, the default) and the floor \
GPUI_MAP_NIGHT chose (0.25 when it chose none). Read `space.globe_transition` with it -- at 0.0 the map is flat \
Mercator and the night floor reaches no pixel, so the value changing while \
the screen does not is the CORRECT answer, not a broken toggle. \
\
This exists because the address search box is a keyboard-only surface -- `/` \
focuses it, arrows walk the results, Enter flies the camera, Esc collapses \
it -- so it cannot be verified through the mouse actions above. `POST \
/map/key` with `slash`, then `enter`, then read the camera from GET \
/map/state, is the whole search path end to end without a human at the \
keyboard. Typing a QUERY still needs `GPUI_MAP_SEARCH` at startup: this \
sends named keys, not text, and Japanese arrives through IME rather than \
through key events at all.",
        params: &[
            Param { name: "key", kind: "string", location: ParamLocation::Body, description: "A key name from the list above." },
            Param { name: "shift", kind: "boolean", location: ParamLocation::Body, description: "Hold Shift. Optional, default false." },
            Param { name: "cmd", kind: "boolean", location: ParamLocation::Body, description: "Hold Command. Optional, default false." },
        ],
    },
    Action {
        name: "move",
        method: "POST",
        path: "/move",
        description: "\
Push a synthetic pointer move (no button held) through the window's own event \
dispatch. Body: {\"x\": .., \"y\": ..}, window-logical pixels. This is what \
drives hover -- the map's `HoverEnter`/`HoverLeave` and whatever a demo draws \
for them -- which a drag cannot reach (a held button pans, and the engine does \
not hover while the camera moves). The camera does not move, so \
`last_gesture.moved` is false by design; read the demo's own output.",
        params: &[
            Param { name: "x", kind: "number", location: ParamLocation::Body, description: "Pointer position, logical px from the window's left edge." },
            Param { name: "y", kind: "number", location: ParamLocation::Body, description: "Pointer position, logical px from the window's top edge." },
        ],
    },
    Action {
        name: "scroll",
        method: "POST",
        path: "/scroll",
        description: "\
Push a synthetic wheel scroll through the window's own event dispatch. Body: \
{\"x\": .., \"y\": .., \"lines\": .., \"alt\": .., \"shift\": .., \"pixels\": .., \"dx_lines\": .., \"steps\": ..}; \
positive `lines` zooms in, `alt` rotates instead, `shift` tilts (vertical; \
negative `lines` tilts in, as two fingers moving up do) and rotates \
(horizontal `dx_lines`). `pixels` sends a continuous trackpad-style scroll \
of 100 px per line split over `steps` events (default 8), the way a \
two-finger swipe arrives. \
\
The cheap discriminator against POST /map/drag: scroll and drag are gated on \
DIFFERENT things -- scroll on `Hitbox::should_handle_scroll` (full \
containment), drag on `Hitbox::is_hovered` (occlusion- and z-order-aware) -- \
so zoom working while drag does not points at something painted over the map, \
and neither working points at the window or the frame. Read the outcome from \
`last_gesture` in GET /map/state.",
        params: &[
            Param { name: "x", kind: "number", location: ParamLocation::Body, description: "Pointer position, logical px from the window's left edge." },
            Param { name: "y", kind: "number", location: ParamLocation::Body, description: "Pointer position, logical px from the window's top edge." },
            Param { name: "lines", kind: "number", location: ParamLocation::Body, description: "Wheel lines; positive zooms in." },
            Param { name: "alt", kind: "boolean", location: ParamLocation::Body, description: "Hold Option, making this a rotate gesture. Optional, default false." },
            Param { name: "shift", kind: "boolean", location: ParamLocation::Body, description: "Hold Shift, making this a tilt (vertical) / rotate (horizontal) gesture. Optional, default false." },
            Param { name: "pixels", kind: "boolean", location: ParamLocation::Body, description: "Continuous trackpad-style pixel deltas instead of wheel lines. Optional, default false." },
            Param { name: "dx_lines", kind: "number", location: ParamLocation::Body, description: "Horizontal travel, in lines. Optional, default 0." },
            Param { name: "steps", kind: "number", location: ParamLocation::Body, description: "Events to split a `pixels` scroll into. Optional, default 8." },
        ],
    },
];

/// One routed call, parsed. A pure function of the wire arguments, with no
/// `MapView` in sight, so `registry_drift` can prove every declared action
/// is reachable without constructing a live map (which needs a real window
/// and a GPU).
#[derive(Debug, Clone, PartialEq)]
pub enum MapRequest {
    /// `fields`: `?fields=camera,ui` keeps only those top-level keys.
    State { fields: Option<Vec<String>> },
    Tiles { state_filter: Option<String> },
    Layers,
    Diagnostics,
    Stats,
    Functions,
    Call { name: String, args: Value },
    SetCamera { lon: f64, lat: f64, zoom: f64 },
    /// `POST /map/ease`: `MapView::ease_to` over `ms`.
    Ease { lon: f64, lat: f64, zoom: f64, ms: f64 },
    /// `POST /map/fly_to`: a jump (or, with `ms`, an ease) to a centre,
    /// zoom and optional pitch and bearing in degrees.
    FlyTo { lon: f64, lat: f64, zoom: f64, pitch: Option<f64>, bearing: Option<f64>, ms: Option<f64> },
    SetTileStatusRecording { on: bool },
    /// `POST /map/frame-log/start` (`true`) or `/take` (`false`).
    FrameLog { start: bool },
    SetDebugTileOverlay { on: bool },
    /// `POST /map/nav/camera/{mode}`: `MapView::set_nav_camera_mode`.
    NavCamera { mode: gpui_maps::nav::host::NavCameraMode },
    Refresh,
    /// A synthetic pointer gesture. See [`Gesture`] for why this exists.
    Input(Gesture),
    /// `POST /gallery/open?id=..` -- ask the gallery (if this process is
    /// one) to switch its one open panel. See [`gallery_registration`] and
    /// `answer`'s own match arm for what "not a gallery" and "no map is
    /// active" mean here.
    GalleryOpen { id: String },
}

/// `true`/`1`/`on`/`yes` and their negations, case-insensitively; anything
/// else is not a boolean and the request does not parse.
///
/// Deliberately NOT "anything that is not `false` is true": a path
/// parameter is where a typo lands, and a `POST
/// /map/tile-status-recording/ture` that silently turned recording ON
/// would be indistinguishable from one that worked.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Some(true),
        "false" | "0" | "off" | "no" => Some(false),
        _ => None,
    }
}

fn body_number(body: &Value, key: &str) -> Option<f64> {
    body.get(key)?.as_f64()
}

/// `(action_name, path_param, query, body)` to a [`MapRequest`], or `None`
/// for anything malformed. `None` becomes a `{"error": ...}` answer, never
/// a panic on the gpui thread.
#[must_use]
pub fn parse_action(name: &str, path_param: Option<&str>, query: &str, body: &str) -> Option<MapRequest> {
    match name {
        "state" => {
            let fields = gpui_agentic_http::parse_query_string(query)
                .into_iter()
                .find(|(k, _)| k == "fields")
                .map(|(_, v)| v.split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect::<Vec<_>>())
                .filter(|f| !f.is_empty());
            Some(MapRequest::State { fields })
        }
        "tiles" => {
            let state_filter = gpui_agentic_http::parse_query_string(query)
                .into_iter()
                .find(|(k, _)| k.as_str() == "state")
                .map(|(_, v)| v.to_ascii_lowercase());
            Some(MapRequest::Tiles { state_filter })
        }
        "layers" => Some(MapRequest::Layers),
        "diagnostics" => Some(MapRequest::Diagnostics),
        "stats" => Some(MapRequest::Stats),
        "functions" => Some(MapRequest::Functions),
        "call" => {
            let payload: Value = serde_json::from_str(body).ok()?;
            let function = payload.get("name")?.as_str()?.to_string();
            // A missing `args` is an empty object, not an error: most of
            // these functions take none, and making a caller type
            // `"args": {}` to ask for the camera is friction with nothing
            // behind it. Carried over verbatim from the harness this
            // replaces.
            let args = payload.get("args").cloned().unwrap_or_else(|| json!({}));
            Some(MapRequest::Call { name: function, args })
        }
        "camera" => {
            let payload: Value = serde_json::from_str(body).ok()?;
            Some(MapRequest::SetCamera {
                lon: body_number(&payload, "lon")?,
                lat: body_number(&payload, "lat")?,
                zoom: body_number(&payload, "zoom")?,
            })
        }
        "ease" => {
            let payload: Value = serde_json::from_str(body).ok()?;
            Some(MapRequest::Ease {
                lon: body_number(&payload, "lon")?,
                lat: body_number(&payload, "lat")?,
                zoom: body_number(&payload, "zoom")?,
                ms: body_number(&payload, "ms").unwrap_or(2000.0).clamp(0.0, 60_000.0),
            })
        }
        // A coding agent's camera move: zoom 12 unless given, and at once
        // unless `ms` asks for an ease. An ease advances per painted frame,
        // and Studio behind the agent's terminal paints none: it would
        // never land (TEST RUN, 2026-10-24).
        "fly_to" => {
            let payload: Value = serde_json::from_str(body).ok()?;
            Some(MapRequest::FlyTo {
                lon: body_number(&payload, "lon")?,
                lat: body_number(&payload, "lat")?,
                zoom: body_number(&payload, "zoom").unwrap_or(12.0).clamp(0.0, 22.0),
                pitch: body_number(&payload, "pitch").map(|p| p.clamp(0.0, 85.0)),
                bearing: body_number(&payload, "bearing"),
                ms: body_number(&payload, "ms").filter(|ms| *ms > 0.0).map(|ms| ms.min(60_000.0)),
            })
        }
        "tile_status_recording" => Some(MapRequest::SetTileStatusRecording { on: parse_bool(path_param?)? }),
        "frame_log" => match path_param? {
            "start" => Some(MapRequest::FrameLog { start: true }),
            "take" => Some(MapRequest::FrameLog { start: false }),
            _ => None,
        },
        "debug_tile_overlay" => Some(MapRequest::SetDebugTileOverlay { on: parse_bool(path_param?)? }),
        "nav_camera" => Some(MapRequest::NavCamera { mode: gpui_maps::nav::host::NavCameraMode::from_name(path_param?)? }),
        "refresh" => Some(MapRequest::Refresh),
        "drag" => {
            let payload: Value = serde_json::from_str(body).ok()?;
            Some(MapRequest::Input(Gesture::Drag {
                x: body_number(&payload, "x")? as f32,
                y: body_number(&payload, "y")? as f32,
                dx: body_number(&payload, "dx")? as f32,
                dy: body_number(&payload, "dy")? as f32,
                // Clamped rather than rejected: a caller who asks for zero
                // steps wants a drag, and a drag with no move event in it
                // would report "did not move" for a reason that is the
                // caller's typo rather than the map's fault -- which is the
                // single most expensive kind of wrong answer this surface
                // can give.
                steps: body_number(&payload, "steps").map_or(8, |s| (s as u32).clamp(1, 256)),
                shift: payload.get("shift").and_then(Value::as_bool).unwrap_or(false),
                ctrl: payload.get("ctrl").and_then(Value::as_bool).unwrap_or(false),
                right: payload.get("button").and_then(Value::as_str) == Some("right"),
            }))
        }
        "key" => {
            let payload: Value = serde_json::from_str(body).ok()?;
            let key = payload.get("key")?.as_str()?.trim().to_ascii_lowercase();
            // Rejected here rather than silently doing nothing: an unknown
            // key name that answers `{"ok": true}` is a caller believing
            // they tested something they did not.
            key_code(&key)?;
            Some(MapRequest::Input(Gesture::Key {
                key,
                shift: payload.get("shift").and_then(Value::as_bool).unwrap_or(false),
                cmd: payload.get("cmd").and_then(Value::as_bool).unwrap_or(false),
            }))
        }
        "move" => {
            let payload: Value = serde_json::from_str(body).ok()?;
            Some(MapRequest::Input(Gesture::Move {
                x: body_number(&payload, "x")? as f32,
                y: body_number(&payload, "y")? as f32,
            }))
        }
        "scroll" => {
            let payload: Value = serde_json::from_str(body).ok()?;
            Some(MapRequest::Input(Gesture::Scroll {
                x: body_number(&payload, "x")? as f32,
                y: body_number(&payload, "y")? as f32,
                lines: body_number(&payload, "lines")? as f32,
                dx_lines: body_number(&payload, "dx_lines").unwrap_or(0.0) as f32,
                alt: payload.get("alt").and_then(Value::as_bool).unwrap_or(false),
                shift: payload.get("shift").and_then(Value::as_bool).unwrap_or(false),
                pixels: payload.get("pixels").and_then(Value::as_bool).unwrap_or(false),
                steps: body_number(&payload, "steps").map_or(8, |s| (s as u32).clamp(1, 256)),
            }))
        }
        "gallery_open" => {
            // Body-only, deliberately: NOT a query-string param like
            // `tiles`' own filter. See this module's own CSRF doc comment
            // for what this does and, importantly, does NOT do --
            // `gpui-agentic-http` hands this fn no request headers to check
            // an `Origin` against, and a full, exact `serde_json::from_str`
            // parse only rules out a literal HTML `<form>` POST (whose
            // encoding always appends `=` and a value). A cross-site
            // `fetch` with `mode: "no-cors"` and `Content-Type: text/plain`
            // can still send this exact body and is not stopped by this
            // check at all.
            let payload: Value = serde_json::from_str(body).ok()?;
            // `entry` is the overview's name for the same thing.
            let id = payload.get("id").or_else(|| payload.get("entry"))?.as_str()?.to_string();
            Some(MapRequest::GalleryOpen { id })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Answering
// ---------------------------------------------------------------------------

/// The state snapshot. See the `state` action's own description for what to
/// read first and why.
/// `GET /map/state?fields=a,b`: only those top-level keys (plus `app`),
/// and `unknown_fields` naming any asked for that the state has not.
/// The whole state is 20-60 KB; a caller after the camera wants 200 bytes.
pub fn select_fields(full: Value, fields: Option<&[String]>) -> Value {
    let (Some(fields), Value::Object(all)) = (fields, &full) else { return full };
    let mut out = serde_json::Map::new();
    if let Some(app) = all.get("app") {
        out.insert("app".into(), app.clone());
    }
    let mut unknown = Vec::new();
    for f in fields {
        match all.get(f) {
            Some(v) => {
                out.insert(f.clone(), v.clone());
            }
            None => unknown.push(f.clone()),
        }
    }
    if !unknown.is_empty() {
        out.insert("unknown_fields".into(), json!(unknown));
        out.insert("fields".into(), json!(all.keys().collect::<Vec<_>>()));
    }
    Value::Object(out)
}

fn state(map: &MapView) -> Value {
    let paint = gpui_maps::render::paint_census();
    let stats = gpui_maps::render::allocation_stats();
    let progress = map.cover_progress();
    let camera = map.camera();
    let provenance = PROVENANCE.lock().ok().and_then(|p| p.clone()).unwrap_or_default();

    // The frame rate over the interval since the LAST `state` call, which
    // is the only rate that answers "is it drawing now". The mean since
    // launch cannot: a map that drew 400 frames in its first two seconds
    // and has been frozen for a minute reports a healthy-looking average
    // forever.
    let interval = {
        let mut last = LAST_STATE_SAMPLE.lock().expect("the state sample mutex is never poisoned by a panic here");
        let now = std::time::Instant::now();
        let previous = last.replace((paint.paints_submitted, now));
        previous.map(|(frames, at)| {
            let seconds = now.duration_since(at).as_secs_f64();
            json!({
                "seconds": seconds,
                "frames": paint.paints_submitted.saturating_sub(frames),
                "fps": if seconds > 0.0 { (paint.paints_submitted.saturating_sub(frames)) as f64 / seconds } else { 0.0 },
                "note": "READ THIS BEFORE QUOTING THE FPS. Every call on this surface ends \
                         with a `cx.notify()`, reads included -- inherited deliberately \
                         from the harness this replaces, because it is what keeps a \
                         backgrounded map advancing at all. So polling here DRIVES frames: \
                         this figure is the rate the renderer achieves WHEN ASKED to draw, \
                         not the rate an unattended map is drawing at. For that, make \
                         exactly two calls with a real gap between them and read \
                         paints_submitted yourself.",
            })
        })
    };

    let sample = map.tile_status_sample();
    let diagnostics = gpui_maps::style_spec::compile_diagnostics();
    let count_of = |kind: gpui_maps::style_spec::DiagnosticKind| {
        diagnostics.iter().filter(|d| d.kind == kind).count()
    };

    let mut out = json!({
        "app": APP_NAME,
        "example": provenance.example,
        "crate_version": gpui_maps::agent::CRATE_VERSION,
        "build": provenance.build,
        "pid": std::process::id(),

        // Read this first. See the action description.
        "paint": paint,
        "paint_interval_since_last_state_call": interval,

        "window": window_json(camera),

        // Absent until something has posted to /map/drag or /map/scroll.
        // Present, with `moved: false`, is the interesting answer: the
        // gesture reached gpui's dispatch and the camera did not move.
        "last_gesture": LAST_GESTURE.lock().ok().and_then(|g| g.clone()).unwrap_or(Value::Null),

        "camera": {
            "lon": camera.center_lon,
            "lat": camera.center_lat,
            "zoom": camera.zoom,
            "bearing_degrees": camera.bearing.to_degrees(),
            "pitch_degrees": camera.pitch.to_degrees(),
            "bearing_radians": camera.bearing,
            "pitch_radians": camera.pitch,
            "viewport_width_px": camera.viewport_width_px,
            "viewport_height_px": camera.viewport_height_px,
        },

        // Null with no trip. `camera_mode` is what the follow camera is
        // doing (Following / Overview / Free); `nav_camera` the mode
        // chosen; `follow_zoom_offset` the user's zoom while following;
        // `puck_px` the vehicle on screen, map-logical px.
        "nav": map.nav_state().map(|state| json!({
            "camera_mode": format!("{:?}", state.camera_mode),
            "nav_camera": map.nav_camera_mode().name(),
            "follow_zoom_offset": map.nav_follow_zoom_offset(),
            "puck_px": map.nav_puck_screen().map(|(x, y)| json!([x, y])),
            // The drive's progress and the speaker, for a check that a
            // background window keeps driving and talking.
            "phase": format!("{:?}", state.phase),
            "remaining_distance_m": state.remaining_distance_m,
            "remaining_duration_s": state.remaining_duration_s,
            "trip_elapsed_ms": state.trip_elapsed_ms,
            "voice_muted": state.voice_muted,
            "sim": state.sim.map(|s| json!({"paused": s.paused, "speed": s.speed, "stopped": s.stopped})),
            "suspended": map.nav_suspended(),
        })),

        "style": {
            "name": provenance.style_name,
            "source": provenance.style_source,
            "layers_compiled": map.style().layers.len(),
            "layers_skipped": count_of(gpui_maps::style_spec::DiagnosticKind::SkippedLayer),
            "layers_hidden": count_of(gpui_maps::style_spec::DiagnosticKind::HiddenLayer),
            "properties_degraded": count_of(gpui_maps::style_spec::DiagnosticKind::DegradedProperty),
            "skip_causes": skip_causes(&diagnostics),
            "sprite_sheet": map.style().sprite,
            "note": "layers_compiled is what this renderer drew from; the three counts \
                     beside it are what the document asked for and did not get. \
                     GET /map/diagnostics has the per-layer detail.",
        },

        "space": space_json(map),

        // A running `POST /story/preview` on this map: {playing, t,
        // segment, waiting_on_tiles, ...}; null when none is.
        "story": crate::story_agentic::preview_state_json(),

        "tiles": tiles_json(map, &progress, &sample, paint.paints_entered),

        "memory": {
            "rss_bytes": stats.rss_bytes,
            "rss_mb": stats.rss_bytes as f64 / 1e6,
            "heap_in_use_bytes": stats.heap_in_use_bytes,
            "heap_in_use_mb": stats.heap_in_use_bytes as f64 / 1e6,
            "gpu_buffers_live": stats.gpu_buffers_live,
            "gpu_buffer_bytes_live": stats.gpu_buffer_bytes_live,
            "gpu_buffers_created_total": stats.gpu_buffers_created_total,
            "gpu_buffer_bytes_created_total": stats.gpu_buffer_bytes_created_total,
            "mesh_installs_total": stats.mesh_installs_total,
            "note": "heap flat while RSS climbs is allocator page retention, not a leak. \
                     GET /map/stats has every counter.",
        },

        "caches": map
            .cache_occupancy()
            .into_iter()
            .map(|(name, len, capacity)| json!({ "name": name, "len": len, "capacity": capacity }))
            .collect::<Vec<_>>(),

        "source": {
            "tile_url": provenance.tile_url,
            "note": "the URL template the example handed the tile source; `MapView` \
                     keeps an `Arc<dyn TileSource>` with no accessor, so this is the \
                     example's own record, not a read-back.",
        },
    });
    if let (Value::Object(map), Ok(extra)) = (&mut out, EXTRA_STATE.lock()) {
        for (key, section) in extra.iter() {
            map.insert((*key).to_string(), section());
        }
    }
    out
}

/// What the globe is sitting in and how it is lit — read straight off
/// `MapView::space()`, which is the same value `paint` hands the shader.
///
/// This block exists because the sun terminator is the one visual state on
/// this map that a screenshot cannot settle: the night floor multiplies the
/// night side only through `render::GlobeParams`, which `MapView::paint`
/// gates on the globe transition, and that transition is `0.0` from zoom 6
/// up (`render::globe::GLOBE_ZOOM_MAX`). So a caller who toggles it at zoom
/// 12 sees no pixel move, and without `globe_transition` beside the value
/// there is no way to tell that from a toggle that did nothing at all —
/// which is exactly the pair of opposite diagnoses this whole surface exists
/// to separate.
fn space_json(map: &MapView) -> Value {
    let space = map.space();
    let atmosphere = space.atmosphere;
    // The SPACE fade, not the projection's globeness: the terminator, the
    // backdrop, the air and the stars are all gated on this one, and it is
    // the one that is 0.0 at zoom 6 and above. The projection stays fully
    // spherical up to zoom 11 (`render::globe::PROJECTION_BLEND_ZOOM_MIN`),
    // which is a different question and reported separately below.
    let transition = gpui_maps::render::globe::space_transition_for_zoom(map.camera().zoom);
    json!({
        "globe_transition": transition,
        // The Globe / Plain switch (`MapView::set_globe_enabled`): off, the
        // frame is flat Mercator at every zoom whatever the transitions say.
        "globe_enabled": map.globe_enabled(),
        "projection_transition":
            gpui_maps::render::globe::transition_for_zoom(map.camera().zoom),
        "terminator": atmosphere.night_brightness_clamped() < 1.0,
        "night_brightness": atmosphere.night_brightness,
        "night_brightness_clamped": atmosphere.night_brightness_clamped(),
        "atmosphere_intensity": atmosphere.intensity,
        "star_intensity": space.star_intensity,
        "note": "globe_transition is the SPACE fade: 1.0 at zoom 3 and below, 0.0 at zoom \
                 6 and above, and NOTHING here reaches a pixel at 0.0. \
                 projection_transition is a different number -- how much of the matrix is \
                 the sphere -- and it is 1.0 up to zoom 11, blending to 0.0 by zoom 12. terminator is night_brightness \
                 clamped below 1.0 -- 1.0 means no ground shading, which is the \
                 terminator switched off, not a bright night side. The initial value \
                 comes from GPUI_MAP_NIGHT and is 1.0 (off) when unset; the T key toggles \
                 it between 1.0 and that value (0.25 when unset).",
        "writable": "Every term here is WRITABLE over this harness, one at a time, via \
                     POST /map/call: map.set_atmosphere, map.set_star_intensity, \
                     map.set_terminator and map.set_space. Use them rather than starting \
                     a second instance to test a lighting change -- a second window \
                     steals OS focus from the one being watched, and the keystrokes meant \
                     for it land in the wrong map. GET /map/functions has the argument \
                     shapes; map.get_space reads back the three terms this block omits \
                     (the sun, the space colour, and star_size_scale).",
    })
}

/// The window half of the snapshot, and an honest account of its limits.
fn window_json(camera: &gpui_maps::render::Camera) -> Value {
    let packed = WINDOW_SIZE.load(Relaxed);
    let (width, height) = ((packed >> 32) as u32, (packed & 0xffff_ffff) as u32);
    let pointer_packed = POINTER.load(Relaxed);
    let pointer_x = f32::from_bits((pointer_packed >> 32) as u32);
    let pointer_y = f32::from_bits((pointer_packed & 0xffff_ffff) as u32);
    let active = match WINDOW_ACTIVE.load(Relaxed) {
        0 => Value::Bool(false),
        1 => Value::Bool(true),
        _ => Value::Null,
    };
    json!({
        "foreground": active,
        "foreground_source": "gpui Window::is_window_active -- OS keyboard focus",
        "polled": POLLED.load(Relaxed),
        "poll_ticks": POLL_TICKS.load(Relaxed),
        "drawable_width_px": width,
        "drawable_height_px": height,
        "pointer": {
            "x": pointer_x,
            "y": pointer_y,
            "note": "Where gpui last saw the mouse, in logical px from the window's \
                     top-left. Updated inside gpui's own event dispatch, so it moves \
                     if and only if the OS delivered an event. Read it twice with the \
                     mouse moving over the window: unchanged means nothing is arriving, \
                     which is a window/OS answer, not a renderer one.",
        },
        "camera_viewport_matches_window":
            (width as f32 - camera.viewport_width_px).abs() < 1.0
                && (height as f32 - camera.viewport_height_px).abs() < 1.0,
        "note": "gpui exposes focus, NOT occlusion, and they are different: a window can \
                 be focused and still not composited, or unfocused and drawing fine. The \
                 occlusion signal is in `paint` instead -- paints_entered stuck at 0, or \
                 paints_skipped_occluded climbing. `polled: false` means the gpui thread \
                 has not run this harness's loop even once, which is a different failure \
                 from the window not drawing.",
    })
}

/// The tile half of the snapshot: what the source has said, and what the
/// renderer did about it.
fn tiles_json(
    map: &MapView,
    progress: &gpui_maps::map_view::CoverProgress,
    sample: &gpui_maps::map_view::TileStatusSample,
    paints_entered: u64,
) -> Value {
    let mut data = std::collections::BTreeMap::<&'static str, u64>::new();
    let mut draw = std::collections::BTreeMap::<&'static str, u64>::new();
    let mut ancestor_distances = std::collections::BTreeMap::<u8, u64>::new();
    for status in &sample.statuses {
        *data.entry(data_label(status.data)).or_default() += 1;
        *draw.entry(draw_label(status.draw)).or_default() += 1;
        if let gpui_maps::render::TileDrawSource::Ancestor { zoom_distance } = status.draw {
            *ancestor_distances.entry(zoom_distance).or_default() += 1;
        }
    }
    let stand_ins: u64 = draw.get("ancestor").copied().unwrap_or(0) + draw.get("descendant").copied().unwrap_or(0);
    let fresh = sample.paints_entered != 0 && sample.paints_entered == paints_entered;

    json!({
        // Live, computed on this call, needing no recording at all.
        "cover": {
            "visible": progress.cover,
            "resident": progress.resident,
            "absent": progress.absent,
            "pending_anywhere": progress.pending,
            "failed_anywhere": progress.failed,
            "settled": progress.cover > 0 && progress.pending == 0 && progress.resident + progress.absent >= progress.cover,
            "note": "resident and absent are counted over the CURRENT cover; \
                     pending and failed are whole-table counts and include prefetch. \
                     `settled` true means the map has finished arriving, as against \
                     merely stopped changing. `visible` here is the bounding-box cover \
                     the fetcher works from and is normally LARGER than `sampled_tiles` \
                     below, which counts only the tiles that also survived frustum \
                     culling -- the ones `paint` actually walked. The two disagreeing is \
                     expected, not a bug.",
        },
        // From the last painted frame's record.
        "recording": map.tile_status_recording(),
        "sample_from_paint": sample.paints_entered,
        "sample_is_current_frame": fresh,
        "sampled_tiles": sample.statuses.len(),
        "data": data,
        "draw": draw,
        "stand_ins": stand_ins,
        "ancestor_zoom_distances": ancestor_distances.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<std::collections::BTreeMap<_, _>>(),
        "legend": {
            "data": "loaded = its own mesh is up; decoded = data cached but no mesh of \
                     its own; pending = fetch in flight; absent = the source states it \
                     holds nothing here (NOT a fault); failed = fetch or decode error; \
                     unknown = nothing known yet.",
            "draw": "own = its own mesh; ancestor = a coarser cached tile magnified, \
                     see ancestor_zoom_distances; descendant = a complete finer set; \
                     nothing = a hole, no pixels of this tile's own data.",
        },
    })
}

fn data_label(state: gpui_maps::render::TileDataState) -> &'static str {
    use gpui_maps::render::TileDataState as S;
    match state {
        S::Loaded => "loaded",
        S::Decoded => "decoded",
        S::Pending => "pending",
        S::Absent => "absent",
        S::Failed => "failed",
        S::Unknown => "unknown",
    }
}

fn draw_label(source: gpui_maps::render::TileDrawSource) -> &'static str {
    use gpui_maps::render::TileDrawSource as S;
    match source {
        S::Own => "own",
        S::Ancestor { .. } => "ancestor",
        S::Descendant => "descendant",
        S::Nothing => "nothing",
    }
}

/// Skip notices grouped by cause rather than listed per layer -- "37 layers
/// skipped" says nothing, "37 skipped, all of them `unsupported layer type
/// \"model\"`" says the style needs a feature this renderer does not have,
/// and a spread across five causes says something else entirely.
///
/// The notice's own text is `skipping layer "id": <cause>`, so the cause is
/// what follows the quoted id. A notice that does not match that shape is
/// bucketed under its whole text rather than dropped.
fn skip_causes(diagnostics: &[gpui_maps::style_spec::StyleDiagnostic]) -> std::collections::BTreeMap<String, u64> {
    let mut causes = std::collections::BTreeMap::<String, u64>::new();
    for diagnostic in diagnostics {
        if diagnostic.kind != gpui_maps::style_spec::DiagnosticKind::SkippedLayer {
            continue;
        }
        let cause = diagnostic.detail.split_once("\": ").map_or(diagnostic.detail.as_str(), |(_, rest)| rest);
        *causes.entry(cause.to_string()).or_default() += 1;
    }
    causes
}

fn tiles(map: &MapView, filter: Option<&str>) -> Value {
    let sample = map.tile_status_sample();
    let rows: Vec<Value> = sample
        .statuses
        .iter()
        .filter(|status| filter.is_none_or(|wanted| data_label(status.data) == wanted))
        .map(|status| {
            json!({
                "z": status.tile.z,
                "x": status.tile.x,
                "y": status.tile.y,
                "data": data_label(status.data),
                "draw": draw_label(status.draw),
                "ancestor_zoom_distance": match status.draw {
                    gpui_maps::render::TileDrawSource::Ancestor { zoom_distance } => Some(zoom_distance),
                    _ => None,
                },
                "label": status.label(),
            })
        })
        .collect();
    json!({
        "recording": map.tile_status_recording(),
        "sample_from_paint": sample.paints_entered,
        "paints_entered": gpui_maps::render::paint_census().paints_entered,
        "filter": filter,
        "count": rows.len(),
        "tiles": rows,
        "note": if sample.statuses.is_empty() {
            "Empty. Either recording is off (POST /map/tile-status-recording/true), or \
             `paint` has not run since it was turned on -- check paints_entered."
        } else {
            "sample_from_paint is the paints_entered count this record was taken on; \
             if it lags the current one, the window has stopped painting."
        },
    })
}

fn layers(map: &MapView) -> Value {
    let diagnostics = gpui_maps::style_spec::compile_diagnostics();
    let of_kind = |kind: gpui_maps::style_spec::DiagnosticKind| {
        diagnostics.iter().filter(|d| d.kind == kind).collect::<Vec<_>>()
    };
    json!({
        "compiled": map.style().layers.iter().map(|layer| {
            let (min_zoom, max_zoom) = layer.zoom_range();
            json!({ "id": layer.id(), "kind": layer.kind(), "min_zoom": min_zoom, "max_zoom": max_zoom })
        }).collect::<Vec<_>>(),
        "skipped": of_kind(gpui_maps::style_spec::DiagnosticKind::SkippedLayer),
        "hidden": of_kind(gpui_maps::style_spec::DiagnosticKind::HiddenLayer),
    })
}

/// `answer`'s `GalleryOpen` arm, factored into a pure decision so it has a
/// unit test that never touches this file's own process-global hooks --
/// installing one for real in a test would race
/// `no_hook_is_installed_by_default_in_this_test_binary`, which depends on
/// nothing else in this binary ever calling `set_gallery_open_hook`. The
/// real call site (`answer`, below) supplies `has_hook`/`id_is_known` from
/// the actual statics and does the queueing side effect itself; this fn
/// only shapes the response.
fn gallery_open_response(id: &str, has_hook: bool, id_is_known: bool, id_is_unavailable: bool, ids: &[String]) -> Value {
    if !has_hook {
        return json!({
            "ok": false,
            "error": "not a gallery",
            "hint": "this process never called agentic::set_gallery_open_hook -- only the gallery example does",
        });
    }
    if !id_is_known {
        let close = did_you_mean(id, ids, 3);
        let hint = match close.first() {
            Some(best) => format!("{id:?} is not a registered demo id or alias; did you mean {best:?}?"),
            None => format!("{id:?} is not a registered demo id or alias; `ids` lists every one"),
        };
        return json!({
            "ok": false,
            "error": "unknown id",
            "hint": hint,
            "did_you_mean": close,
            "ids": ids,
        });
    }
    if id_is_unavailable {
        return json!({
            "ok": false,
            "error": "not available",
            "hint": format!(
                "{id:?} needs a token that reaches it, and none is configured (or the probe has not answered yet); \
                 it is hidden in the sidebar until it does"
            ),
        });
    }
    json!({
        "ok": true,
        "queued": format!("gallery_open({id})"),
        "how_to_verify": "GET /gallery/state: `open_id` is the new panel once it has opened (a tick or two); GET /ui/state has it as `panel`",
    })
}

/// Runs one parsed request against the live map. `&mut MapView` is exactly
/// the access a mouse click already has, which is the whole point.
fn answer(map: &mut MapView, request: MapRequest) -> Value {
    match request {
        MapRequest::State { fields } => select_fields(state(map), fields.as_deref()),
        MapRequest::Tiles { state_filter } => tiles(map, state_filter.as_deref()),
        MapRequest::Layers => layers(map),
        MapRequest::Diagnostics => {
            let diagnostics = gpui_maps::style_spec::compile_diagnostics();
            let count = |kind: gpui_maps::style_spec::DiagnosticKind| {
                diagnostics.iter().filter(|d| d.kind == kind).count()
            };
            json!({
                "diagnostics": diagnostics,
                "counts": {
                    "skipped-layer": count(gpui_maps::style_spec::DiagnosticKind::SkippedLayer),
                    "hidden-layer": count(gpui_maps::style_spec::DiagnosticKind::HiddenLayer),
                    "degraded-property": count(gpui_maps::style_spec::DiagnosticKind::DegradedProperty),
                },
            })
        }
        MapRequest::Stats => {
            let mut value = serde_json::to_value(gpui_maps::render::allocation_stats()).unwrap_or_else(|_| json!({}));
            if let Some(object) = value.as_object_mut() {
                object.insert("paint".to_string(), serde_json::to_value(gpui_maps::render::paint_census()).unwrap_or(Value::Null));
            }
            value
        }
        MapRequest::Functions => json!({
            "crate_version": gpui_maps::agent::CRATE_VERSION,
            "namespace": gpui_maps::agent::NAMESPACE,
            "functions": gpui_maps::agent::agent_functions(),
        }),
        MapRequest::Call { name, args } => match gpui_maps::agent::agent_dispatch(map, &name, args) {
            Ok(result) => json!({ "ok": true, "result": result }),
            Err(error) => json!({ "ok": false, "error": error }),
        },
        MapRequest::SetCamera { lon, lat, zoom } => {
            map.set_camera(lon, lat, zoom);
            json!({ "ok": true, "camera": { "lon": lon, "lat": lat, "zoom": zoom } })
        }
        MapRequest::Ease { lon, lat, zoom, ms } => {
            let target = gpui_maps::camera_control::CameraTarget {
                center: Some((lon, lat)),
                zoom: Some(zoom),
                ..Default::default()
            };
            map.ease_to(target, std::time::Duration::from_secs_f64(ms / 1000.0));
            json!({ "ok": true, "ease": { "lon": lon, "lat": lat, "zoom": zoom, "ms": ms } })
        }
        MapRequest::FlyTo { lon, lat, zoom, pitch, bearing, ms } => {
            let target = gpui_maps::camera_control::CameraTarget {
                center: Some((lon, lat)),
                zoom: Some(zoom),
                pitch: pitch.map(|d| d.to_radians() as f32),
                bearing: bearing.map(|d| d.to_radians() as f32),
                ..Default::default()
            };
            match ms {
                Some(ms) => map.ease_to(target, std::time::Duration::from_secs_f64(ms / 1000.0)),
                None => map.jump_to(target),
            }
            json!({ "ok": true, "camera": { "lon": lon, "lat": lat, "zoom": zoom, "pitch_degrees": pitch, "bearing_degrees": bearing }, "eased_ms": ms })
        }
        MapRequest::SetTileStatusRecording { on } => {
            map.set_tile_status_recording(on);
            json!({ "ok": true, "tile_status_recording": on })
        }
        MapRequest::FrameLog { start } => frame_log_answer(start),
        MapRequest::SetDebugTileOverlay { on } => {
            map.set_debug_tile_overlay(on);
            json!({ "ok": true, "debug_tile_overlay": on })
        }
        MapRequest::NavCamera { mode } => {
            map.set_nav_camera_mode(mode);
            json!({
                "ok": true,
                "nav_camera": map.nav_camera_mode().name(),
                "camera_mode": map.nav_state().map(|s| format!("{:?}", s.camera_mode)),
            })
        }
        MapRequest::Refresh => {
            // Needs a `Window`, which this fn has no route to. Queued for
            // the poll loop, which has one, and acked immediately -- the
            // caller finds out whether a frame happened by reading
            // `paint.paints_entered` back from GET /map/state, which is
            // the honest answer anyway: asking for a redraw and getting
            // one are different events.
            if let Ok(mut queue) = PENDING_WINDOW_WORK.lock() {
                queue.push(WindowWork::Refresh);
            }
            json!({
                "ok": true,
                "queued": "refresh",
                "how_to_verify": "GET /map/state and compare paint.paints_entered",
            })
        }
        MapRequest::Input(gesture) => {
            // Same queue, same reason as `Refresh`: `Window::dispatch_event`
            // needs a `Window`. The camera is sampled HERE, before the
            // gesture runs, so `last_gesture` can report a before/after pair
            // that no later call can have disturbed.
            let before = camera_json(map.camera());
            if let Ok(mut queue) = PENDING_WINDOW_WORK.lock() {
                queue.push(WindowWork::Gesture(gesture.clone()));
            }
            if let Ok(mut last) = LAST_GESTURE.lock() {
                *last = Some(json!({
                    "gesture": format!("{gesture:?}"),
                    "before": before,
                    "after": Value::Null,
                    "status": "queued -- read GET /map/state again in a moment",
                }));
            }
            json!({
                "ok": true,
                "queued": format!("{gesture:?}"),
                "how_to_verify": "GET /map/state and read last_gesture.moved",
            })
        }
        MapRequest::GalleryOpen { id } => gallery_open(id),
    }
}

/// `POST /gallery/open`: answered at once, the switch queued for the
/// pump's hook. Needs no map, so it answers with the slot empty too.
fn gallery_open(id: String) -> Value {
    let has_hook = ON_GALLERY_OPEN.lock().map(|hook| hook.is_some()).unwrap_or(false);
    // No hook installed at all: every id is "known" as far as this
    // check goes, since `has_hook` alone already answers `"not a
    // gallery"` below -- there is no registry to be wrong against.
    let id_is_known = !has_hook
        || GALLERY_KNOWN_ID.lock().ok().and_then(|known| known.as_ref().map(|f| f(&id))).unwrap_or(true);
    // Same reasoning: checked only once the id is actually known,
    // since an unknown id already has its own answer below.
    let id_is_unavailable = has_hook
        && id_is_known
        && GALLERY_UNAVAILABLE_ID.lock().ok().and_then(|hook| hook.as_ref().map(|f| f(&id))).unwrap_or(false);
    let ids = if id_is_known {
        Vec::new()
    } else {
        GALLERY_IDS.lock().ok().and_then(|hook| hook.as_ref().map(|f| f())).unwrap_or_default()
    };
    let response = gallery_open_response(&id, has_hook, id_is_known, id_is_unavailable, &ids);
    if has_hook && id_is_known && !id_is_unavailable {
        if let Ok(mut queue) = PENDING_GALLERY_OPEN.lock() {
            queue.push(id);
        }
    }
    response
}

/// Pushes one synthetic gesture at the window as a real AppKit event.
///
/// # Why AppKit and not gpui
///
/// `gpui::Window::dispatch_event` is the obvious route and is unusable:
/// it is `pub`, but its return type `gpui::window::DispatchEventResult` is
/// not, and rustc rejects the call at every call site outside gpui
/// ("type `DispatchEventResult` is private"). No `#[allow]` reaches it --
/// it is a hard privacy error, not a lint.
///
/// So the event is built the way the operating system builds one and
/// handed to `-[NSWindow sendEvent:]`. That is STRICTLY more of the real
/// path than `dispatch_event` would have been: it exercises AppKit's own
/// hit-testing of the window's view hierarchy, gpui's `mouseDown:` /
/// `mouseDragged:` / `mouseUp:` view methods, gpui's own
/// `PlatformInput::from_native` coordinate conversion, and only then the
/// element tree's hitboxes and mouse listeners. The single thing it still
/// does not cover is the WindowServer deciding which window a physical
/// click belongs to.
///
/// Coordinates in, `(x, y)`, are gpui's: logical pixels from the window's
/// TOP-left, the same frame `MouseDownEvent::position` arrives in. The
/// conversion to AppKit's bottom-left `locationInWindow` is done here once
/// -- see `window_point`.
/// The view a gesture is delivered to, captured while the `Window` is still
/// in hand so the gesture itself can be played after the borrow is released.
///
/// The pointer is only ever dereferenced on the same (main) thread, one
/// poll tick later, and gpui documents the view as valid for the window's
/// lifetime -- the `usize` is there to keep the value `Send` for the async
/// task, not to outlive anything.
#[derive(Debug, Clone, Copy)]
pub struct ViewTarget {
    ns_view: usize,
    /// gpui converts a click back with
    /// `content_size().height - locationInWindow.y`, so this must be the
    /// CONTENT height, not the window frame's. It is the same number
    /// `Window::viewport_size` reports, which is what the rest of this
    /// surface publishes, so the two agree by construction.
    content_height: f64,
}

/// Reads the AppKit view out of a live `Window`. `None` on any platform or
/// hierarchy that is not the one this harness knows how to drive.
#[cfg(target_os = "macos")]
fn view_target(window: &gpui::Window) -> Option<ViewTarget> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return None };
    Some(ViewTarget {
        ns_view: appkit.ns_view.as_ptr() as usize,
        content_height: f64::from(window.viewport_size().height),
    })
}

#[cfg(not(target_os = "macos"))]
fn view_target(_window: &gpui::Window) -> Option<ViewTarget> {
    None
}

#[cfg(target_os = "macos")]
fn play_gesture(gesture: &Gesture, target: ViewTarget) {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSScreen, NSView};
    use objc2_core_foundation::CGPoint;

    // SAFETY: the same contract `MapLayer::new` relies on -- gpui documents
    // this as a valid `NSView` for the window's lifetime, and this runs on
    // the main thread, which AppKit requires for all of it.
    let ns_view: &NSView = unsafe { &*(target.ns_view as *const NSView) };
    let Some(ns_window) = ns_view.window() else {
        eprintln!("agentic http: the view is in no window; the gesture went nowhere");
        return;
    };
    let content_height = target.content_height;
    let window_point = |x: f32, y: f32| CGPoint::new(f64::from(x), content_height - f64::from(y));
    let window_number = ns_window.windowNumber();
    if std::env::var("GPUI_MAP_DEBUG_INPUT").is_ok() {
        use objc2::runtime::{NSObject, NSObjectProtocol};
        let obj: &NSObject = ns_view;
        eprintln!(
            "agentic http: view class {:?}, responds to mouseDown: {}, scrollWheel: {}, window key {}, app active {}",
            obj.class().name(),
            obj.respondsToSelector(objc2::sel!(mouseDown:)),
            obj.respondsToSelector(objc2::sel!(scrollWheel:)),
            ns_window.isKeyWindow(),
            objc2_app_kit::NSApplication::sharedApplication(unsafe { objc2::MainThreadMarker::new_unchecked() }).isActive(),
        );
    }
    // Any strictly-increasing double satisfies AppKit here -- nothing in
    // gpui's `from_native` reads it, and click counts come from the event's
    // own field rather than from timing.
    let timestamp = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    };

    match *gesture {
        Gesture::Drag { x, y, dx, dy, steps, shift, ctrl, right } => {
            let mut flags = NSEventModifierFlags::empty();
            if shift {
                flags |= NSEventModifierFlags::Shift;
            }
            if ctrl {
                flags |= NSEventModifierFlags::Control;
            }
            let (down, dragged, up) = if right {
                (NSEventType::RightMouseDown, NSEventType::RightMouseDragged, NSEventType::RightMouseUp)
            } else {
                (NSEventType::LeftMouseDown, NSEventType::LeftMouseDragged, NSEventType::LeftMouseUp)
            };
            let send = |kind: NSEventType, at: CGPoint, click_count: isize| {
                // `context` is the deprecated parameter AppKit asks to be
                // passed nil.
                let event = {
                    NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                        kind,
                        at,
                        flags,
                        timestamp(),
                        window_number,
                        None,
                        0,
                        click_count,
                        if kind == up { 0.0 } else { 1.0 },
                    )
                };
                match event {
                    // Straight to gpui's view, NOT via
                    // `-[NSWindow sendEvent:]`. AppKit swallows a mouse-down
                    // aimed at an INACTIVE application -- it spends that
                    // click on activating the app instead of delivering it,
                    // which is correct behaviour and made `sendEvent:`
                    // deliver nothing at all here (proven: gpui's own
                    // `Window::mouse_position` never moved). Since the whole
                    // point of this harness is to test a map in a window
                    // nobody is looking at, the event goes to the responder
                    // that would have received it.
                    //
                    Some(event) => match kind {
                        NSEventType::LeftMouseDown => ns_view.mouseDown(&event),
                        NSEventType::LeftMouseDragged => ns_view.mouseDragged(&event),
                        NSEventType::RightMouseDown => ns_view.rightMouseDown(&event),
                        NSEventType::RightMouseDragged => ns_view.rightMouseDragged(&event),
                        NSEventType::RightMouseUp => ns_view.rightMouseUp(&event),
                        _ => ns_view.mouseUp(&event),
                    },
                    None => eprintln!("agentic http: AppKit refused to build a {kind:?} event"),
                }
            };
            if right {
                // `+mouseEventWithType:` cannot set the button number, and
                // gpui reads the button from it -- a right event built that
                // way arrives as a LEFT press and pans. So a right drag is
                // built as a `CGEvent` (which carries the button) and
                // wrapped, placed in screen coordinates the way the scroll
                // path explains.
                use objc2_core_graphics::{CGEvent, CGEventType, CGMouseButton};
                let screen_height = NSScreen::mainScreen(unsafe { objc2::MainThreadMarker::new_unchecked() })
                    .map_or(content_height, |s| s.frame().size.height);
                let send_cg = |kind: CGEventType, at: CGPoint| {
                    let screen = CGPoint::new(at.x, screen_height - at.y);
                    let Some(event) = CGEvent::new_mouse_event(None, kind, screen, CGMouseButton::Right) else {
                        eprintln!("agentic http: CoreGraphics refused to build a right-mouse event");
                        return;
                    };
                    match NSEvent::eventWithCGEvent(&event) {
                        Some(event) => match kind {
                            CGEventType::RightMouseDown => ns_view.rightMouseDown(&event),
                            CGEventType::RightMouseDragged => ns_view.rightMouseDragged(&event),
                            _ => ns_view.rightMouseUp(&event),
                        },
                        None => eprintln!("agentic http: AppKit refused to wrap the right-mouse event"),
                    }
                };
                send_cg(CGEventType::RightMouseDown, window_point(x, y));
                for step in 1..=steps {
                    let t = step as f32 / steps as f32;
                    send_cg(CGEventType::RightMouseDragged, window_point(x + dx * t, y + dy * t));
                }
                send_cg(CGEventType::RightMouseUp, window_point(x + dx, y + dy));
                return;
            }
            send(down, window_point(x, y), 1);
            for step in 1..=steps {
                let t = step as f32 / steps as f32;
                send(dragged, window_point(x + dx * t, y + dy * t), 1);
            }
            send(up, window_point(x + dx, y + dy), 1);
        }
        Gesture::Move { x, y } => {
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType::MouseMoved,
                window_point(x, y),
                NSEventModifierFlags::empty(),
                timestamp(),
                window_number,
                None,
                0,
                0,
                0.0,
            );
            match event {
                // Straight to gpui's view, for the drag's reason.
                Some(event) => ns_view.mouseMoved(&event),
                None => eprintln!("agentic http: AppKit refused to build a mouse-moved event"),
            }
        }
        Gesture::Key { ref key, shift, cmd } => {
            use objc2_foundation::NSString;
            let Some((code, chars)) = key_code(key) else { return };
            let mut flags = NSEventModifierFlags::empty();
            if shift {
                flags |= NSEventModifierFlags::Shift;
            }
            if cmd {
                flags |= NSEventModifierFlags::Command;
            }
            let characters = NSString::from_str(chars);
            let send = |kind: NSEventType| {
                // `context` is the deprecated parameter AppKit asks to be
                // passed nil.
                let event = {
                    NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                        kind,
                        CGPoint::new(0.0, 0.0),
                        flags,
                        timestamp(),
                        window_number,
                        None,
                        &characters,
                        &characters,
                        false,
                        code,
                    )
                };
                match event {
                    // Same reasoning as the mouse path: straight to gpui's
                    // view, because AppKit routes a key event to the KEY
                    // window and this window is deliberately not it.
                    Some(event) => match kind {
                        NSEventType::KeyDown => ns_view.keyDown(&event),
                        _ => ns_view.keyUp(&event),
                    },
                    None => eprintln!("agentic http: AppKit refused to build a {kind:?} event"),
                }
            };
            send(NSEventType::KeyDown);
            send(NSEventType::KeyUp);
        }
        Gesture::Scroll { x, y, lines, dx_lines, alt, shift, pixels, steps } => {
            use objc2_core_graphics::{CGEvent, CGEventFlags, CGScrollEventUnit};

            // AppKit publishes no `+scrollWheelEventWithType:`, so the
            // event is built as a `CGEvent` and converted. A converted
            // event belongs to no window, and `-locationInWindow` on a
            // windowless event answers in SCREEN coordinates -- so the
            // CGEvent's own location (top-left origin, screen) is chosen
            // such that the screen point it reads back as is numerically
            // the window point gpui needs. Ugly, exact, and the reason the
            // conversion is written out rather than inlined.
            // A `pixels` scroll is continuous (`hasPreciseScrollingDeltas`,
            // gpui's `ScrollDelta::Pixels`), the way a trackpad swipe
            // arrives: 100 px per line, split over `steps` events.
            let (unit, count, per_y, per_x) = if pixels {
                let n = steps.max(1);
                (CGScrollEventUnit::Pixel, n, (lines * 100.0 / n as f32).round() as i32, (dx_lines * 100.0 / n as f32).round() as i32)
            } else {
                (CGScrollEventUnit::Line, 1, lines as i32, dx_lines as i32)
            };
            let mut flags = CGEventFlags::empty();
            if alt {
                flags |= CGEventFlags::MaskAlternate;
            }
            if shift {
                flags |= CGEventFlags::MaskShift;
            }
            let screen_height = NSScreen::mainScreen(unsafe { objc2::MainThreadMarker::new_unchecked() })
                .map_or(content_height, |s| s.frame().size.height);
            let target = window_point(x, y);
            for _ in 0..count {
                let Some(event) = CGEvent::new_scroll_wheel_event2(None, unit, 2, per_y, per_x, 0) else {
                    eprintln!("agentic http: CoreGraphics refused to build a scroll event");
                    return;
                };
                CGEvent::set_location(Some(&event), CGPoint::new(target.x, screen_height - target.y));
                CGEvent::set_flags(Some(&event), flags);
                // SAFETY: a live `CGEvent` of a type AppKit knows how to wrap.
                match NSEvent::eventWithCGEvent(&event) {
                    Some(event) => ns_view.scrollWheel(&event),
                    None => eprintln!("agentic http: AppKit refused to wrap the scroll event"),
                }
            }
        }
    }
}

/// Everywhere else there is no synthetic-input path yet: this harness is
/// used on macOS, and a stub that silently reported "did not move" would be
/// worse than one that says why.
#[cfg(not(target_os = "macos"))]
fn play_gesture(_gesture: &Gesture, _target: ViewTarget) {
    eprintln!("agentic http: synthetic input is implemented for macOS only");
}


/// Writes the before/after pair into [`LAST_GESTURE`].
///
/// `moved` is the field a caller reads. It is deliberately a single
/// boolean over all five camera components: a pan that only changed the
/// longitude and a rotate that only changed the bearing are both "the
/// gesture reached the map", and the componentwise numbers are right there
/// for anyone who needs to know which.
fn record_gesture(
    gesture: &Gesture,
    before: &gpui_maps::render::Camera,
    after: &gpui_maps::render::Camera,
    pointer_before: gpui::Point<gpui::Pixels>,
    pointer_after: gpui::Point<gpui::Pixels>,
    drifting_before: bool,
) {
    let moved = camera_moved(before, after);
    if let Ok(mut last) = LAST_GESTURE.lock() {
        *last = Some(json!({
            "gesture": format!("{gesture:?}"),
            "before": camera_json(before),
            "after": camera_json(after),
            "moved": moved,
            // True means `moved` proves nothing: the camera was ALREADY in
            // motion when the gesture was played (an earlier pan's inertia
            // glide, or an eased zoom still running), so any difference
            // between before and after may be that motion rather than this
            // gesture. Wait a couple of seconds and fire again.
            "drifting_before": drifting_before,
            "pointer_before": [f32::from(pointer_before.x), f32::from(pointer_before.y)],
            "pointer_after": [f32::from(pointer_after.x), f32::from(pointer_after.y)],
            "pointer_reached_gpui": pointer_before != pointer_after,
            "note": if moved {
                "The gesture reached the map through gpui's real hit test. If a human's \
                 mouse still does nothing, the fault is between AppKit and the window, \
                 not in the element tree."
            } else {
                "The gesture was dispatched and the camera did not move. Check that \
                 paint.paints_entered > 0 (a window that has never painted has no \
                 hitboxes to hit), that the coordinates are inside the window, and \
                 that nothing is painted over the map at that point."
            },
        }));
    }
}

/// Whether two camera samples differ at all.
///
/// One boolean over all five components: a pan that only changed longitude
/// and a rotate that only changed bearing are both "something reached the
/// map", and the componentwise numbers are published alongside for anyone
/// who needs to know which. The epsilon is loose enough that f32/f64 noise
/// is not a move and tight enough that a one-pixel pan at z15 (~1e-6
/// degrees) is.
fn camera_moved(a: &gpui_maps::render::Camera, b: &gpui_maps::render::Camera) -> bool {
    const EPS: f64 = 1e-9;
    (b.center_lon - a.center_lon).abs() > EPS
        || (b.center_lat - a.center_lat).abs() > EPS
        || (b.zoom - a.zoom).abs() > EPS
        || f64::from((b.bearing - a.bearing).abs()) > EPS
        || f64::from((b.pitch - a.pitch).abs()) > EPS
}

/// The camera as a comparable record, for `last_gesture`'s before/after.
fn camera_json(camera: &gpui_maps::render::Camera) -> Value {
    json!({
        "lon": camera.center_lon,
        "lat": camera.center_lat,
        "zoom": camera.zoom,
        "bearing": camera.bearing,
        "pitch": camera.pitch,
    })
}

/// Shared by both this file's [`ModuleRegistration`]s: parse, then answer,
/// or a `{"error": ...}` for anything malformed -- never a panic on the
/// gpui thread.
/// `POST /map/frame-log/{start,take}`'s answer. See the `frame_log` action.
#[cfg(feature = "probe")]
fn frame_log_answer(start: bool) -> Value {
    if start {
        gpui_maps::render::frame_log_start();
        return json!({ "ok": true, "frame_log": "started" });
    }
    let samples = gpui_maps::render::frame_log_take();
    let origin = samples.first().map(|s| s.ended_at);
    let rows: Vec<Value> = samples
        .iter()
        .map(|s| {
            let t = origin.map_or(0.0, |o| s.ended_at.duration_since(o).as_secs_f64() * 1e3);
            json!({ "t_ms": t, "paint_ms": s.paint_nanos as f64 / 1e6, "acquire_ms": s.acquire_nanos as f64 / 1e6 })
        })
        .collect();
    json!({ "ok": true, "frames": rows })
}

#[cfg(not(feature = "probe"))]
fn frame_log_answer(_start: bool) -> Value {
    json!({ "error": "frame_log needs the `probe` feature" })
}

fn dispatch_common(map: &mut MapView, action_name: &str, path_param: Option<&str>, query: &str, body: &str) -> Value {
    match parse_action(action_name, path_param, query, body) {
        Some(request) => answer(map, request),
        None => json!({
            "error": "could not parse that call",
            "action": action_name,
            "hint": "GET / lists every action with its params and where each one rides",
        }),
    }
}

/// This module's [`ModuleRegistration`]. `T` is `MapView` because that is
/// what the poll loop has a `&mut` to -- the same access a click has.
pub fn registration() -> ModuleRegistration<MapView> {
    ModuleRegistration { prefix: PREFIX, actions: ACTIONS, dispatch: dispatch_common }
}

/// `POST /gallery/open`'s own registry -- a SEPARATE module from
/// [`registration`]'s `map` prefix, so the route is `/gallery/open` (an
/// agent switching panels is talking to the gallery, not to any one map),
/// never `/map/gallery/open`. [`spawn`] registers this unconditionally in
/// every example: the ones that are not a gallery still answer
/// [`MapRequest::GalleryOpen`]'s `"not a gallery"`, a reason rather than a
/// bare unrouted 404.
pub const GALLERY_ACTIONS: &[Action] = &[
Action {
    name: "gallery_open",
    method: "POST",
    path: "/open",
    description: "\
Switch the gallery's one open panel to `id` (a demo id, or one of its \
standalone-example aliases) -- the same switch cmd-down/cmd-up/a sidebar \
click drives, reachable here so an agent can pick a panel without a human \
at the keyboard. Body ONLY, e.g. {\"id\": \"streets\"} -- a query-string id \
does not parse. A browser page cannot call this: the port refuses a \
browser Origin and a non-loopback Host with 403. An unknown id answers \
404 `unknown id` with `did_you_mean` (the closest ids) and `ids` (all). Answers `not a gallery` in every OTHER example (this process \
never installed the switch, see `set_gallery_open_hook`). Needs a map \
already showing: an empty slot answers `no map is active`, the same as \
any other call on this port.",
    params: &[
        Param {
            name: "id",
            kind: "string",
            location: ParamLocation::Body,
            description: "A demo id or alias, as `--open ID` takes.",
        },
        Param {
            name: "entry",
            kind: "string",
            location: ParamLocation::Body,
            description: "The same as `id` (the overview's name for a card).",
        },
    ],
},
Action {
    name: "gallery_scroll_to",
    method: "POST",
    path: "/scroll_to",
    description: "\
Shows the Gallery overview (every demo as a card) scrolled to a section, as \
a click on that section in the sidebar does. Body ONLY, e.g. \
{\"section\": \"big-data\"}: a section key (basemaps, big-data, \
data-formats, navigation, overlays, engine), `big-data/samples`, or \
`top`. Answers `queued`; the overview and the lit section are in \
GET /map/state's `gallery` section (and GET /gallery/state) on the next \
frame. `unknown section` for anything else; `not a gallery` outside Studio.",
    params: &[Param {
        name: "section",
        kind: "string",
        location: ParamLocation::Body,
        description: "A section key, big-data/samples, or top.",
    }],
},
Action {
    name: "gallery_state",
    method: "GET",
    path: "/state",
    description: "The Gallery overview: whether it shows, the lit section, the open demo, \
and every section with its cards (id, title, SDK line, has_preview). Also under `gallery` in GET /map/state. \
`?fields=open_id,ids` answers only those keys; `ids` is every card's id (a few hundred bytes, not the ~20 KB of sections).",
    params: &[Param {
        name: "fields",
        kind: "string",
        location: ParamLocation::Query,
        description: "Optional: comma-separated keys (open_id, page, overview, active_section, sections, ids); everything when absent.",
    }],
},
];

/// The gallery page's own actions, answered by [`set_gallery_page_hook`]'s
/// hook rather than [`parse_action`].
fn is_gallery_page_action(name: &str) -> bool {
    matches!(name, "gallery_scroll_to" | "gallery_state")
}

/// [`GALLERY_ACTIONS`]' dispatch: the page actions go to the gallery's own
/// hook ([`set_gallery_page_hook`]), `gallery_open` to the common path.
fn dispatch_gallery(map: &mut MapView, action_name: &str, path_param: Option<&str>, query: &str, body: &str) -> Value {
    match action_name {
        name if is_gallery_page_action(name) => match GALLERY_PAGE.lock().ok().as_deref().and_then(Option::as_ref) {
            Some(hook) if action_name == "gallery_state" => gallery_state_view(hook(action_name, body), query),
            Some(hook) => hook(action_name, body),
            None => json!({"ok": false, "error": "not a gallery"}),
        },
        _ => dispatch_common(map, action_name, path_param, query, body),
    }
}

/// `GET /gallery/state?fields=...`: [`select_fields`] over the gallery's
/// state, with `ids` (every card's id, in order) derived from `sections`.
pub fn gallery_state_view(full: Value, query: &str) -> Value {
    let fields: Option<Vec<String>> = gpui_agentic_http::parse_query_string(query)
        .into_iter()
        .find(|(k, _)| k == "fields")
        .map(|(_, v)| v.split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect::<Vec<_>>())
        .filter(|f| !f.is_empty());
    let Some(fields) = fields else { return full };
    let mut full = full;
    if fields.iter().any(|f| f == "ids") {
        let ids: Vec<Value> = full
            .get("sections")
            .and_then(Value::as_array)
            .map(|ss| ss.iter().flat_map(|s| s.get("cards").and_then(Value::as_array).cloned().unwrap_or_default()).filter_map(|c| c.get("id").cloned()).collect())
            .unwrap_or_default();
        if let Value::Object(m) = &mut full {
            m.insert("ids".into(), Value::Array(ids));
        }
    }
    select_fields(full, Some(&fields))
}

/// This module's second [`ModuleRegistration`]. See [`GALLERY_ACTIONS`].
pub fn gallery_registration() -> ModuleRegistration<MapView> {
    ModuleRegistration { prefix: "gallery", actions: GALLERY_ACTIONS, dispatch: dispatch_gallery }
}

// ---------------------------------------------------------------------------
// Starting it
// ---------------------------------------------------------------------------

/// The port this process serves, once bound (0 before or without one).
static SERVED_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// The loopback port this process's agentic API listens on, if any
/// (Studio's "Integrate with Claude Code" status).
#[allow(dead_code, reason = "Studio's; the standalone examples do not read it")]
pub fn served_port() -> Option<u16> {
    Some(SERVED_PORT.load(Relaxed)).filter(|p| *p != 0)
}

/// The agentic port's setting: `GPUI_MAP_AGENTIC_PORT`, else the old name
/// `GPUI_MAP_DEBUG_PORT`.
fn port_setting() -> Option<String> {
    std::env::var("GPUI_MAP_AGENTIC_PORT").ok().or_else(|| std::env::var("GPUI_MAP_DEBUG_PORT").ok())
}

/// Studio's agentic port: always on. `GPUI_MAP_AGENTIC_PORT` when set
/// (a bad value is fatal, as in [`spawn`]); otherwise the first port of
/// 7334-7339 that binds, so a second Studio gets the next one rather than
/// no port. `None` only when none binds.
#[allow(dead_code, reason = "Studio's; basic_map and mapbox_standard_dark share this module and use the opt-in `spawn`")]
pub fn spawn_always(provenance: Provenance) -> Option<std::sync::mpsc::Receiver<AgenticCall<MapView>>> {
    if port_setting().is_some() {
        return spawn(provenance);
    }
    gpui_maps::render::note_process_start();
    if let Ok(mut slot) = PROVENANCE.lock() {
        *slot = Some(provenance);
    }
    for port in DEFAULT_AGENTIC_PORT..=7339 {
        if let Some(receiver) = serve(port) {
            return Some(receiver);
        }
    }
    eprintln!("agentic http: no free port in {DEFAULT_AGENTIC_PORT}-7339; the agentic API is off");
    None
}

fn serve(port: u16) -> Option<std::sync::mpsc::Receiver<AgenticCall<MapView>>> {
    // Kit's own accept loop (`agentic_gate`), not `AgenticServer::spawn`:
    // the same modules and `GET /` document, with the Origin/Host checks
    // and the real status codes the crate's loop does not have.
    let receiver = crate::agentic_gate::spawn(APP_NAME, port, port_modules())?;
    SERVED_PORT.store(port, Relaxed);
    eprintln!("agentic http: listening on http://127.0.0.1:{port}");
    eprintln!("agentic http:   GET  http://127.0.0.1:{port}/            every action, self-describing");
    eprintln!("agentic http:   GET  http://127.0.0.1:{port}/map/state   what this map is actually doing");
    eprintln!("agentic http: unauthenticated and mutating, bound to 127.0.0.1 only; browser origins and non-loopback hosts get 403.");
    Some(receiver)
}

/// Every module the port serves: kit's own, then the app's
/// ([`add_module`]), in order.
fn port_modules() -> Vec<ModuleRegistration<MapView>> {
    let mut modules = base_modules();
    for extra in EXTRA_MODULES.lock().map(|m| m.clone()).unwrap_or_default() {
        let module = extra();
        debug_assert!(!modules.iter().any(|m| m.prefix == module.prefix), "two agentic modules registered the prefix {:?}", module.prefix);
        modules.push(module);
    }
    modules
}

/// Kit's own modules, before any app's (`map`, `gallery`, `story`,
/// `capture`). Studio's docs test lists the tools from these plus its own.
#[allow(dead_code, reason = "Studio's docs test reads it; the standalone examples only serve")]
pub fn base_modules() -> Vec<ModuleRegistration<MapView>> {
    vec![registration(), gallery_registration(), crate::story_agentic::story_registration(), crate::story_agentic::capture_registration()]
}

/// Binds the port and starts the accept loop, if `GPUI_MAP_AGENTIC_PORT`
/// (or the old `GPUI_MAP_DEBUG_PORT`) is set. `None` -- no socket, no
/// thread -- otherwise.
///
/// Call BEFORE opening the window. Two reasons, and the second is the one
/// that matters: a port that cannot be bound should fail before a 1-4 GB
/// window exists, and [`gpui_maps::render::note_process_start`] called here
/// makes `uptime_s` mean "since launch" rather than "since the first
/// frame", which is exactly the interval a run that never painted needs
/// reported.
pub fn spawn(provenance: Provenance) -> Option<std::sync::mpsc::Receiver<AgenticCall<MapView>>> {
    let setting = port_setting()?;
    let port = if setting.trim().is_empty() {
        DEFAULT_AGENTIC_PORT
    } else {
        match setting.trim().parse::<u16>() {
            Ok(port) => port,
            // Fatal rather than falling back to the default: a typo'd port
            // that silently listened somewhere else would have the
            // operator curling a closed socket and concluding the harness
            // is broken. Carried over from the harness this replaces.
            Err(error) => panic!("GPUI_MAP_AGENTIC_PORT={setting:?} is not a port number: {error}"),
        }
    };

    gpui_maps::render::note_process_start();
    if let Ok(mut slot) = PROVENANCE.lock() {
        *slot = Some(provenance);
    }

    serve(port)
}

/// Drains queued calls on gpui's own thread, where the `MapView` is, and
/// publishes the two facts only a `Window` knows.
///
/// **`cx.update_window`, not `cx.notify()`.** This is a fix a downstream app
/// measured directly and this harness inherits: with a window in the
/// background -- exactly the state an agent-driven instance sits in -- 130
/// `cx.notify()` calls over eight seconds produced two real renders.
/// `notify` marks an entity dirty; it does not make gpui paint, and nothing
/// else is asking a backgrounded window to. `update_window` reaches into
/// the window and runs a closure whether or not a paint was ever going to
/// happen, which is what keeps this surface answering at all on a window
/// nobody is looking at.
///
/// Note what that does and does not buy: the harness stays responsive on a
/// backgrounded window, but the MAP still does not paint, and so still does
/// not fetch tiles. That is not hidden -- it is the `paints_entered` figure
/// in `GET /map/state`, and telling those two states apart is the entire
/// reason this exists.
///
/// Whether `current` (this tick's `resolve_slot`, `None` if the slot is
/// empty) is a live map [`pump_slot`] has not already armed recording for
/// -- the trigger for re-firing `set_tile_status_recording(true)`. Pulled
/// out as a pure function, over bare `EntityId`s rather than a live
/// `Entity`/`Window`, so this decision has a unit test that needs neither.
///
/// `current.is_some()`: going TO empty is never itself the trigger --
/// there is nothing live to arm recording on -- only arriving at a live
/// entity that was not the one last tick armed it for (a brand new panel,
/// or the very first one, when `last` is `None`).
fn map_changed(last: Option<gpui::EntityId>, current: Option<gpui::EntityId>) -> bool {
    current.is_some() && current != last
}

/// A thin wrapper over [`pump_slot`] for the single-map examples: `map`
/// never changes for the life of this call, so a [`ViewSlot`] built once
/// here and never written again is exactly a fixed `Entity`.
#[allow(dead_code)] // every example but the gallery, which calls `pump_slot`
pub fn pump(
    map: gpui::Entity<MapView>,
    window_handle: gpui::AnyWindowHandle,
    pending: std::sync::mpsc::Receiver<AgenticCall<MapView>>,
    cx: &mut gpui::App,
) {
    pump_slot(std::rc::Rc::new(std::cell::RefCell::new(Some(map.downgrade()))), window_handle, pending, cx);
}

/// The same loop [`pump`] runs, over a [`ViewSlot`] rather than one fixed
/// `Entity` -- so the debug port keeps answering as the gallery switches
/// panels, always against whichever `MapView` is live right now rather
/// than the one that was live when this was first called (the task this
/// function was added for: "the debug port follows the active panel").
///
/// Every action needing `&mut MapView` (`state`, `camera`, the gestures,
/// `gallery_open` included) answers `{"error": "no map is active"}` for
/// the whole batch queued this tick when the slot resolves to `None` --
/// empty, or a panel that has since been dropped (`WeakEntity::upgrade`
/// failing is "dead", treated the same as "never had one").
pub fn pump_slot(
    slot: ViewSlot,
    window_handle: gpui::AnyWindowHandle,
    pending: std::sync::mpsc::Receiver<AgenticCall<MapView>>,
    cx: &mut gpui::App,
) {
    // Best-effort, purely an optimization: if something is already live at
    // this exact instant, arm recording immediately rather than waiting up
    // to one `POLL` interval for the loop below to notice. The loop's own
    // `map_changed` check independently re-fires this for ANY newly-live
    // entity -- including this one, if this eager attempt found nothing
    // yet (normal for the gallery: its first panel has not even drawn a
    // frame at this point) -- so correctness never depends on this block.
    let initial_map_id = resolve_slot(&slot).map(|map| {
        map.update(cx, |view, _cx| view.set_tile_status_recording(true));
        map.entity_id()
    });

    cx.spawn(async move |cx| {
        // Which entity recording was last armed for, across ticks --
        // `None` counts as "nothing armed for yet", so the very first
        // live map (not just the second, third, ...) is `map_changed`.
        let mut last_map_id = initial_map_id;
        loop {
        cx.background_executor().timer(POLL).await;
        let outcome = cx.update_window(window_handle, |_root, window, app_cx| {
            WINDOW_ACTIVE.store(u64::from(window.is_window_active()), Relaxed);
            let size = window.viewport_size();
            WINDOW_SIZE.store(((size.width.to_f64() as u64) << 32) | (size.height.to_f64() as u64 & 0xffff_ffff), Relaxed);
            let pointer = window.mouse_position();
            POINTER.store(
                ((f32::from(pointer.x).to_bits() as u64) << 32) | f32::from(pointer.y).to_bits() as u64,
                Relaxed,
            );
            POLLED.store(true, Relaxed);
            POLL_TICKS.fetch_add(1, Relaxed);

            // Collected before dispatching: `try_recv` holds `pending`
            // borrowed, and `dispatch` wants `&mut MapView`.
            let mut calls = Vec::new();
            while let Ok(call) = pending.try_recv() {
                calls.push(call);
            }
            if let Ok(hook) = ON_SLOT_SYNC.lock() {
                if let Some(hook) = hook.as_ref() {
                    hook(window, app_cx);
                }
            }
            let current = resolve_slot(&slot);
            match current {
                Some(map) => {
                    // Task 6 fix round 1, Finding 4: recording must be
                    // re-armed for EVERY panel that becomes live, the
                    // gallery's very first one included -- almost always
                    // live only after this closure has already run once,
                    // which is exactly what the eager attempt above
                    // cannot cover on its own.
                    let id = map.entity_id();
                    let just_armed = map_changed(last_map_id, Some(id));
                    last_map_id = Some(id);
                    // A story preview on a map that is no longer the live
                    // one (the gallery switched panels) stops, and that
                    // map gets its pre-preview scene back.
                    crate::story_agentic::release_previews_except(Some(id), app_cx);
                    // The story actions' "this map" (a dispatch gets only
                    // `&mut MapView`).
                    crate::story_agentic::set_dispatch_map(Some(&map));
                    map.update(app_cx, |view, entity_cx| {
                        if just_armed {
                            view.set_tile_status_recording(true);
                        }
                        let any = !calls.is_empty();
                        for call in calls {
                            let response = (call.dispatch)(
                                view,
                                call.action_name,
                                call.path_param.as_deref(),
                                &call.query,
                                &call.body,
                            );
                            // The HTTP thread is parked on this. A dropped
                            // receiver only means that client gave up,
                            // which is theirs to handle, not a reason to
                            // panic gpui's foreground thread.
                            // A story or capture call that waits on a
                            // worker answers later, from a thread of its
                            // own: the UI thread never waits on a render.
                            if let Some(reply) = crate::story_agentic::reply_or_defer(&response, call.reply) {
                                let _ = reply.send(response);
                            }
                        }
                        // An offscreen still (`capture.image` with
                        // `layers`) renders a frame of this map per tick
                        // until it settles: no window paint is involved,
                        // so it works behind other windows or hidden.
                        crate::story_agentic::offscreen_tick(id, view, entity_cx);
                        if any {
                            // What `agent_dispatch`'s doc comment says a
                            // host owes it: the write functions leave a
                            // bake owed, and `Render::render` -- which
                            // only runs on a notified entity -- is where
                            // it gets spawned.
                            entity_cx.notify();
                        }
                    });
                    crate::story_agentic::set_dispatch_map(None);
                }
                None => {
                    // Not itself a `map_changed` trigger (see that fn's
                    // own comment), but resetting here means the NEXT
                    // panel to arrive -- even one that happens to reuse an
                    // id gpui has recycled, vanishingly unlikely but not
                    // impossible -- is still treated as new instead of
                    // silently matching a stale `last_map_id`.
                    last_map_id = None;
                    // The last panel closed: its preview stops.
                    crate::story_agentic::release_previews_except(None, app_cx);
                    for call in calls {
                        // Opening a panel needs no map: with none live, it
                        // is the one way out (`POST /gallery/open`).
                        let response = match parse_action(call.action_name, call.path_param.as_deref(), &call.query, &call.body) {
                            Some(MapRequest::GalleryOpen { id }) => gallery_open(id),
                            _ => json!({
                                "error": "no map is active",
                                "hint": "the slot is empty -- between panels, or nothing has opened yet. GET / and POST /gallery/open still answer.",
                            }),
                        };
                        let _ = call.reply.send(response);
                    }
                }
            }

            // Ids `POST /gallery/open` queued this tick or an earlier one.
            // Collected here, run just below, OUTSIDE this closure: the
            // hook re-enters `window_handle` by its own handle, and this
            // closure is already mid-`update_window` for it -- a second
            // call for the SAME window before the first has returned
            // finds it already taken (`App::update_window_id`'s own
            // take/replace) and answers `Err`, silently doing nothing.
            let opened: Vec<String> =
                PENDING_GALLERY_OPEN.lock().map(|mut queue| std::mem::take(&mut *queue)).unwrap_or_default();

            // Drained into a local first: running a gesture re-enters
            // `answer` is not possible, but `dispatch_event` runs arbitrary
            // application listeners, and holding a global `Mutex` across
            // those is how a deadlock gets built.
            let work: Vec<WindowWork> = PENDING_WINDOW_WORK
                .lock()
                .map(|mut queue| queue.drain(..).collect())
                .unwrap_or_default();
            // A gesture must NOT be played here. Feeding an event to the
            // view calls straight back into `Window::dispatch_event`, and
            // gpui reaches that through `handle.update(&mut cx, ..)` --
            // which needs the `App` that this very closure is holding.
            // gpui does not panic on that: `AsyncApp::update` answers `Err`
            // and the callback ends in `.log_err()`, which, with no logger
            // installed, is silence. The event is simply gone.
            //
            // That is not hypothetical -- it is what this harness did for
            // its first three attempts, and `pointer_reached_gpui: false`
            // with a live, painting window is exactly its fingerprint. So
            // the gesture is carried OUT of the closure and played once the
            // borrow has been released.
            let mut queued_gesture = None;
            for item in work {
                match item {
                    WindowWork::Refresh => window.refresh(),
                    WindowWork::Gesture(gesture) => {
                        // No live map to read a camera from: the gesture
                        // has nothing to move and nothing to compare
                        // against, so it is dropped rather than queued.
                        let Some(map) = resolve_slot(&slot) else { continue };
                        // `camera()` returns a `&Camera`; copied out so no
                        // borrow of `app_cx` outlives this statement.
                        let before = *map.read(app_cx).camera();
                        // gpui's `Window` updates `mouse_position` inside
                        // `dispatch_event` itself, before any listener runs.
                        // So this pair is the ONE fact that separates "the
                        // event never reached gpui" from "it reached gpui
                        // and the map ignored it" -- opposite diagnoses,
                        // and guessing between them is how this bug stayed
                        // open for a day.
                        let pointer_before = window.mouse_position();
                        queued_gesture =
                            Some((gesture, before, pointer_before, view_target(window)));
                    }
                }
            }
            (queued_gesture, opened)
        });
        // This ends when the window closes -- `update_window` answers
        // `Err` once its handle no longer resolves -- at which point there
        // is nothing left to drain for, and the task ends with it.
        let Ok((queued_gesture, opened)) = outcome else { return };

        // Outside the closure above (see its own comment): a fresh,
        // non-nested `App` to hand the gallery's own hook.
        cx.update(|app_cx| {
            if let Ok(hook) = ON_TICK.lock() {
                if let Some(hook) = hook.as_ref() {
                    hook(app_cx);
                }
            }
        });
        if !opened.is_empty() {
            cx.update(|app_cx| {
                if let Ok(hook) = ON_GALLERY_OPEN.lock() {
                    if let Some(hook) = hook.as_ref() {
                        for id in &opened {
                            hook(id, app_cx);
                        }
                    }
                }
            });
        }

        // Outside the closure: the `App` borrow is released, so gpui can
        // take it back when the event arrives.
        if let Some((gesture, before, pointer_before, Some(target))) = queued_gesture {
            // The drift control. Releasing a pan leaves the map GLIDING --
            // inertia, easing to a stop over the best part of a second --
            // so a gesture fired shortly after another one is measured
            // against a camera that was already moving, and reports
            // `moved: true` whatever it did. That confound produced a wrong
            // answer once already in this session. Sampling the camera
            // across an interval BEFORE the gesture is what makes it
            // visible instead of silent.
            let drift = {
                let sample = |cx: &mut gpui::AsyncApp| {
                    cx.update_window(window_handle, |_root, _window, app_cx| {
                        resolve_slot(&slot).map(|m| *m.read(app_cx).camera())
                    })
                    .ok()
                    .flatten()
                };
                let first = sample(cx);
                cx.background_executor().timer(DRIFT_WINDOW).await;
                let second = sample(cx);
                match (first, second) {
                    (Some(a), Some(b)) => camera_moved(&a, &b),
                    _ => false,
                }
            };
            // The events run synchronously inside: a control they press
            // sees `synthetic_input()`.
            with_synthetic_input(|| play_gesture(&gesture, target));
            // A pan reaches the camera synchronously; a wheel zoom is eased
            // toward a target over frames, so it needs some to land in.
            // Long enough for the ease at 60 Hz, short enough that a caller
            // polling `state` sees the answer on their next call.
            cx.background_executor().timer(GESTURE_SETTLE).await;
            let _ = cx.update_window(window_handle, |_root, window, app_cx| {
                if let Some(map) = resolve_slot(&slot) {
                    let after = *map.read(app_cx).camera();
                    record_gesture(&gesture, &before, &after, pointer_before, window.mouse_position(), drift);
                }
            });
        }
        }
    })
    .detach();
}

/// How long a gesture is given to land before its outcome is recorded.
const GESTURE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// How long the camera is watched for pre-existing motion before a gesture
/// is played. See the drift control in [`pump`].
const DRIFT_WINDOW: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(test)]
mod tests {
    use super::*;

    /// Every declared action is one [`parse_action`] recognises, and no
    /// `GET` carries a body param. This is the check `gpui-agentic-http`
    /// exists to make possible: a capability the registry advertises but
    /// nothing routes would otherwise surface as a live 404 an agent has no
    /// way to diagnose from `GET /` alone.
    ///
    /// `assert_registry_routable` deliberately does not call `dispatch` --
    /// that needs a real `MapView`, which needs a window and a GPU. Every
    /// action is exercised through `parse_action` instead, which is pure.
    #[test]
    fn every_declared_action_is_routable() {
        gpui_agentic_http::assert_registry_routable(&registration(), |action| {
            parse_action(action.name, sample_path_param(action.name), sample_query(action.name), sample_body(action.name))
                .is_some()
        });
    }

    /// The same drift check, over [`gallery_registration`]'s own module --
    /// a second registry sharing this file's `parse_action`, so it needs
    /// its own proof that `gallery_open` is routable too.
    #[test]
    fn every_declared_gallery_action_is_routable() {
        gpui_agentic_http::assert_registry_routable(&gallery_registration(), |action| {
            is_gallery_page_action(action.name) || parse_action(action.name, None, "", r#"{"id": "streets"}"#).is_some()
        });
    }

    /// The page actions answer "not a gallery" with no hook installed, and
    /// `gallery_open` takes `entry` as well as `id`.
    #[test]
    fn the_gallery_page_actions_need_the_gallery_and_open_takes_entry() {
        assert!(matches!(
            parse_action("gallery_open", None, "", r#"{"entry": "tokyo"}"#),
            Some(MapRequest::GalleryOpen { id }) if id == "tokyo"
        ));
        assert!(is_gallery_page_action("gallery_scroll_to") && is_gallery_page_action("gallery_state"));
        assert!(!is_gallery_page_action("gallery_open"));
    }

    fn sample_path_param(name: &str) -> Option<&'static str> {
        match name {
            "tile_status_recording" | "debug_tile_overlay" => Some("true"),
            "frame_log" => Some("take"),
            "nav_camera" => Some("northup2d"),
            _ => None,
        }
    }

    fn sample_query(name: &str) -> &'static str {
        match name {
            "tiles" => "state=absent",
            _ => "",
        }
    }

    fn sample_body(name: &str) -> &'static str {
        match name {
            "call" => r#"{"name": "map.get_camera"}"#,
            "camera" | "ease" | "fly_to" => r#"{"lon": 139.7671, "lat": 35.6812, "zoom": 14}"#,
            "drag" => r#"{"x": 400, "y": 300, "dx": 120, "dy": 0}"#,
            "scroll" => r#"{"x": 400, "y": 300, "lines": 1}"#,
            "move" => r#"{"x": 400, "y": 300}"#,
            "key" => r#"{"key": "escape"}"#,
            _ => "",
        }
    }

    /// `move` is a buttonless pointer move: the only synthetic input that
    /// reaches the map's hover (a drag holds a button, which pans).
    #[test]
    fn a_move_is_a_buttonless_pointer_move_and_needs_both_coordinates() {
        assert_eq!(
            parse_action("move", None, "", r#"{"x": 700, "y": 400.5}"#),
            Some(MapRequest::Input(Gesture::Move { x: 700.0, y: 400.5 }))
        );
        assert_eq!(parse_action("move", None, "", r#"{"x": 700}"#), None);
    }

    #[test]
    fn the_tiles_filter_is_optional_and_lowercased() {
        assert_eq!(parse_action("tiles", None, "", ""), Some(MapRequest::Tiles { state_filter: None }));
        assert_eq!(
            parse_action("tiles", None, "state=ABSENT", ""),
            Some(MapRequest::Tiles { state_filter: Some("absent".to_string()) })
        );
    }

    #[test]
    fn gallery_state_fields_and_ids() {
        let full = json!({"page": "home", "open_id": "basic", "sections": [{"cards": [{"id": "basic", "title": "B"}, {"id": "streets"}]}, {"cards": [{"id": "geoparquet"}]}]});
        assert_eq!(gallery_state_view(full.clone(), ""), full);
        let small = gallery_state_view(full, "fields=open_id,ids");
        assert_eq!(small, json!({"open_id": "basic", "ids": ["basic", "streets", "geoparquet"]}));
    }

    #[test]
    fn state_fields_keep_only_what_was_asked() {
        assert_eq!(parse_action("state", None, "", ""), Some(MapRequest::State { fields: None }));
        assert_eq!(parse_action("state", None, "fields=camera,ui", ""), Some(MapRequest::State { fields: Some(vec!["camera".into(), "ui".into()]) }));
        let full = json!({"app": "gpui-map", "camera": {"lon": 1}, "ui": {"page": "home"}, "tiles": [1, 2, 3]});
        let some = select_fields(full.clone(), Some(&["camera".to_string()]));
        assert_eq!(some, json!({"app": "gpui-map", "camera": {"lon": 1}}));
        let odd = select_fields(full.clone(), Some(&["nope".to_string()]));
        assert_eq!(odd["unknown_fields"], json!(["nope"]));
        assert_eq!(select_fields(full.clone(), None), full);
    }

    #[test]
    fn a_call_without_args_gets_an_empty_object() {
        assert_eq!(
            parse_action("call", None, "", r#"{"name": "map.get_camera"}"#),
            Some(MapRequest::Call { name: "map.get_camera".to_string(), args: json!({}) })
        );
    }

    #[test]
    fn a_call_without_a_name_does_not_parse() {
        assert_eq!(parse_action("call", None, "", r#"{"args": {}}"#), None);
        assert_eq!(parse_action("call", None, "", "not json"), None);
    }

    #[test]
    fn a_camera_missing_a_coordinate_does_not_parse() {
        assert_eq!(parse_action("camera", None, "", r#"{"lon": 1.0, "zoom": 4}"#), None);
    }

    /// A typo in a boolean path parameter must be a parse failure, not a
    /// silent `true`. See [`parse_bool`]'s own comment.
    #[test]
    fn a_mistyped_boolean_does_not_parse() {
        assert_eq!(parse_action("tile_status_recording", Some("ture"), "", ""), None);
        assert_eq!(
            parse_action("tile_status_recording", Some("OFF"), "", ""),
            Some(MapRequest::SetTileStatusRecording { on: false })
        );
    }

    /// The perf harness's two measurement routes: `frame_log` takes only
    /// `start`/`take`, and `ease` defaults `ms` to 2 s and clamps it.
    #[test]
    fn frame_log_and_ease_parse() {
        assert_eq!(parse_action("frame_log", Some("start"), "", ""), Some(MapRequest::FrameLog { start: true }));
        assert_eq!(parse_action("frame_log", Some("take"), "", ""), Some(MapRequest::FrameLog { start: false }));
        assert_eq!(parse_action("frame_log", Some("stop"), "", ""), None);
        assert_eq!(
            parse_action("ease", None, "", r#"{"lon": 1, "lat": 2, "zoom": 3}"#),
            Some(MapRequest::Ease { lon: 1.0, lat: 2.0, zoom: 3.0, ms: 2000.0 })
        );
        assert_eq!(
            parse_action("ease", None, "", r#"{"lon": 1, "lat": 2, "zoom": 3, "ms": 1e9}"#),
            Some(MapRequest::Ease { lon: 1.0, lat: 2.0, zoom: 3.0, ms: 60_000.0 })
        );
        assert_eq!(parse_action("ease", None, "", r#"{"lon": 1, "zoom": 3}"#), None);
        // fly_to: zoom 12 by default and at once (an ease never lands in a
        // window that does not paint); `ms` eases; pitch is clamped.
        assert_eq!(
            parse_action("fly_to", None, "", r#"{"lon": 139.7, "lat": 35.7}"#),
            Some(MapRequest::FlyTo { lon: 139.7, lat: 35.7, zoom: 12.0, pitch: None, bearing: None, ms: None })
        );
        assert_eq!(
            parse_action("fly_to", None, "", r#"{"lon": 1, "lat": 2, "zoom": 15, "pitch": 120, "bearing": 30, "ms": 800}"#),
            Some(MapRequest::FlyTo { lon: 1.0, lat: 2.0, zoom: 15.0, pitch: Some(85.0), bearing: Some(30.0), ms: Some(800.0) })
        );
        assert_eq!(parse_action("fly_to", None, "", r#"{"lat": 2}"#), None);
    }

    #[test]
    fn an_unknown_action_does_not_parse() {
        assert_eq!(parse_action("capture_screenshot", None, "", ""), None);
    }

    /// Task 6, Step 1 (fix round 1: body, not query -- see the CSRF finding
    /// below): `gallery_open` reads `id` from a JSON body and does not
    /// parse without one.
    #[test]
    fn gallery_open_parses_and_needs_an_id() {
        assert!(matches!(
            parse_action("gallery_open", None, "", r#"{"id": "census"}"#),
            Some(MapRequest::GalleryOpen { id }) if id == "census"
        ));
        assert!(parse_action("gallery_open", None, "", "").is_none());
    }

    /// Fix round 1, Finding 1: the `id` used to ride in the query string,
    /// which closed off nothing -- a `<form action="...?id=census">`
    /// needs no fields at all to hit it. It no longer parses there in any
    /// shape, only in a body that is EXACTLY one JSON object. This closes
    /// that one specific, literal-`<form>` vector; it is NOT a general
    /// CSRF defense -- see this module's own CSRF doc comment for the
    /// `fetch`-based one that remains open.
    #[test]
    fn gallery_open_refuses_a_query_string_id() {
        assert_eq!(parse_action("gallery_open", None, "id=census", ""), None);
        // Even with a body present, a query-string id is not a fallback:
        // only the body is ever consulted.
        assert_eq!(
            parse_action("gallery_open", None, "id=census", r#"{"id": "streets"}"#),
            Some(MapRequest::GalleryOpen { id: "streets".to_string() })
        );
    }

    /// A LITERAL `<form enctype="text/plain">` element -- as against a
    /// script calling `fetch` -- can only ever send `name=value` per
    /// field, one per line; a field named `{"id":"census"}` with an empty
    /// value produces a body of `{"id":"census"}=`, text that CONTAINS
    /// valid JSON but is not, itself, only that JSON, and
    /// `serde_json::from_str` refuses the trailing `=`.
    ///
    /// This is not the CSRF defense (that is `agentic_gate`'s Origin and
    /// Host check, before routing): all this test pins is the narrower
    /// thing, that a `<form>`-encoded body does not parse as the call.
    #[test]
    fn gallery_open_refuses_a_literal_form_encoded_body_shaped_like_json() {
        assert_eq!(parse_action("gallery_open", None, "", "{\"id\":\"census\"}=\n"), None);
        assert_eq!(parse_action("gallery_open", None, "", "{\"id\":\"census\"}="), None);
        // What IS refused by the JSON-body rule is a genuinely malformed
        // body, form-shaped or not -- distinct from the `fetch` case
        // above, whose body is not malformed at all.
    }

    /// `answer`'s `GalleryOpen` arm reads [`ON_GALLERY_OPEN`] before doing
    /// anything else: with no hook installed (this test binary's own
    /// starting state -- nothing in this file calls
    /// `set_gallery_open_hook`), `has_hook` must be `false`, which is what
    /// makes that arm answer "not a gallery" rather than queueing a switch
    /// nothing will ever read. `answer` itself needs a real `MapView` (a
    /// window and a GPU) to call at all, so this checks the one part of
    /// that arm's logic that does not: the hook-presence read.
    #[test]
    fn no_hook_is_installed_by_default_in_this_test_binary() {
        assert!(
            !ON_GALLERY_OPEN.lock().map(|h| h.is_some()).unwrap_or(true),
            "a hook from another test leaked into this one"
        );
    }

    /// Fix round 1, Finding 1 and 2: the three shapes `gallery_open_response`
    /// can answer, over fake `has_hook`/`id_is_known` values rather than the
    /// real process-global hooks (which a real installation would race
    /// `no_hook_is_installed_by_default_in_this_test_binary` over -- see
    /// that fn's own doc comment).
    #[test]
    fn gallery_open_response_shapes() {
        let not_a_gallery = gallery_open_response("streets", false, true, false, &[]);
        assert_eq!(not_a_gallery["ok"], json!(false));
        assert_eq!(not_a_gallery["error"], json!("not a gallery"));

        // `has_hook: false` answers "not a gallery" regardless of
        // `id_is_known` -- there is no registry to have been wrong
        // against in a process that never installed the switch at all.
        let not_a_gallery_either = gallery_open_response("streets", false, false, false, &[]);
        assert_eq!(not_a_gallery_either["error"], json!("not a gallery"));

        let unknown = gallery_open_response("not-a-real-demo", true, false, false, &[]);
        assert_eq!(unknown["ok"], json!(false));
        assert_eq!(unknown["error"], json!("unknown id"));

        let ok = gallery_open_response("streets", true, true, false, &[]);
        assert_eq!(ok["ok"], json!(true));
        assert_eq!(ok["queued"], json!("gallery_open(streets)"));
    }

    /// An unknown id names the closest real ones and lists them all: an
    /// agent that guessed "singapore-smart-city" gets
    /// "smart-city-singapore" back in the same reply.
    #[test]
    fn an_unknown_gallery_id_suggests_the_closest_and_lists_all() {
        let ids: Vec<String> = ["basic", "smart-city-singapore", "smart-city-abu-dhabi", "geoparquet", "gtfs-winnipeg"].iter().map(|s| s.to_string()).collect();
        let r = gallery_open_response("singapore-smart-city", true, false, false, &ids);
        assert_eq!(r["error"], json!("unknown id"));
        assert_eq!(r["did_you_mean"][0], json!("smart-city-singapore"));
        assert!(r["hint"].as_str().unwrap().contains("smart-city-singapore"));
        assert_eq!(r["ids"].as_array().map(Vec::len), Some(5));
        assert_eq!(did_you_mean("geoparqet", &ids, 3).first().map(String::as_str), Some("geoparquet"), "a typo");
        assert_eq!(did_you_mean("singapore", &ids, 3).first().map(String::as_str), Some("smart-city-singapore"), "one word");
        assert!(did_you_mean("zzzz", &ids, 3).is_empty(), "nothing close is nothing");
        let tools: Vec<String> = ["gallery_scroll_to", "map_state"].iter().map(|s| s.to_string()).collect();
        assert!(did_you_mean("no_such_tool_xyz", &tools, 3).is_empty(), "\"tool\" is not \"to\"");
    }

    /// A known id a `Probe`-gate has not (yet) found reachable
    /// (a private style with no token, say): a clear "not available"
    /// error, never a lucky `"ok": true` for a panel the sidebar is not
    /// even showing.
    #[test]
    fn gallery_open_response_names_an_unavailable_id_clearly() {
        let unavailable = gallery_open_response("a-gated-page", true, true, true, &[]);
        assert_eq!(unavailable["ok"], json!(false));
        assert_eq!(unavailable["error"], json!("not available"));
        assert!(unavailable["hint"].as_str().unwrap().contains("a-gated-page"));
    }

    /// Fix round 1, Finding 4 and 3 ("Tests" item): the trigger
    /// `pump_slot` uses to re-arm tile-status recording. Regresses to
    /// never firing (recording silently stays off for every gallery
    /// panel, Finding 4's exact bug) or to firing on every tick regardless
    /// of change (a cheap but real cost this design avoids) equally
    /// visibly here.
    #[test]
    fn map_changed_fires_only_on_a_new_live_entity() {
        // `EntityId`'s `From<u64>` is public (`gpui`'s own slotmap key
        // type), which is what lets this run with no real `MapView`.
        let a: gpui::EntityId = 1u64.into();
        let b: gpui::EntityId = 2u64.into();
        assert!(!map_changed(None, None), "still empty is not a change");
        assert!(map_changed(None, Some(a)), "empty to live is a change -- the FIRST panel, not just later ones");
        assert!(!map_changed(Some(a), Some(a)), "the same panel again is not a change");
        assert!(map_changed(Some(a), Some(b)), "a different panel is a change");
        assert!(!map_changed(Some(a), None), "going empty has nothing to arm recording on");
    }

    /// The port has to stay inside the shared discovery range, or the
    /// bootstrap skill's scan of 7330-7339 will not find this app at all.
    #[test]
    fn the_default_port_is_inside_the_shared_discovery_range() {
        assert!((7330..=7339).contains(&DEFAULT_AGENTIC_PORT), "{DEFAULT_AGENTIC_PORT} is outside 7330-7339");
    }

    /// Studio scans upward from its default, so every port at or above it
    /// can be taken. None of them may be a port another app binds by
    /// default: one downstream app's 7332 is fixed, and taking it leaves that
    /// app with no agentic surface at all (2026-10-15).
    #[test]
    fn the_studio_scan_never_takes_another_apps_default_port() {
        for other in [7330, 7332, 7333] {
            assert!(other < DEFAULT_AGENTIC_PORT, "Studio's scan {DEFAULT_AGENTIC_PORT}-7339 would take {other}");
        }
    }

    /// `GET /`'s `"app"` field is what a scanner reads to tell this app
    /// from a downstream app on the next port along.
    #[test]
    fn the_app_name_is_the_one_a_scanner_looks_for() {
        assert_eq!(APP_NAME, "gpui-map");
    }

    /// The skip breakdown groups by cause, not by layer -- and a notice
    /// that does not match the expected shape is kept whole rather than
    /// dropped, so the counts always sum to the number of skips.
    #[test]
    fn skip_causes_group_by_cause_and_lose_nothing() {
        use gpui_maps::style_spec::{DiagnosticKind, StyleDiagnostic};
        let diagnostics = vec![
            StyleDiagnostic {
                kind: DiagnosticKind::SkippedLayer,
                layer: Some("a".into()),
                detail: "skipping layer \"a\": unsupported layer type \"model\"".into(),
            },
            StyleDiagnostic {
                kind: DiagnosticKind::SkippedLayer,
                layer: Some("b".into()),
                detail: "skipping layer \"b\": unsupported layer type \"model\"".into(),
            },
            StyleDiagnostic {
                kind: DiagnosticKind::SkippedLayer,
                layer: None,
                detail: "a notice in no particular shape".into(),
            },
            StyleDiagnostic {
                kind: DiagnosticKind::HiddenLayer,
                layer: Some("c".into()),
                detail: "skipping layer \"c\": visibility none".into(),
            },
        ];
        let causes = skip_causes(&diagnostics);
        assert_eq!(causes.get("unsupported layer type \"model\""), Some(&2));
        assert_eq!(causes.get("a notice in no particular shape"), Some(&1));
        assert_eq!(causes.values().sum::<u64>(), 3, "hidden layers are not skips and must not be counted");
    }
}
