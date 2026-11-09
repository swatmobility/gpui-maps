//! Animated trips (`render::trips`) at the world-flights sample's scale,
//! headless: the perf gate's `trips.*` section.
//!
//! ```text
//! cargo run --release --example trips -- --measure
//! ```
//!
//! SYNTHETIC and seeded (nothing fetched): 20,000 flights between 60 hub
//! airports, great circles at ~450 kt sampled once a minute inside one
//! hour -- the shape of the Studio's adsb.lol hour, a little larger (1.1M
//! vertices against its 640k). Drawn into a 2560 x 1600 target (1280 x 800 at 2x) on the
//! globe at the sample's camera, as the page draws it: every path whole
//! under a hairline, the fading trails over them.
//!
//! - `trips.build_ms`: `TripSet::from_trips` (CPU, once per data set).
//! - `trips.push_ms`: the first frame, which uploads the vertices.
//! - `trips.globe.frame_ms`: a steady frame, submit to idle.
//! - `trips.tick_cpu_us`: `TripPipeline::prepare` for a frame where only
//!   the clock moved -- the per-tick CPU cost; and `trips.tick_upload_bytes`
//!   (always 0: a clock tick uploads nothing).

use std::time::Instant;

use gpui_maps::render::gpu::GpuContext;
use gpui_maps::render::trips::{TripBlend, TripFrame, TripInput, TripLayerDraw, TripPipeline, TripPoint, TripSet, TripStyle};
use gpui_maps::render::Camera;

const W: u32 = 2560;
const H: u32 = 1600;
const SCALE: f32 = 2.0;
const FRAMES: usize = 30;
const HOUR_MS: f64 = 3_600_000.0;
const T0: f64 = 1_756_994_400_000.0;

fn gate(name: &str, value: f64, unit: &str) {
    println!("gate: trips.{name} {value} {unit}");
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
}

fn unit(lon: f64, lat: f64) -> [f64; 3] {
    let (lon, lat) = (lon.to_radians(), lat.to_radians());
    [lon.sin() * lat.cos(), lat.sin(), lon.cos() * lat.cos()]
}

fn slerp(a: [f64; 3], b: [f64; 3], k: f64) -> (f64, f64) {
    let dot = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]).clamp(-1.0, 1.0);
    let w = dot.acos();
    let (sa, sb) = if w > 1e-9 { (((1.0 - k) * w).sin() / w.sin(), (k * w).sin() / w.sin()) } else { (1.0 - k, k) };
    let p = [a[0] * sa + b[0] * sb, a[1] * sa + b[1] * sb, a[2] * sa + b[2] * sb];
    (p[0].atan2(p[2]).to_degrees(), p[1].clamp(-1.0, 1.0).asin().to_degrees())
}

/// 20,000 seeded flights, a point a minute, inside one hour.
fn synthetic() -> Vec<TripInput> {
    let mut rng = Rng(0x7219_5eed);
    // Hubs, weighted toward Europe and North America as ADS-B coverage is.
    let hubs: Vec<(f64, f64)> = (0..60)
        .map(|i| match i % 3 {
            0 => (-10.0 + rng.next() * 35.0, 37.0 + rng.next() * 20.0),
            1 => (-120.0 + rng.next() * 50.0, 28.0 + rng.next() * 20.0),
            _ => (-180.0 + rng.next() * 360.0, -40.0 + rng.next() * 100.0),
        })
        .collect();
    let palette = ["#FAE300", "#FAC200", "#FD7900", "#E31A1A", "#CF1750", "#AE0E7F", "#7A0DA6", "#482BBD", "#2C51BE", "#223F9A"];
    let colors = gpui_maps::overlay::color_scale::rgba(&palette.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    (0..20_000u32)
        .map(|row| {
            let a = hubs[(rng.next() * hubs.len() as f64) as usize];
            let mut b = hubs[(rng.next() * hubs.len() as f64) as usize];
            if a == b {
                b = (a.0 + 3.0, a.1 + 2.0);
            }
            let (ua, ub) = (unit(a.0, a.1), unit(b.0, b.1));
            let dot = (ua[0] * ub[0] + ua[1] * ub[1] + ua[2] * ub[2]).clamp(-1.0, 1.0);
            // ~830 km/h: the flight's whole duration, of which the hour sees a part.
            let duration = (dot.acos() * 6_371.0 / 830.0 * HOUR_MS).max(10.0 * 60_000.0);
            let departed = T0 - rng.next() * duration * 0.8;
            let mut points = Vec::new();
            let mut t = T0.max(departed);
            while t <= (departed + duration).min(T0 + HOUR_MS) {
                let (lon, lat) = slerp(ua, ub, (t - departed) / duration);
                points.push(TripPoint { lon, lat, alt_m: 10_000.0, time: t });
                t += 60_000.0;
            }
            TripInput { row, points, color: colors[(row as usize * 7) % colors.len()], label: None }
        })
        .collect()
}

fn device() -> GpuContext {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })
            .await
            .expect("an adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor { required_limits: adapter.limits(), ..Default::default() })
            .await
            .expect("a device");
        let max_buffer_size = device.limits().max_buffer_size;
        GpuContext { device, queue, max_buffer_size }
    })
}

/// The Studio page's opening camera (`kepler_trips::camera`).
fn camera() -> Camera {
    Camera {
        center_lon: -38.0,
        center_lat: 43.0,
        zoom: 2.5,
        pitch: 0.0,
        bearing: 0.0,
        viewport_width_px: W as f32 / SCALE,
        viewport_height_px: H as f32 / SCALE,
        fov_y: Camera::DEFAULT_FOV_Y,
    }
}

fn styles() -> (TripStyle, TripStyle) {
    let paths = TripStyle { width_px: 0.35, opacity: 0.22, trail: 0.0, fade: false, blend: TripBlend::Screen, altitude_scale: 0.0, head: None };
    let trails = TripStyle { width_px: 1.5, opacity: 0.8, trail: 336_000.0, fade: true, blend: TripBlend::Screen, altitude_scale: 0.0, head: None };
    (paths, trails)
}

fn measure() {
    let ctx = device();
    let inputs = synthetic();
    let started = Instant::now();
    let set = TripSet::from_trips(&inputs);
    gate("build_ms", round(started.elapsed().as_secs_f64() * 1e3, 1), "ms");
    gate("flights.trips", set.len() as f64, "count");
    gate("flights.vertices", set.vertices.len() as f64, "count");
    gate("upload_kb", (set.upload_bytes() / 1024) as f64, "KB");

    let format = wgpu::TextureFormat::Bgra8Unorm;
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("trips-measure"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let cam = camera();
    let globe = gpui_maps::render::globe::globe_frame(&cam);
    assert!(globe.transition >= 1.0, "the sample's camera is a full globe");
    let frame = TripFrame::new(&cam, &globe, SCALE);
    let mut pipeline = TripPipeline::new(&ctx, format, None);
    let (paths, trails) = styles();
    let frame_at = |pipeline: &mut TripPipeline, clock: f64, generation: u64| -> (f64, f64, u64) {
        let draws = [
            TripLayerDraw { id: "paths", generation, set: &set, style: paths, clock: None },
            TripLayerDraw { id: "trails", generation, set: &set, style: trails, clock: Some(clock) },
        ];
        let t = Instant::now();
        pipeline.prepare(&ctx, &frame, &draws);
        let prepare_us = t.elapsed().as_secs_f64() * 1e6;
        let uploaded = pipeline.stats().upload_bytes;
        let mut encoder = ctx.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("trips-measure-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.07, g: 0.07, b: 0.08, a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pipeline.draw(&mut pass);
        }
        ctx.queue.submit(Some(encoder.finish()));
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        (t.elapsed().as_secs_f64() * 1e3, prepare_us, uploaded)
    };
    // The first frame uploads.
    let (push_ms, _, uploaded) = frame_at(&mut pipeline, T0 + 20.0 * 60_000.0, 1);
    gate("push_ms", round(push_ms, 2), "ms");
    println!("trips push: {} B uploaded on the first frame (two layers, one set: uploaded once)", uploaded);
    // Steady frames: the clock moves a playback step (0.25x: 90 s of data a
    // second, 1.5 s a frame) each time.
    let (mut wall, mut tick, mut bytes) = (Vec::new(), Vec::new(), 0u64);
    for i in 0..FRAMES + 3 {
        let clock = T0 + 20.0 * 60_000.0 + i as f64 * 1_500.0;
        let (ms, us, b) = frame_at(&mut pipeline, clock, 1);
        if i >= 3 {
            wall.push(ms);
            tick.push(us);
            bytes += b;
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let stats = pipeline.stats();
    println!("trips frame: {} instances in {} draws", stats.instances, stats.draws);
    gate("globe.frame_ms", round(median(&mut wall), 2), "ms");
    gate("tick_cpu_us", round(median(&mut tick), 1), "us");
    gate("tick_upload_bytes", bytes as f64, "count");
    gate("globe.draws", stats.draws as f64, "count");
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--measure") => measure(),
        _ => eprintln!("usage: trips --measure"),
    }
}
