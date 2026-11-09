//! Pure widget models (spec §6.1 "model, then paint"). Each takes the
//! hooked cells a widget reads and returns what its painter draws, so the
//! rules (Other past twelve slices, suppression in totals, natural axis
//! order, the five box numbers) are tested without a window.

use std::collections::BTreeMap;

use crate::hook::{Cell, Results};
use crate::sql::Scalar;

/// A cell as a number: `Some` for a numeric value, `None` otherwise.
pub fn num(c: &Cell) -> Option<f64> {
    match c {
        Cell::Value(v) => v.as_f64().or_else(|| v.to_text().parse().ok()),
        _ => None,
    }
}

/// A cell as display text, without a format.
pub fn text(c: &Cell, suppressed: &str) -> String {
    match c {
        Cell::Value(v) => v.to_text(),
        Cell::Null => String::new(),
        Cell::Suppressed => suppressed.to_string(),
    }
}

/// Groups digits in threes.
pub fn thousands(v: i128) -> String {
    let s = v.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if v < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Formats a cell by a §3.7 format token (`integer`, `decimal:N`,
/// `percent:N`, `compact`, `code`, `date`); a suppressed cell reads as the
/// hook's label, never 0 and never blank (§5.2).
pub fn format_cell(c: &Cell, token: Option<&str>, suppressed: &str) -> String {
    let n = match c {
        Cell::Suppressed => return suppressed.to_string(),
        Cell::Null => return "–".to_string(),
        Cell::Value(Scalar::Str(s)) if token.is_none() || token == Some("code") => return s.clone(),
        Cell::Value(v) => match num(c) {
            Some(n) => n,
            None => return v.to_text(),
        },
    };
    format_number(n, token)
}

/// Formats a number by a format token; no token formats integers grouped
/// and fractions to two places.
pub fn format_number(n: f64, token: Option<&str>) -> String {
    let (kind, digits) = match token.and_then(|t| t.split_once(':')) {
        Some((k, d)) => (k, d.parse::<usize>().unwrap_or(0)),
        None => (token.unwrap_or(""), 0),
    };
    let grouped = |x: f64, d: usize| {
        let r = format!("{:.*}", d, x.abs());
        let (int, frac) = r.split_once('.').map_or((r.as_str(), ""), |(i, f)| (i, f));
        let int = thousands(int.parse::<i128>().unwrap_or(0));
        let sign = if x < 0.0 && r.chars().any(|c| c.is_ascii_digit() && c != '0') { "-" } else { "" };
        if frac.is_empty() {
            format!("{sign}{int}")
        } else {
            format!("{sign}{int}.{frac}")
        }
    };
    match kind {
        "integer" => grouped(n.round(), 0),
        "decimal" => grouped(n, digits),
        "percent" => format!("{}%", grouped(n * 100.0, digits)),
        "compact" => {
            let a = n.abs();
            if a >= 1e6 {
                format!("{:.1}M", n / 1e6)
            } else if a >= 1e3 {
                format!("{:.1}k", n / 1e3)
            } else {
                grouped(n, 0)
            }
        }
        "code" => n.to_string(),
        _ if n.fract() == 0.0 => grouped(n, 0),
        _ => grouped(n, 2),
    }
}

/// Orders labels naturally: a leading number sorts numerically ("6-11"
/// before "50-59", "2 stop" before "10 stop"), and "N/A", "不明" and
/// "Other" sort last.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn last(s: &str) -> bool {
        matches!(s, "N/A" | "不明" | "Other" | "その他" | "")
    }
    fn lead(s: &str) -> Option<f64> {
        let t = s.trim_start_matches('-');
        let end = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
        let n: f64 = t[..end].parse().ok()?;
        Some(if s.starts_with('-') && end > 0 { -n } else { n })
    }
    last(a).cmp(&last(b)).then_with(|| match (lead(a), lead(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.cmp(b)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    })
}

/// `(label, cell)` pairs from two columns of a result.
pub fn pairs(r: &Results, label: &str, value: &str) -> Vec<(String, Cell)> {
    let (Some(l), Some(v)) = (r.column(label), r.column(value)) else { return Vec::new() };
    (0..r.row_count()).map(|i| (text(&l.cell(i), r.suppressed_label()), v.cell(i))).collect()
}

/// One pie slice.
#[derive(Debug, Clone, PartialEq)]
pub struct Slice {
    pub label: String,
    /// The value; 0 for a suppressed slice, which draws no wedge.
    pub value: f64,
    pub suppressed: bool,
    /// The share of the drawn total, 0–1.
    pub share: f64,
    /// True for the grouped "Other" slice.
    pub other: bool,
}

/// Slices largest first. Past `max` slices (12, §6.2), the first `max - 1`
/// stay and the rest become one "Other" slice, which is suppressed if any
/// slice it absorbs is.
pub fn pie(items: Vec<(String, Cell)>, max: usize, other_label: &str) -> Vec<Slice> {
    let mut items: Vec<(String, Option<f64>, bool)> =
        items.into_iter().map(|(l, c)| (l, num(&c), c == Cell::Suppressed)).collect();
    items.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| b.1.unwrap_or(0.0).total_cmp(&a.1.unwrap_or(0.0))));
    let mut out: Vec<Slice> = Vec::new();
    let keep = if items.len() > max { max.saturating_sub(1) } else { items.len() };
    for (label, v, s) in items.iter().take(keep) {
        out.push(Slice { label: label.clone(), value: v.unwrap_or(0.0), suppressed: *s, share: 0.0, other: false });
    }
    if items.len() > keep {
        let rest = &items[keep..];
        let suppressed = rest.iter().any(|r| r.2);
        let value = if suppressed { 0.0 } else { rest.iter().filter_map(|r| r.1).sum() };
        out.push(Slice { label: other_label.to_string(), value, suppressed, share: 0.0, other: true });
    }
    let total: f64 = out.iter().map(|s| s.value).sum();
    for s in &mut out {
        s.share = if total > 0.0 { s.value / total } else { 0.0 };
    }
    out
}

/// A stacked bar: categories × series, long form pivoted to wide.
#[derive(Debug, Clone, PartialEq)]
pub struct Stack {
    pub xs: Vec<String>,
    pub series: Vec<String>,
    /// `cells[x][s]`; a missing pair is 0.
    pub cells: Vec<Vec<Cell>>,
    /// Per x: the sum, `Suppressed` if any of its cells is (§5.2).
    pub totals: Vec<Cell>,
    /// The largest drawn total.
    pub max: f64,
}

/// Pivots `(x, series, value)` rows. X keeps natural order; series sort
/// naturally (age groups by their leading number, "N/A" last).
pub fn stack(rows: Vec<(String, String, Cell)>) -> Stack {
    let mut xs: Vec<String> = Vec::new();
    let mut series: Vec<String> = Vec::new();
    for (x, s, _) in &rows {
        if !xs.contains(x) {
            xs.push(x.clone());
        }
        if !series.contains(s) {
            series.push(s.clone());
        }
    }
    xs.sort_by(|a, b| natural_cmp(a, b));
    series.sort_by(|a, b| natural_cmp(a, b));
    let zero = Cell::Value(Scalar::Int(0));
    let mut cells = vec![vec![zero; series.len()]; xs.len()];
    for (x, s, c) in rows {
        let (Some(i), Some(j)) = (xs.iter().position(|v| *v == x), series.iter().position(|v| *v == s)) else { continue };
        cells[i][j] = c;
    }
    let totals: Vec<Cell> = cells
        .iter()
        .map(|row| {
            if row.contains(&Cell::Suppressed) {
                Cell::Suppressed
            } else {
                Cell::Value(Scalar::Float(row.iter().filter_map(num).sum()))
            }
        })
        .collect();
    let max = cells.iter().map(|row| row.iter().filter_map(num).sum::<f64>()).fold(0.0, f64::max);
    Stack { xs, series, cells, totals, max }
}

/// A pivot (OD matrix).
#[derive(Debug, Clone, PartialEq)]
pub struct Pivot {
    pub rows: Vec<String>,
    pub cols: Vec<String>,
    /// Sparse cells by (row, col) index.
    pub cells: BTreeMap<(usize, usize), Cell>,
    /// Totals as the SQL totals queries returned them (§5.2a); `None` when
    /// a key has no total row.
    pub row_totals: Vec<Option<Cell>>,
    pub col_totals: Vec<Option<Cell>>,
    pub grand: Option<Cell>,
    /// The largest visible cell, for heat shading.
    pub max: f64,
}

impl Pivot {
    pub fn cell(&self, r: usize, c: usize) -> Option<&Cell> {
        self.cells.get(&(r, c))
    }

    /// Heat 0–1 of a cell; suppressed and empty cells have none.
    pub fn heat(&self, r: usize, c: usize) -> Option<f64> {
        let v = num(self.cell(r, c)?)?;
        (self.max > 0.0).then(|| (v / self.max).clamp(0.0, 1.0))
    }
}

/// Builds a pivot from sparse `(row, col, value)` cells and the SQL totals.
/// Axes are the union of the cells' keys and the totals' keys, in natural
/// order. A duplicate `(row, col)` keeps the first value.
pub fn pivot(
    cells: Vec<(String, String, Cell)>,
    row_totals: Vec<(String, Cell)>,
    col_totals: Vec<(String, Cell)>,
    grand: Option<Cell>,
) -> Pivot {
    let mut rows: Vec<String> = cells.iter().map(|c| c.0.clone()).chain(row_totals.iter().map(|t| t.0.clone())).collect();
    let mut cols: Vec<String> = cells.iter().map(|c| c.1.clone()).chain(col_totals.iter().map(|t| t.0.clone())).collect();
    rows.sort_by(|a, b| natural_cmp(a, b));
    rows.dedup();
    cols.sort_by(|a, b| natural_cmp(a, b));
    cols.dedup();
    let ri: BTreeMap<&str, usize> = rows.iter().enumerate().map(|(i, r)| (r.as_str(), i)).collect();
    let ci: BTreeMap<&str, usize> = cols.iter().enumerate().map(|(i, c)| (c.as_str(), i)).collect();
    let mut out = BTreeMap::new();
    let mut max: f64 = 0.0;
    for (r, c, v) in cells {
        if let Some(n) = num(&v) {
            max = max.max(n);
        }
        out.entry((ri[r.as_str()], ci[c.as_str()])).or_insert(v);
    }
    let mut rt = vec![None; rows.len()];
    for (k, v) in row_totals {
        rt[ri[k.as_str()]] = Some(v);
    }
    let mut ct = vec![None; cols.len()];
    for (k, v) in col_totals {
        ct[ci[k.as_str()]] = Some(v);
    }
    Pivot { rows, cols, cells: out, row_totals: rt, col_totals: ct, grand, max }
}

/// One box of a box plot.
#[derive(Debug, Clone, PartialEq)]
pub struct BoxStat {
    pub label: String,
    /// min, q1, median, q3, max.
    pub five: [f64; 5],
}

/// Boxes from a result with the category column `x` and the columns `min`,
/// `q1`, `median`, `q3` and `max`; a row missing any is skipped.
pub fn boxes(r: &Results, x: &str) -> Vec<BoxStat> {
    let names = ["min", "q1", "median", "q3", "max"];
    let Some(lab) = r.column(x) else { return Vec::new() };
    let cols: Vec<_> = names.iter().filter_map(|n| r.column(n)).collect();
    if cols.len() != 5 {
        return Vec::new();
    }
    (0..r.row_count())
        .filter_map(|i| {
            let mut five = [0.0; 5];
            for (k, c) in cols.iter().enumerate() {
                five[k] = num(&c.cell(i))?;
            }
            Some(BoxStat { label: text(&lab.cell(i), r.suppressed_label()), five })
        })
        .collect()
}

/// The hover text of a box: the label and the five numbers.
pub fn box_tooltip(b: &BoxStat) -> String {
    let f = |v: f64| format_number(v, Some("decimal:1"));
    format!(
        "{}\nmax {}\nQ3 {}\nmedian {}\nQ1 {}\nmin {}",
        b.label,
        f(b.five[4]),
        f(b.five[3]),
        f(b.five[2]),
        f(b.five[1]),
        f(b.five[0])
    )
}

/// The y extent of a set of boxes, padded to start at 0 when all are
/// positive.
pub fn box_extent(b: &[BoxStat]) -> (f64, f64) {
    let lo = b.iter().map(|b| b.five[0]).fold(f64::INFINITY, f64::min).min(0.0);
    let hi = b.iter().map(|b| b.five[4]).fold(f64::NEG_INFINITY, f64::max);
    if hi.is_finite() && hi > lo {
        (lo, hi)
    } else {
        (0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(n: i64) -> Cell {
        Cell::Value(Scalar::Int(n))
    }

    #[test]
    fn a_pie_past_twelve_slices_groups_other() {
        let items: Vec<(String, Cell)> = (1..=15).map(|i| (format!("c{i}"), v(i))).collect();
        let s = pie(items, 12, "Other");
        assert_eq!(s.len(), 12);
        assert_eq!(s[0].label, "c15");
        let other = s.last().unwrap();
        assert!(other.other);
        assert_eq!(other.value, (1..=4).sum::<i64>() as f64, "the four smallest");
        assert!((s.iter().map(|s| s.share).sum::<f64>() - 1.0).abs() < 1e-9);
        let twelve: Vec<(String, Cell)> = (1..=12).map(|i| (format!("c{i}"), v(i))).collect();
        assert!(pie(twelve, 12, "Other").iter().all(|s| !s.other));
    }

    #[test]
    fn other_holding_a_suppressed_slice_is_suppressed() {
        let mut items: Vec<(String, Cell)> = (1..=13).map(|i| (format!("c{i}"), v(100 + i))).collect();
        items.push(("small".into(), Cell::Suppressed));
        let s = pie(items, 12, "Other");
        assert!(s.last().unwrap().suppressed);
    }

    #[test]
    fn stack_sorts_age_groups_by_leading_number_and_suppression_reaches_the_total() {
        let st = stack(vec![
            ("8".into(), "50-59".into(), v(5)),
            ("8".into(), "6-11".into(), v(3)),
            ("-1".into(), "N/A".into(), v(2)),
            ("17".into(), "6-11".into(), Cell::Suppressed),
        ]);
        assert_eq!(st.series, ["6-11", "50-59", "N/A"]);
        assert_eq!(st.xs, ["-1", "8", "17"]);
        assert_eq!(num(&st.totals[1]), Some(8.0));
        assert_eq!(st.totals[2], Cell::Suppressed);
    }

    #[test]
    fn pivot_totals_come_from_sql_not_from_cells() {
        let p = pivot(
            vec![("A".into(), "B".into(), v(12)), ("A".into(), "C".into(), Cell::Suppressed)],
            vec![("A".into(), v(19)), ("Z".into(), v(4))],
            vec![("B".into(), v(12))],
            Some(v(23)),
        );
        assert_eq!(p.rows, ["A", "Z"]);
        assert_eq!(p.row_totals[0], Some(v(19)), "the SQL total, though one cell is suppressed");
        assert_eq!(p.cell(0, 1), Some(&Cell::Suppressed));
        assert_eq!(p.heat(0, 1), None);
        assert_eq!(p.heat(0, 0), Some(1.0));
    }

    #[test]
    fn formats() {
        assert_eq!(format_number(1234567.0, Some("integer")), "1,234,567");
        assert_eq!(format_number(0.1234, Some("percent:1")), "12.3%");
        assert_eq!(format_number(1234.5, Some("decimal:2")), "1,234.50");
        assert_eq!(format_number(-42.0, None), "-42");
        assert_eq!(format_number(15300.0, Some("compact")), "15.3k");
        assert_eq!(format_cell(&Cell::Suppressed, Some("integer"), "under 10"), "under 10");
        assert!(natural_cmp("6-11", "50-59").is_lt());
        assert!(natural_cmp("N/A", "70+").is_gt());
        assert!(natural_cmp("2 X", "10 Y").is_lt());
    }

    #[test]
    fn the_box_tooltip_holds_five_numbers() {
        let b = BoxStat { label: "八丁堀 2".into(), five: [1.0, 3.0, 5.5, 8.0, 20.0] };
        let t = box_tooltip(&b);
        for n in ["1.0", "3.0", "5.5", "8.0", "20.0"] {
            assert!(t.contains(n), "{t}");
        }
    }
}
