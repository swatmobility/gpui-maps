//! `MapView`'s gpui glue for dot-density sources: the public API, and
//! running `overlay::host`'s [`DotLayers`] jobs off the UI thread.
//!
//! Everything that decides -- which count tiles a frame wants, which
//! request goes next, which ancestor stands in, the view's population per
//! dot, whether a result still belongs to its source -- is in
//! `overlay::dotgrid::layer` (re-exported by `overlay::host`), which the
//! browser and the phone will drive too. This file only spawns the jobs
//! (tokio's blocking pool, the one `drive_point_sources` uses, fetching
//! with `fetch_native::execute`) and hands the results back. The dot
//! mask's per-frame step (when to re-render it, the ring past the cover)
//! is shell-neutral too: `overlay::dotgrid::mask_step`.
//!
//! See `docs/superpowers/specs/2026-09-26-world-dot-density-design.md`.

use std::sync::Arc;

use super::MapView;
use crate::overlay::host::{DotJob, DotLanded, DotStats, DotStyle, GridCountSource};

impl MapView {
    /// Draws population dots from `source` as layer `id`, replacing any
    /// layer of that id: whatever the old one had in flight is dropped
    /// when it lands (`DotLayers::land`'s generation check). The source's
    /// attribution joins the frame's credit line.
    pub fn set_dot_density_source(
        &mut self,
        id: &str,
        source: Arc<dyn GridCountSource>,
        style: DotStyle,
        cx: &mut gpui::Context<Self>,
    ) {
        self.dot_layers.set_source(id, source, style);
        cx.notify();
    }

    /// Stops drawing dot layer `id`; its tiles and GPU textures go.
    pub fn remove_dot_density_source(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        if self.dot_layers.remove_source(id) {
            cx.notify();
        }
    }

    /// Live epochs (Task 15): dot layer `id` draws epoch `a`, or -- with
    /// `b` -- both, fetched together (`GridTileKey::epoch_b`), so that
    /// [`Self::set_dot_epoch_mix`] can move the dots from `a` to `b`
    /// without a fetch, a rebuild or an upload. New epochs are fetched as
    /// any tile is; the layer keeps its generation and ppd.
    pub fn set_dot_epochs(&mut self, id: &str, a: u16, b: Option<u16>, cx: &mut gpui::Context<Self>) {
        if self.dot_layers.set_epochs(id, a, b) {
            cx.notify();
        }
    }

    /// How far layer `id`'s dots are from its epoch `a` toward `b`,
    /// `[0, 1]` (clamped; NaN is 0). Writes only the draws' `DotParams`
    /// on the next paint: `allocation_stats().mesh_installs_total` and
    /// `gpu_buffers_created_total` do not move.
    pub fn set_dot_epoch_mix(&mut self, id: &str, t: f32, cx: &mut gpui::Context<Self>) {
        if self.dot_layers.set_epoch_mix(id, t) {
            cx.notify();
        }
    }

    /// Layer `id`'s colour ramp, e.g. `DotRamp::Difference` (the change
    /// from epoch `a` to `b`). Re-binds the layer's tiles over the new
    /// paint table; their blocks are not uploaded again.
    pub fn set_dot_ramp(&mut self, id: &str, ramp: crate::overlay::dotgrid::layer::DotRamp, cx: &mut gpui::Context<Self>) {
        if self.dot_layers.set_ramp(id, ramp) {
            cx.notify();
        }
    }

    /// Layer `id`'s legend toggles (`DotStyle::category_mask`): group `g`
    /// draws while bit `g` of `mask` is set; a toggled-off group's dots are
    /// hidden, and the ppd does not move. Writes only the draws' `DotParams`
    /// on the next paint.
    pub fn set_dot_category_mask(&mut self, id: &str, mask: u8, cx: &mut gpui::Context<Self>) {
        if self.dot_layers.set_category_mask(id, mask) {
            cx.notify();
        }
    }

    /// Layer `id`'s opacity, `[0, 1]` (`DotStyle::opacity`): remakes its
    /// paint table on the next paint; its tiles are not uploaded again.
    pub fn set_dot_opacity(&mut self, id: &str, opacity: f32, cx: &mut gpui::Context<Self>) {
        if self.dot_layers.set_opacity(id, opacity) {
            cx.notify();
        }
    }

    /// Layer `id` draws only at camera zooms of `min_zoom` and up
    /// (`DotStyle::min_zoom`); below it the layer plans nothing.
    pub fn set_dot_min_zoom(&mut self, id: &str, min_zoom: f64, cx: &mut gpui::Context<Self>) {
        if self.dot_layers.set_min_zoom(id, min_zoom) {
            cx.notify();
        }
    }

    /// What this map's dot layers have sent to the GPU so far (block
    /// textures, groups, uniform writes): MG-23's upload count, this map's
    /// own. All zero before the first paint.
    #[must_use]
    pub fn dot_uploads(&self) -> crate::overlay::host::DotUploads {
        self.gpu.borrow().as_ref().map(|g| g.pipeline.dot_uploads()).unwrap_or_default()
    }

    /// Layer `id`'s style now (its epochs, mix and ramp included).
    #[must_use]
    pub fn dot_style(&self, id: &str) -> Option<DotStyle> {
        self.dot_layers.style(id).cloned()
    }

    /// The basemap tiles past the `visible` cover the dot mask rasterises
    /// this frame: `overlay::dotgrid::mask_step::mask_ring_for_frame`.
    pub(super) fn mask_ring_for_frame(
        &self,
        globe: &crate::render::globe::GlobeCover,
        visible: &[crate::geo::TileId],
    ) -> Vec<crate::geo::TileId> {
        crate::overlay::host::mask_ring_for_frame(&self.dot_layers, self.camera_control.camera(), globe, visible)
    }

    /// For the on-screen counter and the perf gate: dots drawn this frame,
    /// the view ppd, the level, tiles ready/wanted/standing in, GPU bytes
    /// and source errors. `None` for no such layer.
    #[must_use]
    pub fn dot_stats(&self, id: &str) -> Option<DotStats> {
        self.dot_layers.stats(id)
    }

    /// Called from `paint` once the camera is final: plans every dot
    /// layer for this frame and, when a layer has a request or a rebuild
    /// to make, schedules [`Self::drive_dot_sources`].
    pub(super) fn dot_sources_frame(&mut self, cx: &mut gpui::App, globe: &crate::render::globe::GlobeCover) {
        if self.dot_layers.is_empty() {
            return;
        }
        let now = crate::timing::Instant::now();
        let work = self.dot_layers.frame(self.camera_control.camera(), globe, now);
        // A failed key is asked for again after a backoff, but nothing
        // repaints a map sitting still: wake it when the backoff ends.
        if let (Some(wait), false) = (self.dot_layers.next_retry(now), self.dot_retry_pending) {
            if let Some(entity) = self.self_handle.clone() {
                self.dot_retry_pending = true;
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(wait + std::time::Duration::from_millis(50)).await;
                    let _ = entity.update(cx, |view, cx| {
                        view.dot_retry_pending = false;
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        if !work {
            return;
        }
        let Some(entity) = self.self_handle.clone() else { return };
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| view.drive_dot_sources(cx));
        });
    }

    /// Starts every job the dot layers will hand out now (at most
    /// `MAX_REQUESTS_IN_FLIGHT` per source): off the UI thread when there
    /// is a runtime, inline otherwise. A landing re-drives.
    pub(crate) fn drive_dot_sources(&mut self, cx: &mut gpui::Context<Self>) {
        while let Some(job) = self.dot_layers.next_job() {
            match self.runtime.clone() {
                Some(runtime) => {
                    // Kept here, not in the job: a job whose worker panics
                    // returns nothing, and must still be landed (as a
                    // failure) or its request slot is held forever.
                    let ticket = job.ticket();
                    cx.spawn(async move |entity, cx| {
                        let landed = match runtime.spawn_blocking(move || run_dot_job(job)).await {
                            Ok(landed) => landed,
                            Err(_) => DotLanded::abandoned(ticket),
                        };
                        let _ = entity.update(cx, |view, cx| view.land_dot_job(landed, cx));
                    })
                    .detach();
                }
                None => {
                    let landed = run_dot_job(job);
                    self.land_dot_job(landed, cx);
                }
            }
        }
    }

    fn land_dot_job(&mut self, landed: DotLanded, cx: &mut gpui::Context<Self>) {
        if self.dot_layers.land(landed) {
            cx.notify();
        }
    }
}

/// A job's IO, blocking: a local file or an http(s) range, or a
/// ClickHouse POST (`fetch_native::execute`).
fn run_dot_job(job: DotJob) -> DotLanded {
    job.run(crate::overlay::dotgrid::fetch_native::execute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::dotgrid::layer::GHSL_ATTRIBUTION;
    use crate::overlay::dotgrid::{grid, GridDescriptor, GridTile, MemoryGridSource};
    use gpui::AppContext as _;

    struct Credited(&'static str);

    #[async_trait::async_trait]
    impl crate::tile_source::TileSource for Credited {
        async fn fetch(&self, _tile: crate::geo::TileId) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn attribution(&self) -> Option<&str> {
            Some(self.0)
        }
    }

    fn ghsl_source() -> Arc<dyn GridCountSource> {
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: GHSL_ATTRIBUTION.to_string(),
        };
        Arc::new(MemoryGridSource::new(d, |_| GridTile { cells: vec![(0, 10.0, 10.0)], total: 10.0, ..Default::default() }))
    }

    fn map(cx: &mut gpui::TestAppContext) -> gpui::Entity<MapView> {
        cx.new(|_| {
            let handle = crate::map_view::test_runtime_handle();
            MapView::new(
                10.0,
                -5.0,
                5.5,
                800.0,
                600.0,
                crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                Arc::new(Credited("(c) Mapbox (c) OpenStreetMap")),
                handle,
            )
        })
    }

    #[test]
    fn the_attribution_keeps_the_basemap_credit_and_adds_ghsl() {
        let mut app = gpui::TestAppContext::single();
        let map = map(&mut app);
        map.update(&mut app, |v, cx| v.set_dot_density_source("pop", ghsl_source(), DotStyle::default(), cx));
        map.read_with(&app, |v, _| {
            let credit = v.frame_attribution().expect("a credit");
            assert!(credit.contains("(c) Mapbox (c) OpenStreetMap"), "the basemap credit survives: {credit}");
            assert!(credit.contains(GHSL_ATTRIBUTION), "the GHSL credit CC BY 4.0 requires: {credit}");
            assert!(credit.starts_with("(c) Mapbox"), "the map's own credit comes first: {credit}");
        });
        map.update(&mut app, |v, cx| v.remove_dot_density_source("pop", cx));
        map.read_with(&app, |v, _| {
            assert_eq!(v.frame_attribution().as_deref(), Some("(c) Mapbox (c) OpenStreetMap"));
        });
    }

    /// A source whose every request fails: a range of a file that does
    /// not exist.
    struct Failing(GridDescriptor);

    impl GridCountSource for Failing {
        fn descriptor(&self) -> &GridDescriptor {
            &self.0
        }
        fn request(&self, _keys: &[crate::overlay::dotgrid::GridTileKey]) -> crate::overlay::dotgrid::GridRequest {
            crate::overlay::dotgrid::GridRequest::Range { location: "/nonexistent/grid.pmtiles".to_string(), offset: 0, length: 16 }
        }
        fn decode(
            &self,
            _keys: &[crate::overlay::dotgrid::GridTileKey],
            _body: &[u8],
        ) -> Result<Vec<(crate::overlay::dotgrid::GridTileKey, GridTile)>, crate::overlay::dotgrid::GridError> {
            Ok(Vec::new())
        }
    }

    /// A failure is reported, backs its keys off for `FAILED_BACKOFF`, and
    /// says when to wake -- and past that, the keys are asked for again.
    #[test]
    fn a_failed_fetch_backs_off_and_says_when_to_retry() {
        let mut app = gpui::TestAppContext::single();
        let map = map(&mut app);
        let d = ghsl_source().descriptor().clone();
        map.update(&mut app, |v, cx| v.set_dot_density_source("pop", Arc::new(Failing(d)), DotStyle::default(), cx));
        let globe = crate::render::globe::GlobeCover::FLAT;
        // Every wanted key is asked for (four at a time) and fails.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            map.update(&mut app, |v, cx| {
                v.dot_layers.frame(v.camera_control.camera(), &globe, crate::timing::Instant::now());
                v.drive_dot_sources(cx);
            });
            app.run_until_parked();
            let stats = map.read_with(&app, |v, _| v.dot_stats("pop").unwrap());
            if stats.tiles_wanted > 0 && stats.source_errors >= u64::from(stats.tiles_wanted) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the failures never all landed: {stats:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        map.update(&mut app, |v, _| {
            let now = crate::timing::Instant::now();
            assert!(!v.dot_layers.frame(v.camera_control.camera(), &globe, now), "failed keys back off: no work");
            let wait = v.dot_layers.next_retry(now).expect("a retry is due");
            assert!(wait <= crate::overlay::dotgrid::layer::FAILED_BACKOFF && wait > std::time::Duration::from_secs(4));
            assert_eq!(v.dot_stats("pop").unwrap().last_error, Some("io"));
            // Past the backoff the keys are asked for again.
            let later = crate::timing::Instant::from(std::time::Instant::now() + std::time::Duration::from_secs(6));
            assert!(v.dot_layers.frame(v.camera_control.camera(), &globe, later), "retried after the backoff");
        });
    }

    /// A live pair's tile: every cell of `k`'s window with `pop` 100 in
    /// epoch A (1975) and 300 in B (2030) -- only for a key with `epoch_b`.
    fn live_tile(k: &crate::overlay::dotgrid::GridTileKey) -> GridTile {
        let pop_b = if k.epoch_b.is_some() { 300.0 } else { 100.0 };
        GridTile { cells: (0..64).map(|i| (i * 97, 100.0, pop_b)).collect(), total: 6400.0, ..Default::default() }
    }

    /// Task 15: moving `epoch_mix` only rewrites uniforms. After the first
    /// sync uploaded the tiles, eleven mixes from 0 to 1 hand out no job
    /// (no fetch, no rebuild: the ppd budgets `max(pop, pop_b)`, so it does
    /// not move with the mix), upload no block texture, create no group or
    /// buffer, and write each drawn tile's `DotParams` with the mix.
    #[test]
    fn animating_t_uploads_nothing() {
        let d = ghsl_source().descriptor().clone();
        let source: Arc<dyn GridCountSource> = Arc::new(MemoryGridSource::new(d, live_tile));
        let mut layers = crate::overlay::host::DotLayers::new();
        let style = DotStyle { epoch: 1975, epoch_b: Some(2030), ..DotStyle::default() };
        layers.set_source("pop", source, style);
        let camera = crate::render::Camera {
            center_lon: 78.0,
            center_lat: 22.0,
            zoom: 5.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let cover = crate::render::globe::GlobeCover::FLAT;
        for _ in 0..10 {
            layers.frame(&camera, &cover, crate::timing::Instant::now());
            while let Some(job) = layers.next_job() {
                layers.land(job.run(|_| Ok(Vec::new())));
            }
        }
        let plan = layers.plan("pop").unwrap().clone();
        assert!(!plan.draw.is_empty(), "the layer draws");
        assert!(plan.draw.iter().all(|x| x.key.epoch == 1975 && x.key.epoch_b == Some(2030)), "{plan:?}");
        let ctx = pollster::block_on(crate::render::GpuContext::new_headless()).expect("headless device");
        let pipeline = crate::render::TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let frame = crate::render::globe::globe_frame(&camera);
        let globe = crate::render::globe::GlobeUpload::new(&frame, &camera, crate::mvt::MVT_EXTENT as f32, glam::DVec3::ZERO, 1.0);
        let mut gpu = crate::overlay::host::DotLayersGpu::default();
        gpu.sync(&mut layers, &ctx, &pipeline, &camera, &globe, None);
        let first = pipeline.dot_uploads();
        let drawn = u64::from(layers.stats("pop").unwrap().tiles_drawn);
        assert!(drawn > 0 && first.tile_uploads == drawn, "{first:?}, {drawn} tiles drawn");
        let ppd = layers.stats("pop").unwrap().ppd;
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            layers.set_epoch_mix("pop", t);
            assert!(!layers.frame(&camera, &cover, crate::timing::Instant::now()), "t {t}: work was planned");
            assert!(layers.next_job().is_none(), "t {t}: a job");
            gpu.sync(&mut layers, &ctx, &pipeline, &camera, &globe, None);
            assert_eq!(layers.stats("pop").unwrap().ppd, ppd, "t {t}: the ppd moved");
            for params in gpu.drawn_params("pop") {
                assert_eq!(params.t, t, "the mix reaches every draw");
            }
        }
        let after = pipeline.dot_uploads();
        assert_eq!(after.tile_uploads, first.tile_uploads, "a block texture was uploaded");
        assert_eq!(after.groups_created, first.groups_created, "a group (and its buffer) was created");
        assert_eq!(after.params_writes - first.params_writes, 11 * drawn, "one DotParams write per tile per frame");
        // The difference ramp swaps the paint table and re-binds: still no
        // block texture uploaded.
        layers.set_ramp("pop", crate::overlay::host::DotRamp::Difference);
        gpu.sync(&mut layers, &ctx, &pipeline, &camera, &globe, None);
        assert_eq!(pipeline.dot_uploads().tile_uploads, first.tile_uploads, "the ramp re-uploaded blocks");
        assert!(gpu.drawn_params("pop").iter().all(|p| p.color_mode == crate::render::dots::COLOR_DIFFERENCE));
    }

    /// Stepping epochs (`[`/`]` in the demo) asks for the new pair's
    /// tiles, keeps the generation, and the mix is clamped. Review M-1:
    /// until the new pair has landed, the old pair's tiles keep drawing
    /// (no blank frame) and the ppd holds; once it lands, the new pair
    /// draws.
    #[test]
    fn set_dot_epochs_fetches_the_new_pair() {
        let mut app = gpui::TestAppContext::single();
        let map = map(&mut app);
        let d = ghsl_source().descriptor().clone();
        let globe = crate::render::globe::GlobeCover::FLAT;
        let settle = |v: &mut MapView| {
            for _ in 0..10 {
                v.dot_layers.frame(v.camera_control.camera(), &globe, crate::timing::Instant::now());
                while let Some(job) = v.dot_layers.next_job() {
                    v.dot_layers.land(job.run(|_| Ok(Vec::new())));
                }
            }
        };
        map.update(&mut app, |v, cx| {
            v.set_dot_density_source("pop", Arc::new(MemoryGridSource::new(d, live_tile)), DotStyle::default(), cx);
            v.set_dot_epochs("pop", 1975, Some(2030), cx);
            v.set_dot_epoch_mix("pop", 7.0, cx);
            let style = v.dot_style("pop").unwrap();
            assert_eq!((style.epoch, style.epoch_b, style.epoch_mix), (1975, Some(2030), 1.0));
            v.set_dot_epoch_mix("pop", f32::NAN, cx);
            assert_eq!(v.dot_style("pop").unwrap().mix(), 0.0);
            settle(v);
            let plan = v.dot_layers.plan("pop").unwrap();
            assert!(!plan.draw.is_empty() && plan.draw.iter().all(|x| (x.key.epoch, x.key.epoch_b) == (1975, Some(2030))));
            let ppd = v.dot_stats("pop").unwrap().ppd;

            v.set_dot_epochs("pop", 1975, Some(1980), cx);
            // Frames with nothing of the new pair landed: the old pair draws.
            for _ in 0..5 {
                v.dot_layers.frame(v.camera_control.camera(), &globe, crate::timing::Instant::now());
                let plan = v.dot_layers.plan("pop").unwrap();
                assert!(plan.wanted.iter().all(|k| k.epoch_b == Some(1980)), "the new pair is wanted");
                assert!(!plan.draw.is_empty(), "a blank frame while the new pair loads");
                assert!(plan.draw.iter().all(|x| x.key.epoch_b == Some(2030)), "the old pair stands in");
                assert_eq!(v.dot_stats("pop").unwrap().ppd, ppd, "the ppd holds");
            }
            // A second step before the first landed: still the drawn pair.
            v.set_dot_epochs("pop", 1975, Some(1985), cx);
            v.dot_layers.frame(v.camera_control.camera(), &globe, crate::timing::Instant::now());
            assert!(v.dot_layers.plan("pop").unwrap().draw.iter().all(|x| x.key.epoch_b == Some(2030)));
            // Landed: the new pair draws.
            settle(v);
            let plan = v.dot_layers.plan("pop").unwrap();
            assert!(!plan.draw.is_empty() && plan.draw.iter().all(|x| x.key.epoch_b == Some(1985)), "{plan:?}");
        });
    }

    /// The glue end to end, without a GPU: a frame plans, the jobs run on
    /// the runtime and land, and the stats count the ready tiles.
    #[test]
    fn a_dot_source_fetches_and_lands_its_cover() {
        let mut app = gpui::TestAppContext::single();
        let map = map(&mut app);
        map.update(&mut app, |v, cx| v.set_dot_density_source("pop", ghsl_source(), DotStyle::default(), cx));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            map.update(&mut app, |v, cx| {
                let globe = crate::render::globe::GlobeCover::FLAT;
                v.dot_layers.frame(v.camera_control.camera(), &globe, crate::timing::Instant::now());
                v.drive_dot_sources(cx);
            });
            app.run_until_parked();
            let stats = map.read_with(&app, |v, _| v.dot_stats("pop").expect("the layer"));
            if stats.tiles_wanted > 0 && stats.tiles_ready == stats.tiles_wanted {
                assert_eq!(stats.level, 0, "zoom 5.5 on 30″ is level 0");
                assert!(stats.ppd >= 1.0);
                assert_eq!(stats.source_errors, 0);
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the dot layer did not settle in 20 s: {stats:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
