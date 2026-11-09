//! **The pass that draws raster tiles**: one textured, subdivided square
//! per tile, into the same render pass `TilePipeline` draws fills into,
//! immediately before them.
//!
//! # Why this is a second pipeline and not a fifth `PaintOp`
//!
//! The four existing layer kinds funnel into one `PaintPlan`, one vertex
//! format and one pipeline whose two bind groups are both
//! `ShaderStages::VERTEX`-only uniform buffers (`gpu.rs:964`, `:973`). A
//! raster tile needs a texture and a sampler, which that layout cannot
//! carry and which a fragment stage must be able to read. Widening the
//! shared `model-bgl` to reach the fragment stage would change a uniform
//! four pipelines bind.
//!
//! So raster gets its own pipeline -- and, deliberately, **not its own
//! render pass**. It draws into the pass `render_with_space` already opens,
//! as `StarPipeline` and `AtmospherePipeline` do, because a raster tile has
//! to interleave correctly with the backdrop behind it and the fills over
//! it, and a separate pass could only ever be entirely before or entirely
//! after both.
//!
//! # What it shares, and why sharing is the point
//!
//! Bind groups 0 and 1 are `TilePipeline`'s own `camera-bgl` and
//! `model-bgl`, obtained from [`TilePipeline::shared_layouts`]. A raster
//! tile binds the very same per-tile `ModelUniform` a fill tile binds --
//! same `combined_tile_matrix`, same `GlobeParams`, same `blend_t` -- so
//! `map_view` and `capture` build ONE model bind group per tile and hand it
//! to both pipelines. Two uniforms describing the same tile would be two
//! things to keep in step; one cannot drift.
//!
//! Group 2 is this pipeline's own: the tile's texture, the shared sampler,
//! and the resolved `raster-opacity`.
//!
//! # Depth
//!
//! `depth_write_enabled: false`, `depth_compare: LessEqual` -- the same
//! state `TilePipeline::pipeline` has carried since `aa20836` ("a style
//! layer drawn later covers the one drawn before it"), and for a sharper
//! version of the same reason.
//!
//! `interpolate_projection` writes a per-VERTEX clip-space z carrying the
//! far-side horizon test, not a constant across the tile. A raster tile is
//! a subdivided square; the fills drawn over it are triangulations of
//! rings covering the same pixels. Two different triangulations over one
//! pixel interpolate that z differently and round differently in f32 --
//! precisely the ULP disagreement that commit measured at 199,158 lost
//! ocean pixels. If a raster tile wrote depth, every fill above it would
//! be a per-pixel coin toss, and the symptom would be a speckle of
//! satellite imagery showing through solid roads.
//!
//! Ordering is therefore draw ORDER, as it is everywhere else in this
//! pass: raster tiles are submitted before the vector meshes.
//!
//! # Reference
//!
//! MapLibre GL JS `src/webgl/draw/draw_raster.ts` (BSD-3). Its two-pass
//! stencil (`:55-77`) is deliberately not ported -- see
//! `render::raster_mesh`'s module docs for why our neighbours cannot open
//! the gap that machinery exists to close.

use crate::render::gpu::{GpuContext, TilePipeline};
use crate::render::raster_mesh::{RasterMesh, RasterVertex};

/// One raster tile's uploaded texture.
///
/// **One texture per tile, not an atlas** -- the plan document's
/// decision 1. In short: an atlas would remove zero draw calls here, because each
/// tile needs its own model uniform for its own `tile_mercator_coords`
/// regardless; and neighbours packed adjacently in an atlas bleed into
/// each other under a linear filter at their shared edge, which is the
/// exact seam [`RasterPipeline::sampler`]'s `ClampToEdge` exists to
/// prevent.
pub struct RasterTexture {
    view: wgpu::TextureView,
    /// Held so the view stays valid. Never read directly.
    _texture: wgpu::Texture,
    /// The tile's edge length in texels, for accounting.
    size: u32,
}

impl RasterTexture {
    /// Bytes this texture occupies on the device, for the same cache
    /// budgeting `tile_source::TileCache` applies to everything else.
    /// RGBA8, one mip level.
    #[must_use]
    pub fn footprint_bytes(&self) -> usize {
        (self.size as usize) * (self.size as usize) * 4
    }
}

/// The vertex and index buffers for one granularity's square.
///
/// Shared by every tile at that zoom -- see `render::raster_mesh`. Held by
/// a caller-side cache keyed on `RasterMeshKey`, not by this pipeline,
/// because the pipeline has no idea which tiles are on screen.
pub struct RasterBuffers {
    vertex_buffer: crate::render::TrackedBuffer,
    index_buffer: crate::render::TrackedBuffer,
    index_count: u32,
}

/// The `RasterPaint` uniform in `raster_shader.wgsl`. 16 bytes: one `f32`
/// and three of padding, because WGSL rounds a uniform struct up to a
/// 16-byte stride and binding a 4-byte buffer where the shader expects 16
/// is a validation error.
///
/// The padding is explicit HERE and absent on the WGSL side, and that
/// asymmetry is deliberate. WGSL adds its own stride padding; writing a
/// `_pad: vec3<f32>` there to "match" this struct instead made the shader
/// side 32 bytes, because `vec3<f32>` aligns to 16 and so started at
/// offset 16. wgpu caught it at draw time. The rule: pad the Rust side to
/// the stride WGSL will choose, never pad the WGSL side to match Rust.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RasterPaintUniform {
    opacity: f32,
    _pad: [f32; 3],
}

pub struct RasterPipeline {
    pipeline: wgpu::RenderPipeline,
    paint_bgl: wgpu::BindGroupLayout,
    /// **One sampler for every raster tile**, `ClampToEdge` on both axes
    /// and `Linear` for magnification and minification.
    ///
    /// `ClampToEdge` is the seam decision (plan document, decision 3), and
    /// it is what MapLibre binds too (`draw_raster.ts:121`, `:129`). Under
    /// a linear filter a fetch at u just under 0 or just over 1 -- which
    /// WILL happen at the tile boundary -- would with `Repeat` read the
    /// OPPOSITE edge of the tile and draw a one-pixel wrong-coloured line
    /// down every border. Clamping returns the edge texel instead, so two
    /// adjacent tiles each return their own edge texel at their shared
    /// boundary, and those are adjacent samples of the same imagery.
    ///
    /// The rejected alternative was insetting the texture coordinates to
    /// `[0.5/N, 1 - 0.5/N]`. It sounds safer and is worse: it scales the
    /// imagery by `N/(N-1)`, so every tile is drawn slightly too large and
    /// stops lining up with its neighbour's content. That trades a colour
    /// error confined to the boundary for a registration error across the
    /// whole tile.
    ///
    /// No mipmaps and no anisotropy, unlike the reference
    /// (`draw_raster.ts:121`, `:136-139`). Both are worth having on a
    /// pitched map and neither is on the path to a correct first frame;
    /// noted as follow-up rather than silently skipped.
    sampler: wgpu::Sampler,
}

impl RasterPipeline {
    /// Builds the pipeline against an existing [`TilePipeline`], from
    /// which it takes the camera and model bind group layouts and the
    /// depth format.
    ///
    /// Taking the `TilePipeline` rather than the three values separately
    /// is deliberate: it makes it impossible to construct a raster
    /// pipeline that draws into a pass some *other* tile pipeline opened,
    /// which would bind model uniforms written through a different layout.
    #[must_use]
    pub fn new(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        tiles: &TilePipeline,
    ) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps raster shader"),
            source: wgpu::ShaderSource::Wgsl(crate::projection::specialise_wgsl(include_str!("raster_shader.wgsl")).into_owned().into()),
        });

        let paint_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("raster-paint-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let (camera_bgl, model_bgl) = tiles.shared_layouts();
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("raster-pipeline-layout"),
            bind_group_layouts: &[Some(camera_bgl), Some(model_bgl), Some(&paint_bgl)],
            immediate_size: 0,
        });

        // ORDER IS THE CONTRACT, as it is for `TilePipeline::ATTRS`: this
        // list mirrors `RasterVertex`'s FIELD order. Location 3 is the
        // struct's `_pad`, declared here so the accumulated offsets stay
        // right and the stride assertion below is checking something.
        const ATTRS: [wgpu::VertexAttribute; 4] =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32, 3 => Float32];
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<RasterVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTRS,
        };

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("raster-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(vertex_layout)],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: tiles.depth_format(),
                // **No depth write.** See this module's docs, and
                // `aa20836`. A raster tile and the fills over it are two
                // different triangulations of the same pixels, which is
                // exactly the case whose per-vertex z disagrees in f32.
                depth_write_enabled: Some(false),
                // `LessEqual` for the same reason `TilePipeline` uses it:
                // same-depth fragments fall back to draw order rather than
                // being rejected outright.
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    // Straight-alpha source, as `raster_shader.wgsl`'s
                    // `fs_main` returns and as `TilePipeline` uses.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raster-sampler"),
            // Spelled out rather than left to `Default`, although
            // `AddressMode::default()` IS `ClampToEdge`. This is the seam
            // decision; a reader must not have to know a default to see it.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self { pipeline, paint_bgl, sampler }
    }

    /// Uploads one decoded tile as its own texture.
    ///
    /// `Rgba8Unorm`, not `Rgba8UnormSrgb`, matching
    /// `IconPipeline::upload_atlas` and for the same reason spelled out
    /// there: this engine's render target is not sRGB and nothing
    /// re-encodes, so an `*Srgb` texture would decode to linear on read
    /// with nothing to undo it and every tile would come out about 2.2
    /// gamma too dark.
    #[must_use]
    pub fn upload_tile(&self, ctx: &GpuContext, image: &image::RgbaImage) -> RasterTexture {
        let (width, height) = image.dimensions();
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("raster-tile-texture"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            image.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        RasterTexture { view, _texture: texture, size: width.max(height) }
    }

    /// The group-2 bind group for one tile: its texture, the shared
    /// sampler, and this frame's resolved `raster-opacity`.
    ///
    /// The returned buffer must outlive the pass -- the bind group holds
    /// it only by handle, the same contract `create_model_bind_group`
    /// carries.
    #[must_use]
    pub fn create_paint_bind_group(
        &self,
        ctx: &GpuContext,
        texture: &RasterTexture,
        opacity: f32,
    ) -> (crate::render::TrackedBuffer, wgpu::BindGroup) {
        let uniform = RasterPaintUniform { opacity, _pad: [0.0; 3] };
        // `TrackedBuffer`, not `device.create_buffer`: every buffer this
        // crate creates is counted, and
        // `every_buffer_creation_in_the_crate_goes_through_the_tracker`
        // greps `src/` to keep that true. It caught these three.
        let buffer = crate::render::TrackedBuffer::new(
            &ctx.device,
            &wgpu::BufferDescriptor {
                label: Some("raster-paint-uniform"),
                size: std::mem::size_of::<RasterPaintUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        );
        ctx.queue.write_buffer(&buffer, 0, bytemuck::bytes_of(&uniform));
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster-paint-bind-group"),
            layout: &self.paint_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry { binding: 2, resource: buffer.as_entire_binding() },
            ],
        });
        (buffer, bind_group)
    }

    /// Uploads one granularity's square. Call once per distinct
    /// [`RasterMeshKey`](crate::render::raster_mesh::RasterMeshKey) and
    /// keep the result -- every tile at that zoom draws from it.
    #[must_use]
    pub fn create_mesh_buffers(&self, ctx: &GpuContext, mesh: &RasterMesh) -> RasterBuffers {
        let vertex_buffer = crate::render::TrackedBuffer::init(
            &ctx.device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("raster-vertex-buffer"),
                contents: bytemuck::cast_slice(&mesh.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            },
        );
        let index_buffer = crate::render::TrackedBuffer::init(
            &ctx.device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("raster-index-buffer"),
                contents: bytemuck::cast_slice(&mesh.indices),
                usage: wgpu::BufferUsages::INDEX,
            },
        );
        RasterBuffers {
            vertex_buffer,
            index_buffer,
            index_count: mesh.indices.len() as u32,
        }
    }

    /// Draws every entry into an ALREADY-OPEN pass.
    ///
    /// Takes an open pass rather than opening its own -- the shape
    /// `IconPipeline::draw` and the space pipelines already use -- because
    /// raster tiles must land between the backdrop and the vector meshes
    /// in ONE pass. A pass of their own could only be wholly before or
    /// wholly after both.
    ///
    /// Each entry is `(square, model bind group, paint bind group)`. The
    /// square is shared across tiles of one zoom; the model bind group is
    /// the tile's own, the same one `TilePipeline` binds for it.
    ///
    /// Returns the number of draw calls issued, for
    /// `alloc::note_draw_calls` -- the caller adds it to the pass's total
    /// rather than this function touching the global, so the count stays
    /// one number per pass.
    pub fn draw<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        draws: &[(&'a RasterBuffers, &'a wgpu::BindGroup, &'a wgpu::BindGroup)],
    ) -> u64 {
        if draws.is_empty() {
            return 0;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, camera_bind_group, &[]);
        let mut calls = 0;
        for (buffers, model_bind_group, paint_bind_group) in draws {
            if buffers.index_count == 0 {
                continue;
            }
            pass.set_bind_group(1, *model_bind_group, &[]);
            pass.set_bind_group(2, *paint_bind_group, &[]);
            pass.set_vertex_buffer(0, buffers.vertex_buffer.slice(..));
            pass.set_index_buffer(buffers.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..buffers.index_count, 0, 0..1);
            calls += 1;
        }
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vertex attribute list in `new` accumulates its offsets from the
    /// declared formats, so it is only correct if it mirrors
    /// `RasterVertex`'s fields exactly. A mismatch does not fail to
    /// compile -- it silently reads the wrong bytes, which on a texture
    /// coordinate reads as imagery that is subtly in the wrong place.
    #[test]
    fn the_vertex_stride_matches_the_attributes_declared_for_it() {
        // 2 + 2 + 1 + 1 f32s.
        assert_eq!(std::mem::size_of::<RasterVertex>(), 6 * 4);
        // And the attribute list's own accumulated span must be the same,
        // or the stride skips or overlaps.
        const ATTRS: [wgpu::VertexAttribute; 4] =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32, 3 => Float32];
        let last = ATTRS.last().expect("four attributes");
        assert_eq!(last.offset + 4, std::mem::size_of::<RasterVertex>() as u64);
    }

    /// WGSL rounds a uniform struct to a 16-byte stride; writing four
    /// bytes into the binding the shader declares as 16 is a validation
    /// error at bind time, not a wrong picture.
    #[test]
    fn the_paint_uniform_is_a_whole_number_of_sixteen_byte_blocks() {
        assert_eq!(std::mem::size_of::<RasterPaintUniform>() % 16, 0);
        assert_eq!(std::mem::size_of::<RasterPaintUniform>(), 16);
    }

    /// The shader declares three bind groups and this module builds a
    /// layout for exactly one of them, borrowing the other two. A shader
    /// that grew a fourth group would bind nothing to it and fail at
    /// pipeline creation on some backends and silently on others.
    #[test]
    fn the_shader_declares_the_three_bind_groups_this_pipeline_provides() {
        let source = include_str!("raster_shader.wgsl");
        for group in ["@group(0)", "@group(1)", "@group(2)"] {
            assert!(source.contains(group), "the shader must declare {group}");
        }
        assert!(
            !source.contains("@group(3)"),
            "the shader declares a bind group this pipeline's layout does not provide"
        );
    }

    /// The port is only safe while it IS a port. These three functions are
    /// transcribed from `shader.wgsl`; if that file's versions change and
    /// these do not, a raster tile and the fill over it stop agreeing
    /// about where the sphere is.
    ///
    /// Compares the function BODIES textually, which is crude and is
    /// exactly the point: it fails on any edit to either copy, including
    /// one that is mathematically equivalent, and the fix is to look at
    /// both and re-sync them deliberately.
    #[test]
    fn the_projection_functions_are_character_identical_to_the_fill_shaders() {
        fn body(source: &str, signature: &str) -> String {
            let start = source
                .find(signature)
                .unwrap_or_else(|| panic!("{signature} must be present"));
            let rest = &source[start..];
            let open = rest.find('{').expect("a function body");
            let mut depth = 0usize;
            for (i, c) in rest[open..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            // Comments differ between the two files by
                            // design -- each explains itself in its own
                            // context. Only the code is compared.
                            return rest[open..=open + i]
                                .lines()
                                .map(str::trim)
                                .filter(|l| !l.is_empty() && !l.starts_with("//"))
                                .collect::<Vec<_>>()
                                .join("\n");
                        }
                    }
                    _ => {}
                }
            }
            panic!("unbalanced braces after {signature}")
        }

        let fill = include_str!("shader.wgsl");
        let raster = include_str!("raster_shader.wgsl");
        for signature in [
            "fn project_to_sphere(",
            "fn globe_clipping_z(",
            "fn interpolate_projection(",
        ] {
            assert_eq!(
                body(fill, signature),
                body(raster, signature),
                "{signature} has drifted between shader.wgsl and raster_shader.wgsl. \
                 A raster tile and the fill over it would stop agreeing about where the \
                 sphere is -- re-sync them deliberately rather than relaxing this test."
            );
        }
    }

    /// The raster shader compiles to GLSL ES 3.00 (WebGL2), with its
    /// stand-in clip varying and `discard`, on both stages.
    #[test]
    fn the_raster_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("raster_shader.wgsl"),
            &[("vs_main", naga::ShaderStage::Vertex), ("fs_main", naga::ShaderStage::Fragment)],
        );
    }

}
