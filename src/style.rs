use std::collections::HashMap;
use std::sync::Arc;

use smallvec::{smallvec, SmallVec};

use crate::mvt::{DecodedTile, TagLookup, TagValue};

// Spelled out because `web/` and `ios/` include this file by `#[path]`,
// where the implicit `style/` directory would not be found.
#[path = "style/columnar.rs"]
mod columnar;
use columnar::TileEval;
use crate::overlay::dotgrid::mask_class::{self, MaskRank, MaskRanks};

#[path = "style/worldview.rs"]
mod worldview;
pub use worldview::set_worldview;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// The paint every basemap label draws with while data fills sit under
/// the labels (`MapView::set_basemap_label_paint`): text colour and halo,
/// replacing each symbol layer's own. Icons keep their paint.
///
/// A style's label paint is chosen for its own ground -- light text with
/// a dark halo on a dark basemap, grey italics on a pale one -- and a
/// choropleth under the labels replaces that ground with every colour of
/// a palette. MapLibre GL JS has no per-label contrast rule (`text-color`
/// is evaluated per feature, never against the pixels under it), so tools
/// built on it restyle the symbol layers with `setPaintProperty` once data
/// covers the map. This is that restyle, as one paint for every label.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LabelPaint {
    pub text_color: Rgba,
    pub halo_color: Rgba,
    /// `text-halo-width`, layout pixels.
    pub halo_width: f32,
    /// `text-halo-blur`, layout pixels.
    pub halo_blur: f32,
}

impl LabelPaint {
    /// Near-black text in a near-opaque white halo: it reads on every
    /// colour of a sequential palette (viridis' purple and its yellow
    /// alike), on a white data circle, and in both appearances, because
    /// the halo -- not the basemap -- is the label's ground.
    pub const ON_DATA: Self = Self {
        text_color: Rgba { r: 29.0 / 255.0, g: 29.0 / 255.0, b: 31.0 / 255.0, a: 1.0 },
        halo_color: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.9 },
        halo_width: 1.5,
        halo_blur: 0.5,
    };
}

/// Alternating dash/gap run lengths in units of line width, starting with a
/// dash -- the `line-dasharray` paint property.
///
/// `count` is always 0 (not dashed), 2, or 4. Odd-length dasharrays are
/// folded to even at construction (see `from_runs`), so the shader never has
/// to handle an odd count. Four is the longest pattern in the vendored
/// Mapbox Streets v12 style (`admin-1-boundary`'s `[2,2,6,2]`), which is why
/// the mask can be evaluated analytically instead of through a texture atlas.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DashPattern {
    pub runs: [f32; 4],
    pub count: u8,
}

impl DashPattern {
    /// The not-dashed value: every line layer without the property, and the
    /// degradation target for any pattern we cannot represent.
    pub fn none() -> Self {
        Self { runs: [0.0; 4], count: 0 }
    }

    /// Builds a pattern from a raw `line-dasharray` array.
    ///
    /// Returns `none()` -- rendering a solid line -- rather than failing, for
    /// anything unrepresentable: empty, negative or non-finite, or longer
    /// than 4 runs once collapsed. A solid line is a far better failure than
    /// a dropped layer.
    ///
    /// The input is treated as the *cyclic* alternation it actually is, and
    /// normalized before it is stored, in three steps: zero-length runs are
    /// dropped, runs of the same type left adjacent by that drop are merged,
    /// and the last run is merged into the first when the two are the same
    /// type (they are adjacent across the cycle seam). What survives strictly
    /// alternates dash/gap with every run positive, which is what lets the
    /// shader treat every stored run boundary as a real edge. See
    /// `dash_pattern_collapses_zero_length_runs_because_a_same_type_boundary_is_not_an_edge`
    /// for why that matters.
    pub fn from_runs(runs: &[f32]) -> Self {
        if runs.is_empty() || runs.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return Self::none();
        }

        // `(length, is_dash)` for every run that actually covers ground.
        // Position parity gives the type: `line-dasharray` starts on a dash
        // and alternates. Dropping a zero-length run leaves its neighbours
        // touching, and two touching runs of the SAME type are one run --
        // merge them rather than storing a boundary the shader would
        // antialias as if it were an edge.
        let mut collapsed: Vec<(f32, bool)> = Vec::with_capacity(runs.len());
        for (i, &length) in runs.iter().enumerate() {
            if length == 0.0 {
                continue;
            }
            let is_dash = i.is_multiple_of(2);
            match collapsed.last_mut() {
                Some((prev_length, prev_is_dash)) if *prev_is_dash == is_dash => *prev_length += length,
                _ => collapsed.push((length, is_dash)),
            }
        }

        // The cycle seam: the last run runs straight into the first one on
        // the next repeat, so if they are the same type they are a single
        // run. This subsumes the odd-length fold that used to be spelled out
        // here (an odd-length list starts and ends on a dash), and matches
        // what MapLibre's `getDashRanges` effectively renders: `[3,2,5]`
        // becomes dash 8, gap 2.
        if collapsed.len() >= 2 && collapsed[0].1 == collapsed[collapsed.len() - 1].1 {
            let (last_length, _) = collapsed.pop().expect("length is at least 2");
            collapsed[0].0 += last_length;
        }

        // Fewer than two runs left means the pattern never alternates at all:
        // either every run was a dash (a solid line, which `none()` renders
        // exactly, with no shader work) or every run was zero or a gap. The
        // all-gap case would strictly mean "draw nothing", which this type
        // cannot express; `none()`'s documented degradation to solid is the
        // better of the two wrong answers, and no vendored layer asks for it.
        //
        // More than four runs would need MapLibre's texture atlas. Measured
        // here, AFTER collapsing, so a long pattern that collapses to a short
        // one is kept rather than thrown away.
        if collapsed.len() < 2 || collapsed.len() > 4 {
            return Self::none();
        }

        // Every stored pattern starts on a dash. A leading zero-length dash
        // can leave a gap first; rotating puts the dash back in front, which
        // is the same cyclic pattern shifted along the line by one run.
        if !collapsed[0].1 {
            collapsed.rotate_left(1);
        }

        let mut out = Self::none();
        for (slot, (length, _)) in out.runs.iter_mut().zip(&collapsed) {
            *slot = *length;
        }
        out.count = collapsed.len() as u8;
        // Every surviving run is strictly positive, so the period is too --
        // no divide-by-zero reaches the shader. A pattern that summed to zero
        // lost every run to the collapse above and returned `none()` there.
        debug_assert!(out.period() > 0.0, "collapsed pattern must have a positive period");
        out
    }

    /// Round dots, one line width across, centred every `period` line
    /// widths: what MapLibre draws for `line-dasharray: [0, period]` with
    /// `line-cap: round` (a zero-length dash is drawn by its two round
    /// caps alone). `from_runs` cannot say this -- it collapses the
    /// zero-length dash -- so the pattern is flagged instead: a NEGATIVE
    /// fourth run, which `from_runs` never produces, tells `shader.wgsl`
    /// to evaluate a dot mask rather than the dash mask. `none()` for a
    /// period that is not finite and positive.
    pub fn round_dots(period: f32) -> Self {
        if !(period.is_finite() && period > 0.0) {
            return Self::none();
        }
        Self { runs: [0.0, period, 0.0, -1.0], count: 2 }
    }

    /// Whether this is a [`DashPattern::round_dots`] pattern.
    pub fn is_round_dots(&self) -> bool {
        self.count == 2 && self.runs[3] < 0.0
    }

    /// One full cycle of the pattern, in units of line width.
    pub fn period(&self) -> f32 {
        self.runs[..self.count as usize].iter().sum()
    }
}

/// A value that can be linearly interpolated between two endpoints, as used
/// by `ZoomValue::interpolated`'s "interpolate"/"linear" stop expressions.
pub trait Lerp {
    fn lerp(&self, other: &Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        self + (other - self) * t
    }
}

impl Lerp for Rgba {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Rgba {
            r: self.r.lerp(&other.r, t),
            g: self.g.lerp(&other.g, t),
            b: self.b.lerp(&other.b, t),
            a: self.a.lerp(&other.a, t),
        }
    }
}

/// What `style_spec::expression`'s zoom-expression compiler needs of a paint
/// value type. `Lerp` on its own can't serve as that bound.
///
/// The style spec pins some paint properties `"expression": {"interpolated":
/// false}`. `line-dasharray` is one, and its value type (`DashPattern`)
/// deliberately has no `Lerp` impl, because blending two dash patterns is
/// not a meaningful operation -- MapLibre snaps between them. Such a
/// property still needs the `step`/`match`/literal half of the very same
/// compiler that `line-width` and `line-color` use, so that compiler is
/// bounded by this trait instead: `INTERPOLATOR` is `Some` for a type that
/// can be blended and `None` for one that can't, and an `interpolate`
/// expression over a `None` type is a compile error (which, for
/// `line-dasharray`, its caller absorbs into `DashPattern::none()`) rather
/// than a silent misrender.
///
/// This can't be a blanket `impl<T: Clone + Lerp> ZoomExprValue for T`:
/// coherence would then forbid the `DashPattern` impl, since rustc won't
/// reason about `DashPattern: !Lerp`. Hence one explicit impl per type.
pub trait ZoomExprValue: Clone {
    const INTERPOLATOR: Option<fn(&Self, &Self, f32) -> Self>;
}

impl ZoomExprValue for f32 {
    const INTERPOLATOR: Option<fn(&Self, &Self, f32) -> Self> = Some(|a, b, t| a.lerp(b, t));
}

impl ZoomExprValue for Rgba {
    const INTERPOLATOR: Option<fn(&Self, &Self, f32) -> Self> = Some(|a, b, t| a.lerp(b, t));
}

impl ZoomExprValue for DashPattern {
    const INTERPOLATOR: Option<fn(&Self, &Self, f32) -> Self> = None;
}

#[derive(Debug, Clone)]
pub struct ZoomValue<T> {
    /// Sorted ascending by zoom level.
    ///
    /// Inline capacity 2, not the `SmallVec` default of 1: a `constant`
    /// value is one stop and a from/to `interpolated`/`stepped` pair is
    /// two, and across the vendored styles' compiled output those two
    /// shapes together account for ~85% of every `ZoomValue` this crate
    /// builds (~80% one stop, ~6% two). The remaining ~15% is the tail
    /// `compile_interpolate` produces by resampling a non-linear zoom curve
    /// at every integer zoom in its range -- up to ~23 stops -- which
    /// spills to the heap exactly as the old `Vec` always did; N=2 buys the
    /// common case nothing more spills for, and buys nothing chasing the
    /// tail since it heap-allocates regardless of N.
    pub stops: SmallVec<[(u8, T); 2]>,
    /// `None` means step (hard-cutoff) behavior; `Some` means linear
    /// interpolation between bracketing stops using this function.
    ///
    /// This is a function pointer rather than a `T: Lerp` bound on the
    /// struct/`impl` because `value_at`'s step branch must keep working for
    /// every `T: Clone`, including types (like `&str`, used in tests) that
    /// don't implement `Lerp` at all. Requiring `Lerp` only where it's
    /// actually needed -- the `interpolated` constructor, which sets this
    /// field -- keeps `value_at` callable on any `ZoomValue<T: Clone>`.
    interpolator: Option<fn(&T, &T, f32) -> T>,
    /// The curve at its TRUE stop zooms, kept only when a stop sits off the
    /// integer grid (Mapbox Standard's building fade, `15 -> 0, 15.3 -> a`):
    /// `stops` resamples at integers and so stretches that ramp across a
    /// whole zoom -- for `a` = 0.8, ~0.16 at z15.2 where MapLibre gives ~0.53.
    /// Set only where a caller asks for it
    /// (`style_spec::expression::compile_off_grid_stops`).
    /// Piecewise linear, ascending, read by [`ZoomValue::value_at_zoom`]
    /// only -- every other reader keeps the integer samples. `None` for
    /// everything else, so the common shapes carry no allocation.
    off_grid: Option<Arc<[(f32, T)]>>,
}

impl<T: Clone> ZoomValue<T> {
    /// The single most common `ZoomValue` shape (~80% of every one this
    /// crate builds, measured across the vendored styles). With inline
    /// capacity 2 this allocates nothing.
    pub fn constant(value: T) -> Self {
        Self { stops: smallvec![(0, value)], interpolator: None, off_grid: None }
    }

    /// Explicit multi-stop, hard-cutoff-at-each-stop constructor (the
    /// pre-existing `ZoomValue` behavior, now named so callers can choose
    /// it deliberately alongside the new `interpolated`).
    ///
    /// Takes an owned `Vec` rather than a `SmallVec` so call sites that
    /// build stops with `vec![...]` or `.collect::<Vec<_>>()` -- most of
    /// them -- don't need to change; the conversion below is a move, not a
    /// copy, and degrades to a heap allocation only past the inline
    /// capacity, exactly like the `Vec` this replaced.
    pub fn stepped(stops: Vec<(u8, T)>) -> Self {
        Self { stops: stops.into(), interpolator: None, off_grid: None }
    }

    /// `interpolated`, but with the blend function passed in rather than
    /// taken from a `T: Lerp` bound. The expression compiler is generic
    /// over `ZoomExprValue` (whose `INTERPOLATOR` is `None` for types like
    /// `DashPattern` that must never be blended), so it can't name
    /// `T::lerp`; it checks `INTERPOLATOR` once and passes the function
    /// through to here.
    pub fn interpolated_with(stops: Vec<(u8, T)>, interpolator: fn(&T, &T, f32) -> T) -> Self {
        Self { stops: stops.into(), interpolator: Some(interpolator), off_grid: None }
    }

    pub fn value_at(&self, zoom: u8) -> T {
        match self.interpolator {
            None => self
                .stops
                .iter()
                .rev()
                .find(|(stop_zoom, _)| *stop_zoom <= zoom)
                .or_else(|| self.stops.first())
                .expect("ZoomValue must have at least one stop")
                .1
                .clone(),
            Some(interpolator) => self.value_at_linear(zoom, interpolator),
        }
    }

    fn value_at_linear(&self, zoom: u8, interpolator: fn(&T, &T, f32) -> T) -> T {
        let first = self.stops.first().expect("ZoomValue must have at least one stop");
        let last = self.stops.last().expect("ZoomValue must have at least one stop");
        if zoom <= first.0 {
            return first.1.clone();
        }
        if zoom >= last.0 {
            return last.1.clone();
        }
        // Bracket: the last stop at or below `zoom`, and the one right after it.
        let lower_idx = self
            .stops
            .iter()
            .rposition(|(stop_zoom, _)| *stop_zoom <= zoom)
            .expect("zoom is between first and last stop, so a lower bracket exists");
        let (z0, v0) = &self.stops[lower_idx];
        let (z1, v1) = &self.stops[lower_idx + 1];
        let t = (zoom - z0) as f32 / (z1 - z0) as f32;
        interpolator(v0, v1, t)
    }

    /// The bracketing pair (value at `zoom`, value at `zoom + 1`) that a
    /// caller blends continuously via a fractional-zoom GPU uniform,
    /// instead of snapping to whichever integer zoom bucket `zoom`
    /// truncates to. For a `stepped`/`constant` `ZoomValue` (no
    /// interpolator), both elements are identical -- there is nothing to
    /// blend, matching real MapLibre's step expressions, which are never
    /// continuously interpolated the way `interpolate` expressions are.
    pub fn value_at_pair(&self, zoom: u8) -> (T, T) {
        match self.interpolator {
            None => {
                let v = self.value_at(zoom);
                (v.clone(), v)
            }
            Some(_) => (self.value_at(zoom), self.value_at(zoom.saturating_add(1))),
        }
    }

    /// Attaches the curve sampled at its true, fractional stop zooms (see
    /// `off_grid`). Ignored on a value with no interpolator -- a step has no
    /// curve to follow between its stops -- and for an empty `stops`.
    #[must_use]
    pub fn with_off_grid_stops(mut self, stops: Vec<(f32, T)>) -> Self {
        if self.interpolator.is_some() && !stops.is_empty() {
            self.off_grid = Some(Arc::from(stops));
        }
        self
    }

    /// The value at a FRACTIONAL zoom, for a caller that evaluates on the
    /// CPU once per frame (the extrusion opacity uniform) rather than
    /// blending a baked `value_at_pair` on the GPU.
    ///
    /// With off-grid stops it follows the true curve, which is MapLibre's
    /// answer; otherwise it is exactly what the GPU blend computes --
    /// `value_at_pair(floor z)` mixed by `fract z` for an interpolated
    /// value, `value_at(floor z)` for a step. A non-finite or negative zoom
    /// reads as 0.
    pub fn value_at_zoom(&self, zoom: f64) -> T {
        let zoom = if zoom.is_finite() { zoom.max(0.0) } else { 0.0 };
        let whole = zoom.floor().min(f64::from(u8::MAX)) as u8;
        let Some(interpolator) = self.interpolator else {
            return self.value_at(whole);
        };
        if let Some(stops) = &self.off_grid {
            let z = zoom as f32;
            let (first, last) = (&stops[0], &stops[stops.len() - 1]);
            if z <= first.0 {
                return first.1.clone();
            }
            if z >= last.0 {
                return last.1.clone();
            }
            let upper = stops.iter().position(|(stop_zoom, _)| *stop_zoom > z).expect("z is below the last stop");
            let ((z0, v0), (z1, v1)) = (&stops[upper - 1], &stops[upper]);
            return interpolator(v0, v1, (z - z0) / (z1 - z0));
        }
        let (from, to) = self.value_at_pair(whole);
        interpolator(&from, &to, zoom.fract() as f32)
    }

    /// The same value at every zoom, so ANY bracketing pair resolves it
    /// correctly and blending two brackets is always a no-op.
    pub fn is_constant(&self) -> bool {
        self.stops.len() == 1
    }

    /// Whether `value_at` BLENDS between bracketing stops (`true`) or snaps
    /// to the last stop at or below the zoom (`false`) -- i.e. whether this
    /// value came from an `interpolate` expression or a `step`/`match`/
    /// constant one.
    ///
    /// Exposed, alongside `is_constant`, for `style_spec`'s ground-lighting
    /// bake: it rebuilds a colour stop-by-stop and has to decide whether the
    /// rebuilt value may blend. A multi-stop stepped colour may not --
    /// blending its hard cutoff would put a colour on screen the style never
    /// declared -- while a single-stop constant has no cutoff to protect and
    /// blends freely, so the light itself stays continuous. See
    /// `light_ground_color`, which documents the trade-off in full.
    pub fn is_interpolated(&self) -> bool {
        self.interpolator.is_some()
    }
}

impl<T: Clone + Lerp> ZoomValue<T> {
    pub fn interpolated(stops: Vec<(u8, T)>) -> Self {
        Self { stops: stops.into(), interpolator: Some(|a, b, t| a.lerp(b, t)), off_grid: None }
    }
}

impl ZoomValue<f32> {
    /// The zoom range this value ramps STRAIGHT across, when it does.
    ///
    /// A caller that gets `Some((lo, hi))` may resolve the value ONCE at
    /// `lo` and `hi` and recover `value_at(z)` for every `z` by blending
    /// with `clamp((z - lo) / (hi - lo), 0, 1)`. That is what lets the
    /// overlay bake its widths once instead of re-resolving them at every
    /// zoom bucket the camera crosses -- see `overlay::plan::BakeBracket`.
    ///
    /// # Why this is a numeric test and not a structural one
    ///
    /// It is tempting to answer this by counting stops: two stops and an
    /// interpolator, and you have a straight line. That is wrong here,
    /// because by the time a style expression reaches a `ZoomValue` it has
    /// been RESAMPLED at every integer zoom in its range --
    /// `style_spec::expression::compile_interpolate` does this so that a
    /// non-linear `cubic-bezier` or `exponential` curve can be carried as
    /// integer-resolution piecewise-linear data. So the caller's two-stop
    /// `["interpolate", ["linear"], ["zoom"], 8, 2.4, 18, 7.2]` arrives
    /// here with ELEVEN stops, and a structural test would reject the one
    /// shape it most needs to accept.
    ///
    /// Asking whether the stops are collinear answers the question that is
    /// actually being asked -- "can two endpoints reproduce this?" -- and
    /// answers it for however the value was built. A resampled linear ramp
    /// passes; a resampled cubic-bezier fails by whole pixels; a `step`
    /// fails on its missing interpolator.
    ///
    /// The tolerance is a thousandth of a pixel, which is far below anything
    /// that can be seen and far above the f32 rounding a genuinely linear
    /// resample accumulates.
    pub fn linear_ramp(&self) -> Option<(u8, u8)> {
        // A `step`'s hard cutoffs must never be blended into a ramp, no
        // matter where its stops happen to fall.
        self.interpolator?;
        let (lo, first) = *self.stops.first()?;
        let (hi, last) = *self.stops.last()?;
        if lo >= hi {
            return None;
        }
        let span = (hi - lo) as f32;
        for &(zoom, value) in &self.stops {
            let t = (zoom - lo) as f32 / span;
            if (value - (first + (last - first) * t)).abs() > 1e-3 {
                return None;
            }
        }
        Some((lo, hi))
    }
}

/// Two `ZoomValue`s are equal when they resolve the same at every zoom:
/// same stops, and the same step-vs-interpolate behaviour.
///
/// Written out rather than derived because deriving would compare the
/// `interpolator` FUNCTION POINTERS, which rustc warns about for good reason
/// -- addresses are not guaranteed unique, so two identical closures can
/// compare unequal or two different ones equal. Comparing only whether an
/// interpolator is present is both meaningful and sufficient: for any given
/// `T` there is exactly one blend function in play (`T::lerp`, reached
/// either directly or through `ZoomExprValue::INTERPOLATOR`), so "both
/// interpolate" and "both step" are the only distinctions that exist.
///
/// This impl exists because `overlay::FeaturePaint` is `PartialEq` (public
/// API) and its `line_width` is now one of these.
impl<T: PartialEq> PartialEq for ZoomValue<T> {
    fn eq(&self, other: &Self) -> bool {
        self.stops == other.stops
            && self.interpolator.is_some() == other.interpolator.is_some()
            && self.off_grid == other.off_grid
    }
}

/// Numeric comparison operators for `Filter::NumberCompare`, used by
/// style-spec filter expressions like `["<=", ["get", "admin_level"], 6]`
/// whose operand is a number rather than a string (MVT tags decode numeric
/// values to `TagValue::Number`, so these can't be represented by
/// `Filter::Equals`/`Filter::In`, which only ever compare `TagValue::String`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumberOp {
    Eq,
    NotEq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Debug, Clone)]
pub enum Filter {
    Any,
    /// Always false. Symmetric counterpart to `Filter::Any`; used to
    /// compile literal `false` outputs in `case`/`match` filter
    /// expressions without indirecting through `Not(Any)`.
    Never,
    Equals { key: String, value: String },
    /// `["==", ["get", key], true|false]`: true when the tag is that
    /// boolean (`TagValue::Bool`). OpenMapTiles' `hide_3d` is the one
    /// vendored use (Liberty's `building-3d`); `!=` compiles to `Not` of
    /// this, so an absent tag is `null != true`, i.e. passes.
    BoolEquals { key: String, value: bool },
    In { key: String, values: Vec<String> },
    /// AND of sub-filters (Mapbox style-spec `"all"` combinator).
    All(Vec<Filter>),
    /// OR of sub-filters (Mapbox style-spec `"any"` combinator). Named
    /// `AnyOf` rather than `Any` to avoid confusion with the pre-existing
    /// `Filter::Any` variant above, whose meaning ("always true", the
    /// no-filter default) is unrelated to the style-spec `"any"` combinator.
    AnyOf(Vec<Filter>),
    Not(Box<Filter>),
    /// Numeric comparison against a `TagValue::Number` tag.
    NumberCompare { key: String, op: NumberOp, value: f64 },
    /// Style-spec `"has"`: true if the key is present, regardless of value.
    Has(String),
    /// Mirrors the style-spec `step` expression restricted to a zoom
    /// input and filter-valued outputs: `base` applies below the first
    /// stop; otherwise the filter paired with the greatest stop <= the
    /// current zoom applies. `stops` must be non-empty and ascending by
    /// threshold (as the style spec requires for `step`).
    ZoomStep { base: Box<Filter>, stops: Vec<(f64, Filter)> },
    /// Mirrors the style-spec `case` expression: an ordered if/elif/else
    /// chain. The first arm whose condition matches supplies the result;
    /// if none match, `fallback` applies.
    Case { arms: Vec<(Filter, Filter)>, fallback: Box<Filter> },
    /// Generalizes `match` beyond the single-case-group boolean-only
    /// shape handled by `compile_top_level_match`'s fast path (which
    /// still produces `In`/`Not(In)` directly and never constructs this
    /// variant). Case-groups are tried in order; the first whose label
    /// set contains the tagged string value supplies the result; if none
    /// match, `fallback` applies.
    MultiMatch { key: String, cases: Vec<(Vec<String>, Filter)>, fallback: Box<Filter> },
    /// Numeric-label sibling of `MultiMatch`, for `match` expressions whose
    /// case-group labels are bare JSON numbers rather than strings (e.g.
    /// Mapbox Streets' `hillshade` layer's `["match", ["get", "level"], 89,
    /// true, ...]`). Case-groups are tried in order; the first whose label
    /// set contains the tagged numeric value supplies the result; if none
    /// match, `fallback` applies.
    NumberMultiMatch { key: String, cases: Vec<(Vec<f64>, Filter)>, fallback: Box<Filter> },
    /// A numeric comparison where the compared value is a feature tag
    /// minus a zoom-dependent offset: `tag_value - offset.value_at(zoom)
    /// <op> threshold`. The one real shape this exists for: Mapbox
    /// Streets' `landuse` filter's `["<=", ["-", ["to-number", ["get",
    /// "sizerank"]], ["interpolate", ...zoom...]], 14]`. See
    /// `compile_arithmetic_comparison`'s doc comment in
    /// `src/style_spec/filter.rs` for why this is scoped to exactly this
    /// shape rather than a general arithmetic evaluator.
    NumberCompareWithZoomOffset { key: String, op: NumberOp, offset: ZoomValue<f32>, threshold: f64 },
    /// The style-spec `["geometry-type"]` expression, compiled from either
    /// spelling styles use it in: `["==", ["geometry-type"], "LineString"]`
    /// (Mapbox Streets, pervasively) and `["match", ["geometry-type"],
    /// ["LineString", "MultiLineString"], true, false]` (Liberty). True when
    /// the feature's geometry falls in one of these classes.
    ///
    /// Matches nothing away when the caller has no geometry in hand --
    /// see `Filter::matches_geometry`.
    GeometryType(Vec<GeometryKind>),
}

/// The geometry class a `["geometry-type"]` filter tests.
///
/// Three variants, not six, and that is the style spec's own answer rather
/// than a simplification. MapLibre's `EvaluationContext.geometryType()`
/// (`src/expression/evaluation_context.ts`) indexes `['Unknown', 'Point',
/// 'LineString', 'Polygon']` by the vector-tile feature's numeric type, so
/// over a vector tile `["geometry-type"]` NEVER returns a `Multi*` name --
/// the `"MultiLineString"` entries that appear in real styles' `match` lists
/// are dead there, carried along because the same style might also be run
/// over a GeoJSON source.
///
/// This crate only ever evaluates filters over MVT-decoded features, so both
/// sides collapse onto these three: `GeometryKind::of` maps `MultiPoint`
/// alongside `Point` (which matters -- `mvt::decode` returns `MultiPoint`
/// even for a single-point feature, see `match_tile_symbols`), and
/// `from_style_name` accepts the `Multi*` spellings as aliases of their base
/// class. That last part is a deliberate one-line divergence from MapLibre,
/// which would answer a `["==", ["geometry-type"], "MultiPolygon"]` filter
/// over a vector tile with "matches nothing" and empty the layer. No
/// vendored style writes one -- every `Multi*` name in all six appears
/// beside its base name -- so the alias changes no render today, and it
/// makes the failure mode of a style that did write one "draws the
/// polygons" rather than "silently draws nothing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryKind {
    Point,
    LineString,
    Polygon,
}

impl GeometryKind {
    /// The class a decoded feature's geometry belongs to, or `None` for a
    /// shape with no style-spec geometry type (`GeometryCollection`, an
    /// empty `Rect`/`Triangle`, ...). `None` propagates as "unknown", which
    /// `Filter::matches_geometry` treats as unfilterable rather than as a
    /// non-match.
    pub fn of(geometry: &geo_types::Geometry<i32>) -> Option<Self> {
        match geometry {
            geo_types::Geometry::Point(_) | geo_types::Geometry::MultiPoint(_) => Some(Self::Point),
            geo_types::Geometry::LineString(_) | geo_types::Geometry::MultiLineString(_) | geo_types::Geometry::Line(_) => {
                Some(Self::LineString)
            }
            geo_types::Geometry::Polygon(_)
            | geo_types::Geometry::MultiPolygon(_)
            | geo_types::Geometry::Rect(_)
            | geo_types::Geometry::Triangle(_) => Some(Self::Polygon),
            geo_types::Geometry::GeometryCollection(_) => None,
        }
    }

    /// The style-spec names that select this class. Both the base name and
    /// its `Multi*` spelling -- see the type's doc comment.
    pub fn from_style_name(name: &str) -> Option<Self> {
        match name {
            "Point" | "MultiPoint" => Some(Self::Point),
            "LineString" | "MultiLineString" => Some(Self::LineString),
            "Polygon" | "MultiPolygon" => Some(Self::Polygon),
            _ => None,
        }
    }
}

impl Filter {
    /// Evaluates the filter with no geometry in hand. Every
    /// `Filter::GeometryType` clause matches, which is exactly what the
    /// whole filter did before geometry predicates were representable --
    /// so a caller that cannot supply geometry (a paint value's
    /// `ZoomValue::resolve`, which sees only tags) keeps its old behaviour
    /// rather than losing features to a predicate it cannot answer.
    pub fn matches(&self, tags: &dyn TagLookup, zoom: f64) -> bool {
        self.matches_geometry(tags, zoom, None)
    }

    /// Evaluates the filter against a feature's tags AND its geometry class.
    /// `geometry: None` means "not known here"; see `matches`.
    pub fn matches_geometry(&self, tags: &dyn TagLookup, zoom: f64, geometry: Option<GeometryKind>) -> bool {
        match self {
            Filter::Any => true,
            Filter::Never => false,
            Filter::Equals { key, .. }
            | Filter::BoolEquals { key, .. }
            | Filter::In { key, .. }
            | Filter::NumberCompare { key, .. }
            | Filter::Has(key)
            | Filter::NumberCompareWithZoomOffset { key, .. } => self.leaf_matches_value(tags.get(key), zoom),
            Filter::All(filters) => filters.iter().all(|f| f.matches_geometry(tags, zoom, geometry)),
            Filter::AnyOf(filters) => filters.iter().any(|f| f.matches_geometry(tags, zoom, geometry)),
            Filter::Not(filter) => !filter.matches_geometry(tags, zoom, geometry),
            Filter::ZoomStep { base, stops } => stops
                .iter()
                .rev()
                .find(|(threshold, _)| zoom >= *threshold)
                .map(|(_, filter)| filter.matches_geometry(tags, zoom, geometry))
                .unwrap_or_else(|| base.matches_geometry(tags, zoom, geometry)),
            Filter::Case { arms, fallback } => arms
                .iter()
                .find(|(cond, _)| cond.matches_geometry(tags, zoom, geometry))
                .map(|(_, out)| out.matches_geometry(tags, zoom, geometry))
                .unwrap_or_else(|| fallback.matches_geometry(tags, zoom, geometry)),
            Filter::MultiMatch { key, cases, fallback } => {
                let tag_value = tags.get(key).and_then(|v| match v {
                    TagValue::String(s) => Some(s.as_str()),
                    _ => None,
                });
                cases
                    .iter()
                    .find(|(labels, _)| tag_value.is_some_and(|v| labels.iter().any(|l| l == v)))
                    .map(|(_, out)| out.matches_geometry(tags, zoom, geometry))
                    .unwrap_or_else(|| fallback.matches_geometry(tags, zoom, geometry))
            }
            Filter::NumberMultiMatch { key, cases, fallback } => {
                let tag_value = tags.get(key).and_then(|v| match v {
                    TagValue::Number(n) => Some(*n),
                    _ => None,
                });
                cases
                    .iter()
                    .find(|(labels, _)| tag_value.is_some_and(|v| labels.contains(&v)))
                    .map(|(_, out)| out.matches_geometry(tags, zoom, geometry))
                    .unwrap_or_else(|| fallback.matches_geometry(tags, zoom, geometry))
            }
            // No geometry in hand means the predicate cannot be answered,
            // and an unanswerable predicate must not delete the feature --
            // that would silently empty a layer for every caller that only
            // has tags. See `matches`.
            Filter::GeometryType(kinds) => geometry.is_none_or(|kind| kinds.contains(&kind)),
        }
    }

    /// A single-key leaf (`Equals`, `In`, `NumberCompare`, `Has`,
    /// `NumberCompareWithZoomOffset`) decided on the value the feature
    /// carries for its key, `None` when it carries none.
    ///
    /// The one definition of what these predicates mean: row-at-a-time
    /// `matches_geometry` calls it per feature, and the columnar
    /// evaluator (`columnar::LayerEval`) calls it once per distinct
    /// dictionary value -- so the two cannot drift.
    ///
    /// # Panics
    ///
    /// On any other variant; callers dispatch on the leaf variants first.
    pub(crate) fn leaf_matches_value(&self, value: Option<&TagValue>, zoom: f64) -> bool {
        fn compare(n: f64, op: NumberOp, value: f64) -> bool {
            match op {
                NumberOp::Eq => n == value,
                NumberOp::NotEq => n != value,
                NumberOp::Lt => n < value,
                NumberOp::Lte => n <= value,
                NumberOp::Gt => n > value,
                NumberOp::Gte => n >= value,
            }
        }
        match self {
            Filter::Equals { value: wanted, .. } => matches!(value, Some(TagValue::String(s)) if s == wanted),
            Filter::BoolEquals { value: wanted, .. } => matches!(value, Some(TagValue::Bool(b)) if b == wanted),
            Filter::In { values, .. } => matches!(value, Some(TagValue::String(s)) if values.contains(s)),
            Filter::NumberCompare { op, value: threshold, .. } => match value {
                Some(TagValue::Number(n)) => compare(*n, *op, *threshold),
                _ => false,
            },
            Filter::Has(_) => value.is_some(),
            Filter::NumberCompareWithZoomOffset { op, offset, threshold, .. } => match value {
                Some(TagValue::Number(n)) => compare(n - offset.value_at(zoom as u8) as f64, *op, *threshold),
                _ => false,
            },
            other => unreachable!("leaf_matches_value on a non-leaf filter {other:?}"),
        }
    }

    /// True if this filter's *matched feature set* can vary with the zoom
    /// passed to `matches` -- i.e. whether re-evaluating `matches` at a
    /// different zoom, against the same tags, can flip the result. Used to
    /// decide whether a compiled filter's matches can be cached across
    /// zooms within an integer bucket, or must be re-evaluated whenever
    /// zoom changes.
    ///
    /// This is a filter-level fact only. It says nothing about whether a
    /// *layer's* output is zoom-invariant: `match_tile` gates every layer
    /// by `min_zoom`/`max_zoom` (`layer_zoom_range`) *before* running its
    /// filter at all, so a layer with `min_zoom: 12` and a completely
    /// static filter still emits zero ops below zoom 12 and a full set at
    /// zoom 12 and above. A future cache built on this method must still
    /// apply that per-layer zoom gate on every zoom change -- `uses_zoom()
    /// == false` licenses reusing the filter's *matches*, not skipping the
    /// layer's zoom-range check.
    ///
    /// Must fail *safe*: misclassifying a zoom-dependent filter as static
    /// caches a match that should vary, silently rendering the wrong
    /// features at some zooms. Matches exhaustively on every `Filter`
    /// variant -- no catch-all arm -- so a future variant is a compile
    /// error here rather than a silent misclassification.
    pub(crate) fn uses_zoom(&self) -> bool {
        match self {
            Filter::Any
            | Filter::Never
            | Filter::Equals { .. }
            | Filter::BoolEquals { .. }
            | Filter::In { .. }
            | Filter::NumberCompare { .. }
            | Filter::GeometryType(_)
            | Filter::Has(_) => false,
            Filter::ZoomStep { .. } => true,
            Filter::NumberCompareWithZoomOffset { .. } => true,
            Filter::All(filters) | Filter::AnyOf(filters) => {
                filters.iter().any(Filter::uses_zoom)
            }
            Filter::Not(filter) => filter.uses_zoom(),
            Filter::Case { arms, fallback } => {
                arms.iter().any(|(cond, out)| cond.uses_zoom() || out.uses_zoom())
                    || fallback.uses_zoom()
            }
            Filter::MultiMatch { cases, fallback, .. } => {
                cases.iter().any(|(_, out)| out.uses_zoom()) || fallback.uses_zoom()
            }
            Filter::NumberMultiMatch { cases, fallback, .. } => {
                cases.iter().any(|(_, out)| out.uses_zoom()) || fallback.uses_zoom()
            }
        }
    }
}

/// The zoom at and above which a `fill-extrusion` layer draws as an
/// extrusion (spec §2.2, R1). Extrusions draw only on flat frames, and a
/// frame is flat exactly when its zoom is at or past the globe-to-Mercator
/// blend maximum ([`crate::render::globe::PROJECTION_BLEND_ZOOM_MAX`]).
/// Below it, a layer whose `minzoom` reaches lower keeps drawing its flat
/// footprint through a companion `StyleLayer::Fill` capped here.
pub const EXTRUSION_MIN_ZOOM: u8 = 12;

/// `fill-extrusion-height` or `fill-extrusion-base`, compiled (spec §2.2).
/// Resolved per feature at bake time, never folded to a zoom curve: both
/// are data-driven in every vendored style.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtrusionValue {
    Constant(f32),
    /// A bare `["get", key]`: one tag lookup, no JSON walk.
    Get(String),
    /// Any other per-feature expression, kept whole and evaluated with
    /// `style_spec::expression::eval_number_for_feature`.
    Feature { expr: Arc<serde_json::Value>, zoom_dependent: bool },
}

fn clamp_extrusion(v: f64) -> f32 {
    // MapLibre `fill_extrusion.vertex.glsl`: "Sub-terranian floors and
    // ceilings are clamped"; a runtime error takes the default (0).
    if v.is_finite() { v.max(0.0) as f32 } else { 0.0 }
}

impl ExtrusionValue {
    /// At `zoom`, clamped like MapLibre's shader: `max(0, v)`; a non-finite
    /// value or an evaluation error gives 0.
    #[must_use]
    pub fn resolve(&self, tags: &dyn crate::mvt::TagLookup, zoom: u8) -> f32 {
        match self {
            ExtrusionValue::Constant(v) => clamp_extrusion(f64::from(*v)),
            ExtrusionValue::Get(key) => match tags.get(key) {
                Some(crate::mvt::TagValue::Number(n)) => clamp_extrusion(*n),
                // A string or bool fails the property's number assertion in
                // MapLibre, which then takes the default.
                _ => 0.0,
            },
            ExtrusionValue::Feature { expr, .. } => {
                crate::style_spec::expression::eval_number_for_feature(expr, tags, zoom).map_or(0.0, clamp_extrusion)
            }
        }
    }

    /// `(value at zoom, value at zoom + 1)`: equal unless the value is a
    /// `Feature` expression that reads `["zoom"]`.
    #[must_use]
    pub fn resolve_pair(&self, tags: &dyn crate::mvt::TagLookup, zoom: u8) -> (f32, f32) {
        let from = self.resolve(tags, zoom);
        match self {
            ExtrusionValue::Feature { zoom_dependent: true, .. } => (from, self.resolve(tags, zoom.saturating_add(1))),
            _ => (from, from),
        }
    }
}

/// The style-spec `light.anchor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightAnchor {
    Map,
    Viewport,
}

/// The light a `fill-extrusion` layer is shaded with at ONE zoom: MapLibre's
/// root `light` object, or a Mapbox v3 `directional` light mapped onto it
/// (R16). Read it out of an [`ExtrusionLightTable`] at the frame's zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionLight {
    pub anchor: LightAnchor,
    /// `[radial, azimuthal degrees, polar degrees]`, style-spec `light.position`.
    pub position: [f32; 3],
    pub color: Rgba,
    pub intensity: f32,
}

impl Default for ExtrusionLight {
    /// The style spec's defaults for the root `light` object.
    fn default() -> Self {
        ExtrusionLight {
            anchor: LightAnchor::Viewport,
            position: [1.15, 210.0, 30.0],
            color: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            intensity: 0.5,
        }
    }
}

/// The style's extrusion light at every integer zoom `0..=22`, the way
/// `Scope::ground_radiance` carries the scene's radiance: Mapbox Standard's
/// `directional.intensity` is a `["zoom"]` interpolate (0 at z12, 0.5 from
/// z14 under the `night` preset), so one value per style would light every
/// building with the wrong one. Style-global; every extrusion layer of a
/// style carries a clone of the same table (the levels are shared).
///
/// A table of ONE level is a constant light -- the MapLibre root `light`,
/// whose properties this compiler reads as literals, and the default.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtrusionLightTable {
    /// Indexed by integer zoom; the last level holds for every zoom past it.
    /// Never empty.
    levels: Arc<[ExtrusionLight]>,
}

impl ExtrusionLightTable {
    /// The same light at every zoom.
    #[must_use]
    pub fn constant(light: ExtrusionLight) -> Self {
        ExtrusionLightTable { levels: Arc::from([light]) }
    }

    /// One light per integer zoom, starting at zoom 0. An empty `levels`
    /// gives the default light.
    #[must_use]
    pub fn per_zoom(levels: Vec<ExtrusionLight>) -> Self {
        if levels.is_empty() {
            return Self::default();
        }
        ExtrusionLightTable { levels: Arc::from(levels) }
    }

    /// The light at the frame's (fractional) `zoom`: linear between the two
    /// integer levels around it, which is exact for a `["linear"]` zoom
    /// interpolate whose stops sit on integer zooms (Standard's do). The
    /// anchor is the lower level's; it never varies in practice. Clamped
    /// to the table at both ends. The azimuth is lerped numerically, with
    /// no wrap at 360: Standard's direction is constant across zoom within
    /// a preset, so no vendored style crosses it.
    #[must_use]
    pub fn at(&self, zoom: f64) -> ExtrusionLight {
        let last = self.levels.len() - 1;
        let zoom = if zoom.is_finite() { zoom.max(0.0) } else { 0.0 };
        let lower = (zoom.floor() as usize).min(last);
        let upper = (lower + 1).min(last);
        let t = if upper == lower { 0.0 } else { (zoom - lower as f64).clamp(0.0, 1.0) as f32 };
        let (a, b) = (self.levels[lower], self.levels[upper]);
        let mix = |x: f32, y: f32| x + (y - x) * t;
        ExtrusionLight {
            anchor: a.anchor,
            position: [mix(a.position[0], b.position[0]), mix(a.position[1], b.position[1]), mix(a.position[2], b.position[2])],
            color: Rgba { r: mix(a.color.r, b.color.r), g: mix(a.color.g, b.color.g), b: mix(a.color.b, b.color.b), a: mix(a.color.a, b.color.a) },
            intensity: mix(a.intensity, b.intensity),
        }
    }
}

impl Default for ExtrusionLightTable {
    fn default() -> Self {
        Self::constant(ExtrusionLight::default())
    }
}

/// The style-spec `fill-extrusion-translate-anchor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslateAnchor {
    Map,
    Viewport,
}

// A style holds one `Vec` of these, compiled once; boxing the big variant
// would put an indirection in every per-layer match of the bake to save
// a few hundred bytes per layer.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum StyleLayer {
    Background {
        id: String,
        color: ZoomValue<Rgba>,
        min_zoom: Option<u8>,
        max_zoom: Option<u8>,
    },
    Fill {
        id: String,
        source_layer: String,
        filter: Filter,
        color: ZoomValue<Rgba>,
        opacity: ZoomValue<f32>,
        /// `fill-outline-color`: a hairline traced along the fill's own
        /// boundary, over the top of it. `None` -- the value for a layer
        /// that declares no `fill-outline-color`, and for one that switches
        /// the edge off with `fill-antialias: false` -- means NO outline
        /// geometry is emitted at all.
        ///
        /// # Why `None` rather than "default to `fill-color`"
        ///
        /// MapLibre's `FillStyleLayer::recalculate`
        /// (`src/style/style_layer/fill_style_layer.ts`) substitutes
        /// `fill-color` for an unspecified `fill-outline-color`, and
        /// `draw_fill.ts`'s `drawOutline` then draws that outline whenever
        /// `fill-antialias` is true (the spec default). It can afford to,
        /// because there the outline pass exists PRECISELY to antialias the
        /// polygon edge: `fill_outline.fragment.glsl` ramps
        /// `1 - smoothstep(0, 1, dist)` across a one-pixel band, so a
        /// same-coloured outline is not a no-op -- it IS the edge's only
        /// antialiasing. It is also nearly free: `gl.LINES` over
        /// `FillBucket::indexArray2`, a second index buffer into the
        /// vertices the fill already uploaded, with no new vertices at all.
        ///
        /// Neither half holds here. An outline in this renderer is real
        /// stroke geometry through `render::mesh::append_stroke` -- its own
        /// tessellation, its own vertices -- so emitting one per polygon
        /// regardless of what the style asked for would bake a second,
        /// stroked copy of every footprint in the tile (1.1M of them over
        /// Tokyo at station zoom) to draw a line the exact colour of the
        /// fill beneath it. That line is invisible over an opaque fill, and
        /// actively WRONG over a translucent one: it blends a second time
        /// against the fill it covers and darkens every edge.
        ///
        /// So the visible property is honoured exactly, and the invisible
        /// default is skipped. A style that declares `fill-outline-color`
        /// gets MapLibre's result; a style that does not gets MapLibre's
        /// result too, because the outline MapLibre would have drawn there
        /// is indistinguishable from the fill it lies on.
        outline_color: Option<ZoomValue<Rgba>>,
        min_zoom: Option<u8>,
        max_zoom: Option<u8>,
    },
    /// A `fill-extrusion` layer from [`EXTRUSION_MIN_ZOOM`] up (spec §2.2,
    /// R1). One per colour case, like `Fill`; below the floor a layer whose
    /// `minzoom` reaches lower draws through a companion flat `Fill`.
    Extrusion {
        id: String,
        source_layer: String,
        filter: Filter,
        /// Ordinal of the source `fill-extrusion` layer among the style's
        /// extrusion layers. Every colour case of one source layer shares it.
        group: u16,
        color: ZoomValue<Rgba>,
        /// Layer-level (not data-driven): a per-layer uniform, never folded
        /// into colour alpha.
        opacity: ZoomValue<f32>,
        height: ExtrusionValue,
        base: ExtrusionValue,
        vertical_gradient: bool,
        /// Pixels; literal only in v1 (deviation 4): an expression
        /// compiles to `[0, 0]`. The plan turns a `Viewport` one by the
        /// bearing (`render::extrusion::frame_translate`).
        translate: [f32; 2],
        translate_anchor: TranslateAnchor,
        /// The style's light per zoom; read with `ExtrusionLightTable::at`
        /// at the frame's zoom.
        light: ExtrusionLightTable,
        min_zoom: Option<u8>,
        max_zoom: Option<u8>,
    },
    Line {
        id: String,
        source_layer: String,
        filter: Filter,
        color: ZoomValue<Rgba>,
        width: ZoomValue<f32>,
        opacity: ZoomValue<f32>,
        /// `line-gap-width`: the width of a HOLE punched down the middle of
        /// the stroke, in logical pixels. Style-spec default `0`, which is
        /// "no hole" and is what all but a handful of layers declare.
        ///
        /// This is the whole of Mapbox's road-casing system, and it is NOT
        /// "a wider line drawn underneath". A non-zero gap means the stroke
        /// occupies `gap/2 .. gap/2 + width` on EACH side of the centreline:
        /// two parallel ribbons that hug the fill layer drawn on top of them
        /// at every zoom, every interpolated width, and every join -- see
        /// `PaintOp::Line`'s `gap_from` and `render/shader.wgsl`'s
        /// `aa_inset_px` for how that is actually drawn.
        ///
        /// A paint property, so a full `ZoomValue` like `width` beside it:
        /// Mapbox Streets ramps every `*-case` layer's gap with the same
        /// `["interpolate", ["exponential", 1.5], ["zoom"], ...]` shape it
        /// ramps the fill's width with, and a casing that failed to track
        /// its fill would show as a coloured fringe on one side.
        gap_width: ZoomValue<f32>,
        /// `layout.line-cap`. Style-spec default `butt`.
        ///
        /// A plain enum rather than a `ZoomValue`, and that is a DELIBERATE
        /// scoping decision rather than a shortcut -- see `LineCap`.
        cap: LineCap,
        /// `layout.line-join`. Style-spec default `miter`, and geometry
        /// rather than paint in exactly the way `cap` is. See `LineJoin`.
        join: LineJoin,
        /// `line-dasharray`. `DashPattern::none()` -- the value for every
        /// layer that doesn't declare the property, and the degradation
        /// target for one this crate can't represent -- means a solid line.
        dash: ZoomValue<DashPattern>,
        min_zoom: Option<u8>,
        max_zoom: Option<u8>,
    },
    Symbol {
        id: String,
        source_layer: String,
        filter: Filter,
        text_field: Option<TextField>,
        icon_image: Option<IconImage>,
        text_size: LayoutValue,
        text_offset_x: LayoutValue,
        text_offset_y: LayoutValue,
        /// `layout.text-radial-offset`, in EMS: how far from the anchor
        /// point the text's anchored edge sits, in the direction
        /// `text-anchor` points away from. `None` when the layer declares
        /// none. When it resolves above zero it REPLACES `text-offset` for
        /// a point label, as MapLibre's `symbol_layout` does -- see
        /// `radial_text_offset`.
        text_radial_offset: Option<LayoutValue>,
        /// `layout.text-anchor`: which part of the label lands on the
        /// anchor point that `text_offset_x`/`_y` just moved. The two are
        /// independent and both are needed -- the offset says WHERE the
        /// point is, the anchor says WHICH BIT OF THE TEXT goes there. See
        /// `TextAnchor`, and `TextAnchorValue` for why a layer carries an
        /// expression here rather than one keyword.
        text_anchor: TextAnchorValue,
        /// `layout.text-max-width`: how wide the label is allowed to get
        /// before it wraps, in EMS -- multiples of the resolved
        /// `text-size`, not pixels. `DEFAULT_TEXT_MAX_WIDTH_EM` when the
        /// layer declares none (which still means "wrap"; see the
        /// constant).
        ///
        /// A plain `f32`, not a `LayoutValue`, for the same reason
        /// `text_anchor` is a plain enum: the property is read as a literal
        /// number and nothing else, so a style that drives it from an
        /// expression (Mapbox Streets does, on `water-point-label` and
        /// `transit-label`) degrades to the spec default with the layer
        /// otherwise intact.
        text_max_width: f32,
        /// `"icon-size"`: a multiplier on the icon's own pixel dimensions in
        /// the sprite sheet. Mapbox's default is `1`, i.e. draw the sprite at
        /// its native size.
        icon_size: LayoutValue,
        /// `"symbol-sort-key"`: the within-layer placement priority, LOWER
        /// first. `None` when the layer declares none (or declares one in a
        /// shape `compile_symbol_sort_key` doesn't understand).
        symbol_sort_key: Option<SymbolSortKey>,
        text_color: ZoomValue<Rgba>,
        text_halo_color: ZoomValue<Rgba>,
        text_halo_width: LayoutValue,
        /// `paint.text-halo-blur`: how far the halo's OUTER edge fades out,
        /// in the same points `text_halo_width` is in. `0` -- the style-spec
        /// default, and what most layers leave it at -- is a hard edge.
        ///
        /// Independent of the width, and both are needed: the width says how
        /// far the halo reaches, the blur says how sharply it stops. 13 of
        /// the vendored `examples/mapbox-style.json`'s symbol layers set one
        /// (`0.5` on nine, `1` on four), and every one of them was rendering
        /// with a hard edge.
        text_halo_blur: LayoutValue,
        /// `layout.text-font`: the faces this layer's labels are set in, in
        /// preference order.
        ///
        /// Face names, not family names -- `"DIN Pro Medium"`, `"Noto Sans
        /// Italic"` -- because that is the glyph-PBF naming convention
        /// Mapbox's font service keys its ranges by. Splitting one into a
        /// family a font database can look up, and resolving the stack
        /// against what is actually installed, is
        /// `font_fallback::resolve_font_stack`'s job; this carries the
        /// style's request through untouched.
        ///
        /// Never empty for a layer that declares none: it holds
        /// `font_fallback::DEFAULT_FONT_STACK`, the style spec's own
        /// default, so the draw path has one rule and no absent case.
        text_font: crate::font_fallback::FontStack,
        /// `paint.icon-opacity`: a multiplier on the ICON's alpha, and on
        /// nothing else -- the sprite half of the symbol only. Style-spec
        /// default `1`.
        ///
        /// Compiled by the very same `compile_zoom_value` call
        /// `fill-opacity`/`line-opacity` go through, because the property is
        /// zoom-dependent in every real style that sets it: Mapbox Standard
        /// hides `poi-label`'s icons until z17, `traffic signals`' until z16
        /// and `road exit shield`'s until z19 with plain
        /// `["step", ["zoom"], 0, ..., 1]` ramps. Not compiling it at all is
        /// why this renderer drew ~18 POI/transit symbols at z15 that Mapbox
        /// draws none of.
        ///
        /// `0` means the icon does not draw. It does NOT mean the symbol
        /// stops taking part in collision placement: mapbox-gl-js's
        /// `placement.ts` reads only `symbol-z-offset` off `paint`, so an
        /// invisible symbol still occupies its slot there, and matching
        /// that is what keeps the labels that DO draw in the same places
        /// as the reference. See `map_view`'s symbol loop.
        icon_opacity: SymbolOpacity,
        /// `paint.text-opacity`, the TEXT half's independent twin of
        /// `icon_opacity` -- applied to the label's colour and its halo,
        /// and to neither the sprite nor collision. A symbol may well have
        /// one visible and the other not: Mapbox Standard's
        /// `transit-stop-label` fades its text in at z16 and its bus icons
        /// at z16.5.
        text_opacity: SymbolOpacity,
        /// `paint.icon-color` -- the fill an SDF sprite is drawn in. Has no
        /// effect on a plain RGBA sprite, which carries its own colour;
        /// that is the style spec's own rule, not a simplification.
        /// Mapbox's default is opaque black.
        icon_color: ZoomValue<Rgba>,
        /// `paint.icon-halo-color`, the border drawn outside an SDF
        /// sprite's shape. Mapbox's default is fully transparent black,
        /// which together with a default `icon_halo_width` of 0 means "no
        /// halo unless asked for".
        icon_halo_color: ZoomValue<Rgba>,
        /// `paint.icon-halo-width` in LOGICAL points. Mapbox's default is
        /// 0. A `LayoutValue` rather than a `ZoomValue` for the same reason
        /// `text_halo_width` is one: a style may vary it per feature.
        icon_halo_width: LayoutValue,
        placement: SymbolPlacementValue,
        /// `layout.symbol-spacing` in LOGICAL pixels -- the distance between
        /// consecutive anchors along a line. Style-spec default 250.
        /// Resolved per feature and zoom, same as `text_size`/`icon_size`.
        spacing_px: LayoutValue,
        /// `layout.icon-rotate`, in degrees clockwise, added to the line's
        /// own direction. Liberty's `road_one_way_arrow_opposite` uses 180
        /// to point arrows against the digitised direction.
        icon_rotate_deg: f32,
        /// `layout.icon-rotation-alignment`, resolved per `RotationAlignment`'s
        /// doc comment. Governs the ICON only -- see `text_rotation_alignment`
        /// for the separate, independently-set property that governs the text.
        rotation_alignment: RotationAlignment,
        /// `layout.text-rotation-alignment`, resolved exactly the way
        /// `rotation_alignment` (`icon-rotation-alignment`) is: `Map` under
        /// line placement, `Viewport` under point placement, when the
        /// style-spec's `auto` (or an absent property) is in play. The two
        /// properties are independent -- `road-number-shield` sets both to
        /// `"viewport"` explicitly, but a style is free to set only one.
        /// Read by `symbol_pass::text_rotation_alignment`, and it chooses
        /// which of the two draw paths a LINE-placed label takes. `Map`
        /// means the text follows its road: `symbol_pass::line_glyph_quads`
        /// distributes the glyphs along the polyline and turns each one
        /// with the line beneath it. `Viewport` means the line's direction
        /// is not an input at all -- the label is ONE horizontal block at
        /// the anchor, drawn by the same `glyphon::TextArea` path a point
        /// label takes, which is what puts a shield's `BKE` inside its
        /// badge. Distributing the glyphs along the road while holding each
        /// one level is neither, and stacks a shield's letters vertically
        /// on a north-south expressway; it was the v0.6.0 defect.
        /// `match_tile_symbols` does not read it at all -- both alignments
        /// resolve their `text_field`.
        text_rotation_alignment: RotationAlignment,
        /// `layout.icon-text-fit` and `icon-text-fit-padding`: the icon
        /// grows (or shrinks) to wrap the label, as Mapbox Streets' Tokyo
        /// intersection names (`road-intersection`) sit on a white plate
        /// that is a 26x14 sprite stretched to the text. Ignoring it drew
        /// the plate at its native size, a white box over part of the name.
        icon_text_fit: IconTextFit,
        /// `icon-allow-overlap && icon-ignore-placement`: this layer's
        /// symbols are drawn regardless of collision and do not block
        /// others. Only the PAIR is honoured -- `collision::place_symbols`
        /// has no way to express "always draw but still occupy space".
        bypass_collision: bool,
        min_zoom: Option<u8>,
        max_zoom: Option<u8>,
    },
    /// A tile of IMAGERY -- satellite, shaded relief, a scanned chart --
    /// drawn as the tile's own square wearing a texture, under everything
    /// the vector layers draw.
    ///
    /// # What makes this variant structurally unlike the other four
    ///
    /// Every other variant becomes GEOMETRY inside a tile's single mesh:
    /// `resolve_plan` turns it into `PaintOp`s, `tessellate_plan` turns
    /// those into triangles, and one `TilePipeline` draw call draws the
    /// lot in style-layer order. A raster layer has no geometry in the
    /// source bytes to turn into anything. Its pixels are not in the MVT
    /// at all -- they arrive from a SEPARATE tile source as PNG or JPEG.
    ///
    /// So this variant produces **no `PaintOp`**, exactly as
    /// [`StyleLayer::Symbol`] produces none (see `resolve_plan`'s arm for
    /// both), and is drawn by `render::raster_pipeline` in its own ordered
    /// group instead. `LayerMatch::Raster` exists only to keep
    /// `TileMatch::entries` aligned 1:1 with `style.layers`.
    ///
    /// # Why there is no `source_layer`
    ///
    /// Because the style spec gives a raster layer none, and asking for
    /// one is not a harmless extra field: `style_spec::compile_layer`
    /// reads `"source-layer"` with a hard `?` that fails the WHOLE style,
    /// so a raster layer must return before reaching it. That is why the
    /// raster arm sits beside `background`'s early return rather than in
    /// the `match layer_type` below it.
    ///
    /// # Why it carries `source`, when no other variant does
    ///
    /// It is the first variant that can name a tile source OTHER than the
    /// one the vector layers read. `source` is carried and reported, but
    /// this pass does not yet resolve it through a registry of sources --
    /// `MapView` holds one optional raster source, not a map. See the
    /// plan document's Task 2 for why a registry is deferred rather than
    /// half-built.
    Raster {
        id: String,
        /// The style `sources` key this layer's imagery comes from.
        /// Carried verbatim; see the note above on what is and is not
        /// done with it yet.
        source: String,
        /// `paint.raster-opacity`, style-spec default 1. Multiplies into
        /// the sampled texel's alpha -- MapLibre's `u_opacity`
        /// (`raster.fragment.glsl:28`, `draw_raster.ts:104`).
        opacity: ZoomValue<f32>,
        min_zoom: Option<u8>,
        max_zoom: Option<u8>,
    },
    /// A `circle` layer (MapLibre's `circle_style_layer`): one circle per
    /// point of a POINT feature, drawn through the instanced circle
    /// renderer (`render::circles`). One per data-driven case, like `Fill`
    /// and `Line`: the filter carries the case.
    ///
    /// The basemap's tile bake draws nothing for it, and a basemap STYLE
    /// never compiles one (`style_spec::compile_layer_json` is the only
    /// entry that does; see its doc for the vendored styles that carry
    /// circle layers). A circle layer reaches the screen through the
    /// vector-overlay path (`overlay::vector_tiles`), which resolves each
    /// feature's paint from these fields and pushes the points as a point
    /// source.
    Circle {
        id: String,
        source_layer: String,
        filter: Filter,
        paint: CirclePaint,
        min_zoom: Option<u8>,
        max_zoom: Option<u8>,
    },
}

/// A `circle` layer's paint, one data-driven case of it (see
/// `StyleLayer::Circle`). Every value is the style spec's own property, at
/// its spec default when the layer does not declare it.
#[derive(Debug, Clone)]
pub struct CirclePaint {
    /// `circle-radius`, logical pixels. Default 5.
    pub radius: ZoomValue<f32>,
    /// `circle-color`. Default black.
    pub color: ZoomValue<Rgba>,
    /// `circle-opacity`, multiplied into the colour's alpha. Default 1.
    pub opacity: ZoomValue<f32>,
    /// `circle-stroke-width`, logical pixels, OUTSIDE the radius (MapLibre:
    /// the quad grows by it, `circle.vertex.glsl`). Default 0.
    pub stroke_width: ZoomValue<f32>,
    /// `circle-stroke-color`. Default black.
    pub stroke_color: ZoomValue<Rgba>,
    /// `circle-stroke-opacity`. Default 1.
    pub stroke_opacity: ZoomValue<f32>,
    /// `circle-blur`: 0 is a crisp edge, 1 fades from the centre. Default 0.
    pub blur: ZoomValue<f32>,
    /// `circle-pitch-alignment` / `circle-pitch-scale` (layer-wide, not
    /// data-driven in the spec).
    pub pitch: crate::render::circles::CirclePitch,
}

/// One feature's circle paint, resolved at a zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedCircle {
    pub radius: f32,
    /// `circle-color` with `circle-opacity` folded into its alpha.
    pub color: Rgba,
    pub stroke_width: f32,
    /// `circle-stroke-color` with `circle-stroke-opacity` folded in.
    pub stroke_color: Rgba,
    pub blur: f32,
}

impl CirclePaint {
    /// The spec defaults (`circle-radius` 5, black, opaque, no stroke, no
    /// blur, `viewport` / `map`).
    #[must_use]
    pub fn spec_default() -> Self {
        let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        Self {
            radius: ZoomValue::constant(5.0),
            color: ZoomValue::constant(black),
            opacity: ZoomValue::constant(1.0),
            stroke_width: ZoomValue::constant(0.0),
            stroke_color: ZoomValue::constant(black),
            stroke_opacity: ZoomValue::constant(1.0),
            blur: ZoomValue::constant(0.0),
            pitch: crate::render::circles::CirclePitch::default(),
        }
    }

    /// Every property at (fractional) `zoom`, opacities folded into the
    /// colours' alpha the way MapLibre's `circle.fragment.glsl` multiplies
    /// them (`opacity_t` on the fill, `stroke_opacity` on the stroke).
    #[must_use]
    pub fn at(&self, zoom: f64) -> ResolvedCircle {
        let color = self.color.value_at_zoom(zoom);
        let stroke = self.stroke_color.value_at_zoom(zoom);
        ResolvedCircle {
            radius: self.radius.value_at_zoom(zoom).max(0.0),
            color: Rgba { a: color.a * self.opacity.value_at_zoom(zoom).clamp(0.0, 1.0), ..color },
            stroke_width: self.stroke_width.value_at_zoom(zoom).max(0.0),
            stroke_color: Rgba { a: stroke.a * self.stroke_opacity.value_at_zoom(zoom).clamp(0.0, 1.0), ..stroke },
            blur: self.blur.value_at_zoom(zoom).max(0.0),
        }
    }
}

/// `layout.line-cap` -- what is drawn at the two ENDS of a line, beyond its
/// last vertex. Style-spec default `butt` (nothing at all).
///
/// # Why this is a constant per layer and not a `ZoomValue`
///
/// Every other line property this crate compiles is PAINT: it can change
/// between one zoom and the next without the triangulation changing, which is
/// exactly what `render::mesh::rewrite_paint_attributes` exploits -- a zoom
/// crossing rewrites colour/width/gap/dash into the vertices already in the
/// buffer and never re-runs lyon.
///
/// A cap is not paint. It IS geometry: `LineCap::Round` makes lyon emit a fan
/// of vertices that `Butt` does not emit at all. A cap that varied with zoom
/// would therefore have to invalidate the tessellation cache, and the cache's
/// invalidation test (`map_view::ops_share_geometry_set`) compares the ops'
/// GEOMETRY -- a stepped cap would slide straight past it and leave the old
/// triangulation on screen, silently, at the new zoom.
///
/// So the property is read as a literal keyword and nothing else. Real styles
/// do write `["step", ["zoom"], "butt", 14, "round"]` (the vendored Mapbox
/// Streets document does, on 31 of its 134 layers); such an expression
/// degrades to the style spec's own default here, with a notice, rather than
/// being silently resolved at one zoom and then left stale at every other.
/// Both styles this renderer actually draws -- `examples/mapbox-style.json`
/// and `examples/liberty-style.json` -- write a bare `"round"`, which is the
/// case this covers exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    Square,
}

impl LineCap {
    /// The style-spec keyword, or `None` for a string that names no cap this
    /// crate knows. `None` is the caller's cue to fall back to the default
    /// with a notice -- see `style_spec`'s `"line"` arm.
    pub fn from_style_name(name: &str) -> Option<Self> {
        match name {
            "butt" => Some(Self::Butt),
            "round" => Some(Self::Round),
            "square" => Some(Self::Square),
            _ => None,
        }
    }
}

/// `layout.line-join` -- what fills the wedge between two consecutive
/// segments of a line. Style-spec default `miter`.
///
/// Constant per layer for exactly the reasons `LineCap` is; see that type.
///
/// # The miter limit
///
/// `miter` alone is not a complete answer: as a turn approaches a hairpin
/// the miter tip runs away to infinity, and every renderer bounds it. Mapbox
/// and MapLibre bound it with `line-miter-limit` (style-spec default **2**),
/// applied in `line_bucket.ts`'s `addLine` as
///
/// ```text
/// if (currentJoin === 'miter' && miterLength > miterLimit) currentJoin = 'bevel';
/// ```
///
/// where `miterLength` is `1 / cos(half the angle between the join normal
/// and the next segment's normal)` -- the miter tip's distance from the
/// centreline in HALF-WIDTHS.
///
/// lyon spells the same quantity differently, and the difference is a factor
/// of two, not a matter of taste: `miter_limit_is_exceeded`
/// (`lyon_tessellation/src/stroke.rs`) bevels when `normal.length() > 2 *
/// miter_limit`, having defined its limit as miter length over FULL stroke
/// width rather than over half-width. So MapLibre's limit of 2 is lyon's
/// limit of 1, and that is what `render::mesh::append_stroke` passes. Taking
/// lyon's own default of 4 instead would let a spike reach four times as far
/// as Mapbox allows, which is precisely the artifact a limit exists to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineJoin {
    Bevel,
    Round,
    #[default]
    Miter,
}

impl LineJoin {
    /// The style-spec keyword, or `None` for a string that names no join
    /// this crate knows. See `LineCap::from_style_name`.
    pub fn from_style_name(name: &str) -> Option<Self> {
        match name {
            "bevel" => Some(Self::Bevel),
            "round" => Some(Self::Round),
            "miter" => Some(Self::Miter),
            _ => None,
        }
    }
}

/// A compiled `"text-field"` layout property. Scoped to the exact shapes
/// seen in the real vendored styles: a bare literal string, `["get", key]`,
/// `["coalesce", ["get", k1], ["get", k2], ...]`, the bilingual
/// `["case", ["has", k], ["concat", ...], ["coalesce", ...]]` that every
/// OpenMapTiles label layer uses, and `["to-string", ["get", k]]` for a
/// road shield's numeric `ref`.
///
/// Those last two are why 23 of the Liberty style's 25 symbol layers used to
/// be skipped: one unsupported shape took every place name, street name, POI,
/// water name and airport with it.
#[derive(Debug, Clone, PartialEq)]
pub enum TextField {
    Literal(String),
    Get(String),
    Coalesce(Vec<String>),
    /// `["concat", ...]`, reusing the part type `icon-image`'s own `concat`
    /// already compiles to -- same shapes, same numeric `to-string` rule.
    Concat(Vec<ConcatPart>),
    /// `["case", ["has", key], then, otherwise]`. Mirrors
    /// `IconImage::CaseHas`, which compiles the identical shape for sprite
    /// names.
    CaseHas { key: String, then: Box<TextField>, otherwise: Box<TextField> },
    /// `["to-string", ["get", key]]` -- a road shield's `ref`, which arrives
    /// as a NUMBER in real tiles.
    ToStringGet(String),
    /// `["step", ["zoom"], base, z1, f1, ...]` -- Mapbox Streets'
    /// `transit-label`, which names only rail stops at z13, adds the rest at
    /// z14 and every stop at z18. Resolved at the BAKE zoom (a layout
    /// property: MapLibre evaluates it once per tile at the tile's zoom).
    /// `stops` ascending by zoom.
    ZoomStep { base: Box<TextField>, stops: Vec<(u8, TextField)> },
    /// `["step", ["get", key], base, t1, f1, ...]` -- `airport-label`, which
    /// shows only the IATA `ref` for a minor airport (`sizerank >= 15`). A
    /// missing or non-numeric tag takes `base`, exactly as
    /// `DataStepShape::resolve` does. `stops` ascending by threshold.
    DataStep { key: String, base: Box<TextField>, stops: Vec<(f64, TextField)> },
    /// `["match", ["get", key], labels, field, ..., default]` over a STRING
    /// tag (`transit-label`'s `mode`). A missing or non-string tag takes
    /// `default`, as `MatchStepShape::resolve` does.
    Match { key: String, arms: Vec<(Vec<String>, TextField)>, default: Box<TextField> },
    /// `["concat", ...]` whose parts are not all the plain `ConcatPart`
    /// shapes -- `airport-label`'s `["concat", ["get", "ref"], " -\n",
    /// ["coalesce", ["get", "name_en"], ["get", "name"]]]`. Each part is a
    /// whole `TextField`; a part with nothing to say contributes nothing,
    /// the same rule `Concat` follows.
    ConcatFields(Vec<TextField>),
}

impl TextField {
    /// `resolve_at` at zoom 0, for a field with no zoom `step` in it -- every
    /// shape but `ZoomStep`. The bake path calls `resolve_at` with the real
    /// tile zoom.
    pub fn resolve(&self, tags: &dyn TagLookup) -> Option<String> {
        self.resolve_at(tags, 0)
    }

    /// Resolves against a feature's tags at the bake zoom. Returns `None`
    /// when the field has nothing to say -- callers treat that as "skip
    /// this label", not an error. An EMPTY string is nothing to say too:
    /// `transit-label` writes `""` for "no name at this zoom", and MapLibre
    /// draws no text for it.
    pub fn resolve_at(&self, tags: &dyn TagLookup, zoom: u8) -> Option<String> {
        self.resolve_inner(tags, zoom).filter(|text| !text.is_empty())
    }

    fn resolve_inner(&self, tags: &dyn TagLookup, zoom: u8) -> Option<String> {
        fn get_string(tags: &dyn TagLookup, key: &str) -> Option<String> {
            match tags.get(key) {
                Some(TagValue::String(s)) => Some(s.clone()),
                _ => None,
            }
        }
        /// `to-string` on a string is the identity; on a number it is Rust's
        /// own `Display`, which already prints `95.0_f64` as `"95"` rather
        /// than `"95.0"` -- a route number, not a measurement.
        ///
        /// Deliberately unlike `ConcatPart::ToStringGet` in
        /// `IconImage::resolve`, which REJECTS a fractional number: there it
        /// could only ever name a sprite that does not exist, whereas here
        /// `"95.5"` is simply the string the tile asked for.
        fn to_string_get(tags: &dyn TagLookup, key: &str) -> Option<String> {
            match tags.get(key) {
                Some(TagValue::String(s)) => Some(s.clone()),
                Some(TagValue::Number(n)) => Some(format!("{n}")),
                _ => None,
            }
        }
        match self {
            TextField::Literal(s) => Some(s.clone()),
            TextField::Get(key) => get_string(tags, key),
            TextField::Coalesce(keys) => keys.iter().find_map(|k| get_string(tags, k)),
            TextField::ToStringGet(key) => to_string_get(tags, key),
            TextField::CaseHas { key, then, otherwise } => {
                if tags.contains_key(key) { then.resolve_inner(tags, zoom) } else { otherwise.resolve_inner(tags, zoom) }
            }
            TextField::ZoomStep { base, stops } => stops
                .iter()
                .rev()
                .find(|(z, _)| *z <= zoom)
                .map_or(base.as_ref(), |(_, field)| field)
                .resolve_inner(tags, zoom),
            TextField::DataStep { key, base, stops } => {
                let picked = match tags.get(key) {
                    Some(TagValue::Number(n)) => {
                        stops.iter().rev().find(|(threshold, _)| *n >= *threshold).map(|(_, field)| field)
                    }
                    _ => None,
                };
                picked.unwrap_or(base).resolve_inner(tags, zoom)
            }
            TextField::Match { key, arms, default } => {
                let picked = match tags.get(key) {
                    Some(TagValue::String(value)) => {
                        arms.iter().find(|(labels, _)| labels.iter().any(|l| l == value)).map(|(_, field)| field)
                    }
                    _ => None,
                };
                picked.unwrap_or(default).resolve_inner(tags, zoom)
            }
            TextField::ConcatFields(parts) => {
                let mut out = String::new();
                for part in parts {
                    if let Some(text) = part.resolve_inner(tags, zoom) {
                        out.push_str(&text);
                    }
                }
                (!out.trim().is_empty()).then_some(out)
            }
            // A missing part contributes NOTHING rather than killing the whole
            // label -- which is where this deliberately differs from
            // `IconImage::Concat`, whose missing part must yield `None`
            // because a half-assembled sprite name names no sprite.
            //
            // A label is not a lookup key. The real shape here is
            // `["concat", ["get", "name:latin"], " ", ["get", "name:nonlatin"]]`
            // guarded by `["has", "name:nonlatin"]`: the guard proves the
            // SECOND half is present and says nothing about the first, so a
            // feature with only a non-Latin name would lose its label
            // entirely under the sprite rule. Blank output is still `None`,
            // so nothing draws an empty label.
            TextField::Concat(parts) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        ConcatPart::Literal(s) => out.push_str(s),
                        // Both `get` shapes go through `to_string_get`:
                        // inside a `concat` the style spec stringifies every
                        // argument, so an explicit `to-string` wrapper adds
                        // nothing (see `ConcatPart::Get`). `get_string` is
                        // still what `TextField::Get`/`Coalesce` use --
                        // those are bare `get`s outside any `concat`.
                        ConcatPart::Get(key) | ConcatPart::ToStringGet(key) => {
                            out.push_str(to_string_get(tags, key).unwrap_or_default().as_str())
                        }
                    }
                }
                (!out.trim().is_empty()).then_some(out)
            }
        }
    }
}

/// One piece of a `["concat", ...]` sprite name.
#[derive(Debug, Clone, PartialEq)]
pub enum ConcatPart {
    Literal(String),
    /// `["get", k]`. Inside a `concat` the style spec stringifies every
    /// argument, so this reads a number as readily as a string -- Liberty's
    /// road shields concat a numeric `ref_length` with no `to-string` at all.
    Get(String),
    /// `["to-string", ["get", k]]`. `reflen` arrives as a NUMBER in real
    /// tiles, so this is what turns `3` into `"3"` -- which, inside a
    /// `concat`, is exactly what a bare `Get` already does. The two variants
    /// stay distinct only so a compiled style still records which shape the
    /// JSON wrote.
    ToStringGet(String),
}

/// A compiled `"icon-image"` layout property. Scoped to the exact shapes
/// seen in the real vendored styles: a bare literal string, `["get", key]`,
/// `["coalesce", ["image", ["get", k1]], ["image", ["get", k2]], ...]` (the
/// real `maki_beta`/`maki` fallback shape), a zoom-only `["step",
/// ["zoom"], base, t1, v1, ...]` over string outputs (e.g.
/// `oneway-small`/`oneway-large`), that same zoom `step` with a
/// DATA-DRIVEN name at each zoom (`ZoomOfDataIcon`, the real city-label
/// shape), a `["concat", ...]` sprite name assembled from feature
/// properties (the real road-shield shape), a `["case", ["has", k],
/// <then>, <else>]` picking between the shield data's two generations
/// (`CaseHas`, the real `road-number-shield` shape), and a whole-expression
/// `["to-string", ["get", k]]` (`ToStringGet`, Liberty's `poi_transit`
/// bus-stop sprite), and a `["match", ["get", k], ...]` over string labels
/// (`MatchGet`, the shape Liberty's three `poi_r*` layers use). The style
/// spec's own `["image", name, options?]` expression and a `["let", ...]`
/// wrapper around any of the above are accepted too, but leave no trace in
/// this type: the image's NAME is what compiles, its runtime-recolour
/// `params` are dropped (see `note_dropped_image_options`), and the `let`
/// is inlined at its `var` use sites before anything here is built.
/// Anything else is out of scope for `compile_icon_image`.
#[derive(Debug, Clone)]
pub enum IconImage {
    Literal(String),
    Get(String),
    CoalesceImage(Vec<String>),
    ZoomStep(ZoomValue<String>),
    /// `["step", ["zoom"], <shape>, t1, <shape>, ...]` -- a zoom step whose
    /// per-zoom value is itself data-driven. Sorted ascending by zoom, with
    /// the base entry at zoom 0, exactly like `LayoutValue::ZoomOfDataStep`.
    ZoomOfDataIcon(Vec<(u8, DataIconShape)>),
    /// `["concat", <part>, ...]` -- a sprite name assembled from feature
    /// properties. Both Mapbox Streets shield layers use this to pick one of
    /// the atlas's 97 shield families by ref length: `["concat", ["get",
    /// "shield"], "-", ["to-string", ["get", "reflen"]]]` yields
    /// `rectangle-blue-3` for Singapore's expressways.
    Concat(Vec<ConcatPart>),
    /// `["match", ["get", k], <labels>, <output>, ..., <fallback>]` -- the
    /// shape all four of Liberty's POI layers use to name their sprite:
    /// `["match", ["get", "subclass"], ["florist", "furniture"], ["get",
    /// "subclass"], ["get", "class"]]`, i.e. "for those two subclasses the
    /// sprite is named after the subclass, otherwise after the class".
    ///
    /// Arms are held in JSON order and the first whose label list contains
    /// the input's value wins, matching MapLibre's `match`: labels are
    /// compared for EQUALITY against the input (a label written as a single
    /// value in the JSON becomes a one-element list here, since the spec
    /// allows either form), and the final argument is the required fallback,
    /// taken when no arm matches -- including when the input tag is absent
    /// entirely, which equals no label.
    ///
    /// Each output is a nested `IconImage`, so an arm can name its sprite
    /// with a literal or a `get` (the only two forms the real styles use)
    /// without a second output type. An output `get` whose tag is missing
    /// resolves to `None` -- no icon -- and not to an empty sprite name.
    MatchGet {
        key: String,
        arms: Vec<(Vec<String>, IconImage)>,
        fallback: Box<IconImage>,
    },
    /// `["to-string", ["get", k]]` as the WHOLE sprite name -- Liberty's
    /// `poi_transit` (the bus-stop layer) writes exactly that. Reads the tag
    /// through the same rule as `ConcatPart::ToStringGet` inside a `concat`
    /// (see `concat_part_string`): a string passes through, an integral
    /// number renders as `3` and never `3.0`, and anything else -- a
    /// fractional number, a missing tag -- is `None`, since it could only
    /// ever name a sprite that does not exist.
    ///
    /// Distinct from `IconImage::Get`, which stays string-only: that is a
    /// BARE `get` with no `to-string` around it.
    ToStringGet(String),
    /// `["case", ["has", k], <then>, <else>]` -- Mapbox ships two
    /// generations of shield data and prefers the newer `*_beta` properties
    /// when a feature carries them. The style's real condition also tests
    /// whether the beta SPRITE exists (`["to-boolean", ["coalesce",
    /// ["image", ...], ""]]`); `resolve` sees tags and zoom but not the
    /// atlas, so that half is dropped at compile time with a notice. See the
    /// route-shields design doc.
    ///
    /// The real `then`-branch is itself a `coalesce` of two `concat`
    /// expressions (beta shield fields, then legacy ones), and
    /// `compile_icon_image` keeps only its FIRST entry when building this
    /// variant (see that function's doc comment) -- a compile-time
    /// truncation, permanent once compiled. That is a DIFFERENT drop from
    /// `IconImage::CoalesceImage`'s own `coalesce`, under the same style-spec
    /// keyword: `CoalesceImage::resolve` tries every one of its entries in
    /// order at runtime and falls through to the next when one's key is
    /// absent from the feature's tags, exactly per the style spec. Nothing
    /// here reproduces that fall-through -- the second coalesce entry is
    /// gone before `resolve` ever runs.
    CaseHas { key: String, then: Box<IconImage>, otherwise: Box<IconImage> },
}

impl IconImage {
    /// Resolves against a feature's tags and the current zoom. Same lookup
    /// semantics as `TextField::resolve` for the first three variants;
    /// `ZoomStep` never fails to resolve (`ZoomValue::value_at` always
    /// returns a value). `ZoomOfDataIcon` CAN resolve to `None` even though
    /// its zoom step always picks a bracket, because a bracket's name may be
    /// the empty string -- see `DataIconShape::resolve`. `Concat` resolves to
    /// `None` when any part is a `Get`/`ToStringGet` whose key is missing
    /// from the feature's tags, or present with a `TagValue` variant that
    /// cannot name a sprite (a non-integral number, or anything but a string
    /// or number) -- one un-renderable part fails the whole sprite name,
    /// since a shield with a name like `rectangle-blue-` would look up a
    /// sprite that doesn't exist anyway. Both `Get` and `ToStringGet` read
    /// the tag identically here; see `concat_part_string`, which is also the
    /// whole of `ToStringGet`. `MatchGet` resolves to whatever its picked arm
    /// -- or its fallback, taken whenever no label equals the input, a
    /// missing input tag included -- resolves to, so a `get` output over an
    /// absent tag is `None`. `CaseHas` never
    /// fails on its own account; it delegates entirely to whichever branch
    /// its `key` check picks, so it resolves to `None` exactly when that
    /// branch does.
    pub fn resolve(&self, tags: &dyn TagLookup, zoom: u8) -> Option<String> {
        fn get_string(tags: &dyn TagLookup, key: &str) -> Option<String> {
            match tags.get(key) {
                Some(TagValue::String(s)) => Some(s.clone()),
                _ => None,
            }
        }
        match self {
            IconImage::Literal(s) => Some(s.clone()),
            IconImage::Get(key) => get_string(tags, key),
            IconImage::CoalesceImage(keys) => keys.iter().find_map(|k| get_string(tags, k)),
            IconImage::ZoomStep(zv) => Some(zv.value_at(zoom)),
            IconImage::ZoomOfDataIcon(zoom_stops) => zoom_stops
                .iter()
                .rev()
                .find(|(stop_zoom, _)| zoom >= *stop_zoom)
                .map(|(_, shape)| shape.resolve(tags))
                .expect("ZoomOfDataIcon must have a zoom-0 base entry"),
            IconImage::ToStringGet(key) => concat_part_string(tags, key),
            // MapLibre compares each label to the input for equality, so an
            // input the tags don't carry (`get_string` -> `None`) equals no
            // label and lands on the fallback, exactly as a non-matching
            // value does.
            IconImage::MatchGet { key, arms, fallback } => get_string(tags, key)
                .and_then(|value| {
                    arms.iter()
                        .find(|(labels, _)| labels.contains(&value))
                        .map(|(_, output)| output)
                })
                .unwrap_or(fallback)
                .resolve(tags, zoom),
            IconImage::Concat(parts) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        ConcatPart::Literal(s) => out.push_str(s),
                        // Both `get` shapes read the tag the SAME way: inside
                        // a `concat` the style spec stringifies every
                        // argument, so an explicit `to-string` wrapper is
                        // redundant rather than meaningful. See
                        // `concat_part_string`.
                        ConcatPart::Get(key) | ConcatPart::ToStringGet(key) => {
                            out.push_str(&concat_part_string(tags, key)?)
                        }
                    }
                }
                Some(out)
            }
            IconImage::CaseHas { key, then, otherwise } => {
                if tags.contains_key(key) { then.resolve(tags, zoom) } else { otherwise.resolve(tags, zoom) }
            }
        }
    }
}

// `concat_part_string` used to be nested here; it moved to module
// scope so `DataIconShape::Concat` -- the per-zoom-bracket twin of
// `IconImage::Concat` -- reads a `concat` part by exactly the same
// rule instead of a second copy of it.
/// One `["get", k]` part of a sprite-name `concat`, with or without
/// an explicit `["to-string", ...]` around it -- inside a `concat`
/// the two are the same operation. Also the whole of
/// `IconImage::ToStringGet`, where the `to-string` IS the expression
/// rather than one argument of one: same primitive, so the same rule
/// rather than a second copy of it.
///
/// The style spec defines `concat` as varargs of ValueType,
/// stringifying each argument (`compound_expression.ts`: `args.map(
/// arg => valueToString(arg.evaluate(ctx))).join('')`), so a bare
/// `get` on a numeric property is legal and renders as a string.
/// That matters for real data: OpenMapTiles types
/// `transportation_name.ref_length` as a Number, and Liberty's three
/// road-shield layers concat it WITHOUT a `to-string`.
///
/// A number renders integrally -- 3 -> "3", never "3.0" -- and a
/// fractional one yields `None`, because it could only ever name a
/// sprite that does not exist; that degrades to no name at all,
/// exactly like a missing tag, and sprite lookup then fails
/// naturally. An ABSENT tag is still `None` and not an empty string:
/// half a sprite name (`road_`) names no sprite either.
///
/// Deliberately NOT `get_string`, which stays string-only for
/// `IconImage::Get` and `CoalesceImage` -- those are bare `get`s
/// outside any `concat`, where no stringification is in play.
fn concat_part_string(tags: &dyn TagLookup, key: &str) -> Option<String> {
    match tags.get(key) {
        Some(TagValue::String(s)) => Some(s.clone()),
        Some(TagValue::Number(n)) if n.fract() == 0.0 => Some(format!("{}", *n as i64)),
        _ => None,
    }
}

/// One zoom bracket's worth of a data-driven icon NAME, nested inside
/// `IconImage::ZoomOfDataIcon`.
///
/// Deliberately NOT `DataStepShape` (the numeric analogue this module already
/// has, used by `LayoutValue::DataStep`/`ZoomOfDataStep`): that type's `base`
/// and stop outputs are `f32`, and an icon name is a `String`. The two shapes
/// are structurally identical and semantically the same `step`, but they
/// carry different output types, so sharing one type would mean making
/// `DataStepShape` generic over its output for the sake of exactly two
/// users -- more machinery than either shape is worth. The parsers stay
/// side by side instead (`parse_data_step` and `parse_data_icon_step`), and
/// this comment is the link between them.
#[derive(Debug, Clone, PartialEq)]
pub enum DataIconShape {
    /// A constant name for this whole zoom bracket.
    Name(String),
    /// `["step", ["get", key], base, t1, v1, ...]` over string outputs.
    Step(DataIconStep),
    /// `["case", ["==", ["get", key], n], then_name, otherwise]` -- the real
    /// city-label capital test. Only numeric equality is in scope; see
    /// `compile_icon_image`.
    CaseNumberEq {
        key: String,
        value: f64,
        then_name: String,
        otherwise: Box<DataIconShape>,
    },
    /// `["match", ["get", key], <labels>, <shape>, ..., <fallback>]` inside
    /// ONE zoom bracket -- Mapbox Standard's `tunnel-oneway-arrow-blue`,
    /// whose z17 bracket picks the large arrow for the five wider road
    /// classes and the small one for everything else.
    ///
    /// The per-zoom twin of `IconImage::MatchGet` (same semantics, same
    /// label rules: a label written as a single value in the JSON becomes a
    /// one-element list here, arms are tried in JSON order, and the
    /// fallback is taken whenever no label EQUALS the input -- an absent
    /// input tag included). It is a separate type for the same reason
    /// `DataIconShape` is separate from `IconImage` at all: this side of the
    /// pair carries no zoom, because the zoom bracket has already been
    /// chosen by the time it resolves.
    MatchGet {
        key: String,
        arms: Vec<(Vec<String>, DataIconShape)>,
        fallback: Box<DataIconShape>,
    },
    /// `["concat", <part>, ...]` inside ONE zoom bracket -- `poi-label`'s
    /// real `icon-image`, which asks for `["concat", ["get", "maki"],
    /// "-11"]` below z17 and the `-15` sheet above it. Same parts and the
    /// same reading rule as `IconImage::Concat` (`concat_part_string`), so
    /// a part whose tag is missing -- or is a fractional number -- makes the
    /// WHOLE name `None` rather than a half-name like `-11` that could only
    /// miss in the atlas.
    Concat(Vec<ConcatPart>),
}

/// The string-valued sibling of `DataStepShape`. See `DataIconShape`'s doc
/// comment for why the two aren't one type.
#[derive(Debug, Clone, PartialEq)]
pub struct DataIconStep {
    pub key: String,
    pub base: String,
    /// Sorted ascending by threshold.
    pub stops: Vec<(f64, String)>,
}

impl DataIconShape {
    /// Resolves to the icon name for a feature, or `None` for "no icon".
    ///
    /// The empty string is how the Mapbox style spec spells "draw no icon
    /// here" (the city labels' `icon-image` steps to `""` from z8 up, where
    /// the text stands on its own). It must come back as `None`, never
    /// `Some("")`: an empty name would be looked up in the sprite atlas and
    /// miss on every feature, every frame.
    ///
    /// A missing or non-numeric tag falls back the same way real Mapbox
    /// `step`/`==` semantics do -- to the step's `base`, and to the `case`'s
    /// else branch respectively (Mapbox's `==` is type-strict, so a string
    /// `"2"` is genuinely not equal to the number `2` there either).
    fn resolve(&self, tags: &dyn TagLookup) -> Option<String> {
        let name = match self {
            DataIconShape::Name(name) => name.clone(),
            DataIconShape::Step(step) => {
                let Some(TagValue::Number(n)) = tags.get(&step.key) else {
                    return non_empty(step.base.clone());
                };
                step.stops
                    .iter()
                    .rev()
                    .find(|(threshold, _)| *n >= *threshold)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| step.base.clone())
            }
            DataIconShape::CaseNumberEq { key, value, then_name, otherwise } => {
                match tags.get(key) {
                    Some(TagValue::Number(n)) if n == value => then_name.clone(),
                    _ => return otherwise.resolve(tags),
                }
            }
            // Same rule as `IconImage::MatchGet`: the input is compared to
            // each label for EQUALITY, so a feature whose tag is missing --
            // or is not a string at all -- equals no label and takes the
            // fallback, exactly as a non-matching value does.
            DataIconShape::Concat(parts) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        ConcatPart::Literal(s) => out.push_str(s),
                        ConcatPart::Get(key) | ConcatPart::ToStringGet(key) => {
                            out.push_str(&concat_part_string(tags, key)?)
                        }
                    }
                }
                out
            }
            DataIconShape::MatchGet { key, arms, fallback } => {
                let value = match tags.get(key) {
                    Some(TagValue::String(s)) => Some(s.as_str()),
                    _ => None,
                };
                let picked = value
                    .and_then(|value| arms.iter().find(|(labels, _)| labels.iter().any(|label| label == value)))
                    .map_or(&**fallback, |(_, shape)| shape);
                return picked.resolve(tags);
            }
        };
        non_empty(name)
    }
}

/// `""` means "no icon", never an icon literally named "".
fn non_empty(name: String) -> Option<String> {
    (!name.is_empty()).then_some(name)
}

/// The `step(get(key), base, t1, v1, ...)` shape: a data-driven numeric step
/// expression, reused standalone by `LayoutValue::DataStep` and nested (once
/// per zoom bracket) by `LayoutValue::ZoomOfDataStep`.
#[derive(Debug, Clone, PartialEq)]
pub struct DataStepShape {
    pub key: String,
    pub base: f32,
    /// Sorted ascending by threshold.
    pub stops: Vec<(f64, f32)>,
}

impl DataStepShape {
    /// A missing/non-numeric tag falls back to `base` -- the same behavior
    /// as a tag value below every threshold, matching real Mapbox `step`
    /// semantics (see `LayoutValue::resolve`'s doc comment).
    pub(crate) fn resolve(&self, tags: &dyn TagLookup) -> f32 {
        let Some(TagValue::Number(n)) = tags.get(&self.key) else {
            return self.base;
        };
        self.stops
            .iter()
            .rev()
            .find(|(threshold, _)| *n >= *threshold)
            .map(|(_, v)| *v)
            .unwrap_or(self.base)
    }
}

/// The `match(get(key), labels1, out1, ..., default)` shape: a data-driven
/// numeric bucket chosen by STRING EQUALITY against label lists, where
/// `DataStepShape` chooses by comparing a NUMERIC tag with `>=` thresholds.
/// Nested once per zoom bracket by `LayoutValue::ZoomOfMatchStep`, exactly as
/// `DataStepShape` is nested by `ZoomOfDataStep` -- see
/// `style_spec::compile_interpolated_data_step_layout_value`'s doc comment
/// for why an `interpolate` whose stops are `match`es needed this shape at
/// all (real inputs: `road-label`'s, `path-pedestrian-label`'s and
/// `settlement-subdivision-label`'s `text-size`).
///
/// Deliberately NOT `DataStepShape` made generic over its bucket-selection
/// rule: a `>=` threshold and a label-equality set are different enough
/// comparisons (one needs an ordering, the other doesn't) that sharing one
/// type would blur which rule a given `LayoutValue` variant actually applies
/// -- the same reasoning `DataIconShape`'s doc comment gives for not sharing
/// with `DataStepShape` over the OUTPUT type instead.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchStepShape {
    pub key: String,
    /// Tried in JSON order; each arm's label list is matched by equality
    /// (`labels.iter().any(|l| l == tag)`), exactly as
    /// `DataIconShape::MatchGet` and real MapLibre `Match::evaluate` do. Real
    /// styles never repeat a label across arms (MapLibre's own parser
    /// rejects that statically), so first-match-wins order never actually
    /// matters here -- it is just the natural way to search the list.
    pub arms: Vec<(Vec<String>, f32)>,
    /// The trailing bare value: the output for a feature whose tag matches
    /// no arm's labels, INCLUDING a missing or non-string tag. Never the
    /// first arm's value -- confusing the two is exactly the silent-wrong-
    /// bucket failure this shape exists to avoid (see `resolve`).
    pub default: f32,
}

impl MatchStepShape {
    /// A missing or non-string tag matches no arm's labels (a real MapLibre
    /// `Match::evaluate` looks its `input` up in a `cases` map keyed by
    /// stringified label, and a value whose runtime TYPE doesn't match the
    /// declared `inputType` never appears there at all), so both fall
    /// through to `default` -- never to `arms[0]`'s value, which would be
    /// silently wrong for every feature that doesn't happen to match the
    /// first arm.
    pub(crate) fn resolve(&self, tags: &dyn TagLookup) -> f32 {
        let value = match tags.get(&self.key) {
            Some(TagValue::String(s)) => Some(s.as_str()),
            _ => None,
        };
        value
            .and_then(|v| self.arms.iter().find(|(labels, _)| labels.iter().any(|label| label == v)))
            .map(|(_, output)| *output)
            .unwrap_or(self.default)
    }
}

/// The `interpolate(linear, get(key), v0, o0, v1, o1, ...)` shape: a value
/// that ramps CONTINUOUSLY with a numeric feature property, where
/// `DataStepShape` jumps between `>=` buckets. Nested once per zoom bracket
/// by `LayoutValue::ZoomOfDataRamp`.
///
/// The real input is `poi-label`'s `text-offset`, which is what pushes a POI
/// name clear of its own icon:
/// `["interpolate", ["linear"], ["get", "sizerank"], 0, ["literal", [0, 0]],
///   13, ["literal", [0, 0.75]]]`.
/// Dropping it -- which is what this crate did before this type existed --
/// left every POI label sitting ON its icon.
///
/// # LINEAR only, deliberately
///
/// There is no interpolation-curve field. A property-driven `interpolate`
/// with an `exponential` or `cubic-bezier` curve is refused at compile time
/// rather than approximated, because the honest alternative --
/// `expression::Interpolation`, which every zoom curve in this crate eases
/// along -- is `pub(crate)` and this type is public API, and the dishonest
/// one (resampling the curve at arbitrary property values) has no natural
/// sample points: a zoom axis has integers, a `sizerank` axis has nothing.
/// Every property-driven `interpolate` in either vendored style is
/// `["linear"]`, so nothing real is lost, and a style that brings a curve
/// gets a loud degradation rather than a quietly wrong offset.
#[derive(Debug, Clone, PartialEq)]
pub struct DataRampShape {
    pub key: String,
    /// Sorted ascending by the PROPERTY's value (not by zoom). Never empty.
    pub stops: Vec<(f64, f32)>,
}

impl DataRampShape {
    /// Clamps outside the declared range, and falls back to the FIRST stop's
    /// value for a feature whose tag is missing or non-numeric.
    ///
    /// That fallback is the style-spec's own answer in the case that
    /// matters: real Mapbox evaluates `["get", k]` to null, the surrounding
    /// `interpolate` fails its type check, and GL JS substitutes the
    /// property's default -- `[0, 0]` for `text-offset`, which is exactly
    /// the value at the lowest stop of every ramp in these styles. No real
    /// `poi_label` feature reaches it: the source layer always carries a
    /// `sizerank`.
    pub(crate) fn resolve(&self, tags: &dyn TagLookup) -> f32 {
        let first = self.stops.first().expect("a DataRampShape always has at least one stop");
        let Some(TagValue::Number(n)) = tags.get(&self.key) else {
            return first.1;
        };
        let n = *n;
        if n <= first.0 {
            return first.1;
        }
        let last = self.stops.last().expect("a DataRampShape always has at least one stop");
        if n >= last.0 {
            return last.1;
        }
        let upper = self
            .stops
            .iter()
            .position(|(input, _)| *input > n)
            .expect("n is below the last stop, so a stop above it exists");
        let (x0, y0) = self.stops[upper - 1];
        let (x1, y1) = self.stops[upper];
        let t = ((n - x0) / (x1 - x0)) as f32;
        y0 + (y1 - y0) * t
    }
}

/// A compiled zoom-and/or-data-dependent numeric layout property --
/// `text-size`, `icon-size`, and (per-axis, see `compile_layout_value`'s doc
/// comment) `text-offset`. Scoped to the exact shapes seen in the real
/// vendored styles: a bare constant, a zoom-only `["step", ["zoom"], ...]`,
/// a data-only `["step", ["get", key], ...]`, and the composite
/// `["step", ["zoom"], ["step", ["get", key], ...], t1, ["step", ["get",
/// key], ...], ...]` shape confirmed as poi-label's real `text-size`
/// expression.
#[derive(Debug, Clone)]
pub enum LayoutValue {
    Constant(f32),
    ZoomStep(ZoomValue<f32>),
    DataStep(DataStepShape),
    /// Sorted ascending by zoom. Also the target for an `interpolate` whose
    /// stop outputs are per-feature `step` expressions (`state-label`'s,
    /// `country-label`'s and both `settlement-*-label`s' real `text-size`):
    /// the curve is sampled at every integer zoom, and each sample is the
    /// bracketing stops' shapes mixed bucket by bucket -- after every stop
    /// has been put on one common threshold list, which the city labels
    /// need because their stops declare different bucket sets. See
    /// `style_spec::compile_interpolated_data_step_layout_value`.
    ZoomOfDataStep(Vec<(u8, DataStepShape)>),
    /// The string-keyed sibling of `ZoomOfDataStep`: sorted ascending by
    /// zoom, each bracket a `MatchStepShape` rather than a `DataStepShape`.
    /// Real inputs (verbatim, from `examples/mapbox-streets-style.json`):
    /// `road-label`'s, `path-pedestrian-label`'s and
    /// `settlement-subdivision-label`'s `text-size`, each an `interpolate`
    /// over `["zoom"]` whose stop outputs are `["match", ["get", key], ...]`
    /// on a STRING property (`class`/`type`). See
    /// `style_spec::compile_interpolated_data_step_layout_value`'s doc
    /// comment for the compile-time precondition this variant relies on
    /// (every stop's `match` must declare the same key and the same label
    /// GROUPS, so "the corresponding arm in the other stop" is defined) and
    /// `resolve`'s doc comment for why the bracket is picked rather than
    /// blended, exactly like `ZoomOfDataStep`.
    ZoomOfMatchStep(Vec<(u8, MatchStepShape)>),
    /// Sorted ascending by zoom, each bracket a `DataRampShape` -- the
    /// continuous sibling of `ZoomOfDataStep`, and `poi-label`'s real
    /// `text-offset`:
    /// `["step", ["zoom"],
    ///     ["interpolate", ["linear"], ["get", "sizerank"], 0, [0,0], 5, [0,0.75]],
    ///     17,
    ///     ["interpolate", ["linear"], ["get", "sizerank"], 0, [0,0], 13, [0,0.75]]]`.
    ///
    /// A ramp with no zoom wrapper is represented here too, as a single
    /// bracket at zoom 0 -- one variant rather than two for a shape that
    /// only ever appears wrapped in the real styles.
    ZoomOfDataRamp(Vec<(u8, DataRampShape)>),
    /// A zoom `interpolate` whose stop outputs are ARITHMETIC over feature
    /// properties -- `water-point-label`'s and `water-line-label`'s real
    /// `text-size`, `["interpolate", ["linear"], ["zoom"], 0, ["*", ["-", 16,
    /// ["sqrt", ["get", "sizerank"]]], 1], 22, ...]`. No finite bucket list
    /// can hold a square root, so this keeps the expression and evaluates it
    /// per feature at bake time through the style compiler's own evaluator
    /// (`style_spec::expression::eval` with a feature-carrying scope) --
    /// every stop evaluated for the feature, then interpolated by zoom,
    /// which is MapLibre's `Interpolate.evaluate` order.
    ///
    /// `fallback` is what a feature the expression errors on gets (MapLibre:
    /// a runtime error falls back to the property's default), and `blend`
    /// says whether the top level is an `interpolate` (so `resolve_pair`
    /// may hand back two different zooms' values) or not.
    FeatureExpression { expr: std::sync::Arc<serde_json::Value>, fallback: f32, blend: bool },
}

impl LayoutValue {
    /// Sets the value a `FeatureExpression` resolves to for a feature it
    /// errors on -- the property's style-spec default, which only the
    /// caller knows. Every other variant is returned unchanged.
    #[must_use]
    pub fn with_fallback(self, default: f32) -> Self {
        match self {
            LayoutValue::FeatureExpression { expr, blend, .. } => {
                LayoutValue::FeatureExpression { expr, fallback: default, blend }
            }
            other => other,
        }
    }

    /// Never returns `Option`: an absent/wrong-typed tag on a
    /// `DataStep`/`ZoomOfDataStep` falls back to that shape's own `base`
    /// value (matching real Mapbox `step` semantics -- the base is exactly
    /// the "below the first stop" value, and a missing/non-numeric key
    /// behaves the same as "the value doesn't clear any stop threshold").
    pub fn resolve(&self, tags: &dyn TagLookup, zoom: u8) -> f32 {
        match self {
            LayoutValue::Constant(v) => *v,
            LayoutValue::ZoomStep(zv) => zv.value_at(zoom),
            LayoutValue::DataStep(shape) => shape.resolve(tags),
            LayoutValue::ZoomOfDataStep(zoom_stops) => zoom_stops
                .iter()
                .rev()
                .find(|(z, _)| *z <= zoom)
                .or_else(|| zoom_stops.first())
                .map(|(_, shape)| shape.resolve(tags))
                .expect("ZoomOfDataStep must have at least one zoom stop"),
            LayoutValue::ZoomOfMatchStep(zoom_stops) => zoom_stops
                .iter()
                .rev()
                .find(|(z, _)| *z <= zoom)
                .or_else(|| zoom_stops.first())
                .map(|(_, shape)| shape.resolve(tags))
                .expect("ZoomOfMatchStep must have at least one zoom stop"),
            LayoutValue::ZoomOfDataRamp(zoom_stops) => zoom_stops
                .iter()
                .rev()
                .find(|(z, _)| *z <= zoom)
                .or_else(|| zoom_stops.first())
                .map(|(_, shape)| shape.resolve(tags))
                .expect("ZoomOfDataRamp must have at least one zoom stop"),
            LayoutValue::FeatureExpression { expr, fallback, .. } => {
                crate::style_spec::expression::eval_number_for_feature(expr, tags, zoom)
                    .map_or(*fallback, |n| n as f32)
            }
        }
    }

    /// The bracketing pair (the value at `zoom`, and the value at `zoom +
    /// 1`) a caller
    /// blends with the camera's fractional zoom rather than snapping to
    /// whichever integer bucket the camera truncates to -- the layout-side
    /// twin of `ZoomValue::value_at_pair`, which the paint side has used for
    /// its `_from`/`_to` GPU blend all along.
    ///
    /// This exists because real Mapbox evaluates `icon-size`/`text-size` at
    /// the map's CONTINUOUS zoom: `symbol_size.ts`'s `evaluateSizeForZoom`
    /// interpolates between the stops covering `[tileZoom, tileZoom + 1]`
    /// and hands the result to the shader every frame from
    /// `draw_symbol.ts`, so a symbol grows smoothly as the camera moves
    /// through a zoom level. Resolving at the integer bucket alone freezes
    /// the size for a whole level and then jumps it -- and, because these
    /// curves overwhelmingly GROW with zoom, leaves the symbol undersized
    /// everywhere but the bucket's floor.
    ///
    /// A `step` must NOT be blended: its hard cutoff at a stop is the whole
    /// point of the expression, and real Mapbox never interpolates one. The
    /// step-shaped variants therefore return the same value twice, exactly
    /// as `ZoomValue::value_at_pair` does for a `stepped` curve, so the
    /// caller's blend is a no-op on them and needs no special case.
    pub fn resolve_pair(&self, tags: &dyn TagLookup, zoom: u8) -> (f32, f32) {
        match self {
            LayoutValue::Constant(v) => (*v, *v),
            LayoutValue::ZoomStep(zv) => zv.value_at_pair(zoom),
            LayoutValue::DataStep(shape) => {
                let v = shape.resolve(tags);
                (v, v)
            }
            // The one variant that CONFLATES the two cases:
            // `style_spec::compile_layout_value` lands a `step`-of-`step`
            // here (must not blend -- poi-label's real `text-size`) and
            // `compile_interpolated_data_step_layout_value` lands an
            // `interpolate`-of-`step` here (should blend -- the city
            // labels'), and the variant carries no flag saying which.
            // Freezing the pair is the SAFE half of that ambiguity: it is
            // exactly the behaviour every caller had before this method
            // existed, so it can be wrong only in the direction it was
            // already wrong. Nothing drives `icon-size` -- this method's
            // only caller today -- through this shape in either vendored
            // style. Splitting the variant belongs with the `text-size`
            // work, where the interpolate-of-step case actually bites.
            LayoutValue::ZoomOfDataStep(_) => {
                let v = self.resolve(tags, zoom);
                (v, v)
            }
            // Same freeze, same reason: `ZoomOfMatchStep` is the string-keyed
            // twin of `ZoomOfDataStep` immediately above and nothing drives
            // it through this method today either.
            LayoutValue::ZoomOfMatchStep(_) => {
                let v = self.resolve(tags, zoom);
                (v, v)
            }
            // Frozen for a reason that is NOT the ambiguity above: the outer
            // expression here is always a zoom `step`, whose hard cutoff must
            // never be blended. Only the inner axis is continuous, and that
            // axis is the feature's own property, which does not move with
            // the camera.
            LayoutValue::ZoomOfDataRamp(_) => {
                let v = self.resolve(tags, zoom);
                (v, v)
            }
            // A top-level zoom `interpolate` is continuous in zoom, so the
            // pair is two real evaluations; anything else (a `step`, a
            // `case`) keeps its hard cutoffs, as the step variants above do.
            LayoutValue::FeatureExpression { blend, .. } => {
                let v = self.resolve(tags, zoom);
                if *blend { (v, self.resolve(tags, zoom.saturating_add(1))) } else { (v, v) }
            }
        }
    }
}

/// A compiled `paint.icon-opacity`/`paint.text-opacity`: the paint side's
/// own `Vec<(Filter, ZoomValue<f32>)>` -- exactly what `compile_zoom_value`
/// hands back for `fill-opacity` and `line-opacity` -- resolved per FEATURE
/// instead of expanded into one `StyleLayer` per case.
///
/// # Why this is not a cross-product like `fill-opacity`'s
///
/// `compile_layer`'s `"fill"`/`"line"` arms take the same list and multiply
/// the layer by it, ANDing each case's filter into the layer's own. That is
/// right for a fill: a `Fill` layer carries one colour and one opacity, and
/// duplicating it is the only way to say "two opacities here".
///
/// A symbol layer already resolves half a dozen properties per feature
/// (`text_size`, `icon_size`, `text_halo_width`, `spacing_px`,
/// `symbol_sort_key`), so it has somewhere to put a per-feature value and a
/// fill has not. Resolving here rather than expanding keeps the compiled
/// layer COUNT unchanged, which matters: `poi-label`'s icon opacity buckets
/// on `sizerank`, `road number shield`'s on `class` and
/// `transit-stop-label`'s on `mode`, and multiplying those layers out would
/// have re-ordered and duplicated the compiled style for a property that
/// changes no colour and no geometry.
///
/// # First match wins, and why that is exact
///
/// `compile_zoom_value` emits cases that PARTITION the features: a `match`
/// becomes one `In` per label plus `Not(In(all labels))`, a `case` becomes
/// each arm ANDed with the negation of every earlier condition plus a
/// fallback, and a property-bucketing `step` becomes one half-open numeric
/// range per bucket. Exactly one case can match any given feature, so
/// scanning in order and taking the first is the same answer the
/// cross-product would have reached -- not an approximation of it.
#[derive(Debug, Clone)]
pub struct SymbolOpacity {
    /// Every `(Filter, ZoomValue<f32>)` case, in compile order.
    pub cases: Vec<(Filter, ZoomValue<f32>)>,
}

impl SymbolOpacity {
    /// The style-spec default: fully opaque, unconditionally. What a layer
    /// that declares no opacity compiles to, and what one whose declaration
    /// this crate cannot read degrades to -- i.e. exactly the behaviour
    /// every symbol layer had before the property was compiled at all.
    pub fn opaque() -> Self {
        Self { cases: vec![(Filter::Any, ZoomValue::constant(1.0))] }
    }

    /// A feature matching NO case cannot happen for anything
    /// `compile_zoom_value` produced (see the type's doc comment), so the
    /// `1.0` fallback is unreachable rather than a policy. It is the
    /// style-spec default all the same, which makes the unreachable case
    /// the harmless one.
    pub fn resolve(&self, tags: &dyn TagLookup, zoom: u8) -> f32 {
        self.cases
            .iter()
            .find(|(filter, _)| filter.matches(tags, f64::from(zoom)))
            .map_or(1.0, |(_, value)| value.value_at(zoom))
    }
}

/// A compiled `"symbol-sort-key"` layout property -- the within-layer
/// placement priority the collision pass sorts on (see
/// `crate::collision::PlacementCandidate`).
///
/// Deliberately its OWN small type rather than a `LayoutValue`, for two
/// reasons. `LayoutValue` has no plain `["get", key]` case (it only reads a
/// tag through a `step`), which is the one shape that actually matters here:
/// both vendored users of the property, `settlement-major-label` and
/// `settlement-minor-label`, declare exactly `["get", "symbolrank"]`. And
/// `LayoutValue::resolve` takes a zoom, which a sort key has no use for.
///
/// Scoped to a bare literal number and `["get", key]`. Anything else is
/// treated as ABSENT rather than as a compile error: an unsupported sort key
/// costs a layer nothing worse than its within-layer ordering, whereas
/// failing the layer would soft-skip it (see `compile_layer`'s `"symbol"`
/// arm) and cost it every one of its labels.
#[derive(Debug, Clone, PartialEq)]
pub enum SymbolSortKey {
    Literal(f32),
    Get(String),
}

impl SymbolSortKey {
    /// Resolves against a feature's tags, at bake time -- `SymbolFeature`
    /// carries the resulting number, not this expression, for the same
    /// bake-once reason every other symbol property is resolved there.
    ///
    /// An absent or non-numeric tag resolves to `0.0`, matching the real
    /// Mapbox style spec's own default for the property (an unset
    /// `symbol-sort-key` is zero, not infinity). In practice no real feature
    /// hits this: the only layers that set the property are the two
    /// settlement layers, whose filters already require `symbolrank`.
    pub fn resolve(&self, tags: &dyn TagLookup) -> f32 {
        match self {
            SymbolSortKey::Literal(v) => *v,
            SymbolSortKey::Get(key) => match tags.get(key) {
                Some(TagValue::Number(n)) => *n as f32,
                _ => 0.0,
            },
        }
    }
}

/// `layout.symbol-placement`'s two shapes: a constant, or the value resolved
/// for one particular zoom out of a zoom-stepped declaration (see
/// `SymbolPlacementValue`, `style_spec::compile_symbol_layer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolPlacement {
    Point,
    Line,
    /// `"line-center"`: one symbol at the middle of each line part (see the
    /// `LineCenter` handling in `match_tile_symbols`). Drawn exactly as a
    /// `Line` symbol is -- rotated with, and bent along, the line.
    LineCenter,
}

/// `layout.symbol-placement`, which Mapbox Streets' shield layer varies by
/// zoom: a shield rides a road's point feature below z11 and is placed along
/// the line above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolPlacementValue {
    Constant(SymbolPlacement),
    /// Sorted ascending by zoom, with the base entry at zoom 0.
    ZoomStep(Vec<(u8, SymbolPlacement)>),
}

impl SymbolPlacementValue {
    pub fn at(&self, zoom: u8) -> SymbolPlacement {
        match self {
            SymbolPlacementValue::Constant(p) => *p,
            SymbolPlacementValue::ZoomStep(stops) => stops
                .iter()
                .rev()
                .find(|(stop_zoom, _)| zoom >= *stop_zoom)
                .map(|(_, p)| *p)
                .expect("ZoomStep must have a zoom-0 base entry"),
        }
    }
}

/// Shared between `layout.icon-rotation-alignment` (`StyleLayer::Symbol`'s
/// `rotation_alignment`) and `layout.text-rotation-alignment`
/// (`text_rotation_alignment`) -- the two properties compile independently,
/// each into its own field, since a style can set either without the other.
/// The style spec's `auto` is resolved at compile time, identically for
/// both: `Map` under line placement (an arrow follows the road; curve-
/// following text would too, if this crate supported it), `Viewport` under
/// point placement (a POI icon, or its label, stays upright).
/// A symbol layer's `icon-text-fit` (which axes of the icon wrap the
/// label) and `icon-text-fit-padding` (`[top, right, bottom, left]`, layout
/// pixels). The default is Mapbox's: `none`, no padding.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct IconTextFit {
    pub fit: crate::overlay::paint::TextFit,
    pub padding: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationAlignment {
    Map,
    Viewport,
}

/// The style spec's default for `layout.text-max-width`, in ems.
///
/// Worth stating out loud because "the property is absent" is the common
/// case and it does NOT mean "do not wrap": a layer that says nothing about
/// `text-max-width` is asking for a label no wider than ten times its own
/// font size. Rendering such a label as one unbroken line -- which this
/// crate did until the property was compiled -- is a deviation from the
/// style, not a neutral default.
pub const DEFAULT_TEXT_MAX_WIDTH_EM: f32 = 10.0;

/// `["step", ["get", key], base, t1, v1, ...]` over ANCHOR KEYWORDS -- the
/// anchor-valued sibling of `DataStepShape` (numeric) and `DataIconStep`
/// (string). See `DataIconShape`'s doc comment for why the three are three
/// types rather than one generic one; this is the third user, and it makes
/// the same trade for the same reason.
#[derive(Debug, Clone, PartialEq)]
pub struct DataAnchorStep {
    pub key: String,
    pub base: TextAnchor,
    /// Sorted ascending by threshold.
    pub stops: Vec<(f64, TextAnchor)>,
}

impl DataAnchorStep {
    /// A missing or non-numeric tag falls back to `base`, matching real
    /// Mapbox `step` semantics for a null input -- the same rule
    /// `DataStepShape::resolve` follows.
    ///
    /// A step with a key and NO stops is `["get", key]` itself: the tag is
    /// the anchor keyword, and a missing or unknown one is `base`.
    fn resolve(&self, tags: &dyn TagLookup) -> TextAnchor {
        if self.stops.is_empty() && !self.key.is_empty() {
            return match tags.get(&self.key) {
                Some(TagValue::String(keyword)) => TextAnchor::from_keyword(keyword).unwrap_or(self.base),
                _ => self.base,
            };
        }
        let Some(TagValue::Number(n)) = tags.get(&self.key) else {
            return self.base;
        };
        self.stops
            .iter()
            .rev()
            .find(|(threshold, _)| *n >= *threshold)
            .map_or(self.base, |(_, anchor)| *anchor)
    }
}

/// A compiled `layout.text-anchor`: a keyword, or one chosen per zoom and
/// per feature.
///
/// # Why this is not a plain `TextAnchor`
///
/// It was one, and that is what put every POI name on top of its own icon.
/// Mapbox Standard's `poi-label` declares
/// `["step", ["zoom"],
///     ["step", ["get", "sizerank"], "center", 5, "top"],
///     17,
///     ["step", ["get", "sizerank"], "center", 13, "top"]]`
/// -- an ordinary POI anchors its label's TOP edge on the anchor point, so
/// the name hangs below the sprite; only a landmark important enough to
/// clear the `sizerank` threshold gets the centred anchor. Read as "a
/// keyword and nothing else", that whole expression degraded to the spec
/// default of `center`, and the label landed exactly on the icon. With
/// `text-offset` degrading at the same time (see `DataRampShape`) there was
/// nothing left to move it.
#[derive(Debug, Clone, PartialEq)]
pub enum TextAnchorValue {
    Constant(TextAnchor),
    /// Sorted ascending by zoom. A bracket that is a bare keyword becomes a
    /// `DataAnchorStep` with no stops, so mixed declarations need no second
    /// variant; a data step with no zoom wrapper is a single bracket at
    /// zoom 0, for the same reason `LayoutValue::ZoomOfDataRamp` has no
    /// unwrapped variant.
    ZoomOfDataStep(Vec<(u8, DataAnchorStep)>),
}

/// **Where a point label's anchor POINT moves, in ems**, given its resolved
/// `text-anchor`, `text-radial-offset` and `text-offset`.
///
/// A radial offset above zero REPLACES `text-offset` (the spec says not to
/// set both and MapLibre GL JS takes the radial one, `symbol_layout.ts`),
/// and becomes a vector pointing AWAY from the anchored edge:
/// `left` moves the point `r` right, `top` moves it `r` down, and a corner
/// moves it `r / sqrt 2` on each axis. Vertically MapLibre then takes back
/// `7/24` of an em (its `baselineOffset`, `variable_text_anchor.ts`),
/// because the radial offset is measured to where glyphs APPEAR to start
/// and a line box's top or bottom sits that far outside the glyphs; this
/// crate's line box has the same slack (its baseline is 0.98 em into a
/// 1.2 em box), so the same correction applies.
///
/// This is the offset that keeps Mapbox Streets' city names beside their
/// town dots rather than centred on them.
#[must_use]
pub fn point_text_offset(anchor: TextAnchor, radial_em: f32, offset_em: (f32, f32)) -> (f32, f32) {
    if radial_em.is_nan() || radial_em <= 0.0 {
        return offset_em;
    }
    const BASELINE_EM: f32 = 7.0 / 24.0;
    let diagonal = radial_em / std::f32::consts::SQRT_2;
    let x = match anchor {
        TextAnchor::Left => radial_em,
        TextAnchor::Right => -radial_em,
        TextAnchor::TopLeft | TextAnchor::BottomLeft => diagonal,
        TextAnchor::TopRight | TextAnchor::BottomRight => -diagonal,
        TextAnchor::Center | TextAnchor::Top | TextAnchor::Bottom => 0.0,
    };
    let y = match anchor {
        TextAnchor::Top => radial_em - BASELINE_EM,
        TextAnchor::Bottom => -radial_em + BASELINE_EM,
        TextAnchor::TopLeft | TextAnchor::TopRight => diagonal - BASELINE_EM,
        TextAnchor::BottomLeft | TextAnchor::BottomRight => -diagonal + BASELINE_EM,
        TextAnchor::Center | TextAnchor::Left | TextAnchor::Right => 0.0,
    };
    (x, y)
}

#[cfg(test)]
mod point_text_offset_tests {
    use super::{point_text_offset, TextAnchor};

    /// MapLibre GL JS's `fromRadialOffset`, at `ONE_EM = 24` and
    /// `baselineOffset = 7`, in ems: every anchor moves the point AWAY
    /// from its anchored edge, and no radial offset leaves `text-offset`.
    #[test]
    fn a_radial_offset_points_away_from_the_anchored_edge() {
        let r = 0.55;
        let d = r / std::f32::consts::SQRT_2;
        let b = 7.0 / 24.0;
        let cases = [
            (TextAnchor::Center, (0.0, 0.0)),
            (TextAnchor::Left, (r, 0.0)),
            (TextAnchor::Right, (-r, 0.0)),
            (TextAnchor::Top, (0.0, r - b)),
            (TextAnchor::Bottom, (0.0, -r + b)),
            (TextAnchor::TopLeft, (d, d - b)),
            (TextAnchor::TopRight, (-d, d - b)),
            (TextAnchor::BottomLeft, (d, -d + b)),
            (TextAnchor::BottomRight, (-d, -d + b)),
        ];
        for (anchor, (x, y)) in cases {
            let got = point_text_offset(anchor, r, (9.0, 9.0));
            assert!((got.0 - x).abs() < 1e-6 && (got.1 - y).abs() < 1e-6, "{anchor:?}: {got:?}");
        }
        assert_eq!(point_text_offset(TextAnchor::Left, 0.0, (0.9, 0.1)), (0.9, 0.1));
    }
}

impl Default for TextAnchorValue {
    /// The style spec's own default, and the degradation target for a shape
    /// this crate cannot read.
    fn default() -> Self {
        TextAnchorValue::Constant(TextAnchor::Center)
    }
}

impl TextAnchorValue {
    pub fn resolve(&self, tags: &dyn TagLookup, zoom: u8) -> TextAnchor {
        match self {
            TextAnchorValue::Constant(anchor) => *anchor,
            TextAnchorValue::ZoomOfDataStep(brackets) => brackets
                .iter()
                .rev()
                .find(|(z, _)| *z <= zoom)
                .or_else(|| brackets.first())
                .map(|(_, shape)| shape.resolve(tags))
                .expect("TextAnchorValue::ZoomOfDataStep must have at least one zoom bracket"),
        }
    }
}

/// `layout.text-anchor`: which part of the LABEL is placed on the label's
/// anchor point. Not "which side of the anchor the text is on" -- the two
/// read as opposites, and getting them the wrong way round puts every
/// offset label on the wrong side of its icon. `Left` means the text's LEFT
/// EDGE sits on the anchor and the run extends to the RIGHT.
///
/// This is what keeps a POI label off its own icon. Liberty's `poi_transit`
/// pairs `"text-anchor": "left"` with `"text-offset": [0.9, 0]`: the offset
/// moves the anchor point 0.9 em to the right of the icon, and the anchor
/// puts the text's left edge there, so the whole label starts clear of the
/// sprite. Centring it instead -- which is what this renderer did before
/// this enum existed -- straddles the moved point and drops the label's
/// left half back over the icon. That was visible over Orchard Road,
/// Singapore, where `Royal Thai Embassy` and `Far East Plaza` both sat on
/// top of their own transit squares.
///
/// The KEYWORD itself. What a layer carries is a `TextAnchorValue`, which
/// may vary by zoom and by feature; this is what one feature at one zoom
/// resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAnchor {
    #[default]
    Center,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl TextAnchor {
    /// The style-spec keyword, or `None` for anything else -- an unknown
    /// string, or a value that isn't a string at all. The caller decides
    /// what a `None` costs; every caller in this crate degrades to
    /// `Center` rather than dropping the layer.
    pub fn from_keyword(keyword: &str) -> Option<Self> {
        Some(match keyword {
            "center" => TextAnchor::Center,
            "left" => TextAnchor::Left,
            "right" => TextAnchor::Right,
            "top" => TextAnchor::Top,
            "bottom" => TextAnchor::Bottom,
            "top-left" => TextAnchor::TopLeft,
            "top-right" => TextAnchor::TopRight,
            "bottom-left" => TextAnchor::BottomLeft,
            "bottom-right" => TextAnchor::BottomRight,
            _ => return None,
        })
    }

    /// The fraction of the block's WIDTH that comes off the anchor's x to
    /// reach the text's left edge: 0 for a left anchor (the edge is already
    /// there), 1 for a right anchor (the whole run is to the left), 0.5 for
    /// anything horizontally centred.
    ///
    /// Expressed as a fraction rather than a signed nudge so the two axes
    /// COMPOSE: a corner value is just this plus `vertical_fraction`, each
    /// applied to its own axis, with neither able to override the other.
    pub fn horizontal_fraction(self) -> f32 {
        match self {
            TextAnchor::Left | TextAnchor::TopLeft | TextAnchor::BottomLeft => 0.0,
            TextAnchor::Right | TextAnchor::TopRight | TextAnchor::BottomRight => 1.0,
            TextAnchor::Center | TextAnchor::Top | TextAnchor::Bottom => 0.5,
        }
    }

    /// The fraction of the block's HEIGHT that comes off the anchor's y to
    /// reach the text's top edge: 0 for a top anchor (the text hangs
    /// below), 1 for a bottom anchor (the text sits above), 0.5 for
    /// anything vertically centred.
    pub fn vertical_fraction(self) -> f32 {
        match self {
            TextAnchor::Top | TextAnchor::TopLeft | TextAnchor::TopRight => 0.0,
            TextAnchor::Bottom | TextAnchor::BottomLeft | TextAnchor::BottomRight => 1.0,
            TextAnchor::Center | TextAnchor::Left | TextAnchor::Right => 0.5,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Style {
    pub layers: Vec<StyleLayer>,
    /// The style document's top-level `"sprite"` field, verbatim: the base
    /// URL (or `mapbox://sprites/...` reference) the icon sheet this style's
    /// `icon-image` names live in is fetched from. `None` for a style that
    /// declares none, and for every hand-built `Style` in a test.
    ///
    /// Carried here rather than re-read from the style JSON by each
    /// embedding example so that "which sheet do this style's icons come
    /// from" is answered once, by the style, in the same place its layers
    /// come from. `sprite::resolve_sprite_url` turns it into the base URL
    /// `sprite::fetch_sprite_atlas` wants; see `examples/basic_map.rs` for
    /// the whole path from style file to `MapView::set_sprites`.
    pub sprite: Option<String>,
    /// The style document's top-level `"sky"`, compiled -- or the style
    /// spec's defaults for every property it leaves out, including all of
    /// them when it has no `sky` at all. See [`Sky`].
    pub sky: Sky,
}

/// The style's `sky`: MapLibre style-spec v8's root `sky` object, seven
/// properties, each a constant or a zoom expression.
///
/// Ported from MapLibre GL JS (BSD-3, Copyright (c) 2023 MapLibre
/// contributors): the property set and every default below are
/// `maplibre-style-spec`'s `src/reference/v8.json` `"sky"`, and the way they
/// are drawn is `src/style/sky.ts`, `src/webgl/draw/draw_sky.ts` and
/// `src/shaders/glsl/sky.fragment.glsl` -- see `render::sky`.
///
/// **One deliberate divergence: a style with no `sky` gets the spec's
/// DEFAULTS, not MapLibre's "no sky".** `Sky.setSky(undefined)` in
/// `sky.ts` substitutes transparent colours and `atmosphere-blend: 0`, so a
/// MapLibre map whose style omits `sky` draws neither a sky nor an
/// atmosphere. Neither vendored style declares one, and a globe that fades
/// from space into flat background grey is exactly the defect this type
/// exists to remove, so an absent `sky` reads as `"sky": {}` -- with our
/// pale-blue horizon in place of the spec's white ([`HORIZON_COLOR_OURS`]).
#[derive(Debug, Clone)]
pub struct Sky {
    /// `sky-color`, default `#88C6FC`: the sky well above the horizon.
    pub sky_color: ZoomValue<Rgba>,
    /// `horizon-color`, default `#ffffff`: the sky AT the horizon.
    pub horizon_color: ZoomValue<Rgba>,
    /// `fog-color`, default `#ffffff`. The spec says "Requires 3D terrain";
    /// parsed and carried, not drawn -- see `render::sky`'s module doc.
    pub fog_color: ZoomValue<Rgba>,
    /// `fog-ground-blend`, default 0.5, `[0, 1]`. Terrain-only, as above.
    pub fog_ground_blend: ZoomValue<f32>,
    /// `horizon-fog-blend`, default 0.8, `[0, 1]`. Terrain-only, as above.
    pub horizon_fog_blend: ZoomValue<f32>,
    /// `sky-horizon-blend`, default 0.8, `[0, 1]`: how far up the sky the
    /// horizon colour reaches, as a fraction of half the viewport.
    pub sky_horizon_blend: ZoomValue<f32>,
    /// `atmosphere-blend`, default 0.8, `[0, 1]`: multiplies the globe's
    /// scattering pass, as `draw_sky.ts`'s `drawAtmosphere` multiplies it.
    pub atmosphere_blend: ZoomValue<f32>,
}

/// The MapLibre style spec's `sky-color` default, `#88C6FC`.
pub const SKY_COLOR_DEFAULT: Rgba = Rgba { r: 0x88 as f32 / 255.0, g: 0xC6 as f32 / 255.0, b: 0xFC as f32 / 255.0, a: 1.0 };
/// The spec's `horizon-color` and `fog-color` default, `#ffffff`.
pub const HORIZON_COLOR_DEFAULT: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
/// **OUR `horizon-color` for a style with no `sky`**: `#C8E0F8`, a pale
/// sky blue, where the spec says `#ffffff`.
///
/// White blended into `sky-color` by the reference's gradient reads as grey
/// haze at the limb rather than as bright sky: at z5.5, tilted, in full
/// daylight, the pixel just above the limb was `#E4E8EC` -- 7% saturation --
/// under a `#8BBEEA` sky (the user, on an iPhone: "at some point sky looks a
/// bit grey"). The gradient's shape is untouched; only the colour it ends in
/// moved, and only for styles that do not choose one. See `render::sky`'s
/// module doc and `src/render/PROVENANCE.md`.
pub const HORIZON_COLOR_OURS: Rgba = Rgba { r: 0xC8 as f32 / 255.0, g: 0xE0 as f32 / 255.0, b: 0xF8 as f32 / 255.0, a: 1.0 };

impl Default for Sky {
    /// **What a style with NO `sky` draws**: [`Sky::spec`] with our pale-blue
    /// [`HORIZON_COLOR_OURS`] horizon. A style that declares a `sky` starts
    /// from [`Sky::spec`] instead, so an omitted `horizon-color` there is
    /// the spec's white.
    fn default() -> Self {
        Self { horizon_color: ZoomValue::constant(HORIZON_COLOR_OURS), ..Self::spec() }
    }
}

impl Sky {
    /// The MapLibre style spec's defaults for all seven properties, exactly
    /// -- what `"sky": {}` means.
    #[must_use]
    pub fn spec() -> Self {
        Self {
            sky_color: ZoomValue::constant(SKY_COLOR_DEFAULT),
            horizon_color: ZoomValue::constant(HORIZON_COLOR_DEFAULT),
            fog_color: ZoomValue::constant(HORIZON_COLOR_DEFAULT),
            fog_ground_blend: ZoomValue::constant(0.5),
            horizon_fog_blend: ZoomValue::constant(0.8),
            sky_horizon_blend: ZoomValue::constant(0.8),
            atmosphere_blend: ZoomValue::constant(0.8),
        }
    }

    /// Every property evaluated at one (fractional) camera zoom, the number
    /// properties clamped to the spec's `[0, 1]`.
    ///
    /// A fractional zoom blends the two integer stops either side of it --
    /// exact for a linear `interpolate`, which is what the spec's own
    /// `atmosphere-blend` example is, and the same bracketing every other
    /// paint property in this crate gets.
    #[must_use]
    pub fn at_zoom(&self, zoom: f64) -> SkyFrame {
        let z = zoom.clamp(0.0, 254.0);
        let (floor, t) = (z.floor() as u8, z.fract() as f32);
        let colour = |v: &ZoomValue<Rgba>| {
            let (a, b) = v.value_at_pair(floor);
            a.lerp(&b, t)
        };
        let number = |v: &ZoomValue<f32>| {
            let (a, b) = v.value_at_pair(floor);
            let n = a.lerp(&b, t);
            if n.is_finite() { n.clamp(0.0, 1.0) } else { 0.0 }
        };
        SkyFrame {
            sky_color: colour(&self.sky_color),
            horizon_color: colour(&self.horizon_color),
            fog_color: colour(&self.fog_color),
            fog_ground_blend: number(&self.fog_ground_blend),
            horizon_fog_blend: number(&self.horizon_fog_blend),
            sky_horizon_blend: number(&self.sky_horizon_blend),
            atmosphere_blend: number(&self.atmosphere_blend),
        }
    }
}

/// [`Sky`] at one zoom: seven plain values, what a frame's sky is drawn
/// from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyFrame {
    pub sky_color: Rgba,
    pub horizon_color: Rgba,
    pub fog_color: Rgba,
    pub fog_ground_blend: f32,
    pub horizon_fog_blend: f32,
    pub sky_horizon_blend: f32,
    pub atmosphere_blend: f32,
}

impl Default for SkyFrame {
    fn default() -> Self {
        Sky::default().at_zoom(0.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaintOp {
    Background { color: Rgba },
    Fill { color_from: Rgba, color_to: Rgba, geometry: Arc<geo_types::Geometry<i32>> },
    Line {
        color_from: Rgba,
        color_to: Rgba,
        width_from: f32,
        width_to: f32,
        /// `line-gap-width` resolved at the plan's baked zoom, and one zoom
        /// up -- the same bracket pair `width_from`/`width_to` carry, blended
        /// per frame by `model.blend_t` in the vertex shader.
        ///
        /// The FULL declared gap, not half of it: the halving happens once,
        /// on the way into the vertex (`render::mesh::StrokeCtor`), exactly
        /// as `width_from` is halved into `half_width_px`. MapLibre halves
        /// it one step later still (`gapwidth = gapwidth / 2.0` at the top
        /// of `line.vertex.glsl`); the arithmetic is identical either way,
        /// and doing it CPU-side keeps the per-vertex value in the same
        /// "half-extent in logical pixels" unit as `half_width_px` beside it.
        gap_from: f32,
        gap_to: f32,
        /// `layout.line-cap`. Unlike every other field here this is a
        /// GEOMETRY input, not paint: `render::mesh::append_stroke` hands it
        /// to lyon's `StrokeOptions`, and changing it changes the vertices
        /// lyon emits. `map_view::ops_share_geometry_set` compares it for
        /// exactly that reason -- an op whose cap changed must be
        /// re-tessellated, never paint-rewritten.
        cap: LineCap,
        /// `layout.line-join`, carried and compared for the same reasons
        /// `cap` is.
        join: LineJoin,
        dash: DashPattern,
        /// MapLibre's `lineMetrics`: when set, `render::mesh::append_stroke`
        /// writes each vertex's `line_progress` (0 at a LineString's first
        /// point, 1 at its last, by length); when clear, every vertex gets
        /// `-1.0`, which the tile shader's trim ignores. A GEOMETRY input
        /// like `cap` and `join` -- it decides a geometry-derived vertex
        /// field -- so `map_view::ops_share_geometry_set` compares it too.
        /// `false` everywhere except the overlay path of a source declared
        /// with line metrics.
        metrics: bool,
        geometry: Arc<geo_types::Geometry<i32>>,
    },
    /// One `fill-extrusion` feature (spec §3.3): drawn by
    /// `render::extrusion`, never by the 88-byte tile mesh. `geometry` is
    /// the feature's UNCLIPPED decoded geometry -- walls come from the real
    /// rings, and the roof clips its own copy (R3). A change of `row`'s base
    /// or height re-tessellates; a colour change only rewrites the table.
    /// Opacity is a per-layer uniform (§2.1) and is not in the row.
    Extrusion { group: u16, row: ExtrusionRow, geometry: Arc<geo_types::Geometry<i32>> },
}

/// One extrusion feature's resolved paint and shape at the plan's bracket
/// zoom pair: the three texels of its extrusion-table row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionRow {
    pub color_from: Rgba,
    pub color_to: Rgba,
    pub base_from: f32,
    pub height_from: f32,
    pub base_to: f32,
    pub height_to: f32,
}

impl ExtrusionRow {
    /// Equal base and height pairs: the vertices are the same, only the table may differ.
    ///
    /// Compared bitwise, so a NaN cannot make two rows look different (or
    /// the same) by accident.
    #[must_use]
    pub fn same_shape(&self, other: &ExtrusionRow) -> bool {
        let bits = |r: &ExtrusionRow| {
            [r.base_from.to_bits(), r.height_from.to_bits(), r.base_to.to_bits(), r.height_to.to_bits()]
        };
        bits(self) == bits(other)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaintPlan {
    pub ops: Vec<PaintOp>,
}

/// What one `Arc<Geometry<i32>>` costs: the refcount header plus the
/// enum plus whatever is under it.
///
/// The header is two `usize` (strong and weak counts) ahead of the value
/// in a single `ArcInner` allocation, so an `Arc<Geometry<i32>>` is one
/// 64-byte block -- and a tile whose plan holds 49,645 of them holds 3.18
/// MB in refcount headers alone, which is not a term a footprint can round
/// away.
fn arc_geometry_bytes(geometry: &Arc<geo_types::Geometry<i32>>) -> usize {
    crate::render::heap_block_bytes(
        2 * std::mem::size_of::<usize>() + std::mem::size_of::<geo_types::Geometry<i32>>(),
    ) + crate::mvt::geometry_footprint_bytes(geometry)
}

impl PaintPlan {
    /// Every heap block this plan holds, in
    /// [`crate::render::heap_block_bytes`]' unit -- the weight
    /// `MapView`'s `cache` is bounded by.
    ///
    /// # Why it is bounded at all
    ///
    /// `cache` was bounded at 512 entries against a per-entry cost of
    /// **9.79 MB** on the real dense Tokyo z14 tile (49,645 ops) and 0.4
    /// KB on an offshore one -- a 5.0 GB latent ceiling expressed as a
    /// number with no bytes in it. Same defect as `decoded_cache`'s, same
    /// fix; see `DecodedTile::footprint_bytes`.
    ///
    /// # This DELIBERATELY double-counts geometry with `match_cache`
    ///
    /// `resolve_plan` clones the `Arc<Geometry>`s that
    /// `match_tile_static` allocated into every op of a zoom-static
    /// layer, and allocates fresh ones for a layer whose filter reads the
    /// zoom (`FillMatch::Dynamic`/`LineMatch::Dynamic`, which
    /// `resolve_plan` re-filters on every call). So some of a plan's
    /// geometry is bytes the `TileMatch` in `match_cache` is also holding,
    /// and some is the plan's own. This walk charges the plan for ALL of
    /// it, and `TileMatch::footprint_bytes` charges the match for all of
    /// its own -- so the shared shapes are counted twice across the two
    /// caches.
    ///
    /// That is a choice, and the alternative was worse. Attributing each
    /// `Arc` to exactly one cache needs `Arc::strong_count`, which is a
    /// snapshot: the moment `match_cache` evicts a tile whose plan
    /// survives in `cache`, bytes that were charged to the match become
    /// nobody's, and both caches under-account. Double-counting cannot
    /// do that. It makes each budget a CEILING on real heap rather than an
    /// equality -- `cache`'s accounted 17.19 MB against 9.79 MB held on
    /// the dense tile, a factor of 1.76 -- which errs toward evicting
    /// early, and evicting early is the direction an OS-kill fix should
    /// err in. The factor is measured rather than assumed:
    /// `memory_ceiling.plan_accounted_ratio` in `stress/baseline.json`
    /// grades it, so it cannot quietly become 17.
    ///
    /// Nothing else in a `PaintOp` reaches the heap: `Rgba`, the widths,
    /// `LineCap`, `LineJoin` and `DashPattern` (four `f32` and a `u8`,
    /// inline) are all by value, so `Background` costs nothing beyond its
    /// slot in `ops`.
    #[must_use]
    pub fn footprint_bytes(&self) -> usize {
        let mut bytes = crate::render::vec_block_bytes::<PaintOp>(self.ops.capacity());
        for op in &self.ops {
            bytes += match op {
                PaintOp::Background { .. } => 0,
                PaintOp::Fill { geometry, .. }
                | PaintOp::Line { geometry, .. }
                | PaintOp::Extrusion { geometry, .. } => arc_geometry_bytes(geometry),
            };
        }
        bytes
    }
}

impl TileMatch {
    /// Whether this match was built masked ([`match_tile_static_masked`]):
    /// its fills carry their dot-mask ranks.
    #[must_use]
    pub fn ranked(&self) -> bool {
        self.ranked
    }

    /// Every heap block this match holds, in
    /// [`crate::render::heap_block_bytes`]' unit -- the weight
    /// `MapView`'s `match_cache` is bounded by.
    ///
    /// A `TileMatch` is the ONLY owner of the clipped and
    /// boundary-converted geometry it holds: `match_tile_static` is where
    /// those `Arc`s are allocated, and every other holder (a `PaintOp` in
    /// `cache`) has a clone of the pointer. So unlike
    /// [`PaintPlan::footprint_bytes`] this figure needs no double-counting
    /// caveat -- it is what the match uniquely brought into the process.
    /// Measured at **7.76 MB** on the dense Tokyo z14 tile against a
    /// 512-entry bound, i.e. a 4.0 GB latent ceiling, and 4 KB on an
    /// offshore tile.
    ///
    /// Worth stating that these are NOT the decoded tile's coordinates
    /// seen a second time. `clip_fill_geometry_to_tile_extent` and
    /// `boundary_geometry_for_line` each build new rings, so a cached
    /// match is a second full copy of the shapes at a second full cost --
    /// which is exactly why it needs its own budget and not a share of
    /// `decoded_cache`'s.
    ///
    /// A `Dynamic` entry, and `Background`/`Symbol`/`Raster`, hold
    /// nothing: the first is the marker for a layer `resolve_plan`
    /// re-filters from scratch every call, and the other three carry no
    /// per-feature state at all (see [`LayerMatch`]).
    ///
    /// `Raster` holds nothing here even though a raster tile is by far the
    /// largest thing on screen -- a 512x512 RGBA texture is 1 MiB against
    /// a decoded vector tile's tens of KiB. That is not an oversight. The
    /// texture is not in a `TileMatch` and never passes through one: it
    /// lives on the device, and its footprint is accounted where it is
    /// held, by `render::raster_pipeline::RasterTexture::footprint_bytes`.
    /// Counting it here as well would double it in the one place a cache
    /// budget reads.
    #[must_use]
    pub fn footprint_bytes(&self) -> usize {
        // `match_tile_static` shares one `Arc` between every style layer
        // that matched the same feature, so a geometry is weighed once per
        // ALLOCATION (by address, never dereferenced), not once per layer.
        type Seen = std::collections::HashSet<*const geo_types::Geometry<i32>>;
        fn unseen<'g>(seen: &mut Seen, list: impl Iterator<Item = &'g Arc<geo_types::Geometry<i32>>>) -> usize {
            list.filter(|g| seen.insert(Arc::as_ptr(g))).map(arc_geometry_bytes).sum()
        }
        let mut seen = Seen::new();
        let geometries = |seen: &mut Seen, list: &[Arc<geo_types::Geometry<i32>>]| -> usize {
            crate::render::vec_block_bytes::<Arc<geo_types::Geometry<i32>>>(list.len()) + unseen(seen, list.iter())
        };
        let mut bytes = crate::render::vec_block_bytes::<LayerMatch>(self.entries.capacity());
        for entry in &self.entries {
            bytes += match entry {
                LayerMatch::Background | LayerMatch::Symbol | LayerMatch::Raster | LayerMatch::Circle => 0,
                LayerMatch::Fill(FillMatch::Dynamic) | LayerMatch::Line(LineMatch::Dynamic) => 0,
                LayerMatch::Fill(FillMatch::Static(list, ranks)) => {
                    geometries(&mut seen, list) + crate::render::vec_block_bytes::<crate::overlay::dotgrid::mask_class::MaskRank>(ranks.capacity())
                }
                LayerMatch::Line(LineMatch::Static(list)) => geometries(&mut seen, list),
                LayerMatch::Extrusion(ExtrusionMatch::Dynamic) => 0,
                LayerMatch::Extrusion(ExtrusionMatch::Static(list)) => {
                    crate::render::vec_block_bytes::<(u32, Arc<geo_types::Geometry<i32>>, Option<Arc<geo_types::Geometry<i32>>>)>(
                        list.capacity(),
                    ) + unseen(&mut seen, list.iter().map(|(_, g, _)| g))
                        + unseen(&mut seen, list.iter().filter_map(|(_, _, f)| f.as_ref()))
                }
            };
        }
        bytes
    }
}

/// The nominal MVT tile extent (in tile-local integer units) that
/// `render::gpu` bakes into every tile's model matrix. Vector tile
/// generators (this project's OpenFreeMap/OpenMapTiles source included)
/// commonly encode polygon geometry with extra "buffer" vertices *beyond*
/// this nominal extent (observed at exactly extent ± 64 on real OpenFreeMap
/// tiles), so a clipped ring's artificial closing edges can land either
/// exactly on this boundary or just outside it.
const TILE_EXTENT: i32 = 4096;

/// The on-screen width, in pixels, of a `fill-outline-color` hairline.
///
/// Fixed, and deliberately not a style-driven or zoom-driven quantity. The
/// style spec gives `fill-outline-color` no companion width property at
/// all, and MapLibre draws the outline with `gl.LINES` (`draw_fill.ts`'s
/// `drawFillTiles` sets `drawMode = gl.LINES` when `isOutline`), whose
/// width is one device pixel with no zoom term anywhere in the path. A
/// `PaintOp::Line`'s width is already in screen pixels, so `1.0` reproduces
/// that exactly rather than approximating it.
pub(crate) const FILL_OUTLINE_WIDTH_PX: f32 = 1.0;

/// `StyleLayer::Line` is also used to outline polygon-sourced layers (e.g.
/// `water`, `landcover`) whose features carry `Polygon`/`MultiPolygon`
/// geometry rather than `LineString`/`MultiLineString`. The stroke
/// tessellator (`render::mesh::append_stroke`) only knows how to stroke
/// line geometry, so a raw polygon passed straight into `PaintOp::Line`
/// silently produces no geometry at all. Convert a polygon's exterior (and
/// interior/hole) rings into a `MultiLineString` of its boundary so an
/// outline layer over a polygon source actually renders; line-sourced
/// geometry passes through unchanged.
///
/// Vector tile generators clip polygon geometry to each tile (plus a small
/// buffer), which closes the clipped shape with straight edges that hug the
/// tile's clip rectangle rather than following any real geographic
/// boundary. Naively stroking the *whole* ring therefore draws a visible
/// artificial line along every tile edge that a real feature (e.g. open
/// ocean, a park that spans multiple tiles) happens to be clipped against.
/// `split_ring_dropping_seam_edges` removes exactly those clip-rectangle
/// edges before the ring is handed to the stroke tessellator, keeping only
/// the arcs of the ring that trace real geometry.
fn boundary_geometry_for_line(geometry: &geo_types::Geometry<i32>) -> geo_types::Geometry<i32> {
    use geo_types::Geometry::*;
    match geometry {
        Polygon(p) => {
            let mut rings = Vec::new();
            rings.extend(split_ring_dropping_seam_edges(p.exterior(), TILE_EXTENT));
            for interior in p.interiors() {
                rings.extend(split_ring_dropping_seam_edges(interior, TILE_EXTENT));
            }
            MultiLineString(geo_types::MultiLineString::new(rings))
        }
        MultiPolygon(mp) => {
            let mut rings = Vec::new();
            for p in mp.iter() {
                rings.extend(split_ring_dropping_seam_edges(p.exterior(), TILE_EXTENT));
                for interior in p.interiors() {
                    rings.extend(split_ring_dropping_seam_edges(interior, TILE_EXTENT));
                }
            }
            MultiLineString(geo_types::MultiLineString::new(rings))
        }
        // A `Line` layer draws a source's boundary, not the source itself:
        // a point feature has no boundary, so it must contribute nothing
        // here. This is also the invariant `append_stroke` in
        // `src/render/mesh.rs` relies on when it gives `Point`/`MultiPoint`
        // geometry on a `PaintOp::Line` its own round-cap dot rendering --
        // that rendering is for the GeoJSON overlay's `circle-radius`,
        // which builds its `PaintOp::Line` directly rather than through
        // this function, so a basemap `Line` layer must never be able to
        // reach it via a point feature.
        Point(_) | MultiPoint(_) => MultiLineString(geo_types::MultiLineString::new(Vec::new())),
        other => other.clone(),
    }
}

/// An edge is a tile-clip artifact (not real geometry) when it runs exactly
/// along the tile's extent boundary on either axis: both endpoints share
/// the same `x` (a vertical clip edge) or the same `y` (a horizontal clip
/// edge), and that shared coordinate sits at or beyond the nominal extent
/// (`<= 0` or `>= extent`, which also covers clipping done against a
/// buffered extent like `-64`/`extent + 64`). A real geographic edge
/// crossing a tile boundary touches the boundary at isolated points, not
/// along a whole run of axis-aligned edges, so this heuristic only ever
/// drops the artificial "close the shape along the clip rectangle" edges
/// that a tile clipper inserts.
fn is_seam_edge(a: geo_types::Coord<i32>, b: geo_types::Coord<i32>, extent: i32) -> bool {
    (a.x == b.x && (a.x <= 0 || a.x >= extent)) || (a.y == b.y && (a.y <= 0 || a.y >= extent))
}

/// Break a (closed) polygon ring into the open line segments that remain
/// once its clip-boundary edges (see `is_seam_edge`) are removed. Runs of
/// consecutive non-seam edges are kept as separate `LineString`s; the split
/// is done circularly so a run spanning the ring's arbitrary start/end
/// point isn't incorrectly cut in two.
fn split_ring_dropping_seam_edges(
    ring: &geo_types::LineString<i32>,
    extent: i32,
) -> Vec<geo_types::LineString<i32>> {
    let coords: Vec<geo_types::Coord<i32>> = ring.coords().copied().collect();
    if coords.len() < 3 {
        return Vec::new();
    }
    // `ring` is closed (first == last); treat consecutive pairs, including
    // the closing pair, as the ring's edges.
    let edge_count = coords.len() - 1;
    let seam: Vec<bool> =
        (0..edge_count).map(|i| is_seam_edge(coords[i], coords[i + 1], extent)).collect();

    if seam.iter().all(|&s| !s) {
        // No seam edges at all: the whole ring is real geometry.
        return vec![ring.clone()];
    }
    let Some(first_seam) = seam.iter().position(|&s| s) else {
        return vec![ring.clone()];
    };
    if seam.iter().all(|&s| s) {
        // The entire ring is clip-rectangle edges (e.g. a tile fully inside
        // a water/landcover polygon) — nothing real to draw.
        return Vec::new();
    }

    // Walk the ring's edges starting just after a known seam edge so that
    // a run of real edges spanning the array's wraparound point is never
    // split in two.
    let mut runs = Vec::new();
    let mut current: Vec<geo_types::Coord<i32>> = Vec::new();
    let mut idx = (first_seam + 1) % edge_count;
    for _ in 0..edge_count {
        if seam[idx] {
            if current.len() >= 2 {
                runs.push(geo_types::LineString::new(std::mem::take(&mut current)));
            } else {
                current.clear();
            }
        } else {
            if current.is_empty() {
                current.push(coords[idx]);
            }
            current.push(coords[idx + 1]);
        }
        idx = (idx + 1) % edge_count;
    }
    if current.len() >= 2 {
        runs.push(geo_types::LineString::new(current));
    }
    runs
}

/// Sutherland-Hodgman: clips a (closed, ring-shaped) sequence of points
/// against a single half-plane, defined by `keep` (true = point is on the
/// kept side) and `intersect` (computes where an edge crossing the
/// half-plane's boundary intersects it). Standard algorithm: walk the
/// ring's edges; for each edge, if the current point is kept, emit the
/// boundary-intersection point first (only when the previous point was
/// NOT kept, i.e. the edge just entered the kept region), then emit the
/// current point; if the current point is not kept but the previous one
/// was, emit only the boundary-intersection point (the edge just left the
/// kept region). Runs `points` as a closed ring (compares the last point
/// against the first as the final edge).
fn clip_ring_to_half_plane(
    points: &[geo_types::Coord<i32>],
    keep: impl Fn(geo_types::Coord<i32>) -> bool,
    intersect: impl Fn(geo_types::Coord<i32>, geo_types::Coord<i32>) -> geo_types::Coord<i32>,
) -> Vec<geo_types::Coord<i32>> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut output = Vec::new();
    let mut prev = *points.last().unwrap();
    let mut prev_in = keep(prev);
    for &curr in points {
        let curr_in = keep(curr);
        if curr_in {
            if !prev_in {
                output.push(intersect(prev, curr));
            }
            output.push(curr);
        } else if prev_in {
            output.push(intersect(prev, curr));
        }
        prev = curr;
        prev_in = curr_in;
    }
    output
}

/// Clips a polygon ring (as an open point list -- the caller strips the
/// closing duplicate point first, if the source is a closed
/// `geo_types::LineString`) to `[0, extent] x [0, extent]` by applying
/// `clip_ring_to_half_plane` against all four sides in sequence (each
/// pass's output feeds the next pass's input, the standard Sutherland-
/// Hodgman composition for a convex clip window). Intersection points are
/// computed with `i64` arithmetic to avoid overflow during the
/// cross-multiplication, then rounded to the nearest `i32` -- integer
/// input geometry means clipped intersection points are not generally
/// exact integers, and this codebase's `geo_types::Geometry<i32>`
/// convention (a fixed tile-local integer grid, further scaled by
/// `COORD_SCALE` at tessellation time) makes sub-unit rounding error here
/// imperceptible at any real render scale.
fn clip_ring_to_tile_extent(points: &[geo_types::Coord<i32>], extent: i32) -> Vec<geo_types::Coord<i32>> {
    use geo_types::Coord;

    fn lerp_y(a: Coord<i32>, b: Coord<i32>, x: i32) -> i32 {
        let (ax, ay, bx, by) = (a.x as i64, a.y as i64, b.x as i64, b.y as i64);
        let t_num = (x as i64) - ax;
        let t_den = bx - ax;
        (ay + (t_num * (by - ay)) / t_den) as i32
    }
    fn lerp_x(a: Coord<i32>, b: Coord<i32>, y: i32) -> i32 {
        let (ax, ay, bx, by) = (a.x as i64, a.y as i64, b.x as i64, b.y as i64);
        let t_num = (y as i64) - ay;
        let t_den = by - ay;
        (ax + (t_num * (bx - ax)) / t_den) as i32
    }

    let left = clip_ring_to_half_plane(
        points,
        |p| p.x >= 0,
        |a, b| Coord { x: 0, y: lerp_y(a, b, 0) },
    );
    let right = clip_ring_to_half_plane(
        &left,
        |p| p.x <= extent,
        |a, b| Coord { x: extent, y: lerp_y(a, b, extent) },
    );
    let bottom = clip_ring_to_half_plane(
        &right,
        |p| p.y >= 0,
        |a, b| Coord { x: lerp_x(a, b, 0), y: 0 },
    );
    clip_ring_to_half_plane(
        &bottom,
        |p| p.y <= extent,
        |a, b| Coord { x: lerp_x(a, b, extent), y: extent },
    )
}

/// Clips a `Fill` feature's geometry to the nominal tile extent (see
/// `TILE_EXTENT`'s doc comment for why vector tile data commonly extends
/// past it) before it's pushed into `PaintOp::Fill` -- prevents two
/// adjacent tiles' overlapping "buffer" geometry from both drawing into
/// the same screen-space strip, which double-blends semi-transparent
/// fills into a visible seam at tile boundaries. `Line`-sourced geometry
/// and any ring that clips away entirely (fewer than 3 resulting points)
/// pass through unchanged / are dropped respectively; `Point`/
/// `MultiPoint` etc. pass through unchanged (fills never use them, this
/// function is only ever called from the `Fill` arm).
pub(crate) fn clip_fill_geometry_to_tile_extent(geometry: &geo_types::Geometry<i32>) -> geo_types::Geometry<i32> {
    use geo_types::{Coord, Geometry, Geometry::*, LineString, Polygon};

    fn open_points(ring: &LineString<i32>) -> Vec<Coord<i32>> {
        let coords: Vec<Coord<i32>> = ring.coords().copied().collect();
        if coords.len() > 1 && coords[0] == *coords.last().unwrap() {
            coords[..coords.len() - 1].to_vec()
        } else {
            coords
        }
    }

    fn clip_polygon(p: &Polygon<i32>) -> Option<Polygon<i32>> {
        let clipped_exterior = clip_ring_to_tile_extent(&open_points(p.exterior()), TILE_EXTENT);
        if clipped_exterior.len() < 3 {
            return None;
        }
        let mut exterior = clipped_exterior;
        exterior.push(exterior[0]); // re-close the ring
        let interiors: Vec<LineString<i32>> = p
            .interiors()
            .iter()
            .filter_map(|interior| {
                let clipped = clip_ring_to_tile_extent(&open_points(interior), TILE_EXTENT);
                if clipped.len() < 3 {
                    return None;
                }
                let mut ring = clipped;
                ring.push(ring[0]);
                Some(LineString::new(ring))
            })
            .collect();
        Some(Polygon::new(LineString::new(exterior), interiors))
    }

    match geometry {
        Polygon(p) => clip_polygon(p).map(Geometry::Polygon).unwrap_or(Geometry::MultiPolygon(geo_types::MultiPolygon::new(vec![]))),
        MultiPolygon(mp) => {
            let polygons: Vec<Polygon<i32>> = mp.iter().filter_map(clip_polygon).collect();
            Geometry::MultiPolygon(geo_types::MultiPolygon::new(polygons))
        }
        other => other.clone(),
    }
}

impl StyleLayer {
    /// The layer's `"id"` from the style document, verbatim. Not unique
    /// across a compiled `Style`: one JSON layer whose paint is a
    /// feature-dependent `match` expands into one `StyleLayer` per case,
    /// and every one of them carries the JSON layer's id (see
    /// `style_spec::parse_and_compile`).
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            StyleLayer::Background { id, .. }
            | StyleLayer::Fill { id, .. }
            | StyleLayer::Extrusion { id, .. }
            | StyleLayer::Line { id, .. }
            | StyleLayer::Symbol { id, .. }
            | StyleLayer::Raster { id, .. }
            | StyleLayer::Circle { id, .. } => id,
        }
    }

    /// The style-spec type name this layer compiled from -- `"background"`,
    /// `"fill"`, `"fill-extrusion"`, `"line"`, `"symbol"` or `"raster"`. A stable string,
    /// because it is reported over the agent-function wire
    /// (`map.list_layers`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            StyleLayer::Background { .. } => "background",
            StyleLayer::Fill { .. } => "fill",
            StyleLayer::Extrusion { .. } => "fill-extrusion",
            StyleLayer::Line { .. } => "line",
            StyleLayer::Symbol { .. } => "symbol",
            StyleLayer::Raster { .. } => "raster",
            StyleLayer::Circle { .. } => "circle",
        }
    }

    /// The layer's `[minzoom, maxzoom]`, as compiled.
    #[must_use]
    pub fn zoom_range(&self) -> (Option<u8>, Option<u8>) {
        layer_zoom_range(self)
    }

    /// The style `sources` key a `StyleLayer::Raster`'s imagery comes from,
    /// or `None` for every other layer kind.
    ///
    /// A plain accessor, not a decision: it exists so a caller that only
    /// needs the SOURCE NAME (to resolve a tile URL, say -- see
    /// `web/src/app.rs`'s `resolve_raster_config`) never has to destructure
    /// `StyleLayer::Raster { .. }` itself. That distinction is not
    /// cosmetic: `tests/raster_plan_parity.rs` bans exactly that
    /// destructuring in shell code, because a shell that CAN see the
    /// variant's other fields (`opacity`, `min_zoom`, `max_zoom`) can also
    /// be tempted to resolve them itself instead of reading
    /// `render::raster_plan::RasterPlan`. This accessor has nothing to
    /// tempt it with.
    #[must_use]
    pub fn raster_source(&self) -> Option<&str> {
        match self {
            StyleLayer::Raster { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn layer_zoom_range(layer: &StyleLayer) -> (Option<u8>, Option<u8>) {
    match layer {
        StyleLayer::Background { id: _, min_zoom, max_zoom, .. }
        | StyleLayer::Fill { id: _, min_zoom, max_zoom, .. }
        | StyleLayer::Extrusion { id: _, min_zoom, max_zoom, .. }
        | StyleLayer::Line { id: _, min_zoom, max_zoom, .. }
        | StyleLayer::Symbol { id: _, min_zoom, max_zoom, .. }
        | StyleLayer::Raster { id: _, min_zoom, max_zoom, .. }
        | StyleLayer::Circle { id: _, min_zoom, max_zoom, .. } => (*min_zoom, *max_zoom),
    }
}

/// Smooth 1-full-zoom-level appear/disappear fade for a layer's
/// `min_zoom`/`max_zoom` cutoff, riding the same GPU-side pair-blend every
/// other zoom-dependent paint value uses (`ZoomValue::value_at_pair` +
/// `fract(camera.zoom)`). Evaluated at the current zoom bucket (`zoom`)
/// and the next one (`zoom + 1`), the result multiplies into the resolved
/// alpha at `Fill`/`Line`'s `color_from`/`color_to` respectively -- the
/// shader then blends between them continuously as the camera eases
/// through that bucket, so a layer with `min_zoom: N` fades from fully
/// transparent (at bucket N) to fully opaque (at bucket N+1) over exactly
/// one zoom level, instead of the previous hard "invisible below N,
/// instantly full-opacity at N" cutoff.
///
/// The render/skip decision in `resolve_plan`'s existing zoom-range gate
/// (`zoom < min_zoom || zoom > max_zoom`) is UNCHANGED and still correct
/// for the common case (`max_zoom > min_zoom`, or either bound `None`):
/// there, this fade is 0 for BOTH `zoom` and `zoom + 1` exactly when that
/// gate would already skip the layer. That equivalence does NOT hold when
/// `max_zoom <= min_zoom` -- the narrow-range guard above handles that
/// case separately by falling back to the gate's original hard-toggle
/// semantics directly, rather than relying on the (false, in that case)
/// equivalence.
///
/// Deliberately NOT applied to `StyleLayer::Background`: its LIVE
/// clear-color computation lives separately in `map_view.rs`'s `paint()`
/// (not `resolve_plan`'s cached `PaintPlan`), and background layers with an
/// explicit `min_zoom`/`max_zoom` are rare in real styles -- out of scope
/// for this pass.
fn layer_zoom_fade(min_zoom: Option<u8>, max_zoom: Option<u8>, zoom: u8) -> f32 {
    // Guard against discrete-sampling aliasing: this function is only ever
    // evaluated at two INTEGER sample points (`zoom` and `zoom + 1`, baked
    // into a GPU vertex-pair blend elsewhere). When `min_zoom`/`max_zoom`
    // are close together, the TRUE continuous fade trapezoid this is
    // meant to approximate has its peak strictly BETWEEN those two sample
    // points -- sampling only at the integers misses it entirely and
    // aliases to a flat 0 across the whole range. Concretely:
    // `min_zoom == max_zoom == 10` previously (before this guard) went
    // PERMANENTLY INVISIBLE, a real regression from a real style pattern
    // (a layer visible at exactly one zoom level). Below this width
    // (`max_zoom < min_zoom + 1`, i.e. `max_zoom <= min_zoom` for
    // integers), fall back to the pre-fade hard single-bucket toggle
    // instead of the aliased product.
    if let (Some(mn), Some(mx)) = (min_zoom, max_zoom) {
        if mx < mn.saturating_add(1) {
            // Mirror `resolve_plan`'s actual gate (`zoom < min_zoom || zoom >
            // max_zoom` => skip) directly, rather than assuming `mn == mx`
            // (a reversed range like `min_zoom: 10, max_zoom: 5` is always
            // skipped by that gate, at every zoom -- not just "visible
            // when zoom == min_zoom").
            return if zoom >= mn && zoom <= mx { 1.0 } else { 0.0 };
        }
    }
    let appear = match min_zoom {
        None => 1.0,
        // **`min_zoom: 0` has no bucket BELOW it to fade in from.** The ramp
        // below runs across `[mz, mz + 1)` -- transparent at the bottom of
        // the layer's first valid bucket, opaque at the top -- and that
        // reads as a fade-IN only because the camera arrives from `mz - 1`,
        // where the layer was legitimately not drawn at all. Zoom 0 is the
        // floor of the camera's whole range, so there is no such approach: a
        // `min_zoom: 0` layer was simply INVISIBLE at zoom 0.0 and
        // half-strength at 0.5, with nothing the user could do to reach it.
        // Note the asymmetry this removes -- `disappear` below is already
        // inclusive (`zoom <= mz`), because `max_zoom` has a bucket on both
        // sides of it and `min_zoom: 0` does not.
        //
        // On a style that declares a `minzoom` on every layer this is the
        // whole map: Mapbox Streets does exactly that on all 134 of its
        // layers, and the five that survive to z0 (`landcover`, `water`,
        // `water-depth`, `hillshade` and the `land` background) all declare
        // `minzoom: 0`, so every fill went to alpha 0 together and the globe
        // came up as the backdrop sphere alone -- a blank pale disc carrying
        // 62,183 vertices and 95,145 triangles it had drawn at zero alpha,
        // and fully black with the backdrop off. `PaintOp::Background` is
        // exempt from this fade (see the doc comment above), which is
        // exactly why the disc was the background colour rather than
        // nothing. Graded on pixels by
        // `capture::tests::a_min_zoom_of_zero_paints_at_zoom_zero`.
        Some(0) => 1.0,
        Some(mz) => if zoom > mz { 1.0 } else { 0.0 },
    };
    let disappear = match max_zoom {
        None => 1.0,
        Some(mz) => if zoom <= mz { 1.0 } else { 0.0 },
    };
    appear * disappear
}

/// Computes the `(fade_from, fade_to)` pair `resolve_plan` bakes into a
/// `PaintOp`'s `color_from.a`/`color_to.a`, together rather than via two
/// independent `layer_zoom_fade` calls at `zoom` and `zoom + 1`.
///
/// This matters specifically for the narrow-range case
/// (`max_zoom <= min_zoom`, e.g. a layer visible at exactly one zoom
/// level): calling `layer_zoom_fade` independently at `zoom` (in range,
/// 1.0) and `zoom + 1` (out of range, 0.0) would bake the pair `(1.0,
/// 0.0)`, which the GPU shader then linearly blends across the ENTIRE
/// bucket where this layer is visible -- so it would visibly fade from
/// full opacity down to invisible across its one valid zoom bucket,
/// instead of holding at constant full opacity throughout it (the actual
/// intent of the narrow-range fallback: a hard single-bucket toggle, not
/// a fade). Evaluating both pair elements together from the SAME check
/// (whether the CURRENT bucket `zoom` is in range) instead gives a
/// constant `(1.0, 1.0)` or `(0.0, 0.0)` pair -- `mix(v, v, t) == v` for
/// any `t`, so no blending happens within the bucket, and the layer's
/// visibility snaps off entirely at the bucket boundary instead (handled
/// correctly by `resolve_plan`'s unchanged zoom-range skip gate, which
/// excludes the layer from the `PaintPlan` outside its valid bucket, so
/// there's nothing left to blend toward).
///
/// The wide-range (smooth-fade) case is unaffected: it still evaluates
/// `layer_zoom_fade` independently at `zoom` and `zoom + 1`, exactly as
/// `resolve_plan` did before this function existed.
fn layer_zoom_fade_pair(min_zoom: Option<u8>, max_zoom: Option<u8>, zoom: u8) -> (f32, f32) {
    if let (Some(mn), Some(mx)) = (min_zoom, max_zoom) {
        if mx < mn.saturating_add(1) {
            let v = if zoom >= mn && zoom <= mx { 1.0 } else { 0.0 };
            return (v, v);
        }
    }
    (layer_zoom_fade(min_zoom, max_zoom, zoom), layer_zoom_fade(min_zoom, max_zoom, zoom.saturating_add(1)))
}

/// A zoom-invariant `Filter::matches` result, cached once per `Fill`/`Line`
/// style layer by `match_tile_static` and consumed by `resolve_plan`.
///
/// Only ever built for a layer whose `filter.uses_zoom() == false`: the
/// matched feature set can't change with zoom, so the filter pass (14,835
/// `Filter::matches` calls per tile, per the design spec this task
/// implements) and the geometry clip/boundary-conversion that immediately
/// follows a match can both run exactly once per tile instead of once per
/// zoom-bucket crossing. `Dynamic` marks the alternative -- a layer whose
/// filter DOES consult zoom, which `resolve_plan` re-filters from scratch
/// at every call, exactly as `match_tile` always has.
///
/// Deliberately does NOT cache resolved paint values (`color`/`opacity`/
/// `width`): those come from `ZoomValue::value_at_pair`, which the design
/// spec measured at ~0.006ms per tile -- not worth caching, and caching them
/// here would either freeze them at match time (wrong -- they must track
/// the CURRENT zoom) or require threading zoom into `match_tile_static`,
/// which would defeat the whole point of a zoom-independent cache.
#[derive(Debug, Clone)]
enum FillMatch {
    /// The matched features' clipped geometry, and each one's dot-mask rank
    /// (`overlay::dotgrid::mask_class::classify`), index for index -- or an
    /// EMPTY rank list when the layer's source layer cannot rank anything
    /// but `Land` ([`crate::overlay::dotgrid::mask_class::can_rank`]), so
    /// the commonest fill layers carry no per-feature ranks at all.
    Static(Vec<Arc<geo_types::Geometry<i32>>>, Vec<crate::overlay::dotgrid::mask_class::MaskRank>),
    Dynamic,
}

/// The `Line`-layer sibling of `FillMatch`. A separate type (rather than
/// reusing `FillMatch`) because the two hold geometry through different
/// preprocessing steps -- `clip_fill_geometry_to_tile_extent` vs.
/// `boundary_geometry_for_line` -- and collapsing them would blur which
/// preprocessing a cached entry already had applied.
#[derive(Debug, Clone)]
enum LineMatch {
    Static(Vec<Arc<geo_types::Geometry<i32>>>),
    Dynamic,
}

/// Whether `geometry` can be extruded: only polygons have walls and a roof.
fn is_polygonal(geometry: &geo_types::Geometry<i32>) -> bool {
    matches!(geometry, geo_types::Geometry::Polygon(_) | geo_types::Geometry::MultiPolygon(_))
}

/// The `Extrusion`-layer sibling of `FillMatch`. It keeps each feature's
/// index in its source layer (heights and bases are resolved from that
/// feature's tags at bake time) and its UNCLIPPED geometry: walls are built
/// from the real ring, and a wall along a tile's clip edge would be a wall
/// that does not exist.
#[derive(Debug, Clone)]
enum ExtrusionMatch {
    /// (feature index in the source layer, the feature's UNCLIPPED
    /// geometry, and -- on a source layer the dot mask can rank
    /// (`mask_class::can_rank`) -- its CLIPPED footprint, shared with any
    /// flat fill of the same feature, for [`mask_footprint_op`])
    Static(Vec<(u32, Arc<geo_types::Geometry<i32>>, Option<Arc<geo_types::Geometry<i32>>>)>),
    Dynamic,
}

/// One `style.layers` entry's cached match, indexed 1:1 and in the same
/// order as `style.layers` (`TileMatch::entries[i]` corresponds to
/// `style.layers[i]`) so `resolve_plan` can zip the two without a lookup.
/// `Background`/`Symbol` carry no per-feature match state -- `Background`
/// has no features to filter, and `Symbol` layers don't produce `PaintOp`s
/// at all (see `match_tile`'s doc comment on that arm) -- but still need an
/// entry to keep the index alignment.
#[derive(Debug, Clone)]
enum LayerMatch {
    Background,
    Fill(FillMatch),
    Line(LineMatch),
    Extrusion(ExtrusionMatch),
    Symbol,
    /// Like `Background` and `Symbol`, a placeholder that carries no
    /// per-feature match state: a raster layer filters nothing, because
    /// its pixels are not in this tile's decoded features at all. It
    /// exists to hold the index alignment with `style.layers`.
    Raster,
    /// A `circle` layer: drawn by the vector-overlay path, never by the
    /// tile bake, so it matches nothing here (index alignment only).
    Circle,
}

/// A tile's filter pass against a `Style`, cached independent of zoom.
/// Opaque to callers outside this module except through `match_tile_static`
/// (produces one) and `resolve_plan` (consumes one) -- see those functions'
/// doc comments for the caching contract this exists to support.
#[derive(Debug, Clone)]
pub struct TileMatch {
    entries: Vec<LayerMatch>,
    /// Whether the static fill matches carry their dot-mask ranks
    /// ([`match_tile_static_masked`]), and so whether
    /// [`resolve_plan_with_mask`] classifies at all.
    ranked: bool,
}

/// Runs the zoom-independent half of `match_tile`'s work: for every style
/// layer whose filter is zoom-static (`Filter::uses_zoom() == false`), runs
/// `Filter::matches` once and keeps the matched features' clipped/
/// boundary-converted geometry; for a layer whose filter is dynamic, records
/// nothing (`resolve_plan` re-filters it at every call, same as
/// `match_tile` always has).
///
/// Evaluating a zoom-static filter's `matches` at `0.0` here is safe by
/// construction: `uses_zoom() == false` on the top-level filter means no
/// branch anywhere in its tree reads the zoom argument, recursively (see
/// `Filter::uses_zoom`'s doc comment), so the actual zoom value passed to
/// `matches` cannot affect its result. This is a strictly *filter-level*
/// fact, though -- it says nothing about whether the LAYER's output is
/// zoom-invariant. Deliberately does NOT apply the layer's own
/// `min_zoom`/`max_zoom` gate here: gating at match time would make the
/// cached match zoom-dependent (a layer's match would need to be
/// recomputed every time the zoom crosses that layer's `min_zoom`/
/// `max_zoom`, defeating the point of caching it at all) and would corrupt
/// rendering the moment the cache is reused across zoom buckets straddling
/// the gate -- see `resolve_plan`, which applies that gate on every call
/// instead.
pub fn match_tile_static(tile: &DecodedTile, style: &Style) -> TileMatch {
    match_tile_static_masked(tile, style, false)
}

/// [`match_tile_static`], and -- when `masked` -- each matched static fill
/// feature's dot-mask rank (`overlay::dotgrid::mask_class::classify`), so
/// that [`resolve_plan_with_mask`] on this match lists the plan's ranked
/// fill ops. Unmasked, nothing is classified: the dot mask's CPU half is
/// paid only while a dot layer relocates (the final-review I1 ruling:
/// classifying every bake cost the match + resolve stage +2.7 % on the
/// Singapore z14 fixture and +3.3 to +3.5 % on a Manhattan z15 tile,
/// quiet interleaved A/B against the stage without it -- past the 2 %
/// bar). `MapView` bakes with its `basemap_masked` gate.
pub fn match_tile_static_masked(tile: &DecodedTile, style: &Style, masked: bool) -> TileMatch {
    // One evaluator per tile layer, shared by every style layer on it: a
    // key is resolved, and a leaf predicate decided, once per tile layer
    // rather than once per (style layer, feature). See `columnar`.
    let mut eval = TileEval::new(tile, 0.0);
    // A feature matched by several style layers (a road's casing and its
    // fill, a landcover fill and its outline layer) is clipped or
    // boundary-converted ONCE and its `Arc` shared: the conversion depends
    // on the geometry alone, and after the filter walk went columnar it was
    // ~87% of this function on a dense z14 tile.
    type Memo<'t> = HashMap<&'t str, Vec<Option<Arc<geo_types::Geometry<i32>>>>>;
    let mut fill_memo: Memo<'_> = HashMap::new();
    let mut line_memo: Memo<'_> = HashMap::new();
    // Extrusions keep the UNCLIPPED geometry (walls come from the real
    // rings), so they cannot share `fill_memo`'s clipped `Arc`s.
    let mut raw_memo: Memo<'_> = HashMap::new();
    fn shared<'t>(
        memo: &mut Memo<'t>,
        tile_layer: &'t crate::mvt::Layer,
        index: usize,
        convert: fn(&geo_types::Geometry<i32>) -> geo_types::Geometry<i32>,
    ) -> Arc<geo_types::Geometry<i32>> {
        let slots = memo.entry(tile_layer.name.as_str()).or_insert_with(|| vec![None; tile_layer.features.len()]);
        Arc::clone(slots[index].get_or_insert_with(|| Arc::new(convert(&tile_layer.features[index].geometry))))
    }
    let entries = style
        .layers
        .iter()
        .map(|layer| match layer {
            StyleLayer::Background { .. } => LayerMatch::Background,
            StyleLayer::Fill { source_layer, filter, .. } => {
                if filter.uses_zoom() {
                    return LayerMatch::Fill(FillMatch::Dynamic);
                }
                let (geometries, ranks) = eval
                    .matching(source_layer, filter)
                    .map(|(tile_layer, bits)| {
                        let geometries =
                            bits.iter().map(|i| shared(&mut fill_memo, tile_layer, i, clip_fill_geometry_to_tile_extent)).collect();
                        let ranks = if masked && mask_class::can_rank(&tile_layer.name) {
                            bits.iter().map(|i| mask_class::classify(&tile_layer.name, &tile_layer.features[i].tags)).collect()
                        } else {
                            Vec::new()
                        };
                        (geometries, ranks)
                    })
                    .unwrap_or_default();
                LayerMatch::Fill(FillMatch::Static(geometries, ranks))
            }
            StyleLayer::Line { source_layer, filter, .. } => {
                if filter.uses_zoom() {
                    return LayerMatch::Line(LineMatch::Dynamic);
                }
                let geometries = eval
                    .matching(source_layer, filter)
                    .map(|(tile_layer, bits)| {
                        bits.iter().map(|i| shared(&mut line_memo, tile_layer, i, boundary_geometry_for_line)).collect()
                    })
                    .unwrap_or_default();
                LayerMatch::Line(LineMatch::Static(geometries))
            }
            StyleLayer::Extrusion { source_layer, filter, .. } => {
                if filter.uses_zoom() {
                    return LayerMatch::Extrusion(ExtrusionMatch::Dynamic);
                }
                let footprints = mask_class::can_rank(source_layer);
                let features = eval
                    .matching(source_layer, filter)
                    .map(|(tile_layer, bits)| {
                        bits.iter()
                            .filter(|&i| is_polygonal(&tile_layer.features[i].geometry))
                            .map(|i| {
                                let footprint = footprints.then(|| shared(&mut fill_memo, tile_layer, i, clip_fill_geometry_to_tile_extent));
                                (i as u32, shared(&mut raw_memo, tile_layer, i, |g| g.clone()), footprint)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                LayerMatch::Extrusion(ExtrusionMatch::Static(features))
            }
            StyleLayer::Symbol { .. } => LayerMatch::Symbol,
            StyleLayer::Raster { .. } => LayerMatch::Raster,
            StyleLayer::Circle { .. } => LayerMatch::Circle,
        })
        .collect();
    TileMatch { entries, ranked: masked }
}

/// Runs the per-zoom half of a tile's bake against a `TileMatch` produced by
/// `match_tile_static`: applies the per-layer `min_zoom`/`max_zoom` gate and
/// `layer_zoom_fade_pair`, resolves `color`/`opacity`/`width` via
/// `value_at_pair(zoom)`, and emits `PaintOp`s -- with `Arc::clone`d
/// geometry for a statically-matched layer, or a from-scratch
/// `Filter::matches` + geometry pass for a dynamic one.
///
/// `zoom` is the camera's DISPLAY zoom (`MapView::display_zoom()`) -- the
/// camera's integer zoom bucket, deliberately unclamped by the tile
/// source's data ceiling -- NOT `tile.z` (which stays clamped, for tile
/// fetching and geometry transforms only; see `display_zoom`'s doc
/// comment in `map_view.rs`). `map_view.rs` has two production call
/// sites: the decode-completion handler (baking a newly-decoded tile for
/// the first time, immediately after building its `TileMatch` via
/// `match_tile_static`) and `MapView::plan_for` (rebaking an
/// already-decoded tile lazily, on the read that needs it, once the
/// display zoom has crossed an integer boundary since it was last baked,
/// reusing the SAME cached `TileMatch`), both passing `display_zoom()`.
/// Because `plan_for` guarantees every tile `paint()` actually draws gets
/// its baked pair brought current before it's drawn, the per-frame,
/// per-tile GPU blend factor between a `color_from`/`color_to` pair is
/// simply `camera.zoom.fract()` -- see `MapView::tile_blend_t` and
/// docs/superpowers/specs/2026-08-20-per-tile-vertex-binder-design.md
/// (historical design context; superseded on the zoom-argument point by
/// display-zoom style resolution, see that comment's citing call site).
///
/// The zoom gate is applied HERE, not in `match_tile_static`, deliberately:
/// see that function's doc comment for why gating at match time would
/// corrupt the cache. A layer outside its zoom range is still present in
/// `m` (matched, possibly with cached geometry); it's simply skipped here,
/// so it contributes no ops at this zoom.
///
/// `TileMatch::entries` must align 1:1 with `style.layers` -- only
/// `match_tile_static(tile, style)` establishes that alignment for a given
/// `(tile, style)` pair. Panics if `m.entries` and `style.layers` have
/// DIFFERENT LENGTHS, which catches `m` having been built against a style
/// with a different layer count than `style`. It does NOT catch every
/// misuse: a `TileMatch` built against a *different* tile (or a
/// structurally different style with the SAME layer count) passes this
/// check and then aligns variant-for-variant with `style.layers`, silently
/// resolving the wrong tile's/style's static geometry rather than
/// panicking. Unreachable in production today -- `MapView::plan_for` and
/// the decode-completion handler always read `match_cache`/`decoded_cache`
/// under the same `TileId`, and `style` is immutable after construction --
/// so this is a defense against a future caller getting that pairing
/// wrong, not a currently-live bug.
pub fn resolve_plan(tile: &DecodedTile, style: &Style, m: &TileMatch, zoom: u8) -> PaintPlan {
    resolve_plan_with_mask(tile, style, m, zoom).0
}

/// Whether an extrusion layer puts its footprints in the plan as
/// [`mask_footprint_op`]s at `zoom`: its source layer is one the dot mask
/// ranks (`mask_class::can_rank`, i.e. `building`) and its opacity at the
/// bake zoom reaches `mask_class::BUILDING_MIN_OPACITY`, the rule a flat
/// building fill ranks by (final-review M1). While `fill-extrusion`
/// compiled to a flat fill those fills were what put buildings in the dot
/// mask; the mask pass draws only the flat mesh's index lists, so an
/// extrusion's footprint must be in the flat mesh to be ranked.
///
/// Decided from the style and zoom alone, never from whether the match is
/// masked: a masked and an unmasked bake must produce the same plan
/// (`map_view::classify_resident_tile` re-ranks a resident plan and
/// requires it).
fn extrusion_mask_footprints(source_layer: &str, opacity: &ZoomValue<f32>, zoom: u8) -> bool {
    mask_class::can_rank(source_layer) && opacity.value_at_pair(zoom).0 >= mask_class::BUILDING_MIN_OPACITY
}

/// A building extrusion's footprint in the flat mesh for the dot mask
/// ([`extrusion_mask_footprints`]): a fill of fully transparent black, so
/// `ALPHA_BLENDING` leaves every pixel as it was, over the clipped
/// footprint. It costs what the same footprint cost as a visible flat fill
/// before `fill-extrusion` drew in 3D, and no more.
fn mask_footprint_op(geometry: Arc<geo_types::Geometry<i32>>) -> PaintOp {
    let clear = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    PaintOp::Fill { color_from: clear, color_to: clear, geometry }
}

/// [`resolve_plan`], and the plan's dot-mask ranks: `(op index, rank)` for
/// every `PaintOp::Fill` whose feature ranks other than `Land`
/// (`overlay::dotgrid::mask_class::classify` on the feature's tile layer
/// and tags), ascending by op index. A static layer's ranks were computed
/// once by [`match_tile_static_masked`]; a dynamic layer's feature is
/// classified here, as it is matched. Outline ops are never ranked: they
/// are lines. On a match built unmasked ([`match_tile_static`]) nothing is
/// classified and the ranks come back empty and
/// [`MaskRanks::classified`] `false`.
pub fn resolve_plan_with_mask(tile: &DecodedTile, style: &Style, m: &TileMatch, zoom: u8) -> (PaintPlan, MaskRanks) {
    let mut mask = MaskRanks { classified: m.ranked, ..MaskRanks::default() };
    assert_eq!(
        m.entries.len(),
        style.layers.len(),
        "TileMatch::entries must align 1:1 with style.layers -- \
         `m` was not produced by match_tile_static(tile, style)"
    );
    let mut ops = Vec::new();
    // Built lazily: a style whose filters are all zoom-static never asks it for a layer.
    let mut eval = TileEval::new(tile, zoom as f64);
    for (layer, entry) in style.layers.iter().zip(&m.entries) {
        let (min_zoom, max_zoom) = layer_zoom_range(layer);
        if min_zoom.is_some_and(|mz| zoom < mz) || max_zoom.is_some_and(|mz| zoom > mz) {
            continue;
        }
        match (layer, entry) {
            (StyleLayer::Background { color, .. }, LayerMatch::Background) => {
                ops.push(PaintOp::Background { color: color.value_at(zoom) });
            }
            (
                StyleLayer::Fill { source_layer, filter, color, opacity, outline_color, min_zoom, max_zoom, .. },
                LayerMatch::Fill(fill_match),
            ) => {
                let (fade_from, fade_to) = layer_zoom_fade_pair(*min_zoom, *max_zoom, zoom);
                let (color_from, color_to) = color.value_at_pair(zoom);
                let (opacity_from, opacity_to) = opacity.value_at_pair(zoom);
                let mut resolved_from = color_from;
                resolved_from.a *= opacity_from * fade_from;
                let mut resolved_to = color_to;
                resolved_to.a *= opacity_to * fade_to;
                // A building ranks only where the style draws it: its fill
                // at >= `mask_class::BUILDING_MIN_OPACITY` at the plan's
                // (bake) zoom; fainter, its texels are land -- dots do not
                // gather on footprints the map does not show (Task 9 ruling;
                // Streets fades `building` in from 0 at z15 to 1 at z16).
                let shown = opacity_from * fade_from >= mask_class::BUILDING_MIN_OPACITY;
                let ranked = |rank: MaskRank| if rank == MaskRank::Building && !shown { MaskRank::Land } else { rank };
                // `fill-outline-color` is modulated by `fill-opacity` (and
                // by the layer's zoom fade), not by an outline opacity of
                // its own -- the style spec gives the outline no opacity
                // property, and MapLibre's `fill_outline.fragment.glsl`
                // multiplies it by the same `opacity` the fill uses
                // (`fragColor = outline_color * (alpha * opacity)`).
                let outline = outline_color.as_ref().map(|outline| {
                    let (outline_from, outline_to) = outline.value_at_pair(zoom);
                    let mut from = outline_from;
                    from.a *= opacity_from * fade_from;
                    let mut to = outline_to;
                    to.a *= opacity_to * fade_to;
                    (from, to)
                });
                // The outline of a fill is a `PaintOp::Line` over the fill's
                // own ring geometry, at a fixed ONE-PIXEL width.
                //
                // One pixel regardless of zoom is MapLibre's behaviour, not
                // an approximation of it: `draw_fill.ts` draws the outline
                // with `gl.LINES`, whose line width is one device pixel and
                // has no zoom term anywhere. `PaintOp::Line`'s width is
                // already in SCREEN pixels (`render::mesh::Vertex`'s
                // `half_width_px`, extruded in the vertex shader by
                // `half_width_px * world_units_per_px`), so a constant 1.0
                // here is exactly that -- it does not need, and must not
                // get, a zoom expression.
                let outline_op = |geometry: Arc<geo_types::Geometry<i32>>| {
                    let (from, to) = outline.expect("only called when `outline` is Some");
                    PaintOp::Line {
                        metrics: false,
                        color_from: from,
                        color_to: to,
                        width_from: FILL_OUTLINE_WIDTH_PX,
                        width_to: FILL_OUTLINE_WIDTH_PX,
                        // A fill outline is not a line layer and has no
                        // `line-gap-width` to read: MapLibre draws it with
                        // `gl.LINES` over the fill's own vertices, which has
                        // no notion of a hole down the middle.
                        gap_from: 0.0,
                        gap_to: 0.0,
                        // MapLibre's outline pass is `gl.LINES` over the
                        // fill's own vertices: a stream of independent
                        // one-pixel segments with no caps and no joins at
                        // all. `Butt` is the one cap keyword that adds
                        // nothing beyond the last vertex, so it is what
                        // reproduces that.
                        cap: LineCap::Butt,
                        // Ditto: `gl.LINES` has no joins. Bevel is the
                        // cheapest join that adds no vertices beyond the two
                        // a segment already has, and at the fixed one-pixel
                        // width of `FILL_OUTLINE_WIDTH_PX` no join keyword is
                        // distinguishable from any other anyway.
                        join: LineJoin::Bevel,
                        dash: DashPattern::none(),
                        geometry,
                    }
                };
                match fill_match {
                    FillMatch::Static(geometries, ranks) => {
                        for (k, geometry) in geometries.iter().enumerate() {
                            if let Some(rank) = ranks.get(k).map(|&r| ranked(r)).filter(|&r| r != MaskRank::Land) {
                                mask.ops.push((ops.len() as u32, rank));
                            }
                            ops.push(PaintOp::Fill {
                                color_from: resolved_from,
                                color_to: resolved_to,
                                geometry: Arc::clone(geometry),
                            });
                        }
                        // AFTER the whole layer's fills, not interleaved
                        // with them: within one fill layer the features
                        // overlap (adjacent building footprints share
                        // edges), so an outline pushed straight after its
                        // own fill would be painted over by the NEXT
                        // feature's fill and the edge would come and go by
                        // feature order. MapLibre keeps the same
                        // separation for the same reason -- `drawFill`
                        // runs every tile's fills, then `drawOutline`
                        // runs every tile's outlines, as two passes over
                        // the layer.
                        if outline.is_some() {
                            for geometry in geometries {
                                ops.push(outline_op(Arc::new(boundary_geometry_for_line(geometry))));
                            }
                        }
                    }
                    FillMatch::Dynamic => {
                        if let Some((tile_layer, bits)) = eval.matching(source_layer, filter) {
                            let rankable = m.ranked && mask_class::can_rank(&tile_layer.name);
                            // Only when an outline is actually wanted does
                            // the matched geometry need to be held on to
                            // for a second pass; the far commoner no-outline
                            // case keeps its original single streaming loop
                            // and allocates nothing extra.
                            let mut outlined: Vec<Arc<geo_types::Geometry<i32>>> = Vec::new();
                            for feature in bits.iter().map(|i| &tile_layer.features[i]) {
                                let geometry = Arc::new(clip_fill_geometry_to_tile_extent(&feature.geometry));
                                if outline.is_some() {
                                    outlined.push(Arc::clone(&geometry));
                                }
                                if rankable {
                                    let rank = ranked(mask_class::classify(&tile_layer.name, &feature.tags));
                                    if rank != MaskRank::Land {
                                        mask.ops.push((ops.len() as u32, rank));
                                    }
                                }
                                ops.push(PaintOp::Fill {
                                    color_from: resolved_from,
                                    color_to: resolved_to,
                                    geometry,
                                });
                            }
                            for geometry in &outlined {
                                ops.push(outline_op(Arc::new(boundary_geometry_for_line(geometry))));
                            }
                        }
                    }
                }
            }
            (
                StyleLayer::Line { source_layer, filter, color, width, opacity, gap_width, cap, join, dash, min_zoom, max_zoom, .. },
                LayerMatch::Line(line_match),
            ) => {
                let (fade_from, fade_to) = layer_zoom_fade_pair(*min_zoom, *max_zoom, zoom);
                let (color_from, color_to) = color.value_at_pair(zoom);
                let (opacity_from, opacity_to) = opacity.value_at_pair(zoom);
                let mut resolved_from = color_from;
                resolved_from.a *= opacity_from * fade_from;
                let mut resolved_to = color_to;
                resolved_to.a *= opacity_to * fade_to;
                let (width_from, width_to) = width.value_at_pair(zoom);
                // A pair, unlike `dash` below: `line-gap-width` IS an
                // interpolatable paint property, and Mapbox Streets ramps
                // every road casing's gap continuously with zoom. Baking
                // only one end would step the casing at each integer zoom
                // while the fill on top of it slid smoothly, which reads as
                // the casing breathing in and out from under the road.
                let (gap_from, gap_to) = gap_width.value_at_pair(zoom);
                // Not a pair: line-dasharray is never interpolated
                // ("expression": {"interpolated": false} in the style spec),
                // so the pattern at the display zoom is the whole value.
                let dash = dash.value_at(zoom);
                match line_match {
                    LineMatch::Static(geometries) => {
                        for geometry in geometries {
                            ops.push(PaintOp::Line {
                                metrics: false,
                                color_from: resolved_from,
                                color_to: resolved_to,
                                width_from,
                                width_to,
                                gap_from,
                                gap_to,
                                cap: *cap,
                                join: *join,
                                dash,
                                geometry: Arc::clone(geometry),
                            });
                        }
                    }
                    LineMatch::Dynamic => {
                        if let Some((tile_layer, bits)) = eval.matching(source_layer, filter) {
                            for feature in bits.iter().map(|i| &tile_layer.features[i]) {
                                ops.push(PaintOp::Line {
                                    metrics: false,
                                    color_from: resolved_from,
                                    color_to: resolved_to,
                                    width_from,
                                    width_to,
                                    gap_from,
                                    gap_to,
                                    cap: *cap,
                                    join: *join,
                                    dash,
                                    geometry: Arc::new(boundary_geometry_for_line(&feature.geometry)),
                                });
                            }
                        }
                    }
                }
            }
            // Symbol layers don't produce PaintOps -- they're baked
            // separately into a SymbolPlan by `match_tile_symbols` and
            // aren't rendered yet (see that function's doc comment).
            (StyleLayer::Symbol { .. }, LayerMatch::Symbol) => {}
            // Raster layers don't produce PaintOps either, and for a
            // stronger reason than symbols do: there is nothing in this
            // tile's decoded bytes for them to produce one FROM. Their
            // pixels arrive from a separate tile source and are drawn by
            // `render::raster_pipeline` in its own ordered group. The arm
            // is written out rather than left to the `_` below so that a
            // future variant mismatch is a compile error here, not an
            // `unreachable!` in a user's frame.
            (StyleLayer::Raster { .. }, LayerMatch::Raster) | (StyleLayer::Circle { .. }, LayerMatch::Circle) => {}
            // One `PaintOp::Extrusion` per matched polygonal feature, with
            // its height and base resolved from its own tags at the bracket
            // pair. Opacity and the layer zoom fade are deliberately NOT
            // folded into the row: opacity is a per-layer uniform (§2.1),
            // and R2 rules out an engine fade.
            (
                StyleLayer::Extrusion { source_layer, filter, group, color, opacity, height, base, .. },
                LayerMatch::Extrusion(extrusion_match),
            ) => {
                let Some(tile_layer) = tile.layers.iter().find(|l| l.name == *source_layer) else {
                    continue;
                };
                // The dot mask's building footprints (final-review M1): see
                // `extrusion_mask_footprints`. Pushed before the layer's
                // extrusion ops; the plan does not depend on `m.ranked`,
                // only the ranks do.
                if extrusion_mask_footprints(source_layer, opacity, zoom) {
                    let mut footprint = |i: usize, geometry: Arc<geo_types::Geometry<i32>>| {
                        if m.ranked {
                            let rank = mask_class::classify(&tile_layer.name, &tile_layer.features[i].tags);
                            if rank != MaskRank::Land {
                                mask.ops.push((ops.len() as u32, rank));
                            }
                        }
                        ops.push(mask_footprint_op(geometry));
                    };
                    match extrusion_match {
                        ExtrusionMatch::Static(features) => {
                            for (i, _, clipped) in features {
                                footprint(*i as usize, Arc::clone(clipped.as_ref().expect("a rankable layer keeps its footprints")));
                            }
                        }
                        ExtrusionMatch::Dynamic => {
                            if let Some((_, bits)) = eval.matching(source_layer, filter) {
                                for i in bits.iter() {
                                    let geometry = &tile_layer.features[i].geometry;
                                    if is_polygonal(geometry) {
                                        footprint(i, Arc::new(clip_fill_geometry_to_tile_extent(geometry)));
                                    }
                                }
                            }
                        }
                    }
                }
                let (color_from, color_to) = color.value_at_pair(zoom);
                let op = |i: usize, geometry: Arc<geo_types::Geometry<i32>>| {
                    let tags = &tile_layer.features[i].tags;
                    let (base_from, base_to) = base.resolve_pair(tags, zoom);
                    let (height_from, height_to) = height.resolve_pair(tags, zoom);
                    PaintOp::Extrusion {
                        group: *group,
                        row: ExtrusionRow { color_from, color_to, base_from, height_from, base_to, height_to },
                        geometry,
                    }
                };
                match extrusion_match {
                    ExtrusionMatch::Static(features) => {
                        for (i, geometry, _) in features {
                            ops.push(op(*i as usize, Arc::clone(geometry)));
                        }
                    }
                    ExtrusionMatch::Dynamic => {
                        if let Some((tile_layer, bits)) = eval.matching(source_layer, filter) {
                            for i in bits.iter() {
                                let geometry = &tile_layer.features[i].geometry;
                                if is_polygonal(geometry) {
                                    ops.push(op(i, Arc::new(geometry.clone())));
                                }
                            }
                        }
                    }
                }
            }
            _ => unreachable!(
                "TileMatch::entries must align 1:1 with style.layers -- \
                 `m` was not produced by match_tile_static(tile, style)"
            ),
        }
    }
    (PaintPlan { ops }, mask)
}

/// Compiles a decoded tile's features against `style` into a `PaintPlan` at
/// `zoom`, in one call: `resolve_plan(tile, style, &match_tile_static(tile,
/// style), zoom)`. See those two functions' doc comments for what each half
/// actually does (the zoom-independent/per-zoom split this exists to make
/// cacheable) and for the real, current explanation of the `zoom` argument
/// and the production call sites that resolve it.
///
/// Has NO production call sites: `MapView`'s off-thread bake (`bake_tile`,
/// which the decode-completion path runs on a tokio worker) and
/// `plan_for` both call `match_tile_static`/`resolve_plan` directly, so a
/// rebake triggered by crossing a display-zoom bucket can reuse the
/// already-cached `TileMatch` instead of re-running the filter pass this
/// function would throw away and redo every time. Kept only because it's
/// still the simplest one-call way to bake a tile with no `TileMatch` to
/// reuse across calls, which is all every one of its 20 remaining call
/// sites -- all `#[cfg(test)]` -- need.
pub fn match_tile(tile: &DecodedTile, style: &Style, zoom: u8) -> PaintPlan {
    resolve_plan(tile, style, &match_tile_static(tile, style), zoom)
}

/// A single point-anchored label/icon resolved from a `StyleLayer::Symbol`
/// against one tile feature, at a fixed zoom -- the symbol-layer analog of
/// `PaintOp`. `anchor` is in the same tile-local 0..4096 coordinate space as
/// `Feature::geometry`.
#[derive(Debug, Clone)]
pub struct SymbolFeature {
    pub anchor: geo_types::Point<i32>,
    pub text: Option<String>,
    pub icon: Option<String>,
    pub text_size: f32,
    pub text_offset: (f32, f32),
    /// This layer's `layout.text-anchor`, copied verbatim -- the property
    /// is a plain enum, not a data-driven one, so there is nothing to
    /// resolve against this feature's tags. Carried on the feature anyway,
    /// beside `text_offset`, because the draw loop merges symbols from many
    /// tiles' plans and has no `StyleLayer` in hand when it positions one.
    pub text_anchor: TextAnchor,
    /// This layer's `layout.text-max-width` in EMS -- the width past which
    /// the label wraps -- or `None` for a label the property does not apply
    /// to.
    ///
    /// `None` means "do not wrap", and it is not the same thing as the
    /// property being absent from the layer (that is
    /// `DEFAULT_TEXT_MAX_WIDTH_EM`, ten ems). The only `None` today is
    /// LINE-placed text: the style spec makes `text-max-width` require
    /// `symbol-placement: point`, and mapbox-gl-js enforces exactly that in
    /// `performSymbolLayout`, passing a max width of 0 for anything else. A
    /// street name repeated along a road is one line by construction, and
    /// stacking it into fragments across the carriageway would be worse than
    /// letting it run.
    ///
    /// Ems rather than pixels because the conversion needs the RESOLVED
    /// text size, which the draw loop already scales by the display's own
    /// factor; multiplying here would bake in one display's scale.
    pub text_max_width_em: Option<f32>,
    /// Multiplier on the icon's native LOGICAL size (Mapbox's
    /// `"icon-size"`; `1.0` when the layer doesn't set one) -- the sprite
    /// sheet's own pixel dimensions divided by its declared `pixelRatio`,
    /// since this crate fetches the `@2x` sheet. See
    /// `map_view::icon_logical_size`, which applies both.
    ///
    /// A BRACKET, not one value, for the same reason `PaintOp`'s
    /// `color_from`/`color_to` is one: the plan is baked once per integer
    /// zoom bucket but the camera moves continuously inside it, and Mapbox
    /// sizes a symbol at the map's own fractional zoom every frame (see
    /// `LayoutValue::resolve_pair`). `icon_size_from` is the value at the
    /// zoom the plan was baked at and `icon_size_to` the value one zoom up;
    /// the draw loop walks between them with
    /// `map_view::blended_icon_size`. For a constant or `step` `icon-size`
    /// the two are equal and the blend is a no-op.
    pub icon_size_from: f32,
    /// The far end of `icon_size_from`'s bracket -- the value one zoom up.
    pub icon_size_to: f32,
    pub text_color: Rgba,
    pub text_halo_color: Rgba,
    pub text_halo_width: f32,
    /// `paint.text-halo-blur`, resolved -- the width of the fade at the
    /// halo's outer edge, in the same points `text_halo_width` is in. `0` is
    /// a hard edge.
    pub text_halo_blur: f32,
    /// This layer's `layout.text-font`, copied verbatim -- the property is
    /// read as a literal list of face names, so there is nothing to resolve
    /// against this feature's tags. Carried on the feature for the same
    /// reason `text_anchor` is: the draw loop merges symbols from many
    /// tiles' plans and has no `StyleLayer` in hand when it shapes one.
    ///
    /// An `Arc`, so a plan holding thousands of symbols from one layer
    /// holds one copy of its stack.
    pub text_font: crate::font_fallback::FontStack,
    /// This layer's `paint.text-opacity` resolved at the bake zoom -- a
    /// multiplier applied to `text_color`'s and `text_halo_color`'s alpha
    /// when the label is drawn.
    ///
    /// `0.0` means the label does not draw at all. It still gets a
    /// collision box and still competes for a placement slot, because that
    /// is what mapbox-gl-js does: `placement.ts` never reads `text-opacity`
    /// (the only `paint` property it touches is `symbol-z-offset`), so an
    /// invisible label evicts a visible one there too, and reproducing that
    /// is what puts the labels that DO draw in the reference's positions.
    pub text_opacity: f32,
    /// `paint.icon-opacity` resolved at the bake zoom, the sprite half's
    /// independent twin of `text_opacity`. `0.0` means the icon does not
    /// draw; the collision box stays, for the same reason.
    pub icon_opacity: f32,
    /// `icon-color`/`icon-halo-color`/`icon-halo-width`, resolved. Used
    /// ONLY when the resolved `icon`'s sprite is an SDF one; a plain sprite
    /// draws its own pixels and ignores all three. See
    /// `render::icon_pipeline::SdfPaint`.
    pub icon_color: Rgba,
    pub icon_halo_color: Rgba,
    pub icon_halo_width: f32,
    /// Index of the `StyleLayer` in the compiled `Style` this symbol was
    /// resolved from -- the PRIMARY collision-placement priority. Baked in
    /// here rather than recovered later because the symbol overlay loop
    /// merges features from many tiles' plans into one placement pass and
    /// has no other way back to the layer they came from.
    pub layer_index: usize,
    /// This layer's `symbol-sort-key` resolved against this feature's tags
    /// (`0.0` when the layer declares none), the within-layer collision
    /// priority. LOWER places first -- Mapbox's semantics.
    pub sort_key: f32,
    /// For a line-placed symbol, a point further along the line in the
    /// geometry's own direction; `None` for a point symbol. The draw loop
    /// projects it alongside `anchor` and takes `atan2` of the difference,
    /// so the rendered angle stays correct under camera rotation and pitch.
    pub forward_ref: Option<geo_types::Point<i32>>,
    /// For a line-placed symbol, the polyline this anchor was placed on --
    /// tile-local, already clipped, in the same 0..4096 space as `anchor`.
    /// `None` for point placement, which has no line and must not pay for
    /// one.
    ///
    /// What `forward_ref` cannot supply. A forward reference gives ONE
    /// direction, at the anchor; a label that follows its street needs the
    /// geometry either side of the anchor, because each glyph takes the
    /// direction of the line beneath IT. That is what
    /// `symbol_line::place_glyphs_along` walks, and it takes a
    /// `LineString`.
    ///
    /// `Arc` and not a `LineString` because line placement emits one
    /// `SymbolFeature` PER ANCHOR along one line: a plain `LineString`
    /// would clone the whole polyline once per anchor, and a road carrying
    /// a dozen anchors would carry a dozen copies of itself. Every anchor
    /// walked on the same part shares one allocation, which
    /// `SymbolPlan::footprint_bytes` relies on -- it weighs each distinct
    /// allocation once.
    ///
    /// For a `MultiLineString` feature this is the PART the anchor sits on,
    /// not the whole geometry: `symbol-spacing` is measured across the
    /// whole feature but a label bends along one polyline. See
    /// `symbol_line::PartAnchor`, which is how the walk reports it.
    pub line: Option<std::sync::Arc<geo_types::LineString<i32>>>,
    /// How far along [`Self::line`] this anchor sits, in tile units --
    /// `symbol_line::place_glyphs_along`'s `centre_distance`, and
    /// meaningless (and `0.0`) whenever `line` is `None`.
    ///
    /// Carried rather than recomputed for the reason
    /// `symbol_line::PartAnchor::centre_distance` spells out: the anchor
    /// walk has the number for free, and recovering it from `anchor` alone
    /// is an O(n) re-walk of the part with no unique answer on a part that
    /// doubles back.
    pub line_centre_distance: f64,
    /// This layer's `icon-rotate`, in degrees clockwise, added to the
    /// direction derived from `forward_ref`.
    pub icon_rotate_deg: f32,
    /// This layer set both `icon-allow-overlap` and `icon-ignore-placement`:
    /// draw regardless of collision, and do not block other symbols.
    pub bypass_collision: bool,
}

/// Every `SymbolFeature` resolved for one tile at one zoom -- the
/// symbol-layer analog of `PaintPlan`. Baked at the camera's DISPLAY zoom
/// (`MapView::display_zoom()`), not `tile.z`, and rebaked lazily -- on the
/// read that needs it, via `MapView::symbol_plan_for` -- once that display
/// zoom has crossed an integer boundary since it was last baked; see
/// `match_tile`'s doc comment, which `match_tile_symbols` mirrors exactly
/// on this point.
/// Consumed by the symbol overlay draw loop at the end of `MapView::paint`
/// (`map_view.rs`), which projects each feature's `anchor` to a screen
/// pixel and paints its icon/text through gpui's own scene rather than the
/// wgpu surface.
#[derive(Debug, Clone, Default)]
pub struct SymbolPlan {
    pub features: Vec<SymbolFeature>,
}

impl SymbolPlan {
    /// Every heap block this plan holds, in
    /// [`crate::render::heap_block_bytes`]' unit -- the weight
    /// `MapView`'s `symbol_cache` is bounded by.
    ///
    /// The one per-tile cache that did NOT have a memory problem, and this
    /// exists so that can be asserted rather than assumed. Measured at
    /// **0.014 MB** on the dense Tokyo z14 tile against 11.81 MB decoded
    /// and 65.0 MB of triangulation -- a 512-entry bound on it was a 7 MB
    /// latent ceiling, three orders of magnitude below every other cache's
    /// and never the reason for an OS kill. It gets a budget anyway,
    /// because a cache whose weight is never computed is a cache whose
    /// weight nobody would notice changing.
    ///
    /// `text_font` is excluded on purpose. It is an
    /// `Arc<[String]>` built once per symbol LAYER and cloned onto every
    /// feature that layer produces, so charging it per feature would
    /// report a shared font stack once for each of a few thousand labels
    /// -- the one place in these walks where counting a clone would be
    /// wrong by a large factor rather than by the deliberate 1.76 of
    /// [`PaintPlan::footprint_bytes`]. Everything else on a
    /// `SymbolFeature` is by value.
    ///
    /// `line` is the one exception to "by value", and it is counted ONCE
    /// PER DISTINCT ALLOCATION rather than once per feature. Line placement
    /// emits one `SymbolFeature` per anchor and every anchor walked on the
    /// same polyline shares one `Arc` (see `SymbolFeature::line`), so
    /// charging it per feature would report a road's geometry once for each
    /// of its labels -- inflating this plan's weight by the anchor count
    /// and making the byte-budgeted `symbol_cache` evict early for memory
    /// nothing is holding. That is the defect
    /// `memory_ceiling.accounted_ratio` exists to catch, so the dedup is
    /// by `Arc` pointer, which is the identity of the allocation itself.
    #[must_use]
    pub fn footprint_bytes(&self) -> usize {
        use crate::render::heap_block_bytes as block;
        let mut bytes = crate::render::vec_block_bytes::<SymbolFeature>(self.features.capacity());
        // Addresses only, never dereferenced -- `Arc::as_ptr` is the
        // cheapest stable identity for "the same allocation", and the
        // `Arc`s outlive this call because `self` borrows them.
        let mut seen_lines: std::collections::HashSet<*const geo_types::LineString<i32>> = std::collections::HashSet::new();
        for feature in &self.features {
            bytes += feature.text.as_ref().map_or(0, |t| block(t.capacity()));
            bytes += feature.icon.as_ref().map_or(0, |i| block(i.capacity()));
            if let Some(line) = &feature.line {
                if seen_lines.insert(std::sync::Arc::as_ptr(line)) {
                    // The `ArcInner` block -- two atomic counters plus the
                    // `LineString` itself, which is one `Vec` header -- and
                    // then that `Vec`'s own coordinate block.
                    bytes += block(2 * std::mem::size_of::<usize>() + std::mem::size_of::<geo_types::LineString<i32>>());
                    bytes += crate::render::vec_block_bytes::<geo_types::Coord<i32>>(line.0.capacity());
                }
            }
        }
        // The dedup set is a live allocation for the duration of this call
        // only, so it is deliberately not charged: nothing the cache holds
        // pays for it.
        bytes
    }
}

/// Everything a LINE-placed candidate needs to become a `SymbolFeature`,
/// resolved once against its ORIGINATING feature's tags and carried through
/// `symbol_line::merge_line_candidates` and `symbol_line::anchor_too_close`
/// untouched.
///
/// A merge picks a survivor and discards the other side's payload rather
/// than blending the two (see `merge_line_candidates`'s own doc comment) --
/// exactly what MapLibre's `mergeLines` does, and the same bet: adjacent
/// blocks of one named road overwhelmingly share the same class, size and
/// colour, because they are the same road under one style layer.
#[derive(Clone)]
struct LineSymbolPayload {
    icon: Option<String>,
    text_size: f32,
    text_offset: (f32, f32),
    text_anchor: TextAnchor,
    icon_size_from: f32,
    icon_size_to: f32,
    text_color: Rgba,
    text_halo_color: Rgba,
    text_halo_width: f32,
    text_halo_blur: f32,
    text_font: crate::font_fallback::FontStack,
    text_opacity: f32,
    icon_opacity: f32,
    icon_color: Rgba,
    icon_halo_color: Rgba,
    icon_halo_width: f32,
    /// This layer's `symbol-sort-key` resolved against the ORIGINATING
    /// feature's tags -- see `LineSymbolPayload`'s own doc comment for why a
    /// merge does not attempt to reconcile two different resolved values.
    sort_key: f32,
    icon_rotate_deg: f32,
    bypass_collision: bool,
    /// This layer's `symbol-spacing`, already converted to tile units
    /// (`spacing_px.resolve(..) * tile_units_per_px`). Halved, it is also
    /// `anchor_too_close`'s `repeat_distance` -- MapLibre's own
    /// `textRepeatDistance = symbolMinDistance / 2`.
    spacing_tile_units: f64,
}

/// Resolves every `StyleLayer::Symbol` in `style` against `tile`'s features
/// at `zoom`, mirroring `match_tile`'s structure (same zoom-range gating via
/// `layer_zoom_range`, same per-layer `source_layer` lookup, same
/// `Filter::matches` per-feature test). A layer's `placement` selects which
/// geometry it consumes: `SymbolPlacement::Point` reads `Point`/`MultiPoint`
/// features AND line features (see below), and `SymbolPlacement::Line` reads
/// `LineString`/`MultiLineString` features (a point feature is skipped) -- a
/// mismatch never panics, it just produces no symbol for that feature.
///
/// Point placement over LINE geometry anchors at the first vertex of each
/// part, following MapLibre's `symbol_layout.ts` (`addFeature`'s
/// `feature.type === 'LineString'` branch, which cites mapbox-gl-js#3808).
/// This is not a corner case: a shield layer written the conventional way,
/// `"symbol-placement": ["step", ["zoom"], "point", 11, "line"]`, spends
/// every zoom below the step in point placement, and skipping line geometry
/// there drew nothing at all with no warning. `line-center` -- the placement
/// value that does mean the midpoint -- remains unimplemented and distinct.
/// POLYGON geometry under point placement is still skipped; MapLibre labels
/// it at the pole of inaccessibility, which this crate has no equivalent of
/// yet.
///
/// For a line-placed feature, the WHOLE feature is
/// walked in one pass by `crate::symbol_line::line_anchors_across`, at a
/// spacing of the layer's `spacing_px` converted to tile units with
/// `tile_units_per_px` (see `symbol_line::tile_units_per_px`) -- the
/// pixel-to-tile-unit conversion for `symbol-spacing`, since anchors are
/// placed in the tile's own integer coordinate space.
///
/// "The whole feature" is load-bearing and used not to be. Each part of a
/// `MultiLineString` used to be walked independently, which meant a road
/// arriving as hundreds of short parts -- the normal shape for OSM-derived
/// tiles, where a way is split at every junction and attribute change --
/// got the short-line midpoint anchor once per part, and `symbol-spacing`
/// thinned nothing at all. One accumulator across the parts is what makes
/// the property mean what the spec says it means. See
/// `symbol_line::line_anchors_across`.
///
/// `MultiPoint` matters as much as `Point` here, and is in fact the *only*
/// point shape real tiles produce: MVT's wire format has one `POINT`
/// geometry type that can carry any number of points, and `mvt_reader` (see
/// `mvt::decode`) therefore hands back every point feature as a
/// `MultiPoint`, even the overwhelmingly common single-point case. Matching
/// only `Point` meant no real tile feature ever produced a symbol at all --
/// every one of Mapbox Streets' `housenum_label`/`place_label`/`structure`
/// point features fell through the skip. Each point of a multipoint gets its
/// own symbol, which is also what real Mapbox GL does.
///
/// A point-placed feature is also skipped if both `text_field` and
/// `icon_image` resolve to `None` (e.g. a `text-field` `["get", "name_en"]`
/// on a feature with no `name_en` tag and no `icon_image` configured at
/// all). A line-placed feature resolves BOTH `icon_image` and `text_field`,
/// and is skipped only when both come back `None` -- the same rule point
/// placement uses.
///
/// `text_rotation_alignment` no longer gates that resolve. It used to: a
/// `Map`-aligned line-placed `text_field` resolved to `None` outright (and
/// then, for a while, only behind a `MAP_LINE_TEXT=1` environment variable),
/// on the reasoning that a horizontal label repeated every `spacing_px`
/// along a road is "worse than not drawing it". That reasoning does not
/// survive being looked at. It cost the engine EVERY street name in EVERY
/// style that follows the OpenMapTiles convention -- Liberty's
/// `highway-name-major`/`-minor`/`-path` all set `text-rotation-alignment:
/// "map"`, as does Mapbox Streets' `road-label` family -- and because
/// `symbol-placement: line` is how a name-only road layer is written no
/// matter what script the names are in, the visible symptom was a map of
/// Tokyo, Seoul, Cairo or Athens with no road labels at all. It read as a
/// font or a script-coverage bug and was neither.
///
/// With it resolved, Singapore's MacRitchie trails -- winding paths, close
/// to the worst case -- draw legible, sensibly spaced names, and collision
/// keeps each one to one or two placements rather than a repeat at every
/// anchor. The labels now follow the curve too: `symbol_pass` draws a
/// line-placed label as one rotated quad per glyph out of
/// `render::GlyphAtlas`, each glyph carrying the direction of the line
/// beneath it (`symbol_line::place_glyphs_along`), and it is THERE that
/// `text_rotation_alignment` is finally read. Resolving both alignments
/// here is still right: the property decides how the label is DRAWN, not
/// whether its text exists.
///
/// The `Viewport` case is unchanged and was never in question: a style sets
/// `text-rotation-alignment: "viewport"` (as `road-number-shield` does)
/// precisely to say its text is a horizontal label at the anchor, so
/// resolving it is the whole point of the layer -- a route shield with no
/// route number is an empty plate. Icon-only line placement (arrows) has no
/// alignment gate either: an icon is a single sprite with no internal layout
/// to bend.
///
/// Each feature also records the collision-placement priority the symbol
/// overlay will sort on: `layer_index` (this layer's position in
/// `style.layers`, hence the `enumerate` below) and `sort_key` (the layer's
/// `symbol-sort-key` resolved against this feature's tags). Both are baked
/// here for the same reason every other symbol property is -- by the time
/// the overlay loop runs it has merged many tiles' plans into one list and
/// can no longer see the layer any given feature came from.
///
/// A point anchor outside the tile's own `[0, TILE_EXTENT)` range is also
/// skipped. Real Mapbox vector tiles buffer point features slightly beyond
/// this nominal range so a tile renders cleanly in isolation, but that also
/// means a point near a tile boundary can be present in *two* adjacent
/// tiles' decoded data: its real position in one tile, plus a buffered
/// duplicate in the neighbor's overlap region. Without this check, both
/// copies got turned into symbols and drawn at very slightly different
/// projected screen positions (each tile has its own transform matrix),
/// producing an illegible, overlapping "double" label. This was confirmed
/// live, not hypothetical: a "Pulau Padang" label at a river confluence in
/// Malaysia rendered as garbled overlapping text before this filter. Line
/// anchors are not subject to this skip: they come from geometry already
/// clipped to the tile, and `line_anchors`' half-spacing start already keeps
/// them off the boundary.
///
/// A LINE-placed, TEXTED feature is not emitted the moment its own anchors
/// are walked. It is collected into a per-layer `Vec` first, and only once
/// every feature this layer matches in this tile has contributed does that
/// `Vec` go through `symbol_line::merge_line_candidates` (stitching
/// same-text candidates whose endpoints touch -- MapLibre's `mergeLines` --
/// into one longer polyline before ANY anchor is walked on them) and then
/// `symbol_line::anchor_too_close` (refusing a same-text anchor that lands
/// within half a spacing of one already accepted in this layer -- MapLibre's
/// `anchorIsTooClose`). Both are ACROSS-feature operations by nature -- a
/// named road is one vector-tile feature per OSM way, one per city block,
/// and a dual carriageway is two separate, parallel ways sharing a name --
/// so neither can run inside the single-feature loop the icon-only branch
/// still uses. See `symbol_line`'s module doc comment for why gpui-map had
/// neither before, and `LineSymbolPayload` for what travels from a
/// collected candidate to the `SymbolFeature`s it eventually becomes.
pub fn match_tile_symbols(tile: &DecodedTile, style: &Style, zoom: u8, tile_units_per_px: f64) -> SymbolPlan {
    let mut features = Vec::new();
    // Shared by every symbol layer on a source layer; see `columnar`.
    let mut eval = TileEval::new(tile, zoom as f64);
    for (layer_index, layer) in style.layers.iter().enumerate() {
        let (min_zoom, max_zoom) = layer_zoom_range(layer);
        if min_zoom.is_some_and(|mz| zoom < mz) || max_zoom.is_some_and(|mz| zoom > mz) {
            continue;
        }
        let StyleLayer::Symbol {
            source_layer,
            filter,
            text_field,
            icon_image,
            text_size,
            text_offset_x,
            text_offset_y,
            text_radial_offset,
            text_anchor,
            text_max_width,
            icon_size,
            symbol_sort_key,
            text_color,
            text_halo_color,
            text_halo_width,
            text_halo_blur,
            text_font,
            text_opacity,
            icon_opacity,
            icon_color,
            icon_halo_color,
            icon_halo_width,
            placement,
            spacing_px,
            icon_rotate_deg,
            bypass_collision,
            ..
        } = layer
        else {
            continue;
        };
        let placement = placement.at(zoom);
        // `text_rotation_alignment` is deliberately NOT destructured here.
        // It is still compiled onto the layer and is what per-glyph
        // curve-following placement will read, but nothing on the bake path
        // branches on it any more: both alignments draw a horizontal label
        // at the anchor today, and branching on it only ever deleted labels.
        // See this function's doc comment.
        let Some((tile_layer, matched)) = eval.matching(source_layer, filter) else {
            continue;
        };
        // Collected by every TEXTED, single-part line-placed feature this
        // layer matches in this tile, and only turned into `SymbolFeature`s
        // AFTER the loop below finishes -- see this function's doc comment
        // and `LineSymbolPayload`. An icon-only line feature never goes in
        // here (it is emitted directly, inside the loop, same as before).
        let mut line_candidates: Vec<crate::symbol_line::LineMergeCandidate<LineSymbolPayload>> = Vec::new();
        // Every anchor accepted for a given text, in THIS layer, so far --
        // shared by the already-multi-part branch below (which emits
        // immediately, inline) and phase two's merged single-part
        // candidates (which emit after the loop). `anchor_too_close` must
        // see both: MapLibre's own `anchorIsTooClose` runs once per bucket
        // (layer) across EVERY anchor the layer places, not once per
        // sub-population of its features, and a real dual carriageway is
        // routinely two MULTI-PART features already merged upstream (a
        // tile source that pre-joins same-named ways, as OpenFreeMap's
        // `transportation_name` does) -- exactly the shape excluded from
        // `line_candidates` above. Keying dedup on parts.len() would refuse
        // the very case it exists for.
        let mut accepted_by_text: HashMap<String, Vec<geo_types::Point<i32>>> = HashMap::new();
        for feature in matched.iter().map(|i| &tile_layer.features[i]) {
            match placement {
                SymbolPlacement::Point => {
                    // Holds the anchors derived from LINE geometry, which
                    // are computed rather than borrowed. Declared here so
                    // the borrow below outlives the `match`; never read on
                    // the point-geometry arms, which borrow the tile's own
                    // coordinates directly and allocate nothing.
                    let line_starts: Vec<geo_types::Point<i32>>;
                    let anchors: &[geo_types::Point<i32>] = match &feature.geometry {
                        geo_types::Geometry::Point(point) => std::slice::from_ref(point),
                        geo_types::Geometry::MultiPoint(points) => &points.0,
                        // Point placement on LINE geometry is a real case,
                        // not a mismatch to skip. Liberty's shield layers
                        // (`highway-shield-non-us`, `road_shield_us`) write
                        // `"symbol-placement": ["step",
                        // ["zoom"], "point", 11, "line"]` -- one shield at
                        // low zoom, a repeating run once the road is long
                        // enough on screen -- so dropping the point half
                        // meant those layers drew NOTHING below the step
                        // zoom, silently.
                        //
                        // The anchor is the FIRST VERTEX of each part, which
                        // is what MapLibre does: `symbol_layout.ts`'s
                        // `addFeature`, in the `feature.type === 'LineString'`
                        // branch that follows the `line`/`line-center` ones,
                        // places `new Anchor(subdividedLine[0].x,
                        // subdividedLine[0].y, 0)` per part (the branch
                        // carries a link to mapbox-gl-js#3808, which is where
                        // the behaviour was settled). Not the centroid and
                        // not the midpoint: `line-center` is the separate
                        // placement value that means the midpoint, and using
                        // it here would make `point` and `line-center`
                        // indistinguishable.
                        //
                        // Each part contributes its own anchor, again as
                        // MapLibre does. The `[0, TILE_EXTENT)` skip below
                        // and collision then thin them, which is the same
                        // pair of mechanisms MapLibre relies on
                        // (`addSymbolAtAnchor` applies the identical extent
                        // test to these anchors).
                        geo_types::Geometry::LineString(line) => {
                            line_starts = line.points().next().into_iter().collect();
                            &line_starts
                        }
                        geo_types::Geometry::MultiLineString(lines) => {
                            line_starts = lines.iter().filter_map(|line| line.points().next()).collect();
                            &line_starts
                        }
                        _ => continue,
                    };
                    if anchors.is_empty() {
                        continue;
                    }
                    let text = text_field.as_ref().and_then(|tf| tf.resolve_at(&feature.tags, zoom));
                    // `.filter(|name| !name.is_empty())`: a style says "no
                    // icon HERE" by resolving `icon-image` to the empty
                    // string -- `["step", ["zoom"], "circle_11_black", 10,
                    // ""]` in `examples/liberty-style.json` is the idiom, and
                    // it means the dot disappears above z10. Carried through
                    // as `Some("")` it became an atlas lookup that missed on
                    // every such feature on every frame, and showed up in
                    // `label_census::missing_icons` as a sprite the sheet
                    // "does not carry" -- which no sheet ever could.
                    let icon = icon_image
                        .as_ref()
                        .and_then(|ii| ii.resolve(&feature.tags, zoom))
                        .filter(|name| !name.is_empty());
                    if text.is_none() && icon.is_none() {
                        continue;
                    }
                    // Resolved once for the whole feature, not once per
                    // anchor: the pair depends only on the feature's tags
                    // and the bake zoom, and a multipoint can carry many
                    // anchors.
                    let (icon_size_from, icon_size_to) = icon_size.resolve_pair(&feature.tags, zoom);
                    for anchor in anchors {
                        // Skip anchors outside the tile's own nominal extent: real
                        // tiles buffer point features slightly beyond this range so
                        // a near-boundary point can appear in two adjacent tiles'
                        // data, and drawing both copies produces an overlapping,
                        // garbled label. See this function's doc comment.
                        if anchor.x() < 0 || anchor.x() >= TILE_EXTENT || anchor.y() < 0 || anchor.y() >= TILE_EXTENT {
                            continue;
                        }
                        features.push(SymbolFeature {
                            anchor: *anchor,
                            text: text.clone(),
                            icon: icon.clone(),
                            text_size: text_size.resolve(&feature.tags, zoom),
                            text_offset: point_text_offset(
                                text_anchor.resolve(&feature.tags, zoom),
                                text_radial_offset.as_ref().map_or(0.0, |r| r.resolve(&feature.tags, zoom)),
                                (
                                    text_offset_x.resolve(&feature.tags, zoom),
                                    text_offset_y.resolve(&feature.tags, zoom),
                                ),
                            ),
                            text_anchor: text_anchor.resolve(&feature.tags, zoom),
                            text_max_width_em: Some(*text_max_width),
                            icon_size_from,
                            icon_size_to,
                            text_color: text_color.value_at(zoom),
                            text_halo_color: text_halo_color.value_at(zoom),
                            text_halo_width: text_halo_width.resolve(&feature.tags, zoom),
                            text_halo_blur: text_halo_blur.resolve(&feature.tags, zoom),
                            text_font: text_font.clone(),
                            text_opacity: text_opacity.resolve(&feature.tags, zoom),
                            icon_opacity: icon_opacity.resolve(&feature.tags, zoom),
                            icon_color: icon_color.value_at(zoom),
                            icon_halo_color: icon_halo_color.value_at(zoom),
                            icon_halo_width: icon_halo_width.resolve(&feature.tags, zoom),
                            layer_index,
                            sort_key: symbol_sort_key.as_ref().map_or(0.0, |k| k.resolve(&feature.tags)),
                            forward_ref: None,
                            line: None,
                            line_centre_distance: 0.0,
                            icon_rotate_deg: *icon_rotate_deg,
                            bypass_collision: *bypass_collision,
                        });
                    }
                }
                SymbolPlacement::Line | SymbolPlacement::LineCenter => {
                    // Line placement resolves ICON and TEXT alike, whatever
                    // `text_rotation_alignment` says -- see this function's
                    // doc comment for why the `Map` case is no longer a
                    // silent drop.
                    // `.filter(|name| !name.is_empty())`: a style says "no
                    // icon HERE" by resolving `icon-image` to the empty
                    // string -- `["step", ["zoom"], "circle_11_black", 10,
                    // ""]` in `examples/liberty-style.json` is the idiom, and
                    // it means the dot disappears above z10. Carried through
                    // as `Some("")` it became an atlas lookup that missed on
                    // every such feature on every frame, and showed up in
                    // `label_census::missing_icons` as a sprite the sheet
                    // "does not carry" -- which no sheet ever could.
                    let icon = icon_image
                        .as_ref()
                        .and_then(|ii| ii.resolve(&feature.tags, zoom))
                        .filter(|name| !name.is_empty());
                    let text = text_field.as_ref().and_then(|tf| tf.resolve_at(&feature.tags, zoom));
                    // A line-placed feature needs SOMETHING to draw. Icon-only
                    // stays the common case (arrows); text-only is a street
                    // name.
                    if icon.is_none() && text.is_none() {
                        continue;
                    }
                    let (icon_size_from, icon_size_to) = icon_size.resolve_pair(&feature.tags, zoom);
                    let spacing_tile_units = spacing_px.resolve(&feature.tags, zoom) as f64 * tile_units_per_px;
                    let parts: &[geo_types::LineString<i32>] = match &feature.geometry {
                        geo_types::Geometry::LineString(line) => std::slice::from_ref(line),
                        geo_types::Geometry::MultiLineString(lines) => &lines.0,
                        _ => continue,
                    };
                    let payload = LineSymbolPayload {
                        icon,
                        text_size: text_size.resolve(&feature.tags, zoom),
                        text_offset: (text_offset_x.resolve(&feature.tags, zoom), text_offset_y.resolve(&feature.tags, zoom)),
                        text_anchor: text_anchor.resolve(&feature.tags, zoom),
                        icon_size_from,
                        icon_size_to,
                        text_color: text_color.value_at(zoom),
                        text_halo_color: text_halo_color.value_at(zoom),
                        text_halo_width: text_halo_width.resolve(&feature.tags, zoom),
                        text_halo_blur: text_halo_blur.resolve(&feature.tags, zoom),
                        text_font: text_font.clone(),
                        text_opacity: text_opacity.resolve(&feature.tags, zoom),
                        icon_opacity: icon_opacity.resolve(&feature.tags, zoom),
                        icon_color: icon_color.value_at(zoom),
                        icon_halo_color: icon_halo_color.value_at(zoom),
                        icon_halo_width: icon_halo_width.resolve(&feature.tags, zoom),
                        sort_key: symbol_sort_key.as_ref().map_or(0.0, |k| k.resolve(&feature.tags)),
                        icon_rotate_deg: *icon_rotate_deg,
                        bypass_collision: *bypass_collision,
                        spacing_tile_units,
                    };
                    // `line-center`: ONE symbol per line part, anchored at
                    // half the part's length -- MapLibre `symbol_layout.ts`
                    // `addFeature`'s `line-center` branch, which calls
                    // `getCenterAnchor` once per line and never merges lines
                    // (`mergeLines` is for `line` only) nor repeats at
                    // `symbol-spacing`. An infinite spacing makes the shared
                    // walk yield exactly its one midpoint anchor, so the
                    // anchor, its forward reference and its centre distance
                    // come from the same code the `line` placement uses.
                    // An anchor outside the tile's own extent is dropped, as
                    // MapLibre's `addSymbolAtAnchor` does -- the buffered
                    // copy of a line in the neighbouring tile would
                    // otherwise place a second label.
                    if placement == SymbolPlacement::LineCenter {
                        for part in parts {
                            for placed in
                                crate::symbol_line::part_anchors_across(std::slice::from_ref(part), f64::INFINITY)
                            {
                                let anchor = placed.anchor;
                                let at = anchor.at;
                                if at.x() < 0 || at.x() >= TILE_EXTENT || at.y() < 0 || at.y() >= TILE_EXTENT {
                                    continue;
                                }
                                features.push(SymbolFeature {
                                    anchor: at,
                                    text: text.clone(),
                                    icon: payload.icon.clone(),
                                    text_size: payload.text_size,
                                    text_offset: payload.text_offset,
                                    text_anchor: payload.text_anchor,
                                    text_max_width_em: None,
                                    icon_size_from: payload.icon_size_from,
                                    icon_size_to: payload.icon_size_to,
                                    text_color: payload.text_color,
                                    text_halo_color: payload.text_halo_color,
                                    text_halo_width: payload.text_halo_width,
                                    text_halo_blur: payload.text_halo_blur,
                                    text_font: payload.text_font.clone(),
                                    text_opacity: payload.text_opacity,
                                    icon_opacity: payload.icon_opacity,
                                    icon_color: payload.icon_color,
                                    icon_halo_color: payload.icon_halo_color,
                                    icon_halo_width: payload.icon_halo_width,
                                    layer_index,
                                    sort_key: payload.sort_key,
                                    forward_ref: Some(anchor.forward),
                                    line: Some(std::sync::Arc::new(part.clone())),
                                    line_centre_distance: placed.centre_distance,
                                    icon_rotate_deg: payload.icon_rotate_deg,
                                    bypass_collision: payload.bypass_collision,
                                });
                            }
                        }
                        continue;
                    }
                    match text {
                        None => {
                            // No text to merge or dedup on: an icon-only line
                            // (a one-way arrow) repeats along its OWN feature
                            // via `symbol-spacing`, unchanged from before.
                            // One walk over the WHOLE feature, not one per
                            // part -- `part_anchors_across` carries the
                            // spacing accumulator between parts, which is
                            // what makes `symbol-spacing` mean anything for
                            // the `MultiLineString` features real tiles are
                            // full of. See that function's doc comment.
                            let anchors = crate::symbol_line::part_anchors_across(parts, spacing_tile_units);
                            // One `Arc` per part that actually carries an
                            // anchor, cloned (refcount only) onto each of
                            // that part's anchors.
                            let mut part_lines: Vec<Option<std::sync::Arc<geo_types::LineString<i32>>>> =
                                vec![None; parts.len()];
                            for placed in &anchors {
                                let anchor = &placed.anchor;
                                let line = part_lines[placed.part]
                                    .get_or_insert_with(|| std::sync::Arc::new(parts[placed.part].clone()))
                                    .clone();
                                features.push(SymbolFeature {
                                    anchor: anchor.at,
                                    text: None,
                                    icon: payload.icon.clone(),
                                    text_size: payload.text_size,
                                    text_offset: payload.text_offset,
                                    text_anchor: payload.text_anchor,
                                    // `None`, not the layer's width: `text-max-width` requires
                                    // `symbol-placement: point`. See `SymbolFeature::text_max_width_em`.
                                    text_max_width_em: None,
                                    icon_size_from: payload.icon_size_from,
                                    icon_size_to: payload.icon_size_to,
                                    text_color: payload.text_color,
                                    text_halo_color: payload.text_halo_color,
                                    text_halo_width: payload.text_halo_width,
                                    text_halo_blur: payload.text_halo_blur,
                                    text_font: payload.text_font.clone(),
                                    text_opacity: payload.text_opacity,
                                    icon_opacity: payload.icon_opacity,
                                    icon_color: payload.icon_color,
                                    icon_halo_color: payload.icon_halo_color,
                                    icon_halo_width: payload.icon_halo_width,
                                    layer_index,
                                    sort_key: payload.sort_key,
                                    forward_ref: Some(anchor.forward),
                                    line: Some(line),
                                    line_centre_distance: placed.centre_distance,
                                    icon_rotate_deg: payload.icon_rotate_deg,
                                    bypass_collision: payload.bypass_collision,
                                });
                            }
                        }
                        Some(text_value) if parts.len() == 1 => {
                            // NOT emitted yet: collected across every
                            // single-part feature this layer matches in
                            // this tile, then merged and anchor-walked once
                            // the loop over `tile_layer.features` finishes.
                            // See this function's doc comment.
                            //
                            // Single-part only -- see the `parts.len() > 1`
                            // arm below for why a genuine `MultiLineString`
                            // feature does not go through this collection.
                            line_candidates.push(crate::symbol_line::LineMergeCandidate {
                                text: text_value.clone(),
                                points: parts[0].clone(),
                                payload: payload.clone(),
                            });
                        }
                        Some(text_value) => {
                            // A feature that is ALREADY a `MultiLineString`
                            // of more than one part does NOT go through
                            // `merge_line_candidates`: splitting ITS OWN
                            // parts into independent merge candidates would
                            // lose `part_anchors_across`'s whole-feature
                            // spacing accumulator across them (see that
                            // function's doc comment on why a 439-part road
                            // needs one accumulator, not one per part)
                            // whenever the parts do not happen to share
                            // endpoints. This walk -- unchanged from before
                            // the merge/dedup step existed -- already
                            // handles that correctly on its own.
                            //
                            // It STILL goes through `anchor_too_close`,
                            // though, sharing `accepted_by_text` with every
                            // other text-bearing feature in this layer: a
                            // tile source that pre-merges same-named ways
                            // (OpenFreeMap's `transportation_name` does)
                            // hands a dual carriageway's EACH carriageway to
                            // this crate as one `MultiLineString` feature
                            // already, which is exactly the shape this arm
                            // handles -- excluding it from dedup as well as
                            // from merging would refuse the one case the
                            // dedup exists for.
                            let anchors = crate::symbol_line::part_anchors_across(parts, spacing_tile_units);
                            let repeat_distance = spacing_tile_units / 2.0;
                            let mut part_lines: Vec<Option<std::sync::Arc<geo_types::LineString<i32>>>> =
                                vec![None; parts.len()];
                            for placed in &anchors {
                                let anchor = &placed.anchor;
                                let accepted = accepted_by_text.entry(text_value.clone()).or_default();
                                if crate::symbol_line::anchor_too_close(accepted, anchor.at, repeat_distance) {
                                    continue;
                                }
                                accepted.push(anchor.at);
                                let line = part_lines[placed.part]
                                    .get_or_insert_with(|| std::sync::Arc::new(parts[placed.part].clone()))
                                    .clone();
                                features.push(SymbolFeature {
                                    anchor: anchor.at,
                                    text: Some(text_value.clone()),
                                    icon: payload.icon.clone(),
                                    text_size: payload.text_size,
                                    text_offset: payload.text_offset,
                                    text_anchor: payload.text_anchor,
                                    text_max_width_em: None,
                                    icon_size_from: payload.icon_size_from,
                                    icon_size_to: payload.icon_size_to,
                                    text_color: payload.text_color,
                                    text_halo_color: payload.text_halo_color,
                                    text_halo_width: payload.text_halo_width,
                                    text_halo_blur: payload.text_halo_blur,
                                    text_font: payload.text_font.clone(),
                                    text_opacity: payload.text_opacity,
                                    icon_opacity: payload.icon_opacity,
                                    icon_color: payload.icon_color,
                                    icon_halo_color: payload.icon_halo_color,
                                    icon_halo_width: payload.icon_halo_width,
                                    layer_index,
                                    sort_key: payload.sort_key,
                                    forward_ref: Some(anchor.forward),
                                    line: Some(line),
                                    line_centre_distance: placed.centre_distance,
                                    icon_rotate_deg: payload.icon_rotate_deg,
                                    bypass_collision: payload.bypass_collision,
                                });
                            }
                        }
                    }
                }
            }
        }
        // Phase two of line placement: `merge_line_candidates` stitches
        // same-text SINGLE-part candidates whose endpoints touch into one
        // longer polyline (MapLibre's `mergeLines` -- a named road chopped
        // into one vector-tile feature per city block would otherwise have
        // most blocks' labels refused outright as longer than the block
        // itself), and `anchor_too_close` then refuses a same-text anchor
        // that lands within half a spacing of one already accepted in THIS
        // layer (MapLibre's `anchorIsTooClose` -- what keeps a dual
        // carriageway's two separate, parallel, non-merging carriageways
        // from placing the same name twice side by side). Both run here,
        // across the whole layer's SINGLE-part candidates, because both are
        // explicitly cross-feature -- see `symbol_line`'s module doc
        // comment and this function's own. `accepted_by_text` is shared
        // with the already-multi-part arm above rather than started fresh
        // here, so a single-part carriageway and a multi-part one still
        // dedup against each other.
        if !line_candidates.is_empty() {
            let merged = crate::symbol_line::merge_line_candidates(std::mem::take(&mut line_candidates));
            for candidate in merged {
                let repeat_distance = candidate.payload.spacing_tile_units / 2.0;
                let anchors = crate::symbol_line::part_anchors_across(
                    std::slice::from_ref(&candidate.points),
                    candidate.payload.spacing_tile_units,
                );
                if anchors.is_empty() {
                    continue;
                }
                // One `Arc` for the whole merged run, shared by every anchor
                // walked on it -- the same sharing discipline the icon-only
                // branch above keeps per part, just at merged-run
                // granularity instead.
                let line = std::sync::Arc::new(candidate.points.clone());
                for placed in &anchors {
                    let anchor = &placed.anchor;
                    let accepted = accepted_by_text.entry(candidate.text.clone()).or_default();
                    if crate::symbol_line::anchor_too_close(accepted, anchor.at, repeat_distance) {
                        continue;
                    }
                    accepted.push(anchor.at);
                    features.push(SymbolFeature {
                        anchor: anchor.at,
                        text: Some(candidate.text.clone()),
                        icon: candidate.payload.icon.clone(),
                        text_size: candidate.payload.text_size,
                        text_offset: candidate.payload.text_offset,
                        text_anchor: candidate.payload.text_anchor,
                        text_max_width_em: None,
                        icon_size_from: candidate.payload.icon_size_from,
                        icon_size_to: candidate.payload.icon_size_to,
                        text_color: candidate.payload.text_color,
                        text_halo_color: candidate.payload.text_halo_color,
                        text_halo_width: candidate.payload.text_halo_width,
                        text_halo_blur: candidate.payload.text_halo_blur,
                        text_font: candidate.payload.text_font.clone(),
                        text_opacity: candidate.payload.text_opacity,
                        icon_opacity: candidate.payload.icon_opacity,
                        icon_color: candidate.payload.icon_color,
                        icon_halo_color: candidate.payload.icon_halo_color,
                        icon_halo_width: candidate.payload.icon_halo_width,
                        layer_index,
                        sort_key: candidate.payload.sort_key,
                        forward_ref: Some(anchor.forward),
                        line: Some(line.clone()),
                        line_centre_distance: placed.centre_distance,
                        icon_rotate_deg: candidate.payload.icon_rotate_deg,
                        bypass_collision: candidate.payload.bypass_collision,
                    });
                }
            }
        }
    }
    SymbolPlan { features }
}

/// Debug-only: a single style layer that matched a feature at a queried
/// point, along with that feature's tags and the layer's resolved paint
/// values at the queried zoom. See `layers_at_point`.
#[derive(Debug, Clone)]
pub struct LayerHit {
    pub layer_id: String,
    pub source_layer: String,
    pub feature_tags: HashMap<String, TagValue>,
    pub resolved_color: Rgba,
    pub resolved_opacity: f32,
}

/// Ray-casting point-in-ring test (even-odd rule), operating on tile-local
/// integer coordinates promoted to `f64` for the intersection arithmetic.
/// `ring` may be open or closed (a closing duplicate point, if present,
/// doesn't affect the result since the implicit closing edge -- last point
/// back to first -- is covered by wrapping `j` around to `n - 1`).
fn point_in_ring(point: (i32, i32), ring: &geo_types::LineString<i32>) -> bool {
    let (px, py) = (point.0 as f64, point.1 as f64);
    let coords: Vec<geo_types::Coord<i32>> = ring.coords().copied().collect();
    let n = coords.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (coords[i].x as f64, coords[i].y as f64);
        let (xj, yj) = (coords[j].x as f64, coords[j].y as f64);
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// A point is inside a `Polygon` if it's inside the exterior ring and NOT
/// inside any interior (hole) ring.
fn point_in_polygon(point: (i32, i32), polygon: &geo_types::Polygon<i32>) -> bool {
    if !point_in_ring(point, polygon.exterior()) {
        return false;
    }
    !polygon.interiors().iter().any(|interior| point_in_ring(point, interior))
}

/// Hit-tests `point` against a `Fill` feature's geometry. Only
/// `Polygon`/`MultiPolygon` are handled (matching how `append_fill` already
/// only handles these two geometry variants for fill rendering) -- any
/// other geometry variant on a Fill layer is treated as a non-hit.
fn point_in_fill_geometry(point: (i32, i32), geometry: &geo_types::Geometry<i32>) -> bool {
    match geometry {
        geo_types::Geometry::Polygon(p) => point_in_polygon(point, p),
        geo_types::Geometry::MultiPolygon(mp) => mp.iter().any(|p| point_in_polygon(point, p)),
        _ => false,
    }
}

/// Debug-only: for a decoded tile and a tile-local point (in the same
/// 0..4096 coordinate space as feature geometry), returns every style
/// layer whose filter matches a feature whose geometry contains that
/// point (Fill layers only -- Line hit-testing is out of scope, see the
/// module-level design note), at the given zoom -- along with the matched
/// feature's tags and the layer's resolved color/opacity at that zoom.
///
/// Not used by the real rendering path; exists purely to answer "what
/// SHOULD be here, and does the filter/paint pipeline agree", independent
/// of whatever ended up in a cached `PaintPlan`. Re-derives matches from
/// scratch each call rather than consulting any cache -- fine for the
/// occasional interactive debug click this is built for, not meant to run
/// per-frame.
pub fn layers_at_point(tile: &DecodedTile, style: &Style, zoom: u8, point: (i32, i32)) -> Vec<LayerHit> {
    let mut hits = Vec::new();
    for layer in &style.layers {
        let (min_zoom, max_zoom) = layer_zoom_range(layer);
        if min_zoom.is_some_and(|mz| zoom < mz) || max_zoom.is_some_and(|mz| zoom > mz) {
            continue;
        }
        let StyleLayer::Fill { id, source_layer, filter, color, opacity, .. } = layer else {
            // Background has no per-point geometry; Line hit-testing is
            // out of scope (see doc comment above).
            continue;
        };
        let Some(tile_layer) = tile.layers.iter().find(|l| &l.name == source_layer) else {
            continue;
        };
        for feature in &tile_layer.features {
            if !filter.matches_geometry(&feature.tags, zoom as f64, GeometryKind::of(&feature.geometry)) {
                continue;
            }
            if !point_in_fill_geometry(point, &feature.geometry) {
                continue;
            }
            hits.push(LayerHit {
                layer_id: id.clone(),
                source_layer: source_layer.clone(),
                feature_tags: feature.tags.to_map(),
                resolved_color: color.value_at(zoom),
                resolved_opacity: opacity.value_at(zoom),
            });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{DecodedTile, Feature, Layer, TagValue};
    use std::collections::HashMap;
    use geo_types::{Geometry, LineString, Polygon};

    /// The road shields' real case: `ref` is a NUMBER in the tile, and the
    /// style wraps it in `to-string` precisely because of that. `"95.0"` is
    /// not a route number.
    #[test]
    fn text_field_to_string_renders_a_numeric_ref_without_a_decimal_point() {
        let field = TextField::ToStringGet("ref".into());
        let mut tags = HashMap::new();
        tags.insert("ref".to_string(), TagValue::Number(95.0));
        assert_eq!(field.resolve(&tags), Some("95".to_string()));
    }

    /// The bilingual label, both ways through the `case`.
    #[test]
    fn text_field_case_picks_the_bilingual_name_only_when_the_tile_has_one() {
        let field = TextField::CaseHas {
            key: "name:nonlatin".into(),
            then: Box::new(TextField::Concat(vec![
                ConcatPart::Get("name:latin".into()),
                ConcatPart::Literal(" ".into()),
                ConcatPart::Get("name:nonlatin".into()),
            ])),
            otherwise: Box::new(TextField::Coalesce(vec!["name_en".into(), "name".into()])),
        };

        let mut bilingual = HashMap::new();
        bilingual.insert("name:latin".to_string(), TagValue::String("Bangkok".into()));
        bilingual.insert("name:nonlatin".to_string(), TagValue::String("\u{e01}\u{e23}\u{e38}\u{e07}\u{e40}\u{e17}\u{e1e}".into()));
        assert_eq!(
            field.resolve(&bilingual),
            Some("Bangkok \u{e01}\u{e23}\u{e38}\u{e07}\u{e40}\u{e17}\u{e1e}".to_string())
        );

        let mut latin_only = HashMap::new();
        latin_only.insert("name_en".to_string(), TagValue::String("Chiang Mai".into()));
        latin_only.insert("name".to_string(), TagValue::String("ignored".into()));
        assert_eq!(field.resolve(&latin_only), Some("Chiang Mai".to_string()));
    }

    /// Where a text `concat` MUST differ from a sprite-name one: half a name
    /// is still a name. A feature tagged only `name:nonlatin` passes the
    /// `has` guard, so the `concat` runs with `name:latin` missing -- and
    /// under `IconImage::Concat`'s rule (any missing part yields `None`) the
    /// label would vanish instead of showing the name the tile does have.
    #[test]
    fn text_field_concat_keeps_the_label_when_one_half_is_missing() {
        let field = TextField::Concat(vec![
            ConcatPart::Get("name:latin".into()),
            ConcatPart::Literal(" ".into()),
            ConcatPart::Get("name:nonlatin".into()),
        ]);
        let mut only_nonlatin = HashMap::new();
        only_nonlatin.insert("name:nonlatin".to_string(), TagValue::String("\u{e20}\u{e39}\u{e40}\u{e01}\u{e47}\u{e15}".into()));
        assert_eq!(field.resolve(&only_nonlatin), Some(" \u{e20}\u{e39}\u{e40}\u{e01}\u{e47}\u{e15}".to_string()));

        // But nothing at all is still nothing -- no blank labels.
        assert_eq!(field.resolve(&HashMap::new()), None);
    }

    #[test]
    fn concat_icon_name_renders_a_numeric_tag_without_a_decimal_point() {
        // reflen arrives as a NUMBER in real Mapbox tiles. "3.0" would name no
        // sprite in the atlas, and would fail exactly like a missing sprite.
        let icon = IconImage::Concat(vec![
            ConcatPart::Get("shield".into()),
            ConcatPart::Literal("-".into()),
            ConcatPart::ToStringGet("reflen".into()),
        ]);
        let mut tags = HashMap::new();
        tags.insert("shield".to_string(), TagValue::String("rectangle-blue".to_string()));
        tags.insert("reflen".to_string(), TagValue::Number(3.0));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("rectangle-blue-3"));
    }

    #[test]
    fn concat_icon_name_builds_the_motorway_exit_shape() {
        let icon = IconImage::Concat(vec![
            ConcatPart::Literal("motorway-exit-".into()),
            ConcatPart::ToStringGet("reflen".into()),
        ]);
        let mut tags = HashMap::new();
        tags.insert("reflen".to_string(), TagValue::Number(2.0));
        assert_eq!(icon.resolve(&tags, 14).as_deref(), Some("motorway-exit-2"));
    }

    #[test]
    fn concat_icon_name_resolves_to_none_when_a_part_is_missing_or_wrong_typed() {
        let icon = IconImage::Concat(vec![
            ConcatPart::Get("shield".into()),
            ConcatPart::Literal("-".into()),
            ConcatPart::ToStringGet("reflen".into()),
        ]);
        // No tags at all.
        assert_eq!(icon.resolve(&HashMap::new(), 12), None);
        // reflen present but neither a string nor a number: no name rather
        // than a wrong one.
        let mut tags = HashMap::new();
        tags.insert("shield".to_string(), TagValue::String("default".to_string()));
        tags.insert("reflen".to_string(), TagValue::Bool(true));
        assert_eq!(icon.resolve(&tags, 12), None);
    }

    #[test]
    fn concat_icon_name_passes_a_string_valued_tag_through_to_string_unchanged() {
        // `to-string` on a value that is already a string is the identity --
        // `ToStringGet` is a general `to-string` primitive, not one that
        // assumes its tag is numeric.
        let icon = IconImage::Concat(vec![
            ConcatPart::Get("shield".into()),
            ConcatPart::Literal("-".into()),
            ConcatPart::ToStringGet("reflen".into()),
        ]);
        let mut tags = HashMap::new();
        tags.insert("shield".to_string(), TagValue::String("default".to_string()));
        tags.insert("reflen".to_string(), TagValue::String("three".to_string()));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("default-three"));
    }

    #[test]
    fn concat_icon_name_rejects_a_non_integral_number() {
        // A fractional reflen could only ever name a sprite that does not exist;
        // producing None is the same degradation as a missing tag.
        let icon = IconImage::Concat(vec![ConcatPart::ToStringGet("reflen".into())]);
        let mut tags = HashMap::new();
        tags.insert("reflen".to_string(), TagValue::Number(3.5));
        assert_eq!(icon.resolve(&tags, 12), None);
    }

    /// The OpenFreeMap Liberty shields' real shape: `["concat", "road_",
    /// ["get", "ref_length"]]`, where `ref_length` arrives as a NUMBER in
    /// OpenMapTiles' `transportation_name` layer. MapLibre's `concat` takes
    /// varargs of ValueType and stringifies each one, so a bare `get` on a
    /// number is legal there and must be here too -- otherwise every shield
    /// badge in the style resolves to no sprite at all.
    #[test]
    fn concat_icon_name_stringifies_a_bare_get_on_a_numeric_tag() {
        let icon = IconImage::Concat(vec![
            ConcatPart::Literal("road_".into()),
            ConcatPart::Get("ref_length".into()),
        ]);
        let mut tags = HashMap::new();
        tags.insert("ref_length".to_string(), TagValue::Number(3.0));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("road_3"));
    }

    /// Liberty's other two shield layers: `["concat", ["get", "network"],
    /// "_", ["get", "ref_length"]]` -- one bare `get` on a string and one on
    /// a number, in the same expression.
    #[test]
    fn concat_icon_name_mixes_string_and_numeric_bare_gets() {
        let icon = IconImage::Concat(vec![
            ConcatPart::Get("network".into()),
            ConcatPart::Literal("_".into()),
            ConcatPart::Get("ref_length".into()),
        ]);
        let mut tags = HashMap::new();
        tags.insert("network".to_string(), TagValue::String("us-interstate".to_string()));
        tags.insert("ref_length".to_string(), TagValue::Number(2.0));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("us-interstate_2"));
    }

    /// A bare `get` inherits `ToStringGet`'s existing numeric rule verbatim,
    /// fractional rejection included -- `road_3.5` names no sprite either way.
    #[test]
    fn concat_icon_name_rejects_a_non_integral_number_under_a_bare_get() {
        let icon = IconImage::Concat(vec![
            ConcatPart::Literal("road_".into()),
            ConcatPart::Get("ref_length".into()),
        ]);
        let mut tags = HashMap::new();
        tags.insert("ref_length".to_string(), TagValue::Number(3.5));
        assert_eq!(icon.resolve(&tags, 12), None);
    }

    /// Stringifying numbers must not quietly turn an ABSENT property into an
    /// empty string: `road_` is a sprite name that does not exist, and the
    /// whole point of `Concat`'s `None` is to say so.
    #[test]
    fn concat_icon_name_still_resolves_to_none_when_a_bare_get_tag_is_missing() {
        let icon = IconImage::Concat(vec![
            ConcatPart::Literal("road_".into()),
            ConcatPart::Get("ref_length".into()),
        ]);
        assert_eq!(icon.resolve(&HashMap::new(), 12), None);
        // Present but neither string nor number is still no name.
        let mut wrong_typed = HashMap::new();
        wrong_typed.insert("ref_length".to_string(), TagValue::Bool(true));
        assert_eq!(icon.resolve(&wrong_typed, 12), None);
    }

    /// `text-field`'s `concat` compiles to the same `ConcatPart`s, so a bare
    /// `get` on a number stringifies there too -- with `TextField`'s own
    /// looser numeric rule (Rust `Display`, fractions allowed) rather than
    /// the sprite-name one.
    #[test]
    fn text_field_concat_stringifies_a_bare_get_on_a_numeric_tag() {
        let field = TextField::Concat(vec![
            ConcatPart::Get("ref".into()),
            ConcatPart::Literal(" exit".into()),
        ]);
        let mut tags = HashMap::new();
        tags.insert("ref".to_string(), TagValue::Number(95.0));
        assert_eq!(field.resolve(&tags).as_deref(), Some("95 exit"));
    }

    #[test]
    fn case_has_icon_takes_the_else_branch_when_the_key_is_absent() {
        // No feature in a real Mapbox tile carries shield_beta, so this is the
        // branch every shield actually takes today.
        let icon = IconImage::CaseHas {
            key: "shield_beta".into(),
            then: Box::new(IconImage::Concat(vec![ConcatPart::Literal("beta".into())])),
            otherwise: Box::new(IconImage::Concat(vec![
                ConcatPart::Get("shield".into()),
                ConcatPart::Literal("-".into()),
                ConcatPart::ToStringGet("reflen".into()),
            ])),
        };
        let mut tags = HashMap::new();
        tags.insert("shield".to_string(), TagValue::String("rectangle-blue".to_string()));
        tags.insert("reflen".to_string(), TagValue::Number(3.0));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("rectangle-blue-3"));
    }

    #[test]
    fn case_has_icon_takes_the_then_branch_when_the_key_is_present() {
        let icon = IconImage::CaseHas {
            key: "shield_beta".into(),
            then: Box::new(IconImage::Concat(vec![
                ConcatPart::Get("shield_beta".into()),
                ConcatPart::Literal("-".into()),
                ConcatPart::ToStringGet("reflen".into()),
            ])),
            otherwise: Box::new(IconImage::Concat(vec![ConcatPart::Literal("old".into())])),
        };
        let mut tags = HashMap::new();
        tags.insert("shield_beta".to_string(), TagValue::String("rectangle-green".to_string()));
        tags.insert("reflen".to_string(), TagValue::Number(2.0));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("rectangle-green-2"));
    }

    /// Liberty's `poi_transit` -- the bus-stop layer -- names its sprite
    /// with a WHOLE-EXPRESSION `["to-string", ["get", "class"]]`, not one
    /// nested inside a `concat`. `class` is a string in real OpenMapTiles
    /// tiles, where `to-string` is the identity.
    #[test]
    fn to_string_get_icon_name_passes_a_string_tag_through_unchanged() {
        let icon = IconImage::ToStringGet("class".into());
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("bus".to_string()));
        assert_eq!(icon.resolve(&tags, 14).as_deref(), Some("bus"));
    }

    /// The same numeric rule `ConcatPart::ToStringGet` already settled, not a
    /// second one: an integral number renders as `3` and never `3.0`, a
    /// fractional one could only ever name a sprite that does not exist, and
    /// a missing tag is `None` rather than an empty name.
    #[test]
    fn to_string_get_icon_name_reuses_the_concat_numeric_rule() {
        let icon = IconImage::ToStringGet("class".into());

        let mut integral = HashMap::new();
        integral.insert("class".to_string(), TagValue::Number(3.0));
        assert_eq!(icon.resolve(&integral, 14).as_deref(), Some("3"));

        let mut fractional = HashMap::new();
        fractional.insert("class".to_string(), TagValue::Number(3.5));
        assert_eq!(icon.resolve(&fractional, 14), None);

        assert_eq!(icon.resolve(&HashMap::new(), 14), None);
    }

    /// Liberty's four POI layers name their sprite with
    /// `["match", ["get","subclass"], ["florist","furniture"],
    /// ["get","subclass"], ["get","class"]]` -- "use the subclass as the
    /// sprite name for those two, otherwise the class".
    #[test]
    fn match_icon_name_takes_the_arm_whose_label_list_contains_the_tag() {
        let icon = liberty_poi_match();
        let mut florist = HashMap::new();
        florist.insert("subclass".to_string(), TagValue::String("florist".to_string()));
        florist.insert("class".to_string(), TagValue::String("shop".to_string()));
        assert_eq!(icon.resolve(&florist, 17).as_deref(), Some("florist"));

        // Every OTHER member of the same label list matches too -- a label
        // is a list of values, not one value.
        let mut furniture = HashMap::new();
        furniture.insert("subclass".to_string(), TagValue::String("furniture".to_string()));
        assert_eq!(icon.resolve(&furniture, 17).as_deref(), Some("furniture"));
    }

    #[test]
    fn match_icon_name_falls_back_when_no_label_matches() {
        let icon = liberty_poi_match();
        let mut cafe = HashMap::new();
        cafe.insert("subclass".to_string(), TagValue::String("cafe".to_string()));
        cafe.insert("class".to_string(), TagValue::String("restaurant".to_string()));
        assert_eq!(icon.resolve(&cafe, 17).as_deref(), Some("restaurant"));
    }

    /// A `get` with no tag under it is `None` -- no icon -- and never an
    /// empty sprite name, on either side of the match.
    #[test]
    fn match_icon_name_is_none_when_both_the_input_and_the_fallback_tags_are_missing() {
        assert_eq!(liberty_poi_match().resolve(&HashMap::new(), 17), None);
    }

    #[test]
    fn match_icon_name_picks_among_several_label_output_pairs() {
        let icon = IconImage::MatchGet {
            key: "class".into(),
            arms: vec![
                (vec!["bus".into(), "tram".into()], IconImage::Literal("transit".into())),
                (vec!["ferry".into()], IconImage::Literal("harbor".into())),
                (vec!["rail".into()], IconImage::Get("subclass".into())),
            ],
            fallback: Box::new(IconImage::Literal("dot".into())),
        };
        let one = |class: &str| {
            let mut tags = HashMap::new();
            tags.insert("class".to_string(), TagValue::String(class.to_string()));
            tags.insert("subclass".to_string(), TagValue::String("station".to_string()));
            icon.resolve(&tags, 17)
        };
        assert_eq!(one("bus").as_deref(), Some("transit"));
        assert_eq!(one("tram").as_deref(), Some("transit"));
        assert_eq!(one("ferry").as_deref(), Some("harbor"));
        assert_eq!(one("rail").as_deref(), Some("station"));
        assert_eq!(one("pier").as_deref(), Some("dot"));
    }

    /// Liberty's `poi_r1`/`poi_r7`/`poi_r20` `icon-image`, by hand. The
    /// compiled-from-JSON twin of this lives in `style_spec`.
    fn liberty_poi_match() -> IconImage {
        IconImage::MatchGet {
            key: "subclass".into(),
            arms: vec![(
                vec!["florist".into(), "furniture".into()],
                IconImage::Get("subclass".into()),
            )],
            fallback: Box::new(IconImage::Get("class".into())),
        }
    }

    #[test]
    fn dash_pattern_none_is_not_dashed() {
        let d = DashPattern::none();
        assert_eq!(d.count, 0);
        assert_eq!(d.period(), 0.0);
    }

    #[test]
    fn dash_pattern_keeps_even_length_runs_verbatim() {
        let d = DashPattern::from_runs(&[3.0, 3.0]);
        assert_eq!(d.count, 2);
        assert_eq!(&d.runs[..2], &[3.0, 3.0]);
        assert_eq!(d.period(), 6.0);

        let d = DashPattern::from_runs(&[2.0, 2.0, 6.0, 2.0]);
        assert_eq!(d.count, 4);
        assert_eq!(d.runs, [2.0, 2.0, 6.0, 2.0]);
        assert_eq!(d.period(), 12.0);
    }

    // An odd-length dasharray has a true period of twice its sum, with dash and
    // gap swapping roles each cycle. MapLibre's `getDashRanges` instead wraps the
    // final run onto the first -- they are necessarily the same type -- and we
    // match that rendered output. `[3,2,5]` becomes dash 8, gap 2.
    #[test]
    fn dash_pattern_folds_odd_length_runs_onto_the_first() {
        let d = DashPattern::from_runs(&[3.0, 2.0, 5.0]);
        assert_eq!(d.count, 2);
        assert_eq!(&d.runs[..2], &[8.0, 2.0]);
        assert_eq!(d.period(), 10.0);
    }

    // A boundary between two runs of the SAME type is not an edge, so it must
    // never be stored. This test used to assert the opposite -- that `[2,0]`
    // stays `count == 2` with period 2 -- on the reasoning that the phase can
    // never leave the dash, so the zero gap is harmless. That is true without
    // antialiasing and false with it: `[2,0]` stores boundaries b0 = b1 = 2w,
    // so at phase 0 (== period) the signed distance to the nearest boundary is
    // exactly 0, and `smoothstep(-0.5, 0.5, 0)` is 0.5 -- a 50%-alpha band
    // every two line widths along a line that has to be solid. Three vendored
    // layers would show it: `admin-0-boundary` `[10,0]`,
    // `admin-1-boundary-bg` `[1,0]`, and `admin-1-boundary` below z7 `[2,0]`.
    // Collapsing the zero run is what MapLibre's `addRegularDash` does.
    #[test]
    fn dash_pattern_collapses_zero_length_runs_because_a_same_type_boundary_is_not_an_edge() {
        // A trailing zero gap leaves one dash: an all-dash pattern is solid,
        // which `none()` renders with no shader work and no seam.
        for solid in [&[2.0, 0.0], &[1.0, 0.0], &[10.0, 0.0]] {
            let d = DashPattern::from_runs(solid);
            assert_eq!(d.count, 0, "{solid:?} must render solid, not dashed");
            assert_eq!(d.period(), 0.0);
        }

        // A zero gap in the MIDDLE merges the dashes it was separating:
        // `[2,0,6,2]` is dash 2, gap 0, dash 6, gap 2 -- i.e. dash 8, gap 2.
        let d = DashPattern::from_runs(&[2.0, 0.0, 6.0, 2.0]);
        assert_eq!(d.count, 2);
        assert_eq!(&d.runs[..2], &[8.0, 2.0]);
        assert_eq!(d.period(), 10.0);

        // Collapsing happens BEFORE the 4-run limit, so a 6-run pattern that
        // collapses to 2 is kept rather than degraded to solid.
        let d = DashPattern::from_runs(&[1.0, 0.0, 3.0, 2.0, 0.0, 1.0]);
        assert_eq!(d.count, 2, "collapses to dash 4, gap 3");
        assert_eq!(&d.runs[..2], &[4.0, 3.0]);

        // Patterns with no zero runs are untouched by any of this.
        for unchanged in [&[3.0, 3.0][..], &[0.1, 15.0], &[0.4, 0.8], &[2.0, 2.0, 6.0, 2.0]] {
            let d = DashPattern::from_runs(unchanged);
            assert_eq!(d.count as usize, unchanged.len(), "{unchanged:?} must be unchanged");
            assert_eq!(&d.runs[..unchanged.len()], unchanged);
        }
    }

    // A pattern that sums to zero would divide by zero downstream.
    #[test]
    fn dash_pattern_summing_to_zero_collapses_to_none() {
        assert_eq!(DashPattern::from_runs(&[0.0, 0.0]).count, 0);
    }

    // More than 4 runs would need MapLibre's texture atlas. No vendored layer
    // needs it; degrade to solid rather than truncating to a wrong pattern.
    #[test]
    fn dash_pattern_longer_than_four_runs_degrades_to_none() {
        assert_eq!(DashPattern::from_runs(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).count, 0);
    }

    // A one-element pattern is all dash and no gap, so solid is correct. It is
    // also the shape that the seam merge must not touch: merging a lone run
    // into itself would pop the vec empty and then index [0] -- a panic, not a
    // bad render.
    #[test]
    fn dash_pattern_with_a_single_run_is_solid_and_does_not_panic() {
        assert_eq!(DashPattern::from_runs(&[5.0]).count, 0);
    }

    #[test]
    fn dash_pattern_rejects_empty_and_non_finite_runs() {
        assert_eq!(DashPattern::from_runs(&[]).count, 0);
        assert_eq!(DashPattern::from_runs(&[f32::NAN, 1.0]).count, 0);
        assert_eq!(DashPattern::from_runs(&[1.0, f32::INFINITY]).count, 0);
        assert_eq!(DashPattern::from_runs(&[-1.0, 1.0]).count, 0);
    }

    fn square_polygon() -> Geometry<i32> {
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]),
            vec![],
        ))
    }

    fn tile_with_one_water_feature() -> DecodedTile {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("water".to_string()));
        DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landuse".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        }
    }

    #[test]
    fn zoom_value_picks_the_nearest_lower_stop() {
        let value = ZoomValue::stepped(vec![(0, 1.0), (5, 5.0), (10, 10.0)]);
        assert_eq!(value.value_at(0), 1.0);
        assert_eq!(value.value_at(3), 1.0);
        assert_eq!(value.value_at(7), 5.0);
        assert_eq!(value.value_at(20), 10.0);
    }

    #[test]
    fn interpolated_zoom_value_blends_linearly_between_stops() {
        let zv = ZoomValue::interpolated(vec![(10, 0.0f32), (20, 10.0f32)]);
        assert_eq!(zv.value_at(10), 0.0);
        assert_eq!(zv.value_at(20), 10.0);
        assert!((zv.value_at(15) - 5.0).abs() < 1e-6);
    }

    #[test]
    fn interpolated_zoom_value_clamps_outside_the_stop_range() {
        let zv = ZoomValue::interpolated(vec![(10, 0.0f32), (20, 10.0f32)]);
        assert_eq!(zv.value_at(5), 0.0);
        assert_eq!(zv.value_at(25), 10.0);
    }

    #[test]
    fn stepped_zoom_value_keeps_the_existing_hard_cutoff_behavior() {
        // Existing ZoomValue::constant already tests this implicitly; this
        // makes the "stepped" path (multi-stop, no interpolation) explicit.
        let zv = ZoomValue::stepped(vec![(10, "a"), (20, "b")]);
        assert_eq!(zv.value_at(15), "a");
        assert_eq!(zv.value_at(20), "b");
    }

    #[test]
    fn value_at_zoom_blends_the_whole_zoom_pair_without_off_grid_stops() {
        let v = ZoomValue::interpolated(vec![(15, 0.0_f32), (16, 0.8)]);
        assert!((v.value_at_zoom(15.25) - 0.2).abs() < 1e-6);
        let stepped = ZoomValue::stepped(vec![(15, 0.0_f32), (16, 0.8)]);
        assert_eq!(stepped.value_at_zoom(15.9), 0.0, "a step never blends");
        assert_eq!(v.value_at_zoom(f64::NAN), 0.0);
    }

    #[test]
    fn value_at_zoom_follows_off_grid_stops() {
        let v = ZoomValue::interpolated(vec![(15, 0.0_f32), (16, 0.8)]).with_off_grid_stops(vec![(15.0, 0.0), (15.3, 0.8)]);
        assert!((v.value_at_zoom(15.2) - 0.8 * 2.0 / 3.0).abs() < 1e-5, "{}", v.value_at_zoom(15.2));
        assert_eq!(v.value_at_zoom(14.0), 0.0);
        assert_eq!(v.value_at_zoom(15.6), 0.8);
        // The integer readers are untouched.
        assert_eq!(v.value_at_pair(15), (0.0, 0.8));
        // A step ignores them: it has no curve between its stops.
        let s = ZoomValue::stepped(vec![(15, 0.0_f32), (16, 0.8)]).with_off_grid_stops(vec![(15.0, 0.0), (15.3, 0.8)]);
        assert_eq!(s.value_at_zoom(15.2), 0.0);
    }

    #[test]
    fn a_fractional_zoom_stop_compiles_to_off_grid_stops() {
        use crate::style_spec::expression::{compile_off_grid_stops, Scope};
        let leaf = |v: &serde_json::Value, _: Scope<'_>| v.as_f64().map(|n| n as f32).ok_or_else(|| anyhow::anyhow!("number"));
        let stops = compile_off_grid_stops(&serde_json::json!(["interpolate", ["linear"], ["zoom"], 15, 0, 15.3, 0.8]), leaf, Scope::root());
        assert_eq!(stops, Some(vec![(15.0, 0.0), (15.3, 0.8)]), "a linear curve keeps exactly its stops");
        let v = ZoomValue::interpolated(vec![(15, 0.0_f32), (16, 0.8)]).with_off_grid_stops(stops.unwrap());
        assert!((v.value_at_zoom(15.2) - 0.8 * 2.0 / 3.0).abs() < 1e-5);
        // On-grid stops need none.
        assert_eq!(compile_off_grid_stops(&serde_json::json!(["interpolate", ["linear"], ["zoom"], 15, 0, 16, 0.8]), leaf, Scope::root()), None);
        // An exponential curve is sampled finely between its stops, not
        // read as a straight line.
        let stops = compile_off_grid_stops(&serde_json::json!(["interpolate", ["exponential", 2], ["zoom"], 15, 0, 15.5, 1]), leaf, Scope::root());
        let v = ZoomValue::interpolated(vec![(15, 0.0_f32), (16, 1.0)]).with_off_grid_stops(stops.unwrap());
        // Style-spec exponential: (2^0.25 - 1) / (2^0.5 - 1) = 0.457 (linear would be 0.5).
        let want = (2.0_f32.powf(0.25) - 1.0) / (2.0_f32.powf(0.5) - 1.0);
        assert!((v.value_at_zoom(15.25) - want).abs() < 1e-4, "{} vs {want}", v.value_at_zoom(15.25));
        // 15.25 is itself a 1/16 sample; read BETWEEN samples too, against
        // the closed-form curve: (2^(z - 15) - 1) / (2^0.5 - 1).
        for z in [15.03_f64, 15.28, 15.47] {
            let want = ((2.0_f64.powf(z - 15.0) - 1.0) / (2.0_f64.sqrt() - 1.0)) as f32;
            assert!((v.value_at_zoom(z) - want).abs() < 1e-3, "exponential z{z}: {} vs {want}", v.value_at_zoom(z));
        }
        // A cubic-bezier curve, read between samples against the curve
        // itself (`Interpolation::t_at`, the evaluator every compiled
        // resample uses), over a stop range that is off-grid at both ends.
        let json = serde_json::json!(["interpolate", ["cubic-bezier", 0.42, 0, 0.58, 1], ["zoom"], 14.5, 0, 15.7, 10]);
        let curve = crate::style_spec::expression::Interpolation::parse(&json[1], &json).unwrap();
        let stops = compile_off_grid_stops(&json, leaf, Scope::root());
        let v = ZoomValue::interpolated(vec![(14, 0.0_f32), (15, 5.0), (16, 10.0)]).with_off_grid_stops(stops.unwrap());
        for z in [14.5_f64, 14.61, 14.93, 15.1, 15.33, 15.69, 15.7] {
            let want = (10.0 * curve.t_at(z, 14.5, 15.7)) as f32;
            // Piecewise-linear between 1/16-zoom samples: the chord error
            // peaks near the ends where the ease bends hardest, ~0.0103 on
            // this 10-unit range (0.1%) at z14.61. 0.2% of the range.
            assert!((v.value_at_zoom(z) - want).abs() < 2e-2, "cubic-bezier z{z}: {} vs {want}", v.value_at_zoom(z));
        }
        // Ease-in-out: slow at the ends, so a quarter of the way in is well
        // under a quarter of the value -- not the straight line.
        assert!(v.value_at_zoom(14.8) < 1.6, "{}", v.value_at_zoom(14.8));
        assert_eq!(v.value_at_zoom(14.0), 0.0);
        assert_eq!(v.value_at_zoom(16.0), 10.0);
    }

    #[test]
    fn value_at_pair_returns_the_true_bracket_for_an_interpolated_value() {
        let zv = ZoomValue::interpolated(vec![(10, 0.0f32), (20, 10.0f32)]);
        let (from, to) = zv.value_at_pair(14);
        assert!((from - 4.0).abs() < 1e-6, "expected value_at(14) = 4.0, got {from}");
        assert!((to - 5.0).abs() < 1e-6, "expected value_at(15) = 5.0, got {to}");
    }

    #[test]
    fn value_at_pair_returns_an_equal_pair_for_a_stepped_value() {
        let zv = ZoomValue::stepped(vec![(0, 1.0), (5, 5.0), (10, 10.0)]);
        // A step boundary (5) falls strictly between 4 and 5 -- a stepped
        // value must NOT continuously blend across it; both pair elements
        // must equal value_at(4) exactly (the snap-at-bucket behavior).
        let (from, to) = zv.value_at_pair(4);
        assert_eq!(from, 1.0);
        assert_eq!(to, 1.0);
    }

    #[test]
    fn value_at_pair_returns_an_equal_pair_for_a_constant_value() {
        let zv = ZoomValue::constant(42.0);
        let (from, to) = zv.value_at_pair(7);
        assert_eq!(from, 42.0);
        assert_eq!(to, 42.0);
    }

    #[test]
    fn value_at_pair_does_not_panic_at_the_top_of_the_u8_range() {
        let zv = ZoomValue::interpolated(vec![(10, 0.0f32), (20, 10.0f32)]);
        let (from, to) = zv.value_at_pair(255);
        assert_eq!(from, 10.0);
        assert_eq!(to, 10.0, "saturating_add(1) at u8::MAX must not overflow-panic");
    }

    #[test]
    fn resolves_a_constant_layout_value() {
        let v = LayoutValue::Constant(12.0);
        assert_eq!(v.resolve(&HashMap::new(), 10), 12.0);
    }

    #[test]
    fn resolves_a_zoom_step_layout_value() {
        let v = LayoutValue::ZoomStep(ZoomValue::stepped(vec![(0, 10.0), (14, 20.0)]));
        assert_eq!(v.resolve(&HashMap::new(), 10), 10.0);
        assert_eq!(v.resolve(&HashMap::new(), 15), 20.0);
    }

    #[test]
    fn resolves_a_data_step_layout_value_using_the_real_sizerank_shape() {
        // step(get(sizerank), 18, 5, 12) -- base 18 below sizerank=5, 12 at/above.
        let v = LayoutValue::DataStep(DataStepShape {
            key: "sizerank".to_string(),
            base: 18.0,
            stops: vec![(5.0, 12.0)],
        });
        let mut tags_low = HashMap::new();
        tags_low.insert("sizerank".to_string(), TagValue::Number(2.0));
        assert_eq!(v.resolve(&tags_low, 10), 18.0);
        let mut tags_high = HashMap::new();
        tags_high.insert("sizerank".to_string(), TagValue::Number(7.0));
        assert_eq!(v.resolve(&tags_high, 10), 12.0);
        // Missing/wrong-typed tag falls back to base.
        assert_eq!(v.resolve(&HashMap::new(), 10), 18.0);
    }

    #[test]
    fn resolves_the_real_composite_zoom_of_data_step_shape() {
        // poi-label's real text-size shape:
        // step(zoom, step(sizerank, 18, 5, 12), 17, step(sizerank, 18, 13, 12))
        let low_zoom = DataStepShape { key: "sizerank".to_string(), base: 18.0, stops: vec![(5.0, 12.0)] };
        let high_zoom = DataStepShape { key: "sizerank".to_string(), base: 18.0, stops: vec![(13.0, 12.0)] };
        let v = LayoutValue::ZoomOfDataStep(vec![(0, low_zoom), (17, high_zoom)]);
        let mut mid_rank = HashMap::new();
        mid_rank.insert("sizerank".to_string(), TagValue::Number(8.0));
        // At zoom 10 (bracket = low_zoom's step): sizerank 8 >= 5 -> 12.0
        assert_eq!(v.resolve(&mid_rank, 10), 12.0);
        // At zoom 18 (bracket = high_zoom's step): sizerank 8 < 13 -> base 18.0
        assert_eq!(v.resolve(&mid_rank, 18), 18.0);
    }

    #[test]
    fn rgba_lerp_blends_all_four_channels() {
        let a = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
        let b = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let mid = a.lerp(&b, 0.5);
        assert!((mid.r - 0.5).abs() < 1e-6);
        assert!((mid.a - 0.5).abs() < 1e-6);
    }

    #[test]
    fn match_tile_skips_a_layer_outside_its_zoom_range() {
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "test-layer".to_string(),
                source_layer: "landuse".to_string(),
                filter: Filter::Equals { key: "class".to_string(), value: "water".to_string() },
                color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }),
                opacity: ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: Some(5),
                max_zoom: Some(10),
            }],
            sky: Default::default(),
        };
        let tile = tile_with_one_water_feature();

        let below = match_tile(&tile, &style, 3);
        assert_eq!(below.ops.len(), 0);

        let above = match_tile(&tile, &style, 12);
        assert_eq!(above.ops.len(), 0);

        let in_range = match_tile(&tile, &style, 7);
        assert_eq!(in_range.ops.len(), 1);
        assert!(matches!(in_range.ops[0], PaintOp::Fill { .. }));
    }

    #[test]
    fn layer_zoom_fade_returns_zero_at_and_below_min_zoom_full_strength_above_it() {
        assert_eq!(layer_zoom_fade(Some(10), None, 9), 0.0, "below min_zoom: fully faded out");
        assert_eq!(layer_zoom_fade(Some(10), None, 10), 0.0, "at min_zoom itself: still 0 -- the fade-in ramp starts here, ends at min_zoom+1");
        assert_eq!(layer_zoom_fade(Some(10), None, 11), 1.0, "one full zoom level past min_zoom: fully faded in");
        assert_eq!(layer_zoom_fade(Some(10), None, 20), 1.0, "well above min_zoom: still full strength");
    }

    #[test]
    fn layer_zoom_fade_is_full_strength_at_zoom_zero_when_min_zoom_is_zero() {
        // The ramp above needs a bucket BELOW `min_zoom` for "fading in" to
        // mean anything, and `min_zoom: 0` has none -- zoom 0 is the floor
        // of the camera's range. A layer declared from zoom 0 is therefore
        // at full strength AT zoom 0, not ramping up out of nothing.
        assert_eq!(
            layer_zoom_fade(Some(0), None, 0),
            1.0,
            "a `minzoom: 0` layer is the whole map at z0 -- there is no zoom below 0 \
             for it to have faded in from"
        );
        assert_eq!(layer_zoom_fade(Some(0), None, 1), 1.0);
        // And the pair the GPU blends across the [0, 1) bucket is constant,
        // so the layer does not wash out halfway through it either.
        assert_eq!(layer_zoom_fade_pair(Some(0), None, 0), (1.0, 1.0));
        // A bounded range from zero behaves the same, and still ends where
        // `max_zoom` says. `landcover` in the shipped Mapbox Streets style
        // is exactly `minzoom: 0, maxzoom: 12`.
        assert_eq!(layer_zoom_fade(Some(0), Some(12), 0), 1.0);
        assert_eq!(layer_zoom_fade(Some(0), Some(12), 12), 1.0);
        assert_eq!(layer_zoom_fade(Some(0), Some(12), 13), 0.0);
        // Every OTHER `min_zoom` keeps the ramp untouched -- this arm is a
        // statement about zoom 0's missing neighbour, not a removal of the
        // fade.
        assert_eq!(layer_zoom_fade(Some(1), None, 1), 0.0);
        assert_eq!(layer_zoom_fade(Some(1), None, 2), 1.0);
    }

    #[test]
    fn layer_zoom_fade_returns_full_strength_at_and_below_max_zoom_zero_above_it() {
        assert_eq!(layer_zoom_fade(None, Some(10), 10), 1.0, "at max_zoom itself: still full strength");
        assert_eq!(layer_zoom_fade(None, Some(10), 11), 0.0, "one zoom level past max_zoom: fully faded out");
        assert_eq!(layer_zoom_fade(None, Some(10), 5), 1.0, "well below max_zoom: full strength");
    }

    #[test]
    fn layer_zoom_fade_is_one_everywhere_with_no_zoom_range() {
        for z in [0, 5, 10, 255] {
            assert_eq!(layer_zoom_fade(None, None, z), 1.0);
        }
    }

    #[test]
    fn layer_zoom_fade_toggles_correctly_when_min_zoom_equals_max_zoom() {
        // The aliasing bug this guards against: a layer visible at exactly
        // one zoom level must still be visible there, not permanently
        // faded to zero.
        assert_eq!(layer_zoom_fade(Some(10), Some(10), 9), 0.0);
        assert_eq!(layer_zoom_fade(Some(10), Some(10), 10), 1.0, "must be visible at exactly its single valid zoom level");
        assert_eq!(layer_zoom_fade(Some(10), Some(10), 11), 0.0);
    }

    #[test]
    fn layer_zoom_fade_toggles_correctly_when_max_zoom_is_less_than_min_zoom() {
        // A malformed/reversed range (external style data, not validated
        // upstream) -- must not crash or misbehave, falls back to the
        // same narrow-range guard.
        assert_eq!(layer_zoom_fade(Some(10), Some(5), 10), 0.0, "zoom == min_zoom but min_zoom != the guard's comparison target when reversed -- confirm no panic and a defined (0) result");
    }

    #[test]
    fn layer_zoom_fade_still_smoothly_fades_when_the_range_is_wide_enough() {
        // Regression guard: the narrow-range fallback must NOT kick in
        // for a normal, wide zoom range -- the smooth fade from the
        // original feature must still work exactly as before.
        assert_eq!(layer_zoom_fade(Some(10), Some(20), 10), 0.0, "still 0 exactly at min_zoom (fade-in starts here)");
        assert_eq!(layer_zoom_fade(Some(10), Some(20), 11), 1.0, "still fully faded in one level past min_zoom");
        assert_eq!(layer_zoom_fade(Some(10), Some(20), 20), 1.0, "still full strength at max_zoom (fade-out starts here)");
        assert_eq!(layer_zoom_fade(Some(10), Some(20), 21), 0.0, "still fully faded out one level past max_zoom");
    }

    #[test]
    fn layer_zoom_fade_is_smooth_at_the_minimum_safe_gap_of_exactly_one_zoom_level_apart() {
        // The boundary case right at the edge of the guard: max_zoom ==
        // min_zoom + 1 is the narrowest range where the two-sample trick
        // still works WITHOUT aliasing (the appear ramp's rising bucket
        // and the disappear ramp's falling bucket are adjacent, not
        // overlapping) -- confirm the guard does NOT incorrectly kick in
        // here too.
        assert_eq!(layer_zoom_fade(Some(10), Some(11), 10), 0.0, "fade-in starts at min_zoom");
        assert_eq!(layer_zoom_fade(Some(10), Some(11), 11), 1.0, "the one bucket where both ramps are 1 -- full strength");
        assert_eq!(layer_zoom_fade(Some(10), Some(11), 12), 0.0, "fade-out completed one level past max_zoom");
    }

    #[test]
    fn layer_zoom_fade_pair_holds_constant_full_strength_through_a_narrow_ranges_one_valid_bucket() {
        // The bug this fixes: independently sampling layer_zoom_fade at
        // zoom and zoom+1 for a narrow range gives (1.0, 0.0), which the
        // GPU would blend into a fade-out across the layer's one valid
        // bucket. Computing the pair together must hold constant instead.
        assert_eq!(layer_zoom_fade_pair(Some(10), Some(10), 9), (0.0, 0.0));
        assert_eq!(
            layer_zoom_fade_pair(Some(10), Some(10), 10),
            (1.0, 1.0),
            "both pair elements must be 1.0 -- constant full opacity throughout the one valid bucket, no fade-out"
        );
        assert_eq!(layer_zoom_fade_pair(Some(10), Some(10), 11), (0.0, 0.0));
    }

    #[test]
    fn layer_zoom_fade_pair_matches_the_smooth_fade_for_a_wide_range() {
        // Wide-range behavior must be byte-identical to calling
        // layer_zoom_fade independently at zoom/zoom+1, exactly as
        // match_tile did before layer_zoom_fade_pair existed.
        assert_eq!(layer_zoom_fade_pair(Some(10), Some(20), 10), (0.0, 1.0));
        assert_eq!(layer_zoom_fade_pair(Some(10), Some(20), 20), (1.0, 0.0));
        assert_eq!(layer_zoom_fade_pair(None, None, 15), (1.0, 1.0));
    }

    /// The variant name of a `PaintOp`, for assertion messages that need to
    /// show the ORDER ops came out in rather than their contents.
    fn op_kind(op: &PaintOp) -> &'static str {
        match op {
            PaintOp::Background { .. } => "Background",
            PaintOp::Fill { .. } => "Fill",
            PaintOp::Line { .. } => "Line",
            PaintOp::Extrusion { .. } => "Extrusion",
        }
    }

    /// A fill layer that declares `fill-outline-color` emits, after its
    /// fill, a one-pixel `PaintOp::Line` tracing the same polygon's rings.
    ///
    /// Three things are pinned here and each is load-bearing:
    ///
    ///   * the width is exactly `FILL_OUTLINE_WIDTH_PX` and does NOT vary
    ///     with zoom -- MapLibre draws the outline with `gl.LINES`, one
    ///     device pixel, with no zoom term (`draw_fill.ts`);
    ///   * the outline op comes AFTER every fill op of the same layer, so a
    ///     neighbouring footprint's fill cannot paint over an edge already
    ///     drawn (MapLibre separates `drawFill` and `drawOutline` into two
    ///     passes for the same reason);
    ///   * the outline's alpha carries `fill-opacity`, matching
    ///     `fill_outline.fragment.glsl`'s
    ///     `fragColor = outline_color * (alpha * opacity)`.
    #[test]
    fn a_fill_outline_colour_emits_a_one_pixel_line_after_the_layers_fills() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("wood".to_string()));
        let mut second_tags = HashMap::new();
        second_tags.insert("class".to_string(), TagValue::String("wood".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landcover".to_string(),
                features: vec![
                    Feature { geometry: square_polygon(), tags: tags.into() },
                    Feature { geometry: square_polygon(), tags: second_tags.into() },
                ],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "test-layer".to_string(),
                source_layer: "landcover".to_string(),
                filter: Filter::Any,
                color: ZoomValue::constant(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
                opacity: ZoomValue::constant(0.5),
                outline_color: Some(ZoomValue::constant(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 })),
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };

        let plan = match_tile(&tile, &style, 14);
        assert_eq!(plan.ops.len(), 4, "two features must give two fills and two outlines");

        // Both fills first, then both outlines -- not fill/outline/fill/outline.
        assert!(
            matches!(plan.ops[0], PaintOp::Fill { .. }) && matches!(plan.ops[1], PaintOp::Fill { .. }),
            "the layer's fills must come first, got {:?}",
            plan.ops.iter().map(op_kind).collect::<Vec<_>>()
        );

        let PaintOp::Line { color_from, width_from, width_to, dash, geometry, .. } = &plan.ops[2] else {
            panic!(
                "op 2 must be the outline, got {:?}",
                plan.ops.iter().map(op_kind).collect::<Vec<_>>()
            );
        };
        assert_eq!(*width_from, FILL_OUTLINE_WIDTH_PX);
        assert_eq!(*width_to, FILL_OUTLINE_WIDTH_PX);
        assert_eq!(*dash, DashPattern::none(), "an outline is never dashed");
        assert!(
            (color_from.a - 0.5).abs() < 1e-6,
            "the outline must carry fill-opacity (0.5), got {}",
            color_from.a
        );
        assert!(color_from.r > 0.99 && color_from.g < 0.01, "outline colour should be the declared red");
        assert!(
            matches!(**geometry, geo_types::Geometry::MultiLineString(_)),
            "the outline traces the fill's rings as line geometry"
        );
        assert!(matches!(plan.ops[3], PaintOp::Line { .. }), "the second feature's outline follows");
    }

    /// The counterpart: no `fill-outline-color` means no extra geometry at
    /// all. This is the hot path -- every fill layer in every vendored
    /// style except two of Liberty's -- and the whole reason the spec's
    /// "defaults to fill-color" is not reproduced literally, so it is worth
    /// pinning that the op count really is unchanged.
    #[test]
    fn a_fill_without_an_outline_colour_emits_no_line_ops_at_all() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("wood".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landcover".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "test-layer".to_string(),
                source_layer: "landcover".to_string(),
                filter: Filter::Any,
                color: ZoomValue::constant(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
                opacity: ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };

        let plan = match_tile(&tile, &style, 14);
        assert_eq!(plan.ops.len(), 1, "one feature, one fill, no outline");
        assert!(matches!(plan.ops[0], PaintOp::Fill { .. }));
    }

    #[test]
    fn match_tile_holds_a_narrow_range_fill_layer_at_full_opacity_throughout_its_one_valid_bucket() {
        // The match_tile-level regression test for this bug -- checks
        // BOTH color_from.a and color_to.a, which is exactly what the
        // prior (pair-blending) bug would have failed on color_to.a.
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("wood".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landcover".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "test-layer".to_string(),
                source_layer: "landcover".to_string(),
                filter: Filter::Any,
                color: ZoomValue::constant(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
                opacity: ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: Some(10),
                max_zoom: Some(10),
            }],
            sky: Default::default(),
        };

        let plan = match_tile(&tile, &style, 10);
        let PaintOp::Fill { color_from, color_to, .. } = &plan.ops[0] else {
            panic!("expected a Fill op at the layer's one valid zoom bucket");
        };
        assert!((color_from.a - 1.0).abs() < 1e-6, "color_from.a should be full strength, got {}", color_from.a);
        assert!(
            (color_to.a - 1.0).abs() < 1e-6,
            "color_to.a should ALSO be full strength (not faded toward 0) -- this is the exact bug: \
             independently sampling at zoom+1 gave color_to.a=0.0, causing a visible fade-out across \
             the layer's one valid bucket instead of holding steady, got {}",
            color_to.a
        );
    }

    #[test]
    fn match_tile_fades_a_fill_layer_in_across_the_bucket_straddling_its_min_zoom() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("wood".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landcover".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "test-layer".to_string(),
                source_layer: "landcover".to_string(),
                filter: Filter::Any,
                color: ZoomValue::constant(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
                opacity: ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: Some(10),
                max_zoom: None,
            }],
            sky: Default::default(),
        };

        // Below min_zoom: still skipped entirely -- same as before this change.
        let below = match_tile(&tile, &style, 9);
        assert!(below.ops.is_empty(), "layer should still be entirely skipped below its min_zoom");

        // AT min_zoom: now included (not skipped), but its baked alpha is
        // 0 -- this is the START of the fade-in bucket, invisible at the
        // exact integer, becoming visible only as the camera's fractional
        // zoom eases past it (verified by the shader-blend tests already
        // covering `value_at_pair`/`fract(camera.zoom)` elsewhere).
        let at = match_tile(&tile, &style, 10);
        let PaintOp::Fill { color_from, .. } = &at.ops[0] else { panic!("expected a Fill op") };
        assert!(color_from.a < 1e-6, "alpha should be ~0 exactly at min_zoom, got {}", color_from.a);

        // One full zoom level past min_zoom: fully faded in.
        let above = match_tile(&tile, &style, 11);
        let PaintOp::Fill { color_from, .. } = &above.ops[0] else { panic!("expected a Fill op") };
        assert!((color_from.a - 1.0).abs() < 1e-6, "alpha should be full strength one zoom level past min_zoom, got {}", color_from.a);
    }

    #[test]
    fn filter_equals_matches_only_the_right_value() {
        let filter = Filter::Equals { key: "class".to_string(), value: "water".to_string() };
        let mut matching = HashMap::new();
        matching.insert("class".to_string(), TagValue::String("water".to_string()));
        assert!(filter.matches(&matching, 14.0));

        let mut other = HashMap::new();
        other.insert("class".to_string(), TagValue::String("forest".to_string()));
        assert!(!filter.matches(&other, 14.0));

        assert!(!filter.matches(&HashMap::new(), 14.0));
    }

    #[test]
    fn filter_in_matches_any_listed_value() {
        let filter = Filter::In {
            key: "class".to_string(),
            values: vec!["water".to_string(), "river".to_string()],
        };
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("river".to_string()));
        assert!(filter.matches(&tags, 14.0));
    }

    /// The three classes the style spec's `["geometry-type"]` can return
    /// over a vector tile, and the six `geo_types` shapes that map onto
    /// them. `MultiPoint` mapping to `Point` is the one that must not be
    /// got wrong: `mvt::decode` returns `MultiPoint` even for a
    /// single-point feature, so a `Point`-only mapping would make every
    /// `["==", ["geometry-type"], "Point"]` filter in every style match
    /// nothing at all.
    #[test]
    fn geometry_kind_collapses_the_multi_shapes_onto_their_base_class() {
        use geo_types::{Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
        let ring = LineString::from(vec![(0, 0), (10, 0), (10, 10), (0, 0)]);
        let cases: Vec<(Geometry<i32>, GeometryKind)> = vec![
            (Geometry::Point(Point::new(1, 2)), GeometryKind::Point),
            (Geometry::MultiPoint(MultiPoint::new(vec![Point::new(1, 2)])), GeometryKind::Point),
            (Geometry::LineString(LineString::from(vec![(0, 0), (1, 1)])), GeometryKind::LineString),
            (
                Geometry::MultiLineString(MultiLineString::new(vec![LineString::from(vec![(0, 0), (1, 1)])])),
                GeometryKind::LineString,
            ),
            (Geometry::Polygon(Polygon::new(ring.clone(), vec![])), GeometryKind::Polygon),
            (Geometry::MultiPolygon(MultiPolygon::new(vec![Polygon::new(ring, vec![])])), GeometryKind::Polygon),
        ];
        for (geometry, expected) in cases {
            assert_eq!(GeometryKind::of(&geometry), Some(expected), "{geometry:?}");
        }
    }

    /// Every spelling a style may write, `Multi*` included. MapLibre over a
    /// vector tile never RETURNS a `Multi*` name, so those are dead labels
    /// there; accepting them as aliases is what keeps a hypothetical
    /// `["==", ["geometry-type"], "MultiPolygon"]` from emptying a layer.
    /// See `GeometryKind`'s doc comment.
    #[test]
    fn geometry_kind_reads_both_the_base_and_multi_spellings_and_rejects_anything_else() {
        assert_eq!(GeometryKind::from_style_name("Point"), Some(GeometryKind::Point));
        assert_eq!(GeometryKind::from_style_name("MultiPoint"), Some(GeometryKind::Point));
        assert_eq!(GeometryKind::from_style_name("LineString"), Some(GeometryKind::LineString));
        assert_eq!(GeometryKind::from_style_name("MultiLineString"), Some(GeometryKind::LineString));
        assert_eq!(GeometryKind::from_style_name("Polygon"), Some(GeometryKind::Polygon));
        assert_eq!(GeometryKind::from_style_name("MultiPolygon"), Some(GeometryKind::Polygon));
        for unknown in ["Unknown", "linestring", "LINESTRING", "Curve", ""] {
            assert_eq!(GeometryKind::from_style_name(unknown), None, "{unknown:?}");
        }
    }

    /// The predicate excludes the classes it does not name -- and matches
    /// unconditionally when the caller has no geometry to test, which is
    /// what keeps `Filter::matches`' tags-only contract intact.
    #[test]
    fn a_geometry_type_filter_selects_its_classes_and_abstains_without_geometry() {
        let lines = Filter::GeometryType(vec![GeometryKind::LineString]);
        let none = HashMap::new();
        assert!(lines.matches_geometry(&none, 14.0, Some(GeometryKind::LineString)));
        assert!(!lines.matches_geometry(&none, 14.0, Some(GeometryKind::Polygon)));
        assert!(!lines.matches_geometry(&none, 14.0, Some(GeometryKind::Point)));
        assert!(lines.matches_geometry(&none, 14.0, None), "unknown geometry must not delete the feature");
        assert!(lines.matches(&none, 14.0), "the tags-only entry point abstains the same way");

        // Nested inside a combinator, the geometry reaches the leaf.
        let minor_lines = Filter::All(vec![
            lines,
            Filter::Equals { key: "class".into(), value: "minor".into() },
        ]);
        let minor: HashMap<String, TagValue> = [("class".to_string(), TagValue::String("minor".into()))].into_iter().collect();
        assert!(minor_lines.matches_geometry(&minor, 14.0, Some(GeometryKind::LineString)));
        assert!(!minor_lines.matches_geometry(&minor, 14.0, Some(GeometryKind::Polygon)));

        // A geometry predicate never varies with zoom, so it must not force
        // a layer's matches to be recomputed on every zoom change.
        assert!(!Filter::GeometryType(vec![GeometryKind::Point]).uses_zoom());
    }

    #[test]
    fn filter_all_matches_only_when_every_subfilter_matches() {
        let filter = Filter::All(vec![
            Filter::Equals { key: "class".to_string(), value: "river".to_string() },
            Filter::Equals { key: "brunnel".to_string(), value: "tunnel".to_string() },
        ]);

        let mut both = HashMap::new();
        both.insert("class".to_string(), TagValue::String("river".to_string()));
        both.insert("brunnel".to_string(), TagValue::String("tunnel".to_string()));
        assert!(filter.matches(&both, 14.0));

        let mut only_one = HashMap::new();
        only_one.insert("class".to_string(), TagValue::String("river".to_string()));
        assert!(!filter.matches(&only_one, 14.0));
    }

    #[test]
    fn filter_any_of_matches_when_any_subfilter_matches() {
        let filter = Filter::AnyOf(vec![
            Filter::Equals { key: "class".to_string(), value: "river".to_string() },
            Filter::Equals { key: "class".to_string(), value: "lake".to_string() },
        ]);

        let mut river = HashMap::new();
        river.insert("class".to_string(), TagValue::String("river".to_string()));
        assert!(filter.matches(&river, 14.0));

        let mut forest = HashMap::new();
        forest.insert("class".to_string(), TagValue::String("forest".to_string()));
        assert!(!filter.matches(&forest, 14.0));
    }

    #[test]
    fn filter_number_compare_checks_every_operator_and_treats_absent_tags_as_non_matching() {
        let mut tags = HashMap::new();
        tags.insert("rank".to_string(), TagValue::Number(5.0));

        let eq = Filter::NumberCompare { key: "rank".to_string(), op: NumberOp::Eq, value: 5.0 };
        assert!(eq.matches(&tags, 14.0));
        let not_eq = Filter::NumberCompare { key: "rank".to_string(), op: NumberOp::NotEq, value: 5.0 };
        assert!(!not_eq.matches(&tags, 14.0));
        let lt = Filter::NumberCompare { key: "rank".to_string(), op: NumberOp::Lt, value: 6.0 };
        assert!(lt.matches(&tags, 14.0));
        let lte = Filter::NumberCompare { key: "rank".to_string(), op: NumberOp::Lte, value: 5.0 };
        assert!(lte.matches(&tags, 14.0));
        let gt = Filter::NumberCompare { key: "rank".to_string(), op: NumberOp::Gt, value: 4.0 };
        assert!(gt.matches(&tags, 14.0));
        let gte = Filter::NumberCompare { key: "rank".to_string(), op: NumberOp::Gte, value: 5.0 };
        assert!(gte.matches(&tags, 14.0));

        // Absent tag: every direct NumberCompare op (including NotEq) is
        // non-matching -- this is the "false unless the tag is present and
        // numeric" fallback in `Filter::matches`. Style-spec "!=" filters
        // instead compile to `Not(NumberCompare{Eq})` precisely to get the
        // spec's different absent-tag semantics; see style_spec::filter.
        assert!(!not_eq.matches(&HashMap::new(), 14.0));

        // Wrong tag type (string instead of number) also falls through to
        // the non-matching branch rather than panicking or coercing.
        let mut string_tag = HashMap::new();
        string_tag.insert("rank".to_string(), TagValue::String("5".to_string()));
        assert!(!eq.matches(&string_tag, 14.0));
    }

    #[test]
    fn filter_has_matches_when_the_key_is_present_regardless_of_value() {
        let filter = Filter::Has("iata".to_string());
        assert!(!filter.matches(&HashMap::new(), 14.0));
        let mut tags = HashMap::new();
        tags.insert("iata".to_string(), TagValue::String("SIN".to_string()));
        assert!(filter.matches(&tags, 14.0));

        let mut bool_tags = HashMap::new();
        bool_tags.insert("iata".to_string(), TagValue::Bool(true));
        assert!(filter.matches(&bool_tags, 14.0));
    }

    #[test]
    fn filter_not_negates_the_inner_filter() {
        let filter = Filter::Not(Box::new(Filter::Equals {
            key: "brunnel".to_string(),
            value: "tunnel".to_string(),
        }));

        let mut tunnel = HashMap::new();
        tunnel.insert("brunnel".to_string(), TagValue::String("tunnel".to_string()));
        assert!(!filter.matches(&tunnel, 14.0));

        assert!(filter.matches(&HashMap::new(), 14.0));
    }

    #[test]
    fn never_never_matches() {
        assert!(!Filter::Never.matches(&HashMap::new(), 14.0));
    }

    #[test]
    fn zoom_step_uses_base_below_first_stop_and_stops_at_and_above_threshold() {
        let filter = Filter::ZoomStep {
            base: Box::new(Filter::Equals { key: "class".to_string(), value: "low".to_string() }),
            stops: vec![
                (10.0, Filter::Equals { key: "class".to_string(), value: "mid".to_string() }),
                (16.0, Filter::Equals { key: "class".to_string(), value: "high".to_string() }),
            ],
        };
        let mut low = HashMap::new();
        low.insert("class".to_string(), TagValue::String("low".to_string()));
        let mut mid = HashMap::new();
        mid.insert("class".to_string(), TagValue::String("mid".to_string()));
        let mut high = HashMap::new();
        high.insert("class".to_string(), TagValue::String("high".to_string()));

        assert!(filter.matches(&low, 5.0)); // below first stop: base applies
        assert!(!filter.matches(&mid, 5.0));

        assert!(filter.matches(&mid, 10.0)); // at first stop: first stop's filter
        assert!(!filter.matches(&low, 10.0));

        assert!(filter.matches(&mid, 15.9)); // just below second stop: first stop still applies
        assert!(filter.matches(&high, 16.0)); // at second stop: second stop's filter
        assert!(!filter.matches(&mid, 16.0));
    }

    #[test]
    fn case_returns_first_matching_arms_output_else_fallback() {
        let filter = Filter::Case {
            arms: vec![
                (Filter::Has("layer".to_string()), Filter::NumberCompare { key: "layer".to_string(), op: NumberOp::Gte, value: 0.0 }),
            ],
            fallback: Box::new(Filter::Any),
        };
        let mut has_nonneg = HashMap::new();
        has_nonneg.insert("layer".to_string(), TagValue::Number(0.0));
        let mut has_neg = HashMap::new();
        has_neg.insert("layer".to_string(), TagValue::Number(-1.0));

        assert!(filter.matches(&has_nonneg, 14.0));
        assert!(!filter.matches(&has_neg, 14.0));
        assert!(filter.matches(&HashMap::new(), 14.0)); // no "layer" tag: falls through to fallback
    }

    #[test]
    fn multi_match_finds_first_case_group_containing_the_tag_value_else_fallback() {
        let filter = Filter::MultiMatch {
            key: "class".to_string(),
            cases: vec![
                (vec!["track".to_string()], Filter::Any),
                (vec!["service".to_string()], Filter::Never),
            ],
            fallback: Box::new(Filter::Never),
        };
        let mut track = HashMap::new();
        track.insert("class".to_string(), TagValue::String("track".to_string()));
        let mut service = HashMap::new();
        service.insert("class".to_string(), TagValue::String("service".to_string()));
        let mut motorway = HashMap::new();
        motorway.insert("class".to_string(), TagValue::String("motorway".to_string()));

        assert!(filter.matches(&track, 14.0));
        assert!(!filter.matches(&service, 14.0));
        assert!(!filter.matches(&motorway, 14.0));
    }

    #[test]
    fn number_compare_with_zoom_offset_subtracts_the_zoom_dependent_offset_before_comparing() {
        // landuse's real clause: ["<=", ["-", ["to-number", ["get", "sizerank"]],
        // ["interpolate", ["exponential", 1.5], ["zoom"], 12, 0, 18, 14]], 14]
        let filter = Filter::NumberCompareWithZoomOffset {
            key: "sizerank".to_string(),
            op: NumberOp::Lte,
            offset: ZoomValue::interpolated(vec![(12, 0.0), (18, 14.0)]),
            threshold: 14.0,
        };
        let mut low_rank = HashMap::new();
        low_rank.insert("sizerank".to_string(), TagValue::Number(5.0));
        let mut high_rank = HashMap::new();
        high_rank.insert("sizerank".to_string(), TagValue::Number(30.0));

        // At zoom 12, offset is 0.0: 5 - 0 = 5 <= 14 matches; 30 - 0 = 30 <= 14 does not.
        assert!(filter.matches(&low_rank, 12.0));
        assert!(!filter.matches(&high_rank, 12.0));

        // At zoom 18, offset is 14.0: 30 - 14 = 16 <= 14 does not match either,
        // but 5 - 14 = -9 <= 14 still does.
        assert!(filter.matches(&low_rank, 18.0));
        assert!(!filter.matches(&high_rank, 18.0));

        // Absent tag never matches, same convention as NumberCompare.
        assert!(!filter.matches(&HashMap::new(), 12.0));
    }

    #[test]
    fn uses_zoom_is_true_for_the_two_variants_that_consult_zoom_directly() {
        // `matches` takes zoom, but only these two actually read it.
        // Classifying a zoom-reading filter as static caches a match that
        // should vary -- rendering the wrong features at some zooms, with no
        // error anywhere.
        let step = Filter::ZoomStep {
            base: Box::new(Filter::Never),
            stops: vec![(11.0, Filter::Any)],
        };
        assert!(step.uses_zoom());

        // The sneaky one: no "zoom" in the name, but `offset.value_at(zoom)`.
        let offset = Filter::NumberCompareWithZoomOffset {
            key: "sizerank".to_string(),
            op: NumberOp::Lte,
            offset: ZoomValue::constant(0.0),
            threshold: 14.0,
        };
        assert!(offset.uses_zoom(), "a zoom-offset comparison consults zoom");
    }

    #[test]
    fn uses_zoom_is_false_for_every_zoom_free_leaf() {
        assert!(!Filter::Any.uses_zoom());
        assert!(!Filter::Never.uses_zoom());
        assert!(!Filter::Has("class".to_string()).uses_zoom());
        assert!(!Filter::Equals { key: "class".to_string(), value: "park".to_string() }.uses_zoom());
        assert!(!Filter::In { key: "class".to_string(), values: vec!["park".to_string()] }.uses_zoom());
        assert!(!Filter::NumberCompare {
            key: "sizerank".to_string(),
            op: NumberOp::Lte,
            value: 14.0,
        }.uses_zoom());
    }

    #[test]
    fn uses_zoom_propagates_out_of_every_container_variant() {
        // A zoom filter buried inside a combinator must still mark the whole
        // filter dynamic. This is the case most likely to be missed, and it is
        // exactly the shape the real landuse and road filters use.
        let zoomy = || Filter::ZoomStep {
            base: Box::new(Filter::Never),
            stops: vec![(11.0, Filter::Any)],
        };
        assert!(Filter::All(vec![Filter::Any, zoomy()]).uses_zoom(), "all");
        assert!(Filter::AnyOf(vec![Filter::Any, zoomy()]).uses_zoom(), "any");
        assert!(Filter::Not(Box::new(zoomy())).uses_zoom(), "not");
        // Case: the zoom filter can hide in a condition OR in a result.
        assert!(Filter::Case {
            arms: vec![(zoomy(), Filter::Any)],
            fallback: Box::new(Filter::Never),
        }.uses_zoom(), "case condition");
        assert!(Filter::Case {
            arms: vec![(Filter::Any, zoomy())],
            fallback: Box::new(Filter::Never),
        }.uses_zoom(), "case result");
        assert!(Filter::Case {
            arms: vec![(Filter::Any, Filter::Any)],
            fallback: Box::new(zoomy()),
        }.uses_zoom(), "case fallback");
        assert!(Filter::MultiMatch {
            key: "class".to_string(),
            cases: vec![(vec!["park".to_string()], zoomy())],
            fallback: Box::new(Filter::Never),
        }.uses_zoom(), "multi-match");
        assert!(Filter::NumberMultiMatch {
            key: "level".to_string(),
            cases: vec![(vec![89.0], zoomy())],
            fallback: Box::new(Filter::Never),
        }.uses_zoom(), "number multi-match");
    }

    #[test]
    fn the_real_landuse_filter_is_classified_dynamic() {
        // End-to-end guard that the real landuse filter classifies dynamic.
        // The compiled filter is `All([NumberCompare, MultiMatch{..with a
        // ZoomStep arm..}, NumberCompareWithZoomOffset])`; `All`'s `.any()`
        // short-circuits on the second child (the `MultiMatch`'s nested
        // `ZoomStep`), so this test never actually visits the third child
        // and does NOT guard `NumberCompareWithZoomOffset` -- that variant
        // is the reason `NumberCompareWithZoomOffset` exists, but its
        // classification is pinned directly by
        // `uses_zoom_is_true_for_the_two_variants_that_consult_zoom_directly`
        // above, not by this test.
        let bytes = mapbox_fixture_or_skip!("mapbox-streets-style.json");
        let style = crate::style_spec::parse_and_compile(&bytes).expect("Mapbox Streets style must compile cleanly");
        let landuse_filter = style
            .layers
            .iter()
            .find_map(|layer| match layer {
                StyleLayer::Fill { id, filter, .. } if id == "landuse" => Some(filter),
                _ => None,
            })
            .expect("vendored style has a landuse fill layer");
        assert!(landuse_filter.uses_zoom());
    }

    #[test]
    fn uses_zoom_is_false_for_a_container_whose_children_are_all_static() {
        // The mirror of `uses_zoom_propagates_out_of_every_container_variant`:
        // that test only ever asserts `true`, so a container arm that always
        // returns `true` regardless of its children (e.g. `Filter::All(_) =>
        // true`) would pass every existing test while classifying nearly
        // every real Mapbox Streets layer -- almost all of which are wrapped
        // in `["all", ...]` -- as dynamic. That would silently disable the
        // zoom-independent-work cache entirely: green tests, no error, no
        // symptom except nothing getting faster. This test asserts the
        // negative direction so that regression is impossible to miss.
        let static_leaf = || Filter::Equals { key: "class".to_string(), value: "park".to_string() };
        assert!(!Filter::All(vec![static_leaf(), static_leaf()]).uses_zoom(), "all");
        assert!(!Filter::AnyOf(vec![static_leaf(), static_leaf()]).uses_zoom(), "any");
        assert!(!Filter::Not(Box::new(static_leaf())).uses_zoom(), "not");
        assert!(!Filter::Case {
            arms: vec![(static_leaf(), static_leaf())],
            fallback: Box::new(static_leaf()),
        }.uses_zoom(), "case");
        assert!(!Filter::MultiMatch {
            key: "class".to_string(),
            cases: vec![(vec!["park".to_string()], static_leaf())],
            fallback: Box::new(static_leaf()),
        }.uses_zoom(), "multi-match");
        assert!(!Filter::NumberMultiMatch {
            key: "level".to_string(),
            cases: vec![(vec![89.0], static_leaf())],
            fallback: Box::new(static_leaf()),
        }.uses_zoom(), "number multi-match");
    }

    #[test]
    fn the_real_building_filter_is_classified_static() {
        // The mirror of `the_real_landuse_filter_is_classified_dynamic`, in
        // the other direction: an end-to-end guard that a real layer whose
        // filter is genuinely zoom-free actually classifies static, so the
        // cache this method exists for actually turns on. Mapbox Streets'
        // `building` fill layer's real filter is `["all", ["!=", ["get",
        // "type"], "building:part"], ["==", ["get", "underground"],
        // "false"]]` -- no zoom input anywhere -- which compiles to
        // `Filter::All([Filter::Not(Filter::Equals{..}), Filter::Equals{..}])`,
        // exercising `All` and `Not` wrapping only static leaves, the exact
        // shape most likely to be silently misclassified (see
        // `uses_zoom_is_false_for_a_container_whose_children_are_all_static`).
        let bytes = mapbox_fixture_or_skip!("mapbox-streets-style.json");
        let style = crate::style_spec::parse_and_compile(&bytes).expect("Mapbox Streets style must compile cleanly");
        let building_filter = style
            .layers
            .iter()
            .find_map(|layer| match layer {
                StyleLayer::Fill { id, filter, .. } if id == "building" => Some(filter),
                _ => None,
            })
            .expect("vendored style has a building fill layer");
        assert!(!building_filter.uses_zoom());
    }

    #[test]
    fn match_tile_emits_background_regardless_of_tile_content() {
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Background {
                id: "test-layer".to_string(),
                color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile(&DecodedTile { data_hash: 0, layers: vec![] }, &style, 5);
        assert_eq!(plan.ops.len(), 1);
        assert!(matches!(plan.ops[0], PaintOp::Background { .. }));
    }

    #[test]
    fn match_tile_emits_fill_for_matching_features_only() {
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "test-layer".to_string(),
                source_layer: "landuse".to_string(),
                filter: Filter::Equals { key: "class".to_string(), value: "water".to_string() },
                color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }),
                opacity: ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile(&tile_with_one_water_feature(), &style, 5);
        assert_eq!(plan.ops.len(), 1);
        assert!(matches!(plan.ops[0], PaintOp::Fill { .. }));
    }

    #[test]
    fn match_tile_skips_non_matching_features_silently() {
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "test-layer".to_string(),
                source_layer: "landuse".to_string(),
                filter: Filter::Equals { key: "class".to_string(), value: "forest".to_string() },
                color: ZoomValue::constant(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
                opacity: ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile(&tile_with_one_water_feature(), &style, 5);
        assert_eq!(plan.ops.len(), 0);
    }

    #[test]
    fn boundary_line_keeps_a_ring_with_no_tile_clip_edges_untouched() {
        // A polygon entirely inside the tile: no edge runs along the
        // 0/extent boundary, so the whole ring should survive as one line.
        let ring = LineString::from(vec![(100, 100), (300, 100), (300, 300), (100, 300), (100, 100)]);
        let geometry = Geometry::Polygon(Polygon::new(ring.clone(), vec![]));
        let result = boundary_geometry_for_line(&geometry);
        match result {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 1);
                assert_eq!(mls.0[0], ring);
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn boundary_line_drops_the_edge_that_hugs_the_tile_clip_boundary() {
        // A ring shaped like a water polygon clipped against the right
        // edge of the tile (x == TILE_EXTENT): two real corners, one edge
        // that hugs the clip boundary, and two more real edges.
        let ring = LineString::from(vec![
            (100, 50),
            (TILE_EXTENT, 50),
            (TILE_EXTENT, 300),
            (200, 300),
            (100, 50),
        ]);
        let geometry = Geometry::Polygon(Polygon::new(ring, vec![]));
        let result = boundary_geometry_for_line(&geometry);
        match result {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 1, "the seam edge should be dropped, not split into two runs");
                let kept = &mls.0[0];
                // The clip-hugging edge from (4096,50) to (4096,300) must be
                // gone; every remaining coordinate must be real geometry.
                assert!(!kept
                    .lines()
                    .any(|line| line.start.x == line.end.x && line.start.x >= TILE_EXTENT));
                // The two real corners on the clip boundary are still kept
                // as endpoints (only the edge *between* them is dropped).
                assert!(kept.coords().any(|c| *c == (TILE_EXTENT, 300).into()));
                assert!(kept.coords().any(|c| *c == (TILE_EXTENT, 50).into()));
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn boundary_line_drops_a_ring_that_is_entirely_the_tile_clip_rectangle() {
        // A tile fully covered by water/landcover: the whole ring is the
        // clip rectangle, so there's no real boundary to draw at all.
        let ring = LineString::from(vec![
            (0, 0),
            (TILE_EXTENT, 0),
            (TILE_EXTENT, TILE_EXTENT),
            (0, TILE_EXTENT),
            (0, 0),
        ]);
        let geometry = Geometry::Polygon(Polygon::new(ring, vec![]));
        let result = boundary_geometry_for_line(&geometry);
        match result {
            Geometry::MultiLineString(mls) => assert!(mls.0.is_empty()),
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn boundary_line_handles_buffered_clip_edges_beyond_the_nominal_extent() {
        // Real OpenFreeMap tiles clip polygons against a buffered extent
        // (observed at TILE_EXTENT +/- 64, not exactly 0/TILE_EXTENT).
        // Those buffered seam edges must be dropped too.
        let buffered = TILE_EXTENT + 64;
        let ring = LineString::from(vec![
            (100, 50),
            (buffered, 50),
            (buffered, 300),
            (200, 300),
            (100, 50),
        ]);
        let geometry = Geometry::Polygon(Polygon::new(ring, vec![]));
        let result = boundary_geometry_for_line(&geometry);
        match result {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 1);
                assert!(!mls.0[0]
                    .lines()
                    .any(|line| line.start.x == line.end.x && line.start.x >= TILE_EXTENT));
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn clip_ring_to_tile_extent_leaves_a_fully_interior_ring_unchanged() {
        let points = vec![
            geo_types::coord! { x: 1000, y: 1000 },
            geo_types::coord! { x: 3000, y: 1000 },
            geo_types::coord! { x: 3000, y: 3000 },
            geo_types::coord! { x: 1000, y: 3000 },
        ];
        let clipped = clip_ring_to_tile_extent(&points, 4096);
        assert_eq!(clipped, points, "a ring fully inside the extent should be untouched");
    }

    #[test]
    fn clip_ring_to_tile_extent_clips_a_ring_that_overshoots_past_the_buffer() {
        // A square whose right edge overshoots the tile extent by 64 units
        // (the real observed buffer amount) -- must be clipped flush to
        // extent (4096), not left overshooting.
        let points = vec![
            geo_types::coord! { x: 3000, y: 1000 },
            geo_types::coord! { x: 4160, y: 1000 },
            geo_types::coord! { x: 4160, y: 3000 },
            geo_types::coord! { x: 3000, y: 3000 },
        ];
        let clipped = clip_ring_to_tile_extent(&points, 4096);
        assert!(
            clipped.iter().all(|c| c.x <= 4096),
            "no clipped point should exceed the tile extent, got {clipped:?}"
        );
        assert!(
            clipped.iter().any(|c| c.x == 4096),
            "the clip should introduce a point exactly at the extent boundary, got {clipped:?}"
        );
    }

    #[test]
    fn clip_ring_to_tile_extent_drops_a_ring_entirely_outside() {
        let points = vec![
            geo_types::coord! { x: 5000, y: 5000 },
            geo_types::coord! { x: 6000, y: 5000 },
            geo_types::coord! { x: 6000, y: 6000 },
            geo_types::coord! { x: 5000, y: 6000 },
        ];
        let clipped = clip_ring_to_tile_extent(&points, 4096);
        assert!(clipped.is_empty(), "a ring entirely past the extent should clip to nothing, got {clipped:?}");
    }

    #[test]
    fn match_tile_clips_fill_geometry_overshooting_the_tile_buffer_to_the_extent() {
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "test-layer".to_string(),
                source_layer: "landcover".to_string(),
                filter: Filter::Any,
                color: ZoomValue::constant(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
                opacity: ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("wood".to_string()));
        // A polygon whose exterior overshoots the tile extent by 64 units
        // on the right, simulating real MVT buffer geometry.
        let overshooting = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (3000, 1000), (4160, 1000), (4160, 3000), (3000, 3000), (3000, 1000),
            ]),
            vec![],
        ));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landcover".to_string(),
                features: vec![Feature { geometry: overshooting, tags: tags.into() }],
            }],
        };
        let plan = match_tile(&tile, &style, 10);
        let PaintOp::Fill { geometry, .. } = &plan.ops[0] else {
            panic!("expected a Fill op");
        };
        let Geometry::Polygon(p) = geometry.as_ref() else {
            panic!("expected a Polygon geometry, got {geometry:?}");
        };
        assert!(
            p.exterior().coords().all(|c| c.x <= TILE_EXTENT),
            "match_tile's Fill geometry should be clipped to the tile extent, got {:?}",
            p.exterior()
        );
    }

    /// **NO CLIPPED FILL VERTEX ESCAPES `0..=TILE_EXTENT`, ON ANY AXIS, FOR
    /// ANY SHAPE A TILER CAN ENCODE.**
    ///
    /// This is the property that makes a tile seam impossible to
    /// double-composite. Two horizontally adjacent tiles put their shared
    /// edge at `x == TILE_EXTENT` and `x == 0` respectively, and those two
    /// lines are the SAME line in world space; two vertically adjacent ones
    /// do the same in `y`. So the only way both tiles' fills can cover one
    /// screen pixel is for one of them to carry geometry strictly outside
    /// its own `0..=TILE_EXTENT` box -- which is exactly what an MVT
    /// buffer is (observed at `TILE_EXTENT ± 64` on real OpenFreeMap
    /// tiles), and exactly what `clip_fill_geometry_to_tile_extent` is
    /// there to remove.
    ///
    /// The shapes below are the ones a per-ring Sutherland-Hodgman clipper
    /// can get wrong: a ring that overruns all four sides at once, a
    /// concave ring that leaves and re-enters the box (which makes the
    /// clipper insert edges that hug the clip rectangle), a ring wholly
    /// outside, one whose edge lies exactly ON a boundary, and a polygon
    /// with a hole that itself overruns. The assertion is on every
    /// coordinate of every ring of every output.
    #[test]
    fn a_clipped_fill_never_leaves_its_own_tile_box() {
        let b = 64; // the buffer real tilers encode past the extent
        let e = TILE_EXTENT;
        let ring = |pts: Vec<(i32, i32)>| LineString::from(pts);
        let cases: Vec<(&str, Geometry<i32>)> = vec![
            (
                "overruns all four sides",
                Geometry::Polygon(Polygon::new(
                    ring(vec![(-b, -b), (e + b, -b), (e + b, e + b), (-b, e + b), (-b, -b)]),
                    vec![],
                )),
            ),
            (
                "concave, leaves and re-enters through the east edge",
                Geometry::Polygon(Polygon::new(
                    ring(vec![
                        (1000, 1000),
                        (e + b, 1200),
                        (3000, 2000),
                        (e + b, 2800),
                        (1000, 3000),
                        (1000, 1000),
                    ]),
                    vec![],
                )),
            ),
            (
                "an edge lying exactly on the east boundary",
                Geometry::Polygon(Polygon::new(
                    ring(vec![(2000, 1000), (e, 1000), (e, 3000), (2000, 3000), (2000, 1000)]),
                    vec![],
                )),
            ),
            (
                "wholly past the south-east corner",
                Geometry::Polygon(Polygon::new(
                    ring(vec![
                        (e + 10, e + 10),
                        (e + b, e + 10),
                        (e + b, e + b),
                        (e + 10, e + b),
                        (e + 10, e + 10),
                    ]),
                    vec![],
                )),
            ),
            (
                "a hole that overruns the north edge",
                Geometry::Polygon(Polygon::new(
                    ring(vec![(-b, -b), (e + b, -b), (e + b, e + b), (-b, e + b), (-b, -b)]),
                    vec![ring(vec![
                        (1000, -b),
                        (3000, -b),
                        (3000, 2000),
                        (1000, 2000),
                        (1000, -b),
                    ])],
                )),
            ),
            (
                "a multipolygon of two parts, each overrunning a different side",
                Geometry::MultiPolygon(geo_types::MultiPolygon::new(vec![
                    Polygon::new(
                        ring(vec![(-b, 100), (2000, 100), (2000, 900), (-b, 900), (-b, 100)]),
                        vec![],
                    ),
                    Polygon::new(
                        ring(vec![
                            (2000, e - 900),
                            (e + b, e - 900),
                            (e + b, e + b),
                            (2000, e + b),
                            (2000, e - 900),
                        ]),
                        vec![],
                    ),
                ])),
            ),
        ];
        for (name, geometry) in cases {
            let clipped = clip_fill_geometry_to_tile_extent(&geometry);
            let polygons: Vec<&Polygon<i32>> = match &clipped {
                Geometry::Polygon(p) => vec![p],
                Geometry::MultiPolygon(mp) => mp.iter().collect(),
                other => panic!("{name}: a fill clip must answer a polygon, got {other:?}"),
            };
            for polygon in polygons {
                for ring in
                    std::iter::once(polygon.exterior()).chain(polygon.interiors().iter())
                {
                    for c in ring.coords() {
                        assert!(
                            (0..=e).contains(&c.x) && (0..=e).contains(&c.y),
                            "{name}: clipped vertex {c:?} is outside 0..={e} -- an \
                             adjacent tile owns that ground and would composite it a \
                             second time"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn match_tile_line_layer_over_a_polygon_source_omits_seam_edges() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("water".to_string()));
        let ring = LineString::from(vec![
            (100, 50),
            (TILE_EXTENT, 50),
            (TILE_EXTENT, 300),
            (200, 300),
            (100, 50),
        ]);
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "water".to_string(),
                features: vec![Feature { geometry: Geometry::Polygon(Polygon::new(ring, vec![])), tags: tags.into() }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Line {
                id: "test-layer".to_string(),
                source_layer: "water".to_string(),
                filter: Filter::Any,
                color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                width: ZoomValue::constant(1.0),
                opacity: ZoomValue::constant(1.0),
                gap_width: ZoomValue::constant(0.0),
                cap: LineCap::Butt,
                join: LineJoin::Miter,
                dash: ZoomValue::constant(DashPattern::none()),
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile(&tile, &style, 5);
        assert_eq!(plan.ops.len(), 1);
        match &plan.ops[0] {
            PaintOp::Line { geometry, .. } => match geometry.as_ref() {
                Geometry::MultiLineString(mls) => {
                    assert_eq!(mls.0.len(), 1);
                    assert!(!mls.0[0]
                        .lines()
                        .any(|line| line.start.x == line.end.x && line.start.x >= TILE_EXTENT));
                }
                other => panic!("expected a Line op with MultiLineString geometry, got {other:?}"),
            },
            other => panic!("expected a Line op with MultiLineString geometry, got {other:?}"),
        }
    }

    #[test]
    fn match_tile_line_layer_over_a_point_source_produces_no_geometry() {
        // Real MVT POINT features decode to `MultiPoint` (see
        // `match_tile_symbols_places_a_symbol_at_every_point_of_a_multipoint`).
        // A `Line` layer pointed at a source layer that happens to carry
        // point features must contribute nothing -- `boundary_geometry_for_line`
        // is the enforcement point for that invariant.
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("water".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "points".to_string(),
                features: vec![Feature {
                    geometry: Geometry::MultiPoint(geo_types::MultiPoint::new(vec![geo_types::Point::new(
                        100, 100,
                    )])),
                    tags: tags.into(),
                }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Line {
                id: "test-layer".to_string(),
                source_layer: "points".to_string(),
                filter: Filter::Any,
                color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                width: ZoomValue::constant(1.0),
                opacity: ZoomValue::constant(1.0),
                gap_width: ZoomValue::constant(0.0),
                cap: LineCap::Butt,
                join: LineJoin::Miter,
                dash: ZoomValue::constant(DashPattern::none()),
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile(&tile, &style, 5);
        assert_eq!(plan.ops.len(), 1);
        match &plan.ops[0] {
            PaintOp::Line { geometry, .. } => match geometry.as_ref() {
                Geometry::MultiLineString(mls) => {
                    assert!(mls.0.is_empty(), "a point feature must not draw a dot through a Line layer")
                }
                other => panic!("expected a Line op with an empty MultiLineString geometry, got {other:?}"),
            },
            other => panic!("expected a Line op, got {other:?}"),
        }
    }

    fn fill_style_layer(id: &str, source_layer: &str, filter: Filter, color: Rgba) -> StyleLayer {
        StyleLayer::Fill {
            id: id.to_string(),
            source_layer: source_layer.to_string(),
            filter,
            color: ZoomValue::constant(color),
            opacity: ZoomValue::constant(0.6),
            outline_color: None,
            min_zoom: None,
            max_zoom: None,
        }
    }

    fn square_polygon_with_hole() -> Geometry<i32> {
        // A 0..100 square with a 40..60 square hole in the middle.
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)]),
            vec![LineString::from(vec![(40, 40), (60, 40), (60, 60), (40, 60), (40, 40)])],
        ))
    }

    #[test]
    fn layers_at_point_matches_a_point_inside_a_simple_fill_polygon() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("national_park".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landuse_overlay".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        };
        let color = Rgba { r: 0.72, g: 0.87, b: 0.69, a: 1.0 };
        let style = Style {
            sprite: None,
            layers: vec![fill_style_layer("national-park", "landuse_overlay", Filter::Any, color)],
            sky: Default::default(),
        };

        let hits = layers_at_point(&tile, &style, 6, (5, 5));

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].layer_id, "national-park");
        assert_eq!(hits[0].source_layer, "landuse_overlay");
        assert_eq!(hits[0].resolved_color, color);
        assert!((hits[0].resolved_opacity - 0.6).abs() < 1e-6);
    }

    #[test]
    fn layers_at_point_finds_no_match_for_a_point_clearly_outside_the_polygon() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("national_park".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landuse_overlay".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![fill_style_layer(
                "national-park",
                "landuse_overlay",
                Filter::Any,
                Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
            )],
            sky: Default::default(),
        };

        let hits = layers_at_point(&tile, &style, 6, (500, 500));

        assert!(hits.is_empty());
    }

    #[test]
    fn layers_at_point_finds_no_match_for_a_point_inside_a_polygons_hole() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("national_park".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landuse_overlay".to_string(),
                features: vec![Feature { geometry: square_polygon_with_hole(), tags: tags.into() }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![fill_style_layer(
                "national-park",
                "landuse_overlay",
                Filter::Any,
                Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
            )],
            sky: Default::default(),
        };

        // (50, 50) is inside the outer square but inside the 40..60 hole too.
        let hits = layers_at_point(&tile, &style, 6, (50, 50));

        assert!(hits.is_empty());
    }

    #[test]
    fn layers_at_point_finds_no_match_when_the_filter_rejects_the_feature() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("forest".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landuse_overlay".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![fill_style_layer(
                "national-park",
                "landuse_overlay",
                Filter::Equals { key: "class".to_string(), value: "national_park".to_string() },
                Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 },
            )],
            sky: Default::default(),
        };

        // Geometrically inside, but the feature's tags don't satisfy the filter.
        let hits = layers_at_point(&tile, &style, 6, (5, 5));

        assert!(hits.is_empty());
    }

    #[test]
    fn layers_at_point_finds_no_match_when_the_queried_zoom_is_outside_the_layers_range() {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("national_park".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "landuse_overlay".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        };
        let StyleLayer::Fill { id, source_layer, filter, color, opacity, .. } =
            fill_style_layer("national-park", "landuse_overlay", Filter::Any, Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 })
        else {
            unreachable!()
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id,
                source_layer,
                filter,
                color,
                opacity,
                outline_color: None,
                min_zoom: Some(10),
                max_zoom: Some(14),
            }],
            sky: Default::default(),
        };

        // Geometrically inside, but zoom 6 is below the layer's min_zoom of 10.
        let hits = layers_at_point(&tile, &style, 6, (5, 5));

        assert!(hits.is_empty());
    }

    /// Point placement reads LINE geometry as well as point geometry,
    /// anchoring at the first vertex of each part -- MapLibre's rule (see
    /// `match_tile_symbols`' doc comment). It used to skip it, which is what
    /// made a shield layer written as `["step", ["zoom"], "point", 11,
    /// "line"]` draw nothing at all below zoom 11.
    #[test]
    fn match_tile_symbols_point_placement_anchors_line_geometry_at_each_parts_first_vertex() {
        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Kuala Lumpur".to_string()));
        let point_feature = Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(100, 200)), tags: tags.clone().into() };
        let line_feature = Feature {
            geometry: geo_types::Geometry::LineString(geo_types::LineString::new(vec![
                geo_types::Coord { x: 300, y: 400 },
                geo_types::Coord { x: 310, y: 410 },
            ])),
            tags: tags.clone().into(),
        };
        // Three parts, as a road split at its junctions arrives: one anchor
        // each, at each part's own start.
        let multiline_feature = Feature {
            geometry: geo_types::Geometry::MultiLineString(geo_types::MultiLineString::new(vec![
                geo_types::LineString::new(vec![geo_types::Coord { x: 500, y: 600 }, geo_types::Coord { x: 550, y: 600 }]),
                geo_types::LineString::new(vec![geo_types::Coord { x: 700, y: 800 }, geo_types::Coord { x: 750, y: 800 }]),
                geo_types::LineString::new(vec![geo_types::Coord { x: 900, y: 1000 }, geo_types::Coord { x: 950, y: 1000 }]),
            ])),
            tags: tags.clone().into(),
        };
        // Polygon geometry is still skipped: MapLibre labels it at the pole
        // of inaccessibility, which this crate has no equivalent of.
        let polygon_feature = Feature {
            geometry: geo_types::Geometry::Polygon(geo_types::Polygon::new(
                geo_types::LineString::new(vec![
                    geo_types::Coord { x: 1100, y: 1100 },
                    geo_types::Coord { x: 1200, y: 1100 },
                    geo_types::Coord { x: 1200, y: 1200 },
                    geo_types::Coord { x: 1100, y: 1100 },
                ]),
                vec![],
            )),
            tags: tags.into(),
        };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "place_label".to_string(),
                features: vec![point_feature, line_feature, multiline_feature, polygon_feature],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "place-label".to_string(),
                source_layer: "place_label".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile_symbols(&tile, &style, 10, 8.0);
        let anchors: Vec<(i32, i32)> = plan.features.iter().map(|f| (f.anchor.x(), f.anchor.y())).collect();
        assert_eq!(
            anchors,
            vec![(100, 200), (300, 400), (500, 600), (700, 800), (900, 1000)],
            "the point, the LineString's first vertex, and each MultiLineString part's first vertex -- \
             and nothing from the Polygon"
        );
        assert!(plan.features.iter().all(|f| f.text.as_deref() == Some("Kuala Lumpur")));
        assert!(
            plan.features.iter().all(|f| f.forward_ref.is_none()),
            "point placement carries no direction, whatever geometry it read"
        );
    }

    /// `text-anchor` reaches the draw loop the same way `text-offset` does:
    /// baked onto every `SymbolFeature`. The draw loop merges symbols from
    /// many tiles' plans and has no `StyleLayer` in hand by then, so a
    /// property left on the layer is a property it cannot read. Both
    /// placement arms are checked -- a line-placed shield needs its anchor
    /// as much as a POI does, and it would be easy to copy the field into
    /// only one of the two `SymbolFeature` literals.
    #[test]
    fn match_tile_symbols_bakes_the_layers_text_anchor_onto_every_feature() {
        fn anchored(id: &str, placement: SymbolPlacement, anchor: TextAnchor) -> StyleLayer {
            StyleLayer::Symbol {
                id: id.to_string(),
                source_layer: "poi".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.9),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(anchor),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(placement),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }
        }

        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Far East Plaza".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "poi".to_string(),
                features: vec![
                    Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(100, 200)), tags: tags.clone().into() },
                    Feature {
                        geometry: geo_types::Geometry::LineString(geo_types::LineString::new(vec![
                            geo_types::Coord { x: 0, y: 0 },
                            geo_types::Coord { x: 4000, y: 0 },
                        ])),
                        tags: tags.into(),
                    },
                ],
            }],
        };

        for (placement, anchor) in
            [(SymbolPlacement::Point, TextAnchor::Left), (SymbolPlacement::Line, TextAnchor::Bottom)]
        {
            let style = Style { sprite: None, layers: vec![anchored("poi", placement, anchor)], sky: Default::default() };
            let plan = match_tile_symbols(&tile, &style, 17, 8.0);
            assert!(!plan.features.is_empty(), "{placement:?} placement must produce a feature to check");
            for feature in &plan.features {
                assert_eq!(feature.text_anchor, anchor, "{placement:?}");
                // The sibling property, unchanged: the anchor is carried
                // ALONGSIDE the offset, not instead of it.
                assert_eq!(feature.text_offset, (0.9, 0.0), "{placement:?}");
            }
        }
    }

    /// `text-max-width` reaches the draw loop baked onto the feature, like
    /// `text-anchor` -- and, unlike it, only on POINT-placed symbols.
    ///
    /// That asymmetry is the style spec's, not a shortcut: the property's
    /// own requirement is `symbol-placement: point`, and mapbox-gl-js
    /// enforces it in `performSymbolLayout` by passing a max width of 0
    /// (its "no wrapping") for anything else. A street name repeated along
    /// a road is one line by construction; breaking it at 9 em would stack
    /// fragments across the carriageway.
    #[test]
    fn match_tile_symbols_bakes_text_max_width_onto_point_features_and_leaves_line_ones_unwrapped() {
        fn bounded(placement: SymbolPlacement, max_width: f32) -> StyleLayer {
            StyleLayer::Symbol {
                id: "poi".to_string(),
                source_layer: "poi".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: max_width,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(placement),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }
        }

        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Opposite Orchard Station Exit 12".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "poi".to_string(),
                features: vec![
                    Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(100, 200)), tags: tags.clone().into() },
                    Feature {
                        geometry: geo_types::Geometry::LineString(geo_types::LineString::new(vec![
                            geo_types::Coord { x: 0, y: 0 },
                            geo_types::Coord { x: 4000, y: 0 },
                        ])),
                        tags: tags.into(),
                    },
                ],
            }],
        };

        let point_style = Style { sprite: None, layers: vec![bounded(SymbolPlacement::Point, 9.0)], sky: Default::default() };
        let point_plan = match_tile_symbols(&tile, &point_style, 17, 8.0);
        assert!(!point_plan.features.is_empty(), "point placement must produce a feature to check");
        for feature in &point_plan.features {
            assert_eq!(feature.text_max_width_em, Some(9.0));
        }

        let line_style = Style { sprite: None, layers: vec![bounded(SymbolPlacement::Line, 9.0)], sky: Default::default() };
        let line_plan = match_tile_symbols(&tile, &line_style, 17, 8.0);
        assert!(!line_plan.features.is_empty(), "line placement must produce a feature to check");
        for feature in &line_plan.features {
            assert_eq!(feature.text_max_width_em, None, "a line-placed label does not wrap");
        }
    }

    #[test]
    fn match_tile_symbols_skips_a_feature_with_no_resolvable_text_or_icon() {
        let tags = HashMap::new(); // no "name" tag at all
        let point_feature = Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(1, 1)), tags: tags.into() };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer { name: "place_label".to_string(), features: vec![point_feature] }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "place-label".to_string(),
                source_layer: "place_label".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile_symbols(&tile, &style, 10, 8.0);
        assert!(plan.features.is_empty(), "a feature resolving neither text nor icon must be skipped");
    }

    #[test]
    fn match_tile_symbols_places_a_symbol_at_every_point_of_a_multipoint() {
        // This is the shape real tiles actually have: MVT's single POINT
        // geometry type decodes to `MultiPoint` via `mvt_reader`, even for a
        // lone point, so matching only `Geometry::Point` produced zero
        // symbols from every real Mapbox Streets tile.
        let mut tags = HashMap::new();
        tags.insert("house_num".to_string(), TagValue::String("63".to_string()));
        let single = Feature {
            geometry: geo_types::Geometry::MultiPoint(geo_types::MultiPoint::new(vec![geo_types::Point::new(5, 6)])),
            tags: tags.clone().into(),
        };
        let pair = Feature {
            geometry: geo_types::Geometry::MultiPoint(geo_types::MultiPoint::new(vec![
                geo_types::Point::new(10, 11),
                geo_types::Point::new(20, 21),
            ])),
            tags: tags.into(),
        };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer { name: "housenum_label".to_string(), features: vec![single, pair] }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "building-number-label".to_string(),
                source_layer: "housenum_label".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("house_num".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(10.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile_symbols(&tile, &style, 17, 8.0);
        let anchors: Vec<_> = plan.features.iter().map(|f| f.anchor).collect();
        assert_eq!(
            anchors,
            vec![geo_types::Point::new(5, 6), geo_types::Point::new(10, 11), geo_types::Point::new(20, 21)],
            "one symbol per point, in order"
        );
        assert!(plan.features.iter().all(|f| f.text.as_deref() == Some("63")));
    }

    #[test]
    fn match_tile_symbols_excludes_anchors_outside_the_tile_extent() {
        // Real vector tiles buffer point features slightly beyond the
        // tile's nominal [0, TILE_EXTENT) range, so a near-boundary point
        // can be present in two adjacent tiles' decoded data. Anchors
        // outside that range are the buffered duplicate and must be
        // dropped, or a label near a tile boundary renders twice, garbled
        // and overlapping -- this was confirmed live, not hypothetical.
        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Pulau Padang".to_string()));
        let feature = Feature {
            geometry: geo_types::Geometry::MultiPoint(geo_types::MultiPoint::new(vec![
                geo_types::Point::new(-50, 100),           // buffered duplicate: x below 0
                geo_types::Point::new(TILE_EXTENT + 50, 100), // buffered duplicate: x at/beyond TILE_EXTENT
                geo_types::Point::new(100, 200),           // within [0, TILE_EXTENT): the real anchor
            ])),
            tags: tags.into(),
        };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer { name: "place_label".to_string(), features: vec![feature] }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "place-label".to_string(),
                source_layer: "place_label".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let plan = match_tile_symbols(&tile, &style, 10, 8.0);
        assert_eq!(
            plan.features.iter().map(|f| f.anchor).collect::<Vec<_>>(),
            vec![geo_types::Point::new(100, 200)],
            "only the in-range anchor should survive -- the out-of-range ones are buffered duplicates"
        );
    }

    #[test]
    fn match_tile_symbols_resolves_icon_size_per_zoom() {
        let mut tags = HashMap::new();
        tags.insert("maki".to_string(), TagValue::String("cafe".to_string()));
        let point_feature = Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(1, 1)), tags: tags.into() };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer { name: "poi_label".to_string(), features: vec![point_feature] }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "poi-label".to_string(),
                source_layer: "poi_label".to_string(),
                filter: Filter::Any,
                text_field: None,
                icon_image: Some(IconImage::Get("maki".to_string())),
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::ZoomStep(ZoomValue::stepped(vec![(0, 0.25), (17, 0.75)])),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        assert_eq!(match_tile_symbols(&tile, &style, 16, 8.0).features[0].icon_size_from, 0.25);
        assert_eq!(match_tile_symbols(&tile, &style, 18, 8.0).features[0].icon_size_from, 0.75);
    }

    /// The two halves of a symbol's opacity are INDEPENDENT and are baked
    /// onto the feature separately: Mapbox Standard's `transit-stop-label`
    /// fades its text in a zoom before its bus icons, so a symbol really can
    /// have one visible and the other not.
    /// The anchor is resolved PER FEATURE and PER ZOOM at bake time, and the
    /// feature carries the keyword it landed on -- the draw loop merges
    /// symbols from many tiles and has no `StyleLayer` in hand.
    #[test]
    fn match_tile_symbols_resolves_a_data_driven_text_anchor_per_feature() {
        let feature_with = |rank: f64| {
            let mut tags = HashMap::new();
            tags.insert("name".to_string(), TagValue::String("Kopitiam".to_string()));
            tags.insert("sizerank".to_string(), TagValue::Number(rank));
            Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(1, 1)), tags: tags.into() }
        };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "poi_label".to_string(),
                features: vec![feature_with(6.0), feature_with(20.0)],
            }],
        };
        // poi-label's real shape: center below the threshold, top at or
        // above it, with the threshold tightening at z17.
        let anchor = TextAnchorValue::ZoomOfDataStep(vec![
            (
                0,
                DataAnchorStep {
                    key: "sizerank".to_string(),
                    base: TextAnchor::Center,
                    stops: vec![(5.0, TextAnchor::Top)],
                },
            ),
            (
                17,
                DataAnchorStep {
                    key: "sizerank".to_string(),
                    base: TextAnchor::Center,
                    stops: vec![(13.0, TextAnchor::Top)],
                },
            ),
        ]);
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "poi-label".to_string(),
                source_layer: "poi_label".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::ZoomOfDataRamp(vec![(
                    0,
                    DataRampShape { key: "sizerank".to_string(), stops: vec![(0.0, 0.0), (13.0, 0.75)] },
                )]),
                text_anchor: anchor,
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };

        let at = |zoom| {
            let plan = match_tile_symbols(&tile, &style, zoom, 8.0);
            plan.features.iter().map(|f| (f.text_anchor, f.text_offset.1)).collect::<Vec<_>>()
        };
        // z16 uses the 5-threshold: both features are past it.
        assert_eq!(at(16)[0].0, TextAnchor::Top);
        assert_eq!(at(16)[1].0, TextAnchor::Top);
        // z17 uses the 13-threshold: sizerank 6 falls back to centre while
        // sizerank 20 still hangs below.
        assert_eq!(at(17)[0].0, TextAnchor::Center);
        assert_eq!(at(17)[1].0, TextAnchor::Top);
        // And the offset ramps with the same property, independently.
        assert!((at(17)[0].1 - 0.75 * 6.0 / 13.0).abs() < 1e-6, "got {}", at(17)[0].1);
        assert!((at(17)[1].1 - 0.75).abs() < 1e-6, "clamped above the last stop");
    }

    #[test]
    fn match_tile_symbols_bakes_the_icon_and_text_opacities_independently() {
        let mut tags = HashMap::new();
        tags.insert("maki".to_string(), TagValue::String("cafe".to_string()));
        tags.insert("name".to_string(), TagValue::String("Kopitiam".to_string()));
        let point_feature = Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(1, 1)), tags: tags.into() };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer { name: "poi_label".to_string(), features: vec![point_feature] }],
        };
        let ramp = |first_visible_zoom: u8| SymbolOpacity {
            cases: vec![(Filter::Any, ZoomValue::stepped(vec![(0, 0.0), (first_visible_zoom, 1.0)]))],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "transit-stop-label".to_string(),
                source_layer: "poi_label".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: Some(IconImage::Get("maki".to_string())),
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: ramp(17),
                text_opacity: ramp(16),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let at = |zoom| {
            let plan = match_tile_symbols(&tile, &style, zoom, 8.0);
            let f = &plan.features[0];
            (f.icon_opacity, f.text_opacity)
        };
        assert_eq!(at(15), (0.0, 0.0), "both halves hidden below either ramp");
        assert_eq!(at(16), (0.0, 1.0), "the text arrives a zoom before the icon");
        assert_eq!(at(17), (1.0, 1.0));
    }

    #[test]
    fn resolve_pair_brackets_an_interpolated_layout_value_and_freezes_a_stepped_one() {
        // Mapbox evaluates `icon-size` at the map's CONTINUOUS zoom
        // (`src__symbol__symbol_size.ts`'s `evaluateSizeForZoom`), so a
        // caller needs both ends of the bucket to blend between -- exactly
        // what the paint side's `ZoomValue::value_at_pair` already hands it.
        let tags = HashMap::new();
        // `road`'s real curve, resampled at every integer zoom by the
        // expression compiler, so `value_at(15)` and `value_at(16)` are the
        // two ends of the bucket the camera is inside at z15.4.
        let curve = LayoutValue::ZoomStep(ZoomValue::interpolated(vec![(14, 0.122), (15, 0.226), (16, 0.383)]));
        let (from, to) = curve.resolve_pair(&tags, 15);
        assert!((from - 0.226).abs() < 1e-6, "the near end must be the value at the camera's own bucket, got {from}");
        assert!((to - 0.383).abs() < 1e-6, "the far end must be the value one zoom up, got {to}");

        // A `step` has a hard cutoff at each stop and real Mapbox never
        // interpolates one, so both ends must be the SAME value.
        let stepped = LayoutValue::ZoomStep(ZoomValue::stepped(vec![(0, 1.0), (15, 0.9)]));
        assert_eq!(stepped.resolve_pair(&tags, 14), (1.0, 1.0), "a step must not blend across its own cutoff");
        assert_eq!(stepped.resolve_pair(&tags, 15), (0.9, 0.9));
        assert_eq!(LayoutValue::Constant(2.0).resolve_pair(&tags, 7), (2.0, 2.0));
    }

    #[test]
    fn match_tile_symbols_bakes_the_icon_size_bracket_not_one_snapped_value() {
        // The bake happens once per integer zoom bucket, but the camera
        // moves continuously inside it, so the plan has to carry BOTH ends
        // for the draw loop to blend -- a single value snapped at the
        // bucket's floor is what made a symbol hold one size for a whole
        // zoom level and then jump.
        let mut tags = HashMap::new();
        tags.insert("maki".to_string(), TagValue::String("cafe".to_string()));
        let point_feature = Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(1, 1)), tags: tags.into() };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer { name: "poi_label".to_string(), features: vec![point_feature] }],
        };
        let mut style = symbol_style_with_icon_size(LayoutValue::ZoomStep(ZoomValue::interpolated(vec![
            (14, 0.122),
            (15, 0.226),
            (16, 0.383),
        ])));
        let feature = &match_tile_symbols(&tile, &style, 15, 8.0).features[0];
        assert!((feature.icon_size_from - 0.226).abs() < 1e-6, "got {}", feature.icon_size_from);
        assert!((feature.icon_size_to - 0.383).abs() < 1e-6, "got {}", feature.icon_size_to);

        // A stepped `icon-size` -- poi-label's real one -- must bake a
        // FROZEN pair, so the draw loop's blend is a no-op on it.
        style = symbol_style_with_icon_size(LayoutValue::ZoomStep(ZoomValue::stepped(vec![(0, 1.0), (15, 0.9)])));
        let feature = &match_tile_symbols(&tile, &style, 14, 8.0).features[0];
        assert_eq!((feature.icon_size_from, feature.icon_size_to), (1.0, 1.0));
    }

    /// A one-symbol-layer style over the `poi_label` source layer whose only
    /// interesting property is `icon-size`. Shared by the bracket tests
    /// above, which differ in nothing else.
    fn symbol_style_with_icon_size(icon_size: LayoutValue) -> Style {
        Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "poi-label".to_string(),
                source_layer: "poi_label".to_string(),
                filter: Filter::Any,
                text_field: None,
                icon_image: Some(IconImage::Get("maki".to_string())),
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size,
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        }
    }

    #[test]
    fn match_tile_symbols_respects_min_zoom() {
        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Singapore".to_string()));
        let point_feature = Feature { geometry: geo_types::Geometry::Point(geo_types::Point::new(1, 1)), tags: tags.into() };
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer { name: "place_label".to_string(), features: vec![point_feature] }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "place-label".to_string(),
                source_layer: "place_label".to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: Some(10),
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        assert!(match_tile_symbols(&tile, &style, 5, 8.0).features.is_empty(), "zoom 5 is below min_zoom 10");
        assert_eq!(match_tile_symbols(&tile, &style, 10, 8.0).features.len(), 1);
    }

    /// The collision pass merges symbols from many tiles' plans into ONE
    /// placement pass, so by the time it sorts them the style layer they
    /// came from is long out of reach -- it has to be baked onto the feature
    /// here, next to every other resolved symbol property.
    #[test]
    fn match_tile_symbols_bakes_the_style_layer_index_and_the_resolved_sort_key_onto_every_feature() {
        fn symbol_layer(id: &str, source_layer: &str, sort_key: Option<SymbolSortKey>) -> StyleLayer {
            StyleLayer::Symbol {
                id: id.to_string(),
                source_layer: source_layer.to_string(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".to_string())),
                icon_image: None,
                text_size: LayoutValue::Constant(14.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: sort_key,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                text_halo_width: LayoutValue::Constant(1.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Point),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
                min_zoom: None,
                max_zoom: None,
            }
        }

        let mut country_tags = HashMap::new();
        country_tags.insert("name".to_string(), TagValue::String("Malaysia".to_string()));
        let mut city_tags = HashMap::new();
        city_tags.insert("name".to_string(), TagValue::String("Kota Bharu".to_string()));
        city_tags.insert("symbolrank".to_string(), TagValue::Number(11.0));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![
                Layer {
                    name: "place_label".to_string(),
                    features: vec![Feature {
                        geometry: geo_types::Geometry::Point(geo_types::Point::new(10, 10)),
                        tags: country_tags.into(),
                    }],
                },
                Layer {
                    name: "settlement".to_string(),
                    features: vec![Feature {
                        geometry: geo_types::Geometry::Point(geo_types::Point::new(20, 20)),
                        tags: city_tags.into(),
                    }],
                },
            ],
        };
        // Two layers, and the one that reads a sort key is deliberately
        // SECOND so the test would still catch a `layer_index` hardcoded to
        // zero.
        let style = Style {
            sprite: None,
            layers: vec![
                symbol_layer("country-label", "place_label", None),
                symbol_layer("settlement-major-label", "settlement", Some(SymbolSortKey::Get("symbolrank".to_string()))),
            ],
            sky: Default::default(),
        };

        let plan = match_tile_symbols(&tile, &style, 10, 8.0);
        assert_eq!(plan.features.len(), 2);
        let country = plan.features.iter().find(|f| f.text.as_deref() == Some("Malaysia")).expect("country label");
        assert_eq!(country.layer_index, 0);
        assert_eq!(country.sort_key, 0.0, "a layer with no symbol-sort-key bakes the property's zero default");
        let city = plan.features.iter().find(|f| f.text.as_deref() == Some("Kota Bharu")).expect("city label");
        assert_eq!(city.layer_index, 1);
        assert_eq!(city.sort_key, 11.0, "[\"get\", \"symbolrank\"] must resolve against THIS feature's tags");
    }

    /// A one-layer style whose single symbol layer draws the `"arrow"` icon
    /// over the `"road"` source layer, with the placement under test.
    fn road_icon_style(placement: SymbolPlacement) -> Style {
        Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "arrow".into(),
                source_layer: "road".into(),
                filter: Filter::Any,
                text_field: None,
                icon_image: Some(IconImage::Literal("arrow".into())),
                text_size: LayoutValue::Constant(16.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                text_halo_width: LayoutValue::Constant(0.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                min_zoom: None,
                max_zoom: None,
                placement: SymbolPlacementValue::Constant(placement),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: match placement {
                    SymbolPlacement::Line | SymbolPlacement::LineCenter => RotationAlignment::Map,
                    SymbolPlacement::Point => RotationAlignment::Viewport,
                },
                // Mirrored from `rotation_alignment`, which is what the spec's
                // `auto` does under each placement. Tests that want a
                // line-placed LABEL out of this fixture fill in `text_field`
                // themselves and get the `Map` alignment real road-name
                // layers carry.
                text_rotation_alignment: match placement {
                    SymbolPlacement::Line | SymbolPlacement::LineCenter => RotationAlignment::Map,
                    SymbolPlacement::Point => RotationAlignment::Viewport,
                },
                icon_text_fit: Default::default(),
                bypass_collision: false,
            }],
            sky: Default::default(),
        }
    }

    fn decoded_road_tile_with_one_point() -> DecodedTile {
        DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature {
                    // MultiPoint, not Point: `mvt::decode` hands back every
                    // point feature as a MultiPoint (see `match_tile_symbols`).
                    geometry: geo_types::Geometry::MultiPoint(geo_types::MultiPoint::new(vec![
                        geo_types::Point::new(2000, 2000),
                    ])),
                    tags: Default::default(),
                }],
            }],
        }
    }

    fn decoded_road_tile_with_one_line() -> DecodedTile {
        DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (4000, 100)])),
                    tags: Default::default(),
                }],
            }],
        }
    }

    #[test]
    fn a_line_layer_places_one_feature_per_anchor_along_the_geometry() {
        let mut style = road_icon_style(SymbolPlacement::Line);
        let StyleLayer::Symbol { icon_rotate_deg, bypass_collision, .. } = &mut style.layers[0] else {
            unreachable!("road_icon_style always returns a single Symbol layer")
        };
        *icon_rotate_deg = 180.0;
        *bypass_collision = true;
        // A 4000-unit horizontal road; 250 px at 8 units/px is 2000 units, so
        // anchors land at 1000 and 3000.
        let decoded = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".into(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (4000, 100)])),
                    tags: Default::default(),
                }],
            }],
        };

        let plan = match_tile_symbols(&decoded, &style, 16, 8.0);

        assert_eq!(plan.features.len(), 2);
        assert_eq!(plan.features[0].anchor, geo_types::Point::new(1000, 100));
        assert_eq!(plan.features[1].anchor, geo_types::Point::new(3000, 100));
        for feature in &plan.features {
            assert_eq!(feature.icon.as_deref(), Some("arrow"));
            assert_eq!(feature.icon_rotate_deg, 180.0);
            assert!(feature.bypass_collision);
            let forward = feature.forward_ref.expect("a line-placed feature carries a forward reference");
            assert!(forward.x() > feature.anchor.x(), "forward runs with the geometry");
        }
    }

    /// One tile holding a single multi-segment `LineString` road, matched
    /// against a two-layer style -- one `symbol-placement: line`, one point
    /// -- with the resulting symbols split by the layer they came from.
    ///
    /// One tile is enough for both paths because point placement over LINE
    /// geometry anchors at the first vertex of each part (see
    /// `match_tile_symbols`), so the same feature feeds both layers and the
    /// only difference between the two halves is the placement.
    fn symbol_features_for_placement_test() -> (Vec<SymbolFeature>, Vec<SymbolFeature>) {
        let mut layers = road_icon_style(SymbolPlacement::Line).layers;
        let StyleLayer::Symbol { id, text_field, .. } = &mut layers[0] else {
            unreachable!("road_icon_style always returns a single Symbol layer")
        };
        *id = "road-name".into();
        // A street name: the case `SymbolFeature::line` exists for.
        *text_field = Some(TextField::Get("name".to_string()));
        layers.extend(road_icon_style(SymbolPlacement::Point).layers);

        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Omotesando".to_string()));
        // 6000 units of road in three 2000-unit segments; 250 px at 8
        // units/px is 2000 units of spacing, so anchors land at 1000, 3000
        // and 5000 -- three of them, on ONE part.
        let decoded = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".into(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                        (0, 100),
                        (2000, 100),
                        (2000, 2100),
                        (4000, 2100),
                    ])),
                    tags: tags.into(),
                }],
            }],
        };

        let plan = match_tile_symbols(&decoded, &road_style(layers), 16, 8.0);
        let (line_features, point_features): (Vec<_>, Vec<_>) =
            plan.features.into_iter().partition(|f| f.layer_index == 0);
        assert!(line_features.len() >= 2, "the fixture must produce several anchors on one line");
        assert_eq!(point_features.len(), 1, "point placement anchors at the line's first vertex");
        (line_features, point_features)
    }

    fn road_style(layers: Vec<StyleLayer>) -> Style {
        Style { layers, sprite: None, sky: Default::default() }
    }

    #[test]
    fn line_placed_symbols_carry_their_line_and_point_placed_ones_do_not() {
        // The line is what `symbol_line::place_glyphs_along` needs and what
        // `forward_ref` alone cannot supply: a forward reference gives ONE
        // direction at the anchor, and a curved label needs the geometry
        // either side of it.
        let (line_features, point_features) = symbol_features_for_placement_test();
        assert!(
            line_features.iter().all(|f| f.line.is_some()),
            "every line-placed symbol must carry its polyline"
        );
        assert!(
            point_features.iter().all(|f| f.line.is_none()),
            "point placement has no line, and must not pay for one"
        );
        // The carried line is the geometry itself, in the anchor's own
        // space -- not a resampled or re-derived one.
        let carried = line_features[0].line.as_ref().expect("line placement carries a line");
        assert_eq!(
            carried.0,
            geo_types::LineString::from(vec![(0, 100), (2000, 100), (2000, 2100), (4000, 2100)]).0,
            "the line must arrive verbatim, tile-local and already clipped"
        );
        // Anchors on ONE line share ONE allocation.
        let a = line_features[0].line.as_ref().expect("line placement carries a line");
        let b = line_features[1].line.as_ref().expect("line placement carries a line");
        assert!(
            std::sync::Arc::ptr_eq(a, b),
            "anchors from the same polyline must SHARE it, not clone it per anchor"
        );
    }

    #[test]
    fn a_multi_part_feature_gives_each_anchor_the_part_it_sits_on() {
        // The `MultiLineString` case, which is most real roads.
        // `symbol-spacing` is measured across the WHOLE feature (see
        // `symbol_line::line_anchors_across`) but a label bends along ONE
        // polyline, so an anchor's `line` must be the part it was walked on
        // -- not the whole geometry, and not always part 0.
        //
        // The first part is DEGENERATE (a single repeated point). It
        // contributes no length and carries no anchor, and it is here
        // because the walk compacts such parts out of its own working
        // vector: an implementation that indexed into the compacted list
        // would hand every anchor the wrong part, off by exactly the number
        // of degenerate parts before it.
        let mut layers = road_icon_style(SymbolPlacement::Line).layers;
        let StyleLayer::Symbol { text_field, .. } = &mut layers[0] else {
            unreachable!("road_icon_style always returns a single Symbol layer")
        };
        *text_field = Some(TextField::Get("name".to_string()));

        let dead = geo_types::LineString::from(vec![(50, 50), (50, 50)]);
        let west = geo_types::LineString::from(vec![(0, 100), (3000, 100)]);
        let east = geo_types::LineString::from(vec![(0, 900), (3000, 900)]);
        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Route 246".to_string()));
        let decoded = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".into(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::MultiLineString(geo_types::MultiLineString::new(vec![
                        dead,
                        west.clone(),
                        east.clone(),
                    ])),
                    tags: tags.into(),
                }],
            }],
        };

        // 2000 units of spacing over 6000 units of feature: anchors at
        // 1000 and 3000 and 5000. 1000 is inside `west`; 3000 lands exactly
        // on its end and so resolves at `east`'s start (the walk's `>=`
        // rule); 5000 is 2000 into `east`.
        let plan = match_tile_symbols(&decoded, &road_style(layers), 16, 8.0);
        assert_eq!(plan.features.len(), 3);
        let carried: Vec<&geo_types::LineString<i32>> = plan
            .features
            .iter()
            .map(|f| f.line.as_deref().expect("every line-placed symbol carries its polyline"))
            .collect();
        assert_eq!(carried[0], &west, "the first anchor sits on the first non-degenerate part");
        assert_eq!(carried[1], &east, "an anchor past the first part's end belongs to the next part");
        assert_eq!(carried[2], &east);
        // Two anchors on `east`, one allocation between them; and `west`'s
        // is a different one.
        let lines: Vec<&std::sync::Arc<geo_types::LineString<i32>>> =
            plan.features.iter().map(|f| f.line.as_ref().unwrap()).collect();
        assert!(std::sync::Arc::ptr_eq(lines[1], lines[2]), "one part, one allocation");
        assert!(!std::sync::Arc::ptr_eq(lines[0], lines[1]), "different parts are different allocations");
        // Each anchor lies on the part it was given, which the y
        // coordinates make unambiguous here.
        for (feature, line) in plan.features.iter().zip(&carried) {
            assert_eq!(
                feature.anchor.y(),
                line.0[0].y,
                "the anchor must lie on the part it carries"
            );
        }
    }

    #[test]
    fn the_footprint_weighs_a_shared_line_once_and_not_once_per_anchor() {
        // `symbol_cache` is byte-budgeted and this is its weight function.
        // Line placement emits one feature per anchor over one shared
        // `Arc`, so charging the polyline per feature would inflate the
        // plan by the anchor count and evict for memory nothing holds --
        // the defect `memory_ceiling.accounted_ratio` exists to catch.
        let (line_features, point_features) = symbol_features_for_placement_test();
        let anchors = line_features.len();
        assert!(anchors >= 3, "the point of this test is several anchors over one line");

        // What one feature's own strings weigh -- the only thing besides
        // the line and the `Vec` that this walk charges.
        let strings = |f: &SymbolFeature| {
            f.text.as_ref().map_or(0, |t| crate::render::heap_block_bytes(t.capacity()))
                + f.icon.as_ref().map_or(0, |i| crate::render::heap_block_bytes(i.capacity()))
        };
        let vec_bytes = |plan: &SymbolPlan| crate::render::vec_block_bytes::<SymbolFeature>(plan.features.capacity());

        let all = SymbolPlan { features: line_features.clone() };
        let one = SymbolPlan { features: line_features[..1].to_vec() };
        // The same one-anchor plan with the line taken off it, which is
        // what the line itself weighs.
        let mut bare = line_features[..1].to_vec();
        bare[0].line = None;
        let bare = SymbolPlan { features: bare };
        let line_block = one.footprint_bytes() - bare.footprint_bytes();
        assert!(line_block > 0, "the shared line must be charged at all");

        // Going from one anchor to N over the SAME line adds the extra
        // features and their strings, and NOT the line again -- a
        // per-feature charge would be `(anchors - 1) * line_block` over
        // this.
        let growth = all.footprint_bytes() - one.footprint_bytes();
        let expected: usize = all.features[1..].iter().map(strings).sum::<usize>() + vec_bytes(&all) - vec_bytes(&one);
        assert_eq!(growth, expected, "adding anchors over one shared line must not re-charge the line");

        // And point placement, which carries no line, is unchanged.
        let points = SymbolPlan { features: point_features };
        assert_eq!(
            points.footprint_bytes(),
            vec_bytes(&points) + points.features.iter().map(strings).sum::<usize>(),
            "a point plan's weight must not have moved"
        );
    }

    #[test]
    fn an_icon_image_that_resolves_to_the_empty_string_is_no_icon_at_all() {
        // `examples/liberty-style.json`'s own idiom for "no icon above this
        // zoom": `["step", ["zoom"], "circle_11_black", 10, ""]`. The empty
        // string is not the name of a sprite and never can be, so it must
        // arrive at the symbol pass as `None` -- otherwise every such
        // feature costs an atlas lookup that misses on every frame, and the
        // name turns up in `symbol_pass::label_census::missing_icons` as a
        // sprite the sheet is missing, which is a diagnostic pointing at
        // nothing.
        let mut style = road_icon_style(SymbolPlacement::Point);
        let StyleLayer::Symbol { icon_image, .. } = &mut style.layers[0] else {
            panic!("road_icon_style builds a symbol layer")
        };
        *icon_image = Some(IconImage::Literal(String::new()));
        let plan = match_tile_symbols(&decoded_road_tile_with_one_point(), &style, 16, 8.0);
        // The feature is still dropped or kept on its own merits -- what is
        // asserted is only that it never carries an empty icon NAME.
        for feature in &plan.features {
            assert_eq!(feature.icon, None, "an empty icon-image must not become Some(\"\")");
        }
    }

    #[test]
    fn a_point_layer_is_unchanged_and_carries_no_forward_reference() {
        // Guards every existing symbol: the point path must not acquire
        // placement behaviour.
        let style = road_icon_style(SymbolPlacement::Point);
        let plan = match_tile_symbols(&decoded_road_tile_with_one_point(), &style, 16, 8.0);
        assert_eq!(plan.features.len(), 1);
        assert_eq!(plan.features[0].forward_ref, None);
        assert_eq!(plan.features[0].icon_rotate_deg, 0.0);
        assert!(!plan.features[0].bypass_collision);
    }

    #[test]
    fn a_point_layer_with_allow_overlap_and_ignore_placement_bypasses_collision_on_its_features() {
        // A deliberate behaviour change on this branch, not an incidental
        // one: Mapbox Streets' real `crosswalks` and `level-crossing` layers
        // are exactly this shape -- `icon-allow-overlap: true` AND
        // `icon-ignore-placement: true` on a POINT-placed layer (no
        // `symbol-placement` at all) -- and their icons now draw regardless
        // of collision. Compiles the real style_spec path (not a
        // hand-built `StyleLayer`) so this pins the actual
        // `icon-allow-overlap && icon-ignore-placement` derivation, then
        // follows it all the way through `match_tile_symbols` to prove the
        // flag survives onto the matched `SymbolFeature`.
        let layer_json = serde_json::json!({
            "id": "crosswalks-like",
            "type": "symbol",
            "source-layer": "road",
            "layout": {
                "icon-image": "arrow",
                "icon-allow-overlap": true,
                "icon-ignore-placement": true,
            },
        });
        let compiled = crate::style_spec::compile_symbol_layer(&layer_json, crate::style_spec::expression::Scope::root()).expect("should compile");
        assert_eq!(compiled.len(), 1);
        let StyleLayer::Symbol { placement, bypass_collision, .. } = &compiled[0] else {
            unreachable!("compile_symbol_layer always returns Symbol layers");
        };
        assert_eq!(placement.at(0), SymbolPlacement::Point, "absent symbol-placement defaults to point");
        assert!(*bypass_collision, "icon-allow-overlap && icon-ignore-placement must compile to bypass_collision: true");

        let style = Style { layers: compiled, sprite: None, sky: Default::default() };
        let plan = match_tile_symbols(&decoded_road_tile_with_one_point(), &style, 16, 8.0);
        assert_eq!(plan.features.len(), 1);
        assert!(plan.features[0].bypass_collision, "the flag must survive onto the matched SymbolFeature");
    }

    #[test]
    fn a_line_layer_ignores_point_geometry_but_a_point_layer_reads_lines() {
        // A line-placed layer over point geometry is still a skip: there is
        // no polyline to walk.
        let line_style = road_icon_style(SymbolPlacement::Line);
        let point_tile = decoded_road_tile_with_one_point();
        assert!(match_tile_symbols(&point_tile, &line_style, 16, 8.0).features.is_empty());

        // The other direction is NOT a skip, and used to be. A point-placed
        // layer over line geometry anchors at the line's first vertex, as
        // MapLibre does -- see `match_tile_symbols`' doc comment for why the
        // silent drop cost every shield below a `["step", ["zoom"], "point",
        // 11, "line"]` layer's step zoom.
        let point_style = road_icon_style(SymbolPlacement::Point);
        let line_tile = decoded_road_tile_with_one_line();
        let plan = match_tile_symbols(&line_tile, &point_style, 16, 8.0);
        assert_eq!(plan.features.len(), 1, "one shield for the line, at its start");
        assert_eq!(plan.features[0].anchor, geo_types::Point::new(0, 100));
    }

    /// The spacing accumulator runs along the whole feature, so a
    /// `MultiLineString` is spaced as one road. Here the two parts are
    /// 4000 units each against a 2000-unit spacing, so the count matches
    /// what per-part walking gave -- what this pins is that the anchors sit
    /// at feature-wide distances 1000/3000/5000/7000, i.e. the second part's
    /// pair is offset by the first part's leftover rather than restarting.
    #[test]
    fn a_multilinestring_is_spaced_as_one_feature_not_once_per_part() {
        let style = road_icon_style(SymbolPlacement::Line);
        let decoded = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".into(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::MultiLineString(geo_types::MultiLineString::new(vec![
                        geo_types::LineString::from(vec![(0, 0), (4000, 0)]),
                        geo_types::LineString::from(vec![(0, 500), (4000, 500)]),
                    ])),
                    tags: Default::default(),
                }],
            }],
        };
        let plan = match_tile_symbols(&decoded, &style, 16, 8.0);
        let at: Vec<(i32, i32)> = plan.features.iter().map(|f| (f.anchor.x(), f.anchor.y())).collect();
        assert_eq!(at, vec![(1000, 0), (3000, 0), (1000, 500), (3000, 500)]);

        // Now two parts of 900 and 700 units -- 1600 for the feature, under
        // one 2000-unit spacing. Per-part walking gave one midpoint anchor
        // to EACH part; the feature gets one, at distance 800 along the
        // feature, which lands inside the first part.
        let short = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".into(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::MultiLineString(geo_types::MultiLineString::new(vec![
                        geo_types::LineString::from(vec![(0, 0), (900, 0)]),
                        geo_types::LineString::from(vec![(0, 500), (700, 500)]),
                    ])),
                    tags: Default::default(),
                }],
            }],
        };
        let plan = match_tile_symbols(&short, &style, 16, 8.0);
        assert_eq!(plan.features.len(), 1, "one label for the feature, not one per part");
        assert_eq!(plan.features[0].anchor, geo_types::Point::new(800, 0), "the FEATURE's midpoint");
    }

    #[test]
    fn a_map_aligned_line_layer_with_a_text_field_and_no_icon_still_produces_labels() {
        // The street-name case, and the one this engine used to draw
        // nothing for: `symbol-placement: line`, a `text-field`, no icon,
        // and `text-rotation-alignment: map`. That is the exact shape of
        // Liberty's `highway-name-major` and of Mapbox Streets'
        // `road-label`, so a drop here is every road name on the map -- in
        // every script, which is why it read as a font bug. See
        // `match_tile_symbols`'s doc comment.
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "road-label".into(),
                source_layer: "road".into(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".into())),
                icon_image: None,
                text_size: LayoutValue::Constant(16.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                text_halo_width: LayoutValue::Constant(0.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                min_zoom: None,
                max_zoom: None,
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Line),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Map,
                text_rotation_alignment: RotationAlignment::Map,
                icon_text_fit: Default::default(),
                bypass_collision: false,
            }],
            sky: Default::default(),
        };
        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Jalan Ampang".to_string()));
        let decoded = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (4000, 100)])),
                    tags: tags.into(),
                }],
            }],
        };

        let plan = match_tile_symbols(&decoded, &style, 16, 8.0);
        assert!(!plan.features.is_empty(), "a line-placed layer with only a text-field must still place labels");
        for feature in &plan.features {
            assert_eq!(feature.text.as_deref(), Some("Jalan Ampang"));
            assert_eq!(feature.icon, None, "there is no icon-image to resolve");
        }
    }

    /// The bake path is script-blind, and this pins that it stays so.
    ///
    /// The `MAP_LINE_TEXT` drop this replaced was reported as "the engine
    /// cannot render Japanese", because the only labels a bare OpenMapTiles
    /// extract carries are road names and road names are line-placed. A
    /// Japanese name and a Latin one have to come out of `match_tile_symbols`
    /// with the same shape -- same anchor count, same `Some(name)` -- or the
    /// drop has grown back somewhere script-sensitive. Whether the resulting
    /// glyphs find a covering FACE is a separate question, settled in
    /// `font_fallback`.
    #[test]
    fn a_japanese_street_name_survives_line_placement_exactly_as_a_latin_one_does() {
        let mut style = road_icon_style(SymbolPlacement::Line);
        let StyleLayer::Symbol { text_field, icon_image, .. } = &mut style.layers[0] else {
            unreachable!("road_icon_style builds a Symbol layer");
        };
        *text_field = Some(TextField::Get("name".into()));
        // Text-only, the real shape of `highway-name-major`: no icon to
        // keep the feature alive if the text were dropped.
        *icon_image = None;

        let named = |name: &str| DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (4000, 100)])),
                    tags: (HashMap::from([("name".to_string(), TagValue::String(name.to_string()))])).into(),
                }],
            }],
        };

        for name in ["Orchard Road", "国道２４６号線", "외곽순환로", "شارع النيل", "Λεωφόρος", "Тверская"] {
            let plan = match_tile_symbols(&named(name), &style, 16, 8.0);
            assert!(!plan.features.is_empty(), "{name:?} produced no symbol at all");
            for feature in &plan.features {
                assert_eq!(feature.text.as_deref(), Some(name), "{name:?} lost its text on the bake path");
            }
        }
    }

    /// A road cut into two vector-tile FEATURES that touch end to end --
    /// exactly the "one OSM way per city block" shape real transportation
    /// data has -- comes out of `match_tile_symbols` as ONE merged line, not
    /// two. `road_icon_style`'s `spacing_px` is 250, so at `tile_units_per_px
    /// = 8.0` a spacing of 2000 tile units: each 1000/1500-unit block is
    /// SHORTER than that alone (and would get its own single
    /// short-line-midpoint anchor if walked separately -- two features, two
    /// labels), but the 2500-unit combined line is LONGER than one spacing
    /// (one anchor, one label). This is `symbol_line::merge_line_candidates`
    /// wired into the bake path (see `match_tile_symbols`'s own doc comment)
    /// -- MapLibre's `mergeLines`, the reason a named road chopped into
    /// short blocks still gets a name.
    #[test]
    fn two_touching_same_named_line_features_bake_into_one_merged_symbol_feature() {
        let mut style = road_icon_style(SymbolPlacement::Line);
        let StyleLayer::Symbol { text_field, icon_image, .. } = &mut style.layers[0] else {
            unreachable!("road_icon_style builds a Symbol layer");
        };
        *text_field = Some(TextField::Get("name".into()));
        *icon_image = None;

        let name = |n: &str| HashMap::from([("name".to_string(), TagValue::String(n.to_string()))]);
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![
                    Feature {
                        geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (1000, 100)])),
                        tags: (name("Beach Road")).into(),
                    },
                    Feature {
                        geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(1000, 100), (2500, 100)])),
                        tags: (name("Beach Road")).into(),
                    },
                ],
            }],
        };

        let plan = match_tile_symbols(&tile, &style, 16, 8.0);
        assert_eq!(
            plan.features.len(),
            1,
            "two touching blocks of the same named road must merge into one label, not carry one each: {:?}",
            plan.features.iter().map(|f| f.anchor).collect::<Vec<_>>()
        );
        let line = plan.features[0].line.as_ref().expect("a line-placed feature always carries its polyline");
        assert_eq!(line.0.len(), 3, "the shared joint point is not duplicated in the merged line");
    }

    /// Two carriageways of a dual carriageway -- the SAME road name on two
    /// separate, PARALLEL vector-tile features that never touch an endpoint
    /// (so `merge_line_candidates` cannot join them) -- must not both place
    /// their name when the resulting anchors land close together: real
    /// device evidence showed "Beach Road" drawn twice, side by side, once
    /// per carriageway. This is `symbol_line::anchor_too_close`
    /// (MapLibre's `anchorIsTooClose`) wired into the bake path.
    #[test]
    fn two_close_parallel_same_named_line_features_place_the_name_only_once() {
        let mut style = road_icon_style(SymbolPlacement::Line);
        let StyleLayer::Symbol { text_field, icon_image, .. } = &mut style.layers[0] else {
            unreachable!("road_icon_style builds a Symbol layer");
        };
        *text_field = Some(TextField::Get("name".into()));
        *icon_image = None;

        let name = |n: &str| HashMap::from([("name".to_string(), TagValue::String(n.to_string()))]);
        // Each carriageway is independently longer than one `symbol-spacing`
        // (2000 tile units at this style's 250px * 8.0 tile_units_per_px),
        // so each gets its own anchor at its own midpoint if walked alone --
        // (1000, 100) and (1000, 150), fifty tile units apart.
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![
                    Feature {
                        geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (2500, 100)])),
                        tags: (name("Beach Road")).into(),
                    },
                    Feature {
                        geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 150), (2500, 150)])),
                        tags: (name("Beach Road")).into(),
                    },
                ],
            }],
        };

        let plan = match_tile_symbols(&tile, &style, 16, 8.0);
        assert_eq!(
            plan.features.len(),
            1,
            "the second carriageway's anchor is within half a spacing of the first's and must be refused: {:?}",
            plan.features.iter().map(|f| f.anchor).collect::<Vec<_>>()
        );
    }

    /// The dedup in the test above is a NEAR-repeat rule, not a "one label
    /// per text per tile" rule: two same-named features far enough apart
    /// that they are obviously different stretches of road (or a
    /// coincidentally reused name) both keep their label.
    #[test]
    fn two_far_apart_same_named_line_features_both_keep_their_label() {
        let mut style = road_icon_style(SymbolPlacement::Line);
        let StyleLayer::Symbol { text_field, icon_image, .. } = &mut style.layers[0] else {
            unreachable!("road_icon_style builds a Symbol layer");
        };
        *text_field = Some(TextField::Get("name".into()));
        *icon_image = None;

        let name = |n: &str| HashMap::from([("name".to_string(), TagValue::String(n.to_string()))]);
        // 1500 tile units apart -- well past `anchor_too_close`'s 1000-unit
        // repeat distance at this style's spacing.
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![
                    Feature {
                        geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (2500, 100)])),
                        tags: (name("Beach Road")).into(),
                    },
                    Feature {
                        geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 1600), (2500, 1600)])),
                        tags: (name("Beach Road")).into(),
                    },
                ],
            }],
        };

        let plan = match_tile_symbols(&tile, &style, 16, 8.0);
        assert_eq!(plan.features.len(), 2, "two genuinely separate stretches of the same name must not suppress each other");
    }

    /// The real-world shape a `2026-09-22` headless screenshot against
    /// `tiles.openfreemap.org`'s actual `transportation_name` layer showed
    /// still duplicating "Beach Road" even after the two tests above passed:
    /// OpenFreeMap (planetiler) pre-merges same-named connected ways into
    /// ONE `MultiLineString` feature per carriageway SERVER-SIDE, so each of
    /// a dual carriageway's two carriageways reaches `match_tile_symbols`
    /// as an ALREADY-multi-part feature -- the exact branch that does not
    /// go through `merge_line_candidates` (see that arm's own doc comment).
    /// If that branch also skipped `anchor_too_close`, the one case the
    /// dedup exists for would be exempt from it. It must not be.
    #[test]
    fn two_close_parallel_multi_part_same_named_line_features_place_the_name_only_once() {
        let mut style = road_icon_style(SymbolPlacement::Line);
        let StyleLayer::Symbol { text_field, icon_image, .. } = &mut style.layers[0] else {
            unreachable!("road_icon_style builds a Symbol layer");
        };
        *text_field = Some(TextField::Get("name".into()));
        *icon_image = None;

        let name = |n: &str| HashMap::from([("name".to_string(), TagValue::String(n.to_string()))]);
        // Each carriageway is a two-part `MultiLineString` (a planetiler-merged
        // road, split at some tile-internal vertex) rather than the
        // single-`LineString` shape the tests above use, so this exercises
        // the "already a MultiLineString" arm specifically.
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![
                    Feature {
                        geometry: geo_types::Geometry::MultiLineString(geo_types::MultiLineString::new(vec![
                            geo_types::LineString::from(vec![(0, 100), (1200, 100)]),
                            geo_types::LineString::from(vec![(1200, 100), (2500, 100)]),
                        ])),
                        tags: (name("Beach Road")).into(),
                    },
                    Feature {
                        geometry: geo_types::Geometry::MultiLineString(geo_types::MultiLineString::new(vec![
                            geo_types::LineString::from(vec![(0, 150), (1200, 150)]),
                            geo_types::LineString::from(vec![(1200, 150), (2500, 150)]),
                        ])),
                        tags: (name("Beach Road")).into(),
                    },
                ],
            }],
        };

        let plan = match_tile_symbols(&tile, &style, 16, 8.0);
        assert_eq!(
            plan.features.len(),
            1,
            "two already-multi-part carriageways fifty tile units apart must still dedup: {:?}",
            plan.features.iter().map(|f| f.anchor).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_map_aligned_line_layer_with_both_text_field_and_icon_carries_both() {
        // `text_rotation_alignment: Map` no longer costs the layer its text.
        // The engine still draws that text horizontally at the anchor rather
        // than along the curve -- per-glyph curve-following placement is the
        // outstanding gap -- but that is the same thing the `Viewport` arm
        // draws, so the two alignments now resolve identically here. The
        // `Viewport` case is covered by
        // `a_viewport_aligned_line_layer_with_both_text_field_and_icon_carries_both`
        // below and by `style_spec::tests::the_real_shield_text_and_icon_both_resolve_at_point_and_line_placement`.
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "road-label".into(),
                source_layer: "road".into(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("name".into())),
                icon_image: Some(IconImage::Literal("arrow".into())),
                text_size: LayoutValue::Constant(16.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                text_halo_width: LayoutValue::Constant(0.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                min_zoom: None,
                max_zoom: None,
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Line),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Map,
                text_rotation_alignment: RotationAlignment::Map,
                icon_text_fit: Default::default(),
                bypass_collision: false,
            }],
            sky: Default::default(),
        };
        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Jalan Ampang".to_string()));
        let decoded = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (4000, 100)])),
                    tags: tags.into(),
                }],
            }],
        };

        let plan = match_tile_symbols(&decoded, &style, 16, 8.0);
        assert_eq!(plan.features.len(), 2);
        for feature in &plan.features {
            assert_eq!(feature.text.as_deref(), Some("Jalan Ampang"), "Map alignment no longer drops line-placed text");
            assert_eq!(feature.icon.as_deref(), Some("arrow"));
        }
    }

    #[test]
    fn a_viewport_aligned_line_layer_with_both_text_field_and_icon_carries_both() {
        // The shield case: `text_rotation_alignment: Viewport` means the
        // text is a horizontal label at the line anchor, not curve-following
        // -- in scope, and resolved right alongside the icon. This is the
        // synthetic-fixture sibling of
        // `style_spec::tests::the_real_shield_text_and_icon_both_resolve_at_point_and_line_placement`,
        // which exercises the same behaviour through the real compiled
        // `road-number-shield` layer.
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Symbol {
                id: "road-number-shield".into(),
                source_layer: "road".into(),
                filter: Filter::Any,
                text_field: Some(TextField::Get("ref".into())),
                icon_image: Some(IconImage::Literal("rectangle-blue-3".into())),
                text_size: LayoutValue::Constant(9.0),
                text_offset_x: LayoutValue::Constant(0.0),
                text_radial_offset: None,
                text_offset_y: LayoutValue::Constant(0.0),
                text_anchor: TextAnchorValue::Constant(TextAnchor::Center),
                text_max_width: DEFAULT_TEXT_MAX_WIDTH_EM,
                icon_size: LayoutValue::Constant(1.0),
                symbol_sort_key: None,
                text_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                text_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                text_halo_width: LayoutValue::Constant(0.0),
                text_halo_blur: LayoutValue::Constant(0.0),
                text_font: crate::font_fallback::default_font_stack(),
                icon_opacity: SymbolOpacity::opaque(),
                text_opacity: SymbolOpacity::opaque(),
                icon_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                icon_halo_color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                icon_halo_width: LayoutValue::Constant(0.0),
                min_zoom: None,
                max_zoom: None,
                placement: SymbolPlacementValue::Constant(SymbolPlacement::Line),
                spacing_px: LayoutValue::Constant(250.0),
                icon_rotate_deg: 0.0,
                rotation_alignment: RotationAlignment::Viewport,
                text_rotation_alignment: RotationAlignment::Viewport,
                icon_text_fit: Default::default(),
                bypass_collision: false,
            }],
            sky: Default::default(),
        };
        let mut tags = HashMap::new();
        tags.insert("ref".to_string(), TagValue::String("AYE".to_string()));
        let decoded = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature {
                    geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![(0, 100), (4000, 100)])),
                    tags: tags.into(),
                }],
            }],
        };

        let plan = match_tile_symbols(&decoded, &style, 16, 8.0);
        assert_eq!(plan.features.len(), 2);
        for feature in &plan.features {
            assert_eq!(feature.text.as_deref(), Some("AYE"), "Viewport-aligned line-placed text must be resolved");
            assert_eq!(feature.icon.as_deref(), Some("rectangle-blue-3"));
        }
    }

    /// A decoded tile carrying features shaped to exercise real vendored
    /// layers with different `Filter::uses_zoom()` classifications: `water`
    /// (source-layer `water`, no filter at all -- statically `Filter::Any`),
    /// `landuse` (source-layer `landuse`, the real dynamic
    /// `NumberCompareWithZoomOffset`/`ZoomStep` filter recovered in
    /// 066cc5a), and `road-minor` (source-layer `road`, a dynamic filter
    /// whose matched branch for this feature -- `class: track` -- is itself
    /// zoom-free, so this also covers "classified dynamic but this
    /// particular match doesn't waver").
    fn real_shape_fixture_tile() -> DecodedTile {
        let mut landuse_tags = HashMap::new();
        landuse_tags.insert("class".to_string(), TagValue::String("wood".to_string()));
        landuse_tags.insert("sizerank".to_string(), TagValue::Number(1.0));

        let mut road_tags = HashMap::new();
        road_tags.insert("class".to_string(), TagValue::String("track".to_string()));
        road_tags.insert("structure".to_string(), TagValue::String("none".to_string()));

        DecodedTile {
            data_hash: 0,
            layers: vec![
                Layer {
                    name: "water".to_string(),
                    features: vec![Feature { geometry: square_polygon(), tags: Default::default() }],
                },
                Layer {
                    name: "landuse".to_string(),
                    features: vec![Feature { geometry: square_polygon(), tags: landuse_tags.into() }],
                },
                Layer {
                    name: "road".to_string(),
                    features: vec![Feature {
                        geometry: Geometry::LineString(LineString::from(vec![(0, 0), (20, 20)])),
                        tags: road_tags.into(),
                    }],
                },
            ],
        }
    }

    /// An independent copy of `match_tile`'s pre-split, single-pass logic,
    /// kept ONLY in this test module. Deliberately does not call
    /// `match_tile`, `match_tile_static`, or `resolve_plan`: now that
    /// `match_tile` is a thin wrapper over the other two
    /// (`resolve_plan(tile, style, &match_tile_static(tile, style), zoom)`),
    /// comparing `resolve_plan` against `match_tile` would just be comparing
    /// `resolve_plan` to itself -- tautological, and incapable of catching a
    /// bug the split itself introduced. This function is what
    /// `match_tile` looked like before Step 3 of this task, so a divergence
    /// here means `resolve_plan`/`match_tile_static` actually changed
    /// observable behavior.
    fn single_pass_reference_match_tile(tile: &DecodedTile, style: &Style, zoom: u8) -> PaintPlan {
        let mut ops = Vec::new();
        for layer in &style.layers {
            let (min_zoom, max_zoom) = layer_zoom_range(layer);
            if min_zoom.is_some_and(|mz| zoom < mz) || max_zoom.is_some_and(|mz| zoom > mz) {
                continue;
            }
            match layer {
                StyleLayer::Background { color, .. } => {
                    ops.push(PaintOp::Background { color: color.value_at(zoom) });
                }
                StyleLayer::Fill { source_layer, filter, color, opacity, min_zoom, max_zoom, .. } => {
                    let Some(tile_layer) = tile.layers.iter().find(|l| &l.name == source_layer) else {
                        continue;
                    };
                    let (fade_from, fade_to) = layer_zoom_fade_pair(*min_zoom, *max_zoom, zoom);
                    for feature in &tile_layer.features {
                        if filter.matches_geometry(&feature.tags, zoom as f64, GeometryKind::of(&feature.geometry)) {
                            let (color_from, color_to) = color.value_at_pair(zoom);
                            let (opacity_from, opacity_to) = opacity.value_at_pair(zoom);
                            let mut resolved_from = color_from;
                            resolved_from.a *= opacity_from * fade_from;
                            let mut resolved_to = color_to;
                            resolved_to.a *= opacity_to * fade_to;
                            ops.push(PaintOp::Fill {
                                color_from: resolved_from,
                                color_to: resolved_to,
                                geometry: Arc::new(clip_fill_geometry_to_tile_extent(&feature.geometry)),
                            });
                        }
                    }
                }
                StyleLayer::Line { source_layer, filter, color, width, opacity, gap_width, cap, join, dash, min_zoom, max_zoom, .. } => {
                    let Some(tile_layer) = tile.layers.iter().find(|l| &l.name == source_layer) else {
                        continue;
                    };
                    let (fade_from, fade_to) = layer_zoom_fade_pair(*min_zoom, *max_zoom, zoom);
                    for feature in &tile_layer.features {
                        if filter.matches_geometry(&feature.tags, zoom as f64, GeometryKind::of(&feature.geometry)) {
                            let (color_from, color_to) = color.value_at_pair(zoom);
                            let (opacity_from, opacity_to) = opacity.value_at_pair(zoom);
                            let mut resolved_from = color_from;
                            resolved_from.a *= opacity_from * fade_from;
                            let mut resolved_to = color_to;
                            resolved_to.a *= opacity_to * fade_to;
                            let (width_from, width_to) = width.value_at_pair(zoom);
                            let (gap_from, gap_to) = gap_width.value_at_pair(zoom);
                            ops.push(PaintOp::Line {
                                metrics: false,
                                color_from: resolved_from,
                                color_to: resolved_to,
                                width_from,
                                width_to,
                                gap_from,
                                gap_to,
                                cap: *cap,
                                join: *join,
                                dash: dash.value_at(zoom),
                                geometry: Arc::new(boundary_geometry_for_line(&feature.geometry)),
                            });
                        }
                    }
                }
                StyleLayer::Extrusion { source_layer, filter, group, color, opacity, height, base, .. } => {
                    let Some(tile_layer) = tile.layers.iter().find(|l| &l.name == source_layer) else {
                        continue;
                    };
                    if extrusion_mask_footprints(source_layer, opacity, zoom) {
                        for feature in &tile_layer.features {
                            let polygonal = matches!(feature.geometry, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
                            if polygonal && filter.matches_geometry(&feature.tags, zoom as f64, GeometryKind::of(&feature.geometry)) {
                                ops.push(mask_footprint_op(Arc::new(clip_fill_geometry_to_tile_extent(&feature.geometry))));
                            }
                        }
                    }
                    for feature in &tile_layer.features {
                        let polygonal = matches!(feature.geometry, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
                        if polygonal && filter.matches_geometry(&feature.tags, zoom as f64, GeometryKind::of(&feature.geometry)) {
                            let (color_from, color_to) = color.value_at_pair(zoom);
                            let (base_from, base_to) = base.resolve_pair(&feature.tags, zoom);
                            let (height_from, height_to) = height.resolve_pair(&feature.tags, zoom);
                            ops.push(PaintOp::Extrusion {
                                group: *group,
                                row: ExtrusionRow { color_from, color_to, base_from, height_from, base_to, height_to },
                                geometry: Arc::new(feature.geometry.clone()),
                            });
                        }
                    }
                }
                // Neither symbols nor raster contribute a `PaintOp` -- the
                // same arms `resolve_plan` itself has, for the same reasons.
                StyleLayer::Symbol { .. } | StyleLayer::Raster { .. } | StyleLayer::Circle { .. } => {}
            }
        }
        PaintPlan { ops }
    }

    /// A Streets-schema tile with one feature per ranked class and one
    /// that ranks `Land`: water, a park, a residential landuse, a building.
    fn mask_fixture_tile() -> DecodedTile {
        let tags = |pairs: &[(&str, &str)]| -> crate::mvt::Tags {
            let mut t = HashMap::new();
            for (k, v) in pairs {
                t.insert((*k).to_string(), TagValue::String((*v).to_string()));
            }
            t.insert("sizerank".to_string(), TagValue::Number(1.0));
            t.into()
        };
        let layer = |name: &str, features: Vec<crate::mvt::Tags>| Layer {
            name: name.to_string(),
            features: features.into_iter().map(|tags| Feature { geometry: square_polygon(), tags }).collect(),
        };
        DecodedTile {
            data_hash: 0,
            layers: vec![
                layer("water", vec![tags(&[])]),
                layer("landuse", vec![tags(&[("class", "park")]), tags(&[("class", "residential")])]),
                layer("building", vec![tags(&[("underground", "false"), ("type", "building")])]),
            ],
        }
    }

    /// `resolve_plan_with_mask` ends in `_ => unreachable!()`, so a
    /// `(StyleLayer, LayerMatch)` pair it has no arm for is a panic in a
    /// user's frame rather than a compile error. This pins the `Extrusion`
    /// arm on a synthetic style that splits at the floor and on Liberty.
    #[test]
    fn a_style_with_fill_extrusion_resolves_a_plan_without_panicking() {
        let synthetic = crate::style_spec::parse_and_compile(
            br##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},
            "layers":[{"id":"b","type":"fill-extrusion","source":"s","source-layer":"building","minzoom":10,
            "paint":{"fill-extrusion-color":"#888","fill-extrusion-height":["get","height"]}}]}"##,
        )
        .expect("compiles");
        let liberty = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").expect("vendored"))
            .expect("Liberty compiles");
        let tile = mask_fixture_tile();
        for style in [&synthetic, &liberty] {
            assert!(style.layers.iter().any(|l| matches!(l, StyleLayer::Extrusion { .. })));
            for masked in [false, true] {
                let m = match_tile_static_masked(&tile, style, masked);
                assert!(m.footprint_bytes() > 0);
                for zoom in [0u8, 10, 11, 12, 14, 16, 22] {
                    let _ = resolve_plan_with_mask(&tile, style, &m, zoom);
                }
            }
            let _ = single_pass_reference_match_tile(&tile, style, 16);
        }
        // The synthetic style's one building draws flat below the floor and
        // as an extrusion op above it, whose only flat footprint is the dot
        // mask's transparent one (final-review M1).
        let m = match_tile_static(&tile, &synthetic);
        assert!(!resolve_plan(&tile, &synthetic, &m, 11).ops.is_empty(), "the flat fallback draws at z11");
        assert!(
            resolve_plan(&tile, &synthetic, &m, 11).ops.iter().all(|op| matches!(op, PaintOp::Fill { .. })),
            "no extrusion below the floor"
        );
        let z16 = resolve_plan(&tile, &synthetic, &m, 16).ops;
        assert!(z16.iter().any(|op| matches!(op, PaintOp::Extrusion { .. })), "{z16:?}");
        assert!(
            z16.iter().all(|op| match op {
                PaintOp::Extrusion { .. } => true,
                PaintOp::Fill { color_from, color_to, .. } => color_from.a == 0.0 && color_to.a == 0.0,
                _ => false,
            }),
            "nothing visible drawn flat under the extrusion at z16: {z16:?}"
        );
    }

    /// Every coordinate of a polygonal geometry's rings, exterior and holes.
    fn geometry_coords(geometry: &geo_types::Geometry<i32>) -> impl Iterator<Item = (i32, i32)> + '_ {
        let polygons: Vec<&geo_types::Polygon<i32>> = match geometry {
            geo_types::Geometry::Polygon(p) => vec![p],
            geo_types::Geometry::MultiPolygon(mp) => mp.iter().collect(),
            _ => Vec::new(),
        };
        polygons
            .into_iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
            .flat_map(|ring| ring.coords().map(|c| (c.x, c.y)))
    }

    fn extrusion_ops(plan: &PaintPlan) -> Vec<PaintOp> {
        plan.ops.iter().filter(|op| matches!(op, PaintOp::Extrusion { .. })).cloned().collect()
    }

    /// The Ruling amending R19: Liberty's `building-3d` skips an outline
    /// OpenMapTiles flags `hide_3d` (its `building:part` pieces are mapped
    /// separately). On the Manhattan fixture that is the Empire State
    /// Building's whole-lot outline, 0 -> 444 m, which extruded as one slab
    /// over its own stepped parts; the 330 -> 444 m spire part must remain.
    #[test]
    fn liberty_does_not_extrude_a_hide_3d_outline_but_keeps_its_parts() {
        let tile = crate::mvt::decode(std::fs::read("benches/fixtures/manhattan_z14.pbf").unwrap()).unwrap();
        let rows = |style: &Style| -> Vec<(f32, f32)> {
            let m = match_tile_static(&tile, style);
            extrusion_ops(&resolve_plan(&tile, style, &m, 16))
                .iter()
                .map(|op| match op {
                    PaintOp::Extrusion { row, .. } => (row.base_from, row.height_from),
                    _ => unreachable!(),
                })
                .collect()
        };
        let liberty = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let kept = rows(&liberty);
        let is_slab = |&(base, height): &(f32, f32)| base == 0.0 && height >= 440.0;
        let is_spire = |&(base, height): &(f32, f32)| (base - 330.0).abs() < 1.0 && height >= 440.0;
        assert!(!kept.iter().any(is_slab), "the ESB's hide_3d outline is extruded");
        assert!(kept.iter().any(is_spire), "the ESB's 330 -> 444 m part must still be extruded");

        // The control: without the clause, the slab is there.
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let layer = json["layers"].as_array_mut().unwrap().iter_mut().find(|l| l["id"] == "building-3d").unwrap();
        layer.as_object_mut().unwrap().remove("filter");
        let upstream = crate::style_spec::parse_and_compile(serde_json::to_string(&json).unwrap().as_bytes()).unwrap();
        let all = rows(&upstream);
        assert!(all.iter().any(is_slab), "the fixture's ESB outline is the 0 -> 444 m slab");
        // Exactly the fixture's 12 hide_3d features go.
        assert_eq!(all.len() - kept.len(), 12, "{} of {} extrusions dropped", all.len() - kept.len(), all.len());
    }

    /// Liberty with `building-3d`'s filter swapped for one that reads the
    /// zoom but passes every feature from z0, so the layer matches through
    /// `ExtrusionMatch::Dynamic` and must still emit what `Static` does.
    fn liberty_with_a_zoom_filter_on_building_3d() -> Style {
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let layer = json["layers"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|l| l["id"] == "building-3d")
            .expect("Liberty has building-3d");
        // Liberty's own clause AND a zoom step that passes from z0.
        layer["filter"] = serde_json::json!(["all", ["!=", ["get", "hide_3d"], true], ["step", ["zoom"], false, 0, true]]);
        crate::style_spec::parse_and_compile(serde_json::to_string(&json).unwrap().as_bytes()).unwrap()
    }

    /// The fixture's `building` features Liberty's `building-3d` extrudes:
    /// every one but the `hide_3d` outlines (its filter, vendored Ruling).
    fn extrudable_buildings(tile: &crate::mvt::DecodedTile) -> usize {
        let layer = tile.layers.iter().find(|l| l.name == "building").unwrap();
        layer.features.iter().filter(|f| !matches!(f.tags.get("hide_3d"), Some(TagValue::Bool(true)))).count()
    }

    #[test]
    fn extrusion_static_dynamic_and_the_single_pass_reference_agree_on_liberty() {
        let liberty = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let dynamic = liberty_with_a_zoom_filter_on_building_3d();
        for fixture in ["shinjuku_z14", "manhattan_z14"] {
            let tile = crate::mvt::decode(std::fs::read(format!("benches/fixtures/{fixture}.pbf")).unwrap()).unwrap();
            let buildings = extrudable_buildings(&tile);
            let m_static = match_tile_static(&tile, &liberty);
            let m_dynamic = match_tile_static(&tile, &dynamic);
            assert!(
                m_dynamic.entries.iter().any(|e| matches!(e, LayerMatch::Extrusion(ExtrusionMatch::Dynamic))),
                "the zoom filter takes the Dynamic path"
            );
            assert!(!m_static.entries.iter().any(|e| matches!(e, LayerMatch::Extrusion(ExtrusionMatch::Dynamic))));
            for zoom in [14u8, 16] {
                let from_static = extrusion_ops(&resolve_plan(&tile, &liberty, &m_static, zoom));
                let from_dynamic = extrusion_ops(&resolve_plan(&tile, &dynamic, &m_dynamic, zoom));
                let reference = extrusion_ops(&single_pass_reference_match_tile(&tile, &liberty, zoom));
                assert_eq!(from_static.len(), buildings, "{fixture} z{zoom}");
                assert!(from_static == from_dynamic, "{fixture} z{zoom}: Static and Dynamic differ");
                assert!(from_static == reference, "{fixture} z{zoom}: Static and the reference differ");
            }
        }
    }

    #[test]
    fn liberty_building_3d_matches_one_extrusion_op_per_building_with_unclipped_geometry() {
        let style = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let tile = crate::mvt::decode(std::fs::read("benches/fixtures/shinjuku_z14.pbf").unwrap()).unwrap();
        let plan = match_tile(&tile, &style, 14);
        let ops: Vec<_> = plan.ops.iter().filter(|op| matches!(op, PaintOp::Extrusion { .. })).collect();
        let buildings = extrudable_buildings(&tile);
        assert_eq!(ops.len(), buildings, "one op per building Liberty's hide_3d clause keeps");
        assert!(ops.iter().any(|op| matches!(op, PaintOp::Extrusion { row, .. } if row.height_from > 100.0)), "Shinjuku has towers");
        // Unclipped: at least one building crosses the tile edge into the buffer.
        let crosses = ops.iter().any(|op| match op {
            PaintOp::Extrusion { geometry, .. } => geometry_coords(geometry).any(|(x, y)| !(0..=4096).contains(&x) || !(0..=4096).contains(&y)),
            _ => false,
        });
        assert!(crosses, "extrusion ops must keep the producer's buffer (walls are clipped later)");
    }

    fn streets_style() -> Option<Style> {
        let bytes = crate::mapbox_fixtures::read("mapbox-streets-style.json")?;
        Some(crate::style_spec::parse_and_compile(&bytes).expect("Mapbox Streets style must compile cleanly"))
    }

    #[test]
    fn resolve_plan_with_mask_ranks_only_fill_ops_and_matches_resolve_plan() {
        let style = or_skip!(streets_style());
        for tile in [mask_fixture_tile(), real_shape_fixture_tile()] {
            let m = match_tile_static_masked(&tile, &style, true);
            for zoom in [10u8, 14, 15, 17] {
                let (plan, mask) = resolve_plan_with_mask(&tile, &style, &m, zoom);
                assert_eq!(plan, resolve_plan(&tile, &style, &m, zoom), "the same ops at z{zoom}");
                for &(op, rank) in &mask.ops {
                    assert!(matches!(plan.ops[op as usize], PaintOp::Fill { .. }), "op {op} is a fill");
                    assert_ne!(rank, MaskRank::Land, "Land is never listed");
                }
                assert!(mask.ops.windows(2).all(|w| w[0].0 < w[1].0), "strictly ascending");
            }
        }
        // The fixture ranks something at all: water at every zoom here.
        let tile = mask_fixture_tile();
        let (_, mask) = resolve_plan_with_mask(&tile, &style, &match_tile_static_masked(&tile, &style, true), 15);
        assert!(mask.ops.iter().any(|&(_, r)| r == MaskRank::Water));
        assert!(mask.ops.iter().any(|&(_, r)| r == MaskRank::Green), "the park");
    }

    #[test]
    fn a_building_ranks_only_where_the_style_shows_it() {
        let style = or_skip!(streets_style());
        let tile = mask_fixture_tile();
        let m = match_tile_static_masked(&tile, &style, true);
        // Streets' `building` fill-opacity is 0 at z15 and 1 at z16.
        let (_, at16) = resolve_plan_with_mask(&tile, &style, &m, 16);
        assert!(at16.ops.iter().any(|&(_, r)| r == MaskRank::Building), "{at16:?}");
        let (_, at15) = resolve_plan_with_mask(&tile, &style, &m, 15);
        assert!(!at15.ops.iter().any(|&(_, r)| r == MaskRank::Building), "drawn at opacity 0 at z15: land");
        let (_, at14) = resolve_plan_with_mask(&tile, &style, &m, 14);
        assert!(!at14.ops.iter().any(|&(_, r)| r == MaskRank::Building), "Streets `building` has minzoom 15");
    }

    /// Shoelace area of a polygonal geometry (exterior minus holes), in the
    /// geometry's own units.
    fn polygonal_area(geometry: &geo_types::Geometry<i32>) -> f64 {
        fn ring(line: &geo_types::LineString<i32>) -> f64 {
            let c = &line.0;
            let twice: f64 = (0..c.len())
                .map(|k| {
                    let (a, b) = (c[k], c[(k + 1) % c.len()]);
                    f64::from(a.x) * f64::from(b.y) - f64::from(b.x) * f64::from(a.y)
                })
                .sum();
            (twice / 2.0).abs()
        }
        let polygon = |p: &geo_types::Polygon<i32>| ring(p.exterior()) - p.interiors().iter().map(ring).sum::<f64>();
        match geometry {
            Geometry::Polygon(p) => polygon(p),
            Geometry::MultiPolygon(mp) => mp.iter().map(polygon).sum(),
            _ => 0.0,
        }
    }

    /// Final-review M1: Liberty's `building-3d` is the only building layer
    /// it has from z15 (its flat `building` stops at 14). While it compiled
    /// to a flat fill it put its footprints in the dot mask as
    /// `MaskRank::Building`; as a `fill-extrusion` it must still, or
    /// dot-density maps on Liberty move their dots from buildings to land.
    /// End to end on the CPU side: the masked plan, its triangulation, and
    /// the building mask list the GPU mask pass draws.
    #[test]
    fn liberty_puts_building_rank_cells_on_its_extruded_footprints() {
        let liberty = crate::style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        let tile = crate::mvt::decode(std::fs::read("benches/fixtures/shinjuku_z14.pbf").unwrap()).unwrap();
        let buildings = tile.layers.iter().find(|l| l.name == "building").unwrap();
        let footprint_area: f64 = buildings
            .features
            .iter()
            .filter(|f| !matches!(f.tags.get("hide_3d"), Some(TagValue::Bool(true))))
            .map(|f| polygonal_area(&clip_fill_geometry_to_tile_extent(&f.geometry)))
            .sum();
        assert!(footprint_area > 0.0);
        let m = match_tile_static_masked(&tile, &liberty, true);
        for zoom in [14u8, 15, 16] {
            let (plan, ranks) = resolve_plan_with_mask(&tile, &liberty, &m, zoom);
            assert_eq!(plan, resolve_plan(&tile, &liberty, &match_tile_static(&tile, &liberty), zoom), "masking never changes the plan");
            let (mesh, spans) = crate::render::tessellate_plan_with_ranges(
                &plan,
                crate::mvt::MVT_EXTENT,
                crate::render::mesh::COORD_SCALE,
                crate::render::subdivide::Subdivision::NONE,
            );
            assert!(crate::render::compact::mask_ranks_align(&mesh, &spans, &ranks), "z{zoom}");
            let lists = crate::render::compact::mask_lists_unsplit(&mesh, &spans, &ranks);
            let building = &lists.legacy[crate::render::compact::mask_slot(MaskRank::Building).unwrap()];
            let scale = f64::from(crate::render::mesh::COORD_SCALE);
            let ranked_area: f64 = building
                .as_chunks::<3>().0.iter()
                .map(|t| {
                    let p = |i: u32| mesh.vertices[i as usize].position.map(|c| f64::from(c) / scale);
                    let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
                    ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])).abs() / 2.0
                })
                .sum();
            // z14 also has the flat `building` layer (every footprint, the
            // hide_3d outlines too), so its ranked area is at least the
            // extruded footprints'; from z15 it is exactly theirs.
            assert!(ranked_area >= 0.95 * footprint_area, "z{zoom}: building-rank area {ranked_area:.0} of {footprint_area:.0}");
            if zoom >= 15 {
                assert!(ranked_area <= 1.05 * footprint_area, "z{zoom}: building-rank area {ranked_area:.0} of {footprint_area:.0}");
            }
        }
    }

    #[test]
    fn static_and_dynamic_fill_matches_rank_alike() {
        let park = Filter::Equals { key: "class".to_string(), value: "park".to_string() };
        let fill = |id: &str, filter: Filter| StyleLayer::Fill {
            id: id.to_string(),
            source_layer: "landuse".to_string(),
            filter,
            color: ZoomValue::constant(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
            opacity: ZoomValue::constant(1.0),
            outline_color: None,
            min_zoom: None,
            max_zoom: None,
        };
        // The same features, once through a zoom-free filter (cached by
        // `match_tile_static`) and once through a zoom step that is always
        // `park` at these zooms (re-filtered in `resolve_plan`).
        let dynamic = Filter::ZoomStep { base: Box::new(park.clone()), stops: vec![(30.0, Filter::Never)] };
        assert!(dynamic.uses_zoom() && !park.uses_zoom());
        let style = Style { sprite: None, layers: vec![fill("static", park), fill("dynamic", dynamic)], sky: Default::default() };
        let tile = mask_fixture_tile();
        let m = match_tile_static_masked(&tile, &style, true);
        assert!(matches!(m.entries[0], LayerMatch::Fill(FillMatch::Static(..))));
        assert!(matches!(m.entries[1], LayerMatch::Fill(FillMatch::Dynamic)));
        let (plan, mask) = resolve_plan_with_mask(&tile, &style, &m, 15);
        assert_eq!(plan.ops.len(), 2);
        assert_eq!(mask.ops, vec![(0, MaskRank::Green), (1, MaskRank::Green)]);
    }

    /// The final-review I1 gate: an unmasked match classifies nothing --
    /// the same plan, no ranks, `classified` false -- and a masked one
    /// ranks the same plan's fills.
    #[test]
    fn an_unmasked_match_classifies_nothing() {
        let style = or_skip!(streets_style());
        let tile = mask_fixture_tile();
        let plain = match_tile_static(&tile, &style);
        let masked = match_tile_static_masked(&tile, &style, true);
        assert!(!plain.ranked() && masked.ranked());
        for zoom in [14u8, 16] {
            let (plan, ranks) = resolve_plan_with_mask(&tile, &style, &plain, zoom);
            assert_eq!(ranks, MaskRanks::default(), "z{zoom}: nothing classified");
            let (masked_plan, masked_ranks) = resolve_plan_with_mask(&tile, &style, &masked, zoom);
            assert_eq!(plan, masked_plan, "z{zoom}: the gate never changes the ops");
            assert!(masked_ranks.classified && !masked_ranks.ops.is_empty(), "z{zoom}: {masked_ranks:?}");
        }
    }

    #[test]
    fn resolve_plan_equals_an_independent_single_pass_reference_at_every_zoom() {
        // Stays meaningful after `match_tile` becomes a wrapper over
        // `match_tile_static` + `resolve_plan` (see
        // `single_pass_reference_match_tile`'s doc comment for why comparing
        // against `match_tile` itself would no longer be).
        let bytes = mapbox_fixture_or_skip!("mapbox-streets-style.json");
        let style = crate::style_spec::parse_and_compile(&bytes).expect("Mapbox Streets style must compile cleanly");
        let tile = real_shape_fixture_tile();
        let cached = match_tile_static(&tile, &style);
        for zoom in 0u8..=20 {
            let reference = single_pass_reference_match_tile(&tile, &style, zoom);
            let from_cache = resolve_plan(&tile, &style, &cached, zoom);
            assert_eq!(from_cache, reference, "plans must match at zoom {zoom}");
        }
    }

    #[test]
    fn resolve_plan_applies_the_layer_zoom_gate_to_a_zoom_free_filter_from_the_same_cached_match() {
        // The hazard this whole split exists to avoid: `uses_zoom() ==
        // false` means the FILTER's matched feature set can't vary with
        // zoom, not that the LAYER's output can't. A layer with a
        // zoom-free filter and an explicit `min_zoom` must still emit
        // nothing below it and its full ops at/above it -- and it must do
        // so resolved from the SAME cached `TileMatch`, proving the gate is
        // applied at resolve time, not baked into the cached match.
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("water".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "water".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        };
        let style = Style {
            sprite: None,
            layers: vec![StyleLayer::Fill {
                id: "water".to_string(),
                source_layer: "water".to_string(),
                filter: Filter::Any, // zoom-free: uses_zoom() == false
                color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }),
                opacity: ZoomValue::constant(1.0),
                outline_color: None,
                min_zoom: Some(10),
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        assert!(!matches!(&style.layers[0], StyleLayer::Fill { filter, .. } if filter.uses_zoom()));

        let cached = match_tile_static(&tile, &style);

        let below = resolve_plan(&tile, &style, &cached, 9);
        assert_eq!(below.ops.len(), 0, "layer's min_zoom is 10; zoom 9 must emit nothing");

        let at = resolve_plan(&tile, &style, &cached, 10);
        assert_eq!(at.ops.len(), 1, "zoom 10 clears min_zoom and must emit the matched feature");

        let above = resolve_plan(&tile, &style, &cached, 12);
        assert_eq!(above.ops.len(), 1, "zoom above min_zoom with no max_zoom must still emit");
    }

    #[test]
    #[should_panic(expected = "TileMatch::entries must align 1:1 with style.layers")]
    fn resolve_plan_panics_when_the_matchs_layer_count_does_not_match_the_styles() {
        // `resolve_plan`'s doc comment promises a panic if `m` wasn't
        // produced by `match_tile_static(tile, style)` for this exact
        // `(tile, style)` pair. Before this test, that promise was false: a
        // `TileMatch` built against a style with a different LAYER COUNT
        // than the one passed to `resolve_plan` was silently accepted --
        // `Vec::iter().zip()` just truncates to the shorter side, dropping
        // trailing layers with no error at all. This pins the cheap half of
        // closing that gap: a length mismatch must panic, not truncate.
        let one_layer_style = Style {
            sprite: None,
            layers: vec![StyleLayer::Background {
                id: "bg".to_string(),
                color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        };
        let two_layer_style = Style {
            sprite: None,
            layers: vec![
                StyleLayer::Background {
                    id: "bg".to_string(),
                    color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                    min_zoom: None,
                    max_zoom: None,
                },
                StyleLayer::Background {
                    id: "bg2".to_string(),
                    color: ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }),
                    min_zoom: None,
                    max_zoom: None,
                },
            ],
            sky: Default::default(),
        };
        let tile = DecodedTile { data_hash: 0, layers: vec![] };
        let mismatched_match = match_tile_static(&tile, &one_layer_style);

        let _ = resolve_plan(&tile, &two_layer_style, &mismatched_match, 0);
    }

    fn style_with_one_line_layer_dashed(dash: ZoomValue<DashPattern>) -> Style {
        Style {
            sprite: None,
            layers: vec![StyleLayer::Line {
                id: "test-layer".to_string(),
                source_layer: "water".to_string(),
                filter: Filter::Any,
                color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                width: ZoomValue::constant(1.0),
                opacity: ZoomValue::constant(1.0),
                gap_width: ZoomValue::constant(0.0),
                cap: LineCap::Butt,
                join: LineJoin::Miter,
                dash,
                min_zoom: None,
                max_zoom: None,
            }],
            sky: Default::default(),
        }
    }

    fn decoded_tile_with_one_line_feature() -> DecodedTile {
        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("water".to_string()));
        DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "water".to_string(),
                features: vec![Feature { geometry: square_polygon(), tags: tags.into() }],
            }],
        }
    }

    fn dash_of_first_line_op(plan: &PaintPlan) -> DashPattern {
        match &plan.ops[0] {
            PaintOp::Line { dash, .. } => *dash,
            other => panic!("expected a Line op, got {other:?}"),
        }
    }

    #[test]
    fn resolve_plan_carries_the_dash_pattern_for_the_display_zoom() {
        // Uses whatever minimal Style/tile fixture the neighbouring resolve_plan
        // tests in this file already use -- follow the closest existing test's
        // setup rather than inventing a new one.
        let style = style_with_one_line_layer_dashed(
            // solid below z7, dash-dot at and above -- admin-1-boundary's pattern
            ZoomValue::stepped(vec![
                (0, DashPattern::from_runs(&[2.0, 0.0])),
                (7, DashPattern::from_runs(&[2.0, 2.0, 6.0, 2.0])),
            ]),
        );
        let tile = decoded_tile_with_one_line_feature();
        let m = match_tile_static(&tile, &style);

        let at6 = resolve_plan(&tile, &style, &m, 6);
        let at7 = resolve_plan(&tile, &style, &m, 7);

        assert_eq!(dash_of_first_line_op(&at6), DashPattern::from_runs(&[2.0, 0.0]));
        assert_eq!(dash_of_first_line_op(&at7), DashPattern::from_runs(&[2.0, 2.0, 6.0, 2.0]));
    }

    // Every layer the vendored style dashes must actually come out dashed. This
    // is the test that fails if the property is being silently dropped again --
    // which is exactly the bug this feature fixes.
    #[test]
    fn every_vendored_dasharray_layer_compiles_to_a_real_pattern() {
        let bytes = mapbox_fixture_or_skip!("mapbox-streets-style.json");
        let style = crate::style_spec::parse_and_compile(&bytes)
            .expect("vendored style compiles");

        // ids and the zoom at which each specifies a non-solid pattern
        let expected: &[(&str, u8)] = &[
            ("tunnel-minor-case", 10), ("tunnel-street-case", 10),
            ("tunnel-secondary-tertiary-case", 10), ("tunnel-primary-case", 10),
            ("tunnel-major-link-case", 10), ("tunnel-motorway-trunk-case", 10),
            ("tunnel-path", 16), ("tunnel-steps", 16), ("tunnel-pedestrian", 16),
            ("tunnel-construction", 16), ("ferry", 14),
            ("road-path", 16), ("road-steps", 16), ("road-pedestrian", 16),
            ("road-construction", 16), ("road-rail-tracks", 16),
            ("bridge-path", 16), ("bridge-steps", 16), ("bridge-pedestrian", 16),
            ("bridge-construction", 16), ("bridge-rail-tracks", 16),
            ("aerialway", 14), ("admin-1-boundary", 7),
            ("admin-0-boundary-disputed", 7),
        ];

        for (id, zoom) in expected {
            let layer = style.layers.iter()
                .find(|l| matches!(l, StyleLayer::Line { id: lid, .. } if lid == id))
                .unwrap_or_else(|| panic!("{id} must compile as a line layer"));
            let dash = match layer { StyleLayer::Line { dash, .. } => dash.value_at(*zoom), _ => unreachable!() };
            assert!(dash.count > 0, "{id} at z{zoom} must be dashed, got {dash:?}");
            assert!(dash.period() > 0.0, "{id} at z{zoom} must have a positive period");
        }
    }

    // The two zero-gap layers below compile with a zero-length gap run (e.g.
    // `[1, 0]`, `[10, 0]`) -- there is no actual gap between dashes, so
    // `DashPattern::from_runs` collapses them to `count == 0` (solid, not
    // dashed). This is correct: a boundary between two same-type runs isn't
    // a real edge, and leaving it in produced a periodic half-alpha band in
    // the rendered line. Do NOT "fix" this back to a positive count -- it
    // was tried, and it regressed the render. This test pins the collapse
    // itself so the zero-gap case doesn't silently start rendering dashed
    // (or vanish) again.
    #[test]
    fn vendored_zero_gap_layers_collapse_to_solid() {
        let bytes = mapbox_fixture_or_skip!("mapbox-streets-style.json");
        let style = crate::style_spec::parse_and_compile(&bytes).unwrap();

        for id in ["admin-1-boundary-bg", "admin-0-boundary"] {
            let layer = style.layers.iter()
                .find(|l| matches!(l, StyleLayer::Line { id: lid, .. } if lid == id))
                .unwrap_or_else(|| panic!("{id} must compile"));
            let dash = match layer { StyleLayer::Line { dash, .. } => dash.value_at(10), _ => unreachable!() };
            assert_eq!(dash.count, 0, "{id} has a zero-length gap and must collapse to solid (count == 0), got {dash:?}");
        }
    }

    /// `StyleLayer::raster_source` answers `Some` for a raster layer and
    /// `None` for everything else -- the accessor a caller reaches for
    /// instead of destructuring `StyleLayer::Raster { .. }` itself.
    #[test]
    fn raster_source_names_the_layers_own_source_and_nothing_else_has_one() {
        let raster = StyleLayer::Raster {
            id: "natural_earth".into(),
            source: "ne2_shaded".into(),
            opacity: ZoomValue::constant(0.6),
            min_zoom: None,
            max_zoom: None,
        };
        assert_eq!(raster.raster_source(), Some("ne2_shaded"));

        let background = StyleLayer::Background {
            id: "bg".into(),
            color: ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            min_zoom: None,
            max_zoom: None,
        };
        assert_eq!(background.raster_source(), None);
    }
}
