// The tile-status debug overlay's shader: flat-coloured screen-space
// triangles, and nothing else.
//
// Deliberately the simplest pipeline in this renderer. `plate_shader.wgsl`
// evaluates a rounded-box SDF per fragment because an attribution plate has
// corners; `shader.wgsl` extrudes stroke normals because a map line has
// width in world space. This draws neither: `render::debug_tiles` has
// already projected every tile corner to a screen pixel on the CPU in f64
// and built the fill and border quads from those pixels, so all that is
// left is to map a pixel to clip space and pass a colour through.
//
// That is also the whole of the answer to the stroke-precision problem
// recorded in
// `docs/superpowers/specs/2026-08-25-overlay-stroke-precision-design.md`:
// no normal is ever derived from a difference of large coordinates here,
// because no normal reaches the GPU at all.

struct Viewport {
    size: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0) var<uniform> viewport: Viewport;

struct VertexInput {
    // y-down PHYSICAL screen pixels -- the same convention as
    // `IconVertex::anchor_px` and `PlateVertex::anchor_px`.
    @location(0) position_px: vec2<f32>,
    // Straight (non-premultiplied) sRGB, alpha included.
    @location(1) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(
        (input.position_px.x / viewport.size.x) * 2.0 - 1.0,
        1.0 - (input.position_px.y / viewport.size.y) * 2.0,
        0.0,
        1.0,
    );
    out.color = input.color;
    return out;
}

/// # Colour space: this shader writes sRGB, and does not linearise
///
/// Same contract as `plate_shader.wgsl` -- see its long-form note. The
/// render target is a UNORM format, so the value written here is the value
/// stored and the fixed-function blender composites on sRGB-encoded values,
/// which is what MapLibre and Mapbox both do. Do NOT add a transfer
/// function here without changing the target format in the same commit.
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
