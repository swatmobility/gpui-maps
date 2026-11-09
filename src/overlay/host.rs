//! `OverlayHost`: the overlay orchestration every shell drives -- which
//! tiles of a tiled source to bake and in what order, what landed, what goes
//! on the GPU, which stand-in draws while a tile is missing, and (for a shell
//! with no executor) the whole loop, inline, under a time budget.
//!
//! The `NavHost` pattern (`src/nav/host.rs`): the shells hold these objects
//! and reach the overlay machinery only through this module, which
//! `tests/overlays_are_driven_through_the_host.rs` holds them to. See
//! `docs/superpowers/specs/2026-09-26-overlays-on-every-shell-design.md`.
//!
//! Three layers, each usable without the next:
//!
//! - [`OverlayHost`]: the CPU state (tile sets, the retargeted bake queue,
//!   the live-diff pipelines). Executor-free: a bake is handed out as a
//!   [`TileBake`] and handed back through [`OverlayHost::land_bake`], so
//!   native runs it on tokio's blocking pool and the browser runs it inline.
//! - [`OverlayGpu`]: the per-GPU-context half (per-source tile caches, paint
//!   tables, point circle layers) and the paint-time upload/pick/uniform
//!   pass, [`OverlayGpu::prepare_tiles`].
//! - [`ShellOverlays`]: store + host + GPU + a synchronous point driver, for
//!   the web and iOS shells.
//!
//! Names no platform, no executor and no std clock: the browser compiles it
//! and aborts on `std::time::Instant::now`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::geo::TileId;
use crate::timing::Instant;

// The data types a shell names beside the host: re-exported, never
// wrapped, so a shell's only `overlay::` path is `overlay::host`
// (`tests/overlays_are_driven_through_the_host.rs`).
pub use crate::overlay::{
    ClusterOptions, DataFeatureInput, FeatureError, FeatureId, FeatureInput, FeaturePaint, FeatureProperties, FeatureRef,
    GeometryInput, HighlightStyle, PropertyColumn, SourceId, TagValue,
};
// Which basemap labels sit over a page's polygons (the browser's and the
// phone's labels-on-data paint), memoised per label.
#[allow(unused_imports)]
pub(crate) use crate::overlay::hit::LabelCover;
// The phone's vector-tile overlays (`ios/src/app_vector_overlay.rs`) fetch
// and decode their own tiles, then push the features through the host:
// the decode and the layer compile, not a scheduler of their own.
pub use crate::overlay::vector_tiles;
// A shell evaluating one declared paint property on a feature's own
// properties (the iOS highlight ring's `circle-radius`). Crate-only: the
// desktop never names it here, so the import is unused outside iOS.
#[allow(unused_imports)]
pub(crate) use crate::overlay::source_paint::SourcePaint;
// How a shell's own chrome text (the iOS attribution plate) justifies its
// lines: the overlay label vocabulary, so the plate and labels agree.
pub use crate::overlay::paint::TextJustify;
// A page's `setMarkers` (`overlay::markers`): the marker list, its
// layers and labels, and the lon/lat-to-world step its labels place by.
pub use crate::overlay::geom::lon_lat_to_world;
pub use crate::overlay::markers;
// The dot-density layer (`overlay::dotgrid::layer`): shell-neutral like
// this file, driven by every shell through these names.
pub use crate::overlay::dotgrid::layer::{
    run_job_catching_panic, DotJob, DotLanded, DotLayers, DotLayersGpu, DotRamp, DotStats, DotStyle, DotUploads, Platform,
    RACE_PALETTE,
};
pub use crate::overlay::dotgrid::{GridCountSource, GridDescriptor, GridError, GridRequest, RaceDescriptor};
// A browser's dot sources (Task 16): a PMTiles archive opened by `Range`
// in two steps, or ClickHouse through the page's own `/clickhouse/`
// proxy; `shell_fetch` is how the page's `fetch` judges each reply.
pub use crate::overlay::dotgrid::clickhouse::ClickHouseProxySource;
pub use crate::overlay::dotgrid::layer::GHSL_ATTRIBUTION;
pub use crate::overlay::dotgrid::pmtiles_source::{PmtilesGridSource, PmtilesOpener};
pub use crate::overlay::dotgrid::shell_fetch;
// CSV/TSV sources: parsed by the shell (bytes, or the browser's async
// `CsvSource::from_url`), pushed with `ShellOverlays::set_source_csv`.
pub use crate::overlay::csv::{CsvError, CsvOptions, CsvReport, CsvSource};
// FlatGeobuf sources: read by the shell (bytes, or the browser's async
// `FgbSource::from_url` / `FgbRemote` range queries), pushed with
// `ShellOverlays::set_source_fgb`.
pub use crate::overlay::fgb::{FgbError, FgbOptions, FgbRemote, FgbReport, FgbSource};
// GeoParquet sources: read by the shell (bytes, or the browser's async
// `GeoParquetSource::from_url` / `GeoParquetRemote` range queries), pushed
// with `ShellOverlays::set_source_geoparquet`.
#[cfg(feature = "geoparquet")]
pub use crate::overlay::geoparquet::{GeoParquetError, GeoParquetOptions, GeoParquetRemote, GeoParquetReport, GeoParquetSource};
// A native-threaded shell's blocking fetcher (the phone, Task 17): a
// local archive by file range (its http(s) half needs the
// `sprite_http_fetch` feature, which the phone leaves off).
#[cfg(not(target_arch = "wasm32"))]
pub use crate::overlay::dotgrid::fetch_native;
// The dot mask's per-frame step (`overlay::dotgrid::mask_step`): each
// shell hands it its own frame's basemap draw list.
pub use crate::overlay::dotgrid::mask_step::{mask_ring_for_frame, step_dot_mask, DrawnBasemapTile, MaskRekey, MaskStep};
// The arc/extrusion/trip/heatmap layers a shell that is not `MapView`
// draws itself (the phone, `overlay::shell_layers`), and the extrusion
// data and lighting it hands them.
pub use crate::overlay::extrude::{style_light, ExtrusionStyle, OverlayExtrusion};
pub use crate::overlay::shell_layers::{ShellFrame, ShellLayerStats, ShellLayers, ARC_TAP_TOLERANCE_PX};

/// `map_view`'s `CACHE_BUDGET_DIVISOR`, for the overlay tile cache: the
/// budgets are the desktop's divided by 4 on iOS and wasm, never a separate
/// table.
#[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
const OVERLAY_CACHE_DIVISOR: usize = 1;
#[cfg(any(target_os = "ios", target_arch = "wasm32"))]
const OVERLAY_CACHE_DIVISOR: usize = 4;

/// One point-source circle layer on the GPU: its instances and paint
/// table, uploaded once per built layer (`circles` is the identity), and
/// the model and circle uniforms rewritten every frame.
pub(crate) struct PointCircleGpu {
    pub(crate) circles: Arc<crate::overlay::points::PointCircles>,
    pub(crate) buffers: crate::render::gpu::CircleBuffers,
    pub(crate) _table: crate::render::gpu::PaintTableGpu,
    pub(crate) model_buffer: crate::render::alloc::TrackedBuffer,
    pub(crate) model_bind_group: wgpu::BindGroup,
    pub(crate) params_buffer: crate::render::alloc::TrackedBuffer,
    pub(crate) circle_group: wgpu::BindGroup,
}

/// Reports, once per process, a source the chunked upload could not fit.
pub(crate) fn note_overlay_too_large(source: &str, error: &str) {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!("gpui-maps: overlay source {source:?} not drawn: {error}. Reported once per process.");
    }
}

/// One uploaded chunk of an overlay source's mesh.
pub(crate) struct OverlayChunkGpu {
    pub(crate) buffers: crate::render::TileBuffers,
    /// Which of the mesh's vertices this chunk holds, so a paint-only
    /// update can find where a global vertex range landed.
    pub(crate) vertices: std::ops::Range<usize>,
    /// `[min_x, min_y, max_x, max_y]` in the source's local units.
    pub(crate) bounds: [f32; 4],
}

/// Uploads `mesh` as chunks, reusing `existing`'s buffers chunk for chunk
/// where they fit. `Err` when a single unit is over `max_buffer_size`.
pub(crate) fn install_overlay_chunks(
    pipeline: &crate::render::TilePipeline,
    ctx: &crate::render::GpuContext,
    existing: Vec<OverlayChunkGpu>,
    mesh: &crate::render::TileMesh,
    extras: &crate::render::mesh::OverlayExtras,
) -> Result<Vec<OverlayChunkGpu>, String> {
    let plans = crate::render::mesh::plan_overlay_chunks(
        mesh,
        extras,
        crate::render::mesh::MAX_SEGMENT_VERTICES,
        ctx.max_buffer_size,
    )?;
    let mut old = existing.into_iter().map(|chunk| chunk.buffers);
    let (mut s16, mut s32, mut o16, mut o32) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut chunks = Vec::with_capacity(plans.len());
    for plan in plans {
        let u16_format = plan.is_u16();
        let base = plan.vertices.start;
        let indices = crate::render::mesh::rebase_indices(
            &mesh.indices[plan.indices.clone()],
            base,
            u16_format,
            &mut s16,
            &mut s32,
        );
        let outline = crate::render::mesh::rebase_indices(
            &extras.outline[plan.outline.clone()],
            base,
            u16_format,
            &mut o16,
            &mut o32,
        );
        let buffers = pipeline.install_overlay_chunk(
            ctx,
            old.next(),
            &mesh.vertices[plan.vertices.clone()],
            indices,
            outline,
        );
        chunks.push(OverlayChunkGpu { buffers, vertices: plan.vertices, bounds: plan.bounds });
    }
    Ok(chunks)
}

/// At most this many older pictures stand in behind a tiled source's
/// current GPU set (`OverlayTileGpuSet::previous`). A push faster than a
/// cover bake adds one per push; the per-frame pruning drops the ones no
/// cover tile draws from, so this only bounds the worst case's draw work.
pub(crate) const MAX_STAND_INS: usize = 4;

/// Which sets draw each cover tile this frame, tile by tile, the way
/// MapLibre swaps a reloaded GeoJSON tile in as soon as IT is ready. Set
/// `0` is the current set, `i > 0` the stand-in `previous[i - 1]` (newest
/// first); each entry of `per_set[i]` is that set's answer for the cover
/// tile and, for a [`OverlayCoverDraw::Hole`], the share of each of the
/// tile's four quadrants its partial picks cover ([`quadrant_coverage`]).
///
/// Per cover tile, `(set, quadrant mask)` pairs (bit `q` for quadrant
/// `q`; [`ALL_QUADRANTS`] for a whole answer):
/// - the current set wherever it has any whole answer (its own tile, an
///   ancestor, descendants, or known empty), so every landed tile of a
///   new push shows at once;
/// - else the newest stand-in with a whole answer;
/// - else -- no set answers the tile whole -- each QUADRANT from the
///   newest set covering all of it, or failing that from the set covering
///   the most of it. A quadrant no set has anything for stays blank.
///
/// Why quadrants: across a display-zoom change, a re-pushed source's
/// current set has baked little at the new zoom, and its older pictures
/// hold the OLD zoom's tiles -- one zoom finer on a zoom-out, spread over
/// several pictures when pushes outpace bakes. Each new (coarser) cover
/// tile is then a hole in EVERY set, each holding some of its quadrants.
/// Taking one set's partial answer drew a fraction of the ground; taking
/// the current set's (the old rule) drew nothing, released the stand-ins
/// that had drawn nothing, and blanked the source until its newest push
/// baked its cover -- the race map's hexagon blink. MapLibre keeps such
/// partly-loaded children on screen (`_updateRetainedTiles`).
pub(crate) fn compose_cover_quadrants(per_set: &[Vec<(OverlayCoverDraw, [f64; 4])>]) -> Vec<Vec<(usize, u8)>> {
    let Some(current) = per_set.first() else { return Vec::new() };
    let whole = |d: &OverlayCoverDraw| !matches!(d, OverlayCoverDraw::Hole);
    (0..current.len())
        .map(|e| {
            if per_set[0].get(e).is_some_and(|(d, _)| whole(d)) {
                return vec![(0, ALL_QUADRANTS)];
            }
            if let Some(i) = per_set.iter().position(|s| s.get(e).is_some_and(|(d, _)| whole(d))) {
                return vec![(i, ALL_QUADRANTS)];
            }
            let mut out: Vec<(usize, u8)> = Vec::new();
            for q in 0..4 {
                let share = |i: usize| per_set[i].get(e).map_or(0.0, |(_, c)| c[q]);
                let full = (0..per_set.len()).find(|&i| share(i) >= 1.0);
                let best = full.or_else(|| {
                    (0..per_set.len()).filter(|&i| share(i) > 0.0).fold(None, |b: Option<usize>, i| match b {
                        Some(b) if share(b) >= share(i) => Some(b),
                        _ => Some(i),
                    })
                });
                if let Some(i) = best {
                    match out.iter_mut().find(|(s, _)| *s == i) {
                        Some((_, mask)) => *mask |= 1 << q,
                        None => out.push((i, 1 << q)),
                    }
                }
            }
            if out.is_empty() {
                out.push((0, ALL_QUADRANTS));
            }
            out
        })
        .collect()
}

/// Every quadrant of a cover tile: a whole answer.
pub(crate) const ALL_QUADRANTS: u8 = 0b1111;

/// Which quadrant of `tile` the deeper tile `p` lies in (`0..4`, bit
/// order of [`compose_cover_quadrants`]'s masks), or `None` for a tile at
/// or above `tile`'s zoom.
pub(crate) fn quadrant_of(tile: TileId, p: TileId) -> Option<usize> {
    if p.z <= tile.z {
        return None;
    }
    let shift = u32::from(p.z - tile.z - 1);
    Some((((p.y >> shift) & 1) * 2 + ((p.x >> shift) & 1)) as usize)
}

/// The share of each quadrant of cover tile `tile` that `picks` -- a
/// hole's partial descendants from [`overlay_tile_picks`] -- cover: a pick
/// `dz` levels below covers `4^-(dz - 1)` of its quadrant.
pub(crate) fn quadrant_coverage(tile: TileId, picks: &[(TileId, crate::render::TileClip)]) -> [f64; 4] {
    let mut out = [0.0f64; 4];
    for (p, _) in picks {
        if let Some(q) = quadrant_of(tile, *p) {
            out[q] += 0.25f64.powi(i32::from(p.z - tile.z - 1));
        }
    }
    out.map(|c| c.min(1.0))
}

/// Which stand-ins survive this frame: one no cover tile drew from is
/// dropped -- the current set, or a newer stand-in, answers everywhere it
/// could. `keep[i - 1]` for stand-in `i - 1`.
///
/// A stand-in that IS drawing is never dropped whole, whatever the bytes:
/// it is the only picture on those cover tiles, and dropping it blanks
/// them (the re-push flicker this exists to remove). Over budget, each
/// surviving stand-in is instead trimmed to the tiles it drew this frame
/// (`OverlayGpu::prepare_tiles`), so the bound is: the current set's cache
/// (one budget, plus its own keep-exempt overflow) + the stand-in tiles on
/// screen (at most one cover's worth) + the frame's upload overshoot
/// (`OVERLAY_UPLOAD_BYTES_PER_FRAME` + one tile, before this runs).
pub(crate) fn stand_ins_to_keep(chosen: &[usize], stand_ins: usize) -> Vec<bool> {
    (1..=stand_ins).map(|i| chosen.contains(&i)).collect()
}


/// A tiled overlay source's CPU side: its tile index, its paint classes,
/// and the tiles in flight or baked but not yet uploaded. The GPU side is
/// `GpuState::overlay_tile_gpu[source]`: a `TileCache` with the basemap's
/// own LRU and byte budget.
///
/// No CPU mesh cache (A1): a baked tile sits in `landed` until the next
/// paint uploads it, and is dropped then. An evicted tile is re-cut, not
/// re-read.
pub(crate) struct OverlayTileSet {
    pub(crate) generation: u64,
    pub(crate) bracket: crate::overlay::plan::BakeBracket,
    pub(crate) index: Arc<crate::overlay::tile_index::TileIndex>,
    pub(crate) classes: Arc<crate::overlay::plan::FeatureClasses>,
    /// This source's live stamp, which a tile bake polls to abandon work
    /// a newer push has superseded.
    pub(crate) live: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) in_flight: HashSet<TileId>,
    pub(crate) landed: Vec<(TileId, crate::overlay::tile_plan::BakedOverlayTile)>,
    /// Tiles whose cut came back empty (the bbox test let them through,
    /// the clip did not): never re-requested for this generation.
    pub(crate) empty: HashSet<TileId>,
    /// The working set the GPU cache may not evict for history: this
    /// frame's wanted tiles (coarse cover, visible cover, prefetch), their
    /// ancestor chains, and every tile the last frame drew, stand-ins
    /// included. See
    /// `TileCache::put_with_bytes_keeping`.
    pub(crate) keep: HashSet<TileId>,
    /// Every tile the last frame drew for this source, stand-ins included.
    pub(crate) drawn_last: Vec<TileId>,
    /// When this generation was pushed, for the first-frame census row.
    pub(crate) pushed_at: Instant,
    /// The first frame with a tile draw of this generation has been
    /// counted.
    pub(crate) first_frame_seen: bool,
    /// Bumped by a paint update that CHANGED the classes
    /// (`MapView::update_source_paint`'s re-bake path): the index is
    /// re-classed in place, and every tile cut under the old classes is
    /// another epoch's -- never installed, never drawn beside this one's.
    /// Part of the GPU set's key, so the old epoch's tiles become the
    /// stand-in (`OverlayTileGpuSet::previous`) until this one is whole.
    pub(crate) epoch: u64,
    /// Bumped by a paint update that kept the classes and changed only
    /// their colours (the table-only path). A legacy (88-byte) tile bakes
    /// its colours into its vertices, so one that was cut before the
    /// update and lands after it is thrown away rather than installed.
    pub(crate) paint_rev: u64,
    /// The full check behind the last table-only update: see
    /// `MapView::verify_source_paint`.
    pub(crate) verify: PaintCheck,
    /// Bumped by every landed live diff (`MapView::update_source_features`):
    /// the revision of `index` and `classes`. A bake records the revision
    /// it cut from when it is popped.
    pub(crate) data_rev: u64,
    /// Tiles a live diff touched, with the `data_rev` that touched them. A
    /// dirty tile stays cached and keeps drawing (the old picture) and is
    /// asked for again; it is clean once a bake popped at or after that
    /// revision lands.
    pub(crate) dirty: HashMap<TileId, u64>,
    /// Cached tiles a live diff emptied: removed from the GPU cache by the
    /// next paint.
    pub(crate) evict: Vec<TileId>,
    /// The `data_rev` each landed-but-not-uploaded tile was cut at.
    pub(crate) landed_rev: HashMap<TileId, u64>,
    /// The `data_rev` of the picture each cached tile shows: what a live
    /// diff's completion is judged by (`MapView::take_live_samples`).
    pub(crate) shown_rev: HashMap<TileId, u64>,
    /// Tiles in flight or landed (not uploaded) when a class-only diff
    /// touched them, with its `data_rev`: a compact tile is repainted to
    /// the current classes at upload anyway; one that cannot be (no
    /// spans) is dirtied then.
    pub(crate) repaint: HashMap<TileId, u64>,
    /// The `data_rev` whose classes each cached tile shows, where that is
    /// newer than `shown_rev`: a tile recoloured in place while an older
    /// geometry change still re-bakes under it shows the newer job's
    /// classes. A class-only job's completion is judged by it.
    pub(crate) class_rev: HashMap<TileId, u64>,
    /// The `data_rev` of the last row compaction: a tile cut before it
    /// holds the old row numbers, so its spans are unusable.
    pub(crate) rows_rev: u64,
    /// The `empty` of the older generation this set replaced, handed to
    /// the GPU set's stand-in on its rekey (`OverlayTileGpuSet::empty`).
    pub(crate) stand_in_empty: Option<HashSet<TileId>>,
}

/// See `OverlayTileSet::verify`.
#[derive(Debug, Default)]
pub(crate) struct PaintCheck {
    /// The `paint_rev` whose table was written from one feature per class
    /// and not yet checked against every feature.
    pub(crate) unverified: Option<u64>,
    /// A check is on a worker.
    pub(crate) running: bool,
}

impl OverlayTileSet {
    pub(crate) fn new(
        generation: u64,
        bracket: crate::overlay::plan::BakeBracket,
        index: Arc<crate::overlay::tile_index::TileIndex>,
        classes: Arc<crate::overlay::plan::FeatureClasses>,
        live: Arc<std::sync::atomic::AtomicU64>,
        pushed_at: impl Into<Instant>,
    ) -> Self {
        Self {
            generation,
            bracket,
            index,
            classes,
            live,
            in_flight: HashSet::new(),
            landed: Vec::new(),
            empty: HashSet::new(),
            keep: HashSet::new(),
            drawn_last: Vec::new(),
            pushed_at: pushed_at.into(),
            first_frame_seen: false,
            epoch: 0,
            paint_rev: 0,
            verify: PaintCheck::default(),
            data_rev: 0,
            dirty: HashMap::new(),
            evict: Vec::new(),
            landed_rev: HashMap::new(),
            shown_rev: HashMap::new(),
            repaint: HashMap::new(),
            class_rev: HashMap::new(),
            rows_rev: 0,
            stand_in_empty: None,
        }
    }

    /// A bake landed. `false`, and nothing touched, when it was cut from
    /// another generation's index: a stale tile is never installed over a
    /// newer generation, and never clears the newer one's in-flight mark.
    #[cfg(test)]
    pub(crate) fn accept_landed(
        &mut self,
        generation: u64,
        tile: TileId,
        mesh: Option<crate::overlay::tile_plan::BakedOverlayTile>,
    ) -> bool {
        self.accept_landed_at(generation, self.epoch, self.paint_rev, self.data_rev, tile, mesh)
    }

    /// [`Self::accept_landed`], for a bake started at `epoch`/`paint_rev`:
    /// another epoch's cut carries another epoch's class ids, and a legacy
    /// tile cut before a table-only update carries the old colours.
    pub(crate) fn accept_landed_at(
        &mut self,
        generation: u64,
        epoch: u64,
        paint_rev: u64,
        data_rev: u64,
        tile: TileId,
        mesh: Option<crate::overlay::tile_plan::BakedOverlayTile>,
    ) -> bool {
        if generation != self.generation || epoch != self.epoch {
            return false;
        }
        self.in_flight.remove(&tile);
        if paint_rev != self.paint_rev && matches!(mesh, Some(Err(_))) {
            // Stale colours baked into the vertices: asked for again.
            return false;
        }
        if self.dirty.get(&tile).is_some_and(|&at| at <= data_rev) {
            self.dirty.remove(&tile);
        }
        match mesh {
            Some(mesh) => {
                self.landed_rev.insert(tile, data_rev);
                self.landed.push((tile, mesh));
            }
            None => {
                self.empty.insert(tile);
                self.shown_rev.insert(tile, data_rev);
                // A live diff can empty a cached tile: its old picture goes.
                if data_rev > 0 {
                    self.evict.push(tile);
                }
            }
        }
        true
    }

    /// Whether `tile` needs a bake: not cached on the GPU, not in flight,
    /// not waiting for upload, not known empty.
    pub(crate) fn wants(&self, tile: TileId, cached: impl Fn(&TileId) -> bool) -> bool {
        (!cached(&tile) || self.dirty.contains_key(&tile))
            && !self.in_flight.contains(&tile)
            && !self.empty.contains(&tile)
            && !self.landed.iter().any(|(t, _)| *t == tile)
    }

    #[cfg(test)]
    pub(crate) fn for_test(generation: u64) -> Self {
        let index = crate::overlay::tile_index::TileIndex::build(&[], &[], Default::default(), &|| false)
            .expect("an empty index builds");
        let classes = crate::overlay::plan::FeatureClasses { classes: Vec::new(), of_feature: Vec::new(), data_driven: false };
        Self::new(
            generation,
            crate::overlay::plan::BakeBracket::Bucket(3),
            Arc::new(index),
            Arc::new(classes),
            Arc::new(std::sync::atomic::AtomicU64::new(generation)),
            crate::timing::Instant::now(),
        )
    }
}

/// One overlay tile on the GPU: its chunks (A3: at most 65,536 vertices
/// each, u16 indices, the fill outline's line list beside them) and its own
/// model uniform, which always carries the tile's own clip box.
pub(crate) struct OverlayTileGpu {
    pub(crate) body: OverlayTileBody,
    pub(crate) model_buffer: crate::render::alloc::TrackedBuffer,
}

/// A tile's chunks, on whichever vertex it baked to.
pub(crate) enum OverlayTileBody {
    /// Task 3's 88-byte vertex: a tile with strokes.
    Legacy { chunks: Vec<OverlayChunkGpu>, group: wgpu::BindGroup },
    /// The 8-byte vertex, with the model uniform bound beside the source's
    /// fill table (`fill`) and outline table (`outline`, when any chunk
    /// has a hairline).
    ///
    /// `spans` (a per-feature bake) locates each feature's vertices, so a
    /// class-only live update rewrites their paint index in the chunks'
    /// planar paint streams instead of re-baking. Behind a mutex because
    /// the tile is shared (`Arc`) and the patch records the new classes;
    /// only the UI thread takes it.
    Compact {
        chunks: Vec<crate::render::CompactBuffers>,
        fill: wgpu::BindGroup,
        outline: Option<wgpu::BindGroup>,
        spans: std::sync::Mutex<Option<crate::overlay::tile_plan::FeatureSpans>>,
    },
}

impl OverlayTileGpu {
    pub(crate) fn device_bytes(&self) -> usize {
        let chunks: u64 = match &self.body {
            OverlayTileBody::Legacy { chunks, .. } => chunks
                .iter()
                .map(|c| {
                    c.buffers.vertex_capacity_bytes()
                        + c.buffers.index_capacity_bytes()
                        + c.buffers.outline_capacity_bytes()
                })
                .sum(),
            OverlayTileBody::Compact { chunks, .. } => chunks.iter().map(crate::render::CompactBuffers::device_bytes).sum(),
        };
        usize::try_from(chunks + self.model_buffer.tracked_size()).unwrap_or(usize::MAX)
    }
}

/// A per-frame model uniform for a tile drawn under a clip other than its
/// own box (an overzoomed sub-box, a stand-in), with the bind groups its
/// body needs over it.
pub(crate) struct OverlayTileFrameGroups {
    pub(crate) _buffer: crate::render::alloc::TrackedBuffer,
    pub(crate) legacy: Option<wgpu::BindGroup>,
    pub(crate) fill: Option<wgpu::BindGroup>,
    pub(crate) outline: Option<wgpu::BindGroup>,
}

pub(crate) fn weigh_overlay_tile(tile: &Arc<OverlayTileGpu>) -> usize {
    tile.device_bytes()
}

/// 256 MB desktop, divided on iOS/wasm exactly as the basemap's budgets are.
///
/// Sized for a zoom round trip on the 8-byte compact vertex, not one zoom
/// band. The zoom-continuity fix raised it from 256 to 768 MB while a
/// vertex was 88 bytes and the low-zoom national block-group tiles were
/// 2-39 MB: at 256 MB then, every z3 <-> z12 pass re-baked 98-100% of its
/// tiles (docs/measurements/2026-09-26-overlay-zoom-continuity.md). Tasks
/// 5-6 cut a vertex 11x, and the whole DC `--zoom-sweep` -- both slow and
/// both flick passes, next-zoom prefetch included -- now peaks at 140 MB
/// in this cache (docs/measurements/2026-09-25-us-block-groups-breakdown.md,
/// "Overlay memory under zoom"); 256 MB holds that with ~1.8x headroom.
/// MapLibre sizes the same cache in tiles: in view x 5 zoom levels
/// (`tile_manager.ts` `updateCacheSize`). The tiles on screen and under it
/// are also exempt from eviction (`OverlayTileSet::keep`), so the budget
/// bounds only the history.
pub(crate) const OVERLAY_TILE_GPU_BUDGET_BYTES: usize = 256 * 1024 * 1024 / OVERLAY_CACHE_DIVISOR;

/// How many device bytes of landed overlay tiles `paint` installs per
/// frame before deferring the rest to the next one (at least one tile is
/// always installed). A z3-z6 national tile is up to 39 MB and took 7.4 ms
/// to install; two landing together would have cost a frame. 16 MB is
/// ~3 ms of `install_overlay_chunks` at the measured ~5 MB/ms.
pub(crate) const OVERLAY_UPLOAD_BYTES_PER_FRAME: usize = 16 * 1024 * 1024;

/// A tiled source's GPU tiles, for one `(generation, bracket)`: a new key
/// is a new cache, which is how a stale generation's tiles are dropped.
pub(crate) struct OverlayTileGpuSet {
    pub(crate) key: OverlayTileGpuKey,
    pub(crate) cache: crate::tile_source::TileCache<Arc<OverlayTileGpu>>,
    /// The source's `(fill, outline)` paint tables, indexed by class id:
    /// one pair per key, shared by every compact tile. Built with the
    /// first compact tile; rewritten in place by a table-only paint update.
    pub(crate) tables: Option<(crate::render::PaintTableGpu, crate::render::PaintTableGpu)>,
    /// The sets this one replaced -- on a paint re-bake (a new epoch) or a
    /// re-push (a new generation) alike -- newest first, at most
    /// [`MAX_STAND_INS`]. Each fills only the cover tiles the newer sets
    /// still have a hole in ([`compose_cover`]), so the picture advances
    /// tile by tile as the new one lands and never blanks -- MapLibre's
    /// GeoJSON `setData`, where a reloaded tile keeps its old buckets
    /// until its new ones arrive. A stand-in no tile draws from is dropped
    /// that frame, and the oldest go first when the source's bytes pass
    /// one cache budget ([`stand_ins_to_keep`]).
    pub(crate) previous: Vec<OverlayTileGpuSet>,
    /// The tiles known empty for THIS set's generation, recorded when it
    /// became the stand-in for a newer generation (whose `empty` says
    /// nothing about this one's). `None`: same generation as the set it
    /// stands in for, whose `OverlayTileSet::empty` applies.
    pub(crate) empty: Option<HashSet<TileId>>,
}

/// `(generation, bracket, epoch)`: see `OverlayTileSet::epoch`.
pub(crate) type OverlayTileGpuKey = (u64, crate::overlay::plan::BakeBracket, u64);

impl OverlayTileGpuSet {
    pub(crate) fn new(key: OverlayTileGpuKey) -> Self {
        Self {
            key,
            cache: crate::tile_source::TileCache::with_byte_budget(512, OVERLAY_TILE_GPU_BUDGET_BYTES, weigh_overlay_tile),
            tables: None,
            previous: Vec::new(),
            empty: None,
        }
    }

    /// Moves this set to `key`: the outgoing set becomes the newest
    /// stand-in, in front of its own. A new epoch, a new generation and a
    /// new bracket alike (a bracket-only change used to start empty).
    ///
    /// `replaced_empty` is the outgoing generation's known-empty tiles
    /// (`OverlayTileSet::stand_in_empty`) when the generation changed:
    /// every stand-in of that generation judges emptiness by it from now
    /// on, since the new set's `empty` says nothing about theirs.
    pub(crate) fn rekey(&mut self, key: OverlayTileGpuKey, replaced_empty: Option<HashSet<TileId>>) {
        let generation_changed = self.key.0 != key.0;
        let mut old = std::mem::replace(self, Self::new(key));
        let mut previous = std::mem::take(&mut old.previous);
        previous.insert(0, old);
        previous.truncate(MAX_STAND_INS);
        if generation_changed {
            let replaced_empty = replaced_empty.unwrap_or_default();
            for stand_in in previous.iter_mut().filter(|s| s.empty.is_none()) {
                stand_in.empty = Some(replaced_empty.clone());
            }
        }
        self.previous = previous;
    }

    /// Device bytes held for this source: the current set and every
    /// stand-in.
    pub(crate) fn held_bytes(&self) -> usize {
        self.cache.held_bytes() + self.previous.iter().map(|s| s.cache.held_bytes()).sum::<usize>()
    }

    /// A landed live diff (`MapView::apply_live_result`), on the cached
    /// tiles: a tile whose geometry it `touches` is dirtied (re-bakes); one
    /// it only `recolors` takes the class-only fast path -- the changed
    /// features' paint index rewritten in place (`tile_plan::repaint_tile`)
    /// -- or, when it cannot (a legacy tile, or one baked before the source
    /// was live and so without spans), is dirtied too.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_live_diff(
        &self,
        pipeline: &crate::render::TilePipeline,
        ctx: &crate::render::GpuContext,
        set: &mut OverlayTileSet,
        rev: u64,
        touches: impl Fn(TileId) -> bool,
        recolors: impl Fn(TileId) -> bool,
        repaint: &[(u32, u16, [u32; 4])],
    ) -> LiveCacheOutcome {
        let mut out = LiveCacheOutcome::default();
        for (tile, tile_gpu) in self.cache.iter() {
            out.cached += 1;
            out.cached_ids.insert(*tile);
            if touches(*tile) {
                set.dirty.insert(*tile, rev);
                out.dirtied.push(*tile);
                out.rebaked += 1;
            }
            if repaint.is_empty() || !recolors(*tile) {
                continue;
            }
            // The class-only fast path: the changed features' vertices
            // take their new paint index in place.
            let patched = match &tile_gpu.body {
                OverlayTileBody::Compact { chunks, spans, .. } => {
                    let mut spans = spans.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    spans.as_mut().and_then(|spans| {
                        crate::overlay::tile_plan::repaint_tile(pipeline, ctx, chunks, spans, repaint)
                    })
                }
                OverlayTileBody::Legacy { .. } => None,
            };
            match patched {
                Some(n) => {
                    if n > 0 {
                        out.repainted += 1;
                        out.recolored.push(*tile);
                        set.class_rev.insert(*tile, rev);
                        if !set.dirty.contains_key(tile) {
                            set.shown_rev.insert(*tile, rev);
                        }
                    }
                }
                None => {
                    // Legacy, or baked without spans: re-baked.
                    if !touches(*tile) {
                        set.dirty.insert(*tile, rev);
                        out.dirtied.push(*tile);
                        out.rebaked += 1;
                    }
                }
            }
        }
        out
    }
}

/// What [`OverlayTileGpuSet::apply_live_diff`] did.
#[derive(Debug, Default)]
pub(crate) struct LiveCacheOutcome {
    /// Tiles cached, and which.
    pub(crate) cached: usize,
    pub(crate) cached_ids: HashSet<TileId>,
    /// Tiles dirtied (re-baking), and how many.
    pub(crate) dirtied: Vec<TileId>,
    pub(crate) rebaked: usize,
    /// Tiles recoloured in place, and how many.
    pub(crate) recolored: Vec<TileId>,
    pub(crate) repainted: usize,
}

/// Permits for overlay TILE bakes: every core but two (at least two). A
/// tile bake is exactly what the first frame of a tiled source waits on,
/// and there are a cover's worth of them at once, unlike the whole-source
/// bakes `overlay_bake_permits` bounds.
pub(crate) fn overlay_tile_bake_permits() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
    cores.saturating_sub(2).max(2)
}

/// One overlay tile bake: which source, which tile, which generation of
/// the source it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverlayBakeJob {
    pub(crate) source: crate::overlay::SourceId,
    pub(crate) tile: TileId,
    pub(crate) generation: u64,
    /// The tile set's `epoch` when this was queued: see
    /// `OverlayTileSet::epoch`.
    pub(crate) epoch: u64,
}

/// The tiled overlay's bake queue, re-prioritised against the wanted set
/// every frame -- the overlay's `fetch_queue::FetchQueue`.
///
/// It replaced one task per missing tile parked on a FIFO semaphore, which
/// baked every intermediate zoom's cover of a flick in request order while
/// the final cover waited behind it, and never dropped a bake the camera
/// had left (docs/measurements/2026-09-26-overlay-zoom-continuity.md, root
/// cause (a).1). Here the waiting jobs are dropped the frame their tile
/// leaves the wanted set, the rest are re-sorted into this frame's order
/// (coarse cover, then the visible cover nearest the eye first, then the
/// prefetch), and a RUNNING bake whose tile leaves gets its cancel flag
/// raised, which the bake polls -- MapLibre aborts a tile it no longer
/// retains the same way (`_removeTile` -> `_abortTile`).
///
/// Lives on the UI thread with the rest of `MapView`: only `pop` hands work
/// to the runtime, so there is nothing to lock.
pub(crate) struct OverlayBakeQueue {
    /// Waiting, in this frame's priority order.
    pub(crate) pending: Vec<OverlayBakeJob>,
    /// Running, with the cancel flag its bake polls.
    pub(crate) running: Vec<(OverlayBakeJob, Arc<std::sync::atomic::AtomicBool>)>,
    pub(crate) max_running: usize,
    /// Per source, the oldest epoch still worth baking (see
    /// [`Self::cancel_source`]): a running job below it stays cancelled
    /// even when `retarget` finds its tile wanted again.
    pub(crate) current_epoch: HashMap<crate::overlay::SourceId, u64>,
}

impl OverlayBakeQueue {
    pub(crate) fn new(max_running: usize) -> Self {
        Self {
            pending: Vec::new(),
            running: Vec::new(),
            max_running: max_running.max(1),
            current_epoch: HashMap::new(),
        }
    }

    pub(crate) fn stale(&self, job: &OverlayBakeJob) -> bool {
        self.current_epoch.get(&job.source).is_some_and(|&epoch| job.epoch < epoch)
    }

    pub(crate) fn push(&mut self, job: OverlayBakeJob) {
        if !self.pending.iter().chain(self.running.iter().map(|(j, _)| j)).any(|j| *j == job) {
            self.pending.push(job);
        }
    }

    /// Re-orders the waiting jobs by `rank` (lower first; `None` = no
    /// longer wanted) and returns the ones dropped. A running job that is
    /// no longer wanted has its cancel flag raised; one that is wanted
    /// again before it noticed has it lowered.
    pub(crate) fn retarget(
        &mut self,
        rank: impl Fn(&crate::overlay::SourceId, TileId) -> Option<usize>,
    ) -> Vec<OverlayBakeJob> {
        let mut dropped = Vec::new();
        let mut kept: Vec<(usize, OverlayBakeJob)> = Vec::with_capacity(self.pending.len());
        for job in self.pending.drain(..) {
            match rank(&job.source, job.tile) {
                Some(r) => kept.push((r, job)),
                None => dropped.push(job),
            }
        }
        kept.sort_by_key(|(r, _)| *r);
        self.pending = kept.into_iter().map(|(_, j)| j).collect();
        for (job, cancel) in &self.running {
            cancel.store(
                rank(&job.source, job.tile).is_none()
                    || self.stale(job),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        dropped
    }

    /// The next job to start, if a worker is free. A running job already
    /// told to stop does not hold its worker: it only has to reach its
    /// next cancel poll, and the job it would otherwise block is one the
    /// camera wants now.
    pub(crate) fn pop(&mut self) -> Option<(OverlayBakeJob, Arc<std::sync::atomic::AtomicBool>)> {
        let busy = self
            .running
            .iter()
            .filter(|(_, cancel)| !cancel.load(std::sync::atomic::Ordering::Relaxed))
            .count();
        if busy >= self.max_running || self.pending.is_empty() {
            return None;
        }
        let job = self.pending.remove(0);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.running.push((job.clone(), Arc::clone(&cancel)));
        Some((job, cancel))
    }

    /// Drops every waiting job for `source` and raises the cancel flag of
    /// every running one: a paint re-bake made them another epoch's.
    /// Returns the dropped waiting jobs.
    pub(crate) fn cancel_source(&mut self, source: &crate::overlay::SourceId, epoch: u64) -> Vec<OverlayBakeJob> {
        self.current_epoch.insert(Arc::clone(source), epoch);
        let (dropped, kept): (Vec<_>, Vec<_>) = self.pending.drain(..).partition(|job| &job.source == source);
        self.pending = kept;
        for (job, cancel) in &self.running {
            if &job.source == source {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        dropped
    }

    /// A started job finished (baked, cancelled or failed).
    pub(crate) fn finished(&mut self, source: &crate::overlay::SourceId, tile: TileId, epoch: u64) {
        if let Some(i) =
            self.running.iter().position(|(j, _)| j.tile == tile && &j.source == source && j.epoch == epoch)
        {
            self.running.swap_remove(i);
        }
    }
}


/// One live source's diff pipeline: at most one diff job in flight, the
/// rest merged into the next (see `MapView::update_source_features`).
#[derive(Default)]
pub(crate) struct LiveState {
    /// Accepted diffs not yet on a worker, with the generation they were
    /// resolved against (a push since makes them meaningless).
    pub(crate) pending: Vec<(u64, crate::overlay::live::ResolvedDiff)>,
    pub(crate) running: bool,
    /// When each accepted diff was submitted, by seq.
    pub(crate) submitted: HashMap<u64, Instant>,
    /// Landed diffs waiting for their first complete frame.
    pub(crate) awaiting: Vec<LiveLanded>,
    pub(crate) samples: Vec<LiveSample>,
}

pub(crate) struct LiveLanded {
    pub(crate) seqs: Vec<u64>,
    /// The tile set's `data_rev` this job landed at, and the tiles it
    /// dirtied: complete once every one of them on screen shows a picture
    /// cut at `rev` or later.
    pub(crate) rev: u64,
    pub(crate) tiles: Vec<TileId>,
    /// Tiles it only recoloured: see `OverlayTileSet::class_rev`.
    pub(crate) recolored: Vec<TileId>,
    pub(crate) landed: Instant,
    pub(crate) apply: Duration,
    pub(crate) tiles_rebaked: usize,
    pub(crate) tiles_repainted: usize,
    pub(crate) tiles_cached: usize,
    pub(crate) rows_changed: usize,
}

/// One landed live diff job, measured to its first complete frame: what
/// `us_block_groups --live` reports. See [`MapView::take_live_samples`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveSample {
    /// Diffs this job merged.
    pub diffs: usize,
    /// Oldest merged diff's submit -> the first frame drawn with no hole and
    /// every on-screen tile the diff dirtied showing a picture cut at the
    /// diff's revision or later (`OverlayTileSet::shown_rev`).
    pub latency: Duration,
    /// Newest merged diff's submit -> the same frame.
    pub latency_newest: Duration,
    /// Landed on the UI thread -> the same frame: the re-bakes and upload.
    pub land_to_frame: Duration,
    /// The worker's `live::apply` time.
    pub apply: Duration,
    /// Cached tiles the diff dirtied (each re-baked when next wanted).
    pub tiles_rebaked: usize,
    /// Cached tiles repainted in place (the class-only fast path).
    pub tiles_repainted: usize,
    /// Tiles cached when it landed.
    pub tiles_cached: usize,
    /// Rows re-classed or removed.
    pub rows_changed: usize,
}

impl LiveState {
    /// Called by `paint` for a tiled source on a frame drawn whole (no
    /// hole, no stand-in epoch, no dirty tile on screen): every landed diff
    /// is complete as of now.
    pub(crate) fn note_frame_complete(&mut self, done: impl Fn(&LiveLanded) -> bool) {
        let state = self;
        let now = crate::timing::Instant::now();
        let (finished, waiting): (Vec<LiveLanded>, Vec<LiveLanded>) =
            std::mem::take(&mut state.awaiting).into_iter().partition(|job| done(job));
        state.awaiting = waiting;
        for landed in finished {
            let submits: Vec<Instant> = landed.seqs.iter().filter_map(|s| state.submitted.remove(s)).collect();
            let oldest = submits.iter().copied().reduce(|a, b| if b < a { b } else { a });
            let newest = submits.iter().copied().reduce(|a, b| if b > a { b } else { a });
            let (Some(oldest), Some(newest)) = (oldest, newest) else { continue };
            state.samples.push(LiveSample {
                diffs: landed.seqs.len(),
                latency: now.duration_since(oldest),
                latency_newest: now.duration_since(newest),
                land_to_frame: now.duration_since(landed.landed),
                apply: landed.apply,
                tiles_rebaked: landed.tiles_rebaked,
                tiles_repainted: landed.tiles_repainted,
                tiles_cached: landed.tiles_cached,
                rows_changed: landed.rows_changed,
            });
        }
    }
}

/// Whether a tile's own box, grown by one pixel (8 MVT units), meets a
/// fixed-point box: a tile draws clipped to its own box, so a change in its
/// index buffer alone never reaches a pixel of it.
pub(crate) fn live_tile_touches(t: TileId, b: &[u32; 4]) -> bool {
    let span = 1i64 << (32 - u32::from(t.z));
    let m = span / 512;
    let (x0, y0) = (i64::from(t.x) * span - m, i64::from(t.y) * span - m);
    let (x1, y1) = (x0 + span + 2 * m, y0 + span + 2 * m);
    i64::from(b[2]) >= x0 && i64::from(b[0]) <= x1 && i64::from(b[3]) >= y0 && i64::from(b[1]) <= y1
}


/// How [`overlay_tile_picks`] covered one overlay cover tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayCoverDraw {
    Own,
    /// An ancestor this many levels coarser stands in for all or part of
    /// the tile (the rest is covered by loaded descendants).
    Ancestor(u8),
    /// Loaded descendants (and known-empty ones) cover the whole tile.
    Descendants,
    /// The tile, or every one of its children, is known empty.
    Empty,
    /// Some or all of the tile has nothing drawn for it.
    Hole,
}

pub(crate) fn tile_children(t: TileId) -> [TileId; 4] {
    [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(dx, dy)| TileId { z: t.z + 1, x: t.x * 2 + dx, y: t.y * 2 + dy })
}

/// What `paint` draws for one tiled-overlay cover tile: `(drawn tile, clip
/// box)` pairs, and how that covered it.
///
/// `tile` is the overlay tile wanted for the basemap cover tile `display`
/// (they differ only past the index's `max_zoom`, where `tile` is drawn
/// overzoomed and clipped to `display`).
///
/// The rule is MapLibre's `_updateRetainedTiles`
/// (`maplibre-gl-js/src/tile/tile_manager.ts:639`), not "all four children
/// or nothing": each child quadrant is covered on its own -- by the child,
/// by all of its loaded-or-empty grandchildren, or by being known empty
/// (`empty`) -- and only the quadrants left over fall back to the nearest
/// cached ancestor, clipped to that quadrant so it never paints under a
/// descendant (translucent fills would double up). Only when there is no
/// ancestor either are the leftover quadrants partially drawn and the tile
/// counted a hole. Before this rule a coastal tile, whose empty children are
/// never cached, could never be covered by descendants, and a tile with 3
/// of 4 children loaded was dropped outright.
pub(crate) fn overlay_tile_picks(
    tile: TileId,
    display: TileId,
    cached: impl Fn(TileId) -> bool,
    empty: impl Fn(TileId) -> bool,
) -> (Vec<(TileId, crate::render::TileClip)>, OverlayCoverDraw) {
    use crate::render::TileClip;
    if empty(tile) {
        return (Vec::new(), OverlayCoverDraw::Empty);
    }
    if cached(tile) {
        return (vec![(tile, TileClip::for_descendant(tile, display))], OverlayCoverDraw::Own);
    }
    let ancestor = find_cached_ancestor(tile, &cached);
    let levels = |a: TileId| tile.z - a.z;
    if display != tile {
        // Overzoomed: `tile` is the index's deepest level, so it has no
        // descendants to look for.
        return match ancestor {
            Some(a) => (vec![(a, TileClip::for_descendant(a, display))], OverlayCoverDraw::Ancestor(levels(a))),
            None => (Vec::new(), OverlayCoverDraw::Hole),
        };
    }
    let mut picks = Vec::new();
    let mut uncovered: Vec<(TileId, Vec<TileId>)> = Vec::new();
    for child in tile_children(tile) {
        if cached(child) {
            picks.push((child, TileClip::for_descendant(child, child)));
            continue;
        }
        if empty(child) {
            continue;
        }
        let grand = tile_children(child);
        let loaded: Vec<TileId> = grand.iter().copied().filter(|&g| cached(g)).collect();
        if !loaded.is_empty() && grand.iter().all(|&g| cached(g) || empty(g)) {
            picks.extend(loaded.into_iter().map(|g| (g, TileClip::for_descendant(g, g))));
        } else {
            uncovered.push((child, loaded));
        }
    }
    if uncovered.is_empty() {
        let draw = if picks.is_empty() { OverlayCoverDraw::Empty } else { OverlayCoverDraw::Descendants };
        return (picks, draw);
    }
    match ancestor {
        Some(a) if picks.is_empty() => {
            (vec![(a, TileClip::for_descendant(a, display))], OverlayCoverDraw::Ancestor(levels(a)))
        }
        Some(a) => {
            picks.extend(uncovered.into_iter().map(|(child, _)| (a, TileClip::for_descendant(a, child))));
            (picks, OverlayCoverDraw::Ancestor(levels(a)))
        }
        None => {
            for (_, loaded) in uncovered {
                picks.extend(loaded.into_iter().map(|g| (g, TileClip::for_descendant(g, g))));
            }
            (picks, OverlayCoverDraw::Hole)
        }
    }
}


/// Walks up from `tile` toward zoom 0, returning the first ancestor tile
/// for which `is_cached` returns true — used to find a lower-resolution
/// placeholder for a tile that hasn't finished loading yet. Takes a
/// closure rather than the concrete cache type so this logic is testable
/// without any GPU state.
pub(crate) fn find_cached_ancestor(tile: TileId, is_cached: impl Fn(TileId) -> bool) -> Option<TileId> {
    let mut current = tile;
    while current.z > 0 {
        current = TileId { z: current.z - 1, x: current.x / 2, y: current.y / 2 };
        if is_cached(current) {
            return Some(current);
        }
    }
    None
}

/// The CPU half of the overlay orchestration: every tiled source's
/// [`OverlayTileSet`], the one [`OverlayBakeQueue`] they share, and each
/// live source's diff pipeline. Executor-free; see the module doc.
pub struct OverlayHost {
    pub(crate) tile_sets: HashMap<crate::overlay::SourceId, OverlayTileSet>,
    pub(crate) bake_queue: OverlayBakeQueue,
    pub(crate) live: HashMap<crate::overlay::SourceId, LiveState>,
}

impl OverlayHost {
    /// A host whose bake queue runs at most `max_running` bakes at once
    /// (native: [`overlay_tile_bake_permits`]; inline shells: 1).
    pub fn new(max_running: usize) -> Self {
        Self { tile_sets: HashMap::new(), bake_queue: OverlayBakeQueue::new(max_running), live: HashMap::new() }
    }
}

/// Which path a source paint update took (`MapView::update_source_paint`,
/// [`ShellOverlays::update_source_paint`]). See `MapView`'s for the three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePaintPath {
    /// Only the source's paint tables were rewritten.
    TableOnly,
    /// The tile index was re-classed and the cover is re-baking.
    Rebake,
    /// The source was replanned and is re-baking as a re-push would.
    Replan,
}

/// What [`OverlayHost::apply_source_paint`] decided, for its caller to
/// carry out: the table write on its GPU half, nothing more (the re-bake is
/// already queued), or a replan -- which needs the caller's display zoom,
/// and so is the caller's.
pub(crate) enum PaintDecision {
    Table(TableWrite),
    Rebake,
    Replan,
}

/// A table-only paint update's GPU half: the source's two paint tables,
/// for the GPU set held at `key` (`OverlayGpu::write_paint_tables`).
pub(crate) struct TableWrite {
    pub(crate) source: crate::overlay::SourceId,
    pub(crate) key: OverlayTileGpuKey,
    pub(crate) fill: crate::render::compact::PaintTable,
    pub(crate) outline: crate::render::compact::PaintTable,
}

impl OverlayHost {
    /// Everything a source paint update does on the CPU, short of the
    /// replan, the verification and the GPU write: moved from `MapView`
    /// so every shell takes the one path.
    ///
    /// The table-only path resolves the new paint for ONE feature per
    /// class -- a few hundred resolves rather than every feature -- and
    /// writes the tables from that at once. That is exact whenever every
    /// class's features still share a paint; a paint that splits one is
    /// what the full check ([`Self::finish_source_paint_check`]) catches.
    pub(crate) fn apply_source_paint(
        &mut self,
        store: &mut crate::overlay::OverlayStore,
        name: &str,
        paint: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PaintDecision, crate::overlay::FeatureError> {
        if store.source_id(name).is_none() {
            return Err(crate::overlay::FeatureError::SourcePaint {
                source: name.to_string(),
                reason: "no such source".into(),
            });
        }
        store.declare_source_paint(name, paint)?;
        let snapshot = store.source_paint_snapshot(name).expect("checked above");
        let source = Arc::clone(&snapshot.source);
        // Point features draw from the whole-source plan, which bakes the
        // colours into its vertices: only a replan repaints them.
        let has_points = store.source_plan(name).is_some_and(|plan| !plan.plan.ops.is_empty());
        let set = match self.tile_sets.get_mut(&source) {
            Some(set) if snapshot.tiled && set.generation == snapshot.generation && !has_points => set,
            _ => return Ok(PaintDecision::Replan),
        };

        // One representative feature per class: its first.
        let mut first: Vec<Option<usize>> = vec![None; set.classes.classes.len()];
        for (at, &class) in set.classes.of_feature.iter().enumerate() {
            if let Some(slot) = first.get_mut(usize::from(class)) {
                slot.get_or_insert(at);
            }
        }
        // Picked by index into the snapshot, and classed against the
        // source's full property table: no feature or row is copied.
        let representatives: Option<Vec<usize>> = first.into_iter().collect();
        let quick = representatives
            .and_then(|picks| {
                crate::overlay::plan::classify_picked(
                    &snapshot.features,
                    &picks,
                    &snapshot.properties,
                    set.bracket,
                    &snapshot.paint,
                )
            })
            .filter(|quick| {
                quick.classes.len() == set.classes.classes.len()
                    && quick.of_feature.iter().enumerate().all(|(i, &class)| usize::from(class) == i)
            })
            .map(|quick| crate::overlay::plan::FeatureClasses {
                classes: quick.classes,
                of_feature: set.classes.of_feature.clone(),
                data_driven: quick.data_driven,
            })
            .filter(|quick| crate::overlay::tile_plan::classes_differ_only_in_colour(&set.classes, quick));
        if let Some(quick) = quick {
            set.verify.unverified = Some(set.paint_rev + 1);
            return Ok(self.apply_source_classes(&source, quick));
        }

        // The representatives disagree (classes merged, or a kind or
        // hairline changed): classify everything, here.
        let Some(classes) =
            crate::overlay::plan::classify(&snapshot.features, &snapshot.properties, set.bracket, &snapshot.paint)
        else {
            return Ok(PaintDecision::Replan);
        };
        set.verify.unverified = None;
        Ok(self.apply_source_classes(&source, classes))
    }

    /// Installs a tiled source's new classes: a table rewrite when only
    /// colours moved, a re-bake on the same index otherwise.
    pub(crate) fn apply_source_classes(
        &mut self,
        source: &crate::overlay::SourceId,
        classes: crate::overlay::plan::FeatureClasses,
    ) -> PaintDecision {
        let Some(set) = self.tile_sets.get_mut(source) else { return PaintDecision::Replan };
        if crate::overlay::tile_plan::classes_differ_only_in_colour(&set.classes, &classes) {
            set.classes = Arc::new(classes);
            set.paint_rev += 1;
            // A landed legacy tile carries the old colours in its vertices.
            set.landed.retain(|(_, baked)| baked.is_ok());
            let key = (set.generation, set.bracket, set.epoch);
            let (fill, outline) = (
                crate::render::compact::paint_table(&crate::overlay::tile_plan::fill_paint_entries(&set.classes)),
                crate::render::compact::paint_table(&crate::overlay::tile_plan::outline_paint_entries(&set.classes)),
            );
            return PaintDecision::Table(TableWrite { source: Arc::clone(source), key, fill, outline });
        }

        // The classes changed: same geometry, new class ids, a new epoch.
        set.index = Arc::new(set.index.reclassified(&classes.of_feature));
        set.classes = Arc::new(classes);
        set.epoch += 1;
        set.paint_rev += 1;
        set.in_flight.clear();
        set.landed.clear();
        let epoch = set.epoch;
        drop(self.bake_queue.cancel_source(source, epoch));
        PaintDecision::Rebake
    }

    /// A live diff grew `source`'s class table past its texture: every tile
    /// to a new epoch, the old picture standing in (the re-bake path). A
    /// table-only paint's pending full check is dropped with it -- the new
    /// epoch bakes every tile from the current classes, and a check still
    /// owed at the old `paint_rev` would be stale on every landing and
    /// respawn forever.
    pub(crate) fn rebake_after_table_overflow(&mut self, source: &crate::overlay::SourceId) {
        let Some(set) = self.tile_sets.get_mut(source) else { return };
        set.epoch += 1;
        set.paint_rev += 1;
        set.in_flight.clear();
        set.landed.clear();
        set.dirty.clear();
        set.verify.unverified = None;
        let epoch = set.epoch;
        drop(self.bake_queue.cancel_source(source, epoch));
    }

    /// Lands a full check made for `(epoch, rev)`. Stale (a newer update
    /// since): discarded -- `None` -- and the caller checks again. Current:
    /// the full classes are installed, which is the same colours when the
    /// representatives were right (`TableOnly`, nothing to redraw) and a
    /// re-bake when they were not. `Replan`: the paint could not be
    /// classed, and the caller replans.
    pub(crate) fn finish_source_paint_check(
        &mut self,
        source: &crate::overlay::SourceId,
        epoch: u64,
        rev: u64,
        classes: Option<crate::overlay::plan::FeatureClasses>,
    ) -> Option<SourcePaintPath> {
        let set = self.tile_sets.get_mut(source)?;
        if set.epoch != epoch || set.verify.unverified != Some(rev) || set.paint_rev != rev {
            return None;
        }
        set.verify.unverified = None;
        let Some(classes) = classes else {
            return Some(SourcePaintPath::Replan);
        };
        if crate::overlay::tile_plan::classes_differ_only_in_colour(&set.classes, &classes) {
            // The representatives were right: nothing to redraw.
            set.classes = Arc::new(classes);
            return Some(SourcePaintPath::TableOnly);
        }
        // The same colour-only test just failed, so this is the re-bake
        // (a table write cannot come out of it).
        Some(match self.apply_source_classes(source, classes) {
            PaintDecision::Table(_) => {
                debug_assert!(false, "the colour-only test just failed: no table write can come out of it");
                SourcePaintPath::TableOnly
            }
            PaintDecision::Rebake => SourcePaintPath::Rebake,
            PaintDecision::Replan => SourcePaintPath::Replan,
        })
    }
}

impl OverlayGpu {
    /// A table-only paint update's GPU half: rewrites the source's two
    /// paint tables in place (no tile, mesh or bind group is touched), and
    /// evicts its legacy tiles, whose colours are in their vertices.
    /// Nothing, when the GPU does not hold the source at `write.key`.
    pub(crate) fn write_paint_tables(
        &mut self,
        pipeline: &crate::render::TilePipeline,
        ctx: &crate::render::GpuContext,
        write: &TableWrite,
    ) {
        let (key, fill, outline) = (write.key, &write.fill, &write.outline);
        if let Some(held) = self.tile_gpu.get_mut(&write.source).filter(|held| held.key == key) {
            if let Some((fill_gpu, outline_gpu)) = held.tables.as_ref() {
                let written =
                    pipeline.write_paint_table(ctx, fill_gpu, fill) && pipeline.write_paint_table(ctx, outline_gpu, outline);
                debug_assert!(written, "an unchanged class count is an unchanged table size");
            }
            // A stand-in of the same generation and epoch (only a
            // bracket apart) shares these classes: recoloured with
            // them, so no cover tile keeps the old colours. One of
            // another generation or epoch is the OLD picture, old
            // classes and all, and keeps its colours until the new
            // one's tiles replace it. (`set.bracket` is fixed per
            // built set, so today every stand-in is the latter.)
            for stand_in in held.previous.iter().filter(|p| p.key.0 == key.0 && p.key.2 == key.2) {
                if let Some((fill_gpu, outline_gpu)) = stand_in.tables.as_ref() {
                    let _ = pipeline.write_paint_table(ctx, fill_gpu, fill)
                        && pipeline.write_paint_table(ctx, outline_gpu, outline);
                }
            }
            // Legacy tiles bake colour into their vertices: evicted,
            // and re-baked with the new colours when next wanted.
            let legacy: Vec<TileId> = held
                .cache
                .iter()
                .filter(|(_, tile)| matches!(tile.body, OverlayTileBody::Legacy { .. }))
                .map(|(id, _)| *id)
                .collect();
            for id in legacy {
                held.cache.remove(&id);
            }
        }
    }
}

/// The per-GPU-context half: each tiled source's tile cache (and its
/// paint-rebake stand-in set) and each point source's circle layers.
pub struct OverlayGpu {
    pub(crate) tile_gpu: HashMap<crate::overlay::SourceId, OverlayTileGpuSet>,
    pub(crate) point_circles: HashMap<crate::overlay::SourceId, Vec<PointCircleGpu>>,
    /// The per-source byte budget stand-ins are trimmed against
    /// ([`stand_ins_to_keep`]): `OVERLAY_TILE_GPU_BUDGET_BYTES`, or a small
    /// one a test injects to reach the cap with a small fixture.
    pub(crate) tile_budget: usize,
    /// Each source's per-frame `line_fx` (its trim and opacity,
    /// [`SourceLineFx::uniform`]), written INTO every model uniform the
    /// source's draws get this frame -- point circles and tiles alike --
    /// so no whole-uniform write can reset it. A source not in here draws
    /// with `LINE_FX_NONE`. The host (`MapView`) replaces it every frame
    /// before `sync_points` / `prepare_tiles`.
    pub(crate) line_fx: HashMap<crate::overlay::SourceId, [f32; 4]>,
}

impl Default for OverlayGpu {
    fn default() -> Self {
        Self {
            tile_gpu: HashMap::new(),
            point_circles: HashMap::new(),
            tile_budget: OVERLAY_TILE_GPU_BUDGET_BYTES,
            line_fx: HashMap::new(),
        }
    }
}

/// A source's per-frame line effects: the VISIBLE range of line progress
/// (`None` = the whole line) and an opacity multiplier on its alpha. What
/// `MapView::set_source_line_trim` and `set_source_opacity` store; drawn as
/// the model uniform's `line_fx` with no re-bake.
///
/// `trim` is the kept range, the COMPLEMENT of Mapbox's `line-trim-offset`
/// (which names the hidden range): `[0.0, 0.3]` draws the first 30% of
/// each line. A trim only cuts a source with `lineMetrics`; the opacity
/// fades every draw of the source (fills, strokes, circles), but not 3D
/// buildings (`extrusion_shader.wgsl` ignores `line_fx`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceLineFx {
    pub trim: Option<[f32; 2]>,
    pub opacity: f32,
}

impl Default for SourceLineFx {
    fn default() -> Self {
        Self { trim: None, opacity: 1.0 }
    }
}

impl SourceLineFx {
    /// `(keep_start, keep_end, opacity, trim_on)`, the model uniform's
    /// `line_fx`. The default is exactly `render::LINE_FX_NONE`.
    pub fn uniform(&self) -> [f32; 4] {
        match self.trim {
            Some([start, end]) => [start, end, self.opacity, 1.0],
            None => [0.0, 1.0, self.opacity, 0.0],
        }
    }
}

impl OverlayGpu {
    /// Source `name`'s `line_fx` this frame.
    pub(crate) fn line_fx_of(&self, name: &str) -> [f32; 4] {
        self.line_fx.get(name).copied().unwrap_or(crate::render::LINE_FX_NONE)
    }
}
/// One overlay tile bake, handed out by [`OverlayHost::next_bake`]: `Send`
/// and `'static`, so the shell runs it wherever it runs work -- tokio's
/// blocking pool on native, inline under a frame budget in the browser --
/// and hands the result back through [`OverlayHost::land_bake`].
pub struct TileBake {
    pub(crate) ticket: BakeTicket,
    pub(crate) index: Arc<crate::overlay::tile_index::TileIndex>,
    pub(crate) classes: Arc<crate::overlay::plan::FeatureClasses>,
    pub(crate) live: Arc<std::sync::atomic::AtomicU64>,
    /// The source has taken a live diff (`OverlayHost::live` holds it):
    /// bake one op per feature, with the spans a class-only diff repaints
    /// in place (`tile_plan::bake_overlay_tile_compact`).
    pub(crate) per_feature: bool,
}

/// Which bake a result belongs to: the few words [`OverlayHost::land_bake`]
/// needs, without the index and classes the bake itself held (those drop
/// wherever the bake ran, never on the thread that lands it).
#[derive(Clone)]
pub struct BakeTicket {
    pub(crate) source: crate::overlay::SourceId,
    pub(crate) tile: TileId,
    pub(crate) generation: u64,
    pub(crate) epoch: u64,
    pub(crate) paint_rev: u64,
    pub(crate) data_rev: u64,
    pub(crate) cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// What a [`TileBake`] produced: its ticket, the tile (`None` when the cut
/// was empty or the bake was abandoned), and whether it was abandoned by its
/// cancel flag.
pub struct TileBaked {
    pub(crate) ticket: BakeTicket,
    pub(crate) baked: Option<crate::overlay::tile_plan::BakedOverlayTile>,
    pub(crate) cancelled: bool,
}

impl TileBaked {
    /// A bake that never ran to completion (its worker died): nothing
    /// baked, and cancelled exactly when its cancel flag was raised.
    pub fn abandoned(ticket: BakeTicket) -> Self {
        let cancelled = ticket.cancel.load(std::sync::atomic::Ordering::Relaxed);
        Self { ticket, baked: None, cancelled }
    }
}

impl TileBake {
    /// The tile this bake cuts.
    pub fn tile(&self) -> TileId {
        self.ticket.tile
    }

    /// This bake's ticket, kept by a shell that hands the bake to a worker
    /// which may die: [`TileBaked::abandoned`] lands it anyway.
    pub fn ticket(&self) -> BakeTicket {
        self.ticket.clone()
    }

    /// Cuts the tile: compact (8-byte vertex) unless it has strokes, which
    /// keep the 88-byte mesh. Polls the source's live stamp and the cancel
    /// flag, and gives up (`None`) as soon as either says the work is moot.
    /// Consumes the bake, so its index and classes are released on the
    /// thread that ran it.
    pub fn run(self) -> TileBaked {
        let (live, cancel, generation, tile) = (&self.live, &self.ticket.cancel, self.ticket.generation, self.ticket.tile);
        let superseded = || {
            live.load(std::sync::atomic::Ordering::Relaxed) != generation
                || cancel.load(std::sync::atomic::Ordering::Relaxed)
        };
        let baked = if superseded() {
            #[cfg(feature = "probe")]
            crate::render::zoom_probe::bake_finished(tile, false, true);
            None
        } else {
            #[cfg(feature = "probe")]
            crate::render::zoom_probe::bake_started(tile);
            let started = crate::timing::Instant::now();
            // Identifies this bake for `GPUI_MAP_DUMP_BAD_NORMALS`, should
            // its tessellation log a bad-normal warning -- see
            // `render::bad_normal_dump`'s module doc for why this is a
            // thread-local rather than a parameter threaded all the way
            // down to `mesh::append_ops_with`.
            let _diag = crate::render::bad_normal_dump::with_bake_context(
                crate::render::bad_normal_dump::BakeContext {
                    source: self.ticket.source.to_string(),
                    tiled: true,
                    tile: Some(tile),
                    origin: None,
                },
            );
            let baked =
                crate::overlay::tile_plan::bake_overlay_tile_compact(
                    &self.index,
                    &self.classes,
                    tile,
                    self.per_feature,
                    &superseded,
                );
            #[cfg(feature = "probe")]
            crate::render::zoom_probe::bake_finished(tile, baked.is_some(), superseded());
            crate::render::alloc::note_overlay_tile_bake(
                u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
                baked.as_ref().map_or(0, |b| match b {
                    Ok(compact) => compact.vertices() as u64,
                    Err(legacy) => legacy.mesh.vertices.len() as u64,
                }),
            );
            baked
        };
        // Gave up -- the queue dropped it, OR a re-push moved the live
        // stamp -- is not "the cut came back empty". The stamp moves at
        // the push, long before the new generation's tile set installs;
        // landed as empty, a superseded bake marked the tile known-empty
        // in the OLD set, which then, standing in for the new push,
        // answered "nothing here" over the older picture that drew it.
        let cancelled = baked.is_none() && superseded();
        TileBaked { ticket: self.ticket, baked, cancelled }
    }
}

/// A tiled source's index and classes, built from its store snapshot:
/// [`OverlayHost::build_tile_set`]'s output, installed by
/// [`OverlayHost::install_tile_set`].
pub struct TileSetBuild {
    pub(crate) index: Arc<crate::overlay::tile_index::TileIndex>,
    pub(crate) classes: Arc<crate::overlay::plan::FeatureClasses>,
    pub(crate) took: Duration,
}

impl OverlayHost {
    /// Classifies a tiled source's features and builds (or, for an adopted
    /// index, re-classes) its tile index. Pure: the shell runs it wherever
    /// it runs work. `Err(())` when the paints cannot be classed (the
    /// caller falls the source back to the whole-source path), `Ok(None)`
    /// when `cancelled` said stop.
    #[allow(clippy::result_unit_err)]
    pub fn build_tile_set(
        features: &[crate::overlay::OverlayFeature],
        properties: &crate::overlay::PropertyTable,
        paint: &crate::overlay::source_paint::SourcePaint,
        adopted: Option<&crate::overlay::tile_index::TileIndex>,
        options: crate::overlay::tile_index::TileIndexOptions,
        bracket: crate::overlay::plan::BakeBracket,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Option<TileSetBuild>, ()> {
        let started = crate::timing::Instant::now();
        let Some(classes) = crate::overlay::plan::classify(features, properties, bracket, paint) else {
            return Err(());
        };
        let index = match adopted {
            Some(adopted) => Some(adopted.reclassified(&classes.of_feature)),
            None => crate::overlay::tile_index::TileIndex::build(
                features,
                &classes.of_feature,
                options,
                cancelled,
            ),
        };
        let took = started.elapsed();
        Ok(index.map(|index| TileSetBuild { index: Arc::new(index), classes: Arc::new(classes), took }))
    }

    /// Installs a built tile set for `source` at `generation`, replacing any
    /// older one. The older one's GPU tiles keep drawing, as the new set's
    /// stand-in, until the new set covers the view
    /// (`OverlayTileGpuSet::previous`).
    pub fn install_tile_set(
        &mut self,
        source: &crate::overlay::SourceId,
        generation: u64,
        bracket: crate::overlay::plan::BakeBracket,
        build: TileSetBuild,
        live: Arc<std::sync::atomic::AtomicU64>,
        pushed_at: impl Into<Instant>,
    ) {
        let mut set = OverlayTileSet::new(generation, bracket, build.index, build.classes, live, pushed_at);
        // Two installs before one frame: the GPU set is still keyed at the
        // generation the first one replaced, so ITS empties are the ones
        // the stand-in needs, not the skipped generation's.
        set.stand_in_empty = self
            .tile_sets
            .remove(source)
            .filter(|old| old.generation != generation)
            .map(|old| old.stand_in_empty.unwrap_or(old.empty));
        self.tile_sets.insert(Arc::clone(source), set);
    }

    /// Drops everything the host holds for `name`.
    pub fn forget(&mut self, name: &str) {
        self.tile_sets.retain(|source, _| &**source != name);
        self.live.retain(|source, _| &**source != name);
    }

    /// Whether any tiled source is held.
    pub fn has_tiled_sources(&self) -> bool {
        !self.tile_sets.is_empty()
    }

    /// Asks for every overlay tile `wanted` needs that is not on the GPU,
    /// queued, awaiting upload or known empty, then retargets the bake
    /// queue to `wanted` (see `OverlayBakeQueue`). The shell then starts
    /// what fits with [`Self::next_bake`].
    ///
    /// `wanted` is basemap cover tiles in priority order -- this frame's
    /// overlay coarse cover, then the visible cover nearest the eye first
    /// -- mapped per source through `wanted_overlay_tiles` (overzoom past
    /// `max_zoom` asks for the `max_zoom` ancestor, once).
    pub fn request_tiles(&mut self, gpu: Option<&OverlayGpu>, wanted: &[TileId]) {
        if self.tile_sets.is_empty() && self.bake_queue.pending.is_empty() {
            return;
        }
        let mut rank: HashMap<(crate::overlay::SourceId, TileId), usize> = HashMap::new();
        let dropped = {
            for (source, set) in &mut self.tile_sets {
                let key = (set.generation, set.bracket, set.epoch);
                let held = gpu.and_then(|gpu| gpu.tile_gpu.get(source)).filter(|held| held.key == key);
                let tiles: Vec<TileId> = crate::overlay::tile_plan::wanted_overlay_tiles(wanted, set.index.max_zoom())
                    .into_iter()
                    .map(|(_, t)| t)
                    .collect();
                #[cfg(feature = "probe")]
                crate::render::zoom_probe::overlay_wanted(&tiles);
                set.keep.clear();
                set.keep.extend(set.drawn_last.iter().copied());
                for &tile in &tiles {
                    // The tile and its whole ancestor chain: the chain is
                    // what `overlay_tile_picks` stands in from on zoom-out,
                    // and one or two tiles a level, so pinning it is cheap.
                    let mut t = tile;
                    set.keep.insert(t);
                    while t.z > 0 {
                        t = TileId { z: t.z - 1, x: t.x / 2, y: t.y / 2 };
                        set.keep.insert(t);
                    }
                }
                for tile in tiles {
                    let next = rank.len();
                    if rank.contains_key(&(Arc::clone(source), tile)) {
                        continue;
                    }
                    rank.insert((Arc::clone(source), tile), next);
                    if !set.wants(tile, |t| held.is_some_and(|held| held.cache.contains(t))) {
                        continue;
                    }
                    if set.index.is_empty_at(tile) {
                        set.empty.insert(tile);
                        continue;
                    }
                    #[cfg(feature = "probe")]
                    crate::render::zoom_probe::bake_requested(tile, next);
                    set.in_flight.insert(tile);
                    self.bake_queue.push(OverlayBakeJob {
                        source: Arc::clone(source),
                        tile,
                        generation: set.generation,
                        epoch: set.epoch,
                    });
                }
            }
            self.bake_queue.retarget(|source, tile| rank.get(&(Arc::clone(source), tile)).copied())
        };
        for job in dropped {
            #[cfg(feature = "probe")]
            crate::render::zoom_probe::bake_dropped(job.tile);
            if let Some(set) = self.tile_sets.get_mut(&job.source) {
                if set.generation == job.generation && set.epoch == job.epoch {
                    set.in_flight.remove(&job.tile);
                }
            }
        }
    }

    /// The next queued bake, if a worker is free: a job whose tile set has
    /// moved on (another generation or epoch) is finished on the spot and
    /// skipped.
    pub fn next_bake(&mut self) -> Option<TileBake> {
        while let Some((job, cancel)) = self.bake_queue.pop() {
            let Some(set) = self
                .tile_sets
                .get(&job.source)
                .filter(|set| set.generation == job.generation && set.epoch == job.epoch)
            else {
                self.bake_queue.finished(&job.source, job.tile, job.epoch);
                continue;
            };
            let OverlayBakeJob { source, tile, generation, epoch } = job;
            let per_feature = self.live.contains_key(&source);
            return Some(TileBake {
                ticket: BakeTicket {
                    source,
                    tile,
                    generation,
                    epoch,
                    paint_rev: set.paint_rev,
                    data_rev: set.data_rev,
                    cancel,
                },
                index: Arc::clone(&set.index),
                classes: Arc::clone(&set.classes),
                live: Arc::clone(&set.live),
                per_feature,
            });
        }
        None
    }

    /// Hands a finished [`TileBake`] back. `true` when the tile was
    /// accepted into its set's `landed` (the shell should redraw).
    pub fn land_bake(&mut self, result: TileBaked) -> bool {
        let TileBaked { ticket, baked, cancelled } = result;
        let BakeTicket { ref source, tile, generation, epoch, paint_rev, data_rev, .. } = ticket;
        self.bake_queue.finished(source, tile, epoch);
        let Some(set) = self.tile_sets.get_mut(source) else { return false };
        if cancelled {
            // Left the wanted set mid-bake: not empty, just not baked.
            // Asked for again if it comes back.
            if set.generation == generation && set.epoch == epoch {
                set.in_flight.remove(&tile);
            }
            #[cfg(feature = "probe")]
            crate::render::zoom_probe::bake_landed(tile, false);
            false
        } else {
            let accepted = set.accept_landed_at(generation, epoch, paint_rev, data_rev, tile, baked);
            #[cfg(feature = "probe")]
            crate::render::zoom_probe::bake_landed(tile, accepted);
            accepted
        }
    }
}

/// What ONE overlay source drew in one painted frame: the per-source
/// breakdown of `AllocationStats::draw_calls_last_frame`, plus the
/// source's labels and icons.
///
/// Geometry is counted from the frame's own draw list, after culling:
/// whole-source chunks, tiled tiles, a tiled source's stand-ins (an older
/// generation's tiles, or an ancestor, drawn while the new ones bake),
/// fill hairlines and point circles. `symbols` counts the source's labels
/// and icons that survived collision placement in the symbol pass. A
/// source's highlight is not included.
///
/// [`Self::drew`] is the "did not go blank" assertion a consumer
/// re-pushing a source wants; read it together with [`Self::frame`], which
/// says WHICH frame the figures are from. Two reads with the same `frame`
/// are the same frame (no paint happened between them), not two frames
/// that both drew.
///
/// Per view and per frame, which is why it is not a field of the
/// process-global `AllocationStats`: see `MapView::source_draw_stats` and
/// [`ShellOverlays::source_draw_stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct SourceDrawStats {
    /// Geometry draw calls issued for the source (a hairline pass is one
    /// more).
    pub draw_calls: u64,
    /// Triangles those draws rasterise; see
    /// [`crate::render::TileDraw::triangles`]. Hairlines draw lines and
    /// add nothing here.
    pub triangles: u64,
    /// Labels and icons of the source placed by the symbol pass (a feature
    /// with both counts once). Always 0 on [`ShellOverlays`], whose shells
    /// draw no overlay symbols.
    pub symbols: u64,
    /// The painted frame these figures are from: a per-view counter that
    /// moves by one per frame that reached the draw list. `0` when no frame
    /// has been painted since the source was added (or removed and added
    /// again): the zeros beside it are "not yet drawn", not "drew nothing".
    pub frame: u64,
}

impl SourceDrawStats {
    /// The geometry stats of `draws`, which should be one source's slice of
    /// a frame's draw list. `symbols` and `frame` are 0.
    #[must_use]
    pub fn of(draws: &[crate::render::TileDraw<'_>]) -> Self {
        Self {
            draw_calls: draws.len() as u64,
            triangles: draws.iter().map(crate::render::TileDraw::triangles).sum(),
            symbols: 0,
            frame: 0,
        }
    }

    /// Whether anything of the source was drawn: any geometry draw call
    /// (a hairline-only source included) or any placed label or icon.
    #[must_use]
    pub fn drew(&self) -> bool {
        self.draw_calls > 0 || self.symbols > 0
    }
}

/// Every source's [`SourceDrawStats`] for the last painted frame. Owned by
/// `MapView` and by [`ShellOverlays`]; filled by [`emit_source_draws`]
/// (and, for symbols, [`Self::add_symbols`]) and read without allocating.
#[derive(Debug, Default)]
pub(crate) struct SourceDrawLedger {
    frame: u64,
    entries: Vec<(crate::overlay::SourceId, SourceDrawStats)>,
}

impl SourceDrawLedger {
    /// Starts a new painted frame: every entry is dropped, so a source
    /// that no longer draws cannot keep an old figure.
    pub(crate) fn begin_frame(&mut self) {
        self.frame += 1;
        self.entries.clear();
    }

    fn entry(&mut self, name: &crate::overlay::SourceId) -> &mut SourceDrawStats {
        let frame = self.frame;
        match self.entries.iter().position(|(source, _)| source == name) {
            Some(at) => &mut self.entries[at].1,
            None => {
                self.entries.push((Arc::clone(name), SourceDrawStats { frame, ..SourceDrawStats::default() }));
                &mut self.entries.last_mut().expect("just pushed").1
            }
        }
    }

    /// Adds `draws` -- one source's slice of the frame's draw list -- to
    /// `name`'s figures for this frame, creating its entry (so a source
    /// that drew nothing reads a fresh zero, not a missing one).
    pub(crate) fn record(&mut self, name: &crate::overlay::SourceId, draws: &[crate::render::TileDraw<'_>]) {
        let add = SourceDrawStats::of(draws);
        let entry = self.entry(name);
        entry.draw_calls += add.draw_calls;
        entry.triangles += add.triangles;
    }

    /// Adds `count` placed labels/icons to `name` for this frame.
    pub(crate) fn add_symbols(&mut self, name: &crate::overlay::SourceId, count: u64) {
        self.entry(name).symbols += count;
    }

    /// Attributes placed overlay symbols to their sources: `sort_keys` are
    /// the placed symbols' sort keys (`LabelDrawPlan::overlay_sort_keys`),
    /// and `runs` each labelled source's `[start, end)` run of them, as
    /// `MapView::paint` assigns them (a running key: source base + feature
    /// index, so runs are disjoint). A key in no run is ignored.
    pub(crate) fn add_placed_symbols(
        &mut self,
        runs: &[(crate::overlay::SourceId, usize, usize)],
        sort_keys: &[f32],
    ) {
        for &key in sort_keys {
            let key = key as usize;
            if let Some((source, ..)) = runs.iter().find(|(_, start, end)| (*start..*end).contains(&key)) {
                self.add_symbols(source, 1);
            }
        }
    }

    /// Drops `name`'s figures: it was removed, and a source added again
    /// under the same name must not inherit them.
    pub(crate) fn forget(&mut self, name: &str) {
        self.entries.retain(|(source, _)| &**source != name);
    }

    /// `name`'s figures for the last painted frame; for a source that has
    /// no entry (added since, or re-added after a removal), zeros stamped
    /// frame 0.
    pub(crate) fn get(&self, name: &str) -> SourceDrawStats {
        self.entries.iter().find(|(source, _)| &**source == name).map(|(_, stats)| *stats).unwrap_or_default()
    }
}

/// Appends every source's overlay draws to the frame's draw lists and
/// records what each drew in `ledger` -- the per-source loop of
/// `MapView::paint`, shared so a test can run it on a real draw list.
///
/// For each `(name, above_labels)` in `slotted`, in order: `counted`
/// appends the source's geometry (tiles, stand-ins, whole-source chunks,
/// circles) to `above` or `below`, and that slice is recorded; then
/// `uncounted` appends what is drawn with the source but is not its data
/// (its highlight).
///
/// Test-only since `ShellOverlays::push_draws_split` took the
/// three-pass [`emit_source_draws_by_pass`] directly.
#[cfg(test)]
pub(crate) fn emit_source_draws<'n, 'a>(
    ledger: &mut SourceDrawLedger,
    slotted: impl IntoIterator<Item = (&'n crate::overlay::SourceId, bool)>,
    below: &mut Vec<crate::render::TileDraw<'a>>,
    above: &mut Vec<crate::render::TileDraw<'a>>,
    counted: impl FnMut(&crate::overlay::SourceId, &mut Vec<crate::render::TileDraw<'a>>),
    uncounted: impl FnMut(&crate::overlay::SourceId, &mut Vec<crate::render::TileDraw<'a>>),
) {
    let mut over_buildings = Vec::new();
    emit_source_draws_by_pass(
        ledger,
        slotted.into_iter().map(|(name, above_labels)| (name, if above_labels { OverlayPass::AboveLabels } else { OverlayPass::Tiles })),
        [below, &mut over_buildings, above],
        counted,
        uncounted,
    );
}

/// Which pass a source's overlay draws go to in [`emit_source_draws_by_pass`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayPass {
    /// The tile pass: under the 3D buildings and the labels (the default).
    Tiles,
    /// Its own pass after the tile pass: over the buildings, under the labels.
    AboveBuildings,
    /// Its own pass after the symbol pass: over the labels.
    AboveLabels,
}

/// [`emit_source_draws`] with three passes: `lists` is the tile pass's
/// draw list, the above-buildings pass's and the above-labels pass's, in
/// [`OverlayPass`] order.
pub(crate) fn emit_source_draws_by_pass<'n, 'a>(
    ledger: &mut SourceDrawLedger,
    slotted: impl IntoIterator<Item = (&'n crate::overlay::SourceId, OverlayPass)>,
    lists: [&mut Vec<crate::render::TileDraw<'a>>; 3],
    mut counted: impl FnMut(&crate::overlay::SourceId, &mut Vec<crate::render::TileDraw<'a>>),
    mut uncounted: impl FnMut(&crate::overlay::SourceId, &mut Vec<crate::render::TileDraw<'a>>),
) {
    let [tiles, over_buildings, above] = lists;
    for (name, pass) in slotted {
        let draws = match pass {
            OverlayPass::Tiles => &mut *tiles,
            OverlayPass::AboveBuildings => &mut *over_buildings,
            OverlayPass::AboveLabels => &mut *above,
        };
        let first = draws.len();
        counted(name, draws);
        ledger.record(name, &draws[first..]);
        uncounted(name, draws);
    }
}

/// One frame's tiled-overlay draws, from [`OverlayGpu::prepare_tiles`]:
/// per source, the tiles to draw with the per-frame uniform any of them
/// needs (a stand-in or an overzoomed sub-box).
pub struct OverlayTileFrame {
    draws: Vec<(crate::overlay::SourceId, Vec<(Arc<OverlayTileGpu>, Option<OverlayTileFrameGroups>)>)>,
    /// Tile draws this frame, stand-ins included.
    pub draw_count: u64,
    /// Landed tiles are still waiting for upload: the shell should ask for
    /// another frame.
    pub upload_deferred: bool,
}

impl OverlayTileFrame {
    /// Appends source `name`'s tiles to `draws`: every tile's fill, then
    /// every tile's hairline -- MapLibre draws a fill layer's `fill` pass
    /// over all its tiles and then its `fillOutline` pass (`draw_fill.ts`),
    /// so a neighbour's fill never covers an outline. Two pipeline switches
    /// per source rather than two per chunk.
    pub fn push_tile_draws<'a>(&'a self, name: &str, draws: &mut Vec<crate::render::TileDraw<'a>>) {
        let Some((_, tiles)) = self.draws.iter().find(|(source, _)| &**source == name) else { return };
        for (tile, own) in tiles {
            match &tile.body {
                OverlayTileBody::Legacy { chunks, group } => {
                    let bind_group = own.as_ref().and_then(|own| own.legacy.as_ref()).unwrap_or(group);
                    for chunk in chunks {
                        draws.push(crate::render::TileDraw::Legacy(&chunk.buffers, bind_group));
                    }
                }
                OverlayTileBody::Compact { chunks, fill, .. } => {
                    let bind_group = own.as_ref().and_then(|own| own.fill.as_ref()).unwrap_or(fill);
                    for chunk in chunks {
                        draws.push(crate::render::TileDraw::CompactFill(chunk, bind_group));
                    }
                }
            }
        }
        for (tile, own) in tiles {
            if let OverlayTileBody::Compact { chunks, outline: Some(outline), .. } = &tile.body {
                let bind_group = own.as_ref().and_then(|own| own.outline.as_ref()).unwrap_or(outline);
                for chunk in chunks.iter().filter(|chunk| chunk.has_outline()) {
                    draws.push(crate::render::TileDraw::CompactOutline(chunk, bind_group));
                }
            }
        }
    }
}

impl OverlayGpu {
    /// Device bytes held by every tiled source's tile cache.
    pub fn tile_bytes(&self) -> u64 {
        self.tile_gpu.values().map(|held| held.held_bytes() as u64).sum()
    }

    /// Drops everything held on the GPU for `name`.
    pub fn forget(&mut self, name: &str) {
        self.tile_gpu.retain(|source, _| &**source != name);
        self.point_circles.retain(|source, _| &**source != name);
    }

    /// Appends source `name`'s point circle layers to `draws`, coarse
    /// layers first.
    pub fn push_point_draws<'a>(&'a self, name: &str, draws: &mut Vec<crate::render::TileDraw<'a>>) {
        for layer in self.point_circles.get(name).into_iter().flatten() {
            draws.push(crate::render::TileDraw::Circles(&layer.buffers, &layer.model_bind_group, &layer.circle_group));
        }
    }

    /// Uploads and re-uniforms every point source's circle layers for this
    /// frame. `layers_of(name)` is the source's layers, coarse to fine,
    /// each with its exclusion boxes.
    #[allow(clippy::type_complexity)]
    pub fn sync_points(
        &mut self,
        ctx: &crate::render::GpuContext,
        pipeline: &crate::render::TilePipeline,
        camera: &crate::render::Camera,
        globe: &crate::render::globe::GlobeUpload,
        overlay_order: &[crate::overlay::SourceId],
        layers_of: impl Fn(&str) -> Vec<(Arc<crate::overlay::points::PointCircles>, crate::render::circles::CircleParams)>,
    ) {
        // POINT sources' instanced circles: each layer (floor, stand-in,
        // view) uploaded once when it lands, its model uniform written off
        // its own origin every frame exactly as a whole-source mesh's is,
        // and its exclusion boxes (the finer layers' boxes) with it.
        self.point_circles.retain(|source, _| overlay_order.contains(source));
        for name in overlay_order {
            let layers = layers_of(name);
            if layers.is_empty() {
                self.point_circles.remove(name);
                continue;
            }
            let mut held = self.point_circles.remove(name).unwrap_or_default();
            let mut next = Vec::with_capacity(layers.len());
            let line_fx = self.line_fx_of(name);
            for (circles, params) in layers {
                let origin = (circles.origin.world_x, circles.origin.world_y);
                let matrix = crate::render::combined_overlay_matrix(camera, origin).to_cols_array_2d();
                let blend_t = circles.bracket.blend_t(camera.zoom);
                let dash_scale = crate::render::overlay_dash_scale(camera);
                let layer = match held.iter().position(|h| Arc::ptr_eq(&h.circles, &circles)) {
                    Some(at) => {
                        let layer = held.swap_remove(at);
                        pipeline.write_model_uniform_fx(
                            ctx,
                            &layer.model_buffer,
                            matrix,
                            blend_t,
                            dash_scale,
                            globe.for_overlay(origin),
                            crate::render::TileClip::WHOLE,
                            line_fx,
                        );
                        pipeline.write_circle_params(ctx, &layer.params_buffer, params);
                        layer
                    }
                    None => {
                        let buffers = pipeline.create_circle_buffers(ctx, &circles.set.instances);
                        let table = pipeline
                            .create_paint_table(ctx, &crate::render::compact::paint_table(&circles.set.paints));
                        let (model_buffer, model_bind_group) = pipeline.create_model_bind_group_fx(
                            ctx,
                            matrix,
                            blend_t,
                            dash_scale,
                            globe.for_overlay(origin),
                            crate::render::TileClip::WHOLE,
                            line_fx,
                        );
                        let (params_buffer, circle_group) =
                            pipeline.create_circle_bind_group(ctx, &table, params);
                        PointCircleGpu {
                            circles: Arc::clone(&circles),
                            buffers,
                            _table: table,
                            model_buffer,
                            model_bind_group,
                            params_buffer,
                            circle_group,
                        }
                    }
                };
                next.push(layer);
            }
            self.point_circles.insert(Arc::clone(name), next);
        }
    }

    /// Uploads what landed for every tiled source (under the per-frame
    /// byte cap), then picks each cover tile's draw -- the tile itself, a
    /// clipped ancestor stand-in, or a full set of descendants -- exactly
    /// as Pass 1 does for the basemap, choosing between a paint re-bake's
    /// epochs, noting live-diff completion, and writing every uniform.
    /// `release` takes each uploaded CPU tile (native frees it off the UI
    /// thread).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_tiles(
        &mut self,
        host: &mut OverlayHost,
        ctx: &crate::render::GpuContext,
        pipeline: &crate::render::TilePipeline,
        camera: &crate::render::Camera,
        globe: &crate::render::globe::GlobeUpload,
        overlay_order: &[crate::overlay::SourceId],
        cover: &[TileId],
        mut release: impl FnMut(crate::overlay::tile_plan::BakedOverlayTile),
    ) -> OverlayTileFrame {
        // TILED sources: upload what landed, then pick each cover tile's
        // draw -- the tile itself, a clipped ancestor stand-in, or a full
        // set of descendants -- exactly as Pass 1 does for the basemap.
        // Collected here, like `model_buffers`, because `draws` borrows it.
        self.tile_gpu
            .retain(|source, _| overlay_order.contains(source) && host.tile_sets.contains_key(source));
        let budget = self.tile_budget;
        let mut overlay_tile_draws: Vec<(
            crate::overlay::SourceId,
            Vec<(Arc<OverlayTileGpu>, Option<OverlayTileFrameGroups>)>,
        )> = Vec::new();
        let mut overlay_tile_draw_count = 0u64;
        // Shared by every tiled source: the cap is per frame.
        let mut overlay_upload_bytes = 0usize;
        let mut overlay_upload_deferred = false;
        for name in overlay_order {
            let Some(set) = host.tile_sets.get_mut(name) else { continue };
            let key = (set.generation, set.bracket, set.epoch);
            let line_fx = self.line_fx_of(name);
            let held = self
                .tile_gpu
                .entry(Arc::clone(name))
                .or_insert_with(|| OverlayTileGpuSet::new(key));
            if held.key != key {
                held.rekey(key, set.stand_in_empty.take());
            }
            for tile in set.evict.drain(..) {
                held.cache.remove(&tile);
            }
            let mut landed = std::mem::take(&mut set.landed).into_iter();
            for (tile, baked) in landed.by_ref() {
                if overlay_upload_bytes >= OVERLAY_UPLOAD_BYTES_PER_FRAME {
                    set.landed.push((tile, baked));
                    break;
                }
                #[cfg(feature = "probe")]
                let probe_started = crate::timing::Instant::now();
                // Placeholder matrix: rewritten below before every draw.
                let model_buffer = pipeline.create_model_buffer_clipped(
                    ctx,
                    glam::Mat4::IDENTITY.to_cols_array_2d(),
                    0.0,
                    0.0,
                    crate::render::GlobeParams::FLAT,
                    crate::render::TileClip::for_descendant(tile, tile),
                );
                let mut baked = baked;
                let landed_rev = set.landed_rev.remove(&tile).unwrap_or(0);
                // A compact tile is brought up to the CURRENT classes: live
                // class-only diffs since its cut repaint it here, not by a
                // re-bake (`FeatureSpans::retint`).
                let mut repainted = false;
                if let Ok(compact) = &mut baked {
                    if landed_rev < set.rows_rev {
                        compact.spans = None;
                    }
                    if let Some(spans) = compact.spans.as_mut() {
                        spans.retint(&mut compact.chunks, &set.classes.of_feature);
                        repainted = true;
                    }
                }
                if let Some(at) = set.repaint.remove(&tile) {
                    if !repainted && landed_rev < at {
                        let d = set.dirty.entry(tile).or_insert(at);
                        *d = (*d).max(at);
                    }
                }
                let body = match &mut baked {
                    Ok(compact) => {
                        let (fill_table, outline_table) = held.tables.get_or_insert_with(|| {
                            let table = |entries: Vec<(crate::style::Rgba, crate::style::Rgba)>| {
                                pipeline.create_paint_table(ctx, &crate::render::compact::paint_table(&entries))
                            };
                            (
                                table(crate::overlay::tile_plan::fill_paint_entries(&set.classes)),
                                table(crate::overlay::tile_plan::outline_paint_entries(&set.classes)),
                            )
                        });
                        OverlayTileBody::Compact {
                            chunks: compact
                                .chunks
                                .iter()
                                .map(|chunk| pipeline.create_compact_buffers_planar(ctx, chunk))
                                .collect(),
                            fill: pipeline.create_compact_model_bind_group(ctx, &model_buffer, fill_table),
                            outline: compact.has_outline.then(|| {
                                pipeline.create_compact_model_bind_group(ctx, &model_buffer, outline_table)
                            }),
                            spans: std::sync::Mutex::new(compact.spans.take()),
                        }
                    }
                    Err(legacy) => {
                        match install_overlay_chunks(pipeline, ctx, Vec::new(), &legacy.mesh, &legacy.extras) {
                            Ok(chunks) => OverlayTileBody::Legacy {
                                chunks,
                                group: pipeline.create_legacy_model_bind_group(ctx, &model_buffer),
                            },
                            Err(error) => {
                                note_overlay_too_large(name, &error);
                                continue;
                            }
                        }
                    }
                };
                // Flagged on wasm32 only, where wgpu's handles are `!Send`;
                // natively this `Arc` is `Send + Sync`.
                #[allow(clippy::arc_with_non_send_sync)]
                let uploaded = Arc::new(OverlayTileGpu { body, model_buffer });
                let bytes = uploaded.device_bytes();
                overlay_upload_bytes += bytes;
                held.cache.put_with_bytes_keeping(tile, uploaded, bytes, |t| set.keep.contains(t));
                // Repainted and not dirty: its picture is the current data.
                let shown = if repainted && !set.dirty.contains_key(&tile) { set.data_rev } else { landed_rev };
                set.shown_rev.insert(tile, shown);
                if repainted {
                    set.class_rev.insert(tile, set.data_rev);
                } else {
                    set.class_rev.remove(&tile);
                }
                #[cfg(feature = "probe")]
                crate::render::zoom_probe::overlay_uploaded(
                    tile,
                    bytes as u64,
                    u64::try_from(probe_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
                );
                // A1: the CPU mesh goes as soon as the GPU has it -- off
                // the UI thread, like a displaced whole-source mesh.
                release(baked);
            }
            set.landed.extend(landed);
            overlay_upload_deferred |= !set.landed.is_empty();
            // `shown_rev`/`class_rev` gain an entry per uploaded tile and the
            // GPU cache evicts; until this, only a live diff pruned them, so
            // a long pan over a static tiled source kept one entry for every
            // tile it had ever shown. Amortised: pruned once they are twice
            // what could still need them.
            if tile_revisions_need_pruning(
                set.shown_rev.len().max(set.class_rev.len()),
                held.cache.len() + set.landed.len() + set.empty.len(),
            ) {
                let landed_ids: HashSet<TileId> = set.landed.iter().map(|(t, _)| *t).collect();
                let cache = &held.cache;
                prune_tile_revisions(
                    &mut set.shown_rev,
                    &mut set.class_rev,
                    |t| cache.contains(t) || landed_ids.contains(t),
                    &set.empty,
                );
            }
            let wanted_cover = crate::overlay::tile_plan::wanted_overlay_tiles(cover, set.index.max_zoom());
            // A stand-in from an older generation judges emptiness by its
            // own generation's empties, not the new one's.
            let pick_cover = |cache: &crate::tile_source::TileCache<Arc<OverlayTileGpu>>,
                              own_empty: Option<&HashSet<TileId>>| {
                let empty = own_empty.unwrap_or(&set.empty);
                wanted_cover
                    .iter()
                    .map(|&(display, tile)| {
                        let (picks, draw) =
                            overlay_tile_picks(tile, display, |t| cache.contains(&t), |t| empty.contains(&t));
                        (display, tile, picks, draw)
                    })
                    .collect::<Vec<(TileId, TileId, Vec<(TileId, crate::render::TileClip)>, OverlayCoverDraw)>>()
            };
            let holes = |picks: &[(TileId, TileId, Vec<(TileId, crate::render::TileClip)>, OverlayCoverDraw)]| {
                picks.iter().filter(|(_, _, _, draw)| matches!(draw, OverlayCoverDraw::Hole)).count()
            };
            // A paint re-bake or a re-push in progress
            // (`OverlayTileGpuSet::previous`): each cover tile the current
            // set has a hole in is drawn from the newest older picture that
            // has it -- tile by tile, never a blank tile an older picture
            // could fill, and every landed tile of the new set shows at
            // once. Stand-ins nothing drew from, and the oldest past one
            // cache budget, are released here.
            let answers = |picks: &[(TileId, TileId, Vec<(TileId, crate::render::TileClip)>, OverlayCoverDraw)]| {
                picks
                    .iter()
                    .map(|(_, tile, p, draw)| {
                        let covered =
                            if matches!(draw, OverlayCoverDraw::Hole) { quadrant_coverage(*tile, p) } else { [1.0; 4] };
                        (*draw, covered)
                    })
                    .collect::<Vec<_>>()
            };
            let mut per_set = vec![pick_cover(&held.cache, None)];
            per_set.extend(held.previous.iter().map(|s| pick_cover(&s.cache, s.empty.as_ref())));
            let compose = |per_set: &[Vec<(TileId, TileId, Vec<(TileId, crate::render::TileClip)>, OverlayCoverDraw)>]| {
                compose_cover_quadrants(&per_set.iter().map(|p| answers(p)).collect::<Vec<_>>())
            };
            // A pick of set `c` for cover entry `e`, kept under `mask`.
            let in_mask = |tile: TileId, mask: u8, pick: TileId| {
                mask == ALL_QUADRANTS || quadrant_of(tile, pick).is_some_and(|q| mask & (1 << q) != 0)
            };
            let used = |chosen: &[Vec<(usize, u8)>]| chosen.iter().flatten().map(|&(c, _)| c).collect::<Vec<usize>>();
            let mut chosen = compose(&per_set);
            if !held.previous.is_empty() {
                let keep = stand_ins_to_keep(&used(&chosen), held.previous.len());
                if keep.iter().any(|k| !k) {
                    for i in (0..keep.len()).rev() {
                        if !keep[i] {
                            held.previous.remove(i);
                            per_set.remove(i + 1);
                        }
                    }
                    chosen = compose(&per_set);
                }
                // Over budget: every stand-in keeps only the tiles it drew
                // this frame -- its off-screen history goes, never a tile
                // that is the only picture on a visible cover tile. The
                // choices stand: every tile a chosen pick names survives.
                if held.held_bytes() > budget {
                    for (i, stand_in) in held.previous.iter_mut().enumerate() {
                        let drawn: HashSet<TileId> = chosen
                            .iter()
                            .enumerate()
                            .flat_map(|(e, sets)| sets.iter().filter(|&&(c, _)| c == i + 1).map(move |&(_, mask)| (e, mask)))
                            .flat_map(|(e, mask)| {
                                let tile = per_set[i + 1][e].1;
                                per_set[i + 1][e].2.iter().map(|(t, _)| *t).filter(move |&t| in_mask(tile, mask, t))
                            })
                            .collect();
                        let stale: Vec<TileId> =
                            stand_in.cache.iter().map(|(t, _)| *t).filter(|t| !drawn.contains(t)).collect();
                        for tile in stale {
                            stand_in.cache.remove(&tile);
                        }
                    }
                }
            }
            let any_stand_in = used(&chosen).iter().any(|&c| c != 0);
            let current_holes = holes(&per_set[0]);
            let mut chosen_picks = per_set.into_iter().map(|p| p.into_iter().map(Some).collect::<Vec<_>>()).collect::<Vec<_>>();
            let cover_picks: Vec<(usize, (TileId, TileId, Vec<(TileId, crate::render::TileClip)>, OverlayCoverDraw))> = chosen
                .iter()
                .enumerate()
                .flat_map(|(e, sets)| sets.iter().map(move |&(c, mask)| (e, c, mask)))
                .map(|(e, c, mask)| {
                    let (display, tile, mut picks, draw) = chosen_picks[c][e].take().expect("each set draws an entry once");
                    picks.retain(|(p, _)| in_mask(tile, mask, *p));
                    (c, (display, tile, picks, draw))
                })
                .collect();
            #[cfg(feature = "probe")]
            crate::render::zoom_probe::overlay_cover(
                &cover_picks
                    .iter()
                    .map(|(_, (display, tile, _, draw))| {
                        use crate::render::zoom_probe::Drawn;
                        let d = match *draw {
                            OverlayCoverDraw::Own => Drawn::Own,
                            OverlayCoverDraw::Ancestor(l) => Drawn::Ancestor(l),
                            OverlayCoverDraw::Descendants => Drawn::Descendants,
                            OverlayCoverDraw::Empty => Drawn::Empty,
                            OverlayCoverDraw::Hole => Drawn::Hole,
                        };
                        (*display, *tile, d)
                    })
                    .collect::<Vec<_>>(),
                &|t| held.cache.contains(&t) || held.previous.iter().any(|p| p.cache.contains(&t)),
                held.held_bytes() as u64,
                OVERLAY_TILE_GPU_BUDGET_BYTES as u64,
                u32::try_from(held.cache.len()).unwrap_or(u32::MAX),
            );
            set.drawn_last.clear();
            set.drawn_last
                .extend(cover_picks.iter().flat_map(|(_, (_, _, picks, _))| picks.iter().map(|(drawn, _)| *drawn)));
            // A live diff is complete on the first whole frame where every
            // tile it dirtied that is on screen shows a picture cut at its
            // revision or later.
            if !any_stand_in && current_holes == 0 {
                if let Some(state) = host.live.get_mut(name).filter(|s| !s.awaiting.is_empty()) {
                    let drawn: HashSet<TileId> = set.drawn_last.iter().copied().collect();
                    let shown = &set.shown_rev;
                    let classes = &set.class_rev;
                    state.note_frame_complete(|job| {
                        job.tiles.iter().all(|t| !drawn.contains(t) || shown.get(t).is_some_and(|&r| r >= job.rev))
                            && job.recolored.iter().all(|t| {
                                !drawn.contains(t)
                                    || shown.get(t).is_some_and(|&r| r >= job.rev)
                                    || classes.get(t).is_some_and(|&r| r >= job.rev)
                            })
                    });
                }
            }
            let mut own_written: HashSet<(usize, TileId)> = HashSet::new();
            let mut list = Vec::new();
            // "Push -> first painted frame with a tile draw of THIS
            // generation": a stand-in's draw is an older one's.
            let mut drew_current = false;
            for (c, (_, _, picks, _)) in cover_picks {
                let held: &mut OverlayTileGpuSet = if c == 0 { &mut *held } else { &mut held.previous[c - 1] };
                // The drawn set's own bracket: a stand-in from an older
                // generation may have been baked at another one.
                let blend_t = held.key.1.blend_t(camera.zoom);
                for (drawn, clip) in picks {
                    let Some(cached) = held.cache.get(&drawn).cloned() else { continue };
                    let matrix = crate::render::combined_tile_matrix(camera, drawn, crate::mvt::MVT_EXTENT as f32).to_cols_array_2d();
                    let dash_scale = crate::render::dash_scale(camera, drawn, crate::mvt::MVT_EXTENT as f32);
                    let params = globe.for_tile(drawn);
                    // The tile's own uniform carries its own box. Any other
                    // clip over the same buffers (an overzoomed sub-box, a
                    // stand-in) gets a uniform of its own for this frame.
                    if clip == crate::render::TileClip::for_descendant(drawn, drawn) && own_written.insert((c, drawn)) {
                        pipeline.write_model_uniform_fx(
                            ctx,
                            &cached.model_buffer,
                            matrix,
                            blend_t,
                            dash_scale,
                            params,
                            clip,
                            line_fx,
                        );
                        list.push((cached, None));
                    } else {
                        let buffer = pipeline.create_model_buffer_fx(
                            ctx, matrix, blend_t, dash_scale, params, clip, line_fx,
                        );
                        let own = match &cached.body {
                            OverlayTileBody::Legacy { .. } => OverlayTileFrameGroups {
                                legacy: Some(pipeline.create_legacy_model_bind_group(ctx, &buffer)),
                                fill: None,
                                outline: None,
                                _buffer: buffer,
                            },
                            OverlayTileBody::Compact { outline, .. } => {
                                let (fill_table, outline_table) =
                                    held.tables.as_ref().expect("a compact tile was uploaded with its source's tables");
                                OverlayTileFrameGroups {
                                    legacy: None,
                                    fill: Some(pipeline.create_compact_model_bind_group(ctx, &buffer, fill_table)),
                                    outline: outline.is_some().then(|| {
                                        pipeline.create_compact_model_bind_group(ctx, &buffer, outline_table)
                                    }),
                                    _buffer: buffer,
                                }
                            }
                        };
                        list.push((cached, Some(own)));
                    }
                    overlay_tile_draw_count += 1;
                    drew_current |= c == 0;
                }
            }
            if drew_current && !set.first_frame_seen {
                set.first_frame_seen = true;
                crate::render::alloc::note_overlay_tile_first_frame(
                    u64::try_from(set.pushed_at.elapsed().as_nanos()).unwrap_or(u64::MAX),
                );
            }
            overlay_tile_draws.push((Arc::clone(name), list));
        }
        OverlayTileFrame { draws: overlay_tile_draws, draw_count: overlay_tile_draw_count, upload_deferred: overlay_upload_deferred }
    }
}

/// The viewport's Mercator box, grown by `grow` of its size on every
/// side. Unprojects the corners; a corner above the horizon falls back
/// to four viewports around the centre. Shared by `MapView`'s point path
/// and [`ShellOverlays`].
pub fn point_viewport_box(camera: &crate::render::Camera, grow: f64) -> ([f64; 2], [f64; 2]) {
    let (w, h) = (camera.viewport_width_px, camera.viewport_height_px);
    let world = f64::from(crate::render::WORLD_SIZE);
    let corners = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h), (w / 2.0, 0.0)];
    let mut min = [f64::INFINITY; 2];
    let mut max = [f64::NEG_INFINITY; 2];
    let mut fell_back = false;
    for (x, y) in corners {
        match crate::render::unproject_to_ground_f64(camera, x, y) {
            Some(p) => {
                min = [min[0].min(p.x / world), min[1].min(p.y / world)];
                max = [max[0].max(p.x / world), max[1].max(p.y / world)];
            }
            None => fell_back = true,
        }
    }
    let (fmin, fmax) = crate::overlay::points::flat_viewport_box(
        camera.center_lon,
        camera.center_lat,
        camera.zoom,
        f64::from(w) * 4.0,
        f64::from(h) * 4.0,
    );
    if fell_back || !min[0].is_finite() {
        min = [min[0].min(fmin[0]), min[1].min(fmin[1])];
        max = [max[0].max(fmax[0]), max[1].max(fmax[1])];
    }
    let (gw, gh) = ((max[0] - min[0]) * grow, (max[1] - min[1]) * grow);
    ([min[0] - gw, min[1] - gh], [max[0] + gw, max[1] + gh])
}


/// The hover tolerance, in screen pixels: `MapView::HOVER_TOLERANCE_PX`.
pub const SHELL_HOVER_TOLERANCE_PX: f64 = 6.0;

/// A clustered view reaches this far past the viewport on each side, as a
/// fraction of its size (`map_view/points.rs`'s `CLUSTER_VIEW_GROW`).
const SHELL_VIEW_GROW: f64 = 0.5;
/// A view is rebuilt once the viewport grown by this much leaves it
/// (`map_view/points.rs`'s `REBUILD_SLACK`).
const SHELL_REBUILD_SLACK: f64 = 0.25;

/// A tiled source owed an index build: its name, the stamp it was pushed
/// at, and when.
struct PendingBuild {
    source: crate::overlay::SourceId,
    generation: u64,
    pushed_at: Instant,
}

/// The whole overlay loop for a shell with no executor -- the browser and,
/// in this pass, the phone: the store, the [`OverlayHost`], the
/// [`OverlayGpu`], and a synchronous point driver, run INLINE under a
/// per-frame time budget ([`Self::update`]). See the spec's "Web threading"
/// for why inline, and for the worker step `TileBake` is shaped for.
///
/// Draws tiled sources (polygons and lines, fill + hairline outline, on the
/// compact 8-byte vertex) and point sources (clustered or not, instanced
/// circles). A polygon/line GeoJSON source is always put on the tiled path
/// (`OverlayStore::force_tiled`): these shells have no whole-source mesh.
pub struct ShellOverlays {
    store: crate::overlay::OverlayStore,
    host: OverlayHost,
    gpu: OverlayGpu,
    builds: Vec<PendingBuild>,
    /// Each point source's current view's circles, with the data revision
    /// it was built from.
    points: HashMap<crate::overlay::SourceId, (u64, Option<Arc<crate::overlay::points::PointCircles>>)>,
    frame: Option<OverlayTileFrame>,
    order: Vec<crate::overlay::SourceId>,
    /// What each source drew in the last [`Self::push_draws`]. A
    /// `RefCell` because `push_draws` takes `&self` -- the draws it emits
    /// borrow from `self` -- and records as it emits.
    drawn: std::cell::RefCell<SourceDrawLedger>,
    /// Table-only paint updates made since the last [`Self::prepare`],
    /// the latest per source: written there, before the tiles are picked.
    pending_tables: Vec<TableWrite>,
    /// The last [`Self::update`]'s display zoom: the fallback bracket a
    /// paint update's replan plans at (`MapView::display_zoom`'s role).
    display_zoom: u8,
    /// When each source's last table-only paint update was made: its full
    /// check waits until the paint has held for `paint_check_settle`.
    painted_at: HashMap<crate::overlay::SourceId, crate::timing::Instant>,
    /// See [`PAINT_CHECK_SETTLE`].
    paint_check_settle: Duration,
    /// Full paint checks run (each a `classify` over one source).
    paint_checks_run: u64,
    /// Sources kept UNDER the 3D buildings
    /// (`set_source_above_buildings(name, false)`); every other source
    /// draws over them ([`Self::push_draws_split`]).
    under_buildings: std::collections::HashSet<crate::overlay::SourceId>,
    /// Per source, the generation last checked and whether it carries a
    /// label or icon ([`Self::symbol_sources`]). A `RefCell`: read from
    /// the label pass, which holds `&self`.
    symbol_flags: std::cell::RefCell<HashMap<crate::overlay::SourceId, (u64, bool)>>,
}

/// How long a table-only paint must hold before its full check runs
/// (`ShellOverlays::check_source_paint`). The check is a `classify` over
/// every feature of the source; inline, a slider drag that repaints every
/// frame would pay it every frame for a paint the next frame replaces. So
/// it waits for the drag to pause. A paint that splits a class shows the
/// representatives' colours until then, and re-bakes once, for the range
/// the user stopped on.
pub const PAINT_CHECK_SETTLE: Duration = Duration::from_millis(150);

impl Default for ShellOverlays {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellOverlays {
    pub fn new() -> Self {
        Self {
            store: crate::overlay::OverlayStore::new(),
            // One bake at a time: inline, the "worker" is this thread.
            host: OverlayHost::new(1),
            gpu: OverlayGpu::default(),
            builds: Vec::new(),
            points: HashMap::new(),
            frame: None,
            order: Vec::new(),
            drawn: std::cell::RefCell::default(),
            pending_tables: Vec::new(),
            display_zoom: 0,
            painted_at: HashMap::new(),
            paint_check_settle: PAINT_CHECK_SETTLE,
            paint_checks_run: 0,
            under_buildings: std::collections::HashSet::new(),
            symbol_flags: std::cell::RefCell::default(),
        }
    }

    /// Draws source `name` over the 3D buildings (`true`, the default;
    /// still under the labels) or under them (`false`): over, a translucent
    /// building roof never washes the data out. The shell draws the sources
    /// over the buildings through [`Self::push_draws_split`]'s second list.
    /// `MapView::set_source_above_buildings`'s rule. Cleared by
    /// [`Self::remove_source`].
    pub fn set_source_above_buildings(&mut self, name: &str, above: bool) {
        if above {
            self.under_buildings.remove(name);
        } else {
            self.under_buildings.insert(Arc::from(name));
        }
    }

    /// How long a table-only paint holds before its full check runs
    /// ([`PAINT_CHECK_SETTLE`] by default; zero checks on the next
    /// [`Self::update`]).
    pub fn set_paint_check_settle(&mut self, settle: Duration) {
        self.paint_check_settle = settle;
    }

    /// Full paint checks run so far.
    pub fn paint_checks_run(&self) -> u64 {
        self.paint_checks_run
    }

    /// Replaces source `name` with a GeoJSON FeatureCollection. Its
    /// polygons and lines draw through the tile index once the next
    /// [`Self::update`] has built it.
    pub fn set_source(
        &mut self,
        name: &str,
        geojson: &str,
        display_zoom: u8,
    ) -> Result<(), crate::overlay::FeatureError> {
        self.store.set_source(name, geojson, display_zoom)?;
        self.store.force_tiled(name, display_zoom);
        self.after_write(name);
        Ok(())
    }

    /// Replaces source `name` with `features` and typed property `columns`
    /// (`OverlayStore::set_source_columns`): no GeoJSON text, no per-feature
    /// map. Forced onto the tiled path, as `set_source` is.
    pub fn set_source_columns(
        &mut self,
        name: &str,
        features: &[crate::overlay::input::FeatureInput<'_>],
        columns: &[(&str, crate::overlay::columns::PropertyColumn<'_>)],
        display_zoom: u8,
    ) -> Result<(), crate::overlay::FeatureError> {
        self.store.set_source_columns(name, features, columns, display_zoom)?;
        self.store.force_tiled(name, display_zoom);
        self.after_write(name);
        Ok(())
    }

    /// Replaces source `name` with a parsed CSV
    /// (`OverlayStore::set_source_csv`): all points take the point path
    /// (clustered when `cluster` says so), anything else the tiled feature
    /// path, as `set_source_columns` is.
    pub fn set_source_csv(
        &mut self,
        name: &str,
        csv: &crate::overlay::csv::CsvSource,
        paint: &crate::overlay::FeaturePaint,
        cluster: Option<crate::overlay::ClusterOptions>,
        display_zoom: u8,
    ) -> Result<(), crate::overlay::FeatureError> {
        if csv.points().is_some() {
            if let Some(cluster) = cluster {
                self.store.set_source_cluster_options(name, Some(cluster), display_zoom)?;
            }
            self.store.set_source_csv(name, csv, paint, display_zoom)?;
        } else {
            self.store.set_source_csv(name, csv, paint, display_zoom)?;
            self.store.force_tiled(name, display_zoom);
        }
        self.after_write(name);
        Ok(())
    }

    /// Replaces source `name` with a read FlatGeobuf
    /// (`OverlayStore::set_source_fgb`): all points take the point path
    /// (clustered when `cluster` says so), anything else the tiled feature
    /// path, as [`Self::set_source_csv`].
    pub fn set_source_fgb(
        &mut self,
        name: &str,
        fgb: &crate::overlay::fgb::FgbSource,
        paint: &crate::overlay::FeaturePaint,
        cluster: Option<crate::overlay::ClusterOptions>,
        display_zoom: u8,
    ) -> Result<(), crate::overlay::FeatureError> {
        if fgb.points().is_some() {
            if let Some(cluster) = cluster {
                self.store.set_source_cluster_options(name, Some(cluster), display_zoom)?;
            }
            self.store.set_source_fgb(name, fgb, paint, display_zoom)?;
        } else {
            self.store.set_source_fgb(name, fgb, paint, display_zoom)?;
            self.store.force_tiled(name, display_zoom);
        }
        self.after_write(name);
        Ok(())
    }

    /// Replaces source `name` with a read GeoParquet file
    /// (`OverlayStore::set_source_geoparquet`), as [`Self::set_source_fgb`].
    #[cfg(feature = "geoparquet")]
    pub fn set_source_geoparquet(
        &mut self,
        name: &str,
        source: &crate::overlay::geoparquet::GeoParquetSource,
        paint: &crate::overlay::FeaturePaint,
        cluster: Option<crate::overlay::ClusterOptions>,
        display_zoom: u8,
    ) -> Result<(), crate::overlay::FeatureError> {
        if source.points().is_some() {
            if let Some(cluster) = cluster {
                self.store.set_source_cluster_options(name, Some(cluster), display_zoom)?;
            }
            self.store.set_source_geoparquet(name, source, paint, display_zoom)?;
        } else {
            self.store.set_source_geoparquet(name, source, paint, display_zoom)?;
            self.store.force_tiled(name, display_zoom);
        }
        self.after_write(name);
        Ok(())
    }

    /// Declares `name`'s colours as style-spec expressions over its
    /// features' properties (`fill-color`, `line-color`, `circle-color`,
    /// `circle-radius`). Governs the next push.
    ///
    /// `line-gradient` is refused: it needs line metrics, which this shell
    /// does not compute yet (every source here draws tiled; story-content
    /// 1a spec amendment -- web/iOS line progress is piece 3).
    pub fn declare_source_paint(
        &mut self,
        name: &str,
        paint: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), crate::overlay::FeatureError> {
        if paint.contains_key("line-gradient") {
            return Err(crate::overlay::FeatureError::SourcePaint {
                source: name.to_string(),
                reason: SHELL_NO_LINE_GRADIENT.into(),
            });
        }
        self.store.declare_source_paint(name, paint)
    }

    /// Declares symbols placed along `name`'s lines (`symbol-placement:
    /// "line"`), or removes the declaration with `None` / an empty object:
    /// `MapView::declare_source_symbols`, through the same store
    /// (`OverlayStore::declare_source_symbols` has the accepted keys and
    /// the refusals). STICKY; governs the next push.
    ///
    /// Compiled and kept here so a declaration means the same on every
    /// shell, but this shell does not DRAW line symbols yet: the anchor
    /// bake and the symbol pass that place them are `MapView`'s.
    pub fn declare_source_symbols(
        &mut self,
        name: &str,
        layout: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(), crate::overlay::FeatureError> {
        self.store.declare_source_symbols(name, layout)
    }

    /// Replaces `name`'s declared paint AND brings what is drawn up to it,
    /// without a re-push: `MapView::update_source_paint`, through the same
    /// host code. [`SourcePaintPath::TableOnly`] rewrites the source's two
    /// paint tables at the next [`Self::prepare`] -- no re-cut, no re-bake,
    /// no upload -- and the next [`Self::update`]s check the new paint
    /// against every feature (one source per frame), re-baking should it
    /// split a class. [`SourcePaintPath::Rebake`] re-bakes the cover on the
    /// same index through `update`; [`SourcePaintPath::Replan`] (a source
    /// whose tile set is not built yet, or a point source) rebuilds it as a
    /// re-push would.
    ///
    /// One difference from `MapView`, visible only while a split re-bakes:
    /// `MapView` writes the representatives' tables at once, so the old
    /// picture standing in shows roughly the new paint; here the check can
    /// run (in `update`) before `prepare` writes them, the re-bake moves the
    /// epoch, and the write -- keyed to the old one -- is skipped. The old
    /// picture keeps the previous paint's colours until the new epoch
    /// covers the view. Both settle to the same picture.
    ///
    /// Compiled before anything is written: a malformed expression is
    /// refused and changes nothing.
    pub fn update_source_paint(
        &mut self,
        name: &str,
        paint: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<SourcePaintPath, crate::overlay::FeatureError> {
        // A point source resolves its paint when a view is built: declare,
        // drop the view, and let the next `update` rebuild it.
        if self.store.source_point_data(name).is_some() {
            self.store.declare_source_paint(name, paint)?;
            self.store.invalidate_point_view(name);
            self.points.remove(name);
            return Ok(SourcePaintPath::Replan);
        }
        match self.host.apply_source_paint(&mut self.store, name, paint)? {
            PaintDecision::Replan => {
                self.replan_source_paint(name);
                Ok(SourcePaintPath::Replan)
            }
            PaintDecision::Rebake => Ok(SourcePaintPath::Rebake),
            PaintDecision::Table(write) => {
                self.painted_at.insert(Arc::clone(&write.source), crate::timing::Instant::now());
                // Only the latest tables for a source are worth writing.
                self.pending_tables.retain(|w| w.source != write.source);
                self.pending_tables.push(write);
                Ok(SourcePaintPath::TableOnly)
            }
        }
    }

    /// How many class-changing re-bakes source `name`'s tiles have been
    /// through (`None`: no tiled source by that name) --
    /// `MapView::source_paint_epoch`.
    pub fn source_paint_epoch(&self, name: &str) -> Option<u64> {
        self.host.tile_sets.get(name).map(|set| set.epoch)
    }

    /// The re-push-equivalent path (`MapView::replan_source_paint`): the
    /// store replans `name` under its already-declared paint and moves its
    /// stamp, and the tile set is rebuilt as after a push.
    fn replan_source_paint(&mut self, name: &str) {
        if let Ok(update) = self.store.plan_source_paint_update(name, &[]) {
            self.store.apply_paint_and_replan(&[], &update, self.display_zoom);
        }
        self.after_write(name);
    }

    /// Replaces point source `name` with `lon_lat`, clustered when
    /// `cluster` is `Some` with `cluster: true`.
    pub fn set_points(
        &mut self,
        name: &str,
        lon_lat: &[[f64; 2]],
        paint: &crate::overlay::FeaturePaint,
        cluster: crate::overlay::ClusterOptions,
    ) -> Result<(), crate::overlay::FeatureError> {
        self.store.set_source_cluster_options(name, Some(cluster), 0)?;
        self.store.set_source_points(name, lon_lat, None, paint, &[])?;
        self.after_write(name);
        Ok(())
    }

    /// Replaces point source `name` with `lon_lat`, each with its stable
    /// `ids` entry (what [`Self::feature_at`] hands back) and the typed
    /// property `columns` a declared paint reads -- the lean door
    /// `OverlayStore::set_source_points` is, for a host that keeps its own
    /// id map (an embedding phone app, re-pushing a layer by id). Never
    /// clustered: a host that updates points by id wants each one drawn.
    pub fn set_points_with_ids(
        &mut self,
        name: &str,
        lon_lat: &[[f64; 2]],
        ids: &[Option<crate::overlay::FeatureId>],
        paint: &crate::overlay::FeaturePaint,
        columns: &[(&str, crate::overlay::columns::PropertyColumn<'_>)],
    ) -> Result<(), crate::overlay::FeatureError> {
        self.store.set_source_cluster_options(name, Some(crate::overlay::ClusterOptions::points_only()), 0)?;
        self.store.set_source_points(name, lon_lat, Some(ids), paint, columns)?;
        self.after_write(name);
        Ok(())
    }

    /// Replaces source `name` with `features`, each with its OWN paint and
    /// properties (`OverlayStore::set_source_data_from`): the door a
    /// shell's vector-tile overlay pushes each style layer through, its
    /// features' paints resolved per case as `MapView`'s
    /// `push_vector_overlay` does. `points` puts the source on the point
    /// path (instanced circles, never clustered); anything else takes the
    /// tiled path, as [`Self::set_source`] does.
    pub fn set_source_data(
        &mut self,
        name: &str,
        features: &[crate::overlay::DataFeatureInput<'_>],
        points: bool,
        display_zoom: u8,
    ) -> Result<(), crate::overlay::FeatureError> {
        if points {
            self.store.set_source_cluster_options(name, Some(crate::overlay::ClusterOptions::points_only()), display_zoom)?;
        }
        self.store.set_source_data_from(name, features, display_zoom)?;
        if !points {
            self.store.force_tiled(name, display_zoom);
        }
        self.after_write(name);
        Ok(())
    }

    /// Whether source `name`'s paint is an expression over its features'
    /// properties (`None`: no such source).
    pub fn source_is_data_driven(&self, name: &str) -> Option<bool> {
        self.store.source_is_data_driven(name)
    }

    /// Removes source `name` and everything drawn from it.
    pub fn remove_source(&mut self, name: &str) -> bool {
        let removed = self.store.remove_source(name);
        self.painted_at.remove(name);
        self.host.forget(name);
        self.gpu.forget(name);
        self.points.remove(name);
        self.builds.retain(|b| &*b.source != name);
        self.drawn.get_mut().forget(name);
        self.under_buildings.remove(name);
        removed
    }

    /// Puts the named sources in `names`' order, BOTTOM FIRST, among the
    /// slots they hold now (`OverlayStore::set_source_order`): a host that
    /// removes and re-creates a source -- emptied, hidden, re-declared --
    /// puts it back where its own layer list says, instead of on top. The
    /// next [`Self::prepare`] draws in the new order; nothing re-bakes.
    /// `true` if any source moved.
    pub fn set_source_order(&mut self, names: &[&str]) -> bool {
        self.store.set_source_order(names)
    }

    /// The source names, in draw order (a later push draws on top).
    pub fn source_names(&self) -> Vec<crate::overlay::SourceId> {
        self.store.source_names().cloned().collect()
    }

    fn after_write(&mut self, name: &str) {
        self.points.remove(name);
        self.builds.retain(|b| &*b.source != name);
        let Some(snapshot) = self.store.source_snapshot(name) else {
            self.host.forget(name);
            return;
        };
        // A tiled re-push keeps its old tile set drawing until the new one
        // is built and covers the view (`OverlayHost::install_tile_set`),
        // as `MapView` does; anything else has no tiles to keep.
        if !snapshot.tiled {
            self.host.forget(name);
        }
        if snapshot.tiled {
            self.builds.push(PendingBuild {
                source: Arc::clone(&snapshot.source),
                generation: snapshot.generation,
                pushed_at: crate::timing::Instant::now(),
            });
        }
    }

    /// The CPU half of a frame: builds owed tile indexes, (re)builds point
    /// views the camera left, retargets the bake queue to `cover` and runs
    /// bakes until `budget` is spent (at least one, so a frame always makes
    /// progress). `true` when anything changed or work is still owed: the
    /// shell should draw, and come back next frame.
    pub fn update(&mut self, camera: &crate::render::Camera, cover: &[TileId], budget: Duration) -> bool {
        let started = crate::timing::Instant::now();
        let display_zoom = camera.zoom.floor().clamp(0.0, 30.0) as u8;
        self.display_zoom = display_zoom;
        let mut changed = false;
        for build in std::mem::take(&mut self.builds) {
            let Some(snapshot) = self.store.source_snapshot(&build.source) else { continue };
            if snapshot.generation != build.generation {
                continue;
            }
            let Some(live) = self.store.source_live_generation(&build.source) else { continue };
            let bracket = crate::overlay::plan::zoom_dependence(&snapshot.features)
                .unwrap_or(crate::overlay::plan::BakeBracket::Bucket(display_zoom));
            let options = self.store.source_options(&build.source).unwrap_or_default().tile_index_options();
            let built = OverlayHost::build_tile_set(
                &snapshot.features,
                &snapshot.properties,
                &snapshot.paint,
                snapshot.tile_index.as_deref(),
                options,
                bracket,
                &|| false,
            );
            drop(snapshot);
            if let Ok(Some(built)) = built {
                self.store.adopt_tile_index(&build.source, Arc::clone(&built.index));
                self.host.install_tile_set(&build.source, build.generation, bracket, built, live, build.pushed_at);
                changed = true;
            }
        }
        changed |= self.drive_points(camera);
        self.host.request_tiles(Some(&self.gpu), cover);
        let mut ran = 0usize;
        while ran == 0 || started.elapsed() < budget {
            let Some(bake) = self.host.next_bake() else { break };
            changed |= self.host.land_bake(bake.run());
            ran += 1;
        }
        if started.elapsed() < budget {
            changed |= self.check_source_paint();
        }
        changed
            || !self.host.bake_queue.pending.is_empty()
            || self.host.tile_sets.values().any(|set| set.verify.unverified.is_some())
            || self.host.tile_sets.values().any(|set| !set.landed.is_empty())
            || self.frame.as_ref().is_some_and(|frame| frame.upload_deferred)
    }

    /// The full check behind a table-only update (`MapView`'s
    /// `verify_source_paint`), inline: `classify` over every feature of the
    /// first source owed one whose paint has held for
    /// [`PAINT_CHECK_SETTLE`], landed through
    /// [`OverlayHost::finish_source_paint_check`]. `true` when it changed
    /// what draws (a re-bake or a replan).
    fn check_source_paint(&mut self) -> bool {
        let (painted_at, settle) = (&self.painted_at, self.paint_check_settle);
        let Some((source, rev, bracket, epoch, generation)) = self.host.tile_sets.iter().find_map(|(source, set)| {
            let rev = set.verify.unverified?;
            if painted_at.get(source).is_some_and(|at| at.elapsed() < settle) {
                return None;
            }
            Some((Arc::clone(source), rev, set.bracket, set.epoch, set.generation))
        }) else {
            return false;
        };
        self.paint_checks_run += 1;
        let snapshot = self.store.source_snapshot(&source);
        let Some(snapshot) = snapshot.filter(|snapshot| snapshot.generation == generation) else {
            // No such source any more, or a re-push since (this set's
            // classes index an older feature list): nothing to check.
            if let Some(set) = self.host.tile_sets.get_mut(&source) {
                set.verify.unverified = None;
            }
            return false;
        };
        let classes = crate::overlay::plan::classify(&snapshot.features, &snapshot.properties, bracket, &snapshot.paint);
        drop(snapshot);
        match self.host.finish_source_paint_check(&source, epoch, rev, classes) {
            Some(SourcePaintPath::Replan) => {
                self.replan_source_paint(&source);
                true
            }
            Some(SourcePaintPath::Rebake) => true,
            Some(SourcePaintPath::TableOnly) | None => false,
        }
    }

    /// Builds each point source's index once, then a clustered view around
    /// the viewport whenever the viewport leaves the last one. `true` when
    /// a view was installed.
    fn drive_points(&mut self, camera: &crate::render::Camera) -> bool {
        let mut changed = false;
        let zoom = camera.zoom.floor().clamp(0.0, 30.0) as u8;
        for name in self.store.point_source_names() {
            if let Some(data) = self.store.point_index_job(&name) {
                let (index, _) = crate::overlay::points::build_index_timed(&data);
                self.store.adopt_point_index(&name, data.revision(), Arc::new(index));
            }
            let (min, max) = point_viewport_box(camera, SHELL_REBUILD_SLACK);
            let want = crate::overlay::points::ViewKey::for_box(zoom, min, max, 0);
            let revision = self.store.source_point_data(&name).map(|d| d.revision());
            let current = self.points.get(&name).map(|(r, _)| *r);
            if self.store.point_view_key(&name).is_some_and(|k| k.covers(&want)) && current == revision {
                continue;
            }
            let Some((source, data, index, paint)) = self.store.point_view_job(&name) else { continue };
            let (min, max) = point_viewport_box(camera, SHELL_VIEW_GROW);
            let key = crate::overlay::points::ViewKey::for_box(zoom, min, max, 0);
            let view = crate::overlay::points::build_view(&source, &data, &index, &paint, key);
            let circles = view.circles.clone();
            let revision = data.revision();
            if self.store.install_point_view(&name, view) {
                self.points.insert(Arc::clone(&name), (revision, circles));
                changed = true;
            }
        }
        changed
    }

    /// The GPU half of a frame: uploads what landed, picks every cover
    /// tile's draw and writes this frame's uniforms. Then
    /// [`Self::push_draws`].
    pub fn prepare(
        &mut self,
        ctx: &crate::render::GpuContext,
        pipeline: &crate::render::TilePipeline,
        camera: &crate::render::Camera,
        globe: &crate::render::globe::GlobeUpload,
        cover: &[TileId],
    ) {
        self.order = self.store.source_names().cloned().collect();
        for write in std::mem::take(&mut self.pending_tables) {
            self.gpu.write_paint_tables(pipeline, ctx, &write);
        }
        let points = &self.points;
        self.gpu.sync_points(ctx, pipeline, camera, globe, &self.order, |name| {
            points
                .get(name)
                .and_then(|(_, circles)| circles.clone())
                .map(|circles| vec![(circles, crate::render::circles::CircleParams::NONE)])
                .unwrap_or_default()
        });
        self.frame =
            Some(self.gpu.prepare_tiles(&mut self.host, ctx, pipeline, camera, globe, &self.order, cover, drop));
    }

    /// Appends every source's draws, in source order: a tiled source's
    /// fills then outlines, then its circles -- and records, from those same
    /// draws, what each source drew ([`Self::source_draw_stats`]), through
    /// the loop `MapView::paint` uses. One pass; in steady state the
    /// recording allocates nothing (the ledger reuses its entries).
    pub fn push_draws<'a>(&'a self, draws: &mut Vec<crate::render::TileDraw<'a>>) {
        let mut over_buildings = Vec::new();
        self.push_draws_split(draws, &mut over_buildings, false);
        draws.append(&mut over_buildings);
    }

    /// [`Self::push_draws`] in two lists: `tiles` for the tile pass (under
    /// the buildings), `over_buildings` for the sources drawn over them,
    /// which a shell draws in `render_overlay_above_buildings` after its
    /// building draws. `buildings_drawn`: whether this frame draws any
    /// (`frame_plan::extrusion_plan`'s steps); with none, every source goes
    /// to `tiles`. The split is `render::overlay_order::overlay_pass`'s,
    /// `MapView::paint`'s.
    /// The data areas under the labels, for labels over data (see
    /// `MapView::set_basemap_label_paint`): every source's polygons, live.
    pub fn area_cover(&self) -> crate::overlay::hit::AreaCover {
        self.store.area_cover()
    }

    pub fn push_draws_split<'a>(
        &'a self,
        tiles: &mut Vec<crate::render::TileDraw<'a>>,
        over_buildings: &mut Vec<crate::render::TileDraw<'a>>,
        buildings_drawn: bool,
    ) {
        let mut ledger = self.drawn.borrow_mut();
        ledger.begin_frame();
        emit_source_draws_by_pass(
            &mut ledger,
            self.order.iter().map(|name| {
                use crate::render::overlay_order::{overlay_pass, BuildingSlot, LabelSlot};
                let building = if self.under_buildings.contains(name) {
                    BuildingSlot::UnderBuildings
                } else {
                    BuildingSlot::OverBuildings
                };
                (name, overlay_pass(LabelSlot::BelowLabels, building, buildings_drawn))
            }),
            // Every shell source is below the labels; `Vec::new` does not
            // allocate.
            [tiles, over_buildings, &mut Vec::new()],
            |name, draws| {
                if let Some(frame) = &self.frame {
                    frame.push_tile_draws(name, draws);
                }
                self.gpu.push_point_draws(name, draws);
            },
            |_, _| {},
        );
    }

    /// Forgets everything held on the GPU: the shell's device was replaced.
    /// The next [`Self::update`] asks for every wanted tile again.
    pub fn drop_gpu(&mut self) {
        self.gpu = OverlayGpu::default();
        self.frame = None;
    }

    /// What source `name` drew in the last [`Self::push_draws`] -- its
    /// tiles (stand-ins included), hairlines and circles -- stamped with
    /// that frame. `None` for a source that does not exist; zeros at
    /// `frame` 0 for one added since. Recorded by `push_draws` from the
    /// draws it emitted; this only reads it, and allocates nothing.
    #[must_use]
    pub fn source_draw_stats(&self, name: &str) -> Option<SourceDrawStats> {
        self.store.source_generation(name)?;
        Some(self.drawn.borrow().get(name))
    }

    /// Tile draws in the last prepared frame (stand-ins included) and
    /// circle layers held: what a shell's HUD reports.
    pub fn stats(&self) -> (u64, usize) {
        (
            self.frame.as_ref().map_or(0, |frame| frame.draw_count),
            self.gpu.point_circles.values().map(Vec::len).sum(),
        )
    }

    /// The feature under a screen point, topmost source first -- the hover
    /// and click hit test, `MapView::feature_at`'s rule.
    pub fn feature_at(
        &self,
        camera: &crate::render::Camera,
        screen_x: f32,
        screen_y: f32,
    ) -> Option<crate::overlay::FeatureRef> {
        // The shells always draw the globe below z12: pick on it, as
        // `MapView::feature_at` does.
        let ground = crate::render::globe::unproject_to_world_on_globe(camera, true, screen_x, screen_y)?;
        let pitch_scale = crate::render::pitch_scale_factor_at_ground(camera, ground.extend(0.0));
        let tolerance = crate::overlay::hit::tolerance_world(SHELL_HOVER_TOLERANCE_PX, camera.zoom, pitch_scale);
        let world_units_per_px = crate::overlay::hit::world_units_per_px(camera.zoom);
        // The store's order, not the last `prepare`'s snapshot of it: a pick
        // needs no frame drawn first (a test without a GPU never prepares).
        // One difference from that snapshot: a source pushed since the last
        // `prepare` can be picked before its tiles bake -- as any live pick
        // already hits geometry whose tile has not landed. A removed source
        // is gone from both. Collected for `.rev()` (topmost first).
        // Against each live source, not a snapshot of it: building one walks
        // every feature, which at 242 k block groups was the whole cost of a
        // hover (2-3 ms).
        let names: Vec<crate::overlay::SourceId> = self.store.source_names().cloned().collect();
        names
            .iter()
            .rev()
            .find_map(|name| self.store.live_feature_at(name, (ground.x, ground.y), tolerance, world_units_per_px))
    }

    /// A feature's properties, for a hover readout.
    pub fn feature_properties(
        &self,
        feature: &crate::overlay::FeatureRef,
    ) -> Option<Arc<crate::overlay::FeatureProperties>> {
        self.store.feature_properties(&feature.source, feature.index)
    }

    /// Every source that carries a label or an icon, in the store's draw
    /// order, with the sort-key base of its first feature: the overlay
    /// symbol pass's input (`symbol_pass::collect_overlay_candidates`,
    /// feature `i` of a source keyed `base + i`). The keys are
    /// `MapView::paint`'s: each source advances the base by its feature
    /// count, whether it offers symbols or not, so a source's keys do not
    /// move when another gains or loses labels. Whether a source has any
    /// is worked out once per generation (`has_symbols`), not per frame.
    pub fn symbol_sources(&self) -> Vec<(usize, Arc<Vec<crate::overlay::OverlayFeature>>)> {
        let mut out = Vec::new();
        let mut base = 0usize;
        let mut known = self.symbol_flags.borrow_mut();
        let names: Vec<crate::overlay::SourceId> = self.store.source_names().cloned().collect();
        known.retain(|name, _| names.contains(name));
        for name in names {
            let Some(snap) = self.store.source_paint_snapshot(&name) else { continue };
            let has = match known.get(&name) {
                Some((generation, has)) if *generation == snap.generation => *has,
                _ => {
                    let has = snap.features.iter().any(|f| f.paint.text_field.is_some() || f.paint.icon_image.is_some());
                    known.insert(Arc::clone(&name), (snap.generation, has));
                    has
                }
            };
            let len = snap.features.len();
            if has {
                out.push((base, snap.features));
            }
            base += len;
        }
        out
    }
}

// -- layers by string id ---------------------------------------------------
//
// The door an embedding host (the iOS `MapEngine`) pushes through: features
// with string ids and typed properties, owned, so the host can keep its own
// id map and re-push a layer whole. Plain data: no GeoJSON text, no
// per-feature property maps.


/// A property value of a [`LayerFeature`], read by the layer's declared
/// paint expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerValue {
    Number(f64),
    Text(String),
    Bool(bool),
}

/// A [`LayerFeature`]'s geometry: `[lon, lat]` positions.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerGeometry {
    Point([f64; 2]),
    /// One or more parts.
    Line(Vec<Vec<[f64; 2]>>),
    /// One polygon: exterior ring first, then holes.
    Polygon(Vec<Vec<[f64; 2]>>),
}

/// One feature of a layer, by stable string id.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerFeature {
    pub id: String,
    pub geometry: LayerGeometry,
    pub props: Vec<(String, LayerValue)>,
}

/// The layer's typed property columns: every key any feature carries, each
/// typed by its first non-null value. A value of another type reads as
/// absent, as a missing key does.
enum OwnedColumn {
    Number(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    Str(Vec<Option<String>>),
}

fn columns_of(features: &[LayerFeature]) -> Vec<(String, OwnedColumn)> {
    let mut keys: Vec<(String, OwnedColumn)> = Vec::new();
    for f in features {
        for (key, value) in &f.props {
            if keys.iter().any(|(k, _)| k == key) {
                continue;
            }
            let n = features.len();
            keys.push((
                key.clone(),
                match value {
                    LayerValue::Number(_) => OwnedColumn::Number(vec![None; n]),
                    LayerValue::Bool(_) => OwnedColumn::Bool(vec![None; n]),
                    LayerValue::Text(_) => OwnedColumn::Str(vec![None; n]),
                },
            ));
        }
    }
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    for (row, f) in features.iter().enumerate() {
        for (key, value) in &f.props {
            let Ok(at) = keys.binary_search_by(|(k, _)| k.as_str().cmp(key)) else { continue };
            match (&mut keys[at].1, value) {
                (OwnedColumn::Number(col), LayerValue::Number(v)) => col[row] = Some(*v),
                (OwnedColumn::Bool(col), LayerValue::Bool(v)) => col[row] = Some(*v),
                (OwnedColumn::Str(col), LayerValue::Text(v)) => col[row] = Some(v.clone()),
                _ => {}
            }
        }
    }
    keys
}

impl ShellOverlays {
    /// Replaces source `name` with `features`, each keeping its string id
    /// (what [`Self::feature_at`] hands back) and its properties as typed
    /// columns. All points take the point path ([`Self::set_points_with_ids`]),
    /// anything else the tiled path; `paint` is every feature's own.
    pub fn set_layer(
        &mut self,
        name: &str,
        features: &[LayerFeature],
        paint: &FeaturePaint,
        display_zoom: u8,
    ) -> Result<(), crate::overlay::FeatureError> {
        let owned = columns_of(features);
        let str_refs: Vec<Vec<Option<&str>>> = owned
            .iter()
            .map(|(_, c)| match c {
                OwnedColumn::Str(values) => values.iter().map(|v| v.as_deref()).collect(),
                _ => Vec::new(),
            })
            .collect();
        let columns: Vec<(&str, crate::overlay::columns::PropertyColumn<'_>)> = owned
            .iter()
            .zip(&str_refs)
            .map(|((key, column), strs)| {
                let column = match column {
                    OwnedColumn::Number(v) => crate::overlay::columns::PropertyColumn::Number(v),
                    OwnedColumn::Bool(v) => crate::overlay::columns::PropertyColumn::Bool(v),
                    OwnedColumn::Str(_) => crate::overlay::columns::PropertyColumn::Str(strs),
                };
                (key.as_str(), column)
            })
            .collect();
        let ids: Vec<Option<FeatureId>> = features.iter().map(|f| Some(FeatureId::String(f.id.clone()))).collect();
        let points: Option<Vec<[f64; 2]>> = features
            .iter()
            .map(|f| match f.geometry {
                LayerGeometry::Point(p) => Some(p),
                _ => None,
            })
            .collect();
        if let Some(lon_lat) = points {
            return self.set_points_with_ids(name, &lon_lat, &ids, paint, &columns);
        }
        // The engine borrows every level of the geometry; these two ladders
        // are the intermediate levels, a pointer per part.
        let points_of: Vec<[[f64; 2]; 1]> = features
            .iter()
            .map(|f| match f.geometry {
                LayerGeometry::Point(p) => [p],
                _ => [[0.0, 0.0]],
            })
            .collect();
        let parts: Vec<Vec<&[[f64; 2]]>> = features
            .iter()
            .map(|f| match &f.geometry {
                LayerGeometry::Line(parts) | LayerGeometry::Polygon(parts) => parts.iter().map(Vec::as_slice).collect(),
                LayerGeometry::Point(_) => Vec::new(),
            })
            .collect();
        let polygons: Vec<[&[&[[f64; 2]]]; 1]> = parts.iter().map(|rings| [rings.as_slice()]).collect();
        let inputs: Vec<crate::overlay::FeatureInput<'_>> = features
            .iter()
            .zip(ids)
            .enumerate()
            .map(|(i, (f, id))| crate::overlay::FeatureInput {
                id,
                geometry: match f.geometry {
                    LayerGeometry::Point(_) => crate::overlay::GeometryInput::Point(&points_of[i]),
                    LayerGeometry::Line(_) => crate::overlay::GeometryInput::Line(&parts[i]),
                    LayerGeometry::Polygon(_) => crate::overlay::GeometryInput::Polygon(&polygons[i]),
                },
                paint: paint.clone(),
            })
            .collect();
        self.set_source_columns(name, &inputs, &columns, display_zoom)
    }
}

/// The demo polygon source's name on every shell.
pub const DEMO_POLYGON_SOURCE: &str = "demo-blocks";
/// The demo point source's name on every shell.
pub const DEMO_POINT_SOURCE: &str = "demo-points";

/// A tiny deterministic generator (SplitMix64), so every shell draws the
/// same sample from the same seed without a `rand` dependency.
struct SampleRng(u64);

impl SampleRng {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A synthetic block-group-like FeatureCollection: `cols x rows` irregular
/// quadrilateral cells (each with a jittered interior vertex per edge, so
/// the outline is not a plain grid) of `cell_deg` degrees around
/// `(lon, lat)`, each carrying a numeric `value` in 0..100 that varies
/// smoothly across the sheet. The demo's "bundled sample": generated, so
/// the browser and the phone draw byte-identical data without shipping a
/// file.
pub fn sample_blocks_geojson(lon: f64, lat: f64, cols: u32, rows: u32, cell_deg: f64, seed: u64) -> String {
    let mut rng = SampleRng(seed);
    let (w, h) = (f64::from(cols) * cell_deg, f64::from(rows) * cell_deg);
    let (x0, y0) = (lon - w / 2.0, lat - h / 2.0);
    // Shared corner jitter, so neighbouring cells meet exactly.
    let jitter = cell_deg * 0.18;
    let mut corners = vec![[0.0f64; 2]; ((cols + 1) * (rows + 1)) as usize];
    for j in 0..=rows {
        for i in 0..=cols {
            let edge = i == 0 || j == 0 || i == cols || j == rows;
            let (dx, dy) =
                if edge { (0.0, 0.0) } else { ((rng.next_f64() - 0.5) * jitter, (rng.next_f64() - 0.5) * jitter) };
            corners[(j * (cols + 1) + i) as usize] =
                [x0 + f64::from(i) * cell_deg + dx, y0 + f64::from(j) * cell_deg + dy];
        }
    }
    let corner = |i: u32, j: u32| corners[(j * (cols + 1) + i) as usize];
    let mut out = String::with_capacity((cols * rows) as usize * 220);
    out.push_str(r#"{"type":"FeatureCollection","features":["#);
    for j in 0..rows {
        for i in 0..cols {
            let ring = [corner(i, j), corner(i + 1, j), corner(i + 1, j + 1), corner(i, j + 1)];
            let mut coords = Vec::with_capacity(9);
            for k in 0..4 {
                let (a, b) = (ring[k], ring[(k + 1) % 4]);
                coords.push(a);
                // A midpoint nudged off the edge; the same nudge from both
                // cells sharing the edge (seeded by the edge), so no gap.
                // Measured along lo -> hi, so both cells compute the same
                // point whichever way round their ring walks the edge.
                let (lo, hi) = if (a[0], a[1]) < (b[0], b[1]) { (a, b) } else { (b, a) };
                let (ex, ey) = (hi[0] - lo[0], hi[1] - lo[1]);
                let bump = (((lo[0] * 7919.0 + hi[1] * 104_729.0).sin() * 43_758.545).fract() - 0.5) * 0.12;
                coords.push([lo[0] + ex * 0.5 - ey * bump, lo[1] + ey * 0.5 + ex * bump]);
            }
            coords.push(ring[0]);
            let (u, v) = (f64::from(i) / f64::from(cols.max(1)), f64::from(j) / f64::from(rows.max(1)));
            let value = (50.0 + 30.0 * (u * 6.3).sin() * (v * 4.1).cos() + 20.0 * (rng.next_f64() - 0.5)).clamp(0.0, 100.0);
            if i + j > 0 {
                out.push(',');
            }
            out.push_str(r#"{"type":"Feature","id":"#);
            out.push_str(&(j * cols + i).to_string());
            out.push_str(r#","properties":{"value":"#);
            out.push_str(&format!("{value:.1}"));
            out.push_str(r#"},"geometry":{"type":"Polygon","coordinates":[["#);
            for (k, c) in coords.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                out.push_str(&format!("[{:.6},{:.6}]", c[0], c[1]));
            }
            out.push_str("]]}}");
        }
    }
    out.push_str("]}");
    out
}

/// `n` seeded points around `(lon, lat)`: a few Gaussian-ish clumps inside
/// `spread_deg`, so clusters form at low zoom and break apart zooming in.
pub fn sample_points(lon: f64, lat: f64, n: usize, spread_deg: f64, seed: u64) -> Vec<[f64; 2]> {
    let mut rng = SampleRng(seed ^ 0xA5A5_A5A5);
    let clumps: Vec<[f64; 2]> = (0..12)
        .map(|_| [lon + (rng.next_f64() - 0.5) * spread_deg, lat + (rng.next_f64() - 0.5) * spread_deg * 0.7])
        .collect();
    (0..n)
        .map(|k| {
            let c = clumps[k % clumps.len()];
            // Sum of three uniforms: cheap, roughly normal.
            let g = |rng: &mut SampleRng| (rng.next_f64() + rng.next_f64() + rng.next_f64() - 1.5) * spread_deg * 0.12;
            [c[0] + g(&mut rng), c[1] + g(&mut rng)]
        })
        .collect()
}

/// The demo polygons' paint: `value` ramped yellow -> teal -> purple at
/// 0.7 opacity, with a white hairline. The same shape as
/// `demos/us_block_groups.rs`'s income ramp.
pub fn demo_polygon_paint() -> serde_json::Map<String, serde_json::Value> {
    let paint = serde_json::json!({
        "fill-color": ["interpolate", ["linear"], ["get", "value"], 0, "#fde725", 50, "#21918c", 100, "#440154"],
        "fill-opacity": 0.7,
        "fill-outline-color": "#ffffff"
    });
    paint.as_object().cloned().unwrap_or_default()
}

/// Clusters coloured and sized by `point_count` -- MapLibre's cluster
/// example, and `demos/big_points.rs`'s.
pub fn demo_cluster_paint() -> serde_json::Map<String, serde_json::Value> {
    let paint = serde_json::json!({
        "circle-color": ["step", ["get", "point_count"], "#51bbd6", 100, "#f1c40f", 750, "#f28cb1"],
        "circle-radius": ["step", ["get", "point_count"], 12, 100, 16, 750, 22]
    });
    paint.as_object().cloned().unwrap_or_default()
}

impl ShellOverlays {
    /// The demo every shell loads the same way: `cols x rows` sample
    /// polygons ([`sample_blocks_geojson`]) and `points` clustered sample
    /// points around `(lon, lat)`.
    pub fn load_demo(
        &mut self,
        lon: f64,
        lat: f64,
        cols: u32,
        rows: u32,
        points: usize,
        display_zoom: u8,
    ) -> Result<(), crate::overlay::FeatureError> {
        let cell = 0.004;
        self.declare_source_paint(DEMO_POLYGON_SOURCE, &demo_polygon_paint())?;
        self.set_source(DEMO_POLYGON_SOURCE, &sample_blocks_geojson(lon, lat, cols, rows, cell, 7), display_zoom)?;
        if points > 0 {
            self.declare_source_paint(DEMO_POINT_SOURCE, &demo_cluster_paint())?;
            let spread = cell * f64::from(cols.max(rows)) * 1.5;
            let paint = crate::overlay::FeaturePaint {
                circle_radius: 5.0,
                circle_color: crate::style::Rgba { r: 0.11, g: 0.42, b: 0.78, a: 0.9 },
                ..crate::overlay::FeaturePaint::default()
            };
            self.set_points(
                DEMO_POINT_SOURCE,
                &sample_points(lon, lat, points, spread, 11),
                &paint,
                crate::overlay::ClusterOptions::default(),
            )?;
        }
        Ok(())
    }
}

/// Tests only: `camera`'s frame upload, for a test that drives
/// [`OverlayGpu::prepare_tiles`] from `map_view.rs` (which may not build a
/// `GlobeFrame` itself: `render::globe`'s one-call-per-frame scan).
#[cfg(test)]
pub(crate) fn test_globe_upload(camera: &crate::render::Camera) -> crate::render::globe::GlobeUpload {
    crate::render::globe::GlobeUpload::new(&crate::render::globe::globe_frame(camera), camera, 4096.0, glam::DVec3::Z, 1.0)
}

/// Whether an overlay tile set's per-tile revision maps have outgrown the
/// tiles that could still need them (`held`: cached, landed or empty) by
/// enough to be worth a pass. Twice, plus a floor, so the prune runs once
/// per many uploads, not every frame.
pub(crate) fn tile_revisions_need_pruning(revisions: usize, held: usize) -> bool {
    revisions > 2 * held + 64
}

/// Drops the revision entries of tiles nothing holds any more: `shown_rev`
/// keeps a tile that is `held` (cached or landed) or known empty,
/// `class_rev` one that is held. The one rule both the live-diff path
/// (`MapView::update_source_features`) and the upload path prune by.
pub(crate) fn prune_tile_revisions(
    shown_rev: &mut HashMap<TileId, u64>,
    class_rev: &mut HashMap<TileId, u64>,
    held: impl Fn(&TileId) -> bool,
    empty: &HashSet<TileId>,
) {
    shown_rev.retain(|t, _| held(t) || empty.contains(t));
    class_rev.retain(|t, _| held(t));
}

#[cfg(test)]
mod revision_tests {
    use super::*;

    /// A long pan over a static tiled source: every tile is uploaded once,
    /// the GPU cache holds 64, and the revision maps stay within the
    /// pruning bound of what is held -- they used to keep all 20,000.
    #[test]
    fn revision_maps_stay_bounded_over_a_long_pan() {
        let mut cache: crate::tile_source::TileCache<()> = crate::tile_source::TileCache::new(64);
        let mut shown_rev: HashMap<TileId, u64> = HashMap::new();
        let mut class_rev: HashMap<TileId, u64> = HashMap::new();
        let mut empty: HashSet<TileId> = HashSet::new();
        let mut peak = 0;
        for i in 0..20_000u32 {
            let tile = TileId { z: 14, x: 8_000 + i % 400, y: 5_000 + i / 400 };
            if i % 10 == 0 {
                empty.insert(tile);
                shown_rev.insert(tile, 0);
                continue;
            }
            cache.put(tile, ());
            shown_rev.insert(tile, 0);
            class_rev.insert(tile, 0);
            if tile_revisions_need_pruning(shown_rev.len().max(class_rev.len()), cache.len() + empty.len()) {
                prune_tile_revisions(&mut shown_rev, &mut class_rev, |t| cache.contains(t), &empty);
            }
            peak = peak.max(shown_rev.len());
            assert!(class_rev.len() <= 2 * (cache.len() + empty.len()) + 65);
        }
        // Every shown tile is still answered for: cached or empty.
        for t in cache.iter().map(|(t, _)| *t) {
            assert!(shown_rev.contains_key(&t), "a cached tile lost its revision");
        }
        assert!(peak <= 2 * (64 + empty.len()) + 65, "peak {peak}");
        assert!(class_rev.len() <= 2 * 64 + 65 + empty.len());
    }
}

/// Why a shell refuses `line-gradient` (`ShellOverlays::declare_source_paint`).
const SHELL_NO_LINE_GRADIENT: &str =
    "line-gradient needs line metrics, which are not supported on this shell yet";

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn a_shell_refuses_line_gradient_saying_it_is_not_supported_here_yet() {
        let mut shell = ShellOverlays::new();
        let paint: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"line-gradient":["interpolate",["linear"],["line-progress"],0,"blue",1,"red"]}"#).unwrap();
        let error = shell.declare_source_paint("route", &paint).unwrap_err().to_string();
        assert!(error.contains("not supported on this shell yet"), "{error}");
        assert!(!error.contains("declare_source_line_metrics"), "{error}");
    }

    #[test]
    fn a_sources_line_fx_defaults_to_none_and_carries_trim_and_opacity() {
        assert_eq!(SourceLineFx::default().uniform(), crate::render::LINE_FX_NONE);
        let fx = SourceLineFx { trim: Some([0.25, 0.75]), opacity: 0.4 };
        assert_eq!(fx.uniform(), [0.25, 0.75, 0.4, 1.0]);
        let mut gpu = OverlayGpu::default();
        assert_eq!(gpu.line_fx_of("route"), crate::render::LINE_FX_NONE);
        gpu.line_fx.insert("route".into(), fx.uniform());
        assert_eq!(gpu.line_fx_of("route"), [0.25, 0.75, 0.4, 1.0], "what every uniform of the source is written with");
        assert_eq!(gpu.line_fx_of("other"), crate::render::LINE_FX_NONE);
    }

    fn camera(lon: f64, lat: f64, zoom: f64) -> crate::render::Camera {
        crate::render::Camera {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 768.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    const LON: f64 = 139.7671;
    const LAT: f64 = 35.6812;

    #[test]
    fn the_sample_parses_and_every_cell_carries_a_value() {
        let json = sample_blocks_geojson(LON, LAT, 6, 4, 0.004, 7);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("the sample is JSON");
        let features = parsed["features"].as_array().expect("a FeatureCollection");
        assert_eq!(features.len(), 24);
        assert!(features.iter().all(|f| f["properties"]["value"].as_f64().is_some_and(|v| (0.0..=100.0).contains(&v))));
        assert_eq!(json, sample_blocks_geojson(LON, LAT, 6, 4, 0.004, 7), "seeded: every shell draws the same data");
    }

    #[test]
    fn a_columnar_source_is_tiled_bakes_and_picks_with_its_column() {
        let mut shell = ShellOverlays::new();
        let ring: Vec<[f64; 2]> =
            vec![[LON, LAT], [LON + 0.01, LAT], [LON + 0.01, LAT + 0.01], [LON, LAT + 0.01], [LON, LAT]];
        let rings = [&ring[..]];
        let polygons = [&rings[..]];
        let features = [crate::overlay::input::FeatureInput {
            id: Some(crate::overlay::FeatureId::Number(1.0)),
            geometry: crate::overlay::input::GeometryInput::Polygon(&polygons),
            paint: FeaturePaint::default(),
        }];
        let income = [Some(55_000.0)];
        shell.declare_source_paint("bg", &crate::census::ramp::fill_paint_object(false)).unwrap();
        shell
            .set_source_columns("bg", &features, &[("income", crate::overlay::columns::PropertyColumn::Number(&income))], 14)
            .unwrap();
        assert!(shell.store.source_snapshot("bg").unwrap().tiled, "forced onto the tiled path, as set_source is");
        let cam = camera(LON + 0.005, LAT + 0.005, 14.0);
        let cover = crate::geo::visible_tiles((LON + 0.005, LAT + 0.005), 14, (1024.0, 768.0));
        let mut frames = 0;
        while shell.update(&cam, &cover, Duration::from_secs(5)) && frames < 20 {
            for set in shell.host.tile_sets.values_mut() {
                set.landed.clear();
            }
            frames += 1;
        }
        // No GPU here, so what landed is never uploaded and the layer never
        // reports idle (the page's own MG-C5 run is where idle is proven);
        // what baked is on the set.
        let set = shell.host.tile_sets.get("bg").expect("the tile set was installed");
        assert!(set.in_flight.is_empty(), "every wanted tile was baked");
        assert!(!set.landed_rev.is_empty(), "the square's tiles baked");
        let hit = shell
            .feature_at(&cam, cam.viewport_width_px / 2.0, cam.viewport_height_px / 2.0)
            .expect("a hit at the centre");
        let props = shell.feature_properties(&hit).expect("properties");
        assert!(
            matches!(props.get("income"), Some(crate::mvt::TagValue::Number(v)) if *v == 55_000.0),
            "{props:?}"
        );
    }

    /// `set_layer` keeps each feature's string id through both paths (points
    /// and tiled), and its typed properties.
    #[test]
    fn a_layer_by_string_id_picks_back_its_ids_on_both_paths() {
        let mut shell = ShellOverlays::new();
        let cam = camera(LON, LAT, 14.0);
        let (x, y) = (cam.viewport_width_px / 2.0, cam.viewport_height_px / 2.0);
        let props = vec![("n".to_string(), LayerValue::Number(1.0)), ("t".to_string(), LayerValue::Text("x".into()))];
        let stop = LayerFeature { id: "stop-1".into(), geometry: LayerGeometry::Point([LON, LAT]), props: props.clone() };
        shell.set_layer("stops", &[stop], &FeaturePaint::default(), 14).expect("points push");
        // A point source takes the point path (picked by the embedding host
        // against its own positions; the store's hit index is tiled-only).
        assert!(shell.store.source_point_data("stops").is_some(), "points took the point path");
        let line = LayerFeature {
            id: "route-9".into(),
            geometry: LayerGeometry::Line(vec![vec![[LON - 0.01, LAT], [LON + 0.01, LAT]]]),
            props,
        };
        shell.remove_source("stops");
        shell.set_layer("routes", &[line], &FeaturePaint::default(), 14).expect("lines push");
        let hit = shell.feature_at(&cam, x, y).expect("the line picks");
        assert_eq!((&*hit.source, hit.id), ("routes", Some(FeatureId::String("route-9".into()))));
    }

    /// Mixed value types under one key: the first type wins, the rest read
    /// as absent.
    #[test]
    fn layer_columns_are_typed_by_the_first_value() {
        let f = |id: &str, props: Vec<(String, LayerValue)>| LayerFeature {
            id: id.into(),
            geometry: LayerGeometry::Point([0.0, 0.0]),
            props,
        };
        let columns = columns_of(&[
            f("a", vec![("n".into(), LayerValue::Number(1.0))]),
            f("b", vec![("n".into(), LayerValue::Text("x".into())), ("t".into(), LayerValue::Bool(true))]),
        ]);
        assert_eq!(columns.len(), 2);
        assert!(matches!(&columns[0], (k, OwnedColumn::Number(v)) if k == "n" && v == &vec![Some(1.0), None]));
        assert!(matches!(&columns[1], (k, OwnedColumn::Bool(v)) if k == "t" && v == &vec![None, Some(true)]));
    }

    /// `setMarkers`' layers go in as point sources and a pick on a dot maps
    /// back to the page's marker index (`overlay::markers`): the address
    /// dot under the centre is marker 1, not the arrival point beside it.
    #[test]
    fn a_marker_dot_picks_back_to_its_page_index() {
        use crate::overlay::markers::{marker_for_pick, marker_layers, parse_markers};
        let mut shell = ShellOverlays::new();
        let list = format!(
            r#"[{{"lon":{},"lat":{},"kind":"arrival","label":"arrival"}},{{"lon":{LON},"lat":{LAT},"label":"address"}}]"#,
            LON + 0.002,
            LAT + 0.002
        );
        let layers = marker_layers(&parse_markers(&list).unwrap());
        let off = crate::overlay::ClusterOptions { cluster: false, ..crate::overlay::ClusterOptions::default() };
        for l in &layers {
            shell.set_points(&l.source, &l.lon_lat, &l.paint, off.clone()).unwrap();
        }
        let cam = camera(LON, LAT, 16.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 16, (1024.0, 768.0));
        let mut frames = 0;
        while shell.update(&cam, &cover, Duration::from_secs(5)) && frames < 20 {
            frames += 1;
        }
        let hit = shell
            .feature_at(&cam, cam.viewport_width_px / 2.0, cam.viewport_height_px / 2.0)
            .expect("the address dot is under the centre");
        assert_eq!(&*hit.source, "markers:address", "the address dot draws on top of its ring");
        assert_eq!(marker_for_pick(&layers, &hit.source, hit.index), Some(1));
        assert!(shell.feature_at(&cam, 1.0, 1.0).is_none(), "nothing in the corner");
    }

    /// Hovering the sky beside a z2 globe: no ground under the pointer, so
    /// no pick and no panic.
    #[test]
    fn a_pick_off_the_globe_is_none() {
        let mut shell = ShellOverlays::new();
        shell.load_demo(LON, LAT, 20, 14, 0, 2).expect("the demo loads");
        let cam = camera(LON, LAT, 2.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 2, (1024.0, 768.0));
        let mut frames = 0;
        while shell.update(&cam, &cover, Duration::from_secs(5)) && frames < 20 {
            for set in shell.host.tile_sets.values_mut() {
                set.landed.clear();
            }
            frames += 1;
        }
        assert!(shell.feature_at(&cam, 1.0, 1.0).is_none(), "a corner pixel is off the globe");
        // The sample is under the centre: the same shell does pick there,
        // so the corner's None is the globe's edge and not an empty shell.
        let (x, y) = (cam.viewport_width_px / 2.0, cam.viewport_height_px / 2.0);
        assert!(shell.feature_at(&cam, x, y).is_some(), "the sample under the centre picks");
    }

    #[test]
    fn a_small_polygon_source_is_tiled_and_its_cover_bakes_inline() {
        let mut shell = ShellOverlays::new();
        shell.load_demo(LON, LAT, 20, 14, 0, 14).expect("the demo loads");
        let snapshot = shell.store.source_snapshot(DEMO_POLYGON_SOURCE).expect("the source exists");
        assert!(snapshot.tiled, "a shell has no whole-source mesh: a small polygon source is forced onto the tiled path");
        drop(snapshot);
        let cam = camera(LON, LAT, 14.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 14, (1024.0, 768.0));
        // Frame 1 builds the index and bakes at least one tile; enough
        // frames with a generous budget bake the whole cover.
        let mut frames = 0;
        while shell.update(&cam, &cover, Duration::from_secs(5)) && frames < 20 {
            // No GPU in a unit test: take what landed as uploaded.
            for set in shell.host.tile_sets.values_mut() {
                set.landed.clear();
            }
            frames += 1;
        }
        let set = shell.host.tile_sets.get(DEMO_POLYGON_SOURCE).expect("the tile set was installed");
        assert!(set.in_flight.is_empty(), "every wanted tile was baked");
        assert!(set.landed_rev.len() + set.empty.len() >= cover.len(), "every cover tile landed or is known empty");
        assert!(!set.landed_rev.is_empty(), "the sample has tiles with polygons in them");
    }

    #[test]
    fn a_clustered_point_source_builds_its_index_and_a_view_with_circles() {
        let mut shell = ShellOverlays::new();
        shell.load_demo(LON, LAT, 4, 4, 5_000, 11).expect("the demo loads");
        let cam = camera(LON, LAT, 11.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 11, (1024.0, 768.0));
        assert!(shell.update(&cam, &cover, Duration::from_millis(50)));
        let (_, circles) = shell.points.get(DEMO_POINT_SOURCE).expect("a view was built on the first frame");
        let circles = circles.as_ref().expect("a point view draws as instanced circles");
        assert!(!circles.set.instances.is_empty());
        assert!(circles.set.instances.len() < 5_000, "clustered at z11: fewer circles than points");
    }

    /// 400 squares whose incomes spread over the whole ramp
    /// ($2,500..$250,000), every 37th with no estimate, pushed as source
    /// `bg` under the census ramp with its outline.
    fn push_census_like(shell: &mut ShellOverlays) {
        shell.declare_source_paint("bg", &crate::census::ramp::fill_paint_object(true)).unwrap();
        let rings: Vec<Vec<[f64; 2]>> = (0..400)
            .map(|i| {
                let (x, y) = (LON + f64::from(i % 20) * 0.002, LAT + f64::from(i / 20) * 0.002);
                vec![[x, y], [x + 0.0019, y], [x + 0.0019, y + 0.0019], [x, y + 0.0019], [x, y]]
            })
            .collect();
        let ring_refs: Vec<[&[[f64; 2]]; 1]> = rings.iter().map(|r| [&r[..]]).collect();
        let polys: Vec<[&[&[[f64; 2]]]; 1]> = ring_refs.iter().map(|r| [&r[..]]).collect();
        let features: Vec<crate::overlay::input::FeatureInput<'_>> = polys
            .iter()
            .map(|p| crate::overlay::input::FeatureInput {
                id: None,
                geometry: crate::overlay::input::GeometryInput::Polygon(&p[..]),
                paint: FeaturePaint::default(),
            })
            .collect();
        let income: Vec<Option<f64>> =
            (0..400).map(|i| if i % 37 == 0 { None } else { Some(2_500.0 + f64::from(i) * 620.0) }).collect();
        shell
            .set_source_columns(
                "bg",
                &features,
                &[("income", crate::overlay::columns::PropertyColumn::Number(&income))],
                14,
            )
            .unwrap();
    }

    fn census_like_shell() -> (ShellOverlays, crate::render::Camera, Vec<TileId>) {
        // 400 squares whose incomes spread over the whole ramp ($2,500..$250,000),
        // so a moving range really changes which classes are visible.
        let mut shell = ShellOverlays::new();
        // These tests step without waiting: check at once, as before the
        // settle existed (its own test sets one).
        shell.set_paint_check_settle(Duration::ZERO);
        push_census_like(&mut shell);
        let cam = camera(LON, LAT, 14.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 14, (1024.0, 768.0));
        for _ in 0..40 {
            if !shell.update(&cam, &cover, Duration::from_secs(5)) {
                break;
            }
            for set in shell.host.tile_sets.values_mut() {
                set.landed.clear();
            }
        }
        (shell, cam, cover)
    }

    /// A drag repaints every frame: the full check (a `classify` over every
    /// feature) must not run between the moves, only once the range has
    /// held for [`PAINT_CHECK_SETTLE`] -- and then still catch a split.
    #[test]
    fn the_full_check_waits_for_the_range_to_settle_and_still_catches_a_split() {
        let (mut shell, cam, cover) = census_like_shell();
        shell.set_paint_check_settle(Duration::from_millis(60));
        let epoch = shell.source_paint_epoch("bg").expect("the fixture's tile set was built");
        let checks = shell.paint_checks_run();
        // A snapped drag, one move per update, well inside the settle.
        for lo in (0..30).step_by(3) {
            let range = crate::census::ramp::IncomeRange { lo, hi: lo + 40 };
            let paint: serde_json::Map<_, _> = serde_json::from_str(&range.paint_json(true)).unwrap();
            assert_eq!(shell.update_source_paint("bg", &paint).unwrap(), SourcePaintPath::TableOnly);
            assert!(shell.update(&cam, &cover, Duration::from_millis(50)), "a check is owed: come back");
        }
        // The drag ends on a range that splits a class.
        let paint: serde_json::Map<_, _> =
            serde_json::from_str(&crate::census::ramp::range_paint_json(true, Some(8_400.0), None)).unwrap();
        assert_eq!(shell.update_source_paint("bg", &paint).unwrap(), SourcePaintPath::TableOnly);
        shell.update(&cam, &cover, Duration::from_millis(50));
        assert_eq!(shell.paint_checks_run(), checks, "no full check ran during the drag");
        assert!(shell.host.tile_sets.get("bg").unwrap().verify.unverified.is_some(), "one is still owed");
        std::thread::sleep(Duration::from_millis(80));
        for _ in 0..40 {
            if !shell.update(&cam, &cover, Duration::from_secs(5)) {
                break;
            }
            for set in shell.host.tile_sets.values_mut() {
                set.landed.clear();
            }
        }
        assert_eq!(shell.paint_checks_run(), checks + 1, "one check, for the settled range");
        let set = shell.host.tile_sets.get("bg").unwrap();
        assert!(set.verify.unverified.is_none());
        assert_eq!(set.epoch, epoch + 1, "which found the split: one re-bake");
    }

    #[test]
    fn the_default_settle_is_a_drag_pause() {
        assert_eq!(ShellOverlays::new().paint_check_settle, PAINT_CHECK_SETTLE);
        assert_eq!(PAINT_CHECK_SETTLE, Duration::from_millis(150));
    }

    #[test]
    fn a_snapped_drag_is_table_only_on_every_step() {
        let (mut shell, cam, cover) = census_like_shell();
        let stops = crate::census::ramp::slider_stops();
        let epoch = shell.source_paint_epoch("bg");
        assert!(epoch.is_some(), "the fixture's tile set was built");
        for lo in (0..60).step_by(3) {
            let range = crate::census::ramp::IncomeRange { lo, hi: lo + 40 };
            let paint: serde_json::Map<_, _> = serde_json::from_str(&range.paint_json(true)).unwrap();
            assert_eq!(
                shell.update_source_paint("bg", &paint).unwrap(),
                SourcePaintPath::TableOnly,
                "lo={lo} ({})",
                stops[lo]
            );
            shell.update(&cam, &cover, Duration::from_millis(50));
        }
        assert_eq!(shell.source_paint_epoch("bg"), epoch, "no re-bake happened");
        assert!(
            shell.host.tile_sets.get("bg").unwrap().verify.unverified.is_none(),
            "and the last step was checked against every feature"
        );
    }

    #[test]
    fn an_unsnapped_range_settles() {
        let (mut shell, cam, cover) = census_like_shell();
        let paint: serde_json::Map<_, _> =
            serde_json::from_str(&crate::census::ramp::range_paint_json(true, Some(33_333.3), None)).unwrap();
        let path = shell.update_source_paint("bg", &paint).unwrap();
        for _ in 0..40 {
            if !shell.update(&cam, &cover, Duration::from_secs(5)) {
                break;
            }
            for set in shell.host.tile_sets.values_mut() {
                set.landed.clear();
            }
        }
        assert!(matches!(path, SourcePaintPath::TableOnly | SourcePaintPath::Rebake));
        let set = shell.host.tile_sets.get("bg").unwrap();
        assert!(set.verify.unverified.is_none(), "the inline full check ran");
        assert!(set.in_flight.is_empty(), "and any re-bake it forced finished");
    }

    /// A paint update before the tile set is built replans: the store's
    /// stamp moves (so the rebuilt set cannot collide with a GPU set keyed
    /// at the old generation) and the set that installs draws the new paint.
    #[test]
    fn a_paint_update_before_the_first_build_replans_and_draws_the_new_paint() {
        let mut shell = ShellOverlays::new();
        push_census_like(&mut shell);
        let before = shell.store.source_generation("bg").unwrap();
        let red: serde_json::Map<_, _> = serde_json::from_str(r##"{"fill-color": "#ff0000"}"##).unwrap();
        assert_eq!(shell.update_source_paint("bg", &red).unwrap(), SourcePaintPath::Replan);
        let after = shell.store.source_generation("bg").unwrap();
        assert!(after > before, "the replan moved the stamp");
        let cam = camera(LON, LAT, 14.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 14, (1024.0, 768.0));
        for _ in 0..40 {
            if !shell.update(&cam, &cover, Duration::from_secs(5)) {
                break;
            }
            for set in shell.host.tile_sets.values_mut() {
                set.landed.clear();
            }
        }
        let set = shell.host.tile_sets.get("bg").expect("the set was built");
        assert_eq!(set.generation, after, "built at the replanned generation");
        assert_eq!(set.classes.classes.len(), 1, "one literal colour: one class, the new paint");
        assert!(set.in_flight.is_empty());
    }

    /// A point source's paint update drops its view; the next `update`
    /// rebuilds it.
    #[test]
    fn a_point_source_paint_update_rebuilds_its_view() {
        let mut shell = ShellOverlays::new();
        shell.load_demo(LON, LAT, 4, 3, 200, 11).expect("the demo loads");
        let cam = camera(LON, LAT, 11.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 11, (1024.0, 768.0));
        shell.update(&cam, &cover, Duration::from_secs(5));
        assert!(shell.points.contains_key(DEMO_POINT_SOURCE), "a view was built");
        let paint: serde_json::Map<_, _> = serde_json::from_str(r##"{"circle-color": "#ff0000"}"##).unwrap();
        assert_eq!(shell.update_source_paint(DEMO_POINT_SOURCE, &paint).unwrap(), SourcePaintPath::Replan);
        assert!(!shell.points.contains_key(DEMO_POINT_SOURCE), "the view was dropped");
        assert!(shell.update(&cam, &cover, Duration::from_secs(5)), "and a new one built");
        assert!(shell.points.contains_key(DEMO_POINT_SOURCE));
    }

    /// An edge inside a paint class that holds features on both sides of
    /// it: squares 9 ($8,080) and 10 ($8,700) share the ramp step
    /// [$7,989, $8,987), and $8,400 splits them. The representatives (one
    /// feature per class) cannot see that, so the update goes out
    /// table-only; the inline full check catches it and re-bakes, once.
    #[test]
    fn a_class_splitting_range_is_caught_by_the_check_and_rebakes() {
        let (mut shell, cam, cover) = census_like_shell();
        let epoch = shell.source_paint_epoch("bg").expect("the fixture's tile set was built");
        let paint: serde_json::Map<_, _> =
            serde_json::from_str(&crate::census::ramp::range_paint_json(true, Some(8_400.0), None)).unwrap();
        assert_eq!(shell.update_source_paint("bg", &paint).unwrap(), SourcePaintPath::TableOnly);
        assert!(shell.host.tile_sets.get("bg").unwrap().verify.unverified.is_some(), "owed its full check");
        for _ in 0..40 {
            if !shell.update(&cam, &cover, Duration::from_secs(5)) {
                break;
            }
            for set in shell.host.tile_sets.values_mut() {
                set.landed.clear();
            }
        }
        let set = shell.host.tile_sets.get("bg").unwrap();
        assert!(set.verify.unverified.is_none(), "the check ran");
        assert_eq!(set.epoch, epoch + 1, "and found the split: one re-bake");
        assert!(set.in_flight.is_empty(), "which finished");
    }

    #[test]
    fn a_paint_update_to_an_unknown_source_is_refused_and_a_bad_one_changes_nothing() {
        let (mut shell, _, _) = census_like_shell();
        let red: serde_json::Map<_, _> = serde_json::from_str(r##"{"fill-color": "#ff0000"}"##).unwrap();
        assert!(shell.update_source_paint("nope", &red).is_err());
        let bad: serde_json::Map<_, _> = serde_json::from_str(r#"{"fill-color": ["frobnicate"]}"#).unwrap();
        let rev = shell.host.tile_sets.get("bg").unwrap().paint_rev;
        assert!(shell.update_source_paint("bg", &bad).is_err());
        assert_eq!(shell.host.tile_sets.get("bg").unwrap().paint_rev, rev, "a refused paint changes nothing");
        assert!(shell.pending_tables.is_empty());
    }

    /// A snapped range update recolours what is on screen through the
    /// paint tables alone: the tiles on the GPU are the very same objects
    /// before and after, and their bytes do not move.
    #[test]
    fn a_table_write_recolours_without_a_new_upload() {
        let size = 256u32;
        // The centre of square 210 (row 10, column 10): $132,700, which a
        // range ending near $42K hides.
        let (lon, lat) = (LON + 0.021, LAT + 0.021);
        let cam = crate::render::Camera {
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            ..camera(lon, lat, 14.0)
        };
        let cover = crate::geo::visible_tiles((lon, lat), 14, (f64::from(size), f64::from(size)));
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let globe = test_globe_upload(&cam);
        let render = |shell: &mut ShellOverlays| {
            shell.prepare(&ctx, &pipeline, &cam, &globe, &cover);
            let mut draws = Vec::new();
            shell.push_draws(&mut draws);
            pollster::block_on(crate::render::gpu::tests::render_tile_draws(
                &ctx,
                &pipeline,
                &cam,
                size,
                wgpu::Color::BLACK,
                &draws,
            ))
        };
        let centre = |px: &[u8]| {
            let at = ((size / 2 * size + size / 2) * 4) as usize;
            px[at..at + 4].to_vec()
        };
        let mut shell = ShellOverlays::new();
        push_census_like(&mut shell);
        let mut before = Vec::new();
        for _ in 0..20 {
            let more = shell.update(&cam, &cover, Duration::from_secs(5));
            before = render(&mut shell);
            if !more {
                break;
            }
        }
        assert_ne!(centre(&before), vec![0, 0, 0, 255], "the square under the centre draws");
        let held = |shell: &ShellOverlays| {
            let set = shell.gpu.tile_gpu.get("bg").expect("the source has tiles on the GPU");
            let mut tiles: Vec<((u8, u32, u32), usize)> =
                set.cache.iter().map(|(id, tile)| ((id.z, id.x, id.y), Arc::as_ptr(tile) as usize)).collect();
            tiles.sort_unstable();
            (set.key, tiles, shell.gpu.tile_bytes())
        };
        let was = held(&shell);
        assert!(!was.1.is_empty());

        let range = crate::census::ramp::IncomeRange { lo: 0, hi: 40 };
        let paint: serde_json::Map<_, _> = serde_json::from_str(&range.paint_json(true)).unwrap();
        // Not `render::allocation_stats()`: its buffer counters are
        // process-global and every other GPU test in this binary moves them
        // underneath us. The same tile objects at the same key, and the same
        // bytes held, is the exact per-shell statement of "no new upload".
        assert_eq!(shell.update_source_paint("bg", &paint).unwrap(), SourcePaintPath::TableOnly);
        let after = render(&mut shell);
        assert_ne!(centre(&after), centre(&before), "the centre square recoloured (hidden)");
        assert_eq!(centre(&after), vec![0, 0, 0, 255], "hidden: the background shows through");
        assert_eq!(held(&shell), was, "the same tiles at the same key: nothing re-baked or re-uploaded");
    }

    /// A re-push of a tiled source keeps its old picture on screen until
    /// the new one covers the view -- MapLibre's `setData` -- and never a
    /// frame with holes in between; a removal still blanks at once.
    #[test]
    fn a_repushed_source_keeps_its_old_picture_until_the_new_one_covers() {
        let size = 512u32;
        let cam = crate::render::Camera {
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            ..camera(LON, LAT, 14.5)
        };
        let cover = crate::geo::visible_tiles((LON, LAT), 14, (f64::from(size), f64::from(size)));
        assert!(cover.len() > 1, "one bake per frame must leave holes: {cover:?}");
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let globe = test_globe_upload(&cam);
        let json = |seed: u64| sample_blocks_geojson(LON, LAT, 20, 14, 0.004, seed);
        let push = |shell: &mut ShellOverlays, seed: u64| {
            shell.declare_source_paint("net", &demo_polygon_paint()).unwrap();
            shell.set_source("net", &json(seed), 14).unwrap();
        };
        // One frame the way a shell runs it; `budget` zero bakes one tile.
        let frame = |shell: &mut ShellOverlays, budget: Duration| {
            let more = shell.update(&cam, &cover, budget);
            shell.prepare(&ctx, &pipeline, &cam, &globe, &cover);
            let mut draws = Vec::new();
            shell.push_draws(&mut draws);
            let pixels = pollster::block_on(crate::render::gpu::tests::render_tile_draws(
                &ctx,
                &pipeline,
                &cam,
                size,
                wgpu::Color::BLACK,
                &draws,
            ));
            (pixels, more)
        };
        let settle = |shell: &mut ShellOverlays| {
            let mut last = frame(shell, Duration::from_secs(5));
            for _ in 0..20 {
                if !last.1 {
                    break;
                }
                last = frame(shell, Duration::from_secs(5));
            }
            assert!(!last.1, "the cover bakes in 20 frames");
            last.0
        };
        let painted = |px: &[u8]| px.chunks(4).filter(|p| p != &[0, 0, 0, 255]).count();
        let reference = |seed: u64| {
            let mut shell = ShellOverlays::new();
            push(&mut shell, seed);
            settle(&mut shell)
        };
        let (b_ref, c_ref) = (reference(8), reference(9));

        let mut shell = ShellOverlays::new();
        push(&mut shell, 7);
        let a = settle(&mut shell);
        assert!(painted(&a) > 100_000, "the fixture covers the view: {}", painted(&a));
        assert_ne!(a, b_ref, "A and B draw differently");
        assert_ne!(b_ref, c_ref, "B and C draw differently");

        // Replace: before the new set is built, the old picture draws
        // whole; as B's tiles land they show at once, tile by tile, and the
        // tiles B has not reached keep A's -- never a blank tile.
        push(&mut shell, 8);
        shell.prepare(&ctx, &pipeline, &cam, &globe, &cover);
        let mut draws = Vec::new();
        shell.push_draws(&mut draws);
        let before_build = pollster::block_on(crate::render::gpu::tests::render_tile_draws(
            &ctx,
            &pipeline,
            &cam,
            size,
            wgpu::Color::BLACK,
            &draws,
        ));
        assert_eq!(painted(&before_build), painted(&a), "a re-push must not blank the source before its build");
        assert!(before_build == a, "and what draws is the old picture");
        let (first, _) = frame(&mut shell, Duration::ZERO);
        assert_eq!(painted(&first), painted(&a), "one tile of B landed: no tile is blank");
        assert!(first != a, "and the landed tile already shows B");

        // Pushes faster than a cover bakes (every 2 frames, one bake per
        // frame, against a cover of more tiles): every frame is whole, and
        // the picture advances with each push rather than freezing on A.
        let bound = |shell: &ShellOverlays| {
            let held = shell.gpu.tile_gpu.get("net").expect("the source has tiles on the GPU");
            assert!(held.previous.len() <= MAX_STAND_INS, "stand-ins are bounded");
            assert!(held.held_bytes() <= OVERLAY_TILE_GPU_BUDGET_BYTES, "a small fixture stays under one budget (the cap itself: a_repush_at_the_cache_budget_never_blanks)");
        };
        let mut last = first;
        for seed in 20..26u64 {
            push(&mut shell, seed);
            let (f1, _) = frame(&mut shell, Duration::ZERO);
            bound(&shell);
            let (f2, _) = frame(&mut shell, Duration::ZERO);
            bound(&shell);
            assert_eq!(painted(&f1), painted(&a), "push {seed}: no blank tile");
            assert_eq!(painted(&f2), painted(&a), "push {seed}: no blank tile");
            assert!(f2 != last, "push {seed}: the picture advanced");
            last = f2;
        }

        // A -> B -> C, the last push settling: ends on C, pixel for pixel,
        // with every older picture released.
        push(&mut shell, 9);
        let end = settle(&mut shell);
        assert!(end == c_ref, "ends on C, pixel for pixel");
        let held = shell.gpu.tile_gpu.get("net").expect("the source has tiles on the GPU");
        assert!(held.previous.is_empty(), "the old pictures are released once the new one covers the view");

        // Remove: blank at once.
        assert!(shell.remove_source("net"));
        shell.prepare(&ctx, &pipeline, &cam, &globe, &cover);
        let mut draws = Vec::new();
        shell.push_draws(&mut draws);
        assert!(draws.is_empty(), "a removed source draws nothing on the next frame");
        assert!(shell.gpu.tile_gpu.is_empty(), "and holds nothing on the GPU");
    }

    /// The per-source drawn counter under churn, and what makes it mean
    /// something: the source first bakes its WHOLE cover, then is re-pushed
    /// every frame with one tile bake per frame -- far slower than a cover
    /// -- so on every churn frame most cover tiles are holes in the new
    /// generation and only its stand-ins (the older generations' tiles)
    /// can fill them. Every churn frame must still draw the full cover:
    /// as many draws as the settled source, and at least half its
    /// triangles. With stand-ins disabled (`compose_cover` never picking
    /// one) this fails on the first churn frame -- checked by hand when the
    /// test was written, since only the newest landed tile then draws.
    #[test]
    fn a_repushed_tiled_source_draws_its_whole_cover_on_every_churn_frame() {
        let size = 512u32;
        let cam = crate::render::Camera {
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            ..camera(LON, LAT, 14.5)
        };
        let cover = crate::geo::visible_tiles((LON, LAT), 14, (f64::from(size), f64::from(size)));
        assert!(cover.len() >= 4, "a cover several bakes wide: {cover:?}");
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let globe = test_globe_upload(&cam);
        let mut shell = ShellOverlays::new();
        assert_eq!(shell.source_draw_stats("net"), None, "no such source yet");
        shell.declare_source_paint("net", &demo_polygon_paint()).unwrap();
        let push = |shell: &mut ShellOverlays, seed: u64| {
            shell.set_source("net", &sample_blocks_geojson(LON, LAT, 20, 14, 0.004, seed), 14).unwrap();
        };
        push(&mut shell, 100);
        let fresh = shell.source_draw_stats("net").expect("the source exists");
        assert_eq!((fresh.frame, fresh.drew()), (0, false), "pushed, not yet prepared: frame 0");

        // Settle: the whole cover baked and drawn.
        let mut settled = None;
        for _ in 0..20 {
            let more = shell.update(&cam, &cover, Duration::from_secs(5));
            shell.prepare(&ctx, &pipeline, &cam, &globe, &cover);
            shell.push_draws(&mut Vec::new());
            if !more {
                settled = shell.source_draw_stats("net");
                break;
            }
        }
        let settled = settled.expect("the cover bakes in 20 frames");
        assert!(settled.draw_calls >= cover.len() as u64, "every cover tile draws: {settled:?} over {} tiles", cover.len());
        assert!(settled.triangles > 0);

        // Churn: a push every frame, one bake a frame.
        let mut last_frame = settled.frame;
        for frame in 0..12u64 {
            push(&mut shell, 200 + frame);
            shell.update(&cam, &cover, Duration::ZERO);
            shell.prepare(&ctx, &pipeline, &cam, &globe, &cover);
            let mut draws = Vec::new();
            shell.push_draws(&mut draws);
            let stats = shell.source_draw_stats("net").expect("still there");
            assert!(stats.frame > last_frame, "churn frame {frame}: a fresh frame's figures");
            last_frame = stats.frame;
            assert_eq!(
                (stats.draw_calls, stats.triangles),
                (draws.len() as u64, SourceDrawStats::of(&draws).triangles),
                "churn frame {frame}: the counter is the frame's draw list"
            );
            assert!(
                stats.draw_calls >= settled.draw_calls && stats.triangles * 2 >= settled.triangles,
                "churn frame {frame}: the whole cover must draw (new tiles or stand-ins): {stats:?} vs settled {settled:?}"
            );
        }

        // Remove, then add again under the same name: no inherited figures.
        assert!(shell.remove_source("net"));
        assert_eq!(shell.source_draw_stats("net"), None, "a removed source has no stats");
        shell.declare_source_paint("net", &demo_polygon_paint()).unwrap();
        push(&mut shell, 7);
        let again = shell.source_draw_stats("net").expect("re-added");
        assert_eq!(again, SourceDrawStats::default(), "a re-added source starts at frame 0 with zeros");
    }

    /// A whole-source mesh's chunks, as `MapView::paint` draws them.
    fn whole_source_chunks(
        ctx: &crate::render::GpuContext,
        pipeline: &crate::render::TilePipeline,
    ) -> (Vec<OverlayChunkGpu>, crate::render::TileMesh) {
        let mut store = crate::overlay::OverlayStore::new();
        store.set_source("net", &sample_blocks_geojson(LON, LAT, 20, 14, 0.004, 3), 14).unwrap();
        store.force_untiled("net", 14);
        let plan = store.source_plan("net").expect("a whole-source plan").clone();
        let (mesh, _, extras) = crate::render::mesh::tessellate_overlay_plan_reusing(
            &plan.plan,
            &plan.fill_outlines,
            crate::render::subdivide::OVERLAY_SUBDIVISION_EXTENT,
            plan.origin.coord_scale,
            crate::render::subdivide::Subdivision::OVERLAY,
            &|| false,
            crate::render::TileMesh::default(),
        )
        .unwrap_or_else(|_| panic!("uncancelled"));
        let chunks = install_overlay_chunks(pipeline, ctx, Vec::new(), &mesh, &extras).unwrap();
        (chunks, mesh)
    }

    /// `emit_source_draws` -- `MapView::paint`'s per-source loop -- on a
    /// realistic frame: a TILED source above the labels (its real tile
    /// frame, fills and hairlines), a WHOLE-SOURCE source below them (its
    /// real chunks) with a highlight drawn after it. Each source's figures
    /// are exactly its own slice of the list it was drawn into, the
    /// highlight is not counted, the lists get every draw in order, and
    /// the next frame restamps and resets.
    #[test]
    fn the_paint_loop_records_each_sources_own_slice() {
        let size = 512u32;
        let cam = crate::render::Camera {
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            ..camera(LON, LAT, 14.5)
        };
        let cover = crate::geo::visible_tiles((LON, LAT), 14, (f64::from(size), f64::from(size)));
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let globe = test_globe_upload(&cam);
        let mut shell = ShellOverlays::new();
        shell.declare_source_paint("tiled", &demo_polygon_paint()).unwrap();
        shell.set_source("tiled", &sample_blocks_geojson(LON, LAT, 20, 14, 0.004, 5), 14).unwrap();
        for _ in 0..20 {
            if !shell.update(&cam, &cover, Duration::from_secs(5)) {
                break;
            }
        }
        shell.prepare(&ctx, &pipeline, &cam, &globe, &cover);
        let frame = shell.frame.as_ref().expect("prepared");
        let (chunks, mesh) = whole_source_chunks(&ctx, &pipeline);
        let (_buffer, group) = pipeline.create_model_bind_group(
            &ctx,
            glam::Mat4::IDENTITY.to_cols_array_2d(),
            0.0,
            0.0,
            crate::render::GlobeParams::FLAT,
        );
        let (tiled, whole): (crate::overlay::SourceId, crate::overlay::SourceId) = ("tiled".into(), "whole".into());
        let slotted = [(&whole, false), (&tiled, true)];
        let mut ledger = SourceDrawLedger::default();
        let (mut below, mut above) = (Vec::new(), Vec::new());
        ledger.begin_frame();
        emit_source_draws(
            &mut ledger,
            slotted,
            &mut below,
            &mut above,
            |name, draws| {
                frame.push_tile_draws(name, draws);
                if name == &whole {
                    for chunk in &chunks {
                        draws.push(crate::render::TileDraw::Legacy(&chunk.buffers, &group));
                    }
                }
            },
            |name, draws| {
                // The "highlight": one more chunk, drawn but not counted.
                if name == &whole {
                    draws.push(crate::render::TileDraw::Legacy(&chunks[0].buffers, &group));
                }
            },
        );
        let w = ledger.get("whole");
        let t = ledger.get("tiled");
        assert_eq!(below.len(), chunks.len() + 1, "the whole source's chunks, then its highlight");
        assert_eq!((w.draw_calls, w.triangles), (chunks.len() as u64, mesh.indices.len() as u64 / 3), "highlight excluded");
        assert_eq!((t.draw_calls, t.triangles), (above.len() as u64, SourceDrawStats::of(&above).triangles));
        assert!(t.draw_calls > cover.len() as u64, "tiled fills AND hairlines counted: {t:?}");
        assert_eq!((w.frame, t.frame), (1, 1));
        assert!(w.drew() && t.drew());
        ledger.add_symbols(&whole, 3);
        assert_eq!(ledger.get("whole").symbols, 3);

        // Next frame, the whole source gone from the order: reset, restamped.
        let (mut below, mut above) = (Vec::new(), Vec::new());
        ledger.begin_frame();
        emit_source_draws(&mut ledger, [(&tiled, true)], &mut below, &mut above, |name, draws| frame.push_tile_draws(name, draws), |_, _| {});
        assert_eq!(ledger.get("whole"), SourceDrawStats::default(), "no stale figure survives a frame");
        assert_eq!(ledger.get("tiled").frame, 2);
        ledger.forget("tiled");
        assert_eq!(ledger.get("tiled"), SourceDrawStats::default());
    }

    /// `ShellOverlays` records its per-source figures from the very draws
    /// `push_draws` hands the shell -- one pass -- so after a frame they
    /// ARE that draw list, source by source; and a steady-state frame's
    /// `push_draws` (into the shell's reused list) allocates nothing, the
    /// recording included.
    #[test]
    fn shell_stats_are_the_emitted_draws_and_record_without_allocating() {
        let size = 512u32;
        let cam = crate::render::Camera {
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            ..camera(LON, LAT, 14.5)
        };
        let cover = crate::geo::visible_tiles((LON, LAT), 14, (f64::from(size), f64::from(size)));
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let globe = test_globe_upload(&cam);
        let mut shell = ShellOverlays::new();
        for (name, seed) in [("a", 11u64), ("b", 12)] {
            shell.declare_source_paint(name, &demo_polygon_paint()).unwrap();
            shell.set_source(name, &sample_blocks_geojson(LON, LAT, 20, 14, 0.004, seed), 14).unwrap();
        }
        for _ in 0..20 {
            if !shell.update(&cam, &cover, Duration::from_secs(5)) {
                break;
            }
        }
        shell.prepare(&ctx, &pipeline, &cam, &globe, &cover);
        let mut draws = Vec::new();
        shell.push_draws(&mut draws);
        let (a, b) = (shell.source_draw_stats("a").unwrap(), shell.source_draw_stats("b").unwrap());
        assert!(a.drew() && b.drew());
        assert_eq!(a.draw_calls + b.draw_calls, draws.len() as u64, "every emitted draw is some source's");
        let split = a.draw_calls as usize;
        assert_eq!((a.draw_calls, a.triangles), {
            let s = SourceDrawStats::of(&draws[..split]);
            (s.draw_calls, s.triangles)
        }, "source a is the list's first slice");
        assert_eq!((b.draw_calls, b.triangles), {
            let s = SourceDrawStats::of(&draws[split..]);
            (s.draw_calls, s.triangles)
        }, "source b is the rest");

        // Steady state: the same frame again into the shell's reused list.
        let before = (a.frame, b.frame);
        let capacity = draws.capacity();
        draws.clear();
        let allocations = crate::collision::alloc_counting::count();
        shell.push_draws(&mut draws);
        let allocated = crate::collision::alloc_counting::count() - allocations;
        assert_eq!(draws.capacity(), capacity);
        assert_eq!(allocated, 0, "a steady-state push_draws, recording included, allocates nothing");
        let (a2, b2) = (shell.source_draw_stats("a").unwrap(), shell.source_draw_stats("b").unwrap());
        assert_eq!((a2.frame, b2.frame), (before.0 + 1, before.1 + 1));
        assert_eq!((a2.draw_calls, b2.draw_calls), (a.draw_calls, b.draw_calls));
    }

    /// The symbol attribution `MapView::paint` runs: each labelled
    /// source's run of running sort keys maps a placed symbol back to it;
    /// a key in no run (a source without symbols between them) is ignored.
    #[test]
    fn placed_symbols_are_attributed_by_sort_key_run() {
        let (stops, buses): (crate::overlay::SourceId, crate::overlay::SourceId) = ("stops".into(), "buses".into());
        // stops: features 0..3; an unlabelled 5-feature source at 3..8;
        // buses: 8..10.
        let runs = vec![(Arc::clone(&stops), 0, 3), (Arc::clone(&buses), 8, 10)];
        let mut ledger = SourceDrawLedger::default();
        ledger.begin_frame();
        ledger.add_placed_symbols(&runs, &[0.0, 2.0, 4.0, 8.0, 9.0, 9.0, 10.0]);
        assert_eq!(ledger.get("stops").symbols, 2);
        assert_eq!(ledger.get("buses").symbols, 3, "8, 9 and 9; 10 is past the run");
        assert_eq!(ledger.get("stops").frame, 1);
        ledger.begin_frame();
        assert_eq!(ledger.get("stops"), SourceDrawStats::default(), "reset with the frame");
    }

    /// `drew()` counts a hairline-only source (draw calls, no triangles)
    /// and a label-only one (symbols, no geometry).
    #[test]
    fn drew_counts_hairlines_and_symbols() {
        let hairline = SourceDrawStats { draw_calls: 2, triangles: 0, symbols: 0, frame: 1 };
        let labels = SourceDrawStats { draw_calls: 0, triangles: 0, symbols: 4, frame: 1 };
        assert!(hairline.drew() && labels.drew());
        assert!(!SourceDrawStats { frame: 1, ..SourceDrawStats::default() }.drew());
    }

    /// A re-push when the source's cache is at its budget -- the normal
    /// state on iOS and wasm (64 MB) after a zoom session -- must not blank:
    /// over budget, the stand-in sheds its off-screen history, never the
    /// tiles that are the only picture on screen.
    #[test]
    fn a_repush_at_the_cache_budget_never_blanks() {
        let size = 512u32;
        let at = |lon: f64| crate::render::Camera {
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            ..camera(lon, LAT, 14.5)
        };
        let (cam, away) = (at(LON), at(LON + 0.06));
        let cover_of = |lon: f64| crate::geo::visible_tiles((lon, LAT), 14, (f64::from(size), f64::from(size)));
        let (cover, away_cover) = (cover_of(LON), cover_of(LON + 0.06));
        assert!(cover.iter().all(|t| !away_cover.contains(t)), "the pan leaves history off screen");
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let push = |shell: &mut ShellOverlays, seed: u64| {
            shell.declare_source_paint("net", &demo_polygon_paint()).unwrap();
            shell.set_source("net", &sample_blocks_geojson(LON, LAT, 40, 14, 0.004, seed), 14).unwrap();
        };
        let frame = |shell: &mut ShellOverlays, cam: &crate::render::Camera, cover: &[TileId], budget: Duration| {
            let more = shell.update(cam, cover, budget);
            let globe = test_globe_upload(cam);
            shell.prepare(&ctx, &pipeline, cam, &globe, cover);
            let mut draws = Vec::new();
            shell.push_draws(&mut draws);
            let px = pollster::block_on(crate::render::gpu::tests::render_tile_draws(
                &ctx,
                &pipeline,
                cam,
                size,
                wgpu::Color::BLACK,
                &draws,
            ));
            (px, more)
        };
        let settle = |shell: &mut ShellOverlays, cam: &crate::render::Camera, cover: &[TileId]| {
            for _ in 0..20 {
                let (px, more) = frame(shell, cam, cover, Duration::from_secs(5));
                if !more {
                    return px;
                }
            }
            panic!("the cover bakes in 20 frames");
        };
        let painted = |px: &[u8]| px.chunks(4).filter(|p| p != &[0, 0, 0, 255]).count();

        let mut shell = ShellOverlays::new();
        push(&mut shell, 7);
        settle(&mut shell, &away, &away_cover);
        let a = settle(&mut shell, &cam, &cover);
        assert!(painted(&a) > 100_000, "the fixture covers the view");
        let held = shell.gpu.tile_gpu.get("net").unwrap();
        let (a_tiles, a_bytes) = (held.cache.len(), held.held_bytes());
        assert!(a_tiles > cover.len(), "history beyond the cover: {a_tiles} tiles");
        // The cache is AT the budget.
        shell.gpu.tile_budget = a_bytes;

        push(&mut shell, 8);
        let mut frames = 0;
        loop {
            let (px, more) = frame(&mut shell, &cam, &cover, Duration::ZERO);
            assert_eq!(painted(&px), painted(&a), "frame {frames} after a re-push at the budget: no blank tile");
            let held = shell.gpu.tile_gpu.get("net").unwrap();
            if let Some(stand_in) = held.previous.first() {
                if held.held_bytes() > 0 && !held.cache.is_empty() {
                    assert!(stand_in.cache.len() <= cover.len(), "over budget the stand-in keeps only what it draws");
                }
            }
            frames += 1;
            if !more || frames > 40 {
                break;
            }
        }
        let held = shell.gpu.tile_gpu.get("net").unwrap();
        assert!(held.previous.is_empty(), "released once the new picture covers the view");
    }

    /// Two installs before one frame: the GPU set is still keyed at the
    /// FIRST generation, so the stand-in it becomes must judge emptiness
    /// by that generation's empties, not the skipped one's.
    #[test]
    fn two_installs_before_a_frame_keep_the_drawn_generations_empties() {
        let mut shell = ShellOverlays::new();
        shell.declare_source_paint("net", &demo_polygon_paint()).unwrap();
        shell.set_source("net", &sample_blocks_geojson(LON, LAT, 4, 4, 0.004, 7), 14).unwrap();
        let snap = shell.store.source_snapshot("net").unwrap();
        let source = Arc::clone(&snap.source);
        let live = shell.store.source_live_generation("net").unwrap();
        let bracket = crate::overlay::plan::BakeBracket::Bucket(14);
        let build = || {
            OverlayHost::build_tile_set(&snap.features, &snap.properties, &snap.paint, None, Default::default(), bracket, &|| false)
                .unwrap()
                .unwrap()
        };
        let (x, y) = (TileId { z: 14, x: 1, y: 1 }, TileId { z: 14, x: 2, y: 2 });
        let mut host = OverlayHost::new(1);
        host.install_tile_set(&source, 1, bracket, build(), Arc::clone(&live), crate::timing::Instant::now());
        // Generation 1 is what the GPU set was last keyed at; its empties:
        host.tile_sets.get_mut(&source).unwrap().empty.insert(x);
        host.install_tile_set(&source, 2, bracket, build(), Arc::clone(&live), crate::timing::Instant::now());
        host.tile_sets.get_mut(&source).unwrap().empty.insert(y);
        host.install_tile_set(&source, 3, bracket, build(), Arc::clone(&live), crate::timing::Instant::now());
        let carried = host.tile_sets[&source].stand_in_empty.clone().expect("a generation change carries empties");
        assert!(carried.contains(&x) && !carried.contains(&y), "generation 1's empties, not the skipped 2's: {carried:?}");
    }

    #[test]
    fn a_removed_source_leaves_nothing_behind() {
        let mut shell = ShellOverlays::new();
        shell.load_demo(LON, LAT, 4, 4, 100, 12).expect("the demo loads");
        let cam = camera(LON, LAT, 12.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 12, (1024.0, 768.0));
        shell.update(&cam, &cover, Duration::from_millis(50));
        assert!(shell.remove_source(DEMO_POLYGON_SOURCE));
        assert!(shell.remove_source(DEMO_POINT_SOURCE));
        assert!(shell.host.tile_sets.is_empty() && shell.points.is_empty());
        let names = shell.source_names();
        assert!(!names.iter().any(|n| &**n == DEMO_POLYGON_SOURCE || &**n == DEMO_POINT_SOURCE));
    }


    /// The race map's hexagon blink, measured in pixels: two sources
    /// cross-fading (`line_fx` opacity 0.5 each, the coarse one red, the
    /// fine one green), each re-pushed every few frames as its tiles land
    /// -- two bakes a frame between them, so no new generation finishes
    /// its cover before the next push -- while the camera zooms out across two display zooms. Each
    /// source covers the whole view, so each frame must DRAW at least 95%
    /// of the view in each colour.
    ///
    /// What failed: after a display-zoom change, the old generation's
    /// tiles (one zoom finer) cover the new cover tiles only partly, a
    /// partial answer counted as a hole, `compose_cover` took no stand-in
    /// for a hole, the stand-in that drew nothing was dropped, and the
    /// source drew nothing (coverage 0.0) until its newest push baked.
    #[test]
    fn repushed_sources_keep_drawing_while_the_zoom_crosses_display_zooms() {
        let size = 512u32;
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let paint = |colour: &str| -> serde_json::Map<String, serde_json::Value> {
            serde_json::from_str(&format!(r##"{{"fill-color": "{colour}", "fill-opacity": 1.0}}"##)).unwrap()
        };
        let json = |seed: u64| sample_blocks_geojson(LON, LAT, 60, 50, 0.4, seed);
        let cam_at = |z: f64| crate::render::Camera {
            viewport_width_px: size as f32,
            viewport_height_px: size as f32,
            ..camera(LON, LAT, z)
        };
        let cover_of = |cam: &crate::render::Camera| {
            crate::geo::visible_tiles((LON, LAT), cam.zoom.floor() as u8, (f64::from(size), f64::from(size)))
        };
        // `(red share, green share)` of the frame's pixels.
        // `bakes` tile bakes this frame (a zero budget bakes one per
        // `update`), or the whole queue for `None`.
        let frame = |shell: &mut ShellOverlays, cam: &crate::render::Camera, bakes: Option<usize>| {
            let cover = cover_of(cam);
            let globe = test_globe_upload(cam);
            let more = match bakes {
                Some(n) => (0..n.max(1)).fold(false, |_, _| shell.update(cam, &cover, Duration::ZERO)),
                None => shell.update(cam, &cover, Duration::from_secs(5)),
            };
            shell.prepare(&ctx, &pipeline, cam, &globe, &cover);
            let mut draws = Vec::new();
            shell.push_draws(&mut draws);
            let px = pollster::block_on(crate::render::gpu::tests::render_tile_draws(
                &ctx,
                &pipeline,
                cam,
                size,
                wgpu::Color::BLACK,
                &draws,
            ));
            let share = |c: usize| px.chunks(4).filter(|p| p[c] > 20).count() as f64 / f64::from(size * size);
            ((share(0), share(1)), more)
        };
        let mut shell = ShellOverlays::new();
        for (name, colour, seed) in [("lo", "#ff0000", 1), ("hi", "#00ff00", 2)] {
            shell.declare_source_paint(name, &paint(colour)).unwrap();
            shell.set_source(name, &json(seed), 7).unwrap();
            shell.gpu.line_fx.insert(name.into(), SourceLineFx { trim: None, opacity: 0.5 }.uniform());
        }
        let start = cam_at(7.2);
        let mut settled = ((0.0, 0.0), true);
        for _ in 0..60 {
            settled = frame(&mut shell, &start, None);
            if !settled.1 {
                break;
            }
        }
        assert!(!settled.1, "the start view settles");
        assert!(settled.0 .0 > 0.99 && settled.0 .1 > 0.99, "each source covers the view: {:?}", settled.0);
        // Each source re-pushed every 6 frames (~100 ms at 60 Hz; the race
        // panel coalesces to one push per 250 ms while loading), two tile
        // bakes a frame shared by both: a push lands before its cover has
        // baked, every time.
        const PUSH_EVERY: usize = 6;
        const BAKES_PER_FRAME: usize = 2;
        let mut worst = (f64::INFINITY, 0usize, 0.0f64, "");
        for f in 0..60usize {
            let z = 7.2 - 1.6 * f as f64 / 60.0;
            let cam = cam_at(z);
            // Staggered, as the panel's resolutions land at their own pace.
            let display = cam.zoom.floor() as u8;
            if f % PUSH_EVERY == 0 {
                shell.set_source("lo", &json(10 + f as u64), display).unwrap();
            }
            if f % PUSH_EVERY == PUSH_EVERY / 2 {
                shell.set_source("hi", &json(1000 + f as u64), display).unwrap();
            }
            let ((red, green), _) = frame(&mut shell, &cam, Some(BAKES_PER_FRAME));
            for (share, name) in [(red, "lo"), (green, "hi")] {
                if share < worst.0 {
                    worst = (share, f, z, name);
                }
            }
        }
        let (share, f, z, name) = worst;
        assert!(share >= 0.95, "frame {f} at z{z:.3}: {name} draws {share:.3} of the view");
    }

    /// A bake a re-push superseded mid-flight is NOT a tile found empty.
    /// The push moves the source's live stamp at once, but the host's tile
    /// set stays the old generation until the new one's build installs;
    /// a bake of the old set that saw the stamp move and gave up landed
    /// as "no mesh, not cancelled" -- and the old set recorded the tile as
    /// known empty. As the stand-in for the new push, that set then
    /// answered "empty" over the whole view, beating the older picture
    /// that did draw it: the source went blank for a frame or more (the
    /// race map's hexagon blink, logged in a real window).
    #[test]
    fn a_bake_superseded_by_a_re_push_is_not_recorded_as_an_empty_tile() {
        let mut shell = ShellOverlays::new();
        shell.declare_source_paint("net", &demo_polygon_paint()).unwrap();
        shell.set_source("net", &sample_blocks_geojson(LON, LAT, 20, 14, 0.004, 1), 14).unwrap();
        let cam = camera(LON, LAT, 14.0);
        let cover = crate::geo::visible_tiles((LON, LAT), 14, (1024.0, 768.0));
        assert!(cover.len() > 2);
        // Installs the build, queues the cover, bakes one tile.
        shell.update(&cam, &cover, Duration::ZERO);
        let generation = shell.host.tile_sets["net"].generation;
        let bake = shell.host.next_bake().expect("more of the cover is queued");
        let tile = bake.tile();
        // The re-push: the live stamp moves; the set is still `generation`.
        shell.set_source("net", &sample_blocks_geojson(LON, LAT, 20, 14, 0.004, 2), 14).unwrap();
        let result = bake.run();
        assert!(result.baked.is_none(), "the bake saw the stamp move and gave up");
        shell.host.land_bake(result);
        let set = &shell.host.tile_sets["net"];
        assert_eq!(set.generation, generation, "the new build has not installed yet");
        assert!(!set.empty.contains(&tile), "a superseded bake is not an empty tile");
        assert!(!set.in_flight.contains(&tile), "and not in flight any more either");
        // Installed: the old generation's empties (what its stand-in judges
        // by) do not hold it either.
        shell.update(&cam, &cover, Duration::ZERO);
        let set = &shell.host.tile_sets["net"];
        assert_ne!(set.generation, generation);
        assert!(!set.stand_in_empty.as_ref().is_some_and(|e| e.contains(&tile)), "the stand-in's empties do not hold it");
    }
}
