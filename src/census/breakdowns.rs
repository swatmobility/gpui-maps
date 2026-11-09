//! Income breakdowns for the census demo's charts: per state, per
//! metropolitan area, and for whatever the view shows.
//!
//! The data and the aggregation, shared by the native demo (whose
//! `demos/us_block_groups/panel.rs` draws them) and the browser shell.
//! Moved from `demos/us_block_groups/breakdowns.rs`. Every statistic
//! is over BLOCK GROUPS, each weighted by its total population (ACS
//! 5-year 2023, B01003_001E), of each block group's MEDIAN household
//! income (B19013_001E). So:
//!
//! - the "median" of a state is the population-weighted median of its
//!   block groups' medians -- the income of the block group the median
//!   resident lives in -- which tracks, but is not, the Census's own state
//!   median household income;
//! - the Gini here is between-block-group inequality (how unequal the
//!   neighbourhoods' medians are), always well under a household Gini,
//!   which also counts the spread INSIDE each block group. Label it with
//!   [`GINI_LABEL`].
//!
//! A block group with no income estimate or no population estimate is
//! EXCLUDED from the weighting (and counted in `excluded_*`), never
//! counted as zero. A population of 0 is a real estimate, and weighs 0.
//!
//! Cost model:
//! - [`Breakdowns::build`] sorts every weighted block group by income once
//!   and deals them out, in that order, into per-state and per-metro sorted
//!   arrays with cumulative population (measured in the demo's log line).
//! - [`Group::share_in_range`] is two binary searches: O(log n) per group,
//!   no walk over the block groups -- cheap enough for every slider move.
//! - [`Breakdowns::in_view`] ranks the groups by people in view, then makes
//!   one O(n) pass over the global income order (no per-group sort) for
//!   the survivors only; the demo runs it on the background executor next
//!   to the in-view histogram, on the same camera-settled throttle. Each
//!   survivor keeps its own sorted incomes, so its share in range is exact
//!   and two binary searches too.

use std::collections::HashMap;

use super::view::circular_cover;

/// Bars in each group's compact income distribution.
pub const DIST_BINS: usize = 20;

/// How many metros the "big metros" chart shows.
pub const TOP_METROS: usize = 25;

/// What the Gini here measures, for the chart's label.
pub const GINI_LABEL: &str = "Gini between block groups (population-weighted, of block-group median incomes; not a household Gini)";

/// One block group as the breakdowns read it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    /// The 12-digit GEOID: state (2) + county (3) + tract (6) + block group (1).
    pub geoid: u64,
    /// Median household income; `None` when the Census has no estimate.
    pub income: Option<f64>,
    /// Total population; `None` when there is no population row.
    pub population: Option<f64>,
}

impl Row {
    /// The 2-digit state FIPS.
    pub fn state(&self) -> u32 {
        (self.geoid / 10_000_000_000) as u32
    }

    /// The 5-digit state+county FIPS.
    pub fn county(&self) -> u32 {
        (self.geoid / 10_000_000) as u32
    }

    fn weighted(&self) -> Option<(f64, f64)> {
        match (self.income, self.population) {
            (Some(x), Some(w)) if x.is_finite() && w.is_finite() && w >= 0.0 => Some((x, w)),
            _ => None,
        }
    }
}

/// The income distribution's bar edges, ascending, `DIST_BINS + 1` of them
/// by default. An income below the first edge counts in the first bar, one
/// at or past the last in the last (the ACS top-codes at $250,001).
#[derive(Clone, Debug, PartialEq)]
pub struct BinEdges(Vec<f64>);

impl BinEdges {
    /// `bins` equal bars from `lo` to `hi`.
    pub fn linear(lo: f64, hi: f64, bins: usize) -> Self {
        assert!(bins >= 1 && hi > lo, "at least one bar over a non-empty range");
        Self((0..=bins).map(|i| lo + (hi - lo) * i as f64 / bins as f64).collect())
    }

    pub fn edges(&self) -> &[f64] {
        &self.0
    }

    pub fn bins(&self) -> usize {
        self.0.len() - 1
    }

    /// The bar `income` counts in.
    pub fn bin(&self, income: f64) -> usize {
        self.0.partition_point(|&e| e <= income).saturating_sub(1).min(self.bins() - 1)
    }
}

/// The default edges, built once.
pub fn default_edges() -> &'static BinEdges {
    static EDGES: std::sync::OnceLock<BinEdges> = std::sync::OnceLock::new();
    EDGES.get_or_init(BinEdges::default)
}

impl Default for BinEdges {
    /// The demo slider's axis, $2,500 to $250,000, in [`DIST_BINS`] bars.
    fn default() -> Self {
        Self::linear(2_500.0, 250_000.0, DIST_BINS)
    }
}

/// A group's statistics: over its block groups with both an income and a
/// population, each weighted by its population.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupStats {
    /// Population of the weighted block groups (the weights' sum).
    pub population: f64,
    /// Block groups weighted.
    pub block_groups: u32,
    /// Block groups left out for a missing income or population.
    pub excluded_block_groups: u32,
    /// Population of block groups left out for a missing income.
    pub excluded_population: f64,
    /// Population-weighted quartiles of block-group median income.
    pub p25: f64,
    pub median: f64,
    pub p75: f64,
    /// Population-weighted mean of block-group median income.
    pub mean: f64,
    /// Between-block-group Gini, `0..1` -- see [`GINI_LABEL`].
    pub gini: f64,
    /// Share of `population` in each bar of the [`BinEdges`]; sums to 1
    /// (all 0 for an empty group).
    pub bins: Vec<f32>,
}

impl GroupStats {
    /// The interquartile range, `p75 - p25` (the samples print it; the
    /// panel draws the quartiles as a strip instead).
    #[cfg(test)]
    pub fn iqr(&self) -> f64 {
        self.p75 - self.p25
    }

    /// Everything a group's stats need, from `(income, weight)` pairs
    /// already sorted by income.
    pub fn of_sorted(sorted: &[(f64, f64)], edges: &BinEdges) -> Self {
        debug_assert!(sorted.windows(2).all(|p| p[0].0 <= p[1].0), "sorted by income");
        let mut bins = vec![0f64; edges.bins()];
        let (mut total, mut sum_wx) = (0.0, 0.0);
        for &(x, w) in sorted {
            total += w;
            sum_wx += w * x;
            bins[edges.bin(x)] += w;
        }
        let mut stats = Self {
            population: total,
            block_groups: sorted.len() as u32,
            bins: bins.iter().map(|&b| if total > 0.0 { (b / total) as f32 } else { 0.0 }).collect(),
            ..Self::default()
        };
        if total <= 0.0 {
            return stats;
        }
        // Weighted quantile q: the first income whose inclusive cumulative
        // weight reaches q of the total (the lower weighted quantile).
        let (mut cum, mut gini_num) = (0.0, 0.0);
        let (mut q25, mut q50, mut q75) = (None, None, None);
        for &(x, w) in sorted {
            // sum_i w_i x_i (2 C_i - w_i - W), C_i inclusive: the pairwise
            // |x_i - x_j| sum, in one sorted pass.
            gini_num += w * x * (2.0 * (cum + w) - w - total);
            cum += w;
            for (q, slot) in [(0.25, &mut q25), (0.5, &mut q50), (0.75, &mut q75)] {
                if slot.is_none() && w > 0.0 && cum >= q * total * (1.0 - 1e-12) {
                    *slot = Some(x);
                }
            }
        }
        stats.p25 = q25.unwrap_or(f64::NAN);
        stats.median = q50.unwrap_or(f64::NAN);
        stats.p75 = q75.unwrap_or(f64::NAN);
        stats.mean = sum_wx / total;
        stats.gini = if sum_wx > 0.0 { (gini_num / (total * sum_wx)).max(0.0) } else { 0.0 };
        stats
    }
}

/// Incomes ascending with their cumulative population: what makes a share
/// in range two binary searches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SortedIncomes {
    incomes: Vec<f64>,
    /// `cum[i]`: population of `incomes[..i]`; `len() == incomes.len() + 1`.
    cum: Vec<f64>,
}

impl SortedIncomes {
    /// From `(income, weight)` pairs already sorted by income.
    pub fn of_sorted(pairs: &[(f64, f64)]) -> Self {
        let mut cum = Vec::with_capacity(pairs.len() + 1);
        cum.push(0.0);
        let mut c = 0.0;
        for &(_, w) in pairs {
            c += w;
            cum.push(c);
        }
        Self { incomes: pairs.iter().map(|&(x, _)| x).collect(), cum }
    }

    /// Population with `lo <= income < hi` (`None` is open) -- the slider's
    /// `IncomeRange` semantics. Two binary searches.
    pub fn population_in_range(&self, lo: Option<f64>, hi: Option<f64>) -> f64 {
        let below = |t: Option<f64>, open: usize| t.map_or(open, |t| self.incomes.partition_point(|&x| x < t));
        let (a, b) = (below(lo, 0), below(hi, self.incomes.len()));
        if b <= a {
            0.0
        } else {
            self.cum[b] - self.cum[a]
        }
    }

    /// Share of the weighted population with `lo <= income < hi`, `0..=1`;
    /// 0 when empty.
    pub fn share_in_range(&self, lo: Option<f64>, hi: Option<f64>) -> f64 {
        let total = *self.cum.last().unwrap_or(&0.0);
        if total > 0.0 {
            self.population_in_range(lo, hi) / total
        } else {
            0.0
        }
    }
}

/// A state or metro: its whole-area statistics and, for the slider, its
/// block groups' incomes sorted with their cumulative population.
#[derive(Clone, Debug)]
pub struct Group {
    /// State FIPS or CBSA code.
    pub code: u32,
    /// "California", or "Los Angeles-Long Beach-Anaheim, CA".
    pub name: String,
    /// Two-letter postal code for a state, "" for a metro.
    pub abbrev: &'static str,
    /// Population of every block group with a population estimate, income
    /// or not -- what the groups are ranked by.
    pub population_total: f64,
    pub stats: GroupStats,
    /// The group's block groups' extent in Web Mercator (`0..1`, y down),
    /// from [`Breakdowns::set_bounds`]; `None` until then.
    pub bounds: Option<MercatorBounds>,
    /// Weighted block groups' incomes, ascending, with cumulative people.
    sorted: SortedIncomes,
}

impl Group {
    /// See [`SortedIncomes::share_in_range`].
    pub fn share_in_range(&self, lo: Option<f64>, hi: Option<f64>) -> f64 {
        self.sorted.share_in_range(lo, hi)
    }

    fn of_sorted(code: u32, name: String, abbrev: &'static str, pairs: &[(f64, f64)], edges: &BinEdges) -> Self {
        Self {
            code,
            name,
            abbrev,
            population_total: 0.0,
            stats: GroupStats::of_sorted(pairs, edges),
            bounds: None,
            sorted: SortedIncomes::of_sorted(pairs),
        }
    }
}

/// One group's statistics over the block groups in view.
#[derive(Clone, Debug, PartialEq)]
pub struct InViewGroup {
    /// Index into [`Breakdowns::states`] or [`Breakdowns::metros`].
    pub group: usize,
    /// Population in view of block groups with a population estimate.
    pub population_total: f64,
    pub stats: GroupStats,
    /// The in-view block groups' incomes, for an exact share in range.
    pub sorted: SortedIncomes,
}

impl InViewGroup {
    /// See [`SortedIncomes::share_in_range`]: exact, as the States tab's.
    pub fn share_in_range(&self, lo: Option<f64>, hi: Option<f64>) -> f64 {
        self.sorted.share_in_range(lo, hi)
    }
}

/// The top states and metros in view, by population in view, descending.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InView {
    pub states: Vec<InViewGroup>,
    pub metros: Vec<InViewGroup>,
    /// Block groups the visible set named.
    pub block_groups: u32,
}

const NO_GROUP: u16 = u16::MAX;

/// Every state's and metro's breakdown, built once at load.
#[derive(Clone, Debug)]
pub struct Breakdowns {
    /// States (with DC and PR) with any weighted population, by FIPS.
    pub states: Vec<Group>,
    /// Every metropolitan statistical area with any weighted population,
    /// by `population_total`, largest first: `metros[..TOP_METROS]` is the
    /// big-metros chart.
    pub metros: Vec<Group>,
    pub edges: BinEdges,
    /// Per input row: its index in `states` / `metros`, or `NO_GROUP`.
    row_state: Vec<u16>,
    row_metro: Vec<u16>,
    /// Per input row: `Some((income, weight))` when it is weighted.
    row_weight: Vec<Option<(f64, f64)>>,
    /// Per input row: its population when it has one.
    row_population: Vec<Option<f64>>,
    /// The weighted rows' indices, ascending by income.
    order: Vec<u32>,
}

impl Breakdowns {
    /// Groups `rows` (the demo's features, in its order: `in_view` takes
    /// indices into this slice) by state and by metro.
    pub fn build(rows: &[Row], metros: &MetroTable, edges: BinEdges) -> Self {
        let n = rows.len();
        let row_weight: Vec<Option<(f64, f64)>> = rows.iter().map(Row::weighted).collect();
        let mut order: Vec<u32> = (0..n as u32).filter(|&i| row_weight[i as usize].is_some()).collect();
        order.sort_unstable_by(|&a, &b| {
            let (xa, xb) = (row_weight[a as usize].expect("weighted").0, row_weight[b as usize].expect("weighted").0);
            xa.total_cmp(&xb).then(a.cmp(&b))
        });

        // Group keys, in a stable order: states by FIPS, metros by code
        // (re-sorted by population below).
        let mut state_codes: Vec<u32> = rows.iter().map(Row::state).collect();
        state_codes.sort_unstable();
        state_codes.dedup();
        let state_index: HashMap<u32, usize> = state_codes.iter().enumerate().map(|(i, &c)| (c, i)).collect();
        let row_metro_code: Vec<Option<u32>> = rows.iter().map(|r| metros.metro_of_county(r.county()).map(|m| m.code)).collect();
        let mut metro_codes: Vec<u32> = row_metro_code.iter().flatten().copied().collect();
        metro_codes.sort_unstable();
        metro_codes.dedup();
        let metro_index: HashMap<u32, usize> = metro_codes.iter().enumerate().map(|(i, &c)| (c, i)).collect();

        // Deal the weighted rows out in income order: every group's list
        // comes out sorted, with no per-group sort.
        let mut state_pairs = vec![Vec::new(); state_codes.len()];
        let mut metro_pairs = vec![Vec::new(); metro_codes.len()];
        for &i in &order {
            let r = &rows[i as usize];
            let pair = row_weight[i as usize].expect("weighted");
            state_pairs[state_index[&r.state()]].push(pair);
            if let Some(code) = row_metro_code[i as usize] {
                metro_pairs[metro_index[&code]].push(pair);
            }
        }
        let mut states: Vec<Group> = state_codes
            .iter()
            .zip(&state_pairs)
            .map(|(&code, pairs)| {
                let (name, abbrev) = state_name(code).map_or((format!("State {code:02}"), ""), |(n, a)| (n.to_string(), a));
                Group::of_sorted(code, name, abbrev, pairs, &edges)
            })
            .collect();
        let mut metro_groups: Vec<Group> = metro_codes
            .iter()
            .zip(&metro_pairs)
            .map(|(&code, pairs)| {
                let title = metros.title(code).unwrap_or("").to_string();
                Group::of_sorted(code, title, "", pairs, &edges)
            })
            .collect();
        drop((state_pairs, metro_pairs));

        // Totals and exclusions, over every row.
        for (i, r) in rows.iter().enumerate() {
            let s = &mut states[state_index[&r.state()]];
            let m = row_metro_code[i].map(|c| metro_index[&c]);
            let pop = r.population.filter(|p| p.is_finite() && *p >= 0.0).unwrap_or(0.0);
            let excluded = row_weight[i].is_none();
            for g in std::iter::once(s).chain(m.map(|m| &mut metro_groups[m])) {
                g.population_total += pop;
                if excluded {
                    g.stats.excluded_block_groups += 1;
                    g.stats.excluded_population += pop;
                }
            }
        }

        states.retain(|g| g.stats.population > 0.0);
        metro_groups.retain(|g| g.stats.population > 0.0);
        metro_groups.sort_by(|a, b| b.population_total.total_cmp(&a.population_total).then(a.code.cmp(&b.code)));
        let state_at: HashMap<u32, u16> = states.iter().enumerate().map(|(i, g)| (g.code, i as u16)).collect();
        let metro_at: HashMap<u32, u16> = metro_groups.iter().enumerate().map(|(i, g)| (g.code, i as u16)).collect();
        let row_state = rows.iter().map(|r| state_at.get(&r.state()).copied().unwrap_or(NO_GROUP)).collect();
        let row_metro = row_metro_code.iter().map(|c| c.and_then(|c| metro_at.get(&c).copied()).unwrap_or(NO_GROUP)).collect();
        Self {
            states,
            metros: metro_groups,
            edges,
            row_state,
            row_metro,
            row_weight,
            row_population: rows.iter().map(|r| r.population).collect(),
            order,
        }
    }

    /// Fills every group's [`Group::bounds`] from its block groups'
    /// Mercator boxes (`[x0, y0, x1, y1]`, one per row `build` was given,
    /// in the same order). One pass over the rows.
    pub fn set_bounds(&mut self, boxes: &[[f32; 4]]) {
        let mut states = vec![BoundsAcc::default(); self.states.len()];
        let mut metros = vec![BoundsAcc::default(); self.metros.len()];
        for (row, b) in boxes.iter().enumerate().take(self.row_state.len()) {
            if !b.iter().all(|v| v.is_finite()) {
                continue;
            }
            let s = self.row_state[row];
            if s != NO_GROUP {
                states[usize::from(s)].add(b);
            }
            let m = self.row_metro[row];
            if m != NO_GROUP {
                metros[usize::from(m)].add(b);
            }
        }
        for (g, acc) in self.states.iter_mut().zip(&mut states) {
            g.bounds = acc.bounds();
        }
        for (g, acc) in self.metros.iter_mut().zip(&mut metros) {
            g.bounds = acc.bounds();
        }
    }

    /// Block groups (and their people) left out of the weighting, over
    /// every state: what the footnote owns up to. People are counted only
    /// where a population row exists (a missing income, not a missing
    /// population).
    pub fn excluded(&self) -> (u32, f64) {
        self.states.iter().fold((0, 0.0), |(n, p), g| (n + g.stats.excluded_block_groups, p + g.stats.excluded_population))
    }

    /// Block groups and people weighted, over every state.
    pub fn weighted(&self) -> (u32, f64) {
        self.states.iter().fold((0, 0.0), |(n, p), g| (n + g.stats.block_groups, p + g.stats.population))
    }

    /// The big-metros chart: the `n` most populous metros.
    pub fn top_metros(&self, n: usize) -> &[Group] {
        &self.metros[..n.min(self.metros.len())]
    }

    /// States ranked by weighted median income, highest first (the panel
    /// sorts with [`super::panel_json::sort_states`], which also sets
    /// Puerto Rico apart).
    #[cfg(test)]
    pub fn states_by_median(&self) -> Vec<&Group> {
        let mut v: Vec<&Group> = self.states.iter().collect();
        v.sort_by(|a, b| b.stats.median.total_cmp(&a.stats.median).then(a.code.cmp(&b.code)));
        v
    }

    /// Every state's share of population in `[lo, hi)`, in `states` order.
    pub fn state_shares(&self, lo: Option<f64>, hi: Option<f64>) -> Vec<f64> {
        self.states.iter().map(|g| g.share_in_range(lo, hi)).collect()
    }

    /// Every metro's share of population in `[lo, hi)`, in `metros` order
    /// (the ridgeline shows the range as a band instead).
    #[cfg(test)]
    pub fn metro_shares(&self, lo: Option<f64>, hi: Option<f64>) -> Vec<f64> {
        self.metros.iter().map(|g| g.share_in_range(lo, hi)).collect()
    }

    /// Statistics over the visible block groups (`visible`: indices into the
    /// rows `build` was given; the demo's in-view set), for the `top` states
    /// and metros with the most population in view. One pass over the
    /// global income order, so each group's list arrives sorted.
    pub fn in_view(&self, visible: &[u32], top: usize) -> InView {
        let n = self.row_weight.len();
        let mut seen = vec![false; n];
        for &i in visible {
            if let Some(s) = seen.get_mut(i as usize) {
                *s = true;
            }
        }
        // Pass 1: people in view per group, and who is excluded -- cheap
        // counters for every group, to rank them.
        let mut state_acc = vec![Acc::default(); self.states.len()];
        let mut metro_acc = vec![Acc::default(); self.metros.len()];
        let mut block_groups = 0u32;
        for (i, _) in seen.iter().enumerate().filter(|(_, s)| **s) {
            block_groups += 1;
            let pop = self.row_population[i].filter(|p| p.is_finite() && *p >= 0.0).unwrap_or(0.0);
            let excluded = self.row_weight[i].is_none();
            for acc in self.accs(i, &mut state_acc, &mut metro_acc) {
                acc.present = true;
                acc.population_total += pop;
                if excluded {
                    acc.excluded_block_groups += 1;
                    acc.excluded_population += pop;
                }
            }
        }
        // Only the `top` of each kind by people in view go on: their pairs
        // alone are gathered (memory bounded by them, not by every group).
        let pick = |accs: &mut [Acc]| -> Vec<usize> {
            let mut v: Vec<usize> = (0..accs.len()).filter(|&g| accs[g].present).collect();
            v.sort_by(|&a, &b| accs[b].population_total.total_cmp(&accs[a].population_total).then(a.cmp(&b)));
            v.truncate(top);
            for &g in &v {
                accs[g].kept = true;
            }
            v
        };
        let (states, metros) = (pick(&mut state_acc), pick(&mut metro_acc));
        // Pass 2: the survivors' pairs, in the global income order.
        for &i in &self.order {
            if seen[i as usize] {
                let pair = self.row_weight[i as usize].expect("weighted");
                for acc in self.accs(i as usize, &mut state_acc, &mut metro_acc) {
                    if acc.kept {
                        acc.pairs.push(pair);
                    }
                }
            }
        }
        let finish = |picked: Vec<usize>, accs: &mut [Acc]| -> Vec<InViewGroup> {
            picked
                .into_iter()
                .map(|group| {
                    let a = std::mem::take(&mut accs[group]);
                    let mut stats = GroupStats::of_sorted(&a.pairs, &self.edges);
                    stats.excluded_block_groups = a.excluded_block_groups;
                    stats.excluded_population = a.excluded_population;
                    InViewGroup { group, population_total: a.population_total, stats, sorted: SortedIncomes::of_sorted(&a.pairs) }
                })
                .collect()
        };
        InView { states: finish(states, &mut state_acc), metros: finish(metros, &mut metro_acc), block_groups }
    }

    fn accs<'a>(&self, row: usize, states: &'a mut [Acc], metros: &'a mut [Acc]) -> impl Iterator<Item = &'a mut Acc> {
        let s = self.row_state[row];
        let m = self.row_metro[row];
        let state = (s != NO_GROUP).then(|| &mut states[usize::from(s)]);
        let metro = (m != NO_GROUP).then(|| &mut metros[usize::from(m)]);
        state.into_iter().chain(metro)
    }
}

/// A box in Web Mercator `0..1` (y down). A box across the antimeridian
/// (Alaska's Aleutians) has `x1 > 1` rather than `x0 > x1`; its centre is
/// wrapped back into range.
///
/// [`MercatorBounds::fit`] frames it for a flat, north-up, untilted camera:
/// with pitch, bearing or the globe (where most state fits land) the
/// framing is approximate, which is all a fly-to needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MercatorBounds {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl MercatorBounds {
    /// `(lon, lat)` of the box's centre, longitude wrapped into -180..180.
    pub fn center_lon_lat(&self) -> (f64, f64) {
        let x = ((self.x0 + self.x1) / 2.0).rem_euclid(1.0);
        let y = (self.y0 + self.y1) / 2.0;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
        (x * 360.0 - 180.0, lat)
    }

    /// The camera that fits the box into a `w` x `h` (logical px) view with
    /// `pad` px spare on every side: `(lon, lat, zoom)`, zoom clamped to
    /// `0..=max_zoom`. At zoom z the world is `512 * 2^z` px wide.
    pub fn fit(&self, w: f64, h: f64, pad: f64, max_zoom: f64) -> (f64, f64, f64) {
        let (lon, lat) = self.center_lon_lat();
        let (dx, dy) = ((self.x1 - self.x0).max(1e-9), (self.y1 - self.y0).max(1e-9));
        let (aw, ah) = ((w - 2.0 * pad).max(1.0), (h - 2.0 * pad).max(1.0));
        let zoom = (aw / (dx * 512.0)).min(ah / (dy * 512.0)).log2();
        (lon, lat, zoom.clamp(0.0, max_zoom))
    }
}

/// A group's extent, grown box by box: its y range, and every box's x
/// interval for [`circular_cover`]. A box wider than half the world is
/// taken to be one that straddles the antimeridian and was stored the
/// long way round (`-179.2..179.9` rather than `179.9..180.8`): it is
/// flipped to its short side.
#[derive(Clone, Default)]
struct BoundsAcc {
    y0: f64,
    y1: f64,
    x: Vec<(f64, f64)>,
}

impl BoundsAcc {
    fn add(&mut self, b: &[f32; 4]) {
        let [x0, y0, x1, y1] = b.map(f64::from);
        if self.x.is_empty() {
            (self.y0, self.y1) = (y0, y1);
        } else {
            self.y0 = self.y0.min(y0);
            self.y1 = self.y1.max(y1);
        }
        let x0w = x0.rem_euclid(1.0);
        let (a, b) = if x1 - x0 > 0.5 { (x1.rem_euclid(1.0), x0.rem_euclid(1.0) + 1.0) } else { (x0w, x0w + (x1 - x0)) };
        self.x.push((a, if b < a { b + 1.0 } else { b }));
    }

    fn bounds(&mut self) -> Option<MercatorBounds> {
        let (x0, x1) = circular_cover(&self.x, 1.0)?;
        Some(MercatorBounds { x0, y0: self.y0, x1, y1: self.y1 })
    }
}

#[derive(Clone, Default)]
struct Acc {
    /// Some block group of the group is in view.
    present: bool,
    /// Ranked into the top: gather its pairs.
    kept: bool,
    pairs: Vec<(f64, f64)>,
    population_total: f64,
    excluded_block_groups: u32,
    excluded_population: f64,
}

/// A metropolitan statistical area.
#[derive(Clone, Debug, PartialEq)]
pub struct Metro {
    /// 5-digit CBSA code.
    pub code: u32,
    /// "Los Angeles-Long Beach-Anaheim, CA".
    pub title: String,
}

/// County -> metropolitan statistical area, from
/// `cbsa/county_metro_2023.tsv` (`county<TAB>cbsa<TAB>title`, written by
/// `demos/data/fetch_us_block_groups.sh` from the July 2023 delineation;
/// metro areas only, Connecticut by planning region).
#[derive(Clone, Debug, Default)]
pub struct MetroTable {
    metros: Vec<Metro>,
    by_county: HashMap<u32, usize>,
    by_code: HashMap<u32, usize>,
}

impl MetroTable {
    /// Parses the TSV; a header line and malformed lines are skipped.
    pub fn parse(text: &str) -> Self {
        let mut t = Self::default();
        for line in text.lines() {
            let mut f = line.split('\t');
            let (Some(county), Some(code), Some(title)) = (f.next(), f.next(), f.next()) else { continue };
            let (Ok(county), Ok(code)) = (county.trim().parse::<u32>(), code.trim().parse::<u32>()) else { continue };
            let at = *t.by_code.entry(code).or_insert_with(|| {
                t.metros.push(Metro { code, title: title.trim().to_string() });
                t.metros.len() - 1
            });
            t.by_county.insert(county, at);
        }
        t
    }

    /// The metro a 5-digit state+county FIPS belongs to, if any.
    pub fn metro_of_county(&self, county: u32) -> Option<&Metro> {
        self.by_county.get(&county).map(|&i| &self.metros[i])
    }

    pub fn title(&self, code: u32) -> Option<&str> {
        self.by_code.get(&code).map(|&i| self.metros[i].title.as_str())
    }

    #[cfg(test)]
    pub fn metros(&self) -> &[Metro] {
        &self.metros
    }

    #[cfg(test)]
    pub fn counties(&self) -> usize {
        self.by_county.len()
    }
}

/// `acs/population_bg_2023.csv` (`GEOID,population`): GEOID -> population.
/// A row with an empty (no-estimate) value is left out, so it reads as
/// missing, not zero.
pub fn parse_population(text: &str) -> HashMap<u64, f64> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let (geoid, pop) = line.split_once(',')?;
            Some((geoid.trim().parse().ok()?, pop.trim().parse().ok()?))
        })
        .collect()
}

/// The most block groups [`Breakdowns::from_texts`] takes, and the most
/// lines its population CSV may have: the US has ~242,000 block groups,
/// so this is ~1.65x headroom. `order` holds `u32` row indices, and this
/// keeps them far in range.
///
/// Worst-case memory at this bound, per row (`Row` is 40 B):
///
/// - Kept in the [`Breakdowns`]: `row_weight` 24 B (`Option<(f64, f64)>`) +
///   `order` 4 B + `row_state` and `row_metro` 2 + 2 B + `row_population`
///   16 B + the groups' `SortedIncomes` (each row is in at most a state and
///   a metro, 8 B income + 8 B cumulative each) 32 B = 80 B/row
///   -> 400,000 x 80 B = 32 MB.
/// - Transient, freed when `from_texts` returns: the joined `rows` copy
///   40 B + the population `HashMap` (16 B entries plus a control byte, at
///   most ~2.3x over-allocated) <= 40 B + `state_pairs` and `metro_pairs`
///   (16 B each, at most 2x over-allocated by `push`) <= 64 B +
///   `row_metro_code` 8 B + `state_codes` 4 B = <= 156 B/row
///   -> 400,000 x 156 B = 62.4 MB.
///
/// Peak <= ~95 MB over the caller's own `rows` and table text, against
/// ~57 MB for the real 242,000 rows at the same per-row figures.
pub const MAX_ROWS: usize = 400_000;

/// The most lines [`Breakdowns::from_texts`]' CBSA table may have: the
/// real one is ~1,200 counties. It also bounds the metro count well under
/// the `u16` group index (`NO_GROUP`).
pub const MAX_CBSA_LINES: usize = 10_000;

/// The largest block-group GEOID: 12 digits, so a state FIPS is under 100
/// and the state count stays well under the `u16` group index.
const MAX_GEOID: u64 = 999_999_999_999;

impl Breakdowns {
    /// [`Breakdowns::build`] from the tables' text, as the browser fetches
    /// them: `population_csv` is `acs/population_bg_2023.csv`, or `None`
    /// when `rows` already carry their population (the pack's case). With
    /// `Some`, each row's own `population` is ignored and replaced by the
    /// CSV's, `None` where the CSV has no line for its GEOID.
    /// `cbsa_tsv` is `cbsa/county_metro_2023.tsv`. The text is untrusted,
    /// so every count is bounded before anything is built from it: `Err`
    /// past [`MAX_ROWS`] rows or population lines, [`MAX_CBSA_LINES`] CBSA
    /// lines, or on a GEOID longer than 12 digits.
    pub fn from_texts(rows: &[Row], population_csv: Option<&str>, cbsa_tsv: &str, edges: BinEdges) -> Result<Self, String> {
        if rows.len() > MAX_ROWS {
            return Err(format!("{} block groups, more than {MAX_ROWS}", rows.len()));
        }
        if let Some(bad) = rows.iter().find(|r| r.geoid > MAX_GEOID) {
            return Err(format!("GEOID {} is longer than 12 digits", bad.geoid));
        }
        if cbsa_tsv.lines().nth(MAX_CBSA_LINES).is_some() {
            return Err(format!("the CBSA table has more than {MAX_CBSA_LINES} lines"));
        }
        // The header plus one line per block group.
        if population_csv.is_some_and(|csv| csv.lines().nth(MAX_ROWS + 1).is_some()) {
            return Err(format!("the population table has more than {MAX_ROWS} rows"));
        }
        let metros = MetroTable::parse(cbsa_tsv);
        let Some(csv) = population_csv else {
            return Ok(Self::build(rows, &metros, edges));
        };
        let population = parse_population(csv);
        let rows: Vec<Row> = rows.iter().map(|r| Row { population: population.get(&r.geoid).copied(), ..*r }).collect();
        Ok(Self::build(&rows, &metros, edges))
    }
}

/// State FIPS -> (name, postal code): the 50 states, DC and Puerto Rico.
/// The island areas (60 66 69 78) are not in the ACS and are `None`.
pub fn state_name(fips: u32) -> Option<(&'static str, &'static str)> {
    STATES.iter().find(|(f, _, _)| *f == fips).map(|&(_, n, a)| (n, a))
}

/// State FIPS -> (name, postal code): the 50 states, DC and Puerto Rico.
/// The one table: the popup's [`super::names::state_name`] reads it too.
pub const STATES: &[(u32, &str, &str)] = &[
    (1, "Alabama", "AL"),
    (2, "Alaska", "AK"),
    (4, "Arizona", "AZ"),
    (5, "Arkansas", "AR"),
    (6, "California", "CA"),
    (8, "Colorado", "CO"),
    (9, "Connecticut", "CT"),
    (10, "Delaware", "DE"),
    (11, "District of Columbia", "DC"),
    (12, "Florida", "FL"),
    (13, "Georgia", "GA"),
    (15, "Hawaii", "HI"),
    (16, "Idaho", "ID"),
    (17, "Illinois", "IL"),
    (18, "Indiana", "IN"),
    (19, "Iowa", "IA"),
    (20, "Kansas", "KS"),
    (21, "Kentucky", "KY"),
    (22, "Louisiana", "LA"),
    (23, "Maine", "ME"),
    (24, "Maryland", "MD"),
    (25, "Massachusetts", "MA"),
    (26, "Michigan", "MI"),
    (27, "Minnesota", "MN"),
    (28, "Mississippi", "MS"),
    (29, "Missouri", "MO"),
    (30, "Montana", "MT"),
    (31, "Nebraska", "NE"),
    (32, "Nevada", "NV"),
    (33, "New Hampshire", "NH"),
    (34, "New Jersey", "NJ"),
    (35, "New Mexico", "NM"),
    (36, "New York", "NY"),
    (37, "North Carolina", "NC"),
    (38, "North Dakota", "ND"),
    (39, "Ohio", "OH"),
    (40, "Oklahoma", "OK"),
    (41, "Oregon", "OR"),
    (42, "Pennsylvania", "PA"),
    (44, "Rhode Island", "RI"),
    (45, "South Carolina", "SC"),
    (46, "South Dakota", "SD"),
    (47, "Tennessee", "TN"),
    (48, "Texas", "TX"),
    (49, "Utah", "UT"),
    (50, "Vermont", "VT"),
    (51, "Virginia", "VA"),
    (53, "Washington", "WA"),
    (54, "West Virginia", "WV"),
    (55, "Wisconsin", "WI"),
    (56, "Wyoming", "WY"),
    (72, "Puerto Rico", "PR"),
];

#[cfg(test)]
// GEOIDs are written as the Census publishes them, leading state zero
// included (`060_371_011_101` is LA County); they are decimal, not octal.
#[allow(clippy::zero_prefixed_literal)]
mod tests {
    use super::*;

    fn row(geoid: u64, income: Option<f64>, population: Option<f64>) -> Row {
        Row { geoid, income, population }
    }

    /// Five block groups, weights 1..5 (total 15): cumulative 1, 3, 6, 10,
    /// 15. p25 needs 3.75 -> the third (30k); the median 7.5 -> the fourth
    /// (40k); p75 11.25 -> the fifth (50k). Unweighted, the median would be
    /// 30k: the weights move it.
    #[test]
    fn weighted_median_and_iqr_on_a_known_set() {
        let pairs = vec![(10_000.0, 1.0), (20_000.0, 2.0), (30_000.0, 3.0), (40_000.0, 4.0), (50_000.0, 5.0)];
        let s = GroupStats::of_sorted(&pairs, &BinEdges::default());
        assert_eq!((s.p25, s.median, s.p75), (30_000.0, 40_000.0, 50_000.0));
        assert_eq!(s.iqr(), 20_000.0);
        assert_eq!(s.population, 15.0);
        assert_eq!(s.block_groups, 5);
        let mean = (10.0 + 40.0 + 90.0 + 160.0 + 250.0) * 1000.0 / 15.0;
        assert!((s.mean - mean).abs() < 1e-9);
        // An exact half lands on the lower value: {1: 10k, 1: 20k}.
        let s = GroupStats::of_sorted(&[(10_000.0, 1.0), (20_000.0, 1.0)], &BinEdges::default());
        assert_eq!(s.median, 10_000.0);
    }

    #[test]
    fn a_zero_weight_block_group_never_sets_a_quantile() {
        let s = GroupStats::of_sorted(&[(5_000.0, 0.0), (60_000.0, 10.0)], &BinEdges::default());
        assert_eq!((s.p25, s.median, s.p75), (60_000.0, 60_000.0, 60_000.0));
    }

    #[test]
    fn gini_is_zero_when_every_block_group_is_equal() {
        let pairs: Vec<_> = (0..50).map(|i| (72_000.0, 100.0 + f64::from(i))).collect();
        let s = GroupStats::of_sorted(&pairs, &BinEdges::default());
        assert!(s.gini.abs() < 1e-12, "{}", s.gini);
    }

    /// Two groups: 3 people at 20k and 1 at 100k. Mean = 160k/4 = 40k.
    /// Pairwise sum_ij w_i w_j |x_i - x_j| = 2 * 3 * 1 * 80k = 480k;
    /// G = 480k / (2 * W^2 * mean) = 480k / (2 * 16 * 40k) = 0.375.
    #[test]
    fn gini_matches_a_hand_computed_two_group_case() {
        let s = GroupStats::of_sorted(&[(20_000.0, 3.0), (100_000.0, 1.0)], &BinEdges::default());
        assert!((s.gini - 0.375).abs() < 1e-12, "{}", s.gini);
        // All income in one of n equal-weight groups: (n-1)/n.
        let s = GroupStats::of_sorted(&[(0.0, 1.0), (0.0, 1.0), (0.0, 1.0), (90_000.0, 1.0)], &BinEdges::default());
        assert!((s.gini - 0.75).abs() < 1e-12, "{}", s.gini);
    }

    /// A tiny xorshift: the test needs no `rand` dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_rows(rng: &mut Rng, n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| {
                let state = [6u64, 9, 36, 48][rng.below(4) as usize];
                let county = [37u64, 110, 120, 61, 201][rng.below(5) as usize];
                let geoid = state * 10_000_000_000 + county * 10_000_000 + i as u64;
                // Coarse incomes, so ties and range edges on data values happen.
                let income = (rng.below(10) != 0).then(|| (rng.below(60) * 5_000) as f64);
                let population = (rng.below(12) != 0).then(|| rng.below(3_000) as f64);
                row(geoid, income, population)
            })
            .collect()
    }

    #[test]
    fn share_in_range_matches_a_brute_force_walk() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let rows = random_rows(&mut rng, 5_000);
        let b = Breakdowns::build(&rows, &MetroTable::default(), BinEdges::default());
        assert!(!b.states.is_empty());
        for _ in 0..300 {
            let pick = |rng: &mut Rng| match rng.below(4) {
                0 => None,
                1 => Some((rng.below(60) * 5_000) as f64),
                _ => Some(rng.below(300_000) as f64),
            };
            let (lo, hi) = (pick(&mut rng), pick(&mut rng));
            for g in &b.states {
                let (mut inside, mut total) = (0.0, 0.0);
                for r in rows.iter().filter(|r| r.state() == g.code) {
                    let (Some(x), Some(w)) = (r.income, r.population) else { continue };
                    total += w;
                    if lo.is_none_or(|lo| x >= lo) && hi.is_none_or(|hi| x < hi) {
                        inside += w;
                    }
                }
                let want = if total > 0.0 { inside / total } else { 0.0 };
                let got = g.share_in_range(lo, hi);
                assert!((got - want).abs() < 1e-9, "state {} [{lo:?}, {hi:?}): {got} vs {want}", g.code);
            }
        }
    }

    fn metro_table() -> MetroTable {
        MetroTable::parse(
            "county\tcbsa\ttitle\n\
             06037\t31080\tLos Angeles-Long Beach-Anaheim, CA\n\
             06059\t31080\tLos Angeles-Long Beach-Anaheim, CA\n\
             09120\t14860\tBridgeport-Stamford-Danbury, CT\n\
             09190\t14860\tBridgeport-Stamford-Danbury, CT\n\
             09110\t25540\tHartford-West Hartford-East Hartford, CT\n\
             bad line\n",
        )
    }

    #[test]
    fn cbsa_join_on_known_counties() {
        let t = metro_table();
        assert_eq!(t.metros().len(), 3);
        assert_eq!(t.counties(), 5);
        assert_eq!(t.metro_of_county(6037).map(|m| (m.code, m.title.as_str())), Some((31080, "Los Angeles-Long Beach-Anaheim, CA")));
        // Connecticut by 2022 planning region, as TIGER 2023 codes it; the
        // old county code (Fairfield, 09001) is gone.
        assert_eq!(t.metro_of_county(9120).map(|m| m.code), Some(14860));
        assert_eq!(t.metro_of_county(9110).map(|m| m.title.as_str()), Some("Hartford-West Hartford-East Hartford, CT"));
        assert_eq!(t.metro_of_county(9001), None);
        assert_eq!(t.metro_of_county(6071), None);

        // Through the block-group GEOID: a Los Angeles County block group
        // and a Greater Bridgeport one.
        let rows = [
            row(060_371_011_101, Some(80_000.0), Some(1_000.0)),
            row(091_200_101_001, Some(120_000.0), Some(500.0)),
            row(060_710_001_011, Some(50_000.0), Some(700.0)), // San Bernardino: not in this table
        ];
        let b = Breakdowns::build(&rows, &t, BinEdges::default());
        let names: Vec<_> = b.metros.iter().map(|m| (m.name.as_str(), m.stats.population)).collect();
        assert_eq!(names, [("Los Angeles-Long Beach-Anaheim, CA", 1_000.0), ("Bridgeport-Stamford-Danbury, CT", 500.0)]);
        let states: Vec<_> = b.states.iter().map(|s| (s.name.as_str(), s.abbrev, s.stats.population)).collect();
        assert_eq!(states, [("California", "CA", 1_700.0), ("Connecticut", "CT", 500.0)]);
    }

    #[test]
    fn missing_income_or_population_is_excluded_not_zero() {
        let rows = [
            row(060_371_011_101, Some(100_000.0), Some(1_000.0)),
            row(060_371_011_102, Some(100_000.0), Some(1_000.0)),
            // No income: its 5,000 people must not pull the median to 0.
            row(060_371_011_103, None, Some(5_000.0)),
            // No population row: must not count as a weight of 0 either
            // (it would not move a weighted stat) nor as any weight.
            row(060_371_011_104, Some(10_000.0), None),
            // A real zero population weighs 0.
            row(060_371_011_105, Some(20_000.0), Some(0.0)),
        ];
        let b = Breakdowns::build(&rows, &metro_table(), BinEdges::default());
        for g in [&b.states[0], &b.metros[0]] {
            let s = &g.stats;
            assert_eq!((s.median, s.p25, s.p75), (100_000.0, 100_000.0, 100_000.0), "{}", g.name);
            assert_eq!(s.population, 2_000.0);
            assert_eq!(s.block_groups, 3, "the two 100k rows and the zero-population one");
            assert_eq!(s.excluded_block_groups, 2);
            assert_eq!(s.excluded_population, 5_000.0);
            assert_eq!(g.population_total, 7_000.0);
            assert!(s.gini.abs() < 1e-12);
            assert_eq!(g.share_in_range(Some(50_000.0), None), 1.0);
            assert_eq!(g.share_in_range(None, Some(50_000.0)), 0.0);
        }
    }

    #[test]
    fn distribution_bins_are_population_shares() {
        let edges = BinEdges::default();
        assert_eq!(edges.bins(), DIST_BINS);
        assert_eq!(edges.bin(0.0), 0);
        assert_eq!(edges.bin(250_001.0), DIST_BINS - 1);
        assert_eq!(edges.bin(2_500.0 + 12_375.0), 1);
        let s = GroupStats::of_sorted(&[(1_000.0, 1.0), (250_001.0, 3.0)], &edges);
        assert_eq!(s.bins.len(), DIST_BINS);
        assert_eq!((s.bins[0], s.bins[DIST_BINS - 1]), (0.25, 0.75));
        assert!((s.bins.iter().sum::<f32>() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn in_view_of_everything_matches_the_whole_groups() {
        let mut rng = Rng(42);
        let rows = random_rows(&mut rng, 3_000);
        let b = Breakdowns::build(&rows, &metro_table(), BinEdges::default());
        let all: Vec<u32> = (0..rows.len() as u32).collect();
        let v = b.in_view(&all, usize::MAX);
        assert_eq!(v.block_groups, rows.len() as u32);
        assert_eq!(v.states.len(), b.states.len());
        for iv in &v.states {
            let g = &b.states[iv.group];
            assert_eq!(iv.stats, g.stats, "{}", g.name);
            assert_eq!(iv.population_total, g.population_total);
        }
        for iv in &v.metros {
            assert_eq!(iv.stats, b.metros[iv.group].stats);
        }
    }

    #[test]
    fn in_view_of_a_subset_matches_building_from_that_subset() {
        let mut rng = Rng(7);
        let rows = random_rows(&mut rng, 4_000);
        let t = metro_table();
        let b = Breakdowns::build(&rows, &t, BinEdges::default());
        let visible: Vec<u32> = (0..rows.len() as u32).filter(|_| rng.below(3) == 0).collect();
        let subset: Vec<Row> = visible.iter().map(|&i| rows[i as usize]).collect();
        let want = Breakdowns::build(&subset, &t, BinEdges::default());
        let v = b.in_view(&visible, 2);
        assert_eq!(v.states.len(), 2);
        let mut by_pop: Vec<&Group> = want.states.iter().collect();
        by_pop.sort_by(|a, b| b.population_total.total_cmp(&a.population_total));
        for (iv, w) in v.states.iter().zip(by_pop) {
            assert_eq!(b.states[iv.group].code, w.code);
            assert_eq!(iv.population_total, w.population_total);
            let (a, e) = (&iv.stats, &w.stats);
            assert_eq!((a.median, a.p25, a.p75, a.block_groups, a.excluded_block_groups), (e.median, e.p25, e.p75, e.block_groups, e.excluded_block_groups));
            assert!((a.gini - e.gini).abs() < 1e-12 && (a.population - e.population).abs() < 1e-6);
        }
        // Out-of-range indices are ignored, not a panic.
        assert_eq!(b.in_view(&[u32::MAX], 5), InView::default());
    }

    fn merc(lon: f64, lat: f64) -> (f32, f32) {
        let lat_r = lat.to_radians();
        (
            (lon / 360.0 + 0.5) as f32,
            ((1.0 - (lat_r.tan() + 1.0 / lat_r.cos()).ln() / std::f64::consts::PI) / 2.0) as f32,
        )
    }

    /// A lon/lat box as the demo's `ViewItem` holds it: `[x0, y0, x1, y1]`,
    /// y down, so the NORTH edge is `y0`.
    fn merc_box(w: f64, s: f64, e: f64, n: f64) -> [f32; 4] {
        let (x0, y0) = merc(w, n);
        let (x1, y1) = merc(e, s);
        [x0, y0, x1, y1]
    }

    #[test]
    fn bounds_per_group_cover_their_block_groups() {
        let rows = [
            row(060_371_011_101, Some(80_000.0), Some(1_000.0)), // LA metro, CA
            row(060_710_001_011, Some(50_000.0), Some(700.0)),   // CA, no metro in this table
            row(091_200_101_001, Some(120_000.0), Some(500.0)),  // CT, Bridgeport
            row(091_200_101_002, None, Some(10.0)),              // no income: still inside the bounds
        ];
        let mut b = Breakdowns::build(&rows, &metro_table(), BinEdges::default());
        let boxes = [
            merc_box(-118.5, 33.9, -118.2, 34.1),
            merc_box(-117.5, 34.0, -115.0, 35.5),
            merc_box(-73.5, 41.1, -73.3, 41.2),
            merc_box(-73.2, 41.0, -73.0, 41.4),
        ];
        b.set_bounds(&boxes);
        let ca = b.states.iter().find(|g| g.code == 6).and_then(|g| g.bounds).expect("CA bounds");
        let (lon, lat) = ca.center_lon_lat();
        assert!((lon - (-118.5 + -115.0) / 2.0).abs() < 1e-4, "{lon}");
        assert!(lat > 33.9 && lat < 35.5, "{lat}");
        let ct = b.states.iter().find(|g| g.code == 9).and_then(|g| g.bounds).expect("CT bounds");
        let (x0, y0) = merc(-73.5, 41.4);
        let (x1, y1) = merc(-73.0, 41.0);
        assert_eq!((ct.x0, ct.y0, ct.x1, ct.y1), (f64::from(x0), f64::from(y0), f64::from(x1), f64::from(y1)));
        let la = b.metros.iter().find(|g| g.code == 31080).and_then(|g| g.bounds).expect("LA bounds");
        assert!((la.center_lon_lat().0 - (-118.35)).abs() < 1e-4);
    }

    /// Alaska: the Aleutians cross 180 degrees. The box must be the narrow
    /// one from Attu (172.4 E) round to the panhandle (130 W), not the whole
    /// world, and its centre must land in the Bering Sea side.
    #[test]
    fn bounds_across_the_antimeridian_stay_narrow() {
        let rows = [row(20_100_000_001, Some(80_000.0), Some(1.0)), row(20_100_000_002, Some(90_000.0), Some(1.0))];
        let mut b = Breakdowns::build(&rows, &MetroTable::default(), BinEdges::default());
        b.set_bounds(&[merc_box(172.4, 52.4, 179.9, 53.0), merc_box(-179.0, 51.2, -130.0, 71.4)]);
        let ak = b.states[0].bounds.expect("AK bounds");
        let width_deg = (ak.x1 - ak.x0) * 360.0;
        assert!((width_deg - (360.0 - 172.4 - 130.0)).abs() < 1e-3, "{width_deg}");
        let (lon, lat) = ak.center_lon_lat();
        assert!((-180.0..180.0).contains(&lon) && !(-130.0..172.4).contains(&lon), "{lon}");
        assert!((51.2..71.4).contains(&lat), "{lat}");
    }

    /// The real case: `tl_2023_02_bg.shp` record 207 (an Aleutian block
    /// group) has ONE bbox from -179.23 to 179.86 -- its parts on both
    /// sides of 180, stored the long way round. Beside a mainland box it
    /// must still give the narrow Alaska, not the world centred on lon 0.
    #[test]
    fn an_unsplit_box_across_the_antimeridian_is_taken_the_short_way() {
        let rows = [row(20_100_000_001, Some(80_000.0), Some(1.0)), row(20_100_000_002, Some(90_000.0), Some(1.0))];
        let mut b = Breakdowns::build(&rows, &MetroTable::default(), BinEdges::default());
        b.set_bounds(&[merc_box(-179.23, 51.2, 179.86, 52.1), merc_box(-165.0, 54.0, -130.0, 71.4)]);
        let ak = b.states[0].bounds.expect("AK bounds");
        let width_deg = (ak.x1 - ak.x0) * 360.0;
        // 179.86 E round to 130 W: 50.14 degrees.
        assert!((width_deg - (180.0 - 179.86 + 50.0)).abs() < 1e-2, "{width_deg}");
        let (lon, _) = ak.center_lon_lat();
        // 179.86 + 50.14 / 2 = 204.93 E = 155.07 W.
        assert!((lon - (-155.07)).abs() < 0.01, "{lon}");
        let (_, _, zoom) = ak.fit(1280.0 - 356.0, 800.0, 40.0, 9.0);
        assert!(zoom > 2.0, "{zoom}");
    }

    #[test]
    fn circular_cover_takes_the_complement_of_the_widest_gap() {
        // Plain: no wrap.
        assert_eq!(circular_cover(&[(0.2, 0.3), (0.25, 0.4)], 1.0), Some((0.2, 0.4)));
        // Across the seam: 0.95..1.0 and 0.0..0.1 -> 0.95..1.1.
        let (a, b) = circular_cover(&[(0.0, 0.1), (0.95, 1.0)], 1.0).unwrap();
        assert!((a - 0.95).abs() < 1e-12 && (b - 1.1).abs() < 1e-12, "{a} {b}");
        // An interval already past the seam.
        assert_eq!(circular_cover(&[(0.9, 1.05), (0.02, 0.1)], 1.0), Some((0.9, 1.1)));
        // Everything: the whole circle.
        assert_eq!(circular_cover(&[(0.0, 0.6), (0.5, 1.0)], 1.0), Some((0.0, 1.0)));
        assert_eq!(circular_cover(&[], 1.0), None);
    }

    /// A box 1/4 of the world wide across a 512 px view (no pad) fits at
    /// zoom 2; the tighter axis wins; the clamp holds.
    #[test]
    fn fit_picks_the_zoom_that_frames_the_box() {
        let b = MercatorBounds { x0: 0.25, y0: 0.25, x1: 0.5, y1: 0.375 };
        let (lon, lat, z) = b.fit(512.0, 512.0, 0.0, 22.0);
        assert!((z - 2.0).abs() < 1e-9, "{z}");
        assert!((lon - (-45.0)).abs() < 1e-9, "{lon}");
        assert!(lat > 0.0);
        // Twice as tall a view: the width still binds.
        assert!((b.fit(512.0, 1024.0, 0.0, 22.0).2 - 2.0).abs() < 1e-9);
        // A quarter as tall: the height binds, 1/8 of the world in 128 px -> z1.
        assert!((b.fit(512.0, 128.0, 0.0, 22.0).2 - 1.0).abs() < 1e-9);
        let point = MercatorBounds { x0: 0.5, y0: 0.5, x1: 0.5, y1: 0.5 };
        assert_eq!(point.fit(800.0, 600.0, 20.0, 12.0).2, 12.0);
    }

    /// The In-view tab's "in range" is exact at any handle position, not
    /// only at the 20 bars' edges: a brute-force walk over the visible
    /// rows agrees, and only the top groups carry incomes.
    #[test]
    fn in_view_share_in_range_is_exact_at_any_edge() {
        let mut rng = Rng(99);
        let rows = random_rows(&mut rng, 4_000);
        let b = Breakdowns::build(&rows, &metro_table(), BinEdges::default());
        let visible: Vec<u32> = (0..rows.len() as u32).filter(|_| rng.below(2) == 0).collect();
        let v = b.in_view(&visible, 2);
        assert_eq!(v.states.len(), 2, "only the top 2 of 4 states are kept");
        for _ in 0..200 {
            let lo = (rng.below(3) != 0).then(|| rng.below(300_000) as f64);
            let hi = (rng.below(3) != 0).then(|| rng.below(300_000) as f64);
            for iv in &v.states {
                let code = b.states[iv.group].code;
                let (mut inside, mut total) = (0.0, 0.0);
                for &i in &visible {
                    let r = &rows[i as usize];
                    let (Some(x), Some(w)) = (r.income, r.population) else { continue };
                    if r.state() != code {
                        continue;
                    }
                    total += w;
                    if lo.is_none_or(|lo| x >= lo) && hi.is_none_or(|hi| x < hi) {
                        inside += w;
                    }
                }
                let want = if total > 0.0 { inside / total } else { 0.0 };
                assert!((iv.share_in_range(lo, hi) - want).abs() < 1e-9, "state {code} [{lo:?}, {hi:?})");
            }
        }
    }

    #[test]
    fn population_csv_reads_empty_as_missing() {
        let p = parse_population("GEOID,population\n010010201001,568\n010010201002,\n010010201003,0\n");
        assert_eq!(p.get(&10_010_201_001), Some(&568.0));
        assert_eq!(p.get(&10_010_201_002), None);
        assert_eq!(p.get(&10_010_201_003), Some(&0.0));
    }

    #[test]
    fn from_texts_equals_build_on_the_same_rows() {
        let rows = vec![
            Row { geoid: 60375432021, income: Some(50_000.0), population: Some(1_000.0) },
            Row { geoid: 60375432022, income: Some(90_000.0), population: Some(3_000.0) },
            Row { geoid: 360610101001, income: None, population: Some(500.0) },
        ];
        let cbsa = "06037\t31080\tLos Angeles-Long Beach-Anaheim, CA\n";
        let a = Breakdowns::from_texts(&rows, None, cbsa, BinEdges::default()).unwrap();
        let b = Breakdowns::build(&rows, &MetroTable::parse(cbsa), BinEdges::default());
        assert_eq!(a.state_shares(Some(60_000.0), None), b.state_shares(Some(60_000.0), None));
        assert_eq!(a.excluded(), b.excluded());
    }

    /// With a population CSV the rows take their population from it, as
    /// the native demo's `load` joins them.
    #[test]
    fn from_texts_joins_the_population_csv() {
        let bare = vec![row(60375432021, Some(50_000.0), None), row(60375432022, Some(90_000.0), None)];
        let csv = "GEOID,population\n060375432021,1000\n060375432022,\n";
        let a = Breakdowns::from_texts(&bare, Some(csv), "", BinEdges::default()).unwrap();
        let joined = vec![row(60375432021, Some(50_000.0), Some(1_000.0)), row(60375432022, Some(90_000.0), None)];
        let b = Breakdowns::build(&joined, &MetroTable::default(), BinEdges::default());
        let summary = |b: &Breakdowns| b.states.iter().map(|g| (g.code, g.population_total, g.stats.clone())).collect::<Vec<_>>();
        assert_eq!(summary(&a), summary(&b));
        assert_eq!(a.excluded(), (1, 0.0));
    }

    #[test]
    fn from_texts_refuses_too_many_rows() {
        let rows = vec![row(60375432021, Some(50_000.0), Some(1.0)); MAX_ROWS + 1];
        assert!(Breakdowns::from_texts(&rows, None, "", BinEdges::default()).is_err());
        assert!(Breakdowns::from_texts(&rows[..MAX_ROWS], None, "", BinEdges::default()).is_ok());
    }

    #[test]
    fn from_texts_refuses_a_population_csv_past_the_row_bound() {
        let rows = [row(60375432021, Some(50_000.0), None)];
        let csv = format!("GEOID,population\n{}", "060375432021,1\n".repeat(MAX_ROWS + 1));
        assert!(Breakdowns::from_texts(&rows, Some(&csv), "", BinEdges::default()).is_err());
        let csv = format!("GEOID,population\n{}", "060375432021,1\n".repeat(MAX_ROWS));
        assert!(Breakdowns::from_texts(&rows, Some(&csv), "", BinEdges::default()).is_ok());
    }

    /// A GEOID past 12 digits would make a state code past 99, and enough
    /// of them would overflow the `u16` group index.
    #[test]
    fn from_texts_refuses_a_geoid_past_twelve_digits() {
        let rows = [row(60375432021, Some(50_000.0), Some(1.0)), row(1_000_000_000_000, Some(1.0), Some(1.0))];
        assert!(Breakdowns::from_texts(&rows, None, "", BinEdges::default()).is_err());
        assert!(Breakdowns::from_texts(&rows[..1], None, "", BinEdges::default()).is_ok());
        assert!(Breakdowns::from_texts(&[row(MAX_GEOID, Some(1.0), Some(1.0))], None, "", BinEdges::default()).is_ok());
    }

    /// Distinct metros, one per line: past the bound would near the `u16`
    /// group index.
    #[test]
    fn from_texts_refuses_a_cbsa_table_past_the_line_bound() {
        let rows = [row(60375432021, Some(50_000.0), Some(1.0))];
        let tsv = |n: usize| (0..n).map(|i| format!("{}\t{}\tMetro {i}\n", 10_000 + i, 10_000 + i)).collect::<String>();
        assert!(Breakdowns::from_texts(&rows, None, &tsv(MAX_CBSA_LINES + 1), BinEdges::default()).is_err());
        assert!(Breakdowns::from_texts(&rows, None, &tsv(MAX_CBSA_LINES), BinEdges::default()).is_ok());
    }

    /// The real tables, when the fetch script has run: prints the samples
    /// and the build cost. `cargo test --lib --release
    /// -- --ignored --nocapture real_census`.
    #[test]
    #[ignore = "needs demos/data/fetch_us_block_groups.sh's cache"]
    fn real_census_breakdowns() {
        let dir = std::env::var_os("US_BG_CACHE").map_or_else(
            || std::path::PathBuf::from(std::env::var("HOME").expect("HOME")).join(".cache/gpui-map/demos/us-block-groups"),
            std::path::PathBuf::from,
        );
        let incomes = std::fs::read_to_string(dir.join("acs/income_bg_2023.csv")).expect("income csv");
        let features: Vec<(u64, Option<f64>)> = incomes
            .lines()
            .skip(1)
            .filter_map(|l| {
                let (g, i) = l.split_once(',')?;
                Some((g.parse().ok()?, i.trim().parse().ok()))
            })
            .collect();
        let population = std::fs::read_to_string(dir.join("acs/population_bg_2023.csv")).expect("population csv");
        let cbsa = std::fs::read_to_string(dir.join("cbsa/county_metro_2023.tsv")).expect("cbsa tsv");
        let started = crate::timing::Instant::now();
        let rows: Vec<Row> = features.iter().map(|&(geoid, income)| Row { geoid, income, population: None }).collect();
        let b = Breakdowns::from_texts(&rows, Some(&population), &cbsa, BinEdges::default()).expect("tables");
        let took = started.elapsed();
        let rows = features.len();
        println!("breakdowns: {rows} block groups, {} states, {} metros in {:.1} ms", b.states.len(), b.metros.len(), took.as_secs_f64() * 1e3);
        let line = |g: &Group| {
            let s = &g.stats;
            println!(
                "  {:<48} pop {:>11.0} (weighted {:>11.0}, {:>6} bg, {:>4} excluded)  median {:>7.0}  IQR {:>7.0} ({:.0}-{:.0})  gini {:.3}",
                g.name, g.population_total, s.population, s.block_groups, s.excluded_block_groups, s.median, s.iqr(), s.p25, s.p75, s.gini
            );
        };
        println!("top states by population:");
        let mut st: Vec<&Group> = b.states.iter().collect();
        st.sort_by(|a, b| b.population_total.total_cmp(&a.population_total));
        st.iter().take(5).for_each(|g| line(g));
        println!("top states by median:");
        b.states_by_median().into_iter().take(5).for_each(line);
        println!("top metros by population:");
        b.top_metros(TOP_METROS).iter().for_each(line);
        let started = crate::timing::Instant::now();
        let shares = (b.state_shares(Some(100_000.0), Some(150_000.0)), b.metro_shares(Some(100_000.0), Some(150_000.0)));
        println!("shares in [100k, 150k): {} groups in {:.3} ms", shares.0.len() + shares.1.len(), started.elapsed().as_secs_f64() * 1e3);
        let all: Vec<u32> = (0..rows as u32).collect();
        let started = crate::timing::Instant::now();
        let v = b.in_view(&all, 5);
        println!("in view (all {} bg): {:.1} ms", v.block_groups, started.elapsed().as_secs_f64() * 1e3);
        assert_eq!(b.states.len(), 52, "50 states, DC and PR");
        // 393 metropolitan areas in the July 2023 delineation: a CBSA parse
        // regression the states count cannot see.
        assert!(b.metros.len() > 350, "{} metros", b.metros.len());
        assert!(took.as_millis() < 100 || cfg!(debug_assertions), "build budget: {took:?}");
    }
}
