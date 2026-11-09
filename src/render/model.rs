//! 3D models: one triangle mesh drawn once per instance, each instance
//! placed at a lon/lat/altitude, turned by a heading (and optionally a pitch
//! and a roll), scaled in metres and tinted -- race cars, buses, aircraft on
//! a stand. `model_shader.wgsl` draws it.
//!
//! - **Mesh** ([`ModelMesh`]): positions and normals in METRES in the
//!   model's own frame (+x forward, +y left, +z up, the origin on the
//!   ground), `u32` indices, and a material id per vertex into a palette
//!   of at most [`MAX_MATERIALS`] [`ModelMaterial`]s. A material marked
//!   `paint` takes each instance's colour; every other keeps its own (the
//!   tyres stay black whatever the team). Uploaded ONCE per mesh: an
//!   instance update never re-sends it ([`ModelPipeline::mesh_uploads_total`]).
//! - **Instances** ([`ModelInstance`] → [`ModelInstances`], 48 B each on the
//!   GPU): packed on the CPU in f64 RELATIVE TO AN ANCHOR (the first
//!   instance), so the per-frame cost is the instance upload alone -- a few
//!   hundred instances is a few KB -- and the camera never re-packs them.
//!   The anchor's offset from the camera is a per-frame uniform computed in
//!   f64 and narrowed once, so an instance holds still to the pixel at zoom
//!   20 (the same reason `arcs::ArcFrame` keeps the camera's origin out of
//!   the f32 maths).
//! - **Depth**: drawn INSIDE the tile pass, after the basemap's buildings
//!   and the overlay extrusions ([`super::gpu::TileDraw::Model`]), with the
//!   same depth attachment, `LessEqual` and depth writes on -- so a car
//!   behind a building is hidden by it, and a building behind a car is
//!   hidden by the car. The tyres touch the ground plane along a line, not
//!   a face, so nothing z-fights with the map; no part of a sane mesh lies
//!   in the plane `z = 0`.
//! - **Lighting**: one directional "sun" (a floodlight at night), a
//!   hemisphere ambient, a Blinn highlight scaled per material (`gloss`),
//!   and a cheap rim term that keeps a dark car's silhouette readable
//!   against a dark road ([`ModelLight`]). Lit in the local east-north-up
//!   frame, so the sun is where the caller says on flat and globe alike.
//!   Two-sided: no face is culled (the east-west flip folded into the view
//!   matrix mirrors the winding anyway).
//! - **Flat** (Web Mercator): exact. An instance's offset from the anchor
//!   takes the short way across the antimeridian, and the anchor is moved to
//!   the world copy nearest the camera.
//! - **Globe**: each instance is placed on the unit sphere at its lon/lat,
//!   lifted by its altitude, its mesh laid in the local east-north-up frame
//!   there; the transition blends the two clip positions as the trips do.
//!   An instance the planet hides is pushed past the far plane. Placement
//!   is from f32 lon/lat: right to ~1 m, which is sub-pixel at every zoom
//!   the globe is shown at.
//! - **Parts** ([`PartMatrix`], [`ModelVertex::pack`]): a vertex may name a
//!   PART (1..=[`MAX_PARTS`]) in the bits above its material; each frame
//!   the layer hands one local 3x4 matrix per part
//!   ([`ModelLayerDraw::parts`]), applied in the mesh's own frame before
//!   the instance's placement -- a steering wheel turning about its
//!   column, a front wheel steering, an arm's bones following a hand. Part
//!   0 is the rigid body and never reads the table. **Why a part table and
//!   not one instance per part**: every part rides the ONE draw of its
//!   mesh (no extra draw, no extra uniform slot, no extra mesh upload),
//!   the per-frame cost is the table alone (48 B a part, written only for
//!   a layer that has parts; every other layer shares one identity slot),
//!   and a part's matrix is a full affine local transform -- an
//!   instance-per-part split would cost a draw and a layer per part, and
//!   could only turn about the instance's own origin by heading, pitch
//!   and roll.
//! - **Near pass** ([`ModelNearPass`], opt-in per layer): a first-person
//!   "viewmodel" -- the viewer's own vehicle round the eye -- drawn AFTER
//!   the whole scene in its own pass with the depth buffer cleared, by the
//!   same camera pose with its own near and far planes (by default 0.02 m
//!   and 15 m), so nothing at arm's length is near-clipped. three.js's
//!   viewmodel practice: a second camera sharing the eye, `clearDepth()`,
//!   draw. The scene can never cut into it, and it covers the scene
//!   wherever it is drawn. Flat (Mercator) frames only: on a globe frame
//!   (transition > 0) a near layer draws in the tile pass as any other.
//!   Lit with a per-vertex view direction: the eye is centimetres away,
//!   and one view vector per frame would put the highlights and the rim
//!   in the wrong place across the frame.
//! - **Portability**: one uniform block and one part table (dynamic
//!   offsets), two vertex buffers (mesh per-vertex, instances
//!   per-instance), `u32` indices, no storage buffers: WebGL2 / GLSL ES
//!   3.00 (tested).

use std::sync::Arc;

use glam::{DVec2, DVec3};

use super::alloc::TrackedBuffer;
use super::arcs::ArcFrame;
use super::gpu::GpuContext;
use crate::style::Rgba;

/// The most materials one mesh may name (the palette is a uniform array).
pub const MAX_MATERIALS: usize = 8;

/// The most moving parts one mesh may name ([`ModelVertex::pack`]): part
/// ids 1..=`MAX_PARTS` index the layer's part table, 0 is the rigid body.
pub const MAX_PARTS: usize = 32;

/// A part's local transform in the mesh's frame (metres): three ROWS of
/// `[x, y, z, translation]`, `p' = M · (p, 1)`. A rotation and a
/// translation (a uniform scale is allowed: the normals are re-normalised).
pub type PartMatrix = [[f32; 4]; 3];

/// The part that does not move.
pub const PART_IDENTITY: PartMatrix = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];

/// `m` applied to the point `p`: the shader's part transform on the CPU.
#[must_use]
pub fn part_point(m: &PartMatrix, p: [f64; 3]) -> [f64; 3] {
    let r = |k: usize| f64::from(m[k][0]) * p[0] + f64::from(m[k][1]) * p[1] + f64::from(m[k][2]) * p[2] + f64::from(m[k][3]);
    [r(0), r(1), r(2)]
}

/// The part table's bytes per layer: three `vec4` rows per part.
const PARTS_BYTES: u64 = (MAX_PARTS * std::mem::size_of::<PartMatrix>()) as u64;

/// The bits of [`ModelVertex::material`] that hold the material slot; the
/// part id sits in the 8 above them.
const MATERIAL_BITS: u32 = 8;

/// Metres round the equator, as the shader.
const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.686;
const MAX_LAT: f64 = 85.051_129;

/// One mesh vertex on the GPU: 28 B.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ModelVertex {
    /// Metres, model frame: +x forward, +y left, +z up.
    pub position: [f32; 3],
    /// Unit length, model frame.
    pub normal: [f32; 3],
    /// Index into [`ModelMesh::materials`] in the low 8 bits, and the
    /// vertex's part (0 the rigid body, 1..=[`MAX_PARTS`]) in the 8 above
    /// them ([`Self::pack`]).
    pub material: u32,
}

const _: () = assert!(std::mem::size_of::<ModelVertex>() == 28);

impl ModelVertex {
    /// `material` (a slot) and `part` (0 rigid, 1..=[`MAX_PARTS`]) in one
    /// [`Self::material`] word.
    #[must_use]
    pub const fn pack(material: u32, part: u32) -> u32 {
        (material & 0xff) | ((part & 0xff) << MATERIAL_BITS)
    }

    /// The material slot.
    #[must_use]
    pub const fn slot(&self) -> u32 {
        self.material & 0xff
    }

    /// The part (0: the rigid body).
    #[must_use]
    pub const fn part(&self) -> u32 {
        (self.material >> MATERIAL_BITS) & 0xff
    }
}

/// **A first-person near pass** for a model layer (see the module docs):
/// its own near and far planes, metres from the eye.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelNearPass {
    pub near_m: f32,
    pub far_m: f32,
}

impl ModelNearPass {
    /// The viewer's own vehicle: 2 cm to 15 m.
    pub const FIRST_PERSON: Self = Self { near_m: 0.02, far_m: 15.0 };
}

impl Default for ModelNearPass {
    fn default() -> Self {
        Self::FIRST_PERSON
    }
}

/// One material slot of a mesh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelMaterial {
    pub color: Rgba,
    /// Tinted by each instance's colour ([`ModelInstance::color`]); an
    /// instance with no colour keeps `color`.
    pub paint: bool,
    /// Specular strength, 0 (matte: rubber, carbon) to 1 (clear coat).
    pub gloss: f32,
    /// How much of the colour shows whatever the light, 0 (lit, the
    /// default) to 1 (a lamp: the colour itself, day or night).
    pub emissive: f32,
}

impl ModelMaterial {
    #[must_use]
    pub const fn new(color: Rgba, gloss: f32) -> Self {
        Self { color, paint: false, gloss, emissive: 0.0 }
    }

    #[must_use]
    pub const fn paint(color: Rgba, gloss: f32) -> Self {
        Self { color, paint: true, gloss, emissive: 0.0 }
    }

    /// This material glowing by `emissive` (clamped to 0..=1).
    #[must_use]
    pub fn glowing(self, emissive: f32) -> Self {
        Self { emissive: emissive.clamp(0.0, 1.0), ..self }
    }
}

/// A triangle mesh, metres in the model frame (+x forward, +y left, +z
/// up, the origin on the ground under the model's centre).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelMesh {
    pub vertices: Vec<ModelVertex>,
    /// Triangle list.
    pub indices: Vec<u32>,
    /// At most [`MAX_MATERIALS`]; a vertex naming a slot past the end is
    /// drawn in the last one.
    pub materials: Vec<ModelMaterial>,
}

impl ModelMesh {
    #[must_use]
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }

    /// Bytes a mesh upload costs (vertices and indices).
    #[must_use]
    pub fn upload_bytes(&self) -> usize {
        self.vertices.len() * std::mem::size_of::<ModelVertex>() + self.indices.len() * 4
    }

    /// `(min, max)` corners, metres; zeros for an empty mesh.
    #[must_use]
    pub fn bounds(&self) -> ([f32; 3], [f32; 3]) {
        if self.vertices.is_empty() {
            return ([0.0; 3], [0.0; 3]);
        }
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for v in &self.vertices {
            for k in 0..3 {
                lo[k] = lo[k].min(v.position[k]);
                hi[k] = hi[k].max(v.position[k]);
            }
        }
        (lo, hi)
    }

    /// Whether every index is in range and the index count is whole
    /// triangles: what an upload requires.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.indices.len().is_multiple_of(3) && self.indices.iter().all(|&i| (i as usize) < self.vertices.len())
    }
}

/// One placed copy of a mesh, as the caller has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelInstance {
    pub lon: f64,
    pub lat: f64,
    /// Metres above the ground.
    pub alt_m: f64,
    /// Where the model's +x points: degrees clockwise from north.
    pub heading_deg: f64,
    /// Nose up, degrees.
    pub pitch_deg: f64,
    /// Left side up (a right-hand turn's lean), degrees.
    pub roll_deg: f64,
    /// Multiplies the mesh's metres (1 = life size).
    pub scale: f64,
    /// The paint slots' colour; `None` keeps the mesh's own.
    pub color: Option<Rgba>,
}

impl ModelInstance {
    /// Life size, level, at `lon`/`lat` on the ground facing `heading_deg`.
    #[must_use]
    pub fn at(lon: f64, lat: f64, heading_deg: f64) -> Self {
        Self { lon, lat, alt_m: 0.0, heading_deg, pitch_deg: 0.0, roll_deg: 0.0, scale: 1.0, color: None }
    }

    #[must_use]
    pub fn with_color(self, color: Rgba) -> Self {
        Self { color: Some(color), ..self }
    }

    #[must_use]
    pub fn with_scale(self, scale: f64) -> Self {
        Self { scale, ..self }
    }
}

/// One instance on the GPU: 48 B.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ModelInstanceGpu {
    /// World units from the set's anchor (x east, y south, z up), and the
    /// flat scale: world units per mesh metre.
    pub rel_scale: [f32; 4],
    /// lon, lat degrees, altitude metres, and the globe scale: the
    /// instance's `scale` (mesh metres per metre).
    pub lla_scale: [f32; 4],
    /// heading (radians clockwise from north), pitch, roll (radians).
    pub angles: [f32; 3],
    /// RGBA8 straight alpha; alpha 0 keeps the mesh's paint.
    pub color: u32,
}

const _: () = assert!(std::mem::size_of::<ModelInstanceGpu>() == 48);

/// `vs_model`'s attributes: the mesh (slot 0) and the instance (slot 1).
pub const VERTEX_ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Uint32];
pub const INSTANCE_ATTRS: [wgpu::VertexAttribute; 4] =
    wgpu::vertex_attr_array![3 => Float32x4, 4 => Float32x4, 5 => Float32x3, 6 => Unorm8x4];

/// Mercator `(x, y)` in 0..1 of a lon/lat.
fn mercator01(lon: f64, lat: f64) -> DVec2 {
    let lat = lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
    DVec2::new((lon + 180.0) / 360.0, (1.0 - (std::f64::consts::FRAC_PI_4 + lat * 0.5).tan().ln() / std::f64::consts::PI) * 0.5)
}

/// Flat world units per metre at latitude `lat`.
fn world_per_metre(lat: f64) -> f64 {
    let c = lat.clamp(-MAX_LAT, MAX_LAT).to_radians().cos().max(1e-3);
    f64::from(super::WORLD_SIZE) / (EARTH_CIRCUMFERENCE_M * c)
}

/// A layer's instances, packed for the GPU against an anchor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInstances {
    pub gpu: Vec<ModelInstanceGpu>,
    /// The anchor, Mercator 0..1 (the first finite instance).
    pub anchor: DVec2,
}

impl ModelInstances {
    /// Packs `instances`; a non-finite position drops that instance.
    #[must_use]
    pub fn new(instances: &[ModelInstance]) -> Self {
        let finite = |i: &&ModelInstance| i.lon.is_finite() && i.lat.is_finite();
        let Some(first) = instances.iter().find(finite) else { return Self::default() };
        let anchor = mercator01(first.lon, first.lat);
        let world = f64::from(super::WORLD_SIZE);
        let gpu = instances
            .iter()
            .filter(finite)
            .map(|i| {
                // The short way round from the anchor.
                let lon = i.lon - 360.0 * ((i.lon - first.lon) / 360.0).round();
                let m = mercator01(lon, i.lat);
                let alt = if i.alt_m.is_finite() { i.alt_m } else { 0.0 };
                let scale = if i.scale.is_finite() { i.scale } else { 1.0 };
                let k = world_per_metre(i.lat);
                let rel = (m - anchor) * world;
                let angle = |d: f64| if d.is_finite() { d.to_radians() as f32 } else { 0.0 };
                ModelInstanceGpu {
                    rel_scale: [rel.x as f32, rel.y as f32, (alt * k) as f32, (scale * k) as f32],
                    lla_scale: [i.lon as f32, i.lat as f32, alt as f32, scale as f32],
                    angles: [angle(i.heading_deg), angle(i.pitch_deg), angle(i.roll_deg)],
                    color: i.color.map_or(0, super::arcs::pack_color),
                }
            })
            .collect();
        Self { gpu, anchor }
    }

    pub fn len(&self) -> usize {
        self.gpu.len()
    }

    pub fn is_empty(&self) -> bool {
        self.gpu.is_empty()
    }

    /// Bytes an instance upload costs.
    #[must_use]
    pub fn upload_bytes(&self) -> usize {
        self.gpu.len() * std::mem::size_of::<ModelInstanceGpu>()
    }
}

impl ModelInstance {
    /// This instance's box as a label occluder
    /// (`extrusion::ModelOccluder`): `bounds` (the mesh's, metres) scaled,
    /// turned by the heading (pitch and roll ignored: a box upright round
    /// a car or a bus) and placed in flat world units. `None` for a
    /// non-finite position or an empty box.
    #[must_use]
    pub fn label_occluder(&self, bounds: ([f32; 3], [f32; 3])) -> Option<super::extrusion::ModelOccluder> {
        if !(self.lon.is_finite() && self.lat.is_finite()) {
            return None;
        }
        let (lo, hi) = bounds;
        let scale = if self.scale.is_finite() { self.scale } else { 1.0 };
        let k = world_per_metre(self.lat) * scale;
        let half = DVec2::new(f64::from(hi[0] - lo[0]) * 0.5, f64::from(hi[1] - lo[1]) * 0.5) * k;
        if !(half.x > 0.0 && half.y > 0.0 && hi[2] > lo[2]) {
            return None;
        }
        let h = if self.heading_deg.is_finite() { self.heading_deg.to_radians() } else { 0.0 };
        // Forward is (sin, cos) east-north: (sin, -cos) in world (y south);
        // left is (-cos, sin) east-north: (-cos, -sin) in world.
        let forward = DVec2::new(h.sin(), -h.cos());
        let left = DVec2::new(-h.cos(), -h.sin());
        let mid = (f64::from(lo[0] + hi[0]) * 0.5, f64::from(lo[1] + hi[1]) * 0.5);
        let center = mercator01(self.lon, self.lat) * f64::from(super::WORLD_SIZE) + (forward * mid.0 + left * mid.1) * k;
        let alt = if self.alt_m.is_finite() { self.alt_m } else { 0.0 } * world_per_metre(self.lat);
        Some(super::extrusion::ModelOccluder { center, forward, half, z: (alt + f64::from(lo[2]) * k, alt + f64::from(hi[2]) * k) })
    }
}

/// The light a layer is drawn in. Directions are in the local
/// east-north-up frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelLight {
    /// Toward the light (normalised on use).
    pub sun_dir: [f32; 3],
    pub sun_color: Rgba,
    pub sun_intensity: f32,
    /// The hemisphere ambient's sky colour (times its own intensity); the
    /// ground side gets 60 % of it.
    pub ambient: Rgba,
    /// The rim term's strength (0 off).
    pub rim: f32,
    /// **The contact shadow**: the opacity (0 none, 1 black at its middle)
    /// of a soft dark ellipse on the ground under each instance -- the
    /// mesh's footprint ([`ModelMesh::bounds`]) a little larger, darkest
    /// in the middle and gone at its rim. Cheap: six vertices an instance,
    /// drawn before the meshes, blended, depth-tested, writing no depth.
    pub contact_shadow: f32,
}

impl ModelLight {
    /// This light with a contact shadow of `opacity` (clamped to 0..=1).
    #[must_use]
    pub fn with_contact_shadow(self, opacity: f32) -> Self {
        Self { contact_shadow: if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 0.0 }, ..self }
    }
}

/// How much bigger than the mesh's footprint the contact shadow is: along
/// the model's length and across it.
pub const SHADOW_SPREAD: [f32; 2] = [1.15, 1.4];

/// The contact shadow's ellipse for `mesh`, mesh metres: `[centre x,
/// centre y, half-length, half-width]` ([`SHADOW_SPREAD`] past the
/// footprint).
#[must_use]
pub fn shadow_ellipse(mesh: &ModelMesh) -> [f32; 4] {
    let (lo, hi) = mesh.bounds();
    [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (hi[0] - lo[0]) / 2.0 * SHADOW_SPREAD[0], (hi[1] - lo[1]) / 2.0 * SHADOW_SPREAD[1]]
}

impl ModelLight {
    /// A floodlit night: a cool high key light, a dim blue sky and a rim.
    pub const NIGHT: Self = Self {
        sun_dir: [0.35, -0.45, 0.82],
        sun_color: Rgba { r: 1.0, g: 0.96, b: 0.9, a: 1.0 },
        sun_intensity: 0.95,
        ambient: Rgba { r: 0.34, g: 0.37, b: 0.46, a: 1.0 },
        rim: 0.45,
        contact_shadow: 0.0,
    };
    /// Daylight: a warm sun, a bright sky, a faint rim.
    pub const DAY: Self = Self {
        sun_dir: [0.45, -0.35, 0.82],
        sun_color: Rgba { r: 1.0, g: 0.97, b: 0.92, a: 1.0 },
        sun_intensity: 1.0,
        ambient: Rgba { r: 0.45, g: 0.47, b: 0.52, a: 1.0 },
        rim: 0.15,
        contact_shadow: 0.0,
    };
}

impl Default for ModelLight {
    fn default() -> Self {
        Self::DAY
    }
}

/// The frame a model draw needs: the arcs' camera state plus the direction
/// toward the eye on a flat map (east-north-up).
#[derive(Debug, Clone, Copy)]
pub struct ModelFrame {
    pub arc: ArcFrame,
    pub view_enu: [f32; 3],
    /// The camera the frame was built from: a near pass re-projects with
    /// it ([`Self::near_flat_view_proj`]).
    pub camera: super::camera::Camera,
    /// The eye in world units relative to the camera origin (x east, y
    /// south, z up): a near-pass layer's per-vertex view direction.
    pub eye_local: DVec3,
}

impl ModelFrame {
    #[must_use]
    pub fn new(camera: &super::camera::Camera, globe: &super::globe::GlobeFrame, px_scale: f32) -> Self {
        let (eye, target, _) = camera.eye_target_up();
        let d = (eye - target).normalize_or(DVec3::Z);
        let arc = ArcFrame::new(camera, globe, px_scale);
        let eye_local = DVec3::new(eye.x - arc.origin_world.x, eye.y - arc.origin_world.y, eye.z);
        Self { arc, view_enu: [d.x as f32, -d.y as f32, d.z as f32], camera: *camera, eye_local }
    }

    /// The flat view-projection a near pass draws with: the frame's own
    /// pose (every point on the same pixel) with `near`'s planes, the
    /// origin and the east-west flip folded in as in
    /// [`ArcFrame::flat_view_proj`].
    #[must_use]
    pub fn near_flat_view_proj(&self, near: &ModelNearPass) -> glam::DMat4 {
        let per_m = 1.0 / crate::camera_control::metres_per_world_unit(self.camera.center_lat);
        let flip = glam::DMat4::from_scale(DVec3::new(-1.0, 1.0, 1.0));
        let o = self.arc.origin_world;
        flip * self.camera.view_projection_near_far_f64(f64::from(near.near_m) * per_m, f64::from(near.far_m) * per_m)
            * glam::DMat4::from_translation(DVec3::new(o.x, o.y, 0.0))
    }

    /// Whether a near pass applies on this frame: flat frames only.
    #[must_use]
    pub fn near_pass_applies(&self) -> bool {
        self.arc.transition <= 0.0
    }
}

/// The uniform, 512 B, in a 512 B dynamic-offset slot per layer.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ModelUniform {
    pub flat_view_proj: [[f32; 4]; 4],
    pub globe_view_proj: [[f32; 4]; 4],
    /// xyz the eye in sphere space, w the transition.
    pub eye: [f32; 4],
    /// xy the anchor's offset from the camera origin in world units (the
    /// nearest world copy), z 0, w the layer opacity (unused: opaque).
    pub anchor: [f32; 4],
    /// xyz toward the sun (ENU), w its intensity.
    pub sun: [f32; 4],
    /// rgb the sun's colour, w the rim strength.
    pub sun_color: [f32; 4],
    /// rgb the ambient sky, w the contact shadow's opacity.
    pub ambient: [f32; 4],
    /// xyz toward the eye on a flat map (ENU), w unused.
    pub view: [f32; 4],
    /// The contact shadow's ellipse ([`shadow_ellipse`]).
    pub shadow: [f32; 4],
    /// xyz the eye relative to the camera origin (world units); w 1 to
    /// light with a per-vertex view direction from it (a near pass), 0 to
    /// use `view`.
    pub eye_local: [f32; 4],
    /// Per material: `[rgba]`, then `[paint, gloss, 0, 0]`.
    pub materials: [[f32; 4]; 2 * MAX_MATERIALS],
}

const _: () = assert!(std::mem::size_of::<ModelUniform>() == 512);

/// The uniform for one layer; `near` a near-pass layer's planes (applied on
/// a flat frame only, [`ModelFrame::near_pass_applies`]).
#[must_use]
pub fn uniform(frame: &ModelFrame, light: &ModelLight, mesh: &ModelMesh, anchor: DVec2, near: Option<&ModelNearPass>) -> ModelUniform {
    let materials = &mesh.materials;
    let f = &frame.arc;
    let world = f64::from(super::WORLD_SIZE);
    let origin = f.origin_world / world;
    // The anchor's world copy nearest the camera, its offset in f64.
    let shift = (anchor.x - origin.x).round();
    let rel = (DVec2::new(anchor.x - shift, anchor.y) - origin) * world;
    let s = DVec3::from_array(light.sun_dir.map(f64::from)).normalize_or(DVec3::Z);
    let mut table = [[0.0f32; 4]; 2 * MAX_MATERIALS];
    for (k, m) in materials.iter().take(MAX_MATERIALS).enumerate() {
        table[2 * k] = [m.color.r, m.color.g, m.color.b, m.color.a];
        table[2 * k + 1] = [if m.paint { 1.0 } else { 0.0 }, m.gloss, m.emissive.clamp(0.0, 1.0), 0.0];
    }
    let near = near.filter(|_| frame.near_pass_applies());
    let flat_view_proj = near.map_or(f.flat_view_proj, |n| frame.near_flat_view_proj(n));
    let e = frame.eye_local;
    ModelUniform {
        flat_view_proj: flat_view_proj.as_mat4().to_cols_array_2d(),
        globe_view_proj: f.globe_view_proj.as_mat4().to_cols_array_2d(),
        eye: [f.eye.x as f32, f.eye.y as f32, f.eye.z as f32, f.transition],
        anchor: [rel.x as f32, rel.y as f32, 0.0, 1.0],
        sun: [s.x as f32, s.y as f32, s.z as f32, light.sun_intensity],
        sun_color: [light.sun_color.r, light.sun_color.g, light.sun_color.b, light.rim],
        ambient: [light.ambient.r, light.ambient.g, light.ambient.b, light.contact_shadow.clamp(0.0, 1.0)],
        view: [frame.view_enu[0], frame.view_enu[1], frame.view_enu[2], 0.0],
        shadow: if light.contact_shadow > 0.0 { shadow_ellipse(mesh) } else { [0.0; 4] },
        eye_local: [e.x as f32, e.y as f32, e.z as f32, if near.is_some() { 1.0 } else { 0.0 }],
        materials: table,
    }
}

/// One layer to draw this frame.
pub struct ModelLayerDraw<'a> {
    pub id: &'a str,
    /// The mesh; uploaded when it is not the `Arc` this layer last drew.
    pub mesh: &'a Arc<ModelMesh>,
    /// Bumped whenever `instances` changes: the upload is skipped otherwise.
    pub generation: u64,
    pub instances: &'a ModelInstances,
    pub light: ModelLight,
    /// One local transform per part id 1..=`parts.len()` (at most
    /// [`MAX_PARTS`]; a part past the end stays put); empty for a rigid
    /// mesh. Shared by every instance of the layer.
    pub parts: &'a [PartMatrix],
    /// Draw this layer in the first-person near pass ([`ModelNearPass`]).
    pub near: Option<ModelNearPass>,
}

/// What the last [`ModelPipeline::prepare`] planned and uploaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelFrameStats {
    pub layers: u64,
    pub instances: u64,
    /// Triangles drawn (mesh triangles times instances).
    pub triangles: u64,
    pub draws: u64,
    /// Mesh bytes uploaded THIS frame: zero unless a mesh changed.
    pub mesh_upload_bytes: u64,
    /// Instance bytes uploaded this frame.
    pub instance_upload_bytes: u64,
    /// Of `draws`, those in the near pass ([`ModelPipeline::draw_near_pass`]).
    pub near_draws: u64,
    /// Part-table bytes written this frame.
    pub parts_upload_bytes: u64,
}

struct GpuMesh {
    mesh: Arc<ModelMesh>,
    vertices: TrackedBuffer,
    indices: TrackedBuffer,
    index_count: u32,
}

struct GpuLayer {
    mesh: Option<GpuMesh>,
    instances: Option<TrackedBuffer>,
    generation: u64,
    count: u32,
}

#[derive(Debug, Clone)]
struct Planned {
    layer: String,
    slot: u32,
    instances: u32,
    /// Draw the contact shadow first.
    shadow: bool,
    /// The layer's part-table slot (0: the shared identity slot).
    parts_slot: u32,
    /// Drawn in the near pass, not the tile pass.
    near: bool,
}

pub struct ModelPipeline {
    pipeline: wgpu::RenderPipeline,
    /// The contact shadows ([`ModelLight::contact_shadow`]): `None` on a
    /// target with no depth (a shadow needs the ground's depth to sit on).
    shadow_pipeline: Option<wgpu::RenderPipeline>,
    bgl: wgpu::BindGroupLayout,
    /// The uniform slots and the bind group over them and the part table
    /// (rebuilt when either grows), with the uniform's slot capacity.
    uniform: Option<(TrackedBuffer, wgpu::BindGroup, u32)>,
    /// The part table's slots (slot 0 the shared identity) and capacity.
    parts: Option<(TrackedBuffer, u32)>,
    slot_size: u64,
    parts_slot_size: u64,
    depth_format: Option<wgpu::TextureFormat>,
    layers: std::collections::HashMap<String, GpuLayer>,
    planned: Vec<Planned>,
    stats: ModelFrameStats,
    mesh_uploads_total: u64,
    instance_uploads_total: u64,
}

impl ModelPipeline {
    /// `depth_format`: the pass's depth attachment (the tile pass's, so the
    /// models and the buildings occlude each other); `None` draws without a
    /// depth test (a test target with no depth).
    #[must_use]
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat, depth_format: Option<wgpu::TextureFormat>) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps model shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("model_shader.wgsl").into()),
        });
        let bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("model-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<ModelUniform>() as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(PARTS_BYTES),
                    },
                    count: None,
                },
            ],
        });
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("model-pipeline-layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("model-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_model"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[
                    Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<ModelVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &VERTEX_ATTRS,
                    }),
                    Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<ModelInstanceGpu>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &INSTANCE_ATTRS,
                    }),
                ],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
            depth_stencil: depth_format.map(|format| wgpu::DepthStencilState {
                format,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_model"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState { format: color_format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let shadow_pipeline = depth_format.map(|format| {
            ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("model-shadow-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_shadow"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<ModelInstanceGpu>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &INSTANCE_ATTRS,
                    })],
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_shadow"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: color_format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        });
        let align = u64::from(ctx.device.limits().min_uniform_buffer_offset_alignment);
        let slot_size = (std::mem::size_of::<ModelUniform>() as u64).div_ceil(align) * align;
        let parts_slot_size = PARTS_BYTES.div_ceil(align) * align;
        Self {
            pipeline,
            shadow_pipeline,
            bgl,
            uniform: None,
            parts: None,
            slot_size,
            parts_slot_size,
            depth_format,
            layers: std::collections::HashMap::new(),
            planned: Vec::new(),
            stats: ModelFrameStats::default(),
            mesh_uploads_total: 0,
            instance_uploads_total: 0,
        }
    }

    /// Uploads any mesh this pipeline does not hold yet and any instance
    /// set whose generation moved, drops layers no longer drawn, writes one
    /// uniform slot per layer and plans the draws. ONE call per frame.
    pub fn prepare(&mut self, ctx: &GpuContext, frame: &ModelFrame, draws: &[ModelLayerDraw<'_>]) {
        self.layers.retain(|id, _| draws.iter().any(|d| d.id == id));
        let mut stats = ModelFrameStats::default();
        for d in draws {
            let layer =
                self.layers.entry(d.id.to_string()).or_insert(GpuLayer { mesh: None, instances: None, generation: u64::MAX, count: 0 });
            let same_mesh = layer.mesh.as_ref().is_some_and(|m| Arc::ptr_eq(&m.mesh, d.mesh));
            if !same_mesh {
                layer.mesh = None;
                if !d.mesh.indices.is_empty() && d.mesh.is_valid() {
                    let init = |label: &str, bytes: &[u8], usage: wgpu::BufferUsages| {
                        TrackedBuffer::init(&ctx.device, &wgpu::util::BufferInitDescriptor { label: Some(label), contents: bytes, usage })
                    };
                    let vertices = init("model-vertices", bytemuck::cast_slice(&d.mesh.vertices), wgpu::BufferUsages::VERTEX);
                    let indices = init("model-indices", bytemuck::cast_slice(&d.mesh.indices), wgpu::BufferUsages::INDEX);
                    layer.mesh = Some(GpuMesh { mesh: d.mesh.clone(), vertices, indices, index_count: d.mesh.indices.len() as u32 });
                    stats.mesh_upload_bytes += d.mesh.upload_bytes() as u64;
                    self.mesh_uploads_total += 1;
                }
            }
            if layer.generation != d.generation {
                let bytes: &[u8] = bytemuck::cast_slice(&d.instances.gpu);
                if !bytes.is_empty() {
                    let need = bytes.len() as u64;
                    if layer.instances.as_ref().is_none_or(|b| b.tracked_size() < need) {
                        layer.instances = Some(TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                            label: Some("model-instances"),
                            size: need.next_power_of_two().max(256),
                            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        }));
                    }
                    if let Some(buffer) = layer.instances.as_ref() {
                        ctx.queue.write_buffer(buffer, 0, bytes);
                    }
                    stats.instance_upload_bytes += need;
                    self.instance_uploads_total += 1;
                }
                layer.count = d.instances.len() as u32;
                layer.generation = d.generation;
            }
        }
        let mut uniforms: Vec<u8> = Vec::new();
        // The part table: slot 0 the shared identity, then one slot per
        // layer that has parts.
        let identity: Vec<u8> = std::iter::repeat_n(PART_IDENTITY, MAX_PARTS).flat_map(|m| bytemuck::bytes_of(&m).to_vec()).collect();
        let mut parts_bytes: Vec<u8> = identity.clone();
        parts_bytes.resize(self.parts_slot_size as usize, 0);
        let mut planned = Vec::new();
        for d in draws {
            let Some(layer) = self.layers.get(d.id) else { continue };
            let Some(mesh) = layer.mesh.as_ref() else { continue };
            if layer.count == 0 || layer.instances.is_none() {
                continue;
            }
            let near = d.near.filter(|_| frame.near_pass_applies());
            let slot = (uniforms.len() as u64 / self.slot_size) as u32;
            uniforms.extend_from_slice(bytemuck::bytes_of(&uniform(frame, &d.light, d.mesh, d.instances.anchor, near.as_ref())));
            uniforms.resize((u64::from(slot + 1) * self.slot_size) as usize, 0);
            let parts_slot = if d.parts.is_empty() {
                0
            } else {
                let s = (parts_bytes.len() as u64 / self.parts_slot_size) as u32;
                let mut table = identity.clone();
                for (k, m) in d.parts.iter().take(MAX_PARTS).enumerate() {
                    let at = k * std::mem::size_of::<PartMatrix>();
                    table[at..at + std::mem::size_of::<PartMatrix>()].copy_from_slice(bytemuck::bytes_of(m));
                }
                parts_bytes.extend_from_slice(&table);
                parts_bytes.resize((u64::from(s + 1) * self.parts_slot_size) as usize, 0);
                stats.parts_upload_bytes += PARTS_BYTES;
                s
            };
            stats.layers += 1;
            stats.instances += u64::from(layer.count);
            stats.triangles += u64::from(mesh.index_count / 3) * u64::from(layer.count);
            stats.draws += 1;
            if near.is_some() {
                stats.near_draws += 1;
            }
            let shadow = d.light.contact_shadow > 0.0;
            if shadow {
                stats.draws += 1;
            }
            planned.push(Planned { layer: d.id.to_string(), slot, instances: layer.count, shadow, parts_slot, near: near.is_some() });
        }
        if !uniforms.is_empty() {
            let slots = (uniforms.len() as u64 / self.slot_size) as u32;
            let part_slots = (parts_bytes.len() as u64 / self.parts_slot_size) as u32;
            let mut rebind = false;
            if self.parts.as_ref().is_none_or(|(_, have)| *have < part_slots) {
                let have = part_slots.next_power_of_two().max(2);
                self.parts = Some((
                    TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                        label: Some("model-parts"),
                        size: u64::from(have) * self.parts_slot_size,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    }),
                    have,
                ));
                rebind = true;
            }
            if self.uniform.as_ref().is_none_or(|(_, _, have)| *have < slots) || rebind {
                let have = self.uniform.as_ref().map_or(0, |(_, _, have)| *have).max(slots.next_power_of_two().max(2));
                let buffer = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                    label: Some("model-uniforms"),
                    size: u64::from(have) * self.slot_size,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let Some((parts, _)) = self.parts.as_ref() else { return };
                let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("model-bind-group"),
                    layout: &self.bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &buffer,
                                offset: 0,
                                size: wgpu::BufferSize::new(std::mem::size_of::<ModelUniform>() as u64),
                            }),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: parts, offset: 0, size: wgpu::BufferSize::new(PARTS_BYTES) }),
                        },
                    ],
                });
                self.uniform = Some((buffer, bind_group, have));
            }
            if let Some((buffer, _, _)) = self.uniform.as_ref() {
                ctx.queue.write_buffer(buffer, 0, &uniforms);
            }
            if let Some((buffer, _)) = self.parts.as_ref() {
                ctx.queue.write_buffer(buffer, 0, &parts_bytes);
            }
        }
        self.planned = planned;
        self.stats = stats;
    }

    /// Whether the last prepare planned a near-pass layer
    /// ([`Self::draw_near_pass`] has something to draw).
    #[must_use]
    pub fn has_near(&self) -> bool {
        self.planned.iter().any(|p| p.near)
    }

    /// What the last prepare planned.
    #[must_use]
    pub fn stats(&self) -> ModelFrameStats {
        self.stats
    }

    /// Mesh uploads since this pipeline was built: an instance update makes
    /// none.
    #[must_use]
    pub fn mesh_uploads_total(&self) -> u64 {
        self.mesh_uploads_total
    }

    /// Instance uploads since this pipeline was built.
    #[must_use]
    pub fn instance_uploads_total(&self) -> u64 {
        self.instance_uploads_total
    }

    /// The dynamic offsets of planned draw `p`: its uniform slot and its
    /// part-table slot.
    fn offsets(&self, p: &Planned) -> [u32; 2] {
        [(u64::from(p.slot) * self.slot_size) as u32, (u64::from(p.parts_slot) * self.parts_slot_size) as u32]
    }

    /// **The near pass** ([`ModelNearPass`]): every near layer the last
    /// prepare planned, in its own render pass over `color` (loaded) with
    /// `depth` cleared (and discarded after). Opens no pass and returns 0
    /// when there is none. `depth` must have this pipeline's depth format.
    pub fn draw_near_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) -> u64 {
        let Some(format) = self.depth_format else { return 0 };
        if !self.has_near() {
            return 0;
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("model-near-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                stencil_ops: format.has_stencil_aspect().then_some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
            }),
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        self.draw_meshes(&mut pass, true)
    }

    /// The meshes of the planned layers in (`near`) or out of the near
    /// pass, in layer order.
    fn draw_meshes(&self, pass: &mut wgpu::RenderPass<'_>, near: bool) -> u64 {
        let Some((_, bind_group, _)) = self.uniform.as_ref() else { return 0 };
        let mut calls = 0;
        for p in self.planned.iter().filter(|p| p.near == near) {
            let Some(layer) = self.layers.get(&p.layer) else { continue };
            let (Some(mesh), Some(instances)) = (layer.mesh.as_ref(), layer.instances.as_ref()) else { continue };
            if calls == 0 {
                pass.set_pipeline(&self.pipeline);
            }
            pass.set_bind_group(0, bind_group, &self.offsets(p));
            pass.set_vertex_buffer(0, mesh.vertices.slice(..));
            let n = u64::from(p.instances) * std::mem::size_of::<ModelInstanceGpu>() as u64;
            pass.set_vertex_buffer(1, instances.slice(..n));
            pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..p.instances);
            calls += 1;
        }
        calls
    }

    /// Draws what [`Self::prepare`] planned for the tile pass (every layer
    /// but the near pass's), in layer order, and returns the draw calls
    /// issued. Leaves its own pipeline and group 0 bound: a caller drawing
    /// more after it rebinds both.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) -> u64 {
        let Some((_, bind_group, _)) = self.uniform.as_ref() else { return 0 };
        let mut calls = 0;
        // The contact shadows first, every layer's, so no mesh is darkened
        // by another's shadow drawn after it.
        if let Some(shadow) = self.shadow_pipeline.as_ref() {
            let mut bound = false;
            for p in self.planned.iter().filter(|p| p.shadow) {
                let Some(instances) = self.layers.get(&p.layer).and_then(|l| l.instances.as_ref()) else { continue };
                if !bound {
                    pass.set_pipeline(shadow);
                    bound = true;
                }
                pass.set_bind_group(0, bind_group, &self.offsets(p));
                let n = u64::from(p.instances) * std::mem::size_of::<ModelInstanceGpu>() as u64;
                pass.set_vertex_buffer(0, instances.slice(..n));
                pass.draw(0..6, 0..p.instances);
                calls += 1;
            }
        }
        calls + self.draw_meshes(pass, false)
    }
}

/// An instance's mesh point `p` (metres, model frame) in LOGICAL screen
/// pixels on a FLAT frame, as `vs_model` places it: the CPU mirror a test
/// or a pick reads. `None` behind the eye.
#[must_use]
pub fn flat_screen(frame: &ModelFrame, instances: &ModelInstances, index: usize, p: [f64; 3]) -> Option<(f64, f64)> {
    let i = instances.gpu.get(index)?;
    let f = &frame.arc;
    let world = f64::from(super::WORLD_SIZE);
    let origin = f.origin_world / world;
    let shift = (instances.anchor.x - origin.x).round();
    let anchor = (DVec2::new(instances.anchor.x - shift, instances.anchor.y) - origin) * world;
    let enu = rotate(p, i.angles.map(f64::from));
    let k = f64::from(i.rel_scale[3]);
    let w = DVec3::new(
        anchor.x + f64::from(i.rel_scale[0]) + enu.x * k,
        anchor.y + f64::from(i.rel_scale[1]) - enu.y * k,
        f64::from(i.rel_scale[2]) + enu.z * k,
    );
    let clip = f.flat_view_proj * w.extend(1.0);
    (clip.w > 0.0).then(|| f.screen_of_clip(clip)).flatten()
}

/// `vs_model`'s rotation: model frame (forward, left, up) to east-north-up,
/// `angles` = (heading clockwise from north, pitch nose-up, roll left-up),
/// radians.
#[must_use]
pub fn rotate(p: [f64; 3], angles: [f64; 3]) -> DVec3 {
    let [h, pitch, roll] = angles;
    let (x, y, z) = (p[0], p[1], p[2]);
    // Roll about +x: left side up.
    let (y, z) = (y * roll.cos() - z * roll.sin(), y * roll.sin() + z * roll.cos());
    // Pitch about +y: nose up.
    let (x, z) = (x * pitch.cos() - z * pitch.sin(), x * pitch.sin() + z * pitch.cos());
    let forward = DVec3::new(h.sin(), h.cos(), 0.0);
    let left = DVec3::new(-h.cos(), h.sin(), 0.0);
    forward * x + left * y + DVec3::Z * z
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::camera::Camera;

    const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

    #[test]
    fn the_sizes_are_what_the_shader_reads() {
        assert_eq!(std::mem::size_of::<ModelVertex>(), 28);
        assert_eq!(std::mem::size_of::<ModelInstanceGpu>(), 48);
        assert_eq!(std::mem::size_of::<ModelUniform>(), 512);
    }

    #[test]
    fn the_model_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("model_shader.wgsl"),
            &[
                ("vs_model", naga::ShaderStage::Vertex),
                ("fs_model", naga::ShaderStage::Fragment),
                ("vs_shadow", naga::ShaderStage::Vertex),
                ("fs_shadow", naga::ShaderStage::Fragment),
            ],
        );
    }

    /// An instance's label occluder is its mesh's box where the model
    /// layer draws it: centred on the box (not the origin), turned by the
    /// heading, scaled, in world units with y south.
    #[test]
    fn a_label_occluder_is_the_instance_box_turned_and_placed() {
        let bounds = ([-6.0, -1.25, 0.3], [6.0, 1.25, 3.3]);
        let k = world_per_metre(1.3);
        let world = |lon: f64, lat: f64| mercator01(lon, lat) * f64::from(super::super::WORLD_SIZE);
        // Facing east: forward is +x in world.
        let east = ModelInstance::at(103.9, 1.3, 90.0).label_occluder(bounds).expect("a box");
        assert!((east.center - world(103.9, 1.3)).length() < 1e-12);
        assert!((east.forward - DVec2::X).length() < 1e-9, "{:?}", east.forward);
        assert!(((east.half.x / k) - 6.0).abs() < 1e-9 && ((east.half.y / k) - 1.25).abs() < 1e-9);
        assert!(((east.z.0 / k) - 0.3).abs() < 1e-6 && ((east.z.1 / k) - 3.3).abs() < 1e-6, "f32 bounds");
        // Facing north, twice the size, the box's middle 1 m ahead of the origin.
        let north = ModelInstance::at(103.9, 1.3, 0.0).with_scale(2.0).label_occluder(([-5.0, -1.0, 0.0], [7.0, 1.0, 3.0])).expect("a box");
        assert!((north.forward - DVec2::new(0.0, -1.0)).length() < 1e-9, "north is -y in world");
        assert!(((world(103.9, 1.3).y - north.center.y) / k - 2.0).abs() < 1e-6, "1 m ahead, at scale 2");
        assert!(((north.half.x / k) - 12.0).abs() < 1e-9);
        assert!(ModelInstance::at(f64::NAN, 1.3, 0.0).label_occluder(bounds).is_none());
        assert!(ModelInstance::at(103.9, 1.3, 0.0).label_occluder(([0.0; 3], [0.0; 3])).is_none());
    }

    /// Heading 0 points the nose north, 90 east; roll lifts the left side.
    #[test]
    fn the_rotation_turns_the_nose_by_heading() {
        let north = rotate([1.0, 0.0, 0.0], [0.0, 0.0, 0.0]);
        assert!((north - DVec3::Y).length() < 1e-12, "{north}");
        let east = rotate([1.0, 0.0, 0.0], [90f64.to_radians(), 0.0, 0.0]);
        assert!((east - DVec3::X).length() < 1e-12, "{east}");
        let left_facing_north = rotate([0.0, 1.0, 0.0], [0.0, 0.0, 0.0]);
        assert!((left_facing_north + DVec3::X).length() < 1e-12, "left of north is west");
        let up = rotate([1.0, 0.0, 0.0], [0.0, 30f64.to_radians(), 0.0]);
        assert!(up.z > 0.49, "nose up");
        let roll = rotate([0.0, 1.0, 0.0], [0.0, 0.0, 30f64.to_radians()]);
        assert!(roll.z > 0.49, "left side up");
    }

    /// Instances are packed relative to the first; one across the
    /// antimeridian from it is a few metres away, not a world.
    #[test]
    fn instances_pack_relative_to_the_anchor_the_short_way() {
        let set = ModelInstances::new(&[ModelInstance::at(179.9999, 0.0, 0.0), ModelInstance::at(-179.9999, 0.0, 0.0), ModelInstance::at(f64::NAN, 0.0, 0.0)]);
        assert_eq!(set.len(), 2, "the NaN drops");
        let gap_m = f64::from(set.gpu[1].rel_scale[0]) / world_per_metre(0.0);
        assert!((gap_m - 22.26).abs() < 0.1, "0.0002 degrees east, not -360: {gap_m}");
        assert_eq!(set.gpu[0].rel_scale[..3], [0.0, 0.0, 0.0]);
        assert_eq!(set.upload_bytes(), 96);
        assert_eq!(set.gpu[0].color, 0, "no colour keeps the paint");
    }

    // ----- GPU -----

    fn camera(lon: f64, lat: f64, zoom: f64) -> Camera {
        Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: 256.0, viewport_height_px: 256.0, fov_y: Camera::DEFAULT_FOV_Y }
    }

    pub(crate) fn device() -> Option<GpuContext> {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions::default()).await.ok()?;
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor { required_limits: wgpu::Limits::downlevel_webgl2_defaults(), ..Default::default() })
                .await
                .ok()?;
            let max_buffer_size = device.limits().max_buffer_size;
            Some(GpuContext { device, queue, max_buffer_size })
        })
    }

    /// A box `lx` long, `wy` wide, `hz` tall standing on the ground at the
    /// origin, faces flat-shaded, in material `m`.
    fn push_box(mesh: &mut ModelMesh, c: [f32; 3], half: [f32; 3], m: u32) {
        let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        for (n, u, v) in faces {
            let base = mesh.vertices.len() as u32;
            for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let p = std::array::from_fn(|k| c[k] + (n[k] + u[k] * su + v[k] * sv) * half[k]);
                mesh.vertices.push(ModelVertex { position: p, normal: n, material: m });
            }
            mesh.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
    }

    /// A test "car": a paint body 40 m long (+x) and 10 m wide, and a black
    /// (unpainted) 10 m block at its nose.
    fn test_mesh() -> Arc<ModelMesh> {
        let mut mesh = ModelMesh {
            materials: vec![
                ModelMaterial::paint(WHITE, 0.0),
                ModelMaterial::new(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }, 0.0),
            ],
            ..ModelMesh::default()
        };
        push_box(&mut mesh, [0.0, 0.0, 5.0], [20.0, 5.0, 5.0], 0);
        push_box(&mut mesh, [25.0, 0.0, 5.0], [5.0, 5.0, 5.0], 1);
        Arc::new(mesh)
    }

    /// A flat light: ambient only, so a pixel's colour is the material's
    /// (or the tint) times a known factor.
    const FLAT: ModelLight = ModelLight {
        sun_dir: [0.0, 0.0, 1.0],
        sun_color: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        sun_intensity: 0.0,
        ambient: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        rim: 0.0,
        contact_shadow: 0.0,
    };

    /// **The contact shadow**: a car with one darkens the white ground
    /// just beside its body, not far from it, and draws one more call; a
    /// car without it leaves the ground white. The shadow writes no depth,
    /// so the body over it is its own colour.
    #[test]
    fn a_contact_shadow_darkens_the_ground_beside_the_car_only() {
        let Some(ctx) = device() else { return };
        let frame = frame();
        // The ground: a white slab 1 cm thick, 200 m across, drawn first.
        let mut slab = ModelMesh { materials: vec![ModelMaterial::new(WHITE, 0.0)], ..ModelMesh::default() };
        push_box(&mut slab, [0.0, 0.0, 0.005], [100.0, 100.0, 0.005], 0);
        let slab = Arc::new(slab);
        let at = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0)]);
        let mut ground = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        ground.prepare(&ctx, &frame, &[ModelLayerDraw { id: "ground", mesh: &slab, generation: 1, instances: &at, light: FLAT, parts: &[], near: None }]);
        // A low car (half a metre tall), so its roof hides no more ground
        // than its footprint from the eye ~66 m up.
        let mut low = ModelMesh { materials: vec![ModelMaterial::paint(WHITE, 0.0)], ..ModelMesh::default() };
        push_box(&mut low, [5.0, 0.0, 0.25], [25.0, 5.0, 0.25], 0);
        let mesh = Arc::new(low);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let car = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(red)]);
        let shot = |light: ModelLight| {
            let mut pipeline = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
            pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "car", mesh: &mesh, generation: 1, instances: &car, light, parts: &[], near: None }]);
            (render(&ctx, &pipeline, Some(&ground)), pipeline.stats().draws)
        };
        let (plain, plain_draws) = shot(FLAT);
        let (shaded, shaded_draws) = shot(FLAT.with_contact_shadow(1.0));
        assert_eq!((plain_draws, shaded_draws), (1, 2), "the shadow is one more draw");
        // Beside the body (5 m half-width; the shadow reaches 7 m).
        let beside = flat_screen(&frame, &car, 0, [5.0, 5.6, 0.0]).unwrap();
        let far = flat_screen(&frame, &car, 0, [5.0, 25.0, 0.0]).unwrap();
        let body = flat_screen(&frame, &car, 0, [0.0, 0.0, 0.5]).unwrap();
        assert!(px(&plain, beside)[0] > 250, "no shadow: white ground {:?}", px(&plain, beside));
        assert!(px(&shaded, beside)[0] < 240, "the shadow darkens the ground beside the car: {:?}", px(&shaded, beside));
        assert!(px(&shaded, far)[0] > 250, "and not far from it: {:?}", px(&shaded, far));
        assert_eq!(px(&shaded, body), px(&plain, body), "the body over the shadow is its own colour");
        let e = shadow_ellipse(&mesh);
        assert!((e[0] - 5.0).abs() < 1e-6 && (e[2] - 25.0 * SHADOW_SPREAD[0]).abs() < 1e-4 && (e[3] - 5.0 * SHADOW_SPREAD[1]).abs() < 1e-4, "{e:?}");
    }

    struct Target {
        color: wgpu::Texture,
        depth: wgpu::Texture,
        read: TrackedBuffer,
    }

    const SIDE: u32 = 256;
    const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;

    fn target(ctx: &GpuContext) -> Target {
        let size = wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 };
        let tex = |format, usage| {
            ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("model-test"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        Target {
            color: tex(wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC),
            depth: tex(DEPTH, wgpu::TextureUsages::RENDER_ATTACHMENT),
            read: TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                label: Some("model-test-read"),
                size: u64::from(SIDE * SIDE * 4),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
        }
    }

    /// Renders over black with a cleared depth buffer; `before` draws
    /// first in the same pass (an occluder writing depth).
    fn render(ctx: &GpuContext, pipeline: &ModelPipeline, before: Option<&ModelPipeline>) -> Vec<u8> {
        let t = target(ctx);
        let view = t.color.create_view(&Default::default());
        let depth = t.depth.create_view(&Default::default());
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("model-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(first) = before {
                first.draw(&mut pass);
            }
            pipeline.draw(&mut pass);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &t.color, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &t.read,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIDE * 4), rows_per_image: Some(SIDE) },
            },
            wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        t.read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let out = t.read.slice(..).get_mapped_range().expect("mapped").to_vec();
        out
    }

    fn px(img: &[u8], (x, y): (f64, f64)) -> [u8; 4] {
        let (x, y) = (x.round() as i64, y.round() as i64);
        assert!((0..i64::from(SIDE)).contains(&x) && (0..i64::from(SIDE)).contains(&y), "on screen: {x},{y}");
        let i = ((y as usize) * SIDE as usize + x as usize) * 4;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    /// Zoom 18 at the equator: 3.35 px a metre, the 50 m test car ~170
    /// px; the eye ~130 m up.
    fn frame() -> ModelFrame {
        let camera = camera(0.0, 0.0, 18.0);
        ModelFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0)
    }

    /// **The right spot, turned by heading, the paint tinted.** One car at
    /// the centre facing north (heading 0) painted red: its body at the
    /// centre is red, its black nose is NORTH of the centre (up the
    /// screen), the screen to its east is empty. Turned to heading 90 the
    /// nose is east. An instance update uploads no mesh bytes.
    #[test]
    fn a_mesh_draws_at_its_spot_turned_by_heading_with_the_paint_tinted() {
        let Some(ctx) = device() else { return };
        let mesh = test_mesh();
        let frame = frame();
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let mut pipeline = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        let north = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(red)]);
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "cars", mesh: &mesh, generation: 1, instances: &north, light: FLAT, parts: &[], near: None }]);
        assert_eq!(pipeline.stats().triangles, 24);
        assert!(pipeline.stats().mesh_upload_bytes > 0);
        let img = render(&ctx, &pipeline, None);
        let centre = flat_screen(&frame, &north, 0, [0.0, 0.0, 10.0]).expect("on screen");
        assert!((centre.0 - 128.0).abs() < 1.0 && (centre.1 - 128.0).abs() < 1.0, "the anchor is the camera centre: {centre:?}");
        let body = px(&img, centre);
        assert!(body[0] > 200 && body[1] < 30 && body[2] < 30, "the paint is the instance's red: {body:?}");
        let nose = flat_screen(&frame, &north, 0, [25.0, 0.0, 10.0]).unwrap();
        assert!(nose.1 < centre.1 - 30.0, "facing north, the nose is up the screen: {nose:?}");
        assert_eq!(px(&img, nose)[..3], [0, 0, 0], "the nose keeps its own black");
        let east_px = (centre.0 + (nose.1 - centre.1).abs(), centre.1);
        assert_eq!(px(&img, east_px)[..3], [0, 0, 0], "nothing east of a north-facing car");
        let side = flat_screen(&frame, &north, 0, [0.0, 4.0, 10.0]).unwrap();
        assert!(px(&img, side)[0] > 200, "four metres to its left is still body: {side:?}");

        // Turned east: an instance update, no mesh upload.
        let meshes = pipeline.mesh_uploads_total();
        let east = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 90.0).with_color(red)]);
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "cars", mesh: &mesh, generation: 2, instances: &east, light: FLAT, parts: &[], near: None }]);
        assert_eq!(pipeline.mesh_uploads_total(), meshes, "an instance update uploads no mesh");
        assert_eq!(pipeline.stats().mesh_upload_bytes, 0);
        assert_eq!(pipeline.stats().instance_upload_bytes, 48);
        let img = render(&ctx, &pipeline, None);
        let nose_e = flat_screen(&frame, &east, 0, [25.0, 0.0, 10.0]).unwrap();
        assert!(nose_e.0 > centre.0 + 30.0, "facing east, the nose is right of centre: {nose_e:?}");
        assert!(px(&img, nose_e)[0] < 30, "the black nose is there");
        let tail = (centre.0 - 25.0, centre.1);
        assert!(px(&img, tail)[0] > 200, "the body runs back west: {tail:?}");
        assert_eq!(px(&img, (centre.0, centre.1 - 40.0))[..3], [0, 0, 0], "nothing north any more");

        // A same-generation frame (the camera moved) uploads nothing.
        let uploads = pipeline.instance_uploads_total();
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "cars", mesh: &mesh, generation: 2, instances: &east, light: FLAT, parts: &[], near: None }]);
        assert_eq!(pipeline.instance_uploads_total(), uploads);
        assert_eq!(pipeline.stats().instance_upload_bytes + pipeline.stats().mesh_upload_bytes, 0);
    }

    /// **Untinted, the paint keeps its own colour; two instances, two
    /// tints.**
    #[test]
    fn each_instance_tints_its_own_paint() {
        let Some(ctx) = device() else { return };
        let mesh = test_mesh();
        let frame = frame();
        let green = Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
        // 30 m apart east-west, both facing north.
        let lon = 30.0 / (EARTH_CIRCUMFERENCE_M / 360.0);
        let set = ModelInstances::new(&[ModelInstance::at(-lon / 2.0, 0.0, 0.0), ModelInstance::at(lon / 2.0, 0.0, 0.0).with_color(green)]);
        let mut pipeline = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "cars", mesh: &mesh, generation: 1, instances: &set, light: FLAT, parts: &[], near: None }]);
        let img = render(&ctx, &pipeline, None);
        let a = px(&img, flat_screen(&frame, &set, 0, [0.0, 0.0, 10.0]).unwrap());
        let b = px(&img, flat_screen(&frame, &set, 1, [0.0, 0.0, 10.0]).unwrap());
        assert!(a[0] > 200 && a[1] > 200 && a[2] > 200, "untinted: the material's white {a:?}");
        assert!(b[1] > 200 && b[0] < 30 && b[2] < 30, "tinted green {b:?}");
    }

    /// **Depth.** A tall model drawn first occludes a low one drawn after it
    /// where they overlap, whatever the order; a low one drawn first is
    /// covered by a tall one drawn after it.
    #[test]
    fn models_are_occluded_by_depth_not_by_order() {
        let Some(ctx) = device() else { return };
        let mesh = test_mesh();
        let frame = frame();
        let (red, blue) = (Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }, Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 });
        // A "building": the body scaled up 2x (20 m tall), blue.
        let tall = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(blue).with_scale(2.0)]);
        let low = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(red)]);
        let mut first = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        let mut second = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        first.prepare(&ctx, &frame, &[ModelLayerDraw { id: "building", mesh: &mesh, generation: 1, instances: &tall, light: FLAT, parts: &[], near: None }]);
        second.prepare(&ctx, &frame, &[ModelLayerDraw { id: "car", mesh: &mesh, generation: 1, instances: &low, light: FLAT, parts: &[], near: None }]);
        let centre = flat_screen(&frame, &low, 0, [0.0, 0.0, 10.0]).unwrap();
        // Building first, car after: the car is under the roof, hidden.
        let img = render(&ctx, &second, Some(&first));
        let c = px(&img, centre);
        assert!(c[2] > 200 && c[0] < 30, "the car under the roof is hidden by depth: {c:?}");
        // Car first, building after: the same.
        let img = render(&ctx, &first, Some(&second));
        let c = px(&img, centre);
        assert!(c[2] > 200 && c[0] < 30, "drawn first or last, the roof is in front: {c:?}");
    }

    /// **A model stands over the line it stands on.** A thick trips line
    /// (a bus's trail) runs east-west under a car, both drawn in one depth
    /// pass in the order the frame emits them
    /// ([`crate::render::gpu::push_trips_and_models`]): the pixel at the
    /// car's centre is the car's green, not the line's red, while the line
    /// beside the car is still drawn. Before, the trips drew in the symbol
    /// pass after the tile pass, over every model.
    #[test]
    fn a_model_covers_the_trips_line_under_it() {
        use crate::render::gpu::{push_trips_and_models, TileDraw};
        use crate::render::trips::{TripFrame, TripInput, TripLayerDraw, TripPipeline, TripPoint, TripSet, TripStyle};
        let Some(ctx) = device() else { return };
        let mesh = test_mesh();
        let frame = frame();
        let green = Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let car = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(green)]);
        let mut models = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        models.prepare(&ctx, &frame, &[ModelLayerDraw { id: "cars", mesh: &mesh, generation: 1, instances: &car, light: FLAT, parts: &[], near: None }]);
        let point = |lon: f64, time: f64| TripPoint { lon, lat: 0.0, alt_m: 0.0, time };
        let set = TripSet::from_trips(&[TripInput { row: 0, points: vec![point(-0.0005, 0.0), point(0.0005, 10.0)], color: red, label: None }]);
        let cam = camera(0.0, 0.0, 18.0);
        let trip_frame = TripFrame::new(&cam, &crate::render::globe::globe_frame(&cam).flattened(), 1.0);
        let mut trips = TripPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None).with_depth_pass(&ctx, DEPTH);
        let style = TripStyle { width_px: 40.0, trail: 1000.0, fade: false, ..TripStyle::default() };
        trips.prepare(&ctx, &trip_frame, &[TripLayerDraw { id: "trail", generation: 1, set: &set, style, clock: Some(10.0) }]);
        let mut draws = Vec::new();
        push_trips_and_models(Some(&trips), &models, &mut draws);
        assert!(matches!(draws.as_slice(), [TileDraw::Trips(_), TileDraw::Models(_)]), "the trips first, the models over them");

        let t = target(&ctx);
        let view = t.color.create_view(&Default::default());
        let depth = t.depth.create_view(&Default::default());
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("model-over-trips-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            for draw in &draws {
                match draw {
                    TileDraw::Trips(trips) => assert_eq!(trips.draw_in_depth_pass(&mut pass), 1, "one trail draw, no head"),
                    TileDraw::Models(models) => assert!(models.draw(&mut pass) > 0),
                    _ => unreachable!(),
                }
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &t.color, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &t.read,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIDE * 4), rows_per_image: Some(SIDE) },
            },
            wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        t.read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let img = t.read.slice(..).get_mapped_range().expect("mapped").to_vec();
        let centre = flat_screen(&frame, &car, 0, [0.0, 0.0, 10.0]).expect("on screen");
        let c = px(&img, centre);
        assert!(c[1] > 200 && c[0] < 30, "the car's green over the line, not the line's red: {c:?}");
        let beside = px(&img, (centre.0 + 80.0, centre.1));
        assert!(beside[0] > 200 && beside[1] < 30, "the line beside the car is drawn: {beside:?}");
    }

    /// The sun lights a face that looks at it more than one that does not;
    /// at night the rim keeps a black body's silhouette above black.
    #[test]
    fn the_sun_and_the_rim_light_the_faces() {
        let Some(ctx) = device() else { return };
        let mut mesh = ModelMesh { materials: vec![ModelMaterial::paint(WHITE, 0.0)], ..ModelMesh::default() };
        push_box(&mut mesh, [0.0, 0.0, 5.0], [20.0, 20.0, 5.0], 0);
        let mesh = Arc::new(mesh);
        let frame = frame();
        let set = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 })]);
        let mut pipeline = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        let lit = |sun_dir: [f32; 3], pipeline: &mut ModelPipeline| {
            let light = ModelLight { sun_dir, sun_intensity: 1.0, ambient: Rgba { r: 0.2, g: 0.2, b: 0.2, a: 1.0 }, rim: 0.0, ..FLAT };
            pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "m", mesh: &mesh, generation: 1, instances: &set, light, parts: &[], near: None }]);
            px(&render(&ctx, pipeline, None), (128.0, 128.0))[0]
        };
        let overhead = lit([0.0, 0.0, 1.0], &mut pipeline);
        let grazing = lit([1.0, 0.0, 0.05], &mut pipeline);
        assert!(overhead > grazing + 40, "the roof faces an overhead sun: {overhead} vs {grazing}");
    }

    /// **Globe.** On a full globe a (huge) model facing the eye is drawn at
    /// its spot; one on the far side is hidden by the planet.
    #[test]
    fn the_globe_places_models_on_the_sphere_and_hides_the_far_side() {
        let Some(ctx) = device() else { return };
        let mesh = test_mesh();
        let camera = camera(0.0, 0.0, 1.0);
        let globe = crate::render::globe::globe_frame(&camera);
        assert!(globe.transition >= 1.0);
        let frame = ModelFrame::new(&camera, &globe, 1.0);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        // 20,000x: a 1,000 km body.
        let near = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(red).with_scale(20_000.0)]);
        let far = ModelInstances::new(&[ModelInstance::at(180.0, 0.0, 0.0).with_color(red).with_scale(20_000.0)]);
        let mut pipeline = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "m", mesh: &mesh, generation: 1, instances: &near, light: FLAT, parts: &[], near: None }]);
        let img = render(&ctx, &pipeline, None);
        let (x, y) = frame.arc.project_globe((0.0, 0.0), (0.001, 0.0), 0.0, 0.0).expect("visible");
        assert!(px(&img, (x, y))[0] > 150, "the near model is at its spot on the sphere");
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "m", mesh: &mesh, generation: 2, instances: &far, light: FLAT, parts: &[], near: None }]);
        let img = render(&ctx, &pipeline, None);
        assert!(img.chunks(4).all(|p| p[0] == 0), "the far-side model is behind the planet");
    }

    /// **Parts move by their table.** A box tagged part 1 drawn with a part
    /// table that slides it 20 m to the model's left is drawn there, and not
    /// where it was built; the same mesh with no table (and part 0 meshes
    /// beside it) stays put. The table costs bytes only for the layer that
    /// has one.
    #[test]
    fn a_part_moves_by_its_matrix_and_the_rest_stays() {
        let Some(ctx) = device() else { return };
        let frame = frame();
        let mut mesh = ModelMesh { materials: vec![ModelMaterial::paint(WHITE, 0.0)], ..ModelMesh::default() };
        push_box(&mut mesh, [0.0, 0.0, 2.0], [3.0, 3.0, 2.0], 0);
        for v in &mut mesh.vertices {
            v.material = ModelVertex::pack(0, 1);
        }
        assert_eq!((mesh.vertices[0].slot(), mesh.vertices[0].part()), (0, 1));
        let mesh = Arc::new(mesh);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let set = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(red)]);
        let slide: PartMatrix = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 20.0], [0.0, 0.0, 1.0, 0.0]];
        assert_eq!(part_point(&slide, [1.0, 2.0, 3.0]), [1.0, 22.0, 3.0]);
        let mut pipeline = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
        let parts = [slide];
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "m", mesh: &mesh, generation: 1, instances: &set, light: FLAT, parts: &parts, near: None }]);
        assert_eq!(pipeline.stats().parts_upload_bytes, PARTS_BYTES);
        let img = render(&ctx, &pipeline, None);
        let home = flat_screen(&frame, &set, 0, [0.0, 0.0, 4.0]).unwrap();
        let moved = flat_screen(&frame, &set, 0, [0.0, 20.0, 4.0]).unwrap();
        assert!(px(&img, moved)[0] > 200, "the part is drawn where its matrix put it: {moved:?}");
        assert_eq!(px(&img, home)[..3], [0, 0, 0], "and not where it was built");
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "m", mesh: &mesh, generation: 1, instances: &set, light: FLAT, parts: &[], near: None }]);
        assert_eq!(pipeline.stats().parts_upload_bytes, 0, "no table, no bytes");
        let img = render(&ctx, &pipeline, None);
        assert!(px(&img, home)[0] > 200, "with no table the part stays put");
    }

    /// **The near pass draws what the scene clips at arm's length.** An eye
    /// camera 0.86 m over the road, pitched 75 degrees (the in-car view's
    /// pose): the scene's own near plane is ~5% of the eye-to-target
    /// distance. A box 0.30 m ahead of the eye and one 0.06 m ahead are
    /// drawn by the near pass (its 2 cm near plane); the 0.06 m one is
    /// clipped away when the same layer draws in the scene pass. A scene
    /// wall 0.25 m ahead, nearer than the 0.30 m box, does not hide it: the
    /// near pass clears the depth and draws over the scene.
    #[test]
    fn a_near_pass_layer_is_not_clipped_at_arms_length_and_draws_over_the_scene() {
        let Some(ctx) = device() else { return };
        let pitch = 75f64.to_radians();
        let eye_h = 0.86;
        let dist_m = eye_h / pitch.cos();
        let dist_w = dist_m / crate::camera_control::metres_per_world_unit(0.0);
        let fov = Camera::DEFAULT_FOV_Y;
        let zoom = (f64::from(SIDE) / (2.0 * (fov / 2.0).tan() * dist_w)).log2();
        let cam = Camera { pitch: pitch as f32, ..camera(0.0, 0.0, zoom) };
        assert!((cam.eye_height_m() - eye_h).abs() < 1e-6, "the eye {} m up", cam.eye_height_m());
        let scene_near = cam.near_m();
        assert!(scene_near > 0.1, "the scene's near plane {scene_near} m clips anything nearer");
        let frame = ModelFrame::new(&cam, &crate::render::globe::globe_frame(&cam).flattened(), 1.0);
        // The model sits at the look-at point facing north (the view's
        // way); the eye is `dist_m sin p` back and `eye_h` up in its frame.
        let eye = [-dist_m * pitch.sin(), 0.0, eye_h];
        let ahead = |m: f64, left: f64| [eye[0] + m * pitch.sin(), left, eye[2] - m * pitch.cos()];
        let f = |p: [f64; 3]| p.map(|v| v as f32);
        let mut near_mesh = ModelMesh { materials: vec![ModelMaterial::paint(WHITE, 0.0)], ..ModelMesh::default() };
        let (a, b) = (ahead(0.30, 0.0), ahead(0.06, 0.02));
        push_box(&mut near_mesh, f(a), [0.03, 0.03, 0.03], 0);
        push_box(&mut near_mesh, f(b), [0.006, 0.006, 0.006], 0);
        let near_mesh = Arc::new(near_mesh);
        let mut wall = ModelMesh { materials: vec![ModelMaterial::new(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }, 0.0)], ..ModelMesh::default() };
        push_box(&mut wall, f(ahead(0.25, 0.0)), [0.01, 0.2, 0.2], 0);
        let wall = Arc::new(wall);
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let set = ModelInstances::new(&[ModelInstance::at(0.0, 0.0, 0.0).with_color(red)]);
        let pa = flat_screen(&frame, &set, 0, a).expect("ahead");
        let pb = flat_screen(&frame, &set, 0, b).expect("ahead");
        assert!((pa.0 - 128.0).abs() < 2.0 && (pa.1 - 128.0).abs() < 2.0, "the 0.30 m box on the view's centre: {pa:?}");
        let shot = |near: Option<ModelNearPass>, with_wall: bool| -> (Vec<u8>, ModelFrameStats) {
            let mut pipeline = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(DEPTH));
            let mut draws = vec![ModelLayerDraw { id: "own", mesh: &near_mesh, generation: 1, instances: &set, light: FLAT, parts: &[], near }];
            if with_wall {
                draws.push(ModelLayerDraw { id: "wall", mesh: &wall, generation: 1, instances: &set, light: FLAT, parts: &[], near: None });
            }
            pipeline.prepare(&ctx, &frame, &draws);
            (render_with_near(&ctx, &pipeline), pipeline.stats())
        };
        let (scene, stats) = shot(None, false);
        assert_eq!(stats.near_draws, 0);
        assert!(px(&scene, pa)[0] > 120, "the 0.30 m box is inside the scene's frustum: {:?}", px(&scene, pa));
        assert_eq!(px(&scene, pb)[..3], [0, 0, 0], "the scene pass clips the 0.06 m box ({scene_near:.3} m near plane)");
        let (near, stats) = shot(Some(ModelNearPass::FIRST_PERSON), false);
        assert_eq!(stats.near_draws, 1);
        assert!(px(&near, pa)[0] > 120, "the near pass draws the 0.30 m box: {:?}", px(&near, pa));
        assert!(px(&near, pb)[0] > 120, "and the 0.06 m one, unclipped: {:?}", px(&near, pb));
        let (over, _) = shot(Some(ModelNearPass::FIRST_PERSON), true);
        let c = px(&over, pa);
        assert!(c[0] > 120 && c[2] < 30, "the near box over the nearer scene wall: {c:?}");
        let beside = px(&over, (pa.0 + 40.0, pa.1));
        assert!(beside[2] > 120, "the wall is drawn where the near layer is not: {beside:?}");
    }

    /// [`render`] for a frame with a near pass: the scene pass (cleared),
    /// then [`ModelPipeline::draw_near_pass`] on the same colour target.
    fn render_with_near(ctx: &GpuContext, pipeline: &ModelPipeline) -> Vec<u8> {
        let t = target(ctx);
        let view = t.color.create_view(&Default::default());
        let depth = t.depth.create_view(&Default::default());
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("model-test-scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Store }),
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pipeline.draw(&mut pass);
        }
        pipeline.draw_near_pass(&mut encoder, &view, &depth, None);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &t.color, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &t.read,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIDE * 4), rows_per_image: Some(SIDE) },
            },
            wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        t.read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let out = t.read.slice(..).get_mapped_range().expect("mapped").to_vec();
        out
    }
}
