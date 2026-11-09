//! The embedding door: the same `MapApp` the standalone demo runs, driven by
//! a host that owns the UIKit side -- its own view, its own `CAMetalLayer`,
//! its own `CADisplayLink` and its own touch handling.
//!
//! What the host does, and what this does in answer:
//!
//! * hands over a `CAMetalLayer` ([`Host::attach_layer`]) and its size
//!   ([`Host::resize`]) -- the map renders into it;
//! * calls [`Host::render`] on every display-link tick while the last answer
//!   asked for one ([`FrameRequest`]), and pauses its link on
//!   [`FrameRequest::Idle`] until the needs-frame callback
//!   ([`Host::set_needs_frame`]) fires -- a tile landing on a worker thread,
//!   a touch, an overlay update;
//! * forwards touches in points ([`Host::touch`]), which run the same
//!   gesture code (`touch_gesture`, the kinetic pan, the twist) the demo
//!   app does, and get back the overlay feature a tap landed on;
//! * keeps overlay layers up to date BY STABLE ID ([`Host::upsert_points`],
//!   [`Host::upsert_lines`], [`Host::upsert_polygons`], [`Host::remove`]).
//!   The engine's sources replace whole; this layer keeps the id map, and a
//!   layer touched any number of times between two frames is re-pushed once,
//!   at the next [`Host::render`] ([`Host::flush`]).
//!
//! Main thread only, like everything UIKit: `MapApp` holds GPU and
//! Objective-C state that is not `Send`. The needs-frame callback is the one
//! thing called from other threads, and it must be `Send + Sync`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{MapApp, SurfaceMetrics, TileSchema, TouchPhase, TILE_URL};
use crate::camera_control::CameraTarget;
use crate::overlay::host::{FeatureId, FeaturePaint, LayerFeature, LayerGeometry};
use crate::geo::TileId;
use crate::render::Camera;

/// What the host's display link should do after a [`Host::render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRequest {
    /// Nothing is owed: pause the link until the needs-frame callback.
    Idle,
    /// Something is still moving or loading: render again next vsync.
    Again,
    /// Nothing until `at` (a retry's backoff, a label fade's next step),
    /// unless the needs-frame callback fires first.
    At(Instant),
}

/// One feature property's value (the engine's `LayerValue`).
pub use crate::overlay::host::LayerValue as PropValue;

/// A point by stable id.
#[derive(Debug, Clone, PartialEq)]
pub struct HostPoint {
    pub id: String,
    pub lon: f64,
    pub lat: f64,
    pub props: Vec<(String, PropValue)>,
}

/// A line by stable id: one or more parts, each `[lon, lat]` positions.
#[derive(Debug, Clone, PartialEq)]
pub struct HostLine {
    pub id: String,
    pub parts: Vec<Vec<[f64; 2]>>,
    pub props: Vec<(String, PropValue)>,
}

/// A polygon by stable id: rings of `[lon, lat]`, exterior first, then holes.
#[derive(Debug, Clone, PartialEq)]
pub struct HostPolygon {
    pub id: String,
    pub rings: Vec<Vec<[f64; 2]>>,
    pub props: Vec<(String, PropValue)>,
}

/// How far from a satellite's dot a tap still picks it, points: a finger,
/// where the desktop's pointer gets `HIT_RADIUS_PX` (8).
pub const ALTITUDE_TAP_RADIUS_PT: f32 = 22.0;

/// The night side's brightness with the terminator on: the gallery's `T`
/// (`GPUI_MAP_NIGHT=0.25`, `ios/src/app.rs`'s `space_default`).
pub const TERMINATOR_NIGHT_BRIGHTNESS: f32 = 0.25;

/// The overlay feature a tap landed on: its layer and its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapHit {
    pub layer: String,
    pub id: String,
}

/// Why a host call was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum HostError {
    /// A layer holds one kind of feature; this call sent another.
    KindMismatch { layer: String, holds: &'static str, sent: &'static str },
    /// `set_style`'s paint JSON did not parse or compile.
    Paint { layer: String, reason: String },
    /// The engine refused the layer's features.
    Overlay { layer: String, reason: String },
    /// No basemap by that name.
    UnknownBasemap(String),
    /// The surface could not be created.
    Surface(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KindMismatch { layer, holds, sent } => {
                write!(f, "layer {layer:?} holds {holds}; remove or clear it before sending {sent}")
            }
            Self::Paint { layer, reason } => write!(f, "layer {layer:?}: paint refused: {reason}"),
            Self::Overlay { layer, reason } => write!(f, "layer {layer:?}: features refused: {reason}"),
            Self::UnknownBasemap(name) => write!(f, "no basemap named {name:?} (see Host::basemaps)"),
            Self::Surface(reason) => write!(f, "surface: {reason}"),
        }
    }
}

impl std::error::Error for HostError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Points,
    Lines,
    Polygons,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Points => "points",
            Self::Lines => "lines",
            Self::Polygons => "polygons",
        }
    }
}

/// One host layer: its features in a dense vector with an id index, so an
/// upsert is O(1) and a push walks one slice.
#[derive(Debug)]
struct Layer {
    kind: Option<Kind>,
    features: Vec<LayerFeature>,
    index: HashMap<String, usize>,
    /// The constant half of `set_style`: every feature's own paint.
    paint: FeaturePaint,
    /// The `text-*` half of `set_style`, on a point layer: its labels.
    text: Option<TextStyle>,
    /// The data-driven half of `set_style`, as declared on the source: kept
    /// so the declaration survives the source being emptied (an emptied
    /// layer removes its source; the next push re-declares this first).
    declared: serde_json::Map<String, serde_json::Value>,
    /// `set_style` ran since the last push: the next push rebuilds the
    /// source under `declared` instead of updating its paint in place.
    redeclare: bool,
    /// MapLibre's `minzoom` / `maxzoom`: drawn while `min <= z < max`.
    zoom_range: (f64, f64),
    /// Whether the camera's zoom was inside `zoom_range` at the last check.
    visible: bool,
    /// Changed since the last push.
    dirty: bool,
}

impl Layer {
    fn new() -> Self {
        Self {
            kind: None,
            features: Vec::new(),
            index: HashMap::new(),
            paint: FeaturePaint::default(),
            text: None,
            declared: serde_json::Map::new(),
            redeclare: false,
            zoom_range: (0.0, f64::INFINITY),
            visible: true,
            dirty: false,
        }
    }

    fn claim(&mut self, name: &str, kind: Kind) -> Result<(), HostError> {
        match self.kind {
            Some(holds) if holds != kind && !self.features.is_empty() => {
                Err(HostError::KindMismatch { layer: name.to_string(), holds: holds.name(), sent: kind.name() })
            }
            _ => {
                self.kind = Some(kind);
                Ok(())
            }
        }
    }

    fn upsert(&mut self, feature: LayerFeature) {
        match self.index.get(&feature.id) {
            Some(&at) => self.features[at] = feature,
            None => {
                self.index.insert(feature.id.clone(), self.features.len());
                self.features.push(feature);
            }
        }
        self.dirty = true;
    }

    fn remove(&mut self, id: &str) -> bool {
        let Some(at) = self.index.remove(id) else { return false };
        self.features.swap_remove(at);
        if let Some(moved) = self.features.get(at) {
            self.index.insert(moved.id.clone(), at);
        }
        self.dirty = true;
        true
    }
}

/// The embedded map. See the module doc.
pub struct Host {
    app: MapApp,
    layers: HashMap<String, Layer>,
    /// Layer names in creation order: the draw order (a later layer draws
    /// on top) and the pick order, reversed.
    order: Vec<String>,
    /// Set by the tile workers' wake (any thread); folded into the frame
    /// predicate at the next `render`.
    woken: Arc<AtomicBool>,
    needs_frame: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Held for the surface's lifetime.
    instance: Option<wgpu::Instance>,
    paused: bool,
    /// Which feature each label sort key (1-based) belongs to: `(layer, id)`.
    label_ids: Vec<(String, String)>,
    /// The camera as last reported by [`Self::take_camera_change`].
    reported_camera: Option<Camera>,
    /// The left and right bands an app floats over the map, in points
    /// (top and bottom ride in `SurfaceMetrics`). See [`Self::set_padding`].
    side_padding_pt: (f32, f32),
    /// Each arc / extrusion layer's ids, by input row or feature: what a
    /// pick reports (`None`: the index itself, as text).
    shell_ids: HashMap<String, Option<Arc<[String]>>>,
    /// The feature [`Self::set_highlight`] lights, if any.
    highlight: Option<Highlight>,
    /// Second vector sources over the basemap ([`Self::add_vector_overlay`]).
    vectors: super::vector_overlay::VectorOverlays,
}

/// One lit feature: which, and how it is drawn.
#[derive(Debug, Clone, PartialEq)]
struct Highlight {
    layer: String,
    id: String,
    style: crate::overlay::host::HighlightStyle,
}

/// The highlight's own layers, bottom to top: a dark casing, the outline
/// over it, and (a point) the point again on top. A NUL lead keeps them
/// apart from every app layer: never listed, picked or labelled.
const HIGHLIGHT_LAYERS: [&str; 3] = ["\u{0}highlight/casing", "\u{0}highlight/outline", "\u{0}highlight/top"];

fn is_highlight_layer(name: &str) -> bool {
    HIGHLIGHT_LAYERS.contains(&name)
}

impl Host {
    /// A map on OpenFreeMap's planet tiles at Singapore, zoom 13, with no
    /// surface yet. Compiles the first basemap style; spawns the tile
    /// workers (idle until the first frame asks for tiles).
    pub fn new() -> anyhow::Result<Host> {
        let mut app = MapApp::new()?;
        // Keyless, always: a build-time Mapbox token never reaches an
        // embedding app's tiles.
        if app.schema != TileSchema::Omt {
            app.schema = TileSchema::Omt;
            app.tile_url = TILE_URL.to_string();
            app.select_style(0);
        }
        app.manage_idle_timer = false;
        app.pick_on_tap = true;
        // A transit app's routes and stops read over the city, as on the
        // desktop transit app and Moovit: overlays draw above 3D buildings.
        app.overlays_above_buildings = true;
        // A passenger app's camera follows the passenger: no coordinate
        // ever reaches the log from an embedded map, debug logging or not.
        crate::log::set_positions(false);
        let woken = Arc::new(AtomicBool::new(false));
        Ok(Host {
            app,
            layers: HashMap::new(),
            order: Vec::new(),
            woken,
            needs_frame: None,
            instance: None,
            paused: false,
            reported_camera: None,
            label_ids: Vec::new(),
            side_padding_pt: (0.0, 0.0),
            shell_ids: HashMap::new(),
            highlight: None,
            vectors: Default::default(),
        })
    }

    /// Turns the engine's diagnostic lines (frame stats, fetch failures,
    /// gestures) on or off; off by default. They never carry a position:
    /// counts and timings only.
    pub fn set_debug_logging(&mut self, on: bool) {
        crate::log::set_enabled(on);
    }

    /// Registers the callback that tells the host a frame is owed while its
    /// display link is paused. Called on the main thread for a touch or an
    /// update, and from tile worker threads when a tile lands.
    pub fn set_needs_frame(&mut self, callback: Arc<dyn Fn() + Send + Sync>) {
        let woken = Arc::clone(&self.woken);
        let wake = Arc::clone(&callback);
        self.app.fetcher.set_wake(move || {
            woken.store(true, Ordering::Release);
            wake();
        });
        let woken = Arc::clone(&self.woken);
        let wake = Arc::clone(&callback);
        self.vectors.set_wake(Arc::new(move || {
            woken.store(true, Ordering::Release);
            wake();
        }));
        let hook = Arc::clone(&callback);
        self.app.redraw_hook = Some(Box::new(move || hook()));
        self.needs_frame = Some(callback);
    }

    /// Renders into `layer` from now on.
    ///
    /// # Safety
    ///
    /// `layer` must point at a live `CAMetalLayer` that outlives this host
    /// (or the next `attach_layer`/`detach`), and this must be called on the
    /// main thread. wgpu retains the layer itself.
    pub unsafe fn attach_layer(&mut self, layer: *mut std::ffi::c_void, metrics: SurfaceMetrics) -> Result<(), HostError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        // SAFETY: the caller's contract above.
        let surface = unsafe { instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer)) }
            .map_err(|e| HostError::Surface(e.to_string()))?;
        self.app.surface_metrics = metrics;
        let renderer = self.app.create_renderer(&instance, surface, None).map_err(|e| HostError::Surface(format!("{e:#}")))?;
        self.app.install_renderer(renderer);
        self.instance = Some(instance);
        self.request_frame();
        Ok(())
    }

    /// Drops the surface and every GPU resource; the CPU state (camera,
    /// layers, decoded tiles) stays. `attach_layer` again to draw.
    pub fn detach(&mut self) {
        self.app.renderer = None;
        self.app.overlays.drop_gpu();
        self.app.dots_gpu = crate::overlay::host::DotLayersGpu::default();
        self.app.gpu_tiles.clear();
        self.app.raster_tiles.clear();
        self.app.raster_squares.clear();
        self.app.raster_pipeline = None;
        self.instance = None;
    }

    /// The points-to-pixels ratio the label pass lays out with -- what the
    /// 1/3-size-labels bug left at 1.0. For tests.
    #[doc(hidden)]
    pub fn pixel_ratio(&self) -> f32 {
        self.app.dpr
    }

    /// Whether a surface is attached.
    pub fn is_attached(&self) -> bool {
        self.app.renderer.is_some()
    }

    /// The layer's new size and the safe-area bands the engine's chrome
    /// (compass, credits, logo) keeps clear of.
    pub fn resize(&mut self, metrics: SurfaceMetrics) {
        if metrics != self.app.surface_metrics {
            self.app.surface_metrics = metrics;
            self.app.resize_pending = true;
            // Without a surface, the camera's viewport and the chrome's insets
            // are all there is (a tap on the compass before the first frame).
            if self.app.renderer.is_none() {
                let scale = metrics.scale.max(0.1);
                self.app
                    .camera_control
                    .set_viewport((metrics.width_px as f64 / scale) as f32, (metrics.height_px as f64 / scale) as f32);
                self.app.top_inset_pt = metrics.top_inset_pt.max(0.0);
                self.app.bottom_inset_pt = metrics.bottom_inset_pt.max(0.0);
            }
            self.request_frame();
        }
    }

    /// Stops drawing (the app is going to the background): `render` answers
    /// `Idle` without touching the GPU until `resume`.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
        self.request_frame();
    }

    fn request_frame(&mut self) {
        self.app.request_redraw();
        if self.app.renderer.is_none() {
            // `request_redraw` only calls the hook with a surface; a host
            // that has not attached yet still wants to know.
            if let Some(callback) = self.needs_frame.as_ref() {
                callback();
            }
        }
    }

    /// One display-link tick: pushes changed layers, draws if anything owes
    /// a frame, and says when the next one is wanted.
    pub fn render(&mut self) -> FrameRequest {
        if self.woken.swap(false, Ordering::AcqRel) {
            self.app.needs_redraw = true;
        }
        self.apply_zoom_ranges();
        self.flush();
        if self.drive_vectors() {
            self.app.needs_redraw = true;
        }
        if self.paused || self.app.renderer.is_none() {
            return FrameRequest::Idle;
        }
        // A request the frame makes of itself is `Again`, not a callback:
        // the host is inside its tick and reads the answer.
        let hook = self.app.redraw_hook.take();
        let wake_at = self.app.pump();
        self.app.redraw_hook = hook;
        frame_request(self.app.needs_redraw, wake_at)
    }

    /// One touch, in points from the layer's top-left. `Some` when the touch
    /// ended a tap on an overlay feature.
    pub fn touch(&mut self, phase: TouchPhase, id: u64, x: f64, y: f64) -> Option<TapHit> {
        self.flush();
        self.app.on_touch(phase, id, x, y);
        self.request_frame();
        let (tx, ty) = self.app.tapped_at.take()?;
        self.feature_at(tx, ty)
    }

    /// The overlay feature under a point (in points), topmost layer first.
    ///
    /// Lines and polygons are picked by the engine (`ShellOverlays::
    /// feature_at`, the desktop's rule). Points are picked here, against the
    /// host's own positions: the nearest within the circle's radius plus a
    /// finger's slop, [`TAP_SLOP_PT`].
    pub fn feature_at(&mut self, x: f64, y: f64) -> Option<TapHit> {
        self.apply_zoom_ranges();
        self.flush();
        let camera = *self.app.camera_control.camera();
        // Altitude objects (satellites) draw over the map: they pick first,
        // as the last drawn frame placed them, within a finger's reach.
        if let Some((layer, index)) = self.app.altitude.hit_within(x as f32, y as f32, ALTITUDE_TAP_RADIUS_PT) {
            return Some(TapHit { layer, id: index.to_string() });
        }
        // Arcs draw over everything and extrusions over the flat overlays:
        // they pick first (`overlay::shell_layers`).
        if !self.app.shell_layers.is_empty() {
            let globe = crate::render::globe::globe_frame(&camera);
            let tolerance = crate::overlay::host::ARC_TAP_TOLERANCE_PX;
            if let Some(hit) = self.app.shell_layers.pick(&camera, &globe, x as f32, y as f32, tolerance) {
                let id = match self.shell_ids.get(&hit.layer).and_then(|ids| ids.as_ref()) {
                    Some(ids) => ids.get(hit.index).cloned(),
                    None => Some(hit.index.to_string()),
                };
                if let Some(id) = id {
                    return Some(TapHit { layer: hit.layer, id });
                }
            }
        }
        let engine_hit = self.app.overlays.feature_at(&camera, x as f32, y as f32);
        // A vector overlay's feature: the overlay and `"{source}#{index}"`.
        if let Some((layer, id)) = self.vectors.pick(&camera, x, y, engine_hit.as_ref()) {
            return Some(TapHit { layer, id });
        }
        // The highlight's outline over a line or polygon is the engine's
        // topmost hit there: it answers as the feature it lights.
        if engine_hit.as_ref().is_some_and(|hit| is_highlight_layer(&hit.source)) {
            if let Some(h) = self.highlight.as_ref() {
                return Some(TapHit { layer: h.layer.clone(), id: h.id.clone() });
            }
        }
        // Topmost first: the engine's draw order, reversed. The highlight
        // draws over its feature but is not a feature itself.
        for name in self.order.iter().rev().filter(|n| !is_highlight_layer(n)) {
            if let Some(hit) = engine_hit.as_ref().filter(|hit| &*hit.source == name.as_str()) {
                let id = match hit.id.clone()? {
                    FeatureId::String(id) => id,
                    FeatureId::Number(n) => format!("{n}"),
                };
                return Some(TapHit { layer: name.to_string(), id });
            }
            let Some(layer) = self.layers.get(name) else { continue };
            if layer.kind != Some(Kind::Points) || !layer.visible {
                continue;
            }
            let reach = f64::from(layer.paint.circle_radius) + TAP_SLOP_PT;
            let nearest = layer
                .features
                .iter()
                .filter_map(|f| match f.geometry {
                    LayerGeometry::Point([lon, lat]) => {
                        let (sx, sy, _) = camera.project_lon_lat_alt(lon, lat, 0.0)?;
                        let d = (f64::from(sx) - x).hypot(f64::from(sy) - y);
                        (d <= reach).then_some((d, &f.id))
                    }
                    _ => None,
                })
                .min_by(|a, b| a.0.total_cmp(&b.0));
            if let Some((_, id)) = nearest {
                return Some(TapHit { layer: name.to_string(), id: id.clone() });
            }
        }
        None
    }

    // -- camera -------------------------------------------------------------

    pub fn camera(&self) -> Camera {
        *self.app.camera_control.camera()
    }

    /// The camera, when it has changed since the last call: what a host's
    /// `on_camera_changed` reports after a frame.
    pub fn take_camera_change(&mut self) -> Option<Camera> {
        let now = self.camera();
        let changed = self.reported_camera.is_none_or(|was| {
            was.center_lon != now.center_lon
                || was.center_lat != now.center_lat
                || was.zoom != now.zoom
                || was.bearing != now.bearing
                || was.pitch != now.pitch
                || was.viewport_width_px != now.viewport_width_px
                || was.viewport_height_px != now.viewport_height_px
        });
        self.reported_camera = Some(now);
        changed.then_some(now)
    }

    /// The bands of the view an app covers (a bottom sheet, a search bar),
    /// in points. Cheap enough for every frame of a sheet drag: it moves the
    /// engine's chrome (compass, credits, logo) clear of the top and bottom
    /// bands at the next frame, and every later `jump_to`/`ease_to` lands
    /// its centre in the middle of the uncovered area. The camera does not
    /// move by itself when the padding changes.
    pub fn set_padding(&mut self, top: f32, bottom: f32, left: f32, right: f32) {
        self.side_padding_pt = (left.max(0.0), right.max(0.0));
        let metrics = SurfaceMetrics { top_inset_pt: top.max(0.0), bottom_inset_pt: bottom.max(0.0), ..self.app.surface_metrics };
        self.resize(metrics);
    }

    /// Where a camera move's centre lands: the middle of the uncovered area,
    /// unless the target names its own offset.
    fn padded(&self, mut target: CameraTarget) -> CameraTarget {
        if target.anchor_offset_px == (0.0, 0.0) {
            let m = self.app.surface_metrics;
            let (left, right) = self.side_padding_pt;
            target.anchor_offset_px = ((left - right) / 2.0, (m.top_inset_pt - m.bottom_inset_pt) / 2.0);
        }
        target
    }

    /// Frames the box `west..east`, `south..north` (degrees) in the area the
    /// padding leaves uncovered, `margin_pt` inside its edges, north up and
    /// flat, over `duration` (zero jumps). A box smaller than a street --
    /// one stop -- stops at `max_zoom`.
    pub fn fit_bounds(&mut self, bounds: [f64; 4], margin_pt: f64, max_zoom: f64, duration: Duration) {
        let camera = self.camera();
        let m = self.app.surface_metrics;
        let (left, right) = self.side_padding_pt;
        let avail_w = f64::from(camera.viewport_width_px) - f64::from(left + right) - 2.0 * margin_pt;
        let avail_h = f64::from(camera.viewport_height_px) - f64::from(m.top_inset_pt + m.bottom_inset_pt) - 2.0 * margin_pt;
        let target = fit_target(bounds, avail_w.max(1.0), avail_h.max(1.0), max_zoom);
        if duration.is_zero() {
            self.jump_to(target);
        } else {
            self.ease_to(target, duration);
        }
    }

    pub fn jump_to(&mut self, target: CameraTarget) {
        self.app.pan_inertia = None;
        let target = self.padded(target);
        self.app.camera_control.jump_to(target);
        self.request_frame();
    }

    pub fn ease_to(&mut self, target: CameraTarget, duration: Duration) {
        self.app.pan_inertia = None;
        let target = self.padded(target);
        self.app.camera_control.ease_to(target, duration);
        self.request_frame();
    }

    // -- turn-by-turn navigation --------------------------------------------
    //
    // The demo shell's `NavHost`, driven by the embedding app: the engine
    // draws the route line, the puck and the maneuver arrow, and decides
    // everything (the simulated drive, the follow camera, the voice); with
    // `nav_set_hud_by_host(true)` the app draws the banner and the ETA strip
    // natively, from `nav_state`.

    /// The bundled demo drives a host may load, by name.
    #[must_use]
    pub fn nav_routes() -> Vec<&'static str> {
        super::DEMO_ROUTES.iter().map(|(name, _)| *name).collect()
    }

    /// Loads demo route `name` for the next simulated drive, with `options`
    /// (speed, a start part-way along). `false` for an unknown name.
    pub fn nav_set_route(&mut self, name: &str, options: crate::nav::host::SimOptions) -> bool {
        let Some((_, bytes)) = super::DEMO_ROUTES.iter().find(|(known, _)| *known == name) else {
            return false;
        };
        self.app.nav.set_demo_route(Arc::from(*bytes), options);
        true
    }

    /// See `NavHost::set_hud_by_host`.
    pub fn nav_set_hud_by_host(&mut self, on: bool) {
        self.app.nav.set_hud_by_host(on);
        self.request_frame();
    }

    fn nav_frame(&self) -> crate::nav::host::NavFrame {
        super::nav_frame_of(self.app.now_ms(), self.app.camera_control.camera(), self.app.top_inset_pt, self.app.bottom_inset_pt)
    }

    /// Starts the loaded demo route as a simulated drive (replacing any trip).
    pub fn nav_start(&mut self) -> Result<(), String> {
        self.app.pan_inertia = None;
        let frame = self.nav_frame();
        let result = self.app.nav.start_simulation(&self.app.camera_control, &frame);
        self.request_frame();
        result
    }

    /// Ends the trip: the route, the puck and the voice go.
    pub fn nav_end(&mut self) {
        self.app.nav.end_trip();
        self.request_frame();
    }

    pub fn nav_toggle_pause(&mut self) {
        // A stopped drive's Play starts it again, as the engine's chrome does.
        if self.app.nav.drive_stopped() {
            let _ = self.nav_start();
            return;
        }
        self.app.nav.toggle_pause();
        self.request_frame();
    }

    pub fn nav_cycle_speed(&mut self) {
        self.app.nav.cycle_speed();
        self.request_frame();
    }

    pub fn nav_set_voice_muted(&mut self, muted: bool) {
        self.app.nav.set_voice_muted(muted);
        self.request_frame();
    }

    /// How the camera frames the trip; also Recenter (the mode again takes
    /// the camera back from a gesture).
    pub fn nav_set_camera_mode(&mut self, mode: crate::nav::host::NavCameraMode) {
        self.app.pan_inertia = None;
        let frame = self.nav_frame();
        self.app.nav.set_nav_camera_mode(mode, &self.app.camera_control, &frame);
        self.request_frame();
    }

    #[must_use]
    pub fn nav_camera_mode(&self) -> crate::nav::host::NavCameraMode {
        self.app.nav.nav_camera_mode()
    }

    #[must_use]
    pub fn nav_follow_zoom_offset(&self) -> f64 {
        self.app.nav.nav_follow_zoom_offset()
    }

    /// The app's own controls over the map, in points (see
    /// `NavHost::set_host_chrome`): the follow puck rides clear of them.
    pub fn nav_set_host_chrome(&mut self, rects: Vec<crate::nav::host::Rect>) {
        if self.app.nav.host_chrome() != rects.as_slice() {
            self.app.nav.set_host_chrome(rects);
            self.request_frame();
        }
    }

    #[must_use]
    pub fn nav_state(&self) -> Option<crate::nav::host::NavigationState> {
        self.app.nav.state().cloned()
    }

    #[must_use]
    pub fn nav_notice(&self) -> Option<String> {
        self.app.nav.notice().map(str::to_string)
    }

    #[must_use]
    pub fn nav_trip_active(&self) -> bool {
        self.app.nav.trip_active()
    }

    #[must_use]
    pub fn nav_voice_muted(&self) -> bool {
        self.app.nav.voice_muted()
    }

    /// The puck on screen, points.
    #[must_use]
    pub fn nav_puck_on_screen(&self) -> Option<(f32, f32)> {
        self.app.nav.puck_on_screen(self.app.camera_control.camera())
    }

    /// The route line's mesh is on the GPU (a frame since the trip began).
    #[must_use]
    pub fn nav_route_drawn(&self) -> bool {
        self.app.renderer.as_ref().is_some_and(|r| r.nav_route.as_ref().is_some_and(|route| route.draw().is_some()))
    }

    /// Voice prompts handed out so far, said aloud or not.
    #[must_use]
    pub fn nav_prompts(&self) -> u64 {
        self.app.nav_prompts
    }

    /// `false`: count prompts, never say them (a host's tests on a Mac).
    pub fn nav_set_speaker_on(&mut self, on: bool) {
        self.app.speaker_on = on;
    }

    /// The map is off screen (another page covers it): a simulated drive
    /// pauses and the voice stops (`NavHost::set_suspended`).
    pub fn nav_set_suspended(&mut self, suspended: bool) {
        self.app.nav.set_suspended(suspended);
        self.request_frame();
    }

    /// Moves the engine's clock `ms` forward: a host test marches a
    /// simulated drive on without waiting it out. Never for an app.
    #[doc(hidden)]
    pub fn advance_clock_ms(&mut self, ms: f64) {
        self.app.clock_skew_ms += ms.max(0.0);
        self.request_frame();
    }

    // -- basemap ------------------------------------------------------------

    /// The basemap styles this build carries, by name.
    pub fn basemaps(&self) -> Vec<String> {
        self.app.schema.styles().iter().map(|s| s.name.to_string()).collect()
    }

    pub fn basemap(&self) -> String {
        self.app.schema.styles()[self.app.style_index].name.to_string()
    }

    pub fn set_basemap(&mut self, name: &str) -> Result<(), HostError> {
        let index = self
            .app
            .schema
            .styles()
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| HostError::UnknownBasemap(name.to_string()))?;
        if index != self.app.style_index && !self.app.select_style(index) {
            return Err(HostError::UnknownBasemap(name.to_string()));
        }
        self.request_frame();
        Ok(())
    }

    /// The basemap's tiles from the host's own OpenMapTiles archive (an
    /// offline region: `file://<path>`, an absolute path or a URL), or
    /// `None` for the schema's online tiles. See `MapApp::set_basemap_archive`.
    pub fn set_basemap_archive(&mut self, archive: Option<super::BasemapArchive>) {
        self.app.set_basemap_archive(archive);
        self.request_frame();
    }

    /// The host's imagery source, drawn under the basemap style's raster
    /// layers (`MapApp::set_raster_override`); `None` for the style's own.
    pub fn set_raster_source(&mut self, raster: Option<super::RasterConfig>) {
        self.app.set_raster_override(raster);
        self.request_frame();
    }

    /// Every credit the map on screen owes -- the same list the engine draws
    /// in its bottom credit plate.
    pub fn attributions(&self) -> Vec<String> {
        super::ios_credits(self.app.schema, self.app.style_source, self.app.active_archive(), &self.app.raster, &self.app.dots, &self.app.extra_credits, self.app.data_credit.as_deref()).into_iter().map(|c| c.text).collect()
    }

    // -- vector overlays ----------------------------------------------------

    /// Adds (or replaces) a second vector-tile source over the basemap,
    /// drawn with its own style layers -- MapLibre's `addSource({type:
    /// "vector"})` plus an `addLayer` per entry of `style_layers` (a JSON
    /// array of `fill`, `line` and `circle` layers; their `source-layer`s
    /// name layers of the tiles). `url` is `pmtiles+<archive URL>`,
    /// `pmtiles+file://<path>` or an XYZ `{z}/{x}/{y}` template of MVT
    /// tiles. Tiles are fetched and decoded off the main thread for the
    /// view; see `vector_overlay`'s module doc. A tap on a feature answers
    /// `name` and an id [`Self::vector_feature_properties`] reads.
    pub fn add_vector_overlay(&mut self, name: &str, url: &str, style_layers: &str, options: super::vector_overlay::VectorOverlayOptions) -> Result<(), HostError> {
        let layers: serde_json::Value = serde_json::from_str(style_layers).map_err(|e| HostError::Paint { layer: name.to_string(), reason: e.to_string() })?;
        self.vectors.add(&mut self.app.overlays, name, url, &layers, options).map_err(|reason| HostError::Paint { layer: name.to_string(), reason })?;
        self.sync_vector_credits();
        self.request_frame();
        Ok(())
    }

    /// Removes vector overlay `name` and everything it drew; whether there
    /// was one.
    pub fn remove_vector_overlay(&mut self, name: &str) -> bool {
        let removed = self.vectors.remove(&mut self.app.overlays, name);
        if removed {
            self.sync_vector_credits();
            self.request_frame();
        }
        removed
    }

    /// Draws the vector overlays in `names`' order, bottom first.
    pub fn set_vector_overlay_order(&mut self, names: &[&str]) {
        self.vectors.set_order(&mut self.app.overlays, names);
        self.request_frame();
    }

    /// The vector overlays, in draw order (the last draws on top).
    pub fn vector_overlays(&self) -> Vec<String> {
        self.vectors.names()
    }

    pub fn vector_overlay_stats(&self, name: &str) -> Option<super::vector_overlay::VectorOverlayStats> {
        self.vectors.stats(name)
    }

    /// Whether every tile overlay `name` wants for the view has landed and
    /// been drawn.
    pub fn vector_overlay_settled(&self, name: &str) -> bool {
        self.vectors.settled(name, self.app.display_zoom())
    }

    /// Features drawn per engine source of overlay `name`, by source.
    pub fn vector_overlay_counts(&self, name: &str) -> Vec<(String, usize)> {
        self.vectors.pushed_counts(name)
    }

    /// A tapped overlay feature's own MVT properties, sorted by key.
    pub fn vector_feature_properties(&self, name: &str, id: &str) -> Option<Vec<(String, PropValue)>> {
        let props = self.vectors.properties(name, id)?;
        Some(
            props
                .into_iter()
                .map(|(k, v)| {
                    let v = match v {
                        crate::overlay::host::TagValue::Number(n) => PropValue::Number(n),
                        crate::overlay::host::TagValue::String(s) => PropValue::Text(s),
                        crate::overlay::host::TagValue::Bool(b) => PropValue::Bool(b),
                    };
                    (k, v)
                })
                .collect(),
        )
    }

    /// One frame's overlay work: `true` when a frame is owed.
    fn drive_vectors(&mut self) -> bool {
        if self.vectors.is_empty() {
            return false;
        }
        let camera = *self.app.camera_control.camera();
        let bucket = self.app.display_zoom();
        self.vectors.frame(&mut self.app.overlays, &camera, bucket)
    }

    fn sync_vector_credits(&mut self) {
        self.app.extra_credits = self.vectors.credits();
    }

    /// The host draws the credits (every one [`Self::attributions`] lists,
    /// behind its own ⓘ): the engine stops drawing its credit plate. The
    /// SWAT mark stays.
    pub fn set_credits_by_host(&mut self, on: bool) {
        if self.app.credits_by_host != on {
            self.app.credits_by_host = on;
            self.request_frame();
        }
    }

    // -- overlays -----------------------------------------------------------

    pub fn upsert_points(&mut self, layer: &str, points: Vec<HostPoint>) -> Result<(), HostError> {
        let entry = self.layer_mut(layer);
        entry.claim(layer, Kind::Points)?;
        for p in points {
            entry.upsert(LayerFeature { id: p.id, geometry: LayerGeometry::Point([p.lon, p.lat]), props: p.props });
        }
        self.request_frame();
        Ok(())
    }

    pub fn upsert_lines(&mut self, layer: &str, lines: Vec<HostLine>) -> Result<(), HostError> {
        let entry = self.layer_mut(layer);
        entry.claim(layer, Kind::Lines)?;
        for l in lines {
            entry.upsert(LayerFeature { id: l.id, geometry: LayerGeometry::Line(l.parts), props: l.props });
        }
        self.request_frame();
        Ok(())
    }

    pub fn upsert_polygons(&mut self, layer: &str, polygons: Vec<HostPolygon>) -> Result<(), HostError> {
        let entry = self.layer_mut(layer);
        entry.claim(layer, Kind::Polygons)?;
        for p in polygons {
            entry.upsert(LayerFeature { id: p.id, geometry: LayerGeometry::Polygon(p.rings), props: p.props });
        }
        self.request_frame();
        Ok(())
    }

    /// Removes `ids` from `layer`; how many were there.
    pub fn remove(&mut self, layer: &str, ids: &[String]) -> usize {
        let Some(entry) = self.layers.get_mut(layer) else { return 0 };
        let removed = ids.iter().filter(|id| entry.remove(id)).count();
        if removed > 0 {
            self.request_frame();
        }
        removed
    }

    // -- arcs, extrusions, trips and heatmaps --------------------------------

    /// Draws `set` as arc layer `layer` (replacing it), in `style`. `ids`
    /// names each INPUT row (the order `overlay::arcs::build` took them):
    /// what a tap on the arc reports; `None` reports the row number.
    pub fn set_arc_layer(&mut self, layer: &str, set: Arc<crate::render::arcs::ArcSet>, style: crate::render::arcs::ArcStyle, ids: Option<Arc<[String]>>) {
        self.claim_shell(layer);
        self.app.shell_layers.set_arc_layer(layer, set, style);
        self.shell_ids.insert(layer.to_string(), ids);
        self.request_frame();
    }

    /// Arc layer `layer`'s opacity, height and blending: no re-upload.
    pub fn set_arc_style(&mut self, layer: &str, style: crate::render::arcs::ArcStyle) -> bool {
        let found = self.app.shell_layers.set_arc_style(layer, style);
        self.request_frame();
        found
    }

    /// Shows only the arcs whose filter value is in `range`, or all.
    pub fn set_arc_filter_range(&mut self, layer: &str, range: Option<[f64; 2]>) -> bool {
        let found = self.app.shell_layers.set_arc_filter_range(layer, range);
        self.request_frame();
        found
    }

    /// Draws `set` as an extruded layer (3D hexbin columns, 3D polygons).
    /// `ids` names each feature in the order `extrude` took them.
    pub fn set_extrusion_layer(&mut self, layer: &str, set: Arc<crate::overlay::host::OverlayExtrusion>, style: crate::overlay::host::ExtrusionStyle, ids: Option<Arc<[String]>>) {
        self.claim_shell(layer);
        self.app.shell_layers.set_extrusion_layer(layer, set, style);
        self.shell_ids.insert(layer.to_string(), ids);
        self.request_frame();
    }

    /// Draws `set` as an animated trip layer; paths whole until
    /// [`Self::set_trip_clock`].
    pub fn set_trip_layer(&mut self, layer: &str, set: Arc<crate::render::trips::TripSet>, style: crate::render::trips::TripStyle) {
        self.claim_shell(layer);
        self.app.shell_layers.set_trip_layer(layer, set, style);
        self.shell_ids.insert(layer.to_string(), None);
        self.request_frame();
    }

    /// Trip layer `layer`'s clock, in its set's time unit (`None`: whole
    /// paths). A uniform write: cheap enough for every display-link tick.
    pub fn set_trip_clock(&mut self, layer: &str, clock: Option<f64>) -> bool {
        let found = self.app.shell_layers.set_trip_clock(layer, clock);
        self.request_frame();
        found
    }

    /// Draws `set` as heatmap layer `layer`.
    pub fn set_heatmap_layer(&mut self, layer: &str, set: Arc<crate::render::heatmap::HeatmapSet>, style: crate::render::heatmap::HeatmapStyle) {
        self.claim_shell(layer);
        self.app.shell_layers.set_heatmap_layer(layer, set, style);
        self.shell_ids.insert(layer.to_string(), None);
        self.request_frame();
    }

    /// Heatmap layer `layer`'s filter windows, one per filter column.
    pub fn set_heatmap_filter_ranges(&mut self, layer: &str, windows: &[Option<[f64; 2]>]) -> bool {
        let found = self.app.shell_layers.set_heatmap_filter_ranges(layer, windows);
        self.request_frame();
        found
    }

    /// The arc / extrusion / trip / heatmap layers, for inspection.
    pub fn shell_layers(&self) -> &crate::overlay::host::ShellLayers {
        &self.app.shell_layers
    }

    /// What the last frame drew of those layers, and the bytes they hold.
    pub fn shell_layer_stats(&self) -> crate::overlay::host::ShellLayerStats {
        self.app.shell_layers.stats()
    }

    /// A layer name is one layer: a points/lines/polygons layer of the same
    /// name is emptied when a 3D layer takes it.
    fn claim_shell(&mut self, layer: &str) {
        if let Some(entry) = self.layers.get_mut(layer) {
            if !entry.features.is_empty() {
                entry.features.clear();
                entry.index.clear();
                entry.kind = None;
                entry.dirty = true;
            }
        }
    }

    /// Reads the next drawn frame back: `(width, height, RGBA8)` in
    /// physical pixels. For tests; turns the surface's copy usage on (once).
    pub fn capture_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        {
            let renderer = self.app.renderer.as_mut()?;
            if !renderer.surface_config.usage.contains(wgpu::TextureUsages::COPY_SRC) {
                renderer.surface_config.usage |= wgpu::TextureUsages::COPY_SRC;
                renderer.surface.configure(&renderer.ctx.device, &renderer.surface_config);
            }
        }
        self.app.captured = None;
        for _ in 0..4 {
            self.app.capture_requested = true;
            self.app.needs_redraw = true;
            self.render();
            if self.app.captured.is_some() {
                break;
            }
        }
        self.app.capture_requested = false;
        self.app.captured.take()
    }

    /// Empties `layer` (its style stays). An arc, extrusion, trip or
    /// heatmap layer of that name is removed.
    pub fn clear(&mut self, layer: &str) {
        self.remove_vector_overlay(layer);
        if self.app.shell_layers.remove(layer) {
            self.shell_ids.remove(layer);
            self.request_frame();
        }
        if let Some(entry) = self.layers.get_mut(layer) {
            entry.features.clear();
            entry.index.clear();
            entry.kind = None;
            entry.dirty = true;
            self.request_frame();
        }
    }

    /// The ids `layer` holds, in no particular order.
    pub fn ids(&self, layer: &str) -> Vec<String> {
        self.layers.get(layer).map(|l| l.features.iter().map(|f| f.id.clone()).collect()).unwrap_or_default()
    }

    /// Styles `layer` with a style-spec paint object. Constant values
    /// (`"line-width": 4`, `"circle-color": "#e53935"`, `"fill-opacity"` ...)
    /// become every feature's own paint; expressions (`"circle-color":
    /// ["get", "color"]`) are declared on the layer's source and read the
    /// features' `props`. Replaces the layer's previous style.
    pub fn set_style(&mut self, layer: &str, paint_json: &str) -> Result<(), HostError> {
        let bad = |reason: String| HostError::Paint { layer: layer.to_string(), reason };
        let value: serde_json::Value = serde_json::from_str(paint_json).map_err(|e| bad(e.to_string()))?;
        let object = value.as_object().ok_or_else(|| bad("not a JSON object".into()))?;
        // `text-*` is the label half, resolved here per feature (the
        // engine's data-driven overlay paint covers colours and radii only).
        // `min-zoom` / `max-zoom` (and MapLibre's `minzoom` / `maxzoom`):
        // the layer's own, for every kind, applied by the host.
        let zoom_key = |k: &str| matches!(k, "min-zoom" | "minzoom" | "max-zoom" | "maxzoom");
        let mut zoom_range = (0.0, f64::INFINITY);
        for (key, value) in object.iter().filter(|(k, _)| zoom_key(k)) {
            let z = value.as_f64().filter(|z| (0.0..=30.0).contains(z)).ok_or_else(|| bad(format!("{key}: a zoom 0..30, got {value}")))?;
            if key.starts_with("min") {
                zoom_range.0 = z;
            } else {
                zoom_range.1 = z;
            }
        }
        let (text_keys, paint_keys): (serde_json::Map<_, _>, serde_json::Map<_, _>) = object
            .iter()
            .filter(|(k, _)| !zoom_key(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .partition(|(k, _)| k.starts_with("text-"));
        let text = TextStyle::compile(&text_keys).map_err(bad)?;
        let (expressions, constants): (serde_json::Map<_, _>, serde_json::Map<_, _>) =
            // An expression is an array led by its operator name; an array of
            // numbers (`line-dasharray`) is a plain value.
            paint_keys.into_iter().partition(|(_, v)| v.as_array().and_then(|a| a.first()).is_some_and(serde_json::Value::is_string));
        let mut paint = FeaturePaint::from_properties(&constants).map_err(bad)?;
        // MapLibre's model: a style of only `text-*`/`icon-*` keys is a
        // symbol layer and draws no circle; a circle needs a `circle-*` key
        // (or no style at all -- a bare point layer keeps the default dot).
        let symbol_only = !object.keys().any(|k| k.starts_with("circle-"))
            && object.keys().any(|k| k.starts_with("text-") || k.starts_with("icon-"));
        if symbol_only {
            paint.circle_radius = 0.0;
            paint.circle_color.a = 0.0;
        }
        // Checked now, against a scratch source, so a bad expression is
        // refused here; applied at the layer's next push (`flush`).
        const SCRATCH: &str = "\u{0}embed-paint-check";
        let checked = self.app.overlays.declare_source_paint(SCRATCH, &expressions);
        self.app.overlays.remove_source(SCRATCH);
        checked.map_err(|e| bad(e.to_string()))?;
        let entry = self.layer_mut(layer);
        entry.paint = paint;
        entry.text = text;
        entry.declared = expressions;
        // Always re-applied, even for identical JSON: the next push
        // rebuilds the source with this paint.
        entry.redeclare = true;
        entry.zoom_range = zoom_range;
        entry.dirty = true;
        self.request_frame();
        Ok(())
    }

    /// Every labelled point layer's labels, as z14-tile-anchored symbol
    /// plans for the shared label pass: layers in draw order (a later layer
    /// wins a contested spot), features in push order within one.
    fn label_plans(&mut self) -> Vec<(TileId, Arc<crate::style::SymbolPlan>)> {
        let mut by_tile: HashMap<TileId, Vec<crate::style::SymbolFeature>> = HashMap::new();
        let mut sort_key = 0.0f32;
        self.label_ids.clear();
        for name in self.order.iter().rev() {
            let Some(layer) = self.layers.get(name) else { continue };
            let Some(text) = layer.text.as_ref().filter(|_| layer.kind == Some(Kind::Points) && layer.visible) else {
                continue;
            };
            for feature in &layer.features {
                let LayerGeometry::Point([lon, lat]) = feature.geometry else { continue };
                let Some((tile, anchor)) = label_anchor(lon, lat) else { continue };
                let Some(symbol) = text.symbol(&feature.props, sort_key + 1.0) else { continue };
                sort_key += 1.0;
                self.label_ids.push((name.clone(), feature.id.clone()));
                by_tile.entry(tile).or_default().push(crate::style::SymbolFeature { anchor, ..symbol });
            }
        }
        by_tile.into_iter().map(|(tile, features)| (tile, Arc::new(crate::style::SymbolPlan { features }))).collect()
    }

    /// Whether `layer`'s points draw a circle (`set_style`'s symbol-layer
    /// rule). For tests.
    #[doc(hidden)]
    pub fn draws_circles(&self, layer: &str) -> bool {
        self.layers.get(layer).is_none_or(|l| l.paint.circle_radius > 0.0 && l.paint.circle_color.a > 0.0)
    }

    /// The labels the label pass would place on this frame, without a GPU:
    /// `(layer, id, text box in PHYSICAL pixels)`, after collision. The
    /// same `collect_candidates` and `place_visible_symbols` the frame runs,
    /// over the host's labels only (no basemap tiles). For tests.
    #[doc(hidden)]
    pub fn place_labels_headless(&mut self) -> Vec<(String, String, crate::collision::CollisionBox)> {
        self.apply_zoom_ranges();
        self.flush();
        let camera = self.camera();
        let scale = (self.app.surface_metrics.scale as f32).clamp(0.5, 8.0);
        let mut db = glyphon::fontdb::Database::new();
        db.load_font_data(super::EMBEDDED_FONT.to_vec());
        crate::system_fonts::load(&mut db, "en-US");
        let mut font_system = crate::font_fallback::new_font_system_from_db("en-US".to_string(), db);
        let mut shape_cache = crate::symbol_pass::LabelShapeCache::with_default_family(super::FONT_FAMILY);
        let globe_frame = crate::render::globe::globe_frame(&camera);
        let icon_rects = HashMap::new();
        let frame = crate::symbol_pass::SymbolFrame {
            occlusion: None,
            label_paint: None,
            camera: &camera,
            style_layers: &[],
            scale_factor: scale,
            size_scale: scale,
            collision_padding: crate::symbol_pass::SYMBOL_COLLISION_PADDING_LOGICAL_PX * scale,
            size_blend_t: camera.zoom.fract() as f32,
            icon_rects: &icon_rects,
            icon_atlas_size: (0, 0),
            globe: crate::render::globe::GlobeAnchors::new(&globe_frame),
            light: crate::render::SymbolLight::UNLIT,
            fog: crate::render::HorizonFog::NONE,
        };
        let mut candidates = Vec::new();
        let mut bypassed = Vec::new();
        crate::symbol_pass::collect_candidates(
            &frame,
            self.app.overlay_label_plans.iter().map(|(tile, plan)| (*tile, &**plan)),
            &mut font_system,
            &mut shape_cache,
            None,
            &mut candidates,
            &mut bypassed,
        );
        let mut index = crate::collision::CollisionIndex::new(crate::symbol_pass::symbol_collision_bounds(
            camera.viewport_width_px,
            camera.viewport_height_px,
            scale,
        ));
        crate::symbol_pass::place_visible_symbols(candidates, &mut index)
            .into_iter()
            .chain(bypassed)
            .filter_map(|symbol| {
                let (layer, id) = self.label_ids.get((symbol.order.1 as usize).checked_sub(1)?)?.clone();
                Some((layer, id, symbol.text_box?))
            })
            .collect()
    }

    /// Keeps tile bodies under `dir` (the app's `Library/Caches`): read
    /// before the network, bounded at `max_bytes`, refetched after a week.
    /// An empty path turns the cache off.
    pub fn set_cache_dir(&mut self, dir: &str, max_bytes: u64) -> Result<(), HostError> {
        if dir.is_empty() {
            self.app.fetcher.set_disk_cache(None);
            return Ok(());
        }
        let path = std::path::Path::new(dir).join("gpui-maps-tiles-v1");
        let cache = crate::disk_cache::DiskCache::open(&path, max_bytes, crate::disk_cache::DEFAULT_MAX_AGE)
            .map_err(|e| HostError::Surface(format!("tile cache: {e}")))?;
        self.app.fetcher.set_disk_cache(Some(Arc::new(cache)));
        Ok(())
    }

    /// The host's layout direction. Right to left mirrors the engine chrome
    /// that has a side -- the compass moves to the top-left -- and keeps the
    /// SWAT mark bottom-left and the credits bottom-right (their text shapes
    /// right to left by itself).
    pub fn set_layout_direction(&mut self, rtl: bool) {
        if self.app.chrome_rtl != rtl {
            self.app.chrome_rtl = rtl;
            self.request_frame();
        }
    }

    /// The basemap's 3D buildings on (the style's `fill-extrusion`) or off
    /// (their footprints, flat at the ground in the same colour). A
    /// render-time switch: the style is not recompiled, nothing re-bakes.
    /// Labels over data: `true` while the host draws data fills under the
    /// basemap labels, so every label takes the on-data paint
    /// (`style::LabelPaint::ON_DATA`) instead of its style's. A render-time
    /// switch; the style is not recompiled.
    pub fn set_labels_on_data(&mut self, on: bool) {
        let paint = on.then_some(crate::style::LabelPaint::ON_DATA);
        if self.app.label_paint != paint {
            self.app.label_paint = paint;
            self.request_frame();
        }
    }

    // -- objects above the globe (satellites) ---------------------------------

    /// Draws `positions` as altitude layer `id` over the globe -- native's
    /// `MapView::set_altitude_points`. Replaces the layer if it exists.
    pub fn set_altitude_points(
        &mut self,
        id: &str,
        positions: crate::render::altitude_layers::AltitudePositions,
        style: crate::render::altitude_pipeline::AltitudeStyle,
    ) {
        self.app.altitude.set(id, positions, style);
        self.request_frame();
    }

    /// Restyles altitude layer `id`, keeping its positions.
    pub fn set_altitude_style(&mut self, id: &str, style: crate::render::altitude_pipeline::AltitudeStyle) {
        if self.app.altitude.set_style(id, style) {
            self.request_frame();
        }
    }

    /// Shows object `i` of layer `id` only where `mask[i]`.
    pub fn set_altitude_visibility(&mut self, id: &str, mask: &[bool]) {
        if self.app.altitude.set_visibility(id, mask) {
            self.request_frame();
        }
    }

    /// Emphasises up to three objects of layer `id`: hovered, selected, halo.
    pub fn set_altitude_emphasis(&mut self, id: &str, hovered: Option<usize>, selected: Option<usize>, halo: Option<usize>) {
        if self.app.altitude.set_emphasis(id, (hovered, selected, halo)) {
            self.request_frame();
        }
    }

    /// Draws `lines` (orbit rings) as altitude line layer `id` -- native's
    /// `MapView::set_altitude_lines`; an empty list clears it.
    pub fn set_altitude_lines(
        &mut self,
        id: &str,
        frame: crate::render::altitude::RefFrame,
        width_px: f32,
        lines: &[crate::render::altitude::AltitudeLine],
    ) {
        self.app.altitude.set_lines(id, frame, width_px, lines);
        self.request_frame();
    }

    /// Line layer `id`'s look (`Glow`: a bright core in a soft falloff).
    pub fn set_altitude_lines_look(&mut self, id: &str, look: crate::render::altitude_pipeline::AltitudeLook) {
        self.app.altitude.set_line_look(id, look);
        self.request_frame();
    }

    /// Stops drawing altitude layer `id`.
    pub fn remove_altitude(&mut self, id: &str) {
        if self.app.altitude.remove(id) {
            self.request_frame();
        }
    }

    /// The scene clock the altitude layers show (`None`: real time).
    pub fn set_scene_clock(&mut self, clock: Option<crate::render::altitude_layers::SceneClock>) {
        self.app.scene_clock = clock;
        self.request_frame();
    }

    pub fn scene_clock(&self) -> Option<crate::render::altitude_layers::SceneClock> {
        self.app.scene_clock
    }

    /// The scene time now, Unix seconds.
    pub fn scene_time(&self) -> f64 {
        self.app.scene_time()
    }

    /// The altitude object under `(x, y)` (points), within a finger's reach.
    pub fn altitude_point_at(&self, x: f64, y: f64) -> Option<(String, usize)> {
        self.app.altitude.hit_within(x as f32, y as f32, ALTITUDE_TAP_RADIUS_PT)
    }

    /// Where object `i` of layer `id` was last drawn (sub-point and altitude).
    pub fn altitude_point_state(&self, id: &str, i: usize) -> Option<crate::render::altitude_layers::AltitudePointState> {
        self.app.altitude.state(id, i)
    }

    /// Where object `i` of layer `id` was last drawn on screen, in points.
    pub fn altitude_point_screen(&self, id: &str, i: usize) -> Option<(f32, f32)> {
        self.app.altitude.screen(id, i)
    }

    /// Dots the last frame drew, and that frame's altitude advance +
    /// prepare time (ms).
    pub fn altitude_stats(&self) -> (usize, f64) {
        (self.app.altitude_points_drawn, self.app.altitude_cpu_ms)
    }

    /// A data credit joined to the basemap's (`None` removes it).
    pub fn set_data_credit(&mut self, credit: Option<String>) {
        if self.app.data_credit != credit {
            self.app.data_credit = credit;
            self.request_frame();
        }
    }

    /// The camera's zoom range, intersected with the engine's
    /// (`SPACE_MIN_ZOOM` lets a space scene back out past the whole Earth).
    pub fn set_zoom_range(&mut self, min_zoom: f64, max_zoom: f64) {
        self.app.camera_control.set_zoom_range(min_zoom, max_zoom);
        self.request_frame();
    }

    /// The globe's space effects, plain by default: the star field (drawn
    /// below z6) and the day/night terminator at the wall clock's sun.
    pub fn set_space_effects(&mut self, stars: bool, terminator: bool) {
        self.app.space.star_intensity = if stars { 1.0 } else { 0.0 };
        self.app.space.atmosphere.night_brightness =
            if terminator { TERMINATOR_NIGHT_BRIGHTNESS } else { crate::render::atmosphere::Atmosphere::default().night_brightness };
        self.request_frame();
    }

    /// `(stars, terminator)` as [`Self::set_space_effects`] set them.
    pub fn space_effects(&self) -> (bool, bool) {
        (self.app.space.star_intensity > 0.0, self.app.space.atmosphere.night_brightness < 1.0)
    }

    pub fn set_buildings_3d(&mut self, on: bool) {
        if self.app.buildings_flat == on {
            self.app.buildings_flat = !on;
            self.request_frame();
        }
    }

    /// This frame's extrusion plan as `(draw steps, world units per metre)`
    /// -- zero per metre is flat -- and whether the overlays draw in their
    /// own pass over the buildings. For tests.
    #[doc(hidden)]
    pub fn buildings_summary(&self) -> (usize, f32, bool) {
        let plan = self.app.frame_extrusion_plan();
        (plan.steps.len(), plan.meters_to_world, self.app.overlays_above_buildings)
    }

    /// Shows and hides layers whose `min-zoom` / `max-zoom` the camera has
    /// crossed: a crossing marks the layer for the next push.
    fn apply_zoom_ranges(&mut self) {
        let zoom = self.camera().zoom;
        for layer in self.layers.values_mut() {
            let visible = layer.zoom_range.0 <= zoom && zoom < layer.zoom_range.1;
            if visible != layer.visible {
                layer.visible = visible;
                layer.dirty = true;
            }
        }
    }

    /// The sources the engine holds, after a push. For tests.
    #[doc(hidden)]
    pub fn engine_sources(&mut self) -> Vec<String> {
        self.flush();
        self.app.overlays.source_names().iter().map(|n| n.to_string()).collect()
    }

    /// Whether `layer`'s source draws data-driven paint. For tests.
    #[doc(hidden)]
    pub fn is_data_driven(&mut self, layer: &str) -> Option<bool> {
        self.flush();
        self.app.overlays.source_is_data_driven(layer)
    }

    /// `name`'s layer, created (on top) the first time. Under the
    /// highlight's layers, which stay on top of every app layer.
    fn layer_mut(&mut self, name: &str) -> &mut Layer {
        if !self.layers.contains_key(name) {
            let at = if is_highlight_layer(name) {
                self.order.len()
            } else {
                self.order.iter().position(|n| is_highlight_layer(n)).unwrap_or(self.order.len())
            };
            self.order.insert(at, name.to_string());
        }
        self.layers.entry(name.to_string()).or_insert_with(Layer::new)
    }

    // -- highlight ----------------------------------------------------------

    /// Lights feature `id` of `layer` -- the tapped or selected one -- or,
    /// with `None`, clears the highlight; a new one replaces the last.
    /// Returns whether a highlight is now drawn (`false` for a layer or id
    /// the map does not hold, or a layer that is not points, lines or
    /// polygons).
    ///
    /// Drawn over every layer, in `style` (`HighlightStyle::default()`: a
    /// 2 pt white outline over a 1 pt dark casing), as the desktop's
    /// `MapView::set_highlight` draws it: a point gets a ring round its
    /// own circle and is drawn again on top; a line or polygon gets its
    /// outline stroked. It follows the feature through upserts (moved,
    /// restyled), goes when the feature or its layer does, and hides with
    /// the layer outside its zoom range. Never picked: a tap on it is a
    /// tap on the feature under it.
    pub fn set_highlight(&mut self, layer: &str, id: Option<&str>, style: crate::overlay::host::HighlightStyle) -> bool {
        self.highlight = id.map(|id| Highlight { layer: layer.to_string(), id: id.to_string(), style });
        // Rebuilt even for the same feature: the style may be new.
        for name in HIGHLIGHT_LAYERS {
            if let Some(l) = self.layers.get_mut(name) {
                l.dirty = true;
            }
        }
        let drawn = self.sync_highlight();
        self.request_frame();
        drawn
    }

    /// The feature lit by [`Self::set_highlight`], as `(layer, id)`, if it
    /// is drawn.
    pub fn highlighted(&self) -> Option<(String, String)> {
        let h = self.highlight.as_ref()?;
        self.layers.get(h.layer.as_str())?.index.contains_key(&h.id).then(|| (h.layer.clone(), h.id.clone()))
    }

    /// Rebuilds the highlight's layers from the lit feature's current
    /// state when it or its layer changed. Whether a highlight is drawn.
    fn sync_highlight(&mut self) -> bool {
        let want = self.highlight.clone().and_then(|h| {
            let layer = self.layers.get(h.layer.as_str())?;
            let feature = layer.features.get(*layer.index.get(&h.id)?)?.clone();
            Some((h, layer.kind?, feature))
        });
        let fresh = |s: &Self| HIGHLIGHT_LAYERS.iter().all(|n| s.layers.get(*n).is_none_or(|l| !l.dirty));
        let Some((h, kind, feature)) = want else {
            for name in HIGHLIGHT_LAYERS {
                if let Some(l) = self.layers.get_mut(name).filter(|l| !l.features.is_empty()) {
                    l.features.clear();
                    l.index.clear();
                    l.dirty = true;
                }
            }
            return false;
        };
        let source = &self.layers[h.layer.as_str()];
        let (visible, zoom_range) = (source.visible, source.zoom_range);
        // Unchanged since the last build: nothing to redo.
        if !source.dirty && fresh(self) && self.layers.get(HIGHLIGHT_LAYERS[1]).is_some_and(|l| l.features.first().is_some_and(|f| f.id == highlight_id(&h))) {
            return true;
        }
        let style = h.style;
        let mut built: Vec<(Option<Kind>, Vec<LayerFeature>, FeaturePaint, serde_json::Map<String, serde_json::Value>)> = Vec::new();
        let tag = highlight_id(&h);
        match (kind, &feature.geometry) {
            (Kind::Points, LayerGeometry::Point(_)) => {
                let r = highlight_point_radius(source, &feature);
                let disc = |radius: f32, color: crate::style::Rgba| {
                    let paint = FeaturePaint { circle_radius: radius, circle_color: color, ..FeaturePaint::default() };
                    (Some(Kind::Points), vec![LayerFeature { id: tag.clone(), geometry: feature.geometry.clone(), props: Vec::new() }], paint, serde_json::Map::new())
                };
                let outline = r + style.outline_width_px;
                built.push(match style.casing_color {
                    Some(c) => disc(outline + style.casing_width_px, c),
                    None => (None, Vec::new(), FeaturePaint::default(), serde_json::Map::new()),
                });
                built.push(disc(outline, style.outline_color));
                // The point again, in its own paint, over the ring.
                built.push((Some(Kind::Points), vec![LayerFeature { id: tag.clone(), ..feature.clone() }], source.paint.clone(), source.declared.clone()));
            }
            (Kind::Lines | Kind::Polygons, LayerGeometry::Line(parts) | LayerGeometry::Polygon(parts)) => {
                let stroke = |width: f32, color: crate::style::Rgba| {
                    let base = FeaturePaint::default();
                    let paint = FeaturePaint { line_width: crate::style::ZoomValue::constant(width), line_color: color, fill_color: crate::style::Rgba { a: 0.0, ..base.fill_color }, ..base };
                    (Some(Kind::Lines), vec![LayerFeature { id: tag.clone(), geometry: LayerGeometry::Line(parts.clone()), props: Vec::new() }], paint, serde_json::Map::new())
                };
                built.push(match style.casing_color {
                    Some(c) => stroke(style.outline_width_px + 2.0 * style.casing_width_px, c),
                    None => (None, Vec::new(), FeaturePaint::default(), serde_json::Map::new()),
                });
                built.push(stroke(style.outline_width_px, style.outline_color));
                built.push((None, Vec::new(), FeaturePaint::default(), serde_json::Map::new()));
            }
            _ => {
                self.highlight = None;
                return self.sync_highlight();
            }
        }
        for (name, (kind, features, paint, declared)) in HIGHLIGHT_LAYERS.iter().zip(built) {
            let layer = self.layer_mut(name);
            if layer.kind != kind && !features.is_empty() {
                layer.kind = kind;
            }
            layer.index = features.iter().enumerate().map(|(i, f)| (f.id.clone(), i)).collect();
            layer.features = features;
            layer.paint = paint;
            if layer.declared != declared {
                layer.declared = declared;
                layer.redeclare = true;
            }
            layer.text = None;
            layer.visible = visible;
            layer.zoom_range = zoom_range;
            layer.dirty = true;
        }
        true
    }

    /// Pushes every layer changed since the last push to the engine, once.
    /// `render` and `touch` call it; a host never needs to.
    pub fn flush(&mut self) {
        self.sync_highlight();
        let display_zoom = self.app.display_zoom();
        let mut errors = Vec::new();
        let mut relabel = false;
        for name in &self.order {
            let Some(layer) = self.layers.get_mut(name).filter(|l| l.dirty) else { continue };
            layer.dirty = false;
            relabel = true;
            // The declared expressions belong to the LAYER, and reach the
            // engine only together with features: the engine never holds an
            // empty declared source. One that did -- declared at style
            // time, or re-declared when a layer emptied -- and was then
            // prepared through frames before its first features drew every
            // `["get", "color"]` line in the default colour, and restyling
            // did not recover it (the device bugs at 2a2029fc and d35d8371).
            let pushed = if layer.features.is_empty() || !layer.visible {
                self.app.overlays.remove_source(name);
                Ok(())
            } else {
                let exists = self.app.overlays.source_names().iter().any(|n| &**n == name.as_str());
                let declare = layer.redeclare || !exists;
                if declare {
                    // Rebuilt from scratch: declare, then the features, in
                    // one step -- the path a layer filled at once takes.
                    self.app.overlays.remove_source(name);
                }
                let declared = if declare && !layer.declared.is_empty() {
                    self.app.overlays.declare_source_paint(name, &layer.declared)
                } else {
                    Ok(())
                };
                declared.and_then(|()| self.app.overlays.set_layer(name, &layer.features, &layer.paint, display_zoom))
            };
            layer.redeclare = false;
            if let Err(error) = pushed {
                errors.push(HostError::Overlay { layer: name.clone(), reason: error.to_string() });
            }
        }
        if relabel {
            // A push that removed a source (emptied, hidden, outside its
            // zoom range) or re-created one (refilled, re-declared) left it
            // on top of the engine's stack, which is the order the engine
            // draws in. Put the stack back to `order`: the draw order the
            // host promises, whatever a layer's empty/hidden history.
            let names: Vec<&str> = self.order.iter().map(String::as_str).collect();
            self.app.overlays.set_source_order(&names);
            let plans = self.label_plans();
            self.app.overlay_label_plans = plans;
        }
        for error in errors {
            crate::mlog!("[embed] {error}");
        }
    }
}

/// The highlight's own feature id: the lit feature's, tagged with its
/// layer, so a re-light of an equal id on another layer rebuilds.
fn highlight_id(h: &Highlight) -> String {
    format!("{}\u{0}{}", h.layer, h.id)
}

/// The radius `feature` draws at in `layer`: its declared `circle-radius`
/// evaluated on its properties, else the layer's constant one.
fn highlight_point_radius(layer: &Layer, feature: &LayerFeature) -> f32 {
    let declared = layer.declared.get("circle-radius").and_then(|expr| {
        let mut one = serde_json::Map::new();
        one.insert("circle-radius".to_string(), expr.clone());
        let paint = crate::overlay::host::SourcePaint::compile(&one).ok()?;
        let row: crate::overlay::host::FeatureProperties = feature
            .props
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    PropValue::Number(n) => crate::overlay::host::TagValue::Number(*n),
                    PropValue::Text(t) => crate::overlay::host::TagValue::String(t.clone()),
                    PropValue::Bool(b) => crate::overlay::host::TagValue::Bool(*b),
                };
                (k.clone(), v)
            })
            .collect();
        paint.circle_radius(&row)
    });
    declared.unwrap_or(layer.paint.circle_radius).max(0.0)
}

/// Web Mercator `(x, y)` in `0..1` (y down) for a lon/lat in degrees.
fn mercator01(lon: f64, lat: f64) -> (f64, f64) {
    let lat = lat.clamp(-85.051_128, 85.051_128).to_radians();
    ((lon + 180.0) / 360.0, (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0)
}

/// The inverse of [`mercator01`].
fn lon_lat01(x: f64, y: f64) -> (f64, f64) {
    let lon = x * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
    (lon, lat)
}

/// The camera that fits `[west, south, east, north]` in `avail_w` x
/// `avail_h` points: centred on the box's Mercator middle, at the zoom where
/// its larger relative side just fits (512 pt tiles), north up, flat.
pub fn fit_target(bounds: [f64; 4], avail_w: f64, avail_h: f64, max_zoom: f64) -> CameraTarget {
    let [west, south, east, north] = bounds;
    let (x0, y0) = mercator01(west.min(east), north.max(south));
    let (x1, y1) = mercator01(west.max(east), north.min(south));
    let (w, h) = ((x1 - x0).max(1e-12), (y1 - y0).max(1e-12));
    let zoom = (avail_w / (w * 512.0)).min(avail_h / (h * 512.0)).log2().clamp(0.0, max_zoom);
    let center = lon_lat01((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    CameraTarget { center: Some(center), zoom: Some(zoom), bearing: Some(0.0), pitch: Some(0.0), ..Default::default() }
}

/// How far outside a point's circle a tap still picks it, in points: half
/// of Apple's 44 pt minimum touch target, less a typical circle.
pub const TAP_SLOP_PT: f64 = 12.0;

/// The frame predicate's answer, from whether a frame is still owed and the
/// loop's next timed wake.
pub fn frame_request(needs_redraw: bool, wake_at: Option<Instant>) -> FrameRequest {
    match (needs_redraw, wake_at) {
        (true, _) => FrameRequest::Again,
        (false, Some(at)) => FrameRequest::At(at),
        (false, None) => FrameRequest::Idle,
    }
}

// -- point-layer labels ------------------------------------------------------
//
// `text-field` and friends on a point layer, resolved per feature against
// its own `props` (MapLibre's `["get", k]` and `"{k}"` tokens) into
// `style::SymbolFeature`s -- the basemap's own symbol type -- so the labels
// go through `symbol_pass::collect_candidates` and one collision index with
// the basemap's, exactly as a basemap label does.

/// The zoom of the tiles label anchors are expressed in: 4096 units across
/// a z14 tile is ~0.6 m at the equator, finer than any screen pixel below
/// z18.
const LABEL_TILE_ZOOM: u8 = 14;

/// The tile, and the tile-local anchor, of a lon/lat at [`LABEL_TILE_ZOOM`].
fn label_anchor(lon: f64, lat: f64) -> Option<(TileId, geo_types::Point<i32>)> {
    if !(lon.is_finite() && lat.is_finite()) {
        return None;
    }
    let n = f64::from(1u32 << LABEL_TILE_ZOOM);
    let (x, y) = mercator01(lon, lat);
    let (tx, ty) = ((x * n).floor().clamp(0.0, n - 1.0), (y * n).floor().clamp(0.0, n - 1.0));
    let extent = f64::from(crate::mvt::MVT_EXTENT);
    let local = |v: f64, t: f64| ((v * n - t) * extent).round().clamp(0.0, extent - 1.0) as i32;
    Some((TileId { z: LABEL_TILE_ZOOM, x: tx as u32, y: ty as u32 }, geo_types::Point::new(local(x, tx), local(y, ty))))
}

/// A data-driven or constant value of a label property.
#[derive(Debug, Clone, PartialEq)]
enum Driven<T> {
    Const(T),
    /// `["get", key]`, with the value when the feature has no usable one.
    Get(String, T),
}

/// A point layer's labels.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TextStyle {
    /// `["get", k]`, or a string with `{k}` tokens.
    field: Driven<String>,
    size: Driven<f32>,
    color: Driven<crate::style::Rgba>,
    halo_color: crate::style::Rgba,
    halo_width: f32,
    offset_em: (f32, f32),
    anchor: crate::style::TextAnchor,
    max_width_em: f32,
}

fn css_color(value: &serde_json::Value) -> Result<crate::style::Rgba, String> {
    let text = value.as_str().ok_or_else(|| format!("a colour must be a string, got {value}"))?;
    color_text(text).ok_or_else(|| format!("{text:?} is not #rgb, #rrggbb, #rrggbbaa, rgb() or rgba()"))
}

/// A CSS colour: the style compiler's parser, plus `#rrggbbaa`.
fn color_text(text: &str) -> Option<crate::style::Rgba> {
    if let Some(hex) = text.strip_prefix('#').filter(|h| h.len() == 8) {
        let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok().map(|v| f32::from(v) / 255.0);
        return Some(crate::style::Rgba { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: byte(6)? });
    }
    crate::style_spec::color::parse_css_color(text).ok()
}

fn get_key(value: &serde_json::Value) -> Option<String> {
    match value.as_array().map(Vec::as_slice) {
        Some([op, key]) if op == "get" => key.as_str().map(str::to_string),
        _ => None,
    }
}

impl TextStyle {
    /// The `text-*` keys of a style object; `None` without a `text-field`.
    pub(crate) fn compile(keys: &serde_json::Map<String, serde_json::Value>) -> Result<Option<TextStyle>, String> {
        let Some(field) = keys.get("text-field") else {
            if let Some(key) = keys.keys().next() {
                return Err(format!("{key} needs a text-field beside it"));
            }
            return Ok(None);
        };
        let field = match (field.as_str(), get_key(field)) {
            (Some(template), _) => Driven::Const(template.to_string()),
            (None, Some(key)) => Driven::Get(key, String::new()),
            _ => return Err(format!("text-field: [\"get\", key] or a \"{{key}}\" string, got {field}")),
        };
        let mut style = TextStyle {
            field,
            size: Driven::Const(16.0),
            color: Driven::Const(crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            halo_color: crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
            halo_width: 0.0,
            offset_em: (0.0, 0.0),
            anchor: crate::style::TextAnchor::Center,
            max_width_em: 10.0,
        };
        for (key, value) in keys {
            let bad = |why: &str| format!("{key}: {why}, got {value}");
            match key.as_str() {
                "text-field" => {}
                "text-size" => {
                    style.size = match (value.as_f64(), get_key(value)) {
                        (Some(n), _) if n > 0.0 => Driven::Const(n as f32),
                        (None, Some(k)) => Driven::Get(k, 16.0),
                        _ => return Err(bad("a positive number or [\"get\", key]")),
                    }
                }
                "text-color" => {
                    style.color = match get_key(value) {
                        Some(k) => Driven::Get(k, crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                        None => Driven::Const(css_color(value).map_err(|e| bad(&e))?),
                    }
                }
                "text-halo-color" => style.halo_color = css_color(value).map_err(|e| bad(&e))?,
                "text-halo-width" => {
                    style.halo_width = value.as_f64().filter(|w| *w >= 0.0).ok_or_else(|| bad("a number >= 0"))? as f32
                }
                "text-max-width" => {
                    style.max_width_em = value.as_f64().filter(|w| *w > 0.0).ok_or_else(|| bad("a number > 0"))? as f32
                }
                "text-offset" => {
                    let pair = value.as_array().filter(|a| a.len() == 2).ok_or_else(|| bad("[x, y] in ems"))?;
                    let (x, y) = (pair[0].as_f64(), pair[1].as_f64());
                    style.offset_em = (x.ok_or_else(|| bad("[x, y] in ems"))? as f32, y.ok_or_else(|| bad("[x, y] in ems"))? as f32);
                }
                "text-anchor" => {
                    style.anchor = value
                        .as_str()
                        .and_then(crate::style::TextAnchor::from_keyword)
                        .ok_or_else(|| bad("a text-anchor keyword"))?
                }
                other => return Err(format!("{other} is not supported on a point layer's labels yet")),
            }
        }
        Ok(Some(style))
    }

    /// The label for one feature, or `None` when it resolves empty -- the
    /// point's circle still draws (MapLibre: an empty `text-field` places no
    /// text and takes nothing from the icon).
    fn symbol(&self, props: &[(String, PropValue)], sort_key: f32) -> Option<crate::style::SymbolFeature> {
        let prop = |key: &str| props.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        let text_of = |v: &PropValue| match v {
            PropValue::Text(s) => s.clone(),
            PropValue::Number(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
            PropValue::Number(n) => format!("{n}"),
            PropValue::Bool(b) => b.to_string(),
        };
        let text = match &self.field {
            Driven::Get(key, _) => prop(key).map(text_of).unwrap_or_default(),
            Driven::Const(template) => fill_tokens(template, |key| prop(key).map(text_of)),
        };
        if text.trim().is_empty() {
            return None;
        }
        let size = match &self.size {
            Driven::Const(n) => *n,
            Driven::Get(key, fallback) => match prop(key) {
                Some(PropValue::Number(n)) if *n > 0.0 => *n as f32,
                _ => *fallback,
            },
        };
        let color = match &self.color {
            Driven::Const(c) => *c,
            Driven::Get(key, fallback) => match prop(key) {
                Some(PropValue::Text(s)) => color_text(s).unwrap_or(*fallback),
                _ => *fallback,
            },
        };
        Some(crate::style::SymbolFeature {
            anchor: geo_types::Point::new(0, 0),
            text: Some(text),
            icon: None,
            text_size: size,
            text_offset: crate::style::point_text_offset(self.anchor, 0.0, self.offset_em),
            text_anchor: self.anchor,
            text_max_width_em: Some(self.max_width_em),
            icon_size_from: 1.0,
            icon_size_to: 1.0,
            text_color: color,
            text_halo_color: self.halo_color,
            text_halo_width: self.halo_width,
            text_halo_blur: 0.0,
            text_font: crate::font_fallback::default_font_stack(),
            text_opacity: 1.0,
            icon_opacity: 1.0,
            icon_color: color,
            icon_halo_color: self.halo_color,
            icon_halo_width: 0.0,
            layer_index: crate::render::label_plan::OVERLAY_LAYER_INDEX,
            sort_key,
            forward_ref: None,
            line: None,
            line_centre_distance: 0.0,
            icon_rotate_deg: 0.0,
            // Labels of one layer collide with each other and with the
            // basemap's, as on the desktop.
            bypass_collision: false,
        })
    }
}

/// `template` with every `{key}` replaced by the feature's value, or by
/// nothing where it has none (MapLibre's token rule).
fn fill_tokens(template: &str, value: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        match rest[open..].find('}') {
            Some(close) => {
                out.push_str(&value(&rest[open + 1..open + close]).unwrap_or_default());
                rest = &rest[open + close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

// -- vehicle markers ---------------------------------------------------------
//
// The drop-shaped heading marker a downstream desktop app draws for each
// vehicle (its `panels/vehicle_map.rs`, `heading_marker_polygon` and the
// constants beside it), so the phone app's buses match the desktop's.

/// Mean metres per degree of latitude.
const METERS_PER_DEGREE_LAT: f64 = 111_320.0;
/// Ground metres per pixel of a 512 px tile at zoom 0, on the equator.
const EQUATOR_METERS_PER_PIXEL_AT_Z0: f64 = 40_075_016.686 / 512.0;
const MARKER_MIN_RADIUS_PX: f64 = 2.25;
const MARKER_MAX_RADIUS_PX: f64 = 7.0;
const MARKER_MIN_ZOOM: f64 = 10.0;
const MARKER_MAX_ZOOM: f64 = 16.0;
/// The apex sits this many radii ahead of the centre; above 1, or the sides
/// have no tangent to meet.
const MARKER_APEX_RATIO: f64 = 2.6;
const MARKER_ARC_STEPS: usize = 8;
/// Above this many vehicles the desktop draws plain circles instead.
pub const MARKER_POLYGON_LIMIT: usize = 400;

/// The marker's head radius in screen pixels at `zoom`: a ramp between
/// zoom 10 and 16, flat outside.
pub fn marker_radius_px(zoom: f64) -> f64 {
    let t = ((zoom - MARKER_MIN_ZOOM) / (MARKER_MAX_ZOOM - MARKER_MIN_ZOOM)).clamp(0.0, 1.0);
    MARKER_MIN_RADIUS_PX + t * (MARKER_MAX_RADIUS_PX - MARKER_MIN_RADIUS_PX)
}

/// Ground metres per screen pixel at `zoom` and `latitude`.
pub fn meters_per_pixel(zoom: f64, latitude: f64) -> f64 {
    EQUATOR_METERS_PER_PIXEL_AT_Z0 * latitude.to_radians().cos().abs().max(0.01) / 2f64.powf(zoom)
}

fn offset_point(lat: f64, lon: f64, bearing_degrees: f64, distance_meters: f64) -> (f64, f64) {
    let bearing = bearing_degrees.to_radians();
    let dlat = (distance_meters * bearing.cos()) / METERS_PER_DEGREE_LAT;
    let dlon = (distance_meters * bearing.sin()) / (METERS_PER_DEGREE_LAT * lat.to_radians().cos().max(0.01));
    (lat + dlat, lon + dlon)
}

/// A water-drop ring around `(lon, lat)`, its point aimed along
/// `heading_degrees` (compass, clockwise from north), the round head
/// `radius_meters` across: `[lon, lat]` pairs, closed.
pub fn heading_marker_ring(lon: f64, lat: f64, heading_degrees: f64, radius_meters: f64) -> Vec<[f64; 2]> {
    let apex_meters = radius_meters * MARKER_APEX_RATIO;
    let tangent_offset_degrees = (1.0 / MARKER_APEX_RATIO).acos().to_degrees();
    let mut ring = Vec::with_capacity(MARKER_ARC_STEPS + 3);
    let apex = offset_point(lat, lon, heading_degrees, apex_meters);
    ring.push([apex.1, apex.0]);
    let arc_start = heading_degrees + tangent_offset_degrees;
    let arc_sweep = 360.0 - 2.0 * tangent_offset_degrees;
    for step in 0..=MARKER_ARC_STEPS {
        let bearing = arc_start + arc_sweep * (step as f64 / MARKER_ARC_STEPS as f64);
        let point = offset_point(lat, lon, bearing, radius_meters);
        ring.push([point.1, point.0]);
    }
    ring.push([apex.1, apex.0]);
    ring
}

