// The sky: a vertical gradient from the horizon colour at the planet's limb
// up to the sky colour, faded out toward space by the globe transition.
//
// PROVENANCE -- see `render::sky`'s module docs and `src/render/PROVENANCE.md`:
// MapLibre GL JS `src/shaders/glsl/sky.fragment.glsl` (BSD-3, Copyright (c)
// 2023 MapLibre contributors). The blend is the reference's, term for term:
//
//     blend = distance above the horizon
//     if blend < sky_horizon_blend:
//         colour = mix(sky, horizon, pow(1 - blend / sky_horizon_blend, 2))
//     else:
//         colour = sky
//     colour = mix(colour, transparent, projectionTransition)
//
// Two things differ, both because the reference's horizon is a straight
// line and ours is the limb of a sphere:
//
//  * "distance above the horizon" is an ANGLE above the limb's tangent cone
//    rather than a pixel distance above a screen line, and the blend's reach
//    is `sky-horizon-blend` times half the vertical FOV rather than times half
//    the viewport height. The two agree at the centre of the screen, where a
//    pixel is `fov_y / height` of angle.
//  * a ray that analytically hits the sphere is not sky at all: it gets the
//    GROUND -- the style's background colour, shaded by the ground's own
//    night -- opaque (see fs_main). This pass is drawn BEHIND the ground, so
//    that only shows where no tile fill was drawn: bare land, which a style
//    paints with its `background` layer alone, and the slivers between the
//    tile mesh's chords and the true limb.
//
// A third is ours: as the sky fades toward space its horizon colour runs
// toward its sky colour, so the fade never shows a grey band (see fs_main).
//
// And the night is ours, because the reference has no night (see fs_main
// and `render::sky`'s module docs): on the night side the sky is SPACE --
// it paints nothing and hides no stars -- through the SAME terminator the
// ground is shaded by (`atmosphere::terminator_lit`, the same
// `TERMINATOR_HALF_WIDTH_SIN`), with a faint twilight band along the limb
// just past it. By day it hides the stars.
//
// Nothing here comes from any Mapbox GL JS 2.x/3.x tree.

struct Sky {
    /// VIEW space -> SPHERE space, applied to directions only.
    inverse_view: mat4x4<f32>,
    /// The eye in sphere space: the planet is the unit sphere at the origin.
    camera_sphere: vec3<f32>,
    /// `1 - space_transition`: the reference's `mix(.., transparent,
    /// u_sky_blend)`, as the coverage this pass composites with.
    alpha: f32,
    /// Unit direction from the planet's centre TOWARD the sun, sphere space.
    sun_dir: vec3<f32>,
    /// `tan(fov_y / 2)`.
    tan_half_fov_y: f32,
    /// `sky-color`, straight (not premultiplied) alpha.
    sky_color: vec4<f32>,
    /// `horizon-color`, straight alpha.
    horizon_color: vec4<f32>,
    /// Viewport width / height.
    aspect: f32,
    /// `sky-horizon-blend * fov_y / 2`, radians: how far above the limb the
    /// horizon colour reaches. Zero means sky colour only.
    horizon_angle: f32,
    /// How much of the day sky is left on the night side:
    /// `render::sky::sky_night_floor` -- 0 (space) at the default
    /// `night_brightness`, 1 with the terminator switched off.
    night: f32,
    /// The GROUND's night floor, `atmosphere::night_floor` -- what
    /// `shader.wgsl` shades the tiles with: 1.0 from z6 up.
    ground_night: f32,
    /// The style's background colour at this zoom, straight alpha: what a
    /// ray that hits the planet paints (see fs_main).
    ground_color: vec4<f32>,
}

@group(0) @binding(0) var<uniform> sky: Sky;

/// `shader.wgsl`'s and `render::atmosphere`'s terminator half-width. One
/// terminator for the ground, the labels and the sky.
const TERMINATOR_HALF_WIDTH_SIN: f32 = 0.05234;

// OURS, and mirrored in `render::sky` (a test holds the two to the same
// numbers): see that module's "Night" section for what each one is for.
/// How far past the terminator, as a sine (about 9 degrees), the twilight
/// band reaches.
const TWILIGHT_REACH_SIN: f32 = 0.157;
/// The most of the horizon colour the twilight band adds, at the limb.
const TWILIGHT_STRENGTH: f32 = 0.15;
/// How opaque to the STARS the day sky is per unit of coverage: at a
/// quarter coverage (z4) the day sky already hides them completely.
const STAR_OCCLUSION_GAIN: f32 = 4.0;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    /// The fragment's ray in SPHERE space, unnormalised -- linear in NDC, so
    /// interpolating it is exact (see `atmosphere_shader.wgsl`).
    @location(0) ray: vec3<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // One oversized triangle: (-1,-1), (3,-1), (-1,3).
    let ndc = vec2<f32>(
        f32((index << 1u) & 2u) * 2.0 - 1.0,
        f32(index & 2u) * 2.0 - 1.0,
    );
    var out: VertexOutput;
    // At the far plane. The pipeline neither tests nor writes depth, so this
    // only has to be inside the clip volume.
    out.clip_position = vec4<f32>(ndc, 1.0, 1.0);
    let view_ray = vec3<f32>(
        ndc.x * sky.tan_half_fov_y * sky.aspect,
        ndc.y * sky.tan_half_fov_y,
        -1.0,
    );
    out.ray = (sky.inverse_view * vec4<f32>(view_ray, 0.0)).xyz;
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let ray = normalize(input.ray);
    let eye = sky.camera_sphere;
    let d = max(length(eye), 1.0);

    // The ray's angle from the direction to the planet's centre, against the
    // limb's: `asin(1 / d)` is the half-angle of the cone of rays tangent to
    // the unit sphere from an eye `d` radii from its centre.
    let theta = acos(clamp(dot(ray, -eye / d), -1.0, 1.0));
    let limb = asin(clamp(1.0 / d, 0.0, 1.0));
    let elevation = theta - limb;

    // THE GROUND UNDER THE TILES (OURS). A ray below the limb hits the
    // planet, and what is there is the style's background layer -- bare
    // land. Whenever this pass draws the frame clears to SPACE (so the night
    // sky can be black with stars), so without this every pixel no tile fill
    // covered -- bare land between landcover patches, the translucent urban
    // fills over it -- was the sky's colour, or black space with stars in it
    // on whichever side the nearest-point normal below called night (the
    // Kansas "shadow" and the black cities at z6.77, the Atacama at z6.86).
    // Opaque, so no star shows below the horizon; shaded exactly as
    // `shader.wgsl` shades the ground (`atmosphere::night_shade`) at the
    // point the ray hits, so it is the background itself from z6 up, where
    // the ground floor is 1.0.
    if (elevation < 0.0) {
        let to_centre = -dot(eye, ray);
        let closest = eye + ray * to_centre;
        let half_chord = sqrt(max(1.0 - dot(closest, closest), 0.0));
        let hit = normalize(eye + ray * (to_centre - half_chord));
        let ground_lit = smoothstep(-TERMINATOR_HALF_WIDTH_SIN, TERMINATOR_HALF_WIDTH_SIN, dot(hit, sky.sun_dir));
        let shade = mix(sky.ground_night, 1.0, ground_lit);
        return vec4<f32>(sky.ground_color.rgb * shade * sky.ground_color.a, 1.0);
    }

    // `sky.fragment.glsl`, with the reference's pixel distance replaced by
    // the angle above the limb (see the header).
    //
    // OURS: the horizon colour runs toward the SKY colour as the sky fades
    // (`1 - alpha` is the transition). Faded by coverage alone, the spec's
    // white horizon over black space reads as a band of mid-GREY halfway
    // through the band (z4.5: white at 50% = 128,128,128) -- the very colour
    // this pass exists to remove. Exactly the reference at `alpha == 1`.
    let horizon = mix(sky.horizon_color, sky.sky_color, 1.0 - sky.alpha);
    var colour = sky.sky_color;
    // How much of the horizon colour this ray carries: 1 at the limb, 0 from
    // `horizon_angle` up. The twilight band below reuses it.
    var at_horizon = 0.0;
    if (sky.horizon_angle > 0.0) {
        let blend = max(elevation, 0.0) / sky.horizon_angle;
        if (blend < 1.0) {
            at_horizon = pow(1.0 - blend, 2.0);
            colour = mix(sky.sky_color, horizon, at_horizon);
        }
    }

    // OURS: the night. The ground point under this bit of sky is the point
    // of the ray nearest the planet's centre -- the tangent point for a ray
    // that grazes the limb, the eye's own nadir for one aimed straight up.
    // Only ever above the limb: for a ray that HITS the planet that point is
    // inside it, and its direction says nothing about the ground's sun.
    let along = max(-dot(eye, ray), 0.0);
    let nearest = eye + ray * along;
    let normal = nearest / max(length(nearest), 1e-6);
    let cos_zenith = dot(normal, sky.sun_dir);
    // THE GROUND'S TERMINATOR, term for term (`atmosphere::terminator_lit`):
    // the sky over a point turns to night exactly where, and over exactly
    // the width, the ground at that point does.
    let lit = smoothstep(-TERMINATOR_HALF_WIDTH_SIN, TERMINATOR_HALF_WIDTH_SIN, cos_zenith);
    // How much of the day sky shows: all of it by day, `night` of it by
    // night -- and `night` is 0 unless the terminator is switched off, so the
    // night sky is SPACE. Not a scaled-down white (that is grey) and not a
    // tint (that is blue): the user's rule is "black, with stars".
    let day = mix(sky.night, 1.0, lit);
    var rgb = colour.rgb * day;
    // Twilight: a faint lift of the horizon colour along the limb just past
    // the terminator on the night side, gone `TWILIGHT_REACH_SIN` into it,
    // absent on the day side and on a frame whose night is switched off.
    let band = (1.0 - lit) * (1.0 - smoothstep(0.0, TWILIGHT_REACH_SIN, -cos_zenith));
    rgb = rgb + horizon.rgb * (at_horizon * band * (1.0 - sky.night) * TWILIGHT_STRENGTH);

    // PREMULTIPLIED, like the atmosphere pass: the pipeline blends with
    // `PREMULTIPLIED_ALPHA_BLENDING`, so a transparent sky colour -- the
    // reference's own "no sky" -- leaves the framebuffer byte-identical.
    //
    // The ALPHA is not the coverage, and that is how the stars are hidden
    // (OURS). The stars are drawn first and the frame is cleared to the
    // space colour whenever this pass draws (`frame_plan::space_plan`); this
    // is composited over them as `rgb + dst * (1 - alpha)`. With
    // `alpha == coverage` a z4.5 day sky (coverage 0.5) let half of every
    // star through the blue. Here the alpha is the sky's opacity TO THE
    // STARS: `STAR_OCCLUSION_GAIN` times the coverage by day, so a
    // quarter-covered day sky already hides them, and zero by night, where
    // the sky paints nothing and the stars and space are what shows. It
    // still goes to 0 with the coverage, so the stars come back continuously
    // toward z3, and a transparent sky is still no sky.
    let a = colour.a * sky.alpha;
    let occlusion = min(a * STAR_OCCLUSION_GAIN, 1.0) * day;
    return vec4<f32>(rgb * a, occlusion);
}
