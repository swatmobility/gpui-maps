//! `MapView`'s overlay extrusion layers: overlay polygons extruded to a
//! height per feature ([`crate::overlay::extrude`]) and drawn by the
//! basemap's `fill-extrusion` pipeline, after the basemap's own buildings
//! in the tile pass (so they share its depth buffer: a hexbin column and
//! a building occlude each other correctly), under the labels.
//!
//! A layer is an [`OverlayExtrusion`] (tessellated once, off the UI
//! thread if the caller likes) and an [`ExtrusionStyle`]. A style change
//! is a uniform write; a new set is one upload. Drawn on every frame:
//! on the globe (below z12 with it on) each vertex goes to the sphere as
//! the ground's do, raised along the sphere's normal, with a real depth
//! and a per-fragment far-side test (`extrusion_shader.wgsl`).
//!
//! Picking ([`MapView::extrusion_feature_at`], `MapView::pick`) marches the
//! cursor's ray through each layer's footprints (`overlay::extrude::pick`);
//! a highlight (`MapView::set_highlight`) is the picked column again, a
//! touch larger and lit, as one more layer drawn after them.

use std::sync::Arc;

use super::MapView;
use crate::overlay::extrude::{ExtrusionStyle, OverlayExtrusion};
use crate::overlay::FeatureProperties;
use crate::render::overlay_extrusion::OverlayExtrusionGpu;

struct Layer {
    id: String,
    set: Arc<OverlayExtrusion>,
    style: ExtrusionStyle,
    generation: u64,
    /// The host's properties per input feature (`set_extrusion_layer_properties`).
    properties: Option<Arc<[Arc<FeatureProperties>]>>,
}

/// The highlighted column: which, and the layer drawn for it.
struct Highlight {
    layer: String,
    feature: usize,
    drawn: Layer,
}

/// The layer id the highlight draws under: no host id starts with NUL.
const HIGHLIGHT_ID: &str = "\u{0}extrusion-highlight";

#[derive(Default)]
pub(super) struct ExtrusionLayers {
    layers: Vec<Layer>,
    next_generation: u64,
    highlight: Option<Highlight>,
}

/// What the GPU holds for one layer: the generation it was uploaded at.
pub(super) struct HeldExtrusion {
    id: String,
    generation: u64,
    gpu: OverlayExtrusionGpu,
}

/// What the last frame drew of the overlay extrusion layers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtrusionLayerStats {
    pub layers: usize,
    pub draws: usize,
    pub triangles: u64,
    pub uploads_total: u64,
}

impl ExtrusionLayers {
    pub(super) fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.set.is_empty())
    }

    /// Every layer drawn this frame, the highlight last.
    fn drawn(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter().chain(self.highlight.as_ref().map(|h| &h.drawn))
    }

    /// The visible column under logical `(x, y)` of a flat `camera`, across
    /// layers: the hit nearest the eye, a tie going to the later layer.
    pub(super) fn pick(&self, camera: &crate::render::Camera, x: f32, y: f32) -> Option<(String, usize)> {
        let mut best: Option<(usize, crate::overlay::extrude::ExtrusionHit)> = None;
        for (k, l) in self.layers.iter().enumerate() {
            if let Some(hit) = crate::overlay::extrude::pick(&l.set, camera, x, y) {
                if best.is_none_or(|(_, b)| hit.depth >= b.depth) {
                    best = Some((k, hit));
                }
            }
        }
        best.map(|(k, hit)| (self.layers[k].id.clone(), hit.feature))
    }

    /// [`Self::pick`] on a globe frame (`overlay::extrude::pick_on_globe`).
    pub(super) fn pick_on_globe(&self, frame: &crate::render::globe::GlobeFrame, camera: &crate::render::Camera, x: f32, y: f32) -> Option<(String, usize)> {
        let mut best: Option<(usize, crate::overlay::extrude::ExtrusionHit)> = None;
        for (k, l) in self.layers.iter().enumerate() {
            if let Some(hit) = crate::overlay::extrude::pick_on_globe(&l.set, frame, camera, x, y) {
                if best.is_none_or(|(_, b)| hit.depth >= b.depth) {
                    best = Some((k, hit));
                }
            }
        }
        best.map(|(k, hit)| (self.layers[k].id.clone(), hit.feature))
    }

    pub(super) fn properties(&self, id: &str, feature: usize) -> Option<Arc<FeatureProperties>> {
        let l = self.layers.iter().find(|l| l.id == id)?;
        l.properties.as_ref()?.get(feature).cloned()
    }

    /// Highlights `feature` of layer `id` (or nothing): the column again,
    /// grown, in its colour brightened toward `style.outline_color`.
    pub(super) fn set_highlight(&mut self, target: Option<(&str, usize)>, style: &crate::overlay::HighlightStyle) -> bool {
        self.next_generation += 1;
        let generation = self.next_generation;
        self.highlight = target.and_then(|(id, feature)| {
            let l = self.layers.iter().find(|l| l.id == id)?;
            let own = l.set.pick.feature(feature)?.color;
            let o = style.outline_color;
            let mix = |a: f32, b: f32| a + (b - a) * 0.55;
            let lit = crate::style::Rgba { r: mix(own.r, o.r), g: mix(own.g, o.g), b: mix(own.b, o.b), a: 1.0 };
            let set = crate::overlay::extrude::highlight_extrusion(&l.set, feature, lit)?;
            let drawn = Layer {
                id: HIGHLIGHT_ID.to_string(),
                set: Arc::new(set),
                style: ExtrusionStyle { occlude_labels: false, ..l.style },
                generation,
                properties: None,
            };
            Some(Highlight { layer: id.to_string(), feature, drawn })
        });
        self.highlight.is_some()
    }

    pub(super) fn highlighted(&self) -> Option<(&str, usize)> {
        self.highlight.as_ref().map(|h| (h.layer.as_str(), h.feature))
    }

    /// The highlight's own set, as drawn: what a capture of it draws.
    #[cfg(test)]
    pub(super) fn highlight_set(&self) -> Option<(Arc<OverlayExtrusion>, ExtrusionStyle)> {
        self.highlight.as_ref().map(|h| (h.drawn.set.clone(), h.drawn.style))
    }

    /// Every layer's meshes that hide labels (`ExtrusionStyle::occlude_labels`).
    pub(super) fn label_occluders(&self) -> Vec<(crate::geo::TileId, Arc<crate::render::extrusion::ExtrusionMesh>)> {
        self.layers.iter().flat_map(|l| crate::overlay::extrude::label_occluders(&l.set, &l.style)).collect()
    }

    /// Uploads what changed, drops what went, and writes this frame's
    /// uniforms. Returns the layers in draw order.
    pub(super) fn sync(
        &self,
        held: &mut Vec<HeldExtrusion>,
        ctx: &crate::render::GpuContext,
        pipeline: &crate::render::TilePipeline,
        camera: &crate::render::Camera,
        globe: &crate::render::globe::GlobeUpload,
        light: &crate::style::ExtrusionLight,
        uploads: &mut u64,
    ) {
        held.retain(|h| self.drawn().any(|l| l.id == h.id && l.generation == h.generation));
        for l in self.drawn() {
            if !held.iter().any(|h| h.id == l.id) {
                held.push(HeldExtrusion { id: l.id.clone(), generation: l.generation, gpu: OverlayExtrusionGpu::upload(ctx, pipeline, &l.set) });
                *uploads += 1;
            }
        }
        // Layer order, not upload order.
        held.sort_by_key(|h| self.drawn().position(|l| l.id == h.id));
        for h in held.iter_mut() {
            if let Some(l) = self.drawn().find(|l| l.id == h.id) {
                h.gpu.prepare(ctx, pipeline, camera, Some(globe), &l.style, light);
            }
        }
    }
}

/// Appends every held layer's draws, in layer order.
pub(super) fn push_draws<'a>(held: &'a [HeldExtrusion], out: &mut Vec<crate::render::TileDraw<'a>>) {
    for (k, h) in held.iter().enumerate() {
        h.gpu.push_draws(k, out);
    }
}

impl MapView {
    /// Draws `set` as overlay extrusion layer `id` in `style`, replacing
    /// any layer of that id. Layers draw in the order they were first set.
    pub fn set_extrusion_layer(&mut self, id: &str, set: Arc<OverlayExtrusion>, style: ExtrusionStyle, cx: &mut gpui::Context<Self>) {
        self.extrusion_layers.next_generation += 1;
        let generation = self.extrusion_layers.next_generation;
        match self.extrusion_layers.layers.iter_mut().find(|l| l.id == id) {
            Some(l) => {
                l.set = set;
                l.style = style;
                l.generation = generation;
                l.properties = None;
            }
            None => self.extrusion_layers.layers.push(Layer { id: id.to_string(), set, style, generation, properties: None }),
        }
        if self.extrusion_layers.highlight.as_ref().is_some_and(|h| h.layer == id) {
            self.extrusion_layers.highlight = None;
        }
        cx.notify();
    }

    /// Attaches the host's properties to extrusion layer `id`: one map per
    /// feature, in the order `overlay::extrude::extrude` was given them,
    /// so a pick can say what a column is. Cleared by the next
    /// `set_extrusion_layer` of that id. Returns whether the layer exists.
    pub fn set_extrusion_layer_properties(&mut self, id: &str, properties: Arc<[Arc<FeatureProperties>]>) -> bool {
        match self.extrusion_layers.layers.iter_mut().find(|l| l.id == id) {
            Some(l) => {
                l.properties = Some(properties);
                true
            }
            None => false,
        }
    }

    /// Feature `feature` of extrusion layer `id`'s properties, as
    /// [`Self::set_extrusion_layer_properties`] attached them.
    #[must_use]
    pub fn extrusion_properties(&self, id: &str, feature: usize) -> Option<Arc<FeatureProperties>> {
        self.extrusion_layers.properties(id, feature)
    }

    /// The overlay extrusion column under a point in the map's own
    /// (viewport-local) logical pixels, as `(layer id, feature index)` --
    /// the index into the features `extrude` was given. The column the
    /// screen ray meets first from the eye, roof or wall, as the
    /// depth-tested draw shows it (`overlay::extrude::pick`). On a globe
    /// frame the ray is marched over the sphere instead
    /// (`overlay::extrude::pick_on_globe`) once the frame is more globe
    /// than Mercator; the flat pick serves the first half of the z11-12
    /// blend. Basemap buildings do not block it.
    #[must_use]
    pub fn extrusion_feature_at(&self, x: f32, y: f32) -> Option<(String, usize)> {
        if self.extrusion_layers.is_empty() {
            return None;
        }
        let camera = self.camera_control.camera();
        let frame = crate::render::globe::build_globe_frame(camera).with_globe_enabled(self.globe_enabled);
        if frame.transition > 0.5 {
            return self.extrusion_layers.pick_on_globe(&frame, camera, x, y);
        }
        self.extrusion_layers.pick(camera, x, y)
    }

    /// Stops drawing extrusion layer `id`; its buffers go on the next frame.
    pub fn remove_extrusion_layer(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        let before = self.extrusion_layers.layers.len();
        self.extrusion_layers.layers.retain(|l| l.id != id);
        if self.extrusion_layers.highlight.as_ref().is_some_and(|h| h.layer == id) {
            self.extrusion_layers.highlight = None;
        }
        if self.extrusion_layers.layers.len() != before {
            cx.notify();
        }
    }

    /// Layer `id`'s opacity, gradient, light and look: a uniform write, no
    /// re-upload.
    pub fn set_extrusion_layer_style(&mut self, id: &str, style: ExtrusionStyle, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.extrusion_layers.layers.iter_mut().find(|l| l.id == id) {
            if l.style != style {
                l.style = style;
                // The highlight is drawn in its layer's style (opacity,
                // light, look), minus the label occlusion.
                if let Some(h) = self.extrusion_layers.highlight.as_mut().filter(|h| h.layer == id) {
                    h.drawn.style = ExtrusionStyle { occlude_labels: false, ..style };
                }
                cx.notify();
            }
        }
    }

    /// Layer `id`'s set and style as drawn now, or `None`: what a legend
    /// reads, so it states what is on screen.
    #[must_use]
    pub fn extrusion_layer(&self, id: &str) -> Option<(Arc<OverlayExtrusion>, ExtrusionStyle)> {
        self.extrusion_layers.layers.iter().find(|l| l.id == id).map(|l| (l.set.clone(), l.style))
    }

    /// What the last painted frame drew of the overlay extrusion layers,
    /// and how many uploads there have been.
    #[must_use]
    pub fn extrusion_layer_stats(&self) -> ExtrusionLayerStats {
        self.extrusion_layer_stats
    }
}
