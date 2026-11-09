//! Instanced circles: one 20-byte instance per dot, drawn as a quad whose
//! fragment shader cuts the circle out by its signed distance.
//!
//! The overlay's point layer used to tessellate every dot as a lyon round
//! cap -- 64 vertices of the 88-byte vertex, 5.6 KB a dot -- which made a
//! 60k-dot view a 340 MB upload. An instance is the dot's centre (in the
//! overlay's lyon units, the same f32 the cap's vertices carried), its
//! radius in pixels and a paint index into a `compact::paint_table`. The
//! quad is generated from `@builtin(vertex_index)`, so the pipeline keeps
//! ONE vertex buffer (the instances, `step_mode: Instance`).
//!
//! `shader.wgsl`'s `vs_circle` runs the quad's corners through
//! `vertex_main` -- the stroke path the round cap went through: the same
//! screen-pixel extrusion, the same globe projection and line-thickness
//! correction, the same fog -- and `fs_main` antialiases it with the same
//! `aa_outset`/`aa_feather` formula, off an exact distance instead of an
//! interpolated fan normal. So the interior of a circle is bit-identical to
//! the tessellated one and only its one-pixel antialiased rim differs
//! (`capture`'s `the_point_path_draws_the_same_pixels_as_the_whole_source_path_at_high_zoom`).
//! No new inter-stage variable: `VertexOutput` is shared.

use crate::style::{PaintOp, PaintPlan, Rgba};

/// One dot. 20 bytes: `center` (lyon units of the draw's origin), radius in
/// logical pixels, the paint index (entry of the paint table: colour
/// pair `from`/`to`, blended by the model's `blend_t` like `vs_compact`),
/// and the row's filter value (`overlay::filter`; read only when the
/// draw's [`CircleParams::filter`] is on, so 0 for an unfiltered source).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CircleInstance {
    pub center: [f32; 2],
    pub radius_px: f32,
    pub paint: u32,
    pub filter: f32,
}

/// `circle-pitch-alignment` / `circle-pitch-scale`, as the style spec has
/// them: `viewport` / `map` by default -- a circle stays ROUND on screen,
/// and is sized by its distance from the camera the way the map is
/// (MapLibre's `circle.vertex.glsl`, `!u_pitch_with_map` and
/// `u_scale_with_map`). `align_map` lays the circle on the ground instead,
/// foreshortened with it; `scale_map: false` keeps the declared radius in
/// pixels wherever the circle is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CirclePitch {
    pub align_map: bool,
    pub scale_map: bool,
}

impl Default for CirclePitch {
    fn default() -> Self {
        Self { align_map: false, scale_map: true }
    }
}

/// `CircleInstance::paint`'s top bit: `circle-pitch-alignment: map`.
pub const PAINT_ALIGN_MAP: u32 = 1 << 31;
/// `CircleInstance::paint`'s next bit: `circle-pitch-scale: viewport`.
pub const PAINT_SCALE_VIEWPORT: u32 = 1 << 30;
/// The paint index itself, under the two flag bits. `vs_circle` masks with
/// the same value.
pub const PAINT_INDEX_MASK: u32 = PAINT_SCALE_VIEWPORT - 1;

impl CirclePitch {
    /// The flag bits an instance's `paint` carries for this pitch: zero for
    /// the spec's defaults, so an unflagged instance is a default circle.
    #[must_use]
    pub fn flags(self) -> u32 {
        (if self.align_map { PAINT_ALIGN_MAP } else { 0 }) | (if self.scale_map { 0 } else { PAINT_SCALE_VIEWPORT })
    }
}

/// `vs_circle`'s instance attributes, `@location(0..=3)`.
pub const CIRCLE_ATTRS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32,
    2 => Uint32,
    3 => Float32,
];

/// Corners per instance: a triangle strip.
pub const CIRCLE_CORNERS: u32 = 4;

/// A layer of instanced circles, with the paint table its indices address.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CircleSet {
    pub instances: Vec<CircleInstance>,
    /// Colour pairs (`from`, `to`), for `compact::paint_table`.
    pub paints: Vec<(Rgba, Rgba)>,
}

impl CircleSet {
    /// Flags every instance with `pitch` (`CirclePitch::flags`); the paint
    /// index under the flags is unchanged.
    pub fn set_pitch(&mut self, pitch: CirclePitch) {
        let flags = pitch.flags();
        for instance in &mut self.instances {
            instance.paint = (instance.paint & PAINT_INDEX_MASK) | flags;
        }
    }

    /// Bytes an upload of this set costs: instances plus the paint table.
    #[must_use]
    pub fn upload_bytes(&self) -> usize {
        let rows = self.paints.len().div_ceil((crate::render::compact::PAINT_TABLE_WIDTH / 2) as usize).max(1);
        self.instances.len() * std::mem::size_of::<CircleInstance>()
            + rows * crate::render::compact::PAINT_TABLE_WIDTH as usize * 16
    }

    /// Resident heap bytes.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.instances.capacity() * std::mem::size_of::<CircleInstance>()
            + self.paints.capacity() * std::mem::size_of::<(Rgba, Rgba)>()
    }

    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }
}

/// Whether `op` is a dot op: a round-capped stroke over POINT geometry,
/// which is how `overlay::plan` spells a circle (the width is the
/// diameter).
fn dot_geometry(op: &PaintOp) -> Option<(&geo_types::Geometry<i32>, Rgba, Rgba, f32, f32)> {
    match op {
        PaintOp::Line { color_from, color_to, width_from, width_to, geometry, gap_from, gap_to, dash, .. }
            if matches!(**geometry, geo_types::Geometry::Point(_) | geo_types::Geometry::MultiPoint(_))
                && *gap_from == 0.0
                && *gap_to == 0.0
                && dash.count == 0 =>
        {
            Some((geometry, *color_from, *color_to, *width_from, *width_to))
        }
        _ => None,
    }
}

/// Splits a plan into its dots, as instances, and the ops that are not
/// dots. `coord_scale` is the tessellator's (`mesh::OVERLAY_COORD_SCALE`
/// for an overlay), so a centre is the exact f32 the round cap's vertices
/// were placed at. A dot op whose width varies across its zoom bracket
/// (`width_from != width_to`) stays an op: an instance has one radius.
#[must_use]
pub fn split_dots(plan: &PaintPlan, coord_scale: f32) -> (CircleSet, Vec<PaintOp>) {
    let mut set = CircleSet::default();
    let mut rest = Vec::new();
    let bits = |c: Rgba| [c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()];
    let mut by_paint: std::collections::HashMap<[u32; 8], u32> = std::collections::HashMap::new();
    for op in &plan.ops {
        let Some((geometry, from, to, width_from, width_to)) = dot_geometry(op).filter(|d| d.3 == d.4) else {
            rest.push(op.clone());
            continue;
        };
        let _ = width_to;
        let key = {
            let (a, b) = (bits(from), bits(to));
            [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]]
        };
        let paint = *by_paint.entry(key).or_insert_with(|| {
            set.paints.push((from, to));
            (set.paints.len() - 1) as u32
        });
        let radius_px = width_from / 2.0;
        let mut push = |p: &geo_types::Point<i32>| {
            set.instances.push(CircleInstance {
                center: [p.x() as f32 * coord_scale, p.y() as f32 * coord_scale],
                radius_px,
                paint,
                filter: 0.0,
            });
        };
        match geometry {
            geo_types::Geometry::Point(p) => push(p),
            geo_types::Geometry::MultiPoint(mp) => mp.iter().for_each(push),
            _ => unreachable!("dot_geometry admits points only"),
        }
    }
    (set, rest)
}

/// The exclusion boxes a stand-in draw carries: instances whose centre lies
/// inside either box are not drawn (a finer view already covers it). In
/// the draw's own lyon units, `(min_x, min_y, max_x, max_y)`; an empty box
/// (`min > max`) excludes nothing.
///
/// `filter` is the source's range filter (`overlay::filter`): `(lo, hi,
/// on, 0)`. When `on > 0.5` an instance whose `filter` value is not in
/// `lo..=hi` (a NaN never is) is not drawn. Off (`NONE`) the value is never
/// read, so an unfiltered draw is the draw it always was.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CircleParams {
    pub exclude_a: [f32; 4],
    pub exclude_b: [f32; 4],
    pub filter: [f32; 4],
    /// A packed multi-field filter (`overlay::filter::PackedLayout`), read
    /// when `filter[2]` is 2: per field lane, the level's shift and mask in
    /// the instance's code word and the inclusive level window. A lane with
    /// mask 0 passes.
    pub packed_shift: [u32; 4],
    pub packed_mask: [u32; 4],
    pub packed_lo: [u32; 4],
    pub packed_hi: [u32; 4],
}

impl CircleParams {
    pub const NONE: Self = Self {
        exclude_a: [1.0, 1.0, -1.0, -1.0],
        exclude_b: [1.0, 1.0, -1.0, -1.0],
        filter: [0.0, 0.0, 0.0, 0.0],
        packed_shift: [0; 4],
        packed_mask: [0; 4],
        packed_lo: [0; 4],
        packed_hi: [0; 4],
    };

    /// These params with a packed filter's lanes switched on.
    #[must_use]
    pub fn with_packed_filter(self, u: crate::overlay::filter::PackedUniform) -> Self {
        Self { filter: [0.0, 0.0, 2.0, 0.0], packed_shift: u.shift, packed_mask: u.mask, packed_lo: u.lo, packed_hi: u.hi, ..self }
    }

    /// These params with the filter window `lo..=hi` (relative values, as
    /// the instances carry them) switched on.
    #[must_use]
    pub fn with_filter(self, window: [f32; 2]) -> Self {
        Self { filter: [window[0], window[1], 1.0, 0.0], ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot_op(points: Vec<(i32, i32)>, color: Rgba, diameter: f32) -> PaintOp {
        PaintOp::Line {
            metrics: false,
            color_from: color,
            color_to: color,
            width_from: diameter,
            width_to: diameter,
            gap_from: 0.0,
            gap_to: 0.0,
            cap: crate::style::LineCap::Round,
            join: crate::style::LineJoin::Round,
            dash: crate::style::DashPattern::none(),
            geometry: std::sync::Arc::new(geo_types::Geometry::MultiPoint(geo_types::MultiPoint(
                points.into_iter().map(|(x, y)| geo_types::Point::new(x, y)).collect(),
            ))),
        }
    }

    #[test]
    fn an_instance_is_twenty_bytes() {
        assert_eq!(std::mem::size_of::<CircleInstance>(), 20);
        // Three vec4 (boxes, scalar window) and four uvec4 lanes for a
        // packed multi-field filter: a per-draw uniform, not per instance.
        assert_eq!(std::mem::size_of::<CircleParams>(), 112);
    }

    #[test]
    fn dots_split_into_instances_with_deduplicated_paints() {
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let blue = Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
        let plan = PaintPlan {
            ops: vec![
                dot_op(vec![(1, 2), (3, 4)], red, 8.0),
                dot_op(vec![(5, 6)], blue, 4.0),
                dot_op(vec![(7, 8)], red, 6.0),
            ],
        };
        let (set, rest) = split_dots(&plan, 1.0);
        assert!(rest.is_empty());
        assert_eq!(set.paints, vec![(red, red), (blue, blue)]);
        assert_eq!(
            set.instances,
            vec![
                CircleInstance { center: [1.0, 2.0], radius_px: 4.0, paint: 0, filter: 0.0 },
                CircleInstance { center: [3.0, 4.0], radius_px: 4.0, paint: 0, filter: 0.0 },
                CircleInstance { center: [5.0, 6.0], radius_px: 2.0, paint: 1, filter: 0.0 },
                CircleInstance { center: [7.0, 8.0], radius_px: 3.0, paint: 0, filter: 0.0 },
            ]
        );
    }

    #[test]
    fn a_line_or_a_zoom_varying_dot_stays_an_op() {
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let mut ramp = dot_op(vec![(0, 0)], red, 4.0);
        if let PaintOp::Line { width_to, .. } = &mut ramp {
            *width_to = 8.0;
        }
        let line = PaintOp::Line {
            metrics: false,
            color_from: red,
            color_to: red,
            width_from: 2.0,
            width_to: 2.0,
            gap_from: 0.0,
            gap_to: 0.0,
            cap: crate::style::LineCap::Butt,
            join: crate::style::LineJoin::Miter,
            dash: crate::style::DashPattern::none(),
            geometry: std::sync::Arc::new(geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                (0, 0),
                (1, 1),
            ]))),
        };
        let (set, rest) = split_dots(&PaintPlan { ops: vec![ramp, line] }, 1.0);
        assert!(set.is_empty());
        assert_eq!(rest.len(), 2);
    }
}
