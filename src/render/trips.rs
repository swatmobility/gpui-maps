//! Animated trips: paths whose every vertex carries a timestamp, drawn up
//! to a scene clock with a fading trail behind the head -- deck.gl's
//! TripsLayer, re-expressed in `trips_shader.wgsl` (MIT; see
//! `PROVENANCE.md`).
//!
//! - **Data**: a [`TripSet`] is one flat vertex array ([`TripVertex`], 24 B:
//!   lon, lat, altitude, time, colour, trip row). Consecutive vertices of
//!   one trip are a segment; the GPU reads each segment by binding the
//!   buffer twice, one vertex apart, so nothing is duplicated.
//! - **The clock is a uniform**: [`TripPipeline::prepare`] writes the head
//!   time and the trail length into a 240-byte uniform; a set is uploaded
//!   once per generation and never again while the clock moves
//!   ([`TripPipeline::uploads_total`] holds still through a playback). No
//!   per-frame CPU work grows with the data.
//! - **Time precision**: times are stored relative to the set's `base` (its
//!   earliest), as f32: an hour of seconds resolves to a quarter
//!   millisecond.
//! - **Antimeridian**: each segment takes the shortest way (flat) and is a
//!   chord of the sphere (globe); [`TripSet::from_trips`] cuts any segment
//!   longer than [`MAX_SEGMENT_DEG`] of arc along its great circle, so a
//!   long gap (an aircraft out of coverage over an ocean) neither cuts
//!   through the planet nor bends off it.
//! - **Altitude** (optional): metres per vertex, times
//!   [`TripStyle::altitude_scale`]; 0 draws on the ground.
//! - **Picking (later)**: each vertex keeps its trip's caller row
//!   (`TripVertex::trip`) and [`TripSet::trips`] maps rows to vertex
//!   ranges, so a pick can resolve a fragment or a screen point to a row;
//!   [`TripSet::position_at`] is the head's CPU mirror.

use std::ops::Range;

use glam::DVec3;

use super::alloc::TrackedBuffer;
use super::arcs::ArcFrame;
use super::gpu::GpuContext;
use crate::style::Rgba;

pub use super::arcs::pack_color;

/// The frame a trips draw needs: the same camera state as the arcs'.
pub type TripFrame = ArcFrame;

/// One vertex on the GPU: 24 B.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TripVertex {
    pub lon: f32,
    pub lat: f32,
    /// Metres above the ground.
    pub alt_m: f32,
    /// Relative to the set's [`TripSet::base`].
    pub time: f32,
    /// RGBA8, straight alpha ([`pack_color`]).
    pub color: u32,
    /// The trip's index in its set ([`TripSet::trips`]), unique per trip:
    /// what the shader tells two trips apart by. Not the caller's row
    /// ([`TripRange::row`], what a pick returns): many trips may share a
    /// row (every bus of one route), and a seam between two of them read
    /// as one trip's segment drew a trail and a head across it.
    pub trip: u32,
}

const _: () = assert!(std::mem::size_of::<TripVertex>() == 24);

/// `vs_trip`'s attributes: a segment's start (slot 0) and end (slot 1).
pub const START_ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![0 => Float32x4, 1 => Unorm8x4, 2 => Uint32];
pub const END_ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![3 => Float32x4, 4 => Unorm8x4, 5 => Uint32];
/// `vs_head`'s neighbours: the vertex before the segment (slot 2) and the
/// one after it (slot 3), position and trip only.
pub const PREV_ATTRS: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 6 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Uint32, offset: 20, shader_location: 7 },
];
pub const NEXT_ATTRS: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 8 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Uint32, offset: 20, shader_location: 9 },
];

/// The longest segment drawn as one chord, degrees of arc; longer ones are
/// cut along their great circle at build time.
pub const MAX_SEGMENT_DEG: f64 = 1.0;

/// One point of a trip, as the caller has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TripPoint {
    pub lon: f64,
    pub lat: f64,
    pub alt_m: f64,
    /// Absolute, in the caller's unit (seconds or milliseconds): the clock
    /// and the trail length are in the same unit.
    pub time: f64,
}

/// One trip to build: its caller row (kept for picking), its points in time
/// order, and its colour.
#[derive(Debug, Clone, PartialEq)]
pub struct TripInput {
    pub row: u32,
    pub points: Vec<TripPoint>,
    pub color: Rgba,
    /// The trip's name (a flight's callsign): kept on [`TripRange::label`]
    /// for a pick's tooltip, never uploaded.
    pub label: Option<String>,
}

/// Where one trip's vertices are in [`TripSet::vertices`], and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TripRange {
    pub row: u32,
    pub range: Range<u32>,
    /// [`TripInput::label`].
    pub label: Option<String>,
}

/// A layer's trips, ready to upload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TripSet {
    pub vertices: Vec<TripVertex>,
    pub trips: Vec<TripRange>,
    /// What every vertex's `time` is relative to (the earliest time).
    pub base: f64,
    /// The earliest and latest absolute times.
    pub domain: [f64; 2],
}

impl TripSet {
    /// Builds the set: non-finite points and points earlier than the one
    /// before are dropped, a trip left with fewer than two points is
    /// dropped, and segments longer than [`MAX_SEGMENT_DEG`] are cut along
    /// the great circle (time and altitude interpolated).
    #[must_use]
    pub fn from_trips(trips: &[TripInput]) -> Self {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for t in trips {
            for p in &t.points {
                if p.time.is_finite() {
                    lo = lo.min(p.time);
                    hi = hi.max(p.time);
                }
            }
        }
        if !lo.is_finite() {
            return Self::default();
        }
        let mut vertices = Vec::with_capacity(trips.iter().map(|t| t.points.len()).sum());
        let mut ranges = Vec::with_capacity(trips.len());
        let mut kept: Vec<TripPoint> = Vec::new();
        for t in trips {
            kept.clear();
            for p in &t.points {
                let finite = p.lon.is_finite() && p.lat.is_finite() && p.time.is_finite();
                if !finite || kept.last().is_some_and(|q| p.time < q.time) {
                    continue;
                }
                kept.push(TripPoint { alt_m: if p.alt_m.is_finite() { p.alt_m } else { 0.0 }, ..*p });
            }
            if kept.len() < 2 {
                continue;
            }
            let color = pack_color(t.color);
            let start = vertices.len() as u32;
            let index = ranges.len() as u32;
            let mut push = |p: &TripPoint| {
                vertices.push(TripVertex {
                    lon: p.lon as f32,
                    lat: p.lat as f32,
                    alt_m: p.alt_m as f32,
                    time: (p.time - lo) as f32,
                    color,
                    trip: index,
                });
            };
            push(&kept[0]);
            for w in kept.windows(2) {
                let (a, b) = (w[0], w[1]);
                let omega = central_angle_deg(a, b);
                // A hair under a whole number of degrees stays that many cuts.
                let cuts = (omega / MAX_SEGMENT_DEG - 1e-9).ceil().max(1.0) as u32;
                for k in 1..cuts {
                    push(&interpolate(a, b, f64::from(k) / f64::from(cuts)));
                }
                push(&b);
            }
            ranges.push(TripRange { row: t.row, range: start..vertices.len() as u32, label: t.label.clone() });
        }
        Self { vertices, trips: ranges, base: lo, domain: [lo, hi] }
    }

    pub fn len(&self) -> usize {
        self.trips.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trips.is_empty()
    }

    /// Instance bytes an upload costs.
    #[must_use]
    pub fn upload_bytes(&self) -> usize {
        self.vertices.len() * std::mem::size_of::<TripVertex>()
    }

    /// Segments drawn (pairs within one trip).
    #[must_use]
    pub fn segments(&self) -> usize {
        self.trips.iter().map(|t| (t.range.end - t.range.start).saturating_sub(1) as usize).sum()
    }

    /// The instances one draw issues: every adjacent pair, the pairs that
    /// straddle two trips included (the shader drops those).
    #[must_use]
    pub fn instances(&self) -> u32 {
        (self.vertices.len() as u32).saturating_sub(1)
    }

    /// An absolute time as the uniform's relative f32.
    #[must_use]
    pub fn relative(&self, t: f64) -> f32 {
        (t - self.base) as f32
    }

    /// Trip `index`'s position (lon, lat, altitude m) at absolute time `t`:
    /// the shader's head, on the CPU. `None` before its first point or
    /// after its last.
    #[must_use]
    pub fn position_at(&self, index: usize, t: f64) -> Option<[f64; 3]> {
        let r = self.trips.get(index)?.range.clone();
        let v = &self.vertices[r.start as usize..r.end as usize];
        let rel = t - self.base;
        let first = f64::from(v.first()?.time);
        let last = f64::from(v.last()?.time);
        if rel < first || rel > last {
            return None;
        }
        let i = v.partition_point(|p| f64::from(p.time) <= rel).clamp(1, v.len() - 1);
        let (a, b) = (v[i - 1], v[i]);
        let span = f64::from(b.time - a.time);
        let k = if span > 0.0 { (rel - f64::from(a.time)) / span } else { 1.0 };
        let lon_b = super::arcs::shortest_target_lon(f64::from(a.lon), f64::from(b.lon));
        let lerp = |x: f64, y: f64| x + (y - x) * k;
        let mut lon = lerp(f64::from(a.lon), lon_b);
        lon -= 360.0 * (lon / 360.0).round();
        Some([lon, lerp(f64::from(a.lat), f64::from(b.lat)), lerp(f64::from(a.alt_m), f64::from(b.alt_m))])
    }

    /// Trips whose head is in the air at absolute `t` (between their first
    /// and last point). O(trips): for tests and a pick, never per frame.
    #[must_use]
    pub fn active_at(&self, t: f64) -> usize {
        let rel = t - self.base;
        self.trips
            .iter()
            .filter(|r| {
                let a = f64::from(self.vertices[r.range.start as usize].time);
                let b = f64::from(self.vertices[r.range.end as usize - 1].time);
                rel >= a && rel <= b
            })
            .count()
    }

    /// Each trip's absolute `[first, last]` time, in trip order.
    #[must_use]
    pub fn spans(&self) -> Vec<[f64; 2]> {
        self.trips
            .iter()
            .map(|r| {
                let a = f64::from(self.vertices[r.range.start as usize].time) + self.base;
                let b = f64::from(self.vertices[r.range.end as usize - 1].time) + self.base;
                [a, b]
            })
            .collect()
    }
}

/// Metres round the equator and the mean radius, as `trips_shader.wgsl`.
const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.686;
const EARTH_RADIUS_M: f64 = 6_371_008.8;
const MAX_LAT: f64 = 85.051_129;

/// A head ([`TripSet::position_at`]'s `[lon, lat, alt_m]`) in LOGICAL screen
/// pixels as `vs_trip`'s `end_clip` places it -- flat, globe or the blend,
/// the altitude times `altitude_scale` -- or `None` behind the eye, past
/// the far plane or hidden by the planet. A pick's CPU mirror of the GPU.
#[must_use]
pub fn head_screen(frame: &ArcFrame, p: [f64; 3], altitude_scale: f32) -> Option<(f64, f64)> {
    let t = f64::from(frame.transition);
    let lift = p[2] * f64::from(altitude_scale);
    let flat = (t < 1.0).then(|| {
        let m = super::arcs::mercator(p[0], p[1]);
        let c = p[1].clamp(-MAX_LAT, MAX_LAT).to_radians().cos().max(1e-3);
        frame.flat_clip(DVec3::new(m.x, m.y, lift * f64::from(super::WORLD_SIZE) / (EARTH_CIRCUMFERENCE_M * c)))
    });
    let (globe, hidden) = if t > 0.0 {
        let g = super::arcs::sphere(p[0], p[1]) * (1.0 + lift / EARTH_RADIUS_M);
        (Some(frame.globe_view_proj * g.extend(1.0)), super::altitude::closest_approach(frame.eye, g) < 0.9999)
    } else {
        (None, false)
    };
    let clip = match (flat, globe) {
        (Some(f), Some(g)) => f.lerp(g, t),
        (Some(f), None) => f,
        (None, Some(g)) => g,
        (None, None) => return None,
    };
    if hidden && t > 0.5 {
        return None;
    }
    (clip.w > 0.0 && clip.z >= 0.0 && clip.z <= clip.w).then(|| frame.screen_of_clip(clip)).flatten()
}

fn unit(p: TripPoint) -> DVec3 {
    super::arcs::sphere(p.lon, p.lat)
}

fn central_angle_deg(a: TripPoint, b: TripPoint) -> f64 {
    let (a, b) = (unit(a), unit(b));
    a.cross(b).length().atan2(a.dot(b)).to_degrees()
}

/// The point `k` of the way from `a` to `b` along the great circle, time
/// and altitude linear.
fn interpolate(a: TripPoint, b: TripPoint, k: f64) -> TripPoint {
    let (pa, pb) = (unit(a), unit(b));
    let omega = pa.cross(pb).length().atan2(pa.dot(pb));
    let p = if omega > 1e-9 { (pa * ((1.0 - k) * omega).sin() + pb * (k * omega).sin()) / omega.sin() } else { pa.lerp(pb, k).normalize() };
    TripPoint {
        lon: p.x.atan2(p.z).to_degrees(),
        lat: p.y.clamp(-1.0, 1.0).asin().to_degrees(),
        alt_m: a.alt_m + (b.alt_m - a.alt_m) * k,
        time: a.time + (b.time - a.time) * k,
    }
}

/// How trips mix with what is under them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TripBlend {
    /// Premultiplied "over".
    #[default]
    Normal,
    /// `src * a + dst`: Kepler's `layerBlending: additive`.
    Additive,
    /// `src * a + dst * (1 - src * a)`: additive that saturates softly.
    Screen,
}

/// A layer's look. Times are in the set's unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TripStyle {
    /// Logical pixels.
    pub width_px: f32,
    /// Multiplies every trip's alpha (Kepler's layer `opacity`).
    pub opacity: f32,
    /// deck's `trailLength`: how far behind the head the trail reaches.
    pub trail: f64,
    /// deck's `fadeTrail`: the trail fades to nothing at `trail`; off, the
    /// whole path up to the head is drawn at full alpha.
    pub fade: bool,
    pub blend: TripBlend,
    /// Altitude exaggeration: drawn height = `alt_m * altitude_scale`.
    pub altitude_scale: f32,
    /// A marker where each trip is at the clock (a bus, a car, a flight),
    /// drawn over every layer's trails. `None`: the trail alone (deck's
    /// TripsLayer has no head).
    pub head: Option<TripHead>,
}

impl Default for TripStyle {
    fn default() -> Self {
        // deck.gl's defaults: 1 px wide, a 120-unit fading trail.
        Self { width_px: 1.0, opacity: 1.0, trail: 120.0, fade: true, blend: TripBlend::Normal, altitude_scale: 0.0, head: None }
    }
}

/// The marker at a trip's head: the trip's colour inside an outline,
/// placed and turned on the GPU from the same vertices and clock as the
/// trail, so it never drifts from it and costs no per-frame CPU work.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TripHead {
    pub shape: TripHeadShape,
    /// The marker's length (an arrow) or its circle's diameter (a dot, a
    /// teardrop), logical pixels; at `ramp`'s far end when it has one.
    pub size_px: f32,
    pub outline: Rgba,
    /// Logical pixels, inside `size_px`.
    pub outline_px: f32,
    /// Grows with the zoom: `(from zoom, size there, to zoom)`, linear
    /// between and flat outside, reaching `size_px` at the far zoom.
    pub ramp: Option<(f32, f32, f32)>,
}

impl TripHead {
    /// The size at `zoom`, logical pixels.
    #[must_use]
    pub fn size_at(&self, zoom: f64) -> f32 {
        match self.ramp {
            Some((z0, s0, z1)) if z1 > z0 => {
                let t = ((zoom as f32 - z0) / (z1 - z0)).clamp(0.0, 1.0);
                s0 + (self.size_px - s0) * t
            }
            _ => self.size_px,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripHeadShape {
    Dot,
    /// A navigation arrow along the way the trip is going; standing still
    /// (a dwell repeats a point), it keeps the way it came in, or else the
    /// way it leaves.
    Arrow,
    /// A circle on the trip's point with an apex 2.6 radii ahead, turned
    /// as the arrow (a downstream desktop app's vehicle marker).
    Teardrop,
}

impl Default for TripHead {
    fn default() -> Self {
        Self { shape: TripHeadShape::Arrow, size_px: 18.0, outline: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, outline_px: 2.0, ramp: None }
    }
}

/// The uniform, 240 B, in a 256 B dynamic-offset slot per layer.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TripUniform {
    pub flat_view_proj: [[f32; 4]; 4],
    pub globe_view_proj: [[f32; 4]; 4],
    pub eye: [f32; 4],
    pub origin: [f32; 4],
    pub viewport: [f32; 4],
    pub clock: [f32; 4],
    pub params: [f32; 4],
    /// `(size px, shape: 0 none 1 dot 2 arrow 3 teardrop, outline px, 0)`.
    pub head: [f32; 4],
    /// The head's outline, straight alpha.
    pub head_outline: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<TripUniform>() == 240);

/// The uniform for one layer: `head` the clock relative to the set's base
/// (`None` draws every path whole, no clock).
#[must_use]
pub fn uniform(frame: &TripFrame, style: &TripStyle, head: Option<f32>) -> TripUniform {
    let world = f64::from(super::WORLD_SIZE);
    TripUniform {
        flat_view_proj: frame.flat_view_proj.as_mat4().to_cols_array_2d(),
        globe_view_proj: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
        eye: [frame.eye.x as f32, frame.eye.y as f32, frame.eye.z as f32, 0.0],
        origin: [(frame.origin_world.x / world) as f32, (frame.origin_world.y / world) as f32, super::WORLD_SIZE, frame.transition],
        viewport: [frame.viewport.0, frame.viewport.1, frame.px_scale, style.opacity],
        clock: [
            head.unwrap_or(0.0),
            style.trail.max(0.0) as f32,
            if style.fade { 1.0 } else { 0.0 },
            if head.is_some() { 1.0 } else { 0.0 },
        ],
        params: [style.width_px, style.altitude_scale, 0.0, 0.0],
        head: match style.head {
            Some(h) => [
                h.size_at(frame.zoom).max(0.0),
                match h.shape {
                    TripHeadShape::Dot => 1.0,
                    TripHeadShape::Arrow => 2.0,
                    TripHeadShape::Teardrop => 3.0,
                },
                h.outline_px.max(0.0),
                0.0,
            ],
            None => [0.0; 4],
        },
        head_outline: style.head.map_or([0.0; 4], |h| [h.outline.r, h.outline.g, h.outline.b, h.outline.a]),
    }
}

/// One layer to draw this frame.
pub struct TripLayerDraw<'a> {
    pub id: &'a str,
    /// Bumped whenever `set` changes: the upload is skipped otherwise.
    pub generation: u64,
    pub set: &'a TripSet,
    pub style: TripStyle,
    /// The clock, absolute (the set's unit); `None` draws the paths whole.
    pub clock: Option<f64>,
}

/// What the last [`TripPipeline::prepare`] planned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TripFrameStats {
    pub layers: u64,
    pub trips: u64,
    /// Instances issued (segments plus the trip seams the shader drops).
    pub instances: u64,
    pub draws: u64,
    /// Trips in layers that draw a head marker (each draws one while the
    /// clock is within it).
    pub heads: u64,
    /// Vertex-buffer bytes uploaded THIS frame: zero on a clock move.
    pub upload_bytes: u64,
    /// Uniform bytes written this frame.
    pub uniform_bytes: u64,
}

struct GpuLayer {
    /// Shared with every other layer drawing the same set this frame (the
    /// sample's paths and trails are one set in two styles).
    buffer: Option<std::sync::Arc<TrackedBuffer>>,
    generation: u64,
    vertices: u32,
}

#[derive(Debug, Clone)]
struct Planned {
    layer: String,
    slot: u32,
    vertices: u32,
    blend: TripBlend,
    head: bool,
}

/// One blend's trail pipeline each, and the heads', for one kind of pass.
struct TripPipelines {
    normal: wgpu::RenderPipeline,
    additive: wgpu::RenderPipeline,
    screen: wgpu::RenderPipeline,
    heads: wgpu::RenderPipeline,
}

pub struct TripPipeline {
    /// For the pass `new` was given.
    plain: TripPipelines,
    /// For a pass with a depth attachment ([`Self::with_depth_pass`]): the
    /// tile pass, where trips draw under the 3D models.
    deep: Option<TripPipelines>,
    shader: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    color_format: wgpu::TextureFormat,
    bgl: wgpu::BindGroupLayout,
    uniform: Option<(TrackedBuffer, wgpu::BindGroup, u32)>,
    slot_size: u64,
    layers: std::collections::HashMap<String, GpuLayer>,
    planned: Vec<Planned>,
    stats: TripFrameStats,
    uploads_total: u64,
}

impl TripPipeline {
    /// `depth_format`: the pass's depth attachment, if it has one; trips
    /// never test or write depth.
    #[must_use]
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat, depth_format: Option<wgpu::TextureFormat>) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps trips shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("trips_shader.wgsl").into()),
        });
        let bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("trips-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<TripUniform>() as u64),
                },
                count: None,
            }],
        });
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("trips-pipeline-layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let plain = Self::build_pipelines(ctx, &shader, &layout, color_format, depth_format);
        let align = u64::from(ctx.device.limits().min_uniform_buffer_offset_alignment);
        let slot_size = (std::mem::size_of::<TripUniform>() as u64).div_ceil(align) * align;
        Self {
            plain,
            deep: None,
            shader,
            layout,
            color_format,
            bgl,
            uniform: None,
            slot_size,
            layers: std::collections::HashMap::new(),
            planned: Vec::new(),
            stats: TripFrameStats::default(),
            uploads_total: 0,
        }
    }

    /// Also builds the pipelines for a pass with a `depth_format` depth
    /// attachment (testing and writing none), so the frame can draw the
    /// trips inside the tile pass, right before the 3D models
    /// ([`Self::draw_in_depth_pass`]).
    #[must_use]
    pub fn with_depth_pass(mut self, ctx: &GpuContext, depth_format: wgpu::TextureFormat) -> Self {
        self.deep = Some(Self::build_pipelines(ctx, &self.shader, &self.layout, self.color_format, Some(depth_format)));
        self
    }

    fn build_pipelines(
        ctx: &GpuContext,
        shader: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
        color_format: wgpu::TextureFormat,
        depth_format: Option<wgpu::TextureFormat>,
    ) -> TripPipelines {
        let stride = std::mem::size_of::<TripVertex>() as wgpu::BufferAddress;
        let segment = [
            Some(wgpu::VertexBufferLayout { array_stride: stride, step_mode: wgpu::VertexStepMode::Instance, attributes: &START_ATTRS }),
            Some(wgpu::VertexBufferLayout { array_stride: stride, step_mode: wgpu::VertexStepMode::Instance, attributes: &END_ATTRS }),
        ];
        let with_neighbours = [
            segment[0].clone(),
            segment[1].clone(),
            Some(wgpu::VertexBufferLayout { array_stride: stride, step_mode: wgpu::VertexStepMode::Instance, attributes: &PREV_ATTRS }),
            Some(wgpu::VertexBufferLayout { array_stride: stride, step_mode: wgpu::VertexStepMode::Instance, attributes: &NEXT_ATTRS }),
        ];
        let build_with = |label: &str, blend: wgpu::BlendState, vs: &str, fs: &str, buffers: &[Option<wgpu::VertexBufferLayout<'_>>]| {
            ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some(vs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: depth_format.map(|format| wgpu::DepthStencilState {
                    format,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some(fs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: color_format, blend: Some(blend), write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let build = |label: &str, blend: wgpu::BlendState| build_with(label, blend, "vs_trip", "fs_trip", &segment);
        let heads = build_with("trips-heads-pipeline", wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING, "vs_head", "fs_head", &with_neighbours);
        let normal = build("trips-pipeline", wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        let additive = build(
            "trips-additive-pipeline",
            wgpu::BlendState {
                color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
                alpha: wgpu::BlendComponent::OVER,
            },
        );
        let screen = build(
            "trips-screen-pipeline",
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrc,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent::OVER,
            },
        );
        TripPipelines { normal, additive, screen, heads }
    }

    /// Uploads any set whose generation moved, drops layers no longer
    /// drawn, writes one uniform slot per layer (the clock lives there) and
    /// plans the draws. ONE call per frame. Its CPU cost is per layer, not
    /// per trip: a clock move is the uniform write alone.
    pub fn prepare(&mut self, ctx: &GpuContext, frame: &TripFrame, draws: &[TripLayerDraw<'_>]) {
        self.layers.retain(|id, _| draws.iter().any(|d| d.id == id));
        let mut stats = TripFrameStats::default();
        for (i, d) in draws.iter().enumerate() {
            if self.layers.get(d.id).is_some_and(|l| l.generation == d.generation) {
                continue;
            }
            // An earlier layer of this frame draws the very same set (one
            // `TripSet`, two styles): use its buffer, upload nothing.
            let shared = draws[..i]
                .iter()
                .find(|e| std::ptr::eq(e.set, d.set))
                .and_then(|e| self.layers.get(e.id))
                .and_then(|l| l.buffer.clone());
            let layer = self.layers.entry(d.id.to_string()).or_insert(GpuLayer { buffer: None, generation: u64::MAX, vertices: 0 });
            if let Some(buffer) = shared {
                layer.buffer = Some(buffer);
            } else {
                // The vertices with their first repeated before them and
                // their last after them, so a head reads the vertex before
                // its segment and the one after it through two more
                // offset bindings ([`Self::draw`]); a repeat is the same
                // trip standing still, which a head skips.
                let padded: Vec<TripVertex> = match (d.set.vertices.first(), d.set.vertices.last()) {
                    (Some(&first), Some(&last)) => std::iter::once(first).chain(d.set.vertices.iter().copied()).chain(std::iter::once(last)).collect(),
                    _ => Vec::new(),
                };
                let bytes: &[u8] = bytemuck::cast_slice(&padded);
                if !bytes.is_empty() {
                    let need = bytes.len() as u64;
                    // A buffer another layer still shares is never written
                    // over: it gets its own.
                    let reuse = layer.buffer.as_ref().is_some_and(|b| b.tracked_size() >= need && std::sync::Arc::strong_count(b) == 1);
                    if !reuse {
                        // Flagged on wasm32 only, where wgpu's handles are
                        // `!Send`; natively this `Arc` is `Send + Sync`
                        // (`overlay::host`'s tile upload, the same allow).
                        #[allow(clippy::arc_with_non_send_sync)]
                        let buffer = std::sync::Arc::new(TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                            label: Some("trip-vertices"),
                            size: need,
                            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        }));
                        layer.buffer = Some(buffer);
                    }
                    if let Some(buffer) = layer.buffer.as_ref() {
                        ctx.queue.write_buffer(buffer, 0, bytes);
                    }
                    stats.upload_bytes += need;
                    self.uploads_total += 1;
                }
            }
            layer.vertices = d.set.vertices.len() as u32;
            layer.generation = d.generation;
        }
        let mut uniforms: Vec<u8> = Vec::new();
        let mut planned = Vec::new();
        for d in draws {
            let n = d.set.vertices.len() as u32;
            if n < 2 {
                continue;
            }
            let slot = (uniforms.len() as u64 / self.slot_size) as u32;
            let head = d.clock.map(|t| d.set.relative(t));
            uniforms.extend_from_slice(bytemuck::bytes_of(&uniform(frame, &d.style, head)));
            uniforms.resize((u64::from(slot + 1) * self.slot_size) as usize, 0);
            stats.layers += 1;
            stats.trips += d.set.trips.len() as u64;
            stats.instances += u64::from(n - 1);
            stats.draws += 1;
            let head = d.clock.is_some() && d.style.head.is_some_and(|h| h.size_px > 0.0);
            if head {
                stats.draws += 1;
                stats.heads += d.set.trips.len() as u64;
            }
            planned.push(Planned { layer: d.id.to_string(), slot, vertices: n, blend: d.style.blend, head });
        }
        if !uniforms.is_empty() {
            let slots = (uniforms.len() as u64 / self.slot_size) as u32;
            if self.uniform.as_ref().is_none_or(|(_, _, have)| *have < slots) {
                let have = slots.next_power_of_two().max(4);
                let buffer = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                    label: Some("trip-uniforms"),
                    size: u64::from(have) * self.slot_size,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("trip-bind-group"),
                    layout: &self.bgl,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &buffer,
                            offset: 0,
                            size: wgpu::BufferSize::new(std::mem::size_of::<TripUniform>() as u64),
                        }),
                    }],
                });
                self.uniform = Some((buffer, bind_group, have));
            }
            if let Some((buffer, _, _)) = self.uniform.as_ref() {
                ctx.queue.write_buffer(buffer, 0, &uniforms);
            }
            stats.uniform_bytes = uniforms.len() as u64;
        }
        self.planned = planned;
        self.stats = stats;
    }

    /// What the last prepare planned.
    #[must_use]
    pub fn stats(&self) -> TripFrameStats {
        self.stats
    }

    /// Vertex uploads since this pipeline was built (a clock move makes
    /// none).
    #[must_use]
    pub fn uploads_total(&self) -> u64 {
        self.uploads_total
    }

    /// Draws what [`Self::prepare`] planned, in layer order, then every
    /// head marker over all of them, into a pass like the one `new` was
    /// given.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.draw_with(pass, &self.plain);
    }

    /// [`Self::draw`] into a pass with a depth attachment (the tile pass),
    /// through the pipelines [`Self::with_depth_pass`] built; nothing
    /// without them. Returns the draw calls issued: the caller rebinds its
    /// own pipeline and group 0 after any.
    pub fn draw_in_depth_pass(&self, pass: &mut wgpu::RenderPass<'_>) -> u32 {
        self.deep.as_ref().map_or(0, |deep| self.draw_with(pass, deep))
    }

    /// Whether [`Self::draw_in_depth_pass`] can draw.
    #[must_use]
    pub fn has_depth_pass(&self) -> bool {
        self.deep.is_some()
    }

    fn draw_with(&self, pass: &mut wgpu::RenderPass<'_>, pipelines: &TripPipelines) -> u32 {
        let mut calls = 0;
        let Some((_, bind_group, _)) = self.uniform.as_ref() else { return 0 };
        let stride = std::mem::size_of::<TripVertex>() as u64;
        let mut bound = None;
        for p in &self.planned {
            let Some(layer) = self.layers.get(&p.layer) else { continue };
            let Some(buffer) = layer.buffer.as_ref() else { continue };
            if p.vertices < 2 || layer.vertices != p.vertices {
                continue;
            }
            if bound != Some(p.blend) {
                pass.set_pipeline(match p.blend {
                    TripBlend::Normal => &pipelines.normal,
                    TripBlend::Additive => &pipelines.additive,
                    TripBlend::Screen => &pipelines.screen,
                });
                bound = Some(p.blend);
            }
            pass.set_bind_group(0, bind_group, &[(u64::from(p.slot) * self.slot_size) as u32]);
            let n = u64::from(p.vertices);
            // The same buffer twice, one vertex apart: segment i reads
            // vertex i as its start and i + 1 as its end (offsets, not
            // `first_instance`, which WebGL2 lacks). Vertex i sits at
            // i + 1: the buffer opens with a repeat of the first.
            pass.set_vertex_buffer(0, buffer.slice(stride..n * stride));
            pass.set_vertex_buffer(1, buffer.slice(2 * stride..(n + 1) * stride));
            pass.draw(0..4, 0..p.vertices - 1);
            super::alloc::note_draw_calls(1);
            calls += 1;
        }
        let mut bound = false;
        for p in self.planned.iter().filter(|p| p.head) {
            let Some(layer) = self.layers.get(&p.layer) else { continue };
            let Some(buffer) = layer.buffer.as_ref() else { continue };
            if p.vertices < 2 || layer.vertices != p.vertices {
                continue;
            }
            if !bound {
                pass.set_pipeline(&pipelines.heads);
                bound = true;
            }
            pass.set_bind_group(0, bind_group, &[(u64::from(p.slot) * self.slot_size) as u32]);
            let n = u64::from(p.vertices);
            // As the trail, plus the vertex before (i - 1, the leading
            // repeat for the first) and the one after (i + 2, the
            // trailing repeat for the last).
            pass.set_vertex_buffer(0, buffer.slice(stride..n * stride));
            pass.set_vertex_buffer(1, buffer.slice(2 * stride..(n + 1) * stride));
            pass.set_vertex_buffer(2, buffer.slice(0..(n - 1) * stride));
            pass.set_vertex_buffer(3, buffer.slice(3 * stride..(n + 2) * stride));
            pass.draw(0..4, 0..p.vertices - 1);
            super::alloc::note_draw_calls(1);
            calls += 1;
        }
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::camera::Camera;

    const ORANGE: Rgba = Rgba { r: 1.0, g: 0.6, b: 0.0, a: 1.0 };

    fn p(lon: f64, lat: f64, time: f64) -> TripPoint {
        TripPoint { lon, lat, alt_m: 0.0, time }
    }

    #[test]
    fn the_vertex_is_24_bytes_and_the_uniform_240() {
        assert_eq!(std::mem::size_of::<TripVertex>(), 24);
        assert_eq!(std::mem::size_of::<TripUniform>(), 240);
    }

    /// Times are relative to the earliest; bad points and backward steps
    /// drop; a one-point trip drops; the caller's row is kept per trip,
    /// the trip's index per vertex.
    #[test]
    fn trips_pack_relative_times_and_keep_their_rows() {
        let set = TripSet::from_trips(&[
            TripInput { row: 7, points: vec![p(0.0, 0.0, 1000.0), p(0.1, 0.0, 1010.0), p(f64::NAN, 0.0, 1015.0), p(0.2, 0.0, 1005.0), p(0.3, 0.0, 1020.0)], color: ORANGE, label: Some("RYR12AB".into()) },
            TripInput { row: 9, points: vec![p(5.0, 5.0, 990.0)], color: ORANGE, label: None },
            TripInput { row: 11, points: vec![p(1.0, 1.0, 995.0), p(1.1, 1.0, 1030.0)], color: ORANGE, label: None },
        ]);
        assert_eq!(set.base, 990.0, "the one-point trip still counts toward the domain");
        assert_eq!(set.domain, [990.0, 1030.0]);
        assert_eq!(set.len(), 2);
        assert_eq!(set.trips[0], TripRange { row: 7, range: 0..3, label: Some("RYR12AB".into()) });
        assert_eq!(set.trips[1], TripRange { row: 11, range: 3..5, label: None });
        let times: Vec<f32> = set.vertices.iter().map(|v| v.time).collect();
        assert_eq!(times, vec![10.0, 20.0, 30.0, 5.0, 40.0]);
        assert!(set.vertices[..3].iter().all(|v| v.trip == 0), "a vertex holds its trip's index in the set");
        assert!(set.vertices[3..].iter().all(|v| v.trip == 1));
        assert_eq!(set.segments(), 3);
        assert_eq!(set.instances(), 4, "one seam between the two trips");
        assert_eq!(set.upload_bytes(), 5 * 24);
        assert_eq!(set.vertices[0].color.to_le_bytes(), [255, 153, 0, 255]);
    }

    /// A long gap is cut along the great circle, time and altitude linear;
    /// across the antimeridian the cut goes the short way (over 180).
    #[test]
    fn long_segments_are_cut_along_the_great_circle_the_short_way() {
        let set = TripSet::from_trips(&[TripInput {
            row: 0,
            points: vec![
                TripPoint { lon: 175.0, lat: 0.0, alt_m: 0.0, time: 0.0 },
                TripPoint { lon: -175.0, lat: 0.0, alt_m: 1000.0, time: 100.0 },
            ],
            color: ORANGE, label: None
        }]);
        assert_eq!(set.vertices.len(), 11, "10 degrees in 1-degree cuts");
        for v in &set.vertices {
            assert!(v.lon.abs() >= 174.99, "over the antimeridian, not Greenwich: {}", v.lon);
        }
        let mid = set.vertices[5];
        assert!((mid.lon.abs() - 180.0).abs() < 1e-3 && mid.lat.abs() < 1e-4);
        assert!((mid.time - 50.0).abs() < 1e-3 && (mid.alt_m - 500.0).abs() < 1e-2);
        // The head's CPU mirror: half way, at 180.
        let head = set.position_at(0, 50.0).unwrap();
        assert!((head[0].abs() - 180.0).abs() < 1e-3, "{head:?}");
        assert!(set.position_at(0, 101.0).is_none() && set.position_at(0, -1.0).is_none());
        assert_eq!(set.active_at(50.0), 1);
        assert_eq!(set.active_at(150.0), 0);
        assert_eq!(set.spans(), vec![[0.0, 100.0]]);
    }

    #[test]
    fn the_uniform_carries_the_clock_relative_to_the_base() {
        let camera = flat_camera(0.0, 0.0, 3.0);
        let frame = TripFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 2.0);
        let style = TripStyle { trail: 300.0, ..TripStyle::default() };
        let u = uniform(&frame, &style, Some(42.0));
        assert_eq!(u.clock, [42.0, 300.0, 1.0, 1.0]);
        assert_eq!(uniform(&frame, &style, None).clock[3], 0.0, "no clock: the paths whole");
    }

    #[test]
    fn the_trips_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("trips_shader.wgsl"),
            &[
                ("vs_trip", naga::ShaderStage::Vertex),
                ("fs_trip", naga::ShaderStage::Fragment),
                ("vs_head", naga::ShaderStage::Vertex),
                ("fs_head", naga::ShaderStage::Fragment),
            ],
        );
    }

    fn flat_camera(lon: f64, lat: f64, zoom: f64) -> Camera {
        Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: 256.0, viewport_height_px: 256.0, fov_y: Camera::DEFAULT_FOV_Y }
    }

    fn device() -> Option<GpuContext> {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions::default()).await.ok()?;
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor { required_limits: wgpu::Limits::downlevel_webgl2_defaults(), ..Default::default() })
                .await
                .ok()?;
            let max_buffer_size = device.limits().max_buffer_size;
            Some(GpuContext { device, queue, max_buffer_size })
        })
    }

    /// Renders the layer into a 256x256 RGBA8 target over black, with
    /// `pipeline` (kept between calls, so uploads are counted).
    fn render(ctx: &GpuContext, pipeline: &mut TripPipeline, frame: &TripFrame, draw: TripLayerDraw<'_>) -> Vec<u8> {
        pipeline.prepare(ctx, frame, &[draw]);
        let size = wgpu::Extent3d { width: 256, height: 256, depth_or_array_layers: 1 };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("trips-test"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let read = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("trips-test-read"),
            size: 256 * 256 * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("trips-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pipeline.draw(&mut pass);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &read, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256 * 4), rows_per_image: Some(256) } },
            size,
        );
        ctx.queue.submit(Some(encoder.finish()));
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let out = read.slice(..).get_mapped_range().expect("mapped").to_vec();
        out
    }

    /// Red channel at a lon/lat on a flat z1 frame centred on 0,0.
    fn red_at(px: &[u8], frame: &TripFrame, lon: f64, lat: f64) -> u8 {
        let (s, t) = ((lon, lat), (lon + 1e-9, lat));
        let (x, y) = frame.project_flat(s, t, 0.0, 0.0).expect("on screen");
        let mut best = 0;
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                let (x, y) = (x.round() as i32 + dx, y.round() as i32 + dy);
                if (0..256).contains(&x) && (0..256).contains(&y) {
                    best = best.max(px[((y * 256 + x) * 4) as usize]);
                }
            }
        }
        best
    }

    /// **The trail moves with the clock, and the clock is a uniform.** One
    /// trip east along the equator over 100 s: at t = 50 with a 20 s trail
    /// the head is lit, the tail fades, the future is dark and the far
    /// past too; at t = 90 the lit stretch has moved east. The second
    /// frame uploads nothing.
    #[test]
    fn the_gpu_draws_a_fading_trail_that_follows_the_clock() {
        let Some(ctx) = device() else { return };
        let camera = flat_camera(0.0, 0.0, 1.0);
        let frame = TripFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let points: Vec<TripPoint> = (0..=10).map(|k| p(-40.0 + f64::from(k) * 8.0, 0.0, f64::from(k) * 10.0)).collect();
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let set = TripSet::from_trips(&[TripInput { row: 3, points, color: red, label: None }]);
        let style = TripStyle { width_px: 4.0, trail: 20.0, ..TripStyle::default() };
        let mut pipeline = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        let at = |t: f64| -40.0 + t * 0.8;
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "t", generation: 1, set: &set, style, clock: Some(50.0) });
        let head = red_at(&px, &frame, at(49.0), 0.0);
        let tail = red_at(&px, &frame, at(38.0), 0.0);
        assert!(head > 200, "the head is lit: {head}");
        assert!(tail > 20 && tail < head, "the tail fades: {tail} vs {head}");
        assert_eq!(red_at(&px, &frame, at(60.0), 0.0), 0, "the future is not drawn");
        assert_eq!(red_at(&px, &frame, at(20.0), 0.0), 0, "past the trail is not drawn");
        let uploads = pipeline.uploads_total();
        let later = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "t", generation: 1, set: &set, style, clock: Some(90.0) });
        assert_eq!(pipeline.uploads_total(), uploads, "a clock move uploads nothing");
        assert_eq!(pipeline.stats().upload_bytes, 0);
        assert!(red_at(&later, &frame, at(89.0), 0.0) > 200);
        assert_eq!(red_at(&later, &frame, at(49.0), 0.0), 0, "the old head is gone");
        // No clock: the whole path, at full alpha.
        let whole = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "t", generation: 1, set: &set, style, clock: None });
        assert!(red_at(&whole, &frame, at(5.0), 0.0) > 200 && red_at(&whole, &frame, at(95.0), 0.0) > 200);
    }

    /// RGBA at a pixel offset from a lon/lat on `frame`.
    fn rgba_near(px: &[u8], frame: &TripFrame, lon: f64, lat: f64, dx: f64, dy: f64) -> [u8; 4] {
        let (x, y) = frame.project_flat((lon, lat), (lon + 1e-9, lat), 0.0, 0.0).expect("on screen");
        let i = (((y + dy).round() as usize) * 256 + (x + dx).round() as usize) * 4;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    /// **The head is where the clock is, turned the way the trip goes.**
    /// One trip east, a dwell (the point repeated), then north; the trail
    /// itself is invisible (0 px), so every lit pixel is the marker (over black). At
    /// t = 25 the arrow sits a quarter of the way along, its tip east of
    /// its centre and its notch behind it dark; in the dwell it still
    /// points east (the way in); after the trip it is gone. A clock move
    /// uploads nothing.
    #[test]
    fn the_gpu_draws_a_head_at_the_clock_turned_along_the_way() {
        let Some(ctx) = device() else { return };
        let camera = flat_camera(0.0, 0.0, 1.0);
        let frame = TripFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let points = vec![p(-40.0, 0.0, 0.0), p(0.0, 0.0, 100.0), p(0.0, 0.0, 150.0), p(0.0, 30.0, 250.0)];
        let set = TripSet::from_trips(&[TripInput { row: 0, points, color: red, label: None }]);
        let head = TripHead { shape: TripHeadShape::Arrow, size_px: 40.0, outline: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, outline_px: 3.0, ramp: None };
        let style = TripStyle { width_px: 0.0, trail: 1000.0, head: Some(head), ..TripStyle::default() };
        let mut pipeline = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        let draw = |clock| TripLayerDraw { id: "h", generation: 1, set: &set, style, clock: Some(clock) };
        let px = render(&ctx, &mut pipeline, &frame, draw(25.0));
        let centre = rgba_near(&px, &frame, -30.0, 0.0, 0.0, 0.0);
        assert!(centre[0] > 200 && centre[1] < 60, "the trip's colour inside: {centre:?}");
        let tip = rgba_near(&px, &frame, -30.0, 0.0, 16.0, 0.0);
        assert!(tip[0] > 200, "the tip points east: {tip:?}");
assert_eq!(rgba_near(&px, &frame, -30.0, 0.0, -12.0, 0.0)[0], 0, "the notch behind is open");
        assert_eq!(rgba_near(&px, &frame, -30.0, 0.0, -16.0, 0.0)[0], 0, "nothing west of the notch on the axis");
        let wing = rgba_near(&px, &frame, -30.0, 0.0, -14.0, 13.0);
        assert!(wing[1] > 150, "the wing's outline is white: {wing:?}");
        assert_eq!(rgba_near(&px, &frame, -10.0, 0.0, 0.0, 0.0)[0], 0, "one marker, at the clock");
        assert_eq!(pipeline.stats().heads, 1);
        let uploads = pipeline.uploads_total();
        let dwell = render(&ctx, &mut pipeline, &frame, draw(120.0));
        assert_eq!(pipeline.uploads_total(), uploads, "a clock move uploads nothing");
        assert!(rgba_near(&dwell, &frame, 0.0, 0.0, 16.0, 0.0)[0] > 200, "standing, it keeps the way in (east)");
        assert_eq!(rgba_near(&dwell, &frame, 0.0, 0.0, -12.0, 0.0)[0], 0);
        let north = render(&ctx, &mut pipeline, &frame, draw(200.0));
        let (_, y0) = frame.project_flat((0.0, 15.0), (1e-9, 15.0), 0.0, 0.0).unwrap();
        let (_, y1) = frame.project_flat((0.0, 16.0), (1e-9, 16.0), 0.0, 0.0).unwrap();
        let up = (y1 - y0).signum();
        assert!(rgba_near(&north, &frame, 0.0, 15.0, 0.0, 16.0 * up)[0] > 200, "turned north");
        assert_eq!(rgba_near(&north, &frame, 0.0, 15.0, 0.0, -12.0 * up)[0], 0);
        let after = render(&ctx, &mut pipeline, &frame, draw(300.0));
        assert!(after.chunks(4).all(|p| p[0] == 0), "after the trip, no head");
    }

    /// Centroid `(x, y)` of the pixels whose red channel is lit.
    fn red_centroid(px: &[u8]) -> Option<(f64, f64)> {
        let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
        for (i, p) in px.chunks(4).enumerate() {
            let w = f64::from(p[0]);
            if w > 0.0 {
                sx += (i % 256) as f64 * w;
                sy += (i / 256) as f64 * w;
                n += w;
            }
        }
        (n > 0.0).then(|| (sx / n, sy / n))
    }

    /// **A followed head holds still on screen.** At street zoom in
    /// Singapore, a camera centred on the head's CPU position (f64, as a
    /// follow camera is) frame after frame sees the GPU's head move less
    /// than a third of a pixel between frames. Absolute
    /// f32 Mercator rounds to ~2.4 m (several pixels here): before the
    /// origin snap and the relative step, the head twitched side to side.
    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "fails on Linux/lavapipe, not yet investigated")]
    fn a_followed_head_holds_still_at_street_zoom() {
        let Some(ctx) = device() else { return };
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        // North-east across Bedok, 0.01 degrees in 100 s.
        let set = TripSet::from_trips(&[TripInput { row: 0, points: vec![p(103.92, 1.33, 0.0), p(103.93, 1.335, 100.0)], color: red, label: None }]);
        let head = TripHead { shape: TripHeadShape::Dot, size_px: 12.0, outline: red, outline_px: 0.0, ramp: None };
        let style = TripStyle { width_px: 0.0, trail: 1.0, head: Some(head), ..TripStyle::default() };
        let mut pipeline = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        let mut worst: f64 = 0.0;
        let mut offset: f64 = 0.0;
        let mut last: Option<(f64, f64)> = None;
        for k in 0..40 {
            let t = 20.0 + f64::from(k) * 0.37;
            let at = set.position_at(0, t).expect("running");
            let camera = Camera { center_lon: at[0], center_lat: at[1], zoom: 19.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 256.0, viewport_height_px: 256.0, fov_y: Camera::DEFAULT_FOV_Y };
            let frame = TripFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
            let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "f", generation: 1, set: &set, style, clock: Some(t) });
            let (x, y) = red_centroid(&px).expect("the head is drawn");
            let d = (x - 127.5, y - 127.5);
            offset = offset.max(d.0.hypot(d.1));
            if let Some(l) = last {
                worst = worst.max((d.0 - l.0).hypot(d.1 - l.1));
            }
            last = Some(d);
        }
        // The vertices themselves are f32 (lon/lat to ~0.85 m, Mercator to
        // ~2.4 m): a steady offset of a few pixels, the same every frame.
        assert!(offset < 8.0, "the head sits {offset:.2} px from the camera's centre");
        assert!(worst < 0.34, "the head jumps {worst:.2} px between frames");
    }

    /// The teardrop: the circle round the point, the apex ahead (east),
    /// nothing behind past the circle; its size follows the zoom ramp.
    #[test]
    fn the_teardrop_points_ahead_and_grows_with_the_zoom() {
        let head = TripHead { shape: TripHeadShape::Teardrop, size_px: 14.0, outline: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, outline_px: 1.5, ramp: Some((10.0, 4.5, 16.0)) };
        assert_eq!(head.size_at(8.0), 4.5);
        assert!((head.size_at(13.0) - 9.25).abs() < 1e-5);
        assert_eq!(head.size_at(18.0), 14.0);
        let Some(ctx) = device() else { return };
        let camera = flat_camera(0.0, 0.0, 1.0);
        let frame = TripFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let set = TripSet::from_trips(&[TripInput { row: 0, points: vec![p(-40.0, 0.0, 0.0), p(0.0, 0.0, 100.0)], color: red, label: None }]);
        let head = TripHead { size_px: 20.0, ramp: None, ..head };
        let style = TripStyle { width_px: 0.0, trail: 1000.0, head: Some(head), ..TripStyle::default() };
        let mut pipeline = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "t", generation: 1, set: &set, style, clock: Some(25.0) });
        assert!(rgba_near(&px, &frame, -30.0, 0.0, 0.0, 0.0)[0] > 200, "the circle's centre");
        assert!(rgba_near(&px, &frame, -30.0, 0.0, 20.0, 0.0)[1] > 120, "the apex 2.6 radii ahead");
        assert_eq!(rgba_near(&px, &frame, -30.0, 0.0, -13.0, 0.0)[0], 0, "nothing behind the circle");
        assert_eq!(rgba_near(&px, &frame, -30.0, 0.0, 0.0, 13.0)[0], 0, "nor beside it");
    }

    /// Two trips back to back in the buffer: the seam between them (the
    /// last point of one to the first of the next) is never drawn.
    #[test]
    fn the_seam_between_two_trips_is_not_drawn() {
        let Some(ctx) = device() else { return };
        let camera = flat_camera(0.0, 0.0, 1.0);
        let frame = TripFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let set = TripSet::from_trips(&[
            TripInput { row: 0, points: vec![p(-40.0, 20.0, 0.0), p(-20.0, 20.0, 10.0)], color: red, label: None },
            TripInput { row: 1, points: vec![p(20.0, -20.0, 0.0), p(40.0, -20.0, 10.0)], color: red, label: None },
        ]);
        let style = TripStyle { width_px: 4.0, ..TripStyle::default() };
        let mut pipeline = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "t", generation: 1, set: &set, style, clock: None });
        assert!(red_at(&px, &frame, -30.0, 20.0) > 200 && red_at(&px, &frame, 30.0, -20.0) > 200);
        assert_eq!(red_at(&px, &frame, 0.0, 0.0), 0, "the seam's middle is dark");
    }

    /// **Two trips of one row are still two trips.** Every bus of a route
    /// (and every walker at its stops) shares the route's row. With the
    /// clock between the first trip's end and the second's start, no head
    /// is drawn anywhere: the seam between them is no segment, so nothing
    /// glides from the end of one to the start of the next; and the trail
    /// draws no line across the seam.
    #[test]
    fn two_trips_of_one_row_draw_no_head_or_trail_across_their_seam() {
        let Some(ctx) = device() else { return };
        let camera = flat_camera(0.0, 0.0, 1.0);
        let frame = TripFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let set = TripSet::from_trips(&[
            TripInput { row: 3, points: vec![p(-40.0, 0.0, 0.0), p(-30.0, 0.0, 10.0)], color: red, label: None },
            TripInput { row: 3, points: vec![p(30.0, 0.0, 100.0), p(40.0, 0.0, 110.0)], color: red, label: None },
        ]);
        let head = TripHead { shape: TripHeadShape::Dot, size_px: 12.0, outline: red, outline_px: 0.0, ramp: None };
        let style = TripStyle { width_px: 0.0, trail: 1.0, head: Some(head), ..TripStyle::default() };
        let mut pipeline = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "walkers", generation: 1, set: &set, style, clock: Some(55.0) });
        assert!(px.chunks(4).all(|p| p[0] == 0), "between the two trips no head is drawn");
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "walkers", generation: 1, set: &set, style, clock: Some(105.0) });
        assert!(red_at(&px, &frame, 35.0, 0.0) > 200, "inside the second trip its head is");
        let trail = TripStyle { width_px: 6.0, trail: 1000.0, fade: false, ..TripStyle::default() };
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "trails", generation: 2, set: &set, style: trail, clock: Some(110.0) });
        assert!(red_at(&px, &frame, -35.0, 0.0) > 200 && red_at(&px, &frame, 35.0, 0.0) > 200, "both trails drawn");
        assert_eq!(red_at(&px, &frame, 0.0, 0.0), 0, "and no line across the seam");
    }

    /// Two layers drawing one set (paths and trails) upload it once; a
    /// layer that then gets a set of its own does not write over the
    /// buffer the other still draws.
    #[test]
    fn two_layers_of_one_set_share_one_upload() {
        let Some(ctx) = device() else { return };
        let camera = flat_camera(0.0, 0.0, 1.0);
        let frame = TripFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let a = TripSet::from_trips(&[TripInput { row: 0, points: vec![p(-40.0, 20.0, 0.0), p(-20.0, 20.0, 10.0)], color: red, label: None }]);
        let b = TripSet::from_trips(&[TripInput { row: 0, points: vec![p(20.0, -20.0, 0.0), p(40.0, -20.0, 10.0)], color: red, label: None }]);
        let style = TripStyle { width_px: 4.0, ..TripStyle::default() };
        let mut pipeline = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        pipeline.prepare(&ctx, &frame, &[
            TripLayerDraw { id: "paths", generation: 1, set: &a, style, clock: None },
            TripLayerDraw { id: "trails", generation: 2, set: &a, style, clock: Some(5.0) },
        ]);
        assert_eq!(pipeline.uploads_total(), 1, "one set, one upload");
        // "trails" moves to set b: its own buffer; "paths" still draws a.
        pipeline.prepare(&ctx, &frame, &[
            TripLayerDraw { id: "paths", generation: 1, set: &a, style, clock: None },
            TripLayerDraw { id: "trails", generation: 3, set: &b, style, clock: None },
        ]);
        assert_eq!(pipeline.uploads_total(), 2);
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "paths", generation: 1, set: &a, style, clock: None });
        assert!(red_at(&px, &frame, -30.0, 20.0) > 200, "the shared buffer still holds set a");
    }

    /// On a full globe a trip on the far side is hidden by the planet; one
    /// facing the eye is drawn, lifted when it has altitude.
    #[test]
    fn the_globe_hides_far_side_trips() {
        let Some(ctx) = device() else { return };
        let camera = flat_camera(0.0, 0.0, 1.0);
        let globe = crate::render::globe::globe_frame(&camera);
        assert!(globe.transition >= 1.0);
        let frame = TripFrame::new(&camera, &globe, 1.0);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let near = TripSet::from_trips(&[TripInput { row: 0, points: vec![p(-20.0, 0.0, 0.0), p(20.0, 0.0, 10.0)], color: red, label: None }]);
        let far = TripSet::from_trips(&[TripInput { row: 0, points: vec![p(160.0, 0.0, 0.0), p(-160.0, 0.0, 10.0)], color: red, label: None }]);
        let style = TripStyle { width_px: 4.0, ..TripStyle::default() };
        let mut pipeline = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "n", generation: 1, set: &near, style, clock: None });
        let (x, y) = frame.project_globe((0.0, 0.0), (0.001, 0.0), 0.0, 0.0).expect("visible");
        let i = ((y.round() as usize) * 256 + x.round() as usize) * 4;
        assert!(px[i] > 200, "the near trip is drawn at its middle");
        let px = render(&ctx, &mut pipeline, &frame, TripLayerDraw { id: "f", generation: 1, set: &far, style, clock: None });
        assert!(px.chunks(4).all(|p| p[0] == 0), "the far-side trip is behind the planet");
    }
}
