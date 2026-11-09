//! Parses ClickHouse type strings into a structured tree.
//!
//! Ported from clickpad (our own ClickHouse explorer, not public), `ch-core/src/types.rs`, with its tests and its
//! depth cap, unchanged except for this header and the `ch_app` doc link.
//!
//! Total by construction: anything unrecognised becomes `ChType::Unknown`
//! rather than an error, because a client that refuses to display a result
//! over one unfamiliar column is worse than one that shows it as text.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimpleType {
    Bool,
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Int256,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    UInt128,
    UInt256,
    Float32,
    Float64,
    String,
    Uuid,
    Date,
    Date32,
    Ipv4,
    Ipv6,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TupleField {
    pub name: Option<String>,
    pub ty: ChType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChType {
    Nullable(Box<ChType>),
    LowCardinality(Box<ChType>),
    Array(Box<ChType>),
    Map(Box<ChType>, Box<ChType>),
    Tuple(Vec<TupleField>),
    Enum8(Vec<(String, i16)>),
    Enum16(Vec<(String, i16)>),
    Decimal {
        precision: u8,
        scale: u8,
    },
    DateTime64 {
        precision: u8,
        timezone: Option<String>,
    },
    DateTime {
        timezone: Option<String>,
    },
    FixedString(usize),
    /// A value that is *one of* these types, decided per row. ClickHouse
    /// resolves a `if(cond, a, b)` whose branches have no common supertype to
    /// one of these — `if(cond, -number * 5, number * 10)` over an unsigned
    /// column is `Variant(Int64, UInt64)`, which is how an ordinary query
    /// meets this type by accident.
    ///
    /// The member list is what the server wrote, in its order, with duplicates
    /// impossible (ClickHouse normalises and sorts them itself). A member that
    /// this parser does not recognise stays [`ChType::Unknown`] *in place*,
    /// like an unrecognised `Tuple` field: degrading the whole type would
    /// throw away the members that are perfectly clear.
    ///
    /// Note for anyone reaching for a workaround: a `Variant` cannot be cast.
    /// `toInt64(if(...))` over one is rejected outright by ClickHouse 26.2
    /// with `ILLEGAL_TYPE_OF_ARGUMENT`, so the client has to understand the
    /// type rather than tell the user to convert it away.
    Variant(Vec<ChType>),
    Simple(SimpleType),
    Unknown(String),
}

impl ChType {
    /// Strips `Nullable` and `LowCardinality` wrappers to reach the type that
    /// actually decides how a value is rendered.
    ///
    /// [`ChType::Variant`] is deliberately **not** stripped, and it is the
    /// exception a reader will otherwise assume. `Nullable` and
    /// `LowCardinality` are modifiers: they wrap a value domain without
    /// changing it, which is exactly why peeling them is safe. A `Variant`
    /// *is* the domain — its members are alternatives, not a wrapper around
    /// one thing — so peeling it would mean picking one member arbitrarily and
    /// then answering for the others as if that choice were free. Callers that
    /// want to know something about a `Variant` have to look at all of its
    /// members (clickpad's `ch_app::results::type_family` does).
    pub fn unwrap_modifiers(&self) -> &ChType {
        match self {
            ChType::Nullable(inner) | ChType::LowCardinality(inner) => inner.unwrap_modifiers(),
            other => other,
        }
    }
}

fn simple_type(name: &str) -> Option<SimpleType> {
    Some(match name {
        "Bool" => SimpleType::Bool,
        "Int8" => SimpleType::Int8,
        "Int16" => SimpleType::Int16,
        "Int32" => SimpleType::Int32,
        "Int64" => SimpleType::Int64,
        "Int128" => SimpleType::Int128,
        "Int256" => SimpleType::Int256,
        "UInt8" => SimpleType::UInt8,
        "UInt16" => SimpleType::UInt16,
        "UInt32" => SimpleType::UInt32,
        "UInt64" => SimpleType::UInt64,
        "UInt128" => SimpleType::UInt128,
        "UInt256" => SimpleType::UInt256,
        "Float32" => SimpleType::Float32,
        "Float64" => SimpleType::Float64,
        "String" => SimpleType::String,
        "UUID" => SimpleType::Uuid,
        "Date" => SimpleType::Date,
        "Date32" => SimpleType::Date32,
        "IPv4" => SimpleType::Ipv4,
        "IPv6" => SimpleType::Ipv6,
        _ => return None,
    })
}

/// Splits `s` on top-level commas, respecting nesting and single-quoted
/// strings. `Enum8('a,(b' = 1, 'c' = 2)` must yield two args, not four.
fn split_args(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_quote = false;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_quote => i += 1,
            b'\'' if in_quote => {
                // '' is an escaped quote inside a ClickHouse string literal.
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 1;
                } else {
                    in_quote = false;
                }
            }
            b'\'' => in_quote = true,
            b'(' if !in_quote => depth += 1,
            b')' if !in_quote => depth = depth.saturating_sub(1),
            b',' if !in_quote && depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Splits `Name(args)` into `("Name", Some("args"))`, or `Name` into
/// `("Name", None)`.
fn split_head(s: &str) -> (&str, Option<&str>) {
    match s.find('(') {
        Some(i) if s.ends_with(')') => (s[..i].trim(), Some(&s[i + 1..s.len() - 1])),
        _ => (s, None),
    }
}

/// Unquotes a ClickHouse string literal: `'Europe/Tallinn'` -> `Europe/Tallinn`.
///
/// ClickHouse escapes an embedded quote as `\'` in type strings, and `''`
/// also appears; both collapse to a single quote.
fn unquote(s: &str) -> Option<String> {
    let inner = s.trim().strip_prefix('\'')?.strip_suffix('\'')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\'') => out.push('\''),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '\'' => {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                }
                out.push('\'');
            }
            other => out.push(other),
        }
    }
    Some(out)
}

fn parse_enum_pairs(args: &str) -> Option<Vec<(String, i16)>> {
    split_args(args)
        .into_iter()
        .map(|pair| {
            let eq = pair.rfind('=')?;
            let name = unquote(&pair[..eq])?;
            let value = pair[eq + 1..].trim().parse::<i16>().ok()?;
            Some((name, value))
        })
        .collect()
}

fn parse_tuple_field(arg: &str, depth: usize) -> TupleField {
    // A named field is `name Type`. The name is a bare leading identifier
    // immediately followed by whitespace — searching for *any* whitespace
    // would split `Map(String, Array(UInt64))` at the comma's space and
    // silently produce a garbage type.
    let bytes = arg.as_bytes();
    let ident_len = bytes
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    let looks_named = ident_len > 0
        && ident_len < bytes.len()
        && bytes[ident_len].is_ascii_whitespace()
        && !bytes[0].is_ascii_digit();
    if looks_named {
        let (name, rest) = arg.split_at(ident_len);
        let rest = rest.trim();
        if !rest.is_empty() && !matches!(parse_at(rest, depth), ChType::Unknown(_)) {
            return TupleField {
                name: Some(name.to_string()),
                ty: parse_at(rest, depth),
            };
        }
    }
    TupleField {
        name: None,
        ty: parse_at(arg, depth),
    }
}

/// ClickHouse's own nesting is far below this; the cap exists so a
/// pathological input degrades to `Unknown` rather than overflowing the
/// stack, which aborts the process instead of panicking.
const MAX_DEPTH: usize = 64;

/// Parses a ClickHouse type string. Total: never panics, never errors.
pub fn parse(s: &str) -> ChType {
    parse_at(s, 0)
}

fn parse_at(s: &str, depth: usize) -> ChType {
    let s = s.trim();
    if depth > MAX_DEPTH {
        return ChType::Unknown(s.to_string());
    }
    let unknown = || ChType::Unknown(s.to_string());
    let (head, args) = split_head(s);

    match (head, args) {
        ("Nullable", Some(a)) => ChType::Nullable(Box::new(parse_at(a, depth + 1))),
        ("LowCardinality", Some(a)) => ChType::LowCardinality(Box::new(parse_at(a, depth + 1))),
        ("Array", Some(a)) => ChType::Array(Box::new(parse_at(a, depth + 1))),
        ("Map", Some(a)) => match split_args(a).as_slice() {
            [k, v] => ChType::Map(
                Box::new(parse_at(k, depth + 1)),
                Box::new(parse_at(v, depth + 1)),
            ),
            _ => unknown(),
        },
        ("Tuple", Some(a)) => ChType::Tuple(
            split_args(a)
                .into_iter()
                .map(|arg| parse_tuple_field(arg, depth + 1))
                .collect(),
        ),
        // `Variant`'s arguments are plain types, not named fields, so this is
        // `Array`'s recursion widened to a list rather than `Tuple`'s.
        ("Variant", Some(a)) => ChType::Variant(
            split_args(a)
                .into_iter()
                .map(|arg| parse_at(arg, depth + 1))
                .collect(),
        ),
        ("Enum8", Some(a)) => parse_enum_pairs(a).map_or_else(unknown, ChType::Enum8),
        ("Enum16", Some(a)) => parse_enum_pairs(a).map_or_else(unknown, ChType::Enum16),
        ("Decimal", Some(a)) => match split_args(a).as_slice() {
            [p, sc] => match (p.parse::<u8>(), sc.parse::<u8>()) {
                (Ok(precision), Ok(scale)) => ChType::Decimal { precision, scale },
                _ => unknown(),
            },
            _ => unknown(),
        },
        // Decimal32/64/128/256 carry an implied precision.
        ("Decimal32" | "Decimal64" | "Decimal128" | "Decimal256", Some(a)) => {
            let precision = match head {
                "Decimal32" => 9,
                "Decimal64" => 18,
                "Decimal128" => 38,
                _ => 76,
            };
            match a.trim().parse::<u8>() {
                Ok(scale) => ChType::Decimal { precision, scale },
                Err(_) => unknown(),
            }
        }
        ("DateTime64", Some(a)) => match split_args(a).as_slice() {
            [p] => p.parse::<u8>().map_or_else(
                |_| unknown(),
                |precision| ChType::DateTime64 {
                    precision,
                    timezone: None,
                },
            ),
            [p, tz] => match (p.parse::<u8>(), unquote(tz)) {
                (Ok(precision), Some(timezone)) => ChType::DateTime64 {
                    precision,
                    timezone: Some(timezone),
                },
                _ => unknown(),
            },
            _ => unknown(),
        },
        ("DateTime", Some(a)) => match unquote(a) {
            Some(tz) => ChType::DateTime { timezone: Some(tz) },
            None => unknown(),
        },
        ("DateTime", None) => ChType::DateTime { timezone: None },
        ("FixedString", Some(a)) => a
            .trim()
            .parse::<usize>()
            .map_or_else(|_| unknown(), ChType::FixedString),
        (name, None) => simple_type(name).map_or_else(unknown, ChType::Simple),
        _ => unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple(t: SimpleType) -> ChType {
        ChType::Simple(t)
    }

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("UInt64"), simple(SimpleType::UInt64));
        assert_eq!(parse("String"), simple(SimpleType::String));
        assert_eq!(parse("  Float64 "), simple(SimpleType::Float64));
        assert_eq!(parse("IPv6"), simple(SimpleType::Ipv6));
    }

    #[test]
    fn parses_nested_modifiers() {
        assert_eq!(
            parse("Nullable(String)"),
            ChType::Nullable(Box::new(simple(SimpleType::String)))
        );
        assert_eq!(
            parse("LowCardinality(Nullable(String))"),
            ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(simple(
                SimpleType::String
            )))))
        );
    }

    #[test]
    fn parses_decimal_in_all_spellings() {
        assert_eq!(
            parse("Decimal(38, 10)"),
            ChType::Decimal {
                precision: 38,
                scale: 10
            }
        );
        assert_eq!(
            parse("Decimal64(4)"),
            ChType::Decimal {
                precision: 18,
                scale: 4
            }
        );
        assert_eq!(
            parse("Nullable(Decimal(38, 10))"),
            ChType::Nullable(Box::new(ChType::Decimal {
                precision: 38,
                scale: 10
            }))
        );
    }

    #[test]
    fn parses_map_with_nested_array() {
        assert_eq!(
            parse("Map(String, Array(UInt64))"),
            ChType::Map(
                Box::new(simple(SimpleType::String)),
                Box::new(ChType::Array(Box::new(simple(SimpleType::UInt64))))
            )
        );
    }

    #[test]
    fn parses_unnamed_and_named_tuples() {
        assert_eq!(
            parse("Tuple(UInt8, String)"),
            ChType::Tuple(vec![
                TupleField {
                    name: None,
                    ty: simple(SimpleType::UInt8)
                },
                TupleField {
                    name: None,
                    ty: simple(SimpleType::String)
                },
            ])
        );
        assert_eq!(
            parse("Tuple(x UInt8, y String)"),
            ChType::Tuple(vec![
                TupleField {
                    name: Some("x".into()),
                    ty: simple(SimpleType::UInt8)
                },
                TupleField {
                    name: Some("y".into()),
                    ty: simple(SimpleType::String)
                },
            ])
        );
    }

    #[test]
    fn parses_enum_with_commas_and_parens_inside_quotes() {
        assert_eq!(
            parse("Enum8('a' = 1, 'b' = 2)"),
            ChType::Enum8(vec![("a".into(), 1), ("b".into(), 2)])
        );
        // A comma and a paren inside the quoted name must not split the args.
        assert_eq!(
            parse("Enum8('a,(b' = 1, 'c' = -2)"),
            ChType::Enum8(vec![("a,(b".into(), 1), ("c".into(), -2)])
        );
    }

    #[test]
    fn parses_datetime_variants() {
        assert_eq!(
            parse("DateTime64(3, 'UTC')"),
            ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC".into())
            }
        );
        assert_eq!(
            parse("DateTime64(3)"),
            ChType::DateTime64 {
                precision: 3,
                timezone: None
            }
        );
        assert_eq!(
            parse("DateTime('Europe/Tallinn')"),
            ChType::DateTime {
                timezone: Some("Europe/Tallinn".into())
            }
        );
        assert_eq!(parse("DateTime"), ChType::DateTime { timezone: None });
    }

    #[test]
    fn parses_fixed_string() {
        assert_eq!(parse("FixedString(16)"), ChType::FixedString(16));
    }

    #[test]
    fn unknown_types_degrade_instead_of_failing() {
        // The whole point: a type we have never heard of must survive.
        assert_eq!(
            parse("AggregateFunction(quantiles(0.5), UInt64)"),
            ChType::Unknown("AggregateFunction(quantiles(0.5), UInt64)".into())
        );
        assert_eq!(parse("Nothing"), ChType::Unknown("Nothing".into()));
    }

    #[test]
    fn unwrap_modifiers_reaches_the_inner_type() {
        assert_eq!(
            parse("LowCardinality(Nullable(String))").unwrap_modifiers(),
            &simple(SimpleType::String)
        );
        assert_eq!(
            parse("UInt8").unwrap_modifiers(),
            &simple(SimpleType::UInt8)
        );
    }

    #[test]
    fn an_unnamed_compound_tuple_field_is_not_mistaken_for_a_named_one() {
        // Real ClickHouse output. The field name heuristic must not split on
        // the whitespace after the comma inside Map(...).
        assert_eq!(
            parse("Tuple(Map(String, Array(UInt64)))"),
            ChType::Tuple(vec![TupleField {
                name: None,
                ty: ChType::Map(
                    Box::new(ChType::Simple(SimpleType::String)),
                    Box::new(ChType::Array(Box::new(ChType::Simple(SimpleType::UInt64))))
                ),
            }])
        );
        // Named fields with compound types must still work.
        assert_eq!(
            parse("Tuple(m Map(String, UInt8))"),
            ChType::Tuple(vec![TupleField {
                name: Some("m".into()),
                ty: ChType::Map(
                    Box::new(ChType::Simple(SimpleType::String)),
                    Box::new(ChType::Simple(SimpleType::UInt8))
                ),
            }])
        );
    }

    #[test]
    fn an_embedded_quote_is_unescaped_in_both_clickhouse_spellings() {
        // The server emits backslash escaping: Enum8('na\'me' = 1)
        assert_eq!(
            parse(r"Enum8('na\'me' = 1)"),
            ChType::Enum8(vec![("na'me".into(), 1)])
        );
        // The doubled form must keep working.
        assert_eq!(
            parse("Enum8('na''me' = 1)"),
            ChType::Enum8(vec![("na'me".into(), 1)])
        );
    }

    #[test]
    fn parses_variant_including_modified_and_nested_members() {
        // The reported case: ClickHouse 26.2 resolves `if(cond, -x, y)` over
        // an unsigned column to this.
        assert_eq!(
            parse("Variant(Int64, UInt64)"),
            ChType::Variant(vec![simple(SimpleType::Int64), simple(SimpleType::UInt64)])
        );
        // A member can carry its own modifiers.
        assert_eq!(
            parse("Variant(Int64, Nullable(UInt64))"),
            ChType::Variant(vec![
                simple(SimpleType::Int64),
                ChType::Nullable(Box::new(simple(SimpleType::UInt64)))
            ])
        );
        assert_eq!(
            parse("Variant(LowCardinality(String), Int64)"),
            ChType::Variant(vec![
                ChType::LowCardinality(Box::new(simple(SimpleType::String))),
                simple(SimpleType::Int64)
            ])
        );
        // And its own nesting, which must recurse rather than degrade.
        assert_eq!(
            parse("Variant(Array(Int64), Int64)"),
            ChType::Variant(vec![
                ChType::Array(Box::new(simple(SimpleType::Int64))),
                simple(SimpleType::Int64)
            ])
        );
        // A single-member Variant is legal; so, structurally, is an empty one.
        assert_eq!(
            parse("Variant(Int64)"),
            ChType::Variant(vec![simple(SimpleType::Int64)])
        );
        assert_eq!(parse("Variant()"), ChType::Variant(vec![]));
    }

    #[test]
    fn a_variant_member_that_does_not_parse_stays_unknown_in_place() {
        // Same rule as `Tuple`/`Array`: one unrecognised member degrades that
        // member, not the whole type, so the annotation still shows what the
        // server said and the classification can still see the members it does
        // understand (and refuse to call the whole thing numeric).
        assert_eq!(
            parse("Variant(Int64, Nothing)"),
            ChType::Variant(vec![
                simple(SimpleType::Int64),
                ChType::Unknown("Nothing".into())
            ])
        );
    }

    #[test]
    fn a_malformed_variant_degrades_to_unknown() {
        // No closing paren: `split_head` cannot find arguments, so this is a
        // type name this parser has never heard of, which is `Unknown`.
        assert_eq!(parse("Variant("), ChType::Unknown("Variant(".into()));
        assert_eq!(parse("Variant"), ChType::Unknown("Variant".into()));
    }

    #[test]
    fn unwrap_modifiers_leaves_a_variant_alone() {
        // A `Variant` is the value domain, not a wrapper around one: peeling it
        // would mean choosing one member arbitrarily.
        let v = parse("Variant(Int64, UInt64)");
        assert_eq!(v.unwrap_modifiers(), &v);
        // `Nullable(Variant(..))` peels exactly one layer and stops.
        assert_eq!(
            parse("Nullable(Variant(Int64, UInt64))").unwrap_modifiers(),
            &ChType::Variant(vec![simple(SimpleType::Int64), simple(SimpleType::UInt64)])
        );
    }

    #[test]
    fn a_variant_respects_the_depth_cap() {
        // Same contract as `nesting_past_the_depth_cap_degrades_to_unknown`,
        // asserted through the new arm: it must pass `depth + 1` down, or a
        // pathological nesting recurses without bound.
        let depth = MAX_DEPTH + 10;
        let deep = format!("{}UInt8{}", "Variant(".repeat(depth), ")".repeat(depth));
        let parsed = parse(&deep);
        let mut cursor = &parsed;
        let mut levels = 0usize;
        while let ChType::Variant(members) = cursor {
            assert_eq!(members.len(), 1);
            cursor = &members[0];
            levels += 1;
        }
        assert!(
            matches!(cursor, ChType::Unknown(_)),
            "expected the cap to yield Unknown, got {cursor:?} after {levels} levels"
        );
        assert!(
            levels <= MAX_DEPTH + 1,
            "recursed {levels} levels, deeper than the cap allows"
        );
    }

    #[test]
    fn nesting_past_the_depth_cap_degrades_to_unknown() {
        // Assert the cap's contract directly rather than trying to provoke a
        // stack overflow — the depth at which that happens is platform- and
        // profile-dependent, so a "didn't crash" test proves nothing. At 500
        // levels the *unguarded* parser did not crash either.
        let depth = MAX_DEPTH + 10;
        let deep = format!("{}UInt8{}", "Array(".repeat(depth), ")".repeat(depth));
        // Somewhere inside, the cap trips and yields Unknown rather than
        // recursing without bound.
        let parsed = parse(&deep);
        let mut cursor = &parsed;
        let mut levels = 0usize;
        while let ChType::Array(inner) = cursor {
            cursor = inner;
            levels += 1;
        }
        assert!(
            matches!(cursor, ChType::Unknown(_)),
            "expected the cap to yield Unknown, got {cursor:?} after {levels} levels"
        );
        assert!(
            levels <= MAX_DEPTH + 1,
            "recursed {levels} levels, deeper than the cap allows"
        );

        // Nesting within the cap must still parse fully.
        let ok = format!("{}UInt8{}", "Array(".repeat(10), ")".repeat(10));
        let mut cursor = &parse(&ok);
        let mut levels = 0usize;
        while let ChType::Array(inner) = cursor {
            cursor = inner;
            levels += 1;
        }
        assert_eq!(levels, 10);
        assert_eq!(cursor, &ChType::Simple(SimpleType::UInt8));
    }
}
