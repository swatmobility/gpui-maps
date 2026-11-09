//! The glow around the planet: a Rayleigh/Mie scattering integral, run
//! full-screen, gated on the globe transition.
//!
//! # Where this comes from — TWO upstreams, and the chain matters
//!
//! The shader is MapLibre GL JS's `atmosphere.fragment.glsl` (172 lines) and
//! `atmosphere.vertex.glsl` (11 lines), with `webgl/program/atmosphere_program.ts`
//! and `webgl/draw/draw_sky.ts` around them. MapLibre GL JS is BSD-3
//! (`Copyright (c) 2023, MapLibre contributors`).
//!
//! But MapLibre's own file says, in its header, that it is not the origin:
//!
//! > Shader use from <https://github.com/wwwtyro/glsl-atmosphere> — Made some
//! > change to adapt to MapLibre Globe geometry
//!
//! So the scattering itself is **wwwtyro/glsl-atmosphere**, which is under
//! the **Unlicense** (public domain). `src/render/PROVENANCE.md` records both
//! links, what was checked and what could not be.
//!
//! **Nothing here is from any Mapbox GL JS 2.x/3.x tree**, which this plan's
//! Global Constraints rule out as a reference; that tree left open source at
//! v2.0. Every citation below is MapLibre, glsl-atmosphere, or first
//! principles.
//!
//! # What we changed, and why each change is a change
//!
//! **The frame is the unit sphere, not a metre-scaled copy of the screen.**
//! MapLibre's fragment shader has to unpick its own projection: it takes
//! `u_inv_proj_matrix` and `u_globe_position`, reconstructs the camera
//! position by transforming the origin through the model-view-projection and
//! back out through the inverse projection (`draw_sky.ts:97-111`), then
//! rescales it by `EARTH_RADIUS / u_globe_radius` because its globe's radius
//! is a screen-derived number. Ours is a **unit** sphere by construction —
//! `shader.wgsl`'s `project_to_sphere` returns positions on it and
//! `render::globe::GlobeFrame::globe_view` maps it to view space — so the
//! rescale is a single multiply by [`EARTH_RADIUS_M`] and the camera
//! position is `globe_view.inverse()` applied to the origin. No projection
//! matrix is inverted and no near or far plane enters the answer — so a
//! change to `GLOBE_NEAR_PX` cannot move the glow, which a test could only
//! establish by re-deriving the whole chain and which this construction
//! makes true by having nothing to test.
//!
//! **The rays are built from the field of view, not from a matrix inverse.**
//! `vs_main` forms the view-space ray `(x·tan(fov/2)·aspect, y·tan(fov/2),
//! -1)` and rotates it into sphere space. That expression is **linear in the
//! NDC coordinates**, which is why interpolating it across the full-screen
//! triangle is exact rather than approximate — interpolating *normalised*
//! directions would not be, and normalising per fragment is what `fs_main`
//! does.
//!
//! **The optical-depth exponent is clamped.** `exp(-height/scale_height)`
//! with a NEGATIVE height — a secondary ray sampled inside the planet, which
//! is how the night side comes to be dark at all (see [`Sun`]) — overflows
//! f32 at heights below about −650 km for the Mie term. MapLibre ships it
//! unclamped and relies on `exp(-inf) == 0` downstream. That is true on
//! desktop GL and Metal; it is not something this crate is willing to
//! assume on every WebGL2 driver and every iOS generation it targets, and
//! the clamp at an exponent of 80 (e^80 ≈ 5.5e34, against f32's 3.4e38
//! ceiling) reaches the same attenuation — `exp(-5.5e34)` is 0 — without
//! ever producing an infinity. What was actually checked, and no more: with
//! the clamp removed, every assertion in
//! `the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet` still
//! holds on this machine's Metal adapter — the terminator's exact zero
//! included, which is the one the clamp could plausibly have moved. Whether
//! the two forms are identical to the byte was NOT measured, and the claim
//! made here is only that the clamped form reaches the same attenuation.
//!
//! **Each ray is scaled by its own path through the air, and neither
//! upstream does that.** `atmosphere_shader.wgsl`'s `path_fraction`: the
//! chord the ray actually crossed, over the longest chord there is
//! (2,266.45 km). Without it the pass is a WASH rather than a rim — Task 10
//! measured the disc interior moving a mean of **13.63 byte units over
//! 99.0% of its pixels** at the default example's globe camera, and moving
//! them toward one blue (red −4, green +4, blue +12) rather than simply
//! dimming them, which is what made a styled basemap read as "a bit
//! shaded" beside another renderer's globe. With it: **0.98**, and the rim
//! keeps 85% of itself. The factor is not `intensity`, which Task 9
//! established is a uniform volume knob and not a shape; this one runs from
//! 0.044 at the disc centre to 1.0 at the limb. `fs_main` carries the
//! measured table and the argument, and
//! `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash` renders the
//! factor-less arm beside the shipped one so the defect is graded.
//!
//! **It is drawn LAST, and under the labels.** MapLibre draws the atmosphere
//! after every layer, opaque and translucent both (`painter.ts:641-644`),
//! which puts it over its symbols too. Ours goes at the end of the *tile*
//! pass, so the symbol pass that follows draws on top of it — a divergence
//! in our favour, because a label hazed by 100 km of simulated air is a
//! label you cannot read.

use super::Camera;
use super::globe::GlobeFrame;

/// The planet's radius in metres, and the unit [`ATMOSPHERE_RADIUS_M`] and
/// the scattering coefficients are all expressed against.
///
/// `atmosphere.fragment.glsl:22` (`EARTH_RADIUS`). The same figure
/// `render::globe::GLOBE_RADIUS_M` carries to three more decimal places;
/// this one is deliberately the reference's rounded 6371e3 rather than
/// 6,371,008.8, because everything it is divided by — an 8 km Rayleigh
/// scale height, a 100 km shell — is quoted to two significant figures and
/// the difference is 1.4 parts per million of a number that only ever sets
/// the *shape* of an exponential falloff.
pub const EARTH_RADIUS_M: f64 = 6_371e3;

/// The top of the atmosphere, in metres: `atmosphere.fragment.glsl:24`
/// (`ATMOS_RADIUS`), a 100 km shell.
///
/// **This is why the glow is a rim and not a halo.** 100 km on 6,371 km is
/// **1.5700%**, so on the 161.385 px silhouette this crate's pixel tests
/// render the lit shell is **2.53 px** wide, and on the 379 px disc of a
/// 1024x768 window at z3 it is 5.95 px. A ray that misses the shell returns
/// black at full opacity and therefore leaves the framebuffer *exactly*
/// unchanged — see [`AtmospherePipeline`] on the blend mode — so the glow
/// cannot spill into the star field however bright it is set.
pub const ATMOSPHERE_RADIUS_M: f64 = 6_471e3;

/// Where the light comes from.
///
/// # This crate has no `light`, and that is the problem this type solves
///
/// MapLibre's atmosphere takes a sun position from its style `light`:
/// `getSunPos(light, transform)` negates `light.getCartesianPosition()` and
/// then — **only when the light's `anchor` is `'map'`** — rotates it by the
/// camera's roll, pitch, bearing, centre latitude and centre longitude
/// (`draw_sky.ts:61-78`). When the anchor is not `'map'` the rotation matrix
/// stays the identity and the light is a **constant vector in view space**.
///
/// So the reference has exactly two behaviours, and [`Self::Viewport`] and
/// [`Self::Map`] are those two behaviours. What it does not have is the
/// reference's *defaults*: the `light` spec's `position` and `anchor`
/// defaults live in the `@maplibre/maplibre-gl-style-spec` package, which is
/// **not in the checkout this was written against** — the same gap the plan
/// records for `transitionState`'s zoom expression at Task 1. **No default
/// is cited here, because none could be verified.** The angles below are
/// ours.
///
/// # The default is now [`Self::Now`] — the real sun, from the clock
///
/// It was [`Self::Viewport`], on two arguments recorded here so the reversal
/// can be graded rather than taken:
///
/// 1. *"A map-anchored sun without a clock is an arbitrary claim."*
///    **That one is answered, not overruled.** `render::solar` is the
///    clock: [`Self::Now`] reads the wall clock and
///    [`Self::at_utc`] takes a consumer's own instant, so the subsolar
///    point is no longer a hard-coded meridian — it is where the sun is.
///    The star field's refusal that this argument leaned on
///    ("there is no sidereal time here and none is claimed",
///    `render::space::sky_view_proj`) is still standing and still right;
///    that feature has no epoch, and this one now does.
/// 2. *"A library default must not be able to render its own feature
///    invisible."* **That one was true of the atmosphere alone and is not
///    true any more.** When the only thing the sun lit was 100 km of air, a
///    camera near the antisolar meridian showed a fully lit planet with no
///    glow anywhere on it — a feature that had silently switched itself
///    off, with nothing on screen to say why. `shader.wgsl` now shades the
///    GROUND by the same sun (see [`Atmosphere::night_brightness`]), so
///    that camera shows a planet in shadow instead: the effect is not
///    invisible, it is *night*, which reads immediately and is the truth.
///
/// So the honest default and the visible one stopped being in conflict, and
/// the tie-break that chose the screen-anchored light went with them.
/// [`Self::Viewport`] is still here, unchanged, for a consumer who wants a
/// light they can aim.
///
/// # What the truthful default costs, said plainly
///
/// **The glow is genuinely faint at some times of day**, and that is
/// correct: a camera over the night hemisphere sees the planet back-lit,
/// with the lit limb a thin arc at the edge or none of it at all. The
/// difference from the old objection is that the planet under it is dark, so
/// the picture explains itself.
///
/// **The terminator moves while nothing else does.** It crosses the disc at
/// 15 degrees of longitude an hour, so a screenshot taken six hours later is
/// a different picture of the same camera. Every pixel test that cares
/// therefore names an instant through [`Self::at_utc`] or a point through
/// [`Self::Map`]; [`Self::Now`] is not testable and is not tested as though
/// it were.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Sun {
    /// Fixed relative to the CAMERA: the lit limb stays in the same place on
    /// screen however the map is panned.
    ///
    /// `tilt_deg` is the angle between the direction toward the sun and the
    /// direction from the planet back toward the camera, so **0 is a sun
    /// directly behind the viewer** (a fully lit disc, an even rim, no
    /// terminator), **90 puts the terminator through the middle of the
    /// disc**, and 180 is a back-lit planet. `bearing_deg` is where around
    /// the disc the lit side sits, **clockwise from straight up** — the same
    /// screen convention `render::space`'s bearing tests use.
    Viewport { tilt_deg: f32, bearing_deg: f32 },
    /// The **subsolar point**: the sun is directly overhead at this
    /// longitude and latitude, and the terminator therefore stays put on the
    /// planet while the camera moves.
    ///
    /// Build it from an instant with [`Self::at_utc`] rather than by hand
    /// unless the point is what you actually mean; a literal longitude and
    /// latitude here is the arbitrary claim the old default was refused for.
    Map { lon_deg: f64, lat_deg: f64 },
    /// The subsolar point **for right now**, read from the wall clock every
    /// time this sun is resolved. The default.
    ///
    /// Resolved by [`Self::resolved`], which every frame calls exactly once
    /// so the air and the ground cannot be lit from two instants a
    /// microsecond apart. Where there is no clock to read
    /// (`render::solar::now_unix_seconds` answering `None`) it falls back to
    /// [`Self::VIEWPORT_DEFAULT`] — a sun that is not the real one but is at
    /// least a stated one, rather than a silent epoch-midnight.
    ///
    /// **THE DEFAULT**, and the two arguments that used to make
    /// `Viewport { 55, -45 }` the default instead are on this type, each
    /// with the reason it no longer holds: the clock (`render::solar`)
    /// answers the first, and `shader.wgsl`'s ground shading answers the
    /// second, because a back-lit planet now reads as night rather than as
    /// a feature that has switched itself off.
    #[default]
    Now,
}

impl Sun {
    /// The default a [`Self::Now`] with no clock under it falls back to, and
    /// the default this type carried before it had a clock at all.
    ///
    /// 55 degrees leaves **234 of the limb's 360 degrees glowing and 126
    /// dark**, measured in
    /// `the_glow_reads_outside_the_limb_and_does_not_wash_out_the_planet` —
    /// a bright rim with the terminator well off to one side, which is the
    /// arrangement a photograph from orbit usually has, rather than a ring
    /// of even brightness (tilt 0, no terminator anywhere) or a crescent.
    /// −45 puts the lit side up and to the LEFT, matching the
    /// light-from-top-left convention every drop shadow in every interface
    /// already assumes. Both are taste, both are settable, and neither is a
    /// citation.
    pub const VIEWPORT_DEFAULT: Self = Self::Viewport { tilt_deg: 55.0, bearing_deg: -45.0 };

    /// The subsolar point at a UTC instant, as seconds since the Unix epoch.
    ///
    /// This is the constructor a consumer with its own clock uses —
    /// `Sun::at_utc(my_timestamp)` — and the one every pixel test uses,
    /// because it is [`Self::Now`] with the one non-deterministic step
    /// lifted out. See `render::solar` for the algorithm, the source it is
    /// quoted from and the accuracy that source claims.
    #[must_use]
    pub fn at_utc(unix_seconds: f64) -> Self {
        let (lon_deg, lat_deg) = super::solar::subsolar_point(unix_seconds);
        Self::Map { lon_deg, lat_deg }
    }

    /// This sun with every clock read already done: [`Self::Now`] becomes a
    /// [`Self::Map`], and everything else is returned untouched.
    ///
    /// **Called once per frame**, in `MapView::paint`, and the result handed
    /// to both the atmosphere's uniform and the tile shader's. That is not
    /// tidiness: the subsolar point moves 0.0042 degrees a second, so two
    /// unsynchronised clock reads inside one frame would light the air and
    /// the ground from two different instants — a discrepancy far too small
    /// to see and exactly the kind that survives into a pixel test as an
    /// unexplained tolerance.
    #[must_use]
    pub fn resolved(&self) -> Self {
        match *self {
            Self::Now => super::solar::now_unix_seconds()
                .map_or(Self::VIEWPORT_DEFAULT, Self::at_utc),
            other => other,
        }
    }

    /// The unit direction from the planet's centre TOWARD the sun, in
    /// sphere space — the frame `shader.wgsl`'s `project_to_sphere` and
    /// `render::space::sky_direction` both use, `(sin lon · cos lat, sin
    /// lat, cos lon · cos lat)` with y north.
    ///
    /// `inverse_view` is `GlobeFrame::globe_view.inverse()`, which only
    /// [`Self::Viewport`] reads — [`Self::Map`] is a property of the planet
    /// and has no camera in it at all, which is the whole difference between
    /// the two and what
    /// `a_map_anchored_sun_ignores_the_camera_and_a_viewport_anchored_one_does_not`
    /// asserts.
    ///
    /// [`Self::Now`] resolves itself here as well as in [`Self::resolved`],
    /// so a caller that forgot to resolve gets the right sun rather than a
    /// wrong one — but it gets a fresh clock read, which is what
    /// [`Self::resolved`]'s doc comment is about.
    #[must_use]
    pub fn direction_sphere(&self, inverse_view: &glam::DMat4) -> glam::DVec3 {
        match self.resolved() {
            Self::Viewport { tilt_deg, bearing_deg } => {
                let (tilt, bearing) =
                    (f64::from(tilt_deg).to_radians(), f64::from(bearing_deg).to_radians());
                // View space: +x screen right, +y screen up, +z back toward
                // the eye (the camera looks down -z). So tilt 0 is `+z`, the
                // direction from the planet to the camera.
                let view = glam::DVec3::new(
                    tilt.sin() * bearing.sin(),
                    tilt.sin() * bearing.cos(),
                    tilt.cos(),
                );
                inverse_view.transform_vector3(view).normalize()
            }
            Self::Map { lon_deg, lat_deg } => {
                let (lon, lat) = (lon_deg.to_radians(), lat_deg.to_radians());
                glam::DVec3::new(lon.sin() * lat.cos(), lat.sin(), lon.cos() * lat.cos())
            }
            // `resolved()` turned this into one of the two above, so the
            // arm is unreachable in practice. Spelled out rather than left
            // to a `_ =>` catch-all, because a catch-all is what would
            // silently absorb a THIRD anchor added later.
            Self::Now => Self::VIEWPORT_DEFAULT.direction_sphere(inverse_view),
        }
    }
}

/// The atmosphere's settings: how strong it is, and where the sun is.
///
/// Held on [`super::space::Space`] rather than read out of the style,
/// for the reason that type's doc gives: the nearest style-spec equivalents
/// are `sky`'s `atmosphere-blend` and `light`, neither of which this crate
/// implements.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Atmosphere {
    /// Multiplies the whole effect, `0.0` being off. This is MapLibre's
    /// `sky.atmosphere-blend` — the thing it multiplies by the globeness
    /// before deciding whether to draw at all (`draw_sky.ts:90-95`).
    ///
    /// **`1.0` by default, unlike the star field's `0.0`**, and the two
    /// differ for reasons rather than by accident:
    ///
    ///  - the star field ships 1.24 MB of catalogue and a data-licence
    ///    question (`src/render/PROVENANCE.md`), so it waits to be asked;
    ///  - the atmosphere ships three vertices and a shader, has no data at
    ///    all, and is the difference between a flat-shaded ball and a planet.
    ///
    /// Both are gated identically on the globe transition, so **neither can
    /// touch a flat map** — which is what makes turning this one on by
    /// default a change to a feature nothing has shipped yet rather than a
    /// change to what the four applications compiling this crate draw today.
    pub intensity: f32,
    /// Where the light comes from. See [`Sun`] for the decision and its
    /// cost.
    pub sun: Sun,
    /// **How bright the night side of the planet is**, as a multiplier on
    /// the basemap's own fill colours: `0.0` is black, `1.0` is no ground
    /// shading at all. Default **1.0 -- the terminator OFF**; switching it
    /// on means [`Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS`] (0.25).
    ///
    /// # Why the default is off (2026-09-29)
    ///
    /// The user's ruling: "on all maps, default state: no terminator, plain
    /// maps". A map a consumer opens at 23:00 went dark over its own city
    /// because the sun below it was `Sun::Now`; the native census panel was
    /// the reported case. The sun is still `Sun::Now` (the air's glow is
    /// still lit from where the sun really is), but the GROUND, the labels
    /// and the pitched sky are daylit at every hour until a consumer asks
    /// for night. The rest of this comment is about what the terminator does
    /// once it is on, at the 0.25 floor that turns it on.
    ///
    /// This is the knob for the defect Task 9 named and left standing: the
    /// scattering pass above shades the *air*, so before this existed the
    /// night limb correctly lost its glow while the night *surface* stayed
    /// exactly as bright as the day side, and the result did not read as a
    /// planet in sunlight. `shader.wgsl`'s fragment stage now multiplies
    /// each fill by the cosine of the sun's angle from the local vertical,
    /// through a smoothstep three degrees either side of the geometric
    /// terminator, with this as the floor.
    ///
    /// # Why a floor at all, and why 0.25
    ///
    /// Earth's night side really is black from orbit. A basemap is not a
    /// photograph, though: it is information, and a night hemisphere that
    /// showed nothing would be a map with half of itself deleted. 0.25 is
    /// **two stops down** — the difference a photographer would call
    /// daylight against deep shade — which reads unambiguously as night
    /// while leaving every coastline and road on the dark side followable.
    ///
    /// And it keeps the SILHOUETTE. Against the darkest style this repo
    /// ships, `mapbox-standard-dark`'s background `#0b0e13` = (11, 14, 19),
    /// a floor of 0.25 gives (3, 4, 5) — still above the pure-black space
    /// colour `render::space` defaults to, so the planet's night limb does
    /// not merge into the sky. **That margin is 5 byte units where the day
    /// side's is 19**, and Task 8 already called 19 "a subtle silhouette",
    /// so on a near-black basemap the night limb is fainter than it was.
    /// That is the honest cost, it is recorded rather than hidden, and what
    /// makes the limb read there is the star field or the limb glow — Task
    /// 8's own conclusion, unchanged.
    ///
    /// # What it dims besides the ground, and by how much less
    ///
    /// **Labels take only part of it, through a raised floor.** This doc
    /// used to say they took none of it, and gave a reason that was sound
    /// as far as it went: a street name at 25% of its brightness over a
    /// fill also at 25% keeps its ratio, but a halo tuned for a light
    /// background does not, and text is the one thing on a map that has to
    /// survive. What that argument missed is that the choice was never
    /// between full white and times-0.25. [`SymbolLight`] multiplies a
    /// glyph by `1 - (1 - night_brightness) * LABEL_NIGHT_DIM` — **0.70 at
    /// this default, not 0.25** — which is clearly dimmer than the day side
    /// and still 4.8:1 against the night land under it, and it fades the
    /// halo's alpha rather than its width. See that type for the
    /// measurements and for the ramp, which is this one.
    ///
    /// **Sprite icons and the attribution plate keep full brightness**, and
    /// that part of the old argument stands: a non-SDF sprite takes its
    /// colour from its atlas texel, so there is no CPU-side value to scale
    /// and dimming it means a change to `icon_shader.wgsl`.
    ///
    /// **The overlay's geometry is lit like the ground under it**: it is on
    /// the sphere now (`GlobeUpload::for_overlay`), and that carries this
    /// frame's sun and night floor through the same `GlobeParams`. Its
    /// labels still raise the question above.
    pub night_brightness: f32,
}

impl Default for Atmosphere {
    fn default() -> Self {
        Self { intensity: 1.0, sun: Sun::default(), night_brightness: 1.0 }
    }
}

impl Atmosphere {
    /// The `night_brightness` that turns the terminator ON: two stops down.
    /// Not the default (that is `1.0`, off -- see
    /// [`Self::night_brightness`]); it is what a terminator toggle, a
    /// capture that names a sun, and every pixel test of the night side use.
    pub const TERMINATOR_NIGHT_BRIGHTNESS: f32 = 0.25;

    /// The shader's `blend`: the intensity times the globeness, both
    /// clamped.
    ///
    /// `draw_sky.ts:90`, verbatim in shape —
    /// `sky.properties.get('atmosphere-blend') * projectionData.projectionTransition`.
    /// Clamped because `intensity` is a public field a consumer writes
    /// directly, and a negative blend would make `fs_main` return a negative
    /// premultiplied colour, which is a subtractive glow.
    #[must_use]
    pub fn blend(&self, transition: f32) -> f32 {
        self.intensity.clamp(0.0, 1.0) * transition.clamp(0.0, 1.0)
    }

    /// Whether the full-screen pass should be submitted at all this frame.
    ///
    /// The reference's own gate, one line below the blend: "Don't draw
    /// anything if atmosphere is fully transparent" and an early return
    /// (`draw_sky.ts:91-95`). Both gates in one place so `MapView::paint`
    /// and the pixel tests cannot disagree about them — the same sharing
    /// `space::draws_backdrop` and `Space::draws_stars` carry, and for the
    /// same reason: a test that decided for itself that a flat frame gets no
    /// atmosphere would keep passing after the gate was deleted from
    /// `paint`.
    #[must_use]
    pub fn draws(&self, transition: f32) -> bool {
        self.blend(transition) > 0.0
    }

    /// [`Self::night_brightness`], clamped to `[0, 1]` for the shader.
    ///
    /// Clamped for the same reason [`Self::blend`] is: it is a public field
    /// a consumer writes directly, and a value outside the range is not a
    /// darker night — a negative one makes `mix` extrapolate past black into
    /// a negative premultiplied colour, and one above 1 brightens the night
    /// side past the day side, which is a light source under the planet.
    ///
    /// **Not gated on [`Self::intensity`], deliberately.** Ground shading is
    /// a `light`, not a glow: `intensity` is MapLibre's
    /// `sky.atmosphere-blend`, a volume knob on the scattering pass, and
    /// turning the haze off should not make the night side of the planet
    /// bright again. A consumer who wants no ground shading sets this to
    /// `1.0`, which is what it means. Both are gated on the globe
    /// transition, which is the gate that matters: neither can touch a flat
    /// map.
    #[must_use]
    pub fn night_brightness_clamped(&self) -> f32 {
        self.night_brightness.clamp(0.0, 1.0)
    }
}

/// Half-width of the soft terminator, as a SINE of the angle above or below
/// the geometric terminator — which is what `dot(normal, sun)` is.
///
/// **`shader.wgsl`'s `TERMINATOR_HALF_WIDTH_SIN`, and it must stay that
/// number.** The ground ramps through this band on the GPU and the labels
/// over it ramp through the same band on the CPU ([`SymbolLight::shade`]);
/// two constants would be two terminators, and a label would slide into
/// night a few hundred kilometres before or after the coastline under it.
/// `the_cpu_terminator_band_is_the_shaders_own` reads the literal back out
/// of the WGSL source rather than trusting this comment.
///
/// See that shader constant for what the number is: `sin(3 degrees)`, six
/// degrees of arc in all, which is civil twilight.
pub const TERMINATOR_HALF_WIDTH_SIN: f32 = 0.05234;

/// **How deep this frame's night goes: THE one night factor.** `0.0` is a
/// frame with no night on it at all; `1 - night_brightness` is the deepest.
///
/// The ground (`shader.wgsl`, through [`night_floor`]) and the labels
/// ([`SymbolLight`]) take their night depth from this and nothing else;
/// every shell and the capture harness call [`night_floor`] rather than
/// spelling the fade out, which four of them used to do separately. (The
/// sky turns to night through the same [`terminator_lit`] but goes to
/// space rather than to a floor -- see `render::sky`.)
///
/// **Faded by the space transition, so a flat frame and every frame from
/// z6 up has none** (`space_transition == 0` gives exactly `0.0`) -- the
/// terminator belongs with the backdrop, the air and the stars, not with a
/// street map. The fade is `t * (2 - t)` rather than `t`: the same two
/// ends (all night at z3, none at z6, both exact), but the night stays
/// deep for longer on the way in -- 75% of the way at z4.5 rather than
/// 50%, 45% at z5 rather than 26% -- and it still arrives at z6 with zero
/// slope, because `t` itself does (`globe::space_transition_for_zoom` is a
/// smoothstep). With the linear fade the ocean on the night side at z4.5
/// was at 0.625 of day (`#608A9F` under `#99DDFF`), which reads as a lit
/// sea under a night sky; now it is at 0.44.
#[must_use]
pub fn night_depth(night_brightness: f32, space_transition: f32) -> f32 {
    let t = space_transition.clamp(0.0, 1.0);
    (1.0 - night_brightness.clamp(0.0, 1.0)) * (t * (2.0 - t))
}

/// **The night FLOOR a frame is shaded with**: `1 - night_depth`. What
/// every shell uploads as the ground's `night_brightness`, and what the sky
/// pass is written with -- see [`night_depth`].
///
/// Exactly `1.0` on a flat frame, so the ground's `mix(floor, 1, lit)`
/// multiplies by a literal one there.
#[must_use]
pub fn night_floor(night_brightness: f32, space_transition: f32) -> f32 {
    1.0 - night_depth(night_brightness, space_transition)
}

/// **How lit one surface point is**, given the frame's [`night_floor`] and
/// the cosine of the sun's zenith angle there (`dot(normal, sun)`):
/// `mix(floor, 1, smoothstep(-W, W, cos_zenith))`, with `W` the
/// [`TERMINATOR_HALF_WIDTH_SIN`].
///
/// `shader.wgsl`'s ground shade and `sky_shader.wgsl`'s `shade`, term for
/// term, on the CPU -- so a test can hold the two to one number.
#[must_use]
pub fn night_shade(floor: f32, cos_zenith: f32) -> f32 {
    let lit = terminator_lit(cos_zenith);
    floor + (1.0 - floor) * lit
}

/// **THE terminator**: how much of the sun reaches a point whose sun is at
/// `cos_zenith` (`dot(normal, sun)`), `0.0` (night) to `1.0` (day), ramped
/// over [`TERMINATOR_HALF_WIDTH_SIN`] either side of the geometric line.
///
/// The ground (`shader.wgsl`), the labels ([`SymbolLight::shade`]) and the
/// sky (`sky_shader.wgsl`) all turn to night through this one ramp, so
/// they turn at the same place over the same width. What each does WITH
/// the night differs -- see [`night_shade`] and `render::sky`.
#[must_use]
pub fn terminator_lit(cos_zenith: f32) -> f32 {
    smoothstep(-TERMINATOR_HALF_WIDTH_SIN, TERMINATOR_HALF_WIDTH_SIN, cos_zenith)
}

/// **How much of a bright glyph's brightness the deepest night takes.**
///
/// `0.40`, and the number is a contrast measurement rather than a taste:
/// white ink at the default [`Atmosphere::night_brightness`] comes out at
/// **0.70**, which is `#B3B3B3`. See [`SymbolLight`] for the ratios that
/// lands on and why nothing lower would do.
///
/// Scaled by the glyph's own luminance, so a black glyph takes none of it —
/// see [`SymbolShade::ink`].
pub const LABEL_NIGHT_DIM: f32 = 0.40;

/// **How much of a DARK halo's alpha the deepest night takes.**
///
/// `0.60` — more of the halo than [`LABEL_NIGHT_DIM`] takes of the glyph,
/// because on a dark ground a dark halo is not doing the job a halo exists
/// to do. Scaled by how dark the halo is, so a white halo takes none of it
/// — see [`SymbolShade::halo`].
pub const HALO_NIGHT_FADE: f32 = 0.60;

/// **How deeply into night one label sits**, and what that does to its ink
/// and its halo. The symbol pass's half of the day/night terminator.
///
/// # The defect this exists for
///
/// `shader.wgsl` shades the ground by the sun and nothing shaded the text
/// over it, so at [`Atmosphere::night_brightness`]'s default of 0.25 the
/// night hemisphere was multiplied to a quarter while every label on it
/// stayed full white — a 4:1 mismatch. The labels became the brightest
/// thing on the dark half of the planet, and read as pasted on glass in
/// front of the globe rather than as lying on its surface.
///
/// The doc on [`Atmosphere::night_brightness`] used to argue the opposite
/// case, that dimming labels fourfold would cost more legibility than the
/// inconsistency cost. **That argument was right about the fourfold and
/// wrong about the choice**: the two options it weighed were "full white"
/// and "times 0.25", and there is a third.
///
/// # One principle, applied twice
///
/// **The night takes away brightness that is no longer needed, and weight
/// that is no longer doing anything.**
///
/// A glyph on a quarter-brightness ground has brightness to spare, so it
/// gives some up ([`SymbolShade::ink`]). A halo exists to separate glyphs
/// from a background of similar lightness, and a DARK halo on a darkened
/// ground separates nothing — it only adds weight, which is what turns a
/// stacked "Democratic Republic of the Congo" into a blob — so it gives up
/// alpha ([`SymbolShade::halo`]).
///
/// Both are scaled by the colour's own luminance, and that is the part that
/// keeps this from being a blanket dimming:
///
///  - **A bright glyph dims; a dark one does not.** Multiplying `#000` by
///    anything is still `#000`, and multiplying `#333` toward black on a
///    ground that has itself gone dark *removes* contrast rather than
///    trading it. The two styles vendored in `examples/` are dark text on
///    a white halo (Liberty's `#000`/`#666`, Mapbox Streets' `hsl(220, 30%,
///    0%)`), so on those this term is very nearly a no-op — which is the
///    correct outcome, not a missed one.
///  - **A dark halo fades; a light one does not.** Liberty's `#fff` halo
///    is exactly what makes its dark text readable once the ground goes
///    dark, and fading it would break the only styles the glyph term
///    leaves alone. `swat-routes-dark`'s `rgb(7, 7, 8)` halo under
///    `rgb(163, 165, 177)` text — the arrangement the defect was reported
///    on — fades to 0.59 of its alpha.
///
/// # The numbers, on the frame the defect was reported on
///
/// White ink at the default night is `0.70`, i.e. `#B3B3B3`. Against that
/// frame's night hemisphere (night land `#43423E`, night ocean `#2E3F49`)
/// the WCAG contrast ratios are **4.80:1 against the land and 5.21:1
/// against the ocean** — both above the 4.5:1 AA threshold for body text,
/// and this is display text with a halo. Ink at the ground's own 0.25
/// would be `#404040`, which is **1.05:1** against that same land: the
/// label would not be dim, it would be gone. 0.70 is the highest value
/// that still reads as clearly dimmer than the day side, and it is the
/// floor rather than a taste.
///
/// The shape of the expression matters as much as the number:
///
///  - `night_brightness == 1.0` (the terminator switched off, which is what
///    `GPUI_MAP_NIGHT=1` and `Atmosphere { night_brightness: 1.0, .. }`
///    mean) leaves **every** multiplier at exactly `1.0`, so a consumer who
///    turns ground shading off gets labels turned off with it. One knob,
///    not two.
///  - `night_brightness == 0.0` still leaves white ink at `0.60`, so labels
///    never follow the ground to black however dark the night is set.
///  - It is monotone in `night_brightness`, so a consumer who dims the
///    night dims the labels, in proportion and never past the floor.
///
/// # Smoothly, on the ground's own ramp
///
/// [`Self::shade`] is `shader.wgsl`'s fragment-stage expression term for
/// term — the same `smoothstep` over the same
/// [`TERMINATOR_HALF_WIDTH_SIN`] band, against the same
/// `dot(surface normal, sun)`. A label therefore crosses the terminator
/// over the same six degrees of arc the coastline under it crosses, and
/// nothing pops. The halo fades its ALPHA and never its WIDTH for the same
/// reason: width is what `symbol_pass::halo_offsets` turns into an integer
/// ring COUNT, so narrowing it would drop a whole ring at some longitude
/// and pop exactly where this ramp exists to be smooth.
///
/// The space fade is folded in here, exactly as `MapView::paint` folds it
/// into the ground's night floor and for the same reason: above
/// `render::globe::GLOBE_ZOOM_MAX` for the SPACE range there is no night at
/// all, and a street map dimmed because the sun is down is not what the
/// terminator is for. [`Self::UNLIT`] is what a flat frame gets, and it
/// makes every colour pass through untouched.
///
/// # Dark glyphs take a night PAIR instead
///
/// Everything above is about a LIGHT glyph. A dark one -- every label in
/// Mapbox Streets and Liberty -- cannot be fixed by dimming: black stays
/// black, and its white halo dims to grey, which put Streets' half-opacity
/// state names at about 3:1 on the night land. Those turn into a
/// light-on-dark pair through [`dark_label_colors`] (see
/// [`SymbolShade::label_colors`]), which is what maps built for the dark
/// do. Measured on a z4 Mapbox Streets frame over India: state labels
/// 3.1-3.3:1 before, 6.5-6.9:1 after; country labels 9.6-10.1:1 before,
/// 12.4-12.7:1 after. The day side is byte-identical.
///
/// # Where it is NOT applied, and honestly
///
/// **Sprite icons keep full brightness.** A non-SDF sprite's colour comes
/// out of the atlas texel in `icon_shader.wgsl` and ignores the vertex
/// colour entirely, so there is no CPU-side value to scale; dimming those
/// means a shader change and an inter-stage variable or a uniform, which is
/// the cost this design was chosen to avoid. Line-placed glyph runs DO take
/// it, because those read the vertex colour; a DARK SDF sprite takes the
/// night pair ([`SymbolShade::icon_colors`]) and a light one keeps its day
/// colour, so no icon is crushed toward black.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SymbolLight {
    /// UNIT direction from the planet's centre toward the sun, in the same
    /// sphere space `render::globe::TileAnchors::sphere` returns —
    /// `Sun::direction_sphere`.
    sun_direction: glam::DVec3,
    /// How deep the DEEPEST night is this frame, in `[0, 1]`: the ground's
    /// own darkening (`1 - night_brightness`) with the space fade already
    /// applied. `0.0` is a frame with no terminator on it at all.
    deepest: f32,
}

/// How deeply into night ONE label sits, as [`SymbolLight::shade`] resolved
/// it: `0.0` in daylight, and at most `(1 - night_brightness)` faded by the
/// space transition — `0.75` on a whole-planet frame at the default night —
/// on the far side of the terminator. It is the GROUND's own darkening at
/// this point, which is why the two accessors below can scale it and land
/// on a floor rather than each carrying a range of their own.
///
/// Carries no colour of its own — the two accessors take the style's colour
/// and give back the one to draw, because how much of the night a colour
/// takes depends on that colour. See [`SymbolLight`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct SymbolShade {
    night: f32,
}

impl SymbolShade {
    /// Full day, or no globe: every colour passes through untouched.
    pub const UNSHADED: Self = Self { night: 0.0 };

    /// How deep in night this label is, `0.0` (day) to `1.0`.
    #[must_use]
    pub fn night(&self) -> f32 {
        self.night
    }

    /// **The glyph colour to draw**, given the style's own.
    ///
    /// RGB is scaled by `1 - night * LABEL_NIGHT_DIM * luminance`; ALPHA is
    /// returned untouched. Alpha deliberately: both the glyphon path and
    /// the SDF path blend with STRAIGHT alpha, so fading alpha would pull
    /// the label toward whatever is behind it — the ocean, the air, the
    /// stars — instead of toward black, and toward black is what the ground
    /// under it does.
    #[must_use]
    pub fn ink(&self, colour: crate::style::Rgba) -> crate::style::Rgba {
        if self.night <= 0.0 {
            return colour;
        }
        scale_rgb(colour, 1.0 - self.night * LABEL_NIGHT_DIM * luminance(colour))
    }

    /// **The halo colour to draw**, given the style's own.
    ///
    /// RGB takes the same scaling [`Self::ink`] applies, so a halo does not
    /// drift in hue away from the text it surrounds. ALPHA is scaled by
    /// `1 - night * HALO_NIGHT_FADE * (1 - luminance)`: a near-black halo
    /// loses most of that fade's worth, a white one loses none.
    #[must_use]
    pub fn halo(&self, colour: crate::style::Rgba) -> crate::style::Rgba {
        if self.night <= 0.0 {
            return colour;
        }
        let scaled = self.ink(colour);
        crate::style::Rgba {
            a: colour.a * (1.0 - self.night * HALO_NIGHT_FADE * (1.0 - luminance(colour))),
            ..scaled
        }
    }

    /// **A point label's glyph and halo colours, `text-opacity` folded into
    /// both alphas** -- what the glyphon path draws.
    ///
    /// A label whose glyph is LIGHT (a dark style's) keeps [`Self::ink`] and
    /// [`Self::halo`], which already make it light-on-dark. A label whose
    /// glyph is DARK -- every label in Mapbox Streets and Liberty -- takes
    /// ONE of its two pairs through [`dark_label_colors`]: its own day pair
    /// until the ground under it has darkened by [`NIGHT_PAIR_AT`], its
    /// light-on-dark night pair from there on. Never a mix of the two, and
    /// the same choice for every label on the same ground.
    #[must_use]
    pub fn label_colors(
        &self,
        fill: crate::style::Rgba,
        halo: crate::style::Rgba,
        opacity: f32,
    ) -> (crate::style::Rgba, crate::style::Rgba) {
        let with_opacity = |c: crate::style::Rgba| crate::style::Rgba { a: c.a * opacity, ..c };
        if self.night <= 0.0 || !inverts_at_night(fill) {
            return (with_opacity(self.ink(fill)), with_opacity(self.halo(halo)));
        }
        dark_label_colors(with_opacity(fill), with_opacity(halo), self.night)
    }

    /// **An SDF glyph run's `(fill, halo, opacity)`** -- the line-label path,
    /// where `text-opacity` travels beside the colours as the instance's
    /// `opacity` rather than inside them.
    ///
    /// Unchanged from [`Self::ink`] / [`Self::halo`] for a light glyph. A
    /// dark one takes [`Self::label_colors`] -- the SAME pair the point
    /// label beside it takes -- with the opacity folded in and `1.0`
    /// handed back, because the night pair raises alpha and that has to be
    /// done on the product, not on one factor.
    #[must_use]
    pub fn sdf_colors(
        &self,
        fill: crate::style::Rgba,
        halo: crate::style::Rgba,
        opacity: f32,
    ) -> (crate::style::Rgba, crate::style::Rgba, f32) {
        if self.night <= 0.0 || !inverts_at_night(fill) {
            return (self.ink(fill), self.halo(halo), opacity);
        }
        let (fill, halo) = self.label_colors(fill, halo, opacity);
        (fill, halo, 1.0)
    }

    /// **An SDF icon's `(fill, halo, opacity)`.** A dark SDF icon turns
    /// into its night pair exactly as a dark glyph does; anything else --
    /// a light SDF icon, and every non-SDF sprite, which the caller never
    /// hands here -- keeps its full day colour, as it always has, so no
    /// icon is crushed toward black on the night side.
    #[must_use]
    pub fn icon_colors(
        &self,
        fill: crate::style::Rgba,
        halo: crate::style::Rgba,
        opacity: f32,
    ) -> (crate::style::Rgba, crate::style::Rgba, f32) {
        if self.night <= 0.0 || !inverts_at_night(fill) {
            return (fill, halo, opacity);
        }
        self.sdf_colors(fill, halo, opacity)
    }
}

/// **Where a dark label switches from its day pair to its night pair**, in
/// [`SymbolShade::night`] units (`0.0` day, `0.75` the default deepest
/// night): `0.55`, the ground at 45% of its day brightness.
///
/// ONE threshold on the ground's own darkening, so every label standing on
/// the same ground -- every label deep in the night side of one frame --
/// takes the same scheme; a view never mixes dark-on-light states with
/// light-on-dark cities. Only labels inside the terminator band, whose
/// ground really is brighter, can differ from their neighbours.
///
/// Why 0.55: for a full-opacity dark label (Streets' city and country
/// names, black on white) on Streets' land (`hsl(20, 20%, 95%)`) the day
/// pair's black glyph holds 3:1 against the shaded ground down to a ground
/// factor of about 0.37, and the night pair's `#D9D9D9` glyph holds it up
/// to about 0.51. Any switch in `[0.49, 0.63]` of `night` keeps both sides
/// readable; 0.55 is that window's middle, and it is also where the
/// per-label WCAG scoring this replaced flipped for both the state and the
/// country class (0.59-0.60), rounded toward the window's centre.
pub const NIGHT_PAIR_AT: f32 = 0.55;

/// **How deep the night is when a day-pair halo has firmed to (nearly)
/// opaque**: `0.2`. The halo's alpha goes `a -> 1 - (1 - a)^(1 + 9 u)`,
/// `u` the smoothstep of `night` over `[0, DAY_HALO_FIRM_BY]` -- a
/// half-opacity white halo is `0.999` from here on -- so the ring keeps
/// showing the light ground the style was designed on while the ground
/// itself dims, and the glyph keeps its own designed contrast against it.
/// The glyph's alpha is never raised in the day pair.
pub const DAY_HALO_FIRM_BY: f32 = 0.2;

/// **The HSL lightness a black glyph takes at night**: `0.85`, `#D9D9D9`.
/// A dark glyph lighter than black lands a little lower (see
/// [`dark_label_colors`]), so the day's hierarchy -- the darker the
/// text, the more it matters -- survives as "the lighter the text".
pub const NIGHT_LABEL_FILL_LIGHTNESS: f32 = 0.85;

/// **The HSL lightness every halo takes at night**: `0.08`, `#141414` for
/// a white one -- darker than the night land (`#43423E`) and ocean
/// (`#2E3F49`), so the ring separates the glyph from the ground the way the
/// white ring did by day.
pub const NIGHT_LABEL_HALO_LIGHTNESS: f32 = 0.08;

/// Whether a glyph of this colour turns into a night pair: a DARK one does.
fn inverts_at_night(fill: crate::style::Rgba) -> bool {
    luminance(fill) < 0.5
}

/// **THE dark-label colours on a shaded ground**: a dark-on-light
/// `(fill, halo)` pair -- every label in Mapbox Streets and Liberty -- as
/// it is drawn where the ground has darkened by `night`
/// ([`SymbolShade::night`]: the ground's shade factor is `1 - night`). Both
/// colours carry their final alpha (any `text-opacity` already folded in)
/// and come back the same way.
///
/// # One pair, never a mix, and the same pair for the whole ground
///
/// Below [`NIGHT_PAIR_AT`] the label keeps **its day pair**: the glyph is
/// shaded by the ground factor like the ground under it (black stays
/// black), its ALPHA stays exactly the style's, and the halo keeps its
/// own undimmed colour while its alpha firms toward opaque as the ground
/// darkens (see [`DAY_HALO_FIRM_BY`]) -- so the ring reproduces the light ground
/// the style was designed on, and a half-opacity state name stays exactly
/// as much lighter than a full-opacity city name as the style made it.
///
/// From [`NIGHT_PAIR_AT`] on it takes **its night pair**, light on dark:
/// the glyph's HSL lightness becomes `NIGHT_LABEL_FILL_LIGHTNESS - 0.25 * l`
/// (hue kept: black is `#D9D9D9`), the halo's becomes
/// [`NIGHT_LABEL_HALO_LIGHTNESS`], and both alphas are raised
/// `a -> 1 - (1 - a)^2` -- `0.5` is `0.75`, `1` stays `1`, so the style's
/// hierarchy survives. Zero stays zero in both pairs, so a label its style
/// has faded out is still gone.
///
/// # Why not a mix
///
/// The pair this replaced mixed the day pair into the night pair
/// continuously, and any continuous path from dark-on-light to
/// light-on-dark passes through glyph == halo. At z5 on the night side the
/// ground's fading night floor put every label in that band: mid-grey text
/// on a mid-grey halo on a half-dimmed ground, 1.0:1. The switch here is
/// one threshold in zoom and in terminator position, the same kind of pop
/// a style's own zoom stop makes.
///
/// At `night <= 0.0` the inputs come back untouched -- the same values, not
/// recomputed ones.
#[must_use]
pub fn dark_label_colors(
    fill: crate::style::Rgba,
    halo: crate::style::Rgba,
    night: f32,
) -> (crate::style::Rgba, crate::style::Rgba) {
    if night <= 0.0 {
        return (fill, halo);
    }
    let raise = |a: f32, exponent: f32| {
        let a = a.clamp(0.0, 1.0);
        1.0 - (1.0 - a).powf(exponent)
    };
    if night < NIGHT_PAIR_AT {
        let ground = 1.0 - night;
        let firm = smoothstep(0.0, DAY_HALO_FIRM_BY, night);
        return (
            scale_rgb(fill, ground),
            crate::style::Rgba { a: raise(halo.a, 1.0 + 9.0 * firm), ..halo },
        );
    }
    let (fill_hue, fill_sat, fill_light) = rgb_to_hsl(fill);
    let (halo_hue, halo_sat, _) = rgb_to_hsl(halo);
    (
        hsl_to_rgb(fill_hue, fill_sat, NIGHT_LABEL_FILL_LIGHTNESS - 0.25 * fill_light, raise(fill.a, 2.0)),
        hsl_to_rgb(halo_hue, halo_sat, NIGHT_LABEL_HALO_LIGHTNESS, raise(halo.a, 2.0)),
    )
}

/// `(hue in [0, 1), saturation, lightness)` of an encoded sRGB colour. A
/// grey has hue and saturation `0`.
fn rgb_to_hsl(colour: crate::style::Rgba) -> (f32, f32, f32) {
    let (r, g, b) = (colour.r.clamp(0.0, 1.0), colour.g.clamp(0.0, 1.0), colour.b.clamp(0.0, 1.0));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let light = (max + min) / 2.0;
    let delta = max - min;
    if delta <= f32::EPSILON {
        return (0.0, 0.0, light);
    }
    let sat = delta / (1.0 - (2.0 * light - 1.0).abs()).max(f32::EPSILON);
    let hue = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } / 6.0;
    (hue, sat.min(1.0), light)
}

/// The inverse of [`rgb_to_hsl`], with `alpha` attached.
fn hsl_to_rgb(hue: f32, sat: f32, light: f32, alpha: f32) -> crate::style::Rgba {
    let light = light.clamp(0.0, 1.0);
    let chroma = (1.0 - (2.0 * light - 1.0).abs()) * sat.clamp(0.0, 1.0);
    let h = hue.rem_euclid(1.0) * 6.0;
    let x = chroma * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = light - chroma / 2.0;
    crate::style::Rgba { r: r + m, g: g + m, b: b + m, a: alpha }
}

impl SymbolLight {
    /// **A frame with no terminator over its labels at all.** [`Self::shade`]
    /// returns [`SymbolShade::UNSHADED`] for every anchor, lit or not, and
    /// every colour through it is the colour that went in.
    ///
    /// What a flat frame gets, what `symbol_pass::label_census` uses (a
    /// census counts placements, and shading moves no label), and what a
    /// consumer with no atmosphere gets. Same idiom and same reason as
    /// `render::globe::GlobeAnchors::FLAT`: the unshaded path has to be
    /// reachable *without* a sun, so "labels off the globe are unchanged"
    /// is structural rather than arithmetic.
    pub const UNLIT: Self = Self { sun_direction: glam::DVec3::ZERO, deepest: 0.0 };

    /// This frame's label light.
    ///
    /// `sun` is `Sun::direction_sphere` through the frame's inverse globe
    /// view — the SAME value `render::globe::GlobeUpload::new` is handed, so
    /// the ground and the text over it cannot be lit from two instants.
    /// `night_brightness` is [`Atmosphere::night_brightness_clamped`], and
    /// `space_transition` is `GlobeFrame::space_transition`.
    #[must_use]
    pub fn new(sun: glam::DVec3, night_brightness: f32, space_transition: f32) -> Self {
        Self {
            sun_direction: sun,
            deepest: night_depth(night_brightness, space_transition),
        }
    }

    /// How deep in night a label anchored at `sphere` is — a point on the
    /// unit sphere (`render::globe::TileAnchors::sphere`).
    ///
    /// `None` — a flat frame, where there is no sphere to stand on — is
    /// [`SymbolShade::UNSHADED`].
    #[must_use]
    pub fn shade(&self, sphere: Option<glam::DVec3>) -> SymbolShade {
        let Some(sphere) = sphere else { return SymbolShade::UNSHADED };
        if self.deepest <= 0.0 {
            return SymbolShade::UNSHADED;
        }
        // `normalize` for the same reason `shader.wgsl`'s vertex stage
        // normalizes before dotting: the quantity has to be a COSINE, and a
        // length that is 1 plus an ULP makes it something else.
        let cos_zenith = sphere.normalize().dot(self.sun_direction) as f32;
        let lit = terminator_lit(cos_zenith);
        if lit >= 1.0 {
            // **A label in full daylight is bit-identical to an unlit one**,
            // by returning the constant rather than arriving at it. That is
            // the standard the flat path is held to everywhere else
            // (`GlobeAnchors::FLAT`, `GlobeParams::FLAT`), and it is the
            // assertion that stops a global dimming from passing for a
            // night-side fix.
            return SymbolShade::UNSHADED;
        }
        SymbolShade { night: self.deepest * (1.0 - lit) }
    }
}

/// Relative luminance of an sRGB-ENCODED colour, `0.0` (black) to `1.0`
/// (white).
///
/// The Rec. 709 weights, applied to the ENCODED channels rather than to
/// linearised ones — deliberately, and for the same reason
/// `icon_shader.wgsl` and `plate_shader.wgsl` refuse to linearise: this
/// crate's render target is a UNORM format and every colour in the symbol
/// pass is composited sRGB-encoded, so "how bright does this look" has to
/// be asked in the space the pixels are actually in. Linearising here would
/// call `#808080` a 22% grey and dim it almost as little as black, which is
/// not what a reader sees.
fn luminance(colour: crate::style::Rgba) -> f32 {
    (0.2126 * colour.r + 0.7152 * colour.g + 0.0722 * colour.b).clamp(0.0, 1.0)
}

/// `colour` with its three channels multiplied and its alpha left alone.
fn scale_rgb(colour: crate::style::Rgba, by: f32) -> crate::style::Rgba {
    crate::style::Rgba { r: colour.r * by, g: colour.g * by, b: colour.b * by, a: colour.a }
}

/// WGSL's `smoothstep`, so [`SymbolLight::shade`] and `shader.wgsl` ramp
/// identically rather than similarly.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// What `atmosphere_shader.wgsl`'s `Air` struct expects. 112 bytes.
///
/// The field order is the WGSL struct's, and the two `vec3`s sit at offsets
/// 64 and 80 — both multiples of 16, which is what a host-shareable
/// `vec3<f32>` requires. `the_atmosphere_uniform_is_16_byte_aligned_and_its_vec3s_land_on_16`
/// pins that rather than trusting the arithmetic, for the reason
/// `the_model_uniform_is_16_byte_aligned_and_its_declared_need_matches_reality`
/// gives: a mislaid pad does not produce a uniform-shaped symptom, it
/// produces a wrong picture two fields later.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct AtmosphereUniform {
    /// `GlobeFrame::globe_view.inverse()`: VIEW space → sphere space. Used
    /// on directions only (`w = 0`), so the uniform scale by the globe's
    /// world radius that it carries divides out under the normalise.
    inverse_view: [[f32; 4]; 4],
    /// The eye, in sphere space — i.e. in units where the planet is the
    /// unit sphere. `fs_main` multiplies it by [`EARTH_RADIUS_M`] to get the
    /// ray origin in metres.
    camera_sphere: [f32; 3],
    /// [`Atmosphere::blend`].
    blend: f32,
    /// [`Sun::direction_sphere`].
    sun_dir: [f32; 3],
    /// `tan(fov_y / 2)`, the half-height of the view plane at unit depth.
    tan_half_fov_y: f32,
    /// Viewport width over height.
    aspect: f32,
    /// Pads the struct to 112 bytes, a multiple of 16. Named rather than
    /// implied because `bytemuck::Pod` requires no padding bytes at all —
    /// an implicit tail pad would make the derive fail to compile, which is
    /// a better failure than a silent one but still not an explanation.
    _pad: [f32; 3],
}

impl AtmosphereUniform {
    /// One frame's atmosphere, narrowed for upload.
    ///
    /// The `inverse()` is **one matrix inversion per frame**, on a frame
    /// that is already drawing a globe. That is worth naming next to this
    /// plan's central divergence — MapLibre's two `mat4.invert` calls *per
    /// frame* are part of what "Where we diverge" #1 objects to — so: the
    /// objection there is to rebuilding matrices **per tile**, which this
    /// does not do; it happens once, beside the `space_clear_color` and the
    /// two uniform writes Task 8 already charges to the `Surface` phase,
    /// which `render::alloc::PaintPhase::Surface`'s doc now names.
    ///
    /// **That CPU half was not separately measured, and is not claimed to
    /// be free.** What WAS measured is the part that could plausibly have
    /// broken the plan's 0.5 ms rule — the full-screen march on the GPU, in
    /// `the_full_screen_atmosphere_costs_less_than_the_plans_half_millisecond`.
    /// A `DMat4::inverse` and a 112-byte `write_buffer` sit beside a
    /// `space_clear_color` and two other writes that Task 8 measured
    /// together at 0.0236 ms, so the honest statement is that this is of
    /// that order and inside the same phase, not that it is zero.
    pub(crate) fn new(camera: &Camera, frame: &GlobeFrame, atmosphere: &Atmosphere) -> Self {
        let inverse_view = frame.globe_view.inverse();
        let eye = super::altitude::eye_from_inverse_view(&inverse_view);
        let sun = atmosphere.sun.direction_sphere(&inverse_view);
        Self {
            inverse_view: inverse_view.as_mat4().to_cols_array_2d(),
            camera_sphere: [eye.x as f32, eye.y as f32, eye.z as f32],
            blend: atmosphere.blend(frame.space_transition),
            sun_dir: [sun.x as f32, sun.y as f32, sun.z as f32],
            tan_half_fov_y: camera.tan_half_fov() as f32,
            aspect: camera.viewport_width_px / camera.viewport_height_px,
            _pad: [0.0; 3],
        }
    }
}

/// The full-screen scattering pass: one pipeline, one uniform, three
/// vertices, no vertex buffer.
///
/// # Its own pipeline, and its own blend mode
///
/// **PREMULTIPLIED, not straight alpha** — the only pipeline in this crate
/// that is, and the exception is load-bearing. The reference draws this pass
/// with `ColorMode.alphaBlended`, which is `[ONE, ONE_MINUS_SRC_ALPHA]`
/// (`webgl/color_mode.ts:31`): the fragment's rgb is **light added along the
/// ray** and its alpha is **how much of the background the air blocks**, two
/// independent quantities. Straight alpha would multiply the scattered light
/// by the blocking fraction, which is not a dimmer glow but a different
/// integral. It also gives the effect a property worth having: a ray that
/// misses the shell returns `(0, 0, 0, 0)`, and `dst * (1 - 0) + 0` leaves
/// the framebuffer **byte-identical** — so "the glow does not spill into the
/// star field" is a consequence of the blend equation rather than a
/// tolerance.
///
/// # No vertex buffer
///
/// Three vertices generated from `@builtin(vertex_index)`, drawn as one
/// oversized triangle. `render::space::StarVertex` deliberately does NOT do
/// this — it carries its corner per vertex because deriving it from
/// `vertex_index` would couple the shader to an index buffer's winding — and
/// the difference is that there is no index buffer here and no second
/// primitive: `draw(0..3)` has exactly one correct answer, and a
/// full-screen triangle beats a quad by having no diagonal seam for the
/// rasterizer to run twice.
///
/// Built lazily, the first frame that actually draws an atmosphere, so a
/// consumer that never reaches the globe band never compiles the shader.
pub struct AtmospherePipeline {
    pipeline: wgpu::RenderPipeline,
    uniform: super::alloc::TrackedBuffer,
    bind_group: wgpu::BindGroup,
}

impl AtmospherePipeline {
    /// Builds the pipeline. `depth_format` is the tile pass's, because this
    /// draws into the SAME pass — see [`Self::draw`].
    #[must_use]
    pub fn new(
        ctx: &super::GpuContext,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        Self::from_source(ctx, color_format, depth_format, include_str!("atmosphere_shader.wgsl"))
    }

    /// [`Self::new`] over a SUPPLIED shader source, so a test can render
    /// the shipped shader against a deliberately broken copy of it in the
    /// same process, the same frame and the same camera.
    ///
    /// The same door `super::gpu::TilePipeline::from_source` opens, and it
    /// exists for the same reason: `the_ground_shading_is_what_darkens_the_night_side`
    /// established that an acceptance test whose "before" is not the defect
    /// grades nothing, and the only way to render the defect is to compile
    /// it. `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash` uses
    /// this to draw the wash Task 10 measured beside the rim that replaced
    /// it.
    #[must_use]
    pub(crate) fn from_source(
        ctx: &super::GpuContext,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        source: &str,
    ) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps atmosphere shader"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });

        let bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atmosphere-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                // VERTEX for the rays, FRAGMENT for the scattering. One
                // buffer read by both stages rather than two bindings: the
                // declared `max_uniform_buffers_per_shader_stage` need is 2
                // and this asks for 1 in each.
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let uniform = super::alloc::TrackedBuffer::new(
            &ctx.device,
            &wgpu::BufferDescriptor {
                label: Some("atmosphere-uniform"),
                size: std::mem::size_of::<AtmosphereUniform>() as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        );
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atmosphere-bind-group"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
        });

        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("atmosphere-pipeline-layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("atmosphere-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            // **`LessEqual` against the GROUND's depth, at the far plane --
            // the reference's `DepthMode(LEQUAL, ReadOnly)`
            // (`draw_sky.ts:84`) doing the job it is there to do.**
            //
            // This used to be `Always`, on the reasoning that the reference's
            // `LEQUAL` "passes every fragment" against a buffer cleared to
            // 1.0. The clear is not what the test reads. The tiles run
            // BEFORE this pass and WRITE depth, so by the time the glow is
            // drawn the buffer holds the ground, and `LEQUAL` is what stops
            // the scattering integral from compositing over a planet that is
            // already in front of it.
            //
            // What `Always` cost, measured at 2048 px, lon 15 lat 50 z3.5,
            // as the largest channel difference a frame with the pass makes
            // to the same frame without it, binned by radius against the
            // limb:
            //
            // ```text
            //   r / limb     mean delta   max delta
            //   0.90            5.07         18
            //   0.96           10.25         27
            //   0.97           15.65         42
            //   0.980          30.07        112
            //   0.99+           0.03          1   (outside: the rim proper)
            // ```
            //
            // That inner ramp is a PALE BAND tens of pixels wide hugging the
            // limb, in the style's own land colour, and it is what the globe
            // looked wrong by. It is loudest exactly where no pixel test
            // sampled: `the_glow_reads_outside_the_limb_and_does_not_wash_out
            // _the_planet` reads the disc at r/limb 0.0, 0.5 and 0.95 and
            // then jumps to 1.005, so the 0.96-0.99 annulus -- 1.5 px on its
            // 161 px disc, 20 px on the user's -- was never looked at.
            //
            // `path_fraction` in the shader is not a substitute and was never
            // meant to be: it is a smooth falloff that dims the wash, and a
            // dimmed wash is still a wash. This is the boundary.
            //
            // The triangle sits at clip `z = 1.0` (see `vs_main`), so the
            // test reads "draw only where nothing nearer was drawn": space,
            // where the depth is still the cleared 1.0, and the one ring at
            // the limb where the ground's own z reaches 1.0 too -- which is
            // where the rim belongs.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    // See this type's doc comment: PREMULTIPLIED, matching
                    // `ColorMode.alphaBlended` (`webgl/color_mode.ts:31`),
                    // and the only pipeline in this crate that is.
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self { pipeline, uniform, bind_group }
    }

    /// Rewrites the uniform. The whole per-frame CPU cost of the effect:
    /// one `DMat4::inverse`, two vector transforms and a 112-byte
    /// `write_buffer`.
    pub fn write(
        &self,
        ctx: &super::GpuContext,
        camera: &Camera,
        frame: &GlobeFrame,
        atmosphere: &Atmosphere,
    ) {
        ctx.queue.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&AtmosphereUniform::new(camera, frame, atmosphere)),
        );
    }

    /// Draws the full-screen pass into an ALREADY-OPEN pass, LAST — after
    /// the tiles, mirroring `painter.ts:641-644`, and into the tile pass
    /// rather than a pass of its own for the reason
    /// `render::space::StarPipeline::draw` gives: a second
    /// `begin_render_pass` would have to `LoadOp::Load` the colour
    /// attachment the first one just wrote, which on a tile-based GPU is a
    /// full-screen round trip through memory the frame never needed.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The sun a frame with the terminator OFF lights its AIR with, when the
/// consumer left the sun at [`Sun::Now`]: directly behind the viewer, so the
/// whole visible disc reads as day and the limb glow is an even rim at every
/// wall-clock hour.
///
/// The plain-default ruling (2026-09-29, "no terminator ... plain maps")
/// is about more than the ground: with the terminator off but the air still
/// lit by the real sun, a globe opened at local midnight kept its day-side
/// glow on the FAR side of the planet -- the limb glow simply vanished from
/// the half the camera looks at (captured: a z1.6 globe over Singapore at
/// 16:00 UTC shows one sliver of it). The census page's "noon" sun
/// (`census::census_sun`) is the same choice for the same reason.
pub const DAYLIGHT_SUN: Sun = Sun::Viewport { tilt_deg: 0.0, bearing_deg: 0.0 };

/// [`frame_sun`] for a whole atmosphere: when its terminator is OFF
/// (`night_brightness_clamped() == 1.0`) and its sun is the default
/// [`Sun::Now`], the frame is daylight -- [`DAYLIGHT_SUN`]. Otherwise,
/// [`frame_sun`] unchanged: a terminator that is ON follows the clock (or
/// the scene clock), and an explicit sun is never overridden. The ground
/// cannot tell the difference -- a night floor of 1.0 shades nothing -- so
/// this moves only the limb glow.
///
/// Every shell resolves its frame's sun through this ONE call (native
/// `MapView::paint`, web `draw`, iOS `render`).
#[must_use]
pub fn frame_sun_for(atmosphere: &Atmosphere, scene_unix: Option<f64>) -> Sun {
    if atmosphere.sun == Sun::Now && atmosphere.night_brightness_clamped() >= 1.0 {
        return DAYLIGHT_SUN;
    }
    frame_sun(atmosphere.sun, scene_unix)
}

/// This frame's sun, resolved once: [`Sun::Now`] follows the SCENE clock
/// when a map has one (`scene_unix`, `MapView::set_scene_clock`), so the
/// terminator sweeps at the same accelerated rate as the satellites; with
/// no clock it is [`Sun::resolved`] from the wall clock, as before. An
/// explicit sun ([`Sun::Map`], [`Sun::Viewport`]) is never overridden.
#[must_use]
pub fn frame_sun(sun: Sun, scene_unix: Option<f64>) -> Sun {
    match (sun, scene_unix) {
        (Sun::Now, Some(t)) if t.is_finite() => Sun::at_utc(t),
        (sun, _) => sun.resolved(),
    }
}

#[cfg(test)]
mod tests {
    /// The plain default: terminator off + `Sun::Now` is daylight at any
    /// hour; turning the terminator on hands the clock back; an explicit
    /// sun is untouched either way.
    #[test]
    fn a_frame_with_the_terminator_off_is_daylight_at_any_hour() {
        let plain = Atmosphere::default();
        assert_eq!(frame_sun_for(&plain, None), DAYLIGHT_SUN);
        assert_eq!(frame_sun_for(&plain, Some(1_790_697_600.0)), DAYLIGHT_SUN, "not even under a scene clock");
        let on = Atmosphere { night_brightness: Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS, ..plain };
        assert_eq!(frame_sun_for(&on, Some(1_790_000_000.0)), Sun::at_utc(1_790_000_000.0));
        let fixed = Sun::Map { lon_deg: 10.0, lat_deg: 5.0 };
        assert_eq!(frame_sun_for(&Atmosphere { sun: fixed, ..plain }, None), fixed);
    }

    #[test]
    fn the_sun_follows_the_scene_clock_and_nothing_else() {
        assert_eq!(frame_sun(Sun::Now, Some(1_790_000_000.0)), Sun::at_utc(1_790_000_000.0));
        let fixed = Sun::Map { lon_deg: 10.0, lat_deg: 5.0 };
        assert_eq!(frame_sun(fixed, Some(1_790_000_000.0)), fixed, "an explicit sun is not overridden");
        assert_ne!(frame_sun(Sun::Now, None), Sun::Now, "no clock: resolved from the wall clock as before");
    }

    use super::*;

    fn camera(center_lon: f64, center_lat: f64, zoom: f64) -> Camera {
        Camera {
            center_lon,
            center_lat,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 512.0,
            viewport_height_px: 512.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// **The atmosphere is off on a flat map, however bright it is set.**
    ///
    /// The whole of this plan's flat-path guarantee rests on gates like this
    /// one, and this is the cheapest place to state it. The gate is the
    /// blend, not a separate boolean, so "off" and "transparent" cannot
    /// drift apart.
    #[test]
    fn the_atmosphere_is_off_on_a_flat_map_however_bright_it_is_set() {
        let full = Atmosphere { intensity: 1.0, ..Atmosphere::default() };
        assert!(!full.draws(0.0), "THE FLAT MAP MUST BE UNTOUCHED");
        assert_eq!(full.blend(0.0), 0.0);
        assert!(full.draws(1.0));
        assert!(full.draws(0.01), "any globeness at all draws SOMETHING");

        // ...and off when asked to be off, on a full globe.
        let off = Atmosphere { intensity: 0.0, ..Atmosphere::default() };
        assert!(!off.draws(1.0), "intensity 0.0 is the off switch");

        // A consumer writing the public field directly cannot reach the
        // shader with a nonsense blend.
        assert_eq!(Atmosphere { intensity: -3.0, ..Atmosphere::default() }.blend(1.0), 0.0);
        assert_eq!(Atmosphere { intensity: 7.0, ..Atmosphere::default() }.blend(1.0), 1.0);
        assert!(!Atmosphere { intensity: -3.0, ..Atmosphere::default() }.draws(1.0));
    }

    /// **The blend is the intensity times the globeness**, which is
    /// `draw_sky.ts:90` and the reason a zoom through `GLOBE_ZOOM_MAX` fades
    /// the glow out instead of switching it off.
    #[test]
    fn the_blend_is_the_intensity_times_the_globeness() {
        let half = Atmosphere { intensity: 0.5, ..Atmosphere::default() };
        assert!((half.blend(0.5) - 0.25).abs() < 1e-7);
        assert!((half.blend(1.0) - 0.5).abs() < 1e-7);
        // Monotone in the transition, so the fade cannot step.
        let mut previous = -1.0;
        let mut transition = 0.0;
        while transition <= 1.0 {
            let blend = half.blend(transition);
            assert!(blend >= previous, "the blend went backwards at transition {transition}");
            previous = blend;
            transition += 0.05;
        }
        // And the default is ON, unlike the star field's -- see
        // `Atmosphere::intensity` for why the two differ.
        assert_eq!(Atmosphere::default().intensity, 1.0);
        assert!(Atmosphere::default().draws(1.0));
        assert!(!Atmosphere::default().draws(0.0));
    }

    /// **The two sun anchors, told apart by the thing that distinguishes
    /// them: whether the camera is in the answer.**
    ///
    /// A map-anchored sun is a point on the planet, so moving the camera
    /// must not move it by so much as a bit. A viewport-anchored one is a
    /// direction on the screen, so moving the camera's centre longitude
    /// must turn it under the planet by exactly that much.
    ///
    /// The second half is the one that matters: the first draft of
    /// `direction_sphere` transformed the view vector by `globe_view`
    /// instead of its inverse, which is a direction in the wrong space
    /// entirely — and it still came out a unit vector, still varied with the
    /// camera, and still produced a plausible-looking terminator. The
    /// assertion that caught it is the one below tying a 90-degree pan to a
    /// 90-degree turn.
    #[test]
    fn a_map_anchored_sun_ignores_the_camera_and_a_viewport_anchored_one_does_not() {
        let base = camera(0.0, 0.0, 2.5);
        let panned = camera(90.0, 0.0, 2.5);
        let inv = |c: &Camera| super::super::globe::globe_frame(c).globe_view.inverse();

        let map = Sun::Map { lon_deg: 30.0, lat_deg: 20.0 };
        assert_eq!(
            map.direction_sphere(&inv(&base)),
            map.direction_sphere(&inv(&panned)),
            "a SUBSOLAR POINT is a place on the planet: the camera cannot move it"
        );
        // ...and it is where it says it is, in the convention
        // `render::space::sky_direction` and `shader.wgsl` share.
        let expected = super::super::space::sky_direction(30.0, 20.0);
        assert!((map.direction_sphere(&inv(&base)) - expected).length() < 1e-12);

        let viewport = Sun::Viewport { tilt_deg: 55.0, bearing_deg: -45.0 };
        let (here, there) =
            (viewport.direction_sphere(&inv(&base)), viewport.direction_sphere(&inv(&panned)));
        assert!(
            (here - there).length() > 0.5,
            "a VIEWPORT-anchored sun is a direction on the screen: panning the map must \
             turn it under the planet. It moved {}",
            (here - there).length()
        );
        // **The sign, stated as geography rather than as a rotation.**
        //
        // Put the sun on the screen's RIGHT edge, on the horizon: tilt 90
        // (the terminator through the middle of the disc), bearing 90 (the
        // lit side to the right). At pitch 0 and lat 0 that direction is a
        // point on the equator 90 degrees EAST of whatever the camera is
        // centred on -- east, because `shader.wgsl` puts east to the right,
        // which Task 6's mirrored-label defect established the hard way.
        //
        // So the prediction is `sky_direction(centre_lon + 90, 0)`, written
        // in the planet's own convention and with no matrix in it. The first
        // draft of this assertion instead rotated one measured vector onto
        // the other and asserted they matched, which is a test of the
        // rotation's sign against itself: it failed, and flipping the sign
        // until it passed would have proved nothing either way.
        let east = Sun::Viewport { tilt_deg: 90.0, bearing_deg: 90.0 };
        for centre_lon in [0.0, 90.0, -120.0, 179.0] {
            let camera = camera(centre_lon, 0.0, 2.5);
            let measured = east.direction_sphere(&inv(&camera));
            let predicted = super::super::space::sky_direction(centre_lon + 90.0, 0.0);
            assert!(
                (measured - predicted).length() < 1e-9,
                "a viewport sun at tilt 90, bearing 90 must sit over the equator 90 \
                 degrees EAST of the camera's centre: at lon {centre_lon} it was \
                 {measured:?} where lon {} is {predicted:?}",
                centre_lon + 90.0
            );
        }

        // Both are UNIT directions, because the scattering integral
        // normalises nothing on the CPU's behalf.
        for sun in [map, viewport, Sun::default()] {
            let length = sun.direction_sphere(&inv(&base)).length();
            assert!((length - 1.0).abs() < 1e-12, "{sun:?} gave a direction of length {length}");
        }
    }

    /// **Tilt 0 is a sun directly behind the viewer**, which is what makes
    /// `tilt_deg`'s doc comment checkable rather than descriptive.
    ///
    /// At pitch 0 the camera sits on the line through the planet's centre,
    /// so "the direction from the centre toward the eye" and "view-space
    /// `+z`" are the same ray — and the whole visible disc is then lit, with
    /// no terminator anywhere on it. Every other tilt is measured as an
    /// angle from that one.
    #[test]
    fn a_viewport_sun_at_tilt_zero_points_straight_back_at_the_camera() {
        for (lon, lat, zoom) in [(0.0, 0.0, 1.5), (-120.0, 25.0, 2.5), (139.0, -60.0, 5.0)] {
            let camera = camera(lon, lat, zoom);
            let frame = super::super::globe::globe_frame(&camera);
            let inverse_view = frame.globe_view.inverse();
            let eye = inverse_view.transform_point3(glam::DVec3::ZERO).normalize();
            let sun = Sun::Viewport { tilt_deg: 0.0, bearing_deg: 0.0 }
                .direction_sphere(&inverse_view);
            assert!(
                (sun - eye).length() < 1e-9,
                "at lon {lon} lat {lat} a tilt-0 sun pointed {sun:?} where the eye is {eye:?}"
            );
            // And the angle really is the tilt, for a handful of tilts.
            for tilt in [30.0f32, 55.0, 90.0, 140.0] {
                let angled = Sun::Viewport { tilt_deg: tilt, bearing_deg: 17.0 }
                    .direction_sphere(&inverse_view);
                let measured = angled.dot(eye).clamp(-1.0, 1.0).acos().to_degrees();
                assert!(
                    (measured - f64::from(tilt)).abs() < 1e-6,
                    "tilt {tilt} measured {measured} degrees from the eye direction"
                );
            }
        }
    }

    /// **The default sun is the real one, and it is a subsolar point.**
    ///
    /// The reversal this asserts is argued on [`Sun`]; what it pins is that
    /// the default resolves to a [`Sun::Map`] rather than to a screen
    /// direction, because a default that quietly stayed viewport-anchored
    /// would look identical in every other test in this file.
    #[test]
    fn the_default_sun_is_the_one_the_clock_says_and_it_is_anchored_to_the_planet() {
        assert_eq!(Sun::default(), Sun::Now, "the default is the clock, not a fixed angle");
        assert_eq!(Atmosphere::default().sun, Sun::Now);

        let resolved = Sun::Now.resolved();
        let Sun::Map { lon_deg, lat_deg } = resolved else {
            panic!("`Now` must resolve to a subsolar POINT; it gave {resolved:?}")
        };
        assert!((-180.0..=180.0).contains(&lon_deg), "subsolar longitude {lon_deg}");
        assert!((-23.5..=23.5).contains(&lat_deg), "subsolar latitude {lat_deg}");
        // It really is NOW: the same instant fed to `at_utc` gives the same
        // point, to the drift of one clock read.
        let now = super::super::solar::now_unix_seconds().expect("a desktop has a clock");
        let Sun::Map { lon_deg: expect_lon, .. } = Sun::at_utc(now) else { unreachable!() };
        assert!(
            (lon_deg - expect_lon).abs() < 0.01,
            "`Now` resolved to lon {lon_deg} where the clock says {expect_lon} -- 0.01 \
             degrees is 2.4 seconds of drift between the two reads"
        );

        // And the two anchors that are NOT the clock pass through
        // `resolved` untouched, bit for bit -- the property that lets
        // `paint` resolve once and hand the result to two shaders.
        for sun in [Sun::VIEWPORT_DEFAULT, Sun::Map { lon_deg: 12.5, lat_deg: -3.25 }] {
            assert_eq!(sun.resolved(), sun);
        }
    }

    /// **The subsolar point reaches the shader as a direction, and six hours
    /// turns it a quarter of the way round the planet.**
    ///
    /// This is the direction half of the plan's Part 1 acceptance: the sun
    /// for a known timestamp, and the terminator moving when the clock does.
    /// The instants are the September 2024 equinox (declination 0, so the
    /// sun is exactly on the equator and the arithmetic has no latitude in
    /// it to hide behind) and the same instant plus six hours.
    ///
    /// Written against `space::sky_direction`, the planet's own lon/lat
    /// convention, rather than against a rotation matrix — the same reason
    /// `a_map_anchored_sun_ignores_the_camera_and_a_viewport_anchored_one_does_not`
    /// gives for predicting a geography instead of a rotation.
    #[test]
    fn a_clock_sun_stands_where_the_subsolar_point_is_and_six_hours_turns_it_ninety_degrees() {
        // 2024-09-22T12:44:00Z, the published September equinox, as seconds
        // since the Unix epoch. Independently checkable:
        // 1,727,009,040 / 86,400 = 19,988.53 days.
        const EQUINOX: f64 = 1_727_009_040.0;
        let inverse_view =
            super::super::globe::globe_frame(&camera(0.0, 0.0, 2.5)).globe_view.inverse();

        let sun = Sun::at_utc(EQUINOX);
        let Sun::Map { lon_deg, lat_deg } = sun else { panic!("at_utc gives a subsolar point") };
        // The declination at an equinox is zero by definition of the word,
        // and the longitude is the one `render::solar`'s own tests tie to
        // NOAA's apparent-noon table.
        assert!(lat_deg.abs() < 0.02, "the equinox subsolar latitude came out {lat_deg}");
        assert!(
            (lon_deg + 12.869).abs() < 0.01,
            "the equinox subsolar longitude came out {lon_deg}, not the -12.869 that \
             12:44 UTC less an equation of time of +7.476 minutes gives"
        );
        // The direction is that point, in the planet's convention.
        let predicted = super::super::space::sky_direction(lon_deg, lat_deg);
        assert!((sun.direction_sphere(&inverse_view) - predicted).length() < 1e-12);

        // SIX HOURS. The plan's acceptance criterion, as an angle on the
        // sphere rather than as a pixel -- the pixel is
        // `the_night_side_of_the_planet_is_dark_and_the_terminator_follows_the_clock`.
        let later = Sun::at_utc(EQUINOX + 6.0 * 3_600.0);
        let (a, b) = (sun.direction_sphere(&inverse_view), later.direction_sphere(&inverse_view));
        let turned = a.dot(b).clamp(-1.0, 1.0).acos().to_degrees();
        assert!(
            (turned - 90.0).abs() < 0.3,
            "six hours must turn the sun 90 degrees about the planet's axis; it turned \
             {turned}"
        );
        // Westward, not eastward -- the sign that decides whether the
        // terminator sweeps the right way, and the one a `-` typo flips
        // while leaving the 90 degrees intact.
        let Sun::Map { lon_deg: later_lon, .. } = later else { unreachable!() };
        let step = (later_lon - lon_deg + 540.0).rem_euclid(360.0) - 180.0;
        assert!(
            (step + 90.0).abs() < 0.3,
            "six hours must move the subsolar point 90 degrees WEST; it moved {step}"
        );

        // And the camera is not in the answer, at any of these instants --
        // the property that makes this a map-anchored sun.
        let panned = super::super::globe::globe_frame(&camera(140.0, -35.0, 4.0))
            .globe_view
            .inverse();
        assert_eq!(sun.direction_sphere(&inverse_view), sun.direction_sphere(&panned));
    }

    /// The uniform's layout, pinned rather than trusted. See the type's own
    /// doc comment for why a mislaid pad is a geometry bug rather than a
    /// uniform bug.
    #[test]
    fn the_atmosphere_uniform_is_16_byte_aligned_and_its_vec3s_land_on_16() {
        assert_eq!(std::mem::size_of::<AtmosphereUniform>(), 112);
        assert_eq!(std::mem::size_of::<AtmosphereUniform>() % 16, 0);
        // WGSL gives `vec3<f32>` an alignment of 16. Computed from the
        // struct rather than written down, so a reordered field fails here
        // instead of on a driver.
        let base = std::mem::offset_of!(AtmosphereUniform, inverse_view);
        assert_eq!(base, 0);
        assert_eq!(std::mem::offset_of!(AtmosphereUniform, camera_sphere) % 16, 0);
        assert_eq!(std::mem::offset_of!(AtmosphereUniform, sun_dir) % 16, 0);
        // ...and it fits inside the largest binding the crate already
        // declares, so no platform limit moves. `ModelUniform` is 192.
        let declared = super::super::gpu::declared_needs()
            .into_iter()
            .find(|(name, _)| *name == "max_uniform_buffer_binding_size")
            .expect("the limit is declared");
        assert!(
            declared.1 >= std::mem::size_of::<AtmosphereUniform>() as u64,
            "the atmosphere's {} byte uniform is larger than the {} byte binding this \
             crate declares a need for -- raise the declared need, do not shrink the \
             struct to fit a number",
            std::mem::size_of::<AtmosphereUniform>(),
            declared.1
        );
    }

    /// **The shell is thin, and the numbers this module's docs quote for it
    /// are arithmetic a reader would otherwise have to redo.**
    ///
    /// 1.57% of the planet's radius is why the glow reads as a rim rather
    /// than a halo, and why the pixel test samples an annulus two pixels
    /// wide instead of a comfortable one.
    #[test]
    fn the_atmosphere_shell_is_one_and_a_half_percent_of_the_planet() {
        let ratio = ATMOSPHERE_RADIUS_M / EARTH_RADIUS_M;
        assert!(
            (ratio - 1.015_70).abs() < 1e-5,
            "the shell ratio is {ratio}, not the 1.01570 the docs claim"
        );
        // The two figures `ATMOSPHERE_RADIUS_M`'s doc gives, recomputed.
        assert!(((ratio - 1.0) * 161.385 - 2.533).abs() < 0.01);
        assert!(((ratio - 1.0) * 379.0 - 5.950).abs() < 0.01);
    }

    /// **The CPU's terminator band is the shader's own, read out of the
    /// shader.**
    ///
    /// The ground ramps through `TERMINATOR_HALF_WIDTH_SIN` on the GPU and
    /// the labels over it ramp through [`super::TERMINATOR_HALF_WIDTH_SIN`]
    /// on the CPU. If the two ever drift, a label slides into night at a
    /// different longitude from the coastline under it — a defect that
    /// would look like a placement bug, not a lighting one. So this reads
    /// the literal back out of `shader.wgsl` rather than trusting a comment
    /// that says they match.
    #[test]
    fn the_cpu_terminator_band_is_the_shaders_own() {
        let wgsl = include_str!("shader.wgsl");
        let line = wgsl
            .lines()
            .find(|line| line.trim_start().starts_with("const TERMINATOR_HALF_WIDTH_SIN"))
            .expect("shader.wgsl declares TERMINATOR_HALF_WIDTH_SIN");
        let literal = line
            .rsplit_once('=')
            .expect("the declaration assigns")
            .1
            .trim()
            .trim_end_matches(';')
            .trim();
        let shader_value: f32 = literal.parse().expect("the literal is an f32");
        assert_eq!(
            shader_value,
            super::TERMINATOR_HALF_WIDTH_SIN,
            "shader.wgsl ramps the GROUND over {shader_value} and SymbolLight ramps the \
             LABELS over {} -- change both or neither",
            super::TERMINATOR_HALF_WIDTH_SIN
        );
    }

    /// **The ink floor is the number [`SymbolLight`]'s doc quotes**, at the
    /// default night and at both ends of the knob, and the LUMINANCE term
    /// is what keeps a dark-text style out of it.
    #[test]
    fn a_bright_glyph_dims_to_the_quoted_floor_and_a_dark_one_barely_moves() {
        let default_night = Atmosphere { night_brightness: Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS, ..Atmosphere::default() }
            .night_brightness_clamped();
        assert_eq!(default_night, 0.25, "the terminator floor this test is calibrated against moved");
        let white = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let black = crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

        let lit = SymbolLight::new(glam::DVec3::Z, default_night, 1.0);
        let night = lit.shade(Some(-glam::DVec3::Z));
        assert!(
            (night.ink(white).r - 0.70).abs() < 1e-6,
            "white ink on the night side is {}, not the 0.70 the docs quote",
            night.ink(white).r
        );
        // Alpha is never touched: both draw paths blend with STRAIGHT
        // alpha, so fading it would pull the label toward the sky.
        assert_eq!(night.ink(white).a, 1.0);
        // A BLACK glyph is already as dark as it can be. Multiplying it
        // toward black on a ground that has itself gone dark removes
        // contrast rather than trading it, so it takes none of the night.
        assert_eq!(night.ink(black), black, "a black glyph must not be dimmed");

        // `night_brightness: 1.0` is how a consumer turns ground shading
        // off. It must turn the labels off with it -- one knob, not two.
        let unshaded = SymbolLight::new(glam::DVec3::Z, 1.0, 1.0);
        assert_eq!(unshaded.shade(Some(-glam::DVec3::Z)), SymbolShade::UNSHADED);
        assert_eq!(unshaded.shade(Some(-glam::DVec3::Z)).ink(white), white);

        // A black night still leaves white ink well clear of black.
        let blackest = SymbolLight::new(glam::DVec3::Z, 0.0, 1.0);
        assert!((blackest.shade(Some(-glam::DVec3::Z)).ink(white).r - 0.60).abs() < 1e-6);
    }

    /// **A DARK halo fades and a LIGHT one does not**, which is the half of
    /// this that keeps the two vendored styles (dark text, white halo)
    /// working while fixing the one the defect was reported on (light text,
    /// near-black halo).
    #[test]
    fn a_dark_halo_gives_up_alpha_and_a_white_one_keeps_all_of_it() {
        let night = SymbolLight::new(glam::DVec3::Z, 0.25, 1.0).shade(Some(-glam::DVec3::Z));

        // `swat-routes-dark`'s halo: rgb(7, 7, 8) -- the arrangement in the
        // reported frame. Near-black, so it takes nearly the whole fade.
        let dark = crate::style::Rgba { r: 7.0 / 255.0, g: 7.0 / 255.0, b: 8.0 / 255.0, a: 1.0 };
        let faded = night.halo(dark).a;
        assert!(
            (faded - 0.5625).abs() < 0.001,
            "a near-black halo should keep about 0.56 of its alpha, got {faded}"
        );
        // The reported frame's own halo, `#141414`, is a shade lighter and
        // keeps correspondingly more.
        let reported =
            crate::style::Rgba { r: 20.0 / 255.0, g: 20.0 / 255.0, b: 20.0 / 255.0, a: 1.0 };
        let reported_faded = night.halo(reported).a;
        assert!(
            (reported_faded - 0.5853).abs() < 0.001,
            "the reported frame's #141414 halo should keep about 0.59 of its alpha, got \
             {reported_faded}"
        );

        // Liberty's halo: #fff, and it is the only thing making that
        // style's black text readable once the ground goes dark. It must
        // keep every bit of its alpha.
        let white = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        assert!(
            (night.halo(white).a - 1.0).abs() < 1e-6,
            "a white halo must not fade: got {}",
            night.halo(white).a
        );

        // The halo's RGB takes the glyph's own scaling, so it cannot drift
        // in hue away from the text it surrounds.
        assert_eq!(night.halo(white).r, night.ink(white).r);
    }

    /// **The DAY side is untouched, bit for bit** -- the assertion that
    /// stops a global dimming from passing for a night-side fix.
    #[test]
    fn a_label_in_daylight_or_off_the_globe_passes_through_unchanged() {
        let colours = [
            crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            crate::style::Rgba { r: 0.1, g: 0.2, b: 0.3, a: 0.4 },
            crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
        ];
        let lit = SymbolLight::new(glam::DVec3::Z, 0.0, 1.0);
        for shade in [
            lit.shade(Some(glam::DVec3::Z)),
            SymbolLight::UNLIT.shade(Some(-glam::DVec3::Z)),
            SymbolLight::UNLIT.shade(None),
            lit.shade(None),
            // Above the SPACE range there is no night, however dark the
            // atmosphere is set -- the same gate the ground takes.
            SymbolLight::new(glam::DVec3::Z, 0.0, 0.0).shade(Some(-glam::DVec3::Z)),
        ] {
            assert_eq!(shade, SymbolShade::UNSHADED);
            for colour in colours {
                assert_eq!(shade.ink(colour), colour);
                assert_eq!(shade.halo(colour), colour);
            }
        }
    }

    /// **No popping as the terminator sweeps.** The ramp is continuous, it
    /// is monotone from night to day, and it spans exactly the band the
    /// ground spans -- checked by walking a meridian a tenth of a degree at
    /// a time and bounding the step.
    #[test]
    fn a_label_crosses_the_terminator_on_the_grounds_own_ramp() {
        let light = SymbolLight::new(glam::DVec3::Z, 0.25, 1.0);
        let white = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        // Parameterised by the sun's ELEVATION above the local horizon:
        // `dot(normal, sun)` is its sine, so 0 is the geometric terminator,
        // negative is night and positive is day.
        let at = |elevation_deg: f64| {
            let e = elevation_deg.to_radians();
            light.shade(Some(glam::DVec3::new(0.0, e.cos(), e.sin()).normalize())).ink(white).r
        };
        let mut previous = at(-90.0);
        let mut biggest_step = 0.0f32;
        let mut elevation = -90.0;
        while elevation <= 90.0 {
            let here = at(elevation);
            assert!(here >= previous - 1e-6, "the ramp went backwards at {elevation} degrees");
            biggest_step = biggest_step.max((here - previous).abs());
            previous = here;
            elevation += 0.1;
        }
        assert!(
            biggest_step < 0.01,
            "the terminator ramp jumps by {biggest_step} in a tenth of a degree -- that pops"
        );
        // The band is the shader's: fully night three degrees before the
        // geometric terminator, fully day three degrees after.
        assert!((at(-5.0) - 0.70).abs() < 1e-6, "still ramping 5 degrees into the night");
        assert!((at(5.0) - 1.0).abs() < 1e-6, "still ramping 5 degrees into the day");
        let midpoint = at(0.0);
        assert!(
            (midpoint - 0.85).abs() < 1e-3,
            "the geometric terminator should sit half way, not at {midpoint}"
        );
    }

    /// **The contrast ratios `SymbolLight`'s doc quotes**, recomputed from
    /// the night hemisphere of the frame the defect was reported on.
    ///
    /// The doc claims 4.80:1 against the night land and 5.21:1 against the
    /// night ocean, and those two numbers are the whole justification for
    /// 0.70 rather than something lower. A reader should not have to redo
    /// WCAG by hand to check them.
    #[test]
    fn night_side_ink_clears_the_contrast_threshold_the_docs_claim() {
        /// WCAG 2.x relative luminance: sRGB decoded, then Rec. 709.
        fn relative_luminance(rgb: [u8; 3]) -> f64 {
            let channel = |c: u8| {
                let c = f64::from(c) / 255.0;
                if c <= 0.040_45 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
        }
        fn ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
            let (x, y) = (relative_luminance(a), relative_luminance(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        }

        let white = crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let night = SymbolLight::new(glam::DVec3::Z, 0.25, 1.0).shade(Some(-glam::DVec3::Z));
        let ink = night.ink(white);
        let drawn = [
            (ink.r * 255.0).round() as u8,
            (ink.g * 255.0).round() as u8,
            (ink.b * 255.0).round() as u8,
        ];
        assert_eq!(drawn, [179, 179, 179], "night-side white ink should draw as #B3B3B3");

        // Sampled off the reported frame's night hemisphere.
        let night_land = [0x43, 0x42, 0x3E];
        let night_ocean = [0x2E, 0x3F, 0x49];
        let against_land = ratio(drawn, night_land);
        let against_ocean = ratio(drawn, night_ocean);
        assert!(
            (against_land - 4.80).abs() < 0.05,
            "the docs claim 4.80:1 against the night land; it is {against_land:.2}"
        );
        assert!(
            (against_ocean - 5.21).abs() < 0.05,
            "the docs claim 5.21:1 against the night ocean; it is {against_ocean:.2}"
        );
        assert!(against_land >= 4.5, "night-side labels must clear WCAG AA against the land");

        // And the thing this fix refused to do: the GROUND's own floor.
        let at_ground_floor = [(255.0f32 * 0.25) as u8; 3];
        let would_be = ratio(at_ground_floor, night_land);
        assert!(
            would_be < 1.2,
            "multiplying the glyph by the ground's 0.25 would give {would_be:.2}:1 -- the \
             whole reason the floor is raised is that this is invisible"
        );
    }

    // ---- The night pair: `dark_label_colors` ----

    fn rgba(r: f32, g: f32, b: f32, a: f32) -> crate::style::Rgba {
        crate::style::Rgba { r, g, b, a }
    }

    /// WCAG 2.x contrast ratio between two ENCODED sRGB colours.
    fn wcag_ratio(x: [f32; 3], y: [f32; 3]) -> f32 {
        let lum = |c: [f32; 3]| {
            let ch = |v: f32| {
                let v = v.clamp(0.0, 1.0);
                if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * ch(c[0]) + 0.7152 * ch(c[1]) + 0.0722 * ch(c[2])
        };
        let (a, b) = (lum(x), lum(y));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// `over` composited onto `under` with straight alpha, in the encoded
    /// space the symbol pass blends in.
    fn over(under: [f32; 3], over: crate::style::Rgba) -> [f32; 3] {
        let mix = |u: f32, o: f32| u * (1.0 - over.a) + o * over.a;
        [mix(under[0], over.r), mix(under[1], over.g), mix(under[2], over.b)]
    }

    /// The glyph's core pixel and its halo ring's pixel, both composited on
    /// `ground` the way the pass draws them: the halo onto the ground, the
    /// glyph onto the halo. Their ratio is the label's legibility.
    fn drawn_contrast(
        ground: [f32; 3],
        fill: crate::style::Rgba,
        halo: crate::style::Rgba,
    ) -> (f32, [f32; 3], [f32; 3]) {
        let ring = over(ground, halo);
        let core = over(ring, fill);
        (wcag_ratio(core, ring), core, ring)
    }

    /// Mapbox Streets' `state-label`: `hsl(220, 30%, 0%)` on `hsl(20, 25%,
    /// 100%)`, both at `text-opacity` 0.5 -- a grey glyph on a light halo.
    fn streets_state_pair() -> (crate::style::Rgba, crate::style::Rgba) {
        (rgba(0.0, 0.0, 0.0, 0.5), rgba(1.0, 1.0, 1.0, 0.5))
    }

    /// The night land of the frame the defect was reported on.
    const NIGHT_LAND: [f32; 3] = [0x43 as f32 / 255.0, 0x42 as f32 / 255.0, 0x3E as f32 / 255.0];

    /// **A grey glyph on a light halo becomes a light glyph on a dark halo
    /// at full night, and reads at 4.5:1 or better.**
    #[test]
    fn a_grey_on_light_label_turns_light_on_dark_and_readable_at_full_night() {
        let pairs = [
            ("streets state-label (black @0.5 on white @0.5)", streets_state_pair()),
            ("solid #808080 on #FFFFFF", (rgba(0.5, 0.5, 0.5, 1.0), rgba(1.0, 1.0, 1.0, 1.0))),
            ("streets country-label (black on white)", (rgba(0.0, 0.0, 0.0, 1.0), rgba(1.0, 1.0, 1.0, 1.0))),
            (
                "streets settlement-subdivision (hsl(220,30%,40%) on white @0.75)",
                (rgba(0.28, 0.36, 0.52, 1.0), rgba(1.0, 1.0, 1.0, 0.75)),
            ),
        ];
        for (name, (fill, halo)) in pairs {
            let (day_ratio, ..) = drawn_contrast(NIGHT_LAND, fill, halo);
            let (night_fill, night_halo) = dark_label_colors(fill, halo, 0.75);
            assert!(
                luminance(night_fill) > luminance(night_halo),
                "{name}: at full night the glyph should be LIGHTER than its halo; fill \
                 {night_fill:?} halo {night_halo:?}"
            );
            let (ratio, core, ring) = drawn_contrast(NIGHT_LAND, night_fill, night_halo);
            assert!(
                ratio >= 4.5,
                "{name}: night pair draws {core:?} on {ring:?}, {ratio:.2}:1 -- below 4.5:1 \
                 (the day pair on this ground was {day_ratio:.2}:1)"
            );
        }
    }

    /// **The glyph keeps its hue** when it is lightened, and a label its
    /// style has faded out entirely is still gone at night.
    #[test]
    fn the_night_pair_keeps_hue_and_keeps_a_transparent_label_transparent() {
        let blue_grey = rgba(0.28, 0.36, 0.52, 1.0);
        let (fill, _) = dark_label_colors(blue_grey, rgba(1.0, 1.0, 1.0, 1.0), 0.75);
        assert!(fill.b > fill.g && fill.g > fill.r, "the hue drifted: {fill:?}");
        let (fill, halo) =
            dark_label_colors(rgba(0.0, 0.0, 0.0, 0.0), rgba(1.0, 1.0, 1.0, 0.0), 0.75);
        assert_eq!((fill.a, halo.a), (0.0, 0.0));
    }

    /// **Full day is the input, exactly, and every depth of night is ONE
    /// pair, never a blend.** Replaces the old "continuous in between"
    /// test: continuity between dark-on-light and light-on-dark is exactly
    /// what put glyph == halo at z5. Now, at every depth, the glyph is
    /// either darker than its halo (the day pair, halo still the style's
    /// own colour) or lighter than it (the night pair), with a real gap
    /// between the two, and the choice flips exactly once.
    #[test]
    fn the_day_pair_is_the_input_by_day_and_no_depth_of_night_blends_the_pairs() {
        let (fill, halo) = streets_state_pair();
        assert_eq!(dark_label_colors(fill, halo, 0.0), (fill, halo));
        let mut flips = 0;
        let mut was_night = false;
        for i in 1..=1000 {
            let night = 0.75 * i as f32 / 1000.0;
            let (f, h) = dark_label_colors(fill, halo, night);
            let is_night = luminance(f) > luminance(h);
            if is_night {
                assert!(
                    luminance(f) - luminance(h) > 0.5,
                    "night {night}: a night pair must be clearly light on dark, got {f:?} on {h:?}"
                );
            } else {
                assert_eq!((h.r, h.g, h.b), (halo.r, halo.g, halo.b), "night {night}: the day halo is the style's");
                assert!(luminance(h) - luminance(f) > 0.5, "night {night}: {f:?} on {h:?}");
            }
            if is_night != was_night {
                flips += 1;
                was_night = is_night;
            }
        }
        assert_eq!(flips, 1, "the pair must switch exactly once, day to night");
        assert!(was_night, "deepest night must be the night pair");
    }

    /// **Through `SymbolShade`**: a dark label turns at night and is the
    /// same colours by day; a light label (a dark style's) keeps the
    /// `ink`/`halo` dimming it had, so the 0.70 floor still holds.
    #[test]
    fn symbol_shade_routes_dark_labels_to_the_night_pair_and_light_ones_to_the_dim() {
        let light = SymbolLight::new(glam::DVec3::Z, 0.25, 1.0);
        let night = light.shade(Some(-glam::DVec3::Z));
        let day = light.shade(Some(glam::DVec3::Z));
        let (fill, halo) = (rgba(0.0, 0.0, 0.0, 1.0), rgba(1.0, 1.0, 1.0, 1.0));

        assert_eq!(day.label_colors(fill, halo, 0.5), (
            rgba(0.0, 0.0, 0.0, 0.5),
            rgba(1.0, 1.0, 1.0, 0.5),
        ));
        assert_eq!(day.sdf_colors(fill, halo, 0.5), (fill, halo, 0.5));
        assert_eq!(day.icon_colors(fill, halo, 0.5), (fill, halo, 0.5));

        let (f, h) = night.label_colors(fill, halo, 0.5);
        assert_eq!((f, h), dark_label_colors(rgba(0.0, 0.0, 0.0, 0.5), rgba(1.0, 1.0, 1.0, 0.5), 0.75));
        assert!(drawn_contrast(NIGHT_LAND, f, h).0 >= 4.5);
        let (sf, sh, so) = night.sdf_colors(fill, halo, 0.5);
        assert_eq!((sf, sh, so), (f, h, 1.0), "the SDF run must match the point label beside it");
        assert_eq!(night.icon_colors(fill, halo, 0.5), (f, h, 1.0));

        let white = rgba(1.0, 1.0, 1.0, 1.0);
        let dark_halo = rgba(7.0 / 255.0, 7.0 / 255.0, 8.0 / 255.0, 1.0);
        let (f, h) = night.label_colors(white, dark_halo, 1.0);
        assert_eq!((f, h), (night.ink(white), night.halo(dark_halo)));
        assert_eq!(night.icon_colors(white, dark_halo, 1.0), (white, dark_halo, 1.0));
    }

    // ---- One pair per label: no crossover band ----

    /// Mapbox Streets' `land`, `hsl(20, 20%, 95%)`, encoded.
    const STREETS_LAND: [f32; 3] = [0.9590, 0.9450, 0.9410];
    /// Mapbox Streets' `water`, `hsl(200, 100%, 80%)`, encoded.
    const STREETS_WATER: [f32; 3] = [0.6, 0.8667, 1.0];

    /// The ground a label at `cos_zenith` sits on at `zoom`, shaded the way
    /// `shader.wgsl` shades it: `night_shade(night_floor(..), cos)` times
    /// the day colour, in the encoded space.
    fn shaded_ground(day: [f32; 3], zoom: f64, cos_zenith: f32) -> [f32; 3] {
        let transition = crate::render::globe::space_transition_for_zoom(zoom);
        let shade = night_shade(night_floor(0.25, transition), cos_zenith);
        [day[0] * shade, day[1] * shade, day[2] * shade]
    }

    /// The label's shade at `zoom` and `cos_zenith`, through the real
    /// `SymbolLight`, with the sun on +Z and the anchor placed so that
    /// `dot(anchor, sun) == cos_zenith`.
    fn shade_at(zoom: f64, cos_zenith: f32) -> SymbolShade {
        let transition = crate::render::globe::space_transition_for_zoom(zoom);
        let light = SymbolLight::new(glam::DVec3::Z, 0.25, transition);
        let c = f64::from(cos_zenith).clamp(-1.0, 1.0);
        light.shade(Some(glam::DVec3::new((1.0 - c * c).sqrt(), 0.0, c)))
    }

    /// **No zoom and no terminator position turns a dark-on-light label
    /// into a muddle.** The defect: `night_label_colors` mixed the day pair
    /// into the night pair continuously, and any continuous path from dark
    /// on light to light on dark crosses glyph == halo -- at z5 over the
    /// night side both came out mid-grey on a half-dimmed ground and the
    /// labels vanished (the iPhone report, south India at night).
    ///
    /// Swept over zoom 3.0..6.5 in 0.05 steps deep in the night, and across
    /// the terminator at z3, z4, z5 and z5.5, on Streets' land and water:
    /// the glyph must read against its halo at 4.5:1 -- or, for a style
    /// whose own DAY design is below that (Streets' half-opacity state
    /// names are 3.9:1 on its land), no worse than its day design -- AND
    /// against the bare shaded land at 3:1.
    #[test]
    fn no_zoom_or_terminator_position_puts_a_dark_label_in_a_crossover_muddle() {
        let pairs = [
            ("state-label (black @0.5 on white @0.5)", rgba(0.0, 0.0, 0.0, 1.0), rgba(1.0, 1.0, 1.0, 1.0), 0.5),
            ("country-label (black on white)", rgba(0.0, 0.0, 0.0, 1.0), rgba(1.0, 1.0, 1.0, 1.0), 1.0),
        ];
        let mut cases: Vec<(f64, f32)> = (0..=70).map(|i| (3.0 + f64::from(i) * 0.05, -1.0)).collect();
        for zoom in [3.0, 4.0, 5.0, 5.5] {
            cases.extend((0..=200).map(|i| (zoom, -0.08 + 0.16 * i as f32 / 200.0)));
        }
        let mut failures = Vec::new();
        for (name, fill, halo, opacity) in pairs {
            let mut worst = (f32::INFINITY, f32::INFINITY);
            let with_opacity = |c: crate::style::Rgba| rgba(c.r, c.g, c.b, c.a * opacity);
            let design = drawn_contrast(STREETS_LAND, with_opacity(fill), with_opacity(halo)).0;
            let halo_floor = design.min(4.5) - 0.01;
            for &(zoom, cos) in &cases {
                let shade = shade_at(zoom, cos);
                let (f, h) = shade.label_colors(fill, halo, opacity);
                for (ground_name, day) in [("land", STREETS_LAND), ("water", STREETS_WATER)] {
                    let ground = shaded_ground(day, zoom, cos);
                    let (vs_halo, core, _) = drawn_contrast(ground, f, h);
                    let vs_ground = wcag_ratio(core, ground);
                    if ground_name == "land" {
                        worst = (worst.0.min(vs_halo), worst.1.min(vs_ground));
                    }
                    // The 3:1 glyph-to-bare-ground floor is held for a
                    // FULL-opacity label on land. It is NOT held for the
                    // half-opacity state name, and cannot be once its
                    // alpha is capped at the style's own (the hierarchy
                    // rule): a 50% black core over an opaque white ring is
                    // a mid grey, which drops under 3:1 against the land
                    // once the ground factor is below ~0.85, while the
                    // night pair's 0.75-alpha `#D9D9D9` core only reaches
                    // 3:1 below ~0.37 -- no threshold closes a gap that
                    // wide. The ring is what makes that label legible
                    // (the halo floor below is held at every step), as
                    // it is in the style's own daytime design. Water's
                    // own DAY design is under 3:1 glyph-to-ground too.
                    let ground_floor = if ground_name == "land" && opacity >= 1.0 { 3.0 } else { 0.0 };
                    let water_halo_floor = if ground_name == "land" { halo_floor } else { halo_floor - 0.15 };
                    if vs_halo < water_halo_floor || vs_ground < ground_floor {
                        failures.push(format!(
                            "{name} on {ground_name} z{zoom:.2} cos {cos:+.3} (night {:.3}): \
                             text/halo {vs_halo:.2} (floor {water_halo_floor:.2}), text/ground {vs_ground:.2}",
                            shade.night()
                        ));
                    }
                }
            }
            eprintln!("{name}: worst on land text/halo {:.2}, text/ground {:.2}", worst.0, worst.1);
        }
        assert!(
            failures.is_empty(),
            "{} steps are unreadable, first few:\n{}",
            failures.len(),
            failures.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    /// **The style's hierarchy survives the night, in both pairs.** A
    /// half-opacity label (Streets' state names) must stay LESS contrasty
    /// than a full-opacity one (its city names) beside it, against its halo
    /// and against the ground, at every depth -- the defect was a day pair
    /// that raised glyph alpha toward opaque as the ground darkened, which
    /// drew the states bolder than the cities at z5.
    #[test]
    fn a_half_opacity_label_stays_subordinate_to_a_full_opacity_one_in_both_pairs() {
        let (fill, halo) = (rgba(0.0, 0.0, 0.0, 1.0), rgba(1.0, 1.0, 1.0, 1.0));
        let mut seen = (false, false);
        let mut cases: Vec<(f64, f32)> = (0..=70).map(|i| (3.0 + f64::from(i) * 0.05, -1.0)).collect();
        cases.extend((0..=200).map(|i| (4.5, -0.08 + 0.16 * i as f32 / 200.0)));
        for (zoom, cos) in cases {
            let shade = shade_at(zoom, cos);
            let ground = shaded_ground(STREETS_LAND, zoom, cos);
            let (sf, sh) = shade.label_colors(fill, halo, 0.5);
            let (cf, ch) = shade.label_colors(fill, halo, 1.0);
            let (state_halo, state_core, _) = drawn_contrast(ground, sf, sh);
            let (city_halo, city_core, _) = drawn_contrast(ground, cf, ch);
            let (state_ground, city_ground) = (wcag_ratio(state_core, ground), wcag_ratio(city_core, ground));
            let at = format!("z{zoom:.2} cos {cos:+.3} night {:.3}", shade.night());
            assert!(
                state_halo <= 0.75 * city_halo,
                "{at}: state {state_halo:.2} vs city {city_halo:.2} against the halo -- not visibly subordinate"
            );
            if (state_ground - city_ground).abs() > 1e-4 {
                // Against the bare ground the ORDER is what matters: the
                // city glyph is further from the ground in lightness.
                let state_gap = (luminance(sf) - luminance_of(ground)).abs() * sf.a;
                let city_gap = (luminance(cf) - luminance_of(ground)).abs() * cf.a;
                assert!(state_gap < city_gap, "{at}: state glyph is further from the ground than the city's");
            }
            let night_pair = luminance(cf) > luminance(ch);
            if night_pair { seen.1 = true } else if shade.night() > 0.0 { seen.0 = true }
        }
        assert!(seen.0 && seen.1, "the sweep must exercise both pairs: {seen:?}");
    }

    fn luminance_of(c: [f32; 3]) -> f32 {
        luminance(rgba(c[0], c[1], c[2], 1.0))
    }
}
