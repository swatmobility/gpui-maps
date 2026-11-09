//! 3D extrusion of overlay polygons: a height per feature (a property ×
//! an elevation scale, or any metres the caller computes), drawn by the
//! basemap's own `fill-extrusion` path.
//!
//! Nothing here is a second extruder. The features are cut into
//! synthetic Web Mercator tiles at one zoom ([`OverlayExtrusion::tile_zoom`])
//! as `PaintOp::Extrusion` ops in MVT units, and each tile goes through
//! [`crate::render::extrusion::tessellate_extrusions`] -- the roofs clipped
//! to the tile, every wall owned by exactly one tile, the same 12-byte
//! vertex and per-feature table. The GPU side
//! (`render::overlay_extrusion`) uploads those meshes with
//! `TilePipeline::create_extrusion_buffers` and draws them through the
//! same pipelines, lit by [`frame_params`] exactly as a style's
//! `fill-extrusion` layer is lit by `render::frame_plan::extrusion_plan`.
//!
//! # Semantics (MapLibre GL JS `fill-extrusion`, BSD-3, read not ported)
//!
//! `maplibre-gl-js` (github.com/maplibre/maplibre-gl-js): `src/data/bucket/fill_extrusion_bucket.ts`
//! (roof plus side quads per ring edge), `src/style/style_layer/
//! fill_extrusion_style_layer_properties.g.ts` (height and base in metres,
//! `fill-extrusion-vertical-gradient` default true, opacity a layer
//! property, not per feature), `src/render/draw_fill_extrusion.ts` (the
//! translucent depth prepass then colour pass). Here the same: `height_m`
//! and `base_m` in metres above the ground, one opacity per layer
//! ([`ExtrusionStyle::opacity`]), the colour per feature and opaque.
//!
//! Unlike a style layer, an overlay extrusion draws at EVERY zoom (a
//! basemap building is gated to z12+ because its data is): a Kepler hexbin
//! at z11 is still 3D -- and on the globe too, where the shader takes each
//! footprint vertex to the sphere and raises it along the normal. A
//! feature wider than one cell of the globe ground's grid (5.625 degrees,
//! cut at tile zooms below 6) has its roof and walls cut on that grid
//! (`render::extrusion::tessellate_extrusions_on_grid`), so a country's
//! roof bends with the planet instead of cutting a chord through it.
//!
//! No gpui, wgpu or DOM: the browser shell builds the same meshes.

use std::sync::Arc;

use super::geojson::OverlayGeometry;
use super::source_paint::FeatureProperties;
use crate::geo::TileId;
use crate::mvt::TagValue;
use crate::render::extrusion::{tessellate_extrusions_on_grid, ExtrusionLook, ExtrusionMesh, ExtrusionParams, EXTENT};
use crate::style::{ExtrusionLight, ExtrusionRow, PaintOp, PaintPlan, Rgba};

/// The deepest tile zoom features are cut at: MVT units are then about a
/// metre at 37°N, finer than any overlay polygon needs.
pub const MAX_TILE_ZOOM: u8 = 14;
/// The deepest zoom [`extrude_at`] cuts at when a caller asks for it: small
/// models (a car's wing, a wheel) need units of a few centimetres, which
/// z14's ~0.6 m units would square off. Only an explicit zoom goes past
/// [`MAX_TILE_ZOOM`]; [`extrude`] never does.
pub const MAX_CUT_ZOOM: u8 = 20;
/// MVT units the median feature spans, at least, at the chosen zoom: a
/// unit then stays under 1/256 of a feature, so rounding to the tile grid
/// never visibly bends an edge.
const MIN_FEATURE_UNITS: f64 = 256.0;

/// One feature to extrude.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtrudeFeature {
    /// Polygons, each a list of `(lon, lat)` rings: exterior first, then
    /// holes (the [`OverlayGeometry::Polygon`] shape). Any orientation.
    pub polygons: Vec<Vec<Vec<(f64, f64)>>>,
    /// Roof height, metres above the ground.
    pub height_m: f32,
    /// Wall foot, metres above the ground (usually 0).
    pub base_m: f32,
    /// The feature's colour (alpha ignored: opacity is the layer's).
    pub color: Rgba,
}

impl ExtrudeFeature {
    /// `geometry` as a feature, or `None` for anything but a polygon.
    #[must_use]
    pub fn from_geometry(geometry: &OverlayGeometry, height_m: f32, color: Rgba) -> Option<Self> {
        match geometry {
            OverlayGeometry::Polygon(p) => Some(Self { polygons: p.clone(), height_m, base_m: 0.0, color }),
            _ => None,
        }
    }
}

/// A feature's height from its properties: `property` (a number) ×
/// `elevation_scale`, metres; a missing or non-numeric value is 0.
#[must_use]
pub fn height_from_property(properties: &FeatureProperties, property: &str, elevation_scale: f64) -> f32 {
    match properties.get(property) {
        Some(TagValue::Number(v)) if v.is_finite() => (v * elevation_scale).max(0.0) as f32,
        _ => 0.0,
    }
}

/// Overlay polygons with properties (what [`super::hexbin::HexGrid::features`]
/// and every polygon source hand out) as extrusion features: the height
/// from `height` (metres, given the properties), the colour from `color`.
pub fn features_from_properties<'a>(
    features: impl IntoIterator<Item = (&'a OverlayGeometry, &'a FeatureProperties)>,
    height: impl Fn(&FeatureProperties) -> f32,
    color: impl Fn(&FeatureProperties) -> Rgba,
) -> Vec<ExtrudeFeature> {
    features
        .into_iter()
        .filter_map(|(g, p)| ExtrudeFeature::from_geometry(g, height(p), color(p)))
        .collect()
}

/// One synthetic tile's mesh.
#[derive(Debug, Clone)]
pub struct ExtrusionTile {
    pub tile: TileId,
    /// Shared: label occlusion reads its roofs' height grid every frame
    /// (`render::extrusion::BuildingOcclusion::with_overlay_occluders`).
    pub mesh: Arc<ExtrusionMesh>,
}

/// Overlay features tessellated for the extrusion pipeline.
#[derive(Debug, Clone, Default)]
pub struct OverlayExtrusion {
    /// The zoom the features were cut at.
    pub tile_zoom: u8,
    /// Non-empty tiles only.
    pub tiles: Vec<ExtrusionTile>,
    /// Features extruded (polygonal, with a finite position).
    pub features: usize,
    /// The tallest roof, metres.
    pub max_height_m: f32,
    /// `[min_lon, min_lat, max_lon, max_lat]` of every feature.
    pub bounds: [f64; 4],
    /// The features' footprints and heights for picking ([`pick`]):
    /// which column a screen point is on, by input index. Built by
    /// [`extrude`] / [`extrude_at`]; `Default` (and any set assembled
    /// field by field) holds an EMPTY index, so such a set draws but picks
    /// nothing -- build it with `extrude`, or with
    /// [`ExtrusionPickIndex::new`] from the same features.
    pub pick: Arc<ExtrusionPickIndex>,
    /// Built by [`extrude_with_facades`]: the tables carry each building's
    /// [`Facade`], which [`ExtrusionLook::facade`] draws.
    pub facades: bool,
}

/// One building's procedural facade ([`ExtrusionLook::facade`]): what its
/// walls are made of, how tall a floor is, and at night how many of its
/// windows are lit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Facade {
    pub material: FacadeMaterial,
    /// Metres per floor (clamped to 2..=9.9 when drawn).
    pub floor_m: f32,
    /// The share of its windows lit at night, 0..=1.
    pub lit_share: f32,
    /// A per-building seed, 0..1: the window pattern and roof tone.
    pub seed: f32,
}

/// What a [`Facade`]'s walls are: the window grid's bay and window size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacadeMaterial {
    /// A glass curtain wall: narrow mullions, a spandrel band per floor.
    Glass = 0,
    /// Limestone and pre-war masonry: punched windows, wide piers.
    Limestone = 1,
    /// Brick: punched windows, close bays.
    Brick = 2,
    /// Concrete: ribbon windows.
    Concrete = 3,
    /// Cast iron and loft fronts: tall, wide windows.
    CastIron = 4,
}

/// [`Facade::texel`]'s tag on the green channel: a colour's green is at
/// most 1, so a table without facades never reads as one. The shader's
/// `FACADE_TAG`.
pub const FACADE_TAG: f32 = 10.0;

impl Facade {
    /// The table texel the shader reads in `color_to`'s place:
    /// `(floor_m, FACADE_TAG + material, lit_share, seed)`.
    #[must_use]
    pub fn texel(&self) -> Rgba {
        Rgba { r: self.floor_m.clamp(2.0, 9.9), g: FACADE_TAG + self.material as u8 as f32, b: self.lit_share.clamp(0.0, 1.0), a: self.seed.clamp(0.0, 0.999) }
    }
}

/// Metres one vertex `pos` unit (an MVT unit) spans for a layer cut at
/// tile zoom `z` around latitude `lat_deg`: what the facade lays its grid
/// out in. One latitude per layer (its bounds' middle), so the grid does
/// not crawl as the camera pans.
#[must_use]
pub fn facade_metres_per_unit(z: u8, lat_deg: f64) -> f32 {
    let cos = lat_deg.to_radians().cos().max(1e-6);
    (EARTH_CIRCUMFERENCE_M * cos / f64::from(1u32 << z.min(24)) / f64::from(EXTENT)) as f32
}

impl OverlayExtrusion {
    /// [`facade_metres_per_unit`] for this set, or 0 for a set without
    /// facades (the shader's "off").
    #[must_use]
    pub fn facade_metres_per_unit(&self) -> f32 {
        if !self.facades || self.tiles.is_empty() {
            return 0.0;
        }
        facade_metres_per_unit(self.tile_zoom, (self.bounds[1] + self.bounds[3]) / 2.0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tiles.iter().all(|t| t.mesh.is_empty())
    }

    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.tiles.iter().map(|t| t.mesh.vertex_count()).sum()
    }

    #[must_use]
    pub fn index_count(&self) -> usize {
        self.tiles.iter().map(|t| t.mesh.index_count()).sum()
    }

    /// Bytes the GPU upload takes (vertices, indices, tables).
    #[must_use]
    pub fn upload_bytes(&self) -> usize {
        self.tiles.iter().map(|t| t.mesh.upload_bytes()).sum()
    }
}

fn bounds_of(features: &[ExtrudeFeature]) -> Option<[f64; 4]> {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for f in features {
        for &(x, y) in f.polygons.iter().flatten().flatten() {
            if x.is_finite() && y.is_finite() {
                b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
            }
        }
    }
    b[0].is_finite().then_some(b)
}

/// The tile zoom [`extrude`] cuts at: the shallowest at which the MEDIAN
/// feature spans [`MIN_FEATURE_UNITS`] MVT units, capped at
/// [`MAX_TILE_ZOOM`]. From the features' size, never the data's extent: one
/// stray row on the other side of the world (Kepler's SF trees has one at
/// 47°N, 138°W) must not coarsen every hexagon. Only occupied tiles are
/// built, so a deep zoom over a wide extent costs nothing extra.
#[must_use]
pub fn tile_zoom_for(features: &[ExtrudeFeature]) -> u8 {
    let mut spans: Vec<f64> = features
        .iter()
        .filter_map(|f| {
            let b = bounds_of(std::slice::from_ref(f))?;
            let (x0, y0) = crate::geo::lon_lat_to_tile_xy(b[0], b[3], 0.0);
            let (x1, y1) = crate::geo::lon_lat_to_tile_xy(b[2], b[1], 0.0);
            Some((x1 - x0).abs().max((y1 - y0).abs()))
        })
        .collect();
    if spans.is_empty() {
        return MAX_TILE_ZOOM;
    }
    let mid = spans.len() / 2;
    let span = *spans.select_nth_unstable_by(mid, f64::total_cmp).1;
    let z = (MIN_FEATURE_UNITS / (f64::from(EXTENT) * span.max(1e-12))).log2().ceil();
    z.clamp(0.0, f64::from(MAX_TILE_ZOOM)) as u8
}

/// Extrudes `features` at the zoom [`tile_zoom_for`] picks.
#[must_use]
pub fn extrude(features: &[ExtrudeFeature]) -> OverlayExtrusion {
    extrude_at(features, tile_zoom_for(features))
}

/// Extrudes `features` cut at tile zoom `z` (at most [`MAX_CUT_ZOOM`]).
#[must_use]
pub fn extrude_at(features: &[ExtrudeFeature], z: u8) -> OverlayExtrusion {
    let z = z.min(MAX_CUT_ZOOM);
    // The globe ground's grid at this zoom: a country-sized roof is cut so
    // the globe arm bends it with the planet (`tessellate_extrusions_on_grid`).
    extrude_on_grid(features, z, crate::render::subdivide::granularity_for_zoom(z), None)
}

/// [`extrude`], each feature with its [`Facade`] (`facades[i]` for
/// `features[i]`; a missing one is a plain concrete facade), for
/// [`ExtrusionLook::facade`]. The facade rides in the table's `color_to`
/// texel, which an overlay draw never blends toward.
#[must_use]
pub fn extrude_with_facades(features: &[ExtrudeFeature], facades: &[Facade]) -> OverlayExtrusion {
    let z = tile_zoom_for(features).min(MAX_CUT_ZOOM);
    extrude_on_grid(features, z, crate::render::subdivide::granularity_for_zoom(z), Some(facades))
}

/// [`extrude_at`] cut on a `granularity`-per-tile grid (1: none), so a
/// test can draw the uncut mesh beside the cut one.
#[must_use]
pub(crate) fn extrude_on_grid(features: &[ExtrudeFeature], z: u8, granularity: u32, facades: Option<&[Facade]>) -> OverlayExtrusion {
    let z = z.min(MAX_CUT_ZOOM);
    let Some(bounds) = bounds_of(features) else { return OverlayExtrusion { tile_zoom: z, ..Default::default() } };
    let zf = f64::from(z);
    let n = 1i64 << z;
    let mut per_tile: std::collections::BTreeMap<(u32, u32), Vec<PaintOp>> = std::collections::BTreeMap::new();
    let mut count = 0;
    let mut max_height_m = 0.0f32;
    let plain = Facade { material: FacadeMaterial::Concrete, floor_m: 3.6, lit_share: 0.3, seed: 0.5 };
    for (i, f) in features.iter().enumerate() {
        // Every vertex in fractional tile units at `z`.
        let projected: Vec<Vec<Vec<(f64, f64)>>> = f
            .polygons
            .iter()
            .map(|rings| {
                rings
                    .iter()
                    .map(|ring| ring.iter().filter(|p| p.0.is_finite() && p.1.is_finite()).map(|&(x, y)| crate::geo::lon_lat_to_tile_xy(x, y, zf)).collect())
                    .collect()
            })
            .collect();
        let mut fb = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for &(x, y) in projected.iter().flatten().flatten() {
            fb = [fb[0].min(x), fb[1].min(y), fb[2].max(x), fb[3].max(y)];
        }
        if !fb[0].is_finite() {
            continue;
        }
        count += 1;
        let height = f.height_m.max(f.base_m).max(0.0);
        max_height_m = max_height_m.max(height);
        let color = Rgba { a: 1.0, ..f.color };
        let color_to = facades.map_or(color, |fs| fs.get(i).unwrap_or(&plain).texel());
        let row = ExtrusionRow { color_from: color, color_to, base_from: f.base_m.max(0.0), height_from: height, base_to: f.base_m.max(0.0), height_to: height };
        let tx0 = (fb[0].floor() as i64).clamp(0, n - 1);
        let tx1 = (fb[2].floor() as i64).clamp(0, n - 1);
        let ty0 = (fb[1].floor() as i64).clamp(0, n - 1);
        let ty1 = (fb[3].floor() as i64).clamp(0, n - 1);
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let local = |(x, y): (f64, f64)| {
                    geo_types::Coord { x: ((x - tx as f64) * f64::from(EXTENT)).round() as i32, y: ((y - ty as f64) * f64::from(EXTENT)).round() as i32 }
                };
                let polygons: Vec<geo_types::Polygon<i32>> = projected
                    .iter()
                    .filter(|rings| rings.first().is_some_and(|r| r.len() >= 3))
                    .map(|rings| {
                        let mut rings = rings.iter().map(|r| {
                            let mut c: Vec<geo_types::Coord<i32>> = r.iter().copied().map(local).collect();
                            if c.first() != c.last() {
                                c.push(c[0]);
                            }
                            geo_types::LineString(c)
                        });
                        let exterior = rings.next().expect("filtered non-empty");
                        geo_types::Polygon::new(exterior, rings.collect())
                    })
                    .collect();
                if polygons.is_empty() {
                    continue;
                }
                per_tile.entry((tx as u32, ty as u32)).or_default().push(PaintOp::Extrusion {
                    group: 0,
                    row,
                    geometry: Arc::new(geo_types::Geometry::MultiPolygon(geo_types::MultiPolygon(polygons))),
                });
            }
        }
    }
    let tiles = per_tile
        .into_iter()
        .map(|((x, y), ops)| ExtrusionTile { tile: TileId { z, x, y }, mesh: Arc::new(tessellate_extrusions_on_grid(&PaintPlan { ops }, granularity)) })
        .filter(|t| !t.mesh.is_empty())
        .collect();
    let pick = Arc::new(ExtrusionPickIndex::new(features, z));
    OverlayExtrusion { tile_zoom: z, tiles, features: count, max_height_m, bounds, pick, facades: facades.is_some() }
}

/// How an overlay extrusion layer draws: the layer-wide half of
/// `fill-extrusion` (opacity, vertical gradient) plus the light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionStyle {
    /// `fill-extrusion-opacity`, `0..=1`. Below 1 it draws MapLibre's
    /// translucent two-pass way (nearest surface only, no stacking).
    pub opacity: f32,
    /// `fill-extrusion-vertical-gradient` (MapLibre's default: on).
    pub vertical_gradient: bool,
    /// The light; `None` takes the basemap style's own (its first
    /// `fill-extrusion` layer's), else the style spec's default, so an
    /// overlay is lit like the buildings beside it.
    pub light: Option<ExtrusionLight>,
    /// Shading beyond MapLibre's ([`ExtrusionLook::MAPLIBRE`] by default).
    pub look: ExtrusionLook,
    /// Hide the basemap's symbols this layer's solids stand in front of,
    /// through the buildings' own label occlusion
    /// (`render::extrusion::BuildingOcclusion`): a street name behind or
    /// under a column is not seen through it. On by default, whatever the
    /// map's `ExtrusionOverrides::label_occlusion` says for its buildings.
    pub occlude_labels: bool,
}

impl Default for ExtrusionStyle {
    fn default() -> Self {
        Self { opacity: 1.0, vertical_gradient: true, light: None, look: ExtrusionLook::MAPLIBRE, occlude_labels: true }
    }
}

/// The light a style's buildings are lit with at `zoom`: its first
/// `fill-extrusion` layer's, else the style spec's default root `light`.
#[must_use]
pub fn style_light(layers: &[crate::style::StyleLayer], zoom: f64) -> ExtrusionLight {
    layers
        .iter()
        .find_map(|l| match l {
            crate::style::StyleLayer::Extrusion { light, .. } => Some(light.at(zoom)),
            _ => None,
        })
        .unwrap_or_default()
}

/// Metres around the equator.
const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.686;

/// World units per metre at `center_lat_deg`: the height scale
/// `render::frame_plan::extrusion_plan` computes, for a frame with no
/// basemap building (whose plan then carries 0).
#[must_use]
pub fn meters_to_world(center_lat_deg: f64) -> f32 {
    let cos = center_lat_deg.to_radians().cos().max(1e-6);
    (f64::from(crate::render::WORLD_SIZE) / (EARTH_CIRCUMFERENCE_M * cos)) as f32
}

/// The occluders `set` adds to label occlusion when `style` asks for it:
/// every tile's mesh, shared.
#[must_use]
pub fn label_occluders(set: &OverlayExtrusion, style: &ExtrusionStyle) -> Vec<(TileId, Arc<ExtrusionMesh>)> {
    if !style.occlude_labels {
        return Vec::new();
    }
    set.tiles.iter().map(|t| (t.tile, Arc::clone(&t.mesh))).collect()
}

/// The layer's uniform for one frame: what `render::frame_plan::extrusion_plan`
/// writes for a style layer -- the light turned by the bearing, the
/// height scale at the camera's latitude -- with this layer's opacity and
/// look. `viewport_px` is the logical viewport.
#[must_use]
pub fn frame_params(style: &ExtrusionStyle, light: &ExtrusionLight, center_lat_deg: f64, bearing_rad: f64, viewport_px: [f32; 2]) -> ExtrusionParams {
    let meters_to_world = meters_to_world(center_lat_deg);
    let light = style.light.unwrap_or(*light);
    let mut params = ExtrusionParams {
        light_dir: crate::render::extrusion::light_direction(&light, bearing_rad),
        light_intensity: light.intensity,
        light_color: [light.color.r, light.color.g, light.color.b],
        vertical_gradient: if style.vertical_gradient { 1.0 } else { 0.0 },
        translate_px: [0.0; 2],
        opacity: style.opacity.clamp(0.0, 1.0),
        meters_to_world,
        ..ExtrusionParams::ZERO
    };
    style.look.apply(&mut params);
    params.viewport = [viewport_px[0], viewport_px[1], 0.0, 0.0];
    params
}

/// The stencil reference the translucent colour pass of overlay extrusion
/// layer `k` uses: counted down from 255, clear of the style's layers
/// (`render::extrusion::stencil_ref` counts up from 1).
#[must_use]
pub fn overlay_stencil_ref(k: usize) -> u8 {
    (255 - (k % 128)) as u8
}

// ---------------------------------------------------------------------
// Picking
// ---------------------------------------------------------------------

/// One extruded feature as picking reads it: its footprint in Web
/// Mercator `[0, 1]`, its heights, and its colour (what a highlight
/// brightens).
#[derive(Debug, Clone, PartialEq)]
pub struct PickFeature {
    /// The feature's index in the slice [`extrude`] was given.
    pub index: u32,
    /// Polygons of rings, exterior first, Mercator `[0, 1]`.
    pub polygons: Vec<Vec<Vec<glam::DVec2>>>,
    pub base_m: f32,
    /// The roof: `max(height, base)`, as drawn.
    pub top_m: f32,
    pub color: Rgba,
    /// `[min_x, min_y, max_x, max_y]`, Mercator.
    pub bbox: [f64; 4],
}

impl PickFeature {
    /// Whether Mercator point `m` is inside the footprint (even-odd over
    /// each polygon's rings, so a hole is outside).
    #[must_use]
    pub fn contains(&self, m: glam::DVec2) -> bool {
        if m.x < self.bbox[0] || m.x > self.bbox[2] || m.y < self.bbox[1] || m.y > self.bbox[3] {
            return false;
        }
        self.polygons.iter().any(|rings| {
            let mut inside = false;
            for ring in rings {
                let Some(&last) = ring.last() else { continue };
                let mut previous = last;
                for &q in ring {
                    if (q.y > m.y) != (previous.y > m.y) && m.x < (previous.x - q.x) * (m.y - q.y) / (previous.y - q.y) + q.x {
                        inside = !inside;
                    }
                    previous = q;
                }
            }
            inside
        })
    }

    /// The feature back in lon/lat, as [`extrude`] takes it.
    #[must_use]
    pub fn to_extrude_feature(&self) -> ExtrudeFeature {
        let polygons = self
            .polygons
            .iter()
            .map(|rings| rings.iter().map(|ring| ring.iter().map(|q| crate::geo::tile_xy_to_lon_lat(q.x, q.y, 0.0)).collect()).collect())
            .collect();
        ExtrudeFeature { polygons, height_m: self.top_m, base_m: self.base_m, color: self.color }
    }
}

/// The extruded features of one [`OverlayExtrusion`], gridded for
/// [`pick`]: a Web Mercator grid eight cells to a tile of the zoom the
/// features were cut at, each cell listing the features whose box touches
/// it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtrusionPickIndex {
    pub features: Vec<PickFeature>,
    level: u8,
    cells: std::collections::HashMap<u64, Vec<u32>>,
    /// The tallest roof, metres.
    pub max_top_m: f32,
}

/// The grid's cells per tile of the cut zoom (as a power of two).
const PICK_CELLS_PER_TILE_LOG2: u8 = 3;

impl ExtrusionPickIndex {
    /// Indexes `features` (cut at tile zoom `tile_zoom`) by input index.
    #[must_use]
    pub fn new(features: &[ExtrudeFeature], tile_zoom: u8) -> Self {
        let level = (tile_zoom + PICK_CELLS_PER_TILE_LOG2).min(24);
        let side = f64::from(1u32 << level);
        let mut out = Self { level, ..Self::default() };
        for (i, f) in features.iter().enumerate() {
            let polygons: Vec<Vec<Vec<glam::DVec2>>> = f
                .polygons
                .iter()
                .map(|rings| {
                    rings
                        .iter()
                        .map(|ring| {
                            ring.iter()
                                .filter(|p| p.0.is_finite() && p.1.is_finite())
                                .map(|&(x, y)| {
                                    let (x, y) = crate::geo::lon_lat_to_tile_xy(x, y, 0.0);
                                    glam::DVec2::new(x, y)
                                })
                                .collect::<Vec<_>>()
                        })
                        .filter(|ring: &Vec<glam::DVec2>| ring.len() >= 3)
                        .collect::<Vec<_>>()
                })
                .filter(|rings: &Vec<Vec<glam::DVec2>>| !rings.is_empty())
                .collect();
            let mut bbox = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
            for q in polygons.iter().flatten().flatten() {
                bbox = [bbox[0].min(q.x), bbox[1].min(q.y), bbox[2].max(q.x), bbox[3].max(q.y)];
            }
            if !bbox[0].is_finite() {
                continue;
            }
            let top_m = f.height_m.max(f.base_m).max(0.0);
            let k = out.features.len() as u32;
            let (x0, y0) = ((bbox[0] * side).floor() as i64, (bbox[1] * side).floor() as i64);
            let (x1, y1) = ((bbox[2] * side).floor() as i64, (bbox[3] * side).floor() as i64);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    out.cells.entry(Self::key(x, y)).or_default().push(k);
                }
            }
            out.max_top_m = out.max_top_m.max(top_m);
            out.features.push(PickFeature { index: i as u32, polygons, base_m: f.base_m.max(0.0), top_m, color: f.color, bbox });
        }
        out
    }

    fn key(x: i64, y: i64) -> u64 {
        ((y as u64) << 32) | (x as u64 & 0xffff_ffff)
    }

    /// The feature by its input index.
    #[must_use]
    pub fn feature(&self, index: usize) -> Option<&PickFeature> {
        self.features.iter().find(|f| f.index as usize == index)
    }

    /// The tallest solid containing Mercator point `m` at height `z_m`
    /// metres (`base <= z <= top`), as an index into [`Self::features`].
    #[must_use]
    pub fn solid_at(&self, m: glam::DVec2, z_m: f64) -> Option<usize> {
        let side = f64::from(1u32 << self.level);
        let cell = self.cells.get(&Self::key((m.x * side).floor() as i64, (m.y * side).floor() as i64))?;
        cell.iter()
            .map(|&k| k as usize)
            .filter(|&k| {
                let f = &self.features[k];
                f64::from(f.base_m) <= z_m && z_m <= f64::from(f.top_m) && f.contains(m)
            })
            .max_by(|&a, &b| self.features[a].top_m.total_cmp(&self.features[b].top_m))
    }

    /// One grid cell's side, Mercator.
    #[must_use]
    pub fn cell_size(&self) -> f64 {
        1.0 / f64::from(1u32 << self.level)
    }
}

/// Where a screen ray meets an overlay extrusion layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionHit {
    /// The feature's index in the slice [`extrude`] was given.
    pub feature: usize,
    /// On its roof (`true`) or a wall.
    pub roof: bool,
    /// The point hit, world units (`render::WORLD_SIZE` spans the world),
    /// z up: what orders hits across layers (nearer the eye is on top).
    pub point: glam::DVec3,
    /// Horizontal distance from the ray's ground point toward the eye,
    /// world units: larger is nearer the eye.
    pub depth: f64,
}

/// The visible column of `set` under logical screen point `(x, y)` of a
/// FLAT `camera`: the solid the screen ray enters first coming from the
/// eye, roof or wall, as the depth-tested draw shows it.
///
/// Marched with label occlusion's own march (`render::extrusion::march_ray`)
/// from where the ray meets the ground toward the eye, as far as the
/// tallest roof; the LAST sample inside a solid is the one nearest the
/// eye, refined to the solid's boundary by bisection. Inside a solid is
/// exact (the footprint polygons, the feature's base and roof at the
/// camera's metres-to-world scale, as `frame_params` draws them), so a
/// wall is hit where it stands, not where a height grid's cell rounds it.
/// Basemap buildings do not occlude the pick. `None` above the horizon.
#[must_use]
pub fn pick(set: &OverlayExtrusion, camera: &crate::render::Camera, x: f32, y: f32) -> Option<ExtrusionHit> {
    let index = &set.pick;
    if index.features.is_empty() {
        return None;
    }
    let world = f64::from(crate::render::WORLD_SIZE);
    let ground = crate::render::unproject_to_ground_f64(camera, x, y)?;
    let (eye, _, _) = camera.eye_target_up();
    // The ground point on the world copy nearest the eye.
    let mut a = ground.truncate();
    a.x -= world * ((a.x - eye.x) / world).round();
    let m2w = f64::from(meters_to_world(camera.center_lat));
    let to_merc = |p: glam::DVec2| glam::DVec2::new((p.x / world).rem_euclid(1.0), p.y / world);
    let to_eye = eye.truncate() - a;
    let span = to_eye.length();
    if !(span > 1e-12) {
        // Straight down: the tallest roof under the point.
        let m = to_merc(a);
        let f = index.features.iter().filter(|g| g.contains(m)).max_by(|x, y| x.top_m.total_cmp(&y.top_m))?;
        return Some(ExtrusionHit { feature: f.index as usize, roof: true, point: a.extend(f64::from(f.top_m) * m2w), depth: 0.0 });
    }
    let dir = to_eye / span;
    let rise = eye.z / span;
    let reach = (f64::from(index.max_top_m) * m2w / rise).min(span);
    let cell_world = index.cell_size() * world;
    // At most a quarter of a grid cell (an eighth of a tile at the cut
    // zoom), and at least 1,024 steps to the tallest roof: a ray clipping
    // a column's corner, or rising past a short one in front of a tall
    // one, is inside it for a sliver of the march. A sample is a hash
    // lookup and a few point-in-polygon tests, so the whole march stays
    // well under a millisecond.
    let step = (cell_world * 0.25).min(reach / 1024.0).max(1e-15);
    let inside = |p: glam::DVec2, z: f64| index.solid_at(to_merc(p), z / m2w);
    let mut last: Option<(f64, usize)> = None;
    crate::render::extrusion::march_ray(a, dir, 0.0, rise, 0.0, reach + step, step, |d, p, z| {
        if let Some(k) = inside(p, z) {
            last = Some((d, k));
        }
        false
    });
    let (d_in, k) = last?;
    // The boundary between the last inside sample and the next one out.
    let (mut lo, mut hi) = (d_in, d_in + step);
    for _ in 0..32 {
        let mid = 0.5 * (lo + hi);
        if inside(a + dir * mid, mid * rise).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let f = &index.features[inside(a + dir * lo, lo * rise).unwrap_or(k)];
    // Leaving through the roof: just past the boundary the point is still
    // over the footprint, only higher than the roof.
    let roof = f.contains(to_merc(a + dir * hi));
    Some(ExtrusionHit { feature: f.index as usize, roof, point: (a + dir * lo).extend(lo * rise), depth: lo })
}

/// [`pick`] on a globe frame: the visible column of `set` under logical
/// `(x, y)` of `frame`'s sphere, as the globe arm of the extrusion shader
/// draws it -- each footprint on the unit sphere, raised along the normal
/// by its metres over `GLOBE_RADIUS_M`.
///
/// The screen ray ([`crate::render::globe::sphere_ray`]) is marched from
/// where it enters the shell of the tallest roof to where it meets the
/// ground (or leaves the shell again, past the limb), eye first; the FIRST
/// sample inside a solid is the column nearest the eye, refined to its
/// boundary by bisection. A ray that meets the ground before the shell
/// (the planet in the way) finds nothing behind it. `depth` is minus the
/// distance along the ray, so a larger one is nearer the eye, as across
/// layers in [`pick`]; `point` is the hit's Mercator position in world
/// units with its height in metres as `z`.
#[must_use]
pub fn pick_on_globe(set: &OverlayExtrusion, frame: &crate::render::globe::GlobeFrame, camera: &crate::render::Camera, x: f32, y: f32) -> Option<ExtrusionHit> {
    use crate::render::globe::GLOBE_RADIUS_M;
    let index = &set.pick;
    if index.features.is_empty() {
        return None;
    }
    let (o, d) = crate::render::globe::sphere_ray(frame, camera, x, y)?;
    // Roots of |o + t d| = r, nearer first.
    let roots = |r: f64| {
        let b = o.dot(d);
        let c = o.length_squared() - r * r;
        let disc = b * b - c;
        (disc >= 0.0).then(|| (-b - disc.sqrt(), -b + disc.sqrt()))
    };
    let top = 1.0 + f64::from(index.max_top_m.max(0.0)) / GLOBE_RADIUS_M;
    let (t_in, t_far) = roots(top)?;
    let t_end = roots(1.0).map_or(t_far, |(ground, _)| ground);
    let t_in = t_in.max(0.0);
    if !(t_end > t_in) {
        return None;
    }
    let world = f64::from(crate::render::WORLD_SIZE);
    let solid = |t: f64| {
        let p = o + d * t;
        let height_m = (p.length() - 1.0) * GLOBE_RADIUS_M;
        let (mx, my) = crate::render::globe::sphere_point_to_mercator(p);
        index.solid_at(glam::DVec2::new(mx, my), height_m.max(0.0))
    };
    // A quarter of a grid cell on the sphere (`2 pi` per Mercator unit at
    // the equator; narrower toward the poles, which only refines it), and
    // at least 2,048 steps over the span.
    let cell = index.cell_size() * 2.0 * std::f64::consts::PI;
    let span = t_end - t_in;
    let step = (cell * 0.25).min(span / 2048.0).max(span / 65_536.0).max(1e-15);
    let mut t = t_in;
    let mut previous = t_in;
    let mut found = None;
    while t <= t_end + step {
        let at = t.min(t_end);
        if let Some(k) = solid(at) {
            found = Some((previous, at, k));
            break;
        }
        previous = at;
        if at >= t_end {
            break;
        }
        t += step;
    }
    let (mut lo, mut hi, k) = found?;
    for _ in 0..32 {
        let mid = 0.5 * (lo + hi);
        if solid(mid).is_some() {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let k = solid(hi).unwrap_or(k);
    let f = &index.features[k];
    let p = o + d * hi;
    let (mx, my) = crate::render::globe::sphere_point_to_mercator(p);
    let height_m = (p.length() - 1.0) * GLOBE_RADIUS_M;
    // Entered through the roof: just before the boundary the ray is still
    // over the footprint, only higher than the roof.
    let before = o + d * lo;
    let (bx, by) = crate::render::globe::sphere_point_to_mercator(before);
    let roof = f.contains(glam::DVec2::new(bx, by));
    Some(ExtrusionHit { feature: f.index as usize, roof, point: glam::DVec3::new(mx * world, my * world, height_m), depth: -hi })
}

/// [`ExtrusionHit`] as a one-feature extrusion: the column again, a touch
/// wider and taller (so it wins the depth test against itself without
/// fighting it), in `color` -- what a hover highlight draws over the
/// layer. `None` for an index the set does not hold.
#[must_use]
pub fn highlight_extrusion(set: &OverlayExtrusion, feature: usize, color: Rgba) -> Option<OverlayExtrusion> {
    let f = set.pick.feature(feature)?;
    let mut e = f.to_extrude_feature();
    // Grown about the box's centre by 1.5%, and 1.5% (at least 0.3 m)
    // taller: enclosing the original, so no face of it can show through.
    let (cx, cy) = (0.5 * (f.bbox[0] + f.bbox[2]), 0.5 * (f.bbox[1] + f.bbox[3]));
    let (clon, clat) = crate::geo::tile_xy_to_lon_lat(cx, cy, 0.0);
    for p in e.polygons.iter_mut().flatten().flatten() {
        *p = (clon + (p.0 - clon) * 1.015, clat + (p.1 - clat) * 1.015);
    }
    e.height_m = e.height_m * 1.015 + 0.3;
    e.color = color;
    Some(extrude_at(std::slice::from_ref(&e), set.tile_zoom))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(lon: f64, lat: f64, d: f64, h: f32) -> ExtrudeFeature {
        ExtrudeFeature {
            polygons: vec![vec![vec![(lon, lat), (lon + d, lat), (lon + d, lat + d), (lon, lat + d), (lon, lat)]]],
            height_m: h,
            base_m: 0.0,
            color: Rgba { r: 1.0, g: 0.0, b: 0.0, a: 0.5 },
        }
    }

    /// Where world point `p` (world units, z up) lands on `camera`'s
    /// screen, logical px: `unproject_to_ground_f64`'s inverse.
    fn screen(camera: &crate::render::Camera, p: glam::DVec3) -> (f32, f32) {
        let clip = camera.view_projection_f64() * p.extend(1.0);
        let (nx, ny) = (-clip.x / clip.w, clip.y / clip.w);
        (((nx + 1.0) * 0.5 * f64::from(camera.viewport_width_px)) as f32, ((1.0 - ny) * 0.5 * f64::from(camera.viewport_height_px)) as f32)
    }

    fn world_at(lon: f64, lat: f64, z_m: f64, camera: &crate::render::Camera) -> glam::DVec3 {
        let w = f64::from(crate::render::WORLD_SIZE);
        let (x, y) = crate::geo::lon_lat_to_tile_xy(lon, lat, 0.0);
        glam::DVec3::new(x * w, y * w, z_m * f64::from(meters_to_world(camera.center_lat)))
    }

    fn pick_camera(pitch_deg: f32) -> crate::render::Camera {
        crate::render::Camera {
            center_lon: -122.45,
            center_lat: 37.7605,
            zoom: 17.0,
            pitch: pitch_deg.to_radians(),
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    /// Two columns on a north-south line under a camera pitched 50 degrees
    /// and looking north (the eye to the south): a short one in front, a
    /// tall one five metres behind it. Each roof picks its own column as a
    /// roof; the tall one's south wall picks it as a wall where it stands
    /// above the short one, and the SHORT one where the short one hides
    /// it; the ground beside both picks nothing.
    #[test]
    fn the_roof_and_the_wall_nearest_the_eye_pick_at_pitch() {
        let d = 0.0005;
        let gap = 0.000045; // about 5 m
        let front = square(-122.45, 37.76, d, 20.0);
        let back = square(-122.45, 37.76 + d + gap, d, 200.0);
        let set = extrude(&[front, back]);
        let camera = crate::render::Camera { zoom: 16.0, center_lat: 37.762, ..pick_camera(50.0) };
        let at = |lon: f64, lat: f64, z: f64| screen(&camera, world_at(lon, lat, z, &camera));
        let mid = -122.45 + d / 2.0;
        let front_roof = at(mid, 37.76 + d / 2.0, 20.0);
        let hit = pick(&set, &camera, front_roof.0, front_roof.1).expect("the front roof");
        assert_eq!((hit.feature, hit.roof), (0, true));
        let back_roof = at(mid, 37.76 + d + gap + d / 2.0, 200.0);
        let hit = pick(&set, &camera, back_roof.0, back_roof.1).unwrap_or_else(|| panic!("the back roof at {back_roof:?}"));
        assert_eq!((hit.feature, hit.roof), (1, true));
        // The back column's south wall, high above the front roof.
        let back_wall_high = at(mid, 37.76 + d + gap - 1e-7, 150.0);
        let hit = pick(&set, &camera, back_wall_high.0, back_wall_high.1).expect("the back wall");
        assert_eq!((hit.feature, hit.roof), (1, false), "{hit:?}");
        // The same wall at 10 m is behind the 20 m front column.
        let back_wall_low = at(mid, 37.76 + d + gap - 1e-7, 10.0);
        let hit = pick(&set, &camera, back_wall_low.0, back_wall_low.1).expect("the front column, in front");
        assert_eq!(hit.feature, 0, "hidden by the front column: {hit:?}");
        // The front column's own south wall.
        let front_wall = at(mid, 37.76 + 1e-7, 10.0);
        let hit = pick(&set, &camera, front_wall.0, front_wall.1).expect("the front wall");
        assert_eq!((hit.feature, hit.roof), (0, false), "{hit:?}");
        // The ground well to the west of both.
        let beside = at(-122.45 - 2.0 * d, 37.76 + d / 2.0, 0.0);
        assert_eq!(pick(&set, &camera, beside.0, beside.1), None);
    }

    /// Straight down (pitch 0) a roof picks its column, and the ground
    /// between two columns picks nothing.
    #[test]
    fn a_roof_picks_from_straight_above() {
        let d = 0.0005;
        let set = extrude(&[square(-122.45, 37.76, d, 30.0), square(-122.45 + 2.0 * d, 37.76, d, 60.0)]);
        let camera = pick_camera(0.0);
        let at = |lon: f64, lat: f64, z: f64| screen(&camera, world_at(lon, lat, z, &camera));
        let a = at(-122.45 + d / 2.0, 37.76 + d / 2.0, 30.0);
        assert_eq!(pick(&set, &camera, a.0, a.1).map(|h| (h.feature, h.roof)), Some((0, true)));
        let b = at(-122.45 + 2.5 * d, 37.76 + d / 2.0, 60.0);
        assert_eq!(pick(&set, &camera, b.0, b.1).map(|h| (h.feature, h.roof)), Some((1, true)));
        let between = at(-122.45 + 1.5 * d, 37.76 + d / 2.0, 0.0);
        assert_eq!(pick(&set, &camera, between.0, between.1), None);
    }

    /// The highlight is the column again, enclosing it, in the colour asked.
    #[test]
    fn a_highlight_is_the_column_grown_and_recoloured() {
        let set = extrude(&[square(-122.45, 37.76, 0.0005, 30.0), square(-122.4, 37.76, 0.0005, 60.0)]);
        let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let h = highlight_extrusion(&set, 1, white).expect("feature 1");
        assert_eq!(h.features, 1);
        assert!(h.max_height_m > 60.0 && h.max_height_m < 62.0, "{}", h.max_height_m);
        assert!(h.bounds[0] < -122.4 && h.bounds[2] > -122.4 + 0.0005, "wider: {:?}", h.bounds);
        assert_eq!(h.tiles[0].mesh.chunks[0].table[0], [1.0, 1.0, 1.0, 1.0]);
        assert!(highlight_extrusion(&set, 2, white).is_none());
    }

    #[test]
    fn a_small_square_is_one_tile_with_roof_and_four_walls() {
        let e = extrude(&[square(-122.45, 37.76, 0.001, 40.0)]);
        assert_eq!(e.tile_zoom, MAX_TILE_ZOOM);
        assert_eq!(e.tiles.len(), 1);
        assert_eq!(e.features, 1);
        assert_eq!(e.max_height_m, 40.0);
        let mesh = &e.tiles[0].mesh;
        assert_eq!(mesh.wall_count(), 4);
        // Roof (2 triangles) + 4 walls (2 each).
        assert_eq!(mesh.index_count(), 3 * (2 + 8));
        // The table holds the height in metres and the colour opaque.
        let t = &mesh.chunks[0].table;
        assert_eq!(t[0], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(t[2], [0.0, 40.0, 0.0, 40.0]);
    }

    /// A 45 cm part is cut at z19 when asked (units of ~5 cm, where z14's
    /// are ~60 cm) and keeps its four walls; `extrude` alone still stops at
    /// z14, and nothing cuts deeper than `MAX_CUT_ZOOM`.
    #[test]
    fn an_explicit_deep_zoom_keeps_a_small_part() {
        let tiny = square(103.86, 1.29, 0.000_004, 0.5);
        let fine = extrude_at(std::slice::from_ref(&tiny), 19);
        assert_eq!(fine.tile_zoom, 19);
        assert_eq!(extrude_at(std::slice::from_ref(&tiny), 30).tile_zoom, MAX_CUT_ZOOM);
        assert_eq!(extrude(std::slice::from_ref(&tiny)).tile_zoom, MAX_TILE_ZOOM);
        let walls = |e: &OverlayExtrusion| e.tiles.iter().map(|t| t.mesh.wall_count()).sum::<usize>();
        assert_eq!(walls(&fine), 4, "the square's four walls at z19");
    }

    #[test]
    fn a_flat_feature_has_a_roof_and_no_walls() {
        let e = extrude(&[square(-122.45, 37.76, 0.001, 0.0)]);
        assert_eq!(e.tiles[0].mesh.wall_count(), 0);
        assert_eq!(e.tiles[0].mesh.index_count(), 6);
    }

    #[test]
    fn a_feature_across_a_tile_seam_draws_each_wall_once() {
        // Straddles the z14 tile seam through lon 0 (x = 8192 at z14).
        let f = square(-0.001, 10.0, 0.002, 10.0);
        let e = extrude_at(&[f], 14);
        assert_eq!(e.tiles.len(), 2, "two tiles");
        // Four walls in all: two whole (north, south) are split in two,
        // east and west each in one tile; the seam itself owns no wall.
        let walls: usize = e.tiles.iter().map(|t| t.mesh.wall_count()).sum();
        assert_eq!(walls, 6, "north and south halves + east + west, no seam wall");
    }

    #[test]
    fn the_zoom_follows_the_extent() {
        // A 100 m feature: as deep as allowed.
        assert_eq!(tile_zoom_for(&[square(-122.45, 37.76, 0.001, 1.0)]), MAX_TILE_ZOOM);
        // A census-tract-sized one (about 1 km; taller in Mercator at 41°N).
        assert_eq!(tile_zoom_for(&[square(-73.9, 40.7, 0.01, 1.0)]), 11);
        assert_eq!(tile_zoom_for(&[square(-180.0, -60.0, 300.0, 1.0)]), 0);
        // One stray feature far away does not coarsen the rest.
        let mut many: Vec<ExtrudeFeature> = (0..9).map(|k| square(-122.45 + f64::from(k) * 0.002, 37.76, 0.001, 1.0)).collect();
        many.push(square(-138.28, 47.27, 0.001, 1.0));
        assert_eq!(tile_zoom_for(&many), MAX_TILE_ZOOM);
        assert_eq!(tile_zoom_for(&[]), MAX_TILE_ZOOM);
    }

    #[test]
    fn heights_from_properties() {
        let mut p = FeatureProperties::new();
        p.insert("count".into(), TagValue::Number(12.0));
        assert_eq!(height_from_property(&p, "count", 5.0), 60.0);
        assert_eq!(height_from_property(&p, "none", 5.0), 0.0);
        let g = OverlayGeometry::Polygon(vec![vec![vec![(0.0, 0.0), (0.001, 0.0), (0.0, 0.001), (0.0, 0.0)]]]);
        let fs = features_from_properties([(&g, &p)], |p| height_from_property(p, "count", 2.0), |_| Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 });
        assert_eq!(fs[0].height_m, 24.0);
        assert!(ExtrudeFeature::from_geometry(&OverlayGeometry::Point(vec![(0.0, 0.0)]), 1.0, fs[0].color).is_none());
    }

    #[test]
    fn frame_params_match_a_style_layer_at_the_same_light() {
        let light = ExtrusionLight::default();
        let p = frame_params(&ExtrusionStyle { opacity: 0.8, ..Default::default() }, &light, 37.76, 0.3, [800.0, 600.0]);
        let plan_m2w = crate::render::WORLD_SIZE as f64 / (EARTH_CIRCUMFERENCE_M * 37.76f64.to_radians().cos());
        assert!((f64::from(p.meters_to_world) - plan_m2w).abs() < 1e-9);
        assert_eq!(p.light_dir, crate::render::extrusion::light_direction(&light, 0.3));
        assert_eq!((p.opacity, p.vertical_gradient, p.light_intensity), (0.8, 1.0, 0.5));
        assert_eq!(p.bands, [0.0; 4], "no haze or building cut");
        assert_ne!(overlay_stencil_ref(0), crate::render::extrusion::stencil_ref(0));
    }

    /// What cutting on the globe ground's grid costs on real polygon sets:
    /// every US county (`GPUI_MAP_US_COUNTIES_FGB`, default
    /// `~/.cache/gpui-map-data/fgb/UScounties.fgb`, about 3,200 features)
    /// and Natural Earth's admin-0 countries (`GPUI_MAP_WORLD_COUNTRIES_GEOJSON`,
    /// colon-separated GeoJSON paths), each at the zoom `extrude` picks and
    /// at z0, cut against uncut. A report, run by hand:
    /// `cargo test --release --lib extrusion_grid_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a cost report over a local dataset, not a check"]
    fn extrusion_grid_cost_on_counties_and_countries() {
        let feature = |g: &OverlayGeometry| ExtrudeFeature::from_geometry(g, 50_000.0, Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 });
        let mut sets: Vec<(&str, Vec<ExtrudeFeature>)> = Vec::new();
        let counties = std::env::var("GPUI_MAP_US_COUNTIES_FGB")
            .unwrap_or_else(|_| format!("{}/.cache/gpui-map-data/fgb/UScounties.fgb", std::env::var("HOME").unwrap_or_default()));
        match std::fs::read(&counties) {
            Ok(bytes) => {
                let source = crate::overlay::fgb::FgbSource::from_bytes(&bytes, &crate::overlay::fgb::FgbOptions { accept_any_crs: true, ..Default::default() }).expect("a FlatGeobuf");
                sets.push(("us-counties", (0..source.len()).filter_map(|i| source.geometry(i)).filter_map(|g| feature(&g)).collect()));
            }
            Err(e) => println!("us-counties: no file at {counties} ({e})"),
        }
        // Natural Earth admin-0 countries (public domain), as GeoJSON files
        // named by a colon-separated list: the 1:110m and 1:50m layers.
        for path in std::env::var("GPUI_MAP_WORLD_COUNTRIES_GEOJSON").unwrap_or_default().split(':').filter(|p| !p.is_empty()) {
            let json = std::fs::read_to_string(path).expect("a GeoJSON file");
            let parsed = crate::overlay::geojson::parse_feature_collection(&json).expect("a FeatureCollection");
            let name: &'static str = Box::leak(std::path::Path::new(path).file_stem().unwrap().to_string_lossy().into_owned().into_boxed_str());
            sets.push((name, parsed.iter().filter_map(|f| feature(&f.geometry)).collect()));
        }
        for (name, features) in &sets {
            let picked = tile_zoom_for(features);
            let mut zooms = vec![picked];
            if picked != 0 {
                zooms.push(0);
            }
            for z in zooms {
                let time = |granularity: u32| {
                    let mut best = f64::INFINITY;
                    let mut set = OverlayExtrusion::default();
                    for _ in 0..3 {
                        let started = crate::timing::Instant::now();
                        set = extrude_on_grid(features, z, granularity, None);
                        best = best.min(started.elapsed().as_secs_f64() * 1e3);
                    }
                    (set, best)
                };
                let (uncut, uncut_ms) = time(1);
                let (cut, cut_ms) = time(crate::render::subdivide::granularity_for_zoom(z));
                println!(
                    "{name}: {} features, z{z}{} (grid {}): vertices {} -> {} (x{:.2}), indices {} -> {}, upload {:.2} -> {:.2} MB, build {uncut_ms:.1} -> {cut_ms:.1} ms",
                    features.len(),
                    if z == picked { " (extrude's)" } else { "" },
                    crate::render::subdivide::granularity_for_zoom(z),
                    uncut.vertex_count(),
                    cut.vertex_count(),
                    cut.vertex_count() as f64 / uncut.vertex_count().max(1) as f64,
                    uncut.index_count(),
                    cut.index_count(),
                    uncut.upload_bytes() as f64 / 1e6,
                    cut.upload_bytes() as f64 / 1e6,
                );
            }
        }
    }

    #[test]
    fn hexbins_extrude() {
        let pts: Vec<[f64; 2]> = (0..200).map(|k| [-122.45 + f64::from(k % 20) * 0.0005, 37.76 + f64::from(k / 20) * 0.0005]).collect();
        let grid = super::super::hexbin::bin_points(&pts, None, &super::super::hexbin::HexbinOptions { radius_m: 50.0, ..Default::default() });
        let feats = grid.features(1.0);
        let fs = features_from_properties(feats.iter().map(|(g, p)| (g, p.as_ref())), |p| height_from_property(p, "count", 5.0), |_| Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
        let e = extrude(&fs);
        assert_eq!(e.features, grid.bins.len());
        assert!(e.vertex_count() > 0 && e.tiles.iter().all(|t| t.mesh.roof_failures == 0));
    }
}
