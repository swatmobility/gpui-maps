//! The story and capture agent actions (spec S8): `POST /story/validate`,
//! `POST /story/render`, `GET /story/jobs/{id}`, `POST /story/cancel/{id}`
//! and `POST /capture/image`. HTTP stays in the examples (R23): the library
//! only renders.
//!
//! # Nothing here blocks the UI thread
//!
//! `dispatch` runs on gpui's thread with `&mut MapView`, and the vendored
//! server's accept loop answers ONE request at a time, parked on each
//! reply. So nothing that renders waits for its reply: `story.render` and
//! `capture.image` start a job and answer `{job}` at once, and the client
//! polls `GET /story/jobs/{id}` (a poll or a cancel is answered while the
//! still renders). Only `story.validate` waits: it answers a [`DEFERRED`]
//! marker, and [`reply_or_defer`] -- called by the pump with the call's
//! reply channel -- hands that channel to a waiter thread, which answers
//! when the scene checks finish or after 5 s, whichever is first. gpui's
//! thread is free meanwhile, but the server is not, and the action's
//! description says so. A capped reply is `{pending: true, job: id}`: the
//! checks go on, and `GET /story/jobs/{id}` has the report.
//!
//! # One render per process (R19)
//!
//! [`Jobs`] holds at most one running render. A second `story.render` or
//! `capture.image` while one runs answers `{"running": id}` (HTTP 409 in
//! the spec; this port's server replies 200 with the JSON only, so the body
//! carries `"status": 409` as well).
//!
//! # Where outputs may land
//!
//! These actions WRITE FILES, on a port any local browser tab can reach
//! (see `agentic.rs`'s posture doc). So every `out` is confined to
//! [`out_root`] (`~/Movies/gpui-map`, spec R20's default): it must be
//! absolute, hold no `..`, and its deepest existing ancestor must
//! canonicalise (symlinks resolved) to inside the root; a symlink at the
//! output itself, or at its manifest or extras path, is refused. An
//! existing output or manifest is never overwritten unless the body says
//! `"overwrite": true`. Every output also goes through
//! `burn_in::refuse_repo_output`. The script itself names local files the
//! render READS (style, tracks, overlays, wordmark) and URLs it fetches.
//!
//! No token is ever put in a reply or a manifest: the scene snapshot names
//! the variable (`${MAPBOX_ACCESS_TOKEN}`), the loader fills it in, and
//! the manifest records a redacted copy of the script.
#![allow(dead_code)] // each example compiles this file; not all reach every item

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui_agentic_http::{Action, ModuleRegistration, Param, ParamLocation};
use gpui_maps::map_view::MapView;
use gpui::{EntityId, WeakEntity};
use gpui_maps::camera_control::CameraTarget;
use gpui_maps::render::TileDataState;
use gpui_maps::story::timeline::ActiveCaption;
use gpui_maps::story::{self, sink, CameraPath, Progress, RenderManifest, RenderOptions, Timeline};
use serde_json::{json, Value};

/// How long `story.validate` waits for the scene checks before answering
/// `{pending: true, job: id}`.
pub const VALIDATE_REPLY_CAP: Duration = Duration::from_secs(5);

pub const STORY_ACTIONS: &[Action] = &[
    Action {
        name: "validate",
        method: "POST",
        path: "/validate",
        description: "\
Check a story script: the structural parse (answered at once, with every \
diagnostic), then the scene -- style, files, style edits, the Mapbox token, a \
z0 fetch unless `offline`, ffmpeg, the layout warnings (each at its event's \
path) and, with `estimate: true`, the tile estimate (null without it, for a fast \
answer). Answers {ok, diagnostics, estimate, ffmpeg}; if the scene checks take longer than 5 s \
it answers {pending: true, job} and GET /story/jobs/{job} has the report. \
This server answers one request at a time: for up to those 5 s it answers \
nothing else.",
        params: &[
            Param { name: "script", kind: "object", location: ParamLocation::Body, description: "The story script (JSON)." },
            Param { name: "offline", kind: "boolean", location: ParamLocation::Body, description: "Skip the source reachability fetch." },
            Param { name: "base_dir", kind: "string", location: ParamLocation::Body, description: "What the script's relative paths resolve against; default the process's current directory." },
            Param { name: "estimate", kind: "boolean", location: ParamLocation::Body, description: "Plan every frame's tiles for the render estimate (and the cover_too_large warning); default false, which answers faster with estimate: null." },
        ],
    },
    Action {
        name: "render",
        method: "POST",
        path: "/render",
        description: "\
Start rendering a story in the background; answers {job} at once. One render \
per process: while one runs this answers {running: id, status: 409}. Poll \
GET /story/jobs/{job}. `out` must be an absolute path inside ~/Movies/gpui-map \
(no `..`, no symlink out of it) and outside any git work tree; an existing \
output or `<stem>.story.json` is refused unless `overwrite` is true.",
        params: &[
            Param { name: "script", kind: "object", location: ParamLocation::Body, description: "The story script (JSON)." },
            Param { name: "out", kind: "string", location: ParamLocation::Body, description: "Output path inside ~/Movies/gpui-map; `.png` is added for a still or contact sheet without an extension." },
            Param { name: "overwrite", kind: "boolean", location: ParamLocation::Body, description: "Replace an existing output and manifest (default false: refused)." },
            Param { name: "format", kind: "string", location: ParamLocation::Body, description: "mp4 (default), still, contact-sheet or png-seq." },
            Param { name: "at", kind: "number", location: ParamLocation::Body, description: "Story time in seconds of a still (default the middle)." },
            Param { name: "strict", kind: "boolean", location: ParamLocation::Body, description: "Fail on a missing tile." },
            Param { name: "base_dir", kind: "string", location: ParamLocation::Body, description: "What relative paths resolve against." },
        ],
    },
    Action {
        name: "job",
        method: "GET",
        path: "/jobs/{id}",
        description: "\
A job's state (running, done, failed, cancelled), its progress (frame, \
frames, fps, eta), and when done the manifest, poster and contact_sheet \
paths, or the error. A pending validation's report is here too.",
        params: &[Param { name: "id", kind: "string", location: ParamLocation::Path, description: "The job id a render or pending reply named." }],
    },
    Action {
        name: "cancel",
        method: "POST",
        path: "/cancel/{id}",
        description: "\
Cancel a running render: it stops before its next frame (or within 50 ms of a \
tile wait) and a video's partial file is removed. Answers {cancelling: id}.",
        params: &[Param { name: "id", kind: "string", location: ParamLocation::Path, description: "The job to cancel." }],
    },
    Action {
        name: "preview",
        method: "POST",
        path: "/preview",
        description: "\
Play a story on THIS map, in real time: every frame the window draws, the \
camera jumps to the story's pose at t = (now - started) * rate + from, the \
globe and style generations follow the timeline (every generation is \
compiled on a worker first; the clock starts once they are, `story.preparing` \
until then), and the map itself draws the render's story frame: panels, \
images, callouts, furniture and captions, laid out as the video lays them \
out, and the overlay layers with their trim, opacity and per-feature paint. \
At a settle point the story clock pauses until no cover tile is pending, or \
the settle timeout. The buildings take the script's `scene.buildings` look. \
NOT shown, unlike the render (listed as `not_applied` in the answer and in \
GET /map/state's `story`): scene.basemap (the live map keeps its own tile \
source), scene.raster, the labels/atmosphere/stars/terminator params and \
their events, overlays without layers and tracks, an unthemed layer's draw \
head and rings (a themed layer's head, markers, labels and arrowheads ARE \
drawn), \
uncached remote images, and the wordmark. \
Answers at once {ok, from, to, duration, rate, loop, not_applied}; GET \
/map/state has `story` {playing, t, segment, waiting_on_tiles} while it \
runs. A second preview replaces the first. The window only advances while \
it paints (frontmost).",
        params: &[
            Param { name: "script", kind: "object", location: ParamLocation::Body, description: "The story script (JSON)." },
            Param { name: "rate", kind: "number", location: ParamLocation::Body, description: "Story seconds per wall second, (0, 16]; default 1." },
            Param { name: "from", kind: "number", location: ParamLocation::Body, description: "Start at this story second (default 0)." },
            Param { name: "to", kind: "number", location: ParamLocation::Body, description: "Stop at this story second (default the story's end)." },
            Param { name: "loop", kind: "boolean", location: ParamLocation::Body, description: "Wrap from `to` back to `from` until stopped." },
            Param { name: "base_dir", kind: "string", location: ParamLocation::Body, description: "What the script's relative paths resolve against; default the process's current directory." },
        ],
    },
    Action {
        name: "preview_stop",
        method: "POST",
        path: "/preview/stop",
        description: "\
Stop the preview and put back what the map showed before it started: the \
camera, the globe, the style (and its document), the building look and \
tile-status recording. \
Answers {stopped: true, restored: true}, or {stopped: false} when no preview \
was running on this map.",
        params: &[],
    },
];

pub const CAPTURE_ACTIONS: &[Action] = &[Action {
    name: "image",
    method: "POST",
    path: "/image",
    description: "\
Render one PNG. `scene: \"current\"` (the default) is what this map shows. \
With `layers: true` (the default) it is THIS map's own frame, rendered \
offscreen by the map's renderer: the basemap and every layer, overlay, \
symbol and marker the page draws (not the gpui cards over it), whether or \
not the window is frontmost or visible -- re-rendered until the tiles and \
raster imagery and overlay bakes settle (or `timeout` s), at `width` x `height` physical \
pixels at `scale` (default the map's size at 2x); the answer {ok, path, \
width, height, scale, settled, frames} is the job's `result`. With \
`layers: false` (or a story script as `scene` and `at` seconds) it is the \
story renderer: the camera, globe, style document and basemap only \
(`MapView::scene_snapshot`, never a token), with the credit and the SWAT \
mark burned in; its answer {ok, path, manifest, publishable, attribution, \
left_out}. Answers {job} at once (one render per process: {running: id, \
status: 409} while another runs); poll GET /story/jobs/{job}, cancel with \
POST /story/cancel/{job}.",
    params: &[
        Param { name: "out", kind: "string", location: ParamLocation::Body, description: "The PNG path: absolute, inside ~/Movies/gpui-map, outside any git work tree." },
        Param { name: "overwrite", kind: "boolean", location: ParamLocation::Body, description: "Replace an existing file at `out` (default false: refused)." },
        Param { name: "scene", kind: "string", location: ParamLocation::Body, description: "\"current\" (default) or a story script object." },
        Param { name: "at", kind: "number", location: ParamLocation::Body, description: "With a script: the story time in seconds (default the middle)." },
        Param { name: "width", kind: "number", location: ParamLocation::Body, description: "Pixels; default the map's width (times `scale` with `layers`)." },
        Param { name: "height", kind: "number", location: ParamLocation::Body, description: "Pixels; default the map's height (times `scale` with `layers`)." },
        Param { name: "layers", kind: "boolean", location: ParamLocation::Body, description: "true (default): the map's own frame with every layer, offscreen. false: the story renderer's style-and-basemap still." },
        Param { name: "scale", kind: "number", location: ParamLocation::Body, description: "With `layers`: physical pixels per logical pixel, 0.25 to 8 (default 2)." },
        Param { name: "timeout", kind: "number", location: ParamLocation::Body, description: "With `layers`: seconds to wait for the map to settle before writing what it has (default 30, at most 120)." },
    ],
}];

/// One parsed call.
#[derive(Debug, Clone, PartialEq)]
pub enum StoryRequest {
    Validate { script: Value, offline: bool, base_dir: Option<PathBuf>, estimate: bool },
    Render(RenderRequest),
    Job(String),
    Cancel(String),
    Image {
        out: PathBuf,
        scene: Option<Value>,
        at: Option<f64>,
        width: Option<u32>,
        height: Option<u32>,
        overwrite: bool,
        /// The map's own frame offscreen (default) rather than the story
        /// renderer's style-and-basemap still.
        layers: bool,
        /// With `layers`: physical pixels per logical one.
        scale: Option<f32>,
        /// With `layers`: how long to wait for the map to settle.
        timeout: Option<Duration>,
    },
    Preview(PreviewRequest),
    PreviewStop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreviewRequest {
    pub script: Value,
    pub rate: f64,
    pub from: Option<f64>,
    pub to: Option<f64>,
    pub looped: bool,
    pub base_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderRequest {
    pub script: Value,
    pub out: PathBuf,
    pub format: Format,
    pub at: Option<f64>,
    pub strict: bool,
    pub base_dir: Option<PathBuf>,
    /// Replace an existing output (and its manifest); default refused.
    pub overwrite: bool,
    /// Scratch inputs the job made for itself (a snapshot's style file),
    /// removed when the job ends, however it ends.
    pub cleanup: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Mp4,
    Still,
    ContactSheet,
    PngSeq,
}

impl Format {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mp4" => Format::Mp4,
            "still" => Format::Still,
            "contact-sheet" => Format::ContactSheet,
            "png-seq" => Format::PngSeq,
            _ => return None,
        })
    }
}

fn body_path(body: &Value, key: &str) -> Option<PathBuf> {
    body.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn body_u32(body: &Value, key: &str) -> Option<u32> {
    body.get(key).and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).filter(|v| (16..=8192).contains(v))
}

/// The call named `name` (as either registry declares it) with its params,
/// or `None` when it is not one of these actions or a required param is
/// missing or malformed.
pub fn parse_story_action(name: &str, path_param: Option<&str>, _query: &str, body: &str) -> Option<StoryRequest> {
    let parsed = || serde_json::from_str::<Value>(if body.trim().is_empty() { "{}" } else { body }).ok().filter(Value::is_object);
    match name {
        "validate" => {
            let body = parsed()?;
            Some(StoryRequest::Validate {
                script: body.get("script")?.clone(),
                offline: body.get("offline").and_then(Value::as_bool).unwrap_or(false),
                base_dir: body_path(&body, "base_dir"),
                estimate: body.get("estimate").and_then(Value::as_bool).unwrap_or(false),
            })
        }
        "render" => {
            let body = parsed()?;
            let format = match body.get("format") {
                None => Format::Mp4,
                Some(f) => Format::parse(f.as_str()?)?,
            };
            Some(StoryRequest::Render(RenderRequest {
                script: body.get("script").filter(|s| s.is_object())?.clone(),
                out: body_path(&body, "out")?,
                format,
                at: body.get("at").and_then(Value::as_f64).filter(|t| t.is_finite() && *t >= 0.0),
                strict: body.get("strict").and_then(Value::as_bool).unwrap_or(false),
                base_dir: body_path(&body, "base_dir"),
                overwrite: body.get("overwrite").and_then(Value::as_bool).unwrap_or(false),
                cleanup: Vec::new(),
            }))
        }
        "preview" => {
            let body = parsed()?;
            let seconds = |key: &str| -> Option<Option<f64>> {
                match body.get(key) {
                    None | Some(Value::Null) => Some(None),
                    Some(v) => v.as_f64().filter(|t| t.is_finite() && *t >= 0.0).map(Some),
                }
            };
            let rate = match body.get("rate") {
                None | Some(Value::Null) => 1.0,
                Some(v) => v.as_f64().filter(|r| r.is_finite() && *r > 0.0 && *r <= 16.0)?,
            };
            Some(StoryRequest::Preview(PreviewRequest {
                script: body.get("script").filter(|s| s.is_object())?.clone(),
                rate,
                from: seconds("from")?,
                to: seconds("to")?,
                looped: body.get("loop").and_then(Value::as_bool).unwrap_or(false),
                base_dir: body_path(&body, "base_dir"),
            }))
        }
        "preview_stop" => Some(StoryRequest::PreviewStop),
        "job" => path_param.filter(|p| !p.is_empty()).map(|p| StoryRequest::Job(p.to_string())),
        "cancel" => path_param.filter(|p| !p.is_empty()).map(|p| StoryRequest::Cancel(p.to_string())),
        "image" => {
            let body = parsed()?;
            let scene = match body.get("scene") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if s == "current" => None,
                Some(v @ Value::Object(_)) => Some(v.clone()),
                Some(_) => return None,
            };
            Some(StoryRequest::Image {
                out: body_path(&body, "out")?,
                scene,
                at: body.get("at").and_then(Value::as_f64).filter(|t| t.is_finite() && *t >= 0.0),
                width: body_u32(&body, "width"),
                height: body_u32(&body, "height"),
                overwrite: body.get("overwrite").and_then(Value::as_bool).unwrap_or(false),
                layers: match body.get("layers") {
                    None | Some(Value::Null) => true,
                    Some(v) => v.as_bool()?,
                },
                scale: match body.get("scale") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.as_f64().filter(|s| s.is_finite() && (0.25..=8.0).contains(s))? as f32),
                },
                timeout: match body.get("timeout") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(Duration::from_secs_f64(v.as_f64().filter(|s| s.is_finite() && *s > 0.0)?.min(OFFSCREEN_MAX_TIMEOUT.as_secs_f64()))),
                },
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Running,
    Done,
    Failed,
    Cancelled,
}

struct Job {
    id: String,
    state: JobState,
    progress: Option<Progress>,
    manifest: Option<PathBuf>,
    poster: Option<PathBuf>,
    contact: Option<PathBuf>,
    /// The main output (the still, the video, the sheet).
    output: Option<PathBuf>,
    /// The job's own answer when done (a still's reply, a validation's report).
    result: Option<Value>,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    fn status(&self) -> Value {
        json!({
            "job": self.id,
            "state": self.state,
            "progress": self.progress,
            "output": self.output,
            "manifest": self.manifest,
            "poster": self.poster,
            "contact_sheet": self.contact,
            "result": self.result,
            "error": self.error,
        })
    }
}

/// How many validation entries are kept; past it, the oldest FINISHED
/// ones go first.
const VALIDATIONS_KEPT: usize = 16;

/// The render slot, plus pending validations (which never hold it).
#[derive(Default)]
struct Jobs {
    current: Option<Job>,
    validations: HashMap<String, Option<Value>>,
    /// Validation ids, oldest first, for the eviction order.
    validation_order: std::collections::VecDeque<String>,
}

impl Jobs {
    /// Claims the slot for `id`, or answers who holds it.
    fn begin(&mut self, id: String) -> Result<Arc<AtomicBool>, Value> {
        if let Some(job) = self.current.as_ref().filter(|j| j.state == JobState::Running) {
            return Err(json!({ "running": job.id }));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.current = Some(Job {
            id,
            state: JobState::Running,
            progress: None,
            manifest: None,
            poster: None,
            contact: None,
            output: None,
            result: None,
            error: None,
            cancel: Arc::clone(&cancel),
        });
        Ok(cancel)
    }

    /// Records validation `id` (`None` while it runs), then evicts the
    /// oldest finished entries past [`VALIDATIONS_KEPT`]. A running one is
    /// never evicted, and the one just recorded is the newest, so a client
    /// that was answered `{pending, job}` can always poll it.
    fn record_validation(&mut self, id: String, report: Option<Value>) {
        if !self.validations.contains_key(&id) {
            self.validation_order.push_back(id.clone());
        }
        self.validations.insert(id, report);
        let mut i = 0;
        while self.validations.len() > VALIDATIONS_KEPT && i < self.validation_order.len() {
            let finished = self.validation_order.get(i).and_then(|old| self.validations.get(old)).is_some_and(Option::is_some);
            if finished {
                if let Some(old) = self.validation_order.remove(i) {
                    self.validations.remove(&old);
                }
            } else {
                i += 1;
            }
        }
    }

    #[cfg(test)]
    fn start_for_test(&mut self, id: &str) -> Result<(), Value> {
        self.begin(id.to_string()).map(|_| ())
    }

    fn with_job(&mut self, id: &str, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.current.as_mut().filter(|j| j.id == id) {
            f(job);
        }
    }

    fn status(&self, id: &str) -> Value {
        if let Some(job) = self.current.as_ref().filter(|j| j.id == id) {
            return job.status();
        }
        match self.validations.get(id) {
            Some(Some(report)) => json!({ "job": id, "state": "done", "result": report }),
            Some(None) => json!({ "job": id, "state": "running" }),
            None => json!({ "error": "unknown job", "job": id, "hint": "only the latest render and recent validations are kept" }),
        }
    }

    fn cancel(&mut self, id: &str) -> Value {
        match self.current.as_ref().filter(|j| j.id == id) {
            Some(job) if job.state == JobState::Running => {
                job.cancel.store(true, Ordering::SeqCst);
                json!({ "cancelling": id })
            }
            Some(job) => json!({ "job": id, "state": job.state, "note": "not running" }),
            None => json!({ "error": "unknown job", "job": id }),
        }
    }
}

fn jobs() -> &'static Mutex<Jobs> {
    static JOBS: OnceLock<Mutex<Jobs>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(Jobs::default()))
}

fn lock_jobs() -> std::sync::MutexGuard<'static, Jobs> {
    jobs().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_id(kind: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{kind}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Deferred replies
// ---------------------------------------------------------------------------

/// The key a dispatch answers with when its reply comes later.
pub const DEFERRED: &str = "__deferred";

struct Deferred {
    answer: Receiver<Value>,
    cap: Duration,
    on_timeout: Value,
}

fn deferred() -> &'static Mutex<HashMap<u64, Deferred>> {
    static D: OnceLock<Mutex<HashMap<u64, Deferred>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(HashMap::new()))
}

fn defer(answer: Receiver<Value>, cap: Duration, on_timeout: Value) -> Value {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    deferred().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(token, Deferred { answer, cap, on_timeout });
    json!({ DEFERRED: token })
}

fn take_deferred(response: &Value) -> Option<Deferred> {
    let token = response.get(DEFERRED)?.as_u64()?;
    deferred().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&token)
}

/// Waits (on this thread) for a deferred answer, up to its cap.
fn resolve(d: Deferred) -> Value {
    d.answer.recv_timeout(d.cap).unwrap_or(d.on_timeout)
}

/// Called by the pump for every response: a deferred one gets a waiter
/// thread that sends the real answer down `reply` later (and `None` comes
/// back); anything else hands `reply` back for the pump to send `response`
/// on at once.
pub fn reply_or_defer(response: &Value, reply: Sender<Value>) -> Option<Sender<Value>> {
    let Some(d) = take_deferred(response) else {
        return Some(reply);
    };
    let spawned = std::thread::Builder::new().name("agent-reply".into()).spawn(move || {
        let _ = reply.send(resolve(d));
    });
    // A thread that cannot start drops `reply`: the client reads "no reply".
    let _ = spawned;
    None
}

// ---------------------------------------------------------------------------
// The actions
// ---------------------------------------------------------------------------

fn base_dir_or_cwd(base: Option<PathBuf>) -> PathBuf {
    base.or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("."))
}

/// `story.validate`'s inline half: the structural parse. `Err` carries the
/// complete answer for a script that does not parse (every diagnostic).
fn parse_or_answer(raw: &[u8]) -> Result<story::Script, Value> {
    story::script::parse_with_warnings(raw)
        .map(|(script, _)| script)
        .map_err(|errors| json!({ "ok": false, "diagnostics": errors, "estimate": null, "ffmpeg": null }))
}

/// The structural validation, answered inline: `{ok: false, diagnostics}`
/// for a script that does not parse, `{ok: true, parsed: true}` otherwise
/// (the scene checks are [`validate_scene_blocking`]'s).
pub fn validate_inline(raw: &[u8]) -> Value {
    match parse_or_answer(raw) {
        Ok(_) => json!({ "ok": true, "parsed": true }),
        Err(answer) => answer,
    }
}

/// The scene checks (`story::validate_scene`) on the calling thread, with
/// a runtime of its own.
fn validate_scene_blocking(script: &story::Script, raw: &[u8], offline: bool, base_dir: PathBuf, estimate: bool) -> Value {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return json!({ "ok": false, "error": format!("no runtime: {e}") }),
    };
    let report = rt.block_on(story::validate_scene(script, raw, &story::ValidateOptions { offline, base_dir, image_cache_dir: None, estimate, ..story::ValidateOptions::default() }));
    serde_json::to_value(&report).unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }))
}

fn validate(script: &Value, offline: bool, base_dir: Option<PathBuf>, estimate: bool) -> Value {
    let raw = match serde_json::to_vec(script) {
        Ok(raw) => raw,
        Err(e) => return json!({ "ok": false, "error": e.to_string() }),
    };
    let parsed = match parse_or_answer(&raw) {
        Ok(parsed) => parsed,
        Err(answer) => return answer,
    };
    let id = next_id("validate");
    lock_jobs().record_validation(id.clone(), None);
    let (tx, rx) = std::sync::mpsc::channel();
    let base = base_dir_or_cwd(base_dir);
    let job_id = id.clone();
    let spawned = std::thread::Builder::new().name("story-validate".into()).spawn(move || {
        let report = validate_scene_blocking(&parsed, &raw, offline, base, estimate);
        lock_jobs().record_validation(job_id, Some(report.clone()));
        let _ = tx.send(report);
    });
    if let Err(e) = spawned {
        return json!({ "ok": false, "error": format!("could not start the validation: {e}") });
    }
    defer(rx, VALIDATE_REPLY_CAP, json!({ "pending": true, "job": id }))
}

/// `.png` added to a still's or sheet's `out` without an extension.
fn with_png(out: &Path) -> PathBuf {
    if out.extension().is_some() {
        out.to_path_buf()
    } else {
        out.with_extension("png")
    }
}

fn manifest_path(out: &Path) -> PathBuf {
    let stem = out.file_stem().map_or_else(|| "story".into(), |s| s.to_string_lossy().into_owned());
    out.with_file_name(format!("{stem}.story.json"))
}

fn extras_dir(out: &Path) -> PathBuf {
    let stem = out.file_stem().map_or_else(|| "story".into(), |s| s.to_string_lossy().into_owned());
    out.with_file_name(format!("{stem}-extras"))
}

/// The one directory agent outputs may land in: `~/Movies/gpui-map`
/// (spec R20's default, where `examples/story.rs` and the gallery footer
/// write too). Tests point it at the temp dir.
fn out_root() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    if let Some(root) = tests::OUT_ROOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
        return Ok(root);
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join("Movies").join("gpui-map"))
}

/// Refuses `out` unless it lies inside `root` (see the module doc's
/// "Where outputs may land"): absolute, no `..` or `.`, its deepest
/// existing ancestor canonicalised inside the canonical root, no symlink
/// at `out` or at any of `beside` (the manifest, the extras folder), and
/// no existing `out` or manifest unless `overwrite`.
fn confine_out(out: &Path, root: &Path, overwrite: bool, beside: &[PathBuf]) -> anyhow::Result<()> {
    use std::path::Component;
    anyhow::ensure!(out.is_absolute(), "`out` must be an absolute path, got {}", out.display());
    anyhow::ensure!(
        out.components().all(|c| matches!(c, Component::RootDir | Component::Prefix(_) | Component::Normal(_))),
        "`out` may not contain `.` or `..`: {}",
        out.display()
    );
    // A symlink at the output (or beside it) is refused first, dangling
    // or not: nothing is ever written through one.
    for p in std::iter::once(&out.to_path_buf()).chain(beside) {
        if let Ok(meta) = std::fs::symlink_metadata(p) {
            anyhow::ensure!(!meta.file_type().is_symlink(), "refusing to write through the symlink {}", p.display());
            anyhow::ensure!(overwrite, "{} exists; pass \"overwrite\": true to replace it", p.display());
        }
    }
    std::fs::create_dir_all(root).map_err(|e| anyhow::anyhow!("cannot create the output root {}: {e}", root.display()))?;
    let real_root = root.canonicalize()?;
    // The deepest ancestor that exists at all -- a dangling symlink
    // counts as existing (`symlink_metadata`), so it cannot be written
    // through.
    let mut existing = out;
    while std::fs::symlink_metadata(existing).is_err() {
        existing = existing.parent().ok_or_else(|| anyhow::anyhow!("{} has no existing ancestor", out.display()))?;
    }
    let real = existing.canonicalize().map_err(|e| anyhow::anyhow!("cannot resolve {}: {e}", existing.display()))?;
    anyhow::ensure!(
        real.starts_with(&real_root),
        "agent outputs are confined to {}; {} is outside it",
        root.display(),
        out.display()
    );
    Ok(())
}

/// Where a request's output goes: confined to [`out_root`], refused over
/// an existing file without `overwrite`, and inside a git work tree.
fn checked_out(req: &RenderRequest) -> anyhow::Result<PathBuf> {
    let out = match req.format {
        Format::Still | Format::ContactSheet => with_png(&req.out),
        Format::Mp4 if req.out.extension().is_none() => req.out.with_extension("mp4"),
        Format::Mp4 | Format::PngSeq => req.out.clone(),
    };
    anyhow::ensure!(out.is_absolute(), "`out` must be an absolute path, got {}", out.display());
    story::burn_in::refuse_repo_output(&out, false)?;
    confine_out(&out, &out_root()?, req.overwrite, &[manifest_path(&out), extras_dir(&out)])?;
    Ok(out)
}

/// The render itself, on a worker thread: parse, load, play into the
/// format's sink, write the manifest beside `out`.
fn render_job(req: &RenderRequest, out: &Path, cancel: &Arc<AtomicBool>, id: &str) -> anyhow::Result<RenderManifest> {
    let raw = serde_json::to_vec(&req.script)?;
    let script = story::parse(&raw).map_err(|d| anyhow::anyhow!("the script does not parse: {}", serde_json::to_string(&d).unwrap_or_default()))?;
    let base_dir = base_dir_or_cwd(req.base_dir.clone());
    let inputs = story::scene_inputs(&script, &base_dir, false)?;
    let opts = RenderOptions {
        strict: req.strict || script.output.strict == Some(true),
        cancel: Arc::clone(cancel),
        // The publish table's Mapbox video rule follows the format, as the
        // `story` CLI's does (`story::output_is_video`).
        video: story::output_is_video(match req.format {
            Format::Mp4 => "mp4",
            Format::PngSeq => "png-seq",
            Format::Still => "still",
            Format::ContactSheet => "contact-sheet",
        }),
        ..RenderOptions::default()
    };
    let output = script.output();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = {
        let _entered = rt.enter();
        let mut player = story::Player::new(&script, &raw, inputs, opts)?;
        let frames = player.frames();
        let fps = f64::from(output.fps.max(1));
        let at_frame = |s: f64| ((s * fps).round().max(0.0) as usize).min(frames.saturating_sub(1));
        let range = match (req.format, req.at) {
            (_, Some(s)) => at_frame(s)..at_frame(s) + 1,
            (Format::Still, None) => frames / 2..frames / 2 + 1,
            _ => 0..frames,
        };
        player.set_frames(Some(range.clone()));
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let extras = extras_dir(out);
        std::fs::create_dir_all(&extras)?;
        let job_id = id.to_string();
        let mut progress = move |p: &Progress| lock_jobs().with_job(&job_id, |j| j.progress = Some(p.clone()));
        let result = match req.format {
            Format::Mp4 => sink::video_sink(&sink::FfmpegChoice::Detect, out, output.width, output.height, output.fps, sink::Codec::of_script(&script))
                .and_then(|mut main| player.run_with_extras(&mut *main, &extras, &mut progress)),
            Format::PngSeq => player.run_with_extras(&mut sink::PngSequenceSink { dir: out.to_path_buf() }, &extras, &mut progress),
            Format::Still => player.run_with_extras(&mut sink::StillSink::new(out.to_path_buf()), &extras, &mut progress),
            Format::ContactSheet => player.run_contact_sheet(range, out, &extras, &mut progress),
        };
        if result.is_err() {
            let _ = std::fs::remove_dir(&extras);
        }
        result
    };
    rt.shutdown_background();
    let manifest = result?;
    // No credential and no absolute local path (relative to the manifest's
    // folder, then the story's; the home directory as `~`).
    story::write_manifest(&manifest, &manifest_path(out), &base_dir)?;
    Ok(manifest)
}

/// Starts `req` on a worker: `Ok((id, done))` where `done` receives the
/// job's final status, or `Err(answer)` (a running job, a refused path).
fn start_render(
    req: RenderRequest,
    answer_with: impl FnOnce(&RenderManifest, &Path) -> Value + Send + 'static,
) -> Result<(String, Receiver<Value>), Value> {
    let remove_scratch = |files: &[PathBuf]| {
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    };
    let out = checked_out(&req).map_err(|e| {
        remove_scratch(&req.cleanup);
        json!({ "error": format!("{e:#}") })
    })?;
    let id = next_id("job");
    let cancel = lock_jobs().begin(id.clone()).map_err(|mut running| {
        remove_scratch(&req.cleanup);
        running["status"] = json!(409);
        running
    })?;
    lock_jobs().with_job(&id, |j| j.output = Some(out.clone()));
    let (tx, rx) = std::sync::mpsc::channel();
    let job_id = id.clone();
    let spawned = std::thread::Builder::new().name("story-render".into()).spawn(move || {
        let result = render_job(&req, &out, &cancel, &job_id);
        remove_scratch(&req.cleanup);
        let mut jobs = lock_jobs();
        jobs.with_job(&job_id, |j| match &result {
            Ok(m) => {
                j.state = JobState::Done;
                j.manifest = Some(manifest_path(&out));
                j.poster = m.poster.clone();
                j.contact = m.contact_sheet.clone();
                j.result = Some(answer_with(m, &out));
            }
            Err(e) => {
                j.state = if cancel.load(Ordering::SeqCst) { JobState::Cancelled } else { JobState::Failed };
                j.error = Some(format!("{e:#}"));
            }
        });
        let status = jobs.status(&job_id);
        drop(jobs);
        let _ = tx.send(status);
    });
    if let Err(e) = spawned {
        lock_jobs().with_job(&id, |j| {
            j.state = JobState::Failed;
            j.error = Some(format!("could not start the render: {e}"));
        });
    }
    Ok((id, rx))
}

fn render_summary(m: &RenderManifest, out: &Path) -> Value {
    let missing: usize = m.frames.iter().map(|f| f.tiles_missing.len()).sum();
    json!({
        "path": out,
        "publishable": m.publishable,
        "publishable_reason": m.publishable_reason,
        "attribution": m.attribution,
        "frames": m.frames.len(),
        "tiles_missing": missing,
        "gaps": m.gaps,
    })
}

fn render(req: RenderRequest) -> Value {
    match start_render(req, render_summary) {
        Ok((id, _done)) => json!({ "job": id, "state": "running", "poll": format!("/story/jobs/{id}") }),
        Err(answer) => answer,
    }
}

/// A one-frame story from a scene snapshot: a jump to its camera and a
/// settled hold, at `width` x `height`. The style is written to a file the
/// script names; a templated basemap gets its token substituted here, on
/// the way into the script, never into a reply.
fn snapshot_script(snapshot: &story::SceneSnapshot, width: u32, height: u32) -> anyhow::Result<Value> {
    let style = snapshot.style.as_ref().ok_or_else(|| {
        anyhow::anyhow!("this map's style was set without its JSON document (MapView::set_style_json), so a story cannot load it")
    })?;
    // One file per call (two stills in flight never share one); the job
    // removes it when it ends (`RenderRequest::cleanup`).
    let style_path = std::env::temp_dir().join(format!("gpui-map-current-style-{}-{}.json", std::process::id(), next_id("s")));
    std::fs::write(&style_path, serde_json::to_vec(style)?)?;
    let basemap = snapshot.basemap.as_deref().ok_or_else(|| anyhow::anyhow!("this map's basemap is not one a story can name"))?;
    let basemap = if basemap.starts_with("http://") || basemap.starts_with("https://") {
        // The `${MAPBOX_ACCESS_TOKEN}` template stays a template: the
        // loader fills it (`story::scene_inputs`), so the script and the
        // manifest written from it never hold the live token.
        json!({ "tiles": [basemap], "max_zoom": 14, "attribution": snapshot.credit.clone().unwrap_or_default() })
    } else {
        json!({ "source": basemap })
    };
    let c = &snapshot.camera;
    let mut scene = json!({ "style": style_path, "basemap": basemap, "globe": snapshot.globe, "buildings": snapshot.buildings });
    if let Some(raster) = &snapshot.raster {
        scene["raster"] = json!(raster);
    }
    Ok(json!({
        "story": 1,
        "title": "current map",
        "output": { "width": width, "height": height, "fps": 10 },
        "scene": scene,
        "camera": [
            { "type": "jump", "to": { "lon": c.lon, "lat": c.lat, "zoom": c.zoom, "pitch": c.pitch_deg, "bearing": c.bearing_deg } },
            { "type": "hold", "duration": 0.2, "settle": true }
        ],
        "events": []
    }))
}

/// The scratch style file [`snapshot_script`] wrote for `script`, when it
/// is one (named by its prefix in the temp dir), for the job to remove.
fn snapshot_style_file(script: &Value) -> Option<PathBuf> {
    let style = PathBuf::from(script.pointer("/scene/style")?.as_str()?);
    let name = style.file_name()?.to_str()?;
    let ours = format!("gpui-map-current-style-{}-", std::process::id());
    (style.parent() == Some(std::env::temp_dir().as_path()) && name.starts_with(&ours)).then_some(style)
}

fn image_summary(m: &RenderManifest, out: &Path) -> Value {
    let row = m.frames.first();
    json!({
        "ok": true,
        "path": out,
        "manifest": manifest_path(out),
        "publishable": m.publishable,
        "publishable_reason": m.publishable_reason,
        "attribution": m.attribution,
        "tiles_missing": row.map(|r| r.tiles_missing.clone()),
        "sha256": row.map(|r| r.sha256.clone()),
        "gaps": m.gaps,
    })
}

/// `capture.image`, given the scene already resolved on the UI thread: a
/// job like `story.render`, answered at once with its id. The vendored
/// server answers one request at a time, so a reply that waited on the
/// still would hold every other call (a poll, a cancel of this very still)
/// behind it; the client polls `GET /story/jobs/{job}` instead, whose
/// `result` is the still's answer when `state` is `done`.
fn image(out: PathBuf, scene: Result<(Value, Vec<String>), String>, at: Option<f64>, overwrite: bool) -> Value {
    let (script, left_out) = match scene {
        Ok(s) => s,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    // A snapshot's style is a scratch file the job removes when it ends.
    let cleanup = snapshot_style_file(&script).into_iter().collect();
    let req = RenderRequest { script, out, format: Format::Still, at, strict: false, base_dir: None, overwrite, cleanup };
    let answer = move |m: &RenderManifest, out: &Path| {
        let mut answer = image_summary(m, out);
        answer["left_out"] = json!(left_out);
        answer
    };
    match start_render(req, answer) {
        Ok((id, _done)) => json!({ "job": id, "state": "running", "poll": format!("/story/jobs/{id}") }),
        Err(answer) => answer,
    }
}

/// The scene `capture.image` renders: its own script, or this map's
/// snapshot (read here, on the UI thread) turned into a one-frame story.
fn resolve_image_scene(map: &MapView, scene: Option<Value>, width: Option<u32>, height: Option<u32>) -> Result<(Value, Vec<String>), String> {
    if let Some(script) = scene {
        return Ok((script, Vec::new()));
    }
    let snapshot = map.scene_snapshot();
    let camera = map.camera();
    let w = width.unwrap_or((camera.viewport_width_px.round() as u32).clamp(16, 8192));
    let h = height.unwrap_or((camera.viewport_height_px.round() as u32).clamp(16, 8192));
    snapshot_script(&snapshot, w, h).map(|s| (s, snapshot.left_out.clone())).map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// The map's own frame, offscreen (`capture.image` with `layers`)
// ---------------------------------------------------------------------------

/// How long an offscreen still waits for the map to settle by default.
const OFFSCREEN_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest `timeout` a call may ask for.
const OFFSCREEN_MAX_TIMEOUT: Duration = Duration::from_secs(120);
/// The shortest an offscreen still renders for, settled or not: label
/// fades and a just-pushed overlay's first bake take a few frames.
const OFFSCREEN_MIN_SPAN: Duration = Duration::from_millis(600);
/// At most one offscreen frame per this, so a capture costs the UI thread
/// a frame every few pump ticks rather than every one.
const OFFSCREEN_FRAME_GAP: Duration = Duration::from_millis(80);

/// An offscreen still in progress: rendered a frame at a time by the pump
/// ([`offscreen_tick`]) on the map it was asked of, until that map settles
/// or `deadline`, then read back and written on a thread of its own.
struct OffscreenJob {
    id: String,
    map: Option<EntityId>,
    out: PathBuf,
    frame: gpui_maps::map_view::OffscreenFrame,
    started: Instant,
    deadline: Instant,
    last_frame: Option<Instant>,
    frames: u32,
    /// Consecutive settled frames: two in a row before the still is
    /// taken, so imagery that landed on the last one has been uploaded
    /// and drawn by the next.
    settled_run: u32,
    cancel: Arc<AtomicBool>,
}

fn offscreen_jobs() -> &'static Mutex<Vec<OffscreenJob>> {
    static JOBS: OnceLock<Mutex<Vec<OffscreenJob>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(Vec::new()))
}

/// The pixel size of an offscreen still: `width` x `height` when given,
/// else the map's logical size times `scale`; `None` past the limits.
fn offscreen_frame(map: &MapView, width: Option<u32>, height: Option<u32>, scale: Option<f32>) -> Option<gpui_maps::map_view::OffscreenFrame> {
    let scale = scale.unwrap_or(2.0);
    let camera = map.camera();
    let edge = |given: Option<u32>, logical: f32| given.unwrap_or_else(|| (logical.max(1.0) * scale).round() as u32);
    gpui_maps::map_view::OffscreenFrame::new(edge(width, camera.viewport_width_px), edge(height, camera.viewport_height_px), scale)
}

/// `capture.image` with `layers`: checks `out`, claims the render slot
/// and queues the still for the pump. Answers `{job}` at once, as the
/// story renderer's still does.
fn offscreen_image(
    map: &MapView,
    out: PathBuf,
    width: Option<u32>,
    height: Option<u32>,
    scale: Option<f32>,
    timeout: Option<Duration>,
    overwrite: bool,
) -> Value {
    let Some(frame) = offscreen_frame(map, width, height, scale) else {
        return json!({ "ok": false, "error": "width, height and scale make a frame outside 1..8192 px a side" });
    };
    let out = with_png(&out);
    let checked = (|| -> anyhow::Result<()> {
        anyhow::ensure!(out.is_absolute(), "`out` must be an absolute path, got {}", out.display());
        story::burn_in::refuse_repo_output(&out, false)?;
        confine_out(&out, &out_root()?, overwrite, &[])
    })();
    if let Err(e) = checked {
        return json!({ "ok": false, "error": format!("{e:#}") });
    }
    let id = next_id("job");
    let cancel = match lock_jobs().begin(id.clone()) {
        Ok(cancel) => cancel,
        Err(mut running) => {
            running["status"] = json!(409);
            return running;
        }
    };
    lock_jobs().with_job(&id, |j| j.output = Some(out.clone()));
    let now = Instant::now();
    offscreen_jobs().lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(OffscreenJob {
        id: id.clone(),
        map: dispatch_map().map(|(map, _)| map),
        out,
        frame,
        started: now,
        deadline: now + timeout.unwrap_or(OFFSCREEN_TIMEOUT),
        last_frame: None,
        frames: 0,
        settled_run: 0,
        cancel,
    });
    json!({ "job": id, "state": "running", "poll": format!("/story/jobs/{id}") })
}

/// Whether an offscreen still is waiting on a pump tick.
pub fn offscreen_pending() -> bool {
    !offscreen_jobs().lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty()
}

/// The pump's turn for the offscreen stills queued on `map` (`map_id`):
/// one frame each, no more often than [`OFFSCREEN_FRAME_GAP`]; a still
/// whose map has settled (after [`OFFSCREEN_MIN_SPAN`]) or whose deadline
/// passed is read back and written off this thread. Called by the pump
/// inside its update of the live map, every tick.
pub fn offscreen_tick(map_id: EntityId, map: &mut MapView, cx: &mut gpui::Context<MapView>) {
    let mut queue = offscreen_jobs().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if queue.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut i = 0;
    while i < queue.len() {
        let job = &mut queue[i];
        if job.cancel.load(Ordering::SeqCst) {
            let job = queue.remove(i);
            finish_offscreen(&job.id, Err(("cancelled".into(), JobState::Cancelled)));
            continue;
        }
        if job.map.is_some_and(|m| m != map_id) {
            // Its map is not the live one (the slot moved on).
            if now >= job.deadline {
                let job = queue.remove(i);
                finish_offscreen(&job.id, Err(("the map this still was asked of is no longer open".into(), JobState::Failed)));
            } else {
                i += 1;
            }
            continue;
        }
        if job.last_frame.is_some_and(|t| now.duration_since(t) < OFFSCREEN_FRAME_GAP) {
            i += 1;
            continue;
        }
        job.last_frame = Some(now);
        job.frames += 1;
        match map.render_offscreen(job.frame, cx) {
            Err(e) => {
                let job = queue.remove(i);
                finish_offscreen(&job.id, Err((format!("{e:#}"), JobState::Failed)));
            }
            Ok(rendered) => {
                job.settled_run = if rendered.settled { job.settled_run + 1 } else { 0 };
                let done = job.settled_run >= 2 && now.duration_since(job.started) >= OFFSCREEN_MIN_SPAN;
                if done || now >= job.deadline {
                    let job = queue.remove(i);
                    write_offscreen(job, rendered.grab, rendered.settled);
                } else {
                    i += 1;
                }
            }
        }
    }
}

/// Reads `grab` back and writes it as `job.out`, on a thread of its own.
fn write_offscreen(job: OffscreenJob, grab: gpui_maps::render::frame_grab::FrameGrab, settled: bool) {
    let id = job.id.clone();
    let spawned = std::thread::Builder::new().name("offscreen-still".into()).spawn(move || {
        let written = (|| -> anyhow::Result<()> {
            let frame = grab.read().map_err(|e| anyhow::anyhow!(e))?;
            let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
                .ok_or_else(|| anyhow::anyhow!("the frame's pixels do not fill it"))?;
            if let Some(parent) = job.out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            image.save_with_format(&job.out, image::ImageFormat::Png)?;
            Ok(())
        })();
        let answer = written.map(|()| {
            json!({
                "ok": true,
                "path": job.out,
                "layers": true,
                "width": job.frame.width_px,
                "height": job.frame.height_px,
                "scale": job.frame.scale,
                "settled": settled,
                "frames": job.frames,
            })
        });
        finish_offscreen(&job.id, answer.map_err(|e| (format!("{e:#}"), JobState::Failed)));
    });
    if let Err(e) = spawned {
        finish_offscreen(&id, Err((format!("could not start the write: {e}"), JobState::Failed)));
    }
}

/// Ends offscreen job `id` with its answer or its error.
fn finish_offscreen(id: &str, result: Result<Value, (String, JobState)>) {
    lock_jobs().with_job(id, |j| match result {
        Ok(answer) => {
            j.state = JobState::Done;
            j.result = Some(answer);
        }
        Err((error, state)) => {
            j.state = state;
            j.error = Some(error);
        }
    });
}

fn answer(map: &mut MapView, request: StoryRequest) -> Value {
    match request {
        StoryRequest::Validate { script, offline, base_dir, estimate } => validate(&script, offline, base_dir, estimate),
        StoryRequest::Render(req) => render(req),
        StoryRequest::Job(id) => lock_jobs().status(&id),
        StoryRequest::Cancel(id) => lock_jobs().cancel(&id),
        StoryRequest::Image { out, scene: None, width, height, overwrite, layers: true, scale, timeout, .. } => {
            offscreen_image(map, out, width, height, scale, timeout, overwrite)
        }
        StoryRequest::Image { out, scene, at, width, height, overwrite, .. } => {
            let scene = resolve_image_scene(map, scene, width, height);
            image(out, scene, at, overwrite)
        }
        StoryRequest::Preview(req) => match dispatch_map() {
            Some((id, handle)) => preview(map, id, Some(handle), &req),
            None => no_map_identity(),
        },
        StoryRequest::PreviewStop => match dispatch_map() {
            Some((id, _)) => preview_stop(map, id),
            None => no_map_identity(),
        },
    }
}

fn no_map_identity() -> Value {
    json!({ "error": "this call did not come through the agent pump, so the map it is for is unknown" })
}

fn dispatch(map: &mut MapView, action_name: &str, path_param: Option<&str>, query: &str, body: &str) -> Value {
    match parse_story_action(action_name, path_param, query, body) {
        Some(request) => answer(map, request),
        None => json!({
            "error": "could not parse that call",
            "action": action_name,
            "hint": "GET / lists every action with its params",
        }),
    }
}

pub fn story_registration() -> ModuleRegistration<MapView> {
    ModuleRegistration { prefix: "story", actions: STORY_ACTIONS, dispatch }
}

pub fn capture_registration() -> ModuleRegistration<MapView> {
    ModuleRegistration { prefix: "capture", actions: CAPTURE_ACTIONS, dispatch }
}

// ---------------------------------------------------------------------------
// Preview (S9): a story played on the live map, in real time
// ---------------------------------------------------------------------------

/// Story time at `elapsed_s` wall seconds after the preview started, of
/// which `paused_for_s` were spent paused (settle points): `from_s` plus
/// the unpaused wall time times `rate`, wrapped into `[from_s, to_s)` when
/// `looped`, else held at `to_s` once reached. The `bool` is "finished"
/// (never for a loop). An empty range is finished at once (a loop over
/// one is held at `from_s`). Pure: the preview's only arithmetic.
pub fn preview_time(elapsed_s: f64, from_s: f64, to_s: f64, rate: f64, looped: bool, paused_for_s: f64) -> (f64, bool) {
    let story = (elapsed_s - paused_for_s).max(0.0) * rate;
    let span = to_s - from_s;
    if span.is_nan() || span <= 0.0 || !story.is_finite() {
        return if looped { (from_s, false) } else { (to_s.max(from_s), true) };
    }
    if looped {
        (from_s + story.rem_euclid(span), false)
    } else if story >= span {
        (to_s, true)
    } else {
        (from_s + story, false)
    }
}

/// A preview in progress: the resolved story and its clock.
pub struct PreviewState {
    path: CameraPath,
    timeline: Timeline,
    /// The story frame and overlay layers, composed as a render composes
    /// them (Task 20): drawn by the map itself, so the preview shows the
    /// video's panels, images, callouts, furniture, captions and layers.
    /// Built on a worker (images, mip chains, callouts); the clock holds
    /// at `from` (`preparing`) until it is.
    live: Live,
    /// The overlay layers, pushed to the map when the preview starts.
    layers: Vec<(bool, story::LayerSource)>,
    started: Instant,
    from_s: f64,
    to_s: f64,
    rate: f64,
    looped: bool,
    /// `(settle time, when the pause began)` while the clock waits at a
    /// settle point.
    paused_at_settle: Option<(f64, Instant)>,
    /// Wall seconds spent paused so far, plus each pause's overshoot (the
    /// story time a frame ran past the settle point, in wall seconds), so
    /// the clock resumes AT the settle point.
    paused_for_s: f64,
    /// The current pause's timeout, and `paints_entered` when it began:
    /// "no pending tile" counts only from a frame painted at the settle
    /// pose.
    settle_timeout_s: f64,
    settle_paints: u64,
    /// The story time of the last tick; a settle point in `(last_t, t]` is
    /// crossed this tick.
    last_t: f64,
    /// The settle point last waited at: never crossed again until a loop
    /// wraps (the resumed clock can land a hair before it).
    settled_through: f64,
    fps: u32,
    /// Every style generation, compiled on a worker thread when the
    /// preview starts (review F5): no compile lands mid-play.
    styles: Styles,
    applied_generation: Option<u64>,
    style_error: Option<String>,
    /// The view size (logical px) the path, timeline and story frame were
    /// planned at. The story's pose is a function of (script, t, view
    /// size) -- as MapLibre's camera state survives `resize` and only its
    /// matrices are re-derived -- so a view that settles or resizes after
    /// the preview began is re-planned ([`PreviewState::replan_if_resized`]),
    /// never left on the pose of the size it opened at.
    planned_for: (f32, f32),
    /// What a re-plan needs: the script, where its files resolve, and the
    /// request's own `from`/`to` (a fly's length is the view's).
    script: story::Script,
    base_dir: PathBuf,
    req_from: Option<f64>,
    req_to: Option<f64>,
    /// The clock has started: a re-plan's story frame building on a worker
    /// no longer holds it at `from`.
    clock_started: bool,
}

/// The compiled generations of a preview's timeline: compiling on a
/// worker, then ready. Each entry is the generation's style or why it
/// does not compile.
enum Styles {
    Compiling(Receiver<Vec<Result<Arc<gpui_maps::style::Style>, String>>>),
    Ready(Vec<Result<Arc<gpui_maps::style::Style>, String>>),
}

impl Styles {
    /// Starts compiling every generation of `timeline` on a thread of its
    /// own (the UI thread never compiles); a thread that cannot start
    /// compiles here instead.
    fn compile_in_background(timeline: &Timeline) -> Self {
        let timeline = timeline.clone();
        let compile_all = move || -> Vec<Result<Arc<gpui_maps::style::Style>, String>> {
            (0..timeline.generations())
                .map(|g| story::style_cache::compile(&timeline, g).map(Arc::new).map_err(|e| format!("style generation {g}: {e:#}")))
                .collect()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let job = {
            let tx = tx.clone();
            let compile_all = compile_all.clone();
            move || {
                let _ = tx.send(compile_all());
            }
        };
        match std::thread::Builder::new().name("story-preview-styles".into()).spawn(job) {
            Ok(_) => Styles::Compiling(rx),
            Err(_) => Styles::Ready(compile_all()),
        }
    }

    /// Ready now (polled without blocking)?
    fn poll(&mut self) -> bool {
        if let Styles::Compiling(rx) = self {
            match rx.try_recv() {
                Ok(done) => *self = Styles::Ready(done),
                Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    *self = Styles::Ready(vec![Err("the style worker stopped".into())]);
                }
            }
        }
        true
    }

    /// Blocks until compiled (tests).
    #[cfg(test)]
    fn wait(&mut self) {
        if let Styles::Compiling(rx) = self {
            *self = Styles::Ready(rx.recv().unwrap_or_default());
        }
    }

    fn get(&self, generation: u64) -> Option<&Result<Arc<gpui_maps::style::Style>, String>> {
        match self {
            Styles::Ready(all) => usize::try_from(generation).ok().and_then(|g| all.get(g)),
            Styles::Compiling(_) => None,
        }
    }
}

/// A preview's [`story::LiveStory`]: building on a worker, then ready
/// (its mip chains handed to the map once).
enum Live {
    Building(Receiver<story::LiveStory>),
    Ready { live: Box<story::LiveStory>, mips_sent: bool },
    /// The worker stopped without one: the preview plays without its
    /// story frame and layer effects.
    Failed,
}

impl Live {
    /// Starts building `plan` on a thread of its own; a thread that cannot
    /// start builds here instead.
    fn build_in_background(plan: story::LiveStoryPlan) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let slot = std::sync::Arc::new(Mutex::new(Some(plan)));
        let job = {
            let slot = std::sync::Arc::clone(&slot);
            move || {
                let plan = slot.lock().ok().and_then(|mut p| p.take());
                if let Some(plan) = plan {
                    let _ = tx.send(plan.build());
                }
            }
        };
        match std::thread::Builder::new().name("story-preview-live".into()).spawn(job) {
            Ok(_) => Live::Building(rx),
            Err(_) => match slot.lock().ok().and_then(|mut p| p.take()) {
                Some(plan) => Live::Ready { live: Box::new(plan.build()), mips_sent: false },
                None => Live::Failed,
            },
        }
    }

    /// Ready (or failed) now, polled without blocking?
    fn poll(&mut self) -> bool {
        if let Live::Building(rx) = self {
            match rx.try_recv() {
                Ok(live) => *self = Live::Ready { live: Box::new(live), mips_sent: false },
                Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => *self = Live::Failed,
            }
        }
        true
    }

    /// Blocks until built (tests).
    #[cfg(test)]
    fn wait(&mut self) {
        if let Live::Building(rx) = self {
            *self = rx.recv().map_or(Live::Failed, |live| Live::Ready { live: Box::new(live), mips_sent: false });
        }
    }
}

/// One tick's outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewStatus {
    pub t: f64,
    /// Index into the script's `camera` array.
    pub segment: Option<usize>,
    /// The story clock advances (false once a non-looping preview ends).
    pub playing: bool,
    pub finished: bool,
    /// Paused at a settle point until the cover's tiles arrive.
    pub waiting_on_tiles: bool,
    pub style_generation: u64,
    pub captions: Vec<ActiveCaption>,
    /// The style generations are still compiling: the clock has not
    /// started.
    pub preparing: bool,
}

impl PreviewState {
    /// `script`'s camera path and timeline for a viewport of `viewport`
    /// logical pixels, playing `from..to` at `rate` from `started`; the
    /// cards framed around the view's `corner` (its credit and mark).
    fn new(
        script: &story::Script,
        docs: story::SceneDocuments,
        base_dir: &Path,
        viewport: (f32, f32),
        corner: &story::ViewCorner,
        req: &PreviewRequest,
        started: Instant,
    ) -> Result<Self, Value> {
        let plan = story::LiveStory::plan(script, base_dir, docs, viewport, 1.0, corner)
            .map_err(|(stage, d)| json!({ "ok": false, "error": format!("the {stage} does not resolve"), "diagnostics": d }))?;
        let (path, timeline, layers) = (plan.path().clone(), plan.timeline().clone(), plan.layers().to_vec());
        let live = Live::build_in_background(plan);
        let duration = path.duration();
        let from_s = req.from.unwrap_or(0.0).min(duration);
        let to_s = req.to.unwrap_or(duration).min(duration);
        if to_s < from_s {
            return Err(json!({ "ok": false, "error": format!("`to` ({to_s}) is before `from` ({from_s})"), "duration": duration }));
        }
        let styles = Styles::compile_in_background(&timeline);
        Ok(Self {
            path,
            timeline,
            live,
            layers,
            started,
            from_s,
            to_s,
            rate: req.rate,
            looped: req.looped,
            paused_at_settle: None,
            paused_for_s: 0.0,
            settle_timeout_s: 0.0,
            settle_paints: 0,
            // Just before `from`, so a settle point AT `from` is crossed.
            last_t: from_s - 1e-9,
            settled_through: f64::NEG_INFINITY,
            fps: script.output().fps.max(1),
            styles,
            applied_generation: None,
            style_error: None,
            planned_for: viewport,
            script: script.clone(),
            base_dir: base_dir.to_path_buf(),
            req_from: req.from,
            req_to: req.to,
            clock_started: false,
        })
    }

    /// Re-plans the path, timeline and story frame for `map`'s view size
    /// when it is not the one they were planned at (the window settled
    /// after the preview was POSTed, or was resized); `true` if it did.
    /// The style generations and overlay sources do not depend on the
    /// size and are kept. A re-plan that fails keeps the old plan.
    fn replan_if_resized(&mut self, map: &MapView) -> bool {
        let c = map.camera();
        let viewport = (c.viewport_width_px, c.viewport_height_px);
        if viewport == self.planned_for {
            return false;
        }
        self.planned_for = viewport;
        let Ok(docs) = story::scene_documents(&self.script, &self.base_dir) else { return false };
        let Ok(plan) = story::LiveStory::plan(&self.script, &self.base_dir, docs, viewport, 1.0, &map.story_view_corner()) else {
            return false;
        };
        self.path = plan.path().clone();
        self.timeline = plan.timeline().clone();
        self.live = Live::build_in_background(plan);
        let duration = self.path.duration();
        self.from_s = self.req_from.unwrap_or(0.0).min(duration);
        self.to_s = self.req_to.unwrap_or(duration).min(duration).max(self.from_s);
        true
    }

    /// The first settle point in `(after, upto]`, with its timeout.
    fn settle_between(&self, after: f64, upto: f64) -> Option<(f64, f64)> {
        self.timeline
            .settle_points()
            .iter()
            .copied()
            .filter(|(s, _)| *s > after && *s <= upto)
            .min_by(|a, b| a.0.total_cmp(&b.0))
    }
}

/// Whether any tile of the last painted cover is still on its way.
fn cover_pending(sample: &gpui_maps::map_view::TileStatusSample) -> bool {
    sample.statuses.iter().any(|s| matches!(s.data, TileDataState::Pending | TileDataState::Unknown))
}

/// Puts story time `t` on `map`: the pose (`jump_to`), the globe, the
/// style generation (pre-compiled when the preview started, applied only
/// when it changes). Answers the scene, for its captions.
fn apply_story_time(map: &mut MapView, state: &mut PreviewState, t: f64) -> story::SceneState {
    let pose = state.path.pose_at(t);
    let scene = state.timeline.state_at(t, state.fps);
    let c = pose.camera;
    // An authored pose is drawn as written, the way the render draws it:
    // the interactive camera's low-zoom pitch limit would put a pitched
    // keyframe at z3 somewhere the rendered frame does not. `put_back`
    // restores the host's setting.
    map.set_low_zoom_pitch_limit(false);
    // The pose is the render's, in the capture's frame (1080 units of short
    // edge): this window shows the same ground at the offset zoom (spec
    // §13). The one place a story pose becomes the view's camera.
    let view = map.camera();
    let zoom = c.zoom + story::live_zoom_offset((view.viewport_width_px, view.viewport_height_px));
    map.jump_to(CameraTarget {
        center: Some((c.center_lon, c.center_lat)),
        zoom: Some(zoom),
        pitch: Some(c.pitch),
        bearing: Some(c.bearing),
        anchor_offset_px: (0.0, 0.0),
    });
    let globe = scene.globe.unwrap_or(pose.globe);
    if map.globe_enabled() != globe {
        map.set_globe_enabled(globe);
    }
    // The story frame and the overlay layers' effects, drawn by the map
    // (spec R2): the same composition a render's frame `t` gets.
    if let Live::Ready { live, mips_sent } = &mut state.live {
        if !*mips_sent {
            map.set_story_image_mips(live.image_mips());
            *mips_sent = true;
        }
        let (frame, captions) = live.frame_at(&scene, t);
        map.set_story_frame_deferred(frame);
        map.set_story_captions(captions);
        // The per-frame marks (markers, labels, the head, arrowheads),
        // before the effects: a marker layer's own dots stay hidden.
        map.set_story_marks_deferred(live.story_layers_at(&scene, t));
        map.apply_story_layer_fx_deferred(&live.layer_fx(&scene));
    }
    let generation = scene.style_generation;
    if state.applied_generation != Some(generation) {
        let Some(compiled) = state.styles.get(generation) else {
            // Still compiling (only before the clock starts): the map
            // keeps its style for now.
            return scene;
        };
        state.applied_generation = Some(generation);
        match compiled {
            Ok(style) => {
                map.set_style((**style).clone());
                if let Some(doc) = state.timeline.style_document(generation) {
                    map.set_style_document(doc.clone());
                }
                state.style_error = None;
            }
            // The map keeps the style it has; the state names why.
            Err(e) => state.style_error = Some(e.clone()),
        }
    }
    scene
}

/// One frame of a preview: advances the story clock to `now` (pausing at
/// a settle point until `map.tile_status_sample()` has no pending cover
/// tile, or the settle's timeout) and applies that story time to `map`.
/// Shell code: the only place a story reads the wall clock (spec §6.2).
pub fn preview_tick(map: &mut MapView, state: &mut PreviewState, now: Instant) -> PreviewStatus {
    state.replan_if_resized(map);
    // Both halves the worker threads build: the style generations and the
    // story's images and callouts. `&` so both are polled every tick. Only
    // the first build holds the clock: a re-plan's frame joins when ready.
    if !(state.styles.poll() & state.live.poll()) && !state.clock_started {
        // The generations are still compiling: hold at `from` and start
        // the clock only once they are all ready.
        state.started = now;
        let t = state.from_s;
        let scene = apply_story_time(map, state, t);
        return PreviewStatus {
            t,
            segment: state.path.segment_index_at(t),
            playing: true,
            finished: false,
            waiting_on_tiles: false,
            style_generation: scene.style_generation,
            captions: scene.captions,
            preparing: true,
        };
    }
    state.clock_started = true;
    let sample = map.tile_status_sample();
    if let Some((settle_t, since)) = state.paused_at_settle {
        let waited = now.saturating_duration_since(since).as_secs_f64();
        let painted_at_settle = sample.paints_entered > state.settle_paints;
        if (painted_at_settle && !cover_pending(&sample)) || waited >= state.settle_timeout_s {
            state.paused_for_s += waited;
            state.paused_at_settle = None;
            state.settled_through = settle_t;
        } else {
            let scene = apply_story_time(map, state, settle_t);
            return PreviewStatus {
                t: settle_t,
                segment: state.path.segment_index_at(settle_t),
                playing: true,
                finished: false,
                waiting_on_tiles: true,
                style_generation: scene.style_generation,
                captions: scene.captions,
                preparing: false,
            };
        }
    }
    let elapsed = now.saturating_duration_since(state.started).as_secs_f64();
    let (mut t, mut finished) = preview_time(elapsed, state.from_s, state.to_s, state.rate, state.looped, state.paused_for_s);
    if t < state.last_t - 1e-6 {
        // A loop wrapped: settle points from `from` on count again.
        state.last_t = state.from_s - 1e-9;
        state.settled_through = f64::NEG_INFINITY;
    }
    let mut waiting = false;
    if let Some((settle_t, timeout)) = state.settle_between(state.last_t.max(state.settled_through), t) {
        // Hold the clock AT the settle point: the overshoot is paid back.
        state.paused_for_s += (t - settle_t) / state.rate;
        state.paused_at_settle = Some((settle_t, now));
        state.settle_timeout_s = timeout;
        state.settle_paints = sample.paints_entered;
        t = settle_t;
        finished = false;
        waiting = true;
    }
    state.last_t = t;
    let scene = apply_story_time(map, state, t);
    PreviewStatus {
        t,
        segment: state.path.segment_index_at(t),
        playing: !finished,
        finished,
        waiting_on_tiles: waiting,
        style_generation: scene.style_generation,
        captions: scene.captions,
        preparing: false,
    }
}

/// What the map showed before a preview, put back by `preview_stop`.
struct Restore {
    target: CameraTarget,
    globe: bool,
    /// The host's low-zoom pitch limit, which a preview turns off.
    pitch_limit: bool,
    /// The building look a preview replaces with `scene.buildings`.
    extrusion: gpui_maps::map_view::ExtrusionOverrides,
    style: gpui_maps::style::Style,
    document: Option<Value>,
    recording: bool,
    /// The story layer sources the preview pushed: removed on the way
    /// back, with the story frame cleared.
    story_sources: Vec<String>,
}

impl Restore {
    fn of(map: &MapView) -> Self {
        let c = map.camera();
        Self {
            target: CameraTarget {
                center: Some((c.center_lon, c.center_lat)),
                zoom: Some(c.zoom),
                // The user's pitch, not the drawn one: put back at a low
                // zoom, the camera limits it again and keeps the intent.
                pitch: Some(map.pitch_intent()),
                bearing: Some(c.bearing),
                anchor_offset_px: (0.0, 0.0),
            },
            globe: map.globe_enabled(),
            pitch_limit: map.low_zoom_pitch_limit(),
            extrusion: map.extrusion_overrides(),
            style: map.style().clone(),
            document: map.style_document().cloned(),
            recording: map.tile_status_recording(),
            story_sources: Vec::new(),
        }
    }

    fn put_back(self, map: &mut MapView) {
        map.set_globe_enabled(self.globe);
        map.set_low_zoom_pitch_limit(self.pitch_limit);
        map.set_extrusion_overrides(self.extrusion);
        map.jump_to(self.target);
        map.set_style(self.style);
        if let Some(doc) = self.document {
            map.set_style_document(doc);
        }
        map.set_tile_status_recording(self.recording);
        map.set_story_frame_deferred(None);
        map.set_story_captions(Vec::new());
        for name in &self.story_sources {
            map.remove_source_deferred(name);
        }
        map.clear_story_layer_state();
    }
}

struct Preview {
    state: PreviewState,
    restore: Restore,
    /// The map it drives, by gpui entity id (never reused: a slotmap key
    /// carries its version), so another map's root leaves it alone.
    map: EntityId,
    /// That map, for restoring it from outside its own frames (the agent
    /// pump, when the gallery switches away from it). `None` in tests
    /// that drive a bare `MapView`.
    handle: Option<WeakEntity<MapView>>,
    status: Option<PreviewStatus>,
}

/// A preview taken off its map without the map in reach: restored the
/// next time that map's root draws (`preview_frame`) or the agent pump
/// sweeps (`release_previews_except`), whichever comes first.
struct Orphan {
    map: EntityId,
    handle: Option<WeakEntity<MapView>>,
    restore: Restore,
}

#[derive(Default)]
struct Previews {
    active: Option<Preview>,
    orphans: Vec<Orphan>,
}

fn previews() -> &'static Mutex<Previews> {
    static P: OnceLock<Mutex<Previews>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Previews::default()))
}

fn lock_preview() -> std::sync::MutexGuard<'static, Previews> {
    previews().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

thread_local! {
    /// The map the agent pump is dispatching to right now (set around each
    /// `dispatch` call by `agentic.rs`): the story actions' identity for
    /// "this map", since a `ModuleRegistration` dispatch gets only
    /// `&mut MapView`.
    static DISPATCH_MAP: std::cell::RefCell<Option<(EntityId, WeakEntity<MapView>)>> = const { std::cell::RefCell::new(None) };
}

/// Called by the agent pump around its dispatch to `map` (`None` after).
pub fn set_dispatch_map(map: Option<&gpui::Entity<MapView>>) {
    DISPATCH_MAP.with(|d| *d.borrow_mut() = map.map(|m| (m.entity_id(), m.downgrade())));
}

fn dispatch_map() -> Option<(EntityId, WeakEntity<MapView>)> {
    DISPATCH_MAP.with(|d| d.borrow().clone())
}

/// The shell's per-frame hook (`shell::install_preview_hook`): restores
/// `map` if a preview left it orphaned, else ticks the preview driving it,
/// if any.
fn preview_frame(map: &mut MapView, id: EntityId, now: Instant) -> Option<crate::shell::PreviewFrame> {
    let mut guard = lock_preview();
    if let Some(i) = guard.orphans.iter().position(|o| o.map == id) {
        let orphan = guard.orphans.remove(i);
        drop(guard);
        orphan.restore.put_back(map);
        return None;
    }
    let preview = guard.active.as_mut().filter(|p| p.map == id)?;
    if let Some(last_t) = preview.status.as_ref().filter(|s| s.finished).map(|s| s.t) {
        // Ended: the last frame stays, captions included, and no more
        // frames are asked for -- unless the view was resized, when the
        // last frame is re-planned for the new size and re-applied until
        // its story frame is built.
        let state = &mut preview.state;
        let resized = state.replan_if_resized(map);
        let was_building = matches!(state.live, Live::Building(_));
        let ready = state.live.poll();
        if resized || (was_building && ready) {
            apply_story_time(map, state, last_t.min(state.to_s));
        }
        return Some(crate::shell::PreviewFrame { captions: Vec::new(), keep_ticking: !ready });
    }
    let status = preview_tick(map, &mut preview.state, now);
    // The captions are the map's to draw now (in its story frame, as the
    // render places them), not a gpui layer's.
    let frame = crate::shell::PreviewFrame { captions: Vec::new(), keep_ticking: status.playing };
    preview.status = Some(status);
    Some(frame)
}

/// The agent pump's sweep, every poll (review F2): a preview whose map is
/// no longer the live one (the gallery switched panels, or closed the
/// last) is stopped, and every map a preview left is restored if it still
/// exists. A map that was dropped needs nothing.
pub fn release_previews_except(current: Option<EntityId>, cx: &mut gpui::App) {
    let released: Vec<Orphan> = {
        let mut guard = lock_preview();
        if guard.active.as_ref().is_some_and(|p| Some(p.map) != current) {
            if let Some(p) = guard.active.take() {
                guard.orphans.push(Orphan { map: p.map, handle: p.handle, restore: p.restore });
            }
        }
        // An orphan whose map is the live one waits for that map's own
        // frame only if no handle reaches it; otherwise all go now.
        let (go, stay): (Vec<Orphan>, Vec<Orphan>) = std::mem::take(&mut guard.orphans).into_iter().partition(|o| o.handle.is_some());
        guard.orphans = stay;
        go
    };
    for orphan in released {
        if let Some(map) = orphan.handle.as_ref().and_then(WeakEntity::upgrade) {
            map.update(cx, |map, cx| {
                orphan.restore.put_back(map);
                cx.notify();
            });
        }
    }
}

/// What a preview does NOT show that the render does (review F11): the
/// preview drives the live map, which keeps its own source and settings.
/// Said in the action description, the preview's answer and
/// `GET /map/state`'s `story`.
pub const PREVIEW_NOT_APPLIED: &[&str] = &[
    "scene.basemap (the live map keeps its own tile source)",
    "scene.raster",
    "scene.labels, atmosphere, stars and terminator, and set_param/tween events",
    "overlays without layers, and scene.tracks (tracks steer follow segments but are not drawn)",
    "an unthemed layer's draw head and pulse/drop rings (the layers' trim, opacity and per-feature paint, and a themed layer's head, markers, labels and arrowheads ARE drawn)",
    "remote images not already in the image cache (the preview never fetches on the UI thread)",
    "the burned-in wordmark (the map draws its own credit and SWAT mark)",
    "the render's credit: the map credits its own source, not the story basemap's (and acknowledgements), so the credit \
     plate -- and the panels and captions laid out clear of it -- can sit differently near the bottom corners",
];

/// `story.preview`: resolves the script (files only: no tile source, no
/// token, no network), snapshots what the map shows, starts compiling the
/// style generations on a worker and starts the clock once they are
/// ready. Answers at once; the shell's per-frame hook does the playing.
fn preview(map: &mut MapView, id: EntityId, handle: Option<WeakEntity<MapView>>, req: &PreviewRequest) -> Value {
    let raw = match serde_json::to_vec(&req.script) {
        Ok(raw) => raw,
        Err(e) => return json!({ "ok": false, "error": e.to_string() }),
    };
    let script = match parse_or_answer(&raw) {
        Ok(script) => script,
        Err(answer) => return answer,
    };
    let docs = match story::scene_documents(&script, &base_dir_or_cwd(req.base_dir.clone())) {
        Ok(docs) => docs,
        Err(e) => return json!({ "ok": false, "error": format!("{e:#}") }),
    };
    let camera = map.camera();
    let viewport = (camera.viewport_width_px, camera.viewport_height_px);
    let base_dir = base_dir_or_cwd(req.base_dir.clone());
    let state = match PreviewState::new(&script, docs, &base_dir, viewport, &map.story_view_corner(), req, Instant::now()) {
        Ok(state) => state,
        Err(answer) => return answer,
    };
    let answer = json!({
        "ok": true,
        "from": state.from_s,
        "to": state.to_s,
        "duration": state.path.duration(),
        "rate": state.rate,
        "loop": state.looped,
        "settle_points": state.timeline.settle_points().len(),
        "style_generations": state.timeline.generations(),
        "state": "GET /map/state has `story` while it runs; `preparing` while the style generations compile",
        "not_applied": PREVIEW_NOT_APPLIED,
    });
    let mut guard = lock_preview();
    // A preview replacing one on this map keeps the FIRST snapshot: that
    // is what the map showed before any preview. One on another map
    // leaves that map to be restored (review F2).
    let restore = match guard.active.take() {
        Some(old) if old.map == id => old.restore,
        Some(old) => {
            guard.orphans.push(Orphan { map: old.map, handle: old.handle, restore: old.restore });
            Restore::of(map)
        }
        None => match guard.orphans.iter().position(|o| o.map == id) {
            // This map is still owed its pre-preview scene: keep that one.
            Some(i) => guard.orphans.remove(i).restore,
            None => Restore::of(map),
        },
    };
    map.set_tile_status_recording(true);
    // The render's building look (`scene.buildings`), so the preview's
    // buildings are the render's (review F11); `preview_stop` puts the
    // map's own back.
    let (look, label_occlusion) = script.scene.buildings.capture();
    map.set_extrusion_overrides(gpui_maps::map_view::ExtrusionOverrides { look, label_occlusion, ..map.extrusion_overrides() });
    // The story's overlay layers, each a source of its own (the effects
    // drive them every frame); a replaced preview's go first.
    let mut restore = restore;
    for name in std::mem::take(&mut restore.story_sources) {
        map.remove_source_deferred(&name);
    }
    map.clear_story_layer_state();
    let mut layer_errors = Vec::new();
    for (above_labels, source) in &state.layers {
        match map.set_story_layer_source(source, *above_labels) {
            Ok(()) => restore.story_sources.push(story::overlay_layers::live_source_name(&source.layer_id)),
            Err(e) => layer_errors.push(format!("{e:#}")),
        }
    }
    let mut answer = answer;
    if !layer_errors.is_empty() {
        answer["layer_errors"] = json!(layer_errors);
    }
    guard.active = Some(Preview { state, restore, map: id, handle, status: None });
    drop(guard);
    crate::shell::install_preview_hook(preview_frame);
    answer
}

/// `story.preview_stop`: ends the preview on `map` and restores its scene.
fn preview_stop(map: &mut MapView, id: EntityId) -> Value {
    let mut guard = lock_preview();
    let restore = if guard.active.as_ref().is_some_and(|p| p.map == id) {
        guard.active.take().map(|p| p.restore)
    } else {
        guard.orphans.iter().position(|o| o.map == id).map(|i| guard.orphans.remove(i).restore)
    };
    drop(guard);
    match restore {
        Some(restore) => {
            restore.put_back(map);
            json!({ "stopped": true, "restored": true })
        }
        None => json!({ "stopped": false, "note": "no preview is running on this map" }),
    }
}

/// `GET /map/state`'s `story` for the map being dispatched to: `null`
/// when no preview drives it.
pub fn preview_state_json() -> Value {
    dispatch_map().map_or(Value::Null, |(id, _)| preview_state_json_for(id))
}

fn preview_state_json_for(id: EntityId) -> Value {
    let guard = lock_preview();
    let Some(p) = guard.active.as_ref().filter(|p| p.map == id) else {
        return Value::Null;
    };
    let status = p.status.as_ref();
    json!({
        "playing": status.is_none_or(|s| s.playing),
        "finished": status.is_some_and(|s| s.finished),
        "preparing": status.is_none_or(|s| s.preparing),
        "t": status.map_or(p.state.from_s, |s| s.t),
        "segment": status.and_then(|s| s.segment),
        "waiting_on_tiles": status.is_some_and(|s| s.waiting_on_tiles),
        "style_generation": status.map(|s| s.style_generation),
        "style_error": p.state.style_error,
        "captions": status.map_or(0, |s| s.captions.len()),
        "from": p.state.from_s,
        "to": p.state.to_s,
        "rate": p.state.rate,
        "loop": p.state.looped,
        "ticked": status.is_some(),
        "not_applied": PREVIEW_NOT_APPLIED,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_BODY: &str = r#"{"script":{"story":1},"out":"/tmp/x.mp4"}"#;

    /// The in-process twin of the curl run: the script the tests render.
    fn det_script() -> Value {
        let raw = std::fs::read(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/tests/fixtures/stories/det.json")).expect("det.json");
        let mut v: Value = serde_json::from_slice(&raw).expect("det.json is JSON");
        v["scene"]["style"] = json!(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/examples/liberty-style.json"));
        v
    }

    /// [`out_root`] under test: `None` is `~/Movies/gpui-map`.
    pub(super) static OUT_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

    /// Serialises the tests that use the process-wide job slot, and
    /// confines their outputs to the temp dir (where `tempdir()` puts them).
    fn slot() -> std::sync::MutexGuard<'static, ()> {
        static S: Mutex<()> = Mutex::new(());
        *OUT_ROOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::env::temp_dir());
        S.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_done(id: &str) -> Value {
        for _ in 0..1200 {
            let s = lock_jobs().status(id);
            if s["state"] != "running" {
                return s;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("job {id} did not finish within 120 s");
    }

    #[test]
    fn every_declared_story_action_is_routable() {
        gpui_agentic_http::assert_registry_routable(&story_registration(), |a| parse_story_action(a.name, Some("job-1"), "", SAMPLE_BODY).is_some());
        gpui_agentic_http::assert_registry_routable(&capture_registration(), |a| parse_story_action(a.name, None, "", r#"{"out":"/tmp/x.png"}"#).is_some());
    }

    #[test]
    fn a_second_render_while_one_runs_is_409() {
        let mut jobs = Jobs::default();
        let first = jobs.start_for_test("a");
        assert!(first.is_ok());
        assert_eq!(jobs.start_for_test("b").unwrap_err(), serde_json::json!({"running": "a"}));
        // Once it is no longer running, the slot is free again.
        jobs.with_job("a", |j| j.state = JobState::Done);
        assert!(jobs.start_for_test("b").is_ok());
    }

    /// Task 8 review Finding 1: `render_job` is the agent's own render
    /// path -- it calls `story::scene_inputs` directly and never runs
    /// `validate_scene` first, so a `data` overlay whose file cannot be
    /// read must refuse here, exactly as a broken `geojson` overlay
    /// already does, or a published video would silently lack the
    /// overlay with nothing but a gap line to say so.
    #[test]
    fn render_job_refuses_a_missing_overlay_data_file() {
        let mut v = det_script();
        v["scene"]["overlays"] = json!([{
            "id": "route", "data": "nope/route.geojson",
            "layers": [{"id": "route-line", "type": "line"}]
        }]);
        let req = RenderRequest {
            script: v,
            out: std::env::temp_dir().join("task8-f1-render-job-test.mp4"),
            format: Format::Still,
            at: None,
            strict: false,
            base_dir: Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))),
            overwrite: true,
            cleanup: Vec::new(),
        };
        let out = req.out.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let err = render_job(&req, &out, &cancel, "f1-test").expect_err("a missing `data` file must refuse the render");
        assert!(format!("{err:#}").contains("file_not_found"), "{err:#}");
    }

    /// Task 12 review F1: the same holds for an image. `render_job` never
    /// validates, so an image that will not load refuses the render (spec
    /// §7) instead of shipping a video without it.
    #[test]
    fn render_job_refuses_a_missing_image() {
        let mut v = det_script();
        let events = v["events"].as_array_mut().expect("det events");
        events.push(json!({"at": 0.0, "until": 1.0, "image": {"src": "nope/photo.png", "alt": "a", "credit": "c", "placement": "corner"}}));
        let req = RenderRequest {
            script: v,
            out: std::env::temp_dir().join("task12-f1-render-job-test.png"),
            format: Format::Still,
            at: None,
            strict: false,
            base_dir: Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))),
            overwrite: true,
            cleanup: Vec::new(),
        };
        let out = req.out.clone();
        let _ = std::fs::remove_file(&out);
        let cancel = Arc::new(AtomicBool::new(false));
        let err = render_job(&req, &out, &cancel, "t12-f1-test").expect_err("a missing image must refuse the render");
        let text = format!("{err:#}");
        assert!(text.contains("image nope/photo.png (file_not_found)"), "{text}");
        assert!(!out.exists(), "nothing was written");
    }

    #[test]
    fn a_structurally_bad_script_is_answered_inline_with_every_diagnostic() {
        let v = validate_inline(br#"{"story":2,"camera":[]}"#);
        assert_eq!(v["ok"], false);
        assert!(v["diagnostics"].as_array().unwrap().len() >= 2);
        // Through the action too: answered, not deferred.
        let answer = validate(&json!({"story": 2, "camera": []}), true, None, false);
        assert!(answer.get(DEFERRED).is_none(), "{answer}");
        assert_eq!(answer["ok"], false);
    }

    #[test]
    fn a_good_script_validates_through_the_deferred_reply() {
        let answer = validate(&det_script(), true, None, true);
        let d = take_deferred(&answer).expect("the scene checks are deferred");
        let report = resolve(d);
        assert_eq!(report["ok"], true, "{report}");
        assert!(report["estimate"]["frames"].as_u64().unwrap() > 0, "{report}");
        // The default skips the estimate: the same report, `estimate: null`.
        let report = resolve(take_deferred(&validate(&det_script(), true, None, false)).expect("deferred"));
        assert_eq!(report["ok"], true, "{report}");
        assert!(report["estimate"].is_null() && report["diagnostics"].is_array(), "{report}");
    }

    /// Task 19: `story.validate` answers the layout warnings of
    /// `validate_scene`'s report as they are -- a very long lower-third on
    /// a 9:16 reels frame is a `text_truncated` warning at the panel's
    /// event, with its hint, and the story stays `ok`.
    #[test]
    fn story_agentic_validate_returns_layout_warnings_with_their_path() {
        let mut script = det_script();
        script["output"] = json!({"aspect": "9:16", "tier": "hd", "safe_area": "reels", "fps": 10});
        let headline = "The longest container route ever sailed between two ports in a single season, and why it matters for the city ".repeat(10);
        script["events"]
            .as_array_mut()
            .expect("det.json has events")
            .push(json!({"at": 0.0, "until": 2.0, "panel": {"placement": "lower-third", "headline": headline}}));
        let answer = validate(&script, true, None, false);
        let report = resolve(take_deferred(&answer).expect("the scene checks are deferred"));
        assert_eq!(report["ok"], true, "warnings never block: {report}");
        let diagnostics = report["diagnostics"].as_array().expect("diagnostics");
        let ours = diagnostics.iter().find(|d| d["code"] == "text_truncated").unwrap_or_else(|| panic!("no text_truncated: {report}"));
        assert_eq!(ours["path"], "/events/2", "{ours}");
        assert_eq!(ours["severity"], "warning", "{ours}");
        assert!(ours["hint"].is_string(), "{ours}");
    }

    /// `estimate` is opt-in on `story.validate` (Task 19 review F8).
    #[test]
    fn story_agentic_validate_estimate_is_opt_in() {
        let parse = |body: &str| match parse_story_action("validate", None, "", body) {
            Some(StoryRequest::Validate { estimate, .. }) => estimate,
            other => panic!("{other:?}"),
        };
        assert!(!parse(r#"{"script":{"story":1}}"#));
        assert!(parse(r#"{"script":{"story":1},"estimate":true}"#));
    }

    #[test]
    fn a_deferred_reply_past_its_cap_answers_pending() {
        let (_tx, rx) = std::sync::mpsc::channel::<Value>();
        let marker = defer(rx, Duration::from_millis(20), json!({"pending": true, "job": "j"}));
        let (reply, answer) = std::sync::mpsc::channel();
        assert!(reply_or_defer(&marker, reply).is_none(), "a deferred response keeps the reply");
        assert_eq!(answer.recv_timeout(Duration::from_secs(5)).unwrap(), json!({"pending": true, "job": "j"}));
        // Anything else hands the reply straight back.
        let (reply, _answer) = std::sync::mpsc::channel();
        assert!(reply_or_defer(&json!({"ok": true}), reply).is_some());
    }

    #[test]
    fn an_output_inside_the_repository_is_refused_before_a_job_starts() {
        let _slot = slot();
        let out = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).join("agent-refusal.png");
        let req = RenderRequest { script: det_script(), out: out.clone(), format: Format::Still, at: None, strict: false, base_dir: None, overwrite: false, cleanup: Vec::new() };
        let answer = render(req);
        assert!(answer["error"].as_str().unwrap_or("").contains("--allow-repo-output"), "{answer}");
        assert!(answer.get("job").is_none());
        assert!(!out.exists());
    }

    /// The curl run's render half, in process on the fixture tile: a
    /// contact-sheet job runs to `done`, a second render meanwhile is 409,
    /// and the job status names the sheet, which exists.
    #[test]
    fn a_render_job_runs_to_done_and_names_its_contact_sheet() {
        let _slot = slot();
        let dir = tempfile::tempdir().unwrap();
        let mut script = det_script();
        script["scene"]["basemap"] = json!({"source": "fixture"});
        let req = RenderRequest {
            script: script.clone(),
            out: dir.path().join("agent-orbit"),
            format: Format::ContactSheet,
            at: None,
            strict: false,
            base_dir: None,
            overwrite: false,
            cleanup: Vec::new(),
        };
        // The slot held deterministically (a real render can finish before
        // a second call lands): the action answers 409 naming the holder.
        let held = lock_jobs().begin("held-job".into()).expect("the slot is free");
        let second = render(RenderRequest { out: dir.path().join("second"), ..req.clone() });
        assert_eq!(second, json!({"running": "held-job", "status": 409}));
        lock_jobs().with_job("held-job", |j| j.state = JobState::Done);
        drop(held);
        let started = render(req);
        let id = started["job"].as_str().expect("a job id").to_string();
        let status = wait_done(&id);
        assert_eq!(status["state"], "done", "{status}");
        let sheet = PathBuf::from(status["contact_sheet"].as_str().unwrap());
        assert_eq!(sheet, dir.path().join("agent-orbit.png"));
        assert!(sheet.exists() && PathBuf::from(status["manifest"].as_str().unwrap()).exists());
    }

    #[test]
    fn cancel_stops_a_running_render() {
        let _slot = slot();
        let dir = tempfile::tempdir().unwrap();
        let mut script = det_script();
        script["scene"]["basemap"] = json!({"source": "fixture"});
        let req = RenderRequest { script, out: dir.path().join("seq"), format: Format::PngSeq, at: None, strict: false, base_dir: None, overwrite: false, cleanup: Vec::new() };
        let id = render(req)["job"].as_str().expect("a job").to_string();
        assert_eq!(lock_jobs().cancel(&id), json!({"cancelling": id}));
        let status = wait_done(&id);
        // A render this small can finish before the flag is read.
        assert!(status["state"] == "cancelled" || status["state"] == "done", "{status}");
        if status["state"] == "cancelled" {
            assert!(status["error"].as_str().unwrap().contains("cancel"), "{status}");
        }
    }

    /// `capture.image` of a snapshot: the one-frame story it becomes
    /// renders a still with the credit, off the UI thread, and the reply
    /// names what the snapshot left out.
    #[test]
    fn a_snapshot_renders_a_still() {
        let _slot = slot();
        let dir = tempfile::tempdir().unwrap();
        let style: Value = serde_json::from_slice(&std::fs::read(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/examples/liberty-style.json")).unwrap()).unwrap();
        let snapshot = story::SceneSnapshot {
            camera: story::CameraSnapshot { lon: 103.851, lat: 1.2905, zoom: 14.2, pitch_deg: 30.0, bearing_deg: 10.0 },
            globe: false,
            style: Some(style),
            basemap: Some("fixture".into()),
            raster: None,
            overlays: Vec::new(),
            credit: None,
            left_out: vec!["dot-density layers".into()],
            buildings: story::script::BuildingsLook::Maplibre,
        };
        let script = snapshot_script(&snapshot, 320, 200).unwrap();
        let out = dir.path().join("still.png");
        let answer = image(out.clone(), Ok((script, snapshot.left_out.clone())), None, false);
        assert!(answer.get(DEFERRED).is_none(), "a still is a job, never a held reply: {answer}");
        let id = answer["job"].as_str().expect("a job id").to_string();
        let status = wait_done(&id);
        assert_eq!(status["state"], "done", "{status}");
        let reply = &status["result"];
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["left_out"], json!(["dot-density layers"]));
        let png = image::open(&out).unwrap();
        assert_eq!((png.width(), png.height()), (320, 200));
    }

    /// F1: the server answers one request at a time, so `capture.image`
    /// must answer at once and leave the still to a job: a poll and a
    /// cancel of that still are answered while it renders.
    #[test]
    fn a_poll_and_a_cancel_are_answered_while_a_still_renders() {
        let _slot = slot();
        let dir = tempfile::tempdir().unwrap();
        let mut script = det_script();
        script["scene"]["basemap"] = json!({"source": "fixture"});
        let started = std::time::Instant::now();
        let answer = image(dir.path().join("pending.png"), Ok((script, Vec::new())), None, false);
        assert!(started.elapsed() < Duration::from_secs(2), "the image call waited {:?}", started.elapsed());
        assert!(answer.get(DEFERRED).is_none(), "{answer}");
        let id = answer["job"].as_str().expect("a job id").to_string();
        // The poll: the job exists and says where it is.
        let poll = lock_jobs().status(&id);
        assert_eq!(poll["job"], id.as_str());
        assert!(["running", "done"].contains(&poll["state"].as_str().unwrap()), "{poll}");
        // The cancel, through the action path.
        let cancel = parse_story_action("cancel", Some(&id), "", "").unwrap();
        assert_eq!(cancel, StoryRequest::Cancel(id.clone()));
        let answer = lock_jobs().cancel(&id);
        assert!(answer.get("cancelling").is_some() || answer["note"] == "not running", "{answer}");
        let status = wait_done(&id);
        assert!(["cancelled", "done"].contains(&status["state"].as_str().unwrap()), "{status}");
    }

    /// F3: finished validations are evicted oldest first, running ones
    /// never, and the newest (just answered `{pending}`) stays pollable.
    #[test]
    fn validations_are_evicted_oldest_finished_first() {
        let mut jobs = Jobs::default();
        jobs.record_validation("running-0".into(), None);
        for k in 0..VALIDATIONS_KEPT + 4 {
            jobs.record_validation(format!("v-{k}"), Some(json!({"k": k})));
        }
        jobs.record_validation("pending".into(), None);
        assert!(jobs.validations.len() <= VALIDATIONS_KEPT);
        assert_eq!(jobs.status("running-0")["state"], "running", "a running one is never evicted");
        assert_eq!(jobs.status("pending")["state"], "running");
        assert_eq!(jobs.status("v-0")["error"], "unknown job", "the oldest finished went first");
        let newest = format!("v-{}", VALIDATIONS_KEPT + 3);
        assert_eq!(jobs.status(&newest)["state"], "done");
        // Finishing the pending one keeps it too.
        jobs.record_validation("pending".into(), Some(json!({"ok": true})));
        assert_eq!(jobs.status("pending")["state"], "done");
    }

    // --- preview (S9) -----------------------------------------------------

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn preview_time_runs_at_rate_from_from() {
        // Rate 2: one wall second is two story seconds, counted from `from`.
        let (t, done) = preview_time(1.0, 3.0, 20.0, 2.0, false, 0.0);
        assert!(close(t, 5.0) && !done, "{t}");
        let (t, _) = preview_time(0.25, 0.0, 12.0, 2.0, false, 0.0);
        assert!(close(t, 0.5), "{t}");
        // Rate 0.5: slow motion.
        let (t, _) = preview_time(4.0, 0.0, 12.0, 0.5, false, 0.0);
        assert!(close(t, 2.0), "{t}");
    }

    #[test]
    fn preview_time_holds_the_end_or_wraps_a_loop() {
        // Not looped: held at `to` once reached, and finished.
        assert_eq!(preview_time(7.0, 0.0, 12.0, 2.0, false, 0.0), (12.0, true));
        assert_eq!(preview_time(100.0, 2.0, 4.0, 1.0, false, 0.0), (4.0, true));
        // Looped: wraps into [from, to) and never finishes.
        let (t, done) = preview_time(7.0, 0.0, 12.0, 2.0, true, 0.0);
        assert!(close(t, 2.0) && !done, "{t}");
        let (t, done) = preview_time(5.0, 2.0, 4.0, 1.0, true, 0.0);
        assert!(close(t, 3.0) && !done, "{t}");
        // Exactly one span in: back at `from`.
        let (t, _) = preview_time(2.0, 2.0, 4.0, 1.0, true, 0.0);
        assert!(close(t, 2.0), "{t}");
        // An empty range: finished at once, or held at `from` in a loop.
        assert_eq!(preview_time(1.0, 3.0, 3.0, 1.0, false, 0.0), (3.0, true));
        assert_eq!(preview_time(1.0, 3.0, 3.0, 1.0, true, 0.0), (3.0, false));
    }

    #[test]
    fn preview_time_does_not_count_the_settle_pause() {
        // 3 wall seconds in, 1.5 of them paused at rate 2: 3 story seconds.
        let (t, done) = preview_time(3.0, 0.0, 12.0, 2.0, false, 1.5);
        assert!(close(t, 3.0) && !done, "{t}");
        // Paused longer than it ran (the clock started paused): `from`.
        assert_eq!(preview_time(1.0, 4.0, 12.0, 1.0, false, 2.0), (4.0, false));
        // The pause shifts where a loop wraps, too.
        let (t, _) = preview_time(13.0, 0.0, 10.0, 1.0, true, 1.0);
        assert!(close(t, 2.0), "{t}");
    }

    #[test]
    fn preview_parses_its_params() {
        let body = r#"{"script":{"story":1},"rate":2,"from":1.5,"loop":true}"#;
        let Some(StoryRequest::Preview(req)) = parse_story_action("preview", None, "", body) else { panic!("parses") };
        assert_eq!((req.rate, req.from, req.to, req.looped), (2.0, Some(1.5), None, true));
        // The default rate is real time.
        let Some(StoryRequest::Preview(req)) = parse_story_action("preview", None, "", r#"{"script":{}}"#) else { panic!("parses") };
        assert_eq!(req.rate, 1.0);
        // Refused: no script, a rate of 0, a runaway rate, a negative time.
        for bad in [r#"{}"#, r#"{"script":{},"rate":0}"#, r#"{"script":{},"rate":100}"#, r#"{"script":{},"from":-1}"#] {
            assert!(parse_story_action("preview", None, "", bad).is_none(), "{bad}");
        }
        assert_eq!(parse_story_action("preview_stop", None, "", ""), Some(StoryRequest::PreviewStop));
    }

    /// A never-painted native map (no GPU: `MapView::new` builds no
    /// device) around Marina Bay, on the Liberty style.
    fn test_map() -> MapView {
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let rt = RT.get_or_init(|| tokio::runtime::Runtime::new().expect("a test runtime"));
        let bytes = std::fs::read(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/examples/liberty-style.json")).unwrap();
        let style = gpui_maps::style_spec::parse_and_compile(&bytes).expect("liberty compiles");
        let tiles = Arc::new(gpui_maps::tile_source::http::HttpTileSource::new(
            "https://example.invalid/{z}/{x}/{y}.mvt".to_string(),
            14,
            "Test credit",
        ));
        let mut map = MapView::new(100.0, 10.0, 3.0, 800.0, 600.0, style, tiles, rt.handle().clone());
        map.set_style_document(serde_json::from_slice(&bytes).unwrap());
        map.set_globe_enabled(false);
        map
    }

    /// det.json plus a settle point at 1.2 s (0.3 s timeout).
    fn preview_script() -> Value {
        let mut script = det_script();
        script["events"].as_array_mut().unwrap().push(json!({"at": 1.2, "settle": {"timeout_s": 0.3}}));
        script
    }

    fn state_for(script: &Value, rate: f64, started: Instant) -> PreviewState {
        state_at_viewport(script, rate, started, (800.0, 600.0))
    }

    fn state_at_viewport(script: &Value, rate: f64, started: Instant, viewport: (f32, f32)) -> PreviewState {
        let raw = serde_json::to_vec(script).unwrap();
        let parsed = story::parse(&raw).expect("the script parses");
        let docs = story::scene_documents(&parsed, Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))).expect("the files load");
        let req = PreviewRequest { script: script.clone(), rate, from: None, to: None, looped: false, base_dir: None };
        let base = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."));
        let mut state = PreviewState::new(&parsed, docs, base, viewport, &story::ViewCorner::default(), &req, started).unwrap_or_else(|e| panic!("{e}"));
        state.styles.wait();
        state.live.wait();
        state
    }

    /// The start-up race (spec §13): a preview POSTed before the window
    /// settled its size is planned at the map's constructor size; once the
    /// window settles, the pose it draws must be the one a preview started
    /// after the settle draws -- the story's pose is a function of
    /// (script, t, viewport), not of when the preview began.
    #[test]
    fn a_pose_set_before_a_resize_equals_the_pose_set_after_it() {
        let started = Instant::now();
        let settled = (540.0, 960.0);
        // Before: planned at the constructor's 800x600, then the window
        // settles at 540x960 @2.
        let mut early_map = test_map();
        let mut early = state_at_viewport(&preview_script(), 2.0, started, (800.0, 600.0));
        preview_tick(&mut early_map, &mut early, at(started, 0.3));
        early_map.set_view_size(settled.0, settled.1, 2.0);
        preview_tick(&mut early_map, &mut early, at(started, 0.3));
        early.live.wait();
        let before = preview_tick(&mut early_map, &mut early, at(started, 0.3));
        // After: the window settled first.
        let mut late_map = test_map();
        late_map.set_view_size(settled.0, settled.1, 2.0);
        let mut late = state_at_viewport(&preview_script(), 2.0, started, settled);
        let after = preview_tick(&mut late_map, &mut late, at(started, 0.3));
        assert_eq!(before, after, "the same story time and scene");
        let (a, b) = (early_map.camera(), late_map.camera());
        assert_eq!((a.viewport_width_px, a.viewport_height_px), settled);
        assert!(
            (a.center_lon - b.center_lon).abs() < 1e-9
                && (a.center_lat - b.center_lat).abs() < 1e-9
                && (a.zoom - b.zoom).abs() < 1e-9
                && (a.pitch - b.pitch).abs() < 1e-6
                && (a.bearing - b.bearing).abs() < 1e-6,
            "the pose set before the resize {a:?} is the pose set after it {b:?}"
        );
    }

    fn at(started: Instant, s: f64) -> Instant {
        started + Duration::from_secs_f64(s)
    }

    /// The tick is `pose_at(t)` on the live map: the camera lands on the
    /// story's pose, and the segment and captions are the timeline's.
    #[test]
    fn a_tick_puts_the_story_pose_and_captions_on_the_map() {
        let mut map = test_map();
        let started = Instant::now();
        let mut state = state_for(&preview_script(), 2.0, started);
        // 0.3 wall seconds at rate 2: story time 0.6, inside the fly.
        let status = preview_tick(&mut map, &mut state, at(started, 0.3));
        assert!(close(status.t, 0.6), "{status:?}");
        assert!(status.playing && !status.finished && !status.waiting_on_tiles);
        assert_eq!(status.segment, Some(1), "the fly is camera[1]");
        let pose = state.path.pose_at(0.6);
        let c = map.camera();
        assert!((c.center_lon - pose.camera.center_lon).abs() < 1e-9 && (c.center_lat - pose.camera.center_lat).abs() < 1e-9);
        // At the window's offset from the capture's frame (spec §13).
        let offset = story::live_zoom_offset((c.viewport_width_px, c.viewport_height_px));
        assert!(offset < 0.0, "a window smaller than 1080 units: {offset}");
        assert!((c.zoom - (pose.camera.zoom + offset)).abs() < 1e-9 && (c.pitch - pose.camera.pitch).abs() < 1e-6);
        // The caption runs 0.2..1.8: on at 0.6, with its text and anchor.
        assert_eq!(status.captions.len(), 1);
        assert_eq!(status.captions[0].text, "Marina Bay");
        assert_eq!(status.captions[0].anchor, gpui_maps::story::timeline::Anchor::BottomLeft);
        // Before it starts there is none.
        let mut early = state_for(&preview_script(), 1.0, started);
        assert!(preview_tick(&mut map, &mut early, at(started, 0.1)).captions.is_empty());
    }

    /// Generation 0 is applied on the first tick, generation 1 (water
    /// hidden at 0.5 s) once the clock passes it; both were compiled
    /// before the clock started.
    #[test]
    fn a_tick_applies_each_style_generation_once() {
        let mut map = test_map();
        let started = Instant::now();
        let mut state = state_for(&preview_script(), 1.0, started);
        preview_tick(&mut map, &mut state, at(started, 0.1));
        assert_eq!(map.style_document(), state.timeline.style_document(0), "generation 0 on the map");
        preview_tick(&mut map, &mut state, at(started, 0.2));
        let status = preview_tick(&mut map, &mut state, at(started, 0.7));
        assert_eq!(status.style_generation, 1);
        assert_eq!(map.style_document(), state.timeline.style_document(1), "generation 1 on the map");
        let water_hidden = map.style_document().unwrap()["layers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["id"] == "water" && l["layout"]["visibility"] == "none");
        assert!(water_hidden, "the edit is in the document the map now has");
        assert!(state.style_error.is_none());
    }

    /// At a settle point the clock pauses AT it (the overshoot is paid
    /// back), waits (a never-painted map never reports its cover) until
    /// the timeout, then resumes from it.
    #[test]
    fn a_settle_point_pauses_the_clock_until_its_timeout() {
        let mut map = test_map();
        let started = Instant::now();
        let mut state = state_for(&preview_script(), 1.0, started);
        assert!(!preview_tick(&mut map, &mut state, at(started, 1.0)).waiting_on_tiles);
        // Past the settle point: held at 1.2.
        let s = preview_tick(&mut map, &mut state, at(started, 1.3));
        assert!(close(s.t, 1.2) && s.waiting_on_tiles && s.playing, "{s:?}");
        let s = preview_tick(&mut map, &mut state, at(started, 1.5));
        assert!(close(s.t, 1.2) && s.waiting_on_tiles, "still inside the 0.3 s timeout: {s:?}");
        // The pause began at 1.3; its timeout ends it at 1.6.
        let s = preview_tick(&mut map, &mut state, at(started, 1.65));
        assert!(!s.waiting_on_tiles, "{s:?}");
        // Resumed from 1.2: 1.65 wall - 0.1 overshoot - 0.35 waited = 1.2.
        assert!(close(s.t, 1.2), "{s:?}");
        let s = preview_tick(&mut map, &mut state, at(started, 1.95));
        assert!(close(s.t, 1.5) && !s.waiting_on_tiles, "{s:?}");
        // The end: held there, finished, no longer playing.
        let s = preview_tick(&mut map, &mut state, at(started, 10.0));
        assert!(close(s.t, 2.0) && s.finished && !s.playing, "{s:?}");
    }

    /// The acceptance script (a Mapbox scene) previews without a tile
    /// source: only its files are read, so no token or network is needed.
    #[test]
    fn orbit_marina_resolves_for_a_preview_without_its_tile_source() {
        let raw = std::fs::read(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/examples/stories/orbit-marina.json")).unwrap();
        let mut script: Value = serde_json::from_slice(&raw).unwrap();
        // Its style path is relative to its own folder (`../`); this inline
        // twin has no folder, so it names a file outright: the open Streets
        // look-alike, which every tree has (the Mapbox original is not in
        // the open library tree).
        script["scene"]["style"] = json!(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/examples/styles-open/open-streets-style.json"));
        let state = state_for(&script, 1.0, Instant::now());
        assert!(close(state.to_s, 12.0), "{}", state.to_s);
        assert!(state.timeline.settle_points().is_empty());
    }

    /// A loop wraps back to `from` and waits at the settle point again on
    /// the next lap.
    #[test]
    fn a_looped_preview_settles_again_on_every_lap() {
        let mut map = test_map();
        let started = Instant::now();
        let mut state = state_for(&preview_script(), 1.0, started);
        state.looped = true;
        assert!(preview_tick(&mut map, &mut state, at(started, 1.3)).waiting_on_tiles);
        assert!(!preview_tick(&mut map, &mut state, at(started, 1.65)).waiting_on_tiles);
        // 0.45 s paused: wall 2.5 is story 2.05, wrapped to 0.05.
        let s = preview_tick(&mut map, &mut state, at(started, 2.5));
        assert!(close(s.t, 0.05) && !s.finished && s.playing, "{s:?}");
        let s = preview_tick(&mut map, &mut state, at(started, 3.8));
        assert!(close(s.t, 1.2) && s.waiting_on_tiles, "the second lap settles too: {s:?}");
    }

    /// Ticks `map` through the shell's hook until its styles are compiled
    /// and the clock runs (bounded: 10 s).
    fn until_ready(map: &mut MapView, id: EntityId) {
        for _ in 0..200 {
            let _ = preview_frame(map, id, Instant::now());
            if lock_preview().active.as_ref().and_then(|p| p.status.as_ref()).is_some_and(|s| !s.preparing) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the preview's styles did not compile within 10 s");
    }

    fn preview_request(script: Value) -> PreviewRequest {
        PreviewRequest { script, rate: 1.0, from: None, to: None, looped: false, base_dir: None }
    }

    /// Task 20: the preview draws what the render draws -- a panel in the
    /// map's own story frame, and an overlay layer pushed as a source of
    /// its own whose `draw` trims it per frame -- and takes both away on
    /// stop. `not_applied` no longer lists overlays or panels.
    #[test]
    fn a_preview_draws_the_story_frame_and_drives_the_overlay_layers() {
        let _slot = slot();
        let mut app = gpui::TestAppContext::single();
        let entity = app.update(|cx| gpui::AppContext::new(cx, |_| test_map()));
        let id = entity.entity_id();
        let mut script = det_script();
        script["scene"]["overlays"] = json!([{
            "id": "routes", "line_metrics": true,
            "data": {"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
                "geometry": {"type": "LineString", "coordinates": [[103.80, 1.28], [103.86, 1.29], [103.90, 1.30]]}}]},
            "layers": [{"id": "route-line", "type": "line", "paint": {"line-color": "#e33", "line-width": 3}}]
        }]);
        let events = script["events"].as_array_mut().unwrap();
        events.push(json!({"at": 0.0, "until": 2.0, "panel": {"placement": "side", "headline": "Marina Bay", "body": "A live panel."}}));
        events.push(json!({"at": 0.2, "until": 1.6, "overlay": {"layer": "route-line", "effect": "draw", "easing": "linear"}}));
        set_dispatch_map(Some(&entity));
        let body = json!({"script": script, "rate": 1.0}).to_string();
        let request = parse_story_action("preview", None, "", &body).expect("parses");
        let reply = entity.update(&mut app, |map, _| answer(map, request));
        assert_eq!(reply["ok"], true, "{reply}");
        assert!(reply.get("layer_errors").is_none(), "{reply}");
        let gaps: Vec<String> = reply["not_applied"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert!(!gaps.iter().any(|g| g.starts_with("scene.overlays")), "overlays with layers are applied now: {gaps:?}");
        let name = story::overlay_layers::live_source_name("route-line");
        entity.update(&mut app, |map, _| {
            until_ready(map, id);
            let _ = preview_frame(map, id, Instant::now() + Duration::from_millis(900));
            let frame = map.story_frame().expect("the map holds the story frame");
            assert!(
                frame.items.iter().any(|i| matches!(i, story::layout::LayoutItem::Panel { headline: Some(h), .. } if h == "Marina Bay")),
                "the panel is in the map's frame"
            );
            let fx = map.source_line_fx(&name);
            let trim = fx.trim.expect("the draw trims the live source");
            assert!(trim[1] > 0.0 && trim[1] < 1.0, "part-way through its draw: {fx:?}");
        });
        let stopped = entity.update(&mut app, |map, _| answer(map, StoryRequest::PreviewStop));
        assert_eq!(stopped["restored"], true);
        entity.read_with(&app, |map, _| {
            assert!(map.story_frame().is_none());
            assert_eq!(map.source_line_fx(&name), Default::default(), "the layer source is gone");
        });
        set_dispatch_map(None);
    }

    /// Through the actions, as the agent pump calls them (the map named by
    /// its entity id): `preview` answers at once and `/map/state`'s
    /// `story` appears; the hook drives only the map it started on; `stop`
    /// restores the camera, the globe and the style document, and `story`
    /// is null again.
    #[test]
    fn preview_and_stop_restore_the_map() {
        let _slot = slot();
        let mut app = gpui::TestAppContext::single();
        let entity = app.update(|cx| gpui::AppContext::new(cx, |_| test_map()));
        let id = entity.entity_id();
        let other_id = EntityId::from(u64::MAX >> 1);
        let mut other = test_map();
        let (before, before_doc, before_look) =
            entity.read_with(&app, |m, _| (*m.camera(), m.style_document().cloned(), m.extrusion_overrides()));
        set_dispatch_map(Some(&entity));
        assert_eq!(preview_state_json(), Value::Null);
        let body = json!({"script": preview_script(), "rate": 1.0}).to_string();
        let request = parse_story_action("preview", None, "", &body).expect("parses");
        let started = Instant::now();
        let reply = entity.update(&mut app, |map, _| answer(map, request));
        assert!(started.elapsed() < Duration::from_secs(2), "preview answered in {:?}", started.elapsed());
        assert_eq!(reply["ok"], true, "{reply}");
        assert!(close(reply["duration"].as_f64().unwrap(), 2.0), "{reply}");
        assert_eq!(reply["style_generations"], 2, "{reply}");
        assert!(reply["not_applied"].as_array().is_some_and(|a| !a.is_empty()), "F11: the gaps are said: {reply}");
        assert!(preview_state_json()["not_applied"].is_array());
        // F11: the render's building look (det.json names none: enhanced).
        let (look, occlusion) = story::script::BuildingsLook::Enhanced.capture();
        entity.read_with(&app, |m, _| {
            assert_eq!((m.extrusion_overrides().look, m.extrusion_overrides().label_occlusion), (look, occlusion));
        });
        assert_eq!(preview_state_json()["playing"], true);
        assert_eq!(preview_state_json_for(other_id), Value::Null, "another map has no preview");
        // The shell's hook: another map is left alone; this one moves.
        assert!(preview_frame(&mut other, other_id, Instant::now()).is_none());
        entity.update(&mut app, |map, _| {
            until_ready(map, id);
            let frame = preview_frame(map, id, Instant::now() + Duration::from_millis(600)).expect("this map is driven");
            assert!(frame.keep_ticking);
            // Task 20: the map draws the caption itself, in its story
            // frame, where the render places it; the shell draws none.
            assert!(frame.captions.is_empty(), "no gpui caption layer over the map");
            let story_frame = map.story_frame().expect("the map holds the story frame");
            let captions = story_frame.items.iter().filter(|i| matches!(i, story::layout::LayoutItem::Caption { .. })).count();
            assert_eq!(captions, 1, "the caption is on at ~0.6 s");
            assert_ne!(map.camera().center_lon, before.center_lon, "the camera moved");
        });
        let state = preview_state_json();
        assert!(state["t"].as_f64().unwrap() > 0.5 && state["preparing"] == false, "{state}");
        assert_eq!(state["segment"], 1, "{state}");
        // Stop on the other map: nothing to stop there.
        assert_eq!(preview_stop(&mut other, other_id)["stopped"], false);
        let stopped = entity.update(&mut app, |map, _| answer(map, StoryRequest::PreviewStop));
        assert_eq!(stopped, json!({"stopped": true, "restored": true}));
        entity.read_with(&app, |map, _| assert!(map.story_frame().is_none(), "the story frame goes with the preview"));
        entity.update(&mut app, |map, _| {
            let c = map.camera();
            assert!((c.center_lon - before.center_lon).abs() < 1e-9 && (c.zoom - before.zoom).abs() < 1e-9, "the camera is back");
            assert!(!map.globe_enabled());
            assert_eq!(map.style_document().cloned(), before_doc, "the style document is back");
            assert_eq!(map.extrusion_overrides(), before_look, "the building look is back");
            assert!(preview_frame(map, id, Instant::now()).is_none(), "the hook drives nothing once stopped");
        });
        assert_eq!(preview_state_json(), Value::Null);
        set_dispatch_map(None);
        assert!(answer(&mut other, StoryRequest::PreviewStop)["error"].is_string(), "no identity outside the pump");
    }

    /// Review F2: a preview started on map B while one runs on map A
    /// leaves A owed its scene, and A's next frame restores it.
    #[test]
    fn a_preview_on_another_map_restores_the_first_on_its_next_frame() {
        let _slot = slot();
        let (a_id, b_id) = (EntityId::from(1u64 << 40), EntityId::from((1u64 << 40) + 1));
        let mut a = test_map();
        let mut b = test_map();
        let before = *a.camera();
        assert_eq!(preview(&mut a, a_id, None, &preview_request(preview_script()))["ok"], true);
        until_ready(&mut a, a_id);
        let _ = preview_frame(&mut a, a_id, Instant::now() + Duration::from_millis(600));
        assert_ne!(a.camera().center_lon, before.center_lon, "A moved");
        assert_eq!(preview(&mut b, b_id, None, &preview_request(preview_script()))["ok"], true);
        // A's next frame: restored, and the hook drives it no more.
        assert!(preview_frame(&mut a, a_id, Instant::now()).is_none());
        assert!((a.camera().center_lon - before.center_lon).abs() < 1e-9, "A is back");
        assert!(lock_preview().orphans.is_empty());
        assert_eq!(preview_stop(&mut b, b_id)["stopped"], true);
    }

    /// Review F2: the gallery switching away from (or closing) a panel
    /// mid-preview stops the preview and restores that map; the pump's
    /// sweep does it, with the map reached through its entity.
    #[test]
    fn switching_panels_mid_preview_stops_and_restores() {
        let _slot = slot();
        let mut app = gpui::TestAppContext::single();
        let a = app.update(|cx| gpui::AppContext::new(cx, |_| test_map()));
        let b = app.update(|cx| gpui::AppContext::new(cx, |_| test_map()));
        let before = a.read_with(&app, |m, _| *m.camera());
        set_dispatch_map(Some(&a));
        let body = json!({"script": preview_script()}).to_string();
        let started = a.update(&mut app, |map, _| answer(map, parse_story_action("preview", None, "", &body).unwrap()));
        assert_eq!(started["ok"], true, "{started}");
        let a_id = a.entity_id();
        a.update(&mut app, |map, _| {
            until_ready(map, a_id);
            let _ = preview_frame(map, a_id, Instant::now() + Duration::from_millis(600));
        });
        set_dispatch_map(None);
        // The live map is still A: the sweep leaves the preview alone.
        app.update(|cx| release_previews_except(Some(a_id), cx));
        assert!(lock_preview().active.is_some());
        // The gallery switched to B.
        app.update(|cx| release_previews_except(Some(b.entity_id()), cx));
        assert!(lock_preview().active.is_none() && lock_preview().orphans.is_empty(), "stopped");
        let c = a.read_with(&app, |m, _| *m.camera());
        assert!((c.center_lon - before.center_lon).abs() < 1e-9 && (c.zoom - before.zoom).abs() < 1e-9, "A is restored");
        // Closing the panel mid-preview (the slot empties) stops it too.
        set_dispatch_map(Some(&b));
        b.update(&mut app, |map, _| answer(map, parse_story_action("preview", None, "", &body).unwrap()));
        set_dispatch_map(None);
        drop(b);
        app.update(|cx| release_previews_except(None, cx));
        assert!(lock_preview().active.is_none() && lock_preview().orphans.is_empty());
    }

    #[test]
    fn a_preview_of_a_script_that_does_not_parse_is_answered_with_its_diagnostics() {
        let mut map = test_map();
        let id = EntityId::from(7u64 << 40);
        let v = preview(&mut map, id, None, &preview_request(json!({"story": 2, "camera": []})));
        assert_eq!(v["ok"], false, "{v}");
        assert!(v["diagnostics"].as_array().is_some_and(|d| !d.is_empty()), "{v}");
        let mut missing = det_script();
        missing["scene"]["style"] = json!("/nonexistent/style.json");
        let v = preview(&mut map, id, None, &preview_request(missing));
        assert_eq!(v["ok"], false, "{v}");
        assert!(v["error"].as_str().unwrap().contains("does not load"), "{v}");
    }

    #[test]
    fn a_snapshot_without_a_style_document_is_answered_not_rendered() {
        let snapshot = story::SceneSnapshot {
            camera: story::CameraSnapshot { lon: 0.0, lat: 0.0, zoom: 1.0, pitch_deg: 0.0, bearing_deg: 0.0 },
            globe: true,
            style: None,
            basemap: Some("openfreemap".into()),
            raster: None,
            overlays: Vec::new(),
            credit: None,
            left_out: Vec::new(),
            buildings: story::script::BuildingsLook::Maplibre,
        };
        let e = snapshot_script(&snapshot, 100, 100).unwrap_err();
        assert!(format!("{e}").contains("set_style_json"), "{e}");
    }

    /// F3: agent outputs stay inside the output root. Outside it, a `..`,
    /// a relative path, a symlinked folder that leads out and a symlink at
    /// the output itself are all refused; an existing file is refused
    /// until `overwrite` says otherwise.
    #[test]
    fn agent_outputs_are_confined_to_the_root_and_never_overwrite_by_default() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().join("gpui-map");
        let elsewhere = tempfile::tempdir().unwrap();
        let ok = |out: &Path, overwrite: bool| confine_out(out, &root, overwrite, &[manifest_path(out), extras_dir(out)]);

        ok(&root.join("a.png"), false).expect("a new file inside the root");
        ok(&root.join("sub/dir/a.mp4"), false).expect("a new subfolder inside the root");
        let e = ok(&elsewhere.path().join("a.png"), false).unwrap_err();
        assert!(format!("{e}").contains("confined"), "{e}");
        assert!(ok(Path::new("relative.png"), false).is_err());
        let e = ok(&root.join("../escape.png"), false).unwrap_err();
        assert!(format!("{e}").contains(".."), "{e}");

        // A folder inside the root that is a symlink out of it.
        std::os::unix::fs::symlink(elsewhere.path(), root.join("link")).unwrap();
        let e = ok(&root.join("link/a.png"), false).unwrap_err();
        assert!(format!("{e}").contains("confined"), "{e}");
        // A symlink AT the output (dangling, too) is never written through.
        std::os::unix::fs::symlink(elsewhere.path().join("target.png"), root.join("dangling.png")).unwrap();
        let e = ok(&root.join("dangling.png"), true).unwrap_err();
        assert!(format!("{e}").contains("symlink"), "{e}");
        assert!(!elsewhere.path().join("target.png").exists());

        // An existing output, or its manifest, needs `overwrite`.
        std::fs::write(root.join("old.png"), b"x").unwrap();
        let e = ok(&root.join("old.png"), false).unwrap_err();
        assert!(format!("{e}").contains("overwrite"), "{e}");
        ok(&root.join("old.png"), true).expect("overwrite: true replaces it");
        std::fs::write(root.join("m.story.json"), b"{}").unwrap();
        assert!(ok(&root.join("m.mp4"), false).is_err(), "the manifest beside it would be replaced");
    }

    /// F3 through the action: a still outside the root is refused before
    /// a job starts, and `overwrite` parses from the body.
    #[test]
    fn an_image_outside_the_root_is_refused_and_overwrite_parses() {
        let _slot = slot();
        let outside = PathBuf::from("/Users/Shared/gpui-map-agent-escape.png");
        let answer = image(outside.clone(), Ok((det_script(), Vec::new())), None, false);
        assert!(answer["error"].as_str().unwrap_or("").contains("confined"), "{answer}");
        assert!(answer.get("job").is_none() && !outside.exists());
        let parsed = parse_story_action("image", None, "", r#"{"out":"/tmp/x.png","overwrite":true}"#).unwrap();
        assert!(matches!(parsed, StoryRequest::Image { overwrite: true, .. }), "{parsed:?}");
        let parsed = parse_story_action("render", None, "", r#"{"script":{"story":1},"out":"/tmp/x.mp4"}"#).unwrap();
        assert!(matches!(parsed, StoryRequest::Render(RenderRequest { overwrite: false, .. })), "{parsed:?}");
    }

    /// F1 on the agent path: a snapshot of a templated Mapbox basemap
    /// keeps the `${MAPBOX_ACCESS_TOKEN}` template in the script (the
    /// loader fills it), and F13: the scratch style file is recognised
    /// for removal.
    #[test]
    fn a_snapshot_script_never_holds_the_live_token() {
        let style: Value = serde_json::from_slice(&std::fs::read(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/examples/liberty-style.json")).unwrap()).unwrap();
        let snapshot = story::SceneSnapshot {
            camera: story::CameraSnapshot { lon: 0.0, lat: 0.0, zoom: 1.0, pitch_deg: 0.0, bearing_deg: 0.0 },
            globe: false,
            style: Some(style),
            basemap: Some("https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token=${MAPBOX_ACCESS_TOKEN}".into()),
            raster: None,
            overlays: Vec::new(),
            credit: Some("© Mapbox © OpenStreetMap".into()),
            left_out: Vec::new(),
            buildings: story::script::BuildingsLook::Maplibre,
        };
        let script = snapshot_script(&snapshot, 100, 100).unwrap();
        let tiles = script.pointer("/scene/basemap/tiles/0").and_then(Value::as_str).unwrap();
        assert!(tiles.ends_with("access_token=${MAPBOX_ACCESS_TOKEN}"), "{tiles}");
        let style_file = snapshot_style_file(&script).expect("the scratch style is named for removal");
        assert!(style_file.exists());
        std::fs::remove_file(&style_file).unwrap();
        // A script's own style is never taken for scratch.
        assert!(snapshot_style_file(&det_script()).is_none());
    }

    /// `capture.image` parses `layers` (default true), `scale` and
    /// `timeout`, and refuses a malformed one.
    #[test]
    fn capture_image_parses_layers_scale_and_timeout() {
        let parse = |body: Value| parse_story_action("image", None, "", &body.to_string());
        let Some(StoryRequest::Image { layers, scale, timeout, .. }) = parse(json!({"out": "/tmp/x.png"})) else { panic!("parses") };
        assert!(layers, "the map's own frame is the default");
        assert_eq!((scale, timeout), (None, None));
        let Some(StoryRequest::Image { layers, scale, timeout, .. }) =
            parse(json!({"out": "/tmp/x.png", "layers": false, "scale": 1.5, "timeout": 4}))
        else {
            panic!("parses")
        };
        assert!(!layers);
        assert_eq!(scale, Some(1.5));
        assert_eq!(timeout, Some(Duration::from_secs(4)));
        assert!(parse(json!({"out": "/tmp/x.png", "layers": "yes"})).is_none());
        assert!(parse(json!({"out": "/tmp/x.png", "scale": 0})).is_none());
        assert!(parse(json!({"out": "/tmp/x.png", "scale": 99})).is_none());
        assert!(parse(json!({"out": "/tmp/x.png", "timeout": -1})).is_none());
    }

    /// The page's own layers are in the still: a red GeoJSON square pushed
    /// onto the map (never in its style, so the story renderer's
    /// style-and-basemap still could not draw it) fills the middle of the
    /// PNG `capture.image` writes, rendered offscreen by the map's own
    /// renderer with no window at all -- and the edges, outside the
    /// square, are not red.
    #[test]
    fn an_offscreen_still_draws_the_maps_own_overlay() {
        let _slot = slot();
        let mut app = gpui::TestAppContext::single();
        let entity = app.update(|cx| gpui::AppContext::new(cx, |_| test_map()));
        let id = entity.entity_id();
        // The test map looks at (100, 10) at z3: a square of +-10 degrees
        // round it is 114 to 228 px a side (256 or 512 px tiles), well
        // inside a 400 x 300 frame and well clear of its corners.
        let square = json!({"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
            "geometry": {"type": "Polygon", "coordinates": [[[90.0, 0.0], [110.0, 0.0], [110.0, 20.0], [90.0, 20.0], [90.0, 0.0]]]}}]});
        entity.update(&mut app, |map, cx| {
            map.set_source_deferred("red-square", &square.to_string()).expect("the square parses");
            let red: serde_json::Map<String, Value> =
                serde_json::from_value(json!({"fill-color": "#ff0000", "fill-opacity": 1.0})).unwrap();
            map.update_source_paint("red-square", &red, cx).expect("the paint compiles");
        });
        let out = std::env::temp_dir().join(format!("offscreen-still-{}.png", std::process::id()));
        let _ = std::fs::remove_file(&out);
        set_dispatch_map(Some(&entity));
        let body = json!({"out": out, "overwrite": true, "width": 400, "height": 300, "scale": 1, "timeout": 4}).to_string();
        let request = parse_story_action("image", None, "", &body).expect("parses");
        let reply = entity.update(&mut app, |map, _| answer(map, request));
        set_dispatch_map(None);
        let job = reply["job"].as_str().unwrap_or_else(|| panic!("a job: {reply}")).to_string();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            entity.update(&mut app, |map, cx| offscreen_tick(id, map, cx));
            app.run_until_parked();
            let status = lock_jobs().status(&job);
            if status["state"] != "running" || Instant::now() > deadline {
                break status;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status["state"], "done", "{status}");
        let result = &status["result"];
        assert_eq!(result["layers"], true, "{result}");
        assert_eq!((result["width"].as_u64(), result["height"].as_u64()), (Some(400), Some(300)), "{result}");
        assert!(result["frames"].as_u64().unwrap_or(0) >= 1, "{result}");
        let png = image::open(&out).expect("the PNG").to_rgba8();
        assert_eq!(png.dimensions(), (400, 300));
        let is_red = |x: u32, y: u32| {
            let [r, g, b, _] = png.get_pixel(x, y).0;
            r > 200 && g < 60 && b < 60
        };
        assert!(is_red(200, 150), "the square fills the middle: {:?}", png.get_pixel(200, 150));
        assert!(is_red(160, 120) && is_red(240, 180), "and round it");
        for (x, y) in [(4, 4), (395, 4), (4, 295), (395, 295)] {
            assert!(!is_red(x, y), "the corner ({x}, {y}) is outside the square: {:?}", png.get_pixel(x, y));
        }
        // The map is back at its own size: the still did not resize it.
        entity.read_with(&app, |map, _| {
            assert_eq!((map.camera().viewport_width_px, map.camera().viewport_height_px), (800.0, 600.0));
        });
        let _ = std::fs::remove_file(&out);
    }

    /// Imagery, served from memory: every tile a solid green PNG, after a
    /// short delay, so the first frames find the cover still in flight.
    struct GreenTiles;

    impl gpui_maps::tile_source::TileSource for GreenTiles {
        // `TileSource` is an `async_trait`, which this crate does not
        // depend on: its expansion, written out.
        fn fetch<'life0, 'async_trait>(
            &'life0 self,
            _tile: gpui_maps::geo::TileId,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let mut png = Vec::new();
                image::RgbaImage::from_pixel(256, 256, image::Rgba([0, 200, 0, 255]))
                    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
                Ok(Some(png))
            })
        }
        fn max_zoom(&self) -> u8 {
            14
        }
        fn attribution(&self) -> Option<&str> {
            None
        }
    }

    /// A raster basemap is drawn and waited for: under the satellite style
    /// (a raster layer over a background) with imagery that lands only
    /// after a delay, the still is green -- the imagery -- and not the
    /// background it covers, because the capture waits for the raster
    /// fetches as well as the vector cover before it reads back.
    #[test]
    fn an_offscreen_still_waits_for_and_draws_a_raster_basemap() {
        let _slot = slot();
        let mut app = gpui::TestAppContext::single();
        let entity = app.update(|cx| {
            gpui::AppContext::new(cx, |_| {
                let mut map = test_map();
                let bytes = std::fs::read(concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/examples/satellite-style.json")).unwrap();
                map.set_style(gpui_maps::style_spec::parse_and_compile(&bytes).expect("the satellite style compiles"));
                map.set_raster_source(Arc::new(GreenTiles), 256);
                map
            })
        });
        let id = entity.entity_id();
        let out = std::env::temp_dir().join(format!("offscreen-raster-{}.png", std::process::id()));
        let _ = std::fs::remove_file(&out);
        set_dispatch_map(Some(&entity));
        let body = json!({"out": out, "overwrite": true, "width": 320, "height": 240, "scale": 1, "timeout": 10}).to_string();
        let reply = entity.update(&mut app, |map, _| answer(map, parse_story_action("image", None, "", &body).expect("parses")));
        set_dispatch_map(None);
        let job = reply["job"].as_str().unwrap_or_else(|| panic!("a job: {reply}")).to_string();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            entity.update(&mut app, |map, cx| offscreen_tick(id, map, cx));
            app.run_until_parked();
            let status = lock_jobs().status(&job);
            if status["state"] != "running" || Instant::now() > deadline {
                break status;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["result"]["settled"], true, "taken once the imagery had landed, not at the timeout: {status}");
        let png = image::open(&out).expect("the PNG").to_rgba8();
        // Clear of the bottom corners, where the map draws its own mark and
        // the credit's plate.
        for (x, y) in [(160, 120), (20, 20), (300, 20), (40, 150), (280, 150)] {
            let [r, g, b, _] = png.get_pixel(x, y).0;
            assert!(g > 150 && r < 60 && b < 60, "the imagery at ({x}, {y}), not the background: {:?}", (r, g, b));
        }
        let _ = std::fs::remove_file(&out);
    }
}
