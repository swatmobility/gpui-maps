//! What differs between SQL dialects: parameter syntax and types, the NFKC
//! column expression, identifier quoting, `IN`, and reducers (§4.2.2,
//! §4.2.3a, §4.3.2; Postgres is explorer-sql D1).

use serde::{Deserialize, Serialize};

/// A SQL dialect. The dialect decides the parameter syntax; the transport
/// is the connection's business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// Native parameters `{p_name:Type}`, sent as `param_p_name` (§8.2).
    ClickHouse,
    /// Positional `$1…$n` with a typed parameter list (D1).
    Postgres,
}

impl Dialect {
    /// The column side of a `contains` match (§4.2.3a).
    pub fn nfkc_column(self, col: &str) -> String {
        match self {
            Dialect::ClickHouse => format!("normalizeUTF8NFKC({col})"),
            Dialect::Postgres => format!("normalize({col}, NFKC)"),
        }
    }

    /// The `contains` predicate over an already-normalised column expression.
    pub fn contains(self, col_expr: &str, param: &str) -> String {
        match self {
            Dialect::ClickHouse => format!("position({col_expr}, {param}) > 0"),
            Dialect::Postgres => format!("strpos({col_expr}, {param}) > 0"),
        }
    }

    /// The `in` predicate over an array parameter.
    pub fn in_array(self, col: &str, param: &str) -> String {
        match self {
            Dialect::ClickHouse => format!("{col} IN {param}"),
            Dialect::Postgres => format!("{col} = ANY({param})"),
        }
    }

    /// Quotes an identifier that is not a plain one (D1: `"` in Postgres).
    pub fn quote_ident(self, s: &str) -> String {
        match self {
            Dialect::ClickHouse => format!("`{}`", s.replace('\\', "\\\\").replace('`', "\\`")),
            Dialect::Postgres => format!("\"{}\"", s.replace('"', "\"\"")),
        }
    }

    /// The parameter type for a ClickHouse-style declared type. ClickHouse
    /// takes it as declared, wrappers included
    /// (`LowCardinality(Nullable(String))`); Postgres gets the nearest
    /// built-in type, wrappers dropped.
    pub fn param_type(self, declared: &str) -> String {
        match self {
            Dialect::ClickHouse => declared.to_string(),
            Dialect::Postgres => pg_type(declared),
        }
    }

    /// The array type of `declared` for a `multi`.
    pub fn array_type(self, declared: &str) -> String {
        match self {
            Dialect::ClickHouse => format!("Array({declared})"),
            Dialect::Postgres => format!("{}[]", pg_type(declared)),
        }
    }

    /// A reducer call (§4.3.2) in this dialect, or `None` when the dialect
    /// has no SQL form for it (`reducer_unsupported_by_dialect`).
    /// `args` are already-checked identifiers or numeric literals.
    pub fn reducer(self, name: &str, args: &[&str]) -> Option<String> {
        let ch = self == Dialect::ClickHouse;
        Some(match (name, args) {
            ("count", []) => if ch { "count()" } else { "count(*)" }.to_string(),
            ("count_distinct", [c]) => if ch { format!("uniqExact({c})") } else { format!("count(DISTINCT {c})") },
            ("sum", [c]) => format!("sum({c})"),
            ("mean", [c]) => format!("avg({c})"),
            ("min", [c]) => format!("min({c})"),
            ("max", [c]) => format!("max({c})"),
            ("median", [c]) => {
                if ch {
                    format!("quantileExact(0.5)({c})")
                } else {
                    format!("percentile_cont(0.5) WITHIN GROUP (ORDER BY {c})")
                }
            }
            ("quantile", [c, p]) => {
                if ch {
                    format!("quantileExact({p})({c})")
                } else {
                    format!("percentile_cont({p}) WITHIN GROUP (ORDER BY {c})")
                }
            }
            ("wquantile" | "wmedian", _) if !ch => return None,
            ("wmedian", [c, w]) => format!("quantileExactWeighted(0.5)({c}, {w})"),
            ("wquantile", [c, w, p]) => format!("quantileExactWeighted({p})({c}, {w})"),
            _ => return None,
        })
    }
}

/// Unwraps `LowCardinality(…)` and `Nullable(…)`.
pub fn strip_wrappers(t: &str) -> &str {
    let mut t = t.trim();
    loop {
        let inner = ["LowCardinality(", "Nullable("]
            .iter()
            .find_map(|w| t.strip_prefix(w).and_then(|r| r.strip_suffix(')')));
        match inner {
            Some(i) => t = i.trim(),
            None => return t,
        }
    }
}

fn pg_type(declared: &str) -> String {
    let t = strip_wrappers(declared);
    if let Some(inner) = t.strip_prefix("Array(").and_then(|r| r.strip_suffix(')')) {
        return format!("{}[]", pg_type(inner));
    }
    let head = t.split('(').next().unwrap_or(t);
    match head {
        "String" | "FixedString" | "Enum8" | "Enum16" => "text",
        "Int8" | "Int16" | "UInt8" => "int2",
        "Int32" | "UInt16" => "int4",
        "Int64" | "UInt32" => "int8",
        "UInt64" | "Int128" | "UInt128" | "Decimal" | "Decimal32" | "Decimal64" | "Decimal128" => "numeric",
        "Float32" => "float4",
        "Float64" => "float8",
        "Bool" => "bool",
        "Date" | "Date32" => "date",
        "DateTime" | "DateTime64" => "timestamptz",
        "UUID" => "uuid",
        _ => "text",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_per_dialect() {
        let lc = "LowCardinality(Nullable(String))";
        assert_eq!(Dialect::ClickHouse.param_type(lc), lc);
        assert_eq!(Dialect::ClickHouse.array_type(lc), "Array(LowCardinality(Nullable(String)))");
        assert_eq!(Dialect::Postgres.param_type(lc), "text");
        assert_eq!(Dialect::Postgres.array_type("Int8"), "int2[]");
        assert_eq!(Dialect::Postgres.param_type("DateTime64(3)"), "timestamptz");
    }

    #[test]
    fn reducers_per_dialect() {
        assert_eq!(Dialect::ClickHouse.reducer("count", &[]).unwrap(), "count()");
        assert_eq!(Dialect::Postgres.reducer("count", &[]).unwrap(), "count(*)");
        assert_eq!(Dialect::ClickHouse.reducer("count_distinct", &["a"]).unwrap(), "uniqExact(a)");
        assert_eq!(Dialect::Postgres.reducer("count_distinct", &["a"]).unwrap(), "count(DISTINCT a)");
        assert!(Dialect::Postgres.reducer("wmedian", &["a", "w"]).is_none());
        assert!(Dialect::ClickHouse.reducer("whist", &["a", "w", "3"]).is_none());
    }
}
