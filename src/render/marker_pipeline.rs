//! Story markers (spec §6.3-§6.4): screen-space discs with a stroke and a
//! blur, one quad each, drawn analytically by `marker_shader.wgsl` -- the
//! story's "circle instance" for `circle-stroke-width`/`-color`,
//! `circle-blur` and `circle-translate`, which the overlay's tessellated
//! dots do not carry. Modelled on [`super::plate_pipeline`]: no texture,
//! a viewport uniform and a grow-and-keep vertex buffer, drawn into an
//! already-open depth-less pass.

use crate::render::gpu::GpuContext;

/// One marker disc, physical pixels and straight sRGB in 0..1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MarkerInstance {
    /// The disc's centre (translate already applied).
    pub center: [f32; 2],
    /// The fill's radius; the stroke lies outside it.
    pub radius_px: f32,
    pub stroke_px: f32,
    /// MapLibre's `circle-blur`: the fraction of the outer radius the edge
    /// fades over.
    pub blur: f32,
    pub fill: [f32; 4],
    pub stroke: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MarkerVertex {
    anchor_px: [f32; 2],
    offset_px: [f32; 2],
    shape: [f32; 4],
    fill: [f32; 4],
    stroke: [f32; 4],
}

const MARKER_VERTEX_ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32x4,
    3 => Float32x4,
    4 => Float32x4,
];

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    size: [f32; 2],
    _pad: [f32; 2],
}

fn push_quad(vertices: &mut Vec<MarkerVertex>, m: &MarkerInstance) {
    // The outer radius plus a pixel: the fade lies inside it (MapLibre's
    // blur and antialias both eat into the disc), the pixel is slack.
    let half = (m.radius_px.max(0.0) + m.stroke_px.max(0.0)) + 1.0;
    let shape = [m.radius_px.max(0.0), m.stroke_px.max(0.0), m.blur.max(0.0), 0.0];
    let corner = |dx: f32, dy: f32| MarkerVertex { anchor_px: m.center, offset_px: [dx, dy], shape, fill: m.fill, stroke: m.stroke };
    let (tl, tr, bl, br) = (corner(-half, -half), corner(half, -half), corner(-half, half), corner(half, half));
    vertices.extend_from_slice(&[tl, tr, bl, tr, br, bl]);
}

/// The marker pipeline: one per session, kept.
pub(crate) struct MarkerPipeline {
    pipeline: wgpu::RenderPipeline,
    viewport_buffer: crate::render::alloc::TrackedBuffer,
    viewport_bind_group: wgpu::BindGroup,
    vertex_buffer: Option<crate::render::alloc::TrackedBuffer>,
}

impl MarkerPipeline {
    pub(crate) fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps marker shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("marker_shader.wgsl").into()),
        });
        let viewport_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("marker-viewport-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let viewport_buffer = crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("marker-viewport-uniform"),
            size: std::mem::size_of::<ViewportUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let viewport_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("marker-viewport-bind-group"),
            layout: &viewport_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: viewport_buffer.as_entire_binding() }],
        });
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("marker-pipeline-layout"),
            bind_group_layouts: &[Some(&viewport_bgl)],
            immediate_size: 0,
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<MarkerVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &MARKER_VERTEX_ATTRS,
        };
        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("marker-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(vertex_layout)],
            },
            primitive: wgpu::PrimitiveState::default(),
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

    /// Draws `instances`, in order, into an already-open `pass`.
    pub(crate) fn render(&mut self, ctx: &GpuContext, pass: &mut wgpu::RenderPass, instances: &[MarkerInstance], viewport_width_px: f32, viewport_height_px: f32) {
        if instances.is_empty() {
            return;
        }
        let mut vertices = Vec::with_capacity(instances.len() * 6);
        for m in instances {
            push_quad(&mut vertices, m);
        }
        ctx.queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::cast_slice(&[ViewportUniform { size: [viewport_width_px, viewport_height_px], _pad: [0.0, 0.0] }]),
        );
        let bytes: &[u8] = bytemuck::cast_slice(&vertices);
        match self.vertex_buffer.as_ref() {
            Some(buffer) if buffer.tracked_size() >= bytes.len() as u64 => ctx.queue.write_buffer(buffer, 0, bytes),
            _ => {
                self.vertex_buffer = Some(crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
                    label: Some("marker-vertices"),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    async fn render_markers_to_pixels(ctx: &GpuContext, pipeline: &mut MarkerPipeline, instances: &[MarkerInstance], size: u32) -> Vec<u8> {
        let color_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-color"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("marker-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pipeline.render(ctx, &mut pass, instances, size as f32, size as f32);
        }

        let bytes_per_row = (size * 4).next_multiple_of(256);
        let output_buffer = crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (bytes_per_row * size) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &color_texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &output_buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(size) },
            },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));

        let slice = output_buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| { tx.send(result).unwrap(); });
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();

        let data = slice.get_mapped_range().expect("buffer should be mapped after poll");
        let mut pixels = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            let row_start = (y * bytes_per_row) as usize;
            pixels.extend_from_slice(&data[row_start..row_start + (size * 4) as usize]);
        }
        pixels
    }

    /// A stroked disc: the fill inside, the stroke outside it, nothing
    /// past the stroke; a ring with no fill is clear inside.
    #[test]
    fn a_marker_draws_its_fill_inside_and_its_stroke_outside() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = MarkerPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 64u32;
            let red = [1.0, 0.0, 0.0, 1.0];
            let blue = [0.0, 0.0, 1.0, 1.0];
            let m = MarkerInstance { center: [32.0, 32.0], radius_px: 10.0, stroke_px: 6.0, blur: 0.0, fill: red, stroke: blue };
            let px = render_markers_to_pixels(&ctx, &mut pipeline, &[m], size).await;
            let at = |x: u32, y: u32| { let o = ((y * size + x) * 4) as usize; [px[o], px[o + 1], px[o + 2], px[o + 3]] };
            assert_eq!(at(32, 32), [255, 0, 0, 255], "fill at the centre");
            assert_eq!(at(32 + 13, 32), [0, 0, 255, 255], "stroke 13 px out");
            assert_eq!(at(32 + 20, 32)[3], 0, "nothing past the stroke");
            let ring = MarkerInstance { fill: [0.0; 4], ..m };
            let px = render_markers_to_pixels(&ctx, &mut pipeline, &[ring], size).await;
            assert_eq!(px[((32 * size + 32) * 4 + 3) as usize], 0, "a ring is clear inside");
        });
    }
}
