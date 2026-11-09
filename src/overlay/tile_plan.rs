//! A tile cut -> an ordinary tile-local `PaintPlan`, so overlay tiles go
//! through the basemap's own tessellator, subdivision, matrix and globe
//! path (`tessellate_*(.., MVT_EXTENT, COORD_SCALE, ..)`).
//!
//! Geometry stays in MVT units (`i32`, as a decoded basemap tile's does);
//! the tessellator multiplies by `COORD_SCALE` on the way into lyon, exactly
//! as it does for the basemap.
//!
//! The fill outline is A2's: `tessellate_overlay_plan_reusing` records a
//! line-list index over each fill's own vertices, so a tiled source's
//! hairline is one more index buffer per chunk, not a second tessellation.

use crate::geo::TileId;
use crate::overlay::plan::{ClassPaint, FeatureClasses};
use crate::overlay::tile_index::{CutGeometry, TileCut, TileIndex};
use crate::render::subdivide::Subdivision;
use crate::style::{PaintOp, PaintPlan, Rgba};
use std::sync::Arc;

/// Below this many polygon and line vertices a source stays on the
/// whole-source path: one mesh, one draw, exactly as before phase B.
pub const TILED_SOURCE_MIN_POINTS: usize = 200_000;

/// Whether a source this size is tiled. `GPUI_MAP_OVERLAY_TILING=always`
/// or `never` overrides the threshold (tests and the A/B measurements).
///
/// A source with `line_metrics` (`SourceOptions::line_metrics`) is NEVER
/// tiled, whatever its size or the override: its line progress is
/// computed over whole LineStrings on the whole-source path (plan D3).
/// One over the threshold is refused at the push instead
/// (`overlay::check_declaration`).
pub fn should_tile(features: &[crate::overlay::OverlayFeature], line_metrics: bool) -> bool {
    if line_metrics {
        return false;
    }
    match std::env::var("GPUI_MAP_OVERLAY_TILING").as_deref() {
        Ok("always") => return true,
        Ok("never") => return false,
        _ => {}
    }
    tiling_points(features) >= TILED_SOURCE_MIN_POINTS
}

/// The polygon and line vertices the tiling threshold counts, stopping
/// once it reaches [`TILED_SOURCE_MIN_POINTS`] (so the answer is exact
/// below the threshold and "at least" above it).
pub fn tiling_points(features: &[crate::overlay::OverlayFeature]) -> usize {
    let mut points = 0usize;
    for f in features {
        points += match &f.geometry {
            crate::overlay::geojson::OverlayGeometry::Polygon(p) => p.iter().flatten().map(Vec::len).sum(),
            crate::overlay::geojson::OverlayGeometry::Line(l) => l.iter().map(Vec::len).sum(),
            _ => 0,
        };
        if points >= TILED_SOURCE_MIN_POINTS {
            break;
        }
    }
    points
}

/// `Subdivision::for_tile` with the pole caps off: an overlay polygon is
/// not the world's background, so it must not be extended to the poles.
pub fn overlay_tile_subdivision(tile: TileId) -> Subdivision {
    Subdivision { north_pole: false, south_pole: false, ..Subdivision::for_tile(tile) }
}

fn coord(p: [i32; 2]) -> geo_types::Coord<i32> {
    geo_types::coord! { x: p[0], y: p[1] }
}

enum Acc {
    Fill(Vec<geo_types::Polygon<i32>>),
    Line(Vec<geo_types::LineString<i32>>),
}

fn flush(id: u16, class: ClassPaint, acc: Acc, ops: &mut Vec<PaintOp>, outlines: &mut Vec<Option<Rgba>>, op_class: &mut Vec<u16>) {
    let before = ops.len();
    match (class, acc) {
        (ClassPaint::Fill { color, outline }, Acc::Fill(p)) if !p.is_empty() => {
            ops.push(PaintOp::Fill {
                color_from: color,
                color_to: color,
                geometry: Arc::new(geo_types::Geometry::MultiPolygon(geo_types::MultiPolygon(p))),
            });
            outlines.push(outline);
        }
        (ClassPaint::Line { color, width, dash }, Acc::Line(l)) if !l.is_empty() => {
            ops.push(crate::overlay::plan::line_op(
                geo_types::Geometry::MultiLineString(geo_types::MultiLineString(l)),
                color,
                width,
                dash,
            ));
            outlines.push(None);
        }
        _ => {}
    }
    if ops.len() > before {
        op_class.push(id);
    }
}

fn push(acc: &mut Acc, g: &CutGeometry) {
    match (acc, g) {
        (Acc::Fill(out), CutGeometry::Polygons(polys)) => {
            for rings in polys.iter().filter(|r| !r.is_empty()) {
                let ls = |r: &Vec<[i32; 2]>| geo_types::LineString(r.iter().copied().map(coord).collect());
                out.push(geo_types::Polygon::new(ls(&rings[0]), rings[1..].iter().map(ls).collect()));
            }
        }
        (Acc::Line(out), CutGeometry::Lines(lines)) => {
            out.extend(lines.iter().map(|l| geo_types::LineString(l.iter().copied().map(coord).collect())));
        }
        _ => {}
    }
}

fn empty_for(class: ClassPaint) -> Acc {
    match class {
        ClassPaint::Fill { .. } => Acc::Fill(Vec::new()),
        ClassPaint::Line { .. } => Acc::Line(Vec::new()),
    }
}

/// The plan, and per op the `fill-outline-color` its fill's hairline is
/// drawn in (`None` for a line op or a fill with no outline) -- the pair
/// `tessellate_overlay_plan_reusing` takes.
///
/// For a data-driven source, ops are grouped per class in first-seen
/// order, which is the draw order `build_plan_merged` already has.
/// Otherwise one op per run of consecutive features sharing a class, which
/// keeps painter's order exactly.
pub fn tile_plan(cut: &TileCut, classes: &FeatureClasses) -> (PaintPlan, Vec<Option<Rgba>>) {
    let (plan, outlines, _) = tile_plan_with_classes(cut, classes);
    (plan, outlines)
}

/// [`tile_plan`], plus each op's class id: the paint index a compact
/// vertex carries (`render::compact`), which means the same thing in every
/// tile of the source.
pub fn tile_plan_with_classes(cut: &TileCut, classes: &FeatureClasses) -> (PaintPlan, Vec<Option<Rgba>>, Vec<u16>) {
    let mut ops = Vec::new();
    let mut outlines = Vec::new();
    let mut op_class = Vec::new();
    if classes.data_driven {
        let mut order: Vec<u16> = Vec::new();
        let mut accs: std::collections::HashMap<u16, Acc> = std::collections::HashMap::new();
        for f in &cut.features {
            let class = classes.classes[usize::from(f.class)];
            let acc = accs.entry(f.class).or_insert_with(|| {
                order.push(f.class);
                empty_for(class)
            });
            push(acc, &f.geometry);
        }
        for c in order {
            let acc = accs.remove(&c).expect("every class in `order` has an accumulator");
            flush(c, classes.classes[usize::from(c)], acc, &mut ops, &mut outlines, &mut op_class);
        }
    } else {
        let mut run: Option<(u16, Acc)> = None;
        for f in &cut.features {
            if let Some((c, acc)) = run.take_if(|(c, _)| *c != f.class) {
                flush(c, classes.classes[usize::from(c)], acc, &mut ops, &mut outlines, &mut op_class);
            }
            let (_, acc) = run.get_or_insert_with(|| (f.class, empty_for(classes.classes[usize::from(f.class)])));
            push(acc, &f.geometry);
        }
        if let Some((c, acc)) = run {
            flush(c, classes.classes[usize::from(c)], acc, &mut ops, &mut outlines, &mut op_class);
        }
    }
    (PaintPlan { ops }, outlines, op_class)
}

/// [`tile_plan_with_classes`] with ONE OP PER FEATURE, in cut (painter's)
/// order, plus each op's row: what a compact tile bakes, so that a
/// feature's vertices are one contiguous range whatever the classes, and a
/// class change alters its vertices' paint index and nothing else (the
/// live fast path, [`FeatureSpans`]). `None` when a feature is a line
/// class: strokes keep the 88-byte vertex, and the grouped plan.
///
/// An overlay fill is tessellated one polygon at a time
/// (`append_fill_overlay`), so splitting an op per feature changes no
/// vertex, only where op boundaries fall; in a data-driven source it puts
/// the features back in their own order rather than grouped per class,
/// which is MapLibre's fill-bucket order.
pub(crate) fn tile_plan_per_feature(
    cut: &TileCut,
    classes: &FeatureClasses,
) -> Option<(PaintPlan, Vec<Option<Rgba>>, Vec<u16>, Vec<u32>)> {
    let mut ops = Vec::with_capacity(cut.features.len());
    let mut outlines = Vec::with_capacity(cut.features.len());
    let mut op_class = Vec::with_capacity(cut.features.len());
    let mut op_row = Vec::with_capacity(cut.features.len());
    for f in &cut.features {
        let class = classes.classes[usize::from(f.class)];
        if !matches!(class, ClassPaint::Fill { .. }) {
            return None;
        }
        let mut acc = empty_for(class);
        push(&mut acc, &f.geometry);
        let before = ops.len();
        flush(f.class, class, acc, &mut ops, &mut outlines, &mut op_class);
        if ops.len() > before {
            op_row.push(f.feature);
        }
    }
    Some((PaintPlan { ops }, outlines, op_class, op_row))
}

/// Each display tile of `cover` paired with the overlay tile drawn for it:
/// itself at or below `max_zoom`, its `max_zoom` ancestor above (drawn
/// overzoomed and clipped to the display tile's sub-box). Cover order.
pub(crate) fn wanted_overlay_tiles(cover: &[TileId], max_zoom: u8) -> Vec<(TileId, TileId)> {
    cover
        .iter()
        .map(|&t| {
            let d = t.z.saturating_sub(max_zoom);
            (t, TileId { z: t.z - d, x: t.x >> d, y: t.y >> d })
        })
        .collect()
}

/// One baked overlay tile: the triangulation and A2's outline/unit cuts.
pub(crate) struct OverlayTileMesh {
    pub mesh: crate::render::TileMesh,
    pub extras: crate::render::mesh::OverlayExtras,
}

/// The outline's level of detail, in screen pixels at the tile's own zoom:
/// a polygon whose exterior's bounding box is under this in both axes keeps
/// its fill and loses its hairline.
///
/// Why: at national zoom the block groups' hairlines are hundreds of
/// thousands of sub-pixel segments rasterised into the same pixels, the
/// whole of the 10-11 ms GPU spikes (the `us_block_groups` breakdown, "The
/// worst GPU frame"). MapLibre draws `fill-outline-color` as `gl.LINES`
/// over every ring its tile keeps (`draw_fill.ts`, `fill_bucket.ts`
/// `indexArray2`); what bounds its segment count is geojson-vt, which drops
/// a ring under one tolerance-pixel of area (0.375 px on a side at 512 px,
/// `tile.js` `addLine`) and simplifies the rest. This index drops those
/// rings the same way (`TileIndex::cut`), so every outline edge comes from
/// a surviving ring. A 2 px polygon's outline is a 2 px blob of outline
/// colour that covers its own fill; skipping it leaves the fill.
pub const OUTLINE_MIN_EXTENT_PX: f64 = 2.0;

#[cfg(test)]
thread_local! {
    /// Tests only: bake every outline on this thread, the control a pixel
    /// A/B of the level of detail compares against.
    pub(crate) static OUTLINE_LOD_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `GPUI_MAP_PROBE_OUTLINE_LOD=off` (probe builds only): bake every outline,
/// as if the level of detail above did not exist. A deliberate, realistic
/// regression -- more outline indices, slower bakes, a heavier GPU pass --
/// that `scripts/perf-gate.sh`'s self-test must catch. Read once.
#[cfg(feature = "probe")]
fn probe_outline_lod_off() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var("GPUI_MAP_PROBE_OUTLINE_LOD").is_ok_and(|v| v == "off"))
}

/// [`OUTLINE_MIN_EXTENT_PX`] in the tile's input units, for tile `tile` of
/// an index whose natural zoom is `max_zoom`: 0 (every outline kept) at
/// and above it, where the index cuts exactly and nothing is simplified.
pub(crate) fn outline_min_extent_units(tile: TileId, max_zoom: u8) -> i32 {
    #[cfg(test)]
    if OUTLINE_LOD_OFF.with(std::cell::Cell::get) {
        return 0;
    }
    #[cfg(feature = "probe")]
    if probe_outline_lod_off() {
        return 0;
    }
    if tile.z >= max_zoom {
        return 0;
    }
    let units_per_px = f64::from(crate::mvt::MVT_EXTENT) / f64::from(crate::render::WORLD_SIZE);
    (OUTLINE_MIN_EXTENT_PX * units_per_px).round() as i32
}

/// Cut, plan and tessellate one overlay tile on the 88-byte vertex. `None`
/// when nothing of the source reaches it (or the bake was cancelled).
///
/// `MapView` and `capture` bake through [`bake_overlay_tile_compact`],
/// which falls back to exactly this mesh; this stays as the reference its
/// tests compare against.
#[allow(dead_code)]
pub(crate) fn bake_overlay_tile(
    index: &TileIndex,
    classes: &FeatureClasses,
    tile: TileId,
    cancel: &dyn Fn() -> bool,
) -> Option<OverlayTileMesh> {
    let cut = index.cut(tile);
    if cut.features.is_empty() || cancel() {
        return None;
    }
    let (plan, outlines) = tile_plan(&cut, classes);
    drop(cut);
    if cancel() {
        return None;
    }
    let (mesh, _, extras) = crate::render::mesh::tessellate_overlay_plan_lod(
        &plan,
        &outlines,
        outline_min_extent_units(tile, index.max_zoom()),
        crate::mvt::MVT_EXTENT,
        crate::render::mesh::COORD_SCALE,
        overlay_tile_subdivision(tile),
        cancel,
        crate::render::TileMesh::default(),
    )
    .ok()?;
    if mesh.indices.is_empty() {
        return None;
    }
    Some(OverlayTileMesh { mesh, extras })
}

/// A tile baked onto the 8-byte vertex (`render::compact`): A3's chunks
/// (whole units, at most 65,536 vertices, u16 indices), each carrying its
/// fill triangles and A2's hairline line list over the same vertices.
pub(crate) struct CompactTile {
    pub chunks: Vec<crate::render::compact::CompactMesh>,
    pub has_outline: bool,
    /// Where each feature's vertices are: `None` for a tile baked from the
    /// grouped plan (see [`tile_plan_per_feature`]).
    pub spans: Option<FeatureSpans>,
}

/// Per feature of a compact tile, the range of its vertices -- what lets a
/// class-only live update rewrite those vertices' paint index in place
/// instead of re-baking (`MapView::apply_live_result`).
///
/// Compact: every feature's vertices are contiguous and the features are
/// in cut order, so one start per feature does (8 bytes a feature; a
/// national z5 tile of ~5,000 block groups holds ~40 KB).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FeatureSpans {
    /// Each feature's row, ascending (cut order is slot order).
    pub rows: Vec<u32>,
    /// `rows.len() + 1` global vertex indices: feature `k` owns
    /// `starts[k]..starts[k + 1]`.
    pub starts: Vec<u32>,
    /// Each feature's class as baked (or as last patched).
    pub classes: Vec<u16>,
    /// Each chunk's global vertex range `[start, end)`, ascending. A
    /// vertex no chunk holds (a unit with no triangle) is never drawn.
    pub chunks: Vec<[u32; 2]>,
    /// The (rare) vertices with non-zero flags, ascending: their flags.
    pub flags: Vec<(u32, u16)>,
}

impl FeatureSpans {
    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        4 * (self.rows.capacity() + self.starts.capacity() + 2 * self.chunks.capacity())
            + 2 * self.classes.capacity()
            + 8 * self.flags.capacity()
    }

    /// Feature `k`'s vertices as `(chunk, first vertex in the chunk,
    /// [paint, flags] per vertex)` runs, painted `class`: what a planar
    /// buffer's paint stream takes.
    pub fn paint_runs(&self, k: usize, class: u16) -> Vec<(usize, usize, Vec<[u16; 2]>)> {
        let (start, end) = (self.starts[k], self.starts[k + 1]);
        let mut out = Vec::new();
        let first = self.chunks.partition_point(|c| c[1] <= start);
        for (chunk, &[c0, c1]) in self.chunks.iter().enumerate().skip(first) {
            if c0 >= end {
                break;
            }
            let (a, b) = (start.max(c0), end.min(c1));
            if a >= b {
                continue;
            }
            let mut run: Vec<[u16; 2]> = vec![[class, 0]; (b - a) as usize];
            let first_flag = self.flags.partition_point(|&(v, _)| v < a);
            for &(v, f) in self.flags[first_flag..].iter().take_while(|&&(v, _)| v < b) {
                run[(v - a) as usize][1] = f;
            }
            out.push((chunk, (a - c0) as usize, run));
        }
        out
    }

    /// Renumbers `rows` after a row compaction (`live::remapped_row`).
    /// `false`, and unchanged, when a row was dropped: the tile shows a
    /// removed feature and is re-baking.
    pub fn remap_rows(&mut self, remap: &[u32]) -> bool {
        let mut rows = Vec::with_capacity(self.rows.len());
        for &r in &self.rows {
            match crate::overlay::live::remapped_row(remap, r as usize) {
                Some(n) => rows.push(n as u32),
                None => return false,
            }
        }
        self.rows = rows;
        true
    }

    /// Global vertices `a..b`'s `[paint, flags]`, from the classes and
    /// flags recorded: exactly what the bake wrote there, repainted.
    /// A vertex no feature owns keeps paint 0 (`compact_vertices`).
    pub fn paint_stream(&self, a: u32, b: u32) -> Vec<[u16; 2]> {
        let mut out = vec![[0u16, 0u16]; (b - a) as usize];
        let mut k = self.starts.partition_point(|&s| s <= a).saturating_sub(1);
        while k < self.rows.len() && self.starts[k] < b {
            let (s, e) = (self.starts[k].max(a), self.starts[k + 1].min(b));
            if s < e {
                out[(s - a) as usize..(e - a) as usize].fill([self.classes[k], 0]);
            }
            k += 1;
        }
        let first_flag = self.flags.partition_point(|&(v, _)| v < a);
        for &(v, f) in self.flags[first_flag..].iter().take_while(|&&(v, _)| v < b) {
            out[(v - a) as usize][1] = f;
        }
        out
    }

    /// Repaints, in `chunks`, every feature whose class under `of_feature`
    /// differs from the one baked: a landed tile brought up to classes
    /// newer than the index it was cut from. Returns the features repainted.
    pub fn retint(&mut self, chunks: &mut [crate::render::compact::CompactMesh], of_feature: &[u16]) -> usize {
        let mut n = 0;
        for k in 0..self.rows.len() {
            let Some(&class) = of_feature.get(self.rows[k] as usize) else { continue };
            if class == self.classes[k] || class == crate::overlay::tile_index::NO_CLASS {
                continue;
            }
            for (chunk, first, run) in self.paint_runs(k, class) {
                for (v, p) in chunks[chunk].vertices[first..first + run.len()].iter_mut().zip(&run) {
                    v.paint = p[0];
                }
            }
            self.classes[k] = class;
            n += 1;
        }
        n
    }
}

impl CompactTile {
    pub fn vertices(&self) -> usize {
        self.chunks.iter().map(|c| c.vertices.len()).sum()
    }
}

/// What a tile bake hands the upload: compact, or Task 3's 88-byte mesh
/// for a tile with strokes (wide lines keep the extruding vertex).
pub(crate) type BakedOverlayTile = Result<CompactTile, OverlayTileMesh>;

/// Whether tiles bake compact. `GPUI_MAP_OVERLAY_COMPACT=0` keeps every
/// tile on the 88-byte vertex (the A/B measurement).
pub(crate) fn overlay_compact_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GPUI_MAP_OVERLAY_COMPACT").as_deref() != Ok("0"))
}

/// Per class, the colour pair its FILL draws with: the source's fill paint
/// table. A line class gets its colour too, but a line never bakes compact.
pub fn fill_paint_entries(classes: &FeatureClasses) -> Vec<(Rgba, Rgba)> {
    classes
        .classes
        .iter()
        .map(|c| match *c {
            ClassPaint::Fill { color, .. } | ClassPaint::Line { color, .. } => (color, color),
        })
        .collect()
}

/// Per class, the colour its hairline draws with: `fill-outline-color`,
/// A2's rule (`classify` carries it on the fill class). Transparent for a
/// class without one -- which also has no outline indices, so is never
/// drawn with it.
pub fn outline_paint_entries(classes: &FeatureClasses) -> Vec<(Rgba, Rgba)> {
    let none = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    classes
        .classes
        .iter()
        .map(|c| match *c {
            ClassPaint::Fill { outline: Some(o), .. } => (o, o),
            _ => (none, none),
        })
        .collect()
}

/// Whether `new` can replace `old` by rewriting the paint tables alone
/// (`MapView::update_source_paint`'s table-only path): every feature in
/// the same class index, the same number of classes, and each class
/// changing nothing a baked tile holds -- its kind, whether it has a
/// hairline (a tile bakes outline indices only for classes that do), and
/// a line's width and dash. Colours are free to change: a compact vertex
/// carries only its class id.
///
/// `classify` numbers classes in first-seen order, so the same partition
/// of the features always comes back with the same indices, and comparing
/// `of_feature` is comparing the partition.
pub fn classes_differ_only_in_colour(old: &FeatureClasses, new: &FeatureClasses) -> bool {
    old.data_driven == new.data_driven
        && old.classes.len() == new.classes.len()
        && old.of_feature == new.of_feature
        && old.classes.iter().zip(&new.classes).all(|(a, b)| match (a, b) {
            (ClassPaint::Fill { outline: oa, .. }, ClassPaint::Fill { outline: ob, .. }) => oa.is_some() == ob.is_some(),
            (ClassPaint::Line { width: wa, dash: da, .. }, ClassPaint::Line { width: wb, dash: db, .. }) => {
                wa == wb && da == db
            }
            _ => false,
        })
}

/// [`bake_overlay_tile`], converted onto the compact vertex. `Err` keeps
/// the 88-byte mesh: the tile has strokes, or a coordinate out of `i16`.
///
/// `per_feature`: whether the source takes live updates
/// (`OverlayHost::live` holds it). Such a tile bakes one op per feature
/// and carries [`FeatureSpans`], so a class-only diff repaints it in
/// place. A static source keeps one op per class and no spans: on a dense
/// tile the per-feature plan costs ~0.6 ms more (LA z11, 1,703 features in
/// 217 classes), paid for a repaint that never comes. The first live diff
/// re-bakes a spanless tile it recolours (`MapView::apply_live_result`).
pub(crate) fn bake_overlay_tile_compact(
    index: &TileIndex,
    classes: &FeatureClasses,
    tile: TileId,
    per_feature: bool,
    cancel: &dyn Fn() -> bool,
) -> Option<BakedOverlayTile> {
    let cut = index.cut(tile);
    if cut.features.is_empty() {
        return None;
    }
    bake_cut_compact(&cut, classes, tile, index.max_zoom(), per_feature, cancel)
}

/// [`bake_overlay_tile_compact`] from a cut already made.
pub(crate) fn bake_cut_compact(
    cut: &TileCut,
    classes: &FeatureClasses,
    tile: TileId,
    max_zoom: u8,
    per_feature: bool,
    cancel: &dyn Fn() -> bool,
) -> Option<BakedOverlayTile> {
    let per_feature =
        if per_feature && overlay_compact_enabled() { tile_plan_per_feature(cut, classes) } else { None };
    let (plan, outlines, op_class, op_row) = match per_feature {
        Some((plan, outlines, op_class, op_row)) => (plan, outlines, op_class, Some(op_row)),
        None => {
            let (plan, outlines, op_class) = tile_plan_with_classes(cut, classes);
            (plan, outlines, op_class, None)
        }
    };
    let (mesh, spans, extras) = crate::render::mesh::tessellate_overlay_plan_lod(
        &plan,
        &outlines,
        outline_min_extent_units(tile, max_zoom),
        crate::mvt::MVT_EXTENT,
        crate::render::mesh::COORD_SCALE,
        overlay_tile_subdivision(tile),
        cancel,
        crate::render::TileMesh::default(),
    )
    .ok()?;
    drop(plan);
    if mesh.indices.is_empty() {
        return None;
    }
    let legacy = |mesh, extras| Some(Err(OverlayTileMesh { mesh, extras }));
    if !overlay_compact_enabled() {
        return legacy(mesh, extras);
    }
    let Ok(vertices) = crate::render::compact::compact_vertices(&mesh, &spans, &op_class) else {
        return legacy(mesh, extras);
    };
    // 8-byte vertices: no chunk comes near any device's buffer limit, so
    // the only split is u16's 65,536 vertices.
    let Ok(plans) = crate::render::mesh::plan_overlay_chunks(
        &mesh,
        &extras,
        crate::render::mesh::MAX_SEGMENT_VERTICES,
        u64::MAX,
    ) else {
        return legacy(mesh, extras);
    };
    let chunk_ranges: Vec<[u32; 2]> = plans.iter().map(|p| [p.vertices.start as u32, p.vertices.end as u32]).collect();
    let chunks: Vec<crate::render::compact::CompactMesh> = plans
        .into_iter()
        .map(|p| {
            let n = p.vertices.len();
            crate::render::compact::CompactMesh {
                vertices: vertices[p.vertices.clone()].to_vec(),
                fill: crate::render::compact::IndexList::rebased(&mesh.indices[p.indices], p.vertices.start, n),
                outline: crate::render::compact::IndexList::rebased(&extras.outline[p.outline], p.vertices.start, n),
            }
        })
        .collect();
    let feature_spans = op_row.and_then(|rows| feature_spans(&rows, &op_class, &spans, &chunk_ranges, &vertices));
    Some(Ok(CompactTile { has_outline: chunks.iter().any(|c| !c.outline.is_empty()), chunks, spans: feature_spans }))
}

/// Two changed features closer than this many vertices are rewritten as
/// one range (the unchanged ones between re-derived from their spans): a
/// `write_buffer` costs a staging allocation and a copy command, so a
/// thousand scattered features as a thousand writes took 5-12 ms of the UI
/// thread; merged, a national tile is a handful of writes.
const REPAINT_MERGE_GAP: u32 = 1_024;

/// Rewrites, in a compact tile's planar paint streams, the paint index of
/// every `repaint` row (ascending) the tile holds, recording the new
/// classes in `spans`. `Some(features repainted)`; `None` when a chunk is
/// not planar (nothing is written: checked first).
pub(crate) fn repaint_tile(
    pipeline: &crate::render::TilePipeline,
    ctx: &crate::render::GpuContext,
    chunks: &[crate::render::CompactBuffers],
    spans: &mut FeatureSpans,
    repaint: &[(u32, u16, [u32; 4])],
) -> Option<usize> {
    if chunks.len() != spans.chunks.len() || !chunks.iter().all(crate::render::CompactBuffers::is_planar) {
        return None;
    }
    let mut changed: Vec<usize> = Vec::new();
    let (mut i, mut k) = (0usize, 0usize);
    while i < repaint.len() && k < spans.rows.len() {
        let (row, class, _) = repaint[i];
        match row.cmp(&spans.rows[k]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => k += 1,
            std::cmp::Ordering::Equal => {
                if spans.classes[k] != class {
                    spans.classes[k] = class;
                    changed.push(k);
                }
                i += 1;
                k += 1;
            }
        }
    }
    // Vertex ranges, ascending (features are in vertex order), merged.
    let mut ranges: Vec<[u32; 2]> = Vec::new();
    for &k in &changed {
        let (a, b) = (spans.starts[k], spans.starts[k + 1]);
        match ranges.last_mut() {
            Some(last) if a <= last[1] + REPAINT_MERGE_GAP => last[1] = b,
            _ => ranges.push([a, b]),
        }
    }
    for [a, b] in ranges {
        for (chunk, &[c0, c1]) in spans.chunks.iter().enumerate() {
            let (lo, hi) = (a.max(c0), b.min(c1));
            if lo < hi {
                let stream = spans.paint_stream(lo, hi);
                pipeline.write_compact_paint(ctx, &chunks[chunk], (lo - c0) as usize, &stream);
            }
        }
    }
    Some(changed.len())
}

/// A per-feature bake's [`FeatureSpans`]: `None` if a feature's vertices
/// are not one range in order (never, for [`tile_plan_per_feature`]).
fn feature_spans(
    rows: &[u32],
    op_class: &[u16],
    spans: &[crate::render::mesh::OpSpan],
    chunks: &[[u32; 2]],
    vertices: &[crate::render::compact::CompactVertex],
) -> Option<FeatureSpans> {
    let mut out = FeatureSpans {
        rows: Vec::with_capacity(rows.len()),
        starts: Vec::with_capacity(rows.len() + 1),
        classes: Vec::with_capacity(rows.len()),
        chunks: chunks.to_vec(),
        flags: vertices
            .iter()
            .enumerate()
            .filter(|(_, v)| v.flags != 0)
            .map(|(i, v)| (i as u32, v.flags))
            .collect(),
    };
    let mut end = 0usize;
    for ((&row, &class), span) in rows.iter().zip(op_class).zip(spans) {
        if span.vertices.start != end || out.rows.last().is_some_and(|&r| r >= row) {
            return None;
        }
        out.rows.push(row);
        out.starts.push(span.vertices.start as u32);
        out.classes.push(class);
        end = span.vertices.end;
    }
    if end != vertices.len() {
        return None;
    }
    out.starts.push(end as u32);
    Some(out)
}

/// What `--measure` reports for one tiled source: classify + index build,
/// then every tile of `cover` baked as `MapView` bakes it (in parallel,
/// one thread per tile, as the tile permits allow).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CoverBakeMeasure {
    pub classes: usize,
    pub index_build: std::time::Duration,
    pub index_bytes: usize,
    pub cover_bake_wall: std::time::Duration,
    pub cover_bake_max_tile: std::time::Duration,
    pub tiles_baked: usize,
    pub vertices: usize,
    pub indices: usize,
    pub outline_indices: usize,
    /// Tiles that baked onto the 8-byte vertex.
    pub compact_tiles: usize,
    /// What the baked tiles occupy on the GPU (vertices + fill + outline
    /// indices, unpadded), on whichever vertex each baked to.
    pub device_bytes: usize,
    /// The same tiles at Task 3's 88-byte vertex and u32 indices.
    pub legacy_device_bytes: usize,
    /// Wanted tiles whose cut HAS features but whose bake produced nothing:
    /// the bake half of a hole, the half a headless run can see. Should be
    /// 0; a tile whose every polygon degenerates at its zoom would count.
    pub unbaked_with_content: usize,
    /// Tiles the cover wanted (display tiles plus the coarse stand-in
    /// cover), after dedup.
    pub wanted_tiles: usize,
    /// Heap bytes of the compact tiles' [`FeatureSpans`] (the class-only
    /// live fast path's bookkeeping), and the features they cover.
    pub span_bytes: usize,
    pub span_features: usize,
}

/// See [`CoverBakeMeasure`]. `None` for a missing source or one whose
/// paints cannot be classed.
#[doc(hidden)]
pub fn measure_cover_bake(
    store: &crate::overlay::OverlayStore,
    name: &str,
    cover: &[TileId],
    display_zoom: u8,
) -> Option<CoverBakeMeasure> {
    let snapshot = store.source_snapshot(name)?;
    let bracket = crate::overlay::plan::zoom_dependence(&snapshot.features)
        .unwrap_or(crate::overlay::plan::BakeBracket::Bucket(display_zoom));
    let started = crate::timing::Instant::now();
    let classes = crate::overlay::plan::classify(&snapshot.features, &snapshot.properties, bracket, &snapshot.paint)?;
    // After adoption (Task 7) the index is the only geometry copy: re-class
    // it, as `MapView` does for a paint update, rather than build from the
    // `Indexed` placeholders.
    let index = match &snapshot.tile_index {
        Some(adopted) => adopted.reclassified(&classes.of_feature),
        None => TileIndex::build(
            &snapshot.features,
            &classes.of_feature,
            store.source_options(name).unwrap_or_default().tile_index_options(),
            &|| false,
        )?,
    };
    let index_build = started.elapsed();
    let mut tiles: Vec<TileId> = wanted_overlay_tiles(cover, index.max_zoom()).into_iter().map(|(_, t)| t).collect();
    tiles.sort_by_key(|t| (t.z, t.x, t.y));
    tiles.dedup();
    let started = crate::timing::Instant::now();
    let results: Vec<(std::time::Duration, Option<BakedOverlayTile>)> = std::thread::scope(|s| {
        let handles: Vec<_> = tiles
            .iter()
            .map(|&tile| {
                let (index, classes) = (&index, &classes);
                s.spawn(move || {
                    let t = crate::timing::Instant::now();
                    let baked = bake_overlay_tile_compact(index, classes, tile, false, &|| false);
                    (t.elapsed(), baked)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("a bake panicked")).collect()
    });
    let cover_bake_wall = started.elapsed();
    let mut m = CoverBakeMeasure {
        classes: classes.classes.len(),
        index_build,
        index_bytes: index.bytes(),
        cover_bake_wall,
        ..Default::default()
    };
    for (took, baked) in &results {
        m.cover_bake_max_tile = m.cover_bake_max_tile.max(*took);
        match baked {
            Some(Ok(c)) => {
                m.tiles_baked += 1;
                m.compact_tiles += 1;
                let (v, i, o) = c.chunks.iter().fold((0, 0, 0), |(v, i, o), k| {
                    (v + k.vertices.len(), i + k.fill.len(), o + k.outline.len())
                });
                m.vertices += v;
                m.indices += i;
                m.outline_indices += o;
                m.device_bytes += c.chunks.iter().map(crate::render::compact::CompactMesh::byte_len).sum::<usize>();
                m.legacy_device_bytes += v * std::mem::size_of::<crate::render::Vertex>() + (i + o) * 4;
                if let Some(spans) = &c.spans {
                    m.span_bytes += spans.bytes();
                    m.span_features += spans.rows.len();
                }
            }
            Some(Err(b)) => {
                m.tiles_baked += 1;
                m.vertices += b.mesh.vertices.len();
                m.indices += b.mesh.indices.len();
                m.outline_indices += b.extras.outline.len();
                let bytes = b.mesh.vertices.len() * std::mem::size_of::<crate::render::Vertex>()
                    + (b.mesh.indices.len() + b.extras.outline.len()) * 4;
                m.device_bytes += bytes;
                m.legacy_device_bytes += bytes;
            }
            None => {}
        }
    }
    m.wanted_tiles = tiles.len();
    m.unbaked_with_content = tiles
        .iter()
        .zip(&results)
        .filter(|(tile, (_, baked))| baked.is_none() && !index.cut(**tile).features.is_empty())
        .count();
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::plan::{ClassPaint, FeatureClasses};
    use crate::overlay::tile_index::{CutFeature, CutGeometry, TileCut};
    use crate::style::{PaintOp, Rgba};

    fn sq(x: i32) -> Vec<Vec<Vec<[i32; 2]>>> {
        vec![vec![vec![[x, 0], [x + 10, 0], [x + 10, 10], [x, 10], [x, 0]]]]
    }
    fn red() -> Rgba {
        Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }
    }
    fn blue() -> Rgba {
        Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }
    }
    fn fill(color: Rgba) -> ClassPaint {
        ClassPaint::Fill { color, outline: None }
    }

    #[test]
    fn coordinates_stay_in_mvt_units_and_reach_lyon_scaled() {
        let cut = TileCut {
            tile: crate::geo::TileId { z: 3, x: 1, y: 2 },
            points: 5,
            features: vec![CutFeature { feature: 0, class: 0, geometry: CutGeometry::Polygons(sq(-5)) }],
        };
        let classes = FeatureClasses { classes: vec![fill(red())], of_feature: vec![0], data_driven: false };
        let (plan, outlines) = tile_plan(&cut, &classes);
        assert_eq!(outlines, vec![None]);
        let PaintOp::Fill { geometry, .. } = &plan.ops[0] else { panic!() };
        let geo_types::Geometry::MultiPolygon(mp) = &**geometry else { panic!() };
        // MVT units, as a decoded basemap tile's plan holds them ...
        assert_eq!(mp.0[0].exterior().0[0], geo_types::coord! { x: -5, y: 0 });
        // ... and the tessellator scales them by COORD_SCALE, as it does
        // for the basemap.
        let (mesh, _) = crate::render::tessellate_plan_with_ranges(
            &plan,
            crate::mvt::MVT_EXTENT,
            crate::render::mesh::COORD_SCALE,
            Subdivision::NONE,
        );
        let min_x = mesh.vertices.iter().map(|v| v.position[0]).fold(f32::INFINITY, f32::min);
        assert_eq!(min_x, -5000.0);
    }

    #[test]
    fn non_data_driven_sources_keep_painters_order_by_runs() {
        let f = |i, c| CutFeature { feature: i, class: c, geometry: CutGeometry::Polygons(sq(i as i32 * 20)) };
        let cut = TileCut {
            tile: crate::geo::TileId { z: 0, x: 0, y: 0 },
            points: 15,
            features: vec![f(0, 0), f(1, 1), f(2, 0)],
        };
        let classes =
            FeatureClasses { classes: vec![fill(red()), fill(blue())], of_feature: vec![0, 1, 0], data_driven: false };
        assert_eq!(tile_plan(&cut, &classes).0.ops.len(), 3, "red, blue, red: three runs");
        let dd = FeatureClasses { data_driven: true, ..classes };
        assert_eq!(tile_plan(&cut, &dd).0.ops.len(), 2, "data-driven: grouped by class, as build_plan_merged");
    }

    #[test]
    fn a_fill_outline_rides_along_as_the_ops_outline() {
        let cut = TileCut {
            tile: crate::geo::TileId { z: 0, x: 0, y: 0 },
            points: 5,
            features: vec![CutFeature { feature: 0, class: 0, geometry: CutGeometry::Polygons(sq(0)) }],
        };
        let classes = FeatureClasses {
            classes: vec![ClassPaint::Fill { color: red(), outline: Some(blue()) }],
            of_feature: vec![0],
            data_driven: false,
        };
        assert_eq!(tile_plan(&cut, &classes).1, vec![Some(blue())]);
    }

    #[test]
    fn overlay_tiles_are_subdivided_like_basemap_tiles_without_pole_caps() {
        let t = crate::geo::TileId { z: 2, x: 1, y: 0 };
        let s = overlay_tile_subdivision(t);
        assert_eq!(s.granularity, Subdivision::for_tile(t).granularity);
        assert!(!s.north_pole && !s.south_pole);
    }

    #[test]
    fn a_line_metrics_source_is_never_tiled() {
        // 300,000 points: over the threshold on size alone.
        let line: Vec<(f64, f64)> = (0..300_000).map(|i| (f64::from(i) * 1e-4, 0.0)).collect();
        let features = vec![crate::overlay::OverlayFeature {
            index: 0,
            id: None,
            geometry: crate::overlay::geojson::OverlayGeometry::Line(vec![line.clone()]),
            paint: crate::overlay::FeaturePaint::default(),
        }];
        if std::env::var("GPUI_MAP_OVERLAY_TILING").is_err() {
            assert!(should_tile(&features, false), "without line metrics it tiles on size");
        }
        assert!(!should_tile(&features, true), "with line metrics it never tiles");
        assert!(tiling_points(&features) >= TILED_SOURCE_MIN_POINTS);

        // And the store refuses the combination (plan D3), both ways round,
        // changing nothing.
        let coords: Vec<String> = line.iter().map(|(x, y)| format!("[{x},{y}]")).collect();
        let json = format!(
            r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","properties":{{}},"geometry":{{"type":"LineString","coordinates":[{}]}}}}]}}"#,
            coords.join(",")
        );
        let mut store = crate::overlay::OverlayStore::new();
        store.declare_source_line_metrics("route", true).unwrap();
        let error = store.set_source("route", &json, 3).unwrap_err();
        assert!(error.to_string().contains("lineMetrics"), "{error}");
        assert_eq!(store.source_snapshot("route").map(|s| s.features.len()), Some(0), "nothing installed");
        store.declare_source_line_metrics("route", false).unwrap();
        store.set_source("route", &json, 3).unwrap();
        assert!(store.declare_source_line_metrics("route", true).is_err(), "refused on a source already too big");
        assert!(!store.source_options("route").unwrap().line_metrics, "and nothing changed");
    }

    #[test]
    fn small_sources_stay_on_the_whole_source_path() {
        let json = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[0,0],[1,1]]}}]}"#;
        let mut store = crate::overlay::OverlayStore::new();
        store.replace(json, 3).unwrap();
        let snap = store.source_snapshot(crate::overlay::DEFAULT_SOURCE).unwrap();
        assert!(!should_tile(&snap.features, false));
        assert!(!snap.tiled);
    }

    /// Triangle area of a mesh, in lyon units squared.
    fn mesh_area(mesh: &crate::render::TileMesh) -> f64 {
        mesh.indices
            .chunks(3)
            .map(|t| {
                let p = |i: u32| {
                    let v = mesh.vertices[i as usize].position;
                    (f64::from(v[0]), f64::from(v[1]))
                };
                let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
                ((b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1)).abs() * 0.5
            })
            .sum()
    }

    /// The tile index does NOT rewind rings (geojson-vt's `tile.js` does),
    /// so a cut ring arrives in whatever winding the source had. The
    /// tessellator forces exterior-positive / hole-negative per ring, so
    /// all four combinations of the two windings must fill the same area,
    /// with the hole left empty.
    #[test]
    fn either_winding_of_either_ring_fills_the_same_area_with_the_hole_empty() {
        let outer_ccw = vec![[0, 0], [100, 0], [100, 100], [0, 100], [0, 0]];
        let hole_ccw = vec![[25, 25], [75, 25], [75, 75], [25, 75], [25, 25]];
        let rev = |r: &Vec<[i32; 2]>| r.iter().rev().copied().collect::<Vec<_>>();
        let classes = FeatureClasses { classes: vec![fill(red())], of_feature: vec![0], data_driven: false };
        let mut areas = Vec::new();
        for outer in [outer_ccw.clone(), rev(&outer_ccw)] {
            for hole in [hole_ccw.clone(), rev(&hole_ccw)] {
                let cut = TileCut {
                    tile: crate::geo::TileId { z: 5, x: 3, y: 3 },
                    points: 10,
                    features: vec![CutFeature {
                        feature: 0,
                        class: 0,
                        geometry: CutGeometry::Polygons(vec![vec![outer.clone(), hole]]),
                    }],
                };
                let (plan, outlines) = tile_plan(&cut, &classes);
                let (mesh, _, _) = crate::render::mesh::tessellate_overlay_plan_reusing(
                    &plan,
                    &outlines,
                    crate::mvt::MVT_EXTENT,
                    crate::render::mesh::COORD_SCALE,
                    Subdivision::NONE,
                    &|| false,
                    crate::render::TileMesh::default(),
                )
                .unwrap();
                areas.push(mesh_area(&mesh));
            }
        }
        let expect = (100.0 * 100.0 - 50.0 * 50.0) * 1000.0 * 1000.0;
        for a in areas {
            assert!((a - expect).abs() < expect * 1e-6, "{a} vs {expect}: the hole painted, or the fill lost area");
        }
    }

    /// One polygon whose ring has 200,001 vertices (a circle), and one
    /// `Point` feature with `circle-radius` 4.
    pub(crate) fn big_polygon_and_one_point_json() -> String {
        let n = 200_000;
        let mut ring: Vec<String> = (0..n)
            .map(|i| {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                format!("[{},{}]", -98.0 + 5.0 * a.cos(), 39.0 + 5.0 * a.sin())
            })
            .collect();
        ring.push(ring[0].clone());
        format!(
            r#"{{"type":"FeatureCollection","features":[
                {{"type":"Feature","properties":{{}},"geometry":{{"type":"Polygon","coordinates":[[{}]]}}}},
                {{"type":"Feature","properties":{{"circle-radius":4}},"geometry":{{"type":"Point","coordinates":[-98,39]}}}}]}}"#,
            ring.join(",")
        )
    }

    #[test]
    fn a_tiled_source_still_draws_its_points() {
        let mut store = crate::overlay::OverlayStore::new();
        store.set_source("big", &big_polygon_and_one_point_json(), 3).unwrap();
        let snap = store.source_snapshot("big").unwrap();
        assert!(snap.tiled || std::env::var("GPUI_MAP_OVERLAY_TILING").as_deref() == Ok("never"));
        if !snap.tiled {
            return;
        }
        let plan = snap.plan_at(crate::overlay::plan::BakeBracket::Bucket(3)).unwrap();
        assert_eq!(plan.plan.ops.len(), 1);
        assert!(matches!(plan.plan.ops[0], PaintOp::Line { .. }), "the dot is a round-capped line op");
        assert_eq!(plan.feature_ops.len(), 2, "one entry per input feature");
        assert!(plan.feature_ops[0].is_empty(), "the polygon is drawn per tile, not here");
    }

    #[test]
    fn a_source_with_too_many_classes_falls_back_to_the_whole_source_path() {
        let mut store = crate::overlay::OverlayStore::new();
        store.set_source("big", &big_polygon_and_one_point_json(), 3).unwrap();
        if !store.source_snapshot("big").unwrap().tiled {
            return;
        }
        let before = store.source_generation("big").unwrap();
        assert!(store.force_untiled("big", 3));
        let snap = store.source_snapshot("big").unwrap();
        assert!(!snap.tiled);
        assert!(snap.generation > before);
        assert_eq!(store.source_plan("big").unwrap().plan.ops.len(), 2, "the polygon is back in the plan");
    }

    /// Two fill classes (the second with a hairline), 40 squares of 300
    /// units spread over the tile and into its buffer.
    fn two_class_cut(tile: TileId) -> (TileCut, FeatureClasses) {
        let square = |x: i32, y: i32| vec![vec![vec![[x, y], [x + 300, y], [x + 300, y + 300], [x, y + 300], [x, y]]]];
        let features = (0..40)
            .map(|i| CutFeature {
                feature: i,
                class: (i % 2) as u16,
                geometry: CutGeometry::Polygons(square(-600 + (i as i32 % 8) * 650, -600 + (i as i32 / 8) * 1100)),
            })
            .collect();
        let classes = FeatureClasses {
            classes: vec![fill(red()), ClassPaint::Fill { color: blue(), outline: Some(red()) }],
            of_feature: (0..40).map(|i| (i % 2) as u16).collect(),
            data_driven: true,
        };
        (TileCut { tile, points: 200, features }, classes)
    }

    fn outline_of(c: &crate::render::compact::CompactMesh) -> Vec<usize> {
        match &c.outline {
            crate::render::compact::IndexList::U16(v) => v.iter().map(|&i| usize::from(i)).collect(),
            crate::render::compact::IndexList::U32(v) => v.iter().map(|&i| i as usize).collect(),
        }
    }

    #[test]
    fn a_fill_only_tile_bakes_compact_with_class_ids_as_paint() {
        let tile = TileId { z: 2, x: 1, y: 1 };
        let (cut, classes) = two_class_cut(tile);
        let Some(Ok(t)) = bake_cut_compact(&cut, &classes, tile, 14, true, &|| false) else { panic!("compact") };
        let paints: std::collections::BTreeSet<u16> =
            t.chunks.iter().flat_map(|c| c.vertices.iter().map(|v| v.paint)).collect();
        assert_eq!(paints, [0u16, 1].into_iter().collect());
        assert!(t.chunks.iter().all(|c| std::mem::size_of_val(&c.vertices[..]) == c.vertices.len() * 8));
    }

    #[test]
    fn the_outline_indexes_the_fill_vertices_and_adds_none() {
        let tile = TileId { z: 2, x: 1, y: 1 };
        let (cut, classes) = two_class_cut(tile);
        let Some(Ok(t)) = bake_cut_compact(&cut, &classes, tile, 14, true, &|| false) else { panic!("compact") };
        assert!(t.has_outline);
        // The same tile on the 88-byte path: the outline adds no vertices.
        let (plan, outlines) = tile_plan(&cut, &classes);
        let (legacy, _, extras) = crate::render::mesh::tessellate_overlay_plan_reusing(
            &plan,
            &outlines,
            crate::mvt::MVT_EXTENT,
            crate::render::mesh::COORD_SCALE,
            overlay_tile_subdivision(tile),
            &|| false,
            crate::render::TileMesh::default(),
        )
        .unwrap();
        assert_eq!(t.vertices(), legacy.vertices.len(), "no extra vertices for the outline");
        let outline: usize = t.chunks.iter().map(|c| c.outline.len()).sum();
        assert_eq!(outline, extras.outline.len(), "every A2 outline index carried over");
        for c in &t.chunks {
            let idx = outline_of(c);
            assert!(idx.len().is_multiple_of(2), "a line list");
            assert!(idx.iter().all(|&i| i < c.vertices.len()), "every outline index points into its chunk's vertices");
            // Only the outlined class (1) carries hairline edges.
            assert!(idx.iter().all(|&i| c.vertices[i].paint == 1));
        }
    }

    /// The outline level of detail: under 2 px at the tile's zoom, a
    /// polygon keeps its fill and loses its hairline; at the index's
    /// natural zoom every hairline is kept.
    #[test]
    fn a_sub_two_pixel_polygon_keeps_its_fill_and_drops_its_outline_below_the_natural_zoom() {
        assert_eq!(outline_min_extent_units(TileId { z: 3, x: 0, y: 0 }, 14), 16, "2 px at 4096/512 units per px");
        assert_eq!(outline_min_extent_units(TileId { z: 14, x: 0, y: 0 }, 14), 0);
        assert_eq!(outline_min_extent_units(TileId { z: 16, x: 0, y: 0 }, 14), 0);
        let poly = |x: i32, w: i32, h: i32| vec![vec![vec![[x, 100], [x + w, 100], [x + w, 100 + h], [x, 100 + h], [x, 100]]]];
        let classes = FeatureClasses {
            classes: vec![ClassPaint::Fill { color: blue(), outline: Some(red()) }],
            of_feature: vec![0; 3],
            data_driven: true,
        };
        // 15 x 15 units (1.9 px): culled. 16 x 1 (a 2 px sliver): kept. 40 x 40: kept.
        let features = [poly(100, 15, 15), poly(300, 16, 1), poly(500, 40, 40)]
            .into_iter()
            .enumerate()
            .map(|(i, g)| CutFeature { feature: i as u32, class: 0, geometry: CutGeometry::Polygons(g) })
            .collect();
        let tile = TileId { z: 3, x: 0, y: 0 };
        let cut = TileCut { tile, points: 15, features };
        let bake = |max_zoom: u8| {
            let Some(Ok(t)) = bake_cut_compact(&cut, &classes, tile, max_zoom, true, &|| false) else { panic!("compact") };
            t
        };
        // The same z3 tile, of an index whose natural zoom is 14, and of one whose natural zoom is 3.
        let (low, natural) = (bake(14), bake(3));
        let fill = |t: &CompactTile| t.chunks.iter().map(|c| c.fill.len()).sum::<usize>();
        let outline = |t: &CompactTile| t.chunks.iter().map(|c| c.outline.len()).sum::<usize>();
        assert_eq!(fill(&low), fill(&natural), "the fill is never culled");
        assert_eq!(outline(&natural), 3 * 4 * 2, "every ring's four edges at the natural zoom");
        assert_eq!(outline(&low), 2 * 4 * 2, "only the 15-unit square lost its outline");
    }

    #[test]
    fn a_tile_with_wide_strokes_falls_back_to_the_legacy_mesh() {
        let tile = TileId { z: 1, x: 0, y: 0 };
        let cut = TileCut {
            tile,
            points: 3,
            features: vec![CutFeature {
                feature: 0,
                class: 0,
                geometry: CutGeometry::Lines(vec![vec![[100, 100], [2000, 900], [3000, 3000]]]),
            }],
        };
        let classes = FeatureClasses {
            classes: vec![ClassPaint::Line {
                color: red(),
                width: (4.0, 4.0),
                dash: crate::style::DashPattern { runs: [0.0; 4], count: 0 },
            }],
            of_feature: vec![0],
            data_driven: false,
        };
        assert!(matches!(bake_cut_compact(&cut, &classes, tile, 14, true, &|| false), Some(Err(_))));
    }

    /// A static source bakes one op per class and no spans (the plan before
    /// the live fast path); a live one bakes one op per feature, with spans.
    /// Both draw the same triangles.
    #[test]
    fn a_static_tile_groups_by_class_and_a_live_one_by_feature() {
        let tile = TileId { z: 2, x: 1, y: 1 };
        let (cut, classes) = two_class_cut(tile);
        let (grouped, _, op_class) = tile_plan_with_classes(&cut, &classes);
        assert_eq!(grouped.ops.len(), 2, "one op per class");
        assert_eq!(op_class, vec![0, 1]);
        let (per_feature, _, _, rows) = tile_plan_per_feature(&cut, &classes).unwrap();
        assert_eq!(per_feature.ops.len(), cut.features.len(), "one op per feature");
        assert_eq!(rows, (0..40).collect::<Vec<u32>>());

        let Some(Ok(stat)) = bake_cut_compact(&cut, &classes, tile, 14, false, &|| false) else { panic!("compact") };
        let Some(Ok(live)) = bake_cut_compact(&cut, &classes, tile, 14, true, &|| false) else { panic!("compact") };
        assert!(stat.spans.is_none(), "a static tile carries no spans");
        let spans = live.spans.as_ref().expect("a live tile carries spans");
        assert_eq!(spans.rows, (0..40).collect::<Vec<u32>>());
        assert_eq!(stat.vertices(), live.vertices());
        let count = |t: &CompactTile| {
            t.chunks.iter().fold((0, 0), |(f, o), c| (f + c.fill.len(), o + c.outline.len()))
        };
        assert_eq!(count(&stat), count(&live));
    }

    #[test]
    fn paint_entries_follow_class_ids() {
        let (_, classes) = two_class_cut(TileId { z: 0, x: 0, y: 0 });
        let e = fill_paint_entries(&classes);
        assert_eq!(e.len(), classes.classes.len());
        assert_eq!(e[1], (blue(), blue()));
        let o = outline_paint_entries(&classes);
        assert_eq!(o[1], (red(), red()));
        assert_eq!(o[0].0.a, 0.0, "no outline, transparent");
    }

    #[test]
    fn overzoom_maps_to_the_max_zoom_ancestor() {
        let cover = [TileId { z: 16, x: 18_000, y: 25_000 }, TileId { z: 16, x: 18_001, y: 25_000 }];
        let w = wanted_overlay_tiles(&cover, 14);
        assert_eq!(w.len(), 2);
        assert!(w.iter().all(|(_, o)| *o == TileId { z: 14, x: 4_500, y: 6_250 }));
        assert_eq!(w[0].0, cover[0]);
    }

    #[test]
    fn wanted_overlay_tiles_keep_the_cover_order_at_or_below_max_zoom() {
        let cover = [TileId { z: 3, x: 1, y: 2 }, TileId { z: 3, x: 2, y: 2 }];
        assert_eq!(wanted_overlay_tiles(&cover, 14), vec![(cover[0], cover[0]), (cover[1], cover[1])]);
    }
}
