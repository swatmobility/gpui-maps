// `ArcBlend::Glow`'s composite: the arcs were summed additively into a
// float target (`render::arcs::ArcPipeline::accumulate`), premultiplied
// colour in `rgb` and coverage in `a`; this draws one full-screen triangle
// that tone-maps the sums onto the map.
//
// Hue-preserving: the sum is scaled by `t(m) / m`, `m` its brightest
// channel and `t(x) = 1 - exp(-k x)`, so a lone arc comes out as it would
// blended "over" (`k E` for small `E`), a bundle gets brighter with density
// and the hue is the arcs' own however dense. Then a share `d` of the
// per-channel curve `t(E)` is mixed in: dense cores drift pale (Kepler's
// pinkish-white London), but white needs every channel fed, which the
// arcs' colours decide, not the density. Coverage takes `t` too, and
// `rgb <= a` keeps the result valid premultiplied "over".

struct GlowParams {
    /// `x`: the exposure `k`. `y`: the desaturation share `d`. `zw`: unused.
    exposure: vec4<f32>,
};

@group(0) @binding(0) var accum: texture_2d<f32>;
@group(0) @binding(1) var<uniform> params: GlowParams;

@vertex
fn vs_glow(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = select(-1.0, 3.0, i == 1u);
    let y = select(-1.0, 3.0, i == 2u);
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_glow(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let e = textureLoad(accum, vec2<i32>(pos.xy), 0);
    let k = params.exposure.x;
    let sum = max(e.rgb, vec3<f32>(0.0));
    let m = max(max(sum.r, sum.g), sum.b);
    var hue = vec3<f32>(0.0);
    if (m > 1e-6) {
        hue = sum * ((1.0 - exp(-k * m)) / m);
    }
    let per_channel = vec3<f32>(1.0) - exp(-k * sum);
    let rgb = mix(hue, per_channel, clamp(params.exposure.y, 0.0, 1.0));
    let a = 1.0 - exp(-k * max(e.a, 0.0));
    return vec4<f32>(min(rgb, vec3<f32>(a)), a);
}
