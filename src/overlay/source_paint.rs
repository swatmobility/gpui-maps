//! A source's paint as an EXPRESSION over its features' own properties,
//! rather than a resolved value per feature.
//!
//! # The defect this removes
//!
//! An `OverlayFeature` carries a fully resolved [`FeaturePaint`] and, until
//! this module, no properties at all. So the only way to give N objects M
//! different colours was N separate features -- and a consumer animating
//! scheduled bus service had 3,140 vehicles, each its own feature *solely
//! because it carries its route colour*. Every push therefore built 3,140
//! `PaintOp`s, one per bus, for twelve distinct colours. They worked around
//! it by collapsing to twelve colour-grouped `Point` features, and lost the
//! heading each bus points in.
//!
//! MapLibre does not have this problem because paint lives on the LAYER and
//! reads the FEATURE: one source with 3,140 features, one fill layer with
//! `"fill-color": ["get", "route_colour"]`, one bucket, one draw call. This
//! module is that shape with "tile" deleted -- a source carries per-feature
//! properties, its paint is an expression over them, and `plan::build_plan`
//! collapses every feature that resolves to the same paint into ONE op.
//!
//! # It is the same expression compiler the basemap uses
//!
//! `style_spec::expression::compile_zoom_value` already turns a data-driven
//! style expression into a list of `(Filter, ZoomValue<T>)` branches, and
//! `style::Filter::matches` already evaluates one against a feature's tags.
//! That is how every basemap layer's `match`/`case`/`step` paint works.
//! Nothing here re-implements it; [`DataColor::Branches`] IS that output,
//! and [`DataColor::Get`] is the one shape that compiler cannot express (a
//! property whose VALUE is the colour, which no amount of compile-time
//! branching can enumerate).
//!
//! # Only colour, deliberately
//!
//! `line-color`, `fill-color` and `circle-color`, and nothing else. Colour
//! is the measured need, and it is also the one paint property that is
//! provably free of the two things that would make a data-driven value
//! expensive or unsound here:
//!
//! - **It has no zoom axis in an overlay.** `FeaturePaint`'s only
//!   zoom-varying field is `line_width`; every colour is a plain `Rgba`.
//!   So a data-driven colour cannot disturb `plan::zoom_dependence`, which
//!   is what `SourceOptions::zoom_independent` is graded against. This is
//!   ENFORCED rather than assumed: a colour expression whose value varies
//!   with zoom, or whose branch filters do, is refused at declaration time
//!   (see [`SourcePaint::compile`]).
//! - **It is already a per-vertex attribute.** `render::mesh::Vertex` has
//!   carried `color`/`color_to` since before the overlay existed, and
//!   `shader.wgsl` already blends and `srgb_to_linear_rgb`s them. So
//!   varying colour within one draw call needed no vertex-layout change and
//!   no shader change; what it needed was for the PLAN to stop emitting one
//!   op per feature.
//!
//! A data-driven `line-width` would have to join the bracket agreement in
//! `plan::zoom_dependence`, and a data-driven `text-field`/`icon-image`
//! would have to reach the symbol pass rather than the mesh -- both are
//! real, neither is what a consumer is blocked on, and both are refused
//! loudly rather than half-supported.
//!
//! # What this is worth, measured
//!
//! On the consumer's own shape -- 3,140 heading-shaped markers, two
//! 11-position rings each, twelve route colours -- a declaration here takes
//! the plan from **3,140 ops to 12** at an **unchanged 62,800 vertices**.
//!
//! It is an EXPRESSIVENESS change, not a frame-time one, and the numbers
//! say so plainly: `plan::merged_bake_bench` puts `build_plan` at 0.96x and
//! `tessellate` at 1.01x across that 261x op collapse, and
//! `examples/overlay_source_probe`'s two data-driven arms come out within
//! noise of each other. lyon's per-op overhead is small next to its
//! per-polygon work, and a push's real cost is per COORDINATE -- the
//! Mercator projection of each one, which `build_plan` does because it
//! produces the vertices.
//!
//! That per-coordinate cost used to be paid THREE times a push: once for
//! the plan's origin, once for its geometry, and once more by
//! `hit::HitIndex::build`. It is now paid once (see `geom::LonLatBounds`
//! and `hit`'s module doc), which took a 69,080-coordinate push from
//! 3.73 ms to 1.60 ms of UI thread -- a change that dwarfs anything the op
//! collapse did, and one the op collapse is what made worth looking for.
//!
//! What it does buy is the thing the consumer had to give up. They kept
//! 3,140 features only to carry 3,140 colours; collapsing to twelve
//! colour-grouped `Point` features got them twelve ops and cost them the
//! heading each bus points in, per-bus identity, and per-bus labels. With a
//! declaration they get twelve ops AND 3,140 features -- 3,140 ids, 3,140
//! hit targets, 3,140 labels -- which is the shape they asked for.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::mvt::TagValue;
use crate::style::{Filter, Rgba};

use super::columns::{Cell, PropertyTable};
use crate::style_spec::expression::{color_leaf, compile_zoom_value, Scope};

/// One feature's own properties, in the shape `style::Filter::matches`
/// already reads.
///
/// `HashMap<String, TagValue>` is not a new vocabulary: it is exactly what a
/// decoded MVT feature carries (`mvt::Feature::tags`) and exactly what every
/// compiled basemap filter is evaluated against. Reusing it is what lets a
/// data-driven overlay colour go through the same evaluator as a data-driven
/// basemap colour instead of a second one written to look like it.
pub type FeatureProperties = HashMap<String, TagValue>;

/// The shared empty property map: what a `DataFeatureInput` with no
/// properties hands in, and what `OverlayStore::feature_properties` hands
/// back for a feature that has none.
///
/// One allocation for the whole process. The store keeps no per-feature
/// map at all (see `columns`), so this costs a caller a refcount bump.
pub fn no_properties() -> Arc<FeatureProperties> {
    static EMPTY: std::sync::OnceLock<Arc<FeatureProperties>> = std::sync::OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(FeatureProperties::new())))
}

/// Which paint property a [`DataColor`] was declared for. Used only to
/// name the three slots of a [`SourcePaint`] in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorSlot {
    Line,
    Fill,
    /// `fill-outline-color`: the fill's one-pixel hairline, drawn as a
    /// line list over the fill's own vertices. No fallback -- undeclared,
    /// or unmatched, means no outline.
    FillOutline,
    Circle,
}

impl ColorSlot {
    /// The style-spec property name, which is also the key a caller writes
    /// and the key an error message names.
    fn key(self) -> &'static str {
        match self {
            Self::Line => "line-color",
            Self::Fill => "fill-color",
            Self::FillOutline => "fill-outline-color",
            Self::Circle => "circle-color",
        }
    }
}

/// One data-driven colour, compiled.
///
/// No `PartialEq`: `style::Filter` has none, deliberately (it is a
/// compiled predicate, not a value anybody compares), so neither this nor
/// `SourcePaint` can derive one either.
#[derive(Debug, Clone)]
enum DataColor {
    /// `["get", key]`: the feature's OWN property holds the colour, as a
    /// CSS colour string.
    ///
    /// The one shape `compile_zoom_value` genuinely cannot express, and the
    /// shape a fleet actually has. A compiled branch list has to enumerate
    /// every possible output at compile time; `["get", "route_colour"]`
    /// names a value nobody knows until a feature arrives. So this is a
    /// property READ, resolved per feature at plan time -- not a second
    /// expression evaluator: there is no expression left to evaluate.
    Get(String),
    /// Everything the style-spec compiler turns into branches: a literal
    /// colour, and `match`/`case`/`step` over feature properties. FIRST
    /// MATCH WINS, which is `compile_zoom_value`'s own ordering (it emits
    /// the specific branches first and the default last).
    ///
    /// `keys` is every property the filters read (see [`filter_keys`]):
    /// the columnar path memoises on those keys' values, so a `match` on a
    /// route walks the filters once per distinct route.
    Branches { branches: Vec<(Filter, Rgba)>, keys: Vec<String> },
    /// `interpolate`/`step` on a feature's number, and `case` with numeric
    /// or `null` comparisons: the shapes `Branches` cannot enumerate. See
    /// [`NumColor`]. `keys[slot]` names each [`Key`]'s property.
    Numeric { expr: NumColor, keys: Vec<String> },
}

impl DataColor {
    /// This colour for one feature, or `None` when no branch matched.
    ///
    /// `None` is not a failure and must not be treated as one: the caller
    /// falls back to the feature's own resolved `FeaturePaint` colour, which
    /// is the only answer that keeps a feature drawn. A missing property is
    /// ordinary -- a vehicle whose route is not yet known -- and painting it
    /// black or dropping it would both be worse than painting it the
    /// default.
    ///
    /// The per-MAP evaluation: the draw path uses [`Self::resolve_rows`],
    /// and this stays as the reference the property tests hold it to.
    #[cfg(test)]
    fn resolve(&self, properties: &FeatureProperties) -> Option<Rgba> {
        match self {
            Self::Get(key) => match properties.get(key) {
                Some(TagValue::String(text)) => {
                    crate::style_spec::color::parse_css_color(text).ok()
                }
                _ => None,
            },
            // Zoom 0.0 because every filter here has already been checked
            // zoom-free (`filter_is_zoom_free`), so no zoom can change the
            // answer. Passing the real camera zoom would be a lie of a
            // different kind: it would make this look zoom-sensitive when
            // the type system has been arranged so that it is not.
            Self::Branches { branches, .. } => branches
                .iter()
                .find(|(filter, _)| filter.matches(properties, 0.0))
                .map(|(_, color)| *color),
            // An evaluation error is MapLibre's "use the property default",
            // which here is the feature's own colour: `None`.
            Self::Numeric { expr, .. } => expr.evaluate(properties).ok().flatten(),
        }
    }

    /// [`Self::resolve`] for every row of `table` at once: entry `i` is
    /// row `i`'s answer. The columnar path, and the reason the table
    /// exists -- every key is bound to its column once here, not looked up
    /// per feature.
    ///
    /// `only`: just those rows, in that order -- entry `i` is row
    /// `only[i]`'s answer. The paint update's representative check reads
    /// one row per class this way, so its cost is the class count, not the
    /// source's row count, and nothing is copied out of the table.
    fn resolve_rows(&self, table: &PropertyTable, only: Option<&[usize]>) -> Vec<Option<Rgba>> {
        fn each<T>(rows: usize, only: Option<&[usize]>, f: impl FnMut(usize) -> T) -> Vec<T> {
            match only {
                Some(only) => only.iter().copied().map(f).collect(),
                None => (0..rows).map(f).collect(),
            }
        }
        let rows = table.rows();
        let len = only.map_or(rows, <[usize]>::len);
        match self {
            Self::Get(key) => {
                let Some(column) = table.column(key) else { return vec![None; len] };
                if let Some((codes, dict)) = table.dictionary(column) {
                    // One CSS parse per distinct string, not per feature.
                    let parsed: Vec<Option<Rgba>> = dict
                        .iter()
                        .map(|text| crate::style_spec::color::parse_css_color(text).ok())
                        .collect();
                    return each(rows, only, |row| {
                        match codes.get(row) {
                            Some(&code) if table.is_valid(column, row) => parsed[code as usize],
                            _ => None,
                        }
                    });
                }
                each(rows, only, |row| match table.cell(column, row) {
                    Some(Cell::Str(text)) => crate::style_spec::color::parse_css_color(text).ok(),
                    _ => None,
                })
            }
            Self::Numeric { expr, keys } => {
                let columns: Vec<Option<u32>> = keys.iter().map(|key| table.column(key)).collect();
                each(rows, only, |row| expr.evaluate(&BoundRow { table, columns: &columns, row }).ok().flatten())
            }
            Self::Branches { branches, keys } => {
                let columns: Vec<Option<u32>> = keys.iter().map(|key| table.column(key)).collect();
                let walk = |row: usize| -> Option<Rgba> {
                    // Just the keys the filters read: the answer cannot
                    // depend on any other.
                    let projected: FeatureProperties = keys
                        .iter()
                        .zip(&columns)
                        .filter_map(|(key, column)| Some((key.clone(), table.cell((*column)?, row)?.to_tag())))
                        .collect();
                    branches
                        .iter()
                        .find(|(filter, _)| filter.matches(&projected, 0.0))
                        .map(|(_, color)| *color)
                };
                const MEMO_KEYS: usize = 4;
                if columns.is_empty() {
                    // A literal: one walk for the whole source.
                    return vec![walk(0); len];
                }
                if columns.len() > MEMO_KEYS {
                    return each(rows, only, walk);
                }
                let mut memo: HashMap<[(u8, u64); MEMO_KEYS], Option<Rgba>> = HashMap::new();
                each(rows, only, |row| {
                    let mut tuple = [(0u8, 0u64); MEMO_KEYS];
                    for (slot, column) in tuple.iter_mut().zip(&columns) {
                        *slot = table.cell_key(*column, row);
                    }
                    *memo.entry(tuple).or_insert_with(|| walk(row))
                })
            }
        }
    }

    /// Compiles one property's expression.
    fn compile(slot: ColorSlot, expr: &Value) -> Result<Self, String> {
        let key = slot.key();
        if let Some(property) = get_expression_key(expr) {
            return Ok(Self::Get(property.to_string()));
        }
        // The numeric shapes first: the branch compiler refuses a property
        // `interpolate`/`step` ("must be [\"zoom\"]") and numeric `case`
        // conditions. Anything that reads no number -- a literal, a
        // `match`, a string `case`, and every ZOOM expression (whose input
        // is not a `get`) -- still goes to the branch compiler below.
        let numeric = NumColor::try_compile(expr).map_err(|error| format!("{key}: {error}"))?;
        let numeric_path = |mut numeric: NumColor| {
            let mut keys = Vec::new();
            numeric.bind(&mut keys);
            Self::Numeric { expr: numeric, keys }
        };
        let numeric = match numeric {
            Some(numeric) if numeric.reads_a_number() => return Ok(numeric_path(numeric)),
            other => other,
        };

        // A shape the branch compiler refuses but the typed one reads -- a
        // `case` on a BOOL property (`["==", ["get", "capital"], true]`, as
        // a CSV's inferred bool column gives) -- takes the typed path.
        // Only on refusal: whatever compiled before compiles the same way.
        let compiled = match compile_zoom_value(expr, color_leaf, Scope::root()) {
            Ok(compiled) => compiled,
            Err(error) => match numeric {
                Some(numeric) => return Ok(numeric_path(numeric)),
                None => return Err(format!("{key}: {error}")),
            },
        };

        let mut branches = Vec::with_capacity(compiled.len());
        for (filter, value) in compiled {
            // THE zoom check, and the reason `zoom_dependence` needs no
            // change at all. A colour that varies with zoom would have to
            // be baked as a bracketed pair and would then join the ramp
            // agreement `line-width` is subject to -- so a source declared
            // zoom-independent could acquire a colour ramp and quietly
            // start re-resolving at every integer zoom. Refused instead.
            if !value.is_constant() {
                return Err(format!(
                    "{key}: the value varies with ZOOM, which a data-driven source colour \
                     cannot -- a source's colour is baked once per push and never re-resolved, \
                     so only a data-driven (feature-property) expression is accepted here. \
                     Put the zoom ramp in `line-width`, which is bracketed for exactly this."
                ));
            }
            if !filter_is_zoom_free(&filter) {
                return Err(format!(
                    "{key}: a branch selects on ZOOM as well as on feature properties, which a \
                     data-driven source colour cannot -- see the zoom note above"
                ));
            }
            branches.push((filter, value.value_at(0)));
        }
        if branches.is_empty() {
            // `compile_zoom_value` always pushes at least a default pair, so
            // this is unreachable today; a total match beats an `expect`
            // that would panic on a caller's input if that ever changed.
            return Err(format!("{key}: {expr} compiled to no value at all"));
        }
        let mut keys = Vec::new();
        for (filter, _) in &branches {
            filter_keys(filter, &mut keys);
        }
        Ok(Self::Branches { branches, keys })
    }
}

/// Every property key `filter` can read, each once, into `keys`.
///
/// A positive walk over every variant, like [`filter_is_zoom_free`], so a
/// new key-reading variant fails to compile here rather than being
/// memoised on too few keys.
fn filter_keys(filter: &Filter, keys: &mut Vec<String>) {
    let mut add = |key: &String| {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    };
    match filter {
        Filter::Any | Filter::Never | Filter::GeometryType(_) => {}
        Filter::Equals { key, .. }
        | Filter::BoolEquals { key, .. }
        | Filter::In { key, .. }
        | Filter::NumberCompare { key, .. }
        | Filter::Has(key)
        | Filter::NumberCompareWithZoomOffset { key, .. } => add(key),
        Filter::All(filters) | Filter::AnyOf(filters) => filters.iter().for_each(|f| filter_keys(f, keys)),
        Filter::Not(inner) => filter_keys(inner, keys),
        Filter::ZoomStep { base, stops } => {
            filter_keys(base, keys);
            stops.iter().for_each(|(_, f)| filter_keys(f, keys));
        }
        Filter::Case { arms, fallback } => {
            for (cond, out) in arms {
                filter_keys(cond, keys);
                filter_keys(out, keys);
            }
            filter_keys(fallback, keys);
        }
        Filter::MultiMatch { key, cases, fallback } => {
            add(key);
            cases.iter().for_each(|(_, f)| filter_keys(f, keys));
            filter_keys(fallback, keys);
        }
        Filter::NumberMultiMatch { key, cases, fallback } => {
            add(key);
            cases.iter().for_each(|(_, f)| filter_keys(f, keys));
            filter_keys(fallback, keys);
        }
    }
}

/// `Some(key)` for exactly `["get", "key"]`, and nothing else.
///
/// Deliberately not a general `get` recogniser: `["get", key, object]` reads
/// a property of a nested object, which an overlay's flat property map has
/// no equivalent for, and treating it as a plain read would silently look up
/// the wrong thing.
fn get_expression_key(expr: &Value) -> Option<&str> {
    let array = expr.as_array()?;
    if array.len() != 2 || array[0].as_str() != Some("get") {
        return None;
    }
    array[1].as_str()
}

/// Whether a compiled filter's answer can depend on the camera zoom.
///
/// The two variants that read the `zoom` argument of `Filter::matches` are
/// `ZoomStep` and `NumberCompareWithZoomOffset`; every other variant either
/// reads tags alone or delegates to sub-filters. Written as a positive
/// whitelist walk rather than a "reject these two" check so that a NEW
/// zoom-reading variant added to `Filter` fails this rather than passing it
/// unnoticed -- a data-driven colour that quietly became zoom-dependent is
/// exactly the failure `SourceOptions::zoom_independent` exists to make
/// impossible.
fn filter_is_zoom_free(filter: &Filter) -> bool {
    match filter {
        Filter::Any
        | Filter::Never
        | Filter::Equals { .. }
        | Filter::BoolEquals { .. }
        | Filter::In { .. }
        | Filter::NumberCompare { .. }
        | Filter::Has(_)
        | Filter::GeometryType(_) => true,
        Filter::All(filters) | Filter::AnyOf(filters) => {
            filters.iter().all(filter_is_zoom_free)
        }
        Filter::Not(inner) => filter_is_zoom_free(inner),
        Filter::Case { arms, fallback } => {
            arms.iter().all(|(cond, out)| filter_is_zoom_free(cond) && filter_is_zoom_free(out))
                && filter_is_zoom_free(fallback)
        }
        Filter::MultiMatch { cases, fallback, .. } => {
            cases.iter().all(|(_, out)| filter_is_zoom_free(out)) && filter_is_zoom_free(fallback)
        }
        Filter::NumberMultiMatch { cases, fallback, .. } => {
            cases.iter().all(|(_, out)| filter_is_zoom_free(out)) && filter_is_zoom_free(fallback)
        }
        Filter::ZoomStep { .. } | Filter::NumberCompareWithZoomOffset { .. } => false,
    }
}

// ---------------------------------------------------------------------------
// Numeric colour expressions: `interpolate`, `step` and `case` over a
// feature's own NUMBER.
// ---------------------------------------------------------------------------

/// How many steps a property `interpolate` is quantised to along its ramp.
///
/// # Why quantise at all
///
/// A continuous ramp over a continuous property yields a distinct colour per
/// distinct value -- ~200k of them for 242k block-group incomes -- and every
/// distinct colour is its own paint class (`plan::classify`, u16, at most
/// 65,534) and its own op. So the interpolated colour is snapped to a
/// bounded palette BEFORE it becomes a class, and the class count stays at a
/// few hundred however many features there are.
///
/// # Why 256
///
/// 256 is the resolution of the colour maps a ramp like this is copied
/// from: matplotlib's viridis (and magma, inferno, plasma, cividis) IS a
/// 256-entry table, so a 256-step ramp reproduces the published map to
/// its own precision and adds no banding it does not already have. It is
/// also below what an 8-bit display can separate anyway: across viridis's
/// full range each channel moves at most ~220 code values, so one step of
/// 256 is under one 8-bit code value per channel on average -- adjacent
/// steps differ by less than a just-noticeable difference (CIE dE2000 ~0.4
/// on viridis). A 50%-opacity fill halves that again.
///
/// # How the steps are laid out
///
/// Per SEGMENT, not over the whole range, so every stop's own colour is hit
/// exactly (`value == stop` returns the stop's colour bit for bit, as
/// MapLibre does). Segment `i`, spanning `w_i` of the ramp's total input
/// width `W`, gets `n_i = max(1, floor(255 * w_i / W))` steps, and the
/// segment's interpolation factor `t` is rounded to a multiple of `1/n_i`.
/// So one ramp produces at most `1 + sum(n_i) <= 256 + (stops - 2)`
/// distinct colours, and exactly `<= 256` when no segment is narrower than
/// 1/255 of the range -- viridis's nine evenly spaced stops give 249.
pub(crate) const RAMP_STEPS: u32 = 256;

/// A colour expression over numeric (and presence) tests on one feature's
/// properties, evaluated with MapLibre's semantics
/// (`@maplibre/maplibre-gl-style-spec`, BSD-3:
/// `src/expression/definitions/{interpolate,step,case,comparison}.ts`).
///
/// This is not a second copy of the basemap's compiler: that compiler
/// ENUMERATES outputs at compile time into `(Filter, value)` branches,
/// which a continuous ramp cannot be. It exists for the three shapes the
/// branch compiler refuses on a feature property -- `interpolate` and
/// `step` on `["get", key]`, and `case` with numeric comparisons -- and
/// anything it does not recognise goes to the branch compiler unchanged.
#[derive(Debug, Clone)]
enum NumColor {
    Literal(Rgba),
    /// `["get", key]` as a CSS colour string, exactly [`DataColor::Get`].
    Get(Key),
    /// `["interpolate", ["linear" | "exponential", base], ["get", key], stop, colour, ...]`.
    Interpolate { key: Key, base: f64, stops: Vec<(f64, Rgba)>, steps: Vec<u32> },
    /// `["step", ["get", key], colour, stop, colour, ...]`.
    Step { key: Key, base: Box<NumColor>, stops: Vec<(f64, NumColor)> },
    /// `["case", cond, colour, ..., fallback]`.
    Case { arms: Vec<(Cond, NumColor)>, fallback: Box<NumColor> },
}

/// A `case` condition.
#[derive(Debug, Clone)]
enum Cond {
    Const(bool),
    Has(Key),
    Not(Box<Cond>),
    All(Vec<Cond>),
    Any(Vec<Cond>),
    Compare { key: Key, op: CompareOp, rhs: Literal },
}

/// A property name inside a compiled numeric expression, with the SLOT it
/// was given in that expression's own key list (`DataColor::Numeric::keys`).
///
/// The slot is what makes a columnar evaluation bind `["get", key]` to a
/// column ONCE per expression: the caller resolves `keys[slot]` to a
/// column index up front, and a row read is then `columns[slot]` -- no
/// string is hashed per feature. The map path (`FeatureProperties`)
/// ignores the slot and reads by `name`.
#[derive(Debug, Clone)]
struct Key {
    name: String,
    slot: usize,
}

impl Key {
    fn new(name: &str) -> Self {
        Self { name: name.to_string(), slot: 0 }
    }

    fn bind(&mut self, keys: &mut Vec<String>) {
        self.slot = match keys.iter().position(|k| *k == self.name) {
            Some(at) => at,
            None => {
                keys.push(self.name.clone());
                keys.len() - 1
            }
        };
    }
}

/// Where a compiled expression reads its properties from: a feature's own
/// map, or one row of a source's [`PropertyTable`] through columns bound
/// once per expression.
trait Props {
    fn cell(&self, key: &Key) -> Option<Cell<'_>>;
}

impl Props for FeatureProperties {
    fn cell(&self, key: &Key) -> Option<Cell<'_>> {
        self.get(key.name.as_str()).map(Cell::of)
    }
}

/// One row of a table, with the expression's keys already resolved to
/// column indices (`columns[slot]`).
struct BoundRow<'a> {
    table: &'a PropertyTable,
    columns: &'a [Option<u32>],
    row: usize,
}

impl Props for BoundRow<'_> {
    fn cell(&self, key: &Key) -> Option<Cell<'_>> {
        let column = (*self.columns.get(key.slot)?)?;
        self.table.cell(column, self.row)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
enum Literal {
    Null,
    Number(f64),
    String(String),
    Bool(bool),
}

/// An evaluation error: MapLibre's `RuntimeError`. The whole expression
/// then yields no colour, and the caller falls back to the feature's own
/// paint -- the overlay's equivalent of MapLibre falling back to the
/// property's default value.
struct EvalError;

impl NumColor {
    /// `Ok(None)`: not a shape this handles (the caller hands the
    /// expression to the branch compiler). `Err`: it IS one of these shapes
    /// and is malformed, which is reported rather than passed on so the
    /// caller sees the real problem, not the branch compiler's "must be
    /// zoom".
    fn try_compile(expr: &Value) -> Result<Option<Self>, String> {
        let Some(array) = expr.as_array() else {
            return Ok(color_leaf(expr, Scope::root()).ok().map(Self::Literal));
        };
        match array.first().and_then(Value::as_str) {
            Some("get") => Ok(get_expression_key(expr).map(|key| Self::Get(Key::new(key)))),
            Some("interpolate") => Self::compile_interpolate(array),
            Some("step") => Self::compile_step(array),
            Some("case") => Self::compile_case(array),
            _ => Ok(color_leaf(expr, Scope::root()).ok().map(Self::Literal)),
        }
    }

    fn compile_interpolate(array: &[Value]) -> Result<Option<Self>, String> {
        // ["interpolate", interpolation, input, stop, out, stop, out, ...]
        let Some(key) = array.get(2).and_then(get_expression_key) else { return Ok(None) };
        let base = match array.get(1).and_then(Value::as_array).map(Vec::as_slice) {
            Some([name]) if name.as_str() == Some("linear") => 1.0,
            Some([name, base]) if name.as_str() == Some("exponential") => {
                base.as_f64().ok_or("interpolate: the exponential base must be a number")?
            }
            _ => return Err("interpolate: only [\"linear\"] and [\"exponential\", base] are supported".into()),
        };
        let pairs = &array[3..];
        if pairs.is_empty() || !pairs.len().is_multiple_of(2) {
            return Err("interpolate: expected stop/colour pairs after the input".into());
        }
        let mut stops = Vec::with_capacity(pairs.len() / 2);
        for pair in pairs.chunks(2) {
            let stop = pair[0].as_f64().ok_or("interpolate: every stop must be a literal number")?;
            if let Some(&(last, _)) = stops.last() {
                if stop <= last {
                    return Err("interpolate: stops must be in strictly ascending order".into());
                }
            }
            let color = color_leaf(&pair[1], Scope::root())
                .map_err(|error| format!("interpolate: a stop output must be a literal colour: {error}"))?;
            stops.push((stop, color));
        }
        let width = stops.last().unwrap().0 - stops[0].0;
        let steps = stops
            .windows(2)
            .map(|w| {
                let share = if width > 0.0 { (w[1].0 - w[0].0) / width } else { 0.0 };
                ((f64::from(RAMP_STEPS - 1) * share).floor() as u32).max(1)
            })
            .collect();
        Ok(Some(Self::Interpolate { key: Key::new(key), base, stops, steps }))
    }

    fn compile_step(array: &[Value]) -> Result<Option<Self>, String> {
        // ["step", input, base, stop, out, stop, out, ...]
        let Some(key) = array.get(1).and_then(get_expression_key) else { return Ok(None) };
        let Some(base) = array.get(2) else { return Err("step: missing the base output".into()) };
        let Some(base) = Self::try_compile(base)? else { return Ok(None) };
        let pairs = &array[3..];
        if !pairs.len().is_multiple_of(2) {
            return Err("step: expected stop/output pairs after the base output".into());
        }
        let mut stops: Vec<(f64, NumColor)> = Vec::with_capacity(pairs.len() / 2);
        for pair in pairs.chunks(2) {
            let stop = pair[0].as_f64().ok_or("step: every stop must be a literal number")?;
            if stops.last().is_some_and(|(last, _)| stop <= *last) {
                return Err("step: stops must be in strictly ascending order".into());
            }
            let Some(output) = Self::try_compile(&pair[1])? else { return Ok(None) };
            stops.push((stop, output));
        }
        Ok(Some(Self::Step { key: Key::new(key), base: Box::new(base), stops }))
    }

    fn compile_case(array: &[Value]) -> Result<Option<Self>, String> {
        // ["case", cond, out, cond, out, ..., fallback]
        if array.len() < 4 || !array.len().is_multiple_of(2) {
            return Err("case: expected condition/output pairs and a fallback".into());
        }
        let mut arms = Vec::with_capacity(array.len() / 2 - 1);
        for pair in array[1..array.len() - 1].chunks(2) {
            let Some(cond) = Cond::compile(&pair[0]) else { return Ok(None) };
            let Some(output) = Self::try_compile(&pair[1])? else { return Ok(None) };
            arms.push((cond, output));
        }
        let Some(fallback) = Self::try_compile(array.last().unwrap())? else { return Ok(None) };
        Ok(Some(Self::Case { arms, fallback: Box::new(fallback) }))
    }

    /// Worth taking over from the branch compiler: only when something in
    /// it reads a NUMBER (interpolate, step, or a numeric comparison). A
    /// pure string/`has` `case` stays where it has always gone, so every
    /// expression that compiled before compiles to the same thing now.
    fn reads_a_number(&self) -> bool {
        match self {
            Self::Literal(_) | Self::Get(_) => false,
            Self::Interpolate { .. } | Self::Step { .. } => true,
            Self::Case { arms, fallback } => {
                fallback.reads_a_number()
                    || arms.iter().any(|(cond, out)| cond.reads_a_number() || out.reads_a_number())
            }
        }
    }

    /// Gives every key its slot in `keys` (see [`Key`]).
    fn bind(&mut self, keys: &mut Vec<String>) {
        match self {
            Self::Literal(_) => {}
            Self::Get(key) | Self::Interpolate { key, .. } => key.bind(keys),
            Self::Step { key, base, stops } => {
                key.bind(keys);
                base.bind(keys);
                stops.iter_mut().for_each(|(_, out)| out.bind(keys));
            }
            Self::Case { arms, fallback } => {
                for (cond, out) in arms {
                    cond.bind(keys);
                    out.bind(keys);
                }
                fallback.bind(keys);
            }
        }
    }

    fn evaluate<P: Props + ?Sized>(&self, properties: &P) -> Result<Option<Rgba>, EvalError> {
        match self {
            Self::Literal(color) => Ok(Some(*color)),
            Self::Get(key) => Ok(match properties.cell(key) {
                Some(Cell::Str(text)) => crate::style_spec::color::parse_css_color(text).ok(),
                _ => None,
            }),
            Self::Interpolate { key, base, stops, steps } => {
                Ok(Some(interpolate_quantised(number(properties, key)?, *base, stops, steps)))
            }
            Self::Step { key, base, stops } => {
                let value = number(properties, key)?;
                // MapLibre `Step.evaluate`: the last stop <= value wins,
                // the base below the first stop.
                match stops.iter().rposition(|(stop, _)| value >= *stop) {
                    Some(at) => stops[at].1.evaluate(properties),
                    None => base.evaluate(properties),
                }
            }
            Self::Case { arms, fallback } => {
                for (cond, output) in arms {
                    if cond.evaluate(properties)? {
                        return output.evaluate(properties);
                    }
                }
                fallback.evaluate(properties)
            }
        }
    }

    /// Every colour this expression can produce, or `None` when that is
    /// unbounded (a `["get", key]` colour string). The quantisation bound,
    /// made checkable.
    #[cfg(test)]
    fn palette_bound(&self) -> Option<usize> {
        match self {
            Self::Literal(_) => Some(1),
            Self::Get(_) => None,
            Self::Interpolate { steps, .. } => Some(1 + steps.iter().map(|&n| n as usize).sum::<usize>()),
            Self::Step { base, stops, .. } => {
                stops.iter().try_fold(base.palette_bound()?, |sum, (_, out)| Some(sum + out.palette_bound()?))
            }
            Self::Case { arms, fallback } => {
                arms.iter().try_fold(fallback.palette_bound()?, |sum, (_, out)| Some(sum + out.palette_bound()?))
            }
        }
    }
}

/// A property as a number, or MapLibre's runtime error for anything else
/// (a missing property is `null`, which `interpolate`/`step` reject: their
/// input is typed `number`).
fn number<P: Props + ?Sized>(properties: &P, key: &Key) -> Result<f64, EvalError> {
    match properties.cell(key) {
        Some(Cell::Number(value)) if !value.is_nan() => Ok(value),
        _ => Err(EvalError),
    }
}

/// MapLibre's `Interpolate.evaluate` + `interpolationFactor`, with the
/// factor snapped to the segment's step grid (see [`RAMP_STEPS`]).
///
/// COLOUR SPACE: MapLibre's plain `interpolate` blends in the colours'
/// own sRGB-ENCODED, non-premultiplied components -- style-spec
/// `src/util/interpolate.ts`, `color(from, to, t, spaceKey = 'rgb')`:
/// `array(from.rgb, to.rgb, t)`, where `Color.rgb` (`src/util/color.ts`)
/// is "with reversed alpha blending, in sRGB color space". Not linear
/// light; `interpolate-lab`/`-hcl` are the perceptual variants and are not
/// supported here. `Rgba` holds exactly those sRGB, non-premultiplied
/// components (the shader linearises afterwards), so this is a plain
/// component lerp.
fn interpolate_quantised(value: f64, base: f64, stops: &[(f64, Rgba)], steps: &[u32]) -> Rgba {
    let last = stops.len() - 1;
    if stops.len() == 1 || value <= stops[0].0 {
        return stops[0].1;
    }
    if value >= stops[last].0 {
        return stops[last].1;
    }
    // `findStopLessThanOrEqualTo`: the segment whose lower stop <= value.
    let index = stops.iter().rposition(|(stop, _)| *stop <= value).unwrap_or(0).min(last - 1);
    let (lower, from) = stops[index];
    let (upper, to) = stops[index + 1];
    let (difference, progress) = (upper - lower, value - lower);
    let t = if difference == 0.0 {
        0.0
    } else if base == 1.0 {
        progress / difference
    } else {
        (base.powf(progress) - 1.0) / (base.powf(difference) - 1.0)
    };
    let n = steps[index];
    let k = (t * f64::from(n)).round() as u32;
    if k == 0 {
        return from;
    }
    if k >= n {
        return to;
    }
    let t = (f64::from(k) / f64::from(n)) as f32;
    let lerp = |a: f32, b: f32| a + t * (b - a);
    Rgba { r: lerp(from.r, to.r), g: lerp(from.g, to.g), b: lerp(from.b, to.b), a: lerp(from.a, to.a) }
}

impl Cond {
    /// `None` for anything outside the supported set, which sends the whole
    /// expression back to the branch compiler.
    fn compile(expr: &Value) -> Option<Self> {
        if let Some(value) = expr.as_bool() {
            return Some(Self::Const(value));
        }
        let array = expr.as_array()?;
        let op = array.first()?.as_str()?;
        let args = &array[1..];
        match op {
            "has" if args.len() == 1 => Some(Self::Has(Key::new(args[0].as_str()?))),
            "!has" if args.len() == 1 => Some(Self::Not(Box::new(Self::Has(Key::new(args[0].as_str()?))))),
            "!" if args.len() == 1 => Some(Self::Not(Box::new(Self::compile(&args[0])?))),
            "all" => Some(Self::All(args.iter().map(Self::compile).collect::<Option<_>>()?)),
            "any" => Some(Self::Any(args.iter().map(Self::compile).collect::<Option<_>>()?)),
            "==" | "!=" | "<" | "<=" | ">" | ">=" if args.len() == 2 => {
                let op = match op {
                    "==" => CompareOp::Eq,
                    "!=" => CompareOp::Ne,
                    "<" => CompareOp::Lt,
                    "<=" => CompareOp::Le,
                    ">" => CompareOp::Gt,
                    _ => CompareOp::Ge,
                };
                // [op, ["get", key], literal], or mirrored.
                let (key, rhs, op) = match (get_expression_key(&args[0]), get_expression_key(&args[1])) {
                    (Some(key), None) => (key, &args[1], op),
                    (None, Some(key)) => (key, &args[0], op.mirrored()),
                    _ => return None,
                };
                let rhs = match rhs {
                    Value::Null => Literal::Null,
                    Value::Number(n) => Literal::Number(n.as_f64()?),
                    Value::String(s) => Literal::String(s.clone()),
                    Value::Bool(b) => Literal::Bool(*b),
                    _ => return None,
                };
                // An ordering against null or a bool is a MapLibre PARSE
                // error ("Expected ... number or string"); not ours to accept.
                if !matches!(op, CompareOp::Eq | CompareOp::Ne)
                    && !matches!(rhs, Literal::Number(_) | Literal::String(_))
                {
                    return None;
                }
                Some(Self::Compare { key: Key::new(key), op, rhs })
            }
            _ => None,
        }
    }

    fn reads_a_number(&self) -> bool {
        match self {
            Self::Const(_) | Self::Has(_) => false,
            Self::Not(inner) => inner.reads_a_number(),
            Self::All(all) | Self::Any(all) => all.iter().any(Self::reads_a_number),
            Self::Compare { op, rhs, .. } => {
                matches!(rhs, Literal::Number(_) | Literal::Null)
                    || !matches!(op, CompareOp::Eq | CompareOp::Ne)
            }
        }
    }

    /// MapLibre `comparison.ts`: `==`/`!=` compare any two values (a
    /// missing property is `null`, so `["==", ["get", k], null]` is the
    /// "no data" test); `<`/`<=`/`>`/`>=` on a `value`-typed `get` need
    /// both sides the same kind, number or string, and otherwise raise a
    /// runtime error -- which makes the whole expression fall back.
    fn bind(&mut self, keys: &mut Vec<String>) {
        match self {
            Self::Const(_) => {}
            Self::Has(key) | Self::Compare { key, .. } => key.bind(keys),
            Self::Not(inner) => inner.bind(keys),
            Self::All(all) | Self::Any(all) => all.iter_mut().for_each(|c| c.bind(keys)),
        }
    }

    fn evaluate<P: Props + ?Sized>(&self, properties: &P) -> Result<bool, EvalError> {
        Ok(match self {
            Self::Const(value) => *value,
            Self::Has(key) => properties.cell(key).is_some(),
            Self::Not(inner) => !inner.evaluate(properties)?,
            Self::All(all) => {
                for cond in all {
                    if !cond.evaluate(properties)? {
                        return Ok(false);
                    }
                }
                true
            }
            Self::Any(any) => {
                for cond in any {
                    if cond.evaluate(properties)? {
                        return Ok(true);
                    }
                }
                false
            }
            Self::Compare { key, op, rhs } => {
                let lhs = properties.cell(key);
                match op {
                    CompareOp::Eq | CompareOp::Ne => {
                        let equal = match (lhs, rhs) {
                            (None, Literal::Null) => true,
                            (Some(Cell::Number(a)), Literal::Number(b)) => a == *b,
                            (Some(Cell::Str(a)), Literal::String(b)) => a == b,
                            (Some(Cell::Bool(a)), Literal::Bool(b)) => a == *b,
                            _ => false,
                        };
                        equal == (*op == CompareOp::Eq)
                    }
                    _ => {
                        let ordering = match (lhs, rhs) {
                            (Some(Cell::Number(a)), Literal::Number(b)) => a.partial_cmp(b),
                            (Some(Cell::Str(a)), Literal::String(b)) => Some(a.cmp(b.as_str())),
                            _ => return Err(EvalError),
                        };
                        let Some(ordering) = ordering else { return Ok(false) };
                        match op {
                            CompareOp::Lt => ordering.is_lt(),
                            CompareOp::Le => ordering.is_le(),
                            CompareOp::Gt => ordering.is_gt(),
                            _ => ordering.is_ge(),
                        }
                    }
                }
            }
        })
    }
}

impl CompareOp {
    /// The same test with its operands swapped: `5 < x` is `x > 5`.
    fn mirrored(self) -> Self {
        match self {
            Self::Lt => Self::Gt,
            Self::Le => Self::Ge,
            Self::Gt => Self::Lt,
            Self::Ge => Self::Le,
            same => same,
        }
    }
}

/// One source's declared paint expressions.
///
/// `Default` is "nothing declared", which is every source that existed
/// before this type and every source that never calls
/// `declare_source_paint`. A default `SourcePaint` makes `build_plan` take
/// exactly the path it took before -- one op per feature, resolved paint
/// straight off the feature -- which is what keeps a resolved-paint push
/// byte-identical.
#[derive(Debug, Clone, Default)]
pub struct SourcePaint {
    line_color: Option<DataColor>,
    fill_color: Option<DataColor>,
    fill_outline_color: Option<DataColor>,
    circle_color: Option<DataColor>,
    /// `fill-opacity` / `line-opacity`: a CONSTANT multiplier on the
    /// declared colour's alpha (MapLibre multiplies `fill-opacity` into both
    /// the fill and its outline). Only meaningful beside a declared colour:
    /// a feature that falls back to its own `FeaturePaint` keeps that
    /// paint's own opacity, so declaring one without the other is refused.
    fill_opacity: Option<f32>,
    line_opacity: Option<f32>,
    /// `circle-radius` over properties: resolved into each feature's own
    /// `FeaturePaint::circle_radius` when a push is installed or a point
    /// view is built, so the plan and the bake never see an expression.
    circle_radius: Option<RadiusExpr>,
    /// `line-gradient`: the colour along each line by its progress,
    /// replacing `line-color` on a source with `lineMetrics` (MapLibre's
    /// rule: a gradient needs the metrics, and overrides the colour).
    /// `OverlayStore::declare_source_paint` refuses one on a source whose
    /// `SourceOptions::line_metrics` is off. `line-opacity` still
    /// multiplies it.
    pub line_gradient: Option<LineGradient>,
    /// A MIRROR of the source's `SourceOptions::line_metrics`, kept in
    /// step by the `OverlayStore` (`declare_source`,
    /// `declare_source_line_metrics`), so that `plan::build_plan` -- which
    /// sees this paint and not the options -- marks the source's line ops
    /// `metrics: true`. Never set by [`Self::compile`].
    pub(crate) line_metrics: bool,
    /// `circle-pitch-alignment` / `circle-pitch-scale`: how the source's
    /// circles meet a pitched map or the globe. Layer-wide (the spec makes
    /// neither data-driven); read by the point path, which flags its
    /// instances with it (`render::circles::CirclePitch::flags`).
    pub(crate) circle_pitch: crate::render::circles::CirclePitch,
    /// `line-cap` / `line-join`: how the source's lines end and turn.
    /// Layer-wide constants (MapLibre's layout keys of the same names; a
    /// story layer's paint carries them, spec §6.4). `None` keeps the
    /// overlay's own `Butt` / `Bevel` (`plan::line_op`), so a source that
    /// declares neither strokes exactly as before. Not a declaration that
    /// changes the plan's path: [`Self::is_empty`] ignores both. Read by
    /// the whole-source plan (`plan::build_plan`); a tiled source keeps
    /// the overlay's own.
    pub(crate) line_cap: Option<crate::style::LineCap>,
    pub(crate) line_join: Option<crate::style::LineJoin>,
    /// Multiplies every line width (and a dot's diameter) the whole-source
    /// plan bakes: the live map's story layers under a story frame, drawn
    /// at the story's ratio rather than the window's backing scale (spec
    /// §13, ruling R11-b; `OverlayStore::set_source_width_scale`). Never
    /// set by [`Self::compile`]; `0.0` (the default) reads as 1, and a
    /// scale of 1 bakes exactly what it baked before. Not a declaration
    /// that changes the plan's path: [`Self::is_empty`] ignores it. A tiled
    /// source's per-tile bake does not read it.
    pub(crate) width_scale: f32,
    /// The declaration exactly as [`Self::compile`] was given it, kept so
    /// a host can read back what a source was told to draw
    /// (`MapView::declared_source_paint`). Read-only; nothing draws from it.
    /// Shared, so cloning a compiled paint never copies the JSON.
    pub(crate) declared: std::sync::Arc<Map<String, Value>>,
}

impl SourcePaint {
    /// [`Self::width_scale`], 1 when unset or not a positive finite number.
    pub(crate) fn width_scale(&self) -> f32 {
        if self.width_scale.is_finite() && self.width_scale > 0.0 { self.width_scale } else { 1.0 }
    }
}

/// The width of MapLibre's gradient texture: `line-gradient` is sampled at
/// this many evenly spaced progress values (`src/webgl/draw/draw_line.ts`,
/// MapLibre GL JS, BSD-3; see `render/PROVENANCE.md`).
pub const GRADIENT_LUT_SIZE: usize = 256;

/// `line-gradient`: the expression evaluated at progress `i / 255` for
/// each of [`GRADIENT_LUT_SIZE`] entries, as straight (not premultiplied)
/// sRGB -- the same encoding a vertex's `color` carries.
///
/// MapLibre renders the same table as a 256x1 texture sampled per
/// fragment. Here it colours VERTICES (`OverlayPlan::colour_mesh`), and
/// the rasteriser interpolates between them. The overlay plan cuts each
/// line at the gradient's stops (so `interpolate ["linear"]` is exact and
/// a `step` keeps a hard edge, see [`GradientForm`]) and densifies it to
/// up to 64 parts per LineString under a per-source point budget
/// (`overlay::plan::densify_gradient_ops`). No texture, so nothing here
/// meets a WebGL2 texture limit.
#[derive(Clone, Debug, PartialEq)]
pub struct LineGradient {
    pub lut: std::sync::Arc<[Rgba; GRADIENT_LUT_SIZE]>,
    /// The top-level stops over `["line-progress"]`, as `(progress,
    /// colour AT that progress)`, ascending. Empty for a gradient whose
    /// shape is not a top-level `interpolate`/`step` over line progress
    /// (such a gradient is [`GradientForm::Sampled`] from the LUT alone).
    pub stops: std::sync::Arc<[(f32, Rgba)]>,
    /// How [`Self::at`] reads the gradient. See [`GradientForm`].
    pub form: GradientForm,
    /// A `step`'s colour below its first stop.
    pub base: Rgba,
}

/// How a [`LineGradient`] is evaluated per vertex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GradientForm {
    /// `["interpolate", ["linear"], ["line-progress"], ...]`: piecewise
    /// linear through the exact stop colours, so a vertex AT a stop (the
    /// plan cuts every line there) has exactly the stop's colour, and the
    /// rasteriser's linear interpolation between vertices is the
    /// expression itself.
    Linear,
    /// `["step", ["line-progress"], base, p1, c1, ...]`: piecewise
    /// constant, the colour of the last stop at or below the progress --
    /// MapLibre's `NEAREST` gradient texture at unlimited resolution. The
    /// plan cuts every line at `p - eps` and `p + eps`, so the edge is a
    /// blend over `2 eps` of the line only.
    Step,
    /// Anything else (`exponential`, `cubic-bezier`, nested shapes):
    /// linear between the 256 LUT entries, MapLibre's `LINEAR` texture.
    Sampled,
}

impl LineGradient {
    /// Evaluates `expr` at every LUT position through the style
    /// expression evaluator, in a scope that has line progress and nothing
    /// else -- so a `["get", ..]` (MapLibre's `line-gradient` is not
    /// data-driven) or a `["zoom"]` (nor zoom-driven) is refused, as is
    /// anything that does not evaluate to a colour. A top-level
    /// `interpolate`/`step` over `["line-progress"]` also records its
    /// stops (see [`GradientForm`]).
    pub fn compile(expr: &Value) -> Result<Self, String> {
        if mentions_zoom(expr) {
            return Err(format!("line-gradient: must not depend on zoom, got {expr}"));
        }
        // Named colours (`"blue"`) as `rgba(..)`: the evaluator's colour
        // parser has no names, and MapLibre's own gradient examples use them.
        let expr = &with_named_colours_spelled_out(expr);
        let colour_at = |progress: f64| -> Result<Rgba, String> {
            let value = crate::style_spec::expression::eval(expr, Scope::root().with_line_progress(progress), 0)
                .map_err(|error| format!("line-gradient: {error:#}"))?;
            match value {
                crate::style_spec::expression::EvalValue::Color(c) => Ok(c),
                crate::style_spec::expression::EvalValue::String(text) => crate::style_spec::color::parse_css_color(&text)
                    .map_err(|_| format!("line-gradient: {text:?} is not a colour, in {expr}")),
                other => Err(format!("line-gradient: expected a colour, got {other:?} in {expr}")),
            }
        };
        let mut lut = [Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }; GRADIENT_LUT_SIZE];
        for (i, entry) in lut.iter_mut().enumerate() {
            *entry = colour_at(i as f64 / (GRADIENT_LUT_SIZE - 1) as f64)?;
        }
        let is_progress = |v: &Value| v.as_array().is_some_and(|a| a.len() == 1 && a[0].as_str() == Some("line-progress"));
        let arr = expr.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let head = arr.first().and_then(Value::as_str);
        let (form, stop_inputs): (GradientForm, Vec<f64>) = match head {
            Some("interpolate") if arr.len() >= 5 && is_progress(&arr[2]) => {
                let linear = arr[1].as_array().and_then(|a| a.first()).and_then(Value::as_str) == Some("linear");
                let inputs = arr[3..].iter().step_by(2).filter_map(Value::as_f64).collect();
                (if linear { GradientForm::Linear } else { GradientForm::Sampled }, inputs)
            }
            Some("step") if arr.len() >= 3 && is_progress(&arr[1]) => {
                (GradientForm::Step, arr[3..].iter().step_by(2).filter_map(Value::as_f64).collect())
            }
            _ => (GradientForm::Sampled, Vec::new()),
        };
        let mut stops = Vec::with_capacity(stop_inputs.len());
        for p in stop_inputs {
            stops.push((p as f32, colour_at(p)?));
        }
        let base = match (form, stops.first()) {
            (GradientForm::Step, Some(&(first, _))) => colour_at(f64::from(first) - 1.0)?,
            _ => lut[0],
        };
        Ok(Self { lut: std::sync::Arc::new(lut), stops: stops.into(), form, base })
    }

    /// The progresses the plan must cut every line at so that the vertex
    /// colours reproduce this gradient's shape: each stop strictly inside
    /// `(0, 1)` (a `step` is cut on both sides of each, see
    /// [`Self::stepped`]).
    pub fn interior_stops(&self) -> impl Iterator<Item = f32> + '_ {
        self.stops.iter().map(|(p, _)| *p).filter(|p| *p > 0.0 && *p < 1.0)
    }

    /// The gradient at `progress` (clamped to `0..=1`): exact through the
    /// stops for [`GradientForm::Linear`] and [`GradientForm::Step`],
    /// linear between the two nearest LUT entries (the texture's `LINEAR`
    /// filter) for [`GradientForm::Sampled`].
    pub fn at(&self, progress: f32) -> Rgba {
        let p = progress.clamp(0.0, 1.0);
        let mix = |a: Rgba, b: Rgba, t: f32| {
            let m = |x: f32, y: f32| x + (y - x) * t;
            Rgba { r: m(a.r, b.r), g: m(a.g, b.g), b: m(a.b, b.b), a: m(a.a, b.a) }
        };
        match self.form {
            GradientForm::Step if !self.stops.is_empty() => {
                self.stops.iter().take_while(|(at, _)| *at <= p).last().map_or(self.base, |(_, c)| *c)
            }
            GradientForm::Linear if !self.stops.is_empty() => {
                let (first, last) = (self.stops[0], self.stops[self.stops.len() - 1]);
                if p <= first.0 {
                    return first.1;
                }
                if p >= last.0 {
                    return last.1;
                }
                let Some(k) = self.stops.windows(2).find(|w| p < w[1].0) else { return last.1 };
                let span = k[1].0 - k[0].0;
                if span <= 0.0 {
                    return k[1].1;
                }
                mix(k[0].1, k[1].1, (p - k[0].0) / span)
            }
            _ => {
                let x = p * (GRADIENT_LUT_SIZE - 1) as f32;
                let i = (x.floor() as usize).min(GRADIENT_LUT_SIZE - 2);
                mix(self.lut[i], self.lut[i + 1], x - i as f32)
            }
        }
    }

    /// Whether this gradient has hard edges (`step`), which the plan cuts
    /// at both sides of each stop.
    pub fn stepped(&self) -> bool {
        self.form == GradientForm::Step
    }
}

/// `expr` with every string that is a CSS named colour -- other than an
/// operator name at the head of an array -- replaced by its `rgba(..)`.
/// Used for `line-gradient` here, and by `story::overlay_layers` for every
/// colour key of a story overlay layer's paint (T6 review F6).
pub(crate) fn with_named_colours_spelled_out(expr: &Value) -> Value {
    match expr {
        Value::String(text) => match crate::style_spec::color::css_named_color(text) {
            Some(c) => Value::String(format!("rgba({},{},{},{})", c.r * 255.0, c.g * 255.0, c.b * 255.0, c.a)),
            None => expr.clone(),
        },
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| if i == 0 && item.is_string() { item.clone() } else { with_named_colours_spelled_out(item) })
                .collect(),
        ),
        _ => expr.clone(),
    }
}

/// Test access to [`with_named_colours_spelled_out`].
#[cfg(test)]
pub(crate) mod tests_support {
    pub(crate) fn spelled(expr: &serde_json::Value) -> serde_json::Value {
        super::with_named_colours_spelled_out(expr)
    }
}

/// Whether `expr` names `["zoom"]` anywhere.
fn mentions_zoom(expr: &Value) -> bool {
    match expr {
        Value::Array(items) => items.first().and_then(Value::as_str) == Some("zoom") || items.iter().any(mentions_zoom),
        Value::Object(map) => map.values().any(mentions_zoom),
        _ => false,
    }
}

/// A data-driven `circle-radius`: the shapes a cluster layer sizes itself
/// with. Zoom-free, like every overlay data expression.
#[derive(Debug, Clone)]
enum RadiusExpr {
    Const(f32),
    Get(String),
    /// `["step", ["get", k], base, stop, value, ...]`
    Step { key: String, base: f32, stops: Vec<(f64, f32)> },
    /// `["interpolate", ["linear"], ["get", k], stop, value, ...]`
    Linear { key: String, stops: Vec<(f64, f32)> },
    /// `["match", ["get", k], label, value, ..., fallback]`
    Match { key: String, arms: Vec<(TagValue, f32)>, fallback: f32 },
}

impl RadiusExpr {
    fn compile(expr: &Value) -> Result<Self, String> {
        let bad = |why: &str| format!("circle-radius: {why}, got {expr}");
        let num = |v: &Value| v.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as f32);
        if let Some(n) = expr.as_f64() {
            return num(expr).map(Self::Const).ok_or_else(|| bad("must be a non-negative number") + &format!(" ({n})"));
        }
        let Some(arr) = expr.as_array() else { return Err(bad("expected a number or an expression")) };
        let get_key = |v: &Value| -> Option<String> {
            let a = v.as_array()?;
            (a.len() == 2 && a[0].as_str() == Some("get")).then(|| a[1].as_str().map(str::to_string)).flatten()
        };
        let pairs = |rest: &[Value]| -> Result<Vec<(f64, f32)>, String> {
            if !rest.len().is_multiple_of(2) {
                return Err(bad("stops must come in input/output pairs"));
            }
            let mut out: Vec<(f64, f32)> = Vec::new();
            for pair in rest.chunks(2) {
                let input = pair[0].as_f64().ok_or_else(|| bad("a stop input must be a number"))?;
                let output = num(&pair[1]).ok_or_else(|| bad("a stop output must be a non-negative number"))?;
                if out.last().is_some_and(|l| l.0 >= input) {
                    return Err(bad("stop inputs must be strictly ascending"));
                }
                out.push((input, output));
            }
            Ok(out)
        };
        match arr.first().and_then(Value::as_str) {
            Some("get") => get_key(expr).map(Self::Get).ok_or_else(|| bad("[\"get\", key] takes one key")),
            Some("step") if arr.len() >= 3 => {
                let key = get_key(&arr[1]).ok_or_else(|| bad("step input must be [\"get\", key]"))?;
                let base = num(&arr[2]).ok_or_else(|| bad("step base must be a non-negative number"))?;
                Ok(Self::Step { key, base, stops: pairs(&arr[3..])? })
            }
            Some("interpolate") if arr.len() >= 5 => {
                if arr[1].as_array().and_then(|a| a.first()).and_then(Value::as_str) != Some("linear") {
                    return Err(bad("only [\"linear\"] interpolation is supported"));
                }
                let key = get_key(&arr[2]).ok_or_else(|| bad("interpolate input must be [\"get\", key]"))?;
                Ok(Self::Linear { key, stops: pairs(&arr[3..])? })
            }
            Some("match") if arr.len() >= 5 && arr.len() % 2 == 1 => {
                let key = get_key(&arr[1]).ok_or_else(|| bad("match input must be [\"get\", key]"))?;
                let fallback = num(&arr[arr.len() - 1]).ok_or_else(|| bad("match fallback must be a number"))?;
                let mut arms = Vec::new();
                for pair in arr[2..arr.len() - 1].chunks(2) {
                    let label = match &pair[0] {
                        Value::String(s) => TagValue::String(s.clone()),
                        Value::Number(n) => TagValue::Number(n.as_f64().unwrap_or(f64::NAN)),
                        Value::Bool(b) => TagValue::Bool(*b),
                        _ => return Err(bad("match labels must be literals")),
                    };
                    arms.push((label, num(&pair[1]).ok_or_else(|| bad("match outputs must be numbers"))?));
                }
                Ok(Self::Match { key, arms, fallback })
            }
            _ => Err(bad("supported: a number, get, step or linear interpolate over [\"get\", key], match")),
        }
    }

    fn eval(&self, row: &FeatureProperties) -> Option<f32> {
        let number = |key: &str| match row.get(key) {
            Some(TagValue::Number(n)) => Some(*n),
            _ => None,
        };
        match self {
            Self::Const(r) => Some(*r),
            Self::Get(key) => number(key).filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as f32),
            Self::Step { key, base, stops } => {
                let x = number(key)?;
                Some(stops.iter().take_while(|(at, _)| x >= *at).last().map_or(*base, |(_, v)| *v))
            }
            Self::Linear { key, stops } => {
                let x = number(key)?;
                let (first, last) = (stops[0], stops[stops.len() - 1]);
                if x <= first.0 {
                    return Some(first.1);
                }
                if x >= last.0 {
                    return Some(last.1);
                }
                let k = stops.windows(2).find(|w| x < w[1].0)?;
                let t = ((x - k[0].0) / (k[1].0 - k[0].0)) as f32;
                Some(k[0].1 + (k[1].1 - k[0].1) * t)
            }
            Self::Match { key, arms, fallback } => {
                let value = row.get(key);
                Some(arms.iter().find(|(label, _)| Some(label) == value).map_or(*fallback, |(_, v)| *v))
            }
        }
    }
}

impl SourcePaint {
    /// Whether `circle-radius` is declared as an expression.
    pub(crate) fn circle_radius_declared(&self) -> bool {
        self.circle_radius.is_some()
    }

    /// The declared `circle-pitch-alignment` / `circle-pitch-scale`
    /// (the spec's defaults, `viewport` / `map`, when undeclared).
    pub(crate) fn circle_pitch(&self) -> crate::render::circles::CirclePitch {
        self.circle_pitch
    }

    /// The declared `circle-color` for row `row` of `table`: `None` when
    /// none is declared or no branch matched (the feature keeps its own).
    /// What a point's hover ring redraws it in; the columnar path, one row.
    pub(crate) fn circle_color_at(&self, table: &PropertyTable, row: usize) -> Option<Rgba> {
        self.circle_color.as_ref().and_then(|color| color.resolve_rows(table, Some(&[row])).into_iter().next().flatten())
    }

    /// The declared `circle-radius` for one feature's properties: `None`
    /// when none is declared or the expression has no answer for the row
    /// (the feature keeps its own radius).
    pub(crate) fn circle_radius(&self, row: &FeatureProperties) -> Option<f32> {
        self.circle_radius.as_ref().and_then(|e| e.eval(row))
    }

    /// Compiles a caller's style-spec paint object.
    ///
    /// The keys are Mapbox's own paint property names, exactly as
    /// `FeaturePaint::from_properties` reads them and exactly as a caller's
    /// production style writes them, so `{"fill-color": ["match", ["get",
    /// "route"], "12", "#e6194b", "#888888"]}` means here what it means
    /// there.
    ///
    /// AN UNRECOGNISED KEY IS AN ERROR, which is the opposite of
    /// `FeaturePaint::from_properties`'s rule and deliberately so. There,
    /// the object is the caller's GeoJSON `properties` -- their own data
    /// lives in it, so an unknown key must be ignored. Here the object is a
    /// paint declaration and nothing else, so `{"fill-colour": ...}` (or
    /// `line-width`, which is not data-driven) is a caller asking for
    /// something that will silently not happen. Saying so is the only useful
    /// direction.
    pub fn compile(paint: &Map<String, Value>) -> Result<Self, String> {
        let mut built = Self::default();
        for (key, expr) in paint {
            if matches!(key.as_str(), "fill-opacity" | "line-opacity") {
                let opacity = expr
                    .as_f64()
                    .filter(|o| (0.0..=1.0).contains(o))
                    .ok_or_else(|| {
                        format!("{key}: must be a constant number in 0..=1 -- a data-driven opacity is not supported")
                    })? as f32;
                if key == "fill-opacity" {
                    built.fill_opacity = Some(opacity);
                } else {
                    built.line_opacity = Some(opacity);
                }
                continue;
            }
            if key == "circle-radius" {
                built.circle_radius = Some(RadiusExpr::compile(expr)?);
                continue;
            }
            if key == "line-gradient" {
                built.line_gradient = Some(LineGradient::compile(expr)?);
                continue;
            }
            if key == "line-cap" {
                built.line_cap = Some(match expr.as_str() {
                    Some("butt") => crate::style::LineCap::Butt,
                    Some("round") => crate::style::LineCap::Round,
                    Some("square") => crate::style::LineCap::Square,
                    _ => return Err(format!("line-cap: must be \"butt\", \"round\" or \"square\", got {expr}")),
                });
                continue;
            }
            if key == "line-join" {
                built.line_join = Some(match expr.as_str() {
                    Some("bevel") => crate::style::LineJoin::Bevel,
                    Some("round") => crate::style::LineJoin::Round,
                    Some("miter") => crate::style::LineJoin::Miter,
                    _ => return Err(format!("line-join: must be \"bevel\", \"round\" or \"miter\", got {expr}")),
                });
                continue;
            }
            if matches!(key.as_str(), "circle-pitch-alignment" | "circle-pitch-scale") {
                // Both properties take `map` or `viewport` and nothing else.
                let map = match expr.as_str() {
                    Some("map") => true,
                    Some("viewport") => false,
                    _ => return Err(format!("{key}: must be \"map\" or \"viewport\", got {expr}")),
                };
                if key == "circle-pitch-alignment" {
                    built.circle_pitch.align_map = map;
                } else {
                    built.circle_pitch.scale_map = map;
                }
                continue;
            }
            let slot = match key.as_str() {
                "line-color" => ColorSlot::Line,
                "fill-color" => ColorSlot::Fill,
                "fill-outline-color" => ColorSlot::FillOutline,
                "circle-color" => ColorSlot::Circle,
                other => {
                    return Err(format!(
                        "{other:?} is not a data-driven overlay paint property -- \
                         only line-color, fill-color, fill-outline-color, circle-color, circle-radius \
                         and line-gradient can be expressions \
                         (with constant fill-opacity / line-opacity beside them) \
                         over a feature's properties today; everything else belongs in the \
                         feature's own FeaturePaint"
                    ))
                }
            };
            let color = DataColor::compile(slot, expr)?;
            match slot {
                ColorSlot::Line => built.line_color = Some(color),
                ColorSlot::Fill => built.fill_color = Some(color),
                ColorSlot::FillOutline => built.fill_outline_color = Some(color),
                ColorSlot::Circle => built.circle_color = Some(color),
            }
        }
        if built.fill_opacity.is_some() && built.fill_color.is_none() && built.fill_outline_color.is_none() {
            return Err("fill-opacity: declare fill-color (or fill-outline-color) beside it -- a source opacity \
                        scales the declared colour, and a feature's own FeaturePaint carries its own fill-opacity"
                .into());
        }
        if built.line_opacity.is_some() && built.line_color.is_none() && built.line_gradient.is_none() {
            return Err("line-opacity: declare line-color beside it -- a source opacity scales the declared \
                        colour, and a feature's own FeaturePaint carries its own line-opacity"
                .into());
        }
        built.declared = std::sync::Arc::new(paint.clone());
        Ok(built)
    }

    /// What a `line-gradient` colour's alpha is multiplied by BESIDE the
    /// vertex's own alpha: the source's `line-opacity` -- unless a
    /// `line-color` is declared too, whose resolved colour (the vertex's
    /// alpha) already carries it.
    pub(crate) fn gradient_opacity(&self) -> f32 {
        if self.line_color.is_some() {
            1.0
        } else {
            self.line_opacity.unwrap_or(1.0)
        }
    }

    /// True when nothing is declared, and therefore when `build_plan` must
    /// take the original per-feature path.
    pub(crate) fn is_empty(&self) -> bool {
        self.line_color.is_none()
            && self.fill_color.is_none()
            && self.fill_outline_color.is_none()
            && self.circle_color.is_none()
    }

    /// This source's colour for one slot and one feature, falling back to
    /// whatever the feature's own resolved paint holds.
    ///
    /// The fallback is what makes a declaration additive: a source that
    /// declares only `fill-color` leaves its lines and dots painted exactly
    /// as their `FeaturePaint` says, and a feature whose property matches no
    /// branch keeps its own colour rather than vanishing or turning black.
    /// `#[cfg(test)]`: the draw path resolves through
    /// [`Self::resolve_rows`] and a `plan::ResolvedColors`, because the fallback
    /// is the one part of the answer that cannot be memoised. This is the
    /// same composition, spelled out once so the tests below read as
    /// "what colour does this feature get" rather than as a fold.
    #[cfg(test)]
    pub(crate) fn color(
        &self,
        slot: ColorSlot,
        properties: &FeatureProperties,
        fallback: Rgba,
    ) -> Rgba {
        self.resolve(slot, properties).unwrap_or(fallback)
    }

    /// What the expression alone says, with no fallback folded in --
    /// `None` for an undeclared slot and for a feature no branch matched.
    ///
    /// Separate from [`Self::color`] because it is the CACHEABLE half:
    /// two vehicles with equal properties can carry different fallback
    /// colours of their own. The per-map reference for
    /// [`Self::resolve_rows`], which is what the draw path runs.
    #[cfg(test)]
    pub(crate) fn resolve(
        &self,
        slot: ColorSlot,
        properties: &FeatureProperties,
    ) -> Option<Rgba> {
        let declared = match slot {
            ColorSlot::Line => &self.line_color,
            ColorSlot::Fill => &self.fill_color,
            ColorSlot::FillOutline => &self.fill_outline_color,
            ColorSlot::Circle => &self.circle_color,
        };
        let opacity = match slot {
            ColorSlot::Fill | ColorSlot::FillOutline => self.fill_opacity,
            ColorSlot::Line => self.line_opacity,
            ColorSlot::Circle => None,
        };
        let color = declared.as_ref().and_then(|color| color.resolve(properties))?;
        Some(match opacity {
            Some(opacity) => Rgba { a: color.a * opacity, ..color },
            None => color,
        })
    }

    /// [`Self::resolve`] for every row of `table`, or `None` when `slot` is
    /// undeclared. What `plan::classify` and `plan::build_plan_merged`
    /// read, by `OverlayFeature::index`.
    pub(crate) fn resolve_rows(&self, slot: ColorSlot, table: &PropertyTable) -> Option<Vec<Option<Rgba>>> {
        self.resolve_rows_at(slot, table, None)
    }

    /// [`Self::resolve_rows`] for just the rows in `only` (entry `i` is row
    /// `only[i]`), or every row when `None`.
    pub(crate) fn resolve_rows_at(
        &self,
        slot: ColorSlot,
        table: &PropertyTable,
        only: Option<&[usize]>,
    ) -> Option<Vec<Option<Rgba>>> {
        let (declared, opacity) = self.slot(slot);
        let mut colors = declared.as_ref()?.resolve_rows(table, only);
        if let Some(opacity) = opacity {
            for color in colors.iter_mut().flatten() {
                color.a *= opacity;
            }
        }
        Some(colors)
    }

    fn slot(&self, slot: ColorSlot) -> (&Option<DataColor>, Option<f32>) {
        match slot {
            ColorSlot::Line => (&self.line_color, self.line_opacity),
            ColorSlot::Fill => (&self.fill_color, self.fill_opacity),
            ColorSlot::FillOutline => (&self.fill_outline_color, self.fill_opacity),
            ColorSlot::Circle => (&self.circle_color, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gradient_lut_matches_the_expression_at_its_ends_and_middle() {
        let g = LineGradient::compile(&serde_json::json!(["interpolate", ["linear"], ["line-progress"], 0, "blue", 1, "red"])).unwrap();
        assert!((g.at(0.0).b - 1.0).abs() < 1e-3 && (g.at(1.0).r - 1.0).abs() < 1e-3);
        assert!((g.at(0.5).r - 0.5).abs() < 0.01);
        // Out of range clamps; entries are exact at i / 255.
        assert_eq!(g.at(-1.0), g.lut[0]);
        assert_eq!(g.at(2.0), g.lut[GRADIENT_LUT_SIZE - 1]);
        // Not data-driven, not zoom-driven, and a colour.
        assert!(LineGradient::compile(&serde_json::json!(["get", "c"])).is_err());
        assert!(LineGradient::compile(&serde_json::json!(["interpolate", ["linear"], ["zoom"], 0, "blue", 1, "red"])).is_err());
        assert!(LineGradient::compile(&serde_json::json!(["line-progress"])).is_err());
    }

    #[test]
    fn line_gradient_without_line_metrics_is_refused() {
        let gradient = paint_map_of(r#"{"line-gradient":["interpolate",["linear"],["line-progress"],0,"blue",1,"red"]}"#);
        let mut store = crate::overlay::OverlayStore::new();
        let error = store.declare_source_paint("route", &gradient).unwrap_err();
        assert!(error.to_string().contains("lineMetrics"), "{error}");
        assert!(error.to_string().contains("line-gradient needs lineMetrics"), "{error}");
        // With the metrics declared first, it is accepted, and line-opacity beside it too.
        store.declare_source_line_metrics("route", true).unwrap();
        store.declare_source_paint("route", &gradient).unwrap();
        let with_opacity = paint_map_of(
            r#"{"line-gradient":["interpolate",["linear"],["line-progress"],0,"blue",1,"red"],"line-opacity":0.5}"#,
        );
        store.declare_source_paint("route", &with_opacity).unwrap();
        // And turning the metrics off under a declared gradient is refused too.
        assert!(store.declare_source_line_metrics("route", false).is_err());
    }

    fn paint_map_of(json: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    fn paint(json: &str) -> Result<SourcePaint, String> {
        SourcePaint::compile(&serde_json::from_str(json).unwrap())
    }

    fn properties(pairs: &[(&str, &str)]) -> FeatureProperties {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), TagValue::String(v.to_string())))
            .collect()
    }

    const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

    /// THE shape this module exists for: the property itself holds the
    /// colour, so twelve routes need no twelve-branch expression and no
    /// twelve features.
    #[test]
    fn a_get_expression_reads_the_colour_out_of_the_feature() {
        let compiled = paint(r#"{"fill-color":["get","route_colour"]}"#).unwrap();
        let red = compiled.color(
            ColorSlot::Fill,
            &properties(&[("route_colour", "#ff0000")]),
            BLACK,
        );
        assert_eq!(red, crate::style_spec::color::parse_css_color("#ff0000").unwrap());
        let green = compiled.color(
            ColorSlot::Fill,
            &properties(&[("route_colour", "#00ff00")]),
            BLACK,
        );
        assert_eq!(green, crate::style_spec::color::parse_css_color("#00ff00").unwrap());
    }

    /// A feature with no such property, or an unparseable one, keeps its own
    /// colour. Not black, and not dropped -- see `DataColor::resolve`.
    #[test]
    fn a_missing_or_malformed_property_falls_back_to_the_features_own_colour() {
        let compiled = paint(r#"{"fill-color":["get","route_colour"]}"#).unwrap();
        assert_eq!(compiled.color(ColorSlot::Fill, &properties(&[]), BLACK), BLACK);
        assert_eq!(
            compiled.color(ColorSlot::Fill, &properties(&[("route_colour", "nope")]), BLACK),
            BLACK
        );
    }

    /// The other half: a `match` over a property, which is what a
    /// production mapbox-gl style actually contains. Compiled by the SAME
    /// `compile_zoom_value` every basemap layer goes through.
    #[test]
    fn a_match_over_a_property_selects_the_right_branch() {
        let compiled = paint(
            r##"{"line-color":["match",["get","route"],"12","#e6194b","17","#3cb44b","#888888"]}"##,
        )
        .unwrap();
        let at = |route: &str| compiled.color(ColorSlot::Line, &properties(&[("route", route)]), BLACK);
        assert_eq!(at("12"), crate::style_spec::color::parse_css_color("#e6194b").unwrap());
        assert_eq!(at("17"), crate::style_spec::color::parse_css_color("#3cb44b").unwrap());
        assert_eq!(at("99"), crate::style_spec::color::parse_css_color("#888888").unwrap());
    }

    /// A literal colour is a legal declaration and applies to every
    /// feature -- which is how a caller says "one colour for this whole
    /// source" without writing it into every feature's paint.
    #[test]
    fn a_literal_colour_applies_to_every_feature() {
        let compiled = paint(r##"{"circle-color":"#123456"}"##).unwrap();
        assert_eq!(
            compiled.color(ColorSlot::Circle, &properties(&[]), BLACK),
            crate::style_spec::color::parse_css_color("#123456").unwrap()
        );
    }

    /// Only the slots declared are overridden; the rest keep the feature's
    /// own paint. This is what makes a declaration additive rather than a
    /// replacement.
    #[test]
    fn an_undeclared_slot_keeps_the_features_own_colour() {
        let compiled = paint(r#"{"fill-color":["get","c"]}"#).unwrap();
        assert_eq!(compiled.color(ColorSlot::Line, &properties(&[("c", "#ff0000")]), BLACK), BLACK);
        assert_eq!(
            compiled.color(ColorSlot::Circle, &properties(&[("c", "#ff0000")]), BLACK),
            BLACK
        );
    }

    #[test]
    fn nothing_declared_is_empty_and_something_declared_is_not() {
        assert!(paint("{}").unwrap().is_empty());
        assert!(!paint(r#"{"fill-color":["get","c"]}"#).unwrap().is_empty());
    }

    /// THE guard requirement 2 turns on. A zoom-varying colour would have
    /// to be baked as a bracketed pair, which would drag the source onto
    /// the same ramp agreement `line-width` is subject to and could make a
    /// source declared zoom-independent stop being it. Refused at
    /// declaration time, so `plan::zoom_dependence` needs no change and
    /// stays exact.
    #[test]
    fn a_zoom_varying_colour_is_refused() {
        let error =
            paint(r##"{"fill-color":["interpolate",["linear"],["zoom"],8,"#ff0000",18,"#0000ff"]}"##)
                .unwrap_err();
        assert!(error.contains("fill-color"), "error was {error}");
        assert!(error.contains("ZOOM"), "error was {error}");
    }

    /// A `step` on zoom is the other zoom shape, and is refused for the
    /// same reason.
    #[test]
    fn a_zoom_step_colour_is_refused() {
        let error = paint(r##"{"line-color":["step",["zoom"],"#ff0000",14,"#0000ff"]}"##).unwrap_err();
        assert!(error.contains("line-color"), "error was {error}");
        assert!(error.contains("ZOOM"), "error was {error}");
    }

    /// An unknown key is refused rather than ignored -- see
    /// `SourcePaint::compile` for why this rule is the opposite of
    /// `FeaturePaint::from_properties`'s.
    #[test]
    fn an_unknown_paint_key_is_refused() {
        let error = paint(r#"{"line-width":4}"#).unwrap_err();
        assert!(error.contains("line-width"), "error was {error}");
        let error = paint(r##"{"fill-colour":"#ff0000"}"##).unwrap_err();
        assert!(error.contains("fill-colour"), "error was {error}");
    }

    #[test]
    fn a_malformed_colour_expression_is_refused() {
        assert!(paint(r#"{"fill-color":"rebeccapurpel"}"#).is_err());
        assert!(paint(r#"{"fill-color":42}"#).is_err());
    }

    /// `["get", key, object]` is a nested-object read, which a flat
    /// property map has no equivalent for. It must not be taken as a plain
    /// read of `key`.
    #[test]
    fn a_three_argument_get_is_not_treated_as_a_plain_property_read() {
        let error = paint(r#"{"fill-color":["get","c",["properties"]]}"#).unwrap_err();
        assert!(error.contains("fill-color"), "error was {error}");
    }

    /// `no_properties` hands out ONE allocation for the whole process, which
    /// is the entire cost of the new field to a caller who never declares a
    /// data-driven paint.
    #[test]
    fn the_empty_property_map_is_shared() {
        let one = no_properties();
        let two = no_properties();
        assert!(Arc::ptr_eq(&one, &two));
        assert!(one.is_empty());
    }

    /// The whitelist walk, exercised on both sides: a tag-only filter is
    /// zoom-free, a `ZoomStep` is not, and nesting one inside a combinator
    /// does not hide it.
    #[test]
    fn the_zoom_free_walk_sees_a_zoom_filter_however_deeply_nested() {
        let tag = Filter::Equals { key: "a".into(), value: "b".into() };
        assert!(filter_is_zoom_free(&tag));
        let zoomed =
            Filter::ZoomStep { base: Box::new(Filter::Any), stops: vec![(12.0, Filter::Never)] };
        assert!(!filter_is_zoom_free(&zoomed));
        assert!(!filter_is_zoom_free(&Filter::All(vec![tag.clone(), zoomed.clone()])));
        assert!(!filter_is_zoom_free(&Filter::Not(Box::new(zoomed.clone()))));
        assert!(!filter_is_zoom_free(&Filter::Case {
            arms: vec![(tag.clone(), zoomed)],
            fallback: Box::new(Filter::Any),
        }));
        assert!(filter_is_zoom_free(&Filter::All(vec![tag])));
    }

    // --- numeric colour expressions -------------------------------------

    fn numbers(pairs: &[(&str, f64)]) -> FeatureProperties {
        pairs.iter().map(|(k, v)| (k.to_string(), TagValue::Number(*v))).collect()
    }

    fn css(text: &str) -> Rgba {
        crate::style_spec::color::parse_css_color(text).unwrap()
    }

    fn close(a: Rgba, b: Rgba) -> bool {
        (a.r - b.r).abs() < 1e-6 && (a.g - b.g).abs() < 1e-6 && (a.b - b.b).abs() < 1e-6 && (a.a - b.a).abs() < 1e-6
    }

    const RED_TO_BLUE: &str =
        r##"{"fill-color":["interpolate",["linear"],["get","v"],0,"#ff0000",100,"#0000ff",200,"#00ff00"]}"##;

    /// At a stop the stop's own colour, bit for bit (the per-segment grid
    /// puts every stop on a step); below the first and above the last, the
    /// end colours -- MapLibre `Interpolate.evaluate`'s two clamps.
    #[test]
    fn interpolate_is_exact_at_the_stops_and_clamps_beyond_them() {
        let compiled = paint(RED_TO_BLUE).unwrap();
        let at = |v: f64| compiled.color(ColorSlot::Fill, &numbers(&[("v", v)]), BLACK);
        assert_eq!(at(0.0), css("#ff0000"));
        assert_eq!(at(100.0), css("#0000ff"));
        assert_eq!(at(200.0), css("#00ff00"));
        assert_eq!(at(-1e9), css("#ff0000"));
        assert_eq!(at(1e9), css("#00ff00"));
    }

    /// Between stops: a component lerp of the sRGB-encoded colours
    /// (MapLibre's `rgb` space), snapped to the segment's grid of 127
    /// steps (255 * 100/200, floored). 50 is t = 0.5 -> step 64 of 127 (round
    /// half away from zero on 63.5).
    #[test]
    fn interpolate_between_stops_lerps_in_srgb_and_snaps_to_the_grid() {
        let compiled = paint(RED_TO_BLUE).unwrap();
        let mid = compiled.color(ColorSlot::Fill, &numbers(&[("v", 50.0)]), BLACK);
        let t = 64.0 / 127.0;
        assert!(close(mid, Rgba { r: 1.0 - t, g: 0.0, b: t, a: 1.0 }), "got {mid:?}");
        // Off the grid by less than half a step: same colour.
        let near = compiled.color(ColorSlot::Fill, &numbers(&[("v", 50.2)]), BLACK);
        assert_eq!(mid, near);
        // Exponential base, MapLibre's `exponentialInterpolation`.
        let exp = paint(r##"{"fill-color":["interpolate",["exponential",2],["get","v"],0,"#000000",8,"#ffffff"]}"##)
            .unwrap();
        let four = exp.color(ColorSlot::Fill, &numbers(&[("v", 4.0)]), BLACK);
        let t = (2f64.powf(4.0) - 1.0) / (2f64.powf(8.0) - 1.0); // 15/255
        let snapped = (t * 255.0).round() / 255.0;
        assert!((f64::from(four.r) - snapped).abs() < 1e-6, "got {four:?}");
    }

    /// `step`: the base below the first stop, and a stop's output from
    /// exactly that stop upwards (`findStopLessThanOrEqualTo`).
    #[test]
    fn step_switches_exactly_at_each_threshold() {
        let compiled =
            paint(r##"{"line-color":["step",["get","v"],"#111111",10,"#222222",20,"#333333"]}"##).unwrap();
        let at = |v: f64| compiled.color(ColorSlot::Line, &numbers(&[("v", v)]), BLACK);
        assert_eq!(at(-5.0), css("#111111"));
        assert_eq!(at(9.999), css("#111111"));
        assert_eq!(at(10.0), css("#222222"));
        assert_eq!(at(19.0), css("#222222"));
        assert_eq!(at(20.0), css("#333333"));
        assert_eq!(at(1e12), css("#333333"));
    }

    /// `case` over numeric comparisons, with `has` and `== null` as the
    /// two spellings of "no data" -- both take the grey arm.
    #[test]
    fn case_with_numeric_conditions_and_missing_checks() {
        for guard in [r#"["!",["has","v"]]"#, r#"["==",["get","v"],null]"#] {
            let json = format!(
                r##"{{"fill-color":["case",{guard},"#9e9e9e",[">=",["get","v"],100],"#ff0000",[">=",["get","v"],50],"#00ff00","#0000ff"]}}"##
            );
            let compiled = paint(&json).unwrap();
            let at = |p: &FeatureProperties| compiled.color(ColorSlot::Fill, p, BLACK);
            assert_eq!(at(&numbers(&[])), css("#9e9e9e"), "{guard}");
            assert_eq!(at(&numbers(&[("v", 150.0)])), css("#ff0000"));
            assert_eq!(at(&numbers(&[("v", 100.0)])), css("#ff0000"));
            assert_eq!(at(&numbers(&[("v", 60.0)])), css("#00ff00"));
            assert_eq!(at(&numbers(&[("v", 1.0)])), css("#0000ff"));
        }
        // A mirrored comparison reads the same: 50 < v is v > 50.
        let mirrored = paint(r##"{"fill-color":["case",["<",50,["get","v"]],"#ff0000","#0000ff"]}"##).unwrap();
        assert_eq!(mirrored.color(ColorSlot::Fill, &numbers(&[("v", 51.0)]), BLACK), css("#ff0000"));
        assert_eq!(mirrored.color(ColorSlot::Fill, &numbers(&[("v", 50.0)]), BLACK), css("#0000ff"));
    }

    /// The demo's own expression: a missing income falls to the case's grey
    /// fallback; an UNGUARDED ramp with no number to read is MapLibre's
    /// runtime error and falls to the feature's own colour (the property
    /// default), as does an ordering comparison on a missing property.
    #[test]
    fn a_missing_property_falls_to_the_fallback() {
        let guarded = paint(
            r##"{"fill-color":["case",["has","v"],["interpolate",["linear"],["get","v"],0,"#ff0000",1,"#0000ff"],"#9e9e9e"]}"##,
        )
        .unwrap();
        assert_eq!(guarded.color(ColorSlot::Fill, &numbers(&[]), BLACK), css("#9e9e9e"));
        assert_eq!(guarded.color(ColorSlot::Fill, &numbers(&[("v", 1.0)]), BLACK), css("#0000ff"));

        let bare = paint(RED_TO_BLUE).unwrap();
        assert_eq!(bare.color(ColorSlot::Fill, &numbers(&[]), BLACK), BLACK);
        assert_eq!(bare.color(ColorSlot::Fill, &properties(&[("v", "12")]), BLACK), BLACK, "a string is not a number");
        let ordering = paint(r##"{"fill-color":["case",[">=",["get","v"],1],"#ff0000","#0000ff"]}"##).unwrap();
        assert_eq!(ordering.color(ColorSlot::Fill, &numbers(&[]), BLACK), BLACK);
    }

    /// THE bound the paint classes rest on: however many distinct inputs, a
    /// ramp produces at most `1 + sum(n_i)` colours, `<= RAMP_STEPS` for
    /// evenly spread stops -- and the bound is really met, not just stated.
    #[test]
    fn interpolate_output_is_quantised_to_a_bounded_palette() {
        let compiled = paint(RED_TO_BLUE).unwrap();
        let Some(DataColor::Numeric { expr, .. }) = &compiled.fill_color else { panic!("not numeric") };
        let bound = expr.palette_bound().unwrap();
        assert_eq!(bound, 1 + 127 + 127);
        assert!(bound <= RAMP_STEPS as usize);
        let mut seen = std::collections::HashSet::new();
        for i in 0..=200_000 {
            let c = compiled.color(ColorSlot::Fill, &numbers(&[("v", f64::from(i) / 1000.0)]), BLACK);
            seen.insert([c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()]);
        }
        assert_eq!(seen.len(), bound, "every step is reachable and nothing else is");
    }

    /// A zoom `interpolate` still reaches the branch compiler's ZOOM refusal
    /// (its input is not a `get`), and a malformed property ramp is reported
    /// as itself.
    #[test]
    fn malformed_numeric_expressions_are_refused() {
        assert!(paint(r##"{"fill-color":["interpolate",["linear"],["get","v"],10,"#f00",5,"#00f"]}"##)
            .unwrap_err()
            .contains("ascending"));
        assert!(paint(r##"{"fill-color":["interpolate",["cubic-bezier",0,0,1,1],["get","v"],0,"#f00",5,"#00f"]}"##)
            .is_err());
        assert!(paint(r##"{"fill-color":["interpolate",["linear"],["get","v"],0,"#f00",5]}"##).is_err());
    }

    /// `fill-opacity` scales the declared fill AND its outline (MapLibre's
    /// fill-outline shader multiplies the same opacity); refused alone.
    #[test]
    fn a_constant_fill_opacity_scales_the_declared_fill_and_outline() {
        let compiled =
            paint(r##"{"fill-color":"#ff0000","fill-outline-color":"rgba(0,0,0,0.5)","fill-opacity":0.5}"##).unwrap();
        assert_eq!(compiled.color(ColorSlot::Fill, &numbers(&[]), BLACK).a, 0.5);
        assert_eq!(compiled.resolve(ColorSlot::FillOutline, &numbers(&[])).unwrap().a, 0.25);
        assert!(paint(r#"{"fill-opacity":0.5}"#).unwrap_err().contains("fill-color"));
        assert!(paint(r##"{"fill-color":"#fff","fill-opacity":1.5}"##).is_err());
        assert!(paint(r##"{"line-color":"#fff","line-opacity":["get","o"]}"##).is_err());
    }
}

/// The one pixel test: a fill whose colour comes from a property
/// `interpolate`, through `classify` (the path a tiled bake takes) and the
/// real overlay tessellation and pipeline, reads back as the interpolated
/// colour. The helpers mirror `render::gpu`'s own overlay pixel tests.
#[cfg(test)]
mod gpu_tests {
    use std::sync::Arc;

    use crate::render::{GlobeParams, GpuContext, TileBuffers, TilePipeline};

    fn overlay_chunks(ctx: &GpuContext, pipeline: &TilePipeline, plan: &crate::style::PaintPlan) -> Vec<TileBuffers> {
        let (mesh, _, extras) = crate::render::mesh::tessellate_overlay_plan_reusing(
            plan,
            &[None],
            4096,
            0.001,
            crate::render::subdivide::Subdivision::NONE,
            &|| false,
            crate::render::TileMesh::default(),
        )
        .unwrap();
        let plans =
            crate::render::mesh::plan_overlay_chunks(&mesh, &extras, usize::MAX, ctx.max_buffer_size).unwrap();
        let (mut a, mut b, mut c, mut d) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        plans
            .iter()
            .map(|chunk| {
                let u16_format = chunk.is_u16();
                let indices = crate::render::mesh::rebase_indices(
                    &mesh.indices[chunk.indices.clone()],
                    chunk.vertices.start,
                    u16_format,
                    &mut a,
                    &mut b,
                );
                let outline = crate::render::mesh::rebase_indices(
                    &extras.outline[chunk.outline.clone()],
                    chunk.vertices.start,
                    u16_format,
                    &mut c,
                    &mut d,
                );
                pipeline.install_overlay_chunk(ctx, None, &mesh.vertices[chunk.vertices.clone()], indices, outline)
            })
            .collect()
    }

    async fn render_centre(ctx: &GpuContext, pipeline: &TilePipeline, chunks: &[TileBuffers], size: u32) -> [u8; 4] {
        let identity = glam::Mat4::IDENTITY.to_cols_array_2d();
        let (_camera, camera_bind_group) = pipeline.create_camera_bind_group(ctx, identity, 0.0, 1.0);
        let (_model, model_bind_group) = pipeline.create_model_bind_group(ctx, identity, 1.0, 0.0, GlobeParams::FLAT);
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-color"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = pipeline.create_depth_view(ctx, size, size);
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let draws: Vec<(&TileBuffers, &wgpu::BindGroup)> = chunks.iter().map(|c| (c, &model_bind_group)).collect();
        pipeline.render(&mut encoder, &view, &depth_view, wgpu::Color::TRANSPARENT, &camera_bind_group, &draws, None);
        let bytes_per_row = (size * 4).next_multiple_of(256);
        let readback = crate::render::alloc::TrackedBuffer::new(&ctx.device, &wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(bytes_per_row * size),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(size),
                },
            },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(Some(encoder.finish()));
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        rx.recv().unwrap().unwrap();
        let data = slice.get_mapped_range().expect("mapped");
        let at = ((size / 2) * bytes_per_row + (size / 2) * 4) as usize;
        [data[at], data[at + 1], data[at + 2], data[at + 3]]
    }

    #[test]
    fn a_fill_coloured_by_a_property_interpolate_renders_the_interpolated_colour() {
        // #ff0000 at 0 -> #0000ff at 100, v = 25: t = 0.25 on a 255-step
        // grid -> step 64 (63.75 rounded), so r = 191/255, b = 64/255.
        let mut store = crate::overlay::OverlayStore::new();
        let paint = serde_json::json!({
            "fill-color": ["interpolate", ["linear"], ["get", "v"], 0, "#ff0000", 100, "#0000ff"]
        });
        store.declare_source_paint("s", paint.as_object().unwrap()).unwrap();
        let json = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"v":25},
            "geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}]}"#;
        store.set_source("s", json, 3).unwrap();
        let snap = store.source_snapshot("s").unwrap();
        let classes =
            crate::overlay::plan::classify(
                &snap.features,
                &snap.properties,
                crate::overlay::plan::BakeBracket::Bucket(3),
                &snap.paint,
            )
                .unwrap();
        let [crate::overlay::plan::ClassPaint::Fill { color, .. }] = classes.classes[..] else {
            panic!("one fill class, got {:?}", classes.classes)
        };

        // The class colour on a square covering the centre of clip space.
        let square = geo_types::Polygon::new(
            geo_types::LineString::from(vec![(-500, -500), (500, -500), (500, 500), (-500, 500), (-500, -500)]),
            vec![],
        );
        let plan = crate::style::PaintPlan {
            ops: vec![crate::style::PaintOp::Fill {
                color_from: color,
                color_to: color,
                geometry: Arc::new(geo_types::Geometry::MultiPolygon(geo_types::MultiPolygon(vec![square]))),
            }],
        };
        pollster::block_on(async {
            let ctx = GpuContext::new_headless().await.expect("headless device");
            let pipeline = TilePipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
            let chunks = overlay_chunks(&ctx, &pipeline, &plan);
            let pixel = render_centre(&ctx, &pipeline, &chunks, 64).await;
            assert!(pixel[0].abs_diff(191) <= 2, "red is 191/255, got {pixel:?}");
            assert!(pixel[2].abs_diff(64) <= 2, "blue is 64/255, got {pixel:?}");
            assert!(pixel[1] <= 2, "no green, got {pixel:?}");
        });
    }
}

/// B9: the columnar evaluation (`SourcePaint::resolve_rows`) against the
/// per-map one (`SourcePaint::resolve`) it replaced, on randomised
/// properties -- mixed types per key, missing values -- and randomised
/// expression parameters. Every row, every slot, bit for bit.
#[cfg(test)]
mod columnar_tests {
    use super::*;
    use crate::overlay::columns::tests::random_maps;

    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
        fn number(&mut self) -> f64 {
            (self.below(2000) as f64 - 500.0) * 125.0
        }
    }

    /// One expression per shape the compiler has, with random parameters.
    fn expressions(rng: &mut Lcg) -> Vec<serde_json::Value> {
        use serde_json::json;
        let lo = rng.number();
        let hi = lo + 1.0 + rng.below(100_000) as f64;
        let t1 = rng.number();
        let t2 = t1 + 1.0 + rng.below(50_000) as f64;
        let s = |rng: &mut Lcg| format!("s{}", rng.below(7));
        let key = |rng: &mut Lcg| ["a", "b", "mixed", "income", "route"][rng.below(5) as usize];
        vec![
            json!(["get", "colour"]),
            json!(["get", key(rng)]),
            json!("#123456"),
            json!(["match", ["get", "route"], s(rng), "#ff0000", s(rng), "#00ff00", "#0000ff"]),
            json!(["match", ["get", key(rng)], s(rng), "#ff0000", "#0000ff"]),
            json!(["case", ["has", key(rng)], "#00ff00", "#0000ff"]),
            json!(["match", ["get", "route"], [s(rng), s(rng)], "#ff0000", s(rng), "#00ff00", "#0000ff"]),
            json!(["case", ["has", "income"],
                ["interpolate", ["linear"], ["get", "income"], lo, "#440154", (lo + hi) / 2.0, "#21918c", hi, "#fde725"],
                "#9e9e9e"]),
            json!(["interpolate", ["exponential", 1.5], ["get", key(rng)], lo, "#000000", hi, "#ffffff"]),
            json!(["step", ["get", key(rng)], "#111111", t1, "#222222", t2, ["get", "colour"]]),
            json!(["case",
                ["all", ["has", "flag"], ["==", ["get", "flag"], true]], "#ff00ff",
                ["any", [">", ["get", key(rng)], t1], ["<=", ["get", "income"], t2]], "#00ffff",
                ["==", ["get", key(rng)], null], "#abcdef",
                ["!", ["has", "b"]], "#fedcba",
                "#000000"]),
            json!(["case", [">=", ["get", "route"], s(rng)], "#ff0000", "#00ff00"]),
        ]
    }

    #[test]
    fn columnar_resolution_matches_per_map_resolution_on_random_data() {
        let slots = [ColorSlot::Fill, ColorSlot::FillOutline, ColorSlot::Line, ColorSlot::Circle];
        let mut checked = 0usize;
        for seed in 0..40u64 {
            let maps = random_maps(seed.wrapping_mul(7919), 400);
            let table = PropertyTable::from_maps(&maps);
            let mut rng = Lcg(seed ^ 0x9e37_79b9_7f4a_7c15);
            let exprs = expressions(&mut rng);
            for (at, expr) in exprs.iter().enumerate() {
                // The same expression in every slot, with an opacity on
                // the fill pair, so the slot plumbing is covered too.
                let decl = serde_json::json!({
                    "fill-color": expr, "fill-outline-color": expr, "line-color": expr, "circle-color": expr,
                    "fill-opacity": 0.5,
                });
                let paint = SourcePaint::compile(decl.as_object().unwrap())
                    .unwrap_or_else(|error| panic!("seed {seed} expr {at} {expr}: {error}"));
                for slot in slots {
                    let rows = paint.resolve_rows(slot, &table).unwrap();
                    assert_eq!(rows.len(), maps.len());
                    for (row, map) in maps.iter().enumerate() {
                        assert_eq!(
                            rows[row],
                            paint.resolve(slot, map),
                            "seed {seed} expr {at} {expr} slot {slot:?} row {row}: {map:?}"
                        );
                        checked += 1;
                    }
                    // A picked subset, out of order and repeating, reads
                    // exactly the rows it names.
                    let picks: Vec<usize> = (0..37).map(|i| (i * 131 + seed as usize) % maps.len()).collect();
                    let picked = paint.resolve_rows_at(slot, &table, Some(&picks)).unwrap();
                    let expected: Vec<Option<Rgba>> = picks.iter().map(|&row| rows[row]).collect();
                    assert_eq!(picked, expected, "seed {seed} expr {at} {expr} slot {slot:?} picked");
                }
            }
        }
        assert!(checked > 100_000, "{checked}");
    }

    /// An undeclared slot resolves to no column at all, so the plan falls
    /// back to each feature's own paint exactly as before.
    #[test]
    fn an_undeclared_slot_resolves_no_rows() {
        let paint = SourcePaint::compile(serde_json::json!({"fill-color": "#ff0000"}).as_object().unwrap()).unwrap();
        let table = PropertyTable::from_maps(&random_maps(1, 10));
        assert!(paint.resolve_rows(ColorSlot::Line, &table).is_none());
        assert_eq!(paint.resolve_rows(ColorSlot::Fill, &table).unwrap().len(), 10);
    }
}
