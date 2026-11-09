//! The dot mask pass's GPU time (probe builds): a two-stamp
//! `TIMESTAMP_QUERY` set around `TilePipeline::encode_dot_mask_timed`'s
//! pass, read back into [`mask_pass_gpu_ns`]. Its own module, apart from
//! `render::dot_mask`, whose only `COPY_SRC` must stay the mask texture's
//! test/probe read-back (DP-8); the one here is a query-resolve buffer.

/// The mask pass's GPU execution time, per pass, in nanoseconds: what
/// [`MaskPassTimer`] read back (probe builds; `world_dots` prints p50/p95).
static MASK_PASS_GPU_NS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

/// Every mask-pass GPU duration read back so far, in nanoseconds.
#[must_use]
pub fn mask_pass_gpu_ns() -> Vec<u64> {
    MASK_PASS_GPU_NS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

/// The mask pass's GPU time through a `TIMESTAMP_QUERY` query set (probe
/// builds, where the device has the feature; Metal does, the headless test
/// device never asks). As `render::gpu::PassTimer` found on Metal, the
/// resolve must be on a LATER command buffer than the pass: [`Self::collect`]
/// resolves the last pass's two stamps on its own small submit, called at
/// the top of the next frame's mask step.
pub struct MaskPassTimer {
    query_set: wgpu::QuerySet,
    resolve: crate::render::alloc::TrackedBuffer,
    readback: std::sync::Arc<crate::render::alloc::TrackedBuffer>,
    period_ns: f32,
    written: std::cell::Cell<bool>,
    reading: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MaskPassTimer {
    const BYTES: u64 = wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT;

    /// `None` unless the device got `TIMESTAMP_QUERY`.
    #[must_use]
    pub fn new(ctx: &crate::render::GpuContext) -> Option<Self> {
        if !ctx.device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let buffer = |label, usage| crate::render::alloc::TrackedBuffer::new(
            &ctx.device,
            &wgpu::BufferDescriptor { label: Some(label), size: Self::BYTES, usage, mapped_at_creation: false },
        );
        Some(Self {
            query_set: ctx.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("dot-mask-timer"),
                ty: wgpu::QueryType::Timestamp,
                count: 2,
            }),
            resolve: buffer("dot-mask-timer-resolve", wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC),
            readback: std::sync::Arc::new(buffer("dot-mask-timer-readback", wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ)),
            period_ns: ctx.queue.get_timestamp_period(),
            written: std::cell::Cell::new(false),
            reading: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// The pass's `timestamp_writes`, or `None` while a readback is
    /// outstanding (its slots are not free to overwrite yet).
    #[must_use]
    pub fn writes(&self) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        (!self.reading.load(std::sync::atomic::Ordering::Acquire) && !self.written.get()).then_some(wgpu::RenderPassTimestampWrites {
            query_set: &self.query_set,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        })
    }

    /// Records that a submitted pass wrote the slots.
    pub fn mark_written(&self) {
        self.written.set(true);
    }

    /// Resolves the last written pass on its own submit and reads it back
    /// into [`mask_pass_gpu_ns`]'s list when the GPU gets there.
    pub fn collect(&self, ctx: &crate::render::GpuContext) {
        if !self.written.get() || self.reading.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        self.written.set(false);
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dot-mask-timer") });
        encoder.resolve_query_set(&self.query_set, 0..2, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, Self::BYTES);
        ctx.queue.submit(Some(encoder.finish()));
        self.reading.store(true, std::sync::atomic::Ordering::Release);
        let (readback, reading, period_ns) = (std::sync::Arc::clone(&self.readback), std::sync::Arc::clone(&self.reading), self.period_ns);
        self.readback.map_async(wgpu::MapMode::Read, .., move |result| {
            if result.is_ok() {
                let view = readback.slice(..).get_mapped_range().expect("the range just mapped for reading");
                let tick = |i: usize| u64::from_le_bytes(view[i * 8..i * 8 + 8].try_into().expect("8 bytes per timestamp"));
                let nanos = (tick(1).saturating_sub(tick(0)) as f64 * f64::from(period_ns)) as u64;
                drop(view);
                readback.unmap();
                let mut samples = MASK_PASS_GPU_NS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if samples.len() < 100_000 {
                    samples.push(nanos);
                }
            }
            reading.store(false, std::sync::atomic::Ordering::Release);
        });
    }
}
