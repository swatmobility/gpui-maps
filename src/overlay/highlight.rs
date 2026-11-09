//! One feature, drawn again on top of its source: the hover highlight.
//!
//! MapLibre's answer to "highlight what the cursor is over" is feature
//! state (`map.setFeatureState({source, id}, {hover: true})`) read by a
//! paint expression such as `["case", ["boolean", ["feature-state",
//! "hover"], false], ...]` -- which re-evaluates the source's paint for the
//! flagged feature and re-uploads its vertices (`source/source_state.ts`,
//! `data/feature_position_map.ts` in maplibre-gl-js, BSD-3). Its own
//! hover example layers a second `line` layer over the fill for the
//! outline.
//!
//! This is that second layer, dedicated and one feature wide: the chosen
//! feature's geometry, pulled from wherever its source keeps the one copy
//! (the tile index's fixed-point geometry for a tiled source, the store's
//! lon/lat otherwise), stroked through the ordinary overlay line path -- so
//! the outline is `outline_width_px` screen pixels at every zoom and is
//! as precise as the geometry it came from -- with an optional casing
//! under it and an optional fill under both. A single polygon: one small
//! plan, tessellated on demand and cached while the highlight holds. No
//! paint table, no feature-state map, no rebake of the source.

use super::geojson::OverlayGeometry;
use super::plan::{build_plan, zoom_dependence, BakeBracket, OverlayPlan};
use super::{FeaturePaint, OverlayFeature, OverlaySnapshot, PropertyTable};
use crate::style::{Rgba, ZoomValue};

/// How [`crate::MapView::set_feature_highlight`] draws its feature.
///
/// Draw order, bottom to top: `fill_color`, then the casing, then the
/// outline. The casing is a stroke `outline_width_px + 2 *
/// casing_width_px` wide, so `casing_width_px` of it shows on each side
/// of the outline -- the dark-under-white edge that keeps a white outline
/// legible over a pale fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HighlightStyle {
    pub outline_color: Rgba,
    /// Screen pixels at every zoom.
    pub outline_width_px: f32,
    /// A tint over the feature's own fill, e.g. white at 0.15 alpha for a
    /// slight brighten. `None` draws no fill.
    pub fill_color: Option<Rgba>,
    /// `None`, or `casing_width_px` of this colour either side of the
    /// outline.
    pub casing_color: Option<Rgba>,
    pub casing_width_px: f32,
}

impl Default for HighlightStyle {
    /// A 2 px white outline over a 1 px dark casing, no fill.
    fn default() -> Self {
        Self {
            outline_color: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            outline_width_px: 2.0,
            fill_color: None,
            casing_color: Some(Rgba { r: 0.08, g: 0.08, b: 0.1, a: 0.85 }),
            casing_width_px: 1.0,
        }
    }
}

/// Feature `index` of `snapshot`'s paint as its source draws it: its own,
/// with the source's declared `circle-color` and `circle-radius`
/// (`set_source_paint`) for its properties laid over. What a point's hover
/// ring ([`highlight_plan`]) is sized from and redraws the point in.
pub(crate) fn drawn_paint(snapshot: &OverlaySnapshot, index: usize) -> Option<FeaturePaint> {
    let feature = snapshot.features.get(index)?;
    let mut paint = feature.paint.clone();
    if !snapshot.paint.is_empty() {
        if let Some(c) = snapshot.paint.circle_color_at(&snapshot.properties, index) {
            paint.circle_color = c;
        }
        let row = super::row_properties(&snapshot.properties, index);
        if let Some(r) = snapshot.paint.circle_radius(&row) {
            paint.circle_radius = r;
        }
    }
    Some(paint)
}

/// Feature `index` of `snapshot`, with its geometry in lon/lat: out of the
/// tile index for an adopted (tiled) feature, whose store copy is gone,
/// and straight from the store for everything else.
pub(crate) fn snapshot_geometry(snapshot: &OverlaySnapshot, index: usize) -> Option<OverlayGeometry> {
    let feature = snapshot.features.get(index)?;
    match &feature.geometry {
        OverlayGeometry::Indexed { .. } => snapshot.tile_index.as_ref()?.feature_geometry(u32::try_from(index).ok()?),
        OverlayGeometry::Empty => None,
        geometry => Some(geometry.clone()),
    }
}

/// A closed ring as a line that joins at its start: the ring's own
/// closing vertex plus its second one, so the stroke's last segment meets
/// the first with a join rather than two butt caps and a notch.
fn closed_line(ring: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut line = ring.to_vec();
    if line.len() >= 3 {
        if line.first() != line.last() {
            line.push(line[0]);
        }
        line.push(line[1]);
    }
    line
}

/// The highlight's plan: fill, casing and outline as up to three features,
/// in that order (the order they draw in). `None` when the geometry draws
/// nothing -- an empty feature, or a style with every part switched off.
///
/// `own` is the feature's own paint. For a POINT it makes the highlight a
/// ring: the casing and outline are discs `outline_width_px` (and the
/// casing beyond it) larger than the point's own `circle-radius`, and the
/// point is drawn again on top in its own colour, so what shows is the
/// point with a white ring round it. Without it (or for an icon) a point
/// is marked by the outline-sized disc alone.
pub fn highlight_plan(geometry: &OverlayGeometry, style: &HighlightStyle, own: Option<&FeaturePaint>) -> Option<OverlayPlan> {
    plan_of(highlight_features(geometry, style, own))
}

/// The features [`highlight_plan`] draws, bottom to top (empty when the
/// geometry draws nothing).
fn highlight_features(geometry: &OverlayGeometry, style: &HighlightStyle, own: Option<&FeaturePaint>) -> Vec<OverlayFeature> {
    // An invisible point (a circle of alpha 0 and no icon: a hit target
    // laid under a pin) stays invisible on hover. Its ring would draw the
    // outline disc in full, its own transparent colour hiding none of it.
    if let (OverlayGeometry::Point(_), Some(own)) = (geometry, own) {
        if own.icon_image.is_none() && own.circle_color.a <= 0.0 {
            return Vec::new();
        }
    }
    if let (OverlayGeometry::Point(points), Some(own)) = (geometry, own.filter(|p| p.icon_image.is_none() && p.circle_radius > 0.0)) {
        return point_ring_features(points, style, own);
    }
    let stroke_parts: Vec<Vec<(f64, f64)>> = match geometry {
        OverlayGeometry::Polygon(polygons) => polygons.iter().flatten().map(|ring| closed_line(ring)).collect(),
        OverlayGeometry::Line(parts) => parts.clone(),
        // A point is highlighted as a ring of dots: its own position,
        // drawn as a round cap the outline's width.
        OverlayGeometry::Point(points) => vec![points.clone()],
        OverlayGeometry::Empty | OverlayGeometry::Indexed { .. } => return Vec::new(),
    };
    let is_point = matches!(geometry, OverlayGeometry::Point(_));
    let mut features = Vec::with_capacity(3);
    let mut push = |geometry: OverlayGeometry, paint: FeaturePaint| {
        features.push(OverlayFeature { index: features.len(), id: None, geometry, paint });
    };
    if let (Some(fill), OverlayGeometry::Polygon(_)) = (style.fill_color, geometry) {
        push(geometry.clone(), FeaturePaint { fill_color: fill, ..FeaturePaint::default() });
    }
    let stroke = |color: Rgba, width: f32| {
        if is_point {
            let points = stroke_parts.first().cloned().unwrap_or_default();
            (OverlayGeometry::Point(points), FeaturePaint { circle_color: color, circle_radius: width, ..FeaturePaint::default() })
        } else {
            (
                OverlayGeometry::Line(stroke_parts.clone()),
                FeaturePaint { line_color: color, line_width: ZoomValue::constant(width), ..FeaturePaint::default() },
            )
        }
    };
    if let Some(casing) = style.casing_color.filter(|_| style.casing_width_px > 0.0) {
        let (g, p) = stroke(casing, style.outline_width_px + 2.0 * style.casing_width_px);
        push(g, p);
    }
    if style.outline_width_px > 0.0 {
        let (g, p) = stroke(style.outline_color, style.outline_width_px);
        push(g, p);
    }
    features
}

/// One plan over `features` (in draw order), `None` for none.
fn plan_of(features: Vec<OverlayFeature>) -> Option<OverlayPlan> {
    if features.is_empty() {
        return None;
    }
    let bracket = zoom_dependence(&features).unwrap_or(BakeBracket::Bucket(0));
    build_plan(&features, &PropertyTable::default(), bracket, &Default::default())
}

/// The highlight of MANY features at once
/// ([`crate::MapView::set_highlight_where`]): each member's
/// [`highlight_plan`] parts, layered as one -- every member's fill, then
/// every casing, then every outline, then every point redrawn -- so a
/// neighbour's casing never covers a member's outline. One plan, one
/// tessellation. `None` when nothing draws.
pub fn group_highlight_plan(members: &[(OverlayGeometry, Option<FeaturePaint>)], style: &HighlightStyle) -> Option<OverlayPlan> {
    let mut layered: Vec<(usize, OverlayFeature)> = Vec::new();
    for (geometry, own) in members {
        let parts = highlight_features(geometry, style, own.as_ref());
        // A member without a fill part starts at the casing's layer, so
        // the layers line up across members and kinds.
        let offset = usize::from(!matches!(geometry, OverlayGeometry::Polygon(_)) || style.fill_color.is_none());
        layered.extend(parts.into_iter().enumerate().map(|(k, f)| (k + offset, f)));
    }
    layered.sort_by_key(|(k, _)| *k);
    let features = layered
        .into_iter()
        .enumerate()
        .map(|(i, (_, mut f))| {
            f.index = i;
            f
        })
        .collect();
    plan_of(features)
}

/// Which features [`crate::MapView::set_highlight_where`] lights: those
/// whose `field` passes `test`.
#[derive(Debug, Clone, PartialEq)]
pub struct HighlightWhere {
    pub field: String,
    pub test: WhereTest,
}

/// A [`HighlightWhere`]'s test on one property value.
#[derive(Debug, Clone, PartialEq)]
pub enum WhereTest {
    /// A number in the closed window `[lo, hi]`: a histogram bin, a
    /// brushed range.
    Range([f64; 2]),
    /// Equal to one of these: a category bar.
    OneOf(Vec<crate::mvt::TagValue>),
}

impl HighlightWhere {
    pub fn range(field: &str, range: [f64; 2]) -> Self {
        Self { field: field.to_string(), test: WhereTest::Range(range) }
    }

    pub fn one_of(field: &str, values: Vec<crate::mvt::TagValue>) -> Self {
        Self { field: field.to_string(), test: WhereTest::OneOf(values) }
    }

    /// Whether a feature whose `field` is `value` (`None`: it has none)
    /// is lit.
    pub fn matches(&self, value: Option<&crate::mvt::TagValue>) -> bool {
        let Some(value) = value else { return false };
        match (&self.test, value) {
            (WhereTest::Range([lo, hi]), crate::mvt::TagValue::Number(v)) => v >= lo && v <= hi,
            (WhereTest::Range(_), _) => false,
            (WhereTest::OneOf(values), v) => values.contains(v),
        }
    }
}

/// [`highlight_plan`] for a point drawn as a circle of `own` paint: casing
/// disc, outline disc, then the point itself, bottom to top -- a ring.
fn point_ring_features(points: &[(f64, f64)], style: &HighlightStyle, own: &FeaturePaint) -> Vec<OverlayFeature> {
    let r = own.circle_radius;
    let mut features = Vec::with_capacity(3);
    let mut push = |color: Rgba, radius: f32| {
        features.push(OverlayFeature {
            index: features.len(),
            id: None,
            geometry: OverlayGeometry::Point(points.to_vec()),
            paint: FeaturePaint { circle_color: color, circle_radius: radius, ..FeaturePaint::default() },
        });
    };
    if let Some(casing) = style.casing_color.filter(|_| style.casing_width_px > 0.0) {
        push(casing, r + style.outline_width_px + style.casing_width_px);
    }
    if style.outline_width_px > 0.0 {
        push(style.outline_color, r + style.outline_width_px);
    }
    push(own.circle_color, r);
    features
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> OverlayGeometry {
        OverlayGeometry::Polygon(vec![vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]]])
    }

    #[test]
    fn a_polygon_highlights_as_fill_casing_and_outline_in_that_order() {
        let style = HighlightStyle {
            fill_color: Some(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.2 }),
            ..HighlightStyle::default()
        };
        let plan = highlight_plan(&square(), &style, None).expect("a square draws");
        assert_eq!(plan.plan.ops.len(), 3);
        assert!(matches!(plan.plan.ops[0], crate::style::PaintOp::Fill { .. }));
        assert!(matches!(plan.plan.ops[1], crate::style::PaintOp::Line { .. }));
        assert!(matches!(plan.plan.ops[2], crate::style::PaintOp::Line { .. }));
    }

    /// A transparent hit disc (a planning pin's) gets no highlight: hovering
    /// it drew a big opaque white disc behind the pin.
    #[test]
    fn an_invisible_point_draws_no_highlight() {
        let geometry = OverlayGeometry::Point(vec![(0.5, 0.5)]);
        let hit = FeaturePaint { circle_color: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.0 }, circle_radius: 18.0, ..FeaturePaint::default() };
        assert!(highlight_plan(&geometry, &HighlightStyle::default(), Some(&hit)).is_none(), "nothing drawn for an invisible point");
        // A visible circle keeps its ring.
        let shown = FeaturePaint { circle_color: Rgba { r: 0.2, g: 0.4, b: 0.9, a: 1.0 }, circle_radius: 6.0, ..FeaturePaint::default() };
        assert!(highlight_plan(&geometry, &HighlightStyle::default(), Some(&shown)).is_some());
    }

    #[test]
    fn no_fill_and_no_casing_is_the_outline_alone() {
        let style = HighlightStyle { casing_color: None, ..HighlightStyle::default() };
        let plan = highlight_plan(&square(), &style, None).expect("a square draws");
        assert_eq!(plan.plan.ops.len(), 1);
    }

    #[test]
    fn a_closed_ring_strokes_past_its_start_so_the_corner_joins() {
        let line = closed_line(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]);
        assert_eq!(line.last(), Some(&(1.0, 0.0)));
        assert_eq!(line.len(), 5);
    }
}
