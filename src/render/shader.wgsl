struct Camera {
    view_proj: mat4x4<f32>,
    zoom: f32,
    /// Physical pixels per logical pixel. Drives the stroke's edge
    /// antialiasing band, which must be one PHYSICAL pixel wide however
    /// dense the display is -- MapLibre's `u_device_pixel_ratio`, used the
    /// same way.
    device_pixel_ratio: f32,
    /// The horizon fade's band, `(start, end)`, in view-space DEPTH -- the
    /// clip `w` of a vertex, world units along the view direction. `end ==
    /// 0.0` is OFF, and is what `HorizonFog::NONE` writes: below 50 degrees
    /// of pitch, on every globe frame, and whenever the cover was not cut.
    /// Occupies the 8 bytes that were `_pad`, so nothing above it moves.
    fog_range: vec2<f32>,
    /// What the fade blends toward: the frame's clear colour, so faded
    /// ground meets the uncovered background seamlessly. At offset 80.
    fog_color: vec4<f32>,
    /// `.x`: the camera's eye-to-centre distance in world units -- the clip
    /// `w` of the point under the screen's centre, on a flat frame and a
    /// globe frame alike (MapLibre's `u_camera_to_center_distance`). Read
    /// only by `vs_circle`, for `circle-pitch-scale: viewport`; zero (never
    /// written: `render::gpu::TilePipeline::write_circle_frame`) draws that
    /// scale as `map`. At offset 96; `render::gpu::CameraUniform`.
    circle_frame: vec4<f32>,
}

struct Model {
    matrix: mat4x4<f32>,
    /// Unit-sphere position -> clip space, i.e. `render::globe::GlobeFrame`'s
    /// `globe_view_proj`. MapLibre's `u_projection_matrix` in globe mode
    /// (`_projection_globe.vertex.glsl:106`).
    globe_matrix: mat4x4<f32>,
    /// The tile's origin and span in [0,1] Mercator space, `(x, y, w, h)`:
    /// `render::globe::tile_mercator_coords`. MapLibre's
    /// `u_projection_tile_mercator_coords` (`:3`).
    tile_mercator_coords: vec4<f32>,
    /// The horizon plane in unit-sphere space, `xyz` normal and `w` offset:
    /// `render::globe::GlobeFrame`'s `clipping_plane`. MapLibre's
    /// `u_projection_clipping_plane` (`:4`).
    clipping_plane: vec4<f32>,
    blend_t: f32,
    /// Pixels per lyon input unit at the camera's FLOORED zoom, from
    /// `render::camera::dash_scale`. Turns a vertex's `line_distance` (in
    /// lyon units, i.e. MVT coords x1000, and deliberately zoom-independent)
    /// into a distance in screen pixels for the dash phase.
    ///
    /// Floored, not continuous, to match MapLibre's `u_tileratio` (derived
    /// from `transform.tileZoom == floor(zoom)`): the pattern is pinned in
    /// tile units for a whole zoom level, so dashes stretch up to 2x and
    /// snap back at the boundary. Affects the dash period ONLY -- the
    /// extrusion below still uses the continuously blended half-width.
    dash_scale: f32,
    /// Globeness: 0.0 = pure Mercator, 1.0 = pure globe.
    /// `render::globe::transition_for_zoom`. MapLibre's
    /// `u_projection_transition` (`:5`).
    transition: f32,
    /// Multiplies a vertex's `position` from lyon units into the [0,1] in-tile
    /// space `project_to_sphere` wants: `1 / (COORD_SCALE * extent)`. See
    /// `render::gpu::GlobeParams::lyon_to_tile_unit` for why this is a
    /// separate scalar rather than folded into `tile_mercator_coords.zw` the
    /// way MapLibre folds it.
    lyon_to_tile_unit: f32,
    /// [0,1] in-tile units per logical pixel at the CONTINUOUS camera zoom --
    /// the scale the globe's tile-space stroke extrusion is measured in.
    tile_units_per_px: f32,
    /// UNIT direction from the planet's centre toward the sun, in the same
    /// sphere space `project_to_sphere` returns:
    /// `render::atmosphere::Sun::direction_sphere`.
    ///
    /// Read ONLY inside the `model.transition > 0.0` branch, so a flat
    /// frame never touches it -- `render::GlobeParams::FLAT` leaves it zero
    /// and that zero is never dotted with anything.
    ///
    /// WGSL gives `vec3<f32>` a 16-byte alignment, so this lands at offset
    /// 192 and `night_brightness` at 204, for a 208-byte struct.
    /// `render::gpu::ModelUniform`'s `_pad` is what puts it there, and
    /// `the_model_uniform_is_16_byte_aligned_and_its_declared_need_matches_reality`
    /// is what checks it.
    sun_direction: vec3<f32>,
    /// How bright the night side of the planet is, as a multiplier on the
    /// fill colour: `0.0` is black, `1.0` is no shading at all.
    /// `render::atmosphere::Atmosphere::night_brightness`.
    night_brightness: f32,
    /// **The box of this tile's own coordinate space this draw may paint**,
    /// `(min_x, min_y, max_x, max_y)` in the SAME units `VertexInput`'s
    /// `position` carries — MVT coordinates times `mesh::COORD_SCALE`, not
    /// the `[0, 1]` in-tile coordinate `pos_in_tile` below is in.
    ///
    /// Lyon units rather than in-tile units so that this works on a FLAT
    /// frame too: `lyon_to_tile_unit` is zero under `GlobeParams::FLAT`, so
    /// `pos_in_tile` is identically zero there and a clip expressed against
    /// it would mask the whole Mercator map away.
    ///
    /// Read by `vs_main` alone, which normalises the vertex position into
    /// this box and forwards the result as `VertexOutput::clip_coord` —
    /// the `Model` bind group was VERTEX-visible only when this was
    /// written (it is `VERTEX | FRAGMENT` since `line_fx`, but the
    /// normalised varying is still the cheaper read).
    ///
    /// `render::gpu::TileClip::WHOLE` puts `+/-1e30` here, which puts every
    /// real tile coordinate at exactly 0.5 in `clip_coord`, makes the mask
    /// in `fs_main` exactly 1.0, and leaves the draw bit-identical to one
    /// made before this field existed. Anything narrower is an ancestor
    /// STAND-IN clipped to the one missing tile it stands in for — see
    /// `TileClip`.
    ///
    /// A `vec4<f32>` at offset 208, after `night_brightness` at 204: 208 is
    /// already 16-byte aligned, so nothing above it moves. See
    /// `render::gpu::ModelUniform`'s field of the same name.
    clip_bounds: vec4<f32>,
    /// `(keep_start, keep_end, opacity, metrics_on)`, at offset 224:
    /// `render::gpu::ModelUniform::line_fx`. `[x, y]` is the VISIBLE range
    /// of line progress -- the complement of the style spec's
    /// `line-trim-offset`, which names the hidden range. Read only by
    /// `shader.wgsl`: its `vertex_main` and `compact_main` multiply alpha by
    /// `.z`, and its `fs_main` discards outside `[x, y]` when `.w` is on.
    /// `extrusion_shader.wgsl` declares the field only to keep this block
    /// identical and ignores it, so an opacity fade does not reach 3D
    /// buildings. `render::gpu::LINE_FX_NONE` `(0, 1, 1, 0)` is "no trim,
    /// full opacity" and changes no pixel.
    line_fx: vec4<f32>,
}

/// Half-width of the soft terminator, as a SINE of the angle above or below
/// the geometric terminator -- which is what `dot(normal, sun)` is.
///
/// `sin(3 degrees) = 0.05234`, so the band runs 3 degrees either side: six
/// degrees of arc in all. **Ours, and physically motivated rather than
/// ported** -- there is no terminator softness anywhere in MapLibre's
/// shaders to copy, because MapLibre does not shade the ground.
///
/// Six degrees is civil twilight: the sun's centre between 0 and 6 degrees
/// below the horizon is the band in which a person outdoors can still read,
/// and it is about 670 km wide on Earth's surface. On the 758 px disc a
/// 1024x768 window shows at z3 it is 40 px at the sub-camera point,
/// narrowing toward the limb where the surface turns away -- wide enough
/// that the terminator reads as dusk rather than as an edge, narrow enough
/// that it is a terminator and not a gradient across the whole planet.
///
/// A hard step would alias: the terminator is a smooth curve crossing a
/// pixel grid at an arbitrary angle, and nothing downstream antialiases it
/// (the fill's own `aa_feather_px` is a STROKE edge, in the opposite
/// direction). `smoothstep` over a band this wide makes the question moot.
const TERMINATOR_HALF_WIDTH_SIN: f32 = 0.05234;

@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var<uniform> model: Model;

/// The sphere's radius in metres: MapLibre's `GLOBE_RADIUS`
/// (`_projection_globe.vertex.glsl:1`), and `render::globe::GLOBE_RADIUS_M`.
///
/// Read only by `interpolate_projection`'s elevation term, which is always
/// zero today -- see that function.
const GLOBE_RADIUS: f32 = 6371008.8;
const PI: f32 = 3.14159265358979323846;

/// A point in `[0, 1]` in-tile space, projected onto the unit sphere.
///
/// A port of `projectToSphere` (`_projection_globe.vertex.glsl:54-93`). The
/// two-argument form there takes a second, UNTRANSLATED position purely to
/// recognise its pole sentinels (`rawPos.y < -32767.5`, `> 32766.5`,
/// `:84-90`); **this port deliberately omits that**, because those sentinels
/// are values in MVT extent space and this crate's vertex positions are lyon
/// units -- MVT coordinates times `COORD_SCALE` (1000) -- where -32768 is an
/// ordinary coordinate about a third of the way across a tile. Porting the
/// test as written would discard real geometry.
///
/// `render::subdivide` DOES now generate pole vertices (`subdivide_fill`
/// grows a cap from every fill edge on a pole-row tile's boundary), and it
/// marks them in `is_stroke` rather than in a coordinate -- see
/// `POLE_NORTH_MARKER` there for why no coordinate-space sentinel is
/// provably safe in lyon units. `vs_main` applies that marker by replacing
/// this function's result rather than by branching inside it, so
/// `project_to_sphere` stays exactly the reference's one-argument form.
///
/// # Why there is no `atan`, `sin` or `cos` of the latitude here
///
/// The conversion from Mercator y to latitude is the inverse Gudermannian,
/// `lat = 2*atan(exp(PI - y*2*PI)) - PI/2`. The reference refuses to evaluate
/// it that way (`:61-75`) and gives two reasons: the direct formula subtracts
/// PI/2 from a value near PI/2, destroying most float32 mantissa bits near
/// the equator, and some GPUs (Mali is named) have imprecise `atan`/`sin`/
/// `cos` besides. Substituting `t = exp(PI - y*2*PI)` and expanding via the
/// tangent half-angle (Weierstrass) identities eliminates all three,
/// leaving only `exp` and rational arithmetic:
///
///     sin(2*atan(t) - PI/2) = (t^2 - 1) / (t^2 + 1)
///     cos(2*atan(t) - PI/2) =    2*t    / (t^2 + 1)
///
/// `render::globe::mercator_y_to_sin_cos_lat` is the same substitution in
/// f64, and `the_half_angle_substitution_matches_the_direct_latitude_formula`
/// checks it against the direct formula there -- on the CPU, in f64, which is
/// where a transcription error in the algebra can actually be localised.
fn project_to_sphere(pos_in_tile: vec2<f32>) -> vec3<f32> {
    // Position in [0,1] of web mercator's base tile (`:56`).
    let mercator_pos = model.tile_mercator_coords.xy
        + model.tile_mercator_coords.zw * pos_in_tile;

    // A flat world projection (`crate::projection`): a POSITIVE
    // `clipping_plane.w` (the globe's is never above 0) carries its code
    // as `w * 4`, and the "sphere" point is a plane point.
    // @flat-begin
    if (model.clipping_plane.w > 0.1) {
        return project_to_flat(mercator_pos, model.clipping_plane.w * 4.0);
    }
    // @flat-end

    // THE ANTIMERIDIAN, made watertight: the last tile column ends at
    // mercator x = 1.0 and the first begins at 0.0 -- the one shared tile
    // edge whose two sides would otherwise call sin/cos with DIFFERENT
    // arguments (3*PI and PI) that are only mathematically equal. Metal's
    // fast-math trig lands them on slightly different screen positions, a
    // 1 px crack at lon 180 (a dark column over a translucent-water style).
    // Wrapping x into [0, 1) gives both sides the bit-identical argument;
    // the mapping is 2*PI-periodic, so no geometry moves.
    let wx = mercator_pos.x - floor(mercator_pos.x);
    let spherical_x = wx * PI * 2.0 + PI;

    let t = exp(PI - (mercator_pos.y * PI * 2.0));
    let t2 = t * t;
    let denom = t2 + 1.0;
    let sin_sy = (t2 - 1.0) / denom;
    let cos_sy = (2.0 * t) / denom;

    return vec3<f32>(
        sin(spherical_x) * cos_sy,
        sin_sy,
        cos(spherical_x) * cos_sy
    );
}

// @flat-begin
/// Mercator [0,1]^2 -> a flat world projection's plane point `(x, y, 0)`,
/// radius 1, y north-up. The GPU twin of `crate::projection::FlatProjection::forward`
/// (through `render::globe::mercator_to_sphere`). NOT wrapped in x: on a
/// flat world, mercator x = 1.0 is the right edge, not the left.
fn project_to_flat(mercator_pos: vec2<f32>, code: f32) -> vec3<f32> {
    let lon = mercator_pos.x * PI * 2.0 - PI;
    let lat = 2.0 * atan(exp(PI - mercator_pos.y * PI * 2.0)) - PI * 0.5;
    return project_lon_lat_to_flat(lon, lat, code);
}

/// A pole-cap vertex on a flat projection: the pole LINE at the longitude
/// of the tile-edge vertex the cap was grown from (on the globe the pole is
/// one point; on Equal Earth or Robinson it is the outline's flat top).
fn flat_pole(pos_in_tile: vec2<f32>, north: bool) -> vec3<f32> {
    let mercator_x = model.tile_mercator_coords.x + model.tile_mercator_coords.z * pos_in_tile.x;
    let lon = mercator_x * PI * 2.0 - PI;
    return project_lon_lat_to_flat(lon, select(-0.5, 0.5, north) * PI, model.clipping_plane.w * 4.0);
}

fn project_lon_lat_to_flat(lon: f32, lat: f32, code: f32) -> vec3<f32> {
    if (code < 1.5) {
        // Equal Earth (Savric, Patterson & Jenny 2018).
        let m = 0.8660254037844386;
        let theta = asin(m * sin(lat));
        let t2 = theta * theta;
        let t6 = t2 * t2 * t2;
        let x = lon * cos(theta) / (m * (1.340264 + 3.0 * -0.081106 * t2 + t6 * (7.0 * 0.000893 + 9.0 * 0.003796 * t2)));
        let y = theta * (1.340264 + -0.081106 * t2 + t6 * (0.000893 + 0.003796 * t2));
        return vec3<f32>(x, y, 0.0);
    }
    // Robinson, linear in the 5-degree table (PROJ robin.cpp, MIT).
    let u = clamp(abs(lat) * 180.0 / PI / 5.0, 0.0, 18.0);
    let i = min(u32(floor(u)), 17u);
    let f = u - f32(i);
    var table = ROBIN_TABLE;
    let a = table[i];
    let b = table[i + 1u];
    let t = mix(a, b, f);
    return vec3<f32>(0.8487 * t.x * lon, 1.3523 * t.y * sign(lat), 0.0);
}

const ROBIN_TABLE = array<vec2<f32>, 19>(
    vec2<f32>(1.0000, 0.0000), vec2<f32>(0.9986, 0.0620), vec2<f32>(0.9954, 0.1240),
    vec2<f32>(0.9900, 0.1860), vec2<f32>(0.9822, 0.2480), vec2<f32>(0.9730, 0.3100),
    vec2<f32>(0.9600, 0.3720), vec2<f32>(0.9427, 0.4340), vec2<f32>(0.9216, 0.4958),
    vec2<f32>(0.8962, 0.5571), vec2<f32>(0.8679, 0.6176), vec2<f32>(0.8350, 0.6769),
    vec2<f32>(0.7986, 0.7346), vec2<f32>(0.7597, 0.7903), vec2<f32>(0.7186, 0.8435),
    vec2<f32>(0.6732, 0.8936), vec2<f32>(0.6213, 0.9394), vec2<f32>(0.5722, 0.9761),
    vec2<f32>(0.5322, 1.0000),
);
// @flat-end

/// How much the sphere's circumference is compressed at this in-tile y --
/// `cos(latitude)`, the same rational form `project_to_sphere` uses.
///
/// `circumferenceRatioAtTileY` (`_projection_globe.vertex.glsl:34-39`).
/// Private there and private in spirit here: callers want
/// `project_line_thickness`.
fn circumference_ratio_at_tile_y(tile_y: f32) -> f32 {
    let mercator_pos_y = model.tile_mercator_coords.y + model.tile_mercator_coords.w * tile_y;
    let t = exp(PI - (mercator_pos_y * PI * 2.0));
    return (2.0 * t) / (t * t + 1.0);
}

/// The factor a tile-space stroke extrusion must be multiplied by for the
/// stroke to come out the requested width ON SCREEN.
///
/// `projectLineThickness` (`_projection_globe.vertex.glsl:41-48`), used by
/// every one of the reference's line shaders (`line.vertex.glsl:85`,
/// `line_sdf.vertex.glsl:99`, `line_pattern.vertex.glsl:97`,
/// `line_gradient.vertex.glsl:88`, `line_gradient_sdf.vertex.glsl:106`).
///
/// Mercator stretches by `1/cos(lat)` away from the equator, so a tile near
/// a pole covers far less sphere per tile unit than one at the equator: the
/// same tile-space offset is a smaller arc, and therefore fewer pixels. The
/// reciprocal here undoes exactly that. Without it every road would thin
/// towards the poles, which is the artifact this factor exists to remove --
/// and it is NOT something the projection matrix can fix, because the
/// compression varies WITHIN a tile.
///
/// `mix(1.0, thickness, transition)` rather than the bare thickness is what
/// makes the Mercator end of the transition exact: at `transition == 0` this
/// returns exactly 1.0, so the flat path's extrusion is bit-identical to what
/// it was before the globe existed. The reference's own Mercator shader
/// variant returns a hard `1.0` (`_projection_mercator.vertex.glsl:1-3`);
/// this one module has to serve both, so it interpolates to the same value
/// instead.
fn project_line_thickness(tile_y: f32) -> f32 {
    let thickness = 1.0 / circumference_ratio_at_tile_y(tile_y);
    if (model.transition < 0.999) {
        return mix(1.0, thickness, model.transition);
    }
    return thickness;
}

/// The clip-space z that hides the far side of the planet.
///
/// `globeComputeClippingZ` (`_projection_globe.vertex.glsl:99-101`). A point
/// on the near side of `model.clipping_plane` has `dot(n, p) + w > 0` and so
/// gets `z < 1`; a point on the FAR side gets `z > 1`, and once multiplied by
/// `w` in `interpolate_projection` that is outside wgpu's `0 <= z <= w` clip
/// volume, so the rasterizer drops it. That is the whole mechanism: no depth
/// prepass, no backface culling, one plane test per vertex.
///
/// It cannot overshoot at the near end. `|dot(n, p)| <= 1` for a unit-length
/// normal and a point on the unit sphere, and the plane's `w` is negative
/// (`render::globe::clipping_plane` returns
/// `-tangent_plane_distance_to_c * scale`), so `dot + w <= 1` and the result
/// stays at or above zero.
fn globe_clipping_z(sphere_pos: vec3<f32>) -> f32 {
    return 1.0 - (dot(sphere_pos, model.clipping_plane.xyz) + model.clipping_plane.w);
}

/// Blends the globe and Mercator projections of one vertex by
/// `model.transition`, and replaces z with `globe_clipping_z`'s far-side
/// test.
///
/// `interpolateProjection` (`_projection_globe.vertex.glsl:103-130`).
///
/// `flat_position` is passed in already projected, rather than being computed
/// here from `u_projection_fallback_matrix * vec4(posInTile, elevation, 1.0)`
/// as the reference does (`:116`). Two reasons, both ours:
///
///  - this crate's Mercator path extrudes strokes as a WORLD-space direction
///    pushed through `camera.view_proj`, deliberately, to stop a large
///    pre-projection world position swamping the offset in f32 (see
///    `vs_main`) -- so the flat position is not a single matrix multiply and
///    cannot be recomputed here;
///  - and `render::combined_tile_matrix` already folds the tile transform
///    into the camera matrix once per tile, which is what MapLibre's
///    per-tile fallback matrix is for.
///
/// `elevation` is always 0.0 today: the signature is ported (the reference's
/// `projectTileWithElevation` and its `GLOBE_RADIUS`-relative elevation term
/// are the two places elevation enters) but this crate has no elevation
/// source. Keeping the parameter means the term below is already in the right
/// place, and `GLOBE_RADIUS` is already the right constant, when one arrives.
fn interpolate_projection(flat_position: vec4<f32>, sphere_pos: vec3<f32>, elevation: f32) -> vec4<f32> {
    let elevated_pos = sphere_pos * (1.0 + elevation / GLOBE_RADIUS);
    var globe_position = model.globe_matrix * vec4<f32>(elevated_pos, 1.0);
    // Z is the far-side clip, not a depth -- see `globe_clipping_z`.
    globe_position.z = globe_clipping_z(elevated_pos) * globe_position.w;

    if (model.transition > 0.999) {
        // Pure globe: nothing to blend, and the reference short-circuits here
        // too (`:110-113`).
        return globe_position;
    }

    // The reference writes `result.xyw = mix(flatPosition.xyw,
    // globePosition.xyw, t)` (`:122`); WGSL has no multi-component swizzle on
    // the left of an assignment, so the same three components are written one
    // at a time.
    var result = globe_position;
    let xyw = mix(flat_position.xyw, globe_position.xyw, model.transition);
    result.x = xyw.x;
    result.y = xyw.y;
    result.w = xyw.z;

    // Only apply the far-side clip over the last 80% of the transition
    // (`:119-121`). The globe z HIDES the back of the planet, so applying it
    // early would start discarding geometry while the map is still mostly
    // flat and nothing is behind anything.
    //
    // **WHERE THIS DIVERGES FROM THE REFERENCE, AND WHY.**
    //
    // The reference ramps the clip in by SCALING the clip-space z:
    // `result.z = mix(0.0, globePosition.z, t')`. That is not a fade.
    // `globe_clipping_z` is a SIDE-OF-PLANE test -- the rasterizer's
    // comparison is `z > w`, and `z` is `globe_clipping_z * w` -- so
    // multiplying `z` by `k` does not weaken the test, **it moves the
    // plane**: `k * cz * w > w` reads `cz > 1/k`, i.e. the horizon retreats
    // from `cz == 1` to `cz == 1/k` for the whole of the blended band. The
    // ground between the two is BACK-SIDE ground, and back-side ground
    // projects into a thin annulus just inside the limb, over the front-side
    // ground already drawn there. `1/k` by zoom, with
    // `transition_for_zoom`'s own numbers:
    //
    // ```text
    //   zoom   transition    k = (t - 0.2) / 0.8     1/k
    //   3.14      0.99367               0.99209   1.008
    //   3.48      0.93139               0.91424   1.094
    //   4.00      0.74074               0.67593   1.479
    //   5.00      0.25926               0.07407  13.500
    // ```
    //
    // By zoom 5 the far-side clip is not ramped, it is OFF.
    //
    // Ramping the z toward the FAR PLANE instead leaves the plane exactly
    // where `globe_clipping_z == 1` puts it, for every ramp value above
    // zero, and still reaches both of the endpoints the ramp exists for:
    //
    //  - at `clip_ramp == 1` this is `globe_clipping_z * result.w`, i.e. an
    //    NDC z of exactly `globe_clipping_z` -- the pure-globe test above,
    //    unchanged;
    //  - at `clip_ramp == 0` every vertex lands exactly ON the far plane
    //    (`z == w`), which `z <= w` keeps, so nothing is clipped at all --
    //    which is what the threshold is for.
    //
    // It is also what this crate's own CPU twin has always done:
    // `GlobeFrame::occluded` is `dot(n, p) + w < 0` at every transition,
    // "exact rather than ramped" in its own words. Before this, a label
    // vanished at the true horizon while the ground under it did not.
    //
    // **The clip volume is wgpu's `0 <= z <= w`, not WebGL's `-w <= z <= w`,
    // and that difference is not what was wrong here.** `globe_clipping_z`
    // is never negative (`|dot(n, p)| <= 1` on the unit sphere and the
    // plane's `w` is negative, so `dot + w <= 1`), and `clip_ramp` is in
    // `[0, 1]`, so the expression below is in `[0, 1]` times a positive `w`
    // on the near side: the half-space the two conventions disagree about is
    // never entered. Both conventions clip the far side at `z > w`, which is
    // the comparison this whole mechanism is.
    let z_globeness_threshold = 0.2;
    let clip_ramp = clamp(
        (model.transition - z_globeness_threshold) / (1.0 - z_globeness_threshold),
        0.0,
        1.0
    );
    result.z = result.w * (1.0 - clip_ramp * (1.0 - globe_clipping_z(elevated_pos)));
    return result;
    // NOT ported: the reference's pole fade (`:124-128`), which forces
    // `result = globePosition` for a pole vertex and pushes its z to 100.0
    // -- outside the clip volume -- until the last 2% of the transition, so
    // its caps pop in at the very end.
    //
    // Nothing here needs it, and that is a property of how
    // `render::subdivide::append_pole_caps` builds a cap rather than an
    // omission: a pole vertex keeps the POSITION of the tile-edge vertex it
    // grew from, so every cap triangle is exactly degenerate in tile space.
    // At `transition == 0` the mix above returns the flat position for both
    // of a quad's new vertices, the quad has zero area and rasterizes to
    // nothing -- which is what the reference's z = 100.0 is for -- and
    // through the transition the SAME mix grows each cap continuously out of
    // that sliver instead of revealing it in the last 2%.
}

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) normal: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) is_stroke: f32,
    @location(4) half_width_px: f32,
    @location(5) color_to: vec4<f32>,
    @location(6) half_width_px_to: f32,
    /// Half `line-gap-width` in logical pixels: the distance from the
    /// centreline to the stroke's INNER edge. Zero means a solid line.
    /// See `render::mesh::Vertex::gap_half_px`.
    @location(7) gap_half_px: f32,
    @location(8) gap_half_px_to: f32,
    /// Distance along the line at this vertex, in lyon input units.
    @location(9) line_distance: f32,
    /// `line-dasharray` runs, alternating dash/gap, in units of LINE WIDTH
    /// (not pixels): `[2, 2]` on a 4px line is an 8px dash and an 8px gap.
    /// All zeros means "not dashed" -- every fill vertex, and every stroke
    /// whose layer has no dasharray.
    @location(10) dash_runs: vec4<f32>,
    /// `render::mesh::Vertex::line_progress`: 0..1 along the vertex's
    /// LineString for an op built with line metrics, -1.0 otherwise.
    @location(11) line_progress: f32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    /// Distance along the line at this fragment, in SCREEN PIXELS. Computed
    /// in the vertex stage so the rasterizer interpolates it directly.
    @location(1) dash_distance_px: f32,
    @location(2) dash_runs: vec4<f32>,
    /// The FULL line width in pixels **at the floored zoom** -- MapLibre's
    /// `floorwidth`. The dash mask needs a width per-fragment because
    /// `line-dasharray` units are multiples of line width.
    ///
    /// This is deliberately NOT the blended width used to extrude the stroke.
    /// `input.half_width_px` is the paint value already resolved at the
    /// plan's baked zoom (`camera.zoom.floor()`), so `* 2.0` is exactly what
    /// MapLibre's `LineFloorwidthProperty` (`useIntegerZoom: true`) produces,
    /// with no extra plumbing. MapLibre keeps these two widths strictly
    /// separate and so must we: feeding the floored width into the extrusion
    /// instead would make every road visibly JUMP in thickness at each
    /// integer zoom -- a far worse regression than the dash stepping this is
    /// meant to introduce.
    @location(3) dash_width_px: f32,
    /// `fract(camera.zoom)` forwarded to the fragment stage, where it drives
    /// the dash crossfade (`fs_main`) -- exactly the interpolant between the
    /// floored zoom's dash scaling and the next level's.
    ///
    /// Specifically NOT `model.blend_t`, which coincides with it only for
    /// normally resolved tiles; see the comment at the assignment in
    /// `vs_main`.
    ///
    /// This is a varying purely for plumbing: the `Camera` bind group is
    /// VERTEX-visible only (`render::gpu`), so `fs_main` cannot read
    /// `camera.zoom` itself. Forwarding one scalar the rasterizer
    /// interpolates (a per-draw constant, so interpolation is a no-op) is
    /// cheaper than widening the bind group's visibility.
    @location(4) dash_blend_t: f32,
    /// The stroke's extrusion direction, NORMALISED, for edge antialiasing.
    ///
    /// The two vertices across a line carry opposite directions, so the
    /// rasterizer interpolates this from `+u` through `0` to `-u`: its
    /// LENGTH is the fragment's distance from the centreline, as a fraction
    /// of the half-width. That is exactly what MapLibre's line shader reads
    /// (`dist = length(v_normal) * v_width2.s`).
    ///
    /// Normalised rather than raw, because `input.normal` is longer than
    /// unit at a miter join (up to `MAX_NORMAL_LEN`). Feeding the raw vector
    /// here would report a miter tip as lying beyond the line's own edge and
    /// fade it out.
    @location(5) aa_normal: vec2<f32>,
    /// Half-width the stroke was actually extruded to, in logical pixels --
    /// the paint half-width plus half the antialiasing band, plus the whole
    /// of `line-gap-width`'s half-gap when there is one. MapLibre's `outset`
    /// (`line.vertex.glsl`), the `.s` of its `v_width2`.
    @location(6) aa_outset_px: f32,
    /// Where the stroke STARTS, in logical pixels from the centreline --
    /// MapLibre's `inset`, the `.t` of its `v_width2`. Zero for a solid line
    /// (and for every fill vertex), which makes the inner cut in `fs_main` a
    /// no-op; half of `line-gap-width` for a casing, which punches the hole.
    @location(8) aa_inset_px: f32,
    /// Width of the antialiasing ramp, in logical pixels. ZERO for fill
    /// vertices, which is how `fs_main` tells the two apart without a
    /// separate flag.
    @location(7) aa_feather_px: f32,
    /// **The sun, as the fragment stage can see it.** Three things in one
    /// slot, because an inter-stage `vec3<f32>` costs exactly what an
    /// inter-stage `f32` costs -- one of the 15 slots iOS Metal offers:
    ///
    ///  - `.x` -- `dot(sphere_normal, sun_direction)` at this vertex, i.e.
    ///    the cosine of the sun's angle from the local vertical. **The
    ///    rasterizer's linear interpolation of this is exact**, up to the
    ///    chord-vs-arc error: a dot product is linear in its first
    ///    argument, so interpolating the cosine and dotting the
    ///    interpolated position are the same quantity. The chord across
    ///    `render::subdivide`'s 5.625-degree cell is shorter than the arc by
    ///    1 part in 830, which at the terminator (where the cosine is near
    ///    zero, and where the only visible thing happens) is an error of
    ///    zero.
    ///  - `.y` -- `model.transition`, the globeness, which is BOTH the fade
    ///    and the gate. A flat frame writes 0.0 here from the early-out
    ///    below and `fs_main` then runs no shading at all.
    ///  - `.z` -- `model.night_brightness`, forwarded because the `Model`
    ///    bind group was VERTEX-visible only (`render::gpu`) when this was
    ///    written; `line_fx` has since widened it to `VERTEX | FRAGMENT`.
    ///    The varying stays: this is a per-draw constant, so the
    ///    rasterizer's interpolation of it is a no-op.
    @location(9) globe_light: vec3<f32>,
    /// **Where this fragment falls inside the box its draw may paint**,
    /// with `0` at `model.clip_bounds`' minimum and `1` at its maximum on
    /// each axis. `fs_main` keeps the fragment when both components are in
    /// `[0, 1]` and masks it away otherwise.
    ///
    /// NORMALISED here in the vertex stage rather than forwarded as a raw
    /// in-tile position for `fs_main` to compare against the uniform,
    /// because **the `Model` bind group was VERTEX-visible only** when
    /// this was written (see `render::gpu`; `line_fx` has since made it
    /// `VERTEX | FRAGMENT`) — the same constraint `dash_blend_t` and
    /// `globe_light.z` already work around, and the same trade: one
    /// varying the rasterizer interpolates for free, against widening a
    /// uniform's visibility for every draw in the frame. The normalisation
    /// is affine, so interpolating it is exactly evaluating it at the
    /// interpolated position; nothing is approximated.
    ///
    /// It is a varying rather than something `fs_main` could recover on its
    /// own because nothing else in the fragment stage knows where in its
    /// tile a fragment is: `@builtin(position)` is the SCREEN, and a tile's
    /// screen-space region on the globe is not a rectangle, so the clip
    /// cannot be applied there. That is the same argument that rules out a
    /// scissor rectangle — see `render::gpu::TileClip`.
    ///
    /// `TileClip::WHOLE` spans `-1e30 .. 1e30`, which puts every real tile
    /// coordinate at exactly `0.5` here and makes the mask exactly `1.0`;
    /// an unclipped draw is bit-identical to one made before this existed.
    ///
    /// The eleventh inter-stage slot, against iOS Metal's 15 and the WebGL2
    /// fallback's 15 — `render::gpu`'s declared
    /// `max_inter_stage_shader_variables` need is raised to 11 with it, and
    /// `the_declared_inter_stage_need_matches_the_shader_it_is_declared_for`
    /// counts these locations off this struct to keep that honest.
    ///
    /// For a STROKE vertex this is taken from the position on the
    /// CENTRELINE: the extrusion is added in clip space, after this is
    /// written, so both vertices across a line carry the same value. A
    /// clipped stroke is therefore cut within half a line width of the box
    /// edge rather than exactly on it, which is not worth a twelfth slot to
    /// fix — the clip exists for translucent FILLS, whose vertices are the
    /// geometry itself and are cut exactly.
    @location(10) clip_coord: vec2<f32>,
    /// **MapLibre's `v_lineprogress`** (`line_gradient.vertex.glsl`): how
    /// far along its LineString this fragment is, 0..1, or -1.0 for every
    /// vertex without line metrics (fills, basemap strokes, the compact,
    /// circle and dot entry points). `fs_main` trims against
    /// `model.line_fx`; a negative value is never trimmed.
    ///
    /// The TWELFTH inter-stage slot of iOS Metal's 15 and the WebGL2
    /// fallback's 15; `render::gpu`'s declared need is raised to 12 with it.
    @location(11) line_progress: f32,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    return vertex_main(input);
}

// A fill's hairline outline (`fill-outline-color`), drawn as a LINE LIST
// over the fill's OWN vertices -- MapLibre's `fill_outline` program. The
// outline colour rides in the fill vertex's stroke-only slots, which the
// fill path never reads (`normal` = r,g; `gap_half_px`/`gap_half_px_to` =
// b,a; see `render::mesh::pack_outline_color`), so this entry point adds
// no attribute and no inter-stage variable: it re-dresses the vertex as a
// plain fill of that colour and runs the same projection.
@vertex
fn vs_outline(input: VertexInput) -> VertexOutput {
    var v = input;
    let outline = vec4<f32>(input.normal, input.gap_half_px, input.gap_half_px_to);
    v.color = outline;
    v.color_to = outline;
    v.normal = vec2<f32>(0.0, 0.0);
    v.gap_half_px = 0.0;
    v.gap_half_px_to = 0.0;
    return vertex_main(v);
}

/// Compact tile-local fill/hairline vertex (`render::compact::CompactVertex`,
/// 8 bytes against `VertexInput`'s 88). `pos` is MVT units; `paint_flags.x`
/// indexes `paint_table`, `.y` carries the pole flags `Vertex::is_stroke`
/// carries as -1/-2.
struct CompactInput {
    @location(0) pos: vec2<i32>,
    @location(1) paint_flags: vec2<u32>,
}

/// `(from, to)` colour pairs, 256 per 512-texel row; see
/// `render::compact::paint_table`. Read with a VERTEX-stage `textureLoad`
/// and no sampler: core on Metal, Vulkan, WebGPU and WebGL2 (GLES 3.0
/// guarantees 16 vertex texture units). Only `vs_compact` uses it, and wgpu
/// validates only the resources an entry point statically uses, so
/// `vs_main`'s one-binding model group is unaffected.
@group(1) @binding(1) var paint_table: texture_2d<f32>;

fn paint_texel(i: u32) -> vec4<f32> {
    return textureLoad(paint_table, vec2<i32>(i32(i % 512u), i32(i / 512u)), 0);
}

/// `vertex_main`'s FILL path (`is_stroke == 0`), statement for statement,
/// with every stroke-only statement removed and the colour read from the
/// paint table. Returns the same `VertexOutput` to the same `fs_main`: no
/// new inter-stage variable (11 of 15).
/// `a_compact_fill_is_pixel_identical_to_the_legacy_fill_flat_and_on_the_globe`
/// is what keeps the two from drifting: edit both or neither.
///
/// Drawn as a TriangleList for the fill and as a LineList for the hairline
/// (`TilePipeline::compact_outline`) over the same vertices, where the bound
/// table maps the same paint index to the outline colour.
@vertex
fn vs_compact(input: CompactInput) -> VertexOutput {
    let paint = input.paint_flags.x;
    let at = (paint / 256u) * 512u + (paint % 256u) * 2u;
    return compact_main(input, paint_texel(at), paint_texel(at + 1u));
}

/// A basemap tile's paint table (`render::compact::split_tile_mesh`), in its
/// own group so the tile's ordinary one-binding model group (group 1) serves
/// both of the tile's bodies -- including a stand-in's clipped model group,
/// built per draw. Same layout as `paint_table`.
@group(2) @binding(0) var tile_paint_table: texture_2d<f32>;

/// `vs_compact` for a split basemap tile: the colour pair from
/// `tile_paint_table`, everything else `compact_main`.
@vertex
fn vs_compact_tile(input: CompactInput) -> VertexOutput {
    let paint = input.paint_flags.x;
    let at = (paint / 256u) * 512u + (paint % 256u) * 2u;
    let pair_from = textureLoad(tile_paint_table, vec2<i32>(i32(at % 512u), i32(at / 512u)), 0);
    let pair_to = textureLoad(tile_paint_table, vec2<i32>(i32((at + 1u) % 512u), i32((at + 1u) / 512u)), 0);
    return compact_main(input, pair_from, pair_to);
}

/// A split basemap tile's UNDASHED stroke vertex
/// (`render::compact::CompactStrokeVertex`), 16 bytes.
struct CompactStrokeInput {
    /// The centreline point in MVT units.
    @location(0) pos: vec2<i32>,
    /// lyon's extrusion normal, the 88-byte vertex's own f32s.
    @location(1) normal: vec2<f32>,
    /// LINEAR texel index of the entry in `tile_paint_table`: colour from,
    /// colour to, then `(half width from, to, half gap from, to)`.
    @location(2) paint: u32,
}

fn tile_table_texel(at: u32) -> vec4<f32> {
    return textureLoad(tile_paint_table, vec2<i32>(i32(at % 512u), i32(at / 512u)), 0);
}

/// Rebuilds exactly the `VertexInput` the 88-byte stroke vertex carries and
/// runs `vertex_main` on it, so the two cannot drift: `dash_runs` is 0 for
/// every stroke the split converts, and `line_distance` only reaches the
/// fragment through the dash mask an all-zero `dash_runs` skips. No new
/// varyings.
///
/// `is_stroke` (always 1) comes from the table's fourth texel rather than a
/// literal: under Metal's fast math a constant lets the compiler fold the
/// stroke branch and fuse the globe extrusion differently from the 88-byte
/// draw, which moved 815 px by up to 2 levels. See
/// `render::compact::CompactStrokeVertex`.
@vertex
fn vs_compact_stroke(input: CompactStrokeInput) -> VertexOutput {
    let widths = tile_table_texel(input.paint + 2u);
    var v: VertexInput;
    // mesh::COORD_SCALE. Exact: an i16 times 1000 is an f32 integer.
    v.position = vec2<f32>(input.pos) * 1000.0;
    v.normal = input.normal;
    v.color = tile_table_texel(input.paint);
    v.is_stroke = tile_table_texel(input.paint + 3u).x;
    v.half_width_px = widths.x;
    v.color_to = tile_table_texel(input.paint + 1u);
    v.half_width_px_to = widths.y;
    v.gap_half_px = widths.z;
    v.gap_half_px_to = widths.w;
    v.line_distance = 0.0;
    v.dash_runs = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    // No line metrics on this path: never trimmed.
    v.line_progress = -1.0;
    return vertex_main(v);
}

/// One instanced circle (`render::circles::CircleInstance`): the centre in
/// the draw's lyon units, the radius in logical pixels, and a paint index
/// into the table at group 2 binding 0 (addressed like `vs_compact`'s).
struct CircleInstance {
    @location(0) center: vec2<f32>,
    @location(1) radius_px: f32,
    @location(2) paint: u32,
    /// The row's filter value (`overlay::filter`), relative to the column's
    /// minimum; read only when `circle_params.filter_window.z` is on.
    @location(3) filter_value: f32,
}

/// `render::circles::CircleParams`: boxes, in the draw's lyon units, whose
/// instances a stand-in view does NOT draw (a finer view covers them), and
/// the source's filter window `(lo, hi, on, 0)`.
struct CircleParams {
    exclude_a: vec4<f32>,
    exclude_b: vec4<f32>,
    filter_window: vec4<f32>,
    /// A packed multi-field filter (`filter_window.z` 2): per field, the
    /// level's shift and mask in the instance's code word
    /// (`bitcast<u32>(filter_value)`) and the inclusive level window.
    packed_shift: vec4<u32>,
    packed_mask: vec4<u32>,
    packed_lo: vec4<u32>,
    packed_hi: vec4<u32>,
}

@group(2) @binding(1) var<uniform> circle_params: CircleParams;

/// `render::circles::PAINT_ALIGN_MAP`, `PAINT_SCALE_VIEWPORT` and
/// `PAINT_INDEX_MASK`: an instance's `paint` is its paint index under two
/// flag bits, `circle-pitch-alignment: map` and `circle-pitch-scale:
/// viewport`. Zero flags are the style spec's defaults.
const CIRCLE_ALIGN_MAP: u32 = 0x80000000u;
const CIRCLE_SCALE_VIEWPORT: u32 = 0x40000000u;
const CIRCLE_PAINT_INDEX_MASK: u32 = 0x3fffffffu;

fn inside_box(p: vec2<f32>, b: vec4<f32>) -> bool {
    return all(p >= b.xy) && all(p <= b.zw);
}

/// An instanced circle's quad corner, under the instance's
/// `circle-pitch-alignment` and `circle-pitch-scale` (its `paint`'s flag
/// bits, `CIRCLE_ALIGN_MAP` / `CIRCLE_SCALE_VIEWPORT`). MapLibre's
/// `circle.vertex.glsl`, both branches:
///
///  - **`viewport`, the default**: the CENTRE runs through `vertex_main`
///    (normal zero), and the corner is added in CLIP space afterwards --
///    `gl_Position.xy += extrude * radius * u_extrude_scale * k`. The quad
///    is a screen-aligned square however the ground under it is tilted, so
///    the circle stays round at the globe's limb and up a pitched map. `k`
///    is MapLibre's `u_camera_to_center_distance` under `circle-pitch-scale:
///    map` (one pixel at the centre's depth, so a farther circle draws
///    smaller, as the map there does) and the centre's own clip `w` under
///    `viewport` (a constant pixel radius). `u_extrude_scale *
///    u_camera_to_center_distance` -- clip units per pixel at the centre's
///    depth -- is `view_proj`'s x and y row lengths times world units per
///    pixel: the view is rigid, so those lengths are the projection's own
///    `f / aspect` and `f`, and a pixel at the centre is `1 / 2^zoom` world
///    units on the flat camera and on the globe's alike (`render::globe`
///    builds the sphere so).
///  - **`map`**: the corner runs through `vertex_main` as a round cap's rim
///    vertex would -- normal `(+/-1, +/-1)`, extruded by `aa_outset_px` in
///    the ground's own plane (tile space before `project_to_sphere` on the
///    globe), so the quad lies on the ground and foreshortens with it.
///    Under `circle-pitch-scale: viewport` the radius is first scaled by
///    the centre's `w` over the centre distance, MapLibre's
///    `projected_center.w / u_camera_to_center_distance`.
///
/// Either way `aa_normal` is then the RAW corner, not its normalised
/// direction: interpolated across the quad it is the fragment's offset over
/// `aa_outset_px`, so `fs_main`'s `length(aa_normal) * aa_outset_px` is the
/// exact distance from the centre and its rim formula becomes a
/// signed-distance antialias. No new inter-stage variable: the output is
/// `VertexOutput`.
@vertex
fn vs_circle(input: CircleInstance, @builtin(vertex_index) corner_index: u32) -> VertexOutput {
    let corner = vec2<f32>(
        select(-1.0, 1.0, (corner_index & 1u) != 0u),
        select(-1.0, 1.0, (corner_index & 2u) != 0u)
    );
    let paint = input.paint & CIRCLE_PAINT_INDEX_MASK;
    let align_map = (input.paint & CIRCLE_ALIGN_MAP) != 0u;
    // A frame that never wrote the centre distance draws `viewport` scale
    // as the default `map` rather than dividing by zero.
    let center_distance = camera.circle_frame.x;
    let scale_viewport = (input.paint & CIRCLE_SCALE_VIEWPORT) != 0u && center_distance > 0.0;
    let at = (paint / 256u) * 512u + (paint % 256u) * 2u;
    var v: VertexInput;
    v.position = input.center;
    v.normal = vec2<f32>(0.0, 0.0);
    v.color = tile_table_texel(at);
    v.color_to = tile_table_texel(at + 1u);
    v.is_stroke = 1.0;
    v.half_width_px = input.radius_px;
    v.half_width_px_to = input.radius_px;
    v.gap_half_px = 0.0;
    v.gap_half_px_to = 0.0;
    v.line_distance = 0.0;
    v.dash_runs = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    // No line metrics on this path: never trimmed.
    v.line_progress = -1.0;
    var out: VertexOutput;
    if (align_map) {
        if (scale_viewport) {
            let k = vertex_main(v).clip_position.w / center_distance;
            v.half_width_px = v.half_width_px * k;
            v.half_width_px_to = v.half_width_px_to * k;
        }
        v.normal = corner;
        out = vertex_main(v);
    } else {
        out = vertex_main(v);
        let m = camera.view_proj;
        let world_units_per_px = 1.0 / pow(2.0, camera.zoom);
        var clip_per_px = vec2<f32>(
            length(vec3<f32>(m[0].x, m[1].x, m[2].x)),
            length(vec3<f32>(m[0].y, m[1].y, m[2].y))
        ) * world_units_per_px;
        if (scale_viewport) {
            clip_per_px = clip_per_px * (out.clip_position.w / center_distance);
        }
        let offset = corner * out.aa_outset_px * clip_per_px;
        out.clip_position = vec4<f32>(out.clip_position.xy + offset, out.clip_position.zw);
    }
    out.aa_normal = corner;
    // A stand-in's instance under a finer view: a degenerate quad outside
    // the clip volume, so nothing rasterises.
    if (inside_box(input.center, circle_params.exclude_a) || inside_box(input.center, circle_params.exclude_b)) {
        out.clip_position = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    }
    // A range filter (deck.gl's DataFilterExtension shape): an instance
    // outside the window -- or with no value, a NaN, which fails both
    // comparisons -- is the same degenerate quad. Moving the window is a
    // uniform write; no instance is touched.
    if (circle_params.filter_window.z > 0.5 && circle_params.filter_window.z < 1.5
        && !(input.filter_value >= circle_params.filter_window.x && input.filter_value <= circle_params.filter_window.y)) {
        out.clip_position = vec4<f32>(2.0, 2.0, 2.0, 1.0);
    }
    // Several fields at once (deck.gl's `filterSize` > 1): the value is a
    // code word of every field's level, each tested against its own
    // window. Still a uniform write per window move.
    if (circle_params.filter_window.z > 1.5) {
        // An unused lane is all zero (shift, mask, lo, hi): level 0 in
        // [0, 0], so it passes -- no vector `select`, which GLSL ES 3.00
        // (WebGL2) does not have.
        let code = vec4<u32>(bitcast<u32>(input.filter_value));
        let level = (code >> circle_params.packed_shift) & circle_params.packed_mask;
        if (any(level < circle_params.packed_lo) || any(level > circle_params.packed_hi)) {
            out.clip_position = vec4<f32>(2.0, 2.0, 2.0, 1.0);
        }
    }
    return out;
}

/// PCG-RXS-M-XS 32, line for line `overlay::dotgrid::hash::pcg` (wrapping
/// u32 arithmetic is WGSL's own). Integer-only, so it is bit-identical on
/// Metal, Vulkan, WebGPU and GLSL ES 3.0.
fn pcg(v: u32) -> u32 {
    let state = v * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

/// `hash::to_unit`: the top 24 bits as an exact f32 in `[0, 1)`.
fn pcg_unit(x: u32) -> f32 {
    return f32(x >> 8u) / 16777216.0;
}

/// One `render::dots::DotBlock`, as pulled from `dot_blocks`, plus the
/// live mode's `pop_b` (equal to `pop` when not live).
struct DotInput {
    origin: vec2<f32>,
    pop: f32,
    first_dot: u32,
    pop_b: f32,
    /// The block's composition texel (`dot_comps`), under
    /// `DOT_COLOR_CATEGORICAL` only; zero otherwise.
    comp: vec4<u32>,
    /// The block's triangle edges `(e1.x, e1.y, e2.x, e2.y)` as the raw
    /// f32 BITS of `dot_shapes` -- hashed as loaded (`parts::part_key`),
    /// bit-cast only for the position -- under `DOT_SHAPE_TRIANGLE` only;
    /// zero otherwise.
    edge_bits: vec4<u32>,
}

/// `render::dots::DotParams`, 144 bytes.
struct DotParams {
    exclude_a: vec4<f32>,
    exclude_b: vec4<f32>,
    /// First VERTEX of each bucket's run in the tile's one draw; `.x` is 0.
    seg_vertex: vec4<u32>,
    /// First BLOCK (texel) of each bucket's run in `dot_blocks`.
    seg_block: vec4<u32>,
    ppd: f32,
    cell_width: f32,
    tile_origin_y: f32,
    radius_px: f32,
    t: f32,
    ramp_row: u32,
    live: u32,
    unit_world_per_lyon: f32,
    /// Texel of block 0's `pop_b` (its `.x`), when live.
    pop_b_offset: u32,
    /// The densest cells' radius loss, `render::dots::dot_radius_px`.
    radius_shrink: f32,
    /// The overlap fade's exponent, `render::dots::overlap_alpha`; 0 = off.
    overlap_gamma: f32,
    /// Relocation candidates per dot; 0 or 1 = today's placement, no mask
    /// read (`render::dots::relocation_k`).
    mask_k: u32,
    /// The tile's lookup into `dot_mask` (`render::dot_mask::MaskTileLookup`).
    mask_int_offset: vec2<i32>,
    mask_frac: vec2<f32>,
    mask_texels_per_lyon: f32,
    /// `render::dots::COLOR_*`: 0 by density, 1 by the change from epoch
    /// A to B (`render::dots::difference_level`), 2 by group.
    color_mode: u32,
    /// `render::dots::SHAPE_*`: 0 a cell's rectangle, 1 a part's triangle.
    shape: u32,
    /// Under `DOT_COLOR_CATEGORICAL`: a dot whose group bit is 0 is dead.
    category_mask: u32,
}

/// `render::dots::SHAPE_CELL` / `SHAPE_TRIANGLE`.
const DOT_SHAPE_CELL: u32 = 0u;
const DOT_SHAPE_TRIANGLE: u32 = 1u;
/// `render::dots::COLOR_DIFFERENCE` / `COLOR_CATEGORICAL`.
const DOT_COLOR_DIFFERENCE: u32 = 1u;
const DOT_COLOR_CATEGORICAL: u32 = 2u;
/// `overlay::dotgrid::composition::OFFSET_SALT` (spec §3.3).
const DOT_GROUP_SALT: u32 = 0x9E3779B9u;

/// The circle pipeline's group-2 slot, under the dot layout (the circle
/// layout plus `dot_blocks`): only `vs_dot`/`vs_dot_flat` read this and
/// only `vs_circle` reads `circle_params`, and wgpu validates the
/// resources an entry point statically uses.
@group(2) @binding(1) var<uniform> dot_params: DotParams;

/// The tile's `DotBlock`s, one `Rgba32Uint` texel each (the struct's own
/// 16 bytes), `DOT_TEXTURE_WIDTH` per row: bucket 1's blocks, then 4's,
/// 16's and 64's (`dot_params.seg_block`), then -- when live -- one texel
/// per block whose `.x` is its `pop_b`. VERTEX PULLING, not instancing:
/// MG-8 measured a 6-vertex instance per block as the dominant cost.
/// A vertex-stage `textureLoad` (`texelFetch`) is core on WebGL2.
@group(2) @binding(2) var dot_blocks: texture_2d<u32>;

/// `render::dots::DOT_TEXTURE_WIDTH`.
const DOT_TEXTURE_WIDTH: u32 = 2048u;

/// The world-anchored basemap mask (`render::dot_mask`): one
/// `overlay::dotgrid::mask_class::MaskRank` per texel, `R8Uint`, read with
/// `textureLoad` (no sampler; `texelFetch` on WebGL2) in the VERTEX stage.
/// A draw with no mask binds `TilePipeline::land_mask_view`'s 1x1 land
/// texel, and never reads it (`mask_k` 0).
@group(2) @binding(3) var dot_mask: texture_2d<u32>;

/// Each block's composition texel (`overlay::dotgrid::composition::
/// Composition::texel`: `[c0|c1, c2|c3, c4|c5, c6|stride]`, u16 pairs, low
/// half first), at the block's own texel index. Read only under
/// `DOT_COLOR_CATEGORICAL`; a tile without one binds a 1x1 zero texel
/// (`TilePipeline::create_dot_bind_group`).
@group(2) @binding(4) var dot_comps: texture_2d<u32>;

/// Each block's triangle edges `(e1.x, e1.y, e2.x, e2.y)` as f32 bits, at
/// the block's own texel index. Read only under `DOT_SHAPE_TRIANGLE`; a
/// cell tile binds the 1x1 zero texel.
@group(2) @binding(5) var dot_shapes: texture_2d<u32>;

/// `overlay::dotgrid::hash::RANK_BUILDING`: nothing outranks it, so the
/// candidate search stops at the first one.
const DOT_RANK_BUILDING: u32 = 4u;
/// `overlay::dotgrid::hash::RANK_NO_DATA`: a texel no basemap tile covers,
/// and what a candidate outside the mask reads
/// (`render::dot_mask::MASK_CLEAR_RANK`). A dot that reads it keeps
/// candidate 0.
const DOT_RANK_NO_DATA: u32 = 0u;
/// `render::dots::MASK_K_MAX`: the most candidates a dot tries, whatever
/// the uniform says -- bounds the loop against a bad `mask_k`.
const DOT_MASK_K_MAX: u32 = 16u;

/// The mask rank under tile lyon point `p`: `MaskTileLookup::texel`'s
/// `floor(p * tpl + frac) + int_offset` (the mask origin enters only as an
/// integer, so a snap never re-rounds a candidate). Outside the mask rect
/// a candidate has no data (`dot_mask::MASK_CLEAR_RANK`, 0).
fn mask_rank(p: vec2<f32>) -> u32 {
    let t = vec2<i32>(floor(p * dot_params.mask_texels_per_lyon + dot_params.mask_frac)) + dot_params.mask_int_offset;
    let dims = vec2<i32>(textureDimensions(dot_mask));
    if (any(t < vec2<i32>(0)) || any(t >= dims)) { return DOT_RANK_NO_DATA; }
    return textureLoad(dot_mask, t, 0).r;
}

fn dot_texel(i: u32) -> vec4<u32> {
    return textureLoad(dot_blocks, vec2<i32>(i32(i % DOT_TEXTURE_WIDTH), i32(i / DOT_TEXTURE_WIDTH)), 0);
}

/// `render::dots::DOT_VERTICES`: one triangle per dot.
const DOT_VERTICES: u32 = 3u;

/// The block and within-block vertex of `vertex_index` in the tile's one
/// draw: bucket `seg` (sizes 1, 4, 16, 64) spans `DOT_VERTICES * 4^seg`
/// vertices a block, from `seg_vertex[seg]`. Each bucket draws only its
/// live PREFIX (`render::gpu::DotTileGpu`), so `seg_vertex` moves with the
/// view's `ppd` while `seg_block` does not.
fn dot_pull(vertex_index: u32) -> vec2<u32> {
    let seg = u32(vertex_index >= dot_params.seg_vertex.y)
        + u32(vertex_index >= dot_params.seg_vertex.z)
        + u32(vertex_index >= dot_params.seg_vertex.w);
    let per = DOT_VERTICES << (2u * seg);
    let local = vertex_index - dot_params.seg_vertex[seg];
    return vec2<u32>(dot_params.seg_block[seg] + local / per, local % per);
}

fn dot_input(block: u32) -> DotInput {
    let t = dot_texel(block);
    var input: DotInput;
    input.origin = vec2<f32>(bitcast<f32>(t.x), bitcast<f32>(t.y));
    input.pop = bitcast<f32>(t.z);
    input.first_dot = t.w;
    input.pop_b = input.pop;
    if (dot_params.live == 1u) {
        input.pop_b = bitcast<f32>(dot_texel(dot_params.pop_b_offset + block).x);
    }
    let at = vec2<i32>(i32(block % DOT_TEXTURE_WIDTH), i32(block / DOT_TEXTURE_WIDTH));
    input.comp = vec4<u32>(0u);
    if (dot_params.color_mode == DOT_COLOR_CATEGORICAL) {
        input.comp = textureLoad(dot_comps, at, 0);
    }
    input.edge_bits = vec4<u32>(0u);
    if (dot_params.shape == DOT_SHAPE_TRIANGLE) {
        input.edge_bits = textureLoad(dot_shapes, at, 0);
    }
    return input;
}

/// The group (0..8) of dot `i` of a record of `pop` people with
/// composition texel `comp` and hash key `key`:
/// `overlay::dotgrid::composition::group_of`, line for line, with
/// `D = min(P, 65535)` (at least 1) from the block's `pop` -- the
/// composition's own denominator. The bounds are cumulative, so "the
/// first `k` with `p < c_k`" is the number of bounds at or below `p`.
///
/// Summed as SCALAR `u32(bool)`s: a vector-condition `select` becomes
/// `mix(uvec, uvec, bvec)` in naga's GLSL, which ESSL 3.00 (WebGL2) does
/// not have (`render::dots`' `the_dot_shader_is_valid_glsl_es_300`).
fn dot_group(comp: vec4<u32>, pop: f32, key: u32, i: u32) -> u32 {
    let d = max(min(u32(max(pop, 0.0)), 65535u), 1u);
    let off = pcg(key ^ DOT_GROUP_SALT) % d;
    let stride = comp.w >> 16u;
    let p = (stride * (i % d) + off) % d;
    return u32((comp.x & 0xFFFFu) <= p) + u32((comp.x >> 16u) <= p)
        + u32((comp.y & 0xFFFFu) <= p) + u32((comp.y >> 16u) <= p)
        + u32((comp.z & 0xFFFFu) <= p) + u32((comp.z >> 16u) <= p)
        + u32((comp.w & 0xFFFFu) <= p);
}

/// `render::dots::DOT_DENSITY_LOG10_MAX`: the ramp spans 1 to 10^this
/// people per km^2.
const DOT_DENSITY_LOG10_MAX: f32 = 5.0;
/// `render::dots::DOT_DIFF_LOG2_SPAN`: the difference ramp spans 4x fewer
/// to 4x more people (log2 -2 to 2).
const DOT_DIFF_LOG2_SPAN: f32 = 2.0;
/// Mean Earth radius (IUGG), km, for the cell area.
const DOT_EARTH_RADIUS_KM: f32 = 6371.0088;
const DOT_PI: f32 = 3.14159265358979;
/// `render::dots::OVERLAP_KNEE`: expected overlaps below which a dot keeps
/// its alpha.
const DOT_OVERLAP_KNEE: f32 = 1.0;
/// `render::dots::OVERLAP_ALPHA_FLOOR`: the lowest alpha the fade gives a
/// live dot.
const DOT_OVERLAP_ALPHA_FLOOR: f32 = 0.1;
/// `render::dots::INK_CAP` / `INK_CAP_RACE`: the optical depth past which
/// a cell's further dots are not drawn (`dot_slot`'s saturation cap).
const DOT_INK_CAP: f32 = 8.0;
const DOT_INK_CAP_RACE: f32 = 32.0;
/// `render::WORLD_SIZE`: world units across the whole map at zoom 0.
const DOT_WORLD_SIZE: f32 = 512.0;

/// `render::dots::overlap_alpha`, line for line.
fn overlap_alpha(lambda: f32, gamma: f32) -> f32 {
    if (gamma <= 0.0 || !(lambda > DOT_OVERLAP_KNEE)) { return 1.0; }
    return clamp(pow(DOT_OVERLAP_KNEE / lambda, gamma), DOT_OVERLAP_ALPHA_FLOOR, 1.0);
}

/// A dot at `(u, v)` in `[0, 1)^2` of the cell whose SW corner is
/// `origin`, in tile lyon units: north of the corner (`-y`), the height
/// stretched by the Mercator factor. Every candidate and the final position
/// go through this one expression, so a candidate's mask texel is the one
/// under the dot drawn there.
fn dot_at(origin: vec2<f32>, u: f32, v: f32, stretch: f32) -> vec2<f32> {
    return origin + vec2<f32>(u * dot_params.cell_width, -v * dot_params.cell_width * stretch);
}

/// One dot slot, before projection: whether it is drawn, where (lyon
/// units of the tile), its colour (ramp, fade and sub-pixel alpha folded
/// in), its half-width in pixels and its quad corner. Shared by `vs_dot`
/// and `vs_dot_flat`, so the two cannot place or colour a dot differently.
struct DotSlot {
    alive: bool,
    position: vec2<f32>,
    color: vec4<f32>,
    half_width: f32,
    corner: vec2<f32>,
}

/// Slot `vertex_index / 3` of the block's run (`vertex_index` counted
/// within the block, from `dot_pull`), corner `vertex_index % 3` of the
/// equilateral triangle circumscribing the unit disc -- corners at radius
/// 2, so `corner` interpolates to the fragment's exact offset from the
/// centre in units of the outset and `fs_main`'s signed-distance rim cuts
/// the same disc a quad would. One triangle, not two: MG-8 measured the
/// vertex count, not the fragments, as the cost. The nesting rule, the hash and the placement
/// are `overlay::dotgrid::hash`'s, bit for bit
/// (`wgsl_pcg_matches_the_cpu_pcg`).
///
/// The dot is placed NORTH of the SW corner (`-y`: lyon y grows south),
/// so it lands inside its own cell: `origin + (h_u, -h_v * cosh(psi)) *
/// cell_width`, the height stretched by the Mercator factor at the cell.
fn dot_slot(input: DotInput, vertex_index: u32) -> DotSlot {
    var s: DotSlot;
    let dot = input.first_dot + vertex_index / DOT_VERTICES;
    let k = vertex_index % DOT_VERTICES;
    // (-sqrt 3, -1), (sqrt 3, -1), (0, 2)
    s.corner = select(
        select(vec2<f32>(1.7320508, -1.0), vec2<f32>(0.0, 2.0), k == 2u),
        vec2<f32>(-1.7320508, -1.0),
        k == 0u
    );

    // A cell's key (`hash::cell_key`); a part's also hashes its edges
    // (`parts::part_key`, C13), from the same f32s the texel holds.
    var key = bitcast<u32>(input.origin.x) ^ pcg(bitcast<u32>(input.origin.y));
    if (dot_params.shape == DOT_SHAPE_TRIANGLE) {
        let e = input.edge_bits;
        key = key ^ pcg(e.x ^ pcg(e.y ^ pcg(e.z ^ pcg(e.w))));
    }
    let a = pcg(key ^ pcg(dot));
    let b = pcg(a);
    let c = pcg(b);
    let h_t = pcg_unit(a);
    let h_u = pcg_unit(b);
    let h_v = pcg_unit(c);

    let pop = select(input.pop, mix(input.pop, input.pop_b, dot_params.t), dot_params.live == 1u);
    let quota = pop / dot_params.ppd;
    // Integer-exact (C2 ruling, `hash::dot_alive`): every index below
    // floor(quota) lives, the one at it iff h_t < fract(quota). The f32
    // sum `dot + h_t` rounds up when h_t is within an ulp of 1.
    // The min keeps the u32 conversion in range (4294967040 is the
    // largest f32 below 2^32).
    let q_pos = max(quota, 0.0);
    let whole = floor(q_pos);
    let n_whole = u32(min(whole, 4294967040.0));
    let in_quota = dot < n_whole || (dot == n_whole && h_t < q_pos - whole);
    s.alive = !(!in_quota
        || inside_box(input.origin, dot_params.exclude_a)
        || inside_box(input.origin, dot_params.exclude_b));
    if (!s.alive) {
        return s;
    }
    // A race dot (categorical or triangle) is one person: it draws at full
    // alpha however close to the quota's edge its `h_t` falls (Task 6's
    // ruling on T2's open question). A world dot fades in over the last
    // quarter-dot as before, bit for bit.
    let race = dot_params.color_mode == DOT_COLOR_CATEGORICAL || dot_params.shape == DOT_SHAPE_TRIANGLE;
    let fade = select(saturate((quota - f32(dot) - h_t) * 4.0), 1.0, race);
    // The dot's group, and the legend's toggle for it: an unticked group's
    // dot is dead like any other.
    var group = 0u;
    if (dot_params.color_mode == DOT_COLOR_CATEGORICAL) {
        group = dot_group(input.comp, input.pop, key, dot);
        if (((dot_params.category_mask >> group) & 1u) == 0u) {
            s.alive = false;
            return s;
        }
    }

    // Mercator y of the cell, then its stretch: cosh(psi) = 1 / cos(lat).
    let y_world = dot_params.tile_origin_y + input.origin.y * dot_params.unit_world_per_lyon;
    let stretch = cosh(DOT_PI * (1.0 - 2.0 * y_world));
    // Relocation: the best-ranked of the dot's first `mask_k` candidates
    // in its own cell, the lowest index on a tie, stopping at the first
    // building -- `overlay::dotgrid::hash::{dot_candidates, choose}` bit
    // for bit. Candidate 0 is (h_u, h_v): with `mask_k <= 1` the branch is
    // skipped whole and the position is today's. A candidate with no data
    // (no basemap tile under it) ends the search at candidate 0: `best` is
    // then DOT_RANK_NO_DATA, which no mask rank falls to otherwise.
    var u = h_u;
    var v = h_v;
    if (dot_params.mask_k > 1u) {
        var best = mask_rank(dot_at(input.origin, u, v, stretch));
        var p = pcg(c);
        let k_max = min(dot_params.mask_k, DOT_MASK_K_MAX);
        for (var j = 1u; j < k_max && best < DOT_RANK_BUILDING && best != DOT_RANK_NO_DATA; j = j + 1u) {
            let q = pcg(p);
            let cu = pcg_unit(p);
            let cv = pcg_unit(q);
            let rank = mask_rank(dot_at(input.origin, cu, cv, stretch));
            if (rank == DOT_RANK_NO_DATA) {
                best = DOT_RANK_NO_DATA;
                u = h_u;
                v = h_v;
            } else if (rank > best) {
                best = rank;
                u = cu;
                v = cv;
            }
            p = pcg(q);
        }
    }
    if (dot_params.shape == DOT_SHAPE_TRIANGLE) {
        // `parts::sample_in_triangle`: the fold written `u > 1 - v`, exact
        // in f32 for 24-bit hash fractions, so CPU and GPU fold alike.
        if (u > 1.0 - v) {
            u = 1.0 - u;
            v = 1.0 - v;
        }
        let edges = bitcast<vec4<f32>>(input.edge_bits);
        s.position = input.origin + u * edges.xy + v * edges.zw;
    } else {
        s.position = dot_at(input.origin, u, v, stretch);
    }

    // Density: pop / (d_arc^2 R^2 cos(lat)), cos(lat) = 1 / cosh(psi).
    let arc = dot_params.cell_width * dot_params.unit_world_per_lyon * 2.0 * DOT_PI;
    let area_km2 = arc * arc * DOT_EARTH_RADIUS_KM * DOT_EARTH_RADIUS_KM / stretch;
    let density = max(pop / max(area_km2, 1e-12), 1e-6);
    // log10(x) = log2(x) * log10(2).
    let level = saturate(log2(density) * 0.30102999 / DOT_DENSITY_LOG10_MAX);
    // The difference ramp (Task 15) reads the cell's growth from A to B
    // instead: `render::dots::difference_level`, line for line. The radius
    // and the fade below still follow the density.
    var ramp_at = level;
    if (dot_params.color_mode == DOT_COLOR_DIFFERENCE) {
        let growth = log2((max(input.pop_b, 0.0) + 1.0) / (max(input.pop, 0.0) + 1.0));
        ramp_at = clamp(0.5 + 0.5 * growth / DOT_DIFF_LOG2_SPAN, 0.0, 1.0);
    }
    var idx = u32(round(ramp_at * 255.0));
    if (dot_params.color_mode == DOT_COLOR_CATEGORICAL) {
        idx = group;
    }
    s.color = tile_table_texel(dot_params.ramp_row * 512u + idx * 2u);

    // Denser cells draw smaller dots (`render::dots::dot_radius_px`):
    // up to `radius_shrink` of the radius off at the top of the ramp.
    // Below half a pixel the quad stays 1 px wide and the alpha carries
    // the area instead: 4 r^2 = (pi r^2) / (pi 0.5^2), continuous at 0.5.
    let r = dot_params.radius_px * (1.0 - dot_params.radius_shrink * level);
    // Fade by expected overlap (controller ruling A): the alpha falls with
    // how many of this cell's dots are expected on each of its pixels, so a
    // dense core draws a gradient, not a solid block. Existence is untouched.
    let px_per_lyon = exp2(camera.zoom) * DOT_WORLD_SIZE * dot_params.unit_world_per_lyon;
    let cell_px = dot_params.cell_width * px_per_lyon;
    let lambda = max(quota, 0.0) * DOT_PI * r * r / max(cell_px * cell_px * stretch, 1e-6);
    let overlap = overlap_alpha(lambda, dot_params.overlap_gamma);
    s.color.a = s.color.a * fade * overlap * select(1.0, 4.0 * r * r, r < 0.5);
    s.half_width = max(r, 0.5);
    // **The saturation cap** (`render::dots::ink_cap_keep`, line for
    // line). `ink` is the optical depth the cell's dots lay on a pixel they
    // cover: dots per pixel times each one's alpha. Past the cap, a pixel's
    // transmittance `exp(-ink)` is already ~0 from a PREFIX of the cell's
    // dots (by index, so the kept set nests as the ppd moves), and every
    // further dot lands on a pixel its own colour already saturates: it is
    // rasterized and blended for nothing. Measured: those stacks were the
    // whole of the opening globe's 15-18 ms. A capped dot is skipped like a
    // dead slot; it still EXISTS -- the stats and the "1 dot = N people"
    // label count it -- it is just not painted under its cell's own ink.
    var area_px = cell_px * cell_px * stretch;
    if (dot_params.shape == DOT_SHAPE_TRIANGLE) {
        let e = bitcast<vec4<f32>>(input.edge_bits);
        area_px = 0.5 * abs(e.x * e.w - e.y * e.z) * px_per_lyon * px_per_lyon;
    }
    let keep = ink_cap_keep(quota, s.half_width, s.color.a, area_px, race);
    if (f32(dot) >= keep) {
        s.alive = false;
    }
    return s;
}

/// `render::dots::ink_cap_keep`, line for line: how many of a cell's
/// `quota` dots (dot radius `hw` px, alpha `a`, cell area `area_px` px^2)
/// to draw. Over a one-colour ramp a cell's ink spreads over its own area,
/// floored at one dot's disc (a cell smaller than its dots); a race dot's
/// neighbours are other colours, so its ink spreads over the cell grown by
/// a dot's diameter -- the rim it shares with them -- against a higher cap.
fn ink_cap_keep(quota: f32, hw: f32, a: f32, area_px: f32, race: bool) -> f32 {
    let q = max(quota, 0.0);
    let disc = DOT_PI * hw * hw;
    var spread = max(area_px, disc);
    var cap = DOT_INK_CAP;
    if (race) {
        let reach = sqrt(max(area_px, 0.0)) + 2.0 * hw;
        spread = reach * reach;
        cap = DOT_INK_CAP_RACE;
    }
    let ink = q * disc * a / spread;
    if (ink > cap) {
        return q * cap / ink;
    }
    return 4294967040.0;
}

/// `vs_dot`'s output: the flat path's three locations plus the globe
/// light's one varying term -- FOUR inter-stage locations against
/// `VertexOutput`'s 11, for `fs_dot` instead of `fs_main`.
struct DotGlobeOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) corner: vec2<f32>,
    /// `(outset, feather)` in pixels, as `DotFlatOutput::aa`.
    @location(2) aa: vec2<f32>,
    /// `VertexOutput::globe_light.x`: the sun's cosine at this corner. The
    /// other two terms are the draw's `model.transition` and
    /// `model.night_brightness`, which `fs_dot` reads itself.
    @location(3) light: f32,
}

/// **One population dot on a globe frame.** A slot that is not alive
/// returns a degenerate triangle outside the clip volume -- a dead slot
/// costs a hash, not a projection. A live one is `vertex_main`'s stroke
/// path for `normal = corner`, `half_width_px = s.half_width`, no gap, no
/// dash, no trim, statement for statement: the screen-pixel extrusion
/// through `camera.view_proj` and its tile-space twin into
/// `project_to_sphere`, both scaled by `project_line_thickness`, the
/// east-west flip, `interpolate_projection`, the fog and the globe light.
/// It writes `DotGlobeOutput` (4 locations) for `fs_dot`, which is
/// `fs_main`'s arithmetic for exactly those inputs. Drawn whenever
/// `transition > 0`; a flat frame draws `vs_dot_flat` instead (MG-8).
@vertex
fn vs_dot(@builtin(vertex_index) vertex_index: u32) -> DotGlobeOutput {
    let pulled = dot_pull(vertex_index);
    let s = dot_slot(dot_input(pulled.x), pulled.y);
    var out: DotGlobeOutput;
    if (!s.alive) {
        out.clip_position = vec4<f32>(2.0, 2.0, 2.0, 1.0);
        return out;
    }
    let feather = 1.0 / camera.device_pixel_ratio;
    let outset = s.half_width + feather * 0.5;
    var clip = model.matrix * vec4<f32>(s.position, 0.0, 1.0);
    let fog_depth = clip.w;
    let pos_in_tile = s.position * model.lyon_to_tile_unit;
    var adjusted_thickness = 1.0;
    if (model.transition > 0.0) {
        adjusted_thickness = project_line_thickness(pos_in_tile.y);
    }
    let world_units_per_px = 1.0 / pow(2.0, camera.zoom);
    clip = clip + camera.view_proj * vec4<f32>(s.corner * outset * world_units_per_px * adjusted_thickness, 0.0, 0.0);
    clip.x = -clip.x;
    out.light = 0.0;
    if (model.transition <= 0.0) {
        out.clip_position = clip;
    } else {
        let extrude_tile = s.corner * outset * model.tile_units_per_px * adjusted_thickness;
        let sphere_pos = project_to_sphere(pos_in_tile + extrude_tile);
        out.clip_position = interpolate_projection(clip, sphere_pos, 0.0);
        out.light = dot(normalize(sphere_pos), model.sun_direction);
    }
    var color = s.color;
    if (camera.fog_range.y > 0.0) {
        let fade = smoothstep(camera.fog_range.x, camera.fog_range.y, fog_depth);
        color = vec4<f32>(mix(color.rgb, camera.fog_color.rgb, fade), color.a);
    }
    color.a = color.a * model.line_fx.z;
    out.color = color;
    out.corner = s.corner;
    out.aa = vec2<f32>(outset, feather);
    return out;
}

/// `fs_main` for a `vs_dot` fragment: the solid-stroke rim
/// (`aa_inset_px == 0`) and the globe light. Nothing else `fs_main` does
/// reaches a dot: its `clip_bounds` are the whole tile (dots use exclusion
/// boxes), it has no dash and no line metrics.
///
/// A fragment outside the rim is DISCARDED rather than blended at alpha
/// 0: the same pixel either way (no depth write), but a dot's triangle is
/// ~40 % corners, and in a deep stack each blend is an ordered
/// read-modify-write. Measured: globe dot frames 1-2 ms faster.
@fragment
fn fs_dot(input: DotGlobeOutput) -> @location(0) vec4<f32> {
    let distance_px = length(input.corner) * input.aa.x;
    let coverage = clamp(min(distance_px + input.aa.y, input.aa.x - distance_px) / input.aa.y, 0.0, 1.0);
    if (coverage <= 0.0) { discard; }
    var rgb = input.color.rgb;
    if (model.transition > 0.0) {
        let lit = smoothstep(-TERMINATOR_HALF_WIDTH_SIN, TERMINATOR_HALF_WIDTH_SIN, input.light);
        let shade = mix(model.night_brightness, 1.0, lit);
        rgb = rgb * mix(1.0, shade, model.transition);
    }
    return vec4<f32>(rgb, input.color.a * coverage);
}

/// `vs_dot_flat`'s output: THREE inter-stage locations, against
/// `VertexOutput`'s 11 -- the colour, the quad corner, and `(outset,
/// feather)` in pixels for the rim.
struct DotFlatOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) corner: vec2<f32>,
    @location(2) aa: vec2<f32>,
}

/// **MG-8's fallback: a flat frame's dot, without `vertex_main`.** It
/// writes 3 inter-stage locations instead of 11 and skips the globe
/// branches: `vertex_main`'s `transition == 0` stroke path, statement for
/// statement -- the model matrix, the screen-pixel extrusion through
/// `camera.view_proj`, the east-west flip and the fog. Used only when the
/// frame's `transition == 0` (`TileDraw::Dots`' `flat`). MG-8 measured it
/// at 9.9 ms against `vs_dot`'s 13.4 ms for the same 4 M flat dots, which
/// is the flat criterion's difference between 3 M dots and 2.3 M.
@vertex
fn vs_dot_flat(@builtin(vertex_index) vertex_index: u32) -> DotFlatOutput {
    let pulled = dot_pull(vertex_index);
    let s = dot_slot(dot_input(pulled.x), pulled.y);
    var out: DotFlatOutput;
    if (!s.alive) {
        out.clip_position = vec4<f32>(2.0, 2.0, 2.0, 1.0);
        return out;
    }
    let feather = 1.0 / camera.device_pixel_ratio;
    let outset = s.half_width + feather * 0.5;
    var clip = model.matrix * vec4<f32>(s.position, 0.0, 1.0);
    let fog_depth = clip.w;
    let world_units_per_px = 1.0 / pow(2.0, camera.zoom);
    clip = clip + camera.view_proj * vec4<f32>(s.corner * outset * world_units_per_px, 0.0, 0.0);
    clip.x = -clip.x;
    out.clip_position = clip;
    var color = s.color;
    if (camera.fog_range.y > 0.0) {
        let fade = smoothstep(camera.fog_range.x, camera.fog_range.y, fog_depth);
        color = vec4<f32>(mix(color.rgb, camera.fog_color.rgb, fade), color.a);
    }
    out.color = color;
    out.corner = s.corner;
    out.aa = vec2<f32>(outset, feather);
    return out;
}

/// `fs_main`'s rim for a solid stroke (`aa_inset_px == 0`), and nothing
/// else `fs_main` does applies to a flat dot: no clip box (dots use
/// exclusion boxes), no dash, no globe light.
@fragment
fn fs_dot_flat(input: DotFlatOutput) -> @location(0) vec4<f32> {
    let distance_px = length(input.corner) * input.aa.x;
    let coverage = clamp(min(distance_px + input.aa.y, input.aa.x - distance_px) / input.aa.y, 0.0, 1.0);
    // `fs_dot`'s discard: a zero-coverage corner is not blended.
    if (coverage <= 0.0) { discard; }
    return vec4<f32>(input.color.rgb, input.color.a * coverage);
}

fn compact_main(input: CompactInput, color_from: vec4<f32>, color_to: vec4<f32>) -> VertexOutput {
    var out: VertexOutput;
    // mesh::COORD_SCALE. Exact: an i16 times 1000 is an f32 integer.
    let position = vec2<f32>(input.pos) * 1000.0;
    let t = model.blend_t;
    let blended_color = mix(color_from, color_to, t);

    var clip = model.matrix * vec4<f32>(position, 0.0, 1.0);
    let fog_depth = clip.w;
    out.clip_coord = (position - model.clip_bounds.xy) / (model.clip_bounds.zw - model.clip_bounds.xy);
    let pos_in_tile = position * model.lyon_to_tile_unit;
    let extrude_tile = vec2<f32>(0.0, 0.0);

    clip.x = -clip.x;

    let pole = input.paint_flags.y & 3u;
    var sphere_pos = vec3<f32>(0.0, 0.0, 0.0);
    let is_pole = pole != 0u;
    if (is_pole) {
        sphere_pos = vec3<f32>(0.0, select(-1.0, 1.0, pole == 1u), 0.0);
    }

    if (model.transition <= 0.0) {
        out.clip_position = clip;
        out.globe_light = vec3<f32>(0.0, 0.0, 0.0);
    } else {
        if (!is_pole) {
            sphere_pos = project_to_sphere(pos_in_tile + extrude_tile);
        }
        // @flat-begin
        if (is_pole && model.clipping_plane.w > 0.1) {
            sphere_pos = flat_pole(pos_in_tile, sphere_pos.y > 0.0);
        }
        // @flat-end
        out.clip_position = interpolate_projection(clip, sphere_pos, 0.0);
        out.globe_light = vec3<f32>(
            dot(normalize(sphere_pos), model.sun_direction),
            model.transition,
            model.night_brightness
        );
    }
    out.aa_normal = vec2<f32>(0.0, 0.0);
    out.aa_outset_px = 0.0;
    out.aa_inset_px = 0.0;
    out.aa_feather_px = 0.0;
    var shaded = blended_color;
    if (camera.fog_range.y > 0.0) {
        let fade = smoothstep(camera.fog_range.x, camera.fog_range.y, fog_depth);
        shaded = vec4<f32>(mix(blended_color.rgb, camera.fog_color.rgb, fade), blended_color.a);
    }
    out.color = shaded;
    out.dash_distance_px = 0.0 * model.dash_scale;
    out.dash_runs = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    out.dash_width_px = 0.0;
    out.dash_blend_t = fract(camera.zoom);
    // A compact vertex carries no line metrics: never trimmed. It still
    // fades: a tiled overlay's fills draw here, and `line_fx.z` must reach
    // them as it reaches their strokes (`vertex_main`). Exact at 1.0.
    out.line_progress = -1.0;
    out.color.a = out.color.a * model.line_fx.z;
    return out;
}

fn vertex_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    // `blend_t` is computed CPU-side, per DRAWN tile, written into each
    // tile's own model uniform -- NOT a single value shared by every tile
    // this frame; it only looks that way for the common case.
    //
    // For a tile actually resolved this frame (every tile in `visible`,
    // i.e. the vast majority of what's drawn): `MapView::plan_for` rebakes
    // any stale entry lazily before returning it (see its doc comment), so
    // that tile's `PaintPlan` is guaranteed baked as the pair `(v
    // @display_zoom, v@display_zoom + 1)` -- see `display_zoom`. The pair
    // then always tracks the camera's own bracket, so `camera.zoom -
    // display_zoom == camera.zoom.fract()` is the correct interpolant --
    // this is `MapView::tile_blend_t`.
    //
    // A cached ancestor/descendant tile drawn as a pass-1 "fallback"
    // stand-in while its own data is still loading is DIFFERENT:
    // `find_cached_ancestor`/`find_cached_descendants` select those by
    // `gpu_meshes.contains` alone, independent of `visible`, so they never
    // go through `plan_for` this frame -- their baked pair can be
    // arbitrarily older than the camera's current bucket. For those,
    // `MapView::tile_blend_t_for_baked_zoom` computes `clamp(camera.zoom -
    // baked_zoom, 0, 1)` instead of `fract()`, using the zoom the tile's
    // OWN pair was actually baked at. Either way, by the time this shader
    // runs, `model.blend_t` already IS the correct interpolant for
    // whichever of the two cases produced it -- nothing here needs to
    // know which.
    //
    // This used to be unconditionally `clamp(camera.zoom - tile.z, 0, 1)`,
    // back when a `PaintPlan` was baked once at decode time and never
    // touched again -- that clamp existed to stop a stale fallback tile's
    // blend from wrapping/aliasing once the camera drifted past its frozen
    // bracket. Display-zoom rebaking removes the need for a per-tile clamp
    // for a tile actually rebaked this frame, but a pass-1 stand-in is
    // exactly the old clamp's original case again, generalized from
    // `tile.z` to `baked_zoom` -- `fract()` alone regressed this (wrapped
    // instead of saturated) until `tile_blend_t_for_baked_zoom` fixed it.
    // Zero for fills, which is how `fs_main` knows not to fade them.
    var aa_feather_px = 0.0;
    var aa_outset_px = 0.0;
    var aa_inset_px = 0.0;
    var aa_normal = vec2<f32>(0.0, 0.0);

    let t = model.blend_t;
    let half_width_px = mix(input.half_width_px, input.half_width_px_to, t);
    let blended_color = mix(input.color, input.color_to, t);

    // `model.matrix` is now the COMBINED tile+camera matrix
    // (`render::combined_tile_matrix`), computed in f64 CPU-side and narrowed
    // once -- not just the tile-local transform. A single matrix multiply here
    // replaces the old two-step "tile-local -> world (model.matrix) -> clip
    // (camera.view_proj)" chain, which let each step's f32 rounding compound
    // into visible vertex-lattice quantization at high zoom. The
    // stroke-extrusion branch below is UNCHANGED -- it operates on a
    // world-space direction vector, not a tile-local point, so it still needs
    // the separate camera-only `view_proj` (no tile transform involved).
    var clip = model.matrix * vec4<f32>(input.position, 0.0, 1.0);
    // The horizon fade's depth, from the Mercator position BEFORE any
    // stroke extrusion or globe blend: the fade is a property of where the
    // ground is, not of how wide a line drawn on it is.
    let fog_depth = clip.w;

    // This vertex's place inside the box this draw may paint, `0` at the
    // minimum and `1` at the maximum -- the whole of the stand-in clip's
    // vertex-stage work, and the only reader of `model.clip_bounds`.
    //
    // Against `input.position`, in LYON units, and not against the `[0,1]`
    // in-tile position computed just below: `lyon_to_tile_unit` is zero
    // under `GlobeParams::FLAT`, so that one is identically zero on a flat
    // frame and a clip measured against it would mask the whole Mercator
    // map away. See `VertexOutput::clip_coord`.
    out.clip_coord =
        (input.position - model.clip_bounds.xy) / (model.clip_bounds.zw - model.clip_bounds.xy);

    // The same vertex in [0,1] in-tile space, which is the space
    // `project_to_sphere` and `circumference_ratio_at_tile_y` work in.
    //
    // Zero when `GlobeParams::FLAT` is bound (`lyon_to_tile_unit` is 0
    // there), which is harmless: every reader of this is behind a
    // `model.transition > 0.0` branch.
    let pos_in_tile = input.position * model.lyon_to_tile_unit;

    // `projectLineThickness` (`_projection_globe.vertex.glsl:41-48`). Behind
    // a branch on a UNIFORM, so the whole warp takes the same side and the
    // flat path pays nothing -- it would otherwise pay an `exp` per vertex
    // for a factor that is exactly 1.0 at `transition == 0`. The reference
    // gets the same saving from compiling a separate Mercator shader variant
    // (`_projection_mercator.vertex.glsl:1-3` returns a literal 1.0); one
    // WGSL module has to serve both projections, so the branch is where that
    // saving lives here.
    var adjusted_thickness = 1.0;
    if (model.transition > 0.0) {
        adjusted_thickness = project_line_thickness(pos_in_tile.y);
    }
    // The globe's extrusion, in [0,1] in-tile units rather than world units.
    // See the `is_stroke` branch below for why the two exist side by side.
    var extrude_tile = vec2<f32>(0.0, 0.0);

    if (input.is_stroke > 0.5) {
        // Extrude in world space by a SMALL vector (half_width_px worth of
        // world units at this zoom), then project that vector through
        // camera.view_proj as a direction (not adding it to a large
        // pre-projection world-space point, which is what caused the
        // original f32 precision-swamping bug). Projecting as a direction
        // naturally carries the camera's bearing rotation, aspect, and
        // perspective -- unlike a raw NDC-axis offset, which ignored
        // bearing entirely and collapsed to zero width at 90 degrees.
        // Mirrors MapLibre/Mapbox GL JS's real line shader technique
        // (line_vertex.glsl: `projected_extrude = u_matrix * vec4(dist/u_ratio, 0, 0)`).
        //
        // `half_width_px` here is the LOCAL blended value computed above
        // (mix of `input.half_width_px`/`half_width_px_to` by this tile's
        // own `model.blend_t`) -- baking two zoom-bracket widths per
        // vertex and blending here every frame gives continuous
        // zoom-dependent line width with zero CPU cost per frame, matching
        // MapLibre's `CompositeExpressionBinder` pattern
        // (see docs/superpowers/specs/2026-08-20-continuous-zoom-paint-interpolation-design.md).
        let world_units_per_px = 1.0 / pow(2.0, camera.zoom);
        // Widen the quad by half the antialiasing band on each side, then
        // fade that band back out in `fs_main`. Ramping alpha WITHOUT
        // widening would eat half a pixel off each edge and make every line
        // thinner and fainter than its declared width; MapLibre adds the
        // same `ANTIALIASING` term to its own outset for this reason.
        aa_feather_px = 1.0 / camera.device_pixel_ratio;
        // `line-gap-width`, verbatim from MapLibre's `line.vertex.glsl`:
        //
        //   inset  = gapwidth + (gapwidth > 0 ? ANTIALIASING : 0)
        //   outset = gapwidth + halfwidth * (gapwidth > 0 ? 2 : 1)
        //            + (halfwidth == 0 ? 0 : ANTIALIASING)
        //
        // where its `gapwidth` has already been halved and its ANTIALIASING
        // is `1 / dpr / 2` -- i.e. `aa_feather_px * 0.5` here, and
        // `gap_half_px` is likewise already half the declared gap
        // (`render::mesh::StrokeCtor`).
        //
        // The `? 2 : 1` is the part that matters and the part an "under-line"
        // workaround cannot express: with a gap, `line-width` is the width of
        // EACH of the two ribbons, so the band runs from `gap/2` to
        // `gap/2 + width` -- a full width, not a half one -- on each side of
        // the centreline. Without a gap the same expression collapses to the
        // plain half-width this shader extruded before casings existed, so a
        // solid line is bit-identical to what it was.
        let gap_half_px = mix(input.gap_half_px, input.gap_half_px_to, t);
        let has_gap = gap_half_px > 0.0;
        aa_inset_px = gap_half_px + select(0.0, aa_feather_px * 0.5, has_gap);
        aa_outset_px = gap_half_px
            + half_width_px * select(1.0, 2.0, has_gap)
            + select(aa_feather_px * 0.5, 0.0, half_width_px == 0.0);
        // `adjusted_thickness` is 1.0 unless the globe is on, so this line is
        // bit-identical to what it was for every flat frame. MapLibre applies
        // the same factor to the position it hands BOTH projections
        // (`line.vertex.glsl:85-88`), not just the globe one, which is why it
        // belongs here rather than only on `extrude_tile`.
        let extrude_world = input.normal * aa_outset_px * world_units_per_px * adjusted_thickness;
        clip = clip + camera.view_proj * vec4<f32>(extrude_world, 0.0, 0.0);
        // The globe gets a TILE-space extrusion instead, added to the
        // position BEFORE `project_to_sphere` sees it -- which is what
        // MapLibre does (`line.vertex.glsl:87`: `projectTile(pos + ... +
        // dist / u_ratio * adjustedThickness)`).
        //
        // It cannot reuse the world-space vector above. That one works by
        // pushing a DIRECTION through `camera.view_proj`, which is legitimate
        // only because a Mercator projection is affine; the sphere mapping is
        // not, so there is no matrix to push a direction through and the
        // offset has to be applied on the tile side of the projection. The
        // flat path keeps its world-space form because that form is what
        // stops a large pre-projection world position swamping a sub-pixel
        // offset in f32 -- see the comment at the top of this branch.
        extrude_tile = input.normal * aa_outset_px * model.tile_units_per_px * adjusted_thickness;
        // Guarded: a zero normal would make `normalize` produce NaN, and a
        // NaN varying poisons every fragment of the triangle.
        let normal_length = length(input.normal);
        if (normal_length > 1e-6) {
            aa_normal = input.normal / normal_length;
        }
    }

    // The camera's up vector (`render::camera::eye_target_up`'s `up_ref`)
    // was chosen to put north at the top of the screen, which — given this
    // camera's fixed "look straight down" forward direction and a
    // right-handed view matrix — necessarily makes world +x (east) map to
    // NDC -x (screen left), not screen right. There is no up-vector sign
    // choice that fixes both axes at once here (flipping up's sign trades
    // this mirror for a north-south one instead); a single post-projection
    // horizontal flip is the correct, minimal fix, confirmed empirically
    // during Task 1's review ("world +x maps to NDC -x") and again via
    // real drag-direction testing.
    //
    // THE FLIP APPLIES TO THE MERCATOR POSITION ONLY, which is why it now
    // sits here rather than on `out.clip_position` after the blend. It exists
    // to undo a property of `eye_target_up`'s basis, and
    // `render::globe::globe_frame` builds its matrix from scratch instead of
    // reusing that basis: there, `project_to_sphere` puts east at +x
    // (`spherical_x` increases with Mercator x, so `sin(spherical_x)` and
    // hence the sphere's x increase eastward) and the right-handed DirectX
    // projection carries +x to NDC +x, i.e. screen right, already correct.
    // Flipping the blended result would mirror the globe east-for-west.
    clip.x = -clip.x;

    // A pole vertex's `position` is the tile-edge vertex it was grown from
    // (see `render::subdivide::append_pole_caps`), so `project_to_sphere`
    // would put it at latitude +/-85.051 -- the edge of Mercator -- instead
    // of at the pole. This is the reference's `projectToSphere(pos, rawPos)`
    // override (`_projection_globe.vertex.glsl:83-90`), against a marker in
    // `is_stroke` rather than a coordinate sentinel: see
    // `render::subdivide::POLE_NORTH_MARKER` for why lyon units admit no
    // provably safe sentinel and why `is_stroke` does.
    //
    // `-1.0` is north and `-2.0` is south, and both are negative so the
    // `input.is_stroke > 0.5` branch above -- and every other reader of the
    // field -- still treats a pole vertex as the fill vertex it is.
    var sphere_pos = vec3<f32>(0.0, 0.0, 0.0);
    let is_pole = input.is_stroke < -0.5;
    if (is_pole) {
        sphere_pos = vec3<f32>(0.0, select(-1.0, 1.0, input.is_stroke > -1.5), 0.0);
    }

    if (model.transition <= 0.0) {
        // Pure Mercator: return before `project_to_sphere` is ever
        // evaluated, so a flat map costs exactly what it cost before the
        // globe existed -- one `exp`, two trig calls and a mat4 multiply per
        // vertex is not a rounding error at a million vertices a frame.
        //
        // The reference short-circuits the OTHER end of the range
        // (`_projection_globe.vertex.glsl:110`) and not this one, because it
        // compiles a separate shader for pure Mercator and never reaches this
        // code with `transition == 0`. We do, on every flat frame, which is
        // almost all of them.
        out.clip_position = clip;
        // ZERO, and the middle component is what matters: `fs_main`'s
        // shading is gated on `globe_light.y > 0.0`, so a flat fragment
        // executes none of it and the returned colour is the byte-identical
        // `input.color` it was before the ground was lit. This is the one
        // place that guarantee is made -- see
        // `a_flat_frame_is_pixel_identical_however_the_sun_is_written`.
        out.globe_light = vec3<f32>(0.0, 0.0, 0.0);
    } else {
        if (!is_pole) {
            sphere_pos = project_to_sphere(pos_in_tile + extrude_tile);
        }
        // @flat-begin
        if (is_pole && model.clipping_plane.w > 0.1) {
            sphere_pos = flat_pole(pos_in_tile, sphere_pos.y > 0.0);
        }
        // @flat-end
        out.clip_position = interpolate_projection(
            clip,
            sphere_pos,
            // No elevation source in this crate; see
            // `interpolate_projection`.
            0.0
        );
        // THE SURFACE NORMAL IS FREE. On a sphere of unit radius the
        // outward normal at a point IS that point, so the lighting term
        // needs no normal attribute, no tangent frame and no extra matrix --
        // `project_to_sphere` already computed it. That is the whole reason
        // ground shading is reachable here and was not reachable in
        // `atmosphere_shader.wgsl`, which has no surface to stand on.
        //
        // `normalize` rather than the raw position: `project_to_sphere`
        // returns a unit vector by construction and a pole vertex is
        // exactly `(0, +/-1, 0)`, so this is a no-op in f64 -- but it is
        // evaluated in f32 from an `exp` and a rational, and a length of
        // 1.0000001 dotted against the sun would shift the terminator by
        // nothing while making the quantity something other than a cosine.
        // One `inversesqrt` per vertex, on the branch that already pays for
        // an `exp` and a matrix multiply.
        out.globe_light = vec3<f32>(
            dot(normalize(sphere_pos), model.sun_direction),
            model.transition,
            model.night_brightness
        );
    }
    out.aa_normal = aa_normal;
    out.aa_outset_px = aa_outset_px;
    out.aa_inset_px = aa_inset_px;
    out.aa_feather_px = aa_feather_px;
    // THE HORIZON FADE. Off (`fog_range.y == 0.0`) takes the branch not at
    // all, so every frame below 50 degrees of pitch -- every frame that
    // existed before this -- writes `blended_color` exactly as it did:
    // `a_flat_frame_is_pixel_identical_through_the_space_aware_path` holds
    // it to that. Per VERTEX, not per fragment: no new inter-stage variable
    // (11 of iOS's 15 are taken), and the `Camera` group is vertex-visible
    // only. The rasteriser interpolates the faded colour between a
    // triangle's vertices, so a triangle that spans the band gets a linear
    // ramp across it rather than the exact curve -- coarse near the horizon
    // at a source-clamped cover zoom, where tiles are large, but monotone.
    var shaded = blended_color;
    if (camera.fog_range.y > 0.0) {
        let fade = smoothstep(camera.fog_range.x, camera.fog_range.y, fog_depth);
        shaded = vec4<f32>(mix(blended_color.rgb, camera.fog_color.rgb, fade), blended_color.a);
    }
    out.color = shaded;
    // `line_distance` is zoom-independent (lyon units); `dash_scale` carries
    // the whole zoom dependence, so the product is a screen-pixel distance.
    // That distance is now STEPPED, not continuous: `dash_scale` uses
    // `floor(camera.zoom)` (see `render::camera::dash_scale`), so within a
    // zoom level the phase is pinned in tile units while the geometry keeps
    // growing -- MapLibre's `u_tileratio` behaviour. Doing the multiply
    // here, on a per-vertex scalar, lets the rasterizer interpolate the
    // already-scaled value -- no per-fragment work, and no large-magnitude
    // lyon-unit value surviving into the fragment stage.
    out.dash_distance_px = input.line_distance * model.dash_scale;
    out.dash_runs = input.dash_runs;
    // NOT `half_width_px` (the blended local above) -- see `dash_width_px`'s
    // doc comment. The unblended `input.half_width_px` is the width resolved
    // at the floored zoom, i.e. MapLibre's `floorwidth`, and only the dash
    // period may use it. The extrusion above keeps the blended width so
    // on-screen thickness stays continuous.
    out.dash_width_px = input.half_width_px * 2.0;
    // `fract(camera.zoom)`, NOT `t` (`model.blend_t`). These are equal for
    // every normally resolved tile, but they are NOT the same quantity and
    // substituting the paint interpolant here is a real bug.
    //
    // The crossfade in `fs_main` mixes `mask_a` (the pattern at
    // `2^floor(camera.zoom)`) with `mask_b` (the pattern at
    // `2^(floor(camera.zoom)+1)`), and `dash_distance_px` above is scaled by
    // `dash_scale`, which ALWAYS floors the CURRENT camera zoom
    // (`render::camera::dash_scale`). The mix weight must therefore be
    // quantised against that same `floor(camera.zoom)` bracket, or the two
    // ends of the crossfade stop matching the scale they are supposed to
    // interpolate between, and the continuity proof at the integer boundary
    // no longer holds.
    //
    // `model.blend_t` is not always that. For a pass-1 stand-in tile drawn
    // while its own data loads, `MapView::tile_blend_t_for_baked_zoom`
    // deliberately computes `clamp(camera.zoom - baked_zoom, 0, 1)` against
    // the zoom that tile's OWN paint pair was baked at -- correct for colour
    // and width, which really are interpolating that pair. A stand-in baked
    // more than one bucket below the camera therefore has `blend_t == 1.0`,
    // which would select pure `mask_b` while `dash_scale` is still the
    // floored CURRENT scale: the dash period would render up to 2x too short
    // on fallback tiles until real data lands. Hence the separate read of
    // `camera.zoom` here. Do not "simplify" this back to `t`.
    out.dash_blend_t = fract(camera.zoom);
    // LINE PROGRESS AND OPACITY (`model.line_fx`). The progress passes
    // through for `fs_main`'s trim. The opacity multiplies every vertex's
    // alpha -- fills and strokes alike -- here in the vertex stage, where it
    // is one multiply per vertex rather than per fragment; `LINE_FX_NONE`'s
    // `z == 1.0` makes it exact, so an un-faded draw is bit-identical.
    out.line_progress = input.line_progress;
    out.color.a = out.color.a * model.line_fx.z;
    return out;
}

/// # Colour space: this shader writes sRGB, and does not linearise
///
/// The render target is a UNORM format, not `*Srgb` (see `map_view.rs`'s
/// surface-format selection for the full reasoning). So the value written
/// here is the value stored, and the fixed-function blender composites
/// translucent layers on sRGB-ENCODED values -- which is what MapLibre GL
/// JS, Mapbox GL JS and all four MapLibre Native backends do, and what the
/// styles' alphas were chosen against.
///
/// This shader therefore writes the authored sRGB colour through unchanged.
/// It used to convert to linear here and let an `Srgb` target encode it back
/// on write; the two cancelled for every opaque pixel, and existed only to
/// move the blend into linear light. Removing both is what moved it back.
/// Do NOT reintroduce a transfer function here without changing the target
/// format in the same commit -- either one alone is a map about 2.2 gamma
/// out.

/// Signed distance, in pixels, from `phase_px` to the nearest dash-pattern
/// run boundary -- positive inside a dash, negative inside a gap. The caller
/// smoothsteps it for a ~1px antialiased edge.
///
/// Runs alternate dash, gap, dash, gap. `DashPattern` guarantees the live run
/// count is 0, 2, or 4 -- never odd -- and that every live run is STRICTLY
/// positive (zero-length runs were collapsed at style-compile time, because a
/// same-type boundary is not an edge). So a 2-run pattern simply has
/// `runs.z == runs.w == 0.0`, which degenerates this math correctly: `b2 == b1`
/// makes the second in-dash interval empty and the duplicate boundary in the
/// `min` harmless. No separate run count needs to reach the shader.
///
/// `width_px` is the multiplier because `line-dasharray` values are multiples
/// of line width, not pixels.
fn dash_signed_distance(phase_px: f32, runs: vec4<f32>, width_px: f32, period_px: f32) -> f32 {
    let b0 = runs.x * width_px;
    let b1 = b0 + runs.y * width_px;
    let b2 = b1 + runs.z * width_px;

    let inside = phase_px < b0 || (phase_px >= b1 && phase_px < b2);

    // Distance to every boundary, including the wrap at 0 and at `period_px`.
    // The seam is a real edge -- the trailing gap ends exactly where the
    // leading dash begins -- so it has to feather like the interior ones or
    // it aliases into a hard stair-step at every period.
    var d = min(
        min(phase_px, abs(phase_px - b0)),
        min(abs(phase_px - b1), abs(phase_px - b2))
    );
    d = min(d, abs(phase_px - period_px));

    if (inside) {
        return d;
    }
    return -d;
}

/// The antialiased dash coverage at `distance_px` along the line: 1 inside a
/// dash, 0 inside a gap, feathered across the boundary.
///
/// Factored out of `fs_main` because the crossfade there calls it TWICE, at two
/// different distance scales. Both calls must share one copy of the wrap
/// (`floor` modulo) and of `dash_signed_distance`'s zero-gap/seam handling --
/// two hand-kept-identical copies would be a maintenance trap, and any drift
/// between them would reintroduce exactly the boundary jump the crossfade
/// exists to remove.
///
/// The caller guarantees `period_px > 1e-6`, so the modulo's divide is safe.
///
/// +/-0.5px feather, matching what MapLibre achieves via its `sdfgamma` texture
/// lookup but without the LineAtlas: the longest pattern in the vendored style
/// is 4 runs, so an analytic mask buys the same result with no atlas and no SDF
/// rasterizer. The band is a fixed pixel width, independent of dash length and
/// line width, which is what keeps the rail-track `[0.1, 15]` hatch a
/// consistent faint sub-pixel tick instead of vanishing at one zoom and
/// blooming at another.
///
/// Note the feather is 0.5 units *in `distance_px`'s own space*, not 0.5 true
/// screen pixels -- `distance_px` is measured at the floored zoom, so one unit
/// is `2^fract(zoom)` screen pixels. That is what makes the crossfade's two
/// calls agree: see `fs_main`.
fn dash_mask(distance_px: f32, runs: vec4<f32>, width_px: f32, period_px: f32) -> f32 {
    let phase_px = distance_px - floor(distance_px / period_px) * period_px;
    let sd = dash_signed_distance(phase_px, runs, width_px, period_px);
    return smoothstep(-0.5, 0.5, sd);
}

/// The round-dot coverage of a `DashPattern::round_dots` stroke at this
/// fragment: 1 inside a disc of the line's width centred on every period,
/// feathered over a pixel. Crossfaded across the floored zoom exactly as
/// the dash mask is (`fs_main`), but with the along-line offset brought
/// back to screen pixels (one unit of `dash_distance_px` is `2^t` screen
/// pixels at this level, `2^(t-1)` at the next), so a dot stays round
/// while the pattern's spacing steps.
fn round_dot_mask(input: VertexOutput) -> f32 {
    let width_px = input.dash_width_px;
    let period_px = (input.dash_runs.x + input.dash_runs.y) * width_px;
    if (period_px <= 1e-6) {
        return 1.0;
    }
    let across_px = length(input.aa_normal) * input.aa_outset_px;
    let radius_px = width_px * 0.5;
    let t = input.dash_blend_t;
    let phase_a = input.dash_distance_px - floor(input.dash_distance_px / period_px) * period_px;
    let along_a = min(phase_a, period_px - phase_a) * exp2(t);
    let d_b = input.dash_distance_px * 2.0;
    let phase_b = d_b - floor(d_b / period_px) * period_px;
    let along_b = min(phase_b, period_px - phase_b) * exp2(t - 1.0);
    let mask_a = smoothstep(-0.5, 0.5, radius_px - length(vec2<f32>(along_a, across_px)));
    let mask_b = smoothstep(-0.5, 0.5, radius_px - length(vec2<f32>(along_b, across_px)));
    return mix(mask_a, mask_b, t);
}

@fragment
fn fs_main(input: VertexOutput, @builtin(front_facing) front_facing: bool) -> @location(0) vec4<f32> {
    // **THE LINE TRIM** (`model.line_fx`, MapLibre's `line-trim-offset`
    // idea over `lineMetrics` progress). Only when metrics are on for this
    // draw (`w > 0.5`) and this fragment has a progress (`>= 0`, i.e. its
    // op was built with line metrics): outside `[x, y]` it does not exist.
    // `discard` for the reason the stand-in clip below gives -- a zero
    // alpha would still write depth. Scalar `f32` comparisons only (the
    // WebGL2 back end rejects a vector `select` over mixed kinds).
    //
    // A kept range that is EMPTY (`y <= x`) draws nothing, not even the
    // start cap (whose progress is exactly 0, inside `[0, 0]`). An end is
    // cut only when it is inside the line (`x > 0`, `y < 1`): cap and join
    // fragments interpolate to `1.0 + ulp` or `-ulp` under perspective, and
    // a full `[0, 1]` must never lose them. (`TilePipeline::write_model_line_fx`
    // also turns a full range off host-side.)
    if (model.line_fx.w > 0.5 && input.line_progress >= 0.0 &&
        (model.line_fx.y <= model.line_fx.x ||
         (model.line_fx.x > 0.0 && input.line_progress < model.line_fx.x) ||
         (model.line_fx.y < 1.0 && input.line_progress > model.line_fx.y))) {
        discard;
    }
    var alpha = input.color.a;

    // **THE STAND-IN CLIP.** A draw whose `clip_bounds` is narrower than
    // the tile may only paint inside that box of the tile's own coordinate
    // space; everything outside it is left to whichever tile really owns
    // it. See `render::gpu::TileClip` for the defect this removes -- a
    // translucent fill composited TWICE wherever a coarse ancestor was
    // drawn under a ready sibling as well as under the missing tile it was
    // pulled in for.
    //
    // `clip_coord` is a varying, not the uniform, because the `Model` bind
    // group was vertex-visible only when this was written (`line_fx` has
    // since widened it); `vs_main` normalises the vertex into the box and
    // this reads the result. See `VertexOutput::clip_coord`.
    //
    // # It has to be `discard`, and multiplying alpha by zero is NOT enough
    //
    // This was written first as `alpha *= step(...)`, to keep the shader
    // free of a `discard` that costs early depth testing on every tile of
    // every frame. **It was measured, and it destroyed 999 pixels of a
    // ready tile's fill.** A fragment with zero alpha still runs to
    // completion and still WRITES DEPTH, and an ancestor's z and a
    // descendant's z at the same point of the sphere are computed by
    // different arithmetic and round differently in f32 -- the exact ULP
    // disagreement `TilePipeline::backdrop`'s doc comment describes, and
    // the reason that pipeline writes no depth at all. Wherever the
    // masked-out ancestor's z landed the smaller one, the ready tile drawn
    // over it in pass 2 was rejected by `LessEqual` and the pixel came out
    // BARE. `capture`'s `--absent 1 --no-stand-in` control is what caught
    // it: the clipped frame covered 999 fewer pixels than the frame with no
    // stand-in at all, which is a draw that can only ever add coverage
    // taking some away.
    //
    // `discard` is the only spelling that says what is meant -- this
    // fragment does not exist -- and it is what costs nothing that is
    // actually being collected here: all of this crate's geometry is
    // coplanar at z=0 and `depth_compare` is `LessEqual`, so early-Z was
    // never rejecting a tile fragment in the first place.
    //
    // The test is the closed box `0 <= clip_coord <= 1` on both axes.
    // Closed on both ends rather than half-open: the two sides of a shared
    // edge are a measure-zero set of fragment centres, and the cost of
    // getting it wrong in the other direction -- a one-pixel transparent
    // crack along every stand-in's boundary -- is the hole a stand-in
    // exists to prevent.
    //
    // `TileClip::WHOLE` puts every vertex at exactly 0.5 here, so an
    // unclipped draw never takes this branch and its output is unchanged
    // bit for bit -- the standard
    // `a_flat_frame_is_pixel_identical_however_the_sun_is_written` and the
    // globe pixel tests hold this file to.
    if (any(input.clip_coord < vec2<f32>(0.0)) || any(input.clip_coord > vec2<f32>(1.0))) {
        discard;
    }

    // **THE FAR SIDE AT THE LIMB.** A globe fill fragment that faces away
    // from the camera is the far side of the planet, and is dropped.
    // `globe_clipping_z` hides the far side per VERTEX, so a chord triangle
    // that straddles the horizon plane keeps the part of it on the near
    // side -- and when that triangle faces away, it lies under a near-side
    // triangle covering the same pixels. A translucent fill composites
    // there twice: a pale rim, measured as 35 px at lon 0 lat 0 z1.5 and
    // z2.5 (`a_translucent_fill_is_composited_exactly_once_per_pixel_on_the_globe`).
    // Depth cannot stop it, because the far triangle passes whenever it is
    // drawn first.
    //
    // Fills only (`aa_feather_px == 0`; a stroke or circle quad has no
    // winding of its own) and the globe only (`globe_light.y` is the
    // transition, 0 on a flat frame, so a flat frame is unchanged bit for
    // bit). It relies on every fill triangle having one winding, which
    // `render::subdivide::orient_fill_triangles` gives it, pole caps
    // included. A fill outline is a line list, and a line is always front
    // facing.
    if (!front_facing && input.globe_light.y > 0.0 && input.aa_feather_px == 0.0) {
        discard;
    }

    // Feather the stroke's two long edges over one physical pixel, so a line
    // at an angle stops stair-stepping. `aa_normal`'s length is this
    // fragment's distance from the centreline as a fraction of the outset,
    // so multiplying by the outset gives that distance in pixels; the ramp
    // then runs from fully opaque at the paint edge to fully clear at the
    // widened edge.
    //
    // Fill vertices carry `aa_feather_px == 0` and are left alone -- a fill
    // has no centreline for this to be measured from.
    //
    // The `min` against the INNER edge is what draws a `line-gap-width`
    // casing: MapLibre's `alpha = clamp(min(dist - (inset - blur2), outset -
    // dist) / blur2)` (`line.fragment.glsl`), with `blur2` == this feather
    // because `line-blur` is not a property this renderer reads. For a solid
    // line `aa_inset_px` is 0, so the first term is `dist + feather`, which
    // is never the smaller of the two -- the expression reduces exactly to
    // the outer-edge-only ramp that stood here before casings existed.
    if (input.aa_feather_px > 0.0) {
        let distance_px = length(input.aa_normal) * input.aa_outset_px;
        alpha *= clamp(
            min(distance_px - (input.aa_inset_px - input.aa_feather_px),
                input.aa_outset_px - distance_px) / input.aa_feather_px,
            0.0,
            1.0
        );
    }

    // Round dots (`style::DashPattern::round_dots`, flagged by a negative
    // fourth run, which no ordinary pattern carries): a disc one line
    // width across, centred every `(x + y)` widths. Ordinary patterns skip
    // this branch and take the dash mask below untouched.
    if (input.dash_runs.w < 0.0) {
        alpha *= round_dot_mask(input);
    }
    // No `is_stroke` guard is needed: fill vertices carry
    // `dash_runs == vec4(0.0)`, so this sum is 0 and the branch is skipped.
    let period = input.dash_runs.x + input.dash_runs.y + input.dash_runs.z + input.dash_runs.w;
    if (input.dash_runs.w >= 0.0 && period > 0.0) {
        let period_px = period * input.dash_width_px;
        // A hairline width can underflow `period_px` to zero even with a
        // positive pattern, so test the product, not just `period`. One guard
        // covers BOTH masks below -- they share `period_px`, since only the
        // distance scale differs between them.
        if (period_px > 1e-6) {
            // MapLibre's a/b dash crossfade (`line_sdf` + `getCrossfadeParameters`'s
            // `fromScale: 2` / `toScale: 1`), done analytically.
            //
            // `dash_distance_px` is scaled by `2^floor(camera.zoom)`
            // (`render::camera::dash_scale`), NOT by `2^camera.zoom`. That
            // quantisation is wanted -- it is MapLibre's `u_tileratio`
            // behaviour and the stepped sizing Task 8 asked for -- but on its
            // own it makes the scale DOUBLE at every integer zoom. Taken
            // modulo the period, doubling every point's phase repositions the
            // whole pattern along the road: dashes teleport instead of
            // travelling, which is the artifact this crossfade removes.
            //
            // So evaluate the mask at both quantised scales and blend:
            //   mask_a -- this level's scale, 2^floor(z)
            //   mask_b -- the next level's, 2^(floor(z)+1), i.e. twice the
            //             distance. Same `dash_runs`, same floorwidth; only
            //             the distance scale differs, exactly as MapLibre's
            //             line_sdf shares one `floorwidth` between `v_tex_a`
            //             and `v_tex_b`.
            //
            // CONTINUITY. Let D_L = line_distance * 2^L * k and t = fract(z).
            // Just below the boundary (floor(z) == L, t -> 1) the mix is pure
            // mask_b == M(2 * D_L) == M(D_{L+1}). Just above it (floor(z) has
            // become L+1, t -> 0) the mix is pure mask_a, and `dash_scale`
            // has itself doubled, so that is M(D_{L+1}) -- the identical
            // value. Both sides of every integer zoom agree exactly, so no
            // jump remains.
            //
            // The feather and the on-screen period stay continuous by the same
            // identity: one unit of `dash_distance_px` is 2^t screen pixels, so
            // mask_a's 0.5-unit feather is 0.5..1.0 screen px across a level
            // while mask_b's is 0.25..0.5; at t -> 1 mask_b's is 0.5 px, which
            // is precisely mask_a's at t -> 0 on the far side.
            //
            // NOT crossfaded: the pattern itself. A `["step", ["zoom"], ...]`
            // dasharray that changes `dash_runs` at an integer zoom (admin-1 at
            // z7, road-path at z15/16/17) still snaps, because mask_b reuses
            // this level's runs. Fixing that needs a second `dash_runs_to`
            // vertex attribute (+16 bytes/vertex); deliberately deferred -- the
            // scale snap above is the one that shows on every dashed road, the
            // pattern snap is rare and far less visible.
            let mask_a = dash_mask(
                input.dash_distance_px,
                input.dash_runs,
                input.dash_width_px,
                period_px
            );
            let mask_b = dash_mask(
                input.dash_distance_px * 2.0,
                input.dash_runs,
                input.dash_width_px,
                period_px
            );
            alpha = alpha * mix(mask_a, mask_b, input.dash_blend_t);
        }
    }

    // THE NIGHT SIDE OF THE PLANET IS DARK.
    //
    // Task 9 shipped a terminator in the AIR and said plainly what that
    // left undone: "this pass shades the air, so the night limb loses its
    // glow while the night side of the planet itself stays exactly as
    // bright as the day side". This is the rest of it, and it is the
    // `light` that module's docs said the crate did not have.
    //
    // Three deliberate properties, in the order they matter:
    //
    // 1. **`input.globe_light.y > 0.0` is the gate, and it is the globeness.**
    //    A flat frame writes `vec3(0.0)` into this varying from `vs_main`'s
    //    `transition <= 0.0` early-out, so a flat fragment does not execute
    //    one instruction below and returns the colour it returned before
    //    this block existed. The flat path is bit-identical by construction
    //    rather than by tolerance -- the standard every task in this plan
    //    has held to (Task 8 measured 0 of 1,048,576 bytes; Task 9 the
    //    same), and pinned here by
    //    `a_flat_frame_is_pixel_identical_however_the_sun_is_written`.
    //
    // 2. **The transition FADES it in**, by the same scalar that gates it.
    //    A zoom out through `PROJECTION_BLEND_ZOOM_MAX` grows the shading
    //    continuously from nothing, exactly as `interpolate_projection`
    //    grows the sphere. WHERE the shading actually appears is set on the
    //    CPU instead, by `MapView::paint` raising `night_brightness` to a
    //    literal 1.0 outside the SPACE range (`space_transition_for_zoom`,
    //    z3-z6) -- see the comment there for why that reproduces the old
    //    frame term for term, now that the projection's globeness and the
    //    space fade are two different numbers. `mix(1.0, shade, 0.0)` is
    //    bitwise 1.0, so the gate above is belt-and-braces rather than
    //    load-bearing at exactly zero -- the same honest finding Task 8
    //    recorded for `space_clear_color`'s guard, and recorded here for
    //    the same reason.
    //
    // 3. **`night_brightness` is a FLOOR, not a black.** Earth's night side
    //    really is black from orbit, but a basemap is information and an
    //    unreadable one is a worse answer than a slightly untruthful one.
    //    See `render::atmosphere::Atmosphere::night_brightness` for the
    //    default and what it costs on a near-black style.
    //
    // NOT applied to labels, icons or the overlay, and that is a decision
    // rather than an omission -- `render::atmosphere::Atmosphere::night_brightness`
    // carries the argument.
    var rgb = input.color.rgb;
    if (input.globe_light.y > 0.0) {
        // `smoothstep` over the sine of the angle either side of the
        // geometric terminator. `globe_light.x` is `cos(zenith angle)`,
        // which for small angles above the terminator is the sine of the
        // sun's elevation -- so the band is symmetric in elevation, which
        // is the thing twilight is symmetric in.
        let lit = smoothstep(
            -TERMINATOR_HALF_WIDTH_SIN,
            TERMINATOR_HALF_WIDTH_SIN,
            input.globe_light.x
        );
        let shade = mix(input.globe_light.z, 1.0, lit);
        rgb = rgb * mix(1.0, shade, input.globe_light.y);
    }

    return vec4<f32>(rgb, alpha);
}

// ---------------------------------------------------------------------------
// The dot mask pass (`render::dot_mask`, `TilePipeline::encode_dot_mask`):
// a basemap tile's ranked fill triangles, rasterised into the world-anchored
// `R8Uint` mask. A mask-only pipeline layout: `MaskDraw` is group 0 there,
// with a dynamic offset per draw (WebGL2 has no push constants). The camera
// at this module's group 0 is a different layout's; no entry point uses both.
// ---------------------------------------------------------------------------

/// One mask draw: `ndc = pos_lyon * scale + offset` (computed in f64 on the
/// CPU relative to the mask origin, y flipped), and the rank the draw
/// writes. 32 bytes; `render::gpu::MaskDraw` mirrors it.
struct MaskDraw {
    scale: vec2<f32>,
    offset: vec2<f32>,
    rank: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<uniform> mask_draw: MaskDraw;

/// A split tile's compact fill vertex (`CompactInput`'s position alone).
/// Outputs only the position: no inter-stage variable, since `fs_mask`
/// reads the rank from the uniform.
@vertex
fn vs_mask_compact(@location(0) pos: vec2<i32>) -> @builtin(position) vec4<f32> {
    // mesh::COORD_SCALE, as `compact_main`.
    let position = vec2<f32>(pos) * 1000.0;
    return vec4<f32>(position * mask_draw.scale + mask_draw.offset, 0.0, 1.0);
}

/// The 88-byte `Vertex`'s `position` (offset 0), for the legacy body.
@vertex
fn vs_mask_legacy(@location(0) position: vec2<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position * mask_draw.scale + mask_draw.offset, 0.0, 1.0);
}

/// A tile's footprint, the unit square in the draw's units (`MaskDraw`
/// built with one "lyon" unit per tile side), as two triangles from the
/// vertex index alone: the land under the tile's ranked fills.
@vertex
fn vs_mask_quad(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    // Corners (0,0) (1,0) (0,1) (0,1) (1,0) (1,1).
    let corner = vec2<f32>(f32((0x32u >> i) & 1u), f32((0x2Cu >> i) & 1u));
    return vec4<f32>(corner * mask_draw.scale + mask_draw.offset, 0.0, 1.0);
}

/// The draw's rank. No blend (integer targets have none): the last draw
/// covering a texel wins.
@fragment
fn fs_mask() -> @location(0) vec4<u32> {
    return vec4<u32>(mask_draw.rank, 0u, 0u, 0u);
}
