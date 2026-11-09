//! Overlay effects (spec §3) as a pure function of story time: what one
//! overlay layer looks like at `t`, folded from the layer's resolved
//! [`EffectPlan`]s. The capture (Task 7) and the live map (Task 20) both
//! draw [`LayerFx`]; neither keeps effect state of its own.
//!
//! **Pure.** [`layer_fx_at`] reads its arguments and nothing else: no
//! clock, no previous frame, no hash-map order. So frame `n` is the same
//! evaluated first, last or alone, and seeking equals playing
//! (`every_effect_is_a_pure_function_of_t`).
//!
//! **Mechanisms (rulings D1, D2).** `show`/`hide`/`fade` are a per-draw
//! opacity ([`LayerFx::opacity`]), not a paint tween. `highlight`, `drop`
//! and `pulse` are per-feature paint overrides ([`FeatureFx`]) and rings,
//! not `feature-state` (the engine folds `feature-state` to null). `draw`
//! is a line-progress trim ([`LayerFx::trim`]).
//!
//! **Opacity is a chain.** Every `show`/`hide`/`fade`/`draw` of a layer,
//! in `(at, document order)`, ramps from the opacity in force at its own
//! `at` (the previous one's value there) to its target, so a `fade` that
//! starts halfway through a `show` fades from where the show had got to,
//! never from a jump. For the cases spec §3 states -- a `show` of a hidden
//! layer, a `hide` of a shown one -- this is exactly `k_f` and `1 - k_f`.

use std::collections::BTreeMap;

use super::overlay_layers::{point_at_fraction, LayerSource};
use super::script::{Easing, EffectKind};

/// What one overlay layer looks like at `t`. `Default` is "as declared": opacity 1, no trim, no overrides.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerFx {
    pub layer_id: String,
    /// Multiplies the layer's alpha (show, hide, fade).
    pub opacity: f32,
    /// `Some([start, end])` in 0..1 of line progress (draw); `None` draws the whole line.
    pub trim: Option<[f32; 2]>,
    /// The draw head: `(lon, lat, radius_px, rgba)`, drawn as one circle at the tip.
    pub head: Option<(f64, f64, f32, [u8; 4])>,
    /// Per-feature paint overrides, in feature-id order (highlight, drop, pulse).
    pub features: Vec<FeatureFx>,
    /// Pulse rings: `(feature id, radius_px, alpha)`, drawn as a translucent disc under the point.
    pub rings: Vec<(u64, f32, f32)>,
}

impl Default for LayerFx {
    fn default() -> Self {
        LayerFx { layer_id: String::new(), opacity: 1.0, trim: None, head: None, features: Vec::new(), rings: Vec::new() }
    }
}

/// One feature's override: `color` replaces the layer's colour when
/// `Some`, `alpha` multiplies its alpha and `radius_scale` its circle
/// radius. `{None, 1, 1}` is "as declared".
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureFx {
    pub id: u64,
    pub color: Option<[u8; 4]>,
    pub alpha: f32,
    pub radius_scale: f32,
}

/// Resolved once from an `overlay` event (times already resolved by the timeline).
///
/// `until` is `+INFINITY` when the event has none. For a `drop`,
/// `max_radius_px > 0` means `ripple: true` (the ring's final radius);
/// `0` means no ripple.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectPlan {
    pub layer_id: String,
    pub kind: EffectKind,
    pub at: f64,
    pub until: f64,
    pub fade_s: f64,
    pub easing: Easing,
    pub head: Option<(f32, [u8; 4])>,
    pub selected: Vec<u64>,
    pub color: Option<[u8; 4]>,
    pub dim_opacity: f32,
    pub reveal: Vec<(u64, f64)>,
    pub period_s: f64,
    pub max_radius_px: f32,
}

pub const DEFAULT_DIM_OPACITY: f32 = 0.25;
pub const DEFAULT_STAGGER_S: f64 = 0.15;
pub const DEFAULT_PULSE_PERIOD_S: f64 = 1.6;
pub const DEFAULT_PULSE_RADIUS_PX: f32 = 24.0;
/// Envelope of a drop: radius scale 0 -> 1.25 -> 1.0 over 0.35 s, alpha 0 -> 1 over 0.2 s.
pub const DROP_S: f64 = 0.35;
/// A drop's alpha ramp, and when its radius peaks at [`DROP_OVERSHOOT`].
pub const DROP_ALPHA_S: f64 = 0.2;
/// A drop's peak radius scale.
pub const DROP_OVERSHOOT: f32 = 1.25;
/// How long a drop's ripple ring takes to grow to `max_radius_px` and fade out.
pub const RIPPLE_S: f64 = 0.6;

/// `(t - at) / (until - at)` in `[0, 1]`; a zero-length span is a step at `at`.
fn span_fraction(t: f64, at: f64, until: f64) -> f64 {
    if until > at {
        ((t - at) / (until - at)).clamp(0.0, 1.0)
    } else if t >= at {
        1.0
    } else {
        0.0
    }
}

/// `easing` over `since / fade_s`; a zero `fade_s` is a step.
fn fade_fraction(easing: &Easing, since: f64, fade_s: f64) -> f64 {
    if fade_s > 0.0 {
        easing.apply((since / fade_s).clamp(0.0, 1.0))
    } else if since >= 0.0 {
        1.0
    } else {
        0.0
    }
}

/// A plan's `k`: the eased fraction of `[at, until]` reached at `t`.
fn span_k(plan: &EffectPlan, t: f64) -> f64 {
    plan.easing.apply(span_fraction(t, plan.at, plan.until))
}

/// The layer opacity one opacity plan gives at `t >= plan.at`, ramping
/// from `start` (the opacity in force at `plan.at`).
fn opacity_of(plan: &EffectPlan, start: f64, t: f64) -> f64 {
    let (target, k) = match plan.kind {
        // A `show` ramps over `fade_s` whatever its `until` (the pinned
        // test gives one equal to `at`) and holds 1 from then on.
        EffectKind::Show => (1.0, fade_fraction(&plan.easing, t - plan.at, plan.fade_s)),
        EffectKind::Hide => (0.0, fade_fraction(&plan.easing, t - plan.at, plan.fade_s)),
        EffectKind::Fade => (0.0, span_k(plan, t)),
        EffectKind::Draw => (1.0, 1.0),
        _ => return start,
    };
    start + (target - start) * k
}

fn is_opacity_plan(kind: EffectKind) -> bool {
    matches!(kind, EffectKind::Show | EffectKind::Hide | EffectKind::Fade | EffectKind::Draw)
}

/// `a` to `b` by `k`, per channel, rounded.
fn mix(a: [u8; 4], b: [u8; 4], k: f64) -> [u8; 4] {
    let k = k.clamp(0.0, 1.0);
    let mut out = [0u8; 4];
    for ((o, x), y) in out.iter_mut().zip(a).zip(b) {
        *o = (f64::from(x) + (f64::from(y) - f64::from(x)) * k).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// Feature `id`'s own colour for a highlight to mix from: its
/// [`LayerSource::base_colors`] entry (the renderer's own evaluation of the
/// layer's colour for that feature). `None` when it has none to give -- a
/// colour that does not resolve -- and the highlight colour is then used
/// from the start (a snap, warned about as `highlight_base_color`).
fn own_color(source: &LayerSource, id: u64) -> Option<[u8; 4]> {
    let i = source.feature_ids.binary_search(&id).ok()?;
    source.base_colors.get(i).copied().flatten()
}

/// A style colour as bytes.
pub(crate) fn rgba_bytes(c: crate::style::Rgba) -> [u8; 4] {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(c.r), byte(c.g), byte(c.b), byte(c.a)]
}

/// A drop's radius scale `r` seconds after a feature's reveal:
/// 0 -> [`DROP_OVERSHOOT`] over [`DROP_ALPHA_S`], back to 1 by [`DROP_S`].
fn drop_radius(r: f64) -> f32 {
    if r < 0.0 {
        0.0
    } else if r < DROP_ALPHA_S {
        DROP_OVERSHOOT * (r / DROP_ALPHA_S) as f32
    } else if r < DROP_S {
        let k = ((r - DROP_ALPHA_S) / (DROP_S - DROP_ALPHA_S)) as f32;
        DROP_OVERSHOOT + (1.0 - DROP_OVERSHOOT) * k
    } else {
        1.0
    }
}

/// A drop's alpha `r` seconds after a feature's reveal.
fn drop_alpha(r: f64) -> f32 {
    if r < 0.0 {
        0.0
    } else {
        (r / DROP_ALPHA_S).clamp(0.0, 1.0) as f32
    }
}

/// Fold every plan with `at <= t` for `layer`, in (at, document order), onto `LayerFx::default()`.
///
/// `plans` may hold other layers' plans (they are skipped) and is in
/// document order; it is ordered by `at` here (stably). Two rules look
/// ahead of `t`:
/// - a layer whose FIRST opacity effect (`show`/`hide`/`fade`/`draw`) is
///   a `show` or `draw` starts hidden (opacity 0); a `highlight`, `drop`
///   or `pulse` before it does not change that (controller ruling F6);
/// - a layer's FIRST `drop` holds every point it reveals at alpha 0 from
///   the start of the story, whatever comes before it, so the points pop
///   in rather than show, vanish and pop back. A later drop does not
///   re-hide them.
///
/// Pulse rings are multiplied by their point's alpha, so a point a drop
/// still holds hidden (or a highlight dims) has no (or a dimmer) ring.
pub fn layer_fx_at(layer_id: &str, plans: &[EffectPlan], t: f64, source: &LayerSource) -> LayerFx {
    let mut mine: Vec<&EffectPlan> = plans.iter().filter(|p| p.layer_id == layer_id).collect();
    mine.sort_by(|a, b| a.at.total_cmp(&b.at));
    let first_opacity_kind = mine.iter().find(|p| is_opacity_plan(p.kind)).map(|p| p.kind);
    let first_drop = mine.iter().position(|p| p.kind == EffectKind::Drop);
    let mut fx = LayerFx { layer_id: layer_id.to_string(), ..LayerFx::default() };

    // Opacity: the chain (module docs). `value` is the last opacity
    // plan's ramp, evaluated at the next one's `at` to seed it.
    let initial = if matches!(first_opacity_kind, Some(EffectKind::Show | EffectKind::Draw)) { 0.0 } else { 1.0 };
    let mut last: Option<(&EffectPlan, f64)> = None;
    for plan in mine.iter().copied().filter(|p| is_opacity_plan(p.kind) && p.at <= t) {
        let start = last.map_or(initial, |(prev, prev_start)| opacity_of(prev, prev_start, plan.at));
        last = Some((plan, start));
    }
    fx.opacity = last.map_or(initial, |(plan, start)| opacity_of(plan, start, t)) as f32;

    // Feature id -> override: a BTreeMap, so the output is in id order.
    let mut features: BTreeMap<u64, FeatureFx> = BTreeMap::new();
    fn touch(features: &mut BTreeMap<u64, FeatureFx>, id: u64) -> &mut FeatureFx {
        features.entry(id).or_insert(FeatureFx { id, color: None, alpha: 1.0, radius_scale: 1.0 })
    }

    // Pulse rings, kept apart until every feature's alpha is known.
    let mut pulse_rings: Vec<(u64, f32, f32)> = Vec::new();
    for (i, plan) in mine.iter().enumerate() {
        let leading_drop = Some(i) == first_drop;
        if plan.at > t && !leading_drop {
            continue;
        }
        match plan.kind {
            EffectKind::Show | EffectKind::Hide | EffectKind::Fade => {}
            EffectKind::Draw => {
                if t < plan.until {
                    let k = span_k(plan, t);
                    fx.trim = Some([0.0, k as f32]);
                    fx.head = plan.head.and_then(|(radius, rgba)| point_at_fraction(source, k).map(|(lon, lat)| (lon, lat, radius, rgba)));
                } else {
                    fx.trim = Some([0.0, 1.0]);
                    fx.head = None;
                }
            }
            EffectKind::Highlight => {
                // Active from `at` until its reverse ramp ends; then no override.
                if t >= plan.until + plan.fade_s.max(0.0) {
                    continue;
                }
                let k_in = fade_fraction(&plan.easing, t.min(plan.until) - plan.at, plan.fade_s);
                let strength = if t < plan.until { k_in } else { k_in * (1.0 - fade_fraction(&plan.easing, t - plan.until, plan.fade_s)) };
                let dim = (1.0 - (1.0 - f64::from(plan.dim_opacity)) * strength) as f32;
                for &id in &source.feature_ids {
                    let f = touch(&mut features, id);
                    if plan.selected.contains(&id) {
                        if let Some(target) = plan.color {
                            f.color = Some(own_color(source, id).map_or(target, |own| mix(own, target, strength)));
                        }
                    } else {
                        f.alpha *= dim;
                    }
                }
            }
            EffectKind::Drop => {
                for &(id, reveal) in &plan.reveal {
                    let r = t - (plan.at + reveal);
                    let f = touch(&mut features, id);
                    f.alpha *= drop_alpha(r);
                    f.radius_scale *= drop_radius(r);
                    if plan.max_radius_px > 0.0 && (0.0..RIPPLE_S).contains(&r) {
                        let k = (r / RIPPLE_S) as f32;
                        fx.rings.push((id, plan.max_radius_px * k, 1.0 - k));
                    }
                }
            }
            EffectKind::Pulse => {
                if t >= plan.until || plan.period_s.is_nan() || plan.period_s <= 0.0 {
                    continue;
                }
                let phase = (((t - plan.at) % plan.period_s) / plan.period_s) as f32;
                for &id in &source.feature_ids {
                    pulse_rings.push((id, plan.max_radius_px * phase, 1.0 - phase));
                }
            }
        }
    }
    for (id, radius, alpha) in pulse_rings {
        let point_alpha = features.get(&id).map_or(1.0, |f| f.alpha);
        if point_alpha > 0.0 {
            fx.rings.push((id, radius, alpha * point_alpha));
        }
    }
    fx.features = features.into_values().collect();
    fx
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;
    use crate::story::overlay_layers::layer_sources;
    use crate::story::script::OverlaySpec;

    fn source(kind: &str, layer: &str, features: Vec<serde_json::Value>) -> LayerSource {
        let overlay: OverlaySpec = serde_json::from_value(serde_json::json!({
            "id": "o", "data": "o.geojson", "line_metrics": true, "layers": [{"id": layer, "type": kind}]
        }))
        .unwrap();
        layer_sources(0, &overlay, &serde_json::json!({"type": "FeatureCollection", "features": features})).unwrap().remove(0)
    }

    /// A `route-line` LayerSource with one LineString (0,0)->(10,0), ids [0].
    pub(crate) fn line_source() -> LayerSource {
        let src = source("line", "route-line", vec![serde_json::json!(
            {"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": [[0.0, 0.0], [10.0, 0.0]]}})]);
        assert_eq!(src.feature_ids, vec![0]);
        src
    }

    /// A `ports` circle LayerSource of `n` Points at (i, 0), ids 0..n.
    pub(crate) fn points_source(n: usize) -> LayerSource {
        let features = (0..n)
            .map(|i| serde_json::json!({"type": "Feature", "properties": {"rank": i}, "geometry": {"type": "Point", "coordinates": [i as f64, 0.0]}}))
            .collect();
        let src = source("circle", "ports", features);
        assert_eq!(src.feature_ids, (0..n as u64).collect::<Vec<_>>());
        src
    }

    /// A plan on `route-line` (draw/show/hide/fade) or `ports` (the rest),
    /// with the defaults: fade_s 0.4, linear easing, dim 0.25, a white 6 px
    /// head, nothing selected, no reveal, the default pulse.
    pub(crate) fn plan(kind: EffectKind, at: f64, until: f64) -> EffectPlan {
        let layer = match kind {
            EffectKind::Draw | EffectKind::Show | EffectKind::Hide | EffectKind::Fade => "route-line",
            _ => "ports",
        };
        EffectPlan {
            layer_id: layer.to_string(),
            kind,
            at,
            until,
            fade_s: 0.4,
            easing: Easing::Linear,
            head: Some((6.0, [255, 255, 255, 255])),
            selected: Vec::new(),
            color: None,
            dim_opacity: DEFAULT_DIM_OPACITY,
            reveal: Vec::new(),
            period_s: DEFAULT_PULSE_PERIOD_S,
            max_radius_px: DEFAULT_PULSE_RADIUS_PX,
        }
    }

    #[test]
    fn draw_trims_linearly_and_holds_the_whole_line_after() {
        let p = plan(EffectKind::Draw, 1.0, 5.0);
        let src = line_source();
        assert_eq!(layer_fx_at("route-line", std::slice::from_ref(&p), 3.0, &src).trim, Some([0.0, 0.5]));
        assert_eq!(layer_fx_at("route-line", std::slice::from_ref(&p), 9.0, &src).trim, Some([0.0, 1.0]));
        let head = layer_fx_at("route-line", std::slice::from_ref(&p), 3.0, &src).head.expect("a head while drawing");
        assert!((head.0 - 5.0).abs() < 1e-6 && head.1.abs() < 1e-6);
        assert_eq!(layer_fx_at("route-line", &[p], 0.5, &src).opacity, 0.0, "a drawn layer is hidden before its draw");
    }

    #[test]
    fn show_hide_and_fade_are_opacity_ramps() {
        let src = line_source();
        let show = plan(EffectKind::Show, 1.0, 1.0);
        assert!((layer_fx_at("route-line", std::slice::from_ref(&show), 1.2, &src).opacity - 0.5).abs() < 1e-6);
        let hide = EffectPlan { at: 3.0, ..plan(EffectKind::Hide, 3.0, 3.0) };
        assert_eq!(layer_fx_at("route-line", &[show.clone(), hide.clone()], 4.0, &src).opacity, 0.0);
        let fade = plan(EffectKind::Fade, 2.0, 4.0);
        assert!((layer_fx_at("route-line", &[fade], 3.0, &src).opacity - 0.5).abs() < 1e-6);
    }

    #[test]
    fn highlight_colours_the_selection_and_dims_the_rest() {
        let src = points_source(3);
        let mut p = plan(EffectKind::Highlight, 0.0, 10.0);
        p.selected = vec![1];
        p.color = Some([255, 0, 0, 255]);
        let fx = layer_fx_at("ports", &[p], 5.0, &src);
        let by_id = |id| fx.features.iter().find(|f| f.id == id).cloned().unwrap();
        assert_eq!(by_id(1).color, Some([255, 0, 0, 255]));
        assert!((by_id(0).alpha - 0.25).abs() < 1e-6 && (by_id(2).alpha - 0.25).abs() < 1e-6);
    }

    #[test]
    fn drop_reveals_features_in_order_on_the_stagger() {
        let src = points_source(4);
        let mut p = plan(EffectKind::Drop, 1.0, 1.0);
        p.reveal = (0..4).map(|i| (i as u64, i as f64 * 0.5)).collect();
        let fx = layer_fx_at("ports", &[p], 2.0, &src); // features 0,1 revealed (r = 1.0, 0.5), 2 at r = 0, 3 not yet
        let alpha: Vec<f32> = fx.features.iter().map(|f| f.alpha).collect();
        assert_eq!(alpha[0], 1.0);
        assert_eq!(alpha[1], 1.0);
        assert_eq!(alpha[3], 0.0);
    }

    #[test]
    fn pulse_is_periodic_in_t() {
        let src = points_source(1);
        let p = EffectPlan { period_s: 2.0, max_radius_px: 20.0, ..plan(EffectKind::Pulse, 0.0, f64::INFINITY) };
        let a = layer_fx_at("ports", std::slice::from_ref(&p), 0.5, &src).rings;
        let b = layer_fx_at("ports", &[p], 2.5, &src).rings;
        assert_eq!(a, b);
        assert!((a[0].1 - 5.0).abs() < 1e-5 && (a[0].2 - 0.75).abs() < 1e-5);
    }

    /// Xorshift64: a fixed-seed stream with no `rand` dependency.
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        /// Uniform in `[lo, hi)`.
        fn range(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (self.next() >> 11) as f64 / (1u64 << 53) as f64 * (hi - lo)
        }
    }

    #[test]
    fn every_effect_is_a_pure_function_of_t() {
        let (line, points) = (line_source(), points_source(5));
        let mut rng = XorShift(0x5EED_CAFE_F00D_1234);
        let kinds = [EffectKind::Draw, EffectKind::Show, EffectKind::Hide, EffectKind::Fade, EffectKind::Highlight, EffectKind::Drop, EffectKind::Pulse];
        let mut plans = Vec::new();
        for _ in 0..20 {
            let kind = kinds[(rng.next() % kinds.len() as u64) as usize];
            let at = rng.range(0.0, 8.0);
            let until = if rng.next().is_multiple_of(4) { f64::INFINITY } else { at + rng.range(0.1, 4.0) };
            let mut p = plan(kind, at, until);
            p.fade_s = rng.range(0.0, 1.0);
            p.easing = [Easing::Linear, Easing::Ease, Easing::EaseInOut, Easing::Bezier([0.3, -0.5, 0.7, 1.5])][(rng.next() % 4) as usize].clone();
            // Highlight also on the line (a second layer, a second source).
            if kind == EffectKind::Highlight && rng.next().is_multiple_of(2) {
                p.layer_id = "route-line".to_string();
            }
            p.selected = (0..5).filter(|_| rng.next().is_multiple_of(2)).collect();
            p.color = (rng.next().is_multiple_of(2)).then_some([200, 40, 10, 255]);
            p.dim_opacity = rng.range(0.0, 1.0) as f32;
            p.reveal = (0..5).map(|i| (i, rng.range(0.0, 2.0))).collect();
            p.period_s = rng.range(0.3, 3.0);
            p.max_radius_px = if rng.next().is_multiple_of(3) { 0.0 } else { rng.range(5.0, 40.0) as f32 };
            plans.push(p);
        }
        let frame = |n: usize| {
            let t = n as f64 / 30.0;
            (layer_fx_at("route-line", &plans, t, &line), layer_fx_at("ports", &plans, t, &points))
        };
        let forward: Vec<_> = (0..300).map(frame).collect();
        let mut backward: Vec<_> = (0..300).rev().map(frame).collect();
        backward.reverse();
        assert!(forward == backward, "seeking must equal playing");
        // The fold is order-independent in its input too: plans are in
        // document order, sorted by `at` inside.
        let mut shuffled = plans.clone();
        shuffled.sort_by(|a, b| b.at.total_cmp(&a.at));
        shuffled.reverse();
        assert!((0..300).all(|n| {
            let t = n as f64 / 30.0;
            layer_fx_at("ports", &shuffled, t, &points) == forward[n].1
        }));
        // And the plans actually did something, of each kind: a draw frame
        // with a head, a colour override, a dimmed point, a ring, and an
        // opacity strictly between 0 and 1.
        assert!(forward.iter().any(|(l, _)| l.trim.is_some_and(|[_, e]| e > 0.0 && e < 1.0) && l.head.is_some()), "a draw in progress");
        assert!(forward.iter().any(|(l, p)| l.features.iter().chain(&p.features).any(|f| f.color.is_some())), "a highlight colour");
        assert!(forward.iter().any(|(_, p)| p.features.iter().any(|f| f.alpha > 0.0 && f.alpha < 1.0)), "a dimmed or dropping point");
        assert!(forward.iter().any(|(_, p)| !p.rings.is_empty()), "a ring");
        assert!(forward.iter().any(|(l, _)| l.opacity > 0.0 && l.opacity < 1.0), "an opacity ramp");
    }

    #[test]
    fn a_fade_starts_from_where_a_show_had_got_to() {
        let src = line_source();
        let show = plan(EffectKind::Show, 1.0, f64::INFINITY);
        let fade = plan(EffectKind::Fade, 1.2, 2.2);
        // At 1.2 the show is halfway (0.5); the fade halves that by 1.7.
        let fx = layer_fx_at("route-line", &[show, fade], 1.7, &src);
        assert!((fx.opacity - 0.25).abs() < 1e-6, "{}", fx.opacity);
    }

    #[test]
    fn a_highlight_reverses_after_until_and_then_leaves_no_override() {
        let src = points_source(2);
        let mut p = plan(EffectKind::Highlight, 0.0, 2.0);
        p.selected = vec![0];
        let at = |t| layer_fx_at("ports", &[p.clone()], t, &src);
        assert!((at(2.2).features[1].alpha - 0.625).abs() < 1e-6, "halfway back: 1 - 0.75 * 0.5");
        assert!(at(2.4).features.is_empty());
    }

    #[test]
    fn a_leading_drop_hides_its_points_before_it_starts_and_ripples() {
        let src = points_source(2);
        let mut p = plan(EffectKind::Drop, 5.0, f64::INFINITY);
        p.reveal = vec![(0, 0.0), (1, 0.15)];
        let before = layer_fx_at("ports", &[p.clone()], 1.0, &src);
        assert!(before.features.iter().all(|f| f.alpha == 0.0 && f.radius_scale == 0.0));
        assert!(before.rings.is_empty());
        let peak = layer_fx_at("ports", &[p.clone()], 5.2, &src);
        assert!((peak.features[0].radius_scale - DROP_OVERSHOOT).abs() < 1e-6);
        assert_eq!(peak.rings.len(), 2);
        let settled = layer_fx_at("ports", &[p], 9.0, &src);
        assert!(settled.features.iter().all(|f| f.alpha == 1.0 && f.radius_scale == 1.0) && settled.rings.is_empty());
    }

    #[test]
    fn a_choropleth_highlight_ramps_from_each_features_own_colour() {
        let overlay: OverlaySpec = serde_json::from_value(serde_json::json!({
            "id": "c", "data": "c.geojson",
            "layers": [{"id": "countries", "type": "fill", "paint": {"fill-color": ["match", ["get", "iso"], "FRA", "#0000ff", "#00ff00"]}}]
        }))
        .unwrap();
        let square = serde_json::json!({"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 0]]]});
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {"iso": "DEU"}, "geometry": square},
            {"type": "Feature", "properties": {"iso": "FRA"}, "geometry": square}]});
        let src = layer_sources(0, &overlay, &doc).unwrap().remove(0);
        let mut p = plan(EffectKind::Highlight, 0.0, 10.0);
        p.layer_id = "countries".to_string();
        p.selected = vec![1];
        p.color = Some([255, 0, 0, 255]);
        let fra = |t: f64| layer_fx_at("countries", std::slice::from_ref(&p), t, &src).features.iter().find(|f| f.id == 1).and_then(|f| f.color);
        assert_eq!(fra(0.0), Some([0, 0, 255, 255]), "starts at its own blue");
        assert_eq!(fra(0.2), Some([128, 0, 128, 255]));
        // Halfway back (10.2 - 10.0 is a hair under 0.2 in f64: +-1 per channel).
        let back = fra(10.2).unwrap();
        assert!(back.iter().zip([128u8, 0, 128, 255]).all(|(a, b)| a.abs_diff(b) <= 1), "halfway back: {back:?}");
        assert!(layer_fx_at("countries", std::slice::from_ref(&p), 10.4, &src).features.is_empty(), "no override once back");
        let (base, target) = ([0u8, 0, 255, 255], [255u8, 0, 0, 255]);
        for n in 0..330 {
            let c = fra(f64::from(n) / 30.0).unwrap_or(base);
            for ch in 0..4 {
                let (lo, hi) = (base[ch].min(target[ch]), base[ch].max(target[ch]));
                assert!((lo..=hi).contains(&c[ch]), "frame {n}: {c:?} leaves [{base:?}, {target:?}]");
            }
        }
    }

    #[test]
    fn points_stay_hidden_before_the_first_drop_whatever_precedes_it() {
        let src = points_source(3);
        let pulse = plan(EffectKind::Pulse, 0.0, f64::INFINITY);
        let mut drop = plan(EffectKind::Drop, 3.0, f64::INFINITY);
        drop.reveal = vec![(0, 0.0), (1, 0.5), (2, 1.0)];
        drop.max_radius_px = 0.0;
        let show = plan(EffectKind::Show, 0.0, 0.0);
        for before in [pulse.clone(), EffectPlan { layer_id: "ports".to_string(), ..show }] {
            let fx = layer_fx_at("ports", &[before.clone(), drop.clone()], 1.0, &src);
            assert!(fx.features.iter().all(|f| f.alpha == 0.0), "{:?}: {fx:?}", before.kind);
            assert!(fx.rings.is_empty(), "a hidden point has no ring");
        }
        // Once feature 0 has dropped (3.1 s: alpha 0.5) its ring is at half
        // its alpha; the others, still hidden, ring not at all.
        let fx = layer_fx_at("ports", &[pulse.clone(), drop.clone()], 3.1, &src);
        assert_eq!(fx.rings.len(), 1);
        let phase = ((3.1 % DEFAULT_PULSE_PERIOD_S) / DEFAULT_PULSE_PERIOD_S) as f32;
        assert!((fx.rings[0].2 - (1.0 - phase) * 0.5).abs() < 1e-5, "{:?}", fx.rings);
        // A second drop does not re-hide the points the first revealed.
        let mut again = drop.clone();
        again.at = 8.0;
        let fx = layer_fx_at("ports", &[drop, again], 6.0, &src);
        assert!(fx.features.iter().all(|f| f.alpha == 1.0));
    }

    #[test]
    fn only_an_opacity_effect_decides_whether_a_layer_starts_hidden() {
        // F6: a highlight first, then a show: hidden until the show.
        let src = line_source();
        let mut highlight = plan(EffectKind::Highlight, 0.0, 9.0);
        highlight.layer_id = "route-line".to_string();
        let show = plan(EffectKind::Show, 2.0, 2.0);
        assert_eq!(layer_fx_at("route-line", &[highlight.clone(), show.clone()], 1.0, &src).opacity, 0.0);
        assert_eq!(layer_fx_at("route-line", &[highlight.clone(), show], 3.0, &src).opacity, 1.0);
        // A hide first: visible until it.
        let hide = plan(EffectKind::Hide, 2.0, 2.0);
        assert_eq!(layer_fx_at("route-line", &[highlight, hide], 1.0, &src).opacity, 1.0);
    }
}
