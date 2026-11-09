//! The night race's in-car view, headless: the perf gate's `night_race.*`
//! section.
//!
//! ```text
//! cargo run --release --features probe --example night_race_gate -- --measure
//! ```
//!
//! The player's own car (`night_race_car_mesh.rs`'s `own_car_mesh`) and the
//! driver with the cockpit's insides (`night_race_driver.rs`) as the page
//! draws them in the car: two model layers in the engine's first-person
//! near pass (`render::model::ModelNearPass`), their part tables moved
//! every frame by the steering rig, at the in-car camera (0.86 m up,
//! pitched 75 degrees, a 58 degree FOV) into a 2560 x 1600 target (1280 x 800
//! at 2x). No network, no map data: the scene behind is a cleared pass,
//! so the rows are the model pass's own cost.
//!
//! - `night_race.in_car.frame_p50_ms` / `frame_p95_ms`: a frame's CPU
//!   prepare (the uniforms and the part tables) plus the encode, submit
//!   and wait until the GPU is idle.
//! - `night_race.in_car.model_pass_gpu_p50_us` / `_p95_us`: the near
//!   pass's own GPU time from a two-stamp `TIMESTAMP_QUERY` set round it
//!   (as `render::mask_timer::MaskPassTimer` times the dot mask), where
//!   the device has the feature.
//! - `night_race.in_car.triangles`: the triangles the near pass draws
//!   (both layers): exact.
//! - `night_race.in_car.parts_upload_bytes`: the part tables a frame
//!   writes: exact.

#[path = "../examples/panels/night_race_car_dims.rs"]
mod car_dims;
#[path = "../examples/panels/night_race_car_mesh.rs"]
mod car_mesh;
#[path = "../examples/panels/night_race_driver.rs"]
mod driver;

use std::sync::Arc;
use std::time::Instant;

use gpui_maps::render::model::{ModelFrame, ModelInstance, ModelInstances, ModelLayerDraw, ModelLight, ModelNearPass, ModelPipeline};
use gpui_maps::render::{Camera, GpuContext};

const W: u32 = 2560;
const H: u32 = 1600;
const SCALE: f32 = 2.0;
const FRAMES: usize = 240;
const WARMUP: usize = 20;

fn gate(name: &str, value: f64, unit: &str) {
    println!("gate: night_race.in_car.{name} {value} {unit}");
}

fn round(v: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (v * k).round() / k
}

fn device() -> GpuContext {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions::default()).await.expect("an adapter");
        let required_features = adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor { required_features, ..Default::default() })
            .await
            .expect("a device");
        let max_buffer_size = device.limits().max_buffer_size;
        GpuContext { device, queue, max_buffer_size }
    })
}

/// The in-car camera at the equator looking north, and where the car
/// stands so its in-car camera point is the eye: `(camera, car lon, car lat)`.
fn camera() -> (Camera, f64, f64) {
    let pitch = 75f64.to_radians();
    let eye_h = car_dims::IN_CAR_CAMERA[2];
    let fov = 58f64.to_radians();
    let dist_m = eye_h / pitch.cos();
    let m_per_world = gpui_maps::camera_control::metres_per_world_unit(0.0);
    let zoom = (800.0 / (2.0 * (fov / 2.0).tan() * dist_m / m_per_world)).log2();
    let cam = Camera { center_lon: 0.0, center_lat: 0.0, zoom, pitch: pitch as f32, bearing: 0.0, viewport_width_px: 1280.0, viewport_height_px: 800.0, fov_y: fov };
    // The eye is `dist sin p` south of the look-at point; the car's centre
    // is `-IN_CAR_CAMERA[0]` ahead of it.
    let north_m = -dist_m * pitch.sin() - car_dims::IN_CAR_CAMERA[0];
    (cam, 0.0, north_m / 111_320.0)
}

fn measure() {
    let ctx = device();
    let timestamps = ctx.device.features().contains(wgpu::Features::TIMESTAMP_QUERY);
    let format = wgpu::TextureFormat::Bgra8Unorm;
    let depth_format = wgpu::TextureFormat::Depth24PlusStencil8;
    let tex = |format, label| {
        ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    };
    let color = tex(format, "night-race-gate-color").create_view(&Default::default());
    let depth = tex(depth_format, "night-race-gate-depth").create_view(&Default::default());
    let (cam, lon, lat) = camera();
    let frame = ModelFrame::new(&cam, &gpui_maps::render::globe::globe_frame(&cam).flattened(), SCALE);
    let car = ModelInstances::new(&[ModelInstance::at(lon, lat, 0.0)]);
    let own = Arc::new(car_mesh::own_car_mesh());
    let driver_mesh = Arc::new(driver::driver_mesh());
    let near = Some(ModelNearPass::FIRST_PERSON);
    let mut pipeline = ModelPipeline::new(&ctx, format, Some(depth_format));
    let query = timestamps.then(|| ctx.device.create_query_set(&wgpu::QuerySetDescriptor { label: Some("night-race-gate"), ty: wgpu::QueryType::Timestamp, count: 2 }));
    let resolve = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("night-race-gate-resolve"),
        size: wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("night-race-gate-readback"),
        size: wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let period = f64::from(ctx.queue.get_timestamp_period());
    let (mut wall, mut gpu) = (Vec::new(), Vec::new());
    let (mut triangles, mut parts_bytes) = (0u64, 0u64);
    for i in 0..FRAMES + WARMUP {
        // The wheel sawing between 60 degrees left and right.
        let angle = 60.0 * (i as f64 * 0.07).sin();
        let t = Instant::now();
        let (driver_parts, car_parts) = driver::rig(angle);
        let draws = [
            ModelLayerDraw { id: "own", mesh: &own, generation: 1, instances: &car, light: ModelLight::NIGHT, parts: &car_parts, near },
            ModelLayerDraw { id: "driver", mesh: &driver_mesh, generation: 1, instances: &car, light: driver::LIGHT, parts: &driver_parts, near },
        ];
        pipeline.prepare(&ctx, &frame, &draws);
        let mut encoder = ctx.device.create_command_encoder(&Default::default());
        {
            // The scene: cleared (the map's own passes are measured elsewhere).
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("night-race-gate-scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.03, g: 0.035, b: 0.05, a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        let writes = query.as_ref().map(|q| wgpu::RenderPassTimestampWrites { query_set: q, beginning_of_pass_write_index: Some(0), end_of_pass_write_index: Some(1) });
        let calls = pipeline.draw_near_pass(&mut encoder, &color, &depth, writes);
        assert_eq!(calls, 2, "both layers in the near pass");
        ctx.queue.submit(Some(encoder.finish()));
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if let Some(q) = query.as_ref() {
            // On a LATER command buffer than the pass: on Metal a resolve in
            // the pass's own buffer reads zeros (`render::gpu::PassTimer`).
            let mut encoder = ctx.device.create_command_encoder(&Default::default());
            encoder.resolve_query_set(q, 0..2, &resolve, 0);
            encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT);
            ctx.queue.submit(Some(encoder.finish()));
            readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            ctx.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            let view = readback.slice(..).get_mapped_range().expect("mapped");
            let tick = |k: usize| u64::from_le_bytes(view[k * 8..k * 8 + 8].try_into().expect("8 bytes"));
            let ns = tick(1).saturating_sub(tick(0)) as f64 * period;
            drop(view);
            readback.unmap();
            if i >= WARMUP {
                gpu.push(ns / 1e3);
            }
        }
        if i >= WARMUP {
            wall.push(ms);
        }
        let stats = pipeline.stats();
        triangles = stats.triangles;
        parts_bytes = stats.parts_upload_bytes;
    }
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * p).round() as usize]
    };
    println!("night race in-car: {} near-pass layers, {} triangles, timestamps {}", 2, triangles, if timestamps { "on" } else { "off (no TIMESTAMP_QUERY)" });
    gate("frame_p50_ms", round(pct(&mut wall, 0.5), 3), "ms");
    gate("frame_p95_ms", round(pct(&mut wall, 0.95), 3), "ms");
    if !gpu.is_empty() {
        gate("model_pass_gpu_p50_us", round(pct(&mut gpu, 0.5), 1), "us");
        gate("model_pass_gpu_p95_us", round(pct(&mut gpu, 0.95), 1), "us");
    }
    gate("triangles", triangles as f64, "count");
    gate("parts_upload_bytes", parts_bytes as f64, "count");
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--measure") => measure(),
        _ => eprintln!("usage: night_race_gate --measure"),
    }
}
