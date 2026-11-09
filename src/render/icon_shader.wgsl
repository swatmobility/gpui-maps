// Symbol icon shader: a textured-quad vertex/fragment pair.
//
// Quad corners arrive as an anchor (the quad's centre, in y-down physical
// screen pixels) plus an UN-rotated per-corner offset and an angle. The
// vertex stage rotates the offset about the anchor and converts the result
// to clip space, so the CPU never computes a rotated corner position and
// never rewrites corner offsets when a symbol's angle changes.
//
// This mirrors MapLibre's `symbol_icon.vertex.glsl`, which applies
// `mat2(cos, -sin, sin, cos)` to its own `a_offset` and takes the angle
// from a per-symbol dynamic attribute (`a_projected_pos.z`).

struct Viewport {
    size: vec2<f32>,
    _pad: vec2<f32>,
}

@group(1) @binding(0) var<uniform> viewport: Viewport;

struct VertexInput {
    @location(0) anchor_px: vec2<f32>,
    @location(1) offset_px: vec2<f32>,
    @location(2) angle: f32,
    @location(3) uv: vec2<f32>,
    @location(4) color: vec4<f32>,
    /// `(buffer, gamma, is_sdf, opacity)` -- see `IconVertex::sdf_params`.
    @location(5) sdf_params: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) sdf_params: vec4<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    // y-down space, so a POSITIVE angle turns clockwise on screen. Same
    // basis as MapLibre's mat2(cos, -sin, sin, cos).
    let s = sin(input.angle);
    let c = cos(input.angle);
    let rotated = vec2<f32>(
        c * input.offset_px.x - s * input.offset_px.y,
        s * input.offset_px.x + c * input.offset_px.y,
    );
    let p = input.anchor_px + rotated;

    var out: VertexOutput;
    // Physical pixels (top-left origin, y-down) to clip-space NDC. No
    // horizontal flip: it is already baked into the screen coordinates the
    // caller starts from (`project_clip_to_screen`).
    out.clip_position = vec4<f32>(
        (p.x / viewport.size.x) * 2.0 - 1.0,
        1.0 - (p.y / viewport.size.y) * 2.0,
        0.0,
        1.0,
    );
    out.uv = input.uv;
    out.color = input.color;
    out.sdf_params = input.sdf_params;
    return out;
}

@group(0) @binding(0) var icon_texture: texture_2d<f32>;
@group(0) @binding(1) var icon_sampler: sampler;

/// # Colour space: this shader writes sRGB, and does not linearise
///
/// The render target is a UNORM format, not `*Srgb` (see `map_view.rs`'s
/// surface-format selection for the full reasoning). So the value written
/// here is the value stored, and the fixed-function blender composites
/// translucent layers on sRGB-ENCODED values -- which is what MapLibre GL
/// JS, Mapbox GL JS and all four MapLibre Native backends do, and what the
/// styles' alphas were chosen against.
///
/// This shader therefore writes the authored sRGB colour through unchanged.
/// It used to convert to linear here and let an `Srgb` target encode it back
/// on write; the two cancelled for every opaque pixel, and existed only to
/// move the blend into linear light. Removing both is what moved it back.
/// Do NOT reintroduce a transfer function here without changing the target
/// format in the same commit -- either one alone is a map about 2.2 gamma
/// out.

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // `icon_texture` is uploaded as `Rgba8Unorm` (see
    // `IconPipeline::upload_atlas`'s doc comment) -- sampling it does NOT
    // decode, so the sprite's own sRGB-encoded pixels arrive here as
    // authored and go straight out to the Unorm render target unchanged.
    let texel = textureSample(icon_texture, icon_sampler, in.uv);

    // `paint.icon-opacity`, applied to ALPHA alone on both branches. This
    // pipeline blends with straight (non-premultiplied) alpha, so scaling
    // the colour channels too would fade the sprite towards black instead
    // of towards whatever is behind it.
    let opacity = in.sdf_params.w;

    if (in.sdf_params.z < 0.5) {
        // A plain RGBA sprite: its own pixels, exactly as before SDF
        // support existed.
        return vec4<f32>(texel.rgb, texel.a * opacity);
    }

    // An SDF sprite. Its ALPHA is distance from the shape's edge, not
    // coverage. Nothing in the colour-space change above touches it: alpha
    // was never sRGB-encoded even when the atlas was an `Rgba8UnormSrgb`
    // texture (the transfer function applies to the three colour channels
    // only), so this has always been, and still is, the raw stored
    // distance.
    //
    // MapLibre's `symbol_sdf.fragment.glsl`, whose one line this is:
    //   alpha = smoothstep(buff - gamma_scaled, buff + gamma_scaled, dist)
    // The `buffer` decides WHERE the edge is -- the shape's own edge for
    // the fill quad, further out into the field for the halo quad -- and
    // `gamma` how softly it fades. Both are resolved on the CPU by
    // `sdf_thresholds`, because both depend on the device pixel ratio and
    // the icon's drawn scale, neither of which this stage otherwise knows.
    let distance = texel.a;
    let buffer = in.sdf_params.x;
    let gamma = in.sdf_params.y;
    //
    // One departure: the lower edge never goes below 0. Alpha 0 is the
    // FLOOR of the encoding -- every texel 6 or more field texels outside
    // the shape, i.e. the sprite's whole empty margin -- not a distance
    // the halo can claim. A halo whose threshold lands within `gamma` of
    // that floor (`icon-halo-width` near 6 field texels: 2 pt on a 3x
    // sprite, 1.9 pt is enough on a 2x display) would otherwise give every
    // empty texel partial coverage, and the quad would draw as a
    // translucent rectangle in the halo colour. Floored, a texel at the
    // floor is never covered, so the halo saturates at the field's reach
    // instead. MapLibre's own sprites never get here: its halo width is in
    // the sprite's own texels, not in points times the pixel ratio.
    let lower = max(buffer - gamma, 0.0);
    let upper = max(buffer + gamma, lower + 1.0 / 255.0);
    let coverage = smoothstep(lower, upper, distance);
    return vec4<f32>(in.color.rgb, in.color.a * coverage * opacity);
}
