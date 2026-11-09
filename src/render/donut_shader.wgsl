// Donut clusters: one instanced screen-space quad per donut, shaded as a
// signed-distance ring cut into up to eight sectors (spec §5.2).
//
// Modelled on `icon_shader.wgsl`: the anchor arrives CPU-projected in
// y-down physical pixels (the icon path's `project_world_to_screen_on_frame`,
// so a donut sits on the globe where its point is), and the vertex stage
// only offsets the quad's corners around it.
//
// A module of its own, so its inter-stage count is its own: `VertexOutput`
// carries 4 locations (uv, cum_a, cum_b, params), well inside the 15
// inter-stage variables iOS allows. Everything that selects is FLOAT
// arithmetic (`step`, `dot`, a scalar index): no `mix`/`select` over
// integer or bool vectors, which naga writes as an ESSL 3.10 overload that
// WebGL2 rejects (see `render::dots`'s GLSL ES 300 test, which this shader
// joins).

struct Frame {
    /// The render target, in physical pixels.
    size: vec2<f32>,
    _pad: vec2<f32>,
    /// The layer's eight group colours, sRGB-encoded straight alpha.
    colors: array<vec4<f32>, 8>,
}

@group(0) @binding(0) var<uniform> frame: Frame;

struct InstanceInput {
    @location(0) anchor_px: vec2<f32>,
    @location(1) radius_px: f32,
    @location(2) hole: f32,
    @location(3) cum_a: vec4<f32>,
    @location(4) cum_b: vec4<f32>,
    @location(5) alpha: f32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) cum_a: vec4<f32>,
    @location(2) cum_b: vec4<f32>,
    @location(3) params: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, input: InstanceInput) -> VertexOutput {
    // Triangle-strip corners: (-1,-1), (1,-1), (-1,1), (1,1).
    let corner = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u)) * 2.0 - 1.0;
    // One pixel of fringe past the rim, for the antialiased edge.
    let half = input.radius_px + 1.0;
    // A hidden donut (alpha 0: the far side of the globe, or faded out)
    // collapses to a point, so it costs no fragments.
    let shown = step(1e-4, input.alpha) * step(1e-4, input.radius_px);
    let p = input.anchor_px + corner * half * shown;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(
        (p.x / frame.size.x) * 2.0 - 1.0,
        1.0 - (p.y / frame.size.y) * 2.0,
        0.0,
        1.0,
    );
    // uv is 1 at the rim, so the quad's corner is (radius + 1) / radius.
    out.uv = corner * (half / max(input.radius_px, 1e-3));
    out.cum_a = input.cum_a;
    out.cum_b = input.cum_b;
    out.params = vec4<f32>(input.radius_px, input.hole, input.alpha, 0.0);
    return out;
}

const TAU: f32 = 6.283185307179586;

/// The first `k` with `t < cum[k]`: the count of `cum[k] <= t`, in floats.
/// `donut::sector_of` is its CPU mirror. `t` is clamped below 1 (`T_MAX`)
/// because `fract` of a tiny negative angle can return exactly 1.0, which
/// every `cum[k]` would pass, landing on group 7.
fn sector(t: f32, cum_a: vec4<f32>, cum_b: vec4<f32>) -> u32 {
    let tt = vec4<f32>(min(t, 0.99999994));
    let n = dot(step(cum_a, tt), vec4<f32>(1.0)) + dot(step(cum_b, tt), vec4<f32>(1.0));
    return u32(min(n, 7.0));
}

/// # Colour space and blending
///
/// Writes the authored sRGB colour with straight alpha, exactly as
/// `icon_shader.wgsl` does, into a pipeline with the icon pipeline's
/// `BlendState::ALPHA_BLENDING`: the same compositing as every symbol.
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let radius_px = in.params.x;
    let hole = in.params.y;
    let alpha = in.params.z;

    let r = length(uv);
    let aa = max(fwidth(r), 1e-4);
    // Inside the rim at all (the white stroke and centre included).
    let inside = clamp((1.0 - r) / aa + 0.5, 0.0, 1.0);
    // The coloured ring: from the hole out to one pixel inside the rim,
    // which leaves a 1 px white stroke outside it and a white centre.
    let stroke = 1.0 / max(radius_px, 1.0);
    let ring = clamp((r - hole) / aa + 0.5, 0.0, 1.0) * clamp((1.0 - stroke - r) / aa + 0.5, 0.0, 1.0);

    // Clockwise from 12 o'clock in y-down space.
    let t = fract(atan2(uv.x, -uv.y) / TAU);
    // Half a pixel of arc. `fwidth(t)` jumps to ~1 across the 12 o'clock
    // seam, where `t` wraps; the analytic arc length bounds it there.
    let w = 0.5 * min(fwidth(t), 1.0 / (TAU * max(r * radius_px, 0.5)));
    let c = 0.5 * (frame.colors[sector(fract(t - w), in.cum_a, in.cum_b)]
        + frame.colors[sector(fract(t + w), in.cum_a, in.cum_b)]);

    let rgb = mix(vec3<f32>(1.0), c.rgb, ring);
    let a = mix(1.0, c.a, ring) * inside * alpha;
    return vec4<f32>(rgb, a);
}
