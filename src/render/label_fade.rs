//! Opacity fade-in for labels and symbols that arrive after the frame that
//! first wanted them.
//!
//! The label pipeline admits new labels under per-frame budgets (shaping and
//! SDF rasters in `symbol_collect`, glyph rasters for glyphon in
//! `render::text_glyph_budget`), so at a zoom-bucket crossing the new set
//! lands over one to several frames. Drawn at full opacity on arrival, each
//! batch pops. MapLibre fades every symbol's opacity towards its placement
//! target over `fadeDuration` (300 ms by default) with the frame clock
//! (`src/symbol/placement.ts`, `updateOpacities` / `OpacityState`,
//! BSD-3-Clause, (c) MapLibre contributors); this is the same idea for the
//! draw lists this engine builds.
//!
//! # Identity
//!
//! MapLibre keys a symbol's opacity by its cross-tile id. The draw lists
//! here carry no id, so a symbol is matched to last frame's by a content
//! hash (the label's text and face, not its size, so a bucket crossing
//! that re-shapes it does not re-fade it; an icon's atlas rectangle) and
//! then the NEAREST last-frame position with that hash, within a tolerance.
//! Two road shields with the same number swap opacities if they swap
//! places; both are steady, so both stay at 1.
//!
//! # Rules
//!
//! - A symbol matched to last frame's continues from its opacity, rising
//!   `dt / duration` a frame until it reaches 1. A steady symbol is at 1 and
//!   stays there.
//! - A new symbol starts from one frame's worth of rise, not from 1.
//! - The first frame this state ever sees starts everything at 1: the fade
//!   is for late arrivals, not the map's first paint (and a one-frame
//!   capture must not come out blank).
//! - A symbol that stops being drawn keeps its opacity, decaying at the same
//!   rate, so a one- or two-frame gap resumes where it was rather than
//!   restarting from 0. It is not drawn while absent: fading OUT would need
//!   last frame's screen position, which is stale the moment the camera
//!   moves.
//! - [`LabelFade::end_frame`] says whether any drawn symbol is still below
//!   1, which is exactly when another frame must be requested. Once every
//!   fade has finished it says no, so a still map stops redrawing.

use std::collections::HashMap;

/// How long a new symbol takes to reach full opacity, in seconds.
///
/// MapLibre's default `fadeDuration` is 300 ms. Shorter here because the
/// budgets already add one to several frames of latency before the fade
/// starts; 200 ms keeps arrival-to-opaque near MapLibre's total.
pub(crate) const LABEL_FADE_SECONDS: f64 = 0.2;

/// The longest step one frame advances a fade by. A map idle for seconds
/// and then panned must not jump a new label straight to opaque.
const MAX_FRAME_STEP_SECONDS: f64 = 1.0 / 30.0;

#[derive(Clone, Copy)]
struct Slot {
    x: f32,
    y: f32,
    opacity: f32,
    /// Matched (and so drawn) this frame.
    drawn: bool,
}

/// Per-symbol fade state, carried from frame to frame.
pub(crate) struct LabelFade {
    duration_s: f64,
    last_now_s: Option<f64>,
    /// `dt / duration` for the frame in progress.
    step: f32,
    first_frame: bool,
    /// Match radius, in the same pixels as the positions given.
    tolerance: f32,
    slots: HashMap<u64, Vec<Slot>>,
    animating: bool,
}

impl Default for LabelFade {
    fn default() -> Self {
        Self::new(LABEL_FADE_SECONDS)
    }
}

impl LabelFade {
    pub(crate) fn new(duration_s: f64) -> Self {
        Self {
            duration_s: duration_s.max(1e-3),
            last_now_s: None,
            step: 1.0,
            first_frame: true,
            tolerance: 128.0,
            slots: HashMap::new(),
            animating: false,
        }
    }

    /// Starts a frame at `now_s` (any monotonic clock, in seconds).
    /// `tolerance_px` is how far a symbol may move between two frames and
    /// still be the same symbol.
    pub(crate) fn begin_frame(&mut self, now_s: f64, tolerance_px: f32) {
        self.first_frame = self.last_now_s.is_none();
        let dt = self.last_now_s.map_or(0.0, |last| (now_s - last).clamp(0.0, MAX_FRAME_STEP_SECONDS));
        self.last_now_s = Some(now_s);
        self.step = (dt / self.duration_s) as f32;
        self.tolerance = tolerance_px.max(1.0);
        self.animating = false;
        // Absent last frame: decay; gone: forget.
        for slots in self.slots.values_mut() {
            for slot in slots.iter_mut() {
                if !slot.drawn {
                    slot.opacity -= self.step;
                }
                slot.drawn = false;
            }
            slots.retain(|slot| slot.opacity > 0.0);
        }
        self.slots.retain(|_, slots| !slots.is_empty());
    }

    /// The opacity to draw the symbol `id` at `(x, y)` with this frame, in
    /// `0.0..=1.0`. Call once per drawn symbol, between `begin_frame` and
    /// `end_frame`.
    pub(crate) fn opacity(&mut self, id: u64, x: f32, y: f32) -> f32 {
        let first_frame = self.first_frame;
        let step = self.step;
        let tolerance_sq = self.tolerance * self.tolerance;
        let slots = self.slots.entry(id).or_default();
        let nearest = slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| !slot.drawn)
            .map(|(index, slot)| (index, (slot.x - x).powi(2) + (slot.y - y).powi(2)))
            .filter(|(_, d2)| *d2 <= tolerance_sq)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index);
        let opacity = match nearest {
            Some(index) => {
                let slot = &mut slots[index];
                slot.opacity = (slot.opacity.max(0.0) + step).min(1.0);
                slot.x = x;
                slot.y = y;
                slot.drawn = true;
                slot.opacity
            }
            None => {
                let opacity = if first_frame { 1.0 } else { step.clamp(0.0, 1.0) };
                slots.push(Slot { x, y, opacity, drawn: true });
                opacity
            }
        };
        if opacity < 1.0 {
            self.animating = true;
        }
        opacity
    }

    /// Ends the frame. True while any symbol drawn this frame is still
    /// fading in, so the caller must request another frame.
    pub(crate) fn end_frame(&mut self) -> bool {
        self.animating
    }
}

/// How often a fade-only redraw may actually be requested, in seconds.
///
/// A 200 ms fade needs about 12 frames at 60 fps to reach 1. Measured on the
/// DC/LA census sweeps (2026-09-26): with nothing capping it, the zoom-out
/// legs drew 3,450-3,500 frames against 2,570-2,620 before the fade existed,
/// at "about 120 fps instead of 55-75" -- every `LabelFade::end_frame` tick
/// woke a `window.request_animation_frame`/`request_redraw` unconditionally,
/// so a ProMotion display's 120 Hz cadence (or a browser tab's own rAF rate)
/// drew the fade twice as often as it needed to. [`FadeRedrawScheduler`]
/// admits at most one such request every this-many seconds, independent of
/// how fast the host offers to redraw.
pub(crate) const MIN_REDRAW_INTERVAL_SECONDS: f64 = 1.0 / 60.0;

/// Caps how often a shell actually asks for another frame while a fade is
/// the ONLY reason to redraw.
///
/// [`LabelFade::end_frame`] says whether a fade is still running; a caller
/// with no other pending reason to redraw (no gesture, no bake backlog, no
/// network arrival) asks this whether to act on that THIS tick. Any other
/// redraw trigger bypasses this scheduler entirely -- it only throttles the
/// fade's own requests, never a real one.
pub(crate) struct FadeRedrawScheduler {
    last_requested_s: Option<f64>,
}

impl Default for FadeRedrawScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl FadeRedrawScheduler {
    pub(crate) fn new() -> Self {
        Self { last_requested_s: None }
    }

    /// `now_s`: the same monotonic clock passed to [`LabelFade::begin_frame`].
    /// `fade_animating`: this frame's [`LabelFade::end_frame`] result.
    ///
    /// Returns whether the caller should actually request another frame now.
    /// Once the fade stops animating this resets, so a fresh fade starting
    /// later is not held back by the last one's timing.
    pub(crate) fn should_request_redraw(&mut self, now_s: f64, fade_animating: bool) -> bool {
        if !fade_animating {
            self.last_requested_s = None;
            return false;
        }
        // A small epsilon so a driver ticking at EXACTLY the interval (a
        // display genuinely locked to 60 Hz) is not starved by float error.
        let due = self
            .last_requested_s
            .is_none_or(|last| now_s - last >= MIN_REDRAW_INTERVAL_SECONDS - 1e-9);
        if due {
            self.last_requested_s = Some(now_s);
        }
        due
    }
}

/// What an on-demand frame loop must do after a frame, given this frame's
/// [`LabelFade::end_frame`] result. See [`FadeRedrawScheduler::follow_up`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum FadeFollowUp {
    /// No symbol is fading: ask for nothing.
    Idle,
    /// Ask for the next frame now.
    RedrawNow,
    /// The cap turned this frame's request down: wake at `at_s` (the same
    /// clock as `now_s`) and draw then. A loop that dropped this instead
    /// would leave every fading label frozen part-way to opaque until an
    /// unrelated redraw -- which on a still map never comes.
    WakeAt(f64),
}

impl FadeRedrawScheduler {
    /// [`Self::should_request_redraw`], plus the wake a turned-down request
    /// is owed. The native and iOS shells keep their own timers for this
    /// (`MapView::paint`'s delayed wake, `fade_wake_at`); the web shell's
    /// `requestAnimationFrame` loop only draws when `needs_redraw` is set,
    /// so it needs the deadline too, and this is where it comes from.
    pub(crate) fn follow_up(&mut self, now_s: f64, fade_animating: bool) -> FadeFollowUp {
        if !fade_animating {
            self.should_request_redraw(now_s, false);
            FadeFollowUp::Idle
        } else if self.should_request_redraw(now_s, true) {
            FadeFollowUp::RedrawNow
        } else {
            let last = self.last_requested_s.unwrap_or(now_s);
            FadeFollowUp::WakeAt(last + MIN_REDRAW_INTERVAL_SECONDS)
        }
    }
}

/// A stable 64-bit id for a symbol's content: hash of `parts` under a
/// `kind` tag, so a label and an icon never share an id.
pub(crate) fn fade_id<T: std::hash::Hash + ?Sized>(kind: u8, parts: &T) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut hasher);
    parts.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: f64 = 1.0 / 60.0;

    /// A map already drawing one steady label "a" at (100, 100).
    fn warmed() -> (LabelFade, f64) {
        let mut fade = LabelFade::new(0.2);
        let mut now = 10.0;
        fade.begin_frame(now, 64.0);
        assert_eq!(fade.opacity(1, 100.0, 100.0), 1.0, "the first frame is not faded");
        assert!(!fade.end_frame());
        now += FRAME;
        (fade, now)
    }

    #[test]
    fn a_new_label_rises_monotonically_to_one_and_redraws_stop_after_the_fade() {
        let (mut fade, mut now) = warmed();
        let mut seen = Vec::new();
        let mut redraw_frames = 0;
        for _ in 0..40 {
            fade.begin_frame(now, 64.0);
            assert_eq!(fade.opacity(1, 100.0, 100.0), 1.0, "a steady label stays at 1");
            seen.push(fade.opacity(2, 400.0, 300.0));
            if fade.end_frame() {
                redraw_frames += 1;
            }
            now += FRAME;
        }
        assert!(seen[0] > 0.0 && seen[0] < 0.2, "starts near 0, not at 1: {}", seen[0]);
        assert!(seen.windows(2).all(|w| w[1] >= w[0]), "monotonic: {seen:?}");
        assert_eq!(*seen.last().unwrap(), 1.0);
        // 0.2 s at 60 fps is 12 frames; the frames after that ask for nothing.
        assert!((11..=12).contains(&redraw_frames), "{redraw_frames}: {seen:?}");
        let reached = seen.iter().position(|o| *o >= 1.0).unwrap();
        assert!((10..=12).contains(&reached), "reached 1 at frame {reached}");
    }

    #[test]
    fn a_label_that_moves_with_the_camera_keeps_its_opacity() {
        let (mut fade, mut now) = warmed();
        for frame in 0..30 {
            fade.begin_frame(now, 64.0);
            // Drifts 20 px a frame, like a zoom gesture near the edge.
            assert_eq!(fade.opacity(1, 100.0 + 20.0 * frame as f32, 100.0), 1.0);
            assert!(!fade.end_frame());
            now += FRAME;
        }
    }

    #[test]
    fn a_short_gap_resumes_instead_of_restarting() {
        let (mut fade, mut now) = warmed();
        fade.begin_frame(now, 64.0);
        assert!(!fade.end_frame(), "label 1 absent for one frame");
        now += FRAME;
        fade.begin_frame(now, 64.0);
        let back = fade.opacity(1, 100.0, 100.0);
        assert!(back > 0.85, "resumed near where it was: {back}");
    }

    #[test]
    fn a_same_text_label_far_away_is_a_new_label() {
        let (mut fade, now) = warmed();
        fade.begin_frame(now, 64.0);
        assert_eq!(fade.opacity(1, 100.0, 100.0), 1.0);
        assert!(fade.opacity(1, 900.0, 100.0) < 0.2);
        assert!(fade.end_frame());
    }

    #[test]
    fn an_idle_gap_does_not_skip_the_fade() {
        let (mut fade, now) = warmed();
        fade.begin_frame(now + 30.0, 64.0);
        let first = fade.opacity(2, 10.0, 10.0);
        assert!(first < 0.2, "{first}");
    }

    /// Driven at 120 Hz (a ProMotion display, or a browser tab's own rAF
    /// rate), the scheduler must still cap actual redraw requests to about
    /// 60 fps and stop entirely once the fade reaches 1 -- the regression
    /// measured on the DC/LA census sweeps (see this module's own doc on
    /// `MIN_REDRAW_INTERVAL_SECONDS`).
    #[test]
    fn fade_redraws_are_capped_and_stop_after_completion() {
        let (mut fade, mut now) = warmed();
        let mut gate = FadeRedrawScheduler::new();
        const FRAME_120HZ: f64 = 1.0 / 120.0;

        let mut requested = 0usize;
        let mut completed = false;
        for _ in 0..80 {
            fade.begin_frame(now, 64.0);
            let opacity = fade.opacity(2, 400.0, 300.0);
            let animating = fade.end_frame();
            let due = gate.should_request_redraw(now, animating);
            if completed {
                assert!(!due, "a redraw was requested after the fade finished");
            } else if due {
                requested += 1;
            }
            if opacity >= 1.0 {
                completed = true;
            }
            now += FRAME_120HZ;
        }

        assert!((1..=15).contains(&requested), "{requested} redraw requests for one fade");
    }

    #[test]
    fn no_fade_means_no_redraw_request() {
        let mut gate = FadeRedrawScheduler::new();
        assert!(!gate.should_request_redraw(0.0, false));
        assert!(!gate.should_request_redraw(1.0, false));
    }

    /// A display ticking faster than the cap (120 Hz) has every other
    /// frame's request turned down. Each one must come back as a wake, never
    /// as nothing: a loop that draws only on request would otherwise stop
    /// with the labels half faded in (seen on the web viewer as place names
    /// left ghosted under data layers).
    #[test]
    fn a_turned_down_fade_request_is_owed_a_wake_never_dropped() {
        let mut gate = FadeRedrawScheduler::new();
        let tick = 1.0 / 120.0;
        assert_eq!(gate.follow_up(10.0, true), FadeFollowUp::RedrawNow);
        match gate.follow_up(10.0 + tick, true) {
            FadeFollowUp::WakeAt(at) => {
                assert!(at > 10.0 + tick, "the wake is after this frame");
                assert!((at - (10.0 + MIN_REDRAW_INTERVAL_SECONDS)).abs() < 1e-9, "at the cap's interval: {at}");
                assert_eq!(gate.follow_up(at, true), FadeFollowUp::RedrawNow, "and the frame drawn then is admitted");
            }
            other => panic!("a fading frame turned down by the cap must be owed a wake, got {other:?}"),
        }
        assert_eq!(gate.follow_up(11.0, false), FadeFollowUp::Idle, "a finished fade asks for nothing");
        assert_eq!(gate.follow_up(11.0 + tick, true), FadeFollowUp::RedrawNow, "a later fade starts fresh");
    }

    /// The whole loop: a fade drawn only when the gate asks (now or at a
    /// wake) reaches full opacity even with a host offering frames at 120 Hz.
    #[test]
    fn a_fade_driven_only_by_its_own_requests_finishes() {
        let (mut fade, mut now) = warmed();
        let mut gate = FadeRedrawScheduler::new();
        fade.begin_frame(now, 64.0);
        fade.opacity(1, 100.0, 100.0);
        let mut opacity = fade.opacity(2, 300.0, 300.0);
        let mut follow = gate.follow_up(now, fade.end_frame());
        let mut frames = 0;
        while follow != FadeFollowUp::Idle {
            frames += 1;
            assert!(frames < 200, "the fade never finished");
            // Host ticks at 120 Hz; draw only when asked.
            now += 1.0 / 120.0;
            let due = match follow {
                FadeFollowUp::RedrawNow => true,
                FadeFollowUp::WakeAt(at) => now >= at - 1e-9,
                FadeFollowUp::Idle => false,
            };
            if !due {
                continue;
            }
            fade.begin_frame(now, 64.0);
            fade.opacity(1, 100.0, 100.0);
            opacity = fade.opacity(2, 300.0, 300.0);
            follow = gate.follow_up(now, fade.end_frame());
        }
        assert_eq!(opacity, 1.0, "the new label ends fully opaque");
    }
}
