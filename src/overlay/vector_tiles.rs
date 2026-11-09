//! A second vector-tile source drawn as an OVERLAY with its own style
//! layers (gap G7), and the `circle` layer type (G8) drawn through it.
//!
//! `MapView` resolves exactly one vector source, the basemap's. This module
//! is the interim the Kepler research names: the tiles in view of another
//! MVT source (an XYZ template or a vector PMTiles archive, anything that
//! is a [`crate::tile_source::TileSource`]) are fetched and decoded per
//! viewport, each style layer's features are clipped to their tile, and
//! the result is pushed as ordinary overlay sources -- one per style layer
//! -- so everything downstream is code every overlay already goes through:
//! the bake, the globe, hit-testing, [`crate::map_view::MapView::pick`]
//! (which returns the feature's own MVT properties) and the highlight.
//!
//! The style layers are compiled by the basemap's own compiler
//! (`style_spec::compile_layer_json`), so a `fill`, `line` or `circle` layer
//! here means what it means in a style document: its `filter`, its
//! `minzoom`/`maxzoom` and every zoom or data expression in its paint.
//! Each feature keeps the index of the data-driven case it matched, and is
//! resolved at the camera's zoom when its layer is pushed
//! ([`VectorLayer::fill_paint`], [`VectorLayer::line_paint`],
//! [`VectorLayer::circle_paint`]); a zoom-bucket change re-resolves.
//!
//! **Circles** draw through the instanced circle renderer
//! (`render::circles`), by way of the point path: a circle layer's source
//! is a point source (`ClusterOptions::points_only`), its per-feature
//! radius and colour resolved from the case, `circle-pitch-alignment` and
//! `circle-pitch-scale` declared on the source. `circle-stroke-*` is a
//! second point source UNDER the fill one, at `radius + stroke-width` in
//! the stroke colour: MapLibre's ring exactly where the fill is opaque.
//! `circle-blur` has no counterpart in the instanced shader and is
//! reported, not drawn (see `VectorLayer::notes`).
//!
//! **Clipping.** A tile's features carry a buffer past its edge. Polygons
//! are clipped to the tile square (Sutherland-Hodgman per ring), lines to
//! it (segment by segment), and a point outside it is dropped -- so
//! neighbouring tiles abut instead of overlapping, and a translucent fill
//! does not darken along the tile seams.
//!
//! This is not a source registry: the proper fix (a second vector source
//! in the basemap's own tile pipeline) is larger and is not attempted.

use std::collections::HashMap;
use std::sync::Arc;

/// The geometry crate [`encode_tile`] takes, re-exported so a host without
/// its own `geo-types` dependency can build tiles.
pub use geo_types;

use serde_json::Value;

use super::source_paint::FeatureProperties;
use crate::geo::TileId;
use crate::mvt::TagValue;
use crate::render::circles::CirclePitch;
use crate::style::{DashPattern, GeometryKind, ResolvedCircle, Rgba, StyleLayer, ZoomValue};

/// What a compiled overlay layer draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorLayerKind {
    Fill,
    Line,
    Circle,
}

/// One overlay style layer, compiled: its data-driven cases in order.
#[derive(Debug, Clone)]
pub struct VectorLayer {
    pub id: String,
    pub kind: VectorLayerKind,
    pub source_layer: String,
    cases: Vec<StyleLayer>,
    pub min_zoom: Option<u8>,
    pub max_zoom: Option<u8>,
    /// What the layer asked for that is not drawn (once per layer).
    pub notes: Vec<String>,
}

/// A feature's resolved line paint.
#[derive(Debug, Clone, PartialEq)]
pub struct LinePaint {
    pub color: Rgba,
    pub width: ZoomValue<f32>,
    pub dash: DashPattern,
}

impl VectorLayer {
    /// Compiles one style-layer JSON object. `Ok(None)` for a layer the
    /// compiler hid or skipped (its notice is in
    /// `style_spec::this_thread_compile_diagnostics`); `Err` for a layer
    /// type an overlay does not draw (`symbol`, `raster`, ...).
    pub fn compile(layer: &Value) -> Result<Option<Self>, String> {
        let id = layer.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let kind = match layer.get("type").and_then(Value::as_str) {
            Some("fill") => VectorLayerKind::Fill,
            Some("line") => VectorLayerKind::Line,
            Some("circle") => VectorLayerKind::Circle,
            other => return Err(format!("layer {id:?}: type {other:?} is not drawn by a vector overlay (fill, line or circle)")),
        };
        let cases = crate::style_spec::compile_layer_json(layer).map_err(|e| format!("layer {id:?}: {e:#}"))?;
        let Some(first) = cases.first() else { return Ok(None) };
        let (min_zoom, max_zoom) = first.zoom_range();
        let source_layer = match first {
            StyleLayer::Fill { source_layer, .. } | StyleLayer::Line { source_layer, .. } | StyleLayer::Circle { source_layer, .. } => {
                source_layer.clone()
            }
            _ => return Err(format!("layer {id:?}: compiled to an unexpected kind")),
        };
        let mut notes = Vec::new();
        if kind == VectorLayerKind::Circle {
            let blurred = cases.iter().any(|c| matches!(c, StyleLayer::Circle { paint, .. } if (0..=22).any(|z| paint.blur.value_at(z) > 0.0)));
            if blurred {
                notes.push(format!("layer {id:?}: circle-blur is not drawn by the instanced circle shader (edges stay crisp)"));
            }
        }
        if kind == VectorLayerKind::Fill && layer.get("paint").and_then(|p| p.get("fill-outline-color")).is_some() {
            notes.push(format!("layer {id:?}: fill-outline-color is not drawn on a vector overlay"));
        }
        Ok(Some(Self { id, kind, source_layer, cases, min_zoom, max_zoom, notes }))
    }

    /// Whether the layer draws at `zoom`: MapLibre's `minzoom <= z <
    /// maxzoom`.
    pub fn visible_at(&self, zoom: f64) -> bool {
        self.min_zoom.is_none_or(|m| zoom >= f64::from(m)) && self.max_zoom.is_none_or(|m| zoom < f64::from(m))
    }

    /// The case `tags` (of a feature of geometry `kind`) falls in, if any.
    pub fn case_for(&self, tags: &dyn crate::mvt::TagLookup, kind: Option<GeometryKind>, zoom: f64) -> Option<usize> {
        self.cases.iter().position(|c| {
            let filter = match c {
                StyleLayer::Fill { filter, .. } | StyleLayer::Line { filter, .. } | StyleLayer::Circle { filter, .. } => filter,
                _ => return false,
            };
            filter.matches_geometry(tags, zoom, kind)
        })
    }

    /// A fill case's colour at `zoom`, `fill-opacity` folded into alpha.
    pub fn fill_paint(&self, case: usize, zoom: f64) -> Option<Rgba> {
        match self.cases.get(case)? {
            StyleLayer::Fill { color, opacity, .. } => {
                let c = color.value_at_zoom(zoom);
                Some(Rgba { a: c.a * opacity.value_at_zoom(zoom).clamp(0.0, 1.0), ..c })
            }
            _ => None,
        }
    }

    /// A line case's paint at `zoom` (`line-opacity` folded into alpha;
    /// the width stays a curve, as the overlay blends it per frame).
    pub fn line_paint(&self, case: usize, zoom: f64) -> Option<LinePaint> {
        match self.cases.get(case)? {
            StyleLayer::Line { color, width, opacity, dash, .. } => {
                let c = color.value_at_zoom(zoom);
                Some(LinePaint {
                    color: Rgba { a: c.a * opacity.value_at_zoom(zoom).clamp(0.0, 1.0), ..c },
                    width: width.clone(),
                    dash: dash.value_at_zoom(zoom),
                })
            }
            _ => None,
        }
    }

    /// A circle case's paint at `zoom`.
    pub fn circle_paint(&self, case: usize, zoom: f64) -> Option<ResolvedCircle> {
        match self.cases.get(case)? {
            StyleLayer::Circle { paint, .. } => Some(paint.at(zoom)),
            _ => None,
        }
    }

    /// The layer's `circle-pitch-alignment` / `circle-pitch-scale`.
    pub fn circle_pitch(&self) -> CirclePitch {
        match self.cases.first() {
            Some(StyleLayer::Circle { paint, .. }) => paint.pitch,
            _ => CirclePitch::default(),
        }
    }

    /// Whether any case may draw a stroke at some zoom (a circle layer's
    /// stroke source is created only then).
    pub fn may_stroke(&self) -> bool {
        self.cases.iter().any(|c| matches!(c, StyleLayer::Circle { paint, .. } if (0..=22).any(|z| paint.stroke_width.value_at(z) > 0.0)))
    }
}

/// Compiles a JSON array of style layers. A hidden or skipped layer is
/// left out; a layer type the overlay cannot draw is an error naming it.
pub fn compile_layers(layers: &Value) -> Result<Vec<VectorLayer>, String> {
    let array = layers.as_array().ok_or("style layers must be a JSON array")?;
    let mut out = Vec::with_capacity(array.len());
    for layer in array {
        if let Some(compiled) = VectorLayer::compile(layer)? {
            out.push(compiled);
        }
    }
    Ok(out)
}

/// A clipped feature's geometry, in degrees.
#[derive(Debug, Clone, PartialEq)]
pub enum VectorGeometry {
    Points(Vec<[f64; 2]>),
    Lines(Vec<Vec<[f64; 2]>>),
    Polygons(Vec<Vec<Vec<[f64; 2]>>>),
}

/// One decoded, clipped feature of one overlay layer.
#[derive(Debug, Clone)]
pub struct VectorFeature {
    pub geometry: VectorGeometry,
    /// The layer case it matched ([`VectorLayer::case_for`]).
    pub case: u16,
    /// Its MVT properties, as `MapView::pick` returns them.
    pub properties: Arc<FeatureProperties>,
}

/// One tile's features, per overlay layer (index-aligned with the layers
/// the tile was decoded for).
#[derive(Debug, Clone)]
pub struct TileFeatures {
    pub tile: TileId,
    pub layers: Vec<Vec<VectorFeature>>,
}

impl TileFeatures {
    /// Features across every layer.
    pub fn len(&self) -> usize {
        self.layers.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Decodes `bytes` (one MVT tile, gzip or not) and keeps, per layer, the
/// features its filter selects at `zoom`, clipped to the tile, in degrees.
/// A layer whose zoom range excludes `zoom` gets nothing.
pub fn tile_features(layers: &[VectorLayer], tile: TileId, bytes: Vec<u8>, zoom: f64) -> anyhow::Result<TileFeatures> {
    let bytes = gunzip_if_needed(bytes)?;
    let decoded = crate::mvt::decode(bytes)?;
    let mut out = vec![Vec::new(); layers.len()];
    let extent = f64::from(crate::mvt::MVT_EXTENT);
    for (slot, layer) in out.iter_mut().zip(layers) {
        if !layer.visible_at(zoom) {
            continue;
        }
        let Some(tile_layer) = decoded.layers.iter().find(|l| l.name == layer.source_layer) else { continue };
        for feature in &tile_layer.features {
            let kind = GeometryKind::of(&feature.geometry);
            let wanted = match layer.kind {
                VectorLayerKind::Fill => kind == Some(GeometryKind::Polygon),
                VectorLayerKind::Line => matches!(kind, Some(GeometryKind::LineString | GeometryKind::Polygon)),
                VectorLayerKind::Circle => kind.is_some(),
            };
            if !wanted {
                continue;
            }
            let Some(case) = layer.case_for(&feature.tags, kind, zoom) else { continue };
            let Some(geometry) = clip_to_degrees(&feature.geometry, layer.kind, tile, extent) else { continue };
            slot.push(VectorFeature { geometry, case: case as u16, properties: Arc::new(feature.tags.to_map()) });
        }
    }
    Ok(TileFeatures { tile, layers: out })
}

/// MVT bytes served gzipped (tippecanoe's default on S3) are inflated;
/// anything else is returned as is.
fn gunzip_if_needed(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read as _;
        let mut out = Vec::with_capacity(bytes.len() * 4);
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(bytes)
    }
}

/// A tile-local point to degrees.
fn to_degrees(tile: TileId, extent: f64, x: f64, y: f64) -> [f64; 2] {
    let (lon, lat) = crate::geo::tile_xy_to_lon_lat(f64::from(tile.x) + x / extent, f64::from(tile.y) + y / extent, f64::from(tile.z));
    [lon, lat]
}

/// Clips `geometry` to the tile square and converts it to degrees, as the
/// layer kind draws it: rings for a fill, runs for a line (a polygon's
/// rings drawn as its outline), points for a circle (a line's or a
/// polygon's vertices are not circles: only point geometry draws one).
fn clip_to_degrees(geometry: &geo_types::Geometry<i32>, kind: VectorLayerKind, tile: TileId, extent: f64) -> Option<VectorGeometry> {
    use geo_types::Geometry as G;
    let deg = |p: [f64; 2]| to_degrees(tile, extent, p[0], p[1]);
    let ring_of = |ls: &geo_types::LineString<i32>| -> Vec<[f64; 2]> { ls.0.iter().map(|c| [f64::from(c.x), f64::from(c.y)]).collect() };
    match kind {
        VectorLayerKind::Circle => {
            let mut points = Vec::new();
            let mut push = |p: &geo_types::Point<i32>| {
                let (x, y) = (f64::from(p.x()), f64::from(p.y()));
                // Half-open, so a point on a shared edge is drawn by one
                // tile only.
                if (0.0..extent).contains(&x) && (0.0..extent).contains(&y) {
                    points.push(deg([x, y]));
                }
            };
            match geometry {
                G::Point(p) => push(p),
                G::MultiPoint(mp) => mp.iter().for_each(push),
                _ => {}
            }
            (!points.is_empty()).then_some(VectorGeometry::Points(points))
        }
        VectorLayerKind::Fill => {
            let polygons: Vec<&geo_types::Polygon<i32>> = match geometry {
                G::Polygon(p) => vec![p],
                G::MultiPolygon(mp) => mp.0.iter().collect(),
                _ => return None,
            };
            let mut out = Vec::new();
            for polygon in polygons {
                let outer = clip_ring(&ring_of(polygon.exterior()), extent);
                if outer.len() < 4 {
                    continue;
                }
                let mut rings = vec![outer.into_iter().map(deg).collect::<Vec<_>>()];
                for hole in polygon.interiors() {
                    let hole = clip_ring(&ring_of(hole), extent);
                    if hole.len() >= 4 {
                        rings.push(hole.into_iter().map(deg).collect());
                    }
                }
                out.push(rings);
            }
            (!out.is_empty()).then_some(VectorGeometry::Polygons(out))
        }
        VectorLayerKind::Line => {
            let runs: Vec<Vec<[f64; 2]>> = match geometry {
                G::LineString(ls) => vec![ring_of(ls)],
                G::MultiLineString(ml) => ml.0.iter().map(ring_of).collect(),
                G::Polygon(p) => std::iter::once(p.exterior()).chain(p.interiors()).map(ring_of).collect(),
                G::MultiPolygon(mp) => mp.0.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())).map(ring_of).collect(),
                _ => return None,
            };
            let mut out = Vec::new();
            for run in &runs {
                for part in clip_line(run, extent) {
                    if part.len() >= 2 {
                        out.push(part.into_iter().map(deg).collect());
                    }
                }
            }
            (!out.is_empty()).then_some(VectorGeometry::Lines(out))
        }
    }
}

/// Sutherland-Hodgman of one closed ring against `[0, extent]^2`. The
/// result is closed (first point repeated) when non-empty.
pub fn clip_ring(ring: &[[f64; 2]], extent: f64) -> Vec<[f64; 2]> {
    let inside_all = ring.iter().all(|p| (0.0..=extent).contains(&p[0]) && (0.0..=extent).contains(&p[1]));
    if inside_all {
        return ring.to_vec();
    }
    let mut points: Vec<[f64; 2]> = ring.to_vec();
    if points.first() == points.last() {
        points.pop();
    }
    // (axis, bound, keep-if-less-or-equal)
    for (axis, bound, below) in [(0usize, 0.0, false), (0, extent, true), (1, 0.0, false), (1, extent, true)] {
        if points.is_empty() {
            break;
        }
        let inside = |p: &[f64; 2]| if below { p[axis] <= bound } else { p[axis] >= bound };
        let mut next = Vec::with_capacity(points.len() + 4);
        for i in 0..points.len() {
            let current = points[i];
            let previous = points[(i + points.len() - 1) % points.len()];
            let (ci, pi) = (inside(&current), inside(&previous));
            if ci != pi {
                let t = (bound - previous[axis]) / (current[axis] - previous[axis]);
                let mut cut = [previous[0] + t * (current[0] - previous[0]), previous[1] + t * (current[1] - previous[1])];
                cut[axis] = bound;
                next.push(cut);
            }
            if ci {
                next.push(current);
            }
        }
        points = next;
    }
    if points.len() < 3 {
        return Vec::new();
    }
    points.push(points[0]);
    points
}

/// Clips a polyline to `[0, extent]^2` (Liang-Barsky per segment), split
/// into the runs that stay inside.
pub fn clip_line(run: &[[f64; 2]], extent: f64) -> Vec<Vec<[f64; 2]>> {
    let mut out: Vec<Vec<[f64; 2]>> = Vec::new();
    let mut current: Vec<[f64; 2]> = Vec::new();
    for w in run.windows(2) {
        let Some((a, b)) = clip_segment(w[0], w[1], extent) else {
            if current.len() >= 2 {
                out.push(std::mem::take(&mut current));
            }
            current.clear();
            continue;
        };
        if current.last() != Some(&a) {
            if current.len() >= 2 {
                out.push(std::mem::take(&mut current));
            }
            current.clear();
            current.push(a);
        }
        current.push(b);
        if b != w[1] {
            // Left the square: this run ends here.
            if current.len() >= 2 {
                out.push(std::mem::take(&mut current));
            }
            current.clear();
        }
    }
    if current.len() >= 2 {
        out.push(current);
    }
    out
}

fn clip_segment(a: [f64; 2], b: [f64; 2], extent: f64) -> Option<([f64; 2], [f64; 2])> {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, a[0]), (dx, extent - a[0]), (-dy, a[1]), (dy, extent - a[1])] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
        }
    }
    if t0 > t1 {
        return None;
    }
    let at = |t: f64| if t == 0.0 { a } else if t == 1.0 { b } else { [a[0] + t * dx, a[1] + t * dy] };
    Some((at(t0), at(t1)))
}

/// The tiles at zoom `z` a Mercator box (`[0, 1]` units, y down) reaches,
/// nearest its centre first, at most `cap`.
pub fn cover(min: [f64; 2], max: [f64; 2], z: u8, cap: usize) -> Vec<TileId> {
    let n = f64::from(1u32 << z);
    let clamp = |v: f64| v.clamp(0.0, n - 1.0) as u32;
    let (x0, x1) = (clamp((min[0] * n).floor()), clamp((max[0] * n).floor()));
    let (y0, y1) = (clamp((min[1] * n).floor()), clamp((max[1] * n).floor()));
    let (cx, cy) = ((min[0] + max[0]) / 2.0 * n, (min[1] + max[1]) / 2.0 * n);
    let mut tiles: Vec<TileId> = (y0..=y1).flat_map(|y| (x0..=x1).map(move |x| TileId { z, x, y })).collect();
    tiles.sort_by(|a, b| {
        let d = |t: &TileId| (f64::from(t.x) + 0.5 - cx).powi(2) + (f64::from(t.y) + 0.5 - cy).powi(2);
        d(a).total_cmp(&d(b))
    });
    tiles.truncate(cap);
    tiles
}

/// The zoom a source's tiles are fetched at for a camera at `zoom`: its
/// integer bucket, held at the source's `max_zoom` (overzoomed past it),
/// and `None` below its `min_zoom` (nothing is drawn there, as in
/// MapLibre).
pub fn source_zoom(zoom: f64, min_zoom: u8, max_zoom: u8) -> Option<u8> {
    let z = zoom.floor().max(0.0) as u32;
    if z < u32::from(min_zoom) {
        return None;
    }
    Some(z.min(u32::from(max_zoom)) as u8)
}

/// Encodes a Mapbox Vector Tile (spec 2.1) -- the inverse of `mvt::decode`,
/// for tests, the perf gate and a host building its own archive. Each
/// layer is `(name, features)`; a feature's geometry is in tile units
/// (`0..extent`), its properties written as MVT values.
#[doc(hidden)]
pub fn encode_tile(layers: &[(&str, Vec<(geo_types::Geometry<i32>, Vec<(String, TagValue)>)>)], extent: u32) -> Vec<u8> {
    let mut tile = Vec::new();
    for (name, features) in layers {
        let mut layer = Vec::new();
        put_varint_field(&mut layer, 15, 2);
        put_bytes_field(&mut layer, 1, name.as_bytes());
        let mut keys: Vec<String> = Vec::new();
        let mut key_at: HashMap<String, u32> = HashMap::new();
        let mut values: Vec<Vec<u8>> = Vec::new();
        let mut value_at: HashMap<Vec<u8>, u32> = HashMap::new();
        for (geometry, props) in features {
            let mut tags = Vec::new();
            for (k, v) in props {
                let ki = *key_at.entry(k.clone()).or_insert_with(|| {
                    keys.push(k.clone());
                    (keys.len() - 1) as u32
                });
                let encoded = encode_value(v);
                let vi = *value_at.entry(encoded.clone()).or_insert_with(|| {
                    values.push(encoded);
                    (values.len() - 1) as u32
                });
                tags.extend([ki, vi]);
            }
            let (kind, commands) = encode_geometry(geometry);
            if commands.is_empty() {
                continue;
            }
            let mut feature = Vec::new();
            put_packed(&mut feature, 2, &tags);
            put_varint_field(&mut feature, 3, kind);
            put_packed(&mut feature, 4, &commands);
            put_bytes_field(&mut layer, 2, &feature);
        }
        for k in &keys {
            put_bytes_field(&mut layer, 3, k.as_bytes());
        }
        for v in &values {
            put_bytes_field(&mut layer, 4, v);
        }
        put_varint_field(&mut layer, 5, u64::from(extent));
        put_bytes_field(&mut tile, 3, &layer);
    }
    tile
}

/// Writes a PMTiles v3 archive of MVT tiles (gzip-compressed tiles, an
/// uncompressed root directory, no leaf directories) -- for a host turning
/// its own extract into a vector PMTiles the overlay streams from, and for
/// tests. `tiles` may come in any order; duplicates are an error. Refuses
/// an archive whose directory would need leaf directories (over 16 KiB).
pub fn write_pmtiles(mut tiles: Vec<(TileId, Vec<u8>)>, metadata: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
    use std::io::Write as _;
    use crate::overlay::dotgrid::pmtiles_dir::zxy_to_tile_id;
    anyhow::ensure!(!tiles.is_empty(), "no tiles to write");
    tiles.sort_by_key(|(t, _)| zxy_to_tile_id(t.z, t.x, t.y));
    let mut data = Vec::new();
    let mut entries: Vec<(u64, u64, u64)> = Vec::with_capacity(tiles.len()); // (id, offset, length)
    let (mut min_z, mut max_z) = (u8::MAX, 0u8);
    let (mut west, mut south, mut east, mut north) = (180.0f64, 90.0f64, -180.0f64, -90.0f64);
    for (tile, bytes) in &tiles {
        let id = zxy_to_tile_id(tile.z, tile.x, tile.y);
        anyhow::ensure!(entries.last().is_none_or(|e| e.0 != id), "tile {}/{}/{} given twice", tile.z, tile.x, tile.y);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(bytes)?;
        let gz = gz.finish()?;
        entries.push((id, data.len() as u64, gz.len() as u64));
        data.extend_from_slice(&gz);
        min_z = min_z.min(tile.z);
        max_z = max_z.max(tile.z);
        let b = crate::geo::tile_bounds_lonlat(*tile);
        (west, south, east, north) = (west.min(b.min_lon), south.min(b.min_lat), east.max(b.max_lon), north.max(b.max_lat));
    }
    // The directory: count, id deltas, run lengths, lengths, offsets
    // (`offset + 1`, or 0 for "right after the previous tile").
    let mut dir = Vec::new();
    put_varint(&mut dir, entries.len() as u64);
    let mut last = 0u64;
    for e in &entries {
        put_varint(&mut dir, e.0 - last);
        last = e.0;
    }
    for _ in &entries {
        put_varint(&mut dir, 1);
    }
    for e in &entries {
        put_varint(&mut dir, e.2);
    }
    for (i, e) in entries.iter().enumerate() {
        let follows = i > 0 && e.1 == entries[i - 1].1 + entries[i - 1].2;
        put_varint(&mut dir, if follows { 0 } else { e.1 + 1 });
    }
    anyhow::ensure!(dir.len() <= 16_384 - 127, "{} tiles need leaf directories, which this writer does not write", entries.len());
    let meta = serde_json::to_vec(metadata)?;
    let root_offset = 127u64;
    let metadata_offset = root_offset + dir.len() as u64;
    let data_offset = metadata_offset + meta.len() as u64;
    let mut out = Vec::with_capacity(data_offset as usize + data.len());
    out.extend_from_slice(b"PMTiles");
    out.push(3);
    for v in [root_offset, dir.len() as u64, metadata_offset, meta.len() as u64, data_offset, 0, data_offset, data.len() as u64] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    // addressed tiles, tile entries, tile contents
    for v in [entries.len() as u64, entries.len() as u64, entries.len() as u64] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    // clustered, internal compression none, tile compression gzip, MVT
    out.extend_from_slice(&[1, 1, 2, 1, min_z, max_z]);
    let e7 = |v: f64| ((v * 1e7).round() as i32).to_le_bytes();
    for v in [west, south, east, north] {
        out.extend_from_slice(&e7(v));
    }
    out.push(max_z);
    out.extend_from_slice(&e7((west + east) / 2.0));
    out.extend_from_slice(&e7((south + north) / 2.0));
    debug_assert_eq!(out.len(), 127);
    out.extend_from_slice(&dir);
    out.extend_from_slice(&meta);
    out.extend_from_slice(&data);
    Ok(out)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_varint_field(out: &mut Vec<u8>, field: u32, v: u64) {
    put_varint(out, u64::from(field << 3));
    put_varint(out, v);
}

fn put_bytes_field(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_varint(out, u64::from((field << 3) | 2));
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_packed(out: &mut Vec<u8>, field: u32, values: &[u32]) {
    let mut body = Vec::with_capacity(values.len() * 2);
    for &v in values {
        put_varint(&mut body, u64::from(v));
    }
    put_bytes_field(out, field, &body);
}

fn encode_value(v: &TagValue) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        TagValue::String(s) => put_bytes_field(&mut out, 1, s.as_bytes()),
        TagValue::Number(n) => {
            put_varint(&mut out, (3 << 3) | 1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        TagValue::Bool(b) => put_varint_field(&mut out, 7, u64::from(*b)),
    }
    out
}

fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

/// `(GeomType, commands)`.
fn encode_geometry(geometry: &geo_types::Geometry<i32>) -> (u64, Vec<u32>) {
    use geo_types::Geometry as G;
    let mut cmds = Vec::new();
    let mut cursor = (0i32, 0i32);
    let command = |id: u32, count: u32| (id & 7) | (count << 3);
    let run = |cmds: &mut Vec<u32>, cursor: &mut (i32, i32), points: &[(i32, i32)], close: bool| {
        if points.is_empty() {
            return;
        }
        cmds.push(command(1, 1));
        cmds.extend([zigzag(points[0].0 - cursor.0), zigzag(points[0].1 - cursor.1)]);
        *cursor = points[0];
        if points.len() > 1 {
            cmds.push(command(2, (points.len() - 1) as u32));
            for p in &points[1..] {
                cmds.extend([zigzag(p.0 - cursor.0), zigzag(p.1 - cursor.1)]);
                *cursor = *p;
            }
        }
        if close {
            cmds.push(command(7, 1));
        }
    };
    let ring = |ls: &geo_types::LineString<i32>, exterior: bool| -> Vec<(i32, i32)> {
        let mut pts: Vec<(i32, i32)> = ls.0.iter().map(|c| (c.x, c.y)).collect();
        if pts.len() > 1 && pts.first() == pts.last() {
            pts.pop();
        }
        // MVT: an exterior ring has positive area in tile (y-down) space.
        let area: i64 = (0..pts.len())
            .map(|i| {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                i64::from(a.0) * i64::from(b.1) - i64::from(b.0) * i64::from(a.1)
            })
            .sum();
        if (area > 0) != exterior {
            pts.reverse();
        }
        pts
    };
    match geometry {
        G::Point(p) => {
            run(&mut cmds, &mut cursor, &[(p.x(), p.y())], false);
            (1, cmds)
        }
        G::MultiPoint(mp) => {
            let pts: Vec<(i32, i32)> = mp.iter().map(|p| (p.x(), p.y())).collect();
            cmds.push(command(1, pts.len() as u32));
            for p in pts {
                cmds.extend([zigzag(p.0 - cursor.0), zigzag(p.1 - cursor.1)]);
                cursor = p;
            }
            (1, cmds)
        }
        G::LineString(ls) => {
            run(&mut cmds, &mut cursor, &ls.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), false);
            (2, cmds)
        }
        G::MultiLineString(ml) => {
            for ls in &ml.0 {
                run(&mut cmds, &mut cursor, &ls.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), false);
            }
            (2, cmds)
        }
        G::Polygon(p) => {
            run(&mut cmds, &mut cursor, &ring(p.exterior(), true), true);
            for hole in p.interiors() {
                run(&mut cmds, &mut cursor, &ring(hole, false), true);
            }
            (3, cmds)
        }
        G::MultiPolygon(mp) => {
            for p in &mp.0 {
                run(&mut cmds, &mut cursor, &ring(p.exterior(), true), true);
                for hole in p.interiors() {
                    run(&mut cmds, &mut cursor, &ring(hole, false), true);
                }
            }
            (3, cmds)
        }
        _ => (0, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn square(x0: i32, y0: i32, x1: i32, y1: i32) -> geo_types::Geometry<i32> {
        geo_types::Geometry::Polygon(geo_types::Polygon::new(
            geo_types::LineString::from(vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]),
            Vec::new(),
        ))
    }

    fn props(pairs: &[(&str, TagValue)]) -> Vec<(String, TagValue)> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
    }

    #[test]
    fn a_circle_layer_compiles_with_spec_defaults_and_data_driven_cases() {
        let layer = VectorLayer::compile(&json!({
            "id": "places", "type": "circle", "source": "x", "source-layer": "places",
            "paint": {
                "circle-radius": ["interpolate", ["linear"], ["zoom"], 10, 2, 14, 6],
                "circle-color": ["match", ["get", "postcode"], "94103", "#ff0000", "#0000ff"],
                "circle-opacity": 0.5,
                "circle-pitch-alignment": "map"
            }
        }))
        .unwrap()
        .unwrap();
        assert_eq!(layer.kind, VectorLayerKind::Circle);
        assert!(layer.circle_pitch().align_map && layer.circle_pitch().scale_map);
        let mut tags = HashMap::new();
        tags.insert("postcode".to_string(), TagValue::String("94103".into()));
        let red_case = layer.case_for(&tags, Some(GeometryKind::Point), 12.0).unwrap();
        let red = layer.circle_paint(red_case, 12.0).unwrap();
        assert!((red.radius - 4.0).abs() < 1e-4, "{}", red.radius);
        assert_eq!((red.color.r, red.color.b, red.color.a), (1.0, 0.0, 0.5));
        assert_eq!(red.stroke_width, 0.0);
        tags.insert("postcode".to_string(), TagValue::String("10001".into()));
        let blue = layer.circle_paint(layer.case_for(&tags, Some(GeometryKind::Point), 14.0).unwrap(), 14.0).unwrap();
        assert_eq!((blue.color.b, blue.radius), (1.0, 6.0));
    }

    #[test]
    fn a_symbol_layer_is_refused_by_name() {
        let err = VectorLayer::compile(&json!({"id": "l", "type": "symbol", "source-layer": "x"})).unwrap_err();
        assert!(err.contains("\"l\""), "{err}");
    }

    #[test]
    fn encoded_tiles_decode_and_split_into_clipped_layer_features() {
        let fill = VectorLayer::compile(&json!({
            "id": "pop", "type": "fill", "source-layer": "blocks",
            "paint": {"fill-color": ["step", ["get", "total_pop"], "#0198BD", 150, "#42C1BC"], "fill-opacity": 0.8}
        }))
        .unwrap()
        .unwrap();
        let dots = VectorLayer::compile(&json!({"id": "dots", "type": "circle", "source-layer": "places", "minzoom": 3})).unwrap().unwrap();
        let bytes = encode_tile(
            &[
                (
                    "blocks",
                    vec![
                        (square(100, 100, 200, 200), props(&[("total_pop", TagValue::Number(10.0))])),
                        // Overhangs the tile on the left: clipped to x >= 0.
                        (square(-50, 0, 50, 100), props(&[("total_pop", TagValue::Number(500.0))])),
                    ],
                ),
                (
                    "places",
                    vec![
                        (geo_types::Geometry::Point(geo_types::Point::new(10, 10)), props(&[("name", TagValue::String("a".into()))])),
                        // In the buffer: dropped.
                        (geo_types::Geometry::Point(geo_types::Point::new(-5, 10)), props(&[])),
                    ],
                ),
            ],
            4096,
        );
        let tile = TileId { z: 4, x: 2, y: 5 };
        let features = tile_features(&[fill.clone(), dots.clone()], tile, bytes.clone(), 4.0).unwrap();
        assert_eq!(features.layers[0].len(), 2);
        assert_eq!(features.layers[1].len(), 1);
        assert_eq!(features.layers[1][0].properties.get("name"), Some(&TagValue::String("a".into())));
        let low = fill.fill_paint(usize::from(features.layers[0][0].case), 4.0).unwrap();
        let high = fill.fill_paint(usize::from(features.layers[0][1].case), 4.0).unwrap();
        assert_ne!(low, high);
        assert!((low.a - 0.8).abs() < 1e-6);
        let VectorGeometry::Polygons(p) = &features.layers[0][1].geometry else { panic!() };
        let west = crate::geo::tile_bounds_lonlat(tile).min_lon;
        assert!(p[0][0].iter().all(|c| c[0] >= west - 1e-9), "{p:?}");
        // Below the circle layer's minzoom: nothing.
        let early = tile_features(&[fill, dots], tile, bytes, 2.0).unwrap();
        assert!(early.layers[1].is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_written_archive_reads_back_through_the_pmtiles_source() {
        use crate::tile_source::TileSource as _;
        let tiles: Vec<(TileId, Vec<u8>)> = (0..3u32)
            .map(|i| {
                let bytes = encode_tile(
                    &[("places", vec![(geo_types::Geometry::Point(geo_types::Point::new(100 + i as i32, 7)), props(&[("i", TagValue::Number(f64::from(i)))]))])],
                    4096,
                );
                (TileId { z: 12, x: 655 + i, y: 1583 }, bytes)
            })
            .collect();
        let archive = write_pmtiles(tiles, &json!({"vector_layers": [{"id": "places"}]})).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.pmtiles");
        std::fs::write(&path, archive).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let source = rt.block_on(crate::tile_source::pmtiles::PmtilesSource::open(&path)).unwrap();
        assert_eq!(source.max_zoom(), 12);
        let bytes = rt.block_on(source.fetch(TileId { z: 12, x: 656, y: 1583 })).unwrap().expect("the tile");
        let dots = VectorLayer::compile(&json!({"id": "d", "type": "circle", "source-layer": "places"})).unwrap().unwrap();
        let f = tile_features(&[dots], TileId { z: 12, x: 656, y: 1583 }, bytes, 12.0).unwrap();
        assert_eq!(f.layers[0][0].properties.get("i"), Some(&TagValue::Number(1.0)));
        assert!(rt.block_on(source.fetch(TileId { z: 12, x: 700, y: 1583 })).unwrap().is_none());
    }

    #[test]
    fn rings_and_lines_clip_to_the_tile_square() {
        let ring = vec![[-10.0, -10.0], [20.0, -10.0], [20.0, 20.0], [-10.0, 20.0], [-10.0, -10.0]];
        let clipped = clip_ring(&ring, 10.0);
        assert!(clipped.iter().all(|p| (0.0..=10.0).contains(&p[0]) && (0.0..=10.0).contains(&p[1])));
        assert_eq!(clipped.first(), clipped.last());
        let parts = clip_line(&[[-5.0, 5.0], [5.0, 5.0], [15.0, 5.0], [15.0, 6.0], [5.0, 6.0]], 10.0);
        assert_eq!(parts, vec![vec![[0.0, 5.0], [5.0, 5.0], [10.0, 5.0]], vec![[10.0, 6.0], [5.0, 6.0]]]);
    }

    #[test]
    fn the_cover_is_nearest_first_and_the_source_zoom_overzooms() {
        let tiles = cover([0.49, 0.49], [0.51, 0.51], 4, 64);
        assert_eq!(tiles.len(), 4);
        assert_eq!(source_zoom(12.4, 7, 10), Some(10));
        assert_eq!(source_zoom(6.9, 7, 10), None);
        assert_eq!(source_zoom(8.2, 7, 10), Some(8));
    }
}
