// `fill-extrusion` roofs and walls: `render::extrusion_gpu`'s pipeline.
//
// The lighting is MapLibre GL JS's `src/shaders/glsl/fill_extrusion.vertex.glsl`
// (BSD-3), re-expressed in WGSL, per vertex; see `src/render/PROVENANCE.md`
// for what differs and why. `render::extrusion::lit_color` is its CPU mirror
// and must change with it.
//
// `Camera` and `Model` below are copied verbatim from `shader.wgsl`: this
// pipeline binds the tile pipeline's own camera and model groups
// (`TilePipeline::shared_layouts`), and a test compares the two copies.

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

/// `render::extrusion::ExtrusionParams`, 112 bytes; see its doc for each field.
struct ExtrusionParams {
    light_dir: vec3<f32>,
    light_intensity: f32,
    light_color: vec3<f32>,
    vertical_gradient: f32,
    translate_px: vec2<f32>,
    opacity: f32,
    meters_to_world: f32,
    /// `ExtrusionLook`: (face_contrast, ambient_occlusion, occlusion_height_m, tone_jitter).
    look_a: vec4<f32>,
    /// `ExtrusionLook`: (roof_tint, edge_highlight, haze, facade).
    look_b: vec4<f32>,
    haze_color: vec4<f32>,
    /// (haze_start, haze_end, cut_start, cut_end), view depth; end 0 = off.
    bands: vec4<f32>,
    /// (width, height, metres_per_unit, 0): the LOGICAL viewport; times
    /// `camera.device_pixel_ratio` it is framebuffer pixels (`edge_px`).
    /// `z`: a facade layer's metres per vertex `pos` unit, else 0.
    viewport: vec4<f32>,
    /// `xyz`: `light_dir` in unit-sphere space at the camera's centre
    /// (`render::extrusion::globe_light`), MapLibre's `u_lightpos_globe`.
    /// Read only on a globe frame (`model.transition > 0`). `w`:
    /// `ExtrusionLook::night`, read only by the facade.
    globe_light: vec4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var<uniform> model: Model;
/// `render::extrusion::ExtrusionChunk::table`: three texels per feature,
/// `color_from`, `color_to`, `[base_from, height_from, base_to, height_to]`.
@group(2) @binding(0) var extrusion_table: texture_2d<f32>;
@group(3) @binding(0) var<uniform> params: ExtrusionParams;

/// `mesh::COORD_SCALE`: `model.matrix` takes lyon units, MVT times 1000.
const COORD_SCALE: f32 = 1000.0;
/// `shader.wgsl`'s: metres of the unit sphere's radius.
const GLOBE_RADIUS: f32 = 6371008.8;
const PI: f32 = 3.14159265358979323846;
/// `render::extrusion::TABLE_WIDTH`.
const TABLE_WIDTH: u32 = 512u;
/// `render::extrusion::ROOF_TINT`.
const ROOF_TINT: vec3<f32> = vec3<f32>(0.78, 0.83, 0.93);
/// `render::extrusion::FILL_TURN_RAD`'s cosine and sine (100 degrees).
const FILL_COS: f32 = -0.17364818;
const FILL_SIN: f32 = 0.98480775;
const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

/// `render::extrusion::ExtrusionVertex`, 12 bytes.
struct VertexInput {
    @location(0) pos: vec2<i32>,
    /// `(nx, ny, nz, top)`, Snorm8: `top` is 0.0 at the base, 1.0 (127) at the top.
    @location(1) normal_top: vec4<f32>,
    /// `(row, flags)`: the feature's table row and its tone key
    /// (`render::extrusion::tone_key`).
    @location(2) row_flags: vec2<u32>,
};

struct VertexOutput {
    // `@invariant` is LOAD-BEARING: the translucent colour pass draws with
    // depth `LessEqual` against the depth its prepass wrote, from the same
    // geometry through a second pipeline, and passes only where it computes
    // bit-identical z. Without the qualifier a backend may compile the two
    // pipelines' positions differently (GL/WebGL2 at 24-bit depth above
    // all) and silently drop the colour pass's pixels.
    @invariant @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    /// `xy`: the stand-in clip coordinate. `z`: metres of `shading.y` per
    /// framebuffer pixel on a wall (`edge_px`), 0 on a roof. `w`: unused.
    /// Packed here rather than given its own location so the stage keeps
    /// its inter-stage count (WebGL2, iOS: `Limits`' 15 components).
    @location(1) clip_coord: vec4<f32>,
    /// `(metres above base, metres below the roof, wall weight, cut fade)`.
    /// The wall weight is 0 on a roof and, on a wall, how much of the
    /// colour the haze and horizon fade left: the per-fragment occlusion
    /// and edge scale by it, so they fade with the building.
    @location(2) shading: vec4<f32>,
    /// On a globe frame, the point in unit-sphere space (elevated), for
    /// the per-fragment far-side test (`behind_planet`); `w` 1 there, 0
    /// on a flat frame, where the stage never reads it.
    @location(3) sphere: vec4<f32>,
};

/// **How many metres of `shading.y` one framebuffer pixel spans on this
/// wall**: what `fwidth(shading.y)` measured in the fragment stage before,
/// computed here from the geometry instead. A screen-space derivative is
/// evaluated per 2x2 quad by whatever the driver compiled, and on Metal the
/// first draws through a fresh pipeline gave different bits for the same
/// inputs (the 2026-09-30 capture flake); this has no derivative at all.
///
/// `shading.y` is constant along the wall's horizontal lines and falls one
/// metre per `fade * meters_to_world` world units up it. With `a` the pixel
/// motion of one metre of it (up the wall) and `b` the pixel motion along
/// the wall, its screen gradient `g` is the vector with `g . a = 1` and
/// `g . b = 0`: `perp(b) / (perp(b) . a)`. `fwidth` is `|g.x| + |g.y|` of
/// that gradient, so that is what this returns. `clip` is the vertex's
/// clip position before the east-west flip (a flip of x changes no
/// absolute value here).
///
/// `up` and `along` are those two motions in clip space (one metre up the
/// wall, one step along it), so the same formula serves the flat matrix
/// and the globe's: `vs_extrusion` builds them for whichever projection
/// (or blend of the two) the vertex is drawn with.
fn edge_px(clip: vec4<f32>, up: vec4<f32>, along: vec4<f32>) -> f32 {
    let half_px = 0.5 * max(params.viewport.xy, vec2<f32>(1.0)) * camera.device_pixel_ratio;
    let w2 = max(clip.w * clip.w, 1e-12);
    let a = (up.xy * clip.w - clip.xy * up.w) / w2 * half_px;
    let b = (along.xy * clip.w - clip.xy * along.w) / w2 * half_px;
    let b_perp = vec2<f32>(-b.y, b.x);
    let across = abs(dot(b_perp, a));
    return (abs(b_perp.x) + abs(b_perp.y)) / max(across, 1e-12);
}

/// `shader.wgsl`'s `project_to_sphere`, the same arithmetic (see its doc
/// for the half-angle substitution and the antimeridian wrap): `[0, 1]`
/// in-tile position to the unit sphere.
fn project_to_sphere(pos_in_tile: vec2<f32>) -> vec3<f32> {
    let mercator_pos = model.tile_mercator_coords.xy + model.tile_mercator_coords.zw * pos_in_tile;
    let wx = mercator_pos.x - floor(mercator_pos.x);
    let spherical_x = wx * PI * 2.0 + PI;
    let t = exp(PI - (mercator_pos.y * PI * 2.0));
    let t2 = t * t;
    let denom = t2 + 1.0;
    let sin_sy = (t2 - 1.0) / denom;
    let cos_sy = (2.0 * t) / denom;
    return vec3<f32>(sin(spherical_x) * cos_sy, sin_sy, cos(spherical_x) * cos_sy);
}

/// **Whether the planet hides sphere-space point `p` from the eye** -- the
/// far-side test a column needs and the ground gets from
/// `globe_clipping_z`. The ground's test cannot serve a column: it is a
/// plane, and a tall column standing just past the horizon still shows
/// above the limb. So the segment from the eye to `p` is tested against
/// the unit sphere itself, per fragment.
///
/// The eye is on the clipping plane's normal at `1 / -w` from the centre
/// (`render::globe::clipping_plane`: the normal is the unit direction to
/// the camera, `w` minus the reciprocal of its distance). A point below
/// the surface -- the middle of a long roof or wall chord, which cuts
/// under the curve -- is tested as the surface point over it, so a chord
/// never hides itself.
///
/// Hidden when `p` lies beyond its own tangent from the eye (`dot(q, q -
/// eye) > 0`) AND the eye-to-`p` line passes inside the sphere. A ground
/// point satisfies the first exactly on the far side of the horizon
/// plane, so the limb is where the ground's own clip puts it.
fn behind_planet(p: vec3<f32>) -> bool {
    let w = model.clipping_plane.w;
    if (w >= 0.0) {
        return false;
    }
    let eye = model.clipping_plane.xyz / -w;
    let r = length(p);
    let q = p * (max(r, 1.0) / max(r, 1e-6));
    let d = q - eye;
    if (dot(q, d) <= 0.0) {
        return false;
    }
    let te = dot(eye, d);
    let closest2 = dot(eye, eye) - te * te / max(dot(d, d), 1e-20);
    return closest2 < 1.0;
}

fn table_texel(i: u32) -> vec4<f32> {
    return textureLoad(extrusion_table, vec2<i32>(i32(i % TABLE_WIDTH), i32(i / TABLE_WIDTH)), 0);
}

/// A facade layer's `color_to` texel (`overlay::extrude::Facade::texel`):
/// `(floor_m, FACADE_TAG + material, lit_share, seed)`. A colour's green is
/// at most 1, so a table without facades never reads as one.
const FACADE_TAG: f32 = 10.0;

/// **The facade's varyings**, packed into `VertexOutput::sphere` on a flat
/// frame (where that slot is otherwise unused and `w` stays 0):
///
/// - `x`: metres along the wall from the tile's origin (constant up a
///   wall's verticals, so the window columns stay put as the camera moves);
/// - `y`: the building's seed, `0..1000` whole, plus how squarely the wall
///   faces the light in the fraction (`0.98 * (facing * 0.5 + 0.5)`);
/// - `z`: `material * 1000 + floor_dm` whole, plus `0.9 * lit_share`.
///
/// Every value but `x` is the same at the three corners of a wall
/// triangle, so interpolation perturbs the packing by ~1e-4 at most: well
/// inside what `facade` decodes.
fn facade_varyings(pos: vec2<f32>, n: vec3<f32>, fd: vec4<f32>) -> vec4<f32> {
    let along = vec2<f32>(-n.y, n.x);
    let u = dot(pos, along) * params.viewport.z;
    var key = vec2<f32>(0.0, -1.0);
    let key_len = length(params.light_dir.xy);
    if (key_len > 1e-4) {
        key = params.light_dir.xy / key_len;
    }
    let facing = clamp(dot(n.xy, key), -1.0, 1.0);
    let seed = floor(clamp(fd.a, 0.0, 0.999) * 1000.0);
    let material = clamp(floor(fd.g - FACADE_TAG + 0.5), 0.0, 7.0);
    let floor_dm = clamp(floor(fd.r * 10.0 + 0.5), 20.0, 99.0);
    return vec4<f32>(u, seed + 0.98 * (facing * 0.5 + 0.5), material * 1000.0 + floor_dm + 0.9 * clamp(fd.b, 0.0, 1.0), 0.0);
}

fn facade_hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

/// A box `[lo, hi]` on one axis of a cell, antialiased over `w` cell units.
fn facade_band(f: f32, lo: f32, hi: f32, w: f32) -> f32 {
    return smoothstep(lo - w, lo + w, f) * (1.0 - smoothstep(hi - w, hi + w, f));
}

/// **A roof's texture**: tar and gravel, a low-contrast mottle at two
/// scales, and on a 3 m grid a few small soft units (one cell in ten) a
/// shade lighter or darker. Fades out where a cell is under a few pixels.
/// `m_px`: metres per pixel across the roof.
fn roof_detail(rgb: vec3<f32>, input: VertexOutput, m_px: f32) -> vec3<f32> {
    let p = input.sphere.xy;
    let seed = -input.sphere.z;
    let g = p / 3.0;
    let cell = floor(g);
    let f = fract(g);
    let detail = (1.0 - smoothstep(0.25, 1.2, m_px)) * params.look_b.w;
    let h = facade_hash(cell + vec2<f32>(seed, 17.0));
    let w = clamp(m_px / 3.0 * 1.5, 0.02, 0.4);
    // A few small units (one cell in ten), soft-edged, a shade off the roof.
    let unit = facade_band(f.x, 0.25, 0.75, w) * facade_band(f.y, 0.3, 0.7, w) * step(0.9, h);
    let kind = facade_hash(cell + vec2<f32>(seed, 29.0));
    let unit_rgb = rgb * mix(0.8, 1.18, step(0.55, kind));
    // Tar and gravel: a low-contrast mottle at two scales.
    let mottle = 0.95 + 0.05 * facade_hash(floor(p * 0.6) + vec2<f32>(seed, 3.0)) + 0.04 * facade_hash(floor(p * 2.3) + vec2<f32>(seed, 5.0));
    var col = mix(rgb * mix(1.0, mottle, detail), unit_rgb, unit * detail * 0.7);
    // Night: the roofs fall dark.
    col = col * (1.0 - 0.3 * params.globe_light.w);
    return col;
}

/// **The procedural facade** of one wall fragment: the lit wall colour
/// `rgb` with its window grid, and at night its lit windows.
///
/// One window per bay and floor: the floor height is the building's own
/// (PLUTO floors over the roof height, else a default), the bay and the
/// window's share of its cell are the material's. Where a cell is under a
/// few pixels the grid fades to its own mean (the window share of the
/// area), so a far facade neither shimmers nor goes flat. The ground floor
/// is a storefront. Glass is dark and takes the sky (the haze colour,
/// brighter up the building) and a glint from a light it faces.
/// Materials: 0 glass curtain wall, 1 limestone, 2 brick, 3 concrete,
/// 4 cast iron. After Valdrada's window grid and night windows (MIT,
/// M. K. Turkcan: `client/src/world/materials.js` FW14,
/// `client/src/lod/shellMaterial.js`), re-expressed procedurally.
fn facade(rgb: vec3<f32>, input: VertexOutput, d_uv: vec2<f32>) -> vec3<f32> {
    let packed_y = input.sphere.y;
    let packed_z = input.sphere.z;
    let seed = floor(packed_y + 1e-3);
    let facing = clamp(fract(packed_y) / 0.98, 0.0, 1.0) * 2.0 - 1.0;
    let material = floor(packed_z / 1000.0 + 1e-4);
    let rest = packed_z - material * 1000.0;
    let floor_m = floor(rest + 1e-3) / 10.0;
    let lit_share = clamp(fract(rest + 1e-3) / 0.9, 0.0, 1.0);
    let lit_k = input.clip_coord.w;
    let night = params.globe_light.w;
    let strength = params.look_b.w;
    let clear = input.shading.z;
    let above = input.shading.x;
    let storey = max(input.shading.x + input.shading.y, 1e-3);

    // The material's bay (metres) and window box in its cell (x0, x1, y0, y1).
    var bay = 2.4;
    var win = vec4<f32>(0.26, 0.74, 0.24, 0.80);
    var glass = vec3<f32>(0.10, 0.12, 0.15);
    var wall_rgb = rgb;
    if (material < 0.5) {
        bay = 1.52;
        win = vec4<f32>(0.05, 0.95, 0.24, 0.98);
        glass = vec3<f32>(0.11, 0.15, 0.20);
        wall_rgb = mix(rgb, vec3<f32>(0.62, 0.65, 0.68) * lit_k, 0.6);
    } else if (material < 1.5) {
        bay = 2.6;
        win = vec4<f32>(0.30, 0.70, 0.22, 0.80);
    } else if (material < 2.5) {
        bay = 2.2;
        win = vec4<f32>(0.27, 0.73, 0.20, 0.76);
    } else if (material < 3.5) {
        bay = 3.0;
        win = vec4<f32>(0.08, 0.92, 0.32, 0.82);
        glass = vec3<f32>(0.09, 0.11, 0.13);
    } else {
        bay = 3.4;
        win = vec4<f32>(0.14, 0.86, 0.10, 0.86);
    }
    // Each building its own proportions: the bay and the window's size
    // within it vary by the seed, so no two blocks share a rhythm.
    let h0 = facade_hash(vec2<f32>(seed, 11.0));
    let h1 = facade_hash(vec2<f32>(seed, 23.0));
    let h2 = facade_hash(vec2<f32>(seed, 37.0));
    bay = bay * (0.82 + 0.36 * h0);
    let narrow = (h1 - 0.5) * 0.16 * step(0.5, material);
    win = win + vec4<f32>(narrow, -narrow, (h2 - 0.5) * 0.08, (h2 - 0.5) * 0.06);
    // Glass towers: a tint per building (blue, green, bronze, silver).
    if (material < 0.5) {
        let k = facade_hash(vec2<f32>(seed, 51.0));
        var tints = array<vec3<f32>, 4>(vec3<f32>(0.08, 0.13, 0.20), vec3<f32>(0.09, 0.15, 0.14), vec3<f32>(0.16, 0.13, 0.10), vec3<f32>(0.15, 0.16, 0.18));
        glass = tints[min(u32(k * 4.0), 3u)];
    }
    // The ground floor: a storefront, taller and wider glass.
    let ground = 1.0 - step(floor_m * 1.25, above);
    win = mix(win, vec4<f32>(0.06, 0.94, 0.04, 0.82), ground * step(0.5, material));
    let q = vec2<f32>(input.sphere.x / bay, above / max(floor_m, 2.0));
    let cell = floor(q);
    let f = fract(q);
    // `d_uv`: metres per pixel along and up the wall, taken in uniform
    // control flow by `fs_extrusion`.
    let px = max(d_uv / vec2<f32>(bay, max(floor_m, 2.0)), vec2<f32>(1e-4));
    let cell_px = max(px.x, px.y);
    let w = clamp(cell_px * 0.75, 0.002, 0.5);
    let detail = 1.0 - smoothstep(0.2, 0.55, cell_px);
    let area = (win.y - win.x) * (win.w - win.z);
    let in_window = facade_band(f.x, win.x, win.y, w) * facade_band(f.y, win.z, win.w, w);
    // Past a couple of pixels a window, the grid is drawn several bays and
    // floors at a time (`octave`), so a far facade keeps a texture instead
    // of a flat mean, and at night stays points of light.
    let octave = exp2(floor(max(0.0, log2(cell_px * 2.5))));
    let q2 = q / octave;
    let cell2 = floor(q2);
    let f2 = fract(q2);
    let w2 = clamp(cell_px / octave * 0.75, 0.002, 0.5);
    let in_window2 = facade_band(f2.x, win.x, win.y, w2) * facade_band(f2.y, win.z, win.w, w2);
    let win_mask = mix(mix(area, in_window2, 0.45), in_window, detail);

    // Glass: dark, the sky mirrored brighter up the building, a glint.
    let up = clamp(above / storey, 0.0, 1.0);
    // A curtain wall mirrors far more of the sky than a punched window,
    // and each pane a little differently (its tilt, its blind).
    let glassy = 1.0 - step(0.5, material);
    let pane_k = facade_hash(cell + vec2<f32>(seed, 7.0));
    let mirror = (0.22 + 0.30 * up) * mix(1.0, 1.9, glassy) * (0.7 + 0.6 * mix(pane_k, 0.5, glassy * 0.6));
    let sky = params.haze_color.rgb * mirror * (1.0 - 0.85 * night);
    let glint = params.light_color * pow(max(facing, 0.0), 5.0) * mix(0.18, 0.45, glassy) * (1.0 - night);
    let blinds = 0.7 + 0.6 * pane_k;
    var pane = glass * (0.45 + 0.55 * min(lit_k, 1.2)) * blinds + sky + glint;
    // By day a few windows show a lit room or a pale blind.
    pane = mix(pane, vec3<f32>(0.78, 0.74, 0.66) * lit_k * 0.8, step(0.93, facade_hash(cell + vec2<f32>(seed * 1.3, 5.0))) * (1.0 - night) * (1.0 - glassy));
    pane = mix(pane, pane * 0.35, night);
    // The window's recess: its head and one jamb in the wall's shadow,
    // so a punched window reads as set back, not painted on.
    let recess = smoothstep(win.w - 0.16, win.w - 0.02, f.y) * 0.45 + (1.0 - smoothstep(win.x, win.x + 0.06, f.x)) * 0.25;
    pane = pane * (1.0 - recess * detail * step(0.5, material) * (1.0 - night));
    // Sills and floor lines: a masonry cell's edges a little darker.
    let sill = (1.0 - facade_band(f.y, 0.06, 0.94, w)) * 0.12 * detail * step(0.5, material);
    // Weathering: the wall's tone drifts a little bay by bay and floor by
    // floor (soot under the sills, cleaner piers), never one flat colour.
    let grime = 0.92 + 0.12 * facade_hash(floor(q * vec2<f32>(0.5, 0.34)) + vec2<f32>(seed, 3.0)) - 0.06 * smoothstep(0.0, 0.3, f.y) * (1.0 - smoothstep(0.3, 0.6, f.y)) * detail;
    // Masonry: a stone band at each floor line and a cornice under the
    // roof (light on top, its shadow under it).
    let masonry = step(0.5, material) * detail;
    let band = facade_band(f.y, 0.0, 0.07, w) * 0.10 * masonry;
    let to_roof = input.shading.y;
    let cornice = facade_band(to_roof, 0.0, 0.9, w * max(floor_m, 2.0)) * 0.16 - facade_band(to_roof, 0.9, 1.6, w * max(floor_m, 2.0)) * 0.18;
    var col = mix(wall_rgb * (1.0 - sill + band + cornice * step(0.5, material)) * mix(1.0, grime, step(0.5, material)), pane, win_mask);
    // Street canyons: the lower floors of a wall facing the sun stand in
    // the shadow of the block across the street (taken as about the same
    // height, 22 m away), up to where the sun clears it; every wall darkens
    // a little toward the street.
    let sun_len = max(length(params.light_dir.xy), 1e-4);
    let tan_alt = params.light_dir.z / sun_len;
    let across = min(storey * 0.85, 140.0);
    let shade_line = across - 22.0 * tan_alt;
    let in_shade = (1.0 - smoothstep(shade_line - 3.0, shade_line + 3.0, above)) * smoothstep(0.0, 0.25, facing) * (1.0 - night);
    col = col * (1.0 - 0.42 * in_shade);
    col = col * mix(0.78, 1.0, smoothstep(0.0, 28.0, above));

    // Night: a lit share of the windows glows, warm or cool by the window.
    // Where a window is under a couple of pixels the windows are taken
    // several bays and floors at a time (the same chance, the same lit
    // share of the wall), so a far tower is points of light, never a flat
    // warm glow -- Valdrada's night stipple.
    if (night > 0.0) {
        let h = facade_hash(cell2 * vec2<f32>(1.0, 1.7) + vec2<f32>(seed * 0.37, seed + octave));
        let on_now = step(h, lit_share);
        let tint = mix(vec3<f32>(1.0, 0.74, 0.42), vec3<f32>(0.86, 0.92, 1.0), step(0.72, facade_hash(cell2 + vec2<f32>(3.1, seed))));
        let lamp = tint * (0.75 + 0.5 * facade_hash(cell2 + vec2<f32>(seed, 1.3)));
        let glow = lamp * on_now * in_window2 * 1.15;
        col = col * (1.0 - 0.35 * night) + glow * night * (0.6 + 0.4 * ground);
    }
    return mix(rgb, col, strength * clear);
}

@vertex
fn vs_extrusion(input: VertexInput) -> VertexOutput {
    let at = input.row_flags.x * 3u;
    let t = model.blend_t;
    let color_in = mix(table_texel(at), table_texel(at + 1u), t);
    let bh = table_texel(at + 2u);
    // MapLibre: "Sub-terranian floors and ceilings are clamped to ground-level."
    let base = max(0.0, mix(bh.x, bh.z, t));
    let height = max(0.0, mix(bh.y, bh.w, t));
    let top = input.normal_top.w;
    let n = input.normal_top.xyz;

    // Lyon units, the units `model.clip_bounds` is in (`shader.wgsl`
    // `compact_main`). Exact: an i16 times 1000 is an f32 integer.
    let position = vec2<f32>(input.pos) * COORD_SCALE;
    // Past the cover's building data (`cover_plan::building_fade_band`)
    // buildings sink into the ground over a band of view depth measured at
    // their FOOT, reaching nothing at the cut: no hard line where the data
    // stops. The fragment stage discards what has sunk away entirely.
    var fade = 1.0;
    if (params.bands.w > 0.0) {
        let foot_depth = (model.matrix * vec4<f32>(position, 0.0, 1.0)).w;
        fade = 1.0 - smoothstep(params.bands.z, params.bands.w, foot_depth);
    }
    let elevation = mix(base, height, top) * fade;
    // `model.matrix` scales x and y from lyon units to world units and
    // leaves z alone, so the elevation goes in as world units.
    var clip = model.matrix * vec4<f32>(position, elevation * params.meters_to_world, 1.0);
    let fog_depth = clip.w;
    // `fill-extrusion-translate`, as a WORLD-space offset pushed through the
    // camera the way `shader.wgsl` pushes a stroke's corner: a logical pixel
    // is `1 / 2^zoom` world units at the continuous zoom (MapLibre's
    // `pixelsToTileUnits(tile, 1, transform.zoom)`). Not
    // `model.tile_units_per_px`: that one is 0.0 on every flat frame
    // (`GlobeParams::FLAT`). On a globe frame the globe arm below takes no
    // translate (MapLibre's adds it in tile units; nothing that draws on
    // the globe sets one: overlay layers leave it zero).
    clip = clip + camera.view_proj * vec4<f32>(params.translate_px / pow(2.0, camera.zoom), 0.0, 0.0);
    // The Mercator east-west flip every tile draw makes (`shader.wgsl`
    // `vertex_main`, "world +x maps to NDC -x"). It mirrors the triangles,
    // which is why `EXTRUSION_FRONT_FACE` is what the GPU test found.
    clip.x = -clip.x;
    // The edge highlight's two clip-space motions (`edge_px`), flipped
    // with the position they are measured at -- a flip changes no absolute
    // value there, so the flat result is the unflipped one bit for bit.
    // Only when the look asks for the edge (a uniform branch).
    let want_edge = params.look_b.y > 0.0;
    var up_clip = vec4<f32>(0.0);
    var along_clip = vec4<f32>(0.0);
    if (want_edge) {
        up_clip = model.matrix * vec4<f32>(0.0, 0.0, params.meters_to_world * fade, 0.0);
        along_clip = model.matrix * vec4<f32>(-n.y, n.x, 0.0, 0.0);
        up_clip.x = -up_clip.x;
        along_clip.x = -along_clip.x;
    }

    // THE GLOBE (`model.transition > 0`, every frame below z12 with the
    // globe on): MapLibre's `fill_extrusion.vertex.glsl` under `GLOBE`, re-
    // expressed. The footprint vertex goes to the sphere exactly as a tile
    // vertex does (`project_to_sphere`), is raised along the sphere's
    // normal by `elevation / GLOBE_RADIUS` (`interpolateProjectionFor3D`'s
    // `elevatedPos`), and is blended with the Mercator position by the
    // transition -- ALL FOUR components, z included: unlike the ground's
    // `interpolate_projection`, whose z is the far-side clip, a solid
    // needs a real depth to occlude itself. The depth buffer holds nothing
    // else here (the tile pass writes no depth before these draws), and
    // the far side is hidden per fragment by `behind_planet` rather than
    // by depth. Per vertex, so precision is the tile's: positions are
    // tile-relative in `[0, 1]` and the sphere mapping runs in f32 exactly
    // as the ground's does, so a column's foot sits on the ground under it.
    var sphere_out = vec4<f32>(0.0);
    var globe_directional = 0.0;
    if (model.transition > 0.0) {
        let sphere = project_to_sphere(position * model.lyon_to_tile_unit);
        let elevated = sphere * (1.0 + elevation / GLOBE_RADIUS);
        let globe_clip = model.globe_matrix * vec4<f32>(elevated, 1.0);
        // The point's own east and south on the sphere: the tile frame's
        // x and y there (Mercator is conformal), its normal `sphere`.
        let cos_lat = max(length(sphere.xz), 1e-6);
        let east = vec3<f32>(sphere.z, 0.0, -sphere.x) / cos_lat;
        let south = cross(east, sphere);
        let n_sphere = east * n.x + south * n.y + sphere * n.z;
        globe_directional = clamp(dot(n_sphere, params.globe_light.xyz), 0.0, 1.0);
        // One metre up, and one lyon unit along the wall, on the sphere:
        // `d(sphere) / d(mercator)` is `2 pi cos(lat)` along east and south.
        var up_globe = vec4<f32>(0.0);
        var along_globe = vec4<f32>(0.0);
        if (want_edge) {
            let lyon_to_sphere = 2.0 * PI * cos_lat * model.tile_mercator_coords.z * model.lyon_to_tile_unit;
            up_globe = model.globe_matrix * vec4<f32>(sphere * (fade / GLOBE_RADIUS), 0.0);
            along_globe = model.globe_matrix * vec4<f32>((east * -n.y + south * n.x) * lyon_to_sphere, 0.0);
        }
        if (model.transition > 0.999) {
            clip = globe_clip;
            up_clip = up_globe;
            along_clip = along_globe;
        } else {
            clip = mix(clip, globe_clip, model.transition);
            up_clip = mix(up_clip, up_globe, model.transition);
            along_clip = mix(along_clip, along_globe, model.transition);
        }
        sphere_out = vec4<f32>(elevated, 1.0);
    }

    // The building's tone (`ExtrusionLook::tone_jitter`): exactly 1.0 when off.
    let tone = f32(input.row_flags.y) / 65535.0 * 2.0 - 1.0;
    let jittered = vec4<f32>(color_in.rgb * (1.0 + params.look_a.w * tone), color_in.a);

    // MapLibre fill_extrusion.vertex.glsl (BSD-3), re-expressed; see PROVENANCE.md.
    var color = jittered;
    let colorvalue = dot(color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    color = vec4<f32>(color.rgb + vec3<f32>(0.03), color.a);
    var directional = clamp(dot(n, params.light_dir), 0.0, 1.0);
    // MapLibre: "Interpolate dot product result instead of normals and
    // light direction" -- by the transition, so the globe and the flat map
    // light a column alike at the hand-over.
    if (model.transition > 0.0) {
        directional = mix(directional, globe_directional, model.transition);
    }
    directional = mix(1.0 - params.light_intensity, max(1.0 - colorvalue + params.light_intensity, 1.0), directional);
    // R11b: a wall is nz == 0 (not MapLibre's normal.y != 0).
    let wall = select(0.0, 1.0, abs(n.z) < 0.5);
    let vg = params.vertical_gradient;
    let gradient = (1.0 - vg) + vg * clamp((top + base) * pow(height / 150.0, 0.5), mix(0.7, 0.98, 1.0 - params.light_intensity), 1.0);
    directional = directional * mix(1.0, gradient, wall);
    var rgb = clamp(color.rgb * directional * params.light_color, mix(vec3<f32>(0.0), vec3<f32>(0.3), 1.0 - params.light_color), vec3<f32>(1.0));

    // `ExtrusionLook::face_contrast`: the two-light model
    // (`render::extrusion::wall_shade`, its CPU mirror), mixed in.
    let contrast = params.look_a.x;
    if (contrast > 0.0) {
        let key_len = length(params.light_dir.xy);
        var key = vec2<f32>(0.0, -1.0);
        if (key_len > 1e-4) {
            key = params.light_dir.xy / key_len;
        }
        let fill = vec2<f32>(FILL_COS * key.x - FILL_SIN * key.y, FILL_SIN * key.x + FILL_COS * key.y);
        let wall_shade = 0.32 + 0.62 * (dot(n.xy, key) * 0.5 + 0.5) + 0.18 * (dot(n.xy, fill) * 0.5 + 0.5);
        let shade = mix(1.0, wall_shade, wall);
        let enhanced = clamp(jittered.rgb * shade * params.light_color, vec3<f32>(0.0), vec3<f32>(1.0));
        rgb = mix(rgb, enhanced, contrast);
    }
    // `ExtrusionLook::roof_tint`: a roof shifts toward a cool grey at its
    // own luminance, so it never matches warm land.
    let roof_tint = params.look_b.x * (1.0 - wall);
    if (roof_tint > 0.0) {
        let k = dot(rgb, LUMA) / dot(ROOF_TINT, LUMA);
        rgb = mix(rgb, min(ROOF_TINT * k, vec3<f32>(1.0)), roof_tint);
    }

    // `ExtrusionLook::facade` (`facade_varyings`): a flat frame only, on a
    // layer whose table carries facades (`color_to.g >= FACADE_TAG`).
    var facade_light = 0.0;
    if (params.look_b.w > 0.0 && model.transition <= 0.0 && params.viewport.z > 0.0) {
        let fd = table_texel(at + 1u);
        if (fd.g >= FACADE_TAG) {
            let albedo = max(dot(jittered.rgb, LUMA), 1e-3);
            facade_light = clamp(dot(rgb, LUMA) / albedo, 0.0, 2.0);
            sphere_out = facade_varyings(vec2<f32>(input.pos), n, fd);
            if (wall < 0.5) {
                // A roof's varyings: metres east and south in the tile,
                // and its seed as a negative marker (`roof_detail`).
                let pm = vec2<f32>(input.pos) * params.viewport.z;
                sphere_out = vec4<f32>(pm.x, pm.y, -1.0 - floor(clamp(fd.a, 0.0, 0.999) * 1000.0), 0.0);
                // Roofs: tar, gravel and membrane, darker and duller than
                // the walls, a tone per building.
                let k = fract(fd.a * 7.31);
                let roof = mix(vec3<f32>(0.36, 0.35, 0.34), vec3<f32>(0.52, 0.52, 0.5), k) * facade_light;
                rgb = mix(rgb, roof, 0.55 * params.look_b.w);
            }
        }
    }

    // `ExtrusionLook::haze`, from the look-at point outward; then the
    // cut's own fade leans into it, so a sinking building also recedes.
    var clear = 1.0;
    if (params.bands.y > 0.0) {
        var haze = smoothstep(params.bands.x, params.bands.y, fog_depth);
        haze = max(haze, 1.0 - fade) * params.look_b.z;
        rgb = mix(rgb, params.haze_color.rgb, haze);
        clear = 1.0 - haze;
    }
    if (camera.fog_range.y > 0.0) {
        let fog = smoothstep(camera.fog_range.x, camera.fog_range.y, fog_depth);
        rgb = mix(rgb, camera.fog_color.rgb, fog);
        clear = clear * (1.0 - fog);
    }

    var out: VertexOutput;
    out.position = clip;
    // The alpha is `fill-extrusion-opacity` alone; the colour's own alpha
    // is ignored, as in MapLibre. Straight, not premultiplied: the pipeline
    // blends with ALPHA_BLENDING (src·a + dst·(1-a)), which equals
    // MapLibre's premultiplied `(rgb·o, o)` under ONE / ONE_MINUS_SRC_ALPHA.
    out.color = vec4<f32>(rgb, params.opacity);
    var edge_m_per_px = 0.0;
    if (wall > 0.5 && want_edge) {
        edge_m_per_px = edge_px(clip, up_clip, along_clip);
    }
    out.clip_coord = vec4<f32>(
        (position - model.clip_bounds.xy) / (model.clip_bounds.zw - model.clip_bounds.xy),
        edge_m_per_px,
        facade_light,
    );
    let storey = max(height - base, 0.0);
    out.shading = vec4<f32>(top * storey, (1.0 - top) * storey, wall * clear, fade);
    out.sphere = sphere_out;
    return out;
}

@fragment
fn fs_extrusion(input: VertexOutput) -> @location(0) vec4<f32> {
    // The facade's screen footprint, before any branch (derivatives need
    // uniform control flow): metres per pixel along and up the wall.
    let d_uv = vec2<f32>(abs(dpdx(input.sphere.x)) + abs(dpdy(input.sphere.x)), abs(dpdx(input.shading.x)) + abs(dpdy(input.shading.x)));
    // The stand-in clip (shader.wgsl fs_main). It MUST be discard: an
    // alpha-0 fragment would still write depth and hide buildings behind.
    if (any(input.clip_coord.xy < vec2<f32>(0.0)) || any(input.clip_coord.xy > vec2<f32>(1.0))) {
        discard;
    }
    // Sunk away past the cover's building data (`bands.zw`).
    if (input.shading.w <= 0.0) {
        discard;
    }
    // Round the far side of the planet (`behind_planet`): a globe frame only.
    if (input.sphere.w > 0.5 && behind_planet(input.sphere.xyz)) {
        discard;
    }
    var rgb = input.color.rgb;
    let wall = input.shading.z;
    // `ExtrusionLook::facade`: a facade layer's walls on a flat frame.
    if (params.look_b.w > 0.0 && input.sphere.w < 0.5 && input.sphere.z > 1.0 && wall > 0.0) {
        rgb = facade(rgb, input, d_uv);
    }
    if (params.look_b.w > 0.0 && input.sphere.w < 0.5 && input.sphere.z < -0.5) {
        rgb = roof_detail(rgb, input, d_uv.x);
    }
    // `ExtrusionLook::ambient_occlusion`: darker toward the wall's base.
    let ao = params.look_a.y * wall;
    if (ao > 0.0) {
        rgb = rgb * mix(1.0 - ao, 1.0, smoothstep(0.0, params.look_a.z, input.shading.x));
    }
    // `ExtrusionLook::edge_highlight`: a line along the wall's top, about
    // 1.5 px wide whatever the distance, where it meets the roof.
    let edge = params.look_b.y * wall;
    if (edge > 0.0) {
        // `edge_px`, per vertex: no screen-space derivative.
        let w = max(input.clip_coord.z, 1e-4);
        let line = 1.0 - smoothstep(w, 2.0 * w, input.shading.y);
        rgb = mix(rgb, vec3<f32>(1.0), edge * line);
    }
    return vec4<f32>(rgb, input.color.a);
}
