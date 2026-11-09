//! Runtime square-grid binning of points, the way deck.gl's `GridLayer`
//! (and so Kepler.gl's `grid` layer) bins them: square cells in Web
//! Mercator whose side is a ground distance at the data's reference
//! latitude, one aggregate per occupied cell. The square sibling of
//! [`super::hexbin`]: the same options, aggregations (count, sum, mean,
//! min, max and the E10 statistics), percentile window and output shapes.
//!
//! # What was read
//!
//! deck.gl (MIT, `deck.gl` (github.com/visgl/deck.gl)), read, not ported:
//! `modules/aggregation-layers/src/grid-layer/grid-layer.ts`. The cell
//! size in metres becomes common (Mercator) units once, at the data's
//! centre (`unitsPerMeter` at the bounds' centroid), and a point's cell is
//! `floor(p / cellSize)` per axis. Kepler (MIT) passes
//! `cellSize = worldUnitSize · 1000`.
//!
//! # Units
//!
//! Positions are Web Mercator metres (`hexbin::to_mercator`). A ground
//! side `s` at latitude `φ₀` is `s / cos φ₀` there. The lattice origin is
//! Mercator `(0, 0)`, so a cell's id depends on the size and reference
//! latitude only, never on which other points came with it (deck.gl
//! offsets its origin to the data's centre cell for f32 precision; on the
//! CPU in f64 that is not needed, and the cells are the same squares).
//!
//! No gpui, wgpu or DOM: every shell, the browser included, can bin.

use std::collections::HashMap;
use std::sync::Arc;

use super::geojson::OverlayGeometry;
use super::hexbin::{from_mercator, to_mercator, BinAcc, HexAggregation, MAX_LAT};
use super::source_paint::FeatureProperties;
use crate::mvt::TagValue;

/// Binning options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridOptions {
    /// A cell's side, metres on the ground at `reference_lat` (Kepler's
    /// `worldUnitSize` km × 1000).
    pub cell_m: f64,
    pub aggregation: HexAggregation,
    /// The latitude the size is true at; `None` takes the centre of the
    /// points' latitude extent, as deck.gl does.
    pub reference_lat: Option<f64>,
}

impl Default for GridOptions {
    fn default() -> Self {
        Self { cell_m: 1000.0, aggregation: HexAggregation::Count, reference_lat: None }
    }
}

/// One occupied cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridCell {
    /// Column (west to east) and row (south to north) in the lattice.
    pub id: (i32, i32),
    /// The centre, `[lon, lat]`.
    pub center: [f64; 2],
    /// Points that fell in the cell (with or without a finite value).
    pub count: u32,
    /// The aggregate: `count` for a count; `NaN` when no point of the cell
    /// had a finite value.
    pub value: f64,
}

/// A binned point set.
#[derive(Debug, Clone, PartialEq)]
pub struct SquareGrid {
    /// The ground side asked for, metres.
    pub cell_m: f64,
    /// The side in Mercator metres (`cell_m / cos(reference_lat)`).
    pub cell_mercator: f64,
    pub reference_lat: f64,
    pub aggregation: HexAggregation,
    /// Occupied cells, row-major (south to north, west to east):
    /// deterministic for a given input.
    pub cells: Vec<GridCell>,
    /// Points skipped: a non-finite position or one beyond Mercator's
    /// latitude limit.
    pub skipped: usize,
}

/// The cell of a lattice of side `size` (same units as `p`) that `p` falls
/// in: `floor(p / size)` per axis, so a point on a west or south edge
/// belongs to the cell east or north of it.
#[must_use]
pub fn point_to_cell(p: [f64; 2], size: f64) -> (i32, i32) {
    ((p[0] / size).floor() as i32, (p[1] / size).floor() as i32)
}

/// Bins `points` (`[lon, lat]`). `values`, when given, is parallel to
/// `points` and is what every aggregation but `Count` reads.
#[must_use]
pub fn bin_points(points: &[[f64; 2]], values: Option<&[f64]>, options: &GridOptions) -> SquareGrid {
    let reference_lat = options.reference_lat.unwrap_or_else(|| {
        let (lo, hi) = points
            .iter()
            .filter(|p| p[1].is_finite() && p[0].is_finite())
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        if lo.is_finite() { (lo + hi) / 2.0 } else { 0.0 }
    });
    let cos = reference_lat.clamp(-MAX_LAT, MAX_LAT).to_radians().cos().max(1e-6);
    let cell_m = options.cell_m.max(1e-3);
    let size = cell_m / cos;
    let aggregation = options.aggregation;
    let mut acc: HashMap<(i32, i32), BinAcc> = HashMap::new();
    let mut skipped = 0;
    for (k, p) in points.iter().enumerate() {
        if !p[0].is_finite() || !p[1].is_finite() || p[1].abs() > MAX_LAT {
            skipped += 1;
            continue;
        }
        let id = point_to_cell(to_mercator(p[0], p[1]), size);
        acc.entry(id).or_insert_with(|| BinAcc::new(aggregation)).add(aggregation, values.and_then(|v| v.get(k)).copied());
    }
    let mut cells: Vec<GridCell> = acc
        .into_iter()
        .map(|(id, a)| {
            let count = a.count;
            let value = a.finish(aggregation);
            let c = [(f64::from(id.0) + 0.5) * size, (f64::from(id.1) + 0.5) * size];
            GridCell { id, center: from_mercator(c[0], c[1]), count, value }
        })
        .collect();
    cells.sort_unstable_by_key(|c| (c.id.1, c.id.0));
    SquareGrid { cell_m, cell_mercator: size, reference_lat, aggregation, cells, skipped }
}

impl SquareGrid {
    /// Points binned (skipped ones excluded).
    #[must_use]
    pub fn points(&self) -> usize {
        self.cells.iter().map(|c| c.count as usize).sum()
    }

    /// Cell `cell`'s square as a closed `[lon, lat]` ring (counter-clockwise
    /// from its south-west corner), scaled about its centre by `coverage`
    /// (Kepler's `coverage`, `0..=1`; 1 tiles with no gap).
    #[must_use]
    pub fn square(&self, cell: &GridCell, coverage: f64) -> Vec<(f64, f64)> {
        let s = self.cell_mercator;
        let (cx, cy) = ((f64::from(cell.id.0) + 0.5) * s, (f64::from(cell.id.1) + 0.5) * s);
        let h = s * coverage.clamp(0.0, 1.0) / 2.0;
        let mut ring: Vec<(f64, f64)> = [(-h, -h), (h, -h), (h, h), (-h, h)]
            .iter()
            .map(|(dx, dy)| {
                let [lon, lat] = from_mercator(cx + dx, cy + dy);
                (lon, lat)
            })
            .collect();
        ring.push(ring[0]);
        ring
    }

    /// The finite cell values, sorted ascending.
    #[must_use]
    pub fn sorted_values(&self) -> Vec<f64> {
        let mut v: Vec<f64> = self.cells.iter().map(|c| c.value).filter(|v| v.is_finite()).collect();
        v.sort_unstable_by(f64::total_cmp);
        v
    }

    /// deck.gl's percentile window over the sorted cell values, as
    /// [`super::hexbin::HexGrid::percentile_range`].
    #[must_use]
    pub fn percentile_range(&self, lower: f64, upper: f64) -> Option<[f64; 2]> {
        let v = self.sorted_values();
        let n = v.len();
        if n == 0 {
            return None;
        }
        let last = (n - 1) as f64;
        let lo = ((lower.clamp(0.0, 100.0) / 100.0 * last).ceil() as usize).min(n - 1);
        let hi = ((upper.clamp(0.0, 100.0) / 100.0 * last).floor() as usize).min(n - 1);
        Some([v[lo], v[hi.max(lo)]])
    }

    /// Drops every cell whose value lies outside the `[lower, upper]`
    /// percentile window. Returns how many were dropped.
    pub fn retain_percentile(&mut self, lower: f64, upper: f64) -> usize {
        let Some([lo, hi]) = self.percentile_range(lower, upper) else { return 0 };
        let before = self.cells.len();
        self.cells.retain(|c| c.value.is_finite() && c.value >= lo && c.value <= hi);
        before - self.cells.len()
    }

    /// Each cell as an overlay polygon with properties `count` and `value`
    /// (and `cell_i`, `cell_j`).
    #[must_use]
    pub fn features(&self, coverage: f64) -> Vec<(OverlayGeometry, Arc<FeatureProperties>)> {
        self.cells
            .iter()
            .map(|c| {
                let mut props = FeatureProperties::new();
                props.insert("count".into(), TagValue::Number(f64::from(c.count)));
                if c.value.is_finite() {
                    props.insert("value".into(), TagValue::Number(c.value));
                }
                props.insert("cell_i".into(), TagValue::Number(f64::from(c.id.0)));
                props.insert("cell_j".into(), TagValue::Number(f64::from(c.id.1)));
                (OverlayGeometry::Polygon(vec![vec![self.square(c, coverage)]]), Arc::new(props))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(cell_m: f64, aggregation: HexAggregation) -> GridOptions {
        GridOptions { cell_m, aggregation, reference_lat: Some(0.0) }
    }

    /// `[lon, lat]` of a point `dx`, `dy` Mercator metres from the origin.
    fn at(dx: f64, dy: f64) -> [f64; 2] {
        from_mercator(dx, dy)
    }

    #[test]
    fn counts_points_per_cell_on_known_positions() {
        // 100 m cells at the equator: three points in cell (0, 0), one in
        // (1, 0), one in (-1, -1).
        let pts = [at(10.0, 10.0), at(50.0, 90.0), at(99.0, 1.0), at(150.0, 50.0), at(-1.0, -1.0)];
        let g = bin_points(&pts, None, &opts(100.0, HexAggregation::Count));
        let count = |id| g.cells.iter().find(|c| c.id == id).map(|c| c.count);
        assert_eq!(count((0, 0)), Some(3));
        assert_eq!(count((1, 0)), Some(1));
        assert_eq!(count((-1, -1)), Some(1));
        assert_eq!(g.points(), 5);
        assert_eq!(g.cells.len(), 3);
        // Row-major, south first.
        assert_eq!(g.cells[0].id, (-1, -1));
    }

    #[test]
    fn a_smaller_cell_makes_more_cells() {
        let pts: Vec<[f64; 2]> = (0..400).map(|i| at(f64::from(i % 20) * 37.0, f64::from(i / 20) * 41.0)).collect();
        let coarse = bin_points(&pts, None, &opts(400.0, HexAggregation::Count));
        let fine = bin_points(&pts, None, &opts(100.0, HexAggregation::Count));
        assert!(fine.cells.len() > coarse.cells.len() * 4, "{} vs {}", fine.cells.len(), coarse.cells.len());
        assert_eq!(fine.points(), 400);
        assert_eq!(coarse.points(), 400);
    }

    #[test]
    fn aggregates_like_the_hexbin() {
        let pts = [at(10.0, 10.0), at(20.0, 20.0), at(30.0, 30.0), at(40.0, 40.0), at(500.0, 0.0)];
        let vals = [2.0, 4.0, f64::NAN, 4.0, 9.0];
        let get = |a| {
            let g = bin_points(&pts, Some(&vals), &opts(100.0, a));
            let c = *g.cells.iter().find(|c| c.id == (0, 0)).unwrap();
            (c.count, c.value)
        };
        assert_eq!(get(HexAggregation::Count), (4, 4.0));
        assert_eq!(get(HexAggregation::Sum), (4, 10.0));
        assert_eq!(get(HexAggregation::Max), (4, 4.0));
        assert_eq!(get(HexAggregation::Min), (4, 2.0));
        assert_eq!(get(HexAggregation::Median), (4, 4.0));
        assert_eq!(get(HexAggregation::CountUnique), (4, 2.0));
        let (_, mean) = get(HexAggregation::Mean);
        assert!((mean - 10.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn squares_tile_and_are_true_size_on_the_ground() {
        // At 60° N a 1 km cell is 2 km in Mercator.
        let g = bin_points(&[[10.0, 60.0], [10.05, 60.0]], None, &GridOptions { cell_m: 1000.0, aggregation: HexAggregation::Count, reference_lat: Some(60.0) });
        assert!((g.cell_mercator - 2000.0).abs() < 1e-6, "{}", g.cell_mercator);
        // Neighbours share an edge: the east edge of one is the west edge
        // of the next.
        let a = GridCell { id: (3, 7), center: [0.0, 0.0], count: 1, value: 1.0 };
        let b = GridCell { id: (4, 7), ..a };
        let (ra, rb) = (g.square(&a, 1.0), g.square(&b, 1.0));
        assert!((ra[1].0 - rb[0].0).abs() < 1e-9 && (ra[2].0 - rb[3].0).abs() < 1e-9);
        assert_eq!(ra.len(), 5);
        assert_eq!(ra[0], ra[4]);
        // Every point lies in its cell's square.
        for p in [[10.0, 60.0], [10.05, 60.0]] {
            let m = to_mercator(p[0], p[1]);
            let (i, j) = point_to_cell(m, g.cell_mercator);
            assert!(m[0] >= f64::from(i) * g.cell_mercator && m[0] < f64::from(i + 1) * g.cell_mercator);
            assert!(m[1] >= f64::from(j) * g.cell_mercator && m[1] < f64::from(j + 1) * g.cell_mercator);
        }
    }

    #[test]
    fn the_percentile_window_and_features_carry_the_aggregate() {
        let pts: Vec<[f64; 2]> = (0..10).flat_map(|i| (0..=i).map(move |_| at(f64::from(i) * 100.0 + 50.0, 50.0))).collect();
        let mut g = bin_points(&pts, None, &opts(100.0, HexAggregation::Count));
        assert_eq!(g.cells.len(), 10);
        assert_eq!(g.percentile_range(0.0, 100.0), Some([1.0, 10.0]));
        let dropped = g.retain_percentile(0.0, 80.0);
        assert!(dropped >= 1, "the top cells go");
        assert!(g.cells.iter().all(|c| c.value <= 9.0));
        let f = g.features(0.9);
        assert_eq!(f.len(), g.cells.len());
        assert!(matches!(f[0].1.get("count"), Some(TagValue::Number(n)) if *n == 1.0));
        assert!(matches!(f[0].1.get("value"), Some(TagValue::Number(n)) if *n == 1.0));
    }

    #[test]
    fn skips_bad_positions() {
        let g = bin_points(&[[f64::NAN, 0.0], [0.0, 89.0], [0.0, 0.0]], None, &opts(50.0, HexAggregation::Count));
        assert_eq!(g.skipped, 2);
        assert_eq!(g.points(), 1);
    }
}
