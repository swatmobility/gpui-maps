//! Heatmaps: screen-space kernel density, deck.gl's HeatmapLayer
//! re-expressed in `heatmap_shader.wgsl` (MIT; see `PROVENANCE.md`).
//!
//! - **Data**: a [`HeatmapSet`] is one flat point array ([`HeatPoint`], 36
//!   B: lon, lat, weight, four filter values and the point's Mercator
//!   offset from the set's anchor), uploaded once per
//!   generation and never again while the camera, the style or a filter
//!   window moves ([`HeatmapPipeline::uploads_total`] holds still).
//! - **Three passes per layer**: the points sum into a half-resolution
//!   [`DENSITY_FORMAT`] target as Gaussians of `radius_px`; one point per
//!   second texel finds the frame's densest texel into a 1x1 target (a
//!   `Max` blend); the composite, in the symbol pass, maps density over
//!   that maximum through the colour ramp. So the hottest pixel on screen
//!   is the ramp's last colour whatever the data's size (deck's automatic
//!   colour domain), `intensity` scales that, and `threshold` fades the
//!   thin edge.
//! - **Filter**: up to four values per point and a window each, as a
//!   uniform ([`HeatmapStyle`] has none: [`HeatmapLayerDraw::filter`]); a
//!   window move is a uniform write.
//! - **Precision**: the density is f16 (`R16Float` blends everywhere); a
//!   texel past a few thousand overlapping Gaussians stops growing, which
//!   only flattens the very top of the ramp. The flat position is NOT
//!   absolute f32 Mercator (which rounds to ~2.4 m, several pixels at
//!   street zoom, and an offset that grows with the zoom: the heat slid
//!   against the basemap while zooming): each point carries its offset
//!   from the set's anchor (f64 on the CPU, small, so f32 holds it to
//!   millimetres) and the uniform carries the anchor's offset from the
//!   camera's origin, taken in f64.

use super::alloc::TrackedBuffer;
use super::arcs::ArcFrame;
use super::gpu::GpuContext;
use crate::style::Rgba;

/// The frame a heatmap draw needs: the same camera state as the arcs'.
pub type HeatmapFrame = ArcFrame;

/// The density target's format: summed, so float.
pub const DENSITY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R16Float;

/// The density target's size against the colour target's, each way.
pub const DENSITY_SCALE: f32 = 0.5;

/// Texels the max pass skips each way (`vs_max`'s stride).
const MAX_STRIDE: u32 = 2;

/// Ramp texels.
pub const RAMP_SIZE: u32 = 256;

/// The most filter values a point carries.
pub const MAX_FILTERS: usize = 4;

/// One point on the GPU: 36 B.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct HeatPoint {
    pub lon: f32,
    pub lat: f32,
    /// `0..=1` by convention; 0 (or NaN) draws nothing.
    pub weight: f32,
    /// Relative to each column's [`HeatmapSet::filter_base`]; NaN fails an
    /// enabled window.
    pub filter: [f32; MAX_FILTERS],
    /// Web Mercator (`0..1` a world) less [`HeatmapSet::anchor`], computed
    /// in f64: the flat frame's position. `lon`/`lat` serve the globe.
    pub merc: [f32; 2],
}

const _: () = assert!(std::mem::size_of::<HeatPoint>() == 36);

const POINT_ATTRS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Float32x4, 3 => Float32x2];

/// A layer's points, ready to upload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeatmapSet {
    pub points: Vec<HeatPoint>,
    /// The filter columns' names, in slot order.
    pub filter_fields: Vec<String>,
    /// What each filter column's values are relative to (its minimum).
    pub filter_base: Vec<f64>,
    /// The Web Mercator point (`0..1` a world) every [`HeatPoint::merc`]
    /// is relative to: the centre of the points' Mercator box.
    pub anchor: [f64; 2],
}

impl HeatmapSet {
    /// Builds the set: `positions` as `[lon, lat]`; `weights` one per
    /// position (`None`: every point weighs 1); `filters` up to
    /// [`MAX_FILTERS`] named columns, one value per position (NaN: null).
    /// Non-finite positions are dropped.
    #[must_use]
    pub fn new(positions: &[[f64; 2]], weights: Option<&[f64]>, filters: &[(&str, &[f64])]) -> Self {
        let filters = &filters[..filters.len().min(MAX_FILTERS)];
        let base: Vec<f64> = filters.iter().map(|(_, v)| v.iter().copied().filter(|x| x.is_finite()).fold(f64::INFINITY, f64::min)).map(|b| if b.is_finite() { b } else { 0.0 }).collect();
        let merc = |p: &[f64; 2]| {
            let (x, y) = crate::geo::lon_lat_to_tile_xy(p[0], p[1], 0.0);
            [x, y]
        };
        let finite = || positions.iter().filter(|p| p[0].is_finite() && p[1].is_finite()).map(merc);
        let (lo, hi) = finite().fold(([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]), |(lo, hi), m| ([lo[0].min(m[0]), lo[1].min(m[1])], [hi[0].max(m[0]), hi[1].max(m[1])]));
        let anchor = if lo[0].is_finite() { [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5] } else { [0.5, 0.5] };
        let mut points = Vec::with_capacity(positions.len());
        for (i, p) in positions.iter().enumerate() {
            if !(p[0].is_finite() && p[1].is_finite()) {
                continue;
            }
            let m = merc(p);
            let w = weights.map_or(1.0, |w| w.get(i).copied().unwrap_or(f64::NAN));
            let mut filter = [0.0f32; MAX_FILTERS];
            for (k, (_, col)) in filters.iter().enumerate() {
                filter[k] = col.get(i).map_or(f32::NAN, |v| if v.is_finite() { (v - base[k]) as f32 } else { f32::NAN });
            }
            let merc = [(m[0] - anchor[0]) as f32, (m[1] - anchor[1]) as f32];
            points.push(HeatPoint { lon: p[0] as f32, lat: p[1] as f32, weight: if w.is_finite() { w.max(0.0) as f32 } else { 0.0 }, filter, merc });
        }
        Self { points, filter_fields: filters.iter().map(|(n, _)| (*n).to_string()).collect(), filter_base: base, anchor }
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Instance bytes an upload costs.
    #[must_use]
    pub fn upload_bytes(&self) -> usize {
        self.points.len() * std::mem::size_of::<HeatPoint>()
    }

    /// Absolute windows (one per filter column, `None`: off) as the
    /// uniform's relative bounds and enabled mask.
    #[must_use]
    pub fn relative_windows(&self, windows: &[Option<[f64; 2]>]) -> HeatFilter {
        let mut out = HeatFilter::default();
        for (k, w) in windows.iter().enumerate().take(self.filter_base.len().min(MAX_FILTERS)) {
            if let Some([lo, hi]) = w {
                let b = self.filter_base[k];
                out.lo[k] = (lo - b) as f32;
                out.hi[k] = (hi - b) as f32;
                out.mask |= 1 << k;
            }
        }
        out
    }

    /// How many points pass `filter` and weigh something: the CPU twin of
    /// `vs_weight`'s test.
    #[must_use]
    pub fn passing(&self, filter: &HeatFilter) -> usize {
        self.points.iter().filter(|p| p.weight > 0.0 && filter.passes(&p.filter)).count()
    }
}

/// The filter windows as the shader reads them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HeatFilter {
    pub lo: [f32; MAX_FILTERS],
    pub hi: [f32; MAX_FILTERS],
    /// Bit `k`: window `k` is on.
    pub mask: u32,
}

impl HeatFilter {
    /// `passes` in the shader.
    #[must_use]
    pub fn passes(&self, values: &[f32; MAX_FILTERS]) -> bool {
        (0..MAX_FILTERS).all(|k| self.mask & (1 << k) == 0 || (values[k] >= self.lo[k] && values[k] <= self.hi[k]))
    }
}

/// A layer's look.
#[derive(Debug, Clone, PartialEq)]
pub struct HeatmapStyle {
    /// The kernel's radius, logical pixels.
    pub radius_px: f32,
    /// Multiplies the density before the ramp: above 1 more of the map
    /// reaches the hot end.
    pub intensity: f32,
    /// The share of the maximum below which a pixel fades out (deck's
    /// `threshold`).
    pub threshold: f32,
    pub opacity: f32,
    /// The ramp's colours, cold to hot; density 0 is transparent and the
    /// colours spread over `(0, 1]`.
    pub colors: Vec<Rgba>,
}

impl Default for HeatmapStyle {
    fn default() -> Self {
        // Kepler's defaults: 20 px, intensity 1, threshold 0.18, and its
        // "Global Warming" ramp.
        let colors = ["#5A1846", "#900C3F", "#C70039", "#E3611C", "#F1920E", "#FFC300"].iter().map(|h| hex(h)).collect();
        Self { radius_px: 20.0, intensity: 1.0, threshold: 0.18, opacity: 0.8, colors }
    }
}

fn hex(h: &str) -> Rgba {
    let v = u32::from_str_radix(h.trim_start_matches('#'), 16).unwrap_or(0);
    Rgba { r: ((v >> 16) & 255) as f32 / 255.0, g: ((v >> 8) & 255) as f32 / 255.0, b: (v & 255) as f32 / 255.0, a: 1.0 }
}

/// The ramp texture's texels (RGBA8, straight alpha): transparent at 0,
/// `colors[i]` at `(i + 1) / n`, linear between; alpha rises from 0 to the
/// first colour's over the first stop, as Kepler's `heatmap-color`.
#[must_use]
pub fn ramp_texels(colors: &[Rgba]) -> Vec<[u8; 4]> {
    let n = colors.len();
    let byte = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    (0..RAMP_SIZE)
        .map(|i| {
            if n == 0 {
                return [0, 0, 0, 0];
            }
            let t = i as f32 / (RAMP_SIZE - 1) as f32 * n as f32;
            // Stop k sits at t == k + 1; stop 0 (transparent) at t == 0.
            let (a, b, f) = if t <= 1.0 {
                let c = colors[0];
                (Rgba { a: 0.0, ..c }, c, t)
            } else {
                let k = (t.floor() as usize).min(n - 1);
                let lo = colors[(k - 1).min(n - 1)];
                let hi = colors[k.min(n - 1)];
                (lo, hi, t - k as f32)
            };
            let mix = |x: f32, y: f32| x + (y - x) * f.clamp(0.0, 1.0);
            [byte(mix(a.r, b.r)), byte(mix(a.g, b.g)), byte(mix(a.b, b.b)), byte(mix(a.a, b.a))]
        })
        .collect()
}

/// The uniform, 240 B.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct HeatUniform {
    pub flat_view_proj: [[f32; 4]; 4],
    pub globe_view_proj: [[f32; 4]; 4],
    pub eye: [f32; 4],
    pub origin: [f32; 4],
    pub target_size: [f32; 4],
    pub params: [f32; 4],
    pub filter_lo: [f32; 4],
    pub filter_hi: [f32; 4],
    /// `xy`: the set's anchor less the frame's origin, world units, taken
    /// in f64 and wrapped to the nearest copy of the world. `zw`: unused.
    pub anchor: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<HeatUniform>() == 240);

/// The uniform for one layer whose set is anchored at `anchor` (Web
/// Mercator, [`HeatmapSet::anchor`]) on a density target of `density`
/// texels.
#[must_use]
pub fn uniform(frame: &HeatmapFrame, style: &HeatmapStyle, filter: &HeatFilter, anchor: [f64; 2], density: (u32, u32)) -> HeatUniform {
    let world = f64::from(super::WORLD_SIZE);
    let mut dx = anchor[0] * world - frame.origin_world.x;
    dx -= (dx / world).round() * world;
    let dy = anchor[1] * world - frame.origin_world.y;
    HeatUniform {
        flat_view_proj: frame.flat_view_proj.as_mat4().to_cols_array_2d(),
        globe_view_proj: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
        eye: [frame.eye.x as f32, frame.eye.y as f32, frame.eye.z as f32, 0.0],
        origin: [(frame.origin_world.x / world) as f32, (frame.origin_world.y / world) as f32, super::WORLD_SIZE, frame.transition],
        target_size: [density.0 as f32, density.1 as f32, style.radius_px.max(0.5) * frame.px_scale * DENSITY_SCALE, 1.0],
        params: [style.intensity, style.threshold, style.opacity.clamp(0.0, 1.0), filter.mask as f32],
        filter_lo: filter.lo,
        filter_hi: filter.hi,
        anchor: [dx as f32, dy as f32, 0.0, 0.0],
    }
}

/// The density target's size for a colour target of `size`.
#[must_use]
pub fn density_size(size: (u32, u32)) -> (u32, u32) {
    let s = |x: u32| ((x as f32 * DENSITY_SCALE).ceil() as u32).max(1);
    (s(size.0), s(size.1))
}

/// One layer to draw this frame.
pub struct HeatmapLayerDraw<'a> {
    pub id: &'a str,
    /// Bumped whenever `set` changes: the upload is skipped otherwise.
    pub generation: u64,
    pub set: &'a HeatmapSet,
    pub style: &'a HeatmapStyle,
    pub filter: HeatFilter,
}

/// What the last [`HeatmapPipeline::prepare`] planned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeatmapFrameStats {
    pub layers: u64,
    pub points: u64,
    pub upload_bytes: u64,
}

struct Targets {
    size: (u32, u32),
    density: wgpu::Texture,
    density_view: wgpu::TextureView,
    _max: wgpu::Texture,
    max_view: wgpu::TextureView,
}

/// One layer on the GPU.
struct GpuLayer {
    points: Option<TrackedBuffer>,
    count: u32,
    generation: u64,
    uniform: TrackedBuffer,
    ramp: wgpu::Texture,
    ramp_view: wgpu::TextureView,
    ramp_colors: Vec<Rgba>,
    targets: Option<Targets>,
    weight_group: wgpu::BindGroup,
    max_group: Option<wgpu::BindGroup>,
    color_group: Option<wgpu::BindGroup>,
    /// Whether this frame's density holds the layer.
    ready: bool,
    order: usize,
}

pub struct HeatmapPipeline {
    weight: wgpu::RenderPipeline,
    max: wgpu::RenderPipeline,
    color: wgpu::RenderPipeline,
    weight_bgl: wgpu::BindGroupLayout,
    max_bgl: wgpu::BindGroupLayout,
    color_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    layers: std::collections::HashMap<String, GpuLayer>,
    stats: HeatmapFrameStats,
    uploads_total: u64,
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<HeatUniform>() as u64) },
        count: None,
    }
}

fn texture_entry(binding: u32, visibility: wgpu::ShaderStages, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
        count: None,
    }
}

impl HeatmapPipeline {
    #[must_use]
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps heatmap shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("heatmap_shader.wgsl").into()),
        });
        let vf = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let weight_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("heatmap-weight-bgl"), entries: &[uniform_entry(0, vf)] });
        let max_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("heatmap-max-bgl"),
            entries: &[uniform_entry(0, vf), texture_entry(1, wgpu::ShaderStages::VERTEX, true)],
        });
        let color_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("heatmap-color-bgl"),
            entries: &[
                uniform_entry(0, vf),
                texture_entry(1, wgpu::ShaderStages::FRAGMENT, true),
                texture_entry(2, wgpu::ShaderStages::FRAGMENT, false),
                texture_entry(3, wgpu::ShaderStages::FRAGMENT, true),
                wgpu::BindGroupLayoutEntry { binding: 4, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });
        let layout = |label: &str, bgl: &wgpu::BindGroupLayout| ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &[Some(bgl)], immediate_size: 0 });
        let sum = wgpu::BlendState {
            color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
            alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
        };
        let max_blend = wgpu::BlendState {
            color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max },
            alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max },
        };
        let weight_layout = layout("heatmap-weight-layout", &weight_bgl);
        let weight = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("heatmap-weight-pipeline"),
            layout: Some(&weight_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_weight"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<HeatPoint>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &POINT_ATTRS,
                })],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_weight"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState { format: DENSITY_FORMAT, blend: Some(sum), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let max_layout = layout("heatmap-max-layout", &max_bgl);
        let max = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("heatmap-max-pipeline"),
            layout: Some(&max_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_max"), compilation_options: wgpu::PipelineCompilationOptions::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::PointList, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_max"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState { format: DENSITY_FORMAT, blend: Some(max_blend), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let color_layout = layout("heatmap-color-layout", &color_bgl);
        let color = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("heatmap-color-pipeline"),
            layout: Some(&color_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_color"), compilation_options: wgpu::PipelineCompilationOptions::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_color"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState { format: color_format, blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("heatmap-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self { weight, max, color, weight_bgl, max_bgl, color_bgl, sampler, layers: std::collections::HashMap::new(), stats: HeatmapFrameStats::default(), uploads_total: 0 }
    }

    /// Uploads what changed, drops layers no longer drawn and writes each
    /// layer's uniform for a colour target of `size` (physical px). ONE
    /// call per frame, before [`Self::accumulate`].
    pub fn prepare(&mut self, ctx: &GpuContext, frame: &HeatmapFrame, draws: &[HeatmapLayerDraw<'_>], size: (u32, u32)) {
        self.layers.retain(|id, _| draws.iter().any(|d| d.id == id));
        let density = density_size(size);
        let mut stats = HeatmapFrameStats::default();
        for (order, d) in draws.iter().enumerate() {
            let layer = self.layers.entry(d.id.to_string()).or_insert_with(|| {
                let uniform = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                    label: Some("heatmap-uniform"),
                    size: std::mem::size_of::<HeatUniform>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let ramp = ctx.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("heatmap-ramp"),
                    size: wgpu::Extent3d { width: RAMP_SIZE, height: 1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let ramp_view = ramp.create_view(&Default::default());
                let weight_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("heatmap-weight-group"),
                    layout: &self.weight_bgl,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
                });
                GpuLayer {
                    points: None,
                    count: 0,
                    generation: u64::MAX,
                    uniform,
                    ramp,
                    ramp_view,
                    ramp_colors: Vec::new(),
                    targets: None,
                    weight_group,
                    max_group: None,
                    color_group: None,
                    ready: false,
                    order,
                }
            });
            layer.order = order;
            layer.ready = false;
            if layer.generation != d.generation {
                let bytes: &[u8] = bytemuck::cast_slice(&d.set.points);
                if bytes.is_empty() {
                    layer.points = None;
                } else {
                    let need = bytes.len() as u64;
                    if layer.points.as_ref().is_none_or(|b| b.tracked_size() < need) {
                        layer.points = Some(TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                            label: Some("heatmap-points"),
                            size: need,
                            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        }));
                    }
                    if let Some(b) = layer.points.as_ref() {
                        ctx.queue.write_buffer(b, 0, bytes);
                    }
                    stats.upload_bytes += need;
                    self.uploads_total += 1;
                }
                layer.count = d.set.points.len() as u32;
                layer.generation = d.generation;
            }
            if layer.ramp_colors != d.style.colors {
                let texels = ramp_texels(&d.style.colors);
                ctx.queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture: &layer.ramp, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    bytemuck::cast_slice(&texels),
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(RAMP_SIZE * 4), rows_per_image: Some(1) },
                    wgpu::Extent3d { width: RAMP_SIZE, height: 1, depth_or_array_layers: 1 },
                );
                layer.ramp_colors.clone_from(&d.style.colors);
                // The colour group names the ramp view, which is unchanged.
            }
            if layer.targets.as_ref().is_none_or(|t| t.size != density) {
                let tex = |label: &str, w: u32, h: u32| {
                    ctx.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some(label),
                        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: DENSITY_FORMAT,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    })
                };
                let density_tex = tex("heatmap-density", density.0, density.1);
                let max_tex = tex("heatmap-max", 1, 1);
                let density_view = density_tex.create_view(&Default::default());
                let max_view = max_tex.create_view(&Default::default());
                layer.max_group = Some(ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("heatmap-max-group"),
                    layout: &self.max_bgl,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: layer.uniform.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&density_view) },
                    ],
                }));
                layer.color_group = Some(ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("heatmap-color-group"),
                    layout: &self.color_bgl,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: layer.uniform.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&density_view) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&max_view) },
                        wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&layer.ramp_view) },
                        wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    ],
                }));
                layer.targets = Some(Targets { size: density, density: density_tex, density_view, _max: max_tex, max_view });
            }
            ctx.queue.write_buffer(&layer.uniform, 0, bytemuck::bytes_of(&uniform(frame, d.style, &d.filter, d.set.anchor, density)));
            if layer.count > 0 {
                stats.layers += 1;
                stats.points += u64::from(layer.count);
            }
        }
        self.stats = stats;
    }

    /// Sums each layer's points into its density target and finds its
    /// maximum, in passes of their own on `encoder`; call after
    /// [`Self::prepare`] and before the pass [`Self::draw`] draws into.
    pub fn accumulate(&mut self, encoder: &mut wgpu::CommandEncoder) {
        for layer in self.layers.values_mut() {
            layer.ready = false;
            let (Some(points), Some(targets), Some(max_group)) = (layer.points.as_ref(), layer.targets.as_ref(), layer.max_group.as_ref()) else { continue };
            if layer.count == 0 {
                continue;
            }
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("heatmap-weight-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &targets.density_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.weight);
                pass.set_bind_group(0, &layer.weight_group, &[]);
                pass.set_vertex_buffer(0, points.slice(..));
                pass.draw(0..4, 0..layer.count);
                super::alloc::note_draw_calls(1);
            }
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("heatmap-max-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &targets.max_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.max);
                pass.set_bind_group(0, max_group, &[]);
                let (w, h) = targets.size;
                pass.draw(0..w.div_ceil(MAX_STRIDE) * h.div_ceil(MAX_STRIDE), 0..1);
                super::alloc::note_draw_calls(1);
            }
            let _ = &targets.density;
            layer.ready = true;
        }
    }

    /// Composites each accumulated layer over the map, in layer order.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let mut ready: Vec<&GpuLayer> = self.layers.values().filter(|l| l.ready).collect();
        ready.sort_by_key(|l| l.order);
        for layer in ready {
            let Some(group) = layer.color_group.as_ref() else { continue };
            pass.set_pipeline(&self.color);
            pass.set_bind_group(0, group, &[]);
            pass.draw(0..3, 0..1);
            super::alloc::note_draw_calls(1);
        }
    }

    /// What the last prepare planned.
    #[must_use]
    pub fn stats(&self) -> HeatmapFrameStats {
        self.stats
    }

    /// Point uploads since this pipeline was built (a camera, style or
    /// filter move makes none).
    #[must_use]
    pub fn uploads_total(&self) -> u64 {
        self.uploads_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_keeps_finite_points_and_reads_filters_relative_to_their_minimum() {
        let pos = [[1.0, 2.0], [f64::NAN, 0.0], [3.0, 4.0]];
        let w = [0.5, 1.0, f64::NAN];
        let t = [100.0, 50.0, 200.0];
        let set = HeatmapSet::new(&pos, Some(&w), &[("t", &t)]);
        assert_eq!(set.len(), 2, "the NaN position is dropped");
        assert_eq!(set.filter_base, vec![50.0]);
        assert_eq!(set.points[0].filter[0], 50.0);
        assert_eq!(set.points[1].weight, 0.0, "a NaN weight draws nothing");
        let f = set.relative_windows(&[Some([90.0, 150.0])]);
        assert_eq!(f.mask, 1);
        assert_eq!(set.passing(&f), 1, "100 passes, 200 is outside and weightless anyway");
        assert_eq!(set.passing(&HeatFilter::default()), 1, "no window: every point that weighs");
    }

    #[test]
    fn each_point_carries_its_mercator_offset_from_the_anchor_to_millimetres() {
        let pos = [[103.8519, 1.2903], [103.9301, 1.3502], [103.6, 1.25]];
        let set = HeatmapSet::new(&pos, None, &[]);
        let world_m = 40_075_016.7;
        for (p, h) in pos.iter().zip(&set.points) {
            let (x, y) = crate::geo::lon_lat_to_tile_xy(p[0], p[1], 0.0);
            let dx = (set.anchor[0] + f64::from(h.merc[0]) - x) * world_m;
            let dy = (set.anchor[1] + f64::from(h.merc[1]) - y) * world_m;
            assert!(dx.abs() < 0.01 && dy.abs() < 0.01, "{dx} m, {dy} m off");
        }
        let lo = crate::geo::lon_lat_to_tile_xy(103.6, 1.3502, 0.0);
        let hi = crate::geo::lon_lat_to_tile_xy(103.9301, 1.25, 0.0);
        assert!((set.anchor[0] - (lo.0 + hi.0) / 2.0).abs() < 1e-12 && (set.anchor[1] - (lo.1 + hi.1) / 2.0).abs() < 1e-12, "the box's centre");
    }

    #[test]
    fn the_anchor_offset_is_taken_in_f64_and_wraps_to_the_nearest_world() {
        let world = f64::from(crate::render::WORLD_SIZE);
        let frame = HeatmapFrame {
            flat_view_proj: glam::DMat4::IDENTITY,
            globe_view_proj: glam::DMat4::IDENTITY,
            eye: glam::DVec3::ZERO,
            origin_world: glam::DVec2::new(0.99 * world, 0.5 * world),
            transition: 0.0,
            viewport: (200.0, 100.0),
            px_scale: 1.0,
            zoom: 3.0,
            pitch: 0.0,
        };
        let u = uniform(&frame, &HeatmapStyle::default(), &HeatFilter::default(), [0.01, 0.5 + 1e-9], (100, 50));
        assert!((f64::from(u.anchor[0]) - 0.02 * world).abs() < 1e-3, "across the antimeridian, not a world away: {}", u.anchor[0]);
        assert!((f64::from(u.anchor[1]) - 1e-9 * world).abs() < 1e-9, "a sub-ulp difference survives: {}", u.anchor[1]);
    }

    #[test]
    fn a_nan_filter_value_fails_an_enabled_window_only() {
        let set = HeatmapSet::new(&[[0.0, 0.0]], None, &[("x", &[f64::NAN])]);
        assert!(set.points[0].filter[0].is_nan());
        assert_eq!(set.passing(&HeatFilter::default()), 1);
        assert_eq!(set.passing(&set.relative_windows(&[Some([0.0, 1.0])])), 0);
    }

    #[test]
    fn the_ramp_starts_transparent_and_ends_on_the_last_colour() {
        let style = HeatmapStyle::default();
        let r = ramp_texels(&style.colors);
        assert_eq!(r.len(), RAMP_SIZE as usize);
        assert_eq!(r[0][3], 0, "density 0 is clear");
        assert_eq!(r[255], [0xFF, 0xC3, 0x00, 255], "the hottest texel is the last colour");
        let first = r[(RAMP_SIZE as usize - 1) / style.colors.len()];
        assert!(first[3] > 240, "the first colour is opaque at its stop: {first:?}");
    }

    #[test]
    fn the_uniform_carries_the_radius_in_density_texels() {
        let frame = HeatmapFrame {
            flat_view_proj: glam::DMat4::IDENTITY,
            globe_view_proj: glam::DMat4::IDENTITY,
            eye: glam::DVec3::ZERO,
            origin_world: glam::DVec2::ZERO,
            transition: 0.0,
            viewport: (200.0, 100.0),
            px_scale: 2.0,
            zoom: 3.0,
            pitch: 0.0,
        };
        let style = HeatmapStyle { radius_px: 10.0, ..HeatmapStyle::default() };
        let u = uniform(&frame, &style, &HeatFilter { mask: 0b101, ..HeatFilter::default() }, [0.5, 0.5], density_size((200, 100)));
        assert_eq!(u.target_size, [100.0, 50.0, 10.0, 1.0], "10 logical px at 2x on a half-size target");
        assert_eq!(u.params[3], 5.0);
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::*;
    use crate::render::camera::Camera;

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

    /// All three passes into a `w` x `h` RGBA8 target over transparent
    /// black, as `MapView::paint` runs them; the red channel back (on a
    /// grey ramp, the ramp position).
    fn render(ctx: &GpuContext, pipeline: &mut HeatmapPipeline, frame: &HeatmapFrame, draw: HeatmapLayerDraw<'_>, (w, h): (u32, u32)) -> Vec<u8> {
        pipeline.prepare(ctx, frame, &[draw], (w, h));
        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("heatmap-test"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let row = (w * 4).div_ceil(256) * 256;
        let read = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("heatmap-test-read"),
            size: u64::from(row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        pipeline.accumulate(&mut encoder);
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("heatmap-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
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
            wgpu::TexelCopyBufferInfo { buffer: &read, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
            size,
        );
        ctx.queue.submit(Some(encoder.finish()));
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let mapped = read.slice(..).get_mapped_range().expect("mapped").to_vec();
        let mut red = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                red.push(mapped[(y * row + x * 4) as usize]);
            }
        }
        red
    }

    /// The red-weighted centre of the heat over its top tenth (the peak,
    /// sub-pixel), device px.
    fn heat_centre(red: &[u8], w: u32) -> Option<(f64, f64)> {
        let top = f64::from(*red.iter().max()?);
        let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
        for (i, &a) in red.iter().enumerate() {
            let a = f64::from(a);
            if a >= top * 0.9 && a > 0.0 {
                sx += ((i as u32 % w) as f64 + 0.5) * a;
                sy += ((i as u32 / w) as f64 + 0.5) * a;
                n += a;
            }
        }
        (n > 0.0).then(|| (sx / n, sy / n))
    }

    /// The consumer's size: ~2,100 points on a symmetric 25 m grid round
    /// one centre, weighted to a hot core there (a flat field has no peak
    /// to find: the on-screen maximum would follow the screen's edge).
    fn cluster(centre: [f64; 2]) -> HeatmapSet {
        let (mut pos, mut w) = (Vec::new(), Vec::new());
        let deg = 25.0 / 111_320.0;
        for i in -26i32..=26 {
            for j in -26i32..=26 {
                let r2 = f64::from(i * i + j * j);
                if r2 > 26.0 * 26.0 {
                    continue;
                }
                pos.push([centre[0] + f64::from(i) * deg, centre[1] + f64::from(j) * deg]);
                w.push(0.02 + (-r2 / 2.0).exp());
            }
        }
        HeatmapSet::new(&pos, Some(&w), &[])
    }

    /// **The heat sits on the basemap on every frame of a camera move.**
    /// Street-zoom Singapore at 2x: after each pan, zoom step, rotation
    /// and tilt the heat's peak is within 1.5 device px of where the
    /// camera (f64, the basemap's own projection) puts the cluster's
    /// centre on the SAME frame.
    #[test]
    fn the_heat_peak_follows_the_camera_on_the_same_frame() {
        let Some(ctx) = device() else { return };
        let centre = [103.8519, 1.2903];
        let set = cluster(centre);
        let grey = (1..=10u8).map(|i| f32::from(i) / 10.0).map(|v| Rgba { r: v, g: v, b: v, a: 1.0 }).collect();
        let style = HeatmapStyle { radius_px: 20.0, opacity: 1.0, threshold: 0.05, colors: grey, ..HeatmapStyle::default() };
        let mut pipeline = HeatmapPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let mut worst: (f64, String) = (0.0, String::new());
        let mut worst_step: (f64, String) = (0.0, String::new());
        // 2x (the consumer's Retina), and an odd-sized 1x target whose
        // density target is not exactly half of it.
        for (lw, lh, dpr) in [(320.0f32, 240.0f32, 2.0f32), (333.0, 217.0, 1.0)] {
            let size = ((lw * dpr) as u32, (lh * dpr) as u32);
            let base = Camera { center_lon: centre[0], center_lat: centre[1], zoom: 15.0, pitch: 0.0, bearing: 0.0, viewport_width_px: lw, viewport_height_px: lh, fov_y: Camera::DEFAULT_FOV_Y };
            let mut cameras = Vec::new();
            for k in 0..=16 {
                // A zoom from 14 to 18 in quarter steps, panning tens of px
                // each frame, the cluster off the centre.
                let zoom = 14.0 + f64::from(k) * 0.25;
                let step = 25.0 / 2f64.powf(zoom) * 360.0 / 512.0 * f64::from(k % 5);
                cameras.push(Camera { zoom, center_lon: centre[0] + step * 0.7, center_lat: centre[1] - step * 0.4, ..base });
            }
            cameras.push(Camera { zoom: 17.3, bearing: 0.6, ..base });
            cameras.push(Camera { zoom: 17.3, pitch: 0.5, bearing: -0.4, center_lon: centre[0] + 0.0002, ..base });
            let mut last: Option<(f64, f64)> = None;
            for camera in &cameras {
                let frame = HeatmapFrame::new(camera, &crate::render::globe::globe_frame(camera).flattened(), dpr);
                let red = render(&ctx, &mut pipeline, &frame, HeatmapLayerDraw { id: "gps_heat", generation: 1, set: &set, style: &style, filter: HeatFilter::default() }, size);
                let (hx, hy) = heat_centre(&red, size.0).expect("the heat is drawn");
                let (px, py, _) = camera.project_lon_lat_alt(centre[0], centre[1], 0.0).expect("on screen");
                let (ex, ey) = (f64::from(px * dpr), f64::from(py * dpr));
                let d = (hx - ex).hypot(hy - ey);
                let at = format!("{size:?} zoom {:.2} bearing {:.1} pitch {:.1}", camera.zoom, camera.bearing, camera.pitch);
                eprintln!("{at}: heat ({hx:.2}, {hy:.2}) basemap ({ex:.2}, {ey:.2}) off {d:.2}");
                if d > worst.0 {
                    worst = (d, at.clone());
                }
                // How far the heat slides against the basemap frame to frame.
                if let Some((lx, ly)) = last {
                    let step = (hx - ex - lx).hypot(hy - ey - ly);
                    if step > worst_step.0 {
                        worst_step = (step, at.clone());
                    }
                }
                last = Some((hx - ex, hy - ey));
            }
        }
        assert!(worst_step.0 <= 1.0, "the heat slides {:.2} device px against the basemap in one camera step, at {}", worst_step.0, worst_step.1);
        assert!(worst.0 <= 1.5, "the heat sits {:.2} device px off the cluster at {}", worst.0, worst.1);
    }
}
