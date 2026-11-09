// Story markers (spec §6.3-§6.4): screen-space discs with MapLibre's
// circle paint -- a fill to `radius`, a stroke `stroke` wide outside it,
// and `circle-blur` -- evaluated analytically per fragment, as
// `plate_shader.wgsl` evaluates its rounded rect. The edge formula is
// MapLibre GL JS's `circle.fragment.glsl` (BSD-3), restated for straight
// alpha: `opacity_t` fades the disc over its outer `blur` (at least one
// pixel of antialias), `color_t` mixes fill into stroke over the same
// width at the fill's edge.

struct Viewport {
    size: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0) var<uniform> viewport: Viewport;

struct VertexInput {
    @location(0) anchor_px: vec2<f32>,
    @location(1) offset_px: vec2<f32>,
    /// (fill radius, stroke width, blur, unused), physical px / fraction.
    @location(2) shape: vec4<f32>,
    @location(3) fill: vec4<f32>,
    @location(4) stroke: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local_pos: vec2<f32>,
    @location(1) shape: vec4<f32>,
    @location(2) fill: vec4<f32>,
    @location(3) stroke: vec4<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    let p = input.anchor_px + input.offset_px;
    var out: VertexOutput;
    out.clip_position = vec4<f32>((p.x / viewport.size.x) * 2.0 - 1.0, 1.0 - (p.y / viewport.size.y) * 2.0, 0.0, 1.0);
    out.local_pos = input.offset_px;
    out.shape = input.shape;
    out.fill = input.fill;
    out.stroke = input.stroke;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let radius = in.shape.x;
    let stroke_width = in.shape.y;
    let outer = max(radius + stroke_width, 1e-3);
    // Normalised to the outer radius, as MapLibre's `v_data.xy`.
    let extrude_length = length(in.local_pos) / outer;
    let antialiased_blur = -max(in.shape.z, 1.0 / outer);
    // MapLibre writes `smoothstep(0.0, -blur, x)`; WGSL's smoothstep
    // wants edge0 < edge1, so the same ramp spelled ascending.
    let opacity_t = 1.0 - smoothstep(antialiased_blur, 0.0, extrude_length - 1.0);
    var color_t = 0.0;
    if (stroke_width >= 0.01) {
        color_t = smoothstep(antialiased_blur, 0.0, extrude_length - radius / outer);
    }
    // Straight alpha: mix premultiplied, then divide back out.
    let alpha = mix(in.fill.a, in.stroke.a, color_t);
    let premul = mix(in.fill.rgb * in.fill.a, in.stroke.rgb * in.stroke.a, color_t);
    let rgb = select(vec3<f32>(0.0), premul / max(alpha, 1e-6), alpha > 0.0);
    return vec4<f32>(rgb, alpha * opacity_t);
}
