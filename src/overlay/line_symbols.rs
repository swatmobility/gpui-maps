//! Symbols placed ALONG an overlay source's lines -- MapLibre's
//! `symbol-placement: line` for a caller's own data, declared once per
//! source (`OverlayStore::declare_source_symbols`) instead of pushed as one
//! rotated point per arrow.
//!
//! # The defect this removes
//!
//! A consumer drew route-direction chevrons by generating one rotated
//! `Point` feature every 200 m: 30 vehicles x 150 km of route is ~22,500
//! features, built on the UI thread at every push and walked by the
//! overlay symbol loop on every frame. The arrows are a function of the
//! LINE and the ZOOM, not data, and MapLibre treats them that way: a
//! symbol layer with `symbol-placement: line` over the same source, its
//! anchors laid out per tile per zoom on a worker (`getAnchors`).
//!
//! # What is computed where
//!
//! - [`LineSymbolLayout::compile`] runs at the declaration (UI thread, a
//!   handful of JSON keys). Nothing else happens on the UI thread at a
//!   push: the declaration rides in the source's snapshot.
//! - [`WorldLines::build`] projects the source's lines to world units ONCE
//!   per generation, and [`bake_anchors`] walks them once per integer
//!   zoom, both off the UI thread (`MapView::ensure_line_symbol_anchors`).
//!   The walk is `symbol_line::line_anchors_across` -- the basemap's own
//!   MapLibre `getAnchors` port -- with `symbol-spacing` converted from
//!   pixels to the walk's units at the bake zoom.
//! - Per frame, only the anchors in BUCKETS that meet the view are
//!   projected (`LineSymbolAnchors::visible`). An anchor carries the
//!   line's direction, so the icon's screen rotation is the projected
//!   direction -- right under bearing, pitch and the globe, as the
//!   basemap's line-placed icons are (`symbol_pass::symbol_angle_rad`).
//!
//! One walk over the WHOLE line per zoom, not one per tile: spacing is
//! continuous along a route instead of restarting at every tile edge, and a
//! tiled source (whose lines live in its tile index) and a whole-source one
//! take the same path. The cost is linear in the source's vertex count per
//! zoom, on a worker, and is independent of how many arrows a route carries
//! at the zoom the camera is NOT at.

use std::sync::Arc;

use geo_types::{Coord, LineString};
use serde_json::{Map, Value};

use crate::overlay::geojson::OverlayGeometry;
use crate::overlay::OverlayFeature;
use crate::style::LayoutValue;

/// The layout keys a declaration may carry. Anything else is refused,
/// naming the key: a layout property that silently did nothing is
/// indistinguishable from a working map.
pub const ACCEPTED_KEYS: &[&str] = &[
    "symbol-placement",
    "symbol-spacing",
    "icon-image",
    "icon-rotation-alignment",
    "icon-size",
    "icon-allow-overlap",
    "icon-ignore-placement",
    "minzoom",
    "maxzoom",
];

/// MapLibre's default `symbol-spacing`, in pixels.
pub const DEFAULT_SPACING_PX: f32 = 250.0;

/// The deepest zoom anchors are baked at. Deeper zooms draw this zoom's
/// anchors, spaced further apart on screen, rather than holding a list
/// that grows 4x per level for a view nobody can pan across.
pub const MAX_BAKE_ZOOM: u8 = 22;

/// A source's compiled line-symbol declaration. See this module's doc.
#[derive(Debug, Clone)]
pub struct LineSymbolLayout {
    /// The layout object as declared, for read-back.
    pub declared: Map<String, Value>,
    /// `symbol-spacing`, logical pixels: a constant or a zoom curve.
    pub spacing_px: LayoutValue,
    /// `icon-image`: a sprite id the map's sprite sheet (or
    /// `MapView::add_image`) carries.
    pub icon_image: String,
    /// `icon-size`: a constant or a zoom curve, the sprite's scale.
    pub icon_size: LayoutValue,
    /// `icon-allow-overlap` (with `icon-ignore-placement`): either one
    /// takes the arrows out of collision placement altogether -- drawn
    /// always, blocking nothing -- the overlay's own rule
    /// (`symbol_pass::overlay_bypasses_collision`).
    pub icon_allow_overlap: bool,
    pub icon_ignore_placement: bool,
    /// The layer's `minzoom`: drawn at `zoom >= min_zoom`.
    pub min_zoom: f32,
    /// The layer's `maxzoom`: drawn at `zoom < max_zoom`.
    pub max_zoom: f32,
}

fn refuse(reason: impl Into<String>) -> String {
    format!("line symbols: {}", reason.into())
}

/// A zoom-only number: a constant or a zoom `interpolate`/`step`, through
/// the style compiler's own `compile_layout_value`. A per-feature
/// expression is refused, since v1 resolves the layout once per source.
fn zoom_number(key: &str, value: &Value) -> Result<LayoutValue, String> {
    let compiled = crate::style_spec::compile_layout_value(value)
        .map_err(|error| refuse(format!("{key:?} {value}: {error:#}")))?;
    match compiled {
        LayoutValue::Constant(v) if v.is_finite() => Ok(compiled),
        LayoutValue::ZoomStep(_) => Ok(compiled),
        LayoutValue::Constant(_) => Err(refuse(format!("{key:?} must be finite"))),
        _ => Err(refuse(format!(
            "{key:?} {value} reads feature properties; only a number or a zoom expression is supported"
        ))),
    }
}

fn boolean(key: &str, value: &Value) -> Result<bool, String> {
    value.as_bool().ok_or_else(|| refuse(format!("{key:?} must be true or false, got {value}")))
}

fn zoom_bound(key: &str, value: &Value) -> Result<f32, String> {
    match value.as_f64() {
        Some(z) if (0.0..=24.0).contains(&z) => Ok(z as f32),
        _ => Err(refuse(format!("{key:?} must be a number in 0..=24, got {value}"))),
    }
}

impl LineSymbolLayout {
    /// Compiles a MapLibre symbol `layout` object restricted to
    /// [`ACCEPTED_KEYS`]. `Err` (a reason naming the key) for an unknown
    /// key, a `symbol-placement` other than `"line"` (absent is refused
    /// too: MapLibre's default is `"point"`, which is what a plain point
    /// feature already is), an `icon-image` that is not a plain string (a
    /// data-driven image is not in v1), and `icon-rotation-alignment:
    /// "viewport"`.
    pub fn compile(layout: &Map<String, Value>) -> Result<Self, String> {
        if let Some(key) = layout.keys().find(|key| !ACCEPTED_KEYS.contains(&key.as_str())) {
            return Err(refuse(format!(
                "unsupported layout key {key:?} (accepted: {})",
                ACCEPTED_KEYS.join(", ")
            )));
        }
        match layout.get("symbol-placement") {
            Some(Value::String(p)) if p == "line" => {}
            Some(other) => {
                return Err(refuse(format!(
                    "\"symbol-placement\" must be \"line\", got {other} (point symbols are point features)"
                )))
            }
            None => return Err(refuse("\"symbol-placement\": \"line\" is required")),
        }
        let icon_image = match layout.get("icon-image") {
            Some(Value::String(id)) if !id.is_empty() => id.clone(),
            Some(Value::String(_)) => return Err(refuse("\"icon-image\" is empty")),
            Some(other) => {
                return Err(refuse(format!(
                    "\"icon-image\" must be a sprite id string; expressions (data-driven images) are not supported, got {other}"
                )))
            }
            None => return Err(refuse("\"icon-image\" is required")),
        };
        match layout.get("icon-rotation-alignment") {
            None => {}
            Some(Value::String(a)) if a == "map" || a == "auto" => {}
            Some(Value::String(a)) if a == "viewport" => {
                return Err(refuse(
                    "\"icon-rotation-alignment\": \"viewport\" is not supported; a line symbol follows its line (\"map\" or \"auto\")",
                ))
            }
            Some(other) => {
                return Err(refuse(format!(
                    "\"icon-rotation-alignment\" must be \"map\" or \"auto\", got {other}"
                )))
            }
        }
        let spacing_px = match layout.get("symbol-spacing") {
            Some(v) => zoom_number("symbol-spacing", v)?,
            None => LayoutValue::Constant(DEFAULT_SPACING_PX),
        };
        if let LayoutValue::Constant(s) = spacing_px {
            if s < 1.0 {
                return Err(refuse(format!("\"symbol-spacing\" must be at least 1 px, got {s}")));
            }
        }
        let icon_size = match layout.get("icon-size") {
            Some(v) => zoom_number("icon-size", v)?,
            None => LayoutValue::Constant(1.0),
        };
        let icon_allow_overlap =
            layout.get("icon-allow-overlap").map(|v| boolean("icon-allow-overlap", v)).transpose()?.unwrap_or(false);
        let icon_ignore_placement = layout
            .get("icon-ignore-placement")
            .map(|v| boolean("icon-ignore-placement", v))
            .transpose()?
            .unwrap_or(false);
        let min_zoom = layout.get("minzoom").map(|v| zoom_bound("minzoom", v)).transpose()?.unwrap_or(0.0);
        let max_zoom = layout.get("maxzoom").map(|v| zoom_bound("maxzoom", v)).transpose()?.unwrap_or(24.0);
        if min_zoom >= max_zoom {
            return Err(refuse(format!("\"minzoom\" {min_zoom} must be below \"maxzoom\" {max_zoom}")));
        }
        Ok(Self {
            declared: layout.clone(),
            spacing_px,
            icon_image,
            icon_size,
            icon_allow_overlap,
            icon_ignore_placement,
            min_zoom,
            max_zoom,
        })
    }

    /// `symbol-spacing` at integer zoom `zoom`, logical pixels, never
    /// below 1.
    pub fn spacing_px_at(&self, zoom: u8) -> f64 {
        f64::from(self.spacing_px.resolve(&NoTags, zoom)).max(1.0)
    }

    /// `icon-size` at integer zoom `zoom`.
    pub fn icon_size_at(&self, zoom: u8) -> f32 {
        let size = self.icon_size.resolve(&NoTags, zoom);
        if size.is_finite() {
            size.max(0.0)
        } else {
            1.0
        }
    }

    /// Whether the declaration draws at camera zoom `zoom`.
    pub fn visible_at(&self, zoom: f64) -> bool {
        zoom >= f64::from(self.min_zoom) && zoom < f64::from(self.max_zoom)
    }

    /// Whether either collision flag takes the arrows out of placement.
    pub fn bypasses_collision(&self) -> bool {
        crate::symbol_pass::overlay_bypasses_collision(self.icon_allow_overlap, self.icon_ignore_placement)
    }
}

/// A tag lookup with nothing in it: the layout is zoom-only by
/// construction (`zoom_number`), so it never reads one.
#[derive(Debug)]
struct NoTags;

impl crate::mvt::TagLookup for NoTags {
    fn get(&self, _key: &str) -> Option<&crate::mvt::TagValue> {
        None
    }
}

/// A source's lines in world units (`render::WORLD_SIZE` across, y down),
/// one entry per LINE feature, each a list of parts. Built once per
/// source generation, then walked once per bake zoom.
#[derive(Debug, Default)]
pub struct WorldLines {
    pub features: Vec<Vec<Vec<(f64, f64)>>>,
    /// Total vertices, for the bench and the census.
    pub vertices: usize,
}

impl WorldLines {
    /// Every line feature of `features`, projected. A tiled source's
    /// `Indexed` lines are read back from its tile index. Polygons, points
    /// and empty geometry carry no line symbols. `None` when `cancelled`.
    pub fn build(
        features: &[OverlayFeature],
        tile_index: Option<&crate::overlay::tile_index::TileIndex>,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Self> {
        let mut out = Self::default();
        for (at, feature) in features.iter().enumerate() {
            if at % 256 == 0 && cancelled() {
                return None;
            }
            let restored;
            let parts = match &feature.geometry {
                OverlayGeometry::Line(parts) => parts,
                OverlayGeometry::Indexed { line: true, .. } => {
                    let Some(OverlayGeometry::Line(parts)) =
                        tile_index.and_then(|index| index.feature_geometry(at as u32))
                    else {
                        continue;
                    };
                    restored = parts;
                    &restored
                }
                _ => continue,
            };
            let world: Vec<Vec<(f64, f64)>> = parts
                .iter()
                .filter(|part| part.len() >= 2)
                .map(|part| part.iter().map(|&(lon, lat)| crate::overlay::geom::lon_lat_to_world(lon, lat)).collect())
                .collect();
            if world.is_empty() {
                continue;
            }
            out.vertices += world.iter().map(Vec::len).sum::<usize>();
            out.features.push(world);
        }
        Some(out)
    }
}

/// One placed arrow: where it sits and which way its line runs there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldAnchor {
    /// World units, y down.
    pub at: (f64, f64),
    /// The line's direction at `at`, a unit vector in world units (y
    /// DOWN): the segment the anchor was interpolated within, from its own
    /// raw endpoints (`symbol_line::LineAnchor::segment_start`), never from
    /// the rounded anchor.
    pub dir: (f32, f32),
    /// This anchor's place in its feature's sequence (0, 1, 2, ...). What
    /// thins a FORESHORTENED stretch of line under pitch: where the line
    /// is drawn at `1 / 2^d` of its flat length, only every `2^d`-th anchor
    /// draws (`keeps_at`), so the far side of a tilted view keeps roughly
    /// the declared spacing on screen instead of piling arrows up toward
    /// the horizon -- the effect MapLibre gets from laying far tiles out at
    /// their own, coarser zoom.
    pub seq: u32,
}

impl WorldAnchor {
    /// Whether this anchor draws where its line is shown at `scale` of its
    /// flat on-screen length (1 flat; 0.25 a line foreshortened 4x by
    /// pitch): every `2^round(log2(1 / scale))`-th anchor does, so the
    /// on-screen spacing stays within about 0.7x..1.4x of the declared one.
    pub fn keeps_at(&self, scale: f32) -> bool {
        // Unmeasurable (the step ahead failed to project) or not shrunk:
        // every anchor draws.
        if !scale.is_finite() || scale <= 0.0 || scale >= 1.0 {
            return true;
        }
        let level = (1.0 / scale).log2().round().clamp(0.0, 31.0) as u32;
        self.seq.is_multiple_of(1u32 << level)
    }

    /// The line's direction as a map angle: degrees counter-clockwise from
    /// EAST, north up -- a line running east is 0, north 90, west 180,
    /// south -90. The icon's on-screen rotation is this direction
    /// projected by the frame's camera (see `MapView`'s symbol pass); at
    /// bearing 0 and pitch 0 it is `-angle_deg()` clockwise.
    pub fn angle_deg(&self) -> f32 {
        (-self.dir.1).atan2(self.dir.0).to_degrees()
    }
}

/// One cell of the anchor grid: the anchors whose position falls in one
/// tile at the bucket zoom, and that tile's world rectangle.
#[derive(Debug, Clone, Default)]
pub struct AnchorBucket {
    /// `[min_x, min_y, max_x, max_y]`, world units.
    pub rect: [f64; 4],
    pub anchors: Vec<WorldAnchor>,
}

/// A source's anchors at one integer zoom, bucketed for the per-frame
/// cull. What `MapView` caches per source and per zoom.
#[derive(Debug, Clone, Default)]
pub struct LineSymbolAnchors {
    /// The integer zoom the spacing was laid out at.
    pub zoom: u8,
    pub buckets: Vec<AnchorBucket>,
    /// Sum of every bucket's anchors.
    pub total: usize,
}

impl LineSymbolAnchors {
    /// Every anchor in a bucket that meets `view` (`[min_x, min_y, max_x,
    /// max_y]`, world units). The bucket test is the only per-frame work
    /// over the whole source; the caller projects and culls exactly.
    pub fn visible(&self, view: [f64; 4]) -> impl Iterator<Item = &WorldAnchor> + '_ {
        self.buckets
            .iter()
            .filter(move |b| b.rect[0] <= view[2] && b.rect[2] >= view[0] && b.rect[1] <= view[3] && b.rect[3] >= view[1])
            .flat_map(|b| b.anchors.iter())
    }

    /// Every anchor, bucket by bucket (tests and the bench).
    pub fn iter(&self) -> impl Iterator<Item = &WorldAnchor> + '_ {
        self.buckets.iter().flat_map(|b| b.anchors.iter())
    }
}

/// Sub-pixel resolution of the walk: the walk's integer unit is
/// `1 / WALK_UNITS_PER_PX` of a logical pixel at the bake zoom, so the
/// rounding `line_anchors_across` does to an anchor is under a pixel.
const WALK_UNITS_PER_PX: f64 = 8.0;

/// The largest coordinate the walk is handed: well inside `i32`, with
/// room for the segment arithmetic it does in `f64` anyway.
const WALK_LIMIT: f64 = (1u64 << 30) as f64;

/// Anchors every `layout.spacing_px_at(zoom)` logical pixels along each
/// line of `lines`, laid out for integer zoom `zoom` (clamped to
/// [`MAX_BAKE_ZOOM`]), bucketed two zooms coarser than the bake (2048 px
/// cells). `None` when `cancelled`.
///
/// Per feature, its parts are moved to the feature's own origin and scaled
/// to `WALK_UNITS_PER_PX` units per pixel at the bake zoom, so the
/// basemap's i32 walk (`symbol_line::line_anchors_across`) runs on it
/// unchanged: spacing is measured along the WHOLE feature, the first anchor
/// half a spacing in, and a feature shorter than one spacing gets one
/// anchor at its middle (MapLibre's `getAnchors`).
pub fn bake_anchors(
    lines: &WorldLines,
    layout: &LineSymbolLayout,
    zoom: u8,
    cancelled: &dyn Fn() -> bool,
) -> Option<LineSymbolAnchors> {
    let zoom = zoom.min(MAX_BAKE_ZOOM);
    let px_per_world = 2f64.powi(i32::from(zoom));
    let spacing_px = layout.spacing_px_at(zoom);
    let bucket_zoom = zoom.saturating_sub(2);
    let bucket_size = f64::from(crate::render::WORLD_SIZE) / 2f64.powi(i32::from(bucket_zoom));
    let mut buckets: std::collections::HashMap<(i64, i64), Vec<WorldAnchor>> = std::collections::HashMap::new();
    let mut total = 0usize;
    for (at, parts) in lines.features.iter().enumerate() {
        if at % 64 == 0 && cancelled() {
            return None;
        }
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for &(x, y) in parts.iter().flatten() {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        // The finest resolution this feature fits the walk at. A feature
        // too long even at one unit per pixel (thousands of km at z22)
        // is skipped rather than wrapped.
        let extent_px = (max_x - min_x).max(max_y - min_y) * px_per_world;
        let Some(units_per_px) =
            [WALK_UNITS_PER_PX, 4.0, 2.0, 1.0].into_iter().find(|u| extent_px * u < WALK_LIMIT)
        else {
            continue;
        };
        let scale = px_per_world * units_per_px;
        let walk: Vec<LineString<i32>> = parts
            .iter()
            .map(|part| {
                LineString(
                    part.iter()
                        .map(|&(x, y)| Coord { x: ((x - min_x) * scale).round() as i32, y: ((y - min_y) * scale).round() as i32 })
                        .collect(),
                )
            })
            .collect();
        for (seq, anchor) in crate::symbol_line::line_anchors_across(&walk, spacing_px * units_per_px).into_iter().enumerate() {
            let (dx, dy) = (
                f64::from(anchor.segment_end.x() - anchor.segment_start.x()),
                f64::from(anchor.segment_end.y() - anchor.segment_start.y()),
            );
            let length = dx.hypot(dy);
            if length <= 0.0 {
                continue;
            }
            let world = (min_x + f64::from(anchor.at.x()) / scale, min_y + f64::from(anchor.at.y()) / scale);
            let key = ((world.0 / bucket_size).floor() as i64, (world.1 / bucket_size).floor() as i64);
            buckets
                .entry(key)
                .or_default()
                .push(WorldAnchor {
                    at: world,
                    dir: ((dx / length) as f32, (dy / length) as f32),
                    seq: u32::try_from(seq).unwrap_or(u32::MAX),
                });
            total += 1;
        }
    }
    let mut buckets: Vec<AnchorBucket> = buckets
        .into_iter()
        .map(|((bx, by), anchors)| AnchorBucket {
            rect: [
                bx as f64 * bucket_size,
                by as f64 * bucket_size,
                (bx + 1) as f64 * bucket_size,
                (by + 1) as f64 * bucket_size,
            ],
            anchors,
        })
        .collect();
    // Deterministic order (the map's is not): row-major, so a frame's
    // candidates come out in the same order every frame.
    buckets.sort_by(|a, b| a.rect[1].total_cmp(&b.rect[1]).then(a.rect[0].total_cmp(&b.rect[0])));
    Some(LineSymbolAnchors { zoom, buckets, total })
}

/// The zoom a camera at `zoom` bakes anchors for: its integer part,
/// clamped to `0..=MAX_BAKE_ZOOM`.
pub fn bake_zoom_for(zoom: f64) -> u8 {
    if zoom.is_finite() {
        (zoom.floor().max(0.0) as u8).min(MAX_BAKE_ZOOM)
    } else {
        0
    }
}

/// Shared, immutable inputs and outputs, as `MapView` caches them.
pub type SharedLines = Arc<WorldLines>;
pub type SharedAnchors = Arc<LineSymbolAnchors>;

/// A headless A/B of the two shapes a route-arrow map can take, through
/// the code `MapView` runs: the push (`OverlayStore::set_source_from`, the
/// synchronous part of `MapView::set_source_from`) and the per-frame overlay
/// symbol loop (`MapView::paint`'s, step for step: cull, project, build the
/// quad), for `none` (lines only), `declared` (`declare_source_symbols`) and
/// `points` (one rotated point per arrow). `probe` only; driven by
/// `examples/line_symbols_probe.rs --frame-bench`.
///
/// What it does NOT include: the basemap, collision placement (every arm
/// draws its arrows `icon-allow-overlap`, which bypasses it), the upload and
/// the GPU. Those are the same, or nearly, across arms; the windowed run of
/// the same example measures the whole frame.
#[cfg(feature = "probe")]
pub mod bench {
    use std::sync::Arc;

    use super::{bake_anchors, LineSymbolLayout, WorldLines};
    use crate::overlay::{FeatureInput, FeaturePaint, GeometryInput, OverlayStore};
    use crate::render::globe::{globe_frame, GlobeAnchors, WorldProjector};

    /// One arm's cost.
    #[derive(Debug, Clone, Default)]
    pub struct ArmCost {
        /// The synchronous push, ms (point generation included for `points`).
        pub push_ms: f64,
        /// Of which: building the arrow features, ms.
        pub gen_ms: f64,
        /// The off-thread anchor bake (`declared` only), ms.
        pub bake_ms: f64,
        /// Per-frame symbol loop, microseconds.
        pub frame_us_p50: f64,
        pub frame_us_p99: f64,
        /// Icons built per frame (mean).
        pub icons_per_frame: f64,
        /// Features pushed in all.
        pub features: usize,
    }

    /// A camera over `center` at `zoom`, `bearing_deg`, `pitch_deg`, 1280 x 800.
    fn camera(center: (f64, f64), zoom: f64, bearing_deg: f32, pitch_deg: f32) -> crate::render::Camera {
        crate::render::Camera {
            center_lon: center.0,
            center_lat: center.1,
            zoom,
            pitch: pitch_deg.to_radians(),
            bearing: bearing_deg.to_radians(),
            viewport_width_px: 1280.0,
            viewport_height_px: 800.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    fn quantile(values: &[f64], q: f64) -> f64 {
        let mut v = values.to_vec();
        v.sort_by(f64::total_cmp);
        v.get(((v.len().max(1) - 1) as f64 * q).round() as usize).copied().unwrap_or(0.0)
    }

    /// Runs one arm. `arrows` is the `points` arm's (position, clockwise
    /// screen rotation); `routes` every arm's lines.
    #[allow(clippy::too_many_arguments)]
    pub fn run_arm(
        arm: &str,
        routes: &[Vec<[f64; 2]>],
        arrows: &dyn Fn() -> Vec<([f64; 2], f32)>,
        layout: &serde_json::Map<String, serde_json::Value>,
        center: (f64, f64),
        zoom: f64,
        bearing_deg: f32,
        pitch_deg: f32,
        frames: usize,
    ) -> ArmCost {
        let mut store = OverlayStore::new();
        let line_paint = FeaturePaint { line_width: crate::style::ZoomValue::constant(4.0), ..FeaturePaint::default() };
        let parts: Vec<[&[[f64; 2]]; 1]> = routes.iter().map(|l| [l.as_slice()]).collect();
        let mut cost = ArmCost::default();
        let started = crate::timing::Instant::now();
        if arm == "declared" {
            store.declare_source_symbols("routes", Some(layout)).expect("declares");
        }
        let lines: Vec<FeatureInput<'_>> = parts
            .iter()
            .map(|p| FeatureInput { id: None, geometry: GeometryInput::Line(p), paint: line_paint.clone() })
            .collect();
        store.set_source_from("routes", &lines, zoom as u8).expect("routes push");
        cost.features = lines.len();
        if arm == "points" {
            let generated = crate::timing::Instant::now();
            let points = arrows();
            let coords: Vec<[[f64; 2]; 1]> = points.iter().map(|(p, _)| [*p]).collect();
            let features: Vec<FeatureInput<'_>> = points
                .iter()
                .zip(&coords)
                .map(|((_, rotate), c)| FeatureInput {
                    id: None,
                    geometry: GeometryInput::Point(c),
                    paint: FeaturePaint {
                        icon_image: Some("chevron".into()),
                        icon_rotate_deg: *rotate,
                        icon_allow_overlap: true,
                        ..FeaturePaint::default()
                    },
                })
                .collect();
            cost.gen_ms = generated.elapsed().as_secs_f64() * 1e3;
            store.set_source_from("arrows", &features, zoom as u8).expect("arrows push");
            cost.features += features.len();
        }
        cost.push_ms = started.elapsed().as_secs_f64() * 1e3;

        let rect = crate::render::IconRect { x: 0, y: 0, width: 32, height: 32, pixel_ratio: 2.0, sdf: false };
        let rects: std::collections::HashMap<String, crate::render::IconRect> = [("chevron".to_string(), rect)].into();
        let geometry = std::collections::HashMap::new();
        let mut fonts = glyphon::FontSystem::new();
        let mut shapes = crate::symbol_pass::LabelShapeCache::new();
        let anchors = if arm == "declared" {
            let snapshot = store.source_snapshot("routes").expect("pushed");
            let layout = snapshot.line_symbols.clone().expect("declared at the push");
            let baked = crate::timing::Instant::now();
            let world = WorldLines::build(&snapshot.features, snapshot.tile_index.as_deref(), &|| false).expect("built");
            let anchors = bake_anchors(&world, &layout, super::bake_zoom_for(zoom), &|| false).expect("baked");
            cost.bake_ms = baked.elapsed().as_secs_f64() * 1e3;
            Some((layout, Arc::new(anchors)))
        } else {
            None
        };
        let arrows_snapshot = store.source_snapshot("arrows");

        let mut times = Vec::with_capacity(frames);
        let mut icons = 0usize;
        for frame in 0..frames {
            let t = frame as f64 / 60.0;
            let at = (center.0 + 0.04 * (t * 0.6).sin(), center.1 + 0.02 * (t * 0.45).sin());
            let camera = camera(at, zoom, bearing_deg, pitch_deg);
            let globe = GlobeAnchors::new(&globe_frame(&camera));
            let started = crate::timing::Instant::now();
            let projector = WorldProjector::new(&camera, &globe);
            let mut drawn: Vec<crate::render::IconInstance> = Vec::new();
            let on_screen = |x: f32, y: f32| {
                let m = crate::symbol_pass::SYMBOL_CULL_MARGIN_PX;
                x >= -m && y >= -m && x <= camera.viewport_width_px + m && y <= camera.viewport_height_px + m
            };
            if let Some((layout, anchors)) = &anchors {
                // The cover's rectangle, generously: two viewports either way.
                let (cx, cy) = crate::overlay::geom::lon_lat_to_world(at.0, at.1);
                let half = 1280.0 / 2f64.powf(zoom);
                let view = [cx - half, cy - half, cx + half, cy + half];
                let icon_size = layout.icon_size_at(super::bake_zoom_for(zoom));
                for anchor in anchors.visible(view) {
                    let Some(((x, y), rotation, scale)) =
                        crate::map_view::line_symbol_on_screen(&projector, anchor, camera.zoom)
                    else {
                        continue;
                    };
                    if !anchor.keeps_at(scale) || !on_screen(x, y) {
                        continue;
                    }
                    if let Some((icon, _)) =
                        crate::symbol_pass::line_symbol_icon(&rect, (x, y), rotation, icon_size, 2.0, (64, 64))
                    {
                        drawn.push(icon);
                    }
                }
            }
            if let Some(snapshot) = &arrows_snapshot {
                for feature in snapshot.features.iter() {
                    if feature.paint.icon_image.is_none() {
                        continue;
                    }
                    let Some((x, y)) = crate::map_view::overlay_symbol_screen_anchor(&projector, &feature.geometry)
                    else {
                        continue;
                    };
                    if !on_screen(x, y) {
                        continue;
                    }
                    let parts = crate::symbol_pass::overlay_symbol_parts(
                        &feature.paint,
                        x,
                        y,
                        2.0,
                        &rects,
                        (64, 64),
                        &geometry,
                        &mut fonts,
                        &mut shapes,
                    );
                    drawn.extend(parts.icons);
                }
            }
            times.push(started.elapsed().as_secs_f64() * 1e6);
            icons += drawn.len();
            std::hint::black_box(&drawn);
        }
        cost.frame_us_p50 = quantile(&times, 0.5);
        cost.frame_us_p99 = quantile(&times, 0.99);
        cost.icons_per_frame = icons as f64 / frames.max(1) as f64;
        cost
    }

    /// The layout `run_arm`'s `declared` arm checks against: compiled, to
    /// fail early.
    pub fn check_layout(layout: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
        LineSymbolLayout::compile(layout).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(json: &str) -> Result<LineSymbolLayout, String> {
        let value: Value = serde_json::from_str(json).unwrap();
        LineSymbolLayout::compile(value.as_object().unwrap())
    }

    fn arrows(spacing: &str) -> LineSymbolLayout {
        layout(&format!(r#"{{"symbol-placement": "line", "icon-image": "arrow", "symbol-spacing": {spacing}}}"#)).unwrap()
    }

    fn line_feature(points: Vec<(f64, f64)>) -> OverlayFeature {
        OverlayFeature {
            index: 0,
            id: None,
            geometry: OverlayGeometry::Line(vec![points]),
            paint: crate::overlay::FeaturePaint::default(),
        }
    }

    fn lines(features: &[OverlayFeature]) -> WorldLines {
        WorldLines::build(features, None, &|| false).unwrap()
    }

    #[test]
    fn a_minimal_declaration_compiles_with_maplibres_defaults() {
        let compiled = layout(r#"{"symbol-placement": "line", "icon-image": "arrow"}"#).unwrap();
        assert_eq!(compiled.icon_image, "arrow");
        assert_eq!(compiled.spacing_px_at(10), 250.0);
        assert_eq!(compiled.icon_size_at(10), 1.0);
        assert!(!compiled.icon_allow_overlap && !compiled.icon_ignore_placement);
        assert!(compiled.visible_at(0.0) && compiled.visible_at(23.9));
    }

    #[test]
    fn every_accepted_key_compiles() {
        let compiled = layout(
            r#"{"symbol-placement": "line", "symbol-spacing": ["interpolate", ["linear"], ["zoom"], 10, 100, 16, 400],
                "icon-image": "chevron", "icon-rotation-alignment": "map", "icon-size": 0.5,
                "icon-allow-overlap": true, "icon-ignore-placement": true, "minzoom": 9, "maxzoom": 20}"#,
        )
        .unwrap();
        assert_eq!(compiled.spacing_px_at(10), 100.0);
        assert_eq!(compiled.spacing_px_at(16), 400.0);
        assert!(compiled.spacing_px_at(13) > 100.0 && compiled.spacing_px_at(13) < 400.0);
        assert_eq!(compiled.icon_size_at(12), 0.5);
        assert!(compiled.bypasses_collision());
        assert!(!compiled.visible_at(8.9) && compiled.visible_at(9.0) && !compiled.visible_at(20.0));
        assert!(layout(r#"{"symbol-placement": "line", "icon-image": "a", "icon-rotation-alignment": "auto"}"#).is_ok());
    }

    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let error = layout(r#"{"symbol-placement": "line", "icon-image": "a", "text-field": "x"}"#).unwrap_err();
        assert!(error.contains("\"text-field\""), "{error}");
    }

    #[test]
    fn a_placement_other_than_line_is_refused() {
        for placement in [r#""point""#, r#""line-center""#, "3"] {
            let error = layout(&format!(r#"{{"symbol-placement": {placement}, "icon-image": "a"}}"#)).unwrap_err();
            assert!(error.contains("symbol-placement"), "{error}");
        }
        assert!(layout(r#"{"icon-image": "a"}"#).unwrap_err().contains("symbol-placement"));
    }

    #[test]
    fn a_data_driven_icon_image_is_refused() {
        let error = layout(r#"{"symbol-placement": "line", "icon-image": ["get", "icon"]}"#).unwrap_err();
        assert!(error.contains("icon-image") && error.contains("data-driven"), "{error}");
        assert!(layout(r#"{"symbol-placement": "line"}"#).unwrap_err().contains("icon-image"));
    }

    #[test]
    fn data_driven_spacing_viewport_alignment_and_bad_zooms_are_refused() {
        assert!(arrows_err(r#"["get", "gap"]"#).contains("symbol-spacing"));
        assert!(layout(r#"{"symbol-placement": "line", "icon-image": "a", "icon-rotation-alignment": "viewport"}"#)
            .unwrap_err()
            .contains("viewport"));
        assert!(layout(r#"{"symbol-placement": "line", "icon-image": "a", "minzoom": 12, "maxzoom": 10}"#).is_err());
        assert!(layout(r#"{"symbol-placement": "line", "icon-image": "a", "icon-allow-overlap": "yes"}"#).is_err());
        assert!(arrows_err("0").contains("at least 1"));
    }

    fn arrows_err(spacing: &str) -> String {
        layout(&format!(r#"{{"symbol-placement": "line", "icon-image": "arrow", "symbol-spacing": {spacing}}}"#))
            .unwrap_err()
    }

    /// The distance between successive anchors, in logical pixels at the
    /// bake zoom.
    fn gaps_px(anchors: &LineSymbolAnchors) -> Vec<f64> {
        let px = 2f64.powi(i32::from(anchors.zoom));
        let mut points: Vec<(f64, f64)> = anchors.iter().map(|a| a.at).collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        points.windows(2).map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1) * px).collect()
    }

    #[test]
    fn anchors_are_spaced_at_the_declared_pixels_at_each_zoom() {
        // A 0.5-degree east-west line on the equator: ~55 km.
        let source = lines(&[line_feature(vec![(103.5, 0.0), (104.0, 0.0)])]);
        for (zoom, spacing) in [(10u8, 100.0), (13, 200.0), (16, 50.0)] {
            let anchors = bake_anchors(&source, &arrows(&spacing.to_string()), zoom, &|| false).unwrap();
            let gaps = gaps_px(&anchors);
            assert!(gaps.len() > 3, "z{zoom}: {} anchors", anchors.total);
            for gap in &gaps {
                assert!((gap - spacing).abs() < 0.5, "z{zoom}: gap {gap} px, declared {spacing} px");
            }
            // The line's length in pixels over the spacing, give or take one.
            let length_px = (crate::overlay::geom::lon_lat_to_world(104.0, 0.0).0
                - crate::overlay::geom::lon_lat_to_world(103.5, 0.0).0)
                * 2f64.powi(i32::from(zoom));
            let expected = (length_px / spacing).round() as i64;
            assert!((anchors.total as i64 - expected).abs() <= 1, "z{zoom}: {} vs {expected}", anchors.total);
        }
    }

    #[test]
    fn a_zoom_curve_spaces_each_zoom_at_its_own_value() {
        let source = lines(&[line_feature(vec![(103.5, 0.0), (104.0, 0.0)])]);
        let curve = arrows(r#"["interpolate", ["linear"], ["zoom"], 10, 100, 14, 300]"#);
        for (zoom, spacing) in [(10u8, 100.0), (14, 300.0)] {
            let anchors = bake_anchors(&source, &curve, zoom, &|| false).unwrap();
            for gap in gaps_px(&anchors) {
                assert!((gap - spacing).abs() < 0.5, "z{zoom}: gap {gap}");
            }
        }
    }

    #[test]
    fn an_anchor_points_along_its_line() {
        let east = lines(&[line_feature(vec![(103.5, 1.0), (103.6, 1.0)])]);
        let north = lines(&[line_feature(vec![(103.5, 1.0), (103.5, 1.1)])]);
        let west = lines(&[line_feature(vec![(103.6, 1.0), (103.5, 1.0)])]);
        let diagonal = lines(&[line_feature(vec![(103.5, 1.0), (103.6, 1.1)])]);
        let angles = |source: &WorldLines| -> Vec<f32> {
            bake_anchors(source, &arrows("50"), 14, &|| false).unwrap().iter().map(WorldAnchor::angle_deg).collect()
        };
        for (name, source, want) in [("east", &east, 0.0f32), ("north", &north, 90.0), ("west", &west, 180.0)] {
            let got = angles(source);
            assert!(!got.is_empty());
            for angle in got {
                let off = ((angle - want + 540.0) % 360.0 - 180.0).abs();
                assert!(off < 0.01, "{name}: {angle} deg, want {want}");
            }
        }
        // 0.1 deg x 0.1 deg near the equator is a hair over 45 deg on the
        // Mercator plane.
        for angle in angles(&diagonal) {
            assert!((angle - 45.0).abs() < 0.2, "diagonal: {angle}");
        }
    }

    #[test]
    fn spacing_runs_across_a_multi_part_line_and_a_short_line_gets_one_anchor() {
        let short = lines(&[line_feature(vec![(103.5, 0.0), (103.5001, 0.0)])]);
        assert_eq!(bake_anchors(&short, &arrows("250"), 12, &|| false).unwrap().total, 1);
        let polygon = OverlayFeature {
            geometry: OverlayGeometry::Polygon(vec![vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]]]),
            ..line_feature(vec![])
        };
        assert!(lines(&[polygon]).features.is_empty(), "a polygon carries no line symbols");
    }

    /// A TILED source's lines live in its tile index once adopted; the
    /// bake reads them back from there and lays out the same anchors.
    #[test]
    fn a_tiled_sources_indexed_lines_bake_like_its_lon_lat_ones() {
        let route = line_feature(vec![(103.5, 1.0), (103.7, 1.05), (103.9, 1.0)]);
        let index = crate::overlay::tile_index::TileIndex::build(
            std::slice::from_ref(&route),
            &[0],
            Default::default(),
            &|| false,
        )
        .expect("builds");
        let indexed = OverlayFeature {
            geometry: OverlayGeometry::Indexed { bbox_lon_lat: (103.5, 1.0, 103.9, 1.05), line: true },
            ..route.clone()
        };
        let from_index = WorldLines::build(&[indexed], Some(&index), &|| false).unwrap();
        assert_eq!(from_index.features.len(), 1, "the index's line is read back");
        let a = bake_anchors(&lines(&[route]), &arrows("100"), 13, &|| false).unwrap();
        let b = bake_anchors(&from_index, &arrows("100"), 13, &|| false).unwrap();
        assert_eq!(a.total, b.total);
        for (x, y) in a.iter().zip(b.iter()) {
            assert!((x.at.0 - y.at.0).abs() * 2f64.powi(13) < 0.5 && (x.at.1 - y.at.1).abs() * 2f64.powi(13) < 0.5);
        }
    }

    #[test]
    fn a_cancelled_bake_returns_nothing() {
        let source = lines(&[line_feature(vec![(103.5, 0.0), (104.0, 0.0)])]);
        assert!(bake_anchors(&source, &arrows("100"), 12, &|| true).is_none());
        assert!(WorldLines::build(&[line_feature(vec![(0.0, 0.0), (1.0, 0.0)])], None, &|| true).is_none());
    }

    #[test]
    fn the_view_test_keeps_only_buckets_that_meet_it() {
        let source = lines(&[line_feature(vec![(103.0, 0.0), (104.0, 0.0)])]);
        let anchors = bake_anchors(&source, &arrows("100"), 14, &|| false).unwrap();
        let (x0, y0) = crate::overlay::geom::lon_lat_to_world(103.4, 0.0);
        let (x1, _) = crate::overlay::geom::lon_lat_to_world(103.5, 0.0);
        let view = [x0, y0 - 0.001, x1, y0 + 0.001];
        let near: Vec<_> = anchors.visible(view).collect();
        assert!(!near.is_empty() && near.len() < anchors.total / 2, "{} of {}", near.len(), anchors.total);
        // Every anchor inside the view is offered.
        let inside = anchors.iter().filter(|a| a.at.0 >= view[0] && a.at.0 <= view[2]).count();
        assert!(near.len() >= inside);
    }

    #[test]
    fn the_bake_zoom_is_the_integer_zoom_clamped() {
        assert_eq!(bake_zoom_for(12.7), 12);
        assert_eq!(bake_zoom_for(-1.0), 0);
        assert_eq!(bake_zoom_for(30.0), MAX_BAKE_ZOOM);
        assert_eq!(bake_zoom_for(f64::NAN), 0);
    }
}

#[cfg(test)]
mod placement_tests {
    //! The draw-side half of line symbols (`MapView`'s symbol pass): here,
    //! not in `map_view.rs`, because that file may not build a `GlobeFrame`
    //! (`render::globe`'s `paint_calls_globe_frame_exactly_once...`).
    use crate::collision::CollisionBox;
    use crate::map_view::{line_symbol_on_screen, overlay_symbol_keys};
    use crate::render::label_plan::OVERLAY_LAYER_INDEX as OVERLAY_COLLISION_LAYER_INDEX;
    use crate::render::IconInstance;
    use crate::symbol_pass::{sort_symbols_for_drawing, DrawnSymbol};
    use crate::overlay::line_symbols::WorldAnchor;
    use crate::render::globe::{globe_frame, GlobeAnchors, WorldProjector};

    fn camera(bearing_deg: f32, pitch_deg: f32) -> crate::render::Camera {
        crate::render::Camera {
            center_lon: 103.85,
            center_lat: 1.30,
            zoom: 14.0,
            pitch: pitch_deg.to_radians(),
            bearing: bearing_deg.to_radians(),
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    /// An anchor at the camera's centre, on a line running `dir` (world
    /// units, y down).
    fn anchor(dir: (f32, f32)) -> WorldAnchor {
        let length = dir.0.hypot(dir.1);
        WorldAnchor {
            at: crate::overlay::geom::lon_lat_to_world(103.85, 1.30),
            dir: (dir.0 / length, dir.1 / length),
            seq: 0,
        }
    }

    fn rotation_deg(camera: &crate::render::Camera, anchor: &WorldAnchor) -> f32 {
        let globe = GlobeAnchors::new(&globe_frame(camera));
        let projector = WorldProjector::new(camera, &globe);
        let (_, rotation, _) = line_symbol_on_screen(&projector, anchor, camera.zoom).expect("the centre projects");
        rotation.to_degrees()
    }

    fn same_angle(a: f32, b: f32) -> bool {
        ((a - b + 540.0).rem_euclid(360.0) - 180.0).abs() < 0.5
    }

    fn drawn_icon(sort_key: f32, screen_x: f32) -> DrawnSymbol {
        DrawnSymbol {
            glyphs: None,
            order: (OVERLAY_COLLISION_LAYER_INDEX, sort_key),
            icons: vec![IconInstance {
                screen_x,
                screen_y: 0.0,
                width: 1.0,
                height: 1.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                rotation_rad: 0.0,
                sdf: None,
                opacity: 1.0,
            }],
            icon_box: Some(CollisionBox::from_origin_size(screen_x, 0.0, 1.0, 1.0)),
            text: None,
            text_box: None,
            text_clip: None,
            text_clip_behind_icons: true,
        }
    }

    /// North up and flat, the icon turns exactly as far as the line: east
    /// is unrotated, north a quarter turn ANTI-clockwise (-90 in the icon
    /// shader's clockwise, y-down convention). The anchor's own map angle
    /// is the counter-clockwise one: east 0, north 90.
    #[test]
    fn a_line_symbol_turns_with_its_line_north_up() {
        let flat = camera(0.0, 0.0);
        for (name, dir, map_deg, screen_deg) in [
            ("east", (1.0, 0.0), 0.0, 0.0),
            ("north", (0.0, -1.0), 90.0, -90.0),
            ("west", (-1.0, 0.0), 180.0, 180.0),
            ("south", (0.0, 1.0), -90.0, 90.0),
        ] {
            let anchor = anchor(dir);
            assert!(same_angle(anchor.angle_deg(), map_deg), "{name}: map angle {}", anchor.angle_deg());
            let got = rotation_deg(&flat, &anchor);
            assert!(same_angle(got, screen_deg), "{name}: screen rotation {got}, want {screen_deg}");
        }
    }

    /// Under a bearing and a pitch the icon follows the line's PROJECTED
    /// direction: the angle between the anchor and a point 60 px further
    /// along the line, both projected independently.
    #[test]
    fn a_line_symbol_follows_its_projected_line_under_bearing_and_pitch() {
        for (bearing, pitch) in [(90.0, 0.0), (37.0, 0.0), (37.0, 50.0), (-120.0, 30.0)] {
            let camera = camera(bearing, pitch);
            let globe = GlobeAnchors::new(&globe_frame(&camera));
            let projector = WorldProjector::new(&camera, &globe);
            for dir in [(1.0, 0.0), (0.0, -1.0), (0.6, 0.8)] {
                let anchor = anchor(dir);
                let far = 60.0 / 2f64.powf(camera.zoom);
                let ahead = (anchor.at.0 + f64::from(anchor.dir.0) * far, anchor.at.1 + f64::from(anchor.dir.1) * far);
                let a = projector.project(anchor.at).unwrap();
                let b = projector.project(ahead).unwrap();
                let want = (b.1 - a.1).atan2(b.0 - a.0).to_degrees();
                let got = rotation_deg(&camera, &anchor);
                assert!(same_angle(got, want), "bearing {bearing} pitch {pitch} dir {dir:?}: {got} vs {want}");
            }
            // And the bearing really turns the icon: east no longer reads 0.
            assert!(!same_angle(rotation_deg(&camera, &anchor((1.0, 0.0))), 0.0), "bearing {bearing}");
        }
    }

    /// Flat, every anchor draws; foreshortened 4x by pitch, every 4th --
    /// and a real pitched camera foreshortens the far side, not the near.
    #[test]
    fn a_foreshortened_line_keeps_every_nth_anchor() {
        let at = |seq| WorldAnchor { seq, ..anchor((1.0, 0.0)) };
        assert!((0..8).all(|seq| at(seq).keeps_at(1.0)));
        assert_eq!((0..16).filter(|&seq| at(seq).keeps_at(0.25)).count(), 4);
        assert_eq!((0..16).filter(|&seq| at(seq).keeps_at(0.5)).count(), 8);
        assert!(at(3).keeps_at(f32::NAN), "an unmeasurable scale thins nothing");
        let pitched = camera(0.0, 60.0);
        let globe = GlobeAnchors::new(&globe_frame(&pitched));
        let projector = WorldProjector::new(&pitched, &globe);
        // A line running north (up the screen, away from the eye).
        let (_, _, centre) = line_symbol_on_screen(&projector, &anchor((0.0, -1.0)), pitched.zoom).unwrap();
        let far = WorldAnchor { at: (anchor((0.0, -1.0)).at.0, anchor((0.0, -1.0)).at.1 - 200.0 / 2f64.powf(pitched.zoom)), ..anchor((0.0, -1.0)) };
        let (_, _, far_scale) = line_symbol_on_screen(&projector, &far, pitched.zoom).unwrap();
        assert!(far_scale < centre && centre < 1.0, "far {far_scale} centre {centre}");
    }

    /// A source's line symbols share one sort key below its own features'
    /// and above every earlier source's -- so, in draw order, they come
    /// after an earlier source's symbols and before their own source's
    /// point symbols.
    #[test]
    fn line_symbols_draw_above_earlier_sources_and_below_their_own_points() {
        // Source "stops": 3 labelled features, no line symbols.
        let (stops_line, stops_base) = overlay_symbol_keys(0, false);
        assert_eq!(stops_line, None);
        // Source "routes": line symbols and 2 labelled features.
        let (routes_line, routes_base) = overlay_symbol_keys(stops_base + 3, true);
        let routes_line = routes_line.expect("a declaring source gets a key");
        assert!(routes_line > (stops_base + 2) as f32, "above every earlier source's symbols");
        assert!(routes_line < routes_base as f32, "below its own features' keys");
        let mut symbols = vec![
            drawn_icon((routes_base + 1) as f32, 6.0),
            drawn_icon(routes_line, 5.0),
            drawn_icon(routes_base as f32, 4.0),
            drawn_icon((stops_base + 2) as f32, 3.0),
            drawn_icon(stops_base as f32, 1.0),
        ];
        sort_symbols_for_drawing(&mut symbols);
        let xs: Vec<f32> = symbols.iter().map(|s| s.icons[0].screen_x).collect();
        assert_eq!(xs, vec![1.0, 3.0, 5.0, 4.0, 6.0], "stops, then the routes' arrows, then the routes' own points");
    }
}
