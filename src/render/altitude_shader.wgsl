// The altitude layer's point pipeline (spec 2026-09-29-satellites §2.3-2.5). Positions are in Earth radii in
// the sphere frame; clip z is written, not projected (the far plane cannot clip GEO); the planet hides a
// point by the analytic closest-approach test, never by depth (R1). Scalar u32 flag unpacking only:
// WebGL2 has no select/mix on integer vectors (spec §2.9).
struct U {
    view_proj: mat4x4<f32>, view: mat4x4<f32>,
    m0: vec4<f32>, m1: vec4<f32>, m2: vec4<f32>,
    eye: vec4<f32>,          // xyz eye in sphere units, w = eps (the soft occlusion edge, sphere units)
    params: vec4<f32>,       // x s, y delta_s, z alpha (transition x hold dim), w near
    viewport: vec4<f32>,     // x w px, y h px, z outline px, w halo px
    palette: array<vec4<f32>, 16>,
    base_px: array<vec4<f32>, 4>,
    emphasis_px: vec4<f32>,  // x hovered, y selected, z halo object, w px scale (device pixels per size px)
    glow: vec4<f32>,         // the Glow look: x core whiteness, y reach x radius, z reach + px (device), w gain
    trail: vec4<f32>,        // trails: x length (fraction of the interval, signed), y cap px, z glow px, w segments
    glow_gain: array<vec4<f32>, 4>,   // per paint, the Glow halo's gain
};
@group(0) @binding(0) var<uniform> u: U;

struct PointIn {
    @location(0) p0: vec3<f32>, @location(1) v0: vec3<f32>,
    @location(2) p1: vec3<f32>, @location(3) v1: vec3<f32>,
    @location(4) flags: u32, @location(5) hidden_s: vec2<f32>,
};
struct PointOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv_alpha_size: vec4<f32>,   // xy corner offset in px, z alpha, w signed radius px (negative = halo)
    @location(2) gain: f32,                  // the Glow halo's gain for this paint (unused by fs_point)
};

fn culled() -> PointOut {
    var o: PointOut;
    o.clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);  // outside the clip volume: the rasterizer drops the quad
    o.color = vec4<f32>(0.0);
    o.uv_alpha_size = vec4<f32>(0.0);
    o.gain = 0.0;
    return o;
}

fn corner(vi: u32) -> vec2<f32> {
    // Two triangles, as circles.rs's quad: 0,1,2 and 2,1,3 over (-1,-1),(1,-1),(-1,1),(1,1).
    // A function-scope var, so the dynamic index is into memory (legal everywhere), not into a value.
    var ks = array<u32, 6>(0u, 1u, 2u, 2u, 1u, 3u);
    let k = ks[min(vi, 5u)];
    return vec2<f32>(select(-1.0, 1.0, (k & 1u) == 1u), select(-1.0, 1.0, (k & 2u) == 2u));
}

@vertex
fn vs_point(@builtin(vertex_index) vi: u32, i: PointIn) -> PointOut { return point_vertex(vi, i, false); }

// The Glow look's quad: wide enough for the halo's falloff (glow.y x the radius, plus glow.z px).
@vertex
fn vs_point_glow(@builtin(vertex_index) vi: u32, i: PointIn) -> PointOut { return point_vertex(vi, i, true); }

// The Hermite between the instance's keyframes at s, then M: the sphere point.
fn place(i: PointIn, s: f32) -> vec3<f32> {
    let s2 = s * s; let s3 = s2 * s; let dt = u.params.y;
    let local = i.p0 * (2.0 * s3 - 3.0 * s2 + 1.0) + i.v0 * dt * (s3 - 2.0 * s2 + s)
              + i.p1 * (-2.0 * s3 + 3.0 * s2) + i.v1 * dt * (s3 - s2);
    return mat3x3<f32>(u.m0.xyz, u.m1.xyz, u.m2.xyz) * local;
}

fn point_vertex(vi: u32, i: PointIn, glow: bool) -> PointOut {
    let s = u.params.x;
    if (((i.flags >> 4u) & 1u) == 0u || s < i.hidden_s.x || s > i.hidden_s.y) { return culled(); }
    let p = place(i, s);
    let view = u.view * vec4<f32>(p, 1.0);
    if (view.z > -u.params.w) { return culled(); }                    // behind the eye: never mirrored
    let e = u.eye.xyz; let d = p - e;
    let t = clamp(-dot(e, d) / dot(d, d), 0.0, 1.0);
    let vis = smoothstep(1.0 - u.eye.w, 1.0 + u.eye.w, length(e + d * t));
    if (vis <= 0.0) { return culled(); }
    let paint = i.flags & 15u;
    var size = u.base_px[paint / 4u][paint % 4u];
    let halo = ((i.flags >> 7u) & 1u) == 1u;
    if (halo) { size = max(size, u.emphasis_px.z); }
    if (((i.flags >> 5u) & 1u) == 1u) { size = max(size, u.emphasis_px.x); }
    if (((i.flags >> 6u) & 1u) == 1u) { size = max(size, u.emphasis_px.y); }
    size = size * clamp(sqrt(length(e) / length(d)), 0.7, 1.5) * u.emphasis_px.w;   // R20, then device px
    let r = 0.5 * size;
    // A halo object also carries a dark outer ring, one outline wide, outside the white halo.
    let flat_reach = r + u.viewport.z + select(0.0, u.viewport.w + u.viewport.z, halo) + 1.0;   // + 1 px of antialiasing
    let reach = select(flat_reach, max(flat_reach, r * u.glow.y + u.glow.z), glow);
    let c = corner(vi);
    var clip = u.view_proj * vec4<f32>(p, 1.0);
    // Written, not projected: inside the clip volume whatever the far plane (d + 2R) says (spec §2.3).
    clip.z = 0.5 * clip.w;
    clip = vec4<f32>(clip.xy + c * reach * 2.0 / u.viewport.xy * clip.w, clip.z, clip.w);
    var o: PointOut;
    o.clip = clip;
    o.color = u.palette[paint];
    o.uv_alpha_size = vec4<f32>(c * reach, vis * u.params.z, select(r, -r, halo));
    o.gain = u.glow_gain[paint / 4u][paint % 4u];
    return o;
}

@fragment
fn fs_point(i: PointOut) -> @location(0) vec4<f32> {
    let d = length(i.uv_alpha_size.xy);                    // px from the centre
    let r = abs(i.uv_alpha_size.w);
    let halo = i.uv_alpha_size.w < 0.0;
    let outline_end = r + u.viewport.z;
    let halo_end = outline_end + select(0.0, u.viewport.w, halo);
    let fill_a = clamp(r + 0.5 - d, 0.0, 1.0);
    let ring_a = clamp(outline_end + 0.5 - d, 0.0, 1.0) - fill_a;
    let halo_a = clamp(halo_end + 0.5 - d, 0.0, 1.0) - fill_a - ring_a;
    // The white halo vanishes on light ground (Positron land is #f8f4f0): a second outline outside it keeps
    // the halo's edge readable there, and is lost only on dark ground, where the white itself reads.
    let dark_end = halo_end + select(0.0, u.viewport.z, halo);
    let dark_a = clamp(dark_end + 0.5 - d, 0.0, 1.0) - fill_a - ring_a - halo_a;
    let outline = u.palette[15u];                         // AltitudeStyle::outline; spec §1.2: 1 px at 60% black
    let white = vec4<f32>(1.0, 1.0, 1.0, 1.0);
    let rgb = i.color.rgb * i.color.a * fill_a + outline.rgb * outline.a * (ring_a + dark_a) + white.rgb * halo_a;
    let a = i.color.a * fill_a + outline.a * (ring_a + dark_a) + halo_a;
    return vec4<f32>(rgb, a) * i.uv_alpha_size.z;            // premultiplied (R4)
}

// The Glow look (AltitudeLook::Glow): a hot, near-white core (glow.x of the way to white at the centre)
// easing out to the paint colour at the rim, inside a halo of two Gaussians -- a tight bright one and a wide
// faint one -- out to the quad's edge, tapered to exactly 0 there. Premultiplied. The main draw blends it by
// SCREEN (src + dst (1 - src)): an add that saturates softly, so a dense shell blooms brighter where halos
// overlap but approaches white only asymptotically, channel by channel -- a cyan with little red stays cyan.
// The emphasis draw blends it "over", so the ISS keeps its white and dark rings (the dark one is still
// needed over light land, which a screen blend cannot darken).
fn glow_falloff(x: f32) -> f32 {
    return (0.55 * exp(-18.0 * x * x) + 0.45 * exp(-4.5 * x * x)) * (1.0 - smoothstep(0.75, 1.0, x));
}

@fragment
fn fs_point_glow(i: PointOut) -> @location(0) vec4<f32> {
    let d = length(i.uv_alpha_size.xy);
    let r = abs(i.uv_alpha_size.w);
    let halo = i.uv_alpha_size.w < 0.0;
    let reach = max(r * u.glow.y + u.glow.z, r + 1.0);
    let core_a = clamp(r + 0.5 - d, 0.0, 1.0);
    let hot = 1.0 - smoothstep(0.0, max(r, 0.5), d);
    // A dense (low-gain) paint also gets a less white core: its cores overlap too.
    let core_rgb = mix(i.color.rgb, vec3<f32>(1.0, 1.0, 1.0), u.glow.x * clamp(0.4 + i.gain, 0.0, 1.0) * hot);
    let g = clamp(u.glow.w * i.gain * glow_falloff(d / reach) * i.color.a, 0.0, 1.0) * (1.0 - core_a);
    // A dense paint's cores are dimmer too, most of all on a small (crowded) globe, where they overlap.
    let core_k = clamp(0.35 + i.gain, 0.0, 1.0);
    var rgb = core_rgb * core_a * i.color.a * core_k + i.color.rgb * g;
    var a = core_a * i.color.a * core_k + g;
    if (halo) {
        // The flat look's rings, over the glow: dark outline, white halo, dark outer ring.
        let outline_end = r + u.viewport.z;
        let halo_end = outline_end + u.viewport.w;
        let dark_end = halo_end + u.viewport.z;
        let ring_a = clamp(outline_end + 0.5 - d, 0.0, 1.0) - core_a;
        let halo_a = clamp(halo_end + 0.5 - d, 0.0, 1.0) - core_a - ring_a;
        let dark_a = clamp(dark_end + 0.5 - d, 0.0, 1.0) - core_a - ring_a - halo_a;
        let outline = u.palette[15u];
        let ring_rgb = outline.rgb * outline.a * (ring_a + dark_a) + vec3<f32>(halo_a);
        let ring_alpha = outline.a * (ring_a + dark_a) + halo_a;
        rgb = ring_rgb + rgb * (1.0 - ring_alpha);
        a = ring_alpha + a * (1.0 - ring_alpha);
    }
    return vec4<f32>(rgb, clamp(a, 0.0, 1.0)) * i.uv_alpha_size.z;
}

// The line pipeline (spec §2.9): one screen-space quad per segment instance, 6 vertices. The layer's
// width rides in viewport.z (device px), so the stage needs two inter-stage slots. Near clip in view space
// (render::altitude::clip_segment_to_near), then the planet hides the line PER FRAGMENT by the same
// closest-approach test the points use per instance. Selects are on float vectors with a scalar bool
// condition only: legal in WebGL2 (spec §2.9).
struct LineIn { @location(0) a: vec3<f32>, @location(1) b: vec3<f32>, @location(2) color: vec4<f32> };
struct LineOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) sphere_pos_edge: vec4<f32>,   // xyz the sphere point, w px across the quad (-half..+half)
};

@vertex
fn vs_line(@builtin(vertex_index) vi: u32, i: LineIn) -> LineOut { return line_vertex(vi, i, false); }

// The Glow look's line: the quad widened by glow.z px either side for the falloff.
@vertex
fn vs_line_glow(@builtin(vertex_index) vi: u32, i: LineIn) -> LineOut { return line_vertex(vi, i, true); }

fn line_vertex(vi: u32, i: LineIn, glow: bool) -> LineOut {
    let m = mat3x3<f32>(u.m0.xyz, u.m1.xyz, u.m2.xyz);
    let a0 = m * i.a; let b0 = m * i.b;
    let va = u.view * vec4<f32>(a0, 1.0); let vb = u.view * vec4<f32>(b0, 1.0);
    let near = u.params.w;
    let fa = va.z <= -near; let fb = vb.z <= -near;
    if (!fa && !fb) {
        var o: LineOut;
        o.clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
        o.color = vec4<f32>(0.0);
        o.sphere_pos_edge = vec4<f32>(0.0);
        return o;
    }
    // Near clip in view space, carried to the sphere frame by the same t: the view is affine.
    let t = (-near - va.z) / (vb.z - va.z);
    let pa = select(mix(a0, b0, t), a0, fa);
    let pb = select(mix(a0, b0, t), b0, fb);
    let ca = u.view_proj * vec4<f32>(pa, 1.0); let cb = u.view_proj * vec4<f32>(pb, 1.0);
    let sa = ca.xy / ca.w * u.viewport.xy * 0.5; let sb = cb.xy / cb.w * u.viewport.xy * 0.5;
    let dir = select(vec2<f32>(1.0, 0.0), normalize(sb - sa), length(sb - sa) > 1e-4);
    let normal = vec2<f32>(-dir.y, dir.x);
    let c = corner(vi);                                   // x: which end (-1 a, +1 b), y: which side (-1, +1)
    let half = 0.5 * u.viewport.z + 1.0 + select(0.0, u.glow.z, glow);   // the width, 1 px of feather[, the glow]
    let at_b = c.x > 0.0;
    var clip = select(ca, cb, at_b);
    clip.z = 0.5 * clip.w;                                // written, not projected (as vs_point)
    clip = vec4<f32>(clip.xy + normal * c.y * half * 2.0 / u.viewport.xy * clip.w, clip.z, clip.w);
    var o: LineOut;
    o.clip = clip;
    o.color = i.color;
    o.sphere_pos_edge = vec4<f32>(select(pa, pb, at_b), c.y * half);
    return o;
}

@fragment
fn fs_line(i: LineOut) -> @location(0) vec4<f32> {
    let e = u.eye.xyz; let d = i.sphere_pos_edge.xyz - e;
    let t = clamp(-dot(e, d) / dot(d, d), 0.0, 1.0);
    let vis = smoothstep(1.0 - u.eye.w, 1.0 + u.eye.w, length(e + d * t));   // per fragment (spec §2.4)
    let cover = clamp(0.5 * u.viewport.z + 0.5 - abs(i.sphere_pos_edge.w), 0.0, 1.0);   // 1 px feather
    let a = i.color.a * vis * cover * u.params.z;
    return vec4<f32>(i.color.rgb * a, a);                  // premultiplied (R4)
}

// The Glow look's line: a hot core (glow.x of the way to white) the layer's width wide, inside the point
// halo's falloff out to glow.z px either side. Screen-blended, as the points' main draw; hidden by the
// planet per fragment exactly as fs_line.
@fragment
fn fs_line_glow(i: LineOut) -> @location(0) vec4<f32> {
    let e = u.eye.xyz; let d = i.sphere_pos_edge.xyz - e;
    let t = clamp(-dot(e, d) / dot(d, d), 0.0, 1.0);
    let vis = smoothstep(1.0 - u.eye.w, 1.0 + u.eye.w, length(e + d * t));
    let x = abs(i.sphere_pos_edge.w);
    let w2 = 0.5 * u.viewport.z;
    let core = clamp(w2 + 0.5 - x, 0.0, 1.0);
    let g = clamp(u.glow.w * glow_falloff(max(x - w2, 0.0) / max(u.glow.z, 1.0)), 0.0, 1.0) * (1.0 - core);
    let k = i.color.a * vis * u.params.z;
    let rgb = mix(i.color.rgb, vec3<f32>(1.0, 1.0, 1.0), u.glow.x) * core + i.color.rgb * g;
    return vec4<f32>(rgb * k, clamp(core + g, 0.0, 1.0) * k);
}

// Motion trails (TrailStyle): `trail.w` quads per instance, 6 vertices each, sampled from the SAME Hermite as
// the dot at s - trail.x * k / n, so a curved arc stays curved. The whole trail is first held to trail.y px on
// screen (head to full tail, projected), then split. An object that launches or decays inside the interval
// stops its trail at that edge; one that exists throughout may be extrapolated (at most half an interval).
// Each quad carries its ends' sphere points for the per-fragment planet test, as the lines do.
struct TrailOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) sphere_pos_edge: vec4<f32>,   // xyz the sphere point, w px across the quad (-half..+half)
    @location(2) along: vec4<f32>,             // x 0 head .. 1 tail, y core half-width px at the head, z alpha, w glow gain
};

fn trail_culled() -> TrailOut {
    var o: TrailOut;
    o.clip = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    o.color = vec4<f32>(0.0);
    o.sphere_pos_edge = vec4<f32>(0.0);
    o.along = vec4<f32>(0.0);
    return o;
}

fn to_screen(p: vec3<f32>) -> vec2<f32> {
    let c = u.view_proj * vec4<f32>(p, 1.0);
    return c.xy / c.w * u.viewport.xy * 0.5;
}

@vertex
fn vs_trail(@builtin(vertex_index) vi: u32, i: PointIn) -> TrailOut {
    let s = u.params.x;
    if (((i.flags >> 4u) & 1u) == 0u || s < i.hidden_s.x || s > i.hidden_s.y) { return trail_culled(); }
    let n = max(u.trail.w, 1.0);
    let seg = f32(vi / 6u);
    let lo = select(-1.0, i.hidden_s.x, i.hidden_s.x > 0.0);
    let hi = select(2.0, i.hidden_s.y, i.hidden_s.y < 1.0);
    let head = place(i, s);
    // The on-screen cap: the whole trail's chord, head to tail.
    let full = to_screen(place(i, clamp(s - u.trail.x, lo, hi))) - to_screen(head);
    let frac = u.trail.x * min(1.0, u.trail.y / max(length(full), 1e-3));
    let sa = clamp(s - frac * seg / n, lo, hi);
    let sb = clamp(s - frac * (seg + 1.0) / n, lo, hi);
    let pa = place(i, sa); let pb = place(i, sb);
    let near = u.params.w;
    if ((u.view * vec4<f32>(pa, 1.0)).z > -near || (u.view * vec4<f32>(pb, 1.0)).z > -near) { return trail_culled(); }
    let e = u.eye.xyz;
    let paint = i.flags & 15u;
    var size = u.base_px[paint / 4u][paint % 4u];
    size = size * clamp(sqrt(length(e) / length(head - e)), 0.7, 1.5) * u.emphasis_px.w;
    let core = 0.4 * size;                                 // the head's core half-width: a little under the dot's
    let c = corner(vi % 6u);                                // x: which end (-1 a, +1 b), y: which side
    let at_b = c.x > 0.0;
    let ca = u.view_proj * vec4<f32>(pa, 1.0); let cb = u.view_proj * vec4<f32>(pb, 1.0);
    let sa2 = ca.xy / ca.w * u.viewport.xy * 0.5; let sb2 = cb.xy / cb.w * u.viewport.xy * 0.5;
    let dir = select(vec2<f32>(1.0, 0.0), normalize(sb2 - sa2), length(sb2 - sa2) > 1e-4);
    let normal = vec2<f32>(-dir.y, dir.x);
    let half = core + u.trail.z + 1.0;
    var clip = select(ca, cb, at_b);
    clip.z = 0.5 * clip.w;
    // Butt ends, pushed half a pixel out along the trail so two quads meet without a gap.
    let along_px = select(-0.5, 0.5, at_b);
    clip = vec4<f32>(clip.xy + (normal * c.y * half + dir * along_px) * 2.0 / u.viewport.xy * clip.w, clip.z, clip.w);
    var o: TrailOut;
    o.clip = clip;
    o.color = u.palette[paint];
    o.sphere_pos_edge = vec4<f32>(select(pa, pb, at_b), c.y * half);
    o.along = vec4<f32>(select(seg, seg + 1.0, at_b) / n, core, u.params.z, u.glow_gain[paint / 4u][paint % 4u]);
    return o;
}

@fragment
fn fs_trail(i: TrailOut) -> @location(0) vec4<f32> {
    let e = u.eye.xyz; let d = i.sphere_pos_edge.xyz - e;
    let t = clamp(-dot(e, d) / dot(d, d), 0.0, 1.0);
    let vis = smoothstep(1.0 - u.eye.w, 1.0 + u.eye.w, length(e + d * t));
    let a = clamp(i.along.x, 0.0, 1.0);
    let fade = (1.0 - a) * (1.0 - a);
    let x = abs(i.sphere_pos_edge.w);
    let w2 = i.along.y * (1.0 - 0.7 * a);                    // tapers toward the tail
    let core = clamp(w2 + 0.5 - x, 0.0, 1.0);
    let g = clamp(0.6 * i.along.w * glow_falloff(max(x - w2, 0.0) / max(u.trail.z, 1.0)), 0.0, 1.0) * (1.0 - core);
    let k = fade * vis * i.along.z * i.color.a;
    let body = core * 0.85 * clamp(0.4 + 0.6 * i.along.w, 0.0, 1.0);   // a dense paint's trails are dimmer
    let rgb = mix(i.color.rgb, vec3<f32>(1.0, 1.0, 1.0), 0.35 * (1.0 - a)) * body + i.color.rgb * g;
    return vec4<f32>(rgb * k, clamp(body + g, 0.0, 1.0) * k);
}
