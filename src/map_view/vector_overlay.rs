//! `MapView`'s side of a vector-tile overlay (`overlay::vector_tiles`): the
//! per-viewport fetch, decode and cache of a second MVT source, and the
//! push of each of its style layers as an overlay source.
//!
//! - [`MapView::add_vector_overlay`] compiles the style layers and creates
//!   one overlay source per layer, named `"{overlay}/{layer id}"` (a
//!   circle layer with a stroke adds `"{overlay}/{layer id}/stroke"` under
//!   it). Circle layers are point sources, so they draw as instanced
//!   circles. Every source sits below the basemap's labels.
//! - Each frame (`vector_overlays_frame`, from `paint`) compares the tiles
//!   the camera wants at the source's zoom (the integer bucket, overzoomed
//!   past the source's `max_zoom`) against what is held; a difference
//!   schedules [`MapView::drive_vector_overlays`], which starts fetches (on
//!   the map's tokio runtime, decode on a blocking worker) and, once the
//!   wanted tiles have settled, pushes them. Tiles of the previous push
//!   stay drawn where the new ones have not landed yet.
//! - A push resolves every feature's paint at the camera's zoom bucket, so
//!   crossing a bucket re-pushes (re-resolving zoom expressions) without
//!   re-fetching.
//! - [`MapView::pick`] answers for these sources like any other: the
//!   feature's MVT properties. [`MapView::vector_overlay_layer`] maps a
//!   picked source name back to its overlay and layer.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::MapView;
use crate::geo::TileId;
use crate::overlay::vector_tiles::{self, TileFeatures, VectorGeometry, VectorLayer, VectorLayerKind};
use crate::overlay::{DataFeatureInput, FeatureInput, FeaturePaint, GeometryInput};
use crate::style::ZoomValue;
use crate::tile_source::TileSource;

/// How a vector overlay fetches.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorOverlayOptions {
    /// Below this camera zoom nothing is fetched or drawn.
    pub min_zoom: u8,
    /// The deepest tiles the source has; the camera past it overzooms them.
    /// `None`: the source's own `TileSource::max_zoom`.
    pub max_zoom: Option<u8>,
    /// The most tiles one view fetches (nearest the centre first).
    pub max_tiles: usize,
    /// Tile fetches in flight at once, per overlay.
    pub max_in_flight: usize,
    /// The overlay's credit line (on its first source).
    pub credit: Option<String>,
}

impl Default for VectorOverlayOptions {
    fn default() -> Self {
        Self { min_zoom: 0, max_zoom: None, max_tiles: 64, max_in_flight: 8, credit: None }
    }
}

/// What a vector overlay holds and has done, for a status line and the
/// perf gate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VectorOverlayStats {
    /// The tiles the camera wants now.
    pub tiles_wanted: usize,
    /// Decoded tiles held (wanted or kept from the last view).
    pub tiles_held: usize,
    pub tiles_in_flight: usize,
    /// Tiles the source reported absent (`Ok(None)`).
    pub tiles_absent: usize,
    pub tiles_failed: usize,
    /// Features in the last push, over every layer.
    pub features_pushed: usize,
    pub pushes: u64,
    /// Wall time of the last push on the UI thread.
    pub last_push_ms: f64,
    /// Sum of tile decode times (worker threads).
    pub decode_ms_total: f64,
    /// The source zoom of the last push.
    pub zoom: Option<u8>,
}

/// One overlay's state.
pub(super) struct VectorOverlay {
    name: String,
    source: Arc<dyn TileSource>,
    layers: Arc<Vec<VectorLayer>>,
    options: VectorOverlayOptions,
    max_zoom: u8,
    tiles: HashMap<TileId, Arc<TileFeatures>>,
    in_flight: HashSet<TileId>,
    absent: HashSet<TileId>,
    failed: HashMap<TileId, Instant>,
    wanted: Vec<TileId>,
    /// `(tiles, camera zoom bucket)` of the last push.
    pushed: Option<(Vec<TileId>, u8)>,
    last_push: Option<Instant>,
    stats: VectorOverlayStats,
}

#[derive(Default)]
pub(super) struct VectorOverlays {
    list: Vec<VectorOverlay>,
    /// A drive is already deferred.
    scheduled: bool,
}

/// A failed tile is asked again after this long.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// While tiles are still landing, a push waits this long after the last.
const PUSH_EVERY: Duration = Duration::from_millis(250);
/// The cover is grown by this fraction of the viewport on every side.
const VIEW_GROW: f64 = 0.15;

/// The overlay source a layer draws into.
pub fn layer_source_name(overlay: &str, layer: &str) -> String {
    format!("{overlay}/{layer}")
}

/// The source a circle layer's stroke draws into (under the fill).
pub fn stroke_source_name(overlay: &str, layer: &str) -> String {
    format!("{overlay}/{layer}/stroke")
}

impl MapView {
    /// Adds a second vector-tile source as an overlay, drawn with its own
    /// style layers -- MapLibre's `addSource({type: "vector"})` plus
    /// `addLayer` for each of `style_layers` (a JSON array of `fill`,
    /// `line` and `circle` layers whose `source-layer`s name layers of the
    /// tiles; their `source` key is not read).
    ///
    /// `source` is any [`TileSource`]: an XYZ MVT template
    /// (`tile_source::http::HttpTileSource`) or a vector PMTiles archive
    /// (`tile_source::pmtiles::PmtilesSource`), local or remote. Tiles are
    /// fetched for the viewport and cached; see the module doc. Replaces an
    /// overlay of the same `name`. Errors name the layer that could not be
    /// compiled or declared.
    pub fn add_vector_overlay(
        &mut self,
        name: &str,
        source: Arc<dyn TileSource>,
        style_layers: &serde_json::Value,
        options: VectorOverlayOptions,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        let layers = vector_tiles::compile_layers(style_layers)?;
        if layers.is_empty() {
            return Err("no drawable layer (fill, line or circle) in the style layers".into());
        }
        self.remove_vector_overlay(name, cx);
        for layer in &layers {
            for note in &layer.notes {
                eprintln!("vector overlay {name:?}: {note}");
            }
            if layer.kind == VectorLayerKind::Circle {
                let pitch = layer.circle_pitch();
                let mut paint = serde_json::Map::new();
                paint.insert("circle-pitch-alignment".into(), (if pitch.align_map { "map" } else { "viewport" }).into());
                paint.insert("circle-pitch-scale".into(), (if pitch.scale_map { "map" } else { "viewport" }).into());
                let mut sources = vec![layer_source_name(name, &layer.id)];
                if layer.may_stroke() {
                    sources.insert(0, stroke_source_name(name, &layer.id));
                }
                for source_name in sources {
                    self.set_source_cluster_options(&source_name, Some(crate::overlay::cluster::ClusterOptions::points_only()), cx)
                        .map_err(|e| format!("layer {:?}: {e}", layer.id))?;
                    self.declare_source_paint(&source_name, &paint).map_err(|e| format!("layer {:?}: {e}", layer.id))?;
                }
            }
        }
        if let Some(first) = layers.first() {
            let first_source = if first.kind == VectorLayerKind::Circle && first.may_stroke() {
                stroke_source_name(name, &first.id)
            } else {
                layer_source_name(name, &first.id)
            };
            // An empty push creates the source, so the credit has a home
            // before the first tile lands.
            let _ = self.set_source_data_from(&first_source, &[], cx);
            self.set_source_credit(&first_source, options.credit.as_deref());
        }
        let max_zoom = options.max_zoom.unwrap_or_else(|| source.max_zoom());
        self.vector_overlays.list.push(VectorOverlay {
            name: name.to_string(),
            source,
            layers: Arc::new(layers),
            options,
            max_zoom,
            tiles: HashMap::new(),
            in_flight: HashSet::new(),
            absent: HashSet::new(),
            failed: HashMap::new(),
            wanted: Vec::new(),
            pushed: None,
            last_push: None,
            stats: VectorOverlayStats::default(),
        });
        self.drive_vector_overlays(cx);
        Ok(())
    }

    /// Removes vector overlay `name` and every source it drew into. `true`
    /// if there was one.
    pub fn remove_vector_overlay(&mut self, name: &str, cx: &mut gpui::Context<Self>) -> bool {
        let Some(at) = self.vector_overlays.list.iter().position(|o| o.name == name) else { return false };
        let overlay = self.vector_overlays.list.remove(at);
        for source in Self::overlay_sources(&overlay) {
            self.remove_source(&source, cx);
        }
        true
    }

    /// Every overlay source vector overlay `name` draws into, bottom first.
    pub fn vector_overlay_sources(&self, name: &str) -> Vec<String> {
        self.vector_overlays.list.iter().find(|o| o.name == name).map(Self::overlay_sources).unwrap_or_default()
    }

    fn overlay_sources(overlay: &VectorOverlay) -> Vec<String> {
        let mut out = Vec::new();
        for layer in overlay.layers.iter() {
            if layer.kind == VectorLayerKind::Circle && layer.may_stroke() {
                out.push(stroke_source_name(&overlay.name, &layer.id));
            }
            out.push(layer_source_name(&overlay.name, &layer.id));
        }
        out
    }

    /// The `(overlay, layer id)` an overlay source name belongs to -- what a
    /// [`MapView::pick`] on a vector overlay's feature maps back through.
    pub fn vector_overlay_layer(&self, source: &str) -> Option<(String, String)> {
        self.vector_overlays.list.iter().find_map(|o| {
            o.layers.iter().find_map(|l| {
                (source == layer_source_name(&o.name, &l.id) || source == stroke_source_name(&o.name, &l.id))
                    .then(|| (o.name.clone(), l.id.clone()))
            })
        })
    }

    /// Where the cursor last hovered the map, in the map's own logical
    /// pixels (`None` off the map) -- the point a host passes to
    /// [`MapView::pick`] from its `FeatureEvent::HoverEnter` handler.
    #[must_use]
    pub fn hover_point(&self) -> Option<(f32, f32)> {
        self.hover_probe.as_ref().map(|p| p.at)
    }

    /// What vector overlay `name` holds and has done.
    pub fn vector_overlay_stats(&self, name: &str) -> Option<VectorOverlayStats> {
        self.vector_overlays.list.iter().find(|o| o.name == name).map(|o| {
            let mut stats = o.stats.clone();
            stats.tiles_wanted = o.wanted.len();
            stats.tiles_held = o.tiles.len();
            stats.tiles_in_flight = o.in_flight.len();
            stats.tiles_absent = o.absent.len();
            stats.tiles_failed = o.failed.len();
            stats
        })
    }

    /// The properties of every feature vector overlay `name` last pushed
    /// (the tiles on screen), with the id of the style layer it drew in --
    /// what a host's "features in view" chart or legend counts read. A
    /// feature that crosses a tile edge is in each tile it crosses, so
    /// counts are per tile piece, as MapLibre's `querySourceFeatures`.
    pub fn vector_overlay_features(&self, name: &str) -> Vec<(String, Arc<HashMap<String, crate::overlay::TagValue>>)> {
        let Some(o) = self.vector_overlays.list.iter().find(|o| o.name == name) else { return Vec::new() };
        let Some((pushed, _)) = &o.pushed else { return Vec::new() };
        let mut out = Vec::new();
        for tile in pushed {
            let Some(features) = o.tiles.get(tile) else { continue };
            for (layer, list) in o.layers.iter().zip(&features.layers) {
                out.extend(list.iter().map(|f| (layer.id.clone(), f.properties.clone())));
            }
        }
        out
    }

    /// Whether every tile vector overlay `name` wants has landed (or is
    /// absent) and been pushed.
    pub fn vector_overlay_settled(&self, name: &str) -> bool {
        self.vector_overlays.list.iter().find(|o| o.name == name).is_some_and(|o| {
            o.in_flight.is_empty()
                && o.pushed.as_ref().is_some_and(|(tiles, bucket)| {
                    *bucket == self.display_zoom() && o.wanted.iter().all(|t| tiles.contains(t) || o.absent.contains(t) || o.failed.contains_key(t))
                })
        })
    }

    /// The tiles overlay `at` wants for the current camera, and their zoom.
    fn vector_overlay_want(&self, at: usize) -> (Vec<TileId>, Option<u8>) {
        let overlay = &self.vector_overlays.list[at];
        let camera = self.camera_control.camera();
        let Some(z) = vector_tiles::source_zoom(camera.zoom, overlay.options.min_zoom, overlay.max_zoom) else {
            return (Vec::new(), None);
        };
        let (min, max) = crate::overlay::host::point_viewport_box(camera, VIEW_GROW);
        (vector_tiles::cover(min, max, z, overlay.options.max_tiles), Some(z))
    }

    /// Called from `paint`: schedules a drive when an overlay's wanted
    /// tiles or zoom bucket moved. A cover computation per overlay.
    pub(super) fn vector_overlays_frame(&mut self, cx: &mut gpui::App) {
        if self.vector_overlays.list.is_empty() || self.vector_overlays.scheduled {
            return;
        }
        let bucket = self.display_zoom();
        let needs = (0..self.vector_overlays.list.len()).any(|at| {
            let (want, _) = self.vector_overlay_want(at);
            let overlay = &self.vector_overlays.list[at];
            want != overlay.wanted || overlay.pushed.as_ref().is_some_and(|(_, b)| *b != bucket)
        });
        if !needs {
            return;
        }
        let Some(entity) = self.self_handle.clone() else { return };
        self.vector_overlays.scheduled = true;
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| {
                view.vector_overlays.scheduled = false;
                view.drive_vector_overlays(cx);
            });
        });
    }

    /// Starts the fetches every overlay needs and pushes what has landed.
    pub(crate) fn drive_vector_overlays(&mut self, cx: &mut gpui::Context<Self>) {
        let bucket = self.display_zoom();
        let zoom = self.camera_control.camera().zoom;
        for at in 0..self.vector_overlays.list.len() {
            let (want, _) = self.vector_overlay_want(at);
            let now = Instant::now();
            let starts: Vec<TileId> = {
                let overlay = &mut self.vector_overlays.list[at];
                overlay.wanted = want.clone();
                let room = overlay.options.max_in_flight.saturating_sub(overlay.in_flight.len());
                want.iter()
                    .filter(|t| {
                        !overlay.tiles.contains_key(t)
                            && !overlay.in_flight.contains(t)
                            && !overlay.absent.contains(t)
                            && overlay.failed.get(t).is_none_or(|at| now.duration_since(*at) >= RETRY_AFTER)
                    })
                    .take(room)
                    .copied()
                    .collect()
            };
            for tile in starts {
                self.spawn_vector_tile(at, tile, zoom, cx);
            }
            let overlay = &self.vector_overlays.list[at];
            let settled = overlay.wanted.iter().all(|t| !overlay.in_flight.contains(t));
            let loaded: Vec<TileId> = overlay.wanted.iter().filter(|t| overlay.tiles.contains_key(t)).copied().collect();
            let changed = overlay.pushed.as_ref().is_none_or(|(tiles, b)| *tiles != loaded || *b != bucket);
            let due = settled || overlay.last_push.is_none_or(|t| t.elapsed() >= PUSH_EVERY);
            if changed && due && (!loaded.is_empty() || settled) {
                self.push_vector_overlay(at, cx);
            }
        }
    }

    fn spawn_vector_tile(&mut self, at: usize, tile: TileId, zoom: f64, cx: &mut gpui::Context<Self>) {
        let Some(runtime) = self.runtime.clone() else { return };
        let overlay = &mut self.vector_overlays.list[at];
        overlay.in_flight.insert(tile);
        let (source, layers, name) = (Arc::clone(&overlay.source), Arc::clone(&overlay.layers), overlay.name.clone());
        let join = runtime.spawn(async move {
            let bytes = source.fetch(tile).await?;
            let Some(bytes) = bytes else { return Ok(None) };
            let decoded = tokio::task::spawn_blocking(move || {
                let started = Instant::now();
                let features = vector_tiles::tile_features(&layers, tile, bytes, zoom);
                features.map(|f| (f, started.elapsed().as_secs_f64() * 1000.0))
            })
            .await??;
            anyhow::Ok(Some(decoded))
        });
        cx.spawn(async move |entity, cx| {
            let result = join.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = entity.update(cx, |view, cx| view.land_vector_tile(&name, tile, result, cx));
        })
        .detach();
    }

    fn land_vector_tile(
        &mut self,
        name: &str,
        tile: TileId,
        result: anyhow::Result<Option<(TileFeatures, f64)>>,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(overlay) = self.vector_overlays.list.iter_mut().find(|o| o.name == name) else { return };
        overlay.in_flight.remove(&tile);
        match result {
            Ok(Some((features, ms))) => {
                overlay.stats.decode_ms_total += ms;
                overlay.tiles.insert(tile, Arc::new(features));
                overlay.failed.remove(&tile);
            }
            Ok(None) => {
                overlay.absent.insert(tile);
            }
            Err(e) => {
                eprintln!("vector overlay {name:?}: tile {}/{}/{}: {e:#}", tile.z, tile.x, tile.y);
                overlay.failed.insert(tile, Instant::now());
            }
        }
        // Keep the cache to the wanted tiles plus the last push's.
        let keep: HashSet<TileId> =
            overlay.wanted.iter().chain(overlay.pushed.iter().flat_map(|(t, _)| t.iter())).copied().collect();
        if overlay.tiles.len() > keep.len() + 32 {
            overlay.tiles.retain(|t, _| keep.contains(t));
        }
        self.drive_vector_overlays(cx);
        cx.notify();
    }

    /// Pushes overlay `at`'s landed tiles: every wanted tile that is held,
    /// plus the last push's tiles over any wanted tile still missing.
    fn push_vector_overlay(&mut self, at: usize, cx: &mut gpui::Context<Self>) {
        let started = Instant::now();
        let bucket = self.display_zoom();
        let zoom = f64::from(bucket);
        let (name, layers, draw, loaded) = {
            let overlay = &self.vector_overlays.list[at];
            let loaded: Vec<TileId> = overlay.wanted.iter().filter(|t| overlay.tiles.contains_key(t)).copied().collect();
            let missing: Vec<TileId> = overlay.wanted.iter().filter(|t| !overlay.tiles.contains_key(t)).copied().collect();
            let mut draw: Vec<Arc<TileFeatures>> = loaded.iter().filter_map(|t| overlay.tiles.get(t)).cloned().collect();
            if let Some((previous, _)) = &overlay.pushed {
                for t in previous {
                    if loaded.contains(t) {
                        continue;
                    }
                    if missing.iter().any(|m| tiles_overlap(*m, *t)) {
                        if let Some(f) = overlay.tiles.get(t) {
                            draw.push(Arc::clone(f));
                        }
                    }
                }
            }
            (overlay.name.clone(), Arc::clone(&overlay.layers), draw, loaded)
        };
        let mut pushed = 0usize;
        for (i, layer) in layers.iter().enumerate() {
            let visible = layer.visible_at(self.camera_control.camera().zoom);
            let features: Vec<&vector_tiles::VectorFeature> =
                if visible { draw.iter().flat_map(|t| t.layers.get(i).into_iter().flatten()).collect() } else { Vec::new() };
            pushed += features.len();
            match layer.kind {
                VectorLayerKind::Circle => {
                    let mut stroke = Vec::new();
                    let mut fill = Vec::new();
                    for f in &features {
                        let Some(paint) = layer.circle_paint(usize::from(f.case), zoom) else { continue };
                        let VectorGeometry::Points(points) = &f.geometry else { continue };
                        for p in points {
                            fill.push((*p, FeaturePaint { circle_radius: paint.radius, circle_color: paint.color, ..FeaturePaint::default() }, &f.properties));
                            if paint.stroke_width > 0.0 {
                                stroke.push((
                                    *p,
                                    FeaturePaint {
                                        circle_radius: paint.radius + paint.stroke_width,
                                        circle_color: paint.stroke_color,
                                        ..FeaturePaint::default()
                                    },
                                    &f.properties,
                                ));
                            }
                        }
                    }
                    if layer.may_stroke() {
                        self.push_points(&stroke_source_name(&name, &layer.id), &stroke, cx);
                    }
                    self.push_points(&layer_source_name(&name, &layer.id), &fill, cx);
                }
                VectorLayerKind::Fill | VectorLayerKind::Line => {
                    // Borrowed slices of slices for `GeometryInput`.
                    let mut runs: Vec<Vec<&[[f64; 2]]>> = Vec::with_capacity(features.len());
                    let mut paints: Vec<FeaturePaint> = Vec::with_capacity(features.len());
                    let mut kept: Vec<&vector_tiles::VectorFeature> = Vec::with_capacity(features.len());
                    let mut polygons: Vec<Vec<Vec<&[[f64; 2]]>>> = Vec::new();
                    for f in &features {
                        let case = usize::from(f.case);
                        match (&f.geometry, layer.kind) {
                            (VectorGeometry::Polygons(p), VectorLayerKind::Fill) => {
                                let Some(color) = layer.fill_paint(case, zoom) else { continue };
                                polygons.push(p.iter().map(|rings| rings.iter().map(Vec::as_slice).collect()).collect());
                                paints.push(FeaturePaint { fill_color: color, line_width: ZoomValue::constant(0.0), ..FeaturePaint::default() });
                                kept.push(f);
                            }
                            (VectorGeometry::Lines(l), VectorLayerKind::Line) => {
                                let Some(line) = layer.line_paint(case, zoom) else { continue };
                                runs.push(l.iter().map(Vec::as_slice).collect());
                                paints.push(FeaturePaint {
                                    line_color: line.color,
                                    line_width: line.width,
                                    line_dash: line.dash,
                                    ..FeaturePaint::default()
                                });
                                kept.push(f);
                            }
                            _ => {}
                        }
                    }
                    let polygon_refs: Vec<Vec<&[&[[f64; 2]]]>> = polygons.iter().map(|p| p.iter().map(Vec::as_slice).collect()).collect();
                    let inputs: Vec<DataFeatureInput<'_>> = kept
                        .iter()
                        .enumerate()
                        .map(|(k, f)| {
                            let geometry = match layer.kind {
                                VectorLayerKind::Fill => GeometryInput::Polygon(&polygon_refs[k]),
                                _ => GeometryInput::Line(&runs[k]),
                            };
                            DataFeatureInput {
                                feature: FeatureInput { id: None, geometry, paint: paints[k].clone() },
                                properties: Arc::clone(&f.properties),
                            }
                        })
                        .collect();
                    let source = layer_source_name(&name, &layer.id);
                    if let Err(e) = self.set_source_data_from(&source, &inputs, cx) {
                        eprintln!("vector overlay {name:?}: push of {source:?}: {e}");
                    }
                }
            }
        }
        let overlay = &mut self.vector_overlays.list[at];
        overlay.pushed = Some((loaded, bucket));
        overlay.last_push = Some(Instant::now());
        overlay.stats.pushes += 1;
        overlay.stats.features_pushed = pushed;
        overlay.stats.zoom = overlay.wanted.first().map(|t| t.z);
        overlay.stats.last_push_ms = started.elapsed().as_secs_f64() * 1000.0;
    }

    fn push_points(&mut self, source: &str, points: &[([f64; 2], FeaturePaint, &Arc<crate::overlay::FeatureProperties>)], cx: &mut gpui::Context<Self>) {
        let positions: Vec<[[f64; 2]; 1]> = points.iter().map(|(p, _, _)| [*p]).collect();
        let inputs: Vec<DataFeatureInput<'_>> = points
            .iter()
            .zip(&positions)
            .map(|((_, paint, properties), at)| DataFeatureInput {
                feature: FeatureInput { id: None, geometry: GeometryInput::Point(at), paint: paint.clone() },
                properties: Arc::clone(properties),
            })
            .collect();
        if let Err(e) = self.set_source_data_from(source, &inputs, cx) {
            eprintln!("vector overlay: push of {source:?}: {e}");
        }
    }
}

/// Whether two tiles (of any zooms) overlap.
fn tiles_overlap(a: TileId, b: TileId) -> bool {
    let (hi, lo) = if a.z >= b.z { (a, b) } else { (b, a) };
    let shift = hi.z - lo.z;
    (hi.x >> shift) == lo.x && (hi.y >> shift) == lo.y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_overlap_across_zooms() {
        let parent = TileId { z: 10, x: 163, y: 395 };
        assert!(tiles_overlap(parent, TileId { z: 12, x: 655, y: 1583 }));
        assert!(!tiles_overlap(parent, TileId { z: 12, x: 660, y: 1583 }));
        assert!(tiles_overlap(parent, parent));
    }

    #[test]
    fn source_names_are_per_layer() {
        assert_eq!(layer_source_name("fsq", "places"), "fsq/places");
        assert_eq!(stroke_source_name("fsq", "places"), "fsq/places/stroke");
    }
}
