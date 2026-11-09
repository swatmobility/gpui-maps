// 3D models: `render::model`'s pipeline.
//
// One mesh (metres, +x forward, +y left, +z up) drawn once per instance.
// Each instance carries its offset from the layer's anchor in flat world
// units (packed in f64 on the CPU), the anchor's offset from the camera is
// a uniform (f64 on the CPU each frame), so nothing large is ever added in
// f32 and an instance holds still at any zoom. On the globe the instance is
// placed from its lon/lat on the unit sphere, its mesh laid in the local
// east-north-up frame there; the transition blends the two clip positions.
//
// Lighting in east-north-up: a directional sun, a hemisphere ambient, a
// Blinn highlight scaled per material, a rim term. Two-sided (no culling):
// a normal facing away from the eye is flipped.
//
// Parts: a vertex's material word carries a part id in bits 8..15; a part
// 1..=32 is moved by its row of the layer's part table (a 3x4 local
// transform in the mesh's frame) before anything else. Part 0 is rigid.
//
// Near pass: a layer drawn in the first-person near pass has its own flat
// view-projection (same pose, near 2 cm) and lights with a per-vertex view
// direction from the eye (`eye_local.w` 1).
//
// Portability: two vertex buffers, two uniform blocks with dynamically
// indexed vec4 arrays, scalar selects (WebGL2 / GLSL ES 3.00).

struct ModelUniform {
    /// World units RELATIVE TO the camera origin -> clip, the flip folded in.
    flat_view_proj: mat4x4<f32>,
    /// Unit-sphere position -> clip.
    globe_view_proj: mat4x4<f32>,
    /// xyz: the eye in sphere space. w: the transition.
    eye: vec4<f32>,
    /// xy: the anchor's offset from the camera origin, world units.
    anchor: vec4<f32>,
    /// xyz: toward the sun (ENU). w: its intensity.
    sun: vec4<f32>,
    /// rgb: the sun's colour. w: the rim strength.
    sun_color: vec4<f32>,
    /// rgb: the ambient sky.
    ambient: vec4<f32>,
    /// xyz: toward the eye on a flat map (ENU).
    view: vec4<f32>,
    /// The contact shadow's ellipse in mesh metres: xy its centre, zw its
    /// half-extents. Its opacity is `ambient.w` (0: none).
    shadow: vec4<f32>,
    /// xyz: the eye relative to the camera origin, world units. w: 1 to
    /// light with a per-vertex view direction from it (a near pass).
    eye_local: vec4<f32>,
    /// Per material: rgba, then (paint, gloss, emissive, 0).
    materials: array<vec4<f32>, 16>,
};

/// Three rows (x, y, z, translation) per part, parts 1..=32.
struct ModelParts {
    rows: array<vec4<f32>, 96>,
};

@group(0) @binding(0) var<uniform> u: ModelUniform;
@group(0) @binding(1) var<uniform> parts: ModelParts;

struct ModelInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) material: u32,
    /// xyz: world units from the anchor. w: world units per mesh metre.
    @location(3) rel_scale: vec4<f32>,
    /// lon, lat, altitude m, mesh metres per metre.
    @location(4) lla_scale: vec4<f32>,
    /// heading (clockwise from north), pitch, roll; radians.
    @location(5) angles: vec3<f32>,
    @location(6) tint: vec4<f32>,
};

struct ModelOutput {
    @builtin(position) position: vec4<f32>,
    /// rgb the base colour, a the gloss.
    @location(0) base: vec4<f32>,
    /// The normal, ENU.
    @location(1) normal: vec3<f32>,
    /// Toward the eye, ENU.
    @location(2) view: vec3<f32>,
    /// `ModelMaterial::emissive`: how much of the base colour shows
    /// whatever the light (a lamp), 0 to 1.
    @location(3) emissive: f32,
};

const PI: f32 = 3.14159265358979;
const EARTH_RADIUS_M: f32 = 6371008.8;

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

/// Model frame -> east-north-up: roll about +x (left side up), pitch about
/// +y (nose up), then the heading (clockwise from north). `render::model::rotate`.
fn to_enu(p: vec3<f32>, a: vec3<f32>) -> vec3<f32> {
    let ch = cos(a.x);
    let sh = sin(a.x);
    let cp = cos(a.y);
    let sp = sin(a.y);
    let cr = cos(a.z);
    let sr = sin(a.z);
    let y1 = p.y * cr - p.z * sr;
    let z1 = p.y * sr + p.z * cr;
    let x2 = p.x * cp - z1 * sp;
    let z2 = p.x * sp + z1 * cp;
    let forward = vec3<f32>(sh, ch, 0.0);
    let left = vec3<f32>(-ch, sh, 0.0);
    return forward * x2 + left * y1 + vec3<f32>(0.0, 0.0, z2);
}

struct Placed {
    clip: vec4<f32>,
    /// Toward the eye, ENU.
    view: vec3<f32>,
};

/// An ENU offset (mesh metres, rotated) from an instance's spot to clip,
/// flat, globe or blended: `vs_model`'s and `vs_shadow`'s placement.
fn place(enu: vec3<f32>, rel_scale: vec4<f32>, lla_scale: vec4<f32>) -> Placed {
    var out: Placed;
    let transition = u.eye.w;
    var flat_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var globe_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var hidden = false;
    var view = u.view.xyz;
    if (transition < 1.0) {
        let k = rel_scale.w;
        let w = vec3<f32>(
            u.anchor.x + rel_scale.x + enu.x * k,
            u.anchor.y + rel_scale.y - enu.y * k,
            rel_scale.z + enu.z * k,
        );
        flat_c = u.flat_view_proj * vec4<f32>(w, 1.0);
        if (u.eye_local.w > 0.5) {
            let e = u.eye_local.xyz - w;
            view = normalize(vec3<f32>(e.x, -e.y, e.z));
        }
    }
    if (transition > 0.0) {
        let lon = lla_scale.x * (PI / 180.0);
        let lat = lla_scale.y * (PI / 180.0);
        let up = sphere(lla_scale.xy);
        let east = vec3<f32>(cos(lon), 0.0, -sin(lon));
        let north = vec3<f32>(-sin(lat) * sin(lon), cos(lat), -sin(lat) * cos(lon));
        let base = up * (1.0 + lla_scale.z / EARTH_RADIUS_M);
        let g = base + (east * enu.x + north * enu.y + up * enu.z) * (lla_scale.w / EARTH_RADIUS_M);
        globe_c = u.globe_view_proj * vec4<f32>(g, 1.0);
        hidden = occluded(base);
        let v = normalize(u.eye.xyz - g);
        if (transition > 0.5) {
            view = vec3<f32>(dot(v, east), dot(v, north), dot(v, up));
        }
    }
    var clip = flat_c;
    if (transition >= 1.0) {
        clip = globe_c;
    } else if (transition > 0.0) {
        clip = mix(flat_c, globe_c, transition);
    }
    if (hidden && transition > 0.5) {
        clip.z = clip.w * 2.0;
    }
    out.clip = clip;
    out.view = view;
    return out;
}

@vertex
fn vs_model(input: ModelInput) -> ModelOutput {
    var out: ModelOutput;
    var position = input.position;
    var normal = input.normal;
    let part = (input.material >> 8u) & 255u;
    if (part > 0u) {
        let k = (min(part, 32u) - 1u) * 3u;
        let r0 = parts.rows[k];
        let r1 = parts.rows[k + 1u];
        let r2 = parts.rows[k + 2u];
        let p4 = vec4<f32>(position, 1.0);
        position = vec3<f32>(dot(r0, p4), dot(r1, p4), dot(r2, p4));
        normal = vec3<f32>(dot(r0.xyz, normal), dot(r1.xyz, normal), dot(r2.xyz, normal));
    }
    let enu = to_enu(position, input.angles);
    let n = to_enu(normal, input.angles);
    let placed = place(enu, input.rel_scale, input.lla_scale);
    let clip = placed.clip;
    let view = placed.view;

    let slot = min(input.material & 255u, 7u);
    let m = u.materials[slot * 2u];
    let params = u.materials[slot * 2u + 1u];
    var color = m.rgb;
    if (params.x > 0.5 && input.tint.a > 0.0) {
        color = input.tint.rgb;
    }
    out.position = clip;
    out.base = vec4<f32>(color, params.y);
    out.normal = n;
    out.view = view;
    out.emissive = params.z;
    return out;
}

@fragment
fn fs_model(input: ModelOutput) -> @location(0) vec4<f32> {
    let v = normalize(input.view);
    var n = normalize(input.normal);
    if (dot(n, v) < 0.0) {
        n = -n;
    }
    let s = normalize(u.sun.xyz);
    let intensity = u.sun.w;
    let diffuse = max(dot(n, s), 0.0) * intensity;
    let sky = 0.6 + 0.4 * n.z;
    let h = normalize(s + v);
    let spec = input.base.a * pow(max(dot(n, h), 0.0), 40.0) * intensity;
    let rim = u.sun_color.w * pow(1.0 - max(dot(n, v), 0.0), 3.0);
    let lit = input.base.rgb * (u.ambient.rgb * sky + u.sun_color.rgb * diffuse)
        + u.sun_color.rgb * spec
        + (u.ambient.rgb + vec3<f32>(0.15)) * rim;
    let shown = mix(lit, input.base.rgb, clamp(input.emissive, 0.0, 1.0));
    return vec4<f32>(min(shown, vec3<f32>(1.0)), 1.0);
}

// ---- The contact shadow: a soft dark ellipse on the ground under each
// instance, drawn before the meshes (alpha-blended, depth-tested, no depth
// write). Six generated vertices per instance; the instance buffer alone.

struct ShadowInput {
    @builtin(vertex_index) vertex: u32,
    @location(3) rel_scale: vec4<f32>,
    @location(4) lla_scale: vec4<f32>,
    @location(5) angles: vec3<f32>,
};

struct ShadowOutput {
    @builtin(position) position: vec4<f32>,
    /// The quad corner, -1..1 on both axes.
    @location(0) corner: vec2<f32>,
};

/// How far the ellipse lies above the ground, mesh metres: clear of the
/// road's depth, under any tyre.
const SHADOW_LIFT_M: f32 = 0.02;

@vertex
fn vs_shadow(input: ShadowInput) -> ShadowOutput {
    var out: ShadowOutput;
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let c = corners[min(input.vertex, 5u)];
    let local = vec3<f32>(u.shadow.x + c.x * u.shadow.z, u.shadow.y + c.y * u.shadow.w, SHADOW_LIFT_M);
    // Heading only: the shadow lies flat on the road whatever the body's
    // pitch and roll.
    let enu = to_enu(local, vec3<f32>(input.angles.x, 0.0, 0.0));
    out.position = place(enu, input.rel_scale, input.lla_scale).clip;
    out.corner = c;
    return out;
}

@fragment
fn fs_shadow(input: ShadowOutput) -> @location(0) vec4<f32> {
    let d = length(input.corner);
    let a = u.ambient.w * (1.0 - smoothstep(0.35, 1.0, d));
    return vec4<f32>(0.0, 0.0, 0.0, a);
}
