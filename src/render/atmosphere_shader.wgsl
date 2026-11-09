// The atmosphere: a Rayleigh/Mie single-scattering integral, marched
// full-screen, in the unit-sphere frame the globe already draws in.
//
// PROVENANCE, both links of it -- see `src/render/PROVENANCE.md` and
// `render::atmosphere`'s module docs:
//
//   * the scattering function is wwwtyro/glsl-atmosphere (UNLICENSE, public
//     domain), https://github.com/wwwtyro/glsl-atmosphere, `index.glsl`;
//   * the adaptation to a globe -- 5 primary and 3 secondary steps instead of
//     16 and 8, the opacity channel, the planet-surface clamp, the entry-point
//     offset and the exposure -- is MapLibre GL JS's
//     `src/shaders/glsl/atmosphere.fragment.glsl` (BSD-3, Copyright (c) 2023
//     MapLibre contributors), whose own header carries the first link.
//
// One thing of MapLibre's is deliberately NOT here -- its 1/2.2 gamma -- and
// `fs_main` carries the measured table that decided against it.
//
// Nothing here comes from any Mapbox GL JS 2.x/3.x tree.

struct Air {
    /// VIEW space -> SPHERE space. Applied to DIRECTIONS only (`w = 0`), so
    /// the globe-radius scale it carries divides out under the normalise in
    /// `fs_main`.
    inverse_view: mat4x4<f32>,
    /// The eye, in sphere space: the planet is the unit sphere centred at the
    /// origin, so this is the camera distance in planetary radii.
    camera_sphere: vec3<f32>,
    /// `atmosphere-blend * projectionTransition` (`draw_sky.ts:90`). Zero is
    /// never submitted -- `render::atmosphere::Atmosphere::draws` skips the
    /// draw entirely, the reference's own gate at `draw_sky.ts:91-95`.
    blend: f32,
    /// Unit direction from the planet's centre TOWARD the sun, in sphere
    /// space. See `render::atmosphere::Sun`.
    sun_dir: vec3<f32>,
    /// `tan(fov_y / 2)`.
    tan_half_fov_y: f32,
    /// Viewport width / height.
    aspect: f32,
}

@group(0) @binding(0) var<uniform> air: Air;

const PI: f32 = 3.141592653589793;
/// Primary ray steps. MapLibre's 5, against glsl-atmosphere's 16 -- the
/// single largest difference between the two upstreams, and the one that
/// makes a full-screen pass affordable at all.
const I_STEPS: i32 = 5;
/// Secondary (toward-the-sun) ray steps. MapLibre's 3, against 8.
const J_STEPS: i32 = 3;
/// Radius of the planet, metres (`atmosphere.fragment.glsl:22`).
const EARTH_RADIUS: f32 = 6371e3;
/// Radius of the top of the atmosphere, metres
/// (`atmosphere.fragment.glsl:24`). A 100 km shell, 1.57% of the planet.
const ATMOS_RADIUS: f32 = 6471e3;
/// The largest exponent `exp()` is ever asked for. e^80 is 5.5e34, comfortably
/// inside f32's 3.4e38, and an optical depth that large already attenuates to
/// a hard zero -- so this changes no pixel and removes every path to an
/// infinity. It is needed because a secondary ray sampled INSIDE the planet
/// has a negative height, which is exactly how the night side comes to be
/// dark; the reference lets that overflow and relies on `exp(-inf) == 0`.
const MAX_EXPONENT: f32 = 80.0;
/// The LONGEST path through the air any ray can take: the full chord of the
/// shell at grazing incidence, `2 * sqrt(r_atmos^2 - r_planet^2)` =
/// **2,266.45 km**. `fs_main` divides each ray's own path by this, so the
/// factor is 1 exactly where the air is thickest and cannot exceed 1
/// anywhere -- see `fs_main` for what that factor is for.
///
/// Written as `(a - b) * (a + b)` rather than `a*a - b*b` on purpose: the
/// two squares are 4.19e13 and 4.06e13, which in f32 are each known to
/// about +/-4e6, and subtracting them would leave a 1.28e12 difference with
/// six digits of cancellation in it. The factored form multiplies 1.0e5 by
/// 1.2842e7 and loses nothing.
const MAX_AIR_PATH_M: f32 =
    2.0 * sqrt((ATMOS_RADIUS - EARTH_RADIUS) * (ATMOS_RADIUS + EARTH_RADIUS));

/// What `scatter` returns: the light, and the length of the air it came
/// through.
///
/// The path length is returned rather than recomputed in `fs_main` because
/// it is not simply `rsi(...)`: it is `rsi` against the shell, clamped to
/// the camera, and then clamped again to the planet's surface when the
/// planet is really in front (MapLibre's changes 1 and 2, below). Two
/// call sites for those two clamps would be two chances for them to
/// disagree, and the symptom of a disagreement would be a wrong brightness
/// rather than a compile error.
struct Scattered {
    /// Light ADDED along the ray in `rgb`; the transmittance in `a`.
    light: vec4<f32>,
    /// How far the ray travelled through the shell, in metres. `0.0` when
    /// the ray missed the shell entirely.
    path_m: f32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    /// The fragment's ray direction in SPHERE space, unnormalised.
    ///
    /// Unnormalised on purpose: the view-space ray
    /// `(x*tan*aspect, y*tan, -1)` is LINEAR in the NDC coordinates, so
    /// interpolating it across the triangle is exact. Interpolating
    /// normalised directions would not be -- the normalise is a division, and
    /// a division does not commute with a linear blend.
    @location(0) ray: vec3<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // ONE oversized triangle covering the viewport: (-1,-1), (3,-1), (-1,3).
    // A quad would cover the diagonal twice and give the rasterizer a seam to
    // run the scattering integral along. No vertex buffer and no index
    // buffer, so `draw(0..3)` has exactly one meaning -- unlike
    // `star_shader.wgsl`, which carries its corner per vertex precisely
    // because it DOES have an index buffer whose winding could change.
    let ndc = vec2<f32>(
        f32((index << 1u) & 2u) * 2.0 - 1.0,
        f32(index & 2u) * 2.0 - 1.0,
    );
    var out: VertexOutput;
    // `z = 1`: the FAR plane, so that the pipeline's `LessEqual` draws this
    // pass only where nothing nearer has been drawn -- over space, whose
    // depth is still the cleared 1.0, and over the one ring at the limb
    // where the ground's own clip z reaches 1.0. Over the rest of the disc
    // the tiles wrote a nearer depth and the glow is rejected, which is what
    // keeps it a rim instead of a wash across the planet. See
    // `AtmospherePipeline::new` for the band this removes and its size.
    out.clip_position = vec4<f32>(ndc, 1.0, 1.0);
    // View space: +x right, +y up, the camera looking down -z.
    let view_ray = vec3<f32>(
        ndc.x * air.tan_half_fov_y * air.aspect,
        ndc.y * air.tan_half_fov_y,
        -1.0,
    );
    out.ray = (air.inverse_view * vec4<f32>(view_ray, 0.0)).xyz;
    return out;
}

/// Ray-sphere intersection for a sphere centred at the origin. Verbatim from
/// glsl-atmosphere (`index.glsl`), which MapLibre also takes verbatim.
/// No intersection when `result.x > result.y`.
fn rsi(r0: vec3<f32>, rd: vec3<f32>, sr: f32) -> vec2<f32> {
    let a = dot(rd, rd);
    let b = 2.0 * dot(rd, r0);
    let c = dot(r0, r0) - (sr * sr);
    let d = (b * b) - 4.0 * a * c;
    if (d < 0.0) {
        return vec2<f32>(1e5, -1e5);
    }
    return vec2<f32>((-b - sqrt(d)) / (2.0 * a), (-b + sqrt(d)) / (2.0 * a));
}

/// `exp(-x/scale)` with the exponent capped. See `MAX_EXPONENT`.
fn density(height: f32, scale_height: f32) -> f32 {
    return exp(min(-height / scale_height, MAX_EXPONENT));
}

/// The scattering integral, and the path it integrated over. `light.rgb` is
/// light ADDED along the ray; `light.a` is the transmittance -- how much of
/// whatever is behind survives.
///
/// glsl-atmosphere's `atmosphere()` with MapLibre's four changes, each
/// marked below, plus `path_m` -- ours, and see [`Scattered`].
fn scatter(
    ray_in: vec3<f32>,
    r0: vec3<f32>,
    sun_in: vec3<f32>,
    i_sun: f32,
    r_planet: f32,
    r_atmos: f32,
    k_rlh: vec3<f32>,
    k_mie: f32,
    sh_rlh: f32,
    sh_mie: f32,
    g: f32,
) -> Scattered {
    let sun = normalize(sun_in);
    let ray = normalize(ray_in);

    var p = rsi(r0, ray, r_atmos);
    if (p.x > p.y) {
        // The ray misses the shell entirely. Opacity 1, so `fs_main` emits
        // alpha 0 and the framebuffer is left BYTE-IDENTICAL under
        // premultiplied blending. (MapLibre returns the same `vec4(0,0,0,1)`;
        // glsl-atmosphere, which has no alpha, returns `vec3(0)`.)
        //
        // Path zero, so `fs_main`'s factor is zero too -- which cannot make
        // the byte-identity above any stronger, and is the honest answer:
        // a ray that misses the shell crossed no air.
        return Scattered(vec4<f32>(0.0, 0.0, 0.0, 1.0), 0.0);
    }
    // MapLibre's change 1: clamp the entry to the camera. glsl-atmosphere
    // starts its march at `iTime = 0` and so ignores `p.x` altogether, which
    // is only right for a camera already inside the shell.
    if (p.x < 0.0) {
        p.x = 0.0;
    }
    // MapLibre's change 2: stop at the planet's SURFACE, and only when the
    // planet is really in front. glsl-atmosphere takes `rsi(...).x`
    // unconditionally, which is `1e5` on a miss.
    let p2 = rsi(r0, ray, r_planet);
    if (p2.x <= p2.y && p2.x > 0.0) {
        p.y = min(p.y, p2.x);
    }

    let i_step_size = (p.y - p.x) / f32(I_STEPS);
    var i_time = p.x + i_step_size * 0.5;

    var total_rlh = vec3<f32>(0.0, 0.0, 0.0);
    var total_mie = vec3<f32>(0.0, 0.0, 0.0);
    var i_od_rlh = 0.0;
    var i_od_mie = 0.0;

    // The phase functions: Rayleigh's `(1 + mu^2)` and Henyey-Greenstein for
    // Mie, both unchanged from glsl-atmosphere.
    let mu = dot(ray, sun);
    let mumu = mu * mu;
    let gg = g * g;
    let p_rlh = 3.0 / (16.0 * PI) * (1.0 + mumu);
    let p_mie = 3.0 / (8.0 * PI) * ((1.0 - gg) * (mumu + 1.0))
        / (pow(1.0 + gg - 2.0 * mu * g, 1.5) * (2.0 + gg));

    for (var i = 0; i < I_STEPS; i = i + 1) {
        let i_pos = r0 + ray * i_time;
        let i_height = length(i_pos) - r_planet;

        let od_step_rlh = density(i_height, sh_rlh) * i_step_size;
        let od_step_mie = density(i_height, sh_mie) * i_step_size;
        i_od_rlh = i_od_rlh + od_step_rlh;
        i_od_mie = i_od_mie + od_step_mie;

        // Toward the sun. Note what is NOT here: a test for the planet
        // blocking the secondary ray. It is not needed, and that is the
        // mechanism the terminator comes out of -- a sample on the night side
        // has its secondary ray pass THROUGH the planet, where `j_height` is
        // negative, the density explodes, and the attenuation below goes to
        // zero. The shadow is a consequence of the integral rather than a
        // special case in it.
        let j_step_size = rsi(i_pos, sun, r_atmos).y / f32(J_STEPS);
        var j_time = j_step_size * 0.5;
        var j_od_rlh = 0.0;
        var j_od_mie = 0.0;
        for (var j = 0; j < J_STEPS; j = j + 1) {
            let j_pos = i_pos + sun * j_time;
            let j_height = length(j_pos) - r_planet;
            j_od_rlh = j_od_rlh + density(j_height, sh_rlh) * j_step_size;
            j_od_mie = j_od_mie + density(j_height, sh_mie) * j_step_size;
            j_time = j_time + j_step_size;
        }

        let attn = exp(-(k_mie * (i_od_mie + j_od_mie) + k_rlh * (i_od_rlh + j_od_rlh)));
        total_rlh = total_rlh + od_step_rlh * attn;
        total_mie = total_mie + od_step_mie * attn;
        i_time = i_time + i_step_size;
    }

    // MapLibre's change 3: an opacity, which glsl-atmosphere has no need of
    // because it renders a sky rather than something with a planet behind it.
    // Its own comment: "We need to approximate attenuation with a single
    // grayscale value instead of a color."
    let opacity = exp(-(length(k_rlh) * length(total_rlh) + k_mie * length(total_mie)));
    let color = i_sun * (p_rlh * k_rlh * total_rlh + p_mie * k_mie * total_mie);
    // `p.y - p.x` is exactly what the loop above marched: shell entry (or
    // the camera, whichever is nearer) to shell exit (or the planet's
    // surface, whichever is nearer). Both clamps are already applied, which
    // is the whole reason this is returned from here.
    return Scattered(vec4<f32>(color, opacity), p.y - p.x);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // The ray origin in METRES. MapLibre scales its whole world up to
    // EARTH_RADIUS here because its globe's radius is a screen-derived
    // number (`-u_globe_position * EARTH_RADIUS / u_globe_radius`); ours is
    // the unit sphere by construction, so the scale is one multiply and the
    // camera position needs no reconstruction from a projection matrix.
    let r0 = air.camera_sphere * EARTH_RADIUS;

    let scattered = scatter(
        input.ray,                            // ray direction
        r0,                                   // ray origin
        air.sun_dir,                          // toward the sun
        22.0,                                 // intensity of the sun
        EARTH_RADIUS,
        ATMOS_RADIUS,
        vec3<f32>(5.5e-6, 13.0e-6, 22.4e-6),  // Rayleigh scattering coefficient
        21e-6,                                // Mie scattering coefficient
        8e3,                                  // Rayleigh scale height
        1.2e3,                                // Mie scale height
        0.758,                                // Mie preferred scattering direction
    );
    let color = scattered.light;

    // Exposure -- `atmosphere.fragment.glsl:168`, unchanged. A tone map, not
    // a transfer function: it takes an unbounded radiance to [0, 1).
    let exposed = 1.0 - exp(-1.0 * color.rgb);

    // OURS, AND IT IS THE SHAPE OF THE EFFECT: the ray's own path length
    // through the air, as a fraction of the longest path there is.
    //
    // A ray aimed at the middle of the disc crosses the 100 km shell and
    // stops at the ground; one grazing the limb crosses the whole 2,266.45
    // km chord. That is a **22.6x** ratio of air, and without this factor
    // the frame does not show it: the exposure `1 - exp(-rgb)` above and
    // the `1 - opacity` alpha are both saturating maps, and between them
    // they compress 22.6x into about 11x. Measured against a control with
    // the pass withheld, ocean `#4f7499`, `Sun::VIEWPORT_DEFAULT`, largest
    // channel, at 768x768 z3 -- the default example's globe camera, and
    // the worse of the two:
    //
    //   r / limb          without the factor    with it
    //   0.00 centre          d12                  d 1
    //   0.50                 d 7-d22               0-d 1
    //   0.95                  0-d17                0-d 3
    //   1.005 rim             0-d93                0-d79
    //   interior mean        13.63                 0.98
    //   interior changed     99.0%                80.1%
    //
    // and the same shape at 512x512 z1.5 (interior mean 11.99 -> 0.88, rim
    // d142 -> d128). The disc centre's per-channel move -- red -4, green
    // +4, blue +12, which is a DESATURATION and not a dimming -- collapses
    // to (0, 0, +1).
    // `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash` is that
    // table as assertions, with the factor-less arm rendered alongside so
    // the defect is graded and not just described.
    //
    // WHY IT IS NOT `Atmosphere::intensity`, which would also dim the
    // interior: that is a UNIFORM scalar and this is PER FRAGMENT. Task 9
    // established the distinction by measuring it -- `atmosphere-blend`
    // "multiplies the whole premultiplied result, so it scales the rim's
    // emitted light and the wash's by the SAME factor and dims them
    // together. It is a volume knob, not a shape." This factor runs from
    // 0.044 at the disc centre to 1.0 at the limb, so it is the shape.
    //
    // WHY IT MULTIPLIES BOTH CHANNELS. In premultiplied alpha the fragment
    // is `(S, A)` and the composite is `dst * (1 - A) + S`, so the change
    // this pass makes to a pixel is `S - dst * A` and scaling the whole
    // vec4 by `f` scales that change by exactly `f` -- linearly, on every
    // channel, which is why the desaturation goes with the wash rather than
    // surviving it. Scaling the alpha ALONE would be worse than doing
    // nothing: at the disc centre the emitted light is about 38 byte units
    // of blue against an alpha of about 0.17, so removing the alpha term
    // and keeping the light would take the centre from d12 to d38.
    //
    // CLAMPED at 1, so this can only ever dim. A camera inside the shell
    // (below 100 km, which the globe band never reaches -- it ends at z6)
    // has `p.x` clamped to the camera rather than to the shell's top, and
    // the clamp means that cannot come out as a factor above 1 and make the
    // rim brighter than the integral says it is.
    let path_fraction = clamp(scattered.path_m / MAX_AIR_PATH_M, 0.0, 1.0);

    // THE ONE PIECE OF THE REFERENCE WE DO NOT SHIP: its `pow(color, 1/2.2)`
    // (`atmosphere.fragment.glsl:170`), whose own comment calls it a visual
    // aid rather than a correction -- "Gamma-correct the alpha channel as
    // well (blending itself will not be gamma correct, so doing this helps
    // visuals a bit)".
    //
    // It helps against SPACE and it destroys the planet, because the
    // framebuffer this blends into is sRGB-encoded (`map_view.rs` picks the
    // surface format with `remove_srgb_suffix`, blending in sRGB deliberately)
    // and gamma-encoding an ADDITIVE term before adding it to an already
    // encoded background multiplies it by up to ten at low levels. Measured
    // at the shared 512x512 z1.5 camera, ocean `#4f7499` on black space,
    // default sun -- and
    // `the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet`
    // is the shipped row of this table as assertions:
    //
    //   gamma applied to        disc centre     rim over space   rim / centre
    //   rgb and alpha (ref)    (139,200,255)     (143,175,186)      1.8x
    //   alpha only             ( 84,134,183)     ( 71,112,128)      4.3x
    //   neither (shipped)      ( 75,120,165)     ( 71,112,128)     10.7x
    //
    // Those three rows PREDATE the path-length factor above, which then
    // moved the third one: the shipped disc centre is **(79,116,154)** now,
    // while the brightest pixel of the rim over space went from 142 to 131.
    // The table is left exactly as it was measured, because what it
    // compares is the three GAMMA choices against each other, and the
    // factor scales every one of them by the same per-pixel number -- it
    // does not re-open the question this table settled.
    //
    // (rim / centre is each row's gap from the colour it is drawn over. Only
    // the three rows above are measured; nothing below is arithmetic dressed
    // up as a reading.) The reference's row is not a subtle wash: the blue
    // channel CLIPS at 255 across the whole disc, so a styled basemap loses
    // its own colours -- and it loses MORE of them the darker it is, because
    // the emitted light `S` does not depend on what is behind it while the
    // background's own contribution `dst * (1 - A)` shrinks with `dst`. A
    // near-black basemap like `mapbox-standard-dark`'s `#0b0e13` therefore
    // receives the same added light the ocean above does, on top of almost
    // nothing. That was not measured on that background and no figure for it
    // is quoted.
    //
    // Dropping the gamma does not dim the rim -- over space the two lower
    // rows are identical, because the rim's radiance is already saturated by
    // the exposure above -- it only stops the thin haze in FRONT of the
    // planet being amplified. And `atmosphere-blend` cannot substitute for
    // that choice: it multiplies the whole premultiplied result, so it scales
    // the rim's emitted light and the wash's by the SAME factor and dims them
    // together. It is a volume knob, not a shape.
    //
    // What we give up, stated: the emitted light is now linear where the
    // buffer is encoded, so the rim is dimmer than a physically correct
    // composite would make it. That error is ZERO where the background is
    // black -- which is exactly where the effect is supposed to be strong --
    // and largest where the background is bright, which is exactly where it
    // is supposed to be subtle. The sun intensity above is a free parameter
    // anyway, and raising it cannot undo the reference's row: the rim is
    // already at the top of the exposure curve and only the centre would move.
    //
    // PREMULTIPLIED alpha, which IS the reference's: rgb is light emitted
    // along the ray and `1 - opacity` is how much of the background the air
    // blocks -- two independent quantities. `ColorMode.alphaBlended` is
    // `[ONE, ONE_MINUS_SRC_ALPHA]` (`webgl/color_mode.ts:31`). Multiplying the
    // whole vec4 by the blend is therefore a uniform dimming, which it would
    // not be under straight alpha.
    return vec4<f32>(exposed, 1.0 - color.a) * air.blend * path_fraction;
}
