//! `MapView`'s heatmap layers: screen-space kernel density
//! (`render::heatmap`), composited in the symbol pass before the arcs --
//! over every tile and overlay, under the arcs, trips, donuts, icons and
//! labels.
//!
//! A layer is a [`HeatmapSet`] (positions, weights and up to four filter
//! columns), a style and the filter windows. The points are uploaded once
//! per set; a radius, intensity, palette or window change is a uniform or
//! a 1 KB ramp write on the next frame.

use std::sync::Arc;

use super::MapView;
use crate::render::heatmap::{HeatmapFrameStats, HeatmapLayerDraw, HeatmapSet, HeatmapStyle};

struct HeatmapLayer {
    id: String,
    set: Arc<HeatmapSet>,
    style: HeatmapStyle,
    /// Absolute windows, one per filter column (`None`: off).
    windows: Vec<Option<[f64; 2]>>,
    generation: u64,
}

#[derive(Default)]
pub(super) struct HeatmapLayers {
    layers: Vec<HeatmapLayer>,
    next_generation: u64,
}

impl HeatmapLayers {
    pub(super) fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.set.is_empty() || l.style.opacity <= 0.0)
    }

    /// This frame's draws, in layer order.
    pub(super) fn draws(&self) -> Vec<HeatmapLayerDraw<'_>> {
        self.layers
            .iter()
            .filter(|l| !l.set.is_empty() && l.style.opacity > 0.0)
            .map(|l| HeatmapLayerDraw { id: &l.id, generation: l.generation, set: &l.set, style: &l.style, filter: l.set.relative_windows(&l.windows) })
            .collect()
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut HeatmapLayer> {
        self.layers.iter_mut().find(|l| l.id == id)
    }

    fn get(&self, id: &str) -> Option<&HeatmapLayer> {
        self.layers.iter().find(|l| l.id == id)
    }
}

impl MapView {
    /// Draws `set` as heatmap layer `id` in `style`, replacing any layer of
    /// that id (its filter windows are kept when the set's filter columns
    /// are the same). Layers draw in the order they were first set.
    pub fn set_heatmap_layer(&mut self, id: &str, set: Arc<HeatmapSet>, style: HeatmapStyle, cx: &mut gpui::Context<Self>) {
        self.heatmap_layers.next_generation += 1;
        let generation = self.heatmap_layers.next_generation;
        match self.heatmap_layers.get_mut(id) {
            Some(l) => {
                if l.set.filter_fields != set.filter_fields {
                    l.windows.clear();
                }
                l.set = set;
                l.style = style;
                l.generation = generation;
            }
            None => self.heatmap_layers.layers.push(HeatmapLayer { id: id.to_string(), set, style, windows: Vec::new(), generation }),
        }
        cx.notify();
    }

    /// Stops drawing heatmap layer `id`; its GPU targets go on the next
    /// frame.
    pub fn remove_heatmap_layer(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        let before = self.heatmap_layers.layers.len();
        self.heatmap_layers.layers.retain(|l| l.id != id);
        if self.heatmap_layers.layers.len() != before {
            cx.notify();
        }
    }

    /// Layer `id`'s radius, intensity, threshold, opacity and ramp: no
    /// re-upload.
    pub fn set_heatmap_style(&mut self, id: &str, style: HeatmapStyle, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.heatmap_layers.get_mut(id) {
            if l.style != style {
                l.style = style;
                cx.notify();
            }
        }
    }

    /// Layer `id`'s filter windows, absolute, one per filter column of its
    /// set in order (`None`, or a missing entry: that column filters
    /// nothing). A uniform write on the next frame.
    pub fn set_heatmap_filter_ranges(&mut self, id: &str, windows: &[Option<[f64; 2]>], cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.heatmap_layers.get_mut(id) {
            if l.windows != windows {
                l.windows = windows.to_vec();
                cx.notify();
            }
        }
    }

    /// Sets the window of the one filter column named `field` of layer
    /// `id`; `false` when the layer has no such column.
    pub fn set_heatmap_filter_field_range(&mut self, id: &str, field: &str, window: Option<[f64; 2]>, cx: &mut gpui::Context<Self>) -> bool {
        let Some(l) = self.heatmap_layers.get_mut(id) else { return false };
        let Some(k) = l.set.filter_fields.iter().position(|f| f == field) else { return false };
        if l.windows.len() <= k {
            l.windows.resize(k + 1, None);
        }
        if l.windows[k] != window {
            l.windows[k] = window;
            cx.notify();
        }
        true
    }

    /// Layer `id`'s set and style as drawn now: what a legend reads.
    #[must_use]
    pub fn heatmap_layer(&self, id: &str) -> Option<(Arc<HeatmapSet>, HeatmapStyle)> {
        self.heatmap_layers.get(id).map(|l| (l.set.clone(), l.style.clone()))
    }

    /// Layer `id`'s filter windows as set.
    #[must_use]
    pub fn heatmap_filter_ranges(&self, id: &str) -> Option<Vec<Option<[f64; 2]>>> {
        self.heatmap_layers.get(id).map(|l| l.windows.clone())
    }

    /// How many of layer `id`'s points its windows pass (and weigh
    /// something): the CPU twin of the shader's test.
    #[must_use]
    pub fn heatmap_points_passing(&self, id: &str) -> Option<usize> {
        self.heatmap_layers.get(id).map(|l| l.set.passing(&l.set.relative_windows(&l.windows)))
    }

    /// Layer `id`'s data stamp: moves on every `set_heatmap_layer`, never
    /// on a style or window change.
    #[must_use]
    pub fn heatmap_layer_generation(&self, id: &str) -> Option<u64> {
        self.heatmap_layers.get(id).map(|l| l.generation)
    }

    /// What the last painted frame drew. Zero before the first frame with
    /// a heatmap.
    #[must_use]
    pub fn heatmap_stats(&self) -> HeatmapFrameStats {
        self.gpu.borrow().as_ref().and_then(|g| g.heatmap_pipeline.as_ref().map(|p| p.stats())).unwrap_or_default()
    }

    /// Point uploads since the heatmap pass was built: one per set, none
    /// per camera, style or filter move.
    #[must_use]
    pub fn heatmap_uploads_total(&self) -> u64 {
        self.gpu.borrow().as_ref().and_then(|g| g.heatmap_pipeline.as_ref().map(|p| p.uploads_total())).unwrap_or_default()
    }
}
