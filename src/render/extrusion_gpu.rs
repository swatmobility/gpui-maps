//! The `fill-extrusion` pipeline: `extrusion_shader.wgsl` over the 12-byte
//! [`crate::render::extrusion::ExtrusionVertex`], drawn into the tile pass
//! with depth write on (spec §4).
//!
//! What is here:
//!
//! - [`ExtrusionPipeline`]: the shader, its two bind group layouts (group 2
//!   the per-chunk table texture, group 3 the per-layer [`ExtrusionParams`])
//!   and its three render pipelines: opaque, and the translucent pair (a
//!   depth-only prepass, then a stencilled colour pass). Groups 0 and 1 are the tile
//!   pipeline's own camera and model layouts, so an extrusion draw binds the
//!   same model group a fill draw of the same tile does.
//! - [`ExtrusionGpu`]: one [`ExtrusionBuffers`] per mesh chunk.
//! - [`ExtrusionParamsGpu`]: one uniform per plan layer, grown on demand.
//!
//! `TilePipeline::create_extrusion_buffers` / `write_extrusion_table` and
//! the `TileDraw::Extrusion` arm of `encode_tile_draws` are in `gpu.rs`.

use crate::render::extrusion::{ExtrusionParams, GroupSpan, TABLE_WIDTH};
use crate::render::GpuContext;

/// Defined in the wgpu-free `render::extrusion` so `render::frame_plan`
/// can name it; re-exported here, where the pipelines it selects live.
pub use crate::render::extrusion::ExtrusionPass;

/// The winding of a visible (outward-facing) extrusion triangle on screen.
///
/// `render::extrusion` emits every triangle with its `(b − a) × (c − a)`
/// pointing out of the solid in (x, y-down, z-up). What that is on screen
/// depends on the camera's handedness and `shader.wgsl`'s east-west flip
/// (`clip.x = -clip.x`), so it was not derived but measured:
/// `back_face_culling_matches_no_culling_and_front_culling_does_not`
/// culls `Back` against `None` and `Front` at pitch 60°, bearing 30°.
/// It failed with `Ccw`, the first candidate (the flat roof culled away,
/// so `Back` and `None` differed), and passes with `Cw`: the Mercator
/// east-west flip mirrors every triangle.
pub const EXTRUSION_FRONT_FACE: wgpu::FrontFace = wgpu::FrontFace::Cw;

const EXTRUSION_ATTRS: [wgpu::VertexAttribute; 3] =
    wgpu::vertex_attr_array![0 => Sint16x2, 1 => Snorm8x4, 2 => Uint16x2];

/// `ExtrusionVertex`: `pos` `i16x2`, `normal_top` `Snorm8x4`, `[row, flags]`
/// `u16x2`. All three formats are core in GLES 3.0 / WebGL2.
pub const EXTRUSION_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: 12,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &EXTRUSION_ATTRS,
};

const PARAMS_SIZE: u64 = std::mem::size_of::<ExtrusionParams>() as u64;

/// The extrusion shader, its layouts and its pipelines.
pub struct ExtrusionPipeline {
    /// Colour and depth written, no stencil: `fill-extrusion-opacity` 1.
    opaque: wgpu::RenderPipeline,
    /// The translucent layer's first pass: depth only (colour write mask
    /// empty), `LessEqual` with depth write, no stencil -- every fragment
    /// of the layer lays down its depth, so afterwards the depth buffer
    /// holds the layer's NEAREST surface at each pixel.
    prepass: wgpu::RenderPipeline,
    /// The translucent layer's second pass: colour blended, `LessEqual`
    /// with NO depth write (so only fragments at the prepass's nearest
    /// depth pass), and the stencil `NotEqual ref` / `Replace`: the first
    /// fragment to colour a pixel stamps the layer's reference there, so
    /// a coincident surface at the same depth (two OSM outlines, a
    /// duplicate across a tile seam) cannot blend over it a second time.
    /// MapLibre `draw_fill_extrusion.ts:27-47`, `painter.ts:395-405`
    /// (`stencilModeFor3D`).
    translucent: wgpu::RenderPipeline,
    table_bgl: wgpu::BindGroupLayout,
    params_bgl: wgpu::BindGroupLayout,
}

/// The translucent colour pass's stencil: pass where the stencil is NOT
/// the layer's reference yet, and write the reference where it passes
/// (both depth and stencil). Both faces the same (only front faces reach
/// it with back-face culling on; `back` is set anyway so a cull-free test
/// pipeline behaves the same).
const TRANSLUCENT_STENCIL_FACE: wgpu::StencilFaceState = wgpu::StencilFaceState {
    compare: wgpu::CompareFunction::NotEqual,
    fail_op: wgpu::StencilOperation::Keep,
    depth_fail_op: wgpu::StencilOperation::Keep,
    pass_op: wgpu::StencilOperation::Replace,
};

impl ExtrusionPipeline {
    /// Builds the pipeline over the tile pipeline's camera and model layouts
    /// and into its depth format (`TilePipeline::new` calls this).
    #[must_use]
    pub fn new(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        camera_bgl: &wgpu::BindGroupLayout,
        model_bgl: &wgpu::BindGroupLayout,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        Self::build(ctx, color_format, camera_bgl, model_bgl, depth_format, Some(wgpu::Face::Back))
    }

    /// [`Self::new`] with another cull mode: the front-face test's control.
    #[cfg(test)]
    pub(crate) fn new_with_cull_for_test(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        camera_bgl: &wgpu::BindGroupLayout,
        model_bgl: &wgpu::BindGroupLayout,
        depth_format: wgpu::TextureFormat,
        cull: Option<wgpu::Face>,
    ) -> Self {
        Self::build(ctx, color_format, camera_bgl, model_bgl, depth_format, cull)
    }

    fn build(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        camera_bgl: &wgpu::BindGroupLayout,
        model_bgl: &wgpu::BindGroupLayout,
        depth_format: wgpu::TextureFormat,
        cull: Option<wgpu::Face>,
    ) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps extrusion shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("extrusion_shader.wgsl").into()),
        });
        let table_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("extrusion-table-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let params_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("extrusion-params-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                // The fragment stage reads the look (occlusion, edge).
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(PARAMS_SIZE),
                },
                count: None,
            }],
        });
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("extrusion-pipeline-layout"),
            bind_group_layouts: &[Some(camera_bgl), Some(model_bgl), Some(&table_bgl), Some(&params_bgl)],
            immediate_size: 0,
        });
        let make = |label: &str, depth_write: bool, write_mask: wgpu::ColorWrites, stencil: wgpu::StencilState| {
            ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_extrusion"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(EXTRUSION_VERTEX_LAYOUT)],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face: EXTRUSION_FRONT_FACE,
                    cull_mode: cull,
                    ..wgpu::PrimitiveState::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: depth_format,
                    depth_write_enabled: Some(depth_write),
                    // `LessEqual`, as every tile pipeline: a roof and the fill
                    // under it at z = 0 fall back to draw order, and the
                    // translucent colour pass passes exactly at the depth
                    // its prepass wrote.
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil,
                    bias: wgpu::DepthBiasState::default(),
                }),
                // R8: no anti-aliasing in v1.
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_extrusion"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: color_format,
                        // Straight alpha (the shader's alpha is opacity alone)
                        // over `ALPHA_BLENDING`, as every tile fill draws: the
                        // colour lands as `c·a + dst·(1 − a)`, which is what
                        // MapLibre's premultiplied `ONE, ONE_MINUS_SRC_ALPHA`
                        // over a premultiplied `c·a` gives.
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let opaque = make("extrusion-opaque", true, wgpu::ColorWrites::ALL, wgpu::StencilState::default());
        let prepass = make("extrusion-prepass", true, wgpu::ColorWrites::empty(), wgpu::StencilState::default());
        let translucent = make(
            "extrusion-translucent",
            false,
            wgpu::ColorWrites::ALL,
            wgpu::StencilState {
                front: TRANSLUCENT_STENCIL_FACE,
                back: TRANSLUCENT_STENCIL_FACE,
                read_mask: 0xFF,
                write_mask: 0xFF,
            },
        );
        ExtrusionPipeline { opaque, prepass, translucent, table_bgl, params_bgl }
    }

    /// The group-3 layout an [`ExtrusionParamsGpu`] builds its bind groups on.
    #[must_use]
    pub fn params_layout(&self) -> &wgpu::BindGroupLayout {
        &self.params_bgl
    }

    /// The group-2 layout: one chunk's table texture.
    #[must_use]
    pub(crate) fn table_layout(&self) -> &wgpu::BindGroupLayout {
        &self.table_bgl
    }

    /// The render pipeline for `pass`.
    #[must_use]
    pub(crate) fn pipeline(&self, pass: ExtrusionPass) -> &wgpu::RenderPipeline {
        match pass {
            ExtrusionPass::Opaque => &self.opaque,
            ExtrusionPass::Prepass => &self.prepass,
            ExtrusionPass::Color { .. } => &self.translucent,
        }
    }
}

/// One [`crate::render::extrusion::ExtrusionChunk`] on the GPU.
pub struct ExtrusionBuffers {
    pub(crate) vertex: crate::render::alloc::TrackedBuffer,
    pub(crate) index: crate::render::alloc::TrackedBuffer,
    pub(crate) index_format: wgpu::IndexFormat,
    /// `Rgba32Float`, [`TABLE_WIDTH`] wide.
    pub(crate) table: wgpu::Texture,
    pub(crate) table_bind_group: wgpu::BindGroup,
    /// The chunk's draws, one per group.
    pub spans: Vec<GroupSpan>,
}

impl ExtrusionBuffers {
    /// Vertex, index and table bytes.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        let size = self.table.size();
        self.vertex.tracked_size() + self.index.tracked_size() + u64::from(size.width) * u64::from(size.height) * 16
    }
}

/// A tile's extrusion mesh on the GPU: one entry per mesh chunk, in order.
pub struct ExtrusionGpu {
    pub chunks: Vec<ExtrusionBuffers>,
}

impl ExtrusionGpu {
    /// Bytes held by every chunk.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        self.chunks.iter().map(ExtrusionBuffers::device_bytes).sum()
    }
}

/// The texel rows a table of `texels` texels needs (never 0).
pub(crate) fn table_rows(texels: usize) -> u32 {
    texels.div_ceil(TABLE_WIDTH).max(1) as u32
}

/// `table` padded with zero texels to `rows` whole rows.
pub(crate) fn padded_table(table: &[[f32; 4]], rows: u32) -> Vec<[f32; 4]> {
    let mut texels = table.to_vec();
    texels.resize(TABLE_WIDTH * rows as usize, [0.0; 4]);
    texels
}

/// One 48-byte [`ExtrusionParams`] uniform and its group-3 bind group per
/// plan layer. Grown on demand, never shrunk; rewritten in place.
#[derive(Default)]
pub struct ExtrusionParamsGpu {
    slots: Vec<(crate::render::alloc::TrackedBuffer, wgpu::BindGroup)>,
}

impl ExtrusionParamsGpu {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes `params[i]` into slot `i`, creating the slots it lacks.
    pub fn write(&mut self, ctx: &GpuContext, pipeline: &ExtrusionPipeline, params: &[ExtrusionParams]) {
        while self.slots.len() < params.len() {
            let buffer = crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                label: Some("extrusion-params"),
                size: PARAMS_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("extrusion-params-bg"),
                layout: pipeline.params_layout(),
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
            });
            self.slots.push((buffer, bind_group));
        }
        for (slot, p) in self.slots.iter().zip(params) {
            ctx.queue.write_buffer(&slot.0, 0, bytemuck::bytes_of(p));
        }
    }

    /// Slot `layer_index`'s bind group. Panics past the last written slot.
    #[must_use]
    pub fn bind_group(&self, layer_index: usize) -> &wgpu::BindGroup {
        &self.slots[layer_index].1
    }
}

/// Appends the frame's extrusion draws to `out`, **layer-major** (R4): for
/// each plan step, every tile's spans of that step's group, in `tiles`
/// order, before the next step's. One `TileDraw::Extrusion` per span, so a
/// group interleaved with another in a chunk (its spans repeat) draws each
/// of its spans.
///
/// `tiles` is `(the tile's extrusion buffers, its model group)`: exactly
/// the basemap tiles the frame draws, stand-ins with their clipped model
/// group included (R21). `params` must have been written from
/// `plan.layers` this frame (slot `i` = layer `i`).
///
/// Each step's pass, `Color { stencil_ref }` included, is used exactly as
/// the plan wrote it: the plan is the one source of truth for the stencil
/// reference ([`crate::render::frame_plan::extrusion_plan`] derives it with
/// [`crate::render::extrusion::stencil_ref`]; Task 8 review, finding 2).
/// The stencil is cleared to 0 once per frame, when the tile pass opens
/// (the Ruling, Task 7 review F5), and 0 is never a reference.
pub fn push_extrusion_draws<'a>(
    plan: &crate::render::frame_plan::ExtrusionPlan,
    tiles: &[(&'a ExtrusionGpu, &'a wgpu::BindGroup)],
    params: &'a ExtrusionParamsGpu,
    out: &mut Vec<crate::render::TileDraw<'a>>,
) {
    for step in &plan.steps {
        let Some(layer) = plan.layers.get(step.layer_index) else { continue };
        let pass = step.pass;
        let bind_group = params.bind_group(step.layer_index);
        for &(gpu, model) in tiles {
            for chunk in &gpu.chunks {
                for span in chunk.spans.iter().filter(|s| s.group == layer.group && s.index_count > 0) {
                    out.push(crate::render::TileDraw::Extrusion {
                        chunk,
                        model,
                        params: bind_group,
                        first_index: span.first_index,
                        index_count: span.index_count,
                        pass,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::extrusion::{tessellate_extrusions, ExtrusionParams};
    use crate::render::{GpuContext, TileDraw, TilePipeline};
    use crate::style::{ExtrusionRow, PaintOp, PaintPlan, Rgba};

    const SIZE: u32 = 256;
    /// Tile 16/32768/32768 has its north-west corner at (0°, 0°).
    const TILE: crate::geo::TileId = crate::geo::TileId { z: 16, x: 32768, y: 32768 };

    /// Centred on the tile's middle at zoom 16.3, SIZE x SIZE: the same
    /// literal the split-tile pixel test builds (gpu.rs:~5698).
    fn camera(pitch_deg: f64, bearing_deg: f64) -> crate::render::Camera {
        let (lon, lat) = crate::geo::tile_xy_to_lon_lat(f64::from(TILE.x) + 0.5, f64::from(TILE.y) + 0.5, f64::from(TILE.z));
        crate::render::Camera {
            center_lon: lon,
            center_lat: lat,
            zoom: f64::from(TILE.z) + 0.3,
            pitch: pitch_deg.to_radians() as f32,
            bearing: bearing_deg.to_radians() as f32,
            viewport_width_px: SIZE as f32,
            viewport_height_px: SIZE as f32,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    fn box_plan(squares: &[((i32, i32, i32, i32), f32)], color: Rgba) -> PaintPlan {
        PaintPlan {
            ops: squares
                .iter()
                .map(|&((x0, y0, x1, y1), h)| PaintOp::Extrusion {
                    group: 0,
                    row: ExtrusionRow { color_from: color, color_to: color, base_from: 0.0, height_from: h, base_to: 0.0, height_to: h },
                    geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(geo_types::Polygon::new(
                        geo_types::LineString::from(vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]),
                        vec![],
                    ))),
                })
                .collect(),
        }
    }

    fn params(meters_to_world: f32) -> ExtrusionParams {
        ExtrusionParams {
            light_dir: crate::render::extrusion::light_direction(&crate::style::ExtrusionLight::default(), 0.0),
            light_intensity: 0.5,
            light_color: [1.0; 3],
            vertical_gradient: 1.0,
            translate_px: [0.0; 2],
            opacity: 1.0,
            meters_to_world,
            ..ExtrusionParams::ZERO
        }
    }

    /// meters_to_world at lat 0: WORLD_SIZE / 40,075,016.686 m.
    fn m2w() -> f32 {
        crate::render::camera::WORLD_SIZE / 40_075_016.686
    }

    /// Runs `f` inside a validation error scope and fails on any error: the
    /// device's uncaptured-error handler only logs.
    async fn scoped<T>(ctx: &GpuContext, f: impl std::future::Future<Output = T>) -> T {
        let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let out = f.await;
        if let Some(e) = scope.pop().await {
            panic!("wgpu validation error: {e}");
        }
        out
    }

    /// `TilePipeline::new` (and so `ExtrusionPipeline::new`) under a validation scope.
    async fn tile_pipeline(ctx: &GpuContext) -> TilePipeline {
        scoped(ctx, async { TilePipeline::new(ctx, wgpu::TextureFormat::Rgba8Unorm) }).await
    }

    async fn draw(ctx: &GpuContext, pipeline: &TilePipeline, cam: &crate::render::Camera, plan: &PaintPlan, p: ExtrusionParams) -> Vec<u8> {
        draw_passes(ctx, pipeline, cam, plan, p, &[ExtrusionPass::Opaque]).await
    }

    /// [`draw`] once per pass in `passes`, pass-major (every span of the
    /// first pass, then every span of the next), all in one render pass:
    /// the layer-major order `push_extrusion_draws` emits.
    async fn draw_passes(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        cam: &crate::render::Camera,
        plan: &PaintPlan,
        p: ExtrusionParams,
        passes: &[ExtrusionPass],
    ) -> Vec<u8> {
        scoped(ctx, async {
            let mesh = tessellate_extrusions(plan);
            let gpu = pipeline.create_extrusion_buffers(ctx, &mesh).expect("non-empty");
            let matrix = crate::render::combined_tile_matrix(cam, TILE, 4096.0);
            let model = pipeline.test_model_bind_group(ctx, matrix.to_cols_array_2d(), 0.0, 0.0);
            let mut params_gpu = ExtrusionParamsGpu::new();
            params_gpu.write(ctx, pipeline.extrusion(), &[p]);
            let draws: Vec<TileDraw<'_>> = passes
                .iter()
                .flat_map(|&pass| {
                    gpu.chunks.iter().flat_map(move |c| c.spans.iter().map(move |s| (pass, c, s)))
                })
                .map(|(pass, c, s)| TileDraw::Extrusion {
                    chunk: c,
                    model: &model,
                    params: params_gpu.bind_group(0),
                    first_index: s.first_index,
                    index_count: s.index_count,
                    pass,
                })
                .collect();
            crate::render::gpu::tests::render_tile_draws(ctx, pipeline, cam, SIZE, wgpu::Color::BLACK, &draws).await
        })
        .await
    }

    /// [`draw`] through a tile pipeline whose extrusion pipeline culls `cull`.
    async fn draw_with_cull(
        ctx: &GpuContext,
        cam: &crate::render::Camera,
        plan: &PaintPlan,
        cull: Option<wgpu::Face>,
    ) -> Vec<u8> {
        let mut pipeline = tile_pipeline(ctx).await;
        let (camera_bgl, model_bgl) = pipeline.shared_layouts();
        let extrusion = scoped(ctx, async {
            ExtrusionPipeline::new_with_cull_for_test(
                ctx,
                wgpu::TextureFormat::Rgba8Unorm,
                camera_bgl,
                model_bgl,
                pipeline.depth_format(),
                cull,
            )
        })
        .await;
        pipeline.replace_extrusion_for_test(extrusion);
        draw(ctx, &pipeline, cam, plan, params(m2w())).await
    }

    /// The pixel a tile point `tile_xy` (extent 4096) at `z_m` metres lands
    /// on: the shader's own transform (`model.matrix` on lyon units, the
    /// elevation in world units, then the east-west `clip.x` flip), NDC to
    /// pixels with y down.
    fn project(cam: &crate::render::Camera, tile_xy: [f32; 2], z_m: f32) -> (u32, u32) {
        let m = crate::render::combined_tile_matrix(cam, TILE, 4096.0);
        let c = m * glam::Vec4::new(tile_xy[0] * 1000.0, tile_xy[1] * 1000.0, z_m * m2w(), 1.0);
        let (nx, ny) = (-c.x / c.w, c.y / c.w);
        let px = |ndc: f32| ((ndc + 1.0) * 0.5 * SIZE as f32).clamp(0.0, SIZE as f32 - 1.0) as u32;
        (px(nx), px(-ny))
    }

    /// The wall-centre pixel of the test box's wall that faces the camera:
    /// south (+y) at bearing 0, north at 180, west at 90 (the camera looks
    /// east). The light is the default one with `anchor`, turned by
    /// `light_bearing_deg` -- the camera's bearing, unless a test turns it
    /// the wrong way on purpose.
    async fn facing_wall_luminance(
        ctx: &GpuContext,
        pipeline: &TilePipeline,
        bearing_deg: f64,
        anchor: crate::style::LightAnchor,
        light_bearing_deg: f64,
    ) -> u32 {
        let cam = camera(60.0, bearing_deg);
        let light = crate::style::ExtrusionLight { anchor, ..Default::default() };
        let mut p = params(m2w());
        p.light_dir = crate::render::extrusion::light_direction(&light, light_bearing_deg.to_radians());
        p.vertical_gradient = 0.0;
        let px = draw(ctx, pipeline, &cam, &box_plan(&[((1800, 1800, 2300, 2300), 100.0)], Rgba { r: 0.6, g: 0.6, b: 0.6, a: 1.0 }), p).await;
        let face = match bearing_deg as i32 {
            0 => [2050.0, 2300.0],
            180 => [2050.0, 1800.0],
            90 => [1800.0, 2050.0],
            other => panic!("no facing wall sampled for bearing {other}"),
        };
        let (x, y) = project(&cam, face, 50.0);
        let i = ((y * SIZE + x) * 4) as usize;
        px[i] as u32 + px[i + 1] as u32 + px[i + 2] as u32
    }

    #[test]
    fn the_wall_facing_the_light_is_brighter() {
        pollster::block_on(async {
            use crate::style::LightAnchor::{Map, Viewport};
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            // Map-anchored default light is up and to the north: the north wall is lit, the south wall is not.
            let south = facing_wall_luminance(&ctx, &pipeline, 0.0, Map, 0.0).await;
            let north = facing_wall_luminance(&ctx, &pipeline, 180.0, Map, 180.0).await;
            assert!(north > south + 30, "north {north} vs south {south}");
            // Viewport-anchored: the light turns with the camera, so the wall
            // facing the camera is lit the same at every bearing.
            let a = facing_wall_luminance(&ctx, &pipeline, 0.0, Viewport, 0.0).await;
            let b = facing_wall_luminance(&ctx, &pipeline, 180.0, Viewport, 180.0).await;
            // 0 vs 180 cannot see the rotation's SIGN (turning by +pi and
            // -pi is the same), so bearing 90 does: turned by +90 (MapLibre
            // `fromRotation`), the light stays behind the box and the west
            // wall facing the camera is as dark as at bearing 0; turned the
            // wrong way, by -90, it falls on that very wall.
            let right = facing_wall_luminance(&ctx, &pipeline, 90.0, Viewport, 90.0).await;
            let wrong = facing_wall_luminance(&ctx, &pipeline, 90.0, Viewport, -90.0).await;
            eprintln!("wall luminance: map south {south} north {north}; viewport b0 {a} b180 {b} b90 {right} (wrong sign {wrong})");
            assert!(a.abs_diff(b) <= 3, "viewport light: {a} vs {b}");
            assert!(a.abs_diff(right) <= 3, "viewport light at bearing 90: {right} vs {a} at bearing 0");
            assert!(wrong > right + 30, "a -bearing rotation would light the facing wall: {wrong} vs {right}");
        });
    }

    /// MapLibre's gradient is `clamp((top + base) * sqrt(height / 150),
    /// mix(0.7, 0.98, 1 - intensity), 1)` per VERTEX: at the default
    /// intensity 0.5 the floor is 0.84, so a wall under 150 * 0.84^2 = 106 m
    /// clamps to 0.84 at both its foot and its head -- darker overall, but
    /// with no ramp. That holds for a BASE-0 feature only: `(top + base)`
    /// adds the base in metres, so a raised part gets no gradient at all
    /// (`extrusion::tests::a_raised_part_gets_no_vertical_gradient`). The brief's 100 m box sits in that band (measured: foot
    /// = head = 201, off = 240), so the ramp is proved on a 200 m box (head
    /// vertex 1.0) and the 100 m box pins the flat darkening.
    #[test]
    fn the_vertical_gradient_darkens_the_foot_of_a_wall_and_can_be_turned_off() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(60.0, 0.0);
            let grey = Rgba { r: 0.6, g: 0.6, b: 0.6, a: 1.0 };
            let lum = |px: &[u8], (x, y): (u32, u32)| { let i = ((y * SIZE + x) * 4) as usize; px[i] as u32 + px[i + 1] as u32 + px[i + 2] as u32 };
            let foot = project(&cam, [2050.0, 2300.0], 5.0);
            let head = project(&cam, [2050.0, 2300.0], 95.0);
            let mut off_p = params(m2w());
            off_p.vertical_gradient = 0.0;

            let tall = box_plan(&[((1800, 1800, 2300, 2300), 200.0)], grey);
            let on = draw(&ctx, &pipeline, &cam, &tall, params(m2w())).await;
            let off = draw(&ctx, &pipeline, &cam, &tall, off_p).await;
            eprintln!("gradient 200 m: on foot {} head {}, off foot {} head {}", lum(&on, foot), lum(&on, head), lum(&off, foot), lum(&off, head));
            assert!(lum(&on, foot) + 8 < lum(&on, head), "gradient on: the foot is darker");
            assert!(lum(&off, foot).abs_diff(lum(&off, head)) <= 2, "gradient off: a flat face is uniform");
            assert!(lum(&on, head) <= lum(&off, head), "the gradient only ever darkens");

            let short = box_plan(&[((1800, 1800, 2300, 2300), 100.0)], grey);
            let on = draw(&ctx, &pipeline, &cam, &short, params(m2w())).await;
            let off = draw(&ctx, &pipeline, &cam, &short, off_p).await;
            eprintln!("gradient 100 m: on foot {} head {}, off foot {} head {}", lum(&on, foot), lum(&on, head), lum(&off, foot), lum(&off, head));
            assert!(lum(&on, foot).abs_diff(lum(&on, head)) <= 2, "under 106 m both ends clamp to the 0.84 floor");
            assert!(lum(&on, head) + 20 < lum(&off, head), "but the wall is darker than with the gradient off");
        });
    }

    /// The fragment half of `ExtrusionLook`: the wall-foot occlusion
    /// darkens a wall's lowest metres and leaves its head alone, and the
    /// building cut (`bands.zw`) removes a building whose foot is past it.
    #[test]
    fn occlusion_darkens_a_walls_foot_and_the_building_cut_removes_it() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(60.0, 0.0);
            let grey = Rgba { r: 0.6, g: 0.6, b: 0.6, a: 1.0 };
            let lum = |px: &[u8], (x, y): (u32, u32)| { let i = ((y * SIZE + x) * 4) as usize; px[i] as u32 + px[i + 1] as u32 + px[i + 2] as u32 };
            let foot = project(&cam, [2050.0, 2300.0], 2.0);
            let head = project(&cam, [2050.0, 2300.0], 60.0);
            let tall = box_plan(&[((1800, 1800, 2300, 2300), 100.0)], grey);
            let mut plain = params(m2w());
            plain.vertical_gradient = 0.0;
            let mut occluded = plain;
            crate::render::extrusion::ExtrusionLook { ambient_occlusion: 0.4, ..crate::render::extrusion::ExtrusionLook::MAPLIBRE }
                .apply(&mut occluded);
            let off = draw(&ctx, &pipeline, &cam, &tall, plain).await;
            let on = draw(&ctx, &pipeline, &cam, &tall, occluded).await;
            eprintln!("occlusion: off foot {} head {}, on foot {} head {}", lum(&off, foot), lum(&off, head), lum(&on, foot), lum(&on, head));
            assert!(lum(&on, foot) + 40 < lum(&off, foot), "the foot darkens");
            assert!(lum(&on, head).abs_diff(lum(&off, head)) <= 2, "60 m up is past the 20 m occlusion height");

            // Every foot nearer than the band's start draws; with the band
            // ending before the box, nothing of it does.
            let mut cut = plain;
            cut.bands = [0.0, 0.0, 1e-9, 2e-9];
            let gone = draw(&ctx, &pipeline, &cam, &tall, cut).await;
            assert_eq!(painted(&gone).count(), 0, "past the cut, nothing");
            cut.bands = [0.0, 0.0, 1e3, 2e3];
            assert_eq!(draw(&ctx, &pipeline, &cam, &tall, cut).await, off, "a cut far past the box changes nothing");
        });
    }

    /// Extrusion GPU time, MapLibre against the enhanced look, on the
    /// Shinjuku z14 fixture at pitch 60, 1024 px square, the draw list
    /// repeated 8 times in one pass so the shading dominates the readback.
    /// Wall clock per frame, median of 15 interleaved runs.
    #[test]
    #[ignore = "timing: cargo test --release --lib extrusion_gpu_time -- --ignored --nocapture"]
    fn extrusion_gpu_time_maplibre_vs_enhanced() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let tile_id = crate::geo::TileId { z: 14, x: 14549, y: 6450 };
            let data = std::fs::read("benches/fixtures/shinjuku_z14.pbf").expect("fixture");
            let tile = crate::mvt::decode(data).expect("decode");
            let style = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
            let plan = crate::style::match_tile(&tile, &style, 16);
            let (lon, lat) = crate::geo::tile_xy_to_lon_lat(f64::from(tile_id.x) + 0.5, f64::from(tile_id.y) + 0.5, 14.0);
            let cam = crate::render::Camera {
                center_lon: lon,
                center_lat: lat,
                zoom: 15.3,
                pitch: 60f32.to_radians(),
                bearing: 30f32.to_radians(),
                viewport_width_px: 1024.0,
                viewport_height_px: 1024.0,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let mesh = tessellate_extrusions(&plan);
            let gpu = pipeline.create_extrusion_buffers(&ctx, &mesh).expect("non-empty");
            let matrix = crate::render::combined_tile_matrix(&cam, tile_id, 4096.0);
            let model = pipeline.test_model_bind_group(&ctx, matrix.to_cols_array_2d(), 0.0, 0.0);
            let m2w = (f64::from(crate::render::WORLD_SIZE) / (40_075_016.686 * lat.to_radians().cos())) as f32;
            let base = params(m2w);
            let mut enhanced = base;
            crate::render::extrusion::ExtrusionLook::ENHANCED.apply(&mut enhanced);
            let d = cam.distance();
            enhanced.bands = [d as f32, (4.0 * d) as f32, (3.0 * d) as f32, (5.0 * d) as f32];
            enhanced.haze_color = [0.97, 0.96, 0.94, 1.0];
            let mut params_gpu = ExtrusionParamsGpu::new();
            params_gpu.write(&ctx, pipeline.extrusion(), &[base, enhanced]);
            let frame = |slot: usize| {
                let draws: Vec<TileDraw<'_>> = (0..8)
                    .flat_map(|_| gpu.chunks.iter().flat_map(|c| c.spans.iter().map(move |s| (c, s))))
                    .map(|(c, s)| TileDraw::Extrusion {
                        chunk: c,
                        model: &model,
                        params: params_gpu.bind_group(slot),
                        first_index: s.first_index,
                        index_count: s.index_count,
                        pass: ExtrusionPass::Opaque,
                    })
                    .collect::<Vec<_>>();
                draws
            };
            let px = crate::render::gpu::tests::render_tile_draws(&ctx, &pipeline, &cam, 1024, wgpu::Color::BLACK, &frame(1)).await;
            let covered = px.chunks(4).filter(|p| p[0] | p[1] | p[2] != 0).count();
            println!("extrusion_gpu_time: {covered} of 1048576 px painted");
            let mut ms = [Vec::new(), Vec::new()];
            for run in 0..17 {
                for slot in [0, 1] {
                    let draws = frame(slot);
                    let t = crate::timing::Instant::now();
                    let _ = crate::render::gpu::tests::render_tile_draws(&ctx, &pipeline, &cam, 1024, wgpu::Color::BLACK, &draws).await;
                    if run >= 2 {
                        ms[slot].push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                }
            }
            for m in &mut ms {
                m.sort_by(f64::total_cmp);
            }
            let (a, b) = (ms[0][ms[0].len() / 2], ms[1][ms[1].len() / 2]);
            println!("extrusion_gpu_time: triangles {} x8, maplibre {a:.2} ms, enhanced {b:.2} ms, change {:+.1}%", mesh.index_count() / 3, (b / a - 1.0) * 100.0);
        });
    }

    fn painted(px: &[u8]) -> impl Iterator<Item = (u32, u32)> + '_ {
        px.chunks(4).enumerate().filter(|(_, p)| p[0] | p[1] | p[2] != 0).map(|(i, _)| (i as u32 % SIZE, i as u32 / SIZE))
    }

    #[test]
    fn a_pitched_box_rises() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(60.0, 0.0);
            let plan = box_plan(&[((1800, 1800, 2300, 2300), 100.0)], Rgba { r: 0.6, g: 0.6, b: 0.6, a: 1.0 });
            let flat = draw(&ctx, &pipeline, &cam, &plan, params(0.0)).await;
            let tall = draw(&ctx, &pipeline, &cam, &plan, params(m2w())).await;
            let footprint_top = painted(&flat).map(|(_, y)| y).min().expect("the flat roof paints");
            let above = painted(&tall).filter(|&(_, y)| y < footprint_top).count();
            assert!(above > 50, "a 100 m box at pitch 60 must paint above its footprint: {above} px");
            assert_eq!(painted(&flat).filter(|&(_, y)| y < footprint_top).count(), 0);
        });
    }

    /// Straight down on a box under the camera, only its roof shows: one
    /// flat colour, covering the footprint, and grown by exactly the
    /// perspective a roof `h` metres nearer the eye gets, `d / (d - h)`.
    ///
    /// Not the plan's "at most 10% outside the footprint": this camera is
    /// `Camera::distance()` = 128 / tan 30° logical px above the ground,
    /// about 215 m at zoom 16.3 on the equator, so a 100 m roof is 46% of
    /// the way to the eye and grows 1.87x linearly. Measured: 14,520 px
    /// outside a 5,929 px footprint, which is that roof and nothing else.
    #[test]
    fn a_straight_down_box_paints_only_its_roof() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(0.0, 0.0);
            let height = 100.0;
            let plan = box_plan(&[((1800, 1800, 2300, 2300), height)], Rgba { r: 0.6, g: 0.6, b: 0.6, a: 1.0 });
            let flat = draw(&ctx, &pipeline, &cam, &plan, params(0.0)).await;
            let tall = draw(&ctx, &pipeline, &cam, &plan, params(m2w())).await;
            let flat_set: std::collections::HashSet<_> = painted(&flat).collect();
            let tall_set: std::collections::HashSet<_> = painted(&tall).collect();
            assert!(!flat_set.is_empty());
            assert!(flat_set.is_subset(&tall_set), "the roof covers the footprint");
            // One colour: a wall would be shaded differently from the roof.
            let colours: std::collections::HashSet<_> =
                tall.chunks(4).filter(|p| p[0] | p[1] | p[2] != 0).map(|p| [p[0], p[1], p[2]]).collect();
            assert_eq!(colours.len(), 1, "only the roof shows: {colours:?}");
            let width = |set: &std::collections::HashSet<(u32, u32)>| {
                let xs = set.iter().map(|p| p.0);
                f64::from(xs.clone().max().unwrap() - xs.min().unwrap() + 1)
            };
            let d = cam.distance() / f64::from(m2w());
            let expected = d / (d - f64::from(height));
            let grown = width(&tall_set) / width(&flat_set);
            assert!((grown - expected).abs() < 0.05, "roof grew {grown:.3}x, perspective predicts {expected:.3}x (eye {d:.0} m up)");
        });
    }

    #[test]
    fn the_nearer_box_wins_in_either_draw_order() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(60.0, 0.0);
            let near = ((1900, 2300, 2200, 2500), 80.0);
            let far = ((1900, 1700, 2200, 1900), 80.0);
            // Two colours, so the check does not lean on the lighting
            // telling a near wall from a far roof.
            let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
            let blue = Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
            let (near_plan, far_plan) = (box_plan(&[near], red), box_plan(&[far], blue));
            let both = |first: &PaintPlan, second: &PaintPlan| PaintPlan { ops: first.ops.iter().chain(&second.ops).cloned().collect() };
            let a = draw(&ctx, &pipeline, &cam, &both(&near_plan, &far_plan), params(m2w())).await;
            let b = draw(&ctx, &pipeline, &cam, &both(&far_plan, &near_plan), params(m2w())).await;
            // The 0.03 ambient lifts every channel, so "red" is red-dominant.
            let reds = a.chunks(4).filter(|p| u32::from(p[0]) > u32::from(p[2]) + 64).count();
            let blues = a.chunks(4).filter(|p| u32::from(p[2]) > u32::from(p[0]) + 64).count();
            assert!(reds > 0 && blues > 0, "both boxes show: {reds} red, {blues} blue px");
            assert!(a == b, "draw order changed the frame: depth is not doing its job");
        });
    }

    const TRANSLUCENT: [ExtrusionPass; 2] = [ExtrusionPass::Prepass, ExtrusionPass::Color { stencil_ref: 1 }];

    /// The brief's test: a box drawn twice at the same place (two OSM
    /// outlines of one building, a duplicate across a tile seam) composites
    /// ONCE. Both copies pass the colour pass's `LessEqual` at the
    /// prepass's depth, so only the stencil stops the second blend.
    #[test]
    fn a_translucent_duplicate_composites_once() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(60.0, 30.0);
            let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
            let square = ((1800, 1800, 2300, 2300), 100.0);
            let mut p = params(m2w());
            p.opacity = 0.5;
            let once = draw_passes(&ctx, &pipeline, &cam, &box_plan(&[square], white), p, &TRANSLUCENT).await;
            let twice = draw_passes(&ctx, &pipeline, &cam, &box_plan(&[square, square], white), p, &TRANSLUCENT).await;
            let worst = once.iter().zip(&twice).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(worst <= 1, "a coincident duplicate blended twice (worst channel delta {worst}): the stencil is not working");
            assert!(once.chunks(4).any(|px| px[0] > 20), "the box drew");
            // The control: the same two copies with the stencil left out
            // (a plain blended pass after the prepass) DO double up, so the
            // assertion above is the stencil's doing, not a depth accident.
            let unstencilled = draw_passes(
                &ctx,
                &pipeline,
                &cam,
                &box_plan(&[square, square], white),
                p,
                &[ExtrusionPass::Prepass, ExtrusionPass::Color { stencil_ref: 1 }, ExtrusionPass::Color { stencil_ref: 2 }],
            )
            .await;
            let doubled = once.iter().zip(&unstencilled).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(doubled > 20, "a second reference must blend again (delta {doubled}): the control is not a control");
        });
    }

    /// No inner walls: a translucent near box in front of a far box of the
    /// SAME layer shows only its own nearest surface. Where the near box
    /// covers the far one, the frame equals the near box alone, to the
    /// channel, in either draw order: the far box's walls fail the colour
    /// pass's depth test against the prepass. The control drops the
    /// prepass and draws the far box first: its colour then owns the
    /// overlap (the stencil keeps the near box from even blending over
    /// it), which is the defect the prepass exists to prevent.
    #[test]
    fn a_translucent_layer_hides_its_own_far_walls() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(60.0, 0.0);
            let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
            let blue = Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
            let (near, far) = (box_plan(&[((1900, 2300, 2200, 2500), 80.0)], red), box_plan(&[((1900, 1700, 2200, 1900), 160.0)], blue));
            let both = |first: &PaintPlan, second: &PaintPlan| PaintPlan { ops: first.ops.iter().chain(&second.ops).cloned().collect() };
            let mut p = params(m2w());
            p.opacity = 0.5;
            let near_only = draw_passes(&ctx, &pipeline, &cam, &near, p, &TRANSLUCENT).await;
            let far_only = draw_passes(&ctx, &pipeline, &cam, &far, p, &TRANSLUCENT).await;
            let near_first = draw_passes(&ctx, &pipeline, &cam, &both(&near, &far), p, &TRANSLUCENT).await;
            let far_first = draw_passes(&ctx, &pipeline, &cam, &both(&far, &near), p, &TRANSLUCENT).await;
            let no_prepass =
                draw_passes(&ctx, &pipeline, &cam, &both(&far, &near), p, &[ExtrusionPass::Color { stencil_ref: 1 }]).await;
            let lit = |px: &[u8]| px[0] | px[1] | px[2] != 0;
            // Pixels both boxes cover on their own: the near box in front.
            let overlap: Vec<usize> = (0..near_only.len() / 4)
                .filter(|&i| lit(&near_only[i * 4..i * 4 + 4]) && lit(&far_only[i * 4..i * 4 + 4]))
                .collect();
            assert!(overlap.len() > 100, "the near box must hide part of the far one: {} px", overlap.len());
            let worst = |frame: &[u8]| {
                overlap.iter().flat_map(|&i| (0..3).map(move |c| i * 4 + c)).map(|k| near_only[k].abs_diff(frame[k])).max().unwrap()
            };
            assert!(worst(&near_first) <= 1, "the far box shows through the near one (worst delta {})", worst(&near_first));
            assert!(worst(&far_first) <= 1, "draw order leaked the far box through (worst delta {})", worst(&far_first));
            let blue_through = overlap.iter().filter(|&&i| no_prepass[i * 4 + 2] > no_prepass[i * 4] + 32).count();
            assert!(
                blue_through * 2 > overlap.len(),
                "without the prepass the far box should own the overlap ({blue_through}/{} px blue): the control is not a control",
                overlap.len()
            );
        });
    }

    #[test]
    fn back_face_culling_matches_no_culling_and_front_culling_does_not() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let cam = camera(60.0, 30.0);
            let plan = box_plan(&[((1800, 1800, 2300, 2300), 100.0)], Rgba { r: 0.6, g: 0.6, b: 0.6, a: 1.0 });
            let back = draw_with_cull(&ctx, &cam, &plan, Some(wgpu::Face::Back)).await;
            let none = draw_with_cull(&ctx, &cam, &plan, None).await;
            let front = draw_with_cull(&ctx, &cam, &plan, Some(wgpu::Face::Front)).await;
            assert!(painted(&none).count() > 0, "the box paints");
            assert!(back == none, "EXTRUSION_FRONT_FACE is wrong: culling Back changed the picture");
            assert!(front != none, "culling Front must remove the visible faces");
        });
    }

    /// Review Finding 4: one pass holding an extrusion draw AND a flat
    /// fill. The extrusion leaves groups 1-3 bound to its own; the fill
    /// that follows must re-bind its model group (and draw through its own
    /// pipeline layout) or it lands where the extrusion's model puts it.
    /// The fill's model is deliberately a DIFFERENT matrix (extent 2048,
    /// so its geometry is twice as big on screen), so a stale group 1 would
    /// move it. All inside a validation scope: a layout mismatch between
    /// the two pipelines is a wgpu error, which the device handler would
    /// only log.
    #[test]
    fn a_fill_after_an_extrusion_in_one_pass_rebinds_its_groups() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(60.0, 0.0);
            let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
            let green = Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
            let boxes = box_plan(&[((1800, 1800, 2300, 2300), 100.0)], red);
            let fill_plan = PaintPlan {
                ops: vec![PaintOp::Fill {
                    color_from: green,
                    color_to: green,
                    geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(geo_types::Polygon::new(
                        geo_types::LineString::from(vec![(200, 900), (700, 900), (700, 1100), (200, 1100), (200, 900)]),
                        vec![],
                    ))),
                }],
            };
            let frame = scoped(&ctx, async {
                let mesh = tessellate_extrusions(&boxes);
                let gpu = pipeline.create_extrusion_buffers(&ctx, &mesh).expect("non-empty");
                let tile_matrix = crate::render::combined_tile_matrix(&cam, TILE, 4096.0);
                let model = pipeline.test_model_bind_group(&ctx, tile_matrix.to_cols_array_2d(), 0.0, 0.0);
                let fill_mesh = crate::render::tessellate_plan(
                    &fill_plan,
                    4096,
                    crate::render::mesh::COORD_SCALE,
                    crate::render::subdivide::Subdivision::NONE,
                );
                let fill_buffers = pipeline.create_tile_buffers(&ctx, &fill_mesh);
                let fill_matrix = crate::render::combined_tile_matrix(&cam, TILE, 2048.0);
                let fill_model = pipeline.test_model_bind_group(&ctx, fill_matrix.to_cols_array_2d(), 0.0, 0.0);
                let mut params_gpu = ExtrusionParamsGpu::new();
                params_gpu.write(&ctx, pipeline.extrusion(), &[params(m2w())]);
                let span = &gpu.chunks[0].spans[0];
                let extrusion = TileDraw::Extrusion {
                    chunk: &gpu.chunks[0],
                    model: &model,
                    params: params_gpu.bind_group(0),
                    first_index: span.first_index,
                    index_count: span.index_count,
                    pass: ExtrusionPass::Opaque,
                };
                let fill = TileDraw::Legacy(&fill_buffers, &fill_model);
                let render_draws = crate::render::gpu::tests::render_tile_draws;
                let black = wgpu::Color::BLACK;
                let fill_alone = render_draws(&ctx, &pipeline, &cam, SIZE, black, std::slice::from_ref(&fill)).await;
                let box_alone = render_draws(&ctx, &pipeline, &cam, SIZE, black, std::slice::from_ref(&extrusion)).await;
                let mixed = render_draws(&ctx, &pipeline, &cam, SIZE, black, &[extrusion, fill]).await;
                (fill_alone, box_alone, mixed)
            })
            .await;
            let (fill_alone, box_alone, mixed) = frame;
            let px = |f: &[u8], i: usize| [f[4 * i], f[4 * i + 1], f[4 * i + 2]];
            let n = (SIZE * SIZE) as usize;
            let fill_only: Vec<usize> =
                (0..n).filter(|&i| px(&fill_alone, i) != [0; 3] && px(&box_alone, i) == [0; 3]).collect();
            assert!(fill_only.len() > 100, "the fill must show clear of the box: {} px", fill_only.len());
            let moved = fill_only.iter().filter(|&&i| px(&mixed, i) != px(&fill_alone, i)).count();
            assert_eq!(moved, 0, "after an extrusion draw the fill drew elsewhere or otherwise: {moved} px differ");
            let box_px = (0..n).filter(|&i| px(&box_alone, i) != [0; 3]).count();
            let box_kept = (0..n).filter(|&i| px(&box_alone, i) != [0; 3] && px(&mixed, i) == px(&box_alone, i)).count();
            assert!(box_px > 50 && box_kept * 10 >= box_px * 9, "the box draws in the mixed pass: {box_kept}/{box_px} px");
        });
    }

    /// A buildings-only tile (review F3): its flat mesh is EMPTY, yet it
    /// installs, both the plain and the basemap (compact-split) way, and
    /// its extrusion draws beside the empty flat draw with no validation
    /// error -- what `MapView` and `capture` now do for such a tile.
    #[test]
    fn a_tile_with_buildings_and_no_flat_geometry_installs_and_draws() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let cam = camera(60.0, 0.0);
            let boxes = box_plan(&[((1800, 1800, 2300, 2300), 100.0)], Rgba { r: 0.6, g: 0.6, b: 0.6, a: 1.0 });
            let painted_px = scoped(&ctx, async {
                let empty = crate::render::TileMesh::default();
                let plain = pipeline.create_tile_buffers(&ctx, &empty);
                let basemap = pipeline.install_basemap_mesh_masked(&ctx, &empty, &[], &Default::default());
                let mesh = tessellate_extrusions(&boxes);
                let gpu = pipeline.create_extrusion_buffers(&ctx, &mesh).expect("non-empty");
                let matrix = crate::render::combined_tile_matrix(&cam, TILE, 4096.0);
                let model = pipeline.test_model_bind_group(&ctx, matrix.to_cols_array_2d(), 0.0, 0.0);
                let mut params_gpu = ExtrusionParamsGpu::new();
                params_gpu.write(&ctx, pipeline.extrusion(), &[params(m2w())]);
                let span = &gpu.chunks[0].spans[0];
                let draws = [
                    TileDraw::Legacy(&plain, &model),
                    TileDraw::Legacy(&basemap, &model),
                    TileDraw::Extrusion {
                        chunk: &gpu.chunks[0],
                        model: &model,
                        params: params_gpu.bind_group(0),
                        first_index: span.first_index,
                        index_count: span.index_count,
                        pass: ExtrusionPass::Opaque,
                    },
                ];
                let px = crate::render::gpu::tests::render_tile_draws(&ctx, &pipeline, &cam, SIZE, wgpu::Color::BLACK, &draws).await;
                painted(&px).count()
            })
            .await;
            assert!(painted_px > 50, "the buildings draw: {painted_px} px");
        });
    }

    /// `push_extrusion_draws` is layer-major (R4): every tile's spans of
    /// the first step's group, then every tile's spans of the next.
    #[test]
    fn push_extrusion_draws_is_layer_major_over_tiles() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            let grey = Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
            let mut plan = box_plan(&[((100, 100, 200, 200), 10.0), ((300, 300, 400, 400), 10.0)], grey);
            if let PaintOp::Extrusion { group, .. } = &mut plan.ops[1] {
                *group = 1;
            }
            let (a, b, model, params_gpu) = scoped(&ctx, async {
                let mesh = tessellate_extrusions(&plan);
                let a = pipeline.create_extrusion_buffers(&ctx, &mesh).expect("non-empty");
                let b = pipeline.create_extrusion_buffers(&ctx, &mesh).expect("non-empty");
                let model = pipeline.test_model_bind_group(&ctx, glam::Mat4::IDENTITY.to_cols_array_2d(), 0.0, 0.0);
                let mut params_gpu = ExtrusionParamsGpu::new();
                params_gpu.write(&ctx, pipeline.extrusion(), &[params(m2w()), params(m2w())]);
                (a, b, model, params_gpu)
            })
            .await;
            let layer = |group| crate::render::frame_plan::ExtrusionLayerFrame { group, params: params(m2w()), translucent: false };
            let frame = crate::render::frame_plan::ExtrusionPlan {
                layers: vec![layer(1), layer(0)],
                steps: vec![
                    crate::render::frame_plan::ExtrusionStep { layer_index: 0, pass: ExtrusionPass::Opaque },
                    crate::render::frame_plan::ExtrusionStep { layer_index: 1, pass: ExtrusionPass::Opaque },
                ],
                meters_to_world: m2w(),
            };
            let mut out = Vec::new();
            push_extrusion_draws(&frame, &[(&a, &model), (&b, &model)], &params_gpu, &mut out);
            let order: Vec<(bool, u32)> = out
                .iter()
                .map(|d| match d {
                    TileDraw::Extrusion { chunk, first_index, .. } => (std::ptr::eq(*chunk, &a.chunks[0]), *first_index),
                    _ => unreachable!(),
                })
                .collect();
            let first_of = |group: u16| a.chunks[0].spans.iter().find(|s| s.group == group).unwrap().first_index;
            assert_eq!(order, vec![(true, first_of(1)), (false, first_of(1)), (true, first_of(0)), (false, first_of(0))]);

            // The plan is the one source of truth for the stencil reference
            // (Task 8 review, finding 2): a `Color` step's ref reaches every
            // draw as written, not re-derived from the layer index.
            let frame = crate::render::frame_plan::ExtrusionPlan {
                steps: vec![crate::render::frame_plan::ExtrusionStep { layer_index: 0, pass: ExtrusionPass::Color { stencil_ref: 42 } }],
                ..frame
            };
            let mut out = Vec::new();
            push_extrusion_draws(&frame, &[(&a, &model), (&b, &model)], &params_gpu, &mut out);
            assert_eq!(out.len(), 2);
            assert!(out.iter().all(|d| matches!(d, TileDraw::Extrusion { pass: ExtrusionPass::Color { stencil_ref: 42 }, .. })));
        });
    }

    #[test]
    fn an_empty_mesh_uploads_no_buffers() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = tile_pipeline(&ctx).await;
            assert!(pipeline.create_extrusion_buffers(&ctx, &crate::render::extrusion::ExtrusionMesh::default()).is_none());
        });
    }

    #[test]
    fn the_extrusion_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("extrusion_shader.wgsl"),
            &[("vs_extrusion", naga::ShaderStage::Vertex), ("fs_extrusion", naga::ShaderStage::Fragment)],
        );
    }

    #[test]
    fn the_extrusion_shader_uses_four_inter_stage_locations() {
        let src = include_str!("extrusion_shader.wgsl");
        let out = &src[src.find("struct VertexOutput").unwrap()..];
        let out = &out[..out.find("};").unwrap()];
        assert_eq!(out.matches("@location(").count(), 4, "color + clip_coord + shading + sphere; the floor asked for is 12");
    }

    #[test]
    fn the_extrusion_shader_declares_model_and_camera_exactly_as_the_main_shader() {
        // `shader.wgsl` closes a struct with a bare `}` at column 0.
        let block = |src: &str, name: &str| {
            let s = &src[src.find(&format!("struct {name} {{")).unwrap()..];
            s[..s.find("\n}").unwrap()].to_string()
        };
        let main = include_str!("shader.wgsl");
        let ext = include_str!("extrusion_shader.wgsl");
        assert_eq!(block(ext, "Camera"), block(main, "Camera"));
        assert_eq!(block(ext, "Model"), block(main, "Model"));
    }

    #[test]
    fn the_vertex_attributes_are_webgl2_core_and_12_bytes() {
        assert_eq!(EXTRUSION_VERTEX_LAYOUT.array_stride, 12);
        let formats: Vec<_> = EXTRUSION_VERTEX_LAYOUT.attributes.iter().map(|a| a.format).collect();
        assert_eq!(formats, [wgpu::VertexFormat::Sint16x2, wgpu::VertexFormat::Snorm8x4, wgpu::VertexFormat::Uint16x2]);
        assert_eq!(std::mem::size_of::<crate::render::extrusion::ExtrusionVertex>(), 12);
    }
}
