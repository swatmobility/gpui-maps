//! Origin-destination arcs (`render::arcs`) at Kepler's scale, headless:
//! the perf gate's `arcs.*` section.
//!
//! ```text
//! cargo run --release --example arcs -- --measure            # seeded synthetic, the gate
//! cargo run --release --example arcs -- --measure-real CSV   # Kepler's ukcommute data.csv
//! ```
//!
//! `--measure` is SYNTHETIC and seeded (nothing fetched): 679,643 arcs
//! between 7,200 points clustered like England and Wales' MSOA centroids
//! (most commutes a few km, a tail to London), as the "Commute Pattern"
//! sample has them; and 97,986 short arcs over Manhattan for the taxi
//! sample. Each view is drawn into a 2560 x 1600 target (1280 x 800 at
//! 2x) at the sample's own camera, additive as Kepler blends it, and timed
//! on the GPU (timestamp queries, when the adapter has them) and on the
//! wall (submit to idle). Also printed: the same view blended "over", and
//! with deck.gl's fixed 50 segments instead of the per-class counts, which
//! is what the length classes save.
//!
//! Also timed: picking (`render::arc_pick`) at the UK camera -- the index
//! built once for the set (`arcs.pick_index_ms`), then one pick per cursor
//! on a 48 x 30 grid over the view, p50 and p99 (`arcs.pick_ms.p50`,
//! `.p99`), how many of those cursors hit an arc, and a checksum of WHICH
//! arc each picked (`arcs.uk.pick.checksum`, exact: every cursor hits at
//! this camera, so only the checksum sees a changed pick).

use std::time::Instant;

use gpui_maps::overlay::arcs::{build, ArcColors, ArcWidths};
use gpui_maps::render::arcs::{ArcBlend, ArcFrame, ArcLayerDraw, ArcPipeline, ArcSet, ArcStyle};
use gpui_maps::render::gpu::GpuContext;
use gpui_maps::render::Camera;
use gpui_maps::style::Rgba;

const W: u32 = 2560;
const H: u32 = 1600;
const SCALE: f32 = 2.0;
const FRAMES: usize = 30;

fn gate(name: &str, value: f64, unit: &str) {
    println!("gate: arcs.{name} {value} {unit}");
}

fn round(v: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (v * k).round() / k
}

/// xorshift64*: seeded, portable.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.next().max(1e-12), self.next());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// England and Wales' larger towns, roughly (lon, lat, weight).
const TOWNS: &[(f64, f64, f64)] = &[
    (-0.12, 51.51, 30.0),
    (-1.90, 52.48, 8.0),
    (-2.24, 53.48, 8.0),
    (-1.55, 53.80, 6.0),
    (-2.98, 53.41, 5.0),
    (-1.47, 53.38, 4.0),
    (-2.59, 51.45, 4.0),
    (-1.61, 54.97, 4.0),
    (-1.15, 52.95, 3.0),
    (-1.13, 52.63, 3.0),
    (-3.18, 51.48, 3.0),
    (-1.40, 50.90, 3.0),
    (-4.14, 50.37, 2.0),
    (-1.26, 51.75, 2.0),
    (0.12, 52.20, 2.0),
    (-1.08, 53.96, 2.0),
    (-2.10, 52.59, 2.0),
    (-3.94, 51.62, 2.0),
    (1.30, 52.63, 2.0),
    (-0.97, 51.45, 2.0),
];

fn uk_synthetic() -> (Vec<[f64; 2]>, Vec<[f64; 2]>) {
    let mut rng = Rng(0x5eed_a5c5);
    let total: f64 = TOWNS.iter().map(|t| t.2).sum();
    let pick = |rng: &mut Rng| {
        let mut x = rng.next() * total;
        for t in TOWNS {
            x -= t.2;
            if x <= 0.0 {
                return *t;
            }
        }
        TOWNS[0]
    };
    // 7,200 centroids: half in towns, half spread over the country.
    let msoa: Vec<[f64; 2]> = (0..7200)
        .map(|i| {
            if i % 2 == 0 {
                let t = pick(&mut rng);
                [t.0 + rng.normal() * 0.12, t.1 + rng.normal() * 0.08]
            } else {
                [-5.0 + rng.next() * 6.5, 50.3 + rng.next() * 5.0]
            }
        })
        .collect();
    let (mut source, mut target) = (Vec::with_capacity(679_643), Vec::with_capacity(679_643));
    for _ in 0..679_643 {
        let home = msoa[(rng.next() * msoa.len() as f64) as usize];
        let work = if rng.next() < 0.08 {
            let t = pick(&mut rng);
            [t.0 + rng.normal() * 0.03, t.1 + rng.normal() * 0.02]
        } else {
            // A few km, exponential.
            let d = -(rng.next().max(1e-9)).ln() * 0.08;
            let a = rng.next() * std::f64::consts::TAU;
            [home[0] + d * a.cos() * 1.6, home[1] + d * a.sin()]
        };
        source.push(home);
        target.push(work);
    }
    (source, target)
}

fn taxi_synthetic() -> (Vec<[f64; 2]>, Vec<[f64; 2]>, Vec<f64>) {
    let mut rng = Rng(0x7a41);
    let point = |rng: &mut Rng| {
        // Manhattan's spine, roughly: a tilted strip.
        let t = rng.next();
        let w = rng.normal() * 0.006;
        [-74.01 + t * 0.07 + w, 40.70 + t * 0.10 - w * 0.6]
    };
    let (mut s, mut d, mut times) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..97_986 {
        s.push(point(&mut rng));
        d.push(point(&mut rng));
        times.push(1.421e12 + f64::from(i) * 500.0);
    }
    (s, d, times)
}

fn camera(lon: f64, lat: f64, zoom: f64, pitch: f32, bearing: f32) -> Camera {
    Camera {
        center_lon: lon,
        center_lat: lat,
        zoom,
        pitch: pitch.to_radians(),
        bearing: bearing.to_radians(),
        viewport_width_px: W as f32 / SCALE,
        viewport_height_px: H as f32 / SCALE,
        fov_y: Camera::DEFAULT_FOV_Y,
    }
}

fn device() -> GpuContext {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })
            .await
            .expect("an adapter");
        let required_features = adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor { required_features, required_limits: adapter.limits(), ..Default::default() })
            .await
            .expect("a device");
        let max_buffer_size = device.limits().max_buffer_size;
        GpuContext { device, queue, max_buffer_size }
    })
}

struct Timed {
    gpu_ms: Option<f64>,
    frame_ms: f64,
    vertices: u64,
    draws: u64,
}

/// Draws `set` FRAMES times over a dark clear; the median GPU pass time and
/// submit-to-idle wall time.
fn time_view(ctx: &GpuContext, cam: &Camera, set: &ArcSet, style: ArcStyle, window: Option<[f64; 2]>) -> Timed {
    let format = wgpu::TextureFormat::Bgra8Unorm;
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("arcs-measure"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let globe = gpui_maps::render::globe::globe_frame(cam).flattened();
    let frame = ArcFrame::new(cam, &globe, SCALE);
    let mut pipeline = ArcPipeline::new(ctx, format, None);
    let stamps = ctx.device.features().contains(wgpu::Features::TIMESTAMP_QUERY);
    let queries = stamps.then(|| ctx.device.create_query_set(&wgpu::QuerySetDescriptor { label: None, ty: wgpu::QueryType::Timestamp, count: 2 }));
    let resolve = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 16,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let read = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 16,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let period = f64::from(ctx.queue.get_timestamp_period());
    let (mut gpu, mut wall) = (Vec::new(), Vec::new());
    for i in 0..FRAMES + 3 {
        let filter = window.and_then(|w| set.relative_window(w));
        pipeline.prepare(ctx, &frame, &[ArcLayerDraw { id: "m", generation: 1, set, style, filter }]);
        let mut encoder = ctx.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("arcs-measure-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.1, g: 0.11, b: 0.13, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: queries.as_ref().map(|q| wgpu::RenderPassTimestampWrites {
                    query_set: q,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pipeline.draw(&mut pass);
        }
        if let Some(q) = queries.as_ref() {
            encoder.resolve_query_set(q, 0..2, &resolve, 0);
            encoder.copy_buffer_to_buffer(&resolve, 0, &read, 0, 16);
        }
        let t = Instant::now();
        ctx.queue.submit(Some(encoder.finish()));
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if stamps {
            read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            let v: Vec<u64> = {
                let data = read.slice(..).get_mapped_range().expect("mapped");
                bytemuck::cast_slice::<u8, u64>(&data).to_vec()
            };
            read.unmap();
            if i >= 3 {
                gpu.push(v[1].saturating_sub(v[0]) as f64 * period / 1e6);
            }
        }
        if i >= 3 {
            wall.push(ms);
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let stats = pipeline.stats();
    Timed { gpu_ms: (!gpu.is_empty()).then(|| median(&mut gpu)), frame_ms: median(&mut wall), vertices: stats.vertices, draws: stats.draws }
}

const KEPLER_UK: (f64, f64, f64, f32, f32) = (-2.6899063817571394, 52.253971373306165, 6.655984704565685, 49.184_406, -4.928_571);
const KEPLER_TAXI: (f64, f64, f64, f32, f32) = (-73.97755351753737, 40.739516322561585, 12.092072468526053, 56.012_527, -15.25);

fn uk_style() -> ArcStyle {
    // Kepler: opacity 0.3, additive.
    ArcStyle { opacity: 0.3, blend: ArcBlend::Additive, ..ArcStyle::default() }
}

fn uk_colors() -> ArcColors {
    let c = |r: u8, g: u8, b: u8| Rgba { r: f32::from(r) / 255.0, g: f32::from(g) / 255.0, b: f32::from(b) / 255.0, a: 1.0 };
    ArcColors::Constant { source: c(228, 155, 0), target: c(149, 12, 105) }
}

/// Kepler's arc `thickness` in px: `thickness * 2/3`
/// (`PROJECTED_PIXEL_SIZE_MULTIPLIER`).
fn kepler_width(thickness: f32) -> f32 {
    thickness * 2.0 / 3.0
}

fn report(ctx: &GpuContext, name: &str, cam: &Camera, set: &ArcSet, style: ArcStyle, window: Option<[f64; 2]>, graded: bool) {
    let t = time_view(ctx, cam, set, style, window);
    println!(
        "arcs {name}: {} arcs, {} draws, {:.1} M vertices; gpu {} ms, frame {:.2} ms",
        set.len(),
        t.draws,
        t.vertices as f64 / 1e6,
        t.gpu_ms.map_or("n/a".into(), |g| format!("{g:.2}")),
        t.frame_ms
    );
    if graded {
        // Submit to idle for the one pass: the GPU's time for the frame's
        // arcs (Metal's pass timestamps read 0 on some frames, so they are
        // printed above but not graded).
        gate(&format!("{name}.frame_ms"), round(t.frame_ms, 2), "ms");
        gate(&format!("{name}.vertices"), t.vertices as f64, "count");
        gate(&format!("{name}.draws"), t.draws as f64, "count");
    }
}

/// The pick at `cam` over `set`: the index's build, then one pick per
/// cursor of a 48 x 30 grid across the view (logical px), each timed.
fn pick_report(cam: &Camera, set: &ArcSet, style: ArcStyle) {
    use gpui_maps::render::arc_pick::{pick, ArcPickIndex};
    let started = Instant::now();
    let index = ArcPickIndex::new(set);
    let index_ms = started.elapsed().as_secs_f64() * 1e3;
    let frame = ArcFrame::new(cam, &gpui_maps::render::globe::globe_frame(cam).flattened(), SCALE);
    let (w, h) = (f64::from(cam.viewport_width_px), f64::from(cam.viewport_height_px));
    let mut times = Vec::new();
    let mut hits = 0u32;
    // Which arc each cursor picked, folded in cursor order: every cursor
    // hits at this camera, so the count alone could not see a pick change.
    let mut checksum = 0u64;
    for gy in 0..30 {
        for gx in 0..48 {
            let (x, y) = ((f64::from(gx) + 0.5) * w / 48.0, (f64::from(gy) + 0.5) * h / 30.0);
            let started = Instant::now();
            let hit = pick(&frame, set, &index, &style, None, x, y, 3.0);
            times.push(started.elapsed().as_secs_f64() * 1e3);
            hits += u32::from(hit.is_some());
            let k = (gy * 48 + gx + 1) as u64;
            checksum = (checksum + k * hit.map_or(0, |h| h.index as u64 + 1)) % 1_000_000_007;
        }
    }
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
    println!(
        "arcs uk pick: index {index_ms:.1} ms ({:.1} MB, level {}), {} picks p50 {:.3} ms p99 {:.3} ms max {:.3} ms, {hits} hit",
        index.bytes() as f64 / 1e6,
        index.level(),
        times.len(),
        at(0.5),
        at(0.99),
        times[times.len() - 1]
    );
    gate("pick_index_ms", round(index_ms, 1), "ms");
    gate("pick_ms.p50", round(at(0.5), 3), "ms");
    gate("pick_ms.p99", round(at(0.99), 3), "ms");
    gate("uk.pick.hit_hits", f64::from(hits), "count");
    gate("uk.pick.checksum", checksum as f64, "count");
}

fn measure() {
    let ctx = device();
    let (s, t) = uk_synthetic();
    let started = Instant::now();
    let uk = build(&s, &t, &uk_colors(), &ArcWidths::Constant(kepler_width(0.3)), None);
    let build_ms = started.elapsed().as_secs_f64() * 1e3;
    gate("build_ms", round(build_ms, 1), "ms");
    gate("uk.arcs", uk.len() as f64, "count");
    gate("upload_kb", (uk.upload_bytes() / 1024) as f64, "KB");
    let k = KEPLER_UK;
    let cam = camera(k.0, k.1, k.2, k.3, k.4);
    report(&ctx, "uk", &cam, &uk, uk_style(), None, true);
    pick_report(&cam, &uk, uk_style());
    report(&ctx, "uk_over", &cam, &uk, ArcStyle { blend: ArcBlend::Normal, ..uk_style() }, None, false);
    // The fixed-50 comparison: every class at deck's count.
    let fixed = ArcSet { buckets: vec![gpui_maps::render::arcs::ArcBucket { class: 0, range: 0..uk.len() as u32 }], ..uk.clone() };
    report(&ctx, "uk_deck50", &cam, &fixed, uk_style(), None, false);
    let wide = build(&s, &t, &uk_colors(), &ArcWidths::Constant(3.0), None);
    report(&ctx, "uk_3px", &cam, &wide, uk_style(), None, false);

    let (s, d, times) = taxi_synthetic();
    let taxi = build(&s, &d, &uk_colors(), &ArcWidths::Constant(kepler_width(1.0)), Some(("t", &times, true)));
    let k = KEPLER_TAXI;
    let cam = camera(k.0, k.1, k.2, k.3, k.4);
    let style = ArcStyle { opacity: 0.37, blend: ArcBlend::Additive, ..ArcStyle::default() };
    report(&ctx, "taxi", &cam, &taxi, style, None, true);
    let window = [1.421e12, 1.421e12 + 97_986.0 * 500.0 * 0.3];
    report(&ctx, "taxi_window", &cam, &taxi, style, Some(window), false);
}

fn measure_real(path: &str) {
    let ctx = device();
    let bytes = std::fs::read(path).expect("the CSV");
    let started = Instant::now();
    let options = gpui_maps::overlay::csv::CsvOptions { lat: Some("residence_lat".into()), lon: Some("residence_lng".into()), ..Default::default() };
    let csv = gpui_maps::overlay::csv::CsvSource::from_bytes(&bytes, &options).expect("parses");
    let parse_ms = started.elapsed().as_secs_f64() * 1e3;
    let source = csv.points().expect("points").to_vec();
    let target = gpui_maps::overlay::arcs::csv_positions(&csv, "workplace_lng", "workplace_lat").expect("workplace columns");
    let started = Instant::now();
    let uk = build(&source, &target, &uk_colors(), &ArcWidths::Constant(kepler_width(0.3)), None);
    println!("real ukcommute: {} rows parsed in {parse_ms:.0} ms, {} arcs built in {:.0} ms", csv.len(), uk.len(), started.elapsed().as_secs_f64() * 1e3);
    let classes: Vec<String> = uk.buckets.iter().map(|b| format!("{}:{}", b.class, b.range.end - b.range.start)).collect();
    println!("length classes: {}", classes.join(" "));
    let k = KEPLER_UK;
    let cam = camera(k.0, k.1, k.2, k.3, k.4);
    report(&ctx, "real_uk", &cam, &uk, uk_style(), None, false);
    report(&ctx, "real_uk_over", &cam, &uk, ArcStyle { blend: ArcBlend::Normal, ..uk_style() }, None, false);
    let fixed = ArcSet { buckets: vec![gpui_maps::render::arcs::ArcBucket { class: 0, range: 0..uk.len() as u32 }], ..uk.clone() };
    report(&ctx, "real_uk_deck50", &cam, &fixed, uk_style(), None, false);
    let cam = camera(-0.12, 51.5, 9.0, 49.0, 0.0);
    report(&ctx, "real_london_z9", &cam, &uk, uk_style(), None, false);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--measure") => measure(),
        Some("--measure-real") => measure_real(args.get(1).expect("--measure-real CSV")),
        _ => eprintln!("usage: arcs --measure | --measure-real CSV"),
    }
}
