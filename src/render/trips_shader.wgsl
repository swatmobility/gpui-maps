// Animated trips: `render::trips`' pipeline.
//
// deck.gl's TripsLayer (`modules/geo-layers/src/trips-layer/trips-layer.ts`,
// MIT, vis.gl contributors) re-expressed in WGSL; see
// `src/render/PROVENANCE.md`. Every vertex of every trip carries its own
// timestamp; the scene clock (`clock.x`) and the trail length (`clock.y`)
// are uniforms, so moving the clock is one uniform write. A fragment later
// than the clock is discarded, and one older than the trail is discarded
// too, the rest fading linearly toward the tail -- deck's
// `vTime > currentTime || (fadeTrail && vTime < currentTime - trailLength)`.
//
// - **Segments**: one instance per pair of consecutive vertices. The vertex
//   buffer is bound twice, the second binding one vertex later, so a
//   segment reads its start and its end without duplicating either (deck's
//   PathLayer `instanceStartPositions` / `instanceEndPositions`). A pair
//   that straddles two trips (different `trip` ids) draws nothing.
// - **Cull**: a segment wholly after the clock, or wholly before the trail
//   (when it fades), collapses in the vertex stage: the fragments are the
//   cost, and most of a long playback is outside the trail.
// - **Shape**: a screen-space quad, `width_px` wide, half a device pixel of
//   feather each side; under a pixel the alpha carries the width (as
//   `arc_shader.wgsl`).
// - **Flat**: Web Mercator, the end moved onto the start's side of the
//   antimeridian (the shortest way) and the pair wrapped together to the
//   world copy nearest the camera. **Globe**: the unit sphere, lifted by
//   altitude, a point the planet hides pushed past the far plane (as arcs).
// - **Heads** (`vs_head`/`fs_head`, optional): the segment holding the
//   clock draws a screen-space quad at the clock's point, turned along the
//   segment on screen; a standing segment (a dwell repeats a point) takes
//   the way in, from the vertex before, or else the way out, from the one
//   after (two more bindings of the same buffer). The shape is a signed
//   distance: a dot, or a navigation arrow, the trip's colour inside an
//   outline. Every other segment collapses in the vertex stage.
// - **Portability**: ten instance attributes at most, two inter-stage
//   vectors, one uniform block, scalar selects only (WebGL2 / GLSL ES 3.00).

struct TripUniform {
    /// World units RELATIVE TO `origin` -> clip, east-west flip folded in.
    flat_view_proj: mat4x4<f32>,
    /// Unit-sphere position -> clip.
    globe_view_proj: mat4x4<f32>,
    /// `xyz`: the eye in sphere space. `w`: unused.
    eye: vec4<f32>,
    /// `(origin mercator x, origin mercator y, WORLD_SIZE, transition)`.
    origin: vec4<f32>,
    /// `(width, height)` of the target in device px, device px per logical
    /// px, the layer opacity.
    viewport: vec4<f32>,
    /// `(head time, trail length, fade on, clock on)`, times relative to the
    /// set's base.
    clock: vec4<f32>,
    /// `(width px, altitude scale, 0, 0)`.
    params: vec4<f32>,
    /// `(size px, shape: 0 none 1 dot 2 arrow 3 teardrop, outline px, 0)`.
    head: vec4<f32>,
    /// The head's outline, straight alpha.
    head_outline: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: TripUniform;

struct SegmentInput {
    /// lon, lat degrees, altitude metres, time (relative).
    @location(0) a: vec4<f32>,
    @location(1) a_color: vec4<f32>,
    @location(2) a_trip: u32,
    @location(3) b: vec4<f32>,
    @location(4) b_color: vec4<f32>,
    @location(5) b_trip: u32,
};

struct TripOutput {
    @builtin(position) position: vec4<f32>,
    /// Premultiplied colour, coverage and opacity folded in.
    @location(0) color: vec4<f32>,
    /// `x`: signed device px from the centreline. `y`: the core half width.
    /// `z`: the time here (relative).
    @location(1) edge: vec3<f32>,
};

const PI: f32 = 3.14159265358979;
const MAX_LAT: f32 = 85.051129;
/// Metres around the equator, and the mean radius: altitude in world units.
const EARTH_CIRCUMFERENCE_M: f32 = 40075016.686;
const EARTH_RADIUS_M: f32 = 6371008.8;

fn mercator(lon_lat: vec2<f32>) -> vec2<f32> {
    let lat = clamp(lon_lat.y, -MAX_LAT, MAX_LAT) * (PI / 180.0);
    return vec2<f32>((lon_lat.x + 180.0) / 360.0, (1.0 - log(tan(PI * 0.25 + lat * 0.5)) / PI) * 0.5);
}

fn sphere(lon_lat: vec2<f32>) -> vec3<f32> {
    let lon = lon_lat.x * (PI / 180.0);
    let lat = lon_lat.y * (PI / 180.0);
    return vec3<f32>(sin(lon) * cos(lat), sin(lat), cos(lon) * cos(lat));
}

/// Altitude in flat world units at latitude `lat_deg` (a Mercator world
/// unit is `circumference * cos(lat) / WORLD_SIZE` metres).
fn flat_height(alt_m: f32, lat_deg: f32) -> f32 {
    let c = max(cos(clamp(lat_deg, -MAX_LAT, MAX_LAT) * (PI / 180.0)), 1e-3);
    return alt_m * u.params.y * u.origin.z / (EARTH_CIRCUMFERENCE_M * c);
}

fn flat_clip(m: vec2<f32>, z: f32) -> vec4<f32> {
    let rel = (m - u.origin.xy) * u.origin.z;
    return u.flat_view_proj * vec4<f32>(rel, z, 1.0);
}

/// `flat_clip` for a point given as a vertex `m` plus a small step `d`
/// (Mercator): the step is added after the subtraction, where f32 still
/// resolves it. Added before, a point moving along a segment would land on
/// the 2.4 m grid of absolute f32 Mercator and step between its cells.
fn flat_clip_step(m: vec2<f32>, d: vec2<f32>, z: f32) -> vec4<f32> {
    let rel = (m - u.origin.xy) * u.origin.z + d * u.origin.z;
    return u.flat_view_proj * vec4<f32>(rel, z, 1.0);
}

fn occluded(p: vec3<f32>) -> bool {
    let e = u.eye.xyz;
    let d = p - e;
    let dd = max(dot(d, d), 1e-20);
    let k = clamp(-dot(e, d) / dd, 0.0, 1.0);
    return length(e + d * k) < 0.9999;
}

/// One end's clip position: flat (`m`, Mercator, already wrapped) and globe
/// blended by the transition, as `arc_shader.wgsl`'s `arc_clip`.
fn end_clip(m: vec2<f32>, p: vec4<f32>) -> vec4<f32> {
    let transition = u.origin.w;
    var flat_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var globe_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var hidden = false;
    if (transition < 1.0) {
        flat_c = flat_clip(m, flat_height(p.z, p.y));
    }
    if (transition > 0.0) {
        let g = sphere(p.xy) * (1.0 + p.z * u.params.y / EARTH_RADIUS_M);
        globe_c = u.globe_view_proj * vec4<f32>(g, 1.0);
        hidden = occluded(g);
    }
    var clip = flat_c;
    if (transition >= 1.0) {
        clip = globe_c;
    } else if (transition > 0.0) {
        clip = mix(flat_c, globe_c, transition);
    }
    clip.z = min(clip.z, clip.w * 0.9999);
    if (hidden && transition > 0.5) {
        clip.z = clip.w * 2.0;
    }
    return clip;
}

const OFF: vec4<f32> = vec4<f32>(2.0, 2.0, 2.0, 1.0);

@vertex
fn vs_trip(input: SegmentInput, @builtin(vertex_index) vid: u32) -> TripOutput {
    var out: TripOutput;
    out.position = OFF;
    out.color = vec4<f32>(0.0);
    out.edge = vec3<f32>(0.0);
    // Two trips meet here: not a segment.
    if (input.a_trip != input.b_trip) {
        return out;
    }
    let head = u.clock.x;
    let trail = u.clock.y;
    if (u.clock.w > 0.5) {
        // Wholly after the clock, or wholly behind a fading trail.
        if (input.a.w > head) {
            return out;
        }
        if (u.clock.z > 0.5 && input.b.w < head - trail) {
            return out;
        }
    }
    let at_b = (vid >> 1u) == 1u;
    let side = select(-1.0, 1.0, (vid & 1u) == 1u);

    // The shortest way across the antimeridian, then both ends wrapped by
    // the start's shift, so the segment never spans the world.
    var b_ll = input.b.xy;
    b_ll.x = b_ll.x - 360.0 * round((b_ll.x - input.a.x) / 360.0);
    var ma = mercator(input.a.xy);
    var mb = mercator(b_ll);
    let shift = round(ma.x - u.origin.x);
    ma.x = ma.x - shift;
    mb.x = mb.x - shift;

    let ca = end_clip(ma, input.a);
    let cb = end_clip(mb, vec4<f32>(b_ll, input.b.zw));

    let half_vp = u.viewport.xy * 0.5;
    let wa = max(abs(ca.w), 1e-6);
    let wb = max(abs(cb.w), 1e-6);
    var dir = (cb.xy / wb - ca.xy / wa) * half_vp;
    let len = length(dir);
    if (len > 1e-6) {
        dir = dir / len;
    } else {
        dir = vec2<f32>(1.0, 0.0);
    }
    let normal = vec2<f32>(-dir.y, dir.x);
    let width = max(u.params.x, 0.0) * u.viewport.z;
    var core = max(width, 1.0) * 0.5;
    var outer = core + 0.5;
    if (width < 1.0) {
        core = 1.0;
        outer = 0.5;
    }
    var curr = ca;
    var color = input.a_color;
    var t = input.a.w;
    if (at_b) {
        curr = cb;
        color = input.b_color;
        t = input.b.w;
    }
    curr = vec4<f32>(curr.xy + normal * side * outer / half_vp * curr.w, curr.zw);
    let alpha = color.a * u.viewport.w * min(width, 1.0);
    out.position = curr;
    out.color = vec4<f32>(color.rgb * alpha, alpha);
    out.edge = vec3<f32>(side * outer, core, t);
    return out;
}

@fragment
fn fs_trip(input: TripOutput) -> @location(0) vec4<f32> {
    let t = input.edge.z;
    var fade = 1.0;
    if (u.clock.w > 0.5) {
        let head = u.clock.x;
        let trail = max(u.clock.y, 1e-6);
        if (t > head || (u.clock.z > 0.5 && t < head - trail)) {
            discard;
        }
        if (u.clock.z > 0.5) {
            fade = 1.0 - (head - t) / trail;
        }
    }
    let coverage = clamp(input.edge.y + 0.5 - abs(input.edge.x), 0.0, 1.0);
    return input.color * (coverage * fade);
}

struct HeadInput {
    @location(0) a: vec4<f32>,
    @location(1) a_color: vec4<f32>,
    @location(2) a_trip: u32,
    @location(3) b: vec4<f32>,
    @location(4) b_color: vec4<f32>,
    @location(5) b_trip: u32,
    /// The vertex before `a`, and the one after `b`.
    @location(6) p: vec4<f32>,
    @location(7) p_trip: u32,
    @location(8) n: vec4<f32>,
    @location(9) n_trip: u32,
};

struct HeadOutput {
    @builtin(position) position: vec4<f32>,
    /// The trip's colour, straight alpha.
    @location(0) fill: vec4<f32>,
    /// Device px from the marker's centre: `x` ahead, `y` across.
    @location(1) local: vec2<f32>,
};

/// A point's Mercator position, the antimeridian taken the short way from
/// `ref_lon` and the result shifted by `shift` world copies.
fn wrapped(ll: vec2<f32>, ref_lon: f32, shift: f32) -> vec2<f32> {
    var l = ll;
    l.x = l.x - 360.0 * round((l.x - ref_lon) / 360.0);
    var m = mercator(l);
    m.x = m.x - shift;
    return m;
}

/// Device px on the target, from a clip position.
fn device_px(c: vec4<f32>) -> vec2<f32> {
    return c.xy / max(abs(c.w), 1e-6) * u.viewport.xy * 0.5;
}

@vertex
fn vs_head(input: HeadInput, @builtin(vertex_index) vid: u32) -> HeadOutput {
    var out: HeadOutput;
    out.position = OFF;
    out.fill = vec4<f32>(0.0);
    out.local = vec2<f32>(0.0);
    let head = u.clock.x;
    if (u.clock.w < 0.5 || u.head.y < 0.5 || u.head.x <= 0.0 || input.a_trip != input.b_trip) {
        return out;
    }
    // The one segment of this trip that holds the clock.
    if (!(input.a.w <= head && head < input.b.w)) {
        return out;
    }
    let f = (head - input.a.w) / max(input.b.w - input.a.w, 1e-6);
    let ref_lon = input.a.x;
    let shift = round(mercator(input.a.xy).x - u.origin.x);
    let ma = wrapped(input.a.xy, ref_lon, shift);
    let mb = wrapped(input.b.xy, ref_lon, shift);
    var b_ll = input.b.xy;
    b_ll.x = b_ll.x - 360.0 * round((b_ll.x - ref_lon) / 360.0);
    let ll = mix(input.a.xy, b_ll, f);
    let alt = mix(input.a.z, input.b.z, f);
    var clip = end_clip(mix(ma, mb, f), vec4<f32>(ll, alt, head));
    if (u.origin.w <= 0.0) {
        // Flat: the start plus f of the step, resolved relative to the
        // origin (`flat_clip_step`), so a moving head glides.
        clip = flat_clip_step(ma, (mb - ma) * f, flat_height(alt, ll.y));
        clip.z = min(clip.z, clip.w * 0.9999);
    }
    if (clip.w <= 0.0 || clip.z > clip.w) {
        return out;
    }
    // Its way on screen: along the segment, or the way in, or the way out.
    let ca = end_clip(ma, input.a);
    let cb = end_clip(mb, vec4<f32>(b_ll, input.b.zw));
    var dir = device_px(cb) - device_px(ca);
    if (length(dir) < 1e-3 && input.p_trip == input.a_trip) {
        let cp = end_clip(wrapped(input.p.xy, ref_lon, shift), input.p);
        dir = device_px(ca) - device_px(cp);
    }
    if (length(dir) < 1e-3 && input.n_trip == input.b_trip) {
        let cn = end_clip(wrapped(input.n.xy, ref_lon, shift), input.n);
        dir = device_px(cn) - device_px(cb);
    }
    if (length(dir) < 1e-3) {
        dir = vec2<f32>(0.0, 1.0);
    }
    dir = normalize(dir);
    let normal = vec2<f32>(-dir.y, dir.x);
    // The quad: half the size, plus a pixel of feather.
    var extent = u.head.x * 0.5 * u.viewport.z + 1.0;
    if (u.head.y > 2.5) {
        extent = u.head.x * 0.5 * u.viewport.z * TEAR_APEX + 1.0;
    }
    let corner = vec2<f32>(select(-1.0, 1.0, (vid & 1u) == 1u), select(-1.0, 1.0, (vid >> 1u) == 1u));
    let local = corner * extent;
    let offset = dir * local.x + normal * local.y;
    out.position = vec4<f32>(clip.xy + offset / (u.viewport.xy * 0.5) * clip.w, clip.zw);
    out.fill = input.a_color;
    out.local = local;
    return out;
}

/// The teardrop's apex, in radii ahead of its centre (`size` is the
/// circle's diameter).
const TEAR_APEX: f32 = 2.6;

/// Signed distance to a closed four-point polygon, negative inside (any
/// winding, concave allowed).
fn sd_quad(p: vec2<f32>, v0: vec2<f32>, v1: vec2<f32>, v2: vec2<f32>, v3: vec2<f32>) -> f32 {
    var v = array<vec2<f32>, 4>(v0, v1, v2, v3);
    var d = dot(p - v0, p - v0);
    var s = 1.0;
    var j = 3u;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let vi = v[i];
        let vj = v[j];
        let e = vj - vi;
        let w = p - vi;
        let b = w - e * clamp(dot(w, e) / max(dot(e, e), 1e-12), 0.0, 1.0);
        d = min(d, dot(b, b));
        let c0 = p.y >= vi.y;
        let c1 = p.y < vj.y;
        let c2 = e.x * w.y > e.y * w.x;
        if ((c0 && c1 && c2) || (!c0 && !c1 && !c2)) {
            s = -s;
        }
        j = i;
    }
    return s * sqrt(d);
}

@fragment
fn fs_head(input: HeadOutput) -> @location(0) vec4<f32> {
    let r = u.head.x * 0.5 * u.viewport.z;
    let p = input.local;
    var d = length(p) - r;
    if (u.head.y > 2.5) {
        // A teardrop: the circle of radius `r` round the trip's point and
        // an apex ahead of it, joined by tangents (iq's uneven capsule,
        // the far radius 0).
        let rr = r;
        let h = rr * TEAR_APEX;
        let q = vec2<f32>(abs(p.y), p.x);
        let b = rr / h;
        let a = sqrt(1.0 - b * b);
        let k = dot(q, vec2<f32>(-b, a));
        if (k < 0.0) {
            d = length(q) - rr;
        } else if (k > a * h) {
            d = length(q - vec2<f32>(0.0, h));
        } else {
            d = dot(q, vec2<f32>(a, b)) - rr;
        }
    } else if (u.head.y > 1.5) {
        // Tip ahead, two wings behind, a notch between them.
        d = sd_quad(p, vec2<f32>(r, 0.0), vec2<f32>(-0.8 * r, 0.75 * r), vec2<f32>(-0.4 * r, 0.0), vec2<f32>(-0.8 * r, -0.75 * r));
    }
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    if (coverage <= 0.0) {
        discard;
    }
    let outline = u.head.z * u.viewport.z;
    let inside = clamp(0.5 - (d + outline), 0.0, 1.0);
    let rgb = mix(u.head_outline.rgb, input.fill.rgb, inside);
    let a = mix(u.head_outline.a, input.fill.a, inside) * coverage * u.viewport.w;
    return vec4<f32>(rgb * a, a);
}
