//! Compiles Mapbox/MapLibre style-spec filter expressions (the JSON found in
//! a layer's `"filter"` field) into `crate::style::Filter`.
//!
//! Shapes recognized are the ones actually present in the vendored
//! `examples/liberty-style.json` (OpenFreeMap Liberty), verified by
//! grepping that file directly rather than guessing from the spec docs.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::style::{Filter, GeometryKind, NumberOp};
use crate::style_spec::expression::{compile_zoom_value, eval_bool, fold_to_json_scalar, mentions_zoom, number_leaf, Scope};

pub fn compile_filter(json: &Value, scope: Scope<'_>) -> Result<Filter> {
    let arr = json.as_array().ok_or_else(|| anyhow::anyhow!("filter expression must be a JSON array, got {json}"))?;
    let op = arr.first().and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("filter expression must start with an operator string, got {json}"))?;

    match op {
        "==" | "!=" | "<" | "<=" | ">" | ">=" => compile_comparison(op, arr, json, scope),
        "in" => compile_in(arr, json),
        "has" => compile_has(arr, json),
        "!" => compile_not(arr, json, scope),
        "all" => compile_all(arr, json, scope),
        "any" => compile_any(arr, json, scope),
        "match" => compile_top_level_match(arr, json, scope),
        "step" => compile_step(arr, json, scope),
        "case" => compile_case(arr, json, scope),
        "let" => compile_let_filter(arr, json, scope),
        other => bail!("unsupported filter operator {other:?} in {json}"),
    }
}

/// `["let", name, value, ..., body]` as a FILTER: inline each binding at its
/// `["var", name]` use sites, then compile the body as an ordinary filter.
///
/// Inlining, not a lexical scope push, and that is exact rather than a
/// shortcut: Mapbox's own `let`/`var`
/// (`src/style-spec/expression/definitions/let.ts`, `var.ts`, Mapbox GL JS
/// 3.15.0) IS substitution, evaluated in the live context including the
/// feature, and a filter goes through the very same expression machinery as
/// a paint property there (`createFilter` hands the filter array to
/// `createExpression`). `expression::substitute_vars` is the same primitive
/// `expression::compile_let` already uses for a feature-dependent binding,
/// reused rather than re-derived, so the two `let`s cannot drift on what
/// shadowing or a nested `let` means.
///
/// **A body this crate's `Filter` cannot represent degrades the FILTER to
/// `Filter::Any` and keeps the layer**, rather than failing and taking the
/// layer with it. That is not the general policy for filters -- an
/// unsupported operator anywhere else still skips the layer, and must, since
/// "match everything" is a very wrong answer for e.g. an `is-active-floor`
/// clause on the indoor source. It is scoped to `let` on a measured
/// judgement about the one real case, Mapbox Standard's `poi-label`:
///
/// ```text
/// ["let", "densityByClass", ["match", ["get", "class"], ...],
///   ["<=", ["get", "filterrank"],
///     ["case", ["<", 0, ["var", "densityByClass"]],
///       ["+", ["step", ["zoom"], 0, 16, 1, 17, 2], ["var", "densityByClass"]],
///       ["var", "densityByClass"]]]]
/// ```
///
/// Inlined, that is a comparison whose threshold varies per FEATURE (by
/// `class`) and per zoom at once. `Filter` has `NumberCompareWithZoomOffset`
/// for "tag minus a zoom curve", but no per-feature dispatch INSIDE a
/// numeric comparison, so the shape has no faithful encoding today. The
/// clause it drops is a label-density limiter: keeping it costs a crowded
/// map, dropping the layer costs every POI icon on it. The notice is
/// recorded, not just printed, so the crowding is attributable.
///
/// A STRUCTURALLY broken `let` (an odd argument count, a non-string binding
/// name) is still an `Err`. The degradation is for a well-formed filter this
/// crate cannot represent, never a way to swallow malformed input.
fn compile_let_filter(arr: &[Value], json: &Value, scope: Scope<'_>) -> Result<Filter> {
    let bindings: Vec<(String, Value)> = crate::style_spec::expression::parse_let_bindings(arr, json)?
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect();
    let body = arr.last().expect("parse_let_bindings checked the arity");
    let inlined = crate::style_spec::expression::substitute_vars(body, &bindings);
    match compile_filter(&inlined, scope) {
        Ok(filter) => Ok(filter),
        Err(error) => {
            let notice = format!(
                "filter {json}: its \"let\" inlines exactly, but the result is not a shape \
                 `Filter` can represent ({error:#}); matching every feature and keeping the layer"
            );
            eprintln!("style_spec: {notice}");
            super::record_degradation(notice);
            Ok(Filter::Any)
        }
    }
}

/// Compiles a value that the style spec allows to be either a bare JSON
/// boolean literal or a nested filter expression -- used for `case`
/// outputs/fallback, `match` case-group outputs/fallback, and `step`
/// base/stop outputs, all of which accept either shape.
fn compile_filter_or_bool(value: &Value, scope: Scope<'_>) -> Result<Filter> {
    match value.as_bool() {
        Some(true) => Ok(Filter::Any),
        Some(false) => Ok(Filter::Never),
        None => compile_filter(value, scope),
    }
}

/// Extracts the property key from either the modern `["get", key]` form,
/// the legacy bare-string-key form, or either of those wrapped in
/// `["to-number", ...]` (real example: `landuse`'s
/// `[">=", ["to-number", ["get", "sizerank"]], 0]`). The `to-number`
/// coercion itself isn't represented -- callers that need numeric
/// semantics already get them from `TagValue::Number`/`Filter::NumberCompare`
/// downstream; this just lets the key underneath the wrapper be found.
fn extract_key(expr: &Value) -> Option<String> {
    if let Some(s) = expr.as_str() {
        return Some(s.to_string());
    }
    let arr = expr.as_array()?;
    if arr.first()?.as_str()? == "get" {
        return arr.get(1)?.as_str().map(|s| s.to_string());
    }
    if arr.first()?.as_str()? == "to-number" {
        return extract_key(arr.get(1)?);
    }
    None
}

/// Detects the `["geometry-type"]` no-arg expression, used by Liberty as
/// the input to a `match` that dispatches purely on geometry type, and used
/// by Mapbox Streets-v12 (pervasively, in its road/bridge/tunnel casing
/// layers) as the left-hand side of a comparison, e.g.
/// `["==", ["geometry-type"], "LineString"]` / `["!=", ...]`.
/// See https://maplibre.org/maplibre-style-spec/expressions/#geometry-type
///
/// Both forms compile to a real `Filter::GeometryType` predicate, evaluated
/// against the feature's own geometry by `Filter::matches_geometry`. They
/// used to compile to `Filter::Any` -- dropped outright -- because `Filter`
/// matched on a feature's tags alone.
///
/// Dropping them was exact for the polygon-only clauses (`["MultiPolygon",
/// "Polygon"]`, which in all six vendored styles sit only on `fill` layers,
/// where `render::mesh::append_fill` already returns early for non-polygon
/// geometry) and inexact for the LineString-only ones: `StyleLayer::Line`
/// deliberately accepts polygon-sourced features and strokes their boundary
/// rings (`crate::style::boundary_geometry_for_line`) so outline layers over
/// polygon sources render at all, so a dropped `["LineString",
/// "MultiLineString"]` clause let such a layer stroke any *polygon* feature
/// that passed the rest of the filter. Measured against real OpenFreeMap
/// tiles over Singapore, the `transportation` source layer carries 6-10
/// polygon features per tile out of 700-2400 (classes `bridge`/`pier`/
/// `path`), of which the `path` ones (1-4 per z14 tile) passed Liberty's
/// `road_path_pedestrian` family and drew a 1px white outline where MapLibre
/// draws a `fill-pattern`. Those strokes are what this predicate removes.
fn is_geometry_type_expr(expr: &Value) -> bool {
    expr.as_array().is_some_and(|a| a.len() == 1 && a[0].as_str() == Some("geometry-type"))
}

/// Compiles a `["geometry-type"]` label list into the predicate. Accepts a
/// bare string (the `match` spec's single-label form) or an array of them.
///
/// An unrecognized name is an `Err`, not a silent drop: a misspelled label
/// would otherwise compile to a predicate that matches nothing, and quietly
/// emptying a layer is the one outcome a correctness fix here must not
/// produce. `GeometryKind::from_style_name` accepts every spelling the spec
/// defines, `Multi*` included -- see its doc comment for why those are
/// aliases here and dead names in MapLibre.
fn compile_geometry_type_labels(labels: &Value, json: &Value) -> Result<Vec<GeometryKind>> {
    let names: Vec<&str> = match labels {
        Value::String(s) => vec![s.as_str()],
        Value::Array(values) => values
            .iter()
            .map(|v| v.as_str().ok_or_else(|| anyhow::anyhow!("\"geometry-type\" labels must be strings: {json}")))
            .collect::<Result<Vec<_>>>()?,
        _ => bail!("\"geometry-type\" labels must be a string or an array of strings: {json}"),
    };
    let mut kinds = Vec::new();
    for name in names {
        let kind = GeometryKind::from_style_name(name)
            .ok_or_else(|| anyhow::anyhow!("unknown \"geometry-type\" value {name:?} in {json}"))?;
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    if kinds.is_empty() {
        bail!("\"geometry-type\" predicate names no geometry class: {json}");
    }
    Ok(kinds)
}

/// Detects and compiles the one real arithmetic-comparison shape found in
/// Mapbox Streets' `landuse` filter: `[op, ["-", <to-number-wrapped-get>,
/// <zoom-based expression>], <literal threshold>]` -- e.g. `["<=", ["-",
/// ["to-number", ["get", "sizerank"]], ["interpolate", ["exponential",
/// 1.5], ["zoom"], 12, 0, 18, 14]], 14]`. Returns `Ok(None)` (not an
/// error) when the comparison's left side isn't this exact shape, so the
/// caller falls through to the ordinary key/value comparison path
/// unchanged -- this is purely additive.
///
/// Deliberately narrow: only `-` (subtraction), only a
/// `to-number`-wrapped `get` as the minuend, only a numeric literal
/// threshold on the comparison's right side. A general
/// arithmetic-expression evaluator is out of scope -- see the design
/// spec's Scope section.
fn compile_arithmetic_comparison(op: &str, lhs: &Value, rhs: &Value, json: &Value) -> Result<Option<Filter>> {
    let Some(lhs_arr) = lhs.as_array() else { return Ok(None) };
    if lhs_arr.len() != 3 || lhs_arr.first().and_then(Value::as_str) != Some("-") {
        return Ok(None);
    }
    let Some(key) = extract_key(&lhs_arr[1]) else { return Ok(None) };
    let Some(threshold) = rhs.as_f64() else { return Ok(None) };

    let number_op = match op {
        "==" => NumberOp::Eq,
        "!=" => NumberOp::NotEq,
        "<" => NumberOp::Lt,
        "<=" => NumberOp::Lte,
        ">" => NumberOp::Gt,
        ">=" => NumberOp::Gte,
        other => bail!("unsupported comparison operator {other:?} in arithmetic comparison: {json}"),
    };

    let mut offset_pairs = compile_zoom_value(&lhs_arr[2], number_leaf, Scope::root())
        .map_err(|e| anyhow::anyhow!("arithmetic comparison's zoom-dependent offset failed to compile: {e:#}: {json}"))?;
    if offset_pairs.len() != 1 {
        bail!("arithmetic comparison's zoom-dependent offset must not be feature-dependent (a match expression): {json}");
    }
    let offset = offset_pairs.pop().expect("length checked above").1;

    Ok(Some(Filter::NumberCompareWithZoomOffset { key, op: number_op, offset, threshold }))
}

/// Detects and compiles the mirrored arithmetic-comparison shape found in
/// Mapbox Streets' `poi-label` filter: `[op, <key>, ["+", <zoom-based
/// expression>, <literal threshold>]]` -- e.g. `["<=", ["get",
/// "filterrank"], ["+", ["step", ["zoom"], 0, 16, 1, 17, 2], 3]]`. This is
/// the same underlying inequality as `compile_arithmetic_comparison`'s
/// shape, just written with the zoom-dependent term added on the
/// comparison's right side instead of subtracted on the left:
/// `key <= zoom_expr + threshold` is algebraically `key - zoom_expr <=
/// threshold`, so this compiles to the identical `Filter::
/// NumberCompareWithZoomOffset` target. Returns `Ok(None)` (not an error)
/// when the comparison isn't this exact shape, so the caller falls through
/// to the ordinary key/value comparison path unchanged.
///
/// Deliberately narrow, mirroring `compile_arithmetic_comparison`: only
/// `+` (addition), only a plain key (`extract_key`) on the left, only a
/// numeric literal threshold as the addition's second operand.
fn compile_additive_arithmetic_comparison(op: &str, lhs: &Value, rhs: &Value, json: &Value) -> Result<Option<Filter>> {
    let Some(key) = extract_key(lhs) else { return Ok(None) };
    let Some(rhs_arr) = rhs.as_array() else { return Ok(None) };
    if rhs_arr.len() != 3 || rhs_arr.first().and_then(Value::as_str) != Some("+") {
        return Ok(None);
    }
    let Some(threshold) = rhs_arr[2].as_f64() else { return Ok(None) };

    let number_op = match op {
        "==" => NumberOp::Eq,
        "!=" => NumberOp::NotEq,
        "<" => NumberOp::Lt,
        "<=" => NumberOp::Lte,
        ">" => NumberOp::Gt,
        ">=" => NumberOp::Gte,
        other => bail!("unsupported comparison operator {other:?} in arithmetic comparison: {json}"),
    };

    let mut offset_pairs = compile_zoom_value(&rhs_arr[1], number_leaf, Scope::root())
        .map_err(|e| anyhow::anyhow!("arithmetic comparison's zoom-dependent offset failed to compile: {e:#}: {json}"))?;
    if offset_pairs.len() != 1 {
        bail!("arithmetic comparison's zoom-dependent offset must not be feature-dependent (a match expression): {json}");
    }
    let offset = offset_pairs.pop().expect("length checked above").1;

    Ok(Some(Filter::NumberCompareWithZoomOffset { key, op: number_op, offset, threshold }))
}

/// Whether `expr`'s subtree reads `["config", k]` anywhere. Such an operand
/// is a CONSTANT for a given compile -- the config is resolved once, before
/// any layer is compiled (`config::parse_imports`), and rides on `Scope` --
/// so a comparison containing one can be folded.
fn mentions_config(expr: &Value) -> bool {
    match expr {
        Value::Array(items) => {
            items.first().and_then(Value::as_str) == Some("config") || items.iter().any(mentions_config)
        }
        _ => false,
    }
}

/// `["<", a, b]` written the other way round is `[">", b, a]`. Used when the
/// constant operand turns out to be on the LEFT, since
/// `compile_key_value_comparison` always reads the key first.
fn mirrored_operator(op: &str) -> &str {
    match op {
        "<" => ">",
        "<=" => ">=",
        ">" => "<",
        ">=" => "<=",
        symmetric => symmetric,
    }
}

/// Folds a comparison whose operand(s) read `["config", k]`, returning
/// `Ok(None)` -- not an error -- for every comparison that does not, so the
/// caller falls through to the existing paths untouched. Neither vendored
/// style writes `config` at all, so neither reaches past the first line.
///
/// The semantics are not invented here. In the reference a filter is not a
/// language of its own: `createFilter`
/// (`src/style-spec/feature_filter/index.ts`, Mapbox GL JS 3.15.0) hands the
/// filter array to `createExpression(staticFilter, filterSpec, scope,
/// options)` -- the same entry point a paint property goes through, carrying
/// the same `ConfigOptions` -- and `["config", k]` is then served by the
/// ordinary `Config` expression class
/// (`src/style-spec/expression/definitions/config.ts`), which has no
/// filter-specific behaviour whatsoever and reports itself `featureConstant`.
/// So the correct answer is just "evaluate it", and that is what this does,
/// through this module's existing evaluator and the compile's own `Scope`.
///
/// Two outcomes:
///
/// - **Both operands constant** -- the shape Mapbox Standard's `landuse`
///   writes, `["==", ["config", "theme"], "monochrome"]`. The comparison has
///   a single answer for this whole compile, so it becomes `Filter::Any` or
///   `Filter::Never`. A `Never` is deliberately NOT a compile failure: the
///   document is well-formed and the layer must still compile, it simply
///   matches no feature and therefore draws nothing.
/// - **One operand constant, the other reading the feature** -- e.g.
///   `["==", ["get", "class"], ["config", "themeClass"]]`. The folded side
///   is the comparison's value and the ordinary key/value path takes over.
///   Both positions are checked; when the constant is on the left the
///   operator is mirrored, since `["<", ["config", k], ["get", p]]` is
///   `p > k`.
///
/// A comparison that also reads `["zoom"]` is left alone (`Ok(None)`): the
/// zoom a filter is evaluated at is the camera's at draw time, not the one
/// `Scope` folds paint properties at, and `Filter` has its own zoom-aware
/// variants for that. Folding one here would silently pin it to a single
/// zoom.
fn compile_config_comparison(op: &str, arr: &[Value], json: &Value, scope: Scope<'_>) -> Result<Option<Filter>> {
    if !mentions_config(&arr[1]) && !mentions_config(&arr[2]) {
        return Ok(None);
    }
    if mentions_zoom(json) {
        return Ok(None);
    }

    // The whole comparison as one expression: succeeds exactly when neither
    // operand reads the feature.
    if let Ok(constant) = eval_bool(json, scope) {
        return Ok(Some(if constant { Filter::Any } else { Filter::Never }));
    }

    // Otherwise fold just the config-bearing operand into the literal it
    // stands for, and let the ordinary path compile the rest. A fold that
    // fails here (an unresolvable key, a non-scalar value) yields `Ok(None)`,
    // so the caller refuses the comparison exactly as it did before rather
    // than filtering on a made-up value.
    for (constant_side, key_side, effective_op) in [(&arr[1], &arr[2], mirrored_operator(op)), (&arr[2], &arr[1], op)] {
        if !mentions_config(constant_side) {
            continue;
        }
        let (Ok(value), Some(key)) = (fold_to_json_scalar(constant_side, scope), extract_key(key_side)) else {
            continue;
        };
        return compile_key_value_comparison(effective_op, key, &value, json).map(Some);
    }
    Ok(None)
}

fn compile_comparison(op: &str, arr: &[Value], json: &Value, scope: Scope<'_>) -> Result<Filter> {
    if arr.len() != 3 {
        bail!("comparison filter must have exactly 3 elements: {json}");
    }

    if let Some(filter) = compile_config_comparison(op, arr, json, scope)? {
        return Ok(filter);
    }
    if let Some(filter) = compile_arithmetic_comparison(op, &arr[1], &arr[2], json)? {
        return Ok(filter);
    }
    if let Some(filter) = compile_additive_arithmetic_comparison(op, &arr[1], &arr[2], json)? {
        return Ok(filter);
    }

    let key = match extract_key(&arr[1]) {
        Some(key) => key,
        None if is_geometry_type_expr(&arr[1]) && (op == "==" || op == "!=") => {
            // The comparison form, e.g. Mapbox Streets' pervasive
            // `["==", ["geometry-type"], "LineString"]`. Only `==`/`!=` are
            // meaningful on a geometry type; the ordering operators fall
            // through to the bail below.
            let kinds = compile_geometry_type_labels(&arr[2], json)?;
            let predicate = Filter::GeometryType(kinds);
            return Ok(if op == "==" { predicate } else { Filter::Not(Box::new(predicate)) });
        }
        None => bail!("comparison filter's second element must be a key or [\"get\", key]: {json}"),
    };
    compile_key_value_comparison(op, key, &arr[2], json)
}

/// The ordinary `<key> <op> <literal>` comparison, once the key and the
/// literal have been identified. Split out of `compile_comparison` so the
/// config-folding path above can reuse it with a folded operand standing in
/// for the literal.
fn compile_key_value_comparison(op: &str, key: String, value: &Value, json: &Value) -> Result<Filter> {
    if let Some(s) = value.as_str() {
        let value = s.to_string();
        return match op {
            "==" => Ok(Filter::Equals { key, value }),
            "!=" => Ok(Filter::Not(Box::new(Filter::Equals { key, value }))),
            _ => bail!("comparison operator {op:?} requires a numeric value, got string: {json}"),
        };
    }

    if let Some(n) = value.as_f64() {
        // "!=" is special-cased to Not(Eq) rather than NumberOp::NotEq
        // directly: per the style spec's expression semantics, a missing
        // property is `null`, and `null != value` evaluates to true. That
        // matches how `Filter::Not(Equals)` already behaves for the string
        // case (absent tag -> inner Equals false -> Not true), so mirror it
        // here instead of giving NumberCompare's catch-all "absent -> false"
        // fallback (see `Filter::matches`) a special exception for NotEq.
        if op == "!=" {
            return Ok(Filter::Not(Box::new(Filter::NumberCompare { key, op: NumberOp::Eq, value: n })));
        }
        let number_op = match op {
            "==" => NumberOp::Eq,
            "<" => NumberOp::Lt,
            "<=" => NumberOp::Lte,
            ">" => NumberOp::Gt,
            ">=" => NumberOp::Gte,
            _ => unreachable!("op already validated by caller's match"),
        };
        return Ok(Filter::NumberCompare { key, op: number_op, value: n });
    }

    if let Some(b) = value.as_bool() {
        // As `!=` on a number above: `Not(BoolEquals)`, so an absent tag
        // (`null`) is `!= true` -- OpenMapTiles' `hide_3d`, which only the
        // flagged outlines carry.
        return match op {
            "==" => Ok(Filter::BoolEquals { key, value: b }),
            "!=" => Ok(Filter::Not(Box::new(Filter::BoolEquals { key, value: b }))),
            _ => bail!("comparison operator {op:?} requires a numeric value, got a boolean: {json}"),
        };
    }

    bail!("comparison filter's value must be a string, number or boolean: {json}")
}

fn compile_in(arr: &[Value], json: &Value) -> Result<Filter> {
    if arr.len() < 2 {
        bail!("\"in\" filter must have at least a key: {json}");
    }
    let key = extract_key(&arr[1]).ok_or_else(|| anyhow::anyhow!("\"in\" filter's second element must be a key or [\"get\", key]: {json}"))?;

    // Modern form: ["in", ["get", K], ["literal", [...]]]
    if arr.len() == 3 {
        if let Some(literal_arr) = arr[2].as_array() {
            if literal_arr.first().and_then(Value::as_str) == Some("literal") {
                let values = literal_arr
                    .get(1)
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("\"in\" filter's [\"literal\", [...]] must wrap an array: {json}"))?
                    .iter()
                    .map(|v| v.as_str().map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("\"in\" filter values must be strings: {json}")))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(Filter::In { key, values });
            }
        }
    }

    // Legacy form: ["in", K, v1, v2, ...]
    let values = arr[2..]
        .iter()
        .map(|v| v.as_str().map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("\"in\" filter values must be strings: {json}")))
        .collect::<Result<Vec<_>>>()?;
    Ok(Filter::In { key, values })
}

fn compile_has(arr: &[Value], json: &Value) -> Result<Filter> {
    if arr.len() != 2 {
        bail!("\"has\" filter must have exactly 2 elements: {json}");
    }
    let key = arr[1].as_str().ok_or_else(|| anyhow::anyhow!("\"has\" filter's key must be a string: {json}"))?.to_string();
    Ok(Filter::Has(key))
}

fn compile_not(arr: &[Value], json: &Value, scope: Scope<'_>) -> Result<Filter> {
    if arr.len() != 2 {
        bail!("\"!\" filter must have exactly 2 elements: {json}");
    }
    Ok(Filter::Not(Box::new(compile_filter(&arr[1], scope)?)))
}

/// The `"all"` AND-combinator. A nested `["match", ["geometry-type"], ...]`
/// clause used to be dropped from the AND here, because it could only
/// compile to an always-true clause; it now compiles like any other
/// sub-filter, into a real `Filter::GeometryType` predicate.
fn compile_all(arr: &[Value], _json: &Value, scope: Scope<'_>) -> Result<Filter> {
    let sub_filters = arr[1..].iter().map(|sub| compile_filter(sub, scope)).collect::<Result<Vec<_>>>()?;
    Ok(Filter::All(sub_filters))
}

/// The `"any"` OR-combinator. Not used by any layer in the vendored Liberty
/// style (verified by grep), but supported for spec completeness alongside
/// `Filter::AnyOf`.
fn compile_any(arr: &[Value], _json: &Value, scope: Scope<'_>) -> Result<Filter> {
    let sub_filters = arr[1..].iter().map(|sub| compile_filter(sub, scope)).collect::<Result<Vec<_>>>()?;
    Ok(Filter::AnyOf(sub_filters))
}

fn compile_top_level_match(arr: &[Value], json: &Value, scope: Scope<'_>) -> Result<Filter> {
    if arr.len() < 4 {
        bail!("\"match\" filter must have at least an input, one case-group, one output, and a fallback: {json}");
    }
    let input = &arr[1];

    if is_geometry_type_expr(input) {
        return compile_geometry_type_match(arr, json, scope);
    }

    // Fast path: a single-case-group `match` with plain-boolean outputs on
    // both sides compiles directly to `Filter::In`/`Not(In)` without going
    // through `compile_general_match`'s `Filter::MultiMatch` machinery. This
    // only applies when the case-group's labels are actually an array of
    // strings, though -- the style spec also allows a single-case-group
    // label to be a bare string (e.g. transit-label in
    // examples/mapbox-streets-style.json: ["match", ["get", "mode"], "bus",
    // false, true]). When it's not an array of strings, fall through to
    // `compile_general_match`, which already normalizes both a bare string
    // and an array of strings into `Vec<String>`.
    if arr.len() == 5 {
        if let (Some(out1), Some(out2)) = (arr[3].as_bool(), arr[4].as_bool()) {
            let case_values = arr[2]
                .as_array()
                .and_then(|values| values.iter().map(|v| v.as_str().map(|s| s.to_string())).collect::<Option<Vec<_>>>());
            if let Some(case_values) = case_values {
                let key = extract_key(input)
                    .ok_or_else(|| anyhow::anyhow!("\"match\" filter's input must be [\"get\", key] or [\"geometry-type\"]: {json}"))?;
                return match (out1, out2) {
                    (true, false) => Ok(Filter::In { key, values: case_values }),
                    (false, true) => Ok(Filter::Not(Box::new(Filter::In { key, values: case_values }))),
                    _ => compile_general_match(arr, json, input, scope),
                };
            }
        }
    }

    compile_general_match(arr, json, input, scope)
}

/// `["match", ["geometry-type"], labels1, out1, ..., fallback]` -- the
/// spelling Liberty uses, always as `[..., true, false]`, though
/// nothing here assumes that arity or those outputs.
///
/// Compiled to a `Filter::Case` whose arm conditions are the geometry
/// predicates, rather than to a `MultiMatch` sibling keyed on geometry:
/// `Case` already means "first arm whose condition holds supplies the
/// result, else the fallback", which is exactly `match`'s semantics, and
/// reusing it keeps `Filter` to one new variant for the whole feature.
fn compile_geometry_type_match(arr: &[Value], json: &Value, scope: Scope<'_>) -> Result<Filter> {
    // ["match", input, (labels, out) * N, fallback] -- 2 fixed + 2N + 1, so
    // always odd. An even length is a dangling label with no output.
    if arr.len().is_multiple_of(2) {
        bail!("\"match\" filter must have the form [\"match\", input, labels1, out1, ..., fallback]: {json}");
    }
    let body = &arr[2..arr.len() - 1];
    let mut arms = Vec::with_capacity(body.len() / 2);
    for pair in body.as_chunks::<2>().0.iter() {
        let kinds = compile_geometry_type_labels(&pair[0], json)?;
        arms.push((Filter::GeometryType(kinds), compile_filter_or_bool(&pair[1], scope)?));
    }
    let fallback = Box::new(compile_filter_or_bool(&arr[arr.len() - 1], scope)?);
    Ok(Filter::Case { arms, fallback })
}

/// Generalizes `match` beyond the single-case-group boolean-only shape
/// above: `["match", input, labels1, output1, labels2, output2, ...,
/// fallback]`. Each `labelsN` is either a bare string (one label) or an
/// array of strings (multiple labels) -- the style spec allows both forms
/// per case-group. Each output (and the trailing fallback) may be a
/// boolean literal or a nested filter expression.
fn compile_general_match(arr: &[Value], json: &Value, input: &Value, scope: Scope<'_>) -> Result<Filter> {
    let (key, body, fallback_value) = compile_match_prelude(arr, json, input)?;

    // Real style-spec `match` expressions never mix label types across
    // case-groups within one `match`, so checking the first case-group's
    // label is sufficient to decide which label-parsing loop applies.
    let is_numeric = match &body[0] {
        Value::Number(_) => true,
        Value::Array(values) => values.first().is_some_and(Value::is_number),
        _ => false,
    };
    if is_numeric {
        compile_general_number_match(body, fallback_value, json, key, scope)
    } else {
        compile_general_string_match(body, fallback_value, json, key, scope)
    }
}

/// Shared arg-count/oddness validation and `key`/`body`/fallback extraction
/// for `compile_general_match`'s numeric and string label-parsing paths.
/// `body` holds the case-group pairs only (labels/output, labels/output,
/// ...); `fallback_value` is the trailing element, kept separate from `body`.
fn compile_match_prelude<'a>(arr: &'a [Value], json: &Value, input: &Value) -> Result<(String, &'a [Value], &'a Value)> {
    // Valid shapes are ["match", input, labels1, out1, ..., fallback]:
    // 2 fixed elements + 2*N pair elements + 1 fallback = always an ODD
    // total length. An even length (e.g. a dangling label with no
    // matching output) is malformed -- bail here rather than let the
    // pair-parsing loop below index out of bounds on it.
    if arr.len().is_multiple_of(2) {
        bail!("\"match\" filter must have the form [\"match\", input, labels1, out1, ..., fallback]: {json}");
    }
    let key = extract_key(input)
        .ok_or_else(|| anyhow::anyhow!("\"match\" filter's input must be [\"get\", key] or [\"geometry-type\"]: {json}"))?;

    let body = &arr[2..arr.len() - 1];
    let fallback_value = &arr[arr.len() - 1];
    Ok((key, body, fallback_value))
}

/// The string-label case-group parsing loop, extracted from
/// `compile_general_match`'s original body verbatim.
fn compile_general_string_match(body: &[Value], fallback_value: &Value, json: &Value, key: String, scope: Scope<'_>) -> Result<Filter> {
    let mut cases = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let labels = match &body[i] {
            Value::String(s) => vec![s.clone()],
            Value::Array(values) => values
                .iter()
                .map(|v| v.as_str().map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("\"match\" filter's case labels must be strings: {json}")))
                .collect::<Result<Vec<_>>>()?,
            other => bail!("\"match\" filter's case-group labels must be a string or array of strings, got {other}: {json}"),
        };
        let output = compile_filter_or_bool(&body[i + 1], scope)?;
        cases.push((labels, output));
        i += 2;
    }
    let fallback = compile_filter_or_bool(fallback_value, scope)?;
    Ok(Filter::MultiMatch { key, cases, fallback: Box::new(fallback) })
}

/// The numeric-label case-group parsing loop, mirroring
/// `compile_general_string_match` but parsing each case-group's labels as
/// `f64` and building a `Filter::NumberMultiMatch`. Exists for `match`
/// expressions like Mapbox Streets' `hillshade` layer filter, whose
/// case-group labels are bare JSON numbers (e.g. `89`, `78`) rather than
/// strings.
fn compile_general_number_match(body: &[Value], fallback_value: &Value, json: &Value, key: String, scope: Scope<'_>) -> Result<Filter> {
    let mut cases = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let labels = match &body[i] {
            Value::Number(_) => vec![body[i]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("\"match\" filter's case-group label is not a valid number: {json}"))?],
            Value::Array(values) => values
                .iter()
                .map(|v| v.as_f64().ok_or_else(|| anyhow::anyhow!("\"match\" filter's case labels must be numbers: {json}")))
                .collect::<Result<Vec<_>>>()?,
            other => bail!("\"match\" filter's case-group labels must be a number or array of numbers, got {other}: {json}"),
        };
        let output = compile_filter_or_bool(&body[i + 1], scope)?;
        cases.push((labels, output));
        i += 2;
    }
    let fallback = compile_filter_or_bool(fallback_value, scope)?;
    Ok(Filter::NumberMultiMatch { key, cases, fallback: Box::new(fallback) })
}

/// Compiles a `step` filter expression, restricted to a zoom input (the
/// only shape seen in real filter contexts -- style-spec `step` on other
/// inputs is out of scope, see the design spec's Scope section).
fn compile_step(arr: &[Value], json: &Value, scope: Scope<'_>) -> Result<Filter> {
    if arr.len() < 5 || arr.len().is_multiple_of(2) {
        bail!("\"step\" filter must have the form [\"step\", [\"zoom\"], base, stop1, out1, ...]: {json}");
    }
    if arr[1] != serde_json::json!(["zoom"]) {
        bail!("\"step\" filter is only supported with [\"zoom\"] as its input: {json}");
    }
    let base = compile_filter_or_bool(&arr[2], scope)?;
    let mut stops = Vec::new();
    let mut i = 3;
    while i < arr.len() {
        let threshold = arr[i]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("\"step\" filter's stop thresholds must be numbers: {json}"))?;
        let output = compile_filter_or_bool(&arr[i + 1], scope)?;
        stops.push((threshold, output));
        i += 2;
    }
    Ok(Filter::ZoomStep { base: Box::new(base), stops })
}

/// Compiles a `case` filter expression: `["case", cond1, out1, cond2,
/// out2, ..., fallback]` -- an ordered if/elif/else chain. `arr[1..]` has
/// odd length (pairs plus a trailing fallback).
fn compile_case(arr: &[Value], json: &Value, scope: Scope<'_>) -> Result<Filter> {
    if arr.len() < 3 || !arr.len().is_multiple_of(2) {
        bail!("\"case\" filter must have the form [\"case\", cond1, out1, ..., fallback]: {json}");
    }
    let body = &arr[1..arr.len() - 1];
    let mut arms = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let cond = compile_filter(&body[i], scope)?;
        let output = compile_filter_or_bool(&body[i + 1], scope)?;
        arms.push((cond, output));
        i += 2;
    }
    let fallback = compile_filter_or_bool(&arr[arr.len() - 1], scope)?;
    Ok(Filter::Case { arms, fallback: Box::new(fallback) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::TagValue;
    use std::collections::HashMap;

    fn tags(pairs: &[(&str, TagValue)]) -> HashMap<String, TagValue> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn s(v: &str) -> TagValue {
        TagValue::String(v.to_string())
    }

    // ---- `Filter::BoolEquals` (Task 7 fix round 2) ----

    fn compile(json: serde_json::Value) -> Result<Filter> {
        compile_filter(&json, Scope::root())
    }

    fn dbg(filter: &Filter) -> String {
        format!("{filter:?}")
    }

    fn bool_leaf(value: bool) -> Filter {
        Filter::BoolEquals { key: "k".to_string(), value }
    }

    /// What every boolean-literal test is run against: `Bool(true)`,
    /// `Bool(false)`, absent, `String("true")`, `Number(1)`.
    fn bool_probe(filter: &Filter) -> [bool; 5] {
        let with = |v: TagValue| filter.matches(&tags(&[("k", v)]), 14.0);
        [
            with(TagValue::Bool(true)),
            with(TagValue::Bool(false)),
            filter.matches(&tags(&[]), 14.0),
            with(s("true")),
            with(TagValue::Number(1.0)),
        ]
    }

    #[test]
    fn a_boolean_comparison_compiles_to_bool_equals_in_both_syntaxes() {
        for value in [true, false] {
            let eq = dbg(&bool_leaf(value));
            let ne = dbg(&Filter::Not(Box::new(bool_leaf(value))));
            assert_eq!(dbg(&compile(serde_json::json!(["==", ["get", "k"], value])).unwrap()), eq);
            assert_eq!(dbg(&compile(serde_json::json!(["!=", ["get", "k"], value])).unwrap()), ne);
            assert_eq!(dbg(&compile(serde_json::json!(["==", "k", value])).unwrap()), eq, "legacy ==");
            assert_eq!(dbg(&compile(serde_json::json!(["!=", "k", value])).unwrap()), ne, "legacy !=");
        }
    }

    #[test]
    fn an_ordering_operator_on_a_boolean_is_an_error_naming_it() {
        for op in ["<", "<=", ">", ">="] {
            let err = compile(serde_json::json!([op, ["get", "k"], true])).unwrap_err().to_string();
            assert!(err.contains("boolean"), "{op}: {err}");
        }
    }

    #[test]
    fn equals_true_is_strict_and_not_equals_true_passes_everything_else() {
        let eq = compile(serde_json::json!(["==", ["get", "k"], true])).unwrap();
        assert_eq!(bool_probe(&eq), [true, false, false, false, false], "== true: Bool(true) only");
        let ne = compile(serde_json::json!(["!=", ["get", "k"], true])).unwrap();
        assert_eq!(bool_probe(&ne), [false, true, true, true, true], "!= true: absent is null != true");
    }

    #[test]
    fn equals_false_and_not_equals_false() {
        let eq = compile(serde_json::json!(["==", ["get", "k"], false])).unwrap();
        assert_eq!(bool_probe(&eq), [false, true, false, false, false], "== false: Bool(false) only, absent fails");
        let ne = compile(serde_json::json!(["!=", ["get", "k"], false])).unwrap();
        assert_eq!(bool_probe(&ne), [true, false, true, true, true], "!= false: absent and Bool(true) pass");
    }

    #[test]
    fn the_string_literal_true_stays_a_string_equality() {
        let f = compile(serde_json::json!(["==", ["get", "k"], "true"])).unwrap();
        assert!(matches!(f, Filter::Equals { ref value, .. } if value == "true"), "{f:?}");
        assert_eq!(bool_probe(&f), [false, false, false, true, false], "matches String(\"true\") only");
    }

    #[test]
    fn a_bool_leaf_does_not_use_zoom_but_a_step_beside_it_does() {
        assert!(!compile(serde_json::json!(["==", ["get", "k"], true])).unwrap().uses_zoom());
        assert!(!compile(serde_json::json!(["!=", ["get", "k"], true])).unwrap().uses_zoom());
        let stepped =
            compile(serde_json::json!(["all", ["!=", ["get", "k"], true], ["step", ["zoom"], false, 12, true]])).unwrap();
        assert!(stepped.uses_zoom());
    }

    fn n(v: f64) -> TagValue {
        TagValue::Number(v)
    }

    /// A scope carrying a resolved config, the way `parse_and_compile`
    /// hands one to `compile_layer` for an imported document's layers.
    fn config(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> crate::style_spec::config::ResolvedConfig {
        crate::style_spec::config::ResolvedConfig::from_pairs(pairs)
    }

    /// `["==", ["config", k], literal]` -- Mapbox Standard's `landuse`
    /// filter, the one shape that kept that layer out of the compiled
    /// style. Both operands are compile-time constants, so the whole
    /// comparison folds away and the clause is simply true.
    #[test]
    fn folds_a_true_config_comparison_to_an_always_matching_clause() {
        let cfg = config([("theme", serde_json::json!("monochrome"))]);
        let json: Value = serde_json::from_str(r#"["==", ["config", "theme"], "monochrome"]"#).unwrap();
        let filter = compile_filter(&json, Scope::root().with_config(&cfg)).unwrap();
        assert!(matches!(filter, Filter::Any), "expected the clause to fold away, got {filter:?}");
        assert!(filter.matches(&HashMap::new(), 14.0));
    }

    /// The same shape with the config resolving the other way: the layer
    /// must still COMPILE -- skipping it would be wrong, the document is
    /// perfectly well-formed -- but match no feature at all.
    #[test]
    fn folds_a_false_config_comparison_to_a_never_matching_clause() {
        let cfg = config([("theme", serde_json::json!("faded"))]);
        let json: Value = serde_json::from_str(r#"["==", ["config", "theme"], "monochrome"]"#).unwrap();
        let filter = compile_filter(&json, Scope::root().with_config(&cfg)).unwrap();
        assert!(matches!(filter, Filter::Never), "expected the clause to fold to never, got {filter:?}");
        assert!(!filter.matches(&tags(&[("class", s("residential"))]), 14.0));
    }

    /// The operand order is not fixed by the spec -- a filter is just an
    /// expression, and `==` takes two of them -- so `["config", k]` in the
    /// FIRST position folds exactly the same way.
    #[test]
    fn folds_a_config_comparison_written_with_the_config_second() {
        let cfg = config([("theme", serde_json::json!("monochrome"))]);
        let t: Value = serde_json::from_str(r#"["==", "monochrome", ["config", "theme"]]"#).unwrap();
        let f: Value = serde_json::from_str(r#"["!=", "monochrome", ["config", "theme"]]"#).unwrap();
        assert!(matches!(compile_filter(&t, Scope::root().with_config(&cfg)).unwrap(), Filter::Any));
        assert!(matches!(compile_filter(&f, Scope::root().with_config(&cfg)).unwrap(), Filter::Never));
    }

    /// A config operand is not always constant-vs-constant: compared
    /// against a feature property it is just the comparison's VALUE, and
    /// the ordinary key/value path takes over from there.
    #[test]
    fn folds_a_config_operand_compared_against_a_feature_property() {
        let cfg = config([("themeClass", serde_json::json!("residential"))]);
        let json: Value = serde_json::from_str(r#"["==", ["get", "class"], ["config", "themeClass"]]"#).unwrap();
        let filter = compile_filter(&json, Scope::root().with_config(&cfg)).unwrap();
        assert!(filter.matches(&tags(&[("class", s("residential"))]), 14.0));
        assert!(!filter.matches(&tags(&[("class", s("commercial"))]), 14.0));
    }

    /// Mirrored: the config on the LEFT and the feature property on the
    /// right. `["<", ["config", k], ["get", p]]` is `p > k`, so the
    /// operator has to be flipped along with the operands.
    #[test]
    fn flips_the_operator_when_the_config_operand_is_on_the_left() {
        let cfg = config([("minRank", serde_json::json!(7.0))]);
        let json: Value = serde_json::from_str(r#"["<", ["config", "minRank"], ["get", "rank"]]"#).unwrap();
        let filter = compile_filter(&json, Scope::root().with_config(&cfg)).unwrap();
        assert!(
            matches!(&filter, Filter::NumberCompare { key, op: NumberOp::Gt, value } if key == "rank" && *value == 7.0),
            "expected rank > 7, got {filter:?}"
        );
        assert!(filter.matches(&tags(&[("rank", n(8.0))]), 14.0));
        assert!(!filter.matches(&tags(&[("rank", n(7.0))]), 14.0));
    }

    /// A folded config value is a VALUE, never a key: the legacy bare-string
    /// key form (`["==", "class", "wood"]`) must not swallow it. Without
    /// that distinction `["==", ["config", "theme"], "monochrome"]` would
    /// compile to `Equals { key: "monochrome", value: "monochrome" }` --
    /// a filter over a property named after a config value.
    #[test]
    fn a_folded_config_string_is_a_value_not_a_legacy_bare_key() {
        let cfg = config([("theme", serde_json::json!("monochrome"))]);
        let json: Value = serde_json::from_str(r#"["==", ["config", "theme"], "monochrome"]"#).unwrap();
        let filter = compile_filter(&json, Scope::root().with_config(&cfg)).unwrap();
        assert!(
            !matches!(&filter, Filter::Equals { .. }),
            "the config operand was mistaken for a legacy bare property key: {filter:?}"
        );
    }

    /// Nested inside the `case` that Mapbox Standard's `landuse` actually
    /// writes: the folded condition stays a condition, and the arm it
    /// guards is the one that applies.
    #[test]
    fn folds_a_config_comparison_used_as_a_case_condition() {
        let cfg = config([("theme", serde_json::json!("monochrome"))]);
        let json: Value = serde_json::from_str(
            r#"["case", ["==", ["config", "theme"], "monochrome"], ["match", ["get", "class"], ["parking", "sand"], false, true], true]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root().with_config(&cfg)).unwrap();
        assert!(filter.matches(&tags(&[("class", s("residential"))]), 14.0));
        assert!(!filter.matches(&tags(&[("class", s("parking"))]), 14.0));

        // The other theme takes the `true` fallback, so `parking` passes.
        let other = config([("theme", serde_json::json!("faded"))]);
        let filter = compile_filter(&json, Scope::root().with_config(&other)).unwrap();
        assert!(filter.matches(&tags(&[("class", s("parking"))]), 14.0));
    }

    /// An unresolvable key is not a constant, so nothing folds and the
    /// comparison is refused exactly as it was before -- the layer is
    /// skipped rather than silently filtered by a made-up value.
    #[test]
    fn refuses_a_config_comparison_whose_key_does_not_resolve() {
        let cfg = config([("theme", serde_json::json!("monochrome"))]);
        let json: Value = serde_json::from_str(r#"["==", ["config", "missing"], "monochrome"]"#).unwrap();
        assert!(compile_filter(&json, Scope::root().with_config(&cfg)).is_err());
        // And with no config resolved for this compile at all.
        assert!(compile_filter(&json, Scope::root()).is_err());
    }

    #[test]
    fn compiles_a_simple_equals_expression_filter() {
        // landcover_wood: ["==", ["get", "class"], "wood"]
        let json: Value = serde_json::from_str(r#"["==", ["get", "class"], "wood"]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("class", s("wood"))]), 14.0));
        assert!(!filter.matches(&tags(&[("class", s("grass"))]), 14.0));
    }

    #[test]
    fn compiles_a_not_equals_expression_filter() {
        // water: ["!=", ["get", "brunnel"], "tunnel"]
        let json: Value = serde_json::from_str(r#"["!=", ["get", "brunnel"], "tunnel"]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("brunnel", s("bridge"))]), 14.0));
        assert!(filter.matches(&HashMap::new(), 14.0));
        assert!(!filter.matches(&tags(&[("brunnel", s("tunnel"))]), 14.0));
    }

    #[test]
    fn compiles_an_all_combinator_with_a_geometry_type_match_alongside_tag_clauses() {
        // road_minor: ["all", ["match", ["geometry-type"], ["LineString","MultiLineString"], true, false], ["match", ["get","brunnel"], ["bridge","tunnel"], false, true], ["match", ["get","class"], ["minor"], true, false]]
        let json: Value = serde_json::from_str(
            r#"["all", ["match", ["geometry-type"], ["LineString","MultiLineString"], true, false], ["match", ["get","brunnel"], ["bridge","tunnel"], false, true], ["match", ["get","class"], ["minor"], true, false]]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        let minor = tags(&[("class", s("minor"))]);

        // class=minor, brunnel absent, LINE geometry: matches.
        assert!(filter.matches_geometry(&minor, 14.0, Some(GeometryKind::LineString)));
        // The same feature as a POLYGON no longer matches. This is the
        // clause that used to be dropped from the AND entirely, which is
        // how a road layer came to stroke the boundary of a pedestrian-area
        // polygon.
        assert!(!filter.matches_geometry(&minor, 14.0, Some(GeometryKind::Polygon)));
        assert!(!filter.matches_geometry(&minor, 14.0, Some(GeometryKind::Point)));
        // With no geometry in hand the clause cannot be answered and must
        // not delete the feature -- the pre-existing `matches` behaviour.
        assert!(filter.matches(&minor, 14.0));

        // class=minor, brunnel=bridge: the brunnel clause's `false` case
        // for bridge/tunnel is real filtering logic and excludes this.
        assert!(!filter.matches_geometry(
            &tags(&[("class", s("minor")), ("brunnel", s("bridge"))]),
            14.0,
            Some(GeometryKind::LineString)
        ));
    }

    #[test]
    fn compiles_a_numeric_equals_filter() {
        // tunnel_motorway_link_casing: ["==", ["get", "ramp"], 1] (as part of an `all`)
        let json: Value = serde_json::from_str(r#"["==", ["get", "ramp"], 1]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("ramp", n(1.0))]), 14.0));
        assert!(!filter.matches(&tags(&[("ramp", n(0.0))]), 14.0));
        assert!(!filter.matches(&HashMap::new(), 14.0));
    }

    #[test]
    fn compiles_a_numeric_not_equals_filter() {
        // road_motorway_casing: ["!=", ["get", "ramp"], 1]
        let json: Value = serde_json::from_str(r#"["!=", ["get", "ramp"], 1]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("ramp", n(0.0))]), 14.0));
        assert!(!filter.matches(&tags(&[("ramp", n(1.0))]), 14.0));
        // Most road features have no "ramp" tag at all; missing != 1 should match.
        assert!(filter.matches(&HashMap::new(), 14.0));
    }

    #[test]
    fn compiles_less_than_or_equal_and_greater_than_or_equal_numeric_filters() {
        // boundary_3: ["all", [">=", ["get", "admin_level"], 3], ["<=", ["get", "admin_level"], 6], ...]
        let ge: Value = serde_json::from_str(r#"[">=", ["get", "admin_level"], 3]"#).unwrap();
        let le: Value = serde_json::from_str(r#"["<=", ["get", "admin_level"], 6]"#).unwrap();
        let ge_filter = compile_filter(&ge, Scope::root()).unwrap();
        let le_filter = compile_filter(&le, Scope::root()).unwrap();

        assert!(ge_filter.matches(&tags(&[("admin_level", n(3.0))]), 14.0));
        assert!(!ge_filter.matches(&tags(&[("admin_level", n(2.0))]), 14.0));
        assert!(le_filter.matches(&tags(&[("admin_level", n(6.0))]), 14.0));
        assert!(!le_filter.matches(&tags(&[("admin_level", n(7.0))]), 14.0));
    }

    #[test]
    fn compiles_a_less_than_numeric_filter() {
        // poi_r7: ["all", ..., [">=", ["get", "rank"], 7], ["<", ["get", "rank"], 20]]
        let json: Value = serde_json::from_str(r#"["<", ["get", "rank"], 20]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("rank", n(19.0))]), 14.0));
        assert!(!filter.matches(&tags(&[("rank", n(20.0))]), 14.0));
    }

    #[test]
    fn compiles_a_boolean_match_filter_in_true_false_order() {
        // tunnel_service_track_casing: ["match", ["get", "class"], ["service", "track"], true, false]
        let json: Value = serde_json::from_str(r#"["match", ["get", "class"], ["service", "track"], true, false]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("class", s("service"))]), 14.0));
        assert!(filter.matches(&tags(&[("class", s("track"))]), 14.0));
        assert!(!filter.matches(&tags(&[("class", s("motorway"))]), 14.0));
    }

    #[test]
    fn compiles_a_boolean_match_filter_in_inverted_false_true_order() {
        // road_motorway_link_casing: ["match", ["get", "brunnel"], ["bridge", "tunnel"], false, true]
        let json: Value = serde_json::from_str(r#"["match", ["get", "brunnel"], ["bridge", "tunnel"], false, true]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        // Absent/other brunnel values pass (the "else" branch is `true`).
        assert!(filter.matches(&HashMap::new(), 14.0));
        assert!(filter.matches(&tags(&[("brunnel", s("ford"))]), 14.0));
        // Listed values (bridge/tunnel) are excluded (the "then" branch is `false`).
        assert!(!filter.matches(&tags(&[("brunnel", s("bridge"))]), 14.0));
        assert!(!filter.matches(&tags(&[("brunnel", s("tunnel"))]), 14.0));
    }

    #[test]
    fn compiles_a_top_level_geometry_type_match_into_a_real_predicate() {
        // aeroway_fill: ["match", ["geometry-type"], ["MultiPolygon", "Polygon"], true, false]
        let json: Value = serde_json::from_str(r#"["match", ["geometry-type"], ["MultiPolygon", "Polygon"], true, false]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        let none = HashMap::new();
        assert!(filter.matches_geometry(&none, 14.0, Some(GeometryKind::Polygon)));
        assert!(!filter.matches_geometry(&none, 14.0, Some(GeometryKind::LineString)));
        assert!(!filter.matches_geometry(&none, 14.0, Some(GeometryKind::Point)));
        // "MultiPolygon" and "Polygon" collapse to the one class, so the
        // duplicate label costs nothing -- see `GeometryKind`.
        assert!(filter.matches(&none, 14.0), "no geometry in hand must still match");
    }

    #[test]
    fn compiles_a_top_level_geometry_type_equals_into_a_real_predicate() {
        // aeroway-line (Mapbox Streets v12): ["==", ["geometry-type"], "LineString"]
        let json: Value = serde_json::from_str(r#"["==", ["geometry-type"], "LineString"]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        let none = HashMap::new();
        assert!(filter.matches_geometry(&none, 14.0, Some(GeometryKind::LineString)));
        assert!(!filter.matches_geometry(&none, 14.0, Some(GeometryKind::Polygon)));

        // The negated spelling inverts it, including for a feature whose
        // geometry class is not the one named.
        let json: Value = serde_json::from_str(r#"["!=", ["geometry-type"], "LineString"]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(!filter.matches_geometry(&none, 14.0, Some(GeometryKind::LineString)));
        assert!(filter.matches_geometry(&none, 14.0, Some(GeometryKind::Polygon)));
    }

    #[test]
    fn compiles_an_all_combinator_with_a_geometry_type_equals_alongside_tag_clauses() {
        // Modeled on Mapbox Streets v12's road/bridge/tunnel casing layers,
        // e.g. bridge-path: ["all", ["==", ["geometry-type"], "LineString"], ["!=", ["get", "class"], "path"]]
        let json: Value = serde_json::from_str(
            r#"["all", ["==", ["geometry-type"], "LineString"], ["!=", ["get", "class"], "path"]]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        let minor = tags(&[("class", s("minor"))]);

        assert!(filter.matches_geometry(&minor, 14.0, Some(GeometryKind::LineString)));
        assert!(!filter.matches_geometry(&minor, 14.0, Some(GeometryKind::Polygon)));

        // class=path: excluded by the real "!=" clause whatever the geometry.
        assert!(!filter.matches_geometry(&tags(&[("class", s("path"))]), 14.0, Some(GeometryKind::LineString)));
    }

    /// A misspelled or unsupported geometry-type label is an `Err`, not a
    /// predicate that matches nothing: silently emptying a layer is the one
    /// outcome this compile path must not produce.
    #[test]
    fn an_unknown_geometry_type_label_is_a_compile_error_not_an_empty_predicate() {
        for json in [
            r#"["==", ["geometry-type"], "Linestring"]"#,
            r#"["==", ["geometry-type"], "Unknown"]"#,
            r#"["match", ["geometry-type"], ["Curve"], true, false]"#,
            r#"["==", ["geometry-type"], 3]"#,
        ] {
            let value: Value = serde_json::from_str(json).unwrap();
            assert!(compile_filter(&value, Scope::root()).is_err(), "{json} must not compile");
        }
    }

    #[test]
    fn compiles_a_has_filter() {
        // airport: ["all", ["has", "iata"]]
        let json: Value = serde_json::from_str(r#"["has", "iata"]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("iata", s("SIN"))]), 14.0));
        assert!(!filter.matches(&HashMap::new(), 14.0));
    }

    #[test]
    fn compiles_a_negated_has_filter() {
        // boundary_2 / boundary_3: ["!", ["has", "claimed_by"]]
        let json: Value = serde_json::from_str(r#"["!", ["has", "claimed_by"]]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&HashMap::new(), 14.0));
        assert!(!filter.matches(&tags(&[("claimed_by", s("XX"))]), 14.0));
    }

    #[test]
    fn compiles_the_full_boundary_3_filter() {
        // boundary_3: ["all", [">=", ["get", "admin_level"], 3], ["<=", ["get", "admin_level"], 6], ["!=", ["get", "maritime"], 1], ["!=", ["get", "disputed"], 1], ["!", ["has", "claimed_by"]]]
        let json: Value = serde_json::from_str(
            r#"["all", [">=", ["get", "admin_level"], 3], ["<=", ["get", "admin_level"], 6], ["!=", ["get", "maritime"], 1], ["!=", ["get", "disputed"], 1], ["!", ["has", "claimed_by"]]]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();

        assert!(filter.matches(&tags(&[("admin_level", n(4.0))]), 14.0));
        assert!(!filter.matches(&tags(&[("admin_level", n(2.0))]), 14.0)); // below range
        assert!(!filter.matches(&tags(&[("admin_level", n(4.0)), ("maritime", n(1.0))]), 14.0));
        assert!(!filter.matches(&tags(&[("admin_level", n(4.0)), ("claimed_by", s("XX"))]), 14.0));
    }

    #[test]
    fn compiles_an_any_combinator() {
        // Not present in Liberty (verified by grep), but "any" is part of
        // the style spec and Filter::AnyOf exists to support it.
        let json: Value =
            serde_json::from_str(r#"["any", ["==", ["get", "class"], "river"], ["==", ["get", "class"], "lake"]]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("class", s("river"))]), 14.0));
        assert!(filter.matches(&tags(&[("class", s("lake"))]), 14.0));
        assert!(!filter.matches(&tags(&[("class", s("forest"))]), 14.0));
    }

    #[test]
    fn rejects_a_value_selecting_match_used_outside_a_filter_context() {
        // poi_r20's icon-image layout property (not a filter):
        // ["match", ["get", "subclass"], ["florist", "furniture"], ["get", "subclass"], ["get", "class"]]
        let json: Value = serde_json::from_str(
            r#"["match", ["get", "subclass"], ["florist", "furniture"], ["get", "subclass"], ["get", "class"]]"#,
        )
        .unwrap();
        assert!(compile_filter(&json, Scope::root()).is_err());
    }

    #[test]
    fn rejects_a_case_filter_with_a_non_expression_condition() {
        // A `case` condition must be a filter expression, not a bare bool.
        let json: Value = serde_json::from_str(r#"["case", true, "a", "b"]"#).unwrap();
        assert!(compile_filter(&json, Scope::root()).is_err());
    }

    #[test]
    fn compiles_a_single_case_group_match_with_a_bare_string_label() {
        // transit-label (examples/mapbox-streets-style.json):
        // ["match", ["get", "mode"], "bus", false, true]
        let json: Value = serde_json::from_str(r#"["match", ["get", "mode"], "bus", false, true]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(!filter.matches(&tags(&[("mode", s("bus"))]), 14.0));
        assert!(filter.matches(&tags(&[("mode", s("rail"))]), 14.0));
        assert!(filter.matches(&HashMap::new(), 14.0));
    }

    #[test]
    fn compiles_a_zoom_step_filter_using_the_real_motorway_trunk_shape() {
        // road-motorway-trunk-case (examples/mapbox-streets-style.json):
        // ["step", ["zoom"],
        //   ["match", ["get", "class"], ["motorway", "trunk"], true, false],
        //   5,
        //   ["all",
        //     ["match", ["get", "class"], ["motorway", "trunk"], true, false],
        //     ["match", ["get", "structure"], ["none", "ford"], true, false]]]
        let json: Value = serde_json::from_str(
            r#"["step", ["zoom"],
                 ["match", ["get", "class"], ["motorway", "trunk"], true, false],
                 5,
                 ["all",
                   ["match", ["get", "class"], ["motorway", "trunk"], true, false],
                   ["match", ["get", "structure"], ["none", "ford"], true, false]]]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();

        // Below zoom 5: only the class-match applies (base).
        assert!(filter.matches(&tags(&[("class", s("motorway"))]), 4.0));
        assert!(filter.matches(&tags(&[("class", s("motorway")), ("structure", s("bridge"))]), 4.0));

        // At/above zoom 5: the structure-match also applies.
        assert!(filter.matches(&tags(&[("class", s("motorway")), ("structure", s("none"))]), 5.0));
        assert!(!filter.matches(&tags(&[("class", s("motorway")), ("structure", s("bridge"))]), 5.0));
        assert!(!filter.matches(&tags(&[("class", s("residential"))]), 5.0));
    }

    #[test]
    fn compiles_a_case_filter_using_the_real_road_pedestrian_shape() {
        // road-pedestrian-case (examples/mapbox-streets-style.json):
        // ["case", ["has", "layer"], [">=", ["get", "layer"], 0], true]
        let json: Value =
            serde_json::from_str(r#"["case", ["has", "layer"], [">=", ["get", "layer"], 0], true]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();

        assert!(filter.matches(&tags(&[("layer", n(0.0))]), 14.0));
        assert!(!filter.matches(&tags(&[("layer", n(-1.0))]), 14.0));
        assert!(filter.matches(&HashMap::new(), 14.0)); // no "layer" tag: fallback (true)
    }

    #[test]
    fn compiles_a_multi_case_match_filter_using_the_real_road_minor_shape() {
        // road-minor / road-minor-case (examples/mapbox-streets-style.json):
        // ["match", ["get", "class"], ["track"], true, "service", ["step", ["zoom"], false, 14, true], false]
        // Note the second case-group's label is a bare string ("service"), not an array.
        let json: Value = serde_json::from_str(
            r#"["match", ["get", "class"], ["track"], true, "service", ["step", ["zoom"], false, 14, true], false]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();

        assert!(filter.matches(&tags(&[("class", s("track"))]), 5.0)); // "track": always true, any zoom
        assert!(filter.matches(&tags(&[("class", s("track"))]), 20.0));

        assert!(!filter.matches(&tags(&[("class", s("service"))]), 5.0)); // "service": false below zoom 14
        assert!(filter.matches(&tags(&[("class", s("service"))]), 14.0)); // true at/above zoom 14

        assert!(!filter.matches(&tags(&[("class", s("residential"))]), 14.0)); // no case matches: fallback (false)
    }

    #[test]
    fn compiles_a_numeric_match_filter_using_the_real_hillshade_level_shape() {
        // hillshade (examples/mapbox-streets-style.json), simplified from its
        // real ["all", ["step", ...], ["match", ["get", "level"], 89, true,
        // 78, ["step", ["zoom"], false, 5, true], ..., false]] shape: the
        // case-group labels (89, 78, ...) are bare JSON numbers, not
        // strings.
        let json: Value = serde_json::from_str(r#"["match", ["get", "level"], 89, true, 78, false, false]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();

        // level=89.0 (numeric tag): matches the first case-group.
        assert!(filter.matches(&tags(&[("level", n(89.0))]), 14.0));

        // level=78.0: matches the second case-group, whose output is false.
        assert!(!filter.matches(&tags(&[("level", n(78.0))]), 14.0));

        // level tagged as a string instead of a number: wrong type, doesn't
        // match any case-group, falls to fallback (false) rather than
        // panicking.
        assert!(!filter.matches(&tags(&[("level", s("89"))]), 14.0));

        // "level" tag missing entirely: falls to fallback (false).
        assert!(!filter.matches(&HashMap::new(), 14.0));
    }

    #[test]
    fn compiles_a_match_with_a_step_valued_single_case_group_output_using_the_real_landcover_shape() {
        // landcover (examples/mapbox-streets-style.json):
        // ["match", ["get", "class"], ["scrub", "grass"], ["step", ["zoom"], true, 8, false], true]
        let json: Value = serde_json::from_str(
            r#"["match", ["get", "class"], ["scrub", "grass"], ["step", ["zoom"], true, 8, false], true]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();

        assert!(filter.matches(&tags(&[("class", s("grass"))]), 5.0)); // below zoom 8: base (true)
        assert!(!filter.matches(&tags(&[("class", s("grass"))]), 8.0)); // at/above zoom 8: false
        assert!(filter.matches(&tags(&[("class", s("wood"))]), 8.0)); // no case matches: fallback (true)
    }

    #[test]
    fn compiles_a_to_number_wrapped_comparison() {
        // landuse: [">=", ["to-number", ["get", "sizerank"]], 0]
        let json: Value = serde_json::from_str(r#"[">=", ["to-number", ["get", "sizerank"]], 0]"#).unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        assert!(filter.matches(&tags(&[("sizerank", n(0.0))]), 14.0));
        assert!(filter.matches(&tags(&[("sizerank", n(5.0))]), 14.0));
        assert!(!filter.matches(&tags(&[("sizerank", n(-1.0))]), 14.0));
    }

    #[test]
    fn compiles_the_real_landuse_arithmetic_comparison() {
        // landuse: ["<=", ["-", ["to-number", ["get", "sizerank"]],
        //   ["interpolate", ["exponential", 1.5], ["zoom"], 12, 0, 18, 14]], 14]
        let json: Value = serde_json::from_str(
            r#"["<=", ["-", ["to-number", ["get", "sizerank"]],
                 ["interpolate", ["exponential", 1.5], ["zoom"], 12, 0, 18, 14]], 14]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();
        // At zoom 12 the offset is 0: sizerank 5 - 0 = 5 <= 14 matches.
        assert!(filter.matches(&tags(&[("sizerank", n(5.0))]), 12.0));
        // sizerank 30 - 0 = 30 <= 14 does not.
        assert!(!filter.matches(&tags(&[("sizerank", n(30.0))]), 12.0));
        // At zoom 15, the offset is NOT clamped to an endpoint -- it's the
        // true resampled exponential curve value, ~3.19994 (14 * (1.5^3-1)/(1.5^6-1)).
        // This is the assertion that actually proves compile_zoom_value produced
        // a correctly-interpolated interior offset, not just a clamped endpoint.
        assert!(filter.matches(&tags(&[("sizerank", n(17.0))]), 15.0)); // 17 - 3.2 ≈ 13.8 <= 14
        assert!(!filter.matches(&tags(&[("sizerank", n(18.0))]), 15.0)); // 18 - 3.2 ≈ 14.8 > 14
    }

    #[test]
    fn compiles_the_real_poi_label_arithmetic_comparison() {
        // poi-label: ["<=", ["get", "filterrank"], ["+", ["step", ["zoom"], 0, 16, 1, 17, 2], 3]]
        let json: Value = serde_json::from_str(
            r#"["<=", ["get", "filterrank"], ["+", ["step", ["zoom"], 0, 16, 1, 17, 2], 3]]"#,
        )
        .unwrap();
        let filter = compile_filter(&json, Scope::root()).unwrap();

        // At zoom 10, step(zoom) evaluates to its base value 0, so the
        // effective threshold is 0 + 3 = 3.
        assert!(filter.matches(&tags(&[("filterrank", n(2.0))]), 10.0)); // 2 <= 3
        assert!(!filter.matches(&tags(&[("filterrank", n(5.0))]), 10.0)); // 5 <= 3 is false

        // At zoom 17, step(zoom) evaluates to 2, so the threshold is 2 + 3 = 5.
        assert!(filter.matches(&tags(&[("filterrank", n(5.0))]), 17.0)); // 5 <= 5
        assert!(!filter.matches(&tags(&[("filterrank", n(6.0))]), 17.0)); // 6 <= 5 is false
    }
}
