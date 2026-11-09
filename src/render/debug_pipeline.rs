//! Draws the tile-status debug overlay's already-built screen-space
//! triangles.
//!
//! A sibling to `PlatePipeline`, not an extension of it, for this project's
//! usual reason: a plate is an axis-aligned rounded rect with an analytic
//! SDF, and a projected tile under rotation or pitch is neither
//! axis-aligned nor rounded. Bolting an "and sometimes the quad is an
//! arbitrary trapezoid with no corner radius" branch onto that shader would
//! make both paths harder to read for a feature only one of them needs.
//!
//! Everything about it is deliberately dull: one vertex buffer of
//! `(position, colour)`, one draw call, no bind group but the viewport
//! uniform every screen-space pipeline here already needs, and no work at
//! all on a frame where the overlay is off (`render` returns immediately on
//! an empty slice, and `MapView` does not even build the slice).
//!
//! The buffer is GROWN-AND-KEPT for the same reason `PlatePipeline`'s is:
//! `gpu_buffers_created_total` (see `render::alloc`) is this crate's
//! evidence that buffers are reused rather than recreated, and a per-frame
//! `create_buffer_init` here would make it climb forever no matter what
//! every other pipeline did.

use crate::render::debug_tiles::DebugVertex;
use crate::render::gpu::GpuContext;

const DEBUG_VERTEX_ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x4,
];

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    size: [f32; 2],
    _pad: [f32; 2],
}

pub(crate) struct DebugPipeline {
    pipeline: wgpu::RenderPipeline,
    viewport_buffer: crate::render::alloc::TrackedBuffer,
    viewport_bind_group: wgpu::BindGroup,
    /// `None` until the first non-empty `render` call. A session that never
    /// turns the overlay on therefore never allocates it -- which is most
    /// of the "costs nothing when off" claim, the rest being that
    /// `MapView::paint` never builds the vertices.
    vertex_buffer: Option<crate::render::alloc::TrackedBuffer>,
}

impl DebugPipeline {
    pub(crate) fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps debug-tiles shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("debug_shader.wgsl").into()),
        });

        let viewport_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("debug-tiles-viewport-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let viewport_buffer = crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("debug-tiles-viewport-uniform"),
            size: std::mem::size_of::<ViewportUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let viewport_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("debug-tiles-viewport-bind-group"),
            layout: &viewport_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: viewport_buffer.as_entire_binding() }],
        });

        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("debug-tiles-pipeline-layout"),
            bind_group_layouts: &[Some(&viewport_bgl)],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<DebugVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &DEBUG_VERTEX_ATTRS,
        };

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("debug-tiles-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(vertex_layout)],
            },
            // `cull_mode: None` (the default) matters here rather than
            // being incidental: a tile's projected corner order flips
            // winding as the camera rotates past 180 degrees of bearing,
            // and a culled overlay that vanishes when you turn the map
            // around would be read as a renderer bug rather than as a
            // debug-overlay bug.
            primitive: wgpu::PrimitiveState::default(),
            // No depth attachment: this draws into the same depth-less
            // `symbol-overlay-pass` the icon, plate and text pipelines
            // already share.
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self { pipeline, viewport_buffer, viewport_bind_group, vertex_buffer: None }
    }

    /// Draws `vertices` into an ALREADY-OPEN pass -- mirrors
    /// `PlatePipeline::render`'s shape so `map_view.rs` can put the overlay
    /// into the same second pass icons, plates and text already share,
    /// rather than opening a fourth.
    pub(crate) fn render(
        &mut self,
        ctx: &GpuContext,
        pass: &mut wgpu::RenderPass,
        vertices: &[DebugVertex],
        viewport_width_px: f32,
        viewport_height_px: f32,
    ) {
        if vertices.is_empty() {
            return;
        }
        ctx.queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::cast_slice(&[ViewportUniform {
                size: [viewport_width_px, viewport_height_px],
                _pad: [0.0, 0.0],
            }]),
        );
        let bytes: &[u8] = bytemuck::cast_slice(vertices);
        match self.vertex_buffer.as_ref() {
            Some(buffer) if buffer.tracked_size() >= bytes.len() as u64 => {
                ctx.queue.write_buffer(buffer, 0, bytes);
            }
            _ => {
                self.vertex_buffer = Some(crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
                    label: Some("debug-tiles-vertices"),
                    contents: bytes,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                }));
            }
        }
        let vertex_buffer = self.vertex_buffer.as_ref().expect("just written or just allocated");
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.viewport_bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.draw(0..vertices.len() as u32, 0..1);
        crate::render::alloc::note_draw_calls(1);
    }
}
