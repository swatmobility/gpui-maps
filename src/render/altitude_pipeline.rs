//! The altitude layer's point pipeline (spec 2026-09-29-satellites-design.md
//! §2.3-2.5, §2.9): one instanced screen-space quad per object, placed in the
//! globe's unit-sphere frame and drawn above it.
//!
//! - **Where**: the vertex stage evaluates the Hermite between two keyframes
//!   (a static object has `p0 == p1` and zero velocities), rotates it by
//!   `M(t)`, and projects it through `globe_view_proj`. It writes its OWN clip
//!   z (`0.5 w`), so the globe projection's far plane (`d + 2R`, sized to hold
//!   the sphere) never clips a GEO object behind the planet's centre plane.
//!   The near plane is kept as a cull, so nothing behind the eye mirrors
//!   through `w < 0`.
//! - **Hidden by the planet**: the analytic closest-approach test of
//!   [`super::altitude::visibility`], per instance in the vertex stage, never
//!   by depth. Its soft edge is [`occlusion_eps`], held to about 2 px wide on
//!   screen however close the eye is to the planet.
//! - **Blending**: premultiplied "over", unsorted (R4), with the emphasised
//!   (hovered, selected, halo) objects drawn last, as their own draw.
//! - **Portability**: instances are vertex attributes (six, [`POINT_ATTRS`]),
//!   two inter-stage vectors, scalar `u32` flag unpacking only.
//!
//! Draw order in `MapView::paint`: inside the symbol pass, right after the
//! donuts and before the icons and labels -- over the atmosphere and the
//! tiles, UNDER the chrome (the brand mark, compass and credit ride in the
//! icon and label draws). A plan ruling; see `map_view/altitude.rs`.

use glam::{DMat3, DMat4, DVec3};

use super::alloc::TrackedBuffer;
use super::altitude::{eye_sphere, px_per_radius, KeyState};
use super::globe::GLOBE_RADIUS_M;
use super::camera::Camera;
use super::globe::GlobeFrame;
use super::gpu::GpuContext;
use crate::style::Rgba;

/// One object on the GPU: 60 B, read per instance. Positions in Earth radii
/// in the sphere frame, velocities in radii per second.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PointInstance {
    pub p0: [f32; 3],
    pub v0: [f32; 3],
    pub p1: [f32; 3],
    pub v1: [f32; 3],
    /// Paint index in bits 0-3, then [`FLAG_VISIBLE`], [`FLAG_HOVERED`],
    /// [`FLAG_SELECTED`], [`FLAG_HALO`]. See [`pack_flags`].
    pub flags: u32,
    /// The window of `s` (the fraction of the keyframe interval) over which
    /// the object exists; outside it the quad is culled. `[0, 1]`: always.
    pub hidden_s: [f32; 2],
}

const _: () = assert!(std::mem::size_of::<PointInstance>() == 60);

pub const FLAG_VISIBLE: u32 = 1 << 4;
pub const FLAG_HOVERED: u32 = 1 << 5;
pub const FLAG_SELECTED: u32 = 1 << 6;
pub const FLAG_HALO: u32 = 1 << 7;

/// The palette slot [`AltitudeStyle::outline`] is written into; a paint
/// index can use `0..OUTLINE_SLOT` only.
pub const OUTLINE_SLOT: usize = 15;

/// Sizes of the emphasised states, px: hovered, selected, halo (the ISS,
/// spec §4.2).
pub const EMPHASIS_PX: [f32; 3] = [5.0, 7.0, 6.0];

/// The outline ring's width, px (spec §1.2).
pub const OUTLINE_PX: f32 = 1.0;
/// The white halo's width outside the outline, px (spec §4.2). A halo
/// object also gets a second outline ring, [`OUTLINE_PX`] wide in the
/// outline colour, outside the white: on light ground (Positron land,
/// Mapbox Light) the white alone is invisible, and the dark ring keeps its
/// edge readable (satellites Task 10).
pub const HALO_PX: f32 = 1.5;

/// The instance layout: `p0`, `v0`, `p1`, `v1`, `flags`, `hidden_s`.
pub const POINT_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x3, // p0
    1 => Float32x3, // v0
    2 => Float32x3, // p1
    3 => Float32x3, // v1
    4 => Uint32,    // flags
    5 => Float32x2, // hidden_s
];

/// The flags word: `paint` (clamped to `0..=14`, 15 being the outline's
/// slot) in the low four bits, then one bit per state.
#[must_use]
pub fn pack_flags(paint: u8, visible: bool, hovered: bool, selected: bool, halo: bool) -> u32 {
    u32::from(paint.min(OUTLINE_SLOT as u8 - 1))
        | if visible { FLAG_VISIBLE } else { 0 }
        | if hovered { FLAG_HOVERED } else { 0 }
        | if selected { FLAG_SELECTED } else { 0 }
        | if halo { FLAG_HALO } else { 0 }
}

/// A still object at sphere point `p`: both keyframes at `p`, no velocity,
/// present for the whole interval. A non-finite `p` is emitted invisible.
#[must_use]
pub fn static_instance(p: DVec3, flags: u32) -> PointInstance {
    let q = p.as_vec3().to_array();
    let flags = if p.is_finite() { flags } else { flags & !FLAG_VISIBLE };
    PointInstance { p0: q, v0: [0.0; 3], p1: q, v1: [0.0; 3], flags, hidden_s: [0.0, 1.0] }
}

/// A moving object between keyframes `k0` and `k1`, `delta_s` apart:
/// positions in Earth radii and velocities in radii per second, in the
/// keyframes' own frame (the shader applies `M`). `hidden_s` is the window
/// of `s` it exists over ([`super::altitude::hidden_bounds`]).
///
/// A missing end (an object that decays or launches mid-interval) is
/// extrapolated from the valid one along its velocity, `p ± v Δ`, so the
/// Hermite stays on the orbit's tangent over the part that is drawn and
/// never heads for the origin. Neither end valid, or a non-finite state:
/// emitted invisible.
#[must_use]
pub fn keyed_instance(k0: &KeyState, k1: &KeyState, delta_s: f64, hidden_s: [f32; 2], flags: u32) -> PointInstance {
    let (a, b) = match (k0.valid, k1.valid) {
        (true, true) => (*k0, *k1),
        (true, false) => (*k0, KeyState { position_m: k0.position_m + k0.velocity_m_s * delta_s, velocity_m_s: k0.velocity_m_s, valid: true }),
        (false, true) => (KeyState { position_m: k1.position_m - k1.velocity_m_s * delta_s, velocity_m_s: k1.velocity_m_s, valid: true }, *k1),
        (false, false) => (KeyState::HIDDEN, KeyState::HIDDEN),
    };
    let r = GLOBE_RADIUS_M;
    let finite = [a.position_m, a.velocity_m_s, b.position_m, b.velocity_m_s].iter().all(|v| v.is_finite()) && delta_s.is_finite();
    let shown = finite && (k0.valid || k1.valid) && hidden_s[0] <= hidden_s[1];
    let q = |v: DVec3| if finite { (v / r).as_vec3().to_array() } else { [0.0; 3] };
    PointInstance {
        p0: q(a.position_m),
        v0: q(a.velocity_m_s),
        p1: q(b.position_m),
        v1: q(b.velocity_m_s),
        flags: if shown { flags } else { flags & !FLAG_VISIBLE },
        hidden_s: if shown { hidden_s } else { [1.0, 0.0] },
    }
}

/// [`instances_for`] for a keyframed layer: object `i` between `k0[i]` and
/// `k1[i]` over window `hidden[i]` (missing: `[0, 1]`), as the main draw's
/// instances and the emphasis draw's.
#[must_use]
pub fn keyed_instances_for(
    k0: &[KeyState],
    k1: &[KeyState],
    delta_s: f64,
    hidden: &[[f32; 2]],
    style: &AltitudeStyle,
    visible: &[bool],
    emphasis: (Option<usize>, Option<usize>, Option<usize>),
) -> (Vec<PointInstance>, Vec<PointInstance>) {
    let n = k0.len().min(k1.len());
    let hs = |i: usize| hidden.get(i).copied().unwrap_or([0.0, 1.0]);
    let main = (0..n).map(|i| keyed_instance(&k0[i], &k1[i], delta_s, hs(i), main_flags(style, visible, emphasis, i))).collect();
    let mut out: Vec<PointInstance> = Vec::with_capacity(3);
    let mut seen: Vec<usize> = Vec::with_capacity(3);
    for i in [emphasis.0, emphasis.1, emphasis.2].into_iter().flatten().filter(|&i| i < n) {
        if seen.contains(&i) {
            continue;
        }
        seen.push(i);
        let (h, s, g) = emphasis_of(i, emphasis);
        let shown = visible.get(i).copied().unwrap_or(true);
        out.push(keyed_instance(&k0[i], &k1[i], delta_s, hs(i), pack_flags(style.paint_of(i), shown, h, s, g)));
    }
    (main, out)
}

/// Which of the three emphasised states object `i` is in: hovered,
/// selected, halo.
#[must_use]
pub fn emphasis_of(i: usize, emphasis: (Option<usize>, Option<usize>, Option<usize>)) -> (bool, bool, bool) {
    (emphasis.0 == Some(i), emphasis.1 == Some(i), emphasis.2 == Some(i))
}

/// Object `i`'s flags in the main draw: visible unless masked off or
/// emphasised (an emphasised object is drawn once, by its emphasis copy).
#[must_use]
pub fn main_flags(style: &AltitudeStyle, visible: &[bool], emphasis: (Option<usize>, Option<usize>, Option<usize>), i: usize) -> u32 {
    let (h, s, g) = emphasis_of(i, emphasis);
    pack_flags(style.paint_of(i), visible.get(i).copied().unwrap_or(true) && !(h || s || g), false, false, false)
}

/// Static objects at sphere points `sphere` as the main draw's instances
/// and the emphasis draw's (at most three, one per distinct emphasised
/// index, in hovered, selected, halo order). `visible` masks objects off
/// (`[]` or a short mask: the rest are visible); an emphasis index past the
/// end is ignored.
#[must_use]
pub fn instances_for(
    sphere: &[DVec3],
    style: &AltitudeStyle,
    visible: &[bool],
    emphasis: (Option<usize>, Option<usize>, Option<usize>),
) -> (Vec<PointInstance>, Vec<PointInstance>) {
    let main = sphere.iter().enumerate().map(|(i, &p)| static_instance(p, main_flags(style, visible, emphasis, i))).collect();
    (main, emphasis_instances(sphere, style, visible, emphasis))
}

/// The emphasis draw's instances alone (see [`instances_for`]).
#[must_use]
pub fn emphasis_instances(
    sphere: &[DVec3],
    style: &AltitudeStyle,
    visible: &[bool],
    emphasis: (Option<usize>, Option<usize>, Option<usize>),
) -> Vec<PointInstance> {
    let mut out: Vec<PointInstance> = Vec::with_capacity(3);
    let mut seen: [Option<usize>; 3] = [None; 3];
    for (k, i) in [emphasis.0, emphasis.1, emphasis.2].into_iter().enumerate() {
        let Some(i) = i.filter(|&i| i < sphere.len()) else { continue };
        if seen.contains(&Some(i)) {
            continue;
        }
        seen[k] = Some(i);
        let (h, s, g) = emphasis_of(i, emphasis);
        let shown = visible.get(i).copied().unwrap_or(true);
        out.push(static_instance(sphere[i], pack_flags(style.paint_of(i), shown, h, s, g)));
    }
    out
}

/// How an altitude layer's points (and a line layer's lines) are drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AltitudeLook {
    /// Solid dots with a 1 px dark outline, blended "over" (the original
    /// look; what every layer draws unless it asks for [`Self::Glow`]).
    #[default]
    Flat,
    /// A hot near-white core easing out to the paint colour, in a soft
    /// halo ([`POINT_GLOW`]) blended by SCREEN -- an add that saturates
    /// softly, so a dense shell blooms where halos overlap without going
    /// solid white. Lines get a bright core in the same falloff
    /// ([`LINE_GLOW`]). Hovered, selected and halo objects blend "over",
    /// so the halo object keeps its white and dark rings. Made for a dark
    /// ground: a screen blend cannot show over white.
    Glow,
}

/// The Glow look's points, `U::glow`: core whiteness at the centre (0..1),
/// halo reach as a multiple of the dot's radius, plus this many logical px,
/// and the halo's gain at the rim.
pub const POINT_GLOW: [f32; 4] = [0.6, 3.0, 5.0, 0.55];
/// The planet's radius on screen, logical px, at and above which a
/// low-gain (dense) paint's halo is drawn at its full
/// [`AltitudeStyle::glow_gain`]; a smaller globe crowds the same objects
/// into fewer pixels, so their halos are dimmed toward
/// `gain x (gain + (1 - gain) x crowd)`, `crowd = r_px / this`, floor 0.2
/// ([`glow_crowd`]). A paint at gain 1 is never dimmed.
pub const GLOW_FULL_PX_PER_RADIUS: f64 = 300.0;

/// How uncrowded the frame is for [`GLOW_FULL_PX_PER_RADIUS`]: the
/// planet's screen radius over it, clamped to `0.2..=1`.
#[must_use]
pub fn glow_crowd(px_per_radius: f64) -> f32 {
    let c = px_per_radius / GLOW_FULL_PX_PER_RADIUS;
    if c.is_finite() { c.clamp(0.2, 1.0) as f32 } else { 1.0 }
}

/// A paint's halo gain this frame: `gain x (gain + (1 - gain) x crowd)`.
#[must_use]
pub fn crowded_gain(gain: f32, crowd: f32) -> f32 {
    let g = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
    let k = g.min(1.0);
    g * (k + (1.0 - k) * crowd)
}

/// The Glow look's lines, `U::glow`: core whiteness, (unused), the halo's
/// reach either side of the core, logical px, and its gain.
pub const LINE_GLOW: [f32; 4] = [0.55, 0.0, 5.0, 0.5];

/// Motion trails behind a keyframed layer's points: each object draws a
/// short streak back along its own path -- the same Hermite the dot is
/// placed by, sampled at `segments` + 1 instants from the scene time `t`
/// back to `t - Δ` (so a curved LEO arc stays curved) -- fading from the
/// dot's colour at the head to nothing at the tail, screen-blended like
/// [`AltitudeLook::Glow`], and hidden by the planet per fragment.
///
/// `Δ = wall_s x the clock rate` scene seconds: a trail shows how far the
/// object moves in `wall_s` of WALL time, so it is long at 1000x, short at
/// 60x and vanishes at 1x or paused (no trail draw at all then). `Δ` is
/// held to half the keyframe interval (the Hermite is extrapolated at most
/// that far behind its first keyframe), and each trail to `max_px` logical
/// px on screen. A static layer (no clock) never draws one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrailStyle {
    /// The trail's length in wall seconds of motion.
    pub wall_s: f32,
    /// The longest a trail is drawn, logical px.
    pub max_px: f32,
    /// Quads per trail (1..=16).
    pub segments: u32,
    /// The soft glow either side of the trail's core, logical px.
    pub glow_px: f32,
}

impl Default for TrailStyle {
    fn default() -> Self {
        Self { wall_s: 0.12, max_px: 36.0, segments: 4, glow_px: 3.0 }
    }
}

impl TrailStyle {
    /// Quads per trail, clamped to `1..=16`.
    #[must_use]
    pub fn segment_count(&self) -> u32 {
        self.segments.clamp(1, 16)
    }
}

/// A trail's length as a fraction of the keyframe interval, signed (a clock
/// running backwards trails ahead in scene time): `wall_s x rate / delta_s`,
/// held to `±0.5`. 0 when there is no trail to draw.
#[must_use]
pub fn trail_fraction(trail: Option<&TrailStyle>, rate: f32, delta_s: f32) -> f32 {
    let Some(t) = trail else { return 0.0 };
    let f = t.wall_s * rate / delta_s;
    if f.is_finite() { f.clamp(-0.5, 0.5) } else { 0.0 }
}

/// Where a trail's sample `k` of `n` puts `inst`, sphere frame: the Hermite
/// at `s - frac x k / n` (outside a partial existence window, clamped into
/// it), then `M`. The CPU mirror of `vs_trail`'s sampling before its
/// on-screen length cap (which only ever shortens `frac`).
#[must_use]
pub fn trail_sample(frame: &AltitudeFrame, inst: &PointInstance, frac: f32, k: u32, n: u32) -> Option<DVec3> {
    if frame.s < inst.hidden_s[0] || frame.s > inst.hidden_s[1] {
        return None;
    }
    let (lo, hi) = trail_window(inst.hidden_s);
    let s = (frame.s - frac * k as f32 / n.max(1) as f32).clamp(lo, hi);
    AltitudeFrame { s, ..*frame }.placed(&PointInstance { hidden_s: [lo, hi], ..*inst })
}

/// The `s` range a trail may sample: an object that exists over the whole
/// interval (`[0, 1]`) may be extrapolated either side; one that launches
/// or decays inside it stops there.
#[must_use]
pub fn trail_window(hidden_s: [f32; 2]) -> (f32, f32) {
    (if hidden_s[0] > 0.0 { hidden_s[0] } else { -1.0 }, if hidden_s[1] < 1.0 { hidden_s[1] } else { 2.0 })
}

/// Whether a layer in `look` blends its trails by screen (Glow) rather
/// than premultiplied "over" (Flat, which may be drawn on light ground).
#[must_use]
pub fn trails_screen_blended(look: AltitudeLook) -> bool {
    look == AltitudeLook::Glow
}

/// `altitude_shader.wgsl`'s `U`: 656 B.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct AltitudeUniform {
    pub view_proj: [[f32; 4]; 4],
    pub view: [[f32; 4]; 4],
    pub m: [[f32; 4]; 3],
    pub eye: [f32; 4],
    pub params: [f32; 4],
    pub viewport: [f32; 4],
    pub palette: [[f32; 4]; 16],
    pub base_px: [[f32; 4]; 4],
    pub emphasis_px: [f32; 4],
    /// The Glow look's parameters ([`POINT_GLOW`] or [`LINE_GLOW`]), px
    /// already in device pixels.
    pub glow: [f32; 4],
    /// Trails: the length as a fraction of the interval ([`trail_fraction`]),
    /// the on-screen cap and the glow either side (device px), segments.
    pub trail: [f32; 4],
    /// [`AltitudeStyle::glow_gain`], four paints per vector.
    pub glow_gain: [[f32; 4]; 4],
}

const _: () = assert!(std::mem::size_of::<AltitudeUniform>() == 656);

/// How a layer's objects are painted.
#[derive(Clone, Debug, PartialEq)]
pub struct AltitudeStyle {
    /// Paint index to colour; slot [`OUTLINE_SLOT`] is overwritten with
    /// [`Self::outline`].
    pub palette: [Rgba; 16],
    /// One paint index per object (`0..=14`; higher is clamped to 14). An
    /// object past the end of this list is painted 0.
    pub paint: Vec<u8>,
    /// Diameter per paint index, logical px, before the R20 depth cue.
    pub base_px: [f32; 16],
    /// The 1 px ring around every dot.
    pub outline: Rgba,
    /// Flat dots or glowing ones.
    pub look: AltitudeLook,
    /// Motion trails behind a keyframed layer's points; `None` (the
    /// default): none.
    pub trail: Option<TrailStyle>,
    /// Per paint index, the Glow look's halo gain (x [`POINT_GLOW`]'s):
    /// below 1 for a dense group, whose overlapping halos would otherwise
    /// saturate. 1 by default.
    pub glow_gain: [f32; 16],
}

impl Default for AltitudeStyle {
    /// Grey dots of 2 px, outlined at 60% black (spec §1.2, §2.5).
    fn default() -> Self {
        Self {
            palette: [Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 }; 16],
            paint: Vec::new(),
            base_px: [2.0; 16],
            outline: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.6 },
            look: AltitudeLook::Flat,
            trail: None,
            glow_gain: [1.0; 16],
        }
    }
}

impl AltitudeStyle {
    /// Object `i`'s paint index, clamped off the outline slot.
    #[must_use]
    pub fn paint_of(&self, i: usize) -> u8 {
        self.paint.get(i).copied().unwrap_or(0).min(OUTLINE_SLOT as u8 - 1)
    }
}

/// The soft occlusion edge's half-width, sphere units, for eye `e`.
///
/// Spec §2.4 says one pixel in sphere units, `1 / px_per_radius`. That holds
/// with the eye far away, but near the planet the edge smears: a line of
/// sight at angle `θ` off the centre passes the centre at `m = |E| sin θ`, so
/// at the limb `dm/dθ = sqrt(|E|² - 1)`, and a ramp `2 ε` wide in `m` is
/// `2 |E| / sqrt(|E|² - 1)` px wide on screen -- 8 px at `|E| = 1.03`.
/// Dividing that factor back out, `ε = sqrt(|E|² - 1) / focal`, holds the
/// ramp at 2 px at every distance (and tends to `1 / px_per_radius` far
/// out). Never zero, since `smoothstep` with equal edges is undefined.
#[must_use]
pub fn occlusion_eps(camera: &Camera, e: DVec3) -> f64 {
    let focal = px_per_radius(camera, e) * e.length();
    let e2 = e.length_squared();
    let eps = (e2 - 1.0).max(0.0).sqrt() / focal;
    if eps.is_finite() { eps.max(1e-7) } else { 1e-7 }
}

/// Everything a frame of the altitude layer needs from the camera, in f64
/// until it is written into the uniform.
#[derive(Clone, Copy, Debug)]
pub struct AltitudeFrame {
    pub view_proj: DMat4,
    pub view: DMat4,
    /// Keyframe frame to sphere frame (`teme_to_sphere` for inertial
    /// sources; identity for Earth-fixed).
    pub m: DMat3,
    pub eye: DVec3,
    /// [`occlusion_eps`].
    pub eps: f64,
    /// The fraction of the keyframe interval, `0..=1`.
    pub s: f32,
    /// The keyframe interval, seconds.
    pub delta_s: f32,
    /// The layer's opacity: `transition x dim`.
    pub alpha: f32,
    /// The globe projection's near plane, view units: the cull.
    pub near: f64,
    /// The target's size in DEVICE pixels.
    pub viewport: (f32, f32),
    /// Device pixels per logical pixel: sizes in [`AltitudeStyle`] and
    /// [`EMPHASIS_PX`] are logical and multiplied by this. 1 by default;
    /// `MapView::paint` sets it and `viewport` to the physical target.
    pub px_scale: f32,
    /// The scene clock's rate, scene seconds per wall second: sizes a
    /// [`TrailStyle`]. 0 (no trails) unless a layer's motion gives one.
    pub rate: f32,
    /// [`glow_crowd`] of this frame's camera.
    pub crowd: f32,
    /// `tan(fov_y / 2)` of this frame's camera (`Camera::tan_half_fov`).
    pub tan_half_fov: f64,
}

impl AltitudeFrame {
    /// `None` when the frame is flat (`frame.transition == 0`): the layer
    /// is a globe feature and is skipped outright there, so a flat frame
    /// is bit-identical with and without it (R3).
    #[must_use]
    pub fn new(camera: &Camera, frame: &GlobeFrame, m: DMat3, s: f32, delta_s: f32, dim: f32) -> Option<Self> {
        if frame.transition <= 0.0 || frame.transition.is_nan() {
            return None;
        }
        let eye = eye_sphere(frame);
        Some(Self {
            view_proj: frame.globe_view_proj,
            view: frame.globe_view,
            m,
            eye,
            eps: occlusion_eps(camera, eye),
            s,
            delta_s,
            alpha: frame.transition * dim,
            near: super::globe::globe_near(camera),
            viewport: (camera.viewport_width_px, camera.viewport_height_px),
            px_scale: 1.0,
            rate: 0.0,
            crowd: glow_crowd(px_per_radius(camera, eye)),
            tan_half_fov: camera.tan_half_fov(),
        })
    }

    /// This frame with a keyframed layer's own `M`, `s` and interval, as
    /// `prepare_layers` gives that layer's uniform (the hold `dim` goes into
    /// `alpha`).
    #[must_use]
    pub fn with_motion(&self, motion: Option<LayerMotion>) -> Self {
        match motion {
            Some(mo) => Self { m: mo.m, s: mo.s, delta_s: mo.delta_s, alpha: self.alpha * mo.dim, rate: mo.rate, ..*self },
            None => *self,
        }
    }

    /// Where `vs_point` puts `inst` this frame, before any cull but the
    /// flag and the `hidden_s` window: the Hermite at `(s, delta_s)`,
    /// evaluated in f64 from the f32 instance fields, then `M`. `None` when
    /// the object is not drawn by flag or does not exist at `s`.
    #[must_use]
    pub fn placed(&self, inst: &PointInstance) -> Option<DVec3> {
        // The shader's comparisons, in the shader's precision.
        if inst.flags & FLAG_VISIBLE == 0 || self.s < inst.hidden_s[0] || self.s > inst.hidden_s[1] {
            return None;
        }
        let v = |a: [f32; 3]| glam::Vec3::from(a).as_dvec3();
        let local = super::altitude::hermite(v(inst.p0), v(inst.v0), v(inst.p1), v(inst.v1), f64::from(self.delta_s), f64::from(self.s));
        let p = self.m * local;
        p.is_finite().then_some(p)
    }

    /// Whether sphere point `p` is at or behind the near plane: the
    /// shader's `view.z > -near` cull.
    #[allow(clippy::neg_cmp_op_on_partial_ord, reason = "NaN-safe on purpose: a NaN depth counts as behind the eye, as the shader's cull does")]
    fn behind_near(&self, p: DVec3) -> bool {
        !(self.view.transform_point3(p).z <= -self.near)
    }

    /// Whether the planet hides `p`: the shader's soft edge, taken as
    /// hidden at half opacity or less (a dot fading out over the limb stops
    /// being hittable at its midpoint).
    #[allow(clippy::neg_cmp_op_on_partial_ord, reason = "NaN-safe on purpose: a NaN visibility counts as occluded")]
    fn occluded(&self, p: DVec3) -> bool {
        !(super::altitude::visibility(self.eye, p, self.eps) > 0.5)
    }

    /// The CPU mirror of `vs_point`'s position: [`Self::placed`], then
    /// `None` behind the near plane or where the planet hides it
    /// (`visibility <= 0.5`).
    #[must_use]
    pub fn position(&self, inst: &PointInstance) -> Option<DVec3> {
        self.placed(inst).filter(|&p| !self.behind_near(p) && !self.occluded(p))
    }

    /// Sphere point `p` to LOGICAL screen pixels, `None` behind the near
    /// plane: `render::altitude::project_altitude` for this frame's camera.
    /// `viewport` is in device pixels, so it is divided by `px_scale` back
    /// to the logical target the pointer lives in.
    #[must_use]
    pub fn project(&self, p: DVec3) -> Option<(f32, f32)> {
        if self.behind_near(p) {
            return None;
        }
        let mut clip = self.view_proj * p.extend(1.0);
        clip.x = -clip.x;
        let scale = if self.px_scale.is_finite() && self.px_scale > 0.0 { self.px_scale } else { 1.0 };
        let camera = Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 0.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: self.viewport.0 / scale,
            viewport_height_px: self.viewport.1 / scale,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        super::project_clip_to_screen(&camera, clip.as_vec4())
    }

    /// How the shown objects of `instances` move at this frame's `s`
    /// ([`DotMotion`]): the fastest on-screen dot, the soonest an
    /// off-screen one could come into view, and the soonest one appears
    /// or vanishes. `rate` is the clock's (only its sign matters here: it
    /// says which `hidden_s` edges lie ahead).
    ///
    /// Speed, per object: its Hermite velocity at `s`
    /// ([`super::altitude::hermite_velocity`]; `M` is a rotation, so the
    /// length carries over), plus, for an `inertial` layer, the Earth's
    /// turn under it (`ω` times its distance from the polar sphere-`y`
    /// axis), mapped to the screen by the same bound `MapView`'s inertial
    /// rings use: a view point `(X, Y, Z)` at depth `d = -Z` moving `|v|`
    /// moves at most `f |v| / d · (1 + |(X, Y)| / d)` DEVICE px. Occlusion
    /// is ignored (a dot about to clear the limb counts).
    ///
    /// An object off screen (outside [`DOT_REDRAW_NDC_MARGIN`]) moves
    /// nothing visible, but at that bound it cannot reach the viewport
    /// before `distance / speed`: that is its time to enter. One nearer
    /// the eye than [`DOT_ENTER_MIN_DEPTH_NEARS`] near planes is left out
    /// -- an object skimming the camera moves 10^4-10^5 px/s off screen,
    /// and would ask for every frame -- as is one behind the near plane
    /// (culled); either is drawn within [`DOT_REDRAW_MAX_WAIT_S`].
    /// O(objects): about 10k Hermites a paint.
    #[must_use]
    #[allow(clippy::neg_cmp_op_on_partial_ord, reason = "NaN-safe on purpose: a NaN depth is skipped, as the shader culls it")]
    pub fn dot_motion(&self, instances: &[PointInstance], inertial: bool, rate: f64) -> DotMotion {
        let focal = f64::from(self.viewport.1) / 2.0 / self.tan_half_fov;
        let scale = self.view.transform_vector3(DVec3::X).length();
        let half = (f64::from(self.viewport.0) / 2.0, f64::from(self.viewport.1) / 2.0);
        let (s, delta) = (f64::from(self.s), f64::from(self.delta_s));
        let turn = if inertial { super::altitude::EARTH_ROTATION_RAD_S } else { 0.0 };
        let v = |a: [f32; 3]| glam::Vec3::from(a).as_dvec3();
        let mut out = DotMotion::STILL;
        for inst in instances {
            if inst.flags & FLAG_VISIBLE == 0 {
                continue;
            }
            // The next edge of its existence window in the clock's
            // direction: it appears or vanishes there.
            for edge in inst.hidden_s {
                let ahead = if rate > 0.0 { edge > self.s } else { rate < 0.0 && edge < self.s };
                if ahead {
                    out.edge_scene_s = out.edge_scene_s.min((f64::from(edge) - s).abs() * delta);
                }
            }
            if self.s < inst.hidden_s[0] || self.s > inst.hidden_s[1] {
                continue;
            }
            let (p0, v0, p1, v1) = (v(inst.p0), v(inst.v0), v(inst.p1), v(inst.v1));
            let p = self.m * super::altitude::hermite(p0, v0, p1, v1, delta, s);
            let speed = super::altitude::hermite_velocity(p0, v0, p1, v1, delta, s).length() + turn * p.x.hypot(p.z);
            let q = self.view.transform_point3(p);
            if !(q.z <= -self.near) {
                continue;
            }
            let depth = -q.z;
            let px = speed * scale * focal / depth * (1.0 + q.x.hypot(q.y) / depth);
            if !px.is_finite() {
                continue;
            }
            let clip = self.view_proj * p.extend(1.0);
            let (nx, ny) = (clip.x.abs() / clip.w, clip.y.abs() / clip.w);
            if nx <= DOT_REDRAW_NDC_MARGIN && ny <= DOT_REDRAW_NDC_MARGIN {
                out.px_per_scene_s = out.px_per_scene_s.max(px);
            } else if depth >= DOT_ENTER_MIN_DEPTH_NEARS * self.near && px > 0.0 {
                let away = ((nx - DOT_REDRAW_NDC_MARGIN).max(0.0) * half.0).hypot((ny - DOT_REDRAW_NDC_MARGIN).max(0.0) * half.1);
                if away.is_finite() {
                    out.enter_scene_s = out.enter_scene_s.min(away / px);
                }
            }
        }
        out
    }

    /// The uniform a line layer draws with: this frame (its `M` included)
    /// with `viewport.z` the line width in device pixels.
    fn line_uniform(&self, width_px: f32) -> AltitudeUniform {
        let mut u = self.uniform(&AltitudeStyle::default());
        let w = if width_px.is_finite() { width_px.max(0.0) } else { 0.0 };
        u.viewport[2] = w * self.px_scale;
        u.glow = [LINE_GLOW[0], LINE_GLOW[1], LINE_GLOW[2] * self.px_scale, LINE_GLOW[3]];
        u
    }

    /// This frame's trail length for `style` ([`trail_fraction`]).
    #[must_use]
    pub fn trail_fraction(&self, style: &AltitudeStyle) -> f32 {
        trail_fraction(style.trail.as_ref(), self.rate, self.delta_s)
    }

    fn uniform(&self, style: &AltitudeStyle) -> AltitudeUniform {
        let mut palette = style.palette.map(|c| [c.r, c.g, c.b, c.a]);
        let o = style.outline;
        palette[OUTLINE_SLOT] = [o.r, o.g, o.b, o.a];
        let mut base_px = [[0.0; 4]; 4];
        for (k, px) in style.base_px.iter().enumerate() {
            base_px[k / 4][k % 4] = if px.is_finite() { px.max(0.0) } else { 0.0 };
        }
        let mut glow_gain = [[0.0; 4]; 4];
        for (k, g) in style.glow_gain.iter().enumerate() {
            glow_gain[k / 4][k % 4] = crowded_gain(*g, self.crowd);
        }
        let col = |v: DVec3| [v.x as f32, v.y as f32, v.z as f32, 0.0];
        AltitudeUniform {
            view_proj: self.view_proj.as_mat4().to_cols_array_2d(),
            view: self.view.as_mat4().to_cols_array_2d(),
            m: [col(self.m.x_axis), col(self.m.y_axis), col(self.m.z_axis)],
            eye: [self.eye.x as f32, self.eye.y as f32, self.eye.z as f32, self.eps as f32],
            params: [self.s, self.delta_s, self.alpha, self.near as f32],
            viewport: [self.viewport.0, self.viewport.1, OUTLINE_PX * self.px_scale, HALO_PX * self.px_scale],
            palette,
            base_px,
            emphasis_px: [EMPHASIS_PX[0], EMPHASIS_PX[1], EMPHASIS_PX[2], self.px_scale],
            glow: [POINT_GLOW[0], POINT_GLOW[1], POINT_GLOW[2] * self.px_scale, POINT_GLOW[3]],
            trail: style.trail.map_or([0.0; 4], |t| {
                let cap = if t.max_px.is_finite() { t.max_px.max(0.0) } else { 0.0 };
                let glow = if t.glow_px.is_finite() { t.glow_px.max(0.0) } else { 0.0 };
                [self.trail_fraction(style), cap * self.px_scale, glow * self.px_scale, t.segment_count() as f32]
            }),
            glow_gain,
        }
    }
}

/// One segment of an altitude line on the GPU: 28 B, read per instance.
/// Ends in Earth radii in the line layer's keyframe frame (the shader
/// applies `M`), colour as `Unorm8x4`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LineInstance {
    pub a: [f32; 3],
    pub b: [f32; 3],
    pub color: [u8; 4],
}

const _: () = assert!(std::mem::size_of::<LineInstance>() == 28);

/// The line instance layout: `a`, `b`, `color`.
pub const LINE_ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
    0 => Float32x3, // a
    1 => Float32x3, // b
    2 => Unorm8x4,  // color
];

/// One instance per segment of every line, `(points[k], points[k + 1])`,
/// plus `(last, first)` when the line is closed; positions divided by
/// [`GLOBE_RADIUS_M`] into Earth radii. A segment with a non-finite end is
/// dropped (the line breaks there rather than streaking to the origin).
#[must_use]
pub fn line_instances(lines: &[super::altitude::AltitudeLine]) -> Vec<LineInstance> {
    let mut out = Vec::with_capacity(lines.iter().map(|l| l.points.len()).sum());
    for line in lines {
        let c = line.color;
        let u8c = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let color = [u8c(c.r), u8c(c.g), u8c(c.b), u8c(c.a)];
        let n = line.points.len();
        if n < 2 {
            continue;
        }
        let closing = if line.closed && n > 2 { n } else { n - 1 };
        for k in 0..closing {
            let (a, b) = (line.points[k], line.points[(k + 1) % n]);
            if !(a.is_finite() && b.is_finite()) {
                continue;
            }
            let q = |v: DVec3| (v / GLOBE_RADIUS_M).as_vec3().to_array();
            out.push(LineInstance { a: q(a), b: q(b), color });
        }
    }
    out
}

/// One line layer's draw: its own uniform (the frame with this layer's `M`,
/// and its width in `viewport.z`) and its own instance buffer. Built by
/// [`AltitudePipeline::prepare_lines`]; a frame that only moves the camera
/// rewrites the uniform alone with [`AltitudePipeline::refresh_lines`].
pub struct LineBatch {
    look: AltitudeLook,
    uniform: TrackedBuffer,
    bind_group: wgpu::BindGroup,
    instances: Option<TrackedBuffer>,
    count: u32,
}

impl LineBatch {
    /// Segments this batch draws.
    #[must_use]
    pub fn segments(&self) -> usize {
        self.count as usize
    }
}

/// How close to a dot's centre the pointer must be to hit it, logical px.
pub const HIT_RADIUS_PX: f32 = 8.0;

/// On-screen motion, device px, the fastest shown keyframed dot may make
/// since the last paint before the dots ask for a frame of their own. A
/// quarter pixel of an anti-aliased dot several pixels wide is not a
/// visible step.
pub const DOT_REDRAW_PX: f64 = 0.25;

/// How far outside the viewport, in NDC (1 is the edge), a dot still counts
/// as on screen in [`AltitudeFrame::dot_motion`]: a few percent, so a dot
/// straddling the edge counts.
pub const DOT_REDRAW_NDC_MARGIN: f64 = 1.05;

/// How near the eye, in near planes, an off-screen object may be and still
/// be timed to enter the view ([`AltitudeFrame::dot_motion`]). The satellites
/// at z3 have objects skimming the camera at ~9 near planes moving
/// 10^4-10^5 px/s off screen; timing them would ask for every frame.
pub const DOT_ENTER_MIN_DEPTH_NEARS: f64 = 16.0;

/// How keyframed dots move on one frame ([`AltitudeFrame::dot_motion`]).
/// Speeds in device px per SCENE second, times in SCENE seconds; a wall
/// wait divides the times by the clock's rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DotMotion {
    /// The fastest on-screen dot; 0 when none is.
    pub px_per_scene_s: f64,
    /// The soonest an off-screen dot could reach the viewport; infinite
    /// when none could.
    pub enter_scene_s: f64,
    /// The soonest a dot appears or vanishes (its `hidden_s` edge ahead
    /// of `s` in the clock's direction), or, once merged with the layer's
    /// keyframe events ([`DotMotion::with_event`]), its prefetch trigger or
    /// pair end; infinite when none is ahead.
    pub edge_scene_s: f64,
}

impl Default for DotMotion {
    fn default() -> Self {
        Self::STILL
    }
}

impl DotMotion {
    /// Nothing moves, nothing is due.
    pub const STILL: Self = Self { px_per_scene_s: 0.0, enter_scene_s: f64::INFINITY, edge_scene_s: f64::INFINITY };

    /// The faster, the sooner, of both.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            px_per_scene_s: self.px_per_scene_s.max(other.px_per_scene_s),
            enter_scene_s: self.enter_scene_s.min(other.enter_scene_s),
            edge_scene_s: self.edge_scene_s.min(other.edge_scene_s),
        }
    }

    /// With one more event `scene_s` scene seconds ahead (ignored unless
    /// positive and finite).
    #[must_use]
    pub fn with_event(self, scene_s: f64) -> Self {
        if scene_s > 0.0 && scene_s.is_finite() {
            Self { edge_scene_s: self.edge_scene_s.min(scene_s), ..self }
        } else {
            self
        }
    }
}

/// Wall seconds after an event a wake aims for, so its paint lands past
/// the event (the prefetch trigger, the pair end, an appearance), not a
/// hair before it.
pub const DOT_EVENT_SLACK_S: f64 = 0.002;

/// The longest moving dots wait for a frame, wall seconds, however slowly
/// they move: the one bound on what [`AltitudeFrame::dot_motion`] leaves
/// out -- an object coming round from behind the near plane, or skimming
/// past the eye into view.
pub const DOT_REDRAW_MAX_WAIT_S: f64 = 0.5;

/// Wall seconds until the dots need their next frame under clock `rate`:
/// the soonest of the fastest on-screen dot having moved [`DOT_REDRAW_PX`],
/// an off-screen one possibly entering the view, and the next event (plus
/// [`DOT_EVENT_SLACK_S`]); at most [`DOT_REDRAW_MAX_WAIT_S`]. `None` for a
/// paused or NaN clock (nothing moves). At rate 1 on the whole globe the
/// fastest LEO dot moves a few px a second, so the dots ask for a handful
/// of frames a second rather than every display refresh; a fast clock or a
/// close camera is every frame again.
#[must_use]
pub fn dot_redraw_after_s(rate: f64, motion: DotMotion) -> Option<f64> {
    if rate == 0.0 || rate.is_nan() {
        return None;
    }
    let r = rate.abs();
    let px_per_s = motion.px_per_scene_s * r;
    let moved = if px_per_s > 0.0 { DOT_REDRAW_PX / px_per_s } else { f64::INFINITY };
    let enter = motion.enter_scene_s / r;
    let event = motion.edge_scene_s / r + DOT_EVENT_SLACK_S;
    let wait = moved.min(enter).min(event).min(DOT_REDRAW_MAX_WAIT_S);
    Some(if wait.is_nan() { 0.0 } else { wait.max(0.0) })
}

/// The object under `(x, y)` (logical px): the index into `objects` of the
/// one whose drawn centre is nearest, within `radius_px`. Uses exactly the
/// draw's placement and culls ([`AltitudeFrame::position`]), so a toggled
/// off, not-yet-launched, decayed or planet-hidden object is never hit.
/// One linear pass, no index (spec §2.8); the occlusion test runs only for
/// objects already within the radius. Equal distances go to the higher
/// index, the one drawn on top.
#[must_use]
pub fn hit_test(frame: &AltitudeFrame, objects: &[PointInstance], x: f32, y: f32, radius_px: f32) -> Option<usize> {
    let r2 = radius_px * radius_px;
    let mut best: Option<(f32, usize)> = None;
    for (i, inst) in objects.iter().enumerate() {
        let Some(p) = frame.placed(inst) else { continue };
        let Some((sx, sy)) = frame.project(p) else { continue };
        let d2 = (sx - x) * (sx - x) + (sy - y) * (sy - y);
        // `<=`: on an exact tie the later instance wins -- it is drawn on
        // top (no depth, clip z constant), so it is the dot the user sees.
        if d2 <= r2 && best.is_none_or(|(b, _)| d2 <= b) && !frame.occluded(p) {
            best = Some((d2, i));
        }
    }
    best.map(|(_, i)| i)
}

/// One layer's draw: its style (palette and sizes), its instances, and the
/// emphasised copies drawn last. `generation`, when given, lets the
/// pipeline skip re-uploading instances it already holds for that
/// generation; `None` always uploads.
pub struct AltitudeBatch<'a> {
    pub style: &'a AltitudeStyle,
    pub main: &'a [PointInstance],
    pub emphasis: &'a [PointInstance],
    pub generation: Option<u64>,
    /// This layer's own `M`, `s`, interval and hold dim, overriding the
    /// frame's (a keyframed layer next to a static one). `None`: the
    /// frame's.
    pub motion: Option<LayerMotion>,
}

/// A keyframed layer's per-frame motion: [`AltitudeFrame::m`],
/// [`AltitudeFrame::s`] and [`AltitudeFrame::delta_s`] for this layer
/// alone, and `dim`, multiplied into the frame's alpha (0.5 while the layer
/// holds its last positions waiting for a keyframe, spec §3.2).
#[derive(Clone, Copy, Debug)]
pub struct LayerMotion {
    pub m: DMat3,
    pub s: f32,
    pub delta_s: f32,
    pub dim: f32,
    /// The scene clock's rate (sizes the layer's trails).
    pub rate: f32,
}

/// A batch's GPU side: its uniform and bind group, its two instance buffers
/// (grown by doubling, never shrunk) and what they hold this frame.
struct Slot {
    uniform: TrackedBuffer,
    bind_group: wgpu::BindGroup,
    main: Option<TrackedBuffer>,
    emphasis: Option<TrackedBuffer>,
    main_count: u32,
    emphasis_count: u32,
    visible: usize,
    generation: Option<u64>,
    look: AltitudeLook,
    /// Quads per trail this frame; 0: no trail draw.
    trail_segments: u32,
}

/// The point pass. Built once (lazily, by `MapView`, the first frame a
/// layer exists); buffers are kept.
pub struct AltitudePipeline {
    pipeline: wgpu::RenderPipeline,
    /// `vs_line` / `fs_line`: the same shader module, layout and blend.
    line_pipeline: wgpu::RenderPipeline,
    /// [`AltitudeLook::Glow`]: `vs_point_glow` / `fs_point_glow`, screen
    /// blended (the main draw).
    glow_pipeline: wgpu::RenderPipeline,
    /// The same, blended "over" (the emphasis draw: its rings must darken).
    glow_over_pipeline: wgpu::RenderPipeline,
    /// `vs_line_glow` / `fs_line_glow`, screen blended.
    glow_line_pipeline: wgpu::RenderPipeline,
    /// `vs_trail` / `fs_trail`, screen blended: [`TrailStyle`] on a
    /// [`AltitudeLook::Glow`] layer.
    trail_pipeline: wgpu::RenderPipeline,
    /// The same, blended "over": trails on a [`AltitudeLook::Flat`] layer,
    /// which may sit on light ground a screen blend cannot show on.
    trail_over_pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    slots: Vec<Slot>,
    /// Batches prepared this frame (a prefix of `slots`).
    live: usize,
}

impl AltitudePipeline {
    /// `depth_format`: the pass's depth attachment, if it has one. The
    /// layer never tests or writes depth (the planet is hidden analytically,
    /// R1); the format is only there so the pipeline matches the pass.
    #[must_use]
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat, depth_format: Option<wgpu::TextureFormat>) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps altitude shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("altitude_shader.wgsl").into()),
        });
        let bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("altitude-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("altitude-pipeline-layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        // Screen: colour = src + dst (1 - src), a soft-saturating add;
        // alpha as premultiplied "over".
        let screen = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrc,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::OVER,
        };
        let build = |label: &str, vs: &str, fs: &str, stride: usize, attributes: &[wgpu::VertexAttribute], blend: wgpu::BlendState| {
            ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: stride as wgpu::BufferAddress,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes,
                    })],
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
                depth_stencil: depth_format.map(|format| wgpu::DepthStencilState {
                    format,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: color_format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let over = wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING;
        let (point, line) = (std::mem::size_of::<PointInstance>(), std::mem::size_of::<LineInstance>());
        let pipeline = build("altitude-point-pipeline", "vs_point", "fs_point", point, &POINT_ATTRS, over);
        let line_pipeline = build("altitude-line-pipeline", "vs_line", "fs_line", line, &LINE_ATTRS, over);
        let glow_pipeline = build("altitude-glow-pipeline", "vs_point_glow", "fs_point_glow", point, &POINT_ATTRS, screen);
        let glow_over_pipeline = build("altitude-glow-over-pipeline", "vs_point_glow", "fs_point_glow", point, &POINT_ATTRS, over);
        let glow_line_pipeline = build("altitude-glow-line-pipeline", "vs_line_glow", "fs_line_glow", line, &LINE_ATTRS, screen);
        let trail_pipeline = build("altitude-trail-pipeline", "vs_trail", "fs_trail", point, &POINT_ATTRS, screen);
        let trail_over_pipeline = build("altitude-trail-over-pipeline", "vs_trail", "fs_trail", point, &POINT_ATTRS, over);
        Self {
            pipeline,
            line_pipeline,
            glow_pipeline,
            glow_over_pipeline,
            glow_line_pipeline,
            trail_pipeline,
            trail_over_pipeline,
            bgl,
            slots: Vec::new(),
            live: 0,
        }
    }

    /// One layer's frame: [`Self::prepare_layers`] with a single batch.
    pub fn prepare_points(
        &mut self,
        ctx: &GpuContext,
        frame: &AltitudeFrame,
        style: &AltitudeStyle,
        main: &[PointInstance],
        emphasis: &[PointInstance],
    ) {
        self.prepare_layers(ctx, frame, &[AltitudeBatch { style, main, emphasis, generation: None, motion: None }]);
    }

    /// Writes this frame's uniforms and (when changed) instances, one slot
    /// per batch, so every layer keeps its own palette. ONE call per frame:
    /// `queue.write_buffer` lands before any draw in the submission, so a
    /// second call would overwrite the first's slots.
    pub fn prepare_layers(&mut self, ctx: &GpuContext, frame: &AltitudeFrame, batches: &[AltitudeBatch<'_>]) {
        while self.slots.len() < batches.len() {
            let uniform = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                label: Some("altitude-uniform"),
                size: std::mem::size_of::<AltitudeUniform>() as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("altitude-bind-group"),
                layout: &self.bgl,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
            });
            self.slots.push(Slot {
                uniform,
                bind_group,
                main: None,
                emphasis: None,
                main_count: 0,
                emphasis_count: 0,
                visible: 0,
                generation: None,
                look: AltitudeLook::Flat,
                trail_segments: 0,
            });
        }
        for (slot, batch) in self.slots.iter_mut().zip(batches) {
            let own = frame.with_motion(batch.motion);
            ctx.queue.write_buffer(&slot.uniform, 0, bytemuck::bytes_of(&own.uniform(batch.style)));
            slot.look = batch.style.look;
            slot.trail_segments = match batch.style.trail {
                Some(t) if own.trail_fraction(batch.style) != 0.0 => t.segment_count(),
                _ => 0,
            };
            if batch.generation.is_some() && batch.generation == slot.generation {
                continue;
            }
            upload(ctx, &mut slot.main, batch.main, "altitude-instances");
            upload(ctx, &mut slot.emphasis, batch.emphasis, "altitude-emphasis-instances");
            slot.main_count = batch.main.len() as u32;
            slot.emphasis_count = batch.emphasis.len() as u32;
            slot.visible = batch.main.iter().chain(batch.emphasis).filter(|i| i.flags & FLAG_VISIBLE != 0).count();
            slot.generation = batch.generation;
        }
        self.live = batches.len();
    }

    /// Instances with [`FLAG_VISIBLE`] submitted by the last prepare: what
    /// the vertex stage was asked to place (the planet and the near plane
    /// then cull some of them on the GPU).
    #[must_use]
    pub fn points_drawn(&self) -> usize {
        self.slots[..self.live].iter().map(|s| s.visible).sum()
    }

    /// Draws every prepared batch's `main`, then every batch's `emphasis`,
    /// so hovered, selected and halo objects land on top (R4).
    pub fn draw_points(&self, pass: &mut wgpu::RenderPass<'_>) {
        let live = &self.slots[..self.live];
        if live.iter().all(|s| s.main_count == 0 && s.emphasis_count == 0) {
            return;
        }
        // Trails first, under every dot: 6 vertices per quad, `segments`
        // quads per object, main and emphasised alike.
        for slot in live.iter().filter(|s| s.trail_segments > 0) {
            pass.set_pipeline(if trails_screen_blended(slot.look) { &self.trail_pipeline } else { &self.trail_over_pipeline });
            pass.set_bind_group(0, &slot.bind_group, &[]);
            for (buffer, count) in [(&slot.main, slot.main_count), (&slot.emphasis, slot.emphasis_count)] {
                let Some(buffer) = buffer.as_ref().filter(|_| count > 0) else { continue };
                pass.set_vertex_buffer(0, buffer.slice(..u64::from(count) * std::mem::size_of::<PointInstance>() as u64));
                pass.draw(0..6 * slot.trail_segments, 0..count);
                super::alloc::note_draw_calls(1);
            }
        }
        for emphasis in [false, true] {
            for slot in live {
                let (buffer, count) =
                    if emphasis { (&slot.emphasis, slot.emphasis_count) } else { (&slot.main, slot.main_count) };
                let Some(buffer) = buffer.as_ref().filter(|_| count > 0) else { continue };
                pass.set_pipeline(match (slot.look, emphasis) {
                    (AltitudeLook::Flat, _) => &self.pipeline,
                    (AltitudeLook::Glow, false) => &self.glow_pipeline,
                    (AltitudeLook::Glow, true) => &self.glow_over_pipeline,
                });
                pass.set_bind_group(0, &slot.bind_group, &[]);
                pass.set_vertex_buffer(0, buffer.slice(..u64::from(count) * std::mem::size_of::<PointInstance>() as u64));
                pass.draw(0..6, 0..count);
                super::alloc::note_draw_calls(1);
            }
        }
    }
}

impl AltitudePipeline {
    /// One line layer's frame: a new [`LineBatch`] holding `lines` and a
    /// uniform of `frame` (with the layer's `M` already in `frame.m`) at
    /// `width_px` logical pixels, drawn in `look`. Its own buffers, so any
    /// number of batches can be prepared in one frame.
    pub fn prepare_lines(
        &mut self,
        ctx: &GpuContext,
        frame: &AltitudeFrame,
        width_px: f32,
        look: AltitudeLook,
        lines: &[LineInstance],
    ) -> LineBatch {
        let uniform = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("altitude-line-uniform"),
            size: std::mem::size_of::<AltitudeUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("altitude-line-bind-group"),
            layout: &self.bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
        });
        let mut batch = LineBatch { look, uniform, bind_group, instances: None, count: 0 };
        upload_bytes(ctx, &mut batch.instances, bytemuck::cast_slice(lines), "altitude-line-instances");
        batch.count = lines.len() as u32;
        self.refresh_lines(ctx, &mut batch, frame, width_px);
        batch
    }

    /// Rewrites `batch`'s uniform for a new frame (camera, `M`, alpha,
    /// width); its segments are kept.
    #[allow(clippy::unused_self)]
    pub fn refresh_lines(&self, ctx: &GpuContext, batch: &mut LineBatch, frame: &AltitudeFrame, width_px: f32) {
        ctx.queue.write_buffer(&batch.uniform, 0, bytemuck::bytes_of(&frame.line_uniform(width_px)));
    }

    /// Draws `batch`'s segments: 6 vertices per segment instance. Called
    /// before [`Self::draw_points`] in the same pass, so dots sit on their
    /// rings.
    pub fn draw_lines(&self, pass: &mut wgpu::RenderPass<'_>, batch: &LineBatch) {
        let Some(buffer) = batch.instances.as_ref().filter(|_| batch.count > 0) else { return };
        pass.set_pipeline(match batch.look {
            AltitudeLook::Flat => &self.line_pipeline,
            AltitudeLook::Glow => &self.glow_line_pipeline,
        });
        pass.set_bind_group(0, &batch.bind_group, &[]);
        pass.set_vertex_buffer(0, buffer.slice(..u64::from(batch.count) * std::mem::size_of::<LineInstance>() as u64));
        pass.draw(0..6, 0..batch.count);
        super::alloc::note_draw_calls(1);
    }
}

/// Writes `instances` into `buffer`, growing it by doubling when too small.
fn upload(ctx: &GpuContext, buffer: &mut Option<TrackedBuffer>, instances: &[PointInstance], label: &'static str) {
    upload_bytes(ctx, buffer, bytemuck::cast_slice(instances), label);
}

/// [`upload`] for any instance bytes.
fn upload_bytes(ctx: &GpuContext, buffer: &mut Option<TrackedBuffer>, bytes: &[u8], label: &'static str) {
    if bytes.is_empty() {
        return;
    }
    let need = bytes.len() as u64;
    if buffer.as_ref().is_none_or(|b| b.tracked_size() < need) {
        let have = buffer.as_ref().map_or(0, TrackedBuffer::tracked_size);
        let size = need.max(have.saturating_mul(2)).max(60 * 64);
        *buffer = Some(TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
    }
    ctx.queue.write_buffer(buffer.as_ref().expect("just ensured"), 0, bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::altitude::{geo_to_sphere, visibility, GeoPoint};
    use glam::DVec2;

    #[test]
    fn the_instance_is_sixty_bytes_and_the_uniform_fits() {
        assert_eq!(std::mem::size_of::<PointInstance>(), 60);
        assert_eq!(std::mem::size_of::<AltitudeUniform>(), 656);
        assert_eq!(POINT_ATTRS.len(), 6);
        assert_eq!(std::mem::size_of::<LineInstance>(), 28);
        assert_eq!(LINE_ATTRS.len(), 3);
    }

    #[test]
    fn line_instances_are_one_per_segment_plus_the_closing_one() {
        use crate::render::altitude::AltitudeLine;
        let r = GLOBE_RADIUS_M;
        let pts = vec![DVec3::new(r, 0.0, 0.0), DVec3::new(0.0, r, 0.0), DVec3::new(0.0, 0.0, 2.0 * r)];
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 0.5 };
        let open = line_instances(&[AltitudeLine { points: pts.clone(), color: red, closed: false }]);
        assert_eq!(open.len(), 2);
        assert_eq!(open[0], LineInstance { a: [1.0, 0.0, 0.0], b: [0.0, 1.0, 0.0], color: [255, 0, 0, 128] });
        let closed = line_instances(&[AltitudeLine { points: pts.clone(), color: red, closed: true }]);
        assert_eq!(closed.len(), 3);
        assert_eq!((closed[2].a, closed[2].b), ([0.0, 0.0, 2.0], [1.0, 0.0, 0.0]), "last -> first");
        let mut broken = pts;
        broken[1].x = f64::NAN;
        assert_eq!(line_instances(&[AltitudeLine { points: broken, color: red, closed: false }]).len(), 0);
    }

    /// Two dots at identical positions: the one drawn on top (the later
    /// instance) is the one hit.
    #[test]
    fn an_exact_tie_goes_to_the_dot_drawn_on_top() {
        let (_, f) = frame_at(0.0, 0.0);
        let objs = vec![inst(10.0, 5.0, 400.0, true), inst(10.0, 5.0, 400.0, true)];
        let (x, y) = f.project(f.position(&objs[0]).unwrap()).unwrap();
        assert_eq!(hit_test(&f, &objs, x + 2.0, y, HIT_RADIUS_PX), Some(1));
        let three = vec![objs[0], inst(80.0, 0.0, 400.0, true), objs[1]];
        assert_eq!(hit_test(&f, &three, x, y, HIT_RADIUS_PX), Some(2));
    }

    #[test]
    fn flags_pack_into_scalar_bits() {
        let f = pack_flags(13, true, false, true, true);
        assert_eq!(f & 15, 13);
        assert_eq!(f & FLAG_VISIBLE, FLAG_VISIBLE);
        assert_eq!(f & FLAG_HOVERED, 0);
        assert_eq!(f & (FLAG_SELECTED | FLAG_HALO), FLAG_SELECTED | FLAG_HALO);
        assert_eq!(pack_flags(15, false, false, false, false) & 15, 14, "15 is the outline's slot");
        assert_eq!(pack_flags(200, false, false, false, false) & 15, 14);
    }

    #[test]
    fn a_flat_frame_builds_no_altitude_frame() {
        let c = crate::render::Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 14.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 512.0,
            viewport_height_px: 512.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let f = crate::render::globe::globe_frame(&c);
        assert_eq!(f.transition, 0.0);
        assert!(AltitudeFrame::new(&c, &f, glam::DMat3::IDENTITY, 0.0, 60.0, 1.0).is_none(), "R3: skipped at zero");
    }

    #[test]
    fn a_non_finite_point_is_emitted_invisible() {
        let i = static_instance(DVec3::new(f64::NAN, 0.0, 2.0), pack_flags(1, true, false, false, false));
        assert_eq!(i.flags & FLAG_VISIBLE, 0);
        let j = static_instance(DVec3::new(0.0, 0.0, 2.0), pack_flags(1, true, false, false, false));
        assert_eq!((j.p0, j.p1, j.v0, j.hidden_s), ([0.0, 0.0, 2.0], [0.0, 0.0, 2.0], [0.0; 3], [0.0, 1.0]));
    }

    /// The occlusion ramp's width on screen, measured by sweeping a line of
    /// sight across the limb: `focal x` the angle over which `visibility`
    /// goes from 0.001 to 0.999, for a point well past the planet.
    fn ramp_px(camera: &Camera, e_len: f64, eps: f64) -> f64 {
        let e = DVec3::new(0.0, 0.0, e_len);
        let focal = px_per_radius(camera, e) * e_len;
        let limb = (1.0 / e_len).asin();
        let (mut lo, mut hi) = (f64::NAN, f64::NAN);
        let steps = 200_000;
        let span = 40.0 / focal; // +-40 px of angle around the limb
        for k in 0..=steps {
            let theta = limb - span + 2.0 * span * f64::from(k) / f64::from(steps);
            let dir = DVec3::new(theta.sin(), 0.0, -theta.cos());
            let p = e + dir * (2.0 * e_len + 4.0);
            let v = visibility(e, p, eps);
            if v > 0.001 && lo.is_nan() {
                lo = theta;
            }
            if v < 0.999 {
                hi = theta;
            }
        }
        (hi - lo) * focal
    }

    /// Carry from Task 2's review: the raw `1 / px_per_radius` edge is
    /// `2|E|/sqrt(|E|² - 1)` px wide, 8 px at `|E| = 1.03`. `occlusion_eps`
    /// holds it near 2 px at every distance; the raw epsilon is measured
    /// too, so the test shows it would have caught the smear.
    #[test]
    fn the_soft_occlusion_edge_stays_about_two_pixels_wide_near_the_planet() {
        let camera = Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 0.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        for e_len in [1.01, 1.03, 1.1, 1.5, 3.0, 9.5, 40.0] {
            let e = DVec3::new(0.0, 0.0, e_len);
            let w = ramp_px(&camera, e_len, occlusion_eps(&camera, e));
            // A smoothstep from 0.001 to 0.999 spans ~97% of its 2 eps.
            assert!((1.7..=2.3).contains(&w), "|E| = {e_len}: the edge is {w:.2} px wide");
        }
        let e = DVec3::new(0.0, 0.0, 1.03);
        let raw = ramp_px(&camera, 1.03, 1.0 / px_per_radius(&camera, e));
        assert!(raw > 6.0, "the unclamped edge at |E| = 1.03 measures {raw:.2} px");
    }

    #[test]
    fn the_altitude_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("altitude_shader.wgsl"),
            &[
                ("vs_point", naga::ShaderStage::Vertex),
                ("fs_point", naga::ShaderStage::Fragment),
                ("vs_line", naga::ShaderStage::Vertex),
                ("fs_line", naga::ShaderStage::Fragment),
                ("vs_point_glow", naga::ShaderStage::Vertex),
                ("fs_point_glow", naga::ShaderStage::Fragment),
                ("vs_line_glow", naga::ShaderStage::Vertex),
                ("fs_line_glow", naga::ShaderStage::Fragment),
                ("vs_trail", naga::ShaderStage::Vertex),
                ("fs_trail", naga::ShaderStage::Fragment),
            ],
        );
    }

    /// The pipeline builds, and a frame prepares and draws, on a device
    /// created with WebGL2's downlevel limits: six instance attributes, two
    /// inter-stage vectors, a 560 B uniform, no storage buffer -- the point
    /// stage and the line stage (three attributes, two inter-stage vectors).
    #[test]
    fn the_point_pipeline_builds_and_draws_under_downlevel_webgl2_limits() {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::LowPower, ..Default::default() })
                .await
                .expect("an adapter");
            let limits = wgpu::Limits::downlevel_webgl2_defaults();
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor { required_limits: limits, ..Default::default() })
                .await
                .expect("a device at WebGL2's downlevel limits");
            let max_buffer_size = device.limits().max_buffer_size;
            let ctx = GpuContext { device, queue, max_buffer_size };
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let mut pipeline = AltitudePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(wgpu::TextureFormat::Depth24Plus));
            let c = camera(0.0, 0.0, 0.0, 256.0);
            let gf = crate::render::globe::globe_frame(&c);
            let frame = AltitudeFrame::new(&c, &gf, DMat3::IDENTITY, 0.0, 60.0, 1.0).expect("a globe frame");
            let style = AltitudeStyle::default();
            let (main, emph) = instances_for(&[DVec3::new(0.0, 0.0, 1.1), DVec3::new(0.3, 0.0, 1.05)], &style, &[], (Some(1), None, None));
            pipeline.prepare_points(&ctx, &frame, &style, &main, &emph);
            let ring: Vec<DVec3> = (0..64)
                .map(|k| {
                    let a = std::f64::consts::TAU * f64::from(k) / 64.0;
                    DVec3::new(a.cos(), 0.3 * a.sin(), a.sin()) * 1.5 * GLOBE_RADIUS_M
                })
                .collect();
            let lines = line_instances(&[crate::render::altitude::AltitudeLine { points: ring, color: Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 }, closed: true }]);
            let line_batch = pipeline.prepare_lines(&ctx, &frame, 2.0, AltitudeLook::Flat, &lines);
            assert_eq!(line_batch.segments(), 64);
            let make = |format, label| {
                ctx.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width: 256, height: 256, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
            };
            let color = make(wgpu::TextureFormat::Rgba8Unorm, "webgl2-altitude-color");
            let depth = make(wgpu::TextureFormat::Depth24Plus, "webgl2-altitude-depth");
            let (cv, dv) = (color.create_view(&Default::default()), depth.create_view(&Default::default()));
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("webgl2-altitude-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &cv,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &dv,
                        depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pipeline.draw_lines(&mut pass, &line_batch);
                pipeline.draw_points(&mut pass);
            }
            ctx.queue.submit(Some(encoder.finish()));
            ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let error = scope.pop().await;
            assert!(error.is_none(), "validation error under WebGL2 limits: {error:?}");
            assert_eq!(pipeline.points_drawn(), 2);
        });
    }

    fn frame_at(zoom: f64, lat: f64) -> (crate::render::Camera, AltitudeFrame) {
        let c = crate::render::Camera {
            center_lon: 0.0,
            center_lat: lat,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let g = crate::render::globe::globe_frame(&c);
        let f = AltitudeFrame::new(&c, &g, glam::DMat3::IDENTITY, 0.0, 60.0, 1.0).unwrap();
        (c, f)
    }

    /// A circular LEO orbit (550 km, 95.6 min), metres and m/s at `t`.
    fn circular(t: f64) -> KeyState {
        let r = GLOBE_RADIUS_M + 550e3;
        let w = std::f64::consts::TAU / (95.6 * 60.0);
        let (sn, cs) = (w * t).sin_cos();
        // Inclined 53 degrees, as a Starlink shell.
        let tilt = DMat3::from_rotation_x(53f64.to_radians());
        KeyState { position_m: tilt * DVec3::new(r * cs, r * sn, 0.0), velocity_m_s: tilt * DVec3::new(-r * w * sn, r * w * cs, 0.0), valid: true }
    }

    #[test]
    fn trails_are_off_by_default_and_follow_the_clock() {
        let style = AltitudeStyle::default();
        assert_eq!(style.trail, None, "the engine draws no trails unless asked");
        assert_eq!(style.look, AltitudeLook::Flat, "nor glows");
        let (_, f) = frame_at(1.0, 0.0);
        let f = AltitudeFrame { rate: 1000.0, delta_s: 240.0, ..f };
        assert_eq!(f.trail_fraction(&style), 0.0);
        assert_eq!(f.uniform(&style).trail, [0.0; 4]);
        let trail = TrailStyle::default();
        let on = AltitudeStyle { trail: Some(trail), ..AltitudeStyle::default() };
        // 0.12 s at 1000x = 120 scene s = 0.5 of a 240 s interval (the cap).
        assert!((f.trail_fraction(&on) - 0.5).abs() < 1e-6);
        let slow = AltitudeFrame { rate: 60.0, delta_s: 60.0, ..f };
        assert!((slow.trail_fraction(&on) - 0.12).abs() < 1e-6, "{}", slow.trail_fraction(&on));
        assert_eq!(AltitudeFrame { rate: 0.0, ..f }.trail_fraction(&on), 0.0, "paused: no trail");
        assert!(AltitudeFrame { rate: 1.0, delta_s: 60.0, ..f }.trail_fraction(&on) < 0.003, "1x: next to nothing");
        assert!(AltitudeFrame { rate: -1000.0, ..f }.trail_fraction(&on) < 0.0, "backwards: the trail is ahead in time");
        let u = f.uniform(&on);
        assert_eq!((u.trail[1], u.trail[3]), (trail.max_px, trail.segments as f32));
    }

    /// A trail's samples lie on the orbit: sample k of n is where the
    /// object was at `t - k Δ / n`, across a keyframe boundary too (the
    /// Hermite extrapolated behind `k0`).
    #[test]
    fn a_trail_follows_the_orbit_it_was_sampled_from() {
        let (_, f) = frame_at(1.0, 0.0);
        let delta = 240.0;
        let (k0, k1) = (circular(0.0), circular(delta));
        let inst = keyed_instance(&k0, &k1, delta, [0.0, 1.0], pack_flags(0, true, false, false, false));
        let style = AltitudeStyle { trail: Some(TrailStyle { wall_s: 0.12, ..TrailStyle::default() }), ..AltitudeStyle::default() };
        // s = 0: the whole trail extrapolated behind k0; s = 1 with the
        // clock backwards: the whole trail ahead of k1 (the -0.5 cap).
        for (s, rate) in [(0.0_f32, 1000.0_f32), (0.1, 1000.0), (0.5, 1000.0), (0.9, 1000.0), (1.0, -1000.0)] {
            let f = AltitudeFrame { s, delta_s: delta as f32, rate, ..f };
            let frac = f.trail_fraction(&style);
            let dt = f64::from(frac) * delta;
            assert!((dt.abs() - 120.0).abs() < 1e-3 && dt.signum() == f64::from(rate).signum(), "s {s}: {dt}");
            let n = 4;
            let mut worst: f64 = 0.0;
            for k in 0..=n {
                let t = f64::from(s) * delta - dt * f64::from(k) / f64::from(n);
                let got = trail_sample(&f, &inst, frac, k, n).expect("on the trail");
                let want = circular(t).position_m / GLOBE_RADIUS_M;
                worst = worst.max((got - want).length() * GLOBE_RADIUS_M / 1e3);
            }
            // f32 positions (~0.5 m per radius unit) and the Hermite's own
            // error, extrapolated at most half an interval: well under 2 km.
            assert!(worst < 2.0, "s {s}: {worst} km off the orbit");
        }
        // An object that launches mid-interval stops its trail there.
        let born = keyed_instance(&k0, &k1, delta, [0.4, 1.0], pack_flags(0, true, false, false, false));
        let f = AltitudeFrame { s: 0.5, delta_s: delta as f32, rate: 1000.0, ..f };
        let tail = trail_sample(&f, &born, 0.5, 4, 4).unwrap();
        let at_birth = AltitudeFrame { s: 0.4, ..f }.placed(&born).unwrap();
        assert!((tail - at_birth).length() < 1e-9);
    }

    #[test]
    fn a_dense_paints_glow_dims_on_a_small_globe_and_a_full_one_never() {
        assert_eq!(crowded_gain(1.0, 0.2), 1.0);
        assert!((crowded_gain(0.3, 1.0) - 0.3).abs() < 1e-6);
        assert!(crowded_gain(0.3, 0.2) < 0.15);
        assert_eq!(glow_crowd(f64::NAN), 1.0);
        assert_eq!(glow_crowd(1e9), 1.0);
        assert_eq!(glow_crowd(1.0), 0.2);
    }

    fn inst(lon: f64, lat: f64, alt_km: f64, visible: bool) -> PointInstance {
        let p = geo_to_sphere(GeoPoint { lon, lat, alt_m: alt_km * 1e3 });
        static_instance(p, pack_flags(0, visible, false, false, false))
    }

    #[test]
    fn the_cpu_projection_is_project_altitude() {
        let (c, f) = frame_at(-1.0, 20.0);
        let g = crate::render::globe::globe_frame(&c);
        for (lon, lat, alt) in [(0.0, 0.0, 400.0), (40.0, 30.0, 20_200.0), (-70.0, -10.0, 35_786.0)] {
            let p = geo_to_sphere(GeoPoint { lon, lat, alt_m: alt * 1e3 });
            assert_eq!(f.project(p), crate::render::altitude::project_altitude(&c, &g, p));
        }
    }

    /// `MapView::paint` hands the frame a device-pixel viewport and its
    /// scale; `project` still answers in logical px, where the pointer is.
    #[test]
    fn the_cpu_projection_answers_in_logical_pixels_on_a_retina_target() {
        let (c, f) = frame_at(-1.0, 20.0);
        let g = crate::render::globe::globe_frame(&c);
        let retina = AltitudeFrame { viewport: (2048.0, 2048.0), px_scale: 2.0, ..f };
        let p = geo_to_sphere(GeoPoint { lon: 40.0, lat: 30.0, alt_m: 20_200e3 });
        assert_eq!(retina.project(p), crate::render::altitude::project_altitude(&c, &g, p));
    }

    #[test]
    fn the_nearest_visible_object_within_eight_px_wins() {
        let (_, f) = frame_at(0.0, 0.0);
        let objs = vec![inst(0.0, 0.0, 400.0, true), inst(0.5, 0.0, 400.0, true), inst(0.0, 0.0, 800.0, true)];
        let p1 = f.project(f.position(&objs[1]).unwrap()).unwrap();
        assert_eq!(hit_test(&f, &objs, p1.0 + 1.0, p1.1, HIT_RADIUS_PX), Some(1));
        assert_eq!(hit_test(&f, &objs, p1.0 + 40.0, p1.1 + 40.0, HIT_RADIUS_PX), None);
    }

    #[test]
    fn a_toggled_off_group_is_not_hit() {
        let (_, f) = frame_at(0.0, 0.0);
        let objs = vec![inst(0.0, 0.0, 400.0, false)];
        let p = geo_to_sphere(GeoPoint { lon: 0.0, lat: 0.0, alt_m: 4e5 });
        let (x, y) = f.project(p).unwrap();
        assert_eq!(hit_test(&f, &objs, x, y, HIT_RADIUS_PX), None);
    }

    #[test]
    fn a_hidden_state_is_never_hit() {
        let (_, mut f) = frame_at(0.0, 0.0);
        let mut o = inst(0.0, 0.0, 400.0, true);
        o.hidden_s = [0.0, 0.5]; // decayed at s = 0.5
        f.s = 0.75;
        assert!(f.position(&o).is_none());
        let far_side = inst(180.0, 0.0, 400.0, true);
        assert!(f.position(&far_side).is_none(), "behind the planet");
    }

    /// Satellites behind the Earth are never hit, even with the pointer
    /// exactly on where they project (inside the disc), and the hit test
    /// agrees with `position` object by object across the limb.
    #[test]
    fn an_occluded_object_is_never_hit_even_under_the_pointer() {
        let (_, f) = frame_at(0.0, 0.0);
        let far = inst(170.0, 5.0, 20_200.0, true);
        let p = f.placed(&far).unwrap();
        assert!(f.position(&far).is_none(), "hidden by the planet");
        let (x, y) = f.project(p).expect("in front of the eye, so it projects");
        assert_eq!(hit_test(&f, &[far], x, y, HIT_RADIUS_PX), None);
        // A sweep of GEO objects round the equator: hittable exactly where
        // `position` (the draw's cull, `visibility > 0.5`) says drawn.
        for k in 0..360 {
            let o = inst(f64::from(k), 0.0, 35_786.0, true);
            let at = f.placed(&o).and_then(|p| f.project(p)).unwrap();
            let hit = hit_test(&f, &[o], at.0, at.1, HIT_RADIUS_PX).is_some();
            assert_eq!(hit, f.position(&o).is_some(), "lon {k}");
        }
    }

    /// The hit test places a moving object at the frame's `s` through the
    /// same Hermite and `M` the shader uses, not at either keyframe.
    #[test]
    fn a_moving_object_is_hit_where_the_hermite_puts_it_this_frame() {
        let (_, mut f) = frame_at(0.0, 0.0);
        let a = geo_to_sphere(GeoPoint { lon: -10.0, lat: 0.0, alt_m: 550e3 });
        let b = geo_to_sphere(GeoPoint { lon: 10.0, lat: 0.0, alt_m: 550e3 });
        let v = (b - a) / 60.0;
        let q = |d: DVec3| d.as_vec3().to_array();
        let o = PointInstance { p0: q(a), v0: q(v), p1: q(b), v1: q(v), flags: pack_flags(0, true, false, false, false), hidden_s: [0.0, 1.0] };
        f.delta_s = 60.0;
        f.s = 0.5;
        let mid = crate::render::altitude::hermite(a, v, b, v, 60.0, 0.5);
        let (x, y) = f.project(mid).unwrap();
        assert_eq!(hit_test(&f, &[o], x, y, 0.5), Some(0));
        let (x0, y0) = f.project(a).unwrap();
        assert_eq!(hit_test(&f, &[o], x0, y0, HIT_RADIUS_PX), None, "not at the first keyframe");
        // Rotated by M: the hit follows it.
        f.m = glam::DMat3::from_rotation_y(0.2);
        let (xm, ym) = f.project(f.m * mid).unwrap();
        assert_eq!(hit_test(&f, &[o], xm, ym, 0.5), Some(0));
    }

    #[test]
    #[ignore = "measurement: run explicitly in --release"]
    fn mg_s4_hit_test_cost() {
        // 10k objects spread like the synthetic set's shells (static), on the spec's 64x48 grid, 1440x900 at z-1.
        let c = crate::render::Camera {
            center_lon: 0.0,
            center_lat: 20.0,
            zoom: -1.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1440.0,
            viewport_height_px: 900.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let g = crate::render::globe::globe_frame(&c);
        let f = AltitudeFrame::new(&c, &g, glam::DMat3::IDENTITY, 0.0, 60.0, 1.0).unwrap();
        let mut seed = 42u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let objs: Vec<_> = (0..10_000)
            .map(|i| {
                let alt = if i < 7_000 {
                    550.0
                } else if i < 8_500 {
                    500.0 + 700.0 * rnd()
                } else if i < 9_000 {
                    35_786.0
                } else {
                    20_200.0
                };
                inst(360.0 * rnd() - 180.0, 106.0 * rnd() - 53.0, alt, true)
            })
            .collect();
        let mut runs = Vec::new();
        for _ in 0..5 {
            let t = crate::timing::Instant::now();
            let mut hits = 0;
            for gy in 0..48 {
                for gx in 0..64 {
                    let (x, y) = ((gx as f32 + 0.5) * 1440.0 / 64.0, (gy as f32 + 0.5) * 900.0 / 48.0);
                    if hit_test(&f, &objs, x, y, HIT_RADIUS_PX).is_some() {
                        hits += 1;
                    }
                }
            }
            std::hint::black_box(hits);
            runs.push(t.elapsed().as_secs_f64() * 1e6 / (64.0 * 48.0));
        }
        runs.sort_by(f64::total_cmp);
        println!("MG-S4 hit_us_mean={:.1} runs={runs:?}", runs[2]);
        assert!(runs[2] <= 500.0, "MG-S4 FAIL: {:.0} us per hit test (criterion <= 500 us)", runs[2]);
    }

    fn camera(lon: f64, lat: f64, zoom: f64, size: f32) -> Camera {
        Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: size, viewport_height_px: size, fov_y: Camera::DEFAULT_FOV_Y }
    }

    /// Renders `points` (sphere frame, paint 0 = white) into a black
    /// `size`² target under a validation error scope, and reads it back.
    fn render(camera: &Camera, points: &[DVec3], size: u32) -> (Vec<u8>, usize) {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let mut pipeline = AltitudePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
            let gf = crate::render::globe::globe_frame(camera);
            let frame = AltitudeFrame::new(camera, &gf, DMat3::IDENTITY, 0.0, 60.0, 1.0).expect("a globe frame");
            let mut style = AltitudeStyle { base_px: [6.0; 16], outline: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }, ..Default::default() };
            style.palette[0] = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
            let main: Vec<_> = points.iter().map(|&p| static_instance(p, pack_flags(0, true, false, false, false))).collect();
            pipeline.prepare_points(&ctx, &frame, &style, &main, &[]);
            let pixels = draw_and_read(&ctx, size, |pass| pipeline.draw_points(pass));
            let error = scope.pop().await;
            assert!(error.is_none(), "wgpu validation error: {error:?}");
            (pixels, pipeline.points_drawn())
        })
    }

    /// Renders one Earth-fixed line layer (`lines`, `width_px` wide) into a
    /// black `size`² target under a validation error scope, and reads it
    /// back.
    fn render_lines(camera: &Camera, lines: &[LineInstance], width_px: f32, size: u32) -> Vec<u8> {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let mut pipeline = AltitudePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
            let gf = crate::render::globe::globe_frame(camera);
            let frame = AltitudeFrame::new(camera, &gf, DMat3::IDENTITY, 0.0, 60.0, 1.0).expect("a globe frame");
            let batch = pipeline.prepare_lines(&ctx, &frame, width_px, AltitudeLook::Flat, lines);
            let pixels = draw_and_read(&ctx, size, |pass| pipeline.draw_lines(pass, &batch));
            let error = scope.pop().await;
            assert!(error.is_none(), "wgpu validation error: {error:?}");
            pixels
        })
    }

    /// Renders `main` in `style` with `frame` into a black `size`² target
    /// under a validation error scope, `reps` times; the last pixels and
    /// the median wall time of one prepare + draw + readback, ms.
    fn render_styled(frame: &AltitudeFrame, style: &AltitudeStyle, main: &[PointInstance], size: u32, reps: usize) -> (Vec<u8>, f64) {
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let mut pipeline = AltitudePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, None);
            let mut times = Vec::with_capacity(reps);
            let mut pixels = Vec::new();
            for _ in 0..reps.max(1) {
                let t = crate::timing::Instant::now();
                pipeline.prepare_points(&ctx, frame, style, main, &[]);
                pixels = draw_and_read(&ctx, size, |pass| pipeline.draw_points(pass));
                times.push(t.elapsed().as_secs_f64() * 1e3);
            }
            let error = scope.pop().await;
            assert!(error.is_none(), "wgpu validation error: {error:?}");
            times.sort_by(f64::total_cmp);
            (pixels, times[times.len() / 2])
        })
    }

    /// One object on the circular orbit, keyed over 240 s, drawn at `s`
    /// with the camera over its position at `s`.
    fn moving_one(s: f32, rate: f32) -> (AltitudeFrame, PointInstance) {
        let delta = 240.0;
        let inst = keyed_instance(&circular(0.0), &circular(delta), delta, [0.0, 1.0], pack_flags(0, true, false, false, false));
        let here = circular(f64::from(s) * delta).position_m;
        let (lon, lat) = (here.x.atan2(here.z).to_degrees(), (here.y / here.length()).asin().to_degrees());
        let c = camera(lon, lat, 2.0, 256.0);
        let gf = crate::render::globe::globe_frame(&c);
        let f = AltitudeFrame::new(&c, &gf, DMat3::IDENTITY, s, delta as f32, 1.0).expect("a globe frame");
        (AltitudeFrame { rate, ..f }, inst)
    }

    /// On the GPU: a glowing dot lights a wider disc than a flat one, and
    /// with trails at 1000x the lit pixels reach back along the orbit
    /// (toward where the object was) and not forward.
    #[test]
    fn a_trail_streaks_behind_the_dot_on_the_gpu() {
        let (f, inst) = moving_one(0.5, 1000.0);
        let mut style = AltitudeStyle { base_px: [4.0; 16], outline: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }, ..Default::default() };
        style.palette[0] = Rgba { r: 0.2, g: 0.8, b: 1.0, a: 1.0 };
        let lit = |px: &[u8]| px.chunks(4).filter(|p| p[1] > 8).count();
        let (flat, _) = render_styled(&f, &style, &[inst], 256, 1);
        let glow_style = AltitudeStyle { look: AltitudeLook::Glow, ..style.clone() };
        let (glow, _) = render_styled(&f, &glow_style, &[inst], 256, 1);
        assert!(lit(&glow) > 2 * lit(&flat), "glow {} flat {}", lit(&glow), lit(&flat));
        let trail_style = AltitudeStyle { trail: Some(TrailStyle { wall_s: 0.12, max_px: 40.0, ..TrailStyle::default() }), ..glow_style };
        let (trail, _) = render_styled(&f, &trail_style, &[inst], 256, 1);
        assert!(lit(&trail) > lit(&glow) + 20, "trail {} glow {}", lit(&trail), lit(&glow));
        // Where the object was 60 s ago, and where it will be 60 s on.
        let (x0, y0) = f.project(f.placed(&inst).unwrap()).unwrap();
        let at = |ds: f32| f.project(AltitudeFrame { s: f.s + ds, ..f }.placed(&inst).unwrap()).unwrap();
        let (bx, by) = at(-0.25);
        let (ax, ay) = at(0.25);
        let lum = |px: &[u8], x: f32, y: f32| {
            let (x, y) = ((x0 + 0.6 * (x - x0)).round() as usize, (y0 + 0.6 * (y - y0)).round() as usize);
            px[(y * 256 + x) * 4 + 1]
        };
        assert!(lum(&trail, bx, by) > 20, "behind the dot: {}", lum(&trail, bx, by));
        assert!(lum(&trail, ax, ay) < 5, "ahead of the dot: {}", lum(&trail, ax, ay));
        // A flat layer's trails blend "over": still drawn, never screen.
        assert!(trails_screen_blended(AltitudeLook::Glow) && !trails_screen_blended(AltitudeLook::Flat));
        let flat_trails = AltitudeStyle { look: AltitudeLook::Flat, ..trail_style.clone() };
        let (flat_trail, _) = render_styled(&f, &flat_trails, &[inst], 256, 1);
        assert!(lit(&flat_trail) > lit(&flat) + 20, "flat trail {} flat {}", lit(&flat_trail), lit(&flat));
        // Paused: the same style draws no trail.
        let (paused, _) = render_styled(&AltitudeFrame { rate: 0.0, ..f }, &trail_style, &[inst], 256, 1);
        assert_eq!(lit(&paused), lit(&glow));
    }

    /// The glow's GPU cost: 16.5k objects on LEO/MEO/GEO shells at z -0.3
    /// on a 1440x900-sized square, flat vs glow vs glow + trails at 1000x;
    /// the median of prepare + draw + readback over 30 frames each, so the
    /// DIFFERENCES are the glow's and the trails' fill and vertex cost.
    #[test]
    #[ignore = "timing; run with --ignored --nocapture"]
    fn mg_glow_gpu_cost() {
        let (_, f) = frame_at(-0.3, 20.0);
        let delta = 240.0;
        let n = 16_500;
        let main: Vec<PointInstance> = (0..n)
            .map(|i| {
                let k = i as f64;
                let alt = if i % 11 == 0 { 35_786e3 } else if i % 7 == 0 { 20_200e3 } else { 550e3 };
                let r = GLOBE_RADIUS_M + alt;
                let w = std::f64::consts::TAU / (2.0 * std::f64::consts::PI * (r.powi(3) / 3.986e14).sqrt());
                let tilt = DMat3::from_rotation_z(k * 0.37) * DMat3::from_rotation_x((k * 7.3) % 180.0_f64.to_radians());
                let st = |t: f64| {
                    let a = w * t + k;
                    KeyState { position_m: tilt * DVec3::new(r * a.cos(), r * a.sin(), 0.0), velocity_m_s: tilt * DVec3::new(-r * w * a.sin(), r * w * a.cos(), 0.0), valid: true }
                };
                keyed_instance(&st(0.0), &st(delta), delta, [0.0, 1.0], pack_flags((i % 5) as u8, true, false, false, false))
            })
            .collect();
        let f = AltitudeFrame { s: 0.5, delta_s: delta as f32, rate: 1000.0, ..f };
        let mut style = AltitudeStyle { base_px: [1.6, 1.6, 3.0, 2.5, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0], ..Default::default() };
        style.glow_gain[1] = 0.3;
        let glow = AltitudeStyle { look: AltitudeLook::Glow, ..style.clone() };
        let trails = AltitudeStyle { trail: Some(TrailStyle { wall_s: 0.1, max_px: 28.0, segments: 4, glow_px: 3.0 }), ..glow.clone() };
        for (name, st) in [("flat", &style), ("glow", &glow), ("glow+trails", &trails), ("flat", &style)] {
            let (_, ms) = render_styled(&f, st, &main, 1024, 30);
            println!("mg_glow_gpu_cost: {name} {ms:.3} ms per prepare+draw+readback (1024x1024, {n} objects)");
        }
    }

    /// One pass into a black `size`² RGBA8 target, then its pixels.
    fn draw_and_read(ctx: &GpuContext, size: u32, draw: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> Vec<u8> {
        let target = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("altitude-test-color"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("altitude-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            draw(&mut pass);
        }
        let row = (size * 4).next_multiple_of(256);
        let out = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("altitude-readback"),
            size: u64::from(row * size),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &out,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(size) },
            },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        let slice = out.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();
        let data = slice.get_mapped_range().expect("mapped");
        let mut pixels = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            let start = (y * row) as usize;
            pixels.extend_from_slice(&data[start..start + (size * 4) as usize]);
        }
        pixels
    }

    /// Task 7 review Minor 2: `vs_line`'s near clip, on the GPU. A pitched
    /// camera close to the ground, under a ring just below the eye's
    /// altitude in the view's own vertical plane: the ring runs from behind
    /// the eye, through the near plane, up the screen to the horizon. The
    /// straddling segments must be cut at the near plane -- no validation
    /// error, every lit pixel on the CPU's near-clipped segments (a vertex
    /// behind the eye projected unclipped flips through w = 0 and streaks
    /// across the screen), and the front part drawn.
    #[test]
    fn a_ring_through_the_near_plane_is_clipped_on_the_gpu() {
        const SIZE: u32 = 512;
        let cam = Camera { center_lon: 0.0, center_lat: 0.0, zoom: 7.0, pitch: 60f32.to_radians(), bearing: 0.0, viewport_width_px: SIZE as f32, viewport_height_px: SIZE as f32, fov_y: Camera::DEFAULT_FOV_Y };
        let gf = crate::render::globe::globe_frame(&cam);
        let frame = AltitudeFrame::new(&cam, &gf, DMat3::IDENTITY, 0.0, 60.0, 1.0).expect("a globe frame");
        let eye = frame.eye;
        // The ring: the great circle through the eye's direction and the
        // view's forward direction, at 3/4 of the eye's height.
        let fwd_world = frame.view.inverse().transform_vector3(DVec3::NEG_Z).normalize();
        let up = eye.normalize();
        let axis = up.cross(fwd_world).normalize();
        let tangent = axis.cross(up).normalize(); // forward, along the surface
        let r = 1.0 + 0.75 * (eye.length() - 1.0);
        // Coarse (15° chords, half a step out of phase with the eye), so
        // the segment that crosses the near plane runs from behind the eye
        // to a vertex ~800 km ahead that is on screen.
        let n = 24;
        let points: Vec<DVec3> = (0..n)
            .map(|k| {
                let a = std::f64::consts::TAU * (f64::from(k) + 0.5) / f64::from(n);
                (up * a.cos() + tangent * a.sin()) * r * GLOBE_RADIUS_M
            })
            .collect();
        let magenta = Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
        let lines = line_instances(&[crate::render::altitude::AltitudeLine { points: points.clone(), color: magenta, closed: true }]);
        // The CPU's clipped segments, in pixels (row 0 at the top).
        let to_px = |p: DVec3| {
            let c = frame.view_proj * p.extend(1.0);
            let s = f64::from(SIZE) / 2.0;
            DVec2::new((c.x / c.w + 1.0) * s, (1.0 - c.y / c.w) * s)
        };
        let mut segs = Vec::new();
        let mut straddling = 0;
        for l in &lines {
            let (a, b) = (glam::Vec3::from(l.a).as_dvec3(), glam::Vec3::from(l.b).as_dvec3());
            let (va, vb) = (frame.view.transform_point3(a), frame.view.transform_point3(b));
            let Some((t0, t1)) = crate::render::altitude::clip_segment_to_near(va, vb, frame.near) else { continue };
            let clipped = (t0, t1) != (0.0, 1.0);
            straddling += usize::from(clipped);
            // A hair inside the near plane, so w > 0 at the clipped end.
            let (t0, t1) = (t0 + 1e-9 * (t1 - t0), t1 - 1e-9 * (t1 - t0));
            segs.push((to_px(a.lerp(b, t0)), to_px(a.lerp(b, t1)), clipped));
        }
        assert!(straddling >= 2, "the ring must cross the near plane ({straddling} segments do)");
        let dist = |p: DVec2, a: DVec2, b: DVec2| {
            let ab = b - a;
            let t = ((p - a).dot(ab) / ab.length_squared().max(1e-12)).clamp(0.0, 1.0);
            (a + ab * t - p).length()
        };
        let px = render_lines(&cam, &lines, 2.0, SIZE);
        let (mut lit, mut stray) = (0usize, Vec::new());
        for y in 0..SIZE {
            for x in 0..SIZE {
                let i = ((y * SIZE + x) * 4) as usize;
                if px[i] > 40 && px[i + 2] > 40 {
                    lit += 1;
                    let p = DVec2::new(f64::from(x) + 0.5, f64::from(y) + 0.5);
                    if !segs.iter().any(|&(a, b, _)| dist(p, a, b) <= 3.0) {
                        stray.push((x, y));
                    }
                }
            }
        }
        assert!(stray.is_empty(), "{} lit pixels off every clipped segment (a streak), e.g. {:?}", stray.len(), &stray[..stray.len().min(5)]);
        // The front part is drawn: the ring climbs the screen from the
        // bottom edge, so it lights well over a few hundred pixels.
        assert!(lit > 300, "only {lit} ring pixels drawn");
        // And the near-clipped pieces themselves reach the screen: some lit
        // pixel lies on a straddling segment's clipped part.
        let on_clipped = (0..SIZE * SIZE).filter(|&k| {
            let i = (k * 4) as usize;
            px[i] > 40 && px[i + 2] > 40 && {
                let p = DVec2::new(f64::from(k % SIZE) + 0.5, f64::from(k / SIZE) + 0.5);
                segs.iter().any(|&(a, b, clipped)| clipped && dist(p, a, b) <= 1.5)
            }
        });
        assert!(on_clipped.count() > 20, "the clipped segments' front parts are not drawn");
    }

    /// Carry from Task 2's review: the globe projection's far plane is
    /// `d + 2R`, so a GEO object behind the planet's centre plane is past it
    /// (`clip.z > clip.w`). The shader writes its own clip z, so one that is
    /// above the limb still draws. The point is chosen far side, in sight,
    /// and past the far plane, and each of those is asserted first.
    #[test]
    fn a_geo_point_past_the_far_plane_but_above_the_limb_still_draws() {
        let c = camera(0.0, 0.0, -1.0, 512.0);
        let gf = crate::render::globe::globe_frame(&c);
        let e = eye_sphere(&gf);
        let p = geo_to_sphere(GeoPoint { lon: 150.0, lat: 0.0, alt_m: 35_786_000.0 });
        assert!(p.z < -1.0, "behind the centre plane by more than a radius: {p:?}");
        assert!(crate::render::altitude::visible_by_horizon_angle(e, p), "above the limb");
        let clip = gf.globe_view_proj * p.extend(1.0);
        assert!(clip.z > clip.w, "past the projection's far plane: z/w = {}", clip.z / clip.w);
        let (x, y) = crate::render::altitude::project_altitude(&c, &gf, p).expect("in front of the eye");
        let (pixels, drawn) = render(&c, &[p], 512);
        assert_eq!(drawn, 1);
        let at = |x: i32, y: i32| pixels[((y * 512 + x) * 4) as usize];
        let lit = (-1..=1).flat_map(|dy| (-1..=1).map(move |dx| (dx, dy))).any(|(dx, dy)| at(x as i32 + dx, y as i32 + dy) > 200);
        assert!(lit, "no dot at ({x:.1}, {y:.1})");
    }
}
