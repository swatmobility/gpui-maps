//! `MapEngine`: gpui-maps for a native iOS app, through uniffi.
//!
//! The app owns the UIKit side -- a `UIView` whose `layerClass` is
//! `CAMetalLayer`, a `CADisplayLink`, and the touches -- and this crate owns
//! the map: tiles, the basemap style, gestures, overlays, the credit plate.
//!
//! ```text
//! let engine = try MapEngine(layerPtr: ptr, width: w, height: h, scale: s, cacheDir: dir)
//! engine.setDelegate(delegate: self)                 // on_tap / on_camera_changed / on_needs_frame
//! // each display-link tick while running:
//! switch engine.render(t: link.targetTimestamp) {
//!   case .idle: link.isPaused = true                  // until on_needs_frame
//!   case .again: break
//!   case .at(let t): link.isPaused = true; schedule a wake at t
//! }
//! engine.touch(phase: .began, id: id, x: pt.x, y: pt.y, t: t)
//! try engine.upsertPoints(layer: "stops", features: [...])
//! ```
//!
//! # Threads
//!
//! Main thread only, like UIKit: every method checks it is called on the
//! thread that created the engine, and panics otherwise. The one exception
//! is the delegate's `on_needs_frame`, which tile worker threads call when a
//! tile lands -- dispatch to the main queue there. Delegate methods are
//! called after the engine's own lock is released, so they may call back
//! into the engine, EXCEPT `on_needs_frame`, which can arrive from inside an
//! update call: unpause the display link there and nothing more.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use gpui_maps_ios::app::host::{self, Host, HostLine, HostPoint, HostPolygon};
use gpui_maps_ios::app::SurfaceMetrics;
use gpui_maps_ios::camera_control::CameraTarget;

uniffi::setup_scaffolding!();

mod layers3d;
pub use layers3d::*;
mod nav;
pub use nav::*;
mod space;
pub use space::*;

/// What the display link should do after [`MapEngine::render`].
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Enum)]
pub enum FrameRequest {
    /// Nothing is owed: pause the link until `on_needs_frame`.
    Idle,
    /// Still moving or loading: render again next vsync.
    Again,
    /// Nothing until host time `t` (seconds, the clock `render` was given),
    /// unless `on_needs_frame` fires first.
    At { t: f64 },
}

/// A touch's phase, as `UITouch.Phase` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TouchPhase {
    Began,
    Moved,
    Ended,
    Cancelled,
}

/// A position, degrees.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct LngLat {
    pub lon: f64,
    pub lat: f64,
}

/// One feature property's value, read by a layer's paint expressions.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum PropValue {
    Number { value: f64 },
    Text { value: String },
    Bool { value: bool },
}

/// A point, by stable id.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PointFeature {
    pub id: String,
    pub lon: f64,
    pub lat: f64,
    pub props: HashMap<String, PropValue>,
}

/// A line, by stable id.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct LineFeature {
    pub id: String,
    pub coords: Vec<LngLat>,
    pub props: HashMap<String, PropValue>,
}

/// A polygon, by stable id: rings, exterior first, then holes.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PolygonFeature {
    pub id: String,
    pub rings: Vec<Vec<LngLat>>,
    pub props: HashMap<String, PropValue>,
}

/// The camera. Angles in degrees; bearing clockwise from north.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct CameraState {
    pub lon: f64,
    pub lat: f64,
    pub zoom: f64,
    pub bearing_deg: f64,
    pub pitch_deg: f64,
}

/// A camera move: every `None` keeps its current value. A centre needs both
/// `lon` and `lat`.
#[derive(Debug, Clone, Copy, PartialEq, Default, uniffi::Record)]
pub struct CameraUpdate {
    #[uniffi(default = None)]
    pub lon: Option<f64>,
    #[uniffi(default = None)]
    pub lat: Option<f64>,
    #[uniffi(default = None)]
    pub zoom: Option<f64>,
    #[uniffi(default = None)]
    pub bearing_deg: Option<f64>,
    #[uniffi(default = None)]
    pub pitch_deg: Option<f64>,
}

/// The bands of the view the engine's chrome (compass, credits, logo) keeps
/// clear of, in points: the safe area, plus whatever the app floats over
/// the map.
#[derive(Debug, Clone, Copy, PartialEq, Default, uniffi::Record)]
pub struct EdgeInsets {
    pub top: f64,
    pub bottom: f64,
    #[uniffi(default = 0.0)]
    pub left: f64,
    #[uniffi(default = 0.0)]
    pub right: f64,
}

/// What a vector overlay ([`MapEngine::add_vector_overlay`]) holds and
/// has drawn.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct VectorOverlayStats {
    /// Tiles the view wants, at the source zoom.
    pub tiles_wanted: u32,
    /// Decoded tiles held.
    pub tiles_held: u32,
    pub tiles_in_flight: u32,
    /// Tiles the source has no data for.
    pub tiles_absent: u32,
    pub tiles_failed: u32,
    /// Features in the last draw, over every style layer.
    pub features_drawn: u32,
    pub pushes: u64,
    /// The source zoom of the last draw.
    pub zoom: Option<u8>,
    /// Every wanted tile has landed and been drawn.
    pub settled: bool,
    /// The last fetch or decode error.
    pub last_error: Option<String>,
}

/// The overlay feature under a point.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FeatureHit {
    pub layer: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Error)]
#[uniffi(flat_error)]
pub enum MapError {
    /// The `CAMetalLayer` could not become a surface.
    Surface(String),
    /// A layer holds a different kind of feature.
    KindMismatch(String),
    /// A layer's paint JSON was refused.
    Paint(String),
    /// No basemap by that name.
    UnknownBasemap(String),
    /// The engine could not start (its first style did not compile).
    Startup(String),
    /// A drive could not start (no route loaded, or it did not parse).
    Navigation(String),
    /// A basemap archive is not there.
    Archive(String),
}

impl std::error::Error for MapError {}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(m) | Self::KindMismatch(m) | Self::Paint(m) | Self::UnknownBasemap(m) | Self::Startup(m) | Self::Navigation(m) | Self::Archive(m) => {
                f.write_str(m)
            }
        }
    }
}

impl From<host::HostError> for MapError {
    fn from(e: host::HostError) -> Self {
        let message = e.to_string();
        match e {
            host::HostError::KindMismatch { .. } => Self::KindMismatch(message),
            host::HostError::Paint { .. } | host::HostError::Overlay { .. } => Self::Paint(message),
            host::HostError::UnknownBasemap(_) => Self::UnknownBasemap(message),
            host::HostError::Surface(_) => Self::Surface(message),
        }
    }
}

/// The app's side of the conversation.
#[uniffi::export(with_foreign)]
pub trait MapEngineDelegate: Send + Sync {
    /// A tap landed on overlay feature `id` of `layer`. Main thread.
    fn on_tap(&self, layer: String, id: String);
    /// The camera moved (a gesture, a glide, an ease, a jump), reported
    /// once per rendered frame that moved it. Main thread.
    fn on_camera_changed(&self, camera: CameraState);
    /// A frame is owed while the display link may be paused: unpause it.
    /// ANY thread (tile workers call it), and possibly from inside another
    /// engine call -- do not call back into the engine from here.
    fn on_needs_frame(&self);
}

/// `T`, reachable only from the thread that created it. `Host` holds GPU
/// and Objective-C state that is not `Send`; uniffi objects must be
/// `Send + Sync`. Every access goes through [`MapEngine::lock`], which
/// checks the thread first, so the unsafe impls below are never exercised
/// across threads -- except `Drop`, which Swift runs where the last
/// reference dies (the main thread, for a view's engine).
struct OwnedBy<T> {
    owner: ThreadId,
    value: T,
}

// SAFETY: see the type's doc -- every access is checked against `owner`.
unsafe impl<T> Send for OwnedBy<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for OwnedBy<T> {}

struct Inner {
    host: Host,
    delegate: Option<Arc<dyn MapEngineDelegate>>,
    metrics: SurfaceMetrics,
    /// The tile cache's directory, as given (`MapEngine::new`).
    #[allow(dead_code)]
    cache_dir: String,
}

/// The embedded map. See the crate doc.
#[derive(uniffi::Object)]
pub struct MapEngine {
    inner: Mutex<OwnedBy<Inner>>,
}

/// Events collected under the lock and delivered after it is released.
enum Event {
    Tap(FeatureHit),
    Camera(CameraState),
}

fn camera_state(c: &gpui_maps_ios::render::Camera) -> CameraState {
    CameraState {
        lon: c.center_lon,
        lat: c.center_lat,
        zoom: c.zoom,
        bearing_deg: f64::from(c.bearing).to_degrees(),
        pitch_deg: f64::from(c.pitch).to_degrees(),
    }
}

fn target_of(update: CameraUpdate) -> CameraTarget {
    CameraTarget {
        center: update.lon.zip(update.lat),
        zoom: update.zoom,
        bearing: update.bearing_deg.map(|d| d.to_radians() as f32),
        pitch: update.pitch_deg.map(|d| d.to_radians() as f32),
        ..Default::default()
    }
}

fn props_of(props: HashMap<String, PropValue>) -> Vec<(String, host::PropValue)> {
    props
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                PropValue::Number { value } => host::PropValue::Number(value),
                PropValue::Text { value } => host::PropValue::Text(value),
                PropValue::Bool { value } => host::PropValue::Bool(value),
            };
            (k, v)
        })
        .collect()
}

fn ring_of(coords: Vec<LngLat>) -> Vec<[f64; 2]> {
    coords.into_iter().map(|p| [p.lon, p.lat]).collect()
}

impl MapEngine {
    /// The state, after checking this is the creating thread.
    fn lock(&self) -> MutexGuard<'_, OwnedBy<Inner>> {
        let guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            guard.owner,
            std::thread::current().id(),
            "MapEngine is main-thread only: call it on the thread that created it"
        );
        guard
    }

    fn deliver(&self, delegate: Option<Arc<dyn MapEngineDelegate>>, events: Vec<Event>) {
        let Some(delegate) = delegate else { return };
        for event in events {
            match event {
                Event::Tap(hit) => delegate.on_tap(hit.layer, hit.id),
                Event::Camera(camera) => delegate.on_camera_changed(camera),
            }
        }
    }
}

#[uniffi::export]
impl MapEngine {
    /// A map rendering into the `CAMetalLayer` at `layer_ptr`
    /// (`Unmanaged.passUnretained(layer).toOpaque()`), `width` x `height`
    /// PHYSICAL pixels at `scale` pixels per point. The layer must outlive
    /// the engine. `layer_ptr` 0 makes a headless engine (no drawing; for
    /// tests). Starts on OpenFreeMap at Singapore, zoom 13.
    #[uniffi::constructor]
    pub fn new(layer_ptr: u64, width: u32, height: u32, scale: f64, cache_dir: String) -> Result<Arc<Self>, MapError> {
        let mut host = Host::new().map_err(|e| MapError::Startup(format!("{e:#}")))?;
        let metrics = SurfaceMetrics { width_px: width, height_px: height, scale, top_inset_pt: 0.0, bottom_inset_pt: 0.0 };
        host.resize(metrics);
        // Tiles on disk under the app's cache directory; a cache that cannot
        // open is a slower map, not a failed one.
        if let Err(e) = host.set_cache_dir(&cache_dir, gpui_maps_ios::disk_cache::DEFAULT_MAX_BYTES) {
            gpui_maps_ios::mlog!("[embed] {e}");
        }
        if layer_ptr != 0 {
            // SAFETY: the caller's contract (a live CAMetalLayer that
            // outlives the engine), and this is the creating thread.
            unsafe { host.attach_layer(layer_ptr as usize as *mut std::ffi::c_void, metrics) }?;
        }
        Ok(Arc::new(Self {
            inner: Mutex::new(OwnedBy {
                owner: std::thread::current().id(),
                value: Inner { host, delegate: None, metrics, cache_dir },
            }),
        }))
    }

    /// Who hears taps, camera moves and frame requests. Held strongly:
    /// make the delegate's back-reference to its view weak.
    pub fn set_delegate(&self, delegate: Arc<dyn MapEngineDelegate>) {
        let mut guard = self.lock();
        let needs_frame = Arc::clone(&delegate);
        guard.value.host.set_needs_frame(Arc::new(move || needs_frame.on_needs_frame()));
        guard.value.delegate = Some(delegate);
    }

    /// The layer's new drawable size, in physical pixels.
    pub fn resize(&self, width: u32, height: u32, scale: f64) {
        let mut guard = self.lock();
        let inner = &mut guard.value;
        inner.metrics = SurfaceMetrics { width_px: width, height_px: height, scale, ..inner.metrics };
        inner.host.resize(inner.metrics);
    }

    /// The bands of the view the app covers -- the safe area plus a bottom
    /// sheet, a search bar -- in points. Cheap: call it on every frame of a
    /// sheet drag. Moves the credits, compass and logo clear of the top and
    /// bottom bands, and centres later `jump_to`/`ease_to` moves in the
    /// uncovered area.
    pub fn set_insets(&self, insets: EdgeInsets) {
        let mut guard = self.lock();
        let inner = &mut guard.value;
        inner.metrics = SurfaceMetrics { top_inset_pt: insets.top as f32, bottom_inset_pt: insets.bottom as f32, ..inner.metrics };
        inner.host.set_padding(insets.top as f32, insets.bottom as f32, insets.left as f32, insets.right as f32);
    }

    /// Stop drawing (backgrounded); `render` answers `Idle` until `resume`.
    /// The engine's diagnostic lines (frame stats, fetch failures, gestures)
    /// in the device log; off by default. Never coordinates, tile x/y or
    /// URLs: counts and timings only.
    pub fn set_debug_logging(&self, on: bool) {
        self.lock().value.host.set_debug_logging(on);
    }

    pub fn pause(&self) {
        self.lock().value.host.pause();
    }

    pub fn resume(&self) {
        self.lock().value.host.resume();
    }

    /// One display-link tick at host time `t` seconds
    /// (`CADisplayLink.targetTimestamp`, or `CACurrentMediaTime()`).
    pub fn render(&self, t: f64) -> FrameRequest {
        let (request, delegate, events) = {
            let mut guard = self.lock();
            let inner = &mut guard.value;
            let request = inner.host.render();
            let events: Vec<Event> =
                inner.host.take_camera_change().map(|c| Event::Camera(camera_state(&c))).into_iter().collect();
            (request, inner.delegate.clone(), events)
        };
        self.deliver(delegate, events);
        match request {
            host::FrameRequest::Idle => FrameRequest::Idle,
            host::FrameRequest::Again => FrameRequest::Again,
            host::FrameRequest::At(at) => {
                FrameRequest::At { t: t + at.saturating_duration_since(Instant::now()).as_secs_f64() }
            }
        }
    }

    /// One touch, in points from the view's top-left. A tap that lands on an
    /// overlay feature reaches the delegate's `on_tap`. `t` is the event's
    /// host time; the engine currently stamps touches on arrival instead.
    pub fn touch(&self, phase: TouchPhase, id: u64, x: f64, y: f64, t: f64) {
        let _ = t;
        let (delegate, events) = {
            let mut guard = self.lock();
            let inner = &mut guard.value;
            let phase = match phase {
                TouchPhase::Began => gpui_maps_ios::app::TouchPhase::Started,
                TouchPhase::Moved => gpui_maps_ios::app::TouchPhase::Moved,
                TouchPhase::Ended => gpui_maps_ios::app::TouchPhase::Ended,
                TouchPhase::Cancelled => gpui_maps_ios::app::TouchPhase::Cancelled,
            };
            let hit = inner.host.touch(phase, id, x, y);
            let events: Vec<Event> =
                hit.map(|h| Event::Tap(FeatureHit { layer: h.layer, id: h.id })).into_iter().collect();
            (inner.delegate.clone(), events)
        };
        self.deliver(delegate, events);
    }

    /// The overlay feature under a point (in points), topmost layer first.
    pub fn feature_at(&self, x: f64, y: f64) -> Option<FeatureHit> {
        self.lock().value.host.feature_at(x, y).map(|h| FeatureHit { layer: h.layer, id: h.id })
    }

    // -- basemap ------------------------------------------------------------

    /// The basemap styles available, by name (all keyless, OpenFreeMap).
    pub fn basemaps(&self) -> Vec<String> {
        self.lock().value.host.basemaps()
    }

    pub fn set_basemap(&self, name: String) -> Result<(), MapError> {
        Ok(self.lock().value.host.set_basemap(&name)?)
    }

    /// Draws the basemap from an OpenMapTiles PMTiles archive -- an offline
    /// region the app downloaded: an absolute path, `file://<path>` or an
    /// `https://` URL -- instead of the online tiles; `None` returns to
    /// them. The style (Liberty, Dark), camera and overlays stay. Outside
    /// the archive's bounds the map draws the online tiles when
    /// `online_outside` (and the archive's border tiles when offline), else
    /// it is blank. `credit` defaults to OpenMapTiles and OpenStreetMap.
    pub fn set_basemap_archive(&self, archive: Option<String>, credit: Option<String>, online_outside: bool) -> Result<(), MapError> {
        let archive = archive.filter(|a| !a.is_empty());
        if let Some(a) = &archive {
            let path = a.strip_prefix("file://").unwrap_or(a);
            if path.starts_with('/') && !std::path::Path::new(path).is_file() {
                return Err(MapError::Archive(format!("no archive at {path}")));
            }
        }
        let archive = archive.map(|archive| gpui_maps_ios::app::BasemapArchive {
            archive,
            credit: credit.unwrap_or_else(|| gpui_maps_ios::app::BasemapArchive::OMT_CREDIT.to_string()),
            online_outside,
        });
        self.lock().value.host.set_basemap_archive(archive);
        Ok(())
    }

    /// The imagery the basemap style's `raster` layers draw, as a
    /// `{z}/{x}/{y}` template (`tile_size` px, up to `max_zoom`), with its
    /// credit -- a satellite basemap's tiles, a COG or a WMS served as XYZ.
    /// `None` returns to the style's own raster source (a shaded relief),
    /// or none. The engine's own raster path draws it (`raster_plan`, the
    /// one every shell shares); a style without a `raster` layer draws no
    /// imagery whatever is set here.
    pub fn set_raster_source(&self, template: Option<String>, tile_size: u32, max_zoom: u8, credit: Option<String>) {
        let raster = template.filter(|t| !t.is_empty()).map(|template| gpui_maps_ios::app::RasterConfig { template, tile_size: tile_size.max(64), max_zoom, credit });
        self.lock().value.host.set_raster_source(raster);
    }

    /// Every credit the map owes; the engine also draws them on the map.
    pub fn attributions(&self) -> Vec<String> {
        self.lock().value.host.attributions()
    }

    /// `true`: the app shows the credits itself (every one `attributions`
    /// lists, behind its own button, e.g. a collapsible ⓘ with the data's
    /// credits beside the basemap's), and the engine draws no credit plate.
    /// The SWAT mark stays. Off by default.
    pub fn set_credits_by_host(&self, on: bool) {
        self.lock().value.host.set_credits_by_host(on);
    }

    // -- camera -------------------------------------------------------------

    pub fn camera(&self) -> CameraState {
        camera_state(&self.lock().value.host.camera())
    }

    pub fn jump_to(&self, update: CameraUpdate) {
        self.lock().value.host.jump_to(target_of(update));
    }

    /// Right-to-left layout (`UIView.effectiveUserInterfaceLayoutDirection`):
    /// mirrors the engine chrome that has a side (the compass to the
    /// top-left). The SWAT mark stays bottom-left and the credits
    /// bottom-right.
    pub fn set_layout_direction(&self, rtl: bool) {
        self.lock().value.host.set_layout_direction(rtl);
    }

    /// The basemap's 3D buildings on (default) or off: off draws their
    /// footprints flat at the ground in the same colour. Overlays always
    /// draw above the buildings either way. Cheap: no style rebuild.
    /// Labels over data: `true` while the host draws data fills under the
    /// basemap labels (a Studio document decides with
    /// `swat_maps_doc::dashboard::labels_on_data`), so every label draws dark
    /// in a light halo and reads on any palette colour.
    pub fn set_labels_on_data(&self, on: bool) {
        self.lock().value.host.set_labels_on_data(on);
    }

    pub fn set_buildings_3d(&self, on: bool) {
        self.lock().value.host.set_buildings_3d(on);
    }

    pub fn ease_to(&self, update: CameraUpdate, duration_s: f64) {
        let duration = Duration::from_secs_f64(duration_s.clamp(0.0, 30.0));
        self.lock().value.host.ease_to(target_of(update), duration);
    }

    /// Frames the box `west..east` x `south..north` (degrees) inside the area
    /// the insets leave uncovered, `margin` points inside its edges, north
    /// up and flat. `max_zoom` caps a tiny box (one stop): 17 is a street.
    /// `duration_s` 0 jumps.
    pub fn fit_bounds(&self, west: f64, south: f64, east: f64, north: f64, margin: f64, max_zoom: f64, duration_s: f64) {
        let duration = Duration::from_secs_f64(duration_s.clamp(0.0, 30.0));
        self.lock().value.host.fit_bounds([west, south, east, north], margin.max(0.0), max_zoom, duration);
    }

    // -- overlays -----------------------------------------------------------

    /// Adds `features` to `layer`, replacing any with the same id. Creates
    /// the layer (on top of the others) the first time. Drawn from the next
    /// frame; any number of upserts between two frames cost one push.
    pub fn upsert_points(&self, layer: String, features: Vec<PointFeature>) -> Result<(), MapError> {
        let points = features
            .into_iter()
            .map(|f| HostPoint { id: f.id, lon: f.lon, lat: f.lat, props: props_of(f.props) })
            .collect();
        Ok(self.lock().value.host.upsert_points(&layer, points)?)
    }

    pub fn upsert_lines(&self, layer: String, features: Vec<LineFeature>) -> Result<(), MapError> {
        let lines = features
            .into_iter()
            .map(|f| HostLine { id: f.id, parts: vec![ring_of(f.coords)], props: props_of(f.props) })
            .collect();
        Ok(self.lock().value.host.upsert_lines(&layer, lines)?)
    }

    pub fn upsert_polygons(&self, layer: String, features: Vec<PolygonFeature>) -> Result<(), MapError> {
        let polygons = features
            .into_iter()
            .map(|f| HostPolygon { id: f.id, rings: f.rings.into_iter().map(ring_of).collect(), props: props_of(f.props) })
            .collect();
        Ok(self.lock().value.host.upsert_polygons(&layer, polygons)?)
    }

    /// Removes `ids` from `layer`; returns how many it held.
    pub fn remove(&self, layer: String, ids: Vec<String>) -> u32 {
        self.lock().value.host.remove(&layer, &ids) as u32
    }

    /// Empties `layer`; its style stays.
    pub fn clear(&self, layer: String) {
        self.lock().value.host.clear(&layer);
    }

    /// The ids `layer` holds, in no particular order.
    pub fn ids(&self, layer: String) -> Vec<String> {
        self.lock().value.host.ids(&layer)
    }

    /// Lights feature `id` of `layer` -- the one tapped or selected -- or,
    /// with `None`, clears the highlight; a new one replaces the last.
    /// Returns whether a highlight is drawn (`false` for a layer or id the
    /// map does not hold).
    ///
    /// A point gets a 3 pt white ring over a dark casing round its own circle,
    /// and is drawn again on top; a line or polygon gets its outline
    /// stroked, over every layer. It follows the feature through upserts
    /// and goes when the feature does. Never reported by a tap: a tap on
    /// it is a tap on the feature under it.
    pub fn set_highlight(&self, layer: String, id: Option<String>) -> bool {
        self.lock().value.host.set_highlight(&layer, id.as_deref(), touch_highlight())
    }

    /// The feature [`Self::set_highlight`] lights, if it is drawn.
    pub fn highlighted(&self) -> Option<FeatureHit> {
        self.lock().value.host.highlighted().map(|(layer, id)| FeatureHit { layer, id })
    }

    /// Styles `layer` with a style-spec paint object, e.g.
    /// `{"circle-radius": 6, "circle-color": ["get", "color"]}` or
    /// `{"line-width": 4, "line-color": "#0a7"}`. Constants apply to every
    /// feature; expressions read the features' `props`.
    pub fn set_style(&self, layer: String, paint_json: String) -> Result<(), MapError> {
        Ok(self.lock().value.host.set_style(&layer, &paint_json)?)
    }

    // -- vector overlays ----------------------------------------------------

    /// Adds (or replaces) a second vector-tile source over the basemap,
    /// drawn with its own style layers -- MapLibre's `addSource({type:
    /// "vector"})` plus an `addLayer` per entry of `style_layers_json` (a
    /// JSON array of `fill`, `line` and `circle` layers whose
    /// `source-layer`s name layers of the tiles, with their `filter`,
    /// `minzoom`/`maxzoom` and paint expressions). `url` is
    /// `pmtiles+<archive URL>`, `pmtiles+file://<path>` (an archive in the
    /// app's container) or an XYZ `{z}/{x}/{y}` template of MVT tiles.
    /// Nothing is fetched below `min_zoom`; `max_zoom` is the source's
    /// deepest level (`None`: the archive's header, or 14 for a template),
    /// overzoomed past it. `credit` joins `attributions` while the overlay
    /// is on. Tiles are fetched and decoded off the main thread, for the
    /// view. A tap on one of its features reports `name` as the layer, and
    /// [`Self::vector_feature_properties`] reads the feature. `clear(name)`
    /// removes it too.
    pub fn add_vector_overlay(&self, name: String, url: String, style_layers_json: String, min_zoom: u8, max_zoom: Option<u8>, credit: Option<String>) -> Result<(), MapError> {
        let options = gpui_maps_ios::app::vector_overlay::VectorOverlayOptions { min_zoom, max_zoom, credit: credit.filter(|c| !c.is_empty()), ..Default::default() };
        Ok(self.lock().value.host.add_vector_overlay(&name, &url, &style_layers_json, options)?)
    }

    /// Removes vector overlay `name` and everything it drew; whether there
    /// was one.
    pub fn remove_vector_overlay(&self, name: String) -> bool {
        self.lock().value.host.remove_vector_overlay(&name)
    }

    /// Draws the vector overlays in `names`' order, bottom first (one not
    /// named keeps its place after them). An overlay added later draws on
    /// top until this is called.
    pub fn set_vector_overlay_order(&self, names: Vec<String>) {
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.lock().value.host.set_vector_overlay_order(&refs);
    }

    /// The vector overlays, in draw order (the last draws on top).
    pub fn vector_overlays(&self) -> Vec<String> {
        self.lock().value.host.vector_overlays()
    }

    /// What vector overlay `name` holds and has drawn.
    pub fn vector_overlay_stats(&self, name: String) -> Option<VectorOverlayStats> {
        let settled = self.vector_overlay_settled(name.clone());
        self.lock().value.host.vector_overlay_stats(&name).map(|s| VectorOverlayStats {
            tiles_wanted: s.tiles_wanted as u32,
            tiles_held: s.tiles_held as u32,
            tiles_in_flight: s.tiles_in_flight as u32,
            tiles_absent: s.tiles_absent as u32,
            tiles_failed: s.tiles_failed as u32,
            features_drawn: s.features_pushed as u32,
            pushes: s.pushes,
            zoom: s.zoom,
            settled,
            last_error: s.last_error,
        })
    }

    /// Whether every tile overlay `name` wants for the view has landed and
    /// been drawn.
    pub fn vector_overlay_settled(&self, name: String) -> bool {
        self.lock().value.host.vector_overlay_settled(&name)
    }

    /// A tapped vector-overlay feature's own properties (`layer` and `id`
    /// as the tap reported them); `None` when it is no longer drawn.
    pub fn vector_feature_properties(&self, layer: String, id: String) -> Option<HashMap<String, PropValue>> {
        let props = self.lock().value.host.vector_feature_properties(&layer, &id)?;
        Some(
            props
                .into_iter()
                .map(|(k, v)| {
                    let v = match v {
                        host::PropValue::Number(value) => PropValue::Number { value },
                        host::PropValue::Text(value) => PropValue::Text { value },
                        host::PropValue::Bool(value) => PropValue::Bool { value },
                    };
                    (k, v)
                })
                .collect(),
        )
    }
}

/// The highlight a tap lights: the desktop's white-over-dark ring, a
/// little heavier (3 pt over 1.5 pt) so it reads at arm's length and next
/// to a layer's own white outline.
fn touch_highlight() -> gpui_maps_ios::overlay::HighlightStyle {
    gpui_maps_ios::overlay::HighlightStyle { outline_width_px: 3.0, casing_width_px: 1.5, ..Default::default() }
}

/// The drop-shaped heading marker a downstream desktop app draws for a vehicle:
/// a ring around `(lon, lat)` pointing along `heading_deg` (clockwise from
/// north), sized for `zoom` the way the desktop sizes it. Send it with
/// `upsert_polygons`.
#[uniffi::export]
pub fn heading_marker(lon: f64, lat: f64, heading_deg: f64, zoom: f64) -> Vec<LngLat> {
    let radius_m = host::marker_radius_px(zoom) * host::meters_per_pixel(zoom, lat);
    host::heading_marker_ring(lon, lat, heading_deg, radius_m).into_iter().map(|[lon, lat]| LngLat { lon, lat }).collect()
}

/// How many vehicles the desktop draws as drop polygons before switching
/// to plain circles.
#[uniffi::export]
pub fn marker_polygon_limit() -> u32 {
    host::MARKER_POLYGON_LIMIT as u32
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_3d;
