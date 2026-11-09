//! `MapView`'s arc layers: origin-destination arcs (`render::arcs`), drawn
//! at the start of the symbol pass -- over every tile and overlay, under
//! the donuts, icons and labels.
//!
//! A layer is an [`ArcSet`] (built with `overlay::arcs::build`), a style
//! and an optional filter window. Setting the window is a uniform write on
//! the next frame: the instances are uploaded once per set, never per
//! window (`arc_uploads_total` holds still through a playback).
//!
//! Picking ([`MapView::arc_at`], `MapView::pick`) reads a
//! `render::arc_pick::ArcPickIndex` built once per set, OFF the UI thread
//! as `set_arc_layer` lands it (about 150 ms at 680k arcs) and keyed by
//! the layer's generation; until it lands the layer picks nothing. A map
//! with no runtime builds it on the first pick instead. A host that built
//! one already hands it over with [`MapView::set_arc_pick_index`]. A
//! highlight (`MapView::set_highlight`) is one more draw after every layer.

use std::sync::{Arc, OnceLock};

use super::MapView;
use crate::overlay::FeatureProperties;
use crate::render::arc_pick::ArcPickIndex;
use crate::render::arcs::{ArcBlend, ArcFrame, ArcFrameStats, ArcLayerDraw, ArcSet, ArcStyle};

struct ArcLayer {
    id: String,
    set: Arc<ArcSet>,
    style: ArcStyle,
    window: Option<[f64; 2]>,
    generation: u64,
    /// The pick index for THIS generation's set: landed by the build
    /// `set_arc_layer` spawned, or handed over by the host; on a map with
    /// no runtime, built on the first pick.
    pick: OnceLock<Arc<ArcPickIndex>>,
    /// The host's properties per input row (`set_arc_layer_properties`).
    properties: Option<Arc<[Arc<FeatureProperties>]>>,
}

impl ArcLayer {
    fn filter(&self) -> Option<[f32; 2]> {
        self.window.and_then(|w| self.set.relative_window(w))
    }
}

/// The highlighted arc, drawn again as a casing and a brighter core.
struct ArcHighlight {
    layer: String,
    index: usize,
    set: ArcSet,
    generation: u64,
}

/// The GPU layer id the highlight draws under: no host id starts with NUL.
const HIGHLIGHT_ID: &str = "\u{0}arc-highlight";

#[derive(Default)]
pub(super) struct ArcLayers {
    layers: Vec<ArcLayer>,
    next_generation: u64,
    highlight: Option<ArcHighlight>,
}

impl ArcLayers {
    pub(super) fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.set.is_empty())
    }

    /// This frame's draws, in layer order, then the highlight.
    pub(super) fn draws(&self) -> Vec<ArcLayerDraw<'_>> {
        let mut draws: Vec<ArcLayerDraw<'_>> = self
            .layers
            .iter()
            .filter(|l| !l.set.is_empty())
            .map(|l| ArcLayerDraw { id: &l.id, generation: l.generation, set: &l.set, style: l.style, filter: l.filter() })
            .collect();
        if let Some(h) = &self.highlight {
            if let Some(l) = self.layers.iter().find(|l| l.id == h.layer && !l.set.is_empty()) {
                draws.push(ArcLayerDraw {
                    id: HIGHLIGHT_ID,
                    generation: h.generation,
                    set: &h.set,
                    // Over the layer, opaque and "over" whatever the layer's
                    // own blending: a highlight must read on a glowing mass.
                    style: ArcStyle { opacity: 1.0, blend: ArcBlend::Normal, ..l.style },
                    filter: h.set.filter.and(l.filter()),
                });
            }
        }
        draws
    }

    /// Puts the named layers in `names`' order (bottom first) among the
    /// slots they hold; see `OverlayStore::set_source_order`.
    pub(super) fn set_order(&mut self, names: &[&str]) -> bool {
        let picks = names.iter().filter_map(|name| self.layers.iter().position(|l| l.id == *name)).collect();
        crate::overlay::permute_among_slots(&mut self.layers, picks)
    }

    /// The layer ids, in draw order (bottom first).
    pub(super) fn ids(&self) -> Vec<String> {
        self.layers.iter().map(|l| l.id.clone()).collect()
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut ArcLayer> {
        self.layers.iter_mut().find(|l| l.id == id)
    }

    /// The arc under logical `(x, y)` on `frame` across every layer: the
    /// nearest hit, a tie going to the later layer (drawn on top). `(layer,
    /// index, distance px)`. A layer at opacity 0 draws nothing and picks
    /// nothing (a hidden layer must not take the card from what is drawn
    /// beneath it).
    /// `lazy`: build a missing index here (a map with no runtime); else a
    /// layer whose index has not landed picks nothing.
    pub(super) fn pick(&self, frame: &ArcFrame, x: f64, y: f64, tolerance_px: f64, lazy: bool) -> Option<(String, usize, f64)> {
        let mut best: Option<(usize, usize, f64)> = None;
        for (k, l) in self.layers.iter().enumerate() {
            if l.set.is_empty() || l.style.opacity <= 0.0 {
                continue;
            }
            let index = if lazy {
                l.pick.get_or_init(|| Arc::new(ArcPickIndex::new(&l.set)))
            } else {
                match l.pick.get() {
                    Some(index) => index,
                    None => continue,
                }
            };
            if let Some(hit) = crate::render::arc_pick::pick(frame, &l.set, index, &l.style, l.filter(), x, y, tolerance_px) {
                // A later layer draws on top: its arc under the cursor's
                // own pixel wins outright; otherwise the nearer centreline
                // does, unless the earlier one is under the cursor.
                let under = |d: f64| d <= crate::render::arc_pick::UNDER_CURSOR_PX;
                let better = match best {
                    None => true,
                    Some((_, _, d)) => under(hit.distance_px) || (!under(d) && hit.distance_px <= d),
                };
                if better {
                    best = Some((k, hit.index, hit.distance_px));
                }
            }
        }
        best.map(|(k, i, d)| (self.layers[k].id.clone(), i, d))
    }

    /// Arc `index` of layer `id`'s properties: the host's for its row, plus
    /// its ends as `source_lon`, `source_lat`, `target_lon`, `target_lat`
    /// and its input row as `row`.
    pub(super) fn properties(&self, id: &str, index: usize) -> Option<Arc<FeatureProperties>> {
        use crate::mvt::TagValue;
        let l = self.layers.iter().find(|l| l.id == id)?;
        let a = l.set.instances.get(index)?;
        let row = l.set.row(index)?;
        let mut props: FeatureProperties = l.properties.as_ref().and_then(|p| p.get(row)).map(|p| (**p).clone()).unwrap_or_default();
        for (k, v) in [("source_lon", a.source[0]), ("source_lat", a.source[1]), ("target_lon", a.target[0]), ("target_lat", a.target[1])] {
            props.entry(k.to_string()).or_insert(TagValue::Number(f64::from(v)));
        }
        props.entry("row".to_string()).or_insert(TagValue::Number(row as f64));
        Some(Arc::new(props))
    }

    /// Highlights arc `index` of layer `id` (or, with `None`, nothing).
    /// `false` for a layer or index that is not drawn.
    pub(super) fn set_highlight(&mut self, target: Option<(&str, usize)>, style: &crate::overlay::HighlightStyle) -> bool {
        self.next_generation += 1;
        let generation = self.next_generation;
        self.highlight = target.and_then(|(id, index)| {
            let l = self.layers.iter().find(|l| l.id == id)?;
            let set = crate::overlay::arcs::highlight(&l.set, index, style)?;
            Some(ArcHighlight { layer: id.to_string(), index, set, generation })
        });
        self.highlight.is_some()
    }

    pub(super) fn highlighted(&self) -> Option<(&str, usize)> {
        self.highlight.as_ref().map(|h| (h.layer.as_str(), h.index))
    }

    /// Installs `index` for layer `id` if the layer is still at
    /// `generation` (a build for a replaced set is dropped). Returns
    /// whether it was installed.
    fn land_index(&mut self, id: &str, generation: u64, index: Arc<ArcPickIndex>) -> bool {
        match self.layers.iter_mut().find(|l| l.id == id && l.generation == generation) {
            Some(l) => l.pick.set(index).is_ok(),
            None => false,
        }
    }

    /// Whether layer `id`'s pick index has landed.
    pub(super) fn index_ready(&self, id: &str) -> bool {
        self.layers.iter().find(|l| l.id == id).is_some_and(|l| l.pick.get().is_some())
    }
}

impl MapView {
    /// Draws `set` as arc layer `id` in `style`, replacing any layer of that
    /// id (its filter window is kept; its properties and any highlight of it
    /// are not). Layers draw in the order they were first set.
    pub fn set_arc_layer(&mut self, id: &str, set: Arc<ArcSet>, style: ArcStyle, cx: &mut gpui::Context<Self>) {
        self.arc_layers.next_generation += 1;
        let generation = self.arc_layers.next_generation;
        match self.arc_layers.get_mut(id) {
            Some(l) => {
                l.set = set;
                l.style = style;
                l.generation = generation;
                l.pick = OnceLock::new();
                l.properties = None;
            }
            None => self.arc_layers.layers.push(ArcLayer {
                id: id.to_string(),
                set,
                style,
                window: None,
                generation,
                pick: OnceLock::new(),
                properties: None,
            }),
        }
        if self.arc_layers.highlight.as_ref().is_some_and(|h| h.layer == id) {
            self.arc_layers.highlight = None;
        }
        self.spawn_arc_index(id, generation, cx);
        cx.notify();
    }

    /// Builds layer `id`'s pick index on the runtime's blocking pool and
    /// lands it against `generation`. Nothing without a runtime: the first
    /// pick builds it then.
    fn spawn_arc_index(&mut self, id: &str, generation: u64, cx: &mut gpui::Context<Self>) {
        let Some(runtime) = self.runtime.clone() else { return };
        let Some(set) = self.arc_layers.layers.iter().find(|l| l.id == id).map(|l| Arc::clone(&l.set)) else { return };
        if set.is_empty() {
            return;
        }
        let id = id.to_string();
        cx.spawn(async move |entity, cx| {
            let Ok(index) = runtime.spawn_blocking(move || Arc::new(ArcPickIndex::new(&set))).await else { return };
            let _ = entity.update(cx, |view, _| view.arc_layers.land_index(&id, generation, index));
        })
        .detach();
    }

    /// Hands over a pick index the host built for layer `id`'s current set
    /// (`render::arc_pick::ArcPickIndex::new`, on any thread), so the layer
    /// picks at once rather than after its own build lands. Refused
    /// (`false`) for a layer that is not set, or whose set's arc count the
    /// index does not match.
    pub fn set_arc_pick_index(&mut self, id: &str, index: Arc<ArcPickIndex>) -> bool {
        let Some(l) = self.arc_layers.layers.iter().find(|l| l.id == id) else { return false };
        if index.arcs() != l.set.len() {
            return false;
        }
        let generation = l.generation;
        self.arc_layers.land_index(id, generation, index) || self.arc_layers.index_ready(id)
    }

    /// Whether arc layer `id` can be picked yet: its index has landed (or
    /// the map has no runtime, and builds it on the first pick).
    #[must_use]
    pub fn arc_layer_pickable(&self, id: &str) -> bool {
        self.runtime.is_none() || self.arc_layers.index_ready(id)
    }

    /// Stops drawing arc layer `id`; its GPU buffer goes on the next frame.
    pub fn remove_arc_layer(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        let before = self.arc_layers.layers.len();
        self.arc_layers.layers.retain(|l| l.id != id);
        if self.arc_layers.highlight.as_ref().is_some_and(|h| h.layer == id) {
            self.arc_layers.highlight = None;
        }
        if self.arc_layers.layers.len() != before {
            cx.notify();
        }
    }

    /// Attaches the host's properties to arc layer `id`: one map per INPUT
    /// row (the order `overlay::arcs::build` was given its columns in), so
    /// a pick (`MapView::pick`, `MapView::arc_properties`) can say what an
    /// arc is. Cleared by the next `set_arc_layer` of that id. Returns
    /// whether the layer exists.
    pub fn set_arc_layer_properties(&mut self, id: &str, properties: Arc<[Arc<FeatureProperties>]>) -> bool {
        match self.arc_layers.get_mut(id) {
            Some(l) => {
                l.properties = Some(properties);
                true
            }
            None => false,
        }
    }

    /// Arc `index` (its index in the layer's `ArcSet`, as a pick returns
    /// it) of layer `id`: the host's properties for its row
    /// ([`Self::set_arc_layer_properties`]) plus `source_lon`, `source_lat`,
    /// `target_lon`, `target_lat` and `row`.
    #[must_use]
    pub fn arc_properties(&self, id: &str, index: usize) -> Option<Arc<FeatureProperties>> {
        self.arc_layers.properties(id, index)
    }

    /// The arc under a point in the map's own (viewport-local) logical
    /// pixels, as `(layer id, index in its ArcSet)`: the drawn polyline,
    /// flat, globe or between, at the drawn width plus
    /// [`MapView::ARC_TOLERANCE_PX`] (arcs are thin, and a commute map is
    /// dense); the topmost arc whose centreline crosses the
    /// cursor's pixel, else the nearest. An arc the layer's filter window
    /// hides is never returned. A layer whose pick index has not landed
    /// yet (built off the UI thread by `set_arc_layer`, about 150 ms for
    /// 680k arcs; see [`Self::arc_layer_pickable`]) is not hit; once it
    /// has, a pick takes well under a millisecond at the median.
    #[must_use]
    pub fn arc_at(&self, x: f32, y: f32) -> Option<(String, usize)> {
        if self.arc_layers.is_empty() {
            return None;
        }
        let camera = self.camera_control.camera();
        let globe = crate::render::globe::globe_frame(camera).with_globe_enabled(self.globe_enabled);
        let frame = ArcFrame::new(camera, &globe, 1.0);
        self.arc_layers.pick(&frame, f64::from(x), f64::from(y), Self::ARC_TOLERANCE_PX, self.runtime.is_none()).map(|(id, i, _)| (id, i))
    }

    /// Layer `id`'s style (opacity, height, blending): a uniform write, no
    /// re-upload.
    pub fn set_arc_style(&mut self, id: &str, style: ArcStyle, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.arc_layers.get_mut(id) {
            if l.style != style {
                l.style = style;
                cx.notify();
            }
        }
    }

    /// Shows only the arcs of `id` whose filter value lies in the closed
    /// window `range` (absolute, in the column's units), or with `None` all
    /// of them. A uniform write on the next frame; a layer whose set has no
    /// filter values ignores it.
    pub fn set_arc_filter_range(&mut self, id: &str, range: Option<[f64; 2]>, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.arc_layers.get_mut(id) {
            if l.window != range {
                l.window = range;
                cx.notify();
            }
        }
    }

    /// Layer `id`'s set and style as drawn now, or `None` when no such
    /// layer is set: what a legend reads, so it states what is on screen.
    #[must_use]
    pub fn arc_layer(&self, id: &str) -> Option<(Arc<ArcSet>, ArcStyle)> {
        self.arc_layers.layers.iter().find(|l| l.id == id).map(|l| (l.set.clone(), l.style))
    }

    /// The window [`Self::set_arc_filter_range`] set.
    #[must_use]
    pub fn arc_filter_range(&self, id: &str) -> Option<[f64; 2]> {
        self.arc_layers.layers.iter().find(|l| l.id == id).and_then(|l| l.window)
    }

    /// Layer `id`'s data stamp: moves on every `set_arc_layer`, never on a
    /// style or window change.
    #[must_use]
    pub fn arc_layer_generation(&self, id: &str) -> Option<u64> {
        self.arc_layers.layers.iter().find(|l| l.id == id).map(|l| l.generation)
    }

    /// What the last painted frame drew: arcs, draws (one per length class),
    /// vertices and bytes uploaded. Zero before the first frame with arcs.
    #[must_use]
    pub fn arc_stats(&self) -> ArcFrameStats {
        self.gpu.borrow().as_ref().and_then(|g| g.arc_pipeline.as_ref().map(|p| p.stats())).unwrap_or_default()
    }

    /// Instance uploads since the arc pass was built: one per layer set,
    /// none per filter window.
    #[must_use]
    pub fn arc_uploads_total(&self) -> u64 {
        self.gpu.borrow().as_ref().and_then(|g| g.arc_pipeline.as_ref().map(|p| p.uploads_total())).unwrap_or_default()
    }

    /// This frame's arc draws, the highlight's included: what `paint`
    /// hands the arc pipeline.
    #[cfg(test)]
    pub(super) fn arc_draw_count(&self) -> usize {
        self.arc_layers.draws().len()
    }
}
