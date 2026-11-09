//! The globe projection's camera-level maths. **No rendering lives here.**
//!
//! Modelled on MapLibre GL JS (BSD-3, `Copyright (c) 2023, MapLibre
//! contributors`):
//!   - `src/shaders/glsl/_projection_globe.vertex.glsl` — the sphere
//!     projection itself and the precision trick this module's
//!     [`mercator_y_to_sin_cos_lat`] ports.
//!   - `src/geo/projection/vertical_perspective_transform.ts:439-497` —
//!     `_calcMatrices`, the globe view-projection chain.
//!   - `src/geo/projection/vertical_perspective_transform.ts:307-370` —
//!     `_computeClippingPlane`, the horizon plane.
//!   - `src/geo/projection/globe_utils.ts:83-89` — `getGlobeRadiusPixels`.
//!   - `src/geo/transform_helper.ts:692-703` — `getMercatorTileCoordinates`.
//!
//! Nothing here is ported from any Mapbox GL JS 2.x/3.x tree; that codebase
//! left open source at v2.0 and is not a legitimate reference for this work.
//!
//! # Why this module is shaped the way it is
//!
//! MapLibre keeps **two live transforms** — a `MercatorTransform` and a
//! `VerticalPerspectiveTransform` — and `map.ts:4352` re-runs `_calcMatrices`
//! on both every single frame, which on the vertical-perspective side is an
//! eight-step matrix compose, **two `mat4.invert` calls**, a fresh
//! `Float32Array` allocation, a rotated camera position, a trigonometric
//! clipping plane and a `Frustum.fromInvProjectionMatrix`
//! (`vertical_perspective_transform.ts:439-497`). Then `getProjectionData`
//! calls **both** sub-transforms **per tile, per frame** and blends
//! (`globe_transform.ts:284-295`) — so Mercator matrices get rebuilt even in
//! full-globe mode, where they are multiplied by a transition of exactly zero.
//!
//! That is the precise defect shape this project has already measured and
//! fixed once: rebuilding a matrix per item instead of once per array was
//! **74.7×** on 69,080 coordinates (5.544 ms → 0.074 ms), and a subsequent
//! struct-of-arrays pass over the same data bought only 4% more — the win was
//! the setup leaving the loop, not the data layout. We will not reintroduce it
//! at frame scale.
//!
//! **So: [`globe_frame`] computes everything camera-level exactly ONCE per
//! frame — both matrices, the clipping plane and the transition scalar — and
//! [`tile_mercator_coords`] is deliberately the only per-tile work, four
//! divisions and no allocation. The shader blends; MapLibre's shader already
//! does the same, it is only their CPU that is redundant.**

use super::{Camera, WORLD_SIZE};
use crate::geo::TileId;
use std::f64::consts::PI;

/// The globe's radius in **metres**. MapLibre's `GLOBE_RADIUS`
/// (`_projection_globe.vertex.glsl:1`).
///
/// This is *not* the radius the view-projection matrix uses — that one is in
/// world units and depends on zoom and latitude (see
/// [`globe_radius_world`]). This metre figure exists for the one thing
/// MapLibre uses it for: converting a vertex elevation in metres into a
/// fraction of the sphere's radius, `spherePos * (1.0 + elevation /
/// GLOBE_RADIUS)` (`_projection_globe.vertex.glsl:105`). It is the IUGG mean
/// Earth radius.
pub const GLOBE_RADIUS_M: f64 = 6_371_008.8;

/// Zoom at and below which the planet is drawn as **a ball in space**
/// ([`space_transition_for_zoom`] returns exactly `1.0`): full ocean
/// backdrop, full atmosphere, full stars.
///
/// **Which end is the globe: ZOOMED OUT.** Low zoom = whole planet in view =
/// a ball with a limb, an air shell and a starfield behind it. High zoom = a
/// street corner, where none of those three mean anything: there is no limb
/// on screen to put air around and no black sky to put stars in. So
/// [`space_transition_for_zoom`] is **1.0 at `GLOBE_ZOOM_MIN` and below**,
/// falls to **0.0 at [`GLOBE_ZOOM_MAX`] and above**, and is monotonically
/// *decreasing* in between.
///
/// # This is NOT the projection blend — see [`transition_for_zoom`]
///
/// The two used to be one number, and that conflation was a rendering
/// defect: it put the **projection** blend at z3–z6, where the sphere and
/// Mercator disagree by *hundreds of screen pixels*, so every frame in the
/// band drew geometry belonging to neither projection (measured: 36 px of
/// horizontal and 38 px of vertical drift per vertex at a lat-60 camera at
/// z3.63, and 1,771 px at a pole-cap vertex, whose Mercator image is at
/// infinity). See [`transition_for_zoom`] for the number the projection
/// actually blends over and where MapLibre puts it.
///
/// Fading the *space* effects out by z6 is right and is unchanged: MapLibre
/// has no starfield to fade, and its own atmosphere blend multiplies by
/// globeness (`draw_sky.ts:90`) purely so the shell disappears once the
/// planet stops being a ball on screen — which is what this constant is for
/// here. The independent evidence that z6 is the right neighbourhood for
/// *that* is that MapLibre's `fill` subdivision granularity halves per zoom
/// and pins at its minimum of 2 from z6 onwards
/// (`vertical_perspective_projection.ts:13`, with
/// `subdivision_granularity_settings.ts:36-39`'s `max(base >> z, min, 1)`):
/// from z6 they stop spending vertices on curvature, which is only a sane
/// thing to do if there is no curvature left worth drawing.
pub const GLOBE_ZOOM_MIN: f64 = 3.0;

/// Zoom at and above which the planet is **no longer a ball in space**
/// ([`space_transition_for_zoom`] returns exactly `0.0`: no backdrop, no
/// atmosphere, no stars). See [`GLOBE_ZOOM_MIN`] for which end is which, and
/// for why this is not the projection blend.
pub const GLOBE_ZOOM_MAX: f64 = 6.0;

/// Zoom at and below which the geometry is **pure vertical perspective**
/// ([`transition_for_zoom`] returns exactly `1.0`) — no Mercator in the
/// blend at all.
///
/// **MapLibre's number, and now citable.** `createProjectionFromName`
/// expands the style value `"globe"` into
///
/// ```text
/// ['interpolate', ['linear'], ['zoom'], 11, 'vertical-perspective', 12, 'mercator']
/// ```
///
/// (`src/geo/projection/projection_factory.ts:40-50`). An earlier pass of
/// this file recorded that the expression "lives in the
/// `@maplibre/maplibre-gl-style-spec` package, which is not present in our
/// reference checkout" and picked z3–z6 instead. The expression is in the
/// checkout — it is hard-coded in the factory, not read from the spec
/// package — and the range it gives is z11–z12.
///
/// # Why the band has to be up here, and cannot be at z3–z6
///
/// `shader.wgsl`'s `interpolate_projection` mixes the **clip-space** image
/// of a vertex under the sphere with its image under Mercator. That mix is
/// only harmless where the two images are already in the same place. They
/// are not, at low zoom, and the gap is not small:
///
/// ```text
///   camera lat 60, 1024 px frame, transition 0.887 (z3.63 under the old band)
///   vertex              sphere px      Mercator px     blended px     error
///   lon 0   lat 60      119, 172       -280,  512       83,  203     -36, +31
///   lon -30 lat 60      206,  53       -544,  512      144,   91     -62, +38
/// ```
///
/// **And the pole cap comes apart**, which is the worst of it.
/// `render::subdivide::append_pole_caps` grows a fan whose apex vertices
/// all carry the same sphere position — `(0, ±1, 0)`, the pole itself, the
/// one `vs_main` substitutes for `project_to_sphere` — while their FLAT arm
/// is each apex's own longitude at the Mercator square's edge, ±85.051°.
/// Those are the same point on the sphere and a whole world apart on the
/// plane, so blending them fans a point into an arc:
///
/// ```text
///   apex longitude      sphere px      Mercator px     blended px
///   90 (screen centre)  512,  75       512, -408       512, 26
///   60                  512,  75       248, -408       486, 26
///   120                 512,  75       776, -408       538, 26
///   0                   512,  75      -280, -408       433, 26
///   180                 512,  75      1304, -408       591, 26
/// ```
///
/// One point becomes a 158 px smear, 49 px off the pole, varying smoothly
/// with longitude — which is what was reported as "smooth concentric arcs
/// sweeping around the pole", visible once the terminator darkened their
/// surroundings.
///
/// Together these are the lens, the gore and the egg: a silhouette fill
/// ratio of 0.991 at transition 1.000 falling to 0.935 at 0.884, with the
/// ray-marched atmosphere — which never blends, and is drawn from the
/// sphere frame alone — still a correct circle around it.
///
/// The disagreement is the sphere's curvature across the frame, so it halves
/// with every zoom level — and **there is no zoom below z10 where it is
/// small**. Worst on-screen vertex drift at `transition == 0.5`, 2048 px
/// frame, the same lat-60 camera, for a band placed at each zoom:
///
/// ```text
///   band midpoint   z4.5     z6.5     z8.5     z9.5    z10.5    z11.5
///   worst drift    969 px   215 px    43 px    21 px   10.5 px   5.3 px
/// ```
///
/// At the reference's band the whole blend is worth a few pixels of the
/// outermost corner, which is why it can afford to blend there — and why a
/// blend anywhere below z10 cannot be made to look right by any choice of
/// easing. Nor could it be replaced by a step: the two projections are 100
/// px apart at the frame corner at z6, so switching between them without a
/// blend simply concentrates the same error into one frame.
pub const PROJECTION_BLEND_ZOOM_MIN: f64 = 11.0;

/// Zoom at and above which the geometry is **pure Mercator**
/// ([`transition_for_zoom`] returns exactly `0.0`, and `vs_main` returns
/// before `project_to_sphere` is ever evaluated). The `12` of MapLibre's
/// `['interpolate', ['linear'], ['zoom'], 11, 'vertical-perspective', 12,
/// 'mercator']` — see [`PROJECTION_BLEND_ZOOM_MIN`].
pub const PROJECTION_BLEND_ZOOM_MAX: f64 = 12.0;

/// MapLibre's near plane for the globe matrix, in **pixels**
/// (`vertical_perspective_transform.ts:449`, `this._helper._nearZ = 0.5`).
/// Converted to world units by dividing by `2^zoom` — see
/// [`Camera::distance`]'s note on the unit relationship.
const GLOBE_NEAR_PX: f64 = 0.5;

/// Everything the globe shader needs that is a property of the **camera**,
/// computed ONCE PER FRAME. See the module docs for why this split exists.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobeFrame {
    /// Unit-sphere position → clip space. The vertex shader produces the
    /// unit-sphere position itself (from the tile's Mercator coords via
    /// [`mercator_y_to_sin_cos_lat`]) and multiplies by this.
    ///
    /// Narrow to `f32` only at upload, never before — same rule the rest of
    /// this crate's matrix chain follows (see `Camera::view_projection`).
    pub globe_view_proj: glam::DMat4,
    /// The same chain as [`Self::globe_view_proj`] **without the projection
    /// matrix in front**: unit-sphere position → VIEW space, the camera at
    /// the origin looking down `-z`.
    ///
    /// Nothing drawing the planet wants this — `globe_view_proj` is what
    /// goes in the uniform. It exists for `render::atmosphere`, which needs
    /// the inverse: where the eye is *in sphere space* and which sphere-space
    /// direction each pixel's ray points, so it can march a scattering
    /// integral through a shell around the planet. A projection matrix
    /// cannot answer either question — it has a near and a far plane in it,
    /// and a ray has neither — so inverting `globe_view_proj` would mean
    /// undoing `GLOBE_NEAR_PX` and the zoom-dependent far plane to get back
    /// to a quantity that never depended on them.
    ///
    /// **Computed as its own product rather than factored out of
    /// `globe_view_proj`, deliberately.** glam's `*` is left-associative, so
    /// writing `proj * (T * Rx * ...)` instead of `((proj * T) * Rx) * ...`
    /// is the same matrix in exact arithmetic and a few ULP away from it in
    /// f64 — and every globe pixel test in this crate was graded against the
    /// original bracketing. The two chains are therefore kept side by side
    /// and pinned together by
    /// `the_view_half_and_the_projection_compose_into_the_matrix_the_shader_gets`,
    /// which is the only thing standing between "two expressions" and "two
    /// expressions that have drifted".
    pub globe_view: glam::DMat4,
    /// The flat fallback: **world**-space position → clip space, i.e. exactly
    /// `Camera::view_projection_f64()`.
    ///
    /// Note this is world-space, not tile-space: MapLibre's
    /// `u_projection_fallback_matrix` is a *per-tile* matrix, but combining it
    /// with the tile model matrix is work this crate already does once per
    /// tile in `combined_tile_matrix`, and duplicating it here would be the
    /// per-tile matrix rebuild the module docs exist to prevent.
    pub mercator_view_proj: glam::DMat4,
    /// The horizon plane, `xyz` normal and `w` offset, in unit-sphere space. A
    /// point `p` on the sphere is on the **far** side (and must be clipped)
    /// when `dot(n, p) + w < 0`.
    ///
    /// Ported from `_computeClippingPlane`
    /// (`vertical_perspective_transform.ts:307-370`). The plane is the one
    /// through all the points where a tangent line from the eye touches the
    /// sphere, so it is a true silhouette, not an approximation by the
    /// equatorial plane — which matters most at low pitch and low zoom, where
    /// the eye is close enough to the sphere for the two to differ visibly.
    pub clipping_plane: [f64; 4],
    /// Globeness of the **projection**: `0.0` = pure Mercator, `1.0` = pure
    /// vertical perspective. See [`transition_for_zoom`] and
    /// [`PROJECTION_BLEND_ZOOM_MIN`].
    pub transition: f32,
    /// How much of a **ball in space** the planet still is: the backdrop,
    /// atmosphere and starfield fade, and nothing geometric. See
    /// [`space_transition_for_zoom`] and [`GLOBE_ZOOM_MIN`].
    ///
    /// **A second number, and the whole point of this task.** It was one
    /// number, and sharing it forced the projection blend down into z3–z6
    /// where the two projections disagree by hundreds of pixels. They answer
    /// different questions — "how much Mercator is in the matrix" and "is
    /// there still a limb on screen to put air around" — and the zooms at
    /// which each answer changes are five levels apart.
    pub space_transition: f32,
}

impl GlobeFrame {
    /// The same frame with **the planet switched off**: pure Mercator at
    /// every zoom, and no space around it.
    ///
    /// This is the one mechanism behind
    /// [`crate::map_view::MapView::set_globe_enabled`], and it is a method
    /// on the frame rather than a branch inside [`globe_frame`] so that the
    /// switch cannot reach anything else: the matrices, the clipping plane
    /// and the call counter are already computed and are left exactly as
    /// they were.
    ///
    /// # Why it zeroes BOTH transitions
    ///
    /// The two numbers answer different questions and are deliberately
    /// separate ([`transition_for_zoom`] on z11–z12,
    /// [`space_transition_for_zoom`] on z3–z6), so forcing one to zero is
    /// not automatically a reason to force the other. Here it is, and the
    /// argument is what [`space_transition_for_zoom`] is *for*: the
    /// backdrop, the scattering shell, the starfield, the space-tinted
    /// clear colour and the brand mark's light-on-dark ink all exist to
    /// dress **a limb** — the curved edge where the planet stops and space
    /// starts. Force [`Self::transition`] to `0.0` and there is no limb:
    /// the ground fills the viewport edge to edge as a flat plane. An
    /// atmosphere shell drawn around a sphere that is not being projected
    /// would be a ring of scattered light around nothing, and a starfield
    /// behind a plane that covers the frame is a clear colour nobody sees.
    /// So "globe off" means flat Mercator **and** no space dressing, which
    /// is the frame this renderer drew at z13 before the globe existed.
    ///
    /// The counter-argument, recorded because it is not silly: a consumer
    /// might want the starfield as decoration behind a zoomed-out flat
    /// world map, the way some flat web maps put a dark vignette outside
    /// the Mercator square. That is a *backdrop* feature and it wants its
    /// own switch on [`crate::render::space::Space`] — not a globeness of
    /// zero that still claims a planet is there. Making one switch mean two
    /// things is precisely the conflation that put the projection blend in
    /// the z3–z6 band in the first place.
    ///
    /// # What it guarantees
    ///
    /// `transition == 0.0` is not "nearly flat", it is the value every flat
    /// path in this crate already keys off: `shader.wgsl`'s
    /// `transition <= 0.0` early-out (which never reads
    /// [`Self::globe_view_proj`] or [`Self::clipping_plane`], which is why
    /// leaving those intact is safe), [`GlobeCover::new`] returning
    /// [`GlobeCover::FLAT`], and `GlobeAnchors::is_globe` returning
    /// `false`. A flattened frame is therefore byte-identical to the frame
    /// the same camera would produce above z12 — which is what
    /// `the_globe_switch_renders_a_banded_zoom_as_the_pure_mercator_frame`
    /// asserts, field by field.
    #[must_use]
    pub fn flattened(self) -> Self {
        if crate::projection::active().is_some() {
            return self;
        }
        Self { transition: 0.0, space_transition: 0.0, ..self }
    }

    /// [`Self::flattened`] when `enabled` is `false`, and `self` untouched
    /// when it is `true` — the shape a caller holding a `bool` wants, so
    /// that the branch lives here once instead of at each of
    /// `MapView::paint`'s and `MapView::cover_progress`' call sites.
    #[must_use]
    pub fn with_globe_enabled(self, enabled: bool) -> Self {
        if enabled {
            self
        } else {
            self.flattened()
        }
    }
}

/// The sphere's radius in **world units** for this camera.
///
/// MapLibre's `getGlobeRadiusPixels` (`globe_utils.ts:83-89`) is
/// `worldSize / (2*PI) / cos(lat)`; our world square is a fixed
/// [`WORLD_SIZE`] wide at every zoom (pixels are `world * 2^zoom`), so the
/// zoom factor drops out and only the latitude term remains.
///
/// The `cos(lat)` is not a fudge: it makes a feature at the map centre the
/// same size in pixels on the globe as it is on the flat map, which is what
/// keeps a zoom level meaning the same thing in both projections. Mercator
/// stretches by `1/cos(lat)` away from the equator, so the sphere has to grow
/// by the same factor as the centre moves poleward or the map would appear to
/// shrink as you panned north.
///
/// Clamped at the Web Mercator limit (±85.051129°, where
/// `crate::geo::lon_lat_to_tile_xy` already clamps) so a camera parked on a
/// pole cannot divide by a cosine of zero.
pub fn globe_radius_world(camera: &Camera) -> f64 {
    let lat = camera.center_lat.clamp(-85.051_129, 85.051_129).to_radians();
    WORLD_SIZE as f64 / (2.0 * PI) / lat.cos()
}

thread_local! {
    /// How many times this thread has called [`globe_frame`] since the last
    /// [`reset_globe_frame_calls`].
    ///
    /// **A thread-local `Cell`, not a global atomic, and that is the point.**
    /// `paint` runs on one thread, so a per-thread count IS the per-frame
    /// count; and a test asserting "exactly one" cannot be perturbed by the
    /// dozen other tests in the same binary that call [`globe_frame`] on their
    /// own threads, which a process-global counter would have needed a lock to
    /// survive.
    ///
    /// Not behind `probe`, for the reason `alloc::draw_calls_total` is not:
    /// this answers a structural question rather than measuring a cost, it is
    /// one non-atomic increment per FRAME, and the assertion it exists for
    /// (`sixty_four_tiles_in_one_frame_cost_one_globe_frame`) is worth running
    /// in the default test suite rather than only under a feature flag.
    static GLOBE_FRAME_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// This thread's [`globe_frame`] call count. See `GLOBE_FRAME_CALLS`.
#[must_use]
pub fn globe_frame_calls() -> u64 {
    GLOBE_FRAME_CALLS.with(std::cell::Cell::get)
}

/// Zeroes this thread's [`globe_frame`] call count.
pub fn reset_globe_frame_calls() {
    GLOBE_FRAME_CALLS.with(|calls| calls.set(0));
}

/// The globe's perspective matrix — view space → clip space, and the only
/// factor of [`GlobeFrame::globe_view_proj`] that is not in
/// [`GlobeFrame::globe_view`].
///
/// Near/far follow MapLibre's comment at
/// `vertical_perspective_transform.ts:450` — "just set the far plane far
/// enough - we will calculate our own z in the vertex shader anyway". The
/// shader replaces Z with the clipping-plane distance
/// (`_projection_globe.vertex.glsl:99-101,108`), so the depth range only has
/// to *contain* the sphere, not resolve it.
///
/// Its own function so that
/// `the_view_half_and_the_projection_compose_into_the_matrix_the_shader_gets`
/// can recompose the two halves without copying the near/far rule into a
/// test, which is where a silent disagreement would start.
fn globe_projection(camera: &Camera) -> glam::DMat4 {
    let near = globe_near(camera);
    let far = camera.distance() + globe_radius_world(camera) * 2.0;
    let aspect = camera.viewport_width_px as f64 / camera.viewport_height_px as f64;
    glam::dcamera::rh::proj::directx::perspective(
        camera.fov_y_rad(),
        aspect,
        near,
        far,
    )
}

/// The globe projection's near-plane distance (view-space units) for this
/// camera -- what [`globe_projection`] builds its matrix with, and what
/// `render::altitude::behind_near` culls against.
pub(crate) fn globe_near(camera: &Camera) -> f64 {
    GLOBE_NEAR_PX / 2f64.powf(camera.zoom)
}

/// Everything camera-level the globe needs, for one frame. See [`GlobeFrame`].
pub fn globe_frame(camera: &Camera) -> GlobeFrame {
    GLOBE_FRAME_CALLS.with(|calls| calls.set(calls.get() + 1));
    build_globe_frame(camera)
}

/// [`globe_frame`] without the per-thread call count: for the hover and
/// click pick ([`unproject_to_world_on_globe`]), which runs on input
/// events rather than per frame and must not register as a second
/// per-frame build in the counts `paint`'s tests grade.
pub(crate) fn build_globe_frame(camera: &Camera) -> GlobeFrame {
    if let Some(projection) = crate::projection::active() {
        return planar_frame(camera, projection);
    }
    let radius = globe_radius_world(camera);
    let d = camera.distance();
    let pitch = camera.pitch as f64;
    let bearing = camera.bearing as f64;

    // The globe matrix is built from scratch rather than reusing the Mercator
    // view matrix, exactly as `vertical_perspective_transform.ts:439-477`
    // does, because the two look at different things: the Mercator camera
    // looks at a point on a plane, the globe camera looks at a point on a
    // sphere whose centre sits a further `radius` behind it.
    //
    // Near/far follow MapLibre's comment at `:450` -- "just set the far plane
    // far enough - we will calculate our own z in the vertex shader anyway".
    // The shader replaces Z with the clipping-plane distance
    // (`_projection_globe.vertex.glsl:99-101,108`), so the depth range only
    // has to *contain* the sphere, not resolve it.
    let proj = globe_projection(camera);

    // The chain from `_calcMatrices` (`:462-475`), in the same left-to-right
    // order gl-matrix's `mat4.translate(out, a, v) == a * T(v)` produces.
    // Roll is omitted because this crate's `Camera` has no roll.
    //
    // Read right to left as transforms applied to a unit-sphere point:
    //   scale to the sphere's real radius; spin the sphere so the camera's
    //   centre lon/lat faces the eye; push the sphere's *centre* one radius
    //   away so its near surface sits at the origin; apply bearing, then
    //   pitch; then back off by the camera distance; then project.
    let globe_view_proj = proj
        * glam::DMat4::from_translation(glam::DVec3::new(0.0, 0.0, -d))
        * glam::DMat4::from_rotation_x(-pitch)
        * glam::DMat4::from_rotation_z(bearing)
        * glam::DMat4::from_translation(glam::DVec3::new(0.0, 0.0, -radius))
        * glam::DMat4::from_rotation_x(camera.center_lat.to_radians())
        * glam::DMat4::from_rotation_y(-camera.center_lon.to_radians())
        * glam::DMat4::from_scale(glam::DVec3::splat(radius));

    // THE SAME CHAIN, MINUS THE PROJECTION. Written out again rather than
    // factored out of the expression above, and the reason is in
    // `GlobeFrame::globe_view`'s doc comment: re-bracketing a matrix product
    // moves its last bits, and the bracketing above is the one every globe
    // pixel test in this crate was graded against. Edit one and you must
    // edit the other -- which is what
    // `the_view_half_and_the_projection_compose_into_the_matrix_the_shader_gets`
    // is there to notice.
    let globe_view = glam::DMat4::from_translation(glam::DVec3::new(0.0, 0.0, -d))
        * glam::DMat4::from_rotation_x(-pitch)
        * glam::DMat4::from_rotation_z(bearing)
        * glam::DMat4::from_translation(glam::DVec3::new(0.0, 0.0, -radius))
        * glam::DMat4::from_rotation_x(camera.center_lat.to_radians())
        * glam::DMat4::from_rotation_y(-camera.center_lon.to_radians())
        * glam::DMat4::from_scale(glam::DVec3::splat(radius));

    GlobeFrame {
        globe_view_proj,
        globe_view,
        mercator_view_proj: camera.view_projection_f64(),
        clipping_plane: clipping_plane(camera, d / radius),
        transition: transition_for_zoom(camera.zoom),
        space_transition: space_transition_for_zoom(camera.zoom),
    }
}

/// **A flat world projection's frame** (`crate::projection`, spike): the
/// globe's per-vertex path with the sphere point replaced by a plane point
/// `(x, y, 0)` in projection units, and this matrix looking straight at it.
///
/// The plane is scaled so one projection unit is `WORLD_SIZE / (2*PI)`
/// world units -- the globe's radius at the equator -- so a zoom level
/// means the same scale at the equator in every projection. The camera
/// centre's own plane point is what sits under the screen centre.
/// `clipping_plane` is `(0, 0, 0, code / 4)`: a positive `w` (the globe's
/// never is) is the shader's planar branch key, and with a zero normal the
/// far-side clip is the constant `1 - w`, which hides nothing.
fn planar_frame(camera: &Camera, projection: crate::projection::FlatProjection) -> GlobeFrame {
    let d = camera.distance();
    let pitch = camera.pitch as f64;
    let bearing = camera.bearing as f64;
    let scale = WORLD_SIZE as f64 / (2.0 * PI);
    let (cx, cy) = projection.forward(camera.center_lon.to_radians(), camera.center_lat.to_radians());
    let near = globe_near(camera);
    let far = d + scale * 8.0;
    let aspect = camera.viewport_width_px as f64 / camera.viewport_height_px as f64;
    let proj = glam::dcamera::rh::proj::directx::perspective(camera.fov_y_rad(), aspect, near, far);
    let globe_view = glam::DMat4::from_translation(glam::DVec3::new(0.0, 0.0, -d))
        * glam::DMat4::from_rotation_x(-pitch)
        * glam::DMat4::from_rotation_z(bearing)
        * glam::DMat4::from_scale(glam::DVec3::splat(scale))
        * glam::DMat4::from_translation(glam::DVec3::new(-cx, -cy, 0.0));
    GlobeFrame {
        globe_view_proj: proj * globe_view,
        globe_view,
        mercator_view_proj: camera.view_projection_f64(),
        clipping_plane: [0.0, 0.0, 0.0, f64::from(projection.shader_code()) / 4.0],
        transition: 1.0,
        space_transition: 0.0,
    }
}

/// A [`GlobeFrame`] narrowed to `f32` and pre-divided into everything a
/// tile's uniform needs that is **not** a property of the tile — built once
/// per frame, then asked for one tile at a time.
///
/// # This type is the divergence, expressed in the type system
///
/// [`Self::for_tile`] is a method on a value you must already hold, not a
/// free function taking a `&Camera`. That is deliberate: a free function
/// would let a per-tile loop call it with the camera and quietly rebuild the
/// globe matrix, the clipping plane and the transition once per tile, which
/// is precisely what `globe_transform.ts:284-295` does and precisely the
/// shape this project has already measured at **74.7×** (5.544 ms → 0.074 ms
/// on 69,080 coordinates; see the module docs). With the expensive half
/// hoisted into a value, the loop has nothing to rebuild.
///
/// What `for_tile` still does per tile is [`tile_mercator_coords`] — four
/// divisions — and one `powi` for the tile's pixel scale, the same `powi`
/// `camera::dash_scale` already performs per tile beside it. What it does NOT
/// do is touch a matrix.
///
/// The `f64` → `f32` narrowing happens here, once per frame, rather than at
/// each `for_tile`: `DMat4::as_mat4` is sixteen conversions, and doing them
/// per tile would be a smaller version of the same mistake. Narrowing at the
/// upload boundary and never before is the rule the rest of this crate's
/// matrix chain follows (`Camera::view_projection`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobeUpload {
    globe_matrix: [[f32; 4]; 4],
    clipping_plane: [f32; 4],
    transition: f32,
    /// See [`GlobeFrame::space_transition`] — carried here so a caller that
    /// already holds the frame's upload does not need the frame itself to
    /// decide whether to draw the backdrop, the air or the stars.
    space_transition: f32,
    lyon_to_tile_unit: f32,
    /// Logical pixels per world unit at the camera's CONTINUOUS zoom,
    /// `2^zoom`. Held rather than recomputed per tile, and continuous rather
    /// than floored — see [`super::GlobeParams::tile_units_per_px`] for why
    /// the floor `dash_scale` uses would make every line on the globe jump in
    /// thickness at each integer zoom.
    px_per_world_unit: f32,
    /// MapLibre's `getPixelScale()`, blended by globeness:
    /// `lerp(1.0, 1/cos(center_lat), transition)`
    /// (`globe_transform.ts:306-308`, with
    /// `vertical_perspective_transform.ts:405-407`).
    ///
    /// # What it is for, and what was wrong without it
    ///
    /// A stroke's extrusion is computed in **tile** units on the globe, and
    /// one tile unit is not one tile unit's worth of pixels there. The
    /// sphere's radius carries a `1/cos(center_lat)` ([`globe_radius_world`],
    /// which is what makes a feature at the map CENTRE the same size on the
    /// globe as on the flat map), so a tile unit at latitude `lat` projects
    /// to `cos(lat) / cos(center_lat)` times as many pixels as it does flat.
    /// `shader.wgsl`'s `project_line_thickness` already undoes the
    /// `cos(lat)`; this undoes the `cos(center_lat)`, and without it every
    /// stroke on the globe came out `1 / cos(center_lat)` too wide — 1.4x at
    /// lat 45, 2x at lat 60, and exact only on the equator.
    ///
    /// The reference does it in the same two pieces: `projectLineThickness`
    /// for the first (`_projection_globe.vertex.glsl:41-48`) and
    /// `u_ratio = ratioScale / pixelsToTileUnits(...)` for this one, where
    /// `ratioScale` IS `transform.getPixelScale()`
    /// (`draw_line.ts:231`, `line_program.ts:137`).
    ///
    /// **Folded into [`GlobeParams::tile_units_per_px`] rather than sent as
    /// its own uniform**, which is exact for every frame at
    /// `transition == 1.0` — i.e. every frame at or below
    /// [`PROJECTION_BLEND_ZOOM_MIN`], since `interpolate_projection`
    /// short-circuits there and the flat arm's `extrude_world` is never
    /// read. Inside the blend band the flat arm keeps the uncorrected
    /// factor; that residue is `(1/cos(center_lat) - 1)` weighted by
    /// `1 - transition` on an arm already weighted by `1 - transition`, over
    /// one zoom level, and it buys back a uniform slot.
    pixel_scale: f32,
    /// Unit direction toward the sun, in sphere space — see
    /// [`super::GlobeParams::sun_direction`].
    ///
    /// A plain `DVec3`-derived triple rather than a `render::atmosphere::Sun`
    /// deliberately, so that a caller can build one of these without a
    /// `Sun` to resolve. The caller resolves the `Sun` — once per frame, in
    /// `MapView::paint` — and hands over the direction; `web/src/app.rs`,
    /// which has no `Space` and so no `Sun`, hands over `DVec3::ZERO` and
    /// takes the unlit ground.
    ///
    /// The original reason was narrower and is now stale: `atmosphere` was
    /// not in the browser build's module list and `globe` was, so naming
    /// the type here would have dragged the scattering pass into wasm to
    /// carry three floats. That list — the hand-written
    /// `web/src/render/mod.rs` — no longer exists; `web/src/lib.rs`
    /// `#[path]`-includes `src/render/mod.rs` whole, so `atmosphere` IS in
    /// the browser build now and compiles there clean. The decoupling is
    /// kept because the second reason above outlived the first.
    sun_direction: [f32; 3],
    /// See [`super::GlobeParams::night_brightness`].
    night_brightness: f32,
}

impl GlobeUpload {
    /// Narrows one frame's [`GlobeFrame`] for upload. `extent` is the tile
    /// extent the meshes were tessellated at (`MVT_EXTENT`), which is what
    /// fixes the lyon-unit → in-tile-unit scale.
    ///
    /// `sun` is the unit direction from the planet's centre toward the sun
    /// in sphere space — `render::atmosphere::Sun::direction_sphere`,
    /// resolved ONCE per frame by the caller so the ground and the air are
    /// lit from the same instant (see `Sun::resolved`). `night_brightness`
    /// is `Atmosphere::night_brightness`, clamped by its accessor.
    #[must_use]
    pub fn new(
        frame: &GlobeFrame,
        camera: &Camera,
        extent: f32,
        sun: glam::DVec3,
        night_brightness: f32,
    ) -> Self {
        Self {
            globe_matrix: frame.globe_view_proj.as_mat4().to_cols_array_2d(),
            clipping_plane: frame.clipping_plane.map(|c| c as f32),
            transition: frame.transition,
            space_transition: frame.space_transition,
            sun_direction: [sun.x as f32, sun.y as f32, sun.z as f32],
            // A flat projection has no night side: no shading at all.
            night_brightness: if crate::projection::active().is_some() { 1.0 } else { night_brightness },
            // `1 / (COORD_SCALE * extent)`: this crate's vertex positions are
            // MVT coordinates times `mesh::COORD_SCALE`, and the shader's
            // `project_to_sphere` wants `[0, 1]` across the tile. See
            // `GlobeParams::lyon_to_tile_unit` for why this travels separately
            // rather than being folded into `tile_mercator_coords.zw` the way
            // MapLibre's `getMercatorTileCoordinates` folds it.
            lyon_to_tile_unit: 1.0 / (super::mesh::COORD_SCALE * extent),
            px_per_world_unit: 2f32.powf(camera.zoom as f32),
            // `lerp(mercator 1.0, vertical-perspective 1/cos(lat), globeness)`
            // -- `globe_transform.ts:306-308`. The same latitude clamp
            // `globe_radius_world` uses, and for the same reason: a camera
            // parked on a pole must not divide by a cosine of zero.
            pixel_scale: {
                let cos_lat = camera
                    .center_lat
                    .clamp(-85.051_129, 85.051_129)
                    .to_radians()
                    .cos() as f32;
                1.0 + (1.0 / cos_lat - 1.0) * frame.transition
            },
        }
    }

    /// This frame's uniform for one tile. **The only per-tile globe work
    /// there is** — see the type's own doc comment.
    #[must_use]
    pub fn for_tile(&self, tile: TileId) -> super::GlobeParams {
        let tile_world_size = WORLD_SIZE / 2f32.powi(i32::from(tile.z));
        super::GlobeParams {
            globe_matrix: self.globe_matrix,
            tile_mercator_coords: tile_mercator_coords(tile),
            clipping_plane: self.clipping_plane,
            transition: self.transition,
            lyon_to_tile_unit: self.lyon_to_tile_unit,
            // `/ pixel_scale` is MapLibre's `u_ratio`'s `ratioScale` --
            // see `Self::pixel_scale` for what it undoes and why it rides
            // here instead of in a uniform of its own.
            tile_units_per_px: 1.0
                / (tile_world_size * self.px_per_world_unit * self.pixel_scale),
            sun_direction: self.sun_direction,
            night_brightness: self.night_brightness,
        }
    }

    /// This frame's uniform for an **overlay mesh** -- a runtime GeoJSON
    /// source, or the route line -- whose local grid is measured from
    /// `origin_world` (`combined_overlay_matrix`'s origin).
    ///
    /// `shader.wgsl` recovers a vertex's Mercator position as
    /// `tile_mercator_coords.xy + tile_mercator_coords.zw * position *
    /// lyon_to_tile_unit`. A tile makes that its corner plus its span; an
    /// overlay is treated as **the z0 tile translated to its origin**: `xy`
    /// is the origin in `[0, 1]` Mercator, `zw` is the whole world, and
    /// `lyon_to_tile_unit` turns local grid units (`OVERLAY_UNITS_PER_WORLD`
    /// per world unit, `OVERLAY_COORD_SCALE` = 1) into world fractions. From
    /// there every vertex takes exactly the tiles' path: `project_to_sphere`,
    /// `interpolate_projection`'s Mercator-globe blend, the far-side clip,
    /// the tile-space stroke extrusion and the lighting.
    ///
    /// # Precision
    ///
    /// The flat arm is untouched: it still goes through the f64-combined
    /// `combined_overlay_matrix` off the origin, which is what keeps a
    /// street-zoom overlay exact, and above `PROJECTION_BLEND_ZOOM_MAX`
    /// (`transition == 0`) it is the only arm. The globe arm computes its
    /// `[0, 1]` Mercator position in f32, exactly as a tile's does -- the
    /// same quantisation (6e-8 of the world, 0.12 px at z12, halving per
    /// level out) -- plus the origin's own f32 rounding, at most half that
    /// again and constant across the mesh.
    #[must_use]
    pub fn for_overlay(&self, origin_world: (f64, f64)) -> super::GlobeParams {
        let world = f64::from(WORLD_SIZE);
        super::GlobeParams {
            globe_matrix: self.globe_matrix,
            tile_mercator_coords: [
                (origin_world.0 / world) as f32,
                (origin_world.1 / world) as f32,
                1.0,
                1.0,
            ],
            clipping_plane: self.clipping_plane,
            transition: self.transition,
            lyon_to_tile_unit: (1.0
                / (crate::overlay::geom::OVERLAY_UNITS_PER_WORLD
                    * f64::from(super::mesh::OVERLAY_COORD_SCALE)
                    * world)) as f32,
            // `for_tile` at z0: the whole world is the "tile".
            tile_units_per_px: 1.0 / (WORLD_SIZE * self.px_per_world_unit * self.pixel_scale),
            sun_direction: self.sun_direction,
            night_brightness: self.night_brightness,
        }
    }

    /// This frame's globeness, `0.0` = Mercator, `1.0` = sphere. Read by the
    /// phase reporting and by tests; the shader gets it through
    /// [`Self::for_tile`].
    #[must_use]
    pub fn transition(&self) -> f32 {
        self.transition
    }

    /// This frame's **space** fade — backdrop, atmosphere, stars, clear
    /// colour. See [`GlobeFrame::space_transition`]; it is not the
    /// projection's globeness and the two are five zoom levels apart.
    #[must_use]
    pub fn space_transition(&self) -> f32 {
        self.space_transition
    }
}

/// The CPU's half of the globe projection: where a **symbol anchor** lands
/// on screen, and whether the planet is in the way.
///
/// # Why symbols need a CPU twin of the shader at all
///
/// Every other thing this crate draws on the globe is drawn by
/// `shader.wgsl` from a `GlobeParams`, and the CPU never has to know where a
/// vertex ended up. A label is different in two ways at once. Its *position*
/// is decided on the CPU, in screen pixels, because collision placement is a
/// screen-space grid (`collision::CollisionIndex`) — so the anchor has to be
/// projected before anything can be measured against anything else. And its
/// *visibility* cannot come from the shader's far-side z clip
/// (`globe_clipping_z`), because a label is a screen-space billboard drawn by
/// the icon and glyph pipelines, which never see the sphere.
///
/// That is exactly MapLibre's arrangement, not a divergence: placement stays
/// in screen space from globe-projected points
/// (`vertical_perspective_transform.ts:419-438`, `projectTileCoordinates`),
/// and the far side is rejected by an **analytic plane test** per point —
/// `dotResult = plane·spherePos + plane.w; isOccluded = dotResult < 0.0`
/// (`:428-430`) — not by depth, not by backface culling. The final gate is
/// `placement.ts:831-833`, whose own comment reads "Do not show text or icons
/// that are occluded by the globe, even if overlap mode is 'always'!".
///
/// # The divergence this type carries, same as [`GlobeUpload`]'s
///
/// Built ONCE PER FRAME from a [`GlobeFrame`] and then asked one tile at a
/// time, for the reason the module docs give: MapLibre's
/// `getProjectionData` rebuilds both projections per tile per frame
/// (`globe_transform.ts:284-295`), and this project has already measured that
/// shape at **74.7×**. [`Self::for_tile`] does four divisions and copies
/// three already-computed values; it touches no matrix.
///
/// # Why f64 here and f32 in [`GlobeUpload`]
///
/// `GlobeUpload` narrows because its destination is a GPU uniform. This one's
/// destination is a pixel comparison against other pixels, and the anchor it
/// projects has already travelled through f64 (`combined_tile_matrix`
/// multiplies in f64 and narrows once). Keeping the sphere half in f64 costs
/// nothing measurable at a few hundred labels a frame and removes the
/// question of whether a label moved because the world curved or because a
/// matrix rounded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobeAnchors {
    globe_view_proj: glam::DMat4,
    clipping_plane: glam::DVec4,
    /// Globeness, `0.0` = Mercator. `f64` rather than `GlobeFrame`'s `f32`
    /// so the blend below is done in one precision throughout.
    transition: f64,
}

impl GlobeAnchors {
    /// A frame with no globe at all: [`Self::for_tile`] answers `None` and
    /// every caller takes the flat path it took before this type existed.
    ///
    /// Same idiom and same reason as [`GlobeCover::FLAT`] and
    /// `render::GlobeParams::FLAT` — the flat path must be reachable
    /// *without* building a globe frame, so that "the flat path is
    /// unchanged" is a structural fact rather than an arithmetic hope.
    pub const FLAT: Self = Self {
        globe_view_proj: glam::DMat4::ZERO,
        clipping_plane: glam::DVec4::ZERO,
        transition: 0.0,
    };

    /// This frame's anchor projection. Returns [`Self::FLAT`] — before
    /// reading a matrix — for any frame whose transition is zero, which is
    /// every frame above [`GLOBE_ZOOM_MAX`].
    #[must_use]
    pub fn new(frame: &GlobeFrame) -> Self {
        if frame.transition <= 0.0 {
            return Self::FLAT;
        }
        Self {
            globe_view_proj: frame.globe_view_proj,
            clipping_plane: glam::DVec4::from_array(frame.clipping_plane),
            transition: f64::from(frame.transition),
        }
    }

    /// Whether the globe is on at all this frame.
    #[must_use]
    pub fn is_globe(&self) -> bool {
        self.transition > 0.0
    }

    /// This frame's anchor projection for one tile, or `None` when the globe
    /// is off. `extent` is the tile extent the symbol plan's anchors are in
    /// (`mvt::MVT_EXTENT`).
    ///
    /// **The only per-tile globe work symbols do** — four divisions and three
    /// copies. See the type's own doc comment.
    #[must_use]
    pub fn for_tile(&self, tile: TileId, extent: f64) -> Option<TileAnchors> {
        if !self.is_globe() {
            return None;
        }
        let span = 1.0 / f64::from(1u32 << tile.z);
        Some(TileAnchors {
            mercator_origin: glam::DVec2::new(f64::from(tile.x) * span, f64::from(tile.y) * span),
            mercator_per_anchor_unit: span / extent,
            globe_view_proj: self.globe_view_proj,
            clipping_plane: self.clipping_plane,
            transition: self.transition,
        })
    }
}

/// One tile's worth of [`GlobeAnchors`]. See that type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileAnchors {
    mercator_origin: glam::DVec2,
    /// Mercator span per unit of the anchor's own coordinate system, i.e.
    /// `(1 / 2^z) / extent`. The shader reaches the same number the other way
    /// round, multiplying a lyon coordinate by `lyon_to_tile_unit` first
    /// (`GlobeParams::lyon_to_tile_unit`) and then by
    /// `tile_mercator_coords.zw`; a symbol anchor is in raw MVT units and has
    /// no `COORD_SCALE` on it, so the two factors are folded here instead.
    mercator_per_anchor_unit: f64,
    globe_view_proj: glam::DMat4,
    clipping_plane: glam::DVec4,
    transition: f64,
}

impl TileAnchors {
    /// This anchor's position on the unit sphere. `anchor` is in the tile's
    /// own MVT units, the coordinate system `style::SymbolFeature::anchor`
    /// carries.
    ///
    /// The CPU twin of `shader.wgsl`'s `project_to_sphere`, reached through
    /// [`mercator_to_sphere`] so the two cannot derive the sphere
    /// differently.
    #[must_use]
    pub fn sphere(&self, anchor: (f64, f64)) -> glam::DVec3 {
        let mercator = self.mercator_origin
            + glam::DVec2::new(anchor.0, anchor.1) * self.mercator_per_anchor_unit;
        mercator_to_sphere(mercator.x, mercator.y)
    }

    /// Whether a point on the unit sphere is on the **far** side of the
    /// planet, and so must not be labelled.
    ///
    /// `dotResult = plane·spherePos + plane.w; isOccluded = dotResult < 0.0`
    /// — `vertical_perspective_transform.ts:428-430`, verbatim in effect.
    /// The sign is the one [`GlobeFrame::clipping_plane`] documents: a unit
    /// normal with a negative `w`, positive on the near side.
    ///
    /// Deliberately a separate call from [`Self::project`] rather than a
    /// second return value. It needs no matrix — one `exp`, a `sin`, a `cos`
    /// and a dot product — so a caller that only wants the verdict (the
    /// line-label run test) does not pay for a 4×4 multiply, and a caller
    /// that only wants the pixel (a glyph quad inside a run already
    /// accepted) does not pay for the verdict twice.
    #[must_use]
    pub fn occluded(&self, sphere: glam::DVec3) -> bool {
        self.clipping_plane.truncate().dot(sphere) + self.clipping_plane.w < 0.0
    }

    /// The clip-space position of a sphere point, blended with the flat
    /// Mercator one by this frame's globeness.
    ///
    /// `interpolateProjection` (`_projection_globe.vertex.glsl:103-130`) and
    /// `shader.wgsl`'s `interpolate_projection`, **minus the z term**: `z`
    /// there is the far-side clip, not a depth, and a symbol does not go
    /// through the rasterizer that would act on it — [`Self::occluded`] is
    /// the CPU's equivalent and it is exact rather than ramped. `x`, `y` and
    /// `w` are mixed exactly as the shader mixes them (`:122`), so a label
    /// tracks its own geometry through the whole transition instead of
    /// snapping onto the sphere at one end of it.
    ///
    /// `flat_clip` comes in already projected, for the same reason the
    /// shader takes it as a parameter: this crate's flat path is
    /// `combined_tile_matrix`, multiplied in f64 and narrowed once, and
    /// recomputing it here would be a second, differently-rounded answer to
    /// a question already answered.
    #[must_use]
    pub fn project(&self, flat_clip: glam::DVec4, sphere: glam::DVec3) -> glam::DVec4 {
        let globe_clip = self.globe_view_proj * sphere.extend(1.0);
        if self.transition > 0.999 {
            // Pure globe: nothing to blend, and the reference short-circuits
            // at the same threshold (`:110-113`).
            return globe_clip;
        }
        flat_clip + (globe_clip - flat_clip) * self.transition
    }
}

/// The horizon clipping plane, in unit-sphere space.
///
/// A direct port of `_computeClippingPlane`
/// (`vertical_perspective_transform.ts:307-370`), with MapLibre's own diagram
/// re-stated: `C` is the globe centre, `B` the point on the sphere the camera
/// looks at, `A` the point level with the camera directly above `C`, `T` any
/// point where a tangent from the eye grazes the surface. Everything is scaled
/// so the sphere's radius is 1, which is why the only input besides the
/// camera's angles is `distance_camera_to_b` **in radii**.
/// Where a **world point** lands on screen (logical pixels) on this frame,
/// sphere included: `project_world_to_screen_f64` on a flat frame -- the
/// same f64 matrix, the same arithmetic, so a flat caller cannot move by one
/// ULP -- and on a globe frame the same Mercator-globe blend `shader.wgsl`
/// gives an overlay vertex at that point (`GlobeUpload::for_overlay`), with
/// `None` for a point the planet hides.
///
/// For the CPU-placed things that sit ON an overlay -- the navigation puck
/// and the route line's label-collision boxes -- so they stay on the line
/// the GPU drew. `anchors` is the frame's `GlobeAnchors::new(&globe_frame)`.
#[must_use]
pub(crate) fn project_world_to_screen_on_frame(
    camera: &Camera,
    anchors: &GlobeAnchors,
    world: (f64, f64),
) -> Option<(f32, f32)> {
    WorldProjector::new(camera, anchors).project(world)
}

/// [`project_world_to_screen_on_frame`] for many points: the f64
/// view-projection matrix and the z0 tile's anchors are built once, in
/// [`Self::new`], rather than once per point. The same arithmetic, so a
/// point projects bit-identically either way.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WorldProjector<'a> {
    camera: &'a Camera,
    view_projection: glam::DMat4,
    /// `None` on a flat frame.
    tile: Option<TileAnchors>,
}

impl<'a> WorldProjector<'a> {
    #[must_use]
    pub(crate) fn new(camera: &'a Camera, anchors: &GlobeAnchors) -> Self {
        // The z0 tile with an extent of one world: an anchor unit IS a world
        // unit, which is `for_overlay`'s "the whole world is the tile".
        let tile = anchors.for_tile(TileId { z: 0, x: 0, y: 0 }, f64::from(WORLD_SIZE));
        Self { camera, view_projection: camera.view_projection_f64(), tile }
    }

    /// Where `world` lands on screen (logical pixels); `None` for a point
    /// the planet or the near plane hides.
    #[must_use]
    pub(crate) fn project(&self, world: (f64, f64)) -> Option<(f32, f32)> {
        let Some(tile) = self.tile else {
            return super::project_world_to_screen_f64_with_matrix(self.camera, &self.view_projection, world);
        };
        let flat = self.view_projection * glam::DVec4::new(world.0, world.1, 0.0, 1.0);
        let sphere = tile.sphere(world);
        if tile.occluded(sphere) {
            return None;
        }
        // `shader.wgsl` blends AFTER its horizontal flip; do the same, then
        // undo it for `project_clip_to_screen`, which re-applies it -- the same
        // dance `symbol_pass`'s `screen_at` does.
        let mut flipped = flat;
        flipped.x = -flipped.x;
        let mut clip = tile.project(flipped, sphere);
        clip.x = -clip.x;
        super::project_clip_to_screen(self.camera, clip.as_vec4())
    }
}

/// The **world point under a screen pixel** (logical pixels) on the frame
/// this camera draws, sphere included -- the inverse of
/// [`project_world_to_screen_on_frame`], and what hover and click picking
/// must use: `unproject_to_ground_f64` intersects the FLAT Mercator ground
/// plane, which is only what is on screen once the globe transition is
/// zero (above z12). On the z3-z6 globe it lands 40-450 px (logical) away
/// from the pixel under the cursor, growing with distance from the centre.
///
/// `globe_enabled` is `MapView::set_globe_enabled`'s switch: `false` is the
/// flat frame at every zoom, exactly `unproject_to_ground_f64`.
///
/// On a pure globe frame (transition 1, the shader's own short-circuit) the
/// answer is exact: the pixel's ray through the inverse globe matrix, hit
/// against the unit sphere, and the sphere point taken back to Mercator.
/// `None` when the ray misses the planet (the cursor is in space). Inside
/// the z11-z12 blend the drawn position is a mix of the two clip positions,
/// which has no closed-form inverse, so the nearer of the two exact answers
/// is refined by Newton's method against the forward projection itself.
///
/// World x is wrapped into `[0, WORLD_SIZE)`.
#[must_use]
pub fn unproject_to_world_on_globe(
    camera: &Camera,
    globe_enabled: bool,
    screen_x: f32,
    screen_y: f32,
) -> Option<glam::DVec2> {
    let flat = || super::unproject_to_ground_f64(camera, screen_x, screen_y).map(|p| p.truncate());
    let transition = if globe_enabled { transition_for_zoom(camera.zoom) } else { 0.0 };
    if transition <= 0.0 {
        return flat();
    }
    let frame = build_globe_frame(camera);
    let anchors = GlobeAnchors::new(&frame);
    let on_sphere = sphere_world_under(&frame, camera, screen_x, screen_y);
    if transition > 0.999 {
        return on_sphere;
    }
    // The blend band: refine from both exact ends and keep the best
    // answer, and only if it really lands on the pixel -- at the limb, or
    // off the drawn surface, Newton can stall, and a stalled iterate is
    // not a pick.
    let target = glam::DVec2::new(f64::from(screen_x), f64::from(screen_y));
    [on_sphere, flat()]
        .into_iter()
        .flatten()
        .filter_map(|start| refine_on_frame(camera, &anchors, start, target))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, residual)| *residual <= PICK_TOLERANCE_PX)
        .map(|(world, _)| world)
}

/// How far (logical pixels) a solved pick or anchor may land from its
/// pixel and still count: a hover a third of a pixel off is the pixel.
const PICK_TOLERANCE_PX: f64 = 0.35;

/// Whether this camera's frame is drawn on the globe at all: the switch
/// is on and the zoom is inside the projection's globe band (below z12).
/// Everything cursor-anchored keeps its exact flat arithmetic when this
/// is `false`.
#[must_use]
pub fn globe_active(camera: &Camera, globe_enabled: bool) -> bool {
    globe_enabled && transition_for_zoom(camera.zoom) > 0.0
}

/// **The centre that puts `world` under `pixel`** on this camera's globe
/// frame, bearing, pitch and zoom unchanged -- MapLibre's
/// `setLocationAtPoint` (`vertical_perspective_transform.ts`), which every
/// cursor-anchored gesture there goes through on the globe: the grabbed
/// point of a drag, the point under the cursor of a wheel or pinch zoom,
/// the anchor of a rotation.
///
/// Solved numerically rather than by MapLibre's closed form, because this
/// crate's frame is not a pure sphere inside the z11-z12 blend and the
/// sphere's radius follows the centre latitude
/// ([`globe_radius_world`]'s `1/cos(lat)`): Newton's method on the centre
/// lon/lat against [`project_world_to_screen_on_frame`], the renderer's
/// own forward projection, so the answer is where the point is DRAWN.
/// `None` when no centre within tolerance exists (the point would sit
/// behind the planet, or the pixel is off it): callers fall back.
#[must_use]
pub fn center_placing_world_at(
    camera: &Camera,
    world: glam::DVec2,
    pixel: (f32, f32),
) -> Option<(f64, f64)> {
    center_placing_world_at_nearest(camera, world, pixel, None)
        .filter(|(_, miss, _)| *miss <= PICK_TOLERANCE_PX)
        .map(|(c, _, _)| c)
}

/// [`center_placing_world_at`] for a globe DRAG: every candidate centre
/// carries [`globe_size_zoom_adjustment`], so the answer draws `world`
/// under `pixel` on the frame the drag will actually show -- the planet
/// held at its on-screen size while the centre's latitude changes.
/// Returns the centre; its zoom is `camera.zoom` plus the adjustment,
/// clamped to `zoom_range`.
#[must_use]
pub(crate) fn center_placing_world_at_keeping_size(
    camera: &Camera,
    world: glam::DVec2,
    pixel: (f32, f32),
    zoom_range: (f64, f64),
) -> Option<(f64, f64)> {
    center_placing_world_at_nearest(camera, world, pixel, Some(zoom_range))
        .filter(|(_, miss, _)| *miss <= PICK_TOLERANCE_PX)
        .map(|(c, _, _)| c)
}

/// **The zoom change that keeps the planet the same size on screen** when
/// the centre moves from `old_lat` to `new_lat`: MapLibre's
/// `getZoomAdjustment` (`globe_utils.ts:349`, BSD-3),
/// `log2(cos(new) / cos(old))`. The sphere's radius is
/// `2^zoom / cos(lat)` ([`globe_radius_world`], whose Web Mercator clamp
/// both latitudes share), so this is exactly the zoom that cancels it.
#[must_use]
pub(crate) fn globe_size_zoom_adjustment(old_lat: f64, new_lat: f64) -> f64 {
    let cos = |lat: f64| lat.clamp(-85.051_129, 85.051_129).to_radians().cos();
    (cos(new_lat) / cos(old_lat)).log2()
}

/// [`center_placing_world_at`]'s solve without its tolerance: the centre
/// that draws `world` NEAREST `pixel`, and how many pixels off it is. A
/// centre the latitude clamp stopped short sits on the clamp with the miss
/// it could not close, which is what a drag pinned there works from.
/// `keep_size_within` is [`center_placing_world_at_keeping_size`]'s zoom
/// range; `None` holds the zoom fixed. The flag is whether any Newton step
/// aimed past the latitude clamp (the answer lies beyond the limit).
#[must_use]
pub(crate) fn center_placing_world_at_nearest(
    camera: &Camera,
    world: glam::DVec2,
    pixel: (f32, f32),
    keep_size_within: Option<(f64, f64)>,
) -> Option<((f64, f64), f64, bool)> {
    let max_lat = 85.051_128_779_806_59;
    let target = glam::DVec2::new(f64::from(pixel.0), f64::from(pixel.1));
    let drawn = |centre: glam::DVec2| {
        let zoom = keep_size_within.map_or(camera.zoom, |(lo, hi)| {
            (camera.zoom + globe_size_zoom_adjustment(camera.center_lat, centre.y)).clamp(lo, hi)
        });
        let cam = Camera { center_lon: centre.x, center_lat: centre.y, zoom, ..*camera };
        // The point in the world copy nearest this candidate centre. The
        // sphere is periodic in x, but inside the z11-z12 blend the flat
        // half of the projection is not: an anchor picked before the centre
        // crossed the antimeridian would otherwise sit a whole world away
        // from it, and nothing would converge.
        let size = f64::from(WORLD_SIZE);
        let x = world.x + ((cam.world_center_f64().x - world.x) / size).round() * size;
        project_world_to_screen_on_frame(&cam, &GlobeAnchors::new(&build_globe_frame(&cam)), (x, world.y))
            .map(|(x, y)| glam::DVec2::new(f64::from(x), f64::from(y)))
    };
    // A centre step one pixel wide, in degrees.
    let h = 360.0 / (f64::from(WORLD_SIZE) * 2f64.powf(camera.zoom));
    let wrap = |mut c: glam::DVec2| {
        c.y = c.y.clamp(-max_lat, max_lat);
        c.x = (c.x + 180.0).rem_euclid(360.0) - 180.0;
        c
    };
    // Start from the current centre if the point is drawn there at all;
    // else walk the centre toward the point (which is always drawn once it
    // IS the centre) until it comes round the limb -- a zoom-in far from
    // the centre puts it behind the horizon at the target zoom.
    let here = glam::DVec2::new(camera.center_lon, camera.center_lat);
    let (tlon, tlat) = crate::geo::tile_xy_to_lon_lat(
        world.x.rem_euclid(f64::from(WORLD_SIZE)) / f64::from(WORLD_SIZE),
        world.y / f64::from(WORLD_SIZE),
        0.0,
    );
    let towards = glam::DVec2::new((tlon - here.x + 540.0).rem_euclid(360.0) - 180.0, tlat - here.y);
    let mut centre = [0.0, 0.25, 0.5, 0.75, 1.0]
        .into_iter()
        .map(|t| wrap(here + towards * t))
        .find(|c| drawn(*c).is_some())?;
    let mut best: Option<(glam::DVec2, f64)> = None;
    // Whether a step ever aimed past the latitude clamp: the centre that
    // would draw the point there lies beyond the limit.
    let mut past_limit = false;
    for _ in 0..16 {
        // A centre the halvings left with the point hidden ends the run;
        // the best iterate so far still stands.
        let Some(s) = drawn(centre) else { break };
        let residual = target - s;
        let miss = residual.length();
        if best.is_none_or(|(_, b)| miss < b) {
            best = Some((centre, miss));
        }
        if miss < 0.01 {
            break;
        }
        // One-sided differences, taken backwards where forwards is hidden
        // -- or where `wrap`'s latitude clamp would cut the step short. A
        // centre parked on the clamp (where every drag to the pole leaves
        // it) otherwise measured a forward lat step of exactly zero, the
        // Jacobian went singular, and EVERY drag there fell off the exact
        // path, including those that move the centre away from the pole.
        let unclamped = |c: glam::DVec2| (c.y.abs() <= max_lat).then(|| wrap(c));
        let slope = |d: glam::DVec2| {
            let forward = unclamped(centre + d).and_then(drawn).map(|p| (p - s) / h);
            forward.or_else(|| unclamped(centre - d).and_then(drawn).map(|p| (s - p) / h))
        };
        let (Some(jx), Some(jy)) = (slope(glam::DVec2::new(h, 0.0)), slope(glam::DVec2::new(0.0, h))) else { break };
        let jacobian = glam::DMat2::from_cols(jx, jy);
        if jacobian.determinant().abs() < f64::EPSILON {
            break;
        }
        let mut step = jacobian.inverse() * residual;
        // Never more than a quarter turn a step, and halved until the
        // point is still drawn: a full Newton step off the limb can
        // otherwise throw the centre round the planet.
        let len = step.length();
        if len > 90.0 {
            step *= 90.0 / len;
        }
        let mut halvings = 0;
        while drawn(wrap(centre + step)).is_none() && halvings < 12 {
            step *= 0.5;
            halvings += 1;
        }
        // A step aiming past the latitude clamp stops ON it, its longitude
        // re-solved for that (least squares on the longitude column): the
        // clamped step `wrap` would take otherwise keeps a longitude meant
        // for a latitude it never reaches, lands farther off, and the run
        // stalls on its start -- a drag toward the pole then never finds
        // the limit it should pin on.
        if (centre + step).y.abs() > max_lat && jx.length_squared() > 0.0 {
            past_limit = true;
            step.y = max_lat.copysign(centre.y + step.y) - centre.y;
            step.x = jx.dot(residual - jy * step.y) / jx.length_squared();
        }
        centre = wrap(centre + step);
    }
    best.map(|(c, miss)| ((c.x, c.y), miss, past_limit))
}

/// Just under a half turn: a drag can never put the centre exactly on the
/// far side of the planet. MapLibre's `PAN_MAX_ANGLE` (`globe_utils.ts`).
const PAN_MAX_ANGLE: f64 = PI * 0.98;

/// MapLibre's `PAN_FALLOFF_BAND`: exact tracking stops this fraction of
/// the horizon angle before the silhouette at the latest.
const PAN_FALLOFF_BAND: f64 = 0.1;

/// **The point on the unit sphere a globe drag through `pixel` aims at**,
/// and whether it is the point actually drawn there -- MapLibre's
/// `panSurfaceLocation` (`globe_utils.ts`), with the handover moved in to
/// honour this crate's own grab rule.
///
/// Exact tracking breaks down at the silhouette: a ray at angle `a` from
/// the nadir meets the sphere at central angle `asin(D sin a) - a` (`D` the
/// eye's distance in radii), whose slope runs away to infinity as the ray
/// goes tangent, and past it there is no intersection. A pitched camera low
/// over the planet (z10.5, pitch 60) puts that silhouette 70 px below the
/// top of the window, and a 25 px drag grabbed a few pixels under it moved
/// the centre 2,500-4,300 px. So the exact curve is left at the EARLIER of
/// MapLibre's band (`PAN_FALLOFF_BAND` of the horizon angle) and the ray
/// angle where the ground per pixel reaches `max_scale` times the screen
/// centre's -- `camera_control::GRAB_MAX_PITCH_SCALE`, the same limit the
/// flat grab applies -- and continued with MapLibre's hyperbola, matching it
/// in value and slope and saturating at `PAN_MAX_ANGLE`. Beyond the handover
/// the rate only falls, so no drag anywhere moves the ground faster than
/// `max_scale` times the centre's rate. Rays above the horizon get a point
/// too (the hyperbola saturates rather than rejecting them).
///
/// `None` only for a degenerate camera. The point is in the unit-sphere
/// space [`mercator_to_sphere`] maps into.
#[must_use]
pub fn pan_surface_point(camera: &Camera, max_scale: f64, pixel: (f32, f32)) -> Option<(glam::DVec3, bool)> {
    let frame = build_globe_frame(camera);
    let eye = frame.globe_view.inverse() * glam::DVec4::new(0.0, 0.0, 0.0, 1.0);
    if eye.w == 0.0 {
        return None;
    }
    let eye = eye.truncate() / eye.w;
    let distance = eye.length();
    let inverse = frame.globe_view_proj.inverse();
    let ray = |p: (f32, f32)| {
        let ndc_x = f64::from(p.0 / camera.viewport_width_px) * 2.0 - 1.0;
        let ndc_y = 1.0 - f64::from(p.1 / camera.viewport_height_px) * 2.0;
        let near = inverse * glam::DVec4::new(ndc_x, ndc_y, 0.0, 1.0);
        let far = inverse * glam::DVec4::new(ndc_x, ndc_y, 1.0, 1.0);
        (far.truncate() / far.w - near.truncate() / near.w).normalize()
    };
    let direction = ray(pixel);
    if !direction.is_finite() || !(distance > 1.0) {
        return None;
    }
    let up = eye / distance;
    // The ray's angle from the nadir, and the lateral unit vector it leans along.
    let angle_of = |d: glam::DVec3| {
        let c = -d.dot(up);
        let lateral = d + up * c;
        (lateral.length().atan2(c), lateral)
    };
    let (angle, lateral) = angle_of(direction);
    let s = lateral.length();
    if s < 1e-9 {
        return Some((up, true));
    }
    let side = lateral / s;
    let central = |a: f64| (distance * a.sin()).clamp(-1.0, 1.0).asin() - a;
    let slope = |a: f64| {
        let k = distance * a.sin();
        distance * a.cos() / (1.0 - k * k).max(1e-12).sqrt() - 1.0
    };
    let horizon = (1.0 / distance).asin();
    let mut handover = horizon * (1.0 - PAN_FALLOFF_BAND);
    let (centre_angle, _) = angle_of(ray((camera.viewport_width_px / 2.0, camera.viewport_height_px / 2.0)));
    let centre_angle = centre_angle.min(handover);
    let limit = max_scale * slope(centre_angle);
    if slope(handover) > limit {
        let (mut lo, mut hi) = (centre_angle, handover);
        for _ in 0..48 {
            let mid = 0.5 * (lo + hi);
            if slope(mid) > limit {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        handover = lo;
    }
    let (target, exact) = if angle < handover {
        (central(angle), true)
    } else {
        let at_handover = central(handover);
        let k = slope(handover);
        let room = PAN_MAX_ANGLE - at_handover;
        let excess = angle - handover;
        (at_handover + room * (k * excess) / (room + k * excess), false)
    };
    let target = target.clamp(0.0, PAN_MAX_ANGLE);
    Some(((up * target.cos() + side * target.sin()).normalize(), exact))
}

/// **Where the pole is drawn** (`north` or south), through the pure sphere
/// matrix: MapLibre's `locationToScreenPoint(LngLat(0, ±90))`, which
/// Mercator coordinates cannot express (they stop at +/-85.05). `None` when
/// the pole is behind the eye (clip `w` not positive).
#[must_use]
pub(crate) fn pole_screen_point(camera: &Camera, north: bool) -> Option<(f64, f64)> {
    let frame = build_globe_frame(camera);
    let clip = frame.globe_view_proj * glam::DVec4::new(0.0, if north { 1.0 } else { -1.0 }, 0.0, 1.0);
    if !(clip.w > 0.0) {
        return None;
    }
    let (ndc_x, ndc_y) = (clip.x / clip.w, clip.y / clip.w);
    let p = (
        (ndc_x + 1.0) * 0.5 * f64::from(camera.viewport_width_px),
        (1.0 - ndc_y) * 0.5 * f64::from(camera.viewport_height_px),
    );
    (p.0.is_finite() && p.1.is_finite()).then_some(p)
}

/// The centre after turning the planet so the place at unit-sphere point
/// `grabbed` comes to where `to` is now, bearing kept: MapLibre's
/// `versorSetLocationAtPoint` with `fixedBearing`. For the drags
/// [`pan_surface_point`] does not track exactly.
#[must_use]
pub fn center_after_turning(camera: &Camera, grabbed: glam::DVec3, to: glam::DVec3) -> Option<(f64, f64)> {
    let size = f64::from(WORLD_SIZE);
    let centre = camera.world_center_f64();
    let sphere = mercator_to_sphere(centre.x / size, centre.y / size);
    // The planet turns `grabbed` onto `to`; the eye stays, so the point it
    // looks at turns the other way.
    let turned = glam::DQuat::from_rotation_arc(to, grabbed) * sphere;
    if !turned.is_finite() {
        return None;
    }
    let (x, y) = sphere_to_mercator(turned);
    let (lon, lat) = crate::geo::tile_xy_to_lon_lat(x, y, 0.0);
    Some((lon, lat))
}

/// The pure-globe half of [`unproject_to_world_on_globe`]: the ray through
/// the pixel, in unit-sphere space, against the unit sphere.
fn sphere_world_under(frame: &GlobeFrame, camera: &Camera, screen_x: f32, screen_y: f32) -> Option<glam::DVec2> {
    // No horizontal flip here, unlike `unproject_to_ground_f64`: the
    // shader blends the globe clip position AFTER it flips the flat one,
    // so the globe matrix already produces on-screen x
    // (`project_world_to_screen_on_frame` does the same dance forwards).
    let ndc_x = f64::from(screen_x / camera.viewport_width_px) * 2.0 - 1.0;
    let ndc_y = 1.0 - f64::from(screen_y / camera.viewport_height_px) * 2.0;
    let inverse = frame.globe_view_proj.inverse();
    let near = inverse * glam::DVec4::new(ndc_x, ndc_y, 0.0, 1.0);
    let far = inverse * glam::DVec4::new(ndc_x, ndc_y, 1.0, 1.0);
    if near.w == 0.0 || far.w == 0.0 {
        return None;
    }
    let origin = near.truncate() / near.w;
    let dir = (far.truncate() / far.w - origin).normalize();
    // |origin + t dir| = 1, the nearer root: the side of the planet facing
    // the eye, which is the side the clipping plane keeps.
    let b = origin.dot(dir);
    let c = origin.length_squared() - 1.0;
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    let t = -b - disc.sqrt();
    if t < 0.0 {
        return None;
    }
    let (mx, my) = sphere_to_mercator(origin + dir * t);
    let world = f64::from(WORLD_SIZE);
    Some(glam::DVec2::new(mx * world, my * world))
}

/// The screen ray under logical `(screen_x, screen_y)` of `frame`'s PURE
/// globe projection, in unit-sphere space: `(origin, unit direction)`,
/// the origin on the near plane. What `sphere_world_under` intersects
/// with the ground, handed out for a pick that marches above it (an
/// overlay extrusion's columns, `overlay::extrude::pick_on_globe`).
#[must_use]
pub fn sphere_ray(frame: &GlobeFrame, camera: &Camera, screen_x: f32, screen_y: f32) -> Option<(glam::DVec3, glam::DVec3)> {
    let ndc_x = f64::from(screen_x / camera.viewport_width_px) * 2.0 - 1.0;
    let ndc_y = 1.0 - f64::from(screen_y / camera.viewport_height_px) * 2.0;
    let inverse = frame.globe_view_proj.inverse();
    let near = inverse * glam::DVec4::new(ndc_x, ndc_y, 0.0, 1.0);
    let far = inverse * glam::DVec4::new(ndc_x, ndc_y, 1.0, 1.0);
    if near.w == 0.0 || far.w == 0.0 {
        return None;
    }
    let origin = near.truncate() / near.w;
    let dir = (far.truncate() / far.w - origin).try_normalize()?;
    Some((origin, dir))
}

/// [`sphere_to_mercator`], public: a unit-sphere point (any radius: only
/// its direction is read) to `[0, 1]` Mercator.
#[must_use]
pub fn sphere_point_to_mercator(p: glam::DVec3) -> (f64, f64) {
    if let Some(projection) = crate::projection::active() {
        // Outside the outline there is no answer: clamp toward the centre
        // the way MapLibre's `lngLatFromWorld` does (bisect to the edge).
        let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
        if projection.inverse(p.x, p.y).is_none() {
            for _ in 0..30 {
                let mid = (lo + hi) / 2.0;
                if projection.inverse(p.x * mid, p.y * mid).is_some() { lo = mid } else { hi = mid }
            }
        } else {
            lo = 1.0;
        }
        let (lon, lat) = projection.inverse(p.x * lo, p.y * lo).unwrap_or((0.0, 0.0));
        return crate::projection::lon_lat_to_mercator(lon, lat);
    }
    sphere_to_mercator(p / p.length().max(1e-300))
}

/// The inverse of [`mercator_to_sphere`]: a unit-sphere point to `[0, 1]`
/// Mercator, x wrapped into `[0, 1)`, latitude clamped to the Mercator limit.
fn sphere_to_mercator(p: glam::DVec3) -> (f64, f64) {
    let spherical_x = p.x.atan2(p.z);
    let x = ((spherical_x - PI) / (2.0 * PI)).rem_euclid(1.0);
    let lat = p.y.clamp(-1.0, 1.0).asin().clamp(-85.051_129f64.to_radians(), 85.051_129f64.to_radians());
    let y = (1.0 - (PI / 4.0 + lat / 2.0).tan().ln() / PI) / 2.0;
    (x, y)
}

/// Newton's method on `project_world_to_screen_on_frame`: the world point
/// that the frame draws at `target` (logical pixels), from `start`, with
/// how far (pixels) the best iterate still lands from `target`. A Jacobian
/// by finite differences one pixel wide; converges in two or three steps
/// from either exact end of the blend. The BEST iterate is returned, not
/// the last, and the caller judges its residual: an unconverged run (at
/// the limb) must not pass for a pick.
fn refine_on_frame(
    camera: &Camera,
    anchors: &GlobeAnchors,
    start: glam::DVec2,
    target: glam::DVec2,
) -> Option<(glam::DVec2, f64)> {
    let at = |w: glam::DVec2| {
        project_world_to_screen_on_frame(camera, anchors, (w.x, w.y))
            .map(|(x, y)| glam::DVec2::new(f64::from(x), f64::from(y)))
    };
    let h = 2f64.powf(-camera.zoom) * f64::from(WORLD_SIZE) / 512.0;
    let mut w = start;
    let mut best: Option<(glam::DVec2, f64)> = None;
    for _ in 0..8 {
        let Some(s) = at(w) else { break };
        let residual = target - s;
        let miss = residual.length();
        if best.is_none_or(|(_, b)| miss < b) {
            best = Some((w, miss));
        }
        if miss < 0.01 {
            break;
        }
        let (Some(sx), Some(sy)) = (at(w + glam::DVec2::new(h, 0.0)), at(w + glam::DVec2::new(0.0, h))) else {
            break;
        };
        let jacobian = glam::DMat2::from_cols((sx - s) / h, (sy - s) / h);
        if jacobian.determinant().abs() < f64::EPSILON {
            break;
        }
        w += jacobian.inverse() * residual;
    }
    best
}

fn clipping_plane(camera: &Camera, distance_camera_to_b: f64) -> [f64; 4] {
    let pitch = camera.pitch as f64;
    let radius = 1.0;

    let distance_camera_to_a = pitch.sin() * distance_camera_to_b;
    let distance_a_to_c = pitch.cos() * distance_camera_to_b + radius;
    let distance_camera_to_c =
        (distance_camera_to_a * distance_camera_to_a + distance_a_to_c * distance_a_to_c).sqrt();
    // Cosine of the angle cam-C-T, at C.
    let cam_ct_cosine = radius / distance_camera_to_c;
    // How far the plane of all possible tangent points sits from the centre.
    let tangent_plane_distance_to_c = cam_ct_cosine * radius;

    let mut vector_c_to_cam_x = -distance_camera_to_a;
    let mut vector_c_to_cam_y = distance_a_to_c;
    let vector_c_to_cam_length =
        (vector_c_to_cam_x * vector_c_to_cam_x + vector_c_to_cam_y * vector_c_to_cam_y).sqrt();
    vector_c_to_cam_x /= vector_c_to_cam_length;
    vector_c_to_cam_y /= vector_c_to_cam_length;

    // Note the swizzled components -- the 2D (x, y) frame the tangent maths
    // was done in maps onto the sphere's (y, z), because the sphere's own
    // +y axis is north.
    let plane = glam::DVec3::new(0.0, vector_c_to_cam_x, vector_c_to_cam_y);
    // Bearing, then latitude, then longitude. Pitch is deliberately NOT
    // applied: it is already folded into the tangent maths above, since it is
    // what decides where the tangent plane sits.
    let plane = glam::DMat3::from_rotation_z(-(camera.bearing as f64)) * plane;
    let plane = glam::DMat3::from_rotation_x(-camera.center_lat.to_radians()) * plane;
    let plane = glam::DMat3::from_rotation_y(camera.center_lon.to_radians()) * plane;

    let scale = 1.0 / plane.length();
    let plane = plane * scale;
    [
        plane.x,
        plane.y,
        plane.z,
        -tangent_plane_distance_to_c * scale,
    ]
}

/// The tile's origin and span in `[0, 1]` Mercator space, as
/// `(x, y, width, height)` — **and deliberately the only per-tile work the
/// globe does.** Four divisions, no allocation, no matrix. See the module docs.
///
/// The shader recovers a vertex's Mercator position as
/// `xy + zw * pos_in_tile`, where `pos_in_tile` is in `[0, 1]`
/// (`_projection_globe.vertex.glsl:56`).
///
/// **One deliberate difference from MapLibre.** Their
/// `getMercatorTileCoordinates` (`transform_helper.ts:692-703`) returns
/// `1 / scale / EXTENT` for the span, folding the 1/8192 tile-extent divide
/// into the uniform because their shader multiplies by raw `0..8192` tile
/// coordinates. Ours is the tile's true span in Mercator space, `1 / 2^z`,
/// because this crate's vertex positions are not in MVT extent units — they
/// are in pre-scaled lyon units (`tile_model_matrix` divides by
/// `1000.0 * extent`). Whatever consumes this must normalise its in-tile
/// coordinate to `[0, 1]` itself; the extent is not baked in here, and a
/// consumer that assumes it is will place every tile at the origin.
pub fn tile_mercator_coords(tile: TileId) -> [f32; 4] {
    let span = 1.0 / f64::from(1u32 << tile.z);
    [
        (f64::from(tile.x) * span) as f32,
        (f64::from(tile.y) * span) as f32,
        span as f32,
        span as f32,
    ]
}

/// `(sin(lat), cos(lat))` for a Mercator `y` in `[0, 1]`, by the same
/// `exp`-and-rational route the shader takes — **not** by the direct formula.
///
/// The conversion is the inverse Gudermannian,
/// `lat = 2*atan(exp(PI - y*2*PI)) - PI/2`.
/// `_projection_globe.vertex.glsl:61-75` refuses to evaluate it that way, and
/// gives two reasons:
///
/// 1. **Catastrophic cancellation at the equator.** At the equator
///    `exp(PI - y*2*PI)` is 1 and `atan(1)` is `PI/4`, so `2*atan(...)` is
///    `PI/2` and the formula subtracts `PI/2` from a value near `PI/2`. In
///    float32 that leaves a latitude built from only the handful of mantissa
///    bits that survived the subtraction — precision is destroyed exactly
///    where most of the world's tiles are.
/// 2. **Some GPUs (e.g. Mali) have imprecise `atan`/`sin`/`cos`** regardless,
///    so even without the cancellation the transcendentals are not trustworthy.
///
/// The fix is algebraic, not numerical. Substituting `t = exp(PI - y*2*PI)`
/// and expanding with the tangent half-angle (Weierstrass) identities
/// eliminates `atan`, `sin` and `cos` entirely, leaving `exp` and rational
/// arithmetic:
///
/// ```text
///   sin(2*atan(t) - PI/2) = (t^2 - 1) / (t^2 + 1)
///   cos(2*atan(t) - PI/2) =    2*t    / (t^2 + 1)
/// ```
///
/// (Derivation: with `u = atan(t)`, `sin(2u - PI/2) = -cos(2u)` and
/// `cos(2u - PI/2) = sin(2u)`; the double-angle forms in `tan(u) = t` are
/// `cos(2u) = (1 - t^2)/(1 + t^2)` and `sin(2u) = 2t/(1 + t^2)`.)
///
/// Note there is no subtraction of two nearly-equal *large* quantities left:
/// at the equator `t = 1`, so the numerator `t^2 - 1` is an exact zero and the
/// result is exactly `(0, 1)`. That is the whole point.
///
/// This CPU copy exists so the identity can be tested against the direct
/// formula in f64 — see
/// `the_half_angle_substitution_matches_the_direct_latitude_formula`. If the
/// algebra were transcribed wrong the shader would still compile and still
/// draw a sphere, just a subtly misshapen one, and no rendering test would
/// localise it.
///
/// Inputs outside `[0, 1]` (wrapped or overscaled tiles) are fine in f64:
/// `t^2` only overflows past roughly `y < -55`. In **float32** the same
/// expression overflows `t^2` around `y < -6.5`, which is still far outside
/// any tile the map can display, but is worth knowing before anyone reuses
/// this for something unbounded.
pub fn mercator_y_to_sin_cos_lat(y: f64) -> (f64, f64) {
    let t = (PI - y * 2.0 * PI).exp();
    let t2 = t * t;
    let denom = t2 + 1.0;
    ((t2 - 1.0) / denom, (2.0 * t) / denom)
}

/// A point in `[0, 1]` Mercator space, on the unit sphere.
///
/// The CPU twin of `shader.wgsl`'s `project_to_sphere`, and the same thing
/// MapLibre's `projectTileCoordinatesToSphere` (`globe_utils.ts:53-60`) and
/// `angularCoordinatesRadiansToVector` (`:36-42`) compute: `spherical_x` is
/// `mercator_x * 2*PI + PI`, so Mercator x = 0.5 (lon 0) lands on `+z`,
/// `+x` is lon +90 (east) and `+y` is the north pole. Latitude comes through
/// [`mercator_y_to_sin_cos_lat`], so this agrees with the shader's sphere by
/// construction rather than by a second derivation.
///
/// The poles are NOT reachable through this function: Mercator y `0` and `1`
/// are latitude ±85.051129°, so a sphere assembled from tile RECTANGLES
/// alone has a hole at each pole. What closes it is
/// `render::subdivide::append_pole_caps`, MapLibre's `_generatePoleQuad`
/// (`subdivision.ts:491`): it grows a quad from every fill edge on a
/// pole-row tile's boundary, marks the new vertices in `Vertex::is_stroke`,
/// and `shader.wgsl`'s `vs_main` replaces their sphere position with
/// `(0, ±1, 0)` directly rather than routing it through here. That is also
/// why [`GlobeCover::max_dot_over_tile`] extends a pole-row tile's latitude
/// interval to the pole — the cap lies outside the tile's own rectangle.
#[must_use]
pub fn mercator_to_sphere(mercator_x: f64, mercator_y: f64) -> glam::DVec3 {
    if let Some(projection) = crate::projection::active() {
        let (lon, lat) = crate::projection::mercator_to_lon_lat(mercator_x, mercator_y);
        let (x, y) = projection.forward(lon, lat);
        return glam::DVec3::new(x, y, 0.0);
    }
    let spherical_x = mercator_x * 2.0 * PI + PI;
    let (sin_lat, cos_lat) = mercator_y_to_sin_cos_lat(mercator_y);
    glam::DVec3::new(spherical_x.sin() * cos_lat, sin_lat, spherical_x.cos() * cos_lat)
}

/// **Which tiles the sphere actually shows**, and the far-side cull — built
/// once per frame from a [`GlobeFrame`], then asked one tile at a time.
///
/// # The defect this type closes
///
/// Tile selection used to be `geo::visible_tiles` over
/// `render::pitch_aware_tile_bbox` at every zoom, and that bbox is a lon/lat
/// rectangle around the flat viewport's ground footprint. At
/// `lon -120, lat 25, zoom 2.5` it selected twelve tiles and
/// `tile_in_frustum` trimmed them to **exactly four** — `z2 x0 y1`,
/// `z2 x0 y2`, `z2 x1 y1`, `z2 x1 y2`, a 2x2 patch of a 4x4 world, which is
/// what `GET /map/tiles` reported on the running example. A flat map at that
/// camera genuinely shows only that much; a **sphere** at the same camera
/// shows a cap of the planet **128.6 degrees of arc across**, which needs
/// nine of those tiles. So five of the nine were never fetched and rendered
/// as background, and the "globe" on screen was the union of four projected
/// tile quads — which is why its outline was a polygon. It literally was one.
///
/// # Where this follows MapLibre and where it does not
///
/// **Follows: the hard switch.** `GlobeTransform.currentTransform` is the
/// vertical-perspective transform whenever `isGlobeRendering`, i.e. whenever
/// `_globeness > 0` (`globe_transform.ts:230-232,241-243`), and Mercator's own
/// `getClippingPlane()` returns `null` (`mercator_transform.ts:326-328`). So
/// the reference never blends two covers: below the threshold the flat path
/// runs untouched, above it the globe path runs alone. [`Self::FLAT`] is that
/// switch expressed as a value, and [`Self::new`] returns it — unconditionally
/// and before touching a matrix — for any frame whose `transition` is `0.0`.
/// That is what makes the flat path *bit-identical*, not merely
/// *intended to be unchanged*.
///
/// **Follows: one root tile, one level, depth-first.** See [`Self::cover`].
///
/// **Diverges: an exact plane test and a bounding ball, instead of a
/// six-plane convex volume.**
/// `_computeTileBoundingVolume`
/// (`globe_covering_tiles_details_provider.ts:112-297`) builds, per tile, a
/// six-plane `ConvexVolume` with deliberately non-parallel east/west planes
/// ("These axes are NOT opposites of each other, they differ!", `:200-201`),
/// eight `threePlaneIntersection` corner solves, a best-fit AABB beside it,
/// and a double-buffered `BoundingVolumeCache` (`:46,51-53`) to stop it being
/// rebuilt every frame. We do two cheaper things instead, and they are
/// cheaper in different ways:
///
/// - For the **horizon** — the test that actually matters, because it is what
///   removes the far side — we do not bound the tile at all. The maximum of
///   `dot(n, p)` over a tile's lon/lat rectangle has a closed form (see
///   [`Self::max_dot_over_tile`]), so the plane test is **exact**, tighter
///   than any convex volume, and costs one `cos` and a handful of multiplies.
///   Tightness is not a luxury here: with a bounding cap instead, the z2 tile
///   spanning lon -180..-90 at a lon-0 camera tests as *maybe visible* when
///   every point of it is behind the horizon, and those are precisely the
///   tiles that put geometry past the silhouette.
/// - For the **frustum** we do bound the tile, by the smallest ball
///   containing its spherical cap. It is loose at z1-z2 (where the cap is a
///   quarter of the sphere and the ball is the whole sphere) and tight from
///   z3 up, and loose is safe: a frustum false positive costs one fetched
///   tile, and at z1-z2 there are only 4 and 16 tiles in the entire world.
///
/// **Diverges: no variable zoom, because we have none to disable.**
/// `allowVariableZoom` is `coveringZoomLevel > 4`
/// (`globe_covering_tiles_details_provider.ts:100-102`), so below z5 the
/// reference takes the whole visible cap at ONE level. Ours is single-level at
/// every zoom — `tiles_for_zoom` takes a level and returns tiles of it — so
/// there is nothing to switch off, and the globe band is capped at z5 anyway
/// by [`transition_for_zoom`] reaching `0.0` at [`GLOBE_ZOOM_MAX`] = 6.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobeCover {
    /// `sin` of the horizon plane normal's own latitude, i.e. the normal's
    /// `y` — the sphere's `+y` IS north, so no `asin` is needed.
    plane_sin_lat: f64,
    /// `cos` of the same latitude, `sqrt(1 - y^2)`, non-negative because
    /// latitude is in `[-90, 90]`.
    plane_cos_lat: f64,
    /// The plane normal's own `spherical_x`, the longitude-like angle
    /// [`mercator_to_sphere`] works in. `atan2(n.x, n.z)` inverts that
    /// function's `(sin(sx)*cos_lat, sin_lat, cos(sx)*cos_lat)`.
    plane_spherical_x: f64,
    /// The horizon plane's offset — [`GlobeFrame::clipping_plane`]'s `w`.
    plane_w: f64,
    /// Five half-spaces of the globe frustum, each `xyz` normal and `w`
    /// offset in **unit-sphere space**, with "inside" being `<= 0`. Extracted
    /// from [`GlobeFrame::globe_view_proj`]'s rows: `x <= w` is
    /// `row0 - row3`, `-x <= w` is `-row0 - row3`, the same for `y` with
    /// `row1`, and `z >= 0` is `-row2`.
    ///
    /// **Normalised to a unit `xyz`** when they are built, once per frame, so
    /// the per-tile sphere test is `dot(n, centre) + w > radius` with no
    /// `sqrt` in it. Five square roots per tile over a 1,365-node descent is
    /// work that does not depend on the tile at all.
    ///
    /// **There is no far plane, and that is not an omission.**
    /// `globe_frame` sets `far = d + radius*2`, and the camera sits `d + r`
    /// from the sphere's centre, so the most distant point on the sphere is
    /// at exactly `d + 2r` — a far plane there can reject nothing. The far
    /// side is `plane_w`'s job, and it cuts at the silhouette rather than at
    /// the antipode.
    planes: [glam::DVec4; 5],
    /// The frame's globeness. `0.0` means this is [`Self::FLAT`].
    transition: f32,
}

/// The globe cover of `camera`'s frame, for a test outside this module
/// (`map_view.rs`'s test module may not build a `GlobeFrame` itself --
/// see `paint_calls_globe_frame_exactly_once_and_before_its_tile_loops`).
/// One camera-level build per call, as `cover_progress` does.
#[cfg(test)]
pub(crate) fn test_cover_of(camera: &Camera) -> GlobeCover {
    GlobeCover::new(&globe_frame(camera))
}

impl GlobeCover {
    /// The cover for a frame with **no globe**: `transition` zero, and every
    /// other field zero because nothing reads them.
    ///
    /// Same idiom and same reason as `super::GlobeParams::FLAT` — it names
    /// the "no globe" case once, so the callers that must behave exactly as
    /// they did before the globe existed say so in one word.
    pub const FLAT: Self = Self {
        plane_sin_lat: 0.0,
        plane_cos_lat: 0.0,
        plane_spherical_x: 0.0,
        plane_w: 0.0,
        planes: [glam::DVec4::ZERO; 5],
        transition: 0.0,
    };

    /// Narrows one frame's [`GlobeFrame`] into the form tile selection wants:
    /// the horizon plane as angles rather than a vector, and the globe
    /// matrix as five half-spaces.
    ///
    /// Returns [`Self::FLAT`] for a frame with `transition == 0.0`, before
    /// reading a single matrix row. See the type's doc comment — this is
    /// MapLibre's `isGlobeRendering` switch (`globe_transform.ts:230-232`),
    /// and it is what keeps a frame above the transition band paying nothing
    /// and selecting exactly the tiles it selected before.
    #[must_use]
    pub fn new(frame: &GlobeFrame) -> Self {
        if frame.transition <= 0.0 {
            return Self::FLAT;
        }
        let [nx, ny, nz, nw] = frame.clipping_plane;
        let m = frame.globe_view_proj;
        let (r0, r1, r2, r3) = (m.row(0), m.row(1), m.row(2), m.row(3));
        // Unit `xyz` per plane -- see the field's doc comment. A row
        // combination with a zero `xyz` cannot separate anything, so it is
        // left alone rather than divided by zero; its test then reduces to
        // `w > radius`, which for a plane with no direction is as close to
        // "this tells you nothing" as a half-space gets.
        let normalise = |p: glam::DVec4| {
            let len = p.truncate().length();
            if len > 0.0 { p / len } else { p }
        };
        Self {
            plane_sin_lat: ny.clamp(-1.0, 1.0),
            plane_cos_lat: (1.0 - ny * ny).max(0.0).sqrt(),
            plane_spherical_x: nx.atan2(nz),
            plane_w: nw,
            planes: [r0 - r3, -r0 - r3, r1 - r3, -r1 - r3, -r2].map(normalise),
            transition: frame.transition,
        }
    }

    /// Whether the globe is being drawn at all this frame — MapLibre's
    /// `isGlobeRendering`, `_globeness > 0` (`globe_transform.ts:230-232`).
    /// `false` means the caller must take its pre-globe path unchanged.
    #[must_use]
    pub fn is_globe(&self) -> bool {
        self.transition > 0.0
    }

    /// The frame's globeness, `0.0` on [`Self::FLAT`]: what
    /// `render::dots::relocation_camera` gates relocation on.
    #[must_use]
    pub fn transition(&self) -> f32 {
        self.transition
    }

    /// What this frame shows, point by point: a unit-sphere point `p` is
    /// on screen when `dot(h.xyz, p) + h.w <= 0` for all six -- the five
    /// frustum half-spaces [`Self::tile_visible`] tests balls against, and
    /// the horizon (`dot(n, p) + plane_w >= 0`, negated). For
    /// `overlay::dotgrid::in_view`, which counts the cells the view shows.
    #[must_use]
    pub fn visible_half_spaces(&self) -> [glam::DVec4; 6] {
        let n = glam::DVec3::new(
            self.plane_spherical_x.sin() * self.plane_cos_lat,
            self.plane_sin_lat,
            self.plane_spherical_x.cos() * self.plane_cos_lat,
        );
        let [a, b, c, d, e] = self.planes;
        [a, b, c, d, e, (-n).extend(-self.plane_w)]
    }

    /// The largest `dot(n, p)` over every point `p` of `tile`, where `n` is
    /// the horizon plane's normal — so a result with `+ plane_w < 0` means
    /// the tile is **entirely** on the hemisphere facing away.
    ///
    /// Exact, in closed form, and that is worth the algebra (see
    /// [`GlobeCover`]'s note on tightness). A tile is a rectangle in
    /// `(spherical_x, latitude)`, and on it
    ///
    /// ```text
    ///   dot(n, p) = sin(lat)*sin(lat_n) + cos(lat)*cos(lat_n)*cos(dsx)
    /// ```
    ///
    /// where `dsx` is the difference in `spherical_x`. Two facts collapse the
    /// search:
    ///
    /// 1. `cos(dsx)` is decreasing in `|dsx|` over `[0, PI]` and
    ///    `cos(lat)*cos(lat_n) >= 0` always, so the maximum takes the
    ///    **smallest** `|dsx|` the tile's x range allows — zero when the
    ///    plane's own longitude is inside it, and the nearer edge otherwise.
    /// 2. With `dsx` fixed the expression is `A*sin(lat) + B*cos(lat)`, i.e.
    ///    `R*cos(lat - phi)`, whose single maximum over an interval shorter
    ///    than `2*PI` is either at an end or at `phi` itself. Which one is
    ///    decided by the sign of the derivative `A*cos(lat) - B*sin(lat)` at
    ///    the two ends, so there is **no `atan2` per tile** — and no `asin`
    ///    either, because [`mercator_y_to_sin_cos_lat`] hands over the
    ///    latitude edges already as sines and cosines.
    ///
    /// z0 short-circuits to `1.0`: that tile is the whole sphere, which
    /// always contains the near pole. MapLibre special-cases the same level
    /// for the same reason (`:127-132`, "Tile covers the entire sphere").
    ///
    /// **A tile bordering the Mercator north or south edge IS extended to
    /// the pole** — `:165-170`, restored here now that something emits pole
    /// quads. `render::subdivide::append_pole_caps` grows a cap from every
    /// fill edge on such a tile's boundary, and that cap covers latitudes
    /// from the tile's own edge (+/-85.051) to +/-90, which is OUTSIDE the
    /// tile's lon/lat rectangle. Without the extension a cap would be culled
    /// with its tile whenever the tile's own rectangle was behind the
    /// horizon but the pole was not — which is exactly the camera looking
    /// straight down at a pole.
    ///
    /// The extension only ever LOOSENS the test (the latitude interval grows
    /// towards the pole, so the maximum cannot fall), and it applies to the
    /// north and south tile rows only, so no other tile's answer moves. And
    /// it preserves the parent/child monotonicity
    /// `the_descent_finds_exactly_what_a_full_level_scan_would` relies on:
    /// a tile is in the north row iff its parent is (`y == 0` iff
    /// `parent.y == 0`), and the same for the south row.
    fn max_dot_over_tile(&self, tile: TileId) -> f64 {
        if tile.z == 0 {
            return 1.0;
        }
        let span = 1.0 / f64::from(1u32 << tile.z);
        let tau = std::f64::consts::TAU;

        // Smallest |dsx| the tile's x range allows. Computed from the centre
        // and half-width rather than the two edges, because that makes the
        // antimeridian wrap one `rem_euclid` instead of a case analysis --
        // and z0/z1 tiles, whose x range is half the sphere or more, fall out
        // of the same expression.
        let sx_centre = (f64::from(tile.x) + 0.5) * span * tau + PI;
        let sx_half = 0.5 * span * tau;
        let mut delta = (self.plane_spherical_x - sx_centre).rem_euclid(tau);
        if delta > PI {
            delta -= tau;
        }
        let d_sx = (delta.abs() - sx_half).max(0.0);

        let a = self.plane_sin_lat;
        let b = self.plane_cos_lat * d_sx.cos();
        // Mercator y grows southward, so `tile.y` is the tile's NORTH edge.
        // A pole-row tile's edge is the pole itself, `(sin, cos) = (+/-1, 0)`
        // -- the pole-quad extension; see this function's doc comment.
        let (sin_n, cos_n) = if tile.y == 0 {
            (1.0, 0.0)
        } else {
            mercator_y_to_sin_cos_lat(f64::from(tile.y) * span)
        };
        let (sin_s, cos_s) = if tile.y == (1u32 << tile.z) - 1 {
            (-1.0, 0.0)
        } else {
            mercator_y_to_sin_cos_lat(f64::from(tile.y + 1) * span)
        };
        // The derivative at each latitude edge. Rising at the south edge and
        // falling at the north edge brackets the interior maximum; otherwise
        // the function is monotone across the tile and an edge wins.
        if a * cos_s - b * sin_s >= 0.0 && a * cos_n - b * sin_n <= 0.0 {
            (a * a + b * b).sqrt()
        } else {
            (a * sin_n + b * cos_n).max(a * sin_s + b * cos_s)
        }
    }

    /// Is any part of `tile` drawn on this frame's globe?
    ///
    /// Two rejections, in the order they are cheapest and most selective:
    /// entirely behind the horizon plane ([`Self::max_dot_over_tile`]), then
    /// entirely outside one of the frustum's half-spaces. That pairing is
    /// `isTileVisible`'s (`covering_tiles.ts:82-98`): the frustum and the
    /// clipping plane, with `None` from either meaning not visible.
    ///
    /// The frustum half tests the smallest ball containing the tile's
    /// spherical cap — centre `c*cos(theta)`, radius `sin(theta)`, where `c`
    /// is the tile's centre direction and `theta` the angle to its farthest
    /// corner. A corner really is the farthest point: on a lon/lat rectangle
    /// the angular distance from a fixed direction is maximised at a latitude
    /// edge and at the largest `|dsx|`, which is a corner. For
    /// `theta >= 90°` (z1, and z0) that ball degenerates, so those levels
    /// fall back to the whole unit sphere and are never frustum-culled —
    /// which costs nothing, because the descent has at most 4 tiles to test
    /// there and the horizon has already pruned the far ones.
    ///
    /// **Only meaningful while [`Self::is_globe`].** On [`Self::FLAT`] every
    /// field is zero and every tile tests visible, which is why the
    /// `debug_assert` is here and why [`Self::cover`] returns an `Option`
    /// rather than leaving the same trap in the selection path.
    #[must_use]
    pub fn tile_visible(&self, tile: TileId) -> bool {
        debug_assert!(self.is_globe(), "GlobeCover::tile_visible on a flat frame says nothing");
        self.visibility(tile).is_some()
    }

    /// [`Self::tile_visible`], carrying out the near-side score
    /// [`Self::cover`] sorts on so it is not computed twice.
    fn visibility(&self, tile: TileId) -> Option<f64> {
        if crate::projection::active().is_some() {
            // Spike: a flat world view shows every tile. A zoomed-in flat
            // projection needs a frustum test on the tile's projected
            // bounds (plan, phase 2).
            return Some(1.0);
        }
        let max_dot = self.max_dot_over_tile(tile);
        if max_dot + self.plane_w < 0.0 {
            return None;
        }
        if tile.z == 0 {
            return Some(max_dot);
        }
        let span = 1.0 / f64::from(1u32 << tile.z);
        let centre = mercator_to_sphere(
            (f64::from(tile.x) + 0.5) * span,
            (f64::from(tile.y) + 0.5) * span,
        );
        let mut cos_theta = 1.0_f64;
        for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
            let corner =
                mercator_to_sphere((f64::from(tile.x) + dx) * span, (f64::from(tile.y) + dy) * span);
            cos_theta = cos_theta.min(centre.dot(corner));
        }
        // The pole itself, for a pole-row tile, for the same reason
        // `max_dot_over_tile` extends its latitude interval: the tile's pole
        // cap reaches it, so the bounding ball has to contain it. Widening
        // the ball only loosens the frustum test.
        if tile.y == 0 {
            cos_theta = cos_theta.min(centre.y);
        }
        if tile.y == (1u32 << tile.z) - 1 {
            cos_theta = cos_theta.min(-centre.y);
        }
        let (ball_centre, radius) = if cos_theta > 0.0 {
            (centre * cos_theta, (1.0 - cos_theta * cos_theta).max(0.0).sqrt())
        } else {
            (glam::DVec3::ZERO, 1.0)
        };
        for plane in &self.planes {
            if plane.truncate().dot(ball_centre) + plane.w > radius {
                return None;
            }
        }
        Some(max_dot)
    }

    /// Every tile of level `zoom` the sphere shows, nearest the camera first.
    ///
    /// A depth-first descent from **one** root tile, exactly MapLibre's
    /// (`covering_tiles.ts:248-322`). One root, not Mercator's three copies
    /// per side (`:252-258`), because `allowWorldCopies()` is `false` on the
    /// globe (`globe_covering_tiles_details_provider.ts:104-106`) — a sphere
    /// has no second copy of itself to draw.
    ///
    /// **The descent is not an optimisation, it is what makes a hemisphere
    /// affordable.** Scanning level `z` directly is `4^z` tests — 1,024 at
    /// z5, the highest level the globe band reaches. Descending tests only
    /// the children of tiles that are already visible, and
    /// `the_descent_finds_exactly_what_a_full_level_scan_would` pins that the
    /// two agree: a parent is never culled while a child is visible, because
    /// the horizon maximum over a parent's rectangle is the maximum over the
    /// union of its children's, and a parent's bounding ball contains its
    /// children's caps.
    ///
    /// **That same property is what keeps stand-in tiles working here.**
    /// `MapView::coarse_prefetch_tiles` fetches the cover some levels down so
    /// `find_cached_ancestor` has something to magnify into a hole, and an
    /// ancestor is only found if it was fetched. Since an ancestor of a
    /// visible tile is itself visible, the coarse globe cover contains every
    /// ancestor the visible globe cover could ask for — so the hole-filling
    /// this project measured (90.5 holes reactively against 55.2 with the
    /// coarse cover) is not something the globe path quietly loses.
    ///
    /// MapLibre's `fullyVisible` shortcut (`:266,271-278`, skipping the test
    /// for descendants of a fully-visible tile) is deliberately not ported:
    /// it needs the three-way `IntersectionResult` a convex volume gives and
    /// a ball does not, and it saves a test that costs one `cos`.
    ///
    /// **Ordering.** MapLibre sorts by squared distance from the centre point
    /// (`:309,324`) so the middle of the screen is fetched first. Ours sorts
    /// by how far in front of the horizon plane the tile reaches, descending.
    /// At zero pitch that is the same ordering — the plane's normal points at
    /// the camera's look-at — and under pitch it is arguably better, since
    /// the plane tilts with the eye rather than staying with the look-at.
    /// It also needs nothing the cull did not already compute. Ties break on
    /// `(x, y)` so the list is identical frame to frame; a cover whose order
    /// churned would reshuffle the fetch queue every frame.
    ///
    /// **`None` when the globe is off** ([`Self::FLAT`]), which is why the
    /// return type is an `Option` rather than a possibly-empty `Vec`. On a
    /// flat frame every field of this struct is zero, so every tile passes
    /// both tests and the descent would hand back the ENTIRE level — 4,096
    /// tiles at z6, measured. Neither a full level nor an empty cover is a
    /// sane answer to "what does the sphere show when there is no sphere";
    /// the only sane answer is "ask the flat path", and `None` is the one
    /// shape a caller cannot accidentally use as a tile list.
    #[must_use]
    pub fn cover(&self, zoom: u8) -> Option<Vec<TileId>> {
        if !self.is_globe() {
            return None;
        }
        let mut scored: Vec<(TileId, f64)> = Vec::new();
        let mut stack = vec![TileId { z: 0, x: 0, y: 0 }];
        while let Some(tile) = stack.pop() {
            let Some(score) = self.visibility(tile) else { continue };
            if tile.z >= zoom {
                scored.push((tile, score));
                continue;
            }
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                stack.push(TileId { z: tile.z + 1, x: tile.x * 2 + dx, y: tile.y * 2 + dy });
            }
        }
        scored.sort_by(|(a_tile, a_score), (b_tile, b_score)| {
            b_score
                .partial_cmp(a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a_tile.x.cmp(&b_tile.x))
                .then(a_tile.y.cmp(&b_tile.y))
        });
        Some(scored.into_iter().map(|(tile, _)| tile).collect())
    }
}

/// **Globeness of the PROJECTION**: `1.0` at [`PROJECTION_BLEND_ZOOM_MIN`]
/// and below (pure vertical perspective), `0.0` at
/// [`PROJECTION_BLEND_ZOOM_MAX`] and above (pure Mercator), and
/// monotonically decreasing in between.
///
/// This is `u_projection_transition`: the weight `interpolate_projection`
/// puts on the sphere arm of its clip-space mix, and the flag that decides
/// whether the globe exists for this frame at all ([`GlobeCover::is_globe`],
/// [`GlobeAnchors::is_globe`], `GlobeParams::FLAT`).
///
/// **It is NOT the number that fades the backdrop, the atmosphere or the
/// stars** — that is [`space_transition_for_zoom`], which keeps the z3–z6
/// range this function used to share with it. Read
/// [`PROJECTION_BLEND_ZOOM_MIN`] for the measurements that separated them:
/// a blend at z3–z6 moves a vertex by tens of pixels and a pole-cap vertex
/// by 1,771, because Mercator's poles are at infinity.
///
/// **Linear, matching the reference's `['interpolate', ['linear'], ...]`**
/// (`projection_factory.ts:40-50`). This used to be a smoothstep, justified
/// as keeping a zoom animation from showing a kink in the rate at either
/// end of the band. Over a band one zoom level wide whose two endpoints are
/// 3.1 px apart at the frame corner, the entire blend is smaller than the
/// kink the easing was protecting against, and matching the reference is
/// worth more than smoothing three pixels.
pub fn transition_for_zoom(zoom: f64) -> f32 {
    if zoom <= PROJECTION_BLEND_ZOOM_MIN {
        return 1.0;
    }
    if zoom >= PROJECTION_BLEND_ZOOM_MAX {
        return 0.0;
    }
    // s runs 0 -> 1 as zoom runs MIN -> MAX, i.e. as globeness runs 1 -> 0.
    let s = (zoom - PROJECTION_BLEND_ZOOM_MIN)
        / (PROJECTION_BLEND_ZOOM_MAX - PROJECTION_BLEND_ZOOM_MIN);
    (1.0 - s) as f32
}

/// **How much of a ball in space the planet still is**: `1.0` at
/// [`GLOBE_ZOOM_MIN`] and below, `0.0` at [`GLOBE_ZOOM_MAX`] and above,
/// smooth and monotonically decreasing in between.
///
/// Read by everything that draws the planet as an object with an outside —
/// the ocean backdrop (`render::space::draws_backdrop`), the scattering
/// shell (`render::atmosphere::Atmosphere::blend`), the starfield
/// (`render::space::Space::draws_stars`), the clear colour
/// (`render::space::space_clear_color`) and the brand mark's ink choice.
/// **Not** read by the projection — see [`transition_for_zoom`].
///
/// This is the curve, the range and the easing that `transition_for_zoom`
/// carried before the two were separated, so every frame's space effects
/// are exactly what they were.
///
/// The interpolation is smoothstep, `3s^2 - 2s^3`, rather than linear: its
/// first derivative is zero at both ends, so the air and the stars neither
/// start nor stop fading with a visible kink as a zoom animation crosses
/// either boundary. A linear blend is C0 only, and the discontinuity in the
/// rate is exactly the sort of thing that reads as a stutter on a
/// continuous pinch.
pub fn space_transition_for_zoom(zoom: f64) -> f32 {
    if zoom <= GLOBE_ZOOM_MIN {
        return 1.0;
    }
    if zoom >= GLOBE_ZOOM_MAX {
        return 0.0;
    }
    // s runs 0 -> 1 as zoom runs MIN -> MAX, i.e. as globeness runs 1 -> 0.
    let s = (zoom - GLOBE_ZOOM_MIN) / (GLOBE_ZOOM_MAX - GLOBE_ZOOM_MIN);
    let smooth = s * s * (3.0 - 2.0 * s);
    (1.0 - smooth) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The cover never leaves a visible tile out, anywhere in the globe
    /// band — and its size FALLING as you zoom in is correct, not a
    /// collapse.**
    ///
    /// # Why this test exists
    ///
    /// A zoom sweep on the running app appeared to show the cover shedding
    /// tiles and falling back to coarser levels as the zoom rose — 26 tiles
    /// at z3.14 down to 3 tiles at z1 by z3.60 — which would be a bare
    /// planet with the backdrop showing through, and which is what a user
    /// reported seeing. That sweep turned out to have been taken while a
    /// person was driving the same camera, so the counts belonged to
    /// unknown cameras. This test is the controlled version: one fixed
    /// camera, only the zoom moving, every quantity computed here.
    ///
    /// # The property that actually matters, and the one that does not
    ///
    /// "Tile count is non-decreasing in zoom" is **false, and correctly
    /// so** — it is the shape the contaminated sweep suggested and it must
    /// not be asserted. Within one level the visible cap SHRINKS as the
    /// camera comes closer (its half-angle is `acos(r/D)`, and `D` falls
    /// with zoom), so fewer fixed-size tiles of that level intersect it.
    /// Measured at lon 170, lat 30, 1024x768, level = `floor(zoom)` as
    /// `visible_tiles` picks it:
    ///
    /// ```text
    ///   zoom   level   cap half-angle   cover   really visible   of level
    ///   2.60       2         62.50 deg      9                9         16
    ///   2.80       2         60.25 deg      9                8         16
    ///   3.00       3         57.93 deg     19               15         64
    ///   3.14       3         56.29 deg     19               11         64
    ///   3.30       3         54.40 deg     19               11         64
    ///   3.48       3         52.25 deg     15               11         64
    ///   3.60       3         50.82 deg     15               11         64
    ///   3.80       3         48.44 deg     11               11         64
    ///   4.00       4         46.09 deg     22               14        256
    ///   4.50       4         40.36 deg     18                9        256
    ///   5.00       5         35.01 deg     24               11       1024
    /// ```
    ///
    /// The count falls **within** a level and jumps **across** one, which is
    /// the sawtooth any fixed-level cover of a shrinking cap must have. What
    /// must hold at every step, and is what this test asserts, is that the
    /// cover is a SUPERSET of what is genuinely visible — that is the
    /// property whose failure would leave a bare patch — and that the
    /// depth-first descent agrees exactly with a direct scan of the level,
    /// so no ancestor is ever pruned while a descendant is visible.
    ///
    /// "Genuinely visible" is brute-forced rather than reasoned: a 33x33
    /// grid over each tile, a point counting when it is both in front of the
    /// horizon plane and inside all five clip half-spaces. Measured: **0
    /// tiles missed, at every zoom in the table.**
    // `3.14` below is the zoom the globe was seen breaking at, read off
    // the title bar, and `3.48` is where it got worse. Neither is an
    // approximation of PI; keeping the observed figures is the whole
    // point of using them.
    #[allow(clippy::approx_constant)]
    #[test]
    fn the_globe_cover_never_leaves_a_visible_tile_out_across_the_zoom_band() {
        let mut previous_cap = f64::INFINITY;
        for zoom in [2.6f64, 2.8, 3.0, 3.14, 3.3, 3.48, 3.6, 3.8, 4.0, 4.5, 5.0] {
            let camera = Camera {
                center_lon: 170.0,
                center_lat: 30.0,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1024.0,
                viewport_height_px: 768.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            };
            let frame = globe_frame(&camera);
            let cover = GlobeCover::new(&frame);
            // `visible_tiles` picks `floor(zoom)`, so the cover is asked for
            // that level and this test asks for the same one.
            let level = zoom.floor() as u8;
            let selected = cover.cover(level).expect("the globe is on across this band");

            // 1. THE DESCENT AGREES WITH A DIRECT SCAN of the level. A
            // parent pruned while a child is visible is how a quarter of the
            // sphere disappears while still looking like a plausible globe.
            let side = 1u32 << level;
            let direct: Vec<TileId> = (0..side)
                .flat_map(|y| (0..side).map(move |x| TileId { z: level, x, y }))
                .filter(|tile| cover.visibility(*tile).is_some())
                .collect();
            let mut a = selected.clone();
            let mut b = direct.clone();
            a.sort_by_key(|t| (t.z, t.x, t.y));
            b.sort_by_key(|t| (t.z, t.x, t.y));
            assert_eq!(
                a, b,
                "at zoom {zoom} the depth-first descent and a direct scan of level {level}                  must select the same tiles: the descent found {}, the scan {}",
                selected.len(),
                direct.len()
            );

            // 2. THE COVER IS A SUPERSET OF WHAT IS GENUINELY VISIBLE.
            // Brute force, not reasoning: a 33x33 grid per tile, a point
            // counting when it is in front of the horizon AND inside every
            // clip plane.
            let plane_n = glam::DVec3::new(
                cover.plane_spherical_x.sin() * cover.plane_cos_lat,
                cover.plane_sin_lat,
                cover.plane_spherical_x.cos() * cover.plane_cos_lat,
            );
            let span = 1.0 / f64::from(side);
            let mut really_visible = 0usize;
            let mut missed: Vec<TileId> = Vec::new();
            for y in 0..side {
                for x in 0..side {
                    let tile = TileId { z: level, x, y };
                    let visible = (0..=32).any(|i| {
                        (0..=32).any(|j| {
                            let p = mercator_to_sphere(
                                (f64::from(x) + f64::from(i) / 32.0) * span,
                                (f64::from(y) + f64::from(j) / 32.0) * span,
                            );
                            p.dot(plane_n) + cover.plane_w >= 0.0
                                && cover
                                    .planes
                                    .iter()
                                    .all(|plane| plane.truncate().dot(p) + plane.w <= 0.0)
                        })
                    });
                    if visible {
                        really_visible += 1;
                        if !selected.contains(&tile) {
                            missed.push(tile);
                        }
                    }
                }
            }
            assert!(
                missed.is_empty(),
                "at zoom {zoom}, level {level}: {} tiles of the {really_visible} that are                  genuinely visible are NOT in the cover -- every one of them is a bare patch                  with the backdrop showing through: {missed:?}",
                missed.len()
            );
            assert!(
                really_visible > 0 && !selected.is_empty(),
                "at zoom {zoom} the sphere must show something, or this step grades nothing:                  {really_visible} visible, {} selected",
                selected.len()
            );

            // 3. AND THE REASON the count falls: the visible cap shrinks
            // monotonically as the camera comes closer. This is the
            // assertion that replaces the "count must not fall" one, which
            // is false.
            let cap = (-cover.plane_w).acos().to_degrees();
            assert!(
                cap < previous_cap,
                "the visible cap must shrink as the zoom rises -- it is why a fixed level's                  tile count falls -- but at zoom {zoom} it is {cap:.2} deg against                  {previous_cap:.2} deg at the step before"
            );
            previous_cap = cap;
        }
    }

    /// A sun for tests that are not ABOUT the sun: a unit direction (so
    /// `GlobeUpload::new`'s own expectation holds) and a night side exactly
    /// as bright as the day side, which is what `night_brightness == 1.0`
    /// means. Every assertion in this module is about a matrix, a cover or a
    /// plane, none of which the light touches.
    const UNLIT_SUN: glam::DVec3 = glam::DVec3::Z;
    const UNLIT_NIGHT: f32 = 1.0;

    fn camera(zoom: f64) -> Camera {
        Camera {
            center_lon: 13.4,
            center_lat: 52.5,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1200.0,
            viewport_height_px: 900.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    #[test]
    fn the_half_angle_substitution_matches_the_direct_latitude_formula() {
        // The whole reason for the substitution is float32 precision on a GPU, so
        // in f64 on the CPU the two MUST agree closely -- if they do not, the
        // algebra was transcribed wrong, and that is a bug no rendering test
        // would localise.
        for y in [0.0, 0.25, 0.4999, 0.5, 0.5001, 0.75, 1.0] {
            let (sin_direct, cos_direct) = {
                let lat = 2.0
                    * ((std::f64::consts::PI - y * 2.0 * std::f64::consts::PI).exp()).atan()
                    - std::f64::consts::FRAC_PI_2;
                (lat.sin(), lat.cos())
            };
            let (sin_sub, cos_sub) = mercator_y_to_sin_cos_lat(y);
            assert!(
                (sin_sub - sin_direct).abs() < 1e-12,
                "sin mismatch at y={y}: {sin_sub} vs {sin_direct}"
            );
            assert!(
                (cos_sub - cos_direct).abs() < 1e-12,
                "cos mismatch at y={y}: {cos_sub} vs {cos_direct}"
            );
        }
    }

    /// **[`GlobeFrame::globe_view`] and [`GlobeFrame::globe_view_proj`] are
    /// two expressions of one chain, and this is the guard that they have
    /// not drifted apart.**
    ///
    /// `globe_frame` deliberately writes the chain TWICE — once with the
    /// projection in front and once without — rather than composing the
    /// second from the first, because `proj * (view)` and
    /// `((((proj * T) * Rx) * Rz) ...)` are the same matrix in exact
    /// arithmetic and **not** the same matrix in f64: glam's `*` is
    /// left-associative, so re-bracketing moves the last bits, and every
    /// globe pixel test in this crate was graded against the original
    /// bracketing. Keeping `globe_view_proj`'s expression byte-identical is
    /// what makes this task's diff invisible to them.
    ///
    /// The price of that decision is exactly this test: two expressions can
    /// drift. 1e-9 is a loose tolerance on purpose — it is checking that
    /// nobody edited one chain and not the other, not that the re-bracketing
    /// is exact, which it is not. The worst element disagreement measured
    /// over the four cameras below is **1.137e-13**, against a largest
    /// matrix entry of **2,401.3** over the same four — so roughly 2e-17
    /// relative, a couple of hundred ULP of the smallest entries and a
    /// fraction of one ULP of the largest. That is what re-bracketing seven
    /// products costs, and nothing more.
    #[test]
    fn the_view_half_and_the_projection_compose_into_the_matrix_the_shader_gets() {
        let cameras = [
            Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 1.5,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 512.0,
                viewport_height_px: 512.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            Camera {
                center_lon: -120.0,
                center_lat: 25.0,
                zoom: 2.5,
                pitch: 0.5,
                bearing: 0.7,
                viewport_width_px: 1024.0,
                viewport_height_px: 768.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            Camera {
                center_lon: 139.0,
                center_lat: -60.0,
                zoom: 5.0,
                pitch: 1.0,
                bearing: -2.0,
                viewport_width_px: 800.0,
                viewport_height_px: 600.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            Camera {
                center_lon: 10.0,
                center_lat: 80.0,
                zoom: 3.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 400.0,
                viewport_height_px: 1200.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
        ];
        let mut worst = 0.0_f64;
        for camera in cameras {
            let frame = globe_frame(&camera);
            let composed = globe_projection(&camera) * frame.globe_view;
            for column in 0..4 {
                for row in 0..4 {
                    let a = composed.col(column)[row];
                    let b = frame.globe_view_proj.col(column)[row];
                    worst = worst.max((a - b).abs());
                }
            }
            // ...and the view half really is only the view half: it must NOT
            // equal the full matrix, or `globe_view` picked up the
            // projection and every ray the atmosphere builds from it would be
            // bent by the near plane.
            assert_ne!(frame.globe_view, frame.globe_view_proj);
        }
        assert!(
            worst < 1e-9,
            "`globe_view` and `globe_view_proj` disagree by {worst} -- one chain was \
             edited and the other was not"
        );
    }

    #[test]
    fn the_half_angle_substitution_holds_to_two_ulp_across_and_beyond_the_square() {
        // The brief's tolerance above is 1e-12. The identity is in fact exact
        // to within rounding: swept at 1e-4 over y in [-0.2, 1.2] (past both
        // edges of the Mercator square, since wrapped and overscaled tiles go
        // there) the largest deviation from the direct formula is 4.44e-16 --
        // 2 ulp at magnitude 1. Anything looser than this would let a real
        // transcription error through unnoticed.
        let mut worst = 0.0_f64;
        let mut y = -0.2;
        while y <= 1.2 {
            let t = (PI - y * 2.0 * PI).exp();
            let lat = 2.0 * t.atan() - std::f64::consts::FRAC_PI_2;
            let (sin_sub, cos_sub) = mercator_y_to_sin_cos_lat(y);
            worst = worst.max((sin_sub - lat.sin()).abs());
            worst = worst.max((cos_sub - lat.cos()).abs());
            y += 1e-4;
        }
        assert!(worst <= 1e-15, "worst deviation was {worst}");
    }

    #[test]
    fn the_substitution_is_exact_at_the_equator_where_the_direct_formula_cancels() {
        // y = 0.5 is the equator, the case the substitution exists for: t == 1,
        // so `t*t - 1` is an exact zero rather than the residue of subtracting
        // PI/2 from something near PI/2.
        let (sin, cos) = mercator_y_to_sin_cos_lat(0.5);
        assert_eq!(sin, 0.0);
        assert_eq!(cos, 1.0);
    }

    #[test]
    fn the_substitution_lands_on_the_web_mercator_latitude_limits_at_the_edges() {
        // y = 0 is the top of the Mercator square, ~ +85.051129 degrees.
        let (sin_top, _) = mercator_y_to_sin_cos_lat(0.0);
        assert!(
            (sin_top.asin().to_degrees() - 85.051_128_779_806_59).abs() < 1e-9,
            "top edge was {}",
            sin_top.asin().to_degrees()
        );
        let (sin_bottom, _) = mercator_y_to_sin_cos_lat(1.0);
        assert!(
            (sin_bottom.asin().to_degrees() + 85.051_128_779_806_59).abs() < 1e-9,
            "bottom edge was {}",
            sin_bottom.asin().to_degrees()
        );
    }

    #[test]
    fn the_space_fade_is_flat_at_both_ends_and_monotonic_between() {
        // NOTE: the brief asserted `transition_for_zoom(0.0) == 0.0` here, and
        // flagged its own assertion as a question. It is wrong, and so was the
        // brief's one-line summary of this function ("0 below GLOBE_ZOOM_MIN, 1
        // above GLOBE_ZOOM_MAX"). Zoomed OUT is the globe, and this is a
        // globeness (1 = a ball in space), so the polarity runs the other way:
        // 1 at and below GLOBE_ZOOM_MIN, 0 at and above GLOBE_ZOOM_MAX,
        // DEcreasing in between. See `GLOBE_ZOOM_MIN`'s doc comment.
        //
        // This used to grade `transition_for_zoom`, back when one number did
        // both jobs. The curve, the range and the easing are unchanged; only
        // the name is, because the PROJECTION cannot blend down here -- see
        // `PROJECTION_BLEND_ZOOM_MIN`.
        assert_eq!(
            space_transition_for_zoom(0.0),
            1.0,
            "zoomed right out the planet is entirely a ball in space; see GLOBE_ZOOM_MIN"
        );
        assert_eq!(space_transition_for_zoom(GLOBE_ZOOM_MIN - 0.1), 1.0);
        assert_eq!(space_transition_for_zoom(GLOBE_ZOOM_MAX + 0.1), 0.0);
        assert_eq!(space_transition_for_zoom(GLOBE_ZOOM_MIN), 1.0);
        assert_eq!(space_transition_for_zoom(GLOBE_ZOOM_MAX), 0.0);
        let mut prev = 2.0;
        let mut z = GLOBE_ZOOM_MIN;
        while z <= GLOBE_ZOOM_MAX {
            let t = space_transition_for_zoom(z);
            assert!(t <= prev, "the space fade must not go backwards at z={z}");
            prev = t;
            z += 0.1;
        }
    }

    #[test]
    fn the_space_fade_is_exactly_half_at_the_midpoint_and_symmetric_about_it() {
        let mid = (GLOBE_ZOOM_MIN + GLOBE_ZOOM_MAX) / 2.0;
        assert!((space_transition_for_zoom(mid) - 0.5).abs() < 1e-6);
        for d in [0.25f64, 0.5, 1.0, 1.4] {
            let lo = space_transition_for_zoom(mid - d);
            let hi = space_transition_for_zoom(mid + d);
            assert!(
                (lo + hi - 1.0).abs() < 1e-6,
                "smoothstep should be symmetric about the midpoint: {lo} + {hi} at d={d}"
            );
        }
    }

    /// **The projection blend and the space fade are different numbers, and
    /// the whole of the space fade's range is pure globe.**
    ///
    /// The one assertion that would have caught the defect in
    /// `PROJECTION_BLEND_ZOOM_MIN`'s doc comment without rendering anything:
    /// every zoom at which the planet is still drawn as a ball — backdrop,
    /// air, stars — must be projected with NO Mercator in the matrix,
    /// because down there the two projections are hundreds of pixels apart
    /// and a mix of them belongs to neither.
    #[test]
    fn nothing_that_is_still_a_ball_in_space_has_any_mercator_in_its_matrix() {
        let mut z = 0.0_f64;
        while z <= GLOBE_ZOOM_MAX {
            assert_eq!(
                transition_for_zoom(z),
                1.0,
                "z{z} still draws space effects (space fade {}), so it must be pure \
                 vertical perspective -- see PROJECTION_BLEND_ZOOM_MIN for what a \
                 partial blend does down here",
                space_transition_for_zoom(z)
            );
            z += 0.05;
        }
        assert_eq!(
            space_transition_for_zoom(PROJECTION_BLEND_ZOOM_MIN),
            0.0,
            "and the converse: by the time the projection starts blending, the planet \
             stopped being a ball in space five zoom levels ago"
        );
    }

    #[test]
    fn the_projection_blend_runs_over_the_reference_band_and_is_linear_in_it() {
        assert_eq!(transition_for_zoom(PROJECTION_BLEND_ZOOM_MIN), 1.0);
        assert_eq!(transition_for_zoom(PROJECTION_BLEND_ZOOM_MAX), 0.0);
        assert_eq!(transition_for_zoom(PROJECTION_BLEND_ZOOM_MAX + 0.1), 0.0);
        // `['interpolate', ['linear'], ['zoom'], 11, ..., 12, ...]`:
        // halfway through the band is halfway through the blend.
        let mid = (PROJECTION_BLEND_ZOOM_MIN + PROJECTION_BLEND_ZOOM_MAX) / 2.0;
        assert!((transition_for_zoom(mid) - 0.5).abs() < 1e-6);
        for f in [0.25_f64, 0.5, 0.75] {
            let z = PROJECTION_BLEND_ZOOM_MIN
                + f * (PROJECTION_BLEND_ZOOM_MAX - PROJECTION_BLEND_ZOOM_MIN);
            assert!(
                (f64::from(transition_for_zoom(z)) - (1.0 - f)).abs() < 1e-6,
                "linear interpolation, not an easing: transition at z{z} was {}",
                transition_for_zoom(z)
            );
        }
    }

    #[test]
    fn tile_mercator_coords_tile_the_unit_square_without_gaps() {
        // z0 is the whole square.
        assert_eq!(tile_mercator_coords(TileId { z: 0, x: 0, y: 0 }), [0.0, 0.0, 1.0, 1.0]);
        // The four z1 tiles butt up against each other and cover exactly [0,1]^2.
        for (x, y, ox, oy) in [(0u32, 0u32, 0.0f32, 0.0f32), (1, 0, 0.5, 0.0), (0, 1, 0.0, 0.5), (1, 1, 0.5, 0.5)] {
            assert_eq!(tile_mercator_coords(TileId { z: 1, x, y }), [ox, oy, 0.5, 0.5]);
        }
        // The last tile of a deep zoom still ends exactly at 1.0.
        let z = 14u8;
        let last = (1u32 << z) - 1;
        let c = tile_mercator_coords(TileId { z, x: last, y: last });
        assert!((c[0] + c[2] - 1.0).abs() < 1e-6, "x edge was {}", c[0] + c[2]);
        assert!((c[1] + c[3] - 1.0).abs() < 1e-6, "y edge was {}", c[1] + c[3]);
    }

    #[test]
    fn tile_mercator_coords_agree_with_the_crates_own_lon_lat_conversion() {
        // The Mercator coords must address the same square `geo` does, or the
        // sphere would be textured with the tiles shifted.
        let tile = TileId { z: 5, x: 17, y: 10 };
        let c = tile_mercator_coords(tile);
        let (lon, lat) = crate::geo::tile_xy_to_lon_lat(f64::from(tile.x), f64::from(tile.y), f64::from(tile.z));
        // The tile's own top-left corner, pushed through the shader's route.
        let (sin_lat, _) = mercator_y_to_sin_cos_lat(f64::from(c[1]));
        assert!(
            (sin_lat.asin().to_degrees() - lat).abs() < 1e-4,
            "lat {} vs {lat}",
            sin_lat.asin().to_degrees()
        );
        let lon_from_merc = f64::from(c[0]) * 360.0 - 180.0;
        assert!((lon_from_merc - lon).abs() < 1e-4, "lon {lon_from_merc} vs {lon}");
    }

    #[test]
    fn the_clipping_plane_puts_the_near_surface_in_front_and_the_far_side_behind() {
        let f = globe_frame(&camera(2.0));
        let n = glam::DVec3::new(f.clipping_plane[0], f.clipping_plane[1], f.clipping_plane[2]);
        let w = f.clipping_plane[3];
        // The point the camera looks at: centre lon/lat on the unit sphere, in
        // the shader's axis convention (y = north).
        let lon = camera(2.0).center_lon.to_radians();
        let lat = camera(2.0).center_lat.to_radians();
        let near_point = glam::DVec3::new(lon.sin() * lat.cos(), lat.sin(), lon.cos() * lat.cos());
        assert!(
            n.dot(near_point) + w > 0.0,
            "the point under the camera must be visible, got {}",
            n.dot(near_point) + w
        );
        assert!(
            n.dot(-near_point) + w < 0.0,
            "the antipode must be clipped, got {}",
            n.dot(-near_point) + w
        );
    }

    #[test]
    fn the_clipping_plane_is_a_unit_normal_and_relaxes_towards_a_great_circle() {
        // The plane through the tangent points sits `r / |cam - C|` from the
        // centre. Zooming OUT moves the eye further away in radii, so that
        // distance SHRINKS towards zero -- in the limit the silhouette is a
        // great circle through the centre and exactly half the sphere is
        // visible. Zooming in pushes it the other way, towards the surface,
        // where only a small cap is visible. It is strictly inside (0, 1) at
        // every finite distance.
        let mut prev = 1.0;
        for zoom in [6.0, 5.0, 4.0, 3.0, 2.0, 1.0, 0.0] {
            let f = globe_frame(&camera(zoom));
            let n = glam::DVec3::new(f.clipping_plane[0], f.clipping_plane[1], f.clipping_plane[2]);
            assert!((n.length() - 1.0).abs() < 1e-12, "normal not unit at z{zoom}");
            let offset = -f.clipping_plane[3];
            assert!(offset > 0.0, "the plane must not pass the sphere's centre, z{zoom}");
            assert!(offset < 1.0, "the plane must not reach the surface, z{zoom}");
            assert!(offset < prev, "offset should shrink as the camera recedes, z{zoom}");
            prev = offset;
        }
    }

    #[test]
    fn the_globe_matrix_puts_the_camera_centre_at_the_middle_of_the_screen() {
        for (lon, lat, zoom) in [(0.0, 0.0, 1.0), (13.4, 52.5, 3.0), (-122.4, 37.8, 0.0), (151.2, -33.9, 2.0)] {
            let cam = Camera {
                center_lon: lon,
                center_lat: lat,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1200.0,
                viewport_height_px: 900.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            };
            let f = globe_frame(&cam);
            let lo = lon.to_radians();
            let la = lat.to_radians();
            let p = glam::DVec3::new(lo.sin() * la.cos(), la.sin(), lo.cos() * la.cos());
            let clip = f.globe_view_proj * p.extend(1.0);
            assert!(clip.w > 0.0, "the near surface must be in front of the eye at {lon},{lat}");
            assert!(
                (clip.x / clip.w).abs() < 1e-9 && (clip.y / clip.w).abs() < 1e-9,
                "centre landed at {},{} for {lon},{lat}",
                clip.x / clip.w,
                clip.y / clip.w
            );
        }
    }

    #[test]
    fn the_globe_matrix_keeps_the_centre_feature_scale_matching_mercator() {
        // This is what `globe_radius_world`'s cos(lat) term buys: at the map
        // centre, a small step in longitude must move the same number of
        // pixels on the sphere as it does on the flat map, or a zoom level
        // would mean two different things depending on projection.
        for lat in [0.0, 30.0, 52.5, 70.0] {
            let cam = Camera {
                center_lon: 0.0,
                center_lat: lat,
                zoom: 3.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1200.0,
                viewport_height_px: 900.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            };
            let f = globe_frame(&cam);
            // Both sides are computed in f64 straight off the matrices. An
            // earlier version of this test read the Mercator side through
            // `project_world_to_screen_f64`, whose result narrows to f32 --
            // subtracting two ~600 px values to recover a ~0.001 px step there
            // is pure cancellation, and it reported a 1.9% "error" that was
            // entirely f32 quantisation.
            let d_lon = 0.1_f64;
            let half_width = cam.viewport_width_px as f64 / 2.0;
            let px_globe = {
                let to_ndc = |lo: f64| {
                    let la = lat.to_radians();
                    let p = glam::DVec3::new(lo.sin() * la.cos(), la.sin(), lo.cos() * la.cos());
                    let c = f.globe_view_proj * p.extend(1.0);
                    c.x / c.w
                };
                (to_ndc(d_lon.to_radians()) - to_ndc(0.0)).abs() * half_width
            };
            let px_merc = {
                let vp = cam.view_projection_f64();
                let to_ndc = |lo: f64| {
                    let (x, y) = crate::geo::lon_lat_to_tile_xy(lo, lat, 0.0);
                    let c = vp
                        * glam::DVec4::new(
                            x * crate::geo::TILE_SIZE,
                            y * crate::geo::TILE_SIZE,
                            0.0,
                            1.0,
                        );
                    c.x / c.w
                };
                (to_ndc(d_lon) - to_ndc(0.0)).abs() * half_width
            };
            assert!(
                (px_globe - px_merc).abs() / px_merc < 1e-3,
                "lat {lat}: globe {px_globe} px vs mercator {px_merc} px"
            );
        }
    }

    #[test]
    fn globe_frame_reports_the_transition_for_its_own_camera_zoom() {
        assert_eq!(globe_frame(&camera(0.0)).transition, 1.0);
        assert_eq!(globe_frame(&camera(13.0)).transition, 0.0);
        let mid = globe_frame(&camera(11.5)).transition;
        assert!(mid > 0.0 && mid < 1.0, "mid transition was {mid}");
        // And the frame carries the OTHER globeness too, on its own range.
        assert_eq!(globe_frame(&camera(0.0)).space_transition, 1.0);
        assert_eq!(globe_frame(&camera(11.5)).space_transition, 0.0);
        let mid_space = globe_frame(&camera(4.5)).space_transition;
        assert!(mid_space > 0.0 && mid_space < 1.0, "mid space fade was {mid_space}");
    }

    /// **The globe switch, at the CPU level.** The pixel half of this claim
    /// -- that a switched-off frame renders byte-identically to pure
    /// Mercator -- is
    /// `render::gpu`'s `the_globe_switch_renders_a_banded_zoom_as_the_pure_mercator_frame`,
    /// which needs a device. This is the part that does not: the switch
    /// zeroes BOTH transitions at a zoom where each would otherwise be
    /// partial, leaves every matrix alone, and is the identity when on.
    #[test]
    fn switching_the_globe_off_zeroes_both_transitions_and_nothing_else() {
        // z11.1 for the projection (0.8668 today) and z4.5 for the space
        // fade: the two bands do not overlap, so ONE camera cannot have
        // both partial and a test that used one zoom would leave half the
        // claim untested.
        for (zoom, projection_band) in [(11.1, true), (4.5, false)] {
            let frame = globe_frame(&camera(zoom));
            let live = if projection_band { frame.transition } else { frame.space_transition };
            assert!(
                live > 0.0 && live < 1.0,
                "z{zoom} must be inside its band for this case to grade anything, got {live}"
            );

            let off = frame.flattened();
            assert_eq!(off.transition, 0.0, "z{zoom}: the projection must be pure Mercator");
            assert_eq!(off.space_transition, 0.0, "z{zoom}: no limb, so no space dressing");
            assert_eq!(off.globe_view_proj, frame.globe_view_proj, "z{zoom}: matrices untouched");
            assert_eq!(off.globe_view, frame.globe_view);
            assert_eq!(off.mercator_view_proj, frame.mercator_view_proj);
            assert_eq!(off.clipping_plane, frame.clipping_plane);

            // Structurally off, by the same values every pre-existing flat
            // path keys on -- not merely "a small transition".
            assert_eq!(GlobeCover::new(&off), GlobeCover::FLAT);
            assert!(!GlobeCover::new(&off).is_globe());
            assert!(!GlobeAnchors::new(&off).is_globe());

            // On is the identity, so a consumer that never touches the
            // switch cannot be affected by its existence.
            assert_eq!(frame.with_globe_enabled(true), frame, "z{zoom}: `true` must change nothing");
            assert_eq!(frame.with_globe_enabled(false), off);
        }
    }

    #[test]
    fn the_mercator_fallback_is_exactly_the_cameras_own_view_projection() {
        // Not a near-copy, not a rebuild: the same matrix, so the two
        // projections cannot drift apart while blending.
        let cam = camera(4.0);
        assert_eq!(globe_frame(&cam).mercator_view_proj, cam.view_projection_f64());
    }

    /// **The per-frame discipline, asserted at runtime.** A frame with N
    /// visible tiles calls [`globe_frame`] exactly ONCE, whatever N is.
    ///
    /// This is the assertion the plan's whole divergence rests on: MapLibre
    /// calls `getProjectionData` per tile per frame and that function invokes
    /// BOTH sub-transforms and blends (`globe_transform.ts:284-295`), so their
    /// globe matrix is rebuilt once per tile. The shape that prevents it here
    /// is [`GlobeUpload`]: the expensive half is a value you have to already
    /// hold, and `for_tile` is a method on it rather than a free function that
    /// could quietly re-derive it.
    ///
    /// Sixty-four tiles is a full z3 world, comfortably more than the ~16
    /// `MapView::tiles_for_zoom` actually selects there, so the count is
    /// tested against a tile set larger than any the globe band produces.
    fn a_frame_with_many_tiles_builds_the_globe_frame_once(tiles: usize) {
        let cam = camera(3.0);
        reset_globe_frame_calls();
        let upload =
            GlobeUpload::new(&globe_frame(&cam), &cam, 4096.0, UNLIT_SUN, UNLIT_NIGHT);
        let per_tile: Vec<_> = (0..tiles)
            .map(|i| upload.for_tile(TileId { z: 3, x: (i as u32) % 8, y: (i as u32) / 8 }))
            .collect();
        assert_eq!(
            globe_frame_calls(),
            1,
            "{tiles} tiles must cost ONE globe_frame, not one each -- see render::globe's \
             module docs on the 74.7x this project already measured for exactly this shape"
        );
        // And the per-tile half really is per-tile: if `for_tile` ignored its
        // argument every tile would be projected onto the same patch of
        // sphere, which is the other way this can be wrong and is invisible
        // to a call count.
        let distinct: std::collections::HashSet<[u32; 4]> = per_tile
            .iter()
            .map(|p| p.tile_mercator_coords.map(f32::to_bits))
            .collect();
        assert_eq!(distinct.len(), tiles, "every tile needs its own Mercator coords");
        // Everything else in the uniform is the FRAME's, shared by value.
        for params in &per_tile {
            assert_eq!(params.globe_matrix, per_tile[0].globe_matrix);
            assert_eq!(params.clipping_plane, per_tile[0].clipping_plane);
            assert_eq!(params.transition, per_tile[0].transition);
        }
    }

    #[test]
    fn sixty_four_tiles_in_one_frame_cost_one_globe_frame() {
        a_frame_with_many_tiles_builds_the_globe_frame_once(64);
    }

    #[test]
    fn the_globe_upload_carries_the_frames_transition_and_the_tiles_scales() {
        let cam = camera(11.5);
        let frame = globe_frame(&cam);
        let extent = 4096.0_f32;
        let upload = GlobeUpload::new(&frame, &cam, extent, UNLIT_SUN, UNLIT_NIGHT);
        let tile = TileId { z: 4, x: 8, y: 5 };
        let params = upload.for_tile(tile);

        assert_eq!(params.transition, frame.transition);
        assert!(params.transition > 0.0 && params.transition < 1.0, "z11.5 is mid-transition");
        assert_eq!(params.globe_matrix, frame.globe_view_proj.as_mat4().to_cols_array_2d());
        assert_eq!(params.clipping_plane, frame.clipping_plane.map(|c| c as f32));
        assert_eq!(params.tile_mercator_coords, tile_mercator_coords(tile));
        // The two scales the shader needs and MapLibre does not, spelled the
        // way `GlobeParams`' own doc comments define them -- the pixel tests
        // in `render::gpu` compute them by hand at their call site, and this
        // is what keeps the hoisted version agreeing with them.
        assert_eq!(params.lyon_to_tile_unit, 1.0 / (crate::render::mesh::COORD_SCALE * extent));
        let tile_world_size = WORLD_SIZE / 2f32.powi(i32::from(tile.z));
        // `pixel_scale` is in there too -- `GlobeUpload::pixel_scale`. At
        // this camera's latitude it is not 1.0, which is the whole point:
        // without it every stroke on the globe was `1 / cos(52.5 deg)` =
        // 1.64x too wide.
        let pixel_scale = 1.0
            + (1.0 / (cam.center_lat.to_radians().cos() as f32) - 1.0) * frame.transition;
        assert_eq!(
            params.tile_units_per_px,
            1.0 / (tile_world_size * 2f32.powf(cam.zoom as f32) * pixel_scale)
        );
    }

    /// **The frame's sun reaches EVERY tile, the backdrop's z0 included.**
    ///
    /// The light is a property of the frame and not of a tile, so the
    /// interesting claim is a negative one: `for_tile` must not be able to
    /// give two tiles different suns, because a terminator that stepped at
    /// a tile boundary is exactly the split-brain the plan's second
    /// divergence exists to avoid.
    ///
    /// **`BACKDROP_TILE` is named explicitly**, and that is not decoration.
    /// `MapView::paint` draws the ocean sphere — which at z0-z3 is most of
    /// the visible planet, every stretch of ocean no tile covers — through
    /// `globe.for_tile(render::space::BACKDROP_TILE)`. If the sun stopped at
    /// the tile path, the picture would be a shaded patchwork over an
    /// unshaded planet, which is worse than no shading at all.
    /// `render::gpu`'s `backdrop_draw` helper deliberately passes an unlit
    /// sun, so nothing in the pixel tests covers this link; this assertion
    /// is what does.
    #[test]
    fn the_frames_sun_reaches_every_tile_including_the_backdrops() {
        let cam = camera(3.0);
        let frame = globe_frame(&cam);
        let sun = glam::DVec3::new(0.3, 0.5, 0.2).normalize();
        let upload = GlobeUpload::new(&frame, &cam, 4096.0, sun, 0.25);
        let expected = [sun.x as f32, sun.y as f32, sun.z as f32];

        for tile in [
            crate::render::space::BACKDROP_TILE,
            TileId { z: 0, x: 0, y: 0 },
            TileId { z: 3, x: 0, y: 0 },
            TileId { z: 3, x: 7, y: 7 },
            TileId { z: 5, x: 16, y: 10 },
        ] {
            let params = upload.for_tile(tile);
            assert_eq!(
                params.sun_direction, expected,
                "tile {tile:?} got a different sun from the frame's"
            );
            assert_eq!(params.night_brightness, 0.25, "tile {tile:?} got a different night floor");
        }
        // And it is the NARROWED frame value, not something re-derived:
        // `f32` of the `f64` the caller handed in, bit for bit.
        assert_eq!(upload.for_tile(TileId { z: 0, x: 0, y: 0 }).sun_direction[0], sun.x as f32);
    }

    /// `paint()` needs a live `wgpu::Surface`, so no test in this crate can
    /// drive it (see the borrow-safety note in `MapView::paint`). What CAN be
    /// checked is the text a human reads when editing it, and that is the
    /// check that would have caught MapLibre's own mistake: a `globe_frame`
    /// call that drifted inside the tile loop.
    ///
    /// Same idiom, and same justification, as
    /// `render::gpu`'s `the_declared_inter_stage_need_matches_the_shader_it_is_declared_for`,
    /// which scans `shader.wgsl` rather than parsing it.
    #[test]
    fn paint_calls_globe_frame_exactly_once_and_before_its_tile_loops() {
        // Comments stripped first, because the call site's own comment names
        // `globe_frame(&self.camera)` to say what it is NOT doing per tile --
        // and a test that made that comment unwritable would be a test about
        // prose. Cutting each line at its first `//` can only remove text, and
        // nothing this counts can appear after one (a `//` inside a string
        // literal is a URL, not a call).
        let source: String = include_str!("../map_view.rs")
            .lines()
            .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n");
        let source = source.as_str();
        // TWO call sites, and the test names both rather than counting to a
        // looser number. `cover_progress` is the other one: it is asked from
        // OUTSIDE a frame (a camera tour polls it between paints), so it has
        // no frame value to borrow and builds its own -- one camera-level
        // build per external query, which is not the per-tile rebuild this
        // test exists to prevent. A THIRD call site is what this still
        // refuses, and it is why the count is asserted rather than the
        // positions alone.
        //
        // Note this counts the WHOLE file, `map_view.rs`'s own test module
        // included -- so a test in that file cannot build a `GlobeFrame`
        // either. That is not an accident worth working around: the property
        // such a test would want ("above the band the cover is flat") is
        // asserted here, in
        // `the_globe_is_structurally_off_once_the_transition_reaches_zero`,
        // where `globe_frame` is free to call.
        let calls = source.matches("globe_frame(").count();
        assert_eq!(
            calls,
            2,
            "map_view.rs must call globe_frame once per frame in `paint` and once per query in \
             `cover_progress`; found {calls} call sites. A third is almost certainly inside a \
             per-tile loop, which is the per-tile matrix rebuild render::globe exists to prevent."
        );
        let at_progress = source.find("globe_frame(").expect("cover_progress' call site");
        let at_paint_fn = source.find("fn paint(").expect("paint itself");
        assert!(
            at_progress < at_paint_fn,
            "the first globe_frame call site is at byte {at_progress}, inside or after `paint` \
             at {at_paint_fn} -- `paint` must hold exactly the second one"
        );
        let at_call = at_progress + 1 + source[at_progress + 1..].find("globe_frame(").expect("paint's call site");
        assert!(at_call > at_paint_fn, "paint's globe_frame call must be inside paint");
        // Tile SELECTION has to see it too, so the call must precede
        // `visible_tiles` -- the cover is the set of tiles the sphere shows,
        // and computing it needs the horizon plane and the globe frustum.
        let at_visible = source.find("let visible = self.visible_tiles(").expect("paint's cover");
        assert!(
            at_call < at_visible,
            "globe_frame is called at byte {at_call}, after paint selects its tiles at \
             {at_visible} -- the globe cover cannot be computed without it"
        );
        // The first per-tile loop in `paint`: the mesh-upload loop over the
        // upload set. `globe_frame` must be resolved before it, not in it.
        let at_loop = source.find("for &tile in &upload_set {").expect("paint's mesh-upload loop");
        assert!(
            at_call < at_loop,
            "globe_frame is called at byte {at_call}, after paint's per-tile upload loop \
             at {at_loop} -- hoist it above the loop"
        );
        // And the per-tile half is reached from more than one place, because
        // `paint` writes a tile's uniform in two loops (fresh upload, and the
        // stale-matrix rewrite). A `for_tile` count of one means one of them
        // is still passing `GlobeParams::FLAT` and half the drawn tiles would
        // stay flat while the other half curved.
        assert!(
            source.matches("for_tile(").count() >= 2,
            "both of paint's model-uniform writes must go through GlobeUpload::for_tile"
        );
    }

    /// **EAST MUST STILL BE EAST, on BOTH sides of the blend.**
    ///
    /// Task 2 moved `shader.wgsl`'s Mercator x-flip (`clip.x = -clip.x`) off
    /// `out.clip_position` and onto the flat position, *before*
    /// `interpolate_projection`, and flagged it for whoever wired the globe
    /// in. The reason is that the flip undoes a property of
    /// `camera::eye_target_up`'s basis (world +x lands on NDC -x), while
    /// [`globe_frame`] builds its matrix from scratch and does not inherit
    /// that basis. Flip the blended result and the globe comes out mirrored --
    /// and a mirrored globe renders beautifully, satisfies the disc test, the
    /// clipping-plane tests and the centre-scale test, and is wrong.
    ///
    /// Task 3 is the commit that makes `transition` nonzero, so this is where
    /// that has to be checked. The check is the two projections agreeing on
    /// the sign: a point 10 degrees EAST of the camera centre must land right
    /// of screen centre through the Mercator chain (flip included) and right
    /// of screen centre through the globe chain. If they disagree, the blend
    /// between them sweeps a vertex across the screen instead of bending it.
    ///
    /// `render::gpu`'s `the_globe_draws_east_on_the_right_like_the_flat_map_does`
    /// is the same claim in pixels, through the real shader.
    #[test]
    fn the_globe_and_mercator_chains_agree_on_which_way_is_east() {
        for (lon, lat) in [(0.0, 0.0), (13.4, 52.5), (-122.4, 37.8), (151.2, -33.9), (179.0, 5.0)] {
            let cam = Camera {
                center_lon: lon,
                center_lat: lat,
                zoom: 2.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1200.0,
                viewport_height_px: 900.0,
                fov_y: Camera::DEFAULT_FOV_Y,
            };
            let frame = globe_frame(&cam);

            // 10 degrees east of centre, which is well inside the visible
            // hemisphere at z2 and well away from the silhouette.
            let east_lon = lon + 10.0;

            // The GLOBE chain, exactly as `shader.wgsl`'s `project_to_sphere`
            // builds it: x = sin(spherical_x) * cos(lat) with
            // `spherical_x = mercator_x * 2PI + PI`, which for a longitude is
            // `sin(lon)`. No flip -- the shader applies none on this side.
            let ndc_globe = {
                let lo = east_lon.to_radians();
                let la = lat.to_radians();
                let p = glam::DVec3::new(lo.sin() * la.cos(), la.sin(), lo.cos() * la.cos());
                let clip = frame.globe_view_proj * p.extend(1.0);
                clip.x / clip.w
            };

            // The MERCATOR chain, with the shader's flip reproduced here --
            // `shader.wgsl`'s `clip.x = -clip.x`, applied to the flat position
            // before the blend. Reproducing it is the whole point: without the
            // negation this assertion would report the pre-flip sign and pass
            // for the wrong reason.
            let ndc_flat = {
                let (x, y) = crate::geo::lon_lat_to_tile_xy(east_lon, lat, 0.0);
                let clip = frame.mercator_view_proj
                    * glam::DVec4::new(x * crate::geo::TILE_SIZE, y * crate::geo::TILE_SIZE, 0.0, 1.0);
                -(clip.x / clip.w)
            };

            assert!(
                ndc_globe > 0.0,
                "on the globe, {east_lon} degrees must be RIGHT of a camera centred at \
                 {lon},{lat}; NDC x was {ndc_globe}"
            );
            assert!(
                ndc_flat > 0.0,
                "on the flat map, {east_lon} degrees must be RIGHT of a camera centred at \
                 {lon},{lat}; NDC x after the shader's flip was {ndc_flat}"
            );
        }
    }

    /// The flip's POSITION in `shader.wgsl`, which the test above cannot see.
    ///
    /// A future edit that moves `clip.x = -clip.x` back below
    /// `interpolate_projection` would leave both chains above unchanged and
    /// mirror the rendered globe. Same text-scan idiom, and same
    /// justification, as `render::gpu`'s
    /// `the_declared_inter_stage_need_matches_the_shader_it_is_declared_for`.
    #[test]
    fn the_mercator_x_flip_stays_on_the_flat_position_and_not_on_the_blend() {
        let whole = include_str!("shader.wgsl");
        // `vs_dot_flat` is FLAT-ONLY (drawn only at `transition == 0`), so
        // it flips and never blends: checked on its own, then cut out of
        // the pairing below.
        let start = whole.find("fn vs_dot_flat(").expect("shader.wgsl has vs_dot_flat");
        let end = start + whole[start..].find("\n}\n").expect("vs_dot_flat is closed");
        let flat_only = &whole[start..end];
        assert_eq!(flat_only.matches("clip.x = -clip.x").count(), 1, "vs_dot_flat flips its Mercator x once");
        assert!(!flat_only.contains("interpolate_projection("), "vs_dot_flat must not blend toward the globe");
        let shader = format!("{}{}", &whole[..start], &whole[end..]);
        // Once per vertex entry that projects: `vertex_main`, `vs_compact`
        // (the compact fill, which mirrors its fill path) and `vs_dot`
        // (the globe dot, which mirrors `vertex_main`'s stroke path).
        let flips: Vec<usize> = shader.match_indices("clip.x = -clip.x").map(|(at, _)| at).collect();
        let blends: Vec<usize> =
            shader.match_indices("out.clip_position = interpolate_projection(").map(|(at, _)| at).collect();
        assert_eq!(flips.len(), 3, "shader.wgsl must flip x exactly once per vertex entry; found {}", flips.len());
        assert_eq!(blends.len(), flips.len(), "one blend per flip");
        let mut marks: Vec<(usize, bool)> =
            flips.iter().map(|&at| (at, true)).chain(blends.iter().map(|&at| (at, false))).collect();
        marks.sort_unstable();
        for pair in marks.chunks(2) {
            let (at_flip, at_blend) = (pair[0], pair[1]);
            assert!(
                at_flip.1 && !at_blend.1,
                "the x-flip at byte {} comes after the blend at {}. It exists to undo \
                 `camera::eye_target_up`'s basis, which `globe_frame` does not use -- applied to the \
                 blended result it mirrors the globe east for west, and every other globe test still \
                 passes",
                at_flip.0,
                at_blend.0
            );
        }
        assert!(
            !shader.contains("out.clip_position.x = -"),
            "the flip must not be re-applied to the blended clip position"
        );
    }

    /// A camera looking at `lon`/`lat` from `zoom`, in **the demo's own
    /// window**: 1024x768 logical pixels, the default in
    /// `examples/common/window.rs`.
    ///
    /// Every tile-cover figure in these tests is viewport-dependent, because
    /// half of `GlobeCover`'s test is the frustum. Naming the viewport that
    /// someone will actually look at the globe through is what makes those
    /// figures checkable against a screenshot rather than merely internally
    /// consistent.
    fn cover_camera(lon: f64, lat: f64, zoom: f64) -> Camera {
        Camera {
            center_lon: lon,
            center_lat: lat,
            viewport_width_px: 1024.0,
            viewport_height_px: 768.0,
            ..camera(zoom)
        }
    }

    /// What `MapView::flat_tiles_for_zoom` selected when these tests were
    /// written, re-derived here so the "before" half of every comparison
    /// below is computed rather than quoted. That function is retired (the
    /// flat arm is now `cover_plan::flat_cover`); this copy stays because
    /// these tests are about the globe fix measured against the cover the
    /// globe replaced at the time.
    fn flat_cover(camera: &Camera, zoom: u8) -> Vec<TileId> {
        let (min_lon, min_lat, max_lon, max_lat) = super::super::pitch_aware_tile_bbox(camera);
        let centre = ((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0);
        let width_px =
            (max_lon - min_lon) / 360.0 * 2f64.powi(i32::from(zoom)) * crate::geo::TILE_SIZE;
        crate::geo::visible_tiles(centre, zoom, (width_px, width_px))
    }

    /// The tiles the flat path actually DRAWS: its cover, then
    /// `tile_in_frustum`, which is what `paint` does to build
    /// `visible_and_culled`. This -- not the cover -- is the number
    /// `GET /map/tiles` reported on the running example, and conflating the
    /// two would have made the diagnosed four tiles look like twelve.
    fn flat_draw_list(camera: &Camera, zoom: u8) -> Vec<TileId> {
        flat_cover(camera, zoom)
            .into_iter()
            .filter(|&t| super::super::tile_in_frustum(t, camera))
            .collect()
    }

    #[test]
    fn the_globe_selects_the_tiles_the_flat_draw_list_left_bare() {
        // THE MEASUREMENT THIS WHOLE TASK EXISTS FOR, at the camera it was
        // taken at. `GET /map/tiles` on the running example at lon -120,
        // lat 25, zoom 2.5 returned EXACTLY four tiles -- z2 x0 y1, x0 y2,
        // x1 y1, x1 y2, a 2x2 patch of a 4x4 world -- and the first thing
        // this asserts is that the flat path really does produce that set,
        // because a fix aimed at a mis-modelled defect is worse than none.
        // (The flat COVER is 12 tiles there; `tile_in_frustum` trims it to
        // these four, which is why the observed number was four.)
        let cam = cover_camera(-120.0, 25.0, 2.5);
        let mut flat = flat_draw_list(&cam, 2);
        flat.sort_by_key(|t| (t.x, t.y));
        assert_eq!(
            flat,
            vec![
                TileId { z: 2, x: 0, y: 1 },
                TileId { z: 2, x: 0, y: 2 },
                TileId { z: 2, x: 1, y: 1 },
                TileId { z: 2, x: 1, y: 2 },
            ],
            "the flat draw list at the diagnosed camera is not the four tiles that were observed"
        );

        // The sphere at that same camera needs NINE, because its visible cap
        // is 128.6 degrees of arc across. Five of them -- the whole northern
        // row, and a column on the far side of the antimeridian -- were never
        // fetched, and rendered as background.
        let mut globe =
            GlobeCover::new(&globe_frame(&cam)).cover(2).expect("the globe is on at z2.5");
        globe.sort_by_key(|t| (t.x, t.y));
        assert_eq!(
            globe,
            vec![
                TileId { z: 2, x: 0, y: 0 },
                TileId { z: 2, x: 0, y: 1 },
                TileId { z: 2, x: 0, y: 2 },
                TileId { z: 2, x: 1, y: 0 },
                TileId { z: 2, x: 1, y: 1 },
                TileId { z: 2, x: 1, y: 2 },
                TileId { z: 2, x: 3, y: 0 },
                TileId { z: 2, x: 3, y: 1 },
                TileId { z: 2, x: 3, y: 2 },
            ],
            "the globe cover at the diagnosed camera"
        );
        // `x = 3` is lon 90..180, and it is in the cover because the camera
        // sits at lon -120 with a 64.32-degree cap radius: the cap crosses
        // the antimeridian. How far past depends on latitude, and NOT the way
        // a first guess suggests -- at the camera's own lat 25 the cap spans
        // 71.93 degrees of longitude either side and so reaches lon -191.93,
        // which wraps to **168.07**; its furthest western reach is lon
        // -203.93, wrapping to **156.07**, and that is at **lat 77.23**, not
        // at lat 25. (An earlier version of this comment used the lat-25
        // camera's latitude with the tangent-meridian formula, which is the
        // extreme over the WHOLE cap, and asserted lon 160 was visible at
        // lat 25. It is not: 160 is west of 168.07. The assertions below are
        // what caught that, which is the reason they are assertions and not
        // arithmetic in a comment.)
        //
        // A cover that clamped at the antimeridian instead of wrapping would
        // leave a bare wedge exactly there -- the defect
        // `distanceToTileWrapX` exists to avoid in the reference
        // (`globe_covering_tiles_details_provider.ts:27-43`).
        let frame = globe_frame(&cam);
        let n = glam::DVec3::new(frame.clipping_plane[0], frame.clipping_plane[1], frame.clipping_plane[2]);
        let in_front = |lon: f64, lat: f64| {
            let lat_rad: f64 = lat.to_radians();
            let merc_y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) / 2.0;
            n.dot(mercator_to_sphere((lon + 180.0) / 360.0, merc_y)) + frame.clipping_plane[3] > 0.0
        };
        assert!(in_front(170.0, 25.0), "at lat 25 the cap reaches lon 168.07, so 170 is visible");
        assert!(!in_front(165.0, 25.0), "and 165 is west of that edge");
        assert!(in_front(160.0, 77.23), "the cap's furthest west, lon 156.07, is at lat 77.23");
        assert!(!in_front(160.0, 25.0), "which is NOT reached at the camera's own latitude");

        for tile in &flat {
            assert!(globe.contains(tile), "{tile:?} was drawn flat and must still be drawn");
        }

        // The same claim where it is largest. At lon 0, lat 0, zoom 3 the
        // whole sphere is on screen -- a 758px disc in a 1024x768 window --
        // and the globe needs SIXTEEN z3 tiles against the flat draw list's
        // eight. The sphere shows twice what the flat viewport does.
        let wide = cover_camera(0.0, 0.0, 3.0);
        assert_eq!(flat_draw_list(&wide, 3).len(), 8, "the flat draw list at lon 0 lat 0 z3");
        assert_eq!(
            GlobeCover::new(&globe_frame(&wide)).cover(3).expect("the globe is on at z3").len(),
            16,
            "the globe cover at lon 0 lat 0 z3"
        );
    }

    #[test]
    fn at_the_equator_at_z2_point_5_the_flat_path_was_already_right() {
        // **A deliberately unflattering test.** At lon 0, lat 0, zoom 2.5 the
        // flat draw list and the globe cover are the SAME four tiles, and
        // recording that stops this task being credited with a fix it did not
        // make at a camera someone may well check it at.
        //
        // The reason is arithmetic rather than luck: the flat viewport at
        // z2.5 spans 127.3 degrees of longitude and +/-43.1 of latitude, the
        // sphere's visible cap spans 131.7 degrees of arc, and at the equator
        // both fall inside the same two tile columns and two tile rows. Move
        // the camera off the equator -- to lat 25, where the defect was
        // diagnosed -- and Mercator's own asymmetry breaks the coincidence:
        // four tiles against nine.
        let cam = cover_camera(0.0, 0.0, 2.5);
        let mut flat = flat_draw_list(&cam, 2);
        let mut globe = GlobeCover::new(&globe_frame(&cam)).cover(2).expect("the globe is on");
        flat.sort_by_key(|t| (t.x, t.y));
        globe.sort_by_key(|t| (t.x, t.y));
        assert_eq!(globe, flat, "at this one camera the two agree");
        assert_eq!(globe.len(), 4);
    }

    #[test]
    fn the_far_side_of_the_sphere_is_not_selected() {
        // The two outer tile columns at z2 span lon -180..-90 and 90..180.
        // Their nearest point to a lon-0 camera's look-at is 90 degrees away,
        // outside the 65.8-degree radius of the visible cap at this zoom, so
        // every point of them is on the hemisphere facing away. Under
        // perspective those points project OUTSIDE the silhouette disc, not
        // inside it, which is how a far-side tile becomes something sticking
        // out past the planet's edge.
        let cover = GlobeCover::new(&globe_frame(&cover_camera(0.0, 0.0, 2.5)));
        for y in 0..4 {
            for x in [0, 3] {
                assert!(
                    !cover.tile_visible(TileId { z: 2, x, y }),
                    "z2 {x},{y} is on the far side and must not be drawn"
                );
            }
        }
        // The tile holding the antipode of the camera's look-at, at every
        // level the globe band reaches. Not z1: at z1 both tiles span 180
        // degrees of longitude and each one touches the camera's own
        // meridian, so at that level there IS no wholly-far-side tile -- the
        // first assertion this test made claimed otherwise and was wrong.
        for z in 2..=5u8 {
            let n = 1u32 << z;
            let antipode = TileId { z, x: n - 1, y: n / 2 };
            assert!(
                !cover.tile_visible(antipode),
                "the antipodal tile {antipode:?} must not be drawn"
            );
        }
    }

    #[test]
    fn the_cull_drops_no_tile_with_a_point_on_screen_and_in_front_of_the_horizon() {
        // The half of far-side culling that is NOT the easy half: a test that
        // only checked "the far side is dropped" is passed by a cull that
        // drops everything. What must not happen is a tile with real drawn
        // area being dropped, and that is checked by brute force -- a 17x17
        // grid of sample points per tile, each put through the same two tests
        // the GPU applies: in front of the clipping plane (`shader.wgsl`'s
        // `globe_clipping_z`, whose `z > w` is what clips the far side) and
        // inside the clip volume.
        //
        // Both conditions are needed. An earlier version of this test checked
        // only the plane, and failed on `z3 x0 y0` at a lon 13.4 lat 52.5
        // camera -- a tile whose corner at lon -180, lat 85.05 really is 42.3
        // degrees from the look-at and so in front of a 51.9-degree horizon,
        // and which the FRUSTUM culls because it is nowhere near the screen.
        // That was the test being wrong, not the cull.
        for (lon, lat, zoom) in [(0.0, 0.0, 2.5), (-120.0, 25.0, 2.5), (13.4, 52.5, 3.0), (151.2, -33.9, 4.5)] {
            let frame = globe_frame(&cover_camera(lon, lat, zoom));
            let cover = GlobeCover::new(&frame);
            let z = zoom.floor() as u8;
            let n = 1u32 << z;
            let plane = glam::DVec3::new(frame.clipping_plane[0], frame.clipping_plane[1], frame.clipping_plane[2]);
            let span = 1.0 / f64::from(n);
            for x in 0..n {
                for y in 0..n {
                    let tile = TileId { z, x, y };
                    if cover.tile_visible(tile) {
                        continue;
                    }
                    for i in 0..=16 {
                        for j in 0..=16 {
                            let p = mercator_to_sphere(
                                (f64::from(x) + f64::from(i) / 16.0) * span,
                                (f64::from(y) + f64::from(j) / 16.0) * span,
                            );
                            if plane.dot(p) + frame.clipping_plane[3] < 0.0 {
                                continue; // behind the horizon: the GPU clips it too
                            }
                            let clip = frame.globe_view_proj * p.extend(1.0);
                            let on_screen = clip.w > 0.0
                                && clip.x.abs() <= clip.w
                                && clip.y.abs() <= clip.w
                                && clip.z >= 0.0;
                            assert!(
                                !on_screen,
                                "{tile:?} was culled at lon {lon} lat {lat} z {zoom}, but its \
                                 point at ({i}/16, {j}/16) is on screen and in front of the horizon"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_analytic_rectangle_maximum_matches_a_brute_force_scan() {
        // `GlobeCover`'s horizon test is exact rather than sampled: the
        // maximum of `dot(n, p)` over a tile's lon/lat rectangle is attained
        // either at a latitude edge or at the one interior critical point,
        // and which one is decided by the sign of the derivative at the two
        // edges. Get that wrong and the cull is either loose (the far side
        // comes back) or tight (a visible tile vanishes), and neither shows
        // up as anything but "the globe looks odd". So: against a dense scan.
        let frame = globe_frame(&cover_camera(13.4, 52.5, 2.5));
        let cover = GlobeCover::new(&frame);
        let plane = glam::DVec3::new(frame.clipping_plane[0], frame.clipping_plane[1], frame.clipping_plane[2]);
        let mut worst = 0.0_f64;
        for z in 1..=4u8 {
            let n = 1u32 << z;
            let span = 1.0 / f64::from(n);
            for x in 0..n {
                for y in 0..n {
                    let analytic = cover.max_dot_over_tile(TileId { z, x, y });
                    let mut sampled = f64::NEG_INFINITY;
                    for i in 0..=64 {
                        for j in 0..=64 {
                            let p = mercator_to_sphere(
                                (f64::from(x) + f64::from(i) / 64.0) * span,
                                (f64::from(y) + f64::from(j) / 64.0) * span,
                            );
                            sampled = sampled.max(plane.dot(p));
                        }
                    }
                    // A pole-row tile's region includes the POLE and the cap
                    // between it and the tile's own edge, both OUTSIDE the
                    // lon/lat rectangle scanned above:
                    // `render::subdivide::append_pole_caps` grows geometry
                    // there and `max_dot_over_tile` extends its latitude
                    // interval to match. The scan has to follow, or the
                    // analytic maximum legitimately exceeds every sample and
                    // this test fails for the one reason that is correct.
                    if y == 0 {
                        sampled = sampled.max(plane.dot(glam::DVec3::new(0.0, 1.0, 0.0)));
                    }
                    if y == n - 1 {
                        sampled = sampled.max(plane.dot(glam::DVec3::new(0.0, -1.0, 0.0)));
                    }
                    // The cap's interior, not just its apex: a straight scan
                    // in latitude from the tile's edge to the pole, at every
                    // longitude the scan already visits.
                    if y == 0 || y == n - 1 {
                        let edge_y = if y == 0 { 0.0 } else { 1.0 };
                        let (edge_sin, edge_cos) = mercator_y_to_sin_cos_lat(edge_y);
                        let edge_lat = edge_sin.atan2(edge_cos);
                        let pole_lat = if y == 0 { PI / 2.0 } else { -PI / 2.0 };
                        for i in 0..=64 {
                            let sx = ((f64::from(x) + f64::from(i) / 64.0) * span)
                                * std::f64::consts::TAU
                                + PI;
                            for j in 1..=64 {
                                let lat = edge_lat
                                    + (pole_lat - edge_lat) * f64::from(j) / 64.0;
                                let p = glam::DVec3::new(
                                    sx.sin() * lat.cos(),
                                    lat.sin(),
                                    sx.cos() * lat.cos(),
                                );
                                sampled = sampled.max(plane.dot(p));
                            }
                        }
                    }
                    // The scan can only ever find a value at or below the
                    // true maximum, so the analytic figure must be >= it, and
                    // by no more than the scan's own resolution.
                    assert!(
                        analytic >= sampled - 1e-12,
                        "z{z} {x},{y}: analytic {analytic} is BELOW the sampled {sampled} -- \
                         the cull would drop visible geometry"
                    );
                    worst = worst.max(analytic - sampled);
                }
            }
        }
        // The other direction is bounded too, and that is what says the
        // analytic maximum is the real maximum rather than merely an upper
        // bound: over 65x65 samples per tile the largest gap between the
        // closed form and the best sample is **9.13e-5** (measured), which is
        // the curvature over one sample step and nothing more. A formula that
        // was loose -- a bounding cap, say -- would sit orders of magnitude
        // above its own samples here.
        assert!(worst < 1.2e-4, "analytic exceeded the densest sample by {worst}");
    }

    #[test]
    fn the_descent_finds_exactly_what_a_full_level_scan_would() {
        // MapLibre reaches one level through a depth-first descent from a
        // single root tile (`covering_tiles.ts:248-322`, `:260` pushing the
        // one root because `globe_covering_tiles_details_provider.ts:104`
        // refuses world copies), and that is only equivalent to scanning the
        // level if a parent is never culled while a child is visible. It is
        // not: the horizon maximum over a parent's rectangle is the maximum
        // over the union of its children's, and the parent's bounding ball
        // contains the children's caps. This pins that, because a descent
        // that prunes one level too eagerly removes a QUARTER OF THE SPHERE
        // and still looks like a plausible globe.
        for (lon, lat, zoom) in [(0.0, 0.0, 2.5), (-120.0, 25.0, 2.5), (100.0, -60.0, 3.0), (179.0, 5.0, 3.0)] {
            let cover = GlobeCover::new(&globe_frame(&cover_camera(lon, lat, zoom)));
            for target in 0..=3u8 {
                let n = 1u32 << target;
                let mut scanned: Vec<TileId> = Vec::new();
                for x in 0..n {
                    for y in 0..n {
                        let tile = TileId { z: target, x, y };
                        if cover.tile_visible(tile) {
                            scanned.push(tile);
                        }
                    }
                }
                let mut descended = cover.cover(target).expect("the globe is on");
                descended.sort_by_key(|t| (t.x, t.y));
                scanned.sort_by_key(|t| (t.x, t.y));
                assert_eq!(
                    descended, scanned,
                    "the descent and a full z{target} scan disagree at lon {lon} lat {lat} z {zoom}"
                );
            }
        }
    }

    #[test]
    fn the_globe_cover_is_ordered_nearest_the_camera_first() {
        // MapLibre sorts its result by distance from the centre point
        // (`covering_tiles.ts:309,324`) so the tiles the user is looking at
        // are requested first. Ours sorts by how far in front of the horizon
        // plane the tile reaches, which is the same ordering at zero pitch
        // and a better one under pitch -- see `GlobeCover::cover`.
        let cover = GlobeCover::new(&globe_frame(&cover_camera(0.0, 0.0, 2.5)));
        let tiles = cover.cover(3).expect("the globe is on at z2.5");
        let scores: Vec<f64> = tiles.iter().map(|&t| cover.max_dot_over_tile(t)).collect();
        for pair in scores.windows(2) {
            assert!(pair[0] >= pair[1] - 1e-12, "cover order is not nearest-first: {scores:?}");
        }
        // Deterministic: the same camera must give the same list, or a
        // frame-to-frame diff of the cover would churn the fetch queue.
        assert_eq!(Some(&tiles), cover.cover(3).as_ref());
    }

    #[test]
    fn the_cover_size_across_the_transition_band_is_what_was_measured() {
        // The tile count is the memory question, so it is pinned rather than
        // described. Measured at lon 0, lat 0 in the demo's own 1024x768
        // window (`examples/common/window.rs`), at every integer zoom the
        // globe band reaches -- `transition_for_zoom` is exactly 0 from
        // GLOBE_ZOOM_MAX = 6, so z5 is the last one:
        //
        //   camera zoom  level  globe cover  flat draw list  full level
        //   0                0            1               1           1
        //   1                1            4               4           4
        //   2                2            8               6          16
        //   3                3           16               8          64
        //   4                4           24               8         256
        //   5                5           24               8       1,024
        //
        // The cover PEAKS at 24 and then stops growing: a level up quadruples
        // the tiles in the world, but the camera's own footprint on the
        // sphere shrinks by the same factor. That is the whole memory answer
        // for the band -- 24 tiles against `MAX_VISIBLE_TILES` = 512, which
        // the flat path already lives with.
        for (zoom, expected) in [(0.0, 1), (1.0, 4), (2.0, 8), (3.0, 16), (4.0, 24), (5.0, 24)] {
            let cam = cover_camera(0.0, 0.0, zoom);
            let cover = GlobeCover::new(&globe_frame(&cam));
            assert!(cover.is_globe(), "z{zoom} is inside the globe band");
            assert_eq!(
                cover.cover(zoom as u8).map_or(0, |c| c.len()),
                expected,
                "cover size at z{zoom} moved -- re-measure the memory note in Task 7"
            );
        }
    }

    #[test]
    fn the_worst_cover_in_the_band_is_smaller_than_the_flat_path_asks_for() {
        // Swept over lon in 60-degree steps, lat at -80/-45/0/25/45/80, pitch
        // at 0/30/60 degrees and zoom in 0.25 steps across the whole band,
        // the LARGEST globe cover anywhere is **56 tiles**, at lon -180,
        // lat 80, pitch 30, zoom 5. That is the figure to hold against a
        // memory budget, and this test is what stops it drifting silently.
        //
        // **It was 52, at lon -120, lat 80, pitch 60, zoom 5, and the four
        // extra tiles are Task 5's pole caps.** `max_dot_over_tile` and the
        // bounding ball in `visibility` now extend a pole-row tile's region
        // to the pole itself, because `render::subdivide::append_pole_caps`
        // grows geometry there -- MapLibre's own `:165-170`, restored. The
        // extension only ever loosens the test, and only for the north and
        // south tile rows, so it can only ADD tiles, and only at a camera
        // looking near a pole: at lat 80 and z5 that is four of them, and
        // the old camera now reads 54. A cover that had SHRUNK anywhere
        // would be a bug in the extension, which is why both cameras are
        // pinned below rather than just the new worst.
        //
        // The comparison with the flat path is per-camera and is NOT
        // uniformly in the globe's favour, which is worth recording rather
        // than quoting only the flattering half:
        //
        //   camera                                globe cover  flat cover
        //   lon -180 lat 80 pitch 30 z5 (worst)           56          28
        //   lon -120 lat 80 pitch 60 z5                   54       1,024
        //
        // At 60 degrees of pitch `pitch_aware_tile_bbox` grows a
        // bearing-invariant circular footprint so large that the flat cover
        // is the ENTIRE z5 level, which `tiles_for_zoom` then truncates to
        // `MAX_VISIBLE_TILES` = 512 -- so there the globe asks for one
        // nineteenth of what the flat path asks for. At 30 degrees the flat
        // footprint is a modest 28 tiles and the globe legitimately asks for
        // twice it, because a sphere really does show its whole visible cap
        // plus two pole caps while a flat viewport at lat 80 shows a band.
        // Either way 56 is the number to hold against a budget, and against
        // `MAX_VISIBLE_TILES` = 512 it is not a budget question.
        let mut worst = cover_camera(-180.0, 80.0, 5.0);
        worst.pitch = 30f32.to_radians();
        assert_eq!(
            GlobeCover::new(&globe_frame(&worst)).cover(5).map_or(0, |c| c.len()),
            56,
            "the worst globe cover in the band"
        );
        assert_eq!(flat_cover(&worst, 5).len(), 28, "the flat cover at the same camera");
        let mut cam = cover_camera(-120.0, 80.0, 5.0);
        cam.pitch = 60f32.to_radians();
        assert_eq!(
            GlobeCover::new(&globe_frame(&cam)).cover(5).map_or(0, |c| c.len()),
            54,
            "the camera that was the worst before the pole caps existed"
        );
        assert_eq!(flat_cover(&cam, 5).len(), 1024, "the flat cover at that camera");
        // The next-zoom prefetch is the one caller that asks for a level
        // ABOVE the band's own ceiling: it fires past `zoom.fract() > 0.7`,
        // so at z5.8 (transition still 0.0033) it asks for level 6. That is
        // 4,096 tiles in the world and the cover is 16 of them, 24 under 60
        // degrees of pitch -- the descent never enumerates the level.
        let mut top = cover_camera(0.0, 0.0, 5.8);
        assert_eq!(GlobeCover::new(&globe_frame(&top)).cover(6).map_or(0, |c| c.len()), 16);
        top.pitch = 60f32.to_radians();
        assert_eq!(GlobeCover::new(&globe_frame(&top)).cover(6).map_or(0, |c| c.len()), 24);
    }

    #[test]
    fn the_globe_is_structurally_off_once_the_transition_reaches_zero() {
        // The safety property the whole task rests on: above the transition
        // band there is no globe cover to get wrong, because `GlobeCover` IS
        // `FLAT` there, by value. MapLibre draws the same hard line --
        // `isGlobeRendering` is `_globeness > 0` (`globe_transform.ts:230`)
        // and `currentTransform` switches the WHOLE transform on it
        // (`:241-243`), with Mercator's own `getClippingPlane` returning null
        // (`mercator_transform.ts:326-328`).
        //
        // The ceiling is `PROJECTION_BLEND_ZOOM_MAX`, not `GLOBE_ZOOM_MAX`:
        // the space effects stop at z6 but the PROJECTION is still the
        // sphere there, and must be -- see `PROJECTION_BLEND_ZOOM_MIN` for
        // the measurements that separated the two.
        for zoom in [PROJECTION_BLEND_ZOOM_MAX, PROJECTION_BLEND_ZOOM_MAX + 0.001, 14.0, 18.0] {
            let cover = GlobeCover::new(&globe_frame(&cover_camera(13.4, 52.5, zoom)));
            assert_eq!(cover, GlobeCover::FLAT, "z{zoom} must produce the flat cover");
            assert!(!cover.is_globe(), "z{zoom} is not globe-rendering");
        }
        // And just inside the band it is not flat, or the band would be dead.
        let inside = GlobeCover::new(&globe_frame(&cover_camera(
            13.4,
            52.5,
            PROJECTION_BLEND_ZOOM_MAX - 0.001,
        )));
        assert!(inside.is_globe());
        assert_ne!(inside, GlobeCover::FLAT);
    }

    #[test]
    fn globe_radius_world_matches_maplibres_formula_and_survives_the_poles() {
        assert!((globe_radius_world(&camera(0.0)) * camera(0.0).center_lat.to_radians().cos()
            - WORLD_SIZE as f64 / (2.0 * PI))
            .abs()
            < 1e-9);
        // Latitude beyond the Mercator limit must not divide by ~zero.
        let mut polar = camera(0.0);
        polar.center_lat = 90.0;
        assert!(globe_radius_world(&polar).is_finite());
        assert!(globe_radius_world(&polar) > 0.0);
        // Zoom-independent: world units, not pixels.
        assert_eq!(globe_radius_world(&camera(0.0)), globe_radius_world(&camera(17.0)));
    }

    /// The bug this crate shipped: `MIN_ZOOM` used to be `1.0` per shell
    /// (web/iOS) or `0.0` (native), and neither number was ever checked
    /// against whether the globe actually fits on screen at it -- see the
    /// bug report this fixed: "in some positions I cannot zoom out from
    /// the Earth", a tilted globe filling a phone screen at z1.
    /// `crate::camera_control::MIN_ZOOM` (now `-2.0`, matching MapLibre GL
    /// JS's own default, `defaultMinZoom = -2` at `src/ui/map.ts:482` in
    /// `maplibre-gl-js` (github.com/maplibre/maplibre-gl-js), BSD-3, Copyright (c) 2023 MapLibre
    /// contributors) is checked here against THIS module's own
    /// globe-camera maths, not asserted by fiat: the sphere's on-screen
    /// diameter, at pitch 0, must clear the narrowest window this crate
    /// ships to (a 430-logical-px-wide phone in portrait) with margin to
    /// spare, even centred at the Mercator limit -- the worst case, where
    /// `globe_radius_world`'s `1/cos(lat)` term has grown the sphere to
    /// ~11.5x its equatorial size.
    ///
    /// Apparent-radius formula: a sphere of world-radius `R` centred `R +
    /// d` from the eye (`globe_frame`'s own `-d` then `-radius` translation
    /// chain) subtends a half-angle `asin(R / (R + d))`, and a ray at angle
    /// `theta` off the view axis lands `tan(theta) / tan(halfFovY)` of a
    /// HALF-viewport away in NDC on either axis -- `directx::perspective`
    /// scales both screen axes through `viewport_height_px` alone (aspect
    /// only changes how the horizontal NDC unit maps back to
    /// `viewport_width_px`, and the two cancel), which is exactly why this
    /// crate's globe silhouette is a true circle in screen PIXELS, not an
    /// ellipse, and why only `viewport_height_px` enters `Camera::distance`.
    #[test]
    fn the_minimum_zoom_globe_fits_the_phone_viewport() {
        fn screen_diameter_px(camera: &Camera) -> f64 {
            let radius = globe_radius_world(camera);
            let d = camera.distance();
            let half_fov = super::super::camera::FOV_Y_RADIANS_F64 / 2.0;
            let theta = (radius / (radius + d)).clamp(-1.0, 1.0).asin();
            2.0 * (theta.tan() / half_fov.tan()) * camera.viewport_height_px as f64 / 2.0
        }

        const PHONE: (f32, f32) = (430.0, 932.0);
        const DESKTOP: (f32, f32) = (1440.0, 900.0);
        let poles = [0.0, 60.0, crate::camera_control::MAX_MERCATOR_LAT, -crate::camera_control::MAX_MERCATOR_LAT];
        for (label, (w, h)) in [("phone", PHONE), ("desktop", DESKTOP)] {
            for lat in poles {
                let cam = Camera {
                    center_lon: 0.0,
                    center_lat: lat,
                    zoom: crate::camera_control::MIN_ZOOM,
                    pitch: 0.0,
                    bearing: 0.0,
                    viewport_width_px: w,
                    viewport_height_px: h,
                    fov_y: Camera::DEFAULT_FOV_Y,
                };
                let diam = screen_diameter_px(&cam);
                let smaller_dimension = f64::from(w.min(h));
                assert!(
                    diam < smaller_dimension * 0.95,
                    "{label} lat={lat}: globe diameter {diam:.1}px must fit inside the \
                     {smaller_dimension}px viewport with margin at the shared MIN_ZOOM, got {diam:.1}px"
                );
            }
        }
    }
}

/// Where `world` is drawn on `camera`'s globe frame: for code outside this
/// module, which cannot build a `GlobeFrame` itself (`map_view.rs`'s
/// `globe_frame(` call-site count covers its test module) -- tests, and
/// `camera_control`'s pinned-drag solve.
pub(crate) fn drawn_pixel_on_globe(camera: &Camera, world: (f64, f64)) -> Option<(f32, f32)> {
    project_world_to_screen_on_frame(camera, &GlobeAnchors::new(&build_globe_frame(camera)), world)
}

#[cfg(test)]
mod pick_tests {
    use super::*;

    fn distance(a: (f32, f32), b: (f32, f32)) -> f64 {
        (f64::from(a.0 - b.0)).hypot(f64::from(a.1 - b.1))
    }

    /// **The hover pick lands on what is drawn under the cursor, on the
    /// globe.** Reported as "the highlighted block group is about 1 cm from
    /// the cursor at globe zoom": the pick used the flat Mercator ground
    /// plane while the frame was drawn on the sphere. Measured before the
    /// fix on this camera (1400x900 logical, over the US): mean / worst
    /// 140 / 450 px at z3, 76 / 272 px at z4, 39 / 151 px at z5, 20 / 80 px
    /// at z6, 5 / 21 px at z8, still 0.2 / 0.9 px at z11.5 in the blend.
    ///
    /// Round trip both ways, over a grid covering the whole viewport,
    /// through the renderer's own forward projection
    /// (`project_world_to_screen_on_frame`, what the puck and route labels
    /// are placed with): a pixel's picked world point is drawn back at that
    /// pixel, and a drawn point is picked back as itself.
    #[test]
    fn the_pick_round_trips_the_drawn_projection_across_the_globe() {
        for (pitch, bearing) in [(0.0f32, 0.0f32), (0.6, 0.0), (0.9, 2.2)] {
            for zoom in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 11.2, 11.5, 11.8, 12.5] {
                let camera = Camera {
                    center_lon: -98.0,
                    center_lat: 39.0,
                    zoom,
                    pitch,
                    bearing,
                    viewport_width_px: 1400.0,
                    viewport_height_px: 900.0,
                    fov_y: Camera::DEFAULT_FOV_Y,
                };
                let anchors = GlobeAnchors::new(&globe_frame(&camera));
                let (mut picked, mut worst) = (0, 0f64);
                for gy in 0..=18 {
                    for gx in 0..=28 {
                        let pixel = (gx as f32 * 50.0, gy as f32 * 50.0);
                        let Some(world) = unproject_to_world_on_globe(&camera, true, pixel.0, pixel.1) else {
                            continue;
                        };
                        // The pole caps: past the Mercator limit there is
                        // no world point to pick, and it is clamped.
                        if world.y < 1e-3 || world.y > f64::from(WORLD_SIZE) - 1e-3 {
                            continue;
                        }
                        let drawn = project_world_to_screen_on_frame(&camera, &anchors, (world.x, world.y))
                            .expect("a picked point is on the visible side");
                        worst = worst.max(distance(drawn, pixel));
                        picked += 1;
                        // And back: the drawn point picks itself.
                        let again = unproject_to_world_on_globe(&camera, true, drawn.0, drawn.1).unwrap();
                        let redrawn =
                            project_world_to_screen_on_frame(&camera, &anchors, (again.x, again.y)).unwrap();
                        worst = worst.max(distance(redrawn, drawn));
                    }
                }
                assert!(picked > 20, "z{zoom} pitch {pitch}: only {picked} pixels picked anything");
                assert!(worst < 0.5, "z{zoom} pitch {pitch} bearing {bearing}: worst round trip {worst:.2} px");
            }
        }
    }

    /// **An unconverged pick is no pick.** Inside the z11-z12 blend the
    /// pick is Newton-refined, and at the limb of a steeply pitched frame
    /// (and in the sky above it) the iteration can stall; a stalled iterate
    /// must come back `None`, never as a point drawn somewhere else. Swept
    /// over narrow, wide and tiny viewports at full pitch, every `Some`
    /// lands on its pixel.
    #[test]
    fn a_pick_in_the_blend_either_lands_on_its_pixel_or_is_none() {
        for (w, h) in [(3000.0f32, 260.0f32), (240.0, 2200.0), (64.0, 48.0), (1400.0, 900.0)] {
            for zoom in [11.05, 11.5, 11.95] {
                for (lat, bearing) in [(39.0, 0.0f32), (84.0, 1.3), (-70.0, 3.0)] {
                    let camera = Camera {
                        center_lon: 179.9,
                        center_lat: lat,
                        zoom,
                        pitch: std::f32::consts::FRAC_PI_3,
                        bearing,
                        viewport_width_px: w,
                        viewport_height_px: h,
                        fov_y: Camera::DEFAULT_FOV_Y,
                    };
                    let anchors = GlobeAnchors::new(&globe_frame(&camera));
                    let (mut some, mut none) = (0, 0);
                    for gy in 0..=24 {
                        for gx in 0..=24 {
                            let pixel = (w * gx as f32 / 24.0, h * gy as f32 / 24.0);
                            match unproject_to_world_on_globe(&camera, true, pixel.0, pixel.1) {
                                Some(world) => {
                                    some += 1;
                                    let drawn = project_world_to_screen_on_frame(&camera, &anchors, (world.x, world.y))
                                        .expect("a pick is drawn");
                                    let off = distance(drawn, pixel);
                                    assert!(off <= 0.5, "{w}x{h} z{zoom} lat {lat}: {pixel:?} picked a point drawn {off:.2} px away");
                                }
                                None => none += 1,
                            }
                        }
                    }
                    assert!(some > 0, "{w}x{h} z{zoom} lat {lat}: nothing picked at all ({none} none)");
                }
            }
        }
    }

    /// Off the planet there is nothing to hover: a pixel in space picks
    /// nothing (the flat plane under it used to answer with a feature), and
    /// with the globe switched off the pick is exactly the flat one.
    #[test]
    fn space_picks_nothing_and_a_flat_map_picks_the_ground_plane() {
        let camera = Camera {
            center_lon: -98.0,
            center_lat: 39.0,
            zoom: 2.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1400.0,
            viewport_height_px: 900.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        assert!(unproject_to_world_on_globe(&camera, true, 700.0, 450.0).is_some());
        assert!(unproject_to_world_on_globe(&camera, true, 5.0, 5.0).is_none(), "the corner is space at z2");
        let flat = super::super::unproject_to_ground_f64(&camera, 5.0, 5.0).unwrap().truncate();
        assert_eq!(unproject_to_world_on_globe(&camera, false, 5.0, 5.0), Some(flat));
    }
}
