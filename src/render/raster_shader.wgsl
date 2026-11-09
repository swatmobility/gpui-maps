// **A raster tile: the tile's own square, projected exactly as a fill is,
// sampling a texture instead of carrying a colour.**
//
// # This file is a PORT of `shader.wgsl`'s vertex stage, and must stay one
//
// `project_to_sphere`, `globe_clipping_z` and `interpolate_projection`
// below are transcribed from `shader.wgsl` (which is itself a port of
// MapLibre GL JS's `_projection_globe.vertex.glsl`, BSD-3). They are
// character-for-character the same arithmetic, deliberately.
//
// The alternative -- a raster-specific projection "equivalent to" the fill
// one -- is the failure mode this whole design exists to avoid. A raster
// basemap whose sphere disagreed with the vector layers' sphere by even
// one ULP at the limb would put the coastline off the coast, and the
// disagreement would grow and shrink with the globe transition, so it
// would look like a mysterious wobble rather than like two projections.
// Sharing the arithmetic makes the agreement structural.
//
// WGSL has no `#include`, which is why this is a copy and not a reference.
// `a_raster_vertex_lands_where_a_fill_vertex_lands` is what keeps the copy
// honest: it renders the same tile through both pipelines and compares the
// painted silhouettes.
//
// # What is NOT ported from the reference's own raster shader
//
// - `u_image_warp` and `u_coords_top`/`u_coords_bottom`
//   (`raster.vertex.glsl:1-6`, `:23-32`). Those exist for `ImageSource` --
//   an image pinned to four arbitrary corner coordinates, possibly a
//   general quadrilateral. A TILE is an axis-aligned square, for which the
//   warp degenerates to the identity (`bilinearImageWarp = [0, 0, 1]`,
//   `raster_program.ts:16`). We have no `ImageSource`, so the term is
//   omitted rather than hard-coded to its identity.
// - `u_buffer_scale` (`raster.vertex.glsl:40`), which the reference itself
//   notes is "ever something else than a constant 1" only for raster-DEM
//   (`raster_program.ts:67-71`). We have no DEM.
// - `u_tl_parent` / `u_scale_parent` / `u_fade_t` and the second sampler
//   (`raster.fragment.glsl:1-20`) -- the cross-fade against a parent tile.
//   See `RASTER_PROPERTIES_DELIBERATELY_IGNORED` in `style_spec` for why
//   `raster-fade-duration` is a tile-lifecycle feature this engine has no
//   parts for.
// - The colour adjustments (`raster.fragment.glsl:31-47`): spin,
//   saturation, contrast, brightness. Each is a few lines; none is on the
//   path to a satellite basemap.

struct Camera {
    view_proj: mat4x4<f32>,
    zoom: f32,
    device_pixel_ratio: f32,
    // `shader.wgsl`'s horizon fade, applied to imagery the same way: per
    // vertex, off when `fog_range.y == 0.0`.
    fog_range: vec2<f32>,
    fog_color: vec4<f32>,
}

// **Byte-identical to `shader.wgsl`'s `Model`**, because this pipeline
// binds the very same `ModelUniform` buffer `TilePipeline` writes, through
// the very same bind group layout. Nothing here may reorder, resize or
// reinterpret a field: `render::gpu::ModelUniform` is the authority and
// `the_model_uniform_is_16_byte_aligned_and_its_declared_need_matches_reality`
// is what pins it. Fields this shader never reads are still declared, for
// the same reason.
struct Model {
    matrix: mat4x4<f32>,
    globe_matrix: mat4x4<f32>,
    tile_mercator_coords: vec4<f32>,
    clipping_plane: vec4<f32>,
    blend_t: f32,
    dash_scale: f32,
    transition: f32,
    lyon_to_tile_unit: f32,
    tile_units_per_px: f32,
    sun_direction: vec3<f32>,
    night_brightness: f32,
    clip_bounds: vec4<f32>,
}

// The raster layer's own paint, and the only thing in this pipeline that
// is not shared with the fill path.
//
// FRAGMENT-visible, unlike `Model` -- which is precisely why
// `raster-opacity` lives here and not in `Model`. `Model`'s bind group is
// declared `ShaderStages::VERTEX` only (`gpu.rs:973`), so a fragment stage
// cannot read it, and widening that layout would change a uniform four
// other pipelines share.
/// **One `f32`, and no explicit padding.** WGSL rounds a uniform struct up
/// to a 16-byte stride on its own, so this is 16 bytes and matches
/// `render::raster_pipeline::RasterPaintUniform` exactly.
///
/// A `_pad: vec3<f32>` was here first and was wrong in a way worth
/// recording: `vec3<f32>` has a 16-byte ALIGNMENT, so the pad started at
/// offset 16 and made the struct 32 bytes, while the Rust side stayed 16.
/// wgpu caught it at draw time -- "the buffer bound at binding index 2 is
/// bound with size 16 where the shader expects 32" -- but the lesson is
/// that padding a WGSL uniform by hand is a way to ADD a mismatch, not to
/// avoid one.
struct RasterPaint {
    /// `paint.raster-opacity`, already resolved at this frame's zoom.
    /// MapLibre's `u_opacity` (`raster.fragment.glsl:2`, `:28`).
    opacity: f32,
}

@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var<uniform> model: Model;
@group(2) @binding(0) var tile_texture: texture_2d<f32>;
@group(2) @binding(1) var tile_sampler: sampler;
@group(2) @binding(2) var<uniform> paint: RasterPaint;

const PI: f32 = 3.14159265358979323846;

/// The sphere's radius in metres. `shader.wgsl:127`, unchanged; read only
/// by the elevation term, which is always zero here as it is there.
const GLOBE_RADIUS: f32 = 6371008.8;

/// `render::raster_mesh::POLE_NORTH` / `POLE_SOUTH`. A vertex whose `pole`
/// field is below this is a pole vertex; see `vs_main`.
const POLE_THRESHOLD: f32 = -0.5;
/// Between `POLE_NORTH` (-1) and `POLE_SOUTH` (-2): above it is north.
const POLE_NORTH_THRESHOLD: f32 = -1.5;

/// A point in `[0, 1]` in-tile space, projected onto the unit sphere.
///
/// **Ported verbatim from `shader.wgsl:167`.** See that function's doc
/// comment for why there is no `atan`, `sin` or `cos` of the latitude
/// here: the half-angle substitution for the inverse Gudermannian is the
/// reference's own (`_projection_globe.vertex.glsl:54-93`).
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


/// The horizon side-of-plane test. **Ported verbatim from
/// `shader.wgsl:244`.** Not a distance -- see that function's comment.
fn globe_clipping_z(sphere_pos: vec3<f32>) -> f32 {
    return 1.0 - (dot(sphere_pos, model.clipping_plane.xyz) + model.clipping_plane.w);
}

/// Blends the globe and Mercator projections of one vertex by
/// `model.transition`. **Ported verbatim from `shader.wgsl:272`**,
/// including the divergence from MapLibre in how the far-side clip is
/// ramped (ramping toward the FAR PLANE rather than scaling the clip-space
/// z, which would move the horizon instead of fading it). That divergence
/// is explained at length in `shader.wgsl` and is reproduced here rather
/// than re-derived, because a raster tile that ramped its clip differently
/// from the fill over it would disagree with that fill in a thin annulus
/// just inside the limb -- exactly where it would be blamed on the
/// texture.
fn interpolate_projection(flat_position: vec4<f32>, sphere_pos: vec3<f32>, elevation: f32) -> vec4<f32> {
    let elevated_pos = sphere_pos * (1.0 + elevation / GLOBE_RADIUS);
    var globe_position = model.globe_matrix * vec4<f32>(elevated_pos, 1.0);
    globe_position.z = globe_clipping_z(elevated_pos) * globe_position.w;

    if (model.transition > 0.999) {
        return globe_position;
    }

    var result = globe_position;
    let xyw = mix(flat_position.xyw, globe_position.xyw, model.transition);
    result.x = xyw.x;
    result.y = xyw.y;
    result.w = xyw.z;

    let z_globeness_threshold = 0.2;
    let clip_ramp = clamp(
        (model.transition - z_globeness_threshold) / (1.0 - z_globeness_threshold),
        0.0,
        1.0
    );
    result.z = result.w * (1.0 - clip_ramp * (1.0 - globe_clipping_z(elevated_pos)));
    return result;
}

struct VertexInput {
    /// Lyon units, as `render::raster_mesh::RasterVertex::position`.
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) pole: f32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    /// The horizon fade: `rgb` is the fog colour, `a` how far toward it
    /// this vertex has faded. ONE inter-stage slot for both, because the
    /// `Camera` group is vertex-visible only and the fragment stage needs
    /// the colour too. Zero everywhere when the fade is off.
    @location(1) fog: vec4<f32>,
    /// This vertex inside `model.clip_bounds`, `0..1` on each axis: the
    /// stand-in clip, as `shader.wgsl`'s `clip_coord`. An unclipped tile
    /// (`TileClip::WHOLE`) puts every vertex at ~0.5.
    @location(2) clip_coord: vec2<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.uv = input.uv;
    out.clip_coord = (input.position - model.clip_bounds.xy) / (model.clip_bounds.zw - model.clip_bounds.xy);

    // The flat projection, exactly as `shader.wgsl:579` does it: the
    // COMBINED tile+camera matrix, computed per tile in f64 on the CPU by
    // `render::combined_tile_matrix`. A raster vertex carries no stroke
    // extrusion, so there is nothing here that needs the world-space
    // detour the fill shader takes for strokes.
    var clip = model.matrix * vec4<f32>(input.position, 0.0, 1.0);

    // THE HORIZON FADE, exactly as `shader.wgsl` computes it: from the
    // Mercator clip `w` before any globe blend, `smoothstep` over
    // `fog_range`. The cover cut of imagery can land well inside the vector
    // one (`raster_cover_zoom` can be the camera's zoom + 2), and a shell
    // hands this frame the nearer of the two cuts, so without this the
    // imagery's missing tiles showed as a hard edge.
    out.fog = vec4<f32>(0.0);
    if (camera.fog_range.y > 0.0) {
        out.fog = vec4<f32>(camera.fog_color.rgb, smoothstep(camera.fog_range.x, camera.fog_range.y, clip.w));
    }

    // `shader.wgsl:714`: the globe projection carries +x to screen right
    // already, so the blended result must not be flipped again. Same line,
    // same reason -- getting this wrong mirrors the globe east for west,
    // which on a satellite basemap is a very legible bug.
    clip.x = -clip.x;

    if (model.transition <= 0.0) {
        // Pure Mercator. Return before `project_to_sphere` is evaluated,
        // for the same cost reason `shader.wgsl` returns here.
        //
        // A pole-cap vertex keeps the POSITION of the tile edge it grew
        // from (`raster_mesh::build`), so on this path every cap triangle
        // is exactly degenerate and rasterizes to nothing. That is the
        // same property `shader.wgsl` relies on to skip the reference's
        // pole fade, and it is why no cap can bleed onto a flat map.
        out.clip_position = clip;
        return out;
    }

    let pos_in_tile = input.position * model.lyon_to_tile_unit;

    // A pole vertex is RELOCATED, not repositioned: its coordinate is a
    // tile-edge coordinate and `project_to_sphere` would put it at
    // Mercator's +/-85.051 edge. `shader.wgsl:729` does exactly this, and
    // `render::raster_mesh::RasterVertex::pole` carries the same marker
    // values `render::subdivide::POLE_NORTH_MARKER` sets for fills.
    var sphere_pos = vec3<f32>(0.0, 0.0, 0.0);
    let is_pole = input.pole < POLE_THRESHOLD;
    if (is_pole) {
        sphere_pos = vec3<f32>(0.0, select(-1.0, 1.0, input.pole > POLE_NORTH_THRESHOLD), 0.0);
        // The cap vertex sits ON the tile edge it grew from, so its clip y
        // is that edge's -- exactly 0 or 1 for a clip box on the pole row,
        // and outside [0, 1] for one that is not. Snap the f32 round-off
        // of the first case to the edge, so a stand-in for a pole-row
        // child keeps its cap and one for any other child still drops it.
        let cy = out.clip_coord.y;
        out.clip_coord.y = select(cy, clamp(cy, 0.0, 1.0), cy > -1e-3 && cy < 1.001);
    } else {
        sphere_pos = project_to_sphere(pos_in_tile);
    }

    // Elevation 0.0: no elevation source in this crate, as in
    // `shader.wgsl`.
    out.clip_position = interpolate_projection(clip, sphere_pos, 0.0);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // `CLAMP_TO_EDGE` is on the SAMPLER, not expressed here -- see
    // `render::raster_pipeline`, and `draw_raster.ts:121` for the
    // reference binding the same thing. It is what keeps a linear filter
    // at u=0 or u=1 from reaching for a texel outside the tile.
    var color = textureSample(tile_texture, tile_sampler, in.uv);
    // THE STAND-IN CLIP (`map_view::raster_stand_ins`): an ancestor drawn
    // for one missing tile paints only that tile's footprint, so it never
    // composites a second time under a ready sibling. After the sample, so
    // the sample stays in uniform control flow. `discard`, as `shader.wgsl`
    // explains, not a zero alpha.
    if (any(in.clip_coord < vec2<f32>(0.0)) || any(in.clip_coord > vec2<f32>(1.0))) {
        discard;
    }

    // The reference un-premultiplies before compositing
    // (`raster.fragment.glsl:21-26`) because its textures are uploaded
    // with `premultiplyAlpha`. Ours are not: `raster::decode` hands
    // straight RGBA8 to `write_texture` with no alpha handling anywhere,
    // so the texel is already straight-alpha and dividing by alpha here
    // would brighten every semi-transparent pixel. Omitted deliberately,
    // not overlooked.

    // `raster-opacity`, the one paint property this pipeline honours.
    // `raster.fragment.glsl:28`.
    color.a = color.a * paint.opacity;

    // The horizon fade. Skipped outright when the vertex stage wrote zero,
    // so an unfogged frame returns the texel it always did.
    if (in.fog.a > 0.0) {
        color = vec4<f32>(mix(color.rgb, in.fog.rgb, in.fog.a), color.a);
    }

    // Straight alpha out. The pipeline blends with
    // `BlendState::ALPHA_BLENDING` -- `SrcAlpha / OneMinusSrcAlpha`, the
    // same state `TilePipeline` uses (`gpu.rs:1046`) -- which expects a
    // straight-alpha source. So only the alpha is scaled and the RGB is
    // returned untouched, exactly the shape `shader.wgsl`'s fill path
    // returns.
    return color;
}
