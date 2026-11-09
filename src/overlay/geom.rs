//! Where an overlay feature is, in the three spaces it passes through:
//! lon/lat as the caller gives it, Mercator world space as the camera
//! thinks, and a local integer grid the tessellator can work in.

use crate::geo::lon_lat_to_tile_xy;
use crate::render::WORLD_SIZE;

/// Local grid units per world unit.
///
/// Chosen against two bounds. Below: the grid must resolve finer than a
/// screen pixel at the highest zoom this renderer reaches -- one pixel is
/// `1 / 2^zoom` world units (`shader.wgsl`'s `world_units_per_px`), so
/// z20's pixel is about 1e-6 world units and this grid is ten times finer.
/// Above: a local coordinate is an `i32`, and it must stay in the range f32
/// represents usefully once it reaches the tessellator. This bound is a
/// property of the GRID itself, not of the tessellation coordinate scale
/// (`render::mesh::OVERLAY_COORD_SCALE`) applied on top of it: at 1e7 a
/// feature set spanning 1000 km (about 12.78 world units) puts its far edge
/// at about 1.28e8 local units, where f32's spacing at that magnitude is 8
/// units -- an absolute position error of 8 local units, or 8e-7 world
/// units, or about 0.063 m: under a pixel (~0.85 px) at zoom 20. That is
/// comfortable for the city-to-metro-scale data this is built for, and it
/// barely moves with the tessellation scale: at the coordinate magnitude
/// this task's overlay stroke fix moved away from (`OVERLAY_COORD_SCALE =
/// 1000.0`, the same 1.28e8 local units become 1.28e11 lyon units, where
/// f32's spacing is 8192), the same error is about 8.192 local units --
/// still only ~0.064 m, ~0.84 px at zoom 20. Multiplying every coordinate
/// by a constant moves its magnitude and its float spacing together, so
/// POSITION precision is essentially unaffected by that scale either way.
///
/// What the tessellation scale DOES affect is the tessellated STROKE
/// NORMAL's precision, and that is governed by the tessellation half-width
/// (`render::mesh::STROKE_TESSELLATION_WIDTH`, 50 lyon units), not by this
/// grid. lyon derives a stroke vertex's normal from the difference between
/// two points a FIXED 50 lyon units apart -- a distance that does not grow
/// with the coordinate magnitude the way the position error above does. At
/// `OVERLAY_COORD_SCALE = 1000.0` a coordinate's float spacing (8192 lyon
/// units, at the 1000 km span above) dwarfed that fixed 50-unit half-width,
/// so the two points a normal is derived from could round to the same
/// float and the normal collapsed to zero -- an invisible line. See
/// `docs/superpowers/specs/2026-08-25-overlay-stroke-precision-design.md`
/// for the full account and the numbers measured on real data.
pub const OVERLAY_UNITS_PER_WORLD: f64 = 1e7;

/// Longitude/latitude to Mercator world space, the `[0, 512)` square the
/// camera and every tile matrix already work in.
pub fn lon_lat_to_world(lon: f64, lat: f64) -> (f64, f64) {
    let (x, y) = lon_lat_to_tile_xy(lon, lat, 0.0);
    (x * WORLD_SIZE as f64, y * WORLD_SIZE as f64)
}

/// The bounding box of a run of `(lon, lat)` positions, accumulated
/// WITHOUT projecting any of them.
///
/// # Why a lon/lat box rather than a world-space one
///
/// Because `lon_lat_to_world` is monotonic in each axis independently, and
/// that makes projecting two corners exactly as good as projecting every
/// position -- see [`Self::world_corners`] for the argument and
/// `the_two_projected_corners_are_the_box_projecting_everything_gives` for
/// the bit-level proof.
///
/// The projection is the expensive part of touching a coordinate: `x` is
/// two flops, but `y` is a `tan`, a `cos`, a reciprocal and a `ln`. Two
/// `f64::min`/`max` pairs are essentially free beside it. So a bounding box
/// is the one thing an overlay can want from 69,080 coordinates and get for
/// the price of 2 -- which is what both of this module's callers want it
/// for:
///
/// - `plan::build_plan` needs the box's CENTRE (its `OverlayOrigin`) before
///   it can convert anything to local grid units. It used to get it by
///   projecting every coordinate into a `Vec` and throwing the `Vec` away,
///   so every push projected the whole payload twice.
/// - `hit::HitIndex::build` needs one box PER FEATURE as its cheap reject.
///   It used to get it by projecting and STORING every coordinate, so every
///   push projected the whole payload a third time and kept 16 bytes per
///   coordinate of the result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LonLatBounds {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl LonLatBounds {
    /// The box around every position of every run, or `None` if there are
    /// none.
    ///
    /// Takes RUNS -- `(lon, lat)` slices -- rather than a flat stream of
    /// positions, because the inner loop is then a walk of a contiguous
    /// slice that the optimiser will unroll. Flattening the three-way
    /// `chain` of nested `Vec`s that `OverlayGeometry::positions` is into
    /// a position-at-a-time accumulator instead measured 4.0
    /// ns/coordinate against 1.6 for this, on the same payload: the
    /// arithmetic is four compares either way, and the difference is
    /// entirely iterator shape. `hit::HitIndex::build` calls this once per
    /// feature, and it is the whole of what a build now costs.
    pub fn of_runs<'a>(runs: impl Iterator<Item = &'a [(f64, f64)]>) -> Option<Self> {
        let mut bounds = Self::nothing();
        let mut seen = false;
        for run in runs {
            seen |= !run.is_empty();
            for &position in run {
                bounds.extend(position);
            }
        }
        seen.then_some(bounds)
    }

    /// The empty box: every accumulator at the infinity it will be beaten
    /// from. Not public, because on its own it is not a box -- it is
    /// inverted, and `of_runs` is what turns it into one or into `None`.
    fn nothing() -> Self {
        Self {
            min_lon: f64::INFINITY,
            min_lat: f64::INFINITY,
            max_lon: f64::NEG_INFINITY,
            max_lat: f64::NEG_INFINITY,
        }
    }

    fn extend(&mut self, (lon, lat): (f64, f64)) {
        self.min_lon = self.min_lon.min(lon);
        self.min_lat = self.min_lat.min(lat);
        self.max_lon = self.max_lon.max(lon);
        self.max_lat = self.max_lat.max(lat);
    }

    /// The two world-space corners of this box: the north-west
    /// (`min_lon`, `max_lat`) first, then the south-east (`max_lon`,
    /// `min_lat`).
    ///
    /// TWO projections, and they carry the same information as projecting
    /// every position the box was accumulated from, because
    /// `lon_lat_to_world` is separable and monotonic:
    ///
    /// - `x = (lon + 180) / 360 * 512` is non-decreasing in `lon` and does
    ///   not depend on `lat`, so `min x` is at `min_lon` and `max x` at
    ///   `max_lon`.
    /// - `y = (1 - ln(tan(φ) + sec(φ)) / π) / 2 * 512` over
    ///   `φ = clamp(lat, ±85.05)` is non-increasing in `lat` and does not
    ///   depend on `lon`, so `min y` is at `max_lat` -- NORTH IS LESS Y,
    ///   which is the axis flip this method's return order encodes -- and
    ///   `max y` at `min_lat`.
    ///
    /// Non-strict monotonicity is enough: where two distinct inputs project
    /// to the same output (two longitudes a float apart, two latitudes past
    /// the pole clamp) the extremum is that shared value either way. And
    /// because the extremal position is projected by the SAME function on
    /// the same input, the result is not an approximation of what the
    /// project-everything loop produced -- it is bit for bit that value.
    pub fn world_corners(self) -> [(f64, f64); 2] {
        [
            lon_lat_to_world(self.min_lon, self.max_lat),
            lon_lat_to_world(self.max_lon, self.min_lat),
        ]
    }

    /// This box in world space as `(min_x, min_y, max_x, max_y)`.
    pub fn to_world(self) -> (f64, f64, f64, f64) {
        let [(min_x, min_y), (max_x, max_y)] = self.world_corners();
        (min_x, min_y, max_x, max_y)
    }
}

/// The world-space point an overlay's local grid is centred on.
///
/// Local coordinates are small integers measured from here, which is what
/// keeps f32 vertex positions near zero. Tessellating in raw world
/// coordinates instead reintroduces the vertex-lattice quantisation
/// `docs/superpowers/specs/2026-08-19-per-tile-precision-matrix.md` fixed
/// for tiles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayOrigin {
    pub world_x: f64,
    pub world_y: f64,
    /// Lyon units per local grid unit: what this origin's plans are
    /// tessellated at, in place of a fixed `render::mesh::OVERLAY_COORD_SCALE`.
    ///
    /// `1.0` -- the grid IS the lyon space, `OVERLAY_UNITS_PER_WORLD` per
    /// world unit -- unless the source reaches further from its centre than
    /// an `i32` of that grid can (214.7 world units, ~151 deg of
    /// longitude). Then the smallest power of two that brings its far edge
    /// back inside: the GRID coarsens by that factor (2 for any source
    /// within the world: 2e-7 world units, still five times finer than a z20
    /// pixel), and the tessellator multiplies it back out, so the mesh it
    /// bakes is in the same lyon space as every other overlay mesh. Nothing
    /// downstream of the tessellator -- the model matrix, the globe
    /// uniform, the dash scale, chunk culling -- knows the grid was coarse.
    ///
    /// Before this, such a source's `to_local` saturated and drew everything
    /// past 151 deg on a false stripe there. A power of two, so the
    /// multiply back out is exact in f32; and `1.0` for every source that
    /// fits, so those bake bit for bit as they always did.
    pub coord_scale: f32,
}

impl OverlayOrigin {
    /// An origin at `(world_x, world_y)` on the full-resolution grid
    /// (`coord_scale` 1): for a local grid whose extent is known to fit,
    /// such as a dot tile's.
    #[must_use]
    pub fn at(world_x: f64, world_y: f64) -> Self {
        Self { world_x, world_y, coord_scale: 1.0 }
    }

    /// The centre of the bounding box of `points` (world space), or `None`
    /// if there are none.
    ///
    /// The bounding-box centre rather than the mean: a hundred stops
    /// clustered at one end of a route would drag a mean toward them and
    /// leave the far end further from the origin than it needs to be.
    ///
    /// The box also sets [`Self::coord_scale`]: no point of it is further
    /// from the centre than half its larger side.
    pub fn centred_on(points: impl Iterator<Item = (f64, f64)>) -> Option<Self> {
        let mut min = (f64::INFINITY, f64::INFINITY);
        let mut max = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut seen = false;
        for (x, y) in points {
            seen = true;
            min = (min.0.min(x), min.1.min(y));
            max = (max.0.max(x), max.1.max(y));
        }
        seen.then(|| Self {
            world_x: (min.0 + max.0) / 2.0,
            world_y: (min.1 + max.1) / 2.0,
            coord_scale: coord_scale_for_reach(((max.0 - min.0) / 2.0).max((max.1 - min.1) / 2.0)),
        })
    }

    /// [`Self::centred_on`] for runs of positions that are still
    /// `(lon, lat)`, for the price of TWO projections instead of one per
    /// position.
    ///
    /// Identical to `centred_on(positions.map(lon_lat_to_world))` bit for
    /// bit, and not by luck: the origin is a bounding-box CENTRE, a
    /// bounding box is two corners, and [`LonLatBounds::world_corners`] is
    /// exactly those two corners -- so the box handed to `centred_on` below
    /// is the same box the project-everything version accumulated, and the
    /// division that halves it is the same division. `the_origin_is_the_
    /// same_whether_every_point_or_two_corners_are_projected` pins it.
    ///
    /// This is what `plan::build_plan` calls, and it is the whole reason a
    /// push no longer projects its payload to find out where the middle of
    /// it is. RUNS rather than a flat position stream for the reason
    /// [`LonLatBounds::of_runs`] gives.
    pub fn centred_on_lon_lat<'a>(
        runs: impl Iterator<Item = &'a [(f64, f64)]>,
    ) -> Option<Self> {
        let bounds = LonLatBounds::of_runs(runs)?;
        Self::centred_on(bounds.world_corners().into_iter())
    }

    /// World space to this overlay's local integer grid.
    ///
    /// Saturating rather than wrapping: a feature set spanning more of the
    /// world than the grid can express should draw its far features at the
    /// edge of the representable area, not on the opposite side of it.
    /// That is a backstop now -- a source can only reach past its grid if it is
    /// wider than [`coord_scale_for_reach`] coarsens for, which no source
    /// within one world is -- and in units of [`Self::coord_scale`] lyon
    /// units each. At `coord_scale` 1 the division is exact, so the grid is
    /// bit for bit the one it always was.
    pub fn to_local(self, world: (f64, f64)) -> (i32, i32) {
        let step = f64::from(self.coord_scale);
        let scale = |value: f64, origin: f64| -> i32 {
            let units = (value - origin) * OVERLAY_UNITS_PER_WORLD / step;
            if units >= i32::MAX as f64 {
                i32::MAX
            } else if units <= i32::MIN as f64 {
                i32::MIN
            } else {
                units.round() as i32
            }
        };
        (scale(world.0, self.world_x), scale(world.1, self.world_y))
    }

    /// The inverse of `to_local`, lossy by at most one grid unit.
    ///
    /// Exercised only by this module's round-trip test today -- nothing in
    /// the draw path converts back, since the model matrix does that job on
    /// the GPU. Kept because that round trip is what pins the grid's
    /// resolution against `OVERLAY_UNITS_PER_WORLD`'s two bounds.
    #[allow(dead_code)]
    pub fn to_world(self, local: (i32, i32)) -> (f64, f64) {
        let step = f64::from(self.coord_scale);
        (
            self.world_x + local.0 as f64 * step / OVERLAY_UNITS_PER_WORLD,
            self.world_y + local.1 as f64 * step / OVERLAY_UNITS_PER_WORLD,
        )
    }
}

/// The [`OverlayOrigin::coord_scale`] for a source reaching `reach` world
/// units from its origin: `1.0` while `reach` fits an `i32` of
/// `OVERLAY_UNITS_PER_WORLD` grid units, else the smallest power of two
/// that brings it back inside. A reach that is not finite (input
/// validation refuses those before they get here) keeps `1.0`.
#[must_use]
pub fn coord_scale_for_reach(reach: f64) -> f32 {
    let mut scale = 1.0f32;
    if reach.is_finite() {
        // 2^16 is a reach of 2^16 x 214.7 world units: far past any
        // longitude a caller could unwrap to.
        while reach * OVERLAY_UNITS_PER_WORLD / f64::from(scale) >= f64::from(i32::MAX) && scale < 65_536.0 {
            scale *= 2.0;
        }
    }
    scale
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Greenwich on the equator is the centre of the world square.
    #[test]
    fn lon_lat_zero_is_the_middle_of_world_space() {
        let (x, y) = lon_lat_to_world(0.0, 0.0);
        assert!((x - 256.0).abs() < 1e-9, "x was {x}");
        assert!((y - 256.0).abs() < 1e-9, "y was {y}");
    }

    /// East is +x and north is -y, matching the tile scheme the rest of the
    /// renderer uses (tile y grows southward).
    #[test]
    fn east_increases_x_and_north_decreases_y() {
        let (x0, y0) = lon_lat_to_world(0.0, 0.0);
        let (east, _) = lon_lat_to_world(1.0, 0.0);
        let (_, north) = lon_lat_to_world(0.0, 1.0);
        assert!(east > x0);
        assert!(north < y0);
    }

    #[test]
    fn an_origin_over_no_points_is_none() {
        assert!(OverlayOrigin::centred_on(std::iter::empty()).is_none());
    }

    /// The origin is the centre of the bounding box, not the mean of the
    /// points -- a hundred stops clustered at one end must not drag it.
    #[test]
    fn the_origin_is_the_bounding_box_centre_not_the_average() {
        let points = vec![(10.0, 10.0), (10.0, 10.0), (10.0, 10.0), (20.0, 30.0)];
        let origin = OverlayOrigin::centred_on(points.into_iter()).unwrap();
        assert_eq!(origin.world_x, 15.0);
        assert_eq!(origin.world_y, 20.0);
    }

    #[test]
    fn the_origin_maps_to_local_zero() {
        let origin = OverlayOrigin::at(200.0, 300.0);
        assert_eq!(origin.to_local((200.0, 300.0)), (0, 0));
    }

    /// A round trip through the local grid is lossy only by the grid's own
    /// resolution. Singapore-scale offsets must survive far better than a
    /// pixel at the zooms this renderer reaches.
    #[test]
    fn a_local_round_trip_holds_to_grid_resolution() {
        let (ox, oy) = lon_lat_to_world(103.85, 1.29);
        let origin = OverlayOrigin::at(ox, oy);
        // Roughly 25 km east and north of the origin.
        let point = lon_lat_to_world(104.08, 1.52);
        let round_tripped = origin.to_world(origin.to_local(point));
        let tolerance = 1.0 / OVERLAY_UNITS_PER_WORLD;
        assert!((round_tripped.0 - point.0).abs() <= tolerance);
        assert!((round_tripped.1 - point.1).abs() <= tolerance);
    }

    /// The grid must resolve better than one screen pixel at the highest
    /// zoom this renderer reasonably reaches. At zoom z one pixel is
    /// `1 / 2^z` world units (see `shader.wgsl`'s `world_units_per_px`).
    #[test]
    fn grid_resolution_is_finer_than_a_pixel_at_zoom_20() {
        let world_units_per_pixel_at_z20 = 1.0 / 2f64.powi(20);
        assert!(1.0 / OVERLAY_UNITS_PER_WORLD < world_units_per_pixel_at_z20);
    }

    /// A spread of positions covering both hemispheres, both signs of
    /// longitude, the equator, the pole clamp and a couple of exact
    /// duplicates -- the awkward cases for a claim about monotonicity.
    fn awkward_positions() -> Vec<(f64, f64)> {
        vec![
            (103.8198, 1.3521),
            (103.8198, 1.3521),
            (-74.0060, 40.7128),
            (139.6917, 35.6895),
            (-0.1276, 51.5072),
            (0.0, 0.0),
            (18.4241, -33.9249),
            (-179.9999, -85.0),
            (179.9999, 85.0),
            // Past the clamp on both sides, where the projection is
            // deliberately NOT strictly monotonic in latitude.
            (12.0, 89.9),
            (12.0, -89.9),
            (1e-12, 1e-12),
        ]
    }

    /// THE claim `LonLatBounds` rests on: projecting the two corners of the
    /// lon/lat box gives the same world-space box that projecting every
    /// position and taking the extremes of the results gives -- bit for
    /// bit, not to a tolerance.
    ///
    /// If this ever fails, `hit::HitIndex`'s bounding-box reject and
    /// `plan::build_plan`'s origin are both wrong, and the cheap path has
    /// to go back to projecting everything.
    #[test]
    fn the_two_projected_corners_are_the_box_projecting_everything_gives() {
        let positions = awkward_positions();

        let mut expected = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for &(lon, lat) in &positions {
            let (x, y) = lon_lat_to_world(lon, lat);
            expected.0 = expected.0.min(x);
            expected.1 = expected.1.min(y);
            expected.2 = expected.2.max(x);
            expected.3 = expected.3.max(y);
        }

        let cheap = LonLatBounds::of_runs(std::iter::once(positions.as_slice()))
            .unwrap()
            .to_world();
        assert_eq!(cheap.0.to_bits(), expected.0.to_bits(), "min x");
        assert_eq!(cheap.1.to_bits(), expected.1.to_bits(), "min y");
        assert_eq!(cheap.2.to_bits(), expected.2.to_bits(), "max x");
        assert_eq!(cheap.3.to_bits(), expected.3.to_bits(), "max y");
    }

    /// North is LESS y, so the box's north-west corner carries `min_y` and
    /// its south-east corner `max_y`. Getting this flip wrong would make
    /// every bounding box inverted and every hit miss.
    #[test]
    fn the_boxs_first_corner_is_north_west_and_carries_the_smaller_y() {
        let corners = [(10.0, -20.0), (30.0, 40.0)];
        let bounds = LonLatBounds::of_runs(std::iter::once(&corners[..])).unwrap();
        let [(nw_x, nw_y), (se_x, se_y)] = bounds.world_corners();
        assert!(nw_x < se_x, "west is less x");
        assert!(nw_y < se_y, "north is less y");
        assert_eq!(bounds.to_world(), (nw_x, nw_y, se_x, se_y));
    }

    #[test]
    fn a_box_over_no_positions_is_none() {
        assert!(LonLatBounds::of_runs(std::iter::empty()).is_none());
        assert!(LonLatBounds::of_runs(std::iter::once(&[][..])).is_none(), "an empty run is no positions");
    }

    /// The origin `build_plan` now computes must be the one it used to,
    /// exactly -- it is what every i32 local coordinate, and therefore
    /// every vertex, is measured from.
    #[test]
    fn the_origin_is_the_same_whether_every_point_or_two_corners_are_projected() {
        let positions = awkward_positions();
        let projecting_everything =
            OverlayOrigin::centred_on(positions.iter().map(|&(lon, lat)| lon_lat_to_world(lon, lat)))
                .unwrap();
        let two_corners =
            OverlayOrigin::centred_on_lon_lat(std::iter::once(positions.as_slice())).unwrap();
        assert_eq!(two_corners.world_x.to_bits(), projecting_everything.world_x.to_bits());
        assert_eq!(two_corners.world_y.to_bits(), projecting_everything.world_y.to_bits());
    }

    /// No runs at all, and a run that is empty, are both "no positions".
    #[test]
    fn an_origin_over_no_lon_lat_positions_is_none() {
        assert!(OverlayOrigin::centred_on_lon_lat(std::iter::empty()).is_none());
        assert!(OverlayOrigin::centred_on_lon_lat(std::iter::once(&[][..])).is_none());
    }

    /// Run boundaries do not move a bounding box -- the same positions
    /// split differently, with an empty run thrown in, are the same box.
    /// `of_runs` takes runs for the inner loop's shape, not because a run
    /// means anything to a box.
    #[test]
    fn splitting_the_positions_into_runs_does_not_move_the_box() {
        let positions = awkward_positions();
        let one_run = LonLatBounds::of_runs(std::iter::once(positions.as_slice())).unwrap();
        let (head, tail) = positions.split_at(5);
        let three_runs = LonLatBounds::of_runs([head, &[][..], tail].into_iter()).unwrap();
        assert_eq!(three_runs, one_run);
    }

    /// i32 must span a continent's worth of local units without wrapping.
    /// 1000 km at the equator is 1000/40075 of the world square.
    #[test]
    fn the_local_grid_spans_a_thousand_kilometres_without_overflowing_i32() {
        let thousand_km_in_world_units = 512.0 * 1000.0 / 40_075.0;
        let local = thousand_km_in_world_units * OVERLAY_UNITS_PER_WORLD;
        assert!(local < i32::MAX as f64, "local was {local}");
    }

    /// The grid reaches every position of its own source. An origin is a
    /// bounding-box centre, so a source spanning the whole world puts its
    /// corners 256 world units out on both axes; at `coord_scale` 1 the
    /// `i32` saturated at 214.7 (~151 deg of longitude) and drew everything
    /// beyond on a stripe there.
    #[test]
    fn every_position_round_trips_from_the_centre_of_its_own_bounding_box() {
        let world = f64::from(WORLD_SIZE);
        for (points, what) in [
            (vec![(0.0, 0.0), (world, world), (0.0, world), (world, 0.0)], "the whole world"),
            (vec![lon_lat_to_world(-179.5, 0.0), lon_lat_to_world(179.5, 0.0)], "either side of the antimeridian"),
            (vec![lon_lat_to_world(-170.0, -85.0), lon_lat_to_world(170.0, 85.0)], "340 deg by the pole clamp"),
            // Unwrapped the short way across 180 deg twice over: 3.5 worlds.
            (vec![lon_lat_to_world(-180.0, 0.0), (3.5 * world, 256.0)], "a longitude unwrapped past 900 deg"),
        ] {
            let origin = OverlayOrigin::centred_on(points.iter().copied()).unwrap();
            let tolerance = f64::from(origin.coord_scale) / OVERLAY_UNITS_PER_WORLD;
            for &p in &points {
                let back = origin.to_world(origin.to_local(p));
                assert!(
                    (back.0 - p.0).abs() <= tolerance && (back.1 - p.1).abs() <= tolerance,
                    "{what}: {p:?} came back as {back:?} (coord_scale {})",
                    origin.coord_scale
                );
            }
        }
    }

    /// A source that fits keeps the grid it always had -- which is what
    /// keeps every overlay that drew correctly before bit for bit -- and one
    /// that does not coarsens by the smallest power of two that fits.
    #[test]
    fn only_a_source_wider_than_the_grid_coarsens_it() {
        assert_eq!(coord_scale_for_reach(0.0), 1.0);
        assert_eq!(coord_scale_for_reach(214.0), 1.0, "~150 deg from the centre still fits");
        assert_eq!(coord_scale_for_reach(215.0), 2.0);
        assert_eq!(coord_scale_for_reach(256.0), 2.0, "half the world, the most a source within it reaches");
        assert_eq!(coord_scale_for_reach(429.0), 2.0);
        assert_eq!(coord_scale_for_reach(430.0), 4.0);
        assert_eq!(coord_scale_for_reach(f64::INFINITY), 1.0);
        assert_eq!(coord_scale_for_reach(f64::NAN), 1.0);
        // The coarse grid is still far finer than a z20 pixel.
        assert!(f64::from(coord_scale_for_reach(256.0)) / OVERLAY_UNITS_PER_WORLD < 1.0 / 2f64.powi(20) / 4.0);
    }
}
