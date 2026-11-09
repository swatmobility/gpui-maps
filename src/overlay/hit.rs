//! Which feature is under a point.
//!
//! Everything here works in Mercator world space.
//! `render::camera::unproject_to_ground_f64` already turns a cursor
//! position into a world-space ground point and is tested to single-pixel
//! resolution at z20, so hit-testing is a query against the features'
//! stored geometry rather than a projection of every feature to the screen.
//!
//! # What the index holds, and what it deliberately does not
//!
//! ONE BOUNDING BOX PER FEATURE, and no geometry. The exact test -- segment
//! distance, point-in-ring, circle -- projects the candidate feature's own
//! `(lon, lat)` positions at QUERY time, reading them from the feature set
//! the index was built from.
//!
//! This is MapLibre's shape (`FeatureIndex` stores a grid of bboxes per
//! tile and runs the exact geometry test only on what the bbox query
//! returned), and it is here for the same reason: a build that stores
//! geometry has to project every coordinate, and projecting a coordinate is
//! a `tan`, a `cos`, a reciprocal and a `ln`. `plan::build_plan` already
//! projects the whole payload -- it has to, it produces the vertices -- so
//! an index that projected it again was the same transcendental arithmetic
//! run twice for the same numbers. At 69,080 coordinates that measured
//! 1.04 ms of a 3.53 ms push, ~29% of it.
//!
//! A bounding box needs no projection at all: `geom::LonLatBounds`
//! accumulates it in `(lon, lat)` and projects two corners, which is
//! bit-for-bit the box projecting everything would have produced (see its
//! doc comment for why monotonicity makes that exact rather than
//! approximate). So a build is now min/max arithmetic plus two projections
//! per feature, and it stores 40-odd bytes per feature instead of 16 bytes
//! per coordinate.
//!
//! What moved to query time is bounded by the bbox reject: `feature_at`
//! runs on mouse-move and mouse-down only, and a cursor is inside the box
//! of a handful of features at most. Projecting one route's positions on
//! the queries that reach it is nothing beside doing every route's on every
//! push.

use std::sync::Arc;

use crate::overlay::geom::{lon_lat_to_world, LonLatBounds};
use crate::overlay::geojson::OverlayGeometry;
use crate::overlay::{FeatureRef, OverlayFeature};

/// The world-space size of one screen pixel at `zoom`, *as the renderer
/// uses it*: `1.0 / 2f64.powf(zoom)`, verbatim `shader.wgsl`'s
/// `world_units_per_px` and `geo`'s screen_pixels_per_world_unit = 2^zoom
/// convention.
///
/// DELIBERATELY PITCH-FREE, and that is not an oversight -- it is what makes
/// it the right conversion for a DRAWN size. `shader.wgsl` extrudes a stroke
/// vertex by `normal * half_width_px * world_units_per_px` in WORLD space
/// (see `render::mesh::append_stroke`, which is also how a `circle-radius`
/// dot is drawn: a round cap over a zero-length subpath). So an overlay line
/// or marker is a ground-plane ribbon of fixed WORLD width, which a pitched
/// camera then foreshortens -- MapLibre's `circle-pitch-alignment: map`, and
/// its own line shader does the same. Converting a `circle_radius` through
/// this reproduces exactly that, so a marker's hit area is the marker's
/// drawn area at any pitch. Multiplying it by a pitch factor would make the
/// two disagree.
///
/// `tolerance_world`, one below, is the case that DOES need the pitch
/// factor. Read both before touching either.
pub fn world_units_per_px(zoom: f64) -> f64 {
    1.0 / 2f64.powf(zoom)
}

/// How many world units correspond to `tolerance_px` screen pixels at
/// `zoom`, for a cursor whose ground point sits where the camera's
/// `pitch_scale` says.
///
/// Converting a PIXEL tolerance is what keeps a hit target the same physical
/// size on screen at every zoom, instead of a fixed ground distance that
/// becomes unclickable when zoomed out. `pitch_scale` is what keeps it the
/// same size at every point of a PITCHED viewport too.
///
/// Unlike `world_units_per_px` above, a tolerance is not a drawn size at
/// all: it is slack for an imprecise pointing device, and a pointing device
/// is imprecise in SCREEN pixels regardless of where in the viewport it is.
/// Under pitch one screen pixel covers two or three times as much ground
/// near the horizon as it does at the viewport's middle row, and a fraction
/// as much at the bottom -- so a zoom-only conversion silently shrinks the
/// target to a third of its declared size exactly where a user is aiming at
/// the most distorted geometry, and inflates it near the camera. Pass
/// `render::pitch_scale_factor_at_ground` for the cursor's own ground point
/// (see its doc comment for the derivation and for whose upper-bound
/// version of it this is) and 6 declared pixels are 6 pixels everywhere.
///
/// `pitch_scale == 1.0` -- which is what `pitch_scale_factor_at_ground`
/// returns at pitch 0, exactly, at every pixel of the viewport -- collapses
/// this to `tolerance_px / 2f64.powf(zoom)`, the expression this function
/// was before the pitch term existed, bit for bit. Unpitched behaviour is
/// therefore unchanged by construction, not by approximation;
/// `an_unpitched_camera_converts_a_tolerance_exactly_as_it_did_before_pitch_was_carried`
/// pins it.
///
/// The division, rather than a multiplication by `world_units_per_px`, is
/// what makes "bit for bit" literal: `1.0 / 2f64.powf(zoom)` is inexact at
/// fractional zoom, so rounding it first and multiplying second can land an
/// ulp away from dividing once. `tolerance_px * 1.0` is exactly
/// `tolerance_px`, so at `pitch_scale == 1.0` the arithmetic below IS the
/// old arithmetic.
pub fn tolerance_world(tolerance_px: f64, zoom: f64, pitch_scale: f64) -> f64 {
    tolerance_px * pitch_scale / 2f64.powf(zoom)
}

/// One hit-testable feature: where in the feature set it is, how big a box
/// it occupies, and -- for a point feature only -- how wide its marker is.
///
/// NO GEOMETRY. See this module's doc comment: the geometry is read back
/// out of the feature set at query time, which is what lets a build cost
/// two projections per feature instead of one per coordinate.
#[derive(Debug, Clone)]
struct Entry {
    reference: FeatureRef,
    /// This feature's position in the slice `build` was given, which is the
    /// slice `hit_test` must be given.
    ///
    /// Taken from `enumerate` rather than read off `OverlayFeature::index`.
    /// The two agree for every feature the store builds -- `index` IS the
    /// position, and `Source::by_id` already indexes on that -- but a
    /// position is what this field means, and deriving it from a field that
    /// merely happens to equal it is how a future `OverlayFeature` that
    /// renumbers itself would silently hit-test the wrong geometry.
    at: usize,
    /// `Some(circle_radius)` in PIXELS for a point feature, `None`
    /// otherwise.
    ///
    /// Captured at build time rather than read off the feature at query
    /// time, which preserves exactly what the geometry-storing version did:
    /// an in-place `set_feature_paint` that changes `circle-radius` does
    /// NOT move the hit area until the next push. Reading it live would
    /// have been a behaviour change smuggled in with a perf fix.
    point_radius_px: Option<f64>,
    bounds: (f64, f64, f64, f64),
}

/// Every hit-testable feature of ONE SOURCE, in draw order, scanned
/// linearly.
///
/// Not a spatial index despite the name -- a plain `Vec` walked back to
/// front, taking the first entry whose cheap bounding-box test and then
/// whose exact geometry test both pass. Back to front is what makes the
/// TOPMOST feature win, which a bucketed structure would have to reproduce
/// explicitly.
///
/// Linear is the right shape for the sizes this is built for (a fleet and
/// its routes: hundreds of features, queried on mouse-move and mouse-down
/// only, never per frame -- see `MapView::feature_at`). If a caller ever
/// pushes tens of thousands of features, the bounding boxes are already
/// here to bucket.
#[derive(Debug, Default)]
pub struct HitIndex {
    entries: Vec<Entry>,
    /// How many features `build` was given. `hit_test` refuses a slice of a
    /// different length, because every `Entry::at` is a position in THAT
    /// slice and a mismatched one would test somebody else's geometry.
    ///
    /// A length is a weak check and it is deliberately the weak one: the
    /// real guarantee is structural -- `OverlaySnapshot` carries the
    /// features and the index built from them as one value, and
    /// `OverlaySnapshot::feature_at` is the only caller outside tests, so
    /// there is no way to pair a generation's index with another
    /// generation's features. This is the backstop for a future third
    /// caller, and it fails CLOSED (no hit) rather than wrong (a hit on the
    /// wrong feature).
    features: usize,
    /// How many entries are not points (polygons and lines): what
    /// [`AreaCover::push`] reads to skip a points-only source without a walk.
    areas: usize,
}

impl HitIndex {
    /// `source` is the name every `FeatureRef` this index yields is
    /// attributed to -- taken as an argument rather than read off a
    /// feature, because an `OverlayFeature` does not know which source
    /// holds it and should not have to: one `Arc<str>` shared by the whole
    /// index is the right cost, one per feature is not.
    ///
    /// One index PER SOURCE. `MapView::feature_at` queries them in reverse
    /// draw order and takes the first hit, which is how the topmost source
    /// wins a contested point -- the same back-to-front rule this index
    /// already applies within a source, one level up.
    pub fn build(source: &crate::overlay::SourceId, features: &[OverlayFeature]) -> Self {
        let entries: Vec<Entry> = features
            .iter()
            .enumerate()
            .filter_map(|(at, feature)| {
                // `None` for `Empty` and for a geometry whose runs are all
                // empty -- there is nothing on screen to point at, which is
                // the same rule the geometry-storing version applied when
                // `shape_of` or `bounds_of` returned `None`.
                //
                // An adopted tiled feature (`Indexed`) has no runs; its
                // lon/lat box is what it kept, and projects the same way.
                let bounds = match feature.geometry {
                    OverlayGeometry::Indexed { bbox_lon_lat: (w, s, e, n), .. } => {
                        let (x0, y0) = lon_lat_to_world(w, n);
                        let (x1, y1) = lon_lat_to_world(e, s);
                        (x0, y0, x1, y1)
                    }
                    _ => LonLatBounds::of_runs(feature.geometry.runs())?.to_world(),
                };
                let point_radius_px = matches!(feature.geometry, OverlayGeometry::Point(_))
                    .then(|| feature.paint.circle_radius as f64);
                Some(Entry {
                    reference: FeatureRef::of(source, feature),
                    at,
                    point_radius_px,
                    bounds,
                })
            })
            .collect();
        let areas = entries.iter().filter(|entry| entry.point_radius_px.is_none()).count();
        Self { entries, features: features.len(), areas }
    }

    /// The topmost feature within `tolerance_world` of `world`, or `None`.
    ///
    /// `world_units_per_px` is the world-space size of one screen pixel at
    /// the current zoom (`1.0 / 2f64.powf(zoom)`) -- it is what lets a point
    /// feature's PIXEL `circle_radius` be converted into the same world
    /// units as everything else here.
    ///
    /// The two arguments are NOT the same quantity scaled, and a caller must
    /// not derive one from the other: `tolerance_world` carries the camera's
    /// pitch factor and `world_units_per_px` deliberately does not, because
    /// the first bounds a cursor's screen-space slack and the second
    /// reproduces a ground-plane DRAWN size. See this module's two
    /// conversion functions, and `MapView::feature_at`, which is where both
    /// are produced.
    ///
    /// Topmost is LAST in draw order -- the same rule the renderer uses, so
    /// the feature a user clicks is the one they can see.
    ///
    /// `features` must be THE SLICE THIS INDEX WAS BUILT FROM -- the
    /// generation's feature set, which is where the exact test reads its
    /// geometry from (see this module's doc comment). `OverlaySnapshot`
    /// holds the two together and `OverlaySnapshot::feature_at` passes
    /// them together, which is how a caller is kept from pairing one
    /// generation's index with another's features; a slice of the wrong
    /// length is refused outright.
    ///
    /// `index` is the source's adopted tile index, if it has one: an
    /// `Indexed` feature's exact test is `TileIndex::feature_hit` on the
    /// index's fixed-point geometry, and the answer is still the ORIGINAL
    /// feature's `FeatureRef` -- never a tile fragment. Without an index an
    /// `Indexed` feature cannot be tested and misses (fails closed).
    pub fn hit_test(
        &self,
        features: &[OverlayFeature],
        index: Option<&crate::overlay::tile_index::TileIndex>,
        world: (f64, f64),
        tolerance_world: f64,
        world_units_per_px: f64,
    ) -> Option<FeatureRef> {
        self.hit_test_where(features, index, world, tolerance_world, world_units_per_px, &|_| true)
    }

    /// [`Self::hit_test`] over the features `keep` admits (by feature
    /// index): a point a source's GPU filter window hides is not there to
    /// hit, so the next one down is.
    pub fn hit_test_where(
        &self,
        features: &[OverlayFeature],
        index: Option<&crate::overlay::tile_index::TileIndex>,
        world: (f64, f64),
        tolerance_world: f64,
        world_units_per_px: f64,
        keep: &dyn Fn(usize) -> bool,
    ) -> Option<FeatureRef> {
        if features.len() != self.features {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .filter(|entry| keep(entry.at))
            .find(|entry| {
                // A point's true reach can extend past `tolerance_world` --
                // its own radius, converted to world units -- so the cheap
                // bounding-box reject has to allow for that too, or a click
                // near the edge of a large marker would be dropped before
                // the exact circle test ever ran.
                let slack = match entry.point_radius_px {
                    Some(radius_px) => {
                        point_radius_world(radius_px, tolerance_world, world_units_per_px)
                    }
                    None => tolerance_world,
                };
                within_bounds(entry.bounds, world, slack)
                    && match &features[entry.at].geometry {
                        OverlayGeometry::Indexed { .. } => index.is_some_and(|index| {
                            index.feature_hit(entry.at as u32, world, tolerance_world)
                        }),
                        geometry => hits(
                            geometry,
                            entry.point_radius_px,
                            world,
                            tolerance_world,
                            world_units_per_px,
                        ),
                    }
            })
            .map(|entry| entry.reference.clone())
    }
}

impl HitIndex {
    /// Whether `world` lies inside one of these features' AREAS -- a
    /// polygon, never a point or a line -- for labels over data
    /// ([`AreaCover`]). The same exact tests a hover makes, with no
    /// tolerance: inside the ring is inside.
    pub(crate) fn covers_area(
        &self,
        features: &[OverlayFeature],
        index: Option<&crate::overlay::tile_index::TileIndex>,
        world: (f64, f64),
    ) -> bool {
        if features.len() != self.features {
            return false;
        }
        self.entries.iter().any(|entry| {
            entry.point_radius_px.is_none()
                && within_bounds(entry.bounds, world, 0.0)
                && match &features[entry.at].geometry {
                    geometry @ OverlayGeometry::Polygon(_) => hits(geometry, None, world, 0.0, 1.0),
                    // With no tolerance a line part is hit only exactly on
                    // its centreline: in effect only a polygon answers.
                    OverlayGeometry::Indexed { .. } => {
                        index.is_some_and(|index| index.feature_hit(entry.at as u32, world, 0.0))
                    }
                    _ => false,
                }
        })
    }
}

/// The data fills under the labels, as one generation of each overlay
/// source drawn below them: what "is this label over data?" is asked of.
#[derive(Default, Clone)]
pub struct AreaCover {
    parts: Vec<(Arc<Vec<OverlayFeature>>, Arc<HitIndex>, Option<Arc<crate::overlay::tile_index::TileIndex>>)>,
    /// A stamp of the sources and generations in `parts`: a new one drops
    /// [`LabelCover`]'s answers.
    pub(crate) key: u64,
    entries: usize,
}

/// Past this many area entries in all, every label counts as over data
/// (the on-data paint everywhere): a per-label walk of a 242 k block-group
/// map would cost more than the label pass, and data that dense covers
/// the view anyway.
pub(crate) const AREA_COVER_MAX_ENTRIES: usize = 100_000;

impl AreaCover {
    /// Adds one source generation (`source`/`generation` stamp the key).
    pub fn push(
        &mut self,
        source: &str,
        generation: u64,
        features: &Arc<Vec<OverlayFeature>>,
        hit_index: &Arc<HitIndex>,
        tile_index: Option<&Arc<crate::overlay::tile_index::TileIndex>>,
    ) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        // The index's and the tile index's identity too: a tiled source
        // adopts its tile index after the push, without a new generation,
        // and an answer cached before that (an `Indexed` feature with no
        // index to test against misses) must not outlive it.
        let tiles = tile_index.map_or(0, |index| Arc::as_ptr(index) as usize);
        (self.key, source, generation, Arc::as_ptr(hit_index) as usize, tiles).hash(&mut hasher);
        self.key = hasher.finish();
        if hit_index.areas == 0 {
            return;
        }
        self.entries += hit_index.areas;
        self.parts.push((Arc::clone(features), Arc::clone(hit_index), tile_index.cloned()));
    }

    /// No source under the labels has an area (points only, or data
    /// drawn by a layer that is not an overlay source, like hexagon bins).
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Whether `world` is inside a data area; `true` for any point once
    /// the sources hold more than [`AREA_COVER_MAX_ENTRIES`].
    pub fn covers(&self, world: (f64, f64)) -> bool {
        self.entries > AREA_COVER_MAX_ENTRIES
            || self.parts.iter().any(|(features, index, tiles)| index.covers_area(features, tiles.as_deref(), world))
    }
}

/// Per-label memo of [`AreaCover::covers`], kept across frames: a label
/// (its tile and anchor) is tested once per generation of the data under
/// it, not once per frame.
#[derive(Default)]
pub struct LabelCover {
    answers: std::cell::RefCell<std::collections::HashMap<(crate::geo::TileId, i32, i32), bool>>,
    key: std::cell::Cell<u64>,
}

impl LabelCover {
    /// Whether the label anchored at `anchor` (tile extent units) in
    /// `tile` sits over one of `cover`'s areas.
    pub fn covers(&self, cover: &AreaCover, tile: crate::geo::TileId, anchor: (i32, i32)) -> bool {
        let mut answers = self.answers.borrow_mut();
        if self.key.get() != cover.key || answers.len() > 1 << 16 {
            answers.clear();
            self.key.set(cover.key);
        }
        *answers.entry((tile, anchor.0, anchor.1)).or_insert_with(|| cover.covers(anchor_world(tile, anchor)))
    }
}

/// A label anchor (tile extent units in `tile`) in overlay world units.
pub(crate) fn anchor_world(tile: crate::geo::TileId, anchor: (i32, i32)) -> (f64, f64) {
    let extent = f64::from(crate::mvt::MVT_EXTENT);
    let n = f64::from(1u32 << tile.z);
    let world = f64::from(crate::render::WORLD_SIZE);
    (
        (f64::from(tile.x) + f64::from(anchor.0) / extent) / n * world,
        (f64::from(tile.y) + f64::from(anchor.1) / extent) / n * world,
    )
}

fn within_bounds(bounds: (f64, f64, f64, f64), point: (f64, f64), slack: f64) -> bool {
    point.0 >= bounds.0 - slack
        && point.0 <= bounds.2 + slack
        && point.1 >= bounds.1 - slack
        && point.1 <= bounds.3 + slack
}

/// A point's effective on-screen hit radius, in world units: its own PIXEL
/// radius converted at the current zoom, but never smaller than the
/// tolerance -- a tiny (or zero) radius circle should be exactly as
/// clickable as any other point.
///
/// Shared by `hits()`'s exact test and `hit_test`'s bounding-box pre-filter
/// so the two always agree on how big a point's target really is.
fn point_radius_world(radius_px: f64, tolerance_world: f64, world_units_per_px: f64) -> f64 {
    (radius_px * world_units_per_px).max(tolerance_world)
}

/// The exact test, against ONE candidate feature's `(lon, lat)` geometry.
///
/// Every position is projected HERE, once each, and the arithmetic that
/// follows is the same arithmetic the geometry-storing version ran against
/// pre-projected positions -- `lon_lat_to_world` is a pure function of one
/// position, so projecting it late gives the identical `f64` and therefore
/// the identical comparison. Only WHEN the projection happens has moved,
/// and only for the features a bounding box already admitted.
///
/// `point_radius_px` is `Entry::point_radius_px`, passed in rather than
/// re-read off the feature's paint -- see that field for why the build-time
/// capture is the behaviour being preserved.
fn hits(
    geometry: &OverlayGeometry,
    point_radius_px: Option<f64>,
    point: (f64, f64),
    tolerance: f64,
    world_units_per_px: f64,
) -> bool {
    match geometry {
        // `Indexed` is `hit_test`'s to delegate; it never reaches here.
        OverlayGeometry::Empty | OverlayGeometry::Indexed { .. } => false,
        OverlayGeometry::Line(parts) => {
            parts.iter().any(|part| part_hits(point, part, tolerance))
        }
        OverlayGeometry::Polygon(polygons) => polygons.iter().any(|rings| {
            let Some(exterior) = rings.first() else { return false };
            // Inside the exterior and outside every hole. A hole is not part
            // of the polygon, so a click in one must miss.
            point_in_ring(point, exterior)
                && !rings[1..].iter().any(|hole| point_in_ring(point, hole))
        }),
        OverlayGeometry::Point(points) => {
            // The radius is in PIXELS; `world_units_per_px` is what converts
            // it into the same world units as `tolerance` and everything
            // else here.
            let radius_px = point_radius_px.unwrap_or(0.0);
            let radius_world = point_radius_world(radius_px, tolerance, world_units_per_px);
            points.iter().any(|&(lon, lat)| {
                let centre = lon_lat_to_world(lon, lat);
                let (dx, dy) = (point.0 - centre.0, point.1 - centre.1);
                (dx * dx + dy * dy).sqrt() <= radius_world
            })
        }
    }
}

/// Whether `point` is within `tolerance` of any segment of ONE line part.
///
/// Walks the part carrying the previous projected position, rather than
/// projecting a `windows(2)` pair: a run of `n` positions has `n - 1`
/// segments and each interior position is the end of one and the start of
/// the next, so the pair form would project every interior position twice.
/// Same segments, same order, same comparison -- half the trig.
fn part_hits(point: (f64, f64), part: &[(f64, f64)], tolerance: f64) -> bool {
    let mut previous: Option<(f64, f64)> = None;
    for &(lon, lat) in part {
        let current = lon_lat_to_world(lon, lat);
        if let Some(start) = previous {
            if distance_to_segment(point, start, current) <= tolerance {
                return true;
            }
        }
        previous = Some(current);
    }
    false
}

fn distance_to_segment(point: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let length_squared = abx * abx + aby * aby;
    let t = if length_squared <= f64::EPSILON {
        0.0
    } else {
        (((point.0 - a.0) * abx + (point.1 - a.1) * aby) / length_squared).clamp(0.0, 1.0)
    };
    let closest = (a.0 + abx * t, a.1 + aby * t);
    let (dx, dy) = (point.0 - closest.0, point.1 - closest.1);
    (dx * dx + dy * dy).sqrt()
}

/// Standard even-odd ray crossing test, over a ring still in `(lon, lat)`.
///
/// The crossing pairs are `(0, n-1), (1, 0), (2, 1), ...` -- vertex `i`
/// against its predecessor, wrapping -- exactly as the indexed `i`/`j`
/// version this replaced produced them, so the parity it accumulates is the
/// same parity. Each vertex is projected once, plus the wrap's one repeat
/// of the last.
fn point_in_ring(point: (f64, f64), ring: &[(f64, f64)]) -> bool {
    let Some(&(last_lon, last_lat)) = ring.last() else { return false };
    let mut inside = false;
    let mut previous = lon_lat_to_world(last_lon, last_lat);
    for &(lon, lat) in ring {
        let (xi, yi) = lon_lat_to_world(lon, lat);
        let (xj, yj) = previous;
        if (yi > point.1) != (yj > point.1)
            && point.0 < (xj - xi) * (point.1 - yi) / (yj - yi) + xi
        {
            inside = !inside;
        }
        previous = (xi, yi);
    }
    inside
}

impl HitIndex {
    /// The entry table's block (capacity x `size_of::<Entry>()`). The one
    /// `Arc<str>` source name is shared; a `FeatureId::String` id would add
    /// its own block, which the block-group demo (numeric ids) does not have.
    pub fn probe_bytes(&self) -> usize {
        crate::render::vec_block_bytes::<Entry>(self.entries.capacity())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::geojson::parse_feature_collection;
    use crate::overlay::paint::FeaturePaint;
    use crate::overlay::OverlayFeature;

    /// A source name for the index under test. These tests are about
    /// geometry, not attribution, so one arbitrary name is enough -- the
    /// cross-source rule (topmost source wins) lives one level up, in
    /// `MapView::feature_at`, and is tested there.
    fn test_source() -> crate::overlay::SourceId {
        "test".into()
    }

    fn features(json: &str) -> Vec<OverlayFeature> {
        parse_feature_collection(json)
            .unwrap()
            .into_iter()
            .enumerate()
            .map(|(index, feature)| {
                let paint = FeaturePaint::from_properties(&feature.properties).unwrap();
                OverlayFeature {
                    index,
                    id: feature.id,
                    geometry: feature.geometry,
                    paint,
                }
            })
            .collect()
    }

    fn world(lon: f64, lat: f64) -> (f64, f64) {
        crate::overlay::geom::lon_lat_to_world(lon, lat)
    }

    /// An index and the features it was built from, held together --
    /// which is what `hit_test` needs, since the exact test reads the
    /// candidate's geometry back out of them. `OverlaySnapshot` is the
    /// production version of this pairing.
    struct Indexed {
        features: Vec<OverlayFeature>,
        index: HitIndex,
    }

    impl Indexed {
        fn of(json: &str) -> Self {
            let features = features(json);
            let index = HitIndex::build(&test_source(), &features);
            Self { features, index }
        }

        fn hit_test(
            &self,
            world: (f64, f64),
            tolerance_world: f64,
            world_units_per_px: f64,
        ) -> Option<FeatureRef> {
            self.index.hit_test(&self.features, None, world, tolerance_world, world_units_per_px)
        }
    }

    const A_LINE: &str = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":"line","geometry":{"type":"LineString",
         "coordinates":[[0.0,0.0],[1.0,0.0]]},"properties":{}}]}"#;

    const A_SQUARE: &str = r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","id":"square","geometry":{"type":"Polygon",
         "coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]},"properties":{}}]}"#;

    /// Labels over data: only an AREA covers a label; a line, a point or
    /// a points-only source does not, and a points-only source adds
    /// nothing to the cover (so a point-only map keeps every label's own
    /// paint).
    #[test]
    fn only_a_polygon_covers_a_label() {
        let square = Indexed::of(A_SQUARE);
        let line = Indexed::of(A_LINE);
        let points = Indexed::of(r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,1.0]},"properties":{}}]}"#);
        assert!(square.index.covers_area(&square.features, None, world(1.0, 1.0)), "inside the square");
        assert!(!square.index.covers_area(&square.features, None, world(3.0, 1.0)), "outside it");
        assert!(!line.index.covers_area(&line.features, None, world(0.5, 0.0001)), "a line covers nothing");
        assert!(!points.index.covers_area(&points.features, None, world(1.0, 1.0)), "a point covers nothing");

        let mut cover = AreaCover::default();
        cover.push("pts", 1, &Arc::new(points.features), &Arc::new(points.index), None);
        assert!(cover.is_empty(), "a points-only source adds no area");
        cover.push("sq", 1, &Arc::new(square.features), &Arc::new(square.index), None);
        assert!(!cover.is_empty());
        let labels = LabelCover::default();
        let tile = crate::geo::tile_id_at(1.0, 1.0, 8);
        let (x, y) = crate::geo::lon_lat_to_tile_xy(1.0, 1.0, 8.0);
        let extent = f64::from(crate::mvt::MVT_EXTENT);
        let anchor = (((x - f64::from(tile.x)) * extent) as i32, ((y - f64::from(tile.y)) * extent) as i32);
        assert!(labels.covers(&cover, tile, anchor), "a label anchored in the square is over data");
        let away = crate::geo::tile_id_at(10.0, 10.0, 8);
        assert!(!labels.covers(&cover, away, (2048, 2048)), "one far away is not");
    }

    #[test]
    fn an_empty_index_hits_nothing() {
        let index = HitIndex::build(&test_source(), &[]);
        assert!(index.hit_test(&[], None, world(0.0, 0.0), 1.0, 1.0).is_none());
    }

    #[test]
    fn a_point_on_a_line_hits_it() {
        let index = Indexed::of(A_LINE);
        let hit = index.hit_test(world(0.5, 0.0), 1e-6, 1e-6).unwrap();
        assert_eq!(hit.id, Some(crate::overlay::FeatureId::String("line".into())));
    }

    /// The tolerance is a real boundary, not decoration: just inside hits,
    /// just outside misses.
    #[test]
    fn the_tolerance_bounds_a_lines_hit_area() {
        let index = Indexed::of(A_LINE);
        let (line_x, line_y) = world(0.5, 0.0);
        let tolerance = 0.01;

        let just_inside = (line_x, line_y + tolerance * 0.5);
        let just_outside = (line_x, line_y + tolerance * 2.0);
        assert!(index.hit_test(just_inside, tolerance, tolerance).is_some());
        assert!(index.hit_test(just_outside, tolerance, tolerance).is_none());
    }

    #[test]
    fn a_point_inside_a_polygon_hits_it() {
        let index = Indexed::of(A_SQUARE);
        assert!(index.hit_test(world(1.0, 1.0), 1e-9, 1e-9).is_some());
    }

    #[test]
    fn a_point_outside_a_polygon_misses_it() {
        let index = Indexed::of(A_SQUARE);
        assert!(index.hit_test(world(5.0, 5.0), 1e-9, 1e-9).is_none());
    }

    /// A hole is not part of the polygon, so a click in it must miss.
    #[test]
    fn a_point_in_a_polygons_hole_misses_it() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Polygon","coordinates":[
                [[0,0],[4,0],[4,4],[0,4],[0,0]],
                [[1,1],[3,1],[3,3],[1,3],[1,1]]]},"properties":{}}]}"#;
        let index = Indexed::of(json);
        assert!(index.hit_test(world(0.5, 0.5), 1e-9, 1e-9).is_some(), "inside the ring");
        assert!(index.hit_test(world(2.0, 2.0), 1e-9, 1e-9).is_none(), "inside the hole");
    }

    /// Topmost wins, and topmost is LAST in draw order -- the same rule the
    /// renderer uses, so what a user clicks is what they can see.
    #[test]
    fn the_last_feature_in_draw_order_wins_an_overlap() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":"under","geometry":{"type":"Polygon",
             "coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]},"properties":{}},
            {"type":"Feature","id":"over","geometry":{"type":"Polygon",
             "coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]},"properties":{}}]}"#;
        let index = Indexed::of(json);
        let hit = index.hit_test(world(1.0, 1.0), 1e-9, 1e-9).unwrap();
        assert_eq!(hit.id, Some(crate::overlay::FeatureId::String("over".into())));
    }

    /// A point feature's radius is a PIXEL radius that must be converted by
    /// `world_units_per_px`, not scaled by the caller's world-space
    /// tolerance -- the two agree at the feature's exact centre (distance
    /// zero is inside any positive radius, however it is scaled), so this
    /// probes near the true edge of a marker whose radius (7px) is bigger
    /// than the tolerance (6px), the same shape of call the next task
    /// makes. `tolerance * radius_px` and `radius_px * world_units_per_px`
    /// disagree sharply out there.
    #[test]
    fn a_point_features_hit_radius_is_scaled_by_world_units_per_px_not_by_tolerance() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,1.0]},
             "properties":{"circle-radius":7}}]}"#;
        let index = Indexed::of(json);
        let (cx, cy) = world(1.0, 1.0);

        let zoom = 15.0;
        let world_units_per_px = world_units_per_px(zoom);
        // Unpitched, so the pitch factor is 1.0 and this is the same
        // tolerance the zoom-only conversion produced.
        let tolerance = tolerance_world(6.0, zoom, 1.0);
        let radius_world = 7.0 * world_units_per_px;

        let just_inside = (cx + radius_world * 0.9, cy);
        let well_outside = (cx + radius_world * 3.0, cy);

        assert!(index.hit_test(just_inside, tolerance, world_units_per_px).is_some());
        assert!(index.hit_test(well_outside, tolerance, world_units_per_px).is_none());
    }

    /// A feature with no geometry is stored and labelled but cannot be hit
    /// -- there is nothing on screen to point at.
    #[test]
    fn an_empty_geometry_is_never_hit() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":null,"properties":{"text-field":"42"}}]}"#;
        let index = Indexed::of(json);
        assert!(index.hit_test(world(0.0, 0.0), 1.0, 1.0).is_none());
    }

    /// The PIXEL tolerance is constant across zooms while the world
    /// tolerance is not -- which is the entire point of converting one to
    /// the other.
    #[test]
    fn a_pixel_tolerance_shrinks_in_world_units_as_zoom_increases() {
        let at_z10 = tolerance_world(6.0, 10.0, 1.0);
        let at_z16 = tolerance_world(6.0, 16.0, 1.0);
        assert!(at_z16 < at_z10);
        assert!((at_z10 / at_z16 - 64.0).abs() < 1e-9, "six zoom levels is 2^6");
    }

    /// A `pitch_scale` of 1.0 -- what `render::pitch_scale_factor_at_ground`
    /// returns at every pixel of an UNPITCHED viewport, exactly -- must
    /// reproduce `tolerance_px / 2f64.powf(zoom)` to the last bit. Pitch 0
    /// is the overwhelmingly common case and it already worked; the pitch
    /// term must be invisible there, not merely small.
    ///
    /// Fractional zooms are in the list on purpose. `1.0 /
    /// 2f64.powf(zoom)` is inexact at those, so `px * world_units_per_px`
    /// rounds twice and can land an ulp from `px / 2^zoom` -- which is why
    /// `tolerance_world` divides instead of multiplying by its sibling.
    #[test]
    fn an_unpitched_camera_converts_a_tolerance_exactly_as_it_did_before_pitch_was_carried() {
        for zoom in [0.0, 3.0, 7.25, 12.0, 14.3, 16.0, 18.7, 22.0] {
            for tolerance_px in [1.0, 6.0, 0.5, 37.0] {
                let before_the_fix = tolerance_px / 2f64.powf(zoom);
                let now = tolerance_world(tolerance_px, zoom, 1.0);
                assert_eq!(
                    now.to_bits(),
                    before_the_fix.to_bits(),
                    "{tolerance_px}px at z{zoom}: {before_the_fix} became {now}"
                );
            }
        }
    }

    /// `world_units_per_px` is the DRAWN-size conversion and must stay
    /// exactly what the zoom-only `tolerance_world(1.0, zoom)` used to
    /// produce -- it is the argument a point feature's `circle-radius` is
    /// scaled by, and the renderer extrudes that radius in world units with
    /// no pitch term either (`shader.wgsl`). A pitch factor here would make
    /// a marker's hit area disagree with the marker.
    #[test]
    fn the_drawn_size_conversion_is_the_shaders_own_and_carries_no_pitch() {
        for zoom in [0.0, 5.5, 12.0, 16.25, 22.0] {
            assert_eq!(
                world_units_per_px(zoom).to_bits(),
                (1.0 / 2f64.powf(zoom)).to_bits(),
                "z{zoom} must be shader.wgsl's 1.0 / pow(2.0, camera.zoom)"
            );
        }
    }

    /// The pitch factor scales the tolerance linearly and nothing else: a
    /// cursor near the horizon, where one pixel covers twice the ground,
    /// gets twice the world tolerance so that it keeps the same reach in
    /// pixels.
    #[test]
    fn the_pitch_factor_scales_the_tolerance_linearly() {
        let zoom = 16.0;
        let unpitched = tolerance_world(6.0, zoom, 1.0);
        assert!((tolerance_world(6.0, zoom, 2.0) / unpitched - 2.0).abs() < 1e-12);
        assert!((tolerance_world(6.0, zoom, 0.5) / unpitched - 0.5).abs() < 1e-12);
    }

    /// A pitched tolerance widens a LINE's reach without touching a point
    /// feature's own radius, which is the asymmetry `hit_test`'s two
    /// arguments exist to express. Probed at a distance that is outside the
    /// unpitched tolerance and inside the pitched one, against a bare line
    /// -- a hit that the pre-fix conversion refused near the horizon.
    #[test]
    fn a_pitched_tolerance_reaches_further_from_a_line_than_an_unpitched_one() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":"line","geometry":{"type":"LineString",
             "coordinates":[[0.0,-1.0],[0.0,1.0]]},"properties":{}}]}"#;
        let index = Indexed::of(json);
        let (cx, cy) = world(0.0, 0.0);

        let zoom = 16.0;
        let per_px = world_units_per_px(zoom);
        let unpitched = tolerance_world(6.0, zoom, 1.0);
        let pitched = tolerance_world(6.0, zoom, 2.0);

        // Nine pixels out: past six, short of twelve.
        let probe = (cx + 9.0 * per_px, cy);
        assert!(
            index.hit_test(probe, unpitched, per_px).is_none(),
            "9px is outside a 6px tolerance"
        );
        assert!(
            index.hit_test(probe, pitched, per_px).is_some(),
            "9px is inside the same 6px tolerance where a pixel covers twice the ground"
        );
    }

    /// The pre-change even-odd crossing test, over positions that are
    /// ALREADY in world space -- the indexed `i`/`j` form this module used
    /// while the index stored projected geometry.
    ///
    /// Kept in the test module only, as the reference half of
    /// `the_lazy_exact_test_answers_exactly_what_projecting_up_front_answered`:
    /// the live `point_in_ring` projects the ring itself now, so it cannot
    /// double as the eager implementation it is being graded against.
    fn eagerly_in_ring(point: (f64, f64), ring: &[(f64, f64)]) -> bool {
        let mut inside = false;
        let mut j = ring.len().saturating_sub(1);
        for i in 0..ring.len() {
            let (xi, yi) = ring[i];
            let (xj, yj) = ring[j];
            if (yi > point.1) != (yj > point.1)
                && point.0 < (xj - xi) * (point.1 - yi) / (yj - yi) + xi
            {
                inside = !inside;
            }
            j = i;
        }
        inside
    }

    /// THE regression net for moving the projection from build time to
    /// query time: an EAGER reference implementation -- project every
    /// position first, then run the geometric test against the projected
    /// positions, which is verbatim what this module did before -- graded
    /// against the live lazy one over a dense grid of probes.
    ///
    /// Not a spot check. The geometry is a mixed bag on purpose (a
    /// multi-part line, a polygon with a hole, a multi-point marker set,
    /// and a small square sitting on top of the others so the topmost rule
    /// is exercised), and the probes sweep a box that straddles every edge,
    /// the hole and every bounding-box corner, at three zooms. Any probe
    /// where the two disagree -- a hit becoming a miss, or a hit naming a
    /// different feature -- fails, naming the probe.
    #[test]
    fn the_lazy_exact_test_answers_exactly_what_projecting_up_front_answered() {
        let json = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":"road","geometry":{"type":"MultiLineString",
             "coordinates":[[[103.80,1.30],[103.84,1.32],[103.88,1.30]],
                            [[103.90,1.34],[103.94,1.36]]]},"properties":{}},
            {"type":"Feature","id":"block","geometry":{"type":"Polygon",
             "coordinates":[[[103.82,1.28],[103.90,1.28],[103.90,1.36],[103.82,1.36],[103.82,1.28]],
                            [[103.84,1.30],[103.88,1.30],[103.88,1.34],[103.84,1.34],[103.84,1.30]]]},
             "properties":{}},
            {"type":"Feature","id":"stops","geometry":{"type":"MultiPoint",
             "coordinates":[[103.83,1.29],[103.895,1.355],[103.86,1.32]]},
             "properties":{"circle-radius":9}},
            {"type":"Feature","id":"on-top","geometry":{"type":"Polygon",
             "coordinates":[[[103.86,1.315],[103.87,1.315],[103.87,1.325],[103.86,1.325],[103.86,1.315]]]},
             "properties":{}}]}"#;
        let indexed = Indexed::of(json);
        let source = test_source();

        let project =
            |run: &[(f64, f64)]| -> Vec<(f64, f64)> { run.iter().map(|&(lon, lat)| world(lon, lat)).collect() };
        // The eager reference, written out rather than called: this is the
        // arithmetic the change is graded against, so it must not share
        // code with the thing being graded. No bounding-box pre-filter
        // either -- a reject that disagreed with the exact test is exactly
        // the failure this is looking for.
        let eager = |probe: (f64, f64), tolerance: f64, per_px: f64| -> Option<FeatureRef> {
            indexed
                .features
                .iter()
                .rev()
                .find(|feature| match &feature.geometry {
                    OverlayGeometry::Empty | OverlayGeometry::Indexed { .. } => false,
                    OverlayGeometry::Line(parts) => parts.iter().any(|part| {
                        project(part).windows(2).any(|segment| {
                            distance_to_segment(probe, segment[0], segment[1]) <= tolerance
                        })
                    }),
                    OverlayGeometry::Polygon(polygons) => polygons.iter().any(|rings| {
                        let Some(exterior) = rings.first() else { return false };
                        eagerly_in_ring(probe, &project(exterior))
                            && !rings[1..]
                                .iter()
                                .any(|hole| eagerly_in_ring(probe, &project(hole)))
                    }),
                    OverlayGeometry::Point(points) => {
                        let radius_world = point_radius_world(
                            feature.paint.circle_radius as f64,
                            tolerance,
                            per_px,
                        );
                        project(points).iter().any(|centre| {
                            let (dx, dy) = (probe.0 - centre.0, probe.1 - centre.1);
                            (dx * dx + dy * dy).sqrt() <= radius_world
                        })
                    }
                })
                .map(|feature| FeatureRef::of(&source, feature))
        };

        let mut hits = 0usize;
        for zoom in [13.0, 16.0, 19.0] {
            let per_px = world_units_per_px(zoom);
            let tolerance = tolerance_world(6.0, zoom, 1.0);
            for step_lon in 0..40 {
                for step_lat in 0..40 {
                    let lon = 103.79 + step_lon as f64 * 0.004;
                    let lat = 1.27 + step_lat as f64 * 0.0026;
                    let probe = world(lon, lat);
                    let lazy = indexed.hit_test(probe, tolerance, per_px);
                    let reference = eager(probe, tolerance, per_px);
                    assert_eq!(
                        lazy, reference,
                        "z{zoom} at ({lon}, {lat}): lazily {lazy:?}, projecting up front {reference:?}",
                    );
                    hits += usize::from(lazy.is_some());
                }
            }
        }
        // The sweep has to actually HIT things, or the equality above is
        // 4,800 comparisons of `None` against `None`.
        assert!(hits > 400, "the probe grid must land on real geometry; it hit {hits} times");
    }

    /// A slice of the wrong length is a caller pairing one generation's
    /// index with another generation's features. It must fail CLOSED --
    /// no hit -- rather than index into whatever geometry happens to sit at
    /// that position.
    #[test]
    fn an_index_given_the_wrong_feature_set_reports_no_hit() {
        let indexed = Indexed::of(A_SQUARE);
        let inside = world(1.0, 1.0);
        assert!(indexed.hit_test(inside, 1e-9, 1e-9).is_some(), "the right features hit");
        assert!(
            indexed.index.hit_test(&[], None, inside, 1e-9, 1e-9).is_none(),
            "a feature set of the wrong length must not be answered from"
        );
    }
}
