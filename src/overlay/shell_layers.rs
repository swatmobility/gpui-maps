//! `ShellLayers`: arc, overlay-extrusion, trip and heatmap layers for a
//! shell that is not `MapView` (the iOS shell and its embed). The
//! renderers are the engine's own (`render::arcs`, `render::overlay_extrusion`,
//! `render::trips`, `render::heatmap`); this holds the layer state and runs
//! them in `MapView::paint`'s order:
//!
//! 1. **Extrusions in the tile pass**, after the basemap's buildings, on the
//!    same pipelines and depth buffer ([`ShellLayers::push_tile_draws`]), so
//!    a hexbin column and a building occlude each other.
//! 2. **Off-screen accumulation** before the overlay pass
//!    ([`ShellLayers::encode_offscreen`]): a glow arc layer sums into its
//!    float target, each heatmap into its density target.
//! 3. **One pass over the tiles, under the labels**
//!    ([`ShellLayers::draw_overlay`]): heatmaps (density is a backdrop),
//!    then arcs, then trips -- the desktop's symbol-pass order.
//!
//! Picking ([`ShellLayers::pick`]): an arc (drawn over everything) first,
//! then the extrusion column nearest the eye, as `MapView::pick` reads
//! them. Heatmaps and trips pick nothing, as on the desktop.
//!
//! Names no platform and no clock: the browser compiles it too.

use std::sync::{Arc, OnceLock};

use crate::overlay::extrude::{ExtrusionStyle, OverlayExtrusion};
use crate::render::arc_pick::ArcPickIndex;
use crate::render::arcs::{ArcFrame, ArcFrameStats, ArcLayerDraw, ArcPipeline, ArcSet, ArcStyle};
use crate::render::heatmap::{HeatmapFrameStats, HeatmapLayerDraw, HeatmapPipeline, HeatmapSet, HeatmapStyle};
use crate::render::overlay_extrusion::OverlayExtrusionGpu;
use crate::render::trips::{TripFrameStats, TripLayerDraw, TripPipeline, TripSet, TripStyle};

/// The desktop's divisor for these layers' memory: a quarter on iOS and
/// the browser, as `host::OVERLAY_CACHE_DIVISOR` is for the overlay tiles.
#[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
const DIVISOR: u64 = 1;
#[cfg(any(target_os = "ios", target_arch = "wasm32"))]
const DIVISOR: u64 = 4;

/// The bytes every layer's data may hold on the GPU together: the
/// desktop's 256 MB overlay budget, divided by 4 on a phone. A set past it
/// is still drawn (refusing it would be a silent simplification); the
/// shell logs it and [`ShellLayers::over_budget`] says so.
pub const SHELL_LAYER_BUDGET_BYTES: u64 = (256 << 20) / DIVISOR;

/// The desktop's own budget, whatever this build's divisor: what a host
/// test measures a phone's layers against (divided by 4 itself).
pub const DESKTOP_LAYER_BUDGET_BYTES: u64 = 256 << 20;

/// Extra slop round an arc's drawn width when picking, logical px (the
/// desktop's `MapView::ARC_TOLERANCE_PX` is for a mouse; a finger is wider).
pub const ARC_TAP_TOLERANCE_PX: f64 = 8.0;

/// Which renderer a layer draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellLayerKind {
    Arcs,
    Extrusion,
    Trips,
    Heatmap,
}

struct ArcLayer {
    id: String,
    set: Arc<ArcSet>,
    style: ArcStyle,
    window: Option<[f64; 2]>,
    generation: u64,
    /// Built on the first pick (about 150 ms at 680k arcs on a desktop).
    pick: OnceLock<Arc<ArcPickIndex>>,
}

impl ArcLayer {
    fn filter(&self) -> Option<[f32; 2]> {
        self.window.and_then(|w| self.set.relative_window(w))
    }
}

struct ExtrusionLayer {
    id: String,
    set: Arc<OverlayExtrusion>,
    style: ExtrusionStyle,
    generation: u64,
}

struct TripLayer {
    id: String,
    set: Arc<TripSet>,
    style: TripStyle,
    clock: Option<f64>,
    generation: u64,
}

struct HeatLayer {
    id: String,
    set: Arc<HeatmapSet>,
    style: HeatmapStyle,
    windows: Vec<Option<[f64; 2]>>,
    generation: u64,
}

/// An extrusion layer's upload, at the generation it was made from.
struct HeldExtrusion {
    id: String,
    generation: u64,
    gpu: OverlayExtrusionGpu,
}

/// The device half, built lazily: a map with no arcs builds no arc pipeline.
#[derive(Default)]
struct Gpu {
    arcs: Option<ArcPipeline>,
    trips: Option<TripPipeline>,
    heat: Option<HeatmapPipeline>,
    extrusions: Vec<HeldExtrusion>,
}

/// What the last prepared frame drew, and what the layers hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShellLayerStats {
    pub arcs: ArcFrameStats,
    pub trips: TripFrameStats,
    pub heatmaps: HeatmapFrameStats,
    /// Extrusion layers drawn and their triangles this frame.
    pub extrusion_layers: u64,
    pub extrusion_triangles: u64,
    /// Every layer's data, bytes (arc instances, trip vertices, heat
    /// points, extrusion vertices and indices).
    pub data_bytes: u64,
}

/// One frame's inputs: the device, the tile pipeline the extrusions draw
/// with, the camera (logical px), the frame's globe, the style's building
/// light, the pixel ratio and the target's physical size.
pub struct ShellFrame<'a> {
    pub ctx: &'a crate::render::GpuContext,
    pub tile_pipeline: &'a crate::render::TilePipeline,
    pub color_format: wgpu::TextureFormat,
    pub camera: &'a crate::render::Camera,
    pub globe_frame: &'a crate::render::globe::GlobeFrame,
    pub globe: &'a crate::render::globe::GlobeUpload,
    pub light: &'a crate::style::ExtrusionLight,
    pub px_scale: f32,
    pub physical: (u32, u32),
}

/// A pick: the layer, its kind and the index -- an arc's INPUT row
/// (`ArcSet::row`), an extrusion's feature (the order `extrude` was given).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellHit {
    pub layer: String,
    pub kind: ShellLayerKind,
    pub index: usize,
}

/// The layers and their GPU state. See the module doc.
#[derive(Default)]
pub struct ShellLayers {
    arcs: Vec<ArcLayer>,
    extrusions: Vec<ExtrusionLayer>,
    trips: Vec<TripLayer>,
    heatmaps: Vec<HeatLayer>,
    next_generation: u64,
    gpu: Gpu,
    stats: ShellLayerStats,
    /// What this frame prepared, so the passes agree with `prepare`.
    frame_arcs: bool,
    frame_trips: bool,
    frame_heat: bool,
    /// The extrusion triangles [`Self::push_tile_draws`] last pushed.
    extrusion_triangles: std::cell::Cell<u64>,
}

impl ShellLayers {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Which kind layer `id` is, if any.
    #[must_use]
    pub fn kind_of(&self, id: &str) -> Option<ShellLayerKind> {
        if self.arcs.iter().any(|l| l.id == id) {
            Some(ShellLayerKind::Arcs)
        } else if self.extrusions.iter().any(|l| l.id == id) {
            Some(ShellLayerKind::Extrusion)
        } else if self.trips.iter().any(|l| l.id == id) {
            Some(ShellLayerKind::Trips)
        } else if self.heatmaps.iter().any(|l| l.id == id) {
            Some(ShellLayerKind::Heatmap)
        } else {
            None
        }
    }

    /// Layer `id`'s size: arcs, extruded features, trips or heat points.
    #[must_use]
    pub fn len_of(&self, id: &str) -> Option<usize> {
        let a = self.arcs.iter().find(|l| l.id == id).map(|l| l.set.len());
        let e = || self.extrusions.iter().find(|l| l.id == id).map(|l| l.set.features);
        let t = || self.trips.iter().find(|l| l.id == id).map(|l| l.set.len());
        let h = || self.heatmaps.iter().find(|l| l.id == id).map(|l| l.set.len());
        a.or_else(e).or_else(t).or_else(h)
    }

    /// Every layer id, by kind.
    #[must_use]
    pub fn ids(&self) -> Vec<(String, ShellLayerKind)> {
        let a = self.arcs.iter().map(|l| (l.id.clone(), ShellLayerKind::Arcs));
        let e = self.extrusions.iter().map(|l| (l.id.clone(), ShellLayerKind::Extrusion));
        let t = self.trips.iter().map(|l| (l.id.clone(), ShellLayerKind::Trips));
        let h = self.heatmaps.iter().map(|l| (l.id.clone(), ShellLayerKind::Heatmap));
        a.chain(e).chain(t).chain(h).collect()
    }

    /// Whether nothing would draw.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.arcs.iter().all(|l| l.set.is_empty())
            && self.extrusions.iter().all(|l| l.set.is_empty())
            && self.trips.iter().all(|l| l.set.is_empty())
            && self.heatmaps.iter().all(|l| l.set.is_empty() || l.style.opacity <= 0.0)
    }

    /// Removes layer `id`, whatever its kind; whether it was there. Its GPU
    /// buffers go on the next frame.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.arcs.len() + self.extrusions.len() + self.trips.len() + self.heatmaps.len();
        self.arcs.retain(|l| l.id != id);
        self.extrusions.retain(|l| l.id != id);
        self.trips.retain(|l| l.id != id);
        self.heatmaps.retain(|l| l.id != id);
        before != self.arcs.len() + self.extrusions.len() + self.trips.len() + self.heatmaps.len()
    }

    /// Takes `id` out of every kind but `keep`: a layer id names one layer.
    fn claim(&mut self, id: &str, keep: ShellLayerKind) {
        if self.kind_of(id).is_some_and(|k| k != keep) {
            self.remove(id);
        }
    }

    // -- arcs -----------------------------------------------------------------

    /// Draws `set` as arc layer `id` in `style`, replacing any layer of that
    /// id (its filter window is kept). Layers draw in the order first set.
    pub fn set_arc_layer(&mut self, id: &str, set: Arc<ArcSet>, style: ArcStyle) {
        self.claim(id, ShellLayerKind::Arcs);
        let generation = self.generation();
        match self.arcs.iter_mut().find(|l| l.id == id) {
            Some(l) => {
                l.set = set;
                l.style = style;
                l.generation = generation;
                l.pick = OnceLock::new();
            }
            None => self.arcs.push(ArcLayer { id: id.to_string(), set, style, window: None, generation, pick: OnceLock::new() }),
        }
    }

    /// Layer `id`'s style: a uniform write, no re-upload.
    pub fn set_arc_style(&mut self, id: &str, style: ArcStyle) -> bool {
        self.arcs.iter_mut().find(|l| l.id == id).map(|l| l.style = style).is_some()
    }

    /// Shows only the arcs whose filter value is in `range` (absolute), or
    /// all with `None`. A uniform write.
    pub fn set_arc_filter_range(&mut self, id: &str, range: Option<[f64; 2]>) -> bool {
        self.arcs.iter_mut().find(|l| l.id == id).map(|l| l.window = range).is_some()
    }

    /// Layer `id`'s set and style.
    #[must_use]
    pub fn arc_layer(&self, id: &str) -> Option<(Arc<ArcSet>, ArcStyle)> {
        self.arcs.iter().find(|l| l.id == id).map(|l| (Arc::clone(&l.set), l.style))
    }

    // -- extrusions -----------------------------------------------------------

    /// Draws `set` as extrusion layer `id` in `style`, replacing any layer
    /// of that id.
    pub fn set_extrusion_layer(&mut self, id: &str, set: Arc<OverlayExtrusion>, style: ExtrusionStyle) {
        self.claim(id, ShellLayerKind::Extrusion);
        let generation = self.generation();
        match self.extrusions.iter_mut().find(|l| l.id == id) {
            Some(l) => {
                l.set = set;
                l.style = style;
                l.generation = generation;
            }
            None => self.extrusions.push(ExtrusionLayer { id: id.to_string(), set, style, generation }),
        }
    }

    /// Layer `id`'s opacity, light and look: a uniform write.
    pub fn set_extrusion_style(&mut self, id: &str, style: ExtrusionStyle) -> bool {
        self.extrusions.iter_mut().find(|l| l.id == id).map(|l| l.style = style).is_some()
    }

    #[must_use]
    pub fn extrusion_layer(&self, id: &str) -> Option<(Arc<OverlayExtrusion>, ExtrusionStyle)> {
        self.extrusions.iter().find(|l| l.id == id).map(|l| (Arc::clone(&l.set), l.style))
    }

    // -- trips ----------------------------------------------------------------

    /// Draws `set` as trip layer `id` in `style`, replacing any layer of
    /// that id (its clock is kept). A new layer draws its paths whole until
    /// [`Self::set_trip_clock`].
    pub fn set_trip_layer(&mut self, id: &str, set: Arc<TripSet>, style: TripStyle) {
        self.claim(id, ShellLayerKind::Trips);
        let generation = self.generation();
        match self.trips.iter_mut().find(|l| l.id == id) {
            Some(l) => {
                l.set = set;
                l.style = style;
                l.generation = generation;
            }
            None => self.trips.push(TripLayer { id: id.to_string(), set, style, clock: None, generation }),
        }
    }

    pub fn set_trip_style(&mut self, id: &str, style: TripStyle) -> bool {
        self.trips.iter_mut().find(|l| l.id == id).map(|l| l.style = style).is_some()
    }

    /// Layer `id`'s clock, absolute in its set's time unit (`None`: whole
    /// paths). A uniform write on the next frame.
    pub fn set_trip_clock(&mut self, id: &str, clock: Option<f64>) -> bool {
        self.trips.iter_mut().find(|l| l.id == id).map(|l| l.clock = clock).is_some()
    }

    #[must_use]
    pub fn trip_clock(&self, id: &str) -> Option<f64> {
        self.trips.iter().find(|l| l.id == id).and_then(|l| l.clock)
    }

    /// Layer `id`'s set: its time span reads off it (`TripSet::spans`).
    #[must_use]
    pub fn trip_layer(&self, id: &str) -> Option<(Arc<TripSet>, TripStyle)> {
        self.trips.iter().find(|l| l.id == id).map(|l| (Arc::clone(&l.set), l.style))
    }

    // -- heatmaps -------------------------------------------------------------

    /// Draws `set` as heatmap layer `id` in `style`, replacing any layer of
    /// that id (its windows kept when the filter columns match).
    pub fn set_heatmap_layer(&mut self, id: &str, set: Arc<HeatmapSet>, style: HeatmapStyle) {
        self.claim(id, ShellLayerKind::Heatmap);
        let generation = self.generation();
        match self.heatmaps.iter_mut().find(|l| l.id == id) {
            Some(l) => {
                if l.set.filter_fields != set.filter_fields {
                    l.windows.clear();
                }
                l.set = set;
                l.style = style;
                l.generation = generation;
            }
            None => self.heatmaps.push(HeatLayer { id: id.to_string(), set, style, windows: Vec::new(), generation }),
        }
    }

    pub fn set_heatmap_style(&mut self, id: &str, style: HeatmapStyle) -> bool {
        self.heatmaps.iter_mut().find(|l| l.id == id).map(|l| l.style = style).is_some()
    }

    /// Layer `id`'s windows, absolute, one per filter column.
    pub fn set_heatmap_filter_ranges(&mut self, id: &str, windows: &[Option<[f64; 2]>]) -> bool {
        self.heatmaps.iter_mut().find(|l| l.id == id).map(|l| l.windows = windows.to_vec()).is_some()
    }

    // -- budget -----------------------------------------------------------------

    /// Every layer's data bytes (what the GPU holds for them, give or take
    /// the uniforms).
    #[must_use]
    pub fn data_bytes(&self) -> u64 {
        let a: usize = self.arcs.iter().map(|l| l.set.upload_bytes()).sum();
        let e: usize = self.extrusions.iter().map(|l| l.set.upload_bytes()).sum();
        let t: usize = self.trips.iter().map(|l| l.set.upload_bytes()).sum();
        let h: usize = self.heatmaps.iter().map(|l| l.set.upload_bytes()).sum();
        (a + e + t + h) as u64
    }

    /// Whether the layers hold more than [`SHELL_LAYER_BUDGET_BYTES`].
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.data_bytes() > SHELL_LAYER_BUDGET_BYTES
    }

    /// What the last [`Self::prepare`] planned.
    #[must_use]
    pub fn stats(&self) -> ShellLayerStats {
        ShellLayerStats { data_bytes: self.data_bytes(), extrusion_triangles: self.extrusion_triangles.get(), ..self.stats }
    }

    // -- the frame --------------------------------------------------------------

    /// Uploads what changed, drops what went, and writes this frame's
    /// uniforms for every kind. Call once per frame, before the tile pass.
    pub fn prepare(&mut self, f: &ShellFrame<'_>) {
        // Extrusions: the desktop's `ExtrusionLayers::sync`.
        let held = &mut self.gpu.extrusions;
        held.retain(|h| self.extrusions.iter().any(|l| l.id == h.id && l.generation == h.generation && !l.set.is_empty()));
        for l in self.extrusions.iter().filter(|l| !l.set.is_empty()) {
            if !held.iter().any(|h| h.id == l.id) {
                held.push(HeldExtrusion { id: l.id.clone(), generation: l.generation, gpu: OverlayExtrusionGpu::upload(f.ctx, f.tile_pipeline, &l.set) });
            }
        }
        held.sort_by_key(|h| self.extrusions.iter().position(|l| l.id == h.id));
        for h in held.iter_mut() {
            if let Some(l) = self.extrusions.iter().find(|l| l.id == h.id) {
                h.gpu.prepare(f.ctx, f.tile_pipeline, f.camera, Some(f.globe), &l.style, f.light);
            }
        }
        self.stats.extrusion_layers = held.len() as u64;

        let frame = ArcFrame::new(f.camera, f.globe_frame, f.px_scale);

        // Heatmaps: a uniform and a ramp per layer; points on a new set.
        let heat_draws: Vec<HeatmapLayerDraw<'_>> = self
            .heatmaps
            .iter()
            .filter(|l| !l.set.is_empty() && l.style.opacity > 0.0)
            .map(|l| HeatmapLayerDraw { id: &l.id, generation: l.generation, set: &l.set, style: &l.style, filter: l.set.relative_windows(&l.windows) })
            .collect();
        self.frame_heat = !heat_draws.is_empty() || self.gpu.heat.is_some();
        if self.frame_heat {
            let pipeline = self.gpu.heat.get_or_insert_with(|| HeatmapPipeline::new(f.ctx, f.color_format));
            pipeline.prepare(f.ctx, &frame, &heat_draws, f.physical);
            self.stats.heatmaps = pipeline.stats();
            self.frame_heat = pipeline.stats().layers > 0;
        }

        // Arcs: one uniform per length class; instances on a new set.
        let arc_draws: Vec<ArcLayerDraw<'_>> = self
            .arcs
            .iter()
            .filter(|l| !l.set.is_empty())
            .map(|l| ArcLayerDraw { id: &l.id, generation: l.generation, set: &l.set, style: l.style, filter: l.filter() })
            .collect();
        self.frame_arcs = !arc_draws.is_empty() || self.gpu.arcs.is_some();
        if self.frame_arcs {
            let pipeline = self.gpu.arcs.get_or_insert_with(|| ArcPipeline::new(f.ctx, f.color_format, None));
            pipeline.prepare(f.ctx, &frame, &arc_draws);
            self.stats.arcs = pipeline.stats();
            self.frame_arcs = pipeline.stats().draws > 0;
        }

        // Trips: one uniform per layer carries the clock.
        let trip_draws: Vec<TripLayerDraw<'_>> = self
            .trips
            .iter()
            .filter(|l| !l.set.is_empty())
            .map(|l| TripLayerDraw { id: &l.id, generation: l.generation, set: &l.set, style: l.style, clock: l.clock })
            .collect();
        self.frame_trips = !trip_draws.is_empty() || self.gpu.trips.is_some();
        if self.frame_trips {
            let pipeline = self.gpu.trips.get_or_insert_with(|| TripPipeline::new(f.ctx, f.color_format, None));
            pipeline.prepare(f.ctx, &frame, &trip_draws);
            self.stats.trips = pipeline.stats();
            self.frame_trips = pipeline.stats().draws > 0;
        }
    }

    /// Whether [`Self::push_tile_draws`] draws anything: the caller's own
    /// 3D columns, which keep the overlay sources in the tile pass
    /// (`render::overlay_order::overlay_pass`).
    #[must_use]
    pub fn draws_tile_pass(&self) -> bool {
        !self.gpu.extrusions.is_empty()
    }

    /// Appends the extrusion layers' draws to the tile pass's list: call
    /// after the basemap's buildings are pushed, so they share its depth.
    pub fn push_tile_draws<'a>(&'a self, out: &mut Vec<crate::render::TileDraw<'a>>) {
        let from = out.len();
        for (k, h) in self.gpu.extrusions.iter().enumerate() {
            h.gpu.push_draws(k, out);
        }
        let triangles: u64 = out[from..].iter().map(crate::render::TileDraw::triangles).sum();
        // `&self`: recorded through the stats cell the next `stats` reads.
        self.extrusion_triangles.set(triangles);
    }

    /// The passes of their own the glow arcs and the heatmaps need, before
    /// the overlay pass: call between the tile pass and [`Self::draw_overlay`].
    pub fn encode_offscreen(&mut self, ctx: &crate::render::GpuContext, encoder: &mut wgpu::CommandEncoder, physical: (u32, u32)) {
        if self.frame_arcs {
            if let Some(p) = self.gpu.arcs.as_mut() {
                p.accumulate(ctx, encoder, physical);
            }
        }
        if self.frame_heat {
            if let Some(p) = self.gpu.heat.as_mut() {
                p.accumulate(encoder);
            }
        }
    }

    /// Whether [`Self::draw_overlay`] has anything to draw this frame.
    #[must_use]
    pub fn draws_overlay(&self) -> bool {
        self.frame_arcs || self.frame_trips || self.frame_heat
    }

    /// Heatmaps, arcs, then trips, into a pass over the tiles with no depth
    /// attachment (the desktop's symbol pass, before its icons and labels).
    pub fn draw_overlay(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.frame_heat {
            if let Some(p) = self.gpu.heat.as_ref() {
                p.draw(pass);
            }
        }
        if self.frame_arcs {
            if let Some(p) = self.gpu.arcs.as_ref() {
                p.draw(pass);
            }
        }
        if self.frame_trips {
            if let Some(p) = self.gpu.trips.as_ref() {
                p.draw(pass);
            }
        }
    }

    /// Opens a pass on `view` (`LoadOp::Load`) and draws
    /// [`Self::draw_overlay`] into it; nothing when there is nothing.
    pub fn render_overlay_pass(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
        if !self.draws_overlay() {
            return;
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shell-layers-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        self.draw_overlay(&mut pass);
    }

    /// Drops every GPU object (a lost surface); the layers stay and upload
    /// again on the next frame.
    pub fn drop_gpu(&mut self) {
        self.gpu = Gpu::default();
        self.frame_arcs = false;
        self.frame_trips = false;
        self.frame_heat = false;
    }

    /// The layer under logical `(x, y)`: the nearest arc within its drawn
    /// width plus `tolerance_px` (arcs draw over everything), else the
    /// extrusion column the ray meets first.
    #[must_use]
    pub fn pick(&self, camera: &crate::render::Camera, globe_frame: &crate::render::globe::GlobeFrame, x: f32, y: f32, tolerance_px: f64) -> Option<ShellHit> {
        if let Some(hit) = self.pick_arc(camera, globe_frame, x, y, tolerance_px) {
            return Some(hit);
        }
        self.pick_extrusion(camera, globe_frame, x, y)
    }

    fn pick_arc(&self, camera: &crate::render::Camera, globe_frame: &crate::render::globe::GlobeFrame, x: f32, y: f32, tolerance_px: f64) -> Option<ShellHit> {
        let frame = ArcFrame::new(camera, globe_frame, 1.0);
        let mut best: Option<(usize, usize, f64)> = None;
        for (k, l) in self.arcs.iter().enumerate() {
            if l.set.is_empty() || l.style.opacity <= 0.0 {
                continue;
            }
            let index = l.pick.get_or_init(|| Arc::new(ArcPickIndex::new(&l.set)));
            let Some(hit) = crate::render::arc_pick::pick(&frame, &l.set, index, &l.style, l.filter(), f64::from(x), f64::from(y), tolerance_px) else { continue };
            let under = |d: f64| d <= crate::render::arc_pick::UNDER_CURSOR_PX;
            let better = match best {
                None => true,
                Some((_, _, d)) => under(hit.distance_px) || (!under(d) && hit.distance_px <= d),
            };
            if better {
                best = Some((k, hit.index, hit.distance_px));
            }
        }
        let (k, i, _) = best?;
        let l = &self.arcs[k];
        Some(ShellHit { layer: l.id.clone(), kind: ShellLayerKind::Arcs, index: l.set.row(i).unwrap_or(i) })
    }

    fn pick_extrusion(&self, camera: &crate::render::Camera, globe_frame: &crate::render::globe::GlobeFrame, x: f32, y: f32) -> Option<ShellHit> {
        let mut best: Option<(usize, crate::overlay::extrude::ExtrusionHit)> = None;
        for (k, l) in self.extrusions.iter().enumerate() {
            if l.set.is_empty() || l.style.opacity <= 0.0 {
                continue;
            }
            let hit = if globe_frame.transition > 0.5 {
                crate::overlay::extrude::pick_on_globe(&l.set, globe_frame, camera, x, y)
            } else {
                crate::overlay::extrude::pick(&l.set, camera, x, y)
            };
            if let Some(hit) = hit {
                if best.is_none_or(|(_, b)| hit.depth >= b.depth) {
                    best = Some((k, hit));
                }
            }
        }
        best.map(|(k, hit)| ShellHit { layer: self.extrusions[k].id.clone(), kind: ShellLayerKind::Extrusion, index: hit.feature })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arcs() -> Arc<ArcSet> {
        use crate::overlay::arcs::{build, ArcColors, ArcWidths};
        let c = crate::style::Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
        Arc::new(build(&[[0.0, 0.0], [1.0, 1.0]], &[[2.0, 2.0], [3.0, 3.0]], &ArcColors::Constant { source: c, target: c }, &ArcWidths::Constant(2.0), None))
    }

    #[test]
    fn a_layer_id_names_one_layer_of_one_kind() {
        let mut l = ShellLayers::new();
        assert!(l.is_empty());
        l.set_arc_layer("x", arcs(), ArcStyle::default());
        assert_eq!(l.kind_of("x"), Some(ShellLayerKind::Arcs));
        assert_eq!(l.len_of("x"), Some(2));
        l.set_heatmap_layer("x", Arc::new(HeatmapSet::new(&[[0.0, 0.0]], None, &[])), HeatmapStyle::default());
        assert_eq!(l.kind_of("x"), Some(ShellLayerKind::Heatmap), "the heatmap took the name");
        assert_eq!(l.ids().len(), 1);
        assert!(l.remove("x"));
        assert!(!l.remove("x"));
        assert!(l.is_empty());
    }

    #[test]
    fn the_budget_is_the_desktops_divided_on_a_phone() {
        assert_eq!(SHELL_LAYER_BUDGET_BYTES * DIVISOR, DESKTOP_LAYER_BUDGET_BYTES);
        let mut l = ShellLayers::new();
        l.set_arc_layer("a", arcs(), ArcStyle::default());
        assert_eq!(l.data_bytes(), 64, "two 32-byte arcs");
        assert!(!l.over_budget());
    }
}
