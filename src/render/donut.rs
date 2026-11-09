//! Donut clusters: an instanced SDF pipeline that draws a pie/donut chart
//! at each cluster anchor (spec `2026-09-27-us-race-dot-map-design.md`
//! §5.2).
//!
//! One screen-space quad per donut, anchored at a CPU-projected point the
//! way `icon_pipeline` anchors a sprite, shaded in `donut_shader.wgsl` as a
//! ring cut into up to eight sectors, clockwise from 12 o'clock. An atlas
//! of pre-rendered donuts was rejected in the spec: the set of share
//! combinations is unbounded and every new cluster would cost an upload.
//!
//! The sector rule is spelled twice, once per side of the GPU boundary:
//! [`sector_of`] here and `sector` in the shader. The tests hold them to
//! the same `atan2(uv.x, -uv.y)` and the same "first `k` with `t < cum_k`".

use crate::render::alloc::TrackedBuffer;
use crate::render::gpu::GpuContext;

/// The hole's radius as a share of the outer radius: `1 - 0.35`, Walker's
/// ring width (spec §5.2).
pub const DONUT_HOLE: f32 = 1.0 - 0.35;

/// The most donuts one layer holds; `MapView::set_donuts` keeps the first
/// `MAX_DONUTS` and drops the rest. The instance buffer's worst case is
/// therefore `MAX_DONUTS x 64 B = 512 KiB` per layer, times the number of
/// layers, plus one 144 B uniform per layer. Walker's cluster view never
/// has anywhere near this many in view: at 12-64 px across, 8,192 donuts
/// would tile a 2560x1600 screen several times over.
pub const MAX_DONUTS: usize = 8192;

/// Below this a donut is invisible: the vertex stage collapses its quad
/// (`step(1e-4, input.alpha)` in `donut_shader.wgsl`, held to this by a
/// test), and the hit test skips it. One threshold for both.
pub const MIN_VISIBLE_ALPHA: f32 = 1e-4;

/// The largest f32 below 1: where the turn fraction is clamped, on both
/// sides, since `fract` of a tiny negative angle rounds to exactly 1.0.
pub const T_MAX: f32 = 0.999_999_94;

/// One donut on the GPU: 64 B, read per instance.
///
/// `anchor_px` and `radius_px` are PHYSICAL pixels, y-down (the icon
/// pipeline's space). `cum` is [`cumulative_shares`]. `alpha` 0 hides the
/// donut outright: the vertex stage collapses its quad, so a donut on the
/// far side of the globe costs no fragments.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct DonutInstance {
    pub anchor_px: [f32; 2],
    pub radius_px: f32,
    pub hole: f32,
    pub cum: [f32; 8],
    pub alpha: f32,
    pub _pad: [f32; 3],
}

const _: () = assert!(std::mem::size_of::<DonutInstance>() == 64);

/// A donut to draw, in map terms: where, how big (LOGICAL pixels, as a
/// style's `circle-radius` is), how many of each group, and how opaque.
#[derive(Debug, Clone, PartialEq)]
pub struct Donut {
    pub lon: f64,
    pub lat: f64,
    pub radius_px: f32,
    pub counts: [u32; 8],
    pub alpha: f32,
}

/// `cum[k]` is the share of groups `0..=k`, so sector `k` spans
/// `[cum[k-1], cum[k])` of the turn. From the last non-empty group on it
/// is exactly `1.0`, so no `t < 1` can fall past the end through rounding.
/// An empty donut is all `1.0`, but `MapView::set_donuts` never draws one
/// (see [`visible_alpha`]).
#[must_use]
pub fn cumulative_shares(counts: &[u32; 8]) -> [f32; 8] {
    let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return [1.0; 8];
    }
    let last = counts.iter().rposition(|&c| c > 0).unwrap_or(7);
    let mut cum = [1.0f32; 8];
    let mut run = 0u64;
    for k in 0..last {
        run += u64::from(counts[k]);
        cum[k] = (run as f64 / total as f64) as f32;
    }
    cum
}

/// Which sector `uv` falls in: the CPU mirror of `donut_shader.wgsl`'s
/// `sector`. `uv` is the quad's own space (the rim at `length(uv) == 1`),
/// y-down, so 12 o'clock is `(0, -1)` and 3 o'clock `(1, 0)`.
#[must_use]
pub fn sector_of(cum: &[f32; 8], uv: [f32; 2]) -> u8 {
    let turn = uv[0].atan2(-uv[1]) / std::f32::consts::TAU;
    let t = (turn - turn.floor()).min(T_MAX);
    let n = cum.iter().filter(|&&c| c <= t).count();
    n.min(7) as u8
}

/// `donut`'s alpha, sanitised once on the CPU: NaN and infinities are 0,
/// the rest clamped to `[0, 1]`, anything under [`MIN_VISIBLE_ALPHA`] is 0,
/// and so is a donut whose counts are all zero (it has no shares to draw).
/// The draw and the hit test both read this value.
#[must_use]
pub fn visible_alpha(donut: &Donut) -> f32 {
    let a = if donut.alpha.is_finite() { donut.alpha.clamp(0.0, 1.0) } else { 0.0 };
    if a < MIN_VISIBLE_ALPHA || donut.counts.iter().all(|&c| c == 0) {
        0.0
    } else {
        a
    }
}

/// `donut` as drawn this frame: `anchor` is the logical-pixel point the
/// icon path's projection gave (`None` for a point the planet or the near
/// plane hides, which draws at alpha 0), `scale` the device pixel ratio.
#[must_use]
pub fn donut_instance(donut: &Donut, anchor: Option<(f32, f32)>, scale: f32) -> DonutInstance {
    let (anchor_px, alpha) = match anchor {
        Some((x, y)) => ([x * scale, y * scale], visible_alpha(donut)),
        None => ([0.0, 0.0], 0.0),
    };
    DonutInstance {
        anchor_px,
        radius_px: sane_radius(donut.radius_px) * scale,
        hole: DONUT_HOLE,
        cum: cumulative_shares(&donut.counts),
        alpha,
        _pad: [0.0; 3],
    }
}

/// A radius the draw and the hit test agree on: NaN and negatives are 0.
#[must_use]
pub fn sane_radius(radius_px: f32) -> f32 {
    if radius_px.is_finite() { radius_px.max(0.0) } else { 0.0 }
}

/// A palette as the shader's uniform wants it.
#[must_use]
pub fn palette_uniform(palette: &[crate::style::Rgba; 8]) -> [[f32; 4]; 8] {
    palette.map(|c| [c.r, c.g, c.b, c.a])
}

/// `donut_shader.wgsl`'s `Frame`: the target size, then the eight colours.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FrameUniform {
    size: [f32; 2],
    _pad: [f32; 2],
    colors: [[f32; 4]; 8],
}

const INSTANCE_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x2, // anchor_px
    1 => Float32,   // radius_px
    2 => Float32,   // hole
    3 => Float32x4, // cum[0..4]
    4 => Float32x4, // cum[4..8]
    5 => Float32,   // alpha
];

/// One batch: a palette and the range of the frame's instances it colours.
pub type DonutBatch = ([[f32; 4]; 8], std::ops::Range<u32>);

/// The donut pass. Built once; instance and palette buffers are grown and
/// kept, like `IconPipeline`'s vertex buffer. The instance buffer is
/// bounded by what `MapView` hands it: at most [`MAX_DONUTS`] per layer,
/// 512 KiB per layer (see there), and in practice only the on-screen ones.
pub struct DonutPipeline {
    pipeline: wgpu::RenderPipeline,
    frame_bgl: wgpu::BindGroupLayout,
    /// One uniform (and its bind group) per batch drawn in a frame, since
    /// each batch has its own palette and `queue.write_buffer` lands before
    /// any draw: one shared buffer would give every batch the last palette.
    frames: Vec<(TrackedBuffer, wgpu::BindGroup)>,
    instances: Option<TrackedBuffer>,
}

impl DonutPipeline {
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps donut shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("donut_shader.wgsl").into()),
        });
        let frame_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("donut-frame-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("donut-pipeline-layout"),
            bind_group_layouts: &[Some(&frame_bgl)],
            immediate_size: 0,
        });
        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("donut-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<DonutInstance>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &INSTANCE_ATTRS,
                })],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            // No depth: it draws in the symbol pass, which has none.
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    // The icon pipeline's blend, so a donut composites over
                    // the map exactly as a sprite does.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, frame_bgl, frames: Vec::new(), instances: None }
    }

    /// Draws every batch into an already-open `pass`, in order: each batch
    /// is one palette and its range of `instances`, one instanced draw of 4
    /// vertices. `instances` is uploaded as it is, with no copy.
    /// ONE call per frame, for the reason `IconPipeline::render_batches`
    /// gives: every batch's instances share one kept buffer.
    pub fn render_batches(
        &mut self,
        ctx: &GpuContext,
        pass: &mut wgpu::RenderPass,
        instances: &[DonutInstance],
        batches: &[DonutBatch],
        viewport_width_px: f32,
        viewport_height_px: f32,
    ) {
        if instances.is_empty() || batches.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(instances);
        match self.instances.as_ref() {
            Some(buffer) if buffer.tracked_size() >= bytes.len() as u64 => ctx.queue.write_buffer(buffer, 0, bytes),
            _ => {
                self.instances = Some(TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
                    label: Some("donut-instances"),
                    contents: bytes,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                }));
            }
        }
        while self.frames.len() < batches.len() {
            let buffer = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                label: Some("donut-frame-uniform"),
                size: std::mem::size_of::<FrameUniform>() as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("donut-frame-bind-group"),
                layout: &self.frame_bgl,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
            });
            self.frames.push((buffer, bind_group));
        }
        for ((palette, _), (buffer, _)) in batches.iter().zip(&self.frames) {
            let frame = FrameUniform { size: [viewport_width_px, viewport_height_px], _pad: [0.0; 2], colors: *palette };
            ctx.queue.write_buffer(buffer, 0, bytemuck::bytes_of(&frame));
        }
        let buffer = self.instances.as_ref().expect("just written or just allocated");
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, buffer.slice(..));
        for ((_, range), (_, bind_group)) in batches.iter().zip(&self.frames) {
            let range = range.clone();
            if range.is_empty() {
                continue;
            }
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..4, range);
            crate::render::alloc::note_draw_calls(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sectors_run_clockwise_from_twelve() {
        let cum = cumulative_shares(&[1, 1, 1, 1, 0, 0, 0, 0]); // four quarters
        assert_eq!(sector_of(&cum, [0.01, -0.9]), 0); // just right of 12 o'clock
        assert_eq!(sector_of(&cum, [0.9, 0.01]), 1); // 3 o'clock, y down
        assert_eq!(sector_of(&cum, [-0.01, 0.9]), 2);
        assert_eq!(sector_of(&cum, [-0.9, -0.01]), 3);
    }

    #[test]
    fn the_donut_shader_uses_at_most_four_locations() {
        let src = include_str!("donut_shader.wgsl");
        let out = src.split_once("struct VertexOutput {").unwrap().1.split_once('}').unwrap().0;
        assert!(out.matches("@location(").count() <= 4);
    }

    #[test]
    fn the_wgsl_sector_rule_matches_the_rust_one() {
        let src = include_str!("donut_shader.wgsl");
        assert!(src.contains("atan2(uv.x, -uv.y)"));
        // "first k with t < cum_k" is "count of cum_k <= t" on both sides.
        assert!(src.contains("step(cum_a, tt)") && src.contains("step(cum_b, tt)"));
        assert!(src.contains("min(n, 7.0)"));
        // The same clamp below 1, and the same visibility threshold.
        assert!(src.contains(&format!("vec4<f32>(min(t, {T_MAX:?}))")), "T_MAX in the WGSL");
        assert_eq!(T_MAX, 1.0 - f32::EPSILON / 2.0);
        assert!(src.contains(&format!("step({MIN_VISIBLE_ALPHA:e}, input.alpha)")), "MIN_VISIBLE_ALPHA in the WGSL");
    }

    /// Review finding 2: `fract` of a tiny negative turn rounds to 1.0,
    /// which every share passes; clamped, it is the last group.
    #[test]
    fn just_left_of_twelve_is_the_last_group_not_group_seven() {
        let quarters = cumulative_shares(&[1, 1, 1, 1, 0, 0, 0, 0]);
        assert_eq!(sector_of(&quarters, [-1e-8, -0.9]), 3);
        assert_eq!(sector_of(&quarters, [-1e-7, -0.9]), 3);
        assert_eq!(sector_of(&cumulative_shares(&[0, 4, 0, 0, 0, 0, 0, 0]), [-1e-8, -0.9]), 1);
    }

    /// Review findings 3 and 7: NaN, infinite, negative and sub-threshold
    /// alphas are 0; so is a donut with no people in it.
    #[test]
    fn alpha_is_sanitised_once_and_empty_donuts_are_invisible() {
        let d = |alpha: f32, counts: [u32; 8]| Donut { lon: 0.0, lat: 0.0, radius_px: 10.0, counts, alpha };
        let some = [1, 0, 0, 0, 0, 0, 0, 0];
        for (a, want) in [(f32::NAN, 0.0), (f32::INFINITY, 0.0), (-1.0, 0.0), (5e-5, 0.0), (0.5, 0.5), (2.0, 1.0)] {
            assert_eq!(visible_alpha(&d(a, some)), want, "alpha {a}");
            assert_eq!(donut_instance(&d(a, some), Some((1.0, 1.0)), 1.0).alpha, want);
        }
        assert_eq!(visible_alpha(&d(1.0, [0; 8])), 0.0);
        assert_eq!(sane_radius(f32::NAN), 0.0);
        assert_eq!(sane_radius(-3.0), 0.0);
    }

    #[test]
    fn cumulative_shares_end_at_exactly_one_and_skip_empty_tails() {
        let cum = cumulative_shares(&[3, 0, 1, 0, 0, 0, 0, 0]);
        assert_eq!(cum, [0.75, 0.75, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        assert_eq!(cumulative_shares(&[0; 8]), [1.0; 8]);
        let odd = cumulative_shares(&[1, 1, 1, 1, 1, 1, 1, 0]);
        assert_eq!(odd[6], 1.0);
        // An empty group has no sector: just past 12 o'clock is group 2.
        assert_eq!(sector_of(&cumulative_shares(&[0, 0, 5, 5, 0, 0, 0, 0]), [0.01, -0.9]), 2);
        // Just before 12 o'clock is the last non-empty group.
        assert_eq!(sector_of(&cumulative_shares(&[0, 0, 5, 5, 0, 0, 0, 0]), [-0.01, -0.9]), 3);
    }

    #[test]
    fn a_hidden_anchor_draws_at_alpha_zero() {
        let d = Donut { lon: 0.0, lat: 0.0, radius_px: 10.0, counts: [1; 8], alpha: 0.8 };
        assert_eq!(donut_instance(&d, None, 2.0).alpha, 0.0);
        let shown = donut_instance(&d, Some((5.0, 6.0)), 2.0);
        assert_eq!((shown.anchor_px, shown.radius_px, shown.alpha, shown.hole), ([10.0, 12.0], 20.0, 0.8, DONUT_HOLE));
    }

    #[test]
    fn the_donut_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("donut_shader.wgsl"),
            &[("vs_main", naga::ShaderStage::Vertex), ("fs_main", naga::ShaderStage::Fragment)],
        );
    }

    fn palette() -> [[f32; 4]; 8] {
        [
            [0.90, 0.10, 0.10, 1.0],
            [0.10, 0.70, 0.20, 1.0],
            [0.15, 0.25, 0.95, 1.0],
            [0.95, 0.80, 0.10, 1.0],
            [0.5, 0.5, 0.5, 1.0],
            [0.5, 0.5, 0.5, 1.0],
            [0.5, 0.5, 0.5, 1.0],
            [0.5, 0.5, 0.5, 1.0],
        ]
    }

    /// Renders `batches` into a black `w`x`h` Rgba8Unorm target and reads it
    /// back tightly packed. The icon pipeline's headless readback, reused.
    /// `(palette, donuts)` pairs as one instance slice and its batches.
    fn flatten(batches: &[([[f32; 4]; 8], &[DonutInstance])]) -> (Vec<DonutInstance>, Vec<DonutBatch>) {
        let mut all = Vec::new();
        let mut out = Vec::new();
        for (palette, donuts) in batches {
            let start = all.len() as u32;
            all.extend_from_slice(donuts);
            out.push((*palette, start..all.len() as u32));
        }
        (all, out)
    }

    fn render(ctx: &GpuContext, pipeline: &mut DonutPipeline, batches: &[([[f32; 4]; 8], &[DonutInstance])], w: u32, h: u32) -> Vec<u8> {
        let (instances, batches) = flatten(batches);
        let target = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("donut-test-color"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("donut-test-pass"),
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
            pipeline.render_batches(ctx, &mut pass, &instances, &batches, w as f32, h as f32);
        }
        let row = (w * 4).next_multiple_of(256);
        let out = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("donut-readback"),
            size: u64::from(row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &out,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        let slice = out.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();
        let data = slice.get_mapped_range().expect("mapped");
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            let start = (y * row) as usize;
            pixels.extend_from_slice(&data[start..start + (w * 4) as usize]);
        }
        pixels
    }

    /// **Task 13's capture criterion.** One 64 px donut of four equal
    /// groups; a pixel at 45 degrees into each quarter, midway across the
    /// ring, is its group's colour within 3 per channel. Also: the centre
    /// is white, and outside the rim is the black clear.
    #[test]
    #[ignore = "headless donut capture (Task 13); run on demand"]
    fn donut_capture_colours_each_quarter() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = DonutPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let size = 128u32;
            let d = Donut { lon: 0.0, lat: 0.0, radius_px: 32.0, counts: [1, 1, 1, 1, 0, 0, 0, 0], alpha: 1.0 };
            let inst = donut_instance(&d, Some((64.0, 64.0)), 1.0);
            let pal = palette();
            let px = render(&ctx, &mut pipeline, &[(pal, &[inst])], size, size);
            let at = |x: f32, y: f32| {
                let o = ((y.floor() as u32 * size + x.floor() as u32) * 4) as usize;
                [px[o], px[o + 1], px[o + 2], px[o + 3]]
            };
            let mid = 32.0 * (1.0 + DONUT_HOLE) / 2.0;
            for (k, deg) in [45.0f32, 135.0, 225.0, 315.0].into_iter().enumerate() {
                let a = deg.to_radians();
                let got = at(64.0 + mid * a.sin(), 64.0 - mid * a.cos());
                let want = pal[k].map(|c| (c * 255.0).round() as i32);
                eprintln!("quarter {k} at {deg} deg: {got:?}, want {want:?}");
                for c in 0..3 {
                    assert!((i32::from(got[c]) - want[c]).abs() <= 3, "quarter {k}: {got:?} against {want:?}");
                }
            }
            let centre = at(64.0, 64.0);
            assert!(centre[..3].iter().all(|&c| c >= 252), "a white centre, got {centre:?}");
            let outside = at(64.0 + 40.0, 64.0);
            assert!(outside[..3].iter().all(|&c| c <= 3), "the clear outside the rim, got {outside:?}");
            // A hidden donut draws nothing.
            let hidden = donut_instance(&d, None, 1.0);
            let blank = render(&ctx, &mut pipeline, &[(pal, &[hidden])], size, size);
            assert!(blank.as_chunks::<4>().0.iter().all(|p| p[..3] == [0, 0, 0]), "alpha 0 draws no pixel");
        });
    }

    /// Review finding 5: two batches, two palettes, one call. Each donut's
    /// quarter-0 pixel is its OWN palette's group-0 colour, which a single
    /// shared uniform (the bug `IconPipeline` once had) would break.
    #[test]
    #[ignore = "headless donut capture (Task 13); run on demand"]
    fn two_batches_keep_their_own_palettes() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = DonutPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let d = Donut { lon: 0.0, lat: 0.0, radius_px: 32.0, counts: [1, 1, 1, 1, 0, 0, 0, 0], alpha: 1.0 };
            let left = [donut_instance(&d, Some((40.0, 40.0)), 1.0)];
            let right = [donut_instance(&d, Some((120.0, 40.0)), 1.0)];
            let a = palette();
            let mut b = palette();
            b[0] = [0.20, 0.90, 0.90, 1.0];
            let (w, h) = (160u32, 80u32);
            let px = render(&ctx, &mut pipeline, &[(a, &left), (b, &right)], w, h);
            let mid = 32.0 * (1.0 + DONUT_HOLE) / 2.0;
            let q0 = std::f32::consts::FRAC_PI_4;
            let at = |cx: f32| {
                let (x, y) = ((cx + mid * q0.sin()) as u32, (40.0 - mid * q0.cos()) as u32);
                let o = ((y * w + x) * 4) as usize;
                [px[o], px[o + 1], px[o + 2]]
            };
            for (cx, pal, name) in [(40.0, a, "first"), (120.0, b, "second")] {
                let got = at(cx);
                let want = pal[0].map(|c| (c * 255.0).round() as i32);
                eprintln!("{name} batch quarter 0: {got:?}, want {want:?}");
                for c in 0..3 {
                    assert!((i32::from(got[c]) - want[c]).abs() <= 3, "the {name} batch drew {got:?}, its palette says {want:?}");
                }
            }
        });
    }

    /// **MG-R13's draw half: the donut pass at 2,000 donuts is <= 1 ms.**
    ///
    /// 2,000 donuts at 2x over a 2560x1600 target (a 1280x800-point
    /// window), sized from Walker's step table (spec 1.2: 12-64 px across,
    /// each of the 11 steps equally likely -- far more big ones than a real
    /// frame has) and scattered at random, so their quads cover the screen about 2.9
    /// times over. Also the same with 8-40 px RADII (5.4 screens), as a
    /// stress case, printed but not asserted.
    ///
    /// Measured on the GPU's wall clock, amortised: `FRAMES` passes (each a
    /// clear plus the donut draw) in one submission, submit-to-idle over
    /// `FRAMES`, minus the same with no donuts -- so the per-submit and
    /// clear costs cancel and what is left is the draw. Median of 15. GPU
    /// timestamps are not used: on this Metal adapter they read an empty
    /// pass as slower than a full one.
    #[test]
    #[ignore = "timing (MG-R13 draw half); run on demand with --nocapture"]
    fn donut_pass_at_2000_donuts_is_under_a_millisecond() {
        const FRAMES: usize = 20;
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })
                .await
                .expect("an adapter");
            let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor::default()).await.expect("a device");
            let max_buffer_size = device.limits().max_buffer_size;
            let ctx = GpuContext { device, queue, max_buffer_size };
            let (w, h) = (2560u32, 1600u32);
            let mut pipeline = DonutPipeline::new(&ctx, wgpu::TextureFormat::Bgra8Unorm);
            let mut seed = 12345u32;
            let mut rnd = move || {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32
            };
            const WALKER_SIZES: [f32; 11] = [12.0, 15.0, 18.0, 22.0, 27.0, 32.0, 37.0, 43.0, 50.0, 58.0, 64.0];
            let mut donuts = |radius: &mut dyn FnMut(f32) -> f32| -> (Vec<Donut>, Vec<(f32, f32)>) {
                (0..2000)
                    .map(|_| {
                        let r = radius(rnd());
                        let counts = std::array::from_fn(|_| (rnd() * 1000.0) as u32);
                        let d = Donut { lon: 0.0, lat: 0.0, radius_px: r, counts, alpha: 1.0 };
                        (d, (rnd() * 1280.0, rnd() * 800.0))
                    })
                    .unzip()
            };
            let (walker, walker_at) = donuts(&mut |u| WALKER_SIZES[((u * 11.0) as usize).min(10)] / 2.0);
            let (stress, stress_at) = donuts(&mut |u| 8.0 + 32.0 * u);
            let t0 = crate::timing::Instant::now();
            let walker: Vec<DonutInstance> = walker.iter().zip(&walker_at).map(|(d, &a)| donut_instance(d, Some(a), 2.0)).collect();
            let build = t0.elapsed();
            let stress: Vec<DonutInstance> = stress.iter().zip(&stress_at).map(|(d, &a)| donut_instance(d, Some(a), 2.0)).collect();
            let pal = palette();
            let target = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("donut-timing-color"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());
            // One submission of FRAMES passes; ms per pass.
            let mut run = |donuts: &[DonutInstance]| -> f64 {
                let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                for _ in 0..FRAMES {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("donut-timing-pass"),
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
                    let (instances, batches) = flatten(&[(pal, donuts)]);
                    pipeline.render_batches(&ctx, &mut pass, &instances, &batches, w as f32, h as f32);
                }
                let wall = crate::timing::Instant::now();
                ctx.queue.submit(Some(encoder.finish()));
                ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                wall.elapsed().as_secs_f64() * 1e3 / FRAMES as f64
            };
            let median = |mut v: Vec<f64>| {
                v.sort_by(f64::total_cmp);
                v[v.len() / 2]
            };
            let mut measure = |donuts: &[DonutInstance]| -> (f64, f64) {
                for _ in 0..3 {
                    run(donuts);
                    run(&[]);
                }
                let (mut full, mut empty) = (Vec::new(), Vec::new());
                for _ in 0..15 {
                    full.push(run(donuts));
                    empty.push(run(&[]));
                }
                (median(full), median(empty))
            };
            let area = |d: &[DonutInstance]| d.iter().map(|i| (2.0 * (i.radius_px + 1.0)).powi(2)).sum::<f32>() / (w * h) as f32;
            let (full, empty) = measure(&walker);
            let (sfull, sempty) = measure(&stress);
            eprintln!(
                "donut pass, 2,000 donuts at {w}x{h}, {FRAMES} passes per submit, median of 15:\n  \
                 Walker sizes ({:.1} screens of quads): {full:.3} ms/pass, empty pass {empty:.3} ms, draw {:.3} ms\n  \
                 stress 8-40 px radii ({:.1} screens): {sfull:.3} ms/pass, empty {sempty:.3} ms, draw {:.3} ms\n  \
                 CPU instance build (2,000, this profile): {:.3} ms; adapter {:?}",
                area(&walker),
                full - empty,
                area(&stress),
                sfull - sempty,
                build.as_secs_f64() * 1e3,
                adapter.get_info().name,
            );
            assert!(full - empty <= 1.0, "the donut pass took {:.3} ms at 2,000 donuts", full - empty);
        });
    }
}
