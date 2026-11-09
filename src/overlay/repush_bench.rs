//! A headless re-push harness: one line source re-pushed through
//! `set_source` at a fixed rate while frames are drawn at a fixed rate,
//! on the whole-source path or the tiled path, with the frame thread's
//! work split by phase. `probe` only; driven by `examples/repush_bench.rs`.
//!
//! It runs the pipeline `MapView` runs, step for step, with the same
//! functions and the same thread placement:
//!
//! | step | whole-source (untiled) | tiled |
//! |---|---|---|
//! | push (frame thread) | `OverlayStore::set_source`: parse, plan, hit index | same (no plan) |
//! | worker | `tessellate_overlay_plan_reusing` of the whole source | `OverlayHost::build_tile_set`, then per-tile `TileBake::run` |
//! | install (frame thread) | the drawn record's swap (old snapshot dropped here) | `install_tile_set`, `land_bake`, drawn swap |
//! | upload (frame thread) | `install_overlay_chunks` of the whole mesh | `OverlayGpu::prepare_tiles` |
//! | draw (frame thread) | encode + submit one render pass | same |
//!
//! Worker threads are a tokio blocking pool (`MapView`'s own runtime
//! kind), bakes are gated by the same permit counts, and a bake superseded
//! by a newer push is cancelled the way `bake_overlay_source`'s is. Frames
//! are paced in real time at `fps`, so a worker bake overlaps frames the
//! way it does in a window. The GPU wait after submit is timed on its own
//! (`gpu_wait`): a window's present blocks on it, but it is not frame
//! thread CPU.
//!
//! What it does NOT include: the basemap, labels, gpui's own layout, and
//! the window's present. Both paths share those, so they shift both
//! distributions equally and cannot explain a difference between them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::timing::Instant;

use crate::overlay::host::{install_overlay_chunks, OverlayChunkGpu, OverlayGpu, OverlayHost, TileBaked};
use crate::overlay::SourceId;

/// One run's shape.
#[derive(Debug, Clone)]
pub struct Config {
    pub routes: usize,
    pub points_per_route: usize,
    /// Pushes per second of (real, paced) time.
    pub rate_hz: f64,
    pub fps: f64,
    pub seconds: f64,
    /// Fraction of routes whose geometry moves on each push.
    pub change_fraction: f64,
    /// `true`: the tiled path (`force_tiled`, as
    /// `GPUI_MAP_OVERLAY_TILING=always` does); `false`: whole-source.
    pub tiled: bool,
    pub width: u32,
    pub height: u32,
    pub zoom: f64,
    pub seed: u64,
    /// Declare a data-driven `line-color` (`["get", "color"]`) over the
    /// features' properties, beside their per-feature width.
    pub declared_paint: bool,
    /// Re-run `set_source`'s parse, plan and hit-index steps off the frame
    /// clock after each push, to attribute its cost.
    pub attribute_push: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            routes: 500,
            points_per_route: 106,
            rate_hz: 2.5,
            fps: 30.0,
            seconds: 20.0,
            change_fraction: 0.05,
            tiled: false,
            width: 1280,
            height: 800,
            zoom: 11.5,
            seed: 1,
            declared_paint: false,
            attribute_push: true,
        }
    }
}

/// Frame-thread time in one frame, by phase, in nanoseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameSample {
    /// Everything on the frame thread (sum of the phases below plus glue).
    pub cpu: u64,
    /// `set_source` (only on a frame that pushed).
    pub push: u64,
    /// Landing worker results: drawn-record swaps, tile set installs,
    /// `land_bake`s -- and whatever they drop.
    pub install: u64,
    /// Whole-source chunk upload / `prepare_tiles`.
    pub upload: u64,
    /// Building the draw list, encoding and submitting the pass.
    pub draw: u64,
    /// Waiting for the GPU to finish the frame (not frame-thread CPU).
    pub gpu_wait: u64,
    /// Draw calls issued for the source this frame.
    pub draws: u64,
    /// Bytes of mesh uploaded this frame (whole-source path only).
    pub upload_bytes: u64,
}

/// A run's result.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub frames: Vec<FrameSample>,
    pub pushes: usize,
    /// Whole-source bakes / tile-set builds that landed.
    pub landed: usize,
    /// Worker-side time: whole-source tessellations or index builds + tile
    /// bakes, summed, in ns.
    pub worker_ns: u64,
    /// Tile bakes run (tiled path).
    pub tile_bakes: usize,
    /// Frames after the first painted one that drew nothing for the source.
    pub blank_frames_after_first_paint: usize,
    pub vertices_per_bake: usize,
    /// `set_source`'s steps, summed over `attributed` pushes, in ns.
    pub parse_ns: u64,
    pub plan_ns: u64,
    pub hit_ns: u64,
    pub attributed: usize,
}

/// A deterministic LCG, so two runs push the same bytes.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

/// `routes` polylines over Singapore, `points` coordinates each: a random
/// walk from a seeded start, as a bus route wanders.
pub fn synthetic_routes(routes: usize, points: usize, seed: u64) -> Vec<Vec<[f64; 2]>> {
    let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03);
    (0..routes)
        .map(|_| {
            let mut lon = 103.62 + rng.next() * 0.36;
            let mut lat = 1.26 + rng.next() * 0.18;
            let mut heading = rng.next() * std::f64::consts::TAU;
            (0..points)
                .map(|_| {
                    heading += (rng.next() - 0.5) * 0.8;
                    lon = (lon + heading.cos() * 0.0012).clamp(103.6, 104.05);
                    lat = (lat + heading.sin() * 0.0012).clamp(1.22, 1.47);
                    [lon, lat]
                })
                .collect()
        })
        .collect()
}

/// Moves `fraction` of the routes a little: what a push changes.
pub fn perturb(routes: &mut [Vec<[f64; 2]>], fraction: f64, push: u64) {
    let mut rng = Lcg(push.wrapping_mul(0xA24B_AED4_963E_E407) + 7);
    let n = ((routes.len() as f64) * fraction).ceil() as usize;
    for _ in 0..n {
        let r = (rng.next() * routes.len() as f64) as usize % routes.len();
        let (dx, dy) = ((rng.next() - 0.5) * 0.002, (rng.next() - 0.5) * 0.002);
        for p in &mut routes[r] {
            p[0] += dx;
            p[1] += dy;
        }
    }
}

/// The routes as a GeoJSON FeatureCollection, one LineString per route,
/// with a per-route colour and a constant width -- a transit layer.
pub fn routes_geojson(routes: &[Vec<[f64; 2]>]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(routes.iter().map(Vec::len).sum::<usize>() * 40 + 64);
    s.push_str(r#"{"type":"FeatureCollection","features":["#);
    for (i, route) in routes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let hue = (i * 47) % 255;
        let _ = write!(
            s,
            r##"{{"type":"Feature","id":{i},"properties":{{"line-color":"#{hue:02x}{:02x}{:02x}","color":"#{hue:02x}{:02x}{:02x}","line-width":3}},"geometry":{{"type":"LineString","coordinates":["##,
            255 - hue,
            (hue * 3) % 255,
            255 - hue,
            (hue * 3) % 255
        );
        for (j, p) in route.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            let _ = write!(s, "[{:.6},{:.6}]", p[0], p[1]);
        }
        s.push_str("]}}");
    }
    s.push_str("]}");
    s
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A finished whole-source bake.
struct Baked {
    generation: u64,
    mesh: crate::render::TileMesh,
    extras: crate::render::mesh::OverlayExtras,
    origin: crate::overlay::geom::OverlayOrigin,
    bracket: crate::overlay::plan::BakeBracket,
    took: Duration,
}

/// What the whole-source path draws: `MapView`'s `DrawnOverlay` +
/// `DrawnOverlayMesh` + `OverlaySourceGpu`, reduced to what costs.
struct WholeSource {
    snapshot: Option<crate::overlay::OverlaySnapshot>,
    mesh: Option<(crate::render::TileMesh, crate::render::mesh::OverlayExtras)>,
    origin: Option<crate::overlay::geom::OverlayOrigin>,
    bracket: Option<crate::overlay::plan::BakeBracket>,
    generation: u64,
    uploaded: u64,
    chunks: Vec<OverlayChunkGpu>,
}

/// Runs one configuration. `Err` when there is no headless device.
pub fn run(config: &Config) -> anyhow::Result<Report> {
    let ctx = pollster::block_on(crate::render::GpuContext::new_headless())?;
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let pipeline = crate::render::TilePipeline::new(&ctx, format);
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let (w, h) = (config.width, config.height);
    let camera = crate::render::Camera {
        center_lon: 103.83,
        center_lat: 1.35,
        zoom: config.zoom,
        pitch: 0.0,
        bearing: 0.0,
        viewport_width_px: w as f32,
        viewport_height_px: h as f32,
        fov_y: crate::render::Camera::DEFAULT_FOV_Y,
    };
    let display_zoom = camera.zoom.floor() as u8;
    let cover = crate::geo::visible_tiles((camera.center_lon, camera.center_lat), display_zoom, (f64::from(w), f64::from(h)));
    let globe = crate::render::globe::GlobeUpload::new(
        &crate::render::globe::globe_frame(&camera),
        &camera,
        4096.0,
        glam::DVec3::Z,
        1.0,
    );
    let color = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("repush-bench-color"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_view = pipeline.create_depth_view(&ctx, w, h);
    let (_camera_buffer, camera_group) =
        pipeline.create_camera_bind_group(&ctx, camera.view_projection().to_cols_array_2d(), camera.zoom as f32, 1.0);
    let (model_buffer, model_group) = pipeline.create_model_bind_group(
        &ctx,
        glam::Mat4::IDENTITY.to_cols_array_2d(),
        0.0,
        0.0,
        crate::render::GlobeParams::FLAT,
    );

    let name = "routes";
    let mut store = crate::overlay::OverlayStore::new();
    if config.declared_paint {
        let paint: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r#"{"line-color": ["get", "color"]}"#,
        )?;
        store.declare_source_paint(name, &paint).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let mut host = OverlayHost::new(crate::overlay::host::overlay_tile_bake_permits());
    let mut overlay_gpu = OverlayGpu::default();
    let mut whole = WholeSource {
        snapshot: None,
        mesh: None,
        origin: None,
        bracket: None,
        generation: 0,
        uploaded: 0,
        chunks: Vec::new(),
    };
    let permits = if std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get) >= 6 { 3 } else { 2 };
    let (bake_tx, bake_rx) = mpsc::channel::<Baked>();
    let (build_tx, build_rx) =
        mpsc::channel::<(u64, crate::overlay::plan::BakeBracket, Option<crate::overlay::host::TileSetBuild>, Instant)>();
    let (tile_tx, tile_rx) = mpsc::channel::<(TileBaked, Duration)>();
    let in_flight = Arc::new(AtomicU64::new(0));
    let mut pending_bake: Option<(u64, crate::overlay::plan::OverlayPlan)> = None;

    let mut routes = synthetic_routes(config.routes, config.points_per_route, config.seed);
    let mut next_json = routes_geojson(&routes);
    let mut report = Report::default();
    let frame_period = Duration::from_secs_f64(1.0 / config.fps);
    let push_period = Duration::from_secs_f64(1.0 / config.rate_hz);
    let frames = (config.seconds * config.fps).round() as usize;
    let start = crate::timing::Instant::now();
    let mut next_push_at = Duration::ZERO;
    let mut painted = false;
    let order: Vec<SourceId> = vec![name.into()];

    for frame in 0..frames {
        let slot = frame_period * u32::try_from(frame).unwrap_or(u32::MAX);
        if let Some(wait) = slot.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
        let mut sample = FrameSample::default();
        let t_frame = crate::timing::Instant::now();

        // PUSH -- what the consumer's `set_source` call costs the thread.
        if start.elapsed() >= next_push_at {
            next_push_at += push_period;
            let t = crate::timing::Instant::now();
            store.set_source(name, &next_json, display_zoom).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            if config.tiled {
                store.force_tiled(name, display_zoom);
            }
            let snapshot = store.source_snapshot(name).expect("just pushed");
            let generation = snapshot.generation;
            if snapshot.tiled {
                // `build_overlay_tiles`: the index on a worker.
                let features = Arc::clone(&snapshot.features);
                let properties = Arc::clone(&snapshot.properties);
                let paint = Arc::clone(&snapshot.paint);
                let bracket = crate::overlay::plan::zoom_dependence(&snapshot.features)
                    .unwrap_or(crate::overlay::plan::BakeBracket::Bucket(display_zoom));
                let live = store.source_live_generation(name).expect("live");
                let tx = build_tx.clone();
                let pushed_at = crate::timing::Instant::now();
                drop(runtime.spawn_blocking(move || {
                    let cancel = || generation < live.load(Ordering::Relaxed);
                    let built = OverlayHost::build_tile_set(
                        &features,
                        &properties,
                        &paint,
                        None,
                        Default::default(),
                        bracket,
                        &cancel,
                    )
                    .ok()
                    .flatten();
                    let _ = tx.send((generation, bracket, built, pushed_at));
                }));
            } else if let Some(plan) = store.source_plan(name).cloned() {
                pending_bake = Some((generation, plan));
            }
            drop(snapshot);
            sample.push = nanos(t.elapsed());
            report.pushes += 1;
            // The consumer's next payload, built off the clock.
            let t_off = crate::timing::Instant::now();
            // Attribution of `set_source`, off the frame clock: its three
            // big steps, re-run on the same input.
            if config.attribute_push {
                let t = crate::timing::Instant::now();
                let parsed = crate::overlay::geojson::parse_feature_collection(&next_json);
                report.parse_ns += nanos(t.elapsed());
                drop(parsed);
                let snap = store.source_snapshot(name).expect("just pushed");
                let bracket = crate::overlay::plan::zoom_dependence(&snap.features)
                    .unwrap_or(crate::overlay::plan::BakeBracket::Bucket(display_zoom));
                let t = crate::timing::Instant::now();
                let plan = crate::overlay::source_plan_for(&snap.features, &snap.properties, bracket, &snap.paint, false);
                report.plan_ns += nanos(t.elapsed());
                drop(plan);
                let t = crate::timing::Instant::now();
                let hit = crate::overlay::hit::HitIndex::build(&snap.source, &snap.features);
                report.hit_ns += nanos(t.elapsed());
                drop(hit);
                report.attributed += 1;
            }
            perturb(&mut routes, config.change_fraction, report.pushes as u64);
            next_json = routes_geojson(&routes);
            let off = t_off.elapsed();
            // Not frame-thread engine work; subtracted below.
            sample.cpu = sample.cpu.wrapping_sub(nanos(off));
        }

        // Whole-source bake start: `bake_overlay_source`'s permit, and its
        // "superseded before it started" cancel.
        if in_flight.load(Ordering::Relaxed) < permits {
            if let Some((generation, plan)) = pending_bake.take() {
                let live = store.source_live_generation(name).expect("live");
                if generation >= live.load(Ordering::Relaxed) {
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    let tx = bake_tx.clone();
                    let flight = Arc::clone(&in_flight);
                    drop(runtime.spawn_blocking(move || {
                        let t = crate::timing::Instant::now();
                        let cancel = || generation < live.load(Ordering::Relaxed);
                        let baked = crate::render::mesh::tessellate_overlay_plan_reusing(
                            &plan.plan,
                            &plan.fill_outlines,
                            crate::render::subdivide::OVERLAY_SUBDIVISION_EXTENT,
                            plan.origin.coord_scale,
                            crate::render::subdivide::Subdivision::OVERLAY,
                            &cancel,
                            crate::render::TileMesh::default(),
                        );
                        flight.fetch_sub(1, Ordering::Relaxed);
                        if let Ok((mesh, _ranges, extras)) = baked {
                            let _ = tx.send(Baked {
                                generation,
                                mesh,
                                extras,
                                origin: plan.origin,
                                bracket: plan.bracket,
                                took: t.elapsed(),
                            });
                        }
                    }));
                }
            }
        }

        // INSTALL -- worker results landing on the frame thread.
        let t = crate::timing::Instant::now();
        let current = store.source_generation(name).unwrap_or(0);
        while let Ok(baked) = bake_rx.try_recv() {
            report.worker_ns += nanos(baked.took);
            if baked.generation != current {
                drop(runtime.spawn_blocking(move || drop(baked)));
                continue;
            }
            report.landed += 1;
            report.vertices_per_bake = baked.mesh.vertices.len();
            // `install_overlay_mesh`: the drawn record's snapshot and mesh
            // swap; the displaced mesh goes to a worker, the displaced
            // snapshot drops HERE, as it does in `MapView`.
            let displaced = whole.mesh.replace((baked.mesh, baked.extras));
            whole.snapshot = store.source_snapshot(name);
            whole.origin = Some(baked.origin);
            whole.bracket = Some(baked.bracket);
            whole.generation = baked.generation;
            if let Some(displaced) = displaced {
                drop(runtime.spawn_blocking(move || drop(displaced)));
            }
        }
        while let Ok((generation, bracket, built, pushed_at)) = build_rx.try_recv() {
            let Some(built) = built else { continue };
            report.worker_ns += nanos(built.took);
            if generation != current {
                continue;
            }
            report.landed += 1;
            store.adopt_tile_index(name, Arc::clone(&built.index));
            let live = store.source_live_generation(name).expect("live");
            host.install_tile_set(&order[0], generation, bracket, built, live, pushed_at);
            whole.snapshot = store.source_snapshot(name);
        }
        while let Ok((baked, took)) = tile_rx.try_recv() {
            report.worker_ns += nanos(took);
            report.tile_bakes += 1;
            host.land_bake(baked);
        }
        if config.tiled {
            // `ensure_overlay_tiles_requested` + `start_overlay_bakes`.
            host.request_tiles(Some(&overlay_gpu), &cover);
            while let Some(bake) = host.next_bake() {
                let tx = tile_tx.clone();
                drop(runtime.spawn_blocking(move || {
                    let t = crate::timing::Instant::now();
                    let baked = bake.run();
                    let _ = tx.send((baked, t.elapsed()));
                }));
            }
        }
        sample.install = nanos(t.elapsed());

        // UPLOAD.
        let t = crate::timing::Instant::now();
        let tile_frame = if config.tiled {
            let rt = runtime.handle().clone();
            Some(overlay_gpu.prepare_tiles(&mut host, &ctx, &pipeline, &camera, &globe, &order, &cover, |baked| {
                drop(rt.spawn_blocking(move || drop(baked)))
            }))
        } else {
            if whole.uploaded != whole.generation {
                if let Some((mesh, extras)) = &whole.mesh {
                    sample.upload_bytes = (mesh.vertices.len() * std::mem::size_of::<crate::render::Vertex>()
                        + mesh.indices.len() * 4) as u64;
                    let old = std::mem::take(&mut whole.chunks);
                    whole.chunks = install_overlay_chunks(&pipeline, &ctx, old, mesh, extras).unwrap_or_default();
                    whole.uploaded = whole.generation;
                    // `release_cpu_mesh`: a `Ramp` mesh's CPU copy goes to
                    // a worker once the GPU has it.
                    if matches!(whole.bracket, Some(crate::overlay::plan::BakeBracket::Ramp { .. })) {
                        if let Some(released) = whole.mesh.take() {
                            drop(runtime.spawn_blocking(move || drop(released)));
                        }
                    }
                }
            }
            if let Some(origin) = whole.origin {
                pipeline.write_model_uniform(
                    &ctx,
                    &model_buffer,
                    crate::render::combined_overlay_matrix(&camera, (origin.world_x, origin.world_y)).to_cols_array_2d(),
                    whole.bracket.map_or(0.0, |b| b.blend_t(camera.zoom)),
                    crate::render::overlay_dash_scale(&camera),
                    globe.for_overlay((origin.world_x, origin.world_y)),
                );
            }
            None
        };
        sample.upload = nanos(t.elapsed());

        // DRAW.
        let t = crate::timing::Instant::now();
        let mut draws = Vec::new();
        if let Some(tile_frame) = &tile_frame {
            tile_frame.push_tile_draws(name, &mut draws);
        } else {
            for chunk in &whole.chunks {
                draws.push(crate::render::TileDraw::Legacy(&chunk.buffers, &model_group));
            }
        }
        sample.draws = draws.len() as u64;
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        pipeline.render_with_space_and_raster(
            &mut encoder,
            &color_view,
            &depth_view,
            wgpu::Color::BLACK,
            &camera_group,
            &crate::render::SpaceDraw::NONE,
            None,
            &draws,
            None,
        );
        drop(draws);
        ctx.queue.submit(Some(encoder.finish()));
        sample.draw = nanos(t.elapsed());
        let t = crate::timing::Instant::now();
        let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
        sample.gpu_wait = nanos(t.elapsed());
        sample.cpu = sample.cpu.wrapping_add(nanos(t_frame.elapsed()) - sample.gpu_wait);
        if painted && sample.draws == 0 {
            report.blank_frames_after_first_paint += 1;
        }
        painted |= sample.draws > 0;
        report.frames.push(sample);
    }
    // Let in-flight work finish before the runtime drops.
    runtime.shutdown_timeout(Duration::from_secs(10));
    Ok(report)
}

/// The `p`-th percentile (0..=100) of `values`, nearest-rank.
pub fn percentile(values: &[u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}
