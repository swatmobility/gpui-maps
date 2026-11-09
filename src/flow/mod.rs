//! Origin-destination flows, drawn so they read: aggregation of trip rows
//! into corridors between places, Edge-Path Bundling of those corridors,
//! and the banding that turns a load into a handful of drawable groups.
//!
//! Renderer-agnostic: everything here takes and returns `[lon, lat]`
//! positions and numbers. A host draws the polylines (an overlay line
//! source, say) and the cells (overlay polygons); nothing here knows about
//! a map view.
//!
//! - [`od`] snaps both ends of every row onto one hexagonal lattice (the
//!   hexbin layer's, [`crate::overlay::hexbin`]), sums the weight per
//!   ordered cell pair, and indexes each cell's flows by direction.
//!   [`od::OdGrid::from_h3`] (the `h3` feature) snaps them to H3 cells at
//!   one resolution instead.
//! - [`places`] groups the ends into data-driven places instead (an id
//!   column's areas, else weight-ordered greedy clustering), and
//!   [`od::OdGrid::from_places`] sums the rows between them: a flow map's
//!   default nodes, with no lattice showing through the bundle.
//! - [`edge_path_bundling`] routes the heaviest corridors through each
//!   other (Wallinger et al. 2022).
//! - [`kde_bundling`] bundles them by kernel density instead (Hurter,
//!   Ersoy, Telea 2012): every corridor drifts onto the density of all the
//!   others, which suits a dense, local table that has few long edges to
//!   route through.
//! - [`log_ramp`] and [`band_runs`] put a load on a logarithmic `0..=1` and
//!   cut a polyline into runs of equal band, so a renderer that colours
//!   whole polylines can still brighten a strand where it joins a trunk.

pub mod edge_path_bundling;
pub mod kde_bundling;
pub mod od;
pub mod places;

/// Places `value` on `0..=1` against `peak`, logarithmically.
///
/// Travel demand is not merely skewed, it is orders-of-magnitude skewed:
/// the busiest cell pair carries thousands of journeys where the median
/// carries tens. A linear ramp paints everything but the top one or two
/// cells the same dark colour, and a square root only softens that -- at
/// 1% of peak it still returns 0.1, which is indistinguishable from
/// nothing. `log1p` gives the small flows a band of their own, which is
/// where the reading actually is.
#[must_use]
pub fn log_ramp(value: f64, peak: f64) -> f64 {
    if !(value > 0.0) || !(peak > 0.0) {
        return 0.0;
    }
    (value.min(peak).ln_1p() / peak.ln_1p()).clamp(0.0, 1.0)
}

/// Places `value` on `0..=1` between `lo` and `peak`, logarithmically:
/// `lo` (and below) is 0, `peak` 1. With `lo` at or under 1 it is
/// [`log_ramp`].
///
/// Why a floor: bundled loads do not start near zero. On the ONS commute
/// the dimmest of the top 2,000 corridors already carries ~900 people and
/// the London trunks millions, so `log1p` against the peak alone puts every
/// strand in the top half of the ramp and the picture reads as one colour.
/// Spanning the data's own range gives each class some strands.
#[must_use]
pub fn log_ramp_from(value: f64, lo: f64, peak: f64) -> f64 {
    if !(lo > 1.0) || !(peak > lo) {
        return log_ramp(value, peak);
    }
    if !(value > lo) {
        return 0.0;
    }
    ((value.min(peak) / lo).ln() / (peak / lo).ln()).clamp(0.0, 1.0)
}

/// The value at which [`log_ramp_from`] reaches `t` (the inverse): a
/// legend's class break.
#[must_use]
pub fn log_ramp_inverse(t: f64, lo: f64, peak: f64) -> f64 {
    if !(lo > 1.0) || !(peak > lo) {
        return (t * peak.max(0.0).ln_1p()).exp_m1();
    }
    lo * (peak / lo).powf(t)
}

/// The band of `bands` that `value` falls in between `lo` and `peak`,
/// through [`log_ramp_from`]: `0` is the dimmest, `bands - 1` the
/// brightest.
#[must_use]
pub fn load_band(value: f64, lo: f64, peak: f64, bands: usize) -> usize {
    let bands = bands.max(1);
    ((log_ramp_from(value, lo, peak) * bands as f64) as usize).min(bands - 1)
}

/// Cuts a polyline into runs of equal band: `(band, first, last)` with the
/// points `first..=last`. Runs overlap by one point, so the strand has no
/// gap where its brightness steps.
///
/// Banded rather than continuous because a renderer colours whole
/// polylines: a path is cut where its band changes, and each run joins the
/// group for its band.
#[must_use]
pub fn band_runs(load: &[f64], lo: f64, peak: f64, bands: usize) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    if load.len() < 2 {
        return out;
    }
    let mut start = 0usize;
    let mut band = load_band(load[0], lo, peak, bands);
    for (i, l) in load.iter().enumerate().skip(1) {
        let b = load_band(*l, lo, peak, bands);
        if b == band {
            continue;
        }
        out.push((band, start, i));
        start = i;
        band = b;
    }
    if start + 1 < load.len() {
        out.push((band, start, load.len() - 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_ramp_gives_small_flows_a_band_of_their_own() {
        assert_eq!(log_ramp(0.0, 1000.0), 0.0);
        assert_eq!(log_ramp(1000.0, 1000.0), 1.0);
        assert_eq!(log_ramp(5000.0, 1000.0), 1.0, "clamped at the peak");
        assert_eq!(log_ramp(f64::NAN, 1000.0), 0.0);
        // 1% of peak is a third of the way up, not a tenth.
        let one_percent = log_ramp(10.0, 1000.0);
        assert!(one_percent > 0.3, "{one_percent}");
        assert_eq!(load_band(1000.0, 0.0, 1000.0, 6), 5);
        assert_eq!(load_band(0.0, 0.0, 1000.0, 6), 0);
        // With a floor, the data's own range spans every band.
        assert_eq!(load_band(900.0, 900.0, 2.0e6, 6), 0);
        assert_eq!(load_band(2.0e6, 900.0, 2.0e6, 6), 5);
        assert!(log_ramp(900.0, 2.0e6) > 0.45, "without the floor the dimmest strand is half way up");
        for t in [0.0, 0.3, 1.0] {
            assert!((log_ramp_from(log_ramp_inverse(t, 900.0, 2.0e6), 900.0, 2.0e6) - t).abs() < 1e-9);
        }
    }

    #[test]
    fn runs_cover_the_line_and_overlap_where_the_band_steps() {
        let load = [1.0, 1.0, 1.0, 900.0, 900.0, 1.0, 1.0];
        let runs = band_runs(&load, 0.0, 1000.0, 4);
        assert_eq!(runs.first().map(|r| r.1), Some(0));
        assert_eq!(runs.last().map(|r| r.2), Some(6));
        for w in runs.windows(2) {
            assert_eq!(w[0].2, w[1].1, "runs share their boundary point");
            assert_ne!(w[0].0, w[1].0, "a run ends where its band changes");
        }
        assert_eq!(runs.len(), 3);
    }
}
