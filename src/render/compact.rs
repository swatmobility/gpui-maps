//! The 8-byte tile-local vertex for fills and hairlines: `i16x2` position in
//! MVT units, a `u16` paint index into a per-draw paint table, `u16` flags.
//!
//! MapLibre's fill layout is `a_pos` Int16x2 (`fill_attributes.ts:4`) with
//! paint in separate arrays; ours puts paint in a table the vertex indexes,
//! read by `shader.wgsl`'s `vs_compact` with a vertex-stage `textureLoad`.
//!
//! A `CompactVertex` is converted vertex for vertex from a baked
//! [`TileMesh`], so the fill's index list and the A2 hairline's line list
//! carry over unchanged: the outline adds no vertices.

use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
use crate::render::mesh::{OpSpan, TileMesh, COORD_SCALE};
use crate::style::Rgba;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CompactVertex {
    /// MVT units (the lyon position divided by `COORD_SCALE`, rounded).
    pub pos: [i16; 2],
    /// Row-major index into the draw's [`PaintTable`].
    pub paint: u16,
    /// [`FLAG_NORTH_POLE`] / [`FLAG_SOUTH_POLE`], else 0.
    pub flags: u16,
}

/// `Vertex::is_stroke == -1.0` (`shader.wgsl`: `select(-1, 1, is_stroke > -1.5)`).
pub const FLAG_NORTH_POLE: u16 = 1;
/// `Vertex::is_stroke == -2.0`.
pub const FLAG_SOUTH_POLE: u16 = 2;
/// Texels per paint-table row: 256 `(from, to)` entries.
pub const PAINT_TABLE_WIDTH: u32 = 512;
/// `vs_compact`'s `CompactInput`: locations 0 and 1.
pub const COMPACT_ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Sint16x2, 1 => Uint16x2];
/// [`COMPACT_ATTRS`] from a planar buffer: `pos` in one stream...
pub const COMPACT_PLANAR_POS_ATTRS: [wgpu::VertexAttribute; 1] = wgpu::vertex_attr_array![0 => Sint16x2];
/// ...and `[paint, flags]` in the other, each at offset 0.
pub const COMPACT_PLANAR_PAINT_ATTRS: [wgpu::VertexAttribute; 1] = wgpu::vertex_attr_array![1 => Uint16x2];

/// The 16-byte tile-local vertex for a basemap tile's UNDASHED strokes
/// (`shader.wgsl`'s `vs_compact_stroke`), against the 88-byte
/// [`crate::render::mesh::Vertex`]:
///
/// - `pos`: the centreline point in MVT units. A stroke vertex's position is
///   lyon's `position_on_path()`, the input point itself, so a line's own
///   vertices sit on the integer grid; one that does not (a clip or
///   subdivision cut) keeps its triangle 88-byte, as a fill's does.
/// - `normal`: lyon's extrusion normal, f32 BIT FOR BIT. It is not a unit
///   vector (a miter join's is longer) and the extrusion scales it by the
///   line's width in pixels and, on the globe, feeds it through
///   `project_to_sphere`: a quantised normal (snorm16, f16) moves edge
///   pixels. Two f32s are the floor for identity.
/// - `paint`: the LINEAR texel index of the vertex's entry in the tile's
///   paint table ([`SplitTileMesh::table`]): colour from, colour to,
///   `[half width from, to, half gap from, to]`, `[is_stroke, 0, 0, 0]`
///   -- [`STROKE_ENTRY_TEXELS`] texels. u32 because wgpu strides are
///   multiples of 4: a u16 index would leave two bytes of padding, not
///   save them.
///
/// Everything else `vertex_main` reads is constant for an undashed stroke
/// (`dash_runs` 0) or only reaches the fragment through the dash mask,
/// which an all-zero `dash_runs` skips (`line_distance`, the floored
/// `dash_width_px`).
///
/// `is_stroke` is always 1 too, but it is READ FROM THE TABLE, not written
/// as a constant: Metal compiles with fast math, and a constant
/// `is_stroke` lets it fold `vertex_main`'s stroke branch and contract the
/// globe extrusion (`pos_in_tile + extrude_tile`) into different FMAs than
/// the 88-byte draw gets -- 815 px differing by up to 2 on Liberty's z11
/// globe frame at transition 0.7. Opaque, it compiles to the same
/// arithmetic.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CompactStrokeVertex {
    pub pos: [i16; 2],
    pub normal: [f32; 2],
    pub paint: u32,
}

/// `vs_compact_stroke`'s `CompactStrokeInput`: locations 0, 1 and 2.
pub const COMPACT_STROKE_ATTRS: [wgpu::VertexAttribute; 3] =
    wgpu::vertex_attr_array![0 => Sint16x2, 1 => Float32x2, 2 => Uint32];

/// Texels one stroke entry takes in a tile's paint table.
pub const STROKE_ENTRY_TEXELS: usize = 4;

/// A stroke's paint-table entry: the colour pair, then
/// `[half_width_px, half_width_px_to, gap_half_px, gap_half_px_to]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokePaint {
    pub color: [f32; 4],
    pub color_to: [f32; 4],
    pub widths: [f32; 4],
}

/// An index list in the narrowest format that addresses its vertices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexList {
    U16(Vec<u16>),
    U32(Vec<u32>),
}

impl IndexList {
    /// `v` rebased by `base`, u16 when `vertices` fit it.
    pub(crate) fn rebased(v: &[u32], base: usize, vertices: usize) -> Self {
        let base = base as u32;
        if vertices <= usize::from(u16::MAX) + 1 {
            Self::U16(v.iter().map(|&i| (i - base) as u16).collect())
        } else {
            Self::U32(v.iter().map(|&i| i - base).collect())
        }
    }
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U16(v) => v.len(),
            Self::U32(v) => v.len(),
        }
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::U16(v) => bytemuck::cast_slice(v),
            Self::U32(v) => bytemuck::cast_slice(v),
        }
    }
    #[must_use]
    pub fn format(&self) -> wgpu::IndexFormat {
        match self {
            Self::U16(_) => wgpu::IndexFormat::Uint16,
            Self::U32(_) => wgpu::IndexFormat::Uint32,
        }
    }
}

/// One compact draw unit: vertices, the fill's triangle list and the
/// hairline's line list over the same vertices.
#[derive(Debug, Clone)]
pub struct CompactMesh {
    pub vertices: Vec<CompactVertex>,
    pub fill: IndexList,
    pub outline: IndexList,
}

impl CompactMesh {
    /// Device bytes of the three buffers, unpadded.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        std::mem::size_of_val(&self.vertices[..]) + self.fill.bytes().len() + self.outline.bytes().len()
    }
}

/// Why a mesh cannot be drawn compact: it keeps the 88-byte vertex.
#[derive(Debug, PartialEq, Eq)]
pub enum NotCompact {
    /// A stroke vertex: wide lines need the extrusion attributes.
    Stroke { vertex: usize },
    /// A position outside `i16` once divided by `COORD_SCALE`.
    OutOfRange { vertex: usize },
    /// `ranges` and `op_paint` disagree about the op count.
    OpCount { ranges: usize, paints: usize },
}

/// Every vertex of `mesh` as a [`CompactVertex`], tagged with its op's
/// paint index. Vertices no span covers keep paint 0.
///
/// # Errors
/// [`NotCompact`] when any vertex is a stroke or out of `i16` range.
pub fn compact_vertices(mesh: &TileMesh, ranges: &[OpSpan], op_paint: &[u16]) -> Result<Vec<CompactVertex>, NotCompact> {
    if ranges.len() != op_paint.len() {
        return Err(NotCompact::OpCount { ranges: ranges.len(), paints: op_paint.len() });
    }
    let q = |c: f32| -> Option<i16> {
        let m = (c / COORD_SCALE).round();
        (m >= f32::from(i16::MIN) && m <= f32::from(i16::MAX)).then_some(m as i16)
    };
    let mut vertices = vec![CompactVertex::default(); mesh.vertices.len()];
    for (span, &paint) in ranges.iter().zip(op_paint) {
        for i in span.vertices.clone() {
            let v = &mesh.vertices[i];
            let flags = if v.is_stroke == 0.0 {
                0
            } else if v.is_stroke == -1.0 {
                FLAG_NORTH_POLE
            } else if v.is_stroke == -2.0 {
                FLAG_SOUTH_POLE
            } else {
                return Err(NotCompact::Stroke { vertex: i });
            };
            let (Some(x), Some(y)) = (q(v.position[0]), q(v.position[1])) else {
                return Err(NotCompact::OutOfRange { vertex: i });
            };
            vertices[i] = CompactVertex { pos: [x, y], paint, flags };
        }
    }
    Ok(vertices)
}

/// `mesh` as one [`CompactMesh`], index for index. `outline` is A2's line
/// list over `mesh.vertices`.
///
/// # Errors
/// See [`compact_vertices`].
pub fn compact_from_tile_mesh(
    mesh: &TileMesh,
    ranges: &[OpSpan],
    op_paint: &[u16],
    outline: &[u32],
) -> Result<CompactMesh, NotCompact> {
    let vertices = compact_vertices(mesh, ranges, op_paint)?;
    let n = vertices.len();
    Ok(CompactMesh { vertices, fill: IndexList::rebased(&mesh.indices, 0, n), outline: IndexList::rebased(outline, 0, n) })
}

/// `(from, to)` colour pairs as `Rgba32Float` texels, 256 pairs per
/// [`PAINT_TABLE_WIDTH`]-texel row.
#[derive(Debug, Clone, PartialEq)]
pub struct PaintTable {
    pub texels: Vec<[f32; 4]>,
    pub width: u32,
    pub height: u32,
}

/// The table `vs_compact` reads: entry `i` at row `i / 256`, texels
/// `2 * (i % 256)` (from) and `+ 1` (to). At least one row.
#[must_use]
pub fn paint_table(entries: &[(Rgba, Rgba)]) -> PaintTable {
    let per_row = (PAINT_TABLE_WIDTH / 2) as usize;
    let height = entries.len().div_ceil(per_row).max(1) as u32;
    let mut texels = vec![[0.0f32; 4]; (PAINT_TABLE_WIDTH * height) as usize];
    for (i, (from, to)) in entries.iter().enumerate() {
        let at = (i / per_row) * PAINT_TABLE_WIDTH as usize + (i % per_row) * 2;
        texels[at] = [from.r, from.g, from.b, from.a];
        texels[at + 1] = [to.r, to.g, to.b, to.a];
    }
    PaintTable { texels, width: PAINT_TABLE_WIDTH, height }
}

/// Whether basemap tiles draw their fills on the compact vertex
/// ([`split_tile_mesh`]). `GPUI_MAP_BASEMAP_COMPACT=0` keeps every basemap
/// tile on the 88-byte vertex, for an A/B in one binary. Read once.
#[must_use]
pub fn basemap_compact_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GPUI_MAP_BASEMAP_COMPACT").as_deref() != Ok("0"))
}

/// Whether a split basemap tile also moves its undashed strokes onto the
/// 16-byte [`CompactStrokeVertex`]. `GPUI_MAP_BASEMAP_COMPACT_STROKES=0`
/// keeps them 88-byte (the fills-only split), for an A/B in one binary.
/// Read once.
#[must_use]
pub fn basemap_compact_strokes_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GPUI_MAP_BASEMAP_COMPACT_STROKES").as_deref() != Ok("0"))
}

/// The most [`TileRun`]s (draw calls) a split tile may take; past it the
/// tile keeps its one 88-byte draw. A basemap tile interleaves fill and
/// line layers a handful of times (Positron 4, Liberty 6 on the Singapore
/// z14 fixture).
pub const MAX_SPLIT_RUNS: usize = 12;

/// One stretch of a split tile's triangles, in the tile's own draw order,
/// that all live in the same body: `indices` is a range of that body's
/// index buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileRun {
    pub body: RunBody,
    pub indices: std::ops::Range<u32>,
}

/// Which of a split tile's three bodies a [`TileRun`] draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunBody {
    /// The 88-byte vertex: dashes, off-grid vertices, and strokes when
    /// [`basemap_compact_strokes_enabled`] is off.
    Legacy,
    /// Fills on the 8-byte [`CompactVertex`] (`vs_compact_tile`).
    Fill,
    /// Undashed strokes on the 16-byte [`CompactStrokeVertex`]
    /// (`vs_compact_stroke`).
    Stroke,
}

/// A basemap [`TileMesh`] cut in two by [`split_tile_mesh`]: the fill
/// triangles on the 8-byte vertex, everything else still on the 88-byte
/// one, and the runs that interleave them back into op order.
#[derive(Debug, Clone)]
pub struct SplitTileMesh {
    /// The triangles that must keep the 88-byte vertex (strokes, dashes),
    /// with only the vertices they use.
    pub legacy: TileMesh,
    /// The fill triangles; `outline` is always empty.
    pub compact: CompactMesh,
    /// The `(from, to)` colour pair each paint index names: one entry per
    /// distinct pair the tile's fill vertices carry. A layer's paint is
    /// resolved per bucket and per feature class before tessellation, so
    /// this is one entry per (layer, data-driven value) class in the tile,
    /// merged where two classes resolve to the same pair.
    pub paints: Vec<(Rgba, Rgba)>,
    /// The undashed stroke triangles; empty when strokes stay 88-byte.
    pub strokes: Vec<CompactStrokeVertex>,
    pub stroke_indices: IndexList,
    /// One entry per distinct stroke paint, interned like `paints`. A
    /// stroke vertex's `paint` is its entry's first texel:
    /// `2 * paints.len() + STROKE_ENTRY_TEXELS * entry`.
    pub stroke_paints: Vec<StrokePaint>,
    /// Draw order: alternating bodies, covering every index list exactly.
    pub runs: Vec<TileRun>,
}

impl SplitTileMesh {
    /// The tile's paint table, linear in texels: fill entry `i` at texels
    /// `2i, 2i + 1` (the layout `vs_compact_tile` reads, 256 pairs a
    /// [`PAINT_TABLE_WIDTH`] row), then the stroke entries, four texels
    /// each, from texel `2 * paints.len()` on.
    #[must_use]
    pub fn table(&self) -> PaintTable {
        let fill_texels = 2 * self.paints.len();
        let total = fill_texels + STROKE_ENTRY_TEXELS * self.stroke_paints.len();
        let width = PAINT_TABLE_WIDTH as usize;
        let height = total.div_ceil(width).max(1);
        let mut texels = vec![[0.0f32; 4]; width * height];
        for (i, (from, to)) in self.paints.iter().enumerate() {
            texels[2 * i] = [from.r, from.g, from.b, from.a];
            texels[2 * i + 1] = [to.r, to.g, to.b, to.a];
        }
        for (k, p) in self.stroke_paints.iter().enumerate() {
            let at = fill_texels + STROKE_ENTRY_TEXELS * k;
            texels[at] = p.color;
            texels[at + 1] = p.color_to;
            texels[at + 2] = p.widths;
            texels[at + 3] = [1.0, 0.0, 0.0, 0.0];
        }
        PaintTable { texels, width: PAINT_TABLE_WIDTH, height: height as u32 }
    }
}

/// `v` as a compact position and flags, when `vs_compact` draws it exactly
/// as `vertex_main` does: a fill or pole vertex whose stroke-only inputs
/// that the fill path still reads (`half_width_px` for `dash_width_px`,
/// `line_distance`, `dash_runs`) are zero, and whose position fits `i16`
/// MVT units.
fn compact_position(v: &crate::render::mesh::Vertex) -> Option<([i16; 2], u16)> {
    let flags = if v.is_stroke == 0.0 {
        0
    } else if v.is_stroke == -1.0 {
        FLAG_NORTH_POLE
    } else if v.is_stroke == -2.0 {
        FLAG_SOUTH_POLE
    } else {
        return None;
    };
    if v.half_width_px != 0.0 || v.line_distance != 0.0 || v.dash_runs != [0.0; 4] {
        return None;
    }
    // EXACT positions only: `vs_compact` rebuilds `i16 * 1000.0`, so a
    // vertex converts only if that reproduces its f32 bit for bit. The
    // vertices a basemap tile has off the integer grid -- the globe
    // subdivision's cuts (z0-z5 only) and lyon's self-intersection points --
    // keep their triangles on the 88-byte vertex, so the split tile draws
    // the unsplit tile's pixels exactly, flat and on the globe.
    let q = |c: f32| -> Option<i16> {
        let m = (c / COORD_SCALE).round();
        (m >= f32::from(i16::MIN) && m <= f32::from(i16::MAX) && m * COORD_SCALE == c).then_some(m as i16)
    };
    Some(([q(v.position[0])?, q(v.position[1])?], flags))
}

/// `v` as a compact stroke position, when `vs_compact_stroke` draws it
/// exactly as `vertex_main` does: an ordinary stroke vertex (`is_stroke`
/// 1), undashed, at an EXACT `i16` MVT position (see [`compact_position`]).
fn stroke_position(v: &crate::render::mesh::Vertex) -> Option<[i16; 2]> {
    if v.is_stroke != 1.0 || v.dash_runs != [0.0; 4] {
        return None;
    }
    let q = |c: f32| -> Option<i16> {
        let m = (c / COORD_SCALE).round();
        (m >= f32::from(i16::MIN) && m <= f32::from(i16::MAX) && m * COORD_SCALE == c).then_some(m as i16)
    };
    Some([q(v.position[0])?, q(v.position[1])?])
}

/// Splits a basemap tile's triangulation so its fills draw on the 8-byte
/// [`CompactVertex`] and, when [`basemap_compact_strokes_enabled`], its
/// undashed strokes on the 16-byte [`CompactStrokeVertex`]. A triangle
/// whose three vertices pass [`compact_position`] goes to the fill body,
/// one whose three pass [`stroke_position`] to the stroke body, their
/// paints interned as paint-table entries; every other triangle (dashes,
/// off-grid cuts) stays on the 88-byte vertex. Triangle order is kept as
/// [`TileRun`]s, so drawing the runs in order composites exactly as the one
/// draw did.
///
/// Every run is a draw call, so the split never takes more than
/// [`MAX_SPLIT_RUNS`]: past it, [`fit_runs`] sends the smallest compact runs
/// back to the 88-byte body until it fits.
///
/// The classification is per triangle, not per op, so it needs no
/// `OpSpan`s; it agrees with them anyway, since an op's triangles are all
/// fill or all stroke.
///
/// `None` when no triangle is compact (the tile keeps its one 88-byte body),
/// or when the fills carry more than 65,536 distinct colour pairs.
#[must_use]
pub fn split_tile_mesh(mesh: &TileMesh) -> Option<SplitTileMesh> {
    split_tile_mesh_with(mesh, basemap_compact_strokes_enabled())
}

/// [`split_tile_mesh`] with the stroke body forced on or off.
#[must_use]
pub fn split_tile_mesh_with(mesh: &TileMesh, strokes: bool) -> Option<SplitTileMesh> {
    split_bodies(mesh, strokes, &[], &[]).map(|(split, _)| split)
}

/// Per body, three index lists over that body's vertices: `[water, green,
/// building]` ([`mask_slot`]). The dot mask draws them (Task 5 of
/// `docs/superpowers/plans/2026-09-27-dasymetric-dot-placement.md`) from
/// the tile's own vertex buffers, so it never re-tessellates: `compact`
/// indexes [`SplitTileMesh::compact`]'s vertices, `legacy` the 88-byte ones
/// (a split tile's [`SplitTileMesh::legacy`], or the whole mesh of a tile
/// that installs unsplit, [`mask_lists_unsplit`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaskIndexLists {
    pub compact: [Vec<u32>; 3],
    pub legacy: [Vec<u32>; 3],
}

impl MaskIndexLists {
    /// No ranked triangle in either body.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.compact.iter().chain(&self.legacy).all(Vec::is_empty)
    }

    /// Device bytes the six lists take as `u32` index buffers, unpadded.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.compact.iter().chain(&self.legacy).map(|l| 4 * l.len() as u64).sum()
    }
}

/// A rank's list in [`MaskIndexLists`]: `Water` 0, `Green` 1, `Building`
/// 2. `Land` has none -- a plan never lists it.
#[must_use]
pub fn mask_slot(rank: MaskRank) -> Option<usize> {
    match rank {
        MaskRank::Water => Some(0),
        MaskRank::Green => Some(1),
        MaskRank::Building => Some(2),
        MaskRank::Land => None,
    }
}

/// [`split_tile_mesh`], and the ranked ops' triangles as
/// [`MaskIndexLists`] over the split's bodies. `spans` align 1:1 with the
/// plan's ops (`render::tessellate_plan_with_ranges`), and `ranks` are that
/// plan's (`style::resolve_plan_with_mask`). The split is exactly
/// [`split_tile_mesh`]'s: the lists are an extra output, never an input to
/// which body a triangle goes to.
#[must_use]
pub fn split_tile_mesh_masked(mesh: &TileMesh, spans: &[OpSpan], ranks: &MaskRanks) -> Option<(SplitTileMesh, MaskIndexLists)> {
    split_tile_mesh_with_mask(mesh, basemap_compact_strokes_enabled(), spans, ranks)
}

/// [`split_tile_mesh_masked`] with the stroke body forced on or off.
#[must_use]
pub fn split_tile_mesh_with_mask(
    mesh: &TileMesh,
    strokes: bool,
    spans: &[OpSpan],
    ranks: &MaskRanks,
) -> Option<(SplitTileMesh, MaskIndexLists)> {
    split_bodies(mesh, strokes, spans, &ranks.ops)
}

/// Whether `ranks` can be walked over `spans` and `mesh`: op indices
/// strictly ascending, the last one inside `spans`, and every ranked op's
/// span inside `mesh.indices`. What [`split_tile_mesh_masked`] and
/// [`mask_lists_unsplit`] index without checking.
#[must_use]
pub fn mask_ranks_align(mesh: &TileMesh, spans: &[OpSpan], ranks: &MaskRanks) -> bool {
    ranks.ops.windows(2).all(|w| w[0].0 < w[1].0)
        && ranks.ops.iter().all(|&(op, _)| spans.get(op as usize).is_some_and(|s| s.indices.end <= mesh.indices.len()))
}

/// For a tile that installs unsplit: the ranked ops' own indices, as
/// `legacy` (over the whole mesh's vertices, which is the 88-byte body).
#[must_use]
pub fn mask_lists_unsplit(mesh: &TileMesh, spans: &[OpSpan], ranks: &MaskRanks) -> MaskIndexLists {
    let mut lists = MaskIndexLists::default();
    for &(op, rank) in &ranks.ops {
        if let Some(slot) = mask_slot(rank) {
            lists.legacy[slot].extend_from_slice(&mesh.indices[spans[op as usize].indices.clone()]);
        }
    }
    lists
}

/// Demotes compact runs of `plan` (`(body, triangles)`, in draw order) to
/// the 88-byte body until it takes at most `cap` draws.
///
/// Each step demotes the run with the fewest triangles per draw it saves: a
/// run between two 88-byte runs merges all three (two draws saved), one
/// beside a single 88-byte run saves one, and one between two compact runs
/// saves none, so it goes only when nothing else can. Every draw call is a
/// run: without this, the globe subdivision's off-grid cuts (z0-z5), which
/// leave single fill triangles on the 88-byte side, measured 6.67 M draws
/// on a DC zoom sweep against 123 k unsplit (CPU p50 1.3 -> 3.8 ms); and
/// Liberty interleaves enough dashed road layers with solid ones that its
/// stroke split alone takes 13-15 draws.
///
/// Only a plan at most twice the cap is fitted. A more fragmented one (the
/// z0-z5 subdivision cuts every stroke: ~900 runs on the fixture at z3, and
/// 28-150 for its fills alone) first loses its strokes and, if still past
/// twice the cap, everything, so the tile keeps its one 88-byte draw: fitting
/// those too doubled the DC sweep's draw calls (141 k -> 275-308 k) for no
/// measurable byte saving, since the tiles are the few, small low-zoom ones.
fn fit_runs(plan: &mut Vec<(RunBody, usize)>, cap: usize) {
    let fit_max = 2 * cap;
    for demote in [RunBody::Stroke, RunBody::Fill] {
        if plan.len() > fit_max {
            let mut merged: Vec<(RunBody, usize)> = Vec::with_capacity(plan.len());
            for &(b, count) in plan.iter() {
                let b = if b == demote { RunBody::Legacy } else { b };
                match merged.last_mut() {
                    Some((last, c)) if *last == b => *c += count,
                    _ => merged.push((b, count)),
                }
            }
            *plan = merged;
        }
    }
    while plan.len() > cap {
        let legacy_beside = |k: usize| {
            usize::from(k > 0 && plan[k - 1].0 == RunBody::Legacy)
                + usize::from(k + 1 < plan.len() && plan[k + 1].0 == RunBody::Legacy)
        };
        let Some(k) = (0..plan.len())
            .filter(|&k| plan[k].0 != RunBody::Legacy)
            // Triangles demoted per draw saved; a run saving none sorts last.
            .min_by_key(|&k| match legacy_beside(k) {
                0 => (1, plan[k].1),
                saved => (0, plan[k].1.div_ceil(saved)),
            })
        else {
            return;
        };
        plan[k].0 = RunBody::Legacy;
        if k + 1 < plan.len() && plan[k + 1].0 == RunBody::Legacy {
            plan[k].1 += plan[k + 1].1;
            plan.remove(k + 1);
        }
        if k > 0 && plan[k - 1].0 == RunBody::Legacy {
            plan[k - 1].1 += plan[k].1;
            plan.remove(k);
        }
    }
}

/// The split, and when `ranked` is not empty the mask lists: each ranked
/// op's triangles, found by walking `spans` alongside the triangles (both
/// are in draw order), are copied from whichever body the triangle went to,
/// so the lists hold the body's remapped indices. With `ranked` empty the
/// walk never starts and the lists allocate nothing.
fn split_bodies(
    mesh: &TileMesh,
    strokes: bool,
    spans: &[OpSpan],
    ranked: &[(u32, MaskRank)],
) -> Option<(SplitTileMesh, MaskIndexLists)> {
    const UNMAPPED: u32 = u32::MAX;
    let n = mesh.vertices.len();
    let positions: Vec<Option<([i16; 2], u16)>> = mesh.vertices.iter().map(compact_position).collect();
    let stroke_positions: Vec<Option<[i16; 2]>> = if strokes {
        mesh.vertices.iter().map(stroke_position).collect()
    } else {
        vec![None; n]
    };
    if positions.iter().all(Option::is_none) && stroke_positions.iter().all(Option::is_none) {
        return None;
    }
    let mut compact_of = vec![UNMAPPED; n];
    let mut stroke_of = vec![UNMAPPED; n];
    let mut legacy_of = vec![UNMAPPED; n];
    let mut compact_vertices: Vec<CompactVertex> = Vec::new();
    let mut compact_indices: Vec<u32> = Vec::new();
    let mut stroke_vertices: Vec<CompactStrokeVertex> = Vec::new();
    let mut stroke_indices: Vec<u32> = Vec::new();
    let mut legacy = TileMesh::default();
    let mut paints: Vec<(Rgba, Rgba)> = Vec::new();
    let mut paint_of: std::collections::HashMap<[u32; 8], u16> = std::collections::HashMap::new();
    let mut last_paint: Option<([u32; 8], u16)> = None;
    let mut stroke_paints: Vec<StrokePaint> = Vec::new();
    let mut stroke_paint_of: std::collections::HashMap<[u32; 12], u32> = std::collections::HashMap::new();
    let mut last_stroke_paint: Option<([u32; 12], u32)> = None;
    let mut runs: Vec<TileRun> = Vec::new();

    // Classify, then fit the draw cap by demoting whole runs, then build.
    let mut plan: Vec<(RunBody, usize)> = Vec::new();
    for tri in mesh.indices.as_chunks::<3>().0.iter() {
        let body = if tri.iter().all(|&i| positions[i as usize].is_some()) {
            RunBody::Fill
        } else if tri.iter().all(|&i| stroke_positions[i as usize].is_some()) {
            RunBody::Stroke
        } else {
            RunBody::Legacy
        };
        match plan.last_mut() {
            Some((b, count)) if *b == body => *count += 1,
            _ => plan.push((body, 1)),
        }
    }
    fit_runs(&mut plan, MAX_SPLIT_RUNS);
    if plan.iter().all(|&(b, _)| b == RunBody::Legacy) {
        return None;
    }
    let mut bodies = plan.iter().flat_map(|&(b, count)| std::iter::repeat_n(b, count));
    let mut lists = MaskIndexLists::default();
    let mut next_ranked = 0usize;

    for (t, tri) in mesh.indices.as_chunks::<3>().0.iter().enumerate() {
        let body = bodies.next().expect("the plan covers every triangle");
        // The ranked op this triangle belongs to, if any: skip ranked ops
        // wholly before it (spans ascend with the ops).
        let at = 3 * t;
        while next_ranked < ranked.len() && spans[ranked[next_ranked].0 as usize].indices.end <= at {
            next_ranked += 1;
        }
        let slot = ranked
            .get(next_ranked)
            .filter(|&&(op, _)| spans[op as usize].indices.start <= at)
            .and_then(|&(_, rank)| mask_slot(rank));
        match body {
            RunBody::Fill => {
                for &i in tri {
                    let slot = &mut compact_of[i as usize];
                    if *slot == UNMAPPED {
                        let v = &mesh.vertices[i as usize];
                        let mut key = [0u32; 8];
                        for c in 0..4 {
                            key[c] = v.color[c].to_bits();
                            key[4 + c] = v.color_to[c].to_bits();
                        }
                        let paint = match last_paint {
                            Some((k, p)) if k == key => p,
                            _ => {
                                let p = if let Some(&p) = paint_of.get(&key) {
                                    p
                                } else {
                                    let p = u16::try_from(paints.len()).ok()?;
                                    let rgba = |c: [f32; 4]| Rgba { r: c[0], g: c[1], b: c[2], a: c[3] };
                                    paints.push((rgba(v.color), rgba(v.color_to)));
                                    paint_of.insert(key, p);
                                    p
                                };
                                last_paint = Some((key, p));
                                p
                            }
                        };
                        let (pos, flags) = positions[i as usize].expect("checked above");
                        *slot = compact_vertices.len() as u32;
                        compact_vertices.push(CompactVertex { pos, paint, flags });
                    }
                    compact_indices.push(*slot);
                }
            }
            RunBody::Stroke => {
                for &i in tri {
                    let slot = &mut stroke_of[i as usize];
                    if *slot == UNMAPPED {
                        let v = &mesh.vertices[i as usize];
                        let widths = [v.half_width_px, v.half_width_px_to, v.gap_half_px, v.gap_half_px_to];
                        let mut key = [0u32; 12];
                        for c in 0..4 {
                            key[c] = v.color[c].to_bits();
                            key[4 + c] = v.color_to[c].to_bits();
                            key[8 + c] = widths[c].to_bits();
                        }
                        // The ENTRY number; the texel offset is added once the
                        // fill entries are all known, below.
                        let entry = match last_stroke_paint {
                            Some((k, p)) if k == key => p,
                            _ => {
                                let p = *stroke_paint_of.entry(key).or_insert_with(|| {
                                    stroke_paints.push(StrokePaint { color: v.color, color_to: v.color_to, widths });
                                    (stroke_paints.len() - 1) as u32
                                });
                                last_stroke_paint = Some((key, p));
                                p
                            }
                        };
                        let pos = stroke_positions[i as usize].expect("checked above");
                        *slot = stroke_vertices.len() as u32;
                        stroke_vertices.push(CompactStrokeVertex { pos, normal: v.normal, paint: entry });
                    }
                    stroke_indices.push(*slot);
                }
            }
            RunBody::Legacy => {
                for &i in tri {
                    let slot = &mut legacy_of[i as usize];
                    if *slot == UNMAPPED {
                        *slot = legacy.vertices.len() as u32;
                        legacy.vertices.push(mesh.vertices[i as usize]);
                    }
                    legacy.indices.push(*slot);
                }
            }
        }
        if let Some(slot) = slot {
            match body {
                RunBody::Fill => lists.compact[slot].extend_from_slice(&compact_indices[compact_indices.len() - 3..]),
                RunBody::Legacy => lists.legacy[slot].extend_from_slice(&legacy.indices[legacy.indices.len() - 3..]),
                RunBody::Stroke => debug_assert!(false, "a ranked op is a Fill, and a fill triangle is never a stroke"),
            }
        }
        let end = match body {
            RunBody::Fill => compact_indices.len(),
            RunBody::Stroke => stroke_indices.len(),
            RunBody::Legacy => legacy.indices.len(),
        } as u32;
        match runs.last_mut() {
            Some(run) if run.body == body => run.indices.end = end,
            _ => runs.push(TileRun { body, indices: end - 3..end }),
        }
    }
    debug_assert!(runs.len() <= MAX_SPLIT_RUNS, "fit_runs bounds the draws");
    let base = (2 * paints.len()) as u32;
    for v in &mut stroke_vertices {
        v.paint = base + STROKE_ENTRY_TEXELS as u32 * v.paint;
    }
    let count = compact_vertices.len();
    let stroke_count = stroke_vertices.len();
    let split = SplitTileMesh {
        legacy,
        compact: CompactMesh {
            fill: IndexList::rebased(&compact_indices, 0, count),
            vertices: compact_vertices,
            outline: IndexList::U16(Vec::new()),
        },
        paints,
        strokes: stroke_vertices,
        stroke_indices: IndexList::rebased(&stroke_indices, 0, stroke_count),
        stroke_paints,
        runs,
    };
    Some((split, lists))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::mesh::{OpSpan, TileMesh, Vertex, COORD_SCALE};
    use bytemuck::Zeroable;

    fn v(x: f32, y: f32) -> Vertex {
        Vertex { position: [x * COORD_SCALE, y * COORD_SCALE], ..Vertex::zeroed() }
    }

    fn coloured(x: f32, y: f32, c: [f32; 4]) -> Vertex {
        Vertex { color: c, color_to: c, ..v(x, y) }
    }

    fn stroked(x: f32, y: f32) -> Vertex {
        Vertex { is_stroke: 1.0, half_width_px: 1.0, ..v(x, y) }
    }

    #[test]
    fn a_fill_line_fill_tile_splits_into_three_runs_in_op_order() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let blue = [0.0, 0.0, 1.0, 0.5];
        let mesh = TileMesh {
            vertices: vec![
                coloured(0.0, 0.0, red),
                coloured(10.0, 0.0, red),
                coloured(0.0, 10.0, red),
                stroked(0.0, 0.0),
                stroked(5.0, 5.0),
                stroked(9.0, 9.0),
                coloured(20.0, 20.0, blue),
                coloured(30.0, 20.0, blue),
                coloured(20.0, 30.0, blue),
                coloured(30.0, 30.0, red),
            ],
            indices: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 7, 9, 8],
        };
        let split = split_tile_mesh_with(&mesh, false).expect("the tile has fills");
        assert_eq!(
            split.runs,
            vec![
                TileRun { body: RunBody::Fill, indices: 0..3 },
                TileRun { body: RunBody::Legacy, indices: 0..3 },
                TileRun { body: RunBody::Fill, indices: 3..9 },
            ]
        );
        assert_eq!(split.legacy.vertices.len(), 3, "only the stroke's vertices stay 88-byte");
        assert_eq!(split.legacy.indices, vec![0, 1, 2]);
        assert_eq!(split.compact.vertices.len(), 7);
        assert!(matches!(&split.compact.fill, IndexList::U16(i) if i == &[0, 1, 2, 3, 4, 5, 4, 6, 5]));
        // Two distinct pairs, red interned once although it recurs.
        assert_eq!(split.paints.len(), 2);
        assert_eq!(split.compact.vertices[0].paint, 0);
        assert_eq!(split.compact.vertices[3].paint, 1);
        assert_eq!(split.compact.vertices[6].paint, 0);
        assert_eq!(split.compact.vertices[6].pos, [30, 30]);
    }

    #[test]
    fn undashed_strokes_go_to_the_stroke_body_and_dashes_stay_88_byte() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let casing = |x: f32, y: f32| Vertex { normal: [0.25, -1.5], color: red, color_to: red, gap_half_px: 2.0, ..stroked(x, y) };
        let dashed = |x: f32, y: f32| Vertex { dash_runs: [2.0, 1.0, 0.0, 0.0], ..stroked(x, y) };
        let mesh = TileMesh {
            vertices: vec![
                coloured(0.0, 0.0, red),
                coloured(10.0, 0.0, red),
                coloured(0.0, 10.0, red),
                stroked(0.0, 0.0),
                stroked(5.0, 5.0),
                casing(9.0, 9.0),
                dashed(1.0, 1.0),
                dashed(2.0, 1.0),
                dashed(1.0, 2.0),
                stroked(7.0, 7.0),
                // Off the integer grid: a clip cut keeps its triangle 88-byte.
                Vertex { position: [7500.0, 0.0], ..stroked(0.0, 0.0) },
            ],
            indices: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 3, 4, 9, 3, 9, 10],
        };
        let split = split_tile_mesh_with(&mesh, true).expect("the tile splits");
        assert_eq!(
            split.runs,
            vec![
                TileRun { body: RunBody::Fill, indices: 0..3 },
                TileRun { body: RunBody::Stroke, indices: 0..3 },
                TileRun { body: RunBody::Legacy, indices: 0..3 },
                TileRun { body: RunBody::Stroke, indices: 3..6 },
                TileRun { body: RunBody::Legacy, indices: 3..6 },
            ]
        );
        assert_eq!(split.strokes.len(), 4);
        assert_eq!(split.legacy.vertices.len(), 6, "the dash and the cut triangle");
        // One fill pair (texels 0, 1), then two stroke entries of 4 texels.
        assert_eq!(split.paints.len(), 1);
        assert_eq!(split.stroke_paints.len(), 2);
        assert_eq!(split.strokes[0].paint, 2);
        assert_eq!(split.strokes[2].paint, 6);
        assert_eq!(split.strokes[2].normal, [0.25, -1.5]);
        assert_eq!(split.strokes[2].pos, [9, 9]);
        let table = split.table();
        assert_eq!(table.texels[6], red);
        assert_eq!(table.texels[5], [1.0, 0.0, 0.0, 0.0], "is_stroke, read by the shader");
        assert_eq!(table.texels[8], [1.0, 0.0, 2.0, 0.0], "half width 1 -> 0, half gap 2 -> 0");
        assert_eq!(table.texels[4], [1.0, 0.0, 0.0, 0.0]);
        // Off, the same tile is the fills-only split.
        let fills_only = split_tile_mesh_with(&mesh, false).expect("fills still split");
        assert!(fills_only.strokes.is_empty());
        assert_eq!(fills_only.runs.len(), 2);
    }

    #[test]
    fn a_stroke_split_past_the_cap_demotes_its_smallest_runs() {
        let mut mesh = TileMesh { vertices: vec![v(0.0, 0.0), v(1.0, 0.0), v(0.0, 1.0)], indices: vec![0, 1, 2] };
        // Undashed and dashed strokes alternating after a fill:
        // MAX_SPLIT_RUNS + 1 runs. The first undashed run is the biggest.
        for k in 0..MAX_SPLIT_RUNS {
            let x = k as f32 * 10.0;
            let dash = if k % 2 == 0 { [0.0; 4] } else { [1.0, 1.0, 0.0, 0.0] };
            for _ in 0..if k == 0 { 4 } else { 1 } {
                let base = mesh.vertices.len() as u32;
                mesh.vertices.extend([(x, 0.0), (x + 5.0, 0.0), (x, 5.0)].map(|(x, y)| Vertex { dash_runs: dash, ..stroked(x, y) }));
                mesh.indices.extend([base, base + 1, base + 2]);
            }
        }
        let split = split_tile_mesh_with(&mesh, true).expect("fits by demotion");
        assert_eq!(split.runs.len(), MAX_SPLIT_RUNS - 1, "one demotion merges three runs");
        // One one-triangle undashed run went back to the 88-byte body; the
        // four-triangle one stayed compact.
        assert_eq!(split.stroke_indices.len(), 3 * (4 + 4));
        assert_eq!(split.legacy.indices.len(), 3 * (6 + 1));
        assert_eq!(split.runs[1], TileRun { body: RunBody::Stroke, indices: 0..12 });
        let drawn: u32 = split.runs.iter().map(|r| r.indices.len() as u32).sum();
        assert_eq!(drawn as usize, mesh.indices.len());
    }

    fn square(x: i32, y: i32) -> std::sync::Arc<geo_types::Geometry<i32>> {
        let ring = geo_types::LineString::from(vec![(x, y), (x + 100, y), (x + 100, y + 100), (x, y + 100), (x, y)]);
        std::sync::Arc::new(geo_types::Geometry::Polygon(geo_types::Polygon::new(ring, vec![])))
    }

    fn fill_op(geometry: std::sync::Arc<geo_types::Geometry<i32>>, g: f32) -> crate::style::PaintOp {
        let c = Rgba { r: 0.0, g, b: 0.0, a: 1.0 };
        crate::style::PaintOp::Fill { color_from: c, color_to: c, geometry }
    }

    /// The positions of `list`'s vertices in `body`, in MVT units.
    fn compact_positions(split: &SplitTileMesh, list: &[u32]) -> Vec<[i16; 2]> {
        list.iter().map(|&i| split.compact.vertices[i as usize].pos).collect()
    }

    /// The positions of op `span`'s triangles in the source mesh.
    fn source_positions(mesh: &TileMesh, span: &OpSpan) -> Vec<[i16; 2]> {
        mesh.indices[span.indices.clone()]
            .iter()
            .map(|&i| {
                let p = mesh.vertices[i as usize].position;
                [(p[0] / COORD_SCALE) as i16, (p[1] / COORD_SCALE) as i16]
            })
            .collect()
    }

    #[test]
    fn mask_lists_are_the_ranked_ops_triangles_remapped() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        let plan = crate::style::PaintPlan {
            ops: vec![fill_op(square(0, 0), 0.1), fill_op(square(200, 0), 0.2), fill_op(square(0, 200), 0.3)],
        };
        let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
            &plan,
            crate::mvt::MVT_EXTENT,
            COORD_SCALE,
            crate::render::subdivide::Subdivision::NONE,
        );
        let ranks = MaskRanks { classified: true, ops: vec![(0, MaskRank::Water), (2, MaskRank::Building)] };
        let (split, lists) = split_tile_mesh_masked(&mesh, &spans, &ranks).expect("three fills split");
        assert_eq!(lists.compact[0].len(), spans[0].indices.len(), "the water square's triangles");
        assert_eq!(lists.compact[2].len(), spans[2].indices.len(), "the building's");
        assert!(lists.compact[1].is_empty() && lists.legacy.iter().all(Vec::is_empty));
        assert!(!lists.compact[0].is_empty());
        let n = split.compact.vertices.len() as u32;
        assert!(lists.compact.iter().flatten().all(|&i| i < n));
        assert_eq!(compact_positions(&split, &lists.compact[0]), source_positions(&mesh, &spans[0]));
        assert_eq!(compact_positions(&split, &lists.compact[2]), source_positions(&mesh, &spans[2]));
        assert_eq!(lists.bytes(), 4 * (spans[0].indices.len() + spans[2].indices.len()) as u64);
        // The split itself is the unmasked split.
        let plain = split_tile_mesh(&mesh).expect("splits");
        assert_eq!((plain.runs, plain.compact.fill, plain.paints), (split.runs, split.compact.fill, split.paints));
        // Unsplit, the lists are the ops' own indices.
        let unsplit = mask_lists_unsplit(&mesh, &spans, &ranks);
        assert_eq!(unsplit.legacy[0], mesh.indices[spans[0].indices.clone()].to_vec());
        assert_eq!(unsplit.legacy[2], mesh.indices[spans[2].indices.clone()].to_vec());
        assert!(unsplit.compact.iter().all(Vec::is_empty) && unsplit.legacy[1].is_empty());
    }

    #[test]
    fn a_plan_with_no_ranked_ops_builds_no_mask_lists() {
        use crate::overlay::dotgrid::mask_class::MaskRanks;
        let plan = crate::style::PaintPlan { ops: vec![fill_op(square(0, 0), 0.1)] };
        let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
            &plan,
            crate::mvt::MVT_EXTENT,
            COORD_SCALE,
            crate::render::subdivide::Subdivision::NONE,
        );
        let (_, lists) = split_tile_mesh_masked(&mesh, &spans, &MaskRanks::default()).expect("splits");
        assert!(lists.is_empty());
        assert_eq!(lists.bytes(), 0);
        assert!(mask_lists_unsplit(&mesh, &spans, &MaskRanks::default()).is_empty());
    }

    #[test]
    fn a_ranked_fill_demoted_to_the_legacy_body_lands_in_the_legacy_list() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        // MAX_SPLIT_RUNS + 1 alternating one-triangle fill / stroke ops:
        // fitting demotes the first fill (the same tile as
        // `a_tile_past_the_draw_cap_is_fitted_and_a_shattered_one_stays_whole`).
        let mut mesh = TileMesh::default();
        let mut spans = Vec::new();
        let mut ranked = Vec::new();
        for k in 0..=MAX_SPLIT_RUNS {
            let base = mesh.vertices.len() as u32;
            let x = k as f32 * 10.0;
            if k % 2 == 0 {
                mesh.vertices.extend([v(x, 0.0), v(x + 5.0, 0.0), v(x, 5.0)]);
                ranked.push((k as u32, MaskRank::Green));
            } else {
                mesh.vertices.extend([stroked(x, 0.0), stroked(x + 5.0, 0.0), stroked(x, 5.0)]);
            }
            mesh.indices.extend([base, base + 1, base + 2]);
            spans.push(OpSpan { vertices: base as usize..base as usize + 3, indices: base as usize..base as usize + 3 });
        }
        let (split, lists) =
            split_tile_mesh_with_mask(&mesh, false, &spans, &MaskRanks { classified: true, ops: ranked }).expect("fitted");
        assert_eq!(split.compact.fill.len(), 3 * 6, "one of the seven fill triangles demoted");
        assert_eq!(lists.legacy[1].len(), 3, "the demoted ranked fill");
        assert_eq!(lists.compact[1].len(), 3 * 6);
        let n = split.legacy.vertices.len() as u32;
        assert!(lists.legacy[1].iter().all(|&i| i < n));
        let demoted: Vec<[f32; 2]> = lists.legacy[1].iter().map(|&i| split.legacy.vertices[i as usize].position).collect();
        let first: Vec<[f32; 2]> = mesh.vertices[0..3].iter().map(|v| v.position).collect();
        assert_eq!(demoted, first);
    }

    /// DP-9: the mask's index bytes on real tiles, against the split
    /// tile's own device bytes (vertex and index bodies plus the paint
    /// table, unpadded). Criterion: <= 3 MB and <= 10 % of the tile.
    ///
    /// Local tiles only (`GPUI_MAP_PARITY_TILES=/a.mvt:/b.mvt`, the
    /// columnar-parity convention); SKIPS when unset. Baked at 14 and 15
    /// with `examples/mapbox-streets-style.json`, so the tiles should be
    /// Streets v8 (the brief's tile is Tokyo 14/14550/6455).
    #[test]
    #[ignore = "measurement gate DP-9"]
    fn dp9_mask_index_bytes_on_tokyo_z14() {
        let Ok(paths) = std::env::var("GPUI_MAP_PARITY_TILES") else {
            println!("DP-9 SKIPPED: GPUI_MAP_PARITY_TILES is unset (colon-separated local .mvt paths)");
            return;
        };
        let bytes = mapbox_fixture_or_skip!("mapbox-streets-style.json");
        let style = crate::style_spec::parse_and_compile(&bytes).expect("Streets compiles");
        let mut failed = false;
        for path in paths.split(':').filter(|p| !p.is_empty()) {
            let decoded = crate::mvt::decode(std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"))).expect("decodes");
            let m = crate::style::match_tile_static_masked(&decoded, &style, true);
            let layers: Vec<String> = decoded.layers.iter().map(|l| format!("{} {}", l.name, l.features.len())).collect();
            println!("DP-9 {path}: layers {}", layers.join(", "));
            for zoom in [14u8, 15] {
                let (plan, ranks) = crate::style::resolve_plan_with_mask(&decoded, &style, &m, zoom);
                let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                    &plan,
                    crate::mvt::MVT_EXTENT,
                    COORD_SCALE,
                    crate::render::subdivide::Subdivision::NONE,
                );
                let (tile_bytes, lists, split) = match split_tile_mesh_masked(&mesh, &spans, &ranks) {
                    Some((split, lists)) => {
                        let bytes = std::mem::size_of_val(&split.legacy.vertices[..])
                            + 4 * split.legacy.indices.len()
                            + split.compact.byte_len()
                            + std::mem::size_of_val(&split.strokes[..])
                            + split.stroke_indices.bytes().len()
                            + 16 * split.table().texels.len();
                        (bytes as u64, lists, true)
                    }
                    None => {
                        let bytes = std::mem::size_of_val(&mesh.vertices[..]) + 4 * mesh.indices.len();
                        (bytes as u64, mask_lists_unsplit(&mesh, &spans, &ranks), false)
                    }
                };
                let mask = lists.bytes();
                let ratio = mask as f64 / tile_bytes as f64;
                let pass = mask <= 3 * 1024 * 1024 && ratio <= 0.10;
                failed |= !pass;
                let counts = |l: &[Vec<u32>; 3]| l.iter().map(|v| v.len() / 3).collect::<Vec<_>>();
                println!(
                    "DP-9 {path} z{zoom}: ops {} ranked {} split {split} | mask {mask} B (tris compact w/g/b {:?}, legacy {:?}) | tile {tile_bytes} B | ratio {:.2}% | {}",
                    plan.ops.len(),
                    ranks.ops.len(),
                    counts(&lists.compact),
                    counts(&lists.legacy),
                    100.0 * ratio,
                    if pass { "PASS" } else { "FAIL" }
                );
            }
        }
        assert!(!failed, "DP-9 failed on at least one tile/zoom (see the rows above)");
    }

    #[test]
    fn misaligned_ranks_are_detected() {
        use crate::overlay::dotgrid::mask_class::{MaskRank, MaskRanks};
        let mesh = TileMesh { vertices: vec![v(0.0, 0.0), v(1.0, 0.0), v(0.0, 1.0)], indices: vec![0, 1, 2, 0, 1, 2] };
        let spans = [OpSpan { vertices: 0..3, indices: 0..3 }, OpSpan { vertices: 0..3, indices: 3..6 }];
        let ranks = |ops: Vec<(u32, MaskRank)>| MaskRanks { classified: true, ops };
        assert!(mask_ranks_align(&mesh, &spans, &ranks(vec![])));
        assert!(mask_ranks_align(&mesh, &spans, &ranks(vec![(0, MaskRank::Water), (1, MaskRank::Green)])));
        assert!(!mask_ranks_align(&mesh, &spans, &ranks(vec![(2, MaskRank::Water)])), "past the spans");
        assert!(!mask_ranks_align(&mesh, &spans, &ranks(vec![(1, MaskRank::Water), (0, MaskRank::Green)])), "descending");
        assert!(!mask_ranks_align(&mesh, &spans, &ranks(vec![(1, MaskRank::Water), (1, MaskRank::Green)])), "repeated");
        let short = TileMesh { vertices: mesh.vertices.clone(), indices: vec![0, 1, 2] };
        assert!(!mask_ranks_align(&short, &spans, &ranks(vec![(1, MaskRank::Water)])), "a span past the indices");
    }

    /// The masked split IS the plain split, field for field, on real tiles
    /// with fills, undashed strokes and dashes: the vendored Singapore z14
    /// fixture under every vendored style, at the zooms the pixel-identity
    /// test uses, strokes on and off.
    #[test]
    fn the_masked_split_equals_the_plain_split_on_every_field() {
        let root = env!("CARGO_MANIFEST_DIR");
        let decoded = crate::mvt::decode(std::fs::read(format!("{root}/benches/fixtures/singapore_z14.pbf")).expect("fixture"))
            .expect("fixture decodes");
        let (mut compared, mut ranked, mut dashed, mut stroked_tiles) = (0usize, 0usize, 0usize, 0usize);
        for dir in ["examples", "demos"] {
            let mut paths: Vec<_> = std::fs::read_dir(format!("{root}/{dir}"))
                .expect("style dir")
                .map(|e| e.expect("entry").path())
                .filter(|p| p.to_string_lossy().ends_with("-style.json"))
                .collect();
            paths.sort();
            for path in paths {
                let Ok(style) = crate::style_spec::parse_and_compile(&std::fs::read(&path).expect("reads")) else { continue };
                let m = crate::style::match_tile_static_masked(&decoded, &style, true);
                for zoom in [12u8, 14, 15, 17] {
                    let (plan, ranks) = crate::style::resolve_plan_with_mask(&decoded, &style, &m, zoom);
                    let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                        &plan,
                        crate::mvt::MVT_EXTENT,
                        COORD_SCALE,
                        crate::render::subdivide::Subdivision::NONE,
                    );
                    for strokes in [false, true] {
                        let plain = split_tile_mesh_with(&mesh, strokes);
                        let masked = split_tile_mesh_with_mask(&mesh, strokes, &spans, &ranks);
                        let (Some(a), Some((b, lists))) = (plain, masked) else {
                            continue;
                        };
                        let at = format!("{} z{zoom} strokes {strokes}", path.display());
                        assert_eq!(bytemuck::cast_slice::<_, u8>(&a.legacy.vertices), bytemuck::cast_slice::<_, u8>(&b.legacy.vertices), "{at}");
                        assert_eq!(a.legacy.indices, b.legacy.indices, "{at}");
                        assert_eq!(a.compact.vertices, b.compact.vertices, "{at}");
                        assert_eq!(a.compact.fill, b.compact.fill, "{at}");
                        assert_eq!(a.compact.outline, b.compact.outline, "{at}");
                        assert_eq!(a.paints, b.paints, "{at}");
                        assert_eq!(a.strokes, b.strokes, "{at}");
                        assert_eq!(a.stroke_indices, b.stroke_indices, "{at}");
                        assert_eq!(a.stroke_paints, b.stroke_paints, "{at}");
                        assert_eq!(a.runs, b.runs, "{at}");
                        compared += 1;
                        ranked += usize::from(!lists.is_empty());
                        dashed += usize::from(b.legacy.vertices.iter().any(|v| v.dash_runs != [0.0; 4]));
                        stroked_tiles += usize::from(!b.strokes.is_empty());
                    }
                }
            }
        }
        assert!(compared > 20, "{compared} splits compared");
        assert!(ranked > 0 && dashed > 0 && stroked_tiles > 0, "ranked {ranked}, dashed {dashed}, stroked {stroked_tiles}");
    }

    #[test]
    fn the_compact_stroke_vertex_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<CompactStrokeVertex>(), 16);
    }

    #[test]
    fn a_tile_past_the_draw_cap_is_fitted_and_a_shattered_one_stays_whole() {
        let alternating = |runs: usize| {
            let mut mesh = TileMesh::default();
            for k in 0..runs {
                let base = mesh.vertices.len() as u32;
                let x = (k % 3000) as f32 * 10.0;
                if k % 2 == 0 {
                    mesh.vertices.extend([v(x, 0.0), v(x + 5.0, 0.0), v(x, 5.0)]);
                } else {
                    mesh.vertices.extend([stroked(x, 0.0), stroked(x + 5.0, 0.0), stroked(x, 5.0)]);
                }
                mesh.indices.extend([base, base + 1, base + 2]);
            }
            mesh
        };
        let split = split_tile_mesh_with(&alternating(MAX_SPLIT_RUNS + 1), false).expect("fitted");
        assert_eq!(split.runs.len(), MAX_SPLIT_RUNS);
        assert_eq!(split.compact.fill.len(), 3 * 6, "one of the seven fill triangles demoted");
        assert_eq!(split_tile_mesh_with(&alternating(MAX_SPLIT_RUNS), false).expect("at the cap").runs.len(), MAX_SPLIT_RUNS);
        // Past twice the cap, the tile keeps its one 88-byte draw.
        assert!(split_tile_mesh_with(&alternating(2 * MAX_SPLIT_RUNS + 1), false).is_none());
        assert!(split_tile_mesh_with(&alternating(600), true).is_none());
        let split = split_tile_mesh_with(&alternating(2 * MAX_SPLIT_RUNS), true).expect("fitted");
        assert!(split.runs.len() <= MAX_SPLIT_RUNS);
    }

    #[test]
    fn a_tile_with_no_fill_or_an_off_range_fill_is_not_split() {
        let lines = TileMesh { vertices: vec![stroked(0.0, 0.0), stroked(1.0, 0.0), stroked(0.0, 1.0)], indices: vec![0, 1, 2] };
        assert!(split_tile_mesh_with(&lines, false).is_none());
        let far = TileMesh { vertices: vec![v(0.0, 0.0), v(40_000.0, 0.0), v(0.0, 1.0)], indices: vec![0, 1, 2] };
        assert!(split_tile_mesh(&far).is_none(), "a triangle with an off-range vertex stays 88-byte");
        let dashed = TileMesh { vertices: vec![Vertex { dash_runs: [1.0, 1.0, 0.0, 0.0], ..v(0.0, 0.0) }, v(1.0, 0.0), v(0.0, 1.0)], indices: vec![0, 1, 2] };
        assert!(split_tile_mesh(&dashed).is_none());
        // A subdivision cut off the integer grid: exactness beats bytes.
        let cut = TileMesh { vertices: vec![v(0.0, 0.0), v(10.5, 0.0), v(0.0, 1.0)], indices: vec![0, 1, 2] };
        assert!(split_tile_mesh(&cut).is_none(), "a fractional vertex keeps its triangle 88-byte");
    }

    #[test]
    fn the_compact_vertex_is_eight_bytes() {
        assert_eq!(std::mem::size_of::<CompactVertex>(), 8);
    }

    #[test]
    fn conversion_keeps_index_identity_and_tags_paint_per_op() {
        let mesh = TileMesh {
            vertices: vec![v(0.0, 0.0), v(10.0, 0.0), v(0.0, 10.0), v(-1024.0, 5120.0)],
            indices: vec![0, 1, 2, 1, 3, 2],
        };
        let ranges = [OpSpan { vertices: 0..3, indices: 0..3 }, OpSpan { vertices: 3..4, indices: 3..6 }];
        let c = compact_from_tile_mesh(&mesh, &ranges, &[7, 9], &[0, 1, 1, 2]).unwrap();
        assert_eq!(c.vertices[0], CompactVertex { pos: [0, 0], paint: 7, flags: 0 });
        assert_eq!(c.vertices[3], CompactVertex { pos: [-1024, 5120], paint: 9, flags: 0 });
        assert!(matches!(&c.fill, IndexList::U16(i) if i == &[0, 1, 2, 1, 3, 2]));
        assert!(matches!(&c.outline, IndexList::U16(i) if i == &[0, 1, 1, 2]));
    }

    #[test]
    fn pole_vertices_become_flags() {
        let mut n = v(0.0, 0.0);
        n.is_stroke = -1.0;
        let mut s = v(0.0, 4096.0);
        s.is_stroke = -2.0;
        let mesh = TileMesh { vertices: vec![n, s, v(1.0, 1.0)], indices: vec![0, 1, 2] };
        let c = compact_from_tile_mesh(&mesh, &[OpSpan { vertices: 0..3, indices: 0..3 }], &[0], &[]).unwrap();
        assert_eq!(c.vertices[0].flags, FLAG_NORTH_POLE);
        assert_eq!(c.vertices[1].flags, FLAG_SOUTH_POLE);
    }

    #[test]
    fn strokes_and_out_of_range_positions_are_refused() {
        let mut stroke = v(0.0, 0.0);
        stroke.is_stroke = 1.0;
        let m = TileMesh { vertices: vec![stroke], indices: vec![0, 0, 0] };
        assert!(compact_from_tile_mesh(&m, &[OpSpan { vertices: 0..1, indices: 0..3 }], &[0], &[]).is_err());
        let m = TileMesh { vertices: vec![v(40_000.0, 0.0)], indices: vec![0, 0, 0] };
        assert!(compact_from_tile_mesh(&m, &[OpSpan { vertices: 0..1, indices: 0..3 }], &[0], &[]).is_err());
    }

    #[test]
    fn more_than_65535_vertices_switch_to_u32_indices() {
        let n = 70_000;
        let mesh = TileMesh {
            vertices: (0..n).map(|i| v((i % 4000) as f32, (i / 4000) as f32)).collect(),
            indices: vec![0, 1, (n - 1) as u32],
        };
        let c = compact_from_tile_mesh(&mesh, &[OpSpan { vertices: 0..n, indices: 0..3 }], &[0], &[]).unwrap();
        assert!(matches!(c.fill, IndexList::U32(_)));
    }

    #[test]
    fn the_paint_table_is_row_major_pairs() {
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let blue = Rgba { r: 0.0, g: 0.0, b: 1.0, a: 0.5 };
        let entries: Vec<(Rgba, Rgba)> = (0..300).map(|i| if i == 257 { (red, blue) } else { (blue, blue) }).collect();
        let t = paint_table(&entries);
        assert_eq!((t.width, t.height), (512, 2));
        // Entry 257 lives on row 1 at texels 2 and 3.
        assert_eq!(t.texels[512 + 2], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(t.texels[512 + 3], [0.0, 0.0, 1.0, 0.5]);
    }
}
