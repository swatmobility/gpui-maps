//! Origin-destination arcs: one GPU instance per arc (source and target
//! lon/lat, two colours, a width and a filter value), drawn as a triangle
//! strip the vertex shader generates -- deck.gl's ArcLayer, re-expressed in
//! `arc_shader.wgsl` (MIT; see `PROVENANCE.md`).
//!
//! - **Shape**: a half-ellipse whose peak is `height / 2` times the chord
//!   (deck's `paraboloid`; a semicircle at the default height 1). Flat, in
//!   Mercator world units; on the globe, a great circle lifted off the
//!   sphere. [`flat_point`] and [`globe_point`] are the shader's CPU mirror.
//! - **Antimeridian**: the flat arc takes the shortest way
//!   ([`shortest_target_lon`]) and each point is wrapped to the world copy
//!   nearest the camera; the globe arc is a great circle, which has no seam.
//! - **Segments by screen length**: an arc's segment count is fixed per
//!   draw, so [`ArcSet::new`] sorts the arcs into classes by chord length
//!   (powers of two of the world, [`length_class`]) and each class draws
//!   with the segments its longest arc needs at this zoom
//!   ([`segments_for`]): a 20 px commute gets 8, a 2000 px flight 50. At
//!   Kepler's UK view that is a fifth of deck.gl's fixed 50.
//! - **Filter**: `overlay::filter`'s window as a uniform; nothing is
//!   re-uploaded when it moves.
//! - **Blending**: premultiplied "over", additive, screen, or
//!   [`ArcBlend::Glow`]: summed into a float target and tone-mapped onto
//!   the map (`arc_glow.wgsl`), Kepler's additive look on an opaque target.
//!   A frame with a glow layer costs one `Rgba16Float` target the size of
//!   the view and one full-screen draw.

use std::ops::Range;

use glam::{DMat4, DVec2, DVec3};

use super::alloc::TrackedBuffer;
use super::camera::Camera;
use super::globe::GlobeFrame;
use super::gpu::GpuContext;
use crate::style::Rgba;

/// One arc on the GPU: 32 B, read per instance.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ArcInstance {
    /// lon, lat degrees.
    pub source: [f32; 2],
    pub target: [f32; 2],
    /// RGBA8, straight alpha ([`pack_color`]).
    pub source_color: u32,
    pub target_color: u32,
    /// Logical pixels.
    pub width_px: f32,
    /// The row's filter value relative to the set's base
    /// (`overlay::filter`); `NaN` for none. Read only while a window is on.
    pub filter: f32,
}

const _: () = assert!(std::mem::size_of::<ArcInstance>() == 32);

/// `vs_arc`'s instance attributes.
pub const ARC_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Unorm8x4,
    3 => Unorm8x4,
    4 => Float32,
    5 => Float32,
];

/// A colour as the instance carries it: RGBA8, `r` in the low byte.
#[must_use]
pub fn pack_color(c: Rgba) -> u32 {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    u32::from_le_bytes([b(c.r), b(c.g), b(c.b), b(c.a)])
}

/// deck.gl's default `numSegments`: the most any arc is cut into.
pub const MAX_SEGMENTS: u32 = 50;
/// The fewest: a short arc is still a curve.
pub const MIN_SEGMENTS: u32 = 4;
/// Screen pixels of curve per segment that [`segments_for`] aims for.
pub const PX_PER_SEGMENT: f64 = 10.0;
/// Length classes: chords of `2^-k` of the world, `k` in `0..CLASSES`.
pub const CLASSES: u8 = 24;

/// How arcs mix with what is under them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArcBlend {
    /// Premultiplied "over".
    #[default]
    Normal,
    /// `src * a + dst`: Kepler's `layerBlending: additive`. Dense bundles
    /// glow toward white.
    Additive,
    /// `src * a + dst * (1 - src * a)`: additive that saturates softly
    /// (the satellites' glow blend).
    Screen,
    /// Additive into a float target, then tone-mapped per channel,
    /// `1 - exp(-exposure * sum)`, over the map: brighter with density,
    /// the dense cores paler in the arcs' own hue rather than clipped to
    /// white. [`ArcStyle::exposure`].
    Glow,
}

/// A layer's look.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArcStyle {
    /// Multiplies every arc's alpha (Kepler's layer `opacity`).
    pub opacity: f32,
    /// deck.gl's `getHeight`: the peak is `height / 2` times the chord.
    pub height: f32,
    pub blend: ArcBlend,
    /// The most segments an arc is cut into (deck's `numSegments`).
    pub max_segments: u32,
    /// [`ArcBlend::Glow`]'s tone-map gain: higher saturates sooner. The
    /// frame's glow layers share the first one's.
    pub exposure: f32,
    /// [`ArcBlend::Glow`]'s desaturation share in `0..=1`: how far dense
    /// cores drift toward the per-channel curve (paler). 0 keeps the hue
    /// exactly.
    pub desaturate: f32,
}

impl Default for ArcStyle {
    fn default() -> Self {
        Self { opacity: 1.0, height: 1.0, blend: ArcBlend::Normal, max_segments: MAX_SEGMENTS, exposure: 1.0, desaturate: 0.35 }
    }
}

/// The glow target's format: summed, so float (8-bit would round a 0.2 px
/// arc's contribution away).
pub const GLOW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The tone map `arc_glow.wgsl` applies to a summed premultiplied colour:
/// the CPU mirror. Hue-preserving by the brightest channel, then `desaturate`
/// of the per-channel curve mixed in.
#[must_use]
pub fn glow_tonemap(sum: [f32; 3], exposure: f32, desaturate: f32) -> [f32; 3] {
    let t = |x: f32| 1.0 - (-exposure * x.max(0.0)).exp();
    let m = sum[0].max(sum[1]).max(sum[2]);
    let d = desaturate.clamp(0.0, 1.0);
    std::array::from_fn(|i| {
        let hue = if m > 1e-6 { sum[i].max(0.0) * t(m) / m } else { 0.0 };
        hue + (t(sum[i]) - hue) * d
    })
}

struct GlowTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
}

// ---------------------------------------------------------------------
// Geometry: the shader's CPU mirror
// ---------------------------------------------------------------------

/// deck's `getSegmentRatio`: point `i` of `n` segments, denser at the ends.
#[must_use]
pub fn segment_ratio(i: u32, n: u32) -> f64 {
    let x = (f64::from(i) / f64::from(n.max(1))).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// The lift at `r` per unit of chord: `sqrt(r (1 - r)) * height`, so the
/// peak (`r = 0.5`) is `height / 2`.
#[must_use]
pub fn lift(r: f64, height: f64) -> f64 {
    (r * (1.0 - r)).max(0.0).sqrt() * height
}

/// The target's longitude moved by whole turns onto the source's side of
/// the antimeridian: the flat arc goes the shortest way.
#[must_use]
pub fn shortest_target_lon(source_lon: f64, target_lon: f64) -> f64 {
    target_lon - 360.0 * ((target_lon - source_lon) / 360.0).round()
}

/// lon/lat to Web Mercator `[0, 1]`.
#[must_use]
pub fn mercator(lon: f64, lat: f64) -> DVec2 {
    let (x, y) = crate::geo::lon_lat_to_tile_xy(lon, lat, 0.0);
    DVec2::new(x, y)
}

/// The flat arc's point at `r`: Mercator `x, y` (`x` unwrapped, the shortest
/// way, so it may leave `[0, 1]`) and the height in WORLD units (the chord's
/// units: [`super::WORLD_SIZE`] spans the world).
#[must_use]
pub fn flat_point(source: (f64, f64), target: (f64, f64), r: f64, height: f64) -> DVec3 {
    let s = mercator(source.0, source.1);
    let t = mercator(shortest_target_lon(source.0, target.0), target.1);
    let chord = (t - s).length() * f64::from(super::WORLD_SIZE);
    let m = s.lerp(t, r);
    DVec3::new(m.x, m.y, lift(r, height) * chord)
}

/// lon/lat on the unit sphere (`globe::mercator_to_sphere`'s frame).
#[must_use]
pub fn sphere(lon: f64, lat: f64) -> DVec3 {
    let (lon, lat) = (lon.to_radians(), lat.to_radians());
    DVec3::new(lon.sin() * lat.cos(), lat.sin(), lon.cos() * lat.cos())
}

/// The central angle between two lon/lat points, radians.
#[must_use]
pub fn central_angle(source: (f64, f64), target: (f64, f64)) -> f64 {
    let (a, b) = (sphere(source.0, source.1), sphere(target.0, target.1));
    a.cross(b).length().atan2(a.dot(b))
}

/// The globe arc's point at `r`: the great circle, lifted off the unit
/// sphere by `lift(r) * omega` (Earth radii).
#[must_use]
pub fn globe_point(source: (f64, f64), target: (f64, f64), r: f64, height: f64) -> DVec3 {
    let (ps, pt) = (sphere(source.0, source.1), sphere(target.0, target.1));
    let omega = ps.cross(pt).length().atan2(ps.dot(pt));
    let p = if omega > 1e-4 {
        (ps * ((1.0 - r) * omega).sin() + pt * (r * omega).sin()) / omega.sin()
    } else {
        ps.lerp(pt, r).normalize()
    };
    p * (1.0 + lift(r, height) * omega)
}

/// The chord's length class: `k` with the chord (Mercator, the shortest
/// way) in `(2^-(k+1), 2^-k]` of the world, clamped to `0..CLASSES`.
#[must_use]
pub fn length_class(source: (f64, f64), target: (f64, f64)) -> u8 {
    let s = mercator(source.0, source.1);
    let t = mercator(shortest_target_lon(source.0, target.0), target.1);
    let len = (t - s).length();
    if !(len > 0.0) {
        return CLASSES - 1;
    }
    (-len.log2()).floor().clamp(0.0, f64::from(CLASSES - 1)) as u8
}

/// Segments for the arcs of `class` at `zoom` and `pitch`: the longest
/// chord of the class on screen (logical px, `2^(zoom - k)` world tiles),
/// times the half-circle's 1.6 and a pitch allowance (a tilted view
/// magnifies the near side, `1 + sin(pitch)`), at [`PX_PER_SEGMENT`], in
/// `MIN_SEGMENTS..=max`.
#[must_use]
pub fn segments_for(class: u8, zoom: f64, pitch: f32, max: u32) -> u32 {
    let chord_px = f64::from(super::WORLD_SIZE) * (zoom - f64::from(class)).exp2();
    let gain = 1.0 + f64::from(pitch).sin().abs();
    let n = (chord_px * 1.6 * gain / PX_PER_SEGMENT).ceil();
    // A `max` under MIN_SEGMENTS is taken as asked: a flat (`height` 0)
    // layer of short pieces of an already-sampled polyline -- bundled
    // flows -- is exact at one segment each, and four would quadruple its
    // vertices for nothing (deck.gl's `numSegments` takes any count too).
    let max = max.max(1);
    let min = MIN_SEGMENTS.min(max);
    if n.is_finite() {
        (n as u32).clamp(min, max)
    } else {
        max
    }
}

// ---------------------------------------------------------------------
// The CPU set
// ---------------------------------------------------------------------

/// The arcs of one length class: a range of [`ArcSet::instances`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcBucket {
    pub class: u8,
    pub range: Range<u32>,
}

/// A layer's arcs, sorted by length class.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArcSet {
    pub instances: Vec<ArcInstance>,
    pub buckets: Vec<ArcBucket>,
    /// What every instance's `filter` is relative to, and the column's
    /// absolute `[min, max]` (`overlay::filter::PointFilter`), when the set
    /// carries filter values.
    pub filter: Option<ArcFilterInfo>,
    /// Each instance's row in the caller's input, in instance order: the
    /// sort by length class moves arcs, and a pick
    /// (`render::arc_pick`, `MapView::pick`) answers with the instance's
    /// index, which this maps back to the row the host's own columns use.
    ///
    /// INVARIANT: either empty (a set built field by field: an index is
    /// then its own row) or exactly one entry per instance. A caller that
    /// edits `instances` after [`Self::new`] must edit `rows` with it, or
    /// clear it. [`Self::row`] checks it in debug builds.
    pub rows: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArcFilterInfo {
    pub base: f64,
    pub domain: [f64; 2],
}

impl ArcSet {
    /// Sorts `instances` by length class (stable: input order within a
    /// class). A zero-length arc or a non-finite end is dropped: deck draws
    /// nothing there either.
    #[must_use]
    pub fn new(instances: Vec<ArcInstance>) -> Self {
        let mut keyed: Vec<(u8, u32, ArcInstance)> = instances
            .into_iter()
            .enumerate()
            .filter(|(_, a)| a.source.iter().chain(&a.target).all(|v| v.is_finite()) && a.source != a.target)
            .map(|(row, a)| {
                let f = |p: [f32; 2]| (f64::from(p[0]), f64::from(p[1]));
                (length_class(f(a.source), f(a.target)), row as u32, a)
            })
            .collect();
        keyed.sort_by_key(|(k, _, _)| *k);
        let mut buckets: Vec<ArcBucket> = Vec::new();
        for (i, (k, _, _)) in keyed.iter().enumerate() {
            let i = i as u32;
            match buckets.last_mut() {
                Some(b) if b.class == *k => b.range.end = i + 1,
                _ => buckets.push(ArcBucket { class: *k, range: i..i + 1 }),
            }
        }
        let rows = keyed.iter().map(|(_, row, _)| *row).collect();
        Self { instances: keyed.into_iter().map(|(_, _, a)| a).collect(), buckets, filter: None, rows }
    }

    /// Instance `index`'s row in the caller's input ([`Self::rows`]).
    #[must_use]
    pub fn row(&self, index: usize) -> Option<usize> {
        debug_assert!(
            self.rows.is_empty() || self.rows.len() == self.instances.len(),
            "ArcSet::rows: {} rows for {} instances",
            self.rows.len(),
            self.instances.len()
        );
        if index >= self.instances.len() {
            return None;
        }
        Some(self.rows.get(index).map_or(index, |&r| r as usize))
    }

    /// The length class instance `index` draws in, from its bucket.
    #[must_use]
    pub fn class_of(&self, index: usize) -> Option<u8> {
        let i = u32::try_from(index).ok()?;
        let k = self.buckets.partition_point(|b| b.range.end <= i);
        self.buckets.get(k).filter(|b| b.range.contains(&i)).map(|b| b.class)
    }

    /// Whether instance `index` draws under the relative filter window
    /// `filter` ([`Self::relative_window`]): `vs_arc`'s own test, so an arc
    /// the window hides is not hit either.
    #[must_use]
    pub fn passes_filter(&self, index: usize, filter: Option<[f32; 2]>) -> bool {
        let Some(a) = self.instances.get(index) else { return false };
        match filter {
            Some([lo, hi]) => a.filter >= lo && a.filter <= hi,
            None => true,
        }
    }

    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Instance bytes an upload costs.
    #[must_use]
    pub fn upload_bytes(&self) -> usize {
        self.instances.len() * std::mem::size_of::<ArcInstance>()
    }

    /// `filter`'s absolute window as the uniform's relative pair, or `None`
    /// when the set has no filter values (then nothing is filtered).
    #[must_use]
    pub fn relative_window(&self, range: [f64; 2]) -> Option<[f32; 2]> {
        self.filter.map(|f| crate::overlay::filter::relative_window(f.base, range))
    }

    /// The vertices one frame draws, per bucket's segments.
    #[must_use]
    pub fn vertices_at(&self, zoom: f64, pitch: f32, style: &ArcStyle) -> u64 {
        self.buckets
            .iter()
            .map(|b| u64::from(b.range.end - b.range.start) * 2 * u64::from(segments_for(b.class, zoom, pitch, style.max_segments) + 1))
            .sum()
    }
}

// ---------------------------------------------------------------------
// The frame
// ---------------------------------------------------------------------

/// The uniform, 208 B, in a 256 B dynamic-offset slot per draw.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ArcUniform {
    pub flat_view_proj: [[f32; 4]; 4],
    pub globe_view_proj: [[f32; 4]; 4],
    pub eye: [f32; 4],
    pub origin: [f32; 4],
    pub viewport: [f32; 4],
    pub params: [f32; 4],
    pub filter_window: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<ArcUniform>() == 208);

/// Everything a frame of arcs needs from the camera, in f64 until written.
#[derive(Debug, Clone, Copy)]
pub struct ArcFrame {
    /// World units relative to `origin_world` -> clip, the east-west flip
    /// (`shader.wgsl`'s `clip.x = -clip.x`) folded in.
    pub flat_view_proj: DMat4,
    pub globe_view_proj: DMat4,
    pub eye: DVec3,
    /// The camera's centre, world units.
    pub origin_world: DVec2,
    pub transition: f32,
    /// The target in device pixels.
    pub viewport: (f32, f32),
    /// Device pixels per logical pixel.
    pub px_scale: f32,
    pub zoom: f64,
    pub pitch: f32,
}

impl ArcFrame {
    /// `px_scale`: device pixels per logical pixel (the camera's viewport is
    /// logical).
    #[must_use]
    pub fn new(camera: &Camera, globe: &GlobeFrame, px_scale: f32) -> Self {
        // The origin snapped to what the uniform can hold (f32, as a
        // fraction of the world): the matrix and the shader then subtract
        // the very same point. Unsnapped, the f32 rounding of a moving
        // camera's centre (up to ~1 m at Web Mercator's world) changes
        // every frame and the layer shivers against the basemap at street
        // zoom; a followed trip head twitched side to side.
        let centre = camera.world_center_f64();
        let world = f64::from(super::WORLD_SIZE);
        let origin = DVec2::new(f64::from((centre.x / world) as f32) * world, f64::from((centre.y / world) as f32) * world);
        let flip = DMat4::from_scale(DVec3::new(-1.0, 1.0, 1.0));
        let flat_view_proj = flip * camera.view_projection_f64() * DMat4::from_translation(DVec3::new(origin.x, origin.y, 0.0));
        let eye = if globe.transition > 0.0 { super::altitude::eye_sphere(globe) } else { DVec3::ZERO };
        Self {
            flat_view_proj,
            globe_view_proj: globe.globe_view_proj,
            eye,
            origin_world: DVec2::new(origin.x, origin.y),
            transition: globe.transition,
            viewport: (camera.viewport_width_px * px_scale, camera.viewport_height_px * px_scale),
            px_scale,
            zoom: camera.zoom,
            pitch: camera.pitch,
        }
    }

    /// The uniform for one draw of `segments` per arc.
    #[must_use]
    pub fn uniform(&self, style: &ArcStyle, segments: u32, filter: Option<[f32; 2]>) -> ArcUniform {
        let world = f64::from(super::WORLD_SIZE);
        let [lo, hi] = filter.unwrap_or([0.0, 0.0]);
        ArcUniform {
            flat_view_proj: self.flat_view_proj.as_mat4().to_cols_array_2d(),
            globe_view_proj: self.globe_view_proj.as_mat4().to_cols_array_2d(),
            eye: [self.eye.x as f32, self.eye.y as f32, self.eye.z as f32, 0.0],
            origin: [(self.origin_world.x / world) as f32, (self.origin_world.y / world) as f32, super::WORLD_SIZE, self.transition],
            viewport: [self.viewport.0, self.viewport.1, self.px_scale, style.opacity],
            params: [
                segments as f32,
                style.height,
                if filter.is_some() { 1.0 } else { 0.0 },
                if style.blend == ArcBlend::Additive { 1.0 } else { 0.0 },
            ],
            filter_window: [lo, hi, 0.0, 0.0],
        }
    }

    /// The CPU mirror of `vs_arc`'s centreline on a FLAT frame: the arc's
    /// point at `r` in LOGICAL screen pixels (y down), `None` behind the
    /// eye.
    #[must_use]
    pub fn project_flat(&self, source: (f64, f64), target: (f64, f64), r: f64, height: f64) -> Option<(f64, f64)> {
        self.screen_of(self.flat_clip(flat_point(source, target, r, height)))
    }

    /// The same on the globe (`transition == 1`): `None` behind the eye or
    /// hidden by the planet.
    #[must_use]
    pub fn project_globe(&self, source: (f64, f64), target: (f64, f64), r: f64, height: f64) -> Option<(f64, f64)> {
        let p = globe_point(source, target, r, height);
        if super::altitude::closest_approach(self.eye, p) < 0.9999 {
            return None;
        }
        self.screen_of(self.globe_view_proj * p.extend(1.0))
    }

    /// The CPU mirror of `vs_arc`'s `arc_clip` on ANY frame -- flat, globe,
    /// or the blend between them (`transition` in `(0, 1)`): the clip
    /// position of the arc's point at `r`, with the shader's `z` handling
    /// (clamped just inside the far plane; a point the planet hides, past
    /// the blend's midpoint, pushed past it to `2w`). The near plane is
    /// NOT applied here: a segment is clipped against it
    /// ([`clip_segment`]) before it is projected, as the GPU clips the
    /// strip.
    #[must_use]
    pub fn clip(&self, source: (f64, f64), target: (f64, f64), r: f64, height: f64) -> glam::DVec4 {
        let t = f64::from(self.transition);
        let flat = (t < 1.0).then(|| self.flat_clip(flat_point(source, target, r, height)));
        let (globe, hidden) = if t > 0.0 {
            let g = globe_point(source, target, r, height);
            (Some(self.globe_view_proj * g.extend(1.0)), super::altitude::closest_approach(self.eye, g) < 0.9999)
        } else {
            (None, false)
        };
        let mut clip = match (flat, globe) {
            (Some(f), Some(g)) => f.lerp(g, t),
            (Some(f), None) => f,
            (None, Some(g)) => g,
            (None, None) => glam::DVec4::new(0.0, 0.0, 0.0, 1.0),
        };
        clip.z = clip.z.min(clip.w * 0.9999);
        if hidden && t > 0.5 {
            clip.z = clip.w * 2.0;
        }
        clip
    }

    /// The arc's point at `r` in LOGICAL screen pixels, as the GPU places
    /// it ([`Self::clip`]); `None` when the near or far plane clips it
    /// (behind or too close to the eye, or hidden by the planet).
    #[must_use]
    pub fn project(&self, source: (f64, f64), target: (f64, f64), r: f64, height: f64) -> Option<(f64, f64)> {
        let c = self.clip(source, target, r, height);
        (c.w > 0.0 && c.z >= 0.0 && c.z <= c.w).then(|| self.screen_of(c)).flatten()
    }

    /// A clip position in LOGICAL screen pixels (the perspective divide and
    /// the viewport), `None` behind the eye. No plane but `w > 0` is
    /// applied: clip a segment first ([`clip_segment`]).
    #[must_use]
    pub fn screen_of_clip(&self, clip: glam::DVec4) -> Option<(f64, f64)> {
        self.screen_of(clip)
    }

    /// The flat arc's Mercator `x` at `r`, wrapped to the world copy
    /// nearest the camera as `vs_arc` wraps it: two consecutive points
    /// more than half a world apart are the seam segment the shader fades
    /// out.
    #[must_use]
    pub fn wrapped_flat_x(&self, source: (f64, f64), target: (f64, f64), r: f64) -> f64 {
        let x = flat_point(source, target, r, 0.0).x;
        let origin = self.origin_world.x / f64::from(super::WORLD_SIZE);
        x - (x - origin).round()
    }

    pub(crate) fn flat_clip(&self, p: DVec3) -> glam::DVec4 {
        let world = f64::from(super::WORLD_SIZE);
        let origin = self.origin_world / world;
        let x = p.x - (p.x - origin.x).round();
        let rel = DVec3::new((x - origin.x) * world, (p.y - origin.y) * world, p.z);
        self.flat_view_proj * rel.extend(1.0)
    }

    /// A point in `flat_view_proj`'s space (world units relative to the
    /// camera's centre) in LOGICAL screen pixels, `None` behind the eye.
    #[must_use]
    pub fn project_flat_rel(&self, rel: DVec3) -> Option<(f64, f64)> {
        self.screen_of(self.flat_view_proj * rel.extend(1.0))
    }

    /// The ray under LOGICAL screen point `(x, y)` on the flat frame, in
    /// world units relative to the camera's centre (`flat_view_proj`'s
    /// space): its near-plane point and its direction toward the far plane.
    #[must_use]
    pub fn flat_ray(&self, x: f64, y: f64) -> Option<(DVec3, DVec3)> {
        Self::ray(self.flat_view_proj, self.logical_ndc(x, y))
    }

    /// The same in the globe's unit-sphere space.
    #[must_use]
    pub fn globe_ray(&self, x: f64, y: f64) -> Option<(DVec3, DVec3)> {
        Self::ray(self.globe_view_proj, self.logical_ndc(x, y))
    }

    fn logical_ndc(&self, x: f64, y: f64) -> (f64, f64) {
        let (w, h) = (f64::from(self.viewport.0 / self.px_scale), f64::from(self.viewport.1 / self.px_scale));
        (x / w * 2.0 - 1.0, 1.0 - y / h * 2.0)
    }

    fn ray(view_proj: DMat4, (nx, ny): (f64, f64)) -> Option<(DVec3, DVec3)> {
        let inv = view_proj.inverse();
        let near = inv * glam::DVec4::new(nx, ny, 0.0, 1.0);
        let far = inv * glam::DVec4::new(nx, ny, 1.0, 1.0);
        if near.w == 0.0 || far.w == 0.0 {
            return None;
        }
        let (p0, p1) = (near.truncate() / near.w, far.truncate() / far.w);
        let dir = p1 - p0;
        (dir.length_squared() > 0.0 && p0.is_finite() && dir.is_finite()).then_some((p0, dir))
    }

    fn screen_of(&self, clip: glam::DVec4) -> Option<(f64, f64)> {
        if clip.w <= 0.0 {
            return None;
        }
        let (w, h) = (f64::from(self.viewport.0 / self.px_scale), f64::from(self.viewport.1 / self.px_scale));
        Some(((clip.x / clip.w + 1.0) * 0.5 * w, (1.0 - clip.y / clip.w) * 0.5 * h))
    }
}

/// The part of the clip-space segment `a`-`b` the GPU keeps of a strip
/// along it: inside the near (`z >= 0`) and far (`z <= w`) planes, cut by
/// linear interpolation in clip space as the rasteriser clips. `None` when
/// nothing of it is left. What arc picking projects, so an arc that passes
/// the eye picks nowhere it does not draw.
#[must_use]
pub fn clip_segment(a: glam::DVec4, b: glam::DVec4) -> Option<(glam::DVec4, glam::DVec4)> {
    let (mut a, mut b) = (a, b);
    // Each plane as `d(p) >= 0`: near `z`, far `w - z`.
    for d in [|p: glam::DVec4| p.z, |p: glam::DVec4| p.w - p.z] {
        let (da, db) = (d(a), d(b));
        if da < 0.0 && db < 0.0 {
            return None;
        }
        if da < 0.0 {
            a = a.lerp(b, da / (da - db));
        } else if db < 0.0 {
            b = b.lerp(a, db / (db - da));
        }
    }
    (a.w > 0.0 && b.w > 0.0).then_some((a, b))
}

// ---------------------------------------------------------------------
// The GPU side
// ---------------------------------------------------------------------

/// One layer's instance buffer on the GPU.
struct GpuLayer {
    buffer: Option<TrackedBuffer>,
    generation: u64,
    buckets: Vec<ArcBucket>,
}

/// One layer to draw this frame.
pub struct ArcLayerDraw<'a> {
    pub id: &'a str,
    /// Bumped whenever `set` changes: the upload is skipped otherwise.
    pub generation: u64,
    pub set: &'a ArcSet,
    pub style: ArcStyle,
    /// The filter window, relative ([`ArcSet::relative_window`]); `None`
    /// draws every arc.
    pub filter: Option<[f32; 2]>,
}

/// One planned draw: a bucket of a layer at its segment count.
#[derive(Debug, Clone)]
struct Planned {
    layer: String,
    range: Range<u32>,
    slot: u32,
    vertices: u32,
    blend: ArcBlend,
}

/// What the last [`ArcPipeline::prepare`] planned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArcFrameStats {
    pub arcs: u64,
    pub draws: u64,
    pub vertices: u64,
    pub upload_bytes: u64,
}

pub struct ArcPipeline {
    normal: wgpu::RenderPipeline,
    additive: wgpu::RenderPipeline,
    screen: wgpu::RenderPipeline,
    /// The arcs into [`GLOW_FORMAT`], One/One.
    glow_sum: wgpu::RenderPipeline,
    /// The tone-mapped composite over the map.
    glow_composite: wgpu::RenderPipeline,
    glow_bgl: wgpu::BindGroupLayout,
    glow_params: TrackedBuffer,
    glow_target: Option<GlowTarget>,
    /// Whether this frame's glow target holds arcs to composite.
    glow_ready: bool,
    bgl: wgpu::BindGroupLayout,
    uniform: Option<(TrackedBuffer, wgpu::BindGroup, u32)>,
    slot_size: u64,
    layers: std::collections::HashMap<String, GpuLayer>,
    planned: Vec<Planned>,
    stats: ArcFrameStats,
    uploads_total: u64,
}

impl ArcPipeline {
    /// `depth_format`: the pass's depth attachment, if it has one; arcs
    /// never test or write depth.
    #[must_use]
    pub fn new(ctx: &GpuContext, color_format: wgpu::TextureFormat, depth_format: Option<wgpu::TextureFormat>) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps arc shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("arc_shader.wgsl").into()),
        });
        let bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("arc-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<ArcUniform>() as u64),
                },
                count: None,
            }],
        });
        let layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("arc-pipeline-layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::OVER,
        };
        let build = |label: &str, blend: wgpu::BlendState| {
            ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_arc"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<ArcInstance>() as wgpu::BufferAddress,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &ARC_ATTRS,
                    })],
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
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
                    entry_point: Some("fs_arc"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: color_format, blend: Some(blend), write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let normal = build("arc-pipeline", wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        let additive_all = wgpu::BlendState {
            color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
            alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
        };
        let glow_sum = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("arc-glow-sum-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_arc"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ArcInstance>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &ARC_ATTRS,
                })],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_arc"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState { format: GLOW_FORMAT, blend: Some(additive_all), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let glow_shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui-maps arc glow shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("arc_glow.wgsl").into()),
        });
        let glow_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("arc-glow-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let glow_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("arc-glow-layout"),
            bind_group_layouts: &[Some(&glow_bgl)],
            immediate_size: 0,
        });
        let glow_composite = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("arc-glow-composite-pipeline"),
            layout: Some(&glow_layout),
            vertex: wgpu::VertexState {
                module: &glow_shader,
                entry_point: Some("vs_glow"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: depth_format.map(|format| wgpu::DepthStencilState {
                format,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &glow_shader,
                entry_point: Some("fs_glow"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let glow_params = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("arc-glow-params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let additive = build("arc-additive-pipeline", additive);
        let screen = build(
            "arc-screen-pipeline",
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrc,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent::OVER,
            },
        );
        let align = u64::from(ctx.device.limits().min_uniform_buffer_offset_alignment);
        let slot_size = (std::mem::size_of::<ArcUniform>() as u64).div_ceil(align) * align;
        Self {
            normal,
            additive,
            screen,
            glow_sum,
            glow_composite,
            glow_bgl,
            glow_params,
            glow_target: None,
            glow_ready: false,
            bgl,
            uniform: None,
            slot_size,
            layers: std::collections::HashMap::new(),
            planned: Vec::new(),
            stats: ArcFrameStats::default(),
            uploads_total: 0,
        }
    }

    /// Uploads what changed, drops layers no longer drawn, writes one
    /// uniform slot per (layer, bucket) and plans the draws. ONE call per
    /// frame: `queue.write_buffer` lands before any draw of the submission.
    pub fn prepare(&mut self, ctx: &GpuContext, frame: &ArcFrame, draws: &[ArcLayerDraw<'_>]) {
        self.layers.retain(|id, _| draws.iter().any(|d| d.id == id));
        let mut upload_bytes = 0u64;
        for d in draws {
            let layer = self.layers.entry(d.id.to_string()).or_insert(GpuLayer { buffer: None, generation: u64::MAX, buckets: Vec::new() });
            if layer.generation != d.generation {
                let bytes: &[u8] = bytemuck::cast_slice(&d.set.instances);
                if !bytes.is_empty() {
                    let need = bytes.len() as u64;
                    if layer.buffer.as_ref().is_none_or(|b| b.tracked_size() < need) {
                        layer.buffer = Some(TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                            label: Some("arc-instances"),
                            size: need,
                            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        }));
                    }
                    if let Some(buffer) = layer.buffer.as_ref() {
                        ctx.queue.write_buffer(buffer, 0, bytes);
                    }
                    upload_bytes += need;
                    self.uploads_total += 1;
                }
                layer.buckets = d.set.buckets.clone();
                layer.generation = d.generation;
            }
        }
        let mut uniforms: Vec<u8> = Vec::new();
        let mut planned = Vec::new();
        let mut stats = ArcFrameStats { upload_bytes, ..ArcFrameStats::default() };
        for d in draws {
            for b in &d.set.buckets {
                let n = segments_for(b.class, frame.zoom, frame.pitch, d.style.max_segments);
                let slot = (uniforms.len() as u64 / self.slot_size) as u32;
                uniforms.extend_from_slice(bytemuck::bytes_of(&frame.uniform(&d.style, n, d.filter)));
                uniforms.resize((u64::from(slot + 1) * self.slot_size) as usize, 0);
                let count = b.range.end - b.range.start;
                let vertices = 2 * (n + 1);
                stats.arcs += u64::from(count);
                stats.draws += 1;
                stats.vertices += u64::from(count) * u64::from(vertices);
                planned.push(Planned {
                    layer: d.id.to_string(),
                    range: b.range.clone(),
                    slot,
                    vertices,
                    blend: d.style.blend,
                });
            }
        }
        if !uniforms.is_empty() {
            let slots = (uniforms.len() as u64 / self.slot_size) as u32;
            if self.uniform.as_ref().is_none_or(|(_, _, have)| *have < slots) {
                let have = slots.next_power_of_two().max(8);
                let buffer = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
                    label: Some("arc-uniforms"),
                    size: u64::from(have) * self.slot_size,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("arc-bind-group"),
                    layout: &self.bgl,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &buffer,
                            offset: 0,
                            size: wgpu::BufferSize::new(std::mem::size_of::<ArcUniform>() as u64),
                        }),
                    }],
                });
                self.uniform = Some((buffer, bind_group, have));
            }
            if let Some((buffer, _, _)) = self.uniform.as_ref() {
                ctx.queue.write_buffer(buffer, 0, &uniforms);
            }
        }
        if let Some(d) = draws.iter().find(|d| d.style.blend == ArcBlend::Glow) {
            ctx.queue.write_buffer(&self.glow_params, 0, bytemuck::cast_slice(&[d.style.exposure, d.style.desaturate, 0.0, 0.0]));
        }
        self.planned = planned;
        self.stats = stats;
        self.glow_ready = false;
    }

    /// Sums this frame's [`ArcBlend::Glow`] draws into the glow target, in
    /// its own pass on `encoder`; call after [`Self::prepare`] and before
    /// the pass [`Self::draw`] draws into (which composites the sum).
    /// `size`: the colour target's, physical pixels. Nothing happens when
    /// no glow layer is planned.
    pub fn accumulate(&mut self, ctx: &GpuContext, encoder: &mut wgpu::CommandEncoder, size: (u32, u32)) {
        self.glow_ready = false;
        if !self.planned.iter().any(|p| p.blend == ArcBlend::Glow) || size.0 == 0 || size.1 == 0 {
            return;
        }
        if self.glow_target.as_ref().is_none_or(|t| t.size != size) {
            let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("arc-glow-target"),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: GLOW_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("arc-glow-bind-group"),
                layout: &self.glow_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: self.glow_params.as_entire_binding() },
                ],
            });
            self.glow_target = Some(GlowTarget { _texture: texture, view, bind_group, size });
        }
        let Some(target) = self.glow_target.as_ref() else { return };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("arc-glow-sum-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.glow_sum);
        self.draw_planned(&mut pass, |b| b == ArcBlend::Glow, false);
        drop(pass);
        self.glow_ready = true;
    }

    /// What the last prepare planned.
    #[must_use]
    pub fn stats(&self) -> ArcFrameStats {
        self.stats
    }

    /// Instance uploads since this pipeline was built (a filter move makes
    /// none).
    #[must_use]
    pub fn uploads_total(&self) -> u64 {
        self.uploads_total
    }

    /// Draws what [`Self::prepare`] planned, in layer order, then the
    /// glow layers' tone-mapped sum ([`Self::accumulate`]) over them.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.draw_planned(pass, |b| b != ArcBlend::Glow, true);
        if self.glow_ready {
            if let Some(target) = self.glow_target.as_ref() {
                pass.set_pipeline(&self.glow_composite);
                pass.set_bind_group(0, &target.bind_group, &[]);
                pass.draw(0..3, 0..1);
                super::alloc::note_draw_calls(1);
            }
        }
    }

    fn draw_planned(&self, pass: &mut wgpu::RenderPass<'_>, which: impl Fn(ArcBlend) -> bool, set_pipelines: bool) {
        let Some((_, bind_group, _)) = self.uniform.as_ref() else { return };
        let stride = std::mem::size_of::<ArcInstance>() as u64;
        let mut bound = None;
        for p in self.planned.iter().filter(|p| which(p.blend)) {
            let Some(buffer) = self.layers.get(&p.layer).and_then(|l| l.buffer.as_ref()) else { continue };
            if p.range.is_empty() {
                continue;
            }
            if set_pipelines && bound != Some(p.blend) {
                pass.set_pipeline(match p.blend {
                    ArcBlend::Normal => &self.normal,
                    ArcBlend::Additive => &self.additive,
                    ArcBlend::Screen => &self.screen,
                    ArcBlend::Glow => &self.glow_sum,
                });
                bound = Some(p.blend);
            }
            pass.set_bind_group(0, bind_group, &[(u64::from(p.slot) * self.slot_size) as u32]);
            // The bucket's instances by buffer offset rather than
            // `first_instance`, which WebGL2 lacks.
            pass.set_vertex_buffer(0, buffer.slice(u64::from(p.range.start) * stride..u64::from(p.range.end) * stride));
            pass.draw(0..p.vertices, 0..p.range.end - p.range.start);
            super::alloc::note_draw_calls(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arc(s: (f32, f32), t: (f32, f32)) -> ArcInstance {
        ArcInstance {
            source: [s.0, s.1],
            target: [t.0, t.1],
            source_color: pack_color(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }),
            target_color: pack_color(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }),
            width_px: 2.0,
            filter: 0.0,
        }
    }

    #[test]
    fn the_instance_is_32_bytes_and_colours_pack_r_first() {
        assert_eq!(std::mem::size_of::<ArcInstance>(), 32);
        assert_eq!(pack_color(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 0.5 }).to_le_bytes(), [255, 0, 0, 128]);
    }

    /// deck's paraboloid with both ends on the ground: the peak is half the
    /// chord at height 1, at the chord's middle, and the ends are on it.
    #[test]
    fn the_midpoint_rises_half_the_chord_and_the_ends_stay_down() {
        let (s, t) = ((-0.1, 51.5), (-2.24, 53.48));
        let chord = (mercator(t.0, t.1) - mercator(s.0, s.1)).length() * f64::from(crate::render::WORLD_SIZE);
        let mid = flat_point(s, t, 0.5, 1.0);
        assert!((mid.z - chord / 2.0).abs() < 1e-12 * chord.max(1.0), "{} vs {}", mid.z, chord / 2.0);
        assert!((flat_point(s, t, 0.5, 2.0).z - chord).abs() < 1e-9);
        assert_eq!(flat_point(s, t, 0.0, 1.0).z, 0.0);
        assert_eq!(flat_point(s, t, 1.0, 1.0).z, 0.0);
        let m = (mercator(s.0, s.1) + mercator(t.0, t.1)) / 2.0;
        assert!((mid.truncate() - m).length() < 1e-15);
        // Symmetric about the middle, and the segment ratios with it.
        assert!((flat_point(s, t, 0.3, 1.0).z - flat_point(s, t, 0.7, 1.0).z).abs() < 1e-12);
        assert_eq!(segment_ratio(0, 10), 0.0);
        assert_eq!(segment_ratio(10, 10), 1.0);
        assert!((segment_ratio(5, 10) - 0.5).abs() < 1e-15);
        assert!(segment_ratio(1, 10) < 0.1, "denser at the ends");
    }

    /// On the globe the arc lifts off the sphere: the ends on it, the middle
    /// `omega / 2` radii above it, on the great circle's bisector.
    #[test]
    fn the_globe_arc_lifts_off_the_sphere() {
        let (s, t) = ((-74.0, 40.7), (-0.1, 51.5));
        let omega = central_angle(s, t);
        assert!((globe_point(s, t, 0.0, 1.0) - sphere(s.0, s.1)).length() < 1e-12);
        assert!((globe_point(s, t, 1.0, 1.0) - sphere(t.0, t.1)).length() < 1e-12);
        let mid = globe_point(s, t, 0.5, 1.0);
        assert!((mid.length() - (1.0 + omega / 2.0)).abs() < 1e-12, "lifted {} vs {}", mid.length() - 1.0, omega / 2.0);
        let bisector = (sphere(s.0, s.1) + sphere(t.0, t.1)).normalize();
        assert!(mid.normalize().dot(bisector) > 1.0 - 1e-12);
        for k in 1..10 {
            assert!(globe_point(s, t, f64::from(k) / 10.0, 1.0).length() > 1.0, "every inner point is above the sphere");
        }
        // NYC-London is ~5,570 km: ~0.874 rad.
        assert!((omega * 6_371.0 - 5_570.0).abs() < 30.0, "{}", omega * 6_371.0);
    }

    /// Across the antimeridian the flat arc goes the short way (Tokyo to
    /// San Francisco crosses 180, not Greenwich), and the globe arc is the
    /// same great circle either way round.
    #[test]
    fn an_arc_across_the_antimeridian_takes_the_short_way() {
        let (tokyo, sf) = ((139.7, 35.7), (-122.4, 37.8));
        assert!((shortest_target_lon(tokyo.0, sf.0) - 237.6).abs() < 1e-9);
        assert!((shortest_target_lon(sf.0, tokyo.0) - -220.3).abs() < 1e-9);
        assert_eq!(shortest_target_lon(10.0, 20.0), 20.0);
        let mid = flat_point(tokyo, sf, 0.5, 1.0);
        let mid_lon = mid.x * 360.0 - 180.0;
        assert!((mid_lon - 188.65).abs() < 1e-6, "the middle is over the Pacific past 180: {mid_lon}");
        let (a, b) = (flat_point(tokyo, sf, 0.49, 1.0), flat_point(tokyo, sf, 0.51, 1.0));
        assert!((a.x - b.x).abs() < 0.05, "no world-spanning jump");
        let g = globe_point(tokyo, sf, 0.5, 1.0).normalize();
        let lon = g.x.atan2(g.z).to_degrees();
        assert!(lon > 170.0 || lon < -170.0, "the great circle's middle is near 180: {lon}");
        assert!((globe_point(tokyo, sf, 0.25, 1.0) - globe_point(sf, tokyo, 0.75, 1.0)).length() < 1e-12);
        // The length class sees the short chord, not the long one.
        assert_eq!(length_class(tokyo, sf), length_class((139.7 - 360.0, 35.7), sf));
        assert!(length_class(tokyo, sf) <= 2);
    }

    #[test]
    fn arcs_sort_into_length_classes_and_degenerates_drop() {
        let set = ArcSet::new(vec![
            arc((0.0, 0.0), (90.0, 0.0)),
            arc((0.0, 0.0), (0.01, 0.0)),
            arc((1.0, 1.0), (1.0, 1.0)),
            arc((0.0, 0.0), (f32::NAN, 0.0)),
            arc((0.0, 0.0), (0.02, 0.0)),
            arc((0.0, 0.0), (45.0, 0.0)),
        ]);
        assert_eq!(set.len(), 4);
        let classes: Vec<u8> = set.buckets.iter().map(|b| b.class).collect();
        assert_eq!(classes, vec![2, 3, 14, 15]);
        assert_eq!(set.buckets.iter().map(|b| b.range.end - b.range.start).sum::<u32>(), 4);
        assert_eq!(set.instances[0].target, [90.0, 0.0]);
        assert_eq!(set.instances[3].target, [0.01, 0.0]);
        for w in set.buckets.windows(2) {
            assert_eq!(w[0].range.end, w[1].range.start);
        }
    }

    #[test]
    fn short_arcs_get_few_segments_and_long_ones_deck_s_fifty() {
        // A 10 km commute at Kepler's UK view (z6.66): a dozen pixels.
        let k = length_class((-2.0, 52.0), (-1.9, 52.05));
        assert_eq!(k, 11);
        assert!(segments_for(k, 6.66, 0.0, MAX_SEGMENTS) <= 12);
        assert_eq!(segments_for(k, 0.0, 0.0, MAX_SEGMENTS), MIN_SEGMENTS);
        assert_eq!(segments_for(1, 6.66, 0.0, MAX_SEGMENTS), MAX_SEGMENTS);
        assert!(segments_for(k, 6.66, 1.0, MAX_SEGMENTS) > segments_for(k, 6.66, 0.0, MAX_SEGMENTS), "pitch asks for more");
        assert_eq!(segments_for(0, 20.0, 0.0, 12), 12);
        // Under MIN_SEGMENTS is taken as asked (a flat polyline's pieces),
        // and 0 still draws one.
        assert_eq!(segments_for(0, 20.0, 0.0, 1), 1);
        assert_eq!(segments_for(k, 0.0, 0.0, 1), 1);
        assert_eq!(segments_for(k, 0.0, 0.0, 0), 1);
    }

    #[test]
    fn the_filter_window_is_relative_to_the_sets_base() {
        let mut set = ArcSet::new(vec![arc((0.0, 0.0), (1.0, 1.0))]);
        assert_eq!(set.relative_window([0.0, 1.0]), None, "no values, no filter");
        set.filter = Some(ArcFilterInfo { base: 1.4e12, domain: [1.4e12, 1.5e12] });
        let [lo, hi] = set.relative_window([1.4e12 + 1000.0, 1.4e12 + 2000.0]).unwrap();
        assert!((999.0..=1000.0).contains(&lo) && (2000.0..2001.0).contains(&hi));
    }

    #[test]
    fn the_arc_shader_is_valid_glsl_es_300() {
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("arc_shader.wgsl"),
            &[("vs_arc", naga::ShaderStage::Vertex), ("fs_arc", naga::ShaderStage::Fragment)],
        );
        crate::render::dots::assert_valid_glsl_es_300(
            include_str!("arc_glow.wgsl"),
            &[("vs_glow", naga::ShaderStage::Vertex), ("fs_glow", naga::ShaderStage::Fragment)],
        );
    }

    fn flat_camera(lon: f64, lat: f64, zoom: f64, pitch_deg: f32) -> Camera {
        Camera {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch: pitch_deg.to_radians(),
            bearing: 0.0,
            viewport_width_px: 256.0,
            viewport_height_px: 256.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// Renders `instances` once into a 256x256 RGBA8 target over black.
    fn render(ctx: &GpuContext, frame: &ArcFrame, set: &ArcSet, style: ArcStyle, filter: Option<[f32; 2]>) -> Vec<u8> {
        let mut pipeline = ArcPipeline::new(ctx, wgpu::TextureFormat::Rgba8Unorm, None);
        pipeline.prepare(ctx, frame, &[ArcLayerDraw { id: "a", generation: 1, set, style, filter }]);
        let size = wgpu::Extent3d { width: 256, height: 256, depth_or_array_layers: 1 };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("arc-test"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let read = TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("arc-test-read"),
            size: 256 * 256 * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        pipeline.accumulate(ctx, &mut encoder, (256, 256));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("arc-test-pass"),
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
            pipeline.draw(&mut pass);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &read, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256 * 4), rows_per_image: Some(256) } },
            size,
        );
        ctx.queue.submit(Some(encoder.finish()));
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let out = read.slice(..).get_mapped_range().expect("mapped").to_vec();
        out
    }

    fn device(limits: wgpu::Limits) -> Option<GpuContext> {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions::default()).await.ok()?;
            let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor { required_limits: limits, ..Default::default() }).await.ok()?;
            let max_buffer_size = device.limits().max_buffer_size;
            Some(GpuContext { device, queue, max_buffer_size })
        })
    }

    fn lit(px: &[u8], x: f64, y: f64, r: i32) -> u32 {
        let (cx, cy) = (x.round() as i32, y.round() as i32);
        let mut best = 0u32;
        for dy in -r..=r {
            for dx in -r..=r {
                let (x, y) = (cx + dx, cy + dy);
                if !(0..256).contains(&x) || !(0..256).contains(&y) {
                    continue;
                }
                let i = ((y * 256 + x) * 4) as usize;
                best = best.max(u32::from(px[i]) + u32::from(px[i + 1]) + u32::from(px[i + 2]));
            }
        }
        best
    }

    /// The GPU draws the arc where the CPU mirror says, raised off the
    /// ground under pitch, source-coloured at the source end and
    /// target-coloured at the other; a filter window that excludes it
    /// draws nothing, with no re-upload.
    #[test]
    fn the_gpu_draws_the_raised_arc_and_the_filter_hides_it() {
        let Some(ctx) = device(wgpu::Limits::downlevel_webgl2_defaults()) else { return };
        let (s, t) = ((-0.30, 51.45), (0.10, 51.55));
        let camera = flat_camera(-0.10, 51.50, 8.0, 60.0);
        let globe = crate::render::globe::globe_frame(&camera).flattened();
        let frame = ArcFrame::new(&camera, &globe, 1.0);
        let mut one = arc((s.0 as f32, s.1 as f32), (t.0 as f32, t.1 as f32));
        one.filter = 5.0;
        let mut set = ArcSet::new(vec![one]);
        set.filter = Some(ArcFilterInfo { base: 0.0, domain: [0.0, 10.0] });
        let px = render(&ctx, &frame, &set, ArcStyle::default(), None);
        let apex = frame.project_flat(s, t, 0.5, 1.0).expect("on screen");
        let ground = frame.project_flat(s, t, 0.5, 0.0).expect("on screen");
        assert!((apex.1 - ground.1).abs() > 20.0, "pitched, the apex is well above the chord: {apex:?} {ground:?}");
        assert!(lit(&px, apex.0, apex.1, 2) > 100, "the apex is drawn");
        assert_eq!(lit(&px, ground.0, ground.1, 2), 0, "the chord's middle on the ground is not");
        let near_source = frame.project_flat(s, t, 0.08, 1.0).unwrap();
        let i = ((near_source.1.round() as usize) * 256 + near_source.0.round() as usize) * 4;
        let near_target = frame.project_flat(s, t, 0.92, 1.0).unwrap();
        let j = ((near_target.1.round() as usize) * 256 + near_target.0.round() as usize) * 4;
        assert!(px[i] > px[i + 2], "red toward the source: {:?}", &px[i..i + 4]);
        assert!(px[j + 2] > px[j], "blue toward the target: {:?}", &px[j..j + 4]);
        // The window: 5.0 inside draws, outside hides.
        let inside = render(&ctx, &frame, &set, ArcStyle::default(), set.relative_window([4.0, 6.0]));
        assert!(lit(&inside, apex.0, apex.1, 2) > 100);
        let outside = render(&ctx, &frame, &set, ArcStyle::default(), set.relative_window([6.0, 9.0]));
        assert!(outside.chunks(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0), "filtered out, nothing drawn");
    }

    /// Additive blending adds: two coincident half-alpha arcs are brighter
    /// than one, where "over" would only approach the colour.
    #[test]
    fn additive_arcs_add_up() {
        let Some(ctx) = device(wgpu::Limits::downlevel_webgl2_defaults()) else { return };
        let (s, t) = ((-0.30, 51.45), (0.10, 51.55));
        let camera = flat_camera(-0.10, 51.50, 9.0, 0.0);
        let frame = ArcFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let mut a = arc((s.0 as f32, s.1 as f32), (t.0 as f32, t.1 as f32));
        let grey = pack_color(Rgba { r: 0.4, g: 0.4, b: 0.4, a: 1.0 });
        (a.source_color, a.target_color, a.width_px) = (grey, grey, 4.0);
        let p = frame.project_flat(s, t, 0.5, 1.0).unwrap();
        let at = |px: &[u8]| px[((p.1.round() as usize) * 256 + p.0.round() as usize) * 4];
        let one = render(&ctx, &frame, &ArcSet::new(vec![a]), ArcStyle { blend: ArcBlend::Additive, ..ArcStyle::default() }, None);
        let two = render(&ctx, &frame, &ArcSet::new(vec![a, a]), ArcStyle { blend: ArcBlend::Additive, ..ArcStyle::default() }, None);
        let over = render(&ctx, &frame, &ArcSet::new(vec![a, a]), ArcStyle::default(), None);
        assert!((i32::from(at(&one)) - 102).abs() <= 2, "{}", at(&one));
        assert!((i32::from(at(&two)) - 204).abs() <= 3, "{}", at(&two));
        assert!((i32::from(at(&over)) - 102).abs() <= 2, "{}", at(&over));
    }

    /// Glow: a lone arc draws as "over" would (`1 - exp(-E)` ~ `E`), and a
    /// stack that a plain sum clips to white keeps its hue: 40 coincident
    /// orange arcs stay orange-to-pale, blue never reaches the red.
    #[test]
    fn glow_brightens_with_density_without_clipping_to_white() {
        let Some(ctx) = device(wgpu::Limits::downlevel_webgl2_defaults()) else { return };
        let (s, t) = ((-0.30, 51.45), (0.10, 51.55));
        let camera = flat_camera(-0.10, 51.50, 9.0, 0.0);
        let frame = ArcFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        let mut a = arc((s.0 as f32, s.1 as f32), (t.0 as f32, t.1 as f32));
        let orange = pack_color(Rgba { r: 0.9, g: 0.6, b: 0.0, a: 1.0 });
        (a.source_color, a.target_color, a.width_px) = (orange, orange, 4.0);
        let p = frame.project_flat(s, t, 0.5, 1.0).unwrap();
        let at = |px: &[u8]| {
            let i = ((p.1.round() as usize) * 256 + p.0.round() as usize) * 4;
            [px[i], px[i + 1], px[i + 2]]
        };
        let glow = ArcStyle { opacity: 0.3, blend: ArcBlend::Glow, ..ArcStyle::default() };
        let one = at(&render(&ctx, &frame, &ArcSet::new(vec![a]), glow, None));
        let want = (glow_tonemap([0.9 * 0.3, 0.6 * 0.3, 0.0], 1.0, glow.desaturate)[0] * 255.0).round() as i32;
        assert!((i32::from(one[0]) - want).abs() <= 2, "{one:?} vs {want}");
        let many = at(&render(&ctx, &frame, &ArcSet::new(vec![a; 40]), glow, None));
        let summed = at(&render(&ctx, &frame, &ArcSet::new(vec![a; 40]), ArcStyle { blend: ArcBlend::Additive, ..glow }, None));
        assert!(many[0] > one[0] && many[1] > one[1], "brighter with density: {one:?} -> {many:?}");
        assert_eq!(many[2], 0, "no blue from orange arcs");
        assert!(f32::from(many[1]) < 0.85 * f32::from(many[0]), "still orange: {many:?}");
        assert_eq!(summed[..2], [255, 255], "a plain sum clips: {summed:?}");
        assert!(many[1] < 235, "the glow does not: {many:?}");
        // The mirror: the hue holds and blue stays out at any density.
        let dense = glow_tonemap([100.0, 66.0, 0.0], 1.0, 0.35);
        assert!(dense[2] < 0.36 && dense[1] < 0.9 && dense[0] > 0.99, "{dense:?}");
        assert_eq!(glow_tonemap([100.0, 66.0, 0.0], 1.0, 0.0)[1], 0.66, "d = 0 keeps the ratio");
    }

    /// A point between the eye and the near plane is clipped, not
    /// projected: `project` says `None` where the old divide-only mirror
    /// put it far off on screen, and a segment from it to a visible point
    /// is cut at `z = 0`, so its visible part stays where it is drawn.
    #[test]
    fn the_near_plane_clips_a_point_too_close_to_the_eye() {
        let camera = flat_camera(-0.10, 51.50, 13.0, 0.0);
        let frame = ArcFrame::new(&camera, &crate::render::globe::globe_frame(&camera).flattened(), 1.0);
        // Straight under the eye, 98% of the way up to it: in front of it,
        // inside the near plane (5% of the distance).
        let eye_world = camera.distance();
        let (s, t) = ((-0.10 - 0.05, 51.50), (-0.10 + 0.05, 51.50));
        let chord = (mercator(t.0, t.1) - mercator(s.0, s.1)).length() * f64::from(crate::render::WORLD_SIZE);
        let height = 2.0 * 0.98 * eye_world / chord;
        let c = frame.clip(s, t, 0.5, height);
        assert!(c.w > 0.0 && c.z < 0.0, "in front of the eye, before the near plane: {c:?}");
        assert_eq!(frame.project(s, t, 0.5, height), None);
        assert!(frame.screen_of_clip(c).is_some(), "the bare divide would have placed it somewhere");
        let low = frame.clip(s, t, 0.05, height);
        assert!(low.z >= 0.0 && low.z <= low.w, "the arc's foot is visible: {low:?}");
        let (a, b) = clip_segment(low, c).expect("the foot's part survives");
        assert!((b.z).abs() < 1e-9 * b.w.abs().max(1.0) && a == low, "cut at the near plane: {a:?} {b:?}");
        assert!(clip_segment(c, c).is_none(), "wholly inside the near plane: nothing");
        let far = glam::DVec4::new(0.0, 0.0, 3.0, 1.0);
        assert!(clip_segment(far, far).is_none(), "wholly past the far plane: nothing");
    }

    /// On a full globe frame the arc's apex projects off the planet's
    /// surface and the GPU draws it there.
    #[test]
    fn the_gpu_draws_the_globe_arc_above_the_planet() {
        let Some(ctx) = device(wgpu::Limits::downlevel_webgl2_defaults()) else { return };
        let (s, t) = ((-20.0, 0.0), (20.0, 0.0));
        let camera = flat_camera(0.0, 0.0, 1.0, 0.0);
        let globe = crate::render::globe::globe_frame(&camera);
        assert!(globe.transition >= 1.0, "z1 is a full globe");
        let frame = ArcFrame::new(&camera, &globe, 1.0);
        let apex = frame.project_globe(s, t, 0.5, 1.0).expect("visible");
        let ground = frame.project_globe(s, t, 0.5, 0.0).expect("visible");
        assert!((apex.0 - ground.0).abs() < 1.0, "the apex is straight up the bisector, toward the eye: {apex:?} {ground:?}");
        let px = render(&ctx, &frame, &ArcSet::new(vec![arc((s.0 as f32, s.1 as f32), (t.0 as f32, t.1 as f32))]), ArcStyle::default(), None);
        assert!(lit(&px, apex.0, apex.1, 2) > 100, "the apex is drawn");
        // An arc on the far side of the planet is hidden by it.
        let far = render(&ctx, &frame, &ArcSet::new(vec![arc((160.0, 0.0), (-160.0, 0.0))]), ArcStyle { height: 0.2, ..ArcStyle::default() }, None);
        assert!(far.chunks(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0), "the far-side arc is behind the planet");
    }
}
