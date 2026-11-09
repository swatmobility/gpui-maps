// Origin-destination arcs: `render::arcs`' pipeline.
//
// deck.gl's ArcLayer (`modules/layers/src/arc-layer/arc-layer-vertex.glsl.ts`,
// MIT, vis.gl contributors), re-expressed in WGSL; see
// `src/render/PROVENANCE.md` for what differs and why. One instance per arc,
// a triangle strip of `2 * (segments + 1)` vertices generated here from
// `@builtin(vertex_index)`: nothing is tessellated on the CPU.
//
// - **Shape**: deck's `paraboloid` with both ends on the ground, so the arc
//   is a half-ellipse whose peak is `height / 2` times the chord -- a
//   semicircle at deck's default height of 1. Flat: in Mercator world units,
//   the height in the same units as the chord. Globe: a great circle on the
//   unit sphere, lifted by `height / 2` times the central angle.
// - **Width**: extruded on screen, perpendicular to the strip, in device
//   pixels. Under one device pixel the strip stays one pixel wide and the
//   alpha carries the rest (coverage), so 0.2 px Kepler arcs do not drop out
//   to a dashed line. Half a device pixel of feather each side from a pixel
//   up; a sub-pixel arc is a one-pixel strip, unfeathered.
// - **Filter**: `overlay::filter`'s window as a uniform; an arc outside it
//   collapses outside the clip volume (deck.gl's DataFilterExtension).
// - **Portability**: six instance attributes, two inter-stage vectors, one
//   uniform block, scalar selects only (WebGL2 / iOS: 15 inter-stage
//   components; GLSL ES 3.00).

struct ArcUniform {
    /// World units RELATIVE TO `origin` -> clip, east-west flip folded in
    /// (`render::arcs::ArcFrame::flat_view_proj`).
    flat_view_proj: mat4x4<f32>,
    /// Unit-sphere position -> clip: `GlobeFrame::globe_view_proj`.
    globe_view_proj: mat4x4<f32>,
    /// `xyz`: the eye in sphere space (the planet's occlusion). `w`: unused.
    eye: vec4<f32>,
    /// `(origin mercator x, origin mercator y, WORLD_SIZE, transition)`.
    origin: vec4<f32>,
    /// `(width, height)` of the target in device px, device px per logical
    /// px, the layer opacity.
    viewport: vec4<f32>,
    /// `(segments, height, filter on, additive)`.
    params: vec4<f32>,
    /// `(lo, hi, 0, 0)`: the filter window, relative values.
    filter_window: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: ArcUniform;

struct ArcInput {
    @location(0) source: vec2<f32>,
    @location(1) dest: vec2<f32>,
    @location(2) source_color: vec4<f32>,
    @location(3) dest_color: vec4<f32>,
    @location(4) width_px: f32,
    @location(5) filter_value: f32,
};

struct ArcOutput {
    @builtin(position) position: vec4<f32>,
    /// Premultiplied colour, coverage folded in.
    @location(0) color: vec4<f32>,
    /// `x`: signed device px from the centreline. `y`: the core half width,
    /// device px (the feather runs from `y - 0.5` to `y + 0.5`).
    @location(1) edge: vec2<f32>,
};

const PI: f32 = 3.14159265358979;
const MAX_LAT: f32 = 85.051129;

/// lon/lat degrees -> Web Mercator `[0, 1]` (`geo::lon_lat_to_tile_xy` at z0).
fn mercator(lon_lat: vec2<f32>) -> vec2<f32> {
    let lat = clamp(lon_lat.y, -MAX_LAT, MAX_LAT) * (PI / 180.0);
    return vec2<f32>((lon_lat.x + 180.0) / 360.0, (1.0 - log(tan(PI * 0.25 + lat * 0.5)) / PI) * 0.5);
}

/// lon/lat degrees -> the unit sphere, east at +x (`globe::mercator_to_sphere`).
fn sphere(lon_lat: vec2<f32>) -> vec3<f32> {
    let lon = lon_lat.x * (PI / 180.0);
    let lat = lon_lat.y * (PI / 180.0);
    return vec3<f32>(sin(lon) * cos(lat), sin(lat), cos(lon) * cos(lat));
}

/// deck's `getSegmentRatio`: denser toward the ends, where the arc is steep.
fn segment_ratio(i: f32, segments: f32) -> f32 {
    return smoothstep(0.0, 1.0, i / segments);
}

/// deck's `paraboloid` with `sourceZ == targetZ == 0`: the lift at `r`, per
/// unit of chord.
fn lift(r: f32) -> f32 {
    return sqrt(max(r * (1.0 - r), 0.0)) * u.params.y;
}

/// The arc's point at `r` on the flat map, Mercator `[0, 1]` (xy, wrapped to
/// the world copy nearest the origin) and its height in world units (z).
/// `t` is the target already unwrapped onto the source's side of the
/// antimeridian (the shortest way).
fn flat_point(s: vec2<f32>, t: vec2<f32>, r: f32) -> vec3<f32> {
    let chord = length(t - s) * u.origin.z;
    var m = mix(s, t, r);
    m.x = m.x - round(m.x - u.origin.x);
    return vec3<f32>(m, lift(r) * chord);
}

fn flat_clip(p: vec3<f32>) -> vec4<f32> {
    let rel = (p.xy - u.origin.xy) * u.origin.z;
    return u.flat_view_proj * vec4<f32>(rel, p.z, 1.0);
}

/// The arc's point at `r` on the globe: the great circle from `ps` to `pt`
/// (central angle `omega`), lifted off the unit sphere.
fn globe_point(ps: vec3<f32>, pt: vec3<f32>, omega: f32, r: f32) -> vec3<f32> {
    var p = normalize(mix(ps, pt, r));
    if (omega > 1e-4) {
        p = (sin((1.0 - r) * omega) * ps + sin(r * omega) * pt) / sin(omega);
    }
    return p * (1.0 + lift(r) * omega);
}

/// Whether the planet hides sphere point `p` from the eye: the segment
/// eye -> p passes inside the unit sphere (`render::altitude::closest_approach`).
fn occluded(p: vec3<f32>) -> bool {
    let e = u.eye.xyz;
    let d = p - e;
    let dd = max(dot(d, d), 1e-20);
    let k = clamp(-dot(e, d) / dd, 0.0, 1.0);
    return length(e + d * k) < 0.9999;
}

/// The clip position of the arc's point at `r`, flat and globe blended by
/// the transition as `shader.wgsl`'s `interpolate_projection` blends them.
/// `.z` keeps the near plane and never clips at the far one; a point the
/// planet hides is pushed past the far plane.
fn arc_clip(s: vec2<f32>, t: vec2<f32>, ps: vec3<f32>, pt: vec3<f32>, omega: f32, r: f32) -> vec4<f32> {
    let transition = u.origin.w;
    var flat_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var globe_c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    var hidden = false;
    if (transition < 1.0) {
        flat_c = flat_clip(flat_point(s, t, r));
    }
    if (transition > 0.0) {
        let g = globe_point(ps, pt, omega, r);
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
fn vs_arc(input: ArcInput, @builtin(vertex_index) vid: u32) -> ArcOutput {
    var out: ArcOutput;
    out.position = OFF;
    out.color = vec4<f32>(0.0);
    out.edge = vec2<f32>(0.0);
    // The filter window: outside it (or no value, a NaN) the arc is not drawn.
    if (u.params.z > 0.5
        && !(input.filter_value >= u.filter_window.x && input.filter_value <= u.filter_window.y)) {
        return out;
    }
    if (all(input.source == input.dest)) {
        return out;
    }
    let segments = u.params.x;
    let i = f32(vid / 2u);
    let side = select(-1.0, 1.0, (vid & 1u) == 1u);
    let last = i >= segments;
    let j = select(i + 1.0, i - 1.0, last);
    let r = segment_ratio(min(i, segments), segments);
    let r2 = segment_ratio(j, segments);

    // The shortest way: the target moved by 360 degrees onto the source's
    // side of the antimeridian, so the flat arc never spans the world.
    var dst = input.dest;
    let dlon = dst.x - input.source.x;
    dst.x = dst.x - 360.0 * round(dlon / 360.0);
    let s = mercator(input.source);
    let t = mercator(dst);

    var curr: vec4<f32>;
    var other: vec4<f32>;
    var seam = 1.0;
    if (u.origin.w <= 0.0) {
        // A flat frame (almost all of them): no sphere at all.
        //
        // The cull first: the arc lies in the box spanned by its two ends
        // and the same ends raised to its peak, and the projection of a box
        // is inside the hull of its corners. When all four are past one
        // side of the clip volume (and in front of the eye) the whole strip
        // is off screen: zoomed into London, most of 680k commutes are.
        // Linear in z, so the raised corners are one column away.
        let e0 = flat_point(s, t, 0.0);
        let e1 = flat_point(s, t, 1.0);
        if (abs(e0.x - e1.x) < 0.5) {
            let peak = 0.5 * u.params.y * length(t - s) * u.origin.z;
            let up = u.flat_view_proj[2] * peak;
            let c0 = flat_clip(e0);
            let c1 = flat_clip(e1);
            let c2 = c0 + up;
            let c3 = c1 + up;
            let xs = vec4<f32>(c0.x, c1.x, c2.x, c3.x);
            let ys = vec4<f32>(c0.y, c1.y, c2.y, c3.y);
            let ws = vec4<f32>(c0.w, c1.w, c2.w, c3.w);
            if (all(ws > vec4<f32>(0.0))
                && (all(xs > ws) || all(xs < -ws) || all(ys > ws) || all(ys < -ws))) {
                return out;
            }
        }
        let a = flat_point(s, t, r);
        let b = flat_point(s, t, r2);
        curr = flat_clip(a);
        other = flat_clip(b);
        curr.z = min(curr.z, curr.w * 0.9999);
        // The flat arc wraps to the world copy nearest the camera per
        // point; the one segment that crosses the seam (half a world from
        // the camera) would span the map, so it is faded out instead.
        if (abs(a.x - b.x) > 0.5) {
            seam = 0.0;
        }
    } else {
        let ps = sphere(input.source);
        let pt = sphere(input.dest);
        let omega = atan2(length(cross(ps, pt)), dot(ps, pt));
        curr = arc_clip(s, t, ps, pt, omega, r);
        other = arc_clip(s, t, ps, pt, omega, r2);
        if (u.origin.w < 1.0 && abs(flat_point(s, t, r).x - flat_point(s, t, r2).x) > 0.5) {
            seam = 0.0;
        }
    }

    // Screen-space extrusion (deck's `getExtrusionOffset`), in device px.
    let half_vp = u.viewport.xy * 0.5;
    let wa = max(abs(curr.w), 1e-6);
    let wb = max(abs(other.w), 1e-6);
    var dir = (other.xy / wb - curr.xy / wa) * half_vp;
    if (last) {
        dir = -dir;
    }
    let len = length(dir);
    if (len > 1e-6) {
        dir = dir / len;
    } else {
        dir = vec2<f32>(1.0, 0.0);
    }
    let normal = vec2<f32>(-dir.y, dir.x);
    let width = max(input.width_px, 0.0) * u.viewport.z;
    // At a pixel or more: the core, and half a device pixel of feather each
    // side. Under a pixel (Kepler's 0.2 px commute arcs): a one-pixel strip
    // at full coverage, the alpha carrying the width -- half the fragments
    // of a feathered strip, and at that alpha the stair-steps do not show.
    // Fragments are the cost of 680k overlapping arcs, not vertices.
    var core = max(width, 1.0) * 0.5;
    var outer = core + 0.5;
    if (width < 1.0) {
        core = 1.0;
        outer = 0.5;
    }
    curr = vec4<f32>(curr.xy + normal * side * outer / half_vp * curr.w, curr.zw);

    let color = mix(input.source_color, input.dest_color, r);
    let alpha = color.a * u.viewport.w * min(width, 1.0) * seam;
    out.position = curr;
    out.color = vec4<f32>(color.rgb * alpha, alpha);
    out.edge = vec2<f32>(side * outer, core);
    return out;
}

@fragment
fn fs_arc(input: ArcOutput) -> @location(0) vec4<f32> {
    let coverage = clamp(input.edge.y + 0.5 - abs(input.edge.x), 0.0, 1.0);
    return input.color * coverage;
}
