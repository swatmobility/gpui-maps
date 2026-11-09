use crate::geo::{lon_lat_to_tile_xy, TileId, TILE_SIZE};

/// Zoom-independent world size: world coordinates always span [0, WORLD_SIZE)
/// on both axes, regardless of camera zoom. Matches `geo::TILE_SIZE` so a
/// zoom-0 tile occupies the whole world.
pub const WORLD_SIZE: f32 = TILE_SIZE as f32;

/// Vertical field of view (60 degrees), used by the eye/target/view/
/// projection chain (`Camera::eye_target_up`, `Camera::view_projection`,
/// `frustum_horizon_corner_ray`), which does its matrix construction in f64
/// -- see `Camera::view_projection`'s doc comment for why. This is the f64
/// value directly from `f64::consts::FRAC_PI_3` (the correctly-rounded f64
/// value of pi/3), not a widened f32 constant, so it isn't double-rounded.
///
/// `pub(crate)` rather than private because `render::globe` builds a *second*,
/// completely separate perspective matrix for the sphere (MapLibre does the
/// same -- `vertical_perspective_transform.ts:439-447` calls `mat4.perspective`
/// afresh rather than reusing the Mercator one). That matrix must use the very
/// same vertical FOV as `view_projection_f64`, or the globe and the Mercator
/// fallback would disagree on how big the world is at the moment they blend.
pub(crate) const FOV_Y_RADIANS_F64: f64 = std::f64::consts::FRAC_PI_3;
/// Absolute ceiling on the dynamically-computed far distance (see
/// `far_distance`), so a near-horizontal ray at extreme pitch can't blow the
/// frustum/tile-selection radius up to infinity.
const MAX_FAR: f32 = 1_000_000.0;
/// `near_distance` scales the near clip plane to this fraction of the
/// camera's eye-to-target distance (capped at [`NEAR_CEILING`]).
///
/// **0.05, since buildings (fill-extrusion, Task 7); it was 0.5.** At pitch
/// 60 with this 60° FOV the bottom screen row's ray is 30° off nadir and
/// meets the ground at a view depth of exactly the eye height,
/// `0.5 · distance()` -- so a near plane at `0.5 · distance()` touched the
/// ground at the bottom edge, and every building standing there was nearer
/// than it and was cut into slanted shards (captured in
/// `docs/measurements/2026-09-29-building-extrusion.md`). A flat map never
/// showed it: nothing flat rises off z = 0. At 0.05 the bottom-edge ground
/// hit is 10 near-plane distances out. Raising a point there by `h` cuts
/// its view depth by `h · cos 60° = 0.5 h`, so a building up to
/// `0.9 · distance()` tall (1.8x the eye height) is whole there
/// (`at_max_pitch_the_near_plane_clears_a_tower_at_the_bottom_edge` pins
/// `0.8 · distance()`). MapLibre GL JS uses `height / 50`, about
/// `distance() / 43` at this FOV, so ours is the same order and slightly
/// better conditioned. The unproject round trip, which an even smaller
/// fraction once broke through f32 error in `view_projection().inverse()`,
/// passes at 0.05. Side effect: the [`NEAR_CEILING`] of 1.0 now binds only
/// for `distance() >= 20` (it was `>= 2`), so the near plane of low- and
/// mid-zoom frames moved too -- harmless for flat geometry, which sits at
/// the look-at depth far beyond it.
const NEAR_FRACTION: f32 = 0.05;
/// Upper bound on `near_distance`, equal to the old fixed `NEAR = 1.0`.
/// Below the old bug's zoom threshold, `distance()` is large enough that
/// `distance() * NEAR_FRACTION` alone would push near comfortably *above*
/// 1.0 -- capping it here means near-clip behavior (and float precision)
/// for already-correct low/mid zoom stays bit-for-bit identical to before
/// this fix, and near only starts shrinking below 1.0 once zoom pushes
/// `distance() * NEAR_FRACTION` below it -- i.e. right around the old
/// blank-screen cliff, which is exactly where shrinking needs to start.
const NEAR_CEILING: f32 = 1.0;
/// Absolute floor under `near_distance`, purely to keep the value nonzero
/// (and hence the projection matrix non-degenerate) in edge cases where
/// `distance()` itself is ~0 -- e.g. a zero-height viewport during startup,
/// or a pathological future zoom level far beyond anything the UI's zoom
/// clamp currently allows.
const NEAR_MIN: f32 = 1e-4;
/// [`NEAR_MIN`] for the low eye camera. `1e-4` world units is 7.8 m on
/// the ground at the equator: at an eye camera's zoom (z21-24) the
/// fraction above falls far under it, so that floor -- not
/// [`NEAR_FRACTION`] -- set the near plane, and it sliced a car 6-8 m
/// ahead of a close chase camera clean through (and every rival within
/// 7.8 m of the in-car eye). An eye pose floors at a millimetre's order
/// instead, so its near plane is the fraction's: ~0.5 m from a 1-1.8 m
/// eye, inside the bottom edge's ground (1.1 m deep in the car).
const NEAR_MIN_EYE: f32 = 1e-10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub center_lon: f64,
    pub center_lat: f64,
    pub zoom: f64,
    /// Radians. 0 = looking straight down.
    pub pitch: f32,
    /// Radians.
    pub bearing: f32,
    pub viewport_width_px: f32,
    pub viewport_height_px: f32,
    /// Vertical field of view, radians: [`Self::DEFAULT_FOV_Y`] (60
    /// degrees) on every map unless the host widens it -- the low eye
    /// camera (`camera_control::EyeCamera::fov_y_deg`) does. Read it
    /// through [`Self::fov_y_rad`], which holds it to
    /// [`Self::MIN_FOV_Y`]..=[`Self::MAX_FOV_Y`]. Every consumer of the
    /// projection (the matrix, the camera distance, the cover, picking,
    /// labels, the sky, the 3D pipelines) reads it from here, so a camera
    /// with a wider FOV is one camera everywhere.
    pub fov_y: f64,
}

impl Camera {
    /// The default vertical FOV: 60 degrees, bit-identical to the old
    /// shared constant.
    pub const DEFAULT_FOV_Y: f64 = FOV_Y_RADIANS_F64;
    /// The narrowest FOV a camera takes: 10 degrees.
    pub const MIN_FOV_Y: f64 = 10.0 * std::f64::consts::PI / 180.0;
    /// The widest: 120 degrees (a wider vertical FOV puts the horizon
    /// corner rays behind the eye at ordinary pitches).
    pub const MAX_FOV_Y: f64 = 120.0 * std::f64::consts::PI / 180.0;

    /// [`Self::fov_y`], held to [`Self::MIN_FOV_Y`]..=[`Self::MAX_FOV_Y`]; a
    /// non-finite value reads as the default. The default passes through
    /// untouched, so a default camera projects bit for bit as before.
    #[must_use]
    pub fn fov_y_rad(&self) -> f64 {
        Self::clamped_fov_y(self.fov_y)
    }

    /// `fov_y` radians held to [`Self::MIN_FOV_Y`]..=[`Self::MAX_FOV_Y`],
    /// the default for a non-finite one: what [`Self::fov_y_rad`] reads.
    #[must_use]
    pub fn clamped_fov_y(fov_y: f64) -> f64 {
        if fov_y.is_finite() {
            fov_y.clamp(Self::MIN_FOV_Y, Self::MAX_FOV_Y)
        } else {
            Self::DEFAULT_FOV_Y
        }
    }

    /// `tan(fov_y / 2)`: the focal length's inverse, per half-height.
    #[must_use]
    pub fn tan_half_fov(&self) -> f64 {
        (self.fov_y_rad() / 2.0).tan()
    }

    /// The focal length in logical pixels: `viewport_height_px / 2 /
    /// tan(fov_y / 2)`.
    #[must_use]
    pub fn focal_px(&self) -> f64 {
        f64::from(self.viewport_height_px) / 2.0 / self.tan_half_fov()
    }

    /// The point the camera looks at, on the ground plane (z=0), in world units.
    ///
    /// Returns `f32` for callers that genuinely only need a coarse world
    /// position (this module's own direction/orientation tests) -- a thin
    /// downcast of `world_center_f64`, not an independent f32 computation,
    /// so it stays correctly-rounded.
    ///
    /// Do NOT use this in anything that subtracts two world positions and
    /// scales the difference back to screen pixels. World coordinates are
    /// zoom-independent and ~128-256 in magnitude, so one f32 ulp here is
    /// ~1.5e-5 world units -- which is `1.5e-5 * 2^zoom` SCREEN PIXELS
    /// (see this module's docs: screen_pixels_per_world_unit = 2^zoom).
    /// That is 0.25px at z14, 4px at z18, 16px at z20. The
    /// precision-sensitive paths (`eye_target_up`, `view_projection`, and
    /// `MapView::zoom_towards_point`'s zoom-about-a-point math) all use
    /// `world_center_f64`/`unproject_to_ground_f64` instead, so they never
    /// round-trip through this narrower type.
    pub fn world_center(&self) -> glam::Vec3 {
        self.world_center_f64().as_vec3()
    }

    /// f64-precision version of `world_center`. `WORLD_SIZE` is a
    /// zoom-independent constant (~256), so `world_center`'s result always
    /// sits at f32 magnitude ~100-200 regardless of zoom -- meaning a tiny
    /// real change in `center_lon`/`center_lat` (e.g. from the zoom-ease
    /// animation nudging the camera by a sub-pixel amount at high zoom) can
    /// round away to nothing in f32, then suddenly jump once it crosses the
    /// next representable f32 step. That reads as shaking. Feeding this
    /// f64 value into `eye_target_up`/`view_projection`'s matrix
    /// construction instead (only narrowing the *final* combined matrix to
    /// f32, in `view_projection`) keeps the camera responding smoothly to
    /// real, continuous camera motion at every zoom level -- the same
    /// technique MapLibre GL JS uses (its CPU-side matrix math runs in
    /// plain, implicitly-f64 JS numbers, narrowing to `Float32Array` only
    /// at the point a matrix is handed to WebGL; see e.g.
    /// `mercator_transform.ts`'s `new Float32Array(tileMatrix)`).
    pub(crate) fn world_center_f64(&self) -> glam::DVec3 {
        let (x, y) = lon_lat_to_tile_xy(self.center_lon, self.center_lat, 0.0);
        glam::DVec3::new(x * crate::geo::TILE_SIZE, y * crate::geo::TILE_SIZE, 0.0)
    }

    /// Distance from the camera to `world_center()` along the view direction,
    /// chosen so that world-space scale matches `geo`'s existing
    /// screen_pixels_per_world_unit = 2^zoom convention. f64 so callers
    /// building the eye/target frame (`eye_target_up`) can combine it with
    /// `world_center_f64` without an intermediate f32 narrowing.
    ///
    /// `pub(crate)` rather than private because this is exactly MapLibre's
    /// `cameraToCenterDistance` (`transform_helper.ts:584`,
    /// `0.5 / tan(halfFov) * height`), differing only in unit: theirs is in
    /// pixels, ours in world units, and world units are pixels / 2^zoom. The
    /// globe transform in `render::globe` needs it twice -- once to push the
    /// sphere away from the eye, once for the horizon clipping plane -- and
    /// re-deriving it there from `viewport_height_px` and `zoom` would be the
    /// same formula written twice, free to drift.
    pub(crate) fn distance(&self) -> f64 {
        let scale = 2f64.powf(self.zoom);
        self.viewport_height_px as f64 / (2.0 * (self.fov_y_rad() / 2.0).tan() * scale)
    }

    /// How high the eye is over the ground, metres: the camera distance
    /// times `cos(pitch)`, at the look-at point's scale. What the low eye
    /// camera (`camera_control::eye_target`) solves the zoom for.
    #[must_use]
    pub fn eye_height_m(&self) -> f64 {
        self.distance() * f64::from(self.pitch).cos() * crate::camera_control::metres_per_world_unit(self.center_lat)
    }

    /// (eye, target, up) — the camera's rigid frame. Built in a bearing=0
    /// reference frame, then rotated as a whole by bearing, which keeps `up`
    /// exactly perpendicular to the view direction regardless of bearing (a
    /// rigid rotation preserves orthogonality, avoiding roll artifacts a
    /// bearing-only `up` formula would introduce at combined pitch+bearing).
    ///
    /// Computed entirely in f64: `target` sits at f32 magnitude ~100-200
    /// (see `world_center_f64`), and `eye` is `target` plus a
    /// zoom-dependent offset that shrinks toward zero as zoom increases --
    /// exactly the "large base value plus a vanishingly small delta"
    /// pattern that loses the delta entirely in f32. Doing the addition (and
    /// the bearing rotation applied to it) in f64 keeps that delta
    /// resolvable at any zoom the UI reaches.
    pub(crate) fn eye_target_up(&self) -> (glam::DVec3, glam::DVec3, glam::DVec3) {
        let target = self.world_center_f64();
        let d = self.distance();
        let pitch = self.pitch as f64;
        let bearing_rotation = glam::DMat3::from_rotation_z(self.bearing as f64);
        let eye_offset_ref = glam::DVec3::new(0.0, d * pitch.sin(), d * pitch.cos());
        let up_ref = glam::DVec3::new(0.0, -pitch.cos(), pitch.sin());
        let eye = target + bearing_rotation * eye_offset_ref;
        let up = bearing_rotation * up_ref;
        (eye, target, up)
    }

    /// f64-precision version of `view_projection` — the pre-narrowing result.
    /// `view_projection()` itself just narrows this to f32 for GPU upload (see
    /// its own doc comment); `unproject_to_ground` inverts THIS f64 version
    /// instead of the narrowed one, since inverting an already-narrowed matrix
    /// amplifies whatever precision was lost narrowing it, on top of the
    /// inversion's own numerical sensitivity.
    pub(crate) fn view_projection_f64(&self) -> glam::DMat4 {
        let (eye, target, up) = self.eye_target_up();
        let aspect = self.viewport_width_px as f64 / self.viewport_height_px as f64;
        let near = near_distance(self) as f64;
        let far = far_distance(self) as f64;
        let proj = glam::dcamera::rh::proj::directx::perspective(self.fov_y_rad(), aspect, near, far);
        let view = glam::dcamera::rh::view::look_at_mat4(eye, target, up);
        proj * view
    }

    /// The camera's combined view-projection matrix, for the GPU uniform
    /// buffer. All of the actual matrix construction (eye/target/up,
    /// perspective projection, look-at view matrix, and their
    /// multiplication) happens in f64 via `eye_target_up` and `glam`'s f64
    /// `dcamera` module -- the large-magnitude cancellation this involves
    /// (subtracting eye/target coordinates at world-space magnitude
    /// ~100-200 down to on-screen-scale deltas) is exactly the arithmetic
    /// that was previously silently losing precision in f32, per the
    /// module's high-zoom-jitter investigation. `.as_mat4()` below is the
    /// *only* narrowing step in this whole chain -- it downcasts the
    /// already-fully-computed combined matrix to f32 right before handing
    /// it back to callers, which all feed it straight into
    /// `to_cols_array_2d()` for the GPU uniform buffer (a WGSL shader
    /// uniform, `write_camera_uniform`/`create_camera_bind_group` in
    /// `render::gpu`, has no f64 equivalent to upload into). This mirrors
    /// MapLibre GL JS's own approach: CPU-side matrix math in (implicitly
    /// f64) plain JS numbers, narrowed to `Float32Array` only at the point a
    /// finished matrix is handed to WebGL.
    pub fn view_projection(&self) -> glam::Mat4 {
        self.view_projection_f64().as_mat4()
    }

    /// [`Self::view_projection_f64`] with its own `near`/`far` clip planes
    /// (world units): the same eye, look and FOV, so a point lands on the
    /// same pixel, only its depth is remapped. What a first-person "near
    /// pass" projects with (`render::model::ModelNearPass`): three.js's
    /// viewmodel recipe, a second camera sharing the main one's pose with
    /// a tighter near/far, drawn after the scene into a cleared depth
    /// buffer. `near` is clamped above zero and `far` past it.
    #[must_use]
    pub fn view_projection_near_far_f64(&self, near: f64, far: f64) -> glam::DMat4 {
        let (eye, target, up) = self.eye_target_up();
        let aspect = self.viewport_width_px as f64 / self.viewport_height_px as f64;
        let near = if near.is_finite() { near.max(1e-12) } else { 1e-12 };
        let far = if far.is_finite() { far.max(near * 2.0) } else { near * 1e6 };
        let proj = glam::dcamera::rh::proj::directx::perspective(self.fov_y_rad(), aspect, near, far);
        proj * glam::dcamera::rh::view::look_at_mat4(eye, target, up)
    }

    /// The near clip plane's distance from the eye, metres at the look-at
    /// point's scale: what slices a 3D model (a car) that comes closer.
    #[must_use]
    pub fn near_m(&self) -> f64 {
        f64::from(near_distance(self)) * crate::camera_control::metres_per_world_unit(self.center_lat)
    }

    /// **Where a point `alt_m` metres over `lon`/`lat` is drawn** on a flat
    /// (Mercator) frame: logical pixels, and its view depth in metres --
    /// the model, extrusion and trips pipelines' own projection, for a host
    /// that needs to know where its 3D object lands (a chase camera framing
    /// a car). `None` at or behind the near plane (the GPU would clip it).
    #[must_use]
    pub fn project_lon_lat_alt(&self, lon: f64, lat: f64, alt_m: f64) -> Option<(f32, f32, f64)> {
        let (x, y) = lon_lat_to_tile_xy(lon, lat, 0.0);
        let m = crate::camera_control::metres_per_world_unit(lat);
        let world = glam::DVec4::new(x * crate::geo::TILE_SIZE, y * crate::geo::TILE_SIZE, alt_m / m, 1.0);
        let clip = self.view_projection_f64() * world;
        if clip.w <= 0.0 || clip.z < 0.0 {
            return None;
        }
        let (sx, sy) = project_clip_to_screen(self, clip.as_vec4())?;
        Some((sx, sy, clip.w * crate::camera_control::metres_per_world_unit(self.center_lat)))
    }
}

/// The world-space near clip plane distance for this camera's actual GPU
/// projection matrix (`view_projection`), and the floor `far_distance`
/// clamps to so near/far can never invert.
///
/// A single fixed world-space constant (the old `const NEAR: f32 = 1.0`)
/// cannot work here: `Camera::distance()` -- the eye-to-look-at-target
/// distance -- shrinks as `viewport_height_px / (2*tan(FOV_Y/2)*2^zoom)`, so
/// it *keeps shrinking as zoom increases* while a fixed near plane does not.
/// At a typical ~900px viewport that crossover happens around zoom 9.6-10:
/// past it, `distance()` drops below the fixed 1.0, meaning the eye ends up
/// closer to its own look-at point than the near clip plane sits, so the
/// entire ground plane (z=0, where every tile's geometry lives) falls inside
/// the clipped region and the map renders blank. Scaling near down as a
/// fixed *fraction* of `distance()` instead keeps the same relative margin
/// between "how close the eye actually is" and "where clipping starts" at
/// every zoom level, low or high -- it's zoom-invariant in the way the old
/// constant wasn't. `ground_radius` (below) already had to learn this same
/// lesson for tile-selection-radius purposes; this closes the same gap for
/// the actual GPU projection matrix.
fn near_distance(camera: &Camera) -> f32 {
    let floor = if is_eye_pose(camera) { NEAR_MIN_EYE } else { NEAR_MIN };
    (camera.distance() * NEAR_FRACTION as f64).clamp(floor as f64, NEAR_CEILING as f64) as f32
}

/// Whether `camera` is a low eye camera's pose (`camera_control::EyeCamera`):
/// pitched past [`crate::camera_control::MAX_PITCH`], which only a map
/// that opted into the eye camera can reach. Every other camera keeps its
/// near plane bit for bit.
fn is_eye_pose(camera: &Camera) -> bool {
    camera.pitch > crate::camera_control::MAX_PITCH + 1e-4
}

/// The frustum ray that reaches furthest across the ground plane: the
/// horizon-corner ray, combining the full vertical half-FOV tilt
/// (toward the horizon under pitch) with the full horizontal half-FOV tilt
/// (toward one screen edge — the choice of left/right doesn't matter, the
/// setup is symmetric). Returns (eye, direction), both in f64 -- this reuses
/// `Camera::eye_target_up`'s f64 frame directly rather than round-tripping
/// it through f32 first, since `far_distance`/`ground_radius` (below) do the
/// same eye-to-ground-plane subtraction that motivated widening
/// `eye_target_up` in the first place.
fn frustum_horizon_corner_ray(camera: &Camera) -> (glam::DVec3, glam::DVec3) {
    let (eye, target, up) = camera.eye_target_up();
    let forward = (target - eye).normalize();
    let right = forward.cross(up).normalize();

    let half_fov_y = camera.fov_y_rad() / 2.0;
    let aspect = camera.viewport_width_px as f64 / camera.viewport_height_px as f64;
    let half_fov_x = (aspect * half_fov_y.tan()).atan();

    let tilted_toward_horizon = (forward * half_fov_y.cos() + up * half_fov_y.sin()).normalize();
    let dir = (tilted_toward_horizon * half_fov_x.cos() + right * half_fov_x.sin()).normalize();
    (eye, dir)
}

/// Distance from the camera eye to where `frustum_horizon_corner_ray` hits the
/// ground plane (z=0), capped at `MAX_FAR`. Used both as the camera's far
/// clip plane (tight at low pitch, appropriately larger at high pitch —
/// following maplibre-rs's `furthest_distance` technique, `view_state.rs`)
/// and, via `ground_radius`, as the basis for the bearing-invariant tile
/// selection radius in `pitch_aware_tile_bbox`.
pub fn far_distance(camera: &Camera) -> f32 {
    let (eye, dir) = frustum_horizon_corner_ray(camera);
    if dir.z >= -1e-4 {
        // Looking at or above the horizon (only possible at extreme pitch):
        // this ray never hits the ground plane going forward.
        return MAX_FAR;
    }
    let t = -eye.z / dir.z;
    t.clamp(near_distance(camera) as f64, MAX_FAR as f64) as f32
}

/// Where the eye sits over the ground plane: the eye position with its
/// height dropped, in world units. The point a tilted camera's nearest
/// ground is measured from. At pitch 0 it is the look-at point itself, so
/// "nearest the camera" and "nearest the centre" agree there.
pub(crate) fn eye_ground_point(camera: &Camera) -> glam::DVec2 {
    let (eye, _, _) = camera.eye_target_up();
    eye.truncate()
}

/// `(near, far)` as `view_projection` builds them, for a caller that tests
/// many tiles against one frustum and should not rebuild either per tile.
pub(crate) fn near_far(camera: &Camera) -> (f32, f32) {
    (near_distance(camera), far_distance(camera))
}

/// Ground-plane (xy) distance from the look-at target to where the frustum's
/// farthest-reaching ray hits the ground. Invariant to bearing by
/// construction (bearing only rotates the whole camera frame around the
/// target), unlike an axis-aligned bounding box of the same footprint —
/// this is what lets us select tiles without maplibre-rs's forced bearing
/// clamp (see the design doc's maplibre-rs section for why theirs needs one).
fn ground_radius(camera: &Camera) -> f32 {
    let (eye, dir) = frustum_horizon_corner_ray(camera);
    // Deliberately does NOT reuse `far_distance`'s near-clip floor
    // (`near_distance`): that floor exists only so `far_distance` stays
    // valid as a camera far-clip plane. For tile selection there's no
    // reason a ground intersection distance couldn't legitimately be very
    // small (or, at the limit, 0) as zoom increases, so this clamps at 0.0
    // rather than at any positive near-plane-derived value.
    let t = if dir.z >= -1e-4 {
        MAX_FAR as f64
    } else {
        (-eye.z / dir.z).clamp(0.0, MAX_FAR as f64)
    };
    let intersection = eye + dir * t;
    let target = camera.world_center_f64();
    (intersection - target).truncate().length() as f32
}

/// Maps a tile's pre-scaled (×1000) local MVT coordinates (0..extent*1000) into
/// world space, positioning the tile at its correct location and size for its zoom.
pub fn tile_model_matrix(tile: TileId, extent: f32) -> glam::Mat4 {
    let tiles_across = 2f32.powi(i32::from(tile.z));
    let tile_world_size = WORLD_SIZE / tiles_across;
    let scale = tile_world_size / (1000.0 * extent);
    let origin = glam::Vec3::new(
        tile.x as f32 * tile_world_size,
        tile.y as f32 * tile_world_size,
        0.0,
    );
    glam::Mat4::from_scale_rotation_translation(
        glam::Vec3::new(scale, scale, 1.0),
        glam::Quat::IDENTITY,
        origin,
    )
}

/// Pixels per lyon input unit for this tile at the camera's **floored**
/// zoom -- the scale the fragment shader needs to turn a vertex's
/// `line_distance` into a distance in screen pixels for dash phase.
///
/// Derived from the same two facts the rest of the pipeline uses:
/// `tile_model_matrix`'s `scale` is world units per lyon unit, and the vertex
/// shader's `world_units_per_px` is `1 / 2^camera.zoom` -- except that here
/// the exponent is `floor(camera.zoom)`, not the continuous zoom.
///
/// The floor is deliberate and mirrors MapLibre exactly. Its `u_tileratio`
/// (`line_program.ts`'s `calculateTileRatio`) is computed from
/// `transform.tileZoom == Math.floor(transform.zoom)`, which pins the dash
/// pattern in TILE units for the whole of a zoom level: the geometry grows on
/// screen by up to 2x as you zoom in while the pattern's phase does not, so
/// the dashes stretch up to 2x and reset at the integer boundary. That
/// stepped look is what real Mapbox shows, and matching it is a deliberate
/// product decision (see `.superpowers/sdd/2026-08-22-line-dasharray/task-8-brief.md`);
/// an earlier revision of this function used the continuous zoom and was
/// reversed on purpose. Do not "fix" it back.
///
/// Note this affects only the dash PERIOD. The line's rendered thickness is
/// still continuously interpolated in the vertex shader -- see `shader.wgsl`,
/// which keeps the blended half-width for extrusion and uses the unblended
/// (floored-zoom) width only for the dash mask, exactly as MapLibre's
/// `LineFloorwidthProperty` does.
///
/// On its own this quantisation makes the scale DOUBLE at the boundary, which
/// repositions the whole pattern along the line (dashes teleport rather than
/// travel). MapLibre hides that with its a/b crossfade, and so do we: `fs_main`
/// evaluates the dash mask at this scale AND at twice it, mixing by
/// `fract(camera.zoom)` so both sides of every integer zoom agree exactly. See
/// `shader.wgsl`'s `dash_mask` / `fs_main`. Nothing here changes for that --
/// the doubling is done shader-side, from this one floored scale.
pub fn dash_scale(camera: &Camera, tile: TileId, extent: f32) -> f32 {
    let tiles_across = 2f32.powi(i32::from(tile.z));
    let tile_world_size = WORLD_SIZE / tiles_across;
    let world_units_per_lyon_unit = tile_world_size / (1000.0 * extent);
    world_units_per_lyon_unit * 2f32.powf(camera.zoom.floor() as f32)
}

/// The model uniform's `blend_t` for a tile whose paint pair was baked at
/// `baked_zoom`: how far the camera is past that bake zoom, CLAMPED to the
/// pair's two endpoints rather than wrapped at the next integer.
///
/// A mesh stores every zoom-dependent paint value as `(v@b, v@b+1)` for its
/// bake bucket `b`, and `shader.wgsl` mixes the two by `blend_t`. For a tile
/// baked at the camera's own bucket this is exactly `camera_zoom.fract()`
/// (bit for bit: for a positive `x`, `x.fract()` is `x - x.trunc()`, and
/// `trunc` is `floor`). For a STALE tile -- one still waiting for its
/// re-bake after an integer crossing, or a stand-in -- `fract()` selects
/// the wrong end: at z16.03 a mesh baked at 15 would draw its z15 widths and
/// colours (`t = 0.03`) until the re-bake lands, then jump to z16's. The
/// clamp draws it at `v@16` instead, which is where the fresh mesh starts,
/// so the crossing is continuous whenever the re-bake lands.
///
/// Every shell calls this for every drawn vector tile; see
/// `tests/stale_mesh_blend_parity.rs`.
pub fn tile_blend_t_for_baked_zoom(camera_zoom: f64, baked_zoom: f64) -> f32 {
    (camera_zoom - baked_zoom).clamp(0.0, 1.0) as f32
}

/// `dash_scale` for the caller-supplied overlay rather than a tile.
///
/// The rule is `dash_scale`'s, unchanged, including the deliberate
/// `floor(camera.zoom)` -- read its doc comment for why the quantisation is
/// there and why it must not be "fixed" to the continuous zoom. Only the
/// first factor differs: world units per lyon unit comes from the overlay's
/// model matrix (`overlay_model_matrix_f64`) instead of a tile's, which makes
/// it `1 / OVERLAY_UNITS_PER_WORLD` alone -- NO `COORD_SCALE` factor, because
/// the overlay tessellates at `OVERLAY_COORD_SCALE` (`1.0`), so a lyon unit
/// IS a local grid unit -- independent of any tile grid, which is why this
/// takes no `TileId` or `extent`.
///
/// Only the dash PERIOD depends on this. An overlay feature with no
/// `line-dasharray` has `dash_runs == vec4(0)`, and `shader.wgsl`'s `fs_main`
/// skips the mask entirely for those.
pub fn overlay_dash_scale(camera: &Camera) -> f32 {
    let world_units_per_lyon_unit = 1.0 / crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;
    (world_units_per_lyon_unit * 2f64.powf(camera.zoom.floor())) as f32
}

/// f64-precision version of `tile_model_matrix`, same scale+translation
/// logic. Used only as an intermediate by `combined_tile_matrix` -- kept
/// private, `tile_model_matrix`'s existing f32 public API is unchanged and
/// still used directly wherever only the tile-local transform is needed.
fn tile_model_matrix_f64(tile: TileId, extent: f32) -> glam::DMat4 {
    let tiles_across = 2f64.powi(i32::from(tile.z));
    let tile_world_size = WORLD_SIZE as f64 / tiles_across;
    let scale = tile_world_size / (1000.0 * extent as f64);
    let origin = glam::DVec3::new(
        tile.x as f64 * tile_world_size,
        tile.y as f64 * tile_world_size,
        0.0,
    );
    glam::DMat4::from_scale_rotation_translation(
        glam::DVec3::new(scale, scale, 1.0),
        glam::DQuat::IDENTITY,
        origin,
    )
}

/// Combines this tile's model transform with the camera's view-projection
/// matrix into one matrix, in f64, narrowing to f32 only at the very end --
/// mirrors MapLibre GL JS's real `calculatePosMatrix` technique (verified
/// against their source, `src/geo/projection/mercator_transform.ts`:
/// `mat4.multiply` in `Float64Array`, narrowed to `Float32Array` only for
/// the final WebGL upload). Avoids the GPU ever separately rounding a
/// "tile-local -> world" step and a "world -> clip" step as two
/// independent f32 operations, which is what let those two roundings
/// compound into visible vertex-lattice quantization at high zoom (measured
/// up to ~17px at z20 before this fix).
pub fn combined_tile_matrix(camera: &Camera, tile: TileId, extent: f32) -> glam::Mat4 {
    (camera.view_projection_f64() * tile_model_matrix_f64(tile, extent)).as_mat4()
}

/// The overlay's local grid to world space, in f64.
///
/// The same shape as `tile_model_matrix_f64`: a uniform scale from lyon
/// units (local grid units x `OVERLAY_COORD_SCALE`, which is `1.0`, so a
/// lyon unit IS a local grid unit) to world units, then a translation to the
/// overlay's origin. The scale MUST stay uniform -- `shader.wgsl` treats a
/// stroke vertex's `normal` as a world-space direction, which is only true
/// when local and world space differ by a uniform scale.
fn overlay_model_matrix_f64(origin_world: (f64, f64)) -> glam::DMat4 {
    // No COORD_SCALE factor: overlay geometry is tessellated with
    // `OVERLAY_COORD_SCALE` (1.0), so a lyon unit IS a local grid unit.
    // This scale and that one are a pair -- change either and the overlay
    // renders at a thousand times the wrong size.
    let scale = 1.0 / crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;
    glam::DMat4::from_scale_rotation_translation(
        glam::DVec3::new(scale, scale, 1.0),
        glam::DQuat::IDENTITY,
        glam::DVec3::new(origin_world.0, origin_world.1, 0.0),
    )
}

/// The overlay's combined model + view-projection matrix, multiplied in f64
/// and narrowed once -- the same technique and the same reason as
/// `combined_tile_matrix`.
pub fn combined_overlay_matrix(camera: &Camera, origin_world: (f64, f64)) -> glam::Mat4 {
    (camera.view_projection_f64() * overlay_model_matrix_f64(origin_world)).as_mat4()
}

/// A conservative lon/lat bounding box covering the visible ground area, as
/// the enclosing square of a bearing-invariant circular footprint (radius
/// from `ground_radius`) centered on the camera's look-at point. A circle is
/// invariant to bearing by construction, unlike a rectangle that only widens
/// with pitch (which misses a rotated footprint's corners at nonzero bearing
/// — the exact defect that forced maplibre-rs to hard-clamp bearing to
/// +/-30 degrees; see the design doc). Used for tile *selection* (what to
/// fetch) — render-time frustum culling (`tile_in_frustum`) then trims the
/// actual draw set.
pub fn pitch_aware_tile_bbox(camera: &Camera) -> (f64, f64, f64, f64) {
    let radius_tiles = ground_radius(camera) as f64 / WORLD_SIZE as f64;

    let center_x_tiles = camera.center_lon / 360.0 + 0.5;
    let lat_rad = camera.center_lat.to_radians();
    let center_y_tiles =
        (1.0 - ((lat_rad.tan() + 1.0 / lat_rad.cos()).ln()) / std::f64::consts::PI) / 2.0;

    let min_x_tiles = center_x_tiles - radius_tiles;
    let max_x_tiles = center_x_tiles + radius_tiles;
    let min_y_tiles = center_y_tiles - radius_tiles;
    let max_y_tiles = center_y_tiles + radius_tiles;

    let tile_xy_to_lonlat = |x: f64, y: f64| -> (f64, f64) {
        let lon = x * 360.0 - 180.0;
        let lat_rad = (std::f64::consts::PI * (1.0 - 2.0 * y)).sinh().atan();
        (lon, lat_rad.to_degrees())
    };
    let (min_lon, max_lat) = tile_xy_to_lonlat(min_x_tiles, min_y_tiles);
    let (max_lon, min_lat) = tile_xy_to_lonlat(max_x_tiles, max_y_tiles);
    (min_lon, min_lat, max_lon, max_lat)
}

/// Conservative render-time visibility test: is any part of `tile` within the
/// camera's view frustum? Tests the tile's four ground-plane corners against
/// the view-projection clip planes (a superset test — no false negatives, may
/// have false positives for tiles just outside frame, which is acceptable for
/// a culling optimization).
pub fn tile_in_frustum(tile: TileId, camera: &Camera) -> bool {
    let tiles_across = 2f32.powi(i32::from(tile.z));
    let tile_world_size = WORLD_SIZE / tiles_across;
    let min = glam::Vec3::new(tile.x as f32 * tile_world_size, tile.y as f32 * tile_world_size, 0.0);
    let max = min + glam::Vec3::splat(tile_world_size);

    let vp = camera.view_projection();
    let corners = [
        glam::Vec3::new(min.x, min.y, 0.0),
        glam::Vec3::new(max.x, min.y, 0.0),
        glam::Vec3::new(min.x, max.y, 0.0),
        glam::Vec3::new(max.x, max.y, 0.0),
    ];

    // If every corner is simultaneously outside the same clip-space bound
    // (e.g. all four have clip.x > clip.w), the tile is fully outside that
    // plane. If no single plane rejects all four corners, treat it as visible.
    let clip: Vec<glam::Vec4> = corners
        .iter()
        .map(|c| vp * glam::Vec4::new(c.x, c.y, c.z, 1.0))
        .collect();

    let all_outside = |pred: &dyn Fn(&glam::Vec4) -> bool| clip.iter().all(pred);

    if all_outside(&|c| c.x > c.w) { return false; }
    if all_outside(&|c| c.x < -c.w) { return false; }
    if all_outside(&|c| c.y > c.w) { return false; }
    if all_outside(&|c| c.y < -c.w) { return false; }
    // `c.w` here is not an arbitrary clip-space quantity -- for this
    // (DirectX-convention, right-handed) perspective matrix, `w` is set
    // exactly to the eye-space view distance along the view direction, i.e.
    // the same world-space "distance from the eye" number `near_distance`/
    // `far_distance` themselves compute. So the near/far bounds tested here
    // must be the *same* near/far the matrix `vp` (built moments above via
    // `camera.view_projection()`) was actually constructed with, or this
    // culling test would disagree with the real projection -- e.g. rejecting
    // a tile as "behind the near plane" using a stale fixed threshold when
    // the live projection's near plane has since moved much closer to the
    // eye (exactly what a fixed `NEAR` constant used to do once zoom pushed
    // `near_distance` below it).
    if all_outside(&|c| c.w <= near_distance(camera)) { return false; }
    if all_outside(&|c| c.w >= far_distance(camera)) { return false; }
    true
}

/// Casts a ray from the camera through the given screen pixel (window-
/// relative, matching `Camera::viewport_width_px`/`viewport_height_px`) and
/// intersects it with the ground plane (world z=0), returning the world-
/// space point under that pixel — or `None` if the ray points above the
/// horizon and never reaches the ground going forward.
///
/// `f32` result, for callers that only need a coarse world position. Anything
/// that subtracts this from another world position and scales the difference
/// back to screen pixels must call `unproject_to_ground_f64` instead — see
/// `Camera::world_center`'s doc comment for the pixel cost of an f32 ulp at
/// world scale.
pub fn unproject_to_ground(camera: &Camera, screen_x: f32, screen_y: f32) -> Option<glam::Vec3> {
    unproject_to_ground_f64(camera, screen_x, screen_y).map(|p| p.as_vec3())
}

/// f64-precision `unproject_to_ground`. The ray math below was already all
/// f64 (it inverts `view_projection_f64`); this is where it now *stays* f64
/// instead of being narrowed on the way out.
pub fn unproject_to_ground_f64(camera: &Camera, screen_x: f32, screen_y: f32) -> Option<glam::DVec3> {
    let ndc_x_seen = (screen_x / camera.viewport_width_px) as f64 * 2.0 - 1.0;
    let ndc_y_seen = 1.0 - (screen_y / camera.viewport_height_px) as f64 * 2.0;
    // Undo the vertex shader's post-projection clip.x flip (see
    // shader.wgsl's fix for the east-west mirror) so this matches the raw,
    // unflipped convention `Camera::view_projection()` itself uses.
    let raw_ndc_x = -ndc_x_seen;
    let raw_ndc_y = ndc_y_seen;

    let inv_vp = camera.view_projection_f64().inverse();
    let near = inv_vp * glam::DVec4::new(raw_ndc_x, raw_ndc_y, 0.0, 1.0);
    let far = inv_vp * glam::DVec4::new(raw_ndc_x, raw_ndc_y, 1.0, 1.0);
    if near.w == 0.0 || far.w == 0.0 {
        return None;
    }
    let p0 = near.truncate() / near.w;
    let p1 = far.truncate() / far.w;
    let dir = p1 - p0;
    if dir.z >= -1e-6 {
        return None; // ray doesn't descend toward the ground
    }
    let t = -p0.z / dir.z;
    Some(p0 + dir * t)
}

/// How much GROUND one screen pixel covers at `ground`, as a multiple of
/// how much it covers at the camera's own look-at point.
///
/// `1.0` everywhere at pitch 0, and *exactly* 1.0 — a ground point's
/// view-space depth is the same all over an unpitched viewport, because the
/// ground plane is then parallel to the image plane. That is what lets
/// `crate::overlay::hit` fold this factor into a pixel tolerance without
/// changing a single unpitched answer.
///
/// Under pitch it is the whole story. `1 / 2^zoom` world units per pixel
/// (`shader.wgsl`'s `world_units_per_px`, and `geo`'s
/// screen_pixels_per_world_unit convention) is a *center* figure: it is what
/// a pixel covers at the look-at point, where the eye sits `distance()`
/// away. A perspective divide scales everything by `1/w`, so a pixel at
/// depth `w` covers `w / distance()` times as much ground — a factor that
/// runs from well under 1 at the bottom of a pitched viewport to 2-3x near
/// the horizon. Anything that converts a SCREEN distance into a world
/// distance and then compares it against real geometry has to multiply by
/// this, or the screen distance it thinks it is enforcing is the wrong one
/// everywhere except the middle row of pixels.
///
/// This is MapLibre's `maxPitchScaleFactor`, evaluated per query point
/// rather than at its worst case. Theirs (`mercator_transform.ts`,
/// `transform_state.cpp`) is literally `topPoint[3] /
/// cameraToCenterDistance`: the clip-space `w` of the ground point under the
/// TOP corner of the viewport, over the eye-to-center distance. It is a
/// deliberate upper bound, because MapLibre spends it on `queryPadding` —
/// inflating tile-space bounding boxes during a broad phase that runs
/// *before* it knows which point of the query geometry a candidate feature
/// is near (`tile_manager.ts`'s `tilesIn`, `feature_index.ts`'s `query`), so
/// it must not under-estimate for any point in the query. We have no broad
/// phase to be conservative for: `HitIndex` is a flat `Vec` walked back to
/// front for ONE cursor position, which is already in hand. So the same
/// ratio is taken at that position and is exact there, instead of at the
/// corner and loose everywhere else.
///
/// Computed geometrically (a dot product against the view axis) rather than
/// by pushing the point through `view_projection_f64` — the two agree to
/// f64 round-off, which
/// `the_pitch_scale_factor_is_the_projection_matrixs_own_w_ratio` pins, and
/// the dot product costs no 4x4 inverse. f64 throughout, for the reason
/// `Camera::world_center_f64` gives: `eye` is a large world-magnitude base
/// plus a zoom-shrinking delta, and the subtraction below is exactly the
/// cancellation that loses the delta in f32.
///
/// ONE SCALAR, for a correction that is not isotropic. Screen *x* scales
/// exactly as `1/w`, so a horizontal offset is converted exactly. A vertical
/// one is additionally foreshortened by the angle between the view ray and
/// the ground, which this does not carry — the same simplification MapLibre
/// makes, and the only one available to a caller whose tolerance is a
/// world-space *circle*. Under-correcting the vertical is the right way to
/// be wrong: it errs toward the unpitched answer rather than past it.
pub(crate) fn pitch_scale_factor_at_ground(camera: &Camera, ground: glam::DVec3) -> f64 {
    let (eye, target, _up) = camera.eye_target_up();
    let to_target = target - eye;
    let distance = to_target.length();
    // A zero-height viewport during startup makes `distance()` zero; a
    // degenerate camera must not turn a tolerance into NaN or infinity.
    // `is_finite` first, because a NaN fails every ordered comparison and
    // would otherwise fall through to divide by itself.
    if !distance.is_finite() || distance <= 0.0 {
        return 1.0;
    }
    // FLATTENED onto z=0, not taken as given. `unproject_to_ground_f64`
    // computes its z as `p0.z + dir.z * (-p0.z / dir.z)`, which lands within
    // an ulp or two of zero rather than ON it -- and carrying that crumb
    // into the dot product below is the difference between returning 1.0 at
    // pitch 0 and returning 1.0 - 1e-12. The exact 1.0 is worth having: it
    // is what lets `overlay::hit::tolerance_world` promise bit-identical
    // unpitched arithmetic instead of merely indistinguishable arithmetic.
    // Flattening is also simply what the parameter means -- a point on the
    // ground plane.
    let on_ground = glam::DVec3::new(ground.x, ground.y, 0.0);
    let depth = (on_ground - eye).dot(to_target / distance);
    // A point at or behind the eye has no meaningful on-screen scale.
    // `unproject_to_ground_f64` never returns one (it only intersects rays
    // going forward), so this is a guard against a hand-built argument, not
    // a case the picking path reaches.
    if !depth.is_finite() || depth <= 0.0 {
        return 1.0;
    }
    depth / distance
}

/// Projects a world point to a window pixel through *exactly* the pipeline the
/// GPU uses: `Camera::view_projection()` followed by `shader.wgsl`'s
/// post-projection `out.clip_position.x = -out.clip_position.x` flip, then the
/// standard wgpu NDC->framebuffer mapping (NDC x=-1 is the left edge, NDC y=+1
/// is the *top* edge) into gpui's y-down window coordinates.
///
/// Test-only since the symbol overlay moved to `combined_tile_matrix` +
/// `project_clip_to_screen`: nothing in the shipping render path projects a
/// bare *world* point any more.
///
/// Used by tests, deliberately written independently of
/// `unproject_to_ground` -- forward direction, with the flip applied
/// explicitly rather than "undone" -- so round-trip tests pin unprojection to
/// what is actually rendered, not merely to unprojection's own internal sign
/// choices.
///
/// Returns `None` for a point behind the camera's near plane (see
/// `project_clip_to_screen`).
///
/// Note for symbol/label placement: do **not** compose this with
/// `tile_model_matrix` to project a tile-local point. That reintroduces the
/// two-independent-f32-roundings problem `combined_tile_matrix` exists to
/// avoid; use `combined_tile_matrix` + `project_clip_to_screen` instead.
#[cfg(test)]
pub(crate) fn project_world_to_screen(camera: &Camera, world: glam::Vec3) -> Option<(f32, f32)> {
    project_clip_to_screen(camera, camera.view_projection() * world.extend(1.0))
}

/// Projects an f64 WORLD-space point (the `[0, WORLD_SIZE)` Mercator square
/// `overlay::geom::lon_lat_to_world` produces) to a window pixel, through the
/// same clip-to-screen mapping every other symbol anchor goes through.
///
/// The f64 sibling of `project_world_to_screen`, and the world-space sibling
/// of `map_view::project_symbol_anchor_with_matrix`: same shape -- multiply by
/// a matrix, hand the clip position to `project_clip_to_screen` -- with the
/// multiply kept in f64 (`view_projection_f64`, not `view_projection`) and
/// narrowed only once, at the very end. A world coordinate at z=14 is in the
/// millions of units, where an f32 ulp is already a visible fraction of a
/// pixel; that is the same reasoning `combined_tile_matrix` carries for
/// tile-local anchors, applied to a point that has no tile to be local to.
///
/// Returns `None` for a point behind the camera's near plane (see
/// `project_clip_to_screen`), which is reachable at any nonzero pitch for a
/// feature beyond the horizon.
///
/// `#[allow(dead_code)]` for the same reason
/// `symbol_pass::project_symbol_anchor` carries it: every production caller
/// projects more than one point and so takes the `_with_matrix` form, leaving
/// this a legitimate one-off convenience API that nothing in this crate
/// currently needs. Kept as production code rather than `#[cfg(test)]`-gated
/// because it is the readable spelling, and because the exactness test below
/// is only meaningful if both forms are real.
#[allow(dead_code)]
pub(crate) fn project_world_to_screen_f64(camera: &Camera, world: (f64, f64)) -> Option<(f32, f32)> {
    project_world_to_screen_f64_with_matrix(camera, &camera.view_projection_f64(), world)
}

/// `project_world_to_screen_f64` with the view-projection matrix supplied by
/// the caller instead of rebuilt per call.
///
/// The world-space sibling of `symbol_pass::project_symbol_anchor_with_matrix`,
/// and it exists for the same reason: `view_projection_f64` constructs
/// eye/target/up, a perspective matrix, a look-at matrix and their 4x4 f64
/// product, and a loop over N points has no business doing that N times.
///
/// Measured over 69,080 coordinates, 20 reps, release: 5.544 ms rebuilding the
/// matrix per point against 0.074 ms with it hoisted -- **74.7x, and all of it
/// setup**. An SoA pass over the same data with the matrix decomposed into
/// scalars reached 0.071 ms, only 4% better again, which is the useful part of
/// the finding: the win here is that the setup leaves the loop, not that the
/// arithmetic vectorises. A linear-algebra dependency would be paying for the
/// 4%.
///
/// Today's call volume makes this worth about 0.25 ms per frame (one call per
/// overlay label, four per debug tile), so it is a trap removed rather than a
/// bottleneck fixed -- and it is removed now, before anything in this crate
/// projects a whole coordinate array and meets the other 74x.
///
/// Callers projecting a single point should keep using
/// `project_world_to_screen_f64`, which is a wrapper over this.
pub(crate) fn project_world_to_screen_f64_with_matrix(
    camera: &Camera,
    view_projection: &glam::DMat4,
    world: (f64, f64),
) -> Option<(f32, f32)> {
    let clip = *view_projection * glam::DVec4::new(world.0, world.1, 0.0, 1.0);
    project_clip_to_screen(camera, clip.as_vec4())
}

/// Maps a clip-space position -- as produced by `Camera::view_projection()`
/// or (for tile-local input, in f64) `combined_tile_matrix` -- to a window
/// pixel through *exactly* the rest of the pipeline the GPU uses:
/// `shader.wgsl`'s post-projection `out.clip_position.x = -out.clip_position.x`
/// flip, then the standard wgpu NDC->framebuffer mapping (NDC x=-1 is the left
/// edge, NDC y=+1 is the *top* edge) into gpui's y-down window coordinates.
///
/// `None` when the point is at or behind the camera's near plane. For this
/// (DirectX-convention, right-handed) perspective matrix `w` is the eye-space
/// view distance, so `w <= 0` means "at or behind the eye" -- dividing by a
/// negative `w` flips both NDC signs and yields a plausible-looking but
/// mirrored on-screen position, which a plain in-viewport bounds check does
/// *not* catch. Reachable in practice at any nonzero pitch, where ground
/// points beyond the horizon fall behind the near plane.
///
/// Reached from non-test code via `crate::render::project_clip_to_screen`,
/// called by `map_view::project_symbol_anchor` (the shared anchor-projection
/// helper for symbol placement).
pub(crate) fn project_clip_to_screen(camera: &Camera, clip: glam::Vec4) -> Option<(f32, f32)> {
    if clip.w <= 0.0 {
        return None;
    }
    let ndc_x = -clip.x / clip.w; // the negation is shader.wgsl's horizontal flip
    let ndc_y = clip.y / clip.w;
    Some((
        (ndc_x + 1.0) * 0.5 * camera.viewport_width_px,
        (1.0 - ndc_y) * 0.5 * camera.viewport_height_px,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::TileId;

    /// An eye `height_m` over Marina Bay, pitched `pitch_deg`, facing
    /// north-east, in a 1000 x 700 view, at vertical FOV `fov_deg`.
    fn eye_at(fov_deg: f64, height_m: f64, pitch_deg: f32) -> Camera {
        let fov = fov_deg.to_radians();
        let t = crate::camera_control::eye_target_fov((103.8590, 1.2914), height_m, 40f32.to_radians(), pitch_deg.to_radians(), 700.0, fov);
        let (lon, lat) = t.center.unwrap();
        Camera { center_lon: lon, center_lat: lat, zoom: t.zoom.unwrap(), pitch: t.pitch.unwrap(), bearing: t.bearing.unwrap(), viewport_width_px: 1000.0, viewport_height_px: 700.0, fov_y: fov }
    }

    /// **Per-camera FOV**: the same eye (position and orientation) at 60
    /// and at 80 degrees draws a point's offset from the screen centre in
    /// the ratio `tan(40) / tan(30)`, on both axes, as a pinhole must; the
    /// eye stands where it stood (`eye_target_fov` solves the zoom), and
    /// picking inverts the wide projection exactly.
    #[test]
    fn a_wider_fov_shrinks_the_projection_as_tan_half_fov_predicts() {
        let (narrow, wide) = (eye_at(60.0, 1.8, 80.0), eye_at(80.0, 1.8, 80.0));
        let (e0, _, _) = narrow.eye_target_up();
        let (e1, _, _) = wide.eye_target_up();
        let m = crate::camera_control::metres_per_world_unit(narrow.center_lat);
        assert!((e0 - e1).length() * m < 1e-6, "the eye did not move: {} m", (e0 - e1).length() * m);
        assert!((wide.eye_height_m() - 1.8).abs() < 1e-9 && (narrow.eye_height_m() - 1.8).abs() < 1e-9);
        let want = 40f64.to_radians().tan() / 30f64.to_radians().tan();
        // Ground points ahead of the eye, left and right of the axis.
        let (eye_lon, eye_lat): (f64, f64) = (103.8590, 1.2914);
        let mut checked = 0;
        for (east_m, north_m) in [(12.0, 9.0), (6.0, 14.0), (20.0, 20.0), (9.0, 5.0)] {
            let lon = eye_lon + east_m / (111_319.49 * eye_lat.to_radians().cos());
            let lat = eye_lat + north_m / 110_574.0;
            let (x, y) = crate::geo::lon_lat_to_tile_xy(lon, lat, 0.0);
            let w = (x * crate::geo::TILE_SIZE, y * crate::geo::TILE_SIZE);
            let (Some(a), Some(b)) = (project_world_to_screen_f64(&narrow, w), project_world_to_screen_f64(&wide, w)) else { continue };
            let (ax, ay) = (f64::from(a.0) - 500.0, f64::from(a.1) - 350.0);
            let (bx, by) = (f64::from(b.0) - 500.0, f64::from(b.1) - 350.0);
            assert!((ax / bx - want).abs() < 1e-3 * want, "x: {ax} / {bx} vs {want}");
            assert!((ay / by - want).abs() < 1e-3 * want, "y: {ay} / {by} vs {want}");
            let back = unproject_to_ground_f64(&wide, b.0, b.1).expect("ground");
            assert!((back.x - w.0).hypot(back.y - w.1) * m < 0.01, "picking inverts the wide projection");
            checked += 1;
        }
        assert!(checked >= 3, "{checked} points in both views");
        // The horizon: tan(90 - pitch) / tan(fov / 2) of the half-height
        // above the centre.
        for cam in [narrow, wide] {
            let row = 350.0 * (1.0 - (10f64.to_radians().tan() / cam.tan_half_fov()));
            assert!(unproject_to_ground_f64(&cam, 500.0, (row - 2.0) as f32).is_none(), "sky above the horizon at {row}");
            assert!(unproject_to_ground_f64(&cam, 500.0, (row + 2.0) as f32).is_some(), "ground below it");
        }
    }

    /// The default FOV is the old constant bit for bit, and a non-finite or
    /// out-of-range FOV reads as the default or the nearest limit.
    #[test]
    fn the_default_fov_is_the_old_constant_bit_for_bit() {
        let base = Camera { fov_y: Camera::DEFAULT_FOV_Y, ..eye_at(60.0, 1.1, 85.0) };
        assert_eq!(base.fov_y_rad().to_bits(), std::f64::consts::FRAC_PI_3.to_bits());
        assert_eq!(Camera::DEFAULT_FOV_Y.to_bits(), FOV_Y_RADIANS_F64.to_bits());
        assert_eq!(Camera { fov_y: f64::NAN, ..base }.view_projection_f64(), base.view_projection_f64());
        assert_eq!(Camera { fov_y: 3.0, ..base }.fov_y_rad(), Camera::MAX_FOV_Y);
        assert_eq!(Camera { fov_y: 0.0, ..base }.fov_y_rad(), Camera::MIN_FOV_Y);
        let eye = crate::camera_control::EyeCamera::new(600.0, crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }, crate::style::Rgba { r: 0.1, g: 0.1, b: 0.1, a: 1.0 });
        assert_eq!(eye.fov_y_rad().to_bits(), FOV_Y_RADIANS_F64.to_bits(), "an eye camera keeps the default unless widened");
        assert!((eye.with_fov_deg(78.0).fov_y_rad().to_degrees() - 78.0).abs() < 1e-9);
    }

    /// The near plane may not cut what stands at the bottom edge of a
    /// max-pitch frame (the Task 7 shards): at `MAX_PITCH`, zoom 16, the
    /// bottom-centre ray meets the ground at a view depth of `0.5 · d`
    /// (`d = distance()`, the eye height being `0.5 · d`), and the near
    /// plane must sit at no more than a fifth of that, `0.1 · d`. Raising a
    /// point there by `h` cuts its depth by `0.5 h`, so a building up to
    /// `0.8 · d` tall (1.6x the eye height) is whole.
    #[test]
    fn at_max_pitch_the_near_plane_clears_a_tower_at_the_bottom_edge() {
        let camera = Camera {
            center_lon: 139.7,
            center_lat: 35.69,
            zoom: 16.0,
            pitch: crate::camera_control::MAX_PITCH,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let (eye, _, _) = camera.eye_target_up();
        // The bottom-centre ray: half the FOV below the forward axis, which
        // at pitch 60 is 30° off nadir; it meets z = 0 at eye.z / cos 30°,
        // and its view depth is that times cos(FOV / 2).
        let half = FOV_Y_RADIANS_F64 / 2.0;
        let slant = eye.z / (camera.pitch as f64 - half).cos();
        let depth = slant * half.cos();
        let d = camera.distance();
        assert!((depth / d - 0.5).abs() < 1e-6, "bottom-edge ground depth {depth} vs d {d}");
        let near = f64::from(near_distance(&camera));
        assert!((near / d - f64::from(NEAR_FRACTION)).abs() < 1e-6, "near {near} = NEAR_FRACTION · d");
        assert!(near <= 0.2 * depth, "near {near} must be <= 0.2 x the bottom-edge depth {depth}");
    }

    /// **The eye camera's near plane is the fraction's, not the floor's**:
    /// the close chase (1.8 m, 80 degrees, 65-degree FOV) and the cockpit
    /// (1.0 m, 84, 78) put it about half a metre out -- a car 3 m ahead is
    /// whole (the 1e-4 world-unit floor put it 7.8 m out and sliced it) --
    /// and still inside the bottom edge's ground; an ordinary camera at a
    /// deep zoom keeps the old floor bit for bit.
    #[test]
    fn an_eye_cameras_near_plane_sits_half_a_metre_out() {
        for (fov, h, pitch) in [(65.0, 1.8, 80.0f32), (78.0, 1.0, 84.0)] {
            let cam = eye_at(fov, h, pitch);
            let near = cam.near_m();
            assert!((0.2..1.0).contains(&near), "{fov} deg at {h} m: near {near} m");
            // The bottom edge's ground, its view depth.
            let below = f64::from(90.0 - pitch).to_radians() + cam.fov_y_rad() / 2.0;
            let depth = h / below.sin() * (cam.fov_y_rad() / 2.0).cos();
            assert!(near < 0.6 * depth, "{fov}: near {near} vs the bottom edge's ground {depth}");
        }
        let deep = Camera { pitch: 0.5, zoom: 21.0, ..eye_at(60.0, 1.1, 85.0) };
        assert_eq!(near_distance(&deep), NEAR_MIN, "an ordinary camera keeps its floor");
    }

    /// A tile baked at the camera's own bucket blends exactly as `fract()`
    /// did -- bit for bit, so no settled frame changes by a pixel.
    #[test]
    fn a_fresh_tile_blends_exactly_as_fract_did() {
        for zoom in [0.0, 1.0, 13.25, 15.6, 15.999_999, 16.0, 16.03, 17.2, 18.75, 19.0] {
            assert_eq!(
                tile_blend_t_for_baked_zoom(zoom, f64::floor(zoom)).to_bits(),
                (f64::fract(zoom) as f32).to_bits(),
                "zoom {zoom}"
            );
        }
    }

    /// A mesh one bucket behind the camera draws at its pair's END (where
    /// the fresh mesh starts), and one bucket ahead -- a zoom-out crossing
    /// -- at its pair's START; never the wrapped `fract()`.
    #[test]
    fn a_stale_tile_clamps_to_the_nearer_end_of_its_pair_instead_of_wrapping() {
        // Zoom-in: baked at 15, camera at 16.03 -> v@16. `fract()` gave 0.03,
        // i.e. v@15: the width snapped back a whole bucket.
        assert_eq!(tile_blend_t_for_baked_zoom(16.03, 15.0), 1.0);
        // Zoom-out: baked at 16, camera at 15.97 -> v@16, which is where the
        // fresh z15 mesh (t = 0.97) is heading. `fract()` gave 0.97, i.e.
        // almost v@17.
        assert_eq!(tile_blend_t_for_baked_zoom(15.97, 16.0), 0.0);
        // Two buckets behind still saturates.
        assert_eq!(tile_blend_t_for_baked_zoom(17.5, 15.0), 1.0);
    }

    fn tokyo_camera() -> Camera {
        Camera {
            center_lon: 139.7671,
            center_lat: 35.6812,
            zoom: 14.0,
            pitch: 0.6,
            bearing: 0.4,
            viewport_width_px: 1024.0,
            viewport_height_px: 768.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// Exactness, not approximate agreement. The hoisted form must multiply by
    /// the SAME matrix, so any difference at all is a DIFFERENT matrix and not
    /// a rounding difference. A tolerance here would hide precisely the bug
    /// this refactor could introduce.
    #[test]
    fn with_matrix_agrees_exactly_with_the_per_point_form() {
        let camera = tokyo_camera();
        let m = camera.view_projection_f64();
        for world in [(4096.0, 2048.0), (4096.5, 2048.25), (0.0, 0.0), (256.0, 256.0)] {
            assert_eq!(
                project_world_to_screen_f64_with_matrix(&camera, &m, world),
                project_world_to_screen_f64(&camera, world),
                "hoisting the matrix must not move the pixel for {world:?}"
            );
        }
    }

    /// The `None` for `w <= 0` is load-bearing (see `project_clip_to_screen`):
    /// a pitched camera projects a beyond-horizon ground point to a plausible
    /// but MIRRORED pixel, which a viewport bounds check does not catch. Both
    /// forms must agree on the rejection, not just on the arithmetic.
    #[test]
    fn with_matrix_still_rejects_a_point_behind_the_near_plane() {
        let camera = Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 4.0,
            pitch: 1.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let m = camera.view_projection_f64();
        // World y GROWS southward and this camera (bearing 0, pitch 1.0 rad)
        // looks north from south of its target, so a large POSITIVE y is the
        // half-space behind the eye. Probed rather than assumed: y = -1e6 still
        // projects to a pixel (399.9, -33.6) -- far off-screen but in front --
        // while y >= 500 is rejected. A test that used a large negative y would
        // agree on a pixel and assert nothing.
        let far_behind = (128.0, 1000.0);
        assert_eq!(
            project_world_to_screen_f64_with_matrix(&camera, &m, far_behind),
            project_world_to_screen_f64(&camera, far_behind),
            "both forms must agree on rejecting a point behind the near plane"
        );
        assert!(
            project_world_to_screen_f64(&camera, far_behind).is_none(),
            "and the agreed answer must actually BE the rejection -- two forms \
             agreeing on a wrong pixel would pass the assertion above, which is \
             exactly what the first draft of this test did"
        );
    }

    fn straight_down_camera(zoom: f64) -> Camera {
        Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    #[test]
    fn world_center_is_at_mercator_origin_for_lon_lat_zero() {
        let camera = straight_down_camera(2.0);
        let center = camera.world_center();
        assert!((center.x - WORLD_SIZE / 2.0).abs() < 0.01, "x was {}", center.x);
        assert!((center.y - WORLD_SIZE / 2.0).abs() < 0.01, "y was {}", center.y);
        assert_eq!(center.z, 0.0);
    }

    #[test]
    fn straight_down_camera_projects_world_center_to_screen_center() {
        let camera = straight_down_camera(3.0);
        let vp = camera.view_projection();
        let world_center = camera.world_center();
        let clip = vp * glam::Vec4::new(world_center.x, world_center.y, world_center.z, 1.0);
        let ndc_x = clip.x / clip.w;
        let ndc_y = clip.y / clip.w;
        assert!(ndc_x.abs() < 0.01, "ndc_x was {ndc_x}");
        assert!(ndc_y.abs() < 0.01, "ndc_y was {ndc_y}");
    }

    #[test]
    fn zooming_in_one_level_doubles_apparent_scale() {
        // A world point one world-unit east of center should land twice as far
        // from screen-center (in NDC) at zoom+1 as it does at zoom.
        let offset_point = |camera: &Camera| {
            let center = camera.world_center();
            let vp = camera.view_projection();
            let clip = vp * glam::Vec4::new(center.x + 1.0, center.y, center.z, 1.0);
            clip.x / clip.w
        };
        let low = straight_down_camera(4.0);
        let high = straight_down_camera(5.0);
        let ndc_low = offset_point(&low);
        let ndc_high = offset_point(&high);
        assert!(
            (ndc_high / ndc_low - 2.0).abs() < 0.01,
            "expected ~2x, got {}",
            ndc_high / ndc_low
        );
    }

    #[test]
    fn tile_model_matrix_places_tile_corners_in_world_space() {
        let tile = TileId { z: 2, x: 1, y: 1 };
        let extent = 4096.0;
        let model = tile_model_matrix(tile, extent);

        let local_min = glam::Vec4::new(0.0, 0.0, 0.0, 1.0);
        let local_max = glam::Vec4::new(extent * 1000.0, extent * 1000.0, 0.0, 1.0);

        let world_min = model * local_min;
        let world_max = model * local_max;

        let tile_world_size = WORLD_SIZE / 4.0; // 2^2 tiles across the world at z=2
        assert!((world_min.x - tile_world_size).abs() < 0.01, "min.x was {}", world_min.x);
        assert!((world_min.y - tile_world_size).abs() < 0.01, "min.y was {}", world_min.y);
        assert!(
            (world_max.x - tile_world_size * 2.0).abs() < 0.01,
            "max.x was {}",
            world_max.x
        );
        assert!(
            (world_max.y - tile_world_size * 2.0).abs() < 0.01,
            "max.y was {}",
            world_max.y
        );
    }

    #[test]
    fn pitch_aware_bbox_grows_as_pitch_increases() {
        let mut camera = straight_down_camera(4.0);
        let flat_bbox = pitch_aware_tile_bbox(&camera);
        camera.pitch = 0.8;
        let tilted_bbox = pitch_aware_tile_bbox(&camera);

        let flat_width = flat_bbox.2 - flat_bbox.0;
        let tilted_width = tilted_bbox.2 - tilted_bbox.0;
        assert!(
            tilted_width > flat_width,
            "tilted bbox ({tilted_width}) should be wider than flat ({flat_width})"
        );
    }

    #[test]
    fn tile_bbox_is_invariant_to_bearing() {
        // A rectangle that only widens with pitch (not bearing) would miss a
        // rotated footprint's corners at nonzero bearing — maplibre-rs hit
        // exactly this and had to hard-clamp bearing to +/-30 degrees as a
        // result. Our circular footprint must cover the same ground area
        // regardless of bearing, so the bbox side length shouldn't shrink as
        // bearing changes.
        let mut camera = straight_down_camera(4.0);
        camera.pitch = 0.6;
        let bbox_at_zero_bearing = pitch_aware_tile_bbox(&camera);
        let width_at_zero = bbox_at_zero_bearing.2 - bbox_at_zero_bearing.0;

        camera.bearing = std::f32::consts::FRAC_PI_4;
        let bbox_at_45_bearing = pitch_aware_tile_bbox(&camera);
        let width_at_45 = bbox_at_45_bearing.2 - bbox_at_45_bearing.0;

        assert!(
            (width_at_zero - width_at_45).abs() < 0.0001,
            "bbox width changed with bearing: {width_at_zero} vs {width_at_45}"
        );
    }

    #[test]
    fn ground_radius_keeps_shrinking_at_high_zoom_unlike_far_distance() {
        // far_distance clamps to near_distance(camera) as its floor, purely
        // so it stays valid as this camera's actual far clip plane (near <
        // far) -- not to give ground_radius a usable floor. ground_radius
        // deliberately does not reuse that floor at all (see its own doc
        // comment above) and clamps at 0.0 instead, so tile-selection radius
        // keeps shrinking all the way to (in principle) 0 with no
        // floor-induced flattening -- unlike the old fixed `NEAR = 1.0`,
        // which used to make far_distance itself flatten out past roughly
        // zoom 10 (back when near_distance was a zoom-independent constant
        // rather than proportional to `distance()`).
        let low = straight_down_camera(8.0);
        let high = straight_down_camera(16.0);
        let radius_low = ground_radius(&low);
        let radius_high = ground_radius(&high);
        assert!(
            radius_high < radius_low / 100.0,
            "expected ground_radius to keep shrinking by many orders of magnitude from zoom 8 to zoom 16, got {radius_low} -> {radius_high}"
        );
    }

    #[test]
    fn far_distance_is_bounded_even_at_steep_pitch() {
        let mut camera = straight_down_camera(4.0);
        let flat_far = far_distance(&camera);
        camera.pitch = 1.0; // ~57 degrees, close to the horizon
        let steep_far = far_distance(&camera);

        assert!(steep_far > flat_far, "far distance should grow with pitch");
        assert!(
            steep_far < flat_far * 1000.0,
            "far distance must stay bounded near the horizon, got {steep_far}"
        );
    }

    #[test]
    fn tile_directly_ahead_is_in_frustum_tile_far_outside_view_is_not() {
        let camera = straight_down_camera(2.0);
        let ahead = TileId { z: 2, x: 2, y: 2 }; // contains world center at z=2
        assert!(tile_in_frustum(ahead, &camera));

        // A much smaller, deeper tile at the far corner (z=6) is genuinely outside the FOV
        // at zoom 2 with an 800x600 viewport and 60° FOV.
        let far_away = TileId { z: 6, x: 0, y: 0 };
        assert!(!tile_in_frustum(far_away, &camera));
    }

    #[test]
    fn unprojecting_the_screen_center_returns_world_center() {
        let camera = straight_down_camera(4.0);
        let center_screen = (camera.viewport_width_px / 2.0, camera.viewport_height_px / 2.0);
        let ground = unproject_to_ground(&camera, center_screen.0, center_screen.1)
            .expect("straight-down camera must hit the ground at screen center");
        let world_center = camera.world_center();
        assert!((ground.x - world_center.x).abs() < 0.01, "x was {}", ground.x);
        assert!((ground.y - world_center.y).abs() < 0.01, "y was {}", ground.y);
    }

    #[test]
    fn unprojecting_left_of_center_returns_a_point_west_of_center() {
        // Screen-right must correspond to world +x (east) after the shader's
        // clip.x flip is accounted for here too — a point left of center must
        // unproject to smaller world-x (west), matching the corrected, no-longer-
        // mirrored rendering.
        let camera = straight_down_camera(4.0);
        let left_of_center = (camera.viewport_width_px / 4.0, camera.viewport_height_px / 2.0);
        let ground = unproject_to_ground(&camera, left_of_center.0, left_of_center.1).unwrap();
        let world_center = camera.world_center();
        assert!(ground.x < world_center.x, "expected west of center, got x={}", ground.x);
    }

    #[test]
    fn unprojecting_above_the_horizon_returns_none() {
        let mut camera = straight_down_camera(4.0);
        // This is a direct `Camera`-struct test, not going through any
        // UI-side pitch clamp (`MapView::tilt_by` clamps to 0.9 rad), so an
        // out-of-UI-range value is legal here. At the screen's top row and
        // dead-center column, the cast ray's direction is exactly
        // `-cos(pitch + half_fov_y)` in z (half_fov_y = FRAC_PI_6 for this
        // camera's 60-degree vertical FOV) — it points at or above the
        // horizon once `pitch >= FRAC_PI_2 - FRAC_PI_6 == FRAC_PI_3`
        // (~1.0472 rad). 1.2 rad clears that threshold with a healthy
        // margin (dir.z ~= +0.11, versus the `>= -1e-6` cutoff), so the
        // ray-above-horizon branch is reached deterministically rather than
        // depending on exact FOV math the way a borderline pitch would.
        camera.pitch = 1.2;
        let above_horizon = unproject_to_ground(&camera, camera.viewport_width_px / 2.0, 0.0);
        assert!(
            above_horizon.is_none(),
            "expected the top-center ray at pitch=1.2 to point above the horizon and return None, got {above_horizon:?}"
        );
    }

    #[test]
    fn unproject_to_ground_matches_a_hand_computed_f64_ray_intersection_at_high_zoom() {
        let camera = Camera {
            center_lon: 103.8198,
            center_lat: 1.3521,
            zoom: 19.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 768.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let center_screen = unproject_to_ground(&camera, 512.0, 384.0)
            .expect("center of viewport at pitch 0 must hit the ground");
        let expected = camera.world_center();
        // At pitch 0, the exact viewport center must unproject to exactly the
        // camera's look-at point (world_center) -- any deviation here at z19 is
        // f32-inversion quantization error, not a legitimate approximation.
        assert!(
            (center_screen.x - expected.x).abs() < 0.01,
            "x drifted by {} world units at z19 -- unproject_to_ground is not using f64 precision",
            (center_screen.x - expected.x).abs()
        );
        assert!(
            (center_screen.y - expected.y).abs() < 0.01,
            "y drifted by {} world units at z19",
            (center_screen.y - expected.y).abs()
        );
    }

    #[test]
    fn unproject_to_ground_f64_resolves_a_single_pixel_at_z20() {
        // World coordinates are zoom-independent and ~128-256 in magnitude,
        // so at z20 one screen pixel is 1/2^20 ~= 9.5e-7 world units -- about
        // 1/16 of an f32 ulp at that magnitude. The f32-returning wrapper
        // therefore CANNOT tell two adjacent pixels apart up here; the f64
        // version must. `MapView::zoom_towards_point` depends on exactly this
        // resolution, and losing it is what made zoom-toward-cursor visibly
        // wobble at high zoom.
        let camera = Camera {
            center_lon: 103.8198,
            center_lat: 1.3521,
            zoom: 20.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 768.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let a = unproject_to_ground_f64(&camera, 512.0, 384.0).unwrap();
        let b = unproject_to_ground_f64(&camera, 513.0, 384.0).unwrap();
        let expected = 1.0 / 2f64.powf(camera.zoom);
        let measured = b.x - a.x;
        assert!(
            (measured / expected - 1.0).abs() < 1e-3,
            "one pixel at z20 measured {measured:.3e} world units, expected {expected:.3e}"
        );

        // And the documented failure mode of the f32 wrapper, so the reason
        // the f64 variant exists stays visible if anyone is tempted to merge
        // them back together.
        let a32 = unproject_to_ground(&camera, 512.0, 384.0).unwrap();
        let b32 = unproject_to_ground(&camera, 513.0, 384.0).unwrap();
        assert_eq!(
            a32.x, b32.x,
            "f32 world coordinates are expected to be too coarse to separate adjacent pixels at \
             z20; if this now differs, re-check the ulp reasoning in Camera::world_center's docs"
        );
    }

    #[test]
    fn rendering_puts_east_on_the_right_and_north_at_the_top() {
        // The independently-verifiable half of the round-trip below: this is a
        // claim about the *rendered image* (confirmed by real drag-direction
        // testing during the mirror fix), stated without reference to
        // `unproject_to_ground`. World +x is east; world +y is south (web
        // Mercator tile-y grows downward), so north is world -y.
        let camera = straight_down_camera(4.0);
        let center = camera.world_center();
        let (cx, cy) = project_world_to_screen(&camera, center)
            .expect("the camera's own look-at point is always in front of the near plane");
        assert!((cx - camera.viewport_width_px / 2.0).abs() < 0.01, "center x was {cx}");
        assert!((cy - camera.viewport_height_px / 2.0).abs() < 0.01, "center y was {cy}");

        let (east_x, east_y) = project_world_to_screen(&camera, center + glam::Vec3::new(2.0, 0.0, 0.0))
            .expect("a point 2 world units east of center is in front of the near plane");
        assert!(east_x > cx, "east must render right of center, got {east_x} vs {cx}");
        assert!((east_y - cy).abs() < 0.01, "east must not move vertically, got {east_y}");

        let (north_x, north_y) = project_world_to_screen(&camera, center + glam::Vec3::new(0.0, -2.0, 0.0))
            .expect("a point 2 world units north of center is in front of the near plane");
        assert!(north_y < cy, "north must render above center, got {north_y} vs {cy}");
        assert!((north_x - cx).abs() < 0.01, "north must not move horizontally, got {north_x}");
    }

    #[test]
    fn unproject_round_trips_the_rendered_projection() {
        // For a spread of camera poses and ground points, projecting a world
        // point to a pixel (via the real render pipeline, above) and
        // unprojecting that pixel must return the same world point. This is
        // what would fail if the NDC y convention, the clip.x un-flip, or the
        // two-point ray construction had a sign/order error.
        for &(pitch, bearing) in &[
            (0.0f32, 0.0f32),
            (0.0, 1.2),
            (0.5, 0.0),
            (0.5, -2.0),
            (1.0, 0.7),
        ] {
            for &zoom in &[2.0f64, 4.0, 8.0] {
                let camera = Camera { pitch, bearing, zoom, ..straight_down_camera(zoom) };
                let center = camera.world_center();
                let step = 8.0 / 2f32.powf(zoom as f32 - 4.0);
                for &(dx, dy) in &[(0.0f32, 0.0f32), (step, 0.0), (-step, 0.0), (0.0, step), (0.0, -step), (step, -step)] {
                    let world = center + glam::Vec3::new(dx, dy, 0.0);
                    let (sx, sy) = project_world_to_screen(&camera, world).unwrap_or_else(|| {
                        panic!("pitch={pitch} bearing={bearing} zoom={zoom} offset=({dx},{dy}): a ground point near the look-at point must be in front of the near plane")
                    });
                    let recovered = unproject_to_ground(&camera, sx, sy).unwrap_or_else(|| {
                        panic!("pitch={pitch} bearing={bearing} zoom={zoom} offset=({dx},{dy}): a point that projects to pixel ({sx},{sy}) must unproject back to the ground")
                    });
                    let tolerance = step.abs().max(1.0) * 1e-3;
                    assert!(
                        (recovered.x - world.x).abs() < tolerance && (recovered.y - world.y).abs() < tolerance,
                        "pitch={pitch} bearing={bearing} zoom={zoom} offset=({dx},{dy}): expected {world:?}, got {recovered:?}"
                    );
                    assert!(recovered.z.abs() < tolerance, "result must lie on the ground plane, z={}", recovered.z);
                }
            }
        }
    }

    #[test]
    fn unprojecting_right_of_center_returns_a_point_east_of_center() {
        // Mirror image of `unprojecting_left_of_center_...`: together these
        // pin the horizontal sign from both sides, so a spurious x-flip
        // cannot pass by accident.
        let camera = straight_down_camera(4.0);
        let ground = unproject_to_ground(
            &camera,
            camera.viewport_width_px * 0.75,
            camera.viewport_height_px / 2.0,
        )
        .unwrap();
        let world_center = camera.world_center();
        assert!(ground.x > world_center.x, "expected east of center, got x={}", ground.x);
        assert!((ground.y - world_center.y).abs() < 0.01, "y should be unchanged, got {}", ground.y);
    }

    #[test]
    fn unprojecting_above_center_returns_a_point_north_of_center() {
        // World +y is south in web Mercator, so a pixel above screen-center
        // must unproject to a *smaller* world y.
        let camera = straight_down_camera(4.0);
        let ground = unproject_to_ground(
            &camera,
            camera.viewport_width_px / 2.0,
            camera.viewport_height_px * 0.25,
        )
        .unwrap();
        let world_center = camera.world_center();
        assert!(ground.y < world_center.y, "expected north of center, got y={}", ground.y);
        assert!((ground.x - world_center.x).abs() < 0.01, "x should be unchanged, got {}", ground.x);
    }

    #[test]
    fn camera_orbits_target_at_constant_distance_and_realized_tilt_matches_pitch() {
        for &pitch in &[0.0f32, 0.3, 0.6, 1.0, 1.4] {
            let camera = Camera { pitch, ..straight_down_camera(4.0) };
            let (eye, target, up) = camera.eye_target_up();

            let distance_from_target = (eye - target).length();
            let expected_distance = camera.distance();
            assert!(
                (distance_from_target - expected_distance).abs() < 0.01,
                "pitch={pitch}: eye-to-target distance was {distance_from_target}, expected {expected_distance}"
            );

            assert!((up.length() - 1.0).abs() < 0.001, "pitch={pitch}: up must stay unit length");
            let forward = (target - eye).normalize();
            assert!(
                forward.dot(up).abs() < 0.001,
                "pitch={pitch}: up must stay perpendicular to the view direction, dot was {}",
                forward.dot(up)
            );

            let realized_tilt = (-forward.z).clamp(-1.0, 1.0).acos();
            assert!(
                (realized_tilt - pitch as f64).abs() < 0.01,
                "pitch={pitch}: realized tilt was {realized_tilt}"
            );
        }
    }

    /// Ground plane point at `zoom`/`viewport_height_px` must not be
    /// near/far clipped: projects `world_center()` (the camera's own look-at
    /// point, always on the z=0 ground plane) through `view_projection()`
    /// and asserts the resulting NDC depth is finite and inside the
    /// `directx`-convention [0, 1] range this projection uses (see
    /// `glam::camera::rh::proj::directx`'s NDC Z range documented as [0, 1]).
    /// A value outside that range (or NaN/infinite from a degenerate
    /// near>=far matrix) means the ground plane got clipped away entirely --
    /// exactly the old bug at high zoom, where the fixed `NEAR = 1.0`
    /// exceeded `Camera::distance()`.
    fn assert_ground_plane_not_clipped(camera: &Camera) {
        let vp = camera.view_projection();
        let world_center = camera.world_center();
        let clip = vp * glam::Vec4::new(world_center.x, world_center.y, world_center.z, 1.0);
        assert!(clip.w > 0.0, "look-at target must be in front of the eye, w was {}", clip.w);
        let ndc_z = clip.z / clip.w;
        assert!(ndc_z.is_finite(), "ndc_z was not finite: {ndc_z}");
        assert!(
            (0.0..=1.0).contains(&ndc_z),
            "ground plane point was clipped (ndc_z={ndc_z} outside directx's [0,1] NDC-Z range), \
             zoom={} viewport_height={}",
            camera.zoom, camera.viewport_height_px
        );
    }

    #[test]
    fn view_projection_does_not_near_clip_the_ground_at_high_zoom() {
        // Zoom 14 at a realistic ~900px viewport height is well past the old
        // ~9.6-10 cliff where `Camera::distance()` first drops below the old
        // fixed `NEAR = 1.0` -- before this fix, this camera's own look-at
        // point (always exactly on the ground plane it's rendering) would
        // itself have been near-clipped, and the whole map would render
        // blank at this zoom.
        let camera = Camera {
            viewport_width_px: 1200.0,
            viewport_height_px: 900.0,
            ..straight_down_camera(14.0)
        };
        assert_ground_plane_not_clipped(&camera);
    }

    #[test]
    fn view_projection_still_does_not_near_clip_the_ground_at_low_zoom() {
        // Same assertion at a low zoom (well below the old cliff), to prove
        // the fix didn't just move the blank-screen bug rather than remove
        // it -- the already-correct low-zoom case must keep working.
        let camera = Camera {
            viewport_width_px: 1200.0,
            viewport_height_px: 900.0,
            ..straight_down_camera(2.0)
        };
        assert_ground_plane_not_clipped(&camera);
    }

    #[test]
    fn near_stays_below_far_across_zoom_and_pitch() {
        // Property test: near_distance/far_distance must never invert (an
        // inverted or degenerate near>=far pair silently produces a broken
        // projection matrix rather than panicking, so this has to be
        // checked explicitly rather than trusted). Covers zooms from well
        // below to well above the old NEAR=1.0 cliff, and pitches from
        // straight-down through past `MapView::tilt_by`'s real 0.9 rad UI
        // clamp (see map_view.rs), with a little headroom beyond it.
        for &zoom in &[0.0f64, 2.0, 4.0, 8.0, 9.6, 10.0, 12.0, 14.0, 18.0, 22.0] {
            for &pitch in &[0.0f32, 0.3, 0.6, 0.9, 1.0, 1.05] {
                let camera = Camera { pitch, ..straight_down_camera(zoom) };
                let near = near_distance(&camera);
                let far = far_distance(&camera);
                assert!(
                    near > 0.0 && near.is_finite(),
                    "zoom={zoom} pitch={pitch}: near must be positive and finite, got {near}"
                );
                assert!(
                    far.is_finite(),
                    "zoom={zoom} pitch={pitch}: far must be finite, got {far}"
                );
                assert!(
                    near < far,
                    "zoom={zoom} pitch={pitch}: near ({near}) must stay strictly below far ({far})"
                );
            }
        }
    }

    #[test]
    fn combined_tile_matrix_matches_separately_applying_model_then_view_proj_at_low_zoom() {
        let camera = Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 2.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 768.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let tile = TileId { z: 2, x: 2, y: 2 };
        let extent = 4096.0;

        let combined = combined_tile_matrix(&camera, tile, extent);
        let separate = camera.view_projection() * tile_model_matrix(tile, extent);

        let point = glam::Vec4::new(2048.0, 2048.0, 0.0, 1.0);
        let via_combined = combined * point;
        let via_separate = separate * point;

        // At a low, precision-friendly zoom the two approaches should agree
        // closely -- this is a sanity check that combined_tile_matrix computes
        // the SAME transform as the old two-step approach, not a different one,
        // before Task 3 ever wires it into real rendering. The real precision
        // WIN only shows up at high zoom (not practical to assert numerically
        // in a unit test -- see the plan's Task 3 manual verification step).
        assert!((via_combined.x - via_separate.x).abs() < 0.01);
        assert!((via_combined.y - via_separate.y).abs() < 0.01);
    }

    // At camera zoom == tile.z, one tile spans WORLD_SIZE/2^z world units and
    // exactly WORLD_SIZE pixels... so pixels per lyon unit is WORLD_SIZE over the
    // tile's full lyon width (extent * COORD_SCALE).
    //
    // DELIBERATE REVERSAL -- do not "correct" this back. An earlier revision
    // asserted sqrt(2)x at z10.5, i.e. dash_scale varying CONTINUOUSLY with
    // fractional zoom. The user compared our render against real Mapbox,
    // observed that Mapbox's dashes step at integer zooms rather than growing
    // smoothly, and chose to match it. `dash_scale` now floors the zoom,
    // mirroring MapLibre's `transform.tileZoom = Math.floor(transform.zoom)`
    // feeding `u_tileratio`. Stepping here is the intended behaviour, not a
    // rounding bug. (Line THICKNESS stays continuous -- that is the vertex
    // shader's blended half-width, a separate path.)
    #[test]
    fn dash_scale_steps_at_integer_zoom() {
        let extent = 4096.0;
        let tile = TileId { z: 10, x: 0, y: 0 };

        let at_native = dash_scale(&straight_down_camera(10.0), tile, extent);
        let expected = 512.0 / (1000.0 * extent);
        assert!((at_native - expected).abs() < 1e-9, "got {at_native}, want {expected}");

        // Stepped, not continuous: anywhere inside a zoom level the value is
        // identical to its value at the level's floor.
        let half_in = dash_scale(&straight_down_camera(10.5), tile, extent);
        assert!(
            (half_in - at_native).abs() < 1e-9,
            "expected dash_scale to be CONSTANT within a zoom level, got {half_in} at z10.5 vs {at_native} at z10.0"
        );
        let nearly_next = dash_scale(&straight_down_camera(10.999), tile, extent);
        assert!(
            (nearly_next - at_native).abs() < 1e-9,
            "expected dash_scale to be CONSTANT within a zoom level, got {nearly_next} at z10.999 vs {at_native} at z10.0"
        );

        // ...and doubles exactly at the boundary.
        let one_in = dash_scale(&straight_down_camera(11.0), tile, extent);
        assert!(
            (one_in / at_native - 2.0).abs() < 1e-5,
            "expected exactly 2x at the next integer zoom, got {}", one_in / at_native
        );
    }

    /// The overlay's dash scale, pinned to a value derived independently of
    /// the function, and pinned to step at integer zoom exactly as
    /// `dash_scale` does.
    ///
    /// The absolute value is load-bearing, not decorative: it is what turns a
    /// vertex's `line_distance` (lyon units) into the screen-pixel phase the
    /// dash mask is evaluated at. A plausible-looking `1.0` here is seven
    /// orders of magnitude out and makes every dashed overlay feature's
    /// period nonsense, which is not a subtle failure but also not one any
    /// test outside this file would catch.
    #[test]
    fn overlay_dash_scale_matches_the_overlay_grid_and_steps_at_integer_zoom() {
        // Restated here from first principles rather than read from
        // `OVERLAY_COORD_SCALE`, so this is a genuine cross-check of the
        // function and not a restatement of it: a lyon unit IS an overlay
        // grid unit now (`OVERLAY_COORD_SCALE` is `1.0`), and one overlay
        // grid unit is `1 / OVERLAY_UNITS_PER_WORLD` world units. A
        // `COORD_SCALE` (1000) factor here would be a 1000x error -- that
        // factor belongs to a TILE's lyon coordinates, not the overlay's.
        let one_lyon_unit_in_world = 1.0 / crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;

        let at_native = overlay_dash_scale(&straight_down_camera(10.0));
        let expected = (one_lyon_unit_in_world * 1024.0) as f32;
        assert!(
            (at_native / expected - 1.0).abs() < 1e-6,
            "got {at_native}, want {expected}"
        );

        // Stepped, not continuous -- `floor(camera.zoom)`, the same
        // deliberate quantisation `dash_scale` uses.
        assert_eq!(overlay_dash_scale(&straight_down_camera(10.5)), at_native);
        assert_eq!(overlay_dash_scale(&straight_down_camera(10.999)), at_native);

        // ...and doubles exactly at the boundary.
        let one_in = overlay_dash_scale(&straight_down_camera(11.0));
        assert!(
            (one_in / at_native - 2.0).abs() < 1e-5,
            "expected exactly 2x at the next integer zoom, got {}",
            one_in / at_native
        );
    }

    /// The overlay matrix must agree with the tile matrix on the one case
    /// where both describe the same mapping: a point at the overlay origin
    /// lands wherever the camera projects that world point.
    #[test]
    fn combined_overlay_matrix_places_the_origin_at_the_projected_world_point() {
        let camera = straight_down_camera(12.0);
        let origin = (256.0_f64, 256.0_f64);
        let matrix = combined_overlay_matrix(&camera, origin);
        let clip = matrix * glam::Vec4::new(0.0, 0.0, 0.0, 1.0);
        let via_matrix = project_clip_to_screen(&camera, clip).unwrap();

        let expected = project_world_to_screen(
            &camera,
            glam::Vec3::new(origin.0 as f32, origin.1 as f32, 0.0),
        )
        .unwrap();
        assert!((via_matrix.0 - expected.0).abs() < 0.01, "{via_matrix:?} vs {expected:?}");
        assert!((via_matrix.1 - expected.1).abs() < 0.01, "{via_matrix:?} vs {expected:?}");
    }

    /// One local unit maps to exactly `1 / OVERLAY_UNITS_PER_WORLD` world
    /// units, and equally on both axes -- the uniform-scale requirement the
    /// stroke shader depends on.
    #[test]
    fn the_overlay_matrix_scale_is_uniform_and_matches_the_grid() {
        use crate::overlay::geom::OVERLAY_UNITS_PER_WORLD;
        let matrix = overlay_model_matrix_f64((0.0, 0.0));
        // A lyon unit IS a local unit (OVERLAY_COORD_SCALE in
        // render/mesh.rs is 1.0, unlike a tile's COORD_SCALE).
        let one_local_unit_in_lyon = 1.0;
        let moved = matrix * glam::DVec4::new(one_local_unit_in_lyon, one_local_unit_in_lyon, 0.0, 1.0);
        let expected = 1.0 / OVERLAY_UNITS_PER_WORLD;
        assert!((moved.x - expected).abs() < 1e-12, "x scale was {}", moved.x);
        assert!((moved.y - expected).abs() < 1e-12, "y scale was {}", moved.y);
    }

    /// A pitched camera for the pitch-scale tests: Singapore, z16, a
    /// 1024x768 viewport, and 0.85 rad of pitch (~48.7 degrees, just under
    /// the old 0.9 `MAX_PITCH`; it is 60 degrees since the 3D camera work) --
    /// high in the UI's pitch range, where the correction matters most.
    fn pitched_camera(pitch: f32) -> Camera {
        Camera {
            center_lon: 103.855,
            center_lat: 1.295,
            zoom: 16.0,
            pitch,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 768.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// EXACTLY 1.0, at every pixel of an unpitched viewport, not merely
    /// close to it. `overlay::hit::tolerance_world` multiplies by this
    /// factor, so anything other than a literal 1.0 here would perturb
    /// every unpitched hit test by an ulp or two -- harmless in effect but
    /// enough to make "pitch 0 is bit-identical" a claim that is not true.
    ///
    /// The exactness comes from two places, both deliberate: the ground
    /// plane is parallel to the image plane at pitch 0 (so the depth really
    /// is constant across the viewport), and
    /// `pitch_scale_factor_at_ground` flattens its argument onto z=0 rather
    /// than trusting `unproject_to_ground_f64`'s near-zero z.
    #[test]
    fn the_pitch_scale_factor_is_exactly_one_everywhere_at_pitch_zero() {
        let camera = pitched_camera(0.0);
        for y in [0.0, 1.0, 200.0, 384.0, 500.0, 767.0] {
            for x in [0.0, 1.0, 300.0, 512.0, 1023.0] {
                let ground = unproject_to_ground_f64(&camera, x, y)
                    .expect("an unpitched ray always reaches the ground");
                let factor = pitch_scale_factor_at_ground(&camera, ground);
                assert_eq!(
                    factor, 1.0,
                    "pitch 0 must give a literal 1.0 at ({x}, {y}), got {factor}"
                );
            }
        }
    }

    /// The factor is the PROJECTION's own `w` ratio -- the quantity
    /// MapLibre's `maxPitchScaleFactor` takes at the viewport corner
    /// (`mercator_transform.ts`: `topPoint[3] / cameraToCenterDistance`) --
    /// and not merely something monotonic in pitch that happens to look
    /// right.
    ///
    /// `pitch_scale_factor_at_ground` computes it as a dot product against
    /// the view axis, which is cheap and needs no matrix inverse; this is
    /// what holds that shortcut to the matrix it is standing in for. If
    /// `view_projection_f64`'s projection convention ever changes such that
    /// clip `w` stops being view-space depth, this is the test that says so.
    #[test]
    fn the_pitch_scale_factor_is_the_projection_matrixs_own_w_ratio() {
        let camera = pitched_camera(0.85);
        let view_proj = camera.view_projection_f64();
        let center_distance = camera.distance();
        for y in [150.0, 300.0, 384.0, 600.0, 760.0] {
            let ground = unproject_to_ground_f64(&camera, 512.0, y)
                .expect("a below-horizon ray reaches the ground");
            let clip = view_proj * glam::DVec4::new(ground.x, ground.y, 0.0, 1.0);
            let from_matrix = clip.w / center_distance;
            let measured = pitch_scale_factor_at_ground(&camera, ground);
            assert!(
                (measured / from_matrix - 1.0).abs() < 1e-9,
                "at y={y} the dot-product factor {measured} disagrees with the \
                 matrix's w ratio {from_matrix}"
            );
        }
    }

    /// Above the middle row of a pitched viewport a pixel covers MORE
    /// ground; below it, less. The factor therefore rises monotonically
    /// from bottom to top, passes through 1.0 at the look-at point, and at
    /// the top of the UI's pitch range reaches a couple of times the centre
    /// figure -- the same order of magnitude as MapLibre's own
    /// `maxPitchScaleFactor` unit test, which pins 2.366 for a camera of
    /// theirs (`mercator_transform.test.ts`).
    ///
    /// This is the quantity a zoom-only pixel-to-world conversion was
    /// silently assuming to be 1, which is what made a 6-pixel hover
    /// tolerance a ~2-pixel one near the horizon.
    #[test]
    fn the_pitch_scale_factor_rises_toward_the_horizon_and_falls_toward_the_camera() {
        let camera = pitched_camera(0.85);
        let factor_at = |y: f32| {
            let ground = unproject_to_ground_f64(&camera, 512.0, y)
                .expect("a below-horizon ray reaches the ground");
            pitch_scale_factor_at_ground(&camera, ground)
        };

        let rows = [100.0, 250.0, 384.0, 500.0, 700.0];
        let factors: Vec<f64> = rows.iter().map(|y| factor_at(*y)).collect();
        for pair in factors.windows(2) {
            assert!(
                pair[0] > pair[1],
                "the factor must fall as screen y increases, got {factors:?} for {rows:?}"
            );
        }

        // The look-at point is the definition of "one": `world_units_per_px`
        // is the figure AT the centre, so the factor there is 1 by
        // construction and this is the anchor the other rows are relative to.
        let at_centre = factor_at(384.0);
        assert!(
            (at_centre - 1.0).abs() < 1e-6,
            "the viewport centre unprojects to the look-at point, so its factor is 1, got {at_centre}"
        );
        assert!(
            factors[0] > 1.5 && factors[0] < 4.0,
            "near the horizon a pixel should cover a small multiple of the centre figure, got {}",
            factors[0]
        );
        assert!(
            *factors.last().unwrap() < 0.8,
            "near the bottom it should cover clearly less, got {}",
            factors.last().unwrap()
        );
    }

    /// Bearing rotates the camera frame rigidly around its look-at point
    /// (`eye_target_up`), so the factor at a given SCREEN position cannot
    /// depend on bearing -- and picking must not start behaving differently
    /// just because the user rotated the map. A formula written in world-x
    /// or world-y rather than along the view axis would fail this.
    #[test]
    fn the_pitch_scale_factor_does_not_depend_on_bearing() {
        let reference = {
            let camera = pitched_camera(0.85);
            let ground = unproject_to_ground_f64(&camera, 400.0, 200.0).unwrap();
            pitch_scale_factor_at_ground(&camera, ground)
        };
        for bearing in [0.7_f32, 2.4, -1.9, std::f32::consts::PI] {
            let mut camera = pitched_camera(0.85);
            camera.bearing = bearing;
            let ground = unproject_to_ground_f64(&camera, 400.0, 200.0).unwrap();
            let rotated = pitch_scale_factor_at_ground(&camera, ground);
            assert!(
                (rotated / reference - 1.0).abs() < 1e-6,
                "bearing {bearing} changed the factor from {reference} to {rotated}"
            );
        }
    }

    /// A degenerate camera returns 1.0 rather than NaN or infinity. A
    /// zero-height viewport really does occur -- `MapView::new` runs before
    /// a window exists -- and a NaN tolerance would make every hit test
    /// answer `None` (every comparison against NaN is false), which is a
    /// dead map rather than a slightly wrong one.
    #[test]
    fn a_degenerate_camera_yields_a_neutral_pitch_scale_factor() {
        let mut camera = pitched_camera(0.85);
        camera.viewport_height_px = 0.0;
        let factor = pitch_scale_factor_at_ground(&camera, camera.world_center_f64());
        assert_eq!(factor, 1.0, "a zero-height viewport must not poison the factor");

        // A GROUND point behind the camera has no on-screen scale at all: it
        // projects to negative depth. The picking path cannot produce one
        // (`unproject_to_ground_f64` intersects only forward-going rays), but
        // a hand-built argument can, and a negative factor would hand
        // `hit_test` a negative tolerance.
        let camera = pitched_camera(0.85);
        let (eye, target, _up) = camera.eye_target_up();
        // Away from the look-at point, along the ground, past the camera's
        // own nadir: `eye - target` points backwards, and three times its
        // ground-plane extent clears the nadir comfortably.
        let backwards = (eye - target).truncate();
        let behind = target + (backwards * 3.0).extend(0.0);
        assert_eq!(
            pitch_scale_factor_at_ground(&camera, behind),
            1.0,
            "a ground point behind the camera must not produce a negative tolerance"
        );
    }
}
