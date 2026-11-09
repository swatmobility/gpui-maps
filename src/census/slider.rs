//! The income range slider's press-and-drag rules, on a track measured in
//! fractions (0 at the ramp's low end, 1 at its high end): which handle a
//! press holds, how a drag moves it, and where a `?range=` opens it. Moved
//! from `demos/us_block_groups.rs` so the native legend and the browser's
//! DOM slider take the same decisions; each shell only turns its pointer
//! into a fraction.
//!
//! Handles sit on [`slider_stops`] only, so a drag never splits a paint
//! class and every move is a table-only paint update.

use super::ramp::{income_fraction, slider_stops, IncomeRange, VIRIDIS_STOPS};

/// The knobs' diameter, in px (native `KNOB`, the DOM's knob).
pub const KNOB_PX: f64 = 14.0;
/// The narrowest bar the native legend draws (`LEGEND_MIN_BAR`), in px.
pub const MIN_BAR_PX: f64 = 206.0;

/// A track's knob size and pixel, as fractions of its length: what a
/// press's fraction is compared with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Track {
    /// The knob's diameter over the bar's length.
    pub knob: f64,
    /// One pixel over the bar's length: how far a pointer must move off
    /// an [`Handle::Either`] press before the direction decides.
    pub px: f64,
}

impl Track {
    /// A bar `bar_px` long with [`KNOB_PX`] knobs. A bar under a pixel is
    /// taken as one pixel.
    pub fn of_px(bar_px: f64) -> Self {
        let bar = if bar_px.is_finite() { bar_px.max(1.0) } else { MIN_BAR_PX };
        Self { knob: KNOB_PX / bar, px: 1.0 / bar }
    }

    /// The native legend's narrowest bar: 14 px knobs on 206 px.
    pub fn default_bar() -> Self {
        Self::of_px(MIN_BAR_PX)
    }
}

/// What a press on the slider holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Handle {
    Lo,
    Hi,
    /// Pressed where the two knobs overlap, at this fraction: which one is
    /// held is decided by the first move's direction (left -> `Lo`), since
    /// from the press alone either is what the user sees under the
    /// pointer, and the wrong one cannot move that way.
    Either(f64),
    /// The band between the knobs, pressed at stop `grab_stop` while the
    /// selection was `lo..hi`: both knobs move by the stops the pointer
    /// has crossed since, the width held.
    Band { grab_stop: usize, lo: usize, hi: usize },
}

/// Where the knob at stop `i` sits along the track.
pub fn knob_frac(i: usize) -> f64 {
    income_fraction(slider_stops()[i])
}

/// The stop nearest track fraction `frac`, clamped into the slider.
pub fn stop_at(frac: f64) -> usize {
    let (first, last) = (VIRIDIS_STOPS[0].0, VIRIDIS_STOPS[VIRIDIS_STOPS.len() - 1].0);
    let t = if frac.is_nan() { 0.0 } else { frac.clamp(0.0, 1.0) };
    IncomeRange::nearest_stop(first + t * (last - first))
}

/// [`grab_on`] on the native legend's narrowest bar ([`Track::default_bar`]).
pub fn grab(range: IncomeRange, frac: f64) -> Option<Handle> {
    grab_on(Track::default_bar(), range, frac)
}

/// What a press at `frac` holds: the knob under it -- or, where both knobs
/// overlap, [`Handle::Either`], for the first move to decide -- then the
/// band between the knobs, and `None` on the bare track outside them. The
/// knobs win: each keeps its whole face, even the half over the band, so
/// the band is only what lies between the two faces -- nothing at all once
/// the knobs touch.
pub fn grab_on(track: Track, range: IncomeRange, frac: f64) -> Option<Handle> {
    let (lo_f, hi_f) = (knob_frac(range.lo), knob_frac(range.hi));
    let half = track.knob / 2.0;
    let (on_lo, on_hi) = ((frac - lo_f).abs() <= half, (frac - hi_f).abs() <= half);
    match (on_lo, on_hi) {
        (true, true) => Some(Handle::Either(frac)),
        (true, false) => Some(Handle::Lo),
        (false, true) => Some(Handle::Hi),
        (false, false) if lo_f < frac && frac < hi_f => {
            Some(Handle::Band { grab_stop: stop_at(frac), lo: range.lo, hi: range.hi })
        }
        (false, false) => None,
    }
}

/// [`drag_on`] on the native legend's narrowest bar.
pub fn drag(range: IncomeRange, held: Option<Handle>, frac: f64) -> (Option<Handle>, IncomeRange) {
    drag_on(Track::default_bar(), range, held, frac)
}

/// The pointer at `frac` with the button held: what is held now (the knob
/// nearest the pointer, if nothing was) and the range that makes. A held
/// band moves by whole stops -- the pointer's stop less the press's -- from
/// where it was pressed, and stops at either end of the slider rather than
/// narrowing. The knobs never cross.
pub fn drag_on(track: Track, range: IncomeRange, held: Option<Handle>, frac: f64) -> (Option<Handle>, IncomeRange) {
    let last = slider_stops().len() - 1;
    if let Some(Handle::Band { grab_stop, lo, hi }) = held {
        let width = hi - lo;
        let moved = stop_at(frac) as isize - grab_stop as isize;
        let lo = (lo as isize + moved).clamp(0, (last - width) as isize) as usize;
        return (held, IncomeRange { lo, hi: lo + width });
    }
    let mut held = held;
    if let Some(Handle::Either(from)) = held {
        if (frac - from).abs() < track.px {
            return (held, range);
        }
        held = Some(if frac < from { Handle::Lo } else { Handle::Hi });
    }
    let stop = stop_at(frac);
    let handle = *held.get_or_insert_with(|| {
        let (lo, hi) = (range.lo.abs_diff(stop), range.hi.abs_diff(stop));
        if lo < hi || (lo == hi && stop < range.lo) { Handle::Lo } else { Handle::Hi }
    });
    let mut range = range;
    match handle {
        Handle::Lo => range.lo = stop.min(range.hi - 1),
        Handle::Hi => range.hi = stop.clamp(range.lo + 1, last),
        Handle::Either(_) | Handle::Band { .. } => unreachable!("decided above"),
    }
    (held, range)
}

/// `?range=LO,HI` in dollars (`40000,90000`): each end snapped to its
/// nearest handle position. `None` unless both are finite, `LO < HI`, and
/// they snap to two different stops.
pub fn range_from_query(text: &str) -> Option<IncomeRange> {
    let (a, b) = text.split_once(',')?;
    let parse = |s: &str| s.trim().parse::<f64>().ok().filter(|v| v.is_finite());
    let (lo, hi) = (parse(a)?, parse(b)?);
    if lo >= hi {
        return None;
    }
    let (lo, hi) = (IncomeRange::nearest_stop(lo), IncomeRange::nearest_stop(hi));
    (lo < hi).then_some(IncomeRange { lo, hi })
}

/// A range from two handle positions, `None` unless `lo < hi <= last`.
pub fn range_of(lo: usize, hi: usize) -> Option<IncomeRange> {
    (lo < hi && hi < slider_stops().len()).then_some(IncomeRange { lo, hi })
}

/// How the paint updates behind the slider went, summed over the census
/// sources since the page loaded (`SourcePaintPath`'s three).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathCounts {
    pub table_only: u64,
    pub rebake: u64,
    pub replan: u64,
}

/// The slider's state as the page draws it: `{"lo", "hi", "lo_frac",
/// "hi_frac", "label", "paths": {"table_only", "rebake", "replan"}}`.
pub fn state_json(range: IncomeRange, paths: PathCounts) -> String {
    serde_json::json!({
        "lo": range.lo,
        "hi": range.hi,
        "lo_frac": knob_frac(range.lo),
        "hi_frac": knob_frac(range.hi),
        "label": range.caption(),
        "paths": { "table_only": paths.table_only, "rebake": paths.rebake, "replan": paths.replan },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_band_drag_moves_both_handles_by_whole_stops_and_stops_at_the_ends() {
        let r = IncomeRange { lo: 50, hi: 90 };
        let mid = crate::census::ramp::income_fraction(crate::census::ramp::slider_stops()[70]);
        let held = grab(r, mid);
        assert!(matches!(held, Some(Handle::Band { .. })));
        let (_, moved) = drag(r, held, mid + 0.1);
        assert_eq!(moved.hi - moved.lo, 40, "the band keeps its width");
        assert!(moved.lo > 50);
        let (_, pinned) = drag(r, held, 5.0);
        assert_eq!(pinned.hi, crate::census::ramp::slider_stops().len() - 1, "clamped at the top");
        assert_eq!(pinned.hi - pinned.lo, 40);
    }

    #[test]
    fn handles_never_cross() {
        let r = IncomeRange { lo: 50, hi: 60 };
        let (_, after) = drag(r, Some(Handle::Lo), 0.99);
        assert!(after.lo < after.hi);
    }

    #[test]
    fn range_from_query_snaps_to_the_nearest_stops() {
        let r = range_from_query("40000,90000").unwrap();
        let s = crate::census::ramp::slider_stops();
        assert!((s[r.lo] - 40_000.0).abs() < 1_000.0 && (s[r.hi] - 90_000.0).abs() < 1_000.0);
        assert!(range_from_query("90000,40000").is_none());
        assert!(range_from_query("abc").is_none());
    }

    #[test]
    fn a_range_query_that_snaps_to_one_stop_or_is_not_finite_is_refused() {
        assert!(range_from_query("40000,40001").is_none(), "one stop");
        assert!(range_from_query("0,inf").is_none());
        assert!(range_from_query("0,NaN").is_none());
        assert!(range_from_query("1,2,3").is_none());
        assert_eq!(range_from_query("0,1e9"), Some(IncomeRange::full()), "clamped into the slider");
    }

    #[test]
    fn range_of_needs_two_ordered_stops_on_the_slider() {
        assert_eq!(range_of(3, 9), Some(IncomeRange { lo: 3, hi: 9 }));
        assert_eq!(range_of(9, 9), None);
        assert_eq!(range_of(9, 3), None);
        assert_eq!(range_of(0, slider_stops().len()), None);
    }

    #[test]
    fn the_state_json_carries_the_knobs_the_caption_and_the_paths() {
        let r = IncomeRange { lo: 10, hi: 20 };
        let v: serde_json::Value =
            serde_json::from_str(&state_json(r, PathCounts { table_only: 5, rebake: 1, replan: 0 })).unwrap();
        assert_eq!(v["lo_frac"], knob_frac(10));
        assert_eq!(v["hi_frac"], knob_frac(20));
        assert_eq!(v["label"], r.caption());
        assert_eq!(v["paths"]["table_only"], 5);
        assert_eq!(v["paths"]["rebake"], 1);
        assert_eq!(v["paths"]["replan"], 0);
    }

    #[test]
    fn a_track_of_px_is_knob_and_pixel_over_the_bar() {
        let t = Track::of_px(280.0);
        assert!((t.knob - 14.0 / 280.0).abs() < 1e-12 && (t.px - 1.0 / 280.0).abs() < 1e-12);
        assert_eq!(Track::of_px(0.0).px, 1.0, "a collapsed bar is one pixel");
        assert_eq!(Track::of_px(f64::NAN), Track::default_bar());
    }

    /// The native demo's press-and-drag tests (`slider_geometry`), on the
    /// fraction axis: a 300 px bar, about 1.2 px a stop, so a knob covers
    /// about 12 stops.
    mod ported {
        use super::*;

        const BAR: f64 = 300.0;
        const LAST: usize = 249;

        fn track() -> Track {
            Track::of_px(BAR)
        }

        /// `px` pixels as a fraction of the bar.
        fn px(px: f64) -> f64 {
            px / BAR
        }

        fn press_drag(range: IncomeRange, from: f64, path: &[f64]) -> Vec<IncomeRange> {
            let mut held = grab_on(track(), range, from);
            let mut range = range;
            let mut out = Vec::new();
            if held.is_none() {
                (held, range) = drag_on(track(), range, held, from);
                out.push(range);
            }
            for &x in path {
                (held, range) = drag_on(track(), range, held, x);
                out.push(range);
            }
            out
        }

        fn band_mid(r: IncomeRange) -> f64 {
            (knob_frac(r.lo) + knob_frac(r.hi)) / 2.0
        }

        #[test]
        fn a_band_drag_moves_both_handles_and_keeps_the_width() {
            let r = IncomeRange { lo: 60, hi: 120 };
            let x = band_mid(r);
            let steps = press_drag(r, x, &[x + px(30.0), x - px(50.0)]);
            for s in &steps {
                assert_eq!(s.hi - s.lo, 60, "the width holds: {steps:?}");
            }
            assert!(steps[0].lo > 60, "right moves the band right: {steps:?}");
            assert!(steps[1].lo < 60, "left moves it left: {steps:?}");
        }

        #[test]
        fn a_band_drag_clamps_at_each_end_without_shrinking() {
            let r = IncomeRange { lo: 60, hi: 120 };
            let x = band_mid(r);
            let steps = press_drag(r, x, &[px(-1000.0), x, px(5000.0), x]);
            assert_eq!(steps[0], IncomeRange { lo: 0, hi: 60 }, "pinned at the low end");
            assert_eq!(steps[1], r, "back to the press point, back to the start");
            assert_eq!(steps[2], IncomeRange { lo: LAST - 60, hi: LAST }, "pinned at the high end");
            assert_eq!(steps[3], r);
        }

        #[test]
        fn a_band_drag_snaps_to_stops() {
            let r = IncomeRange { lo: 100, hi: 140 };
            let x = band_mid(r);
            let path: Vec<f64> = (1..200).map(|k| x + px(k as f64 * 0.37)).collect();
            let steps = press_drag(r, x, &path);
            for (s, &p) in steps.iter().zip(&path) {
                let moved = stop_at(p) as isize - stop_at(x) as isize;
                let lo = (100 + moved).clamp(0, (LAST - 40) as isize) as usize;
                assert_eq!(*s, IncomeRange { lo, hi: lo + 40 }, "at {p}");
            }
            assert!(steps.last().unwrap().lo > 150, "the sweep crossed many stops: {:?}", steps.last());
        }

        #[test]
        fn a_press_on_a_handle_still_grabs_that_handle() {
            let r = IncomeRange { lo: 60, hi: 180 };
            let (lo_x, hi_x) = (knob_frac(r.lo), knob_frac(r.hi));
            let half = px(KNOB_PX / 2.0);
            for x in [lo_x, lo_x - half + px(0.5), lo_x + half - px(0.5)] {
                assert!(matches!(grab_on(track(), r, x), Some(Handle::Lo)), "x {x}");
                let s = press_drag(r, x, &[x + px(20.0)])[0];
                assert!(s.lo > 60 && s.hi == 180, "the low knob alone: {s:?}");
            }
            for x in [hi_x, hi_x + half - px(0.5), hi_x - half + px(0.5)] {
                assert!(matches!(grab_on(track(), r, x), Some(Handle::Hi)), "x {x}");
                let s = press_drag(r, x, &[x - px(20.0)])[0];
                assert!(s.hi < 180 && s.lo == 60, "the high knob alone: {s:?}");
            }
            let x = lo_x + half + px(1.0);
            let s = press_drag(r, x, &[x + px(20.0)]);
            assert!(s[0].lo > 60 && s[0].hi > 180 && s[0].hi - s[0].lo == 120, "just past the knob, the band: {s:?}");
        }

        #[test]
        fn a_narrow_band_still_lets_you_pick_a_handle() {
            let lo = 100;
            let knob = px(KNOB_PX);
            let hi = (lo + 1..=LAST).find(|&h| knob_frac(h) - knob_frac(lo) > knob + px(3.0)).unwrap();
            let r = IncomeRange { lo, hi };
            let (lo_x, hi_x) = (knob_frac(lo), knob_frac(hi));
            let s = press_drag(r, lo_x + knob / 2.0 - px(0.5), &[lo_x - px(20.0)])[0];
            assert!(s.lo < lo && s.hi == hi, "the low knob's inner edge: {s:?}");
            let s = press_drag(r, hi_x - knob / 2.0 + px(0.5), &[hi_x + px(20.0)])[0];
            assert!(s.hi > hi && s.lo == lo, "the high knob's inner edge: {s:?}");
            let x = band_mid(r);
            let s = press_drag(r, x, &[x + px(20.0)])[0];
            assert!(s.lo > lo && s.hi - s.lo == hi - lo, "the sliver between them, the band: {s:?}");
            let r = IncomeRange { lo: 100, hi: 103 };
            let x = band_mid(r);
            assert!(matches!(grab_on(track(), r, x), Some(Handle::Either(_))));
            let s = press_drag(r, x, &[x - px(20.0)]);
            assert!(s[0].lo < 100 && s[0].hi == 103, "{s:?}");
        }

        /// A press on the bare track outside the knobs jumps the nearer
        /// knob there, as native's `press_at` does.
        #[test]
        fn a_press_on_the_bare_track_jumps_the_nearer_knob() {
            let r = IncomeRange { lo: 100, hi: 140 };
            assert_eq!(grab_on(track(), r, px(5.0)), None);
            let s = press_drag(r, px(5.0), &[]);
            assert!(s[0].lo < 100 && s[0].hi == 140, "{s:?}");
        }
    }
}
