//! Task 0's bench harness -- the pre-flight baseline the OMIM
//! allocation-pooling plan is falsifiable against. See
//! `.superpowers/sdd/2026-09-02-omim-optimizations/task-0-brief.md`.
//!
//! Four cases, timed and allocation-counted SEPARATELY so the costs are
//! never conflated, each over one real z14 Singapore tile
//! (`fixtures/singapore_z14.pbf`) and the vendored OpenFreeMap Liberty
//! style (`examples/liberty-style.json`, the same one `examples/basic_map.rs`
//! renders):
//!
//! - `mvt::decode` -- parsing the wire bytes into a `DecodedTile`.
//! - `match_tile_static` -- the zoom-independent half of style matching.
//! - `resolve_plan` -- the zoom-dependent half, at zoom 14 (the tile's own
//!   zoom, and a real bucket-crossing point: several Liberty layers gate
//!   their `minzoom`/`maxzoom` in the 12-15 range, so resolving at 14
//!   exercises the gate rather than resolving every layer identically at
//!   every call).
//! - `tessellate_plan_with_ranges` -- turning that plan into a GPU mesh.
//!
//! A fifth case, added by Task 4, does NOT use that tile. See
//! `overlay_replace_from` below for what it covers and why the other four
//! could not.
//!
//! # Allocation counting: a bench-local `#[global_allocator]`, not a new
//! shipped counter
//!
//! `render::alloc` already carries process-wide allocation diagnostics
//! (`heap_in_use_bytes`, GPU buffer tracking), but it reports LIVE bytes,
//! not alloc/free CHURN -- a case that allocates and frees ten temporary
//! `Vec`s within one call and ends with nothing live reads as zero there.
//! Pooling is exactly a churn optimisation (same live data, fewer
//! round trips through the allocator), so this bench needs the call
//! counts, not the net.
//!
//! Getting real alloc/dealloc counts on stable Rust means intercepting
//! every call, which means a `GlobalAlloc` impl. `#[global_allocator]` is
//! process-wide -- but "process" here is THIS bench binary. `cargo bench`
//! compiles `benches/perf.rs` as its own crate, separate from the `gpui-maps`
//! rlib, that happens to link it as a dependency. Declaring the attribute
//! here governs allocation only inside that one process: it has no effect
//! on the library artifact any consumer links, on any `examples/*.rs`
//! binary, or on `cargo test` (a different binary entirely, built from a
//! different crate root). A shipped build's allocator is exactly whatever
//! it would be if this file did not exist.
//!
//! # Reading the two reports below
//!
//! `main` prints two independent things, in this order:
//!
//! 1. An allocation report: for each case, `n` isolated calls, each
//!    bracketed by a snapshot of the global counters immediately before and
//!    immediately after -- nothing else runs in that window, so the delta
//!    is that call's own alloc/dealloc traffic, not warm-up noise or the
//!    cost of dropping the previous iteration's output (which is dropped
//!    only after the window closes). Mean and (sample) standard deviation
//!    over `n` are reported for alloc count, alloc bytes, dealloc count and
//!    dealloc bytes.
//! 2. Criterion's own timing benchmarks, one per case, run after the
//!    allocation pass completes. Criterion's default 100-sample plan
//!    satisfies "n >= 20"; its printed output already carries the mean and
//!    variance the brief asks for.
//!
//! The two passes are sequential and separate on purpose: interleaving
//! them would let criterion's own bookkeeping allocations bleed into the
//! counts this file exists to keep clean.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use criterion::{BatchSize, Criterion};

use gpui_maps::mvt;
use gpui_maps::overlay::{FeatureId, FeatureInput, FeaturePaint, GeometryInput};
use gpui_maps::render::tessellate_plan_with_ranges;
use gpui_maps::style::{match_tile_static, resolve_plan};
use gpui_maps::style_spec;

// ---------------------------------------------------------------------------
// The counting allocator. See the module doc comment for why this is safe
// to declare here and nowhere else.
// ---------------------------------------------------------------------------

struct CountingAllocator;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static DEALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_COUNT.fetch_add(1, Relaxed);
        DEALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        System.alloc_zeroed(layout)
    }

    // Counted as one dealloc of the old size plus one alloc of the new
    // size -- a `Vec` growth path (exactly the churn pooling targets)
    // would otherwise hide behind a single opaque "realloc" bucket the
    // brief's alloc/free framing has no slot for.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        DEALLOC_COUNT.fetch_add(1, Relaxed);
        DEALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        ALLOC_COUNT.fetch_add(1, Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy)]
struct AllocSnapshot {
    allocs: u64,
    alloc_bytes: u64,
    deallocs: u64,
    dealloc_bytes: u64,
}

fn snapshot() -> AllocSnapshot {
    AllocSnapshot {
        allocs: ALLOC_COUNT.load(Relaxed),
        alloc_bytes: ALLOC_BYTES.load(Relaxed),
        deallocs: DEALLOC_COUNT.load(Relaxed),
        dealloc_bytes: DEALLOC_BYTES.load(Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Fixture and style loading.
// ---------------------------------------------------------------------------

/// The tile this bench measures against: a real OpenFreeMap z14 tile over
/// Singapore, x=12918 y=8134 (`fixtures/singapore_z14.pbf`, 123602 bytes).
/// See `task-0-report.md` for the fetch URL and coordinate derivation.
fn fixture_bytes() -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/benches/fixtures/singapore_z14.pbf");
    std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The vendored OpenFreeMap Liberty style -- the same one
/// `examples/basic_map.rs` compiles and renders.
fn liberty_style() -> gpui_maps::style::Style {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json");
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    style_spec::parse_and_compile(&bytes).expect("vendored Liberty style must compile")
}

/// Zoom `resolve_plan` is benchmarked at -- the tile's own zoom, and a real
/// bucket-crossing point for the Liberty style. See the module doc comment.
const ZOOM: u8 = 14;

/// Matches the private `MVT_EXTENT`/`COORD_SCALE` constants
/// `src/map_view.rs`'s real draw path passes to this same function
/// (`crate::render::tessellate_plan_with_ranges(&plan, MVT_EXTENT,
/// crate::render::mesh::COORD_SCALE)`); neither is public -- `MVT_EXTENT`
/// is module-private to `src/map_view.rs` and `COORD_SCALE` is
/// `pub(crate)` -- so this bench crate, outside the library, reproduces the
/// literals rather than reaching them.
const EXTENT: u32 = 4096;
const COORD_SCALE: f32 = 1000.0;

// ---------------------------------------------------------------------------
// The overlay push -- the fifth case, and the only one that is not about the
// tile fixture at all.
// ---------------------------------------------------------------------------

/// # Why this case exists
///
/// The four cases above walk the TILE path: wire bytes, style matching,
/// plan resolution, tessellation. None of them reaches the OVERLAY path --
/// the one an application drives when it pushes its own features, which is
/// where a fleet simulation spends every tick. Task 1 removed a real
/// per-feature allocation on exactly that path (`FeaturePaint::clone()`,
/// `1.000 -> 0.000` allocs per clone, from making `ZoomValue::stops` a
/// `SmallVec`) and proved it with a throwaway example that was deleted
/// afterwards, so nothing has guarded it since. This case is that guard.
///
/// # What it covers, and what it does not
///
/// `OverlayStore::replace_from` is the whole CPU half of an overlay push,
/// short of tessellation: it validates every feature, copies the borrowed
/// coordinates into the owned form (`GeometryInput::to_geometry`), CLONES
/// EVERY `FeaturePaint`, builds the id map, rebuilds the hit index and
/// rebuilds the paint plan (`plan::build_plan`) that a bake then
/// tessellates. It is also what bumps the generation that
/// `MapView::bake_overlay_plan` reads.
///
/// It does NOT tessellate that overlay plan. `OverlayStore::plan()` is
/// `pub(crate)`, and the tessellator it would feed is the same
/// `tessellate_plan_with_ranges` the fourth case already measures on a
/// larger plan -- so widening the library's public API purely to re-measure
/// a function already in this file would buy nothing. The uncovered part is
/// the overlay's *plan construction*, and that is what this measures.
///
/// # The fleet
///
/// Shaped after the collection this crate's overlay work is sized against:
/// a fleet of route lines, one feature each, all pushed whole on every
/// tick. `FEATURES` sets how many `FeaturePaint`s are cloned per call --
/// the quantity Task 1's fix is measured in -- and `POINTS_PER_ROUTE` is
/// kept modest deliberately, because coordinate count adds allocations that
/// scale with geometry and would dilute the per-feature signal this case
/// exists to hold.
///
/// Synthetic rather than a committed fixture: the numbers that matter here
/// are allocation COUNTS, which depend on the feature and part counts and
/// not on where the coordinates are, and a generated fleet is exactly
/// reproducible with no new binary in the tree.
const FEATURES: usize = 206;
const POINTS_PER_ROUTE: usize = 64;

/// Owns the coordinates the borrowed `GeometryInput`s point into. Kept as a
/// separate value from the `FeatureInput`s because `GeometryInput::Line`
/// borrows a slice OF SLICES, so both levels have to outlive the inputs.
struct Fleet {
    routes: Vec<Vec<[f64; 2]>>,
}

impl Fleet {
    fn build() -> Self {
        // Deterministic, and spread over a degree or so of Singapore so the
        // plan's origin and local coordinates are the same order of
        // magnitude as a real push. No RNG: the point is reproducibility.
        let routes = (0..FEATURES)
            .map(|feature| {
                (0..POINTS_PER_ROUTE)
                    .map(|point| {
                        let t = point as f64 / POINTS_PER_ROUTE as f64;
                        let lane = feature as f64 / FEATURES as f64;
                        [103.6 + t * 0.4 + lane * 0.02, 1.2 + lane * 0.3 + t * 0.05]
                    })
                    .collect()
            })
            .collect();
        Self { routes }
    }

    /// One `[&[[f64; 2]]; 1]` per route -- a single-part line, which is the
    /// shape a vehicle track has.
    fn parts(&self) -> Vec<[&[[f64; 2]]; 1]> {
        self.routes.iter().map(|route| [route.as_slice()]).collect()
    }

    fn inputs<'a>(parts: &'a [[&'a [[f64; 2]]; 1]]) -> Vec<FeatureInput<'a>> {
        parts
            .iter()
            .enumerate()
            .map(|(index, part)| FeatureInput {
                // Ids present, not `None`: an id is what makes the store
                // build its `by_id` map, and a fleet always has them.
                id: Some(FeatureId::Number(index as f64)),
                geometry: GeometryInput::Line(&part[..]),
                // The default carries `line_width: ZoomValue::constant(3.0)`
                // -- the one-stop case, which is 79.8% of every `ZoomValue`
                // this crate builds and the exact case Task 1's `SmallVec`
                // removed the allocation for.
                paint: FeaturePaint::default(),
            })
            .collect()
    }
}

/// A store already holding one generation of the fleet.
///
/// The measured call is therefore a REPLACEMENT, not a first push, which is
/// what a simulation actually does every tick: same feature count, new
/// positions, the previous generation dropped inside the call. A first push
/// into an empty store would miss that drop entirely.
fn loaded_store(inputs: &[FeatureInput<'_>]) -> gpui_maps::overlay::OverlayStore {
    let mut store = gpui_maps::overlay::OverlayStore::new();
    store.replace_from(inputs, ZOOM).expect("the synthetic fleet must be accepted");
    store
}

// ---------------------------------------------------------------------------
// A synthetic plan whose lines actually take `append_stroke`'s back-track
// split path, so the fix's own added cost -- not just the ordinary
// (unsplit) Singapore tile -- has a bench case. See
// `fix/line-backtracks`'s code review: the Singapore fixture's own back-
// track share, if any, was never measured to be non-zero, so
// `tessellate_plan_with_ranges`'s allocation counts being identical before
// and after the fix says the fast path is free -- it says nothing about
// the split path itself.
// ---------------------------------------------------------------------------

const BACKTRACK_LINES: usize = 300;
const POINTS_PER_BACKTRACK_LINE: usize = 20;

/// `BACKTRACK_LINES` line features, each a `POINTS_PER_BACKTRACK_LINE`-point
/// zigzag (MVT-tile-scale integer coordinates, comparable to a real road's
/// vertex density) with ONE exact `A -> B -> A` back-track spliced in near
/// its midpoint -- the same shape `a_backtracking_polyline_has_only_unit_
/// normals` pins, at a count large enough that the split loop's own
/// allocation (the `bounds` `Vec` and the `segment_scratch` refill, see
/// `append_stroke`) is not lost in the noise of everything else one
/// `tessellate_plan_with_ranges` call does.
fn synthetic_backtrack_plan() -> gpui_maps::style::PaintPlan {
    use gpui_maps::style::{DashPattern, LineCap, LineJoin, PaintOp, PaintPlan, Rgba};
    let mut ops = Vec::with_capacity(BACKTRACK_LINES);
    for i in 0..BACKTRACK_LINES {
        let base_x = (i as i32 * 7) % 3800;
        let base_y = (i as i32 * 13) % 3800;
        let mut coords: Vec<geo_types::Coord<i32>> = (0..POINTS_PER_BACKTRACK_LINE)
            .map(|p| geo_types::Coord { x: base_x + (p as i32) * 4, y: base_y + (p as i32 % 2) * 3 })
            .collect();
        // Splice an exact back-track in near the midpoint: out to a point
        // 50x40 units off the path and straight back to it.
        let mid = POINTS_PER_BACKTRACK_LINE / 2;
        let apex = coords[mid];
        let out = geo_types::Coord { x: apex.x + 50, y: apex.y - 40 };
        coords.insert(mid + 1, out);
        coords.insert(mid + 2, apex);

        ops.push(PaintOp::Line {
            metrics: false,
            color_from: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
            color_to: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
            width_from: 2.0,
            width_to: 2.0,
            gap_from: 0.0,
            gap_to: 0.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            dash: DashPattern::none(),
            geometry: std::sync::Arc::new(geo_types::Geometry::LineString(geo_types::LineString(coords))),
        });
    }
    PaintPlan { ops }
}

// ---------------------------------------------------------------------------
// Allocation report.
// ---------------------------------------------------------------------------

const ALLOC_SAMPLES: usize = 50;

struct AllocReport {
    n: usize,
    allocs: (f64, f64),
    alloc_bytes: (f64, f64),
    deallocs: (f64, f64),
    dealloc_bytes: (f64, f64),
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn stdev(xs: &[f64], mean: f64) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (xs.len() - 1) as f64;
    var.sqrt()
}

fn summarize(xs: Vec<f64>) -> (f64, f64) {
    let m = mean(&xs);
    (m, stdev(&xs, m))
}

/// Runs `run` `n` times, each preceded by `prepare` (whose own allocations
/// happen OUTSIDE the measured window) and followed by dropping the output
/// AFTER the window closes, so the recorded delta is exactly what `run`
/// itself allocated and freed.
fn measure_allocations<In, Out>(
    n: usize,
    mut prepare: impl FnMut() -> In,
    mut run: impl FnMut(In) -> Out,
) -> AllocReport {
    let mut allocs = Vec::with_capacity(n);
    let mut alloc_bytes = Vec::with_capacity(n);
    let mut deallocs = Vec::with_capacity(n);
    let mut dealloc_bytes = Vec::with_capacity(n);

    for _ in 0..n {
        let input = prepare();
        let before = snapshot();
        let output = black_box(run(black_box(input)));
        let after = snapshot();
        allocs.push((after.allocs - before.allocs) as f64);
        alloc_bytes.push((after.alloc_bytes - before.alloc_bytes) as f64);
        deallocs.push((after.deallocs - before.deallocs) as f64);
        dealloc_bytes.push((after.dealloc_bytes - before.dealloc_bytes) as f64);
        drop(output);
    }

    AllocReport {
        n,
        allocs: summarize(allocs),
        alloc_bytes: summarize(alloc_bytes),
        deallocs: summarize(deallocs),
        dealloc_bytes: summarize(dealloc_bytes),
    }
}

fn print_report(name: &str, report: &AllocReport) {
    println!("{name} (n={})", report.n);
    println!(
        "  allocs:   mean {:>10.1}  stdev {:>10.1}",
        report.allocs.0, report.allocs.1
    );
    println!(
        "  alloc B:  mean {:>10.1}  stdev {:>10.1}",
        report.alloc_bytes.0, report.alloc_bytes.1
    );
    println!(
        "  deallocs: mean {:>10.1}  stdev {:>10.1}",
        report.deallocs.0, report.deallocs.1
    );
    println!(
        "  dealloc B:mean {:>10.1}  stdev {:>10.1}",
        report.dealloc_bytes.0, report.dealloc_bytes.1
    );
}

fn run_allocation_report() {
    let fixture = fixture_bytes();
    let style = liberty_style();
    let tile = mvt::decode(fixture.clone()).expect("fixture must decode");
    let tile_match = match_tile_static(&tile, &style);
    let plan = resolve_plan(&tile, &style, &tile_match, ZOOM);

    println!("=== Task 0 allocation baseline ===");

    let decode_report = measure_allocations(
        ALLOC_SAMPLES,
        || fixture.clone(),
        |bytes| mvt::decode(bytes).expect("fixture must decode"),
    );
    print_report("mvt::decode", &decode_report);

    let match_report = measure_allocations(ALLOC_SAMPLES, || (), |()| match_tile_static(&tile, &style));
    print_report("match_tile_static", &match_report);

    let resolve_report = measure_allocations(
        ALLOC_SAMPLES,
        || (),
        |()| resolve_plan(&tile, &style, &tile_match, ZOOM),
    );
    print_report("resolve_plan", &resolve_report);

    let tessellate_report = measure_allocations(
        ALLOC_SAMPLES,
        || (),
        |()| tessellate_plan_with_ranges(&plan, EXTENT, COORD_SCALE, gpui_maps::render::subdivide::Subdivision::for_tile(gpui_maps::geo::TileId { z: 14, x: 22491, y: 13724 })),
    );
    print_report("tessellate_plan_with_ranges", &tessellate_report);

    let backtrack_plan = synthetic_backtrack_plan();
    let backtrack_report = measure_allocations(
        ALLOC_SAMPLES,
        || (),
        |()| tessellate_plan_with_ranges(&backtrack_plan, EXTENT, COORD_SCALE, gpui_maps::render::subdivide::Subdivision::NONE),
    );
    print_report("tessellate_plan_with_ranges_backtracks", &backtrack_report);

    // The overlay path. Nothing above it touches the tile fixture's
    // neighbours here -- see `Fleet` for why this case is synthetic.
    let fleet = Fleet::build();
    let parts = fleet.parts();
    let inputs = Fleet::inputs(&parts);
    let overlay_report = measure_allocations(
        ALLOC_SAMPLES,
        || loaded_store(&inputs),
        |mut store| {
            store
                .replace_from(&inputs, ZOOM)
                .expect("the synthetic fleet must be accepted");
            store
        },
    );
    print_report("overlay_replace_from", &overlay_report);

    println!("=== end allocation baseline ===\n");
}

// ---------------------------------------------------------------------------
// Criterion timing. Custom `main` (not `criterion_main!`) so the
// allocation report above runs first, in isolation, before criterion's own
// bookkeeping starts allocating on this same counting allocator.
// ---------------------------------------------------------------------------

fn run_timing(c: &mut Criterion) {
    let fixture = fixture_bytes();
    let style = liberty_style();
    let tile = mvt::decode(fixture.clone()).expect("fixture must decode");
    let tile_match = match_tile_static(&tile, &style);
    let plan = resolve_plan(&tile, &style, &tile_match, ZOOM);

    c.bench_function("mvt::decode", |b| {
        b.iter_batched(
            || fixture.clone(),
            |bytes| mvt::decode(black_box(bytes)).expect("fixture must decode"),
            BatchSize::PerIteration,
        )
    });

    c.bench_function("match_tile_static", |b| {
        b.iter(|| match_tile_static(black_box(&tile), black_box(&style)))
    });

    c.bench_function("resolve_plan", |b| {
        b.iter(|| resolve_plan(black_box(&tile), black_box(&style), black_box(&tile_match), black_box(ZOOM)))
    });

    c.bench_function("tessellate_plan_with_ranges", |b| {
        b.iter(|| tessellate_plan_with_ranges(black_box(&plan), black_box(EXTENT), black_box(COORD_SCALE), black_box(gpui_maps::render::subdivide::Subdivision::for_tile(gpui_maps::geo::TileId { z: 14, x: 22491, y: 13724 }))))
    });

    // The split path itself -- see `synthetic_backtrack_plan`'s doc comment.
    let backtrack_plan = synthetic_backtrack_plan();
    c.bench_function("tessellate_plan_with_ranges_backtracks", |b| {
        b.iter(|| {
            tessellate_plan_with_ranges(
                black_box(&backtrack_plan),
                black_box(EXTENT),
                black_box(COORD_SCALE),
                black_box(gpui_maps::render::subdivide::Subdivision::NONE),
            )
        })
    });

    let fleet = Fleet::build();
    let parts = fleet.parts();
    let inputs = Fleet::inputs(&parts);
    c.bench_function("overlay_replace_from", |b| {
        b.iter_batched(
            || loaded_store(&inputs),
            |mut store| {
                store
                    .replace_from(black_box(&inputs), black_box(ZOOM))
                    .expect("the synthetic fleet must be accepted");
                store
            },
            // `PerIteration` for the same reason `mvt::decode` uses it: the
            // setup allocates a whole generation of features, and batching
            // several of those into one timed group would hold every one of
            // them live at once.
            BatchSize::PerIteration,
        )
    });
}

fn main() {
    run_allocation_report();

    let mut criterion = Criterion::default().configure_from_args();
    run_timing(&mut criterion);
    criterion.final_summary();
}
