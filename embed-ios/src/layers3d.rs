//! Arcs, extruded polygons, animated trips and heatmaps: the engine's
//! `render::arcs`, `render::overlay_extrusion`, `render::trips` and
//! `render::heatmap`, drawn in the desktop's pass order
//! (`overlay::shell_layers`).
//!
//! ```text
//! try engine.setArcLayer(layer: "od", arcs: [...], style: ArcLayerStyle(...))
//! try engine.setExtrusionLayer(layer: "tracts", polygons: [...], style: ...)
//! try engine.setTripLayer(layer: "buses", trips: [...], style: ...)
//! engine.setTripTime(layer: "buses", t: 1_700_000_000_000)   // every tick
//! try engine.setHeatmapLayer(layer: "density", points: [...], style: ...)
//! ```
//!
//! A tap on an arc or a column reaches `on_tap` with the feature's id, as
//! for the flat layers. `clear(layer)` removes any of these layers. A Rust
//! host (Studio for iOS) can hand over the engine's own sets instead
//! ([`MapEngine::set_arc_set`] and its siblings) and skip the records.

use std::sync::Arc;

use gpui_maps_ios::overlay::extrude::{ExtrudeFeature, ExtrusionStyle, OverlayExtrusion};
use gpui_maps_ios::render::arcs::{ArcBlend, ArcSet, ArcStyle};
use gpui_maps_ios::render::heatmap::{HeatmapSet, HeatmapStyle};
use gpui_maps_ios::render::trips::{TripInput, TripSet, TripStyle};
use gpui_maps_ios::style::Rgba;

use crate::{LngLat, MapEngine, MapError};

/// How an arc layer blends over the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, uniffi::Enum)]
pub enum ArcBlendMode {
    #[default]
    Normal,
    /// Dense bundles sum toward white (Kepler's `additive`).
    Additive,
    /// Additive that saturates softly.
    Screen,
    /// Additive into a float target, tone-mapped: dense cores paler in the
    /// arcs' own hue.
    Glow,
}

/// One origin-destination arc.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ArcFeature {
    pub id: String,
    pub source: LngLat,
    pub target: LngLat,
    /// CSS colours (`#rrggbb`, `rgba(...)`); the target's defaults to the
    /// source's.
    pub source_color: String,
    #[uniffi(default = None)]
    pub target_color: Option<String>,
    /// Logical pixels.
    #[uniffi(default = 1.5)]
    pub width_px: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct ArcLayerStyle {
    #[uniffi(default = 1.0)]
    pub opacity: f32,
    /// deck.gl's `getHeight`: the peak is `height / 2` times the chord; 0
    /// draws flat lines.
    #[uniffi(default = 1.0)]
    pub height: f32,
    pub blend: ArcBlendMode,
}

/// One polygon raised to a height.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ExtrudedPolygon {
    pub id: String,
    /// Exterior first, then holes.
    pub rings: Vec<Vec<LngLat>>,
    pub height_m: f32,
    #[uniffi(default = 0.0)]
    pub base_m: f32,
    pub color: String,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct ExtrusionLayerStyle {
    #[uniffi(default = 1.0)]
    pub opacity: f32,
    #[uniffi(default = true)]
    pub vertical_gradient: bool,
}

/// A point of a trip: where, and when (any unit: the clock and the trail
/// are in the same one).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct TripStop {
    pub lon: f64,
    pub lat: f64,
    pub time: f64,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct TripFeature {
    pub id: String,
    /// In time order.
    pub points: Vec<TripStop>,
    pub color: String,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct TripLayerStyle {
    #[uniffi(default = 2.0)]
    pub width_px: f32,
    #[uniffi(default = 1.0)]
    pub opacity: f32,
    /// How far behind the head the trail reaches, in the time unit.
    pub trail: f64,
    #[uniffi(default = true)]
    pub fade: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct HeatPoint {
    pub lon: f64,
    pub lat: f64,
    #[uniffi(default = 1.0)]
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct HeatmapLayerStyle {
    /// Logical pixels.
    #[uniffi(default = 20.0)]
    pub radius_px: f32,
    #[uniffi(default = 1.0)]
    pub intensity: f32,
    #[uniffi(default = 0.18)]
    pub threshold: f32,
    #[uniffi(default = 0.8)]
    pub opacity: f32,
    /// Cold to hot; empty takes Kepler's "Global Warming".
    pub colors: Vec<String>,
}

/// What the last frame drew of these layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct LayerStats3d {
    pub arcs: u64,
    pub arc_vertices: u64,
    pub extrusion_layers: u64,
    pub extrusion_triangles: u64,
    pub trips: u64,
    pub trip_instances: u64,
    pub heatmap_points: u64,
    /// Every layer's data on the GPU, bytes.
    pub data_bytes: u64,
    /// The phone's budget for that data (the desktop's divided by 4).
    pub budget_bytes: u64,
}

fn color(layer: &str, css: &str) -> Result<Rgba, MapError> {
    gpui_maps_ios::style_spec::color::parse_css_color(css).map_err(|e| MapError::Paint(format!("{layer}: {css}: {e}")))
}

fn ids_of(ids: Vec<String>) -> Option<Arc<[String]>> {
    Some(Arc::from(ids))
}

/// The engine's arc style for a record.
pub fn arc_style(s: ArcLayerStyle) -> ArcStyle {
    let blend = match s.blend {
        ArcBlendMode::Normal => ArcBlend::Normal,
        ArcBlendMode::Additive => ArcBlend::Additive,
        ArcBlendMode::Screen => ArcBlend::Screen,
        ArcBlendMode::Glow => ArcBlend::Glow,
    };
    ArcStyle { opacity: s.opacity.clamp(0.0, 1.0), height: s.height.max(0.0), blend, ..ArcStyle::default() }
}

#[uniffi::export]
impl MapEngine {
    /// Draws `arcs` as layer `layer` (replacing it). Drawn over every flat
    /// layer and the 3D buildings, under the labels; flat on a flat map,
    /// great circles on the globe. A tap reports the arc's id.
    pub fn set_arc_layer(&self, layer: String, arcs: Vec<ArcFeature>, style: ArcLayerStyle) -> Result<(), MapError> {
        let mut source = Vec::with_capacity(arcs.len());
        let mut target = Vec::with_capacity(arcs.len());
        let mut colors = Vec::with_capacity(arcs.len());
        let mut widths = Vec::with_capacity(arcs.len());
        let mut ids = Vec::with_capacity(arcs.len());
        for a in arcs {
            let s = color(&layer, &a.source_color)?;
            let t = match a.target_color.as_deref() {
                Some(c) => color(&layer, c)?,
                None => s,
            };
            source.push([a.source.lon, a.source.lat]);
            target.push([a.target.lon, a.target.lat]);
            colors.push((s, t));
            widths.push(a.width_px.max(0.1));
            ids.push(a.id);
        }
        use gpui_maps_ios::overlay::arcs::{build, ArcColors, ArcWidths};
        let set = build(&source, &target, &ArcColors::PerArc(colors), &ArcWidths::PerArc(widths), None);
        self.set_arc_set(layer, Arc::new(set), arc_style(style), ids_of(ids));
        Ok(())
    }

    /// Arc layer `layer`'s opacity, height and blending: no re-upload.
    pub fn set_arc_layer_style(&self, layer: String, style: ArcLayerStyle) -> bool {
        self.lock().value.host.set_arc_style(&layer, arc_style(style))
    }

    /// Raises `polygons` as layer `layer` (replacing it): drawn with the
    /// basemap's buildings, sharing their depth, lit by the style's light.
    /// A tap reports the polygon's id.
    pub fn set_extrusion_layer(&self, layer: String, polygons: Vec<ExtrudedPolygon>, style: ExtrusionLayerStyle) -> Result<(), MapError> {
        let mut features = Vec::with_capacity(polygons.len());
        let mut ids = Vec::with_capacity(polygons.len());
        for p in polygons {
            let rings = p.rings.into_iter().map(|r| r.into_iter().map(|c| (c.lon, c.lat)).collect()).collect();
            features.push(ExtrudeFeature { polygons: vec![rings], height_m: p.height_m.max(0.0), base_m: p.base_m.max(0.0), color: color(&layer, &p.color)? });
            ids.push(p.id);
        }
        let set = gpui_maps_ios::overlay::extrude::extrude(&features);
        let style = ExtrusionStyle { opacity: style.opacity.clamp(0.0, 1.0), vertical_gradient: style.vertical_gradient, ..ExtrusionStyle::default() };
        self.set_extrusion_set(layer, Arc::new(set), style, ids_of(ids));
        Ok(())
    }

    /// Draws `trips` as an animated layer: each a fading trail behind its
    /// head at the time [`Self::set_trip_time`] sets (whole paths until then).
    pub fn set_trip_layer(&self, layer: String, trips: Vec<TripFeature>, style: TripLayerStyle) -> Result<(), MapError> {
        let mut inputs = Vec::with_capacity(trips.len());
        for (row, t) in trips.into_iter().enumerate() {
            let color = color(&layer, &t.color)?;
            let points = t.points.iter().map(|p| gpui_maps_ios::render::trips::TripPoint { lon: p.lon, lat: p.lat, alt_m: 0.0, time: p.time }).collect();
            inputs.push(TripInput { row: row as u32, points, color, label: Some(t.id) });
        }
        let style = TripStyle { width_px: style.width_px.max(0.5), opacity: style.opacity.clamp(0.0, 1.0), trail: style.trail.max(0.0), fade: style.fade, ..TripStyle::default() };
        self.set_trip_set(layer, Arc::new(TripSet::from_trips(&inputs)), style);
        Ok(())
    }

    /// Trip layer `layer`'s clock, in the trips' time unit; `None` draws
    /// the paths whole. A uniform write: call it every display-link tick.
    pub fn set_trip_time(&self, layer: String, t: Option<f64>) -> bool {
        self.lock().value.host.set_trip_clock(&layer, t)
    }

    /// The time span of trip layer `layer`'s trips: `[first, last]`.
    pub fn trip_time_range(&self, layer: String) -> Option<Vec<f64>> {
        let guard = self.lock();
        let (set, _) = guard.value.host.shell_layers().trip_layer(&layer)?;
        let spans = set.spans();
        let lo = spans.iter().map(|s| s[0]).fold(f64::INFINITY, f64::min);
        let hi = spans.iter().map(|s| s[1]).fold(f64::NEG_INFINITY, f64::max);
        (lo.is_finite() && hi >= lo).then(|| vec![lo, hi])
    }

    /// Draws `points` as a screen-space heatmap: density summed per pixel,
    /// coloured on a ramp, under the arcs and labels.
    pub fn set_heatmap_layer(&self, layer: String, points: Vec<HeatPoint>, style: HeatmapLayerStyle) -> Result<(), MapError> {
        let positions: Vec<[f64; 2]> = points.iter().map(|p| [p.lon, p.lat]).collect();
        let weights: Vec<f64> = points.iter().map(|p| p.weight).collect();
        let set = HeatmapSet::new(&positions, Some(&weights), &[]);
        let colors = if style.colors.is_empty() {
            HeatmapStyle::default().colors
        } else {
            style.colors.iter().map(|c| color(&layer, c)).collect::<Result<_, _>>()?
        };
        let style = HeatmapStyle { radius_px: style.radius_px.max(1.0), intensity: style.intensity.max(0.0), threshold: style.threshold.clamp(0.0, 1.0), opacity: style.opacity.clamp(0.0, 1.0), colors };
        self.set_heatmap_set(layer, Arc::new(set), style);
        Ok(())
    }

    /// What the last frame drew of the arc, extrusion, trip and heatmap
    /// layers, and the bytes they hold against the phone's budget.
    pub fn layer_stats_3d(&self) -> LayerStats3d {
        let s = self.lock().value.host.shell_layer_stats();
        LayerStats3d {
            arcs: s.arcs.arcs,
            arc_vertices: s.arcs.vertices,
            extrusion_layers: s.extrusion_layers,
            extrusion_triangles: s.extrusion_triangles,
            trips: s.trips.trips,
            trip_instances: s.trips.instances,
            heatmap_points: s.heatmaps.points,
            data_bytes: s.data_bytes,
            budget_bytes: gpui_maps_ios::overlay::shell_layers::SHELL_LAYER_BUDGET_BYTES,
        }
    }
}

/// The Rust host's door: the engine's own sets, built off the main thread
/// with `overlay::arcs`, `overlay::extrude`, `render::trips` or
/// `render::heatmap`. Main thread, like every engine call.
impl MapEngine {
    /// `ids` names each input row; `None` reports the row number.
    pub fn set_arc_set(&self, layer: String, set: Arc<ArcSet>, style: ArcStyle, ids: Option<Arc<[String]>>) {
        self.lock().value.host.set_arc_layer(&layer, set, style, ids);
    }

    pub fn set_arc_set_style(&self, layer: &str, style: ArcStyle) -> bool {
        self.lock().value.host.set_arc_style(layer, style)
    }

    /// `ids` names each feature in the order `extrude` took them.
    pub fn set_extrusion_set(&self, layer: String, set: Arc<OverlayExtrusion>, style: ExtrusionStyle, ids: Option<Arc<[String]>>) {
        self.lock().value.host.set_extrusion_layer(&layer, set, style, ids);
    }

    pub fn set_trip_set(&self, layer: String, set: Arc<TripSet>, style: TripStyle) {
        self.lock().value.host.set_trip_layer(&layer, set, style);
    }

    pub fn set_heatmap_set(&self, layer: String, set: Arc<HeatmapSet>, style: HeatmapStyle) {
        self.lock().value.host.set_heatmap_layer(&layer, set, style);
    }

    /// The layers' kinds and stats, for tests and budgets.
    pub fn shell_layer_stats(&self) -> gpui_maps_ios::overlay::shell_layers::ShellLayerStats {
        self.lock().value.host.shell_layer_stats()
    }

    /// Layer `layer`'s size (arcs, extruded features, trips, heat points).
    pub fn shell_set_len(&self, layer: &str) -> Option<usize> {
        self.lock().value.host.shell_layers().len_of(layer)
    }

    /// Which 3D layer kind `layer` is, if it is one.
    pub fn shell_layer_kind(&self, layer: &str) -> Option<gpui_maps_ios::overlay::shell_layers::ShellLayerKind> {
        self.lock().value.host.shell_layers().kind_of(layer)
    }

    /// Reads the next frame back (`width`, `height`, RGBA8): for tests on
    /// an offscreen `CAMetalLayer`. `None` for a headless engine.
    pub fn capture_frame(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.lock().value.host.capture_frame()
    }
}
