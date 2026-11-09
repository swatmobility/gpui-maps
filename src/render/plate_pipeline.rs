
use crate::render::gpu::GpuContext;

/// One rounded-rect "plate" to draw this frame, in PHYSICAL pixels
/// (top-left origin, y-down) -- the same screen-space convention as
/// `IconInstance`. Today's only caller is the attribution credit's
/// translucent backing plate, but nothing here is specific to that: this is
/// a general flat-colour rounded rect.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlateInstance {
    pub screen_x: f32,
    pub screen_y: f32,
    pub width: f32,
    pub height: f32,
    /// Corner radius in physical pixels. Clamped to at most half the
    /// shorter side by `push_quad` -- a radius larger than that would make
    /// `rounded_box_sdf`'s `q` term go the wrong sign and the shape would
    /// pinch rather than degrade to a stadium/circle, which is the correct
    /// clamp for a radius that large.
    pub radius_px: f32,
    /// Straight (non-premultiplied) sRGB, alpha included.
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PlateVertex {
    anchor_px: [f32; 2],
    offset_px: [f32; 2],
    half_size: [f32; 2],
    radius_px: f32,
    color: [f32; 4],
}

const PLATE_VERTEX_ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32x2,
    3 => Float32,
    4 => Float32x4,
];

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    size: [f32; 2],
    _pad: [f32; 2],
}

/// Emits the six vertices (two triangles) of one plate.
fn push_quad(vertices: &mut Vec<PlateVertex>, instance: &PlateInstance) {
    let half_w = instance.width / 2.0;
    let half_h = instance.height / 2.0;
    let anchor_px = [instance.screen_x + half_w, instance.screen_y + half_h];
    // A radius past the shorter half-side inverts `rounded_box_sdf`'s inner
    // term rather than degrading to a stadium/circle -- clamp here so a
    // caller-supplied radius can never do that, whatever the plate's own
    // (possibly content-driven) size turns out to be.
    let half_size = [half_w, half_h];
    let radius_px = instance.radius_px.max(0.0).min(half_w.min(half_h));
    let color = instance.color;

    let corner = |dx: f32, dy: f32| PlateVertex {
        anchor_px,
        offset_px: [dx, dy],
        half_size,
        radius_px,
        color,
    };
    let top_left = corner(-half_w, -half_h);
    let top_right = corner(half_w, -half_h);
    let bottom_left = corner(-half_w, half_h);
    let bottom_right = corner(half_w, half_h);

    vertices.extend_from_slice(&[top_left, top_right, bottom_left, top_right, bottom_right, bottom_left]);
}

/// A small, dedicated flat-colour rounded-rect pipeline -- a sibling to
/// `IconPipeline` and `TilePipeline`, not an extension of either, following
/// this project's existing single-responsibility-per-pipeline convention.
/// Unlike `IconPipeline` this needs no atlas texture or bind group at all:
/// the shape comes from an analytic SDF evaluated per-fragment
/// (`plate_shader.wgsl`), not from sampled pixels, so the only resource this
/// pipeline owns is the viewport uniform every screen-space pipeline in this
/// renderer needs.
pub(crate) struct PlatePipeline {
    pipeline: wgpu::RenderPipeline,
    viewport_buffer: crate::render::alloc::TrackedBuffer,
    viewport_bind_group: wgpu::BindGroup,
    /// The quad vertices, allocated once and then GROWN-AND-KEPT: `render`
    /// rewrites it in place every frame and only reallocates when this
    /// frame's plates do not fit, never to shrink it back. `None` until the
    /// first `render` call, because the size is not known before one.
    ///
    /// It exists for the same reason `viewport_buffer` is built in `new`
    /// rather than per call. `render` used to `create_buffer_init` a fresh
    /// buffer on every call -- once per frame, at frame rate, for one
    /// 6-vertex quad. The bytes were trivial (~144 a frame); the counter
    /// damage was not. `gpu_buffers_created_total` is the crate's evidence
    /// that buffers are being reused rather than recreated (see
    /// `render::alloc`), and a per-frame allocation here made it climb
    /// forever no matter what every other pipeline did.
    vertex_buffer: Option<crate::render::alloc::TrackedBuffer>,
}

impl PlatePipeline {
    pub(crate) fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps plate shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("plate_shader.wgsl").into()),
        });

        let viewport_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("plate-viewport-bgl"),
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
            label: Some("plate-viewport-uniform"),
            size: std::mem::size_of::<ViewportUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let viewport_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("plate-viewport-bind-group"),
            layout: &viewport_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: viewport_buffer.as_entire_binding() }],
        });

        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("plate-pipeline-layout"),
            bind_group_layouts: &[Some(&viewport_bgl)],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<PlateVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &PLATE_VERTEX_ATTRS,
        };

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("plate-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(vertex_layout)],
            },
            primitive: wgpu::PrimitiveState::default(),
            // No depth attachment: this pipeline draws into the same
            // depth-less `symbol-overlay-pass` `IconPipeline` and glyphon
            // already share (`map_view.rs`) -- see `IconPipeline::new`'s own
            // doc comment for why that pass has none.
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    // A translucent plate over the map/tiles already in the
                    // target -- same reasoning as `IconPipeline`'s blend
                    // state.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self { pipeline, viewport_buffer, viewport_bind_group, vertex_buffer: None }
    }

    /// Draws every `instances` entry into an ALREADY-OPEN `pass` -- mirrors
    /// `IconPipeline::render`'s shape, so `map_view.rs` can draw the plate
    /// into the same second pass Task 3/4 already opened for text and icons,
    /// not a fourth pass.
    ///
    /// Takes `&mut self` because it writes this frame's quads into the
    /// pipeline's own kept `vertex_buffer` (see that field), growing it only
    /// when a frame's plates do not fit. Nothing is allocated on a steady
    /// frame.
    pub(crate) fn render(
        &mut self,
        ctx: &GpuContext,
        pass: &mut wgpu::RenderPass,
        instances: &[PlateInstance],
        viewport_width_px: f32,
        viewport_height_px: f32,
    ) {
        if instances.is_empty() {
            return;
        }
        let mut vertices = Vec::with_capacity(instances.len() * 6);
        for instance in instances {
            push_quad(&mut vertices, instance);
        }
        ctx.queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::cast_slice(&[ViewportUniform {
                size: [viewport_width_px, viewport_height_px],
                _pad: [0.0, 0.0],
            }]),
        );
        // Grow-and-keep. `vertices` is never empty here (the early return
        // above), so there is no zero-length `write_buffer` to guard
        // against, and a buffer wider than this frame's quads is harmless:
        // `draw` below is bounded by `vertices.len()`, not by the buffer's
        // capacity, so last frame's leftover vertices past the end are
        // never read.
        let bytes: &[u8] = bytemuck::cast_slice(&vertices);
        match self.vertex_buffer.as_ref() {
            Some(buffer) if buffer.tracked_size() >= bytes.len() as u64 => {
                ctx.queue.write_buffer(buffer, 0, bytes);
            }
            // Either the first frame, or more plates than the kept buffer
            // holds. `create_buffer_init` sizes to `bytes` exactly, which is
            // already `max(needed, current)`: this arm is only reached when
            // `needed` exceeds what is currently held.
            _ => {
                self.vertex_buffer = Some(crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
                    label: Some("plate-vertices"),
                    contents: bytes,
                    // `COPY_DST` so the branch above can rewrite it in
                    // place; without it every frame lands here.
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
mod tests {
    use super::*;

    #[test]
    fn push_quad_emits_anchor_centre_and_corner_offsets() {
        let instance =
            PlateInstance { screen_x: 100.0, screen_y: 200.0, width: 10.0, height: 20.0, radius_px: 2.0, color: [1.0, 1.0, 1.0, 0.5] };
        let mut vertices = Vec::new();
        push_quad(&mut vertices, &instance);

        assert_eq!(vertices.len(), 6, "two triangles");
        for v in &vertices {
            assert_eq!(v.anchor_px, [105.0, 210.0]);
            assert_eq!(v.half_size, [5.0, 10.0]);
            assert_eq!(v.radius_px, 2.0);
            assert_eq!(v.color, [1.0, 1.0, 1.0, 0.5]);
        }
        let mut offsets: Vec<[f32; 2]> = vertices.iter().map(|v| v.offset_px).collect();
        offsets.sort_by(|a, b| a.partial_cmp(b).expect("no NaNs"));
        offsets.dedup();
        assert_eq!(offsets, vec![[-5.0, -10.0], [-5.0, 10.0], [5.0, -10.0], [5.0, 10.0]]);
    }

    /// A radius wider than the shorter half-side would flip
    /// `rounded_box_sdf`'s inner `min(max(q.x, q.y), 0.0)` term negative in
    /// the wrong place and pinch the shape instead of degrading to a
    /// stadium/circle -- clamped here so a caller-supplied radius can never
    /// reach the shader un-clamped.
    #[test]
    fn a_radius_past_the_shorter_half_side_is_clamped_to_it() {
        let instance = PlateInstance { screen_x: 0.0, screen_y: 0.0, width: 10.0, height: 40.0, radius_px: 1000.0, color: [1.0; 4] };
        let mut vertices = Vec::new();
        push_quad(&mut vertices, &instance);
        assert!(vertices.iter().all(|v| v.radius_px == 5.0), "clamped to half_size.x (the shorter side)");
    }

    /// Extracted readback plumbing, mirroring `icon_pipeline`'s own
    /// `render_icons_to_pixels`: opens an offscreen `size`x`size` render
    /// target, draws `instances` into it, and returns tightly-packed
    /// (no row padding) RGBA8 bytes.
    async fn render_plates_to_pixels(ctx: &GpuContext, pipeline: &mut PlatePipeline, instances: &[PlateInstance], size: u32) -> Vec<u8> {
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
                label: Some("plate-test-pass"),
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

    /// Headless-wgpu proof that `PlatePipeline` actually draws a filled,
    /// translucent rect where asked, mirroring `icon_pipeline`'s own
    /// `draws_an_uploaded_icon_at_the_expected_screen_position`.
    #[test]
    fn draws_a_translucent_plate_at_the_expected_screen_position() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = PlatePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            let instances = [PlateInstance { screen_x: 16.0, screen_y: 16.0, width: 32.0, height: 32.0, radius_px: 4.0, color: [1.0, 1.0, 1.0, 0.5] }];

            let pixels = render_plates_to_pixels(&ctx, &mut pipeline, &instances, size).await;

            let bytes_per_row = size * 4;
            let center_pixel_offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let pixel = &pixels[center_pixel_offset..center_pixel_offset + 4];
            assert!(pixel[3] > 100, "expected the plate's centre to be translucent (alpha > 0), got {pixel:?}");
            assert!(pixel[0] > 100, "expected white content at the centre, got {pixel:?}");

            // Well outside the plate entirely: must stay fully transparent.
            let corner_pixel_offset = 0usize;
            let corner = &pixels[corner_pixel_offset..corner_pixel_offset + 4];
            assert_eq!(corner[3], 0, "expected the target's own corner (0,0), far outside the plate, to stay transparent, got {corner:?}");
        });
    }

    /// The defect this pipeline shipped with: `render` built a fresh
    /// `create_buffer_init` vertex buffer on EVERY call, and it is called
    /// once per frame from `MapView::paint` for the attribution plate. One
    /// quad is only ~144 bytes, so this was never the consumer's memory
    /// growth -- but it made `gpu_buffers_created_total` climb at frame
    /// rate, which is exactly the signal that counter exists to give, and a
    /// per-frame allocation drowning it out is worse than the allocation
    /// itself.
    ///
    /// Identity, not size: two buffers of the same size are
    /// indistinguishable by `tracked_size()`, and "the same buffer came
    /// back" is the whole claim.
    #[test]
    fn the_vertex_buffer_is_allocated_once_and_reused_across_frames() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = PlatePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let instances = [PlateInstance { screen_x: 8.0, screen_y: 8.0, width: 16.0, height: 16.0, radius_px: 2.0, color: [1.0; 4] }];

            render_plates_to_pixels(&ctx, &mut pipeline, &instances, 64).await;
            let first = pipeline.vertex_buffer.as_ref().expect("a vertex buffer after the first frame").handle();

            for frame in 1..8 {
                render_plates_to_pixels(&ctx, &mut pipeline, &instances, 64).await;
                let now = pipeline.vertex_buffer.as_ref().expect("still a vertex buffer").handle();
                assert_eq!(now, first, "frame {frame} must reuse the buffer frame 0 allocated, not allocate its own");
            }
        });
    }

    /// Grow-and-keep, the same rule the overlay's mesh buffers follow: a
    /// batch that does not fit forces one reallocation, and a smaller batch
    /// afterwards keeps the grown buffer rather than trading it for a
    /// tighter one. Exact-size reuse would reallocate on every change in
    /// plate count; a high-water mark converges and then stops allocating.
    #[test]
    fn the_vertex_buffer_grows_for_a_bigger_batch_and_never_shrinks_back() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = PlatePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let plate = |x: f32| PlateInstance { screen_x: x, screen_y: 8.0, width: 12.0, height: 12.0, radius_px: 2.0, color: [1.0; 4] };
            let one = [plate(2.0)];
            let four = [plate(2.0), plate(16.0), plate(30.0), plate(44.0)];

            render_plates_to_pixels(&ctx, &mut pipeline, &one, 64).await;
            let small = pipeline.vertex_buffer.as_ref().expect("a buffer").tracked_size();

            render_plates_to_pixels(&ctx, &mut pipeline, &four, 64).await;
            let grown = pipeline.vertex_buffer.as_ref().expect("a buffer").handle();
            let grown_size = pipeline.vertex_buffer.as_ref().expect("a buffer").tracked_size();
            assert!(grown_size > small, "four plates must not fit in a one-plate buffer ({grown_size} vs {small})");

            render_plates_to_pixels(&ctx, &mut pipeline, &one, 64).await;
            assert_eq!(
                pipeline.vertex_buffer.as_ref().expect("a buffer").handle(),
                grown,
                "a smaller batch must keep the grown buffer, not allocate a tighter one"
            );
            assert_eq!(pipeline.vertex_buffer.as_ref().expect("a buffer").tracked_size(), grown_size, "capacity must never shrink");
        });
    }

    /// The correctness half of reuse: a reused buffer still holds the
    /// PREVIOUS frame's vertices past the end of this frame's, and the draw
    /// must not reach them. `draw(0..vertices.len())` is what bounds it --
    /// this test is what would fail if that bound were ever taken from the
    /// buffer's capacity instead.
    #[test]
    fn a_shrinking_batch_does_not_draw_the_previous_frames_leftover_vertices() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = PlatePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 64u32;
            let near = PlateInstance { screen_x: 4.0, screen_y: 4.0, width: 16.0, height: 16.0, radius_px: 2.0, color: [1.0; 4] };
            let far = PlateInstance { screen_x: 40.0, screen_y: 40.0, width: 16.0, height: 16.0, radius_px: 2.0, color: [1.0; 4] };

            let alpha_at = |pixels: &[u8], x: u32, y: u32| pixels[((y * size * 4) + x * 4 + 3) as usize];

            let both = render_plates_to_pixels(&ctx, &mut pipeline, &[near, far], size).await;
            assert!(alpha_at(&both, 12, 12) > 200, "the near plate must draw");
            assert!(alpha_at(&both, 48, 48) > 200, "the far plate must draw");

            let just_near = render_plates_to_pixels(&ctx, &mut pipeline, &[near], size).await;
            assert!(alpha_at(&just_near, 12, 12) > 200, "the near plate must still draw");
            assert_eq!(
                alpha_at(&just_near, 48, 48),
                0,
                "the far plate's vertices are still in the reused buffer but must not be drawn"
            );
        });
    }

    /// The whole point of this pipeline over a plain filled rect: a corner
    /// that is inside the quad's bounding box but outside the ROUNDED shape
    /// must be transparent, while the plate's own centre (well inside any
    /// reasonable radius) stays opaque.
    #[test]
    fn a_quad_corner_outside_the_rounded_shape_stays_transparent() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = PlatePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let size = 64u32;
            // A big radius (10) relative to the plate (40x40) at (12,12),
            // so the very corner of its bounding box (12,12) sits clearly
            // outside the rounded shape, and its centre clearly inside it.
            let instances = [PlateInstance { screen_x: 12.0, screen_y: 12.0, width: 40.0, height: 40.0, radius_px: 10.0, color: [1.0, 1.0, 1.0, 1.0] }];

            let pixels = render_plates_to_pixels(&ctx, &mut pipeline, &instances, size).await;
            let bytes_per_row = size * 4;
            let at = |x: u32, y: u32| {
                let i = (y * bytes_per_row + x * 4) as usize;
                pixels[i + 3]
            };

            assert_eq!(at(12, 12), 0, "the quad's own top-left corner must fall outside the rounded shape");
            assert!(at(32, 32) > 200, "the plate's centre must be opaque");
        });
    }
}
