//! Overlay features as an ordinary `style::PaintPlan`.
//!
//! Producing the same structure the tile path produces is the point: the
//! overlay then tessellates through `render::mesh::tessellate_plan`, gets
//! fills, strokes, dashes and line distance from code that is already
//! tested, and needs no parallel tessellator of its own.

use std::sync::Arc;

use crate::overlay::geojson::{OverlayGeometry, TileKind};
use crate::overlay::geom::{lon_lat_to_world, OverlayOrigin};
use crate::overlay::columns::PropertyTable;
use crate::overlay::source_paint::{ColorSlot, LineGradient, SourcePaint};
use crate::overlay::OverlayFeature;
use crate::style::{DashPattern, PaintOp, PaintPlan, Rgba};

/// Which two zooms a plan's zoom-varying paint was resolved at, and
/// therefore how the GPU must weight them.
///
/// The shader always does the same thing -- `mix(value_from, value_to,
/// blend_t)`. What differs is what the two baked values MEAN, and this type
/// is what keeps the bake and the weight from drifting apart: both come out
/// of the same value, so a plan cannot be blended by a weight that does not
/// belong to it.
///
/// # Why two shapes
///
/// `Bucket` is what tiles use and what the overlay used to use
/// unconditionally. It is fully general -- it can resolve any expression
/// shape, including `step` and multi-stop ramps -- but its pair is only
/// correct for one integer zoom, so crossing a bucket boundary means
/// re-resolving the plan and rewriting every vertex's paint. For this
/// crate's own fleet dataset that measured **~55 ms of UI thread per
/// crossing**: 22 ms to rebuild the plan (which re-projects all 414k input
/// points), 4 ms to rewrite 2.55M vertices, and 29 ms to re-upload the
/// 204 MB vertex buffer. A three-to-four-frame stall at every integer
/// zoom.
///
/// `Ramp` removes all three costs by baking at the EXPRESSION'S OWN stop
/// zooms rather than at the camera's current bucket. `["interpolate",
/// ["linear"], ["zoom"], 8, 2.4, 18, 7.2]` is baked once as
/// `(2.4, 7.2)` and blended by `clamp((zoom - 8) / 10, 0, 1)`, which
/// reproduces `value_at(z)` exactly at every zoom -- so the pair never goes
/// stale, and no zoom change ever touches a vertex.
///
/// This is how MapLibre avoids the same cost, arrived at from the same
/// constraint. Its `CompositeExpressionBinder::setUniform` bakes a paint
/// attribute's pair once per bucket and then changes only a uniform:
/// `uniform.set(clamp(expression.interpolationFactor(zoom, this.zoom,
/// this.zoom + 1), 0, 1))`. It can keep a per-bucket bracket where we
/// cannot because ITS buckets are tiles: crossing a zoom level loads new
/// tiles, which were going to be baked at that zoom anyway, so the re-bake
/// is free. An overlay is not tiled and has no such boundary, so it takes
/// the bracket from the expression instead of from the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BakeBracket {
    /// Resolved at `(z, z+1)`, so correct only while `z` is the display
    /// bucket. `MapView::refresh_overlay_bake_zoom` re-resolves it on every
    /// crossing.
    Bucket(u8),
    /// Resolved at a ramp's own two stop zooms, so correct at EVERY zoom
    /// and never re-resolved. Chosen by `shared_width_ramp`, which only
    /// returns one when every feature agrees the shape is exact.
    Ramp { lo: u8, hi: u8 },
}

impl BakeBracket {
    /// The weight `shader.wgsl` must blend this plan's baked pair with.
    ///
    /// For `Bucket` this is `fract(camera.zoom)`, the same weight tiles
    /// use, valid only because `refresh_overlay_bake_zoom` has already
    /// guaranteed the bucket is current.
    ///
    /// For `Ramp` it is the position of the camera within the ramp,
    /// clamped -- the clamp is not a safety net but the ramp's own
    /// behaviour outside its stops, where `ZoomValue::value_at_linear`
    /// returns the first or last stop's value rather than extrapolating.
    /// Without it a route would keep thinning below z8 and keep thickening
    /// above z18.
    pub fn blend_t(self, camera_zoom: f64) -> f32 {
        match self {
            Self::Bucket(z) => (camera_zoom - z as f64).clamp(0.0, 1.0) as f32,
            Self::Ramp { lo, hi } => {
                debug_assert!(lo < hi, "a Ramp's span must be positive: {lo}..{hi}");
                let span = (hi as f64 - lo as f64).max(1.0);
                ((camera_zoom - lo as f64) / span).clamp(0.0, 1.0) as f32
            }
        }
    }

    /// Resolves one zoom-varying value into the pair this bracket describes.
    ///
    /// `Bucket` goes through `value_at_pair` rather than resolving the two
    /// zooms itself, because that function deliberately collapses a `step`
    /// to a single value in both brackets -- a step must never be blended
    /// into a ramp. `Ramp` may resolve its two zooms directly: it is only
    /// ever chosen for values that are constant or exactly-linear over
    /// `lo..hi`, where the two are the same thing.
    fn resolve<T: Clone>(self, value: &crate::style::ZoomValue<T>) -> (T, T) {
        match self {
            Self::Bucket(z) => value.value_at_pair(z),
            Self::Ramp { lo, hi } => (value.value_at(lo), value.value_at(hi)),
        }
    }
}

/// The one zoom bracket every feature's `line-width` can be baked at once
/// and blended from forever, when the collection admits one.
///
/// Returns `None` -- meaning "fall back to `Bucket`, and pay a rewrite per
/// crossing" -- as soon as any width has a shape a fixed bracket cannot
/// reproduce: a `step`, a ramp of three or more stops, or a ramp whose
/// stops disagree with another feature's. The disagreement case is not a
/// limitation of the expression but of the draw: one SOURCE is one draw
/// call, so there is exactly one `blend_t` uniform for all of it, and two
/// features ramping over different zoom ranges cannot share one weight.
///
/// SCOPED TO ONE SOURCE, which is the whole reason sources exist for this
/// predicate. It used to be asked of the entire overlay, so ONE vehicle
/// feature with a `step` width dragged five thousand stops onto the
/// `Bucket` path with it, and every integer zoom then re-resolved and
/// rewrote all of them. Asked per source, the stops keep their `Ramp` and
/// the vehicles keep their `step`, independently.
///
/// Constants are ignored rather than counted, because a constant resolves
/// to the same value at every zoom and so is exact under any bracket at
/// all. A collection of nothing but constants therefore gets an arbitrary
/// bracket -- and correctly never re-bakes, which is a real fix on its own:
/// before this, an overlay with entirely constant paint still paid the full
/// 55 ms at every integer zoom, rewriting 2.55M vertices with the values
/// they already held.
pub fn shared_width_ramp(features: &[OverlayFeature]) -> Option<BakeBracket> {
    zoom_dependence(features).ok()
}

/// `shared_width_ramp`, but an answer of "no fixed bracket" carries WHICH
/// feature denied one and why.
///
/// The same walk, and deliberately the same walk: this is what
/// `OverlayStore` checks a caller's `SourceOptions::zoom_independent`
/// declaration against, and a second implementation of the predicate --
/// one to bake by and one to validate by -- is exactly how a source comes
/// to be declared zoom-independent and re-baked anyway. `shared_width_ramp`
/// is now a thin `.ok()` over this, so there is one predicate.
///
/// # What "zoom-independent" honestly means
///
/// Not that the source's geometry avoids zoom -- no overlay geometry has
/// ever depended on zoom, since `build_plan` projects lon/lat through
/// `lon_lat_to_world` and nothing else. What depends on zoom is the PAINT
/// baked into those vertices, and the only zoom-varying paint an overlay
/// feature has is `line-width` (`FeaturePaint`'s one `ZoomValue`;
/// `circle-radius` is a plain `f32`). So the question is entirely: can
/// every width in this source be baked once as a pair and recovered
/// exactly at every zoom by `BakeBracket::blend_t`?
///
/// `Ramp` says yes, and that answer is exact rather than approximate:
/// `value_at_linear` over two stops IS a linear interpolation, which is
/// what the shader's `mix` computes. `Bucket` says no, and a `Bucket`
/// source is re-resolved and rewritten at every integer zoom by
/// `MapView::refresh_overlay_bake_zoom`.
///
/// It fails CLOSED in both directions that matter. A width shape this walk
/// does not recognise (a `step`, a three-stop ramp) is refused rather than
/// approximated, and two features that disagree are refused rather than
/// having one of them silently win. A source is only ever called
/// zoom-independent when every width in it is either constant or an
/// exactly-linear two-stop ramp over the same two zooms.
///
/// `min_zoom` is deliberately not part of this. It makes a feature's LABEL
/// appear and disappear with zoom, but it is tested per frame in the
/// symbol pass and never baked into a vertex, so it costs a crossing
/// nothing and cannot make a mesh stale.
///
/// A DATA-DRIVEN COLOUR is deliberately not part of this either, and that
/// is a checked claim rather than an omission. `overlay::source_paint`
/// refuses a colour expression whose value varies with zoom, and refuses one
/// whose branch filters do (`filter_is_zoom_free`), at the moment the
/// declaration is made. So a data-driven colour resolves to the same `Rgba`
/// at every zoom, is baked into `color_from`/`color_to` identically, and
/// cannot make a source that this walk calls zoom-independent stop being it.
/// Had colour been allowed a zoom axis it would have had to join the ramp
/// agreement below, and a `zoom_independent` source could have acquired a
/// colour ramp and quietly started re-resolving at every integer zoom.
pub fn zoom_dependence(features: &[OverlayFeature]) -> Result<BakeBracket, ZoomDependence> {
    zoom_dependence_of(features.iter().map(|feature| (feature.index, &feature.paint.line_width)))
}

/// `zoom_dependence` over the widths alone, each paired with the feature
/// index to name it by.
///
/// THE predicate. Both public forms are wrappers, which is the point: the
/// bracket a source bakes at, the bracket `plan_paint_update` asks about a
/// collection it has not written yet, and the check a caller's
/// `zoom_independent` declaration is graded against are all the same walk.
/// Two of them would be one walk to bake by and another to be judged by.
pub fn zoom_dependence_of<'a>(
    widths: impl Iterator<Item = (usize, &'a crate::style::ZoomValue<f32>)>,
) -> Result<BakeBracket, ZoomDependence> {
    let mut agreed: Option<(u8, u8, usize)> = None;
    for (index, width) in widths {
        if width.is_constant() {
            continue;
        }
        let Some(ramp) = width.linear_ramp() else {
            return Err(ZoomDependence {
                feature: index,
                reason: "`line-width` is a zoom expression no fixed bake bracket can \
                         reproduce -- only a constant or an exactly-linear two-stop ramp \
                         can be baked once and blended at every zoom"
                    .into(),
            });
        };
        match agreed {
            None => agreed = Some((ramp.0, ramp.1, index)),
            Some((lo, hi, _)) if (lo, hi) == ramp => {}
            Some((lo, hi, first)) => {
                return Err(ZoomDependence {
                    feature: index,
                    reason: format!(
                        "`line-width` ramps over zoom {}..{} while feature {first} ramps over \
                         {lo}..{hi} -- one source is one draw call with one blend weight, so \
                         two ramps in it cannot both be exact",
                        ramp.0, ramp.1,
                    ),
                })
            }
        }
    }
    // `(0, 1)` for an all-constant collection: any span works, and a
    // positive one keeps `blend_t`'s division well-formed.
    let (lo, hi) = agreed.map(|(lo, hi, _)| (lo, hi)).unwrap_or((0, 1));
    Ok(BakeBracket::Ramp { lo, hi })
}

/// Which feature denies a source a fixed bake bracket, and what about it
/// does. Carried into `FeatureError::Feature` when a caller declared
/// `SourceOptions::zoom_independent` on a source that is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoomDependence {
    /// The offending feature's position in the source's own collection.
    pub feature: usize,
    pub reason: String,
}

/// `shared_width_ramp` over the widths alone, for a caller asking what the
/// bracket WOULD be under paint it has not applied yet.
///
/// That question is how `OverlayStore::plan_paint_update` decides whether a
/// recolour can be patched into the mesh or has to rebake it: the answer
/// changing means every feature's baked width is now wrong, not just the
/// updated one's.
pub fn shared_width_ramp_of<'a>(
    widths: impl Iterator<Item = &'a crate::style::ZoomValue<f32>>,
) -> Option<BakeBracket> {
    zoom_dependence_of(widths.enumerate()).ok()
}

/// ONE SOURCE's plan, plus the origin its coordinates are measured from,
/// and the bracket its paint was resolved at. All three travel together
/// because none means anything without the others: the origin is what the
/// plan's coordinates are measured from, and `bracket` is what says which
/// two zooms its paint values are -- a plan resolved at z9 paired with a
/// weight meant for z16 draws the wrong width, silently.
///
/// # Why per source rather than one plan with per-source op ranges
///
/// A single plan carrying a range of ops per source is the smaller diff,
/// and it is the wrong shape for all three of the things that actually
/// travel with a plan:
///
/// - **The origin.** `OverlayOrigin::centred_on` is the bounding-box
///   centre of the features in the plan. Shared between sources, adding a
///   source moves the origin every other source's i32 local coordinates
///   are measured from, so a push to one source re-projects (and therefore
///   re-tessellates) the rest -- which is the defect, reintroduced through
///   the geometry instead of through the op list.
/// - **The bracket.** One plan is one `blend_t` uniform. Shared, a single
///   `step` line-width anywhere in the overlay puts every source on the
///   `Bucket` path and makes every integer zoom re-resolve all of them.
///   See `shared_width_ramp`.
/// - **The mesh.** A shared plan tessellates to one vertex/index buffer.
///   Re-baking one source's op range means splicing a run into the middle
///   of it, shifting every later vertex index and invalidating the whole
///   GPU buffer -- the ~200 MB upload the bracket work exists to avoid.
///
/// So a source is the unit a plan is built at, and `feature_ops` stays
/// exactly the live per-input-feature machinery it always was, indexed by
/// position within its own source's collection. The concatenation the
/// draw path does is at the DRAW CALL level: `MapView::paint` pushes one
/// `(buffers, model bind group)` pair per source, in source order, which
/// is also what gives each source its own matrix and its own blend
/// weight. This is MapLibre's shape -- a bucket per source-layer, drawn in
/// layer order, each with its own uniforms -- rather than one mega-buffer
/// with offsets into it.
#[derive(Debug, Clone)]
pub struct OverlayPlan {
    pub origin: OverlayOrigin,
    pub plan: PaintPlan,
    /// What `build_plan` resolved every zoom-varying paint value at. Its
    /// partner is the GPU's `blend_t`, which must come from
    /// `BakeBracket::blend_t` on this same value.
    pub bracket: BakeBracket,
    /// For each INPUT feature, in feature order, the half-open range of
    /// `plan.ops` that feature produced. Empty (`start == end`) for a
    /// feature that draws nothing.
    ///
    /// Recorded during the build rather than reconstructed afterwards,
    /// because it cannot be reconstructed: `build_plan` drops a feature
    /// five different ways (null geometry, no line part with two
    /// positions, no ring with four, no point positions, a non-positive
    /// `circle-radius`) and the op list keeps no record of which feature
    /// each op came from. `MapView::set_feature_paint` resolves every
    /// paint-only update through this, and getting it wrong does not
    /// crash -- it recolours somebody else's line.
    ///
    /// ONE FEATURE MAPS TO AT MOST ONE OP, and on a `data_driven` plan
    /// SEVERAL FEATURES MAY MAP TO THE SAME ONE. That second half is new
    /// and is the whole point of a data-driven source -- 3,140 vehicles in
    /// twelve colours become twelve ops, each claimed by ~262 features. It
    /// is also why `data_driven` exists as a flag: an in-place repaint of a
    /// shared op would recolour every feature in it, so
    /// `OverlayStore::plan_source_paint_update` refuses the fast path on
    /// such a plan rather than taking it and being wrong.
    pub feature_ops: Vec<std::ops::Range<usize>>,
    /// True when this plan was built against a non-empty `SourcePaint` --
    /// so its paint came from the source's own expressions rather than from
    /// each feature, and its ops are MERGED across features that resolved
    /// to the same paint.
    ///
    /// Carried on the plan rather than re-derived from the source, because
    /// it is a property of what was actually built: a plan built before a
    /// declaration and one built after it differ in exactly this, and the
    /// paint-update path has to know which of the two it is holding.
    pub data_driven: bool,
    /// Per op, the `fill-outline-color` its fill draws a hairline in, or
    /// `None`. Empty when no op has one. Beside the ops rather than in
    /// `PaintOp::Fill`, so the basemap's op type is untouched; the overlay
    /// tessellation (`render::mesh::tessellate_overlay_plan_reusing`)
    /// reads it by op index.
    pub fill_outlines: Vec<Option<Rgba>>,
    /// The source's `line-gradient`, when it has one AND `lineMetrics`:
    /// every line op of this plan is then `metrics: true` and densified
    /// (see [`build_plan`]), and [`Self::colour_mesh`] colours the
    /// tessellated vertices by their progress. `None` otherwise, and on
    /// every plan built before gradients existed.
    pub line_gradient: Option<crate::overlay::source_paint::LineGradient>,
    /// `line-opacity` (or 1), multiplied into the gradient's alpha.
    pub line_gradient_opacity: f32,
}

/// The most uniform parts [`build_plan`] cuts each LineString of a
/// gradient line into (no segment longer than 1/64 of the line): a curved
/// gradient's vertex ramp then follows MapLibre's 256-texel texture to
/// within 1/64 of the line. Fewer when the source's point budget
/// ([`GRADIENT_POINT_BUDGET`]) is shared across many lines; stop cuts
/// come first and keep `linear`/`step` shapes exact either way.
pub const GRADIENT_PARTS: u32 = 64;

impl OverlayPlan {
    /// Colours a tessellated mesh of THIS plan by its `line-gradient`:
    /// every vertex with a line progress (`>= 0`, i.e. every metrics line
    /// vertex; fills and dots carry `-1`) gets `color` and `color_to`
    /// equal to `gradient.at(progress)` with its alpha times
    /// `line_gradient_opacity`. A no-op without a gradient.
    ///
    /// Called wherever the plan's mesh is tessellated or its paint
    /// rewritten (`MapView`'s `tessellate_overlay` and
    /// `refresh_overlay_bake_zoom`, the capture path), because a paint
    /// rewrite writes the op's flat colour back.
    pub fn colour_mesh(&self, vertices: &mut [crate::render::Vertex]) {
        if let Some(gradient) = &self.line_gradient {
            crate::render::mesh::colour_by_line_progress(vertices, |p| gradient.at(p), self.line_gradient_opacity);
        }
    }
}

/// Builds one plan covering every drawable feature, in array order, with
/// every zoom-varying paint value resolved for the bucket `bake_zoom`.
///
/// `None` when nothing is drawable -- an empty collection, or one whose
/// features all have null geometry. Those still have labels and hit
/// targets; they just have no triangles, and a mesh of no triangles is not
/// worth a GPU buffer.
///
/// Takes `&[OverlayFeature]` rather than the `&[(ParsedFeature,
/// FeaturePaint)]` pairs it used to, because this is now called TWICE for
/// the same features: once by `OverlayStore::replace`, and again by
/// `MapView::refresh_overlay_bake_zoom` every time the camera crosses an
/// integer zoom bucket. `OverlayFeature` is what the store keeps (behind an
/// `Arc`, snapshotted with the drawn mesh), and `ParsedFeature` is thrown
/// away at the end of `replace` -- so taking pairs would have meant either
/// keeping a second copy of every geometry alive for the re-resolve, or
/// re-parsing the caller's GeoJSON on a zoom crossing. Both are absurd next
/// to reading the geometry that is already in hand. The pairs carried no
/// information `OverlayFeature` lacks; it is the same geometry and the same
/// paint with an index and an id attached.
///
/// `paint` is the SOURCE's declared paint expressions. An empty one -- every
/// source that has never called `declare_source_paint`, which is every
/// source that existed before data-driven paint -- takes the original path
/// below, one op per feature, byte for byte. A non-empty one takes
/// [`build_plan_merged`], which resolves each feature's colour from its own
/// properties and then collapses every feature that landed on the same paint
/// into ONE op. The two are separate functions rather than one function with
/// branches so that the resolved-paint path cannot be changed by accident
/// while making the data-driven one work.
///
/// A source with `lineMetrics` (`SourcePaint::line_metrics`, the store's
/// mirror of `SourceOptions::line_metrics`) gets every LINE op marked
/// `metrics: true`, so its vertices carry progress; with a `line-gradient`
/// as well, each of its LineStrings is densified to [`GRADIENT_PARTS`]
/// parts first (`render::mesh::densify_for_gradient`) and the plan carries
/// the gradient for [`OverlayPlan::colour_mesh`]. Dots stay `metrics:
/// false` (a point has no progress).
pub fn build_plan(
    features: &[OverlayFeature],
    properties: &PropertyTable,
    bracket: BakeBracket,
    paint: &SourcePaint,
) -> Option<OverlayPlan> {
    build_plan_with(features, properties, bracket, paint, false)
}

/// [`build_plan`] with ONE OP PER FEATURE even under a declared
/// `SourcePaint` (no merging of features that resolve to the same paint):
/// what a story overlay layer with a per-feature effect (`highlight`,
/// `drop`, `pulse`) is planned with, so one feature's paint can be
/// rewritten in place (`render::mesh::rewrite_paint_attributes_for_ops` on
/// its own op) without touching another's. Without a declared paint this
/// is [`build_plan`] exactly (that path is already one op per feature).
pub fn build_plan_per_feature(
    features: &[OverlayFeature],
    properties: &PropertyTable,
    bracket: BakeBracket,
    paint: &SourcePaint,
) -> Option<OverlayPlan> {
    build_plan_with(features, properties, bracket, paint, true)
}

fn build_plan_with(
    features: &[OverlayFeature],
    properties: &PropertyTable,
    bracket: BakeBracket,
    paint: &SourcePaint,
    per_feature: bool,
) -> Option<OverlayPlan> {
    let mut plan = build_plan_unmetered(features, properties, bracket, paint, per_feature)?;
    let width_scale = paint.width_scale();
    if width_scale != 1.0 {
        for op in &mut plan.plan.ops {
            scale_op_widths(op, width_scale);
        }
    }
    if !paint.line_metrics {
        return Some(plan);
    }
    for op in &mut plan.plan.ops {
        if let PaintOp::Line { metrics, geometry, .. } = op {
            if matches!(**geometry, geo_types::Geometry::MultiLineString(_)) {
                *metrics = true;
            }
        }
    }
    let Some(gradient) = paint.line_gradient.as_ref() else { return Some(plan) };
    densify_gradient_ops(&mut plan.plan.ops, gradient);
    plan.line_gradient = Some(gradient.clone());
    plan.line_gradient_opacity = paint.gradient_opacity();
    Some(plan)
}

/// The most points a `line-gradient` source's lines may hold AFTER
/// densification: the D3 whole-source gate
/// ([`crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS`]) counts the
/// points the gradient adds, not only the ones pushed. What a mesh of many
/// short lines would otherwise cost (50k lines x 64 parts is 3.2M points,
/// about a gigabyte of vertices) is what this bounds.
pub const GRADIENT_POINT_BUDGET: usize = crate::overlay::tile_plan::TILED_SOURCE_MIN_POINTS;

/// The points a gradient's STOP cuts add to `features`' lines: one per
/// interior stop per LineString (`interpolate`), two for a `step` (`p -
/// eps`, `p + eps`). They come before the uniform parts, so a source whose
/// pushed points plus these exceed [`GRADIENT_POINT_BUDGET`] is refused
/// (`overlay::check_gradient_budget`) rather than drawn with its stops
/// smeared.
pub(crate) fn gradient_cut_points(features: &[OverlayFeature], gradient: &LineGradient) -> usize {
    let per_line = gradient.interior_stops().count() * if gradient.stepped() { 2 } else { 1 };
    if per_line == 0 {
        return 0;
    }
    let lines: usize = features
        .iter()
        .map(|f| match &f.geometry {
            OverlayGeometry::Line(parts) => parts.iter().filter(|part| part.len() >= 2).count(),
            _ => 0,
        })
        .sum();
    lines.saturating_mul(per_line)
}

/// Densifies every metrics line op of a gradient plan, under
/// [`GRADIENT_POINT_BUDGET`]:
///
/// 1. every LineString is cut at each of the gradient's interior stops
///    (`step`: at `p - eps` and `p + eps`, `eps` the larger of 1/4096 of
///    the line and one grid unit), so a stop's colour is exact at a vertex
///    and a step's edge spans `2 eps`;
/// 2. what the budget has left after the pushed points and those cuts is
///    shared across the LineStrings by LENGTH, each getting at most
///    [`GRADIENT_PARTS`] uniform parts (a line whose share is under one
///    part gets none). One route gets all 64; 50k short street segments
///    get a handful between them.
fn densify_gradient_ops(ops: &mut [PaintOp], gradient: &LineGradient) {
    let as_points = |line: &geo_types::LineString<i32>| -> Vec<(i32, i32)> { line.coords().map(|c| (c.x, c.y)).collect() };
    let length = |points: &[(i32, i32)]| -> f64 {
        points
            .windows(2)
            .map(|w| {
                let (dx, dy) = (f64::from(w[1].0) - f64::from(w[0].0), f64::from(w[1].1) - f64::from(w[0].1));
                (dx * dx + dy * dy).sqrt()
            })
            .sum()
    };
    let stops: Vec<f64> = gradient.interior_stops().map(f64::from).collect();
    let cuts_per_line = stops.len() * if gradient.stepped() { 2 } else { 1 };
    // Pass 1: the budget left for uniform parts, and the total length.
    let (mut pushed, mut lines, mut total_length) = (0usize, 0usize, 0.0f64);
    for op in ops.iter() {
        let PaintOp::Line { metrics: true, geometry, .. } = op else { continue };
        let geo_types::Geometry::MultiLineString(ml) = &**geometry else { continue };
        for line in &ml.0 {
            pushed += line.0.len();
            lines += 1;
            total_length += length(&as_points(line));
        }
    }
    let uniform_budget =
        GRADIENT_POINT_BUDGET.saturating_sub(pushed).saturating_sub(lines.saturating_mul(cuts_per_line)) as f64;
    // Pass 2: cut and densify.
    for op in ops.iter_mut() {
        let PaintOp::Line { metrics: true, geometry, .. } = op else { continue };
        let geo_types::Geometry::MultiLineString(ml) = &**geometry else { continue };
        let dense: Vec<geo_types::LineString<i32>> = ml
            .0
            .iter()
            .map(|line| {
                let points = as_points(line);
                let len = length(&points);
                let share = if total_length > 0.0 { uniform_budget * len / total_length } else { 0.0 };
                // `parts - 1` points added, never more than the share.
                let parts = (share.floor() + 1.0).min(f64::from(GRADIENT_PARTS)) as u32;
                let cuts: Vec<f64> = if gradient.stepped() {
                    let eps = (1.0 / 4096.0f64).max(if len > 0.0 { 1.0 / len } else { 0.0 });
                    stops.iter().flat_map(|p| [p - eps, p + eps]).collect()
                } else {
                    stops.clone()
                };
                crate::render::mesh::densify_at_progress(&points, parts, &cuts)
                    .into_iter()
                    .map(|(x, y)| geo_types::Coord { x, y })
                    .collect()
            })
            .collect();
        *geometry = Arc::new(geo_types::Geometry::MultiLineString(geo_types::MultiLineString(dense)));
    }
}

/// [`build_plan`] before line metrics: the per-feature or merged plan.
fn build_plan_unmetered(
    features: &[OverlayFeature],
    properties: &PropertyTable,
    bracket: BakeBracket,
    paint: &SourcePaint,
    per_feature: bool,
) -> Option<OverlayPlan> {
    // `centred_on_lon_lat`, not `centred_on`: the origin is a bounding-box
    // centre, and a bounding box costs TWO projections whatever the payload
    // (`geom::LonLatBounds`). This line used to read
    // `centred_on(features.flat_map(world_points))`, which projected every
    // coordinate into a `Vec` per feature and then dropped the `Vec` -- so
    // a push projected its whole payload once here and again in `ring`
    // below, for an identical answer. At 69,080 coordinates that was
    // ~0.9 ms of the 1.88 ms `build_plan` cost, spent to find out where the
    // middle of the data was.
    let origin = OverlayOrigin::centred_on_lon_lat(
        features.iter().flat_map(|feature| feature.geometry.runs()),
    )?;

    if !paint.is_empty() {
        return build_plan_merged(features, properties, bracket, origin, paint, per_feature);
    }

    let layer_paint = paint;
    let mut ops = Vec::with_capacity(features.len());
    let mut feature_ops = Vec::with_capacity(features.len());
    for feature in features {
        // The range is closed at the BOTTOM of the loop, past every skip,
        // so a feature that draws nothing still gets its (empty) entry and
        // `feature_ops` stays indexed by feature rather than by op. The
        // labelled block is what makes that possible: the skips below were
        // `continue`s, which would have jumped over the push.
        let start = ops.len();
        'feature: {
            let paint = &feature.paint;
            match &feature.geometry {
                // An adopted tiled feature is drawn per tile, never here.
                OverlayGeometry::Empty | OverlayGeometry::Indexed { .. } => {}
                OverlayGeometry::Line(parts) => {
                    let lines: Vec<geo_types::LineString<i32>> = parts
                        .iter()
                        .filter(|part| part.len() >= 2)
                        .map(|part| ring(part, &origin))
                        .collect();
                    if lines.is_empty() {
                        break 'feature;
                    }
                    ops.push(with_layer_stroke(
                        line_op(
                            geo_types::Geometry::MultiLineString(geo_types::MultiLineString(lines)),
                            paint.line_color,
                            bracket.resolve(&paint.line_width),
                            paint.line_dash,
                        ),
                        layer_paint,
                    ));
                }
                OverlayGeometry::Polygon(polygons) => {
                    let built: Vec<geo_types::Polygon<i32>> = polygons
                        .iter()
                        .filter(|rings| rings.first().is_some_and(|outer| outer.len() >= 4))
                        .map(|rings| {
                            geo_types::Polygon::new(
                                ring(&rings[0], &origin),
                                rings[1..].iter().map(|hole| ring(hole, &origin)).collect(),
                            )
                        })
                        .collect();
                    if built.is_empty() {
                        break 'feature;
                    }
                    ops.push(PaintOp::Fill {
                        color_from: paint.fill_color,
                        color_to: paint.fill_color,
                        geometry: Arc::new(geo_types::Geometry::MultiPolygon(
                            geo_types::MultiPolygon(built),
                        )),
                    });
                }
                OverlayGeometry::Point(points) => {
                    if points.is_empty() {
                        break 'feature;
                    }
                    // `circle-radius: 0` means "no circle", and is how a caller
                    // whose point is drawn by an ICON turns off the dot that
                    // would otherwise sit under it. Without this the two draw
                    // together, and since `circle-color` has a default, a
                    // feature that asked only for an icon still got a blue dot
                    // peeking out from behind it.
                    //
                    // Skipped here rather than left to tessellate: a
                    // zero-diameter round cap produces no triangles anyway, so
                    // this changes nothing on screen and saves walking every
                    // point of every such feature.
                    if paint.circle_radius <= 0.0 {
                        break 'feature;
                    }
                    // A dot is a ROUND-CAPPED stroke over a zero-length subpath,
                    // which is what `render::mesh::append_stroke` draws for POINT
                    // geometry on a line op -- see its own comment. The width is
                    // the diameter, so the cap's radius is `circle-radius`, in
                    // screen pixels at every zoom.
                    //
                    // Emitting the positions as `MultiPoint` rather than as
                    // degenerate `LineString`s is load-bearing, not cosmetic:
                    // line strings take the shared BUTT cap, and a butt-capped
                    // zero-length segment tessellates to nothing whatsoever --
                    // which is exactly how every point feature on this branch
                    // came to be invisible. The point variant is the signal that
                    // selects the round cap.
                    let dots: Vec<geo_types::Point<i32>> = points
                        .iter()
                        .map(|point| {
                            let (x, y) = local(*point, &origin);
                            geo_types::Point::new(x, y)
                        })
                        .collect();
                    // `circle-radius` is still a constant (see `paint`'s module
                    // doc), so both brackets take the same diameter and the
                    // shader's `mix` is a no-op for a dot at every zoom.
                    let diameter = paint.circle_radius * 2.0;
                    ops.push(line_op(
                        geo_types::Geometry::MultiPoint(geo_types::MultiPoint(dots)),
                        paint.circle_color,
                        (diameter, diameter),
                        crate::style::DashPattern::none(),
                    ));
                }
            }
        }
        feature_ops.push(start..ops.len());
    }

    (!ops.is_empty()).then(|| OverlayPlan {
        origin,
        plan: PaintPlan { ops },
        bracket,
        feature_ops,
        data_driven: false,
        fill_outlines: Vec::new(),
        line_gradient: None,
        line_gradient_opacity: 1.0,
    })
}

/// Everything about one op except its geometry, as bit patterns, so two
/// features can be asked "do you belong in the same op".
///
/// BITS, not the `f32`s themselves, because this keys a `HashMap` and
/// `Rgba`/`DashPattern` are float structs with no `Hash` and no `Eq`. Two
/// colours merge exactly when their four channels are bit-identical, which
/// is stricter than `==` on the one value where the two disagree (`-0.0`
/// against `0.0`, which is the same colour and would merely fail to merge)
/// and looser on none.
///
/// EVERYTHING that reaches the op is in here, not just the colour. A merge
/// that ignored `width` would silently draw two routes at one width, and
/// one that ignored `dash` would make a dashed line solid -- both are
/// "looks like a working map" failures, which is the class this crate
/// refuses. The cap and join are absent because they are the whole
/// source's (`line_op`, or `SourcePaint::line_cap`/`line_join`), the same
/// for every op it merges; if either ever becomes per-feature it belongs
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MergeKey {
    Line { color: [u32; 4], width: (u32, u32), dash: ([u32; 4], u8) },
    Fill { color: [u32; 4], outline: Option<[u32; 4]> },
    Dot { color: [u32; 4], diameter: u32 },
}

fn color_bits(color: Rgba) -> [u32; 4] {
    [color.r.to_bits(), color.g.to_bits(), color.b.to_bits(), color.a.to_bits()]
}

fn dash_bits(dash: DashPattern) -> ([u32; 4], u8) {
    (
        [
            dash.runs[0].to_bits(),
            dash.runs[1].to_bits(),
            dash.runs[2].to_bits(),
            dash.runs[3].to_bits(),
        ],
        dash.count,
    )
}

/// The geometry accumulated for one merged op, in the shape its `PaintOp`
/// wants.
///
/// Three variants rather than one `Vec<Geometry>` because the three op
/// shapes are genuinely different downstream: a `MultiPoint` on a line op is
/// what selects the ROUND cap a dot is drawn by (see `build_plan`'s point
/// arm), so a dot collapsed into a `MultiLineString` of zero-length parts
/// would tessellate to nothing at all -- which is exactly how every point
/// feature on an earlier branch came to be invisible.
enum MergedParts {
    Lines(Vec<geo_types::LineString<i32>>),
    Polygons(Vec<geo_types::Polygon<i32>>),
    Dots(Vec<geo_types::Point<i32>>),
}

/// Every declared colour slot, resolved for every row of the source's
/// [`PropertyTable`] up front -- the expression walked over COLUMNS, each
/// `["get", key]` bound to its column once (see
/// `SourcePaint::resolve_rows`).
///
/// It replaces a cache keyed by the ADDRESS of each feature's own
/// property map: sound, but only as good as the caller's sharing (a caller
/// who built one map per feature missed on every feature). Rows are keyed
/// by `OverlayFeature::index`, so there is nothing to share.
///
/// It holds the EXPRESSION's answer, not the finished colour. The fallback
/// is the feature's own `FeaturePaint` colour and differs between two
/// features with equal properties, so it is folded in per feature.
struct ResolvedColors {
    line: Option<Vec<Option<Rgba>>>,
    fill: Option<Vec<Option<Rgba>>>,
    outline: Option<Vec<Option<Rgba>>>,
    circle: Option<Vec<Option<Rgba>>>,
}

impl ResolvedColors {
    fn new(paint: &SourcePaint, table: &PropertyTable) -> Self {
        Self {
            line: paint.resolve_rows(ColorSlot::Line, table),
            fill: paint.resolve_rows(ColorSlot::Fill, table),
            outline: paint.resolve_rows(ColorSlot::FillOutline, table),
            circle: paint.resolve_rows(ColorSlot::Circle, table),
        }
    }

    /// Just the rows in `only`: entry `i` of each slot is row `only[i]`'s.
    fn picked(paint: &SourcePaint, table: &PropertyTable, only: &[usize]) -> Self {
        Self {
            line: paint.resolve_rows_at(ColorSlot::Line, table, Some(only)),
            fill: paint.resolve_rows_at(ColorSlot::Fill, table, Some(only)),
            outline: paint.resolve_rows_at(ColorSlot::FillOutline, table, Some(only)),
            circle: paint.resolve_rows_at(ColorSlot::Circle, table, Some(only)),
        }
    }

    fn get_opt(&self, slot: ColorSlot, row: usize) -> Option<Rgba> {
        let column = match slot {
            ColorSlot::Line => &self.line,
            ColorSlot::Fill => &self.fill,
            ColorSlot::FillOutline => &self.outline,
            ColorSlot::Circle => &self.circle,
        };
        column.as_ref()?.get(row).copied().flatten()
    }

    fn get(&self, slot: ColorSlot, row: usize, fallback: Rgba) -> Rgba {
        self.get_opt(slot, row).unwrap_or(fallback)
    }
}

/// One op under construction: its paint, and the parts every feature that
/// resolved to that paint contributed.
struct MergedOp {
    color: Rgba,
    outline: Option<Rgba>,
    width: (f32, f32),
    dash: DashPattern,
    parts: MergedParts,
}

/// `build_plan` for a source whose paint is an expression over its
/// features' properties.
///
/// # What it collapses, and why that is the whole feature
///
/// One op per DISTINCT RESOLVED PAINT, not one per feature. A fleet of
/// 3,140 vehicles in twelve route colours becomes twelve ops; the same fleet
/// in one colour becomes one. The vertices are unchanged -- geometry is
/// geometry, and every ring still has to be projected and tessellated -- but
/// everything charged per OP disappears: an `Arc<Geometry>` allocation, a
/// `Vec` of parts, a lyon `BuffersBuilder` and tessellator pass, an
/// `OpSpan`, and a `PaintOp` (176 bytes) per feature per push.
///
/// # Draw order
///
/// Ops appear in the order their paint was FIRST SEEN, and within an op the
/// parts are in feature order. So relative order is preserved exactly
/// between any two features that share a paint, and between two features
/// with different paints it becomes group order rather than array order.
///
/// That is a real difference and it is why data-driven paint is opt-in per
/// source rather than applied to every source that happens to admit it. It
/// is invisible whenever no two differently-painted features in the source
/// overlap on screen -- a fleet of vehicle markers, the case this exists for
/// -- and a caller who needs exact painter's order across colours has the
/// tool for it already: two sources, which draw in push order with no
/// merging at all.
fn build_plan_merged(
    features: &[OverlayFeature],
    properties: &PropertyTable,
    bracket: BakeBracket,
    origin: OverlayOrigin,
    paint: &SourcePaint,
    per_feature: bool,
) -> Option<OverlayPlan> {
    let mut merged: Vec<MergedOp> = Vec::new();
    // Keyed by the paint AND, when `per_feature`, the feature's row, so no
    // two features share an op (`build_plan_per_feature`); `usize::MAX`
    // otherwise, which merges exactly as before.
    let mut by_key: std::collections::HashMap<(MergeKey, usize), usize> = std::collections::HashMap::new();
    let merge_row = |row: usize| if per_feature { row } else { usize::MAX };
    let mut feature_ops = Vec::with_capacity(features.len());
    let colors = ResolvedColors::new(paint, properties);

    for feature in features {
        // Closed at the BOTTOM of the loop past every skip, exactly as the
        // resolved-paint path closes its range there, so a feature that
        // draws nothing still gets its (empty) entry and `feature_ops`
        // stays indexed by FEATURE.
        let mut claimed: Option<usize> = None;
        'feature: {
            let own = &feature.paint;
            let row = feature.index;
            match &feature.geometry {
                // An adopted tiled feature is drawn per tile, never here.
                OverlayGeometry::Empty | OverlayGeometry::Indexed { .. } => {}
                OverlayGeometry::Line(parts) => {
                    let lines: Vec<geo_types::LineString<i32>> = parts
                        .iter()
                        .filter(|part| part.len() >= 2)
                        .map(|part| ring(part, &origin))
                        .collect();
                    if lines.is_empty() {
                        break 'feature;
                    }
                    let color = colors.get(ColorSlot::Line, row, own.line_color);
                    let width = bracket.resolve(&own.line_width);
                    let key = MergeKey::Line {
                        color: color_bits(color),
                        width: (width.0.to_bits(), width.1.to_bits()),
                        dash: dash_bits(own.line_dash),
                    };
                    let at = *by_key.entry((key, merge_row(row))).or_insert_with(|| {
                        merged.push(MergedOp {
                            color,
                            outline: None,
                            width,
                            dash: own.line_dash,
                            parts: MergedParts::Lines(Vec::new()),
                        });
                        merged.len() - 1
                    });
                    let MergedParts::Lines(held) = &mut merged[at].parts else {
                        unreachable!("a MergeKey::Line op holds lines")
                    };
                    held.extend(lines);
                    claimed = Some(at);
                }
                OverlayGeometry::Polygon(polygons) => {
                    let built: Vec<geo_types::Polygon<i32>> = polygons
                        .iter()
                        .filter(|rings| rings.first().is_some_and(|outer| outer.len() >= 4))
                        .map(|rings| {
                            geo_types::Polygon::new(
                                ring(&rings[0], &origin),
                                rings[1..].iter().map(|hole| ring(hole, &origin)).collect(),
                            )
                        })
                        .collect();
                    if built.is_empty() {
                        break 'feature;
                    }
                    let color = colors.get(ColorSlot::Fill, row, own.fill_color);
                    let outline = colors.get_opt(ColorSlot::FillOutline, row);
                    let key = MergeKey::Fill { color: color_bits(color), outline: outline.map(color_bits) };
                    let at = *by_key.entry((key, merge_row(row))).or_insert_with(|| {
                        merged.push(MergedOp {
                            color,
                            outline,
                            width: (0.0, 0.0),
                            dash: DashPattern::none(),
                            parts: MergedParts::Polygons(Vec::new()),
                        });
                        merged.len() - 1
                    });
                    let MergedParts::Polygons(held) = &mut merged[at].parts else {
                        unreachable!("a MergeKey::Fill op holds polygons")
                    };
                    held.extend(built);
                    claimed = Some(at);
                }
                OverlayGeometry::Point(points) => {
                    if points.is_empty() {
                        break 'feature;
                    }
                    // `circle-radius: 0` is "draw no dot", the same opt-out
                    // the resolved-paint path honours -- see its point arm.
                    if own.circle_radius <= 0.0 {
                        break 'feature;
                    }
                    let dots: Vec<geo_types::Point<i32>> = points
                        .iter()
                        .map(|point| {
                            let (x, y) = local(*point, &origin);
                            geo_types::Point::new(x, y)
                        })
                        .collect();
                    let color = colors.get(ColorSlot::Circle, row, own.circle_color);
                    let diameter = own.circle_radius * 2.0;
                    let key = MergeKey::Dot {
                        color: color_bits(color),
                        diameter: diameter.to_bits(),
                    };
                    let at = *by_key.entry((key, merge_row(row))).or_insert_with(|| {
                        merged.push(MergedOp {
                            color,
                            outline: None,
                            width: (diameter, diameter),
                            dash: DashPattern::none(),
                            parts: MergedParts::Dots(Vec::new()),
                        });
                        merged.len() - 1
                    });
                    let MergedParts::Dots(held) = &mut merged[at].parts else {
                        unreachable!("a MergeKey::Dot op holds dots")
                    };
                    held.extend(dots);
                    claimed = Some(at);
                }
            }
        }
        feature_ops.push(match claimed {
            Some(at) => at..at + 1,
            None => 0..0,
        });
    }

    if merged.is_empty() {
        return None;
    }

    let fill_outlines: Vec<Option<Rgba>> = if merged.iter().any(|op| op.outline.is_some()) {
        merged.iter().map(|op| op.outline).collect()
    } else {
        Vec::new()
    };
    let ops = merged
        .into_iter()
        .map(|op| match op.parts {
            MergedParts::Lines(lines) => with_layer_stroke(
                line_op(geo_types::Geometry::MultiLineString(geo_types::MultiLineString(lines)), op.color, op.width, op.dash),
                paint,
            ),
            MergedParts::Polygons(polygons) => PaintOp::Fill {
                color_from: op.color,
                color_to: op.color,
                geometry: Arc::new(geo_types::Geometry::MultiPolygon(geo_types::MultiPolygon(
                    polygons,
                ))),
            },
            MergedParts::Dots(dots) => line_op(
                geo_types::Geometry::MultiPoint(geo_types::MultiPoint(dots)),
                op.color,
                op.width,
                DashPattern::none(),
            ),
        })
        .collect();

    Some(OverlayPlan {
        origin,
        plan: PaintPlan { ops },
        bracket,
        feature_ops,
        data_driven: true,
        fill_outlines,
        line_gradient: None,
        line_gradient_opacity: 1.0,
    })
}

/// The sentinel `FeatureClasses::of_feature` entry for a feature that
/// `classify` gives no class: a point, or an empty geometry, neither of
/// which classification serves (see `classify`).
///
/// This mirrors `overlay::tile_index::NO_CLASS` (also `u16::MAX`), defined
/// separately here rather than imported from it: Task 1 (the tile index)
/// and Task 2 (this file) are disjoint, parallel work on the same plan,
/// and neither needs the other's module to build. The two constants agree
/// on value because `TileIndex::build`'s `class_of: &[u16]` argument is
/// exactly a `FeatureClasses::of_feature`, so a point's `NO_CLASS` written
/// here must read back as `NO_CLASS` there.
pub(crate) const NO_CLASS: u16 = u16::MAX;

/// One class's resolved paint -- everything a tiled source's baked tile
/// needs for a feature's fill or its hairline outline, independent of which
/// feature produced it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClassPaint {
    Fill { color: Rgba, outline: Option<Rgba> },
    Line { color: Rgba, width: (f32, f32), dash: DashPattern },
}

/// A tiled source's features collapsed into distinct resolved paints: one
/// `ClassPaint` per class, and one class index per feature.
///
/// This is exactly `build_plan_merged`'s own collapse -- the same
/// `MergeKey`, the same `ResolvedColors` -- stopped at "which class" instead of
/// going on to build an op's geometry. A tiled source cuts geometry per
/// tile instead (`tile_index::TileCut`), so a feature's class is looked up
/// by index at bake time, once per tile it appears in, rather than
/// re-resolved from its properties every time.
pub struct FeatureClasses {
    pub classes: Vec<ClassPaint>,
    pub of_feature: Vec<u16>,
    pub data_driven: bool,
}

/// Classifies every feature of a source into a paint class.
///
/// Returns `None` when more than 65,534 distinct paints would be needed:
/// `u16::MAX` is reserved for [`NO_CLASS`], so a source with that many
/// distinct resolved paints cannot be classed at all, and stays on the
/// untiled, whole-source path.
///
/// A point (or empty-geometry) feature gets [`NO_CLASS`]: points stay on
/// the whole-source path (see the phase-B file map's `tile_index` entry),
/// so no paint class is meaningful for them here.
pub fn classify(
    features: &[OverlayFeature],
    properties: &PropertyTable,
    bracket: BakeBracket,
    paint: &SourcePaint,
) -> Option<FeatureClasses> {
    let colors = ResolvedColors::new(paint, properties);
    classify_with(features.iter().map(|feature| (feature, feature.index)), features.len(), &colors, bracket, paint)
}

/// [`classify`] over just `features[at]` for each `at` in `picks`, in that
/// order: `of_feature[i]` is `picks[i]`'s class.
///
/// Nothing is cloned -- neither the features nor their property rows --
/// and only the picked rows' colours are resolved, against the source's
/// own full `properties`. A paint update's table-only path classes one
/// representative per class this way, so its cost is the class count,
/// not the source's size.
pub fn classify_picked(
    features: &[OverlayFeature],
    picks: &[usize],
    properties: &PropertyTable,
    bracket: BakeBracket,
    paint: &SourcePaint,
) -> Option<FeatureClasses> {
    let rows: Vec<usize> = picks.iter().map(|&at| features[at].index).collect();
    let colors = ResolvedColors::picked(paint, properties, &rows);
    let picked = picks.iter().enumerate().map(|(i, &at)| (&features[at], i));
    classify_with(picked, picks.len(), &colors, bracket, paint)
}

/// The shared body: each item is a feature and its row in `colors`.
fn classify_with<'a>(
    features: impl Iterator<Item = (&'a OverlayFeature, usize)>,
    len: usize,
    colors: &ResolvedColors,
    bracket: BakeBracket,
    paint: &SourcePaint,
) -> Option<FeatureClasses> {
    let mut classes = Vec::new();
    let mut by_key: std::collections::HashMap<MergeKey, u16> = std::collections::HashMap::new();
    let mut of_feature = Vec::with_capacity(len);

    for (feature, row) in features {
        let own = &feature.paint;
        // `tile_kind`, not the variant: an adopted tiled feature
        // (`Indexed`) keeps its kind, and a paint update re-classes it.
        let (key, class) = match feature.geometry.tile_kind() {
            Some(TileKind::Fill) => {
                let color = colors.get(ColorSlot::Fill, row, own.fill_color);
                let outline = colors.get_opt(ColorSlot::FillOutline, row);
                (MergeKey::Fill { color: color_bits(color), outline: outline.map(color_bits) }, ClassPaint::Fill { color, outline })
            }
            Some(TileKind::Line) => {
                let color = colors.get(ColorSlot::Line, row, own.line_color);
                let width = bracket.resolve(&own.line_width);
                let key = MergeKey::Line {
                    color: color_bits(color),
                    width: (width.0.to_bits(), width.1.to_bits()),
                    dash: dash_bits(own.line_dash),
                };
                (key, ClassPaint::Line { color, width, dash: own.line_dash })
            }
            None => {
                of_feature.push(NO_CLASS);
                continue;
            }
        };
        let next = classes.len();
        let at = *by_key.entry(key).or_insert_with(|| {
            classes.push(class);
            next as u16
        });
        if classes.len() >= usize::from(NO_CLASS) {
            return None;
        }
        of_feature.push(at);
    }

    Some(FeatureClasses { classes, of_feature, data_driven: !paint.is_empty() })
}

/// The `MergeKey` `classify_with` files a class under, from the class.
fn class_key(class: &ClassPaint) -> MergeKey {
    match *class {
        ClassPaint::Fill { color, outline } => MergeKey::Fill { color: color_bits(color), outline: outline.map(color_bits) },
        ClassPaint::Line { color, width, dash } => MergeKey::Line {
            color: color_bits(color),
            width: (width.0.to_bits(), width.1.to_bits()),
            dash: dash_bits(dash),
        },
    }
}

/// [`classify_picked`], filed into `existing`'s class set: a pick whose
/// paint is already a class takes that id, a new paint is APPENDED, so
/// every id `existing` handed out keeps its meaning (a baked tile's class
/// ids stay valid). Returns the full class list and each pick's id
/// ([`NO_CLASS`] for a point or empty feature). `None` past 65,534
/// classes. The live-update path (`overlay::live`).
pub fn classify_into(
    existing: &[ClassPaint],
    features: &[OverlayFeature],
    picks: &[usize],
    properties: &PropertyTable,
    bracket: BakeBracket,
    paint: &SourcePaint,
) -> Option<(Vec<ClassPaint>, Vec<u16>)> {
    let local = classify_picked(features, picks, properties, bracket, paint)?;
    let mut classes = existing.to_vec();
    let mut by_key: std::collections::HashMap<MergeKey, u16> =
        classes.iter().enumerate().map(|(i, c)| (class_key(c), i as u16)).collect();
    let ids: Vec<u16> = local.classes.iter().map(|c| {
        let next = classes.len() as u16;
        *by_key.entry(class_key(c)).or_insert_with(|| {
            classes.push(*c);
            next
        })
    }).collect();
    if classes.len() >= usize::from(NO_CLASS) {
        return None;
    }
    let of_pick = local.of_feature.iter().map(|&c| if c == NO_CLASS { NO_CLASS } else { ids[usize::from(c)] }).collect();
    Some((classes, of_pick))
}

/// Overwrites one op's paint from the feature that produced it, leaving
/// its geometry -- and therefore its tessellation -- exactly as it was.
///
/// The inverse of the three `ops.push` sites in `build_plan`, and it has to
/// stay their inverse: a paint-only update that wrote a different value
/// here from the one a full rebuild would write is a fast path that
/// silently disagrees with the slow one. `overlay::tests::an_in_place_
/// update_leaves_the_plan_a_full_replace_would_have_built` is what holds
/// the two together.
///
/// The FEATURE, not the op kind, chooses which paint applies: a dot and a
/// line are both `PaintOp::Line` (a dot is a round-capped stroke over a
/// zero-length subpath -- see `build_plan`), and only the geometry says
/// which of `line-color` and `circle-color` this op was built from.
pub fn repaint_op(op: &mut PaintOp, feature: &OverlayFeature, bracket: BakeBracket) {
    let paint = &feature.paint;
    match (&feature.geometry, op) {
        (OverlayGeometry::Line(_), PaintOp::Line { color_from, color_to, width_from, width_to, dash, .. }) => {
            *color_from = paint.line_color;
            *color_to = paint.line_color;
            let (from, to) = bracket.resolve(&paint.line_width);
            *width_from = from;
            *width_to = to;
            *dash = paint.line_dash;
        }
        (OverlayGeometry::Polygon(_), PaintOp::Fill { color_from, color_to, .. }) => {
            *color_from = paint.fill_color;
            *color_to = paint.fill_color;
        }
        (OverlayGeometry::Point(_), PaintOp::Line { color_from, color_to, width_from, width_to, dash, .. }) => {
            *color_from = paint.circle_color;
            *color_to = paint.circle_color;
            // The diameter, in screen pixels at every zoom -- see
            // `build_plan`'s point arm.
            let diameter = paint.circle_radius * 2.0;
            *width_from = diameter;
            *width_to = diameter;
            *dash = crate::style::DashPattern::none();
        }
        (geometry, op) => unreachable!(
            "an op built from {geometry:?} cannot be a {op:?} -- feature_ops has drifted from the plan"
        ),
    }
}

/// Multiplies a line op's widths and gaps by `k`
/// (`SourcePaint::width_scale`): a dot's diameter is its width, so it
/// scales too. A fill has none.
pub(crate) fn scale_op_widths(op: &mut PaintOp, k: f32) {
    if let PaintOp::Line { width_from, width_to, gap_from, gap_to, .. } = op {
        *width_from *= k;
        *width_to *= k;
        *gap_from *= k;
        *gap_to *= k;
    }
}

/// `width` is the BRACKETING PAIR the plan's `BakeBracket` resolved -- the
/// value at each of that bracket's two zooms.
///
/// `shader.wgsl` blends `half_width_px`/`half_width_px_to` by the model
/// uniform's `blend_t` on every frame. The pair and that weight are a
/// matched set, which is why they come from one `BakeBracket` value: blend
/// a `Ramp`-baked pair by `fract(zoom)` and the width sawtooths at every
/// integer; blend a `Bucket`-baked pair by a ramp position and it is simply
/// the wrong number. See `BakeBracket`.
///
/// `color` still takes the same value in both brackets: `line-color` is
/// not zoom-varying yet (see `paint`'s module doc), so its `mix` is a
/// no-op at every zoom.
pub(crate) fn line_op(
    geometry: geo_types::Geometry<i32>,
    color: crate::style::Rgba,
    width: (f32, f32),
    dash: crate::style::DashPattern,
) -> PaintOp {
    PaintOp::Line {
        metrics: false,
        color_from: color,
        color_to: color,
        width_from: width.0,
        width_to: width.1,
        // The overlay has no `line-gap-width`: `FeaturePaint` exposes
        // `line-width`/`line-color`/`line-dasharray` and nothing else, and a
        // casing over a user's own route geometry has no fill layer to hug.
        gap_from: 0.0,
        gap_to: 0.0,
        // `Butt` unless the source declares a `line-cap` (`with_layer_stroke`,
        // a story layer's, spec §6.4). `Butt` is what every overlay
        // route has always been stroked with; the dot path below overrides it
        // to `Round` per feature, in `append_stroke`, because a zero-length
        // subpath is drawn by its cap alone.
        cap: crate::style::LineCap::Butt,
        // `Bevel` unless the source declares a `line-join` (as `cap`).
        // `Bevel`, not the style spec's `Miter`: an overlay route is the
        // caller's own GPS trace, sampled densely enough that a miter spike
        // at one noisy sample is a visible barb on an otherwise smooth line,
        // and `FeaturePaint` gives the caller no `line-join` to turn it off
        // with. This is also exactly what every overlay has always been
        // stroked with, so honouring a property the overlay does not expose
        // would be a silent change to shipped output for no gain.
        join: crate::style::LineJoin::Bevel,
        dash,
        geometry: Arc::new(geometry),
    }
}

/// `op` (a line op) stroked with the source's declared `line-cap` /
/// `line-join` (`SourcePaint::line_cap`, spec §6.4); a key the source does
/// not declare keeps [`line_op`]'s `Butt` / `Bevel`, so a source that
/// declares neither is stroked exactly as before.
fn with_layer_stroke(mut op: PaintOp, paint: &SourcePaint) -> PaintOp {
    if let PaintOp::Line { cap, join, .. } = &mut op {
        if let Some(c) = paint.line_cap {
            *cap = c;
        }
        if let Some(j) = paint.line_join {
            *join = j;
        }
    }
    op
}

fn local(lon_lat: (f64, f64), origin: &OverlayOrigin) -> (i32, i32) {
    origin.to_local(lon_lat_to_world(lon_lat.0, lon_lat.1))
}

fn ring(points: &[(f64, f64)], origin: &OverlayOrigin) -> geo_types::LineString<i32> {
    geo_types::LineString(
        points
            .iter()
            .map(|point| {
                let (x, y) = local(*point, origin);
                geo_types::Coord { x, y }
            })
            .collect(),
    )
}

/// Test plumbing: features and the property table they index, travelling
/// together, and `build_plan` / `classify` over the pair. Explicitly
/// imported by each test module, so it shadows the glob-imported
/// four-argument originals.
#[cfg(test)]
pub(crate) mod test_props {
    use super::*;

    /// Features plus their `PropertyTable` (row `i` = feature index `i`).
    pub(crate) struct Fx {
        pub features: Vec<OverlayFeature>,
        pub properties: PropertyTable,
    }

    impl std::ops::Deref for Fx {
        type Target = Vec<OverlayFeature>;
        fn deref(&self) -> &Self::Target {
            &self.features
        }
    }

    pub(crate) trait WithProps {
        fn parts(&self) -> (&[OverlayFeature], &PropertyTable);
    }

    impl WithProps for Vec<OverlayFeature> {
        fn parts(&self) -> (&[OverlayFeature], &PropertyTable) {
            static EMPTY: std::sync::OnceLock<PropertyTable> = std::sync::OnceLock::new();
            (self, EMPTY.get_or_init(PropertyTable::default))
        }
    }

    impl WithProps for Fx {
        fn parts(&self) -> (&[OverlayFeature], &PropertyTable) {
            (&self.features, &self.properties)
        }
    }

    impl WithProps for crate::overlay::OverlaySnapshot {
        fn parts(&self) -> (&[OverlayFeature], &PropertyTable) {
            (&self.features, &self.properties)
        }
    }

    pub(crate) fn build_plan(f: &impl WithProps, bracket: BakeBracket, paint: &SourcePaint) -> Option<OverlayPlan> {
        let (features, properties) = f.parts();
        super::build_plan(features, properties, bracket, paint)
    }

    pub(crate) fn classify(f: &impl WithProps, bracket: BakeBracket, paint: &SourcePaint) -> Option<FeatureClasses> {
        let (features, properties) = f.parts();
        super::classify(features, properties, bracket, paint)
    }
}

#[cfg(test)]
mod tests {

    /// A `lineMetrics` source `route` with `gradient`, holding `geojson`.
    fn gradient_store(gradient: serde_json::Value, geojson: &str) -> Result<crate::overlay::OverlayStore, crate::overlay::FeatureError> {
        let mut store = crate::overlay::OverlayStore::new();
        store.declare_source_line_metrics("route", true)?;
        let paint = serde_json::Map::from_iter([("line-gradient".to_string(), gradient)]);
        store.declare_source_paint("route", &paint)?;
        store.set_source("route", geojson, 3)?;
        Ok(store)
    }

    fn long_route() -> &'static str {
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[-10,0],[10,0]]}}]}"#
    }

    fn coloured_mesh(store: &crate::overlay::OverlayStore) -> crate::render::TileMesh {
        let plan = store.source_plan("route").expect("planned");
        let mut mesh = crate::render::tessellate_overlay_plan(&plan.plan, plan.origin.coord_scale);
        plan.colour_mesh(&mut mesh.vertices);
        mesh
    }

    /// Review F2: a `step` gradient keeps a hard edge -- every vertex is
    /// one colour or the other, and the blend between the last red and
    /// the first blue vertex spans under 2 px of a 1600 px route.
    #[test]
    fn a_step_gradient_edge_falls_within_two_px_of_its_stop() {
        let store =
            gradient_store(serde_json::json!(["step", ["line-progress"], "red", 0.5, "blue"]), long_route()).unwrap();
        let mesh = coloured_mesh(&store);
        let (mut red_max, mut blue_min) = (f32::MIN, f32::MAX);
        for v in mesh.vertices.iter().filter(|v| v.line_progress >= 0.0) {
            if v.color == [1.0, 0.0, 0.0, 1.0] {
                red_max = red_max.max(v.line_progress);
            } else if v.color == [0.0, 0.0, 1.0, 1.0] {
                blue_min = blue_min.min(v.line_progress);
            } else {
                panic!("a step vertex is pure red or pure blue, got {:?} at {}", v.color, v.line_progress);
            }
        }
        assert!(red_max < 0.5 && blue_min >= 0.5, "the edge is at the stop: red to {red_max}, blue from {blue_min}");
        let edge_px = (blue_min - red_max) * 1600.0;
        assert!(edge_px <= 2.0, "the blend spans {edge_px} px of a 1600 px route");
    }

    /// Review F2: a linear gradient with an interior stop is EXACT at every
    /// vertex (the plan cuts the line at the stop), so the rasteriser's
    /// interpolation between vertices is the expression itself.
    #[test]
    fn a_linear_gradient_is_exact_at_its_stops() {
        let expr = serde_json::json!(["interpolate", ["linear"], ["line-progress"], 0, "blue", 0.3, "red", 1, "lime"]);
        let store = gradient_store(expr.clone(), long_route()).unwrap();
        let gradient = store.source_plan("route").unwrap().line_gradient.clone().unwrap();
        let mesh = coloured_mesh(&store);
        let at_stop = mesh
            .vertices
            .iter()
            .filter(|v| v.line_progress >= 0.0)
            .min_by(|a, b| (a.line_progress - 0.3).abs().total_cmp(&(b.line_progress - 0.3).abs()))
            .unwrap();
        assert!((at_stop.line_progress - 0.3).abs() < 1e-5, "a vertex at the stop: {}", at_stop.line_progress);
        assert!(at_stop.color[0] > 0.9999 && at_stop.color[1] < 1e-4 && at_stop.color[2] < 1e-4, "pure red: {:?}", at_stop.color);
        let expr = crate::overlay::source_paint::tests_support::spelled(&expr);
        for v in mesh.vertices.iter().filter(|v| v.line_progress >= 0.0) {
            let want = crate::style_spec::expression::eval(
                &expr,
                crate::style_spec::expression::Scope::root().with_line_progress(f64::from(v.line_progress)),
                0,
            )
            .unwrap()
            .as_color()
            .unwrap();
            let got = gradient.at(v.line_progress);
            assert!((want.r - got.r).abs() < 1e-5 && (want.g - got.g).abs() < 1e-5 && (want.b - got.b).abs() < 1e-5);
            assert_eq!(v.color, [got.r, got.g, got.b, got.a]);
        }
    }

    /// Review F3: densification is bounded by the D3 gate. 50k short lines
    /// with a gradient stay within `GRADIENT_POINT_BUDGET` points in the
    /// plan (no gigabyte mesh), and a step gradient whose stop cuts alone
    /// would exceed it is refused at the push.
    #[test]
    fn a_gradient_on_many_short_lines_is_bounded_or_refused() {
        let n = 50_000;
        let features: Vec<String> = (0..n)
            .map(|i| {
                let x = -170.0 + f64::from(i) * 0.006;
                format!(r#"{{"type":"Feature","properties":{{}},"geometry":{{"type":"LineString","coordinates":[[{x},10],[{x},10.01]]}}}}"#)
            })
            .collect();
        let geojson = format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","));
        let linear = serde_json::json!(["interpolate", ["linear"], ["line-progress"], 0, "blue", 1, "red"]);
        let store = gradient_store(linear, &geojson).unwrap();
        let plan = store.source_plan("route").unwrap();
        let points: usize = plan
            .plan
            .ops
            .iter()
            .map(|op| match op {
                PaintOp::Line { geometry, .. } => match &**geometry {
                    geo_types::Geometry::MultiLineString(ml) => ml.0.iter().map(|l| l.0.len()).sum(),
                    _ => 0,
                },
                _ => 0,
            })
            .sum();
        assert!(points >= 2 * n as usize, "every pushed point kept: {points}");
        assert!(points <= GRADIENT_POINT_BUDGET, "densified within the budget: {points}");

        let step = serde_json::json!(["step", ["line-progress"], "red", 0.25, "blue", 0.5, "red", 0.75, "blue"]);
        let error = gradient_store(step, &geojson).expect_err("refused");
        assert!(error.to_string().contains("line-gradient"), "{error}");
        assert!(error.to_string().contains("fewer stops"), "{error}");
    }
    use super::test_props::build_plan;
    use super::*;
    use crate::overlay::geojson::parse_feature_collection;
    use crate::overlay::paint::FeaturePaint;

    fn features(json: &str) -> Vec<crate::overlay::OverlayFeature> {
        parse_feature_collection(json)
            .unwrap()
            .into_iter()
            .enumerate()
            .map(|(index, f)| {
                let paint = FeaturePaint::from_properties(&f.properties).unwrap();
                crate::overlay::OverlayFeature {
                    index,
                    id: f.id,
                    geometry: f.geometry,
                    paint,
                }
            })
            .collect()
    }

    /// The per-bucket bracket, which is what every test below that is about
    /// geometry rather than about zoom brackets wants: the fully general
    /// resolution, at one named zoom.
    fn at(zoom: u8) -> BakeBracket {
        BakeBracket::Bucket(zoom)
    }

    const ONE_LINE: &str = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","geometry":{"type":"LineString",
         "coordinates":[[103.85,1.29],[103.86,1.30]]},"properties":{"line-width":6}}]}"#;

    #[test]
    fn a_collection_with_no_drawable_geometry_builds_no_plan() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":null,"properties":{"text-field":"42"}}]}"#;
        assert!(build_plan(&features(json), at(12), &SourcePaint::default()).is_none());
    }

    #[test]
    fn a_line_feature_becomes_a_line_op() {
        let built = build_plan(&features(ONE_LINE), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.plan.ops.len(), 1);
        assert!(matches!(built.plan.ops[0], PaintOp::Line { .. }));
    }

    /// Spec §6.4: a source's `line-cap` / `line-join` stroke its lines,
    /// on the per-feature path and the merged one; a source declaring
    /// neither keeps the overlay's `Butt` / `Bevel`.
    #[test]
    fn a_declared_line_cap_and_join_stroke_the_lines() {
        let stroke = |built: &OverlayPlan| match &built.plan.ops[0] {
            PaintOp::Line { cap, join, .. } => (*cap, *join),
            other => panic!("expected a line op, got {other:?}"),
        };
        let declared = |json: &str| SourcePaint::compile(&serde_json::from_str(json).unwrap()).unwrap();
        let plain = build_plan(&features(ONE_LINE), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(stroke(&plain), (crate::style::LineCap::Butt, crate::style::LineJoin::Bevel));
        let round = declared(r#"{"line-cap": "round", "line-join": "round"}"#);
        assert!(round.is_empty(), "a stroke alone keeps the per-feature path");
        let built = build_plan(&features(ONE_LINE), at(12), &round).unwrap();
        assert_eq!(stroke(&built), (crate::style::LineCap::Round, crate::style::LineJoin::Round));
        let merged = build_plan(&features(ONE_LINE), at(12), &declared(r##"{"line-color": "#ff0000", "line-cap": "square"}"##)).unwrap();
        assert_eq!(stroke(&merged), (crate::style::LineCap::Square, crate::style::LineJoin::Bevel), "the merged path, join undeclared");
        assert!(SourcePaint::compile(&serde_json::from_str(r#"{"line-cap": "pointy"}"#).unwrap()).is_err());
    }

    /// A bare `line-width` still bakes IDENTICAL brackets, so the vertex
    /// shader's `mix(..., blend_t)` stays a no-op at every zoom for a
    /// constant -- which is what keeps the overwhelmingly common case free
    /// of any zoom-crossing cost at all. (`ZoomValue::value_at_pair` returns
    /// the same value twice for a `constant`/`stepped` value; only an
    /// `interpolate` ever produces two different ones.)
    ///
    /// This no longer means the mesh is never rebuilt: an INTERPOLATED width
    /// does change on a bucket crossing, and `MapView::refresh_overlay_bake_
    /// zoom` handles that by rewriting the paint attributes in place
    /// (`render::rewrite_paint_attributes`), never by re-tessellating.
    #[test]
    fn constant_paint_bakes_identical_zoom_brackets() {
        let built = build_plan(&features(ONE_LINE), at(12), &SourcePaint::default()).unwrap();
        match &built.plan.ops[0] {
            PaintOp::Line { color_from, color_to, width_from, width_to, .. } => {
                assert_eq!(color_from, color_to);
                assert_eq!(width_from, width_to);
                assert_eq!(*width_from, 6.0);
            }
            other => panic!("expected a line op, got {other:?}"),
        }
    }

    /// A zoom-interpolated `line-width` bakes the pair that BRACKETS the
    /// bake zoom -- `(width@z, width@z+1)` -- so `shader.wgsl`'s
    /// `mix(from, to, fract(camera.zoom))` lands on the true curve at every
    /// fractional zoom between the two, rather than snapping at each integer.
    #[test]
    fn an_interpolated_width_bakes_the_pair_that_brackets_the_bake_zoom() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.85,1.29],[103.86,1.30]]},
             "properties":{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}}]}"#;
        let built = build_plan(&features(json), at(9), &SourcePaint::default()).unwrap();
        assert_eq!(built.bracket, BakeBracket::Bucket(9));
        match &built.plan.ops[0] {
            PaintOp::Line { width_from, width_to, .. } => {
                assert!((*width_from - 2.88).abs() < 1e-4, "width_from was {width_from}");
                assert!((*width_to - 3.36).abs() < 1e-4, "width_to was {width_to}");
            }
            other => panic!("expected a line op, got {other:?}"),
        }
    }

    /// The same features, planned at a different zoom, give a different
    /// width -- which is what lets `MapView` re-resolve on a bucket crossing
    /// without re-parsing or re-tessellating anything.
    #[test]
    fn the_same_features_planned_at_a_higher_zoom_give_a_wider_line() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.85,1.29],[103.86,1.30]]},
             "properties":{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}}]}"#;
        let parsed = features(json);
        let width_at = |zoom| match &build_plan(&parsed, at(zoom), &SourcePaint::default()).unwrap().plan.ops[0] {
            PaintOp::Line { width_from, .. } => *width_from,
            other => panic!("expected a line op, got {other:?}"),
        };
        assert!(width_at(16) > width_at(9), "{} vs {}", width_at(16), width_at(9));
        assert!((width_at(16) - 6.24).abs() < 1e-4, "z16 was {}", width_at(16));
    }

    /// A point whose stop is drawn by an ICON must not also get the
    /// default dot. `circle-color` has a default, so before this a feature
    /// that asked only for `icon-image` still drew a blue circle, which
    /// showed as a ring around every pin and as a field of dots wherever
    /// the pins were too small to cover them.
    #[test]
    fn a_zero_radius_point_emits_no_dot_at_all() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[103.85,1.29]},
             "properties":{"circle-radius":0,"icon-image":"stop-pin"}}]}"#;
        assert!(
            build_plan(&features(json), at(12), &SourcePaint::default()).is_none(),
            "a point with no circle and no other geometry has nothing to tessellate"
        );
    }

    /// ...but a point that did NOT ask for zero still gets its dot, at the
    /// style's default radius. The skip must be opt-in, not a change to
    /// what a plain point feature draws.
    #[test]
    fn a_point_with_a_default_radius_still_emits_its_dot() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[103.85,1.29]},
             "properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).expect("a dot is drawable");
        assert_eq!(built.plan.ops.len(), 1);
        assert!(matches!(built.plan.ops[0], PaintOp::Line { .. }), "a dot is a round-capped stroke");
    }

    /// Geometry is emitted in the local grid, near zero -- which is the
    /// whole reason an origin exists.
    #[test]
    fn geometry_is_local_to_the_origin_and_small() {
        let built = build_plan(&features(ONE_LINE), at(12), &SourcePaint::default()).unwrap();
        let PaintOp::Line { geometry, .. } = &built.plan.ops[0] else { panic!("expected a line") };
        let geo_types::Geometry::MultiLineString(lines) = geometry.as_ref() else {
            panic!("expected a multilinestring, got {geometry:?}")
        };
        for coord in lines.0.iter().flat_map(|line| line.0.iter()) {
            assert!(coord.x.abs() < 100_000, "x was {}", coord.x);
            assert!(coord.y.abs() < 100_000, "y was {}", coord.y);
        }
    }

    /// Draw order is array order: a later feature's op comes after an
    /// earlier one's, so it draws over it.
    #[test]
    fn ops_follow_the_features_array_order() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"line-color":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"line-color":"#00ff00"}}]}"##;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        let color_of = |index: usize| match &built.plan.ops[index] {
            PaintOp::Line { color_from, .. } => *color_from,
            other => panic!("expected a line op, got {other:?}"),
        };
        assert_eq!(color_of(0).r, 1.0, "first op is the red feature");
        assert_eq!(color_of(1).g, 1.0, "second op is the green feature");
    }

    /// Equal `from`/`to` on the fill path matters exactly as it does on the
    /// line path: it is what lets this op's mesh be tessellated once and
    /// never rebuilt as the camera zooms.
    #[test]
    fn a_polygon_feature_becomes_a_fill_op() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Polygon",
             "coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]},"properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert!(matches!(built.plan.ops[0], PaintOp::Fill { .. }));
        match &built.plan.ops[0] {
            PaintOp::Fill { color_from, color_to, .. } => assert_eq!(color_from, color_to),
            other => panic!("expected a fill op, got {other:?}"),
        }
    }

    /// A point is drawn as a round-ended stroke of one degenerate segment
    /// rather than through a circle tessellator that does not exist.
    ///
    /// Equal `from`/`to` on the point path matters exactly as it does on the
    /// line path: it is what lets this op's mesh be tessellated once and
    /// never rebuilt as the camera zooms.
    #[test]
    fn a_point_feature_becomes_a_stroke_as_wide_as_its_diameter() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[103.85,1.29]},
             "properties":{"circle-radius":7}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        match &built.plan.ops[0] {
            PaintOp::Line { color_from, color_to, width_from, width_to, .. } => {
                assert_eq!(*width_from, 14.0);
                assert_eq!(width_from, width_to);
                assert_eq!(color_from, color_to);
            }
            other => panic!("expected a line op, got {other:?}"),
        }
    }

    /// The cross-check the `PaintOp` assertions above cannot make: a point
    /// feature must survive `build_plan` AND `tessellate_plan` and arrive at
    /// the GPU as actual triangles.
    ///
    /// This is the test that was missing. Every point assertion in this
    /// module stopped at the `PaintOp`, and `hit.rs`'s stopped at the
    /// geometry list, so a point op that tessellated to nothing at all --
    /// which is exactly what a butt-capped zero-length segment does -- read
    /// as fully covered while every vehicle and every stop was invisible.
    #[test]
    fn a_point_feature_tessellates_to_a_non_empty_mesh() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[103.85,1.29]},
             "properties":{"circle-radius":7}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        let mesh = crate::render::tessellate_plan(&built.plan, 1, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
        assert!(!mesh.vertices.is_empty(), "a point feature produced no vertices");
        assert!(!mesh.indices.is_empty(), "a point feature produced no triangles");
        assert_eq!(mesh.indices.len() % 3, 0, "indices must form whole triangles");
        assert!(
            mesh.vertices.iter().all(|vertex| vertex.is_stroke == 1.0),
            "a dot is drawn by the stroke path, so every vertex must extrude"
        );
    }

    /// The same cross-check for the other geometry kind, so the pair pins
    /// that both actually reach the GPU rather than only reaching a
    /// `PaintOp`.
    #[test]
    fn a_line_feature_tessellates_to_a_non_empty_mesh() {
        let built = build_plan(&features(ONE_LINE), at(12), &SourcePaint::default()).unwrap();
        let mesh = crate::render::tessellate_plan(&built.plan, 1, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
        assert!(!mesh.vertices.is_empty(), "a line feature produced no vertices");
        assert!(!mesh.indices.is_empty(), "a line feature produced no triangles");
        assert_eq!(mesh.indices.len() % 3, 0, "indices must form whole triangles");
    }

    /// A feature with an empty geometry contributes no op but does not stop
    /// its neighbours contributing theirs.
    #[test]
    fn an_empty_geometry_contributes_no_op_but_does_not_break_the_plan() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":null,"properties":{}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.plan.ops.len(), 1);
    }

    /// The origin sits inside the data, so the plan's own geometry brackets
    /// zero rather than sitting far off to one side.
    #[test]
    fn the_origin_lands_inside_the_features_bounds() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString",
             "coordinates":[[103.0,1.0],[104.0,2.0]]},"properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        let (west, _) = crate::overlay::geom::lon_lat_to_world(103.0, 1.0);
        let (east, _) = crate::overlay::geom::lon_lat_to_world(104.0, 2.0);
        assert!(built.origin.world_x > west && built.origin.world_x < east);
    }

    /// The map every paint-only update is resolved through: one range per
    /// INPUT feature, in feature order, naming the ops that feature
    /// produced.
    ///
    /// Derived from the ops instead, this would be a guess -- `build_plan`
    /// skips five different ways and the op list carries no record of which
    /// feature each op came from. A guess that is wrong recolours the wrong
    /// feature, which is not a crash but a wrong-coloured line.
    #[test]
    fn feature_ops_names_one_range_per_feature_and_covers_every_op() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"line-color":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"Polygon",
             "coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]},"properties":{}},
            {"type":"Feature","geometry":{"type":"Point","coordinates":[2,2]},
             "properties":{"circle-radius":4}}]}"##;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.feature_ops.len(), 3, "one range per input feature");
        assert_eq!(built.feature_ops, vec![0..1, 1..2, 2..3]);
        // The ranges tile `plan.ops` exactly: no op is claimed twice and
        // none is left unclaimed.
        let claimed: usize = built.feature_ops.iter().map(|range| range.len()).sum();
        assert_eq!(claimed, built.plan.ops.len());
    }

    /// A polygon produces exactly ONE op today -- a fill, with no outline.
    /// Pinned so `feature_ops` is asserted against what `build_plan`
    /// actually emits rather than against what a reader assumes it emits.
    #[test]
    fn a_polygon_claims_exactly_the_one_fill_op_it_produces() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Polygon",
             "coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]},"properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.feature_ops, vec![0..1]);
        assert!(matches!(built.plan.ops[0], PaintOp::Fill { .. }));
    }

    /// Skip case one: `OverlayGeometry::Empty` pushes nothing at all.
    #[test]
    fn a_null_geometry_feature_claims_an_empty_range() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":null,"properties":{"text-field":"42"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.feature_ops, vec![0..0, 0..1]);
    }

    /// Skip case two: every part shorter than two positions, so `lines` is
    /// empty and the line op is never pushed.
    #[test]
    fn a_line_with_no_part_long_enough_claims_an_empty_range() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"MultiLineString","coordinates":[[[0,0]]]},
             "properties":{}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.plan.ops.len(), 1, "only the second feature draws");
        assert_eq!(built.feature_ops, vec![0..0, 0..1]);
    }

    /// Skip case three: an outer ring of fewer than four positions, so
    /// `built` is empty and the fill op is never pushed.
    #[test]
    fn a_polygon_with_too_short_an_outer_ring_claims_an_empty_range() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[0,0]]]},
             "properties":{}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.plan.ops.len(), 1);
        assert_eq!(built.feature_ops, vec![0..0, 0..1]);
    }

    /// Skip case four: a `MultiPoint` with no positions in it.
    #[test]
    fn a_point_feature_with_no_positions_claims_an_empty_range() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"MultiPoint","coordinates":[]},
             "properties":{}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.plan.ops.len(), 1);
        assert_eq!(built.feature_ops, vec![0..0, 0..1]);
    }

    /// Skip case five: `circle-radius: 0`, the "an icon draws this, not a
    /// dot" opt-out.
    #[test]
    fn a_zero_radius_point_claims_an_empty_range() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[0,0]},
             "properties":{"circle-radius":0,"icon-image":"stop-pin"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.plan.ops.len(), 1);
        assert_eq!(built.feature_ops, vec![0..0, 0..1]);
    }

    /// Two skips in a row, then a drawable feature: the ranges must stay
    /// aligned to FEATURE index, not collapse to op index.
    #[test]
    fn consecutive_skips_do_not_shift_the_ranges_that_follow_them() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{}},
            {"type":"Feature","geometry":null,"properties":{}},
            {"type":"Feature","geometry":{"type":"MultiPoint","coordinates":[]},
             "properties":{}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[2,2],[3,3]]},
             "properties":{}}]}"#;
        let built = build_plan(&features(json), at(12), &SourcePaint::default()).unwrap();
        assert_eq!(built.feature_ops, vec![0..1, 1..1, 1..1, 1..2]);
        assert_eq!(built.plan.ops.len(), 2);
    }
}

/// `build_plan_merged`, graded against the resolved-paint path it must
/// agree with everywhere except the op count.
#[cfg(test)]
mod merged_tests {
    use super::test_props::{build_plan, Fx};
    use super::*;
    use crate::overlay::geojson::parse_feature_collection;
    use crate::overlay::paint::FeaturePaint;

    fn at(zoom: u8) -> BakeBracket {
        BakeBracket::Bucket(zoom)
    }

    /// Features carrying their GeoJSON `properties` as a
    /// `FeatureProperties`, the way `OverlayStore::set_source` does for a
    /// data-driven source.
    fn features_with_properties(json: &str) -> Fx {
        let parsed = parse_feature_collection(json).unwrap();
        let maps: Vec<_> = parsed.iter().map(|f| crate::overlay::properties_from_json(&f.properties)).collect();
        let features = parsed
            .into_iter()
            .enumerate()
            .map(|(index, f)| {
                let paint = FeaturePaint::from_properties(&f.properties).unwrap();
                OverlayFeature { index, id: f.id, geometry: f.geometry, paint }
            })
            .collect();
        Fx { features, properties: PropertyTable::from_maps(&maps) }
    }

    fn features_without_properties(json: &str) -> Vec<OverlayFeature> {
        parse_feature_collection(json)
            .unwrap()
            .into_iter()
            .enumerate()
            .map(|(index, f)| {
                let paint = FeaturePaint::from_properties(&f.properties).unwrap();
                OverlayFeature {
                    index,
                    id: f.id,
                    geometry: f.geometry,
                    paint,
                }
            })
            .collect()
    }

    fn source_paint(json: &str) -> SourcePaint {
        SourcePaint::compile(&serde_json::from_str(json).unwrap()).unwrap()
    }

    fn fill_colors(plan: &OverlayPlan) -> Vec<Rgba> {
        plan.plan
            .ops
            .iter()
            .map(|op| match op {
                PaintOp::Fill { color_from, color_to, .. } => {
                    assert_eq!(color_from, color_to, "an overlay colour has no zoom axis");
                    *color_from
                }
                other => panic!("expected a fill op, got {other:?}"),
            })
            .collect()
    }

    fn polygon_count(op: &PaintOp) -> usize {
        let PaintOp::Fill { geometry, .. } = op else { panic!("expected a fill") };
        let geo_types::Geometry::MultiPolygon(polygons) = geometry.as_ref() else {
            panic!("expected a multipolygon, got {geometry:?}")
        };
        polygons.0.len()
    }

    /// Six squares in three colours, declared through `["get", "colour"]`.
    fn six_squares() -> String {
        let square = |at: f64| {
            format!(
                "[[[{a},{a}],[{b},{a}],[{b},{b}],[{a},{b}],[{a},{a}]]]",
                a = at,
                b = at + 1.0
            )
        };
        let colours = ["#ff0000", "#00ff00", "#0000ff", "#ff0000", "#00ff00", "#0000ff"];
        let features: Vec<String> = colours
            .iter()
            .enumerate()
            .map(|(index, colour)| {
                format!(
                    r#"{{"type":"Feature","id":"sq-{index}","geometry":{{"type":"Polygon",
                       "coordinates":{}}},"properties":{{"colour":"{colour}"}}}}"#,
                    square(index as f64)
                )
            })
            .collect();
        format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))
    }

    /// THE headline property: N features that differ only in one paint
    /// colour collapse to one op PER COLOUR, while staying N features.
    #[test]
    fn six_features_in_three_colours_become_three_ops() {
        let features = features_with_properties(&six_squares());
        let paint = source_paint(r#"{"fill-color":["get","colour"]}"#);

        let resolved = build_plan(&features, at(12), &SourcePaint::default()).unwrap();
        assert_eq!(resolved.plan.ops.len(), 6, "one op per feature without a declaration");
        assert!(!resolved.data_driven);

        let merged = build_plan(&features, at(12), &paint).unwrap();
        assert_eq!(merged.plan.ops.len(), 3, "one op per distinct resolved colour");
        assert!(merged.data_driven);
        // Still six features, each claiming exactly one op -- which is what
        // keeps ids, labels and hit targets per bus.
        assert_eq!(merged.feature_ops.len(), 6);
        assert_eq!(merged.feature_ops, vec![0..1, 1..2, 2..3, 0..1, 1..2, 2..3]);
        // And the same geometry: two squares in each of the three ops.
        assert_eq!(merged.plan.ops.iter().map(polygon_count).collect::<Vec<_>>(), vec![2, 2, 2]);
    }

    /// The colours are ACTUALLY RIGHT, not merely fewer. A merged path that
    /// painted everything one colour would be faster and useless, so this
    /// asserts the three `Rgba`s against the three hex strings in the
    /// properties -- and that they are three DIFFERENT values, which a
    /// single-colour bug would fail.
    #[test]
    fn each_merged_op_carries_the_colour_its_features_property_named() {
        let features = features_with_properties(&six_squares());
        let paint = source_paint(r#"{"fill-color":["get","colour"]}"#);
        let merged = build_plan(&features, at(12), &paint).unwrap();

        let want = |hex: &str| crate::style_spec::color::parse_css_color(hex).unwrap();
        assert_eq!(fill_colors(&merged), vec![want("#ff0000"), want("#00ff00"), want("#0000ff")]);
    }

    /// The same colours survive TESSELLATION and land on the vertices.
    ///
    /// This is the assertion the op-level one cannot make.
    /// `srgb_to_linear_rgb` happens in the SHADER, so a vertex must hold
    /// the sRGB components verbatim; a `Vertex::color` that had been
    /// linearised on the CPU would draw dark and oversaturated and would
    /// pass every `PaintOp` assertion above.
    #[test]
    fn the_merged_mesh_carries_three_distinct_vertex_colours_in_srgb() {
        let features = features_with_properties(&six_squares());
        let paint = source_paint(r#"{"fill-color":["get","colour"]}"#);
        let merged = build_plan(&features, at(12), &paint).unwrap();
        let mesh =
            crate::render::tessellate_plan(&merged.plan, 1, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE);
        assert!(!mesh.vertices.is_empty());

        let mut seen: Vec<[f32; 4]> = Vec::new();
        for vertex in &mesh.vertices {
            assert_eq!(vertex.color, vertex.color_to, "an overlay colour has no zoom axis");
            if !seen.contains(&vertex.color) {
                seen.push(vertex.color);
            }
        }
        seen.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let want = |hex: &str| {
            let c = crate::style_spec::color::parse_css_color(hex).unwrap();
            [c.r, c.g, c.b, c.a]
        };
        let mut expected = vec![want("#ff0000"), want("#00ff00"), want("#0000ff")];
        expected.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(seen, expected, "three colours, verbatim sRGB, on the vertices");
        // Not a single colour. Stated separately because "fast and one
        // colour" is the failure this whole test exists to catch.
        assert_eq!(seen.len(), 3);
    }

    /// The merged mesh holds the same VERTICES as the unmerged one:
    /// geometry is geometry, and merging changes only how many ops it
    /// arrives in.
    ///
    /// Positions are compared as a multiset rather than in order, because
    /// merging reorders parts across colour groups by construction -- see
    /// `build_plan_merged`'s draw-order note.
    #[test]
    fn merging_changes_the_op_count_and_not_the_vertices() {
        let features = features_with_properties(&six_squares());
        let paint = source_paint(r#"{"fill-color":["get","colour"]}"#);
        let mesh_of = |plan: &OverlayPlan| {
            crate::render::tessellate_plan(&plan.plan, 1, crate::render::mesh::COORD_SCALE, crate::render::subdivide::Subdivision::NONE)
        };
        let resolved = mesh_of(&build_plan(&features, at(12), &SourcePaint::default()).unwrap());
        let merged = mesh_of(&build_plan(&features, at(12), &paint).unwrap());
        assert_eq!(merged.vertices.len(), resolved.vertices.len());
        assert_eq!(merged.indices.len(), resolved.indices.len());

        let positions = |mesh: &crate::render::mesh::TileMesh| {
            let mut out: Vec<[u32; 2]> = mesh
                .vertices
                .iter()
                .map(|v| [v.position[0].to_bits(), v.position[1].to_bits()])
                .collect();
            out.sort_unstable();
            out
        };
        assert_eq!(positions(&merged), positions(&resolved));
    }

    /// A source with no declaration takes the original path byte for byte,
    /// which is what makes this change backward compatible -- including for
    /// features that happen to be carrying properties.
    #[test]
    fn an_empty_source_paint_builds_exactly_the_resolved_paint_plan() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"line-color":"#ff0000","line-width":6}},
            {"type":"Feature","geometry":{"type":"Point","coordinates":[2,2]},
             "properties":{"circle-radius":4}},
            {"type":"Feature","geometry":{"type":"Polygon",
             "coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]},"properties":{}}]}"##;
        let with = features_with_properties(json);
        let without = features_without_properties(json);
        let one = build_plan(&with, at(12), &SourcePaint::default()).unwrap();
        let two = build_plan(&without, at(12), &SourcePaint::default()).unwrap();
        assert_eq!(format!("{one:#?}"), format!("{two:#?}"));
        assert!(!one.data_driven);
    }

    /// Merging keys on EVERYTHING that reaches the op, not just the colour.
    /// Two lines of one colour and two different widths stay two ops; a
    /// merge that ignored width would draw one of them wrong and look like
    /// a working map.
    #[test]
    fn one_colour_and_two_widths_do_not_merge() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"c":"#ff0000","line-width":2}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[2,2],[3,3]]},
             "properties":{"c":"#ff0000","line-width":8}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[4,4],[5,5]]},
             "properties":{"c":"#ff0000","line-width":2}}]}"##;
        let features = features_with_properties(json);
        let paint = source_paint(r#"{"line-color":["get","c"]}"#);
        let merged = build_plan(&features, at(12), &paint).unwrap();
        assert_eq!(
            merged.plan.ops.len(),
            2,
            "the two width-2 lines merge, the width-8 one does not"
        );
        assert_eq!(merged.feature_ops, vec![0..1, 1..2, 0..1]);
    }

    /// A dashed line and a solid one of the same colour stay two ops, for
    /// the same reason.
    #[test]
    fn one_colour_and_two_dash_patterns_do_not_merge() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[2,2],[3,3]]},
             "properties":{"c":"#ff0000","line-dasharray":[2,2]}}]}"##;
        let features = features_with_properties(json);
        let paint = source_paint(r#"{"line-color":["get","c"]}"#);
        assert_eq!(build_plan(&features, at(12), &paint).unwrap().plan.ops.len(), 2);
    }

    /// A line, a fill and a dot never merge with each other whatever their
    /// colour: a `MultiPoint` on a line op is what selects the ROUND cap a
    /// dot is drawn by, so a dot folded into a `MultiLineString` would
    /// tessellate to nothing at all.
    #[test]
    fn the_three_geometry_kinds_never_merge_into_one_op() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"Polygon",
             "coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]},"properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"Point","coordinates":[2,2]},
             "properties":{"c":"#ff0000","circle-radius":4}}]}"##;
        let features = features_with_properties(json);
        let paint = source_paint(
            r#"{"line-color":["get","c"],"fill-color":["get","c"],"circle-color":["get","c"]}"#,
        );
        let merged = build_plan(&features, at(12), &paint).unwrap();
        assert_eq!(merged.plan.ops.len(), 3);
        assert_eq!(merged.feature_ops, vec![0..1, 1..2, 2..3]);
        // The dot really is a MultiPoint, which is the load-bearing half.
        let PaintOp::Line { geometry, .. } = &merged.plan.ops[2] else {
            panic!("expected a line op")
        };
        assert!(
            matches!(geometry.as_ref(), geo_types::Geometry::MultiPoint(_)),
            "a dot must stay a MultiPoint or it takes the butt cap and vanishes",
        );
    }

    /// Every one of `build_plan`'s five skips still produces an EMPTY range
    /// on the merged path, so `feature_ops` stays indexed by feature.
    /// Asserted separately from the resolved-paint path's own tests because
    /// the merged builder closes its range in a different place.
    #[test]
    fn the_merged_path_honours_every_skip_and_stays_indexed_by_feature() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":null,"properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"MultiLineString","coordinates":[[[0,0]]]},
             "properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[0,0]]]},
             "properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"MultiPoint","coordinates":[]},
             "properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"Point","coordinates":[0,0]},
             "properties":{"c":"#ff0000","circle-radius":0}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"c":"#ff0000"}}]}"##;
        let features = features_with_properties(json);
        let paint = source_paint(
            r#"{"line-color":["get","c"],"fill-color":["get","c"],"circle-color":["get","c"]}"#,
        );
        let merged = build_plan(&features, at(12), &paint).unwrap();
        assert_eq!(merged.plan.ops.len(), 1, "only the last feature draws");
        assert_eq!(merged.feature_ops, vec![0..0, 0..0, 0..0, 0..0, 0..0, 0..1]);
    }

    /// A source whose every feature resolves to the SAME colour collapses
    /// to exactly one op -- the best case, and the one a caller drawing one
    /// coloured fleet gets.
    #[test]
    fn one_colour_across_every_feature_collapses_to_one_op() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[2,2],[3,3]]},
             "properties":{"c":"#ff0000"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[4,4],[5,5]]},
             "properties":{"c":"#ff0000"}}]}"##;
        let features = features_with_properties(json);
        let paint = source_paint(r#"{"line-color":["get","c"]}"#);
        let merged = build_plan(&features, at(12), &paint).unwrap();
        assert_eq!(merged.plan.ops.len(), 1);
        assert_eq!(merged.feature_ops, vec![0..1, 0..1, 0..1]);
    }

    /// A data-driven colour leaves the bake bracket exactly where
    /// `zoom_dependence` puts it -- requirement 2, asserted rather than
    /// argued. The colour has no zoom axis (`source_paint` refuses one), so
    /// a `Ramp` source stays a `Ramp` source with a declaration on it, and
    /// every merged op still carries the ramp's own bracketed width pair.
    #[test]
    fn a_data_driven_colour_does_not_change_the_bake_bracket() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"c":"#ff0000",
               "line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[2,2],[3,3]]},
             "properties":{"c":"#00ff00",
               "line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}}]}"##;
        let features = features_with_properties(json);
        assert_eq!(zoom_dependence(&features), Ok(BakeBracket::Ramp { lo: 8, hi: 18 }));
        let paint = source_paint(r#"{"line-color":["get","c"]}"#);
        let merged = build_plan(&features, BakeBracket::Ramp { lo: 8, hi: 18 }, &paint).unwrap();
        assert_eq!(merged.bracket, BakeBracket::Ramp { lo: 8, hi: 18 });
        assert_eq!(merged.plan.ops.len(), 2, "two colours, two ops");
        for op in &merged.plan.ops {
            let PaintOp::Line { width_from, width_to, .. } = op else {
                panic!("expected a line")
            };
            assert!((*width_from - 2.4).abs() < 1e-5, "width_from was {width_from}");
            assert!((*width_to - 7.2).abs() < 1e-5, "width_to was {width_to}");
        }
    }

    /// A feature whose property matches nothing keeps its OWN colour, so a
    /// partial feed draws rather than vanishing -- and that fallback is a
    /// merge key like any other, so two such features merge together.
    #[test]
    fn features_that_match_no_branch_fall_back_and_merge_on_the_fallback() {
        let json = r##"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]},
             "properties":{"line-color":"#123456"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[2,2],[3,3]]},
             "properties":{"line-color":"#123456"}},
            {"type":"Feature","geometry":{"type":"LineString","coordinates":[[4,4],[5,5]]},
             "properties":{"c":"#ff0000","line-color":"#123456"}}]}"##;
        let features = features_with_properties(json);
        let paint = source_paint(r#"{"line-color":["get","c"]}"#);
        let merged = build_plan(&features, at(12), &paint).unwrap();
        assert_eq!(merged.plan.ops.len(), 2);
        assert_eq!(merged.feature_ops, vec![0..1, 0..1, 1..2]);
        let PaintOp::Line { color_from, .. } = &merged.plan.ops[0] else { panic!("a line") };
        assert_eq!(*color_from, crate::overlay::paint::parse_color("#123456").unwrap());
    }
}

/// What 261x fewer ops is worth on the BAKE, which the probe's stopwatch
/// around the push cannot see.
#[cfg(test)]
mod merged_bake_bench {
    use super::test_props::{build_plan, classify, Fx};
    use super::*;
    use crate::overlay::source_paint::no_properties;
    use crate::overlay::FeatureProperties;
    use crate::overlay::paint::FeaturePaint;
    use crate::mvt::TagValue;
    use std::time::Duration;

    const ROUTES: usize = 12;
    const PALETTE: [&str; ROUTES] = [
        "#e6194b", "#3cb44b", "#ffe119", "#4363d8", "#f58231", "#911eb4", "#46f0f0", "#f032e6",
        "#bcf60c", "#fabebe", "#008080", "#e6beff",
    ];

    /// The probe's own heading-shaped marker: two 11-position rings,
    /// deterministic, so two runs measure the same geometry.
    fn marker(index: usize) -> Vec<Vec<(f64, f64)>> {
        let mut state = (index as u64).wrapping_mul(6364136223846793005).wrapping_add(1);
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 0.5
        };
        let lon = 103.60 + 0.30 * (next() + 0.5);
        let lat = 1.22 + 0.20 * (next() + 0.5);
        let heading = next() * std::f64::consts::TAU;
        let scale = 25.0 / 111_320.0;
        let ring = |radius: f64| -> Vec<(f64, f64)> {
            let mut out = Vec::with_capacity(11);
            for step in 0..10 {
                let angle = heading + step as f64 * std::f64::consts::TAU / 10.0;
                out.push((lon + radius * scale * angle.cos(), lat + radius * scale * angle.sin()));
            }
            out.push(out[0]);
            out
        };
        vec![ring(1.0), ring(0.55)]
    }

    /// 3,140 vehicles as one feature each. `resolved` writes the route
    /// colour into the feature's own paint; the other carries it as a
    /// property for the source's expression to read. The GEOMETRY is
    /// identical, which is the whole point of the comparison.
    fn fleet(count: usize, resolved: bool) -> Fx {
        let shared: Vec<Arc<FeatureProperties>> = (0..ROUTES)
            .map(|route| {
                Arc::new(FeatureProperties::from([(
                    "route_colour".to_string(),
                    TagValue::String(PALETTE[route].to_string()),
                )]))
            })
            .collect();
        let features = (0..count)
            .map(|index| {
                let route = index % ROUTES;
                let color = crate::style_spec::color::parse_css_color(PALETTE[route]).unwrap();
                OverlayFeature {
                    index,
                    id: None,
                    geometry: OverlayGeometry::Polygon(
                        marker(index).into_iter().map(|ring| vec![ring]).collect(),
                    ),
                    paint: if resolved {
                        FeaturePaint { fill_color: color, ..FeaturePaint::default() }
                    } else {
                        FeaturePaint::default()
                    },
                }
            })
            .collect::<Vec<_>>();
        let maps: Vec<Arc<FeatureProperties>> =
            (0..count).map(|index| if resolved { no_properties() } else { Arc::clone(&shared[index % ROUTES]) }).collect();
        Fx { features, properties: PropertyTable::from_maps(maps.iter().map(|m| &**m)) }
    }

    /// The headline case's BAKE cost: 3,140 resolved-paint features (69,080
    /// coordinates) against the same fleet as one data-driven feature set.
    ///
    ///     cargo test --release the_merged_bake_against_the_resolved_one -- --ignored --nocapture
    ///
    /// `#[ignore]`d because a timing is evidence, not an assertion -- it
    /// belongs to whoever asks for it, and it would be flaky as a gate on a
    /// shared machine. What it measures is the half `examples/
    /// overlay_source_probe`'s stopwatch structurally cannot: tessellation
    /// runs on a blocking worker, so a timer around the push sees the plan
    /// build and never the lyon passes the plan's op count decides.
    #[test]
    #[ignore]
    fn the_merged_bake_against_the_resolved_one() {
        measure(3140);
    }

    /// The same attribution at FOUR PAYLOAD SIZES, which is what separates
    /// a per-coordinate cost from a per-feature or per-op one.
    ///
    ///     cargo test --release a_push_at_four_payload_sizes -- --ignored --nocapture
    ///
    /// Every feature is the same 22-position marker whatever the count, so
    /// features, ops and coordinates all scale together here -- the
    /// discrimination comes from comparing the per-coordinate figures the
    /// last column prints, which stay flat for a cost that is really per
    /// coordinate and fall as 1/n for one that is really per push.
    ///
    /// Measured this way, `build_plan` was 27.0 ns/coordinate and
    /// `HitIndex::build` 15.6, both flat across a 64x payload range, before
    /// the projection was made to happen once; they are 12.0 and 1.9 after.
    /// The last line is the counterweight: `feature_at` moved from 1.46 us
    /// to 2.08 us per query at 3,140 features, because the exact test now
    /// projects the candidate it is testing rather than reading a stored
    /// copy. A push happens per tick and a query per mouse-move, and 0.6 us
    /// against a 16 ms frame is the price of 2.1 ms of UI thread.
    ///
    /// DISJOINT WINDOWS, never cumulative: every figure is the median of 31
    /// independent timings of one whole operation (`interleaved`), so no
    /// window overlaps another and no total is divided by a run count.
    #[test]
    #[ignore]
    fn a_push_at_four_payload_sizes() {
        for count in [196, 785, 3140, 12560] {
            measure(count);
        }
    }

    fn measure(count: usize) {
        let paint = SourcePaint::compile(
            &serde_json::from_str(r#"{"fill-color":["get","route_colour"]}"#).unwrap(),
        )
        .unwrap();

        let resolved_features = fleet(count, true);
        let data_features = fleet(count, false);
        let bracket = BakeBracket::Ramp { lo: 0, hi: 1 };

        // INTERLEAVED, and medians rather than means. This runs on a
        // shared machine whose load moves by 4x within one run; measuring
        // arm A five times and then arm B five times charges whatever the
        // load was doing to whichever arm was unlucky, which is how a 0.98x
        // and a 1.26x came out of the same two code paths twenty minutes
        // apart. Alternating them puts the same drift through both.
        const RUNS: usize = 31;
        let median = |mut runs: Vec<Duration>| {
            runs.sort_unstable();
            runs[runs.len() / 2]
        };
        let interleaved = |a: &mut dyn FnMut(), b: &mut dyn FnMut()| {
            let mut left = Vec::with_capacity(RUNS);
            let mut right = Vec::with_capacity(RUNS);
            for _ in 0..RUNS {
                let started = crate::timing::Instant::now();
                a();
                left.push(started.elapsed());
                let started = crate::timing::Instant::now();
                b();
                right.push(started.elapsed());
            }
            (median(left), median(right))
        };

        let resolved_plan =
            build_plan(&resolved_features, bracket, &SourcePaint::default()).unwrap();
        let merged_plan = build_plan(&data_features, bracket, &paint).unwrap();
        assert_eq!(resolved_plan.plan.ops.len(), count);
        assert_eq!(merged_plan.plan.ops.len(), ROUTES);

        let (plan_resolved, plan_merged) = interleaved(
            &mut || {
                build_plan(&resolved_features, bracket, &SourcePaint::default()).unwrap();
            },
            &mut || {
                build_plan(&data_features, bracket, &paint).unwrap();
            },
        );

        let scale = crate::render::mesh::COORD_SCALE;
        let (tess_resolved, tess_merged) = interleaved(
            &mut || {
                crate::render::tessellate_plan(&resolved_plan.plan, 1, scale, crate::render::subdivide::Subdivision::NONE);
            },
            &mut || {
                crate::render::tessellate_plan(&merged_plan.plan, 1, scale, crate::render::subdivide::Subdivision::NONE);
            },
        );

        let one = crate::render::tessellate_plan(&resolved_plan.plan, 1, scale, crate::render::subdivide::Subdivision::NONE);
        let two = crate::render::tessellate_plan(&merged_plan.plan, 1, scale, crate::render::subdivide::Subdivision::NONE);

        println!(
            "\n{count} heading-shaped markers (two 11-position rings each), twelve route colours\n\
             \n  resolved paint   {:>5} ops  {:>7} vertices  build_plan {:>8.3} ms  tessellate {:>8.3} ms\
             \n  data-driven      {:>5} ops  {:>7} vertices  build_plan {:>8.3} ms  tessellate {:>8.3} ms\
             \n  ratio            {:>5.1}x       {:>7.3}x                {:>8.2}x              {:>8.2}x\n",
            resolved_plan.plan.ops.len(),
            one.vertices.len(),
            plan_resolved.as_secs_f64() * 1e3,
            tess_resolved.as_secs_f64() * 1e3,
            merged_plan.plan.ops.len(),
            two.vertices.len(),
            plan_merged.as_secs_f64() * 1e3,
            tess_merged.as_secs_f64() * 1e3,
            resolved_plan.plan.ops.len() as f64 / merged_plan.plan.ops.len() as f64,
            one.vertices.len() as f64 / two.vertices.len() as f64,
            plan_resolved.as_secs_f64() / plan_merged.as_secs_f64(),
            tess_resolved.as_secs_f64() / tess_merged.as_secs_f64(),
        );

        // The vertices must NOT move -- geometry is geometry. Asserted, not
        // printed, because a "faster" number bought by tessellating less is
        // not the same change.
        assert_eq!(one.vertices.len(), two.vertices.len());
        assert_eq!(one.indices.len(), two.indices.len());

        // And the twelve colours really are on the merged mesh.
        let mut seen: Vec<[u32; 4]> = two
            .vertices
            .iter()
            .map(|v| [v.color[0].to_bits(), v.color[1].to_bits(), v.color[2].to_bits(), v.color[3].to_bits()])
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ROUTES, "twelve distinct vertex colours, not one");

        // WHERE THE REST OF A PUSH GOES. The op collapse is worth what the
        // ratio above says and no more, so the honest next question is what
        // the other ~85% of a push is -- printed here rather than guessed
        // at, because the answer changes what a consumer should optimise
        // next.
        let (hit_index, _) = interleaved(
            &mut || {
                crate::overlay::hit::HitIndex::build(&"bench".into(), &resolved_features);
            },
            &mut || {},
        );
        // A WHOLE PUSH through the public door, so the stages above can be
        // read as fractions of something rather than in isolation. The
        // borrow tree is built here rather than reused from `fleet`,
        // because a real caller hands in borrowed positions and converting
        // those is one of the stages being attributed.
        let rings: Vec<Vec<Vec<[f64; 2]>>> = (0..count)
            .map(|index| {
                marker(index)
                    .into_iter()
                    .map(|ring| ring.into_iter().map(|(x, y)| [x, y]).collect())
                    .collect()
            })
            .collect();
        let borrowed: Vec<Vec<Vec<&[[f64; 2]]>>> = rings
            .iter()
            .map(|feature| feature.iter().map(|ring| vec![ring.as_slice()]).collect())
            .collect();
        let nested: Vec<Vec<&[&[[f64; 2]]]>> = borrowed
            .iter()
            .map(|feature| feature.iter().map(|poly| poly.as_slice()).collect())
            .collect();
        let inputs: Vec<crate::overlay::FeatureInput<'_>> = nested
            .iter()
            .enumerate()
            .map(|(index, polys)| crate::overlay::FeatureInput {
                id: Some(crate::overlay::FeatureId::String(format!("bus-{index}"))),
                geometry: crate::overlay::GeometryInput::Polygon(polys.as_slice()),
                paint: FeaturePaint::default(),
            })
            .collect();
        let shared: Vec<Arc<FeatureProperties>> = (0..ROUTES)
            .map(|route| {
                Arc::new(FeatureProperties::from([(
                    "route_colour".to_string(),
                    TagValue::String(PALETTE[route].to_string()),
                )]))
            })
            .collect();
        let data_inputs: Vec<crate::overlay::DataFeatureInput<'_>> = inputs
            .iter()
            .enumerate()
            .map(|(index, feature)| crate::overlay::DataFeatureInput {
                feature: feature.clone(),
                properties: Arc::clone(&shared[index % ROUTES]),
            })
            .collect();

        let mut resolved_store = crate::overlay::OverlayStore::new();
        let mut data_store = crate::overlay::OverlayStore::new();
        data_store
            .declare_source_paint(
                "v",
                &serde_json::from_str(r#"{"fill-color":["get","route_colour"]}"#).unwrap(),
            )
            .unwrap();
        let (push_resolved, push_data) = interleaved(
            &mut || {
                resolved_store.set_source_from("v", &inputs, 12).unwrap();
            },
            &mut || {
                data_store.set_source_data_from("v", &data_inputs, 12).unwrap();
            },
        );
        assert_eq!(resolved_store.source_op_count("v"), Some(count));
        assert_eq!(data_store.source_op_count("v"), Some(ROUTES));

        // PAYLOAD CONSTANCY, asserted rather than assumed. Every timing
        // above is a mean over a window of runs against these exact
        // numbers, and a "faster" figure bought by planning fewer features
        // or tessellating fewer vertices is not the same measurement.
        // 22 positions per feature: two 11-position rings (see `marker`).
        let coordinates = count * 22;
        assert_eq!(
            resolved_features.iter().map(|f| f.geometry.positions().count()).sum::<usize>(),
            coordinates,
            "the resolved arm must carry exactly the payload the per-coordinate column divides by",
        );
        assert_eq!(
            data_features.iter().map(|f| f.geometry.positions().count()).sum::<usize>(),
            coordinates,
            "and the data-driven arm the same one",
        );
        assert_eq!(resolved_plan.plan.ops.len(), count);
        assert_eq!(one.vertices.len(), count * 20);

        let per_coordinate = |elapsed: Duration| elapsed.as_secs_f64() * 1e9 / coordinates as f64;
        println!(
            "  WHERE A PUSH ACTUALLY GOES, at {coordinates} coordinates \
             ({count} features x 22 positions):"
        );
        println!(
            "    set_source_from, whole push                {:>8.3} ms  ({count} ops)   \
             {:>6.1} ns/coord",
            push_resolved.as_secs_f64() * 1e3,
            per_coordinate(push_resolved),
        );
        println!(
            "    set_source_data_from, whole push           {:>8.3} ms  ({ROUTES} ops)   \
             {:>6.1} ns/coord",
            push_data.as_secs_f64() * 1e3,
            per_coordinate(push_data),
        );
        println!(
            "      of which build_plan                      {:>8.3} ms                 \
             {:>6.1} ns/coord",
            plan_resolved.as_secs_f64() * 1e3,
            per_coordinate(plan_resolved),
        );
        println!(
            "      of which HitIndex::build                 {:>8.3} ms                 \
             {:>6.1} ns/coord",
            hit_index.as_secs_f64() * 1e3,
            per_coordinate(hit_index),
        );
        println!(
            "      the rest -- to_geometry, validate, the\n\
             \x20     paint clones, the id map               {:>8.3} ms                 \
             {:>6.1} ns/coord",
            (push_resolved.as_secs_f64() - plan_resolved.as_secs_f64() - hit_index.as_secs_f64())
                * 1e3,
            per_coordinate(
                push_resolved.saturating_sub(plan_resolved).saturating_sub(hit_index)
            ),
        );
        println!(
            "    tessellate, off the UI thread, afterwards  {:>8.3} ms                 \
             {:>6.1} ns/coord",
            tess_resolved.as_secs_f64() * 1e3,
            per_coordinate(tess_resolved),
        );

        // THE OTHER SIDE OF THE LEDGER. `hit::HitIndex` stores bounding
        // boxes and projects a candidate's geometry at QUERY time, so a
        // build that got cheaper must not have made a query expensive --
        // `MapView::feature_at` runs on every mouse-move, and a
        // mouse-move's whole budget is one frame. Measured at two points:
        // one dead on a marker (bbox passes, the exact test runs and
        // projects) and one in open water inside the fleet's overall
        // bounds (every bbox rejects, nothing is projected).
        let snapshot = resolved_store.source_snapshot("v").expect("the source was just pushed");
        let zoom = 16.0;
        let per_px = crate::overlay::hit::world_units_per_px(zoom);
        let tolerance = crate::overlay::hit::tolerance_world(6.0, zoom, 1.0);
        // The MIDDLE of one marker, not one of its vertices: a vertex sits
        // exactly on the ring and an even-odd crossing test is undefined
        // on its own boundary.
        let on_a_marker = {
            let outer = marker(count / 2).swap_remove(0);
            let points = outer.len() as f64;
            let lon = outer.iter().map(|&(lon, _)| lon).sum::<f64>() / points;
            let lat = outer.iter().map(|&(_, lat)| lat).sum::<f64>() / points;
            crate::overlay::geom::lon_lat_to_world(lon, lat)
        };
        // North-west of every marker (`marker` scatters over
        // 103.60..103.90, 1.22..1.42) but well inside nothing.
        let on_nothing = crate::overlay::geom::lon_lat_to_world(103.55, 1.45);
        assert!(
            snapshot.feature_at(on_a_marker, tolerance, per_px).is_some(),
            "the hit probe must actually hit",
        );
        assert!(
            snapshot.feature_at(on_nothing, tolerance, per_px).is_none(),
            "the miss probe must actually miss",
        );
        const QUERIES: usize = 200;
        let query = |probe: (f64, f64)| -> Duration {
            let mut runs = Vec::with_capacity(QUERIES);
            for _ in 0..QUERIES {
                let started = crate::timing::Instant::now();
                std::hint::black_box(snapshot.feature_at(probe, tolerance, per_px));
                runs.push(started.elapsed());
            }
            median(runs)
        };
        println!(
            "    feature_at, per query: on a marker         {:>8.3} us    \
             in open water {:>8.3} us\n",
            query(on_a_marker).as_secs_f64() * 1e6,
            query(on_nothing).as_secs_f64() * 1e6,
        );
    }

    fn fill_collection(colours: Vec<String>) -> String {
        let feats: Vec<String> = colours
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    r#"{{"type":"Feature","id":{i},"properties":{{"fill":"{c}"}},"geometry":{{"type":"Polygon","coordinates":[[[{x},0],[{x1},0],[{x1},1],[{x},1],[{x},0]]]}}}}"#,
                    x = i as f64 * 0.01,
                    x1 = i as f64 * 0.01 + 0.005
                )
            })
            .collect();
        format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, feats.join(","))
    }

    fn data_driven_fill_source(json: &str) -> (crate::overlay::OverlaySnapshot, Arc<SourcePaint>) {
        let mut store = crate::overlay::OverlayStore::new();
        let paint = serde_json::json!({"fill-color": ["get", "fill"]});
        store.declare_source_paint("s", paint.as_object().unwrap()).unwrap();
        store.set_source("s", json, 3).unwrap();
        let snap = store.source_snapshot("s").unwrap();
        (snap.clone(), snap.paint.clone())
    }

    fn plain_source(json: &str) -> (crate::overlay::OverlaySnapshot, Arc<SourcePaint>) {
        let mut store = crate::overlay::OverlayStore::new();
        store.set_source("s", json, 3).unwrap();
        let snap = store.source_snapshot("s").unwrap();
        (snap.clone(), snap.paint.clone())
    }

    #[test]
    fn classify_gives_one_class_per_distinct_resolved_paint() {
        // Nine fill colours over 900 features, like the block-group demo.
        let json = fill_collection((0..900).map(|i| format!("#0{}0000", i % 9)).collect());
        let (features, paint) = data_driven_fill_source(&json);
        let c = classify(&features, BakeBracket::Bucket(3), &paint).unwrap();
        assert_eq!(c.classes.len(), 9);
        assert_eq!(c.of_feature.len(), 900);
        assert!(c.of_feature.iter().all(|&k| usize::from(k) < 9));
        assert!(c.data_driven);
    }

    /// The paint update's representative check: classing picked features
    /// against the full table gives each pick the paint the full classify
    /// gave it, one class per distinct paint in pick order.
    #[test]
    fn classify_picked_agrees_with_the_full_classify_on_every_pick() {
        let json = fill_collection((0..900).map(|i| format!("#0{}0000", i % 9)).collect());
        let (snap, paint) = data_driven_fill_source(&json);
        let full = classify(&snap, BakeBracket::Bucket(3), &paint).unwrap();
        let picks = [899usize, 3, 13, 450, 7, 3];
        let picked =
            super::classify_picked(&snap.features, &picks, &snap.properties, BakeBracket::Bucket(3), &paint).unwrap();
        assert_eq!(picked.of_feature.len(), picks.len());
        for (i, &at) in picks.iter().enumerate() {
            assert_eq!(
                picked.classes[usize::from(picked.of_feature[i])],
                full.classes[usize::from(full.of_feature[at])],
                "pick {i} (feature {at})"
            );
        }
        assert_eq!(picked.of_feature[1], picked.of_feature[5], "a repeated pick is one class");
        assert_eq!(picked.classes.len(), 5, "five distinct paints among the picks");
    }

    #[test]
    fn classify_matches_build_plan_merged_op_for_op() {
        let json = fill_collection((0..50).map(|i| format!("#00{:02x}00", i * 5)).collect());
        let (features, paint) = data_driven_fill_source(&json);
        let c = classify(&features, BakeBracket::Bucket(3), &paint).unwrap();
        let plan = build_plan(&features, BakeBracket::Bucket(3), &paint).unwrap();
        assert_eq!(c.classes.len(), plan.plan.ops.len());
        for (class, op) in c.classes.iter().zip(&plan.plan.ops) {
            match (class, op) {
                (ClassPaint::Fill { color, .. }, PaintOp::Fill { color_from, .. }) => assert_eq!(color, color_from),
                other => panic!("mismatch {other:?}"),
            }
        }
    }

    #[test]
    fn points_get_no_class() {
        let json = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#;
        let (features, paint) = plain_source(json);
        let c = classify(&features, BakeBracket::Bucket(3), &paint).unwrap();
        assert_eq!(c.of_feature, vec![NO_CLASS]);
    }

    /// A continuous ramp over 10k random incomes (one in ten missing) stays
    /// at a few hundred paint classes, not 10k: `RAMP_STEPS` quantisation
    /// (at most 256 ramp colours) plus the grey no-data arm.
    #[test]
    fn classify_a_continuous_ramp_over_10k_random_incomes_stays_bounded() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let feats: Vec<String> = (0..10_000)
            .map(|i| {
                let r = next();
                let properties = if r % 10 == 0 {
                    String::new()
                } else {
                    format!(r#""income":{}"#, 2_500.0 + (r >> 11) as f64 / (1u64 << 53) as f64 * 247_500.0)
                };
                format!(
                    r#"{{"type":"Feature","id":{i},"properties":{{{properties}}},"geometry":{{"type":"Polygon","coordinates":[[[0,0],[0.1,0],[0.1,0.1],[0,0.1],[0,0]]]}}}}"#
                )
            })
            .collect();
        let json = format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, feats.join(","));
        let mut store = crate::overlay::OverlayStore::new();
        let paint = serde_json::json!({
            "fill-color": ["case", ["has", "income"],
                ["interpolate", ["linear"], ["get", "income"],
                    2500, "#440154", 33437.5, "#472d7b", 64375, "#3b528b", 95312.5, "#2c728e",
                    126250, "#21918c", 157187.5, "#28ae80", 188125, "#5ec962", 219062.5, "#addc30",
                    250000, "#fde725"],
                "#9e9e9e"],
            "fill-outline-color": "rgba(20,20,40,0.35)",
            "fill-opacity": 0.5
        });
        store.declare_source_paint("s", paint.as_object().unwrap()).unwrap();
        store.set_source("s", &json, 3).unwrap();
        let snap = store.source_snapshot("s").unwrap();
        let c = classify(&snap, BakeBracket::Bucket(3), &snap.paint).unwrap();
        assert_eq!(c.of_feature.len(), 10_000);
        assert!(c.classes.len() <= 257, "{} classes", c.classes.len());
        assert!(c.classes.len() > 200, "the ramp is really continuous: {} classes", c.classes.len());
    }
}
