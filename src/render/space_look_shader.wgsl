// The space look: a star + Milky Way sky behind the planet, and clouds,
// sun shading and a thin blue rim over it. Opt-in (`render::space_look`),
// ours -- MapLibre GL JS has no star field, no clouds and no sphere
// shading, and nothing here comes from any Mapbox GL JS tree.
//
// Two full-screen passes share this module and its one uniform:
//
//  * `fs_sky` -- drawn FIRST, before the BSC point stars: the equirectangular
//    Deep Star Maps 2020 texture (NASA/GSFC SVS; Gaia DR2 ESA/Gaia/DPAC),
//    looked up along the pixel's ray in RA/Dec. Additive over the space clear.
//  * `fs_globe` -- drawn after the tiles and before the limb glow: a ray/sphere
//    hit gives the ground point, where the Blue Marble clouds (NASA GSFC,
//    Reto Stoeckli) are sampled and the sun shading is applied. One
//    premultiplied output does both: `rgb + dst * (1 - a)`, with
//    `a = 1 - (1 - cloud) * shade`, darkens the ground by `shade` and lays a
//    lit cloud over it.
//
// Sphere space is `render::sky`'s: the unrotated planet frame
// (`(sin lon cos lat, sin lat, cos lon cos lat)`, y north), the unit sphere at
// the origin. The BSC stars live in that frame too (`space::sky_direction`),
// so RA is the frame's longitude -- plus the sidereal angle, which both the
// point stars and this texture are turned by together.
//
// WebGL2/iOS: one inter-stage vec3, `textureSampleLevel` only (the samples
// sit in non-uniform control flow, and there are no mips), no int/bool
// vector mixing.

struct Look {
    /// VIEW space -> SPHERE space, directions only.
    inverse_view: mat4x4<f32>,
    /// The eye in sphere space.
    camera_sphere: vec3<f32>,
    /// `tan(fov_y / 2)`.
    tan_half_fov_y: f32,
    /// Unit direction toward the sun, sphere space.
    sun_dir: vec3<f32>,
    /// Viewport width / height.
    aspect: f32,
    /// Milky Way gain; 0 draws nothing (and the pass is not submitted).
    milky_way: f32,
    /// Cloud opacity, the zoom fade already applied.
    clouds: f32,
    /// How much of the sun shading applies, the zoom fade already applied.
    shading: f32,
    /// Greenwich sidereal angle, radians: RA = frame longitude + this.
    sidereal: f32,
    /// Rim strength, the zoom fade applied.
    rim: f32,
    /// Night-side floor of the shading.
    night: f32,
    _pad0: f32,
    _pad1: f32,
}

@group(0) @binding(0) var<uniform> look: Look;
@group(0) @binding(1) var star_map: texture_2d<f32>;
@group(0) @binding(2) var cloud_map: texture_2d<f32>;
@group(0) @binding(3) var map_sampler: sampler;

const PI: f32 = 3.14159265;
const TAU: f32 = 6.2831853;
/// How far above the ground the cloud deck is sampled, in planet radii
/// (exaggerated for parallax; real cloud tops are ~0.002).
const CLOUD_ALTITUDE: f32 = 0.006;
/// The rim's colour: a thin, soft sky blue.
const RIM_COLOUR: vec3<f32> = vec3<f32>(0.35, 0.6, 1.0);

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    /// The fragment's ray in SPHERE space, unnormalised (linear in NDC).
    @location(0) ray: vec3<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    let ndc = vec2<f32>(
        f32((index << 1u) & 2u) * 2.0 - 1.0,
        f32(index & 2u) * 2.0 - 1.0,
    );
    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc, 1.0, 1.0);
    let view_ray = vec3<f32>(
        ndc.x * look.tan_half_fov_y * look.aspect,
        ndc.y * look.tan_half_fov_y,
        -1.0,
    );
    out.ray = (look.inverse_view * vec4<f32>(view_ray, 0.0)).xyz;
    return out;
}

/// Distance along unit `ray` from `eye` to the near hit of the sphere of
/// `radius` at the origin, or a negative number for a miss.
fn hit_sphere(eye: vec3<f32>, ray: vec3<f32>, radius: f32) -> f32 {
    let b = dot(eye, ray);
    let c = dot(eye, eye) - radius * radius;
    let disc = b * b - c;
    if (disc < 0.0) {
        return -1.0;
    }
    return -b - sqrt(disc);
}

/// Equirectangular UV of a unit direction whose "longitude" is
/// `atan2(x, z)`: u from -180 at 0 to +180 at 1, v north at 0.
fn lon_lat_uv(d: vec3<f32>) -> vec2<f32> {
    let lon = atan2(d.x, d.z);
    let lat = asin(clamp(d.y, -1.0, 1.0));
    return vec2<f32>(lon / TAU + 0.5, 0.5 - lat / PI);
}

@fragment
fn fs_sky(input: VertexOutput) -> @location(0) vec4<f32> {
    let ray = normalize(input.ray);
    // Below the limb is the planet: nothing to add (the ocean sphere and the
    // tiles cover it, and a zero keeps this pass cheap to blend there).
    if (hit_sphere(look.camera_sphere, ray, 1.0) > 0.0) {
        return vec4<f32>(0.0);
    }
    // Deep Star Maps: centred on 0h, RA increasing to the LEFT.
    let ra = atan2(ray.x, ray.z) + look.sidereal;
    let dec = asin(clamp(ray.y, -1.0, 1.0));
    let uv = vec2<f32>(fract(0.5 - ra / TAU), 0.5 - dec / PI);
    let texel = textureSampleLevel(star_map, map_sampler, uv, 0.0).rgb;
    // The map's diffuse glow is bright next to a planet: a power curve
    // pushes the faint haze back toward black so the sky reads as black with
    // a dusty band, and the gain sets the band's level.
    let lifted = pow(texel, vec3<f32>(1.6)) * look.milky_way;
    // Additive over the space clear: alpha 0 under premultiplied blending.
    return vec4<f32>(lifted, 0.0);
}

@fragment
fn fs_globe(input: VertexOutput) -> @location(0) vec4<f32> {
    let ray = normalize(input.ray);
    let eye = look.camera_sphere;
    let t = hit_sphere(eye, ray, 1.0);
    if (t <= 0.0) {
        return vec4<f32>(0.0);
    }
    let normal = normalize(eye + ray * t);
    let sun = look.sun_dir;
    let n_dot_l = dot(normal, sun);
    // How squarely the eye sees this point: 1 at the disc's centre, 0 at the
    // limb.
    let mu = clamp(-dot(normal, ray), 0.0, 1.0);

    // Sun shading: a broad lit face that rolls softly to the night floor,
    // and a mild limb darkening so the disc reads as a ball.
    let lit = smoothstep(-0.3, 0.5, n_dot_l);
    let limb = mix(0.55, 1.0, pow(mu, 0.45));
    let shade_full = mix(look.night, 1.0, lit) * limb;
    let shade = mix(1.0, shade_full, look.shading);

    // Clouds, sampled on a deck slightly above the ground (parallax toward
    // the limb). Luminance to coverage with a floor, so the thin grey haze
    // of the composite stays clear sky and only the swirls read white.
    let tc = hit_sphere(eye, ray, 1.0 + CLOUD_ALTITUDE);
    let cloud_point = normalize(eye + ray * max(tc, 0.0));
    let grey = textureSampleLevel(cloud_map, map_sampler, lon_lat_uv(cloud_point), 0.0).r;
    let cover = smoothstep(0.12, 0.66, grey) * look.clouds;
    // Clouds are lit by the same sun, a touch brighter than the ground.
    let cloud_light = mix(look.night, 1.0, smoothstep(-0.2, 0.45, n_dot_l)) * mix(0.7, 1.0, mu);
    let cloud_rgb = vec3<f32>(0.97, 0.98, 1.0) * mix(1.0, cloud_light, look.shading);

    // The rim: a thin soft blue hugging the limb on the lit side.
    let rim = pow(1.0 - mu, 3.0) * smoothstep(-0.25, 0.4, n_dot_l) * look.rim;

    let a = 1.0 - (1.0 - cover) * shade;
    return vec4<f32>(cloud_rgb * cover + RIM_COLOUR * rim, a);
}
