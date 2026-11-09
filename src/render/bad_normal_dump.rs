//! Diagnostic dump for the "N% of a stroke op's normals are not unit
//! vectors" warning (`mesh::append_ops_with`), for a consumer report that
//! could not be reproduced synthetically (Singapore bus routes, most
//! readily on the TILED overlay path with
//! `GPUI_MAP_OVERLAY_TILING=always`).
//!
//! Set `GPUI_MAP_DUMP_BAD_NORMALS=<dir>` and every occurrence of the
//! warning writes one JSON file into `<dir>` with enough to reconstruct the
//! op offline: the source, whether the bake was tiled, the tile id, the
//! op's stroke parameters, its input points (in tessellator units, plus
//! lon/lat when a tile id makes that cheap), and the bad normals' indices,
//! values and positions.
//!
//! # Cost when unset
//!
//! [`dump_dir`] reads the env var exactly once (`OnceLock`) and every call
//! after that is a relaxed load of an already-resolved `Option`. The
//! call site in `mesh::append_ops_with` is already behind `share > 0.01` --
//! this adds one more branch on that same cold path, nothing on the hot one.
//!
//! # Why a thread-local context and not a threaded parameter
//!
//! The warning fires deep in `mesh::append_ops_with`, which knows an op's
//! geometry and paint but not which *source* or *tile* it came from --
//! that lives several calls up, at the overlay tile bake
//! (`overlay::host::TileBake::run`) and the whole-source bake
//! (`MapView::bake_overlay_source`). Both are exactly one call each, so
//! [`with_bake_context`] threads the identifying info down as a scoped
//! thread-local rather than adding a parameter to every tessellation
//! function between them (`tessellate_overlay_plan_lod`, `append_ops`,
//! `append_stroke`, ...) for the sake of a diagnostic nothing else needs.
//! A bake that does not set the context (a test, `capture`, the basemap's
//! own tiles) still gets a dump -- just with `source: "unknown"`.

use crate::geo::TileId;
use crate::style::PaintOp;
#[cfg(not(target_arch = "wasm32"))]
use crate::style::{DashPattern, LineCap, LineJoin};
use std::cell::RefCell;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;

/// Identifies the bake a `tessellate_*` call is running as part of, for the
/// dump alone -- nothing here affects tessellation.
#[derive(Clone, Debug)]
pub(crate) struct BakeContext {
    pub source: String,
    pub tiled: bool,
    pub tile: Option<TileId>,
    /// The whole-source plan's origin (`OverlayPlan::origin`), `None` for a
    /// tiled bake (whose geometry is tile-local, not origin-relative).
    pub origin: Option<(f64, f64)>,
}

thread_local! {
    static BAKE_CONTEXT: RefCell<Option<BakeContext>> = const { RefCell::new(None) };
}

/// Restores the previous context on drop, so a nested or re-entrant bake
/// (none exist today, but nothing here should assume it) cannot leak its
/// context into its caller's.
pub(crate) struct BakeContextGuard(Option<BakeContext>);

impl Drop for BakeContextGuard {
    fn drop(&mut self) {
        BAKE_CONTEXT.with(|c| *c.borrow_mut() = self.0.take());
    }
}

/// Sets the current thread's bake context for the life of the returned
/// guard. Call this around the one tessellation call a tile bake or a
/// whole-source bake makes.
#[must_use]
pub(crate) fn with_bake_context(ctx: BakeContext) -> BakeContextGuard {
    let previous = BAKE_CONTEXT.with(|c| c.borrow_mut().replace(ctx));
    BakeContextGuard(previous)
}

fn current_bake_context() -> Option<BakeContext> {
    BAKE_CONTEXT.with(|c| c.borrow().clone())
}

/// `GPUI_MAP_DUMP_BAD_NORMALS`, read once and cached -- an unset env var
/// costs one `OnceLock` load per bad-normal op forever after, not one
/// `env::var` syscall-equivalent per call.
#[cfg(not(target_arch = "wasm32"))]
fn dump_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("GPUI_MAP_DUMP_BAD_NORMALS").map(PathBuf::from)).as_ref()
}

/// Files written this process. Capped so a pathological run (or a source
/// that is bad on every tile) fills a directory instead of a disk.
#[cfg(not(target_arch = "wasm32"))]
static DUMP_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The most files [`maybe_dump_bad_normals`] will write in one process.
#[cfg(not(target_arch = "wasm32"))]
const MAX_DUMPS: usize = 50;

/// One bad normal: its index into the op's own vertex slice (not the whole
/// tile's), its value, and the position it sits at.
#[cfg(not(target_arch = "wasm32"))]
#[derive(serde::Serialize)]
struct BadNormal {
    index: usize,
    normal: [f32; 2],
    position: [f32; 2],
}

/// One input point, in the units the tessellator actually saw (post
/// `coord_scale`) and, when cheaply recoverable, its lon/lat.
#[cfg(not(target_arch = "wasm32"))]
#[derive(serde::Serialize)]
struct InputPoint {
    /// Raw op-geometry units (MVT/world `i32`, pre-`coord_scale`).
    raw: [i32; 2],
    /// `raw * coord_scale` -- what lyon's tessellator sees.
    lyon: [f32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lat: Option<f64>,
}

/// The subset of a stroke `PaintOp::Line`'s fields the dump needs, plus the
/// scale/extent/origin the tessellation ran under. `None` for anything but
/// a `Line` op -- the only kind that reaches `collapsed_normal_share`.
#[cfg(not(target_arch = "wasm32"))]
#[derive(serde::Serialize)]
struct StrokeOpDump {
    half_width_from_px: f32,
    half_width_to_px: f32,
    gap_half_from_px: f32,
    gap_half_to_px: f32,
    /// The half-width lyon's `StrokeTessellator` actually tessellates with
    /// (`mesh::STROKE_TESSELLATION_WIDTH` / 2) -- fixed, independent of the
    /// style's declared width; see `append_stroke`'s doc comment for why.
    tessellation_half_width: f32,
    cap: String,
    join: String,
    dash_runs: [f32; 4],
    coord_scale: f32,
    extent: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_world: Option<[f64; 2]>,
}

#[cfg(not(target_arch = "wasm32"))]
fn cap_name(cap: LineCap) -> String {
    format!("{cap:?}")
}

#[cfg(not(target_arch = "wasm32"))]
fn join_name(join: LineJoin) -> String {
    format!("{join:?}")
}

#[cfg(not(target_arch = "wasm32"))]
fn dash_runs(dash: DashPattern) -> [f32; 4] {
    if dash.count == 0 { [0.0; 4] } else { dash.runs }
}

/// Flattens a stroke op's geometry into the same points `append_stroke` fed
/// lyon: every coordinate of every `LineString`/`MultiLineString`, or every
/// `Point`/`MultiPoint` (a dot, drawn as a zero-length subpath -- see
/// `append_stroke`), in geometry order.
#[cfg(not(target_arch = "wasm32"))]
fn op_input_points(geometry: &geo_types::Geometry<i32>) -> Vec<(i32, i32)> {
    use geo_types::Geometry::*;
    match geometry {
        LineString(l) => l.0.iter().map(|c| (c.x, c.y)).collect(),
        MultiLineString(ml) => ml.iter().flat_map(|l| l.0.iter().map(|c| (c.x, c.y))).collect(),
        Point(p) => vec![(p.0.x, p.0.y)],
        MultiPoint(mp) => mp.iter().map(|p| (p.0.x, p.0.y)).collect(),
        _ => Vec::new(),
    }
}

/// Converts one raw input point to lon/lat, when a tile id is in scope: the
/// point is `extent`-relative MVT-tile-local, so `tile.x + raw/extent` is
/// the fractional global tile coordinate `geo::tile_xy_to_lon_lat` inverts.
#[cfg(not(target_arch = "wasm32"))]
fn point_lonlat(raw: (i32, i32), extent: u32, tile: Option<TileId>) -> (Option<f64>, Option<f64>) {
    let Some(tile) = tile else { return (None, None) };
    if extent == 0 {
        return (None, None);
    }
    let fx = f64::from(tile.x) + f64::from(raw.0) / f64::from(extent);
    let fy = f64::from(tile.y) + f64::from(raw.1) / f64::from(extent);
    let (lon, lat) = crate::geo::tile_xy_to_lon_lat(fx, fy, f64::from(tile.z));
    (Some(lon), Some(lat))
}

/// Called from `mesh::append_ops_with` exactly where it already logs the
/// warning, with the op that triggered it and its own slice of the mesh's
/// vertices (`buffers.vertices[span.vertices.clone()]`). Writes one JSON
/// file into `GPUI_MAP_DUMP_BAD_NORMALS` and does nothing else -- in
/// particular it never panics or changes what the caller does next, since a
/// diagnostic dump must not be able to make the warning it is investigating
/// worse.
#[cfg(target_arch = "wasm32")]
pub(crate) fn maybe_dump_bad_normals(
    _op: &PaintOp,
    _coord_scale: f32,
    _extent: u32,
    _op_vertices: &[crate::render::mesh::Vertex],
) {
    // No filesystem to dump into; the warning itself still prints.
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn maybe_dump_bad_normals(
    op: &PaintOp,
    coord_scale: f32,
    extent: u32,
    op_vertices: &[crate::render::mesh::Vertex],
) {
    let Some(dir) = dump_dir() else { return };
    // Reserve a slot before doing any work: two threads racing here get two
    // distinct slots (or both bail past `MAX_DUMPS`), never the same file.
    let slot = DUMP_COUNT.fetch_add(1, Ordering::Relaxed);
    if slot >= MAX_DUMPS {
        return;
    }

    let PaintOp::Line { width_from, width_to, gap_from, gap_to, cap, join, dash, geometry, .. } = op else {
        // Only a `Line` op reaches `collapsed_normal_share`'s caller; this
        // is defensive, not reachable today.
        return;
    };

    let ctx = current_bake_context();
    let source = ctx.as_ref().map_or_else(|| "unknown".to_string(), |c| c.source.clone());
    let tiled = ctx.as_ref().is_some_and(|c| c.tiled);
    let tile = ctx.as_ref().and_then(|c| c.tile);
    let origin = ctx.as_ref().and_then(|c| c.origin);

    let bad_normals: Vec<BadNormal> = op_vertices
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_stroke > 0.5)
        .filter_map(|(i, v)| {
            let len = (v.normal[0] * v.normal[0] + v.normal[1] * v.normal[1]).sqrt();
            (len < 0.9).then_some(BadNormal { index: i, normal: v.normal, position: v.position })
        })
        .collect();

    let raw_points = op_input_points(geometry);
    let input_points: Vec<InputPoint> = raw_points
        .iter()
        .map(|&(x, y)| {
            let (lon, lat) = point_lonlat((x, y), extent, tile);
            InputPoint { raw: [x, y], lyon: [x as f32 * coord_scale, y as f32 * coord_scale], lon, lat }
        })
        .collect();

    let stroke_op = StrokeOpDump {
        half_width_from_px: width_from / 2.0,
        half_width_to_px: width_to / 2.0,
        gap_half_from_px: gap_from / 2.0,
        gap_half_to_px: gap_to / 2.0,
        tessellation_half_width: crate::render::mesh::STROKE_TESSELLATION_WIDTH / 2.0,
        cap: cap_name(*cap),
        join: join_name(*join),
        dash_runs: dash_runs(*dash),
        coord_scale,
        extent,
        origin_world: origin.map(|(x, y)| [x, y]),
    };

    let total_stroke_vertices = op_vertices.iter().filter(|v| v.is_stroke > 0.5).count();
    let payload = serde_json::json!({
        "source": source,
        "tiled": tiled,
        "tile": tile.map(|t| serde_json::json!({"z": t.z, "x": t.x, "y": t.y})),
        "op": stroke_op,
        "input_points": input_points,
        "total_stroke_vertices": total_stroke_vertices,
        "bad_normal_count": bad_normals.len(),
        "bad_normals": bad_normals,
    });

    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let pid = std::process::id();
    let path = dir.join(format!("bad-normals-{pid}-{slot:03}.json"));
    if let Ok(text) = serde_json::to_string_pretty(&payload) {
        let _ = std::fs::write(path, text);
    }
}
