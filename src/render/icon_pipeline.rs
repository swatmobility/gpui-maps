use std::collections::HashMap;


use crate::render::gpu::GpuContext;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IconRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Carried through from the source sprite sheet's manifest entry
    /// (`sprite::SpriteRect::pixel_ratio`): how many of this rect's atlas
    /// pixels make up one logical point. The packer copies the icon's
    /// pixels 1:1, so a `@2x` sheet's 32x32 entry stays 32x32 here and
    /// only `map_view::icon_logical_size` knows to divide the ratio back
    /// out -- without this field the draw loop would stretch those 32
    /// texels over a 32-logical-point (64-physical-pixel on retina) quad.
    pub pixel_ratio: f32,
    /// Carried through from `sprite::SpriteRect::sdf`: this icon's alpha
    /// channel is a signed distance field, not coverage, so it is drawn by
    /// `icon_shader.wgsl`'s SDF branch and takes its colour from the
    /// layer's `icon-color`/`icon-halo-color` rather than from its own
    /// pixels.
    ///
    /// Packing an SDF sprite beside a plain one in the same atlas is safe:
    /// the two never share a quad, and `IconInstance::sdf` is what selects
    /// the branch per quad. What is NOT safe is letting the packer place
    /// them flush against each other -- see `pack_icon_atlas`'s padding.
    pub sdf: bool,
}

/// Packs every icon in `sprites` into one shared RGBA buffer using a simple
/// shelf packer (icons placed left-to-right in rows, a new row started when
/// the current one is full) -- simple and sufficient at Mapbox real
/// sprite-sheet scale (a few hundred small icons), not aiming for optimal
/// packing density. `names` not present in `sprites` are silently skipped
/// (a data-driven `icon-image` expression can reference a name the fetched
/// sprite sheet doesn't happen to contain).
pub(crate) fn pack_icon_atlas(
    sprites: &crate::sprite::SpriteAtlas,
    names: &[String],
) -> (image::RgbaImage, HashMap<String, IconRect>) {
    const ATLAS_WIDTH: u32 = 1024;
    // One transparent texel between neighbours, MapLibre's own atlas
    // padding. The sampler is bilinear, so a quad's edge texel is blended
    // with whatever sits next to it -- flush packing lets one sprite's
    // pixels bleed into another's outermost row.
    //
    // Invisible for a plain RGBA icon (the neighbour contributes at most a
    // sliver of half-transparent colour on the very edge) and NOT invisible
    // for an SDF one, which is why this arrived with SDF support: an SDF
    // fragment thresholds alpha at ~0.75 to decide inside-from-outside, so
    // a neighbour bleeding high alpha into the edge does not soften the
    // border, it draws a stray piece of shape there -- in the halo's own
    // colour, since the halo pass thresholds lower still.
    const PAD: u32 = 1;
    let mut rects = HashMap::new();
    let mut cursor_x = 0u32;
    let mut cursor_y = 0u32;
    let mut row_height = 0u32;
    // First pass: compute placement without allocating the final image yet
    // (need the total height first).
    let mut placements: Vec<(String, u32, u32, u32, u32)> = Vec::new();
    for name in names {
        let Some(icon) = sprites.get(name) else { continue };
        let (w, h) = icon.dimensions();
        if cursor_x + w > ATLAS_WIDTH {
            cursor_x = 0;
            cursor_y += row_height + PAD;
            row_height = 0;
        }
        placements.push((name.clone(), cursor_x, cursor_y, w, h));
        cursor_x += w + PAD;
        row_height = row_height.max(h);
    }
    let atlas_height = cursor_y + row_height;
    let mut atlas = image::RgbaImage::new(ATLAS_WIDTH, atlas_height.max(1));
    for (name, x, y, w, h) in placements {
        let icon = sprites.get(&name).expect("name came from sprites.get above");
        for iy in 0..h {
            for ix in 0..w {
                atlas.put_pixel(x + ix, y + iy, *icon.get_pixel(ix, iy));
            }
        }
        // `unwrap_or(1.0)` can't actually be hit (`slice_atlas` fills both
        // maps from the same manifest entries), and 1.0 is the manifest's
        // own default for a missing `pixelRatio` anyway.
        let pixel_ratio = sprites.pixel_ratio(&name).unwrap_or(1.0);
        // Same `unwrap_or` reasoning as `pixel_ratio` above: both maps are
        // filled from the same manifest entries, and `false` is the
        // manifest's own default for a missing `sdf`.
        let sdf = sprites.is_sdf(&name).unwrap_or(false);
        rects.insert(name, IconRect { x, y, width: w, height: h, pixel_ratio, sdf });
    }
    (atlas, rects)
}

/// One quad to draw this frame: a screen-space rect (PHYSICAL pixels,
/// top-left origin, matching the rest of the symbol-drawing code's
/// physical-pixel convention -- see `map_view.rs`'s `scale_factor` note next
/// to the text loop) sampling a sub-rect of the shared icon atlas texture
/// (UV coordinates, 0..1).
#[derive(Debug, Clone, Copy)]
pub(crate) struct IconInstance {
    pub screen_x: f32,
    pub screen_y: f32,
    pub width: f32,
    pub height: f32,
    pub uv_min: (f32, f32),
    pub uv_max: (f32, f32),
    /// Clockwise rotation about the quad's centre, in radians, in y-down
    /// physical screen space. `0.0` for a point symbol. Applied in
    /// `icon_shader.wgsl`'s `vs_main`, never on the CPU.
    pub rotation_rad: f32,
    /// How this quad is coloured. `None` -- the overwhelmingly common case
    /// -- samples the atlas and draws its own pixels, exactly as every icon
    /// did before SDF support. `Some` means the sprite is a distance field
    /// with no colour of its own, and these are the colours to give it.
    pub sdf: Option<SdfPaint>,
    /// Mapbox's `paint.icon-opacity` for the symbol this quad belongs to, a
    /// plain multiplier on the quad's alpha. `1.0` is "draw as-is", which
    /// is what every caller that has no opinion passes.
    ///
    /// A `0.0` icon is never given to this pipeline at all -- the symbol
    /// loop stops building the quad, so an invisible sprite costs no
    /// vertices -- but the value is honoured here anyway rather than
    /// assumed away, because a fractional one is real (Mapbox Streets'
    /// `building-entrance` sits at 0.4).
    pub opacity: f32,
}

/// The colours an SDF sprite is drawn with, since it carries none itself.
///
/// Mapbox's `icon-color`, `icon-halo-color` and `icon-halo-width`, resolved
/// for one symbol. One SDF pin image plus one of these per route is what
/// replaces "one baked PNG per colour" -- the reason SDF sprites exist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SdfPaint {
    /// `icon-color`: the shape's fill, straight (NOT premultiplied) sRGB.
    pub fill: [f32; 4],
    /// `icon-halo-color`: the border drawn outside the shape's edge.
    pub halo: [f32; 4],
    /// `icon-halo-width` in PHYSICAL pixels -- the style's logical points
    /// already multiplied by the window's scale factor, so it is in the
    /// same units as `font_scale`'s numerator and the two divide cleanly.
    pub halo_width: f32,
    /// The quad's rendered height in PHYSICAL pixels divided by the
    /// sprite's own height in atlas texels -- MapLibre's `fontScale`.
    ///
    /// A distance field encodes distance in ITS OWN texels, so drawing it
    /// larger stretches the gradient: a fixed threshold would give a border
    /// that grows with zoom, and a fixed smoothstep width would give an
    /// edge that softens with zoom. Dividing by this converts both back
    /// into field units and fixes both.
    ///
    /// The device pixel ratio is already IN this number, because the height
    /// it is built from is physical. That is why neither this struct nor
    /// the shader takes a separate `device_pixel_ratio`: on a 2x display
    /// the same icon has twice the physical height, so `font_scale`
    /// doubles, and the edge correctly resolves twice as finely. Passing
    /// the ratio as well would divide it out twice.
    pub font_scale: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct IconVertex {
    /// The quad's CENTRE in y-down physical screen pixels -- the point the
    /// shader rotates `offset_px` about. Replicated across the quad's six
    /// vertices, matching MapLibre's `a_pos_offset.xy`.
    anchor_px: [f32; 2],
    /// This corner's offset from `anchor_px`, in physical pixels, BEFORE
    /// rotation -- MapLibre's `a_offset`. Static per corner: the CPU never
    /// computes a rotated position, which is what keeps a future move to
    /// static/instanced buffers local (see the spec's Deferred section).
    offset_px: [f32; 2],
    /// Clockwise rotation about `anchor_px`, in radians, in the same y-down
    /// space. Replicated across the quad's six vertices; this pipeline is
    /// not instanced.
    angle: f32,
    uv: [f32; 2],
    /// The colour to paint this quad's shape, straight (non-premultiplied)
    /// sRGB. Ignored entirely unless `sdf_params.z` says this is an SDF
    /// quad, since a plain icon's colour is its own pixels.
    color: [f32; 4],
    /// `(buffer, gamma, is_sdf, opacity)` -- everything the fragment stage
    /// needs beyond the atlas sample, resolved on the CPU so the shader
    /// needs no uniforms of its own and no knowledge of the device pixel
    /// ratio. The first three turn a distance into coverage; see
    /// `sdf_thresholds`.
    ///
    /// `opacity` is Mapbox's `icon-opacity`, and it rides HERE rather than
    /// in `color` because `color` is meaningless for a plain RGBA sprite --
    /// which is exactly the quad `icon-opacity` most often has to fade.
    /// Applied to alpha only, so a half-opaque sprite blends over the map
    /// rather than darkening towards it.
    ///
    /// `is_sdf` is `0.0` or `1.0` rather than a `u32` so the whole thing
    /// travels as one `Float32x4` attribute; a mixed-type vertex would need
    /// a second binding for no benefit.
    sdf_params: [f32; 4],
}

const ICON_VERTEX_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32,
    3 => Float32x2,
    4 => Float32x4,
    5 => Float32x4,
];

/// MapLibre's `symbol_sdf.fragment.glsl`, `#define SDF_PX 8.0`: how many of
/// a distance field's own pixels one unit of its encoded distance spans.
/// The sprite generator and the shader have to agree on this number; 8 is
/// what every Mapbox/MapLibre SDF sprite is generated against.
pub(crate) const SDF_PX: f32 = 8.0;

/// The alpha a distance field carries exactly ON the shape's edge, as a
/// fraction: `(256 - 64) / 256`. The generator centres the edge 64 levels
/// below full, so anything above this is inside the shape.
pub(crate) const SDF_EDGE: f32 = (256.0 - 64.0) / 256.0;

/// MapLibre's `EDGE_GAMMA`, the base softness of the edge before the icon's
/// own drawn scale is divided out.
///
/// MapLibre's own line (`symbol_sdf.fragment.glsl:24,33`) is
/// `EDGE_GAMMA = 0.105 / u_device_pixel_ratio` and then
/// `gamma = EDGE_GAMMA / (fontScale * u_gamma_scale)`. It is tempting to
/// transcribe that literally -- divide this constant by the window's scale
/// factor too -- and WRONG to do so here, because the two engines give
/// `fontScale` different units:
///
/// - MapLibre's `fontScale` is `size / 24`, where `size` is the STYLE's
///   `text-size`/`icon-size` in logical units, resolved on the CPU once per
///   zoom stop and reused across a continuously-changing device pixel
///   ratio. It never sees the display's density, so MapLibre has to fold
///   that in separately, via `u_device_pixel_ratio`.
/// - This renderer's `font_scale` (`SdfPaint::font_scale`, built by
///   `symbol_pass::icon_font_scale`/`render::glyph_font_scale`) is defined
///   as PHYSICAL pixels per atlas texel, and is built from a PHYSICAL size
///   (`logical_size * scale_factor`) every single frame. The window's scale
///   factor is already multiplied in before `sdf_thresholds` ever sees it
///   (see `SdfPaint::font_scale`'s doc comment).
///
/// Concretely, for a fixed logical size, `gamma = EDGE_GAMMA / font_scale`
/// already halves when the scale factor doubles and thirds when it triples,
/// because `font_scale` itself does -- exactly what the literal MapLibre
/// line achieves by dividing `EDGE_GAMMA` by `u_device_pixel_ratio`
/// explicitly. The physical-pixel half-width of the smoothstep band this
/// produces is `gamma * SDF_PX * font_scale == EDGE_GAMMA * SDF_PX`, a
/// constant (0.84 physical px) at ANY scale factor. Adding a second
/// division by the scale factor here -- on top of the one already inside
/// `font_scale` -- would divide it out TWICE, shrinking that band to
/// `0.84 / scale_factor` physical pixels (0.28 px at a 3x/retina scale
/// factor): a smoothstep narrower than a single physical pixel, which reads
/// as aliased, uneven-weight glyph strokes -- the opposite of the fix such
/// a change would be trying to make. See `SdfPaint::font_scale` and
/// `symbol_pass::icon_font_scale` for where the scale factor already enters.
const EDGE_GAMMA: f32 = 0.105;

/// Where the halo's threshold starts before `icon-halo-width` pushes it
/// outward, in the distance field's own units scaled by `SDF_PX`.
/// MapLibre's `(6.0 - halo_width / fontScale) / SDF_PX`; `6` rather than
/// `SDF_EDGE * SDF_PX` (which is 6.0 too) is written out here the way
/// MapLibre writes it, so the two can be compared line for line.
pub(crate) const SDF_HALO_BASE: f32 = 6.0;

/// The `(buffer, gamma)` pair that turns this quad's sampled distance into
/// coverage -- MapLibre's `symbol_sdf.fragment.glsl`, resolved on the CPU.
///
/// `buffer` is the distance threshold the edge sits at. The fill pass uses
/// the shape's own edge; the halo pass moves the threshold OUTWARD, to
/// `(6 - halo_width / font_scale) / SDF_PX`, so a wider halo reaches
/// further out into the field. Dividing by `font_scale` is what makes
/// `icon-halo-width` a width in POINTS rather than in the sprite's own
/// texels: the same field drawn twice as large has twice as many pixels per
/// unit of distance.
///
/// `gamma` is the half-width of the smoothstep either side of that
/// threshold -- the antialiased edge. It shrinks as the icon is drawn
/// larger (`font_scale`) and as the display gets denser (`dpr`), because
/// both mean more physical pixels across the same gradient.
fn sdf_thresholds(paint: &SdfPaint, is_halo: bool) -> (f32, f32) {
    // Guarded because it divides: a zero-height quad would otherwise
    // produce NaN coverage, which reads as an icon that silently vanished
    // rather than as a loud failure.
    let font_scale = if paint.font_scale > 0.0 { paint.font_scale } else { 1.0 };
    let gamma = EDGE_GAMMA / font_scale;
    if is_halo {
        // Clamped at 0: a halo wider than the base would put the threshold
        // below zero, where every texel of the quad -- including the empty
        // corners -- counts as inside and the pin becomes a filled
        // rectangle. The encoding leaves 6 texels of usable field outside
        // the shape (`sprite::sdf_from_mask`), so a halo asking for more
        // than that saturates rather than inverting.
        //
        // The clamp alone is NOT enough, and once was assumed to be: at a
        // threshold of 0 the smoothstep is centred ON the field's floor,
        // so every empty texel still draws at half coverage -- and any
        // threshold within `gamma` of 0 draws them partly. That is the
        // translucent halo-coloured rectangle a 1.9 pt halo on a 3x pin
        // produced. The fragment shader floors the smoothstep's lower edge
        // at 0 to close it; see `icon_shader.wgsl`.
        let buffer = ((SDF_HALO_BASE - paint.halo_width / font_scale) / SDF_PX).max(0.0);
        (buffer, gamma)
    } else {
        (SDF_EDGE, gamma)
    }
}

/// The viewport size the shader needs to convert physical pixels to clip
/// space -- the job `pixel_to_clip` used to do on the CPU. `_pad` brings the
/// struct to the 16-byte minimum a WGSL uniform requires.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    size: [f32; 2],
    _pad: [f32; 2],
}

/// Emits the six vertices of one quad, coloured by `color`/`sdf_params`.
fn push_one_quad(
    vertices: &mut Vec<IconVertex>,
    instance: &IconInstance,
    color: [f32; 4],
    sdf_params: [f32; 4],
) {
    let half_w = instance.width / 2.0;
    let half_h = instance.height / 2.0;
    let anchor_px = [instance.screen_x + half_w, instance.screen_y + half_h];
    let (u0, v0) = instance.uv_min;
    let (u1, v1) = instance.uv_max;
    let angle = instance.rotation_rad;

    let corner = |dx: f32, dy: f32, u: f32, v: f32| IconVertex {
        anchor_px,
        offset_px: [dx, dy],
        angle,
        uv: [u, v],
        color,
        sdf_params,
    };
    let top_left = corner(-half_w, -half_h, u0, v0);
    let top_right = corner(half_w, -half_h, u1, v0);
    let bottom_left = corner(-half_w, half_h, u0, v1);
    let bottom_right = corner(half_w, half_h, u1, v1);

    vertices.extend_from_slice(&[top_left, top_right, bottom_left, top_right, bottom_right, bottom_left]);
}

/// Emits everything one instance draws: ONE quad for a plain icon, and TWO
/// -- halo first, then fill -- for an SDF one.
///
/// The halo and the fill are separate quads over the same geometry because
/// they threshold the same distance field at different distances, and a
/// single fragment cannot blend two thresholds of itself correctly: the
/// halo has to be laid down and blended against first, then the fill drawn
/// over it. MapLibre draws them as two whole PASSES for exactly this
/// reason.
///
/// Keeping them ADJACENT rather than batching all halos then all fills is
/// the one difference, and it is the whole point. Within a single draw call
/// primitive order is preserved for blending, so a symbol's halo and fill
/// land as a unit before the next symbol's halo begins -- which means a
/// symbol drawn later covers the earlier one's border cleanly instead of
/// blending into it. Batching by pass is what makes overlapping borders
/// blend in mapbox-gl (mapbox/mapbox-gl-js#11504, closed without a fix);
/// the caller orders instances back-to-front and gets the correct result
/// for free.
fn push_quad(vertices: &mut Vec<IconVertex>, instance: &IconInstance) {
    let Some(paint) = instance.sdf else {
        // A plain RGBA icon: the colour and thresholds are unused, and the
        // `0.0` in `is_sdf` is what tells the fragment stage so. The
        // opacity still rides along -- it is the one part of the paint a
        // plain sprite does obey.
        push_one_quad(vertices, instance, [0.0; 4], [0.0, 0.0, 0.0, instance.opacity]);
        return;
    };
    // A zero-width halo has nothing to draw; emitting it anyway would cost
    // a quad per symbol whose only contribution is a fully transparent
    // fragment. Mapbox gates its own halo pass on the same condition.
    if paint.halo_width > 0.0 && paint.halo[3] > 0.0 {
        let (buffer, gamma) = sdf_thresholds(&paint, true);
        push_one_quad(vertices, instance, paint.halo, [buffer, gamma, 1.0, instance.opacity]);
    }
    let (buffer, gamma) = sdf_thresholds(&paint, false);
    push_one_quad(vertices, instance, paint.fill, [buffer, gamma, 1.0, instance.opacity]);
}

/// A small, dedicated textured-quad pipeline for symbol icons -- a sibling
/// to `TilePipeline`, not an extension of it, following this project's
/// existing single-responsibility-per-pipeline convention. Every sliced
/// sprite icon is packed into ONE shared atlas texture at upload time
/// (`pack_icon_atlas` + `upload_atlas`), so the per-frame draw loop needs
/// zero per-icon texture/bind-group switching: every instance just carries
/// a different UV rect into the same shared texture/bind group.
pub(crate) struct IconPipeline {
    pipeline: wgpu::RenderPipeline,
    atlas_bgl: wgpu::BindGroupLayout,
    viewport_buffer: crate::render::alloc::TrackedBuffer,
    viewport_bind_group: wgpu::BindGroup,
    /// The icon quads, allocated once and then GROWN-AND-KEPT: `render`
    /// rewrites it in place every frame and only reallocates when this
    /// frame's icons do not fit, never to shrink it back. `None` until the
    /// first `render` call. Mirrors `PlatePipeline::vertex_buffer` (see its
    /// doc comment for the full rationale) -- `render` used to
    /// `create_buffer_init` a fresh buffer on EVERY call, once per frame,
    /// which made `gpu_buffers_created_total` climb at frame rate whenever
    /// any symbol was on screen.
    ///
    /// Unlike the plate's single fixed-size quad, icon instance counts
    /// genuinely vary a lot frame to frame as symbols come and go -- so a
    /// SHRINKING frame is the case that matters here: the reused buffer
    /// still holds the previous, larger frame's vertices past the end of
    /// this frame's, and `render`'s `draw(0..vertices.len() as u32, ..)`
    /// (bounded by this frame's vertex count, never by the buffer's
    /// capacity) is what keeps those leftover vertices off screen. See
    /// `a_shrinking_batch_of_icons_does_not_draw_the_previous_frames_leftover_instances`.
    vertex_buffer: Option<crate::render::alloc::TrackedBuffer>,
}

impl IconPipeline {
    pub(crate) fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps icon shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("icon_shader.wgsl").into()),
        });

        let atlas_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("icon-atlas-bgl"),
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
            ],
        });

        let viewport_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("icon-viewport-bgl"),
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
            label: Some("icon-viewport-uniform"),
            size: std::mem::size_of::<ViewportUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let viewport_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("icon-viewport-bind-group"),
            layout: &viewport_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: viewport_buffer.as_entire_binding() }],
        });

        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("icon-pipeline-layout"),
            bind_group_layouts: &[Some(&atlas_bgl), Some(&viewport_bgl)],
            immediate_size: 0,
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<IconVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ICON_VERTEX_ATTRS,
        };

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("icon-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(vertex_layout)],
            },
            primitive: wgpu::PrimitiveState::default(),
            // No depth attachment: this pipeline draws into the SAME second
            // render pass Task 3/4's glyphon text draws into
            // (`map_view.rs`'s `symbol-overlay-pass`), which itself has no
            // depth attachment -- see that pass's own doc comment for why
            // (symbol overlap is resolved before drawing, by the collision
            // pass in `map_view.rs`, so nothing reaching this pipeline
            // overlaps anything else in it; draw order settles only the
            // icon-under-its-own-label layering).
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    // Icons have real alpha (transparent sprite-sheet
                    // padding, antialiased edges) and draw over the map/text
                    // already in the target -- same reasoning as
                    // `TilePipeline`'s own blend state.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self { pipeline, atlas_bgl, viewport_buffer, viewport_bind_group, vertex_buffer: None }
    }

    /// Uploads a packed `image::RgbaImage` (from `pack_icon_atlas`) as a
    /// `wgpu::Texture` and returns a bind group pairing its view with a
    /// linear-filtering sampler. `Rgba8Unorm`, not `Rgba8Unorm`, and it
    /// has to match the render target: sprite PNGs are authored in sRGB, the
    /// target is a UNORM format that stores sRGB-encoded values (see
    /// `map_view.rs`'s surface-format selection), so the sprite's pixels must
    /// travel from the atlas to the framebuffer without a transfer function
    /// applied anywhere. An `*Srgb` atlas would make `textureSample` decode
    /// to linear on read with nothing to re-encode it -- every sprite ~2.2
    /// gamma too dark.
    ///
    /// The two formats are one decision, not two. If the render target ever
    /// goes back to `*Srgb`, this goes back to `Rgba8Unorm` in the same
    /// commit and `icon_shader.wgsl` linearises its vertex colours again.
    pub(crate) fn upload_atlas(&self, ctx: &GpuContext, atlas: &image::RgbaImage) -> wgpu::BindGroup {
        let (width, height) = atlas.dimensions();
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("icon-atlas-texture"),
            size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            atlas.as_raw(),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * width), rows_per_image: Some(height) },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("icon-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("icon-atlas-bind-group"),
            layout: &self.atlas_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        })
    }

    /// A story image (`story::media`) with its whole mip chain (`levels`,
    /// largest first, each half the last) and a trilinear sampler, so a
    /// photo drawn several times smaller than its texture averages instead
    /// of aliasing. Same format and layout as [`Self::upload_atlas`]; the
    /// sprite atlas keeps its single level (its goldens are unchanged).
    pub(crate) fn upload_mipped(&self, ctx: &GpuContext, levels: &[image::RgbaImage]) -> Option<wgpu::BindGroup> {
        let first = levels.first()?;
        let (width, height) = first.dimensions();
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("story-image-texture"),
            size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
            mip_level_count: u32::try_from(levels.len()).ok()?,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, img) in levels.iter().enumerate() {
            let (w, h) = img.dimensions();
            ctx.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: u32::try_from(level).ok()?,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                img.as_raw(),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * w), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("story-image-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        Some(ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("story-image-bind-group"),
            layout: &self.atlas_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        }))
    }

    /// Draws every `instances` entry into an ALREADY-OPEN `pass` -- mirrors
    /// `TilePipeline::render`'s shape of taking an open pass rather than
    /// opening its own, so `map_view.rs` can draw icons into the same
    /// second pass Task 3 already opened for text, not a third pass.
    ///
    /// Takes `&mut self` because it writes this frame's quads into the
    /// pipeline's own kept `vertex_buffer` (see that field), growing it only
    /// when a frame's icons do not fit. Nothing is allocated on a steady
    /// frame, and a frame with FEWER icons than the buffer's capacity draws
    /// only its own `vertices.len()` -- the buffer's leftover tail from a
    /// larger previous frame is never read.
    ///
    /// # Why batches, rather than one call per atlas
    ///
    /// Because ONE call is the only safe shape. Every batch's vertices go
    /// into the pipeline's single kept `vertex_buffer`, and
    /// `queue.write_buffer` is applied before ANY command in the submitted
    /// encoder runs -- so a second call for a second atlas would clobber
    /// the first's vertices before either draw executed, and the first
    /// batch would draw the second's quads through the first's texture.
    /// Appending every batch into one buffer and issuing one `draw` per
    /// batch over its own vertex range is what keeps them apart. Pinned by
    /// `two_batches_each_draw_their_own_atlas_rather_than_clobbering_one_buffer`.
    ///
    /// That is what line-placed TEXT uses: one rotated quad per glyph out
    /// of `render::GlyphAtlas`, a second batch rather than a second
    /// pipeline, because the only thing that differs from a sprite quad is
    /// which texture is bound -- the vertex layout, the shader and the SDF
    /// thresholds are the ones sprites already use.
    ///
    /// Batches draw in the order given, and within a batch in instance
    /// order -- the blending property `push_quad`'s doc comment depends on.
    pub(crate) fn render_batches(
        &mut self,
        ctx: &GpuContext,
        pass: &mut wgpu::RenderPass,
        batches: &[(&wgpu::BindGroup, &[IconInstance])],
        viewport_width_px: f32,
        viewport_height_px: f32,
    ) {
        let total: usize = batches.iter().map(|(_, instances)| instances.len()).sum();
        if total == 0 {
            return;
        }
        // An SDF instance contributes two quads, so this is a floor rather
        // than an exact size -- the plain-icon case, which is still most of
        // them, allocates exactly right.
        let mut vertices = Vec::with_capacity(total * 6);
        // `(first_vertex, vertex_count)` per batch, recorded as the
        // vertices are appended: an SDF instance emits two quads and a
        // plain one emits one, so a batch's range cannot be computed from
        // its instance count.
        let mut ranges: Vec<std::ops::Range<u32>> = Vec::with_capacity(batches.len());
        for (_, instances) in batches {
            let start = vertices.len() as u32;
            for instance in *instances {
                push_quad(&mut vertices, instance);
            }
            ranges.push(start..vertices.len() as u32);
        }
        ctx.queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::cast_slice(&[ViewportUniform {
                size: [viewport_width_px, viewport_height_px],
                _pad: [0.0, 0.0],
            }]),
        );
        // Grow-and-keep, same shape as `PlatePipeline::render`. `vertices`
        // is never empty here (the early return above), so there is no
        // zero-length `write_buffer` to guard against.
        let bytes: &[u8] = bytemuck::cast_slice(&vertices);
        match self.vertex_buffer.as_ref() {
            Some(buffer) if buffer.tracked_size() >= bytes.len() as u64 => {
                ctx.queue.write_buffer(buffer, 0, bytes);
            }
            _ => {
                self.vertex_buffer = Some(crate::render::alloc::TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor {
                    label: Some("icon-vertices"),
                    contents: bytes,
                    // `COPY_DST` so the branch above can rewrite it in
                    // place; without it every frame lands here.
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                }));
            }
        }
        let vertex_buffer = self.vertex_buffer.as_ref().expect("just written or just allocated");
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(1, &self.viewport_bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        for ((atlas_bind_group, _), range) in batches.iter().zip(ranges) {
            if range.is_empty() {
                continue;
            }
            pass.set_bind_group(0, *atlas_bind_group, &[]);
            // Bounded by THIS batch's own vertex range, not the buffer's
            // capacity -- the property that keeps a shrinking frame from
            // drawing the previous, larger frame's leftover instances still
            // sitting in the buffer's tail.
            pass.draw(range, 0..1);
            crate::render::alloc::note_draw_calls(1);
        }
    }
}

#[cfg(test)]
#[path = "icon_overlay_readback_tests.rs"]
mod overlay_readback_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_quad_emits_anchor_centre_and_corner_offsets() {
        let instance = IconInstance {
            screen_x: 100.0,
            screen_y: 200.0,
            width: 10.0,
            height: 20.0,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            rotation_rad: 0.0,
            sdf: None,
            opacity: 1.0,
        };
        let mut vertices = Vec::new();
        push_quad(&mut vertices, &instance);

        assert_eq!(vertices.len(), 6, "two triangles");
        // Every vertex shares the quad's CENTRE as its anchor -- the shader
        // rotates about this point, so it must be the centre, not the top-left.
        for v in &vertices {
            assert_eq!(v.anchor_px, [105.0, 210.0]);
            assert_eq!(v.angle, 0.0);
        }
        // The four distinct corner offsets, half-width/half-height from centre.
        let mut offsets: Vec<[f32; 2]> = vertices.iter().map(|v| v.offset_px).collect();
        offsets.sort_by(|a, b| a.partial_cmp(b).expect("no NaNs"));
        offsets.dedup();
        assert_eq!(offsets, vec![[-5.0, -10.0], [-5.0, 10.0], [5.0, -10.0], [5.0, 10.0]]);
        // No trigonometry on this side: that is the whole point of the change.
        assert!(
            vertices.iter().all(|v| v.offset_px[0].abs() == 5.0 && v.offset_px[1].abs() == 10.0),
            "corner offsets must be un-rotated; the shader rotates them"
        );
    }

    #[test]
    fn packs_named_icons_without_overlap() {
        let mut manifest = HashMap::new();
        manifest.insert(
            "red".to_string(),
            crate::sprite::SpriteRect { x: 0, y: 0, width: 4, height: 4, pixel_ratio: 1.0, sdf: false, ..Default::default() },
        );
        manifest.insert(
            "blue".to_string(),
            crate::sprite::SpriteRect { x: 4, y: 0, width: 6, height: 6, pixel_ratio: 1.0, sdf: false, ..Default::default() },
        );
        let mut source = image::RgbaImage::new(10, 6);
        for y in 0..4 { for x in 0..4 { source.put_pixel(x, y, image::Rgba([255, 0, 0, 255])); } }
        for y in 0..6 { for x in 4..10 { source.put_pixel(x, y, image::Rgba([0, 0, 255, 255])); } }
        let sprites = crate::sprite::slice_atlas(
            &{ let mut buf = Vec::new(); source.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).unwrap(); buf },
            &manifest,
        ).unwrap();

        let (packed, rects) = pack_icon_atlas(&sprites, &["red".to_string(), "blue".to_string()]);
        let red_rect = rects.get("red").unwrap();
        let blue_rect = rects.get("blue").unwrap();
        // No overlap: one rect's x-range must not intersect the other's (a
        // shelf packer places them side by side on the first row, or on
        // separate rows -- either way, check via non-overlap on at least
        // one axis rather than asserting exact packer layout).
        let x_ranges_disjoint = red_rect.x + red_rect.width <= blue_rect.x || blue_rect.x + blue_rect.width <= red_rect.x;
        let y_ranges_disjoint = red_rect.y + red_rect.height <= blue_rect.y || blue_rect.y + blue_rect.height <= red_rect.y;
        assert!(x_ranges_disjoint || y_ranges_disjoint, "red {red_rect:?} and blue {blue_rect:?} overlap");

        // Packed pixels match source colors at each rect's origin.
        assert_eq!(*packed.get_pixel(red_rect.x, red_rect.y), image::Rgba([255, 0, 0, 255]));
        assert_eq!(*packed.get_pixel(blue_rect.x, blue_rect.y), image::Rgba([0, 0, 255, 255]));
    }

    #[test]
    fn packed_rects_carry_each_sprites_pixel_ratio_through_to_the_draw_loop() {
        // The packer copies pixels 1:1, so a `@2x` sheet's 4x4 entry stays
        // 4x4 in the packed atlas and nothing about the rect alone says
        // those are device pixels rather than logical points. `map_view`'s
        // `icon_logical_size` needs the ratio to divide it back out, so it
        // has to survive slicing AND packing.
        let mut manifest = HashMap::new();
        manifest.insert(
            "retina".to_string(),
            crate::sprite::SpriteRect { x: 0, y: 0, width: 4, height: 4, pixel_ratio: 2.0, sdf: false, ..Default::default() },
        );
        let mut source = image::RgbaImage::new(4, 4);
        for y in 0..4 { for x in 0..4 { source.put_pixel(x, y, image::Rgba([255, 0, 0, 255])); } }
        let sprites = crate::sprite::slice_atlas(
            &{ let mut buf = Vec::new(); source.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).unwrap(); buf },
            &manifest,
        ).unwrap();

        let (_packed, rects) = pack_icon_atlas(&sprites, &["retina".to_string()]);
        let rect = rects.get("retina").unwrap();
        assert_eq!(rect.pixel_ratio, 2.0);
        assert_eq!((rect.width, rect.height), (4, 4), "the packer must not rescale pixels");
    }

    /// Extracted readback plumbing shared by every headless-wgpu test in
    /// this module: opens an offscreen `size`x`size` render target, draws
    /// `instances` into it via `pipeline.render`, copies the result to a
    /// mapped buffer, and returns tightly-packed (no row padding) RGBA8
    /// bytes -- `pixels[((y * size + x) * 4) as usize]` is the red channel
    /// of pixel (x, y).
    pub(super) async fn render_icons_to_pixels(
        ctx: &GpuContext,
        pipeline: &mut IconPipeline,
        atlas_bind_group: &wgpu::BindGroup,
        instances: &[IconInstance],
        size: u32,
    ) -> Vec<u8> {
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
                label: Some("icon-test-pass"),
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
            pipeline.render_batches(ctx, &mut pass, &[(atlas_bind_group, instances)], size as f32, size as f32);
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

    /// Two batches, two atlases, ONE call -- and each batch samples its
    /// OWN texture.
    ///
    /// This is the property the whole batched shape exists for, and it
    /// cannot be checked by counting draws. `render_batches` writes every
    /// batch's vertices into one kept buffer, and `queue.write_buffer`
    /// lands before any draw in the encoder, so the version of this that
    /// called a single-batch `render` twice would have the second call's
    /// vertices overwrite the first's: both draws would then read the
    /// SECOND batch's quads, the first through the first atlas's bind
    /// group. Both quads still draw, both are still the right size, and the
    /// left one is the wrong colour in the wrong place. Hence pixels.
    ///
    /// Line-placed text is exactly this case in production: glyph quads
    /// sampling `render::GlyphAtlas` beside sprite quads sampling the icon
    /// atlas.
    #[test]
    fn two_batches_each_draw_their_own_atlas_rather_than_clobbering_one_buffer() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let red = pipeline.upload_atlas(&ctx, &image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255])));
            let blue = pipeline.upload_atlas(&ctx, &image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 255, 255])));

            let size = 64u32;
            let quad = |screen_x: f32| IconInstance {
                screen_x,
                screen_y: 24.0,
                width: 16.0,
                height: 16.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                rotation_rad: 0.0,
                sdf: None,
                opacity: 1.0,
            };
            // Left quad out of the RED atlas, right quad out of the BLUE
            // one -- so which texture each batch read is legible from the
            // pixels alone.
            let left = [quad(8.0)];
            let right = [quad(40.0)];

            let color_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("two-batch-test-color"),
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
                    label: Some("two-batch-test-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &color_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pipeline.render_batches(
                    &ctx,
                    &mut pass,
                    &[(&red, &left), (&blue, &right)],
                    size as f32,
                    size as f32,
                );
            }
            let bytes_per_row = (size * 4).next_multiple_of(256);
            let output_buffer = crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                label: Some("two-batch-readback"),
                size: (bytes_per_row * size) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &color_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &output_buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(bytes_per_row),
                        rows_per_image: Some(size),
                    },
                },
                wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            );
            ctx.queue.submit(Some(encoder.finish()));
            let slice = output_buffer.slice(..);
            let (tx, rx) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |r| {
                tx.send(r).unwrap();
            });
            ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            rx.recv().unwrap().unwrap();
            let data = slice.get_mapped_range().expect("mapped");

            let at = |x: u32, y: u32| {
                let o = (y * bytes_per_row + x * 4) as usize;
                [data[o], data[o + 1], data[o + 2], data[o + 3]]
            };
            let left_pixel = at(16, 32);
            let right_pixel = at(48, 32);
            assert!(
                left_pixel[0] > 200 && left_pixel[2] < 50,
                "the first batch must sample the RED atlas, got {left_pixel:?}"
            );
            assert!(
                right_pixel[2] > 200 && right_pixel[0] < 50,
                "the second batch must sample the BLUE atlas, got {right_pixel:?}"
            );
        });
    }

    /// Headless-wgpu proof that `IconPipeline` actually draws a textured
    /// quad, mirroring `src/render/gpu.rs`'s `renders_a_solid_triangle_to_an_offscreen_texture`
    /// pattern: upload a small atlas with a known-color icon, draw it at a
    /// known screen position/size, read the offscreen target back, and
    /// confirm the icon's color landed where expected.
    #[test]
    fn draws_an_uploaded_icon_at_the_expected_screen_position() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            // A tiny 4x4-icon atlas: solid green in the icon's rect, solid
            // (very different) blue everywhere else in the atlas, so a
            // wrong UV rect or an unclipped full-atlas sample would be
            // caught by the assertion below.
            let mut atlas_image = image::RgbaImage::new(8, 8);
            for y in 0..8 {
                for x in 0..8 {
                    atlas_image.put_pixel(x, y, image::Rgba([0, 0, 255, 255]));
                }
            }
            for y in 0..4 {
                for x in 0..4 {
                    atlas_image.put_pixel(x, y, image::Rgba([0, 255, 0, 255]));
                }
            }
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &atlas_image);

            let size = 64u32;
            // Draw the icon centered on the 64x64 target, covering roughly
            // its middle quarter -- the readback below samples the exact
            // center pixel, which should land inside this rect.
            let instances = [IconInstance {
                screen_x: 16.0,
                screen_y: 16.0,
                width: 32.0,
                height: 32.0,
                uv_min: (0.0, 0.0),
                uv_max: (0.5, 0.5), // the green quadrant only
                rotation_rad: 0.0,
            sdf: None,
            opacity: 1.0,
            }];

            let pixels = render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &instances, size).await;

            let bytes_per_row = size * 4;
            let center_pixel_offset = (size / 2 * bytes_per_row + (size / 2) * 4) as usize;
            let pixel = &pixels[center_pixel_offset..center_pixel_offset + 4];

            assert!(pixel[1] > 200, "expected green channel to be bright (icon drawn), got {pixel:?}");
            assert!(pixel[2] < 50, "expected blue channel to be dark (not the atlas's blue background), got {pixel:?}");
            assert!(pixel[3] > 200, "expected alpha to be opaque where the icon was drawn, got {pixel:?}");
        });
    }

    #[test]
    fn push_quad_replicates_the_instance_rotation_onto_every_vertex() {
        let instance = IconInstance {
            screen_x: 0.0,
            screen_y: 0.0,
            width: 4.0,
            height: 4.0,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            rotation_rad: std::f32::consts::FRAC_PI_2,
            sdf: None,
            opacity: 1.0,
        };
        let mut vertices = Vec::new();
        push_quad(&mut vertices, &instance);
        assert!(vertices.iter().all(|v| v.angle == std::f32::consts::FRAC_PI_2));
        // Offsets stay UN-rotated regardless of the angle.
        assert!(vertices.iter().all(|v| v.offset_px[0].abs() == 2.0 && v.offset_px[1].abs() == 2.0));
    }

    /// A quarter turn in y-down space must move a WIDE bar to a TALL bar.
    /// This proves rotation is actually applied (a no-op `angle` would leave
    /// the bar wide), but it does NOT prove the rotation runs the correct
    /// direction: the instance is a rectangle centred on its own anchor, so
    /// it has 180-degree rotational symmetry, and `-90 = +90 - 180`, so a
    /// sign-flipped basis maps this exact rectangle onto exactly the same
    /// pixels at a quarter turn -- corners `{(dx,dy)}` rotated +90 or -90
    /// both land on the set `{(-dy,dx), (dy,-dx)}` swapped, which for a
    /// centred rectangle is the same four points either way. See
    /// `an_oblique_turn_reveals_a_sign_flipped_rotation_basis` below for the
    /// test that actually distinguishes the two directions.
    #[test]
    fn a_quarter_turn_rotates_the_drawn_icon_about_its_anchor() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            // Atlas: an 8x8 sheet, fully green (the icon is the whole sheet, so
            // UVs are trivial and the SHAPE of the drawn quad is what varies).
            let mut atlas_image = image::RgbaImage::new(8, 8);
            for y in 0..8 {
                for x in 0..8 {
                    atlas_image.put_pixel(x, y, image::Rgba([0, 255, 0, 255]));
                }
            }
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &atlas_image);

            // A 32x4 bar centred at (32, 32) of a 64x64 target: unrotated it
            // covers (44, 32) and not (32, 44); rotated a quarter turn, the
            // reverse. (Sampled at 44, not the box's exact edge at 48: with
            // pixel-center sampling the box's continuous span is the
            // half-open [16, 48), so pixel 48 -- centre (48.5, 48.5) -- is
            // legitimately just outside it. Confirmed by reading back the
            // row: pixel 47 is lit, pixel 48 is not, for both the flat and
            // turned cases symmetrically. 44 keeps a solid margin inside the
            // box while still telling wide from tall.)
            let instance = |rotation_rad: f32| IconInstance {
                screen_x: 16.0,
                screen_y: 30.0,
                width: 32.0,
                height: 4.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                rotation_rad,
                sdf: None,
                opacity: 1.0,
            };

            let flat = render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &[instance(0.0)], 64).await;
            let turned = render_icons_to_pixels(
                &ctx,
                &mut pipeline,
                &atlas_bind_group,
                &[instance(std::f32::consts::FRAC_PI_2)],
                64,
            )
            .await;

            let is_green = |pixels: &[u8], x: u32, y: u32| {
                let i = ((y * 64 + x) * 4) as usize;
                pixels[i] < 64 && pixels[i + 1] > 192
            };

            assert!(is_green(&flat, 44, 32), "unrotated bar must cover a point to its right");
            assert!(!is_green(&flat, 32, 44), "unrotated bar must not reach below its anchor");
            assert!(is_green(&turned, 32, 44), "a quarter turn must swing the bar downward (y-down)");
            assert!(!is_green(&turned, 44, 32), "a quarter turn must clear the point to the right");
        });
    }

    /// The test that actually catches a sign-flipped (or otherwise
    /// mis-oriented) rotation basis: a quarter turn cannot, because the bar
    /// is a rectangle centred on its own anchor with 180-degree rotational
    /// symmetry, so `R(+90)` and `R(-90)` land it on the identical pixel
    /// set (see the doc comment on
    /// `a_quarter_turn_rotates_the_drawn_icon_about_its_anchor`). PI/6 (30
    /// degrees) is not a multiple of 90, so `R(+30)` and `R(-30)` are
    /// genuine mirror images of each other, not the same shape.
    ///
    /// The sampled points are NOT the bar's far tip -- that tip is a
    /// boundary point (the midpoint of the rotated far edge lies exactly on
    /// the rectangle's own boundary, not in its interior), and sampling
    /// exactly on a boundary is exactly the fragile-to-rounding mistake the
    /// fix on `a_quarter_turn_rotates_the_drawn_icon_about_its_anchor`
    /// already had to correct once. `(42, 38)` and `(42, 26)` were instead
    /// picked by rendering `+PI/6` and `-PI/6` and reading back which
    /// pixels the pipeline actually lit (see the task's fix report for the
    /// rendered maps): both points sit solidly inside one rotation's band
    /// and solidly outside the other's, well clear of any edge.
    #[test]
    fn an_oblique_turn_reveals_a_sign_flipped_rotation_basis() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            let mut atlas_image = image::RgbaImage::new(8, 8);
            for y in 0..8 {
                for x in 0..8 {
                    atlas_image.put_pixel(x, y, image::Rgba([0, 255, 0, 255]));
                }
            }
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &atlas_image);

            // Same 32x4 bar, anchored at (32, 32), as the quarter-turn test.
            let instance = |rotation_rad: f32| IconInstance {
                screen_x: 16.0,
                screen_y: 30.0,
                width: 32.0,
                height: 4.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                rotation_rad,
                sdf: None,
                opacity: 1.0,
            };

            let plus = render_icons_to_pixels(
                &ctx,
                &mut pipeline,
                &atlas_bind_group,
                &[instance(std::f32::consts::PI / 6.0)],
                64,
            )
            .await;
            let minus = render_icons_to_pixels(
                &ctx,
                &mut pipeline,
                &atlas_bind_group,
                &[instance(-std::f32::consts::PI / 6.0)],
                64,
            )
            .await;

            let is_green = |pixels: &[u8], x: u32, y: u32| {
                let i = ((y * 64 + x) * 4) as usize;
                pixels[i] < 64 && pixels[i + 1] > 192
            };

            assert!(is_green(&plus, 42, 38), "+PI/6 must swing the bar's far end down towards (42, 38)");
            assert!(!is_green(&plus, 42, 26), "+PI/6 must not reach the mirrored point above the anchor");
            assert!(is_green(&minus, 42, 26), "-PI/6 must swing the bar's far end up towards (42, 26), the mirror image of +PI/6");
            assert!(!is_green(&minus, 42, 38), "-PI/6 must not reach the point +PI/6 covers");
        });
    }

    fn sdf_paint(halo_width: f32, font_scale: f32) -> SdfPaint {
        SdfPaint {
            fill: [1.0, 1.0, 1.0, 1.0],
            halo: [1.0, 0.0, 0.0, 1.0],
            halo_width,
            font_scale,
        }
    }

    /// The fill threshold is the field's own edge, whatever else is set.
    /// Everything else about SDF drawing hangs off this number being the
    /// one the sprite generator encoded against.
    #[test]
    fn the_fill_threshold_is_the_encoded_edge() {
        let (buffer, _) = sdf_thresholds(&sdf_paint(4.0, 2.0), false);
        assert_eq!(buffer, SDF_EDGE);
        // And it does not move when the halo does -- the two quads
        // threshold the same field independently.
        let (wide, _) = sdf_thresholds(&sdf_paint(40.0, 2.0), false);
        assert_eq!(wide, SDF_EDGE);
    }

    /// A halo's threshold sits BELOW the edge -- further out into the field,
    /// where the distance is smaller -- and a wider halo pushes it further.
    /// Backwards, and the border would eat into the shape instead of
    /// surrounding it.
    #[test]
    fn a_wider_halo_pushes_its_threshold_further_outside_the_shape() {
        let (narrow, _) = sdf_thresholds(&sdf_paint(2.0, 1.0), true);
        let (wide, _) = sdf_thresholds(&sdf_paint(6.0, 1.0), true);
        assert!(narrow < SDF_EDGE, "a halo must threshold outside the edge, got {narrow}");
        assert!(wide < narrow, "wider must reach further out: {wide} vs {narrow}");
    }

    /// `icon-halo-width` is a width in PHYSICAL PIXELS, so the same style
    /// value has to mean the same number of pixels whatever size the sprite
    /// is drawn at. Two icons drawn at different scales with a halo scaled
    /// to match must land on the same threshold.
    #[test]
    fn the_halo_threshold_tracks_the_drawn_scale_so_its_pixel_width_is_fixed() {
        // Twice the drawn scale, twice the pixel width: the same border.
        let (small, _) = sdf_thresholds(&sdf_paint(3.0, 1.0), true);
        let (large, _) = sdf_thresholds(&sdf_paint(6.0, 2.0), true);
        assert!((small - large).abs() < 1e-6, "{small} vs {large}");
    }

    /// A halo wider than the field can express saturates rather than
    /// inverting. Without the clamp the threshold goes negative, every
    /// texel of the quad counts as inside, and the pin renders as a solid
    /// rectangle of halo colour -- a spectacular failure for what is only
    /// an over-large style value. The clamp is half of it: what keeps the
    /// empty texels at a 0 threshold from drawing at half coverage is the
    /// shader's floor, pinned by
    /// `a_halo_at_the_fields_reach_leaves_the_empty_margin_untouched`.
    #[test]
    fn an_absurdly_wide_halo_saturates_instead_of_filling_the_whole_quad() {
        let (buffer, _) = sdf_thresholds(&sdf_paint(1000.0, 1.0), true);
        assert_eq!(buffer, 0.0);
    }

    /// The antialiased edge is measured in the FIELD's units, so drawing the
    /// same sprite larger -- more physical pixels across the same gradient
    /// -- must narrow it. A fixed gamma would make big icons look blurry
    /// and small ones aliased.
    #[test]
    fn the_edge_sharpens_as_the_icon_is_drawn_larger() {
        let (_, small) = sdf_thresholds(&sdf_paint(0.0, 1.0), false);
        let (_, large) = sdf_thresholds(&sdf_paint(0.0, 4.0), false);
        assert!(large < small, "a larger icon must have a tighter edge: {large} vs {small}");
        assert!((large * 4.0 - small).abs() < 1e-6, "and inversely proportional");
    }

    /// The DPR half of the root-cause claim in the sdf-gamma-device-pixel-
    /// ratio task, pinned as a test so it cannot be "fixed" back in by
    /// mistake: `font_scale` here already carries the window's scale
    /// factor (see `symbol_pass::icon_font_scale`/`render::glyph_font_scale`
    /// and `SdfPaint::font_scale`'s doc comment), unlike MapLibre's
    /// `fontScale`, which is style-size-only and needs `EDGE_GAMMA` divided
    /// by `u_device_pixel_ratio` separately. So for the SAME logical icon
    /// size, going from a 1x to a 2x to a 3x scale factor must already
    /// halve, then third, `gamma` here -- with no extra division anywhere
    /// in this module -- and must hold the smoothstep's PHYSICAL pixel
    /// half-width (`gamma * SDF_PX * font_scale`) constant across all three.
    #[test]
    fn gamma_already_halves_and_thirds_with_scale_factor_through_font_scale_alone() {
        // font_scale for the same logical size at scale factor 1x, 2x, 3x --
        // exactly what `symbol_pass::icon_font_scale`/`render::glyph_font_scale`
        // hand `SdfPaint::font_scale` for a fixed logical icon/text size.
        let logical_size_texels = 2.0;
        let (_, gamma_1x) = sdf_thresholds(&sdf_paint(0.0, logical_size_texels * 1.0), false);
        let (_, gamma_2x) = sdf_thresholds(&sdf_paint(0.0, logical_size_texels * 2.0), false);
        let (_, gamma_3x) = sdf_thresholds(&sdf_paint(0.0, logical_size_texels * 3.0), false);

        assert!(
            (gamma_2x - gamma_1x / 2.0).abs() < 1e-6,
            "a 2x scale factor must halve gamma: {gamma_1x} -> {gamma_2x}"
        );
        assert!(
            (gamma_3x - gamma_1x / 3.0).abs() < 1e-6,
            "a 3x scale factor must third gamma: {gamma_1x} -> {gamma_3x}"
        );

        // And the physical-pixel band this produces must be the SAME
        // constant (EDGE_GAMMA * SDF_PX) at every scale factor -- dividing
        // EDGE_GAMMA by the scale factor a second time would shrink it
        // instead, at exactly the retina densities this task is about.
        let physical_band = |gamma: f32, font_scale: f32| gamma * SDF_PX * font_scale;
        let band_1x = physical_band(gamma_1x, logical_size_texels * 1.0);
        let band_2x = physical_band(gamma_2x, logical_size_texels * 2.0);
        let band_3x = physical_band(gamma_3x, logical_size_texels * 3.0);
        assert!((band_1x - EDGE_GAMMA * SDF_PX).abs() < 1e-6, "{band_1x}");
        assert!((band_2x - band_1x).abs() < 1e-6, "band must not move at 2x: {band_1x} vs {band_2x}");
        assert!((band_3x - band_1x).abs() < 1e-6, "band must not move at 3x: {band_1x} vs {band_3x}");
    }

    /// A zero-height quad must not produce NaN thresholds. It reaches here
    /// through an icon culled to nothing rather than through a bug, and NaN
    /// coverage would silently blank the icon.
    #[test]
    fn a_degenerate_scale_falls_back_rather_than_producing_nan() {
        let (buffer, gamma) = sdf_thresholds(&sdf_paint(2.0, 0.0), true);
        assert!(buffer.is_finite() && gamma.is_finite(), "{buffer}, {gamma}");
    }

    /// A plain sprite still emits exactly one quad -- SDF support must cost
    /// the common case nothing.
    #[test]
    fn a_plain_icon_emits_one_quad() {
        let instance = IconInstance {
            screen_x: 0.0, screen_y: 0.0, width: 8.0, height: 8.0,
            uv_min: (0.0, 0.0), uv_max: (1.0, 1.0), rotation_rad: 0.0,
            sdf: None,
            opacity: 1.0,
        };
        let mut vertices = Vec::new();
        push_quad(&mut vertices, &instance);
        assert_eq!(vertices.len(), 6);
        assert!(vertices.iter().all(|v| v.sdf_params[2] == 0.0), "and is not flagged as SDF");
    }

    /// `paint.icon-opacity` reaches BOTH kinds of quad. A plain sprite has
    /// no colour of its own to fold it into -- which is precisely why the
    /// opacity travels in `sdf_params.w` rather than in `color` -- and an
    /// SDF sprite's halo has to fade with its fill or a half-opaque pin
    /// shows a solid border around nothing.
    #[test]
    fn icon_opacity_reaches_every_quad_of_both_a_plain_and_an_sdf_icon() {
        let plain = IconInstance {
            screen_x: 0.0, screen_y: 0.0, width: 8.0, height: 8.0,
            uv_min: (0.0, 0.0), uv_max: (1.0, 1.0), rotation_rad: 0.0,
            sdf: None,
            opacity: 0.4,
        };
        let mut vertices = Vec::new();
        push_quad(&mut vertices, &plain);
        assert!(vertices.iter().all(|v| (v.sdf_params[3] - 0.4).abs() < 1e-6), "a plain sprite fades too");

        let sdf = IconInstance {
            sdf: Some(SdfPaint {
                fill: [1.0, 0.0, 0.0, 1.0],
                halo: [0.0, 0.0, 1.0, 1.0],
                halo_width: 2.0,
                font_scale: 1.0,
            }),
            opacity: 0.25,
            ..plain
        };
        let mut vertices = Vec::new();
        push_quad(&mut vertices, &sdf);
        assert_eq!(vertices.len(), 12, "halo quad then fill quad");
        assert!(vertices.iter().all(|v| (v.sdf_params[3] - 0.25).abs() < 1e-6), "halo and fill fade together");
    }

    /// An SDF icon with a halo emits TWO quads, halo FIRST. The order is the
    /// whole mechanism: within one draw call the later primitive blends over
    /// the earlier, so fill-then-halo would paint the border on top of the
    /// face.
    #[test]
    fn an_sdf_icon_emits_its_halo_before_its_fill() {
        let instance = IconInstance {
            screen_x: 0.0, screen_y: 0.0, width: 8.0, height: 8.0,
            uv_min: (0.0, 0.0), uv_max: (1.0, 1.0), rotation_rad: 0.0,
            sdf: Some(sdf_paint(2.0, 1.0)),
            opacity: 1.0,
        };
        let mut vertices = Vec::new();
        push_quad(&mut vertices, &instance);
        assert_eq!(vertices.len(), 12, "halo quad then fill quad");
        assert_eq!(vertices[0].color, [1.0, 0.0, 0.0, 1.0], "first six are the halo");
        assert_eq!(vertices[6].color, [1.0, 1.0, 1.0, 1.0], "last six are the fill");
        assert!(vertices[0].sdf_params[0] < vertices[6].sdf_params[0], "the halo thresholds further out");
        assert!(vertices.iter().all(|v| v.sdf_params[2] == 1.0));
    }

    /// No halo asked for, no halo quad drawn. Emitting one anyway would
    /// double every symbol's vertex count to contribute nothing but fully
    /// transparent fragments.
    #[test]
    fn an_sdf_icon_with_no_halo_emits_only_its_fill() {
        for paint in [sdf_paint(0.0, 1.0), SdfPaint { halo: [1.0, 0.0, 0.0, 0.0], ..sdf_paint(4.0, 1.0) }] {
            let instance = IconInstance {
                screen_x: 0.0, screen_y: 0.0, width: 8.0, height: 8.0,
                uv_min: (0.0, 0.0), uv_max: (1.0, 1.0), rotation_rad: 0.0,
                sdf: Some(paint),
                opacity: 1.0,
            };
            let mut vertices = Vec::new();
            push_quad(&mut vertices, &instance);
            assert_eq!(vertices.len(), 6, "only the fill, for {paint:?}");
        }
    }

    /// Neighbouring sprites must not touch. Bilinear sampling reads a
    /// quad's outermost texel together with whatever abuts it, and for an
    /// SDF sprite -- whose alpha is thresholded, not blended -- a neighbour
    /// bleeding in draws a stray fragment of shape rather than a soft edge.
    #[test]
    fn packing_leaves_a_gap_between_neighbouring_sprites() {
        let mut manifest = HashMap::new();
        manifest.insert(
            "a".to_string(),
            crate::sprite::SpriteRect { x: 0, y: 0, width: 4, height: 4, pixel_ratio: 1.0, sdf: true, ..Default::default() },
        );
        manifest.insert(
            "b".to_string(),
            crate::sprite::SpriteRect { x: 4, y: 0, width: 4, height: 4, pixel_ratio: 1.0, sdf: false, ..Default::default() },
        );
        let source = image::RgbaImage::new(8, 4);
        let sprites = crate::sprite::slice_atlas(&to_png(&source), &manifest).expect("slices");
        let (_, rects) = pack_icon_atlas(&sprites, &["a".to_string(), "b".to_string()]);
        let a = rects["a"];
        let b = rects["b"];
        assert!(
            a.x + a.width < b.x || b.x + b.width < a.x,
            "the two rects must not abut: {a:?} and {b:?}"
        );
        // And the manifest's `sdf` flag survives the pack, which is what
        // selects the shader branch at draw time.
        assert!(a.sdf, "an SDF sprite must still be marked SDF after packing");
        assert!(!b.sdf);
    }

    fn to_png(image: &image::RgbaImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encodes");
        bytes
    }

    /// End to end, on a real device: an SDF sprite draws in the colour the
    /// STYLE gives it, not in the colour stored in its own pixels.
    ///
    /// The atlas here is deliberately blue-channelled nonsense whose ALPHA
    /// carries the field. A renderer that ignored `sdf` and sampled RGBA
    /// would draw blue; the assertion is that it draws the requested green.
    #[test]
    fn an_sdf_icon_is_drawn_in_its_style_colour_not_its_own_pixels() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            // 8x8, fully "inside" the shape (alpha 255 is well above the
            // 0.75 edge), with a blue RGB nobody should ever see.
            let mut atlas_image = image::RgbaImage::new(8, 8);
            for y in 0..8 {
                for x in 0..8 {
                    atlas_image.put_pixel(x, y, image::Rgba([0, 0, 255, 255]));
                }
            }
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &atlas_image);

            let size = 64u32;
            let instance = IconInstance {
                screen_x: 16.0,
                screen_y: 16.0,
                width: 32.0,
                height: 32.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                rotation_rad: 0.0,
                opacity: 1.0,
                sdf: Some(SdfPaint {
                    fill: [0.0, 1.0, 0.0, 1.0],
                    halo: [0.0, 0.0, 0.0, 0.0],
                    halo_width: 0.0,
                    font_scale: 4.0,
                }),
            };
            let pixels =
                render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &[instance], size).await;

            let centre = ((size / 2 * size + size / 2) * 4) as usize;
            let (r, g, b, a) = (
                pixels[centre],
                pixels[centre + 1],
                pixels[centre + 2],
                pixels[centre + 3],
            );
            assert!(g > 200, "the style's green must be what lands, got ({r},{g},{b},{a})");
            assert!(b < 60, "the sprite's own blue must NOT: got ({r},{g},{b},{a})");
            assert!(a > 200, "and fully inside the shape must be opaque: got {a}");
        });
    }

    /// The halo is drawn OUTSIDE the shape. With a field that is uniformly
    /// "inside", a halo has nowhere to show -- so this uses a field with a
    /// real edge and checks the colour just beyond it.
    #[test]
    fn the_halo_colour_appears_outside_the_shapes_edge() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            // A 16x16 field: alpha falls off with distance from the centre,
            // so the shape is a disc and there is real field outside it.
            let mut atlas_image = image::RgbaImage::new(16, 16);
            for y in 0..16 {
                for x in 0..16 {
                    let dx = x as f32 - 7.5;
                    let dy = y as f32 - 7.5;
                    let distance = (dx * dx + dy * dy).sqrt();
                    // 0 at the disc's radius, +/- SDF_PX texels of range
                    // either side -- the standard encoding.
                    let signed = (4.0 - distance) / SDF_PX + SDF_EDGE;
                    atlas_image.put_pixel(
                        x,
                        y,
                        image::Rgba([0, 0, 0, (signed.clamp(0.0, 1.0) * 255.0) as u8]),
                    );
                }
            }
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &atlas_image);

            let size = 64u32;
            let instance = IconInstance {
                screen_x: 0.0,
                screen_y: 0.0,
                width: 64.0,
                height: 64.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                rotation_rad: 0.0,
                opacity: 1.0,
                sdf: Some(SdfPaint {
                    fill: [0.0, 1.0, 0.0, 1.0],
                    halo: [1.0, 0.0, 0.0, 1.0],
                    halo_width: 8.0,
                    font_scale: 4.0,
                }),
            };
            let pixels =
                render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &[instance], size).await;
            let at = |x: u32, y: u32| {
                let i = ((y * size + x) * 4) as usize;
                (pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3])
            };

            // Dead centre is inside the disc: the FILL colour.
            let (_, g, _, a) = at(32, 32);
            assert!(g > 200 && a > 200, "the disc's centre must be the fill colour, got {:?}", at(32, 32));

            // The disc's radius is 4 of 16 texels, drawn over 64 px, so its
            // edge is at 16 px from centre. Two pixels beyond it is halo.
            let (r, g, _, a) = at(32 + 18, 32);
            assert!(r > 200, "just outside the edge must be the halo colour, got {:?}", at(32 + 18, 32));
            assert!(g < 60, "and not the fill colour");
            assert!(a > 150, "and actually drawn");
        });
    }

    /// THE property this whole arrangement exists for: two overlapping SDF
    /// pins resolve as one cleanly in front of the other, rather than their
    /// borders blending together.
    ///
    /// mapbox-gl cannot do this (mapbox/mapbox-gl-js#11504, confirmed by a
    /// maintainer and closed without a fix) because it batches its symbol
    /// drawing by PASS: every halo in the layer, then every fill. The
    /// second pin's halo therefore lands on top of the first pin's finished
    /// face. Here each symbol contributes `[halo, fill]` adjacently in one
    /// draw call, so the second pin's fill is the last thing written over
    /// the overlap.
    ///
    /// The assertion is on a point inside the SECOND disc and inside the
    /// FIRST disc's halo ring: it must be the second's fill colour, with no
    /// trace of the first's border.
    #[test]
    fn a_later_sdf_icon_covers_an_earlier_ones_border_instead_of_blending_with_it() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);

            // The same disc field as the halo test: radius 4 of 16 texels.
            let mut atlas_image = image::RgbaImage::new(16, 16);
            for y in 0..16 {
                for x in 0..16 {
                    let dx = x as f32 - 7.5;
                    let dy = y as f32 - 7.5;
                    let signed = (4.0 - (dx * dx + dy * dy).sqrt()) / SDF_PX + SDF_EDGE;
                    atlas_image.put_pixel(x, y, image::Rgba([0, 0, 0, (signed.clamp(0.0, 1.0) * 255.0) as u8]));
                }
            }
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &atlas_image);

            let size = 64u32;
            // Two 32px discs, 12px apart horizontally, so they overlap.
            // FIRST is red-bordered blue; SECOND is green with a black
            // border. Both borders are wide enough to be unmissable.
            let disc = |left: f32, fill: [f32; 4], halo: [f32; 4]| IconInstance {
                screen_x: left,
                screen_y: 16.0,
                width: 32.0,
                height: 32.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                rotation_rad: 0.0,
                opacity: 1.0,
                sdf: Some(SdfPaint {
                    fill,
                    halo,
                    halo_width: 6.0,
                    font_scale: 2.0,
                }),
            };
            let first = disc(10.0, [0.0, 0.0, 1.0, 1.0], [1.0, 0.0, 0.0, 1.0]);
            let second = disc(22.0, [0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 0.0, 1.0]);

            let pixels =
                render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &[first, second], size).await;
            let at = |x: u32, y: u32| {
                let i = ((y * size + x) * 4) as usize;
                (pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3])
            };

            // Each disc's centre: (26, 32) and (38, 32). The overlap sits
            // between them, and (34, 32) is inside the second disc while
            // still within reach of the first's border.
            let overlap = at(34, 32);
            assert!(
                overlap.1 > 200,
                "inside the later disc must be ITS fill, got {overlap:?}"
            );
            assert!(
                overlap.0 < 60,
                "with no red bleeding through from the earlier disc's border: {overlap:?}"
            );

            // And the earlier disc is still itself where nothing covers it.
            let untouched = at(20, 32);
            assert!(untouched.2 > 150, "the earlier disc keeps its own fill: {untouched:?}");
        });
    }

    /// A small helper atlas and single-icon instance, for the buffer-reuse
    /// tests below -- none of them care what the icon looks like, only how
    /// many GPU buffers `render` allocates to draw it.
    fn one_icon(screen_x: f32) -> IconInstance {
        IconInstance {
            screen_x,
            screen_y: 8.0,
            width: 8.0,
            height: 8.0,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            rotation_rad: 0.0,
            sdf: None,
            opacity: 1.0,
        }
    }

    /// The defect this pipeline shipped with, same class as
    /// `PlatePipeline`'s (see `the_vertex_buffer_is_allocated_once_and_reused_across_frames`
    /// on that type): `render` built a fresh `create_buffer_init` vertex
    /// buffer on EVERY call, and it is called once per frame from
    /// `MapView::paint` whenever any symbol icon is on screen. That made
    /// `gpu_buffers_created_total` climb at frame rate for as long as icons
    /// were visible.
    ///
    /// Identity, not size: two buffers of the same size are
    /// indistinguishable by `tracked_size()`, and "the same buffer came
    /// back" is the whole claim.
    #[test]
    fn the_vertex_buffer_is_allocated_once_and_reused_across_frames() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &image::RgbaImage::new(4, 4));
            let instances = [one_icon(4.0)];

            render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &instances, 32).await;
            let first = pipeline.vertex_buffer.as_ref().expect("a vertex buffer after the first frame").handle();

            for frame in 1..8 {
                render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &instances, 32).await;
                let now = pipeline.vertex_buffer.as_ref().expect("still a vertex buffer").handle();
                assert_eq!(now, first, "frame {frame} must reuse the buffer frame 0 allocated, not allocate its own");
            }
        });
    }

    /// Grow-and-keep, mirroring `PlatePipeline`'s
    /// `the_vertex_buffer_grows_for_a_bigger_batch_and_never_shrinks_back`.
    /// This matters MORE here than for the plate's one fixed quad: real
    /// symbol frames genuinely vary their icon count a lot as features
    /// enter and leave the viewport, so an exact-size-match reuse guard
    /// would reallocate on nearly every frame. A high-water mark converges
    /// after a handful of frames and then allocates nothing.
    #[test]
    fn the_vertex_buffer_grows_for_a_bigger_batch_and_never_shrinks_back() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &image::RgbaImage::new(4, 4));
            let one = [one_icon(4.0)];
            let six: Vec<IconInstance> = (0..6).map(|i| one_icon(4.0 + i as f32 * 4.0)).collect();

            render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &one, 64).await;
            let small = pipeline.vertex_buffer.as_ref().expect("a buffer").tracked_size();

            render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &six, 64).await;
            let grown = pipeline.vertex_buffer.as_ref().expect("a buffer").handle();
            let grown_size = pipeline.vertex_buffer.as_ref().expect("a buffer").tracked_size();
            assert!(grown_size > small, "six icons must not fit in a one-icon buffer ({grown_size} vs {small})");

            render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &one, 64).await;
            assert_eq!(
                pipeline.vertex_buffer.as_ref().expect("a buffer").handle(),
                grown,
                "a smaller frame must keep the grown buffer, not allocate a tighter one"
            );
            assert_eq!(pipeline.vertex_buffer.as_ref().expect("a buffer").tracked_size(), grown_size, "capacity must never shrink");
        });
    }

    /// The correctness half of reuse, and the one this pipeline's own doc
    /// comment on `vertex_buffer` calls out by name: a reused buffer still
    /// holds the PREVIOUS, LARGER frame's icon quads past the end of this
    /// frame's fewer icons, and `render` must not draw them. Ghost icons
    /// left over from a bigger frame are exactly what a `draw` call bounded
    /// by the buffer's capacity, rather than by this frame's own vertex
    /// count, would produce -- and no unit test that only checks the icons
    /// this frame DOES draw would catch it, so this one specifically checks
    /// where an icon used to be that this frame does not ask for.
    #[test]
    fn a_shrinking_batch_of_icons_does_not_draw_the_previous_frames_leftover_instances() {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let mut atlas_image = image::RgbaImage::new(4, 4);
            for y in 0..4 {
                for x in 0..4 {
                    atlas_image.put_pixel(x, y, image::Rgba([0, 255, 0, 255]));
                }
            }
            let atlas_bind_group = pipeline.upload_atlas(&ctx, &atlas_image);
            let size = 64u32;
            let near = IconInstance {
                screen_x: 8.0,
                screen_y: 8.0,
                width: 16.0,
                height: 16.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                rotation_rad: 0.0,
                sdf: None,
                opacity: 1.0,
            };
            let far = IconInstance { screen_x: 48.0, screen_y: 48.0, ..near };

            let is_green = |pixels: &[u8], x: u32, y: u32| {
                let i = ((y * size + x) * 4) as usize;
                pixels[i] < 64 && pixels[i + 1] > 192
            };

            let both = render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &[near, far], size).await;
            assert!(is_green(&both, 8, 8), "the near icon must draw");
            assert!(is_green(&both, 48, 48), "the far icon must draw");

            let just_near = render_icons_to_pixels(&ctx, &mut pipeline, &atlas_bind_group, &[near], size).await;
            assert!(is_green(&just_near, 8, 8), "the near icon must still draw");
            assert!(
                !is_green(&just_near, 48, 48),
                "the far icon's vertices are still in the reused buffer's tail but must not be drawn"
            );
        });
    }
}
