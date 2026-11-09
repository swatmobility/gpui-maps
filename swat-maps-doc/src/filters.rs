//! Filters (spec §5): the kinds, where each one runs, and the CPU mask.
//!
//! A point source has four GPU filter slots (the engine's
//! `overlay::filter::MAX_FILTER_FIELDS`); moving a GPU filter's window is
//! one uniform write. [`allocate_slots`] decides which filters get them,
//! with Kepler's rule (`MAX_GPU_FILTERS = 4`):
//!
//! - range and time filters on a dataset drawn only by point layers take
//!   slots in creation order until four are taken;
//! - the fifth and later, every `select` and `multiSelect`, and every
//!   filter on a dataset with a non-point layer run on the CPU (the map
//!   follows on release, by a re-push of the passing rows);
//! - a filter keeps its slot while it lives; removing one hands the freed
//!   slot to the oldest CPU range or time filter of that dataset.
//!
//! [`cpu_mask`] is the CPU statement of every filter: a row passes when it
//! passes each enabled filter, and a row with no value in a filtered field
//! fails (Kepler hides it; the engine stores it as NaN).

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::doc::Filter;
use crate::table::{Column, Table};

/// GPU slots per point source.
pub const MAX_GPU_FILTERS: usize = 4;

/// Kepler's `FILTER_TYPES` that v1 draws (spec §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FilterKind {
    /// Integer and real fields: a histogram and a brush.
    #[serde(rename = "range")]
    Range,
    /// Timestamp and date fields: the time slider with playback.
    #[serde(rename = "timeRange")]
    TimeRange,
    /// Boolean fields: true, false or both.
    #[serde(rename = "select")]
    Select,
    /// Strings, whatever their cardinality: chips for the common values;
    /// the value is a set (`equals` / `in`) or a substring (`contains`).
    #[serde(rename = "multiSelect")]
    MultiSelect,
}

impl FilterKind {
    /// Whether this kind may take a GPU slot at all.
    pub fn gpu_kind(self) -> bool {
        matches!(self, FilterKind::Range | FilterKind::TimeRange)
    }
}

/// A filter's value, by kind: a closed window (range, time; time in ms
/// since 1970 UTC), one boolean (select), the chosen strings
/// (multiSelect: equals one of them), a substring (multiSelect:
/// `{"contains": "..."}`, case-insensitive), or everything (`null`: a
/// select set to both, a new multiSelect).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FilterValue {
    Range([f64; 2]),
    Bool(bool),
    Values(Vec<String>),
    Contains(Contains),
    #[default]
    All,
}

/// A text filter's substring (`{"contains": "airport"}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contains {
    pub contains: String,
}

/// Which entries of a string column's dictionary pass `value`: `None`
/// when the value is not a string test (or the empty set, "everything").
/// One pass over the dictionary, a set lookup per entry; rows then read
/// their code, so a filter costs one array index per row whatever the
/// set's size or the column's cardinality.
pub fn dict_mask(value: &FilterValue, dict: &[std::sync::Arc<str>]) -> Option<Vec<bool>> {
    match value {
        FilterValue::Values(set) if !set.is_empty() => {
            let want: std::collections::HashSet<&str> = set.iter().map(String::as_str).collect();
            Some(dict.iter().map(|d| want.contains(&**d)).collect())
        }
        FilterValue::Contains(c) => {
            let needle = c.contains.to_lowercase();
            Some(
                dict.iter()
                    .map(|d| d.to_lowercase().contains(&needle))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Where a filter runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Placement {
    /// A GPU slot (0..[`MAX_GPU_FILTERS`]) of each point source of its
    /// dataset.
    Gpu(usize),
    #[default]
    Cpu,
}

impl Placement {
    pub fn is_gpu(self) -> bool {
        matches!(self, Placement::Gpu(_))
    }
}

/// The words a CPU filter's tag explains itself with (spec §5.2).
pub const CPU_NOTE: &str = "Four filters per dataset run on the graphics card and update as you drag. This one updates when you let go.";

/// Each filter's placement, in `filters`' order (creation order).
///
/// `previous` holds the placements from before the edit (by filter id), so
/// a GPU filter keeps its slot; `gpu_capable(source)` says whether a
/// dataset is drawn only by point layers. Pure.
pub fn allocate_slots(
    filters: &[Filter],
    previous: &HashMap<String, Placement>,
    gpu_capable: impl Fn(&str) -> bool,
) -> Vec<Placement> {
    let eligible = |f: &Filter| f.kind.gpu_kind() && gpu_capable(&f.source);
    let mut out = vec![Placement::Cpu; filters.len()];
    let mut taken: HashMap<&str, BTreeSet<usize>> = HashMap::new();
    // Slots held before the edit stay where they are.
    for (i, f) in filters.iter().enumerate() {
        if let (true, Some(Placement::Gpu(s))) = (eligible(f), previous.get(&f.id)) {
            let used = taken.entry(f.source.as_str()).or_default();
            if *s < MAX_GPU_FILTERS && used.insert(*s) {
                out[i] = Placement::Gpu(*s);
            }
        }
    }
    // The rest, oldest first, into the lowest free slot of their dataset.
    for (i, f) in filters.iter().enumerate() {
        if out[i].is_gpu() || !eligible(f) {
            continue;
        }
        let used = taken.entry(f.source.as_str()).or_default();
        if let Some(free) = (0..MAX_GPU_FILTERS).find(|s| !used.contains(s)) {
            used.insert(free);
            out[i] = Placement::Gpu(free);
        }
    }
    out
}

/// Whether row `i` passes `filter` (ignoring `enabled`): a null fails.
pub fn passes(filter: &Filter, column: &Column, i: usize) -> bool {
    match (&filter.value, column) {
        (FilterValue::Range([lo, hi]), Column::Number(v) | Column::Time { ms: v, .. }) => v
            .get(i)
            .copied()
            .flatten()
            .is_some_and(|x| x >= *lo && x <= *hi),
        (FilterValue::Bool(want), Column::Bool(v)) => v.get(i).copied().flatten() == Some(*want),
        (FilterValue::Values(set), Column::Str { .. }) => column
            .str_at(i)
            .is_some_and(|s| set.is_empty() || set.iter().any(|w| w == s)),
        (FilterValue::Contains(c), Column::Str { .. }) => column
            .str_at(i)
            .is_some_and(|s| s.to_lowercase().contains(&c.contains.to_lowercase())),
        (FilterValue::All, c) => present(c, i),
        // A value of the wrong shape for its column filters nothing out
        // but nulls (a hand-edited document).
        (_, c) => present(c, i),
    }
}

fn present(column: &Column, i: usize) -> bool {
    match column {
        Column::Number(v) | Column::Time { ms: v, .. } => {
            v.get(i).copied().flatten().is_some_and(f64::is_finite)
        }
        Column::Bool(v) => v.get(i).copied().flatten().is_some(),
        Column::Str { .. } => column.str_at(i).is_some(),
    }
}

/// Which rows of `table` pass every enabled filter in `filters` (those of
/// other datasets are ignored). A filter naming a missing column passes
/// nothing: the dataset changed under the document.
pub fn cpu_mask<'a>(table: &Table, filters: impl IntoIterator<Item = &'a Filter>) -> Vec<bool> {
    let mut mask = vec![true; table.rows];
    for f in filters
        .into_iter()
        .filter(|f| f.enabled && f.source == table.id)
    {
        let Some(field) = table.field(&f.field) else {
            mask.iter_mut().for_each(|m| *m = false);
            break;
        };
        // A string test reads each row's dictionary code (`dict_mask`).
        let by_code = match &field.column {
            Column::Str { codes, dict } => dict_mask(&f.value, dict).map(|ok| (codes, ok)),
            _ => None,
        };
        if let Some((codes, ok)) = by_code {
            for (m, &c) in mask.iter_mut().zip(codes) {
                if *m && !ok.get(c as usize).copied().unwrap_or(false) {
                    *m = false;
                }
            }
            continue;
        }
        for (i, m) in mask.iter_mut().enumerate() {
            if *m && !passes(f, &field.column, i) {
                *m = false;
            }
        }
    }
    mask
}

/// The indices of the `true` rows of `mask`.
pub fn passing_rows(mask: &[bool]) -> Vec<u32> {
    mask.iter()
        .enumerate()
        .filter(|(_, m)| **m)
        .map(|(i, _)| i as u32)
        .collect()
}

/// A new filter's kind for a field of type `ty`, `None` when the field
/// cannot be filtered (a geometry). Every string takes a multiSelect,
/// whatever its cardinality: ids and names filter by `equals`, `in` and
/// `contains` as categories do.
pub fn kind_for(field: &crate::table::Field) -> Option<FilterKind> {
    use crate::table::FieldType;
    match field.ty {
        FieldType::Integer | FieldType::Real => Some(FilterKind::Range),
        FieldType::Timestamp | FieldType::Date => Some(FilterKind::TimeRange),
        FieldType::Boolean => Some(FilterKind::Select),
        FieldType::String => Some(FilterKind::MultiSelect),
        _ => None,
    }
}

/// A new filter's starting value: the field's whole domain (range, time),
/// or everything.
pub fn initial_value(field: &crate::table::Field, kind: FilterKind) -> FilterValue {
    match kind {
        FilterKind::Range | FilterKind::TimeRange => {
            field.range().map_or(FilterValue::All, FilterValue::Range)
        }
        FilterKind::Select | FilterKind::MultiSelect => FilterValue::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{Field, FieldType, Geometry};

    fn filter(id: &str, source: &str, kind: FilterKind) -> Filter {
        Filter::new(id, source, id, kind, FilterValue::All)
    }

    fn place(filters: &[Filter], prev: &HashMap<String, Placement>) -> Vec<Placement> {
        allocate_slots(filters, prev, |s| s != "polys")
    }

    fn prev_of(filters: &[Filter], placed: &[Placement]) -> HashMap<String, Placement> {
        filters
            .iter()
            .zip(placed)
            .map(|(f, p)| (f.id.clone(), *p))
            .collect()
    }

    #[test]
    fn the_fifth_range_filter_runs_on_the_cpu() {
        let filters: Vec<Filter> = (0..6)
            .map(|i| {
                filter(
                    &format!("f{i}"),
                    "quakes",
                    if i == 2 {
                        FilterKind::TimeRange
                    } else {
                        FilterKind::Range
                    },
                )
            })
            .collect();
        let placed = place(&filters, &HashMap::new());
        assert_eq!(
            placed,
            vec![
                Placement::Gpu(0),
                Placement::Gpu(1),
                Placement::Gpu(2),
                Placement::Gpu(3),
                Placement::Cpu,
                Placement::Cpu
            ]
        );
        // Another dataset has its own four.
        let mut more = filters.clone();
        more.push(filter("g0", "cities", FilterKind::Range));
        assert_eq!(
            place(&more, &prev_of(&filters, &placed))[6],
            Placement::Gpu(0)
        );
    }

    #[test]
    fn removing_a_gpu_filter_promotes_the_oldest_cpu_one() {
        let mut filters: Vec<Filter> = (0..6)
            .map(|i| filter(&format!("f{i}"), "quakes", FilterKind::Range))
            .collect();
        let placed = place(&filters, &HashMap::new());
        let prev = prev_of(&filters, &placed);
        // Remove the filter in slot 1: f4 (the oldest CPU one) takes slot
        // 1, everyone else keeps theirs, f5 stays on the CPU.
        filters.remove(1);
        let after = place(&filters, &prev);
        let by_id: HashMap<&str, Placement> = filters
            .iter()
            .map(|f| f.id.as_str())
            .zip(after.iter().copied())
            .collect();
        assert_eq!(by_id["f0"], Placement::Gpu(0));
        assert_eq!(by_id["f2"], Placement::Gpu(2));
        assert_eq!(by_id["f3"], Placement::Gpu(3));
        assert_eq!(
            by_id["f4"],
            Placement::Gpu(1),
            "the oldest CPU filter takes the freed slot"
        );
        assert_eq!(by_id["f5"], Placement::Cpu);
        // A select between them is never promoted.
        let mut mixed = vec![
            filter("a", "quakes", FilterKind::Range),
            filter("b", "quakes", FilterKind::Range),
            filter("c", "quakes", FilterKind::Range),
            filter("d", "quakes", FilterKind::Range),
            filter("s", "quakes", FilterKind::Select),
            filter("e", "quakes", FilterKind::TimeRange),
        ];
        let p = place(&mixed, &HashMap::new());
        mixed.remove(0);
        let after = place(
            &mixed,
            &prev_of(
                &[
                    filter("a", "quakes", FilterKind::Range),
                    filter("b", "quakes", FilterKind::Range),
                    filter("c", "quakes", FilterKind::Range),
                    filter("d", "quakes", FilterKind::Range),
                    filter("s", "quakes", FilterKind::Select),
                    filter("e", "quakes", FilterKind::TimeRange),
                ],
                &p,
            ),
        );
        assert_eq!(after[3], Placement::Cpu, "the select stays on the CPU");
        assert_eq!(after[4], Placement::Gpu(0), "the time filter takes slot 0");
    }

    #[test]
    fn multiselect_is_always_cpu() {
        let filters = vec![
            filter("m", "quakes", FilterKind::MultiSelect),
            filter("s", "quakes", FilterKind::Select),
            filter("r", "quakes", FilterKind::Range),
        ];
        assert_eq!(
            place(&filters, &HashMap::new()),
            vec![Placement::Cpu, Placement::Cpu, Placement::Gpu(0)]
        );
    }

    #[test]
    fn a_polygon_dataset_filters_on_the_cpu() {
        let filters = vec![filter("r", "polys", FilterKind::Range)];
        assert_eq!(place(&filters, &HashMap::new()), vec![Placement::Cpu]);
    }

    fn table() -> Table {
        let num = |v: Vec<Option<f64>>| Column::Number(v);
        Table {
            id: "t".into(),
            label: "t".into(),
            rows: 4,
            geometry: Geometry::Points(vec![[0.0, 0.0]; 4]),
            fields: vec![
                Field {
                    name: "mag".into(),
                    ty: FieldType::Real,
                    hint: None,
                    column: num(vec![Some(1.0), None, Some(5.0), Some(3.0)]),
                },
                Field {
                    name: "ok".into(),
                    ty: FieldType::Boolean,
                    hint: None,
                    column: Column::Bool(vec![Some(true), Some(true), None, Some(false)]),
                },
                Field {
                    name: "kind".into(),
                    ty: FieldType::String,
                    hint: None,
                    column: Column::from_strs([Some("a"), Some("b"), Some("a"), None]),
                },
                Field {
                    name: "t".into(),
                    ty: FieldType::Timestamp,
                    hint: None,
                    column: Column::Time {
                        ms: vec![Some(10.0), Some(20.0), None, Some(40.0)],
                        raw: None,
                    },
                },
            ],
        }
    }

    #[test]
    fn nulls_fail_every_filter() {
        let t = table();
        let all = |field: &str, kind, value| Filter::new(field, "t", field, kind, value);
        assert_eq!(
            cpu_mask(
                &t,
                &[all(
                    "mag",
                    FilterKind::Range,
                    FilterValue::Range([f64::MIN, f64::MAX])
                )]
            ),
            vec![true, false, true, true]
        );
        assert_eq!(
            cpu_mask(&t, &[all("ok", FilterKind::Select, FilterValue::All)]),
            vec![true, true, false, true]
        );
        assert_eq!(
            cpu_mask(
                &t,
                &[all(
                    "kind",
                    FilterKind::MultiSelect,
                    FilterValue::Values(vec![])
                )]
            ),
            vec![true, true, true, false]
        );
        assert_eq!(
            cpu_mask(&t, &[all("t", FilterKind::TimeRange, FilterValue::All)]),
            vec![true, true, false, true]
        );
    }

    #[test]
    fn filters_and_together_and_disabled_ones_pass() {
        let t = table();
        let mag = Filter::new(
            "m",
            "t",
            "mag",
            FilterKind::Range,
            FilterValue::Range([2.0, 6.0]),
        );
        let kind = Filter::new(
            "k",
            "t",
            "kind",
            FilterKind::MultiSelect,
            FilterValue::Values(vec!["a".into()]),
        );
        assert_eq!(cpu_mask(&t, [&mag, &kind]), vec![false, false, true, false]);
        let mut off = kind.clone();
        off.enabled = false;
        assert_eq!(cpu_mask(&t, [&mag, &off]), vec![false, false, true, true]);
        let yes = Filter::new("o", "t", "ok", FilterKind::Select, FilterValue::Bool(false));
        assert_eq!(passing_rows(&cpu_mask(&t, [&yes])), vec![3]);
        // Another dataset's filter is not this table's.
        let other = Filter::new(
            "x",
            "u",
            "mag",
            FilterKind::Range,
            FilterValue::Range([100.0, 200.0]),
        );
        assert_eq!(cpu_mask(&t, [&other]), vec![true; 4]);
    }

    #[test]
    fn a_high_cardinality_string_filters_by_set_and_by_substring() {
        // 5,000 distinct ids, none of them "categorical".
        let ids: Vec<String> = (0..5000).map(|i| format!("stop-{i}")).collect();
        let t = Table {
            id: "t".into(),
            label: "t".into(),
            rows: ids.len(),
            geometry: Geometry::Points(vec![[0.0, 0.0]; ids.len()]),
            fields: vec![Field {
                name: "id".into(),
                ty: FieldType::String,
                hint: None,
                column: Column::from_strs(ids.iter().map(|s| Some(s.as_str()))),
            }],
        };
        assert_eq!(kind_for(&t.fields[0]), Some(FilterKind::MultiSelect));
        let within = Filter::new(
            "i",
            "t",
            "id",
            FilterKind::MultiSelect,
            FilterValue::Values(vec!["stop-15".into(), "stop-20".into(), "nope".into()]),
        );
        assert_eq!(passing_rows(&cpu_mask(&t, [&within])), vec![15, 20]);
        let has = Filter::new(
            "c",
            "t",
            "id",
            FilterKind::MultiSelect,
            FilterValue::Contains(Contains {
                contains: "OP-499".into(),
            }),
        );
        assert_eq!(
            passing_rows(&cpu_mask(&t, [&has])),
            vec![499, 4990, 4991, 4992, 4993, 4994, 4995, 4996, 4997, 4998, 4999]
        );
        // The row-at-a-time statement agrees with the dictionary one.
        assert!(
            (0..t.rows).all(|i| passes(&within, &t.fields[0].column, i) == [15, 20].contains(&i))
        );
        assert!(passes(&has, &t.fields[0].column, 4990));
    }

    #[test]
    fn a_field_gets_the_kind_its_type_asks_for() {
        let t = table();
        let kinds: Vec<Option<FilterKind>> = t.fields.iter().map(kind_for).collect();
        assert_eq!(
            kinds,
            vec![
                Some(FilterKind::Range),
                Some(FilterKind::Select),
                Some(FilterKind::MultiSelect),
                Some(FilterKind::TimeRange)
            ]
        );
        assert_eq!(
            initial_value(&t.fields[0], FilterKind::Range),
            FilterValue::Range([1.0, 5.0])
        );
    }

    #[test]
    fn values_round_trip_through_json() {
        for v in [
            FilterValue::Range([1.5, 2.0]),
            FilterValue::Bool(true),
            FilterValue::Values(vec!["a".into()]),
            FilterValue::Contains(Contains {
                contains: "air".into(),
            }),
            FilterValue::All,
        ] {
            let s = serde_json::to_string(&v).unwrap();
            assert_eq!(serde_json::from_str::<FilterValue>(&s).unwrap(), v, "{s}");
        }
        assert_eq!(
            serde_json::to_string(&FilterKind::TimeRange).unwrap(),
            "\"timeRange\""
        );
    }
}
