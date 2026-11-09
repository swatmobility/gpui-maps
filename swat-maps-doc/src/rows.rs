//! The data table's pure half (spec §2.7): which rows it lists, in which
//! order, and each column's mini-stat. Everything here is a pass over a
//! column, so a host runs it off the UI thread past a few thousand rows.
//!
//! The table lists the rows that pass the dataset's filters by default
//! ("12,301 of 54,936 rows"), and can list every row. Kepler's table lists
//! every row; we start filtered so the table and the map agree, and keep
//! "All rows" one click away.

use std::cmp::Ordering;

use crate::doc::Filter;
use crate::table::{Column, Field, Table};

/// The rows the table lists: those passing every enabled filter of the
/// table's dataset, or all of them.
pub fn listed(table: &Table, filters: &[Filter], filtered_only: bool) -> Vec<u32> {
    if !filtered_only || !filters.iter().any(|f| f.enabled && f.source == table.id) {
        return (0..table.rows as u32).collect();
    }
    crate::filters::passing_rows(&crate::filters::cpu_mask(table, filters))
}

/// Sorts `rows` by `field`, ascending or descending; a row with no value
/// sorts last either way, and ties keep their order.
pub fn sort(field: &Field, rows: &mut [u32], descending: bool) {
    let dir = |o: Ordering| if descending { o.reverse() } else { o };
    match &field.column {
        Column::Number(v) | Column::Time { ms: v, .. } => {
            let key = |r: u32| v.get(r as usize).copied().flatten().filter(|x| !x.is_nan());
            rows.sort_by(|&a, &b| match (key(a), key(b)) {
                (Some(x), Some(y)) => dir(x.total_cmp(&y)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            });
        }
        Column::Bool(v) => {
            let key = |r: u32| v.get(r as usize).copied().flatten();
            rows.sort_by(|&a, &b| match (key(a), key(b)) {
                (Some(x), Some(y)) => dir(x.cmp(&y)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            });
        }
        Column::Str { codes, dict } => {
            // Rank the dictionary once; rows then compare integers.
            let mut order: Vec<u32> = (0..dict.len() as u32).collect();
            order.sort_by(|&a, &b| dict[a as usize].cmp(&dict[b as usize]));
            let mut rank = vec![0u32; dict.len()];
            for (r, &code) in order.iter().enumerate() {
                rank[code as usize] = r as u32;
            }
            let key = |r: u32| {
                codes
                    .get(r as usize)
                    .and_then(|&c| rank.get(c as usize).copied())
            };
            rows.sort_by(|&a, &b| match (key(a), key(b)) {
                (Some(x), Some(y)) => dir(x.cmp(&y)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            });
        }
    }
}

/// A column's mini-stat over `rows`.
#[derive(Clone, Debug, PartialEq)]
pub enum Stat {
    /// The smallest and largest value (ms for a time column).
    Range { min: f64, max: f64, missing: usize },
    /// How many distinct values (strings, booleans).
    Distinct { count: usize, missing: usize },
    /// No value at all.
    Empty,
}

/// `field`'s mini-stat over `rows`.
pub fn stat(field: &Field, rows: &[u32]) -> Stat {
    match &field.column {
        Column::Number(v) | Column::Time { ms: v, .. } => {
            let mut missing = 0;
            let mut range: Option<(f64, f64)> = None;
            for &r in rows {
                match v
                    .get(r as usize)
                    .copied()
                    .flatten()
                    .filter(|x| x.is_finite())
                {
                    Some(x) => range = Some(range.map_or((x, x), |(a, b)| (a.min(x), b.max(x)))),
                    None => missing += 1,
                }
            }
            range.map_or(Stat::Empty, |(min, max)| Stat::Range { min, max, missing })
        }
        Column::Bool(v) => {
            let (mut t, mut f, mut missing) = (false, false, 0);
            for &r in rows {
                match v.get(r as usize).copied().flatten() {
                    Some(true) => t = true,
                    Some(false) => f = true,
                    None => missing += 1,
                }
            }
            let count = usize::from(t) + usize::from(f);
            if count == 0 {
                Stat::Empty
            } else {
                Stat::Distinct { count, missing }
            }
        }
        Column::Str { codes, dict } => {
            let mut seen = vec![false; dict.len()];
            let (mut count, mut missing) = (0, 0);
            for &r in rows {
                match codes.get(r as usize).copied() {
                    Some(c) if (c as usize) < seen.len() => {
                        if !seen[c as usize] {
                            seen[c as usize] = true;
                            count += 1;
                        }
                    }
                    _ => missing += 1,
                }
            }
            if count == 0 {
                Stat::Empty
            } else {
                Stat::Distinct { count, missing }
            }
        }
    }
}

/// The floor under which a count is never shown as a number, and a value
/// is never named (agent chat spec §8.2, k = 10).
pub const K_FLOOR: usize = 10;

/// What a column summary reports for a count under the floor.
pub const UNDER_K: &str = "<10";

/// A count as the agent sees it: the number at or above `k`, `"<10"`
/// below (zero stays zero: "none" reveals no one).
pub fn k_count(n: usize, k: usize) -> serde_json::Value {
    if n == 0 || n >= k {
        serde_json::json!(n)
    } else {
        serde_json::json!(UNDER_K)
    }
}

/// Whether a column's name reads as an identifier of a person or a thing
/// (spec §8.2): one of its words is `id`, `uuid`, `email`, `phone`,
/// `name`, `address`, `nric`, `fin`, `passport` or `card`, or it is
/// `my_number` / `mynumber`. Words split on punctuation and camelCase, so
/// `user_id` and `userId` match and `width` does not.
pub fn identifier_name(name: &str) -> bool {
    const WORDS: [&str; 10] = [
        "id", "uuid", "email", "phone", "name", "address", "nric", "fin", "passport", "card",
    ];
    let lower = name.to_ascii_lowercase();
    let squashed: String = lower
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if squashed == "mynumber" || squashed.ends_with("mynumber") {
        return true;
    }
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    // `userid`, `tripid`: an id glued to what it identifies.
    const OWNERS: [&str; 11] = [
        "user",
        "trip",
        "event",
        "customer",
        "device",
        "vehicle",
        "person",
        "account",
        "order",
        "driver",
        "passenger",
    ];
    words.iter().any(|w| {
        WORDS.contains(&w.as_str()) || w.strip_suffix("id").is_some_and(|p| OWNERS.contains(&p))
    })
}

/// One column's summary for the agent: schema and statistics, never a
/// row (spec §8.2). Strings name their top values only when categorical
/// and only values at or above `k`; identifier columns name nothing and
/// report no extremes.
pub fn summary(field: &Field, rows: Option<&[u32]>, k: usize) -> serde_json::Value {
    use serde_json::{json, Map, Value};
    let n = rows.map_or(field.column.len(), <[u32]>::len);
    let at = |i: usize| rows.map_or(i, |r| r[i] as usize);
    let mut out = Map::new();
    out.insert("name".into(), json!(field.name));
    out.insert("type".into(), json!(field.ty.name()));
    if let Some(h) = field.hint {
        out.insert(
            "hint".into(),
            json!(match h {
                crate::table::FieldHint::H3 => "h3",
                crate::table::FieldHint::Categorical => "categorical",
            }),
        );
    }
    let named_id = identifier_name(&field.name);
    match &field.column {
        Column::Number(v) | Column::Time { ms: v, .. } => {
            let mut vals: Vec<f64> = (0..n)
                .filter_map(|i| v.get(at(i)).copied().flatten())
                .filter(|x| x.is_finite())
                .collect();
            out.insert("nulls".into(), json!(n - vals.len()));
            // A numeric id (a phone number, a card number) is an identifier
            // by its name; its extremes would be someone's value.
            if named_id {
                out.insert("identifier".into(), json!(true));
                return Value::Object(out);
            }
            if vals.is_empty() {
                return Value::Object(out);
            }
            vals.sort_by(f64::total_cmp);
            let q = |p: f64| vals[(((vals.len() - 1) as f64) * p).round() as usize];
            let is_time = matches!(field.column, Column::Time { .. });
            if is_time {
                out.insert(
                    "time".into(),
                    json!({"min": iso_utc(vals[0]), "max": iso_utc(vals[vals.len() - 1])}),
                );
            } else {
                let mean = vals.iter().sum::<f64>() / vals.len() as f64;
                let r = |x: f64| {
                    if x.abs() >= 1e6 || x.fract() == 0.0 {
                        x
                    } else {
                        (x * 1e4).round() / 1e4
                    }
                };
                out.insert("number".into(), json!({"min": r(vals[0]), "max": r(vals[vals.len() - 1]), "mean": r(mean), "p10": r(q(0.1)), "p50": r(q(0.5)), "p90": r(q(0.9))}));
            }
        }
        Column::Bool(v) => {
            let (mut t, mut f, mut missing) = (0usize, 0usize, 0usize);
            for i in 0..n {
                match v.get(at(i)).copied().flatten() {
                    Some(true) => t += 1,
                    Some(false) => f += 1,
                    None => missing += 1,
                }
            }
            out.insert("nulls".into(), json!(missing));
            out.insert(
                "boolean".into(),
                json!({"true": k_count(t, k), "false": k_count(f, k)}),
            );
        }
        Column::Str { codes, dict } => {
            let mut counts = vec![0usize; dict.len()];
            let mut missing = 0;
            for i in 0..n {
                match codes.get(at(i)).copied() {
                    Some(c) if (c as usize) < counts.len() => counts[c as usize] += 1,
                    _ => missing += 1,
                }
            }
            let distinct = counts.iter().filter(|c| **c > 0).count();
            out.insert("nulls".into(), json!(missing));
            out.insert("distinct".into(), json!(distinct));
            let present = n - missing;
            let identifier = named_id || (present >= k && distinct * 10 >= present * 9);
            if identifier {
                out.insert("identifier".into(), json!(true));
                return Value::Object(out);
            }
            if field.hint == Some(crate::table::FieldHint::Categorical) {
                let mut order: Vec<usize> = (0..counts.len()).filter(|&c| counts[c] > 0).collect();
                order.sort_by(|&a, &b| {
                    counts[b]
                        .cmp(&counts[a])
                        .then_with(|| dict[a].cmp(&dict[b]))
                });
                let top: Vec<Value> = order
                    .iter()
                    .filter(|&&c| counts[c] >= k)
                    .take(10)
                    .map(|&c| json!({"value": &*dict[c], "count": counts[c]}))
                    .collect();
                let under = order.iter().filter(|&&c| counts[c] < k).count();
                out.insert("top".into(), Value::Array(top));
                if under > 0 {
                    out.insert("under_k".into(), json!({"values": under, "count_each": UNDER_K, "note": "values with fewer than 10 rows are not named"}));
                }
            }
        }
    }
    Value::Object(out)
}

/// `ms` since 1970 UTC as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso_utc(ms: f64) -> String {
    let secs = (ms / 1000.0).floor() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::field;
    use crate::filters::{FilterKind, FilterValue};
    use crate::table::Geometry;

    fn table() -> Table {
        Table {
            id: "t".into(),
            label: "t".into(),
            rows: 5,
            geometry: Geometry::Points(vec![[0.0, 0.0]; 5]),
            fields: vec![
                field(
                    "mag",
                    Column::Number(vec![Some(3.0), None, Some(1.0), Some(5.0), Some(1.0)]),
                ),
                field(
                    "kind",
                    Column::from_strs([Some("b"), Some("a"), None, Some("c"), Some("a")]),
                ),
            ],
        }
    }

    #[test]
    fn nulls_sort_last_both_ways_and_ties_keep_order() {
        let t = table();
        let mut rows: Vec<u32> = (0..5).collect();
        sort(&t.fields[0], &mut rows, false);
        assert_eq!(rows, [2, 4, 0, 3, 1]);
        sort(&t.fields[0], &mut rows, true);
        assert_eq!(rows, [3, 0, 2, 4, 1]);
        let mut rows: Vec<u32> = (0..5).collect();
        sort(&t.fields[1], &mut rows, false);
        assert_eq!(rows, [1, 4, 0, 3, 2], "by text, not by dictionary code");
    }

    #[test]
    fn the_filtered_list_is_what_passes() {
        let t = table();
        let f = Filter::new(
            "f",
            "t",
            "mag",
            FilterKind::Range,
            FilterValue::Range([1.0, 3.0]),
        );
        assert_eq!(listed(&t, std::slice::from_ref(&f), true), [0, 2, 4]);
        assert_eq!(
            listed(&t, std::slice::from_ref(&f), false).len(),
            5,
            "All rows lists every row"
        );
        let mut off = f.clone();
        off.enabled = false;
        assert_eq!(
            listed(&t, &[off], true).len(),
            5,
            "a disabled filter hides nothing"
        );
    }

    #[test]
    fn summary_suppresses_values_under_k_and_flags_identifiers() {
        use crate::table::{FieldHint, FieldType};
        // 30 rows: "a" x 20, "b" x 9, and one row holding a sentinel.
        let mut values: Vec<&str> = vec!["a"; 20];
        values.extend(vec!["b"; 9]);
        values.push("SENTINEL-ROW-VALUE");
        let mut kind = field("kind", Column::from_strs(values.iter().map(|v| Some(*v))));
        kind.hint = Some(FieldHint::Categorical);
        let s = summary(&kind, None, K_FLOOR);
        let text = s.to_string();
        assert_eq!(s["top"], serde_json::json!([{"value": "a", "count": 20}]));
        assert_eq!(s["under_k"]["values"], 2);
        assert!(
            !text.contains("SENTINEL") && !text.contains("\"b\""),
            "values under k are never named: {text}"
        );
        assert_eq!(s["distinct"], 3);
        // Unique strings: an identifier by distinctness, nothing named.
        let names: Vec<String> = (0..30).map(|i| format!("person-{i}")).collect();
        let who = field(
            "label",
            Column::from_strs(names.iter().map(|v| Some(v.as_str()))),
        );
        let s = summary(&who, None, K_FLOOR);
        assert_eq!(s["identifier"], true);
        assert!(s.get("top").is_none() && !s.to_string().contains("person-"));
        // By name: a numeric phone column reports no extremes.
        let mut phone = field(
            "Phone",
            Column::Number(
                (0..30)
                    .map(|i| Some(6_590_000_000.0 + f64::from(i)))
                    .collect(),
            ),
        );
        phone.ty = FieldType::Integer;
        let s = summary(&phone, None, K_FLOOR);
        assert_eq!(s["identifier"], true);
        assert!(s.get("number").is_none(), "{s}");
        for (name, id) in [
            ("user_id", true),
            ("userId", true),
            ("id", true),
            ("email", true),
            ("my_number", true),
            ("width", false),
            ("final", false),
            ("mag", false),
            ("place", false),
            ("depth", false),
        ] {
            assert_eq!(identifier_name(name), id, "{name}");
        }
        // Numbers: quantiles over the listed rows only.
        let mag = field(
            "mag",
            Column::Number((1..=10).map(|i| Some(f64::from(i))).collect()),
        );
        let s = summary(&mag, Some(&[0, 1, 2, 3, 4]), K_FLOOR);
        assert_eq!(s["number"]["min"], 1.0);
        assert_eq!(s["number"]["max"], 5.0);
        assert_eq!(s["number"]["p50"], 3.0);
        assert_eq!(k_count(3, K_FLOOR), serde_json::json!("<10"));
        assert_eq!(k_count(0, K_FLOOR), serde_json::json!(0));
        assert_eq!(iso_utc(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_700_000_000_000.0), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn stats_count_over_the_listed_rows() {
        let t = table();
        assert_eq!(
            stat(&t.fields[0], &[0, 1, 2, 3, 4]),
            Stat::Range {
                min: 1.0,
                max: 5.0,
                missing: 1
            }
        );
        assert_eq!(
            stat(&t.fields[1], &[0, 1, 2, 3, 4]),
            Stat::Distinct {
                count: 3,
                missing: 1
            }
        );
        assert_eq!(
            stat(&t.fields[1], &[1, 4]),
            Stat::Distinct {
                count: 1,
                missing: 0
            }
        );
        assert_eq!(stat(&t.fields[0], &[1]), Stat::Empty);
    }
}
