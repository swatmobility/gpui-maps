//! Compiles Mapbox/MapLibre style-spec *value* expressions (the JSON found
//! in a paint property, e.g. `fill-color` or `line-width`) into
//! `(crate::style::Filter, crate::style::ZoomValue<T>)` pairs.
//!
//! Per the plan's "expand match, keep interpolate as zoom-only" strategy: a
//! bare literal or a zoom-only `interpolate` compiles to a single pair
//! (`Filter::Any`, the value); a `match` on a feature property expands into
//! one pair per case (each case's filter ANDed with whatever its own
//! recursively-compiled sub-expression produced), plus one pair for the
//! default branch, filtered by the negation of every explicit case.
//!
//! Shapes are verified against the vendored `examples/liberty-style.json`
//! (OpenFreeMap Liberty) by grepping/parsing that file directly rather than
//! guessing from the spec docs -- see the doc comments on
//! `compile_interpolate` and `compile_match` for what was actually found.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::style::{DashPattern, Filter, ZoomExprValue, ZoomValue};
use crate::style_spec::config::ResolvedConfig;
use crate::style_spec::lut::ColorLut;

/// The lexical scope an expression is being compiled in: the chain of
/// `let`-bound names visible at this point in the tree.
///
/// It also carries the integer zoom the enclosing fold is being done at, and
/// the resolved `["config", k]` values (an `Import`'s `ResolvedConfig`, see
/// `style_spec::config`). Neither is lexical and neither belongs to the word
/// "scope", but both have to travel the same road for the same reason:
/// `parse_leaf` is where an expression over bound names is actually folded
/// (see below), and its signature is fixed by the `+ Copy` bound, so
/// anything the folder needs has to arrive as part of this one parameter.
/// `compile_folded` is the only thing that ever varies the zoom, one integer
/// at a time; the config is set once, with `with_config`, and never varies
/// again for the rest of a compile.
///
/// ### Why a parameter and not a captured closure
///
/// The obvious design -- have a `let` arm wrap `parse_leaf` in a new
/// closure that knows the new bindings -- cannot be written in Rust here.
/// `compile_zoom_value` is generic over `impl Fn(&Value, Scope<'_>) ->
/// Result<T> + Copy` and recurses with the SAME closure value
/// (`compile_match`, `compile_step`, the `case` arm). Wrapping it per
/// binding frame would give every recursion level a distinct closure type,
/// so monomorphization would never terminate -- a compile error, not a
/// slowdown. So the scope has to be an ordinary parameter of one concrete
/// type, and that type has to be the same at every depth.
///
/// ### Why a borrowed cons-list
///
/// Each `let` frame lives in the stack frame of the `compile_*` call that
/// pushed it and borrows its bindings from the style JSON, so pushing
/// costs one `Scope` by value and allocates nothing -- which matters
/// because a `match`/`interpolate` cross-product re-walks its sub-
/// expressions once per case. `Scope` is `Copy` for the same reason the
/// `parse_leaf` bound is `Copy`: it is passed down unchanged into every
/// recursive call and into the per-stop closures.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    /// This frame's bindings, in declaration order. `lookup` searches
    /// backwards so a `let` that rebinds a name shadows its own earlier
    /// binding. Empty at the root.
    bindings: &'a [(&'a str, &'a Value)],
    /// The frame this one was pushed onto -- `None` at the root. Borrowed
    /// from the caller's stack; `Scope` is covariant in `'a`, which is what
    /// lets a deeper (shorter-lived) borrow of an outer frame typecheck.
    enclosing: Option<&'a Scope<'a>>,
    /// The integer zoom the constant folder is currently folding at, which
    /// `["zoom"]` reads. 0 at the root; `compile_folded` walks it over
    /// `0..=MAX_FOLD_ZOOM` for an expression that actually depends on it.
    zoom: u8,
    /// The resolved config `["config", k]` reads, or `None` for a compile
    /// with nothing to resolve against (every style before Mapbox
    /// Standard, including both vendored fixtures). Like `zoom`, this is
    /// not lexical -- one config for the whole compile, not a `let`-style
    /// binding -- but it rides on `Scope` for the same reason `zoom` does:
    /// `parse_leaf`'s `+ Copy` bound means anything the folder needs has to
    /// arrive as part of this one parameter. Set with `with_config`.
    config: Option<&'a ResolvedConfig>,
    /// The imported document's raw `lights` array (`imports[0].data.lights`
    /// -- an `ambient` entry and a `directional` one, each an ordinary
    /// expression tree in this same vocabulary), or `None` for a compile
    /// with no lights to resolve against. Rides on `Scope` for the same
    /// reason `config` does: not lexical, one value for the whole compile,
    /// but `parse_leaf`'s `+ Copy` bound means it has to travel as part of
    /// this one parameter. Set with `with_lights`; read by
    /// `["measure-light", "brightness"]`.
    lights: Option<&'a Value>,
    /// The SCENE's `u_ground_radiance` -- one `[r, g, b]` per integer zoom,
    /// indexed by zoom -- that every colour compiled in this scope is lit
    /// with, or `None` for a compile with no lighting at all.
    ///
    /// Deliberately the already-EVALUATED radiance rather than the `lights`
    /// above, and deliberately separate from them, because in the reference
    /// those are two different things. `Style.mergeAll`
    /// (`src/style/style.ts`, Mapbox GL JS 3.15.0) evaluates each fragment's
    /// lights in that FRAGMENT's own scope and hoists the results onto the
    /// root style, and `painter.uploadCommonLightUniforms` then uploads that
    /// one pair for every layer of every fragment. So the radiance is scene-
    /// global while the expressions that produced it are not: a root-document
    /// layer is lit by an imported fragment's lights without gaining that
    /// fragment's `config`, which is exactly what this field carries and
    /// `lights` could not. Set with `with_ground_radiance`; read by
    /// `style_spec::light_ground_color`.
    ground_radiance: Option<&'a [[f32; 3]]>,
    /// The `color-theme` colour cube every colour compiled in this scope is
    /// graded through, or `None` for a document that declares none (every
    /// style before Mapbox Standard, both vendored fixtures included --
    /// which is what keeps their colours byte-identical).
    ///
    /// Per-DOCUMENT, unlike `ground_radiance`, and deliberately so: in the
    /// reference each fragment `Style` reads its OWN `stylesheet['color-
    /// theme']` and assigns the resulting cube to its own layers
    /// (`Style._load` and `Style._reloadColorTheme`, Mapbox GL JS 3.15.0),
    /// where lighting is hoisted onto the root and shared. So a root-
    /// document layer is NOT graded by an imported basemap's theme, and the
    /// scope this rides on is exactly the per-document one that already
    /// carries `config`. Set with `with_lut`; read by
    /// `style_spec::themed_and_lit_color`.
    lut: Option<&'a ColorLut>,
    /// The feature a bake-time evaluation reads `get`/`has` from. `None` at
    /// compile time, where a `get` stays an error exactly as before -- the
    /// constant folder never sees a feature.
    feature: Option<&'a dyn crate::mvt::TagLookup>,
    /// The line progress `["line-progress"]` reads -- a fraction `0..=1`
    /// along a LineString -- or `None` everywhere except inside a
    /// `line-gradient` evaluation (`overlay::source_paint::LineGradient`).
    /// MapLibre allows the operator only in `line-gradient`
    /// (`src/style-spec/expression/definitions/index.ts`, the
    /// `line-progress` entry, and the expression parser's "only allowed in
    /// line-gradient" check); here any other use is an evaluation error,
    /// which is what a scope without progress gives. Set with
    /// `with_line_progress`.
    line_progress: Option<f64>,
}

impl Default for Scope<'_> {
    /// [`Scope::root`].
    fn default() -> Self {
        Scope::root()
    }
}

impl<'a> Scope<'a> {
    /// The empty scope: no `let` frames, so every name is unbound. This is
    /// what every entry point into the expression compiler passes.
    pub const fn root() -> Self {
        Scope {
            bindings: &[],
            enclosing: None,
            zoom: 0,
            config: None,
            lights: None,
            ground_radiance: None,
            lut: None,
            feature: None,
            line_progress: None,
        }
    }

    /// The same scope, folding at `zoom`. See the `zoom` field.
    pub const fn at_zoom(self, zoom: u8) -> Self {
        Scope { zoom, ..self }
    }

    /// The zoom this scope folds at.
    pub const fn zoom(&self) -> u8 {
        self.zoom
    }

    /// The same scope, resolving `["config", k]` against `config`. See the
    /// `config` field.
    pub const fn with_config(self, config: &'a ResolvedConfig) -> Self {
        Scope { config: Some(config), ..self }
    }

    /// The config this scope resolves `["config", k]` against, or `None` if
    /// this compile has nothing to resolve against.
    pub const fn config(&self) -> Option<&'a ResolvedConfig> {
        self.config
    }

    /// The same scope, resolving `["measure-light", "brightness"]` against
    /// `lights` (an `Import`'s `data["lights"]`, untouched JSON). See the
    /// `lights` field.
    pub const fn with_lights(self, lights: &'a Value) -> Self {
        Scope { lights: Some(lights), ..self }
    }

    /// The raw `lights` array this scope resolves `measure-light` against,
    /// or `None` if this compile has no lights to resolve against.
    pub const fn lights(&self) -> Option<&'a Value> {
        self.lights
    }

    /// The same scope, lighting every colour compiled in it with
    /// `radiance`. See the `ground_radiance` field.
    pub const fn with_ground_radiance(self, radiance: &'a [[f32; 3]]) -> Self {
        Scope { ground_radiance: Some(radiance), ..self }
    }

    /// The scene's per-zoom ground radiance, or `None` when this compile
    /// applies no lighting. See the `ground_radiance` field.
    pub const fn ground_radiance(&self) -> Option<&'a [[f32; 3]]> {
        self.ground_radiance
    }

    /// The same scope, grading every colour compiled in it through `lut`.
    /// See the `lut` field.
    pub const fn with_lut(self, lut: &'a ColorLut) -> Self {
        Scope { lut: Some(lut), ..self }
    }

    /// The document's `color-theme` cube, or `None` when this document
    /// declares no theme. See the `lut` field.
    pub const fn lut(&self) -> Option<&'a ColorLut> {
        self.lut
    }

    /// A scope that evaluates `["get", k]`/`["has", k]` against `tags` --
    /// the bake-time, per-feature use of this same evaluator (see
    /// `LayoutValue::FeatureExpression`).
    pub const fn with_feature(self, tags: &'a dyn crate::mvt::TagLookup) -> Self {
        Scope { feature: Some(tags), ..self }
    }

    /// The same scope, answering `["line-progress"]` with `progress` (a
    /// fraction `0..=1` along the line). Only `line-gradient` evaluates in
    /// such a scope; everywhere else `["line-progress"]` is an error. See
    /// the `line_progress` field.
    pub const fn with_line_progress(self, progress: f64) -> Self {
        Scope { line_progress: Some(progress), ..self }
    }

    /// Pushes a `let`'s bindings onto this scope, yielding the scope its
    /// body is compiled in. Non-allocating: the returned `Scope` borrows
    /// both `bindings` and `self`, so the caller keeps them alive on its
    /// own stack frame for exactly as long as the body is being compiled.
    pub fn push(&'a self, bindings: &'a [(&'a str, &'a Value)]) -> Scope<'a> {
        Scope {
            bindings,
            enclosing: Some(self),
            zoom: self.zoom,
            config: self.config,
            lights: self.lights,
            ground_radiance: self.ground_radiance,
            lut: self.lut,
            feature: self.feature,
            line_progress: self.line_progress,
        }
    }

    /// The innermost binding of `name` -- its still-unevaluated JSON, and
    /// the scope that JSON must be evaluated in -- or `None` if the name is
    /// unbound.
    ///
    /// The second half is what makes declaration-order shadowing work.
    /// Bindings hold raw JSON, not values, so a `var` is resolved by
    /// evaluating what it names; evaluating that in the USE site's scope
    /// would make `["let", "x", 1, "x", ["+", ["var", "x"], 10], ...]` --
    /// legal, and the shape a `let` that adjusts an outer binding takes --
    /// refer to itself and recurse forever. So the scope returned here is
    /// everything declared strictly BEFORE the binding that matched.
    pub fn lookup(&self, name: &str) -> Option<(&'a Value, Scope<'a>)> {
        let mut scope = self;
        loop {
            if let Some(index) = scope.bindings.iter().rposition(|(bound, _)| *bound == name) {
                let declaring = Scope {
                    bindings: &scope.bindings[..index],
                    enclosing: scope.enclosing,
                    zoom: scope.zoom,
                    config: scope.config,
                    lights: scope.lights,
                    ground_radiance: scope.ground_radiance,
                    lut: scope.lut,
                    feature: scope.feature,
                    line_progress: scope.line_progress,
                };
                return Some((scope.bindings[index].1, declaring));
            }
            scope = scope.enclosing?;
        }
    }

    /// Whether any binding visible here -- in this frame or an enclosing one
    /// -- satisfies `predicate`. `var` substitution is lazy, so a body that
    /// never writes `["zoom"]` can still fold differently at different
    /// zooms because one of its bindings does; this is how `compile_folded`
    /// finds that out.
    fn any_binding(&self, predicate: &dyn Fn(&Value) -> bool) -> bool {
        let mut scope = self;
        loop {
            if scope.bindings.iter().any(|(_, value)| predicate(value)) {
                return true;
            }
            match scope.enclosing {
                Some(next) => scope = next,
                None => return false,
            }
        }
    }
}

/// The interpolation curve declared by an `interpolate` expression's second
/// element -- `["linear"]`, `["exponential", base]`, or
/// `["cubic-bezier", x1, y1, x2, y2]`.
///
/// `pub(crate)` because the layout side needs it too: symbol `text-size`
/// expressions whose stop OUTPUTS are per-feature `step` expressions can't
/// go through `compile_zoom_value` at all (they compile to a
/// `LayoutValue::ZoomOfDataStep`, not a `(Filter, ZoomValue)` pair), but
/// they must ease along exactly the same curve. Sharing this type is what
/// keeps the two sides agreeing on what a given curve means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Interpolation {
    /// `["exponential", base]`, and also `["linear"]` -- linear is
    /// mathematically the `base == 1.0` case (a constant rate of change),
    /// and real Liberty data has one `["linear", 1]` occurrence where the
    /// stray `1` means exactly that.
    Exponential(f64),
    /// `["cubic-bezier", x1, y1, x2, y2]`: the CSS easing curve, a unit
    /// cubic bezier with fixed endpoints P0 = (0,0) and P3 = (1,1) and the
    /// two declared control points in between.
    CubicBezier { x1: f64, y1: f64, x2: f64, y2: f64 },
}

impl Interpolation {
    /// Parses the `interpolate` expression's second element.
    pub(crate) fn parse(type_json: &Value, json: &Value) -> Result<Self> {
        let type_arr = type_json.as_array().ok_or_else(|| {
            anyhow::anyhow!("\"interpolate\" expression's second element must be an interpolation-type array like [\"linear\"], [\"exponential\", base] or [\"cubic-bezier\", x1, y1, x2, y2]: {json}")
        })?;
        let interp_type = type_arr.first().and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!("\"interpolate\" expression's second element must be an interpolation-type array like [\"linear\"], [\"exponential\", base] or [\"cubic-bezier\", x1, y1, x2, y2]: {json}")
        })?;
        match interp_type {
            "linear" => Ok(Interpolation::Exponential(1.0)),
            "exponential" => Ok(Interpolation::Exponential(type_arr.get(1).and_then(Value::as_f64).ok_or_else(
                || anyhow::anyhow!("\"exponential\" interpolation type must include a numeric base as its second element: {json}"),
            )?)),
            // Four distinct control-point sets appear in the vendored
            // Mapbox Streets style ([0.2,0,0.7,1] on `country-label`,
            // [0.85,0.7,0.65,1] on `state-label`, [0.5,0,1,1] on
            // `settlement-subdivision-label`, [0.2,0,0.9,1] elsewhere).
            "cubic-bezier" => {
                let points: Vec<f64> = type_arr[1..].iter().filter_map(Value::as_f64).collect();
                if points.len() != 4 || type_arr.len() != 5 {
                    bail!("\"cubic-bezier\" interpolation type needs exactly four numeric control-point coordinates [x1, y1, x2, y2]: {json}");
                }
                Ok(Interpolation::CubicBezier { x1: points[0], y1: points[1], x2: points[2], y2: points[3] })
            }
            other => bail!("unsupported interpolation type {other:?} (only \"linear\", \"exponential\" and \"cubic-bezier\" are supported): {json}"),
        }
    }

    /// The eased fraction in `[0, 1]` for zoom `z` between bracketing stop
    /// zooms `za` and `zb` -- what `interpolate_at` hands to the type's
    /// `ZoomExprValue::INTERPOLATOR` to mix the two stops' values.
    ///
    /// The exponential case is MapLibre's own formula,
    /// `(base^(z-za) - 1) / (base^(zb-za) - 1)`, which depends on the raw
    /// zoom SPAN and not just the normalized position, degenerating to
    /// `(z-za) / (zb-za)` at `base == 1` where it would otherwise divide by
    /// zero. That is why this takes three zooms rather than one fraction.
    pub(crate) fn t_at(&self, z: f64, za: f64, zb: f64) -> f64 {
        let fraction = (z - za) / (zb - za);
        match *self {
            Interpolation::Exponential(base) if (base - 1.0).abs() < 1e-9 => fraction,
            Interpolation::Exponential(base) => (base.powf(z - za) - 1.0) / (base.powf(zb - za) - 1.0),
            Interpolation::CubicBezier { x1, y1, x2, y2 } => cubic_bezier_ease(fraction, x1, y1, x2, y2),
        }
    }
}

/// Evaluates a CSS-style unit cubic bezier easing curve at `fraction`.
///
/// The curve is parametric: `x(t)` and `y(t)` are independent cubics in the
/// parameter `t`, and `fraction` is an X coordinate, not `t`. So this first
/// SOLVES `x(t) = fraction` for `t` and only then evaluates `y(t)` -- the
/// standard implementation (WebKit's `UnitBezier`, and the one MapLibre GL
/// JS ships as its own `UnitBezier`).
///
/// Newton-Raphson converges in a handful of iterations for the well-behaved
/// curves real styles use, but it can stall where `x'(t)` is near zero (a
/// control point on the X axis makes the curve momentarily flat), so a
/// bisection fallback guarantees termination and accuracy for every input.
fn cubic_bezier_ease(fraction: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    // Polynomial coefficients for a unit bezier with P0 = 0 and P3 = 1:
    // f(t) = ((a*t + b)*t + c)*t.
    let coefficients = |p1: f64, p2: f64| {
        let c = 3.0 * p1;
        let b = 3.0 * (p2 - p1) - c;
        (1.0 - c - b, b, c)
    };
    let (ax, bx, cx) = coefficients(x1, x2);
    let (ay, by, cy) = coefficients(y1, y2);
    let sample_x = |t: f64| ((ax * t + bx) * t + cx) * t;
    let sample_y = |t: f64| ((ay * t + by) * t + cy) * t;
    let sample_dx = |t: f64| (3.0 * ax * t + 2.0 * bx) * t + cx;

    // The endpoints are exact by construction; short-circuiting them also
    // keeps the solver from having to converge onto them.
    if fraction <= 0.0 {
        return 0.0;
    }
    if fraction >= 1.0 {
        return 1.0;
    }

    const EPSILON: f64 = 1e-7;
    let mut t = fraction; // x(t) ~ t is a good first guess for these curves.
    for _ in 0..8 {
        let error = sample_x(t) - fraction;
        if error.abs() < EPSILON {
            return sample_y(t);
        }
        let derivative = sample_dx(t);
        if derivative.abs() < 1e-6 {
            break; // Flat here -- Newton would take a wild step; bisect instead.
        }
        t -= error / derivative;
    }

    // Bisection fallback over the full valid parameter range. `x(t)` is
    // monotonically non-decreasing on [0, 1] for any control points in
    // [0, 1], so the bracket is always valid.
    let (mut low, mut high) = (0.0f64, 1.0f64);
    t = fraction.clamp(low, high);
    for _ in 0..60 {
        let x = sample_x(t);
        if (x - fraction).abs() < EPSILON {
            break;
        }
        if x < fraction {
            low = t;
        } else {
            high = t;
        }
        t = (low + high) / 2.0;
    }
    sample_y(t)
}

/// Compiles a paint-property's raw JSON expression value into one or more
/// `(Filter, ZoomValue<T>)` pairs. `parse_leaf` converts a leaf JSON value
/// (a bare literal, or a `match` case's terminal output) into `T` -- e.g.
/// `|v: &Value, _| v.as_str().and_then(...).and_then(parse_css_color)` for
/// colors, or `|v: &Value, _| v.as_f64().map(|f| f as f32)` for numbers. It
/// takes `&Value` rather than `&str`/`f64` directly because the same
/// generic recursion has to work for both string-encoded leaves (colors)
/// and numeric leaves (widths/opacities) without knowing which to expect
/// ahead of time.
///
/// `scope` is the lexical scope the expression is compiled in, passed down
/// unchanged through every recursion -- including into `parse_leaf`, which
/// needs it because an unrecognized array (the `_` arm below) is handed
/// straight to the leaf parser, so a leaf can itself be an expression over
/// bound names. Nothing binds a name yet; every caller passes
/// `Scope::root()`. See `Scope` for why this is a parameter rather than
/// something `parse_leaf` captures.
pub fn compile_zoom_value<T: ZoomExprValue>(
    expr_json: &Value,
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    let Some(arr) = expr_json.as_array() else {
        // Not an array at all: a bare literal, e.g. park's
        // `"fill-color": "#d8e8c8"` or waterway_tunnel's `"line-opacity": 1`.
        return Ok(vec![(Filter::Any, ZoomValue::constant(parse_leaf(expr_json, scope)?))]);
    };

    match arr.first().and_then(Value::as_str) {
        Some("interpolate") => compile_interpolate(arr, parse_leaf, expr_json, scope),
        Some("match") => compile_match(arr, parse_leaf, expr_json, scope),
        Some("step") => compile_step(arr, parse_leaf, expr_json, scope),
        // `let` binds names and compiles its BODY through this same
        // function, so a `match` or `interpolate` under a `let` keeps
        // expanding exactly as it would without one -- the bindings are
        // simply visible inside each case.
        Some("let") => compile_let(arr, parse_leaf, expr_json, scope),
        Some("case") => {
            // ["case", cond1, out1, ..., fallback]: real `case` semantics
            // are if/elif/else, not if/if/else -- arm *k* fires when its
            // OWN condition is true and every earlier arm's condition is
            // false. So each arm's own cases are ANDed with both that
            // arm's condition AND the negation of every condition before
            // it (`negations`, accumulated as we go); the fallback's cases
            // are ANDed with the negation of every condition, the same
            // `negations` list carried one step further. Unlike `match`,
            // whose case labels are mutually exclusive by construction (a
            // tag has one value), an arbitrary boolean `case` condition
            // carries no such guarantee, so the running negation is load-
            // bearing here even though `compile_match` doesn't need one for
            // its explicit cases.
            if arr.len() < 4 || arr.len() % 2 != 0 {
                bail!("\"case\" must have the form [\"case\", cond, out, ..., fallback]: {expr_json}");
            }
            let mut out = Vec::new();
            let mut negations: Vec<Filter> = Vec::new();
            let mut i = 1;
            let mut reached_fallback = true;
            while i + 1 < arr.len() {
                // A condition made only of constants and `let` bindings has
                // ONE answer for the whole style, and no `Filter` can
                // express it -- the Standard `landcover` layer guards a
                // branch with `["==", ["var", "s_colorLand"], 0]`. Folding
                // it first decides the arm outright: true takes it (and
                // ends the chain, since nothing after an unconditionally-
                // true arm can fire), false drops it. Only a condition the
                // folder cannot answer goes to `compile_case_condition`,
                // which is where every feature-dependent condition still
                // lands.
                match eval(&arr[i], scope, scope.zoom()) {
                    Ok(folded) if folded.truthy() => {
                        for (filter, value) in compile_zoom_value(&arr[i + 1], parse_leaf, scope)? {
                            let mut all = negations.clone();
                            all.push(filter);
                            out.push((crate::style_spec::and_filters(all), value));
                        }
                        reached_fallback = false;
                        break;
                    }
                    Ok(_) => {
                        i += 2;
                        continue;
                    }
                    Err(_) => {}
                }
                let condition = compile_case_condition(&arr[i])?;
                for (filter, value) in compile_zoom_value(&arr[i + 1], parse_leaf, scope)? {
                    let mut all = negations.clone();
                    all.push(condition.clone());
                    all.push(filter);
                    out.push((crate::style_spec::and_filters(all), value));
                }
                negations.push(Filter::Not(Box::new(condition)));
                i += 2;
            }
            if reached_fallback {
                for (filter, value) in compile_zoom_value(&arr[arr.len() - 1], parse_leaf, scope)? {
                    let mut all = negations.clone();
                    all.push(filter);
                    out.push((crate::style_spec::and_filters(all), value));
                }
            }
            Ok(out)
        }
        // An array whose first element isn't one of the two operators this
        // zoom-only compiler recognizes -- covers both "not an operator
        // string at all" and "a real style-spec operator this function
        // doesn't implement". Two real kinds of array-shaped paint values
        // hit this branch in Liberty: `["get", "render_min_height"/
        // "render_height"]` (building-3d's `fill-extrusion-base`/`-height`,
        // a data-driven, non-zoom property), and plain numeric-tuple
        // `line-dasharray` values like `[1, 1.5]`/`[3, 3]` (17 occurrences
        // across park_outline/waterway_tunnel/tunnel_motorway_link_casing/
        // etc, verified by parsing the vendored file directly). Falling
        // through to `parse_leaf` rather than a bespoke error here is what
        // makes both work out: `color_leaf`/`number_leaf` can't make sense
        // of an array and so still yield the `Err` the "anything else ->
        // Err" requirement wants, while `dash_leaf` reads exactly this
        // bare-array shape as the dash pattern it is.
        _ if is_folded_expression(expr_json) => match hoist_feature_dependent_case(expr_json) {
            // A recognized constant-folder operator (`-`, `hsl`, `at`, ...)
            // with a feature-dependent `match`/`case` buried one argument
            // deep -- exactly what a feature-dependent `let` binding
            // inlines into (see `compile_let`'s doc comment). The leaf/
            // constant-fold path this arm would otherwise take
            // (`compile_folded` -> `eval()`) has no feature context and can
            // never resolve one (`eval_inner` has no `"get"` arm at all);
            // hoisting first and recompiling gets the recursion back to
            // THIS function's own `match`/`case` dispatch, which can.
            Some(hoisted) => compile_zoom_value(&hoisted, parse_leaf, scope),
            None => compile_folded(expr_json, parse_leaf, scope),
        },
        _ => Ok(vec![(Filter::Any, ZoomValue::constant(parse_leaf(expr_json, scope)?))]),
    }
}

/// Rebuilds `expr` -- `["op", arg0, ..., argN]` -- with ONE feature-
/// dependent `match`/`case` child hoisted to the top: `op(..., match(input,
/// L1, V1, ..., Ln, Vn, Dflt), ...)` becomes `match(input, L1, op(...,V1,
/// ...), ..., Ln, op(...,Vn,...), op(...,Dflt,...))`, and likewise for
/// `case`'s `(cond, out)` pairs plus its fallback.
///
/// Only called from `compile_zoom_value`'s fallback arm, on an expression
/// whose head is already a recognized `FOLDED_OPERATORS` name -- so this
/// never touches a `match`/`case`/`interpolate`/`step`/`let` at its OWN top
/// level; those already have their own dispatch arms above.
///
/// Two DELIBERATE, conservative limits, neither exercised by either
/// vendored fixture today, so neither is worth widening speculatively.
///
/// Both are safe in the sense that matters -- an expression this function
/// declines to hoist compiles exactly as it did before the hoist existed,
/// so nothing that used to work stops working and no wrong number is ever
/// produced. What it does NOT mean is "never a lost layer": this arm is
/// `compile_zoom_value`'s SHARED fallback, reached by every paint property,
/// and the consequence of a refusal is the CALLER's. Only
/// `compile_emissive_strength` absorbs one into the property's spec default
/// and keeps the layer; `fill-color`, `line-color` and friends route their
/// failures through `try_or_skip!`, which drops the whole layer. That was
/// true before this function existed and is unchanged by it -- but the
/// blast radius of widening either limit is every paint property, not just
/// the one this was written for.
///
/// - Scans only `expr`'s DIRECT children, not recursively (NOT
///   depth-first): `["-", 10, ["*", 2, case]]`, with the feature-dependent
///   `case` two levels down inside the `*`, is not hoisted here -- `*`'s
///   own recursive `compile_zoom_value` call would need to hoist it out of
///   `*` first, and nothing currently drives that second call for a
///   sub-expression that was never itself dispatched through
///   `compile_zoom_value` (it's a plain JSON child, inspected in place).
/// - For a `case` child, tests only `child_arr[1]`, its FIRST condition,
///   for feature-dependence. A multi-condition `case` whose feature
///   dependence is on its SECOND (or later) condition, with an earlier
///   constant one, is not hoisted either.
///
/// Both shapes fall through to the caller's normal `compile_folded`/
/// `eval()` path, which then fails on the unresolved `["get", ...]`
/// (`eval_inner` has no `"get"` arm at all) and degrades.
///
/// Only ever hoists the FIRST direct child that DOES qualify, in argument
/// order; a second qualifying child is left for a later call to find (each
/// branch this produces is itself recompiled via `compile_zoom_value`, from
/// `compile_zoom_value`'s own `match`/`case` arms), so two independent
/// feature-dependent DIRECT children of the SAME operator only get pulled
/// apart one at a time -- also unexercised, also still correct.
///
/// Returns `None` when there is nothing to hoist by the above rules:
/// `expr` isn't an array, or no direct child qualifies -- in which case the
/// caller falls back to its normal `compile_folded`/`eval()` path,
/// unchanged.
fn hoist_feature_dependent_case(expr: &Value) -> Option<Value> {
    let arr = expr.as_array()?;
    let rebuild_with = |i: usize, replacement: Value| {
        let mut rebuilt = arr.to_vec();
        rebuilt[i] = replacement;
        Value::Array(rebuilt)
    };
    for (i, child) in arr.iter().enumerate() {
        if i == 0 {
            continue; // the operator name itself
        }
        let Some(child_arr) = child.as_array() else { continue };
        match child_arr.first().and_then(Value::as_str) {
            Some("match") if child_arr.len() >= 5 && (child_arr.len() - 3).is_multiple_of(2) => {
                let input = &child_arr[1];
                if feature_operator_in(input).is_none() {
                    continue;
                }
                let mut hoisted = vec![Value::String("match".to_string()), input.clone()];
                let mut j = 2;
                while j + 1 < child_arr.len() - 1 {
                    hoisted.push(child_arr[j].clone());
                    hoisted.push(rebuild_with(i, child_arr[j + 1].clone()));
                    j += 2;
                }
                hoisted.push(rebuild_with(i, child_arr[child_arr.len() - 1].clone()));
                return Some(Value::Array(hoisted));
            }
            Some("case") if child_arr.len() >= 4 && child_arr.len().is_multiple_of(2) => {
                let condition = &child_arr[1];
                if feature_operator_in(condition).is_none() {
                    continue;
                }
                let mut hoisted = vec![Value::String("case".to_string())];
                let mut j = 1;
                while j + 1 < child_arr.len() {
                    hoisted.push(child_arr[j].clone());
                    hoisted.push(rebuild_with(i, child_arr[j + 1].clone()));
                    j += 2;
                }
                hoisted.push(rebuild_with(i, child_arr[child_arr.len() - 1].clone()));
                return Some(Value::Array(hoisted));
            }
            _ => continue,
        }
    }
    None
}

/// The highest zoom `compile_folded` resamples to. `ZoomValue`'s stops are
/// `u8` and the runtime zoom is `display_zoom() = camera.zoom.floor()`, so
/// covering every integer zoom a tile can be matched at is exact rather
/// than approximate; 22 is the deepest zoom any real vector tile source in
/// this crate serves.
pub(crate) const MAX_FOLD_ZOOM: u8 = 22;

/// The operator names that route an otherwise-unrecognized array to the
/// constant folder rather than straight to `parse_leaf`.
///
/// Deliberately narrow. `literal` and a bare numeric array belong to
/// `dash_leaf`; `match`/`case`/`step`/`interpolate` have compiler arms
/// above that must keep producing per-feature and per-zoom variation
/// instead of collapsing to one value; `let` has its own arm. What is left
/// is the set that can only ever mean "compute one value", which is exactly
/// what the folder does.
///
/// `"config"` has to be here too, not just handled inside `eval_inner`: 11
/// real layers in the vendored Mapbox Standard fixture
/// (`imports[0].data.layers`, e.g. `waterway`, `admin-0-boundary`) write a
/// paint value as a BARE `["config", key]`, with no enclosing `at`/`hsl`/
/// arithmetic to already be a recognized operator. Without `"config"`
/// listed here, that shape never reaches `is_folded_expression` at all, so
/// it falls straight through to `parse_leaf` (`color_leaf`/`number_leaf`),
/// which reject an array with a plain, non-`Unfoldable` error --
/// indistinguishable from an unsupported construct, so `try_or_skip!` drops
/// the whole layer instead of degrading the one property, even though the
/// key resolves perfectly well.
const FOLDED_OPERATORS: &[&str] = &[
    "var", "at", "to-hsla", "hsl", "hsla", "+", "-", "*", "/", "%", "abs", "min", "max", "to-number", "to-boolean",
    "to-string", "config",
];

fn is_folded_expression(expr: &Value) -> bool {
    expr.as_array()
        .and_then(|arr| arr.first())
        .and_then(Value::as_str)
        .is_some_and(|head| FOLDED_OPERATORS.contains(&head))
}

/// `["interpolate", type, input, ...]` where `input` is neither `["zoom"]`
/// nor feature-dependent, but an expression that FOLDS TO A NUMBER right
/// here -- so the whole curve has one answer for the entire style.
///
/// This is the one and only shape of `interpolate` that may reach the
/// constant folder, and the reason `"interpolate"` is deliberately absent
/// from `FOLDED_OPERATORS`. Listing it there would route every
/// array-shaped colour and number leaf headed `"interpolate"` through
/// `eval`, whose own `interpolate` arm needs the input to fold to a plain
/// number -- which is exactly what an ordinary zoom-driven ramp must NOT
/// do, since it has to keep varying per zoom at runtime.
///
/// The distinction cannot be made syntactically. Two real, structurally
/// identical expressions want opposite outcomes:
///
/// - `["interpolate", ["linear"], ["measure-light", "brightness"], 0, dark,
///   0.02, plain]` -- the Standard document's darkening ramps, resolved
///   from the style's own `lights`. `water`'s `fill-color` nests one as the
///   zoom-14 stop of an outer zoom curve; 5 more layers write one as a
///   whole paint value. These MUST fold, or the layer is dropped.
/// - `["interpolate", ["linear"], ["get", "min_depth"], ...]` -- Mapbox
///   Streets' `water-depth`. This must NOT fold, and must not even be
///   reported as a folder refusal: an `Unfoldable` would send it to
///   `or_degrade`, which would resurrect a layer that is correctly skipped
///   today and paint it in a literal colour nobody asked for.
///
/// Asking whether the INPUT folds separates them exactly. A `true` here
/// means every later failure really is a folder refusal and should degrade
/// the property; a `false` leaves the expression on the unsupported-
/// construct path it has always been on.
fn constant_interpolate_input(expr: &Value, scope: Scope<'_>) -> Option<f64> {
    let arr = expr.as_array()?;
    if arr.first().and_then(Value::as_str) != Some("interpolate") {
        return None;
    }
    let input = arr.get(2)?;
    if input.as_array().is_some_and(|input| input.first().and_then(Value::as_str) == Some("zoom")) {
        return None;
    }
    eval(input, scope, scope.zoom()).ok()?.as_number().ok()
}

fn is_constant_input_interpolate(expr: &Value, scope: Scope<'_>) -> bool {
    constant_interpolate_input(expr, scope).is_some()
}

/// The ONE stop a constant `input` selects outright, if it selects one:
/// at or below the first stop, at or above the last (the style spec clamps
/// outside the declared range), or exactly on an interior stop. `None`
/// means the input falls strictly between two stops and the answer is a
/// blend of both.
///
/// Every darkening ramp in the vendored Standard fixture takes the clamped
/// path: night's brightness is 0, at or below every ramp's first stop.
fn interpolate_stop_selected_by<'v>(stops: &[(f64, &'v Value)], input: f64) -> Option<&'v Value> {
    let (first_at, first) = *stops.first()?;
    if input <= first_at {
        return Some(first);
    }
    let (last_at, last) = *stops.last()?;
    if input >= last_at {
        return Some(last);
    }
    stops.iter().find(|&&(at, _)| at == input).map(|&(_, value)| value)
}

/// Emits the `ZoomValue<T>` for an expression the constant folder owns,
/// through the same seam `compile_interpolate` uses: fold the expression at
/// each integer zoom and hand the results over as stops.
///
/// The overwhelming majority of folded expressions do not read the zoom at
/// all -- the Standard document has none that do -- so the flat case is
/// checked first and collapses to a single constant stop, which keeps the
/// compiled `Style` the same size it was.
fn compile_folded<T: ZoomExprValue>(
    expr_json: &Value,
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    if !mentions_zoom(expr_json) && !scope.any_binding(&mentions_zoom) {
        return Ok(vec![(Filter::Any, ZoomValue::constant(parse_leaf(expr_json, scope.at_zoom(0))?))]);
    }
    // Stepped, not interpolated: there is a stop at every zoom `value_at`
    // can be asked about, so stepping between them is exact, and it is the
    // only form available to a `T` with no interpolator.
    let stops = (0..=MAX_FOLD_ZOOM)
        .map(|zoom| Ok((zoom, parse_leaf(expr_json, scope.at_zoom(zoom))?)))
        .collect::<Result<Vec<_>>>()?;
    Ok(vec![(Filter::Any, ZoomValue::stepped(stops))])
}

/// `["let", name1, value1, ..., body]` as a paint value: bind, then compile
/// the body in the extended scope.
///
/// Mapbox's own `let`/`var` (`src/style-spec/expression/definitions/let.ts`,
/// `var.ts`, Mapbox GL JS 3.15.0) is pure substitution, evaluated in the
/// live context including the feature -- it places no restriction on a
/// binding being feature-dependent. This compiler used to refuse one
/// categorically here too (see `let_bindings`'s doc comment -- still
/// correct for `eval_let`'s use, which folds to ONE constant and genuinely
/// has no feature to answer with); refusing it HERE as well was a defect,
/// not a design choice: `compile_let` is the STRUCTURAL path, which CAN
/// represent per-feature variation, via `Filter`-discriminated cases.
///
/// A binding that folds to a constant given everything decided so far stays
/// on the lexical `Scope`, exactly as before this change -- zero-cost
/// (`Scope` borrows the original JSON, no clone) and behaviourally
/// unchanged for every binding that already worked. Once a binding turns
/// out to be feature-dependent, it -- and every binding after it in this
/// same `let` -- is instead SUBSTITUTED textually at each `["var", name]`
/// use site (`substitute_vars`), in both later bindings' own values and the
/// body. This turns e.g. `roads`' real `isHighClass` binding (`["match",
/// ["get", "class"], [...], true, false]`) into an ordinary feature-
/// dependent expression wherever it's referenced, which the structural
/// `match`/`case` dispatch (above, in `compile_zoom_value`) can expand --
/// directly if the substituted `var` ends up as the body's own top-level
/// `match`/`case`, or one level down inside an arithmetic/colour-
/// decomposition operator with `hoist_feature_dependent_case`'s help
/// (`roads`' real shape: `roadsBrightness - case(isHighClass, 0.1, 0.2)`).
fn compile_let<T: ZoomExprValue>(
    arr: &[Value],
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    json: &Value,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    let bindings = parse_let_bindings(arr, json)?;
    let mut scope_bindings: Vec<(&str, &Value)> = Vec::new();
    let mut substitutions: Vec<(String, Value)> = Vec::new();
    for (name, raw_value) in bindings {
        if substitutions.is_empty() {
            if feature_operator_in(raw_value).is_none() {
                scope_bindings.push((name, raw_value));
                continue;
            }
            substitutions.push((name.to_string(), raw_value.clone()));
        } else {
            // Once any earlier binding in this `let` went to substitution,
            // keep every later one on that same track too (whether or not
            // IT is itself feature-dependent) rather than splitting across
            // both mechanisms: it may reference the earlier one by `var`,
            // which only textual substitution can resolve, and folding a
            // later CONSTANT binding into the substitution map instead of
            // the lexical `Scope` is exactly as correct, just not the
            // zero-cost path.
            let value = substitute_vars(raw_value, &substitutions);
            substitutions.push((name.to_string(), value));
        }
    }
    // `push` borrows from a place, and the frame it returns lives exactly as
    // long as this call -- which is exactly as long as the body is being
    // compiled.
    let enclosing = scope;
    let inner = enclosing.push(&scope_bindings);
    let body = &arr[arr.len() - 1];
    if substitutions.is_empty() {
        compile_zoom_value(body, parse_leaf, inner)
    } else {
        compile_zoom_value(&substitute_vars(body, &substitutions), parse_leaf, inner)
    }
}

/// Textually substitutes each `["var", name]` node in `expr` with (a clone
/// of) its bound value from `bindings`, searching backward so a later
/// (shadowing) entry for the same name wins -- the same order
/// `Scope::lookup` searches in. An unmatched `["var", name]` (one this
/// `bindings` list doesn't cover, e.g. bound further out on the lexical
/// `Scope` instead -- see `compile_let`) is left exactly as it was, for
/// `eval_inner`'s own "var" arm to resolve from `Scope` the normal way.
///
/// Recurses into every array (JSON objects, e.g. an `["image", name,
/// {params}]` sprite argument, are cloned whole and NOT recursed into --
/// style expressions are arrays, so this never loses a real `var`),
/// correctly respecting a NESTED `let`'s own shadowing: that nested let's
/// binding VALUES are substituted using the names visible to it from the
/// OUTSIDE (`bindings`, extended one entry at a time as its own bindings
/// are walked -- mirroring `compile_let`'s own order-dependent evaluation,
/// so an inner binding can reference an earlier inner one or an outer one,
/// never a later inner one), but once a name is shadowed by the nested
/// let's own binding, `["var", that name]` inside ITS body resolves to the
/// inner binding, not the outer one -- the inner entry was appended last,
/// and lookup searches backward.
///
/// A `match`'s label lists (`["match", input, L1, V1, ..., Ln, Vn,
/// Default]`) are cloned verbatim, never walked into: labels are literals
/// by the spec, never evaluated (see `compile_match`'s own doc comment),
/// so a label array that happened to start with the string `"var"` (or
/// `"let"`) would otherwise be misread as an expression and rewritten. A
/// `["literal", payload]`'s payload is guarded for exactly the same reason:
/// it is data, and `eval_inner` hands it back untouched.
pub(crate) fn substitute_vars(expr: &Value, bindings: &[(String, Value)]) -> Value {
    let Value::Array(items) = expr else {
        return expr.clone();
    };
    let head = items.first().and_then(Value::as_str);
    if head == Some("var") {
        if let Some(name) = items.get(1).and_then(Value::as_str) {
            if let Some((_, value)) = bindings.iter().rev().find(|(bound, _)| bound == name) {
                return value.clone();
            }
        }
        return expr.clone();
    }
    if head == Some("let") && items.len() >= 4 && items.len().is_multiple_of(2) {
        let mut inner = bindings.to_vec();
        let mut out = vec![Value::String("let".to_string())];
        let mut i = 1;
        while i + 1 < items.len() - 1 {
            let substituted_value = substitute_vars(&items[i + 1], &inner);
            out.push(items[i].clone());
            out.push(substituted_value.clone());
            if let Some(name) = items[i].as_str() {
                inner.push((name.to_string(), substituted_value));
            }
            i += 2;
        }
        out.push(substitute_vars(&items[items.len() - 1], &inner));
        return Value::Array(out);
    }
    if head == Some("match") && items.len() >= 5 {
        // Substitute the input and every output, but clone each label list
        // (`items[i]` below) verbatim -- see this function's doc comment.
        let mut out = vec![items[0].clone(), substitute_vars(&items[1], bindings)];
        let mut i = 2;
        while i + 1 < items.len() - 1 {
            out.push(items[i].clone());
            out.push(substitute_vars(&items[i + 1], bindings));
            i += 2;
        }
        out.push(substitute_vars(&items[items.len() - 1], bindings));
        return Value::Array(out);
    }
    if head == Some("literal") {
        // A `["literal", payload]`'s payload is DATA, not an expression --
        // `eval_inner`'s own `"literal"` arm hands it back untouched -- so
        // an array element that happens to read `["var", "x"]` is a
        // two-element array of strings, not a variable reference. Same rule
        // and same reason as the `match` label guard above; without it, a
        // `let` whose body carried such a literal would have its DATA
        // rewritten. Unreachable in any real style, guarded because the
        // sibling case was.
        return expr.clone();
    }
    Value::Array(items.iter().map(|item| substitute_vars(item, bindings)).collect())
}

/// The only `case` conditions in scope: `["has", k]`, and `["all", ...]`
/// containing exactly one `["has", k]`.
///
/// Mapbox's shield `text-color` guards its newer generation with
/// `["all", ["has", k], ["to-boolean", ["coalesce", ["image", ...], ""]]]`.
/// The second clause asks whether a SPRITE exists, which cannot be answered
/// where a paint value is resolved (tags and zoom, no atlas), so it is
/// dropped and a notice is printed. Any other clause shape is an `Err`, so
/// the layer degrades as it would have before rather than silently
/// compiling to a filter that means something different.
fn compile_case_condition(condition: &Value) -> Result<Filter> {
    let arr = condition
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unsupported \"case\" condition: {condition}"))?;
    match arr.first().and_then(Value::as_str) {
        // `["match", ["get", key], labels1, true, ..., false]` -- a
        // boolean match used as a condition, the shape a feature-dependent
        // `let` binding inlines into (`roads`' real `isHighClass`, bound to
        // exactly this and branched on with `case` -- see `compile_let`'s
        // doc comment). Every non-default output must be `true` and the
        // default must be `false`; a `match` that isn't shaped that way is
        // a VALUE-producing match, not a condition, and stays unsupported.
        Some("match") if arr.len() >= 5 && (arr.len() - 3).is_multiple_of(2) => {
            let Some(key) = extract_get_key(&arr[1]) else {
                bail!("unsupported \"case\" condition: {condition}");
            };
            if arr[arr.len() - 1] != Value::Bool(false) {
                bail!("unsupported \"case\" condition (a \"match\" condition's default output must be false): {condition}");
            }
            let mut values = Vec::new();
            let mut i = 2;
            while i + 1 < arr.len() - 1 {
                if arr[i + 1] != Value::Bool(true) {
                    bail!(
                        "unsupported \"case\" condition (every \"match\" condition output but the \
                         default must be true or false): {condition}"
                    );
                }
                values.extend(extract_match_labels(&arr[i], condition)?);
                i += 2;
            }
            Ok(Filter::In { key, values })
        }
        Some("has") => {
            let key = arr
                .get(1)
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("\"has\" needs a string key: {condition}"))?;
            Ok(Filter::Has(key.to_string()))
        }
        Some("all") => {
            let mut has: Option<Filter> = None;
            for clause in &arr[1..] {
                let inner = clause
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("unsupported \"case\" condition: {condition}"))?;
                match inner.first().and_then(Value::as_str) {
                    Some("has") => {
                        if has.is_some() {
                            bail!("unsupported \"case\" condition: more than one \"has\" clause: {condition}");
                        }
                        let key = inner
                            .get(1)
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("\"has\" needs a string key: {condition}"))?;
                        has = Some(Filter::Has(key.to_string()));
                    }
                    Some("to-boolean") => {
                        eprintln!(
                            "style_spec: \"case\" condition {condition}: dropping the \
                             image-availability test, which cannot be evaluated where a \
                             paint value is resolved"
                        );
                    }
                    _ => bail!("unsupported \"case\" condition clause: {clause}"),
                }
            }
            has.ok_or_else(|| anyhow::anyhow!("unsupported \"case\" condition: {condition}"))
        }
        _ => bail!("unsupported \"case\" condition: {condition}"),
    }
}

/// The color leaf parser for `compile_zoom_value`: reconciles Task 3's
/// `&str`-based `parse_css_color` with `compile_zoom_value`'s `&Value`-based
/// `parse_leaf` bound (the latter has to take `&Value`, not `&str`, so the
/// same generic recursion also works for numeric leaves like widths, which
/// are JSON numbers, not strings).
pub fn color_leaf(value: &Value, scope: Scope<'_>) -> Result<crate::style::Rgba> {
    // `is_constant_input_interpolate` is the STOP-VALUE case: an
    // `interpolate` nested inside an enclosing curve never passes through
    // `compile_zoom_value` at all -- `compile_interpolate` and `compile_step`
    // hand each raw stop value straight to this leaf parser -- so this is the
    // only place `water`'s zoom-14 stop can be recognized. See that
    // predicate for why the two `interpolate` shapes must part ways here.
    if is_folded_expression(value) || is_constant_input_interpolate(value, scope) {
        return fold_leaf(value, scope, EvalValue::as_color);
    }
    let text = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("expected a CSS color string, got {value}"))?;
    crate::style_spec::color::parse_css_color(text)
}

/// The numeric leaf parser for `compile_zoom_value` (`line-width` and
/// friends). Errors on anything that isn't a JSON number -- notably the
/// array-shaped `line-dasharray` values, which have their own `dash_leaf`
/// and never reach this one.
pub fn number_leaf(value: &Value, scope: Scope<'_>) -> Result<f32> {
    if is_folded_expression(value) || is_constant_input_interpolate(value, scope) {
        return fold_leaf(value, scope, |folded| folded.as_number().map(|n| n as f32));
    }
    value.as_f64().map(|f| f as f32).ok_or_else(|| anyhow::anyhow!("expected a number, got {value}"))
}

/// The leaf parser `compile_emissive_strength` (`style_spec::mod`) hands to
/// `compile_zoom_value`: `number_leaf` plus one more foldable shape --
/// `match`/`case`/`step` nested as a raw stop value, e.g. the real vendored
/// `water`'s `fill-emissive-strength` (`["interpolate", ["linear"],
/// ["zoom"], 5, ["match", ["config", "theme"], "monochrome", 0.05, 0.1], 7,
/// 0]`) and `recreation-polygon`/`hillshade`'s own, and `ferry`/
/// `ferry-auto`'s `line-emissive-strength`.
///
/// `compile_interpolate`/`compile_step` hand a stop value straight to the
/// leaf parser without ever passing back through `compile_zoom_value`'s own
/// top-level `match`/`case`/`step` dispatch -- see
/// `is_constant_input_interpolate`'s doc comment for the identical story
/// with a nested `interpolate` -- and `number_leaf`'s own gate
/// (`is_folded_expression`, i.e. `FOLDED_OPERATORS`) deliberately excludes
/// `match`/`case`/`step`, for exactly the reason `FOLDED_OPERATORS`'s own
/// doc comment gives: at `compile_zoom_value`'s TOP level they must keep
/// producing per-feature/per-zoom variation, not collapse to one value.
/// That reasoning does not apply one level down, at a stop value that is
/// already required to be a single constant regardless of its shape -- so
/// here, try folding a `match`/`case`/`step`-headed stop value the same way
/// `is_constant_input_interpolate` already tries a nested `interpolate` one:
/// `eval()`'s own `match`/`case`/`step` arms (see `eval_inner`) correctly
/// refuse (`Unfoldable`, via `["get", ...]` failing with no feature in
/// scope) one whose discriminant is feature-dependent, so this is exactly
/// as conservative as the existing `interpolate` special case, not less.
///
/// Deliberately NOT folded into the shared `number_leaf`/`color_leaf`: no
/// currently-compiled property (`fill`/`line`'s color/width/opacity/dash)
/// nests a `match`/`case`/`step` as a raw stop value anywhere in either
/// vendored fixture (verified by parsing both), so widening the shared gate
/// is untested surface this task doesn't need -- kept local to
/// emissive-strength, the one property that actually requires it.
pub fn emissive_strength_leaf(value: &Value, scope: Scope<'_>) -> Result<f32> {
    let heads_a_foldable_match_shape =
        matches!(value.as_array().and_then(|arr| arr.first()).and_then(Value::as_str), Some("match" | "case" | "step"));
    if heads_a_foldable_match_shape {
        return fold_leaf(value, scope, |folded| folded.as_number().map(|n| n as f32));
    }
    number_leaf(value, scope)
}

/// The `line-dasharray` leaf parser.
///
/// Accepts a bare array (`[3, 3]`) and the `["literal", [...]]` wrapper that
/// `step` stop values use. Anything else -- and any array `DashPattern`
/// cannot represent -- yields `DashPattern::none()`, i.e. a solid line.
///
/// This parser deliberately never returns `Err`. `try_or_skip!` in
/// `compile_layer` drops the entire layer on a paint-property error, so an
/// unrecognized dash pattern must degrade to "solid", never to "this road
/// class is missing from the map".
pub fn dash_leaf(value: &Value, _scope: Scope<'_>) -> Result<DashPattern> {
    let array = match value {
        Value::Array(items) => match items.first().and_then(Value::as_str) {
            // ["literal", [2, 2, 6, 2]]
            Some("literal") => match items.get(1) {
                Some(Value::Array(inner)) => inner,
                _ => return Ok(DashPattern::none()),
            },
            // A leading string that isn't "literal" is some expression we
            // don't implement.
            Some(_) => return Ok(DashPattern::none()),
            None => items,
        },
        _ => return Ok(DashPattern::none()),
    };

    let mut runs = Vec::with_capacity(array.len());
    for item in array {
        match item.as_f64() {
            Some(n) => runs.push(n as f32),
            None => return Ok(DashPattern::none()),
        }
    }
    Ok(DashPattern::from_runs(&runs))
}

/// Detects the `["match", ["get", key], ...]` shape used as a step/
/// interpolate segment's value (see `compile_step`/`compile_interpolate`'s
/// "jointly varying" path). Returns the key if it matches; `None`
/// otherwise (including for `["match", ["geometry-type"], ...]`, which
/// isn't relevant to paint values and is deliberately not special-cased
/// here).
fn match_on_get_key(value: &Value) -> Option<&str> {
    let arr = value.as_array()?;
    if arr.first()?.as_str()? != "match" {
        return None;
    }
    let input_arr = arr.get(1)?.as_array()?;
    if input_arr.first()?.as_str()? == "get" {
        return input_arr.get(1)?.as_str();
    }
    None
}

/// Extracts a step/interpolate segment's `match`-on-`["get", key]`
/// structure into (label -> raw output JSON) plus a default raw output
/// JSON, WITHOUT recursively compiling anything yet --
/// `compile_jointly_varying_step`/`_interpolate` do that themselves once
/// they know which labels exist across every segment. Case labels are
/// flattened (a grouped multi-label case maps every one of its labels to
/// the same output). Only call this after `match_on_get_key` confirmed
/// `value` is match-shaped.
fn parse_match_segment(value: &Value) -> Result<(HashMap<String, &Value>, &Value)> {
    let key = match_on_get_key(value).expect("caller confirmed this is match-shaped via match_on_get_key");
    let mut explicit: HashMap<String, &Value> = HashMap::new();
    let mut current = value;
    loop {
        let arr = current.as_array().expect("match-shaped by construction: checked before every assignment to `current`");
        if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
            bail!("\"match\" expression must have the form [\"match\", input, case1_labels, case1_output, ..., default_output]: {current}");
        }
        let mut i = 2;
        while i + 1 < arr.len() - 1 {
            let labels = extract_match_labels(&arr[i], current)?;
            for label in labels {
                // `or_insert`, not `insert`: with the flattening below, an
                // inner `match`'s case for a label the OUTER one already
                // claimed is unreachable (Mapbox Standard's `landuse`
                // fill-opacity writes exactly that -- `sand` is in the
                // outer case group AND in the nested default's), and the
                // spec's own case order makes the first one win.
                explicit.entry(label).or_insert(&arr[i + 1]);
            }
            i += 2;
        }
        let default = &arr[arr.len() - 1];
        // A `match` whose DEFAULT output is another `match` on the SAME
        // property is one case list written in two pieces, not a nested
        // data-driven value: "these labels, else those labels, else this".
        // Flattening it here is what lets the joint step/interpolate
        // expansion see the whole list, instead of handing the inner
        // `match` to `parse_leaf` -- which has no per-feature vocabulary
        // and can only fail. Mapbox Standard's `landuse` `fill-opacity` is
        // the real instance; a nested `match` on a DIFFERENT property is
        // left alone, since one flat label set cannot express two keys.
        match match_on_get_key(default) {
            Some(inner) if inner == key => current = default,
            _ => return Ok((explicit, default)),
        }
    }
}

/// Confirms every match-shaped value in `values` uses the same `["get",
/// key]` input, and returns that key. Bails if two match-shaped segments
/// disagree (not seen in any real style; genuinely ambiguous which
/// property should drive the expansion).
fn shared_match_key<'a>(values: impl Iterator<Item = &'a Value>, json: &Value) -> Result<String> {
    let mut key: Option<String> = None;
    for v in values {
        if let Some(k) = match_on_get_key(v) {
            match &key {
                None => key = Some(k.to_string()),
                Some(existing) if existing == k => {}
                Some(existing) => bail!(
                    "\"step\"/\"interpolate\" segments use \"match\" on different properties ({existing:?} vs {k:?}), not supported: {json}"
                ),
            }
        }
    }
    Ok(key.expect("caller only invokes this after confirming at least one segment is match-shaped"))
}

/// `["step", ["zoom"], base, t1, v1, t2, v2, ...]`.
///
/// Two real shapes found in `examples/mapbox-streets-style.json` (verified
/// by parsing that file directly): `line-opacity` step expressions whose
/// base/stop values are flat literals (13 occurrences, e.g.
/// `["step", ["zoom"], 0, 14, 1]`, on `road-street`/`tunnel-street`/
/// `bridge-*-case` and others) -- now this function's highest-volume real
/// caller, since `line-opacity` is a compiled property. The other real
/// shape: `road-motorway-trunk`'s `line-color`,
/// whose base and stop are themselves `match` expressions on the SAME
/// property (`class`) but with DIFFERENT explicit case sets (`"trunk"` is
/// its own case in the base segment, but falls through to the second
/// segment's default) -- see `compile_jointly_varying_step`'s doc comment
/// for how that's handled.
fn compile_step<T: ZoomExprValue>(
    arr: &[Value],
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    json: &Value,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("\"step\" expression must have the form [\"step\", [\"zoom\"], base, t1, v1, ...]: {json}");
    }
    let is_zoom_input = arr[1].as_array().is_some_and(|z| z.first().and_then(Value::as_str) == Some("zoom"));
    // A bare `["step", ["get", key], base, t1, v1, ...]` -- the settlement
    // labels' real `text-color`, bucketed by `symbolrank` -- is the
    // one-segment case of a zoom step whose segments are property steps:
    // one zoom bracket from 0, holding that data step. So it goes through
    // the SAME bucket expansion (`Filter::NumberCompare` per bucket, a
    // missing tag landing in the base bucket as MapLibre's `step` puts a
    // below-every-stop input), rather than a second property-step path.
    if !is_zoom_input && numeric_step_on_get_key(json).is_some() {
        return compile_numerically_bucketed_step(&[(0, json)], parse_leaf, json, scope);
    }
    if !is_zoom_input {
        bail!("\"step\" expression's second element must be [\"zoom\"] (property-based step is not supported): {json}");
    }

    // (threshold, raw value) for every segment; the base's threshold is 0,
    // which `ZoomValue::stepped`'s `value_at` already treats as "applies to
    // every zoom below the next real stop" (see its doc comment) -- real
    // zoom is never negative, so this is exact, not an approximation.
    let mut segments: Vec<(u8, &Value)> = vec![(0, &arr[2])];
    let mut i = 3;
    while i + 1 < arr.len() {
        let threshold = arr[i].as_f64().ok_or_else(|| anyhow::anyhow!("\"step\" stop threshold must be a number: {json}"))?;
        segments.push((threshold as u8, &arr[i + 1]));
        i += 2;
    }

    // A segment that is a `case` -- `country-label`'s real `icon-opacity`,
    // `["step", ["zoom"], ["case", ["has", "text_anchor"], 1, 0], 7, 0]` --
    // is hoisted OUT of the step: `step(zoom, case(c, a, b), 7, 0)` is
    // `case(c, step(zoom, a, 7, 0), step(zoom, b, 7, 0))` for every feature
    // and zoom, and the rebuilt `case` goes back through
    // `compile_zoom_value`'s own `case` arm (if/elif/else filters), whose
    // arms are ordinary zoom steps again.
    if let Some(index) = arr.iter().enumerate().skip(2).find_map(|(i, v)| {
        let is_segment = i == 2 || (i >= 4 && i % 2 == 0);
        (is_segment && v.as_array().and_then(|a| a.first()).and_then(Value::as_str) == Some("case")).then_some(i)
    }) {
        let case = arr[index].as_array().expect("checked above");
        if case.len() < 4 || !case.len().is_multiple_of(2) {
            bail!("\"case\" must have the form [\"case\", cond, out, ..., fallback]: {json}");
        }
        let with_segment = |output: &Value| {
            let mut rebuilt = arr.to_vec();
            rebuilt[index] = output.clone();
            Value::Array(rebuilt)
        };
        let mut hoisted = vec![Value::String("case".into())];
        for pair in case[1..case.len() - 1].chunks(2) {
            hoisted.push(pair[0].clone());
            hoisted.push(with_segment(&pair[1]));
        }
        hoisted.push(with_segment(case.last().expect("length checked")));
        return compile_zoom_value(&Value::Array(hoisted), parse_leaf, scope);
    }

    if segments.iter().any(|&(_, v)| match_on_get_key(v).is_some()) {
        return compile_jointly_varying_step(&segments, parse_leaf, json, scope);
    }
    if segments.iter().any(|&(_, v)| numeric_step_on_get_key(v).is_some()) {
        return compile_numerically_bucketed_step(&segments, parse_leaf, json, scope);
    }

    let stops: Vec<(u8, T)> = segments.iter().map(|&(t, v)| Ok((t, parse_leaf(v, scope)?))).collect::<Result<Vec<_>>>()?;
    Ok(vec![(Filter::Any, ZoomValue::stepped(stops))])
}

/// Handles a `step` expression where at least one segment is a `match` on a
/// feature property (real example: `road-motorway-trunk`'s `line-color`,
/// see `compile_step`'s doc comment). Expands into one `(Filter,
/// ZoomValue)` pair per label seen across ANY segment, plus one default
/// pair -- mirroring `compile_match`'s expansion strategy, but with the
/// value at each zoom threshold coming from whichever segment is active
/// there rather than a single flat literal.
///
/// A segment that doesn't explicitly list a given label uses that
/// segment's own default output for it -- this is what correctly handles
/// `road-motorway-trunk`: the base segment (zoom < 9) has explicit
/// "motorway"/"trunk" cases; the zoom >= 9 segment only has an explicit
/// "motorway" case, so "trunk" at zoom >= 9 resolves to the zoom >= 9
/// segment's own default color, not the base segment's "trunk" color.
fn compile_jointly_varying_step<T: ZoomExprValue>(
    segments: &[(u8, &Value)],
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    json: &Value,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    let key = shared_match_key(segments.iter().map(|&(_, v)| v), json)?;
    let parsed: Vec<(u8, HashMap<String, &Value>, &Value)> = segments
        .iter()
        .map(|&(t, v)| {
            if match_on_get_key(v).is_some() {
                let (explicit, default) = parse_match_segment(v)?;
                Ok((t, explicit, default))
            } else {
                Ok((t, HashMap::new(), v))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let mut all_labels: Vec<String> = parsed.iter().flat_map(|(_, explicit, _)| explicit.keys().cloned()).collect();
    all_labels.sort();
    all_labels.dedup();

    let mut results = Vec::new();
    for label in &all_labels {
        let stops: Vec<(u8, T)> = parsed
            .iter()
            .map(|(t, explicit, default)| {
                let raw = explicit.get(label).copied().unwrap_or(*default);
                Ok((*t, parse_leaf(raw, scope)?))
            })
            .collect::<Result<Vec<_>>>()?;
        results.push((Filter::In { key: key.clone(), values: vec![label.clone()] }, ZoomValue::stepped(stops)));
    }

    let default_stops: Vec<(u8, T)> =
        parsed.iter().map(|(t, _, default)| Ok((*t, parse_leaf(default, scope)?))).collect::<Result<Vec<_>>>()?;
    let default_filter = Filter::Not(Box::new(Filter::In { key, values: all_labels }));
    results.push((default_filter, ZoomValue::stepped(default_stops)));

    Ok(results)
}

/// Detects a segment shaped `["step", ["get", key], base, t1, v1, ...]` --
/// a step that buckets features by a NUMERIC tag -- and names the key.
///
/// The numeric sibling of `match_on_get_key`, and the reason it is needed
/// is the same: a nested expression handed to `compile_step` as a raw
/// segment value never passes back through `compile_zoom_value`'s dispatch,
/// so this is the only place the shape can be recognised.
fn numeric_step_on_get_key(value: &Value) -> Option<&str> {
    let arr = value.as_array()?;
    if arr.first()?.as_str()? != "step" {
        return None;
    }
    let input_arr = arr.get(1)?.as_array()?;
    if input_arr.first()?.as_str()? == "get" {
        return input_arr.get(1)?.as_str();
    }
    None
}

/// One property-bucketing `step`'s buckets: the raw output JSON for each
/// `>=` threshold, with the base carried as the `None` threshold.
type NumericBuckets<'a> = Vec<(Option<f64>, &'a Value)>;

/// Splits `["step", ["get", key], base, t1, v1, ...]` into its key and its
/// `(threshold, raw output)` list, the base carried as the `None` threshold.
fn parse_numeric_step_segment<'a>(value: &'a Value, json: &Value) -> Result<(String, NumericBuckets<'a>)> {
    let arr = value.as_array().expect("the caller only routes array-shaped JSON here");
    let key = numeric_step_on_get_key(value)
        .ok_or_else(|| anyhow::anyhow!("expected a [\"step\", [\"get\", key], ...] segment: {json}"))?
        .to_string();
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("a property-bucketing \"step\" must have the form [\"step\", [\"get\", key], base, t1, v1, ...]: {json}");
    }
    let mut buckets: Vec<(Option<f64>, &Value)> = vec![(None, &arr[2])];
    let mut i = 3;
    while i + 1 < arr.len() {
        let threshold = arr[i]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("a property-bucketing \"step\"'s thresholds must be numbers: {json}"))?;
        buckets.push((Some(threshold), &arr[i + 1]));
        i += 2;
    }
    Ok((key, buckets))
}

/// Handles a zoom `step` at least one of whose segments is itself a `step`
/// bucketing features by a NUMERIC tag -- the property analogue of
/// `compile_jointly_varying_step`, which does the same job for a `match` on
/// a string tag.
///
/// The real shape this exists for is Mapbox Standard's `poi-label`:
///   `"icon-opacity": ["step", ["zoom"],
///        ["step", ["get", "sizerank"], 0, 10, 0], 17, 1]`
/// and, in Mapbox Streets and the SWAT styles, the genuinely varying
///   `["step", ["zoom"], ["step", ["get", "sizerank"], 0, 5, 1],
///        17, ["step", ["get", "sizerank"], 0, 13, 1]]`
/// -- POI icons that appear at a zoom decided by how important the POI is.
/// Without this the whole property fell back to a literal, which for an
/// opacity means "fully opaque": the icons Mapbox hides until z17 were
/// drawn from the layer's `minzoom` of 6.
///
/// # How the expansion works
///
/// Every data-step segment's thresholds are pooled into one ascending list,
/// which cuts the property's range into buckets whose boundaries no segment
/// disagrees about -- the numeric mirror of `compile_jointly_varying_step`'s
/// union of `match` labels, and of `refine_data_step_onto_thresholds` on the
/// layout side. Inside one bucket every segment holds a single value, so the
/// bucket compiles to an ordinary zoom `ZoomValue` and a `Filter` naming the
/// bucket. A segment that is NOT a data step (`poi-label`'s `1` above)
/// contributes that one value to every bucket.
///
/// # Why the low bucket is `Not(>= t)` and not `< t`
///
/// They differ on a feature whose tag is MISSING, and that difference is the
/// whole correctness of this. `Filter::NumberCompare` is false for an absent
/// or non-numeric tag, so a `<` filter would match no such feature and the
/// feature would fall out of every bucket -- losing the symbol entirely.
/// Real `step` semantics put a null input in the BASE bucket
/// (`DataStepShape::resolve` implements exactly that), and negating the
/// upper comparison is what reproduces it: `Not(>= t)` is true both below
/// the threshold and when there is nothing to compare.
fn compile_numerically_bucketed_step<T: ZoomExprValue>(
    segments: &[(u8, &Value)],
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    json: &Value,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    // Parsed once per segment, and the key agreed across all of them: two
    // segments bucketing on DIFFERENT properties have no common bucket
    // structure to refine onto, exactly as `shared_match_key` refuses two
    // `match`es on different keys.
    let mut key: Option<String> = None;
    let mut parsed: Vec<(u8, Option<NumericBuckets<'_>>, &Value)> = Vec::new();
    for &(zoom, value) in segments {
        if numeric_step_on_get_key(value).is_some() {
            let (segment_key, buckets) = parse_numeric_step_segment(value, json)?;
            match &key {
                None => key = Some(segment_key),
                Some(existing) if *existing == segment_key => {}
                Some(existing) => bail!(
                    "a zoom \"step\" whose segments bucket on DIFFERENT properties ({existing:?} and \
                     {segment_key:?}) has no common bucket structure: {json}"
                ),
            }
            parsed.push((zoom, Some(buckets), value));
        } else {
            parsed.push((zoom, None, value));
        }
    }
    let key = key.expect("the caller only routes here when a segment is a property-bucketing step");

    let mut thresholds: Vec<f64> = parsed
        .iter()
        .filter_map(|(_, buckets, _)| buckets.as_ref())
        .flat_map(|buckets| buckets.iter().filter_map(|(threshold, _)| *threshold))
        .collect();
    thresholds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    thresholds.dedup();

    // `None` is the base bucket (everything below the first threshold, and
    // every feature with no such tag at all); each `Some(t)` is the bucket
    // starting at `t` and running to the next threshold.
    let bucket_lower_bounds: Vec<Option<f64>> =
        std::iter::once(None).chain(thresholds.iter().copied().map(Some)).collect();

    let mut results = Vec::new();
    for (index, lower) in bucket_lower_bounds.iter().enumerate() {
        let stops: Vec<(u8, T)> = parsed
            .iter()
            .map(|(zoom, buckets, raw)| {
                let value = match buckets {
                    // The segment's own output in effect throughout this
                    // bucket: the last of its thresholds at or below the
                    // bucket's floor, or its base when there is none.
                    Some(buckets) => buckets
                        .iter()
                        .rev()
                        .find(|(threshold, _)| match (threshold, lower) {
                            (None, _) => true,
                            (Some(_), None) => false,
                            (Some(t), Some(l)) => *t <= *l,
                        })
                        .map_or(*raw, |(_, output)| *output),
                    None => *raw,
                };
                Ok((*zoom, parse_leaf(value, scope)?))
            })
            .collect::<Result<Vec<_>>>()?;

        let at_least = |threshold: f64| Filter::NumberCompare {
            key: key.clone(),
            op: crate::style::NumberOp::Gte,
            value: threshold,
        };
        let upper = bucket_lower_bounds.get(index + 1).copied().flatten();
        let filter = match (lower, upper) {
            (None, None) => Filter::Any,
            // See this function's doc comment: the negation, not a `<`, so
            // a feature with no such tag lands in the base bucket.
            (None, Some(hi)) => Filter::Not(Box::new(at_least(hi))),
            (Some(lo), None) => at_least(*lo),
            (Some(lo), Some(hi)) => Filter::All(vec![at_least(*lo), Filter::Not(Box::new(at_least(hi)))]),
        };
        results.push((filter, ZoomValue::stepped(stops)));
    }
    Ok(results)
}

/// Handles an `interpolate` expression where at least one stop's value is
/// a `match` on a feature property (real examples: `waterway`/
/// `waterway-shadow`/`aeroway-line`'s `line-width`, each a 2-stop
/// exponential interpolate whose stops are `match` on the same key with
/// the SAME case set at every stop). Expands into one `(Filter,
/// ZoomValue)` pair per label seen across ANY stop, plus one default pair,
/// each interpolated independently across the stops using the same
/// resampling approach as the non-match path (`interpolate_at`, resampled
/// at every integer zoom in the stops' range).
fn compile_jointly_varying_interpolate<T: ZoomExprValue>(
    raw_segments: &[(f64, &Value)],
    curve: Interpolation,
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    json: &Value,
    interpolator: fn(&T, &T, f32) -> T,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    let key = shared_match_key(raw_segments.iter().map(|&(_, v)| v), json)?;
    let parsed: Vec<(f64, HashMap<String, &Value>, &Value)> = raw_segments
        .iter()
        .map(|&(z, v)| {
            if match_on_get_key(v).is_some() {
                let (explicit, default) = parse_match_segment(v)?;
                Ok((z, explicit, default))
            } else {
                Ok((z, HashMap::new(), v))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let first_zoom = parsed.first().expect("caller's arr.len() check guarantees at least one stop").0;
    let last_zoom = parsed.last().expect("caller's arr.len() check guarantees at least one stop").0;
    let start = first_zoom.floor() as i64;
    let end = last_zoom.ceil() as i64;

    let mut all_labels: Vec<String> = parsed.iter().flat_map(|(_, explicit, _)| explicit.keys().cloned()).collect();
    all_labels.sort();
    all_labels.dedup();

    let mut results = Vec::new();
    for label in &all_labels {
        let raw_stops: Vec<(f64, T)> = parsed
            .iter()
            .map(|(z, explicit, default)| {
                let raw = explicit.get(label).copied().unwrap_or(*default);
                Ok((*z, parse_leaf(raw, scope)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let resampled = (start..=end).map(|z| (z as u8, interpolate_at(&raw_stops, z as f64, curve, interpolator))).collect();
        results.push((Filter::In { key: key.clone(), values: vec![label.clone()] }, ZoomValue::interpolated_with(resampled, interpolator)));
    }

    let default_raw_stops: Vec<(f64, T)> =
        parsed.iter().map(|(z, _, default)| Ok((*z, parse_leaf(default, scope)?))).collect::<Result<Vec<_>>>()?;
    let default_resampled = (start..=end).map(|z| (z as u8, interpolate_at(&default_raw_stops, z as f64, curve, interpolator))).collect();
    let default_filter = Filter::Not(Box::new(Filter::In { key, values: all_labels }));
    results.push((default_filter, ZoomValue::interpolated_with(default_resampled, interpolator)));

    Ok(results)
}

/// `["interpolate", [<type>, ...base?], ["zoom"], z1, v1, z2, v2, ...]`.
///
/// Verified against every `interpolate` use in `examples/liberty-style.json`
/// (88 occurrences across paint/layout): `<type>` is always `["linear"]`,
/// `["linear", 1]`, or `["exponential", <base>]` with base in
/// {1.2, 1.3, 1.4, 1.5}. The vendored Mapbox Streets style additionally uses
/// `["cubic-bezier", x1, y1, x2, y2]` (four distinct curves, all on place
/// labels); see `Interpolation`. In Liberty the input is always exactly
/// `["zoom"]` (no
/// property-based interpolation); every stop value is a literal at this
/// nesting depth (never a nested expression); stop counts are 2, 3, or 4.
///
/// Real Liberty data also has half-integer zoom stops (e.g. `12, 0, 12.5, 1`
/// in `tunnel_street_casing`'s `line-opacity`, and similarly across ~24
/// `tunnel_*`/`road_*`/`bridge_*` layers' `line-width`/`line-opacity`) even
/// though `ZoomValue`'s stops are keyed by `u8` (Task 2's design). An
/// earlier version of this function rounded each raw stop's zoom to the
/// nearest integer, which for 6 of these 8 real shapes collided two
/// distinct stops onto the same `u8` key -- and `ZoomValue::value_at_linear`
/// resolves duplicate keys to the FIRST match, silently discarding the
/// later (correct) stop and zeroing out real width ramps right at the zoom
/// a road class first appears (e.g. `tunnel_motorway_link`'s
/// `12.5,0 / 13,1.5` collapsed to `13,0` instead of `13,1.5`).
///
/// Fixed by resampling the true curve -- computed with the REAL formula for
/// the declared type (linear, or exponential with its declared base), not
/// a linear approximation between raw stops -- at every integer zoom in
/// `[floor(first_stop_zoom), ceil(last_stop_zoom)]`, and using those
/// resampled points as `ZoomValue`'s stops. Since each resampled zoom is a
/// distinct integer, duplicate keys can't occur by construction, and the
/// result is exact at every zoom `match_tile`'s `zoom: u8` parameter could
/// ever query -- that parameter being the camera's own display zoom (an
/// integer bucket, not a continuously-varying value); see `match_tile`'s
/// doc comment in `crate::style`.
///
/// This also fixes a second, independent inaccuracy the old rounding
/// approach had: "exponential degrades to linear" linearly interpolated
/// between the RAW (possibly far-apart, non-integer) stops, which could be
/// substantially wrong at a real integer zoom -- e.g. `waterway_river`'s
/// `line-width` (base 1.2, stops `11,0.5 / 20,6`): the true exponential
/// value at z15 is ~1.92px, but linearly connecting the two raw stops gave
/// ~2.94px (+53%). Resampling the true exponential formula pointwise fixes
/// this for free.
fn compile_interpolate<T: ZoomExprValue>(
    arr: &[Value],
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    json: &Value,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("\"interpolate\" expression must have the form [\"interpolate\", [type,...], [\"zoom\"], z1, v1, z2, v2, ...]: {json}");
    }

    // The style spec pins a few paint properties `"interpolated": false`
    // (`line-dasharray` is the one this crate compiles). Their value types
    // have no interpolator at all, so an `interpolate` over one is an error
    // here rather than something silently rendered as a step.
    let Some(interpolator) = T::INTERPOLATOR else {
        bail!("\"interpolate\" is not supported for this paint property: its value type cannot be blended between stops (the style spec pins it \"interpolated\": false): {json}");
    };

    let curve = Interpolation::parse(&arr[1], json)?;

    let is_zoom_input = arr[2].as_array().is_some_and(|z| z.first().and_then(Value::as_str) == Some("zoom"));
    if !is_zoom_input {
        // Not a zoom curve, so it does not vary with the camera -- but it
        // may still have ONE answer for the whole style. The Standard
        // document's darkening ramps interpolate over
        // `["measure-light", "brightness"]`, which the folder resolves from
        // the style's own `lights`; 5 fixture layers (`hillshade`,
        // `road-rail`, `road-rail-tracks`, `bridge-rail`,
        // `bridge-rail-tracks`) write a whole paint value that way.
        //
        // `constant_interpolate_input` is what separates that from a
        // feature-driven interpolate (Mapbox Streets' `water-depth`,
        // `["interpolate", ["linear"], ["get", "min_depth"], ...]`), which
        // keeps the plain error below and stays skipped whole.
        if let Some(input) = constant_interpolate_input(json, scope) {
            let mut stops: Vec<(f64, &Value)> = Vec::new();
            let mut i = 3;
            while i + 1 < arr.len() {
                let at = arr[i]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("\"interpolate\" stop input must be a number: {json}"))?;
                stops.push((at, &arr[i + 1]));
                i += 2;
            }
            // A constant input almost always picks ONE stop rather than
            // blending two (see `interpolate_stop_selected_by`), and the
            // selected stop is then COMPILED, not folded -- so a per-feature
            // `match` inside it keeps expanding into filtered cases exactly
            // as it would anywhere else. `hillshade` and
            // `recreation-polygon` are why this matters: both are
            // `["interpolate", ["linear"], ["measure-light", "brightness"],
            // ...]` whose stops are `match`es on a feature property, so
            // folding them refuses and `or_degrade` repaints two real
            // basemap layers in opaque black.
            if let Some(chosen) = interpolate_stop_selected_by(&stops, input) {
                return compile_zoom_value(chosen, parse_leaf, scope);
            }
            // Strictly between two stops. The exact answer is a blend,
            // which exists only if BOTH stops fold to a value of their own,
            // so try that first -- it is right whenever it is available.
            if let Ok(blended) = compile_folded(json, parse_leaf, scope) {
                return Ok(blended);
            }
            // And when it is not available, compile the NEARER stop rather
            // than letting the refusal reach `or_degrade`. Falling through
            // to a degradation here would re-create the exact defect the
            // clamped path above exists to prevent: `recreation-polygon`'s
            // stops are `["match", ["get", "type"], ...]`, which never fold,
            // and its domain is [0.1, 0.4] -- so a `dawn`/`default` config
            // (brightness 0.396140 from this fixture's own lights, per
            // `calculateLightsBrightness`) lands strictly inside it, and the
            // fallback colour would blanket every park and car park in
            // opaque black from z15 up. The pinned fixture is `night`
            // (brightness 0), which clamps, so the bug would have sat
            // dormant.
            //
            // The nearer stop is a deliberate approximation, and a cheap one
            // to defend: these are near-binary darkness ramps (`water`'s
            // domain is 0 to 0.02) fed by a brightness figure computed by
            // Mapbox's own formula (`measure_light_brightness`), so the
            // nearer end is the branch the style author meant. It is one of
            // the style's own declared values rather than an invented one,
            // and it keeps any
            // per-feature `match` inside it expanding into filtered cases.
            // The alternatives are painting black or dropping a real basemap
            // layer whole; neither is better than picking the end of a
            // two-ended ramp that the input is closer to.
            let (_, nearest) = stops
                .iter()
                .copied()
                .min_by(|(a, _), (b, _)| {
                    (a - input).abs().total_cmp(&(b - input).abs())
                })
                .expect("arr.len() was checked above, so there is at least one stop");
            let notice = format!(
                "{json}: its input folds to {input}, which falls between stops that \
                 cannot both be folded; compiling the nearer stop"
            );
            eprintln!("style_spec: {notice}");
            // Same recorder `or_degrade` uses, and deliberately so: this is
            // still a case where the compiled output is not a lossless fold
            // of the source style, just reached by a different refusal path
            // (a between-stops interpolate rather than a whole property).
            // The pinned-fixture test below exists specifically to catch a
            // silent approximation like this one; a separate channel would
            // only need to be checked in the same place for the same
            // reason, so it would not buy any real isolation.
            crate::style_spec::record_degradation(notice);
            return compile_zoom_value(nearest, parse_leaf, scope);
        }
        bail!("\"interpolate\" expression's third element must be [\"zoom\"] (property-based interpolation is not supported): {json}");
    }

    let mut raw_segments: Vec<(f64, &Value)> = Vec::new();
    let mut i = 3;
    while i + 1 < arr.len() {
        let zoom = arr[i].as_f64().ok_or_else(|| anyhow::anyhow!("\"interpolate\" stop zoom must be a number: {json}"))?;
        raw_segments.push((zoom, &arr[i + 1]));
        i += 2;
    }

    if raw_segments.iter().any(|&(_, v)| match_on_get_key(v).is_some()) {
        return compile_jointly_varying_interpolate(&raw_segments, curve, parse_leaf, json, interpolator, scope);
    }

    let raw_stops: Vec<(f64, T)> = raw_segments.iter().map(|&(z, v)| Ok((z, parse_leaf(v, scope)?))).collect::<Result<Vec<_>>>()?;

    let first_zoom = raw_stops.first().expect("arr.len() checked above guarantees at least one stop").0;
    let last_zoom = raw_stops.last().expect("arr.len() checked above guarantees at least one stop").0;
    let start = first_zoom.floor() as i64;
    let end = last_zoom.ceil() as i64;

    let resampled = (start..=end).map(|z| (z as u8, interpolate_at(&raw_stops, z as f64, curve, interpolator))).collect();
    Ok(vec![(Filter::Any, ZoomValue::interpolated_with(resampled, interpolator))])
}

/// The true curve of a top-level `["interpolate", curve, ["zoom"], ...]`
/// whose stops sit OFF the integer zoom grid, for
/// `ZoomValue::with_off_grid_stops`; `None` for anything else (every stop on
/// an integer, a non-zoom input, a stop value that does not parse).
///
/// `compile_zoom_value` resamples at integer zooms, which loses a stop like
/// Mapbox Standard's `15 -> 0, 15.3 -> 0.8` building fade. Asked for only
/// where a caller reads the value at a fractional zoom on the CPU (the
/// `fill-extrusion-opacity` uniform), so no other compiled value changes. A
/// linear curve is exact at its own stops; any other is also sampled every
/// 1/16 zoom between them.
pub fn compile_off_grid_stops<T: ZoomExprValue>(
    json: &Value,
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    scope: Scope<'_>,
) -> Option<Vec<(f32, T)>> {
    let arr = json.as_array()?;
    if arr.first().and_then(Value::as_str) != Some("interpolate") || arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        return None;
    }
    let interpolator = T::INTERPOLATOR?;
    if !arr[2].as_array().is_some_and(|z| z.first().and_then(Value::as_str) == Some("zoom")) {
        return None;
    }
    let curve = Interpolation::parse(&arr[1], json).ok()?;
    let mut raw_stops: Vec<(f64, T)> = Vec::new();
    for pair in arr[3..].chunks(2) {
        raw_stops.push((pair[0].as_f64()?, parse_leaf(&pair[1], scope).ok()?));
    }
    if raw_stops.iter().all(|(z, _)| z.fract() == 0.0) {
        return None;
    }
    let (first_zoom, last_zoom) = (raw_stops[0].0, raw_stops[raw_stops.len() - 1].0);
    let mut zooms: Vec<f64> = raw_stops.iter().map(|(z, _)| *z).collect();
    if !matches!(curve, Interpolation::Exponential(base) if base == 1.0) {
        let mut z = (first_zoom * 16.0).ceil() / 16.0;
        while z < last_zoom {
            zooms.push(z);
            z += 1.0 / 16.0;
        }
    }
    zooms.sort_by(f64::total_cmp);
    zooms.dedup();
    Some(zooms.into_iter().map(|z| (z as f32, interpolate_at(&raw_stops, z, curve, interpolator))).collect())
}

/// Evaluates the true `interpolate` curve at zoom `z`, bracketed by the two
/// nearest raw stops in `stops`, easing between them along the declared
/// `curve` (see `Interpolation::t_at` for each type's formula). Clamps to
/// the first/last stop's value outside their range, matching the style
/// spec's own clamping behavior.
fn interpolate_at<T: Clone>(
    stops: &[(f64, T)],
    z: f64,
    curve: Interpolation,
    interpolator: fn(&T, &T, f32) -> T,
) -> T {
    let first = &stops[0];
    if z <= first.0 {
        return first.1.clone();
    }
    let last = &stops[stops.len() - 1];
    if z >= last.0 {
        return last.1.clone();
    }
    let idx = stops
        .iter()
        .rposition(|(stop_zoom, _)| *stop_zoom <= z)
        .expect("z is strictly between first and last stop (checked above), so a lower bracket exists");
    let (za, va) = &stops[idx];
    let (zb, vb) = &stops[idx + 1];
    let t = curve.t_at(z, *za, *zb);
    interpolator(va, vb, t as f32)
}

/// `["match", ["get", key], case1_labels, case1_output, case2_labels,
/// case2_output, ..., default_output]`.
///
/// Not used by any paint property in `examples/liberty-style.json`
/// (verified by grep/parse: the only `match` uses there are in `layout`
/// `icon-image`/`text-field`, and their case outputs are `["get", ...]`
/// property lookups, not zoom/literal values this compiler's
/// `T: ZoomExprValue` bound could represent), so this shape -- and the
/// recursive handling of a
/// case output that is itself an `interpolate` -- is exercised by synthetic
/// tests built from the real style-spec pattern rather than a literal
/// Liberty citation.
///
/// Each case's label(s) becomes a `Filter::In` on `key`, ANDed
/// (`Filter::All`) with whatever `compile_zoom_value` recursively produces
/// for that case's own output. The default branch gets `Filter::Not(AnyOf(
/// all case filters))` -- "whatever no explicit case already claimed".
fn compile_match<T: ZoomExprValue>(
    arr: &[Value],
    parse_leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
    json: &Value,
    scope: Scope<'_>,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("\"match\" expression must have the form [\"match\", input, case1_labels, case1_output, ..., default_output]: {json}");
    }

    let Some(key) = extract_get_key(&arr[1]) else {
        // Not a feature lookup. It may still have one answer for the whole
        // style -- 582 of the Standard document's `match`es switch on style
        // configuration -- in which case the branch it picks is compiled on
        // its own, with no filter, exactly as if the author had written that
        // branch in place of the `match`. Note "compiled", not "folded": a
        // selected branch that varies per feature still expands.
        let input = eval(&arr[1], scope, scope.zoom()).with_context(|| {
            format!("\"match\" expression's input must be [\"get\", key] or fold to a constant: {json}")
        })?;
        let mut i = 2;
        while i + 1 < arr.len() - 1 {
            if match_labels(&arr[i]).iter().any(|label| label_matches(label, &input)) {
                return compile_zoom_value(&arr[i + 1], parse_leaf, scope);
            }
            i += 2;
        }
        return compile_zoom_value(&arr[arr.len() - 1], parse_leaf, scope);
    };

    let default_output = &arr[arr.len() - 1];
    let mut case_filters = Vec::new();
    let mut results = Vec::new();

    let mut i = 2;
    while i + 1 < arr.len() - 1 {
        let values = extract_match_labels(&arr[i], json)?;
        let case_filter = Filter::In { key: key.clone(), values };
        case_filters.push(case_filter.clone());
        for (sub_filter, zoom_value) in compile_zoom_value(&arr[i + 1], parse_leaf, scope)? {
            results.push((Filter::All(vec![case_filter.clone(), sub_filter]), zoom_value));
        }
        i += 2;
    }

    let default_filter = Filter::Not(Box::new(Filter::AnyOf(case_filters)));
    for (sub_filter, zoom_value) in compile_zoom_value(default_output, parse_leaf, scope)? {
        results.push((Filter::All(vec![default_filter.clone(), sub_filter]), zoom_value));
    }

    Ok(results)
}

/// Extracts the property key from the modern `["get", key]` form. Unlike
/// `style_spec::filter`'s `extract_key`, this deliberately doesn't accept a
/// bare-string legacy key: no real Liberty `match` (filter- or value-
/// selecting) uses that form for its input.
pub(crate) fn extract_get_key(expr: &Value) -> Option<String> {
    let arr = expr.as_array()?;
    if arr.first()?.as_str()? == "get" {
        return arr.get(1)?.as_str().map(|s| s.to_string());
    }
    None
}

/// A match case's label(s): either a single string or an array of strings
/// (the array form is what Liberty's real `match` uses, e.g.
/// `["florist", "furniture"]`; the single-string shorthand is part of the
/// style spec and supported for completeness).
pub(crate) fn extract_match_labels(expr: &Value, json: &Value) -> Result<Vec<String>> {
    if let Some(s) = expr.as_str() {
        return Ok(vec![s.to_string()]);
    }
    let arr = expr
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("\"match\" case labels must be a string or array of strings: {json}"))?;
    arr.iter()
        .map(|v| v.as_str().map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("\"match\" case labels must be strings: {json}")))
        .collect()
}

/// The marker on an error that means "the constant folder understood this
/// expression and could not reduce it to a value" -- as opposed to "this is
/// a construct the compiler does not implement".
///
/// The distinction decides what happens to the layer. `try_or_skip!` in
/// `style_spec::compile_layer` drops the WHOLE layer on a paint-property
/// error, which is the right answer for an unrecognized construct but a
/// catastrophic one here: the Mapbox Standard document reaches for the
/// evaluator in 796 places, so a single unhandled form taken as "skip the
/// layer" would delete most of the style. A property carrying an error with
/// this marker degrades to a literal fallback and keeps its layer instead --
/// the `line-dasharray` precedent, one level up.
#[derive(Debug)]
pub struct Unfoldable(String);

impl std::fmt::Display for Unfoldable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unfoldable {}

/// Whether `error` came from the constant folder refusing an expression,
/// and so must degrade the property rather than drop the layer. Checks the
/// whole cause chain, so a caller is free to add its own `.context(...)`.
pub fn is_unfoldable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Unfoldable>())
}

/// Re-marks an error as a constant-folder refusal. Used where a fold
/// succeeded but its result was the wrong kind for the property (a string
/// where a number was wanted), and where a `let`'s own shape is at fault --
/// both are the folder giving up, not an unrecognized construct.
fn unfoldable(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(Unfoldable(format!("{error:#}")))
}

/// Folds `value` in `scope` and converts the result to the leaf type.
fn fold_leaf<T>(value: &Value, scope: Scope<'_>, convert: impl FnOnce(&EvalValue) -> Result<T>) -> Result<T> {
    let folded = eval(value, scope, scope.zoom())?;
    convert(&folded).map_err(|error| unfoldable(error.context(format!("folding {value}"))))
}

/// The intermediate value the constant folder computes in. The style spec's
/// expression language is dynamically typed and its operators cross type
/// boundaries freely -- `to-hsla` turns a colour into an array of numbers,
/// `hsl` turns numbers back into a colour -- so folding cannot be done in
/// the destination type (`Rgba`, `f32`) the way `parse_leaf` works.
///
/// Deliberately NOT named `Value`, as the design doc writes it: this module
/// already uses that name for `serde_json::Value`, which appears in every
/// signature around it.
#[derive(Clone, Debug, PartialEq)]
pub enum EvalValue {
    Number(f64),
    String(String),
    Bool(bool),
    Color(crate::style::Rgba),
    Array(Vec<EvalValue>),
    Null,
}

impl EvalValue {
    /// The name used in error messages -- what the value IS, when an
    /// operator wanted something else.
    fn type_name(&self) -> &'static str {
        match self {
            EvalValue::Number(_) => "number",
            EvalValue::String(_) => "string",
            EvalValue::Bool(_) => "boolean",
            EvalValue::Color(_) => "color",
            EvalValue::Array(_) => "array",
            EvalValue::Null => "null",
        }
    }

    pub(crate) fn as_number(&self) -> Result<f64> {
        match self {
            EvalValue::Number(n) => Ok(*n),
            other => bail!("expected a number, got a {}", other.type_name()),
        }
    }

    /// A colour, or a CSS colour string that names one -- `["to-hsla",
    /// ["config", "colorLand"]]` is handed a string, since that is what a
    /// config entry and a style literal both are.
    pub fn as_color(&self) -> Result<crate::style::Rgba> {
        match self {
            EvalValue::Color(c) => Ok(*c),
            EvalValue::String(s) => crate::style_spec::color::parse_css_color(s),
            other => bail!("expected a color, got a {}", other.type_name()),
        }
    }

    /// The style spec's own truthiness: `false`, `0`, `""`, `null` and NaN
    /// are false and everything else is true.
    fn truthy(&self) -> bool {
        match self {
            EvalValue::Bool(b) => *b,
            EvalValue::Number(n) => *n != 0.0 && !n.is_nan(),
            EvalValue::String(s) => !s.is_empty(),
            EvalValue::Null => false,
            EvalValue::Color(_) | EvalValue::Array(_) => true,
        }
    }

    fn to_display_string(&self) -> Result<String> {
        Ok(match self {
            EvalValue::Null => String::new(),
            EvalValue::Bool(b) => b.to_string(),
            EvalValue::Number(n) => n.to_string(),
            EvalValue::String(s) => s.clone(),
            EvalValue::Color(c) => format!("rgba({},{},{},{})", c.r * 255.0, c.g * 255.0, c.b * 255.0, c.a),
            EvalValue::Array(_) => bail!("\"to-string\" has no rendering for an array"),
        })
    }

    /// A JSON literal, as `["literal", ...]` hands it over: converted
    /// wholesale, with nothing inside it evaluated.
    fn from_json(json: &Value) -> Result<Self> {
        Ok(match json {
            Value::Null => EvalValue::Null,
            Value::Bool(b) => EvalValue::Bool(*b),
            Value::Number(n) => EvalValue::Number(
                n.as_f64().ok_or_else(|| anyhow::anyhow!("number {n} is not representable as an f64"))?,
            ),
            Value::String(s) => EvalValue::String(s.clone()),
            Value::Array(items) => EvalValue::Array(items.iter().map(EvalValue::from_json).collect::<Result<_>>()?),
            Value::Object(_) => bail!("a JSON object is not a style-spec value: {json}"),
        })
    }
}

/// Whether `expr`'s subtree reads the zoom -- i.e. whether folding it at one
/// zoom would give a different answer than folding it at another. Drives
/// `compile_folded`'s choice between one constant stop and one stop per
/// integer zoom.
pub(crate) fn mentions_zoom(expr: &Value) -> bool {
    match expr {
        Value::Array(items) => {
            items.first().and_then(Value::as_str) == Some("zoom") || items.iter().any(mentions_zoom)
        }
        _ => false,
    }
}

/// The operators that read the feature being painted. None of them can be
/// folded at style-compile time -- that is the whole point of the design's
/// "reject feature-dependent bindings" decision, since a `let` bound to one
/// of these and referenced *k* times would re-expand the paint compiler's
/// per-feature `match` cross-product at every use site, exponentially in
/// *k*.
///
/// `"feature-state"` is deliberately NOT one of them, even though the style
/// spec lists it alongside the feature lookups. It does not read the
/// feature: it reads state an application attaches to a feature id at
/// runtime, via `map.setFeatureState`. This crate exposes no such API, so
/// no key is ever set and the value is not merely unknown at compile time
/// -- it is permanently the unset one, which is a compile-time constant.
/// See `eval_inner`'s `"feature-state"` arm for what that constant is and
/// where the reference implementation says so.
const FEATURE_OPERATORS: &[&str] = &["get", "has", "geometry-type", "id", "properties"];

/// The first feature-reading operator anywhere in `expr`'s subtree, if any.
/// Used both to reject a feature-dependent `let` binding by name and to
/// explain the refusal.
fn feature_operator_in(expr: &Value) -> Option<&str> {
    match expr {
        Value::Array(items) => {
            if let Some(head) = items.first().and_then(Value::as_str) {
                if let Some(op) = FEATURE_OPERATORS.iter().find(|op| **op == head) {
                    return Some(op);
                }
            }
            items.iter().find_map(feature_operator_in)
        }
        _ => None,
    }
}

/// Folds a style-spec expression to a single `EvalValue`, at style-compile
/// time only. `scope` supplies the enclosing `let` bindings and `zoom` the
/// integer zoom the fold is being done at (`["zoom"]` reads it).
///
/// Every error this returns carries the `Unfoldable` marker, so a caller can
/// tell "the folder gave up on this" apart from "this is not an expression
/// the compiler knows", and degrade the property rather than drop the layer.
///
/// `["config", k]` resolves against `scope.config()` -- attach one with
/// `Scope::with_config` -- and degrades the same way when `k` isn't
/// resolvable, or when the scope carries no config at all.
///
/// `["measure-light", "brightness"]` resolves against `scope.lights()` --
/// attach one with `Scope::with_lights` -- by evaluating the `ambient`
/// light's `color` and `intensity` (ordinary expressions in this same
/// vocabulary, so they fold through `scope.config()` exactly as any other
/// `["config", ...]`-bearing expression does) and reducing them to one
/// scalar. See `measure_light_brightness`.
/// Evaluates `expr` for ONE feature at the bake zoom -- the same evaluator
/// the constant folder is, with `get`/`has`/`coalesce` reading `tags`
/// (`Scope::with_feature`). `None` is MapLibre's runtime error (a missing
/// property fed to arithmetic, say), which the caller turns into the
/// property's default.
pub fn eval_number_for_feature(
    expr: &Value,
    tags: &dyn crate::mvt::TagLookup,
    zoom: u8,
) -> Option<f64> {
    eval_inner(expr, Scope::root().with_feature(tags), zoom)
        .ok()?
        .as_number()
        .ok()
        .filter(|n| n.is_finite())
}

/// Every operator `eval_inner` evaluates for a feature at bake time. An
/// expression built only from these (and literals) can be kept whole and
/// evaluated per feature; anything else is refused at compile time instead
/// of failing silently on every feature.
const PER_FEATURE_OPERATORS: &[&str] = &[
    "interpolate", "step", "match", "case", "zoom", "get", "has", "!has", "coalesce", "to-number", "literal", "+",
    "-", "*", "/", "%", "sqrt", "abs", "min", "max", "==", "!=", "<", "<=", ">", ">=", "all", "any", "!", "linear",
    "exponential", "cubic-bezier", "number", "to-boolean", "feature-state",
];

/// Whether `expr` is made only of `PER_FEATURE_OPERATORS`. Match labels
/// and interpolation-curve arguments are literal data, not expressions, and
/// are skipped.
pub(crate) fn is_per_feature_evaluable(expr: &Value) -> bool {
    let Some(arr) = expr.as_array() else { return !expr.is_object() };
    let Some(head) = arr.first().and_then(Value::as_str) else { return false };
    if !PER_FEATURE_OPERATORS.contains(&head) {
        return false;
    }
    match head {
        "literal" | "linear" | "exponential" | "cubic-bezier" | "get" | "has" | "!has" | "feature-state" => true,
        // ["match", input, labels, out, ..., default]: skip the labels.
        "match" => arr.iter().enumerate().skip(1).all(|(i, v)| (i >= 2 && i % 2 == 0 && i != arr.len() - 1) || is_per_feature_evaluable(v)),
        "interpolate" => arr.iter().skip(2).all(is_per_feature_evaluable),
        _ => arr.iter().skip(1).all(is_per_feature_evaluable),
    }
}

pub fn eval(expr: &Value, scope: Scope<'_>, zoom: u8) -> Result<EvalValue> {
    eval_inner(expr, scope, zoom).map_err(|error| anyhow::Error::new(Unfoldable(format!("{error:#}"))))
}

/// Folds `expr` to a single `true`/`false` at style-compile time, using the
/// style spec's own truthiness. Errors -- carrying `Unfoldable`, like every
/// other error out of `eval` -- when any part of `expr` is not a compile-time
/// constant, which for a filter clause means it reads the feature.
///
/// Exists for `filter::compile_filter`: in the reference a filter is not a
/// language of its own but an ordinary expression evaluated to a boolean
/// (`createFilter` in `src/style-spec/feature_filter/index.ts`, Mapbox GL JS
/// 3.15.0, hands the filter array straight to `createExpression`), so a
/// filter clause made entirely of constants has one answer for the whole
/// compile and this is how to get it.
pub fn eval_bool(expr: &Value, scope: Scope<'_>) -> Result<bool> {
    Ok(eval(expr, scope, scope.zoom())?.truthy())
}

/// Folds `expr` to the JSON scalar a style document could have written in
/// its place -- a string, a number or a boolean.
///
/// The point is that the result is raw JSON again, so a caller that already
/// knows how to compile a literal operand needs no second code path for a
/// folded one. Non-scalar results (a colour, an array) are refused rather
/// than rendered: nothing that compares against a feature tag can use them.
pub fn fold_to_json_scalar(expr: &Value, scope: Scope<'_>) -> Result<Value> {
    let folded = eval(expr, scope, scope.zoom())?;
    match folded {
        EvalValue::String(s) => Ok(Value::String(s)),
        EvalValue::Bool(b) => Ok(Value::Bool(b)),
        EvalValue::Number(n) => serde_json::Number::from_f64(n)
            .map(Value::Number)
            .ok_or_else(|| unfoldable(anyhow::anyhow!("{n} is not representable as JSON: {expr}"))),
        other => Err(unfoldable(anyhow::anyhow!("expected a string, number or boolean, got a {}: {expr}", other.type_name()))),
    }
}

fn eval_inner(expr: &Value, scope: Scope<'_>, zoom: u8) -> Result<EvalValue> {
    let Some(arr) = expr.as_array() else {
        // A bare JSON literal is its own value.
        return EvalValue::from_json(expr);
    };

    let head = arr
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("an expression's first element must be an operator name: {expr}"))?;
    let args = &arr[1..];
    let arity = |n: usize| -> Result<()> {
        if args.len() == n {
            Ok(())
        } else {
            bail!("{head:?} takes {n} argument(s), got {}: {expr}", args.len())
        }
    };
    let number_at = |i: usize| -> Result<f64> {
        eval_inner(&args[i], scope, zoom)?.as_number().with_context(|| format!("in {head:?}: {expr}"))
    };

    match head {
        "literal" => {
            arity(1)?;
            EvalValue::from_json(&args[0])
        }

        "zoom" => {
            arity(0)?;
            Ok(EvalValue::Number(zoom as f64))
        }

        // MapLibre's `["line-progress"]`: the fraction along the line, which
        // only a `line-gradient` evaluation supplies (`Scope::
        // with_line_progress`). Anywhere else it is refused -- MapLibre
        // refuses it at parse time outside `line-gradient`; here it is an
        // evaluation error, which the compile of any other paint property
        // surfaces the same way (F37, story-content 1a).
        "line-progress" => {
            arity(0)?;
            scope
                .line_progress
                .map(EvalValue::Number)
                .ok_or_else(|| anyhow::anyhow!("\"line-progress\" is only available in line-gradient: {expr}"))
        }

        "let" => eval_let(arr, expr, scope, zoom),
        "var" => {
            arity(1)?;
            let name = args[0]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("\"var\" takes a binding name as a string: {expr}"))?;
            // Evaluated in the scope the binding was DECLARED in, not the
            // one the `var` appears in: a `let` that rebinds a name in terms
            // of its own earlier binding is legal and common, and resolving
            // in the use site's scope would make it refer to itself.
            let (bound, declaring) =
                scope.lookup(name).ok_or_else(|| anyhow::anyhow!("\"var\" refers to unbound name {name:?}: {expr}"))?;
            eval_inner(bound, declaring, zoom).with_context(|| format!("evaluating the binding for {name:?}"))
        }

        "at" => {
            arity(2)?;
            let index = number_at(0)?;
            let EvalValue::Array(items) = eval_inner(&args[1], scope, zoom)? else {
                bail!("\"at\" indexes an array: {expr}");
            };
            if index < 0.0 || index.fract() != 0.0 || index as usize >= items.len() {
                bail!("\"at\" index {index} is out of range for an array of {} element(s): {expr}", items.len());
            }
            Ok(items[index as usize].clone())
        }

        // `to-hsla` reports hue in DEGREES (0-360), saturation and lightness
        // as PERCENTAGES (0-100) and alpha as a unit fraction (0-1). Not a
        // guess: the vendored Standard document writes
        // `["min", 100, ["+", ["at", 2, ["to-hsla", C]], 2]]` and
        // `["max", 0, ["-", ["at", 1, ["to-hsla", C]], 25]]` for lightness
        // and saturation, `["max", 0, ["-", ["at", 0, ...], 20]]` for hue,
        // and `["-", ["at", 3, ...], 0.2]` for alpha.
        "to-hsla" => {
            arity(1)?;
            let color = eval_inner(&args[0], scope, zoom)?.as_color().with_context(|| format!("in \"to-hsla\": {expr}"))?;
            let (h, s, l) = crate::style_spec::color::rgb_to_hsl(color.r, color.g, color.b);
            Ok(EvalValue::Array(vec![
                EvalValue::Number(h as f64),
                EvalValue::Number((s * 100.0) as f64),
                EvalValue::Number((l * 100.0) as f64),
                EvalValue::Number(color.a as f64),
            ]))
        }

        // The inverse pairing, in the same units. The division by 100 is
        // done in `f32` rather than `f64` so that `["hsl", 20, 20, 95]` is
        // bit-identical to `parse_css_color("hsl(20, 20%, 95%)")`, which
        // divides `f32`s -- a double rounding through `f64` can differ in
        // the last bit.
        "hsl" | "hsla" => {
            let wants = if head == "hsl" { 3 } else { 4 };
            arity(wants)?;
            let h = number_at(0)? as f32;
            let s = number_at(1)? as f32 / 100.0;
            let l = number_at(2)? as f32 / 100.0;
            let a = if wants == 4 { number_at(3)? as f32 } else { 1.0 };
            let (r, g, b) = crate::style_spec::color::hsl_to_rgb(h, s, l);
            Ok(EvalValue::Color(crate::style::Rgba { r, g, b, a }))
        }

        "+" | "*" => {
            if args.is_empty() {
                bail!("{head:?} needs at least one argument: {expr}");
            }
            let identity = if head == "+" { 0.0 } else { 1.0 };
            let mut acc = identity;
            for i in 0..args.len() {
                let n = number_at(i)?;
                acc = if head == "+" { acc + n } else { acc * n };
            }
            Ok(EvalValue::Number(acc))
        }
        "-" => match args.len() {
            1 => Ok(EvalValue::Number(-number_at(0)?)),
            2 => Ok(EvalValue::Number(number_at(0)? - number_at(1)?)),
            n => bail!("\"-\" takes 1 (negation) or 2 arguments, got {n}: {expr}"),
        },
        "/" => {
            arity(2)?;
            Ok(EvalValue::Number(number_at(0)? / number_at(1)?))
        }
        "%" => {
            arity(2)?;
            Ok(EvalValue::Number(number_at(0)? % number_at(1)?))
        }
        "abs" => {
            arity(1)?;
            Ok(EvalValue::Number(number_at(0)?.abs()))
        }
        "sqrt" => {
            arity(1)?;
            Ok(EvalValue::Number(number_at(0)?.sqrt()))
        }

        // The feature reads. Only a BAKE-time scope carries a feature
        // (`Scope::with_feature`); at compile time these are the same error
        // an unknown operator always was, so no expression that failed to
        // fold before folds now. MapLibre semantics (`get`/`has` in
        // `definitions/index.ts`): a missing property is `null`.
        "get" | "has" | "!has" if scope.feature.is_some() => {
            arity(1)?;
            let key = args[0]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{head:?} takes a property name as a string: {expr}"))?;
            let tags = scope.feature.expect("guarded above");
            let value = tags.get(key);
            Ok(match head {
                "has" => EvalValue::Bool(value.is_some()),
                "!has" => EvalValue::Bool(value.is_none()),
                _ => match value {
                    None => EvalValue::Null,
                    Some(crate::mvt::TagValue::Number(n)) => EvalValue::Number(*n),
                    Some(crate::mvt::TagValue::String(s)) => EvalValue::String(s.clone()),
                    Some(crate::mvt::TagValue::Bool(b)) => EvalValue::Bool(*b),
                },
            })
        }
        // MapLibre `Coalesce.evaluate`: the first argument that evaluates
        // to non-null. An argument that ERRORS is not skipped there either
        // -- the error propagates -- so neither is it here.
        "coalesce" if scope.feature.is_some() => {
            for arg in args {
                let value = eval_inner(arg, scope, zoom)?;
                if !matches!(value, EvalValue::Null) {
                    return Ok(value);
                }
            }
            Ok(EvalValue::Null)
        }
        "min" | "max" => {
            if args.is_empty() {
                bail!("{head:?} needs at least one argument: {expr}");
            }
            let mut acc = number_at(0)?;
            for i in 1..args.len() {
                let n = number_at(i)?;
                acc = if head == "min" { acc.min(n) } else { acc.max(n) };
            }
            Ok(EvalValue::Number(acc))
        }

        // The coercions are multi-argument in the style spec: each argument
        // is tried in turn and the first that converts wins.
        "to-number" => {
            if args.is_empty() {
                bail!("\"to-number\" needs at least one argument: {expr}");
            }
            let mut last: Option<anyhow::Error> = None;
            for arg in args {
                match eval_inner(arg, scope, zoom).and_then(|v| coerce_to_number(&v)) {
                    Ok(n) => return Ok(EvalValue::Number(n)),
                    Err(error) => last = Some(error),
                }
            }
            Err(last.expect("args is non-empty, so at least one attempt was made"))
                .with_context(|| format!("no argument of \"to-number\" converted: {expr}"))
        }
        // The `number` TYPE ASSERTION (MapLibre `Assertion`, `definitions/
        // assertion.ts`): each argument is evaluated in turn and the first
        // that IS a number is returned; if none is, it is a runtime error.
        // Unlike `to-number` there is no coercion: a string "12" fails.
        "number" => {
            if args.is_empty() {
                bail!("\"number\" needs at least one argument: {expr}");
            }
            for arg in args {
                if let EvalValue::Number(n) = eval_inner(arg, scope, zoom)? {
                    return Ok(EvalValue::Number(n));
                }
            }
            bail!("no argument of \"number\" is a number: {expr}")
        }
        "to-boolean" => {
            arity(1)?;
            Ok(EvalValue::Bool(eval_inner(&args[0], scope, zoom)?.truthy()))
        }
        "to-string" => {
            arity(1)?;
            Ok(EvalValue::String(eval_inner(&args[0], scope, zoom)?.to_display_string()?))
        }
        // Variadic string join, with each operand converted the way
        // `to-string` converts it -- the spec's own definition. Needed
        // because Mapbox Standard builds a `text-font` entry out of one:
        // `["concat", ["config", "font"], " Medium"]`, on 17 of the vendored
        // dark style's symbol layers. Without it the primary face of every
        // one of them is unfoldable and the stack falls back to its
        // Arial-coverage tail.
        "concat" => {
            let mut joined = String::new();
            for arg in args {
                joined.push_str(&eval_inner(arg, scope, zoom)?.to_display_string()?);
            }
            Ok(EvalValue::String(joined))
        }

        "==" | "!=" | "<" | "<=" | ">" | ">=" => {
            if args.len() != 2 {
                bail!("{head:?} takes 2 arguments, got {}: {expr}", args.len());
            }
            let a = eval_inner(&args[0], scope, zoom)?;
            let b = eval_inner(&args[1], scope, zoom)?;
            Ok(EvalValue::Bool(compare(head, &a, &b, expr)?))
        }
        "all" | "any" => {
            let mut acc = head == "all";
            for arg in args {
                let value = eval_inner(arg, scope, zoom)?.truthy();
                acc = if head == "all" { acc && value } else { acc || value };
            }
            Ok(EvalValue::Bool(acc))
        }
        "!" => {
            arity(1)?;
            Ok(EvalValue::Bool(!eval_inner(&args[0], scope, zoom)?.truthy()))
        }

        // `match`/`case`/`step`/`interpolate` also exist as COMPILERS in
        // this module, producing `(Filter, ZoomValue<T>)` pairs so that a
        // feature-varying or zoom-varying property keeps varying at runtime.
        // These arms are the additive constant-folding path, reached only
        // when one of them is nested inside an expression that has to
        // collapse to a single value (a `let` binding, an `hsl` component).
        // The compilers keep their existing behaviour untouched.
        "match" => {
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!("\"match\" must have the form [\"match\", input, labels, output, ..., default]: {expr}");
            }
            let input = eval_inner(&arr[1], scope, zoom)?;
            let mut i = 2;
            while i + 1 < arr.len() - 1 {
                if match_labels(&arr[i]).iter().any(|label| label_matches(label, &input)) {
                    return eval_inner(&arr[i + 1], scope, zoom);
                }
                i += 2;
            }
            eval_inner(&arr[arr.len() - 1], scope, zoom)
        }
        "case" => {
            if arr.len() < 4 || arr.len() % 2 != 0 {
                bail!("\"case\" must have the form [\"case\", cond, out, ..., fallback]: {expr}");
            }
            let mut i = 1;
            while i + 1 < arr.len() {
                if eval_inner(&arr[i], scope, zoom)?.truthy() {
                    return eval_inner(&arr[i + 1], scope, zoom);
                }
                i += 2;
            }
            eval_inner(&arr[arr.len() - 1], scope, zoom)
        }
        "step" => {
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!("\"step\" must have the form [\"step\", input, base, t1, v1, ...]: {expr}");
            }
            let input = eval_inner(&arr[1], scope, zoom)?.as_number()?;
            let mut chosen = &arr[2];
            let mut i = 3;
            while i + 1 < arr.len() {
                let threshold = eval_inner(&arr[i], scope, zoom)?.as_number()?;
                if input >= threshold {
                    chosen = &arr[i + 1];
                }
                i += 2;
            }
            eval_inner(chosen, scope, zoom)
        }
        "interpolate" => {
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!("\"interpolate\" must have the form [\"interpolate\", type, input, i1, v1, ...]: {expr}");
            }
            let curve = Interpolation::parse(&arr[1], expr)?;
            let input = eval_inner(&arr[2], scope, zoom)?.as_number()?;
            let mut stops: Vec<(f64, EvalValue)> = Vec::new();
            let mut i = 3;
            while i + 1 < arr.len() {
                let at = arr[i]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("\"interpolate\" stop input must be a literal number: {expr}"))?;
                stops.push((at, eval_inner(&arr[i + 1], scope, zoom)?));
                i += 2;
            }
            interpolate_folded(&stops, input, curve, expr)
        }

        _ if FEATURE_OPERATORS.contains(&head) => bail!(
            "{head:?} reads the feature being painted, which has no value at style-compile time: {expr}"
        ),
        // `["feature-state", key]` folds to NULL, which is not a guess and
        // not a degradation: it is what the reference implementation
        // evaluates this to for a map on which no state has been set, and
        // this crate has no `setFeatureState` for anything to set it with.
        //
        // Mapbox GL JS 3.15.0, `src/style-spec/expression/definitions/
        // index.ts`:
        //
        //     'feature-state': [ValueType, [StringType],
        //         (ctx, [key]) => get(key.evaluate(ctx), ctx.featureState || {})]
        //
        //     function get<T extends object>(key: keyof T, obj: T) {
        //         const v = obj[key];
        //         return typeof v === 'undefined' ? null : v;
        //     }
        //
        // `EvaluationContext.featureState` starts `null` (its constructor,
        // `evaluation_context.ts`) and is only ever assigned the per-id
        // entry of the `featureStates` map that `setFeatureState` writes
        // (`ProgramConfigurationSet.updatePaintArrays`, `program_
        // configuration.ts`: `const state = featureStates[id.toString()]`,
        // `undefined` when nothing was set). Either way the `|| {}` makes
        // the lookup miss and `get` returns `null` -- so an unset key is
        // `null`, per the spec, not `false` and not an error.
        //
        // That matters because of what the styles do with it. Fifteen
        // layers of the vendored Mapbox Standard document read a
        // feature-state (`select`, `highlight` or `hide`), and every one
        // of them does it inside a `to-boolean` in a `case` -- the
        // buildings' shape being `["case", ["to-boolean", ["feature-state",
        // "select"]], <selected colour>, ["to-boolean", ["feature-state",
        // "highlight"]], <highlight colour>, <ordinary colour>]`.
        // `["to-boolean", null]` is `Boolean(null)`, i.e. `false`
        // (`definitions/coercion.ts`), so the fold takes the else branch --
        // the ordinary colour the author wrote for exactly this case.
        // Refusing to fold instead threw that branch away along with the
        // whole layer.
        //
        // If this crate ever grows a feature-state API, this arm is the one
        // thing that has to go: the value stops being constant, and the
        // expression becomes genuinely per-feature.
        "feature-state" => {
            arity(1)?;
            if !args[0].is_string() {
                bail!("\"feature-state\" takes a key name as a string: {expr}");
            }
            Ok(EvalValue::Null)
        }
        // Resolved from `scope.config()`, below, which is `None` for a
        // compile with nothing to resolve against (every style before
        // Mapbox Standard) and `Some` once an `Import`'s `ResolvedConfig`
        // has been attached via `Scope::with_config`. Either "no config at
        // all" or "config present but this key isn't in it" is an ordinary
        // `eval_inner` error, which `eval` marks `Unfoldable` like every
        // other error here -- so an unresolvable key degrades the property
        // instead of dropping the layer, exactly as the doc comment on
        // `eval` promises.
        "config" => {
            arity(1)?;
            let key = args[0]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("\"config\" takes a key name as a string: {expr}"))?;
            let config = scope
                .config()
                .ok_or_else(|| anyhow::anyhow!("no config resolved for this compile: {expr}"))?;
            let value = config
                .resolve(key)
                .ok_or_else(|| anyhow::anyhow!("{key:?} is not a resolvable config key: {expr}"))?;
            EvalValue::from_json(value)
        }
        "measure-light" => {
            arity(1)?;
            let component = args[0]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("\"measure-light\" takes a component name as a string: {expr}"))?;
            measure_light_brightness(component, scope, zoom, expr)
        }
        "image" => {
            bail!("{head:?} is not resolved by this compiler yet: {expr}")
        }
        _ => bail!("unsupported operator {head:?}: {expr}"),
    }
}

/// Evaluates `["measure-light", component]` -- `Style.calculateLightsBrightness`,
/// `src/style/style.ts:2225-2256` in the extracted Mapbox GL JS 3.15.0
/// source, transcribed verbatim (this replaces an earlier approximation,
/// `luminance(ambient.color) * ambient.intensity`, written before that
/// source was available; it was wrong by 1.7-1.9x against the real
/// fixture). `component` must be `"brightness"`, the only component any
/// real Standard expression asks for.
///
/// Reduces the `lights` array's `ambient` AND `directional` entries --
/// `color`/`intensity` on both, plus `direction` on `directional` -- each
/// an ordinary expression in this vocabulary folded through `eval_inner`
/// exactly like `ground_radiance_inner`'s inputs, and can themselves
/// reference `["config", ...]`. The reference returns early (`if
/// (!directional || !ambient) return;`) when either light is missing;
/// `find_light` erroring on whichever is absent gives the same refusal,
/// consistent with `ground_radiance`'s identical both-lights gate (see
/// `ground_radiance_with_no_directional_entry_does_not_fold` and this
/// function's own `measure_light_with_no_directional_entry_does_not_fold`).
///
/// The formula, per the source:
/// ```text
/// relativeLuminance(c) = 0.2126*f(r) + 0.7152*f(g) + 0.0722*f(b)
///     where f(x) = x <= 0.03928 ? x/12.92 : ((x+0.055)/1.055)^2.4
///                                          -- W3C relative luminance, NOT
///                                             a plain dot product.
/// polarIntensity = 1 - cartesianPositionToSpherical(direction).polar / 90
/// directionalBrightness = relativeLuminance(directional.color)
///                        * directional.intensity * polarIntensity
/// ambientBrightness = relativeLuminance(ambient.color) * ambient.intensity
/// brightness = (directionalBrightness + ambientBrightness) / 2
/// ```
/// rounded with `Number(brightness.toFixed(6))` -- kept verbatim, NOT
/// dropped: the source's own comment says this is deliberate ("prevents
/// bucket re-evaluation which was caused by small precision differences"),
/// and something downstream may compare two folded brightness values for
/// exact equality.
///
/// Scoping note: this re-folds `directional`/`ambient`'s own expressions
/// against whatever `lights` is on `scope` (the declaring document's, via
/// `Scope::with_lights` -- see `compile_single_layer`'s doc comment in
/// `style_spec/mod.rs`), once per call site. The reference instead
/// computes `Style._brightness` ONCE, scene-globally, and broadcasts it to
/// every worker; an expression reads it back with no re-fold at all
/// (`ctx.globals.brightness`, `measureLight`, `src/style-spec/expression/
/// evaluation_context.ts:65`). `ground_radiance` was deliberately changed
/// to that scene-global shape in the lighting-model series' Task 3 (see
/// `style_spec/mod.rs`'s `scene_radiance_table`/`merge_scene_lights`), but
/// `measure-light` was deliberately left re-folding per document -- making
/// it scene-global too would mean resolving `["config", ...]` inside the
/// light expressions against a document chosen independently of which
/// document's layer is asking, which is a real behaviour change to
/// `mod.rs`'s import-composition loop, not a small one, and every style
/// this compiler has ever seen (both vendored fixtures included) declares
/// `lights` in at most one fragment -- for which the two shapes are
/// observationally identical. `style_spec/mod.rs`'s own doc comment on
/// this same seam (search "deliberately not merged into one") makes the
/// same call for the same reason.
///
/// Two consequences of that choice, stated so nobody has to rediscover
/// them, and both pinned by
/// `a_root_layers_measure_light_degrades_loudly_rather_than_resolving`:
///
/// - a `["measure-light", ...]` in a ROOT-document layer never folds, in
///   any style. Root layers are compiled with the scene's radiance but no
///   `lights` (attaching the declaring import's `lights` would drag its
///   `config` along, changing how the root's own `["config", ...]`
///   expressions resolve).
/// - a style that declares its `lights` at the ROOT and its basemap in an
///   import lights every colour correctly -- `merge_scene_lights` walks the
///   root too -- while `measure-light` inside the imported layers does not
///   fold, because that document has no `lights` of its own.
///
/// Neither is silent: the fold fails, `or_degrade` records a degradation
/// naming the layer and the property, prints it, and keeps the layer with a
/// literal fallback. A wrong brightness would be the dangerous outcome;
/// this is a reported one. Closing the gap properly means pre-folding a
/// scene-global BRIGHTNESS table the way `scene_radiance_table` pre-folds
/// radiance, and reading it here instead of re-folding -- the machinery is
/// proven, the change is to this evaluator and its sixteen `with_lights`
/// test sites, and no style that exists needs it yet.
fn measure_light_brightness(component: &str, scope: Scope<'_>, zoom: u8, expr: &Value) -> Result<EvalValue> {
    if component != "brightness" {
        bail!("unsupported \"measure-light\" component {component:?} (only \"brightness\" is supported): {expr}");
    }
    let lights = scope.lights().ok_or_else(|| anyhow::anyhow!("no \"lights\" resolved for this compile: {expr}"))?;
    let ambient = find_light(lights, "ambient")?;
    let directional = find_light(lights, "directional")?;

    let ambient_color = eval_inner(light_property(ambient, "color", "ambient")?, scope, zoom)
        .and_then(|v| v.as_color())
        .with_context(|| format!("evaluating the ambient light's color: {expr}"))?;
    let ambient_intensity = eval_inner(light_property(ambient, "intensity", "ambient")?, scope, zoom)
        .and_then(|v| v.as_number())
        .with_context(|| format!("evaluating the ambient light's intensity: {expr}"))?;
    let directional_color = eval_inner(light_property(directional, "color", "directional")?, scope, zoom)
        .and_then(|v| v.as_color())
        .with_context(|| format!("evaluating the directional light's color: {expr}"))?;
    let directional_intensity = eval_inner(light_property(directional, "intensity", "directional")?, scope, zoom)
        .and_then(|v| v.as_number())
        .with_context(|| format!("evaluating the directional light's intensity: {expr}"))?;
    let direction = eval_inner(light_property(directional, "direction", "directional")?, scope, zoom)
        .with_context(|| format!("evaluating the directional light's direction: {expr}"))?;
    let [azimuthal, polar] = direction_pair(&direction)?;

    // `direction.{x,y,z}` in the reference is already the Cartesian vector
    // the style's spherical `[azimuthal, polar]` pair converts to
    // (`spherical_direction_to_cartesian`, shared with
    // `ground_radiance_inner`). `cartesianPositionToSpherical`, the
    // reference's own round trip back to spherical, recovers exactly this
    // `polar` for a unit vector -- transcribed as the round trip anyway,
    // rather than shortcut to `polar` directly, to keep this a literal
    // port rather than a simplification a future non-unit direction would
    // get wrong.
    let dir = spherical_direction_to_cartesian(azimuthal, polar);
    let spherical_roundtrip = cartesian_position_to_spherical(dir[0], dir[1], dir[2]);
    let polar_intensity = 1.0 - spherical_roundtrip[2] / 90.0;

    let directional_brightness = relative_luminance(directional_color) * directional_intensity * polar_intensity;
    let ambient_brightness = relative_luminance(ambient_color) * ambient_intensity;
    let brightness = (directional_brightness + ambient_brightness) / 2.0;

    // `Number(brightness.toFixed(6))` in the reference: NOT optional, see
    // this function's doc comment.
    let rounded: f64 =
        format!("{brightness:.6}").parse().expect("a value formatted with fixed precision always reparses");
    Ok(EvalValue::Number(rounded))
}

/// `f(x)` inside `relativeLuminance`, `src/style/style.ts:2225-2256`:
/// https://www.w3.org/WAI/GL/wiki/Relative_luminance, per channel.
fn relative_luminance_channel(c: f32) -> f64 {
    let c = c as f64;
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// `relativeLuminance`, `src/style/style.ts:2225-2256` -- the W3C
/// relative-luminance formula `calculateLightsBrightness` weighs each
/// light's colour by, NOT the plain Rec. 709 dot product
/// `measure_light_brightness` used before this formula was transcribed
/// (the two coincide only where every channel is exactly 0 or 1).
fn relative_luminance(color: crate::style::Rgba) -> f64 {
    0.2126 * relative_luminance_channel(color.r)
        + 0.7152 * relative_luminance_channel(color.g)
        + 0.0722 * relative_luminance_channel(color.b)
}

/// `cartesianPositionToSpherical`, `src/util/util.ts:599`, transcribed
/// verbatim -- returns `[radial, azimuthal, polar]`, all in degrees except
/// `radial`. `calculateLightsBrightness` reads only `[2]` (`polar`); kept
/// as the full triple here anyway, matching the source's own return
/// shape, rather than trimmed to the one field this caller needs.
fn cartesian_position_to_spherical(x: f64, y: f64, z: f64) -> [f64; 3] {
    let radial = (x * x + y * y + z * z).sqrt();
    let polar = if radial > 0.0 { (z / radial).acos().to_degrees() } else { 0.0 };
    let mut azimuthal = if x != 0.0 || y != 0.0 { (-y).atan2(-x).to_degrees() + 90.0 } else { 0.0 };
    if azimuthal < 0.0 {
        azimuthal += 360.0;
    }
    [radial, azimuthal, polar]
}

/// Which of the style spec's two light KINDS a `lights` array entry is:
/// `"ambient"` or `"directional"`, read from its `type`.
///
/// `type` and not `id`, because that is what the reference discriminates
/// on -- `Style.mergeAll` branches on `light.type === 'ambient'` /
/// `'directional'` (`src/style/style.ts:924-931`, Mapbox GL JS 3.15.0) and
/// `validate_lights.ts` requires both fields, with `id` a free-form string
/// the author picks. A style declaring `{"id": "sun", "type":
/// "directional"}` is perfectly legal, and reading it back by `id` found
/// nothing -- which silently compiled the whole style UNLIT and degraded
/// every `measure-light` in it.
///
/// Falls back to `id` for an entry carrying no `type` at all. That is not
/// in the reference (where `type` is required); it is here because this
/// crate's own synthetic `lights` arrays -- in tests, and in any hand-built
/// style -- are commonly written with just an `id`, and silently refusing
/// to light those would be the same class of failure in the other
/// direction.
///
/// One definition, shared by both readers (`measure_light_brightness`,
/// `ground_radiance_inner`, through `find_light`) and by the writer
/// (`style_spec::merge_scene_lights`, which picks the scene's winning
/// entries). They used to discriminate differently, which is exactly how
/// the mismatch above got in.
pub(crate) fn light_kind(light: &Value) -> Option<&str> {
    light
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| light.get("id").and_then(Value::as_str))
}

/// Finds a `lights` array entry by kind (`"ambient"` or `"directional"`) --
/// see `light_kind` for why kind and not `id`.
///
/// Shared by `measure_light_brightness` and `ground_radiance_inner`, which
/// both start the same way: `scope.lights()` gives the raw merged `lights`
/// array, and each needs one particular entry out of it.
fn find_light<'a>(lights: &'a Value, kind: &str) -> Result<&'a Value> {
    lights
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("\"lights\" must be an array"))?
        .iter()
        .find(|light| light_kind(light) == Some(kind))
        .ok_or_else(|| anyhow::anyhow!("no {kind:?} entry in \"lights\""))
}

/// The unevaluated expression at `light.properties.<property>` -- still raw
/// JSON, since `color`/`intensity`/`direction` are ordinary expressions in
/// this vocabulary that the caller folds with `eval_inner` at whatever zoom
/// it needs. Shared by `measure_light_brightness` and
/// `ground_radiance_inner`, same reason as `find_light`.
fn light_property<'a>(light: &'a Value, property: &str, id: &str) -> Result<&'a Value> {
    light
        .get("properties")
        .ok_or_else(|| anyhow::anyhow!("the {id:?} light has no \"properties\""))?
        .get(property)
        .ok_or_else(|| anyhow::anyhow!("the {id:?} light has no \"properties.{property}\""))
}

/// `sphericalDirectionToCartesian([azimuthal, polar])`, from Mapbox GL JS
/// 3.15.0's `src/util/util.ts` -- actually `sphericalPositionToCartesian`
/// called with radius 1, which is all `sphericalDirectionToCartesian` does.
/// Transcribed verbatim, INCLUDING the +90 degree compass correction (the
/// source's own comment: "We abstract 'north'/'up' (compass-wise) to be 0
/// degrees when really this is 90 degrees (pi/2): correct for that here").
/// Getting that wrong changes `dir.z`, which both `calculate_ground_radiance`
/// and `calculate_ambient_directional_factor` read directly (as `NdotL` for
/// the ground normal), so a mistake here silently shifts every radiance.
fn spherical_direction_to_cartesian(azimuthal_deg: f64, polar_deg: f64) -> [f64; 3] {
    let a = (azimuthal_deg + 90.0).to_radians();
    let p = polar_deg.to_radians();
    [a.cos() * p.sin(), a.sin() * p.sin(), p.cos()]
}

/// `calculateAmbientDirectionalFactor` from `3d-style/render/lights.ts`,
/// transcribed in its general form -- over an arbitrary `normal`, not
/// hardcoded to the ground case -- because that is what the source does
/// and a future 3D use (`fill-extrusion`, or true 3D geometry) would need
/// the general form. This renderer only ever calls it with
/// `normal = GROUND_NORMAL = [0, 0, 1]` (see `calculate_ground_radiance`),
/// for which two things collapse:
///   - `NdotL = dot(normal, dir) = dir.z`
///   - `vertical_factor = lerp(0.92, 1, asin(clamp(1, -1, 1)) / PI + 0.5)
///                       = lerp(0.92, 1, (PI/2) / PI + 0.5) = lerp(0.92, 1, 1)
///                       = 1.0` exactly, for every `dir`.
///
/// So today this always multiplies by exactly 1.0 -- transcribed anyway
/// rather than hardcoded, so it stays correct if a real normal ever shows
/// up.
fn calculate_ambient_directional_factor(dir: [f64; 3], normal: [f64; 3], dir_color: [f64; 3]) -> f64 {
    let n_dot_l = normal[0] * dir[0] + normal[1] * dir[1] + normal[2] * dir[2];

    let factor_reduction_max = 0.3;
    let dir_luminance = dir_color[0] * 0.2126 + dir_color[1] * 0.7152 + dir_color[2] * 0.0722;
    let directional_factor_min = 1.0 - factor_reduction_max * dir_luminance.min(1.0);

    let lerp = |a: f64, b: f64, t: f64| (1.0 - t) * a + t * b;
    let ambient_directional_factor = lerp(directional_factor_min, 1.0, (n_dot_l + 1.0).min(1.0));

    let vertical_factor_min = 0.92;
    let vertical_factor =
        lerp(vertical_factor_min, 1.0, normal[2].clamp(-1.0, 1.0).asin() / std::f64::consts::PI + 0.5);

    vertical_factor * ambient_directional_factor
}

/// The ground normal every 2D layer this renderer draws is lit with -- see
/// `calculate_ground_radiance`'s doc comment. Mapbox's own
/// `calculateGroundRadiance` hardcodes this as a local, not a parameter;
/// named here only so `calculate_ambient_directional_factor`'s call site
/// reads as what it is.
const GROUND_NORMAL: [f64; 3] = [0.0, 0.0, 1.0];

/// `calculateGroundRadiance` from `3d-style/render/lights.ts`, transcribed
/// verbatim. `dir_color_linear`/`ambient_color_linear` are already in
/// linear space (Mapbox's `sRGBToLinearAndScale`, applied by the caller,
/// `ground_radiance_inner`) and the RETURN is sRGB again (Mapbox's
/// `linearVec3TosRGB`) -- this is the exact `u_ground_radiance` value
/// `_prelude_lighting.glsl`'s `apply_lighting_ground` multiplies straight
/// into an sRGB paint colour. Multiplying a linear quantity into an sRGB
/// one is not physically rigorous and is not this module's place to fix:
/// match it.
///
/// NOT clamped: `day` legitimately exceeds 1.0 in the acceptance fixture
/// (`[1.3029, 1.3029, 1.3029]`) and clamping happens later, when a colour
/// is written to a `u8`.
fn calculate_ground_radiance(dir: [f64; 3], dir_color_linear: [f64; 3], ambient_color_linear: [f64; 3]) -> [f64; 3] {
    let ambient_directional_factor = calculate_ambient_directional_factor(dir, GROUND_NORMAL, dir_color_linear);

    let radiance_linear = [
        ambient_color_linear[0] * ambient_directional_factor + dir_color_linear[0] * dir[2],
        ambient_color_linear[1] * ambient_directional_factor + dir_color_linear[1] * dir[2],
        ambient_color_linear[2] * ambient_directional_factor + dir_color_linear[2] * dir[2],
    ];

    [radiance_linear[0].powf(1.0 / 2.2), radiance_linear[1].powf(1.0 / 2.2), radiance_linear[2].powf(1.0 / 2.2)]
}

/// A directional light's `direction` property, folded to its
/// `[azimuthal, polar]` pair. The style JSON spells it `["literal", [az,
/// pol]]` (see `examples/mapbox-standard-dark-style.json`), which
/// `eval_inner`'s `"literal"` arm reduces to `EvalValue::Array([Number,
/// Number])` -- there is no dedicated "direction" value kind in this
/// folder's vocabulary.
fn direction_pair(value: &EvalValue) -> Result<[f64; 2]> {
    let EvalValue::Array(items) = value else {
        bail!("a directional light's \"direction\" must evaluate to a two-element array, got a {}", value.type_name());
    };
    match items.as_slice() {
        [azimuthal, polar] => Ok([azimuthal.as_number()?, polar.as_number()?]),
        other => bail!(
            "a directional light's \"direction\" must have exactly two elements [azimuthal, polar], got {}",
            other.len()
        ),
    }
}

/// The scene's `directional` light, evaluated at `zoom`: its `(color,
/// intensity, [azimuthal, polar] degrees)`. A property the light does not
/// declare takes the Mapbox v3 style-spec default (white, `0.5`,
/// `[210, 30]`); one that declares but will not fold is an error.
///
/// For `style_spec::compile_extrusion_light`, which calls it at every
/// integer zoom (the intensity is a zoom interpolate in Standard) and maps
/// each result onto the MapLibre-shaped light extrusions are shaded with
/// (R16).
pub(crate) fn directional_light(scope: Scope<'_>, zoom: u8) -> Result<(crate::style::Rgba, f64, [f64; 2])> {
    let lights = scope.lights().ok_or_else(|| anyhow::anyhow!("no \"lights\" resolved for this compile"))?;
    let directional = find_light(lights, "directional")?;
    let property = |name: &str| directional.get("properties").and_then(|properties| properties.get(name));
    let color = match property("color") {
        Some(expr) => eval_inner(expr, scope, zoom).and_then(|v| v.as_color()).context("evaluating the directional light's color")?,
        None => crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    };
    let intensity = match property("intensity") {
        Some(expr) => {
            eval_inner(expr, scope, zoom).and_then(|v| v.as_number()).context("evaluating the directional light's intensity")?
        }
        None => 0.5,
    };
    let direction = match property("direction") {
        Some(expr) => {
            direction_pair(&eval_inner(expr, scope, zoom).context("evaluating the directional light's direction")?)?
        }
        None => [210.0, 30.0],
    };
    Ok((color, intensity, direction))
}

/// Evaluates `u_ground_radiance` -- the vec3 (NOT a scalar: a coloured
/// light tints the map) that `_prelude_lighting.glsl`'s
/// `apply_lighting_ground` multiplies into every 2D layer's colour.
/// Computing it is the whole job of this function; applying it is
/// `style_spec::light_ground_color`'s, which bakes it into each compiled
/// colour.
///
/// Reduces the `lights` array's `ambient` AND `directional` entries --
/// `color`, `intensity`, and (`directional` only) `direction`, each an
/// ordinary expression in this vocabulary exactly like
/// `measure_light_brightness`'s inputs -- through `eval_inner` at `zoom`,
/// converts each light's colour to linear space scaled by its intensity
/// (`sRGBToLinearAndScale`, `3d-style/render/lights.ts`), turns
/// `direction` into a unit vector (`spherical_direction_to_cartesian`),
/// and folds all of it through `calculate_ground_radiance`.
///
/// Depends on `zoom`: `directional.intensity` is a zoom `interpolate` in
/// the real fixture, so this is NOT safe to compute once per style --
/// call it at whatever zoom is being drawn, the same way
/// `compile_folded` walks a zoom-dependent paint property one integer
/// zoom at a time.
///
/// A style with no `lights` on the scope -- every style before Mapbox
/// Standard, including both vendored fixtures -- errors, marked
/// `Unfoldable` the same way `eval` marks every error out of this module:
/// there is no radiance without a `lights` array, which is the
/// `LIGHTING_3D_MODE` gate and what keeps Liberty and Mapbox Streets
/// byte-identical (see `measure_light_with_no_lights_on_the_scope_does_not_fold`,
/// the existing precedent for this same gate on `measure-light`).
///
/// Same refusal when `lights` is present but only ONE of `ambient`/
/// `directional` is in it (`find_light` errors on whichever is missing,
/// propagating out of `ground_radiance_inner` the same way the "no
/// `lights` at all" case does) -- this matches the reference, not just an
/// arbitrary choice: `src/render/painter.ts`'s `uploadCommonLightUniforms`
/// (~line 1769) only computes/uploads `u_ground_radiance` `if
/// (directionalLight && ambientLight)`, i.e. Mapbox itself skips lighting
/// entirely unless BOTH lights are present, not just one. See
/// `ground_radiance_with_no_directional_entry_does_not_fold`.
pub fn ground_radiance(scope: Scope<'_>, zoom: u8) -> Result<[f32; 3]> {
    ground_radiance_inner(scope, zoom).map_err(|error| anyhow::Error::new(Unfoldable(format!("{error:#}"))))
}

fn ground_radiance_inner(scope: Scope<'_>, zoom: u8) -> Result<[f32; 3]> {
    let lights = scope.lights().ok_or_else(|| anyhow::anyhow!("no \"lights\" resolved for this compile"))?;
    let ambient = find_light(lights, "ambient")?;
    let directional = find_light(lights, "directional")?;

    let ambient_color = eval_inner(light_property(ambient, "color", "ambient")?, scope, zoom)
        .and_then(|v| v.as_color())
        .context("evaluating the ambient light's color")?;
    let ambient_intensity = eval_inner(light_property(ambient, "intensity", "ambient")?, scope, zoom)
        .and_then(|v| v.as_number())
        .context("evaluating the ambient light's intensity")?;
    let directional_color = eval_inner(light_property(directional, "color", "directional")?, scope, zoom)
        .and_then(|v| v.as_color())
        .context("evaluating the directional light's color")?;
    let directional_intensity = eval_inner(light_property(directional, "intensity", "directional")?, scope, zoom)
        .and_then(|v| v.as_number())
        .context("evaluating the directional light's intensity")?;
    let direction = eval_inner(light_property(directional, "direction", "directional")?, scope, zoom)
        .context("evaluating the directional light's direction")?;
    let [azimuthal, polar] = direction_pair(&direction)?;

    let dir = spherical_direction_to_cartesian(azimuthal, polar);

    // `sRGBToLinearAndScale`, `3d-style/render/lights.ts`: `pow(channel,
    // 2.2) * intensity`, per channel.
    let to_linear = |channel: f32, intensity: f64| (channel as f64).powf(2.2) * intensity;
    let ambient_linear = [
        to_linear(ambient_color.r, ambient_intensity),
        to_linear(ambient_color.g, ambient_intensity),
        to_linear(ambient_color.b, ambient_intensity),
    ];
    let directional_linear = [
        to_linear(directional_color.r, directional_intensity),
        to_linear(directional_color.g, directional_intensity),
        to_linear(directional_color.b, directional_intensity),
    ];

    let radiance = calculate_ground_radiance(dir, directional_linear, ambient_linear);
    Ok([radiance[0] as f32, radiance[1] as f32, radiance[2] as f32])
}

/// `["let", name1, value1, ..., body]` -- the folder's own `let`. Refuses a
/// feature-dependent binding, unlike `compile_let`: this function reduces
/// an expression to exactly ONE `EvalValue`, with no feature in scope at
/// all, so a feature-dependent binding has no possible answer here -- see
/// `let_bindings`'s own doc comment for the full story of why the two
/// `let`s now disagree.
fn eval_let(arr: &[Value], expr: &Value, scope: Scope<'_>, zoom: u8) -> Result<EvalValue> {
    let bindings = let_bindings(arr, expr)?;
    // `push` borrows `scope`, so it needs a place to borrow from; the
    // returned frame lives exactly as long as this call, which is exactly as
    // long as the body is being evaluated.
    let enclosing = scope;
    let inner = enclosing.push(&bindings);
    eval_inner(&arr[arr.len() - 1], inner, zoom)
}

/// The `(name, unevaluated value)` pairs of a `let`, shape-checked only --
/// no judgement on whether a binding is feature-dependent. Shared by
/// `let_bindings` (which adds the eval-time refusal below, for `eval_let`)
/// and `compile_let` (which allows a feature-dependent binding and inlines
/// it instead -- see `compile_let`'s doc comment).
pub(crate) fn parse_let_bindings<'a>(arr: &'a [Value], expr: &Value) -> Result<Vec<(&'a str, &'a Value)>> {
    if arr.len() < 4 || !arr.len().is_multiple_of(2) {
        bail!("\"let\" must have the form [\"let\", name, value, ..., body]: {expr}");
    }
    let mut bindings = Vec::with_capacity(arr.len() / 2 - 1);
    let mut i = 1;
    while i + 1 < arr.len() - 1 {
        let name = arr[i]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("\"let\" binding names must be strings: {expr}"))?;
        bindings.push((name, &arr[i + 1]));
        i += 2;
    }
    Ok(bindings)
}

/// The `(name, unevaluated value)` pairs of a `let`, with every feature-
/// dependent binding refused by name. Used only by `eval_let`, the
/// constant-fold-time `let` (`eval()`/`eval_inner`, which reduces an
/// expression to exactly ONE `EvalValue` with no feature in scope at all --
/// a feature-dependent binding there has no possible answer, so refusing it
/// is not a missing feature, it is the only correct behaviour for that
/// path). `compile_let` -- the STRUCTURAL `let`, which can represent
/// per-feature variation via `Filter`-discriminated cases -- does NOT use
/// this; see its own doc comment for why a feature-dependent binding is
/// fine there.
///
/// Audited over the vendored `examples/mapbox-standard-dark-style.json`: it
/// has 39 `let` expressions binding 110 names, and exactly 4 of those
/// bindings read the feature (`isHighClass` twice, `icon_name`,
/// `densityByClass`) -- the two `isHighClass`es are `roads`/`bridge-roads`'
/// real `line-emissive-strength`, which `compile_let` now handles; the
/// other two are a filter operator's own operand (`poi-label`'s `let`
/// inside `["<=", ["get","filterrank"], ...]`), never reaching this
/// module's `let_bindings`/`compile_let` at all -- filters are compiled by
/// `style_spec::filter`, a separate module.
fn let_bindings<'a>(arr: &'a [Value], expr: &Value) -> Result<Vec<(&'a str, &'a Value)>> {
    let bindings = parse_let_bindings(arr, expr)?;
    for (name, value) in &bindings {
        if let Some(operator) = feature_operator_in(value) {
            bail!(
                "\"let\" binding {name:?} depends on the feature being painted (it uses {operator:?}), \
                 which the constant folder refuses to reduce to one value here (it has no feature in \
                 scope to answer with) rather than the STRUCTURAL \"let\" (`compile_let`), which \
                 inlines a binding like this one instead: {value}"
            );
        }
    }
    Ok(bindings)
}

/// The style spec's `to-number`: numbers pass through, `null` is 0, booleans
/// are 1 and 0, and a string is parsed. Anything else fails to convert,
/// which the multi-argument form treats as "try the next one".
fn coerce_to_number(value: &EvalValue) -> Result<f64> {
    match value {
        EvalValue::Number(n) => Ok(*n),
        EvalValue::Null => Ok(0.0),
        EvalValue::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        EvalValue::String(s) => {
            s.trim().parse::<f64>().map_err(|_| anyhow::anyhow!("string {s:?} does not name a number"))
        }
        other => bail!("a {} cannot be converted to a number", other.type_name()),
    }
}

/// A `match` case's label list: a bare label, or an array of them.
fn match_labels(labels: &Value) -> Vec<&Value> {
    match labels {
        Value::Array(items) => items.iter().collect(),
        single => vec![single],
    }
}

/// Whether a `match` case label (raw JSON, never evaluated -- labels are
/// literals by the spec) selects `input`.
fn label_matches(label: &Value, input: &EvalValue) -> bool {
    match (label, input) {
        (Value::String(l), EvalValue::String(i)) => l == i,
        (Value::Number(l), EvalValue::Number(i)) => l.as_f64() == Some(*i),
        (Value::Bool(l), EvalValue::Bool(i)) => l == i,
        _ => false,
    }
}

/// `==`/`!=` compare any two values of the same kind; the four orderings are
/// numbers and strings only, as the style spec defines them.
fn compare(operator: &str, a: &EvalValue, b: &EvalValue, expr: &Value) -> Result<bool> {
    if operator == "==" || operator == "!=" {
        let equal = a == b;
        return Ok(if operator == "==" { equal } else { !equal });
    }
    let ordering = match (a, b) {
        (EvalValue::Number(x), EvalValue::Number(y)) => x.partial_cmp(y),
        (EvalValue::String(x), EvalValue::String(y)) => Some(x.cmp(y)),
        _ => bail!("{operator:?} compares two numbers or two strings, got a {} and a {}: {expr}", a.type_name(), b.type_name()),
    };
    let Some(ordering) = ordering else {
        return Ok(false); // NaN compares false against everything.
    };
    Ok(match operator {
        "<" => ordering.is_lt(),
        "<=" => ordering.is_le(),
        ">" => ordering.is_gt(),
        _ => ordering.is_ge(),
    })
}

/// The folded `interpolate`: the same bracket-and-ease evaluation
/// `interpolate_at` does for the zoom compiler, but over already-folded
/// `EvalValue` stops and clamping outside the declared range.
fn interpolate_folded(stops: &[(f64, EvalValue)], input: f64, curve: Interpolation, expr: &Value) -> Result<EvalValue> {
    let first = stops.first().ok_or_else(|| anyhow::anyhow!("\"interpolate\" needs at least one stop: {expr}"))?;
    if input <= first.0 {
        return Ok(first.1.clone());
    }
    let last = &stops[stops.len() - 1];
    if input >= last.0 {
        return Ok(last.1.clone());
    }
    let idx = stops
        .iter()
        .rposition(|(at, _)| *at <= input)
        .expect("input is strictly between the first and last stop, so a lower bracket exists");
    let (ia, va) = &stops[idx];
    let (ib, vb) = &stops[idx + 1];
    // Kept in `f64` for the numeric case: rounding `t` to `f32` first, as
    // the zoom compiler does (its stops are already `f32`-typed), is enough
    // to turn a folded 40 into 40.00000059604645.
    let t = curve.t_at(input, *ia, *ib);
    match (va, vb) {
        (EvalValue::Number(a), EvalValue::Number(b)) => Ok(EvalValue::Number(a + (b - a) * t)),
        _ => match (va.as_color(), vb.as_color()) {
            (Ok(a), Ok(b)) => Ok(EvalValue::Color(crate::style::Lerp::lerp(&a, &b, t as f32))),
            _ => bail!("\"interpolate\" can only blend numbers or colors: {expr}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::TagValue;
    use crate::style_spec::color::parse_css_color;
    use std::collections::HashMap;

    fn tags(pairs: &[(&str, TagValue)]) -> HashMap<String, TagValue> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn line_progress_evaluates_inside_a_scope_that_has_it_and_is_refused_outside() {
        let expr = serde_json::json!(["interpolate", ["linear"], ["line-progress"], 0, "#000000", 1, "#ffffff"]);
        let v = eval(&expr, Scope::default().with_line_progress(0.5), 10).unwrap();
        assert!(matches!(v, EvalValue::Color(c) if (c.r - 0.5).abs() < 0.01), "{v:?}");
        assert!(eval(&expr, Scope::default(), 10).is_err());
        // Survives a `let` frame: the progress is not lexical.
        let in_let = serde_json::json!(["let", "p", ["line-progress"], ["*", 2, ["var", "p"]]]);
        let v = eval(&in_let, Scope::default().with_line_progress(0.25), 0).unwrap();
        assert!(matches!(v, EvalValue::Number(n) if (n - 0.5).abs() < 1e-12), "{v:?}");
    }

    #[test]
    fn compiles_a_literal_color_to_a_single_constant_pair() {
        // park: fill-color: "#d8e8c8" (a bare literal, not an array expression)
        let json: Value = serde_json::json!("#d8e8c8");
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1);
        assert!(matches!(pairs[0].0, Filter::Any));

        let expected = parse_css_color("#d8e8c8").unwrap();
        let c0 = pairs[0].1.value_at(0);
        let c20 = pairs[0].1.value_at(20);
        assert_eq!((c0.r, c0.g, c0.b, c0.a), (expected.r, expected.g, expected.b, expected.a));
        // Same value at every zoom (constant, not interpolated).
        assert_eq!((c0.r, c0.g, c0.b, c0.a), (c20.r, c20.g, c20.b, c20.a));
    }

    #[test]
    fn compiles_a_pure_zoom_interpolate_to_a_single_interpolated_pair() {
        // landuse_residential: fill-color:
        // ["interpolate", ["linear"], ["zoom"], 9, "hsla(0,3%,85%,0.84)", 12, "hsla(35,57%,88%,0.49)"]
        let json: Value = serde_json::from_str(
            r#"["interpolate", ["linear"], ["zoom"], 9, "hsla(0,3%,85%,0.84)", 12, "hsla(35,57%,88%,0.49)"]"#,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1);
        assert!(matches!(pairs[0].0, Filter::Any));

        let stop1 = parse_css_color("hsla(0,3%,85%,0.84)").unwrap();
        let stop2 = parse_css_color("hsla(35,57%,88%,0.49)").unwrap();
        let v9 = pairs[0].1.value_at(9);
        let v12 = pairs[0].1.value_at(12);
        assert_eq!((v9.r, v9.g, v9.b, v9.a), (stop1.r, stop1.g, stop1.b, stop1.a));
        assert_eq!((v12.r, v12.g, v12.b, v12.a), (stop2.r, stop2.g, stop2.b, stop2.a));

        // A midpoint zoom must be strictly between the two stops
        // componentwise -- confirms linear interpolation, not a step.
        let mid = pairs[0].1.value_at(10);
        for (m, a, b) in [(mid.r, stop1.r, stop2.r), (mid.g, stop1.g, stop2.g), (mid.b, stop1.b, stop2.b), (mid.a, stop1.a, stop2.a)] {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            assert!(m > lo && m < hi, "component {m} not strictly between {lo} and {hi}");
        }
    }

    #[test]
    fn compiles_exponential_interpolate_using_the_true_curve_not_a_linear_approximation() {
        // waterway_river: line-width: ["interpolate", ["exponential", 1.2], ["zoom"], 11, 0.5, 20, 6]
        let json: Value = serde_json::from_str(r#"["interpolate", ["exponential", 1.2], ["zoom"], 11, 0.5, 20, 6]"#).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1);
        assert!(matches!(pairs[0].0, Filter::Any));

        assert!((pairs[0].1.value_at(11) - 0.5).abs() < 1e-6);
        assert!((pairs[0].1.value_at(20) - 6.0).abs() < 1e-6);

        // True exponential value at z15 (base 1.2):
        // t = (1.2^4 - 1) / (1.2^9 - 1) ≈ 0.25809, value ≈ 0.5 + 5.5*t ≈ 1.9195
        // (computed independently in Python). A naive linear interpolation
        // between the two raw (non-integer-free) stops would instead give
        // ≈ 2.9444 -- the +53% bug this function's resampling fix
        // corrected. Assert the compiled value matches the true curve and
        // is clearly NOT the naive-linear value.
        let true_exponential = 1.9195_f32;
        let naive_linear_between_raw_stops = 2.9444_f32;
        assert!((pairs[0].1.value_at(15) - true_exponential).abs() < 1e-3);
        assert!((pairs[0].1.value_at(15) - naive_linear_between_raw_stops).abs() > 0.5);
    }

    #[test]
    fn compiles_interpolate_with_a_half_integer_zoom_stop() {
        // tunnel_street_casing: line-opacity: ["interpolate", ["linear"], ["zoom"], 12, 0, 12.5, 1]
        let json: Value = serde_json::from_str(r#"["interpolate", ["linear"], ["zoom"], 12, 0, 12.5, 1]"#).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1);
        // Below the first stop's (floored) resample start, clamps to the
        // first stop's value.
        assert_eq!(pairs[0].1.value_at(11), 0.0);
        // At/after the last stop's (ceiled) resample end (12.5 -> 13),
        // value is clamped to the last stop's value.
        assert_eq!(pairs[0].1.value_at(13), 1.0);
    }

    #[test]
    fn resamples_exponential_stops_so_fractional_zoom_stops_dont_collide() {
        // tunnel_motorway_link: line-width:
        // ["interpolate", ["exponential", 1.2], ["zoom"], 12.5, 0, 13, 1.5, 14, 2.5, 20, 11.5]
        //
        // Regression test: an earlier version of `compile_interpolate`
        // rounded each raw stop's zoom to the nearest integer, which
        // collided the 12.5 and 13 stops onto the same u8 key (13).
        // `ZoomValue::value_at_linear` resolves duplicate keys to the
        // FIRST match, so `value_at(13)` silently returned 0 (the
        // collided-away 12.5 stop's value) instead of the real MapLibre
        // value of 1.5 -- zeroing out this road class's width right at the
        // zoom it first appears. The fix resamples the true curve at
        // integer zooms instead of rounding raw stop positions, so no two
        // resampled stops can ever share a key.
        let json: Value =
            serde_json::from_str(r#"["interpolate", ["exponential", 1.2], ["zoom"], 12.5, 0, 13, 1.5, 14, 2.5, 20, 11.5]"#).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1.value_at(13), 1.5, "z13 must be the real MapLibre value (1.5), not 0 from a collided stop");
    }

    #[test]
    fn resamples_a_second_colliding_fractional_zoom_shape() {
        // road_minor: line-width: ["interpolate", ["exponential", 1.2], ["zoom"], 13.5, 0, 14, 2.5, 20, 18]
        // Same collision class as the tunnel_motorway_link regression test
        // above, but with the fractional stop first and an even zoom
        // second (13.5 and 14 both used to round to 14).
        let json: Value = serde_json::from_str(r#"["interpolate", ["exponential", 1.2], ["zoom"], 13.5, 0, 14, 2.5, 20, 18]"#).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1.value_at(14), 2.5, "z14 must be the real MapLibre value (2.5), not 0 from a collided stop");
    }

    #[test]
    fn expands_a_match_on_get_into_one_pair_per_case() {
        // A synthetic but realistic example matching Liberty's real
        // match-on-["get", key] pattern (case-labels array, per-case
        // output, trailing default) -- see `compile_match`'s doc comment
        // for why no real Liberty *paint* property uses "match" verbatim.
        let json: Value = serde_json::from_str(
            r##"["match", ["get", "class"], ["motorway", "trunk"], "#e892a2", ["primary"], "#fcd6a4", "#f7fabf"]"##,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 3);

        let motorway = tags(&[("class", TagValue::String("motorway".into()))]);
        let trunk = tags(&[("class", TagValue::String("trunk".into()))]);
        let primary = tags(&[("class", TagValue::String("primary".into()))]);
        let other = tags(&[("class", TagValue::String("residential".into()))]);

        // Every input matches exactly one pair's filter: the explicit
        // cases claim their own listed classes, and the default filter
        // (negation of all case filters) claims everything else.
        let matching_count = |t: &HashMap<String, TagValue>| pairs.iter().filter(|(f, _)| f.matches(t, 14.0)).count();
        assert_eq!(matching_count(&motorway), 1);
        assert_eq!(matching_count(&trunk), 1);
        assert_eq!(matching_count(&primary), 1);
        assert_eq!(matching_count(&other), 1);

        let color_for = |t: &HashMap<String, TagValue>| pairs.iter().find(|(f, _)| f.matches(t, 14.0)).unwrap().1.value_at(10);

        let motorway_color = color_for(&motorway);
        let expected_motorway = parse_css_color("#e892a2").unwrap();
        assert_eq!((motorway_color.r, motorway_color.g, motorway_color.b), (expected_motorway.r, expected_motorway.g, expected_motorway.b));

        let trunk_color = color_for(&trunk);
        assert_eq!((trunk_color.r, trunk_color.g, trunk_color.b), (expected_motorway.r, expected_motorway.g, expected_motorway.b));

        let primary_color = color_for(&primary);
        let expected_primary = parse_css_color("#fcd6a4").unwrap();
        assert_eq!((primary_color.r, primary_color.g, primary_color.b), (expected_primary.r, expected_primary.g, expected_primary.b));

        let default_color = color_for(&other);
        let expected_default = parse_css_color("#f7fabf").unwrap();
        assert_eq!((default_color.r, default_color.g, default_color.b), (expected_default.r, expected_default.g, expected_default.b));
    }

    #[test]
    fn expands_a_match_whose_case_output_is_itself_an_interpolate() {
        // Exercises the recursive case: a match case's output is another
        // expression (an interpolate), not a flat literal. Not present in
        // real Liberty paint data (no paint property uses "match" at all,
        // per `compile_match`'s doc comment), but the recursion is real
        // code that must be tested directly.
        let json: Value = serde_json::from_str(
            r#"["match", ["get", "class"], ["motorway"], ["interpolate", ["linear"], ["zoom"], 10, 1, 18, 4], 2]"#,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        // One pair for the motorway case, one for the default.
        assert_eq!(pairs.len(), 2);

        let motorway = tags(&[("class", TagValue::String("motorway".into()))]);
        let other = tags(&[("class", TagValue::String("residential".into()))]);

        let motorway_pair = pairs.iter().find(|(f, _)| f.matches(&motorway, 14.0)).unwrap();
        assert_eq!(motorway_pair.1.value_at(10), 1.0);
        assert_eq!(motorway_pair.1.value_at(18), 4.0);
        // Confirms it's really interpolated, not a flat constant.
        assert!(motorway_pair.1.value_at(14) > 1.0 && motorway_pair.1.value_at(14) < 4.0);

        let default_pair = pairs.iter().find(|(f, _)| f.matches(&other, 14.0)).unwrap();
        assert_eq!(default_pair.1.value_at(0), 2.0);
        assert_eq!(default_pair.1.value_at(20), 2.0);
    }

    #[test]
    fn compiles_a_case_over_has_into_filtered_cases() {
        let json = serde_json::json!(["case",
            ["has", "beta"],
            "#ff0000",
            "#00ff00"]);
        let cases = compile_zoom_value(&json, color_leaf, Scope::root()).expect("case must compile");
        assert_eq!(cases.len(), 2);
        assert!(matches!(&cases[0].0, Filter::Has(k) if k == "beta"));
        assert!(matches!(&cases[1].0, Filter::Not(inner) if matches!(&**inner, Filter::Has(k) if k == "beta")));
    }

    #[test]
    fn a_later_arms_condition_is_anded_with_the_negation_of_every_earlier_one() {
        // Real `case` semantics are if/elif/else, not if/if/else: a feature
        // satisfying BOTH conditions must be claimed by exactly one arm (the
        // first whose condition is true), never by both.
        let json = serde_json::json!(["case",
            ["has", "a"],
            "#ff0000",
            ["has", "b"],
            "#00ff00",
            "#0000ff"]);
        let cases = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(cases.len(), 3);

        let both = tags(&[("a", TagValue::String("1".into())), ("b", TagValue::String("1".into()))]);
        let matching: Vec<_> = cases.iter().filter(|(f, _)| f.matches(&both, 14.0)).collect();
        assert_eq!(matching.len(), 1, "a feature satisfying both conditions must match exactly one arm's filter");

        // And it must be the FIRST arm's output, not the second's.
        let matched_color = matching[0].1.value_at(10);
        let expected = parse_css_color("#ff0000").unwrap();
        assert_eq!((matched_color.r, matched_color.g, matched_color.b), (expected.r, expected.g, expected.b));

        // A feature satisfying only the second condition still gets exactly
        // one match (the second arm) -- the negation of the first condition
        // doesn't accidentally exclude features that never satisfied it.
        let only_b = tags(&[("b", TagValue::String("1".into()))]);
        assert_eq!(cases.iter().filter(|(f, _)| f.matches(&only_b, 14.0)).count(), 1);
    }

    #[test]
    fn a_case_arm_may_itself_be_a_match() {
        // The real shield shape: each branch is a match over a colour name.
        let json = serde_json::json!(["case",
            ["has", "beta"],
            ["match", ["get", "beta_color"], "white", "#ffffff", "#000000"],
            ["match", ["get", "color"], "white", "#ffffff", "red", "#ff0000", "#000000"]]);
        let cases = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        // 2 arms from the first branch (match + its fallback) and 3 from the
        // second, each ANDed with the case condition or its negation.
        assert_eq!(cases.len(), 5);
        assert!(cases.iter().all(|(f, _)| matches!(f, Filter::All(_) | Filter::Has(_) | Filter::Not(_))));
    }

    #[test]
    fn a_case_condition_drops_an_unevaluable_image_test_but_keeps_the_has() {
        // The real condition: ["all", ["has", k], ["to-boolean", ["coalesce",
        // ["image", ...], ""]]]. The image test cannot be evaluated where the
        // value is resolved, so only the `has` survives.
        let json = serde_json::json!(["case",
            ["all", ["has", "beta"], ["to-boolean", ["coalesce", ["image", "x"], ""]]],
            "#ff0000",
            "#00ff00"]);
        let cases = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert!(matches!(&cases[0].0, Filter::Has(k) if k == "beta"));
    }

    #[test]
    fn a_case_condition_this_compiler_cannot_read_is_an_error() {
        // Anything other than `has` / `all`-containing-`has` stays out of scope,
        // so the layer degrades exactly as it does today rather than compiling
        // to a filter that means something else.
        let json = serde_json::json!(["case", ["==", ["get", "x"], 1], "#ff0000", "#00ff00"]);
        assert!(compile_zoom_value(&json, color_leaf, Scope::root()).is_err());
    }

    #[test]
    fn an_all_condition_with_more_than_one_has_clause_is_an_error() {
        // `compile_case_condition` is documented to support "exactly one"
        // `has` clause inside an `"all"`. A second one must be rejected, not
        // silently overwrite the first and drop it with no notice.
        let json = serde_json::json!(["case",
            ["all", ["has", "a"], ["has", "b"]],
            "#ff0000",
            "#00ff00"]);
        assert!(compile_zoom_value(&json, color_leaf, Scope::root()).is_err());
    }

    #[test]
    fn rejects_an_unsupported_operator() {
        // building-3d: fill-extrusion-base: ["get", "render_min_height"] --
        // a data-driven (not zoom-driven) property this compiler doesn't
        // support.
        let json: Value = serde_json::from_str(r#"["get", "render_min_height"]"#).unwrap();
        assert!(compile_zoom_value(&json, number_leaf, Scope::root()).is_err());
    }

    #[test]
    fn rejects_property_based_interpolation() {
        let json: Value = serde_json::from_str(r#"["interpolate", ["linear"], ["get", "rank"], 1, 0, 10, 1]"#).unwrap();
        assert!(compile_zoom_value(&json, number_leaf, Scope::root()).is_err());
    }

    #[test]
    fn compiles_a_plain_step_expression_with_flat_literal_segments() {
        // Synthetic (real line-opacity steps have this shape, e.g.
        // road-street's ["step", ["zoom"], 0, 14, 1] -- line-opacity is now
        // a compiled property, so this exact shape is real, common input;
        // this test still needs to exist directly since nothing else
        // exercises the plain path).
        let json: Value = serde_json::from_str(r#"["step", ["zoom"], 1.0, 10, 2.0, 15, 3.0]"#).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1);
        assert!(matches!(pairs[0].0, Filter::Any));
        assert_eq!(pairs[0].1.value_at(5), 1.0);
        assert_eq!(pairs[0].1.value_at(10), 2.0);
        assert_eq!(pairs[0].1.value_at(14), 2.0);
        assert_eq!(pairs[0].1.value_at(15), 3.0);
        assert_eq!(pairs[0].1.value_at(20), 3.0);
    }

    #[test]
    fn compiles_the_real_road_motorway_trunk_step_shape_with_per_segment_defaults() {
        // road-motorway-trunk (examples/mapbox-streets-style.json):
        // ["step", ["zoom"],
        //   ["match", ["get", "class"], "motorway", "hsl(30, 88%, 64%)", "trunk", "hsl(50, 81%, 54%)", "hsl(20, 18%, 100%)"],
        //   9,
        //   ["match", ["get", "class"], "motorway", "hsl(30, 100%, 70%)", "hsl(50, 89%, 70%)"]]
        let json: Value = serde_json::from_str(
            r#"["step", ["zoom"],
                 ["match", ["get", "class"], "motorway", "hsl(30, 88%, 64%)", "trunk", "hsl(50, 81%, 54%)", "hsl(20, 18%, 100%)"],
                 9,
                 ["match", ["get", "class"], "motorway", "hsl(30, 100%, 70%)", "hsl(50, 89%, 70%)"]]"#,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).unwrap();
        // One pair for "motorway", one for "trunk", one default -- three total.
        assert_eq!(pairs.len(), 3);

        let motorway = tags(&[("class", TagValue::String("motorway".into()))]);
        let trunk = tags(&[("class", TagValue::String("trunk".into()))]);

        let motorway_pair = pairs.iter().find(|(f, _)| f.matches(&motorway, 14.0)).unwrap();
        let below9 = motorway_pair.1.value_at(5);
        let at9 = motorway_pair.1.value_at(9);
        let expected_below9 = parse_css_color("hsl(30, 88%, 64%)").unwrap();
        let expected_at9 = parse_css_color("hsl(30, 100%, 70%)").unwrap();
        assert_eq!((below9.r, below9.g, below9.b), (expected_below9.r, expected_below9.g, expected_below9.b));
        assert_eq!((at9.r, at9.g, at9.b), (expected_at9.r, expected_at9.g, expected_at9.b));

        // The key assertion: "trunk" below zoom 9 uses the BASE segment's
        // own "trunk" case, but at/above zoom 9 -- where the second
        // segment has no explicit "trunk" case -- it must fall through to
        // the SECOND segment's own default, not reuse the base segment's
        // "trunk" color.
        let trunk_pair = pairs.iter().find(|(f, _)| f.matches(&trunk, 14.0)).unwrap();
        let trunk_below9 = trunk_pair.1.value_at(5);
        let trunk_at9 = trunk_pair.1.value_at(9);
        let expected_trunk_below9 = parse_css_color("hsl(50, 81%, 54%)").unwrap();
        let expected_trunk_at9_is_second_segment_default = parse_css_color("hsl(50, 89%, 70%)").unwrap();
        assert_eq!(
            (trunk_below9.r, trunk_below9.g, trunk_below9.b),
            (expected_trunk_below9.r, expected_trunk_below9.g, expected_trunk_below9.b)
        );
        assert_eq!(
            (trunk_at9.r, trunk_at9.g, trunk_at9.b),
            (expected_trunk_at9_is_second_segment_default.r, expected_trunk_at9_is_second_segment_default.g, expected_trunk_at9_is_second_segment_default.b),
            "trunk at zoom>=9 must use the SECOND segment's own default color, not the base segment's trunk color"
        );
    }

    #[test]
    fn compiles_the_real_waterway_interpolate_shape_with_a_match_valued_stop() {
        // waterway / waterway-shadow (examples/mapbox-streets-style.json):
        // ["interpolate", ["exponential", 1.3], ["zoom"],
        //   9, ["match", ["get", "class"], ["canal", "river"], 0.1, 0],
        //   20, ["match", ["get", "class"], ["canal", "river"], 8, 3]]
        let json: Value = serde_json::from_str(
            r#"["interpolate", ["exponential", 1.3], ["zoom"],
                 9, ["match", ["get", "class"], ["canal", "river"], 0.1, 0],
                 20, ["match", ["get", "class"], ["canal", "river"], 8, 3]]"#,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        // One pair for "canal", one for "river", one default -- three total.
        assert_eq!(pairs.len(), 3);

        let canal = tags(&[("class", TagValue::String("canal".into()))]);
        let other = tags(&[("class", TagValue::String("stream".into()))]);

        let canal_pair = pairs.iter().find(|(f, _)| f.matches(&canal, 14.0)).unwrap();
        assert!((canal_pair.1.value_at(9) - 0.1).abs() < 1e-6);
        assert!((canal_pair.1.value_at(20) - 8.0).abs() < 1e-6);
        // Strictly between the two stops at a midpoint zoom -- confirms
        // real interpolation, not a step.
        let mid = canal_pair.1.value_at(14);
        assert!(mid > 0.1 && mid < 8.0);

        let default_pair = pairs.iter().find(|(f, _)| f.matches(&other, 14.0)).unwrap();
        assert!((default_pair.1.value_at(9) - 0.0).abs() < 1e-6);
        assert!((default_pair.1.value_at(20) - 3.0).abs() < 1e-6);
    }

    /// The four `cubic-bezier` control-point sets below are every distinct
    /// one used anywhere in `examples/mapbox-streets-style.json` (extracted
    /// by parsing the file, not guessed).
    const REAL_CUBIC_BEZIERS: [[f64; 4]; 4] =
        [[0.2, 0.0, 0.7, 1.0], [0.85, 0.7, 0.65, 1.0], [0.5, 0.0, 1.0, 1.0], [0.2, 0.0, 0.9, 1.0]];

    #[test]
    fn the_cubic_bezier_solver_is_well_behaved_on_every_curve_the_real_style_uses() {
        // A CSS-style unit cubic bezier has fixed endpoints P0 = (0,0) and
        // P3 = (1,1) and two declared control points, so for ANY valid
        // curve the eased fraction must pin to 0 and 1 at the ends and stay
        // monotonic in between. Asserting that for all four real curves is
        // what proves the parametric solve (find t with x(t) = u, then
        // evaluate y(t)) converges rather than, say, silently returning its
        // starting guess.
        for [x1, y1, x2, y2] in REAL_CUBIC_BEZIERS {
            let curve = Interpolation::CubicBezier { x1, y1, x2, y2 };
            let ease = |u: f64| curve.t_at(u, 0.0, 1.0);
            assert!(ease(0.0).abs() < 1e-6, "{x1},{y1},{x2},{y2}: t=0 must ease to 0, got {}", ease(0.0));
            assert!((ease(1.0) - 1.0).abs() < 1e-6, "{x1},{y1},{x2},{y2}: t=1 must ease to 1, got {}", ease(1.0));

            let mid = ease(0.5);
            assert!(mid > 0.0 && mid < 1.0, "{x1},{y1},{x2},{y2}: the midpoint must be a TRUE interior value, got {mid}");

            let mut previous = -1.0;
            for step in 0..=100 {
                let eased = ease(step as f64 / 100.0);
                assert!(eased >= previous - 1e-9, "{x1},{y1},{x2},{y2}: must be monotonic, {eased} followed {previous}");
                previous = eased;
            }
        }
    }

    #[test]
    fn the_cubic_bezier_solver_reproduces_independently_computed_values() {
        // Any curve whose control points lie ON the diagonal (x1 == y1 and
        // x2 == y2) is the identity easing -- x(t) and y(t) are the same
        // polynomial, so y must come back exactly equal to the input
        // fraction. This catches an x/y coefficient mix-up that the
        // monotonicity test above would happily accept.
        let identity = Interpolation::CubicBezier { x1: 0.2, y1: 0.2, x2: 0.7, y2: 0.7 };
        for step in 0..=20 {
            let u = step as f64 / 20.0;
            assert!((identity.t_at(u, 0.0, 1.0) - u).abs() < 1e-5, "identity curve must be y == x at {u}");
        }

        // Midpoint values for the four real curves, computed independently
        // (200-step bisection on x(t) in Python) rather than read back out
        // of this implementation.
        let expected = [0.549731, 0.493418, 0.278066, 0.456052];
        for ([x1, y1, x2, y2], want) in REAL_CUBIC_BEZIERS.into_iter().zip(expected) {
            let got = Interpolation::CubicBezier { x1, y1, x2, y2 }.t_at(0.5, 0.0, 1.0);
            assert!((got - want).abs() < 1e-5, "{x1},{y1},{x2},{y2} at u=0.5: got {got}, want {want}");
        }
    }

    #[test]
    fn compiles_a_cubic_bezier_interpolate_expression_end_to_end() {
        // The interpolation type itself, driving a plain numeric curve.
        // `["cubic-bezier", 0.2, 0, 0.7, 1]` is `country-label`'s real one;
        // its own stop outputs are `step` expressions (handled on the
        // layout side), so this exercises the type in isolation.
        let json: Value =
            serde_json::from_str(r#"["interpolate", ["cubic-bezier", 0.2, 0, 0.7, 1], ["zoom"], 1, 0.0, 9, 8.0]"#)
                .unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1);
        assert!((pairs[0].1.value_at(1) - 0.0).abs() < 1e-5);
        assert!((pairs[0].1.value_at(9) - 8.0).abs() < 1e-5);
        // z5 is the curve's midpoint (u = 0.5), eased to 0.549731, so the
        // value is 8 * 0.549731 = 4.39785 -- decisively NOT the linear 4.0,
        // which is what proves the declared easing is honoured.
        let mid = pairs[0].1.value_at(5);
        assert!((mid - 4.39785).abs() < 1e-3, "z5 must follow the bezier, got {mid}");
    }

    #[test]
    fn rejects_a_malformed_cubic_bezier_control_point_list() {
        // Four numeric control-point coordinates, no more and no fewer --
        // a short list must be a loud error, not a silent default curve.
        let json: Value =
            serde_json::from_str(r#"["interpolate", ["cubic-bezier", 0.2, 0], ["zoom"], 1, 0.0, 9, 8.0]"#).unwrap();
        assert!(compile_zoom_value(&json, number_leaf, Scope::root()).is_err());
    }

    #[test]
    fn rejects_step_segments_that_match_on_different_properties() {
        let json: Value = serde_json::from_str(
            r##"["step", ["zoom"],
                 ["match", ["get", "class"], "a", "#f00", "#000"],
                 10,
                 ["match", ["get", "structure"], "b", "#00f", "#000"]]"##,
        )
        .unwrap();
        assert!(compile_zoom_value(&json, color_leaf, Scope::root()).is_err());
    }

    fn only_ramp(src: &str) -> Option<(u8, u8)> {
        let json: Value = serde_json::from_str(src).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).unwrap();
        assert_eq!(pairs.len(), 1, "these fixtures are zoom-only, so they compile to one pair");
        pairs[0].1.linear_ramp()
    }

    /// `ZoomValue::linear_ramp` has to survive this module's RESAMPLING.
    /// A caller's two-stop `interpolate` does not reach it as two stops --
    /// `compile_interpolate` expands it to one stop per integer zoom -- so
    /// the only workable test of "can two endpoints reproduce this?" is a
    /// numeric one. This pins that the resampling and the test agree.
    #[test]
    fn a_resampled_linear_interpolate_is_recognised_as_one_straight_ramp() {
        assert_eq!(
            only_ramp(r#"["interpolate", ["linear"], ["zoom"], 8, 2.4, 18, 7.2]"#),
            Some((8, 18)),
            "the overlay's whole zoom-crossing fast path depends on this shape being recognised"
        );
    }

    /// The other half: a curve that ISN'T straight must be refused, or the
    /// overlay would bake its two endpoints and draw a line where the style
    /// asked for a curve. A cubic-bezier over this range departs from the
    /// chord by whole pixels, so nothing subtle is being relied on.
    #[test]
    fn a_resampled_cubic_bezier_is_not_mistaken_for_a_straight_ramp() {
        assert_eq!(
            only_ramp(r#"["interpolate", ["cubic-bezier", 0.2, 0, 0.8, 1], ["zoom"], 8, 2.4, 18, 7.2]"#),
            None
        );
    }

    /// A multi-segment `interpolate` is piecewise linear, not linear: no
    /// pair of endpoints reproduces it.
    #[test]
    fn a_bent_multi_stop_interpolate_is_not_a_straight_ramp() {
        assert_eq!(
            only_ramp(r#"["interpolate", ["linear"], ["zoom"], 8, 2.0, 12, 4.0, 18, 8.0]"#),
            None
        );
        // ...but one whose middle stop happens to sit ON the chord is
        // genuinely straight, and is recognised. The test is the geometry,
        // not the stop count.
        assert_eq!(
            only_ramp(r#"["interpolate", ["linear"], ["zoom"], 8, 2.0, 12, 4.4, 18, 8.0]"#),
            Some((8, 18))
        );
    }

    /// A `step` is a hard cutoff at each stop. Blending it into a ramp
    /// would be wrong even if its values happened to lie on a line, so the
    /// missing interpolator is what decides -- not the numbers.
    ///
    /// The `interpolate` line below is the control: the SAME zooms and
    /// values, differing only in the operator, are recognised as a ramp. So
    /// the `None` above is the step-ness being rejected and not the numbers
    /// failing the collinearity test by accident.
    #[test]
    fn a_step_is_never_a_ramp_even_with_collinear_stops() {
        assert_eq!(only_ramp(r#"["step", ["zoom"], 2.0, 12, 5.0, 16, 6.0]"#), None);
        assert_eq!(
            only_ramp(r#"["interpolate", ["linear"], ["zoom"], 0, 2.0, 12, 5.0, 16, 6.0]"#),
            Some((0, 16))
        );
    }

    // ---- the constant folder, as reached through `compile_zoom_value` ----

    #[test]
    fn a_let_bound_colour_compiles_to_one_unconditional_constant_pair() {
        let json: Value = serde_json::from_str(r#"["let", "l", 95, ["hsl", 20, 20, ["var", "l"]]]"#).unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1);
        assert!(matches!(pairs[0].0, Filter::Any));
        assert_eq!(pairs[0].1.value_at(0), parse_css_color("hsl(20, 20%, 95%)").unwrap());
        assert_eq!(pairs[0].1.value_at(22), parse_css_color("hsl(20, 20%, 95%)").unwrap());
    }

    /// The whole Mapbox Standard fill/background idiom, as a paint value:
    /// decompose a colour into `let`-bound h/s/l and recompose it. This is
    /// the shape 106 of the document's 110 `let`s have.
    #[test]
    fn the_colour_decomposition_pattern_compiles_as_a_paint_value() {
        let json: Value = serde_json::from_str(
            r#"["let",
                 "h_colorLand", ["at", 0, ["to-hsla", "hsl(20, 20%, 95%)"]],
                 "s_colorLand", ["at", 1, ["to-hsla", "hsl(20, 20%, 95%)"]],
                 "l_colorLand", ["at", 2, ["to-hsla", "hsl(20, 20%, 95%)"]],
                 ["hsl", ["var", "h_colorLand"], ["var", "s_colorLand"], ["var", "l_colorLand"]]]"#,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1);
        let folded = pairs[0].1.value_at(11);
        let direct = parse_css_color("hsl(20, 20%, 95%)").unwrap();
        assert!(
            (folded.r - direct.r).abs() < 1e-6 && (folded.g - direct.g).abs() < 1e-6 && (folded.b - direct.b).abs() < 1e-6,
            "folded {folded:?} != direct {direct:?}"
        );
    }

    #[test]
    fn a_let_bound_number_compiles_as_a_width() {
        let json: Value = serde_json::from_str(r#"["let", "w", 4, ["max", 1, ["-", ["var", "w"], 1.5]]]"#).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1.value_at(0), 2.5);
    }

    /// The `let` arm must not swallow the per-feature compiler: a `match` in
    /// the body still expands into one filtered pair per case, with the
    /// bindings visible inside each case's own expression.
    #[test]
    fn a_let_body_that_is_a_match_still_expands_into_one_pair_per_case() {
        let json: Value = serde_json::from_str(
            r#"["let", "l", 40,
                 ["match", ["get", "class"],
                   "park", ["hsl", 120, 50, ["var", "l"]],
                   ["hsl", 0, 0, ["var", "l"]]]]"#,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 2, "one pair for the case, one for the default");
        assert_eq!(pairs[0].1.value_at(0), parse_css_color("hsl(120, 50%, 40%)").unwrap());
        assert_eq!(pairs[1].1.value_at(0), parse_css_color("hsl(0, 0%, 40%)").unwrap());
    }

    /// `["literal", …]`'s payload is DATA -- `eval_inner` hands it back
    /// untouched -- so a `let` body carrying a literal array whose first
    /// element happens to be the string `"var"` must come through with that
    /// array intact, not rewritten into whatever `var` was bound to. Same
    /// rule as the `match`-label guard beside it.
    #[test]
    fn substitute_vars_leaves_a_literals_payload_alone() {
        let expr: Value = serde_json::from_str(r#"["literal", [["var", "x"], "plain"]]"#).unwrap();
        let bindings = vec![("x".to_string(), serde_json::json!("SUBSTITUTED"))];
        assert_eq!(substitute_vars(&expr, &bindings), expr, "a literal's payload is data, not an expression");
    }

    #[test]
    fn a_feature_dependent_let_binding_inlines_into_a_filtered_match() {
        // `roads`' real `isHighClass` shape (see `compile_let`'s doc
        // comment): a `let` binds a boolean `match` on a feature property,
        // then a `case` branches on it. This USED to be refused outright
        // (`let_bindings` categorically rejected any feature-dependent
        // binding); `compile_let` now inlines it instead, turning the
        // `case` into an ordinary feature-dependent expression that
        // `compile_zoom_value`'s own `case`/`match` machinery expands into
        // real filtered cases -- exactly like a bare
        // `["match", ["get", "class"], ...]` already does.
        let json: Value = serde_json::from_str(
            r##"["let", "isHighClass", ["match", ["get", "class"], ["motorway", "trunk"], true, false],
                 ["case", ["var", "isHighClass"], "#ff0000", "#00ff00"]]"##,
        )
        .unwrap();
        let cases = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(cases.len(), 2, "one case per case-branch, filtered by class: {cases:?}");
        let red = crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let green = crate::style::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
        let matched = cases.iter().find_map(|(filter, value)| match filter {
            Filter::In { key, values } if key == "class" && values == &["motorway".to_string(), "trunk".to_string()] => {
                Some(value.value_at(0))
            }
            _ => None,
        });
        assert_eq!(matched, Some(red), "motorway/trunk takes the case's TRUE branch: {cases:?}");
        let fallback = cases.iter().find_map(|(filter, value)| match filter {
            Filter::Not(inner) => match inner.as_ref() {
                Filter::In { key, values } if key == "class" && values == &["motorway".to_string(), "trunk".to_string()] => {
                    Some(value.value_at(0))
                }
                _ => None,
            },
            _ => None,
        });
        assert_eq!(fallback, Some(green), "everything else takes the case's FALSE branch: {cases:?}");
    }

    #[test]
    fn hoist_lifts_a_feature_dependent_match_out_of_arithmetic() {
        // A `match` on a feature property, nested one level inside a `+`
        // that isn't itself in `FOLDED_OPERATORS`' interpolate/match/case/
        // step/let special-cased set -- exactly the shape a feature-
        // dependent `let` binding inlines into when the BOUND value (not
        // just a `case`'s condition) is itself the feature-dependent
        // `match`. `hoist_feature_dependent_case`'s `"match"` arm handles
        // this; its `"case"` arm (the one `roads`' real shape exercises)
        // is covered by `the_real_roads_line_emissive_strength_resolves_by_class`
        // in `style_spec::tests`.
        let json: Value =
            serde_json::from_str(r#"["+", 10, ["match", ["get", "class"], "a", 1, "b", 2, 3]]"#).unwrap();
        let cases = compile_zoom_value(&json, number_leaf, Scope::root()).expect("must compile");
        assert_eq!(cases.len(), 3, "one case per match label, plus the default: {cases:?}");
        fn contains_in(filter: &Filter, key: &str, want_values: &[&str]) -> bool {
            match filter {
                Filter::In { key: k, values } => k == key && values.iter().map(String::as_str).eq(want_values.iter().copied()),
                Filter::All(inner) | Filter::AnyOf(inner) => inner.iter().any(|f| contains_in(f, key, want_values)),
                Filter::Not(inner) => contains_in(inner, key, want_values),
                _ => false,
            }
        }
        fn contains_not(filter: &Filter) -> bool {
            match filter {
                Filter::Not(_) => true,
                Filter::All(inner) => inner.iter().any(contains_not),
                _ => false,
            }
        }
        let value_for = |key: &str, want_values: &[&str]| {
            cases
                .iter()
                .find_map(|(filter, value)| contains_in(filter, key, want_values).then(|| value.value_at(0)))
                .unwrap_or_else(|| panic!("no case matched key {key:?} values {want_values:?}: {cases:?}"))
        };
        assert_eq!(value_for("class", &["a"]), 11.0);
        assert_eq!(value_for("class", &["b"]), 12.0);
        let default = cases
            .iter()
            .find_map(|(filter, value)| contains_not(filter).then(|| value.value_at(0)))
            .unwrap_or_else(|| panic!("no default (Not) case: {cases:?}"));
        assert_eq!(default, 13.0);
    }

    #[test]
    fn a_folded_expression_that_reads_the_zoom_is_resampled_at_every_integer_zoom() {
        let json: Value = serde_json::from_str(r#"["+", ["zoom"], 100]"#).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1.stops.len(), 23, "one stop per integer zoom in 0..=22");
        for zoom in 0..=22u8 {
            assert_eq!(pairs[0].1.value_at(zoom), 100.0 + zoom as f32, "at zoom {zoom}");
        }
    }

    /// A binding can be what reads the zoom, with the body never mentioning
    /// it -- `var` substitution is lazy, so the body alone does not say.
    #[test]
    fn a_let_binding_that_reads_the_zoom_still_resamples() {
        let json: Value = serde_json::from_str(r#"["let", "z", ["zoom"], ["*", ["var", "z"], 2]]"#).unwrap();
        let pairs = compile_zoom_value(&json, number_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs[0].1.stops.len(), 23);
        assert_eq!(pairs[0].1.value_at(9), 18.0);
    }

    /// The folder must not claim shapes that already have an owner. A bare
    /// numeric array and a `["literal", [...]]` are `line-dasharray`
    /// values, and both compiled to a single constant pair before the
    /// folder existed.
    #[test]
    fn dasharray_shapes_are_untouched_by_the_folder() {
        let bare: Value = serde_json::from_str("[3, 3]").unwrap();
        let pairs = compile_zoom_value(&bare, dash_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1.stops.len(), 1);
        assert_eq!(pairs[0].1.value_at(0), DashPattern::from_runs(&[3.0, 3.0]));

        let wrapped: Value = serde_json::from_str(r#"["literal", [2, 2, 6, 2]]"#).unwrap();
        let pairs = compile_zoom_value(&wrapped, dash_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs[0].1.value_at(0), DashPattern::from_runs(&[2.0, 2.0, 6.0, 2.0]));
    }

    /// An expression the folder does not recognize keeps today's behaviour:
    /// a plain error that drops the layer, NOT a degradation.
    #[test]
    fn an_unrecognized_expression_is_still_a_plain_error() {
        let json: Value = serde_json::from_str(r#"["get", "min_depth"]"#).unwrap();
        let error = compile_zoom_value(&json, color_leaf, Scope::root()).expect_err("must fail");
        assert!(!is_unfoldable(&error), "an unknown construct must still drop the layer: {error:#}");
    }

    /// ...whereas a shape the folder DOES recognize and cannot reduce is
    /// degradable, so the property falls back and the layer survives.
    /// `Scope::root()` carries no config, so `["config", ...]` is exactly
    /// this: a recognized construct the folder cannot resolve.
    #[test]
    fn a_recognized_but_unfoldable_expression_is_degradable() {
        let json: Value = serde_json::from_str(r#"["at", 0, ["to-hsla", ["config", "colorLand"]]]"#).unwrap();
        let error =
            compile_zoom_value(&json, color_leaf, Scope::root()).expect_err("must fail: no config resolved");
        assert!(is_unfoldable(&error), "{error:#}");
    }

    /// A BARE `["config", key]` -- no enclosing `at`/`hsl`/arithmetic that
    /// would already be a recognized operator -- must reach the folder too,
    /// not fall through to `parse_leaf` and hard-fail the layer. This is
    /// exactly `waterway`'s real `line-color` in the vendored Mapbox
    /// Standard fixture (`imports[0].data.layers`), one of 11 real layers
    /// using this shape; regression test for `FOLDED_OPERATORS` missing
    /// `"config"`, which made `compile_zoom_value(["config", "colorWater"],
    /// color_leaf, ...)` return a plain (non-`Unfoldable`) error even with a
    /// config that resolves the key, dropping the whole layer via
    /// `try_or_skip!` instead of folding.
    #[test]
    fn a_bare_top_level_config_reference_folds_not_just_a_nested_one() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();
        let imports = crate::style_spec::config::parse_imports(&root).unwrap();
        let import = &imports[0];

        let waterway = import.data["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l.get("id").and_then(Value::as_str) == Some("waterway"))
            .expect("the fixture has a waterway layer");
        let line_color = &waterway["paint"]["line-color"];
        assert_eq!(line_color, &serde_json::json!(["config", "colorWater"]), "fixture shape moved out from under this test");

        let scope = Scope::root().with_config(&import.config);
        let pairs = compile_zoom_value(line_color, color_leaf, scope)
            .unwrap_or_else(|e| panic!("a bare top-level [\"config\", k] must fold, not drop the layer: {e:#}"));
        assert_eq!(pairs.len(), 1);
        assert!(matches!(pairs[0].0, Filter::Any));
        // colorWater is one of the fixture's 20 overridden keys: "hsl(0, 0%, 76%)".
        let expected = parse_css_color("hsl(0, 0%, 76%)").unwrap();
        assert_eq!(pairs[0].1.value_at(0), expected);
    }

    /// Same shape, minimal and synthetic, so the regression is pinned down
    /// even if the fixture ever changes `waterway`'s paint.
    #[test]
    fn a_bare_config_reference_folds_with_a_synthetic_resolved_config() {
        let resolved =
            crate::style_spec::config::ResolvedConfig::from_pairs([("colorWater", serde_json::json!("#336699"))]);
        let json: Value = serde_json::from_str(r#"["config", "colorWater"]"#).unwrap();
        let scope = Scope::root().with_config(&resolved);
        let pairs = compile_zoom_value(&json, color_leaf, scope)
            .unwrap_or_else(|e| panic!("must fold: {e:#}"));
        assert_eq!(pairs[0].1.value_at(0), parse_css_color("#336699").unwrap());
    }

    /// The Standard `landcover` layer guards a branch with `["==", ["var",
    /// "s_colorLand"], 0]` -- a condition made entirely of `let` bindings,
    /// which has one answer for the whole style. `compile_case_condition`
    /// has no filter for it and never could; the folder does.
    #[test]
    fn a_case_condition_that_folds_to_a_constant_selects_its_arm_outright() {
        let json: Value = serde_json::from_str(
            r##"["let", "s", 0,
                 ["case", ["==", ["var", "s"], 0], "#111111", "#222222"]]"##,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1, "a constant condition leaves no branch to filter on");
        assert!(matches!(pairs[0].0, Filter::Any));
        assert_eq!(pairs[0].1.value_at(0), parse_css_color("#111111").unwrap());
    }

    #[test]
    fn a_case_condition_that_folds_to_false_drops_its_arm() {
        let json: Value = serde_json::from_str(
            r##"["let", "s", 40,
                 ["case", ["==", ["var", "s"], 0], "#111111", "#222222"]]"##,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1.value_at(0), parse_css_color("#222222").unwrap());
    }

    /// A constant arm must not swallow the feature-dependent arms around it.
    #[test]
    fn a_constant_case_arm_still_carries_the_earlier_arms_negations() {
        let json: Value = serde_json::from_str(
            r##"["case", ["has", "tunnel"], "#111111", ["==", 1, 1], "#222222", "#333333"]"##,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 2, "the always-true arm ends the chain, so the fallback is unreachable");
        assert_eq!(pairs[0].1.value_at(0), parse_css_color("#111111").unwrap());
        assert!(matches!(&pairs[0].0, Filter::Has(key) if key == "tunnel"), "{:?}", pairs[0].0);
        assert_eq!(pairs[1].1.value_at(0), parse_css_color("#222222").unwrap());
        assert!(
            matches!(&pairs[1].0, Filter::Not(inner) if matches!(&**inner, Filter::Has(key) if key == "tunnel")),
            "the always-true arm is still guarded by NOT has(tunnel): {:?}",
            pairs[1].0
        );
    }

    /// `match` on something that is not a feature lookup: 582 of the
    /// Standard document's `match`es switch on style configuration, which
    /// has one answer for the whole style.
    #[test]
    fn a_match_on_a_constant_input_selects_its_branch_outright() {
        let json: Value = serde_json::from_str(
            r##"["let", "preset", "night",
                 ["match", ["var", "preset"], "day", "#ffffff", ["dusk", "night"], "#000000", "#888888"]]"##,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1);
        assert!(matches!(pairs[0].0, Filter::Any));
        assert_eq!(pairs[0].1.value_at(0), parse_css_color("#000000").unwrap());
    }

    #[test]
    fn a_match_on_a_constant_input_falls_through_to_its_default() {
        let json: Value = serde_json::from_str(
            r##"["match", "dawn", "day", "#ffffff", "night", "#000000", "#888888"]"##,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1.value_at(0), parse_css_color("#888888").unwrap());
    }

    /// The selected branch is compiled, not folded: a `match` on a constant
    /// whose chosen branch varies per feature still expands.
    #[test]
    fn the_branch_a_constant_match_selects_is_still_compiled_normally() {
        let json: Value = serde_json::from_str(
            r##"["match", "night",
                 "night", ["match", ["get", "class"], "park", "#00ff00", "#0000ff"],
                 "#888888"]"##,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 2, "the inner feature match still expands");
    }

    /// `match` on a feature lookup keeps its existing expansion -- the
    /// constant path must not capture it.
    #[test]
    fn a_match_on_a_feature_lookup_is_untouched_by_the_folder() {
        let json: Value = serde_json::from_str(
            r##"["match", ["get", "class"], "park", "#00ff00", "#0000ff"]"##,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, Scope::root()).expect("must compile");
        assert_eq!(pairs.len(), 2);
    }
}

#[cfg(test)]
mod eval_tests {
    use super::*;
    use crate::style_spec::color::parse_css_color;

    /// `eval` at zoom 0 in the empty scope -- the shape almost every test
    /// below wants.
    fn fold(src: &str) -> Result<EvalValue> {
        let json: Value = serde_json::from_str(src).expect("test expression must be valid JSON");
        eval(&json, Scope::root(), 0)
    }

    fn number(src: &str) -> f64 {
        match fold(src).unwrap_or_else(|e| panic!("{src} should fold: {e:#}")) {
            EvalValue::Number(n) => n,
            other => panic!("{src} folded to {other:?}, not a number"),
        }
    }

    fn color(src: &str) -> crate::style::Rgba {
        match fold(src).unwrap_or_else(|e| panic!("{src} should fold: {e:#}")) {
            EvalValue::Color(c) => c,
            other => panic!("{src} folded to {other:?}, not a colour"),
        }
    }

    // ---- literals -------------------------------------------------------

    #[test]
    fn a_bare_json_literal_folds_to_itself() {
        assert_eq!(fold("7.5").unwrap(), EvalValue::Number(7.5));
        assert_eq!(fold(r#""hello""#).unwrap(), EvalValue::String("hello".into()));
        assert_eq!(fold("true").unwrap(), EvalValue::Bool(true));
        assert_eq!(fold("null").unwrap(), EvalValue::Null);
    }

    #[test]
    fn literal_wraps_an_array_without_evaluating_it() {
        assert_eq!(
            fold(r#"["literal", [1, "two", false]]"#).unwrap(),
            EvalValue::Array(vec![EvalValue::Number(1.0), EvalValue::String("two".into()), EvalValue::Bool(false)])
        );
    }

    #[test]
    fn a_bare_array_is_not_an_expression() {
        // Without the `literal` wrapper an array's first element has to be
        // an operator name; `[1, 2]` is the `line-dasharray` shape, which
        // has its own leaf parser and must never reach the evaluator.
        assert!(fold("[1, 2]").is_err());
    }

    // ---- arithmetic -----------------------------------------------------

    #[test]
    fn arithmetic_folds_to_its_value() {
        assert_eq!(number(r#"["+", 2, 3]"#), 5.0);
        assert_eq!(number(r#"["+", 1, 2, 3, 4]"#), 10.0);
        assert_eq!(number(r#"["-", 10, 4]"#), 6.0);
        assert_eq!(number(r#"["-", 4]"#), -4.0);
        assert_eq!(number(r#"["*", 3, 4]"#), 12.0);
        assert_eq!(number(r#"["*", 2, 3, 4]"#), 24.0);
        assert_eq!(number(r#"["/", 9, 2]"#), 4.5);
        assert_eq!(number(r#"["min", 3, 1, 2]"#), 1.0);
        assert_eq!(number(r#"["max", 0, -5]"#), 0.0);
        assert_eq!(number(r#"["abs", -7.25]"#), 7.25);
        assert_eq!(number(r#"["%", 7, 4]"#), 3.0);
    }

    #[test]
    fn the_lightness_clamp_the_standard_style_actually_writes() {
        // ["max", 0, ["-", <lightness>, 55]] -- the darkening idiom the
        // design doc quotes, with the lightness of hsl(0, 0%, 76%) (76, on
        // the 0-100 scale `to-hsla` reports) standing in for the config.
        assert_eq!(number(r#"["max", 0, ["-", 76, 55]]"#), 21.0);
        assert_eq!(number(r#"["max", 0, ["-", 30, 55]]"#), 0.0);
        assert_eq!(number(r#"["min", 100, ["+", 95, 10]]"#), 100.0);
    }

    #[test]
    fn arithmetic_on_a_non_number_is_an_error() {
        assert!(fold(r#"["+", 1, "two"]"#).is_err());
        assert!(fold(r#"["abs", true]"#).is_err());
        assert!(fold(r#"["-", 1, 2, 3]"#).is_err());
    }

    // ---- coercions ------------------------------------------------------

    #[test]
    fn to_number_coerces_the_shapes_the_style_spec_says_it_does() {
        assert_eq!(number(r#"["to-number", "42.5"]"#), 42.5);
        assert_eq!(number(r#"["to-number", true]"#), 1.0);
        assert_eq!(number(r#"["to-number", false]"#), 0.0);
        assert_eq!(number(r#"["to-number", null]"#), 0.0);
        assert_eq!(number(r#"["to-number", 3]"#), 3.0);
        // Multi-argument form: the first argument that converts wins.
        assert_eq!(number(r#"["to-number", "nonsense", "8"]"#), 8.0);
        assert!(fold(r#"["to-number", "nonsense"]"#).is_err());
    }

    #[test]
    fn to_boolean_uses_the_style_specs_falsy_set() {
        for (src, expected) in [
            (r#"["to-boolean", 1]"#, true),
            (r#"["to-boolean", 0]"#, false),
            (r#"["to-boolean", ""]"#, false),
            (r#"["to-boolean", "no"]"#, true),
            (r#"["to-boolean", null]"#, false),
            (r#"["to-boolean", false]"#, false),
            (r#"["to-boolean", true]"#, true),
        ] {
            assert_eq!(fold(src).unwrap(), EvalValue::Bool(expected), "{src}");
        }
    }

    #[test]
    fn to_string_renders_each_value_kind() {
        for (src, expected) in [
            (r#"["to-string", null]"#, ""),
            (r#"["to-string", true]"#, "true"),
            (r#"["to-string", 2.5]"#, "2.5"),
            (r#"["to-string", 3]"#, "3"),
            (r#"["to-string", "already"]"#, "already"),
        ] {
            assert_eq!(fold(src).unwrap(), EvalValue::String(expected.into()), "{src}");
        }
    }

    // ---- at -------------------------------------------------------------

    #[test]
    fn at_indexes_an_array() {
        assert_eq!(fold(r#"["at", 1, ["literal", [10, 20, 30]]]"#).unwrap(), EvalValue::Number(20.0));
    }

    #[test]
    fn at_out_of_range_is_an_error() {
        assert!(fold(r#"["at", 3, ["literal", [10, 20, 30]]]"#).is_err());
        assert!(fold(r#"["at", -1, ["literal", [10, 20, 30]]]"#).is_err());
        assert!(fold(r#"["at", 0.5, ["literal", [10, 20, 30]]]"#).is_err());
        assert!(fold(r#"["at", 0, 7]"#).is_err());
    }

    // ---- to-hsla / hsl / hsla -------------------------------------------

    /// The units are not guessable: the vendored Standard document writes
    /// `["max", 0, ["-", ["at", 2, ["to-hsla", C]], 55]]` (lightness on a
    /// 0-100 scale), `["min", 100, ["+", ["at", 2, ...], 2]]` (same),
    /// `["max", 0, ["-", ["at", 1, ...], 25]]` (saturation, same scale),
    /// `["max", 0, ["-", ["at", 0, ...], 20]]` (hue, degrees) and
    /// `["-", ["at", 3, ...], 0.2]` (alpha, 0-1).
    #[test]
    fn to_hsla_reports_degrees_percent_percent_and_unit_alpha() {
        let hsla = fold(r#"["to-hsla", "hsla(210, 40%, 30%, 0.5)"]"#).unwrap();
        let EvalValue::Array(parts) = hsla else { panic!("to-hsla must produce an array, got {hsla:?}") };
        assert_eq!(parts.len(), 4);
        let n = |i: usize| match &parts[i] {
            EvalValue::Number(v) => *v,
            other => panic!("component {i} is {other:?}, not a number"),
        };
        assert!((n(0) - 210.0).abs() < 1e-2, "hue {} should be in degrees", n(0));
        assert!((n(1) - 40.0).abs() < 1e-2, "saturation {} should be 0-100", n(1));
        assert!((n(2) - 30.0).abs() < 1e-2, "lightness {} should be 0-100", n(2));
        assert!((n(3) - 0.5).abs() < 1e-6, "alpha {} should be 0-1", n(3));
    }

    #[test]
    fn to_hsla_of_a_grey_reports_zero_hue_and_saturation() {
        let hsla = fold(r#"["to-hsla", "hsl(0, 0%, 76%)"]"#).unwrap();
        let EvalValue::Array(parts) = hsla else { panic!("expected an array") };
        assert_eq!(parts[0], EvalValue::Number(0.0));
        assert_eq!(parts[1], EvalValue::Number(0.0));
        match parts[2] {
            EvalValue::Number(l) => assert!((l - 76.0).abs() < 1e-2, "lightness {l}"),
            ref other => panic!("{other:?}"),
        }
        assert_eq!(parts[3], EvalValue::Number(1.0));
    }

    #[test]
    fn hsl_and_hsla_build_the_same_colour_the_css_parser_does() {
        let built = color(r#"["hsl", 20, 20, 95]"#);
        let parsed = parse_css_color("hsl(20, 20%, 95%)").unwrap();
        assert_eq!(built, parsed);

        let built = color(r#"["hsla", 210, 40, 30, 0.5]"#);
        let parsed = parse_css_color("hsla(210, 40%, 30%, 0.5)").unwrap();
        assert_eq!(built, parsed);
    }

    #[test]
    fn hsl_rejects_the_wrong_arity_and_non_numeric_components() {
        assert!(fold(r#"["hsl", 20, 20]"#).is_err());
        assert!(fold(r#"["hsl", 20, 20, 95, 1]"#).is_err());
        assert!(fold(r#"["hsla", 20, 20, 95]"#).is_err());
        assert!(fold(r#"["hsl", "20", 20, 95]"#).is_err());
    }

    // ---- let / var ------------------------------------------------------

    #[test]
    fn let_binds_a_name_that_var_reads() {
        assert_eq!(number(r#"["let", "x", 4, ["*", ["var", "x"], 3]]"#), 12.0);
    }

    #[test]
    fn a_binding_can_reference_an_earlier_binding_in_the_same_let() {
        assert_eq!(number(r#"["let", "a", 2, "b", ["+", ["var", "a"], 3], ["*", ["var", "b"], 10]]"#), 50.0);
    }

    #[test]
    fn a_later_binding_shadows_an_earlier_one_in_declaration_order() {
        // The second `x` shadows the first for the body, and its own value
        // still sees the FIRST -- lexical, declaration-ordered scoping. If
        // `var` resolved in the body's scope this would recurse forever.
        assert_eq!(number(r#"["let", "x", 1, "x", ["+", ["var", "x"], 10], ["var", "x"]]"#), 11.0);
    }

    #[test]
    fn a_nested_let_shadows_the_outer_binding_and_the_outer_one_comes_back() {
        assert_eq!(
            number(r#"["let", "x", 1, ["+", ["let", "x", 100, ["var", "x"]], ["var", "x"]]]"#),
            101.0
        );
    }

    #[test]
    fn an_unbound_var_is_an_error_naming_the_binding() {
        let error = fold(r#"["var", "missing"]"#).unwrap_err();
        assert!(format!("{error:#}").contains("missing"), "{error:#}");
    }

    #[test]
    fn a_malformed_let_is_an_error() {
        assert!(fold(r#"["let", "x", 1]"#).is_err());
        assert!(fold(r#"["let", "x", 1, "y", ["var", "x"]]"#).is_err());
        assert!(fold(r#"["let", 1, 1, ["var", "x"]]"#).is_err());
    }

    /// 106 of the Standard document's 110 `let`s are this exact shape. A
    /// literal colour stands in for the `["config", ...]` this commit
    /// cannot yet resolve; the decomposition and recomposition around it
    /// are byte-for-byte what the real style writes.
    #[test]
    fn the_colour_decomposition_pattern_folds_back_to_the_colour_it_started_from() {
        let source = r#"
            ["let",
              "h_colorLand", ["at", 0, ["to-hsla", "hsl(20, 20%, 95%)"]],
              "s_colorLand", ["at", 1, ["to-hsla", "hsl(20, 20%, 95%)"]],
              "l_colorLand", ["at", 2, ["to-hsla", "hsl(20, 20%, 95%)"]],
              ["hsl", ["var", "h_colorLand"], ["var", "s_colorLand"], ["var", "l_colorLand"]]]
        "#;
        let folded = color(source);
        let direct = parse_css_color("hsl(20, 20%, 95%)").unwrap();
        assert!(
            (folded.r - direct.r).abs() < 1e-6
                && (folded.g - direct.g).abs() < 1e-6
                && (folded.b - direct.b).abs() < 1e-6
                && (folded.a - direct.a).abs() < 1e-6,
            "folded {folded:?} != direct {direct:?}"
        );
    }

    /// The same pattern with the darkening the Standard water layer applies
    /// to its lightness -- the reason `to-hsla`'s units have to be right.
    #[test]
    fn the_decomposition_pattern_with_a_lightness_adjustment() {
        let source = r#"
            ["let",
              "h", ["at", 0, ["to-hsla", "hsl(0, 0%, 76%)"]],
              "s", ["at", 1, ["to-hsla", "hsl(0, 0%, 76%)"]],
              "l", ["at", 2, ["to-hsla", "hsl(0, 0%, 76%)"]],
              ["hsl", ["var", "h"], ["var", "s"], ["max", 0, ["-", ["var", "l"], 55]]]]
        "#;
        let folded = color(source);
        // 76 - 55 = 21% lightness, still a grey.
        let expected = parse_css_color("hsl(0, 0%, 21%)").unwrap();
        assert!((folded.r - expected.r).abs() < 1e-4, "{folded:?} vs {expected:?}");
        assert_eq!(folded.r, folded.g);
        assert_eq!(folded.g, folded.b);
    }

    // ---- feature dependence ---------------------------------------------

    #[test]
    fn a_feature_lookup_cannot_be_folded() {
        for src in [
            r#"["get", "class"]"#,
            r#"["has", "class"]"#,
            r#"["geometry-type"]"#,
            r#"["id"]"#,
            r#"["properties"]"#,
            // `["feature-state", k]` is deliberately absent: it does not
            // read the feature, and it DOES fold. See the three tests
            // below.
        ] {
            assert!(fold(src).is_err(), "{src} must not fold");
        }
    }

    // ---- feature-state ----------------------------------------------------

    /// The whole basis of the fold, transcribed from Mapbox GL JS 3.15.0:
    /// `'feature-state'` evaluates to `get(key, ctx.featureState || {})`,
    /// and that `get` returns `null` for a key that is `undefined`
    /// (`src/style-spec/expression/definitions/index.ts`). This crate has
    /// no `setFeatureState`, so every key is unset and every lookup is
    /// that `null` -- not a refusal, and not `false`.
    #[test]
    fn an_unset_feature_state_folds_to_null_because_that_is_what_the_reference_returns() {
        assert_eq!(fold(r#"["feature-state", "hover"]"#).unwrap(), EvalValue::Null);
        assert_eq!(fold(r#"["feature-state", "select"]"#).unwrap(), EvalValue::Null);
        // A key nothing in any style ever writes folds the same way: there
        // is no set of "known" keys here, because none of them can be set.
        assert_eq!(fold(r#"["feature-state", "no-style-writes-this"]"#).unwrap(), EvalValue::Null);
    }

    /// `["to-boolean", null]` is `Boolean(null)` in the reference
    /// (`definitions/coercion.ts`), i.e. `false` -- which is the coercion
    /// every Mapbox Standard layer actually writes around its
    /// `feature-state` read.
    #[test]
    fn to_boolean_of_an_unset_feature_state_is_false() {
        assert_eq!(fold(r#"["to-boolean", ["feature-state", "select"]]"#).unwrap(), EvalValue::Bool(false));
        assert_eq!(fold(r#"["to-boolean", ["feature-state", "highlight"]]"#).unwrap(), EvalValue::Bool(false));
    }

    /// The shape verbatim from Mapbox Standard's `building-underground`
    /// `fill-color`, `2d-building`'s and `3d-building`'s
    /// `fill-extrusion-color`, and eleven label layers' `text-color`: two
    /// state-gated arms and the colour the author wrote for the ordinary,
    /// nothing-selected case. Folding the gates picks that last one --
    /// which is the point. Before this, the unfoldable gate took the whole
    /// LAYER down and the else branch was never reached.
    #[test]
    fn the_standard_select_highlight_case_folds_to_its_else_branch() {
        let folded = fold(
            r##"["case",
                 ["to-boolean", ["feature-state", "select"]], "#ff0000",
                 ["to-boolean", ["feature-state", "highlight"]], "#00ff00",
                 "#0000ff"]"##,
        )
        .unwrap();
        assert_eq!(folded, EvalValue::String("#0000ff".into()), "the else branch, not a state-gated arm");
    }

    /// A `feature-state` key must still be a string, as the reference's
    /// `[StringType]` signature says. A malformed one is an ordinary
    /// `Unfoldable`, so it degrades the property rather than pretending the
    /// state is unset.
    #[test]
    fn a_feature_state_key_that_is_not_a_string_does_not_fold() {
        for src in [r#"["feature-state", 3]"#, r#"["feature-state"]"#, r#"["feature-state", "a", "b"]"#] {
            let error = fold(src).unwrap_err();
            assert!(is_unfoldable(&error), "{src}: {error:#}");
        }
    }

    // ---- per-feature: Standard's building height ---------------------------

    fn feature(pairs: &[(&str, crate::mvt::TagValue)]) -> std::collections::HashMap<String, crate::mvt::TagValue> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    /// The vendored `3d-building` height, verbatim from
    /// `examples/mapbox-standard-dark-style.json` (`imports/data`).
    const STANDARD_HEIGHT: &str = r#"["coalesce", ["get","est_height"], ["case",
        ["to-boolean",["feature-state","select"]], ["+",["number",["get","height"]],1],
        ["to-boolean",["feature-state","highlight"]], ["+",["number",["get","height"]],1],
        ["number",["get","height"]]]]"#;
    const STANDARD_BASE: &str = r#"["case", [">=",["number",["get","min_height"]],0], ["get","min_height"], 0]"#;

    fn json(src: &str) -> Value {
        serde_json::from_str(src).expect("test expression must be valid JSON")
    }

    #[test]
    fn number_returns_its_first_numeric_argument() {
        use crate::mvt::TagValue;
        let tags = feature(&[("h", TagValue::Number(42.0))]);
        assert_eq!(eval_number_for_feature(&json(r#"["number",["get","h"]]"#), &tags, 15), Some(42.0));
        assert_eq!(eval_number_for_feature(&json(r#"["number",["get","missing"],7]"#), &tags, 15), Some(7.0));
    }

    #[test]
    fn number_of_a_non_number_is_a_runtime_error() {
        use crate::mvt::TagValue;
        let tags = feature(&[("h", TagValue::String("12".into()))]);
        // MapLibre's `number` assertion throws on a string, and the caller
        // falls back to the property default.
        assert_eq!(eval_number_for_feature(&json(r#"["number",["get","h"]]"#), &tags, 15), None);
        assert_eq!(eval_number_for_feature(&json(r#"["number",["get","missing"]]"#), &tags, 15), None);
    }

    #[test]
    fn standards_building_height_is_per_feature_evaluable() {
        assert!(is_per_feature_evaluable(&json(STANDARD_HEIGHT)));
        assert!(is_per_feature_evaluable(&json(STANDARD_BASE)));
    }

    #[test]
    fn standards_height_takes_the_unselected_branch() {
        use crate::mvt::TagValue;
        let plain = feature(&[("height", TagValue::Number(120.0))]);
        assert_eq!(eval_number_for_feature(&json(STANDARD_HEIGHT), &plain, 15), Some(120.0));
        let estimated = feature(&[("height", TagValue::Number(120.0)), ("est_height", TagValue::Number(50.0))]);
        assert_eq!(eval_number_for_feature(&json(STANDARD_HEIGHT), &estimated, 15), Some(50.0));
    }

    #[test]
    fn standards_base_reads_min_height_and_errors_without_it() {
        use crate::mvt::TagValue;
        let with = feature(&[("min_height", TagValue::Number(10.0))]);
        assert_eq!(eval_number_for_feature(&json(STANDARD_BASE), &with, 15), Some(10.0));
        // ["number", null] throws in MapLibre, so the whole `case` errors and
        // the property takes its default (0) -- in the caller, not here.
        assert_eq!(eval_number_for_feature(&json(STANDARD_BASE), &feature(&[]), 15), None);
    }

    #[test]
    fn feature_state_is_null_per_feature_too() {
        let tags = feature(&[]);
        let expr = json(r#"["case", ["to-boolean",["feature-state","select"]], 1, 2]"#);
        assert!(is_per_feature_evaluable(&expr));
        assert_eq!(eval_number_for_feature(&expr, &tags, 15), Some(2.0));
    }

    // ---- measure-light ----------------------------------------------------

    /// `Scope::root()` carries no lights, so `["measure-light", ...]`
    /// degrades exactly like an unresolvable `["config", k]` does -- one
    /// unhandled form must not take a layer down.
    #[test]
    fn measure_light_with_no_lights_on_the_scope_does_not_fold() {
        let error = fold(r#"["measure-light", "brightness"]"#).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
    }

    /// A minimal synthetic `lights` array -- ambient white at intensity
    /// 0.5, directional BLACK -- pins `relative_luminance(color) *
    /// intensity`, averaged with a directional contribution that drops out
    /// (`relative_luminance(black) == 0`, regardless of its own intensity
    /// or direction), independent of the real fixture.
    #[test]
    fn measure_light_brightness_is_relative_luminance_times_intensity_averaged_with_directional() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "#ffffff", "intensity": 0.5 } },
            { "id": "directional", "properties": {
                "color": "#000000", "intensity": 0.9, "direction": ["literal", [0, 0]]
            } }
        ]);
        let json: Value = serde_json::from_str(r#"["measure-light", "brightness"]"#).unwrap();
        let scope = Scope::root().with_lights(&lights);
        // relative_luminance(white) == 1 exactly (every channel is exactly
        // 1.0, where the W3C curve and a plain dot product agree), so
        // ambient_brightness == 0.5; directional_brightness == 0 (black);
        // brightness == (0 + 0.5) / 2 == 0.25.
        assert_eq!(eval(&json, scope, 0).unwrap(), EvalValue::Number(0.25));
    }

    #[test]
    fn measure_light_of_black_at_any_intensity_is_zero() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "#000000", "intensity": 0.9 } },
            { "id": "directional", "properties": {
                "color": "#000000", "intensity": 0.9, "direction": ["literal", [0, 0]]
            } }
        ]);
        let json: Value = serde_json::from_str(r#"["measure-light", "brightness"]"#).unwrap();
        let scope = Scope::root().with_lights(&lights);
        assert_eq!(eval(&json, scope, 0).unwrap(), EvalValue::Number(0.0));
    }

    /// The `color`/`intensity` expressions are ordinary expressions in this
    /// vocabulary -- including `["config", ...]`, resolved through the same
    /// `scope.config()` any other expression uses. The directional light is
    /// BLACK (as in the sibling tests above), so it drops out of the
    /// average and this isolates `["config", "myColor"]`/`["config",
    /// "myIntensity"]` on the ambient side.
    ///
    /// A non-white color, deliberately: white would make this pass even if
    /// the `color` expression were silently dropped and only `intensity`
    /// resolved through config, since `relative_luminance(white) *
    /// intensity == intensity` either way. Red's channels are exactly `1`
    /// and `0`, where the W3C curve agrees with a plain dot product, so
    /// `relative_luminance(red) == 0.2126` exactly, making both config keys
    /// load-bearing in the expected value without pulling the W3C curve's
    /// own correctness into this test (that is `relative_luminance`'s own
    /// job -- see the acceptance test below).
    #[test]
    fn measure_light_reads_ambient_color_and_intensity_through_config() {
        let lights = serde_json::json!([
            {
                "id": "ambient",
                "properties": {
                    "color": ["config", "myColor"],
                    "intensity": ["config", "myIntensity"]
                }
            },
            { "id": "directional", "properties": {
                "color": "#000000", "intensity": 0.9, "direction": ["literal", [0, 0]]
            } }
        ]);
        let resolved = crate::style_spec::config::ResolvedConfig::from_pairs([
            ("myColor", serde_json::json!("#ff0000")),
            ("myIntensity", serde_json::json!(0.5)),
        ]);
        let json: Value = serde_json::from_str(r#"["measure-light", "brightness"]"#).unwrap();
        let scope = Scope::root().with_config(&resolved).with_lights(&lights);
        // `.toFixed(6)`-rounded, same as the implementation -- see
        // `measure_light_brightness`'s doc comment on why that rounding is
        // load-bearing and not to be dropped.
        let expected: f64 = format!("{:.6}", 0.2126_f64 * 0.5 / 2.0).parse().unwrap();
        assert_eq!(eval(&json, scope, 0).unwrap(), EvalValue::Number(expected));
    }

    #[test]
    fn measure_light_rejects_an_unsupported_component() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "#ffffff", "intensity": 1.0 } }
        ]);
        let json: Value = serde_json::from_str(r#"["measure-light", "contrast"]"#).unwrap();
        let scope = Scope::root().with_lights(&lights);
        let error = eval(&json, scope, 0).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
    }

    #[test]
    fn measure_light_with_no_ambient_entry_does_not_fold() {
        let lights = serde_json::json!([{ "id": "directional", "properties": {} }]);
        let json: Value = serde_json::from_str(r#"["measure-light", "brightness"]"#).unwrap();
        let scope = Scope::root().with_lights(&lights);
        let error = eval(&json, scope, 0).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
    }

    /// The mirror image of the test above, and of
    /// `ground_radiance_with_no_directional_entry_does_not_fold`: a real
    /// document shape (a `lights` array with `ambient` but no
    /// `directional`) must refuse to fold, not silently treat the missing
    /// light as black/zero -- matching the reference's own `if
    /// (!directional || !ambient) return;` gate in
    /// `calculateLightsBrightness`.
    #[test]
    fn measure_light_with_no_directional_entry_does_not_fold() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "#ffffff", "intensity": 1.0 } }
        ]);
        let json: Value = serde_json::from_str(r#"["measure-light", "brightness"]"#).unwrap();
        let scope = Scope::root().with_lights(&lights);
        let error = eval(&json, scope, 0).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
    }

    /// The correct formula's acceptance criterion, per fix round 2 of the
    /// task-2 report: `measure-light`'s brightness for all four
    /// `lightPreset`s, computed against the real
    /// `examples/mapbox-standard-dark-style.json` fixture at `theme:
    /// monochrome`, zoom 14 -- unlike `ground_radiance_matches_the_four_
    /// preset_acceptance_table`, this uses the fixture's REAL directional
    /// intensity (a zoom `interpolate`), not an override, since
    /// `measure-light` is exactly the zoom-14 case real paint properties
    /// ask for.
    #[test]
    fn measure_light_brightness_matches_the_four_preset_acceptance_table() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();

        let cases: [(&str, f64); 4] =
            [("day", 0.502778), ("dawn", 0.387086), ("dusk", 0.013218), ("night", 0.0)];

        for (preset, expected) in cases {
            let mut preset_root = root.clone();
            preset_root["imports"][0]["config"]["lightPreset"] = serde_json::json!(preset);
            let imports = crate::style_spec::config::parse_imports(&preset_root).unwrap();
            let import = &imports[0];
            assert_eq!(import.config.resolve("lightPreset"), Some(&serde_json::json!(preset)));
            assert_eq!(
                import.config.resolve("theme"),
                Some(&serde_json::json!("monochrome")),
                "the fixture's own default theme, unchanged"
            );

            let scope = Scope::root().with_config(&import.config).with_lights(&import.data["lights"]);
            let json: Value = serde_json::from_str(r#"["measure-light", "brightness"]"#).unwrap();
            let brightness = eval(&json, scope, 14)
                .unwrap_or_else(|e| panic!("measure-light must fold for preset {preset:?}: {e:#}"));
            let EvalValue::Number(got) = brightness else { panic!("must fold to a number, got {brightness:?}") };
            assert!((got - expected).abs() < 0.0000005, "preset {preset:?}: got {got:.6}, want {expected:.6}");
        }
    }

    /// `sphericalDirectionToCartesian`'s +90 degree compass correction
    /// (`src/util/util.ts`), pinned directly: without the correction,
    /// `az=0` would point along `+x`; with it (compass "north" = `+y`),
    /// `az=0, pol=90` (horizon) must point along `+y`, not `+x`.
    #[test]
    fn spherical_direction_applies_the_90_degree_compass_correction() {
        let [x, y, z] = spherical_direction_to_cartesian(0.0, 90.0);
        assert!((x - 0.0).abs() < 1e-9, "x should be ~0, got {x}");
        assert!((y - 1.0).abs() < 1e-9, "y should be ~1 -- the +90 degree correction moved az=0 to +y, not +x: got {y}");
        assert!((z - 0.0).abs() < 1e-9, "z should be ~0, got {z}");
    }

    /// Straight up (`polar = 0`) is `[0, 0, 1]` regardless of azimuthal --
    /// `sin(polar) = 0` zeroes out both `x` and `y` before the compass
    /// correction ever has anything to act on.
    #[test]
    fn spherical_direction_straight_up_is_the_ground_normal() {
        let [x, y, z] = spherical_direction_to_cartesian(37.0, 0.0);
        assert!((x - 0.0).abs() < 1e-9, "x should be ~0, got {x}");
        assert!((y - 0.0).abs() < 1e-9, "y should be ~0, got {y}");
        assert!((z - 1.0).abs() < 1e-9, "z should be ~1, got {z}");
    }

    /// `calculate_ambient_directional_factor`'s `vertical_factor` collapses
    /// to exactly 1.0 for the ground normal `[0, 0, 1]` (`asin(clamp(1,-1,1))
    /// / PI + 0.5 == 1.0`, so `lerp(0.92, 1.0, 1.0) == 1.0`) -- verified here
    /// by checking the factor equals what it would be if `vertical_factor`
    /// were dropped entirely (i.e. just `calculateAmbientDirectionalFactor`'s
    /// `ambientDirectionalFactor` term), for a `dir`/`dir_color` pair where
    /// the two are NOT trivially both 1.0 by coincidence.
    #[test]
    fn ambient_directional_factor_vertical_term_collapses_to_one_for_the_ground_normal() {
        let dir = spherical_direction_to_cartesian(240.0, 30.0);
        let dir_color = [0.5, 0.5, 0.5];
        let factor = calculate_ambient_directional_factor(dir, GROUND_NORMAL, dir_color);

        let n_dot_l = dir[2];
        let dir_luminance: f64 = 0.5 * 0.2126 + 0.5 * 0.7152 + 0.5 * 0.0722;
        let directional_factor_min = 1.0 - 0.3 * dir_luminance.min(1.0);
        let lerp = |a: f64, b: f64, t: f64| (1.0 - t) * a + t * b;
        let ambient_directional_factor_only = lerp(directional_factor_min, 1.0, (n_dot_l + 1.0).min(1.0));

        assert!(
            (factor - ambient_directional_factor_only).abs() < 1e-12,
            "vertical_factor should be exactly 1.0 for the ground normal, so the full factor \
             ({factor}) should equal the ambient-directional term alone ({ambient_directional_factor_only})"
        );
    }

    /// `ground_radiance` with no `lights` on the scope errors, marked
    /// `Unfoldable` -- the same `LIGHTING_3D_MODE` gate
    /// `measure_light_with_no_lights_on_the_scope_does_not_fold` pins for
    /// `measure-light`, and what keeps a style with no `lights` (Liberty,
    /// Mapbox Streets) from computing any radiance at all.
    #[test]
    fn ground_radiance_with_no_lights_on_the_scope_does_not_fold() {
        let error = ground_radiance(Scope::root(), 0).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
    }

    /// A real document shape a style author can write: a `lights` array
    /// with an `ambient` entry but no `directional` one (the mirror image
    /// of the pre-existing `measure_light_with_no_ambient_entry_does_not_fold`,
    /// which pins the same gate for the opposite omission on
    /// `measure-light`). Mapbox's own `uploadCommonLightUniforms`
    /// (`src/render/painter.ts`, ~line 1769) only computes lighting `if
    /// (directionalLight && ambientLight)` -- ONE light present is not
    /// enough, so this must refuse to fold exactly like the no-`lights`-at-
    /// all case, not silently treat the missing light as black/zero.
    #[test]
    fn ground_radiance_with_no_directional_entry_does_not_fold() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "#ffffff", "intensity": 1.0 } }
        ]);
        let scope = Scope::root().with_lights(&lights);
        let error = ground_radiance(scope, 0).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
    }

    /// The task's acceptance criterion, transcribed verbatim from the
    /// task-2 brief's "The numbers this produces for our fixture": ground
    /// radiance for all four `lightPreset`s, computed against the real
    /// `examples/mapbox-standard-dark-style.json` fixture with `theme:
    /// monochrome` (the fixture's own default -- see
    /// `waters_fill_color_folds_dark_for_night_and_plain_for_day`, which
    /// pins the same default), asserted per channel to 3 decimal places.
    ///
    /// The real fixture's `directional.intensity` is a zoom `interpolate`
    /// with no stop equal to 1.0 at any preset/zoom -- so, exactly as the
    /// brief specifies ("directional intensity taken as 1.0"), this test
    /// takes the real fixture's `lights` array and overrides ONLY
    /// `directional.properties.intensity` to the literal `1.0`, leaving
    /// every color and direction expression exactly as shipped. That
    /// isolates the `calculate_ground_radiance`/
    /// `spherical_direction_to_cartesian` transcription from the separate
    /// (and already-tested-elsewhere) question of whether the intensity
    /// `interpolate` itself folds correctly at a given zoom.
    #[test]
    fn ground_radiance_matches_the_four_preset_acceptance_table() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();

        let cases: [(&str, [f32; 3]); 4] = [
            ("day", [1.3029, 1.3029, 1.3029]),
            ("dawn", [1.2046, 1.0407, 0.8728]),
            ("dusk", [0.4720, 0.4733, 0.4807]),
            ("night", [0.0000, 0.0000, 0.0000]),
        ];

        for (preset, expected) in cases {
            let mut preset_root = root.clone();
            preset_root["imports"][0]["config"]["lightPreset"] = serde_json::json!(preset);
            let imports = crate::style_spec::config::parse_imports(&preset_root).unwrap();
            let import = &imports[0];
            assert_eq!(import.config.resolve("lightPreset"), Some(&serde_json::json!(preset)));
            assert_eq!(
                import.config.resolve("theme"),
                Some(&serde_json::json!("monochrome")),
                "the fixture's own default theme, unchanged"
            );

            let mut lights = import.data["lights"].clone();
            let directional = lights
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|light| light.get("id").and_then(Value::as_str) == Some("directional"))
                .expect("the fixture has a directional light");
            directional["properties"]["intensity"] = serde_json::json!(1.0);

            let scope = Scope::root().with_config(&import.config).with_lights(&lights);
            // Zoom is irrelevant here: every expression left in play
            // (color, direction, and now the overridden literal intensity)
            // is zoom-independent in this fixture.
            let radiance = ground_radiance(scope, 14)
                .unwrap_or_else(|e| panic!("ground radiance must fold for preset {preset:?}: {e:#}"));

            for (channel, (got, want)) in ["r", "g", "b"].iter().zip(radiance.iter().zip(expected.iter())) {
                assert!(
                    (got - want).abs() < 0.0005,
                    "preset {preset:?} channel {channel}: got {got:.4}, want {want:.4}"
                );
            }
        }
    }

    /// The acceptance case, and the one that matters most in the whole plan:
    /// water's real `fill-color`, folded with the real fixture's resolved
    /// lights and config, renders DARK for a night/monochrome config and
    /// plain `colorWater` for a day config -- proof a dark style actually
    /// renders dark rather than merely compiling.
    ///
    /// The drift guard: pin down exactly the sub-shapes this test and the
    /// `measure-light` design rely on, so a fixture change fails the test
    /// loudly instead of silently testing a stale expression.
    #[test]
    fn waters_fill_color_folds_dark_for_night_and_plain_for_day() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();

        let find_water = |root: &Value| -> Value {
            let imports = crate::style_spec::config::parse_imports(root).unwrap();
            let import = &imports[0];
            let water = import.data["layers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l.get("id").and_then(Value::as_str) == Some("water"))
                .expect("the fixture has a water layer")
                .clone();
            water
        };

        let water = find_water(&root);
        let fill_color = &water["paint"]["fill-color"];

        // Drift guard: the outer interpolate is zoom-driven, its zoom-14
        // stop is itself an interpolate over measure-light("brightness")
        // with stops 0 and 0.02, and the far stop is plain colorWater --
        // exactly the shape "Where the dark actually comes from" describes.
        assert_eq!(fill_color[0], serde_json::json!("interpolate"), "fixture shape moved out from under this test");
        assert_eq!(fill_color[2], serde_json::json!(["zoom"]), "fixture shape moved out from under this test");
        assert_eq!(fill_color[3], serde_json::json!(13), "fixture shape moved out from under this test");
        assert_eq!(fill_color[4], serde_json::json!(["config", "colorWater"]), "fixture shape moved out from under this test");
        assert_eq!(fill_color[5], serde_json::json!(14), "fixture shape moved out from under this test");
        let inner = &fill_color[6];
        assert_eq!(inner[0], serde_json::json!("interpolate"), "fixture shape moved out from under this test");
        assert_eq!(inner[2], serde_json::json!(["measure-light", "brightness"]), "fixture shape moved out from under this test");
        assert_eq!(inner[3], serde_json::json!(0), "fixture shape moved out from under this test");
        assert_eq!(inner[5], serde_json::json!(0.02), "fixture shape moved out from under this test");
        assert_eq!(inner[6], serde_json::json!(["config", "colorWater"]), "fixture shape moved out from under this test");

        // Zoom 14 lands exactly on the outer interpolate's second stop, so
        // the result is the inner (measure-light-driven) interpolate's own
        // value with no blending -- isolating exactly the branch this test
        // needs to check.

        // Night + monochrome: the fixture's own real config -- ambient
        // color hsl(217,100%,0%) at intensity 0, brightness 0 -- must take
        // the DARKENED branch (lightness reduced by 55).
        let imports = crate::style_spec::config::parse_imports(&root).unwrap();
        let night_import = &imports[0];
        assert_eq!(night_import.config.resolve("lightPreset"), Some(&serde_json::json!("night")));
        assert_eq!(night_import.config.resolve("theme"), Some(&serde_json::json!("monochrome")));
        let night_scope = Scope::root().with_config(&night_import.config).with_lights(&night_import.data["lights"]);
        let night_water = find_water(&root);
        let night_folded = eval(&night_water["paint"]["fill-color"], night_scope, 14)
            .unwrap_or_else(|e| panic!("water's fill-color must fold for a night config: {e:#}"));
        let night_color = night_folded.as_color().expect("must fold to a color");
        // colorWater is overridden to hsl(0, 0%, 76%) in the fixture's
        // config; darkened is the same hue/saturation with lightness
        // 76 - 55 = 21%.
        let expected_dark = parse_css_color("hsl(0, 0%, 21%)").unwrap();
        assert_eq!(night_color, expected_dark, "night/monochrome must render the DARKENED branch, not plain colorWater");

        // Day: only `lightPreset` is overridden here, one key deeper than
        // the fixture's own config object, so the other 19 real overrides
        // (`colorWater` among them) survive -- `theme` stays "monochrome",
        // the fixture's own value, rather than falling back to a schema
        // default the shipped document never renders. Ambient color is
        // hsl(0,0%,100%) for "day" regardless of theme, at intensity 0.85
        // (theme "monochrome"; 0.8 for "default" -- either way well above
        // the darkening threshold), so this must take the plain-`colorWater`
        // branch.
        let mut day_root = root.clone();
        day_root["imports"][0]["config"]["lightPreset"] = serde_json::json!("day");
        let day_imports = crate::style_spec::config::parse_imports(&day_root).unwrap();
        let day_import = &day_imports[0];
        assert_eq!(day_import.config.resolve("lightPreset"), Some(&serde_json::json!("day")));
        assert_eq!(day_import.config.resolve("theme"), Some(&serde_json::json!("monochrome")));
        let day_scope = Scope::root().with_config(&day_import.config).with_lights(&day_import.data["lights"]);
        let day_water = find_water(&day_root);
        let day_folded = eval(&day_water["paint"]["fill-color"], day_scope, 14)
            .unwrap_or_else(|e| panic!("water's fill-color must fold for a day config: {e:#}"));
        let day_color = day_folded.as_color().expect("must fold to a color");
        // colorWater's real override in this fixture: hsl(0, 0%, 76%).
        let expected_plain = parse_css_color("hsl(0, 0%, 76%)").unwrap();
        assert_eq!(day_color, expected_plain, "day must render plain colorWater, not the darkened branch");
    }

    /// The same water `fill-color`, but through the PAINT COMPILER rather
    /// than `eval` directly -- which is the path `parse_and_compile` takes
    /// and the one that was still dropping the layer whole.
    ///
    /// The outer `interpolate` is over `["zoom"]`, so `compile_interpolate`
    /// owns it and reads each stop value with `parse_leaf` directly. The
    /// zoom-14 stop is itself an `interpolate`, over
    /// `["measure-light", "brightness"]` -- a compile-constant input, not
    /// zoom -- so it has exactly one answer and must fold there. Before this
    /// seam existed `color_leaf` saw an array it did not recognize, returned
    /// a PLAIN error, and `try_or_skip!` deleted the whole water layer.
    #[test]
    fn a_stop_value_that_is_an_interpolate_over_a_constant_input_folds() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();
        let imports = crate::style_spec::config::parse_imports(&root).unwrap();
        let import = &imports[0];
        let water = import.data["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l.get("id").and_then(Value::as_str) == Some("water"))
            .expect("the fixture has a water layer");
        let scope = Scope::root().with_config(&import.config).with_lights(&import.data["lights"]);

        let mut pairs = compile_zoom_value(&water["paint"]["fill-color"], color_leaf, scope)
            .unwrap_or_else(|e| panic!("water's fill-color must compile, not skip the layer: {e:#}"));
        assert_eq!(pairs.len(), 1, "water's fill-color has no feature-dependent case");
        let (_, ramp) = pairs.remove(0);
        // Zoom 13 is the outer curve's first stop: plain colorWater, which
        // the fixture's config overrides to hsl(0, 0%, 76%).
        assert_eq!(ramp.value_at(13), parse_css_color("hsl(0, 0%, 76%)").unwrap());
        // Zoom 14 is its second stop: the folded, DARKENED branch.
        assert_eq!(ramp.value_at(14), parse_css_color("hsl(0, 0%, 21%)").unwrap());
    }

    /// The same shape one level up: 5 real fixture layers (`hillshade`,
    /// `road-rail`, `road-rail-tracks`, `bridge-rail`, `bridge-rail-tracks`)
    /// write the whole paint value as an `interpolate` over
    /// `["measure-light", "brightness"]`, with no enclosing zoom curve. That
    /// reaches `compile_interpolate` itself, whose `["zoom"]`-input check
    /// used to reject it outright.
    #[test]
    fn a_top_level_interpolate_over_a_constant_input_folds_to_a_constant() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "hsl(217, 100%, 0%)", "intensity": 0 } },
            { "id": "directional", "properties": {
                "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]]
            } }
        ]);
        let json: Value = serde_json::from_str(
            r##"["interpolate", ["linear"], ["measure-light", "brightness"], 0, "#102030", 0.02, "#ffffff"]"##,
        )
        .unwrap();
        let scope = Scope::root().with_lights(&lights);
        let mut pairs = compile_zoom_value(&json, color_leaf, scope).expect("must fold, not fail");
        assert_eq!(pairs.len(), 1);
        let (_, ramp) = pairs.remove(0);
        // Brightness 0 pins the curve to its first stop at every zoom.
        assert_eq!(ramp.value_at(0), parse_css_color("#102030").unwrap());
        assert_eq!(ramp.value_at(22), parse_css_color("#102030").unwrap());
    }

    /// The counter-case, and the guard on the whole seam: an `interpolate`
    /// over a FEATURE lookup is not compile-constant and never will be.
    /// Mapbox Streets' `water-depth` layer is the real one
    /// (`["interpolate", ["linear"], ["get", "min_depth"], ...]`), and it is
    /// correctly skipped today. The error must stay PLAIN -- not
    /// `Unfoldable` -- so `try_or_skip!` keeps dropping the layer instead of
    /// `or_degrade` resurrecting it in a colour nobody asked for.
    #[test]
    fn an_interpolate_over_a_feature_lookup_still_fails_and_is_not_a_folder_refusal() {
        let json: Value =
            serde_json::from_str(r##"["interpolate", ["linear"], ["get", "min_depth"], 0, "#000000", 100, "#ffffff"]"##)
                .unwrap();
        let error = compile_zoom_value(&json, color_leaf, Scope::root()).unwrap_err();
        assert!(
            !is_unfoldable(&error),
            "a feature-driven interpolate is an unsupported CONSTRUCT, not a folder refusal: {error:#}"
        );
    }

    /// `hillshade` and `recreation-polygon`, both real: an `interpolate`
    /// over `["measure-light", "brightness"]` whose STOPS are `match`es on
    /// a feature property. The input is compile-constant, so the curve
    /// picks one stop -- but that stop still varies per feature, so it must
    /// be COMPILED into filtered cases, not folded. Folding refuses, and
    /// `or_degrade` would then repaint two real basemap layers in opaque
    /// black.
    #[test]
    fn a_constant_input_selects_a_stop_that_is_then_compiled_not_folded() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "hsl(217, 100%, 0%)", "intensity": 0 } },
            { "id": "directional", "properties": {
                "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]]
            } }
        ]);
        // hillshade's real shape, trimmed: brightness 0 clamps to the
        // 0.25 stop, whose value is a match on the feature's "class".
        let json: Value = serde_json::from_str(
            r#"["interpolate", ["linear"], ["measure-light", "brightness"],
                0.25, ["match", ["get", "class"], "shadow", "hsla(235, 10%, 10%, 0.1)", "hsla(235, 20%, 80%, 0)"],
                0.3,  ["match", ["get", "class"], "shadow", "hsla(40, 20%, 20%, 0.04)", "hsla(20, 20%, 100%, 0.12)"]]"#,
        )
        .unwrap();
        let scope = Scope::root().with_lights(&lights);
        let pairs = compile_zoom_value(&json, color_leaf, scope).expect("must compile, not degrade");
        assert_eq!(pairs.len(), 2, "the selected stop's match must expand into its own filtered cases");
        let shadow = pairs
            .iter()
            .find(|(filter, _)| matches!(filter, Filter::All(clauses)
                if clauses.iter().any(|c| matches!(c, Filter::In { key, values } if key == "class" && values == &["shadow".to_string()]))))
            .expect("a \"shadow\" case");
        assert_eq!(shadow.1.value_at(0), crate::style_spec::color::parse_css_color("hsla(235, 10%, 10%, 0.1)").unwrap());
    }

    /// The between-stops case, which the clamped path above does not cover.
    ///
    /// `recreation-polygon`'s ramp runs over [0.1, 0.4] and its stops are
    /// `["match", ["get", "type"], ...]`, which never fold; this layer's
    /// `["measure-light", "brightness"]` input folds ONCE, at zoom 0 (no
    /// literal `["zoom"]` anywhere in its expression -- see
    /// `compile_folded`'s doc comment), so it is one fixed number per
    /// preset/theme, not a per-zoom curve.
    ///
    /// Under the correct `calculateLightsBrightness` formula (fix round 2 of
    /// the task-2 report -- this was `dusk`/`default` under the earlier,
    /// wrong approximation, which this test used to pin): `dawn` with the
    /// "default" (or "faded") theme is the one that lands strictly INSIDE
    /// the domain (0.396140; `monochrome`'s 0.414247 clamps just above it),
    /// and a blend of two unfoldable stops is a folder refusal, so before
    /// this seam existed the property degraded and every park and car park
    /// went opaque black from z15 up. It must compile to the nearer stop's
    /// own cases instead -- here the UPPER one, 0.4, since 0.396140 sits
    /// only 0.00386 below it (versus 0.29614 from the lower stop).
    #[test]
    fn a_constant_input_between_two_unfoldable_stops_compiles_the_nearer_one() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let mut root: Value = serde_json::from_slice(&bytes).unwrap();
        // One index deeper than the whole config object, so the other 18
        // real overrides (`colorWater` etc.) stay at the values the shipped
        // document actually sets -- `theme` is still forced to "default"
        // here because "dawn"+"monochrome" clamps to a different directional
        // intensity and lands outside this layer's [0.1, 0.4] domain (see
        // `the_vendored_mapbox_standard_dark_fixture_compiles_with_no_folder_degradations`).
        root["imports"][0]["config"]["lightPreset"] = serde_json::json!("dawn");
        root["imports"][0]["config"]["theme"] = serde_json::json!("default");
        let imports = crate::style_spec::config::parse_imports(&root).unwrap();
        let import = &imports[0];
        let scope = Scope::root().with_config(&import.config).with_lights(&import.data["lights"]);

        // Drift guard: brightness really does land strictly between this
        // layer's two stops for this preset.
        let brightness = eval(&serde_json::json!(["measure-light", "brightness"]), scope, 0)
            .unwrap()
            .as_number()
            .unwrap();
        assert!((0.1..0.4).contains(&brightness), "dawn/default brightness was {brightness}");

        let layer = import.data["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l.get("id").and_then(Value::as_str) == Some("recreation-polygon"))
            .expect("the fixture has a recreation-polygon layer");
        let pairs = compile_zoom_value(&layer["paint"]["fill-color"], color_leaf, scope)
            .expect("must compile rather than refuse");

        assert_eq!(pairs.len(), 2, "the nearer stop's match must expand into its own filtered cases");
        let black = crate::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        for (_, ramp) in &pairs {
            assert_ne!(ramp.value_at(15), black, "a real landuse fill must never compile to opaque black");
        }
        // The nearer stop is now 0.4, whose default branch is a literal.
        assert!(
            pairs.iter().any(|(_, ramp)| ramp.value_at(15)
                == crate::style_spec::color::parse_css_color("hsl(52, 65%, 86%)").unwrap()),
            "the 0.4 stop's default colour must be one of the compiled cases"
        );
    }

    /// The direction the fixture-driven test above never reaches: an input
    /// closer to the UPPER of two unfoldable stops must compile that upper
    /// stop, not just default toward the lower one by construction.
    #[test]
    fn nearest_stop_fallback_prefers_the_upper_stop_when_input_is_closer_to_it() {
        let resolved = crate::style_spec::config::ResolvedConfig::from_pairs([("t", serde_json::json!(0.37))]);
        let scope = Scope::root().with_config(&resolved);
        let json: Value = serde_json::from_str(
            r#"["interpolate", ["linear"], ["config", "t"],
                0.1, ["match", ["get", "kind"], "a", "hsl(0, 100%, 50%)", "hsl(120, 100%, 50%)"],
                0.4, ["match", ["get", "kind"], "a", "hsl(240, 100%, 50%)", "hsl(300, 100%, 50%)"]]"#,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, scope).expect("must compile the nearer stop, not refuse");
        assert!(
            pairs.iter().any(|(_, ramp)| ramp.value_at(0) == parse_css_color("hsl(240, 100%, 50%)").unwrap()),
            "0.37 is closer to the 0.4 stop than to 0.1, so its \"a\" case must be compiled: {pairs:?}"
        );
        assert!(
            pairs.iter().all(|(_, ramp)| ramp.value_at(0) != parse_css_color("hsl(0, 100%, 50%)").unwrap()),
            "the farther (0.1) stop must not be selected: {pairs:?}"
        );
    }

    /// `min_by` returns the FIRST minimum, so an exact tie between two
    /// stops' distances must resolve to the lower (earlier) one. This path
    /// is live on the real fixture (not a hypothetical): pin the tie-break
    /// direction rather than leaving it to fall out of iteration order by
    /// accident.
    #[test]
    fn nearest_stop_fallback_breaks_an_exact_tie_toward_the_lower_stop() {
        let resolved = crate::style_spec::config::ResolvedConfig::from_pairs([("t", serde_json::json!(0.25))]);
        let scope = Scope::root().with_config(&resolved);
        let json: Value = serde_json::from_str(
            r#"["interpolate", ["linear"], ["config", "t"],
                0.1, ["match", ["get", "kind"], "a", "hsl(0, 100%, 50%)", "hsl(120, 100%, 50%)"],
                0.4, ["match", ["get", "kind"], "a", "hsl(240, 100%, 50%)", "hsl(300, 100%, 50%)"]]"#,
        )
        .unwrap();
        let pairs = compile_zoom_value(&json, color_leaf, scope).expect("must compile the nearer stop, not refuse");
        assert!(
            pairs.iter().any(|(_, ramp)| ramp.value_at(0) == parse_css_color("hsl(0, 100%, 50%)").unwrap()),
            "0.25 is exactly equidistant between 0.1 and 0.4, so the tie must resolve to the lower stop: {pairs:?}"
        );
        assert!(
            pairs.iter().all(|(_, ramp)| ramp.value_at(0) != parse_css_color("hsl(240, 100%, 50%)").unwrap()),
            "the upper stop must not be selected on a tie: {pairs:?}"
        );
    }

    /// The other direction of the same seam. Once the INPUT folds, the
    /// expression really is one the folder owns, so a failure further in
    /// (here an unresolvable `config` key in a stop value) is a folder
    /// REFUSAL and must carry `Unfoldable`, degrading the property rather
    /// than deleting the layer.
    #[test]
    fn a_constant_input_interpolate_that_cannot_finish_folding_is_a_folder_refusal() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "hsl(217, 100%, 0%)", "intensity": 0 } },
            { "id": "directional", "properties": {
                "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]]
            } }
        ]);
        let resolved = crate::style_spec::config::ResolvedConfig::empty();
        let json: Value = serde_json::from_str(
            r##"["interpolate", ["linear"], ["measure-light", "brightness"], 0, ["config", "notAKey"], 0.02, "#ffffff"]"##,
        )
        .unwrap();
        let scope = Scope::root().with_lights(&lights).with_config(&resolved);
        let error = compile_zoom_value(&json, color_leaf, scope).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
    }

    // ---- config -----------------------------------------------------------

    /// No config attached to the scope at all -- every style before Mapbox
    /// Standard, including both vendored fixtures.
    #[test]
    fn config_with_no_scope_config_does_not_fold() {
        let error = fold(r#"["config", "colorLand"]"#).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
        // Distinct from the sibling test below (a resolved-but-missing
        // key): this is "no config attached to the scope at all".
        assert!(format!("{error:#}").contains("no config resolved"), "{error:#}");
    }

    /// `eval` just reads whatever `ResolvedConfig::resolve` returns for a
    /// key -- `resolve_config` (config.rs) is what actually merges an
    /// override over the schema default, and to `eval` a schema-default
    /// value and an override-applied value are indistinguishable, both
    /// arriving as an already-resolved `ResolvedConfig`. (Two keys here,
    /// not one, mostly to keep the point visible: a single-key version of
    /// this test would look identical whichever of the two it happened to
    /// use, which is exactly why the schema-default and override-preferred
    /// cases used to be two separate tests asserting the same code path.)
    #[test]
    fn config_reads_whatever_the_resolved_config_holds() {
        let resolved = crate::style_spec::config::ResolvedConfig::from_pairs([
            ("colorLand", serde_json::json!("hsl(20, 20%, 95%)")),
            ("lightPreset", serde_json::json!("night")),
        ]);
        let scope = Scope::root().with_config(&resolved);
        assert_eq!(
            eval(&serde_json::json!(["config", "colorLand"]), scope, 0).unwrap(),
            EvalValue::String("hsl(20, 20%, 95%)".into())
        );
        assert_eq!(
            eval(&serde_json::json!(["config", "lightPreset"]), scope, 0).unwrap(),
            EvalValue::String("night".into())
        );
    }

    /// A key in neither the schema nor the override degrades the property
    /// rather than dropping the layer -- 796 uses of `config` means one
    /// unresolvable key must not take the whole style down.
    #[test]
    fn an_unknown_config_key_degrades_without_dropping_the_layer() {
        let resolved = crate::style_spec::config::ResolvedConfig::empty();
        let json: Value = serde_json::from_str(r#"["config", "notAKey"]"#).unwrap();
        let scope = Scope::root().with_config(&resolved);
        let error = eval(&json, scope, 0).unwrap_err();
        assert!(is_unfoldable(&error), "{error:#}");
        // Distinct from the sibling test above (no config attached at
        // all): a config IS resolved here, it just doesn't have this key.
        assert!(format!("{error:#}").contains("notAKey"), "{error:#}");
        assert!(format!("{error:#}").contains("not a resolvable config key"), "{error:#}");
    }

    /// The acceptance case: the real background layer's full
    /// `let`/`at`/`to-hsla`/`hsl` decomposition, with the real fixture's
    /// schema and config resolved exactly as `config.rs` resolves them,
    /// folds to the colour `hsl(20, 20%, 95%)` parses to. `colorLand` is
    /// not one of the fixture's 20 overridden keys, so this also exercises
    /// the schema-default path end to end.
    #[test]
    fn the_background_layers_full_expression_folds_to_the_schema_default_colour() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();
        let imports = crate::style_spec::config::parse_imports(&root).unwrap();
        let import = &imports[0];

        let background_layer = import.data["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l.get("type").and_then(Value::as_str) == Some("background"))
            .expect("the fixture has a background layer");
        let expr = &background_layer["paint"]["background-color"];

        let scope = Scope::root().with_config(&import.config);
        let folded = eval(expr, scope, 0).unwrap_or_else(|e| panic!("must fold: {e:#}"));
        let EvalValue::Color(colour) = folded else { panic!("expected a colour, got {folded:?}") };
        let expected = parse_css_color("hsl(20, 20%, 95%)").unwrap();
        assert_eq!(colour, expected);
    }

    // ---- the branching operators the folder needs ------------------------

    #[test]
    fn match_folds_to_the_branch_its_constant_input_selects() {
        assert_eq!(number(r#"["match", "night", "day", 1, "night", 2, 3]"#), 2.0);
        assert_eq!(number(r#"["match", "dusk", "day", 1, "night", 2, 3]"#), 3.0);
        // Grouped labels.
        assert_eq!(number(r#"["match", "dawn", ["dawn", "dusk"], 1, 2]"#), 1.0);
    }

    #[test]
    fn case_folds_to_the_first_arm_whose_condition_holds() {
        assert_eq!(number(r#"["case", false, 1, true, 2, 3]"#), 2.0);
        assert_eq!(number(r#"["case", false, 1, false, 2, 3]"#), 3.0);
        assert_eq!(number(r#"["case", ["==", 2, 2], 1, 0]"#), 1.0);
        assert_eq!(number(r#"["case", ["all", ["<", 1, 2], ["!", false]], 1, 0]"#), 1.0);
        assert_eq!(number(r#"["case", ["any", ["!=", 1, 1], [">=", 3, 4]], 1, 0]"#), 0.0);
    }

    #[test]
    fn step_folds_to_the_bucket_its_constant_input_lands_in() {
        assert_eq!(number(r#"["step", 0.5, 10, 1, 20, 2, 30]"#), 10.0);
        assert_eq!(number(r#"["step", 1, 10, 1, 20, 2, 30]"#), 20.0);
        assert_eq!(number(r#"["step", 5, 10, 1, 20, 2, 30]"#), 30.0);
    }

    #[test]
    fn interpolate_folds_numbers_and_colours_along_the_declared_curve() {
        assert_eq!(number(r#"["interpolate", ["linear"], 5, 0, 0, 10, 100]"#), 50.0);
        // Clamped outside the stop range, exactly as the style spec says.
        assert_eq!(number(r#"["interpolate", ["linear"], -3, 0, 0, 10, 100]"#), 0.0);
        assert_eq!(number(r#"["interpolate", ["linear"], 99, 0, 0, 10, 100]"#), 100.0);

        let mid = color(r##"["interpolate", ["linear"], 0.5, 0, "#000000", 1, "#ffffff"]"##);
        assert!((mid.r - 0.5).abs() < 1e-6 && (mid.g - 0.5).abs() < 1e-6 && (mid.b - 0.5).abs() < 1e-6, "{mid:?}");
    }

    #[test]
    fn zoom_folds_to_the_zoom_the_fold_is_being_done_at() {
        let json: Value = serde_json::from_str(r#"["+", ["zoom"], 1]"#).unwrap();
        assert_eq!(eval(&json, Scope::root(), 7).unwrap(), EvalValue::Number(8.0));
        let json: Value = serde_json::from_str(r#"["interpolate", ["linear"], ["zoom"], 0, 0, 10, 100]"#).unwrap();
        assert_eq!(eval(&json, Scope::root(), 4).unwrap(), EvalValue::Number(40.0));
    }

    #[test]
    fn an_unknown_operator_does_not_fold() {
        assert!(fold(r#"["distance-from-center"]"#).is_err());
        assert!(fold(r#"["not-an-operator", 1]"#).is_err());
    }
}
