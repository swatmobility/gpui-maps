//! Callouts (spec §4.4): a small card pinned by a leader line to a map
//! coordinate. Where the coordinate lands on screen is the caller's job
//! (native: the capture's `WorldProjector`, behind the globe's horizon is
//! `None`); this module decides, from that projection over story time,
//! which side of its anchor the card sits on and how visible it is.
//!
//! **Pure over story time (R8).** [`schedule`] walks the callout's span on
//! a fixed [`SCHEDULE_STEP_S`] grid -- never the output's frame times --
//! and records only change points, so [`side_at`] and
//! [`visibility_alpha`] are lookups: a frame seeked to directly reads what
//! a frame played to reads, and 24, 30 and 60 fps outputs agree at every
//! time they share. No clock, no previous-frame state.
//!
//! **Hysteresis.** A card flips only when the side it is on would put it
//! more than [`FLIP_MARGIN`] units outside the free frame `[MARGIN,
//! frame_w - MARGIN]` (and the other side is clearly better), and once
//! flipped it stays until THAT side overflows the same way: an anchor
//! hovering about the frame edge flips once, not every frame.
//!
//! **Fade.** The card is visible while its anchor projects inside the frame
//! inset by [`VISIBLE_INSET`]; every change ramps over [`FADE_S`]. A point
//! that goes behind the horizon keeps its last projected place on the grid
//! while it fades ([`callout_at`]), and at alpha 0 nothing is placed.
//!
//! Compiled on web and iOS through `#[path]` includes.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

use super::layout::{Side, CALLOUT_OFFSET, MARGIN};

/// Where a callout's anchor is at `t`: in 1080 units, or `None` when it is
/// behind the globe's horizon or the near plane.
pub type Project<'a> = &'a dyn Fn(f64) -> Option<(f32, f32)>;

/// The schedule's grid step, seconds: independent of the output fps.
pub const SCHEDULE_STEP_S: f64 = 1.0 / 30.0;
/// Hysteresis: the card flips only when the side it is on would put it
/// more than FLIP_MARGIN units outside the free frame.
pub const FLIP_MARGIN: f32 = 24.0;
/// How long a visibility change takes to fade, seconds.
pub const FADE_S: f64 = 0.3;
/// A callout is visible while its anchor is inside the frame inset by this
/// many units.
pub const VISIBLE_INSET: f32 = 8.0;
/// The most grid steps one schedule walks (a day at 30 steps a second): a
/// guard against a nonsense span, never reached by a real story.
const MAX_STEPS: u64 = 30 * 60 * 60 * 24;

#[derive(Clone, Debug, PartialEq)]
pub struct CalloutSchedule {
    /// (t, side) change points, starting at `at` with the side that fits.
    pub sides: Vec<(f64, Side)>,
    /// (t, visible) change points: visible = projected, inside the frame
    /// inset by [`VISIBLE_INSET`] units.
    pub visibility: Vec<(f64, bool)>,
}

/// How far a card on `side` of an anchor at `x` reaches past the free
/// frame `[MARGIN, frame_w - MARGIN]`, units (negative: room to spare).
pub fn overflow(side: Side, x: f32, card_w: f32, frame_w: f32) -> f32 {
    match side {
        Side::Right => x + CALLOUT_OFFSET + card_w - (frame_w - MARGIN),
        Side::Left => MARGIN - (x - CALLOUT_OFFSET - card_w),
    }
}

fn other(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
    }
}

/// The side that fits an anchor at `x`: the right when it fits, else the
/// left when it fits, else whichever overflows less (the right on a tie).
fn fitting_side(x: f32, card_w: f32, frame_w: f32) -> Side {
    let (right, left) = (overflow(Side::Right, x, card_w, frame_w), overflow(Side::Left, x, card_w, frame_w));
    if right <= 0.0 || (left > 0.0 && right <= left) {
        Side::Right
    } else {
        Side::Left
    }
}

fn finite(p: Option<(f32, f32)>) -> Option<(f32, f32)> {
    p.filter(|(x, y)| x.is_finite() && y.is_finite())
}

/// Grid point `k` of a schedule starting at `at`: from the index, never
/// summed, so it is the same number however it is reached.
fn grid(at: f64, k: u64) -> f64 {
    at + k as f64 * SCHEDULE_STEP_S
}

/// Walk [at, until] on the fixed SCHEDULE_STEP_S grid (independent of the
/// output fps), deciding the side with hysteresis from the card width
/// `card_w` and the frame `(w, h)` in units. Pure; the same inputs give
/// the same schedule.
///
/// (The plan's signature took `frame_w` alone; the visibility rule --
/// inside the frame inset by 8 units -- needs the height too.)
pub fn schedule(at: f64, until: f64, card_w: f32, frame: (f32, f32), project: Project<'_>) -> CalloutSchedule {
    let at = if at.is_finite() { at } else { 0.0 };
    let span = if until.is_finite() && until > at { until - at } else { 0.0 };
    let steps = ((span / SCHEDULE_STEP_S).ceil() as u64).min(MAX_STEPS);
    let (w, h) = frame;
    let card_w = if card_w.is_finite() { card_w.max(0.0) } else { 0.0 };
    let visible_at = |p: Option<(f32, f32)>| {
        p.is_some_and(|(x, y)| (VISIBLE_INSET..=w - VISIBLE_INSET).contains(&x) && (VISIBLE_INSET..=h - VISIBLE_INSET).contains(&y))
    };
    let mut sides: Vec<(f64, Side)> = Vec::new();
    let mut visibility: Vec<(f64, bool)> = Vec::new();
    let mut side: Option<Side> = None;
    for k in 0..=steps {
        // The last point is `until` itself, not past it.
        let t = grid(at, k).min(at + span);
        let p = finite(project(t));
        let visible = visible_at(p);
        if visibility.last().is_none_or(|(_, v)| *v != visible) {
            visibility.push((t, visible));
        }
        let Some((x, _)) = p else { continue };
        match side {
            // The first projected point decides the starting side, which
            // is recorded at `at` whenever it is found.
            None => {
                let s = fitting_side(x, card_w, w);
                sides.push((at, s));
                side = Some(s);
            }
            Some(s) => {
                let here = overflow(s, x, card_w, w);
                let there = overflow(other(s), x, card_w, w);
                if here > FLIP_MARGIN && there + FLIP_MARGIN < here {
                    sides.push((t, other(s)));
                    side = Some(other(s));
                }
            }
        }
    }
    if sides.is_empty() {
        sides.push((at, Side::Right));
    }
    CalloutSchedule { sides, visibility }
}

/// The side at `t` (the last change point at or before it; the first
/// side before the first point).
pub fn side_at(s: &CalloutSchedule, t: f64) -> Side {
    s.sides.iter().take_while(|(c, _)| *c <= t).last().or(s.sides.first()).map_or(Side::Right, |(_, side)| *side)
}

/// `a` moved `dt` seconds towards `visible` at one full fade per FADE_S.
fn ramp(a: f64, visible: bool, dt: f64) -> f64 {
    let step = if FADE_S > 0.0 { dt.max(0.0) / FADE_S } else { f64::INFINITY };
    if visible {
        (a + step).min(1.0)
    } else {
        (a - step).max(0.0)
    }
}

/// Alpha at `t`, `0..=1`: a FADE_S ramp towards the last visibility change,
/// continuing from wherever the previous ramp had reached (no jump when a
/// change comes mid-fade). The callout starts settled at its first state.
/// The caller multiplies it by the element's own alpha ([`callout_at`]).
pub fn visibility_alpha(s: &CalloutSchedule, t: f64) -> f32 {
    let Some(&(t0, v0)) = s.visibility.first() else { return 1.0 };
    let t = if t.is_nan() { t0 } else { t };
    let mut alpha = if v0 { 1.0 } else { 0.0 };
    let (mut from, mut target) = (t0, v0);
    for &(c, v) in s.visibility.iter().skip(1) {
        if t < c {
            break;
        }
        alpha = ramp(alpha, target, c - from);
        (from, target) = (c, v);
    }
    ramp(alpha, target, t - from) as f32
}

/// A callout on one frame: where its anchor is, which side its card is on
/// and its alpha (the element's times the visibility fade).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalloutAt {
    pub anchor: (f32, f32),
    pub side: Side,
    pub alpha: f32,
}

/// The callout at `t`, or `None` when it draws nothing: alpha 0, or no
/// finite anchor. A point that is not projected at `t` itself (it has just
/// gone behind the horizon and is fading out) keeps its last projected
/// place on the schedule's grid within FADE_S before `t` -- found by
/// walking the grid back from `t`, so it is still a pure function of `t`.
pub fn callout_at(s: &CalloutSchedule, element_alpha: f32, t: f64, project: Project<'_>) -> Option<CalloutAt> {
    let element_alpha = if element_alpha.is_finite() { element_alpha.clamp(0.0, 1.0) } else { 0.0 };
    let alpha = element_alpha * visibility_alpha(s, t);
    if !(alpha.is_finite() && alpha > 0.0 && t.is_finite()) {
        return None;
    }
    let anchor = finite(project(t)).or_else(|| {
        let at = s.sides.first().map_or(t, |(a, _)| *a);
        if t < at {
            return None;
        }
        let k = ((t - at) / SCHEDULE_STEP_S).floor() as u64;
        let back = (FADE_S / SCHEDULE_STEP_S).ceil() as u64 + 1;
        (k.saturating_sub(back)..=k).rev().map(|k| grid(at, k)).filter(|g| *g <= t).find_map(|g| finite(project(g)))
    })?;
    Some(CalloutAt { anchor, side: side_at(s, t), alpha })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: (f32, f32) = (1920.0, 1080.0);
    const CARD_W: f32 = 420.0;

    /// Review Focus 2, the schedule half (the drawing half is
    /// `capture::story_chrome::tests::a_callout_behind_the_globe_fades_out_and_draws_nothing`):
    /// a point that goes behind the globe at t = 2 fades out over FADE_S,
    /// and then the callout places nothing at all.
    #[test]
    fn a_callout_behind_the_globe_fades_out_and_draws_nothing() {
        let project = |t: f64| if t < 2.0 { Some((500.0, 500.0)) } else { None };
        let s = schedule(0.0, 5.0, CARD_W, FRAME, &project);
        assert_eq!(s.visibility.len(), 2, "{:?}", s.visibility);
        assert_eq!(visibility_alpha(&s, 1.9), 1.0);
        let mid = visibility_alpha(&s, 2.15);
        assert!((mid - 0.5).abs() <= 0.05, "half way through the fade: {mid}");
        assert_eq!(visibility_alpha(&s, 2.4), 0.0);
        // Fading, the card stays at the point's last projected place.
        let fading = callout_at(&s, 1.0, 2.15, &project).expect("still fading");
        assert_eq!(fading.anchor, (500.0, 500.0));
        assert!((fading.alpha - mid).abs() < 1e-6);
        // Behind the globe and faded: nothing is placed, whatever the element's alpha.
        assert_eq!(callout_at(&s, 1.0, 3.0, &project), None);
        assert_eq!(callout_at(&s, 1.0, 2.4, &project), None);
        // A NaN projection is no projection.
        let nan = |_: f64| Some((f32::NAN, 500.0));
        let s = schedule(0.0, 1.0, CARD_W, FRAME, &nan);
        assert_eq!(s.visibility, vec![(0.0, false)]);
        assert_eq!(callout_at(&s, 1.0, 0.5, &nan), None);
    }

    /// Pre-flight ruling F20's script. The free frame is [MARGIN, w -
    /// MARGIN]; a right-hand card of CARD_W overflows it once its anchor is
    /// past `fits` (1428) and flips once past `flips` (1452). The anchor
    /// drifts 1400 -> 1500 over 1 s, then swings 1380..1500 (twice a
    /// second, 3 s) -- crossing both lines every swing. Exactly one flip,
    /// and no flip back.
    #[test]
    fn a_callout_at_the_frame_edge_flips_once_not_every_frame() {
        let fits = FRAME.0 - MARGIN - CARD_W - CALLOUT_OFFSET;
        let flips = fits + FLIP_MARGIN;
        assert_eq!((fits, flips), (1428.0, 1452.0));
        let x = |t: f64| -> f32 {
            if t <= 1.0 {
                1400.0 + 100.0 * t as f32
            } else {
                1440.0 + 60.0 * (2.0 * std::f64::consts::PI * (t - 1.0) / 0.5).cos() as f32
            }
        };
        let project = |t: f64| Some((x(t), 540.0));
        let s = schedule(0.0, 4.0, CARD_W, FRAME, &project);
        assert_eq!(s.sides.len(), 2, "{:?}", s.sides);
        assert_eq!(s.sides[0], (0.0, Side::Right));
        let (flip_t, side) = s.sides[1];
        assert_eq!(side, Side::Left);
        assert!(x(flip_t) > flips && x(flip_t - SCHEDULE_STEP_S) <= flips, "flipped where it first passed {flips}: {flip_t}");
        // The path really does hover at the edge: a rule without hysteresis
        // ("the right when it fits") would change side on every swing.
        let naive = (0..=120).map(|k| fitting_side(x(k as f64 / 30.0), CARD_W, FRAME.0)).collect::<Vec<_>>();
        let naive_flips = naive.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(naive_flips >= 10, "the path straddles the edge ({naive_flips} naive flips)");
        // Always visible: the anchor never leaves the frame.
        assert_eq!(s.visibility, vec![(0.0, true)]);
    }

    #[test]
    fn side_at_reads_the_last_change_point_and_starts_with_the_side_that_fits() {
        let s = CalloutSchedule { sides: vec![(1.0, Side::Left), (2.0, Side::Right)], visibility: vec![(1.0, true)] };
        assert_eq!(side_at(&s, 0.5), Side::Left, "before the first point: the first side");
        assert_eq!(side_at(&s, 1.0), Side::Left);
        assert_eq!(side_at(&s, 1.99), Side::Left);
        assert_eq!(side_at(&s, 2.0), Side::Right);
        // An anchor near the right edge starts on the left; one in the
        // middle on the right.
        let right_edge = |_: f64| Some((1800.0, 500.0));
        assert_eq!(schedule(0.0, 1.0, CARD_W, FRAME, &right_edge).sides, vec![(0.0, Side::Left)]);
        let middle = |_: f64| Some((900.0, 500.0));
        assert_eq!(schedule(0.0, 1.0, CARD_W, FRAME, &middle).sides, vec![(0.0, Side::Right)]);
        // Not projected at first: the first projected point decides, recorded at `at`.
        let late = |t: f64| (t > 0.5).then_some((1800.0, 500.0));
        assert_eq!(schedule(0.25, 1.0, CARD_W, FRAME, &late).sides, vec![(0.25, Side::Left)]);
    }

    #[test]
    fn a_point_leaving_the_frame_fades_and_one_returning_fades_back_in_from_where_it_was() {
        // In the frame, out past the right edge (inset 8) at 1.0, back at 1.1.
        let project = |t: f64| Some((if (1.0..1.1).contains(&t) { 1915.0 } else { 900.0 }, 500.0));
        let s = schedule(0.0, 3.0, CARD_W, FRAME, &project);
        assert_eq!(s.visibility.len(), 3, "{:?}", s.visibility);
        let back = s.visibility[2].0;
        let low = visibility_alpha(&s, back);
        assert!(low > 0.5 && low < 0.8, "the fade out was cut short: {low}");
        // It climbs back from there, not from 0, and settles at 1.
        assert!(visibility_alpha(&s, back + 0.05) > low);
        assert_eq!(visibility_alpha(&s, back + FADE_S), 1.0);
        // Every alpha is in range, and a NaN time is the first state.
        for k in 0..400 {
            let a = visibility_alpha(&s, k as f64 / 100.0 - 0.5);
            assert!((0.0..=1.0).contains(&a), "{a}");
        }
        assert_eq!(visibility_alpha(&s, f64::NAN), 1.0);
    }

    #[test]
    fn the_schedule_is_pure_and_degenerate_spans_are_one_point() {
        let project = |t: f64| Some((1300.0 + 200.0 * t as f32, 500.0));
        let a = schedule(0.0, 2.0, CARD_W, FRAME, &project);
        assert_eq!(a, schedule(0.0, 2.0, CARD_W, FRAME, &project));
        // Grid points are `at + k * step`: a later frame time never shifts them.
        assert!(a.sides.iter().all(|(t, _)| ((t / SCHEDULE_STEP_S) - (t / SCHEDULE_STEP_S).round()).abs() < 1e-9));
        assert_eq!(schedule(f64::NAN, 2.0, CARD_W, FRAME, &project), a, "a NaN start is 0");
        for (at, until) in [(1.0, 1.0), (1.0, 0.5), (1.0, f64::INFINITY), (1.0, f64::NAN)] {
            let s = schedule(at, until, CARD_W, FRAME, &project);
            assert_eq!(s.sides.len(), 1, "{at} {until}");
            assert_eq!(s.visibility.len(), 1);
        }
        let none = |_: f64| None;
        let s = schedule(0.0, 1.0, CARD_W, FRAME, &none);
        assert_eq!(s, CalloutSchedule { sides: vec![(0.0, Side::Right)], visibility: vec![(0.0, false)] });
        assert_eq!(callout_at(&s, 1.0, 0.5, &none), None);
    }
}
