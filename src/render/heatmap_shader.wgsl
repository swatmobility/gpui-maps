// Heatmaps: `render::heatmap`'s three passes, deck.gl's HeatmapLayer
// (`modules/aggregation-layers/src/heatmap-layer/`, MIT, vis.gl
// contributors) re-expressed in WGSL; see `src/render/PROVENANCE.md`.
//
// 1. `vs_weight` / `fs_weight`: every point is a screen-facing quad of
//    `radius` device px, summed One/One into a half-resolution float
//    target with deck's Gaussian (`weights.wgsl`).
// 2. `vs_max` / `fs_max`: one point per density texel into a 1x1 target
//    with a `Max` blend: the frame's densest texel (deck's max pass).
// 3. `vs_color` / `fs_color`: a full-screen triangle samples the density,
//    divides by that maximum, looks the colour up in the ramp and fades
//    the low end by `threshold` (deck's `triangle-layer`).
//
// - **Filter**: up to four per-point values against four windows, as the
//   point sources' GPU filter; a point outside any enabled window
//   collapses outside the clip volume.
// - **Globe**: the centre is projected flat and on the sphere and blended
//   by the transition, as `arc_shader.wgsl` does; a point the planet hides
//   is dropped. The quad is extruded on screen, so the radius is pixels
//   in both.

struct HeatUniform {
    /// World units RELATIVE TO `origin` -> clip, east-west flip folded in.
    flat_view_proj: mat4x4<f32>,
    /// Unit-sphere position -> clip.
    globe_view_proj: mat4x4<f32>,
    /// `xyz`: the eye in sphere space. `w`: unused.
    eye: vec4<f32>,
    /// `(origin mercator x, origin mercator y, WORLD_SIZE, transition)`.
    origin: vec4<f32>,
    /// `(density target w, density target h, radius in density texels, weight scale)`.
    target_size: vec4<f32>,
    /// `(intensity, threshold, opacity, enabled filter mask)`.
    params: vec4<f32>,
    filter_lo: vec4<f32>,
    filter_hi: vec4<f32>,
    /// `xy`: the set's anchor less `origin`, world units (f64 on the CPU).
    anchor: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: HeatUniform;

struct PointInput {
    @location(0) lon_lat: vec2<f32>,
    @location(1) weight: f32,
    @location(2) filter_values: vec4<f32>,
    /// Mercator less the set's anchor (f64 on the CPU).
    @location(3) merc: vec2<f32>,
};

struct WeightOutput {
    @builtin(position) position: vec4<f32>,
    /// The quad corner, `[-1, 1]` both ways.
    @location(0) corner: vec2<f32>,
    @location(1) weight: f32,
};

const PI: f32 = 3.14159265358979;

fn sphere(lon_lat: vec2<f32>) -> vec3<f32> {
    let lon = lon_lat.x * (PI / 180.0);
    let lat = lon_lat.y * (PI / 180.0);
    return vec3<f32>(sin(lon) * cos(lat), sin(lat), cos(lon) * cos(lat));
}

fn occluded(p: vec3<f32>) -> bool {
    let e = u.eye.xyz;
    let d = p - e;
    let dd = max(dot(d, d), 1e-20);
    let k = clamp(-dot(e, d) / dd, 0.0, 1.0);
    return length(e + d * k) < 0.9999;
}

/// Whether `v` passes window `i` of the filter (a disabled one passes; a
/// NaN never passes an enabled one).
fn passes(i: u32, v: f32) -> bool {
    let mask = u32(u.params.w + 0.5);
    if (((mask >> i) & 1u) == 0u) {
        return true;
    }
    return v >= u.filter_lo[i] && v <= u.filter_hi[i];
}

/// The flat position is the anchor's offset plus the point's, both small:
/// never absolute f32 Mercator, which rounds to metres (pixels at street
/// zoom, more with every zoom level).
fn centre_clip(lon_lat: vec2<f32>, merc: vec2<f32>) -> vec4<f32> {
    let transition = u.origin.w;
    var flat_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var globe_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var hidden = false;
    if (transition < 1.0) {
        var rel = u.anchor.xy + merc * u.origin.z;
        rel.x = rel.x - round(rel.x / u.origin.z) * u.origin.z;
        flat_c = u.flat_view_proj * vec4<f32>(rel, 0.0, 1.0);
    }
    if (transition > 0.0) {
        let g = sphere(lon_lat);
        globe_c = u.globe_view_proj * vec4<f32>(g, 1.0);
        hidden = occluded(g);
    }
    var c = flat_c;
    if (transition >= 1.0) {
        c = globe_c;
    } else if (transition > 0.0) {
        c = mix(flat_c, globe_c, transition);
    }
    if (hidden && transition > 0.5) {
        c = vec4<f32>(0.0, 0.0, 2.0, 1.0);
    }
    return c;
}

@vertex
fn vs_weight(@builtin(vertex_index) vi: u32, p: PointInput) -> WeightOutput {
    var out: WeightOutput;
    let corner = vec2<f32>(select(-1.0, 1.0, (vi & 1u) == 1u), select(-1.0, 1.0, (vi & 2u) == 2u));
    out.corner = corner;
    out.weight = p.weight * u.target_size.w;
    let pass_all = passes(0u, p.filter_values.x) && passes(1u, p.filter_values.y) && passes(2u, p.filter_values.z) && passes(3u, p.filter_values.w);
    let c = centre_clip(p.lon_lat, p.merc);
    // `c.z > c.w`: `centre_clip`'s hidden marker (a point behind the
    // planet). The quad below rewrites z, so it is culled here, or every
    // hidden point lands in one pile at the screen centre -- the frame's
    // densest texel, which then dims the whole field.
    if (!pass_all || c.w <= 0.0 || c.z > c.w || !(p.weight > 0.0)) {
        out.position = vec4<f32>(0.0, 0.0, 2.0, 1.0);
        return out;
    }
    // Device px -> NDC on the density target, times w so the divide keeps it.
    let ndc = corner * u.target_size.z * 2.0 / u.target_size.xy;
    out.position = vec4<f32>(c.xy + ndc * c.w, 0.5 * c.w, c.w);
    return out;
}

@fragment
fn fs_weight(input: WeightOutput) -> @location(0) vec4<f32> {
    // deck's kernel: `d` from 0 at the centre to 0.5 at the radius.
    let d = length(input.corner) * 0.5;
    if (d > 0.5) {
        discard;
    }
    let gaussian = exp(-pow(2.0 * d, 2.0) / 0.05555) / (1.77245385 * 0.166666);
    return vec4<f32>(input.weight * gaussian, 0.0, 0.0, 1.0);
}

// ---------------------------------------------------------------------
// The max pass
// ---------------------------------------------------------------------

@group(0) @binding(1) var density_in: texture_2d<f32>;

struct MaxOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) value: f32,
};

/// One point per `MAX_STRIDE`-th texel each way (`target_size.xy` texels):
/// the densest one lands in the 1x1 target through the `Max` blend.
@vertex
fn vs_max(@builtin(vertex_index) vi: u32) -> MaxOutput {
    var out: MaxOutput;
    let w = u32(u.target_size.x);
    let stride = 2u;
    let cols = (w + stride - 1u) / stride;
    let x = (vi % cols) * stride;
    let y = (vi / cols) * stride;
    out.value = textureLoad(density_in, vec2<i32>(i32(x), i32(y)), 0).r;
    out.position = vec4<f32>(0.0, 0.0, 0.5, 1.0);
    return out;
}

@fragment
fn fs_max(input: MaxOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(input.value, 0.0, 0.0, 1.0);
}

// ---------------------------------------------------------------------
// The colour pass
// ---------------------------------------------------------------------

@group(0) @binding(2) var max_in: texture_2d<f32>;
@group(0) @binding(3) var ramp: texture_2d<f32>;
@group(0) @binding(4) var linear_sampler: sampler;

struct ColorOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_color(@builtin(vertex_index) i: u32) -> ColorOutput {
    var out: ColorOutput;
    let x = select(-1.0, 3.0, i == 1u);
    let y = select(-1.0, 3.0, i == 2u);
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

@fragment
fn fs_color(input: ColorOutput) -> @location(0) vec4<f32> {
    let weight = textureSample(density_in, linear_sampler, input.uv).r;
    let max_value = max(textureLoad(max_in, vec2<i32>(0, 0), 0).r, 1e-6);
    let intensity = max(u.params.x, 1e-6);
    let t = clamp(weight * intensity / max_value, 0.0, 1.0);
    var color = textureSample(ramp, linear_sampler, vec2<f32>(t, 0.5));
    let threshold = max(u.params.y, 1e-4);
    let fade = min(weight * intensity / (max_value * threshold), 1.0);
    if (weight <= 0.0) {
        return vec4<f32>(0.0);
    }
    let a = color.a * fade * u.params.z;
    return vec4<f32>(color.rgb * a, a);
}
