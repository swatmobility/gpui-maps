//! The point path: a big point source held as columns, indexed by
//! `cluster::ClusterIndex`, and drawn as a VIEW.
//!
//! See `docs/superpowers/specs/2026-09-26-big-point-sources-design.md`.
//!
//! A point source keeps its points here -- lon/lat, a paint index, ids,
//! the property columns -- and never as one `OverlayFeature` each. What it
//! draws is a [`PointView`]: the index's items inside a tile-aligned box
//! around the viewport at one integer zoom, as ordinary `OverlayFeature`s
//! with their own property rows, plan and hit index. The view is built off
//! the UI thread ([`build_view`] is pure) and installed into the source as
//! its content, so everything downstream -- the whole-source bake, the
//! globe, paint expressions, hit-testing, hover, labels -- is the code every
//! other source already goes through.

use std::sync::Arc;

use super::cluster::{abbreviate, lat_y, lng_x, ClusterIndex, ClusterItem, ClusterOptions};
use super::columns::{Cell, PropertyTable};
use super::geojson::{FeatureId, OverlayGeometry};
use super::paint::FeaturePaint;
use super::source_paint::{FeatureProperties, SourcePaint};
use super::{hit, plan, OverlayFeature, SourceId};
use crate::mvt::TagValue;

/// Distinct paints a point source may carry before it is refused: the
/// paint index is a `u16`, and a point source with more distinct paints
/// than this is not a big uniform point layer.
pub const MAX_POINT_PAINTS: usize = 4096;

/// The most features a non-clustered view holds before it reads a coarser
/// level of detail.
pub const MAX_VIEW_FEATURES: usize = 60_000;

/// One point source's points, as pushed.
#[derive(Debug)]
pub struct PointData {
    pub(crate) lon_lat: Vec<[f64; 2]>,
    /// `None` when no point has an id.
    pub(crate) ids: Option<Vec<Option<FeatureId>>>,
    /// The distinct paints; `paint_of[i]` indexes it (empty: all `paints[0]`).
    pub(crate) paints: Vec<FeaturePaint>,
    pub(crate) paint_of: Vec<u16>,
    pub(crate) properties: Arc<PropertyTable>,
    pub(crate) options: ClusterOptions,
    /// The source's range filter column (`overlay::filter`), when one is
    /// declared: each view's instances carry its values.
    pub(crate) filter: Option<Arc<super::filter::PointFilter>>,
    /// Moves on every push and every option change: what an index and a
    /// view are built FOR.
    pub(crate) revision: u64,
}

impl PointData {
    /// Builds the columns of a point push. `points[i]` is `(lon_lat, id,
    /// paint)`; every refusal names the point's index, before anything is
    /// kept.
    pub fn build<'p>(
        lon_lat: Vec<[f64; 2]>,
        ids: Option<Vec<Option<FeatureId>>>,
        paint_for: &'p dyn Fn(usize) -> &'p FeaturePaint,
        properties: PropertyTable,
        options: ClusterOptions,
    ) -> Result<Self, (usize, String)> {
        for (i, p) in lon_lat.iter().enumerate() {
            if !(p[0].is_finite() && p[1].is_finite()) {
                return Err((i, format!("coordinate [{}, {}] is not a finite number", p[0], p[1])));
            }
        }
        let mut paints: Vec<FeaturePaint> = Vec::new();
        let mut paint_of: Vec<u16> = Vec::with_capacity(lon_lat.len());
        let mut last: Option<u16> = None;
        for i in 0..lon_lat.len() {
            let paint = paint_for(i);
            let at = match last.filter(|&l| paints[usize::from(l)] == *paint) {
                Some(l) => l,
                None => {
                    let at = match paints.iter().position(|p| p == paint) {
                        Some(at) => at,
                        None => {
                            paint.validate().map_err(|reason| (i, reason))?;
                            if paints.len() == MAX_POINT_PAINTS {
                                return Err((
                                    i,
                                    format!("a point source holds at most {MAX_POINT_PAINTS} distinct paints"),
                                ));
                            }
                            paints.push(paint.clone());
                            paints.len() - 1
                        }
                    };
                    at as u16
                }
            };
            last = Some(at);
            paint_of.push(at);
        }
        if paints.len() <= 1 {
            paint_of = Vec::new();
        }
        if paints.is_empty() {
            paints.push(FeaturePaint::default());
        }
        let ids = ids.filter(|ids| ids.iter().any(Option::is_some));
        Ok(Self { lon_lat, ids, paints, paint_of, properties: Arc::new(properties), options, filter: None, revision: 0 })
    }

    pub fn len(&self) -> usize {
        self.lon_lat.len()
    }

    /// The push (or option change) this data belongs to: what an index is
    /// adopted against.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_empty(&self) -> bool {
        self.lon_lat.is_empty()
    }

    /// The declared range filter, if any.
    pub fn filter(&self) -> Option<&super::filter::PointFilter> {
        self.filter.as_deref()
    }

    pub(crate) fn paint(&self, i: usize) -> &FeaturePaint {
        &self.paints[self.paint_of.get(i).map_or(0, |&p| usize::from(p))]
    }

    pub(crate) fn id(&self, i: usize) -> Option<FeatureId> {
        self.ids.as_ref().and_then(|ids| ids[i].clone())
    }

    /// Resident bytes: coordinates, paint indices, ids and columns.
    pub fn bytes(&self) -> usize {
        self.lon_lat.capacity() * 16
            + self.paint_of.capacity() * 2
            + self.ids.as_ref().map_or(0, |ids| ids.capacity() * std::mem::size_of::<Option<FeatureId>>())
            + self.properties.bytes()
    }

    /// Point `i` as an ordinary feature, at index `index`: what the whole
    /// collection turns back into when the point path is switched off.
    pub(crate) fn feature(&self, i: usize, index: usize) -> OverlayFeature {
        let p = self.lon_lat[i];
        OverlayFeature {
            index,
            id: self.id(i),
            geometry: OverlayGeometry::Point(vec![(p[0], p[1])]),
            paint: self.paint(i).clone(),
        }
    }
}

/// Builds the index for `data`, with each aggregate's input column read
/// out of the property table (`NaN` where a point has no number).
pub fn build_index(data: &PointData) -> ClusterIndex {
    build_index_timed(data).0
}

/// [`build_index`] with other options, on the same points: what
/// `--measure` times the three modes with.
#[doc(hidden)]
pub fn build_index_with(data: &PointData, options: &ClusterOptions) -> (ClusterIndex, super::cluster::BuildPhases) {
    let inputs: Vec<Vec<f64>> = options
        .properties
        .iter()
        .map(|p| {
            let column = data.properties.column(&p.column);
            (0..data.len())
                .map(|row| match column.and_then(|c| data.properties.cell(c, row)) {
                    Some(Cell::Number(v)) => v,
                    _ => f64::NAN,
                })
                .collect()
        })
        .collect();
    ClusterIndex::build_timed(&data.lon_lat, options, &inputs, options.cluster)
}

/// [`build_index`], with the build's wall time per phase (the aggregate
/// columns' read is counted in `project_ms`).
pub fn build_index_timed(data: &PointData) -> (ClusterIndex, super::cluster::BuildPhases) {
    let started = crate::timing::Instant::now();
    let inputs: Vec<Vec<f64>> = data
        .options
        .properties
        .iter()
        .map(|p| {
            let column = data.properties.column(&p.column);
            (0..data.len())
                .map(|row| match column.and_then(|c| data.properties.cell(c, row)) {
                    Some(Cell::Number(v)) => v,
                    _ => f64::NAN,
                })
                .collect()
        })
        .collect();
    let read_ms = started.elapsed().as_secs_f64() * 1e3;
    let (index, mut phases) = ClusterIndex::build_timed(&data.lon_lat, &data.options, &inputs, data.options.cluster);
    phases.project_ms += read_ms;
    (index, phases)
}

/// Which box a view covers: the tile range `x0..=x1`, `y0..=y1` of zoom
/// `z`, in 512 px tiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewKey {
    pub z: u8,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl ViewKey {
    /// The tiles of zoom `z` touching the Mercator box (`[0, 1]`, y down),
    /// grown by `margin` tiles on every side.
    pub fn for_box(z: u8, min: [f64; 2], max: [f64; 2], margin: u32) -> Self {
        let n = 1u64 << z.min(31);
        let last = (n - 1) as u32;
        let tile = |v: f64| ((v.clamp(0.0, 1.0) * n as f64).floor() as u64).min(n - 1) as u32;
        Self {
            z,
            x0: tile(min[0]).saturating_sub(margin),
            y0: tile(min[1]).saturating_sub(margin),
            x1: tile(max[0]).saturating_add(margin).min(last),
            y1: tile(max[1]).saturating_add(margin).min(last),
        }
    }

    /// Whether this view already covers `inner`: same zoom, and the box
    /// inside this one.
    pub fn covers(&self, inner: &Self) -> bool {
        self.z == inner.z && self.x0 <= inner.x0 && self.y0 <= inner.y0 && self.x1 >= inner.x1 && self.y1 >= inner.y1
    }

    /// The Mercator box this key covers.
    pub fn mercator_box(&self) -> ([f64; 2], [f64; 2]) {
        let n = (1u64 << self.z.min(31)) as f64;
        (
            [f64::from(self.x0) / n, f64::from(self.y0) / n],
            [f64::from(self.x1 + 1) / n, f64::from(self.y1 + 1) / n],
        )
    }
}

/// A compiled source paint, for a caller outside the crate that builds a
/// view itself (`demos/big_points.rs --measure`).
#[doc(hidden)]
pub struct SourcePaintHandle(SourcePaint);

impl SourcePaintHandle {
    pub fn compile(paint: &serde_json::Map<String, serde_json::Value>) -> Result<Self, String> {
        SourcePaint::compile(paint).map(Self)
    }

    pub fn paint(&self) -> &SourcePaint {
        &self.0
    }
}

/// A view, built and ready to install.
#[derive(Debug)]
pub struct PointView {
    pub key: ViewKey,
    pub revision: u64,
    pub(crate) features: Vec<OverlayFeature>,
    pub(crate) properties: PropertyTable,
    pub(crate) plan: Option<plan::OverlayPlan>,
    pub(crate) hit_index: hit::HitIndex,
    /// How many of `features` are clusters.
    pub clusters: usize,
    /// The view's dots as instanced circles, when every op of its plan is a
    /// dot (then `plan` is `None`: nothing is tessellated).
    pub circles: Option<Arc<PointCircles>>,
    /// A filtered source's pushed row behind each feature (`None`: a
    /// cluster); empty when the data has no filter.
    pub(crate) rows: Vec<Option<usize>>,
}

/// A view's instanced circles, in the lyon units of `origin`.
#[derive(Debug)]
pub struct PointCircles {
    pub key: ViewKey,
    /// The data revision the circles were built from.
    pub revision: u64,
    pub origin: super::geom::OverlayOrigin,
    pub bracket: plan::BakeBracket,
    pub set: crate::render::circles::CircleSet,
    /// `Some(base)` when every instance carries its row's filter value
    /// relative to `base` (`overlay::filter`); a draw applies the source's
    /// window only then.
    pub filter_base: Option<f64>,
}

/// Writes each instance's filter value. `split_dots` emits instances in
/// op order, one per point of each dot op; an op holds the dots of the
/// features whose `feature_ops` range names it, in feature order -- one
/// feature per op on the per-feature path, many on the merged one (a
/// data-driven colour merges every feature of one colour into one op). So
/// walking each op's features in order meets the instances in order. A
/// cluster (no single row) gets NaN: outside every window. `None` (values
/// left unread) if any op's dot count disagrees with its features'.
fn fill_filter_values(
    set: &mut crate::render::circles::CircleSet,
    plan: &plan::OverlayPlan,
    features: &[OverlayFeature],
    leaf_of: &[Option<usize>],
    filter: &super::filter::PointFilter,
) -> Option<f64> {
    let ops = &plan.plan.ops;
    let mut per_op: Vec<Vec<(f32, usize)>> = vec![Vec::new(); ops.len()];
    for (f, range) in plan.feature_ops.iter().enumerate() {
        let value = leaf_of.get(f).copied().flatten().map_or(f32::NAN, |leaf| filter.value(leaf));
        let n = match &features.get(f)?.geometry {
            OverlayGeometry::Point(p) => p.len(),
            _ => 0,
        };
        for k in range.clone() {
            per_op.get_mut(k)?.push((value, n));
        }
    }
    let mut at = 0usize;
    for (op, owners) in ops.iter().zip(&per_op) {
        let crate::style::PaintOp::Line { geometry, .. } = op else { return None };
        let dots = match &**geometry {
            geo_types::Geometry::Point(_) => 1,
            geo_types::Geometry::MultiPoint(mp) => mp.0.len(),
            _ => return None,
        };
        if owners.iter().map(|o| o.1).sum::<usize>() != dots {
            return None;
        }
        for &(value, n) in owners {
            for instance in set.instances.get_mut(at..at + n)? {
                instance.filter = value;
            }
            at += n;
        }
    }
    (at == set.instances.len()).then_some(filter.base)
}

impl PointView {
    pub fn len(&self) -> usize {
        self.features.len()
    }

    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// The plan's op count and dot count, for reports.
    pub fn plan_ops(&self) -> usize {
        self.plan.as_ref().map_or(0, |p| p.plan.ops.len())
    }
}

/// A cluster's properties, as MapLibre gives them.
pub(crate) fn cluster_properties(index: &ClusterIndex, id: u32, count: u32, agg: u32) -> FeatureProperties {
    let mut map = FeatureProperties::new();
    map.insert("cluster".into(), TagValue::Bool(true));
    map.insert("cluster_id".into(), TagValue::Number(f64::from(id)));
    map.insert("point_count".into(), TagValue::Number(f64::from(count)));
    map.insert("point_count_abbreviated".into(), TagValue::String(abbreviate(count)));
    for (p, value) in index.options().properties.iter().zip(index.aggregates(agg)) {
        if !value.is_nan() {
            map.insert(p.name.clone(), TagValue::Number(*value));
        }
    }
    map
}

/// Legacy `{token}` substitution in a `text-field`, from the feature's own
/// properties -- how a cluster labels itself with
/// `"{point_count_abbreviated}"`. A token with no value becomes empty.
fn substitute_tokens(text: &str, row: &FeatureProperties) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let key = &rest[open + 1..open + close];
        match row.get(key) {
            Some(TagValue::String(s)) => out.push_str(s),
            Some(TagValue::Number(n)) => out.push_str(&format_number(*n)),
            Some(TagValue::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
            None => {}
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Builds the view for `key`: the index's items in the key's box (padded
/// by a tenth of a tile, so a cluster whose centre sits just outside still
/// draws its edge), each as a feature with its own property row, then the
/// plan and the hit index. Pure; runs on a worker.
pub fn build_view(
    name: &SourceId,
    data: &PointData,
    index: &ClusterIndex,
    paint: &SourcePaint,
    key: ViewKey,
) -> PointView {
    let (min, max) = key.mercator_box();
    let pad = 0.1 / (1u64 << key.z.min(31)) as f64;
    // The non-clustered level of detail keeps a view under
    // `MAX_VIEW_FEATURES` by reading a coarser level when a dense box would
    // exceed it. Dots are instanced now (16 bytes each), so the cap bounds
    // the view's build time, rows and hit index rather than a mesh.
    //
    // A FILTERED non-clustered source starts at the raw level instead: the
    // level of detail draws a cell's points as one representative, and a
    // time window must show or hide each point by its own value. Exact up
    // to `MAX_VIEW_FEATURES` points in the box; past that, the coarser
    // levels' representatives (an approximation the cap already makes).
    let mut query_zoom = if data.filter.is_some() && !data.options.cluster { index.raw_zoom().max(key.z) } else { key.z };
    let (z, items) = loop {
        let (z, items) =
            index.range_items(f64::from(query_zoom), min[0] - pad, min[1] - pad, max[0] + pad, max[1] + pad);
        if data.options.cluster || items.len() <= MAX_VIEW_FEATURES || query_zoom == 0 {
            break (z, items);
        }
        query_zoom -= 1;
    };
    let mut features = Vec::with_capacity(items.len());
    let mut rows: Vec<FeatureProperties> = Vec::with_capacity(items.len());
    let mut clusters = 0;
    // The pushed row behind each feature (`None`: a cluster), for the
    // filter values the instances carry.
    let mut leaf_of: Vec<Option<usize>> = Vec::with_capacity(if data.filter.is_some() { items.len() } else { 0 });
    let has_text = data.paints.iter().any(|p| p.text_field.as_deref().is_some_and(|t| t.contains('{')));
    for i in items {
        let at = features.len();
        let (feature, row, leaf) = match index.item(z, i) {
            ClusterItem::Cluster { id, lon_lat, count, agg } => {
                clusters += 1;
                let row = cluster_properties(index, id, count, agg);
                let feature = OverlayFeature {
                    index: at,
                    id: Some(FeatureId::Number(f64::from(id))),
                    geometry: OverlayGeometry::Point(vec![(lon_lat[0], lon_lat[1])]),
                    paint: data.paints[0].clone(),
                };
                (feature, row, None)
            }
            ClusterItem::Leaf { index: leaf } => {
                let leaf = leaf as usize;
                (data.feature(leaf, at), data.properties.row_map(leaf), Some(leaf))
            }
        };
        if data.filter.is_some() {
            leaf_of.push(leaf);
        }
        features.push(feature);
        rows.push(row);
    }
    // Per-feature paint that reads the row: a data-driven `circle-radius`,
    // and `{token}`s in a `text-field`.
    if paint.circle_radius_declared() || has_text {
        for (feature, row) in features.iter_mut().zip(&rows) {
            if let Some(r) = paint.circle_radius(row) {
                feature.paint.circle_radius = r;
            }
            if let Some(text) = feature.paint.text_field.as_deref().filter(|t| t.contains('{')) {
                // A label that resolves to nothing is no label (MapLibre's
                // rule, `symbol_pass::overlay_label_text`): `None`, so the
                // symbol pass skips the feature before projecting it -- a
                // cluster label's `{point_count_abbreviated}` is empty on
                // every single point, up to `MAX_VIEW_FEATURES` of them.
                let text = substitute_tokens(text, row);
                feature.paint.text_field = (!text.is_empty()).then_some(text);
            }
        }
    }
    let properties = PropertyTable::from_maps(rows.iter());
    drop(rows);
    let bracket = plan::zoom_dependence(&features).unwrap_or(plan::BakeBracket::Bucket(key.z));
    let mut plan = super::source_plan_for(&features, &properties, bracket, paint, false);
    // Every op of a point view is a dot (`circle-radius` is zoom-constant),
    // so the whole plan becomes instances and nothing is tessellated. A plan
    // with any other op keeps the tessellated path whole.
    let mut circles = None;
    if let Some(p) = &plan {
        let (mut set, rest) = crate::render::circles::split_dots(&p.plan, p.origin.coord_scale);
        set.set_pitch(paint.circle_pitch());
        if rest.is_empty() && !set.is_empty() {
            let filter_base = data.filter.as_deref().and_then(|f| fill_filter_values(&mut set, p, &features, &leaf_of, f));
            circles = Some(Arc::new(PointCircles {
                key,
                revision: data.revision,
                origin: p.origin,
                bracket: p.bracket,
                set,
                filter_base,
            }));
            plan = None;
        }
    }
    let hit_index = hit::HitIndex::build(name, &features);
    PointView { key, revision: data.revision, features, properties, plan, hit_index, clusters, circles, rows: leaf_of }
}

/// What a view costs the GPU: `(circle instances, upload bytes)` -- the
/// instances and their paint table, or, for a view that fell back to
/// tessellation, the mesh `probe_tessellate` builds. For `--measure`.
#[doc(hidden)]
pub fn probe_upload(view: &PointView) -> (usize, usize) {
    if let Some(c) = &view.circles {
        return (c.set.len(), c.set.upload_bytes());
    }
    let (vertices, indices) = probe_tessellate(view);
    (0, vertices * std::mem::size_of::<crate::render::Vertex>() + indices * 4)
}

/// Tessellates a view's plan the way `MapView`'s whole-source bake does:
/// `(vertices, indices)`. For `--measure`.
#[doc(hidden)]
pub fn probe_tessellate(view: &PointView) -> (usize, usize) {
    let Some(plan) = &view.plan else {
        // Instanced: four corners and two triangles an instance, drawn from
        // one 16-byte instance.
        return view.circles.as_ref().map_or((0, 0), |c| (c.set.len() * 4, c.set.len() * 6));
    };
    match crate::render::mesh::tessellate_overlay_plan_reusing(
        &plan.plan,
        &plan.fill_outlines,
        crate::render::subdivide::OVERLAY_SUBDIVISION_EXTENT,
        plan.origin.coord_scale,
        crate::render::subdivide::Subdivision::OVERLAY,
        &|| false,
        crate::render::TileMesh::default(),
    ) {
        Ok((mesh, _, _)) => (mesh.vertices.len(), mesh.indices.len()),
        Err(_) => (0, 0),
    }
}

/// The Mercator box a camera shows, from its centre and zoom, for a
/// `width` x `height` px viewport, looking straight down. A pitched camera
/// sees more; `MapView` unprojects its corners instead.
pub fn flat_viewport_box(lon: f64, lat: f64, zoom: f64, width: f64, height: f64) -> ([f64; 2], [f64; 2]) {
    let world = 512.0 * 2f64.powf(zoom);
    let (cx, cy) = (lng_x(lon), lat_y(lat));
    let (hw, hh) = (width / 2.0 / world, height / 2.0 / world);
    ([cx - hw, cy - hh], [cx + hw, cy + hh])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(points: Vec<[f64; 2]>, options: ClusterOptions) -> PointData {
        let paint = FeaturePaint { circle_radius: 3.0, ..FeaturePaint::default() };
        let n = points.len();
        let values: Vec<Option<f64>> = (0..n).map(|i| Some(i as f64)).collect();
        let table = PropertyTable::from_columns(n, &[("v", crate::overlay::PropertyColumn::Number(&values))]).unwrap();
        PointData::build(points, None, &|_| &paint, table, options).unwrap()
    }

    fn grid(n: usize) -> Vec<[f64; 2]> {
        (0..n).map(|i| [-74.0 + (i % 100) as f64 * 1e-3, 40.7 + (i / 100) as f64 * 1e-3]).collect()
    }

    /// A declared filter column rides on the instances: instance `k` is
    /// feature `k`'s single dot, and carries that point's value relative to
    /// the column's minimum; a window passes exactly the points inside it.
    #[test]
    fn a_filtered_view_carries_each_points_value_on_its_instance() {
        let mut d = data(grid(5000), ClusterOptions::points_only());
        let filter = crate::overlay::filter::PointFilter::read(&d.properties, d.len(), "v").expect("a column");
        assert_eq!(filter.domain, [0.0, 4999.0]);
        d.filter = Some(Arc::new(filter.clone()));
        let index = build_index(&d);
        let (min, max) = flat_viewport_box(-73.95, 40.725, 18.0, 1280.0, 800.0);
        let view = build_view(&"p".into(), &d, &index, &SourcePaint::default(), ViewKey::for_box(18, min, max, 1));
        let circles = view.circles.as_ref().expect("instanced");
        assert_eq!(circles.filter_base, Some(0.0));
        assert_eq!(circles.set.instances.len(), view.features.len());
        assert!(!view.features.is_empty());
        let row_of = |f: &OverlayFeature| match &f.geometry {
            OverlayGeometry::Point(p) => {
                let (lon, lat) = p[0];
                ((lon + 74.0) / 1e-3).round() as usize + 100 * ((lat - 40.7) / 1e-3).round() as usize
            }
            _ => unreachable!(),
        };
        let mut rows: Vec<usize> = view.features.iter().map(row_of).collect();
        rows.sort_unstable();
        let (lo, hi) = (rows[rows.len() / 4], rows[rows.len() * 3 / 4]);
        let window = [lo as f64, hi as f64];
        let (mut inside, mut outside) = (0, 0);
        for (instance, feature) in circles.set.instances.iter().zip(&view.features) {
            let row = row_of(feature);
            assert_eq!(instance.filter, row as f32, "instance and row disagree");
            let pass = filter.passes(row, window);
            assert_eq!(pass, (lo..=hi).contains(&row));
            if pass { inside += 1 } else { outside += 1 }
        }
        assert!(inside > 0 && outside > 0, "{inside} {outside}");
        // At a low zoom the level of detail would merge nearby points into
        // one representative; filtered, every point in the box is its own
        // instance, so a window shows or hides each by its own value.
        let (lmin, lmax) = flat_viewport_box(-73.95, 40.725, 9.0, 1280.0, 800.0);
        let low = build_view(&"p".into(), &d, &index, &SourcePaint::default(), ViewKey::for_box(9, lmin, lmax, 1));
        assert_eq!(low.features.len(), 5000, "a filtered view reads every point");
        let unfiltered = data(grid(5000), ClusterOptions::points_only());
        let uindex = build_index(&unfiltered);
        let merged = build_view(&"p".into(), &unfiltered, &uindex, &SourcePaint::default(), ViewKey::for_box(9, lmin, lmax, 1));
        assert!(merged.features.len() < 5000, "the level of detail merges: {}", merged.features.len());
        // A data-driven colour merges every feature of one colour into one
        // op: the values still land on their own instances.
        let paint_json: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r##"{"circle-color":["step",["get","v"],"#ff0000",2500,"#0000ff"]}"##).unwrap();
        let paint = SourcePaint::compile(&paint_json).expect("compiles");
        let merged_view = build_view(&"p".into(), &d, &index, &paint, ViewKey::for_box(9, lmin, lmax, 1));
        let circles = merged_view.circles.as_ref().expect("instanced");
        assert!(circles.set.paints.len() == 2, "two colours: {}", circles.set.paints.len());
        assert_eq!(circles.filter_base, Some(0.0), "the merged plan's values were not mapped");
        let mut seen: Vec<u32> = circles.set.instances.iter().map(|i| i.filter as u32).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..5000).collect::<Vec<u32>>());
        for i in &circles.set.instances {
            let red = circles.set.paints[(i.paint & crate::render::circles::PAINT_INDEX_MASK) as usize].0.r > 0.5;
            assert_eq!(red, i.filter < 2500.0, "value {} on the wrong colour's dot", i.filter);
        }
        // Unfiltered data: no values, so a draw never applies a window.
        let plain = data(grid(5000), ClusterOptions::points_only());
        let index = build_index(&plain);
        let view = build_view(&"p".into(), &plain, &index, &SourcePaint::default(), ViewKey::for_box(18, min, max, 1));
        assert_eq!(view.circles.as_ref().map(|c| c.filter_base), Some(None));
    }

    #[test]
    fn a_high_zoom_view_holds_every_point_in_its_box_exactly() {
        let d = data(grid(5000), ClusterOptions::points_only());
        let index = build_index(&d);
        let (min, max) = flat_viewport_box(-73.95, 40.725, 18.0, 1280.0, 800.0);
        let key = ViewKey::for_box(18, min, max, 1);
        let view = build_view(&"p".into(), &d, &index, &SourcePaint::default(), key);
        let (bmin, bmax) = key.mercator_box();
        let want = d
            .lon_lat
            .iter()
            .filter(|p| {
                let (x, y) = (lng_x(p[0]), lat_y(p[1]));
                x >= bmin[0] && x <= bmax[0] && y >= bmin[1] && y <= bmax[1]
            })
            .count();
        assert!(want > 0);
        assert!(view.len() >= want && view.len() <= want + 200, "{} vs {want}", view.len());
        assert_eq!(view.clusters, 0);
        // Exact positions: every view point is an input point.
        for f in &view.features {
            let OverlayGeometry::Point(p) = &f.geometry else { panic!() };
            assert!(d.lon_lat.iter().any(|q| q[0] == p[0].0 && q[1] == p[0].1));
        }
    }

    #[test]
    fn a_low_zoom_clustered_view_carries_cluster_properties_that_sum_to_the_total() {
        let options = ClusterOptions {
            properties: vec![super::super::cluster::ClusterProperty::sum("total", "v")],
            ..ClusterOptions::default()
        };
        let d = data(grid(5000), options);
        let index = build_index(&d);
        let key = ViewKey { z: 3, x0: 0, y0: 0, x1: 7, y1: 7 };
        let view = build_view(&"p".into(), &d, &index, &SourcePaint::default(), key);
        assert!(view.clusters >= 1);
        let mut count = 0.0;
        let mut total = 0.0;
        for (i, f) in view.features.iter().enumerate() {
            let row = view.properties.row_map(i);
            match row.get("point_count") {
                Some(TagValue::Number(c)) => {
                    count += c;
                    assert_eq!(row.get("cluster"), Some(&TagValue::Bool(true)));
                    assert!(matches!(f.id, Some(FeatureId::Number(_))));
                    let Some(TagValue::Number(t)) = row.get("total") else { panic!("aggregate missing") };
                    total += t;
                }
                _ => {
                    count += 1.0;
                    let Some(TagValue::Number(v)) = row.get("v") else { panic!() };
                    total += v;
                }
            }
        }
        assert_eq!(count, 5000.0);
        assert_eq!(total, (0..5000).map(f64::from).sum::<f64>());
    }

    /// The store's door: a declared source takes its pushes down the point
    /// path, refuses what is not a single point, answers the cluster
    /// queries once indexed, and goes back to ordinary features when the
    /// declaration is withdrawn.
    #[test]
    fn a_declared_source_routes_pushes_to_the_point_path_and_back() {
        use crate::overlay::{FeatureInput, GeometryInput, OverlayStore};
        let mut store = OverlayStore::new();
        store.set_source_cluster_options("p", Some(ClusterOptions::default()), 0).unwrap();
        let positions: Vec<[f64; 2]> = grid(3000);
        let singles: Vec<[[f64; 2]; 1]> = positions.iter().map(|p| [*p]).collect();
        let paint = FeaturePaint { circle_radius: 3.0, ..FeaturePaint::default() };
        let inputs: Vec<FeatureInput<'_>> = singles
            .iter()
            .enumerate()
            .map(|(i, p)| FeatureInput {
                id: Some(FeatureId::Number(i as f64)),
                geometry: GeometryInput::Point(p),
                paint: paint.clone(),
            })
            .collect();
        store.set_source_from("p", &inputs, 0).unwrap();
        assert!(store.source_features("p").unwrap().is_empty(), "nothing drawn before the first view");
        assert_eq!(store.source_point_data("p").unwrap().len(), 3000);

        // A line in a point source is refused, naming it, and changes nothing.
        let line = [[0.0, 0.0], [1.0, 1.0]];
        let parts: [&[[f64; 2]]; 1] = [&line];
        let bad = [FeatureInput { id: None, geometry: GeometryInput::Line(&parts), paint: paint.clone() }];
        assert!(store.set_source_from("p", &bad, 0).is_err());
        assert_eq!(store.source_point_data("p").unwrap().len(), 3000);

        let data = store.source_point_data("p").unwrap();
        let index = Arc::new(build_index(&data));
        assert!(store.adopt_point_index("p", data.revision(), Arc::clone(&index)));
        let key = ViewKey { z: 2, x0: 0, y0: 0, x1: 3, y1: 3 };
        let (name, data, index, paint) = store.point_view_job("p").unwrap();
        let view = build_view(&name, &data, &index, &paint, key);
        assert!(store.install_point_view("p", view));
        let features = store.source_features("p").unwrap();
        assert!(!features.is_empty());
        let Some(FeatureId::Number(id)) = features[0].id.clone() else { panic!("a cluster's id is its cluster_id") };
        let expansion = store.cluster_expansion_zoom("p", id as u32).unwrap();
        assert!(expansion > 2);
        let leaves = store.cluster_leaves("p", id as u32, usize::MAX, 0).unwrap();
        let count = store.feature_properties("p", 0).unwrap();
        let Some(TagValue::Number(n)) = count.get("point_count") else { panic!("point_count") };
        assert_eq!(leaves.len(), *n as usize);
        assert!(leaves.iter().all(|l| l.id == Some(FeatureId::Number(l.index as f64))), "leaves keep their ids");

        // Withdrawn: the points come back as ordinary features.
        store.set_source_cluster_options("p", None, 0).unwrap();
        assert_eq!(store.source_features("p").unwrap().len(), 3000);
        assert!(store.source_point_data("p").is_none());
    }

    #[test]
    fn tokens_substitute_from_the_row() {
        let mut row = FeatureProperties::new();
        row.insert("point_count_abbreviated".into(), TagValue::String("1.2k".into()));
        row.insert("n".into(), TagValue::Number(3.0));
        assert_eq!(substitute_tokens("{point_count_abbreviated}", &row), "1.2k");
        assert_eq!(substitute_tokens("a{n}b{missing}c", &row), "a3bc");
    }

    /// A cluster-count label on the source's paint: a cluster says its
    /// count, and a single point -- whose token is empty -- carries no
    /// label at all (`None`, not `Some("")`), so the symbol pass skips it
    /// before projecting it.
    #[test]
    fn a_cluster_label_resolves_and_a_single_points_is_none() {
        let paint = FeaturePaint { circle_radius: 3.0, text_field: Some("{point_count_abbreviated}".into()), ..FeaturePaint::default() };
        let points = grid(3000);
        let n = points.len();
        let table = PropertyTable::from_columns(n, &[]).unwrap();
        let d = PointData::build(points, None, &|_| &paint, table, ClusterOptions::default()).unwrap();
        let index = build_index(&d);
        let low = build_view(&"p".into(), &d, &index, &SourcePaint::default(), ViewKey { z: 2, x0: 0, y0: 0, x1: 3, y1: 3 });
        assert!(low.clusters > 0);
        assert!(low.features.iter().all(|f| f.paint.text_field.as_deref().is_some_and(|t| !t.is_empty() && !t.contains('{'))), "every cluster labelled");
        let (min, max) = flat_viewport_box(-73.95, 40.725, 18.0, 1280.0, 800.0);
        let high = build_view(&"p".into(), &d, &index, &SourcePaint::default(), ViewKey::for_box(18, min, max, 1));
        assert!(high.clusters == 0 && !high.features.is_empty());
        assert!(high.features.iter().all(|f| f.paint.text_field.is_none()), "a single point has no label");
    }

    #[test]
    fn keys_cover_and_clamp() {
        let k = ViewKey::for_box(2, [0.0, 0.0], [0.3, 0.3], 1);
        assert_eq!((k.x0, k.y0, k.x1, k.y1), (0, 0, 2, 2));
        let inner = ViewKey::for_box(2, [0.26, 0.26], [0.3, 0.3], 0);
        assert!(k.covers(&inner));
        assert!(!inner.covers(&k));
    }
}
