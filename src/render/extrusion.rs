//! `fill-extrusion` geometry: roofs and walls tessellated into a 12-byte
//! vertex, plus the per-feature table the shader reads height, base and
//! colour from (spec `docs/superpowers/specs/2026-09-29-3d-building-extrusion-design.md`
//! §3: §3.2 "Ours" is what this file implements, §3.3 the plan op it reads).
//!
//! No gpui, wgpu or DOM here: every shell shares this file.
//!
//! # What was read, and what differs
//!
//! MapLibre GL JS was read, not ported (BSD-3, `maplibre-gl-js` (github.com/maplibre/maplibre-gl-js)):
//!
//! - `src/data/bucket/fill_extrusion_bucket.ts`: the roof is the footprint
//!   triangulated with `normal = (0, 0, 1)`, `t = 1` (`processPolygon`), and
//!   every ring edge becomes a 4-vertex, 2-triangle quad sharing the edge's
//!   unit perpendicular (`_generateSideFaces`).
//! - `src/data/extent_bounds.ts`: `isBoundaryEdge` skips an axis-parallel
//!   edge outside `[0, EXTENT]`, the producer's buffer cut.
//! - MapLibre Native `src/mln/renderer/buckets/fill_extrusion_bucket.cpp`:
//!   the same quad, with no boundary-edge test in that checkout.
//!
//! Ours differs in three ways (spec §3.2, R3):
//!
//! 1. **Ring orientation is computed**, not taken from the producer: an
//!    exterior ring is traversed with a positive shoelace sum
//!    `Σ (x_i·y_{i+1} − x_{i+1}·y_i)` in tile coordinates (x right, y down),
//!    a hole with a negative one, so the edge normal `normalize(dy, −dx)`
//!    always points away from the solid.
//! 2. **Walls are clipped to the half-open tile square `[0, EXTENT)²`**: a
//!    segment is Liang–Barsky-clipped to the closed square, then dropped if
//!    it lies on `x = EXTENT` or `y = EXTENT`. Every real wall is drawn by
//!    exactly one tile, and the producer's buffer cut makes no wall at all.
//! 3. **Height, base and colour are not per vertex.** They sit in a
//!    per-chunk table of [`TABLE_TEXELS_PER_ROW`] `Rgba32Float` texels per
//!    feature, so the zoom pair costs nothing per vertex.
//!
//! The roof is the footprint CLIPPED to the tile
//! (`style::clip_fill_geometry_to_tile_extent`), triangulated by lyon the way
//! `mesh::append_fill` does, and then normalised to one winding: lyon does
//! not document its output winding, and the camera can see a roof from
//! below (spec §5.3), so back-face culling needs every roof triangle wound
//! the same way.
//!
//! # The triangle rule
//!
//! Every emitted triangle `(a, b, c)` has `(b − a) × (c − a)` pointing OUT of
//! the solid in (x, y-down, z-up): a roof triangle has a positive xy signed
//! area, and a wall quad on `p1 → p2` is `(p1 base, p2 base, p1 top)` and
//! `(p1 top, p2 base, p2 top)`. Which `wgpu::FrontFace` that is, Task 6's
//! GPU test decides.

use lyon::math::Point;
use lyon::path::polygon::Polygon as LyonPolygon;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, FillVertexConstructor, VertexBuffers,
};

use crate::render::compact::IndexList;
use crate::style::{ExtrusionRow, PaintOp, PaintPlan};

/// The MVT tile extent the walls are clipped to.
pub const EXTENT: i32 = 4096;
/// Texels per feature in the extrusion table: `color_from`, `color_to`,
/// `[base_from, height_from, base_to, height_to]`.
pub const TABLE_TEXELS_PER_ROW: usize = 3;
/// The extrusion table texture's width in texels.
pub const TABLE_WIDTH: usize = 512;
/// Rows one chunk's table holds: a vertex's `row` is a `u16`.
pub const MAX_ROWS_PER_CHUNK: usize = 65_536;
/// Extrusion vertices per tile before it counts as over budget (spec §5.2:
/// Shinjuku z14 plus about 15%).
pub const EXTRUSION_VERTEX_BUDGET: usize = 300_000;

/// One extrusion vertex, 12 bytes (spec §3.2).
///
/// - `pos`: MVT units, `0..=EXTENT` (roofs are clipped, walls too).
/// - `normal_top`: `Snorm8x4` `(nx, ny, nz, top)`, `top` 0 (base) or 127 (top).
/// - `row`: the feature's row in its chunk's table.
/// - `flags`: the building's [`tone_key`], the same on every vertex of it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ExtrusionVertex {
    pub pos: [i16; 2],
    pub normal_top: [i8; 4],
    pub row: u16,
    pub flags: u16,
}

/// The indices of one extrusion group (one source `fill-extrusion` layer)
/// within a chunk: what one draw call covers.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupSpan {
    pub group: u16,
    pub first_index: u32,
    pub index_count: u32,
}

/// Up to [`MAX_ROWS_PER_CHUNK`] features: one vertex/index buffer pair and
/// one table.
#[derive(Debug, Clone)]
pub struct ExtrusionChunk {
    pub vertices: Vec<ExtrusionVertex>,
    pub indices: IndexList,
    /// TABLE_TEXELS_PER_ROW texels per feature: color_from, color_to, [base_from, height_from, base_to, height_to].
    pub table: Vec<[f32; 4]>,
    pub spans: Vec<GroupSpan>,
    /// Wall quads emitted.
    pub walls: usize,
}

/// A tile's extrusion geometry: one chunk per [`MAX_ROWS_PER_CHUNK`]
/// features, none for a tile with no extrusion op.
#[derive(Debug, Clone, Default)]
pub struct ExtrusionMesh {
    /// The roofs as a [`HeightGrid`], for label occlusion: built with the
    /// mesh, on the tessellation worker, never on the paint thread. Empty
    /// (no cells) for a tile with no building.
    pub height_grid: HeightGrid,
    pub chunks: Vec<ExtrusionChunk>,
    /// Roof polygons lyon failed to tessellate, skipped (their walls still
    /// draw). Counted rather than logged here, so the caller can log once
    /// per tile as it does [`Self::over_budget`] (final-review M6): the
    /// same polygon fails on every rebake.
    pub roof_failures: usize,
}

impl ExtrusionMesh {
    /// Nothing to draw: no chunk, or no index in any chunk.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.iter().all(|c| c.indices.is_empty())
    }
    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.chunks.iter().map(|c| c.vertices.len()).sum()
    }
    #[must_use]
    pub fn index_count(&self) -> usize {
        self.chunks.iter().map(|c| c.indices.len()).sum()
    }
    #[must_use]
    pub fn wall_count(&self) -> usize {
        self.chunks.iter().map(|c| c.walls).sum()
    }
    /// Vertex + index + table bytes, what an upload costs.
    ///
    /// The table is counted as the texels it holds, not padded to whole
    /// [`TABLE_WIDTH`] rows.
    #[must_use]
    pub fn upload_bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|c| {
                c.vertices.len() * std::mem::size_of::<ExtrusionVertex>()
                    + c.indices.bytes().len()
                    + c.table.len() * std::mem::size_of::<[f32; 4]>()
            })
            .sum()
    }
    /// More vertices than [`EXTRUSION_VERTEX_BUDGET`]. Still drawn (spec §5.2).
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.vertex_count() > EXTRUSION_VERTEX_BUDGET
    }

    /// CPU bytes beyond the upload: the height grid.
    #[must_use]
    pub fn cpu_only_bytes(&self) -> usize {
        self.height_grid.cells.capacity() * std::mem::size_of::<f32>()
    }
}

/// Cells per side of a [`HeightGrid`]: 32 MVT units a cell, about 15 m on a
/// z14 tile at 40 degrees north.
pub const HEIGHT_GRID_SIZE: usize = 128;

/// A tile's building heights on a [`HEIGHT_GRID_SIZE`]-square grid over
/// `[0, EXTENT)^2`, metres, 0 where no roof is: what label occlusion marches
/// a ray through ([`BuildingOcclusion`]). A cell holds the tallest roof
/// whose triangle covers its centre, or holds the triangle's centroid (so
/// a roof smaller than a cell still counts). A raised part is treated as
/// solid down to the ground.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HeightGrid {
    pub cells: Vec<f32>,
    /// The tallest cell, metres: where a ray may stop marching.
    pub max: f32,
}

impl HeightGrid {
    #[must_use]
    pub fn from_mesh(mesh: &ExtrusionMesh) -> Self {
        let n = HEIGHT_GRID_SIZE;
        let mut cells = vec![0.0f32; n * n];
        let cell = EXTENT as f32 / n as f32;
        let mut put = |cx: i64, cy: i64, h: f32| {
            if (0..n as i64).contains(&cx) && (0..n as i64).contains(&cy) {
                let c = &mut cells[cy as usize * n + cx as usize];
                *c = c.max(h);
            }
        };
        for chunk in &mesh.chunks {
            let index = |i: usize| -> usize {
                match &chunk.indices {
                    IndexList::U16(v) => usize::from(v[i]),
                    IndexList::U32(v) => v[i] as usize,
                }
            };
            for t in 0..chunk.indices.len() / 3 {
                let [a, b, c] = [index(3 * t), index(3 * t + 1), index(3 * t + 2)].map(|i| chunk.vertices[i]);
                // Roof triangles only: normal straight up, at the top.
                if [a, b, c].iter().any(|v| v.normal_top[2] != 127 || v.normal_top[3] != 127) {
                    continue;
                }
                let bh = chunk.table[usize::from(a.row) * TABLE_TEXELS_PER_ROW + 2];
                let h = bh[1].max(bh[3]);
                if !(h > 0.0) {
                    continue;
                }
                let p = [a.pos, b.pos, c.pos].map(|q| [f32::from(q[0]), f32::from(q[1])]);
                let centroid = [(p[0][0] + p[1][0] + p[2][0]) / 3.0, (p[0][1] + p[1][1] + p[2][1]) / 3.0];
                put((centroid[0] / cell).floor() as i64, (centroid[1] / cell).floor() as i64, h);
                let lo = |k: usize| (p[0][k].min(p[1][k]).min(p[2][k]) / cell - 0.5).ceil() as i64;
                let hi = |k: usize| (p[0][k].max(p[1][k]).max(p[2][k]) / cell - 0.5).floor() as i64;
                let edge = |u: [f32; 2], v: [f32; 2], q: [f32; 2]| (v[0] - u[0]) * (q[1] - u[1]) - (v[1] - u[1]) * (q[0] - u[0]);
                for cy in lo(1).max(0)..=hi(1).min(n as i64 - 1) {
                    for cx in lo(0).max(0)..=hi(0).min(n as i64 - 1) {
                        let q = [(cx as f32 + 0.5) * cell, (cy as f32 + 0.5) * cell];
                        let (e0, e1, e2) = (edge(p[0], p[1], q), edge(p[1], p[2], q), edge(p[2], p[0], q));
                        if (e0 >= 0.0 && e1 >= 0.0 && e2 >= 0.0) || (e0 <= 0.0 && e1 <= 0.0 && e2 <= 0.0) {
                            put(cx, cy, h);
                        }
                    }
                }
            }
        }
        let max = cells.iter().copied().fold(0.0, f32::max);
        if max == 0.0 {
            // No roof: no cells, so a flat tile costs nothing to keep.
            return Self::default();
        }
        Self { cells, max }
    }

    /// The height at in-tile `[0, 1)^2` coordinates, metres.
    #[must_use]
    pub fn at_unit(&self, u: f64, v: f64) -> f32 {
        let n = HEIGHT_GRID_SIZE;
        if self.cells.is_empty() {
            return 0.0;
        }
        let (x, y) = ((u * n as f64).floor(), (v * n as f64).floor());
        if !(0.0..n as f64).contains(&x) || !(0.0..n as f64).contains(&y) {
            return 0.0;
        }
        self.cells[y as usize * n + x as usize]
    }
}

/// Which symbol layers label occlusion may hide: a symbol whose style layer
/// id is in `keep` never is; one whose id or `source-layer` is in `occlude`
/// is ([`ALL_SYMBOL_LAYERS`], `"*"`, names every layer); every other one
/// never is. The default occludes EVERY symbol -- POIs, bus stops, house
/// numbers, one-way arrows, route shields, road and place names -- as
/// Mapbox GL JS v3 does behind its 3D buildings: a sign behind a tower is
/// not seen through it. [`OcclusionLayers::POIS_ONLY`] is the earlier
/// ruling (POIs alone). `'static` slices so `ExtrusionOverrides` stays
/// `Copy`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OcclusionLayers {
    pub occlude: &'static [&'static str],
    pub keep: &'static [&'static str],
}

/// The POI source-layers of the two schemas this renderer reads:
/// OpenMapTiles' `poi` (Liberty) and Mapbox Streets v8's `poi_label`.
/// Streets' `transit_stop_label` and `airport_label`, and OpenMapTiles'
/// `aerodrome_label`, are not POIs by this rule: a station or airport name
/// is a landmark people navigate by, like a place name, and stays.
pub const POI_SOURCE_LAYERS: &[&str] = &["poi", "poi_label"];

/// Symbol layers kept visible although they read a POI source-layer:
/// Liberty's `poi_transit` (stations and airports in OpenMapTiles' `poi`),
/// so a station name stays on both schemas, as Streets' separate
/// `transit_stop_label` does.
pub const POI_LAYERS_KEPT: &[&str] = &["poi_transit"];

/// `occlude` naming every symbol layer.
pub const ALL_SYMBOL_LAYERS: &[&str] = &["*"];

impl Default for OcclusionLayers {
    fn default() -> Self {
        Self { occlude: ALL_SYMBOL_LAYERS, keep: &[] }
    }
}

impl OcclusionLayers {
    /// POI labels alone ([`POI_SOURCE_LAYERS`], less [`POI_LAYERS_KEPT`]):
    /// place names, road names, shields, arrows, house numbers and transit
    /// stops draw over buildings. The default before every symbol was
    /// occluded.
    pub const POIS_ONLY: Self = Self { occlude: POI_SOURCE_LAYERS, keep: POI_LAYERS_KEPT };

    /// One flag per style layer: whether its symbols may be hidden.
    #[must_use]
    pub fn mask(&self, layers: &[crate::style::StyleLayer]) -> Vec<bool> {
        layers
            .iter()
            .map(|l| match l {
                crate::style::StyleLayer::Symbol { id, source_layer, .. } => {
                    !self.keep.contains(&id.as_str())
                        && (self.occlude.contains(&"*")
                            || self.occlude.contains(&id.as_str())
                            || self.occlude.contains(&source_layer.as_str()))
                }
                _ => false,
            })
            .collect()
    }
}

/// Frames a label's occlusion must disagree with its shown state before it
/// flips ([`OcclusionMemory`]): three frames, 50 ms at 60 Hz, so a label
/// grazing a tower's edge while the camera moves does not flicker.
pub const OCCLUSION_FLIP_FRAMES: u8 = 3;
/// Height samples one label's ray may take: past this the step grows, so
/// a far label costs no more than a near one.
pub const OCCLUSION_MAX_SAMPLES_PER_LABEL: usize = 160;
/// Height samples one frame's occlusion may take in all. A label tested
/// after the budget is spent keeps the previous frame's decision
/// ([`OcclusionMemory`]); a frame with no memory (a capture) has no budget.
///
/// Sized for every symbol, not POIs alone: the real Shinjuku z14 tile at
/// the buildings panel's pose offers 2,000 symbols, 304 of them POIs, and
/// testing all 2,000 takes about 0.7 ms on an M-series core
/// (`occlusion_cost_every_symbol_vs_pois_only`). At 60,000 samples that
/// frame ran out after about half of them, and a label first seen past the
/// budget is shown until its turn comes -- every one of them popped in and
/// faded out again on a pan.
pub const OCCLUSION_FRAME_SAMPLES: usize = 250_000;
/// How long a label takes to fade out behind a building, or back in from
/// behind one, in seconds: the flip after the hysteresis
/// ([`OCCLUSION_FLIP_FRAMES`]) is eased rather than popped. MapLibre's
/// symbol `fadeDuration` is 300 ms; this matches the engine's own arrival
/// fade (`render::label_fade`), 200 ms.
pub const OCCLUSION_FADE_SECONDS: f64 = 0.2;
/// The longest step one frame advances an occlusion fade by: a map idle
/// for seconds and then moved must not jump a label straight to gone.
const OCCLUSION_FADE_MAX_STEP_SECONDS: f64 = 1.0 / 30.0;

/// A label's identity from frame to frame: its tile, its anchor, its layer.
pub type OcclusionKey = (crate::geo::TileId, [i32; 2], usize);

/// What a live map remembers of its labels' occlusion between frames:
/// the hysteresis state and the decision a budget-starved frame reuses.
/// Captures pass none, so a capture is a pure function of its inputs.
#[derive(Debug, Default)]
pub struct OcclusionMemory {
    /// `(hidden now, frames the raw test has disagreed)`.
    labels: std::collections::HashMap<OcclusionKey, (bool, u8)>,
    /// Where the next frame starts testing, in the order labels arrive:
    /// labels before it reuse their decision first, so a budget-starved
    /// frame's tail is tested first next frame and no label starves.
    start: usize,
    /// Each label's fade opacity, for the labels mid-fade or hidden: a
    /// label absent here is settled visible (or new, and starts settled).
    opacity: std::collections::HashMap<OcclusionKey, f32>,
    /// When the frame that wrote this memory ran: the next frame's fade
    /// step is the time since.
    at: Option<crate::timing::Instant>,
}

impl OcclusionMemory {
    /// A label is mid-flip, or the budget deferred some: another frame is
    /// needed to settle, with or without input.
    #[must_use]
    pub fn pending(&self) -> bool {
        self.start > 0
            || self.labels.values().any(|&(_, streak)| streak > 0)
            || self.opacity.values().any(|&o| o > 0.0 && o < 1.0)
    }

    /// Remembers `key` as settled visible, as if last frame showed it.
    #[cfg(test)]
    pub(crate) fn force_visible_for_test(&mut self, tile: crate::geo::TileId, anchor: [i32; 2], layer: usize) {
        self.labels.insert((tile, anchor, layer), (false, 0));
        self.opacity.remove(&(tile, anchor, layer));
    }

    /// As if the last frame ran long ago: the next fade step is the
    /// longest one, whatever the test's wall clock did.
    #[cfg(test)]
    pub(crate) fn age_for_test(&mut self) {
        self.at = None;
    }
}

/// **Which POI labels a building hides** (`ExtrusionOverrides::label_occlusion`).
///
/// Readback-free and shell-independent: the frame's building tiles as
/// [`HeightGrid`]s (built with the mesh, on the tessellation worker), and
/// for each POI label a ray from its anchor to the eye marched through
/// them on the CPU. The anchor sits on its own roof when it is inside a
/// building (a POI in a tower is labelled on the tower, not hidden by it),
/// else on the ground. A label is hidden when some building along the ray
/// stands [`OCCLUSION_MARGIN_M`] above it.
///
/// **Grid resolution.** A cell is ~32 MVT units (15 m at z14, 4 m at z16),
/// and it takes the height of any roof covering its centre, so a building's
/// silhouette is dilated by up to half a cell: a POI within about half a
/// cell of a tower's wall may be hidden when it is just visible, and one
/// in a courtyard narrower than a cell reads as inside the building.
///
/// Why not the GPU depth buffer: the point labels' text is drawn by
/// glyphon, whose shader cannot sample our depth, and a per-fragment depth
/// test would clip a label along a tower's silhouette rather than hide it;
/// a query readback lags a frame and is not on WebGL2. The CPU test hides
/// the whole label, frees its collision space for a visible one, and runs
/// the same on every shell.
pub struct BuildingOcclusion {
    /// Tiles by `(z, x, y)`, and the zooms present, finest first: a point
    /// is looked up in the finest tile holding it, so a loaded child wins
    /// over the stand-in ancestor that also covers it.
    tiles: std::collections::HashMap<crate::geo::TileId, std::sync::Arc<ExtrusionMesh>>,
    zooms: Vec<u8>,
    eye: glam::DVec3,
    meters_to_world: f64,
    /// World units per metre times the tallest building: the ray stops
    /// once it is above every roof.
    ceiling: f64,
    /// Half a grid cell of the finest building tile, world units.
    step: f64,
    /// Per style layer, whether its symbols may be hidden ([`OcclusionLayers::mask`]).
    layers: Vec<bool>,
    memory: Option<std::cell::RefCell<OcclusionMemory>>,
    next: std::cell::RefCell<std::collections::HashMap<OcclusionKey, (bool, u8)>>,
    budget: std::cell::Cell<usize>,
    /// This frame's fade step, opacity units (`dt / OCCLUSION_FADE_SECONDS`),
    /// and the opacities it leaves for the next frame.
    fade_step: f32,
    next_opacity: std::cell::RefCell<std::collections::HashMap<OcclusionKey, f32>>,
    /// Labels seen this frame, and where the budget ran out (the next
    /// frame's `OcclusionMemory::start`).
    seen: std::cell::Cell<usize>,
    starved_at: std::cell::Cell<Option<usize>>,
    /// `(labels tested, milliseconds spent)` this frame: [`Self::stats`].
    stats: std::cell::Cell<(usize, f64)>,
    /// Overlay extrusion layers' meshes ([`Self::with_overlay_occluders`]),
    /// by tile, and their zooms. Unlike the basemap's (finest tile wins,
    /// because a stand-in and its child hold the same buildings) these are
    /// separate solids: a point's height is the tallest of them and of the
    /// basemap's.
    overlay: std::collections::HashMap<crate::geo::TileId, Vec<std::sync::Arc<ExtrusionMesh>>>,
    overlay_zooms: Vec<u8>,
    /// 3D models' boxes ([`Self::with_model_occluders`]): a bus or a car
    /// standing between a label and the eye hides it, as a building does.
    models: Vec<ModelOccluder>,
}

/// **A 3D model as a label occluder** (`MapView`'s model layers): one
/// instance's bounding box, upright and turned by its heading, in world
/// units (x east, y south, z up). A label whose ray to the eye passes
/// through it -- behind the model, or under it on the ground -- is hidden
/// ([`BuildingOcclusion::with_model_occluders`]); the box is exact, with
/// no grid and no margin, so a label beside the model or in front of it
/// keeps its place. Mapbox GL JS v3 hides symbols behind its 3D models by
/// their depth the same way (MapLibre draws every symbol over them).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelOccluder {
    /// The footprint's centre, world units.
    pub center: glam::DVec2,
    /// The model's forward axis, a unit vector in world units (y south).
    pub forward: glam::DVec2,
    /// Half its length (along `forward`) and half its width, world units.
    pub half: glam::DVec2,
    /// Its bottom and top over the ground, world units.
    pub z: (f64, f64),
}

impl ModelOccluder {
    /// Whether the segment from `a` to `b` (world units, z up) passes
    /// through this box: the slab test in the box's own frame.
    #[must_use]
    pub fn crosses(&self, a: glam::DVec3, b: glam::DVec3) -> bool {
        let left = glam::DVec2::new(self.forward.y, -self.forward.x);
        let local = |p: glam::DVec3| {
            let d = p.truncate() - self.center;
            glam::DVec3::new(d.dot(self.forward), d.dot(left), p.z)
        };
        let (p, q) = (local(a), local(b));
        let lo = glam::DVec3::new(-self.half.x, -self.half.y, self.z.0);
        let hi = glam::DVec3::new(self.half.x, self.half.y, self.z.1);
        let dir = q - p;
        let (mut t0, mut t1) = (0.0f64, 1.0f64);
        for axis in 0..3 {
            let (o, d, l, h) = (p[axis], dir[axis], lo[axis], hi[axis]);
            if d.abs() < 1e-15 {
                if o < l || o > h {
                    return false;
                }
                continue;
            }
            let (mut ta, mut tb) = ((l - o) / d, (h - o) / d);
            if ta > tb {
                std::mem::swap(&mut ta, &mut tb);
            }
            t0 = t0.max(ta);
            t1 = t1.min(tb);
            if t0 > t1 {
                return false;
            }
        }
        true
    }
}

/// How far a building must stand above the ray to hide a label, metres.
pub const OCCLUSION_MARGIN_M: f64 = 3.0;

impl BuildingOcclusion {
    /// The frame's occluders: `tiles` are the tiles whose buildings draw
    /// (stand-ins included: they occlude while their children load),
    /// `meters_to_world` the extrusion plan's, `layers` the style's with
    /// `rule` deciding which of them may be hidden. `memory` is the live
    /// map's hysteresis ([`Self::into_memory`] hands it back); `None` (a
    /// capture) decides every label afresh, with no budget.
    #[must_use]
    pub fn new(
        camera: &crate::render::Camera,
        meters_to_world: f32,
        tiles: Vec<(crate::geo::TileId, std::sync::Arc<ExtrusionMesh>)>,
        layers: &[crate::style::StyleLayer],
        rule: OcclusionLayers,
        memory: Option<OcclusionMemory>,
    ) -> Self {
        let (eye, _, _) = camera.eye_target_up();
        let tallest = tiles.iter().map(|(_, m)| m.height_grid.max).fold(0.0, f32::max);
        let meters_to_world = f64::from(meters_to_world);
        let mut zooms: Vec<u8> = tiles.iter().map(|(t, _)| t.z).collect();
        zooms.sort_unstable_by(|a, b| b.cmp(a));
        zooms.dedup();
        let finest = zooms.first().copied().unwrap_or(0);
        let step = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << finest) / HEIGHT_GRID_SIZE as f64 * 0.5;
        let budget = if memory.is_some() { OCCLUSION_FRAME_SAMPLES } else { usize::MAX };
        let dt = memory
            .as_ref()
            .and_then(|m| m.at)
            .map_or(OCCLUSION_FADE_MAX_STEP_SECONDS, |at| at.elapsed().as_secs_f64().min(OCCLUSION_FADE_MAX_STEP_SECONDS));
        Self {
            fade_step: (dt / OCCLUSION_FADE_SECONDS) as f32,
            next_opacity: std::cell::RefCell::new(std::collections::HashMap::new()),
            tiles: tiles.into_iter().collect(),
            zooms,
            eye,
            meters_to_world,
            ceiling: f64::from(tallest) * meters_to_world,
            step,
            layers: rule.mask(layers),
            memory: memory.map(std::cell::RefCell::new),
            next: std::cell::RefCell::new(std::collections::HashMap::new()),
            budget: std::cell::Cell::new(budget),
            stats: std::cell::Cell::new((0, 0.0)),
            seen: std::cell::Cell::new(0),
            starved_at: std::cell::Cell::new(None),
            overlay: std::collections::HashMap::new(),
            overlay_zooms: Vec::new(),
            models: Vec::new(),
        }
    }

    /// Adds 3D models' boxes ([`ModelOccluder`]) as occluders: a label
    /// whose ray to the eye passes through one is hidden, under the same
    /// rule, memory and fade as one behind a building.
    #[must_use]
    pub fn with_model_occluders(mut self, models: Vec<ModelOccluder>) -> Self {
        self.models = models;
        self
    }

    /// Adds overlay extrusion layers' meshes (`overlay::extrude`) as
    /// occluders: the same ray test hides a label behind or under one of
    /// their solids as behind a building. Each tile's roofs keep their own
    /// height grid; where a basemap building and an overlay solid overlap
    /// the taller one counts. The sampling step follows the finest grid
    /// of either.
    #[must_use]
    pub fn with_overlay_occluders(mut self, tiles: Vec<(crate::geo::TileId, std::sync::Arc<ExtrusionMesh>)>) -> Self {
        for (tile, mesh) in tiles {
            self.ceiling = self.ceiling.max(f64::from(mesh.height_grid.max) * self.meters_to_world);
            if !self.overlay_zooms.contains(&tile.z) {
                self.overlay_zooms.push(tile.z);
            }
            self.overlay.entry(tile).or_default().push(mesh);
        }
        let finest = self.zooms.iter().chain(&self.overlay_zooms).copied().max().unwrap_or(0);
        self.step = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << finest) / HEIGHT_GRID_SIZE as f64 * 0.5;
        self
    }

    /// The tallest overlay solid under world point `p`, world units.
    fn overlay_height_at(&self, p: glam::DVec2) -> f64 {
        let mut best = 0.0f32;
        for &z in &self.overlay_zooms {
            let n = f64::from(1u32 << z);
            let size = f64::from(crate::render::WORLD_SIZE) / n;
            let (fx, fy) = (p.x / size, p.y / size);
            if !(fx >= 0.0 && fy >= 0.0 && fx < n && fy < n) {
                continue;
            }
            if let Some(meshes) = self.overlay.get(&crate::geo::TileId { z, x: fx as u32, y: fy as u32 }) {
                for m in meshes {
                    best = best.max(m.height_grid.at_unit(fx.fract(), fy.fract()));
                }
            }
        }
        f64::from(best) * self.meters_to_world
    }

    /// `(POI labels tested, CPU milliseconds they took)` so far this frame.
    #[must_use]
    pub fn stats(&self) -> (usize, f64) {
        self.stats.get()
    }

    /// The memory for the next frame: this frame's labels only.
    #[must_use]
    pub fn into_memory(self) -> OcclusionMemory {
        OcclusionMemory {
            labels: self.next.into_inner(),
            start: self.starved_at.get().unwrap_or(0),
            opacity: self.next_opacity.into_inner(),
            at: Some(crate::timing::Instant::now()),
        }
    }

    /// The building height under world point `p`, world units: the
    /// unhinted lookup, kept as the reference the hinted one is tested
    /// against.
    #[cfg(test)]
    fn height_at(&self, p: glam::DVec2) -> f64 {
        self.height_at_hinted(p, &mut None)
    }

    /// The building height under world point `p`, world units, remembering
    /// the FINEST-zoom tile it last found
    /// in `hint`. A label's ray marches up to
    /// `OCCLUSION_MAX_SAMPLES_PER_LABEL` points, nearly all of them in the
    /// tile the previous one was in, and each `height_at` was a SipHash
    /// `HashMap` lookup per zoom: on the buildings panel the occlusion
    /// test was a quarter of `paint`'s CPU, half of that the hashing. A
    /// hint only ever holds a tile of the finest zoom, so a point inside it
    /// would have been answered by that very tile on the first iteration of
    /// the loop below -- the hinted answer is the same answer.
    fn height_at_hinted<'s>(&'s self, p: glam::DVec2, hint: &mut Option<(crate::geo::TileId, &'s ExtrusionMesh)>) -> f64 {
        let basemap = self.basemap_height_at_hinted(p, hint);
        if self.overlay.is_empty() {
            basemap
        } else {
            basemap.max(self.overlay_height_at(p))
        }
    }

    /// [`Self::height_at_hinted`] over the basemap's buildings alone.
    fn basemap_height_at_hinted<'s>(&'s self, p: glam::DVec2, hint: &mut Option<(crate::geo::TileId, &'s ExtrusionMesh)>) -> f64 {
        if let Some((tile, mesh)) = *hint {
            let size = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << tile.z);
            let (fx, fy) = (p.x / size, p.y / size);
            if fx >= 0.0 && fy >= 0.0 && fx as u32 == tile.x && fy as u32 == tile.y && fx < f64::from(1u32 << tile.z) && fy < f64::from(1u32 << tile.z) {
                return f64::from(mesh.height_grid.at_unit(fx.fract(), fy.fract())) * self.meters_to_world;
            }
        }
        for (i, &z) in self.zooms.iter().enumerate() {
            let n = f64::from(1u32 << z);
            let size = f64::from(crate::render::WORLD_SIZE) / n;
            let (fx, fy) = (p.x / size, p.y / size);
            if !(fx >= 0.0 && fy >= 0.0 && fx < n && fy < n) {
                continue;
            }
            let tile = crate::geo::TileId { z, x: fx as u32, y: fy as u32 };
            if let Some(mesh) = self.tiles.get(&tile) {
                if i == 0 {
                    *hint = Some((tile, &**mesh));
                }
                return f64::from(mesh.height_grid.at_unit(fx.fract(), fy.fract())) * self.meters_to_world;
            }
        }
        0.0
    }

    /// How visible the label of style layer `layer` anchored at `anchor`
    /// (MVT units) in `tile` is this frame, `0.0..=1.0`: [`Self::hides_label`]'s
    /// decision, eased. With memory (a live map) the opacity walks towards
    /// `0` (hidden) or `1` by `dt / OCCLUSION_FADE_SECONDS` a frame, so a
    /// label slides behind a tower over [`OCCLUSION_FADE_SECONDS`] rather
    /// than popping; a label seen for the first time starts settled (the
    /// arrival fade, `render::label_fade`, is not this one's job). Without
    /// memory (a capture) it is exactly `0` or `1`. `0` means do not place.
    #[must_use]
    pub fn label_visibility(&self, tile: crate::geo::TileId, anchor: [i32; 2], layer: usize) -> f32 {
        let hidden = self.hides_label(tile, anchor, layer);
        let target = if hidden { 0.0 } else { 1.0 };
        let Some(memory) = &self.memory else { return target };
        if !self.layers.get(layer).copied().unwrap_or(false) {
            return target;
        }
        let key = (tile, anchor, layer);
        let (seen_before, previous) = {
            let m = memory.borrow();
            (m.labels.contains_key(&key), m.opacity.get(&key).copied())
        };
        let from = match previous {
            Some(o) => o,
            // Remembered, but not in the opacity table: settled visible.
            None if seen_before => 1.0,
            None => target,
        };
        let opacity =
            if from < target { (from + self.fade_step).min(target) } else { (from - self.fade_step).max(target) };
        if opacity < 1.0 {
            self.next_opacity.borrow_mut().insert(key, opacity);
        }
        opacity
    }

    /// Whether the label of style layer `layer` anchored at `anchor` (MVT
    /// units) in `tile` is hidden this frame: [`Self::hides`] for a layer
    /// the rule occludes, through the hysteresis and the frame budget when
    /// there is memory.
    #[must_use]
    pub fn hides_label(&self, tile: crate::geo::TileId, anchor: [i32; 2], layer: usize) -> bool {
        if !self.layers.get(layer).copied().unwrap_or(false) {
            return false;
        }
        let started = crate::timing::Instant::now();
        let hidden = self.hides_label_timed(tile, anchor, layer);
        let (n, ms) = self.stats.get();
        self.stats.set((n + 1, ms + started.elapsed().as_secs_f64() * 1000.0));
        hidden
    }

    fn hides_label_timed(&self, tile: crate::geo::TileId, anchor: [i32; 2], layer: usize) -> bool {
        let Some(memory) = &self.memory else { return self.hides(tile, anchor) };
        let key = (tile, anchor, layer);
        let (previous, start) = {
            let m = memory.borrow();
            (m.labels.get(&key).copied(), m.start)
        };
        let i = self.seen.get();
        self.seen.set(i + 1);
        // Before this frame's start a remembered label waits its turn; past
        // a spent budget every label does, and the first to wait at or
        // after the start is where the next frame begins.
        let deferred = previous.is_some() && i < start;
        if self.budget.get() == 0 && i >= start && self.starved_at.get().is_none() {
            self.starved_at.set(Some(i));
        }
        let state = if deferred || self.budget.get() == 0 {
            previous.unwrap_or((false, 0))
        } else {
            let raw = self.hides(tile, anchor);
            match previous {
                None => (raw, 0),
                Some((shown, _)) if shown == raw => (shown, 0),
                Some((shown, streak)) if streak + 1 >= OCCLUSION_FLIP_FRAMES => (!shown, 0),
                Some((shown, streak)) => (shown, streak + 1),
            }
        };
        self.next.borrow_mut().insert(key, state);
        state.0
    }

    /// Whether a building hides the point at `anchor` (MVT units) in
    /// `tile`, afresh: no rule, no memory.
    #[must_use]
    pub fn hides(&self, tile: crate::geo::TileId, anchor: [i32; 2]) -> bool {
        if (self.tiles.is_empty() && self.overlay.is_empty() && self.models.is_empty()) || !(self.meters_to_world > 0.0) {
            return false;
        }
        let size = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << tile.z);
        let a = glam::DVec2::new(
            (f64::from(tile.x) + f64::from(anchor[0]) / f64::from(EXTENT)) * size,
            (f64::from(tile.y) + f64::from(anchor[1]) / f64::from(EXTENT)) * size,
        );
        let mut hint = None;
        // The anchor stands on a basemap roof it is inside (a building's
        // own POI), but on the GROUND under an overlay solid: a street name
        // under a hexbin column is hidden by it, not lifted onto it.
        let z0 = self.basemap_height_at_hinted(a, &mut hint);
        // A model between the anchor and the eye (or over the anchor): an
        // exact box test, one sample of the budget each.
        if !self.models.is_empty() {
            let from = a.extend(z0);
            let hidden = self.models.iter().any(|m| m.crosses(from, self.eye));
            self.budget.set(self.budget.get().saturating_sub(self.models.len().div_ceil(16)));
            if hidden {
                return true;
            }
            if self.tiles.is_empty() && self.overlay.is_empty() {
                return false;
            }
        }
        let to_eye = self.eye.truncate() - a;
        let span = to_eye.length();
        if !(span > 0.0) || self.eye.z <= z0 {
            return false;
        }
        let rise = (self.eye.z - z0) / span;
        let margin = OCCLUSION_MARGIN_M * self.meters_to_world;
        let reach = ((self.ceiling - z0) / rise).min(span);
        let step = self.step.max(reach / OCCLUSION_MAX_SAMPLES_PER_LABEL as f64);
        let dir = to_eye / span;
        // Start one cell out, past the anchor's own roof edge.
        let (samples, hidden) =
            march_ray(a, dir, z0, rise, 2.0 * self.step, reach, step, |_, p, z| self.height_at_hinted(p, &mut hint) > z + margin);
        self.budget.set(self.budget.get().saturating_sub(samples.max(1)));
        hidden
    }
}

/// THE ray march through extruded solids, shared by label occlusion
/// ([`BuildingOcclusion::hides`]) and picking
/// (`overlay::extrude::pick`): from ground point `a` (world units, at
/// height `z0`) along the horizontal unit direction `dir` toward the eye,
/// rising `rise` world units per unit of horizontal travel, sampled every
/// `step` from `start` while short of `reach`. `visit(d, point, ray_z)`
/// sees each sample's horizontal distance, ground position and the ray's
/// height there, and returns `true` to stop. Returns the samples taken and
/// whether a visit stopped the march.
#[allow(clippy::too_many_arguments)]
pub fn march_ray(
    a: glam::DVec2,
    dir: glam::DVec2,
    z0: f64,
    rise: f64,
    start: f64,
    reach: f64,
    step: f64,
    mut visit: impl FnMut(f64, glam::DVec2, f64) -> bool,
) -> (usize, bool) {
    let mut d = start;
    let mut samples = 0;
    while d < reach {
        samples += 1;
        if visit(d, a + dir * d, z0 + d * rise) {
            return (samples, true);
        }
        d += step;
    }
    (samples, false)
}

/// One wall this tile owns, in tile coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WallSegment {
    pub p1: [i32; 2],
    pub p2: [i32; 2],
    /// Unit, pointing away from the solid; the UNCLIPPED edge's normal.
    pub normal: [f32; 2],
}

/// The row's three texels, with R15 applied: a height below its base is
/// raised to it, so the roof sits at `max(base, height)`.
fn table_texels(row: &ExtrusionRow) -> [[f32; 4]; 3] {
    let c = |c: crate::style::Rgba| [c.r, c.g, c.b, c.a];
    [
        c(row.color_from),
        c(row.color_to),
        [row.base_from, row.height_from.max(row.base_from), row.base_to, row.height_to.max(row.base_to)],
    ]
}

/// The plan's extrusion ops, in order, with their chunk-local row.
fn extrusion_ops(plan: &PaintPlan) -> impl Iterator<Item = (u16, &ExtrusionRow, &geo_types::Geometry<i32>)> {
    plan.ops.iter().filter_map(|op| match op {
        PaintOp::Extrusion { group, row, geometry } => Some((*group, row, &**geometry)),
        _ => None,
    })
}

/// Builds one chunk; `finish` narrows its indices.
#[derive(Default)]
struct ChunkBuilder {
    vertices: Vec<ExtrusionVertex>,
    indices: Vec<u32>,
    table: Vec<[f32; 4]>,
    spans: Vec<GroupSpan>,
    walls: usize,
    /// Wall segments that survived `wall_segments` and were emitted; the
    /// debug invariant `walls` is checked against.
    segments_emitted: usize,
    rows: usize,
}

impl ChunkBuilder {
    fn finish(self) -> ExtrusionChunk {
        debug_assert_eq!(self.walls, self.segments_emitted, "one wall quad per surviving segment");
        let indices = IndexList::rebased(&self.indices, 0, self.vertices.len());
        ExtrusionChunk { vertices: self.vertices, indices, table: self.table, spans: self.spans, walls: self.walls }
    }

    /// Opens a span for `group` unless the current one already is it.
    fn enter_group(&mut self, group: u16) {
        let first_index = self.indices.len() as u32;
        match self.spans.last() {
            Some(span) if span.group == group => {}
            _ => self.spans.push(GroupSpan { group, first_index, index_count: 0 }),
        }
    }

    fn close_span(&mut self) {
        let end = self.indices.len() as u32;
        if let Some(span) = self.spans.last_mut() {
            span.index_count = end - span.first_index;
        }
    }
}

/// Makes a roof vertex out of each lyon vertex; `row` is filled later.
struct RoofCtor;

impl FillVertexConstructor<[f32; 2]> for RoofCtor {
    fn new_vertex(&mut self, vertex: FillVertex) -> [f32; 2] {
        let p = vertex.position();
        [p.x, p.y]
    }
}

/// Reused across a tile's features, like `mesh::append_ops_with`'s.
struct Scratch {
    tess: FillTessellator,
    ring: Vec<Point>,
    roof: VertexBuffers<[f32; 2], u32>,
    roof_failures: usize,
    /// The grid split's buffers (`render::subdivide`), for a feature
    /// wider than a [`GlobeGrid`] cell.
    subdivide: crate::render::subdivide::SubdivideScratch,
    /// One wall's grid crossings, ordered along it.
    crossings: Vec<(f64, Point)>,
}

/// The grid a feature's roof and walls are cut on so that, drawn on the
/// globe, no triangle spans more than one cell: `granularity` cells per
/// tile edge, each `cell` MVT units (`EXTENT / granularity`).
///
/// The globe ground's own grid (`render::subdivide::granularity_for_zoom`
/// of the tile's zoom, 5.625 degrees of longitude a cell at every zoom up
/// to z6, aligned on the same 1/64-world lines whatever the zoom), so an
/// overlay roof bends exactly where the ground under it does. See
/// [`tessellate_extrusions_on_grid`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct GlobeGrid {
    granularity: u32,
    cell: f32,
}

impl GlobeGrid {
    fn new(granularity: u32) -> Option<Self> {
        (granularity >= 2).then(|| Self { granularity, cell: EXTENT as f32 / granularity as f32 })
    }

    /// The grid for `geometry`, or `None` when it fits one cell on both
    /// axes (its unclipped bounding box at most a cell wide and tall):
    /// such a feature already has no triangle wider than a cell, and is
    /// tessellated exactly as with no grid at all -- a hexbin, a grid
    /// square, a county.
    fn for_feature(grid: Option<Self>, geometry: &geo_types::Geometry<i32>) -> Option<Self> {
        let grid = grid?;
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        let mut visit = |ring: &geo_types::LineString<i32>| {
            for c in &ring.0 {
                (x0, y0, x1, y1) = (x0.min(c.x), y0.min(c.y), x1.max(c.x), y1.max(c.y));
            }
        };
        match geometry {
            geo_types::Geometry::Polygon(p) => visit(p.exterior()),
            geo_types::Geometry::MultiPolygon(mp) => mp.iter().for_each(|p| visit(p.exterior())),
            _ => return None,
        }
        let cell = f64::from(grid.cell);
        (x0 <= x1 && (f64::from(x1) - f64::from(x0) > cell || f64::from(y1) - f64::from(y0) > cell)).then_some(grid)
    }
}

fn round_i16(v: f32) -> i16 {
    v.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// Triangulates the clipped footprint into `chunk`: roof vertices at the
/// top, every triangle wound to a positive xy signed area.
fn append_roof(chunk: &mut ChunkBuilder, geometry: &geo_types::Geometry<i32>, row: u16, tone: u16, grid: Option<GlobeGrid>, scratch: &mut Scratch) {
    use geo_types::Geometry::*;
    let clipped = crate::style::clip_fill_geometry_to_tile_extent(geometry);
    let polygons: Vec<&geo_types::Polygon<i32>> = match &clipped {
        Polygon(p) => vec![p],
        MultiPolygon(mp) => mp.iter().collect(),
        _ => return,
    };
    let options = FillOptions::tolerance(0.1).with_fill_rule(FillRule::NonZero);
    for polygon in polygons {
        crate::render::mesh::ring_points(polygon.exterior(), 1.0, &mut scratch.ring);
        if scratch.ring.len() < 3 {
            continue;
        }
        scratch.roof.vertices.clear();
        scratch.roof.indices.clear();
        {
            let mut out = BuffersBuilder::new(&mut scratch.roof, RoofCtor);
            crate::render::mesh::force_winding(&mut scratch.ring, true);
            let mut builder = scratch.tess.builder(&options, &mut out);
            builder.add_polygon(LyonPolygon { points: scratch.ring.as_slice(), closed: true });
            for ring in polygon.interiors() {
                crate::render::mesh::ring_points(ring, 1.0, &mut scratch.ring);
                if scratch.ring.len() < 3 {
                    continue;
                }
                crate::render::mesh::force_winding(&mut scratch.ring, false);
                builder.add_polygon(LyonPolygon { points: scratch.ring.as_slice(), closed: true });
            }
            if builder.build().is_err() {
                scratch.roof_failures += 1;
                continue;
            }
        }
        if let Some(grid) = grid {
            // Cut in lyon's f32 units, before rounding: the crossings are
            // `subdivide::intersect`'s, canonical per edge, so a crossing
            // two triangles share rounds to one integer vertex -- and a
            // wall's top edge (`append_walls`) meets the same one.
            crate::render::subdivide::subdivide_fill_span(&mut scratch.roof, 0, 0, grid.granularity, grid.cell, &mut scratch.subdivide);
        }
        let base = chunk.vertices.len() as u32;
        chunk.vertices.extend(scratch.roof.vertices.iter().map(|p| ExtrusionVertex {
            pos: [round_i16(p[0]), round_i16(p[1])],
            normal_top: [0, 0, 127, 127],
            row,
            flags: tone,
        }));
        let vertices = &chunk.vertices;
        for t in scratch.roof.indices.as_chunks::<3>().0.iter() {
            let [a, b, c] = [t[0] + base, t[1] + base, t[2] + base];
            let pos = |i: u32| {
                let p = vertices[i as usize].pos;
                (i64::from(p[0]), i64::from(p[1]))
            };
            let ((ax, ay), (bx, by), (cx, cy)) = (pos(a), pos(b), pos(c));
            let area = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
            match area.signum() {
                0 => {}
                1 => chunk.indices.extend([a, b, c]),
                _ => chunk.indices.extend([a, c, b]),
            }
        }
    }
}

/// One quad per wall segment this tile owns.
fn append_walls(chunk: &mut ChunkBuilder, geometry: &geo_types::Geometry<i32>, row: u16, tone: u16, grid: Option<GlobeGrid>, crossings: &mut Vec<(f64, Point)>) {
    let segments = wall_segments(geometry);
    let mut pieces = 0;
    for w in &segments {
        debug_assert!(
            w.normal.iter().all(|n| n.is_finite())
                && ((w.normal[0] * w.normal[0] + w.normal[1] * w.normal[1]).sqrt() - 1.0).abs() < 1e-3,
            "wall normal {w:?} is not a finite unit vector"
        );
        let n = [(w.normal[0] * 127.0).round() as i8, (w.normal[1] * 127.0).round() as i8];
        let v = |p: [i16; 2], top: i8| ExtrusionVertex { pos: p, normal_top: [n[0], n[1], 0, top], row, flags: tone };
        let mut quad = |a: [i16; 2], b: [i16; 2]| {
            let first = chunk.vertices.len() as u32;
            chunk.vertices.extend([v(a, 0), v(b, 0), v(a, 127), v(b, 127)]);
            chunk.indices.extend([first, first + 1, first + 2, first + 2, first + 1, first + 3]);
            chunk.walls += 1;
            pieces += 1;
        };
        let end = |p: [i32; 2]| [p[0] as i16, p[1] as i16];
        let Some(grid) = grid else {
            quad(end(w.p1), end(w.p2));
            continue;
        };
        // One quad per grid cell the segment crosses, cut where the roof's
        // boundary edge is cut (`subdivide::intersect`, the same canonical
        // expression), so the wall's top meets the roof vertex for vertex.
        let (a, b) = (lyon::math::point(w.p1[0] as f32, w.p1[1] as f32), lyon::math::point(w.p2[0] as f32, w.p2[1] as f32));
        crossings.clear();
        for axis in 0..2 {
            let (from, to) = if axis == 0 { (a.x, b.x) } else { (a.y, b.y) };
            let (low, high) = (from.min(to), from.max(to));
            let mut line = (low / grid.cell).floor() as i64 + 1;
            while (line as f32) * grid.cell < high {
                let coord = line as f32 * grid.cell;
                crossings.push((f64::from((coord - from) / (to - from)), crate::render::subdivide::intersect(a, b, axis, coord)));
                line += 1;
            }
        }
        crossings.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut previous = end(w.p1);
        for p in crossings.iter().map(|(_, p)| [round_i16(p.x), round_i16(p.y)]).chain(std::iter::once(end(w.p2))) {
            if p != previous {
                quad(previous, p);
                previous = p;
            }
        }
    }
    chunk.segments_emitted += pieces;
}

/// Tessellates every `PaintOp::Extrusion` in `plan`, in order: a roof from
/// the clipped footprint, and walls from the unclipped rings (spec §3.2).
/// No grid: a basemap building tile (z12 and deeper) is never wider than a
/// globe cell. [`tessellate_extrusions_on_grid`] cuts wide features.
#[must_use]
pub fn tessellate_extrusions(plan: &PaintPlan) -> ExtrusionMesh {
    tessellate_extrusions_on_grid(plan, 1)
}

/// [`tessellate_extrusions`], with every feature wider or taller than one
/// cell of a `granularity`-per-tile-edge grid cut on it: its roof
/// triangles clipped to the cells (`render::subdivide::subdivide_fill_span`,
/// the fill mesh's own watertight split) and its walls split where they
/// cross a grid line, so no roof triangle or wall quad spans more than a
/// cell. A `granularity` below 2 is no grid.
///
/// # Why: the globe arm projects per vertex
///
/// `extrusion_shader.wgsl` puts each VERTEX on the sphere and raises it
/// along the normal; the rasterizer joins them with straight lines. A
/// country-sized roof tessellates to a few triangles whose corners are
/// tens of degrees apart, so the roof between them is a chord plane
/// through the planet: across 40 degrees it sags about 6% of the radius
/// (some 380 km) below the ground, the far-side test discards it, and
/// the ground shows through the roof's middle (walls likewise bow under
/// the surface between their ends). Cut on a 5.625-degree grid the sag
/// is at most `1 - cos(2.8 deg)` = 0.12% of the radius in a cell's
/// diagonal, the same sag the ground's own cell has under it.
///
/// # Which grid
///
/// The overlay's tile zoom `z` passes `granularity_for_zoom(z)`: MapLibre's
/// `fill` granularity scheme (`max(base >> z, 1)`, `subdivision_granularity
/// _settings.ts`, used for its globe `fill-extrusion` roofs and walls in
/// `fill_extrusion_bucket.ts`) at this crate's base of 64, the one the
/// globe ground mesh is cut with. A cell is then 5.625 degrees of
/// longitude at every tile zoom to z6 and the grid lines are the ground's,
/// so a roof bends where the ground under it bends; from z6 a whole tile
/// is one cell and nothing is cut.
///
/// # What stays identical
///
/// A feature whose unclipped bounding box fits in one cell is tessellated
/// exactly as by [`tessellate_extrusions`] (`GlobeGrid::for_feature`):
/// every hexbin, grid square and county-sized polygon, whatever grid
/// line it happens to straddle. The flat (Mercator) draw of a cut feature
/// covers the same area -- the pieces are coplanar -- with each new vertex
/// rounded to the MVT unit grid like every other.
#[must_use]
pub fn tessellate_extrusions_on_grid(plan: &PaintPlan, granularity: u32) -> ExtrusionMesh {
    let grid = GlobeGrid::new(granularity);
    let mut scratch = Scratch {
        tess: FillTessellator::new(),
        ring: Vec::new(),
        roof: VertexBuffers::new(),
        roof_failures: 0,
        subdivide: crate::render::subdivide::SubdivideScratch::default(),
        crossings: Vec::new(),
    };
    let mut chunks = Vec::new();
    let mut chunk: Option<ChunkBuilder> = None;
    for (group, row, geometry) in extrusion_ops(plan) {
        if chunk.as_ref().is_some_and(|c| c.rows == MAX_ROWS_PER_CHUNK) {
            chunks.push(chunk.take().expect("checked").finish());
        }
        let c = chunk.get_or_insert_with(ChunkBuilder::default);
        let row_index = c.rows as u16;
        c.rows += 1;
        c.table.extend(table_texels(row));
        c.enter_group(group);
        let tone = tone_key(geometry, row);
        let feature_grid = GlobeGrid::for_feature(grid, geometry);
        append_roof(c, geometry, row_index, tone, feature_grid, &mut scratch);
        if row.height_from > row.base_from || row.height_to > row.base_to {
            append_walls(c, geometry, row_index, tone, feature_grid, &mut scratch.crossings);
        }
        c.close_span();
    }
    chunks.extend(chunk.map(ChunkBuilder::finish));
    let mut mesh = ExtrusionMesh { chunks, roof_failures: scratch.roof_failures, height_grid: HeightGrid::default() };
    mesh.height_grid = HeightGrid::from_mesh(&mesh);
    mesh
}

/// Recomputes every chunk's table from the plan's Extrusion ops, in order. The vertices are untouched.
///
/// Returns `false`, and leaves `mesh` exactly as it was, when the plan's
/// extrusion-op count differs from the rows `mesh` holds: the caller must
/// then re-tessellate with [`tessellate_extrusions`]. A mismatch is
/// recoverable, so it is reported rather than asserted.
///
/// A count is all this can check. A plan with the same count but different
/// or reordered features would write other features' colours and heights,
/// so the caller must first gate on the plans' extrusion ops agreeing op by
/// op in group, geometry and `ExtrusionRow::same_shape` (what
/// `map_view::ops_share_geometry_set` and `mesh::op_geometry_matches`
/// compare); only the colours may differ.
#[must_use = "false means the mesh was left stale and must be re-tessellated"]
pub fn rewrite_table(mesh: &mut ExtrusionMesh, plan: &PaintPlan) -> bool {
    let rows_in_mesh: usize = mesh.chunks.iter().map(|c| c.table.len() / TABLE_TEXELS_PER_ROW).sum();
    if extrusion_ops(plan).count() != rows_in_mesh {
        return false;
    }
    let mut rows = extrusion_ops(plan).map(|(_, row, _)| row);
    for chunk in &mut mesh.chunks {
        let n = chunk.table.len() / TABLE_TEXELS_PER_ROW;
        chunk.table.clear();
        for row in rows.by_ref().take(n) {
            chunk.table.extend(table_texels(row));
        }
    }
    true
}

/// A ring's points with consecutive duplicates and the closing repeat
/// removed, oriented so its shoelace sum has the sign `positive` asks for;
/// `None` for a ring with no area (fewer than 3 distinct points, or
/// collinear), whose walls would be a zero-thickness sheet.
fn oriented_ring(ring: &geo_types::LineString<i32>, positive: bool) -> Option<Vec<[i64; 2]>> {
    let mut points: Vec<[i64; 2]> = Vec::with_capacity(ring.0.len());
    for c in &ring.0 {
        let p = [i64::from(c.x), i64::from(c.y)];
        if points.last() != Some(&p) {
            points.push(p);
        }
    }
    while points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    let n = points.len();
    if n < 3 {
        return None;
    }
    let sum: i64 = (0..n)
        .map(|i| {
            let (p, q) = (points[i], points[(i + 1) % n]);
            p[0] * q[1] - q[0] * p[1]
        })
        .sum();
    if sum == 0 {
        return None;
    }
    if (sum > 0) != positive {
        points.reverse();
    }
    Some(points)
}

/// Liang–Barsky: `p1 → p2` clipped to the CLOSED square `[0, EXTENT]²`, or
/// `None` when no part of it is inside.
fn clip_to_square(p1: [i64; 2], p2: [i64; 2]) -> Option<([f64; 2], [f64; 2])> {
    let e = f64::from(EXTENT);
    let (x0, y0) = (p1[0] as f64, p1[1] as f64);
    let (dx, dy) = ((p2[0] - p1[0]) as f64, (p2[1] - p1[1]) as f64);
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    for (p, q) in [(-dx, x0), (dx, e - x0), (-dy, y0), (dy, e - y0)] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                t0 = t0.max(r);
            } else {
                if r < t0 {
                    return None;
                }
                t1 = t1.min(r);
            }
        }
    }
    Some(([x0 + t0 * dx, y0 + t0 * dy], [x0 + t1 * dx, y0 + t1 * dy]))
}

/// Every wall of `geometry` this tile owns: unclipped rings oriented so
/// normal = normalize(dy, -dx) points away from the solid, each segment
/// clipped to the half-open square [0, EXTENT)^2 (spec §3.2, R3).
#[must_use]
pub fn wall_segments(geometry: &geo_types::Geometry<i32>) -> Vec<WallSegment> {
    use geo_types::Geometry::*;
    let polygons: Vec<&geo_types::Polygon<i32>> = match geometry {
        Polygon(p) => vec![p],
        MultiPolygon(mp) => mp.iter().collect(),
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let rings = polygons
        .into_iter()
        .flat_map(|p| std::iter::once((p.exterior(), true)).chain(p.interiors().iter().map(|r| (r, false))));
    for (ring, exterior) in rings {
        let Some(points) = oriented_ring(ring, exterior) else { continue };
        let n = points.len();
        for i in 0..n {
            let (a, b) = (points[i], points[(i + 1) % n]);
            let Some((c1, c2)) = clip_to_square(a, b) else { continue };
            let r = |v: [f64; 2]| [v[0].round() as i32, v[1].round() as i32];
            let (p1, p2) = (r(c1), r(c2));
            // The half-open rule: the east and south edges belong to the neighbour.
            if (p1[0] == EXTENT && p2[0] == EXTENT) || (p1[1] == EXTENT && p2[1] == EXTENT) || p1 == p2 {
                continue;
            }
            let (dx, dy) = ((b[0] - a[0]) as f64, (b[1] - a[1]) as f64);
            let len = dx.hypot(dy);
            out.push(WallSegment { p1, p2, normal: [(dy / len) as f32, (-dx / len) as f32] });
        }
    }
    out
}

/// The `fill-extrusion` pipeline's per-layer uniform (`extrusion_shader.wgsl`
/// `ExtrusionParams`, group 3), 128 bytes. WGSL puts each `vec3<f32>` at a
/// 16-byte boundary with the next `f32` in its fourth lane, which is the
/// layout these fields spell.
///
/// - `light_dir`: (x east, y south, z up), of length `r` (the style's radial
///   `light.position[0]`, 1.15 by default), NOT unit -- MapLibre's
///   `u_lightpos`; already turned by the bearing for a `viewport` light
///   ([`light_direction`]).
/// - `light_intensity`, `light_color`: the style's light at the frame's zoom.
/// - `vertical_gradient`: `fill-extrusion-vertical-gradient`, 1.0 or 0.0.
/// - `translate_px`: `fill-extrusion-translate` in logical pixels, in the
///   map's (x east, y south) frame; a `viewport` anchor is turned by the
///   caller.
/// - `opacity`: `fill-extrusion-opacity`, the output alpha on its own (the
///   colour's alpha is ignored, as in MapLibre).
/// - `meters_to_world`: world units per metre at the frame's latitude; 0.0
///   flattens every building onto its footprint.
/// - `viewport`: `(width, height, 0, 0)`, the frame's LOGICAL viewport
///   ([`crate::render::Camera`]'s); the shader multiplies by the camera
///   uniform's `device_pixel_ratio` for framebuffer pixels. It turns a clip
///   position into pixels for the edge highlight's width, computed per
///   vertex so the fragment stage needs no screen-space derivative (see
///   `extrusion_shader.wgsl` `edge_px`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ExtrusionParams {
    pub light_dir: [f32; 3],
    pub light_intensity: f32,
    pub light_color: [f32; 3],
    pub vertical_gradient: f32,
    pub translate_px: [f32; 2],
    pub opacity: f32,
    pub meters_to_world: f32,
    /// [`ExtrusionLook`]: `(face_contrast, ambient_occlusion,
    /// occlusion_height_m, tone_jitter)`. All zero is MapLibre.
    pub look_a: [f32; 4],
    /// [`ExtrusionLook`]: `(roof_tint, edge_highlight, haze, facade)`.
    pub look_b: [f32; 4],
    /// What the distance haze blends toward (rgb, alpha unused): the
    /// frame's background, so far buildings recede into the same colour
    /// the ground and horizon fade to.
    pub haze_color: [f32; 4],
    /// View-depth bands (clip `w`): `(haze_start, haze_end, cut_start,
    /// cut_end)`. The haze ramps over the first pair; buildings sink to
    /// nothing over the second (`cover_plan::building_fade_band`), so they
    /// never stop on a hard line where the cover's building data ends. An
    /// `end` of 0 turns its band off.
    pub bands: [f32; 4],
    /// `(width, height, metres_per_unit, 0)`: the logical viewport, and
    /// for a facade layer the metres one vertex position unit spans at
    /// its tile zoom (`overlay::extrude::facade_metres_per_unit`), so the
    /// window grid is laid out in metres; 0 otherwise.
    pub viewport: [f32; 4],
    /// `xyz`: [`Self::light_dir`] re-expressed in the globe's unit-sphere
    /// space at the camera's centre ([`globe_light`]), what a column on a
    /// globe frame dots its sphere-space normal with -- MapLibre's
    /// `u_lightpos_globe`. Read only where the draw's `model.transition`
    /// is above zero; zero (the default) on every flat frame. `w`:
    /// [`ExtrusionLook::night`], read only by the facade.
    pub globe_light: [f32; 4],
}

impl ExtrusionParams {
    /// Every field zero: a base for struct-update syntax.
    pub const ZERO: Self = Self {
        light_dir: [0.0; 3],
        light_intensity: 0.0,
        light_color: [0.0; 3],
        vertical_gradient: 0.0,
        translate_px: [0.0; 2],
        opacity: 0.0,
        meters_to_world: 0.0,
        look_a: [0.0; 4],
        look_b: [0.0; 4],
        haze_color: [0.0; 4],
        bands: [0.0; 4],
        viewport: [0.0; 4],
        globe_light: [0.0; 4],
    };
}

/// How buildings are shaded beyond MapLibre's `fill-extrusion` lighting.
/// Engine options, not style properties: every field at zero
/// ([`Self::MAPLIBRE`], the default) is MapLibre GL JS's look exactly;
/// [`Self::ENHANCED`] is what the 3D buildings gallery panel turns on.
///
/// - `face_contrast` (0..=1): mixes MapLibre's one-light model toward a
///   two-light one (a key light from the style's light azimuth and a
///   weaker fill light 100 degrees round from it, both half-Lambert, a
///   lower ambient): walls facing different ways always differ, where
///   MapLibre gives every wall facing away from the light the same shade.
/// - `ambient_occlusion` (0..=1), `occlusion_height_m`: a wall darkens
///   toward its base, by up to `ambient_occlusion`, over its lowest
///   `occlusion_height_m` metres above its base. Per fragment, from the
///   interpolated height above base: no extra geometry.
/// - `tone_jitter`: each building's lightness is scaled by up to
///   `1 +/- tone_jitter`, from a hash of its footprint's shape (the
///   decoder keeps no feature id), so neighbours of one colour separate.
/// - `roof_tint` (0..=1): roofs are mixed toward a cool light grey
///   ([`ROOF_TINT`]), so a roof never matches warm land.
/// - `edge_highlight` (0..=1): a one-to-two-pixel light line along the top
///   of every wall, where it meets its roof.
/// - `haze` (0..=1): buildings blend toward the background with view
///   depth, from the look-at point (nothing nearer) to four times its
///   distance, by at most `haze`.
/// - `facade` (0..=1): a procedural facade on the walls of an overlay
///   layer extruded with facades (`overlay::extrude::extrude_with_facades`):
///   a window grid scaled by each building's floor height, mullions and
///   spandrels, by its material (glass curtain wall, limestone, brick,
///   concrete, cast iron), fading to its mean where a window is under a
///   couple of pixels. No effect on a layer without facades, the basemap's
///   buildings included, or on a globe frame.
/// - `night` (0..=1): how dark it is for the facade: lit windows (a share
///   per building, random per window) glow, the walls between them do not.
/// - `haze_color`: what an OVERLAY layer's `haze` blends toward, and the
///   sky a facade's glass mirrors. A style layer hazes toward the frame's
///   background whatever this says; an overlay layer is hazed only when
///   this is set (none was before it existed), over the same band, or
///   over `haze_range_m` (metres of view depth) when that is set: an
///   aerial perspective that does not tie the haze to the camera's
///   distance, so a skyline kilometres off stays readable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionLook {
    pub face_contrast: f32,
    pub ambient_occlusion: f32,
    pub occlusion_height_m: f32,
    pub tone_jitter: f32,
    pub roof_tint: f32,
    pub edge_highlight: f32,
    pub haze: f32,
    pub facade: f32,
    pub night: f32,
    pub haze_color: Option<crate::style::Rgba>,
    pub haze_range_m: Option<[f32; 2]>,
}

/// The cool light grey [`ExtrusionLook::roof_tint`] mixes a roof toward.
pub const ROOF_TINT: [f32; 3] = [0.78, 0.83, 0.93];

impl ExtrusionLook {
    /// MapLibre GL JS's lighting, nothing added.
    pub const MAPLIBRE: Self = Self {
        face_contrast: 0.0,
        ambient_occlusion: 0.0,
        occlusion_height_m: 20.0,
        tone_jitter: 0.0,
        roof_tint: 0.0,
        edge_highlight: 0.0,
        haze: 0.0,
        facade: 0.0,
        night: 0.0,
        haze_color: None,
        haze_range_m: None,
    };
    /// Every improvement on, at the strengths the gallery was tuned to.
    pub const ENHANCED: Self = Self {
        face_contrast: 1.0,
        ambient_occlusion: 0.38,
        occlusion_height_m: 20.0,
        tone_jitter: 0.05,
        roof_tint: 0.5,
        edge_highlight: 0.35,
        haze: 0.45,
        facade: 0.0,
        night: 0.0,
        haze_color: None,
        haze_range_m: None,
    };

    /// Writes this look into `params`' `look_a` and `look_b`.
    pub fn apply(&self, params: &mut ExtrusionParams) {
        params.look_a = [
            self.face_contrast.clamp(0.0, 1.0),
            self.ambient_occlusion.clamp(0.0, 1.0),
            self.occlusion_height_m.max(0.1),
            self.tone_jitter.clamp(0.0, 0.5),
        ];
        params.look_b = [self.roof_tint.clamp(0.0, 1.0), self.edge_highlight.clamp(0.0, 1.0), self.haze.clamp(0.0, 1.0), self.facade.clamp(0.0, 1.0)];
        params.globe_light[3] = self.night.clamp(0.0, 1.0);
    }
}

impl Default for ExtrusionLook {
    fn default() -> Self {
        Self::MAPLIBRE
    }
}

/// A building's tone key, `0..=u16::MAX`, the vertex `flags`: a hash of its
/// footprint that does not move with the tile. The vertex count and the
/// coordinate sums relative to the footprint's own bounding box are the
/// same in every tile that carries the whole building (an MVT producer
/// repeats a building in each neighbour's buffer), so one building gets one
/// tone on both sides of a seam; the height decorrelates neighbours of the
/// same shape. Only a building larger than the tile buffer, clipped
/// differently per tile, can differ across a seam.
#[must_use]
pub fn tone_key(geometry: &geo_types::Geometry<i32>, row: &ExtrusionRow) -> u16 {
    use geo_types::Geometry::*;
    let polygons: Vec<&geo_types::Polygon<i32>> = match geometry {
        Polygon(p) => vec![p],
        MultiPolygon(mp) => mp.iter().collect(),
        _ => return 0,
    };
    let mut min = [i64::MAX; 2];
    let mut count = 0u64;
    for p in &polygons {
        for c in p.exterior().coords() {
            min = [min[0].min(i64::from(c.x)), min[1].min(i64::from(c.y))];
            count += 1;
        }
    }
    let (mut sx, mut sy) = (0i64, 0i64);
    for p in &polygons {
        for c in p.exterior().coords() {
            sx += i64::from(c.x) - min[0];
            sy += i64::from(c.y) - min[1];
        }
    }
    // splitmix64's finaliser after each word: every input bit reaches the
    // 16 bits kept.
    let mix = |mut z: u64| {
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    let mut h = 0x9e37_79b9_7f4a_7c15_u64;
    for v in [count, sx as u64, sy as u64, u64::from(row.height_from.to_bits()), u64::from(row.base_from.to_bits())] {
        h = mix(h ^ v);
    }
    (h >> 48) as u16
}

/// The vector toward the light, in (x east, y south, z up), of length `r`.
///
/// MapLibre `sphericalToCartesian` (`src/util/util.ts:674`): the azimuth
/// gains 90° ("north" is 0° in the style, 90° in the maths), then
/// `x = r·cos(az)·sin(polar)`, `y = r·sin(az)·sin(polar)`, `z = r·cos(polar)`.
/// A `viewport` light is then turned with the camera
/// (`fill_extrusion_program.ts:84-87`, `mat3.fromRotation(lightMat,
/// transform.bearingInRadians)`): MapLibre's `bearingInRadians` is the
/// compass bearing in radians, which is what `bearing_rad` is here too
/// (`Camera::bearing`), so the rotation is the same `(c·x − s·y, s·x + c·y)`.
///
/// **Not normalised**, as in MapLibre, which dots the unit normal with this
/// raw vector: the default `r = 1.15` lets `directional` reach 1.15 before
/// its clamp, so a lit face is brighter than a unit vector would make it.
/// `r = 0` is the zero vector, ambient only.
#[must_use]
pub fn light_direction(light: &crate::style::ExtrusionLight, bearing_rad: f64) -> [f32; 3] {
    let [r, azimuth, polar] = light.position;
    let r = f64::from(r);
    let az = (f64::from(azimuth) + 90.0).to_radians();
    let polar = f64::from(polar).to_radians();
    let (mut x, mut y, z) = (r * az.cos() * polar.sin(), r * az.sin() * polar.sin(), r * polar.cos());
    if light.anchor == crate::style::LightAnchor::Viewport {
        let (s, c) = bearing_rad.sin_cos();
        (x, y) = (c * x - s * y, s * x + c * y);
    }
    [x as f32, y as f32, z as f32]
}

/// `light_dir` (the tile frame: x east, y south, z up, as
/// [`light_direction`] returns it) in the globe's unit-sphere space at
/// `(center_lon, center_lat)`: the frame's light, fixed at the camera's
/// centre, which a column elsewhere on the sphere meets at the angle its
/// own surface makes with it (MapLibre's `u_lightpos_globe`). The sphere
/// space is `shader.wgsl`'s `project_to_sphere`: `(sin lon cos lat, sin
/// lat, cos lon cos lat)`, so east is `(cos lon, 0, -sin lon)`, up the
/// point itself, and south `east x up`. At the centre a column's sphere
/// normal dotted with this equals its tile normal dotted with `light_dir`,
/// so the globe and flat lights agree there. `w` is 0.
#[must_use]
pub fn globe_light(light_dir: [f32; 3], center_lon_deg: f64, center_lat_deg: f64) -> [f32; 4] {
    let (sl, cl) = center_lon_deg.to_radians().sin_cos();
    let (sp, cp) = center_lat_deg.clamp(-89.9, 89.9).to_radians().sin_cos();
    let up = glam::DVec3::new(sl * cp, sp, cl * cp);
    let east = glam::DVec3::new(cl, 0.0, -sl);
    let south = east.cross(up);
    let [x, y, z] = light_dir.map(f64::from);
    let v = east * x + south * y + up * z;
    [v.x as f32, v.y as f32, v.z as f32, 0.0]
}

/// The CPU mirror of `extrusion_shader.wgsl`'s lighting, line for line in
/// f32 (fog excluded: that is the frame's, not the building's). `color` is
/// the blended table colour, `normal` the vertex's unit normal, `top` 0.0
/// or 1.0, `base`/`height` the blended metres. The alpha is `p.opacity`
/// alone, `color[3]` ignored (MapLibre); straight, as the shader returns it.
#[must_use]
pub fn lit_color(color: [f32; 4], normal: [f32; 3], top: f32, base: f32, height: f32, p: &ExtrusionParams) -> [f32; 4] {
    lit_color_toned(color, normal, top, base, height, 0x8000, p)
}

/// [`lit_color`] for a building whose [`tone_key`] is `tone`: the vertex
/// stage's colour, [`ExtrusionLook`]'s face contrast, tone and roof tint
/// included (its per-fragment occlusion, edge and haze excluded). `0x8000`
/// is the neutral tone.
#[must_use]
pub fn lit_color_toned(
    color: [f32; 4],
    normal: [f32; 3],
    top: f32,
    base: f32,
    height: f32,
    tone: u16,
    p: &ExtrusionParams,
) -> [f32; 4] {
    let jitter = 1.0 + p.look_a[3] * (f32::from(tone) / 65535.0 * 2.0 - 1.0);
    let color = [color[0] * jitter, color[1] * jitter, color[2] * jitter, color[3]];
    let maplibre = lit_color_maplibre(color, normal, top, base, height, p);
    let contrast = p.look_a[0];
    let wall = normal[2].abs() < 0.5;
    let shade = if wall { wall_shade(normal, p.light_dir) } else { ROOF_SHADE };
    let mut rgb = [0.0; 3];
    for (i, c) in rgb.iter_mut().enumerate() {
        let enhanced = (color[i] * shade * p.light_color[i]).clamp(0.0, 1.0);
        *c = maplibre[i] * (1.0 - contrast) + enhanced * contrast;
    }
    if !wall {
        let t = p.look_b[0];
        let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let k = lum(rgb) / lum(ROOF_TINT);
        for (i, c) in rgb.iter_mut().enumerate() {
            *c = *c * (1.0 - t) + (ROOF_TINT[i] * k).min(1.0) * t;
        }
    }
    [rgb[0], rgb[1], rgb[2], p.opacity]
}

/// The enhanced model's roof shade: a roof takes the full light.
pub const ROOF_SHADE: f32 = 1.0;

/// The enhanced model's wall shade (`extrusion_shader.wgsl` mirrors it):
/// ambient 0.32, a half-Lambert key light 0.62 from the light's azimuth,
/// and a half-Lambert fill 0.18 turned 100 degrees from it, so two walls
/// the key light sees alike (mirror images about it) still differ.
#[must_use]
pub fn wall_shade(normal: [f32; 3], light_dir: [f32; 3]) -> f32 {
    let len = (light_dir[0] * light_dir[0] + light_dir[1] * light_dir[1]).sqrt();
    let key = if len > 1e-4 { [light_dir[0] / len, light_dir[1] / len] } else { [0.0, -1.0] };
    let (s, c) = FILL_TURN_RAD.sin_cos();
    let fill = [c * key[0] - s * key[1], s * key[0] + c * key[1]];
    let half = |l: [f32; 2]| (normal[0] * l[0] + normal[1] * l[1]) * 0.5 + 0.5;
    0.32 + 0.62 * half(key) + 0.18 * half(fill)
}

/// How far the fill light is turned from the key light, radians (100 degrees).
pub const FILL_TURN_RAD: f32 = 1.745_329_3;

fn lit_color_maplibre(color: [f32; 4], normal: [f32; 3], top: f32, base: f32, height: f32, p: &ExtrusionParams) -> [f32; 4] {
    let mix = |a: f32, b: f32, t: f32| a * (1.0 - t) + b * t;
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let (base, height) = (base.max(0.0), height.max(0.0));
    let colorvalue = dot([color[0], color[1], color[2]], [0.2126, 0.7152, 0.0722]);
    let rgb = [color[0] + 0.03, color[1] + 0.03, color[2] + 0.03];
    let mut directional = dot(normal, p.light_dir).clamp(0.0, 1.0);
    directional = mix(1.0 - p.light_intensity, (1.0 - colorvalue + p.light_intensity).max(1.0), directional);
    let wall = if normal[2].abs() < 0.5 { 1.0 } else { 0.0 };
    let vg = p.vertical_gradient;
    let gradient =
        (1.0 - vg) + vg * ((top + base) * (height / 150.0).powf(0.5)).clamp(mix(0.7, 0.98, 1.0 - p.light_intensity), 1.0);
    directional *= mix(1.0, gradient, wall);
    let channel = |c: f32, l: f32| (c * directional * l).clamp(mix(0.0, 0.3, 1.0 - l), 1.0);
    [
        channel(rgb[0], p.light_color[0]),
        channel(rgb[1], p.light_color[1]),
        channel(rgb[2], p.light_color[2]),
        p.opacity,
    ]
}

/// Which of the extrusion pipelines a draw goes through: `Opaque`, or the
/// translucent pair, a depth-only `Prepass` then a stencilled `Color`.
///
/// Lives here, not in `extrusion_gpu` (which re-exports it), so the
/// wgpu-free `render::frame_plan` can name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtrusionPass {
    Opaque,
    Prepass,
    Color { stencil_ref: u8 },
}

/// The stencil reference the translucent colour pass of the extruded layer
/// at `layer_index` in the frame's plan uses: `layer_index mod 255 + 1`, never 0
/// (the value the stencil is cleared to, once per frame, when the tile
/// pass opens).
///
/// `group` itself is a `u16` and the stencil is 8 bits, so the group id
/// cannot be the reference: groups 1 and 257 would alias. The layer index
/// can only alias past 255 extruded layers in one frame, which no style
/// has (Liberty has 1, Mapbox Standard 2); `extrusion_plan` draws any
/// layer past the 255th opaque rather than let its reference alias.
#[must_use]
pub fn stencil_ref(layer_index: usize) -> u8 {
    (layer_index % 255 + 1) as u8
}

/// `fill-extrusion-translate` for this frame, in the map's (x east,
/// y south) logical-pixel frame the shader adds it in: a `viewport` anchor
/// is turned by `+bearing`, as MapLibre `translatePosition`
/// (`src/util/util.ts:69`) does for a non-viewport-unit caller, the same
/// `(c·x − s·y, s·x + c·y)` as [`light_direction`].
///
/// A translated building still CLIPS at its untranslated tile edge: the
/// stand-in clip box and the wall ownership rule are in tile coordinates
/// and the offset is added after them. Accepted (spec: literal translate
/// only, no vendored style translates an extrusion).
#[must_use]
pub fn frame_translate(translate: [f32; 2], anchor: crate::style::TranslateAnchor, bearing_rad: f64) -> [f32; 2] {
    if anchor == crate::style::TranslateAnchor::Map || translate == [0.0, 0.0] {
        return translate;
    }
    let (s, c) = bearing_rad.sin_cos();
    let (x, y) = (f64::from(translate[0]), f64::from(translate[1]));
    [(c * x - s * y) as f32, (s * x + c * y) as f32]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ExtrusionRow, PaintOp, PaintPlan, Rgba};
    use geo_types::{Geometry, LineString, Polygon};
    use std::sync::Arc;

    #[test]
    fn the_params_uniform_is_144_bytes() {
        assert_eq!(std::mem::size_of::<ExtrusionParams>(), 144);
    }

    /// At the centre the globe light is the tile light in sphere space:
    /// up stays up, east is `(cos lon, 0, -sin lon)`, and south points
    /// toward the south pole (negative sphere y).
    #[test]
    fn the_globe_light_is_the_tile_light_at_the_centre() {
        let close = |a: [f32; 4], b: [f32; 3]| (0..3).all(|i| (a[i] - b[i]).abs() < 1e-6);
        assert!(close(globe_light([0.0, 0.0, 1.0], 0.0, 0.0), [0.0, 0.0, 1.0]), "up at (0, 0) is +z");
        assert!(close(globe_light([1.0, 0.0, 0.0], 0.0, 0.0), [1.0, 0.0, 0.0]), "east at (0, 0) is +x");
        assert!(close(globe_light([0.0, 1.0, 0.0], 0.0, 0.0), [0.0, -1.0, 0.0]), "south at (0, 0) is -y");
        assert!(close(globe_light([0.0, 0.0, 1.0], 90.0, 0.0), [1.0, 0.0, 0.0]), "up at (90E, 0) is +x");
        let l = globe_light([0.3, -0.4, 0.8], -98.0, 39.0);
        let len = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
        assert!((len - (0.09f32 + 0.16 + 0.64).sqrt()).abs() < 1e-5, "a rotation keeps the length");
    }

    #[test]
    fn stencil_refs_never_alias_within_255_layers_and_are_never_zero() {
        let refs: std::collections::HashSet<u8> = (0..255).map(stencil_ref).collect();
        assert_eq!(refs.len(), 255);
        assert!(!refs.contains(&0), "0 is the once-per-frame clear value");
        assert_eq!(stencil_ref(0), 1);
        assert_eq!(stencil_ref(254), 255);
        assert_eq!(stencil_ref(255), 1, "wraps only past 255 layers in one frame");
    }

    #[test]
    fn a_viewport_translate_turns_with_plus_bearing_and_a_map_one_does_not() {
        use crate::style::TranslateAnchor;
        let t = [10.0, 0.0];
        assert_eq!(frame_translate(t, TranslateAnchor::Map, 1.0), t);
        // +90°: east (x) becomes south (y), MapLibre `translatePosition`.
        let v = frame_translate(t, TranslateAnchor::Viewport, std::f64::consts::FRAC_PI_2);
        assert!(v[0].abs() < 1e-5 && (v[1] - 10.0).abs() < 1e-5, "{v:?}");
        assert_eq!(frame_translate([0.0, 0.0], TranslateAnchor::Viewport, 1.0), [0.0, 0.0]);
    }

    #[test]
    fn the_default_light_points_up_and_north_east_at_bearing_0() {
        let d = light_direction(&crate::style::ExtrusionLight::default(), 0.0);
        // position [1.15, 210, 30]: az 210+90 = 300°, polar 30°.
        // x = 1.15·sin30·cos300 = 0.2875, y = 1.15·sin30·sin300 = -0.4980,
        // z = 1.15·cos30 = 0.9959: MapLibre's raw vector, length r, not unit.
        assert!((d[0] - 0.2875).abs() < 1e-3 && (d[1] + 0.4980).abs() < 1e-3 && (d[2] - 0.9959).abs() < 1e-3, "{d:?}");
    }

    #[test]
    fn a_viewport_light_at_bearing_90_turns_north_into_east() {
        // The camera faces east, so the light's north (-y) component
        // becomes east (+x): 1.15·(0.433, 0.25, 0.866).
        let d = light_direction(&crate::style::ExtrusionLight::default(), std::f64::consts::FRAC_PI_2);
        let want = [1.15 * 0.4330, 1.15 * 0.25, 1.15 * 0.8660];
        assert!(d.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-3), "{d:?} vs {want:?}");
    }

    #[test]
    fn a_longer_light_vector_lights_a_roof_brighter() {
        // MapLibre dots with the raw r-length vector: r = 1.15 gives a roof
        // dot of 0.996, r = 1.0 gives 0.866. A normalisation would make
        // these equal.
        let lum = |r: f32| {
            let light = crate::style::ExtrusionLight { position: [r, 210.0, 30.0], ..crate::style::ExtrusionLight::default() };
            let p = ExtrusionParams { light_dir: light_direction(&light, 0.0), light_intensity: 0.5,
                light_color: [1.0; 3], vertical_gradient: 0.0, translate_px: [0.0; 2], opacity: 1.0, meters_to_world: 1.0, ..ExtrusionParams::ZERO };
            let c = lit_color([0.3, 0.3, 0.3, 1.0], [0.0, 0.0, 1.0], 1.0, 0.0, 50.0, &p);
            c[0] + c[1] + c[2]
        };
        assert!(lum(1.15) > lum(1.0), "r 1.15 {} vs r 1.0 {}", lum(1.15), lum(1.0));
    }

    #[test]
    fn the_alpha_is_the_opacity_and_ignores_the_colours_alpha() {
        let p = ExtrusionParams { light_dir: light_direction(&crate::style::ExtrusionLight::default(), 0.0), light_intensity: 0.5,
            light_color: [1.0; 3], vertical_gradient: 1.0, translate_px: [0.0; 2], opacity: 0.8, meters_to_world: 1.0, ..ExtrusionParams::ZERO };
        assert_eq!(lit_color([0.5, 0.5, 0.5, 0.25], [0.0, 0.0, 1.0], 1.0, 0.0, 50.0, &p)[3], 0.8);
        assert_eq!(lit_color([0.5, 0.5, 0.5, 1.0], [1.0, 0.0, 0.0], 0.0, 0.0, 50.0, &p)[3], 0.8);
    }

    /// MapLibre's gradient term is `(top + base) * sqrt(height / 150)`, and
    /// `base` is in METRES added to the 0/1 `top` flag: for a part raised
    /// more than a metre or two the foot term alone reaches the clamp's 1,
    /// so its walls get no gradient and no darkening at all. MapLibre-exact
    /// (`fill_extrusion.vertex.glsl`); pinned so nobody "fixes" it.
    #[test]
    fn a_raised_part_gets_no_vertical_gradient() {
        let p = ExtrusionParams { light_dir: light_direction(&crate::style::ExtrusionLight::default(), 0.0), light_intensity: 0.5,
            light_color: [1.0; 3], vertical_gradient: 1.0, translate_px: [0.0; 2], opacity: 1.0, meters_to_world: 1.0, ..ExtrusionParams::ZERO };
        let off = ExtrusionParams { vertical_gradient: 0.0, ..p };
        let wall = [0.0, 1.0, 0.0];
        for top in [0.0, 1.0] {
            // base 10 m, height 50 m: (0 + 10) * sqrt(50 / 150) = 5.8 -> clamped to 1.
            assert_eq!(lit_color([0.5; 4], wall, top, 10.0, 50.0, &p), lit_color([0.5; 4], wall, top, 10.0, 50.0, &off), "top {top}");
        }
        // The same wall on the ground IS darkened: 0.84 at foot and head.
        assert!(lit_color([0.5; 4], wall, 0.0, 0.0, 50.0, &p)[0] < lit_color([0.5; 4], wall, 0.0, 0.0, 50.0, &off)[0]);
    }

    #[test]
    fn a_viewport_light_turns_with_the_bearing_and_a_map_light_does_not() {
        let viewport = crate::style::ExtrusionLight::default();
        let map = crate::style::ExtrusionLight { anchor: crate::style::LightAnchor::Map, ..viewport };
        assert_eq!(light_direction(&map, 0.0), light_direction(&map, 1.0));
        assert_ne!(light_direction(&viewport, 0.0), light_direction(&viewport, 1.0));
    }

    #[test]
    fn the_cpu_lighting_orders_walls_by_their_facing() {
        let p = ExtrusionParams { light_dir: light_direction(&crate::style::ExtrusionLight::default(), 0.0), light_intensity: 0.5,
            light_color: [1.0; 3], vertical_gradient: 0.0, translate_px: [0.0; 2], opacity: 1.0, meters_to_world: 1.0, ..ExtrusionParams::ZERO };
        let lum = |n: [f32; 3]| { let c = lit_color([0.5, 0.5, 0.5, 1.0], n, 1.0, 0.0, 50.0, &p); c[0] + c[1] + c[2] };
        let (north, east, south, west) = (lum([0.0, -1.0, 0.0]), lum([1.0, 0.0, 0.0]), lum([0.0, 1.0, 0.0]), lum([-1.0, 0.0, 0.0]));
        assert!(north > east && east > south, "N {north} E {east} S {south}");
        assert_eq!(south, west, "both face away from the light: clamp(dot) = 0");
    }

    /// Liberty's `building-3d` colour, hsl(35, 8%, 85%).
    const LIBERTY_BUILDING: [f32; 4] = [0.8668, 0.85, 0.8332, 1.0];

    fn look_params(look: ExtrusionLook, bearing_rad: f64) -> ExtrusionParams {
        let mut p = ExtrusionParams {
            light_dir: light_direction(&crate::style::ExtrusionLight::default(), bearing_rad),
            light_intensity: 0.5,
            light_color: [1.0; 3],
            vertical_gradient: 1.0,
            opacity: 1.0,
            meters_to_world: 1.0,
            ..ExtrusionParams::ZERO
        };
        look.apply(&mut p);
        p
    }

    fn levels(c: [f32; 4]) -> f32 {
        (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) * 255.0
    }

    #[test]
    fn the_maplibre_look_is_maplibre_exactly_whatever_the_tone() {
        let p = look_params(ExtrusionLook::MAPLIBRE, 0.5);
        for n in [[0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [-0.6, 0.8, 0.0]] {
            for tone in [0, 0x8000, u16::MAX] {
                assert_eq!(
                    lit_color_toned(LIBERTY_BUILDING, n, 0.0, 0.0, 80.0, tone, &p),
                    lit_color_maplibre(LIBERTY_BUILDING, n, 0.0, 0.0, 80.0, &p),
                    "{n:?} {tone}"
                );
            }
        }
    }

    /// The complaint (a): under MapLibre's light the two walls facing away
    /// from it are one shade, so a block of towers reads as one mass. The
    /// enhanced model separates every pair of adjacent faces of a box by at
    /// least 25 levels at the gallery's bearings, and keeps its darkest
    /// wall legible. (No orientation-only shading can separate EVERY pair
    /// at every bearing: `f(t) - f(t + 90)` sums to zero over a turn, so it
    /// has a zero somewhere -- see the shading's measurement notes.)
    #[test]
    fn enhanced_walls_facing_different_ways_differ_by_25_levels() {
        let faces = [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]];
        let maplibre = look_params(ExtrusionLook::MAPLIBRE, 0.0);
        let lum_m: Vec<f32> = faces.iter().map(|&n| levels(lit_color(LIBERTY_BUILDING, n, 1.0, 0.0, 150.0, &maplibre))).collect();
        assert!((lum_m[2] - lum_m[3]).abs() < 1.0, "MapLibre: S and W are one shade ({lum_m:?})");
        for bearing_deg in [30.0f64, 45.0, 60.0] {
            let p = look_params(ExtrusionLook::ENHANCED, bearing_deg.to_radians());
            let lum: Vec<f32> = faces.iter().map(|&n| levels(lit_color_toned(LIBERTY_BUILDING, n, 1.0, 0.0, 150.0, 0x8000, &p))).collect();
            for i in 0..4 {
                assert!((lum[i] - lum[(i + 1) % 4]).abs() >= 25.0, "bearing {bearing_deg}: faces {i} and {} ({lum:?})", (i + 1) % 4);
            }
            assert!(lum.iter().all(|&l| l >= 60.0), "bearing {bearing_deg}: a wall went near black ({lum:?})");
        }
    }

    #[test]
    fn the_tone_jitter_spreads_neighbours_by_a_few_percent() {
        let p = look_params(ExtrusionLook::ENHANCED, 0.0);
        let roof = |tone| levels(lit_color_toned(LIBERTY_BUILDING, [0.0, 0.0, 1.0], 1.0, 0.0, 30.0, tone, &p));
        let (dark, mid, light) = (roof(0), roof(0x8000), roof(u16::MAX));
        assert!(dark < mid && mid < light, "{dark} {mid} {light}");
        let spread = (light - dark) / mid;
        assert!((0.06..=0.14).contains(&spread), "+/-{:.1}%", spread * 50.0);
    }

    #[test]
    fn a_roof_is_tinted_cool_and_off_warm_land() {
        // Liberty's land, #f8f4f0: warm, blue lowest.
        let land = [248.0 / 255.0, 244.0 / 255.0, 240.0 / 255.0];
        let off = lit_color_toned(LIBERTY_BUILDING, [0.0, 0.0, 1.0], 1.0, 0.0, 30.0, 0x8000, &look_params(ExtrusionLook::MAPLIBRE, 0.0));
        let on = lit_color_toned(LIBERTY_BUILDING, [0.0, 0.0, 1.0], 1.0, 0.0, 30.0, 0x8000, &look_params(ExtrusionLook::ENHANCED, 0.0));
        assert!(off[2] < off[0], "MapLibre's roof is as warm as the land: {off:?}");
        assert!(on[2] > on[0] + 0.03, "the enhanced roof is cool: {on:?}");
        let dist = |c: [f32; 4]| ((c[0] - land[0]).powi(2) + (c[1] - land[1]).powi(2) + (c[2] - land[2]).powi(2)).sqrt();
        assert!(dist(on) > 0.1, "the roof stays clear of the land: {on:?}");
    }

    #[test]
    fn a_tone_key_ignores_where_the_building_is_and_splits_different_shapes() {
        let r = row(0.0, 30.0);
        let moved: Vec<(i32, i32)> = SQUARE.iter().map(|&(x, y)| (x + 4000, y - 700)).collect();
        assert_eq!(tone_key(&poly(SQUARE, &[]), &r), tone_key(&poly(&moved, &[]), &r), "a building repeated in a neighbour tile's buffer");
        let wider: Vec<(i32, i32)> = SQUARE.iter().map(|&(x, y)| (if x > 1500 { x + 40 } else { x }, y)).collect();
        assert_ne!(tone_key(&poly(SQUARE, &[]), &r), tone_key(&poly(&wider, &[]), &r));
        assert_ne!(tone_key(&poly(SQUARE, &[]), &r), tone_key(&poly(SQUARE, &[]), &row(0.0, 31.0)));
        let mesh = tessellate_extrusions(&plan(vec![(0, r, poly(SQUARE, &[]))]));
        let key = tone_key(&poly(SQUARE, &[]), &r);
        assert!(mesh.chunks[0].vertices.iter().all(|v| v.flags == key), "every vertex of a building carries its key");
    }

    #[test]
    fn the_height_grid_holds_each_roof_over_its_footprint() {
        let mesh = tessellate_extrusions(&plan(vec![(0, row(0.0, 120.0), poly(SQUARE, &[]))]));
        let grid = &mesh.height_grid;
        let at = |x: f64, y: f64| grid.at_unit(x / 4096.0, y / 4096.0);
        assert_eq!(at(1500.0, 1500.0), 120.0);
        assert_eq!(at(1010.0, 1990.0), 120.0);
        assert_eq!(at(900.0, 1500.0), 0.0);
        assert_eq!(at(2100.0, 2100.0), 0.0);
        assert_eq!(grid.max, 120.0);
    }

    /// The complaint (g): a POI behind a tower is hidden, one in front of it
    /// or far enough behind that the ray clears the roof is not, and one ON
    /// the tower (a shop inside it) is labelled on its roof.
    #[test]
    fn a_label_behind_a_tower_is_hidden_and_one_in_front_or_on_it_is_not() {
        let tile = crate::geo::TileId { z: 14, x: 8192, y: 8192 };
        let tower: &[(i32, i32)] = &[(1800, 2000), (2300, 2000), (2300, 2600), (1800, 2600)];
        let mesh = std::sync::Arc::new(tessellate_extrusions(&plan(vec![(0, row(0.0, 200.0), poly(tower, &[]))])));
        // Looking north (bearing 0) at the tile's centre: the eye is south.
        let camera = crate::render::Camera {
            center_lon: 0.011,
            center_lat: -0.011,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let m2w = crate::render::WORLD_SIZE / 40_075_016.686;
        let occlusion = BuildingOcclusion::new(&camera, m2w, vec![(tile, mesh)], &[], OcclusionLayers::default(), None);
        assert!(occlusion.hides(tile, [2048, 1500]), "300 m behind a 200 m tower");
        assert!(!occlusion.hides(tile, [2048, 3000]), "in front of it");
        assert!(!occlusion.hides(tile, [2048, 2300]), "on it: labelled on its roof");
        assert!(!occlusion.hides(tile, [2048, 300]), "so far behind the ray clears the roof");
        assert!(!occlusion.hides(tile, [600, 1500]), "beside it, off the line of sight");
        let none = BuildingOcclusion::new(&camera, m2w, Vec::new(), &[], OcclusionLayers::default(), None);
        assert!(!none.hides(tile, [2048, 1500]), "no buildings, nothing hidden");
    }

    /// Overlay extrusion layers (`with_overlay_occluders`) hide labels
    /// through the same test, with one difference: a label UNDER an overlay
    /// solid stands on the ground (a street name under a hexbin column),
    /// so it is hidden, where one inside a basemap building is lifted onto
    /// its roof. Mixed with the basemap's, the taller occluder counts, in
    /// tiles of other zooms.
    #[test]
    fn overlay_solids_hide_labels_under_and_behind_them_and_keep_those_beside() {
        let tile = crate::geo::TileId { z: 14, x: 8192, y: 8192 };
        let column: &[(i32, i32)] = &[(1800, 2000), (2300, 2000), (2300, 2600), (1800, 2600)];
        let overlay = std::sync::Arc::new(tessellate_extrusions(&plan(vec![(0, row(0.0, 200.0), poly(column, &[]))])));
        let camera = crate::render::Camera {
            center_lon: 0.011,
            center_lat: -0.011,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let m2w = crate::render::WORLD_SIZE / 40_075_016.686;
        let occlusion =
            BuildingOcclusion::new(&camera, m2w, Vec::new(), &[], OcclusionLayers::default(), None).with_overlay_occluders(vec![(tile, overlay.clone())]);
        assert!(occlusion.hides(tile, [2048, 2300]), "under the column: on the ground, hidden");
        assert!(occlusion.hides(tile, [2048, 1500]), "behind it");
        assert!(!occlusion.hides(tile, [2048, 3000]), "in front of it");
        assert!(!occlusion.hides(tile, [600, 1500]), "beside it");
        // The same solid as a basemap building: the label inside is on its roof.
        let building = BuildingOcclusion::new(&camera, m2w, vec![(tile, overlay.clone())], &[], OcclusionLayers::default(), None);
        assert!(!building.hides(tile, [2048, 2300]));
        // A basemap tile at another zoom does not mask the overlay's tile
        // (no finest-tile-wins between the two sets).
        let flat = std::sync::Arc::new(tessellate_extrusions(&plan(vec![(0, row(0.0, 0.0), poly(SQUARE, &[]))])));
        let mixed = BuildingOcclusion::new(&camera, m2w, vec![(crate::geo::TileId { z: 15, x: 16384, y: 16384 }, flat)], &[], OcclusionLayers::default(), None)
            .with_overlay_occluders(vec![(tile, overlay)]);
        assert!(mixed.hides(tile, [2048, 1500]), "behind the column, with a finer basemap tile over the area");
    }

    fn shinjuku_fixture() -> (crate::style::Style, crate::mvt::DecodedTile, crate::geo::TileId) {
        let style = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let tile = crate::mvt::decode(std::fs::read("benches/fixtures/shinjuku_z14.pbf").unwrap()).unwrap();
        (style, tile, crate::geo::TileId { z: 14, x: 14549, y: 6450 })
    }

    fn shinjuku_camera() -> crate::render::Camera {
        crate::render::Camera {
            center_lon: 139.6995,
            center_lat: 35.6905,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 30f32.to_radians(),
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    /// The old ruling, still available as `OcclusionLayers::POIS_ONLY`:
    /// label occlusion hides POIs only. On the real Shinjuku z14 tile no
    /// place label (Nishi-Shinjuku 西新宿 and the rest) is ever hidden,
    /// while POIs behind towers are; road labels and Liberty's stations
    /// (`poi_transit`) stay too.
    #[test]
    fn the_pois_only_rule_hides_pois_behind_towers_and_never_the_shinjuku_place_label() {
        let (style, tile, id) = shinjuku_fixture();
        let plan = crate::style::match_tile(&tile, &style, 16);
        let mesh = std::sync::Arc::new(tessellate_extrusions(&plan));
        let symbols = crate::style::match_tile_symbols(&tile, &style, 16, 1.0);
        let camera = shinjuku_camera();
        let m2w = (f64::from(crate::render::WORLD_SIZE) / (40_075_016.686 * 35.69f64.to_radians().cos())) as f32;
        let occlusion =
            BuildingOcclusion::new(&camera, m2w, vec![(id, mesh)], &style.layers, OcclusionLayers::POIS_ONLY, None);
        let source_layer = |i: usize| match &style.layers[i] {
            crate::style::StyleLayer::Symbol { source_layer, id, .. } => (source_layer.clone(), id.clone()),
            _ => unreachable!(),
        };
        let mut place = 0;
        let mut pois = (0, 0);
        let mut shinjuku = 0;
        for f in &symbols.features {
            let (sl, layer_id) = source_layer(f.layer_index);
            let hidden = occlusion.hides_label(id, [f.anchor.x(), f.anchor.y()], f.layer_index);
            if sl == "poi" && layer_id != "poi_transit" && f.line.is_none() {
                pois.0 += 1;
                pois.1 += usize::from(hidden);
            } else {
                assert!(!hidden, "{layer_id} ({sl}) {:?} was hidden", f.text);
                place += usize::from(sl == "place");
            }
            if sl == "place" && f.text.as_deref().is_some_and(|t| t.contains("西新宿")) {
                shinjuku += 1;
            }
        }
        assert!(shinjuku > 0, "precondition: the fixture carries the Nishi-Shinjuku place labels");
        assert!(place > 0 && pois.0 > 20, "place {place}, POIs {}", pois.0);
        assert!(pois.1 > 0 && pois.1 < pois.0, "some POIs are behind towers, not all: {} of {}", pois.1, pois.0);
        // The ward label "Shinjuku 新宿区" (class city, `label_city`) sits
        // in the neighbouring tile; put it exactly where a hidden POI is:
        // the POI hides, the place label at the same anchor does not.
        let label_city = style.layers.iter().position(|l| l.id() == "label_city").unwrap();
        let hidden_poi = symbols
            .features
            .iter()
            .find(|f| source_layer(f.layer_index).1.starts_with("poi_r") && occlusion.hides(id, [f.anchor.x(), f.anchor.y()]))
            .expect("a POI behind a tower");
        let at = [hidden_poi.anchor.x(), hidden_poi.anchor.y()];
        assert!(occlusion.hides_label(id, at, hidden_poi.layer_index), "the POI behind the tower hides");
        assert!(!occlusion.hides_label(id, at, label_city), "\"Shinjuku 新宿区\" at the same spot never hides");
    }

    /// The bug report "bus stops are visible through buildings": by
    /// default EVERY symbol whose anchor a tower hides is hidden -- POIs,
    /// stations, place names, road names and shields (line-placed ones by
    /// their anchor) -- not POIs alone. On the real Shinjuku tile each of
    /// Liberty's symbol source-layers has a symbol behind a tower, and
    /// not all of them are.
    #[test]
    fn the_default_rule_hides_every_kind_of_symbol_behind_a_tower() {
        let (style, tile, id) = shinjuku_fixture();
        let plan = crate::style::match_tile(&tile, &style, 16);
        let mesh = std::sync::Arc::new(tessellate_extrusions(&plan));
        let symbols = crate::style::match_tile_symbols(&tile, &style, 16, 1.0);
        let m2w = (f64::from(crate::render::WORLD_SIZE) / (40_075_016.686 * 35.69f64.to_radians().cos())) as f32;
        let occlusion = BuildingOcclusion::new(
            &shinjuku_camera(),
            m2w,
            vec![(id, mesh)],
            &style.layers,
            OcclusionLayers::default(),
            None,
        );
        let mut by_layer: std::collections::BTreeMap<String, (usize, usize)> = std::collections::BTreeMap::new();
        for f in &symbols.features {
            let layer = style.layers[f.layer_index].id().to_string();
            let hidden = occlusion.hides_label(id, [f.anchor.x(), f.anchor.y()], f.layer_index);
            assert_eq!(hidden, occlusion.hides(id, [f.anchor.x(), f.anchor.y()]), "{layer}: the rule tests every layer");
            let e = by_layer.entry(layer).or_default();
            e.0 += 1;
            e.1 += usize::from(hidden);
        }
        // A station (an icon), a route shield, a one-way arrow and a street
        // name: the kinds the bug report saw through buildings.
        for layer in ["poi_transit", "highway-shield-non-us", "road_one_way_arrow", "highway-name-minor"] {
            let (n, hidden) = by_layer.get(layer).copied().unwrap_or_default();
            assert!(hidden > 0, "{layer}: {hidden} of {n} hidden ({by_layer:?})");
        }
        let total: (usize, usize) = by_layer.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        assert!(total.1 > 0 && total.1 < total.0, "{total:?}");
    }

    /// What testing every symbol costs against POIs alone, on the real
    /// Shinjuku tile at the buildings panel's pose: labels tested and CPU
    /// milliseconds, unbudgeted (a capture) and budgeted (a live frame,
    /// `OCCLUSION_FRAME_SAMPLES`). Run alone, in release:
    /// `cargo test --release --lib occlusion_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing; run alone in release"]
    fn occlusion_cost_every_symbol_vs_pois_only() {
        let (style, tile, id) = shinjuku_fixture();
        let plan = crate::style::match_tile(&tile, &style, 16);
        let mesh = std::sync::Arc::new(tessellate_extrusions(&plan));
        let symbols = crate::style::match_tile_symbols(&tile, &style, 16, 1.0);
        let m2w = (f64::from(crate::render::WORLD_SIZE) / (40_075_016.686 * 35.69f64.to_radians().cos())) as f32;
        for (name, rule) in [("pois-only", OcclusionLayers::POIS_ONLY), ("every-symbol", OcclusionLayers::default())] {
            for budgeted in [false, true] {
                let mut best = f64::MAX;
                let mut tested = 0;
                let mut hidden = 0;
                for _ in 0..20 {
                    let memory = budgeted.then(OcclusionMemory::default);
                    let o = BuildingOcclusion::new(&shinjuku_camera(), m2w, vec![(id, mesh.clone())], &style.layers, rule, memory);
                    let start = crate::timing::Instant::now();
                    hidden = symbols
                        .features
                        .iter()
                        .filter(|f| o.label_visibility(id, [f.anchor.x(), f.anchor.y()], f.layer_index) == 0.0)
                        .count();
                    best = best.min(start.elapsed().as_secs_f64() * 1000.0);
                    tested = o.stats().0;
                }
                eprintln!(
                    "OCCLUSION_COST {name} budgeted={budgeted} symbols={} tested={tested} hidden={hidden} best_ms={best:.3}",
                    symbols.features.len()
                );
            }
        }
    }

    #[test]
    fn a_per_layer_rule_adds_and_removes_layers() {
        let (style, ..) = shinjuku_fixture();
        let index = |name: &str| style.layers.iter().position(|l| l.id() == name).unwrap();
        let default = OcclusionLayers::default().mask(&style.layers);
        assert!(default[index("poi_r1")] && default[index("poi_transit")] && default[index("label_city")]);
        let pois = OcclusionLayers::POIS_ONLY.mask(&style.layers);
        assert!(pois[index("poi_r1")] && !pois[index("poi_transit")] && !pois[index("label_city")]);
        let custom = OcclusionLayers { occlude: &["poi", "label_city"], keep: &["poi_r1"] }.mask(&style.layers);
        assert!(!custom[index("poi_r1")] && custom[index("poi_r7")] && custom[index("label_city")]);
    }

    /// A live map's label flips only after `OCCLUSION_FLIP_FRAMES` frames
    /// that agree, and a frame whose budget is spent keeps each label's
    /// last decision; a capture (no memory) decides afresh every time.
    #[test]
    fn occlusion_flips_after_a_streak_and_a_spent_budget_keeps_the_last_decision() {
        let tile = crate::geo::TileId { z: 14, x: 8192, y: 8192 };
        let tower: &[(i32, i32)] = &[(1800, 2000), (2300, 2000), (2300, 2600), (1800, 2600)];
        let mesh = std::sync::Arc::new(tessellate_extrusions(&plan(vec![(0, row(0.0, 200.0), poly(tower, &[]))])));
        let camera = crate::render::Camera {
            center_lon: 0.011,
            center_lat: -0.011,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let (style, ..) = shinjuku_fixture();
        let poi = style.layers.iter().position(|l| l.id() == "poi_r1").unwrap();
        let m2w = crate::render::WORLD_SIZE / 40_075_016.686;
        let (behind, open) = ([2048, 1500], [2048, 3000]);
        let frame = |memory: OcclusionMemory, anchor: [i32; 2]| {
            let o = BuildingOcclusion::new(&camera, m2w, vec![(tile, mesh.clone())], &style.layers, OcclusionLayers::default(), Some(memory));
            let hidden = o.hides_label(tile, anchor, poi);
            (hidden, o.into_memory())
        };
        // First sight decides at once.
        let (hidden, mut memory) = frame(OcclusionMemory::default(), behind);
        assert!(hidden);
        // The same key now reads visible (moved "open" anchor under the
        // behind key): simulate by forcing the raw answer through a
        // memory entry for the open anchor.
        memory.labels.insert((tile, open, poi), (true, 0));
        let mut shown = Vec::new();
        for _ in 0..OCCLUSION_FLIP_FRAMES {
            let (h, m) = frame(memory, open);
            shown.push(h);
            memory = m;
        }
        let flip = usize::from(OCCLUSION_FLIP_FRAMES) - 1;
        assert!(shown[..flip].iter().all(|&h| h) && !shown[flip], "hidden until the streak completes: {shown:?}");
        // A spent budget: the last decision stands.
        memory.labels.insert((tile, open, poi), (true, 0));
        let o = BuildingOcclusion::new(&camera, m2w, vec![(tile, mesh.clone())], &style.layers, OcclusionLayers::default(), Some(memory));
        o.budget.set(0);
        assert!(o.hides_label(tile, open, poi), "no budget left: the remembered 'hidden' stands");
        // No memory: afresh, every time.
        let fresh = BuildingOcclusion::new(&camera, m2w, vec![(tile, mesh.clone())], &style.layers, OcclusionLayers::default(), None);
        assert!(fresh.hides_label(tile, behind, poi) && !fresh.hides_label(tile, open, poi));
    }

    fn tower_scene() -> (crate::render::Camera, crate::geo::TileId, std::sync::Arc<ExtrusionMesh>, crate::style::Style, usize, f32) {
        let tile = crate::geo::TileId { z: 14, x: 8192, y: 8192 };
        let tower: &[(i32, i32)] = &[(1800, 2000), (2300, 2000), (2300, 2600), (1800, 2600)];
        let mesh = std::sync::Arc::new(tessellate_extrusions(&plan(vec![(0, row(0.0, 200.0), poly(tower, &[]))])));
        let camera = crate::render::Camera {
            center_lon: 0.011,
            center_lat: -0.011,
            zoom: 16.0,
            pitch: 60f32.to_radians(),
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let (style, ..) = shinjuku_fixture();
        let poi = style.layers.iter().position(|l| l.id() == "poi_r1").unwrap();
        (camera, tile, mesh, style, poi, crate::render::WORLD_SIZE / 40_075_016.686)
    }

    /// N1: after the camera stops, pending flips settle by frames alone --
    /// `pending()` is what makes the map request them -- and then nothing
    /// is pending.
    #[test]
    fn pending_flips_settle_with_no_further_input() {
        let (camera, tile, mesh, style, poi, m2w) = tower_scene();
        let open = [2048, 3000];
        let mut memory = OcclusionMemory::default();
        // It was hidden while the camera moved; now, still, it is not.
        memory.labels.insert((tile, open, poi), (true, 0));
        let mut frames = 0;
        let mut hidden;
        loop {
            let o = BuildingOcclusion::new(&camera, m2w, vec![(tile, mesh.clone())], &style.layers, OcclusionLayers::default(), Some(memory));
            hidden = o.hides_label(tile, open, poi);
            memory = o.into_memory();
            frames += 1;
            if !memory.pending() || frames > 10 {
                break;
            }
        }
        assert!(!hidden && !memory.pending(), "settled visible after {frames} frames");
        assert!(frames <= usize::from(OCCLUSION_FLIP_FRAMES), "{frames} frames");
    }

    /// A budget that covers one label a frame still reaches every label:
    /// each frame starts where the last one ran out.
    #[test]
    fn a_starved_budget_rotates_so_no_label_starves() {
        let (camera, tile, mesh, style, poi, m2w) = tower_scene();
        let anchors: Vec<[i32; 2]> = (0..6).map(|k| [300 + 600 * k, 1500]).collect();
        let mut memory = OcclusionMemory::default();
        // Every label remembered as the opposite of the truth.
        let fresh = BuildingOcclusion::new(&camera, m2w, vec![(tile, mesh.clone())], &style.layers, OcclusionLayers::default(), None);
        let truth: Vec<bool> = anchors.iter().map(|&a| fresh.hides(tile, a)).collect();
        for (a, t) in anchors.iter().zip(&truth) {
            memory.labels.insert((tile, *a, poi), (!t, 0));
        }
        for _ in 0..(anchors.len() * usize::from(OCCLUSION_FLIP_FRAMES) * 2) {
            let o = BuildingOcclusion::new(&camera, m2w, vec![(tile, mesh.clone())], &style.layers, OcclusionLayers::default(), Some(memory));
            o.budget.set(1);
            for a in &anchors {
                let _ = o.hides_label(tile, *a, poi);
            }
            memory = o.into_memory();
        }
        for (a, t) in anchors.iter().zip(&truth) {
            assert_eq!(memory.labels[&(tile, *a, poi)].0, *t, "{a:?} starved");
        }
    }

    /// A finer loaded tile answers for the ground it covers; its stand-in
    /// ancestor answers only where no finer tile is.
    #[test]
    fn the_finest_tile_answers_where_a_stand_in_overlaps_it() {
        let child = crate::geo::TileId { z: 14, x: 8192, y: 8192 };
        let parent = crate::geo::TileId { z: 13, x: 4096, y: 4096 };
        let tall = std::sync::Arc::new(tessellate_extrusions(&plan(vec![(0, row(0.0, 300.0), poly(&[(0, 0), (4095, 0), (4095, 4095), (0, 4095)], &[]))])));
        let low = std::sync::Arc::new(tessellate_extrusions(&plan(vec![(0, row(0.0, 10.0), poly(&[(0, 0), (4095, 0), (4095, 4095), (0, 4095)], &[]))])));
        let camera = crate::render::Camera { center_lon: 0.01, center_lat: -0.01, zoom: 15.0, pitch: 1.0, bearing: 0.0, viewport_width_px: 512.0, viewport_height_px: 512.0, fov_y: crate::render::Camera::DEFAULT_FOV_Y };
        let o = BuildingOcclusion::new(&camera, 1.0, vec![(parent, low), (child, tall)], &[], OcclusionLayers::default(), None);
        let size = f64::from(crate::render::WORLD_SIZE) / f64::from(1u32 << 14);
        let inside_child = glam::DVec2::new((8192.5) * size, (8192.5) * size);
        let outside_child = glam::DVec2::new((8193.5) * size, (8193.5) * size);
        assert_eq!(o.height_at(inside_child), 300.0);
        assert_eq!(o.height_at(outside_child), 10.0);
        // The ray march's tile hint answers exactly as a fresh lookup does,
        // walking in and out of the child and across the parent's other
        // quadrants: a hint is only ever the finest tile, and is never
        // trusted for a point outside it.
        let mut hint = None;
        for k in 0..400 {
            let t = f64::from(k) / 400.0;
            let p = glam::DVec2::new((8191.6 + 2.2 * t) * size, (8192.4 + 1.3 * (t * 7.0).sin()) * size);
            assert_eq!(o.height_at_hinted(p, &mut hint).to_bits(), o.height_at(p).to_bits(), "step {k}");
        }
        assert!(hint.is_some_and(|(tile, _)| tile == child), "the march passed through the child, so the hint holds it");
    }

    const GREY: Rgba = Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };

    fn row(base: f32, height: f32) -> ExtrusionRow {
        ExtrusionRow { color_from: GREY, color_to: GREY, base_from: base, height_from: height, base_to: base, height_to: height }
    }
    fn ring(points: &[(i32, i32)]) -> LineString<i32> {
        let mut v: Vec<(i32, i32)> = points.to_vec();
        v.push(points[0]);
        LineString::from(v)
    }
    fn poly(exterior: &[(i32, i32)], holes: &[&[(i32, i32)]]) -> Geometry<i32> {
        Geometry::Polygon(Polygon::new(ring(exterior), holes.iter().map(|h| ring(h)).collect()))
    }
    fn plan(ops: Vec<(u16, ExtrusionRow, Geometry<i32>)>) -> PaintPlan {
        PaintPlan { ops: ops.into_iter().map(|(group, row, g)| PaintOp::Extrusion { group, row, geometry: Arc::new(g) }).collect() }
    }
    const SQUARE: &[(i32, i32)] = &[(1000, 1000), (2000, 1000), (2000, 2000), (1000, 2000)];

    #[test]
    fn a_box_has_four_walls_and_a_two_triangle_roof() {
        let mesh = tessellate_extrusions(&plan(vec![(0, row(0.0, 30.0), poly(SQUARE, &[]))]));
        assert_eq!(mesh.chunks.len(), 1);
        assert_eq!(mesh.wall_count(), 4);
        assert_eq!(mesh.vertex_count(), 4 + 16, "4 roof vertices + 4 per wall");
        assert_eq!(mesh.index_count(), 3 * 2 + 6 * 4, "2 roof triangles + 2 per wall");
    }

    #[test]
    fn wall_normals_point_away_from_the_solid_whatever_the_input_winding() {
        let ccw = wall_segments(&poly(SQUARE, &[]));
        let reversed: Vec<(i32, i32)> = SQUARE.iter().rev().copied().collect();
        let cw = wall_segments(&poly(&reversed, &[]));
        for walls in [&ccw, &cw] {
            assert_eq!(walls.len(), 4);
            for w in walls.iter() {
                let mid = [(w.p1[0] + w.p2[0]) as f32 / 2.0 - 1500.0, (w.p1[1] + w.p2[1]) as f32 / 2.0 - 1500.0];
                assert!(w.normal[0] * mid[0] + w.normal[1] * mid[1] > 0.0, "{w:?} faces inward");
            }
        }
    }

    #[test]
    fn a_hole_gets_walls_facing_into_the_hole() {
        let hole: &[(i32, i32)] = &[(1400, 1400), (1600, 1400), (1600, 1600), (1400, 1600)];
        let walls = wall_segments(&poly(SQUARE, &[hole]));
        assert_eq!(walls.len(), 8);
        let inner: Vec<_> = walls.iter().filter(|w| w.p1[0] >= 1400 && w.p1[0] <= 1600 && w.p1[1] >= 1400 && w.p1[1] <= 1600).collect();
        assert_eq!(inner.len(), 4);
        for w in inner {
            let mid = [(w.p1[0] + w.p2[0]) as f32 / 2.0 - 1500.0, (w.p1[1] + w.p2[1]) as f32 / 2.0 - 1500.0];
            assert!(w.normal[0] * mid[0] + w.normal[1] * mid[1] < 0.0, "hole wall {w:?} must face the hole's centre");
        }
    }

    #[test]
    fn walls_on_the_tile_boundary_belong_to_one_tile() {
        let on_x0 = wall_segments(&poly(&[(0, 100), (200, 100), (200, 300), (0, 300)], &[]));
        assert!(on_x0.iter().any(|w| w.p1[0] == 0 && w.p2[0] == 0), "x = 0 is kept");
        let on_x_e = wall_segments(&poly(&[(3900, 100), (4096, 100), (4096, 300), (3900, 300)], &[]));
        assert!(!on_x_e.iter().any(|w| w.p1[0] == 4096 && w.p2[0] == 4096), "x = 4096 is dropped");
        let on_y0 = wall_segments(&poly(&[(100, 0), (300, 0), (300, 200), (100, 200)], &[]));
        assert!(on_y0.iter().any(|w| w.p1[1] == 0 && w.p2[1] == 0), "y = 0 is kept");
        let on_y_e = wall_segments(&poly(&[(100, 3900), (300, 3900), (300, 4096), (100, 4096)], &[]));
        assert!(!on_y_e.iter().any(|w| w.p1[1] == 4096 && w.p2[1] == 4096), "y = 4096 is dropped");
    }

    #[test]
    fn the_producers_buffer_cut_makes_no_wall() {
        // MapLibre's isBoundaryEdge case: the producer clipped at x = -64.
        let walls = wall_segments(&poly(&[(-64, 100), (300, 100), (300, 300), (-64, 300)], &[]));
        assert!(!walls.iter().any(|w| w.p1[0] < 0 || w.p2[0] < 0), "{walls:?}");
        assert!(!walls.iter().any(|w| w.p1[0] == 0 && w.p2[0] == 0), "no wall at the clip line either");
    }

    #[test]
    fn a_building_across_a_seam_has_the_same_walls_as_one_tile_would() {
        // West tile coordinates; the east tile sees the same building shifted by -4096.
        let west = poly(&[(3000, 1000), (5000, 1000), (5000, 2000), (3000, 2000)], &[]);
        let east = poly(&[(-1096, 1000), (904, 1000), (904, 2000), (-1096, 2000)], &[]);
        let mut world: Vec<WallSegment> = wall_segments(&west);
        world.extend(wall_segments(&east).into_iter().map(|w| WallSegment { p1: [w.p1[0] + 4096, w.p1[1]], p2: [w.p2[0] + 4096, w.p2[1]], ..w }));
        assert!(!world.iter().any(|w| w.p1[0] == 4096 && w.p2[0] == 4096), "no wall at the seam");
        let length: f64 = world.iter().map(|w| (((w.p2[0] - w.p1[0]) as f64).powi(2) + ((w.p2[1] - w.p1[1]) as f64).powi(2)).sqrt()).sum();
        assert_eq!(length, 2.0 * (2000.0 + 1000.0), "the perimeter, drawn exactly once across both tiles");
    }

    #[test]
    fn height_at_or_below_base_is_a_roof_only() {
        let mesh = tessellate_extrusions(&plan(vec![(0, row(20.0, 10.0), poly(SQUARE, &[]))]));
        assert_eq!(mesh.wall_count(), 0);
        assert_eq!(mesh.index_count(), 6);
        // R15: the roof sits at max(base, height).
        let t = &mesh.chunks[0].table;
        assert_eq!(t[2], [20.0, 20.0, 20.0, 20.0]);
    }

    #[test]
    fn a_tile_with_only_zero_heights_has_roofs_and_no_walls() {
        let mesh = tessellate_extrusions(&plan(vec![(0, row(0.0, 0.0), poly(SQUARE, &[])), (0, row(0.0, 0.0), poly(SQUARE, &[]))]));
        assert_eq!(mesh.wall_count(), 0);
        assert_eq!(mesh.index_count(), 12);
    }

    #[test]
    fn an_empty_plan_makes_an_empty_mesh() {
        let mesh = tessellate_extrusions(&PaintPlan { ops: vec![] });
        assert!(mesh.is_empty());
        assert!(mesh.chunks.is_empty(), "no chunk, so nothing to upload");
    }

    #[test]
    fn zero_length_and_closing_edges_make_no_wall() {
        // A duplicated point, and a ring that already repeats its first point
        // (ring() appends it again): neither may produce a wall or a NaN normal.
        let dup = poly(&[(1000, 1000), (2000, 1000), (2000, 1000), (2000, 2000), (1000, 2000), (1000, 1000)], &[]);
        let walls = wall_segments(&dup);
        assert_eq!(walls.len(), 4);
        assert!(walls.iter().all(|w| w.normal[0].is_finite() && w.normal[1].is_finite()));
    }

    #[test]
    fn roof_triangles_share_one_winding() {
        let l_shape = poly(&[(0, 0), (3000, 0), (3000, 1000), (1000, 1000), (1000, 3000), (0, 3000)], &[]);
        let mesh = tessellate_extrusions(&plan(vec![(0, row(0.0, 0.0), l_shape)]));
        let chunk = &mesh.chunks[0];
        let idx: Vec<u32> = match &chunk.indices { crate::render::compact::IndexList::U16(v) => v.iter().map(|&i| i as u32).collect(), crate::render::compact::IndexList::U32(v) => v.clone() };
        for t in idx.chunks(3) {
            let [a, b, c] = [chunk.vertices[t[0] as usize].pos, chunk.vertices[t[1] as usize].pos, chunk.vertices[t[2] as usize].pos];
            let area = (b[0] as i64 - a[0] as i64) * (c[1] as i64 - a[1] as i64) - (b[1] as i64 - a[1] as i64) * (c[0] as i64 - a[0] as i64);
            assert!(area > 0, "roof triangle {t:?} has area {area}");
        }
    }

    #[test]
    fn every_wall_triangle_faces_out_of_the_solid() {
        let mesh = tessellate_extrusions(&plan(vec![(0, row(0.0, 30.0), poly(SQUARE, &[]))]));
        let chunk = &mesh.chunks[0];
        let idx: Vec<u32> = match &chunk.indices { crate::render::compact::IndexList::U16(v) => v.iter().map(|&i| i as u32).collect(), crate::render::compact::IndexList::U32(v) => v.clone() };
        for t in idx.chunks(3) {
            let p = |i: u32| { let v = chunk.vertices[i as usize]; [v.pos[0] as f32, v.pos[1] as f32, v.normal_top[3] as f32 * 1000.0] };
            let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
            let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let w = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let n = [u[1] * w[2] - u[2] * w[1], u[2] * w[0] - u[0] * w[2], u[0] * w[1] - u[1] * w[0]];
            let v = chunk.vertices[t[0] as usize].normal_top;
            let outward = [v[0] as f32, v[1] as f32, v[2] as f32];
            assert!(n[0] * outward[0] + n[1] * outward[1] + n[2] * outward[2] > 0.0, "triangle {t:?}");
        }
    }

    #[test]
    fn the_vertex_is_12_bytes() {
        assert_eq!(std::mem::size_of::<ExtrusionVertex>(), 12);
    }

    #[test]
    fn a_tile_over_65536_features_splits_its_table() {
        let tiny = || poly(&[(10, 10), (12, 10), (12, 12)], &[]);
        let ops = (0..MAX_ROWS_PER_CHUNK + 1).map(|_| (0u16, row(0.0, 0.0), tiny())).collect();
        let mesh = tessellate_extrusions(&plan(ops));
        assert_eq!(mesh.chunks.len(), 2);
        assert_eq!(mesh.chunks[0].table.len(), MAX_ROWS_PER_CHUNK * TABLE_TEXELS_PER_ROW);
        assert_eq!(mesh.chunks[1].table.len(), TABLE_TEXELS_PER_ROW);
    }

    #[test]
    fn group_spans_cover_each_groups_indices() {
        let mesh = tessellate_extrusions(&plan(vec![(0, row(0.0, 10.0), poly(SQUARE, &[])), (1, row(0.0, 10.0), poly(SQUARE, &[]))]));
        let spans = &mesh.chunks[0].spans;
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].group, spans[0].first_index, spans[0].index_count), (0, 0, 30));
        assert_eq!((spans[1].group, spans[1].first_index, spans[1].index_count), (1, 30, 30));
    }

    #[test]
    fn a_colour_change_rewrites_the_table_and_keeps_the_vertices() {
        let geometry = poly(SQUARE, &[]);
        let mut mesh = tessellate_extrusions(&plan(vec![(0, row(0.0, 10.0), geometry.clone())]));
        let vertices = mesh.chunks[0].vertices.clone();
        let mut red = row(0.0, 10.0);
        red.color_from = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        assert!(rewrite_table(&mut mesh, &plan(vec![(0, red, geometry)])));
        assert_eq!(mesh.chunks[0].vertices, vertices);
        assert_eq!(mesh.chunks[0].table[0], [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_plan_with_a_different_feature_count_leaves_the_table_alone() {
        let geometry = poly(SQUARE, &[]);
        let mut mesh = tessellate_extrusions(&plan(vec![(0, row(0.0, 10.0), geometry.clone()), (0, row(0.0, 20.0), geometry.clone())]));
        let before = mesh.chunks[0].table.clone();
        let mut red = row(0.0, 10.0);
        red.color_from = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        assert!(!rewrite_table(&mut mesh, &plan(vec![(0, red, geometry.clone())])), "fewer ops");
        assert_eq!(mesh.chunks[0].table, before, "untouched after a short plan");
        let three = plan(vec![(0, red, geometry.clone()), (0, red, geometry.clone()), (0, red, geometry)]);
        assert!(!rewrite_table(&mut mesh, &three), "more ops");
        assert_eq!(mesh.chunks[0].table, before, "untouched after a long plan");
        assert!(!rewrite_table(&mut ExtrusionMesh::default(), &three), "an empty mesh and a non-empty plan");
        assert!(rewrite_table(&mut ExtrusionMesh::default(), &PaintPlan { ops: vec![] }), "nothing to nothing");
    }

    /// A chunk's triangles as vertex-index triples.
    fn triangles_of(chunk: &ExtrusionChunk) -> Vec<[usize; 3]> {
        let flat: Vec<usize> = match &chunk.indices {
            IndexList::U16(v) => v.iter().map(|&i| usize::from(i)).collect(),
            IndexList::U32(v) => v.iter().map(|&i| i as usize).collect(),
        };
        flat.as_chunks::<3>().0.to_vec()
    }

    /// Small features, whatever grid line they straddle, are tessellated
    /// exactly as with no grid: a hexagon over the z0 grid's x = 1024 line
    /// and a 60-unit square on the cell corner at (1024, 1024).
    #[test]
    fn a_feature_within_one_cell_is_cut_on_no_grid() {
        let hex: Vec<(i32, i32)> = (0..=6).map(|k| {
            let a = std::f64::consts::PI / 3.0 * f64::from(k % 6);
            ((1024.0 + 30.0 * a.cos()).round() as i32, (700.0 + 30.0 * a.sin()).round() as i32)
        }).collect();
        let small = plan(vec![
            (0, row(0.0, 40.0), poly(&hex, &[])),
            (0, row(0.0, 40.0), poly(&[(994, 994), (1054, 994), (1054, 1054), (994, 1054), (994, 994)], &[])),
        ]);
        let plain = tessellate_extrusions(&small);
        for granularity in [2, 16, 64] {
            let gridded = tessellate_extrusions_on_grid(&small, granularity);
            assert_eq!(gridded.chunks[0].vertices, plain.chunks[0].vertices, "granularity {granularity}");
            assert_eq!(gridded.chunks[0].indices, plain.chunks[0].indices, "granularity {granularity}");
            assert_eq!(gridded.wall_count(), plain.wall_count());
        }
    }

    /// A feature wider than a cell is cut so that no roof triangle and no
    /// wall quad spans more than one cell; the roof covers the same area,
    /// is watertight (every edge inside it is shared by exactly two
    /// triangles, so a cut made no T-junction), and every wall's top
    /// corner is a roof vertex, so the walls meet the roof.
    #[test]
    fn a_feature_wider_than_a_cell_is_cut_into_cells_with_no_crack() {
        // A slanted quadrilateral with a hole, over many 256-unit cells.
        let outer = [(300, 200), (3900, 700), (3500, 3800), (100, 3300), (300, 200)];
        let hole = [(1500, 1500), (1500, 2300), (2400, 2300), (2400, 1500), (1500, 1500)];
        let p = plan(vec![(0, row(0.0, 50_000.0), poly(&outer, &[&hole]))]);
        let plain = tessellate_extrusions(&p);
        let cut = tessellate_extrusions_on_grid(&p, 16);
        let cell = 256;
        let chunk = &cut.chunks[0];
        let (roof, walls): (Vec<[usize; 3]>, Vec<[usize; 3]>) = triangles_of(chunk).into_iter().partition(|t| chunk.vertices[t[0]].normal_top[2] == 127);
        let pos = |i: usize| chunk.vertices[i].pos.map(i64::from);
        assert!(roof.len() > 10 * (triangles_of(&plain.chunks[0]).len() - 2 * plain.wall_count()), "the roof is cut: {} triangles", roof.len());
        for t in &roof {
            for axis in 0..2 {
                let (lo, hi) = (t.iter().map(|&i| pos(i)[axis]).min().unwrap(), t.iter().map(|&i| pos(i)[axis]).max().unwrap());
                assert!(hi - lo <= cell, "roof triangle {t:?} spans {} units on axis {axis}", hi - lo);
            }
        }
        for t in &walls {
            for axis in 0..2 {
                let (lo, hi) = (t.iter().map(|&i| pos(i)[axis]).min().unwrap(), t.iter().map(|&i| pos(i)[axis]).max().unwrap());
                assert!(hi - lo <= cell, "wall triangle spans {} units on axis {axis}", hi - lo);
            }
        }
        let area = |tris: &[[usize; 3]], at: &dyn Fn(usize) -> [i64; 2]| -> i64 {
            tris.iter().map(|t| {
                let (a, b, c) = (at(t[0]), at(t[1]), at(t[2]));
                ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs()
            }).sum()
        };
        let plain_chunk = &plain.chunks[0];
        let plain_roof: Vec<[usize; 3]> = triangles_of(plain_chunk).into_iter().filter(|t| plain_chunk.vertices[t[0]].normal_top[2] == 127).collect();
        let (a_cut, a_plain) = (area(&roof, &pos), area(&plain_roof, &|i| plain_chunk.vertices[i].pos.map(i64::from)));
        assert!((a_cut - a_plain).abs() * 1000 < a_plain, "same roof area: {a_cut} vs {a_plain} (twice the area)");
        // Watertight: an edge seen once is on the outline (or the hole),
        // never inside the roof.
        let mut edges: std::collections::HashMap<([i64; 2], [i64; 2]), usize> = std::collections::HashMap::new();
        for t in &roof {
            for k in 0..3 {
                let (a, b) = (pos(t[k]), pos(t[(k + 1) % 3]));
                *edges.entry(if a < b { (a, b) } else { (b, a) }).or_default() += 1;
            }
        }
        let on_outline = |p: [i64; 2]| {
            [&outer[..], &hole[..]].iter().any(|ring| ring.windows(2).any(|w| {
                let (a, b) = ([i64::from(w[0].0), i64::from(w[0].1)], [i64::from(w[1].0), i64::from(w[1].1)]);
                let (dx, dy) = ((b[0] - a[0]) as f64, (b[1] - a[1]) as f64);
                let cross = (dx * (p[1] - a[1]) as f64 - dy * (p[0] - a[0]) as f64).abs() / dx.hypot(dy);
                let along = (dx * (p[0] - a[0]) as f64 + dy * (p[1] - a[1]) as f64) / dx.hypot(dy);
                cross <= 1.0 && along >= -1.0 && along <= dx.hypot(dy) + 1.0
            }))
        };
        for (&(a, b), &n) in &edges {
            assert!(n <= 2, "edge {a:?}-{b:?} used {n} times");
            if n == 1 {
                assert!(on_outline(a) && on_outline(b), "an open edge {a:?}-{b:?} inside the roof: a crack");
            }
        }
        let roof_vertices: std::collections::HashSet<[i16; 2]> = roof.iter().flatten().map(|&i| chunk.vertices[i].pos).collect();
        let tops: Vec<[i16; 2]> = walls.iter().flatten().filter(|&&i| chunk.vertices[i].normal_top[3] == 127).map(|&i| chunk.vertices[i].pos).collect();
        let missing = tops.iter().filter(|p| !roof_vertices.contains(*p)).count();
        assert_eq!(missing, 0, "every wall top corner is a roof vertex ({missing} of {} are not)", tops.len());
        assert!(cut.wall_count() > 4 * plain.wall_count(), "the walls are cut: {} vs {}", cut.wall_count(), plain.wall_count());
    }

    #[test]
    fn rings_without_area_make_no_wall() {
        // Two distinct points (a back-and-forth sliver), and three collinear ones.
        let sliver = poly(&[(1000, 1000), (2000, 1000)], &[]);
        assert!(wall_segments(&sliver).is_empty());
        let collinear = poly(&[(1000, 1000), (1500, 1000), (2000, 1000)], &[]);
        assert!(wall_segments(&collinear).is_empty());
        let point = poly(&[(1000, 1000), (1000, 1000), (1000, 1000)], &[]);
        assert!(wall_segments(&point).is_empty());
        // A zero-area hole is skipped; the exterior keeps its four walls.
        let with_sliver_hole = poly(SQUARE, &[&[(1200, 1200), (1400, 1200)]]);
        assert_eq!(wall_segments(&with_sliver_hole).len(), 4);
    }

    /// `world` (a polygon in the coordinates of tile (0, 0), which may run
    /// into its neighbours) cut into each of `tiles`: every tile's walls,
    /// shifted back to world coordinates.
    fn walls_across(world: &[(i32, i32)], tiles: &[(i32, i32)]) -> Vec<WallSegment> {
        let mut out = Vec::new();
        for &(tx, ty) in tiles {
            let local: Vec<(i32, i32)> = world.iter().map(|&(x, y)| (x - tx * EXTENT, y - ty * EXTENT)).collect();
            out.extend(wall_segments(&poly(&local, &[])).into_iter().map(|w| WallSegment {
                p1: [w.p1[0] + tx * EXTENT, w.p1[1] + ty * EXTENT],
                p2: [w.p2[0] + tx * EXTENT, w.p2[1] + ty * EXTENT],
                ..w
            }));
        }
        out
    }

    fn perimeter(world: &[(i32, i32)]) -> f64 {
        (0..world.len())
            .map(|i| {
                let (a, b) = (world[i], world[(i + 1) % world.len()]);
                f64::from(b.0 - a.0).hypot(f64::from(b.1 - a.1))
            })
            .sum()
    }

    fn total_length(walls: &[WallSegment]) -> f64 {
        walls.iter().map(|w| f64::from(w.p2[0] - w.p1[0]).hypot(f64::from(w.p2[1] - w.p1[1]))).sum()
    }

    /// Asserts `world`'s walls, gathered over `tiles`, add up to its
    /// perimeter, and that no piece of wall appears twice.
    fn assert_drawn_once(world: &[(i32, i32)], tiles: &[(i32, i32)]) -> Vec<WallSegment> {
        let walls = walls_across(world, tiles);
        assert!((total_length(&walls) - perimeter(world)).abs() < 1e-6, "{world:?}: {} vs {}: {walls:?}", total_length(&walls), perimeter(world));
        for (i, a) in walls.iter().enumerate() {
            for b in &walls[i + 1..] {
                let same = (a.p1 == b.p1 && a.p2 == b.p2) || (a.p1 == b.p2 && a.p2 == b.p1);
                assert!(!same, "{a:?} drawn twice");
            }
        }
        walls
    }

    fn on_line(walls: &[WallSegment], axis: usize, at: i32) -> Vec<WallSegment> {
        walls.iter().filter(|w| w.p1[axis] == at && w.p2[axis] == at).copied().collect()
    }

    #[test]
    fn a_real_wall_on_an_east_west_seam_is_drawn_once() {
        let tiles = [(0, 0), (1, 0)];
        // A building whose EAST wall is the seam, and one whose WEST wall is.
        for world in [
            &[(3000, 1000), (4096, 1000), (4096, 2000), (3000, 2000)][..],
            &[(4096, 1000), (5000, 1000), (5000, 2000), (4096, 2000)][..],
        ] {
            let walls = assert_drawn_once(world, &tiles);
            let seam = on_line(&walls, 0, EXTENT);
            assert_eq!(seam.len(), 1, "{world:?}: {walls:?}");
            assert_eq!((seam[0].p2[1] - seam[0].p1[1]).abs(), 1000);
        }
    }

    #[test]
    fn a_real_wall_on_a_north_south_seam_is_drawn_once() {
        let tiles = [(0, 0), (0, 1)];
        for world in [
            &[(1000, 3000), (2000, 3000), (2000, 4096), (1000, 4096)][..],
            &[(1000, 4096), (2000, 4096), (2000, 5000), (1000, 5000)][..],
        ] {
            let walls = assert_drawn_once(world, &tiles);
            let seam = on_line(&walls, 1, EXTENT);
            assert_eq!(seam.len(), 1, "{world:?}: {walls:?}");
            assert_eq!((seam[0].p2[0] - seam[0].p1[0]).abs(), 1000);
        }
    }

    #[test]
    fn walls_at_a_four_tile_corner_are_drawn_once() {
        let tiles = [(0, 0), (1, 0), (0, 1), (1, 1)];
        // A square straddling the corner, one with walls ON both seams, and a
        // triangle whose hypotenuse runs exactly through (4096, 4096).
        assert_drawn_once(&[(3500, 3500), (4700, 3500), (4700, 4700), (3500, 4700)], &tiles);
        let on_seams = assert_drawn_once(&[(4096, 4096), (5000, 4096), (5000, 5000), (4096, 5000)], &tiles);
        assert_eq!(on_line(&on_seams, 0, EXTENT).len(), 1);
        assert_eq!(on_line(&on_seams, 1, EXTENT).len(), 1);
        assert_drawn_once(&[(3000, 3000), (5000, 5000), (3000, 5000)], &tiles);
    }

    /// The spec's worst measured tile (§5.1) must stay under the budget
    /// (§5.2): a tessellation change that pushed Shinjuku past it would
    /// otherwise only show in the ignored printer below.
    #[test]
    fn shinjuku_z14_stays_under_the_vertex_budget() {
        let style = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let tile = crate::mvt::decode(std::fs::read("benches/fixtures/shinjuku_z14.pbf").unwrap()).unwrap();
        let mesh = tessellate_extrusions(&crate::style::match_tile(&tile, &style, 14));
        assert!(mesh.vertex_count() > 200_000, "the fixture still extrudes: {}", mesh.vertex_count());
        assert!(!mesh.over_budget(), "{} vertices > {EXTRUSION_VERTEX_BUDGET}", mesh.vertex_count());
    }

    /// The fixture numbers `docs/measurements/2026-09-29-building-extrusion.md`
    /// records: Liberty, matched at z14, tessellated.
    #[test]
    #[ignore = "prints the fixture tessellation numbers for the measurement doc"]
    fn print_fixture_extrusion_counts() {
        let style = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        for fixture in ["shinjuku_z14", "manhattan_z14", "singapore_z14"] {
            let path = format!("benches/fixtures/{fixture}.pbf");
            let Ok(bytes) = std::fs::read(&path) else {
                println!("{fixture}: no fixture at {path}");
                continue;
            };
            let tile = crate::mvt::decode(bytes).unwrap();
            let plan = crate::style::match_tile(&tile, &style, 14);
            let ops = plan.ops.iter().filter(|op| matches!(op, PaintOp::Extrusion { .. })).count();
            let started = crate::timing::Instant::now();
            let mesh = tessellate_extrusions(&plan);
            let elapsed = started.elapsed();
            let roof_vertices: usize = mesh.chunks.iter().map(|c| c.vertices.len() - 4 * c.walls).sum();
            let triangles = mesh.index_count() / 3;
            println!(
                "{fixture}: ops {ops} chunks {} vertices {} (roof {roof_vertices}, wall {}) indices {} triangles {triangles} (wall {}, roof {}) walls {} upload_bytes {} u32 {} over_budget {} tessellate {:.1} ms",
                mesh.chunks.len(),
                mesh.vertex_count(),
                4 * mesh.wall_count(),
                mesh.index_count(),
                2 * mesh.wall_count(),
                triangles - 2 * mesh.wall_count(),
                mesh.wall_count(),
                mesh.upload_bytes(),
                mesh.chunks.iter().any(|c| matches!(c.indices, IndexList::U32(_))),
                mesh.over_budget(),
                elapsed.as_secs_f64() * 1e3,
            );
        }
    }
}
