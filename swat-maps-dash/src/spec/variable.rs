//! Variables (§3.5) and filter bindings (§3.6).

use serde::{Deserialize, Serialize};

use super::{Localised, SqlText};

/// A variable's value shape and its control (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Single,
    Multi,
    Range,
    DateRange,
    Slider,
    Search,
    Hidden,
}

impl Control {
    /// The operator a filter on this control uses when it names none.
    pub fn default_op(self) -> Op {
        match self {
            Control::Single | Control::Slider | Control::Hidden => Op::Eq,
            Control::Multi => Op::In,
            Control::Range | Control::DateRange => Op::Between,
            Control::Search => Op::Contains,
        }
    }

    pub fn is_range(self) -> bool {
        matches!(self, Control::Range | Control::DateRange)
    }
}

/// `required`: `false`, `true` (strict) or `"one_value"` (§4.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Required {
    #[default]
    No,
    /// One value bound with `=`; a `multi` never satisfies it.
    Strict,
    /// The above, or a `multi` holding exactly one element.
    OneValue,
}

impl Serialize for Required {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Required::No => s.serialize_bool(false),
            Required::Strict => s.serialize_bool(true),
            Required::OneValue => s.serialize_str("one_value"),
        }
    }
}

impl<'de> Deserialize<'de> for Required {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(false) => Ok(Required::No),
            serde_json::Value::Bool(true) => Ok(Required::Strict),
            serde_json::Value::String(s) if s == "one_value" => Ok(Required::OneValue),
            other => Err(serde::de::Error::custom(format!(
                "required must be false, true or \"one_value\", not {other}"
            ))),
        }
    }
}

/// One entry of `variables` (§3.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variable {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Localised>,
    pub control: Control,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// The bound parameter type, ClickHouse-style (`String`, `Int8`, …). When
    /// absent it is inferred from the options query's first column, wrappers
    /// included (`LowCardinality(Nullable(String))`), else `String`.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_sql: Option<SqlText>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "is_not_required")]
    pub required: Required,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<[f64; 2]>,
}

fn is_not_required(r: &Required) -> bool {
    *r == Required::No
}

/// A filter operator (§3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    In,
    Between,
    Overlaps,
    Ge,
    Le,
    Contains,
}

/// Which slots a filter reaches (§4.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clause {
    #[default]
    Where,
    Having,
}

/// One entry of `filters` (§3.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterBinding {
    pub var: String,
    /// A column, checked by [`super::is_column_identifier`]. For `overlaps`
    /// on a server source it names the interval's base: the predicate reads
    /// `<column>_lo` and `<column>_hi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub normalized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<Op>,
    #[serde(default, skip_serializing_if = "is_where")]
    pub clause: Clause,
}

fn is_where(c: &Clause) -> bool {
    *c == Clause::Where
}
