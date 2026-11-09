//! Field-type detection (spec §4.2), after Kepler's type analyser: classify
//! on a sample, then confirm on the whole column.
//!
//! - booleans: `true`/`false` in any case; `0`/`1` only for an `is_`/`has_`
//!   column;
//! - integer vs real; a leading zero (`02134`) keeps a column a string (an
//!   identifier, as `overlay::csv` already decides);
//! - time: ISO 8601 dates and date-times, `YYYY/MM/DD hh:mm:ss`, at a 95%
//!   threshold; epoch numbers only when the column NAME says time;
//! - H3 cell ids (15 hex digits, mode 1) are flagged `h3`;
//! - a string with at most 64 distinct values is flagged categorical.

use std::collections::HashSet;

use crate::table::{Column, Field, FieldHint, FieldType, Table};

/// How many leading non-empty values the sample takes, and how many more it
/// spreads through the rest.
pub const SAMPLE_HEAD: usize = 500;
pub const SAMPLE_SPREAD: usize = 500;
/// The share of sampled values that must parse as a time.
pub const TIME_THRESHOLD: f64 = 0.95;
/// At most this many distinct values is categorical.
pub const CATEGORICAL_MAX: usize = 64;

/// The rows to sample from a column of `rows` rows whose row `i` is empty
/// when `empty(i)`: the first [`SAMPLE_HEAD`] non-empty rows, plus
/// [`SAMPLE_SPREAD`] spread evenly through the rest -- so a column that
/// turns to text late is still caught.
pub fn sample_indices(rows: usize, empty: impl Fn(usize) -> bool) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows && out.len() < SAMPLE_HEAD {
        if !empty(i) {
            out.push(i);
        }
        i += 1;
    }
    let rest = rows - i;
    if rest > 0 {
        let n = rest.min(SAMPLE_SPREAD);
        for k in 0..n {
            // Evenly spaced, the last one on the last row.
            let j = i + ((k + 1) * rest) / n - 1;
            if !empty(j) {
                out.push(j);
            }
        }
    }
    out
}

/// The type of a column named `name` from its sampled text values.
pub fn detect(
    name: &str,
    sample: &mut dyn Iterator<Item = Option<&str>>,
) -> (FieldType, Option<FieldHint>) {
    let values: Vec<&str> = sample
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if values.is_empty() {
        return (FieldType::String, None);
    }
    let lower = name.to_ascii_lowercase();
    if values
        .iter()
        .all(|v| v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false"))
    {
        return (FieldType::Boolean, None);
    }
    if (lower.starts_with("is_") || lower.starts_with("has_"))
        && values.iter().all(|v| *v == "0" || *v == "1")
    {
        return (FieldType::Boolean, None);
    }
    let all_int = values.iter().all(|v| is_integer_text(v));
    let all_num = all_int
        || values
            .iter()
            .all(|v| v.parse::<f64>().is_ok_and(f64::is_finite));
    if all_num {
        if values.iter().any(|v| has_leading_zero(v)) {
            return string_with_hint(&values);
        }
        if name_says_time(&lower) {
            let nums: Vec<f64> = values
                .iter()
                .filter_map(|v| v.parse::<f64>().ok())
                .collect();
            if nums.iter().all(|n| epoch_ms(*n).is_some()) {
                return (FieldType::Timestamp, None);
            }
        }
        return (
            if all_int {
                FieldType::Integer
            } else {
                FieldType::Real
            },
            None,
        );
    }
    let parsed: Vec<(f64, bool)> = values.iter().filter_map(|v| parse_time(v)).collect();
    if parsed.len() as f64 >= TIME_THRESHOLD * values.len() as f64 {
        let any_time = parsed.iter().any(|(_, has_time)| *has_time);
        return (
            if any_time {
                FieldType::Timestamp
            } else {
                FieldType::Date
            },
            None,
        );
    }
    if values.iter().all(|v| is_h3(v)) {
        return (FieldType::String, Some(FieldHint::H3));
    }
    string_with_hint(&values)
}

fn string_with_hint(values: &[&str]) -> (FieldType, Option<FieldHint>) {
    let distinct: HashSet<&&str> = values.iter().collect();
    let hint = (distinct.len() <= CATEGORICAL_MAX).then_some(FieldHint::Categorical);
    (FieldType::String, hint)
}

fn is_integer_text(v: &str) -> bool {
    let digits = v
        .strip_prefix('-')
        .or_else(|| v.strip_prefix('+'))
        .unwrap_or(v);
    !digits.is_empty() && digits.len() <= 18 && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `02134`, `-07`: an identifier, not a number. `0` and `0.5` are numbers.
fn has_leading_zero(v: &str) -> bool {
    let digits = v.strip_prefix('-').unwrap_or(v);
    digits.len() > 1 && digits.starts_with('0') && digits.as_bytes()[1].is_ascii_digit()
}

/// Whether a column NAME says it holds times, so a number in it may be an
/// epoch. Whole words only (`ts`, `start_time`, `epoch_ms`), so `counts`
/// or `stats` do not qualify.
fn name_says_time(lower: &str) -> bool {
    lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        matches!(
            w,
            "time" | "date" | "ts" | "epoch" | "timestamp" | "datetime"
        ) || w.ends_with("time")
            || w.ends_with("date")
    })
}

/// An epoch value read as milliseconds: seconds between 1973 and 2286, or
/// milliseconds over the same span. Anything else is not an epoch.
pub fn epoch_ms(n: f64) -> Option<f64> {
    if (1.0e8..1.0e10).contains(&n) {
        Some(n * 1000.0)
    } else if (1.0e11..1.0e13).contains(&n) {
        Some(n)
    } else {
        None
    }
}

/// An H3 cell id: 15 hex digits, mode bits 1 (so it starts with `8`).
fn is_h3(v: &str) -> bool {
    v.len() == 15 && v.starts_with('8') && v.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parses an ISO 8601 date or date-time, or `YYYY/MM/DD[ hh:mm[:ss]]`, to
/// milliseconds since 1970 UTC, and whether it had a time part. A zone
/// offset is applied; no zone is UTC.
pub fn parse_time(v: &str) -> Option<(f64, bool)> {
    let b = v.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let sep = b[4];
    if !(sep == b'-' || sep == b'/') || b[7] != sep {
        return None;
    }
    let num = |s: &[u8]| -> Option<i64> {
        std::str::from_utf8(s)
            .ok()?
            .parse::<i64>()
            .ok()
            .filter(|_| s.iter().all(u8::is_ascii_digit))
    };
    let (y, m, d) = (num(&b[0..4])?, num(&b[5..7])?, num(&b[8..10])?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    let mut ms = days as f64 * 86_400_000.0;
    let rest = &b[10..];
    if rest.is_empty() {
        return Some((ms, false));
    }
    if !(rest[0] == b'T' || rest[0] == b' ') || rest.len() < 6 || rest[3] != b':' {
        return None;
    }
    let (hh, mm) = (num(&rest[1..3])?, num(&rest[4..6])?);
    if hh > 23 || mm > 59 {
        return None;
    }
    let mut i = 6;
    let mut secs = 0.0;
    if rest.len() >= 9 && rest[6] == b':' {
        secs = num(&rest[7..9])? as f64;
        i = 9;
        if rest.get(9) == Some(&b'.') {
            let end = rest[10..]
                .iter()
                .position(|c| !c.is_ascii_digit())
                .map_or(rest.len(), |p| p + 10);
            let frac = std::str::from_utf8(&rest[9..end])
                .ok()?
                .parse::<f64>()
                .ok()?;
            secs += frac;
            i = end;
        }
    }
    ms += (hh * 3_600_000 + mm * 60_000) as f64 + secs * 1000.0;
    // `2015-01-15 19:05:39 +00:00` (the TLC taxi records): a space may
    // stand before the offset.
    let zone = rest[i..].strip_prefix(b" ").unwrap_or(&rest[i..]);
    match zone {
        [] | [b'Z'] => {}
        [s @ (b'+' | b'-'), h1, h2, b':', m1, m2] | [s @ (b'+' | b'-'), h1, h2, m1, m2] => {
            let off = num(&[*h1, *h2])? * 3_600_000 + num(&[*m1, *m2])? * 60_000;
            ms -= if *s == b'+' {
                off as f64
            } else {
                -(off as f64)
            };
        }
        _ => return None,
    }
    Some((ms, true))
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Retypes a table built straight from a reader: number columns become
/// integer or real (or a timestamp, for an epoch column named as time),
/// text columns become timestamp or date where they parse, and every
/// string column gets its hint. One pass over each column; the caller runs
/// it off the UI thread.
pub fn refine(table: &mut Table) {
    let rows = table.rows;
    for field in &mut table.fields {
        let lower = field.name.to_ascii_lowercase();
        match &field.column {
            Column::Bool(_) => field.ty = FieldType::Boolean,
            Column::Time { .. } => {}
            Column::Number(values) => {
                let integral = values
                    .iter()
                    .flatten()
                    .all(|v| v.fract() == 0.0 && v.abs() < 9.0e15);
                field.ty = if integral {
                    FieldType::Integer
                } else {
                    FieldType::Real
                };
                let any = values.iter().flatten().next().is_some();
                if any
                    && name_says_time(&lower)
                    && values.iter().flatten().all(|v| epoch_ms(*v).is_some())
                {
                    let ms = values.iter().map(|v| v.and_then(epoch_ms)).collect();
                    let raw = std::mem::replace(&mut field.column, Column::Number(Vec::new()));
                    field.column = Column::Time {
                        ms,
                        raw: Some(Box::new(raw)),
                    };
                    field.ty = FieldType::Timestamp;
                }
            }
            Column::Str { .. } => {
                let col = &field.column;
                let idx = sample_indices(rows.min(col.len()), |i| {
                    col.str_at(i).is_none_or(|s| s.trim().is_empty())
                });
                let (ty, hint) = detect(&field.name, &mut idx.iter().map(|&i| col.str_at(i)));
                match ty {
                    FieldType::Timestamp | FieldType::Date => {
                        // Confirmed on the whole column: a value that does
                        // not parse is missing, and if more than 5% miss
                        // the column stays text.
                        let ms: Vec<Option<f64>> = (0..col.len())
                            .map(|i| {
                                col.str_at(i)
                                    .and_then(|s| parse_time(s.trim()))
                                    .map(|(ms, _)| ms)
                            })
                            .collect();
                        let present = (0..col.len())
                            .filter(|&i| col.str_at(i).is_some_and(|s| !s.trim().is_empty()))
                            .count();
                        let parsed = ms.iter().flatten().count();
                        if parsed as f64 >= TIME_THRESHOLD * present as f64 {
                            let raw =
                                std::mem::replace(&mut field.column, Column::Number(Vec::new()));
                            field.column = Column::Time {
                                ms,
                                raw: Some(Box::new(raw)),
                            };
                            field.ty = ty;
                            field.hint = None;
                        } else {
                            field.ty = FieldType::String;
                            field.hint = None;
                        }
                    }
                    _ => {
                        field.ty = FieldType::String;
                        field.hint = match hint {
                            Some(FieldHint::Categorical) => {
                                let distinct = match col {
                                    Column::Str { dict, .. } => dict.len(),
                                    _ => usize::MAX,
                                };
                                (distinct <= CATEGORICAL_MAX || distinct * 100 <= rows)
                                    .then_some(FieldHint::Categorical)
                            }
                            other => other,
                        };
                    }
                }
            }
        }
    }
}

/// A field with the type `refine` would give it, for tests and loaders
/// that build tables by hand.
pub fn field(name: &str, column: Column) -> Field {
    Field {
        name: name.into(),
        ty: FieldType::String,
        hint: None,
        column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(name: &str, values: &[&str]) -> (FieldType, Option<FieldHint>) {
        detect(name, &mut values.iter().map(|v| Some(*v)))
    }

    #[test]
    fn an_iso_datetime_column_is_a_timestamp() {
        assert_eq!(
            d(
                "when",
                &[
                    "2024-01-02T03:04:05Z",
                    "2024-01-02 03:04:05.250+08:00",
                    "2024-02-29T00:00"
                ]
            )
            .0,
            FieldType::Timestamp
        );
        assert_eq!(
            parse_time("1970-01-02T00:00:00Z"),
            Some((86_400_000.0, true))
        );
        assert_eq!(parse_time("1970-01-01T08:00:00+08:00"), Some((0.0, true)));
        assert_eq!(
            parse_time("2015-01-15 19:05:39 +00:00"),
            parse_time("2015-01-15T19:05:39Z"),
            "a space before the offset"
        );
        assert_eq!(parse_time("1970-01-01 08:00:00 +08:00"), Some((0.0, true)));
        assert_eq!(parse_time("2024/03/01 12:00:00").map(|t| t.1), Some(true));
    }

    #[test]
    fn a_bare_date_column_is_a_date() {
        assert_eq!(
            d("day", &["2024-01-02", "2024-12-31", "1999/07/04"]).0,
            FieldType::Date
        );
        assert_eq!(parse_time("1970-01-01"), Some((0.0, false)));
        assert_eq!(parse_time("2000-03-01").unwrap().0, 951_868_800_000.0);
    }

    #[test]
    fn a_zip_code_with_leading_zeros_stays_a_string() {
        assert_eq!(d("zip", &["02134", "10001", "94103"]).0, FieldType::String);
        assert_eq!(d("n", &["0", "10", "-3"]).0, FieldType::Integer);
        assert_eq!(d("x", &["0.5", "10", "-3.25"]).0, FieldType::Real);
    }

    #[test]
    fn epoch_millis_is_time_only_when_the_name_says_so() {
        let ms = ["1700000000000", "1700000360000", "1700000720000"];
        assert_eq!(d("event_time", &ms).0, FieldType::Timestamp);
        assert_eq!(
            d("ts", &["1700000000", "1700000360"]).0,
            FieldType::Timestamp
        );
        assert_eq!(d("population", &ms).0, FieldType::Integer);
        assert_eq!(
            d("counts", &ms).0,
            FieldType::Integer,
            "`counts` is not a time word"
        );
        let mut t = Table {
            id: "t".into(),
            label: "t".into(),
            rows: 2,
            geometry: crate::table::Geometry::None,
            fields: vec![field(
                "pickup_time",
                Column::Number(vec![Some(1.7e12), None]),
            )],
        };
        refine(&mut t);
        assert_eq!(t.fields[0].ty, FieldType::Timestamp);
        assert!(
            matches!(&t.fields[0].column, Column::Time { ms, .. } if ms[0] == Some(1.7e12) && ms[1].is_none())
        );
    }

    #[test]
    fn a_column_that_turns_text_late_is_a_string() {
        // 10,000 dates, then text in the last rows: the head sample alone
        // would call it a date; the spread sample sees the text.
        let mut values: Vec<String> = (0..10_000)
            .map(|i| format!("2024-01-{:02}", 1 + i % 28))
            .collect();
        for v in values.iter_mut().skip(9_000) {
            *v = "unknown".into();
        }
        let col = Column::from_strs(values.iter().map(|s| Some(s.as_str())));
        let mut t = Table {
            id: "t".into(),
            label: "t".into(),
            rows: values.len(),
            geometry: crate::table::Geometry::None,
            fields: vec![field("when", col)],
        };
        refine(&mut t);
        assert_eq!(t.fields[0].ty, FieldType::String);
        let idx = sample_indices(10_000, |_| false);
        assert!(idx.len() <= SAMPLE_HEAD + SAMPLE_SPREAD && *idx.last().unwrap() == 9_999);
    }

    #[test]
    fn h3_ids_are_flagged_h3() {
        assert_eq!(
            d(
                "hex",
                &["8928308280fffff", "89283082807ffff", "8928308280bffff"]
            ),
            (FieldType::String, Some(FieldHint::H3))
        );
        assert_ne!(
            d("hex", &["not-an-h3-cell!", "8928308280fffff"]).1,
            Some(FieldHint::H3)
        );
    }

    #[test]
    fn a_low_cardinality_string_is_categorical() {
        assert_eq!(
            d("mode", &["bus", "rail", "bus", "ferry"]),
            (FieldType::String, Some(FieldHint::Categorical))
        );
        let many: Vec<String> = (0..200).map(|i| format!("id-{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(d("id", &many), (FieldType::String, None));
        assert_eq!(d("ok", &["TRUE", "false"]).0, FieldType::Boolean);
        assert_eq!(d("is_open", &["0", "1"]).0, FieldType::Boolean);
        assert_eq!(d("rank", &["0", "1"]).0, FieldType::Integer);
    }
}
