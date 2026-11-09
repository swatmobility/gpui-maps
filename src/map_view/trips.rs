//! `MapView`'s trip layers: animated paths (`render::trips`), drawn in the
//! symbol pass right after the arcs -- over every tile and overlay, under
//! the donuts, icons and labels.
//!
//! A layer is a [`TripSet`] (built with `TripSet::from_trips` or
//! `overlay::trips`), a style and a clock. Setting the clock is a uniform
//! write on the next frame: the vertices are uploaded once per set, never
//! per clock tick (`trip_uploads_total` holds still through a playback),
//! and the CPU work per tick is one uniform per layer whatever the data.

use std::sync::Arc;

use super::MapView;
use crate::render::trips::{TripFrameStats, TripLayerDraw, TripSet, TripStyle};

struct TripLayer {
    id: String,
    set: Arc<TripSet>,
    style: TripStyle,
    clock: Option<f64>,
    generation: u64,
}

#[derive(Default)]
pub(super) struct TripLayers {
    layers: Vec<TripLayer>,
    next_generation: u64,
    /// Clock writes that changed a layer's clock (a test's counter).
    clock_writes: u64,
}

impl TripLayers {
    pub(super) fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.set.is_empty())
    }

    /// This frame's draws, in layer order.
    pub(super) fn draws(&self) -> Vec<TripLayerDraw<'_>> {
        self.layers
            .iter()
            .filter(|l| !l.set.is_empty())
            .map(|l| TripLayerDraw { id: &l.id, generation: l.generation, set: &l.set, style: l.style, clock: l.clock })
            .collect()
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut TripLayer> {
        self.layers.iter_mut().find(|l| l.id == id)
    }

    fn get(&self, id: &str) -> Option<&TripLayer> {
        self.layers.iter().find(|l| l.id == id)
    }
}

impl MapView {
    /// Trip heads are hit within this many logical pixels of their drawn
    /// edge: a head is a point, not a line, so more slack than an arc's.
    pub const TRIP_TOLERANCE_PX: f64 = 6.0;

    /// Draws `set` as trip layer `id` in `style`, replacing any layer of that
    /// id (its clock is kept). Layers draw in the order they were first set.
    /// A new layer starts with no clock: every path whole.
    pub fn set_trip_layer(&mut self, id: &str, set: Arc<TripSet>, style: TripStyle, cx: &mut gpui::Context<Self>) {
        self.trip_layers.next_generation += 1;
        let generation = self.trip_layers.next_generation;
        match self.trip_layers.get_mut(id) {
            Some(l) => {
                l.set = set;
                l.style = style;
                l.generation = generation;
            }
            None => self.trip_layers.layers.push(TripLayer { id: id.to_string(), set, style, clock: None, generation }),
        }
        cx.notify();
    }

    /// Stops drawing trip layer `id`; its GPU buffer goes on the next frame.
    pub fn remove_trip_layer(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        let before = self.trip_layers.layers.len();
        self.trip_layers.layers.retain(|l| l.id != id);
        if self.trip_layers.layers.len() != before {
            cx.notify();
        }
    }

    /// Layer `id`'s style (width, opacity, trail, fade, blending, altitude):
    /// a uniform write, no re-upload.
    pub fn set_trip_style(&mut self, id: &str, style: TripStyle, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.trip_layers.get_mut(id) {
            if l.style != style {
                l.style = style;
                cx.notify();
            }
        }
    }

    /// The scene clock of layer `id`, absolute in the set's time unit: each
    /// trip is drawn up to it, with the style's trail behind its head. `None`
    /// draws every path whole. A uniform write on the next frame -- what a
    /// playback calls once per frame (`overlay::filter::Playback`), or what a
    /// caller driving the map's scene clock (`render::altitude::scene_time_at`)
    /// passes on.
    pub fn set_trip_clock(&mut self, id: &str, clock: Option<f64>, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.trip_layers.get_mut(id) {
            if l.clock != clock {
                l.clock = clock;
                self.trip_layers.clock_writes += 1;
                cx.notify();
            }
        }
    }

    /// The clock [`Self::set_trip_clock`] set.
    #[must_use]
    pub fn trip_clock(&self, id: &str) -> Option<f64> {
        self.trip_layers.get(id).and_then(|l| l.clock)
    }

    /// Clock changes accepted since the view was built (across layers).
    #[must_use]
    pub fn trip_clock_writes(&self) -> u64 {
        self.trip_layers.clock_writes
    }

    /// Layer `id`'s set and style as drawn now, or `None` when no such layer
    /// is set: what a legend reads, so it states what is on screen.
    #[must_use]
    pub fn trip_layer(&self, id: &str) -> Option<(Arc<TripSet>, TripStyle)> {
        self.trip_layers.get(id).map(|l| (l.set.clone(), l.style))
    }

    /// Layer `id`'s data stamp: moves on every `set_trip_layer`, never on a
    /// style or clock change.
    #[must_use]
    pub fn trip_layer_generation(&self, id: &str) -> Option<u64> {
        self.trip_layers.get(id).map(|l| l.generation)
    }

    /// The trip whose HEAD is under a point in the map's own
    /// (viewport-local) logical pixels, as `(layer id, index in its
    /// TripSet::trips)`: of every layer with a clock, each trip in the air
    /// at that clock ([`TripSet::position_at`]) projected as the shader
    /// places it ([`crate::render::trips::head_screen`]), the nearest
    /// within [`Self::TRIP_TOLERANCE_PX`] plus half the drawn width; a tie
    /// goes to the later layer (drawn on top). A layer with no clock draws
    /// its paths whole and has no heads: it is never hit, nor is a trail
    /// behind a head. O(trips) per call: a pick, never per frame.
    #[must_use]
    pub fn trip_at(&self, x: f32, y: f32) -> Option<(String, usize)> {
        if self.trip_layers.is_empty() {
            return None;
        }
        let camera = self.camera_control.camera();
        let globe = crate::render::globe::globe_frame(camera).with_globe_enabled(self.globe_enabled);
        let frame = crate::render::arcs::ArcFrame::new(camera, &globe, 1.0);
        let (x, y) = (f64::from(x), f64::from(y));
        let mut best: Option<(usize, usize, f64)> = None;
        for (k, l) in self.trip_layers.layers.iter().enumerate() {
            let Some(clock) = l.clock else { continue };
            let reach = Self::TRIP_TOLERANCE_PX + f64::from(l.style.width_px.max(1.0)) * 0.5;
            for i in 0..l.set.len() {
                let Some(head) = l.set.position_at(i, clock) else { continue };
                let Some((hx, hy)) = crate::render::trips::head_screen(&frame, head, l.style.altitude_scale) else { continue };
                let d = (hx - x).hypot(hy - y);
                if d <= reach && best.is_none_or(|b| d <= b.2) {
                    best = Some((k, i, d));
                }
            }
        }
        best.map(|(k, i, _)| (self.trip_layers.layers[k].id.clone(), i))
    }

    /// Trip `trip` of layer `id` (its index in `TripSet::trips`): `row` (the
    /// input row), `label` when it has one, and its head at the layer's
    /// clock as `lon`, `lat` and `altitude_m` (absent while it is not in
    /// the air or the layer has no clock).
    #[must_use]
    pub fn trip_properties(&self, id: &str, trip: usize) -> Option<Arc<crate::overlay::FeatureProperties>> {
        use crate::mvt::TagValue;
        let l = self.trip_layers.get(id)?;
        let r = l.set.trips.get(trip)?;
        let mut props = crate::overlay::FeatureProperties::new();
        props.insert("row".into(), TagValue::Number(f64::from(r.row)));
        if let Some(label) = &r.label {
            props.insert("label".into(), TagValue::String(label.clone()));
        }
        if let Some([lon, lat, alt]) = l.clock.and_then(|c| l.set.position_at(trip, c)) {
            props.insert("lon".into(), TagValue::Number(lon));
            props.insert("lat".into(), TagValue::Number(lat));
            props.insert("altitude_m".into(), TagValue::Number(alt));
        }
        Some(Arc::new(props))
    }

    /// What the last painted frame drew: layers, trips, instances, draws and
    /// bytes uploaded. Zero before the first frame with trips.
    #[must_use]
    pub fn trip_stats(&self) -> TripFrameStats {
        self.gpu.borrow().as_ref().and_then(|g| g.trip_pipeline.as_ref().map(|p| p.stats())).unwrap_or_default()
    }

    /// Vertex uploads since the trip pass was built: one per layer set, none
    /// per clock tick.
    #[must_use]
    pub fn trip_uploads_total(&self) -> u64 {
        self.gpu.borrow().as_ref().and_then(|g| g.trip_pipeline.as_ref().map(|p| p.uploads_total())).unwrap_or_default()
    }
}
