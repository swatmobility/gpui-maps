//! The income histogram: national and in-view bar counts, shared by the
//! native demo's legend (`demos/us_block_groups.rs`) and the browser's
//! `map_census_histograms_json` (`web/src/app/census_page.rs`).
//!
//! Bars sit on the slider's own stops, [`HIST_CLASSES_PER_BIN`] classes to
//! a bar, so every bar's edges are handle positions and a bar sits exactly
//! under the incomes it counts (`bin_span`, checked against
//! [`crate::census::ramp::slider_stops`] by `national_bins_line_up_with_slider_stops`).

use crate::census::hover::thousands;
use crate::census::ramp::{income_fraction, slider_stops, viridis_rgba};

/// Slider classes per histogram bar: the histograms bin on the slider's
/// own stops, three classes to a bar (249 = 3 x 83), so every bar's edges
/// are handle positions and a bar sits exactly under the incomes it counts.
pub const HIST_CLASSES_PER_BIN: usize = 3;

/// A histogram bin that means "no estimate" (not drawn).
pub const NO_BIN: u16 = u16::MAX;

/// The overall bars, muted: the same grey in and out of range, lighter out.
pub const HIST_OVERALL: u32 = 0xc4c9d1;
pub const HIST_OVERALL_OUT: u32 = 0xe8eaee;
/// Out-of-range in-view bars keep their hue at this alpha.
pub const HIST_OUT_ALPHA: f32 = 0.22;

pub fn hist_bins() -> usize {
    (slider_stops().len() - 1) / HIST_CLASSES_PER_BIN
}

/// The slider class `income` falls in -- `stops[c] <= income < stops[c+1]`,
/// the two end classes open outward exactly as `IncomeRange::contains` is.
pub fn income_class(income: f64) -> usize {
    let stops = slider_stops();
    stops.partition_point(|&s| s <= income).saturating_sub(1).min(stops.len() - 2)
}

/// The histogram bar `income` is counted in.
pub fn income_bin(income: f64) -> usize {
    income_class(income) / HIST_CLASSES_PER_BIN
}

/// The incomes bar `k` spans: `(stops[3k], stops[3k + 3])`.
pub fn bin_span(k: usize) -> (f64, f64) {
    let stops = slider_stops();
    (stops[k * HIST_CLASSES_PER_BIN], stops[(k + 1) * HIST_CLASSES_PER_BIN])
}

/// The viridis colour of `income`, as `#rrggbb`.
pub fn viridis_hex(income: f64) -> String {
    let [r, g, b, _] = viridis_rgba(income);
    let c = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
    format!("#{:02x}{:02x}{:02x}", c(r), c(g), c(b))
}

/// Block-group counts per histogram bar. No data is counted apart.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Histogram {
    pub counts: Vec<u32>,
    /// Block groups with an income (the sum of `counts`).
    pub total: u32,
    pub no_data: u32,
}

impl Histogram {
    pub fn of_bins(bins: impl Iterator<Item = u16>) -> Self {
        let mut h = Self { counts: vec![0; hist_bins()], ..Self::default() };
        for bin in bins {
            if bin == NO_BIN {
                h.no_data += 1;
            } else {
                h.counts[usize::from(bin)] += 1;
                h.total += 1;
            }
        }
        h
    }

    pub fn of_incomes(incomes: impl Iterator<Item = Option<f64>>) -> Self {
        Self::of_bins(incomes.map(|i| i.map_or(NO_BIN, |i| income_bin(i) as u16)))
    }

    pub fn peak(&self) -> u32 {
        self.counts.iter().copied().max().unwrap_or(0)
    }
}

/// The line under the ticks when nothing is hovered: which colour is
/// which, their totals, and how they are scaled. (`hist_caption`'s hover
/// branch stays native/UI-only -- it needs a swatch element, not text.)
pub fn caption(overall: &Histogram, in_view: Option<&Histogram>) -> String {
    let in_view_total = in_view.map_or_else(|| "…".to_string(), |h| thousands(u64::from(h.total)));
    format!("in view {in_view_total} · all {} · each to its own peak", thousands(u64::from(overall.total)))
}

/// `map_census_histograms_json`'s body: the national bins, the in-view
/// bins (`None` before the first count over the camera's view -> JSON
/// `null`), each histogram's own peak (`max` and `in_view_max`: every bar
/// scales to its own peak, and JS is never asked to reduce a bin array
/// itself), the default caption, each bar's `[lo_frac, hi_frac]` along the
/// ramp bar, and the paint colours -- `HIST_OVERALL`, `HIST_OVERALL_OUT`,
/// `HIST_OUT_ALPHA` and the in-view viridis colour of each bin's midpoint
/// income -- JS has no other way to compute.
pub fn histograms_json(overall: &Histogram, in_view: Option<&Histogram>) -> String {
    let bin_fracs: Vec<[f64; 2]> = (0..hist_bins())
        .map(|k| {
            let (a, b) = bin_span(k);
            [income_fraction(a), income_fraction(b)]
        })
        .collect();
    let in_view_colors: Vec<String> = (0..hist_bins())
        .map(|k| {
            let (a, b) = bin_span(k);
            viridis_hex((a + b) / 2.0)
        })
        .collect();
    serde_json::json!({
        "bins": overall.counts,
        "in_view": in_view.map(|h| h.counts.clone()),
        "max": overall.peak(),
        "in_view_max": in_view.map(Histogram::peak),
        "caption": caption(overall, in_view),
        "bin_fracs": bin_fracs,
        "colors": {
            "overall": format!("#{HIST_OVERALL:06x}"),
            "overall_out": format!("#{HIST_OVERALL_OUT:06x}"),
            "out_alpha": HIST_OUT_ALPHA,
            "in_view": in_view_colors,
        },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn national_bins_line_up_with_slider_stops() {
        for k in 0..hist_bins() {
            let (a, _) = bin_span(k);
            assert_eq!(income_bin(a), k);
        }
    }

    #[test]
    fn a_histogram_counts_every_income_once_and_no_data_apart() {
        let incomes = [Some(2_500.0), Some(2_600.0), Some(250_000.0), None, Some(126_250.0), Some(1e9)];
        let h = Histogram::of_incomes(incomes.into_iter());
        assert_eq!((h.total, h.no_data), (5, 1));
        assert_eq!(h.counts.iter().sum::<u32>(), 5);
        assert_eq!(h.counts[0], 2);
        assert_eq!(h.counts[hist_bins() - 1], 2);
        assert_eq!(h.peak(), 2);
    }

    #[test]
    fn the_json_is_null_before_the_first_settle_and_carries_every_field() {
        let overall = Histogram::of_incomes([Some(2_500.0), Some(126_250.0), None].into_iter());
        let v: serde_json::Value = serde_json::from_str(&histograms_json(&overall, None)).unwrap();
        assert!(v["in_view"].is_null());
        assert!(v["in_view_max"].is_null(), "no in-view peak without an in-view histogram either");
        assert_eq!(v["bins"].as_array().unwrap().len(), hist_bins());
        assert_eq!(v["bin_fracs"].as_array().unwrap().len(), hist_bins());
        assert_eq!(v["colors"]["in_view"].as_array().unwrap().len(), hist_bins());
        assert_eq!(v["colors"]["overall"], "#c4c9d1");
        assert_eq!(v["max"], 1);
        let in_view = Histogram::of_incomes([Some(2_500.0), Some(2_500.0), Some(126_250.0)].into_iter());
        let v2: serde_json::Value = serde_json::from_str(&histograms_json(&overall, Some(&in_view))).unwrap();
        assert_eq!(v2["in_view"].as_array().unwrap().len(), hist_bins());
        // The in-view peak is its OWN peak (2, the doubled-up bin), not the
        // overall histogram's (1) -- the DOM must never be able to confuse
        // the two, or compute either from the bin array itself.
        assert_eq!(v2["in_view_max"], 2);
        assert!(v2["caption"].as_str().unwrap().contains("in view 3"));
    }
}
