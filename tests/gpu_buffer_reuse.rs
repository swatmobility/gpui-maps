//! The counter-level acceptance check for GPU-buffer reuse, run from
//! OUTSIDE the crate against the public `render::allocation_stats` surface
//! -- the same surface the consumer scrapes.
//!
//! Why an integration test and not a unit test: the counters in
//! `render::alloc` are process-global, and the lib test binary runs its
//! ~1150 tests in parallel, several of which allocate GPU buffers on other
//! threads. Any assertion of the form "this counter did not move" is
//! meaningless there. An integration test gets its own process, and this
//! file deliberately holds ONE test so nothing else in it can allocate
//! either -- see that test's own doc comment for why its overlay-model-
//! uniform phase is a phase of it rather than a second `#[test]` here. The
//! reuse mechanism itself is unit-tested in `render::gpu` and
//! `render::icon_pipeline` (buffer identity across a reinstall/frame); what
//! can only be checked here is the pair of counters the diagnostics
//! contract actually promises.

use gpui_maps::render::{allocation_stats, GpuContext, TileMesh, TilePipeline, Vertex};

/// A distinct model matrix per iteration -- the camera moves every frame in
/// the real app, so a fix that only avoided reallocating for an UNCHANGED
/// matrix would miss the real case entirely.
fn model_matrix(t: f32) -> [[f32; 4]; 4] {
    glam::Mat4::from_translation(glam::Vec3::new(t, 0.0, 0.0)).to_cols_array_2d()
}

/// A windowless device. `GpuContext::new_headless` is `#[cfg(test)]`, i.e.
/// crate-internal, so this rebuilds the two lines of it an integration test
/// needs out of the public `GpuContext::new`.
async fn headless() -> GpuContext {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            ..Default::default()
        })
        .await
        .expect("a wgpu adapter");
    GpuContext::new(&adapter).await.expect("a wgpu device")
}

fn vertex(x: f32) -> Vertex {
    Vertex {
        position: [x, 0.0],
        normal: [0.0, 0.0],
        color: [1.0, 1.0, 1.0, 1.0],
        is_stroke: 0.0,
        half_width_px: 0.0,
        color_to: [1.0, 1.0, 1.0, 1.0],
        half_width_px_to: 0.0,
        gap_half_px: 0.0,
        gap_half_px_to: 0.0,
        line_distance: 0.0,
        dash_runs: [0.0; 4],
        line_progress: -1.0,
    }
}

/// `vertices` vertices and a matching index per vertex -- shaped like an
/// overlay push, where the count moves with the feature set rather than
/// staying put.
fn mesh_of(vertices: usize) -> TileMesh {
    TileMesh {
        vertices: (0..vertices).map(|i| vertex(i as f32)).collect(),
        indices: (0..vertices as u32).collect(),
    }
}

/// The behaviour the consumer's RSS report turns on: a caller re-pushing an
/// overlay must not allocate a fresh pair of GPU buffers per push.
///
/// Three phases in ONE test, not three `#[test]`s, because this file's
/// whole reason for being an integration test rather than a unit test is
/// that `gpu_buffers_created_total` is process-global (see the module doc
/// comment) -- a second `#[test]` function in this file would run
/// concurrently with this one under the default test harness and each
/// would see the other's allocations. Phase 3 was added for the same
/// defect class as phases 1/2, found a second time at the overlay's model
/// uniform (`MapView::paint` used to call `create_model_bind_group` -- a
/// fresh buffer and bind group -- on every frame instead of writing the one
/// built once), and it belongs here rather than in a file of its own for
/// exactly that reason.
///
/// 1. A constant-size mesh reinstalled 50 times -- `mesh_installs_total`
///    climbs by 50, `gpu_buffers_created_total` does not move at all.
/// 2. A mesh whose vertex count JITTERS on every push, which is what a live
///    feed actually does and what an exact-size-match reuse guard would
///    miss every time. Grow-and-keep converges on the high-water mark: a
///    handful of allocations early, then none, no matter how many more
///    pushes arrive.
/// 3. The overlay's model-uniform buffer, built once via
///    `create_model_bind_group` and then rewritten 120 times via
///    `write_model_uniform` (a fixed 192-byte struct, so unlike phases 1/2
///    there is no growth to measure -- only that the fixed-size in-place
///    rewrite allocates nothing at all).
#[test]
fn reinstalling_an_overlay_mesh_stops_allocating_gpu_buffers() {
    pollster::block_on(async {
        let ctx = headless().await;
        let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8UnormSrgb);

        // Phase 1: same size every time.
        let mut buffers = pipeline.create_tile_buffers(&ctx, &mesh_of(10_000));
        let before = allocation_stats();
        for _ in 0..50 {
            buffers = pipeline.install_mesh(&ctx, Some(buffers), &mesh_of(10_000));
        }
        let after = allocation_stats();
        assert_eq!(
            after.mesh_installs_total - before.mesh_installs_total,
            50,
            "50 installs must be counted as 50 installs"
        );
        assert_eq!(
            after.gpu_buffers_created_total - before.gpu_buffers_created_total,
            0,
            "a same-size reinstall must allocate nothing: created_total {} -> {}",
            before.gpu_buffers_created_total,
            after.gpu_buffers_created_total
        );
        assert_eq!(
            after.gpu_buffer_bytes_created_total - before.gpu_buffer_bytes_created_total,
            0,
            "and must therefore have uploaded no newly allocated bytes"
        );

        // Phase 2: a size that moves on every push, the way a live feature
        // set does. The counts are drawn from a fixed pattern rather than a
        // random source so a failure here is reproducible.
        let jitter = [9_500usize, 10_400, 9_900, 10_100, 10_300, 9_700, 10_200, 10_000];
        let settle = allocation_stats();
        for tick in 0..40 {
            buffers = pipeline.install_mesh(&ctx, Some(buffers), &mesh_of(jitter[tick % jitter.len()]));
        }
        let settled = allocation_stats();
        let allocations_while_settling =
            settled.gpu_buffers_created_total - settle.gpu_buffers_created_total;
        assert!(
            allocations_while_settling <= 4,
            "a jittering vertex count must converge on its high-water mark, not allocate per push: \
             {allocations_while_settling} allocations over 40 pushes"
        );

        // And once settled, nothing at all: 40 more pushes over the same
        // pattern, now that the high-water mark has been reached.
        let steady_before = allocation_stats();
        for tick in 0..40 {
            buffers = pipeline.install_mesh(&ctx, Some(buffers), &mesh_of(jitter[tick % jitter.len()]));
        }
        let steady_after = allocation_stats();
        assert_eq!(
            steady_after.gpu_buffers_created_total - steady_before.gpu_buffers_created_total,
            0,
            "past the high-water mark a re-push must allocate nothing"
        );
        assert_eq!(
            steady_after.mesh_installs_total - steady_before.mesh_installs_total,
            40,
            "while still counting every one of those pushes as an install"
        );

        // Phase 3: the overlay's model uniform. Built once via
        // `create_model_bind_group`, then rewritten 120 times via
        // `write_model_uniform` with a DIFFERENT matrix/blend_t/dash_scale
        // each call -- the camera moves every frame in the real app, so a
        // fix that only avoided reallocating for an unchanged matrix would
        // miss the real case entirely. Fixed 192-byte struct, so unlike
        // phases 1/2 there is no growth to measure, only that the in-place
        // rewrite allocates nothing at all.
        let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
        let (model_buffer, _bind_group) =
            pipeline.create_model_bind_group(&ctx, identity, 0.0, 0.0, gpui_maps::render::GlobeParams::FLAT);
        let model_before = allocation_stats();
        for frame in 0..120 {
            let t = frame as f32 / 120.0;
            pipeline.write_model_uniform(
                &ctx,
                &model_buffer,
                model_matrix(t),
                t,
                1.0 + t,
                gpui_maps::render::GlobeParams::FLAT,
            );
        }
        let model_after = allocation_stats();
        assert_eq!(
            model_after.gpu_buffers_created_total - model_before.gpu_buffers_created_total,
            0,
            "120 in-place rewrites of the overlay model uniform must allocate nothing: created_total {} -> {}",
            model_before.gpu_buffers_created_total,
            model_after.gpu_buffers_created_total
        );
        assert_eq!(
            model_after.gpu_buffer_bytes_created_total - model_before.gpu_buffer_bytes_created_total,
            0,
            "and therefore no newly allocated bytes either"
        );

        drop(buffers);
        drop(model_buffer);
    });
}
