//! `MapView`'s donut clusters: the public API, what a frame draws, and the
//! screen-space hit test.
//!
//! Donuts are drawn by `render::donut::DonutPipeline` at the start of the
//! symbol pass: over every overlay the tile pass drew, under the labels.
//! Each anchor goes through the icon path's projection
//! (`render::globe::project_world_to_screen_on_frame`), so a donut sits on
//! the sphere where its point is; a point the planet hides draws at alpha
//! 0. See spec `2026-09-27-us-race-dot-map-design.md` §5.2.

use super::MapView;
use crate::render::donut::{
    donut_instance, palette_uniform, Donut, DonutBatch, DonutInstance, MAX_DONUTS,
};
use crate::style::Rgba;

/// One layer of donuts, in draw order within the layer.
struct DonutLayer {
    id: String,
    /// Each donut's Mercator world point, converted once. ONE world copy:
    /// on a flat map zoomed out past one world width a donut draws in one
    /// copy only (the US race map is on the globe at those zooms).
    world: Vec<(f64, f64)>,
    /// Each donut's instance at scale 1 with its anchor unset: the
    /// cumulative shares and the sanitised alpha (`render::donut::visible_alpha`) are
    /// computed once here, not once per frame. The hit test reads the same
    /// `alpha` and `radius_px` the draw does.
    templates: Vec<DonutInstance>,
    palette: [[f32; 4]; 8],
}

/// Every donut layer, in the order they were first set (later draws on
/// top), and the kept per-frame output.
#[derive(Default)]
pub(super) struct DonutLayers {
    layers: Vec<DonutLayer>,
    /// This frame's instances, every layer's back to back, and each
    /// layer's range of them. Cleared and refilled by [`Self::frame`],
    /// never reallocated once big enough.
    instances: Vec<DonutInstance>,
    batches: Vec<DonutBatch>,
    /// Donuts dropped past [`MAX_DONUTS`], over this map's lifetime.
    truncated: u64,
    /// Layers already warned about, so the log line is once per layer.
    warned: std::collections::HashSet<String>,
}

impl DonutLayers {
    /// Replaces layer `id`'s donuts in place (keeping its draw position),
    /// or adds it on top. Keeps the first [`MAX_DONUTS`].
    pub(super) fn set(&mut self, id: &str, mut donuts: Vec<Donut>, palette: [Rgba; 8]) {
        if donuts.len() > MAX_DONUTS {
            let dropped = donuts.len() - MAX_DONUTS;
            self.truncated += dropped as u64;
            if self.warned.insert(id.to_string()) {
                eprintln!("gpui-maps: donut layer {id:?} has {} donuts; drawing the first {MAX_DONUTS}", donuts.len());
            }
            donuts.truncate(MAX_DONUTS);
        }
        let world = donuts.iter().map(|d| crate::overlay::geom::lon_lat_to_world(d.lon, d.lat)).collect();
        let templates = donuts.iter().map(|d| donut_instance(d, Some((0.0, 0.0)), 1.0)).collect();
        let layer = DonutLayer { id: id.to_string(), world, templates, palette: palette_uniform(&palette) };
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(existing) => *existing = layer,
            None => self.layers.push(layer),
        }
    }

    pub(super) fn remove(&mut self, id: &str) -> bool {
        let before = self.layers.len();
        self.layers.retain(|l| l.id != id);
        self.layers.len() != before
    }

    pub(super) fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.templates.is_empty())
    }

    pub(super) fn truncated(&self) -> u64 {
        self.truncated
    }

    /// Fills this frame's instances and batches: `project` maps a world
    /// point to LOGICAL pixels (`None` when hidden), `scale` is the device
    /// pixel ratio, `viewport` the logical size. Only what can show is
    /// emitted: a hidden anchor, an invisible donut (alpha 0, no people,
    /// radius 0) or a disc wholly off screen is left out, so the upload is
    /// bounded by what is on screen.
    pub(super) fn frame(
        &mut self,
        project: impl Fn((f64, f64)) -> Option<(f32, f32)>,
        scale: f32,
        viewport: (f32, f32),
    ) -> (&[DonutInstance], &[DonutBatch]) {
        self.instances.clear();
        self.batches.clear();
        for layer in &self.layers {
            let start = self.instances.len() as u32;
            for (t, &w) in layer.templates.iter().zip(&layer.world) {
                if t.alpha <= 0.0 || t.radius_px <= 0.0 {
                    continue;
                }
                let Some((x, y)) = project(w) else { continue };
                let r = t.radius_px + 1.0;
                if x + r < 0.0 || y + r < 0.0 || x - r > viewport.0 || y - r > viewport.1 {
                    continue;
                }
                let mut i = *t;
                i.radius_px *= scale;
                i.anchor_px = [x * scale, y * scale];
                self.instances.push(i);
            }
            let end = self.instances.len() as u32;
            if end > start {
                self.batches.push((layer.palette, start..end));
            }
        }
        (&self.instances, &self.batches)
    }

    /// What the last [`Self::frame`] filled.
    pub(super) fn drawn(&self) -> (&[DonutInstance], &[DonutBatch]) {
        (&self.instances, &self.batches)
    }

    /// The topmost visible donut whose disc holds `(x, y)` (logical
    /// pixels): the last layer first, and within a layer the last donut
    /// first, which is the reverse of draw order. Visible means what the
    /// draw means: the template's sanitised alpha and radius.
    pub(super) fn hit(&self, project: impl Fn((f64, f64)) -> Option<(f32, f32)>, x: f32, y: f32) -> Option<(String, usize)> {
        for layer in self.layers.iter().rev() {
            for (i, (t, &w)) in layer.templates.iter().zip(&layer.world).enumerate().rev() {
                if t.alpha <= 0.0 || t.radius_px <= 0.0 {
                    continue;
                }
                let Some((ax, ay)) = project(w) else { continue };
                if (x - ax).hypot(y - ay) <= t.radius_px {
                    return Some((layer.id.clone(), i));
                }
            }
        }
        None
    }
}

impl MapView {
    /// Draws `donuts` as layer `id`, replacing that layer's donuts if it
    /// exists (it keeps its place in the draw order) or adding it over the
    /// others. `palette[k]` colours group `k`'s sector; the ring is
    /// `render::donut::DONUT_HOLE` of the radius in, with a white centre
    /// and a 1 px white stroke. Donuts draw over the overlays and under the
    /// labels.
    ///
    /// - At most [`MAX_DONUTS`] (8,192) per layer: the rest are dropped,
    ///   logged once per layer and counted in [`Self::donuts_truncated`].
    ///   Walker's cluster view has far fewer than that in view at any zoom.
    /// - A donut whose counts are all zero, or whose alpha is NaN or below
    ///   `render::donut::MIN_VISIBLE_ALPHA`, is not drawn and not hit.
    /// - Each donut is drawn in one world copy (see `DonutLayer::world`).
    pub fn set_donuts(&mut self, id: &str, donuts: Vec<Donut>, palette: [Rgba; 8], cx: &mut gpui::Context<Self>) {
        self.donut_layers.set(id, donuts, palette);
        cx.notify();
    }

    /// Stops drawing donut layer `id`.
    pub fn remove_donuts(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        if self.donut_layers.remove(id) {
            cx.notify();
        }
    }

    /// How many donuts [`Self::set_donuts`] has dropped past
    /// [`MAX_DONUTS`], over this map's lifetime.
    #[must_use]
    pub fn donuts_truncated(&self) -> u64 {
        self.donut_layers.truncated()
    }

    /// The donut under `(x, y)` (logical pixels, as [`Self::feature_at`]
    /// takes them): its layer id and its index in the `Vec` given to
    /// [`Self::set_donuts`]. The topmost wins; a donut that does not draw
    /// (alpha 0, no people, the far side of the globe) is never hit.
    /// Projected with the camera at call time, which is the same one-frame
    /// lag [`Self::feature_at`] has.
    #[must_use]
    pub fn donut_at(&self, x: f32, y: f32) -> Option<(String, usize)> {
        if self.donut_layers.is_empty() {
            return None;
        }
        let camera = self.camera_control.camera();
        // The frame as drawn, sphere included -- without registering a
        // second per-frame `globe_frame` build (see `build_globe_frame`).
        let frame = crate::render::globe::build_globe_frame(camera).with_globe_enabled(self.globe_enabled);
        let anchors = crate::render::globe::GlobeAnchors::new(&frame);
        // One matrix for every donut, not one per donut.
        let projector = crate::render::globe::WorldProjector::new(camera, &anchors);
        self.donut_layers.hit(|w| projector.project(w), x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn donut(lon: f64, radius_px: f32, alpha: f32) -> Donut {
        Donut { lon, lat: 0.0, radius_px, counts: [1, 1, 0, 0, 0, 0, 0, 0], alpha }
    }

    /// World x straight to screen x (y 0), so a donut's lon picks its spot.
    fn project(w: (f64, f64)) -> Option<(f32, f32)> {
        Some(((w.0 / f64::from(crate::render::WORLD_SIZE) * 1000.0) as f32, 0.0))
    }

    fn at(lon: f64) -> f32 {
        project(crate::overlay::geom::lon_lat_to_world(lon, 0.0)).unwrap().0
    }

    #[test]
    fn the_hit_test_takes_the_topmost_donut_within_its_radius() {
        let pal = [Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }; 8];
        let mut layers = DonutLayers::default();
        layers.set("a", vec![donut(0.0, 10.0, 1.0), donut(1.0, 10.0, 1.0)], pal);
        layers.set("b", vec![donut(0.0, 4.0, 1.0), donut(90.0, 10.0, 0.0)], pal);
        let x = at(0.0);
        // Layer b's small donut is on top at its centre...
        assert_eq!(layers.hit(project, x, 0.0), Some(("b".into(), 0)));
        // ...and outside its 4 px, layer a's later donut, then its first.
        assert_eq!(layers.hit(project, x + 6.0, 0.0), Some(("a".into(), 1)));
        assert_eq!(layers.hit(project, x - 9.0, 0.0), Some(("a".into(), 0)));
        assert_eq!(layers.hit(project, x - 11.0, 0.0), None);
        // A donut at alpha 0 is never hit.
        assert_eq!(layers.hit(project, at(90.0), 0.0), None);
        // Nor is a hidden anchor.
        assert_eq!(layers.hit(|_| None, x, 0.0), None);
    }

    const VIEW: (f32, f32) = (1000.0, 100.0);

    #[test]
    fn a_frame_scales_to_physical_pixels_and_leaves_out_what_cannot_show() {
        let pal = [Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }; 8];
        let mut layers = DonutLayers::default();
        layers.set("a", vec![donut(0.0, 10.0, 0.5), donut(0.0, 10.0, 0.0), donut(179.0, 10.0, 1.0)], pal);
        layers.set("empty", Vec::new(), pal);
        {
            let (instances, batches) = layers.frame(project, 2.0, VIEW);
            assert_eq!(batches.len(), 1, "an empty layer draws nothing");
            assert_eq!(batches[0].0[0], [1.0, 0.0, 0.0, 1.0]);
            // The alpha-0 donut is left out; the one at lon 179 is on screen.
            assert_eq!(batches[0].1, 0..2);
            assert_eq!(instances[0].anchor_px, [at(0.0) * 2.0, 0.0]);
            assert_eq!((instances[0].radius_px, instances[0].alpha), (20.0, 0.5));
            assert_eq!(instances[0].cum[0], 0.5);
        }
        // Off screen by more than its radius: left out.
        assert_eq!(layers.frame(project, 1.0, (at(90.0), 100.0)).0.len(), 1);
        // Hidden by the planet: left out.
        assert!(layers.frame(|_| None, 2.0, VIEW).1.is_empty());
        // Replacing a layer keeps its place; removing it reports it.
        layers.set("b", vec![donut(0.0, 1.0, 1.0)], pal);
        layers.set("a", vec![donut(0.0, 3.0, 1.0)], pal);
        assert_eq!(layers.frame(project, 1.0, VIEW).0[0].radius_px, 3.0);
        assert!(layers.remove("a") && !layers.remove("a"));
    }

    /// Review finding 6: the frame's Vecs are kept, not reallocated.
    #[test]
    fn a_steady_frame_reuses_its_buffers() {
        let pal = [Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }; 8];
        let mut layers = DonutLayers::default();
        layers.set("a", (0..100).map(|i| donut(f64::from(i) - 50.0, 5.0, 1.0)).collect(), pal);
        let first = layers.frame(project, 1.0, VIEW).0.as_ptr();
        let again = layers.frame(project, 1.0, VIEW).0.as_ptr();
        assert_eq!(first, again);
    }

    /// Review finding 1: the cap holds on both sides of it.
    #[test]
    fn a_layer_keeps_at_most_max_donuts() {
        let pal = [Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }; 8];
        let mut layers = DonutLayers::default();
        layers.set("at", vec![donut(0.0, 1.0, 1.0); MAX_DONUTS], pal);
        assert_eq!((layers.layers[0].templates.len(), layers.truncated()), (MAX_DONUTS, 0));
        layers.set("over", vec![donut(0.0, 1.0, 1.0); MAX_DONUTS + 1], pal);
        assert_eq!((layers.layers[1].templates.len(), layers.layers[1].world.len()), (MAX_DONUTS, MAX_DONUTS));
        assert_eq!(layers.truncated(), 1);
        // Once per layer in the log; every time in the count.
        layers.set("over", vec![donut(0.0, 1.0, 1.0); MAX_DONUTS + 5], pal);
        assert_eq!((layers.truncated(), layers.warned.len()), (6, 1));
        // The instance buffer's worst case, as the doc states it.
        assert_eq!(MAX_DONUTS * std::mem::size_of::<DonutInstance>(), 512 * 1024);
    }

    /// Review findings 3 and 7: NaN alpha and all-zero counts neither draw
    /// nor hit, and the draw and the hit test use one threshold.
    #[test]
    fn nan_alpha_and_empty_donuts_neither_draw_nor_hit() {
        let pal = [Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }; 8];
        let mut layers = DonutLayers::default();
        let empty = Donut { counts: [0; 8], ..donut(0.0, 10.0, 1.0) };
        let faint = donut(0.0, 10.0, 5e-5);
        layers.set("a", vec![donut(0.0, 10.0, f32::NAN), empty, faint, donut(0.0, f32::NAN, 1.0)], pal);
        assert!(layers.frame(project, 1.0, VIEW).0.is_empty());
        assert_eq!(layers.hit(project, at(0.0), 0.0), None);
    }

    /// Review finding 4: the per-frame CPU half of MG-R13 -- 2,000 donuts
    /// over the continental US on the z4 globe, projected (one matrix per
    /// frame, `WorldProjector`), culled and packed by `DonutLayers::frame`.
    /// Median of 200. Run in release for the real figure.
    #[test]
    #[ignore = "timing (MG-R13 CPU half); run on demand with --release --nocapture"]
    fn donut_frame_cpu_at_2000_donuts() {
        let camera = crate::render::Camera {
            center_lon: -98.0,
            center_lat: 39.0,
            zoom: 4.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1280.0,
            viewport_height_px: 800.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let pal = [Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }; 8];
        let mut seed = 99u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            f64::from(seed >> 8) / f64::from(1u32 << 24)
        };
        let donuts: Vec<Donut> = (0..2000)
            .map(|_| Donut {
                lon: -124.0 + 57.0 * rnd(),
                lat: 25.0 + 24.0 * rnd(),
                radius_px: 6.0 + 26.0 * rnd() as f32,
                counts: std::array::from_fn(|_| (rnd() * 1000.0) as u32),
                alpha: 1.0,
            })
            .collect();
        let mut layers = DonutLayers::default();
        layers.set("us", donuts, pal);
        let frame = crate::render::globe::globe_frame(&camera);
        let anchors = crate::render::globe::GlobeAnchors::new(&frame);
        let mut times = Vec::new();
        let mut drawn = 0;
        for _ in 0..200 {
            let t = crate::timing::Instant::now();
            let projector = crate::render::globe::WorldProjector::new(&camera, &anchors);
            drawn = layers.frame(|w| projector.project(w), 2.0, (1280.0, 800.0)).0.len();
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        times.sort_by(f64::total_cmp);
        // The helper called per donut, as round 0 did, for comparison.
        let t = crate::timing::Instant::now();
        let mut n = 0;
        for w in &layers.layers[0].world {
            n += usize::from(crate::render::globe::project_world_to_screen_on_frame(&camera, &anchors, *w).is_some());
        }
        let per_donut = t.elapsed().as_secs_f64() * 1e3;
        eprintln!(
            "donut frame CPU, 2,000 donuts, z4 globe over the US: median {:.3} ms, p95 {:.3} ms ({drawn} drawn, globe: {}); \
             the per-donut helper (a matrix each) {per_donut:.3} ms for {n}",
            times[100],
            times[190],
            anchors.is_globe(),
        );
        assert_eq!(drawn, 2000, "all of the US is in view at z4");
    }
}
