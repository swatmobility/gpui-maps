//! A second vector source over the basemap, on the phone: an MVT or
//! vector PMTiles source drawn as an OVERLAY with its own style layers --
//! the desktop's `MapView::add_vector_overlay` (`src/map_view/
//! vector_overlay.rs`), on this shell's threads instead of tokio.
//!
//! The style layers are compiled by the engine's own compiler
//! (`overlay::vector_tiles::compile_layers`: `fill`, `line` and `circle`,
//! with their `filter`, `minzoom`/`maxzoom` and zoom or data expressions).
//! Each frame ([`VectorOverlays::frame`]) works out the tiles the camera
//! wants at the source's zoom (the integer bucket, overzoomed past the
//! source's `max_zoom`), queues the missing ones on [`WORKERS`] threads
//! that FETCH AND DECODE them (a `pmtiles+<archive>` range read through
//! `pmtiles_http`, a local archive by seek and read, or a plain XYZ GET;
//! then `vector_tiles::tile_features`), and once the wanted tiles have
//! settled pushes each style layer as one engine overlay source
//! (`ShellOverlays::set_source_data`), named `"{overlay}/{layer id}"`, its
//! features' paints resolved at the camera's zoom bucket. Nothing heavier
//! than the push runs on the main thread, and the push waits
//! [`PUSH_EVERY`] while tiles are still landing.
//!
//! A tap ([`VectorOverlays::pick`]) answers the overlay's name and
//! `"{source}#{index}"`; [`VectorOverlays::properties`] turns that back
//! into the feature's own MVT properties, for the app's popup.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::geo::TileId;
use crate::overlay::host::vector_tiles::{self, TileFeatures, VectorGeometry, VectorLayer, VectorLayerKind};
use crate::overlay::host::{DataFeatureInput, FeatureInput, FeaturePaint, FeatureProperties, GeometryInput};
use crate::style::ZoomValue;

/// Fetch-and-decode threads, shared by every overlay.
const WORKERS: usize = 4;
/// A failed tile is asked again after this long.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// While tiles are still landing, a push waits this long after the last.
const PUSH_EVERY: Duration = Duration::from_millis(300);
/// The cover is grown by this fraction of the viewport on every side.
const VIEW_GROW: f64 = 0.1;
/// Tiles one overlay fetches at once.
const MAX_IN_FLIGHT: usize = 6;
/// A finger's slop round a circle, in points.
const TAP_SLOP_PT: f64 = 10.0;

/// The project's User-Agent on every request (the repo URL, never an
/// address or a token): Overture's CDN answers 403 to a request without one.
pub const USER_AGENT: &str = "gpui-maps (+https://github.com/swatmobility/gpui-maps)";

/// What an overlay is added with.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorOverlayOptions {
    /// Below this camera zoom nothing is fetched or drawn.
    pub min_zoom: u8,
    /// The deepest tiles the source has; `None`: a PMTiles archive's
    /// header says (an XYZ template then defaults to 14).
    pub max_zoom: Option<u8>,
    /// The most tiles one view fetches (nearest the centre first).
    pub max_tiles: usize,
    /// The overlay's credit line, on the map while it is added.
    pub credit: Option<String>,
}

impl Default for VectorOverlayOptions {
    fn default() -> Self {
        Self { min_zoom: 0, max_zoom: None, max_tiles: 24, credit: None }
    }
}

/// What an overlay holds and has done.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VectorOverlayStats {
    pub tiles_wanted: usize,
    pub tiles_held: usize,
    pub tiles_in_flight: usize,
    pub tiles_absent: usize,
    pub tiles_failed: usize,
    /// Features in the last push, over every layer.
    pub features_pushed: usize,
    pub pushes: u64,
    /// The source zoom of the last push.
    pub zoom: Option<u8>,
    /// The last fetch or decode error, if any.
    pub last_error: Option<String>,
}

/// The engine source a layer draws into.
pub fn layer_source_name(overlay: &str, layer: &str) -> String {
    format!("{overlay}/{layer}")
}

/// The source a circle layer's stroke draws into (under the fill).
pub fn stroke_source_name(overlay: &str, layer: &str) -> String {
    format!("{overlay}/{layer}/stroke")
}

/// One pushed source's features, for taps and popups: positions (circle
/// sources, picked here) and every feature's properties, by push index.
struct Pushed {
    points: Vec<([f64; 2], f32)>,
    properties: Vec<Arc<FeatureProperties>>,
}

struct Overlay {
    name: String,
    /// `pmtiles+<archive>` or an XYZ template.
    url: String,
    layers: Arc<Vec<VectorLayer>>,
    options: VectorOverlayOptions,
    /// Known once the archive's header is read (`None` while opening).
    max_zoom: Option<u8>,
    opening: bool,
    /// Bumped on every add: a landing of an older add is dropped.
    generation: u64,
    tiles: HashMap<TileId, Arc<TileFeatures>>,
    in_flight: HashSet<TileId>,
    absent: HashSet<TileId>,
    failed: HashMap<TileId, Instant>,
    wanted: Vec<TileId>,
    /// `(tiles, camera zoom bucket)` of the last push.
    pushed: Option<(Vec<TileId>, u8)>,
    last_push: Option<Instant>,
    /// Per engine source: what it holds.
    sources: HashMap<String, Pushed>,
    stats: VectorOverlayStats,
}

impl Overlay {
    /// The archive behind a `pmtiles+` url.
    fn archive(&self) -> Option<&str> {
        self.url.strip_prefix(crate::pmtiles_http::PREFIX)
    }

    /// Every engine source the overlay draws into, bottom first.
    fn source_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for layer in self.layers.iter() {
            if layer.kind == VectorLayerKind::Circle && layer.may_stroke() {
                out.push(stroke_source_name(&self.name, &layer.id));
            }
            out.push(layer_source_name(&self.name, &layer.id));
        }
        out
    }
}

enum Job {
    Open { name: String, generation: u64, archive: String },
    Tile { name: String, generation: u64, tile: TileId, url: String, layers: Arc<Vec<VectorLayer>>, zoom: f64 },
}

impl Job {
    fn key(&self) -> (&str, u64, Option<TileId>) {
        match self {
            Job::Open { name, generation, .. } => (name, *generation, None),
            Job::Tile { name, generation, tile, .. } => (name, *generation, Some(*tile)),
        }
    }
}

enum Landed {
    Opened { name: String, generation: u64, zooms: Result<(u8, u8), String> },
    Tile { name: String, generation: u64, tile: TileId, result: Result<Option<TileFeatures>, String> },
}

type Wake = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

/// The fetch-and-decode threads.
struct Workers {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    landed: mpsc::Receiver<Landed>,
}

impl Workers {
    fn new(wake: Wake) -> Self {
        let queue = Arc::new((Mutex::new(Queue { jobs: VecDeque::new(), closed: false }), Condvar::new()));
        let (tx, landed) = mpsc::channel();
        for i in 0..WORKERS {
            let (queue, tx, wake) = (Arc::clone(&queue), tx.clone(), Arc::clone(&wake));
            let agent = agent();
            let spawned = std::thread::Builder::new().name(format!("vector-overlay-{i}")).stack_size(1 << 20).spawn(move || loop {
                let job = {
                    let (lock, cvar) = &*queue;
                    let Ok(mut q) = lock.lock() else { return };
                    loop {
                        if q.closed {
                            return;
                        }
                        if let Some(job) = q.jobs.pop_front() {
                            break job;
                        }
                        match cvar.wait(q) {
                            Ok(g) => q = g,
                            Err(_) => return,
                        }
                    }
                };
                let landed = run(&agent, job);
                if tx.send(landed).is_err() {
                    return;
                }
                if let Some(w) = wake.lock().ok().and_then(|w| w.clone()) {
                    w();
                }
            });
            if spawned.is_err() {
                crate::mlog!("[vector-overlay] a worker thread did not start");
            }
        }
        Self { queue, landed }
    }

    fn submit(&self, job: Job) {
        let (lock, cvar) = &*self.queue;
        if let Ok(mut q) = lock.lock() {
            q.jobs.push_back(job);
            cvar.notify_one();
        }
    }

    /// Drops every queued job `keep` refuses; returns what was dropped.
    fn retain(&self, mut keep: impl FnMut(&Job) -> bool) -> Vec<(String, u64, TileId)> {
        let mut dropped = Vec::new();
        if let Ok(mut q) = self.queue.0.lock() {
            q.jobs.retain(|j| {
                let k = keep(j);
                if !k {
                    if let (name, generation, Some(tile)) = j.key() {
                        dropped.push((name.to_string(), generation, tile));
                    }
                }
                k
            });
        }
        dropped
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        if let Ok(mut q) = self.queue.0.lock() {
            q.closed = true;
            q.jobs.clear();
        }
        self.queue.1.notify_all();
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(45)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// One job, on a worker.
fn run(agent: &ureq::Agent, job: Job) -> Landed {
    match job {
        Job::Open { name, generation, archive } => Landed::Opened { name, generation, zooms: crate::pmtiles_http::open_zooms(agent, &archive) },
        Job::Tile { name, generation, tile, url, layers, zoom } => {
            let result = fetch(agent, &url, tile).and_then(|bytes| match bytes {
                None => Ok(None),
                Some(bytes) => vector_tiles::tile_features(&layers, tile, bytes, zoom).map(Some).map_err(|e| format!("decode: {e:#}")),
            });
            Landed::Tile { name, generation, tile, result }
        }
    }
}

/// A tile's bytes (`None`: the source has none there).
fn fetch(agent: &ureq::Agent, url: &str, tile: TileId) -> Result<Option<Vec<u8>>, String> {
    if url.starts_with(crate::pmtiles_http::PREFIX) {
        return crate::pmtiles_http::fetch_tile(agent, &format!("{url}/{}/{}/{}", tile.z, tile.x, tile.y));
    }
    let filled = url.replace("{z}", &tile.z.to_string()).replace("{x}", &tile.x.to_string()).replace("{y}", &tile.y.to_string());
    let mut response = agent.get(&filled).call().map_err(|e| crate::sprite::redact_access_token(&e.to_string()))?;
    match response.status().as_u16() {
        204 | 404 | 410 => Ok(None),
        s if (200..300).contains(&s) => response.body_mut().with_config().limit(32 << 20).read_to_vec().map(Some).map_err(|e| e.to_string()),
        s => Err(format!("HTTP {s}")),
    }
}

/// Every vector overlay of one map.
#[derive(Default)]
pub struct VectorOverlays {
    list: Vec<Overlay>,
    workers: Option<Workers>,
    wake: Wake,
    next_generation: u64,
}

impl VectorOverlays {
    /// The callback a landing calls (any thread): the host's needs-frame.
    pub fn set_wake(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut w) = self.wake.lock() {
            *w = Some(wake);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.list.iter().map(|o| o.name.clone()).collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.list.iter().any(|o| o.name == name)
    }

    /// The credits of every overlay, in add order.
    pub fn credits(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in self.list.iter().filter_map(|o| o.options.credit.clone()) {
            if !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    /// Adds (or replaces) overlay `name`: `url` is `pmtiles+<archive URL
    /// or file>` or an XYZ `{z}/{x}/{y}` template of MVT tiles, and
    /// `style_layers` a JSON array of `fill`, `line` and `circle` layers.
    /// Errors name the layer that could not be compiled.
    pub fn add(&mut self, overlays: &mut crate::overlay::host::ShellOverlays, name: &str, url: &str, style_layers: &serde_json::Value, options: VectorOverlayOptions) -> Result<(), String> {
        let layers = vector_tiles::compile_layers(style_layers)?;
        if layers.is_empty() {
            return Err("no drawable layer (fill, line or circle) in the style layers".into());
        }
        if !url.starts_with(crate::pmtiles_http::PREFIX) && !(url.contains("{z}") && url.contains("{x}") && url.contains("{y}")) {
            return Err(format!("{name}: the url is neither pmtiles+<archive> nor a {{z}}/{{x}}/{{y}} template"));
        }
        self.remove(overlays, name);
        for layer in &layers {
            for note in &layer.notes {
                crate::mlog!("[vector-overlay] {name}: {note}");
            }
        }
        self.next_generation += 1;
        let generation = self.next_generation;
        let mut overlay = Overlay {
            name: name.to_string(),
            url: url.to_string(),
            layers: Arc::new(layers),
            max_zoom: options.max_zoom.or(if url.starts_with(crate::pmtiles_http::PREFIX) { None } else { Some(14) }),
            options,
            opening: false,
            generation,
            tiles: HashMap::new(),
            in_flight: HashSet::new(),
            absent: HashSet::new(),
            failed: HashMap::new(),
            wanted: Vec::new(),
            pushed: None,
            last_push: None,
            sources: HashMap::new(),
            stats: VectorOverlayStats::default(),
        };
        if overlay.max_zoom.is_none() {
            let archive = overlay.archive().unwrap_or_default().to_string();
            overlay.opening = true;
            self.workers().submit(Job::Open { name: name.to_string(), generation, archive });
        }
        self.list.push(overlay);
        Ok(())
    }

    /// Removes overlay `name` and every source it drew into.
    pub fn remove(&mut self, overlays: &mut crate::overlay::host::ShellOverlays, name: &str) -> bool {
        let Some(at) = self.list.iter().position(|o| o.name == name) else { return false };
        let overlay = self.list.remove(at);
        for source in overlay.source_names() {
            overlays.remove_source(&source);
        }
        if let Some(w) = &self.workers {
            w.retain(|j| j.key().0 != name);
        }
        true
    }

    fn workers(&mut self) -> &Workers {
        let wake = Arc::clone(&self.wake);
        self.workers.get_or_insert_with(|| Workers::new(wake))
    }

    pub fn stats(&self, name: &str) -> Option<VectorOverlayStats> {
        self.list.iter().find(|o| o.name == name).map(|o| {
            let mut s = o.stats.clone();
            s.tiles_wanted = o.wanted.len();
            s.tiles_held = o.tiles.len();
            s.tiles_in_flight = o.in_flight.len();
            s.tiles_absent = o.absent.len();
            s.tiles_failed = o.failed.len();
            s
        })
    }

    /// Whether every tile overlay `name` wants has landed (or is absent or
    /// failed) and been pushed at the camera's zoom bucket.
    pub fn settled(&self, name: &str, bucket: u8) -> bool {
        self.list.iter().find(|o| o.name == name).is_some_and(|o| {
            !o.opening
                && o.in_flight.is_empty()
                && o.pushed.as_ref().is_some_and(|(tiles, b)| *b == bucket && o.wanted.iter().all(|t| tiles.contains(t) || o.absent.contains(t) || o.failed.contains_key(t)))
        })
    }

    /// One frame's work, main thread: lands what the workers finished,
    /// queues what the camera wants, pushes what settled. `true` when a
    /// source changed (a frame is owed); a landing wakes the host itself.
    pub fn frame(&mut self, overlays: &mut crate::overlay::host::ShellOverlays, camera: &crate::render::Camera, bucket: u8) -> bool {
        if self.list.is_empty() {
            return false;
        }
        self.land();
        let mut changed = false;
        let now = Instant::now();
        for at in 0..self.list.len() {
            let want = want(&self.list[at], camera);
            let (generation, name, url, layers) = {
                let o = &self.list[at];
                (o.generation, o.name.clone(), o.url.clone(), Arc::clone(&o.layers))
            };
            let starts: Vec<TileId> = {
                let o = &mut self.list[at];
                if o.wanted != want {
                    o.wanted = want.clone();
                }
                let room = MAX_IN_FLIGHT.saturating_sub(o.in_flight.len());
                want.iter()
                    .filter(|t| !o.tiles.contains_key(t) && !o.in_flight.contains(t) && !o.absent.contains(t) && o.failed.get(t).is_none_or(|at| now.duration_since(*at) >= RETRY_AFTER))
                    .take(room)
                    .copied()
                    .collect()
            };
            // A queued tile the camera left is dropped before it reaches a
            // socket; it is asked for again if the camera comes back.
            if let Some(w) = &self.workers {
                let keep: HashSet<TileId> = want.iter().copied().collect();
                for (_, _, tile) in w.retain(|j| match j {
                    Job::Tile { name: n, tile, .. } if *n == name => keep.contains(tile),
                    _ => true,
                }) {
                    self.list[at].in_flight.remove(&tile);
                }
            }
            for tile in starts {
                self.list[at].in_flight.insert(tile);
                let job = Job::Tile { name: name.clone(), generation, tile, url: url.clone(), layers: Arc::clone(&layers), zoom: camera.zoom };
                self.workers().submit(job);
            }
            let o = &self.list[at];
            let settled = o.wanted.iter().all(|t| !o.in_flight.contains(t));
            let loaded: Vec<TileId> = o.wanted.iter().filter(|t| o.tiles.contains_key(t)).copied().collect();
            let differs = o.pushed.as_ref().is_none_or(|(tiles, b)| *tiles != loaded || *b != bucket);
            let due = settled || o.last_push.is_none_or(|t| t.elapsed() >= PUSH_EVERY);
            if differs && due && (!loaded.is_empty() || settled) && !o.opening {
                push(&mut self.list[at], overlays, camera.zoom, bucket);
                changed = true;
            }
        }
        if changed {
            // A source created by this push went on top of the engine's
            // stack: put every overlay's sources back in list order.
            self.apply_order(overlays);
        }
        changed
    }

    /// Draws the overlays in `names`' order, bottom first (an overlay not
    /// named keeps its place after them); MapLibre's `moveLayer`.
    pub fn set_order(&mut self, overlays: &mut crate::overlay::host::ShellOverlays, names: &[&str]) {
        let rank = |o: &Overlay| names.iter().position(|n| *n == o.name).unwrap_or(usize::MAX);
        self.list.sort_by_key(rank);
        self.apply_order(overlays);
    }

    fn apply_order(&self, overlays: &mut crate::overlay::host::ShellOverlays) {
        let all: Vec<String> = self.list.iter().flat_map(Overlay::source_names).collect();
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        overlays.set_source_order(&refs);
    }

    fn land(&mut self) {
        let Some(w) = &self.workers else { return };
        let landed: Vec<Landed> = w.landed.try_iter().collect();
        for l in landed {
            match l {
                Landed::Opened { name, generation, zooms } => {
                    let Some(o) = self.list.iter_mut().find(|o| o.name == name && o.generation == generation) else { continue };
                    o.opening = false;
                    match zooms {
                        Ok((_, max)) => o.max_zoom = Some(max),
                        Err(e) => {
                            crate::mlog!("[vector-overlay] {name}: the archive did not open: {}", crate::log::scrub(&e));
                            o.stats.last_error = Some(e);
                            // Retried by the next add; nothing draws.
                            o.max_zoom = None;
                        }
                    }
                }
                Landed::Tile { name, generation, tile, result } => {
                    let Some(o) = self.list.iter_mut().find(|o| o.name == name && o.generation == generation) else { continue };
                    o.in_flight.remove(&tile);
                    match result {
                        Ok(Some(features)) => {
                            o.tiles.insert(tile, Arc::new(features));
                            o.failed.remove(&tile);
                        }
                        Ok(None) => {
                            o.absent.insert(tile);
                        }
                        Err(e) => {
                            crate::mlog!("[vector-overlay] {name}: tile {}/{}/{}: {}", tile.z, tile.x, tile.y, crate::log::scrub(&e));
                            o.stats.last_error = Some(e);
                            o.failed.insert(tile, Instant::now());
                        }
                    }
                    let keep: HashSet<TileId> = o.wanted.iter().chain(o.pushed.iter().flat_map(|(t, _)| t.iter())).copied().collect();
                    if o.tiles.len() > keep.len() + 32 {
                        o.tiles.retain(|t, _| keep.contains(t));
                    }
                }
            }
        }
    }

    /// The overlay feature under a screen point (in points), topmost
    /// overlay first: circles against their pushed positions (radius plus
    /// a finger's slop), fills and lines by the engine's own hit test
    /// (`engine_hit`, already computed by the caller). `(overlay, id)`.
    pub fn pick(&self, camera: &crate::render::Camera, x: f64, y: f64, engine_hit: Option<&crate::overlay::host::FeatureRef>) -> Option<(String, String)> {
        for o in self.list.iter().rev() {
            for source in o.source_names().iter().rev() {
                let Some(p) = o.sources.get(source) else { continue };
                if let Some(hit) = engine_hit.filter(|h| &*h.source == source.as_str()) {
                    return Some((o.name.clone(), format!("{source}#{}", hit.index)));
                }
                let nearest = p
                    .points
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (ll, r))| {
                        let (sx, sy, _) = camera.project_lon_lat_alt(ll[0], ll[1], 0.0)?;
                        let d = (f64::from(sx) - x).hypot(f64::from(sy) - y);
                        (d <= f64::from(*r) + TAP_SLOP_PT).then_some((d, i))
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0));
                if let Some((_, i)) = nearest {
                    // A stroke ring answers as its fill's feature.
                    let fill = source.strip_suffix("/stroke").unwrap_or(source);
                    let index = if fill != source.as_str() { o.sources.get(fill).and_then(|f| f.properties.iter().position(|q| Arc::ptr_eq(q, &p.properties[i]))).unwrap_or(i) } else { i };
                    return Some((o.name.clone(), format!("{fill}#{index}")));
                }
            }
        }
        None
    }

    /// The properties of a feature [`Self::pick`] answered, sorted by key.
    pub fn properties(&self, overlay: &str, id: &str) -> Option<Vec<(String, crate::overlay::host::TagValue)>> {
        let o = self.list.iter().find(|o| o.name == overlay)?;
        let (source, index) = id.rsplit_once('#')?;
        let p = o.sources.get(source)?.properties.get(index.parse::<usize>().ok()?)?;
        let mut out: Vec<(String, crate::overlay::host::TagValue)> = p.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Some(out)
    }

    /// Features in the last push of every source of overlay `name`, by
    /// source: what a test reads.
    pub fn pushed_counts(&self, name: &str) -> Vec<(String, usize)> {
        let Some(o) = self.list.iter().find(|o| o.name == name) else { return Vec::new() };
        let mut v: Vec<(String, usize)> = o.sources.iter().map(|(k, p)| (k.clone(), p.properties.len())).collect();
        v.sort();
        v
    }
}

/// The tiles overlay `o` wants for `camera`.
fn want(o: &Overlay, camera: &crate::render::Camera) -> Vec<TileId> {
    let Some(max_zoom) = o.max_zoom else { return Vec::new() };
    let Some(z) = vector_tiles::source_zoom(camera.zoom, o.options.min_zoom, max_zoom) else { return Vec::new() };
    let (min, max) = crate::overlay::host::point_viewport_box(camera, VIEW_GROW);
    vector_tiles::cover(min, max, z, o.options.max_tiles)
}

/// Whether two tiles (of any zooms) overlap.
fn tiles_overlap(a: TileId, b: TileId) -> bool {
    let (hi, lo) = if a.z >= b.z { (a, b) } else { (b, a) };
    let shift = hi.z - lo.z;
    (hi.x >> shift) == lo.x && (hi.y >> shift) == lo.y
}

/// Pushes overlay `o`'s landed tiles -- every wanted tile held, plus the
/// last push's tiles over any wanted tile still missing -- one engine
/// source per style layer.
fn push(o: &mut Overlay, overlays: &mut crate::overlay::host::ShellOverlays, camera_zoom: f64, bucket: u8) {
    let zoom = f64::from(bucket);
    let loaded: Vec<TileId> = o.wanted.iter().filter(|t| o.tiles.contains_key(t)).copied().collect();
    let missing: Vec<TileId> = o.wanted.iter().filter(|t| !o.tiles.contains_key(t)).copied().collect();
    let mut draw: Vec<Arc<TileFeatures>> = loaded.iter().filter_map(|t| o.tiles.get(t)).cloned().collect();
    if let Some((previous, _)) = &o.pushed {
        for t in previous.iter().filter(|t| !loaded.contains(t)) {
            if missing.iter().any(|m| tiles_overlap(*m, *t)) {
                if let Some(f) = o.tiles.get(t) {
                    draw.push(Arc::clone(f));
                }
            }
        }
    }
    let mut total = 0usize;
    let mut sources: HashMap<String, Pushed> = HashMap::new();
    for (i, layer) in o.layers.iter().enumerate() {
        let visible = layer.visible_at(camera_zoom);
        let features: Vec<&vector_tiles::VectorFeature> = if visible { draw.iter().flat_map(|t| t.layers.get(i).into_iter().flatten()).collect() } else { Vec::new() };
        match layer.kind {
            VectorLayerKind::Circle => {
                let mut fill: Vec<([f64; 2], FeaturePaint, Arc<FeatureProperties>)> = Vec::new();
                let mut stroke: Vec<([f64; 2], FeaturePaint, Arc<FeatureProperties>)> = Vec::new();
                for f in &features {
                    let Some(paint) = layer.circle_paint(usize::from(f.case), zoom) else { continue };
                    let VectorGeometry::Points(points) = &f.geometry else { continue };
                    for p in points {
                        fill.push((*p, FeaturePaint { circle_radius: paint.radius, circle_color: paint.color, ..FeaturePaint::default() }, Arc::clone(&f.properties)));
                        if paint.stroke_width > 0.0 {
                            stroke.push((*p, FeaturePaint { circle_radius: paint.radius + paint.stroke_width, circle_color: paint.stroke_color, ..FeaturePaint::default() }, Arc::clone(&f.properties)));
                        }
                    }
                }
                total += fill.len();
                if layer.may_stroke() {
                    let name = stroke_source_name(&o.name, &layer.id);
                    push_points(overlays, &name, &stroke, bucket);
                    sources.insert(name, Pushed { points: stroke.iter().map(|(p, paint, _)| (*p, paint.circle_radius)).collect(), properties: stroke.into_iter().map(|(_, _, q)| q).collect() });
                }
                let name = layer_source_name(&o.name, &layer.id);
                push_points(overlays, &name, &fill, bucket);
                sources.insert(name, Pushed { points: fill.iter().map(|(p, paint, _)| (*p, paint.circle_radius)).collect(), properties: fill.into_iter().map(|(_, _, q)| q).collect() });
            }
            VectorLayerKind::Fill | VectorLayerKind::Line => {
                let mut runs: Vec<Vec<&[[f64; 2]]>> = Vec::new();
                let mut polygons: Vec<Vec<Vec<&[[f64; 2]]>>> = Vec::new();
                let mut paints: Vec<FeaturePaint> = Vec::new();
                let mut kept: Vec<&vector_tiles::VectorFeature> = Vec::new();
                for f in &features {
                    let case = usize::from(f.case);
                    match (&f.geometry, layer.kind) {
                        (VectorGeometry::Polygons(p), VectorLayerKind::Fill) => {
                            let Some(color) = layer.fill_paint(case, zoom) else { continue };
                            polygons.push(p.iter().map(|rings| rings.iter().map(Vec::as_slice).collect()).collect());
                            paints.push(FeaturePaint { fill_color: color, line_width: ZoomValue::constant(0.0), ..FeaturePaint::default() });
                            kept.push(f);
                        }
                        (VectorGeometry::Lines(l), VectorLayerKind::Line) => {
                            let Some(line) = layer.line_paint(case, zoom) else { continue };
                            runs.push(l.iter().map(Vec::as_slice).collect());
                            paints.push(FeaturePaint { line_color: line.color, line_width: line.width, line_dash: line.dash, ..FeaturePaint::default() });
                            kept.push(f);
                        }
                        _ => {}
                    }
                }
                let polygon_refs: Vec<Vec<&[&[[f64; 2]]]>> = polygons.iter().map(|p| p.iter().map(Vec::as_slice).collect()).collect();
                let inputs: Vec<DataFeatureInput<'_>> = kept
                    .iter()
                    .enumerate()
                    .map(|(k, f)| {
                        let geometry = match layer.kind {
                            VectorLayerKind::Fill => GeometryInput::Polygon(&polygon_refs[k]),
                            _ => GeometryInput::Line(&runs[k]),
                        };
                        DataFeatureInput { feature: FeatureInput { id: None, geometry, paint: paints[k].clone() }, properties: Arc::clone(&f.properties) }
                    })
                    .collect();
                total += inputs.len();
                let name = layer_source_name(&o.name, &layer.id);
                if inputs.is_empty() {
                    overlays.remove_source(&name);
                } else if let Err(e) = overlays.set_source_data(&name, &inputs, false, bucket) {
                    crate::mlog!("[vector-overlay] {}: push of {name}: {e}", o.name);
                }
                sources.insert(name, Pushed { points: Vec::new(), properties: kept.iter().map(|f| Arc::clone(&f.properties)).collect() });
            }
        }
    }
    o.sources = sources;
    o.pushed = Some((loaded, bucket));
    o.last_push = Some(Instant::now());
    o.stats.pushes += 1;
    o.stats.features_pushed = total;
    o.stats.zoom = o.wanted.first().map(|t| t.z);
}

fn push_points(overlays: &mut crate::overlay::host::ShellOverlays, name: &str, points: &[([f64; 2], FeaturePaint, Arc<FeatureProperties>)], bucket: u8) {
    if points.is_empty() {
        overlays.remove_source(name);
        return;
    }
    let positions: Vec<[[f64; 2]; 1]> = points.iter().map(|(p, _, _)| [*p]).collect();
    let inputs: Vec<DataFeatureInput<'_>> = points
        .iter()
        .zip(&positions)
        .map(|((_, paint, properties), at)| DataFeatureInput { feature: FeatureInput { id: None, geometry: GeometryInput::Point(at), paint: paint.clone() }, properties: Arc::clone(properties) })
        .collect();
    if let Err(e) = overlays.set_source_data(name, &inputs, true, bucket) {
        crate::mlog!("[vector-overlay] push of {name}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_overlap_across_zooms() {
        let parent = TileId { z: 10, x: 163, y: 395 };
        assert!(tiles_overlap(parent, TileId { z: 12, x: 655, y: 1583 }));
        assert!(!tiles_overlap(parent, TileId { z: 12, x: 660, y: 1583 }));
    }

    #[test]
    fn source_names_are_per_layer() {
        assert_eq!(layer_source_name("fsq", "places"), "fsq/places");
        assert_eq!(stroke_source_name("fsq", "places"), "fsq/places/stroke");
    }
}
