//! A document read as a dashboard: whether it opens as one, and how its
//! time series bucket. Pure, shared by every shell (desktop, phone,
//! browser) so they agree on what a dashboard document is and how its
//! charts read.
//!
//! - [`opens_as_dashboard`]: a document opens as its dashboard when its
//!   `profile` (or the hosted map's `kind`) says so, or when it has charts
//!   and no layer draws a source with places. A charts-only document
//!   (national monthly figures, daily cash flows, page views) has no map.
//! - [`Grain`]: a time series is drawn in buckets of the grain the data
//!   has (monthly figures stay monthly), coarser when the span is long (a
//!   year of days by week), as the web viewer draws it.

use crate::doc::Doc;

/// Whether `doc` opens as its dashboard rather than a map. `kind` is the
/// hosted map's kind when there is one (`"dashboard"`); `has_places`
/// answers whether a source's loaded rows have geometry.
pub fn opens_as_dashboard(doc: &Doc, kind: Option<&str>, has_places: impl Fn(&str) -> bool) -> bool {
    if doc.profile == "dashboard" || kind == Some("dashboard") {
        return true;
    }
    if doc.charts.is_empty() {
        return false;
    }
    !doc.map.layers.iter().any(|l| has_places(&l.source))
}

const DAY_MS: f64 = 86_400_000.0;
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// The most buckets a time series draws; a longer run takes the next
/// coarser grain.
pub const MAX_BUCKETS: usize = 160;

/// A time series bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grain {
    Hour,
    Day,
    /// Weeks start on Monday (UTC).
    Week,
    Month,
    Year,
}

impl Grain {
    fn coarser(self) -> Option<Self> {
        Some(match self {
            Self::Hour => Self::Day,
            Self::Day => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::Year,
            Self::Year => return None,
        })
    }

    /// `hour` | `day` | `week` | `month` | `year`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// The grain `times` (epoch milliseconds) are drawn in: the coarser of the
/// data's own step and what the span fits.
pub fn grain_for(times: &[f64]) -> Grain {
    let mut sorted: Vec<f64> = times.iter().copied().filter(|t| t.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    sorted.dedup();
    let (Some(&lo), Some(&hi)) = (sorted.first(), sorted.last()) else { return Grain::Day };
    let step = sorted.windows(2).map(|w| w[1] - w[0]).fold(f64::INFINITY, f64::min);
    let by_step = if step >= 360.0 * DAY_MS {
        Grain::Year
    } else if step >= 27.0 * DAY_MS {
        Grain::Month
    } else if step >= 7.0 * DAY_MS {
        Grain::Week
    } else if step >= DAY_MS {
        Grain::Day
    } else {
        Grain::Hour
    };
    let span = hi - lo;
    let by_span = if span > 3.0 * 365.0 * DAY_MS {
        Grain::Month
    } else if span > 90.0 * DAY_MS {
        Grain::Week
    } else if span > 3.0 * DAY_MS {
        Grain::Day
    } else {
        Grain::Hour
    };
    let mut g = by_step.max(by_span);
    while bucket_count(lo, hi, g) > MAX_BUCKETS {
        match g.coarser() {
            Some(c) => g = c,
            None => break,
        }
    }
    g
}

fn bucket_count(lo: f64, hi: f64, g: Grain) -> usize {
    let mut n = 0;
    let mut t = bucket_start(lo, g);
    while t <= hi && n <= MAX_BUCKETS {
        t = bucket_next(t, g);
        n += 1;
    }
    n
}

/// `y-m-d` of a day number from 1970-01-01 (Hinnant's `civil_from_days`).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days from 1970-01-01 to `y-m-d` (Hinnant's `days_from_civil`).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The start of `t`'s bucket (UTC; weeks start on Monday).
pub fn bucket_start(t: f64, g: Grain) -> f64 {
    let day = (t / DAY_MS).floor();
    match g {
        Grain::Hour => (t / 3_600_000.0).floor() * 3_600_000.0,
        Grain::Day => day * DAY_MS,
        // 1970-01-01 was a Thursday: Monday is three days before.
        Grain::Week => (((day as i64 + 3).div_euclid(7)) * 7 - 3) as f64 * DAY_MS,
        Grain::Month => {
            let (y, m, _) = civil_from_days(day as i64);
            days_from_civil(y, m, 1) as f64 * DAY_MS
        }
        Grain::Year => {
            let (y, _, _) = civil_from_days(day as i64);
            days_from_civil(y, 1, 1) as f64 * DAY_MS
        }
    }
}

/// The start of the bucket after the one starting at `start`.
pub fn bucket_next(start: f64, g: Grain) -> f64 {
    match g {
        Grain::Hour => start + 3_600_000.0,
        Grain::Day => start + DAY_MS,
        Grain::Week => start + 7.0 * DAY_MS,
        Grain::Month => {
            let (y, m, _) = civil_from_days((start / DAY_MS).floor() as i64);
            let (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            days_from_civil(y, m, 1) as f64 * DAY_MS
        }
        Grain::Year => {
            let (y, _, _) = civil_from_days((start / DAY_MS).floor() as i64);
            days_from_civil(y + 1, 1, 1) as f64 * DAY_MS
        }
    }
}

/// The bucket edges covering `lo..=hi`: the first bucket's start to the
/// last one's end, one more edge than buckets.
pub fn bucket_edges(lo: f64, hi: f64, g: Grain) -> Vec<f64> {
    let mut out = vec![bucket_start(lo, g)];
    while out.len() <= MAX_BUCKETS * 2 {
        let next = bucket_next(*out.last().unwrap_or(&lo), g);
        out.push(next);
        if next > hi {
            break;
        }
    }
    out
}

/// A bucket's label: "Jan 2015" by month, "2015" by year, "Sep 29" by
/// week or day, "14:00" by hour.
pub fn bucket_label(start: f64, g: Grain) -> String {
    let (y, m, d) = civil_from_days((start / DAY_MS).floor() as i64);
    let mon = MONTHS[(m as usize).saturating_sub(1).min(11)];
    match g {
        Grain::Year => format!("{y}"),
        Grain::Month => format!("{mon} {y}"),
        Grain::Hour => format!("{:02}:00", (start.rem_euclid(DAY_MS) / 3_600_000.0) as u32),
        Grain::Day | Grain::Week => format!("{mon} {d}"),
    }
}

/// A bucket's label on the axis of a series running `lo..=hi`: as
/// [`bucket_label`], with the year written into a day or week label
/// ("Sep 29, 2025") when the series crosses a calendar year, so a
/// multi-year axis says which year each date is in. Month and year
/// labels carry it already.
pub fn axis_label(start: f64, g: Grain, lo: f64, hi: f64) -> String {
    let year = |t: f64| civil_from_days((t / DAY_MS).floor() as i64).0;
    let multi_year = lo.is_finite() && hi.is_finite() && year(lo) != year(hi);
    match g {
        Grain::Day | Grain::Week if multi_year => format!("{}, {}", bucket_label(start, g), year(start)),
        _ => bucket_label(start, g),
    }
}

/// A date as a range line writes it: "Oct 1, 2025".
pub fn date_label(t: f64) -> String {
    let (y, m, d) = civil_from_days((t / DAY_MS).floor() as i64);
    format!("{} {d}, {y}", MONTHS[(m as usize).saturating_sub(1).min(11)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Chart, ChartKind, Layer};

    fn ms(y: i64, m: u32, d: u32) -> f64 {
        days_from_civil(y, m, d) as f64 * DAY_MS
    }

    #[test]
    fn a_dashboard_profile_or_a_charts_only_document_opens_as_the_dashboard() {
        let mut doc = Doc { profile: "dashboard".into(), ..Doc::default() };
        assert!(opens_as_dashboard(&doc, None, |_| false));
        doc.profile = "map".into();
        assert!(!opens_as_dashboard(&doc, None, |_| false), "nothing at all: the map");
        assert!(opens_as_dashboard(&doc, Some("dashboard"), |_| false), "the hosted kind says so");
        doc.charts.push(Chart::new("c", "s", ChartKind::Bar, "category"));
        assert!(opens_as_dashboard(&doc, None, |_| false), "charts and no layer");
        doc.map.layers.push(serde_json::from_value::<Layer>(serde_json::json!({"id": "l", "type": "points", "source": "s"})).expect("a layer"));
        assert!(opens_as_dashboard(&doc, None, |_| false), "a layer over a place-less table draws nothing");
        assert!(!opens_as_dashboard(&doc, None, |s| s == "s"), "a layer with places keeps the map");
    }

    #[test]
    fn monthly_figures_stay_monthly_and_a_year_of_days_goes_by_week() {
        let monthly: Vec<f64> = (0..140).map(|k| ms(2015 + k / 12, (k % 12) as u32 + 1, 1)).collect();
        assert_eq!(grain_for(&monthly), Grain::Month);
        let edges = bucket_edges(monthly[0], monthly[139], Grain::Month);
        assert_eq!(edges.len(), 141, "140 buckets");
        assert_eq!(bucket_label(edges[0], Grain::Month), "Jan 2015");
        assert_eq!(bucket_label(edges[139], Grain::Month), "Aug 2026");
        let jan = ms(2025, 10, 1);
        let year: Vec<f64> = (0..365).map(|k| jan + k as f64 * DAY_MS).collect();
        assert_eq!(grain_for(&year), Grain::Week);
        assert_eq!(grain_for(&year[..30]), Grain::Day);
        // 2025-10-01 (a Wednesday) falls in the week of Monday Sep 29.
        assert_eq!(bucket_label(bucket_start(jan, Grain::Week), Grain::Week), "Sep 29");
        assert_eq!(bucket_label(bucket_next(bucket_start(ms(2015, 12, 17), Grain::Month), Grain::Month), Grain::Month), "Jan 2016");
        assert_eq!(date_label(ms(2026, 9, 30)), "Sep 30, 2026");
    }

    #[test]
    fn a_multi_year_axis_writes_the_year_into_day_and_week_labels() {
        let (lo, hi) = (ms(2024, 9, 1), ms(2025, 3, 1));
        assert_eq!(axis_label(bucket_start(ms(2024, 10, 1), Grain::Week), Grain::Week, lo, hi), "Sep 30, 2024");
        assert_eq!(axis_label(ms(2025, 1, 5), Grain::Day, lo, hi), "Jan 5, 2025");
        assert_eq!(axis_label(ms(2025, 1, 1), Grain::Month, lo, hi), "Jan 2025", "a month says its year already");
        // Within one year the short form stays.
        assert_eq!(axis_label(ms(2025, 1, 5), Grain::Day, ms(2025, 1, 1), ms(2025, 6, 1)), "Jan 5");
    }

    #[test]
    fn a_long_run_coarsens_to_fit() {
        let monthly: Vec<f64> = (0..600).map(|k| ms(1970 + k / 12, (k % 12) as u32 + 1, 1)).collect();
        assert_eq!(grain_for(&monthly), Grain::Year);
        // Minutes over two years: weeks (105 buckets), not 1M minutes.
        let minutes: Vec<f64> = (0..2000).map(|k| ms(2009, 12, 1) + k as f64 * 60_000.0 * 523.0).collect();
        assert_eq!(grain_for(&minutes), Grain::Week);
    }
}

/// Whether `doc` draws data fills under the basemap labels, so every shell
/// draws those labels in the on-data paint (dark text in an opaque light
/// halo, `gpui_maps::style::LabelPaint::ON_DATA`) instead of the
/// basemap's own: a style's label paint is chosen for its own ground, and
/// over a choropleth's bright palette colours a dark basemap's light text
/// in a dark halo turns to mud, while over its dark ones a light
/// basemap's grey italics vanish.
///
/// A visible layer that covers area counts: a filled GeoJSON layer (a
/// choropleth), hexagon and square bins, H3 cells and the hex demand map.
/// Points, arcs, lines, clusters, trips, flows and the heatmap glow leave
/// the basemap's ground showing and keep its paint.
pub fn labels_on_data(doc: &Doc) -> bool {
    use crate::doc::LayerType;
    doc.map.layers.iter().any(|layer| {
        layer.visible
            && match layer.kind {
                LayerType::GeoJson => layer.vis.filled,
                LayerType::Hexbin | LayerType::Grid | LayerType::H3 | LayerType::HexDemand => true,
                _ => false,
            }
    })
}

/// The basemap a hosted map is drawn on in the system's appearance: a
/// document on a default light basemap (`streets`, `light`, none) is drawn
/// on Graphite in dark appearance, so the page stays dark; any other name
/// is the document pinning its basemap, drawn as named in both.
pub fn basemap_for_appearance(name: &str, dark: bool) -> &str {
    if dark && matches!(name, "" | "streets" | "light") {
        "graphite"
    } else {
        name
    }
}

/// [`basemap_for_appearance`] for a document's view: a basemap the reader
/// chose ([`crate::doc::View::basemap_pinned`]) is drawn as named in both
/// appearances, a default light one on Graphite in dark. The one rule
/// every hosted map (a cloud one, the Explorer's own) follows.
pub fn view_basemap_for_appearance(view: &crate::doc::View, dark: bool) -> &str {
    if view.basemap_pinned() {
        &view.basemap
    } else {
        basemap_for_appearance(&view.basemap, dark)
    }
}

#[cfg(test)]
mod appearance_tests {
    use super::{basemap_for_appearance, view_basemap_for_appearance};

    fn doc_with(layers: &[(crate::doc::LayerType, bool, bool)]) -> crate::doc::Doc {
        let mut doc: crate::doc::Doc = serde_json::from_value(serde_json::json!({"mapdoc": 1, "profile": "map"})).expect("a doc");
        for (k, (kind, visible, filled)) in layers.iter().enumerate() {
            let mut layer: crate::doc::Layer = serde_json::from_value(serde_json::json!({"id": format!("l{k}"), "type": "points", "source": "s"})).expect("a layer");
            layer.kind = *kind;
            layer.visible = *visible;
            layer.vis.filled = *filled;
            doc.map.layers.push(layer);
        }
        doc
    }

    #[test]
    fn a_fill_under_the_labels_puts_them_in_the_on_data_paint() {
        use crate::doc::LayerType::*;
        // A census map: a filled choropleth and its circles.
        assert!(super::labels_on_data(&doc_with(&[(GeoJson, true, true), (Points, true, true)])));
        for area in [Hexbin, Grid, H3, HexDemand] {
            assert!(super::labels_on_data(&doc_with(&[(area, true, true)])), "{area:?}");
        }
        // Point-only (the Tokyo stations map), lines, arcs: the basemap's own.
        assert!(!super::labels_on_data(&doc_with(&[(Points, true, true)])));
        assert!(!super::labels_on_data(&doc_with(&[(Points, true, true), (Arc, true, true), (Line, true, true), (Heatmap, true, true)])));
        assert!(!super::labels_on_data(&doc_with(&[])));
        // An outline-only GeoJSON layer, or a hidden choropleth, covers nothing.
        assert!(!super::labels_on_data(&doc_with(&[(GeoJson, true, false)])));
        assert!(!super::labels_on_data(&doc_with(&[(GeoJson, false, true), (Points, true, true)])));
    }

    #[test]
    fn dark_appearance_draws_a_default_light_map_on_graphite() {
        for name in ["streets", "light", ""] {
            assert_eq!(basemap_for_appearance(name, true), "graphite", "{name:?}");
            assert_eq!(basemap_for_appearance(name, false), name, "{name:?} stays light in light appearance");
        }
        assert_eq!(basemap_for_appearance("dark", false), "dark", "pinned");
        assert_eq!(basemap_for_appearance("imagery", true), "imagery", "pinned");
    }

    #[test]
    fn a_basemap_the_reader_chose_stays_in_dark_appearance() {
        let mut view = crate::doc::View::default();
        assert_eq!(view_basemap_for_appearance(&view, true), "graphite", "the default follows the appearance");
        assert_eq!(view_basemap_for_appearance(&view, false), "streets");
        view.pin_basemap("streets");
        assert!(view.basemap_pinned());
        assert_eq!(view_basemap_for_appearance(&view, true), "streets", "the reader chose streets");
        let back: crate::doc::View = serde_json::from_value(serde_json::to_value(&view).expect("writes")).expect("reads");
        assert!(back.basemap_pinned(), "the pin is saved with the document");
    }
}
