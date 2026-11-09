//! Values: what a variable holds, what a parameter binds, and the scalar a
//! result cell reads as.

use serde::Serialize;

/// One value. Integers stay integers (`Int`/`UInt` are exact over the
/// whole 64-bit range) and decimals stay exact (`Decimal` keeps the
/// unscaled integer and its scale), so nothing loses precision on its way
/// from the server to a consumer. (The precision rule follows clickpad's
/// `ch-core::value`, which keeps Int64/UInt64/Decimal exact.)
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// `value × 10^-scale`, exactly.
    Decimal { value: i128, scale: u8 },
    Str(String),
    /// Milliseconds since the Unix epoch, UTC (dates and date-times).
    Time(i64),
}

impl Scalar {
    /// The value as text, for display, parameters and exports.
    pub fn to_text(&self) -> String {
        match self {
            Scalar::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Scalar::Int(v) => v.to_string(),
            Scalar::UInt(v) => v.to_string(),
            Scalar::Float(v) => v.to_string(),
            Scalar::Decimal { value, scale } => decimal_text(*value, *scale),
            Scalar::Str(s) => s.clone(),
            Scalar::Time(ms) => ms.to_string(),
        }
    }

    /// The value as `f64` where it is a number (a decimal or a 64-bit
    /// integer beyond 2^53 rounds).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Scalar::Int(v) => Some(*v as f64),
            Scalar::UInt(v) => Some(*v as f64),
            Scalar::Float(v) => Some(*v),
            Scalar::Decimal { value, scale } => Some(*value as f64 / 10f64.powi(*scale as i32)),
            Scalar::Time(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// From a JSON scalar (a page default, an agent's value).
    pub fn from_json(v: &serde_json::Value) -> Option<Scalar> {
        Some(match v {
            serde_json::Value::Bool(b) => Scalar::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Scalar::Int(i)
                } else if let Some(u) = n.as_u64() {
                    Scalar::UInt(u)
                } else {
                    Scalar::Float(n.as_f64()?)
                }
            }
            serde_json::Value::String(s) => Scalar::Str(s.clone()),
            _ => return None,
        })
    }
}

impl Serialize for Scalar {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Scalar::Bool(b) => s.serialize_bool(*b),
            Scalar::Int(v) => s.serialize_i64(*v),
            Scalar::UInt(v) => s.serialize_u64(*v),
            Scalar::Float(v) => s.serialize_f64(*v),
            // Exact: a decimal is written as its text, never rounded.
            Scalar::Decimal { value, scale } => s.serialize_str(&decimal_text(*value, *scale)),
            Scalar::Str(v) => s.serialize_str(v),
            Scalar::Time(v) => s.serialize_i64(*v),
        }
    }
}

/// `12345, 2` → `"123.45"`.
pub fn decimal_text(value: i128, scale: u8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    let neg = value < 0;
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale + 1 - digits.len())) } else { digits };
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{int}.{frac}", if neg { "-" } else { "" })
}

/// `"-150", 2` → `"-1.50"`: a decimal point placed into exact integer
/// text (for `Decimal256`, which no native integer holds).
pub fn decimal_text_from_str(int_text: &str, scale: u8) -> String {
    let (neg, digits) = match int_text.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, int_text),
    };
    let scale = scale as usize;
    if scale == 0 {
        return int_text.to_string();
    }
    let padded = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale + 1 - digits.len())) } else { digits.to_string() };
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{int}.{frac}", if neg { "-" } else { "" })
}

/// What a variable holds (§3.5, §4.2.3).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum VarValue {
    /// Nothing chosen: no predicate, and dependants of a required variable wait.
    #[default]
    Unset,
    /// The `"all"` sentinel on a `multi`: every option, no clause.
    All,
    One(Scalar),
    Many(Vec<Scalar>),
    /// A `range`/`date_range`; an open end is `None`.
    Range { lo: Option<Scalar>, hi: Option<Scalar> },
}

impl VarValue {
    /// From a page `default` or an agent value: `"all"`, a scalar, an
    /// array, or `{ "lo": …, "hi": … }`.
    pub fn from_json(v: &serde_json::Value) -> VarValue {
        match v {
            serde_json::Value::Null => VarValue::Unset,
            serde_json::Value::String(s) if s == "all" => VarValue::All,
            serde_json::Value::Array(a) => VarValue::Many(a.iter().filter_map(Scalar::from_json).collect()),
            serde_json::Value::Object(o) if o.contains_key("lo") || o.contains_key("hi") => VarValue::Range {
                lo: o.get("lo").and_then(Scalar::from_json),
                hi: o.get("hi").and_then(Scalar::from_json),
            },
            other => Scalar::from_json(other).map_or(VarValue::Unset, VarValue::One),
        }
    }

    /// True when the value emits no predicate: unset, `"all"`, an empty set,
    /// or a range open at both ends.
    pub fn is_unset(&self) -> bool {
        match self {
            VarValue::Unset | VarValue::All => true,
            VarValue::Many(v) => v.is_empty(),
            VarValue::Range { lo, hi } => lo.is_none() && hi.is_none(),
            VarValue::One(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_are_exact_text() {
        assert_eq!(decimal_text(12345, 2), "123.45");
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(i128::MAX, 4), "17014118346046923173168730371588410.5727");
        assert_eq!(Scalar::UInt(u64::MAX).to_text(), "18446744073709551615");
    }
}
