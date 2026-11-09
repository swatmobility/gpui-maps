use lyon::math::Point;
use lyon::path::polygon::Polygon as LyonPolygon;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, FillVertexConstructor,
    LineCap, LineJoin, StrokeOptions, StrokeTessellator, StrokeVertex, StrokeVertexConstructor,
    VertexBuffers,
};

use crate::render::run_split;
use crate::render::subdivide::{self, SubdivideScratch, Subdivision};
use crate::style::{DashPattern, PaintOp, PaintPlan, Rgba};

/// The style's `line-cap` in lyon's own vocabulary.
///
/// A free function rather than a method on `style::LineCap` so the mapping
/// lives next to the tessellator that is the only thing that needs it -- and
/// so `style.rs` keeps knowing nothing about lyon.
fn lyon_cap(cap: crate::style::LineCap) -> LineCap {
    match cap {
        crate::style::LineCap::Butt => LineCap::Butt,
        crate::style::LineCap::Round => LineCap::Round,
        crate::style::LineCap::Square => LineCap::Square,
    }
}

/// The style's `line-join` in lyon's own vocabulary. Sibling of `lyon_cap`.
///
/// lyon has a fourth variant, `MiterClip`, with no style-spec counterpart:
/// it truncates an over-long miter to a flat edge at the limit instead of
/// falling back to a bevel. Mapbox falls back to a bevel
/// (`line_bucket.ts`: `if (currentJoin === 'miter' && miterLength >
/// miterLimit) currentJoin = 'bevel'`), so `Miter` -- which is lyon's
/// bevel-falling-back variant -- is the faithful mapping, not `MiterClip`.
fn lyon_join(join: crate::style::LineJoin) -> LineJoin {
    match join {
        crate::style::LineJoin::Bevel => LineJoin::Bevel,
        crate::style::LineJoin::Round => LineJoin::Round,
        crate::style::LineJoin::Miter => LineJoin::Miter,
    }
}

/// MapLibre's `line-miter-limit` default of 2, expressed in lyon's units.
///
/// The factor of two is real and is derived in `style::LineJoin`'s doc
/// comment: lyon measures the miter against the FULL stroke width and bevels
/// at `normal.length() > 2 * miter_limit`, while Mapbox measures it in
/// half-widths and bevels at `miterLength > miterLimit`. Leaving lyon's own
/// default of 4.0 in place would let a hairpin's spike reach four times as
/// far as Mapbox draws it.
///
/// It also keeps `MAX_NORMAL_LEN`'s clamp off the join path entirely: with
/// this limit no miter normal lyon emits can exceed length 2, so the clamp
/// at 4 stays what it was written to be -- a backstop for the CONCAVE side,
/// which lyon does not bound at all.
const MITER_LIMIT: f32 = 1.0;

/// Flattening tolerance for the stroke tessellator, in lyon units -- how far
/// a round join's or round cap's polyline may sag from the true arc.
///
/// This is a COST constant, not a quality one, and it is derived from
/// MapLibre rather than picked. MapLibre subdivides a round join into
/// `round(angle_degrees / DEG_PER_TRIANGLE)` slices with `DEG_PER_TRIANGLE =
/// 20` (`line_bucket.ts`), i.e. one triangle per 20 degrees of turn, and
/// that is the whole of how much geometry a round join costs it. lyon
/// instead subdivides until the sagitta is under `tolerance`, measured
/// against the configured half-width -- `STROKE_TESSELLATION_WIDTH / 2`, so
/// 50 here. Setting the two equal:
///
/// ```text
/// tolerance = r * (1 - cos(theta / 2)) = 50 * (1 - cos(10 deg)) = 0.76
/// ```
///
/// The old value was `0.1`, which predates any round join reaching this
/// function: with `LineJoin::Bevel` hardcoded, tolerance affected nothing
/// but the overlay's marker dots. Now that `layout.line-join` is honoured
/// -- and 44 of Liberty's line layers and 16 of
/// `examples/mapbox-style.json`'s ask for `"round"` -- it governs the size
/// of every road's every corner, and `0.1` is roughly 7 degrees per
/// triangle: measured over a 1000-point zig-zag it produced up to 34
/// vertices per input point at near-hairpin turns, against bevel's 3. At
/// `0.76` the same sweep peaks at 10 and sits at 5-6 for the gentle turns
/// real road geometry is mostly made of -- within one triangle of what
/// MapLibre itself emits, and drawing an arc whose worst-case sag at a
/// 10-pixel half-width is 0.15 px.
const STROKE_TOLERANCE: f32 = 0.76;

/// Lyon units per source coordinate unit: every geometry coordinate (MVT tile
/// units for a tile, local grid units for an overlay) is multiplied by this
/// before it reaches lyon, and the model matrix divides it back out.
///
/// `pub(crate)` so `render::camera::overlay_dash_scale` can derive the
/// overlay's lyon-unit size from this constant rather than restating the
/// literal. (`dash_scale` and the two model-matrix builders in that module
/// still spell it out inline; converting those is an unrelated tidy-up.)
pub(crate) const COORD_SCALE: f32 = 1000.0;
/// The overlay's coordinate scale: none.
///
/// `COORD_SCALE` exists so lyon gets sub-integer resolution within a tile,
/// whose coordinates are integers 0..4096. The overlay's local grid is
/// already 1e7 units per world unit -- one unit is 7.8 mm -- so scaling it
/// buys resolution nothing can display, at the cost of three orders of
/// magnitude of coordinate range. That range is what destroyed its stroke
/// normals: see the spec.
pub(crate) const OVERLAY_COORD_SCALE: f32 = 1.0;
/// Bounds the stroke extrusion normal's length so unbounded miter spikes on
/// the concave side of sharp turns (lyon's `StrokeVertex::normal()` is not
/// clamped by `miter_limit` there) can't blow up into visible artifacts.
const MAX_NORMAL_LEN: f32 = 4.0;
/// A FIXED width, not `width_from` -- lyon's own extrusion never reaches
/// the screen (see below), so the value handed to `with_line_width` is
/// otherwise inert for rendering, but it is NOT inert for lyon
/// internally, and NOT only in the small way that first motivated this
/// fix. The width feeds `StrokeTessellator`'s internal
/// `square_merge_threshold` (`min(tolerance^2 * 0.5, line_width^2 *
/// 0.05)`, clamped above 1e-8), which governs vertex merging, and
/// separately feeds each join's fold/no-fold decision: for a turn
/// sharper than 90 degrees under `LineJoin::Bevel`, lyon projects the
/// join's "back vertex" extrusion (built from the configured
/// half-width) onto the adjacent edges and OMITS that vertex ("folds"
/// the join) if the projection would overshoot the edge -- a decision
/// made from the configured width, not the path alone. So the TOPOLOGY
/// lyon produces (vertex count, whether a join vertex exists at all)
/// genuinely CAN depend on the configured width, on top of the smaller
/// (~1e-6, last-two-mantissa-bits in the common case, larger under f32
/// cancellation at large tile-scale coordinates) floating-point noise
/// in `normal` this fix was first found to require -- see
/// `rewriting_paint_attributes_matches_a_full_retessellation_at_the_new_
/// zoom` below for the general cache-equivalence property, and
/// `gpu::tests::sharp_turn_wide_line_join_normals_are_independent_of_
/// declared_width` for a sharp turn specifically (that test pins exact
/// `normal` equality across widths at large coordinates; it does NOT
/// reach the fold/no-fold branch above -- see its own doc comment for
/// why -- so the fold case itself is undemonstrated by any test today).
/// Regardless of which mechanism is in play for a given path, this must
/// be FIXED, not merely "some noise to tolerate":
/// `tessellate_plan_with_ranges`/`rewrite_paint_attributes`'s cache
/// invariant requires a rewrite at the new zoom to reproduce EXACTLY
/// the same vertex layout a fresh tessellation at that zoom
/// would produce -- true only if the geometry lyon emits is a pure
/// function of the path, independent of the style's actual
/// (zoom-dependent) `width_from`/`width_to`. What actually determines
/// on-screen width is `half_width_from`/`half_width_to` (below), carried
/// per-vertex into the mesh and consumed by the vertex shader together
/// with the zoom uniform to extrude each stroke vertex by the real
/// per-layer width.
/// ANY fixed value satisfies the cache-determinism invariant above
/// (topology independent of the style's actual, zoom-dependent width) --
/// that invariant does not by itself pick a number. What it does pick
/// between is two competing failure modes as the fixed width varies:
/// normal-vector PRECISION (small width -> larger relative f32 error in
/// lyon's emitted normals) versus JOIN-TOPOLOGY FIDELITY (large width ->
/// risk of tripping lyon's Bevel-join fold branch, which omits a join's
/// "back vertex" when the configured half-width is large enough,
/// relative to the path's own segment lengths, to overshoot the
/// adjacent edge on projection). `100.0` is chosen to satisfy both.
///
/// Precision: lyon computes `normal` as `(side_point - position) /
/// half_width` in f32, at tile coordinates up to `4096 * COORD_SCALE`
/// (~4.1e6). A `1.0` width (half-width 0.5) puts that division at the
/// f32 ULP for these coordinates -- far-corner normals came out as
/// exactly `(0.5, -1.0)` where the true value is `(0.447, -0.894)`, a
/// 7-12% per-vertex magnitude error (worst measured: 7.5% at MVT coord
/// ~512, 9.9% at ~2048, 11.8% at ~4000) plus a few degrees of direction
/// error, across roughly seven-eighths of every tile. `100.0`
/// (half-width 50) drops the worst measured error to ~0.15%; `1000.0`
/// (half-width 500, `COORD_SCALE`) does slightly better still
/// (~0.004%-0.017%) but at the cost below.
///
/// Topology: the fold branch fires when segments are short enough,
/// relative to the configured half-width, for lyon's back-vertex
/// projection to overshoot -- concretely, in the regime where
/// half-width approaches the ~1000-lyon-unit minimum MVT segment length
/// (`coord` values are scaled by `COORD_SCALE` before reaching lyon, so
/// a 1-MVT-unit segment becomes `COORD_SCALE` lyon units). `1000.0`'s
/// half-width of 500 sits just under that minimum -- precisely the
/// fold-branch danger zone this constant exists to avoid. `100.0`'s
/// half-width of 50 is 20x below that minimum, well clear of it, so the
/// fold branch stays unreachable exactly as it was pre-
/// `STROKE_TESSELLATION_WIDTH` (when real style widths of 0.25-10 units
/// left three-plus orders of magnitude of headroom).
///
/// `100.0` also keeps `square_merge_threshold` in the same regime as
/// before: `min(tolerance^2 * 0.5, w^2 * 0.05)` = `min(0.005, 0.05 *
/// w^2)`, and `0.05 * w^2 >= 0.005` (so the `tolerance` term still wins
/// the `min`, unchanged) for any `w >= 0.316` -- `100.0` clears that
/// with room to spare. See
/// `sharp_turn_wide_line_join_normals_are_independent_of_declared_width`:
/// unaffected by this change (it does not reach the fold branch either
/// way -- see its own doc comment), still pins the same vertex
/// count/topology before and after.
pub(crate) const STROKE_TESSELLATION_WIDTH: f32 = 100.0;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 2],
    pub normal: [f32; 2],
    pub color: [f32; 4],
    /// 0.0 for fill vertices, 1.0 for stroke vertices — lets one shared vertex
    /// shader branch cheaply instead of needing two separate vertex layouts.
    pub is_stroke: f32,
    /// Half the style-declared line width, in screen pixels. Unused (0.0)
    /// for fill vertices. Consumed by the vertex shader to build a small
    /// extrusion vector in WORLD space (`normal * half_width_px /
    /// 2^zoom`), which is then projected through `camera.view_proj` as a
    /// direction and added to the already-projected clip-space position
    /// (see `shader.wgsl`'s `vs_main`) -- so the real per-layer width ends
    /// up on screen, instead of a hardcoded constant, and the extrusion
    /// naturally picks up the camera's bearing/perspective the way
    /// MapLibre/Mapbox GL JS's line shader does.
    pub half_width_px: f32,
    /// The same property evaluated one zoom level higher than `color`
    /// (`ZoomValue::value_at_pair`'s second element). The vertex shader
    /// blends `color`/`color_to` by `fract(camera.zoom)` every frame,
    /// giving continuous zoom-dependent paint with zero CPU cost per
    /// frame -- matching MapLibre's `CompositeExpressionBinder` pattern.
    pub color_to: [f32; 4],
    /// Paired with `half_width_px` the same way `color_to` pairs with
    /// `color`. Unused (0.0) for fill vertices, same as `half_width_px`.
    pub half_width_px_to: f32,
    /// HALF the style-declared `line-gap-width`, in screen pixels -- the
    /// distance from the centreline to the INNER edge of the stroke.
    ///
    /// Zero (the overwhelmingly common case, and every fill vertex) means a
    /// solid line and costs nothing: `shader.wgsl` then computes exactly the
    /// outset it computed before this field existed, and the inner cut is a
    /// no-op. Non-zero turns the stroke into a CASING: the band it covers
    /// runs from `gap_half_px` to `gap_half_px + full width` on each side of
    /// the centreline, with the middle punched out in `fs_main` -- MapLibre's
    /// own `inset`/`outset` pair (`line.vertex.glsl`), not an approximation
    /// of it. The geometry lyon emits is untouched; only how far the vertex
    /// shader extrudes it, and where the fragment shader stops drawing,
    /// change.
    ///
    /// ZOOM-DEPENDENT: paint data, like `color`/`half_width_px`.
    /// `rewrite_paint_attributes` MUST update it.
    pub gap_half_px: f32,
    /// Paired with `gap_half_px` the way `half_width_px_to` pairs with
    /// `half_width_px`: the same property one zoom level up, blended by
    /// `model.blend_t` in the vertex shader.
    pub gap_half_px_to: f32,
    /// Distance along the path at this vertex, in lyon input units (MVT
    /// coordinates x `COORD_SCALE`), straight from
    /// `StrokeVertex::advancement()`. 0.0 for fill vertices.
    ///
    /// ZOOM-INDEPENDENT: geometry-derived, like `position`/`normal`/
    /// `is_stroke`. `rewrite_paint_attributes` must never touch it.
    ///
    /// Because `StrokeCtor` reads `position_on_path()` -- the centreline
    /// point -- both extruded sides of a segment share one advancement, so
    /// the rasterizer interpolates true distance along the line.
    pub line_distance: f32,
    /// The resolved `line-dasharray` runs, in units of line width. All zeros
    /// means not dashed.
    ///
    /// ZOOM-DEPENDENT: paint data, like `color`/`half_width_px`.
    /// `rewrite_paint_attributes` MUST update it.
    ///
    /// SHADER-COUPLED INVARIANT: no run count is sent alongside these four
    /// floats. `shader.wgsl`'s `dash_signed_distance` instead lets a shorter
    /// pattern's trailing zeros degenerate its boundary math -- which is
    /// sound only because `style::DashPattern::from_runs` guarantees every
    /// live run is STRICTLY positive (zero-length runs are collapsed at
    /// construction), so a zero here can only ever be trailing padding.
    /// Relaxing `DashPattern` to permit an interior zero would silently
    /// corrupt the mask, and is a shader-breaking change.
    pub dash_runs: [f32; 4],
    /// 0..1 along this vertex's LineString when its op was built with line
    /// metrics; -1.0 otherwise.
    ///
    /// MapLibre's `lineMetrics` progress (`line_bucket.ts`: `lineSoFar /
    /// totalDistance`, carried to `line_gradient.vertex.glsl` as
    /// `v_lineprogress`): `line_distance` divided by the LineString's total
    /// length, so it is 0 at the first point and 1 at the last, per
    /// LineString of a MultiLineString. Every fill vertex, and every stroke
    /// whose `PaintOp::Line::metrics` is false, carries `-1.0`, which
    /// `shader.wgsl`'s trim treats as "not trimmable".
    ///
    /// APPENDED LAST (location 11), so every existing field's byte offset
    /// is unchanged; the vertex grows from 88 to 92 bytes.
    ///
    /// ZOOM-INDEPENDENT: geometry-derived, like `line_distance`.
    /// `rewrite_paint_attributes` must never touch it.
    pub line_progress: f32,
}

#[derive(Debug, Clone, Default)]
pub struct TileMesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

impl TileMesh {
    /// Every heap block this triangulation holds, in
    /// [`crate::render::heap_block_bytes`]' unit -- the weight
    /// `MapView`'s `mesh_cache` and `prefetched_meshes` are bounded by
    /// (together with their `Vec<OpSpan>`; see `map_view::CachedMesh`).
    ///
    /// # This is the biggest per-tile cost in the renderer, by far
    ///
    /// On the real dense Tokyo z14 tile (14/14550/6455) this is **64.97
    /// MB**: 590,911 vertices at [`Vertex`]'s 88 bytes, plus 1,487,301
    /// `u32` indices. That is 5.6x what the same tile's decoded MVT costs
    /// (11.81 MB) and 2.2x what the decoded tile, its `TileMatch`, its
    /// `PaintPlan` and its `SymbolPlan` cost put together.
    ///
    /// It is also 63x the figure the caches holding it were sized against.
    /// `MESH_CACHE_CAPACITY`'s and `PREFETCHED_MESH_CACHE_CAPACITY`'s doc
    /// comments both justify 128 entries as "~132 MB" from a measured
    /// ~1.03 MB per `CachedMesh` -- taken on a SYNTHETIC ~1,600-feature
    /// tile, at a time when `Vertex` was 60 bytes rather than 88. The real
    /// tile has 25,264 features and produces 49,645 ops, so 128 entries is
    /// 8.3 GB and not 132 MB. Nothing drifted quietly: the per-entry
    /// figure was measured once, on a stand-in, and the bound was never
    /// re-derived when the real data arrived. A bound in bytes cannot go
    /// stale that way, which is the whole reason this method exists.
    ///
    /// # Two figures, and this is the CPU one
    ///
    /// This counts what the CPU heap holds. The same vertices and indices
    /// are also uploaded, and the device-side copy is
    /// `vertices.len() * size_of::<Vertex>() + indices.len() * 4` -- 57.95
    /// MB for that tile, without the `OpSpan`s (which never leave the CPU)
    /// and without capacity slack (`install_mesh` uploads the used range).
    /// `gpu_meshes` is budgeted against that second figure; see
    /// `map_view::CachedTile::device_bytes`.
    ///
    /// Capacity and not length, for both `Vec`s: `append_ops` grows them by
    /// pushing, so the capacity is what the allocator is holding and the
    /// length is merely what is in use. The gap is real -- it is why this
    /// figure tracks the heap to within 0.1% on the dense tile, and why a
    /// length-based sum would not.
    #[must_use]
    pub fn footprint_bytes(&self) -> usize {
        crate::render::vec_block_bytes::<Vertex>(self.vertices.capacity())
            + crate::render::vec_block_bytes::<u32>(self.indices.capacity())
    }
}

/// Where one `PaintOp`'s output lives inside a `TileMesh`.
///
/// `vertices` is what `rewrite_paint_attributes` walks to overwrite an op's
/// paint fields in place. `indices` exists for `splice_changed_ops`, which
/// has to cut a run of ops out of BOTH buffers and stitch replacements in;
/// a vertex range alone cannot locate the triangles that reference it.
///
/// Both ranges are half-open and may be empty (`start == end`) --
/// `PaintOp::Background` emits nothing, and a degenerate geometry can
/// tessellate to nothing -- so an op is never skipped and `spans[i]` always
/// corresponds to `plan.ops[i]` by index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpSpan {
    pub vertices: std::ops::Range<usize>,
    pub indices: std::ops::Range<usize>,
}

fn to_lyon_point(coord: (i32, i32), coord_scale: f32) -> Point {
    lyon::math::point(coord.0 as f32 * coord_scale, coord.1 as f32 * coord_scale)
}

/// Fills `out` (cleared first) with `line_string`'s points converted to lyon
/// units, dropping a duplicated closing point if the ring repeats its first
/// coordinate as its last.
///
/// `out` is caller-owned scratch: `append_fill` reuses the same `Vec` across
/// every ring of every op in a tile rather than allocating one per ring
/// (`append_ops` hoists it, exactly as it hoists the tessellators below).
/// Reads `line_string.0` directly instead of collecting through
/// `.coords()` into an intermediate `Vec<(i32, i32)>` first, which is what
/// the per-ring allocation used to be doubled: one intermediate buffer to
/// find the duplicated closing point, one more to hold the converted
/// `Point`s.
pub(crate) fn ring_points(line_string: &geo_types::LineString<i32>, coord_scale: f32, out: &mut Vec<Point>) {
    out.clear();
    let coords = &line_string.0;
    let n = coords.len();
    let take = if n > 1 && coords[0] == coords[n - 1] { n - 1 } else { n };
    out.extend(coords[..take].iter().map(|c| to_lyon_point((c.x, c.y), coord_scale)));
}

fn signed_area2(ring: &[Point]) -> f32 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    (0..n)
        .map(|i| {
            let p = ring[i];
            let q = ring[(i + 1) % n];
            p.x * q.y - q.x * p.y
        })
        .sum()
}

pub(crate) fn force_winding(ring: &mut [Point], want_positive: bool) {
    if (signed_area2(ring) > 0.0) != want_positive {
        ring.reverse();
    }
}

struct FillCtor {
    color_from: [f32; 4],
    color_to: [f32; 4],
}

impl FillVertexConstructor<Vertex> for FillCtor {
    fn new_vertex(&mut self, vertex: FillVertex) -> Vertex {
        let p = vertex.position();
        Vertex {
            position: [p.x, p.y],
            normal: [0.0, 0.0],
            color: self.color_from,
            is_stroke: 0.0,
            half_width_px: 0.0,
            color_to: self.color_to,
            half_width_px_to: 0.0,
            gap_half_px: 0.0,
            gap_half_px_to: 0.0,
            line_distance: 0.0,
            dash_runs: [0.0; 4],
            line_progress: -1.0,
        }
    }
}

/// `Copy` because a fresh `BuffersBuilder` is constructed per run and per
/// dot (see `append_stroke`) -- the constructor is stateless plain data, so
/// handing each builder its own copy costs nothing.
#[derive(Clone, Copy)]
struct StrokeCtor {
    color_from: [f32; 4],
    color_to: [f32; 4],
    half_width_from: f32,
    half_width_to: f32,
    /// Half `line-gap-width` at the baked zoom and one zoom up. See
    /// `Vertex::gap_half_px`.
    gap_half_from: f32,
    gap_half_to: f32,
    dash_runs: [f32; 4],
}

impl StrokeVertexConstructor<Vertex> for StrokeCtor {
    fn new_vertex(&mut self, vertex: StrokeVertex) -> Vertex {
        let p = vertex.position_on_path();
        let mut n = vertex.normal();
        let len = n.length();
        if len > MAX_NORMAL_LEN {
            n *= MAX_NORMAL_LEN / len;
        }
        Vertex {
            position: [p.x, p.y],
            normal: [n.x, n.y],
            color: self.color_from,
            is_stroke: 1.0,
            half_width_px: self.half_width_from,
            color_to: self.color_to,
            half_width_px_to: self.half_width_to,
            gap_half_px: self.gap_half_from,
            gap_half_px_to: self.gap_half_to,
            line_distance: vertex.advancement(),
            dash_runs: self.dash_runs,
            // `append_stroke` overwrites this with the real progress once a
            // LineString with metrics is complete; without metrics it stays.
            line_progress: -1.0,
        }
    }
}

/// `tess` and `ring_scratch` are caller-owned: `append_ops` constructs one
/// `FillTessellator` and one ring buffer for the whole tile and threads them
/// through every `Fill` op, rather than each op building its own
/// tessellator from empty (`FillTessellator::new()` allocates its event
/// queue and active-edge storage) and its own per-ring `Vec`s. Safe because
/// lyon's `FillTessellator` carries no state across `tessellate_*` calls
/// that affects output -- its fields are exactly the CPU working buffers
/// (`events`, `active`, `edges_below`, `fill`, `scan`, `attrib_buffer`) that
/// this pooling is meant to stop reallocating; `tessellate()`
/// (`lyon_tessellation-1.0.21/src/fill.rs:577`) rebuilds `events` fresh from
/// the input path every call via `mem::take` + `into_builder`, and
/// `fill_rule`/`tolerance` are retaken from `options` on every call. See
/// `pooled_tessellators_match_fresh_ones_byte_for_byte` for the proof.
///
/// `ring_scratch` holds one ring at a time: `add_polygon`
/// (`lyon_path-1.0.19/src/builder.rs:560`) copies a ring's points into the
/// tessellator's path immediately (`begin`/`line_to`/`end`), so the buffer
/// is free to be cleared and refilled for the next ring -- exterior, then
/// each interior in turn -- without needing a `Vec<Vec<Point>>` to hold
/// them all live at once, which is what the original per-polygon
/// `interiors` collection was doing.
fn append_fill(
    buffers: &mut VertexBuffers<Vertex, u32>,
    geometry: &geo_types::Geometry<i32>,
    color_from: Rgba,
    color_to: Rgba,
    coord_scale: f32,
    tess: &mut FillTessellator,
    ring_scratch: &mut Vec<Point>,
) {
    use geo_types::Geometry::*;
    let polygons: Vec<&geo_types::Polygon<i32>> = match geometry {
        Polygon(p) => vec![p],
        MultiPolygon(mp) => mp.iter().collect(),
        _ => return,
    };

    let options = FillOptions::tolerance(0.1).with_fill_rule(FillRule::NonZero);
    let color_from = [color_from.r, color_from.g, color_from.b, color_from.a];
    let color_to = [color_to.r, color_to.g, color_to.b, color_to.a];
    let mut builder_buffers = BuffersBuilder::new(buffers, FillCtor { color_from, color_to });

    for polygon in polygons {
        ring_points(polygon.exterior(), coord_scale, ring_scratch);
        if ring_scratch.len() < 3 {
            continue;
        }
        force_winding(ring_scratch, true);

        let mut builder = tess.builder(&options, &mut builder_buffers);
        builder.add_polygon(LyonPolygon { points: ring_scratch.as_slice(), closed: true });
        for ring in polygon.interiors() {
            ring_points(ring, coord_scale, ring_scratch);
            if ring_scratch.len() < 3 {
                continue;
            }
            force_winding(ring_scratch, false);
            builder.add_polygon(LyonPolygon { points: ring_scratch.as_slice(), closed: true });
        }
        if let Err(err) = builder.build() {
            eprintln!("gpui-maps: fill tessellation failed for a feature, skipping: {err:?}");
        }
    }
}

/// Packs a fill-outline colour into the fill vertex's stroke-only slots:
/// `normal` = (r, g), `gap_half_px` = b, `gap_half_px_to` = a.
///
/// A fill vertex never reads any of the three -- `shader.wgsl`'s
/// `vertex_main` touches `normal` and the gaps only inside its
/// `is_stroke > 0.5` branch -- so the fill draws exactly as before, and
/// `vs_outline` reads them back. That is what lets the outline reuse the
/// fill's vertices with no attribute and no varying added.
pub(crate) fn pack_outline_color(vertex: &mut Vertex, outline: Rgba) {
    vertex.normal = [outline.r, outline.g];
    vertex.gap_half_px = outline.b;
    vertex.gap_half_px_to = outline.a;
}

/// A fill constructor that also records which vertex each ring endpoint
/// became, so the outline can index the fill's OWN vertices.
struct OutlineFillCtor<'a> {
    color_from: [f32; 4],
    color_to: [f32; 4],
    outline: Option<Rgba>,
    /// The index the next vertex will have in the whole buffer.
    next: u32,
    endpoint_vertex: &'a mut Vec<u32>,
}

impl FillVertexConstructor<Vertex> for OutlineFillCtor<'_> {
    fn new_vertex(&mut self, vertex: FillVertex) -> Vertex {
        let p = vertex.position();
        let index = self.next;
        self.next += 1;
        if self.outline.is_some() {
            for source in vertex.sources() {
                if let lyon::tessellation::VertexSource::Endpoint { id } = source {
                    let at = id.to_usize();
                    if self.endpoint_vertex.len() <= at {
                        self.endpoint_vertex.resize(at + 1, u32::MAX);
                    }
                    self.endpoint_vertex[at] = index;
                }
            }
        }
        overlay_fill_vertex([p.x, p.y], self.color_from, self.color_to, self.outline)
    }
}

/// One overlay fill vertex, as [`OutlineFillCtor`] makes it.
fn overlay_fill_vertex(position: [f32; 2], color_from: [f32; 4], color_to: [f32; 4], outline: Option<Rgba>) -> Vertex {
    let mut out = Vertex {
        position,
        normal: [0.0, 0.0],
        color: color_from,
        is_stroke: 0.0,
        half_width_px: 0.0,
        color_to,
        half_width_px_to: 0.0,
        gap_half_px: 0.0,
        gap_half_px_to: 0.0,
        line_distance: 0.0,
        dash_runs: [0.0; 4],
        line_progress: -1.0,
    };
    if let Some(outline) = outline {
        pack_outline_color(&mut out, outline);
    }
    out
}

/// `[x0, y0, x1, y1]` when `polygon` is a hole-free, axis-aligned
/// rectangle with area: a closed ring of exactly four distinct corners in
/// order around the box (`c[0]` and `c[2]` opposite, `c[1]` and `c[3]` the
/// other two), so its edges alternate horizontal and vertical. The tile
/// index's tiny-polygon squares, and any rectangle a source happens to
/// hold. A ring that only LOOKS axis-aligned -- `(0,0) (10,0) (10,5)
/// (10,0) (0,0)` doubles back and has no area -- is not one, and goes to
/// lyon, which draws nothing for it.
fn axis_rect(polygon: &geo_types::Polygon<i32>) -> Option<[i32; 4]> {
    if !polygon.interiors().is_empty() {
        return None;
    }
    let c = &polygon.exterior().0;
    if c.len() != 5 || c[0] != c[4] {
        return None;
    }
    let (a, b) = (c[0], c[2]);
    if a.x == b.x || a.y == b.y {
        return None;
    }
    let (p, q) = (geo_types::coord! { x: b.x, y: a.y }, geo_types::coord! { x: a.x, y: b.y });
    if !((c[1] == p && c[3] == q) || (c[1] == q && c[3] == p)) {
        return None;
    }
    Some([a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y)])
}

/// Where one overlay triangulation UNIT ends: a fill polygon, or a whole
/// non-fill op. No triangle and no outline segment crosses a unit
/// boundary, so a chunk may start at any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnitCut {
    pub vertices: usize,
    pub indices: usize,
    pub outline: usize,
}

/// What an overlay bake produces beside its mesh.
///
/// - `outline`: LINE-LIST indices over the mesh's own vertices, two per
///   ring edge, for every fill op with a `fill-outline-color` -- MapLibre's
///   `FillBucket.indexArray2` (`fill_bucket.ts:51,57,71`). No second
///   tessellation: the hairline is one more index buffer.
/// - `cuts`: the end of every unit, in order. [`plan_overlay_chunks`]
///   groups units into chunks of at most 65,536 vertices.
/// - `outline_min_extent`: the outline's level of detail, an INPUT to the
///   bake. A polygon whose exterior's bounding box is under this many
///   input units in BOTH axes keeps its fill but records no outline edge.
///   0 (the default) keeps every outline. See
///   `overlay::tile_plan::outline_min_extent_units`.
#[derive(Debug, Clone, Default)]
pub(crate) struct OverlayExtras {
    pub outline: Vec<u32>,
    pub cuts: Vec<UnitCut>,
    pub outline_min_extent: i32,
    endpoint_vertex: Vec<u32>,
    ring_lens: Vec<usize>,
}

impl OverlayExtras {
    fn close_unit(&mut self, buffers: &VertexBuffers<Vertex, u32>) {
        let cut = UnitCut {
            vertices: buffers.vertices.len(),
            indices: buffers.indices.len(),
            outline: self.outline.len(),
        };
        if self.cuts.last().is_none_or(|last| *last != cut) {
            self.cuts.push(cut);
        }
    }

    /// Frees the working scratch, keeping what the upload reads.
    fn finish(&mut self) {
        self.endpoint_vertex = Vec::new();
        self.ring_lens = Vec::new();
    }

    /// Heap bytes held, by capacity.
    #[cfg(test)]
    pub(crate) fn footprint_bytes(&self) -> usize {
        crate::render::vec_block_bytes::<u32>(self.outline.capacity())
            + crate::render::vec_block_bytes::<UnitCut>(self.cuts.capacity())
    }
}

/// The overlay's fill: [`append_fill`], one polygon at a time, each
/// subdivided on its own and closed as a unit, with its outline recorded.
#[allow(clippy::too_many_arguments)]
fn append_fill_overlay(
    buffers: &mut VertexBuffers<Vertex, u32>,
    geometry: &geo_types::Geometry<i32>,
    color_from: Rgba,
    color_to: Rgba,
    outline: Option<Rgba>,
    coord_scale: f32,
    extent: u32,
    subdivision: Subdivision,
    tess: &mut FillTessellator,
    ring_scratch: &mut Vec<Point>,
    subdivide_scratch: &mut SubdivideScratch,
    extras: &mut OverlayExtras,
) {
    use geo_types::Geometry::*;
    let polygons: Vec<&geo_types::Polygon<i32>> = match geometry {
        Polygon(p) => vec![p],
        MultiPolygon(mp) => mp.iter().collect(),
        _ => return,
    };
    let options = FillOptions::tolerance(0.1).with_fill_rule(FillRule::NonZero);
    let color_from = [color_from.r, color_from.g, color_from.b, color_from.a];
    let color_to = [color_to.r, color_to.g, color_to.b, color_to.a];

    for polygon in polygons {
        // An axis-aligned rectangle is two triangles over its own four
        // corners: written directly, it covers exactly what lyon's
        // triangulation of it covers, without a tessellator pass (the
        // national cover's ~46k tiny-polygon squares, `TinyArea`).
        if let Some([x0, y0, x1, y1]) = axis_rect(polygon) {
            let vertex_start = buffers.vertices.len();
            let index_start = buffers.indices.len();
            let v = vertex_start as u32;
            for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
                let p = to_lyon_point((x, y), coord_scale);
                buffers.vertices.push(overlay_fill_vertex([p.x, p.y], color_from, color_to, outline));
            }
            buffers.indices.extend_from_slice(&[v, v + 1, v + 2, v, v + 2, v + 3]);
            subdivide::subdivide_fill(
                buffers,
                vertex_start,
                index_start,
                subdivision,
                extent,
                coord_scale,
                subdivide_scratch,
            );
            if outline.is_some() && polygon_reaches(polygon, extras.outline_min_extent) {
                for i in 0..4u32 {
                    extras.outline.push(v + i);
                    extras.outline.push(v + (i + 1) % 4);
                }
            }
            extras.close_unit(buffers);
            continue;
        }
        ring_points(polygon.exterior(), coord_scale, ring_scratch);
        if ring_scratch.len() < 3 {
            continue;
        }
        force_winding(ring_scratch, true);
        let vertex_start = buffers.vertices.len();
        let index_start = buffers.indices.len();
        extras.endpoint_vertex.clear();
        extras.ring_lens.clear();
        {
            let ctor = OutlineFillCtor {
                color_from,
                color_to,
                outline,
                next: vertex_start as u32,
                endpoint_vertex: &mut extras.endpoint_vertex,
            };
            let mut builder_buffers = BuffersBuilder::new(buffers, ctor);
            let mut builder = tess.builder(&options, &mut builder_buffers);
            builder.add_polygon(LyonPolygon { points: ring_scratch.as_slice(), closed: true });
            extras.ring_lens.push(ring_scratch.len());
            for ring in polygon.interiors() {
                ring_points(ring, coord_scale, ring_scratch);
                if ring_scratch.len() < 3 {
                    continue;
                }
                force_winding(ring_scratch, false);
                builder.add_polygon(LyonPolygon { points: ring_scratch.as_slice(), closed: true });
                extras.ring_lens.push(ring_scratch.len());
            }
            if let Err(err) = builder.build() {
                eprintln!("gpui-maps: fill tessellation failed for a feature, skipping: {err:?}");
            }
        }
        // Per polygon, so the new vertices land right after this polygon's
        // own and no triangle reaches back into an earlier unit. The cut
        // positions are the op-wide pass's; only cross-polygon vertex
        // sharing along a cell line is lost, which moves nothing on screen.
        subdivide::subdivide_fill(
            buffers,
            vertex_start,
            index_start,
            subdivision,
            extent,
            coord_scale,
            subdivide_scratch,
        );
        if outline.is_some() && polygon_reaches(polygon, extras.outline_min_extent) {
            // Ring edges over the ORIGINAL ring vertices: subdivision only
            // appends vertices and rewrites triangles, never moves these.
            let map = &extras.endpoint_vertex;
            let vertex_of = |endpoint: usize| map.get(endpoint).copied().filter(|&v| v != u32::MAX);
            let mut first = 0usize;
            for &len in &extras.ring_lens {
                for i in 0..len {
                    let (a, b) = (first + i, first + (i + 1) % len);
                    if let (Some(va), Some(vb)) = (vertex_of(a), vertex_of(b)) {
                        if va != vb {
                            extras.outline.push(va);
                            extras.outline.push(vb);
                        }
                    }
                }
                first += len;
            }
        }
        extras.close_unit(buffers);
    }
}

/// Whether `polygon`'s exterior spans at least `min_extent` input units in
/// either axis: the outline level of detail (`OverlayExtras::outline_min_extent`).
fn polygon_reaches(polygon: &geo_types::Polygon<i32>, min_extent: i32) -> bool {
    if min_extent <= 0 {
        return true;
    }
    let mut coords = polygon.exterior().0.iter();
    let Some(first) = coords.next() else { return false };
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x, first.y);
    for c in coords {
        x0 = x0.min(c.x);
        x1 = x1.max(c.x);
        y0 = y0.min(c.y);
        y1 = y1.max(c.y);
        if x1 - x0 >= min_extent || y1 - y0 >= min_extent {
            return true;
        }
    }
    x1 - x0 >= min_extent || y1 - y0 >= min_extent
}

/// One overlay chunk's index data, in the narrowest format that holds it.
#[derive(Debug, Clone, Copy)]
pub enum ChunkIndices<'a> {
    U16(&'a [u16]),
    U32(&'a [u32]),
}

impl ChunkIndices<'_> {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::U16(indices) => bytemuck::cast_slice(indices),
            Self::U32(indices) => bytemuck::cast_slice(indices),
        }
    }
    pub fn len(&self) -> usize {
        match self {
            Self::U16(indices) => indices.len(),
            Self::U32(indices) => indices.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn format(&self) -> wgpu::IndexFormat {
        match self {
            Self::U16(_) => wgpu::IndexFormat::Uint16,
            Self::U32(_) => wgpu::IndexFormat::Uint32,
        }
    }
}

/// MapLibre's `SegmentVector.MAX_VERTEX_ARRAY_LENGTH` (`segment.ts:136`):
/// the most vertices a u16 index can address.
pub(crate) const MAX_SEGMENT_VERTICES: usize = u16::MAX as usize + 1;

/// One chunk of an overlay mesh: whole units, their ranges in the mesh,
/// and their bounding box in the mesh's own (overlay-local) units.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChunkPlan {
    pub vertices: std::ops::Range<usize>,
    pub indices: std::ops::Range<usize>,
    pub outline: std::ops::Range<usize>,
    /// `[min_x, min_y, max_x, max_y]`.
    pub bounds: [f32; 4],
}

impl ChunkPlan {
    /// Whether this chunk's indices fit u16.
    pub fn is_u16(&self) -> bool {
        self.vertices.len() <= MAX_SEGMENT_VERTICES
    }
}

/// Groups `extras.cuts`' units into chunks of at most `max_vertices`
/// vertices (and never more than 65,536, so u16 indices address them),
/// each of whose buffers fit `max_buffer_size`.
///
/// A unit bigger than that on its own is a chunk on its own, with u32
/// indices. `Err` when one unit alone cannot fit `max_buffer_size`: the
/// only thing chunking cannot split.
pub(crate) fn plan_overlay_chunks(
    mesh: &TileMesh,
    extras: &OverlayExtras,
    max_vertices: usize,
    max_buffer_size: u64,
) -> Result<Vec<ChunkPlan>, String> {
    let vertex_size = std::mem::size_of::<Vertex>() as u64;
    let max_vertices = max_vertices
        .clamp(1, MAX_SEGMENT_VERTICES)
        .min((max_buffer_size / vertex_size).max(1) as usize);
    // Under u16 an index is 2 bytes; a chunk's indices must fit too.
    let max_indices = (max_buffer_size / 2).max(1) as usize;
    let mut chunks: Vec<ChunkPlan> = Vec::new();
    let mut start = UnitCut { vertices: 0, indices: 0, outline: 0 };
    let mut end = start;
    let close = |chunks: &mut Vec<ChunkPlan>, start: UnitCut, end: UnitCut| {
        if end.indices > start.indices || end.outline > start.outline {
            chunks.push(ChunkPlan {
                vertices: start.vertices..end.vertices,
                indices: start.indices..end.indices,
                outline: start.outline..end.outline,
                bounds: bounds_of(&mesh.vertices[start.vertices..end.vertices]),
            });
        }
    };
    // A mesh with no recorded units (one not baked through
    // `tessellate_overlay_plan_reusing`) is one unit.
    let whole = [UnitCut { vertices: mesh.vertices.len(), indices: mesh.indices.len(), outline: extras.outline.len() }];
    let cuts: &[UnitCut] = if extras.cuts.is_empty() { &whole } else { &extras.cuts };
    for cut in cuts {
        let unit_vertices = cut.vertices - end.vertices;
        let unit_bytes = (unit_vertices as u64 * vertex_size)
            .max((cut.indices - end.indices) as u64 * 4)
            .max((cut.outline - end.outline) as u64 * 4);
        if unit_bytes > max_buffer_size {
            return Err(format!(
                "one overlay feature tessellates to {unit_vertices} vertices ({unit_bytes} B), over the \
                 device's max_buffer_size of {max_buffer_size} B"
            ));
        }
        let grown_vertices = cut.vertices - start.vertices;
        let grown_indices = (cut.indices - start.indices).max(cut.outline - start.outline);
        if end != start && (grown_vertices > max_vertices || grown_indices > max_indices) {
            close(&mut chunks, start, end);
            start = end;
        }
        end = *cut;
    }
    close(&mut chunks, start, end);
    Ok(chunks)
}

fn bounds_of(vertices: &[Vertex]) -> [f32; 4] {
    let mut b = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for v in vertices {
        b[0] = b[0].min(v.position[0]);
        b[1] = b[1].min(v.position[1]);
        b[2] = b[2].max(v.position[0]);
        b[3] = b[3].max(v.position[1]);
    }
    b
}

/// `indices` rebased to `base` in the chunk's format, written into the
/// matching scratch.
pub(crate) fn rebase_indices<'a>(
    indices: &[u32],
    base: usize,
    u16_format: bool,
    scratch16: &'a mut Vec<u16>,
    scratch32: &'a mut Vec<u32>,
) -> ChunkIndices<'a> {
    let base = base as u32;
    if u16_format {
        scratch16.clear();
        scratch16.extend(indices.iter().map(|&i| (i - base) as u16));
        ChunkIndices::U16(scratch16)
    } else {
        scratch32.clear();
        scratch32.extend(indices.iter().map(|&i| i - base));
        ChunkIndices::U32(scratch32)
    }
}

/// `tess`, `points_scratch` and `local_scratch` are caller-owned, threaded
/// through from `append_ops` the same way `append_fill`'s `tess` and
/// `ring_scratch` are: one `StrokeTessellator` and two point buffers for the
/// whole tile, reused across every `Line` op rather than rebuilt per op or
/// per run. `points_scratch` replaces `ring_points_open`'s old return-a-Vec
/// shape; `local_scratch` replaces the run loop's old per-run `local: Vec`
/// (see below).
fn append_stroke(
    buffers: &mut VertexBuffers<Vertex, u32>,
    geometry: &geo_types::Geometry<i32>,
    color_from: Rgba,
    color_to: Rgba,
    width_from: f32,
    width_to: f32,
    gap_from: f32,
    gap_to: f32,
    cap: crate::style::LineCap,
    join: crate::style::LineJoin,
    dash: DashPattern,
    metrics: bool,
    coord_scale: f32,
    extent: u32,
    subdivision: Subdivision,
    tess: &mut StrokeTessellator,
    points_scratch: &mut Vec<Point>,
    local_scratch: &mut Vec<Point>,
    segment_scratch: &mut Vec<Point>,
    subdivide_scratch: &mut SubdivideScratch,
) {
    use geo_types::Geometry::*;
    // POINT geometry on a stroke op is how a pixel-sized DOT is asked for:
    // each position becomes a zero-length subpath, and a zero-length subpath
    // is drawn by its CAP alone. A basemap `Line` layer is kept from ever
    // reaching this branch: `src/style.rs`'s `boundary_geometry_for_line`
    // maps `Point`/`MultiPoint` feature geometry to an EMPTY
    // `MultiLineString` rather than passing it through, so this branch
    // belongs to the overlay's `circle-radius`/`circle-color`, which builds
    // its `PaintOp::Line` directly rather than going through that function.
    // If either side of this changes, check the other.
    //
    // A dot has to come from the stroke path rather than from a filled ring:
    // `shader.wgsl` extrudes a stroke vertex by `normal * half_width_px *
    // world_units_per_px`, i.e. PERPENDICULAR to the path and in SCREEN
    // pixels, while a fill's (or a ring's) own extent is in world units and
    // therefore grows and shrinks with zoom. A marker's radius is a pixel
    // radius -- `hit.rs` converts `circle_radius` through
    // `world_units_per_px` for exactly that reason -- so only the cap gives
    // a dot that stays the size it was asked for at every zoom.
    let (lines, dots): (Vec<&geo_types::LineString<i32>>, Vec<&geo_types::Point<i32>>) =
        match geometry {
            LineString(l) => (vec![l], Vec::new()),
            MultiLineString(ml) => (ml.iter().collect(), Vec::new()),
            Point(p) => (Vec::new(), vec![p]),
            MultiPoint(mp) => (Vec::new(), mp.iter().collect()),
            _ => return,
        };

    // The cap comes from the style (`layout.line-cap`); the width does NOT
    // (see `STROKE_TESSELLATION_WIDTH`), and the join is still fixed here.
    //
    // A cap is only ever drawn at a subpath's two ENDS, so this is a smaller
    // change than it looks: for the closed rings a fill outline is made of,
    // and for the interior of every polyline, it emits nothing at all. What
    // it does change is every road stub, every dead end, and -- since 16 of
    // `examples/mapbox-style.json`'s layers and 15 of Liberty's ask for
    // `"round"` -- the point at which two same-coloured road segments meet
    // across a tile boundary, where a butt cap leaves a visible notch and a
    // round one closes it.
    let options = StrokeOptions::tolerance(STROKE_TOLERANCE)
        .with_line_width(STROKE_TESSELLATION_WIDTH)
        .with_line_join(lyon_join(join))
        .with_miter_limit(MITER_LIMIT)
        .with_line_cap(lyon_cap(cap));
    let color_from_arr = [color_from.r, color_from.g, color_from.b, color_from.a];
    let color_to_arr = [color_to.r, color_to.g, color_to.b, color_to.a];
    // `count == 0` is the not-dashed case; `runs` is already all zeros there,
    // but normalize explicitly so the shader's "all zeros means solid" test
    // can never be defeated by a stray run length behind a zero count.
    let mut dash_runs = dash.runs;
    if dash.count == 0 {
        dash_runs = [0.0; 4];
    }
    let ctor = StrokeCtor {
        color_from: color_from_arr,
        color_to: color_to_arr,
        half_width_from: width_from / 2.0,
        half_width_to: width_to / 2.0,
        // Halved here for the same reason the width is: the shader wants a
        // distance from the CENTRELINE, and `line-gap-width` names the width
        // of the whole hole. MapLibre does the identical `gapwidth / 2.0` at
        // the top of `line.vertex.glsl`.
        gap_half_from: gap_from / 2.0,
        gap_half_to: gap_to / 2.0,
        dash_runs,
    };

    // `Round` if the layer's join is round, `Butt` otherwise -- the cap an
    // apex (see below) gets at each of the two new ends the split makes of
    // it, in place of `line-cap`. MapLibre keeps the layer's OWN join at a
    // reversal rather than switching to its cap (`line_bucket.ts`: a round
    // join's `miterLength` stays `Infinity` there, so it stays round; a
    // miter or bevel join's join normal collapses to `-nextNormal`, a butt
    // end, well before the reversal is exact). Lyon has no interior-join
    // hook to keep once the path is cut, so this is the cap that
    // reproduces the same SHAPE at the cut as the layer's join would have
    // drawn, rather than `line-cap`'s shape, which has no relationship to
    // `line-join` at all and is wrong whenever the two differ (a `butt`
    // road with `round` joins would draw no cap at a reversal; a `round`
    // road with `miter` joins would draw one it shouldn't).
    let apex_cap =
        if matches!(join, crate::style::LineJoin::Round) { LineCap::Round } else { LineCap::Butt };

    for line in lines {
        ring_points_open(line, coord_scale, points_scratch);
        if points_scratch.len() < 2 {
            continue;
        }
        // A repeated consecutive point is a zero-length segment. lyon
        // itself already treats this safely (`points_are_too_close` in
        // `stroke.rs` merges it before any join is computed, at a ~0.54
        // lyon-unit threshold under `STROKE_TESSELLATION_WIDTH`) -- dedup
        // is not fixing a lyon defect. It exists so an A->B->B->A
        // back-track is not HIDDEN from `backtrack_apexes`, which only
        // looks at each point's immediate neighbours: without dedup, the
        // zero-length B->B segment sits between the two real ones, so
        // neither the A,B,B triple nor the B,B,A triple is a reversal by
        // itself (`dot(A->B, B->B) = dot(B->B, B->A) = 0`, neither
        // negative), and the real reversal one step further out is never
        // examined as a triple at all. `Vec::dedup` removes only ADJACENT
        // duplicates, turning that shape into a plain A->B->A this
        // function can see -- MapLibre dedups the same way for a related
        // reason (`vertices[i].equals(nextVertex)` in `line_bucket.ts`,
        // seen locally at
        // maplibre-gl-js/src/data/bucket/line_bucket.ts;
        // BSD-3).
        //
        // ONE pass decides both whether to dedup and whether to look for
        // apexes (`scan_duplicates_and_apexes`): on the common line -- no
        // duplicate, no apex -- that is the only walk this adds over the
        // points `ring_points_open` just wrote, and the apex scan below is
        // skipped outright. A line with a duplicate is deduped and
        // re-scanned by `backtrack_apexes` itself, exactly as before.
        let (has_duplicate, may_have_apex) = scan_duplicates_and_apexes(points_scratch);
        if has_duplicate {
            points_scratch.dedup();
            if points_scratch.len() < 2 {
                continue;
            }
        }
        // A back-track's incoming and outgoing directions cancel (or
        // nearly do), which is exactly where lyon's own join-normal
        // computation degenerates to `(0, 0)` -- see
        // `LYON_NORMAL_DEGENERACY_EPSILON`. MapLibre resolves the same
        // "180-degree angles, prev and next normals cancel" case inside
        // its own custom joiner by offsetting a phantom vertex before the
        // corner (`line_bucket.ts`); lyon has no such hook, so instead the
        // polyline is cut at the apex, turning the one undefined interior
        // join into two well-defined ends, each capped per `apex_cap`
        // above rather than `line-cap`.
        let mut distance_so_far = 0.0f32;
        // This LineString's vertex range, for `line_progress` below.
        let line_first_vertex = buffers.vertices.len();
        match if has_duplicate || may_have_apex { backtrack_apexes(points_scratch) } else { None } {
            None => tessellate_clean_polyline(
                points_scratch,
                subdivision,
                extent,
                coord_scale,
                subdivide_scratch,
                local_scratch,
                tess,
                &options,
                ctor,
                buffers,
                &mut distance_so_far,
            ),
            Some(apexes) => {
                // `apexes` is strictly increasing and strictly between `0`
                // and `points_scratch.len() - 1`, so bracketing it with
                // those two always yields `end > start` for every
                // resulting window -- even two back-to-back apexes (A, B,
                // A, B) bound a real, visible two-point segment (`[B,
                // A]`), not a degenerate one.
                let mut bounds = Vec::with_capacity(apexes.len() + 2);
                bounds.push(0usize);
                bounds.extend(apexes);
                bounds.push(points_scratch.len() - 1);
                let last_window = bounds.len() - 2;
                for (i, pair) in bounds.windows(2).enumerate() {
                    let (start, end) = (pair[0], pair[1]);
                    // The true ends of the line (the first and last
                    // window) keep the layer's own `line-cap`; every cut
                    // this function introduced gets `apex_cap` instead.
                    let start_cap = if i == 0 { lyon_cap(cap) } else { apex_cap };
                    let end_cap = if i == last_window { lyon_cap(cap) } else { apex_cap };
                    let segment_options = options.with_start_cap(start_cap).with_end_cap(end_cap);
                    segment_scratch.clear();
                    segment_scratch.extend_from_slice(&points_scratch[start..=end]);
                    tessellate_clean_polyline(
                        segment_scratch,
                        subdivision,
                        extent,
                        coord_scale,
                        subdivide_scratch,
                        local_scratch,
                        tess,
                        &segment_options,
                        ctor,
                        buffers,
                        &mut distance_so_far,
                    );
                }
            }
        }
        if metrics {
            write_line_progress(&mut buffers.vertices[line_first_vertex..], distance_so_far);
        }
    }

    if dots.is_empty() {
        return;
    }
    // ROUND, not the `Butt` above. `LineCap::Butt` tessellates a zero-length
    // subpath to nothing at all -- there is no length for a butt cap's quad
    // to span -- which is precisely how every overlay point feature came to
    // draw zero vertices. Round is what makes lyon emit its "empty cap": a
    // fan of vertices all at the point itself, each carrying a UNIT RADIAL
    // normal, which the vertex shader then extrudes by `half_width_px` into
    // a disc of exactly that pixel radius.
    // Its own tolerance, NOT `STROKE_TOLERANCE`. That constant is sized for
    // a join: a few degrees of arc buried inside a road, where MapLibre's own
    // 20-degrees-per-triangle is the standard to meet. A dot is the opposite
    // case -- the arc is the ENTIRE feature, a full 360 degrees of silhouette
    // at whatever pixel radius the caller asked for -- and coarsening it from
    // 64 segments to 16 would be a visible change to every overlay marker,
    // made for the sake of a vertex count that is already `DOT_VERTICES`
    // small. `dot_capacity_is_estimated_exactly` pins the 64.
    let dot_options = options.with_line_cap(LineCap::Round).with_tolerance(DOT_TOLERANCE);
    for dot in dots {
        let centre = to_lyon_point((dot.x(), dot.y()), coord_scale);
        // Rebased to the ORIGIN and translated back, exactly as a run is.
        // A dot has no extent of its own, so it needs no splitting: the
        // origin IS its frame.
        //
        // DEFENSIVE, NOT A BUG FIX -- do not delete it, but do not credit it
        // with more than it does. Measured across magnitudes from 0 to 1e12
        // lyon units, a dot's normals were ALREADY exactly unit without
        // this: lyon's round "empty cap" emits unit radial normals
        // directly rather than deriving them from the
        // `(side_point - position) / half_width` subtraction that collapses
        // for lines, so the coordinate magnitude never enters the
        // computation. What this buys is that the dot path no longer DEPENDS
        // on that lyon internal: it is magnitude-independent structurally,
        // the same way a run is, so a future lyon that builds cap normals by
        // subtraction cannot silently make every far marker invisible.
        // Positions are unchanged either way (`0 + centre` in f64 is exact).
        // `a_dot_far_from_the_origin_still_has_unit_normals` pins it.
        let first_vertex = buffers.vertices.len();
        // Two identical points, open: lyon's `move_to` + `line_to` at the
        // same position, which is the shape its empty-cap path recognises.
        let origin = lyon::math::point(0.0, 0.0);
        if let Err(err) = tess.tessellate_polygon(
            LyonPolygon { points: &[origin, origin], closed: false },
            &dot_options,
            &mut BuffersBuilder::new(buffers, ctor),
        ) {
            // As in the run loop: on failure any partial vertices lyon left
            // behind stay parked at the origin rather than at the dot.
            eprintln!("gpui-maps: dot tessellation failed for a feature, skipping: {err:?}");
            continue;
        }
        // In f64, so the addition does not discard what the origin frame
        // just bought. `line_distance` is zero for a zero-length subpath,
        // so there is no dash phase to carry here.
        for vertex in &mut buffers.vertices[first_vertex..] {
            vertex.position[0] = (vertex.position[0] as f64 + centre.x as f64) as f32;
            vertex.position[1] = (vertex.position[1] as f64 + centre.y as f64) as f32;
        }
    }
}

/// MapLibre's `lineMetrics` progress over ONE LineString's vertices, once
/// all of its runs are tessellated: `line_distance / total`, clamped to
/// `[0, 1]`, where `total` is the `distance_so_far` the run loop ended on
/// -- the LineString's whole length, across every run and every apex
/// window, since `tessellate_clean_polyline` carries it through both. `0.0`
/// everywhere for a zero-length LineString, which has no direction to trim
/// along.
///
/// Round-cap and join vertices get the progress of the point they surround:
/// `StrokeCtor` reads `advancement()`, which lyon gives every vertex of a
/// cap or join as the advancement of its centreline point.
///
/// Follows `line_bucket.ts`'s `lineSoFar / totalDistance` (MapLibre GL JS,
/// BSD-3; see `render/PROVENANCE.md`). MapLibre scales its progress into a
/// 15-bit integer to pack it; this is an `f32` attribute, so it is not.
fn write_line_progress(vertices: &mut [Vertex], total: f32) {
    for v in vertices {
        v.line_progress = if total > 0.0 { (v.line_distance / total).clamp(0.0, 1.0) } else { 0.0 };
    }
}

/// Densifies one line so that no segment spans more than `1/parts` of its
/// total length. Every input point is kept exactly; new points are placed
/// at progress `k / parts` along the line, rounded to the grid, then
/// adjacent duplicates are dropped. A line of zero length, or `parts <=
/// 1`, comes back unchanged.
///
/// For `line-gradient` (`overlay::plan::build_plan`): colour is per
/// vertex and interpolated linearly by the rasteriser, so with 64 parts
/// the ramp follows MapLibre's 256-texel gradient texture
/// (`src/webgl/draw/draw_line.ts`, MapLibre GL JS, BSD-3) to within 1/64 of
/// the line. Rounding moves a new point at most half a unit off the
/// segment -- far below a pixel at every overlay zoom -- and never makes a
/// back-track (points stay in order along the line).
pub fn densify_for_gradient(line: &[(i32, i32)], parts: u32) -> Vec<(i32, i32)> {
    densify_at_progress(line, parts, &[])
}

/// [`densify_for_gradient`] plus a point at each of `cuts` (progress
/// values in `(0, 1)`, any order): where a gradient's stops fall, so the
/// vertex colours reproduce the stop exactly (`interpolate`) or its hard
/// edge (`step`, cut at `p - eps` and `p + eps`). `parts <= 1` adds no
/// uniform points, only the cuts.
pub fn densify_at_progress(line: &[(i32, i32)], parts: u32, cuts: &[f64]) -> Vec<(i32, i32)> {
    let seg_len = |a: (i32, i32), b: (i32, i32)| {
        let (dx, dy) = (f64::from(b.0) - f64::from(a.0), f64::from(b.1) - f64::from(a.1));
        (dx * dx + dy * dy).sqrt()
    };
    let mut cumulative = Vec::with_capacity(line.len());
    let mut total = 0.0f64;
    for (i, &p) in line.iter().enumerate() {
        if i > 0 {
            total += seg_len(line[i - 1], p);
        }
        cumulative.push(total);
    }
    if total <= 0.0 || line.len() < 2 {
        return line.to_vec();
    }
    // Target distances along the line, ascending.
    let mut targets: Vec<f64> = (1..parts.max(1)).map(|k| f64::from(k) / f64::from(parts) * total).collect();
    targets.extend(cuts.iter().filter(|c| **c > 0.0 && **c < 1.0).map(|c| c * total));
    targets.sort_by(f64::total_cmp);
    let mut out: Vec<(i32, i32)> = Vec::with_capacity(line.len() + targets.len());
    let push = |p: (i32, i32), out: &mut Vec<(i32, i32)>| {
        if out.last() != Some(&p) {
            out.push(p);
        }
    };
    out.push(line[0]);
    let mut next = 0usize;
    for i in 1..line.len() {
        let (a, b) = (line[i - 1], line[i]);
        let (from, to) = (cumulative[i - 1], cumulative[i]);
        while next < targets.len() && targets[next] < to {
            let d = targets[next];
            next += 1;
            if d <= from || to <= from {
                continue;
            }
            let t = (d - from) / (to - from);
            let p = (
                (f64::from(a.0) + (f64::from(b.0) - f64::from(a.0)) * t).round() as i32,
                (f64::from(a.1) + (f64::from(b.1) - f64::from(a.1)) * t).round() as i32,
            );
            push(p, &mut out);
        }
        push(b, &mut out);
    }
    out
}

/// Colours every vertex that carries a line progress (`line_progress >=
/// 0`; fills, dots and lines without metrics carry `-1` and are left
/// alone) with `colour_at(progress)` in both `color` and `color_to`. The
/// alpha is the gradient's times the vertex's OWN alpha (its op's resolved
/// line colour, which carries the feature's opacity -- a highlight's dim
/// shows through) times `opacity`: MapLibre ignores `line-color` under a
/// gradient but multiplies `line-opacity`.
///
/// NOT idempotent: call it on vertices whose colour the op has just
/// written (a tessellation, or a paint rewrite of exactly these vertices).
/// The `line-gradient` pass (`overlay::plan::OverlayPlan::colour_mesh`).
pub fn colour_by_line_progress(vertices: &mut [Vertex], colour_at: impl Fn(f32) -> Rgba, opacity: f32) {
    for v in vertices.iter_mut().filter(|v| v.line_progress >= 0.0) {
        let c = colour_at(v.line_progress);
        let rgba = [c.r, c.g, c.b, c.a * v.color[3] * opacity];
        v.color = rgba;
        v.color_to = rgba;
    }
}

/// Same shape as `ring_points` (caller-owned scratch, cleared and refilled)
/// but for an open line: no closing-point dedup, since a line's first and
/// last points are never meant to coincide the way a ring's are.
fn ring_points_open(line_string: &geo_types::LineString<i32>, coord_scale: f32, out: &mut Vec<Point>) {
    out.clear();
    out.extend(line_string.coords().map(|c| to_lyon_point((c.x, c.y), coord_scale)));
}

/// The band this splits reversals across a 180-degree turn in, chosen to
/// be EQUIVALENT TO -- not bit-for-bit identical to -- lyon's own
/// degeneracy threshold for a join normal.
///
/// `lyon_tessellation-1.0.20`'s `math_utils::compute_normal(v1, v2)` (`v1`,
/// `v2` the unit incoming/outgoing tangents) returns `(0, 0)` exactly when
/// `(v1 + v2).square_length() < 1e-4`, and `stroke.rs`'s
/// `compute_join_side_positions_fixed_width` calls it unconditionally, for
/// every join kind -- Miter, Bevel and Round alike -- before looking at
/// `line_join` at all. Squared-length-of-a-tangent-sum is exactly `2 + 2
/// cos(theta)` for unit `v1`, `v2` (`theta` the turn angle), so lyon's own
/// `1e-4` fires within about 0.57 degrees either side of a true
/// 180-degree reversal, not only at one.
///
/// This crate's own check runs in `f64` on the RAW (not run-rebased)
/// point differences, while lyon's runs in `f32` on tangents normalized
/// from run-local, rebased points -- so matching lyon's `1e-4` exactly
/// would still leave a razor-thin band (about 1e-5 of the 0.57-degree
/// window) where `f32` rounding makes lyon call a vertex degenerate that
/// this crate's `f64` arithmetic does not, and that vertex would keep a
/// zero normal and the warning would still fire. `4e-4` -- 4x lyon's own
/// threshold -- clears that band with margin to spare, at no extra
/// runtime cost (still one comparison), AND happens to match MapLibre's
/// own "almost parallel" reversal case: `line_bucket.ts`'s join code
/// treats a turn as almost-parallel once `miterLength > 100`, i.e.
/// `cosHalfAngle < 0.01`, i.e. `|n1+n2|^2 = |t1+t2|^2 < 4e-4` -- widening
/// to it, rather than to an arbitrary safety margin, means this crate's
/// splitting band is a real reference's band, not a number tuned only to
/// this codebase's own tests. (0.57-1.15 degrees off a reversal, where
/// lyon still computes a Miter/Bevel join rather than degenerating, is
/// unaffected either way: its concave normal is clamped by
/// `MAX_NORMAL_LEN`, a pre-existing behaviour this change does not touch.)
///
/// An equality test on the raw points (`backtrack_apexes`' very first
/// version) caught only the special case where the line returns to the
/// exact same coordinate -- most reversals a real vector tile produces
/// are collinear back-tracks of a DIFFERENT length
/// (`(0,0)->(10,0)->(4,0)`), which never satisfy point equality but land
/// squarely inside this band.
const LYON_NORMAL_DEGENERACY_EPSILON: f64 = 4e-4;

/// True iff lyon's own join-normal computation would degenerate to
/// `(0, 0)` for two consecutive, not-yet-normalized segment vectors `a`
/// (incoming) and `b` (outgoing), given their dot product and their own
/// squared lengths -- see `LYON_NORMAL_DEGENERACY_EPSILON`. SQRT-FREE: a
/// stroke bake calls this once per point, on every basemap tile, so it
/// must cost close to nothing on the overwhelmingly common (non-apex)
/// point, and a `sqrt` per point was measured to be the whole of a 6%
/// regression an earlier version of this function carried.
///
/// # Derivation
///
/// With unit tangents `t1 = a / |a|`, `t2 = b / |b|`:
/// `|t1 + t2|^2 = |t1|^2 + |t2|^2 + 2 t1.t2 = 2 + 2 cos(theta)`, `theta`
/// the turn angle, and `cos(theta) = dot(a, b) / (|a| |b|)`. lyon's own
/// `|t1 + t2|^2 < epsilon` is therefore:
///
/// ```text
/// 2 + 2 dot(a,b) / (|a||b|) < epsilon
/// dot(a,b) / (|a||b|)       < epsilon/2 - 1
/// dot(a,b)                  < -(1 - epsilon/2) * |a| * |b|      -- (*)
/// ```
///
/// `1 - epsilon/2` is positive (`epsilon` is `1e-4`), so the right side of
/// `(*)` is always negative, and `(*)` can only hold when `dot(a, b)` is
/// too -- exactly the cheap, `sqrt`-free NECESSARY condition
/// (`dot(a, b) < 0`, i.e. a turn sharper than 90 degrees) this function
/// checks first, rejecting every gentler turn -- the common case -- with
/// one multiply-add and a comparison, no squared length and no `sqrt` at
/// all.
///
/// Only when that holds are both sides of `(*)` known negative, so
/// negating and squaring preserves the inequality with no sign case to
/// track:
///
/// ```text
/// -dot(a,b)   > (1 - epsilon/2) * |a| * |b|
/// dot(a,b)^2  > (1 - epsilon/2)^2 * |a|^2 * |b|^2
/// ```
///
/// which is exactly what this function evaluates once `dot < 0`, still
/// with no `sqrt`: `len_a_sq`/`len_b_sq` are `|a|^2`/`|b|^2`, computed by
/// the caller from plain sums of squares.
/// `the_sqrt_free_degeneracy_test_matches_the_normalized_form` sweeps the
/// turn angle across the threshold, at several unrelated `|a|`/`|b|`
/// pairs, and pins this bit-for-bit equivalent to the normalized form
/// lyon itself (and this function's own previous version) uses.
fn is_normal_degenerate(dot: f64, len_a_sq: f64, len_b_sq: f64) -> bool {
    if dot >= 0.0 {
        return false;
    }
    let k = 1.0 - LYON_NORMAL_DEGENERACY_EPSILON / 2.0;
    dot * dot > k * k * len_a_sq * len_b_sq
}

/// `(has an adjacent duplicate, has an apex)` for `points` as they stand,
/// in one branch-free pass -- `append_stroke`'s gate in front of both
/// `Vec::dedup` and `backtrack_apexes`, which used to run on every line
/// and now run only on a line this flags (none of the Singapore z14
/// tile's 977).
///
/// The apex half is only meaningful when the duplicate half is `false`:
/// then `points` is already exactly what `dedup` would leave, its triples
/// are exactly `backtrack_apexes`' triples, and the per-point test below
/// is `is_normal_degenerate` on the same `f64`-widened `f32` differences
/// (`&` in place of its early return, so there is no branch per point),
/// so `false` here means `backtrack_apexes` would return `None`. With a
/// duplicate, the caller dedups and runs `backtrack_apexes` regardless of
/// the second flag. `the_dedup_and_apex_gate_matches_the_passes_it_gates`
/// pins both halves.
fn scan_duplicates_and_apexes(points: &[Point]) -> (bool, bool) {
    if points.len() < 2 {
        return (false, false);
    }
    let k = 1.0 - LYON_NORMAL_DEGENERACY_EPSILON / 2.0;
    // Each segment's difference and squared length is computed ONCE and
    // carried into the next point's test as its incoming side, rather than
    // recomputed per triple -- the same values `backtrack_apexes` computes (the
    // same `f32` subtraction, then widened), so the same answer.
    //
    // A duplicate is tested as a zero squared length rather than by point
    // equality -- one comparison instead of two, on a value the apex test
    // needs anyway. The two agree exactly: an `f32` difference of two
    // finite values is zero only when they are equal (gradual underflow),
    // and the square of any non-zero `f32` is far above `f64`'s smallest
    // positive value, so a non-zero difference never squares to zero.
    let mut apex = false;
    let mut ax = f64::from(points[1].x - points[0].x);
    let mut ay = f64::from(points[1].y - points[0].y);
    let mut len_a_sq = ax * ax + ay * ay;
    let mut dup = len_a_sq == 0.0;
    for w in points[1..].windows(2) {
        let bx = f64::from(w[1].x - w[0].x);
        let by = f64::from(w[1].y - w[0].y);
        let len_b_sq = bx * bx + by * by;
        let dot = ax * bx + ay * by;
        dup |= len_b_sq == 0.0;
        apex |= (dot < 0.0) & (dot * dot > k * k * len_a_sq * len_b_sq);
        (ax, ay, len_a_sq) = (bx, by, len_b_sq);
    }
    (dup, apex)
}

/// Indices of every point in an already-deduped polyline where lyon's own
/// join-normal computation would degenerate to `(0, 0)` -- see
/// `is_normal_degenerate` -- or `None` if there isn't one, the common case
/// and the only one `append_stroke`'s fast path needs to distinguish.
///
/// Evaluated on the pre-subdivision points rather than lyon's own final
/// input: `subdivide_line` only inserts points that are exactly COLLINEAR
/// with the segment they split (see its doc comment), so it never changes
/// the tangent direction either side of an original vertex, and evaluating
/// here is what lets `append_stroke` cut the polyline before subdivision
/// or run-splitting ever sees it.
///
/// In `f64`, widened from the `f32` lyon points: the widening itself is
/// exact (every `f32` is exactly representable in `f64`), and doing the
/// dot/length-squared arithmetic at the wider precision is what keeps a
/// near-boundary case (a real turn a few hundredths of a degree off the
/// threshold, at a far tile's large coordinate magnitude) from being
/// decided by `f32` rounding in the multiply-heavy squared form instead of
/// by the turn angle itself.
fn backtrack_apexes(points: &[Point]) -> Option<Vec<usize>> {
    if points.len() < 3 {
        return None;
    }
    let mut apexes = Vec::new();
    for i in 1..points.len() - 1 {
        let ax = f64::from(points[i].x - points[i - 1].x);
        let ay = f64::from(points[i].y - points[i - 1].y);
        let bx = f64::from(points[i + 1].x - points[i].x);
        let by = f64::from(points[i + 1].y - points[i].y);
        let dot = ax * bx + ay * by;
        // The cheap rejection: see `is_normal_degenerate`'s doc comment.
        // Skipped inline (rather than always calling `is_normal_
        // degenerate` and letting it early-return) so `len_a_sq`/
        // `len_b_sq` -- two more multiplies and an add each -- are never
        // computed for the common, non-apex point either.
        if dot >= 0.0 {
            continue;
        }
        let len_a_sq = ax * ax + ay * ay;
        let len_b_sq = bx * bx + by * by;
        if is_normal_degenerate(dot, len_a_sq, len_b_sq) {
            apexes.push(i);
        }
    }
    if apexes.is_empty() {
        None
    } else {
        Some(apexes)
    }
}

/// The subdivide -> run-split -> lyon-tessellate pipeline for one already-
/// clean (deduped, back-track-free) polyline, factored out of
/// `append_stroke`'s line loop so a line WITH a back-track can run it once
/// per split segment while a line without one -- the overwhelmingly common
/// case -- still runs it exactly once, in place, with no extra allocation.
#[allow(clippy::too_many_arguments)]
fn tessellate_clean_polyline(
    points: &mut Vec<Point>,
    subdivision: Subdivision,
    extent: u32,
    coord_scale: f32,
    subdivide_scratch: &mut SubdivideScratch,
    local_scratch: &mut Vec<Point>,
    tess: &mut StrokeTessellator,
    options: &StrokeOptions,
    ctor: StrokeCtor,
    buffers: &mut VertexBuffers<Vertex, u32>,
    distance_so_far: &mut f32,
) {
    if points.len() < 2 {
        return;
    }
    // BEFORE `split_into_runs` and before lyon, which is the only place it
    // can go: a stroke's extrusion direction lives in `Vertex::normal` and
    // is applied in the vertex shader, so cutting an already-extruded
    // ribbon on the grid would have to invent normals for the interior
    // points. The reference subdivides the line layer's input geometry for
    // the same reason (`subdivideVertexLine`, `subdivision.ts:757`).
    subdivide::subdivide_line(points, subdivision, extent, coord_scale, subdivide_scratch);
    for run in run_split::split_into_runs(points, run_split::MAX_RUN_EXTENT) {
        // Tessellate relative to the run's OWN first vertex, where the
        // float spacing is at its finest. This is the whole fix: lyon
        // derives a stroke normal from the difference of two points
        // `STROKE_TESSELLATION_WIDTH / 2` apart, in f32, and needs the
        // local spacing to stay far below that half-width. Rebasing makes
        // the spacing a function of the run's own EXTENT (bounded by
        // `MAX_RUN_EXTENT`) rather than of its distance from the
        // coordinate origin, which is unbounded.
        let offset = points[run.start];
        // `lyon::math::Point` spelled out: `use geo_types::Geometry::*` in
        // `append_stroke` shadows the bare `Point` with geo's enum
        // variant. `local_scratch` is cleared and refilled per run rather
        // than allocated fresh: `add_polygon`/`tessellate_polygon` consume
        // a run's points synchronously, so nothing outlives this call.
        local_scratch.clear();
        local_scratch.extend(
            points[run.clone()]
                .iter()
                .map(|p| lyon::math::point(p.x - offset.x, p.y - offset.y)),
        );

        let first_vertex = buffers.vertices.len();
        // A BuffersBuilder per run rather than one for the whole stroke: it
        // borrows `buffers` mutably for its whole lifetime, and the
        // translate-back pass below needs that borrow released.
        // Constructing one is just wrapping a `&mut` around a `Copy`
        // constructor, so the per-run cost is nil.
        if let Err(err) = tess.tessellate_polygon(
            LyonPolygon { points: local_scratch.as_slice(), closed: false },
            options,
            &mut BuffersBuilder::new(buffers, ctor),
        ) {
            // Any vertices lyon pushed before failing stay parked near the
            // ORIGIN, untranslated, rather than near the geometry -- and
            // this run contributes nothing to `distance_so_far`, so later
            // runs' dash phase shifts. Both are accepted:
            // `tessellate_polygon` on a plain open polyline has no known
            // failure mode, the shape predates this change, and there is
            // nothing sensible to translate a half-built run by.
            eprintln!("gpui-maps: stroke tessellation failed for a run, skipping: {err:?}");
            continue;
        }

        // Translate back into the shared frame, in f64 so the addition
        // itself does not throw away the precision the small frame just
        // bought, and carry the dash phase across the boundary.
        //
        // `run_length` is read from the PRE-accumulation `line_distance`
        // values, so it is this run's own length and not a running total;
        // adding `distance_so_far` afterwards is what makes the phase
        // continuous.
        let mut run_length = 0.0f32;
        for vertex in &mut buffers.vertices[first_vertex..] {
            vertex.position[0] = (vertex.position[0] as f64 + offset.x as f64) as f32;
            vertex.position[1] = (vertex.position[1] as f64 + offset.y as f64) as f32;
            run_length = run_length.max(vertex.line_distance);
            vertex.line_distance += *distance_so_far;
        }
        *distance_so_far += run_length;
    }
}

/// Tessellates an overlay source's (or the route line's) plan: its origin's
/// local grid at [`OVERLAY_COORD_SCALE`], cut on the overlay's own
/// subdivision grid. The one definition every overlay mesh is baked with --
/// `MapView`'s sources, `render::nav_layer`'s route line and `capture` --
/// so none of them can drift from the others.
///
/// `coord_scale` is the plan's origin's
/// (`overlay::geom::OverlayOrigin::coord_scale`): [`OVERLAY_COORD_SCALE`]
/// for every source that fits the full-resolution grid, a power of two above
/// it for one wider than ~151 deg from its centre.
#[must_use]
pub fn tessellate_overlay_plan(plan: &PaintPlan, coord_scale: f32) -> TileMesh {
    tessellate_plan(plan, subdivide::OVERLAY_SUBDIVISION_EXTENT, coord_scale, Subdivision::OVERLAY)
}

/// [`tessellate_overlay_plan`] with each op's vertex and index ranges: what
/// an in-place paint rewrite of one op of an overlay mesh
/// ([`rewrite_paint_attributes_for_ops`]) needs, as the capture's story
/// overlay cache keeps it.
pub fn tessellate_overlay_plan_with_ranges(plan: &PaintPlan, coord_scale: f32) -> (TileMesh, Vec<OpSpan>) {
    tessellate_plan_with_ranges(plan, subdivide::OVERLAY_SUBDIVISION_EXTENT, coord_scale, Subdivision::OVERLAY)
}

pub fn tessellate_plan(
    plan: &PaintPlan,
    extent: u32,
    coord_scale: f32,
    subdivision: Subdivision,
) -> TileMesh {
    tessellate_plan_with_ranges(plan, extent, coord_scale, subdivision).0
}

/// What fraction of `vertices`' STROKE normals have collapsed toward zero.
///
/// A stroke normal is a direction: its length is 1 for a straight run, and
/// *greater* than 1 at a miter join, where the extrusion has to reach past
/// the outside corner to keep the stroke's width constant along the turn
/// (bounded by `MAX_NORMAL_LEN`, which clamps the spikes sharp turns would
/// otherwise produce). So elongation is normal and must not be flagged --
/// only a length near zero is a defect: it means lyon differenced two
/// points that rounded to the same f32, and the vertex shader will extrude
/// it by nothing, drawing a line that is present in the buffer and
/// invisible on screen. Fill vertices carry a zero normal legitimately and
/// are not counted.
fn collapsed_normal_share(vertices: &[Vertex]) -> f32 {
    let strokes = vertices.iter().filter(|v| v.is_stroke > 0.5);
    let (mut total, mut bad) = (0usize, 0usize);
    for vertex in strokes {
        total += 1;
        let length =
            (vertex.normal[0] * vertex.normal[0] + vertex.normal[1] * vertex.normal[1]).sqrt();
        if length < 0.9 {
            bad += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    bad as f32 / total as f32
}

/// How many vertices lyon's STROKE tessellator emits per input point under
/// a BEVEL or MITER join (`LineCap` does not move this -- a cap is drawn
/// once per subpath end, not once per point).
///
/// Measured, not guessed: an open polyline of `n` points tessellates to
/// exactly `3n - 1` or `3n - 2` vertices across every shape tried (2, 3,
/// 10, 100 and 10,000 points; straight, zig-zag, and the 89 real routes in
/// `geojsons/simulation.json`) -- two side vertices per point plus one
/// bevel-join vertex, minus the join the two end caps do not have.
///
/// This is a true CEILING for both joins it covers, which is what lets
/// `estimate_buffer_capacity` be an upper bound for them: a miter emits one
/// vertex where a bevel emits two, and a miter past `MITER_LIMIT` degrades
/// to exactly the bevel this number was measured on. `LineJoin::Round` is
/// the one that does not fit under it -- see the constant below.
const STROKE_VERTICES_PER_POINT: usize = 3;
/// Indices per input point on the same measurement: `9n - 15`, i.e. three
/// triangles per point.
const STROKE_INDICES_PER_POINT: usize = 9;

/// The same count for a `LineJoin::Round` op, where a join is a FAN rather
/// than a single vertex.
///
/// Not a ceiling, and deliberately so. Measured over 1000-point zig-zags at
/// `STROKE_TOLERANCE`, the count runs 3 per point for turns under about 10
/// degrees, 6 at the moderate turns real road geometry is mostly made of,
/// and 10 at near-hairpins; indices are exactly three times vertices at
/// every one of those. `reserve_for_ops`'s doc comment is explicit that
/// this estimate "must not be padded to be safe" -- one or two reallocs are
/// harmless, while every byte over-reserved stays resident for the life of
/// the mesh -- so this is the typical, not the worst case. A hairpin-heavy
/// op reallocs once; a road layer does not carry 10/3 of the buffer it
/// needs forever.
const STROKE_VERTICES_PER_POINT_ROUND_JOIN: usize = 6;
/// Indices per input point for a round join. Exactly `3x` the vertices, at
/// every turn angle measured.
const STROKE_INDICES_PER_POINT_ROUND_JOIN: usize = 18;

/// The per-point vertex/index estimate for one stroke op, by join.
fn stroke_per_point(join: crate::style::LineJoin) -> (usize, usize) {
    match join {
        crate::style::LineJoin::Round => {
            (STROKE_VERTICES_PER_POINT_ROUND_JOIN, STROKE_INDICES_PER_POINT_ROUND_JOIN)
        }
        crate::style::LineJoin::Bevel | crate::style::LineJoin::Miter => {
            (STROKE_VERTICES_PER_POINT, STROKE_INDICES_PER_POINT)
        }
    }
}

/// How many vertices lyon's FILL tessellator emits per input ring point.
///
/// Measured at `1.00 * n` (`0.990`-`1.000` over convex rings of 100 and
/// 2000 points, a 200-point star, a ring with two holes, and a 20-polygon
/// multipolygon): a triangulation of a simple ring re-uses its input
/// points and adds none. A SELF-INTERSECTING ring does add Steiner points
/// at each crossing, so this is an underestimate there -- deliberately, see
/// `reserve_for_ops`: an underestimate costs one realloc, an overestimate
/// costs resident bytes on every mesh forever.
const FILL_VERTICES_PER_POINT: usize = 1;
/// Indices per input ring point on the same measurement: `3n - 6`, i.e. the
/// `n - 2` triangles a simple polygon triangulates to.
const FILL_INDICES_PER_POINT: usize = 3;

/// Vertices in one overlay DOT -- a round "empty cap" over a zero-length
/// subpath (see `append_stroke`).
///
/// A CONSTANT rather than a function of the dot's radius, because lyon
/// flattens that cap at a FIXED half-width: `append_stroke` always builds
/// its `StrokeOptions` with `STROKE_TESSELLATION_WIDTH` and
/// `tolerance(0.1)`, and the style's real `circle-radius` reaches the GPU
/// as the per-vertex `half_width_px` instead. That is the same property
/// `STROKE_TESSELLATION_WIDTH`'s doc comment calls the cache-determinism
/// invariant -- topology independent of the style's actual width -- so if
/// this number ever moves, that invariant has been broken and much more
/// than an estimate is wrong. Measured at exactly 64 vertices / 186 indices
/// per dot; `dot_capacity_is_estimated_exactly` pins it.
const DOT_VERTICES: usize = 64;
/// Flattening tolerance for a marker dot's round cap; see `dot_options` in
/// `append_stroke` for why it is not `STROKE_TOLERANCE`. This is the value
/// every stroke used before `layout.line-join` was honoured, so a dot's
/// geometry is byte-for-byte what it has always been.
const DOT_TOLERANCE: f32 = 0.1;
/// Indices in one overlay dot; see `DOT_VERTICES`.
const DOT_INDICES: usize = 186;

/// Extra capacity added to the estimate, as a fraction (1/64 = 1.6%).
///
/// Covers the two places the per-point counts above run slightly light:
/// `run_split::split_into_runs` repeats the boundary point between
/// neighbouring runs (so a split line is tessellated as slightly more
/// points than it has), and lyon's own count wobbles by a vertex or two per
/// subpath. Without it, `geojsons/simulation.json` lands 380 vertices
/// (0.015%) UNDER its real 2,486,864 -- and being one vertex short is
/// expensive in a way that being 1.6% long is not: the `Vec` then doubles
/// to 4.97M vertices, 200 MB of which is waste that stays resident for the
/// life of the mesh. 1.6% of a 199 MB buffer is 3 MB.
const ESTIMATE_HEADROOM_DIVISOR: usize = 64;

/// Input points in a polygon's rings. `O(rings)`, not `O(points)`:
/// `LineString`'s coordinates are a `Vec`, so its length is already known.
fn polygon_point_count(polygon: &geo_types::Polygon<i32>) -> usize {
    polygon.exterior().0.len()
        + polygon.interiors().iter().map(|ring| ring.0.len()).sum::<usize>()
}

/// Grid lines a polygon's rings cross -- the fill counterpart of
/// `subdivide::line_crossings`, and the input to the fill half of the
/// subdivision estimate.
fn polygon_crossings(
    polygon: &geo_types::Polygon<i32>,
    subdivision: Subdivision,
    extent: u32,
    coord_scale: f32,
) -> usize {
    if subdivision.is_none() {
        return 0;
    }
    std::iter::once(polygon.exterior())
        .chain(polygon.interiors())
        .map(|ring| subdivide::line_crossings(ring, subdivision, extent, coord_scale))
        .sum()
}

/// Vertices `subdivide_fill` adds per ring crossing of a grid line.
///
/// A fill's growth is not a function of its crossings alone -- lyon's long
/// thin triangles cross lines the ring never does, and a polygon spanning
/// many cells gains interior grid points -- so this is a measured ceiling,
/// not a derivation. On TIGER block groups the real growth ran 35 (CA) to
/// 108 (MT) vertices and up to 650 indices per ring crossing; the constants
/// sit above both. Over-reserving is cheap where under-reserving is not: an
/// untouched tail is virtual until written and `shrink_if_overgrown` hands
/// it back, while one vertex short doubles the `Vec`. Before fills were
/// counted, a national overlay (+9.6 % vertices, +18.8 % indices from
/// subdivision) doubled both `Vec`s on every state: 7.2 GB of heap for
/// 0.75 GB of real growth. Measured on California: peak heap 1,326 -> 919
/// MB and `MALLOC_LARGE (empty)` 666 -> 149 MB resident.
const FILL_VERTICES_PER_CROSSING: usize = 128;
/// Indices per crossing; see `FILL_VERTICES_PER_CROSSING`.
const FILL_INDICES_PER_CROSSING: usize = 704;

/// Buffers below this many bytes are never shrunk for an estimate's
/// overshoot alone: they do not leak (`reserve_for_ops`), and a tile bake
/// should not pay a `realloc` to give back a few kilobytes.
const SHRINK_MIN_BYTES: usize = 8 << 20;

/// Gives back a buffer's tail when it carries more than 10 % slack that
/// THIS bake created: a doubling past the reservation (`outgrew`), or a
/// large reservation (`reserved_here`, judged on the whole mesh by the
/// caller against `SHRINK_MIN_BYTES`) the estimate overshot. Never a
/// reuse-arena donor's own capacity, which is kept on purpose (see
/// `tessellate_plan_with_ranges_reusing`).
///
/// `shrink_to_fit` is a `realloc` down; for a large block libmalloc does
/// that in place, returning the tail pages rather than copying the mesh.
fn shrink_if_overgrown<T>(buffer: &mut Vec<T>, reserved_here: bool, outgrew: bool) {
    let len = buffer.len();
    if buffer.capacity() <= len + len / 10 {
        return;
    }
    if outgrew || reserved_here {
        buffer.shrink_to_fit();
    }
}

/// A cheap upper-ish bound on the `(vertices, indices)` `append_ops` will
/// push for `ops`, from the input point counts alone.
///
/// COSTS NOTHING TO COMPUTE. Every geometry variant's point count is a
/// `Vec::len` on a ring, so this walks ops and rings, never coordinates --
/// against `geojsons/simulation.json` that is 2,264 ops and 2,264 rings
/// versus the 828,828 coordinates `append_ops` is about to visit. It only
/// counts the geometry variants `append_fill`/`append_stroke` actually
/// tessellate, so a `Fill` handed line geometry (or a `Line` handed a
/// polygon) contributes nothing here exactly as it contributes nothing
/// there.
fn estimate_buffer_capacity(
    ops: &[PaintOp],
    coord_scale: f32,
    extent: u32,
    subdivision: Subdivision,
) -> (usize, usize) {
    use geo_types::Geometry::*;
    let (mut vertices, mut indices) = (0usize, 0usize);
    let mut add = |points: usize, per_vertex: usize, per_index: usize| {
        vertices += points * per_vertex;
        indices += points * per_index;
    };
    for op in ops {
        match op {
            // An extrusion is never in the 88-byte mesh: `render::extrusion`
            // tessellates it into its own buffers.
            PaintOp::Background { .. } | PaintOp::Extrusion { .. } => {}
            PaintOp::Fill { geometry, .. } => {
                let points = match &**geometry {
                    Polygon(p) => polygon_point_count(p),
                    MultiPolygon(mp) => mp.0.iter().map(polygon_point_count).sum(),
                    _ => 0,
                };
                add(points, FILL_VERTICES_PER_POINT, FILL_INDICES_PER_POINT);
                // What `subdivide_fill` adds on top, bounded by the ring's
                // own grid crossings. A no-op without subdivision, and for
                // the common overlay feature that sits inside one cell.
                let crossings = match &**geometry {
                    Polygon(p) => polygon_crossings(p, subdivision, extent, coord_scale),
                    MultiPolygon(mp) => mp
                        .0
                        .iter()
                        .map(|p| polygon_crossings(p, subdivision, extent, coord_scale))
                        .sum(),
                    _ => 0,
                };
                add(crossings, FILL_VERTICES_PER_CROSSING, FILL_INDICES_PER_CROSSING);
            }
            PaintOp::Line { geometry, join, .. } => match &**geometry {
                LineString(l) => {
                    let (v, i) = stroke_per_point(*join);
                    add(l.0.len() + subdivide::line_crossings(l, subdivision, extent, coord_scale), v, i);
                }
                MultiLineString(ml) => {
                    let (v, i) = stroke_per_point(*join);
                    add(
                        ml.0.iter()
                            .map(|line| {
                                line.0.len()
                                    + subdivide::line_crossings(line, subdivision, extent, coord_scale)
                            })
                            .sum(),
                        v,
                        i,
                    );
                }
                Point(_) => add(1, DOT_VERTICES, DOT_INDICES),
                MultiPoint(mp) => add(mp.0.len(), DOT_VERTICES, DOT_INDICES),
                _ => {}
            },
        }
    }
    (
        vertices + vertices / ESTIMATE_HEADROOM_DIVISOR,
        indices + indices / ESTIMATE_HEADROOM_DIVISOR,
    )
}

/// Sizes `buffers` for what `ops` is about to put in it.
///
/// THIS IS NOT A MICRO-OPTIMISATION. Deleting it reintroduces a
/// multi-gigabyte leak.
///
/// macOS's libmalloc does not reuse -- or return to the kernel -- the pages
/// behind a large `Vec` that was grown from empty through a long chain of
/// reallocs. `VertexBuffers::new()` is two `Vec::new()`, and an overlay
/// bake pushes them from zero to ~199 MB (2.49M vertices x 80 B) and
/// ~30 MB: about 22 doublings. Every one of those intermediate blocks stays
/// resident after it is freed -- `vmmap` shows them as `MALLOC_REALLOC
/// (empty)` / `MALLOC_LARGE (empty)` regions, ~1.4 GB apiece, with no live
/// allocations in them. Since `MapView::set_features` re-bakes the whole
/// overlay on every push, RSS then climbs by roughly one whole mesh per
/// push and never comes back down: a consumer's app reached 8.8 GB in
/// fifteen minutes, and pushing `geojsons/simulation.json` 45 times here
/// reached 8.69 GB.
///
/// Reproduced in isolation, single-threaded, with no gpui/tokio/wgpu in the
/// process: ONE large allocation recycles fine, `Vec::with_capacity(n)`
/// then fill recycles fine, and `Vec::new()` then push `n` times leaks
/// ~191 MB per iteration forever. It is the doubling CHAIN that libmalloc
/// will not recycle, not the size.
///
/// Which is why the estimate does not have to be exact, and why it must not
/// be padded "to be safe": one or two large reallocs are harmless (a
/// deliberately crude 2,000,000-vertex reservation against the same
/// 2,486,864-vertex mesh was already flat), while every byte over-reserved
/// stays resident for the life of the mesh. Buffers below roughly 10 MB do
/// not leak at all, so tile meshes are unaffected either way and this runs
/// for them only because it is the same code path.
///
/// Measured on `examples/overlay_rss_loop` over 24 pushes of
/// `geojsons/simulation.json`: WITHOUT this, RSS went 646 MB -> 3,497 MB by
/// push 12 (+238 MB/push) before the OS started reclaiming under pressure.
/// WITH it, RSS settles at ~1,020 MB and is flat from the second push.
fn reserve_for_ops(
    buffers: &mut VertexBuffers<Vertex, u32>,
    ops: &[PaintOp],
    coord_scale: f32,
    extent: u32,
    subdivision: Subdivision,
) {
    let (vertices, indices) = estimate_buffer_capacity(ops, coord_scale, extent, subdivision);
    // `reserve_exact`, not `reserve`: `reserve`'s amortised-growth policy is
    // free to round up to the next power of two, which on a 199 MB buffer is
    // 57 MB of waste to no purpose -- the estimate is already the answer.
    buffers.vertices.reserve_exact(vertices);
    buffers.indices.reserve_exact(indices);
}

/// Drops a reused buffer's allocation when it has grown far larger than the
/// work in front of it, so the arena's high-water mark is bounded rather
/// than permanent.
///
/// # Why a reuse arena needs this at all
///
/// Reuse without it is a RATCHET: capacity climbs to the largest collection
/// ever pushed and never comes back, so one momentary 109 MB fleet leaves
/// every later bake sitting on 109 MB of buffer regardless of what it is
/// actually drawing. That is a bounded leak rather than an unbounded one --
/// it cannot exceed the largest push -- but "bounded by the worst thing
/// that ever happened" is not a bound worth having on a view that lives for
/// the life of the process.
///
/// # Why the threshold is 2x, and why a smaller one would be worse
///
/// The buffer is released only when it is more than TWICE what this bake
/// needs. The gap between "keep" and "release" is what stops the arena
/// thrashing, and it has to be a real gap, because releasing re-arms the
/// decision at the new size:
///
/// - **A workload that oscillates never shrinks.** Alternating between S
///   and 2S leaves capacity at 2S and need at S; `2S > 2 * S` is false, so
///   the big buffer is kept and reused for both. A tighter threshold --
///   release when merely larger than needed, say -- would free and
///   reallocate on every single tick of exactly that pattern, which is the
///   commonest shape a fleet has, and would be strictly worse than not
///   pooling at all.
/// - **A workload that genuinely shrinks converges fast.** Each release
///   leaves capacity equal to the new need, so triggering another one takes
///   a FURTHER halving. A collection falling from 109 MB to nothing sheds
///   it in a handful of bakes, and a collection that has found its size
///   stops shedding immediately.
///
/// So the waste is capped at one bake's worth of slack -- under 2x what is
/// currently drawn -- instead of at the historical maximum, and the steady
/// state, where `want` and `capacity` are equal, never takes this path at
/// all.
///
/// `saturating_mul` because `want` is derived from geometry point counts
/// and a pathological plan should degrade to "keep the buffer", never to a
/// wrapped comparison that throws a good allocation away.
fn release_oversized<T>(buffer: &mut Vec<T>, want: usize) {
    debug_assert!(buffer.is_empty(), "hysteresis must run on a cleared buffer, or it drops data");
    if buffer.capacity() > want.saturating_mul(2) {
        // `Vec::new` does not allocate; the old allocation is freed by this
        // assignment, on this thread, which is a blocking worker.
        *buffer = Vec::new();
    }
}

/// Same tessellation as `tessellate_plan`, but also returns, for each op in
/// `plan.ops` (same order, same length), the range of `mesh.vertices` that
/// op produced. `PaintOp::Background` emits no vertices, so its range is
/// empty (`start == end`) rather than being skipped -- every op gets exactly
/// one entry, so `ranges[i]` always corresponds to `plan.ops[i]` by index,
/// with no shifting if an earlier op happened to emit nothing.
///
/// This is what makes `rewrite_paint_attributes` possible: the triangulation
/// (vertex positions/normals/is_stroke, and the index buffer) depends only
/// on an op's geometry, not its paint values, so caching these ranges
/// alongside the mesh lets a later zoom crossing -- which changes paint
/// values but not geometry -- overwrite just the paint fields in place
/// instead of re-tessellating from scratch.
pub fn tessellate_plan_with_ranges(
    plan: &PaintPlan,
    extent: u32,
    coord_scale: f32,
    subdivision: Subdivision,
) -> (TileMesh, Vec<OpSpan>) {
    tessellate_plan_with_ranges_cancellable(plan, extent, coord_scale, subdivision, &|| false)
        .expect("a tessellation that can never be cancelled always produces a mesh")
}

/// [`tessellate_plan_with_ranges`], but abandons the mesh and returns
/// `None` the moment `cancel` says the result is no longer wanted.
///
/// `cancel` is polled BEFORE the reservation and then between every pair of
/// ops -- never inside one, because lyon's tessellators own the loop over a
/// single op's geometry and there is no callback into it. Op granularity is
/// enough for the case this exists for: an overlay bake is thousands of ops
/// (one per feature), so an abort lands within one feature's triangulation
/// rather than after the whole collection's.
///
/// WHY ABORTING MID-WORK AND NOT AT THE DOOR. A superseded bake was already
/// being thrown away -- `install_overlay_mesh` checks the generation and
/// drops the mesh. But by then the mesh EXISTS: its full vertex and index
/// capacity has been reserved (`reserve_for_ops` takes the whole estimate
/// up front, which for a large fleet collection is over a hundred
/// megabytes), every triangle has been computed, and the discarded buffers
/// are freed on whichever thread holds them. Checking early converts all of
/// that into an early return.
///
/// This is Organic Maps' discipline rather than MapLibre's. MapLibre
/// coalesces a pending render into a single flag (`map.ts:4500-4502`),
/// which is all a `requestAnimationFrame` idiom can do: its "cancel" means
/// "was never scheduled", because JS cannot preempt work already running.
/// OMIM passes its cancellation predicate DOWN into the per-feature walk
/// (`ReadMWMTask`'s expired `weak_ptr`, checked at `tile_info.cpp:51` and
/// again inside the feature loop at `:59`) and aborts mid-read. That is the
/// shape reproduced here, because the work being abandoned is the expensive
/// part.
///
/// A `&dyn Fn` rather than a generic parameter: this is called once per
/// bake and polled once per op against a relaxed atomic load, so the
/// indirect call is unmeasurable beside a triangulation, and monomorphising
/// it would duplicate the whole tessellator for each caller's closure type.
pub fn tessellate_plan_with_ranges_cancellable(
    plan: &PaintPlan,
    extent: u32,
    coord_scale: f32,
    subdivision: Subdivision,
    cancel: &dyn Fn() -> bool,
) -> Option<(TileMesh, Vec<OpSpan>)> {
    tessellate_plan_with_ranges_reusing(
        plan,
        extent,
        coord_scale,
        subdivision,
        cancel,
        TileMesh::default(),
    )
    .ok()
}

/// [`tessellate_plan_with_ranges_cancellable`], but tessellating INTO the
/// buffers of a mesh that is no longer wanted, so a steady state that keeps
/// rebuilding a collection of roughly the same size stops going back to the
/// allocator for the biggest allocation in this crate.
///
/// # What `scratch` is
///
/// Capacity, not data. Its contents are cleared before anything is written,
/// so what is passed in has no effect whatsoever on what comes out -- only
/// on how many times the allocator is called on the way. Passing
/// `TileMesh::default()` is exactly the un-pooled behaviour, which is what
/// `tessellate_plan_with_ranges_cancellable` does.
///
/// # Why this is worth a second entry point
///
/// `reserve_for_ops` `reserve_exact`s the WHOLE estimate up front -- for a
/// large fleet collection that is a single allocation of over a hundred
/// megabytes, made before one triangle is written, and freed a bake or two
/// later when the mesh it produced is displaced. A `Vec` whose length is
/// cleared keeps its capacity, so handing the displaced mesh back here
/// makes `reserve_exact` a no-op whenever the new collection fits inside
/// the old one's high-water mark, which in a simulation stepping the same
/// fleet is every single tick.
///
/// This is the discipline `MapView::install_mesh` already applies to GPU
/// buffers -- keep the allocation, reuse it while it is big enough --
/// applied to the CPU side, and it is Organic Maps' `ChangeBuffer`/pooled
/// staging buffers (`read_manager.hpp:99`) in Rust terms.
///
/// # The trade, stated plainly
///
/// Retaining one mesh's capacity to hand back is retention: the arena holds
/// a full mesh that would otherwise have been freed. It does NOT raise the
/// peak -- the displaced mesh and the mesh replacing it are both live at
/// the moment of the swap either way, which is where the peak already was
/// -- but it does raise the resting figure by one mesh. Churn is traded for
/// steady-state bytes, deliberately, and the caller that holds the arena
/// is the right place to decide that.
///
/// # Errors
///
/// `Err` is a cancellation, not a failure, and it carries the buffers back
/// out rather than dropping them -- an abandoned bake's capacity is the
/// most valuable thing it has, and losing it would mean the next bake after
/// every supersession pays the full reservation again.
pub fn tessellate_plan_with_ranges_reusing(
    plan: &PaintPlan,
    extent: u32,
    coord_scale: f32,
    subdivision: Subdivision,
    cancel: &dyn Fn() -> bool,
    scratch: TileMesh,
) -> Result<(TileMesh, Vec<OpSpan>), TileMesh> {
    // `extent` was dead for two years (`let _ = extent`, "kept for API
    // clarity") because the scale it described is carried by the model
    // matrix. Subdivision makes it live again, and only it: a cell is
    // `EXTENT / granularity` (`subdivision.ts:46`), and the tile's extent is
    // not recoverable from a vertex position. See `Subdivision::cell_size`.
    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    buffers.vertices = scratch.vertices;
    buffers.indices = scratch.indices;
    // THE load-bearing line of this function. `Vec::clear` drops the
    // elements and sets the length to zero while KEEPING the capacity,
    // which is the entire trick: the buffers below are empty by every
    // measure `append_ops` uses, and still hold the previous mesh's
    // allocation. Skipping it would append this plan's triangles after the
    // last collection's -- a silently wrong mesh, not a crash -- which is
    // what `a_reused_arena_produces_a_byte_identical_mesh` pins.
    buffers.vertices.clear();
    buffers.indices.clear();

    // Before the reservation, deliberately -- and before the estimate that
    // sizes it, so a bake that is already superseded when it reaches the
    // tessellator pays for neither. The reservation is the single largest
    // allocation this function makes and it happens before one triangle is
    // written. Handing the donor straight back with its capacity untouched
    // is also what lets a cancelled bake return a buffer the next one can
    // still use.
    if cancel() {
        return Err(TileMesh { vertices: buffers.vertices, indices: buffers.indices });
    }

    // Computed once here rather than through `reserve_for_ops`, because the
    // hysteresis below needs the same two numbers and walking every op
    // twice to get them would be the one place this optimisation paid for
    // itself in CPU.
    let (want_vertices, want_indices) =
        estimate_buffer_capacity(&plan.ops, coord_scale, extent, subdivision);
    release_oversized(&mut buffers.vertices, want_vertices);
    release_oversized(&mut buffers.indices, want_indices);

    // `reserve_exact`, not `reserve`, and load-bearing against a
    // multi-gigabyte RSS leak rather than a speed tweak -- see
    // `reserve_for_ops`, whose body this is. A no-op when the donor was
    // already big enough, which is the point of the donor.
    let donor_vertices = buffers.vertices.capacity();
    let donor_indices = buffers.indices.capacity();
    buffers.vertices.reserve_exact(want_vertices);
    buffers.indices.reserve_exact(want_indices);
    let reserved_vertices = buffers.vertices.capacity();
    let reserved_indices = buffers.indices.capacity();
    let mut spans = Vec::with_capacity(plan.ops.len());
    if !append_ops(&mut buffers, &plan.ops, coord_scale, extent, subdivision, &mut spans, cancel) {
        return Err(TileMesh { vertices: buffers.vertices, indices: buffers.indices });
    }
    // Slack THIS bake created -- a doubling past the reservation, or a
    // large reservation the estimate overshot -- is waste the mesh would
    // otherwise carry for its life. A donor's own capacity is left alone.
    #[cfg(feature = "probe")]
    if std::env::var_os("GPUI_MAP_PROBE_ESTIMATE").is_some() {
        let (bv, bi) = estimate_buffer_capacity(&plan.ops, coord_scale, extent, Subdivision::NONE);
        eprintln!(
            "probe estimate: want {want_vertices}/{want_indices} (no-subdiv {bv}/{bi}) real {}/{}",
            buffers.vertices.len(),
            buffers.indices.len()
        );
    }
    let outgrew_vertices = buffers.vertices.capacity() > reserved_vertices;
    let outgrew_indices = buffers.indices.capacity() > reserved_indices;
    // Large is judged on the whole mesh, so its small index buffer is
    // trimmed along with its vertices.
    let large = reserved_vertices * std::mem::size_of::<Vertex>()
        + reserved_indices * std::mem::size_of::<u32>()
        >= SHRINK_MIN_BYTES;
    shrink_if_overgrown(&mut buffers.vertices, large && reserved_vertices != donor_vertices, outgrew_vertices);
    shrink_if_overgrown(&mut buffers.indices, large && reserved_indices != donor_indices, outgrew_indices);
    Ok((TileMesh { vertices: buffers.vertices, indices: buffers.indices }, spans))
}

/// [`tessellate_plan_with_ranges_reusing`] for an overlay source: the same
/// mesh contract, plus its [`OverlayExtras`] -- outline indices for every
/// fill op whose `outlines[op]` is `Some`, and the unit cuts the upload
/// chunks on.
pub(crate) fn tessellate_overlay_plan_reusing(
    plan: &PaintPlan,
    outlines: &[Option<Rgba>],
    extent: u32,
    coord_scale: f32,
    subdivision: Subdivision,
    cancel: &dyn Fn() -> bool,
    scratch: TileMesh,
) -> Result<(TileMesh, Vec<OpSpan>, OverlayExtras), TileMesh> {
    tessellate_overlay_plan_lod(plan, outlines, 0, extent, coord_scale, subdivision, cancel, scratch)
}

/// [`tessellate_overlay_plan_reusing`] with the outline's level of detail:
/// a polygon under `outline_min_extent` input units in both axes records no
/// outline edge (its fill is unchanged). See `OverlayExtras`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tessellate_overlay_plan_lod(
    plan: &PaintPlan,
    outlines: &[Option<Rgba>],
    outline_min_extent: i32,
    extent: u32,
    coord_scale: f32,
    subdivision: Subdivision,
    cancel: &dyn Fn() -> bool,
    scratch: TileMesh,
) -> Result<(TileMesh, Vec<OpSpan>, OverlayExtras), TileMesh> {
    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    buffers.vertices = scratch.vertices;
    buffers.indices = scratch.indices;
    buffers.vertices.clear();
    buffers.indices.clear();
    if cancel() {
        return Err(TileMesh { vertices: buffers.vertices, indices: buffers.indices });
    }
    let (want_vertices, want_indices) =
        estimate_buffer_capacity(&plan.ops, coord_scale, extent, subdivision);
    release_oversized(&mut buffers.vertices, want_vertices);
    release_oversized(&mut buffers.indices, want_indices);
    let donor_vertices = buffers.vertices.capacity();
    let donor_indices = buffers.indices.capacity();
    buffers.vertices.reserve_exact(want_vertices);
    buffers.indices.reserve_exact(want_indices);
    let reserved_vertices = buffers.vertices.capacity();
    let reserved_indices = buffers.indices.capacity();
    let mut extras = OverlayExtras { outline_min_extent, ..OverlayExtras::default() };
    let mut spans = Vec::with_capacity(plan.ops.len());
    if !append_ops_with(
        &mut buffers,
        &plan.ops,
        coord_scale,
        extent,
        subdivision,
        &mut spans,
        cancel,
        Some((outlines, &mut extras)),
    ) {
        return Err(TileMesh { vertices: buffers.vertices, indices: buffers.indices });
    }
    extras.finish();
    extras.outline.shrink_to_fit();
    extras.cuts.shrink_to_fit();
    let outgrew_vertices = buffers.vertices.capacity() > reserved_vertices;
    let outgrew_indices = buffers.indices.capacity() > reserved_indices;
    let large = reserved_vertices * std::mem::size_of::<Vertex>()
        + reserved_indices * std::mem::size_of::<u32>()
        >= SHRINK_MIN_BYTES;
    shrink_if_overgrown(&mut buffers.vertices, large && reserved_vertices != donor_vertices, outgrew_vertices);
    shrink_if_overgrown(&mut buffers.indices, large && reserved_indices != donor_indices, outgrew_indices);
    Ok((TileMesh { vertices: buffers.vertices, indices: buffers.indices }, spans, extras))
}

/// The vertices ONE op tessellates to, on its own, through the overlay's
/// path: identical to that op's slice of a whole-plan mesh (an op's
/// vertices do not depend on where in the buffer it lands, `append_ops`,
/// and subdivision works per op), so a paint-only update of a released
/// overlay mesh re-derives exactly the vertices
/// [`tessellate_overlay_plan_reusing`] baked, outline colour included.
pub(crate) fn tessellate_overlay_op_vertices(
    op: &PaintOp,
    outline: Option<Rgba>,
    extent: u32,
    coord_scale: f32,
    subdivision: Subdivision,
) -> Vec<Vertex> {
    let ops = std::slice::from_ref(op);
    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    reserve_for_ops(&mut buffers, ops, coord_scale, extent, subdivision);
    let mut spans = Vec::with_capacity(1);
    let mut extras = OverlayExtras::default();
    let outlines = [outline];
    append_ops_with(
        &mut buffers,
        ops,
        coord_scale,
        extent,
        subdivision,
        &mut spans,
        &|| false,
        Some((&outlines, &mut extras)),
    );
    buffers.vertices
}

/// Tessellates `ops` onto the end of `buffers`, pushing one `OpSpan` per op.
///
/// Split out of `tessellate_plan_with_ranges` so `splice_changed_ops` can
/// tessellate a RUN of ops through exactly the same code path, into a fresh
/// buffer, and get spans back in the same shape.
///
/// One `FillTessellator`, one `StrokeTessellator`, and three point scratch
/// `Vec`s are built ONCE here and threaded through every op in `ops`
/// (`append_fill`/`append_stroke`), rather than each op building its own
/// tessellator from empty and its own per-ring/per-run `Vec`s -- porting the
/// discipline OMIM's `BatchersPool` uses for its `Batcher`s (construct once,
/// check out and reuse per unit of work, never per op). This is safe for
/// output equivalence: see `append_fill`'s doc comment for why lyon's
/// tessellators carry no state across calls that affects the result, and
/// `pooled_tessellators_match_fresh_ones_byte_for_byte` for the proof. And
/// `splice_changed_ops` calling this on a fresh, smaller `ops` slice for
/// just the changed run still gets its own fresh tessellators/scratch --
/// pooling never crosses a call to `append_ops`, only ops WITHIN one call --
/// so the splice-equivalence property below is untouched by this change.
/// `BuffersBuilder` offsets the indices it emits by the buffer's length at
/// construction -- so an op's VERTICES are identical wherever it is
/// tessellated, and its INDICES differ only by that constant offset. That is
/// precisely the property the splice relies on to rebuild a mesh piecewise
/// and still match a full re-tessellation exactly.
///
/// Returns `false` if `cancel` stopped it early, in which case `buffers`
/// and `spans` hold a PARTIAL result and neither is usable -- the caller
/// must discard both rather than install a truncated mesh.
fn append_ops(
    buffers: &mut VertexBuffers<Vertex, u32>,
    ops: &[PaintOp],
    coord_scale: f32,
    extent: u32,
    subdivision: Subdivision,
    spans: &mut Vec<OpSpan>,
    cancel: &dyn Fn() -> bool,
) -> bool {
    append_ops_with(buffers, ops, coord_scale, extent, subdivision, spans, cancel, None)
}

/// [`append_ops`], with the overlay's two extras when `overlay` is `Some`:
/// each fill POLYGON is subdivided on its own and closes a triangulation
/// unit (so no triangle spans two units and a chunk boundary can fall
/// between any two), and a fill op with an outline colour (`overlay.0[op]`)
/// records its rings' edges as LINE-LIST indices over its own vertices.
/// See [`OverlayExtras`].
#[allow(clippy::too_many_arguments)]
fn append_ops_with(
    buffers: &mut VertexBuffers<Vertex, u32>,
    ops: &[PaintOp],
    coord_scale: f32,
    extent: u32,
    subdivision: Subdivision,
    spans: &mut Vec<OpSpan>,
    cancel: &dyn Fn() -> bool,
    mut overlay: Option<(&[Option<Rgba>], &mut OverlayExtras)>,
) -> bool {
    let mut fill_tess = FillTessellator::new();
    let mut stroke_tess = StrokeTessellator::new();
    let mut ring_scratch: Vec<Point> = Vec::new();
    let mut points_scratch: Vec<Point> = Vec::new();
    let mut local_scratch: Vec<Point> = Vec::new();
    // Holds one back-track's split-off sub-polyline at a time; empty and
    // unused for every line without one. See `append_stroke`'s `Some(
    // apexes)` branch.
    let mut segment_scratch: Vec<Point> = Vec::new();
    // One set of subdivision working buffers for the whole tile, threaded
    // through every op, for the same reason the tessellators are.
    let mut subdivide_scratch = SubdivideScratch::default();

    for op in ops {
        // Between ops, not inside one: lyon owns the loop over a single
        // op's geometry. See `tessellate_plan_with_ranges_cancellable`.
        if cancel() {
            return false;
        }
        let vertex_start = buffers.vertices.len();
        let index_start = buffers.indices.len();
        match op {
            // Extrusions go to `render::extrusion`'s own mesh; their span
            // here is empty so op indices stay aligned.
            PaintOp::Background { .. } | PaintOp::Extrusion { .. } => {}
            PaintOp::Fill { color_from, color_to, geometry } if overlay.is_some() => {
                let (outlines, extras) = overlay.as_mut().expect("guarded");
                let outline = outlines.get(spans.len()).copied().flatten();
                append_fill_overlay(
                    buffers,
                    geometry,
                    *color_from,
                    *color_to,
                    outline,
                    coord_scale,
                    extent,
                    subdivision,
                    &mut fill_tess,
                    &mut ring_scratch,
                    &mut subdivide_scratch,
                    extras,
                );
            }
            PaintOp::Fill { color_from, color_to, geometry } => {
                append_fill(
                    buffers,
                    geometry,
                    *color_from,
                    *color_to,
                    coord_scale,
                    &mut fill_tess,
                    &mut ring_scratch,
                );
                // AFTER lyon, which is the same position in the pipeline as
                // the reference's (it subdivides earcut's output,
                // `subdivision.ts:596-598`), and HERE rather than inside
                // `append_fill` because `vertex_start`/`index_start` above
                // already delimit exactly this op's triangles and
                // `BuffersBuilder` holds `buffers` borrowed for the whole of
                // that function. This is the call that makes the globe's
                // limb a circle; see `render::subdivide`.
                subdivide::subdivide_fill(
                    buffers,
                    vertex_start,
                    index_start,
                    subdivision,
                    extent,
                    coord_scale,
                    &mut subdivide_scratch,
                );
            }
            PaintOp::Line { color_from, color_to, width_from, width_to, gap_from, gap_to, cap, join, dash, metrics, geometry } => append_stroke(
                buffers,
                geometry,
                *color_from,
                *color_to,
                *width_from,
                *width_to,
                *gap_from,
                *gap_to,
                *cap,
                *join,
                *dash,
                *metrics,
                coord_scale,
                extent,
                subdivision,
                &mut stroke_tess,
                &mut points_scratch,
                &mut local_scratch,
                &mut segment_scratch,
                &mut subdivide_scratch,
            ),
        }
        let span = OpSpan {
            vertices: vertex_start..buffers.vertices.len(),
            indices: index_start..buffers.indices.len(),
        };
        if let Some((_, extras)) = overlay.as_mut() {
            // A line op (or anything else not cut per polygon) is ONE unit.
            extras.close_unit(buffers);
        }

        if matches!(op, PaintOp::Line { .. }) {
            let share = collapsed_normal_share(&buffers.vertices[span.vertices.clone()]);
            if share > 0.01 {
                eprintln!(
                    "gpui-maps: {:.0}% of a stroke op's normals are not unit vectors -- \
                     this line will draw thin or not at all. Its coordinates are probably \
                     too large for the tessellation half-width; see \
                     docs/superpowers/specs/2026-08-25-overlay-stroke-precision-design.md",
                    share * 100.0
                );
                crate::render::bad_normal_dump::maybe_dump_bad_normals(
                    op,
                    coord_scale,
                    extent,
                    &buffers.vertices[span.vertices.clone()],
                );
            }
        }

        spans.push(span);
    }
    true
}

/// Rewrites just the paint-dependent fields (`color`, `color_to`,
/// `half_width_px`, `half_width_px_to`, `gap_half_px`, `gap_half_px_to`,
/// `dash_runs`) of every vertex `mesh`
/// already holds, using `ranges` (as produced by
/// `tessellate_plan_with_ranges` for the SAME geometry, at an earlier zoom)
/// to find each op's slice of `mesh.vertices`. `position`, `normal`,
/// `is_stroke`, `line_distance`, `line_progress`, and `mesh.indices` are left
/// untouched --
/// they depend only on geometry, which by contract hasn't changed between
/// the tessellation that produced `ranges` and this call's `plan`.
///
/// Vertex count and topology are likewise untouched: `resolve_paint_dirty_
/// upload` re-uploads the buffer with `queue.write_buffer`, which wgpu
/// validates against the buffer's original byte size.
///
/// Panics if `ranges.len() != plan.ops.len()`: `ranges` must come from
/// tessellating a plan with the same ops (same geometry, same order) as
/// `plan`, just at a different zoom -- see this function's callers, which
/// only invoke it after confirming the tile's geometry set is unchanged.
pub fn rewrite_paint_attributes(mesh: &mut TileMesh, ranges: &[OpSpan], plan: &PaintPlan) {
    assert_eq!(
        ranges.len(),
        plan.ops.len(),
        "ranges must align 1:1 with plan.ops -- rewrite_paint_attributes must only be called \
         with ranges produced by tessellating a plan with the same ops/geometry as `plan`"
    );
    for (span, op) in ranges.iter().zip(&plan.ops) {
        rewrite_one_op(&mut mesh.vertices[span.vertices.clone()], op);
    }
}

/// `rewrite_paint_attributes`, restricted to the ops named in `ops`.
///
/// The wholesale form is what a ZOOM crossing needs: every op's paint was
/// resolved at the old bracket and every one of them is stale. A
/// paint-only FEATURE update is the other shape -- one route out of 206
/// changed colour, and rewriting the other 205 would touch 2.55M vertices
/// to write back the values they already hold, and then invalidate the
/// whole 204 MB vertex buffer for the sake of about 1 MB of it.
///
/// `ops` indexes `ranges`/`plan.ops`, and need not be sorted or unique --
/// rewriting an op twice writes the same values twice. Out-of-range
/// indices panic rather than being skipped: an index this function cannot
/// find is a caller whose feature-to-op map has drifted from the plan, and
/// the failure that mapping produces when it is merely wrong (rather than
/// out of bounds) is a silently mis-coloured feature. Loud is the only
/// useful direction.
///
/// Deliberately NOT expressed by relaxing the wholesale form's 1:1
/// assertion: that assertion is what stops a caller passing a `ranges`
/// table belonging to a different plan, and the zoom-crossing callers rely
/// on it.
pub fn rewrite_paint_attributes_for_ops(
    mesh: &mut TileMesh,
    ranges: &[OpSpan],
    plan: &PaintPlan,
    ops: &[usize],
) {
    for &op in ops {
        rewrite_one_op(&mut mesh.vertices[ranges[op].vertices.clone()], &plan.ops[op]);
    }
}

/// [`rewrite_one_op`] for a caller holding one op's vertex slice and a
/// paint op of its own making (the capture's story overlay overrides).
pub(crate) fn rewrite_op_paint(vertices: &mut [Vertex], op: &PaintOp) {
    rewrite_one_op(vertices, op);
}

/// Overwrites one op's paint-dependent vertex fields in place. The shared
/// body of the wholesale and subset rewrites, so the two cannot drift into
/// disagreeing about which fields are paint.
fn rewrite_one_op(vertices: &mut [Vertex], op: &PaintOp) {
    match op {
        // An extrusion's colour lives in the extrusion table
        // (`render::extrusion::rewrite_table`), and its span here is empty.
        PaintOp::Background { .. } | PaintOp::Extrusion { .. } => {}
        PaintOp::Fill { color_from, color_to, .. } => {
            let color = [color_from.r, color_from.g, color_from.b, color_from.a];
            let color_to = [color_to.r, color_to.g, color_to.b, color_to.a];
            for v in vertices {
                v.color = color;
                v.color_to = color_to;
            }
        }
        PaintOp::Line { color_from, color_to, width_from, width_to, gap_from, gap_to, dash, .. } => {
            let color = [color_from.r, color_from.g, color_from.b, color_from.a];
            let color_to = [color_to.r, color_to.g, color_to.b, color_to.a];
            let half_width_px = width_from / 2.0;
            let half_width_px_to = width_to / 2.0;
            let gap_half_px = gap_from / 2.0;
            let gap_half_px_to = gap_to / 2.0;
            let dash_runs = if dash.count == 0 { [0.0; 4] } else { dash.runs };
            for v in vertices {
                v.color = color;
                v.color_to = color_to;
                v.half_width_px = half_width_px;
                v.half_width_px_to = half_width_px_to;
                // Paint, not geometry: a casing that ramps with zoom must
                // follow its fill through a `rewrite_paint_attributes`
                // crossing, and lyon's triangulation is untouched by it --
                // the whole point of cutting the hole in `fs_main`.
                v.gap_half_px = gap_half_px;
                v.gap_half_px_to = gap_half_px_to;
                // NOT `v.line_distance` or `v.line_progress` -- both are
                // geometry-derived and survive a zoom crossing untouched.
                // See `Vertex`.
                v.dash_runs = dash_runs;
            }
        }
    }
}

/// The byte range a run of vertices occupies inside a vertex buffer -- what
/// a partial `wgpu::Queue::write_buffer` is given for a paint-only update
/// of part of a mesh.
///
/// Spelled out here, with its own test, rather than inlined at the call
/// site: an off-by-one in a vertex-buffer offset corrupts GEOMETRY (it
/// shifts positions and normals by a fraction of a vertex), which looks
/// nothing like the paint bug that would have caused it.
pub fn vertex_byte_range(vertices: &std::ops::Range<usize>) -> std::ops::Range<u64> {
    let stride = std::mem::size_of::<Vertex>() as u64;
    vertices.start as u64 * stride..vertices.end as u64 * stride
}

/// The vertex ranges the ops named in `ops` occupy, with runs that touch
/// merged into one.
///
/// `ops` must be ascending, which is what makes merging a single pass and
/// what `OverlayStore::plan_paint_update` produces. Empty spans are dropped
/// rather than merged through -- an op that tessellated to nothing sits
/// between its neighbours without joining them, and a zero-length write is
/// rejected by wgpu anyway.
///
/// Merging matters because ops from one feature are adjacent by
/// construction and consecutive features are adjacent too: recolouring a
/// contiguous run of features is one `write_buffer` rather than one per op.
pub fn coalesced_vertex_spans(
    ranges: &[OpSpan],
    ops: &[usize],
) -> Vec<std::ops::Range<usize>> {
    let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
    for &op in ops {
        let range = ranges[op].vertices.clone();
        if range.is_empty() {
            continue;
        }
        match spans.last_mut() {
            Some(last) if last.end == range.start => last.end = range.end,
            _ => spans.push(range),
        }
    }
    spans
}

/// True when two ops would tessellate to the same triangles -- same kind,
/// same geometry by VALUE. The paint fields are ignored, exactly as in
/// `map_view::ops_share_geometry_set` (which is this predicate lifted to
/// whole plans); see that function's doc comment for why geometry is
/// compared by value rather than by `Arc` pointer.
fn op_geometry_matches(a: &PaintOp, b: &PaintOp) -> bool {
    match (a, b) {
        (PaintOp::Background { .. }, PaintOp::Background { .. }) => true,
        (PaintOp::Fill { geometry: g1, .. }, PaintOp::Fill { geometry: g2, .. }) => g1 == g2,
        (PaintOp::Line { geometry: g1, .. }, PaintOp::Line { geometry: g2, .. }) => g1 == g2,
        (PaintOp::Extrusion { group: g1, row: r1, geometry: a }, PaintOp::Extrusion { group: g2, row: r2, geometry: b }) =>
            g1 == g2 && a == b && r1.same_shape(r2),
        _ => false,
    }
}

/// How many ops at the front, and how many at the back, `old` and `new`
/// share by geometry -- the alignment every splice is cut around.
///
/// Lifted out of [`splice_changed_ops`] so [`can_splice_changed_ops`] can
/// answer "would a splice work?" from the two PLANS alone, with no mesh and
/// no spans in hand. That question is asked on the `spawn_blocking` pool, by
/// `MapView::rebake_tile`, which holds the old plan (an `Arc` clone, free)
/// but deliberately does not hold the tile's triangulation -- see
/// `MapView::plan_for`'s stale-bucket branch. Shared rather than duplicated
/// so the pool's prediction and the UI thread's attempt can never disagree
/// about where the cut falls.
fn splice_alignment(old: &PaintPlan, new: &PaintPlan) -> (usize, usize) {
    let prefix = old
        .ops
        .iter()
        .zip(&new.ops)
        .take_while(|(a, b)| op_geometry_matches(a, b))
        .count();
    // Capped so prefix and suffix cannot overlap when one plan is a strict
    // extension of the other (e.g. ops [A, A, A] -> [A, A]: the prefix
    // already claims both survivors, and an uncapped suffix scan would claim
    // them again, yielding a negative-length middle).
    let max_suffix = (old.ops.len().min(new.ops.len())) - prefix;
    let suffix = old
        .ops
        .iter()
        .rev()
        .zip(new.ops.iter().rev())
        .take_while(|(a, b)| op_geometry_matches(a, b))
        .count()
        .min(max_suffix);
    (prefix, suffix)
}

/// Whether [`splice_changed_ops`] would splice these two plans rather than
/// decline -- i.e. whether they share any leading or trailing ops at all.
///
/// The predicate half of the splice, separated from the work so the decision
/// can be made where the plans are but the mesh is not. `MapView::
/// rebake_tile` runs on the blocking pool with the old plan and the new one
/// and nothing else; if this says yes, the arriving rebake splices the
/// resident triangulation on the UI thread (a few ops' worth of lyon), and
/// if it says no the pool tessellates the whole tile itself rather than
/// handing the UI thread that bill.
///
/// Exactly [`splice_changed_ops`]'s own `prefix == 0 && suffix == 0` test,
/// against the same [`splice_alignment`], so a `true` here cannot turn into
/// a `false` there for the same pair of plans.
pub fn can_splice_changed_ops(old: &PaintPlan, new: &PaintPlan) -> bool {
    let (prefix, suffix) = splice_alignment(old, new);
    prefix != 0 || suffix != 0
}

/// Re-tessellates only the ops that actually changed between `old` and
/// `new`, splicing the result into `mesh`/`spans` in place. Returns `false`
/// without touching anything when the two plans share no leading or trailing
/// ops at all, in which case the caller should just tessellate `new` from
/// scratch -- a splice of the whole op list would only add bookkeeping on
/// top of the same work. [`can_splice_changed_ops`] predicts that `false`
/// from the plans alone.
///
/// WHY THIS EXISTS. A display-zoom crossing at or above the tile source's
/// `max_zoom()` re-plans the SAME tiles (`visible_tiles` clamps tile z to
/// the ceiling), so a style layer whose zoom range ends there adds or
/// removes its features from an otherwise identical plan. Measured on
/// OpenFreeMap Liberty at the z14<->15 crossing, whose `building` fill layer
/// has `maxzoom: 14`: one or two ops differ out of 122-446, and treating
/// that as a whole-tile geometry change cost ~11ms of `plan_for` time for 10
/// visible tiles -- a re-triangulation of ~99.5% unchanged geometry, once
/// per crossing, in both directions, with nothing caching both variants.
///
/// HOW. `old` and `new` are aligned by common prefix and common suffix
/// rather than by a general diff: ops are emitted in style-layer order (see
/// `style::resolve_plan`), so a layer entering or leaving inserts or removes
/// one contiguous run, which is exactly what prefix/suffix alignment finds.
/// A general LCS would buy nothing here and would cost O(n^2) geometry
/// comparisons on plans of several hundred ops.
///
/// Only the misaligned middle is re-tessellated. The retained prefix and
/// suffix keep their triangulation; their PAINT still has to be refreshed,
/// which is the caller's job via `rewrite_paint_attributes` -- this function
/// deliberately does not do it, so that the "geometry unchanged" and
/// "geometry changed" paths share one paint-rewrite call site.
///
/// The result is exactly what tessellating `new` from scratch would
/// produce, vertex for vertex and index for index; `splicing_one_removed_op_
/// matches_a_full_retessellation` and its siblings pin that.
pub fn splice_changed_ops(
    mesh: &mut TileMesh,
    spans: &mut Vec<OpSpan>,
    old: &PaintPlan,
    new: &PaintPlan,
    coord_scale: f32,
    extent: u32,
    subdivision: Subdivision,
) -> bool {
    assert_eq!(
        spans.len(),
        old.ops.len(),
        "spans must align 1:1 with old.ops -- splice_changed_ops must only be called with spans \
         produced by tessellating `old`"
    );

    let (prefix, suffix) = splice_alignment(old, new);

    if prefix == 0 && suffix == 0 {
        return false;
    }

    let old_middle = prefix..old.ops.len() - suffix;
    let new_middle = prefix..new.ops.len() - suffix;

    // Where the middle sits in the buffers today. An op's span can be empty
    // (`Background`, or a geometry that tessellated to nothing), so the cut
    // points are taken from the surrounding spans rather than from the first
    // and last op of the middle: the end of the last retained prefix op, and
    // the start of the first retained suffix op. With no prefix that is 0;
    // with no suffix it is the buffer's end.
    let cut_vertices = spans.get(prefix.wrapping_sub(1)).map_or(0, |s| s.vertices.end)
        ..spans.get(old_middle.end).map_or(mesh.vertices.len(), |s| s.vertices.start);
    let cut_indices = spans.get(prefix.wrapping_sub(1)).map_or(0, |s| s.indices.end)
        ..spans.get(old_middle.end).map_or(mesh.indices.len(), |s| s.indices.start);

    // Tessellate the replacement run on its own. `append_ops` gives spans
    // relative to this fresh buffer; they are rebased onto the real one
    // below.
    let mut replacement: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    // Same reason as in `tessellate_plan_with_ranges`, and for the same
    // price: this buffer is only ever tile-sized (a splice runs on a tile's
    // plan, never the overlay's), so it is below the size that leaks -- but
    // it is the same growth pattern from the same empty `Vec`, and the
    // estimate costs one pass over the replacement ops' ring lengths.
    reserve_for_ops(&mut replacement, &new.ops[new_middle.clone()], coord_scale, extent, subdivision);
    let mut replacement_spans = Vec::with_capacity(new_middle.len());
    // The SAME subdivision the whole mesh was baked with, not
    // `Subdivision::NONE`: a spliced op must come out exactly as a full
    // re-tessellation would, which is what
    // `splicing_one_removed_op_matches_a_full_retessellation` pins, and a
    // splice at a different granularity would leave one op unsubdivided in
    // an otherwise subdivided tile.
    append_ops(
        &mut replacement,
        &new.ops[new_middle.clone()],
        coord_scale,
        extent,
        subdivision,
        &mut replacement_spans,
        // A splice runs on the UI thread inside one `set_feature_paint`
        // call, synchronously, with nothing able to supersede it before it
        // returns. There is no generation to poll and nothing to abort to.
        &|| false,
    );

    let vertex_delta = replacement.vertices.len() as isize - cut_vertices.len() as isize;
    let index_delta = replacement.indices.len() as isize - cut_indices.len() as isize;
    let vertex_base = cut_vertices.start;

    // Every index the replacement emitted is relative to its own empty
    // buffer, so it shifts by where the middle starts. Every index in the
    // RETAINED SUFFIX still points at the vertex numbering from before the
    // splice, so it shifts by however much the middle grew or shrank.
    // Indices in the retained prefix address vertices below the cut and need
    // no adjustment at all.
    let rebased = replacement.indices.iter().map(|i| i + vertex_base as u32).collect::<Vec<u32>>();
    mesh.vertices.splice(cut_vertices.clone(), replacement.vertices);
    mesh.indices.splice(cut_indices.clone(), rebased);
    let suffix_indices_start = (cut_indices.start as isize + replacement.indices.len() as isize) as usize;
    for index in &mut mesh.indices[suffix_indices_start..] {
        *index = (*index as isize + vertex_delta) as u32;
    }

    let shift = |range: &std::ops::Range<usize>, delta: isize| {
        (range.start as isize + delta) as usize..(range.end as isize + delta) as usize
    };
    let spliced: Vec<OpSpan> = replacement_spans
        .iter()
        .map(|span| OpSpan {
            vertices: shift(&span.vertices, vertex_base as isize),
            indices: shift(&span.indices, cut_indices.start as isize),
        })
        .chain(spans[old_middle.end..].iter().map(|span| OpSpan {
            vertices: shift(&span.vertices, vertex_delta),
            indices: shift(&span.indices, index_delta),
        }))
        .collect();
    spans.truncate(prefix);
    spans.extend(spliced);

    debug_assert_eq!(spans.len(), new.ops.len(), "a splice must leave one span per new op");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{DashPattern, PaintOp, PaintPlan, Rgba};
    use bytemuck::Zeroable;
    use geo_types::{Geometry, LineString, Polygon};
    use std::sync::Arc;

    fn red() -> Rgba {
        Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }
    }

    /// A ring whose edges are all axis-aligned but that doubles back on
    /// itself has no area: not a rectangle, so lyon draws it (as nothing).
    #[test]
    fn a_doubled_back_axis_ring_is_not_a_rectangle() {
        let ring = |v: Vec<(i32, i32)>| Polygon::new(LineString::from(v), vec![]);
        assert_eq!(axis_rect(&ring(vec![(0, 0), (10, 0), (10, 5), (10, 0), (0, 0)])), None);
        assert_eq!(axis_rect(&ring(vec![(0, 0), (10, 0), (0, 0), (0, 5), (0, 0)])), None);
        assert_eq!(axis_rect(&ring(vec![(0, 0), (10, 5), (10, 0), (0, 5), (0, 0)])), None, "a bow tie");
        assert_eq!(axis_rect(&ring(vec![(0, 0), (10, 0), (10, 5), (0, 5), (0, 0)])), Some([0, 0, 10, 5]));
        assert_eq!(axis_rect(&ring(vec![(0, 5), (0, 0), (10, 0), (10, 5), (0, 5)])), Some([0, 0, 10, 5]));
        let op = PaintOp::Fill {
            color_from: red(),
            color_to: red(),
            geometry: Arc::new(Geometry::Polygon(ring(vec![(0, 0), (10, 0), (10, 5), (10, 0), (0, 0)]))),
        };
        let (mesh, _, _) = tessellate_overlay_plan_lod(
            &PaintPlan { ops: vec![op] },
            &[None],
            0,
            4096,
            COORD_SCALE,
            crate::render::subdivide::Subdivision::NONE,
            &|| false,
            TileMesh::default(),
        )
        .unwrap();
        let area: f64 = mesh
            .indices
            .as_chunks::<3>().0.iter()
            .map(|t| {
                let [a, b, c] = [t[0], t[1], t[2]].map(|i| mesh.vertices[i as usize].position);
                (f64::from(b[0] - a[0]) * f64::from(c[1] - a[1]) - f64::from(c[0] - a[0]) * f64::from(b[1] - a[1])).abs()
            })
            .sum();
        assert_eq!(area, 0.0, "lyon draws nothing for it");
    }

    /// The axis-aligned-rectangle fast path (`axis_rect`) covers exactly
    /// what lyon's triangulation of the same rectangle covers: the same
    /// area, inside the same box, with no overlap -- on a subdivided tile,
    /// across a cell line too -- and the same outline edges.
    #[test]
    fn a_rectangle_written_directly_covers_what_lyon_covers() {
        let tri_area = |m: &TileMesh| -> f64 {
            m.indices
                .as_chunks::<3>().0.iter()
                .map(|t| {
                    let [a, b, c] = [t[0], t[1], t[2]].map(|i| m.vertices[i as usize].position);
                    (f64::from(b[0] - a[0]) * f64::from(c[1] - a[1]) - f64::from(c[0] - a[0]) * f64::from(b[1] - a[1])).abs() / 2.0
                })
                .sum()
        };
        let bbox = |m: &TileMesh| {
            m.vertices.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, v| {
                [b[0].min(v.position[0]), b[1].min(v.position[1]), b[2].max(v.position[0]), b[3].max(v.position[1])]
            })
        };
        let sub = crate::overlay::tile_plan::overlay_tile_subdivision(crate::geo::TileId { z: 3, x: 1, y: 2 });
        for (x0, y0, w, h) in [(100, 200, 3, 3), (2046, 1000, 4, 3), (4093, 4093, 3, 3), (10, 10, 900, 40)] {
            let fast = vec![(x0, y0), (x0 + w, y0), (x0 + w, y0 + h), (x0, y0 + h), (x0, y0)];
            // A collinear midpoint: not a 4-corner ring, so lyon tessellates it.
            let slow = vec![(x0, y0), (x0 + w, y0), (x0 + w, y0 + h), (x0, y0 + h), (x0, y0 + 1), (x0, y0)];
            let bake = |ring: Vec<(i32, i32)>| {
                let op = PaintOp::Fill {
                    color_from: red(),
                    color_to: red(),
                    geometry: Arc::new(Geometry::Polygon(Polygon::new(LineString::from(ring), vec![]))),
                };
                let (mesh, _, extras) = tessellate_overlay_plan_lod(
                    &PaintPlan { ops: vec![op] },
                    &[Some(red())],
                    0,
                    4096,
                    COORD_SCALE,
                    sub,
                    &|| false,
                    TileMesh::default(),
                )
                .unwrap();
                (mesh, extras.outline.len())
            };
            let ((a, ao), (b, bo)) = (bake(fast), bake(slow));
            let rect = f64::from(w) * f64::from(h) * f64::from(COORD_SCALE).powi(2);
            assert!((tri_area(&a) - rect).abs() < 1e-3 * rect, "{x0},{y0}: fast {} vs {rect}", tri_area(&a));
            assert!((tri_area(&b) - rect).abs() < 1e-3 * rect, "{x0},{y0}: lyon {} vs {rect}", tri_area(&b));
            assert_eq!(bbox(&a), bbox(&b));
            assert_eq!(ao, 8, "four edges");
            assert_eq!(bo, 10, "lyon's ring has five");
        }
    }

    fn square_polygon_with_hole() -> Arc<Geometry<i32>> {
        Arc::new(Geometry::Polygon(Polygon::new(
            LineString::from(vec![(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)]),
            vec![LineString::from(vec![
                (25, 25),
                (25, 75),
                (75, 75),
                (75, 25),
                (25, 25),
            ])],
        )))
    }

    fn diagonal_line() -> Arc<Geometry<i32>> {
        Arc::new(Geometry::LineString(LineString::from(vec![(0, 0), (100, 100)])))
    }

    /// A square at an offset, so a plan can hold several fills that are
    /// distinguishable by geometry (which is what the splice aligns on).
    fn square_at(x: i32, y: i32) -> Arc<Geometry<i32>> {
        Arc::new(Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (x, y),
                (x + 60, y),
                (x + 60, y + 60),
                (x, y + 60),
                (x, y),
            ]),
            vec![],
        )))
    }

    fn fill_at(x: i32, y: i32, color: Rgba) -> PaintOp {
        PaintOp::Fill { color_from: color, color_to: color, geometry: square_at(x, y) }
    }

    fn line_op(color: Rgba, width: f32) -> PaintOp {
        PaintOp::Line {
            metrics: false,
            color_from: color,
            color_to: color,
            width_from: width,
            width_to: width,
            gap_from: 0.0,
            gap_to: 0.0,
            cap: crate::style::LineCap::Butt,
            join: crate::style::LineJoin::Miter,
            dash: DashPattern::none(),
            geometry: diagonal_line(),
        }
    }

    // --- `estimate_buffer_capacity` ------------------------------------
    //
    // These are the only part of the capacity reservation that IS unit
    // testable. The leak it exists for is a property of libmalloc's page
    // recycling and shows up only as RSS across repeated bakes, so no test
    // in this file can demonstrate it; what these pin is the thing a future
    // change could plausibly break in a way that quietly reintroduces it --
    // the estimate drifting so far under the real count that the buffer is
    // back to growing by doubling.
    //
    // Every one asserts the SAME two-sided property, because both sides
    // matter and for different reasons: at least the real count (or the
    // doubling chain restarts, which is the leak), and no more than a
    // stated factor of it (or the reservation itself becomes the waste it
    // was meant to remove).

    /// `(estimated, real)` for one op, at the overlay's coordinate scale.
    fn estimate_vs_real(op: PaintOp) -> ((usize, usize), (usize, usize)) {
        let ops = [op];
        let estimated = estimate_buffer_capacity(&ops, COORD_SCALE, 4096, Subdivision::NONE);
        let (mesh, _) = tessellate_plan_with_ranges(
            &PaintPlan { ops: ops.to_vec() },
            1,
            OVERLAY_COORD_SCALE,
            Subdivision::NONE,
        );
        (estimated, (mesh.vertices.len(), mesh.indices.len()))
    }

    fn assert_covers_within(op: PaintOp, factor: f64, what: &str) {
        let ((est_v, est_i), (real_v, real_i)) = estimate_vs_real(op);
        assert!(
            est_v >= real_v && est_i >= real_i,
            "{what}: the estimate must cover the real output, or the buffer resumes doubling \
             (estimated {est_v} vertices / {est_i} indices, real {real_v} / {real_i})"
        );
        assert!(
            (est_v as f64) <= real_v as f64 * factor && (est_i as f64) <= real_i as f64 * factor,
            "{what}: the estimate must stay within {factor}x of the real output, or the \
             reservation is itself the waste (estimated {est_v} vertices / {est_i} indices, \
             real {real_v} / {real_i})"
        );
    }

    /// `LineJoin::Bevel`, deliberately, not the style spec's default of
    /// `Miter`: `STROKE_VERTICES_PER_POINT` is the BEVEL measurement, and
    /// these tests exist to pin the estimate against the shape that
    /// actually reaches it. A miter emits one vertex per join where a bevel
    /// emits two, so the same assertions under `Miter` would pass by a
    /// margin that says nothing about whether the ceiling still holds. See
    /// `a_round_joined_stroke_is_estimated_within_a_factor_of_two` for the
    /// join the ceiling does NOT hold for.
    fn stroke_op(geometry: Geometry<i32>) -> PaintOp {
        PaintOp::Line {
            metrics: false,
            color_from: red(),
            color_to: red(),
            width_from: 2.0,
            width_to: 2.0,
            gap_from: 0.0,
            gap_to: 0.0,
            cap: crate::style::LineCap::Butt,
            join: crate::style::LineJoin::Bevel,
            dash: DashPattern::none(),
            geometry: Arc::new(geometry),
        }
    }

    /// Same op with a round join -- what 44 of Liberty's line layers and 16
    /// of `examples/mapbox-style.json`'s actually ask for.
    fn round_joined_stroke_op(geometry: Geometry<i32>) -> PaintOp {
        match stroke_op(geometry) {
            PaintOp::Line { color_from, color_to, width_from, width_to, gap_from, gap_to, cap, dash, geometry, .. } => {
                PaintOp::Line {
                    metrics: false,
                    color_from,
                    color_to,
                    width_from,
                    width_to,
                    gap_from,
                    gap_to,
                    cap,
                    join: crate::style::LineJoin::Round,
                    dash,
                    geometry,
                }
            }
            other => other,
        }
    }

    /// A zig-zag of `n` points, `step` overlay units apart -- every interior
    /// point is a real bevel join, which is the case that emits the most
    /// vertices per point.
    fn zigzag(n: usize, step: i32) -> LineString<i32> {
        LineString::from(
            (0..n as i32).map(|i| (i * step, (i % 7) * (step - step / 10))).collect::<Vec<_>>(),
        )
    }

    /// The overlay's own shape, at a size worth reserving for: a
    /// `MultiLineString` of many long routes, which is exactly what
    /// `overlay::plan::build_plan` emits for a `LineString` feature.
    #[test]
    fn a_large_multi_line_stroke_is_estimated_within_five_percent() {
        let lines: Vec<LineString<i32>> = (0..40).map(|_| zigzag(5_000, 100)).collect();
        assert_covers_within(
            stroke_op(Geometry::MultiLineString(geo_types::MultiLineString(lines))),
            1.05,
            "200k-point multi-line stroke",
        );
    }

    /// A casing is a stroke with a HOLE, not a second stroke: it changes
    /// how far the vertex shader extrudes and where the fragment shader
    /// stops drawing, and it changes NOTHING lyon emits.
    ///
    /// This is the property the whole `line-gap-width` design rests on, and
    /// it is worth a test of its own because the obvious implementation --
    /// two offset polylines, stroked separately -- has the opposite
    /// property, and would have doubled every road's geometry and every
    /// road's tessellation time. It is also what makes the gap PAINT for
    /// `rewrite_paint_attributes`' purposes: a zoom crossing that ramps a
    /// casing rewrites two floats per vertex instead of retessellating the
    /// tile.
    #[test]
    fn a_casing_costs_no_geometry_at_all_only_a_wider_extrusion() {
        let line = Geometry::LineString(zigzag(500, 1_000));
        let solid = stroke_op(line.clone());
        let cased = match stroke_op(line) {
            PaintOp::Line { color_from, color_to, width_from, width_to, cap, join, dash, geometry, .. } => {
                PaintOp::Line {
                    metrics: false,
                    color_from,
                    color_to,
                    width_from,
                    width_to,
                    gap_from: 9.0,
                    gap_to: 11.0,
                    cap,
                    join,
                    dash,
                    geometry,
                }
            }
            other => other,
        };
        let (solid_mesh, _) =
            tessellate_plan_with_ranges(&PaintPlan { ops: vec![solid] }, 1, OVERLAY_COORD_SCALE, Subdivision::NONE);
        let (cased_mesh, ranges) =
            tessellate_plan_with_ranges(&PaintPlan { ops: vec![cased.clone()] }, 1, OVERLAY_COORD_SCALE, Subdivision::NONE);

        assert_eq!(
            (cased_mesh.vertices.len(), cased_mesh.indices.len()),
            (solid_mesh.vertices.len(), solid_mesh.indices.len()),
            "a gap must not add a single vertex or index"
        );
        for (c, s) in cased_mesh.vertices.iter().zip(&solid_mesh.vertices) {
            assert_eq!((c.position, c.normal), (s.position, s.normal), "and must not move one either");
        }

        // What it DOES change: half the declared gap, per vertex, in both
        // zoom brackets -- `render::shader.wgsl`'s `aa_inset_px`/`outset`.
        for v in &cased_mesh.vertices {
            assert_eq!((v.gap_half_px, v.gap_half_px_to), (4.5, 5.5));
        }
        for v in &solid_mesh.vertices {
            assert_eq!((v.gap_half_px, v.gap_half_px_to), (0.0, 0.0));
        }

        // ...and it survives a zoom crossing's paint rewrite, which is the
        // path a ramped casing actually takes at run time.
        let mut rewritten = solid_mesh.clone();
        rewrite_paint_attributes(&mut rewritten, &ranges, &PaintPlan { ops: vec![cased] });
        for v in &rewritten.vertices {
            assert_eq!((v.gap_half_px, v.gap_half_px_to), (4.5, 5.5), "the rewrite must carry the gap");
        }
    }

    /// `line-cap` and `line-join`, by contrast, ARE geometry: they reach
    /// lyon's `StrokeOptions`, and each keyword produces a different vertex
    /// count from the same path. Pinned as strict inequalities rather than
    /// as counts, because the counts are lyon's business and would move
    /// under a version bump; what must not change is that the style value
    /// reaches the tessellator at all, which is exactly what was untrue
    /// before these two properties were read.
    #[test]
    fn a_cap_and_a_join_reach_lyon_and_change_what_it_emits() {
        use crate::style::{LineCap, LineJoin};
        let mesh = |cap: LineCap, join: LineJoin| {
            // Twenty separate short subpaths, so the caps are a real share
            // of the output rather than two vertices in a thousand.
            let lines: Vec<LineString<i32>> = (0..20).map(|_| zigzag(40, 1_000)).collect();
            let geometry = Geometry::MultiLineString(geo_types::MultiLineString(lines));
            let op = match stroke_op(geometry) {
                PaintOp::Line { color_from, color_to, width_from, width_to, gap_from, gap_to, dash, metrics, geometry, .. } => {
                    PaintOp::Line { color_from, color_to, width_from, width_to, gap_from, gap_to, cap, join, dash, metrics, geometry }
                }
                other => other,
            };
            tessellate_plan_with_ranges(&PaintPlan { ops: vec![op] }, 1, OVERLAY_COORD_SCALE, Subdivision::NONE).0
        };

        let butt_bevel = mesh(LineCap::Butt, LineJoin::Bevel);
        let n = butt_bevel.vertices.len();
        assert!(
            mesh(LineCap::Round, LineJoin::Bevel).vertices.len() > n,
            "a round cap must add a fan at each subpath end"
        );
        assert!(
            mesh(LineCap::Butt, LineJoin::Round).vertices.len() > n,
            "a round join must add a fan at each corner"
        );
        assert!(
            mesh(LineCap::Butt, LineJoin::Miter).vertices.len() < n,
            "a miter join emits ONE vertex where a bevel emits two"
        );

        // A SQUARE cap is the one keyword that adds no vertices: lyon keeps
        // the two the butt cap already had and tilts their normals outward
        // along the path instead, which the vertex shader then extrudes into
        // the half-width overhang the spec asks for. So it is pinned on the
        // normals, not on the count -- and pinned on the LAST subpath's end,
        // where a butt cap's normal is exactly perpendicular and a square
        // cap's is not.
        let square = mesh(LineCap::Square, LineJoin::Bevel);
        assert_eq!(square.vertices.len(), n, "a square cap reuses the butt cap's two vertices");
        assert!(
            square.vertices.iter().zip(&butt_bevel.vertices).any(|(a, b)| a.normal != b.normal),
            "a square cap must tilt the end normals outward -- it changed nothing at all"
        );
    }

    /// The one join whose estimate is NOT a ceiling, pinned in both
    /// directions so that "typical, not worst case" stays a measured claim
    /// rather than an excuse.
    ///
    /// `STROKE_VERTICES_PER_POINT_ROUND_JOIN` is 6 because that is what a
    /// moderate turn costs. A gentle turn costs 3 and a near-hairpin costs
    /// 10, so the estimate is deliberately allowed to land on either side of
    /// the real count -- what it may not do is miss by the order of
    /// magnitude that would put the buffer back on a doubling chain (see
    /// `reserve_for_ops`). Two zig-zags at opposite ends of the measured
    /// range, one under the estimate and one over it, bound both directions
    /// at 2x.
    #[test]
    fn a_round_joined_stroke_is_estimated_within_a_factor_of_two() {
        // `zigzag`'s step-to-amplitude ratio puts these at roughly 25 and 85
        // degrees of turn respectively -- the gentle end of real road
        // geometry, and the sharp end.
        for (n, step) in [(2_000usize, 10_000i32), (2_000, 1_000)] {
            let ((est_v, est_i), (real_v, real_i)) =
                estimate_vs_real(round_joined_stroke_op(Geometry::LineString(zigzag(n, step))));
            for (est, real, what) in [(est_v, real_v, "vertices"), (est_i, real_i, "indices")] {
                assert!(
                    est * 2 >= real && real * 2 >= est,
                    "round join, {n} points at step {step}: estimated {est} {what}, real {real} \
                     -- the round-join estimate may miss, but not by more than 2x either way"
                );
            }
            // ...and the indices stay three per vertex to within the handful
            // the two end caps' missing joins account for, which is the
            // relation `STROKE_INDICES_PER_POINT_ROUND_JOIN` is derived from.
            assert!(
                real_i.abs_diff(real_v * 3) < 32,
                "round-join indices are 3x vertices at every turn angle: {real_i} vs {real_v} x 3"
            );
        }
    }

    /// Short strokes are where the per-point count is furthest from its
    /// asymptote -- the two caps' missing joins are most of a 2-point line,
    /// which really does tessellate to 4 vertices and 6 indices where the
    /// estimate says 6 and 18. Bounded in ABSOLUTE terms rather than as a
    /// ratio, because a ratio is meaningless at n=2 and the thing that
    /// actually matters is that the overshoot stays a handful of vertices
    /// however many short ops a plan holds.
    #[test]
    fn short_strokes_are_estimated_generously_but_not_wildly() {
        for n in [2usize, 3, 10, 100] {
            let ((est_v, est_i), (real_v, real_i)) =
                estimate_vs_real(stroke_op(Geometry::LineString(zigzag(n, 1_000))));
            assert!(
                est_v >= real_v && est_i >= real_i,
                "{n}-point stroke: estimated {est_v}/{est_i}, real {real_v}/{real_i}"
            );
            // A cap's worth of fixed overshoot (the joins the two ends do
            // not have), plus the 1/64 headroom, plus rounding.
            let bound = |real: usize| real + 16 + real / 32;
            assert!(
                est_v <= bound(real_v) && est_i <= bound(real_i),
                "{n}-point stroke overshoots by more than the caps and the headroom: estimated \
                 {est_v}/{est_i}, real {real_v}/{real_i}"
            );
        }
    }

    /// The case the headroom exists for. A line spanning more than
    /// `run_split::MAX_RUN_EXTENT` is tessellated as several runs, and
    /// neighbouring runs OVERLAP BY ONE POINT -- so lyon sees more points
    /// than the geometry has, and a flat `3 * points` estimate would land
    /// under the real count.
    #[test]
    fn a_line_long_enough_to_be_split_into_runs_is_still_covered() {
        // 400 points 100k units apart: 40e6 units across, five times
        // `MAX_RUN_EXTENT`.
        let line = zigzag(400, 100_000);
        let ((est_v, _), (real_v, _)) = estimate_vs_real(stroke_op(Geometry::LineString(line)));
        assert!(
            est_v >= real_v,
            "a split line must still be covered -- this is what ESTIMATE_HEADROOM_DIVISOR buys \
             (estimated {est_v}, real {real_v})"
        );
    }

    /// A dot's vertex count is a CONSTANT, not a function of its radius:
    /// `append_stroke` flattens every round cap at `STROKE_TESSELLATION_
    /// WIDTH`. So this is pinned exactly rather than within a factor -- if
    /// it ever stops holding, `STROKE_TESSELLATION_WIDTH`'s cache-
    /// determinism invariant has been broken and the estimate is the least
    /// of the problems.
    #[test]
    fn dot_capacity_is_estimated_exactly() {
        let dots: Vec<geo_types::Point<i32>> =
            (0..250).map(|i| geo_types::Point::new(i * 1_000, 0)).collect();
        let op = stroke_op(Geometry::MultiPoint(geo_types::MultiPoint(dots)));
        let (_, (real_v, real_i)) = estimate_vs_real(op);
        assert_eq!(real_v, 250 * DOT_VERTICES, "DOT_VERTICES no longer matches lyon's round cap");
        assert_eq!(real_i, 250 * DOT_INDICES, "DOT_INDICES no longer matches lyon's round cap");
    }

    /// A dot's radius must not move the estimate, for the same reason.
    #[test]
    fn a_dots_radius_does_not_change_its_estimate_or_its_real_count() {
        let dots = geo_types::MultiPoint(vec![geo_types::Point::new(0, 0)]);
        let at = |width: f32| PaintOp::Line {
            metrics: false,
            color_from: red(),
            color_to: red(),
            width_from: width,
            width_to: width,
            gap_from: 0.0,
            gap_to: 0.0,
            cap: crate::style::LineCap::Butt,
            join: crate::style::LineJoin::Miter,
            dash: DashPattern::none(),
            geometry: Arc::new(Geometry::MultiPoint(dots.clone())),
        };
        assert_eq!(estimate_vs_real(at(2.0)), estimate_vs_real(at(64.0)));
    }

    #[test]
    fn a_large_fill_is_estimated_within_five_percent() {
        // A 4000-point convex ring plus two holes -- the shape a
        // multipolygon landcover feature has.
        let ring = |n: usize, r: f64, cx: f64, cy: f64| {
            let mut points: Vec<(i32, i32)> = (0..n)
                .map(|i| {
                    let a = std::f64::consts::TAU * i as f64 / n as f64;
                    ((cx + r * a.cos()) as i32, (cy + r * a.sin()) as i32)
                })
                .collect();
            points.push(points[0]);
            LineString::from(points)
        };
        let polygon = Polygon::new(
            ring(4_000, 500_000.0, 0.0, 0.0),
            vec![ring(500, 80_000.0, -200_000.0, 0.0), ring(500, 80_000.0, 200_000.0, 0.0)],
        );
        assert_covers_within(
            PaintOp::Fill {
                color_from: red(),
                color_to: red(),
                geometry: Arc::new(Geometry::MultiPolygon(geo_types::MultiPolygon(vec![polygon]))),
            },
            1.05,
            "5000-point multipolygon fill",
        );
    }

    /// The estimate must count only what the tessellator actually draws.
    /// `append_fill` ignores line geometry and `append_stroke` ignores
    /// polygons -- both emit nothing -- so reserving for them would be pure
    /// waste, and `PaintOp::Background` has no geometry at all.
    #[test]
    fn ops_that_tessellate_to_nothing_are_estimated_at_zero() {
        let ops = vec![
            PaintOp::Background { color: red() },
            PaintOp::Fill { color_from: red(), color_to: red(), geometry: diagonal_line() },
            PaintOp::Line {
                metrics: false,
                color_from: red(),
                color_to: red(),
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: square_polygon_with_hole(),
            },
        ];
        assert_eq!(estimate_buffer_capacity(&ops, COORD_SCALE, 4096, Subdivision::NONE), (0, 0));
        let (mesh, _) = tessellate_plan_with_ranges(&PaintPlan { ops }, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(mesh.vertices.is_empty() && mesh.indices.is_empty());
    }

    /// A whole plan's estimate is the sum of its ops', including the ops
    /// that contribute nothing -- the property that lets one pass over the
    /// plan size both buffers.
    #[test]
    fn a_mixed_plan_is_covered_by_one_reservation() {
        let ops = vec![
            PaintOp::Background { color: red() },
            fill_at(0, 0, red()),
            line_op(red(), 2.0),
            stroke_op(Geometry::MultiLineString(geo_types::MultiLineString(vec![
                zigzag(2_000, 100),
                zigzag(1_500, 100),
            ]))),
            stroke_op(Geometry::MultiPoint(geo_types::MultiPoint(
                (0..100).map(|i| geo_types::Point::new(i * 500, 0)).collect(),
            ))),
        ];
        let (est_v, est_i) = estimate_buffer_capacity(&ops, COORD_SCALE, 4096, Subdivision::NONE);
        let (mesh, _) =
            tessellate_plan_with_ranges(&PaintPlan { ops }, 1, OVERLAY_COORD_SCALE, Subdivision::NONE);
        assert!(
            est_v >= mesh.vertices.len() && est_i >= mesh.indices.len(),
            "estimated {est_v}/{est_i}, real {}/{}",
            mesh.vertices.len(),
            mesh.indices.len()
        );
        assert!(
            (est_v as f64) <= mesh.vertices.len() as f64 * 1.05
                && (est_i as f64) <= mesh.indices.len() as f64 * 1.05,
            "estimated {est_v}/{est_i}, real {}/{}",
            mesh.vertices.len(),
            mesh.indices.len()
        );
    }

    /// The reservation must actually be USED -- a `tessellate_plan_with_
    /// ranges` that estimated correctly and then forgot to call
    /// `reserve_for_ops` would pass every test above and still leak. This is
    /// the closest a unit test gets to the bug: after the bake, the vertex
    /// buffer's capacity must be the reserved one, not a power-of-two
    /// doubling landing point.
    #[test]
    fn a_baked_mesh_holds_the_reserved_capacity_not_a_doubling_landing_point() {
        let ops = vec![stroke_op(Geometry::MultiLineString(geo_types::MultiLineString(
            (0..10).map(|_| zigzag(3_000, 100)).collect(),
        )))];
        let (est_v, est_i) = estimate_buffer_capacity(&ops, COORD_SCALE, 4096, Subdivision::NONE);
        let (mesh, _) =
            tessellate_plan_with_ranges(&PaintPlan { ops }, 1, OVERLAY_COORD_SCALE, Subdivision::NONE);
        assert_eq!(
            mesh.vertices.capacity(),
            est_v,
            "the estimate covered the mesh, so no realloc can have happened -- a capacity other \
             than the reserved one means `reserve_for_ops` was not called (or not with \
             `reserve_exact`)"
        );
        assert_eq!(mesh.indices.capacity(), est_i);
    }

    /// Splices `new` over `old` and asserts the outcome is indistinguishable
    /// from tessellating `new` from scratch -- vertices, indices, and the
    /// per-op spans that later paint rewrites depend on.
    ///
    /// The span check is not redundant with the buffer checks: a splice that
    /// produced the right buffers but left `spans` misaligned would corrupt
    /// the NEXT crossing's `rewrite_paint_attributes` instead of this one,
    /// which no vertex comparison here would catch.
    fn assert_splice_matches_full(old: &PaintPlan, new: &PaintPlan) {
        let (mut mesh, mut spans) = tessellate_plan_with_ranges(old, 4096, COORD_SCALE, Subdivision::NONE);
        let (expected_mesh, expected_spans) = tessellate_plan_with_ranges(new, 4096, COORD_SCALE, Subdivision::NONE);

        assert!(
            splice_changed_ops(&mut mesh, &mut spans, old, new, COORD_SCALE, 4096, Subdivision::NONE),
            "these plans share ops, so the splice must take the incremental path"
        );

        assert_eq!(mesh.vertices, expected_mesh.vertices, "spliced vertices");
        assert_eq!(mesh.indices, expected_mesh.indices, "spliced indices");
        assert_eq!(spans, expected_spans, "spliced spans");
    }

    /// The z14<->15 crossing in miniature: one op disappears from the middle
    /// of a plan whose other ops are untouched. This is the case the whole
    /// splice exists for -- on real Liberty tiles it is one or two `building`
    /// fills out of 122-446 ops.
    #[test]
    fn splicing_one_removed_op_matches_a_full_retessellation() {
        let old = PaintPlan {
            ops: vec![
                PaintOp::Background { color: red() },
                fill_at(0, 0, red()),
                fill_at(200, 0, red()), // the "building"
                fill_at(400, 0, red()),
                line_op(red(), 4.0),
            ],
        };
        let new = PaintPlan {
            ops: vec![
                PaintOp::Background { color: red() },
                fill_at(0, 0, red()),
                fill_at(400, 0, red()),
                line_op(red(), 4.0),
            ],
        };
        assert_splice_matches_full(&old, &new);
    }

    /// The same crossing taken the other way: zooming back out re-adds the
    /// op. Both directions matter -- the measured stall was symmetric.
    #[test]
    fn splicing_one_inserted_op_matches_a_full_retessellation() {
        let without = PaintPlan {
            ops: vec![
                PaintOp::Background { color: red() },
                fill_at(0, 0, red()),
                fill_at(400, 0, red()),
                line_op(red(), 4.0),
            ],
        };
        let with = PaintPlan {
            ops: vec![
                PaintOp::Background { color: red() },
                fill_at(0, 0, red()),
                fill_at(200, 0, red()),
                fill_at(400, 0, red()),
                line_op(red(), 4.0),
            ],
        };
        assert_splice_matches_full(&without, &with);
    }

    /// No suffix: the changed run reaches the end of the plan, so the cut
    /// point has no following span to read and must fall back to the
    /// buffers' own length.
    #[test]
    fn splicing_a_change_at_the_very_end_matches_a_full_retessellation() {
        let old = PaintPlan {
            ops: vec![fill_at(0, 0, red()), fill_at(200, 0, red()), line_op(red(), 4.0)],
        };
        let new = PaintPlan { ops: vec![fill_at(0, 0, red()), fill_at(200, 0, red())] };
        assert_splice_matches_full(&old, &new);
    }

    /// No prefix: the changed run starts at op 0, so the cut point has no
    /// preceding span and must fall back to 0. `prefix.wrapping_sub(1)`
    /// underflows to `usize::MAX` here, which `spans.get` is relied on to
    /// turn into `None` rather than a panic.
    #[test]
    fn splicing_a_change_at_the_very_start_matches_a_full_retessellation() {
        let old = PaintPlan {
            ops: vec![fill_at(0, 0, red()), fill_at(200, 0, red()), line_op(red(), 4.0)],
        };
        let new = PaintPlan { ops: vec![fill_at(200, 0, red()), line_op(red(), 4.0)] };
        assert_splice_matches_full(&old, &new);
    }

    /// Several ops leaving at once -- a whole style layer's features rather
    /// than a single feature.
    #[test]
    fn splicing_a_multi_op_run_matches_a_full_retessellation() {
        let old = PaintPlan {
            ops: vec![
                fill_at(0, 0, red()),
                fill_at(200, 0, red()),
                fill_at(400, 0, red()),
                fill_at(600, 0, red()),
                line_op(red(), 4.0),
            ],
        };
        let new = PaintPlan { ops: vec![fill_at(0, 0, red()), line_op(red(), 4.0)] };
        assert_splice_matches_full(&old, &new);
    }

    /// An op changing KIND at one position, with matching ops either side.
    /// `op_geometry_matches` must refuse to align a Fill with a Line even
    /// though neither is "missing".
    #[test]
    fn splicing_an_op_that_changed_kind_matches_a_full_retessellation() {
        let old = PaintPlan {
            ops: vec![fill_at(0, 0, red()), fill_at(200, 0, red()), line_op(red(), 4.0)],
        };
        let new =
            PaintPlan { ops: vec![fill_at(0, 0, red()), line_op(red(), 6.0), line_op(red(), 4.0)] };
        assert_splice_matches_full(&old, &new);
    }

    /// Identical geometry either side of a repeated op: the prefix scan
    /// claims every surviving op, and an uncapped suffix scan would claim
    /// them a second time and compute a negative-length middle. The `min`
    /// on `max_suffix` is what prevents that; without it this panics.
    #[test]
    fn splicing_a_repeated_op_out_of_a_run_of_identical_ops() {
        let old = PaintPlan {
            ops: vec![fill_at(0, 0, red()), fill_at(0, 0, red()), fill_at(0, 0, red())],
        };
        let new = PaintPlan { ops: vec![fill_at(0, 0, red()), fill_at(0, 0, red())] };
        assert_splice_matches_full(&old, &new);
    }

    /// Two plans with nothing in common at either end: the splice declines,
    /// leaving the mesh untouched so the caller can tessellate from scratch.
    /// Reporting success here would be worse than useless -- the caller
    /// would ship a mesh built for the wrong plan.
    #[test]
    fn splice_declines_when_no_ops_are_shared() {
        let old = PaintPlan { ops: vec![fill_at(0, 0, red()), line_op(red(), 4.0)] };
        let new = PaintPlan { ops: vec![line_op(red(), 4.0), fill_at(0, 0, red())] };

        let (mut mesh, mut spans) = tessellate_plan_with_ranges(&old, 4096, COORD_SCALE, Subdivision::NONE);
        let before = mesh.clone();

        assert!(!splice_changed_ops(&mut mesh, &mut spans, &old, &new, COORD_SCALE, 4096, Subdivision::NONE));
        assert_eq!(mesh.vertices, before.vertices, "a declined splice must not touch the mesh");
        assert_eq!(mesh.indices, before.indices, "a declined splice must not touch the mesh");
        assert_eq!(spans.len(), old.ops.len(), "a declined splice must not touch the spans");
    }

    /// `can_splice_changed_ops` must agree with `splice_changed_ops` on
    /// every shape this module has a fixture for.
    ///
    /// The two are called from different THREADS and a disagreement is not
    /// a wrong answer, it is a wasted round trip: `MapView::rebake_tile`
    /// runs the predicate on the blocking pool to decide whether to
    /// tessellate the tile itself or leave it to a splice on the UI thread,
    /// and a `true` that the UI thread then declines costs a whole extra
    /// frame and a whole extra `style::resolve_plan` (see
    /// `MapView::rebakes_restarted`). They share `splice_alignment` so that
    /// they cannot drift; this is the test that says so.
    #[test]
    fn can_splice_changed_ops_predicts_what_splice_changed_ops_does() {
        let background = || PaintOp::Background { color: red() };
        let cases: Vec<(&str, PaintPlan, PaintPlan)> = vec![
            (
                "one op removed from the middle -- the z14<->15 crossing",
                PaintPlan { ops: vec![background(), fill_at(0, 0, red()), fill_at(200, 0, red()), line_op(red(), 4.0)] },
                PaintPlan { ops: vec![background(), fill_at(0, 0, red()), line_op(red(), 4.0)] },
            ),
            (
                "a change at the very end -- prefix only",
                PaintPlan { ops: vec![fill_at(0, 0, red()), fill_at(200, 0, red()), line_op(red(), 4.0)] },
                PaintPlan { ops: vec![fill_at(0, 0, red()), fill_at(200, 0, red())] },
            ),
            (
                "paint-only: identical geometry throughout",
                PaintPlan { ops: vec![background(), fill_at(0, 0, red()), line_op(red(), 4.0)] },
                PaintPlan { ops: vec![PaintOp::Background { color: blue() }, fill_at(0, 0, blue()), line_op(blue(), 4.0)] },
            ),
            (
                "nothing shared at either end -- the one shape that declines",
                PaintPlan { ops: vec![fill_at(0, 0, red()), line_op(red(), 4.0)] },
                PaintPlan { ops: vec![line_op(red(), 4.0), fill_at(0, 0, red())] },
            ),
            (
                "an empty old plan: nothing to align on",
                PaintPlan { ops: vec![] },
                PaintPlan { ops: vec![fill_at(0, 0, red())] },
            ),
        ];
        for (name, old, new) in cases {
            let (mut mesh, mut spans) = tessellate_plan_with_ranges(&old, 4096, COORD_SCALE, Subdivision::NONE);
            let predicted = can_splice_changed_ops(&old, &new);
            let actual = splice_changed_ops(&mut mesh, &mut spans, &old, &new, COORD_SCALE, 4096, Subdivision::NONE);
            assert_eq!(predicted, actual, "{name}: the pool's prediction must match what the splice does");
        }
    }

    /// The real crossing, end to end: geometry changes in the middle AND
    /// every op's paint changes, because that is what crossing a zoom bucket
    /// does. Splice, then rewrite paint, and the result must still be
    /// indistinguishable from a full tessellation of the new plan -- this is
    /// the property `plan_for` relies on.
    #[test]
    fn splicing_then_rewriting_paint_matches_a_full_retessellation() {
        let old = PaintPlan {
            ops: vec![
                PaintOp::Background { color: red() },
                fill_at(0, 0, red()),
                fill_at(200, 0, red()),
                line_op(red(), 4.0),
            ],
        };
        let new = PaintPlan {
            ops: vec![
                PaintOp::Background { color: blue() },
                PaintOp::Fill { color_from: blue(), color_to: green(), geometry: square_at(0, 0) },
                PaintOp::Line {
                    metrics: false,
                    color_from: green(),
                    color_to: blue(),
                    width_from: 8.0,
                    width_to: 12.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: DashPattern::none(),
                    geometry: diagonal_line(),
                },
            ],
        };

        let (mut mesh, mut spans) = tessellate_plan_with_ranges(&old, 4096, COORD_SCALE, Subdivision::NONE);
        let expected = tessellate_plan(&new, 4096, COORD_SCALE, Subdivision::NONE);

        assert!(splice_changed_ops(&mut mesh, &mut spans, &old, &new, COORD_SCALE, 4096, Subdivision::NONE));
        rewrite_paint_attributes(&mut mesh, &spans, &new);

        assert_eq!(mesh.vertices, expected.vertices);
        assert_eq!(mesh.indices, expected.indices);
    }

    /// Two crossings in a row on the same mesh. A splice that left `spans`
    /// subtly wrong would still pass every single-splice test above and only
    /// corrupt the mesh on the next crossing.
    #[test]
    fn splicing_twice_in_a_row_matches_a_full_retessellation() {
        let a = PaintPlan {
            ops: vec![fill_at(0, 0, red()), fill_at(200, 0, red()), line_op(red(), 4.0)],
        };
        let b = PaintPlan { ops: vec![fill_at(0, 0, red()), line_op(red(), 4.0)] };
        let c = PaintPlan {
            ops: vec![fill_at(0, 0, red()), fill_at(400, 0, red()), line_op(red(), 4.0)],
        };

        let (mut mesh, mut spans) = tessellate_plan_with_ranges(&a, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(splice_changed_ops(&mut mesh, &mut spans, &a, &b, COORD_SCALE, 4096, Subdivision::NONE));
        assert!(splice_changed_ops(&mut mesh, &mut spans, &b, &c, COORD_SCALE, 4096, Subdivision::NONE));

        let (expected_mesh, expected_spans) = tessellate_plan_with_ranges(&c, 4096, COORD_SCALE, Subdivision::NONE);
        assert_eq!(mesh.vertices, expected_mesh.vertices);
        assert_eq!(mesh.indices, expected_mesh.indices);
        assert_eq!(spans, expected_spans);
    }

    /// The guard measures the OUTPUT invariant -- a stroke normal's length
    /// is 1 for a straight run and never *shorter*, though a miter join
    /// legitimately makes it longer -- rather than any coordinate
    /// magnitude, so it keeps working if the scales or the bound ever
    /// change.
    #[test]
    fn collapsed_normals_are_detected() {
        let good = vec![
            Vertex { normal: [1.0, 0.0], is_stroke: 1.0, ..Vertex::zeroed() },
            Vertex { normal: [0.0, 1.0], is_stroke: 1.0, ..Vertex::zeroed() },
        ];
        assert_eq!(collapsed_normal_share(&good), 0.0);

        let bad = vec![
            Vertex { normal: [0.0, 0.0], is_stroke: 1.0, ..Vertex::zeroed() },
            Vertex { normal: [1.0, 0.0], is_stroke: 1.0, ..Vertex::zeroed() },
        ];
        assert_eq!(collapsed_normal_share(&bad), 0.5);

        // Fill vertices legitimately carry a zero normal and must not count.
        let fills = vec![Vertex { normal: [0.0, 0.0], is_stroke: 0.0, ..Vertex::zeroed() }];
        assert_eq!(collapsed_normal_share(&fills), 0.0);

        // A miter join's normal legitimately exceeds unit length -- it must
        // not be mistaken for collapse. Only a length near zero is a defect.
        let miter_and_collapsed = vec![
            Vertex { normal: [2.0, 0.0], is_stroke: 1.0, ..Vertex::zeroed() },
            Vertex { normal: [0.0, 0.0], is_stroke: 1.0, ..Vertex::zeroed() },
        ];
        assert_eq!(collapsed_normal_share(&miter_and_collapsed), 0.5);
    }

    /// `GPUI_MAP_DUMP_BAD_NORMALS`: forces the defect the warning exists to
    /// catch -- a stroke op whose own vertex slice holds a collapsed
    /// (near-zero) normal, the exact shape a coordinate too large for
    /// `STROKE_TESSELLATION_WIDTH`'s half-width produces inside lyon (see
    /// `run_split`'s module doc; the consumer report this feature is for
    /// could not reproduce that collapse synthetically, so this test
    /// targets the dump's own contract -- given a bad-normal op, is
    /// everything the spec asks for written -- rather than lyon's internal
    /// precision behaviour, which is exactly what could not be pinned down
    /// from the outside). Goes through the real, public
    /// `maybe_dump_bad_normals` entry point `mesh::append_ops_with` calls,
    /// with a tiled `BakeContext` in scope so the tile id and "tiled" flag
    /// are also exercised.
    ///
    /// Reads/writes the process-global `GPUI_MAP_DUMP_BAD_NORMALS` env var
    /// and the dump directory it names; safe here because this is the only
    /// test in the crate that calls `maybe_dump_bad_normals`, so it is the
    /// only caller that can ever populate `bad_normal_dump`'s cached
    /// `OnceLock` read of that var.
    #[test]
    fn a_bad_normal_op_is_dumped_when_the_env_var_is_set() {
        let dir = std::env::temp_dir()
            .join(format!("gpui_map_bad_normal_dump_test_{}_{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Safety: no other thread in this process reads or writes
        // `GPUI_MAP_DUMP_BAD_NORMALS` -- it exists solely for
        // `bad_normal_dump::dump_dir`'s cached read, and this is the only
        // test that can make that read observe a non-empty value (see the
        // doc comment above).
        unsafe { std::env::set_var("GPUI_MAP_DUMP_BAD_NORMALS", &dir) };

        let line = LineString::from(vec![(0, 0), (4096, 0), (4096, 2048)]);
        let op = stroke_op(Geometry::LineString(line));
        // One good stroke vertex and one collapsed (near-zero) one, exactly
        // what `collapsed_normal_share` flags and `append_ops_with` would
        // have just logged the warning for.
        let op_vertices = [
            Vertex { position: [10.0, 20.0], normal: [1.0, 0.0], is_stroke: 1.0, ..Vertex::zeroed() },
            Vertex { position: [4096.0, 2048.0], normal: [0.0, 0.0], is_stroke: 1.0, ..Vertex::zeroed() },
        ];

        let tile = crate::geo::TileId { z: 10, x: 511, y: 300 };
        let _diag = crate::render::bad_normal_dump::with_bake_context(crate::render::bad_normal_dump::BakeContext {
            source: "test-routes".to_string(),
            tiled: true,
            tile: Some(tile),
            origin: None,
        });
        crate::render::bad_normal_dump::maybe_dump_bad_normals(&op, COORD_SCALE, 4096, &op_vertices);
        drop(_diag);

        let entries: Vec<_> =
            std::fs::read_dir(&dir).expect("dump dir created").filter_map(|e| e.ok()).collect();
        assert_eq!(entries.len(), 1, "exactly one bad-normal op fired exactly one dump: {entries:?}");
        let text = std::fs::read_to_string(entries[0].path()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["source"], "test-routes");
        assert_eq!(json["tiled"], true);
        assert_eq!(json["tile"]["z"], 10);
        assert_eq!(json["tile"]["x"], 511);
        assert_eq!(json["tile"]["y"], 300);
        assert_eq!(json["bad_normal_count"], 1, "{json}");
        let bad_normals = json["bad_normals"].as_array().unwrap();
        assert_eq!(bad_normals.len(), 1);
        assert_eq!(bad_normals[0]["index"], 1);
        assert_eq!(bad_normals[0]["normal"], serde_json::json!([0.0, 0.0]));
        assert_eq!(bad_normals[0]["position"], serde_json::json!([4096.0, 2048.0]));
        assert_eq!(json["op"]["coord_scale"], COORD_SCALE);
        assert_eq!(json["op"]["extent"], 4096);
        assert_eq!(json["op"]["cap"], "Butt");
        assert_eq!(json["op"]["join"], "Bevel");
        assert!(json["op"]["half_width_from_px"].is_number());
        assert!(json["op"]["tessellation_half_width"].is_number());
        let input_points = json["input_points"].as_array().unwrap();
        assert_eq!(input_points.len(), 3, "the line's own three points, unchanged");
        assert_eq!(input_points[0]["raw"], serde_json::json!([0, 0]));
        assert_eq!(input_points[1]["raw"], serde_json::json!([4096, 0]));
        // Tile 10/511/300's own point at MVT-local (4096, 0) is its NE
        // corner -- recovered lon/lat should land there, cheaply, from the
        // tile id alone.
        assert!(input_points[1]["lon"].is_number(), "{json}");
        assert!(input_points[1]["lat"].is_number(), "{json}");

        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::remove_var("GPUI_MAP_DUMP_BAD_NORMALS") };
    }

    #[test]
    fn fill_op_produces_a_punched_triangle_mesh() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Fill { color_from: red(), color_to: red(), geometry: square_polygon_with_hole() }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(!mesh.indices.is_empty());
        assert_eq!(mesh.indices.len() % 3, 0, "fill indices must form triangles");
        for v in &mesh.vertices {
            assert_eq!(v.is_stroke, 0.0);
            assert_eq!(v.color, [1.0, 0.0, 0.0, 1.0]);
        }
    }

    #[test]
    fn line_op_vertices_carry_half_the_declared_width() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: red(),
                color_to: red(),
                width_from: 4.0,
                width_to: 4.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: diagonal_line(),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);

        let stroke_vertices: Vec<_> = mesh.vertices.iter().filter(|v| v.is_stroke > 0.5).collect();
        assert!(!stroke_vertices.is_empty(), "expected at least one stroke vertex");
        for v in stroke_vertices {
            assert_eq!(v.half_width_px, 2.0, "half_width_px should be width / 2.0");
        }
    }

    /// POINT geometry on a line op is a pixel-sized DOT, and it must produce
    /// real triangles.
    ///
    /// The mechanism is the ROUND cap: a butt-capped zero-length subpath
    /// tessellates to nothing at all, which is how the overlay's point
    /// features (`circle-radius`, `circle-color`) drew zero vertices. Every
    /// vertex sits at the point itself and carries a UNIT normal, which is
    /// what `shader.wgsl` needs to extrude a disc of exactly `half_width_px`
    /// SCREEN pixels -- a radius that does not grow or shrink with zoom, the
    /// way a filled ring's world-space radius would.
    #[test]
    fn point_geometry_on_a_line_op_produces_a_pixel_sized_dot() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: red(),
                color_to: red(),
                width_from: 14.0,
                width_to: 14.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: Arc::new(Geometry::MultiPoint(geo_types::MultiPoint(vec![
                    geo_types::Point::new(30, 40),
                ]))),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);

        assert!(!mesh.vertices.is_empty(), "a dot must produce vertices");
        assert!(!mesh.indices.is_empty(), "a dot must produce triangles");
        assert_eq!(mesh.indices.len() % 3, 0, "dot indices must form whole triangles");
        let centre = [30.0 * COORD_SCALE, 40.0 * COORD_SCALE];
        for v in &mesh.vertices {
            assert_eq!(v.is_stroke, 1.0, "a dot is drawn by the stroke path");
            assert_eq!(v.half_width_px, 7.0, "half_width_px is the circle's pixel radius");
            assert_eq!(v.position, centre, "every dot vertex sits at the point itself");
            let length = (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt();
            assert!(
                (length - 1.0).abs() < 1e-3,
                "a cap vertex's normal must be a unit radial direction, was {length}"
            );
        }
    }

    #[test]
    fn fill_op_vertices_carry_zero_half_width() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Fill { color_from: red(), color_to: red(), geometry: square_polygon_with_hole() }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        for v in &mesh.vertices {
            assert_eq!(v.half_width_px, 0.0);
        }
    }

    #[test]
    fn line_op_produces_a_stroked_triangle_mesh_with_normals() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: red(),
                color_to: red(),
                width_from: 4.0,
                width_to: 4.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: diagonal_line(),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(!mesh.indices.is_empty());
        assert_eq!(mesh.indices.len() % 3, 0, "stroke indices must form triangles");
        assert!(
            mesh.vertices.iter().any(|v| v.normal != [0.0, 0.0]),
            "stroke vertices must carry a nonzero extrusion normal"
        );
        for v in &mesh.vertices {
            assert_eq!(v.is_stroke, 1.0);
        }
    }

    #[test]
    fn background_op_produces_no_geometry() {
        let plan = PaintPlan { ops: vec![PaintOp::Background { color: red() }] };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(mesh.vertices.is_empty());
        assert!(mesh.indices.is_empty());
    }

    #[test]
    fn degenerate_geometry_is_skipped_not_panicking() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Fill {
                color_from: red(),
                color_to: red(),
                geometry: Arc::new(Geometry::Polygon(Polygon::new(
                    LineString::from(vec![] as Vec<(i32, i32)>),
                    vec![],
                ))),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(mesh.vertices.is_empty());
        assert!(mesh.indices.is_empty());
    }

    #[test]
    fn coordinates_are_prescaled_by_1000() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Fill {
                color_from: red(),
                color_to: red(),
                geometry: Arc::new(Geometry::Polygon(Polygon::new(
                    LineString::from(vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]),
                    vec![],
                ))),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let max_x = mesh.vertices.iter().map(|v| v.position[0]).fold(0.0f32, f32::max);
        assert!((max_x - 10_000.0).abs() < 0.01, "expected 10 * 1000, got {max_x}");
    }

    #[test]
    fn fill_vertices_carry_a_distinct_color_to_when_the_pair_differs() {
        let from = red();
        let to = Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
        let plan = PaintPlan {
            ops: vec![PaintOp::Fill { color_from: from, color_to: to, geometry: square_polygon_with_hole() }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(!mesh.vertices.is_empty());
        for v in &mesh.vertices {
            assert_eq!(v.color, [1.0, 0.0, 0.0, 1.0], "color should carry color_from");
            assert_eq!(v.color_to, [0.0, 0.0, 1.0, 1.0], "color_to should carry color_to, not be left at zero/default");
        }
    }

    fn blue() -> Rgba {
        Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }
    }

    fn green() -> Rgba {
        Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }
    }

    #[test]
    fn rewriting_paint_attributes_matches_a_full_retessellation_at_the_new_zoom() {
        // The equivalence that makes the triangulation cache safe: rewriting
        // paint fields in place must produce byte-identical vertices to
        // tessellating the new plan from scratch. Three ops (Background,
        // Fill, Line), same geometry as the "new zoom" plan, only the paint
        // values differ -- exercising color, color_to, half_width_px and
        // half_width_px_to all changing at once.
        let plan_a = PaintPlan {
            ops: vec![
                PaintOp::Background { color: red() },
                PaintOp::Fill { color_from: red(), color_to: red(), geometry: square_polygon_with_hole() },
                PaintOp::Line {
                    metrics: false,
                    color_from: red(),
                    color_to: red(),
                    width_from: 4.0,
                    width_to: 4.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: DashPattern::none(),
                    geometry: diagonal_line(),
                },
            ],
        };
        let plan_b = PaintPlan {
            ops: vec![
                PaintOp::Background { color: blue() },
                PaintOp::Fill { color_from: blue(), color_to: green(), geometry: square_polygon_with_hole() },
                PaintOp::Line {
                    metrics: false,
                    color_from: green(),
                    color_to: blue(),
                    width_from: 8.0,
                    width_to: 12.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: DashPattern::none(),
                    geometry: diagonal_line(),
                },
            ],
        };

        let (mut mesh, ranges) = tessellate_plan_with_ranges(&plan_a, 4096, COORD_SCALE, Subdivision::NONE);
        let expected = tessellate_plan(&plan_b, 4096, COORD_SCALE, Subdivision::NONE);
        rewrite_paint_attributes(&mut mesh, &ranges, &plan_b);
        assert_eq!(mesh.vertices, expected.vertices);
        assert_eq!(mesh.indices, expected.indices, "indices must not change");
    }

    #[test]
    fn rewrite_paint_attributes_handles_a_leading_background_op_that_emits_no_vertices() {
        // `PaintOp::Background` emits no vertices. If range bookkeeping
        // assumes every op contributes at least one vertex, the Fill op's
        // range (the second op) shifts and this rewrite corrupts the mesh --
        // writing the Fill's colors over nothing, or over the wrong slice.
        let plan_a = PaintPlan {
            ops: vec![
                PaintOp::Background { color: red() },
                PaintOp::Fill { color_from: red(), color_to: red(), geometry: square_polygon_with_hole() },
            ],
        };
        let plan_b = PaintPlan {
            ops: vec![
                PaintOp::Background { color: green() },
                PaintOp::Fill { color_from: blue(), color_to: green(), geometry: square_polygon_with_hole() },
            ],
        };

        let (mut mesh, ranges) = tessellate_plan_with_ranges(&plan_a, 4096, COORD_SCALE, Subdivision::NONE);
        let expected = tessellate_plan(&plan_b, 4096, COORD_SCALE, Subdivision::NONE);
        rewrite_paint_attributes(&mut mesh, &ranges, &plan_b);
        assert_eq!(mesh.vertices, expected.vertices);
        assert_eq!(mesh.indices, expected.indices);
    }

    fn plan_with_one_straight_line_of_length(length: i32) -> PaintPlan {
        PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: red(),
                color_to: red(),
                width_from: 4.0,
                width_to: 4.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: Arc::new(Geometry::LineString(LineString::from(vec![(0, 0), (length, 0)]))),
            }],
        }
    }

    fn plan_with_one_dashed_line(dash: DashPattern) -> PaintPlan {
        PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: red(),
                color_to: red(),
                width_from: 4.0,
                width_to: 4.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash,
                geometry: diagonal_line(),
            }],
        }
    }

    fn plan_with_one_fill() -> PaintPlan {
        PaintPlan {
            ops: vec![PaintOp::Fill { color_from: red(), color_to: red(), geometry: square_polygon_with_hole() }],
        }
    }

    // lyon's advancement is distance along the path in INPUT units, which for us
    // is MVT coords multiplied by COORD_SCALE. A straight 0->100 MVT-unit line
    // must therefore end at 100 * COORD_SCALE.
    #[test]
    fn stroke_vertices_carry_distance_along_the_line() {
        let plan = plan_with_one_straight_line_of_length(100);
        let (mesh, ranges) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let verts = &mesh.vertices[ranges[0].vertices.clone()];

        let max = verts.iter().fold(0.0_f32, |a, v| a.max(v.line_distance));
        assert!(
            (max - 100.0 * COORD_SCALE).abs() < 1.0,
            "expected ~{}, got {max}",
            100.0 * COORD_SCALE
        );
        assert!(verts.iter().any(|v| v.line_distance == 0.0), "start must be at distance 0");
    }

    #[test]
    fn stroke_vertices_carry_the_dash_pattern() {
        let plan = plan_with_one_dashed_line(DashPattern::from_runs(&[2.0, 2.0, 6.0, 2.0]));
        let (mesh, ranges) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        for v in &mesh.vertices[ranges[0].vertices.clone()] {
            assert_eq!(v.dash_runs, [2.0, 2.0, 6.0, 2.0]);
        }
    }

    #[test]
    fn fill_vertices_are_never_dashed() {
        let plan = plan_with_one_fill();
        let (mesh, ranges) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        for v in &mesh.vertices[ranges[0].vertices.clone()] {
            assert_eq!(v.dash_runs, [0.0; 4]);
            assert_eq!(v.line_distance, 0.0);
        }
    }

    // The cache invariant. line_distance is geometry-derived and must survive a
    // paint rewrite untouched; dash_runs is paint and must be updated.
    #[test]
    fn rewriting_paint_attributes_updates_dash_but_never_distance() {
        let solid = plan_with_one_dashed_line(DashPattern::none());
        let (mut mesh, ranges) = tessellate_plan_with_ranges(&solid, 4096, COORD_SCALE, Subdivision::NONE);
        let distances_before: Vec<f32> = mesh.vertices.iter().map(|v| v.line_distance).collect();

        let dashed = plan_with_one_dashed_line(DashPattern::from_runs(&[3.0, 3.0]));
        rewrite_paint_attributes(&mut mesh, &ranges, &dashed);

        let distances_after: Vec<f32> = mesh.vertices.iter().map(|v| v.line_distance).collect();
        assert_eq!(distances_before, distances_after, "line_distance is zoom-independent");
        for v in &mesh.vertices[ranges[0].vertices.clone()] {
            assert_eq!(&v.dash_runs[..2], &[3.0, 3.0], "dash_runs is zoom-dependent");
        }
    }

    /// `line_progress`, like `line_distance`, is geometry: a paint rewrite
    /// (zoom crossing, recolour, highlight) must leave it bit-for-bit.
    #[test]
    fn rewriting_paint_attributes_never_touches_line_progress() {
        let line = Geometry::LineString(LineString::from(vec![(0, 0), (1000, 0), (4000, 0)]));
        let mut mesh = tessellate_one_line(line.clone(), true);
        let before: Vec<f32> = mesh.vertices.iter().map(|v| v.line_progress).collect();
        assert!(before.iter().any(|p| *p > 0.5), "the fixture must carry real progress");
        let ranges = vec![OpSpan { vertices: 0..mesh.vertices.len(), indices: 0..mesh.indices.len() }];
        let mut recoloured = one_line_op(line, true);
        if let PaintOp::Line { color_from, color_to, width_from, .. } = &mut recoloured {
            *color_from = Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
            *color_to = *color_from;
            *width_from = 9.0;
        }
        let plan = PaintPlan { ops: vec![recoloured] };
        rewrite_paint_attributes(&mut mesh, &ranges, &plan);
        rewrite_paint_attributes_for_ops(&mut mesh, &ranges, &plan, &[0]);
        let after: Vec<f32> = mesh.vertices.iter().map(|v| v.line_progress).collect();
        assert_eq!(before, after, "line_progress is geometry-derived");
        assert!(mesh.vertices.iter().all(|v| v.color[2] == 1.0), "the rewrite did run");
    }

    #[test]
    fn a_gradient_line_is_densified_to_64_parts_and_coloured_by_progress() {
        // One 2-point line with a blue -> red gradient.
        let dense = densify_for_gradient(&[(0, 0), (4000, 0)], 64);
        assert_eq!(dense.len(), 65, "64 parts");
        assert_eq!((dense[0], dense[64]), ((0, 0), (4000, 0)), "the ends are kept exactly");
        let coords: Vec<(i32, i32)> = dense.clone();
        let mut mesh = tessellate_one_line(Geometry::LineString(LineString::from(coords)), true);
        let blue_to_red = |p: f32| Rgba { r: p, g: 0.0, b: 1.0 - p, a: 1.0 };
        colour_by_line_progress(&mut mesh.vertices, blue_to_red, 1.0);
        let mut xs: Vec<u32> = mesh.vertices.iter().map(|v| v.position[0].to_bits()).collect();
        xs.sort_unstable();
        xs.dedup();
        assert!(xs.len() >= 65, "at least 65 distinct x positions, got {}", xs.len());
        let nearest = mesh
            .vertices
            .iter()
            .min_by(|a, b| (a.line_progress - 0.25).abs().total_cmp(&(b.line_progress - 0.25).abs()))
            .expect("vertices");
        assert!((nearest.color[0] - 0.25).abs() < 0.03, "r at progress ~0.25: {:?}", nearest.color);
        assert_eq!(nearest.color, nearest.color_to);
        // Every vertex: r equals its progress (the gradient is linear in it).
        for v in &mesh.vertices {
            assert!((v.color[0] - v.line_progress).abs() < 1e-6);
        }
        // Opacity multiplies alpha only; a vertex without progress is untouched.
        let mut fill = tessellate_one_line(Geometry::LineString(LineString::from(vec![(0, 0), (10, 0)])), false);
        let before: Vec<[f32; 4]> = fill.vertices.iter().map(|v| v.color).collect();
        colour_by_line_progress(&mut fill.vertices, blue_to_red, 0.5);
        assert_eq!(before, fill.vertices.iter().map(|v| v.color).collect::<Vec<_>>());
        let mut half = tessellate_one_line(Geometry::LineString(LineString::from(dense)), true);
        colour_by_line_progress(&mut half.vertices, blue_to_red, 0.5);
        assert!(half.vertices.iter().all(|v| v.color[3] == 0.5));
    }

    #[test]
    fn densify_keeps_every_input_point_and_caps_every_piece() {
        let line = [(0, 0), (1000, 0), (1000, 3), (0, 3)];
        let dense = densify_for_gradient(&line, 64);
        for p in line {
            assert!(dense.contains(&p), "{p:?} kept");
        }
        let total = 2003.0f64;
        for w in dense.windows(2) {
            let (dx, dy) = (f64::from(w[1].0 - w[0].0), f64::from(w[1].1 - w[0].1));
            assert!((dx * dx + dy * dy).sqrt() <= total / 64.0 + 1.0, "{w:?}");
            assert_ne!(w[0], w[1], "no zero-length piece");
        }
        assert_eq!(densify_for_gradient(&[(5, 5), (5, 5)], 64), vec![(5, 5), (5, 5)], "zero length: unchanged");
    }

    /// A solid 4 px line op over `geometry`, with or without line metrics.
    fn one_line_op(geometry: Geometry<i32>, metrics: bool) -> PaintOp {
        PaintOp::Line {
            color_from: red(),
            color_to: red(),
            width_from: 4.0,
            width_to: 4.0,
            gap_from: 0.0,
            gap_to: 0.0,
            cap: crate::style::LineCap::Butt,
            join: crate::style::LineJoin::Miter,
            dash: DashPattern::none(),
            metrics,
            geometry: Arc::new(geometry),
        }
    }

    /// `append_stroke` (through `tessellate_plan`) over one line op.
    fn tessellate_one_line(geometry: Geometry<i32>, metrics: bool) -> TileMesh {
        tessellate_plan(&PaintPlan { ops: vec![one_line_op(geometry, metrics)] }, 4096, COORD_SCALE, Subdivision::NONE)
    }

    #[test]
    fn a_line_with_metrics_carries_progress_from_0_to_1_and_one_without_carries_minus_1() {
        let line = Geometry::LineString(LineString::from(vec![(0, 0), (1000, 0), (2000, 0), (4000, 0)]));
        let with = tessellate_one_line(line.clone(), true);
        let without = tessellate_one_line(line, false);
        let p: Vec<f32> = with.vertices.iter().map(|v| v.line_progress).collect();
        assert!(!p.is_empty());
        assert!(p.iter().all(|x| (0.0..=1.0).contains(x)), "{p:?}");
        assert!(p.iter().any(|x| *x < 0.01) && p.iter().any(|x| *x > 0.99), "{p:?}");
        // The vertex at x = 2000 is half-way by length.
        let at_half = with
            .vertices
            .iter()
            .filter(|v| (v.position[0] - 2000.0 * COORD_SCALE).abs() < 1.0)
            .map(|v| v.line_progress)
            .collect::<Vec<_>>();
        assert!(!at_half.is_empty(), "lyon emits the join at x = 2000");
        assert!(at_half.iter().all(|x| (x - 0.5).abs() < 1e-3), "{at_half:?}");
        assert!(without.vertices.iter().all(|v| v.line_progress == -1.0));
    }

    #[test]
    fn a_fill_carries_no_line_progress() {
        let plan = plan_with_one_fill();
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(!mesh.vertices.is_empty());
        assert!(mesh.vertices.iter().all(|v| v.line_progress == -1.0));
    }

    /// MapLibre's progress is per LineString (`line_bucket.ts` resets
    /// `lineSoFar` and recomputes `totalDistance` per line), so each part of
    /// a MultiLineString runs 0..1 on its own -- even when the parts'
    /// lengths differ fourfold, which normalising over the whole feature
    /// would turn into a short part stopping at 0.2.
    #[test]
    fn progress_is_per_linestring_in_a_multilinestring() {
        let short = LineString::from(vec![(0, 0), (1000, 0)]);
        let long = LineString::from(vec![(0, 500), (2000, 500), (4000, 500)]);
        let short_alone = tessellate_one_line(Geometry::LineString(short.clone()), true);
        let both = tessellate_one_line(Geometry::MultiLineString(geo_types::MultiLineString(vec![short, long])), true);
        let n = short_alone.vertices.len();
        assert!(n > 0 && both.vertices.len() > n);
        for (name, part) in [("short", &both.vertices[..n]), ("long", &both.vertices[n..])] {
            let p: Vec<f32> = part.iter().map(|v| v.line_progress).collect();
            assert!(p.iter().all(|x| (0.0..=1.0).contains(x)), "{name}: {p:?}");
            assert!(p.iter().any(|x| *x < 0.01), "{name} starts at 0: {p:?}");
            assert!(p.iter().any(|x| *x > 0.99), "{name} reaches 1: {p:?}");
        }
        // The short part alone and inside the MultiLineString agree exactly.
        let alone: Vec<f32> = short_alone.vertices.iter().map(|v| v.line_progress).collect();
        let inside: Vec<f32> = both.vertices[..n].iter().map(|v| v.line_progress).collect();
        assert_eq!(alone, inside);
    }

    /// Every vertex lyon emits AT an input point (its joins, caps and cut
    /// ends all carry that point's `position_on_path`) must have progress
    /// equal to the point's cumulative length over the total -- whichever
    /// run or apex window emitted it. That is continuity at every cut:
    /// both sides of a seam sit on the same input point.
    fn assert_progress_at_inputs_is_cumulative_length(mesh: &TileMesh, points: &[(f64, f64)], near: f32) {
        let mut cumulative = vec![0.0f64];
        for pair in points.windows(2) {
            let (dx, dy) = (pair[1].0 - pair[0].0, pair[1].1 - pair[0].1);
            cumulative.push(cumulative.last().copied().unwrap_or(0.0) + (dx * dx + dy * dy).sqrt());
        }
        let total = *cumulative.last().expect("points");
        for (i, &(x, y)) in points.iter().enumerate() {
            let expected = (cumulative[i] / total) as f32;
            let at: Vec<f32> = mesh
                .vertices
                .iter()
                .filter(|v| (v.position[0] as f64 - x).abs() <= near as f64 && (v.position[1] as f64 - y).abs() <= near as f64)
                .map(|v| v.line_progress)
                .collect();
            assert!(!at.is_empty(), "no vertex at input point {i} ({x}, {y})");
            for p in &at {
                assert!((p - expected).abs() < 1e-4, "point {i}: progress {p}, cumulative fraction {expected} ({at:?})");
            }
        }
    }

    /// A back-track is cut into windows at its apexes (`backtrack_apexes`),
    /// each tessellated separately; `distance_so_far` carries across them,
    /// so progress at every input point -- the apexes included, where two
    /// windows meet -- is its cumulative length over the whole line's, and
    /// reaches 1 only at the true end.
    #[test]
    fn a_line_split_into_runs_at_backtrack_apexes_keeps_monotonic_progress() {
        // Two near-reversals, 0.29 and 0.57 degrees off 180, both inside
        // the split band; the points do not coincide, so each is findable.
        let coords = vec![(0, 0), (1000, 0), (0, 5), (1000, 10)];
        let points: Vec<Point> =
            coords.iter().map(|&(x, y)| lyon::math::point(x as f32 * COORD_SCALE, y as f32 * COORD_SCALE)).collect();
        assert_eq!(backtrack_apexes(&points), Some(vec![1, 2]), "the fixture must be split");
        let mesh = tessellate_one_line(Geometry::LineString(LineString::from(coords.clone())), true);
        let scaled: Vec<(f64, f64)> =
            coords.iter().map(|&(x, y)| (x as f64 * COORD_SCALE as f64, y as f64 * COORD_SCALE as f64)).collect();
        assert_progress_at_inputs_is_cumulative_length(&mesh, &scaled, 1.0);
        let max = mesh.vertices.iter().map(|v| v.line_progress).fold(f32::MIN, f32::max);
        assert!((max - 1.0).abs() < 1e-6, "ends at 1, got {max}");
    }

    /// A line longer than `run_split::MAX_RUN_EXTENT` is tessellated in
    /// several runs, rebased separately; progress is continuous at every
    /// run seam and equals the cumulative-length fraction at each point.
    #[test]
    fn progress_is_continuous_across_run_split_seams() {
        let coords: Vec<(i32, i32)> = (0..=20).map(|i| (i * 1000, if i % 2 == 0 { 0 } else { 300 })).collect();
        let points: Vec<Point> =
            coords.iter().map(|&(x, y)| lyon::math::point(x as f32 * COORD_SCALE, y as f32 * COORD_SCALE)).collect();
        let runs = run_split::split_into_runs(&points, run_split::MAX_RUN_EXTENT);
        assert!(runs.len() >= 2, "the fixture must be split into runs: {runs:?}");
        let mesh = tessellate_one_line(Geometry::LineString(LineString::from(coords.clone())), true);
        let scaled: Vec<(f64, f64)> =
            coords.iter().map(|&(x, y)| (x as f64 * COORD_SCALE as f64, y as f64 * COORD_SCALE as f64)).collect();
        // f32 spacing at 2e7 is 2: match within a few units.
        assert_progress_at_inputs_is_cumulative_length(&mesh, &scaled, 4.0);
    }

    #[test]
    fn stroke_vertices_carry_a_distinct_half_width_px_to_when_the_pair_differs() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: red(),
                color_to: red(),
                width_from: 4.0,
                width_to: 12.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: diagonal_line(),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let stroke_vertices: Vec<_> = mesh.vertices.iter().filter(|v| v.is_stroke > 0.5).collect();
        assert!(!stroke_vertices.is_empty());
        for v in stroke_vertices {
            assert_eq!(v.half_width_px, 2.0, "half_width_px should be width_from / 2.0");
            assert_eq!(v.half_width_px_to, 6.0, "half_width_px_to should be width_to / 2.0");
        }
    }

    /// The scale is the caller's to choose, and it lands on the geometry.
    /// A tile passes COORD_SCALE; the overlay passes 1.0, because its own
    /// grid is already 7.8 mm per unit and multiplying by 1000 only costs
    /// coordinate range.
    #[test]
    fn the_coordinate_scale_is_a_parameter_not_a_constant() {
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: std::sync::Arc::new(geo_types::Geometry::MultiLineString(
                    geo_types::MultiLineString(vec![geo_types::LineString(vec![
                        geo_types::Coord { x: 0, y: 0 },
                        geo_types::Coord { x: 1000, y: 0 },
                    ])]),
                )),
            }],
        };

        let scaled = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let unscaled = tessellate_plan(&plan, 4096, OVERLAY_COORD_SCALE, Subdivision::NONE);

        let span = |mesh: &TileMesh| {
            let xs: Vec<f32> = mesh.vertices.iter().map(|v| v.position[0]).collect();
            xs.iter().cloned().fold(f32::MIN, f32::max) - xs.iter().cloned().fold(f32::MAX, f32::min)
        };
        // Same topology, coordinates a thousand times apart.
        assert_eq!(scaled.vertices.len(), unscaled.vertices.len());
        assert!((span(&scaled) / span(&unscaled) - 1000.0).abs() < 1.0, "{} vs {}", span(&scaled), span(&unscaled));
    }

    /// THE regression. A polyline spanning what a 600 km feature set spans
    /// must still produce unit-length normals. Before this work 94% of them
    /// were exactly (0, 0), and the line drew nothing at all.
    #[test]
    fn a_polyline_far_from_the_origin_still_has_unit_normals() {
        // 3.1e7 units: this dataset's far edge, in the overlay's own grid.
        let far = 31_000_000;
        let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
            geo_types::LineString(
                (0..200)
                    .map(|i| geo_types::Coord { x: far + i * 1000, y: far })
                    .collect(),
            ),
        ]));
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: std::sync::Arc::new(geometry),
            }],
        };
        let mesh = tessellate_plan(&plan, 1, OVERLAY_COORD_SCALE, Subdivision::NONE);

        let mut collapsed = 0;
        let mut strokes = 0;
        for v in &mesh.vertices {
            if v.is_stroke < 0.5 {
                continue;
            }
            strokes += 1;
            let len = (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt();
            if len < 0.9 || len > 1.1 {
                collapsed += 1;
            }
        }
        assert!(strokes > 0, "the line must produce stroke vertices at all");
        assert_eq!(collapsed, 0, "{collapsed} of {strokes} normals are not unit length");
    }

    /// A polyline that reverses on itself exactly -- e.g. a Mapbox vector
    /// tile's `856,1360 -> 858,1352 -> 856,1360` -- must not hand lyon's
    /// miter join a `(0, 0)` direction sum. Measured before the fix: this
    /// shape tripped the "not unit vectors" warning in
    /// `tessellate_plan_with_ranges_cancellable`.
    #[test]
    fn a_backtracking_polyline_has_only_unit_normals() {
        let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
            geo_types::LineString(vec![
                geo_types::Coord { x: 856, y: 1360 },
                geo_types::Coord { x: 858, y: 1352 },
                geo_types::Coord { x: 856, y: 1360 },
            ]),
        ]));
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: std::sync::Arc::new(geometry),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);

        // See `collapsed_normal_share`: only near-zero is the defect.
        let mut collapsed = 0;
        let mut strokes = 0;
        for v in &mesh.vertices {
            if v.is_stroke < 0.5 {
                continue;
            }
            strokes += 1;
            let len = (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt();
            if len < 0.9 {
                collapsed += 1;
            }
        }
        assert!(strokes > 0, "the back-track must still produce stroke vertices");
        assert_eq!(collapsed, 0, "{collapsed} of {strokes} normals are not unit length");
    }

    /// `estimate_buffer_capacity` prices no apex caps -- basemap data has
    /// almost no back-tracks, and an under-reservation costs one amortised
    /// `Vec` growth, less than walking every point of every line on every
    /// tile to count them. So an apex-dense line must still tessellate
    /// correctly once it outgrows the reservation: here 199 exact
    /// back-tracks, each split into two round caps, well past the plain
    /// per-point estimate.
    #[test]
    fn an_apex_dense_line_outgrows_the_plain_estimate_and_still_tessellates() {
        let coords: Vec<geo_types::Coord<i32>> = (0..201)
            .map(|i| if i % 2 == 0 { geo_types::Coord { x: 100, y: 100 } } else { geo_types::Coord { x: 900, y: 700 } })
            .collect();
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Round,
                join: crate::style::LineJoin::Round,
                dash: DashPattern::none(),
                geometry: std::sync::Arc::new(geo_types::Geometry::LineString(geo_types::LineString(coords))),
            }],
        };
        let (est_v, est_i) = estimate_buffer_capacity(&plan.ops, COORD_SCALE, 4096, Subdivision::NONE);
        let (mesh, spans) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(
            mesh.vertices.len() > est_v && mesh.indices.len() > est_i,
            "the fixture must outgrow the reservation to test growth: estimated {est_v}/{est_i}, real {}/{}",
            mesh.vertices.len(),
            mesh.indices.len()
        );
        assert_eq!(spans.len(), 1);
        assert!(mesh.indices.iter().all(|&i| (i as usize) < mesh.vertices.len()), "an index points past the mesh");
        let collapsed = mesh
            .vertices
            .iter()
            .filter(|v| v.is_stroke >= 0.5)
            .filter(|v| (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt() < 0.9)
            .count();
        assert_eq!(collapsed, 0, "{collapsed} stroke normals are not unit length");
        // Every one of the 200 segments is drawn: same as tessellating the
        // 200 two-point pieces the splits make, each on its own.
        let piece = PaintPlan {
            ops: vec![match &plan.ops[0] {
                PaintOp::Line { color_from, color_to, width_from, width_to, gap_from, gap_to, cap, join, dash, .. } => PaintOp::Line {
                    metrics: false,
                    color_from: *color_from,
                    color_to: *color_to,
                    width_from: *width_from,
                    width_to: *width_to,
                    gap_from: *gap_from,
                    gap_to: *gap_to,
                    cap: *cap,
                    join: *join,
                    dash: *dash,
                    geometry: std::sync::Arc::new(geo_types::Geometry::LineString(geo_types::LineString(vec![
                        geo_types::Coord { x: 100, y: 100 },
                        geo_types::Coord { x: 900, y: 700 },
                    ]))),
                },
                _ => unreachable!(),
            }],
        };
        let one = tessellate_plan(&piece, 4096, COORD_SCALE, Subdivision::NONE);
        assert_eq!(mesh.vertices.len(), 200 * one.vertices.len());
        assert_eq!(mesh.indices.len(), 200 * one.indices.len());
    }

    /// Deterministic polylines for the gate tests below: every length from
    /// 2 up, on a coordinate range small enough that adjacent duplicates,
    /// exact back-tracks and shallow turns all occur, plus near-reversal
    /// sweeps straddling `LYON_NORMAL_DEGENERACY_EPSILON` at several
    /// segment lengths and a far-from-origin offset.
    fn gate_test_lines() -> Vec<Vec<(i32, i32)>> {
        let mut lines = Vec::new();
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = |m: i32| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) % m as u64) as i32
        };
        for len in 2..14 {
            for _ in 0..400 {
                let mut line = Vec::with_capacity(len);
                let (mut x, mut y) = (next(9) - 4, next(9) - 4);
                for _ in 0..len {
                    line.push((x, y));
                    // Mostly short steps, sometimes a repeat or a jump back.
                    match next(6) {
                        0 => {}
                        1 if line.len() >= 2 => {
                            let (px, py) = line[line.len() - 2];
                            (x, y) = (px, py);
                        }
                        _ => {
                            x += next(7) - 3;
                            y += next(7) - 3;
                        }
                    }
                }
                lines.push(line);
            }
        }
        for &(len_a, len_b) in &[(1000.0f64, 1000.0f64), (1000.0, 37.0), (5.0, 4000.0)] {
            for step in 0..=400 {
                // 180 degrees +- 2.5 degrees, straddling the ~1.15-degree band.
                let theta = (177.5 + 5.0 * step as f64 / 400.0).to_radians();
                for &(ox, oy) in &[(0, 0), (1 << 24, -(1 << 24))] {
                    let a = (ox, oy);
                    let b = (ox + len_a.round() as i32, oy);
                    let c = (
                        b.0 + (len_b * theta.cos()).round() as i32,
                        b.1 + (len_b * theta.sin()).round() as i32,
                    );
                    lines.push(vec![a, b, c]);
                    lines.push(vec![(ox - 50, oy - 50), a, b, b, c]);
                }
            }
        }
        lines
    }

    /// `scan_duplicates_and_apexes` gates `append_stroke`'s `dedup` and
    /// `backtrack_apexes` off for the common line, so it must never say
    /// "nothing to do" for a line either pass would change: its duplicate
    /// flag is exactly "`dedup` would drop a point", and, on a line with
    /// no duplicate, its apex flag is exactly "`backtrack_apexes` finds
    /// one" -- which together make the gated path's split set identical to
    /// running both passes unconditionally.
    #[test]
    fn the_dedup_and_apex_gate_matches_the_passes_it_gates() {
        let mut flagged = (0usize, 0usize);
        for coords in gate_test_lines() {
            for &scale in &[COORD_SCALE, 1.0, 0.125] {
                let points: Vec<Point> =
                    coords.iter().map(|&c| to_lyon_point(c, scale)).collect();
                let (dup, apex) = scan_duplicates_and_apexes(&points);
                let mut deduped = points.clone();
                deduped.dedup();
                assert_eq!(dup, deduped.len() != points.len(), "duplicate flag, {coords:?} at {scale}");
                if !dup {
                    assert_eq!(apex, backtrack_apexes(&points).is_some(), "apex flag, {coords:?} at {scale}");
                }
                // The gate as `append_stroke` applies it.
                let gated = if dup || apex { backtrack_apexes(&deduped) } else { None };
                assert_eq!(gated, backtrack_apexes(&deduped), "split set, {coords:?} at {scale}");
                flagged.0 += usize::from(dup);
                flagged.1 += usize::from(apex && !dup);
            }
        }
        // The fixture must actually reach both halves of the gate.
        assert!(flagged.0 > 100 && flagged.1 > 100, "{flagged:?}");
    }

    /// `is_normal_degenerate`'s sqrt-free, dot/squared-length form must
    /// agree with the f64 normalized-tangent-sum form of the SAME
    /// `LYON_NORMAL_DEGENERACY_EPSILON` EVERYWHERE, not just at the handful
    /// of shapes the other tests here exercise -- this pins the algebraic
    /// derivation, not a claim that either form matches lyon's own `f32`
    /// arithmetic bit-for-bit (see `LYON_NORMAL_DEGENERACY_EPSILON`'s doc
    /// comment for why it deliberately does not, and does not need to).
    /// Swept across the turn angle, densely, straddling the threshold
    /// (`4e-4` puts it about 1.146 degrees off a true 180-degree reversal),
    /// and at several unrelated segment-length pairs, since nothing about
    /// the derivation should depend on `|a|`/`|b|` individually (only the
    /// angle should decide it -- the degeneracy test is scale-invariant in
    /// each vector's own length).
    #[test]
    fn the_sqrt_free_degeneracy_test_matches_the_normalized_form() {
        // lyon's own test, restated directly: unit tangents summed and
        // squared, compared to the epsilon. Independent of `len_a`/`len_b`
        // by construction (a unit tangent has already normalized its own
        // vector's length away) -- passed through only so the caller can
        // assert both forms were evaluated over the same inputs.
        fn normalized_form(theta_deg: f64) -> bool {
            let theta = theta_deg.to_radians();
            let t1 = (1.0f64, 0.0f64);
            let t2 = (theta.cos(), theta.sin());
            let sum = (t1.0 + t2.0, t1.1 + t2.1);
            sum.0 * sum.0 + sum.1 * sum.1 < LYON_NORMAL_DEGENERACY_EPSILON
        }
        // The form this module actually runs: un-normalized vectors at the
        // given lengths and turn angle, dot product and squared lengths fed
        // to `is_normal_degenerate`.
        fn sqrt_free_form(len_a: f64, len_b: f64, theta_deg: f64) -> bool {
            let theta = theta_deg.to_radians();
            let (ax, ay) = (len_a, 0.0);
            let (bx, by) = (len_b * theta.cos(), len_b * theta.sin());
            let dot = ax * bx + ay * by;
            let len_a_sq = ax * ax + ay * ay;
            let len_b_sq = bx * bx + by * by;
            is_normal_degenerate(dot, len_a_sq, len_b_sq)
        }

        let length_pairs = [(1.0, 1.0), (2.0, 0.5), (0.3, 7.0), (1000.0, 1000.0), (1.0, 1_000_000.0)];
        let mut checked = 0usize;
        let mut mismatches = Vec::new();
        // 178.0..=182.0 by 0.0005 degrees: dense enough to land on both
        // sides of the ~178.854/181.146-degree threshold many hundreds of
        // times over, for every length pair.
        let mut theta_deg = 178.0f64;
        while theta_deg <= 182.0 {
            let expected = normalized_form(theta_deg);
            for &(len_a, len_b) in &length_pairs {
                let actual = sqrt_free_form(len_a, len_b, theta_deg);
                checked += 1;
                if actual != expected {
                    mismatches.push((theta_deg, len_a, len_b, expected, actual));
                }
            }
            theta_deg += 0.0005;
        }
        assert!(checked > 10_000, "the sweep must actually cover the threshold densely: {checked}");
        assert!(
            mismatches.is_empty(),
            "{} of {checked} disagreed, e.g. {:?}",
            mismatches.len(),
            &mismatches[..mismatches.len().min(5)]
        );

        // A handful of angles far from the threshold in either direction,
        // as a sanity check on the cheap `dot < 0` rejection itself: every
        // one of these must be unanimously "not degenerate" under both
        // forms, at every length pair.
        for theta_deg in [0.0, 45.0, 89.0, 90.0, 91.0, 135.0, 170.0] {
            assert!(!normalized_form(theta_deg), "theta={theta_deg} must not be degenerate");
            for &(len_a, len_b) in &length_pairs {
                assert!(
                    !sqrt_free_form(len_a, len_b, theta_deg),
                    "theta={theta_deg} len_a={len_a} len_b={len_b} must not be degenerate"
                );
            }
        }
    }

    /// A collinear back-track of a DIFFERENT length than the run-out --
    /// e.g. a Mapbox vector tile's `(0,0) -> (10,0) -> (4,0)` -- degenerates
    /// exactly the way an exact `A -> B -> A` reversal does: the incoming
    /// and outgoing tangents are exact opposites, so their sum is `(0, 0)`
    /// regardless of the two segments' lengths. The point-EQUALITY check
    /// this function used before `backtrack_apexes` became angle-based
    /// caught only the special case where the line returns to precisely
    /// the same coordinate; this shape never satisfies that equality
    /// (`(0,0) != (4,0)`) and so was missed.
    #[test]
    fn a_collinear_backtrack_of_a_different_length_has_only_unit_normals() {
        let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
            geo_types::LineString(vec![
                geo_types::Coord { x: 0, y: 0 },
                geo_types::Coord { x: 10, y: 0 },
                geo_types::Coord { x: 4, y: 0 },
            ]),
        ]));
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: std::sync::Arc::new(geometry),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);

        // See `collapsed_normal_share`: only near-zero is the defect.
        let mut collapsed = 0;
        let mut strokes = 0;
        for v in &mesh.vertices {
            if v.is_stroke < 0.5 {
                continue;
            }
            strokes += 1;
            let len = (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt();
            if len < 0.9 {
                collapsed += 1;
            }
        }
        assert!(strokes > 0, "the collinear back-track must still produce stroke vertices");
        assert_eq!(collapsed, 0, "{collapsed} of {strokes} normals are not unit length");
    }

    /// A NON-collinear near-reversal: `(0,0) -> (1000,0) -> (0,5)` turns
    /// about 0.29 degrees short of a full 180-degree reversal (not exactly
    /// on it, unlike every other test here), safely inside
    /// `LYON_NORMAL_DEGENERACY_EPSILON`'s `4e-4` band (about 1.146 degrees
    /// either side of 180) with margin -- deliberately not
    /// `(0,0)->(100,0)->(0,1)`, whose ~0.573-degree deviation sits close
    /// enough to the OLD `1e-4` threshold to be a boundary case rather
    /// than a clear one.
    #[test]
    fn a_near_reversal_off_the_collinear_axis_has_only_unit_normals() {
        let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
            geo_types::LineString(vec![
                geo_types::Coord { x: 0, y: 0 },
                geo_types::Coord { x: 1000, y: 0 },
                geo_types::Coord { x: 0, y: 5 },
            ]),
        ]));
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: std::sync::Arc::new(geometry),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);

        let mut collapsed = 0;
        let mut strokes = 0;
        for v in &mesh.vertices {
            if v.is_stroke < 0.5 {
                continue;
            }
            strokes += 1;
            let len = (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt();
            if len < 0.9 {
                collapsed += 1;
            }
        }
        assert!(strokes > 0, "the near-reversal must still produce stroke vertices");
        assert_eq!(collapsed, 0, "{collapsed} of {strokes} normals are not unit length");
    }

    /// A repeated point sitting exactly AT a back-track's apex
    /// (`A -> B -> B -> A`) must not defeat `backtrack_apexes`. Without
    /// `dedup` this function would see a zero-length `B -> B` segment
    /// sitting between the two real ones, which HIDES the reversal from a
    /// check that only looks at immediate neighbours: `dot(A->B, B->B)`
    /// and `dot(B->B, B->A)` are both `0`, neither triple is flagged, and
    /// the real reversal one step further out is never examined as a
    /// triple at all -- not a lyon defect (lyon's own `points_are_too_
    /// close` already merges an exact duplicate before any join is
    /// computed, at roughly 0.54 lyon units under
    /// `STROKE_TESSELLATION_WIDTH`, and never reaches a state this bad).
    /// `dedup` turns the shape into a plain, well-defined `A -> B -> A`
    /// before this function's own angle test ever runs.
    #[test]
    fn a_repeated_point_at_a_backtracks_apex_has_only_unit_normals() {
        let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
            geo_types::LineString(vec![
                geo_types::Coord { x: 0, y: 0 },
                geo_types::Coord { x: 100, y: 0 },
                geo_types::Coord { x: 100, y: 0 },
                geo_types::Coord { x: 0, y: 0 },
            ]),
        ]));
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern::none(),
                geometry: std::sync::Arc::new(geometry),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);

        let mut collapsed = 0;
        let mut strokes = 0;
        for v in &mesh.vertices {
            if v.is_stroke < 0.5 {
                continue;
            }
            strokes += 1;
            let len = (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt();
            assert!(len.is_finite(), "a normal must never be NaN or infinite: {:?}", v.normal);
            if len < 0.9 {
                collapsed += 1;
            }
        }
        assert!(strokes > 0, "the repeated-point back-track must still produce stroke vertices");
        assert_eq!(collapsed, 0, "{collapsed} of {strokes} normals are not unit length");
    }

    /// Total STROKE vertex count for one `Line` op over a fixed exact
    /// back-track (`(0,0) -> (100,0) -> (0,0)`), at the given `cap`/`join`.
    /// Shared by the two apex-cap tests below.
    fn backtrack_stroke_vertex_count(cap: crate::style::LineCap, join: crate::style::LineJoin) -> usize {
        let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
            geo_types::LineString(vec![
                geo_types::Coord { x: 0, y: 0 },
                geo_types::Coord { x: 100, y: 0 },
                geo_types::Coord { x: 0, y: 0 },
            ]),
        ]));
        let plan = PaintPlan {
            ops: vec![PaintOp::Line {
                metrics: false,
                color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                width_from: 2.0,
                width_to: 2.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap,
                join,
                dash: DashPattern::none(),
                geometry: std::sync::Arc::new(geometry),
            }],
        };
        let mesh = tessellate_plan(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        mesh.vertices.iter().filter(|v| v.is_stroke > 0.5).count()
    }

    /// A back-track's apex must draw the layer's JOIN shape, not its cap:
    /// with `line-cap: butt` (which by itself draws no extra geometry at
    /// an end), a `round` join must still draw a round arc at the apex,
    /// giving strictly MORE vertices than a `miter` join does -- both
    /// segments' true ends are butt-capped identically either way, and a
    /// plain 2-point segment has no other join for `line-join` to affect,
    /// so any difference is the apex. Before this change the apex always
    /// took `line-cap` regardless of `line-join`, so `round` and `miter`
    /// produced the exact same (butt) vertex count here.
    #[test]
    fn a_backtrack_apex_with_a_round_join_outdraws_a_miter_join_under_butt_cap() {
        let round = backtrack_stroke_vertex_count(crate::style::LineCap::Butt, crate::style::LineJoin::Round);
        let miter = backtrack_stroke_vertex_count(crate::style::LineCap::Butt, crate::style::LineJoin::Miter);
        assert!(
            round > miter,
            "a round join must draw more geometry at the apex than a miter join, even \
             though line-cap is butt on both: round={round} miter={miter}"
        );
    }

    /// The mirror of the test above: with `line-cap: round` (which by
    /// itself draws a full arc at a real end), a `miter` join must still
    /// draw a plain butt at the apex -- strictly FEWER vertices than a
    /// `round` join does, even though `line-cap` is round on both. Before
    /// this change the apex always took `line-cap`, so `miter` and `round`
    /// produced the exact same (round) vertex count here.
    #[test]
    fn a_backtrack_apex_with_a_miter_join_underdraws_a_round_join_under_round_cap() {
        let round = backtrack_stroke_vertex_count(crate::style::LineCap::Round, crate::style::LineJoin::Round);
        let miter = backtrack_stroke_vertex_count(crate::style::LineCap::Round, crate::style::LineJoin::Miter);
        assert!(
            miter < round,
            "a miter join must draw less geometry at the apex than a round join, even \
             though line-cap is round on both: round={round} miter={miter}"
        );
    }

    /// Distance from the origin must stop mattering. The same shape near
    /// the origin and far from it must extrude identically.
    #[test]
    fn normals_do_not_depend_on_distance_from_the_origin() {
        let build = |offset: i32| {
            let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
                geo_types::LineString(
                    (0..50)
                        .map(|i| geo_types::Coord { x: offset + i * 1000, y: offset + i * 300 })
                        .collect(),
                ),
            ]));
            tessellate_plan(
                &PaintPlan {
                    ops: vec![PaintOp::Line {
                        metrics: false,
                        color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                        color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                        width_from: 2.0,
                        width_to: 2.0,
                        gap_from: 0.0,
                        gap_to: 0.0,
                        cap: crate::style::LineCap::Butt,
                        join: crate::style::LineJoin::Miter,
                        dash: DashPattern::none(),
                        geometry: std::sync::Arc::new(geometry),
                    }],
                },
                1,
                OVERLAY_COORD_SCALE,
                Subdivision::NONE,
            )
        };
        let near = build(0);
        let far = build(31_000_000);
        assert_eq!(near.vertices.len(), far.vertices.len());
        for (a, b) in near.vertices.iter().zip(far.vertices.iter()) {
            assert!((a.normal[0] - b.normal[0]).abs() < 0.02, "{:?} vs {:?}", a.normal, b.normal);
            assert!((a.normal[1] - b.normal[1]).abs() < 0.02, "{:?} vs {:?}", a.normal, b.normal);
        }
    }

    /// Positions survive the translation back into the shared frame. The
    /// mesh must still span the geometry it was built from.
    #[test]
    fn a_split_polyline_still_spans_its_original_extent() {
        let far = 31_000_000i32;
        let last = far + 199 * 1000;
        let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
            geo_types::LineString(
                (0..200).map(|i| geo_types::Coord { x: far + i * 1000, y: far }).collect(),
            ),
        ]));
        let mesh = tessellate_plan(
            &PaintPlan {
                ops: vec![PaintOp::Line {
                    metrics: false,
                    color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    width_from: 2.0,
                    width_to: 2.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: DashPattern::none(),
                    geometry: std::sync::Arc::new(geometry),
                }],
            },
            1,
            OVERLAY_COORD_SCALE,
            Subdivision::NONE,
        );
        let xs: Vec<f32> = mesh.vertices.iter().map(|v| v.position[0]).collect();
        let min = xs.iter().cloned().fold(f32::MAX, f32::min);
        let max = xs.iter().cloned().fold(f32::MIN, f32::max);
        // Within a few units: the extremes are the path's own ends, and f32
        // spacing at 3.1e7 is 4.
        assert!((min - far as f32).abs() < 16.0, "min {min} vs {far}");
        assert!((max - last as f32).abs() < 16.0, "max {max} vs {last}");
    }

    /// Dash phase must not restart at a run boundary, or a dashed overlay
    /// line visibly resets its pattern every ~33 km.
    #[test]
    fn line_distance_accumulates_across_run_boundaries() {
        let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(vec![
            geo_types::LineString(
                (0..200).map(|i| geo_types::Coord { x: i * 100_000, y: 0 }).collect(),
            ),
        ]));
        let mesh = tessellate_plan(
            &PaintPlan {
                ops: vec![PaintOp::Line {
                    metrics: false,
                    color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
                    width_from: 2.0,
                    width_to: 2.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: DashPattern::from_runs(&[2.0, 1.0]),
                    geometry: std::sync::Arc::new(geometry),
                }],
            },
            1,
            OVERLAY_COORD_SCALE,
            Subdivision::NONE,
        );
        let max_distance =
            mesh.vertices.iter().map(|v| v.line_distance).fold(0.0_f32, f32::max);
        // The path is 199 * 100_000 units long. If distance restarted at
        // each run it would never exceed one run's own length.
        assert!(max_distance > 1.9e7, "max line_distance was {max_distance}");
    }

    /// The regression test the brief's
    /// `a_polyline_far_from_the_origin_still_has_unit_normals` was meant to
    /// be, but calibrated for the world AFTER `OVERLAY_COORD_SCALE` landed.
    ///
    /// The 94%-collapse measurement in the spec was taken when the overlay
    /// still multiplied its coordinates by `COORD_SCALE`, putting lyon at
    /// ~3.1e10, where the f32 spacing is 2048 -- forty times the half-width
    /// of 50, so `(side_point - position)` rounds to exactly zero. With
    /// `OVERLAY_COORD_SCALE` the same dataset reaches lyon at ~3.1e7, where
    /// the spacing is only 2 and the normals were already fine: the brief's
    /// four tests all PASSED before this change. See the task report.
    ///
    /// Collapse begins where the spacing approaches the half-width, i.e.
    /// above ~2^29. `i32` geometry reaches 2.1e9, so this is inside what the
    /// grid can express -- one badly-centred overlay origin away. Measured
    /// here before the fix: 598 of 598 stroke normals outside the unit band,
    /// worst |len - 1| = 0.81. After it, every one is exactly unit, because
    /// the run is tessellated at the origin and the magnitude never reaches
    /// lyon at all.
    /// Counts stroke vertices and how many of their normals are not unit
    /// length -- the shape every collapse test below asserts on.
    fn count_collapsed_normals(mesh: &TileMesh) -> (usize, usize) {
        let mut collapsed = 0;
        let mut strokes = 0;
        for v in &mesh.vertices {
            if v.is_stroke < 0.5 {
                continue;
            }
            strokes += 1;
            let len = (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt();
            if len < 0.9 || len > 1.1 {
                collapsed += 1;
            }
        }
        (strokes, collapsed)
    }

    #[test]
    fn normals_stay_unit_at_the_far_edge_of_the_coordinate_grid() {
        let far = 1_000_000_000;
        // Two steps deliberately: 1_000 keeps the path inside
        // `MAX_RUN_EXTENT` so it is a SINGLE run (rebasing alone is under
        // test), while 100_000 spans 1.99e7 and genuinely SPLITS, so runs at
        // index > 0 -- the ones tessellated in a frame whose offset is not
        // the line's own start -- have their normals checked too. That
        // combination, far from the origin AND split, is what this whole
        // task exists for.
        for (step, must_split) in [(1_000, false), (100_000, true)] {
            let coords: Vec<geo_types::Coord<i32>> =
                (0..200).map(|i| geo_types::Coord { x: far + i * step, y: far }).collect();

            let lyon_points: Vec<lyon::math::Point> = coords
                .iter()
                .map(|c| to_lyon_point((c.x, c.y), OVERLAY_COORD_SCALE))
                .collect();
            let runs = crate::render::run_split::split_into_runs(
                &lyon_points,
                crate::render::run_split::MAX_RUN_EXTENT,
            );
            assert_eq!(
                runs.len() > 1,
                must_split,
                "step {step} produced {} run(s); this test is only meaningful if it splits as expected",
                runs.len()
            );

            let geometry = geo_types::Geometry::MultiLineString(geo_types::MultiLineString(
                vec![geo_types::LineString(coords)],
            ));
            let mesh = tessellate_plan(
                &PaintPlan {
                    ops: vec![PaintOp::Line {
                        metrics: false,
                        color_from: red(),
                        color_to: red(),
                        width_from: 2.0,
                        width_to: 2.0,
                        gap_from: 0.0,
                        gap_to: 0.0,
                        cap: crate::style::LineCap::Butt,
                        join: crate::style::LineJoin::Miter,
                        dash: DashPattern::none(),
                        geometry: std::sync::Arc::new(geometry),
                    }],
                },
                1,
                OVERLAY_COORD_SCALE,
                Subdivision::NONE,
            );

            let (strokes, collapsed) = count_collapsed_normals(&mesh);
            assert!(strokes > 0, "step {step}: the line must produce stroke vertices at all");
            assert_eq!(
                collapsed, 0,
                "step {step}: {collapsed} of {strokes} normals are not unit length"
            );
        }
    }

    /// A DOT must be magnitude-independent too -- but note what this test
    /// does and does not demonstrate.
    ///
    /// It PASSED before dots were rebased, and it passes after. Measured
    /// from 0 to 1e12 lyon units, a dot's normals are exactly unit either
    /// way, because lyon's round "empty cap" emits unit radial normals
    /// directly instead of deriving them from the
    /// `(side_point - position) / half_width` subtraction that collapses for
    /// lines. So this is a CHARACTERISATION test pinning a property the dot
    /// path now holds structurally (by being tessellated at the origin)
    /// rather than by lyon's good graces -- it is not a regression test for
    /// a bug that existed. It would start earning its keep the day lyon
    /// changed how it builds cap normals.
    #[test]
    fn a_dot_far_from_the_origin_still_has_unit_normals() {
        let far = 1_000_000_000;
        let geometry = geo_types::Geometry::MultiPoint(geo_types::MultiPoint(vec![
            geo_types::Point::new(far, far),
            geo_types::Point::new(far, -far),
        ]));
        let mesh = tessellate_plan(
            &PaintPlan {
                ops: vec![PaintOp::Line {
                    metrics: false,
                    color_from: red(),
                    color_to: red(),
                    width_from: 8.0,
                    width_to: 8.0,
                    gap_from: 0.0,
                    gap_to: 0.0,
                    cap: crate::style::LineCap::Butt,
                    join: crate::style::LineJoin::Miter,
                    dash: DashPattern::none(),
                    geometry: std::sync::Arc::new(geometry),
                }],
            },
            1,
            OVERLAY_COORD_SCALE,
            Subdivision::NONE,
        );

        let (strokes, collapsed) = count_collapsed_normals(&mesh);
        assert!(strokes > 0, "the dots must produce stroke vertices at all");
        assert_eq!(collapsed, 0, "{collapsed} of {strokes} dot normals are not unit length");

        // ... and the disc still sits where the dot was asked for, not at
        // the origin the cap was tessellated in.
        for v in &mesh.vertices {
            assert!(
                (v.position[0] - far as f32).abs() < 128.0,
                "dot vertex at {:?} is not at the requested centre",
                v.position
            );
        }
    }

    // --- the subset paint rewrite -------------------------------------

    /// Three ops with distinct colours, so "which op did the rewrite
    /// touch" is answerable from the vertices alone.
    fn three_coloured_ops(a: Rgba, b: Rgba, c: Rgba) -> PaintPlan {
        PaintPlan {
            ops: vec![fill_at(0, 0, a), line_op(b, 4.0), fill_at(200, 200, c)],
        }
    }

    fn colours_of(mesh: &TileMesh, spans: &[OpSpan], op: usize) -> Vec<[f32; 4]> {
        mesh.vertices[spans[op].vertices.clone()].iter().map(|v| v.color).collect()
    }

    /// A paint-only feature update rewrites ONE op's vertices. Every other
    /// op must come out of it byte for byte unchanged -- recolouring a
    /// neighbour is not a crash, it is a wrong-coloured line that no
    /// assertion about the recoloured op would ever catch.
    #[test]
    fn rewriting_named_ops_leaves_every_other_op_untouched() {
        let before = three_coloured_ops(red(), green(), blue());
        let (mut mesh, spans) = tessellate_plan_with_ranges(&before, 4096, COORD_SCALE, Subdivision::NONE);
        let untouched_first = colours_of(&mesh, &spans, 0);
        let untouched_last = colours_of(&mesh, &spans, 2);

        let after = three_coloured_ops(red(), red(), blue());
        rewrite_paint_attributes_for_ops(&mut mesh, &spans, &after, &[1]);

        assert_eq!(colours_of(&mesh, &spans, 0), untouched_first);
        assert_eq!(colours_of(&mesh, &spans, 2), untouched_last);
        assert!(
            colours_of(&mesh, &spans, 1).iter().all(|c| *c == [1.0, 0.0, 0.0, 1.0]),
            "the named op is the one that changed"
        );
    }

    /// The subset rewrite is the wholesale one restricted to a list, not a
    /// second implementation of it: naming every op must produce exactly
    /// what `rewrite_paint_attributes` produces.
    #[test]
    fn naming_every_op_matches_the_wholesale_rewrite() {
        let before = three_coloured_ops(red(), green(), blue());
        let after = three_coloured_ops(blue(), red(), green());

        let (mut wholesale, spans) = tessellate_plan_with_ranges(&before, 4096, COORD_SCALE, Subdivision::NONE);
        rewrite_paint_attributes(&mut wholesale, &spans, &after);

        let (mut subset, _) = tessellate_plan_with_ranges(&before, 4096, COORD_SCALE, Subdivision::NONE);
        rewrite_paint_attributes_for_ops(&mut subset, &spans, &after, &[0, 1, 2]);

        assert_eq!(wholesale.vertices, subset.vertices);
    }

    /// Naming nothing writes nothing.
    #[test]
    fn naming_no_ops_rewrites_nothing() {
        let before = three_coloured_ops(red(), green(), blue());
        let (mut mesh, spans) = tessellate_plan_with_ranges(&before, 4096, COORD_SCALE, Subdivision::NONE);
        let untouched = mesh.vertices.clone();
        rewrite_paint_attributes_for_ops(
            &mut mesh,
            &spans,
            &three_coloured_ops(blue(), blue(), blue()),
            &[],
        );
        assert_eq!(mesh.vertices, untouched);
    }

    /// The wholesale caller relies on the 1:1 assertion to catch a `ranges`
    /// table that does not belong to `plan`. Adding a subset form must not
    /// weaken it.
    #[test]
    #[should_panic(expected = "ranges must align 1:1")]
    fn the_wholesale_rewrite_still_refuses_misaligned_ranges() {
        let plan = three_coloured_ops(red(), green(), blue());
        let (mut mesh, spans) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        rewrite_paint_attributes(&mut mesh, &spans[..2], &plan);
    }

    // --- partial vertex uploads ---------------------------------------

    /// The byte range an op's vertices occupy, which is what a partial
    /// `queue.write_buffer` is given. Asserted against arithmetic spelled
    /// out here rather than against the function's own expression: an
    /// off-by-one in this offset corrupts GEOMETRY, not colour, and looks
    /// nothing like the paint bug that caused it.
    #[test]
    fn a_vertex_range_maps_to_the_byte_range_the_gpu_is_given() {
        let stride = std::mem::size_of::<Vertex>() as u64;
        assert_eq!(stride, 92, "23 f32 fields -- the stride the byte offsets below are built on");
        assert_eq!(vertex_byte_range(&(0..0)), 0..0);
        assert_eq!(vertex_byte_range(&(0..1)), 0..stride);
        assert_eq!(vertex_byte_range(&(1..3)), stride..3 * stride);
        assert_eq!(vertex_byte_range(&(7..7)), 7 * stride..7 * stride);
    }

    /// ...and the range's own bytes are the ones the buffer holds there:
    /// slicing the whole mesh's bytes by it gives the same bytes as
    /// slicing the vertices by the vertex range.
    #[test]
    fn a_vertex_ranges_bytes_are_the_bytes_at_its_offset() {
        let plan = three_coloured_ops(red(), green(), blue());
        let (mesh, spans) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let all: &[u8] = bytemuck::cast_slice(&mesh.vertices);
        for span in &spans {
            let bytes = vertex_byte_range(&span.vertices);
            let slice: &[u8] = bytemuck::cast_slice(&mesh.vertices[span.vertices.clone()]);
            assert_eq!(&all[bytes.start as usize..bytes.end as usize], slice);
        }
    }

    /// Ops from one feature are adjacent by construction, and consecutive
    /// features are adjacent too, so a run of recoloured features is ONE
    /// buffer write rather than one per op.
    #[test]
    fn touching_vertex_spans_coalesce_into_one_write() {
        let span = |v: std::ops::Range<usize>| OpSpan { vertices: v, indices: 0..0 };
        let ranges = [span(0..10), span(10..20), span(20..30), span(30..40)];
        assert_eq!(coalesced_vertex_spans(&ranges, &[0, 1, 2]), vec![0..30]);
        assert_eq!(coalesced_vertex_spans(&ranges, &[0, 2]), vec![0..10, 20..30]);
        assert_eq!(coalesced_vertex_spans(&ranges, &[3]), vec![30..40]);
        assert_eq!(coalesced_vertex_spans(&ranges, &[]), Vec::<std::ops::Range<usize>>::new());
    }

    /// An op that tessellated to nothing is dropped, not merged through:
    /// wgpu rejects a zero-length write, and an empty span between two
    /// others does not make them one run.
    #[test]
    fn an_empty_span_is_dropped_rather_than_written() {
        let span = |v: std::ops::Range<usize>| OpSpan { vertices: v, indices: 0..0 };
        let ranges = [span(0..10), span(10..10), span(10..20)];
        assert_eq!(coalesced_vertex_spans(&ranges, &[1]), Vec::<std::ops::Range<usize>>::new());
        assert_eq!(coalesced_vertex_spans(&ranges, &[0, 1, 2]), vec![0..20]);
    }

    // --- pooling the tessellators ---------------------------------------
    //
    // `append_ops` now hoists ONE `FillTessellator`/`StrokeTessellator` and
    // three scratch point `Vec`s for the whole call, reusing them across
    // every op instead of each op building its own from empty. The risk
    // named in the task brief is that a pooled tessellator carries state
    // between calls that subtly changes its output -- so this does not
    // just run the pooled path and eyeball it; it builds a second buffer by
    // hand, giving EVERY op its own brand-new `FillTessellator` /
    // `StrokeTessellator` and brand-new scratch `Vec`s (the exact shape
    // `append_fill`/`append_stroke` had before this change), and asserts
    // the two vertex/index buffers -- and the op spans -- are equal.
    // `Vertex` derives `PartialEq` over its raw `f32` fields (no epsilon),
    // so this is a bit-exact comparison, not an approximate one.

    /// A plan touching every geometry shape `append_fill`/`append_stroke`
    /// handle: a polygon with a hole (forces the interior-ring path), two
    /// more plain fills (so the pooled fill tessellator is reused across
    /// more than one op), a plain stroke, a dashed stroke with different
    /// from/to width and colour (exercises the run-splitting/translate-back
    /// path), and a point stroke (the dot/empty-cap path). If pooling ever
    /// smuggles state between ops, this is the plan that would show it.
    fn pooling_proof_ops() -> Vec<PaintOp> {
        vec![
            PaintOp::Fill { color_from: red(), color_to: red(), geometry: square_polygon_with_hole() },
            fill_at(200, 200, green()),
            fill_at(300, 10, blue()),
            line_op(red(), 4.0),
            PaintOp::Line {
                metrics: false,
                color_from: green(),
                color_to: blue(),
                width_from: 2.0,
                width_to: 6.0,
                gap_from: 0.0,
                gap_to: 0.0,
                cap: crate::style::LineCap::Butt,
                join: crate::style::LineJoin::Miter,
                dash: DashPattern { runs: [4.0, 2.0, 0.0, 0.0], count: 2 },
                geometry: diagonal_line(),
            },
            stroke_op(Geometry::Point(geo_types::Point::new(50, 50))),
        ]
    }

    /// Tessellates `ops` the way `append_fill`/`append_stroke` worked
    /// BEFORE pooling: a fresh `FillTessellator`/`StrokeTessellator` and
    /// fresh scratch `Vec`s constructed inline for every single op. This is
    /// the pre-pooling baseline the pooled path (`append_ops`, exercised
    /// through `tessellate_plan_with_ranges` below) is checked against.
    fn tessellate_with_fresh_tessellator_per_op(
        ops: &[PaintOp],
        coord_scale: f32,
    ) -> (VertexBuffers<Vertex, u32>, Vec<OpSpan>) {
        let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
        let mut spans = Vec::with_capacity(ops.len());
        for op in ops {
            let vertex_start = buffers.vertices.len();
            let index_start = buffers.indices.len();
            match op {
                PaintOp::Background { .. } | PaintOp::Extrusion { .. } => {}
                PaintOp::Fill { color_from, color_to, geometry } => {
                    let mut tess = FillTessellator::new();
                    let mut ring_scratch = Vec::new();
                    append_fill(
                        &mut buffers,
                        geometry,
                        *color_from,
                        *color_to,
                        coord_scale,
                        &mut tess,
                        &mut ring_scratch,
                    );
                }
                PaintOp::Line { color_from, color_to, width_from, width_to, gap_from, gap_to, cap, join, dash, metrics, geometry } => {
                    let mut tess = StrokeTessellator::new();
                    let mut points_scratch = Vec::new();
                    let mut local_scratch = Vec::new();
                    append_stroke(
                        &mut buffers,
                        geometry,
                        *color_from,
                        *color_to,
                        *width_from,
                        *width_to,
                        *gap_from,
                        *gap_to,
                        *cap,
                        *join,
                        *dash,
                        *metrics,
                        coord_scale,
                        4096,
                        Subdivision::NONE,
                        &mut tess,
                        &mut points_scratch,
                        &mut local_scratch,
                        &mut Vec::new(),
                        &mut SubdivideScratch::default(),
                    );
                }
            }
            spans.push(OpSpan {
                vertices: vertex_start..buffers.vertices.len(),
                indices: index_start..buffers.indices.len(),
            });
        }
        (buffers, spans)
    }

    /// The one hazard of tessellating into a reused buffer: if the arena's
    /// contents are not cleared first, this plan's triangles land AFTER the
    /// previous collection's and every `OpSpan` is offset by however many
    /// vertices the old mesh had. That is a silently wrong mesh -- garbage
    /// geometry drawn in the right colours -- not a crash, and no other
    /// test in this file would catch it, because every other test starts
    /// from an empty buffer.
    ///
    /// So: a bake into a FULL arena must equal a bake into an empty one,
    /// byte for byte, spans included. The donor here is deliberately a
    /// different plan from the one being tessellated, so its vertices could
    /// not coincidentally be a prefix of the expected output.
    #[test]
    fn a_reused_arena_produces_a_byte_identical_mesh() {
        let donor_plan = PaintPlan { ops: vec![fill_at(700, 700, blue()), line_op(green(), 9.0)] };
        let (donor, _) = tessellate_plan_with_ranges(&donor_plan, 4096, COORD_SCALE, Subdivision::NONE);
        assert!(
            !donor.vertices.is_empty(),
            "the donor must actually carry vertices, or this test proves nothing"
        );

        let plan = PaintPlan { ops: pooling_proof_ops() };
        let (fresh, fresh_spans) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let (reused, reused_spans) =
            tessellate_plan_with_ranges_reusing(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| false, donor)
                .expect("a predicate that never fires must always produce a mesh");

        assert_eq!(
            fresh.vertices, reused.vertices,
            "tessellating into a reused buffer must produce the same vertices as into a fresh \
             one -- a mismatch here is the arena appending to stale contents"
        );
        assert_eq!(fresh.indices, reused.indices, "and the same indices");
        assert_eq!(
            fresh_spans, reused_spans,
            "and the same op spans -- these index into the mesh, so a stale prefix would shift \
             every one of them"
        );
    }

    /// The capacity actually survives the round trip, which is the entire
    /// reason the arena exists.
    ///
    /// Without this the change is unfalsifiable: a `clear()` that had been
    /// written as `= Vec::new()` would pass the byte-identity test above
    /// and quietly reallocate on every bake, leaving the defect in place
    /// behind a correct-looking mesh.
    #[test]
    fn a_reused_arena_hands_its_capacity_on_rather_than_reallocating() {
        let plan = PaintPlan { ops: pooling_proof_ops() };
        let (first, _) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let donated_vertices = first.vertices.capacity();
        let donated_indices = first.indices.capacity();
        assert!(donated_vertices > 0, "the first bake must have reserved something");

        let vertex_ptr = first.vertices.as_ptr();
        let (second, _) =
            tessellate_plan_with_ranges_reusing(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| false, first)
                .expect("a predicate that never fires must always produce a mesh");

        assert_eq!(
            second.vertices.as_ptr(),
            vertex_ptr,
            "re-baking the same plan into its own previous buffers must not reallocate -- same \
             allocation, same address"
        );
        assert!(
            second.vertices.capacity() >= donated_vertices
                && second.indices.capacity() >= donated_indices,
            "capacity must never shrink across a reuse: that is the high-water-mark discipline \
             this exists to apply"
        );
    }

    /// A donor far larger than the work in front of it is released rather
    /// than carried forever.
    ///
    /// Without this the arena is a ratchet: one momentary large fleet
    /// leaves every later bake sitting on that collection's buffers no
    /// matter what it is actually drawing. Bounded by the largest push, but
    /// "bounded by the worst thing that ever happened" is not a bound worth
    /// having on a view that lives as long as the process.
    #[test]
    fn a_donor_far_larger_than_the_work_is_released() {
        let plan = PaintPlan { ops: pooling_proof_ops() };
        let (want_vertices, want_indices) = estimate_buffer_capacity(&plan.ops, COORD_SCALE, 4096, Subdivision::NONE);
        assert!(want_vertices > 0, "the fixture must need a real reservation");

        let donor = TileMesh {
            vertices: Vec::with_capacity(want_vertices * 10),
            indices: Vec::with_capacity(want_indices * 10),
        };
        assert_eq!(
            donor.vertices.capacity(),
            want_vertices * 10,
            "`Vec::with_capacity` is expected to allocate exactly what was asked for; if it \
             ever does not, the thresholds below are being measured against the wrong number"
        );

        let (mesh, _) =
            tessellate_plan_with_ranges_reusing(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| false, donor)
                .expect("a predicate that never fires must always produce a mesh");

        assert!(
            mesh.vertices.capacity() <= want_vertices * 2,
            "a 10x oversized donor must be dropped, leaving under 2x the current need: got {} \
             for a need of {want_vertices}",
            mesh.vertices.capacity()
        );
        assert!(
            mesh.indices.capacity() <= want_indices * 2,
            "and the index buffer is released on its own terms, not the vertex buffer's"
        );
        assert!(
            mesh.vertices.capacity() >= want_vertices,
            "releasing must not leave the bake short -- the reservation still has to happen"
        );
    }

    /// The other half of the threshold, and the one that decides whether
    /// shrinking is an improvement or a regression: a workload that
    /// oscillates between two sizes must NEVER release.
    ///
    /// Alternating between S and 2S is the commonest shape a fleet has. A
    /// tighter rule -- release whenever the buffer is merely larger than
    /// needed -- would free and reallocate on every tick of exactly that
    /// pattern, which is strictly worse than not pooling at all. At exactly
    /// 2x the buffer is kept, so the big allocation serves both sizes.
    #[test]
    fn a_donor_at_the_threshold_is_kept_so_an_oscillating_workload_never_thrashes() {
        let plan = PaintPlan { ops: pooling_proof_ops() };
        let (want_vertices, want_indices) = estimate_buffer_capacity(&plan.ops, COORD_SCALE, 4096, Subdivision::NONE);

        let donor = TileMesh {
            vertices: Vec::with_capacity(want_vertices * 2),
            indices: Vec::with_capacity(want_indices * 2),
        };
        assert_eq!(donor.vertices.capacity(), want_vertices * 2);
        let kept = donor.vertices.as_ptr();

        let (mesh, _) =
            tessellate_plan_with_ranges_reusing(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| false, donor)
                .expect("a predicate that never fires must always produce a mesh");

        assert_eq!(
            mesh.vertices.as_ptr(),
            kept,
            "a donor at exactly twice the need must be REUSED, not released -- the same \
             allocation, the same address. Releasing here is what turns a fleet oscillating \
             between two sizes into a free and a malloc on every single tick"
        );
        assert_eq!(
            mesh.vertices.capacity(),
            want_vertices * 2,
            "and it keeps the larger capacity, ready for the next tick's bigger half"
        );
    }

    /// Shrinking converges instead of shedding one buffer per bake forever.
    ///
    /// Each release leaves capacity equal to the need, so triggering the
    /// next one takes a FURTHER halving. Re-baking the same plan after a
    /// release must therefore reuse rather than release again -- otherwise
    /// the hysteresis would have replaced a permanent over-allocation with
    /// a permanent alloc/free pair, which is the worse of the two.
    #[test]
    fn releasing_once_settles_rather_than_releasing_on_every_bake() {
        let plan = PaintPlan { ops: pooling_proof_ops() };
        let (want_vertices, _) = estimate_buffer_capacity(&plan.ops, COORD_SCALE, 4096, Subdivision::NONE);

        let oversized = TileMesh {
            vertices: Vec::with_capacity(want_vertices * 10),
            indices: Vec::with_capacity(4),
        };
        let (settled, _) =
            tessellate_plan_with_ranges_reusing(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| false, oversized)
                .expect("never cancelled");
        let settled_ptr = settled.vertices.as_ptr();

        let (again, _) =
            tessellate_plan_with_ranges_reusing(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| false, settled)
                .expect("never cancelled");

        assert_eq!(
            again.vertices.as_ptr(),
            settled_ptr,
            "once the buffer has shrunk to the size of the work, the next bake at the same size \
             must reuse it -- a hysteresis that releases every time is worse than none"
        );
    }

    /// A cancelled bake gives its buffers back instead of dropping them.
    ///
    /// This is the case that matters most, and the least obvious: a run
    /// that supersedes bakes often is exactly the run that can least afford
    /// to pay a hundred-megabyte reservation again, so throwing away an
    /// aborted bake's allocation would make the cancellation path punish
    /// the very workload cancellation exists to help.
    #[test]
    fn a_cancelled_bake_returns_its_buffers_rather_than_dropping_them() {
        let plan = PaintPlan { ops: pooling_proof_ops() };
        let (donor, _) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let donated = donor.vertices.capacity();
        assert!(donated > 0, "the donor must carry a real reservation");

        // Cancelled at the door: the buffers come straight back untouched.
        let returned = tessellate_plan_with_ranges_reusing(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| true, donor)
            .expect_err("a predicate that is already true must cancel");
        assert!(
            returned.vertices.is_empty(),
            "an abandoned mesh must be handed back empty -- it is capacity, not data"
        );
        assert_eq!(
            returned.vertices.capacity(),
            donated,
            "and with its capacity intact, which is the only reason to hand it back at all"
        );

        // Cancelled part way through: same rule, after triangles were
        // written into it.
        let polls = std::cell::Cell::new(0usize);
        let mid = tessellate_plan_with_ranges_reusing(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| {
            let seen = polls.get();
            polls.set(seen + 1);
            seen > 2
        }, returned)
        .expect_err("a predicate that fires after two ops must cancel");
        assert!(
            mid.vertices.capacity() >= donated,
            "a bake abandoned mid-flight must still hand its capacity back"
        );
    }

    /// A cancellation that is already true when the tessellator is entered
    /// must stop before `reserve_for_ops`.
    ///
    /// That reservation is the single largest allocation this function
    /// makes -- `reserve_exact` of the WHOLE estimate, over a hundred
    /// megabytes for a large fleet collection -- and it happens before one
    /// triangle is written. A cancellation that only took effect after it
    /// would leave the entire defect in place, because the memory is what
    /// costs, not the triangles.
    #[test]
    fn a_bake_cancelled_at_the_door_never_reserves() {
        let plan = PaintPlan { ops: pooling_proof_ops() };
        assert!(
            tessellate_plan_with_ranges_cancellable(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| true).is_none(),
            "a bake cancelled before it starts must produce no mesh"
        );
    }

    /// The point of the whole exercise: a bake that is superseded PART WAY
    /// THROUGH abandons what it has and returns, rather than finishing a
    /// mesh nobody will install.
    ///
    /// The predicate here turns true after two ops, which is what a real
    /// supersession looks like -- the store moved on while lyon was in the
    /// middle of the collection. Before this existed, `install_overlay_mesh`
    /// dropped the finished mesh at the door and every triangle in it had
    /// still been computed and every byte still allocated.
    #[test]
    fn a_bake_superseded_between_ops_abandons_its_partial_mesh() {
        let plan = PaintPlan { ops: pooling_proof_ops() };
        assert!(plan.ops.len() > 3, "the plan must have ops left to abandon, or this is vacuous");

        let polls = std::cell::Cell::new(0usize);
        let cancelled = tessellate_plan_with_ranges_cancellable(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| {
            let seen = polls.get();
            polls.set(seen + 1);
            // Poll 0 is the pre-reservation check; the per-op checks follow.
            seen > 2
        });

        assert!(cancelled.is_none(), "a superseded bake must not return a truncated mesh");
        assert!(
            polls.get() < plan.ops.len() + 1,
            "it must have stopped early, not walked every op: {} polls over {} ops",
            polls.get(),
            plan.ops.len()
        );
    }

    /// The uncancelled path must be the same function, not a parallel one.
    ///
    /// `tessellate_plan_with_ranges` now delegates to the cancellable form
    /// with a predicate that never fires, so the two must agree byte for
    /// byte. If they ever did not, every caller that cannot be cancelled --
    /// the tile path, and `splice_changed_ops` -- would silently be running
    /// different tessellation from the overlay.
    #[test]
    fn a_cancellation_that_never_fires_changes_nothing() {
        let plan = PaintPlan { ops: pooling_proof_ops() };
        let (plain, plain_spans) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);
        let (cancellable, cancellable_spans) =
            tessellate_plan_with_ranges_cancellable(&plan, 4096, COORD_SCALE, Subdivision::NONE, &|| false)
                .expect("a predicate that never fires must always produce a mesh");

        assert!(!plain.vertices.is_empty(), "the fixture must produce vertices, or this is vacuous");
        assert_eq!(plain.vertices, cancellable.vertices, "vertices must be identical");
        assert_eq!(plain.indices, cancellable.indices, "indices must be identical");
        assert_eq!(plain_spans, cancellable_spans, "op spans must be identical");
    }

    #[test]
    fn pooled_tessellators_match_fresh_ones_byte_for_byte() {
        let ops = pooling_proof_ops();
        let plan = PaintPlan { ops: ops.clone() };

        // The pooled path: exactly what production code runs.
        let (pooled, pooled_spans) = tessellate_plan_with_ranges(&plan, 4096, COORD_SCALE, Subdivision::NONE);

        // The pre-pooling baseline: fresh tessellator/scratch per op.
        let (fresh, fresh_spans) = tessellate_with_fresh_tessellator_per_op(&ops, COORD_SCALE);

        assert!(
            !pooled.vertices.is_empty(),
            "the proof plan must actually produce vertices, or this test is vacuous"
        );
        assert_eq!(
            pooled.vertices, fresh.vertices,
            "a pooled FillTessellator/StrokeTessellator must emit byte-identical vertices to a \
             fresh one per op"
        );
        assert_eq!(
            pooled.indices, fresh.indices,
            "a pooled FillTessellator/StrokeTessellator must emit byte-identical indices to a \
             fresh one per op"
        );
        assert_eq!(pooled_spans, fresh_spans, "op spans must match too");
    }

    /// Same proof, run through `splice_changed_ops`'s code path rather than
    /// `tessellate_plan_with_ranges`'s: `append_ops` is called a second time
    /// there, on a different (smaller) ops slice, with ITS OWN fresh
    /// tessellators (pooling never crosses a call to `append_ops`, only ops
    /// within one call -- see `append_ops`'s doc comment). Appending the
    /// proof ops a second time with different geometry forces a splice
    /// rather than the whole-plan-changed fallback.
    #[test]
    fn pooled_tessellators_match_fresh_ones_through_a_splice() {
        let mut before_ops = pooling_proof_ops();
        // A trailing op the splice will retain, so `after` differs from
        // `before` only in its middle -- otherwise `splice_changed_ops`
        // takes the "everything changed" path and never calls `append_ops`
        // on a sub-slice at all.
        before_ops.push(fill_at(500, 500, red()));
        let mut after_ops = pooling_proof_ops();
        after_ops[1] = fill_at(199, 199, green()); // nudges one fill's geometry
        after_ops.push(fill_at(500, 500, red()));

        let before = PaintPlan { ops: before_ops };
        let after = PaintPlan { ops: after_ops.clone() };
        let (mut mesh, mut spans) = tessellate_plan_with_ranges(&before, 4096, COORD_SCALE, Subdivision::NONE);
        let spliced = splice_changed_ops(&mut mesh, &mut spans, &before, &after, COORD_SCALE, 4096, Subdivision::NONE);
        assert!(spliced, "expected the differing middle to take the splice path, not a full rebuild");

        // A full fresh re-tessellation of `after`, entirely through the
        // fresh-per-op baseline, is what the splice must still match.
        let (fresh, fresh_spans) = tessellate_with_fresh_tessellator_per_op(&after_ops, COORD_SCALE);
        assert_eq!(mesh.vertices, fresh.vertices, "a spliced pooled mesh must match a fresh full rebuild");
        assert_eq!(mesh.indices, fresh.indices, "a spliced pooled mesh must match a fresh full rebuild");
        assert_eq!(spans, fresh_spans, "op spans must match too");
    }
}



#[cfg(test)]
mod subdivided_fill_capacity_tests {
    use super::*;
    use crate::style::{PaintOp, PaintPlan, Rgba};
    use geo_types::{Geometry, LineString, Polygon};
    use std::sync::Arc;

    /// A ring of `n` points around `(cx, cy)`, radius `r`, its radius
    /// wobbled by `jag` -- the rural-block-group shape that crosses grid
    /// lines.
    fn ring(cx: f64, cy: f64, r: f64, n: usize, jag: f64) -> LineString<i32> {
        let mut pts: Vec<(i32, i32)> = (0..n)
            .map(|k| {
                let a = k as f64 / n as f64 * std::f64::consts::TAU;
                let rr = r * (1.0 + jag * ((k * 7919 % 13) as f64 / 13.0 - 0.5));
                ((cx + rr * a.cos()) as i32, (cy + rr * a.sin()) as i32)
            })
            .collect();
        pts.push(pts[0]);
        LineString::from(pts)
    }

    /// `(radius in cells, points, jag)`: inside one cell, then from half a
    /// cell to ten across, round, jagged and near-square.
    const SHAPES: [(f64, usize, f64); 7] = [
        (0.2, 200, 0.2),
        (0.5, 400, 0.3),
        (1.0, 1000, 0.3),
        (3.0, 3000, 0.3),
        (10.0, 5000, 0.2),
        (0.7, 60, 0.0),
        (5.0, 40, 0.0),
    ];

    fn fill_plan(r: f64, n: usize, jag: f64) -> PaintPlan {
        let cell = subdivide::OVERLAY_CELL_LYON_UNITS;
        let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        let l = ring(cell * 37.3, cell * 41.7, r * cell, n, jag);
        PaintPlan {
            ops: vec![PaintOp::Fill {
                color_from: black,
                color_to: black,
                geometry: Arc::new(Geometry::Polygon(Polygon::new(l, vec![]))),
            }],
        }
    }

    fn bake(plan: &PaintPlan) -> TileMesh {
        tessellate_plan_with_ranges(
            plan,
            subdivide::OVERLAY_SUBDIVISION_EXTENT,
            OVERLAY_COORD_SCALE,
            Subdivision::OVERLAY,
        )
        .0
    }

    /// The fill half of the estimate covers what subdivision really adds,
    /// so the buffer never falls back to doubling.
    #[test]
    fn a_subdivided_fill_is_covered_by_its_estimate() {
        for (r, n, jag) in SHAPES {
            let plan = fill_plan(r, n, jag);
            let (est_v, est_i) = estimate_buffer_capacity(
                &plan.ops,
                OVERLAY_COORD_SCALE,
                subdivide::OVERLAY_SUBDIVISION_EXTENT,
                Subdivision::OVERLAY,
            );
            let mesh = bake(&plan);
            assert!(
                est_v >= mesh.vertices.len() && est_i >= mesh.indices.len(),
                "r={r} n={n}: estimated {est_v}/{est_i}, real {}/{}",
                mesh.vertices.len(),
                mesh.indices.len()
            );
        }
    }

    /// THE A1 PROPERTY: a subdivided fill's mesh carries at most 10 % of
    /// slack. It used to carry up to 100 % -- the doubling past a
    /// fills-blind estimate.
    ///
    /// Every shape, eight times over at different offsets, so the mesh is
    /// past `SHRINK_MIN_BYTES` like a real overlay's rather than a toy.
    #[test]
    fn a_subdivided_fills_capacity_is_within_ten_percent_of_its_length() {
        let mut ops = Vec::new();
        for copy in 0..8 {
            for (r, n, jag) in SHAPES {
                let mut plan = fill_plan(r, n, jag);
                if let PaintOp::Fill { geometry, .. } = &mut plan.ops[0] {
                    let shift = (copy as f64 * 23.0 * subdivide::OVERLAY_CELL_LYON_UNITS) as i32;
                    let Geometry::Polygon(p) = &**geometry else { unreachable!() };
                    let moved: Vec<(i32, i32)> =
                        p.exterior().0.iter().map(|c| (c.x + shift, c.y)).collect();
                    *geometry = Arc::new(Geometry::Polygon(Polygon::new(LineString::from(moved), vec![])));
                }
                ops.extend(plan.ops);
            }
        }
        let mesh = bake(&PaintPlan { ops });
        let (v, i) = (mesh.vertices.len(), mesh.indices.len());
        assert!(
            v * std::mem::size_of::<Vertex>() >= SHRINK_MIN_BYTES,
            "the fixture must be a large mesh ({v} vertices)"
        );
        assert!(
            mesh.vertices.capacity() as f64 <= v as f64 * 1.1,
            "vertex capacity {} for {v}",
            mesh.vertices.capacity()
        );
        assert!(
            mesh.indices.capacity() as f64 <= i as f64 * 1.1,
            "index capacity {} for {i}",
            mesh.indices.capacity()
        );
    }

    /// A buffer that outgrows its reservation -- the doubling the old,
    /// fills-blind estimate caused on every state -- is shrunk back, at
    /// any size.
    #[test]
    fn an_outgrown_buffer_is_shrunk_back() {
        let mut v: Vec<u32> = Vec::with_capacity(10);
        v.extend(0..11);
        assert!(v.capacity() >= 20);
        shrink_if_overgrown(&mut v, true, true);
        assert_eq!(v.capacity(), 11);
    }
}

#[cfg(test)]
mod overlay_chunk_tests {
    use super::*;
    use crate::style::{PaintOp, PaintPlan, Rgba};
    use std::sync::Arc;

    const RED: Rgba = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
    const GREEN: Rgba = Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };

    fn ring(points: &[(i32, i32)]) -> geo_types::LineString<i32> {
        let mut closed = points.to_vec();
        closed.push(points[0]);
        geo_types::LineString::from(closed)
    }

    fn bake(polygons: Vec<geo_types::Polygon<i32>>, outline: Option<Rgba>) -> (TileMesh, OverlayExtras) {
        let plan = PaintPlan {
            ops: vec![PaintOp::Fill {
                color_from: GREEN,
                color_to: GREEN,
                geometry: Arc::new(geo_types::Geometry::MultiPolygon(geo_types::MultiPolygon(polygons))),
            }],
        };
        let (mesh, _, extras) = tessellate_overlay_plan_reusing(
            &plan,
            &[outline],
            subdivide::OVERLAY_SUBDIVISION_EXTENT,
            OVERLAY_COORD_SCALE,
            Subdivision::OVERLAY,
            &|| false,
            TileMesh::default(),
        )
        .unwrap();
        (mesh, extras)
    }

    /// The outline is one LINE-LIST pair per ring edge -- the exterior's
    /// and every hole's -- over vertices the fill itself emitted, and
    /// nothing is tessellated a second time.
    #[test]
    fn the_outline_index_count_equals_the_rings_edge_count() {
        let polygon = || {
            geo_types::Polygon::new(
                ring(&[(0, 0), (1000, 0), (1000, 1000), (500, 1300), (0, 1000)]),
                vec![ring(&[(200, 200), (400, 200), (300, 400)])],
            )
        };
        let (mesh, extras) = bake(vec![polygon()], Some(RED));
        assert_eq!(extras.outline.len(), 2 * (5 + 3));
        let (plain, plain_extras) = bake(vec![polygon()], None);
        assert!(plain_extras.outline.is_empty());
        assert_eq!(plain.vertices.len(), mesh.vertices.len(), "the outline adds no vertex");
        for &index in &extras.outline {
            let v = mesh.vertices[index as usize];
            assert_eq!(v.is_stroke, 0.0, "an outline index names a FILL vertex");
            assert_eq!([v.normal[0], v.normal[1], v.gap_half_px, v.gap_half_px_to], [1.0, 0.0, 0.0, 1.0]);
        }
    }

    /// Units group into chunks under the vertex cap, every triangle lands
    /// in exactly one chunk, and every chunk-local index fits u16.
    #[test]
    fn chunks_partition_the_mesh_with_u16_local_indices() {
        let squares: Vec<_> = (0..50)
            .map(|i| {
                geo_types::Polygon::new(ring(&[(i * 10, 0), (i * 10 + 5, 0), (i * 10 + 5, 5), (i * 10, 5)]), vec![])
            })
            .collect();
        let (mesh, extras) = bake(squares, Some(RED));
        assert_eq!(extras.cuts.len(), 50, "one unit per polygon");
        let chunks = plan_overlay_chunks(&mesh, &extras, 12, u64::MAX).unwrap();
        assert!(chunks.len() > 1);
        assert_eq!(chunks.iter().map(|c| c.indices.len()).sum::<usize>(), mesh.indices.len());
        assert_eq!(chunks.iter().map(|c| c.outline.len()).sum::<usize>(), extras.outline.len());
        for chunk in &chunks {
            assert!(chunk.is_u16() && chunk.vertices.len() <= 12);
            for &i in mesh.indices[chunk.indices.clone()].iter().chain(&extras.outline[chunk.outline.clone()]) {
                assert!(chunk.vertices.contains(&(i as usize)), "a triangle crossed a chunk boundary");
            }
        }
        // The device limit caps a chunk too, and a unit alone over it is
        // an error rather than a silent blank.
        let tiny = plan_overlay_chunks(&mesh, &extras, MAX_SEGMENT_VERTICES, 4 * std::mem::size_of::<Vertex>() as u64).unwrap();
        assert_eq!(tiny.len(), 50);
        assert!(plan_overlay_chunks(&mesh, &extras, MAX_SEGMENT_VERTICES, std::mem::size_of::<Vertex>() as u64).is_err());
    }
}

