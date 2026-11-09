//! The Breakdowns panel's words and JSON: what the native panel
//! (`demos/us_block_groups/panel.rs`) prints and what the browser's
//! `web/census.js` draws, from [`Breakdowns`] and the slider's
//! [`IncomeRange`]. The browser shell cannot be tested, so every number,
//! label, order and colour its panel shows is decided here; `census.js`
//! only lays the JSON out in pixels.
//!
//! - [`states_json`]: the States tab -- one row per state in the chosen
//!   sort, Puerto Rico apart at the bottom, each with its income strip and
//!   its live share in range.
//! - [`metros_json`]: the Metros tab -- the [`TOP_METROS`] largest metros'
//!   ridges on the slider's axis, by median, with the Gini and its label.
//! - [`in_view_json`]: the In view tab -- the states and metros with the
//!   most people in view.
//!
//! A drag calls the first two once a move: each is a
//! [`crate::census::breakdowns::Group::share_in_range`] a group (two binary
//! searches) plus formatting, never a walk over the block groups.

use super::breakdowns::{default_edges, Breakdowns, Group, InView, InViewGroup, GINI_LABEL, TOP_METROS};
use super::hist::viridis_hex;
use super::hover::thousands;
use super::ramp::{income_fraction, IncomeRange};
use super::slider::knob_frac;
use serde_json::{json, Value};

/// The States tab's sort buttons: code and label.
pub const SORTS: [(u32, &str); 3] = [(SORT_MEDIAN, "median"), (SORT_SHARE, "in range"), (SORT_POPULATION, "population")];

/// `states_json`'s sorts, as the page passes them.
pub const SORT_MEDIAN: u32 = 0;
pub const SORT_SHARE: u32 = 1;
pub const SORT_POPULATION: u32 = 2;

/// Puerto Rico's FIPS: listed, but apart.
pub const PR: u32 = 72;

/// How many states and metros the In view tab lists, each.
pub const IN_VIEW_GROUPS: usize = 8;

/// How far a ridge rises into the rows above it, in rows: the ridgeline's
/// tallest bar is `1 + RIDGE_OVERLAP` row steps tall.
pub const RIDGE_OVERLAP: f64 = 1.4;

/// The ridgeline's row step target, in px: the plot is
/// `RIDGE_STEP_PX * (ridges + RIDGE_OVERLAP)` tall.
pub const RIDGE_STEP_PX: f64 = 18.0;

/// Where the ridgeline's rows sit: ridge `i`'s baseline is
/// `amplitude + i * step`, the tallest bar anywhere is `amplitude` tall
/// (`1 + overlap` steps, so it rises into the rows above), the first
/// ridge's peak can reach y = 0 and the last baseline is `height`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RidgeScale {
    pub step: f32,
    pub amplitude: f32,
}

impl RidgeScale {
    pub fn new(ridges: usize, height: f32, overlap: f32) -> Self {
        let n = ridges.max(1) as f32;
        let step = height / (n - 1.0 + 1.0 + overlap);
        Self { step, amplitude: step * (1.0 + overlap) }
    }

    pub fn baseline(&self, i: usize) -> f32 {
        self.amplitude + i as f32 * self.step
    }

    /// Ridge `i`'s outline over a `width`-wide axis: `(x, y)` from the
    /// baseline at x = 0, through each bar's centre at `bins[k] / peak` of
    /// the amplitude, back to the baseline at x = width. `peak` is the
    /// tallest bar over ALL ridges, so their areas compare.
    pub fn ridge(&self, i: usize, bins: &[f32], peak: f32, width: f32) -> Vec<(f32, f32)> {
        let base = self.baseline(i);
        let n = bins.len().max(1) as f32;
        let mut pts = Vec::with_capacity(bins.len() + 2);
        pts.push((0.0, base));
        for (k, &b) in bins.iter().enumerate() {
            let h = if peak > 0.0 { (b / peak).clamp(0.0, 1.0) * self.amplitude } else { 0.0 };
            pts.push(((k as f32 + 0.5) / n * width, base - h));
        }
        pts.push((width, base));
        pts
    }
}

/// The tallest bar across `groups`' distributions.
pub fn common_peak<'a>(groups: impl Iterator<Item = &'a [f32]>) -> f32 {
    groups.flat_map(|b| b.iter().copied()).fold(0.0, f32::max)
}



#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateSort {
    Median,
    Share,
    Population,
}

impl StateSort {
    /// [`SORT_MEDIAN`], [`SORT_SHARE`] or [`SORT_POPULATION`]; anything
    /// else is the default, by median.
    pub fn from_code(code: u32) -> Self {
        match code {
            SORT_SHARE => Self::Share,
            SORT_POPULATION => Self::Population,
            _ => Self::Median,
        }
    }

    pub fn code(self) -> u32 {
        match self {
            Self::Median => SORT_MEDIAN,
            Self::Share => SORT_SHARE,
            Self::Population => SORT_POPULATION,
        }
    }
}

/// `98_843.0` -> `"$98.8K"`, `141_477.0` -> `"$141K"`, `950.0` -> `"$950"`;
/// the legend's capital K. NaN (an empty group) -> `"–"`.
pub fn money(x: f64) -> String {
    if !x.is_finite() {
        return "–".to_string();
    }
    let x = x.max(0.0);
    if x < 1_000.0 {
        format!("${:.0}", x)
    } else if x < 99_950.0 {
        format!("${:.1}K", x / 1_000.0)
    } else {
        format!("${:.0}K", x / 1_000.0)
    }
}

/// A share `0..=1` as a percent that never claims "0%" or "100%" when it
/// is not: `"<1%"`, `">99%"` at the edges.
pub fn percent(share: f64) -> String {
    if !share.is_finite() || share <= 0.0 {
        "0%".to_string()
    } else if share >= 1.0 {
        "100%".to_string()
    } else if share < 0.01 {
        "<1%".to_string()
    } else if share > 0.99 {
        ">99%".to_string()
    } else {
        format!("{:.0}%", share * 100.0)
    }
}

/// `39_242_785.0` -> `"39M"`, `672_079.0` -> `"672K"`, `9_870.0` -> `"9.9K"`.
pub fn people(p: f64) -> String {
    let p = p.max(0.0);
    if p >= 9_995_000.0 {
        format!("{:.0}M", p / 1e6)
    } else if p >= 999_500.0 {
        format!("{:.1}M", p / 1e6)
    } else if p >= 9_995.0 {
        format!("{:.0}K", p / 1e3)
    } else if p >= 1_000.0 {
        format!("{:.1}K", p / 1e3)
    } else {
        format!("{:.0}", p)
    }
}

/// A CBSA title's first city and first state: `"San Francisco-Oakland-
/// Fremont, CA"` -> `"San Francisco, CA"`, `"St. Louis, MO-IL"` ->
/// `"St. Louis, MO"`.
pub fn short_metro(title: &str) -> String {
    let (cities, states) = title.split_once(", ").unwrap_or((title, ""));
    let city = cities.split('-').next().unwrap_or(cities).trim();
    let state = states.split('-').next().unwrap_or("").trim();
    if state.is_empty() {
        city.to_string()
    } else {
        format!("{city}, {state}")
    }
}

/// The states' display order: indices into `states`, highest first by
/// `by` (`shares[i]` is state i's share in range), ties by FIPS; Puerto
/// Rico returned apart, for the bottom.
pub fn sort_states(states: &[Group], shares: &[f64], by: StateSort) -> (Vec<usize>, Vec<usize>) {
    let key = |i: usize| match by {
        StateSort::Median => states[i].stats.median,
        StateSort::Share => shares.get(i).copied().unwrap_or(0.0),
        StateSort::Population => states[i].population_total,
    };
    let (mut main, apart): (Vec<usize>, Vec<usize>) = (0..states.len()).partition(|&i| states[i].code != PR);
    main.sort_by(|&a, &b| {
        let (ka, kb) = (key(a), key(b));
        // NaN (an empty group) sorts last.
        kb.is_nan().cmp(&ka.is_nan()).reverse().then(kb.total_cmp(&ka)).then(states[a].code.cmp(&states[b].code))
    });
    (main, apart)
}

/// The slider's selection along the income axis (the strips' and the
/// ridgeline's x), as the slider's own knobs sit.
fn range_fracs(range: IncomeRange) -> (f64, f64) {
    (knob_frac(range.lo), knob_frac(range.hi))
}

/// Each distribution bar's `[x0, x1]` along the income axis.
fn bar_fracs() -> Vec<[f64; 2]> {
    default_edges().edges().windows(2).map(|e| [income_fraction(e[0]), income_fraction(e[1])]).collect()
}

/// A compact income strip: per bar, its height against the group's own
/// tallest bar (`0..=1`) and its viridis colour (the bar's middle income).
fn strip(bins: &[f32]) -> Vec<Value> {
    let peak = bins.iter().copied().fold(0.0f32, f32::max);
    let e = default_edges().edges();
    bins.iter()
        .enumerate()
        .map(|(k, &b)| {
            let h = if peak > 0.0 { f64::from((b / peak).clamp(0.0, 1.0)) } else { 0.0 };
            let mid = e.get(k + 1).map_or(e[k], |hi| (e[k] + hi) / 2.0);
            json!({ "h": h, "color": viridis_hex(mid) })
        })
        .collect()
}

/// A clickable row's tooltip: what a click does, or -- before the group's
/// bounds exist (they are set once the pack's states are in) -- that it
/// cannot fly yet.
fn fly_title(g: &Group, people_text: &str) -> String {
    if g.bounds.is_some() {
        format!("{} · {people_text} people · click to fly there", g.name)
    } else {
        format!("{} · {people_text} people · still loading, fly-to not ready", g.name)
    }
}

/// Where a median sits along the axis; `null` for an empty group.
fn median_frac(median: f64) -> Value {
    if median.is_finite() { json!(income_fraction(median)) } else { Value::Null }
}

/// The states' footnote lines (the native panel's "About these numbers").
fn footnote(b: &Breakdowns) -> Vec<String> {
    let (excluded_bg, excluded_people) = b.excluded();
    let (weighted_bg, weighted_people) = b.weighted();
    vec![
        "Median: the population-weighted median of block-group median household incomes (ACS 2023 5-year) \
         -- the income of the block group the middle resident lives in, not the Census's own household median."
            .to_string(),
        "Gini: inequality BETWEEN block groups' medians, population-weighted -- not a household Gini, \
         which also counts the spread inside each block group and runs far higher."
            .to_string(),
        format!(
            "Weighted: {} block groups, {} people. Excluded for a missing income or population: {} block groups ({} people).",
            thousands(u64::from(weighted_bg)),
            people(weighted_people),
            thousands(u64::from(excluded_bg)),
            people(excluded_people),
        ),
    ]
}

/// The States tab: `{"sort", "full", "lo_frac", "hi_frac", "bar_fracs",
/// "rows": [{"fips", "postal", "name", "median", "median_text",
/// "median_frac", "share", "share_text", "population", "population_text",
/// "strip": [{"h", "color"}], "apart", "can_fly", "title"}], "sorts":
/// [{"code", "label"}], "sort_label", "columns", "footnote": [..]}`. The rows are
/// in `sort`'s order ([`sort_states`]), Puerto Rico last with `apart`.
pub fn states_json(b: &Breakdowns, range: IncomeRange, sort: u32) -> String {
    let by = StateSort::from_code(sort);
    let (lo, hi) = range.bounds();
    let shares = b.state_shares(lo, hi);
    let (main, apart) = sort_states(&b.states, &shares, by);
    let row = |i: usize, apart: bool| {
        let g = &b.states[i];
        let share = shares[i].clamp(0.0, 1.0);
        json!({
            "fips": g.code,
            "postal": g.abbrev,
            "name": g.name,
            "median": if g.stats.median.is_finite() { json!(g.stats.median) } else { Value::Null },
            "median_text": money(g.stats.median),
            "median_frac": median_frac(g.stats.median),
            "share": share,
            "share_text": percent(share),
            "population": g.population_total,
            "population_text": people(g.population_total),
            "strip": strip(&g.stats.bins),
            "apart": apart,
            "can_fly": g.bounds.is_some(),
            "title": fly_title(g, &people(g.population_total)),
        })
    };
    let rows: Vec<Value> = main.iter().map(|&i| row(i, false)).chain(apart.iter().map(|&i| row(i, true))).collect();
    let (lo_frac, hi_frac) = range_fracs(range);
    json!({
        "sort": by.code(),
        "sorts": SORTS.iter().map(|&(code, label)| json!({ "code": code, "label": label })).collect::<Vec<_>>(),
        "sort_label": "Sort",
        "columns": ["", "income distribution", "median", "people in range", ""],
        "full": range.is_full(),
        "lo_frac": lo_frac,
        "hi_frac": hi_frac,
        "bar_fracs": bar_fracs(),
        "rows": rows,
        "footnote": footnote(b),
    })
    .to_string()
}

/// The Metros tab: the [`TOP_METROS`] largest metros by median, highest
/// first. `{"gini_label", "key", "full", "lo_frac", "hi_frac", "overlap",
/// "axis": [[label, frac]], "metros": [{"code", "title", "full_title",
/// "median", "median_text", "gini", "gini_text", "color", "baseline",
/// "points", "share", "share_text"}], "height_px"}`. The layout is
/// [`RidgeScale`]'s over a unit plot: each ridge's `baseline` and its
/// outline `points` `[x, y]` are fractions of the plot's width and height
/// (`height_px` tall), bars against the tallest bar over ALL the ridges so
/// their areas compare.
pub fn metros_json(b: &Breakdowns, range: IncomeRange) -> String {
    let top = b.top_metros(TOP_METROS);
    let mut order: Vec<usize> = (0..top.len()).collect();
    order.sort_by(|&x, &y| top[y].stats.median.total_cmp(&top[x].stats.median).then(top[x].code.cmp(&top[y].code)));
    let peak = common_peak(top.iter().map(|g| g.stats.bins.as_slice()));
    // Fractions of the plot: x of its width, y of its height.
    let scale = RidgeScale::new(order.len(), 1.0, RIDGE_OVERLAP as f32);
    let (lo, hi) = range.bounds();
    let metros: Vec<Value> = order
        .iter()
        .enumerate()
        .map(|(row, &i)| {
            let g = &top[i];
            let share = g.share_in_range(lo, hi).clamp(0.0, 1.0);
            let points: Vec<[f64; 2]> =
                scale.ridge(row, &g.stats.bins, peak, 1.0).into_iter().map(|(x, y)| [f64::from(x), f64::from(y)]).collect();
            json!({
                "baseline": f64::from(scale.baseline(row)),
                "points": points,
                "code": g.code,
                "title": short_metro(&g.name),
                "full_title": g.name,
                "median": if g.stats.median.is_finite() { json!(g.stats.median) } else { Value::Null },
                "median_text": money(g.stats.median),
                "gini": g.stats.gini,
                "gini_text": format!("{:.2}", g.stats.gini),
                "color": viridis_hex(if g.stats.median.is_finite() { g.stats.median } else { 0.0 }),
                "share": share,
                "share_text": percent(share),
            })
        })
        .collect();
    let (lo_frac, hi_frac) = range_fracs(range);
    json!({
        "gini_label": GINI_LABEL,
        "key": format!("{} largest metros, by median; colour = median", metros.len()),
        "full": range.is_full(),
        "lo_frac": lo_frac,
        "hi_frac": hi_frac,
        "height_px": RIDGE_STEP_PX * (metros.len() as f64 + RIDGE_OVERLAP),
        "axis": axis_labels(),
        "metros": metros,
    })
    .to_string()
}

/// The income axis' three labels, `[label, frac]`: the distribution
/// edges' first, middle and last incomes.
fn axis_labels() -> Vec<(String, f64)> {
    let e = default_edges().edges();
    let (first, last) = (e[0], e[e.len() - 1]);
    [first, (first + last) / 2.0, last].into_iter().map(|x| (money(x), income_fraction(x))).collect()
}

/// The In view tab, from the last settled count (`None`: not counted yet).
/// `{"counting": true, "text"}` before it; after, `{"counting": false,
/// "heading", "lo_frac", "hi_frac", "bar_fracs", "states": [row],
/// "metros": [row], "no_metro"}` with each row `{"kind", "code", "name",
/// "people_text", "median_text", "median_frac", "share", "share_text",
/// "strip", "can_fly", "title"}`, most people in view first, and the
/// headings `columns` and `sections` (`[states, metros]`). `no_metro` is the line to show
/// when `metros` is empty, else `null`.
pub fn in_view_json(b: &Breakdowns, v: Option<&InView>, range: IncomeRange) -> String {
    let Some(v) = v else {
        return json!({ "counting": true, "text": "Counting what the view shows…" }).to_string();
    };
    let (lo, hi) = range.bounds();
    let row = |kind: &str, g: &Group, name: String, iv: &InViewGroup| {
        let share = iv.share_in_range(lo, hi).clamp(0.0, 1.0);
        json!({
            "can_fly": g.bounds.is_some(),
            "title": fly_title(g, &people(iv.population_total)),
            "kind": kind,
            "code": g.code,
            "name": name,
            "people_text": people(iv.population_total),
            "median_text": money(iv.stats.median),
            "median_frac": median_frac(iv.stats.median),
            "share": share,
            "share_text": percent(share),
            "strip": strip(&iv.stats.bins),
        })
    };
    let states: Vec<Value> = v
        .states
        .iter()
        .filter_map(|iv| b.states.get(iv.group).map(|g| row("state", g, g.name.clone(), iv)))
        .collect();
    let metros: Vec<Value> = v
        .metros
        .iter()
        .filter_map(|iv| b.metros.get(iv.group).map(|g| row("metro", g, short_metro(&g.name), iv)))
        .collect();
    let (lo_frac, hi_frac) = range_fracs(range);
    json!({
        "counting": false,
        "heading": format!(
            "{} block groups in view; the {IN_VIEW_GROUPS} states and metros with the most people in it:",
            thousands(u64::from(v.block_groups))
        ),
        "lo_frac": lo_frac,
        "hi_frac": hi_frac,
        "bar_fracs": bar_fracs(),
        "columns": ["people", "median", "in range"],
        "sections": ["States", "Metros"],
        "no_metro": if metros.is_empty() { json!("No metro area in view") } else { Value::Null },
        "states": states,
        "metros": metros,
    })
    .to_string()
}

/// The paint a census source gets: the range's own, or -- while the panel
/// points at another state -- that paint dimmed
/// ([`super::ramp::dimmed_paint_json`]). `pointed`: the state FIPS the
/// panel's row hover points at; `source_fips`: this source's state.
pub fn source_paint_json(range: IncomeRange, pointed: Option<u32>, source_fips: u32) -> String {
    let paint = range.paint_json(true);
    if is_dimmed(pointed, source_fips) { super::ramp::dimmed_paint_json(&paint) } else { paint }
}

/// Whether `source_fips`'s source is painted dimmed while the panel points
/// at `pointed`.
pub fn is_dimmed(pointed: Option<u32>, source_fips: u32) -> bool {
    pointed.is_some_and(|p| p != source_fips)
}

/// Whether moving the pointed state from `old` to `new` changes
/// `source_fips`'s paint: only the old and new states' sources between two
/// states, every source to or from none.
pub fn source_needs_repaint(old: Option<u32>, new: Option<u32>, source_fips: u32) -> bool {
    is_dimmed(old, source_fips) != is_dimmed(new, source_fips)
}

/// The panel's revision: what its open tab was drawn from. The page skips
/// a redraw while it is unchanged, so EVERYTHING the tabs show that can
/// change without the camera moving bumps it -- the slider's range and the
/// groups' bounds landing (each row's `can_fly` and `title`). The In view
/// tab also redraws on its own settled recount.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelRev(u64);

impl PanelRev {
    pub fn value(self) -> u64 {
        self.0
    }

    /// The slider's range changed.
    pub fn range_moved(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    /// Sets the groups' bounds (the fly-to's; `Breakdowns::set_bounds`)
    /// and bumps the revision, since rows' `can_fly` may have changed.
    pub fn set_bounds(&mut self, b: &mut Breakdowns, boxes: &[[f32; 4]]) {
        b.set_bounds(boxes);
        self.0 = self.0.wrapping_add(1);
    }
}

/// `map_census_dim_state`'s argument: a FIPS `0..100`, or anything else
/// (the page passes `-1`) for "no state: restore every source".
pub fn pointed_state(fips: i32) -> Option<u32> {
    u32::try_from(fips).ok().filter(|&f| f < 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::census::breakdowns::{BinEdges, Row};

    /// CA (06), NY (36) and PR (72): three block groups each, in LA, NYC
    /// and San Juan counties, so the metros table places them.
    fn sample_breakdowns() -> Breakdowns {
        let geoid = |state: u64, county: u64, i: u64| state * 10_000_000_000 + county * 10_000_000 + i;
        let rows: Vec<Row> = [
            (geoid(6, 37, 1), 60_000.0, 1_200.0),
            (geoid(6, 37, 2), 95_000.0, 900.0),
            (geoid(6, 37, 3), 180_000.0, 700.0),
            (geoid(36, 61, 1), 45_000.0, 1_500.0),
            (geoid(36, 61, 2), 120_000.0, 1_000.0),
            (geoid(36, 61, 3), 240_000.0, 400.0),
            (geoid(72, 127, 1), 15_000.0, 800.0),
            (geoid(72, 127, 2), 22_000.0, 700.0),
            (geoid(72, 127, 3), 38_000.0, 600.0),
        ]
        .iter()
        .map(|&(geoid, x, p)| Row { geoid, income: Some(x), population: Some(p) })
        .collect();
        let cbsa = "county\tcbsa\ttitle\n\
                    06037\t31080\tLos Angeles-Long Beach-Anaheim, CA\n\
                    36061\t35620\tNew York-Newark-Jersey City, NY-NJ\n\
                    72127\t41980\tSan Juan-Bayamón-Caguas, PR\n";
        Breakdowns::from_texts(&rows, None, cbsa, BinEdges::default()).unwrap()
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn states_json_sorts_three_ways_and_keeps_puerto_rico_last() {
        let b = sample_breakdowns();
        for sort in [SORT_MEDIAN, SORT_SHARE, SORT_POPULATION] {
            let v: serde_json::Value = serde_json::from_str(&states_json(&b, IncomeRange::full(), sort)).unwrap();
            let rows = v["rows"].as_array().unwrap();
            assert_eq!(rows.last().unwrap()["postal"], "PR", "sort {sort}");
            assert!(rows.iter().all(|r| r["share"].as_f64().unwrap() <= 1.0));
            assert_eq!(rows[0]["strip"].as_array().unwrap().len(), crate::census::breakdowns::DIST_BINS);
        }
    }

    #[test]
    fn metros_json_carries_the_gini_label_and_the_range_fractions() {
        let b = sample_breakdowns();
        let r = IncomeRange { lo: 40, hi: 120 };
        let v: serde_json::Value = serde_json::from_str(&metros_json(&b, r)).unwrap();
        assert_eq!(v["gini_label"], crate::census::breakdowns::GINI_LABEL);
        assert!(v["lo_frac"].as_f64().unwrap() < v["hi_frac"].as_f64().unwrap());
    }

    #[test]
    fn dim_paint_keeps_classes_so_it_is_table_only() {
        let base = IncomeRange::full().paint_json(true);
        let dim = crate::census::ramp::dimmed_paint_json(&base);
        assert_eq!(base.matches('[').count(), dim.matches('[').count(), "the expression's shape is unchanged");
    }

    #[test]
    fn the_states_rows_follow_each_sort_and_the_range() {
        let b = sample_breakdowns();
        let order = |sort, range| -> Vec<String> {
            parse(&states_json(&b, range, sort))["rows"].as_array().unwrap().iter().map(|r| r["postal"].as_str().unwrap().to_string()).collect()
        };
        // Medians: CA 95k (1,200 of its 2,800 people at 60k, the next 900
        // at 95k cross half); NY 45k (1,500 of 2,900 already over half).
        assert_eq!(order(SORT_MEDIAN, IncomeRange::full()), ["CA", "NY", "PR"]);
        assert_eq!(order(SORT_POPULATION, IncomeRange::full()), ["NY", "CA", "PR"]);
        // At >= ~100k: CA 700/2,800 = 25%, NY 1,400/2,900 = 48%.
        let rich = IncomeRange { lo: IncomeRange::nearest_stop(100_000.0), hi: IncomeRange::full().hi };
        assert_eq!(order(SORT_SHARE, rich), ["NY", "CA", "PR"]);
        let v = parse(&states_json(&b, rich, SORT_SHARE));
        let ny = &v["rows"][0];
        assert!((ny["share"].as_f64().unwrap() - 1_400.0 / 2_900.0).abs() < 1e-9, "{ny}");
        assert_eq!(ny["share_text"], "48%");
        assert_eq!(ny["name"], "New York");
        assert_eq!(ny["fips"], 36);
        assert_eq!(ny["population_text"], "2.9K");
        assert_eq!(v["rows"][2]["apart"], true);
        assert_eq!(v["rows"][0]["apart"], false);
        assert_eq!(v["sort"], SORT_SHARE);
        assert_eq!(v["full"], false);
        assert_eq!(v["bar_fracs"].as_array().unwrap().len(), crate::census::breakdowns::DIST_BINS);
        assert_eq!(v["footnote"].as_array().unwrap().len(), 3);
        let sorts: Vec<&str> = v["sorts"].as_array().unwrap().iter().map(|s| s["label"].as_str().unwrap()).collect();
        assert_eq!(sorts, ["median", "in range", "population"]);
        assert_eq!(v["sorts"][1]["code"], SORT_SHARE);
        assert_eq!(v["columns"].as_array().unwrap().len(), 5);
        // No bounds yet (the pack's states still loading): not flyable.
        assert_eq!(ny["can_fly"], false);
        assert!(ny["title"].as_str().unwrap().contains("still loading"), "{}", ny["title"]);
        // A strip: heights to the row's own peak (so one bar is 1), each
        // bar's viridis colour.
        let strip = v["rows"][0]["strip"].as_array().unwrap();
        assert!(strip.iter().any(|s| s["h"] == 1.0));
        assert!(strip.iter().all(|s| s["color"].as_str().unwrap().starts_with('#')));
        // An unknown sort is the median's.
        assert_eq!(order(99, IncomeRange::full()), ["CA", "NY", "PR"]);
    }

    #[test]
    fn the_metros_are_ranked_by_median_with_ridges_against_one_peak() {
        let b = sample_breakdowns();
        let v = parse(&metros_json(&b, IncomeRange::full()));
        let metros = v["metros"].as_array().unwrap();
        let titles: Vec<&str> = metros.iter().map(|m| m["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["Los Angeles, CA", "New York, NY", "San Juan, PR"]);
        assert_eq!(metros[1]["full_title"], "New York-Newark-Jersey City, NY-NJ");
        // RidgeScale over a unit plot: the last baseline is the plot's
        // bottom, the tallest bar anywhere reaches its top, every point in.
        let n = crate::census::breakdowns::DIST_BINS;
        assert!((metros[2]["baseline"].as_f64().unwrap() - 1.0).abs() < 1e-6);
        let ys: Vec<f64> = metros.iter().flat_map(|m| m["points"].as_array().unwrap().iter().map(|p| p[1].as_f64().unwrap())).collect();
        let xs: Vec<f64> = metros.iter().flat_map(|m| m["points"].as_array().unwrap().iter().map(|p| p[0].as_f64().unwrap())).collect();
        assert_eq!(ys.len(), 3 * (n + 2));
        // The tallest bar anywhere rises the full amplitude (the first
        // ridge's baseline, whose peak could reach y = 0).
        let rise = metros
            .iter()
            .map(|m| m["baseline"].as_f64().unwrap() - m["points"].as_array().unwrap().iter().map(|p| p[1].as_f64().unwrap()).fold(1.0, f64::min))
            .fold(0.0, f64::max);
        assert!((rise - metros[0]["baseline"].as_f64().unwrap()).abs() < 1e-6, "{rise}");
        assert!(ys.iter().chain(&xs).all(|&v| (-1e-6..=1.0 + 1e-6).contains(&v)));
        assert_eq!(metros[0]["points"][0][0], 0.0);
        assert_eq!(metros[0]["points"][n + 1][0], 1.0);
        assert!((v["height_px"].as_f64().unwrap() - RIDGE_STEP_PX * (3.0 + RIDGE_OVERLAP)).abs() < 1e-9);
        let axis: Vec<&str> = v["axis"].as_array().unwrap().iter().map(|a| a[0].as_str().unwrap()).collect();
        assert_eq!(axis, ["$2.5K", "$126K", "$250K"]);
        assert_eq!(v["axis"][1][1], 0.5);
        assert_eq!(v["full"], true);
        assert_eq!(v["key"], "3 largest metros, by median; colour = median");
        assert_eq!(metros[0]["share"], 1.0, "the full range holds everyone");
    }

    #[test]
    fn in_view_json_counts_then_lists_the_most_populous_groups() {
        let b = sample_breakdowns();
        let counting = parse(&in_view_json(&b, None, IncomeRange::full()));
        assert_eq!(counting["counting"], true);
        // NY's three block groups and one of CA's.
        let v = b.in_view(&[0, 3, 4, 5], IN_VIEW_GROUPS);
        let j = parse(&in_view_json(&b, Some(&v), IncomeRange::full()));
        assert_eq!(j["counting"], false);
        assert_eq!(j["heading"], "4 block groups in view; the 8 states and metros with the most people in it:");
        let names: Vec<&str> = j["states"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["New York", "California"]);
        assert_eq!(j["states"][1]["people_text"], "1.2K");
        assert_eq!(j["metros"][0]["name"], "New York, NY");
        assert_eq!(j["metros"][0]["kind"], "metro");
        assert!(j["no_metro"].is_null());
        let none = b.in_view(&[], IN_VIEW_GROUPS);
        let j = parse(&in_view_json(&b, Some(&none), IncomeRange::full()));
        assert_eq!(j["no_metro"], "No metro area in view");
        assert_eq!(j["states"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_partly_loaded_pack_still_flies_to_the_states_that_arrived() {
        let mut b = sample_breakdowns();
        // Only California's three rows (0..3) were pushed -- and in
        // reverse, as the pack's view order may -- then the load stopped.
        let bbox = |x: f32| crate::census::view::ViewItem { bbox: [x, 0.40, x + 0.01, 0.41], bin: 0 };
        let items = [bbox(0.17), bbox(0.16), bbox(0.15)];
        let boxes = crate::census::view::row_boxes(&items, &[2, 1, 0], 9);
        assert_eq!(boxes.len(), 9);
        assert!(boxes[3..].iter().all(|b| b.iter().all(|v| v.is_nan())));
        assert_eq!(boxes[0][0], 0.15);
        b.set_bounds(&boxes);
        let v = parse(&states_json(&b, IncomeRange::full(), SORT_MEDIAN));
        let by = |postal: &str| v["rows"].as_array().unwrap().iter().find(|r| r["postal"] == postal).unwrap().clone();
        assert_eq!(by("CA")["can_fly"], true);
        assert!(by("CA")["title"].as_str().unwrap().contains("click to fly"));
        assert_eq!(by("NY")["can_fly"], false);
        let ca = b.states.iter().find(|g| g.code == 6).unwrap().bounds.unwrap();
        assert!((ca.x0 - 0.15).abs() < 1e-6 && (ca.x1 - 0.18).abs() < 1e-6, "{ca:?}");
    }

    /// The page's redraw key is this revision: finishing the load (the
    /// bounds landing) must move it, or the States rows stay "still
    /// loading" and unclickable after every state is in.
    #[test]
    fn the_panel_revision_moves_when_the_load_completes() {
        let mut b = sample_breakdowns();
        let mut rev = PanelRev::default();
        let before = (rev.value(), states_json(&b, IncomeRange::full(), SORT_MEDIAN));
        assert!(parse(&before.1)["rows"].as_array().unwrap().iter().all(|r| r["can_fly"] == false));
        let bbox = |x: f32| crate::census::view::ViewItem { bbox: [x, 0.40, x + 0.01, 0.41], bin: 0 };
        let items: Vec<_> = (0..9).map(|i| bbox(0.1 + i as f32 * 0.05)).collect();
        let rows: Vec<u32> = (0..9).collect();
        rev.set_bounds(&mut b, &crate::census::view::row_boxes(&items, &rows, 9));
        let after = (rev.value(), states_json(&b, IncomeRange::full(), SORT_MEDIAN));
        assert_ne!(after.0, before.0, "the load completing moves the revision");
        assert_ne!(after.1, before.1, "and what the rows show did change");
        assert!(parse(&after.1)["rows"].as_array().unwrap().iter().all(|r| r["can_fly"] == true));
        let mut moved = rev;
        moved.range_moved();
        assert_ne!(moved.value(), rev.value());
    }

    #[test]
    fn a_pointed_state_dims_every_other_source_and_minus_one_restores() {
        let r = IncomeRange { lo: 10, hi: 90 };
        let plain = r.paint_json(true);
        assert_eq!(source_paint_json(r, None, 6), plain);
        assert_eq!(source_paint_json(r, Some(6), 6), plain, "the pointed state keeps its paint");
        assert_eq!(source_paint_json(r, Some(6), 36), crate::census::ramp::dimmed_paint_json(&plain));
        // Between two states only their two sources change; to or from
        // none, every source.
        let moved: Vec<u32> = [6u32, 36, 48, 72].into_iter().filter(|&f| source_needs_repaint(Some(6), Some(36), f)).collect();
        assert_eq!(moved, [6, 36]);
        assert!([6u32, 36, 48].iter().all(|&f| source_needs_repaint(None, Some(36), f) == (f != 36)));
        assert!([6u32, 36, 48].iter().all(|&f| source_needs_repaint(Some(36), None, f) == (f != 36)));
        assert!(!source_needs_repaint(Some(6), Some(6), 36));
        // What each call repaints matches the paint the source ends with.
        for (old, new) in [(None, Some(6)), (Some(6), Some(36)), (Some(36), None)] {
            for f in [6u32, 36, 48] {
                let changed = source_paint_json(r, old, f) != source_paint_json(r, new, f);
                assert_eq!(source_needs_repaint(old, new, f), changed, "{old:?} -> {new:?}, {f}");
            }
        }
        assert_eq!(pointed_state(-1), None);
        assert_eq!(pointed_state(100), None);
        assert_eq!(pointed_state(6), Some(6));
    }

    #[test]
    fn money_reads_like_the_legend() {
        assert_eq!(money(98_843.0), "$98.8K");
        assert_eq!(money(141_477.0), "$141K");
        assert_eq!(money(99_960.0), "$100K");
        assert_eq!(money(99_940.0), "$99.9K");
        assert_eq!(money(2_500.0), "$2.5K");
        assert_eq!(money(950.0), "$950");
        assert_eq!(money(250_001.0), "$250K");
        assert_eq!(money(f64::NAN), "–");
    }

    #[test]
    fn percent_never_rounds_to_a_false_zero_or_hundred() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.004), "<1%");
        assert_eq!(percent(0.01), "1%");
        assert_eq!(percent(0.426), "43%");
        assert_eq!(percent(0.995), ">99%");
        assert_eq!(percent(1.0), "100%");
        assert_eq!(percent(f64::NAN), "0%");
    }

    #[test]
    fn people_are_compact() {
        assert_eq!(people(39_242_785.0), "39M");
        assert_eq!(people(6_992_395.0), "7.0M");
        assert_eq!(people(672_079.0), "672K");
        assert_eq!(people(999_600.0), "1.0M");
        assert_eq!(people(9_870.0), "9.9K");
        assert_eq!(people(950.0), "950");
    }

    #[test]
    fn metro_names_shorten_to_first_city_and_state() {
        assert_eq!(short_metro("San Francisco-Oakland-Fremont, CA"), "San Francisco, CA");
        assert_eq!(short_metro("St. Louis, MO-IL"), "St. Louis, MO");
        assert_eq!(short_metro("New York-Newark-Jersey City, NY-NJ"), "New York, NY");
        assert_eq!(short_metro("Nowhere"), "Nowhere");
    }

    fn states() -> Breakdowns {
        // (FIPS, income, population): CA richer and bigger than TX, WY
        // richest but tiny, PR poorest.
        let rows: Vec<Row> = [(6u64, 90_000.0, 1_000.0), (6, 110_000.0, 1_000.0), (48, 70_000.0, 1_500.0), (56, 120_000.0, 100.0), (72, 25_000.0, 500.0)]
            .iter()
            .enumerate()
            .map(|(i, &(s, x, p))| Row { geoid: s * 10_000_000_000 + i as u64, income: Some(x), population: Some(p) })
            .collect();
        Breakdowns::build(&rows, &crate::census::breakdowns::MetroTable::default(), BinEdges::default())
    }

    fn codes(b: &Breakdowns, idx: &[usize]) -> Vec<u32> {
        idx.iter().map(|&i| b.states[i].code).collect()
    }

    #[test]
    fn states_sort_by_median_share_or_population_with_pr_apart() {
        let b = states();
        let shares = b.state_shares(Some(100_000.0), None);
        let (main, apart) = sort_states(&b.states, &shares, StateSort::Median);
        assert_eq!(codes(&b, &main), [56, 6, 48]);
        assert_eq!(codes(&b, &apart), [72]);
        let (main, _) = sort_states(&b.states, &shares, StateSort::Share);
        // WY 100% >= 100k, CA 50%, TX 0%.
        assert_eq!(codes(&b, &main), [56, 6, 48]);
        let shares = b.state_shares(None, Some(100_000.0));
        let (main, _) = sort_states(&b.states, &shares, StateSort::Share);
        assert_eq!(codes(&b, &main), [48, 6, 56]);
        let (main, apart) = sort_states(&b.states, &shares, StateSort::Population);
        assert_eq!(codes(&b, &main), [6, 48, 56]);
        assert_eq!(codes(&b, &apart), [72]);
        for s in [StateSort::Median, StateSort::Share, StateSort::Population] {
            assert_eq!(StateSort::from_code(s.code()), s);
        }
    }

    #[test]
    fn ridge_scale_fills_the_height_and_peaks_at_the_amplitude() {
        let s = RidgeScale::new(25, 450.0, 1.4);
        assert!((s.baseline(24) - 450.0).abs() < 1e-3, "last baseline {}", s.baseline(24));
        assert!((s.amplitude - s.step * 2.4).abs() < 1e-4);
        assert!((s.baseline(0) - s.amplitude).abs() < 1e-6);
        let bins = [0.0, 0.1, 0.4, 0.2, 0.0];
        let pts = s.ridge(0, &bins, 0.4, 150.0);
        assert_eq!(pts.len(), bins.len() + 2);
        assert_eq!(pts[0], (0.0, s.baseline(0)));
        assert_eq!(*pts.last().unwrap(), (150.0, s.baseline(0)));
        // The common peak reaches y = 0 on the first ridge.
        assert!(pts[3].1.abs() < 1e-4, "{:?}", pts[3]);
        assert_eq!(pts[1].1, s.baseline(0));
        assert!(pts.windows(2).all(|w| w[0].0 < w[1].0), "x rises");
        // A lower ridge's bar at half the peak rises half the amplitude.
        let pts = s.ridge(3, &[0.2], 0.4, 100.0);
        assert!((s.baseline(3) - pts[1].1 - s.amplitude / 2.0).abs() < 1e-4);
        assert_eq!(pts[1].0, 50.0);
        // No peak: flat.
        assert!(s.ridge(1, &[0.0, 0.0], 0.0, 10.0).iter().all(|p| p.1 == s.baseline(1)));
        assert_eq!(common_peak([&[0.1f32, 0.3][..], &[0.2][..]].into_iter()), 0.3);
    }
}
