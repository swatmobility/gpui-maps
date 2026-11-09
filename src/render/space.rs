//! What the globe sits ON: the space backdrop, and a star field of real
//! stars.
//!
//! # The defect this module exists to remove
//!
//! The map's background is the render pass's **clear colour**
//! (`map_view::paint` → [`super::TilePipeline::clear_color`]);
//! `style::PaintOp::Background` emits no vertices at all. On a flat map that
//! is exactly right — the background is behind every tile and fills the
//! viewport, which is what a Mercator map's background is. On the **globe**
//! it is exactly wrong twice over:
//!
//!  - the clear colour paints the whole viewport, *including outside the
//!    silhouette disc*, so the planet sits on a field of its own ocean
//!    colour and the silhouette is only visible where a tile's fill happens
//!    to differ from it;
//!  - and if the clear colour were simply changed to a space colour, every
//!    part of the sphere the tiles do not cover — which at z0-z3 is most of
//!    the ocean — would become a hole onto space instead.
//!
//! So both halves are needed, and neither alone is coherent:
//!
//!  1. the clear colour becomes the **space** colour, blended in by the
//!     globe transition so the flat map is untouched
//!     ([`space_clear_color`]); and
//!  2. the style's background layer becomes **geometry on the sphere** — an
//!     ocean sphere drawn behind the tiles ([`backdrop_plan`]).
//!
//! # Where the reference stands
//!
//! MapLibre GL JS (BSD-3, `Copyright (c) 2023, MapLibre contributors`) does
//! exactly (2): `drawBackground` does not clear, it *draws*, and in globe
//! mode the thing it draws is a sphere mesh per covering tile —
//! `projection.getMeshFromTileID(context, tileID.canonical, false, true,
//! 'raster')` (`src/webgl/draw/draw_background.ts:68`), whose `allowPoles`
//! argument is `true` so the mesh reaches past Mercator's ±85.051°
//! (`vertical_perspective_projection.ts:75-86`). Its own comment is worth
//! keeping: "For globe rendering, background uses tile meshes *without*
//! borders and no stencil clipping. This works assuming the tileIDs list
//! contains only tiles of the same zoom level."
//!
//! **We diverge by drawing ONE z0 mesh rather than one per covering tile.**
//! We can, and they cannot, because our backdrop is a single flat colour and
//! `render::globe::tile_mercator_coords` for `z0/0/0` spans the entire
//! sphere, so one mesh is the whole ocean. It is also one draw call instead
//! of up to 56 (Task 5's measured worst globe cover). The seam question that
//! divergence raises answers itself: `render::subdivide`'s granularity table
//! halves per zoom from a base of 64, so a cell is **5.625° at every level
//! in the globe band** — z0 at granularity 64 and z3 at granularity 8 put
//! their vertices on the same parallels and meridians, and the backdrop's
//! limb polygon therefore coincides with the tiles' rather than crossing it.
//!
//! For (1) the reference's shape is its **atmosphere** pass, and the part we
//! take is the gate, not the physics: `atmosphereBlend = sky.properties.get(
//! 'atmosphere-blend') * projectionData.projectionTransition`, then "Don't
//! draw anything if atmosphere is fully transparent" and an early return
//! (`draw_sky.ts:90-95`). A globe-only effect is multiplied by the globeness
//! and **skipped entirely at zero** — which is what makes the flat map
//! bit-identical rather than merely close. [`space_clear_color`] and
//! [`Space::star_intensity`] are both gated that way.
//!
//! # The stars are ours, because the reference has none
//!
//! MapLibre GL JS has **no star field**: `grep -rinE "\bstars?\b"` over its
//! `src/` returns nothing, and its globe-only sky work is the Rayleigh/Mie
//! `atmosphere.fragment.glsl` above. Nothing in this module is ported from
//! any Mapbox GL JS 2.x/3.x tree either — that codebase left open source at
//! v2.0 and this plan's Global Constraints rule it out as a reference.
//!
//! So the design decisions are ours and are argued here rather than cited:
//!
//!  - **Real positions, not random ones.** The Yale Bright Star Catalogue's
//!    9,096 stars with a V magnitude (`src/render/PROVENANCE.md`), so
//!    Orion and the Plough are where they belong and a brightness is a
//!    measurement rather than a sample. A uniform random sky is cheaper to
//!    generate and reads as noise.
//!  - **The stars are fixed to the GROUND.** [`sky_view_proj`] carries the
//!    camera's orientation *and* the planet's own rotation
//!    (`Rx(lat) * Ry(-lon)`), the same pair in the same order as
//!    `render::globe::globe_frame`'s chain, because the camera's centre
//!    longitude and latitude are where the eye SITS and therefore decide
//!    which way it LOOKS. Pan east and new sky comes over the eastern
//!    limb. The zoom still moves no star: it slides the eye along its own
//!    axis without turning it, and there are no translations here.
//!    **This corrects what Task 8 shipped**, which omitted that pair and
//!    called the result "an inertial frame"; the star field slid with the
//!    viewport instead, and [`sky_view_proj`]'s doc comment carries the
//!    argument in full.
//!  - **Sized and lit from magnitude, both, and by different laws.** See
//!    [`star_brightness`] and [`star_half_size_px`].
//!
//! The one thing MapLibre's globe DOES have and this module deliberately
//! does not is the scattering pass — that is `render::atmosphere`, Task 9,
//! which Task 8 deferred on the grounds that it needed a sun and this crate
//! has no `light`. It is reached through [`Space::atmosphere`], drawn on the
//! far side of the tiles rather than behind them, and gated on exactly the
//! same globe transition everything here is.
//!
//! # A separate pipeline, not a wider tile shader
//!
//! [`StarPipeline`] is its own pipeline with its own shader and its own
//! 28-byte vertex, a sibling of `PlatePipeline` and `IconPipeline` rather
//! than an extension of `TilePipeline`. The tile shader's vertex is 88 bytes
//! of fill/stroke/dash paint a star has no use for, its `VertexOutput`
//! already fills **ten** of the fifteen inter-stage slots iOS Metal offers
//! (`gpu::shader_needs`), and `max_vertex_buffers` is 1 on the WebGL2
//! fallback so a second buffer is not available to carry star attributes
//! alongside. A star needs four attributes and two inter-stage slots; giving
//! it its own pipeline spends neither of the two limits this crate has
//! already shipped an iOS crash over.

use super::{Camera, TileMesh};
use crate::style::Rgba;

/// The style-independent half of what the globe sits on: the colour of
/// space, and whether there are stars in it.
///
/// Held by `MapView` (`set_space`/`space`) rather than read out of the
/// style, because neither field has a style-spec property this crate
/// implements: MapLibre's nearest equivalents are `sky` and `light`, whose
/// `atmosphere-blend` is the one thing here with a direct equivalent, and it
/// lives on [`Self::atmosphere`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Space {
    /// What fills the viewport OUTSIDE the planet's silhouette at
    /// `transition == 1.0`, blended toward the style's background colour as
    /// the globe flattens. See [`space_clear_color`].
    ///
    /// **Pure black, and the choice was measured rather than picked.**
    ///
    /// Space is black, so the physically honest value is also the
    /// maximal-contrast one — and the case that decides it is the darkest
    /// style this repo ships, `mapbox-standard-dark`, whose background is
    /// `#0b0e13`. Against that background, per channel:
    ///
    /// ```text
    ///   space colour   gap vs #0b0e13   largest gap
    ///   #05070f          6,  7,  4                7
    ///   #01030a         10, 11,  9               11
    ///   #000000         11, 14, 19               19
    /// ```
    ///
    /// A first draft used `#05070f`, a slight blue-black, on the argument
    /// that a black backdrop "leaves no headroom under the faintest stars".
    /// That argument is **backwards**: [`star_brightness`] draws the
    /// faintest catalogue star at 1.7% alpha, which over `#05070f` reads
    /// (9, 11, 19) — barely distinguishable from the backdrop itself — and
    /// over black reads (4, 4, 4), which is a fourfold contrast ratio
    /// instead of a 1.2-fold one. Black is better on both counts.
    ///
    /// **And 19 byte units is still a subtle silhouette**, which is the
    /// honest limit of a colour-only fix for a near-black basemap. What
    /// makes the limb read there is the star field (opt-in, see
    /// [`Self::star_intensity`]) or the limb glow — which Task 8 deferred
    /// and Task 9 built, so [`Self::atmosphere`] is now the answer and it is
    /// on by default.
    pub color: Rgba,
    /// Multiplies every star's alpha. **`0.0` by default, which skips the
    /// star draw entirely** — no existing consumer of this crate gains a
    /// star field without asking for one, and four applications compile it.
    ///
    /// The gate is MapLibre's, for its own globe-only pass: the blend is
    /// multiplied by the globeness and the draw is skipped at zero rather
    /// than submitted and multiplied out (`draw_sky.ts:90-95`).
    pub star_intensity: f32,
    /// Multiplies every star's radius. `1.0` is [`star_half_size_px`]'s own
    /// scale, which puts the faintest star in the catalogue at 1.2 logical
    /// pixels across and Sirius at 4.91.
    pub star_size_scale: f32,
    /// The glow around the limb, and where its light comes from.
    ///
    /// Lives here rather than on a `light` because this crate has none —
    /// see [`super::atmosphere::Sun`] for what that costs and what a real
    /// `light` would later change. **On by default**, unlike
    /// [`Self::star_intensity`]; the two differ for stated reasons, in
    /// [`super::atmosphere::Atmosphere::intensity`].
    pub atmosphere: super::atmosphere::Atmosphere,
}

impl Default for Space {
    fn default() -> Self {
        Self {
            // Pure black -- see the field's own doc comment for the three
            // candidates and the measurement that chose between them, and
            // `the_default_space_colour_maximises_the_gap_to_the_darkest_shipped_background`
            // for the same table as assertions.
            color: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            star_intensity: 0.0,
            star_size_scale: 1.0,
            atmosphere: super::atmosphere::Atmosphere::default(),
        }
    }
}

impl Space {
    /// `star_intensity` clamped to `[0, 1]` — what the shader is handed.
    ///
    /// Clamped rather than trusted because it is a public field a consumer
    /// sets directly: a negative value would make `fs_main` return a
    /// negative alpha (undefined blending), and a value above 1 would push
    /// the faintest stars up to full white, which is the one thing a real
    /// sky never does.
    #[must_use]
    pub fn star_alpha(&self) -> f32 {
        self.star_intensity.clamp(0.0, 1.0)
    }

    /// Whether the star draw should be submitted at all, for a frame of the
    /// given globeness.
    ///
    /// Both gates, in one place, so the pixel tests and `paint` cannot
    /// disagree about them: no stars on a flat map (`transition <= 0.0`),
    /// and no stars at zero intensity.
    #[must_use]
    pub fn draws_stars(&self, transition: f32) -> bool {
        transition > 0.0 && self.star_alpha() > 0.0
    }
}

/// The render pass's clear colour: the style's background on a flat map,
/// and the SPACE colour on every frame with any globe in it.
///
/// **This used to be `lerp(background, space, transition)`, and that lerp
/// was the grey sky.** Above the limb nothing is drawn but the clear, so
/// zooming in through the globe band the whole sky tended to the style's
/// LAND colour -- at z5 pitch 60 over East Africa the top third of an
/// iPhone screen was flat `#E7E3DF`-ish grey. What is above the limb now is
/// `render::sky`'s pass: the style's `sky` gradient, drawn over this clear
/// with coverage `1 - transition`, so the space colour here shows through
/// it by exactly the transition -- MapLibre's `mix(sky, transparent,
/// projectionTransition)` over a globe's black. The step from background
/// to space at the band's edge is not visible: at `transition` just above
/// 0 the sky covers all of it.
///
/// At `transition <= 0.0` the background is returned bitwise unchanged --
/// the flat map is untouched, as a line of code a reader can point at. The
/// reference gates its own globe-only pass the same way (`draw_sky.ts:90-95`).
///
/// `background` is `None` for a style with no background layer, which clears
/// to `wgpu::Color::TRANSPARENT` on a flat frame.
#[must_use]
pub fn space_clear_color(background: Option<Rgba>, space: &Space, transition: f32) -> Rgba {
    let background = background.unwrap_or(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 });
    if transition.is_nan() || transition <= 0.0 {
        return background;
    }
    space.color
}

/// Whether the ocean sphere should be drawn at all, for a frame of this
/// globeness.
///
/// **The flat gate, in the one place both `MapView::paint` and the pixel
/// tests read it from.** That sharing is the point: a test that decided for
/// itself that a flat frame gets no backdrop would keep passing after the
/// gate was deleted from `paint` — which is exactly the failure this plan
/// has caught in one of its own tests already.
///
/// `> 0.0` rather than `>= some epsilon`: `space_transition_for_zoom` returns
/// exactly `0.0` from `GLOBE_ZOOM_MAX` upward, and any positive fade
/// at all means the sphere is bulging out of the Mercator square, so there
/// is already somewhere for the clear colour to show through.
#[must_use]
pub fn draws_backdrop(transition: f32) -> bool {
    transition > 0.0
}

/// The style's background layer as a `PaintPlan`: one polygon covering the
/// whole of the `z0` tile, in MVT coordinates.
///
/// This is the **ocean sphere**. `render::subdivide::Subdivision::for_tile`
/// on `z0/0/0` gives it granularity 64 and both pole caps, so the mesh it
/// tessellates to closes the sphere at the poles and follows the limb to
/// 0.19 px on the 161 px disc the pixel tests render (Task 5's measured
/// chord figures). Drawn through the ordinary tile shader with the tile's
/// own `GlobeParams`, so it curves by exactly the same code the tiles do —
/// there is no second projection to keep in step.
///
/// The polygon's edges lie *exactly* on the tile boundary, which is what
/// `render::subdivide::append_pole_caps` grows a cap from; a polygon inset
/// by even one unit would leave the sphere open at both poles.
/// **`color_from`/`color_to` are the style's zoom-STOP PAIR, not one
/// lerped colour**, and that is the difference between rebuilding this mesh
/// once and rebuilding it every frame.
///
/// `map_view::paint` reads the background layer as
/// `color.value_at_pair(display_zoom())` and the shader blends the pair by
/// `blend_t`, exactly as it does for every other fill. Baking a single
/// already-lerped colour instead looks simpler and is a trap: a
/// `background-color` written as a zoom expression changes value on *every*
/// frame of a zoom, so the rebuild condition would fire on every frame of a
/// zoom and re-tessellate 4,225 vertices on the frame thread each time
/// (**0.365 ms release, 8.701 ms debug** -- measured, see
/// `the_space_setup_costs_nothing_a_flat_frame_can_notice`). With the pair
/// baked, the mesh changes only when the STOP PAIR does, which for a
/// constant background is never.
#[must_use]
pub fn backdrop_plan(extent: u32, color_from: Rgba, color_to: Rgba) -> crate::style::PaintPlan {
    let e = extent as i32;
    let ring = geo_types::LineString::from(vec![(0, 0), (e, 0), (e, e), (0, e), (0, 0)]);
    crate::style::PaintPlan {
        ops: vec![crate::style::PaintOp::Fill {
            color_from,
            color_to,
            geometry: std::sync::Arc::new(geo_types::Geometry::Polygon(geo_types::Polygon::new(
                ring,
                Vec::new(),
            ))),
        }],
    }
}

/// The ocean sphere's mesh. See [`backdrop_plan`] for what the two colours
/// are and why there are two of them.
///
/// Built once per background colour PAIR, not per frame: the geometry
/// depends on nothing but `extent`, and the pair is baked into the vertices
/// the way every other fill's is.
#[must_use]
pub fn backdrop_mesh(extent: u32, color_from: Rgba, color_to: Rgba) -> TileMesh {
    super::tessellate_plan(
        &backdrop_plan(extent, color_from, color_to),
        extent,
        super::mesh::COORD_SCALE,
        super::subdivide::Subdivision::for_tile(BACKDROP_TILE),
    )
}

/// The tile the backdrop is: the single root tile, whose Mercator span is
/// the whole sphere.
///
/// Named rather than spelled out at each of the four places that need it
/// (the mesh, the model matrix, the `GlobeParams` and the tests), because
/// the value is load-bearing in a way `TileId { z: 0, x: 0, y: 0 }` does
/// not advertise: it is the *only* tile for which
/// `render::globe::tile_mercator_coords` returns `(0, 0, 1, 1)`, and
/// therefore the only one whose mesh can be the entire ocean.
pub const BACKDROP_TILE: crate::geo::TileId = crate::geo::TileId { z: 0, x: 0, y: 0 };

// ---------------------------------------------------------------------------
// The sky's frame
// ---------------------------------------------------------------------------

/// Where the sky sphere is put, in the units [`SKY_NEAR`]/[`SKY_FAR`] bound.
///
/// Any value strictly inside that interval draws the same pixels: the stars
/// are DIRECTIONS, so the only thing this distance decides is the depth the
/// quads land at, and the star pipeline neither tests nor writes depth. It
/// exists so the point handed to the projection is inside the clip volume
/// rather than on its near plane.
const SKY_RADIUS: f64 = 2.0;
/// The sky projection's near plane. Its own, deliberately not
/// `render::globe`'s `0.5 / 2^zoom`: the stars are at infinity, so nothing
/// about their screen position may depend on zoom, and a near/far pair
/// bracketing [`SKY_RADIUS`] is the smallest thing that keeps them inside
/// the clip volume at every zoom.
const SKY_NEAR: f64 = 1.0;
/// The sky projection's far plane. See [`SKY_NEAR`].
const SKY_FAR: f64 = 3.0;

/// Sky direction → clip space. **The frame the stars live in, and the one
/// place it is decided: the GROUND's.**
///
/// # What is in it, and what is deliberately left out
///
/// `render::globe::globe_frame` builds the planet's matrix as
///
/// ```text
///   proj * T(0,0,-d) * Rx(-pitch) * Rz(bearing) * T(0,0,-radius)
///        * Rx(lat) * Ry(-lon) * S(radius)
/// ```
///
/// This one is
///
/// ```text
///   proj_sky * Rx(-pitch) * Rz(bearing) * Rx(lat) * Ry(-lon) * S(SKY_RADIUS)
/// ```
///
/// — **the same rotations in the same order**, and every remaining
/// difference is a property the sky is supposed to have:
///
///  - **`Rx(lat) * Ry(-lon)` IS here, and Task 8 left it out.** The
///    omission was documented as "an inertial frame". It is not one; it
///    was a bug. That pair is where the *camera* sits relative to the
///    planet, so it decides which way the camera LOOKS — and a camera
///    looking somewhere else sees other stars. Without it the sky answered
///    to pitch and bearing alone, so panning slid the star field along
///    with the viewport while the planet turned underneath it. With it, panning east brings new sky over the
///    eastern limb, which is what a camera orbiting a planet sees.
///
///    The reference draws the same distinction for its light source, in
///    the same two rotations and the same order: `getSunPos` applies
///    `rotateX(lat)`/`rotateY(-lon)` exactly when the light's `anchor` is
///    `'map'` (`draw_sky.ts:61-78`). A star field is anchored to the map
///    in precisely that sense — Orion is over a place, not over a corner
///    of the viewport — so it takes the same pair. Task 8 cited that line
///    for the opposite conclusion.
///  - **no translations** — a star is at infinity, so only the direction
///    from the eye matters, and `T(0,0,-d)` / `T(0,0,-radius)` move the eye
///    without turning it. This is also why the zoom still moves no star.
///  - **`proj_sky`, not the globe's `proj`** — same FOV
///    (`camera::FOV_Y_RADIANS_F64`) and same aspect, so a star and a
///    landmark an equal angle apart are an equal number of pixels apart;
///    different near/far, because the globe's depend on zoom and distance
///    and the sky's must not.
///
/// # What "fixed to the ground" is not, and the one place it differs
///
/// With the planet's orientation in the chain the stars are fixed to the
/// **Earth's surface**, not to a true inertial frame. The two differ only
/// if the Earth's rotation is modelled independently of the camera's
/// longitude — i.e. only once a CLOCK turns the planet under a stationary
/// camera. `render::solar` has introduced one (the subsolar point moves
/// 0.0042°/s), so the distinction is now nameable: a true inertial sky
/// would need `Ry(-lon)` replaced by `Ry(-(lon + GMST(t)))`, and Polaris
/// would then stay over the pole while the sun crossed the sky. Nothing
/// here animates the planet's rotation, so the two frames coincide at
/// every instant this crate renders, and no sidereal claim is made or
/// needed. **Noted, deliberately not built** — it is a separate feature
/// with its own epoch to get right.
///
/// The frame the directions themselves live in is the *unrotated sphere
/// frame* — the space `shader.wgsl`'s `project_to_sphere` returns positions
/// in, `(sin(lon)*cos(lat), sin(lat), cos(lon)*cos(lat))`, y north. So the
/// sky's north celestial pole and the planet's north pole point the same
/// way (which is what makes Polaris sit over the Arctic), and at
/// `pitch 0, bearing 0` the direction at the centre of the screen is
/// `-sky_direction(lon, lat)`, i.e. **RA `lon + 180`, Dec `-lat`** — the
/// antipode of the ground point under the camera, which is exactly the
/// direction an eye above that point faces outward in. At
/// `lon 0, lat 0` that is `(0, 0, -1)`, RA 180. The sky visible over a
/// planet is the half behind it, so the antipode being the middle of it is
/// the correct relationship rather than an arbitrary one, and it is how
/// `the_sky_shares_the_grounds_frame_and_only_the_zoom_leaves_it_alone`
/// picks the directions it probes.
#[must_use]
pub fn sky_view_proj(camera: &Camera) -> glam::DMat4 {
    let aspect = f64::from(camera.viewport_width_px) / f64::from(camera.viewport_height_px);
    let proj = glam::dcamera::rh::proj::directx::perspective(
        camera.fov_y_rad(),
        aspect,
        SKY_NEAR,
        SKY_FAR,
    );
    proj * glam::DMat4::from_rotation_x(-f64::from(camera.pitch))
        * glam::DMat4::from_rotation_z(f64::from(camera.bearing))
        * glam::DMat4::from_rotation_x(camera.center_lat.to_radians())
        * glam::DMat4::from_rotation_y(-camera.center_lon.to_radians())
        * glam::DMat4::from_scale(glam::DVec3::splat(SKY_RADIUS))
}

/// [`sky_view_proj`] with the celestial sphere turned under the ground by
/// the Greenwich sidereal angle: a star at right ascension `ra` sits over
/// longitude `ra - sidereal`, so the chain gains `Ry(-sidereal)` on the
/// right (`Ry(a)` adds `a` to a direction's longitude in this frame). The
/// module doc's "What fixed to the ground is not" named this as the
/// inertial sky; only the opt-in space look asks for it, and at `0.0` it
/// is [`sky_view_proj`] exactly.
#[must_use]
pub fn sky_view_proj_turned(camera: &Camera, sidereal_rad: f64) -> glam::DMat4 {
    sky_view_proj(camera) * glam::DMat4::from_rotation_y(-sidereal_rad)
}

/// A sky direction from a right ascension and declination in DEGREES, in
/// the same convention `shader.wgsl`'s `project_to_sphere` returns for the
/// planet: `(sin*cos, sin, cos*cos)`, y north.
///
/// The same formula the planet uses is not a coincidence to be tidied away
/// — it is what makes [`sky_view_proj`]'s frame statement checkable: a
/// star at `(ra, dec)` sits in the direction the planet's own surface point
/// `(lon = ra, lat = dec)` sits in, before the planet is rotated.
#[must_use]
pub fn sky_direction(ra_deg: f64, dec_deg: f64) -> glam::DVec3 {
    let (ra, dec) = (ra_deg.to_radians(), dec_deg.to_radians());
    glam::DVec3::new(ra.sin() * dec.cos(), dec.sin(), ra.cos() * dec.cos())
}

// ---------------------------------------------------------------------------
// The catalogue
// ---------------------------------------------------------------------------

/// The Yale Bright Star Catalogue, quantised. See `src/render/PROVENANCE.md`
/// for where it came from, what was dropped and what is and is not known
/// about the terms; and [`catalogue`] for the record layout.
const STAR_BYTES: &[u8] = include_bytes!("bsc5_stars.bin");

/// Bytes per quantised record: `u16` right ascension, `i16` declination,
/// `i8` magnitude.
const STAR_RECORD_BYTES: usize = 5;

/// How many stars the shipped catalogue holds. Pinned as a constant because
/// three separate figures in this module's docs are derived from it.
pub const STAR_COUNT: usize = 9_096;

/// One catalogue star, decoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Star {
    /// Right ascension, degrees, J2000, `[0, 360)`.
    pub ra_deg: f32,
    /// Declination, degrees, J2000, `[-90, 90]`.
    pub dec_deg: f32,
    /// Johnson V magnitude. Smaller is brighter; Sirius is −1.46 and the
    /// faintest record here is 7.94 after quantisation.
    pub vmag: f32,
}

/// Every shipped star, decoded from [`STAR_BYTES`].
///
/// # The storage form, and what it gave up
///
/// Measured, for the same 9,096 records:
///
/// ```text
///   form                                   bytes
///   the parsed CSV as fetched             218,946
///   three f32 per star                    109,152
///   u16 RA + i16 Dec + i8 mag (shipped)    45,480
/// ```
///
/// The quantisation is exact enough that the loss is not the interesting
/// number. Round-tripping all 9,096 records through it moves the worst one
/// by **10.90 arcseconds** and **0.030 magnitudes**; a 768 px viewport at
/// this crate's 60° vertical FOV is 0.078° per pixel, so 10.90" is
/// **0.039 px** — two orders of magnitude below the pixel grid the stars
/// are rasterised onto — and 0.030 mag is 2.8% of a star's flux. What was
/// given up is anything a *future* consumer might want the catalogue for
/// that needs arcsecond astrometry; this is a backdrop, not an ephemeris.
///
/// **No magnitude cut.** A cut at the naked-eye limit of V = 6.5 would drop
/// **692** of the 9,096 (not the 50 fainter than 7.0, which is what a
/// glance at the integer-magnitude histogram suggests) and save 3,460
/// bytes. Those 692 are exactly the stars that give the sky its texture and
/// make the Milky Way read as a band, and 3.4 KB is not a budget.
pub fn catalogue() -> impl Iterator<Item = Star> + Clone {
    STAR_BYTES.as_chunks::<STAR_RECORD_BYTES>().0.iter().map(|record| {
        let ra = u16::from_le_bytes([record[0], record[1]]);
        let dec = i16::from_le_bytes([record[2], record[3]]);
        let mag = record[4] as i8;
        Star {
            // The encoder's inverse, and written as the inverse rather than
            // as a pair of magic multipliers: `round(ra / 360 * 65536) mod
            // 65536`, `round(dec / 90 * 32767)`, `round(mag * 16)`. See
            // `src/render/PROVENANCE.md`.
            ra_deg: f32::from(ra) * 360.0 / 65_536.0,
            dec_deg: f32::from(dec) * 90.0 / 32_767.0,
            vmag: f32::from(mag) / 16.0,
        }
    })
}

/// The magnitude that renders at full brightness. Brighter stars are
/// clamped to it; 298 of the 9,096 are.
const STAR_MAG_FULL: f32 = 3.5;
/// The faintest magnitude the shipped catalogue reaches, and the anchor
/// [`star_half_size_px`] measures down from.
const STAR_MAG_FAINT: f32 = 7.0;
/// Half-extent, in logical pixels, of a star at [`STAR_MAG_FAINT`].
const STAR_HALF_PX_FAINT: f32 = 0.6;
/// Extra half-extent, in logical pixels, per magnitude brighter than
/// [`STAR_MAG_FAINT`].
const STAR_HALF_PX_PER_MAG: f32 = 0.22;

/// A star's alpha at `star_intensity == 1.0`: its **flux**, relative to a
/// star of magnitude [`STAR_MAG_FULL`], clamped to `[0, 1]`.
///
/// This is Pogson's definition of the magnitude scale and not a curve
/// fitted to taste: five magnitudes are a factor of exactly 100 in flux, so
/// one magnitude is `10^(-0.4)` = 0.398. Over the shipped catalogue's range
/// (−1.4375 to 7.9375 after quantisation) that is a factor of **5,623** —
/// which is why the brightness law and the SIZE law below are different
/// laws: mapping 5,623 onto a radius would make Sirius a blob, and mapping
/// it onto alpha alone would make everything past magnitude 6 invisible.
///
/// The clamp at [`STAR_MAG_FULL`] rather than at the catalogue's own
/// brightest is what keeps the first magnitudes distinguishable: with the
/// reference point at Sirius, a magnitude-4 star — the faintest most people
/// can see from a city — would come out at 0.06% alpha.
#[must_use]
pub fn star_brightness(vmag: f32) -> f32 {
    10.0f32.powf(0.4 * (STAR_MAG_FULL - vmag)).clamp(0.0, 1.0)
}

/// A star's billboard half-extent in LOGICAL pixels, at
/// `star_size_scale == 1.0`.
///
/// Linear in MAGNITUDE, not in flux — see [`star_brightness`] for why the
/// two laws differ. The shipped catalogue therefore spans 0.6 px (V = 7.94,
/// floored) to 2.456 px (Sirius at V = −1.4375), i.e. discs of 1.2 to 4.91
/// logical pixels across.
#[must_use]
pub fn star_half_size_px(vmag: f32) -> f32 {
    (STAR_HALF_PX_FAINT + STAR_HALF_PX_PER_MAG * (STAR_MAG_FAINT - vmag)).max(STAR_HALF_PX_FAINT)
}

/// One vertex of one star's billboard. 28 bytes; see the module docs for why
/// this is a separate vertex type rather than four more fields on
/// `mesh::Vertex`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct StarVertex {
    /// Unit direction in the sky frame. See [`sky_direction`].
    pub dir: [f32; 3],
    /// [`star_half_size_px`].
    pub half_size_px: f32,
    /// [`star_brightness`].
    pub brightness: f32,
    /// Which corner of the quad, in `[-1, 1]^2`.
    pub corner: [f32; 2],
}

const STAR_VERTEX_ATTRS: [wgpu::VertexAttribute; 4] =
    wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32, 2 => Float32, 3 => Float32x2];

/// Four vertices and six indices per catalogue star.
///
/// Measured for the shipped 9,096: **36,384 vertices at 28 bytes =
/// 1,018,752 B**, plus **54,576 `u32` indices = 218,304 B**, for 1.24 MB of
/// static GPU buffer built once per `StarPipeline`. Against `gpu_meshes`'
/// 1,536 MB budget that is 0.08%, and unlike a tile mesh it never changes:
/// the camera reaches the stars entirely through [`sky_view_proj`], so
/// nothing here is rewritten when the map moves.
#[must_use]
pub fn star_mesh() -> (Vec<StarVertex>, Vec<u32>) {
    let mut vertices = Vec::with_capacity(STAR_COUNT * 4);
    let mut indices = Vec::with_capacity(STAR_COUNT * 6);
    for star in catalogue() {
        let dir = sky_direction(f64::from(star.ra_deg), f64::from(star.dec_deg));
        let dir = [dir.x as f32, dir.y as f32, dir.z as f32];
        let half_size_px = star_half_size_px(star.vmag);
        let brightness = star_brightness(star.vmag);
        let base = vertices.len() as u32;
        for corner in [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]] {
            vertices.push(StarVertex { dir, half_size_px, brightness, corner });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

// ---------------------------------------------------------------------------
// The stars as a ground observer's night sky
// ---------------------------------------------------------------------------

/// How far above the horizon the sky stars reach full brightness, as the
/// SINE of the altitude: about 7 degrees. Below it they fade into the
/// horizon haze and the city's glow ([`horizon_fade`]); at and below the
/// horizon they are gone. Not wider: a chase camera tilted to 68 degrees
/// sees only the lowest 8 degrees of sky, and a wider fade left it
/// starless.
pub const SKY_STAR_FADE_SIN: f32 = 0.12;

/// The sky stars' size against the space view's: a little larger, so
/// the catalogue's faint end still reads as points over a lit city.
pub const SKY_STAR_SIZE: f32 = 1.3;

/// The most a sky-star field is drawn at, under a black sky: a city's
/// light pollution washes out the faintest stars even overhead.
pub const SKY_STAR_MAX_INTENSITY: f32 = 0.85;

/// **The horizon fade of a sky star** at altitude `altitude_sin` (the sine
/// of its angle above the observer's horizon), for a fade `width_sin`
/// wide: `0` at and below the horizon, a smoothstep up to `1` at
/// `width_sin`. `width_sin <= 0` is no fade at all (the globe's space
/// view, where there is no horizon to fade into). `star_shader.wgsl`'s
/// `vs_main` computes the same expression; this CPU mirror is what the
/// tests read.
#[must_use]
pub fn horizon_fade(altitude_sin: f32, width_sin: f32) -> f32 {
    if width_sin <= 0.0 {
        return 1.0;
    }
    if altitude_sin <= 0.0 {
        return 0.0;
    }
    let t = (altitude_sin / width_sin).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// **How bright a sky-star field may be under this sky**: full
/// ([`SKY_STAR_MAX_INTENSITY`]) under a near-black sky colour, nothing once
/// the sky is as light as a dusk blue. A day style's sky (MapLibre's
/// default `#88C6FC`, any blue or white) is `0.0`, so a day map never
/// draws them, whatever the map asked for: the relative luminance of the
/// sky colour (Rec. 709 weights on the style's straight values) against
/// a ramp from 0.06 (full) to 0.16 (none).
#[must_use]
pub fn sky_star_intensity(sky: &crate::style::SkyFrame) -> f32 {
    let c = sky.sky_color;
    let luma = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    let night = ((0.16 - luma) / 0.10).clamp(0.0, 1.0);
    night * SKY_STAR_MAX_INTENSITY
}

/// The observer's "up" in the sky frame ([`sky_view_proj`]'s): the outward
/// normal of the ground under the camera's centre, which is
/// [`sky_direction`] at the camera's own longitude and latitude. A star's
/// altitude above the horizon is `asin(dot(star, up))`.
#[must_use]
pub fn observer_up(camera: &Camera) -> glam::DVec3 {
    sky_direction(camera.center_lon, camera.center_lat)
}

/// What `star_shader.wgsl`'s `Sky` struct expects. 96 bytes, already a
/// multiple of 16, so no explicit pad is needed — and
/// `the_star_uniform_fits_the_binding_size_the_crate_declares` pins that
/// rather than trusting the arithmetic.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct StarUniform {
    sky_view_proj: [[f32; 4]; 4],
    viewport_px: [f32; 2],
    intensity: f32,
    size_scale: f32,
    /// `xyz`: the observer's up ([`observer_up`]); `w`: the horizon fade's
    /// width ([`horizon_fade`]), `0` for none (the space view).
    horizon: [f32; 4],
}

impl StarUniform {
    /// One frame's sky, narrowed for upload.
    pub(crate) fn new(camera: &Camera, space: &Space) -> Self {
        Self {
            sky_view_proj: sky_view_proj(camera).as_mat4().to_cols_array_2d(),
            viewport_px: [camera.viewport_width_px, camera.viewport_height_px],
            intensity: space.star_alpha(),
            size_scale: space.star_size_scale.max(0.0),
            horizon: [0.0; 4],
        }
    }

    /// The sky over a ground observer: the stars above the horizon at
    /// `intensity`, faded into it ([`horizon_fade`]).
    pub(crate) fn over_horizon(camera: &Camera, intensity: f32, size_scale: f32) -> Self {
        let up = observer_up(camera);
        Self {
            sky_view_proj: sky_view_proj(camera).as_mat4().to_cols_array_2d(),
            viewport_px: [camera.viewport_width_px, camera.viewport_height_px],
            intensity: intensity.clamp(0.0, 1.0),
            size_scale: size_scale.max(0.0) * SKY_STAR_SIZE,
            horizon: [up.x as f32, up.y as f32, up.z as f32, SKY_STAR_FADE_SIN],
        }
    }
}

/// The star field's pipeline and its one static vertex/index buffer.
///
/// Built lazily — `MapView` creates one the first frame a `Space` with a
/// non-zero `star_intensity` is actually drawn — so a consumer that never
/// turns stars on pays for neither the 1.24 MB of buffers nor the shader
/// module. See the module docs for why this is a pipeline of its own.
pub struct StarPipeline {
    pipeline: wgpu::RenderPipeline,
    sky_buffer: super::alloc::TrackedBuffer,
    sky_bind_group: wgpu::BindGroup,
    vertex_buffer: super::alloc::TrackedBuffer,
    index_buffer: super::alloc::TrackedBuffer,
    index_count: u32,
    /// Whether the last uniform written was [`Self::write_sky_over_horizon`]'s:
    /// the stars are then drawn OVER the sky gradient rather than under it
    /// (`TilePipeline::render_with_space_and_raster`).
    over_sky: std::sync::atomic::AtomicBool,
}

impl StarPipeline {
    /// Builds the pipeline and uploads the whole sky.
    ///
    /// `depth_format` is the tile pass's, because the stars are drawn INTO
    /// the tile pass — see [`Self::draw`] for why they are not a pass of
    /// their own, and for the depth state.
    #[must_use]
    pub fn new(
        ctx: &super::GpuContext,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps star shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("star_shader.wgsl").into()),
        });

        let sky_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("star-sky-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let sky_buffer = super::alloc::TrackedBuffer::new(
            &ctx.device,
            &wgpu::BufferDescriptor {
                label: Some("star-sky-uniform"),
                size: std::mem::size_of::<StarUniform>() as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        );
        let sky_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("star-sky-bind-group"),
            layout: &sky_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: sky_buffer.as_entire_binding() }],
        });

        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("star-pipeline-layout"),
            bind_group_layouts: &[Some(&sky_bgl)],
            immediate_size: 0,
        });

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("star-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<StarVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &STAR_VERTEX_ATTRS,
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            // The tile pass HAS a depth attachment, so a pipeline drawing
            // into it must declare a matching format -- but the stars
            // neither test nor write depth:
            //
            //  - `depth_write_enabled: false`, because a star quad is at
            //    infinity and writing its depth would let it reject the
            //    planet's own fragments at the same pixel;
            //  - `depth_compare: Always`, because the stars are drawn FIRST
            //    and the far side of the planet is hidden by
            //    `shader.wgsl`'s `globe_clipping_z`, not by depth, so there
            //    is nothing in the buffer for them to be behind.
            //
            // Occlusion by the planet is draw ORDER: stars, then the
            // backdrop sphere, then the tiles. MapLibre's own globe-only
            // full-screen pass is `DepthMode.ReadOnly` for the same reason
            // (`draw_sky.ts:84`).
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
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
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let (vertices, indices) = star_mesh();
        let vertex_buffer = super::alloc::TrackedBuffer::init(
            &ctx.device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("star-vertices"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            },
        );
        let index_buffer = super::alloc::TrackedBuffer::init(
            &ctx.device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("star-indices"),
                contents: bytemuck::cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            },
        );

        Self {
            pipeline,
            sky_buffer,
            sky_bind_group,
            vertex_buffer,
            index_buffer,
            index_count: indices.len() as u32,
            over_sky: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Rewrites the sky uniform. The **only** per-frame work the star field
    /// costs: one 96-byte `write_buffer`, over a matrix that is one
    /// perspective and two rotations. The 1.24 MB of vertices never moves.
    pub fn write_sky(&self, ctx: &super::GpuContext, camera: &Camera, space: &Space) {
        self.over_sky.store(false, std::sync::atomic::Ordering::Relaxed);
        ctx.queue.write_buffer(
            &self.sky_buffer,
            0,
            bytemuck::bytes_of(&StarUniform::new(camera, space)),
        );
    }

    /// [`Self::write_sky`], with the sky turned by the Greenwich sidereal
    /// angle `sidereal_rad` -- what the space look (`render::space_look`)
    /// writes, so the point stars stay registered with its Milky Way
    /// texture under a scene clock. `0.0` is exactly [`Self::write_sky`].
    pub fn write_sky_turned(&self, ctx: &super::GpuContext, camera: &Camera, space: &Space, sidereal_rad: f64) {
        self.over_sky.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut uniform = StarUniform::new(camera, space);
        if sidereal_rad != 0.0 {
            uniform.sky_view_proj = sky_view_proj_turned(camera, sidereal_rad).as_mat4().to_cols_array_2d();
        }
        ctx.queue.write_buffer(&self.sky_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    /// **The stars as a ground observer's night sky**: the uniform for a
    /// pitched view that sees above the horizon (`MapView::set_sky_stars`).
    /// Only stars above the observer's horizon draw, faded toward it
    /// ([`horizon_fade`]), at `intensity` ([`sky_star_intensity`]); and the
    /// field is drawn OVER the sky gradient (which would hide it), still
    /// under every tile, extrusion and model.
    pub fn write_sky_over_horizon(&self, ctx: &super::GpuContext, camera: &Camera, intensity: f32, size_scale: f32) {
        self.over_sky.store(true, std::sync::atomic::Ordering::Relaxed);
        ctx.queue.write_buffer(&self.sky_buffer, 0, bytemuck::bytes_of(&StarUniform::over_horizon(camera, intensity, size_scale)));
    }

    /// Whether the stars go over the sky gradient this frame (see
    /// [`Self::write_sky_over_horizon`]).
    #[must_use]
    pub fn over_sky(&self) -> bool {
        self.over_sky.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Draws the whole sky into an ALREADY-OPEN pass, mirroring
    /// `PlatePipeline::render`'s shape.
    ///
    /// Into the tile pass rather than a pass of its own because a second
    /// `begin_render_pass` would have to `LoadOp::Load` the colour
    /// attachment the first one just cleared, which on a tile-based GPU is
    /// a full-screen read of a buffer that never needed to leave tile
    /// memory -- the same cost `TilePipeline::render`'s `StoreOp::Discard`
    /// comment measures at 23.8 MB a frame for depth.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.sky_bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..self.index_count, 0, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Angular separation between two unit directions, in ARCSECONDS.
    fn sep_arcsec(a: glam::DVec3, b: glam::DVec3) -> f64 {
        a.dot(b).clamp(-1.0, 1.0).acos().to_degrees() * 3600.0
    }

    /// **The shipped catalogue really is the Yale Bright Star Catalogue, at
    /// the positions those stars are actually at.**
    ///
    /// The blob is 45,480 opaque bytes; nothing else in this crate can tell
    /// whether they were quantised correctly, byte-swapped, or generated
    /// from a different epoch. Five named stars, spread over both
    /// hemispheres and nine magnitudes, pin all three: the record layout (a
    /// byte-swap moves a star by degrees), the quantisation constants (a
    /// wrong divisor scales the whole sky), and the epoch (B1900 positions
    /// are up to ~1.4 degrees from J2000 ones, i.e. 5,000 arcseconds --
    /// four hundred times the tolerance below).
    ///
    /// The expected values are modern J2000 coordinates, NOT the
    /// catalogue's own, which is why the magnitude tolerance is 0.05 rather
    /// than the 0.031 the quantisation alone costs: BSC5 lists Arcturus at
    /// V = -0.04 and Polaris at 2.02 where modern photometry says -0.05 and
    /// 1.98. Positions agree far better -- every one of the five is within
    /// **11 arcseconds**, which is the quantisation step and nothing else.
    #[test]
    fn the_shipped_catalogue_puts_five_named_stars_where_they_belong() {
        // (name, ra_deg, dec_deg, V), J2000.
        const KNOWN: [(&str, f64, f64, f32); 5] = [
            ("Sirius (alpha CMa)", 101.287_16, -16.716_12, -1.46),
            ("Canopus (alpha Car)", 95.987_96, -52.695_72, -0.72),
            ("Arcturus (alpha Boo)", 213.915_30, 19.182_41, -0.05),
            ("Vega (alpha Lyr)", 279.234_74, 38.783_69, 0.03),
            ("Polaris (alpha UMi)", 37.954_56, 89.264_11, 1.98),
        ];
        // The quantisation's own worst case over all 9,096 records,
        // measured when the blob was generated: 10.90 arcseconds. A
        // tolerance of 12 leaves no room for a real position error to hide
        // in -- the next-nearest catalogue star to any of these five is
        // minutes of arc away, not seconds.
        const TOLERANCE_ARCSEC: f64 = 12.0;

        let stars: Vec<Star> = catalogue().collect();
        assert_eq!(
            stars.len(),
            STAR_COUNT,
            "the blob holds {} records of {STAR_RECORD_BYTES} bytes, not {STAR_COUNT}",
            stars.len()
        );

        for (name, ra, dec, vmag) in KNOWN {
            let want = sky_direction(ra, dec);
            let (best_sep, best) = stars
                .iter()
                .map(|star| {
                    let dir = sky_direction(f64::from(star.ra_deg), f64::from(star.dec_deg));
                    (sep_arcsec(want, dir), *star)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .expect("the catalogue is not empty");
            assert!(
                best_sep < TOLERANCE_ARCSEC,
                "{name} should be at ra {ra}, dec {dec}; the nearest shipped star is \
                 ra {} dec {} ({best_sep:.2} arcsec away), which is {:.0}x the \
                 quantisation step -- suspect the record layout or the epoch, not the \
                 rounding",
                best.ra_deg,
                best.dec_deg,
                best_sep / 10.9
            );
            assert!(
                (best.vmag - vmag).abs() < 0.05,
                "{name} should be V = {vmag}; the shipped record says {}",
                best.vmag
            );
        }
    }

    /// The catalogue's shape, pinned so a regenerated blob cannot quietly
    /// change what the sky is made of.
    ///
    /// Every figure here is one this module's docs or `PROVENANCE.md`
    /// quotes, which is the point: a doc comment nothing asserts is a
    /// comment.
    #[test]
    fn the_catalogue_is_the_size_and_range_the_docs_claim() {
        assert_eq!(STAR_BYTES.len(), 45_480, "the shipped blob's size");
        assert_eq!(STAR_BYTES.len(), STAR_COUNT * STAR_RECORD_BYTES);
        assert_eq!(
            STAR_COUNT * 12,
            109_152,
            "the three-f32 form PROVENANCE.md compares against"
        );

        let stars: Vec<Star> = catalogue().collect();
        let brightest = stars.iter().map(|s| s.vmag).fold(f32::INFINITY, f32::min);
        let faintest = stars.iter().map(|s| s.vmag).fold(f32::NEG_INFINITY, f32::max);
        // -1.46 and 7.96 before quantisation; `round(mag * 16) / 16` leaves
        // -1.4375 and 7.9375.
        assert_eq!(brightest, -1.4375, "the brightest shipped magnitude");
        assert_eq!(faintest, 7.9375, "the faintest shipped magnitude");
        assert!(
            stars.iter().all(|s| (0.0..360.0).contains(&s.ra_deg)),
            "every right ascension must decode into [0, 360)"
        );
        assert!(
            stars.iter().all(|s| (-90.0..=90.0).contains(&s.dec_deg)),
            "every declination must decode into [-90, 90]"
        );

        let (vertices, indices) = star_mesh();
        assert_eq!(vertices.len(), STAR_COUNT * 4);
        assert_eq!(indices.len(), STAR_COUNT * 6);
        assert_eq!(
            vertices.len() * std::mem::size_of::<StarVertex>(),
            1_018_752,
            "the static vertex buffer's size, which this module's docs quote"
        );
        assert_eq!(indices.len() * 4, 218_304, "the static index buffer's size");
        // Every direction must be a UNIT vector, or the billboard would sit
        // at a distance that varies with where in the sky it is -- which
        // `vs_main` divides by.
        for vertex in &vertices {
            let length = glam::Vec3::from(vertex.dir).length();
            assert!(
                (length - 1.0).abs() < 1e-5,
                "a star direction of length {length} is not a direction"
            );
        }
    }

    /// **The flat map's clear colour is the style's background colour,
    /// untouched.**
    ///
    /// Not "within a tolerance" and not "after a lerp that should be
    /// exact": bit-for-bit the value the flat path had before this module
    /// existed. Tasks 3, 5 and 7 each preserved bit-identical flat output
    /// and this change lands closer to the flat path than any of them did,
    /// so the guarantee is asserted on the equality operator.
    #[test]
    fn the_flat_maps_clear_colour_is_the_background_colour_untouched() {
        let background = Rgba { r: 0.311, g: 0.457, b: 0.603, a: 1.0 };
        // A garish space colour on purpose: if any of it leaks into the
        // flat answer the equality below fails on the first channel.
        let space = Space { color: Rgba { r: 0.9, g: 0.1, b: 0.2, a: 1.0 }, ..Space::default() };
        assert_eq!(
            space_clear_color(Some(background), &space, 0.0),
            background,
            "at transition 0.0 the clear colour must be the background, unmodified"
        );
        // Negative globeness is not reachable from `space_transition_for_zoom`,
        // but the early return is written `<= 0.0` and that is worth
        // pinning: a lerp with t < 0 would extrapolate AWAY from space.
        assert_eq!(space_clear_color(Some(background), &space, -1.0), background);
        // The other end: pure space, with nothing of the background left.
        assert_eq!(space_clear_color(Some(background), &space, 1.0), space.color);
        // Clamped above 1.0 rather than extrapolated, for the same reason.
        assert_eq!(space_clear_color(Some(background), &space, 2.0), space.color);
        // And in between: SPACE, not a blend toward the background. This
        // used to assert the half-way lerp, which is the grey sky the user
        // saw above the limb at z5 -- what is above the limb in the band is
        // `render::sky`'s pass now, over this colour.
        assert_eq!(space_clear_color(Some(background), &space, 0.5), space.color);
        assert_eq!(space_clear_color(Some(background), &space, 1e-6), space.color);

        // A style with no background layer: the flat path clears to
        // transparent today, so that is what transition 0.0 must still
        // give.
        assert_eq!(
            space_clear_color(None, &space, 0.0),
            Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }
        );
        assert_eq!(space_clear_color(None, &space, 1.0), space.color);
    }

    /// **Stars are off unless asked for, and never on a flat map.**
    ///
    /// Four applications compile this crate and none of them has heard of a
    /// star field. The default is the assertion.
    #[test]
    fn stars_are_off_by_default_and_never_drawn_flat() {
        let default = Space::default();
        assert_eq!(default.star_intensity, 0.0, "stars must default to OFF");
        assert!(!default.draws_stars(1.0), "a default Space draws no stars on a full globe");
        assert!(!default.draws_stars(0.0));

        let on = Space { star_intensity: 0.8, ..Space::default() };
        assert!(on.draws_stars(1.0));
        assert!(on.draws_stars(0.5));
        assert!(
            !on.draws_stars(0.0),
            "THE FLAT MAP MUST BE UNCHANGED: no stars at transition 0.0 however high \
             the intensity"
        );

        // A consumer writing to the public field directly cannot make the
        // shader see a nonsense alpha.
        assert_eq!(Space { star_intensity: -3.0, ..Space::default() }.star_alpha(), 0.0);
        assert_eq!(Space { star_intensity: 7.0, ..Space::default() }.star_alpha(), 1.0);
        assert!(!Space { star_intensity: -3.0, ..Space::default() }.draws_stars(1.0));
    }

    /// The default space colour must maximise the gap to the darkest
    /// basemap background this repo ships.
    ///
    /// That is the whole point of the task: the silhouette has to read, and
    /// `mapbox-standard-dark`'s `#0b0e13` is the tightest case there is. A
    /// space colour lighter than the planet in any channel would invert
    /// figure and ground; one merely darker leaves a gap so small the
    /// silhouette still does not read, which is what the first draft's
    /// `#05070f` did at a largest gap of **7** byte units.
    ///
    /// Pinned as a gap rather than as an inequality, because "darker" was
    /// the assertion that let `#05070f` through.
    #[test]
    fn the_default_space_colour_maximises_the_gap_to_the_darkest_shipped_background() {
        // #0b0e13, `mapbox-standard-dark`'s background-color.
        let darkest = [11u8, 14, 19];
        let space = Space::default().color;
        let sky = [space.r, space.g, space.b].map(|c| (c * 255.0).round() as i32);
        let gaps: Vec<i32> =
            (0..3).map(|i| i32::from(darkest[i]) - sky[i]).collect();
        assert!(
            gaps.iter().all(|gap| *gap > 0),
            "space {sky:?} must be darker than the darkest shipped background \
             {darkest:?} in EVERY channel, or the planet reads as the hole; gaps {gaps:?}"
        );
        // 19 is what pure black gives against #0b0e13, and it is the most
        // any colour can give: the background is the brightest thing on
        // the near-black end of the range this crate's styles cover.
        assert_eq!(
            gaps.iter().copied().max(),
            Some(19),
            "the default space colour must be as far from #0b0e13 as a colour can get, \
             i.e. black: gaps {gaps:?} from {sky:?}"
        );
        // The faintest star must have real contrast against it. 1.7% alpha
        // over black is byte 4; over the first draft's #05070f it was 19
        // against a backdrop of 15, which is nothing.
        let faint = star_brightness(7.9375);
        let over_space: Vec<i32> =
            (0..3).map(|i| (sky[i] as f32 + faint * (255.0 - sky[i] as f32)).round() as i32).collect();
        assert!(
            (0..3).all(|i| over_space[i] >= sky[i] * 2 + 2),
            "the faintest star reads {over_space:?} over a space colour of {sky:?}, which \
             is not enough contrast to see it"
        );
    }

    /// **The sky shares the GROUND's frame: everything that turns the
    /// planet turns the sky by exactly as much, and only the zoom does
    /// not.**
    ///
    /// # What this test used to assert, and why that was wrong
    ///
    /// Until this commit it was
    /// `the_sky_matrix_ignores_longitude_latitude_and_zoom_but_not_orientation`,
    /// and it asserted `sky_view_proj` **exactly equal** for two cameras
    /// differing in centre longitude, centre latitude or zoom — with the
    /// planet's matrix as the control, so the equality could not be
    /// satisfied by a camera change that changed nothing. Task 8 called
    /// that "an inertial frame" and sabotage-checked it: adding
    /// `Rx(lat) * Ry(-lon)` to the chain made it fail, and the failure was
    /// recorded as proof the test worked.
    ///
    /// **It was defending the defect.** The camera's centre longitude and
    /// latitude are not a property of the *picture*, they are where the
    /// eye IS relative to the planet — so they decide which way the eye
    /// LOOKS, and a camera that looks elsewhere sees other stars. With
    /// them omitted the sky responded to pitch and bearing alone, so the
    /// star field slid with the viewport while the planet turned under it:
    /// pan east and Orion stayed nailed to the same corner of the screen.
    /// It was seen and fixed twice.
    ///
    /// The correct statement is the one a camera orbiting a planet obeys:
    /// **the stars are fixed relative to the ground.** So the rotational
    /// part of the sky's chain must be the rotational part of the planet's,
    /// and nothing weaker — and the zoom, which moves the eye along its own
    /// axis without turning it, must still move no star at all.
    ///
    /// # Why the positive half is asserted through a screen position
    ///
    /// "The two chains share a rotation" cannot be asserted by comparing
    /// the two matrices, because only one of them has the translations and
    /// the near/far pair in it. It CAN be asserted where it is observable:
    /// a direction `d` taken very far out along the planet's own axes lands
    /// on the same pixel as the star in direction `d`, because in a
    /// right-handed perspective projection the screen x and y are
    /// `x_view / -z_view` scaled by the FOV — the near and far planes do
    /// not enter, and the translations fall away as `O(1/K)`. `K = 1e6`
    /// radii leaves that residue at about 1e-6 of a screen, i.e. below the
    /// 1e-4 tolerance by two orders of magnitude.
    /// **A ground observer's stars are drawn only above the horizon.** A
    /// pitched eye camera over Tokyo, tilted 80 degrees and turned: every
    /// catalogue star the horizon fade lets through lands ABOVE the
    /// horizon's row on screen (from the pinhole alone: `pitch - 90`
    /// degrees from the centre), and every star at or below the horizon
    /// gets no light at all. The fade rises smoothly from the horizon and
    /// is full by [`SKY_STAR_FADE_SIN`]; the space view's uniform has no
    /// fade.
    #[test]
    fn sky_stars_are_drawn_only_above_the_horizon() {
        assert_eq!(horizon_fade(-0.1, SKY_STAR_FADE_SIN), 0.0);
        assert_eq!(horizon_fade(0.0, SKY_STAR_FADE_SIN), 0.0);
        assert!(horizon_fade(0.1 * SKY_STAR_FADE_SIN, SKY_STAR_FADE_SIN) < 0.05, "faint just over the haze");
        let half = horizon_fade(0.5 * SKY_STAR_FADE_SIN, SKY_STAR_FADE_SIN);
        assert!(half > 0.4 && half < 0.6, "{half}");
        assert_eq!(horizon_fade(SKY_STAR_FADE_SIN, SKY_STAR_FADE_SIN), 1.0);
        assert_eq!(horizon_fade(-0.5, 0.0), 1.0, "no horizon in the space view");

        for (pitch_deg, bearing_deg) in [(80.0f32, 0.0f32), (75.0, 137.0), (66.0, 281.0)] {
            let camera = Camera {
                center_lon: 139.78,
                center_lat: 35.63,
                zoom: 20.0,
                pitch: pitch_deg.to_radians(),
                bearing: bearing_deg.to_radians(),
                viewport_width_px: 1200.0,
                viewport_height_px: 800.0,
                fov_y: 60f64.to_radians(),
            };
            let u = StarUniform::over_horizon(&camera, 0.8, 1.0);
            let up = glam::DVec3::new(f64::from(u.horizon[0]), f64::from(u.horizon[1]), f64::from(u.horizon[2]));
            let m = sky_view_proj(&camera);
            let h = f64::from(camera.viewport_height_px);
            let f = h / 2.0 / (camera.fov_y / 2.0).tan();
            let horizon_row = h / 2.0 - f * (90f64 - f64::from(pitch_deg)).to_radians().tan();
            let (mut above, mut below) = (0, 0);
            for star in catalogue() {
                let dir = sky_direction(f64::from(star.ra_deg), f64::from(star.dec_deg));
                let fade = horizon_fade(dir.dot(up) as f32, u.horizon[3]);
                let clip = m * dir.extend(1.0);
                if clip.w <= 0.0 {
                    continue;
                }
                let (x, y) = (clip.x / clip.w, clip.y / clip.w);
                if x.abs() > 1.0 || y.abs() > 1.0 {
                    continue;
                }
                let row = (1.0 - y) / 2.0 * h;
                if fade > 0.0 {
                    above += 1;
                    assert!(row < horizon_row + 0.5, "pitch {pitch_deg}: a lit star at row {row:.1}, the horizon at {horizon_row:.1}");
                } else {
                    below += 1;
                    assert!(row >= horizon_row - 0.5, "pitch {pitch_deg}: an unlit star at row {row:.1}, above the horizon at {horizon_row:.1}");
                }
            }
            assert!(above > 20, "pitch {pitch_deg}: {above} stars in the sky");
            assert!(below > 20, "pitch {pitch_deg}: {below} stars under the ground, unlit");
        }
        // The space view's uniform: no horizon.
        assert_eq!(StarUniform::new(&Camera { fov_y: Camera::DEFAULT_FOV_Y, center_lon: 0.0, center_lat: 0.0, zoom: 2.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 10.0, viewport_height_px: 10.0 }, &Space::default()).horizon[3], 0.0);
    }

    #[test]
    fn the_sky_shares_the_grounds_frame_and_only_the_zoom_leaves_it_alone() {
        let base = Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 1.5,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 512.0,
            viewport_height_px: 512.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };

        // --- 1. The positive statement: the sky's frame IS the ground's.
        //
        // `ndc` of a direction through the sky, against `ndc` of the same
        // direction taken K radii out along the planet's own axes.
        let ndc = |clip: glam::DVec4| glam::DVec2::new(clip.x / clip.w, clip.y / clip.w);
        const K: f64 = 1.0e6;
        for camera in [
            base,
            Camera { center_lon: 90.0, ..base },
            Camera { center_lat: 45.0, ..base },
            Camera { center_lon: -120.0, center_lat: 25.0, zoom: 2.5, ..base },
            Camera { center_lon: 37.0, center_lat: -64.0, pitch: 0.5, bearing: -1.1, ..base },
        ] {
            let sky = sky_view_proj(&camera);
            let planet = super::super::globe::globe_frame(&camera).globe_view_proj;
            // Directions NEAR THE SCREEN, chosen from the camera rather
            // than written down: the direction at the centre of the screen
            // is RA `lon + 180`, Dec `-lat` (see `sky_view_proj`'s frame
            // note), so these five are the centre and four neighbours
            // within 25 degrees of it. Points BEHIND the camera would pass
            // or fail this comparison on the strength of a divide by a
            // negative `w`, which is not a claim about any pixel.
            for (dra, ddec) in [(0.0, 0.0), (25.0, 0.0), (-25.0, 0.0), (0.0, 20.0), (-15.0, -15.0)]
            {
                let (ra, dec) = (camera.center_lon + 180.0 + dra, -camera.center_lat + ddec);
                let dir = sky_direction(ra, dec);
                // The PRECONDITION is read off the planet's matrix, never
                // the sky's: the planet's frame is the one under test, so
                // it is the one that decides whether this direction is a
                // direction the camera can see. Reading it off the sky
                // would let the defect disqualify its own witness — the
                // unrotated chain sends the direction at the centre of the
                // screen to `w = 0` at lon 90, and a precondition on `w`
                // would then skip exactly the case that matters.
                let planet_clip = planet * (dir * K).extend(1.0);
                let far_out = ndc(planet_clip);
                assert!(
                    planet_clip.w > 0.0 && far_out.length() < 4.0,
                    "the direction at RA {ra} Dec {dec} must be in front of this camera \
                     and near its screen for the comparison below to be about a pixel: \
                     w = {}, ndc {far_out:?}",
                    planet_clip.w
                );
                let on_the_sky = ndc(sky * dir.extend(1.0));
                assert!(
                    (on_the_sky - far_out).length() < 1e-4,
                    "the star at RA {ra} Dec {dec} lands at {on_the_sky:?} on the sky \
                     and the ground's own direction lands at {far_out:?}, at camera \
                     lon {} lat {} pitch {} bearing {} -- the sky and the planet do \
                     not share a frame, so the stars are not fixed to the ground",
                    camera.center_lon,
                    camera.center_lat,
                    camera.pitch,
                    camera.bearing
                );
            }
        }

        // --- 2. Everything that turns the planet turns the sky.
        let sky = sky_view_proj(&base);
        for (what, moved) in [
            ("longitude", Camera { center_lon: 90.0, ..base }),
            ("latitude", Camera { center_lat: 45.0, ..base }),
            ("pitch", Camera { pitch: 0.4, ..base }),
            ("bearing", Camera { bearing: 0.7, ..base }),
        ] {
            assert_ne!(
                sky_view_proj(&moved),
                sky,
                "the sky must turn with the camera's {what}: a camera that sits \
                 somewhere else on the planet, or faces somewhere else, sees other \
                 stars"
            );
            // The control, kept from the old test in the opposite role: the
            // planet must have moved too, so "the sky moved" is not being
            // satisfied by a camera change that moved everything trivially.
            assert_ne!(
                super::super::globe::globe_frame(&moved).globe_view_proj,
                super::super::globe::globe_frame(&base).globe_view_proj,
                "the control: moving the camera's {what} must move the planet"
            );
        }

        // --- 3. The zoom still moves no star. A star is at infinity, so
        // backing the eye off along its own axis cannot shift one, and
        // `SKY_NEAR`/`SKY_FAR` are constants for exactly that reason.
        for zoom in [0.0f64, 3.0, 5.9] {
            let moved = Camera { zoom, ..base };
            assert_eq!(
                sky_view_proj(&moved),
                sky,
                "zooming to {zoom} must leave the SKY untouched -- if this fails, \
                 `sky_view_proj` has picked up the globe's zoom-dependent near plane"
            );
            assert_ne!(
                super::super::globe::globe_frame(&moved).globe_view_proj,
                super::super::globe::globe_frame(&base).globe_view_proj,
                "the control: zooming to {zoom} must move the planet"
            );
        }

        // --- 4. And a whole turn of longitude is the SAME ground point
        // under the camera, so it must move neither the sky nor the
        // planet. This is what makes (2) a statement about the ground
        // rather than about arithmetic: the rotation is PERIODIC in
        // longitude, not accumulating in it.
        //
        // Approximate rather than bit-exact, and the reason is arithmetic
        // and not geometry: `sin` of the f64 nearest 2*PI is 2.45e-16, not
        // zero, so `Ry(-360deg)` is the identity to within an ULP and no
        // further. The same residue appears in the planet's own chain.
        let round = Camera { center_lon: base.center_lon + 360.0, ..base };
        assert!(
            sky_view_proj(&round).abs_diff_eq(sky, 1e-12),
            "360 degrees of longitude is the same ground point under the camera, so \
             the sky must come back to where it was: {:?} against {:?}",
            sky_view_proj(&round),
            sky
        );
        assert!(
            super::super::globe::globe_frame(&round)
                .globe_view_proj
                .abs_diff_eq(super::super::globe::globe_frame(&base).globe_view_proj, 1e-9),
            "the control: 360 degrees of longitude must bring the PLANET back too"
        );
    }

    /// **A bearing turns the sky by exactly as much as it turns the
    /// planet**, in the same direction.
    ///
    /// `sky_view_proj`'s `Rz(bearing)` carries the sign, and a sign error
    /// there is invisible to the equality test above (which only asks
    /// whether the matrix moved) and invisible in a single screenshot: a
    /// mirrored sky still looks like a sky. So the assertion is not against
    /// a compass convention this test would have to get right on its own --
    /// the first draft asserted `+bearing` and measured `-bearing`, which
    /// says nothing about the renderer -- but against the PLANET, measured
    /// through `globe_frame`'s own matrix at the same camera.
    ///
    /// Both come out at **exactly `-bearing`** under the screen convention
    /// below, and that they agree is a consequence worth writing down:
    /// `Rz(bearing)` is the same factor in the same position of both chains,
    /// and at pitch 0 the translations that distinguish them
    /// (`T(0,0,-radius)`, `T(0,0,-d)`) only touch z, which the screen angle
    /// divides out.
    #[test]
    fn a_bearing_turns_the_sky_exactly_as_much_as_it_turns_the_planet() {
        let base = Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 1.5,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 512.0,
            viewport_height_px: 512.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        // Clockwise from straight up, rows increasing downward.
        let screen_angle = |clip: glam::Vec4| {
            let ndc = clip.truncate() / clip.w;
            let x = f64::from((ndc.x + 1.0) * 0.5 * 512.0);
            let y = f64::from((1.0 - ndc.y) * 0.5 * 512.0);
            (x - 256.0).atan2(256.0 - y)
        };
        // The sky 20 degrees north of the screen centre's own direction
        // (RA 180, Dec 0 -- see `sky_view_proj`), and the PLANET's surface
        // point 20 degrees north of the camera's centre. Both project
        // straight up from the centre at bearing 0.
        let sky_dir = sky_direction(180.0, 20.0);
        let planet_dir = super::super::globe::mercator_to_sphere(
            0.5,
            crate::geo::lon_lat_to_tile_xy(0.0, 20.0, 0.0).1,
        );
        let sky_angle = |camera: &Camera| {
            screen_angle(
                sky_view_proj(camera).as_mat4()
                    * glam::Vec4::new(sky_dir.x as f32, sky_dir.y as f32, sky_dir.z as f32, 1.0),
            )
        };
        let planet_angle = |camera: &Camera| {
            screen_angle(
                super::super::globe::globe_frame(camera).globe_view_proj.as_mat4()
                    * glam::Vec4::new(
                        planet_dir.x as f32,
                        planet_dir.y as f32,
                        planet_dir.z as f32,
                        1.0,
                    ),
            )
        };

        assert!(
            sky_angle(&base).abs() < 1e-5 && planet_angle(&base).abs() < 1e-5,
            "at bearing 0 both must be straight up from the centre: sky {:.5} rad, \
             planet {:.5} rad",
            sky_angle(&base),
            planet_angle(&base)
        );
        for bearing in [0.3f32, 1.0, -0.6] {
            let camera = Camera { bearing, ..base };
            let (sky, planet) = (sky_angle(&camera), planet_angle(&camera));
            assert!(
                (sky - planet).abs() < 1e-4,
                "at bearing {bearing} the sky turned {sky:.5} rad and the planet \
                 {planet:.5} rad -- a difference means the sky and the planet disagree \
                 about which way round the world is"
            );
            assert!(
                (sky + f64::from(bearing)).abs() < 1e-4,
                "and the measured rotation is -bearing: at {bearing} the sky moved \
                 {sky:.5} rad"
            );
        }
    }

    /// The two magnitude laws, with the figures this module's docs quote.
    ///
    /// Pinned as assertions because three of them are arithmetic a reader
    /// would otherwise have to redo: 5,623x of flux across the catalogue,
    /// 1.2 px for the faintest disc, 4.91 px for Sirius. The 5,623 is here
    /// because the first draft of `star_brightness`'s doc comment said
    /// 1,560 -- the ratio from magnitude 0, not from the catalogue's
    /// brightest -- and nothing but this assertion noticed.
    #[test]
    fn brightness_follows_pogsons_law_and_size_follows_magnitude() {
        // Five magnitudes are exactly a factor of 100 -- the definition of
        // the scale, not a fit. Measured either side of `STAR_MAG_FULL`
        // deliberately FAINTWARD of it: the first draft of this assertion
        // used magnitudes 1 and 6 and read 10x, because magnitude 1 is
        // inside the clamp and the ratio it measured was half the law and
        // half the clamp.
        let ratio = star_brightness(4.0) / star_brightness(9.0);
        assert!(
            (ratio - 100.0).abs() < 1e-3,
            "five magnitudes must be 100x of flux; got {ratio}"
        );
        // Clamped at STAR_MAG_FULL and above, so the first magnitudes stay
        // distinguishable from each other in SIZE rather than saturating
        // alpha.
        assert_eq!(star_brightness(STAR_MAG_FULL), 1.0);
        assert_eq!(star_brightness(-1.4375), 1.0);
        assert!(star_brightness(6.5) < 0.07 && star_brightness(6.5) > 0.05);

        // The catalogue's own flux range, which is why brightness and size
        // are different laws.
        let flux = 10.0f32.powf(0.4 * (7.9375 + 1.4375));
        assert!(
            (flux - 5_623.0).abs() < 10.0,
            "the shipped catalogue spans {flux:.0}x of flux, not the ~5,623 the docs claim"
        );

        // Sizes, as logical-pixel DIAMETERS.
        assert!((star_half_size_px(7.9375) * 2.0 - 1.2).abs() < 1e-5, "the faintest disc");
        assert!(
            (star_half_size_px(-1.4375) * 2.0 - 4.9125).abs() < 1e-4,
            "Sirius's disc: {}",
            star_half_size_px(-1.4375) * 2.0
        );
        // Monotone: brighter is never smaller.
        let mut previous = 0.0f32;
        let mut mag = 8.0f32;
        while mag > -2.0 {
            let size = star_half_size_px(mag);
            assert!(size >= previous, "size must not shrink as magnitude falls, at {mag}");
            previous = size;
            mag -= 0.25;
        }
    }

    /// The ocean sphere is a whole sphere: subdivided, pole-capped, and one
    /// tile whose Mercator span is everything.
    ///
    /// `BACKDROP_TILE` being `z0/0/0` is load-bearing in a way the literal
    /// does not advertise -- it is the only tile for which
    /// `tile_mercator_coords` is `(0, 0, 1, 1)` -- so that is asserted
    /// rather than described, and so is the pole handling the backdrop needs
    /// to close at +/-90 instead of stopping at Mercator's +/-85.051.
    #[test]
    fn the_backdrop_is_one_pole_capped_sphere_spanning_the_whole_mercator_square() {
        assert_eq!(
            super::super::globe::tile_mercator_coords(BACKDROP_TILE),
            [0.0, 0.0, 1.0, 1.0],
            "the backdrop tile must span all of Mercator space, or the ocean is a patch"
        );
        let subdivision = super::super::subdivide::Subdivision::for_tile(BACKDROP_TILE);
        assert!(
            subdivision.north_pole && subdivision.south_pole,
            "the backdrop must carry both pole caps: it is the only geometry that covers \
             the sphere everywhere, so a missing cap is a hole onto SPACE rather than \
             onto another tile"
        );
        assert!(
            subdivision.granularity >= 32,
            "granularity {} is too coarse for the thing whose edge IS the silhouette",
            subdivision.granularity
        );

        let colour = Rgba { r: 0.311, g: 0.457, b: 0.603, a: 1.0 };
        // The STOP PAIR really is carried as two colours, so a
        // zoom-expression background can be blended on the GPU by `blend_t`
        // instead of re-tessellating 4,225 vertices on every frame of a
        // zoom. See `backdrop_plan`.
        let other = Rgba { r: 0.9, g: 0.1, b: 0.2, a: 1.0 };
        let pair = backdrop_mesh(4096, colour, other);
        assert!(
            pair.vertices.iter().all(|v| {
                v.color == [colour.r, colour.g, colour.b, colour.a]
                    && v.color_to == [other.r, other.g, other.b, other.a]
            }),
            "the backdrop mesh must bake BOTH endpoints of the background colour's stop \
             pair, or a zoom-dependent background rebuilds it every frame"
        );

        let mesh = backdrop_mesh(4096, colour, colour);
        // `(g+1)^2` vertices and `g^2 * 6` indices for the grid
        // (`create_tile_mesh.ts:121-131`), plus the two pole caps.
        let grid_vertices = (subdivision.granularity as usize + 1).pow(2);
        assert!(
            mesh.vertices.len() > grid_vertices,
            "{} vertices is no more than the bare grid's {grid_vertices}, so the pole \
             caps contributed nothing",
            mesh.vertices.len()
        );
        assert!(
            mesh.vertices.iter().all(|v| v.color == [colour.r, colour.g, colour.b, colour.a]),
            "every backdrop vertex carries the background colour: the ocean is what the \
             style said the background was"
        );
        // The caps are marked in `is_stroke`, negative, so every existing
        // `is_stroke > 0.5` reader keeps treating them as fill.
        assert!(
            mesh.vertices.iter().any(|v| v.is_stroke < -0.5),
            "no pole-marked vertex in the backdrop mesh"
        );
    }
}
