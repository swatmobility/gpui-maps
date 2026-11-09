//! Does pushing one overlay source cost anything for the sources beside it?
//!
//! A consumer reported the symptom exactly right: **hiding the bus-stop
//! layer made the buses move faster.** There used to be one overlay
//! collection, so a fleet and a stop layer had to share it, and every
//! vehicle position update re-planned, re-tessellated, re-indexed and
//! re-uploaded five thousand stops that had not moved. Nothing in any
//! design says content that never changes should cost motion.
//!
//! This measures whether it still does, in three arms over the same data
//! on the same binary:
//!
//! 1. **`shared`** -- vehicles AND stops in ONE source, re-pushed together
//!    every tick. This is what the pre-sources API forced, so it is the
//!    BEFORE number, measured here rather than remembered.
//! 2. **`sources+stops`** -- stops in their own source, pushed ONCE;
//!    vehicles in their own source, re-pushed every tick.
//! 3. **`sources only`** -- vehicles alone, no stop source at all.
//!
//! The acceptance condition is arm 2 == arm 3: pushing the vehicles must
//! cost the same whether or not a large static source is present. Arm 1 is
//! there so the difference has a scale.
//!
//! # And two arms for DATA-DRIVEN PAINT
//!
//! 4. **`resolved x12`** -- the same 3,140 vehicles, each carrying one of
//!    twelve route colours as RESOLVED paint. This is what an overlay
//!    feature could express before data-driven paint, and the reason the
//!    consumer had 3,140 features: twelve colours, no way to say so except
//!    one feature per object. One `PaintOp` per vehicle.
//! 5. **`data-driven`** -- the SAME 3,140 vehicles and the same twelve
//!    colours, pushed once as properties with
//!    `{"fill-color": ["get", "route_colour"]}` declared on the source.
//!    Twelve `PaintOp`s, and still 3,140 features with 3,140 ids and 3,140
//!    hit targets.
//!
//! The acceptance condition there is arm 5's `ops per bake` being the
//! COLOUR COUNT where arm 4's is the FEATURE COUNT, at an unchanged
//! `vertices per bake` -- geometry is geometry, and a merge that changed
//! the vertex count would have changed what is drawn. It is NOT a frame
//! time claim; see "What `ops per bake` is and is NOT worth" below, which
//! is the part of this file worth reading before quoting a number from it.
//!
//! # What it controls for
//!
//! - **Byte-identical content on every push.** The features are built
//!   ONCE, leaked, and re-pushed; nothing about the payload changes
//!   between the first push and the last. The payload is also RE-MEASURED
//!   every window and asserted constant rather than assumed -- see
//!   `Window::payload`. Two measurement artifacts in this area have
//!   already been caught by exactly that check.
//! - **Disjoint windows, never cumulative.** Every counter is read as a
//!   delta across one window, and the bake queue is DRAINED at each
//!   window's end before the sample is taken, so a bake started in window
//!   3 cannot be charged to window 4.
//! - **No window, no paint, no GPU.** `Application::headless` gives a real
//!   gpui `App` with a real foreground executor -- so `cx.spawn`'d bakes
//!   are genuinely polled and `install_overlay_mesh` genuinely runs -- but
//!   `Render::render` is never called and no surface exists. Everything
//!   measured here is upstream of the GPU.
//! - **`EmptyTileSource`.** No network, no tiles, no decode. The overlay
//!   path is the only thing running.
//!
//! # What `ops per bake` is and is NOT worth -- measured, not assumed
//!
//! `overlay_bake_vertices_total` is FLAT across arms 4 and 5 and must be:
//! the same rings are projected and tessellated either way. What collapses
//! is `overlay_bake_ops_total`, 3,140 to 12, and with each op goes an
//! `Arc<Geometry>`, a parts `Vec`, a lyon tessellator pass, an `OpSpan` and
//! a 176-byte `PaintOp`.
//!
//! **That collapse buys almost no CPU, and this probe says so rather than
//! implying otherwise.** Arms 4 and 5 come out within noise of each other
//! on `sync mean`, and `overlay::plan::merged_bake_bench` -- which times
//! the two halves a stopwatch around a push cannot separate -- puts
//! `build_plan` at 0.96x and `tessellate` at 1.01x for 261x fewer ops.
//! lyon's per-op overhead is genuinely small next to its per-polygon work,
//! and `append_ops` already pools the tessellator across ops.
//!
//! So the honest reading is: data-driven paint is an EXPRESSIVENESS change
//! with a memory-and-allocation dividend, not a frame-time one. What it
//! actually unlocks is that arm 5 keeps **3,140 features** -- 3,140 ids,
//! 3,140 hit targets, 3,140 labels -- while the draw collapses to twelve
//! ops. The consumer's workaround gave that up: twelve colour-grouped
//! `Point` features is twelve ops too, and it costs them the heading each
//! bus points in, per-bus identity, and per-bus labels.
//!
//! Where a push's time DOES go, at this payload, is per COORDINATE and not
//! per feature or per op: `build_plan` projects all 69,080 of them into
//! Mercator world space, which is a `tan`, a `cos`, a reciprocal and a
//! `ln` each. See `merged_bake_bench`, which prints the attribution at
//! four payload sizes and gives the per-coordinate figures the claim rests
//! on.
//!
//! It used to be paid THREE times a push -- `build_plan` projected
//! everything once to find its origin and again for the geometry, and
//! `HitIndex::build` projected everything a third time to store it. Two of
//! the three are gone (`geom::LonLatBounds`, and `hit`'s bbox-plus-lazy
//! shape), which took this payload's push from 3.73 ms to 1.60 ms.
//!
//! # Why `vertices per bake` is the headline and not the push time
//!
//! The synchronous cost of a push is measurable with a stopwatch around
//! the call, and it is real -- it is UI-thread time. But the expensive
//! half of a push is the tessellation, which runs on a blocking worker and
//! is invisible to that stopwatch. `overlay_bake_vertices_total /
//! overlay_bakes_total` is the number that answers the actual question:
//! under one shared collection every delivered bake tessellates
//! vehicles + stops, and under sources it tessellates the vehicles alone.
//! That ratio is also insensitive to how many bakes were superseded, which
//! a raw total is not.
//!
//! # What this does NOT measure
//!
//! Zoom independence. `refresh_overlay_bake_zoom` runs from `paint`, and
//! nothing here paints, so a camera move costs this process nothing for
//! reasons that have nothing to do with sources. That claim is proved by
//! `map_view::tests::a_zoom_crossing_never_touches_a_zoom_independent_
//! source_beside_a_dependent_one`, which counts re-resolves rather than
//! timing them, and by `MapView::overlay_zoom_refreshes` for a host that
//! wants to watch it on its own data.
//!
//! Run with:
//!   cargo run --release --example overlay_source_probe

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext};
use gpui_maps::map_view::MapView;
use gpui_maps::overlay::{
    DataFeatureInput, FeatureId, FeatureInput, FeaturePaint, FeatureProperties, GeometryInput,
    SourceOptions, TagValue,
};
use gpui_maps::style::Style;
use gpui_maps::tile_source::EmptyTileSource;

const VEHICLES_SOURCE: &str = "vehicles";
const STOPS_SOURCE: &str = "stops";

/// The consumer's twelve route colours, near enough: twelve visibly
/// different hues, so a merged mesh painted one colour by mistake is
/// something a test can catch rather than something a reader has to
/// believe.
const PALETTE: [&str; 12] = [
    "#e6194b", "#3cb44b", "#ffe119", "#4363d8", "#f58231", "#911eb4", "#46f0f0", "#f032e6",
    "#bcf60c", "#fabebe", "#008080", "#e6beff",
];

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// One heading-shaped vehicle marker: an 11-position ring, twice, which is
/// the shape the consumer had to DEGRADE to twelve colour-grouped Point
/// features to keep their frame budget. Deterministic, so two runs of this
/// probe push the same bytes.
fn marker(index: usize) -> [Vec<[f64; 2]>; 2] {
    // A cheap deterministic scatter -- an LCG, not `rand`, so the probe has
    // no dependency and no seed to forget.
    let mut state = (index as u64).wrapping_mul(6364136223846793005).wrapping_add(1);
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as f64 / (1u64 << 31) as f64) - 0.5
    };
    let lon = 103.60 + 0.30 * (next() + 0.5);
    let lat = 1.22 + 0.20 * (next() + 0.5);
    let heading = next() * std::f64::consts::TAU;
    // Metres-per-degree at the equator, near enough for a synthetic marker.
    let scale = 25.0 / 111_320.0;
    let ring = |radius: f64| -> Vec<[f64; 2]> {
        let mut out = Vec::with_capacity(11);
        for step in 0..10 {
            let angle = heading + step as f64 * std::f64::consts::TAU / 10.0;
            out.push([lon + radius * scale * angle.cos(), lat + radius * scale * angle.sin()]);
        }
        // Closed ring: GeoJSON wants the first position repeated, and
        // `build_plan` drops a ring of fewer than four positions.
        out.push(out[0]);
        out
    };
    [ring(1.0), ring(0.55)]
}

/// One bus stop: a point with a CONSTANT `circle-radius`, so the source is
/// zoom-independent and a crossing can never touch it.
fn stop(index: usize) -> Vec<[f64; 2]> {
    let mut state = (index as u64 ^ 0x5deece66d).wrapping_mul(6364136223846793005).wrapping_add(7);
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as f64 / (1u64 << 31) as f64) - 0.5
    };
    vec![[103.60 + 0.30 * (next() + 0.5), 1.22 + 0.20 * (next() + 0.5)]]
}

/// Which arm one window belongs to and what it did.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// Vehicles and stops in one collection, re-pushed together -- the
    /// pre-sources API.
    Shared,
    /// Separate sources, stops present and pushed once.
    SourcesWithStops,
    /// Separate sources, no stop source at all.
    SourcesOnly,
    /// The vehicles in twelve route colours, each colour carried as
    /// RESOLVED paint on its own feature -- which is the only way an
    /// overlay feature could express twelve colours before data-driven
    /// paint, and the reason the consumer had 3,140 features.
    ResolvedColours,
    /// The same vehicles and the same twelve colours, as ONE data-driven
    /// feature set: properties per feature, `["get", "route_colour"]`
    /// declared on the source.
    DataDriven,
}

impl Arm {
    fn label(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::SourcesWithStops => "sources+stops",
            Self::SourcesOnly => "sources only",
            Self::ResolvedColours => "resolved x12",
            Self::DataDriven => "data-driven",
        }
    }
}

/// What an arm pushes, whichever of the two structured doors it uses.
///
/// An enum rather than two loops because everything around the push --
/// the window boundaries, the drain, the payload re-measurement, the
/// counter deltas -- has to be identical for the arms to be comparable at
/// all, and two loops is how one of them quietly stops draining.
#[derive(Clone, Copy)]
enum Payload {
    Resolved(&'static [FeatureInput<'static>]),
    Data(&'static [DataFeatureInput<'static>]),
}

impl Payload {
    fn len(self) -> usize {
        match self {
            Self::Resolved(features) => features.len(),
            Self::Data(features) => features.len(),
        }
    }

    fn coordinates(self) -> usize {
        match self {
            Self::Resolved(features) => coordinates(features),
            Self::Data(features) => {
                features.iter().map(|f| geometry_coordinates(&f.feature.geometry)).sum()
            }
        }
    }
}

/// Everything one window measured. Every field is a DELTA across that
/// window, or a mean over it -- nothing here is cumulative.
struct Window {
    arm: Arm,
    index: usize,
    pushes: usize,
    /// Synchronous cost of the push call itself: UI-thread time.
    mean_ms: f64,
    max_ms: f64,
    /// Bakes that ran a tessellation to completion in this window, and the
    /// vertices they produced.
    bakes: u64,
    vertices: u64,
    /// `PaintOp`s those bakes tessellated -- the number data-driven paint
    /// collapses, and the one `vertices` cannot see.
    ops: u64,
    /// Bakes abandoned because a later push to the same source superseded
    /// them. Reported because `vertices / bakes` has to be read knowing
    /// how much was thrown away.
    cancelled: u64,
    /// What was pushed, re-measured this window: (features, coordinates).
    /// Asserted equal to every other window of the same arm rather than
    /// assumed.
    payload: (usize, usize),
}

impl Window {
    fn vertices_per_bake(&self) -> f64 {
        self.per_bake(self.vertices)
    }

    fn ops_per_bake(&self) -> f64 {
        self.per_bake(self.ops)
    }

    fn per_bake(&self, total: u64) -> f64 {
        if self.bakes == 0 {
            0.0
        } else {
            total as f64 / self.bakes as f64
        }
    }
}

fn main() {
    let vehicles_count = env_usize("PROBE_VEHICLES", 3150);
    let stops_count = env_usize("PROBE_STOPS", 4931);
    let push_hz = env_f64("PROBE_HZ", 5.0);
    let window_secs = env_f64("PROBE_WINDOW_SECS", 4.0);
    let windows = env_usize("PROBE_WINDOWS", 3);

    // Leaked so the borrowed `FeatureInput`s can be `'static` and live
    // inside the spawned loop. A probe process that exits when it is done
    // has nothing to give back.
    let vehicle_rings: &'static Vec<[Vec<[f64; 2]>; 2]> =
        Box::leak(Box::new((0..vehicles_count).map(marker).collect::<Vec<_>>()));
    let vehicle_polys: &'static Vec<[[&'static [[f64; 2]]; 1]; 2]> = Box::leak(Box::new(
        vehicle_rings
            .iter()
            .map(|[outer, inner]| [[outer.as_slice()], [inner.as_slice()]])
            .collect::<Vec<_>>(),
    ));
    let vehicle_nested: &'static Vec<[&'static [&'static [[f64; 2]]]; 2]> = Box::leak(Box::new(
        vehicle_polys
            .iter()
            .map(|[a, b]| [a.as_slice(), b.as_slice()])
            .collect::<Vec<_>>(),
    ));
    let vehicles: &'static Vec<FeatureInput<'static>> = Box::leak(Box::new(
        vehicle_nested
            .iter()
            .enumerate()
            .map(|(i, polys)| FeatureInput {
                id: Some(FeatureId::String(format!("bus-{i}"))),
                geometry: GeometryInput::Polygon(polys.as_slice()),
                paint: FeaturePaint::default(),
            })
            .collect::<Vec<_>>(),
    ));

    let stop_points: &'static Vec<Vec<[f64; 2]>> =
        Box::leak(Box::new((0..stops_count).map(stop).collect::<Vec<_>>()));
    let stops: &'static Vec<FeatureInput<'static>> = Box::leak(Box::new(
        stop_points
            .iter()
            .enumerate()
            .map(|(i, point)| FeatureInput {
                id: Some(FeatureId::String(format!("stop-{i}"))),
                geometry: GeometryInput::Point(point.as_slice()),
                // A constant radius, which is what makes this source
                // zoom-independent. `declare_source` below insists on it.
                paint: FeaturePaint { circle_radius: 4.0, ..FeaturePaint::default() },
            })
            .collect::<Vec<_>>(),
    ));

    // Both arms 1 and 2 have to push the same bytes, so the shared arm's
    // one collection is literally the concatenation of the two sources.
    let both: &'static Vec<FeatureInput<'static>> = Box::leak(Box::new(
        vehicles.iter().cloned().chain(stops.iter().cloned()).collect::<Vec<_>>(),
    ));

    // Arm 4: the same vehicles, each with one of twelve route colours
    // written into its OWN resolved paint. Same geometry as `vehicles` --
    // the same leaked rings, borrowed again -- so arms 4 and 5 differ from
    // each other in nothing but where the colour comes from.
    let coloured: &'static Vec<FeatureInput<'static>> = Box::leak(Box::new(
        vehicle_nested
            .iter()
            .enumerate()
            .map(|(i, polys)| FeatureInput {
                id: Some(FeatureId::String(format!("bus-{i}"))),
                geometry: GeometryInput::Polygon(polys.as_slice()),
                paint: FeaturePaint {
                    fill_color: gpui_maps::style_spec::color::parse_css_color(
                        PALETTE[i % PALETTE.len()],
                    )
                    .expect("the probe's own palette is valid"),
                    ..FeaturePaint::default()
                },
            })
            .collect::<Vec<_>>(),
    ));

    // Arm 5: the same vehicles again, with the route colour as a PROPERTY.
    // Built ONCE and leaked, exactly as every other payload here is, which
    // is also how a real consumer holds them -- a bus's route colour does
    // not change when the bus moves, so only the geometry is new per tick
    // and the `Arc` clone per push is a refcount.
    let route_properties: &'static Vec<Arc<FeatureProperties>> = Box::leak(Box::new(
        (0..PALETTE.len())
            .map(|route| {
                Arc::new(FeatureProperties::from([(
                    "route_colour".to_string(),
                    TagValue::String(PALETTE[route].to_string()),
                )]))
            })
            .collect::<Vec<_>>(),
    ));
    let data_driven: &'static Vec<DataFeatureInput<'static>> = Box::leak(Box::new(
        vehicle_nested
            .iter()
            .enumerate()
            .map(|(i, polys)| DataFeatureInput {
                feature: FeatureInput {
                    id: Some(FeatureId::String(format!("bus-{i}"))),
                    geometry: GeometryInput::Polygon(polys.as_slice()),
                    // No colour of its own at all: the source's expression
                    // is what paints it.
                    paint: FeaturePaint::default(),
                },
                properties: Arc::clone(&route_properties[i % PALETTE.len()]),
            })
            .collect::<Vec<_>>(),
    ));

    println!(
        "overlay_source_probe: {vehicles_count} vehicles ({} coords), \
         {stops_count} stops ({} coords), {push_hz} Hz, {windows} x {window_secs}s per arm",
        coordinates(vehicles),
        coordinates(stops),
    );
    println!(
        "  the shared arm pushes all {} features every tick, which is what one collection forced",
        both.len()
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let handle = runtime.handle().clone();

    gpui_platform::headless().run(move |cx: &mut App| {
        let handle = handle.clone();
        cx.spawn(async move |cx| {
            let mut report: Vec<Window> = Vec::new();

            for arm in [
                Arm::Shared,
                Arm::SourcesWithStops,
                Arm::SourcesOnly,
                Arm::ResolvedColours,
                Arm::DataDriven,
            ] {
                // A FRESH view per arm. Sharing one would let the arm that
                // ran first leave its mesh arena warm (and its stop source
                // resident) for the arm that ran second, which is exactly
                // the kind of carry-over these numbers are supposed to be
                // free of.
                let view = cx
                    .update(|cx| {
                        cx.new(|_| {
                            MapView::new(
                                103.75,
                                1.32,
                                13.0,
                                1024.0,
                                768.0,
                                Style::default(),
                                Arc::new(EmptyTileSource)
                                    as Arc<dyn gpui_maps::tile_source::TileSource>,
                                handle.clone(),
                            )
                        })
                    });

                // The one-time setup each arm needs before any timing
                // starts, drained to quiet so the first window is not
                // measuring somebody else's bake.
                let setup = view.update(cx, |view, cx| match arm {
                    Arm::Shared | Arm::SourcesOnly | Arm::ResolvedColours => Ok(()),
                    Arm::SourcesWithStops => {
                        // Declared, so the probe is not merely hoping the
                        // stops are zoom-independent -- a push that broke
                        // it would be refused here.
                        view.declare_source(STOPS_SOURCE, SourceOptions { zoom_independent: true, ..Default::default() })?;
                        view.set_source_from(STOPS_SOURCE, stops, cx)
                    }
                    Arm::DataDriven => {
                        // The whole arm, in one call: the source's fill
                        // colour is now an expression over each feature's
                        // own `route_colour`.
                        view.declare_source_paint(
                            VEHICLES_SOURCE,
                            &serde_json::from_str(r#"{"fill-color":["get","route_colour"]}"#)
                                .expect("the probe's own declaration is valid JSON"),
                        )?;
                        // And zoom-independent as well, so the two
                        // declarations are asserted to compose rather than
                        // assumed to: a data-driven colour that had
                        // acquired a zoom axis would be refused right here.
                        view.declare_source(
                            VEHICLES_SOURCE,
                            SourceOptions { zoom_independent: true, ..Default::default() },
                        )?;
                        Ok(())
                    }
                });
                match setup {
                    Ok(()) => {}
                    Err(error) => panic!("the probe's own features must be valid: {error}"),
                }
                drain(cx).await;

                if arm == Arm::SourcesWithStops {
                    let (independent, features) = view
                        .read_with(cx, |view, _| {
                            (
                                view.overlay().source_is_zoom_independent(STOPS_SOURCE),
                                view.overlay().source_features(STOPS_SOURCE).map(|f| f.len()),
                            )
                        });
                    println!(
                        "\n-- {} -- stop source: {:?} features, zoom-independent: {:?}",
                        arm.label(),
                        features,
                        independent
                    );
                } else {
                    println!("\n-- {} --", arm.label());
                }

                let payload = match arm {
                    Arm::Shared => Payload::Resolved(both),
                    Arm::SourcesWithStops | Arm::SourcesOnly => Payload::Resolved(vehicles),
                    Arm::ResolvedColours => Payload::Resolved(coloured),
                    Arm::DataDriven => Payload::Data(data_driven),
                };

                for index in 0..windows {
                    let window = push_for(
                        &view,
                        cx,
                        arm,
                        index,
                        payload,
                        Duration::from_secs_f64(1.0 / push_hz),
                        window_secs,
                    )
                    .await;
                    println!("{}", line(&window));
                    report.push(window);
                }

                // What the store actually built, read once per arm from
                // the source itself. An op count is deterministic, so
                // this is an ASSERTION about the collapse rather than a
                // measurement of it -- and it is the claim the vertex
                // counter cannot make.
                if matches!(arm, Arm::ResolvedColours | Arm::DataDriven) {
                    let built = view
                        .read_with(cx, |view, _| {
                            (
                                view.overlay().source_op_count(VEHICLES_SOURCE),
                                view.overlay().source_is_data_driven(VEHICLES_SOURCE),
                                view.overlay().source_features(VEHICLES_SOURCE).map(|f| f.len()),
                                view.overlay().source_is_zoom_independent(VEHICLES_SOURCE),
                            )
                        });
                    println!(
                        "     built: {:?} ops from {:?} features, data-driven: {:?}, \
                         zoom-independent: {:?}",
                        built.0, built.2, built.1, built.3,
                    );
                }
            }

            summarise(&report);
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

fn coordinates(features: &[FeatureInput<'_>]) -> usize {
    features.iter().map(|feature| geometry_coordinates(&feature.geometry)).sum()
}

fn geometry_coordinates(geometry: &GeometryInput<'_>) -> usize {
    match geometry {
        GeometryInput::Empty => 0,
        GeometryInput::Point(points) => points.len(),
        GeometryInput::Line(parts) => parts.iter().map(|part| part.len()).sum(),
        GeometryInput::Polygon(polygons) => {
            polygons.iter().flat_map(|rings| rings.iter()).map(|ring| ring.len()).sum()
        }
    }
}

/// Waits until no overlay bake is outstanding, so one window's
/// tessellation cannot be charged to the next.
///
/// Polls rather than awaiting a signal because there is nothing to await
/// on: `overlay_bakes_in_flight` is a gauge, and a probe that is about to
/// sample counters can afford a 5 ms tick. Bounded so a stuck bake fails
/// the run rather than hanging it.
async fn drain(cx: &mut gpui::AsyncApp) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if gpui_maps::render::allocation_stats().overlay_bakes_in_flight == 0 {
            return;
        }
        if Instant::now() > deadline {
            eprintln!("overlay_source_probe: bakes did not drain in 30s -- numbers are suspect");
            return;
        }
        cx.background_executor().timer(Duration::from_millis(5)).await;
    }
}

/// Pushes at `interval` for `secs`, timing every call, then drains the
/// bake queue and samples. Everything returned is a delta across this
/// window alone.
#[allow(clippy::too_many_arguments)]
async fn push_for(
    view: &gpui::Entity<MapView>,
    cx: &mut gpui::AsyncApp,
    arm: Arm,
    index: usize,
    features: Payload,
    interval: Duration,
    secs: f64,
) -> Window {
    // Read BEFORE the first push and again after the drain, so the window
    // is closed on both sides. A cumulative read would make every window
    // look like the sum of the ones before it, which is one of the two
    // measurement mistakes this area has already produced.
    let before = gpui_maps::render::allocation_stats();
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let mut total = Duration::ZERO;
    let mut max = Duration::ZERO;
    let mut pushes = 0usize;
    while Instant::now() < deadline {
        let started = Instant::now();
        let outcome = view.update(cx, |view, cx| match (arm, features) {
            // The pre-sources call, unchanged and still working -- which
            // is also this arm's other job: it is the backward-compat
            // path under load.
            (Arm::Shared, Payload::Resolved(features)) => view.set_features_from(features, cx),
            (_, Payload::Resolved(features)) => {
                view.set_source_from(VEHICLES_SOURCE, features, cx)
            }
            (_, Payload::Data(features)) => {
                view.set_source_data_from(VEHICLES_SOURCE, features, cx)
            }
        });
        let elapsed = started.elapsed();
        match outcome {
            Ok(()) => {}
            Err(error) => panic!("the probe's own features must be valid: {error}"),
        }
        total += elapsed;
        max = max.max(elapsed);
        pushes += 1;
        cx.background_executor().timer(interval).await;
    }
    drain(cx).await;
    let after = gpui_maps::render::allocation_stats();

    Window {
        arm,
        index,
        pushes,
        mean_ms: if pushes == 0 { 0.0 } else { total.as_secs_f64() * 1e3 / pushes as f64 },
        max_ms: max.as_secs_f64() * 1e3,
        bakes: after.overlay_bakes_total - before.overlay_bakes_total,
        vertices: after.overlay_bake_vertices_total - before.overlay_bake_vertices_total,
        ops: after.overlay_bake_ops_total - before.overlay_bake_ops_total,
        cancelled: after.overlay_bakes_cancelled_total - before.overlay_bakes_cancelled_total,
        // RE-MEASURED, not remembered. The slice is the same leaked slice
        // every window, so this is constant by construction -- and
        // `summarise` asserts it anyway, because "constant by
        // construction" is what the last two artifacts in this area also
        // looked like.
        payload: (features.len(), features.coordinates()),
    }
}

fn line(w: &Window) -> String {
    format!(
        "  window {}  pushes {:>4}  sync mean {:>8.3} ms  max {:>8.3} ms  \
         bakes {:>4}  vertices/bake {:>9.0}  ops/bake {:>7.1}  cancelled {:>4}",
        w.index,
        w.pushes,
        w.mean_ms,
        w.max_ms,
        w.bakes,
        w.vertices_per_bake(),
        w.ops_per_bake(),
        w.cancelled,
    )
}

/// One arm's means, named rather than positional: five arms times five
/// numbers is exactly where a tuple index starts pointing at the wrong
/// column.
#[derive(Clone, Copy)]
struct ArmMeans {
    arm: Arm,
    features: usize,
    mean_ms: f64,
    vertices_per_bake: f64,
    ops_per_bake: f64,
    bakes: u64,
}

fn summarise(report: &[Window]) {
    println!("\n=== per-arm means over disjoint windows ===");
    let mut arms: Vec<ArmMeans> = Vec::new();
    for arm in [
        Arm::Shared,
        Arm::SourcesWithStops,
        Arm::SourcesOnly,
        Arm::ResolvedColours,
        Arm::DataDriven,
    ] {
        let windows: Vec<&Window> = report.iter().filter(|w| w.arm == arm).collect();
        if windows.is_empty() {
            continue;
        }

        // The payload-constancy assertion, per arm. Asserted rather than
        // assumed: a payload that grew between windows would make a rising
        // cost look like a leak, and a payload that shrank would hide one.
        let payload = windows[0].payload;
        for window in &windows {
            assert_eq!(
                window.payload, payload,
                "{} window {} pushed {:?} where window 0 pushed {:?} -- \
                 the arms are no longer comparable",
                arm.label(),
                window.index,
                window.payload,
                payload,
            );
        }

        let mean = |of: fn(&Window) -> f64| {
            windows.iter().map(|w| of(w)).sum::<f64>() / windows.len() as f64
        };
        let means = ArmMeans {
            arm,
            features: payload.0,
            mean_ms: mean(|w| w.mean_ms),
            vertices_per_bake: mean(Window::vertices_per_bake),
            ops_per_bake: mean(Window::ops_per_bake),
            bakes: windows.iter().map(|w| w.bakes).sum(),
        };
        println!(
            "  {:<14} payload {:>5} features / {:>8} coords   sync mean {:>8.3} ms   \
             vertices per bake {:>10.0}   ops per bake {:>8.1}   bakes {}",
            arm.label(),
            payload.0,
            payload.1,
            means.mean_ms,
            means.vertices_per_bake,
            means.ops_per_bake,
            means.bakes,
        );
        arms.push(means);
    }

    let find = |wanted: Arm| arms.iter().find(|means| means.arm == wanted).copied();
    let ratio = |a: f64, b: f64| if b > 0.0 { a / b } else { f64::NAN };

    if let (Some(resolved), Some(data)) = (find(Arm::ResolvedColours), find(Arm::DataDriven)) {
        println!("\n=== the data-driven acceptance condition ===");
        println!(
            "  the same vehicles in twelve route colours, as {} resolved-paint features \
             against {} features in ONE data-driven set:",
            resolved.features, data.features,
        );
        println!(
            "    ops per bake       {:.1} resolved / {:.1} data-driven  -> {:.1}x fewer",
            resolved.ops_per_bake,
            data.ops_per_bake,
            ratio(resolved.ops_per_bake, data.ops_per_bake),
        );
        println!(
            "    vertices per bake  {:.0} resolved / {:.0} data-driven  -> {:.3}x",
            resolved.vertices_per_bake,
            data.vertices_per_bake,
            ratio(resolved.vertices_per_bake, data.vertices_per_bake),
        );
        println!(
            "    sync mean          {:.3} ms resolved / {:.3} ms data-driven  -> {:.2}x",
            resolved.mean_ms,
            data.mean_ms,
            ratio(resolved.mean_ms, data.mean_ms),
        );
        println!(
            "  PASS means the ops collapsed to the COLOUR count while the vertices did NOT \
             move -- geometry is geometry, and a merge that changed the vertex count would \
             have changed what is drawn."
        );
    }

    let (Some(shared), Some(with), Some(without)) = (
        find(Arm::Shared),
        find(Arm::SourcesWithStops),
        find(Arm::SourcesOnly),
    ) else {
        return;
    };

    println!("\n=== the per-source acceptance condition ===");
    println!("  pushing the vehicles WITH the static stop source present, against WITHOUT it:");
    println!(
        "    sync mean          {:.3} ms with / {:.3} ms without  -> {:.2}x",
        with.mean_ms,
        without.mean_ms,
        ratio(with.mean_ms, without.mean_ms)
    );
    println!(
        "    vertices per bake  {:.0} with / {:.0} without  -> {:.2}x",
        with.vertices_per_bake,
        without.vertices_per_bake,
        ratio(with.vertices_per_bake, without.vertices_per_bake)
    );
    println!(
        "  and the same push under ONE shared collection, which is what the API forced before:"
    );
    println!(
        "    sync mean          {:.3} ms  -> {:.2}x the sources cost",
        shared.mean_ms,
        ratio(shared.mean_ms, without.mean_ms)
    );
    println!(
        "    vertices per bake  {:.0}  -> {:.2}x the sources cost",
        shared.vertices_per_bake,
        ratio(shared.vertices_per_bake, without.vertices_per_bake)
    );
    println!(
        "\n  PASS means the two `sources` rows agree and the `shared` row does not. \
         A ratio near 1.00 on the first two lines is the defect being gone; the \
         `shared` ratio is how much the stops were costing the buses."
    );
}
