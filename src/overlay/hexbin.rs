//! Runtime hexagon binning of points, the way deck.gl's `HexagonLayer`
//! (and so Kepler.gl's `hexagon` layer) bins them: a planar grid of
//! pointy-top hexagons in Web Mercator, whose radius is a ground distance
//! at the data's reference latitude, one aggregate per occupied bin.
//!
//! The output is ordinary overlay polygons with properties
//! ([`HexGrid::features`], [`HexGrid::geojson`]): a flat hexbin map is a
//! data-driven fill source, a 3D one goes through
//! [`crate::overlay::extrude`].
//!
//! # What was read
//!
//! deck.gl (MIT, `deck.gl` (github.com/visgl/deck.gl)), read, not ported line by line:
//!
//! - `modules/aggregation-layers/src/hexagon-layer/hexbin.ts`: the lattice
//!   (`DIST_X = 2 sin 60°`, `DIST_Y = 1.5`, odd rows shifted half a
//!   column), the nearest-centre test of d3-hexbin, and the vertices at
//!   `(sin(i·60°), −cos(i·60°))`.
//! - `hexagon-layer.ts`: the radius in metres is turned into common
//!   (Mercator) units once, at the data's centre (`unitsPerMeter` at the
//!   bounds' centroid), so every hexagon is the same size in Mercator and
//!   the requested size on the ground at that latitude.
//!
//! Kepler (MIT) passes `radius = worldUnitSize · 1000`, `coverage`, and
//! with no colour field aggregates by point count.
//!
//! # Units
//!
//! Positions are projected to Web Mercator metres (`R = 6 378 137`,
//! y north). A ground radius `r` at latitude `φ₀` is `r / cos φ₀` there.
//! The lattice origin is Mercator `(0, 0)`, so a bin's id depends on the
//! radius and reference latitude only, never on which other points came
//! with it.
//!
//! No gpui, wgpu or DOM: every shell, the browser included, can bin.

use std::collections::HashMap;
use std::sync::Arc;

use super::geojson::OverlayGeometry;
use super::source_paint::FeatureProperties;
use crate::mvt::TagValue;

/// The WGS84 / Web Mercator sphere radius, metres.
pub const MERCATOR_RADIUS_M: f64 = 6_378_137.0;
const DIST_X: f64 = 1.732_050_807_568_877_2; // 2 sin 60°
const DIST_Y: f64 = 1.5;
/// Web Mercator's latitude limit.
pub(crate) const MAX_LAT: f64 = 85.051_128_779_806_59;

/// How a bin's points become its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HexAggregation {
    /// The number of points (values ignored).
    #[default]
    Count,
    /// The sum of the finite values.
    Sum,
    /// The mean of the finite values.
    Mean,
    /// The largest finite value.
    Max,
    /// The smallest finite value.
    Min,
    /// The median of the finite values (d3-array's `median`: the mean of
    /// the two middle values of an even count).
    Median,
    /// The sample standard deviation of the finite values (d3-array's
    /// `deviation`, `n - 1` in the denominator); `NaN` under two values.
    Stdev,
    /// The sample variance of the finite values (d3-array's `variance`);
    /// `NaN` under two values.
    Variance,
    /// The most frequent finite value; a tie goes to the smallest value
    /// (Kepler's `getMode` keeps the first key it met, which is input
    /// order: smallest is the deterministic stand-in).
    Mode,
    /// How many distinct finite values (Kepler's `countUnique`).
    CountUnique,
}

impl HexAggregation {
    /// Every aggregation, in Kepler.gl's menu order.
    pub const ALL: [HexAggregation; 10] = [
        HexAggregation::Count,
        HexAggregation::Mean,
        HexAggregation::Max,
        HexAggregation::Min,
        HexAggregation::Median,
        HexAggregation::Stdev,
        HexAggregation::Sum,
        HexAggregation::Variance,
        HexAggregation::Mode,
        HexAggregation::CountUnique,
    ];

    /// Kepler's / deck.gl's names (`average`, `sum`, `maximum`, `minimum`,
    /// `count`, `median`, `stdev`, `variance`, `mode`, `countUnique`),
    /// case-insensitively, plus `mean`, `max`, `min`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "count" => Self::Count,
            "sum" => Self::Sum,
            "mean" | "average" => Self::Mean,
            "max" | "maximum" => Self::Max,
            "min" | "minimum" => Self::Min,
            "median" => Self::Median,
            "stdev" | "deviation" => Self::Stdev,
            "variance" => Self::Variance,
            "mode" => Self::Mode,
            "countunique" | "count unique" | "count_unique" => Self::CountUnique,
            _ => return None,
        })
    }

    /// Kepler.gl's name for it (what a config stores).
    #[must_use]
    pub fn kepler_name(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Mean => "average",
            Self::Max => "maximum",
            Self::Min => "minimum",
            Self::Median => "median",
            Self::Stdev => "stdev",
            Self::Variance => "variance",
            Self::Mode => "mode",
            Self::CountUnique => "countUnique",
        }
    }

    /// Whether it reads every value of a bin (rather than a running total):
    /// those bins keep their values until the aggregate is taken.
    #[must_use]
    pub fn needs_all_values(self) -> bool {
        matches!(self, Self::Median | Self::Stdev | Self::Variance | Self::Mode | Self::CountUnique)
    }
}

/// One bin's running aggregate: the point count, the finite values seen,
/// and either a running accumulator or (for [`HexAggregation::needs_all_values`])
/// the values themselves. Shared by [`bin_points`] and
/// [`super::gridbin::bin_points`].
#[derive(Debug, Clone)]
pub(crate) struct BinAcc {
    pub count: u32,
    finite: u32,
    acc: f64,
    values: Vec<f64>,
}

impl BinAcc {
    pub(crate) fn new(aggregation: HexAggregation) -> Self {
        let acc = match aggregation {
            HexAggregation::Max => f64::NEG_INFINITY,
            HexAggregation::Min => f64::INFINITY,
            _ => 0.0,
        };
        Self { count: 0, finite: 0, acc, values: Vec::new() }
    }

    /// Adds one point and, unless `aggregation` is a count, its value.
    pub(crate) fn add(&mut self, aggregation: HexAggregation, value: Option<f64>) {
        self.count += 1;
        if aggregation == HexAggregation::Count {
            return;
        }
        let Some(v) = value.filter(|v| v.is_finite()) else { return };
        self.finite += 1;
        match aggregation {
            HexAggregation::Sum | HexAggregation::Mean => self.acc += v,
            HexAggregation::Max => self.acc = self.acc.max(v),
            HexAggregation::Min => self.acc = self.acc.min(v),
            HexAggregation::Count => {}
            _ => self.values.push(v),
        }
    }

    /// The bin's aggregate: `count` for a count, `NaN` when no point had
    /// a finite value (or a deviation has fewer than two).
    pub(crate) fn finish(mut self, aggregation: HexAggregation) -> f64 {
        if aggregation == HexAggregation::Count {
            return f64::from(self.count);
        }
        if self.finite == 0 {
            return f64::NAN;
        }
        let n = self.values.len();
        match aggregation {
            HexAggregation::Mean => self.acc / f64::from(self.finite),
            HexAggregation::Sum | HexAggregation::Max | HexAggregation::Min | HexAggregation::Count => self.acc,
            HexAggregation::Median => {
                self.values.sort_unstable_by(f64::total_cmp);
                if n % 2 == 1 {
                    self.values[n / 2]
                } else {
                    (self.values[n / 2 - 1] + self.values[n / 2]) / 2.0
                }
            }
            HexAggregation::Stdev | HexAggregation::Variance => {
                if n < 2 {
                    return f64::NAN;
                }
                // Welford: stable for large, close values.
                let (mut mean, mut m2) = (0.0, 0.0);
                for (k, v) in self.values.iter().enumerate() {
                    let d = v - mean;
                    mean += d / (k + 1) as f64;
                    m2 += d * (v - mean);
                }
                let var = m2 / (n - 1) as f64;
                if aggregation == HexAggregation::Stdev {
                    var.sqrt()
                } else {
                    var
                }
            }
            HexAggregation::Mode | HexAggregation::CountUnique => {
                self.values.sort_unstable_by(f64::total_cmp);
                let (mut unique, mut best, mut best_run) = (0usize, self.values[0], 0usize);
                let mut i = 0;
                while i < n {
                    let mut j = i + 1;
                    while j < n && self.values[j] == self.values[i] {
                        j += 1;
                    }
                    unique += 1;
                    if j - i > best_run {
                        best_run = j - i;
                        best = self.values[i];
                    }
                    i = j;
                }
                if aggregation == HexAggregation::Mode {
                    best
                } else {
                    unique as f64
                }
            }
        }
    }
}

/// Binning options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HexbinOptions {
    /// Centre to vertex, metres on the ground at `reference_lat`
    /// (Kepler's `worldUnitSize` km × 1000).
    pub radius_m: f64,
    pub aggregation: HexAggregation,
    /// The latitude the radius is true at; `None` takes the centre of the
    /// points' latitude extent, as deck.gl does.
    pub reference_lat: Option<f64>,
}

impl Default for HexbinOptions {
    fn default() -> Self {
        Self { radius_m: 1000.0, aggregation: HexAggregation::Count, reference_lat: None }
    }
}

/// One occupied bin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HexBin {
    /// Lattice column and row (odd rows are shifted half a column east).
    pub id: (i32, i32),
    /// The centre, `[lon, lat]`.
    pub center: [f64; 2],
    /// Points that fell in the bin (with or without a finite value).
    pub count: u32,
    /// The aggregate: `count` for [`HexAggregation::Count`]; `NaN` when no
    /// point of the bin had a finite value.
    pub value: f64,
}

/// A binned point set.
#[derive(Debug, Clone, PartialEq)]
pub struct HexGrid {
    /// The ground radius asked for, metres.
    pub radius_m: f64,
    /// The radius in Mercator metres (`radius_m / cos(reference_lat)`).
    pub radius_mercator: f64,
    pub reference_lat: f64,
    pub aggregation: HexAggregation,
    /// Occupied bins, in row-major lattice order (south to north, west to
    /// east): deterministic for a given input.
    pub bins: Vec<HexBin>,
    /// Points skipped: a non-finite position or one beyond Mercator's
    /// latitude limit.
    pub skipped: usize,
}

/// `[lon, lat]` in degrees to Web Mercator metres (y north).
#[must_use]
pub fn to_mercator(lon: f64, lat: f64) -> [f64; 2] {
    let lat = lat.clamp(-MAX_LAT, MAX_LAT);
    [MERCATOR_RADIUS_M * lon.to_radians(), MERCATOR_RADIUS_M * (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln()]
}

/// Web Mercator metres to `[lon, lat]` degrees.
#[must_use]
pub fn from_mercator(x: f64, y: f64) -> [f64; 2] {
    [(x / MERCATOR_RADIUS_M).to_degrees(), (2.0 * (y / MERCATOR_RADIUS_M).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees()]
}

/// The bin of the lattice of radius `radius` (same units as `p`) that `p`
/// falls in: d3-hexbin's two-candidate test, with the candidates compared
/// by true distance (see the body).
#[must_use]
pub fn point_to_hexbin(p: [f64; 2], radius: f64) -> (i32, i32) {
    let py = p[1] / radius / DIST_Y;
    let mut pj = py.round();
    let px = p[0] / radius / DIST_X - (pj as i64 & 1) as f64 / 2.0;
    let mut pi = px.round();
    let py1 = py - pj;
    if py1.abs() * 3.0 > 1.0 {
        let px1 = px - pi;
        let pi2 = pi + (if px < pi { -1.0 } else { 1.0 }) / 2.0;
        let pj2 = pj + (if py < pj { -1.0 } else { 1.0 });
        let px2 = px - pi2;
        let py2 = py - pj2;
        // Compared in true (isotropic) units: d3-hexbin compares the
        // normalised offsets, which leans the boundary off the drawn
        // hexagon edge by up to a few percent of the radius; here a point
        // goes to the centre it is nearest, so it always lies in the
        // hexagon drawn for its bin.
        let d1 = (px1 * DIST_X).powi(2) + (py1 * DIST_Y).powi(2);
        let d2 = (px2 * DIST_X).powi(2) + (py2 * DIST_Y).powi(2);
        if d1 > d2 {
            pi = pi2 + (if pj as i64 & 1 != 0 { 1.0 } else { -1.0 }) / 2.0;
            pj = pj2;
        }
    }
    (pi as i32, pj as i32)
}

/// The centre of bin `id` in the lattice of radius `radius`.
#[must_use]
pub fn hexbin_center(id: (i32, i32), radius: f64) -> [f64; 2] {
    let (i, j) = id;
    [(f64::from(i) + f64::from(j & 1) / 2.0) * radius * DIST_X, f64::from(j) * radius * DIST_Y]
}

/// The six vertices of a hexagon of radius `radius` round `center`,
/// pointy-top, clockwise from north in a y-north frame.
#[must_use]
pub fn hexagon_vertices(center: [f64; 2], radius: f64) -> [[f64; 2]; 6] {
    std::array::from_fn(|k| {
        let a = k as f64 * std::f64::consts::FRAC_PI_3;
        [center[0] + radius * a.sin(), center[1] + radius * a.cos()]
    })
}

/// Bins `points` (`[lon, lat]`). `values`, when given, is parallel to
/// `points` and is what every aggregation but `Count` reads.
#[must_use]
pub fn bin_points(points: &[[f64; 2]], values: Option<&[f64]>, options: &HexbinOptions) -> HexGrid {
    let reference_lat = options.reference_lat.unwrap_or_else(|| {
        let (lo, hi) = points
            .iter()
            .filter(|p| p[1].is_finite() && p[0].is_finite())
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        if lo.is_finite() { (lo + hi) / 2.0 } else { 0.0 }
    });
    let cos = reference_lat.clamp(-MAX_LAT, MAX_LAT).to_radians().cos().max(1e-6);
    let radius_m = options.radius_m.max(1e-3);
    let radius = radius_m / cos;
    let mut acc: HashMap<(i32, i32), BinAcc> = HashMap::new();
    let mut skipped = 0;
    let aggregation = options.aggregation;
    for (k, p) in points.iter().enumerate() {
        if !p[0].is_finite() || !p[1].is_finite() || p[1].abs() > MAX_LAT {
            skipped += 1;
            continue;
        }
        let id = point_to_hexbin(to_mercator(p[0], p[1]), radius);
        acc.entry(id).or_insert_with(|| BinAcc::new(aggregation)).add(aggregation, values.and_then(|v| v.get(k)).copied());
    }
    let mut bins: Vec<HexBin> = acc
        .into_iter()
        .map(|(id, a)| {
            let count = a.count;
            let value = a.finish(aggregation);
            let c = hexbin_center(id, radius);
            HexBin { id, center: from_mercator(c[0], c[1]), count, value }
        })
        .collect();
    bins.sort_unstable_by_key(|b| (b.id.1, b.id.0));
    HexGrid { radius_m, radius_mercator: radius, reference_lat, aggregation: options.aggregation, bins, skipped }
}

impl HexGrid {
    /// Points binned (skipped ones excluded).
    #[must_use]
    pub fn points(&self) -> usize {
        self.bins.iter().map(|b| b.count as usize).sum()
    }

    /// Bin `bin`'s hexagon as a closed `[lon, lat]` ring, scaled by
    /// `coverage` (Kepler's `coverage`, `0..=1`; 1 tiles with no gap).
    #[must_use]
    pub fn hexagon(&self, bin: &HexBin, coverage: f64) -> Vec<(f64, f64)> {
        let c = hexbin_center(bin.id, self.radius_mercator);
        let mut ring: Vec<(f64, f64)> = hexagon_vertices(c, self.radius_mercator * coverage.clamp(0.0, 1.0))
            .iter()
            .map(|v| {
                let [lon, lat] = from_mercator(v[0], v[1]);
                (lon, lat)
            })
            .collect();
        ring.push(ring[0]);
        ring
    }

    /// The finite bin values, sorted ascending.
    #[must_use]
    pub fn sorted_values(&self) -> Vec<f64> {
        let mut v: Vec<f64> = self.bins.iter().map(|b| b.value).filter(|v| v.is_finite()).collect();
        v.sort_unstable_by(f64::total_cmp);
        v
    }

    /// deck.gl's percentile window over the sorted bin values
    /// (`BinSorter.getValueRange`): `[sorted[ceil(lo%·(n−1))],
    /// sorted[floor(hi%·(n−1))]]`. `None` with no finite value.
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

    /// Drops every bin whose value lies outside the `[lower, upper]`
    /// percentile window: deck.gl hides those cells (`upperPercentile`,
    /// Kepler's `percentile`). Returns how many were dropped.
    pub fn retain_percentile(&mut self, lower: f64, upper: f64) -> usize {
        let Some([lo, hi]) = self.percentile_range(lower, upper) else { return 0 };
        let before = self.bins.len();
        self.bins.retain(|b| b.value.is_finite() && b.value >= lo && b.value <= hi);
        before - self.bins.len()
    }

    /// Each bin as an overlay polygon with properties `count` and `value`
    /// (and `hex_i`, `hex_j`).
    #[must_use]
    pub fn features(&self, coverage: f64) -> Vec<(OverlayGeometry, Arc<FeatureProperties>)> {
        self.bins
            .iter()
            .map(|b| {
                let mut props = FeatureProperties::new();
                props.insert("count".into(), TagValue::Number(f64::from(b.count)));
                if b.value.is_finite() {
                    props.insert("value".into(), TagValue::Number(b.value));
                }
                props.insert("hex_i".into(), TagValue::Number(f64::from(b.id.0)));
                props.insert("hex_j".into(), TagValue::Number(f64::from(b.id.1)));
                (OverlayGeometry::Polygon(vec![vec![self.hexagon(b, coverage)]]), Arc::new(props))
            })
            .collect()
    }

    /// The bins as a GeoJSON FeatureCollection of `Polygon`s with `count`
    /// and `value` properties (coordinates to 6 decimals), for any door
    /// that takes GeoJSON text (`MapView::set_source`, a capture overlay).
    /// `extra` is spliced into every feature's properties as is (a JSON
    /// object's members without the braces, or empty).
    #[must_use]
    pub fn geojson(&self, coverage: f64, extra: &str) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(self.bins.len() * 220 + 64);
        out.push_str(r#"{"type":"FeatureCollection","features":["#);
        for (n, b) in self.bins.iter().enumerate() {
            if n > 0 {
                out.push(',');
            }
            out.push_str(r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[["#);
            for (k, (x, y)) in self.hexagon(b, coverage).iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                let _ = write!(out, "[{x:.6},{y:.6}]");
            }
            let _ = write!(out, r#"]]}},"properties":{{"count":{}"#, b.count);
            if b.value.is_finite() {
                let _ = write!(out, r#","value":{}"#, b.value);
            }
            if !extra.is_empty() {
                out.push(',');
                out.push_str(extra);
            }
            out.push_str("}}");
        }
        out.push_str("]}");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(radius_m: f64, aggregation: HexAggregation) -> HexbinOptions {
        HexbinOptions { radius_m, aggregation, reference_lat: Some(0.0) }
    }

    /// A point `(dx, dy)` metres from `[lon0, lat0]` at the equator
    /// (Mercator metres are ground metres there).
    fn at(dx: f64, dy: f64) -> [f64; 2] {
        from_mercator(dx, dy)
    }

    #[test]
    fn mercator_round_trips() {
        for &(lon, lat) in &[(0.0, 0.0), (-122.45, 37.76), (151.2, -33.9), (179.9, 84.0)] {
            let m = to_mercator(lon, lat);
            let back = from_mercator(m[0], m[1]);
            assert!((back[0] - lon).abs() < 1e-9 && (back[1] - lat).abs() < 1e-9, "{lon},{lat} -> {back:?}");
        }
    }

    #[test]
    fn counts_points_per_bin_on_known_positions() {
        // radius 100 m at the equator: centres (0,0), (173.2,0), (86.6,150).
        let pts = [at(0.0, 0.0), at(10.0, -20.0), at(-30.0, 40.0), at(173.0, 5.0), at(170.0, -10.0), at(86.0, 150.0)];
        let g = bin_points(&pts, None, &opts(100.0, HexAggregation::Count));
        let count = |id: (i32, i32)| g.bins.iter().find(|b| b.id == id).map(|b| b.count);
        assert_eq!(count((0, 0)), Some(3));
        assert_eq!(count((1, 0)), Some(2));
        assert_eq!(count((0, 1)), Some(1));
        assert_eq!(g.bins.len(), 3);
        assert_eq!(g.points(), 6);
        assert!(g.bins.iter().all(|b| b.value == f64::from(b.count)));
    }

    #[test]
    fn aggregates_values_sum_mean_max_min() {
        let pts = [at(0.0, 0.0), at(5.0, 5.0), at(-5.0, 0.0), at(500.0, 0.0)];
        let vals = [2.0, 4.0, f64::NAN, 7.0];
        let get = |a| {
            let g = bin_points(&pts, Some(&vals), &opts(100.0, a));
            let b = *g.bins.iter().find(|b| b.id == (0, 0)).unwrap();
            (b.count, b.value)
        };
        assert_eq!(get(HexAggregation::Sum), (3, 6.0));
        assert_eq!(get(HexAggregation::Mean), (3, 3.0), "NaN skipped");
        assert_eq!(get(HexAggregation::Max), (3, 4.0));
        assert_eq!(get(HexAggregation::Min), (3, 2.0));
        assert_eq!(get(HexAggregation::Count), (3, 3.0));
        let nan_only = bin_points(&[at(0.0, 0.0)], Some(&[f64::NAN]), &opts(100.0, HexAggregation::Mean));
        assert!(nan_only.bins[0].value.is_nan());
    }

    /// E10: Kepler's statistics over a bin, as d3-array takes them
    /// (sample variance and deviation, the mean of the middle pair for an
    /// even median), with NaN skipped.
    #[test]
    fn aggregates_median_stdev_variance_mode_and_unique_like_kepler() {
        let pts = [at(0.0, 0.0), at(5.0, 5.0), at(-5.0, 0.0), at(3.0, -3.0), at(-2.0, 2.0), at(1.0, 1.0), at(500.0, 0.0)];
        let vals = [2.0, 4.0, 4.0, f64::NAN, 4.0, 5.0, 7.0];
        let get = |a| {
            let g = bin_points(&pts, Some(&vals), &opts(100.0, a));
            let b = *g.bins.iter().find(|b| b.id == (0, 0)).unwrap();
            (b.count, b.value)
        };
        // Finite values in the bin: 2, 4, 4, 4, 5.
        assert_eq!(get(HexAggregation::Median), (6, 4.0));
        assert_eq!(get(HexAggregation::Mode), (6, 4.0));
        assert_eq!(get(HexAggregation::CountUnique), (6, 3.0));
        let (_, var) = get(HexAggregation::Variance);
        assert!((var - 1.2).abs() < 1e-12, "sample variance of 2,4,4,4,5 is 1.2: {var}");
        let (_, sd) = get(HexAggregation::Stdev);
        assert!((sd - 1.2f64.sqrt()).abs() < 1e-12);
        // An even count's median is the mean of the middle pair; one value
        // has no deviation.
        let even = bin_points(&[at(0.0, 0.0), at(1.0, 0.0)], Some(&[1.0, 2.0]), &opts(100.0, HexAggregation::Median));
        assert_eq!(even.bins[0].value, 1.5);
        let one = bin_points(&[at(0.0, 0.0)], Some(&[3.0]), &opts(100.0, HexAggregation::Stdev));
        assert!(one.bins[0].value.is_nan());
        // A tie goes to the smallest value.
        let tie = bin_points(&[at(0.0, 0.0), at(1.0, 0.0), at(2.0, 0.0), at(3.0, 0.0)], Some(&[9.0, 1.0, 9.0, 1.0]), &opts(100.0, HexAggregation::Mode));
        assert_eq!(tie.bins[0].value, 1.0);
        for a in HexAggregation::ALL {
            assert_eq!(HexAggregation::parse(a.kepler_name()), Some(a), "{a:?} round-trips by its Kepler name");
        }
    }

    #[test]
    fn skips_bad_positions() {
        let g = bin_points(&[[f64::NAN, 0.0], [0.0, 89.0], [0.0, 0.0]], None, &opts(50.0, HexAggregation::Count));
        assert_eq!((g.skipped, g.points()), (2, 1));
    }

    /// Strictly inside a convex polygon (ring without the closing repeat),
    /// with a small inward tolerance so shared edges count for neither.
    fn strictly_inside(p: [f64; 2], ring: &[[f64; 2]; 6], eps: f64) -> bool {
        let mut sign = 0.0;
        for k in 0..6 {
            let a = ring[k];
            let b = ring[(k + 1) % 6];
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            let cross = ((b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])) / len;
            if cross.abs() <= eps {
                return false;
            }
            if sign == 0.0 {
                sign = cross.signum();
            } else if cross.signum() != sign {
                return false;
            }
        }
        true
    }

    fn inside_or_on(p: [f64; 2], ring: &[[f64; 2]; 6], eps: f64) -> bool {
        let mut pos = false;
        let mut neg = false;
        for k in 0..6 {
            let a = ring[k];
            let b = ring[(k + 1) % 6];
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            let cross = ((b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])) / len;
            if cross > eps {
                pos = true;
            }
            if cross < -eps {
                neg = true;
            }
        }
        !(pos && neg)
    }

    /// Every point lies in (or on) the hexagon of the bin it was assigned
    /// to, and strictly inside no other hexagon of the lattice: the
    /// hexagons cover the plane with no gap and no overlap, and the
    /// assignment agrees with the drawn shapes.
    #[test]
    fn hexagons_tile_the_plane_with_no_gaps_or_overlaps() {
        let r = 37.0;
        // A deterministic scatter (LCG), negative coordinates and odd rows included.
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..20_000 {
            let p = [(next() - 0.5) * 2000.0, (next() - 0.5) * 2000.0];
            let id = point_to_hexbin(p, r);
            let own = hexagon_vertices(hexbin_center(id, r), r);
            assert!(inside_or_on(p, &own, 1e-9), "{p:?} not in its own hexagon {id:?}");
            for dj in -2..=2 {
                for di in -2..=2 {
                    let other = (id.0 + di, id.1 + dj);
                    if other == id {
                        continue;
                    }
                    let hex = hexagon_vertices(hexbin_center(other, r), r);
                    assert!(!strictly_inside(p, &hex, 1e-9), "{p:?} in {id:?} and also inside {other:?}");
                }
            }
        }
        // The lattice's cell area is exactly a hexagon's: DIST_X·DIST_Y·r²
        // = 3√3/2·r², so equal-size cells leave no room for a gap.
        let hex_area = 1.5 * 3f64.sqrt() * r * r;
        assert!((DIST_X * DIST_Y * r * r - hex_area).abs() < 1e-9);
    }

    #[test]
    fn neighbouring_hexagons_share_their_edges() {
        let r = 10.0;
        let a = hexagon_vertices(hexbin_center((0, 0), r), r);
        for nb in [(1, 0), (-1, 0), (0, 1), (-1, 1), (0, -1), (-1, -1)] {
            let b = hexagon_vertices(hexbin_center(nb, r), r);
            let shared = a.iter().filter(|p| b.iter().any(|q| (p[0] - q[0]).abs() < 1e-9 && (p[1] - q[1]).abs() < 1e-9)).count();
            assert_eq!(shared, 2, "neighbour {nb:?} shares an edge");
        }
    }

    #[test]
    fn radius_is_ground_metres_at_the_reference_latitude() {
        let g = bin_points(&[[-122.45, 37.76]], None, &HexbinOptions { radius_m: 50.0, ..Default::default() });
        assert!((g.reference_lat - 37.76).abs() < 1e-12);
        assert!((g.radius_mercator * 37.76f64.to_radians().cos() - 50.0).abs() < 1e-9);
        let ring = g.hexagon(&g.bins[0], 1.0);
        assert_eq!(ring.len(), 7);
        assert_eq!(ring[0], ring[6]);
        // North vertex to south vertex: 100 m on the ground (to ~0.1%).
        let dlat = (ring[0].1 - ring[3].1).to_radians() * MERCATOR_RADIUS_M;
        assert!((dlat - 100.0).abs() < 0.1, "{dlat}");
    }

    #[test]
    fn percentile_window_hides_the_top_bins_like_deck() {
        let pts: Vec<[f64; 2]> = (0..10).flat_map(|k| std::iter::repeat_n(at(f64::from(k) * 1000.0, 0.0), k as usize + 1)).collect();
        let mut g = bin_points(&pts, None, &opts(100.0, HexAggregation::Count));
        assert_eq!(g.percentile_range(0.0, 100.0), Some([1.0, 10.0]));
        // floor(0.85 * 9) = 7 -> the 8th value, 8.
        assert_eq!(g.percentile_range(0.0, 85.0), Some([1.0, 8.0]));
        assert_eq!(g.retain_percentile(0.0, 85.0), 2);
        assert_eq!(g.bins.len(), 8);
    }

    #[test]
    fn geojson_and_features_carry_the_aggregate() {
        let g = bin_points(&[at(0.0, 0.0), at(1.0, 1.0)], None, &opts(100.0, HexAggregation::Count));
        let text = g.geojson(0.9, r#""k":1"#);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["features"][0]["properties"]["count"], 2);
        assert_eq!(v["features"][0]["properties"]["k"], 1);
        assert_eq!(v["features"][0]["geometry"]["coordinates"][0].as_array().unwrap().len(), 7);
        let f = g.features(1.0);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].1.get("count"), Some(&TagValue::Number(2.0)));
    }
}
