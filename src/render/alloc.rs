//! Always-on allocation counters for this crate's GPU buffers, plus two
//! process-wide memory figures: the C heap the allocator says is live, and
//! the resident set size the kernel says is mapped.
//!
//! This is DIAGNOSTIC INFRASTRUCTURE, not a debug-only feature: it is
//! compiled into every build, on every profile, and there is no cargo
//! feature to switch it off. The reason is that the failures it exists to
//! catch -- a buffer recreated every frame instead of rewritten in place,
//! a mesh installed and never released -- only show up in a long-running
//! embedding application, which is exactly the build that would have had
//! the feature turned off.
//!
//! Everything here is a `Relaxed` load/store on a process-global
//! `AtomicU64`. No ordering is claimed between counters: a single
//! `allocation_stats()` call is a set of independent samples, not an atomic
//! snapshot, so two counters read in one call can come from either side of
//! a concurrent allocation. That is deliberate -- a mutex around the
//! counters would put a lock on every buffer creation to buy consistency
//! nobody reading a diagnostic gauge needs.
//!
//! # What "live" does and does not capture
//!
//! `gpu_buffer_bytes_live`/`gpu_buffers_live` count buffers this crate
//! created through [`TrackedBuffer`] and have not yet dropped. Read that
//! literally:
//!
//! - It counts what THIS crate allocates. wgpu allocates plenty on its own
//!   behalf (staging belts, the backing store for every texture, driver-side
//!   command buffers) and none of that is here. A gap between
//!   `gpu_buffer_bytes_live` and process RSS is expected, and is not by
//!   itself evidence of a leak in this crate.
//! - It is denominated in the size wgpu reports for the buffer
//!   (`wgpu::Buffer::size()`, i.e. `create_buffer_init`'s 4-byte-aligned
//!   round-up), not the physical allocation the driver made for it. A
//!   Metal heap's real footprint is larger.
//! - "Dropped" means the Rust `TrackedBuffer` was dropped. `wgpu::Buffer`
//!   is itself reference-counted, so a buffer still referenced by an
//!   in-flight command submission is counted as released here while the GPU
//!   still holds it. Over any window longer than a frame that difference
//!   washes out; within one frame it does not.
//! - A buffer created by wgpu code outside this crate, or by a
//!   `create_buffer` call that has not been routed through
//!   [`TrackedBuffer`], is invisible to every counter here. The test
//!   `every_buffer_creation_in_the_crate_goes_through_the_tracker` is what
//!   keeps that from silently becoming untrue.
//!
//! # `heap_in_use_bytes` versus `rss_bytes`
//!
//! These two are a PAIR and neither is useful alone.
//!
//! - [`heap_in_use_bytes`] is libmalloc's own accounting of bytes handed out
//!   by `malloc`/`calloc`/`realloc` and not yet freed. It falls the instant
//!   a buffer is dropped.
//! - [`rss_bytes`] is what the kernel has RESIDENT for the process: the
//!   touched part of the heap, plus the binary and every mapped library,
//!   thread stacks, GPU driver mappings, and -- crucially -- every page
//!   libmalloc has freed but not returned to the kernel.
//!
//! So: **heap flat while RSS climbs is allocator page retention, not a
//! leak.** Reading RSS alone cannot tell those two apart, and a single
//! before/after RSS pair cannot tell either of them from run-to-run
//! variance. Whoever is chasing a memory number wants both fields and more
//! than one run.
//!
//! Neither field bounds the other, and it is worth knowing why before
//! reading a surprising pair. RSS is usually much the larger, because of
//! everything in the list above that is not heap at all. But a large
//! `calloc` that is never written -- which is exactly what `vec![0u8; n]`
//! does -- is live heap the kernel has not made resident, and puts
//! `heap_in_use_bytes` ABOVE `rss_bytes`. Measured here, not assumed: a
//! 256 MiB untouched `Vec` reads as 268 MB of heap against 13 MB of RSS.
//!
//! The monotonic totals have none of those caveats: they only ever count
//! up, and `gpu_buffers_created_total` climbing while
//! `gpu_buffer_bytes_live` stays flat is the signature of buffers being
//! recreated rather than reused. That pair is the reason this module
//! exists.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static BUFFERS_CREATED: AtomicU64 = AtomicU64::new(0);
static BYTES_CREATED: AtomicU64 = AtomicU64::new(0);
static BUFFERS_LIVE: AtomicU64 = AtomicU64::new(0);
static BYTES_LIVE: AtomicU64 = AtomicU64::new(0);
static MESH_INSTALLS: AtomicU64 = AtomicU64::new(0);

/// Monotonic count of OVERLAY meshes that reached the screen -- every
/// `MapView::install_overlay_mesh` that survived its staleness check.
///
/// SEPARATE FROM [`MESH_INSTALLS`] because that one counts tile meshes
/// too, and a caller asking "how often does my animated overlay actually
/// update" cannot subtract a basemap's tile churn it does not control.
///
/// The counterpart to [`BAKES_CANCELLED`], and the pair is the diagnosis:
/// cancelled counts the bakes thrown away, this counts the ones that
/// landed. An application pushing faster than it can tessellate has an
/// `overlay_generation` that races, a `overlay_bakes_cancelled_total`
/// that climbs, and an `overlay_mesh_installs_total` that crawls -- a
/// frozen picture behind healthy frame timings.
static OVERLAY_MESH_INSTALLS: AtomicU64 = AtomicU64::new(0);
static OVERLAY_GENERATION: AtomicU64 = AtomicU64::new(0);
static TILES_RESIDENT: AtomicU64 = AtomicU64::new(0);

/// GPU-side wall clock for the two render passes, in nanoseconds, as
/// [`crate::render::gpu::PassTimer`] reads it back off the device: a sum, a
/// high-water mark, and the sample count that is the denominator.
///
/// NOT the same quantity as [`FRAME_NANOS_SUM`], and the difference is why
/// these exist. That one times `MapView::paint`, which ends at
/// `queue.submit` -- it measures how long the CPU took to BUILD a frame.
/// These measure how long the GPU took to DRAW it, and the two diverge
/// under exactly the conditions worth catching: a full-screen depth
/// writeback, a deep overdraw stack, a pitch that triples the visible
/// cover. Before these there was no GPU-side number in this project at all,
/// so every claim about GPU cost in it was arithmetic on a drawable size.
///
/// NOT behind `probe`, unlike the frame timings. That feature's charter is
/// instrumentation *unsafe to leave reachable in a shipped binary*
/// (`Cargo.toml` says so, and `DecodedTile::strip` is why); a duration read
/// off a query set is safe everywhere, costs a 256-byte buffer map per
/// frame, and only exists at all on a device that offered
/// `TIMESTAMP_QUERY`.
///
/// `GPU_PASS_SAMPLES` is not a frame count and must not be read as one.
/// Timestamps come back through an asynchronous buffer map, and a frame
/// whose predecessor's readback has not landed yet skips sampling entirely
/// (see `PassTimer`'s `reading` flag), so on a GPU-bound run this counts a
/// FRACTION of the frames drawn. It is the divisor for the sums, and what
/// separates "measured 0 ns" from "never measured".
///
/// The symbol pass gets its own divisor because that pass is opened only
/// when there is a label, icon or attribution plate to draw. A frame with
/// none writes no symbol timestamps, and folding those into one shared
/// sample count would report a mean no frame ever took.
static GPU_TILE_PASS_NANOS_SUM: AtomicU64 = AtomicU64::new(0);
static GPU_TILE_PASS_NANOS_MAX: AtomicU64 = AtomicU64::new(0);
static GPU_SYMBOL_PASS_NANOS_SUM: AtomicU64 = AtomicU64::new(0);
static GPU_SYMBOL_PASS_NANOS_MAX: AtomicU64 = AtomicU64::new(0);
static GPU_PASS_SAMPLES: AtomicU64 = AtomicU64::new(0);
static GPU_SYMBOL_PASS_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Draw calls issued: monotonically, in the frame that just ended, and in
/// the worst frame so far.
///
/// The per-frame pair is the useful one, and the monotonic total is what
/// makes it cheap. The three draw sites in this crate
/// (`TilePipeline::render`'s `draw_indexed` loop, `IconPipeline::render`,
/// `PlatePipeline::render`) only ever add to `DRAW_CALLS`, and
/// [`note_frame_draw_calls`] takes the difference once per `paint`. No
/// coordination between the pipelines and no per-site frame boundary to
/// keep in sync -- a fourth draw site added later is counted correctly by
/// doing nothing but calling [`note_draw_calls`].
///
/// Process-global and last-writer-wins across `MapView`s, like
/// [`OVERLAY_GENERATION`]: with two maps painting into one process, the
/// frame this gauge describes is whichever one called `paint` last.
///
/// Worth a number rather than an assumption because the count being LOW is
/// load-bearing here. This renderer bakes all of a tile's style layers into
/// one buffer pair and issues one `draw_indexed` per tile, where MapLibre
/// GL JS issues one per (layer, tile) -- an order of magnitude more. A
/// change that quietly turned a tile back into a per-layer draw would show
/// up here before it showed up anywhere else.
static DRAW_CALLS: AtomicU64 = AtomicU64::new(0);
static DRAW_CALLS_AT_FRAME_START: AtomicU64 = AtomicU64::new(0);
static DRAW_CALLS_LAST_FRAME: AtomicU64 = AtomicU64::new(0);
static DRAW_CALLS_MAX: AtomicU64 = AtomicU64::new(0);

/// Overlay bakes spawned and not yet finished, and the largest that count
/// has ever reached. NOT behind `probe`: these are the counters whose
/// absence is why an unbounded bake queue went undiagnosed.
///
/// `mesh_installs_total` cannot see a queue, and that is structural rather
/// than an oversight in how it was read. It counts LANDINGS -- one per
/// mesh that reached the GPU -- so a run in which every push is baked
/// promptly and a run in which forty bakes are stacked behind one another,
/// each holding its own reservation, report the SAME number of installs.
/// The quantity that distinguishes them is depth, and depth is what these
/// two hold: `BAKES_IN_FLIGHT` is the instantaneous gauge and
/// `BAKES_IN_FLIGHT_MAX` the high-water mark, which survives being sampled
/// at the wrong moment. A pusher faster than the bake it triggers is the
/// only thing in this path that grows with ELAPSED TIME rather than with
/// data size, so it is invisible to every other counter in this module.
///
/// Read them as a pair. `BAKES_IN_FLIGHT` at 0 with `BAKES_IN_FLIGHT_MAX`
/// at 12 means the queue drained before you looked, not that there was
/// never one.
static BAKES_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static BAKES_IN_FLIGHT_MAX: AtomicU64 = AtomicU64::new(0);

/// Overlay bakes abandoned because the store moved on before they could
/// finish -- at either generation check, or from inside the tessellator.
///
/// The third member of the pair above, and the one that says whether the
/// bound is doing its job or merely hiding the queue. A falling
/// `BAKES_IN_FLIGHT_MAX` is ambiguous on its own: it happens both when the
/// pusher slowed down and when bakes are being abandoned early. This
/// disambiguates. Cancellations climbing while `mesh_installs_total` keeps
/// pace with `overlay_generation` is the healthy shape -- supersession
/// detected before a mesh is built rather than after.
///
/// Deliberately NOT incremented when the tessellation panics. That is a
/// crash, and counting it here would make one read as the optimisation
/// working.
static BAKES_CANCELLED: AtomicU64 = AtomicU64::new(0);

/// Overlay bakes delivered, and the vertices they produced. See
/// [`AllocationStats::overlay_bakes_total`] for why the vertex count is
/// the load-bearing half.
static BAKES_DELIVERED: AtomicU64 = AtomicU64::new(0);
static BAKE_VERTICES: AtomicU64 = AtomicU64::new(0);
/// `PaintOp`s those bakes tessellated. See
/// [`AllocationStats::overlay_bake_ops_total`].
static BAKE_OPS: AtomicU64 = AtomicU64::new(0);

/// Zoom-bucket crossings that cost a drawn source a re-resolve and a
/// vertex rewrite. See [`AllocationStats::overlay_zoom_refreshes_total`].
static ZOOM_REFRESHES: AtomicU64 = AtomicU64::new(0);

/// Whole-tile tessellations `MapView::plan_for` ran on the FRAME thread.
/// See [`AllocationStats::paint_retessellations_total`].
///
/// NOT behind `probe`, for `draw_calls_total`'s reason: it answers a
/// question a consumer asks about their own running application ("is my
/// frame thread triangulating tiles?"), and a counter only a measurement
/// build can read cannot answer it in the build they shipped.
static PAINT_RETESSELLATIONS: AtomicU64 = AtomicU64::new(0);

/// Smallest retention bound `MapView::apply_retention_capacity` has
/// derived since the process started, and the largest. `u64::MAX` in the
/// minimum means "never derived one".
///
/// Behind `probe` because it exists to answer one measurement question --
/// whether the bound is STABLE within a zoom level or collapses inside it
/// -- and a shipped renderer has no use for it. The first version of that
/// bound was derived from the live cover, which shrinks by ~2.4x across a
/// single zoom level's fractional range (`tiles_for_zoom` scales its pixel
/// width by `2^-frac(zoom)`), so the caches were evicting on a wobble
/// nobody designed. That defect was found by arithmetic on the source; it
/// should have been findable by reading a counter, and now it is.
///
/// **Currently unwired.** `MapView::apply_retention_capacity` does not
/// exist on this branch (a prior retention design that called
/// `note_retention_bound` was reverted), so nothing ever calls it, and
/// both fields read as the STABLE-looking `(0, 0)` -- indistinguishable
/// from a real bound of exactly zero. Do not read `min == max` here as
/// "the bound is stable": on this branch it means "no bound was derived
/// at all". A real producer is a later task.
#[cfg(feature = "probe")]
static RETENTION_BOUND_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
#[cfg(feature = "probe")]
static RETENTION_BOUND_MAX: AtomicU64 = AtomicU64::new(0);

/// Monotonic count of (visible tile, frame) pairs that `paint` drew with
/// NEITHER their own GPU mesh NOR any stand-in -- no fully-covering set of
/// cached descendants and no cached ancestor. Each one is a frame in which
/// that tile's area of the map showed whatever was already on screen and
/// nothing new.
///
/// Behind `probe`, and added for one specific claim: retention is supposed
/// to change how long a tile is KEPT, never what is DRAWN, and the
/// stand-in path makes that claim false in principle -- an ancestor held
/// for a not-yet-loaded tile is by definition not in the visible set, so
/// nothing touches it per frame, so it is the LRU-coldest entry in
/// `gpu_meshes` and the first casualty of a bound that shrinks. Whether
/// that matters in practice is a measurement, and this is the counter that
/// makes it one.
#[cfg(feature = "probe")]
static VISIBLE_TILES_WITHOUT_STAND_IN: AtomicU64 = AtomicU64::new(0);

/// Monotonic sum, over frames, of ancestors a frame wanted as stand-ins
/// and did not hold. See [`note_uncovered_ancestors`].
///
/// **Currently unwired, and reads 0.** On `task2-wip` this was a
/// genuinely different quantity from `VISIBLE_TILES_WITHOUT_STAND_IN`:
/// counted once per frame over the UNCULLED visible set, not gated on
/// descendant cover, which is why it read 151-207/sweep there against
/// ~55 holes -- a demand roughly 3x the hole count, not a duplicate of
/// it. This branch has no call site left that computes that broader
/// quantity (the FrameTiles/ancestor_requests machinery it depended on
/// was part of the reverted retention design). Do NOT wire this to the
/// same call site as `note_visible_tile_without_stand_in` with a fixed
/// count of 1 -- that would make it a same-count alias of the hole
/// counter under a name that claims to measure the larger, uncached-at-
/// any-level demand, and the 151-207 figure this plan's task order rests
/// on would then look reproduced by a field that had never counted the
/// same thing. A real producer, computed the way `task2-wip`'s was, is
/// later plan work.
#[cfg(feature = "probe")]
static UNCOVERED_ANCESTORS: AtomicU64 = AtomicU64::new(0);

/// Monotonic count of tile fetches this process has ISSUED -- one per
/// `TileId` that `MapView::ensure_visible_tiles_requested` put into
/// `pending` and spawned a fetch for, whatever the request's origin
/// (visible cover, next-zoom prefetch, or coarse prefetch cover) and
/// whatever the outcome.
///
/// Counted at issue rather than at completion on purpose: this is the
/// BANDWIDTH counter, and a request that 404s or times out has still cost
/// a round trip. Paired with [`TILE_FETCH_BYTES`], which counts only what
/// actually came back, so the two together separate "how many requests"
/// from "how many bytes".
///
/// Exists because the coarse prefetch cover is the one change in the tile
/// retention port that costs network, and a cost that is assumed small is
/// not a measurement.
#[cfg(feature = "probe")]
static TILE_FETCHES: AtomicU64 = AtomicU64::new(0);

/// Monotonic sum of the bytes every SUCCESSFUL tile fetch returned, before
/// decode. See [`TILE_FETCHES`] for why the two are separate counters.
///
/// This is the compressed-on-the-wire figure as the `TileSource` handed it
/// back, so it is the closest thing this process can report to "bytes off
/// the network" without instrumenting the HTTP stack itself. It excludes
/// request/response headers and any TLS overhead.
#[cfg(feature = "probe")]
static TILE_FETCH_BYTES: AtomicU64 = AtomicU64::new(0);

/// Wall-clock nanoseconds spent inside `MapView::paint`, summed over
/// frames, with the frame count beside it and the single worst frame's
/// nanoseconds separately.
///
/// The MAX is the field that matters and the reason a mean is not
/// enough. The cost this exists to catch is a rebake landing on the four
/// frames of a zoom sweep that cross an integer level: four frames in
/// eleven thousand, invisible in any average and invisible in the tour's
/// wall-clock duration, but a dropped frame every time the user crosses
/// a zoom. A budget is a ceiling, so it is measured with a ceiling.
///
/// Timed by [`FrameTimer`], which records on drop -- `paint` has half a
/// dozen early returns (GPU init failure, an `Outdated`/`Lost`/`Occluded`
/// surface) and a frame that took one of them is still a frame.
#[cfg(feature = "probe")]
static FRAME_NANOS_SUM: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "probe")]
static FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "probe")]
static FRAME_NANOS_MAX: AtomicU64 = AtomicU64::new(0);

/// Frame durations bucketed at 1 ms, index 0 being `[0, 1)` ms and the
/// last bucket being everything at or above `FRAME_BUCKETS - 1` ms.
///
/// A histogram rather than a reservoir because it needs no allocation,
/// no lock and no ordering to be correct under concurrent writes, and
/// because 1 ms resolution is all a comparison against an 8.33 ms budget
/// needs. Its purpose is the tail: [`frame_ms_percentile`] reads a
/// percentile out of it, and a percentile is what says whether a slow
/// frame is a once-per-sweep event or the shape of every frame.
#[cfg(feature = "probe")]
const FRAME_BUCKETS: usize = 65;
#[cfg(feature = "probe")]
static FRAME_HISTOGRAM: [AtomicU64; FRAME_BUCKETS] =
    [const { AtomicU64::new(0) }; FRAME_BUCKETS];

/// Ancestor stand-ins drawn, and how far they were magnified.
///
/// The successor to [`VISIBLE_TILES_WITHOUT_STAND_IN`], added at the
/// same time as the change that saturates it. That counter asks "was
/// there anything at all to draw", and once a coarse prefetch cover is
/// resident the answer is always yes, so it reads 0 and can never move
/// again. It cannot distinguish a parent one zoom up (nearly the right
/// picture) from an ancestor four zooms up (a 16x magnified smear), and
/// that distinction is the whole remaining question.
///
/// So: one event per (visible tile, frame) pair drawn with an ANCESTOR
/// standing in for it, with `tile.z - ancestor.z` as the magnitude.
/// Lower is better and 0 is unreachable by construction (an ancestor is
/// at least one level up). A change that replaces a distance-4 stand-in
/// with a distance-1 stand-in is an improvement this can see and the
/// hole counter cannot.
///
/// [`DESCENDANT_STAND_INS`] counts the other branch, where a fully
/// covering set of finer tiles is drawn instead. Those need no distance:
/// they are already at or beyond the wanted resolution, which is why
/// `paint` prefers them.
#[cfg(feature = "probe")]
static ANCESTOR_STAND_INS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "probe")]
static ANCESTOR_STAND_IN_ZOOM_DISTANCE_SUM: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "probe")]
static ANCESTOR_STAND_IN_ZOOM_DISTANCE_MAX: AtomicU64 = AtomicU64::new(0);

/// Visible tiles drawn from a fully-covering set of cached DESCENDANTS.
/// See [`ANCESTOR_STAND_INS`].
#[cfg(feature = "probe")]
static DESCENDANT_STAND_INS: AtomicU64 = AtomicU64::new(0);

/// One sample of every counter this module keeps.
///
/// Fields are named exactly as the counters are named in the crate's
/// diagnostics contract, so the JSON this serialises to is the interface --
/// renaming a field is a breaking change to whatever is scraping it.
///
/// Not an atomic snapshot; see the module doc comment.
///
/// # `#[non_exhaustive]`, and why it is not cosmetic here
///
/// This type only ever grows: every counter added to this module adds a
/// `pub` field, and three arrived in one commit. Without the attribute each
/// of those is a BREAKING change to any consumer that destructures a sample
/// exhaustively or builds one as a struct literal -- neither of which is an
/// unreasonable thing to do to a plain data struct full of `u64`s.
///
/// `docs/versioning.md` makes the minor slot this crate's break signal, so
/// the cost of omitting this is not a compile error somebody shrugs at: it
/// is shipping a break in a PATCH release and telling the consumer, through
/// the only channel that carries that information, that there was not one.
/// Reading every field, which is all a scraper does, is unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct AllocationStats {
    /// Monotonic count of GPU buffers this crate has ever created.
    pub gpu_buffers_created_total: u64,
    /// Monotonic sum of those buffers' sizes, in bytes.
    pub gpu_buffer_bytes_created_total: u64,
    /// GPU buffers created by this crate and not yet dropped.
    pub gpu_buffers_live: u64,
    /// Sum of those buffers' sizes, in bytes. See the module doc comment
    /// for what this does and does not include.
    pub gpu_buffer_bytes_live: u64,
    /// Monotonic count of mesh installs -- every time a `TileMesh` (a map
    /// tile's or the overlay's) was uploaded into a fresh pair of GPU
    /// buffers via `TilePipeline::create_tile_buffers`. A mesh whose paint
    /// attributes were rewritten in place (`write_vertex_buffer`) is NOT an
    /// install and is deliberately not counted: the whole point of the
    /// in-place path is that it allocates nothing.
    pub mesh_installs_total: u64,
    /// Monotonic count of overlay meshes ACCEPTED for drawing -- handed to
    /// `MapView::overlay_drawn` past the staleness check. See
    /// [`OVERLAY_MESH_INSTALLS`] for why this is not a subset of
    /// `mesh_installs_total` a caller could compute, and why it must be
    /// read beside `overlay_bakes_cancelled_total`.
    ///
    /// **This is a CPU-side handoff and is NOT evidence that a device was
    /// reached.** `install_overlay_mesh` stores the mesh and returns; the GPU
    /// upload happens later, in `paint`. So this counter can equal
    /// `overlay_bakes_total` exactly while `gpu_buffers_created_total` is
    /// zero, and that combination means the overlay is baking and being
    /// accepted, not that it is on screen.
    ///
    /// It previously read "reached the screen", which a consumer reasonably
    /// took to mean a device had been reached and used to conclude their
    /// overlay was installing when the real question was elsewhere. For "did
    /// anything reach the GPU", read `gpu_buffers_created_total`: every tile
    /// mesh, every camera bind group and every model bind group goes through
    /// `TrackedBuffer`, so a drawing map cannot leave it at zero. A zero
    /// there beside a visibly drawing map does not mean nothing was created
    /// -- it means the statics being READ are not the statics being WRITTEN,
    /// i.e. two copies of this crate are linked into the binary (check with
    /// `cargo tree -d`). Every counter in this struct is a process-global
    /// atomic and is per-copy.
    pub overlay_mesh_installs_total: u64,
    /// The overlay store's generation counter -- incremented once per
    /// accepted `set_features`/`set_feature_paint`. Process-global and
    /// last-writer-wins: with more than one `MapView` alive this is
    /// whichever one most recently replaced its features, not a sum.
    pub overlay_generation: u64,
    /// Tiles whose GPU buffers are currently held in a `MapView`'s
    /// `gpu_meshes` cache. Summed across every live `MapView`.
    pub tiles_resident: u64,
    /// Overlay bakes spawned and not yet finished, right now, and the
    /// largest that has ever been. See [`BAKES_IN_FLIGHT`] for why
    /// `mesh_installs_total` cannot answer this and this pair must be read
    /// together.
    pub overlay_bakes_in_flight: u64,
    pub overlay_bakes_in_flight_max: u64,
    /// Overlay bakes abandoned because a later generation superseded them.
    /// Monotonic. See [`BAKES_CANCELLED`] for why a falling
    /// `overlay_bakes_in_flight_max` cannot be read without it.
    pub overlay_bakes_cancelled_total: u64,
    /// Overlay bakes that ran a tessellation to completion, and the
    /// vertices those tessellations produced. Monotonic, process-global.
    ///
    /// These are the pair that says whether sources are actually
    /// independent, and they are the ONLY honest way to ask. A push's
    /// synchronous cost is measurable from outside the crate, but the
    /// tessellation it spawns is not -- it happens on a blocking worker --
    /// so "does pushing `vehicles` re-tessellate `stops`" is invisible to
    /// a stopwatch around the call. It is exactly visible here: with one
    /// shared plan, every push tessellated every source's vertices, so
    /// this climbed by (vehicles + stops) per push; with sources it climbs
    /// by the pushed source's vertices alone.
    ///
    /// Counted where the mesh is finished rather than where the bake is
    /// spawned, so a cancelled bake contributes neither a count nor its
    /// partial vertices -- `overlay_bakes_cancelled_total` is where those
    /// go, and adding them here would make supersession look like work
    /// delivered.
    pub overlay_bakes_total: u64,
    pub overlay_bake_vertices_total: u64,
    /// `PaintOp`s those same bakes tessellated. Monotonic.
    ///
    /// The number DATA-DRIVEN PAINT is graded by, and the one
    /// `overlay_bake_vertices_total` beside it cannot see. Geometry is
    /// geometry: 3,140 heading-shaped vehicle markers tessellate to the same
    /// ~63,000 vertices whether they arrive as 3,140 resolved-paint features
    /// or as one data-driven feature set, so the vertex counter is FLAT
    /// across the change that matters. What collapses is the op count --
    /// 3,140 to one per distinct resolved colour -- and with it an
    /// `Arc<Geometry>`, a parts `Vec`, a lyon tessellator pass, an `OpSpan`
    /// and a 176-byte `PaintOp` per feature per push.
    ///
    /// Read it as a count of WORK AND ALLOCATIONS, not as a proxy for frame
    /// time. `overlay::plan::merged_bake_bench` measures that same 261x
    /// collapse at 0.96x on `build_plan` and 1.01x on `tessellate`: lyon's
    /// per-op overhead is small next to its per-polygon work. What the
    /// counter is for is proving the collapse HAPPENED, which nothing else
    /// can see from outside the crate.
    ///
    /// Counted beside the vertices, at the same site and on the same terms:
    /// a cancelled bake contributes neither.
    pub overlay_bake_ops_total: u64,
    /// Times a drawn overlay source's paint attributes were re-resolved and
    /// rewritten because the camera crossed an integer zoom. Monotonic.
    ///
    /// The number a zoom-independence claim is graded by, and a COUNT
    /// rather than a timing on purpose: a timing of zero work is
    /// indistinguishable from a timing of cheap work, and the claim is that
    /// a `BakeBracket::Ramp` source is never touched at all. One crossing
    /// with a zoom-independent source present must move this by zero. See
    /// `MapView::refresh_overlay_bake_zoom`.
    pub overlay_zoom_refreshes_total: u64,
    /// Whole-tile triangulations `MapView::plan_for` ran ON THE FRAME
    /// THREAD, inside `paint`. Monotonic, process-global.
    ///
    /// **The hitch counter.** `paint` calls `plan_for` once per visible
    /// tile, and `plan_for` runs a full `render::tessellate_plan_with_
    /// ranges` -- 17.9 ms for one dense z14 tile -- whenever it has no
    /// cached triangulation it can reuse. Every other tessellation in this
    /// crate happens off-thread (`bake_tile`, on the blocking pool), so a
    /// nonzero delta here is frame time the user sees as a stall, and a
    /// delta measured across a single integer-zoom crossing is the cost of
    /// that crossing. The 20-phase `paint` split that motivated this
    /// counter measured `plan_lookup` at a 0.004 ms p50 against a 99.064 ms
    /// max: a count is what says WHY, because a timing cannot distinguish
    /// one slow tile from twenty ordinary ones.
    ///
    /// **Two branches of `plan_for` feed it, and they are not the same
    /// event.** A tile whose plan is current for the display zoom but whose
    /// `mesh_cache` entry is missing is SEEDED (a first triangulation); a
    /// tile whose plan was baked for a stale zoom bucket and has no
    /// triangulation to rewrite or splice is RETESSELLATED (a second one).
    /// Both are a whole tile's lyon pass on the frame thread, which is why
    /// they share one counter -- a consumer watching for stalls cares about
    /// the wall clock, not the cause. `MapView`'s own per-outcome counters
    /// (see `PlanOutcomeCounts`) keep the split for tests.
    ///
    /// **Deliberately excludes the partial paths**, because neither costs a
    /// whole tile: a paint-only rewrite (`render::rewrite_paint_attributes`,
    /// no tessellation at all) and a splice (`render::splice_changed_ops`,
    /// which re-tessellates only the ops that changed -- counted separately
    /// by `MapView::splice_count`). Counting those here would make the
    /// mitigations already in place read as the defect they mitigate.
    ///
    /// **A zoom crossing no longer moves this counter at all.** Every
    /// stale-bucket rebake is queued for the blocking pool -- the resolve,
    /// the reuse-vs-splice-vs-tessellate decision and whatever mesh work it
    /// picks -- and the tile keeps drawing its stale bracket until the
    /// result lands, so the only whole-tile tessellation still on the frame
    /// thread is a FIRST bake for a tile that has decoded data but no
    /// cached plan. The two mitigations now run as an arriving rebake is
    /// installed rather than in `paint`, which is why they are not here
    /// either. A consumer that sees this counter climbing while the camera
    /// zooms is looking at a regression of that fix.
    pub paint_retessellations_total: u64,
    /// GPU nanoseconds in the tile pass and the symbol pass, summed and
    /// peaked over the frames that were sampled, with each pass's sample
    /// count as its own divisor. All zero on a device without
    /// `TIMESTAMP_QUERY`. See [`GPU_TILE_PASS_NANOS_SUM`] for why the
    /// sample counts are NOT frame counts.
    pub gpu_tile_pass_nanos_sum: u64,
    pub gpu_tile_pass_nanos_max: u64,
    pub gpu_symbol_pass_nanos_sum: u64,
    pub gpu_symbol_pass_nanos_max: u64,
    pub gpu_pass_samples: u64,
    pub gpu_symbol_pass_samples: u64,
    /// Draw calls: monotonic, in the last completed frame, and in the worst
    /// frame so far. See [`DRAW_CALLS`].
    ///
    /// **`draw_calls_total` is the proof-of-paint field.** `paint` calls
    /// `note_frame_draw_calls` at its END, so a NONZERO value here means
    /// `paint` actually ran and issued draw calls; a zero means it did not,
    /// however alive the process looks. Read it before believing any
    /// performance number taken from a running app.
    ///
    /// This is not hypothetical. A consumer spent a week measuring CPU
    /// percentages from `nohup`-launched runs whose window was never brought
    /// to the front, and a non-frontmost window does not paint: every figure
    /// was 10x off, and the variance looked like ambient machine load. Five
    /// other counters (`mesh_installs_total`, `tiles_resident`,
    /// `gpu_buffers_created_total`, `gpu_buffers_live`,
    /// `gpu_buffer_bytes_live`) correctly read zero throughout, and
    /// `overlay_bakes_total` did NOT -- overlay bakes run on
    /// `spawn_blocking` and fire whether or not a frame is ever drawn, so a
    /// climbing bake counter is not evidence of rendering either.
    ///
    /// A zero here with a nonzero `overlay_bakes_total` is precisely the
    /// "process is busy but nothing is on screen" signature.
    pub draw_calls_total: u64,
    pub draw_calls_last_frame: u64,
    pub draw_calls_max: u64,
    /// Process resident set size in bytes, or 0 where this build cannot
    /// read it. See [`rss_bytes`].
    pub rss_bytes: u64,
    /// Bytes the C allocator says are allocated and live right now, or 0
    /// where this build cannot read it. See [`heap_in_use_bytes`], and the
    /// module doc comment for why this must be read alongside `rss_bytes`
    /// rather than instead of it.
    pub heap_in_use_bytes: u64,
    /// Smallest and largest retention bound derived so far, or `(0, 0)`
    /// before the first frame. See [`note_retention_bound`]. `probe` only.
    #[cfg(feature = "probe")]
    pub retention_bound_min: u64,
    #[cfg(feature = "probe")]
    pub retention_bound_max: u64,
    /// See [`note_visible_tile_without_stand_in`]. `probe` only.
    #[cfg(feature = "probe")]
    pub visible_tiles_without_stand_in_total: u64,
    /// See [`note_uncovered_ancestors`]. `probe` only.
    #[cfg(feature = "probe")]
    pub uncovered_ancestors_total: u64,
    /// See [`note_tile_fetch_issued`]. `probe` only.
    #[cfg(feature = "probe")]
    pub tile_fetches_total: u64,
    /// See [`note_tile_fetch_bytes`]. `probe` only.
    #[cfg(feature = "probe")]
    pub tile_fetch_bytes_total: u64,
    /// Frames timed, total time in them, and the worst single one, since
    /// the last [`reset_frame_stats`]. See [`FRAME_NANOS_SUM`]. `probe`
    /// only.
    #[cfg(feature = "probe")]
    pub frame_count: u64,
    #[cfg(feature = "probe")]
    pub frame_nanos_sum: u64,
    #[cfg(feature = "probe")]
    pub frame_nanos_max: u64,
    /// The 99th and 50th percentile frame, in whole milliseconds, read
    /// out of [`FRAME_HISTOGRAM`]. 1 ms resolution -- see that static.
    #[cfg(feature = "probe")]
    pub frame_ms_p99: u64,
    #[cfg(feature = "probe")]
    pub frame_ms_p50: u64,
    /// See [`ANCESTOR_STAND_INS`]. `probe` only.
    #[cfg(feature = "probe")]
    pub ancestor_stand_ins_total: u64,
    #[cfg(feature = "probe")]
    pub ancestor_stand_in_zoom_distance_sum: u64,
    #[cfg(feature = "probe")]
    pub ancestor_stand_in_zoom_distance_max: u64,
    /// See [`DESCENDANT_STAND_INS`]. `probe` only.
    #[cfg(feature = "probe")]
    pub descendant_stand_ins_total: u64,
}

/// Samples every counter. Cheap enough to call per frame (eight relaxed
/// atomic loads and one `proc_pidinfo` syscall), though nothing in this
/// crate does.
#[must_use]
pub fn allocation_stats() -> AllocationStats {
    AllocationStats {
        gpu_buffers_created_total: BUFFERS_CREATED.load(Relaxed),
        gpu_buffer_bytes_created_total: BYTES_CREATED.load(Relaxed),
        gpu_buffers_live: BUFFERS_LIVE.load(Relaxed),
        gpu_buffer_bytes_live: BYTES_LIVE.load(Relaxed),
        mesh_installs_total: MESH_INSTALLS.load(Relaxed),
        overlay_mesh_installs_total: OVERLAY_MESH_INSTALLS.load(Relaxed),
        overlay_generation: OVERLAY_GENERATION.load(Relaxed),
        tiles_resident: TILES_RESIDENT.load(Relaxed),
        overlay_bakes_in_flight: BAKES_IN_FLIGHT.load(Relaxed),
        overlay_bakes_in_flight_max: BAKES_IN_FLIGHT_MAX.load(Relaxed),
        overlay_bakes_cancelled_total: BAKES_CANCELLED.load(Relaxed),
        overlay_bakes_total: BAKES_DELIVERED.load(Relaxed),
        overlay_bake_vertices_total: BAKE_VERTICES.load(Relaxed),
        overlay_bake_ops_total: BAKE_OPS.load(Relaxed),
        overlay_zoom_refreshes_total: ZOOM_REFRESHES.load(Relaxed),
        paint_retessellations_total: PAINT_RETESSELLATIONS.load(Relaxed),
        gpu_tile_pass_nanos_sum: GPU_TILE_PASS_NANOS_SUM.load(Relaxed),
        gpu_tile_pass_nanos_max: GPU_TILE_PASS_NANOS_MAX.load(Relaxed),
        gpu_symbol_pass_nanos_sum: GPU_SYMBOL_PASS_NANOS_SUM.load(Relaxed),
        gpu_symbol_pass_nanos_max: GPU_SYMBOL_PASS_NANOS_MAX.load(Relaxed),
        gpu_pass_samples: GPU_PASS_SAMPLES.load(Relaxed),
        gpu_symbol_pass_samples: GPU_SYMBOL_PASS_SAMPLES.load(Relaxed),
        draw_calls_total: DRAW_CALLS.load(Relaxed),
        draw_calls_last_frame: DRAW_CALLS_LAST_FRAME.load(Relaxed),
        draw_calls_max: DRAW_CALLS_MAX.load(Relaxed),
        rss_bytes: rss_bytes(),
        heap_in_use_bytes: heap_in_use_bytes(),
        #[cfg(feature = "probe")]
        retention_bound_min: match RETENTION_BOUND_MIN.load(Relaxed) {
            u64::MAX => 0,
            seen => seen,
        },
        #[cfg(feature = "probe")]
        retention_bound_max: RETENTION_BOUND_MAX.load(Relaxed),
        #[cfg(feature = "probe")]
        visible_tiles_without_stand_in_total: VISIBLE_TILES_WITHOUT_STAND_IN.load(Relaxed),
        #[cfg(feature = "probe")]
        uncovered_ancestors_total: UNCOVERED_ANCESTORS.load(Relaxed),
        #[cfg(feature = "probe")]
        tile_fetches_total: TILE_FETCHES.load(Relaxed),
        #[cfg(feature = "probe")]
        tile_fetch_bytes_total: TILE_FETCH_BYTES.load(Relaxed),
        #[cfg(feature = "probe")]
        frame_count: FRAME_COUNT.load(Relaxed),
        #[cfg(feature = "probe")]
        frame_nanos_sum: FRAME_NANOS_SUM.load(Relaxed),
        #[cfg(feature = "probe")]
        frame_nanos_max: FRAME_NANOS_MAX.load(Relaxed),
        #[cfg(feature = "probe")]
        frame_ms_p99: frame_ms_percentile(99.0),
        #[cfg(feature = "probe")]
        frame_ms_p50: frame_ms_percentile(50.0),
        #[cfg(feature = "probe")]
        ancestor_stand_ins_total: ANCESTOR_STAND_INS.load(Relaxed),
        #[cfg(feature = "probe")]
        ancestor_stand_in_zoom_distance_sum: ANCESTOR_STAND_IN_ZOOM_DISTANCE_SUM.load(Relaxed),
        #[cfg(feature = "probe")]
        ancestor_stand_in_zoom_distance_max: ANCESTOR_STAND_IN_ZOOM_DISTANCE_MAX.load(Relaxed),
        #[cfg(feature = "probe")]
        descendant_stand_ins_total: DESCENDANT_STAND_INS.load(Relaxed),
    }
}

/// Records one sampled frame's GPU pass durations, in nanoseconds. Called
/// from `render::gpu::PassTimer`'s `map_async` callback -- so on wgpu's
/// polling thread, not the paint thread, and describing a frame that was
/// presented some frames ago. See [`GPU_TILE_PASS_NANOS_SUM`].
///
/// `symbol_nanos` is `None` for a frame that drew no labels, icons or
/// attribution plate: that pass was never opened, so its query slots hold
/// whatever the previous frame left there. A `None` moves neither the
/// symbol sums nor the symbol sample count.
pub(crate) fn note_gpu_pass_nanos(tile_nanos: u64, symbol_nanos: Option<u64>) {
    GPU_PASS_SAMPLES.fetch_add(1, Relaxed);
    GPU_TILE_PASS_NANOS_SUM.fetch_add(tile_nanos, Relaxed);
    GPU_TILE_PASS_NANOS_MAX.fetch_max(tile_nanos, Relaxed);
    if let Some(symbol_nanos) = symbol_nanos {
        GPU_SYMBOL_PASS_SAMPLES.fetch_add(1, Relaxed);
        GPU_SYMBOL_PASS_NANOS_SUM.fetch_add(symbol_nanos, Relaxed);
        GPU_SYMBOL_PASS_NANOS_MAX.fetch_max(symbol_nanos, Relaxed);
    }
    #[cfg(feature = "probe")]
    crate::render::zoom_probe::note_gpu(tile_nanos);
}

/// Records draw calls issued by one pipeline's render pass. Additive and
/// unsynchronised; [`note_frame_draw_calls`] is what turns the running
/// total into a per-frame figure. See [`DRAW_CALLS`].
pub(crate) fn note_draw_calls(count: u64) {
    DRAW_CALLS.fetch_add(count, Relaxed);
}

/// Closes the frame's draw-call accounting. Called once per `MapView::paint`,
/// after the last pass has been encoded.
///
/// A `swap` rather than a load-then-store so the previous frame's mark and
/// the new one change together: two `MapView`s painting concurrently then
/// split the draws between their two frames instead of both reporting the
/// sum since whichever mark was read last.
pub(crate) fn note_frame_draw_calls() {
    let total = DRAW_CALLS.load(Relaxed);
    let this_frame = total.saturating_sub(DRAW_CALLS_AT_FRAME_START.swap(total, Relaxed));
    DRAW_CALLS_LAST_FRAME.store(this_frame, Relaxed);
    DRAW_CALLS_MAX.fetch_max(this_frame, Relaxed);
}

/// Records one frame's duration inside `MapView::paint`. Called only by
/// [`FrameTimer`]'s `Drop`, so that a frame taken by one of `paint`'s
/// early returns is counted like any other.
#[cfg(feature = "probe")]
pub fn note_frame_nanos(nanos: u64) {
    FRAME_COUNT.fetch_add(1, Relaxed);
    FRAME_NANOS_SUM.fetch_add(nanos, Relaxed);
    FRAME_NANOS_MAX.fetch_max(nanos, Relaxed);
    let bucket = (nanos / 1_000_000).min(FRAME_BUCKETS as u64 - 1) as usize;
    FRAME_HISTOGRAM[bucket].fetch_add(1, Relaxed);
    crate::render::zoom_probe::frame_end(nanos);
}

/// Times one call to `MapView::paint` and records it on drop.
///
/// A guard rather than a pair of calls because `paint` returns early in
/// at least four places (no GPU, an `Outdated`/`Lost` surface, a
/// `Timeout`/`Occluded`/`Validation` texture, no live `gpu`), and a
/// hand-placed second call would miss exactly the frames that went
/// wrong.
#[cfg(feature = "probe")]
pub struct FrameTimer(crate::timing::Instant);

#[cfg(feature = "probe")]
impl Default for FrameTimer {
    fn default() -> Self {
        Self(crate::timing::Instant::now())
    }
}

#[cfg(feature = "probe")]
impl Drop for FrameTimer {
    fn drop(&mut self) {
        let nanos = self.0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        note_frame_nanos(nanos);
        let acquire = SURFACE_ACQUIRE_NANOS.swap(0, Relaxed);
        if let Ok(mut log) = FRAME_LOG.lock() {
            if let Some(log) = log.as_mut() {
                log.push(FrameSample { ended_at: crate::timing::Instant::now(), paint_nanos: nanos, acquire_nanos: acquire });
            }
        }
    }
}

/// One `MapView::paint`, as [`frame_log_take`] reports it: when it ended,
/// how long the whole paint took, and how much of that was spent inside
/// `Surface::get_current_texture` (0 when the paint returned before
/// asking for a drawable, or skipped the GPU pass).
#[cfg(feature = "probe")]
#[derive(Clone, Copy, Debug)]
pub struct FrameSample {
    pub ended_at: crate::timing::Instant,
    pub paint_nanos: u64,
    pub acquire_nanos: u64,
}

#[cfg(feature = "probe")]
static FRAME_LOG: std::sync::Mutex<Option<Vec<FrameSample>>> = std::sync::Mutex::new(None);
#[cfg(feature = "probe")]
static SURFACE_ACQUIRE_NANOS: AtomicU64 = AtomicU64::new(0);

/// Starts (or restarts) the per-paint log [`frame_log_take`] drains.
#[cfg(feature = "probe")]
pub fn frame_log_start() {
    if let Ok(mut log) = FRAME_LOG.lock() {
        *log = Some(Vec::new());
    }
}

/// Every paint since [`frame_log_start`], and stops the log.
#[cfg(feature = "probe")]
pub fn frame_log_take() -> Vec<FrameSample> {
    FRAME_LOG.lock().ok().and_then(|mut log| log.take()).unwrap_or_default()
}

/// Time the current paint spent waiting for a drawable.
#[cfg(feature = "probe")]
pub(crate) fn note_surface_acquire_nanos(nanos: u64) {
    SURFACE_ACQUIRE_NANOS.fetch_add(nanos, Relaxed);
}

/// The whole-millisecond frame duration at `percentile`, read out of
/// [`FRAME_HISTOGRAM`]; 0 when no frame has been recorded.
///
/// Returns the LOWER edge of the bucket the percentile falls in, so a
/// reported 8 means "at least 8 ms, under 9". Erring low is deliberate:
/// this figure is read against a budget, and a tail estimate that
/// rounded up would manufacture violations.
#[cfg(feature = "probe")]
#[must_use]
pub fn frame_ms_percentile(percentile: f64) -> u64 {
    let counts: Vec<u64> = FRAME_HISTOGRAM.iter().map(|b| b.load(Relaxed)).collect();
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0;
    }
    // `ceil` so p99 of 100 frames is the 99th frame, not the 99th
    // boundary -- a percentile that can be satisfied by one frame fewer
    // than it names is not the percentile it claims to be.
    let target = ((percentile / 100.0) * total as f64).ceil() as u64;
    let mut seen = 0u64;
    for (ms, count) in counts.iter().enumerate() {
        seen += count;
        if seen >= target {
            return ms as u64;
        }
    }
    (FRAME_BUCKETS - 1) as u64
}

/// Zeroes the frame timings and the stand-in distance maxima.
///
/// Exists because a MAXIMUM cannot be turned into a delta. The hole and
/// fetch counters are monotonic sums, so the probe isolates the tour by
/// snapshotting them at tour start and subtracting; subtracting two
/// readings of a running maximum tells you nothing unless the tour's own
/// worst happened to exceed startup's. The probe therefore reads these
/// once for the pre-tour settle, resets, and reads them again for the
/// tour -- which needs this.
///
/// `probe` only, and safe to call at any point: nothing in this crate
/// reads these counters, and a concurrent `note_frame_nanos` racing a
/// reset can only lose or double-count a single frame.
#[cfg(feature = "probe")]
pub fn reset_frame_stats() {
    FRAME_COUNT.store(0, Relaxed);
    FRAME_NANOS_SUM.store(0, Relaxed);
    FRAME_NANOS_MAX.store(0, Relaxed);
    for bucket in FRAME_HISTOGRAM.iter() {
        bucket.store(0, Relaxed);
    }
    ANCESTOR_STAND_INS.store(0, Relaxed);
    ANCESTOR_STAND_IN_ZOOM_DISTANCE_SUM.store(0, Relaxed);
    ANCESTOR_STAND_IN_ZOOM_DISTANCE_MAX.store(0, Relaxed);
    DESCENDANT_STAND_INS.store(0, Relaxed);
    // The GPU pass window, for the same reason: a tour's worst pass cannot
    // be recovered by subtracting two readings of a running maximum.
    GPU_TILE_PASS_NANOS_SUM.store(0, Relaxed);
    GPU_TILE_PASS_NANOS_MAX.store(0, Relaxed);
    GPU_SYMBOL_PASS_NANOS_SUM.store(0, Relaxed);
    GPU_SYMBOL_PASS_NANOS_MAX.store(0, Relaxed);
    GPU_PASS_SAMPLES.store(0, Relaxed);
    GPU_SYMBOL_PASS_SAMPLES.store(0, Relaxed);
    // The maximum only. `DRAW_CALLS` is a monotonic total the caller can
    // difference for itself, and zeroing it here would leave
    // `DRAW_CALLS_AT_FRAME_START` above it and cost the next frame its
    // count.
    DRAW_CALLS_MAX.store(0, Relaxed);
    // The phase split apportions the frame total this just zeroed, so the
    // two have to cover the same frames. See `reset_paint_phases`.
    reset_paint_phases();
}

// ---------------------------------------------------------------------------
// The paint-phase split
// ---------------------------------------------------------------------------
//
// `FRAME_NANOS_SUM` above says how long a frame took. It cannot say WHERE
// the time went, and that gap has already cost real work: a previous plan
// spent a whole task restructuring a data structure that a phase split
// later showed to be 0.2% of its pass. Everything below exists to make a
// frame's cost ATTRIBUTABLE, one region of `MapView::paint` at a time.
//
// `probe` only, like the frame timer, and for the same reason: this is
// measurement apparatus, not a shipped gauge. Each recorded phase costs
// two `Instant::now()` calls and three relaxed atomics, twenty times a
// frame; that is cheap enough to measure with and still not something a
// consumer's release build should be paying.

/// One measured region of `MapView::paint`, in the order `paint` runs them.
///
/// These are the phases the function ACTUALLY has, read off the source
/// rather than guessed at -- see `docs`/the task report for the line ranges
/// each one spanned when it was written. The names describe what the code
/// in the region does, so a phase that stops existing should be deleted
/// here rather than quietly re-pointed at something else.
///
/// The regions are contiguous by construction: [`PhaseLap`] restarts its
/// clock at every boundary, so the gaps between them (comments, a `let` of
/// a `Vec::new()`) land in the phase that follows. That is what makes the
/// twenty figures SUM to the span from the first lap to the last, which
/// is the property attribution needs and a set of independently-timed
/// regions would not have.
#[cfg(feature = "probe")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaintPhase {
    /// Window title, then the five `window.on_mouse_event`/key handler
    /// registration blocks. Closure construction, no map work.
    Events,
    /// `ensure_gpu` + `configure_surface_if_needed`.
    GpuSetup,
    /// `flush_pending_zoom_input`, the zoom/orientation eases, the pan
    /// glide, and `refresh_overlay_bake_zoom`.
    Animation,
    /// `visible_tiles`, `display_zoom`, `coarse_prefetch_tiles`, the
    /// `covers_to_request` fetch issuing, and `upload_set`.
    ///
    /// On the globe this is where the COVER DESCENT lands -- `GlobeCover`'s
    /// depth-first walk from the z0 root, once per cover. That is the right
    /// phase for it: it is tile selection, and the flat bbox cover it
    /// replaces was always charged here. Measured in release, median of five
    /// runs of a thousand: the flat bbox cover is 0.0002 ms, a globe cover is
    /// 0.0006 ms at z2 and 0.0054 ms at the worst camera in the band (52
    /// tiles, lon -120, lat 80, 60 degrees of pitch, z5) -- so at most
    /// +0.0052 ms, about 100x under this plan's 0.5 ms rule for a globe
    /// phase.
    VisibleTiles,
    /// The `plans` map: `plan_for` over the upload set, which is where a
    /// lazy rebake and its tessellation land.
    PlanLookup,
    /// The `visible_symbol_plans` map: `symbol_plan_for` over `visible`.
    SymbolPlan,
    /// The globe's camera-level maths: the ONE `render::globe::globe_frame`
    /// call of the frame, the `GlobeUpload` narrowing that hands its `f64`
    /// matrix and plane to the uniform as `f32`, and the `GlobeCover`
    /// narrowing that hands the same plane and matrix to tile selection as
    /// five frustum half-spaces and a pair of plane angles.
    ///
    /// All three are camera-level, which is why they share a phase. The
    /// cover's own per-TILE work -- the descent -- is charged to
    /// [`Self::VisibleTiles`] beside the flat selection it replaces.
    ///
    /// **This phase exists to be small, and its size is the claim.** The
    /// reference recomputes both projections' matrices per tile per frame
    /// (`globe_transform.ts:284-295`, and `map.ts:4352` re-running
    /// `_calcMatrices` on both sub-transforms unconditionally); we do it once
    /// and leave `tile_mercator_coords` -- four divisions, no matrix -- as the
    /// only per-tile work, where it is charged to [`Self::MeshUpload`] and
    /// [`Self::TileDrawList`] alongside the `combined_tile_matrix` it rides
    /// with. So this figure does NOT grow with the visible tile count, and a
    /// reading that does is the divergence having been undone. See
    /// `render::globe`'s module docs.
    GlobeTransform,
    /// The `gpu_meshes` borrow and the per-tile `create_tile_buffers` /
    /// `create_model_bind_group` / paint-dirty rewrite loop.
    MeshUpload,
    /// The `gpu` borrow, `get_current_texture`, the colour/depth views, the
    /// camera uniform write, and -- on a globe frame only -- the space
    /// backdrop's setup: the clear colour, the ocean sphere's mesh (rebuilt
    /// only when the background colour changes) and uniform, the star
    /// field's sky uniform, and the atmosphere's.
    ///
    /// Tasks 8 and 9 deliberately added no phase of their own for any of
    /// that. On a flat frame it is two `f32` comparisons; on a globe frame
    /// it is one `space_clear_color`, one 192-byte model-uniform memcpy,
    /// one 80-byte sky-uniform memcpy and one 112-byte atmosphere-uniform
    /// memcpy behind a single `DMat4::inverse`, all once per frame rather
    /// than per tile -- measured at 0.0236 ms release by
    /// `the_space_setup_costs_nothing_a_flat_frame_can_notice`, against
    /// this plan's 0.5 ms ceiling for a globe phase.
    ///
    /// **What this phase does NOT cover is the atmosphere's GPU cost**, and
    /// no phase could: `paint` ends at `queue.submit`, and a full-screen
    /// 5x3-step scattering march is entirely on the other side of that.
    /// `the_full_screen_atmosphere_costs_less_than_the_plans_half_millisecond`
    /// measures it instead, by A/B over whole submitted frames: 0.105 ms at
    /// z3 and 0.170 ms at z5.8, in a 1024x768 viewport.
    Surface,
    /// Frustum cull, the two stand-in passes, the debug-status record, and
    /// the per-tile model-matrix rewrite loop.
    TileDrawList,
    /// The overlay's per-source GPU buffers and matrices, plus the `draws`
    /// list the tile pass is handed.
    OverlayUpload,
    /// Background colour, the command encoder, and `TilePipeline::render` --
    /// CPU-side ENCODING of the tile pass, not its GPU time (that is
    /// `GPU_TILE_PASS_NANOS_SUM`).
    TileEncode,
    /// The glyphon viewport update, the `SymbolFrame` setup, and
    /// `symbol_pass::collect_candidates` over the basemap's symbol plans.
    SymbolCollect,
    /// The overlay's own symbol candidates: one flattened pass over every
    /// drawn source's labelled features.
    OverlaySymbols,
    /// `CollisionIndex::reset`, `place_visible_symbols`, the sort, the
    /// behind-icon label clip, and the split into text/icon lists.
    Placement,
    /// The attribution credit: shape, measure, plate, text.
    Attribution,
    /// The coverage and tile-status debug overlays' vertices and labels.
    DebugOverlay,
    /// `symbol_pass::text_areas` and glyphon's `prepare`.
    TextPrepare,
    /// The symbol overlay render pass: icons, plate, debug fills, text.
    SymbolEncode,
    /// `atlas.trim`, `label_shape_cache.end_frame`, `queue.submit`, the
    /// timestamp read-back and `present`.
    SubmitPresent,
}

/// The number of [`PaintPhase`] variants. A `const` rather than a
/// `strum`-style count because it sizes the statics below.
#[cfg(feature = "probe")]
pub const PAINT_PHASE_COUNT: usize = 20;

/// Per-phase samples retained for the percentile. A sliding window over the
/// most recent frames rather than the whole run, which is the honest window
/// for this measurement: the first frames of a map are spent with tiles
/// still arriving, and a percentile that includes them describes a loading
/// screen. The cumulative sums beside it (see [`paint_phases_mean`]) keep
/// the whole-run view.
///
/// Public because it is the denominator of the missed-deadline count:
/// [`paint_phase_window_over`] hands back a numerator, and "3 frames over
/// 16.7 ms" says nothing without the 1024 it is out of.
#[cfg(feature = "probe")]
pub const PAINT_PHASE_SAMPLES: usize = 1024;

/// The last [`PAINT_PHASE_SAMPLES`] durations of each phase, in nanoseconds.
///
/// A ring of exact samples rather than a bucketed histogram like
/// [`FRAME_HISTOGRAM`], and the reason is resolution: collision placement
/// measures 0.058 ms, so every phase this is meant to separate would land
/// in bucket 0 of a 1 ms histogram and the split would read as twenty
/// zeroes. 1024 slots x 20 phases x 8 bytes is 164 KB of BSS that exists
/// only under `probe`, which is a cheaper price than a histogram fine
/// enough to see a microsecond.
#[cfg(feature = "probe")]
static PAINT_PHASE_RING: [[AtomicU64; PAINT_PHASE_SAMPLES]; PAINT_PHASE_COUNT] =
    [const { [const { AtomicU64::new(0) }; PAINT_PHASE_SAMPLES] }; PAINT_PHASE_COUNT];

/// Total writes per phase. Doubles as the ring's write cursor (`writes %
/// PAINT_PHASE_SAMPLES`) and as the sample count, so the two can never
/// disagree about how much of the ring is populated.
#[cfg(feature = "probe")]
static PAINT_PHASE_WRITES: [AtomicU64; PAINT_PHASE_COUNT] =
    [const { AtomicU64::new(0) }; PAINT_PHASE_COUNT];

/// Nanoseconds per phase summed over every frame since the last
/// [`reset_paint_phases`], with the worst single occurrence beside it. The
/// ring above forgets; these do not.
#[cfg(feature = "probe")]
static PAINT_PHASE_NANOS_SUM: [AtomicU64; PAINT_PHASE_COUNT] =
    [const { AtomicU64::new(0) }; PAINT_PHASE_COUNT];
#[cfg(feature = "probe")]
static PAINT_PHASE_NANOS_MAX: [AtomicU64; PAINT_PHASE_COUNT] =
    [const { AtomicU64::new(0) }; PAINT_PHASE_COUNT];

#[cfg(feature = "probe")]
impl PaintPhase {
    /// Every phase, in `paint` order. The array a reader iterates to print
    /// a split, and the one the anti-drift test walks to prove each variant
    /// reaches its own field of [`PaintPhases`].
    pub const ALL: [PaintPhase; PAINT_PHASE_COUNT] = [
        PaintPhase::Events,
        PaintPhase::GpuSetup,
        PaintPhase::Animation,
        PaintPhase::VisibleTiles,
        PaintPhase::PlanLookup,
        PaintPhase::SymbolPlan,
        PaintPhase::GlobeTransform,
        PaintPhase::MeshUpload,
        PaintPhase::Surface,
        PaintPhase::TileDrawList,
        PaintPhase::OverlayUpload,
        PaintPhase::TileEncode,
        PaintPhase::SymbolCollect,
        PaintPhase::OverlaySymbols,
        PaintPhase::Placement,
        PaintPhase::Attribution,
        PaintPhase::DebugOverlay,
        PaintPhase::TextPrepare,
        PaintPhase::SymbolEncode,
        PaintPhase::SubmitPresent,
    ];

    /// The phase's name as the field it lands in is spelled, minus the
    /// `_ms`. What a printed split is labelled with.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PaintPhase::Events => "events",
            PaintPhase::GpuSetup => "gpu_setup",
            PaintPhase::Animation => "animation",
            PaintPhase::VisibleTiles => "visible_tiles",
            PaintPhase::PlanLookup => "plan_lookup",
            PaintPhase::SymbolPlan => "symbol_plan",
            PaintPhase::GlobeTransform => "globe_transform",
            PaintPhase::MeshUpload => "mesh_upload",
            PaintPhase::Surface => "surface",
            PaintPhase::TileDrawList => "tile_draw_list",
            PaintPhase::OverlayUpload => "overlay_upload",
            PaintPhase::TileEncode => "tile_encode",
            PaintPhase::SymbolCollect => "symbol_collect",
            PaintPhase::OverlaySymbols => "overlay_symbols",
            PaintPhase::Placement => "placement",
            PaintPhase::Attribution => "attribution",
            PaintPhase::DebugOverlay => "debug_overlay",
            PaintPhase::TextPrepare => "text_prepare",
            PaintPhase::SymbolEncode => "symbol_encode",
            PaintPhase::SubmitPresent => "submit_present",
        }
    }
}

/// One statistic of every [`PaintPhase`], in milliseconds.
///
/// Which statistic depends on which accessor produced it --
/// [`paint_phases_mean`], [`paint_phases_p50`] or [`paint_phases_max`] --
/// and that distinction is not cosmetic. Only the MEAN is additive: the
/// twenty means sum to the mean frame, so "placement is 1.5% of paint" is
/// a statement the mean supports and the p50 does not. The p50 is for the
/// shape of a phase (is it 0.5 ms every frame, or 0 ms with a spike?), and
/// the max is for the frame that dropped.
///
/// `#[non_exhaustive]` for the same reason [`AllocationStats`] carries it:
/// this struct grows a field every time `paint` grows a phase, and
/// `docs/versioning.md` makes the minor slot this crate's break signal.
#[cfg(feature = "probe")]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct PaintPhases {
    pub events_ms: f64,
    pub gpu_setup_ms: f64,
    pub animation_ms: f64,
    pub visible_tiles_ms: f64,
    pub plan_lookup_ms: f64,
    pub symbol_plan_ms: f64,
    pub globe_transform_ms: f64,
    pub mesh_upload_ms: f64,
    pub surface_ms: f64,
    pub tile_draw_list_ms: f64,
    pub overlay_upload_ms: f64,
    pub tile_encode_ms: f64,
    pub symbol_collect_ms: f64,
    pub overlay_symbols_ms: f64,
    pub placement_ms: f64,
    pub attribution_ms: f64,
    pub debug_overlay_ms: f64,
    pub text_prepare_ms: f64,
    pub symbol_encode_ms: f64,
    pub submit_present_ms: f64,
}

#[cfg(feature = "probe")]
impl PaintPhases {
    /// The figure for one phase. The indexed view of the named fields, so a
    /// caller printing a split does not have to spell out twenty of them.
    #[must_use]
    pub fn get(&self, phase: PaintPhase) -> f64 {
        match phase {
            PaintPhase::Events => self.events_ms,
            PaintPhase::GpuSetup => self.gpu_setup_ms,
            PaintPhase::Animation => self.animation_ms,
            PaintPhase::VisibleTiles => self.visible_tiles_ms,
            PaintPhase::PlanLookup => self.plan_lookup_ms,
            PaintPhase::SymbolPlan => self.symbol_plan_ms,
            PaintPhase::GlobeTransform => self.globe_transform_ms,
            PaintPhase::MeshUpload => self.mesh_upload_ms,
            PaintPhase::Surface => self.surface_ms,
            PaintPhase::TileDrawList => self.tile_draw_list_ms,
            PaintPhase::OverlayUpload => self.overlay_upload_ms,
            PaintPhase::TileEncode => self.tile_encode_ms,
            PaintPhase::SymbolCollect => self.symbol_collect_ms,
            PaintPhase::OverlaySymbols => self.overlay_symbols_ms,
            PaintPhase::Placement => self.placement_ms,
            PaintPhase::Attribution => self.attribution_ms,
            PaintPhase::DebugOverlay => self.debug_overlay_ms,
            PaintPhase::TextPrepare => self.text_prepare_ms,
            PaintPhase::SymbolEncode => self.symbol_encode_ms,
            PaintPhase::SubmitPresent => self.submit_present_ms,
        }
    }

    /// Every phase's figure summed. Meaningful for the mean (it is the mean
    /// frame, minus whatever `paint` does outside the lapped span);
    /// meaningless as a total for the p50 and the max, where it is only
    /// useful as the denominator of a percentage.
    #[must_use]
    pub fn total_ms(&self) -> f64 {
        PaintPhase::ALL.iter().map(|&phase| self.get(phase)).sum()
    }
}

/// Records one occurrence of one phase. Called by [`PhaseLap::lap`], which
/// is the only thing that knows where the boundaries are.
#[cfg(feature = "probe")]
pub fn note_paint_phase(phase: PaintPhase, nanos: u64) {
    let index = phase as usize;
    let writes = PAINT_PHASE_WRITES[index].fetch_add(1, Relaxed);
    PAINT_PHASE_RING[index][writes as usize % PAINT_PHASE_SAMPLES].store(nanos, Relaxed);
    PAINT_PHASE_NANOS_SUM[index].fetch_add(nanos, Relaxed);
    PAINT_PHASE_NANOS_MAX[index].fetch_max(nanos, Relaxed);
    crate::render::zoom_probe::note_phase(phase.name(), nanos);
}

/// How many times a phase has been recorded since the last
/// [`reset_paint_phases`].
///
/// Not the same for every phase, and the difference is the point: `paint`
/// has half a dozen early returns, so a run where `surface` was recorded
/// far fewer times than `events` is a run that kept losing its drawable.
#[must_use]
#[cfg(feature = "probe")]
pub fn paint_phase_samples(phase: PaintPhase) -> u64 {
    PAINT_PHASE_WRITES[phase as usize].load(Relaxed)
}

/// The retained window of one phase's samples, in nanoseconds, unordered.
#[cfg(feature = "probe")]
fn paint_phase_window(phase: PaintPhase) -> Vec<u64> {
    let index = phase as usize;
    let written = PAINT_PHASE_WRITES[index].load(Relaxed) as usize;
    let retained = written.min(PAINT_PHASE_SAMPLES);
    (0..retained).map(|slot| PAINT_PHASE_RING[index][slot].load(Relaxed)).collect()
}

/// How many of a phase's retained samples took longer than `threshold_ms`.
///
/// The hitch RATE, and the figure that was missing when this was written.
/// The three statistics beside it cannot answer "how often does this
/// stall": [`paint_phases_p50`] hides a hitch by construction (`plan_lookup`
/// reads 0.004 ms), [`paint_phases_mean`] averages it away (0.380 ms), and
/// [`paint_phases_max`] is ONE sample -- a `fetch_max` held since process
/// start, so its 124.358 ms was set once between frames 420 and 480 and
/// then did not move for 1,700 frames. Every A/B on the zoom-crossing plan
/// compared one run's single worst frame to another's and decided nothing.
/// What a user feels is a count of frames that missed their deadline, which
/// is this.
///
/// Reads the same ring as [`paint_phase_window`] and adds no state of its
/// own, so the count is over the last [`PAINT_PHASE_SAMPLES`] frames and
/// forgets on the same schedule as the p50. That is deliberate: a rate is
/// only meaningful over a window, and a cumulative count would climb
/// forever and read as a regression in every later sample.
///
/// A sample exactly AT the threshold is not over it -- the comparison is in
/// milliseconds rather than nanoseconds so that 16.7 means the f64 16.7 and
/// a frame that spent precisely its 60 Hz budget is not reported as having
/// missed it.
#[must_use]
#[cfg(feature = "probe")]
pub fn paint_phase_window_over(phase: PaintPhase, threshold_ms: f64) -> u64 {
    paint_phase_window(phase)
        .into_iter()
        .filter(|&nanos| nanos as f64 / 1.0e6 > threshold_ms)
        .count() as u64
}

/// Builds a [`PaintPhases`] by asking `figure` for each phase. The one
/// place the enum is mapped onto the named fields, so the three accessors
/// below cannot disagree about which field a phase belongs in.
#[cfg(feature = "probe")]
fn paint_phases_from(figure: impl Fn(PaintPhase) -> f64) -> PaintPhases {
    PaintPhases {
        events_ms: figure(PaintPhase::Events),
        gpu_setup_ms: figure(PaintPhase::GpuSetup),
        animation_ms: figure(PaintPhase::Animation),
        visible_tiles_ms: figure(PaintPhase::VisibleTiles),
        plan_lookup_ms: figure(PaintPhase::PlanLookup),
        symbol_plan_ms: figure(PaintPhase::SymbolPlan),
        globe_transform_ms: figure(PaintPhase::GlobeTransform),
        mesh_upload_ms: figure(PaintPhase::MeshUpload),
        surface_ms: figure(PaintPhase::Surface),
        tile_draw_list_ms: figure(PaintPhase::TileDrawList),
        overlay_upload_ms: figure(PaintPhase::OverlayUpload),
        tile_encode_ms: figure(PaintPhase::TileEncode),
        symbol_collect_ms: figure(PaintPhase::SymbolCollect),
        overlay_symbols_ms: figure(PaintPhase::OverlaySymbols),
        placement_ms: figure(PaintPhase::Placement),
        attribution_ms: figure(PaintPhase::Attribution),
        debug_overlay_ms: figure(PaintPhase::DebugOverlay),
        text_prepare_ms: figure(PaintPhase::TextPrepare),
        symbol_encode_ms: figure(PaintPhase::SymbolEncode),
        submit_present_ms: figure(PaintPhase::SubmitPresent),
    }
}

/// The median duration of each phase over its retained window, in
/// milliseconds; 0.0 for a phase nothing has recorded.
///
/// An even-sized sample set is interpolated between its two middle values,
/// so the p50 of a 2 ms and a 4 ms frame is 3 ms rather than whichever of
/// the two the ordering happened to put second.
///
/// Read it for the SHAPE of a phase and not for its share of the frame:
/// percentiles do not add, so twenty p50s summing to less than the p50
/// frame is arithmetic rather than missing time. [`paint_phases_mean`] is
/// the one to apportion with.
#[must_use]
#[cfg(feature = "probe")]
pub fn paint_phases_p50() -> PaintPhases {
    paint_phases_from(|phase| {
        let mut window = paint_phase_window(phase);
        if window.is_empty() {
            return 0.0;
        }
        window.sort_unstable();
        let count = window.len();
        let nanos = if count % 2 == 1 {
            window[count / 2] as f64
        } else {
            (window[count / 2 - 1] as f64 + window[count / 2] as f64) / 2.0
        };
        nanos / 1.0e6
    })
}

/// The mean duration of each phase over every frame since the last
/// [`reset_paint_phases`], in milliseconds.
///
/// THE attribution figure: means are additive, so these twenty sum to the
/// mean lapped frame and each one's share of that total is the phase's share
/// of `paint`. A phase whose sample count differs from its neighbours' (an
/// early return between them) is averaged over its own count, which is why
/// [`paint_phase_samples`] is worth printing beside a split.
#[must_use]
#[cfg(feature = "probe")]
pub fn paint_phases_mean() -> PaintPhases {
    paint_phases_from(|phase| {
        let index = phase as usize;
        let count = PAINT_PHASE_WRITES[index].load(Relaxed);
        if count == 0 {
            return 0.0;
        }
        PAINT_PHASE_NANOS_SUM[index].load(Relaxed) as f64 / count as f64 / 1.0e6
    })
}

/// The worst single occurrence of each phase since the last
/// [`reset_paint_phases`], in milliseconds. The frame that dropped, broken
/// down -- the same argument [`FRAME_NANOS_MAX`] is kept for.
#[must_use]
#[cfg(feature = "probe")]
pub fn paint_phases_max() -> PaintPhases {
    paint_phases_from(|phase| PAINT_PHASE_NANOS_MAX[phase as usize].load(Relaxed) as f64 / 1.0e6)
}

/// The worst occurrence of each phase within the retained window, in
/// milliseconds; 0.0 for a phase nothing has recorded.
///
/// An ADDITION to [`paint_phases_max`], never a replacement: the two
/// answer different questions and a reader needs both. All-time max is
/// "did this run ever stall", which a single 124.358 ms frame satisfies for
/// the whole process; windowed max is "is it stalling NOW", which drops
/// that frame once 1024 more have gone by. A windowed max that has fallen
/// below the all-time max is not a measurement error -- it is the two
/// figures disagreeing in exactly the way that tells you the spike is
/// behind you.
///
/// Read it beside [`paint_phase_window_over`], not instead of it: a max is
/// still n=1 even over a window, so it says a hitch happened and not how
/// often.
#[must_use]
#[cfg(feature = "probe")]
pub fn paint_phases_window_max() -> PaintPhases {
    paint_phases_from(|phase| {
        paint_phase_window(phase).into_iter().max().unwrap_or(0) as f64 / 1.0e6
    })
}

/// Zeroes every phase's window, sums and maxima.
///
/// Called by [`reset_frame_stats`] as well as directly: a phase split and
/// the frame total it apportions have to cover the same frames, and
/// resetting one without the other produces a split that adds up to a
/// number nothing measured.
#[cfg(feature = "probe")]
pub fn reset_paint_phases() {
    for index in 0..PAINT_PHASE_COUNT {
        PAINT_PHASE_WRITES[index].store(0, Relaxed);
        PAINT_PHASE_NANOS_SUM[index].store(0, Relaxed);
        PAINT_PHASE_NANOS_MAX[index].store(0, Relaxed);
        for slot in PAINT_PHASE_RING[index].iter() {
            slot.store(0, Relaxed);
        }
    }
}

/// Serialises the paint-phase tests against each other.
///
/// The accumulator is process-global and each of those tests starts by
/// zeroing it, so two running in parallel -- which is cargo's default --
/// would each be reading samples the other wrote.
///
/// Lives out here rather than inside `mod tests` because the tests that
/// need it are no longer all in this file: `render::gpu`'s
/// `the_globe_transform_phase_costs_the_same_flat_or_spherical` drives a real
/// headless frame through `PhaseLap` and reads the same statics back. `paint`
/// itself is still not reachable from a test (it needs a live
/// `wgpu::Surface`), so this lock remains the whole of the required
/// coordination.
#[cfg(all(test, feature = "probe"))]
static PHASES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Takes [`PHASES`]. A poisoned lock is not a reason to fail the next test:
/// the data it guards is reset by every one of them on entry.
#[cfg(all(test, feature = "probe"))]
pub(crate) fn phase_lock() -> std::sync::MutexGuard<'static, ()> {
    PHASES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Walks `paint`'s phase boundaries, recording the span since the previous
/// boundary each time it is lapped.
///
/// A lap timer rather than twenty independent guards, and that is a
/// deliberate accounting choice: the clock restarts at each boundary, so
/// every nanosecond between the first lap and the last is charged to some
/// phase. Independent guards would leave the gaps between the regions
/// unmeasured, and "the phases sum to 60% of the frame" is a result nobody
/// can act on.
///
/// Not a `Drop` guard like [`FrameTimer`]: a phase boundary is a point in
/// the middle of a function, not the end of a scope, and `paint`'s early
/// returns should leave the remaining phases UNRECORDED rather than charged
/// a zero -- a zero would drag the mean of a phase that never ran.
#[cfg(feature = "probe")]
pub struct PhaseLap(crate::timing::Instant);

#[cfg(feature = "probe")]
impl Default for PhaseLap {
    fn default() -> Self {
        Self(crate::timing::Instant::now())
    }
}

#[cfg(feature = "probe")]
impl PhaseLap {
    /// Closes `phase` at the current instant and opens the next one.
    pub fn lap(&mut self, phase: PaintPhase) {
        let now = crate::timing::Instant::now();
        note_paint_phase(phase, now.duration_since(self.0).as_nanos().min(u128::from(u64::MAX)) as u64);
        self.0 = now;
    }
}


/// Records one visible tile drawn with an ANCESTOR standing in for it,
/// `zoom_distance` levels coarser. Called from `paint`'s pass 1, at the
/// point the ancestor is accepted. See [`ANCESTOR_STAND_INS`].
#[cfg(feature = "probe")]
pub fn note_ancestor_stand_in(zoom_distance: u8) {
    let distance = u64::from(zoom_distance);
    ANCESTOR_STAND_INS.fetch_add(1, Relaxed);
    ANCESTOR_STAND_IN_ZOOM_DISTANCE_SUM.fetch_add(distance, Relaxed);
    ANCESTOR_STAND_IN_ZOOM_DISTANCE_MAX.fetch_max(distance, Relaxed);
}

/// Records one visible tile drawn from a fully-covering set of cached
/// descendants. See [`DESCENDANT_STAND_INS`].
#[cfg(feature = "probe")]
pub fn note_descendant_stand_in() {
    DESCENDANT_STAND_INS.fetch_add(1, Relaxed);
}

/// Records one tile fetch being ISSUED. Called from
/// `MapView::ensure_visible_tiles_requested`, at the single point a
/// `TileId` is added to `pending` and a fetch task is spawned for it --
/// so every request, from every call site, is counted exactly once and a
/// tile already cached, pending, or inside its failure backoff is not.
#[cfg(feature = "probe")]
pub fn note_tile_fetch_issued() {
    TILE_FETCHES.fetch_add(1, Relaxed);
}

/// Records the bytes one successful tile fetch returned, before decode.
/// Called on the fetch worker, immediately after `TileSource::fetch`
/// resolves `Ok`.
#[cfg(feature = "probe")]
pub fn note_tile_fetch_bytes(bytes: usize) {
    TILE_FETCH_BYTES.fetch_add(bytes as u64, Relaxed);
}

/// Records one derived retention bound. Called once per frame from
/// `MapView::apply_retention_capacity`.
#[cfg(feature = "probe")]
pub fn note_retention_bound(bound: usize) {
    let bound = bound as u64;
    RETENTION_BOUND_MIN.fetch_min(bound, Relaxed);
    RETENTION_BOUND_MAX.fetch_max(bound, Relaxed);
}

/// Records how many ancestors this frame wanted and did not have.
///
/// A visible tile with no cover at all needs an ancestor that is not in
/// memory. Both reference engines would request it; we do not, because
/// requesting it unconditionally measured worse on both holes and heap
/// (see the note at the call site). This counts the demand so the
/// follow-up that adds a cache-only necessity level has a before figure.
///
/// **Not currently called from anywhere in this crate** -- see
/// [`UNCOVERED_ANCESTORS`] for why calling it from the existing fallback
/// loop, at the same point and with the same count as
/// [`note_visible_tile_without_stand_in`], would be wrong rather than
/// merely incomplete.
#[cfg(feature = "probe")]
pub fn note_uncovered_ancestors(count: usize) {
    UNCOVERED_ANCESTORS.fetch_add(count as u64, Relaxed);
}

/// Records one visible tile drawn with no mesh and no stand-in. Called
/// from `paint`'s fallback pass, at the single point where neither a
/// covering descendant set nor an ancestor was found.
#[cfg(feature = "probe")]
pub fn note_visible_tile_without_stand_in() {
    VISIBLE_TILES_WITHOUT_STAND_IN.fetch_add(1, Relaxed);
}

/// Records one mesh install. Called from
/// `TilePipeline::create_tile_buffers`, the single place a `TileMesh`
/// becomes GPU buffers.
pub(crate) fn note_mesh_install() {
    MESH_INSTALLS.fetch_add(1, Relaxed);
}

/// Records an overlay mesh reaching the screen. Called from
/// `MapView::install_overlay_mesh` PAST its staleness check, so a bake
/// that finished after a newer push is not counted here -- it is counted
/// by [`note_bake_cancelled`] instead. See [`OVERLAY_MESH_INSTALLS`].
pub(crate) fn note_overlay_mesh_install() {
    OVERLAY_MESH_INSTALLS.fetch_add(1, Relaxed);
}

/// Publishes the overlay store's current generation. Called from
/// `OverlayStore` on every increment, NOT from the paint path, so the gauge
/// still advances for an application that is pushing features with no
/// window open -- which is the shape of the leak this was built to measure.
pub(crate) fn note_overlay_generation(generation: u64) {
    OVERLAY_GENERATION.store(generation, Relaxed);
}

/// One overlay bake's presence in the queue, counted for as long as this
/// value is alive.
///
/// An RAII guard rather than a matched pair of `note_bake_started` /
/// `note_bake_finished` calls, because the thing being counted has FOUR
/// exits -- the mesh installs, the generation check cancels it at the top,
/// the tessellator cancels it between ops, or the entity is gone and
/// `update` fails -- and a counter that leaks one decrement is worse than
/// no counter at all: it reads as a permanently stuck queue and would send
/// the next reader chasing a bug that is in the instrument. `Drop` covers
/// every exit including a panic, by construction.
///
/// Held across the permit wait too, deliberately: a bake sitting behind a
/// full semaphore IS in flight for every purpose this counter serves --
/// it is retained work whose input is pinned in memory -- and a gauge that
/// only counted the bakes actually running would report the bound it
/// enforces rather than the demand it is absorbing.
pub(crate) struct BakeInFlight(());

impl BakeInFlight {
    pub(crate) fn enter() -> Self {
        // `fetch_add` returns the PREVIOUS value, so the depth including
        // this bake is one more. Two bakes entering concurrently each see
        // their own depth and each `fetch_max` it, so the larger wins
        // whichever order they land in.
        let depth = BAKES_IN_FLIGHT.fetch_add(1, Relaxed) + 1;
        BAKES_IN_FLIGHT_MAX.fetch_max(depth, Relaxed);
        Self(())
    }
}

impl Drop for BakeInFlight {
    fn drop(&mut self) {
        BAKES_IN_FLIGHT.fetch_sub(1, Relaxed);
    }
}

/// Records an overlay bake abandoned because a later generation superseded
/// it. See [`BAKES_CANCELLED`].
pub(crate) fn note_bake_cancelled() {
    BAKES_CANCELLED.fetch_add(1, Relaxed);
}

/// Records one overlay bake that finished a tessellation, and how many
/// vertices and `PaintOp`s it produced.
///
/// Called where the mesh comes back from the tessellator -- before the
/// staleness check at the door, because the work was done whether or not
/// it is kept, and a counter that only counted the ones that landed would
/// under-report exactly the run that is superseding bakes. See
/// [`AllocationStats::overlay_bakes_total`].
pub(crate) fn note_overlay_bake(vertices: usize, ops: usize) {
    BAKES_DELIVERED.fetch_add(1, Relaxed);
    BAKE_VERTICES.fetch_add(vertices as u64, Relaxed);
    BAKE_OPS.fetch_add(ops as u64, Relaxed);
}

/// Records one drawn overlay source re-resolved and rewritten for a new
/// zoom bucket. See [`AllocationStats::overlay_zoom_refreshes_total`].
pub(crate) fn note_overlay_zoom_refresh() {
    ZOOM_REFRESHES.fetch_add(1, Relaxed);
}

/// Records one whole-tile triangulation run by `MapView::plan_for` on the
/// frame thread. See [`AllocationStats::paint_retessellations_total`] for
/// which of `plan_for`'s branches call this and which deliberately do not.
pub(crate) fn note_paint_retessellation() {
    PAINT_RETESSELLATIONS.fetch_add(1, Relaxed);
}

/// Records a tile's GPU mesh becoming resident.
pub(crate) fn note_tile_resident() {
    TILES_RESIDENT.fetch_add(1, Relaxed);
}

/// Records a tile's GPU mesh being released (evicted from `gpu_meshes`, or
/// dropped with the whole `MapView`).
pub(crate) fn note_tile_released() {
    TILES_RESIDENT.fetch_sub(1, Relaxed);
}

/// The process's resident set size in bytes, or 0 if it cannot be read.
///
/// On macOS this is `proc_pidinfo(PROC_PIDTASKINFO)`'s `pti_resident_size`
/// -- the same number `ps -o rss` reports, read directly instead of by
/// sampling a subprocess. `libproc` lives in `libSystem`, which is already
/// linked into every binary on the platform, so this costs no new
/// dependency.
///
/// Returns 0 on every other platform: this crate's only real target is
/// macOS (the surface layer is `CAMetalLayer`), and a wrong number would be
/// worse than an obviously absent one.
#[must_use]
pub fn rss_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        macos_rss::rss_bytes()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// Bytes currently allocated and live on the C heap, or 0 if it cannot be
/// read.
///
/// On macOS this is `malloc_zone_statistics(NULL, ...)`'s `size_in_use`,
/// summed by libmalloc across every malloc zone in the process (passing a
/// null zone is the documented way to ask for the total). It counts bytes
/// requested through `malloc` and friends and not yet freed -- Rust's global
/// allocator is `System`, so every `Vec`, `Box`, `String` and hash map in
/// this process lands here.
///
/// It goes DOWN when memory is freed, which is the entire reason it exists:
/// [`rss_bytes`] does not, because libmalloc holds the pages. See the module
/// doc comment.
///
/// What it does NOT include: memory the process got straight from the kernel
/// without going through malloc -- `mmap`/`vm_allocate` mappings, thread
/// stacks, and in particular whatever the Metal driver maps on the GPU's
/// behalf. A gap between this and `rss_bytes` is normal and expected.
///
/// `libmalloc` is in `libSystem`, linked into every binary on the platform,
/// so this costs no new dependency. Returns 0 on every other platform, for
/// the same reason [`rss_bytes`] does.
#[must_use]
pub fn heap_in_use_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        macos_heap::heap_in_use_bytes()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// Hands the pages of FREED blocks the allocator is still caching back to
/// the system, and returns how many bytes it released (0 off macOS).
///
/// Why this exists: macOS's allocator keeps large freed blocks in a cache
/// for reuse, resident, and COUNTED in `heap_in_use_bytes`. A national
/// overlay push frees gigabytes at once (the reader's buffers, then the
/// tiled source's lon/lat copy when its index is adopted), and the cache
/// held about 2.2 GB of them: headless `--index-only` measured 3.68 GB RSS
/// after the push with the cache on and 1.45 GB with `MallocLargeCache=0`,
/// and a 566 MB `calloc` served from a cached 1.13 GB block showed in
/// `heap_in_use_bytes` as 983 MB (the Phase B / Task 1 "heap gap"). One
/// call after a big free is milliseconds, and nothing live is touched.
pub fn release_cached_free_memory() -> u64 {
    #[cfg(target_os = "macos")]
    {
        macos_heap::release_cached_free_memory()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// What one heap block of `requested` bytes actually costs.
///
/// The unit every `footprint_bytes` in this crate is denominated in, and
/// the reason those figures track [`heap_in_use_bytes`] instead of drifting
/// below it. Two things a `capacity * size_of` sum gets wrong, both of
/// which this corrects:
///
/// - **A zero-length allocation is not free.** No allocator hands out a
///   block smaller than its quantum, so every live block costs at least 16
///   bytes. This matters far more than it sounds: the dense Tokyo z14 tile
///   holds 46,804 separate coordinate `Vec`s, most of them a handful of
///   `Coord<i32>`s, and a walk that charged a 3-coordinate ring its 24
///   bytes rather than the 32 it occupies would be short by a third on the
///   commonest block in the whole cache.
/// - **Sizes are rounded up.** 16-byte granularity, measured directly
///   against `malloc_size` on this platform: every size from 1 to 4096
///   lands on `(n + 15) & !15` or above it, exactly on it for all 16
///   powers of two and for every multiple of 16 up to 128. See
///   `the_block_model_never_overstates_what_malloc_hands_out`.
///
/// # Why a model and not `malloc_size`
///
/// `malloc_size` is right here in `libSystem` and would give the exact
/// figure per block -- for the blocks whose pointer is reachable. A
/// `HashMap`'s table is not one of them, and a tile's per-feature tag maps
/// are 59% of its decoded footprint (6.99 MB of 11.81 MB on the dense
/// Tokyo tile), so an exact-where-possible/modelled-elsewhere accounting
/// would be a model anyway, in the half that matters, with a per-block FFI
/// call added to the half that does not. It would also make the unit
/// platform-dependent, and therefore make eviction differ between
/// platforms for reasons nothing in the renderer can see.
///
/// So: one model, everywhere, and the stress suite grades what it adds up
/// to against what the allocator says
/// (`memory_ceiling.decoded_accounted_ratio` and
/// `memory_ceiling.mesh_accounted_ratio` in `stress/baseline.json`). A
/// model that is checked against reality on every run is not an estimate
/// that drifts; an uncheckable exact figure for one platform would be.
///
/// # Accuracy, measured
///
/// On the real Japan z14 tiles, accounted against the allocator's
/// own in-use delta: **1.014** for the dense Tokyo tile's decoded form
/// (11.808 MB accounted, 11.641 MB held), **0.999** for its 65 MB
/// triangulation. The residual is size-class rounding above 128 bytes,
/// where this platform's allocator jumps in coarser steps than 16 (513
/// bytes is served from a 640-byte block) -- so the model UNDERSTATES a
/// medium block and never overstates one. Deliberately not modelled: those
/// classes are an implementation detail of one allocator, the error they
/// leave is under 2% on real data, and a budget is a ceiling, so being
/// within 2% of the truth from below is a rounding choice and not a
/// safety one.
#[must_use]
pub const fn heap_block_bytes(requested: usize) -> usize {
    if requested == 0 {
        // Not `16`: an absent block is absent. A `Vec` that has never
        // allocated has a dangling pointer and no heap behind it, and
        // charging it 16 bytes would put 25,264 phantom blocks on a tile
        // whose point features carry their coordinates inline.
        return 0;
    }
    let rounded = (requested + 15) & !15;
    if rounded < 16 {
        16
    } else {
        rounded
    }
}

/// [`heap_block_bytes`] for one `Vec<T>`, given its `capacity`.
///
/// Capacity and not length, always. The two are the same for a `Vec` built
/// by `collect` and are emphatically not for one grown by `push`, and it
/// is the capacity the allocator is holding -- the whole reason
/// `mvt::decode` trims its geometry (see `mvt::shrink_geometry`, which
/// found 281 allocated slots per slot used) is that the gap between them
/// was 91% of this renderer's per-tile memory.
#[must_use]
pub fn vec_block_bytes<T>(capacity: usize) -> usize {
    heap_block_bytes(capacity * std::mem::size_of::<T>())
}

/// [`heap_block_bytes`] for the table behind a `std::collections::HashMap
/// <K, V>` holding `usable` capacity.
///
/// `HashMap` exposes no accessor for its allocation, so this reproduces
/// hashbrown's layout from the one number it does expose. `capacity()`
/// reports the USABLE capacity, which hashbrown derives from a power-of-two
/// bucket count at a 7/8 load factor (`capacity_to_buckets`), so the
/// bucket count is recoverable from it: 4 buckets below 4, 8 below 8, and
/// `(usable * 8 / 7).next_power_of_two()` above. The block itself is one
/// `(K, V)` per bucket plus one control byte per bucket plus a trailing
/// group's worth of control bytes.
///
/// Measured against the heap for `HashMap<String, u64>` at 1, 4 and 8
/// entries: 160/288/544 bytes modelled against 161/321/641 held. It
/// understates, for the same size-class reason [`heap_block_bytes`] does,
/// and by the same tolerable margin -- the tag maps this is used for are
/// 2 entries on average across the dense Tokyo tile (49,970 entries over
/// 25,264 features), which is the range where the model is tightest.
///
/// Pinned to hashbrown's layout rather than measured at runtime, and
/// therefore a thing that can go stale if the standard library's map
/// changes shape. `the_hashmap_table_model_matches_the_real_allocation`
/// is what catches that: it grows a real map and compares.
#[must_use]
pub fn hashmap_table_bytes<K, V>(usable: usize) -> usize {
    if usable == 0 {
        return 0;
    }
    let buckets = if usable < 4 {
        4
    } else if usable < 8 {
        8
    } else {
        (usable * 8 / 7).next_power_of_two()
    };
    // `+ buckets` is the control byte per bucket; `+ 16` is the trailing
    // `Group::WIDTH` control bytes hashbrown appends so a group load at
    // the last bucket stays in bounds.
    heap_block_bytes(buckets * std::mem::size_of::<(K, V)>() + buckets + 16)
}

#[cfg(target_os = "macos")]
mod macos_heap {
    /// `<malloc/malloc.h>`'s `malloc_statistics_t`, field for field.
    ///
    /// Declared here rather than pulled from `libc` for the same reason
    /// `ProcTaskInfo` below is: this crate does not depend on `libc`
    /// directly and adding one for two symbols would put it in every
    /// consumer's tree. `repr(C)` reproduces the C padding exactly -- a
    /// `u32` at offset 0, then three `usize` at 8/16/24, total 32 bytes --
    /// the offsets `offsetof` reports for the real header, pinned here by
    /// `malloc_statistics_has_the_layout_the_header_declares`.
    #[repr(C)]
    #[derive(Default)]
    struct MallocStatistics {
        blocks_in_use: u32,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }

    extern "C" {
        /// A null `zone` means "every zone in the process", which is what
        /// makes one call enough.
        fn malloc_zone_statistics(
            zone: *mut core::ffi::c_void,
            stats: *mut MallocStatistics,
        );
        /// `<malloc/malloc.h>`: returns free memory the zone is caching to
        /// the system, up to `goal` bytes (0 = as much as possible). Null
        /// zone = every zone.
        fn malloc_zone_pressure_relief(zone: *mut core::ffi::c_void, goal: usize) -> usize;
    }

    pub(super) fn release_cached_free_memory() -> u64 {
        // SAFETY: a null zone is the documented "all zones" argument; the
        // call only returns cached free pages and touches no live block.
        unsafe { malloc_zone_pressure_relief(core::ptr::null_mut(), 0) as u64 }
    }

    /// The layout the C header gives for `malloc_statistics_t`, which is
    /// what `malloc_zone_statistics` writes through the pointer above. If
    /// `repr(C)` ever laid this struct out differently, `size_in_use` would
    /// be read from the wrong offset and every heap number this module
    /// reports would be silently wrong rather than obviously broken.
    #[cfg(test)]
    #[test]
    fn malloc_statistics_has_the_layout_the_header_declares() {
        assert_eq!(core::mem::size_of::<MallocStatistics>(), 32);
        assert_eq!(core::mem::offset_of!(MallocStatistics, blocks_in_use), 0);
        assert_eq!(core::mem::offset_of!(MallocStatistics, size_in_use), 8);
        assert_eq!(core::mem::offset_of!(MallocStatistics, max_size_in_use), 16);
        assert_eq!(core::mem::offset_of!(MallocStatistics, size_allocated), 24);
    }

    pub(super) fn heap_in_use_bytes() -> u64 {
        let mut stats = MallocStatistics::default();
        // SAFETY: `stats` is a live, correctly-laid-out `malloc_statistics_t`
        // and the null zone pointer is the documented "all zones" argument.
        // The call writes exactly the four fields of the struct and returns
        // nothing; there is no failure signal to check, and a zone that
        // cannot be introspected simply contributes nothing to the sum.
        unsafe { malloc_zone_statistics(core::ptr::null_mut(), &raw mut stats) };
        stats.size_in_use as u64
    }
}

#[cfg(target_os = "macos")]
mod macos_rss {
    /// `<sys/proc_info.h>`'s `struct proc_taskinfo`, field for field.
    ///
    /// Declared here rather than pulled from `libc` because this crate does
    /// not depend on `libc` directly and adding one for two symbols would
    /// put it in every consumer's tree. The layout is ABI-stable -- it is a
    /// public kernel interface that `ps`, `top` and Activity Monitor all
    /// read -- and `proc_pidinfo` validates the size argument, so a
    /// mismatch would fail the call rather than scribble.
    #[repr(C)]
    #[derive(Default)]
    struct ProcTaskInfo {
        pti_virtual_size: u64,
        pti_resident_size: u64,
        pti_total_user: u64,
        pti_total_system: u64,
        pti_threads_user: u64,
        pti_threads_system: u64,
        pti_policy: i32,
        pti_faults: i32,
        pti_pageins: i32,
        pti_cow_faults: i32,
        pti_messages_sent: i32,
        pti_messages_received: i32,
        pti_syscalls_mach: i32,
        pti_syscalls_unix: i32,
        pti_csw: i32,
        pti_threadnum: i32,
        pti_numrunning: i32,
        pti_priority: i32,
    }

    /// `PROC_PIDTASKINFO` from `<sys/proc_info.h>`.
    const PROC_PIDTASKINFO: i32 = 4;

    extern "C" {
        fn getpid() -> i32;
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut core::ffi::c_void,
            buffersize: i32,
        ) -> i32;
    }

    pub(super) fn rss_bytes() -> u64 {
        let mut info = ProcTaskInfo::default();
        let size = core::mem::size_of::<ProcTaskInfo>() as i32;
        // SAFETY: `info` is a live, correctly-sized `ProcTaskInfo`, and
        // `size` is its real size -- `proc_pidinfo` writes at most that
        // many bytes and returns how many it wrote. Any short or failed
        // write is caught by the return-value check below, which leaves
        // `info` at its zeroed default.
        let written = unsafe {
            proc_pidinfo(getpid(), PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size)
        };
        if written == size {
            info.pti_resident_size
        } else {
            0
        }
    }
}

/// A `wgpu::Buffer` whose creation and drop move the counters above.
///
/// Every `create_buffer`/`create_buffer_init` call in this crate goes
/// through [`TrackedBuffer::init`] or [`TrackedBuffer::new`]; nothing calls
/// wgpu's constructors directly. `Deref` to `wgpu::Buffer` makes that
/// substitution invisible at the ~65 use sites -- `buffer.slice(..)`,
/// `buffer.as_entire_binding()` and `&buffer` in a `&wgpu::Buffer`
/// position all still work.
///
/// Deliberately NOT `Clone`. `wgpu::Buffer` is `Clone` (it is an `Arc`
/// inside), and cloning one out from under the tracker would leave a live
/// buffer with no live `TrackedBuffer` to decrement for it -- the live
/// gauge would then under-count, silently and permanently. Making the
/// wrapper un-clonable turns that into a compile error.
pub struct TrackedBuffer {
    buffer: wgpu::Buffer,
    /// Cached at construction so `Drop` decrements by exactly what the
    /// increment added, without re-asking wgpu for a size while it is
    /// tearing the buffer down.
    size: u64,
}

impl TrackedBuffer {
    /// Wraps `wgpu::util::DeviceExt::create_buffer_init`.
    pub(crate) fn init(
        device: &wgpu::Device,
        descriptor: &wgpu::util::BufferInitDescriptor<'_>,
    ) -> Self {
        use wgpu::util::DeviceExt;
        Self::track(device.create_buffer_init(descriptor))
    }

    /// Wraps `wgpu::Device::create_buffer`.
    pub(crate) fn new(device: &wgpu::Device, descriptor: &wgpu::BufferDescriptor<'_>) -> Self {
        Self::track(device.create_buffer(descriptor))
    }

    /// The size wgpu reports for this buffer, in bytes -- what it
    /// contributed to `gpu_buffer_bytes_created_total` and contributes to
    /// `gpu_buffer_bytes_live`.
    #[must_use]
    pub fn tracked_size(&self) -> u64 {
        self.size
    }

    /// The underlying `wgpu::Buffer`, cloned, purely so a test can compare
    /// buffer IDENTITY across two calls and prove a buffer was REUSED
    /// rather than recreated -- a size comparison cannot tell those apart.
    ///
    /// `#[cfg(test)]` and no wider on purpose. Cloning a `wgpu::Buffer` out
    /// from under the tracker is precisely what this type's doc comment
    /// forbids in production code: the clone outlives the `TrackedBuffer`'s
    /// `Drop`, so the live gauge would under-count a buffer the driver is
    /// still holding. In a test the clone is dropped within the same
    /// assertion, and identity is the only thing being read off it.
    #[cfg(test)]
    pub(crate) fn handle(&self) -> wgpu::Buffer {
        self.buffer.clone()
    }

    fn track(buffer: wgpu::Buffer) -> Self {
        // `buffer.size()`, not the descriptor's, so the aligned round-up
        // `create_buffer_init` applies is what gets counted -- the number
        // wgpu will actually be holding.
        let size = buffer.size();
        BUFFERS_CREATED.fetch_add(1, Relaxed);
        BYTES_CREATED.fetch_add(size, Relaxed);
        BUFFERS_LIVE.fetch_add(1, Relaxed);
        BYTES_LIVE.fetch_add(size, Relaxed);
        Self { buffer, size }
    }
}

impl std::ops::Deref for TrackedBuffer {
    type Target = wgpu::Buffer;

    fn deref(&self) -> &wgpu::Buffer {
        &self.buffer
    }
}

impl std::fmt::Debug for TrackedBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackedBuffer").field("size", &self.size).finish()
    }
}

impl Drop for TrackedBuffer {
    fn drop(&mut self) {
        BUFFERS_LIVE.fetch_sub(1, Relaxed);
        BYTES_LIVE.fetch_sub(self.size, Relaxed);
    }
}

// ---------------------------------------------------------------------------
// The paint census: is this window drawing at all?
// ---------------------------------------------------------------------------
//
// ALWAYS ON. Not behind `probe`, unlike `FrameTimer` and the frame
// histogram above, and the difference is deliberate. Those measure how
// *expensive* a frame was, which is a question you only ask once you know
// frames are happening. These measure whether they are happening at all,
// which is the question that has to be answerable in every build -- and the
// one this project has already got wrong from the outside.
//
// The bug this exists for, in full: a headless-ish run of an example was
// captured, the capture came back black, and that was read as a rendering
// failure. It was not. The window had never been foreground, macOS never
// composited it, `Element::paint` never ran, no tile was ever fetched, and
// the screenshot was of a window that had drawn precisely nothing. The run
// had in fact already reported `frames 0`; nobody looked, because looking
// meant a screenshot and the screenshot looked like an answer. Reading one
// counter would have settled it.
//
// So this counts five things, and the differences between them are the
// diagnosis:
//
// - `paints_entered` -- `Element::paint` was called. Zero means gpui never
//   asked this view to draw: the window is occluded, minimised, behind
//   another window, or on another Space. Nothing downstream of paint
//   (tile requests included) has ever run. **This is the `frames 0` case.**
// - `paints_submitted` -- a command buffer actually reached the queue.
//   Pixels were produced.
// - `paints_skipped_no_gpu` -- paint ran but there is no `GpuState`: device
//   creation failed, or the surface has no size yet.
// - `paints_skipped_occluded` -- wgpu answered `Occluded`/`Timeout` for the
//   surface texture. The window IS being asked to draw and the compositor
//   is refusing it: covered by another window, or off-screen.
// - `paints_skipped_surface_lost` -- `Outdated`/`Lost`; the surface is being
//   reconfigured. A few of these around a resize are normal; a stream of
//   them is not.
//
// `seconds_since_last_paint` is the live half of the same question. A frame
// count that is large but static reads exactly like a healthy idle map until
// you know when the last one was.
//
// Cost: three relaxed atomic stores per frame, worst case. There is no
// build in which that is worth switching off, which is why there is no way
// to.

static PAINTS_ENTERED: AtomicU64 = AtomicU64::new(0);
static PAINTS_SUBMITTED: AtomicU64 = AtomicU64::new(0);
static PAINTS_SKIPPED_NO_GPU: AtomicU64 = AtomicU64::new(0);
static PAINTS_SKIPPED_OCCLUDED: AtomicU64 = AtomicU64::new(0);
static PAINTS_SKIPPED_SURFACE_LOST: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds since [`origin`] at which `Element::paint` was last entered;
/// `u64::MAX` for "never", which no real elapsed time can collide with
/// (it is ~584 years) and which sorts correctly as "longest ago" in the
/// one comparison anything does with it.
static LAST_PAINT_NANOS: AtomicU64 = AtomicU64::new(u64::MAX);

/// The instant every figure here is relative to, fixed at whichever comes
/// first: the first paint, or the first read of [`paint_census`].
///
/// A host that wants `uptime_s` to mean "since the process started" calls
/// [`paint_census`] (or [`note_process_start`]) early -- which is exactly
/// what the examples' agentic HTTP harness does when it binds its port,
/// before the window is even opened.
static ORIGIN: std::sync::OnceLock<crate::timing::Instant> = std::sync::OnceLock::new();

fn origin() -> crate::timing::Instant {
    *ORIGIN.get_or_init(crate::timing::Instant::now)
}

/// Fixes [`ORIGIN`] now, so `uptime_s` counts from here rather than from
/// the first paint. Idempotent; a second call is ignored.
pub fn note_process_start() {
    let _ = origin();
}

/// Why a `paint` produced no pixels. Every variant is an early return that
/// already existed in `MapView::paint`; naming them is what turns "no
/// frames" into a cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintSkip {
    /// No `GpuState`: device/surface creation has not succeeded.
    NoGpu,
    /// `wgpu::CurrentSurfaceTexture::Occluded` or `Timeout`.
    Occluded,
    /// `wgpu::CurrentSurfaceTexture::Outdated` or `Lost`.
    SurfaceLost,
}

static PAINTS_SKIPPED_UNCHANGED: AtomicU64 = AtomicU64::new(0);

/// A paint that skipped the GPU pass because the map had nothing new to
/// draw (`MapView::set_skip_unchanged_frames`).
pub(crate) fn note_paint_skipped_unchanged() {
    PAINTS_SKIPPED_UNCHANGED.fetch_add(1, Relaxed);
}

/// Paints that skipped the GPU pass because the map had nothing new to
/// draw -- the window drew for another view. Counted apart from
/// [`PaintCensus`]'s skips, which are all frames the map WANTED.
#[must_use]
pub fn paints_skipped_unchanged() -> u64 {
    PAINTS_SKIPPED_UNCHANGED.load(Relaxed)
}

/// Records that `Element::paint` was entered. Called unconditionally as the
/// first statement of `MapView::paint`.
pub(crate) fn note_paint_entered() {
    PAINTS_ENTERED.fetch_add(1, Relaxed);
    LAST_PAINT_NANOS.store(elapsed_nanos_since_origin(), Relaxed);
}

/// Records that a command buffer reached the queue.
pub(crate) fn note_paint_submitted() {
    PAINTS_SUBMITTED.fetch_add(1, Relaxed);
}

/// Records one of `paint`'s early returns.
pub(crate) fn note_paint_skipped(reason: PaintSkip) {
    match reason {
        PaintSkip::NoGpu => &PAINTS_SKIPPED_NO_GPU,
        PaintSkip::Occluded => &PAINTS_SKIPPED_OCCLUDED,
        PaintSkip::SurfaceLost => &PAINTS_SKIPPED_SURFACE_LOST,
    }
    .fetch_add(1, Relaxed);
}

fn elapsed_nanos_since_origin() -> u64 {
    origin().elapsed().as_nanos().min(u64::MAX as u128 - 1) as u64
}

/// Whether this window is drawing, and if not, how far it got before it
/// stopped. See this section's own comment for what each field rules in
/// and out.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct PaintCensus {
    /// `Element::paint` calls. **Zero is the headline diagnosis**: gpui
    /// has never asked this view to draw, so nothing downstream of paint
    /// -- including every tile request -- has ever run.
    pub paints_entered: u64,
    /// Frames that reached `queue.submit`.
    pub paints_submitted: u64,
    /// Paints that returned early with no `GpuState`.
    pub paints_skipped_no_gpu: u64,
    /// Paints the compositor refused a surface texture for.
    pub paints_skipped_occluded: u64,
    /// Paints that found the surface `Outdated`/`Lost`.
    pub paints_skipped_surface_lost: u64,
    /// Seconds since [`ORIGIN`] -- see its doc comment for what fixes it.
    pub uptime_s: f64,
    /// Seconds since `paint` was last entered, or `None` if it never was.
    /// A large `paints_entered` with a large value here is a map that
    /// *was* drawing and has stopped, which reads identically to a healthy
    /// idle map in the count alone.
    pub seconds_since_last_paint: Option<f64>,
    /// `paints_submitted / uptime_s`. The achieved frame rate over the
    /// whole run -- an average that includes every second the window spent
    /// idle, so it is a floor on what the renderer can do, never a
    /// benchmark. Compare two [`paint_census`] readings for the rate over
    /// an interval.
    pub mean_fps_since_origin: f64,
}

/// Samples the paint census. Five relaxed loads and one `Instant::elapsed`.
#[must_use]
pub fn paint_census() -> PaintCensus {
    let uptime = origin().elapsed().as_secs_f64();
    let last = LAST_PAINT_NANOS.load(Relaxed);
    let submitted = PAINTS_SUBMITTED.load(Relaxed);
    PaintCensus {
        paints_entered: PAINTS_ENTERED.load(Relaxed),
        paints_submitted: submitted,
        paints_skipped_no_gpu: PAINTS_SKIPPED_NO_GPU.load(Relaxed),
        paints_skipped_occluded: PAINTS_SKIPPED_OCCLUDED.load(Relaxed),
        paints_skipped_surface_lost: PAINTS_SKIPPED_SURFACE_LOST.load(Relaxed),
        uptime_s: uptime,
        seconds_since_last_paint: (last != u64::MAX)
            .then(|| (elapsed_nanos_since_origin().saturating_sub(last)) as f64 / 1e9),
        mean_fps_since_origin: if uptime > 0.0 { submitted as f64 / uptime } else { 0.0 },
    }
}

#[cfg(test)]
mod footprint_model_tests {
    use super::*;

    #[test]
    fn an_absent_block_costs_nothing_and_the_smallest_present_one_costs_sixteen() {
        assert_eq!(heap_block_bytes(0), 0, "a Vec that never allocated has no block behind it");
        for requested in 1..=16 {
            assert_eq!(
                heap_block_bytes(requested),
                16,
                "no allocator hands out a block below its quantum, and charging a \
                 3-coordinate ring its 24 bytes rather than the 32 it occupies is short by \
                 a third on the commonest block in the whole cache"
            );
        }
        assert_eq!(heap_block_bytes(17), 32);
        assert_eq!(heap_block_bytes(1_000_001), 1_000_016);
    }

    /// The model must never claim a block is BIGGER than the allocator
    /// actually hands out, because these figures are budget denominators:
    /// over-counting would evict early (safe, and deliberate for
    /// `PaintPlan`), but a model that over-counted a block would make
    /// every measured accounting ratio a fiction.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_block_model_never_overstates_what_malloc_hands_out() {
        extern "C" {
            fn malloc_size(ptr: *const std::ffi::c_void) -> usize;
        }
        let mut exact = 0usize;
        let mut checked = 0usize;
        let mut modelled_total = 0usize;
        let mut actual_total = 0usize;
        for requested in 1..=4096usize {
            let buffer: Vec<u8> = Vec::with_capacity(requested);
            // SAFETY: `requested >= 1`, so `with_capacity` allocated and
            // `as_ptr` is that live block's base address -- the one thing
            // `malloc_size` requires.
            let actual = unsafe { malloc_size(buffer.as_ptr().cast()) };
            let modelled = heap_block_bytes(requested);
            assert!(
                modelled <= actual,
                "heap_block_bytes({requested}) = {modelled} claims more than the \
                 allocator's {actual}"
            );
            checked += 1;
            modelled_total += modelled;
            actual_total += actual;
            if modelled == actual {
                exact += 1;
            }
            if requested <= 128 {
                assert_eq!(
                    modelled, actual,
                    "the 16-byte quantum must be EXACT through the tiny classes: \
                     heap_block_bytes({requested}) = {modelled} against {actual}. This is \
                     the range that matters most -- the dense Tokyo z14 tile holds 46,804 \
                     coordinate Vecs and most of them are a handful of Coord<i32>"
                );
            }
        }
        assert_eq!(checked, 4096);
        // Above 128 bytes this allocator jumps in coarser steps than 16
        // (513 bytes is served from a 640-byte block), so exactness there
        // is the exception rather than the rule -- 448 of 4096 sizes on
        // this platform. The property that matters is not per-size
        // exactness but that the AGGREGATE understatement stays small, so
        // that is what is asserted, measured rather than assumed.
        assert!(exact >= 128, "only {exact} of {checked} sizes modelled exactly");
        let shortfall = (actual_total - modelled_total) * 100 / actual_total;
        assert!(
            shortfall <= 12,
            "the model is {shortfall}% short of the allocator across 1..=4096 bytes; the \
             per-tile accounting ratios every cache budget was set against assume a few \
             percent, so a model this far off would make those budgets describe a heap \
             much larger than they claim"
        );
    }

    /// `hashmap_table_bytes` reproduces hashbrown's layout from
    /// `capacity()`, which is a thing that can go stale if the standard
    /// library's map changes shape. This is what catches that.
    #[test]
    fn the_hashmap_table_model_matches_the_real_allocation() {
        use std::collections::HashMap;

        // The bucket count is what the model has to recover, and the
        // published relationship is a 7/8 load factor on a power-of-two
        // table with a floor of 4 buckets. Checked through a real map's
        // own `capacity()` at every growth step it takes, rather than
        // against remembered constants.
        let mut map: HashMap<String, u64> = HashMap::new();
        let mut seen_capacities = Vec::new();
        for i in 0..64u64 {
            map.insert(format!("key{i}"), i);
            let capacity = map.capacity();
            if seen_capacities.last() != Some(&capacity) {
                seen_capacities.push(capacity);
            }
            assert!(
                capacity >= map.len(),
                "capacity must cover the entries actually held, or the model is being fed \
                 a number that does not mean what it says"
            );
            let modelled = hashmap_table_bytes::<String, u64>(capacity);
            let floor = capacity * std::mem::size_of::<(String, u64)>();
            assert!(
                modelled > floor,
                "a table of {capacity} usable slots must cost more than {floor} bytes -- the \
                 control bytes and the power-of-two round-up are not rounding terms"
            );
            assert!(
                modelled <= 4 * floor + 16,
                "modelled {modelled} for {capacity} slots is more than four times the \
                 payload, which no load factor explains"
            );
        }
        assert!(
            seen_capacities.len() >= 4,
            "the map never grew, so nothing about the growth model was exercised"
        );
        assert_eq!(hashmap_table_bytes::<String, u64>(0), 0, "a map that never allocated");
        // 4 buckets below 4 usable, 8 below 8, then power-of-two.
        assert_eq!(
            hashmap_table_bytes::<String, u64>(3),
            hashmap_table_bytes::<String, u64>(1),
            "1 and 3 usable slots are the same 4-bucket table"
        );
        assert!(
            hashmap_table_bytes::<String, u64>(8) > hashmap_table_bytes::<String, u64>(7),
            "7 usable is an 8-bucket table and 8 usable is a 16-bucket one"
        );
    }

    #[test]
    fn vec_block_bytes_charges_capacity_and_not_length() {
        // Capacity, always: `mvt::shrink_geometry` exists because the gap
        // between the two was 91% of this renderer's per-tile memory.
        assert_eq!(vec_block_bytes::<u64>(0), 0);
        assert_eq!(vec_block_bytes::<u64>(2), 16);
        assert_eq!(vec_block_bytes::<u64>(3), 32, "24 bytes occupies a 32-byte block");
        // Capacity deliberately ahead of length, which is the state a
        // `Vec` grown by `push` (or reserved and then partly filled)
        // spends its life in -- and the state a length-based sum reports
        // wrongly.
        let mut reserved: Vec<u64> = Vec::with_capacity(8);
        reserved.extend([1, 2]);
        assert_eq!(reserved.len(), 2);
        assert_eq!(reserved.capacity(), 8);
        assert_eq!(
            vec_block_bytes::<u64>(reserved.capacity()),
            64,
            "eight u64 slots is a 64-byte block, whatever the length is"
        );
        assert_eq!(
            vec_block_bytes::<u64>(reserved.len()),
            16,
            "and charging the LENGTH would have reported 16 -- a quarter of what the \
             allocator is holding"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anti-drift guard for the tracker itself. Every `create_buffer`
    /// and `create_buffer_init` call in `src/` must be inside this module
    /// -- anywhere else, and the buffer it makes is invisible to every
    /// counter here while looking, at the call site, exactly like a
    /// tracked one.
    ///
    /// A source scan rather than a runtime assertion because there is no
    /// runtime moment at which "all buffers have been created" is true.
    #[test]
    fn every_buffer_creation_in_the_crate_goes_through_the_tracker() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut files = vec![root];
        while let Some(path) = files.pop() {
            if path.is_dir() {
                for entry in std::fs::read_dir(&path).expect("src/ must be readable") {
                    files.push(entry.expect("a readable dir entry").path());
                }
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // This module IS the wrapper; its own two calls are the point.
            if path.file_name().and_then(|n| n.to_str()) == Some("alloc.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a readable .rs file");
            for (index, line) in text.lines().enumerate() {
                if line.contains(".create_buffer(") || line.contains(".create_buffer_init(") {
                    offenders.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these buffer creations bypass render::alloc::TrackedBuffer and are invisible to \
             the allocation counters -- route them through TrackedBuffer::new/init:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn the_stats_struct_reports_every_counter() {
        // Not asserting values -- other tests in this binary allocate GPU
        // buffers on other threads, so every counter here is live. What
        // this pins is that `allocation_stats` reads all of them and that
        // the monotonic totals dominate the live gauges, which is true at
        // every instant regardless of what else is running.
        let stats = allocation_stats();
        assert!(
            stats.gpu_buffers_created_total >= stats.gpu_buffers_live,
            "created_total ({}) must never be below live ({})",
            stats.gpu_buffers_created_total,
            stats.gpu_buffers_live
        );
        assert!(
            stats.gpu_buffer_bytes_created_total >= stats.gpu_buffer_bytes_live,
            "bytes_created_total ({}) must never be below bytes_live ({})",
            stats.gpu_buffer_bytes_created_total,
            stats.gpu_buffer_bytes_live
        );
    }

    #[test]
    fn the_counters_serialise_under_their_contract_names() {
        let json = serde_json::to_value(allocation_stats()).expect("stats must serialise");
        let object = json.as_object().expect("stats must serialise to an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        // The `probe` feature adds measurement-only counters. They are
        // listed separately rather than folded into the contract list
        // because they are NOT part of the shipped contract -- a scraper
        // reading a release build must never see them.
        #[cfg(feature = "probe")]
        {
            for probe_only in
                ["ancestor_stand_in_zoom_distance_max",
                "ancestor_stand_in_zoom_distance_sum",
                "ancestor_stand_ins_total",
                "descendant_stand_ins_total",
                "frame_count",
                "frame_ms_p50",
                "frame_ms_p99",
                "frame_nanos_max",
                "frame_nanos_sum",
                "retention_bound_max",
                "retention_bound_min",
                "tile_fetch_bytes_total",
                "tile_fetches_total",
                "uncovered_ancestors_total",
                "visible_tiles_without_stand_in_total",]
            {
                let at = keys
                    .iter()
                    .position(|k| *k == probe_only)
                    .unwrap_or_else(|| panic!("{probe_only} must serialise under `probe`"));
                keys.remove(at);
            }
        }
        assert_eq!(
            keys,
            [
                "draw_calls_last_frame",
                "draw_calls_max",
                "draw_calls_total",
                "gpu_buffer_bytes_created_total",
                "gpu_buffer_bytes_live",
                "gpu_buffers_created_total",
                "gpu_buffers_live",
                "gpu_pass_samples",
                "gpu_symbol_pass_nanos_max",
                "gpu_symbol_pass_nanos_sum",
                "gpu_symbol_pass_samples",
                "gpu_tile_pass_nanos_max",
                "gpu_tile_pass_nanos_sum",
                "heap_in_use_bytes",
                "mesh_installs_total",
                "overlay_bake_ops_total",
                "overlay_bake_vertices_total",
                "overlay_bakes_cancelled_total",
                "overlay_bakes_in_flight",
                "overlay_bakes_in_flight_max",
                "overlay_bakes_total",
                "overlay_generation",
                "overlay_mesh_installs_total",
                "overlay_zoom_refreshes_total",
                "paint_retessellations_total",
                "rss_bytes",
                "tiles_resident",
            ],
            "the serialised counter names are the interface a scraper reads; \
             renaming one is a breaking change"
        );
    }

    /// Serialises the bake-counter tests against each other. The gauge is
    /// process-global, so two of them running on different test threads
    /// would each see the other's guard and read a delta that is not their
    /// own. Nothing outside these tests constructs a `BakeInFlight` in this
    /// binary, so this lock is sufficient as well as necessary.
    static BAKE_COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The gauge half. A guard must take a slot for exactly as long as it
    /// is alive, and give it back when it is not -- otherwise
    /// `overlay_bakes_in_flight` drifts upward over a session and reads as
    /// a permanently stuck queue, which is a worse failure than having no
    /// counter, because it is indistinguishable from the defect it exists
    /// to detect.
    #[test]
    fn a_bake_guard_takes_a_slot_and_gives_it_back() {
        let _serial = BAKE_COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let before = allocation_stats().overlay_bakes_in_flight;
        let guard = BakeInFlight::enter();
        assert_eq!(
            allocation_stats().overlay_bakes_in_flight,
            before + 1,
            "entering a bake must show one more bake in flight"
        );
        drop(guard);
        assert_eq!(
            allocation_stats().overlay_bakes_in_flight,
            before,
            "dropping the guard must give the slot back, on every exit path"
        );
    }

    /// A guard dropped by an unwind is still a guard dropped. Three of this
    /// bake's four exits are ordinary returns, but `spawn_blocking` turns a
    /// panic inside the tessellation into a `JoinError` on the awaiting
    /// side -- and if that path leaked the slot, one bad geometry would
    /// permanently poison the gauge.
    #[test]
    fn a_panicking_bake_still_leaves_the_queue() {
        let _serial = BAKE_COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let before = allocation_stats().overlay_bakes_in_flight;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let unwound = std::panic::catch_unwind(|| {
            let _guard = BakeInFlight::enter();
            panic!("a bake that panics is still a bake that left the queue");
        });
        std::panic::set_hook(hook);

        assert!(unwound.is_err(), "the panic must actually have happened");
        assert_eq!(
            allocation_stats().overlay_bakes_in_flight,
            before,
            "an unwind past the guard must still give the slot back"
        );
    }

    /// The burst this counter exists to see: bakes SPAWNED but not yet
    /// polled must already be visible in the gauge.
    ///
    /// `MapView::bake_overlay_source` calls `enter()` synchronously and moves
    /// the guard into the future it hands to gpui's foreground executor.
    /// That ordering is the whole point. Several pushes in one turn of the
    /// executor spawn several bakes and run NONE of them until the turn
    /// ends, so a guard taken inside the future's body would read a depth
    /// of zero at exactly the moment the queue is deepest -- the same
    /// structural blindness `mesh_installs_total` has, reproduced inside
    /// the instrument built to detect it.
    ///
    /// The futures below are deliberately never polled, which is precisely
    /// the state a just-spawned bake is in.
    #[test]
    fn bakes_spawned_in_one_turn_are_visible_before_any_of_them_runs() {
        let _serial = BAKE_COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let before = allocation_stats().overlay_bakes_in_flight;
        let spawned: Vec<_> = (0..3)
            .map(|_| {
                let guard = BakeInFlight::enter();
                async move {
                    let _guard = guard;
                }
            })
            .collect();

        assert_eq!(
            allocation_stats().overlay_bakes_in_flight,
            before + 3,
            "three bakes spawned in one turn and none of them polled must read as a depth of \
             three -- a gauge that only counts bakes the executor has reached is blind to the \
             exact burst this counter was added for"
        );

        drop(spawned);
        assert_eq!(
            allocation_stats().overlay_bakes_in_flight,
            before,
            "and a future dropped without ever being polled must still give its slot back, or \
             a cancelled executor turn would leak the gauge upward forever"
        );
    }

    /// The high-water half, which is the one that survives being sampled at
    /// the wrong moment: a queue that has already drained reads 0 on the
    /// gauge and still reads its true depth here.
    #[test]
    fn the_high_water_mark_outlives_the_queue_it_measured() {
        let _serial = BAKE_COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let base = allocation_stats().overlay_bakes_in_flight;
        let first = BakeInFlight::enter();
        let second = BakeInFlight::enter();
        let peak = allocation_stats().overlay_bakes_in_flight_max;
        assert!(
            peak >= base + 2,
            "two bakes in flight at once must be visible as a depth of at least two"
        );
        drop(first);
        drop(second);

        let after = allocation_stats();
        assert_eq!(
            after.overlay_bakes_in_flight, base,
            "both slots must be back"
        );
        assert_eq!(
            after.overlay_bakes_in_flight_max, peak,
            "the high-water mark must NOT fall when the queue drains -- that is \
             the entire reason it exists beside the gauge"
        );
    }

    /// Total buffers ever DROPPED -- `created_total - live`. Monotonic
    /// non-decreasing, which is what makes the assertions below immune to
    /// the other tests in this binary allocating on their own threads: a
    /// concurrent create raises both terms and a concurrent drop raises the
    /// difference, so neither can make this go backwards or mask a missing
    /// decrement in the buffer under test.
    fn dropped_totals() -> (u64, u64) {
        let stats = allocation_stats();
        (
            stats.gpu_buffers_created_total - stats.gpu_buffers_live,
            stats.gpu_buffer_bytes_created_total - stats.gpu_buffer_bytes_live,
        )
    }

    /// The pair the whole module exists for, exercised against a real
    /// device: creating a buffer must move the monotonic totals, and
    /// dropping it must give the bytes back.
    ///
    /// Without the drop half, `gpu_buffer_bytes_live` would be a second
    /// copy of `gpu_buffer_bytes_created_total` wearing a different name --
    /// and the question the consumer is trying to answer (are buffers being
    /// recreated instead of reused?) is exactly the one those two counters
    /// only answer when they can disagree.
    #[test]
    fn creating_and_dropping_a_buffer_moves_the_counters_both_ways() {
        pollster::block_on(async {
            let ctx = crate::render::GpuContext::new_headless().await.expect("headless device");

            const SIZE: u64 = 4096;
            let before = allocation_stats();
            let (dropped_before, dropped_bytes_before) = dropped_totals();

            let buffer = TrackedBuffer::new(
                &ctx.device,
                &wgpu::BufferDescriptor {
                    label: Some("alloc-counter-test"),
                    size: SIZE,
                    usage: wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                },
            );
            assert_eq!(buffer.tracked_size(), SIZE);

            let held = allocation_stats();
            assert!(
                held.gpu_buffers_created_total > before.gpu_buffers_created_total,
                "creating a buffer must raise gpu_buffers_created_total"
            );
            assert!(
                held.gpu_buffer_bytes_created_total >= before.gpu_buffer_bytes_created_total + SIZE,
                "creating a {SIZE}-byte buffer must raise gpu_buffer_bytes_created_total by at least that"
            );

            drop(buffer);

            let (dropped_after, dropped_bytes_after) = dropped_totals();
            assert!(
                dropped_after > dropped_before,
                "dropping a TrackedBuffer must decrement gpu_buffers_live"
            );
            assert!(
                dropped_bytes_after >= dropped_bytes_before + SIZE,
                "dropping a {SIZE}-byte TrackedBuffer must give all {SIZE} bytes back to \
                 gpu_buffer_bytes_live"
            );
        });
    }

    /// A mesh install is counted where the mesh actually reaches the GPU,
    /// so the counter covers the overlay's installs as well as tiles'
    /// (both go through `create_tile_buffers`).
    #[test]
    fn installing_a_mesh_counts_one_install_and_two_buffers() {
        pollster::block_on(async {
            let ctx = crate::render::GpuContext::new_headless().await.expect("headless device");
            let pipeline =
                crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let mesh = crate::render::TileMesh { vertices: Vec::new(), indices: Vec::new() };

            let before = allocation_stats();
            let buffers = pipeline.create_tile_buffers(&ctx, &mesh);
            let after = allocation_stats();

            assert!(
                after.mesh_installs_total > before.mesh_installs_total,
                "create_tile_buffers must count a mesh install"
            );
            assert!(
                after.gpu_buffers_created_total >= before.gpu_buffers_created_total + 2,
                "a mesh install is a vertex buffer and an index buffer"
            );
            drop(buffers);
        });
    }

    /// The whole reason this field exists: a large live allocation must
    /// show up in `heap_in_use_bytes`, and dropping it must take it away
    /// again. RSS does NOT satisfy the second half -- libmalloc keeps the
    /// pages -- and mistaking that retention for a leak is what this
    /// counter is here to prevent.
    ///
    /// 256 MiB because the test binary's other threads are allocating
    /// concurrently: the buffer has to be far larger than that noise for
    /// the assertion to mean anything. The tolerance is deliberately loose
    /// (three quarters of the buffer) for the same reason -- what is being
    /// pinned is that the counter MOVES WITH THE HEAP, not that it is
    /// exact to the byte.
    #[cfg(target_os = "macos")]
    #[test]
    fn heap_in_use_follows_a_large_allocation_up_and_back_down() {
        const SIZE: usize = 256 << 20;
        const TOLERANCE: u64 = (SIZE as u64) / 4 * 3;

        let before = heap_in_use_bytes();
        let mut buffer = std::hint::black_box(vec![0u8; SIZE]);
        // Touched so the compiler cannot elide the allocation, and so the
        // pages are real on the RSS side too. Writing to it is NOT enough
        // on its own: under `--release` LLVM proves the whole buffer dead
        // and removes the allocation entirely, and the test then measures
        // two consecutive reads of an unchanged heap (observed:
        // 17145616 -> 17147072). `black_box` is what makes the allocation
        // observable to the optimiser, so the assertion holds in both
        // profiles rather than only in debug.
        buffer[0] = 1;
        buffer[SIZE - 1] = 1;
        let buffer = std::hint::black_box(buffer);
        let held = heap_in_use_bytes();
        assert!(
            held >= before + TOLERANCE,
            "a live {SIZE}-byte buffer must raise heap_in_use_bytes ({before} -> {held})"
        );

        drop(buffer);
        let after = heap_in_use_bytes();
        assert!(
            after + TOLERANCE <= held,
            "dropping a {SIZE}-byte buffer must give the bytes back to heap_in_use_bytes \
             ({held} -> {after}) -- if it does not, this is measuring RSS by another name"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn heap_in_use_is_a_plausible_number_for_a_live_test_process() {
        let heap = heap_in_use_bytes();
        assert!(
            // A floor of 32 KiB, not the megabyte `rss_bytes` uses: a test
            // binary's live heap at an arbitrary instant is genuinely small
            // (measured: ~165 KiB), because unlike RSS it does not count the
            // mapped binary or its libraries.
            (1 << 15..1 << 40).contains(&heap),
            "implausible heap-in-use {heap} -- check malloc_statistics_t's layout against \
             <malloc/malloc.h>"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rss_is_a_plausible_number_for_a_live_test_process() {
        let rss = rss_bytes();
        // A running Rust test binary is never under a megabyte and never
        // over a terabyte. Anything outside that means the struct layout
        // or the flavor constant is wrong, which is the only failure mode
        // worth a test here.
        assert!(
            (1 << 20..1 << 40).contains(&rss),
            "implausible RSS {rss} -- check ProcTaskInfo's layout against <sys/proc_info.h>"
        );
    }

    #[cfg(feature = "probe")]
    #[test]
    fn paint_phases_round_trip_through_the_accumulator() {
        let _guard = phase_lock();
        reset_paint_phases();
        note_paint_phase(PaintPhase::Placement, 2_000_000); // 2 ms in nanos
        note_paint_phase(PaintPhase::Placement, 4_000_000);
        let p = paint_phases_p50();
        assert!(
            (p.placement_ms - 3.0).abs() < 0.001,
            "the p50 of 2 ms and 4 ms is 3 ms, got {}",
            p.placement_ms
        );
        // The two statistics beside it, from the same two samples: the mean
        // agrees with the interpolated median on a symmetric pair, and the
        // max is the worse of the two rather than their average.
        assert!((paint_phases_mean().placement_ms - 3.0).abs() < 0.001);
        assert!((paint_phases_max().placement_ms - 4.0).abs() < 0.001);
        assert_eq!(paint_phase_samples(PaintPhase::Placement), 2);
    }

    /// The anti-drift test for `paint_phases_from`/`PaintPhases::get`: the
    /// enum is mapped onto twenty named fields by hand, twice, and a
    /// transposed pair there would silently report the overlay's cost as
    /// the tile pass's. Every phase gets a distinct duration and has to
    /// come back as exactly its own.
    #[cfg(feature = "probe")]
    #[test]
    fn every_paint_phase_reaches_its_own_field_and_nobody_elses() {
        let _guard = phase_lock();
        reset_paint_phases();
        for (index, &phase) in PaintPhase::ALL.iter().enumerate() {
            note_paint_phase(phase, (index as u64 + 1) * 1_000_000);
        }
        let means = paint_phases_mean();
        for (index, &phase) in PaintPhase::ALL.iter().enumerate() {
            let expected = index as f64 + 1.0;
            assert!(
                (means.get(phase) - expected).abs() < 0.001,
                "{} should read {expected} ms, got {}",
                phase.name(),
                means.get(phase)
            );
        }
        assert_eq!(PaintPhase::ALL.len(), PAINT_PHASE_COUNT);
        // Means are additive, and that is the property the split is read
        // with: 1 + 2 + ... + 19 ms.
        let total: f64 = (1..=PAINT_PHASE_COUNT).map(|ms| ms as f64).sum();
        assert!((means.total_ms() - total).abs() < 0.001, "got {}", means.total_ms());
    }

    /// A phase that never ran divides nothing by zero. `paint` returns
    /// early in half a dozen places, so a run with no `submit_present`
    /// sample at all is ordinary rather than exotic, and a `NaN` in the
    /// split would poison every percentage printed beside it.
    #[cfg(feature = "probe")]
    #[test]
    fn an_unrecorded_phase_reads_zero_rather_than_nan() {
        let _guard = phase_lock();
        reset_paint_phases();
        for &phase in PaintPhase::ALL.iter() {
            assert_eq!(paint_phase_samples(phase), 0);
            assert_eq!(paint_phases_mean().get(phase), 0.0);
            assert_eq!(paint_phases_p50().get(phase), 0.0);
            assert_eq!(paint_phases_max().get(phase), 0.0);
        }
    }

    /// The writer half: a lap charges the span since the previous boundary
    /// to the phase it names, and opens the next one. Without this the
    /// accumulator could be perfect and `paint` still be recording nothing.
    #[cfg(feature = "probe")]
    #[test]
    fn a_lap_charges_the_span_to_the_phase_it_names() {
        let _guard = phase_lock();
        reset_paint_phases();
        let mut lap = PhaseLap::default();
        std::thread::sleep(std::time::Duration::from_millis(2));
        lap.lap(PaintPhase::PlanLookup);
        std::thread::sleep(std::time::Duration::from_millis(2));
        lap.lap(PaintPhase::Placement);

        assert_eq!(paint_phase_samples(PaintPhase::PlanLookup), 1);
        assert_eq!(paint_phase_samples(PaintPhase::Placement), 1);
        let means = paint_phases_mean();
        // A 2 ms sleep is at least 2 ms and, on any machine that can host
        // this suite, nowhere near a second. The assertion is on the
        // ORDER of magnitude on purpose: a sleep is not a stopwatch, and
        // the failure this guards against is a lap that measures zero or
        // measures the whole process.
        for phase in [PaintPhase::PlanLookup, PaintPhase::Placement] {
            assert!(
                (2.0..1000.0).contains(&means.get(phase)),
                "{} measured {} ms for a 2 ms sleep",
                phase.name(),
                means.get(phase)
            );
        }
        // The clock restarted at the first lap: the second phase was NOT
        // charged the first one's span as well.
        assert!(
            means.get(PaintPhase::Placement) < 2.0 + means.get(PaintPhase::PlanLookup),
            "the second lap looks cumulative: {} ms after {} ms",
            means.get(PaintPhase::Placement),
            means.get(PaintPhase::PlanLookup)
        );
        // Nothing else was touched.
        assert_eq!(paint_phase_samples(PaintPhase::Events), 0);
    }

    /// A phase fed known durations reports how many of them missed the
    /// deadline. The durations are the measured ones off the 2026-09-15
    /// `plan_lookup` split -- a 0.004 ms ordinary frame and the 99.064 ms
    /// crossing -- so this is the arithmetic the printer will be doing on
    /// real samples rather than on a set of round numbers.
    ///
    /// The boundary case is deliberate: a sample AT the threshold is not
    /// over it. 16.7 ms is the 60 Hz budget, and a frame that spent exactly
    /// its budget did not miss it.
    #[cfg(feature = "probe")]
    #[test]
    fn a_phase_counts_the_samples_that_missed_the_deadline() {
        let _guard = phase_lock();
        reset_paint_phases();
        for nanos in [4_000_u64, 16_000_000, 16_700_000, 17_000_000, 99_064_000] {
            note_paint_phase(PaintPhase::PlanLookup, nanos);
        }
        assert_eq!(
            paint_phase_window_over(PaintPhase::PlanLookup, 16.7),
            2,
            "17.000 and 99.064 ms are over 16.7; 16.700 ms is not"
        );
        // Every sample is over nothing, and none is over a threshold above
        // the worst -- the two ends that a fencepost error would break.
        assert_eq!(paint_phase_window_over(PaintPhase::PlanLookup, 0.0), 5);
        assert_eq!(paint_phase_window_over(PaintPhase::PlanLookup, 200.0), 0);
        // A phase nothing recorded reads zero rather than counting the
        // zeroed ring slots as 0 ms samples.
        assert_eq!(paint_phase_window_over(PaintPhase::Placement, 0.0), 0);
    }

    /// The whole point of Task 4: `paint_phases_max` is a `fetch_max` held
    /// since process start, so it reports the worst frame of the run
    /// forever -- n=1, which is what made every A/B on the zoom-crossing
    /// plan unreadable. The windowed max forgets, so the two agree while
    /// the ring is still filling and diverge the moment it wraps past a
    /// spike.
    ///
    /// The durations are the log's: a single 124.358 ms `plan_lookup`
    /// frame, set between frames 420 and 480, against a 0.004 ms p50 that
    /// held for the next 1,700 frames.
    #[cfg(feature = "probe")]
    #[test]
    fn the_windowed_max_forgets_a_spike_the_all_time_max_keeps_forever() {
        let _guard = phase_lock();
        reset_paint_phases();
        note_paint_phase(PaintPhase::PlanLookup, 124_358_000);
        for _ in 0..8 {
            note_paint_phase(PaintPhase::PlanLookup, 4_000);
        }

        // Nine samples into a 1024-slot ring, nothing has been forgotten
        // yet and the two maxima are the same number.
        let windowed = paint_phases_window_max().plan_lookup_ms;
        let all_time = paint_phases_max().plan_lookup_ms;
        assert!(
            (windowed - 124.358).abs() < 0.001 && (windowed - all_time).abs() < 0.001,
            "while the ring is filling both maxima are 124.358 ms, got {windowed} and {all_time}"
        );
        assert_eq!(paint_phase_window_over(PaintPhase::PlanLookup, 16.7), 1);

        // Now overwrite every slot with an ordinary frame. The spike is no
        // longer in the retained window.
        for _ in 0..PAINT_PHASE_SAMPLES {
            note_paint_phase(PaintPhase::PlanLookup, 4_000);
        }
        let windowed = paint_phases_window_max().plan_lookup_ms;
        let all_time = paint_phases_max().plan_lookup_ms;
        assert!(
            (windowed - 0.004).abs() < 0.001,
            "the windowed max should have dropped the spike, got {windowed} ms"
        );
        assert!(
            (all_time - 124.358).abs() < 0.001,
            "the all-time max must still hold the spike -- the windowed figure is an \
             addition, not a replacement -- got {all_time} ms"
        );
        assert_eq!(
            paint_phase_window_over(PaintPhase::PlanLookup, 16.7),
            0,
            "the missed-deadline count is a rate over the window, so it forgets too"
        );
    }
}

/// Overlay chunks skipped by per-chunk culling, process-wide. A
/// diagnostic: the tests read it to prove a chunk outside the cover is not
/// drawn.
static OVERLAY_CHUNKS_CULLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn note_overlay_chunks_culled(count: u64) {
    OVERLAY_CHUNKS_CULLED.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
}

/// Total overlay chunks culled so far in this process.
pub fn overlay_chunks_culled_total() -> u64 {
    OVERLAY_CHUNKS_CULLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A tiled overlay source's pipeline, process-wide: what
/// `demos/us_block_groups.rs` reports for phase B. Diagnostics only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayTileCensus {
    /// Tile indexes built (one per tiled generation that landed).
    pub index_builds_total: u64,
    /// Wall time of the last index build (classify + `TileIndex::build`).
    pub index_build_nanos_last: u64,
    /// Bytes the last built index holds (`TileIndex::bytes`).
    pub index_bytes_last: u64,
    /// Overlay tiles cut, planned and tessellated.
    pub tile_bakes_total: u64,
    /// Summed wall time of those bakes, on their workers.
    pub tile_bake_nanos_total: u64,
    /// Vertices those bakes produced.
    pub tile_bake_vertices_total: u64,
    /// Overlay tile draws (whole tiles, stand-ins included; not chunks) in
    /// the last frame painted.
    pub tile_draws_last_frame: u64,
    /// Push -> the first painted frame with a tile draw of that
    /// generation, for the last generation to reach one. 0 until then.
    pub first_frame_nanos_last: u64,
    /// Device bytes the overlay tile caches hold, all sources, as of the
    /// last frame painted.
    pub tile_gpu_bytes: u64,
}

static TILE_INDEX_BUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TILE_INDEX_BUILD_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TILE_INDEX_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TILE_BAKES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TILE_BAKE_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TILE_BAKE_VERTICES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TILE_DRAWS_LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TILE_FIRST_FRAME_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TILE_GPU_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[allow(dead_code)] // native `MapView` only; iOS and web build no overlay tiles
pub(crate) fn note_overlay_tile_index(nanos: u64, bytes: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    TILE_INDEX_BUILDS.fetch_add(1, Relaxed);
    TILE_INDEX_BUILD_NANOS.store(nanos, Relaxed);
    TILE_INDEX_BYTES.store(bytes, Relaxed);
}

#[allow(dead_code)] // native `MapView` only
pub(crate) fn note_overlay_tile_bake(nanos: u64, vertices: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    TILE_BAKES.fetch_add(1, Relaxed);
    TILE_BAKE_NANOS.fetch_add(nanos, Relaxed);
    TILE_BAKE_VERTICES.fetch_add(vertices, Relaxed);
}

#[allow(dead_code)] // native `MapView` only
pub(crate) fn note_overlay_tile_draws(draws: u64, gpu_bytes: u64) {
    TILE_DRAWS_LAST.store(draws, std::sync::atomic::Ordering::Relaxed);
    TILE_GPU_BYTES.store(gpu_bytes, std::sync::atomic::Ordering::Relaxed);
}

#[allow(dead_code)] // native `MapView` only
pub(crate) fn note_overlay_tile_first_frame(nanos: u64) {
    TILE_FIRST_FRAME_NANOS.store(nanos, std::sync::atomic::Ordering::Relaxed);
}

/// See [`OverlayTileCensus`].
pub fn overlay_tile_census() -> OverlayTileCensus {
    use std::sync::atomic::Ordering::Relaxed;
    OverlayTileCensus {
        index_builds_total: TILE_INDEX_BUILDS.load(Relaxed),
        index_build_nanos_last: TILE_INDEX_BUILD_NANOS.load(Relaxed),
        index_bytes_last: TILE_INDEX_BYTES.load(Relaxed),
        tile_bakes_total: TILE_BAKES.load(Relaxed),
        tile_bake_nanos_total: TILE_BAKE_NANOS.load(Relaxed),
        tile_bake_vertices_total: TILE_BAKE_VERTICES.load(Relaxed),
        tile_draws_last_frame: TILE_DRAWS_LAST.load(Relaxed),
        first_frame_nanos_last: TILE_FIRST_FRAME_NANOS.load(Relaxed),
        tile_gpu_bytes: TILE_GPU_BYTES.load(Relaxed),
    }
}
