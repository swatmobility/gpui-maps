//! Level selection and the view-driven `ppd` (population-per-dot) controller.
//!
//! `level_for_zoom` picks which pyramid level's tiles to fetch for a camera
//! zoom. `PpdController` smooths the population-per-dot budget frame to
//! frame from the visible population, so a slow zoom or pan does not rebuild
//! dot buffers every frame; `ppd_build_for`/`needs_rebuild` pick the ppd a
//! tile is actually built at and the window it stays valid over.

use super::grid;
use super::{GridDescriptor, GridTileKey};

/// The pyramid level to fetch at camera `zoom`: `clamp(floor(crossover_zoom -
/// zoom), 0, levels - 1)`. Level 0 is the finest, base-resolution tiles;
/// coarser levels (bigger cells, fewer of them) take over as the camera
/// zooms out past the descriptor's crossover.
pub fn level_for_zoom(d: &GridDescriptor, zoom: f64) -> u8 {
    let raw = (grid::crossover_zoom(d.base_arcsec) - zoom).floor();
    raw.clamp(0.0, (d.levels - 1) as f64) as u8
}

/// Total visible population across a frame's tiles: each tile's total count
/// weighted by how much of it is on screen (the caller computes the
/// fraction from the tile's clipped screen area; 1.0 for a fully visible
/// tile, 0.0 for one entirely off-screen).
pub fn visible_pop(tiles: &[(GridTileKey, f64, f64)]) -> f64 {
    tiles.iter().map(|(_, total, frac)| total * frac).sum()
}

/// Smoothed population-per-dot budget: how many people one dot represents
/// this frame. Driven by the visible population so the total dot count on
/// screen stays near a fixed budget regardless of zoom, smoothed in log
/// space so it doesn't jump, and held still by hysteresis near its current
/// value so per-frame noise in the visible-population estimate doesn't
/// creep it across a rebuild threshold.
pub struct PpdController {
    budget_dots: f64,
    ppd: f64,
    /// Seconds the target has sat at the floor (1) inside the band: after
    /// [`FLOOR_DWELL_S`] the ppd takes it ([`Self::update`]).
    floor_held_s: f64,
}

/// How long a target of 1 inside the ±10 % band must hold before the ppd
/// drops onto it (T8 review F2): a pan whose view population wobbles
/// across the budget would otherwise track it frame by frame between 1
/// and 1.11 and blink up to a tenth of the dots.
pub const FLOOR_DWELL_S: f64 = 0.25;

impl PpdController {
    /// `budget_dots` below one (or NaN) is held as one: a zero budget
    /// would make every ppd infinite.
    pub fn new(budget_dots: f64) -> Self {
        Self { budget_dots: budget_dots.max(1.0), ppd: 1.0, floor_held_s: 0.0 }
    }

    /// Sets ppd straight to `max(1, visible_pop / budget_dots)` and returns
    /// it: the FIRST estimate a layer gets, so its first tiles are not
    /// built at ppd 1 (billions of slots on a world view) while the eased
    /// value climbs from 1 over the next second.
    pub fn snap(&mut self, visible_pop: f64) -> f32 {
        self.ppd = (visible_pop / self.budget_dots).max(1.0);
        self.ppd as f32
    }

    /// The current smoothed ppd, without updating it.
    pub fn ppd(&self) -> f32 {
        self.ppd as f32
    }

    /// Returns the smoothed ppd for this frame. `dt_s` is seconds since the
    /// last frame. Target is `max(1, visible_pop / budget_dots)`; the
    /// current value moves towards it in log space with a 0.25 s time
    /// constant (`ppd += (target - ppd) * (1 - exp(-dt / 0.25))`, computed
    /// on the logs); a target within ±10 % of the current value is ignored
    /// -- except the floor: a target of 1 inside the band held for
    /// [`FLOOR_DWELL_S`] is taken, so a view that fits the budget does read
    /// one dot per person (T8 fix round 0: the band held a dense Manhattan
    /// view at 1.08 for good).
    pub fn update(&mut self, visible_pop: f64, dt_s: f64) -> f32 {
        let target = (visible_pop / self.budget_dots).max(1.0);
        let floor_in_band = target == 1.0 && self.ppd > 1.0 && self.ppd - 1.0 <= 0.1 * self.ppd;
        self.floor_held_s = if floor_in_band { self.floor_held_s + dt_s.max(0.0) } else { 0.0 };
        if floor_in_band {
            if self.floor_held_s >= FLOOR_DWELL_S {
                self.ppd = 1.0;
                self.floor_held_s = 0.0;
            }
        } else if (target - self.ppd).abs() > 0.1 * self.ppd {
            let log_ppd = self.ppd.ln();
            let log_target = target.ln();
            let alpha = 1.0 - (-dt_s / 0.25).exp();
            self.ppd = (log_ppd + (log_target - log_ppd) * alpha).exp().max(1.0);
        }
        self.ppd as f32
    }
}

/// The ppd a tile's dot slots are built at for view `ppd`: `ppd / √2`,
/// never less than 1. That puts `ppd` in the MIDDLE (geometrically) of
/// the window [`needs_rebuild`] accepts, `[built, 2·built]` -- about
/// ±41 % either way -- so a view ppd wobbling around any value rebuilds
/// once and then holds.
///
/// Not floored to a power of two any more (the plan's rule): a fresh
/// build then sat at the window's bottom edge, `ppd ∈ [built, 2·built)`,
/// and a wobble across a power of two rebuilt on every crossing both ways
/// (19 rebuilds in 10 s of ±20 % at 1 Hz, review round 1). Nothing needs a
/// power of two: `build_blocks` takes `ceil(pop / ppd_build)` slots, the
/// shader's quota is `pop / ppd` of the VIEW, and `DotTileGpu`'s live
/// prefix compares `max_pop / first_dot` against the view ppd.
pub fn ppd_build_for(ppd: f32) -> f32 {
    (ppd / std::f32::consts::SQRT_2).max(1.0)
}

/// Whether a tile built at `built` must rebuild for the current view `ppd`:
/// true once the view asks for more detail than was built (`ppd < built`),
/// or so much less that the built buffer carries more than 2x the slots a
/// fresh build at `ppd` would need (`ppd > 2 * built`).
///
/// 2x, not the plan's 4x: MG-8 measured a tile built at `ppd / 4` with
/// ~3 M live dots at 14.9 ms on the globe, over the 12 ms criterion (the
/// partly-live block at each cell's edge is drawn), so the band was
/// tightened (progress.md ruling, Task 4/7).
pub fn needs_rebuild(built: f32, ppd: f32) -> bool {
    ppd < built || ppd > 2.0 * built
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T8 fix round 0: the ±10 % band never holds the ppd just above the
    /// floor when the view fits the budget.
    #[test]
    fn a_view_within_the_budget_reaches_one_through_the_band() {
        let mut c = PpdController::new(1_000.0);
        c.snap(1_080.0);
        assert!((c.ppd() - 1.08).abs() < 1e-6);
        // Held for the dwell (F2), not at once: a wobble across the budget
        // during a pan does not blink the dots.
        assert!((c.update(900.0, 0.1) - 1.08).abs() < 1e-6, "not yet");
        assert!((c.update(1_050.0, 0.1) - 1.08).abs() < 1e-6, "the wobble back up resets the dwell");
        assert!((c.update(900.0, 0.1) - 1.08).abs() < 1e-6);
        assert!((c.update(900.0, 0.1) - 1.08).abs() < 1e-6);
        assert_eq!(c.update(900.0, 0.1), 1.0, "0.3 s at the floor: taken");
        // The band's edge (1.11 is within 10 % of itself above 1).
        c.snap(1_110.0);
        assert_eq!(c.update(700.0, 0.3), 1.0, "1.11 -> 1");
        // Above the floor the band still holds.
        c.snap(1_500.0);
        assert!((c.update(1_450.0, 0.016) - 1.5).abs() < 1e-6);
    }

    /// Final review: a zero budget would make `visible_pop / 0` an
    /// infinite ppd; it is clamped to one dot.
    #[test]
    fn a_zero_budget_is_clamped_to_one_dot() {
        let mut c = PpdController::new(0.0);
        assert_eq!(c.snap(1e6), 1e6);
        assert!(c.update(2e6, 1.0).is_finite());
        assert_eq!(PpdController::new(f64::NAN).snap(5.0), 5.0);
    }

    /// Same field values as `grid::tests::d` (Task 1): 10 levels at 3″, 7 at
    /// 30″, `max_data_zoom` from `base_data_zoom`.
    fn test_descriptor(arcsec: f64) -> GridDescriptor {
        GridDescriptor {
            base_arcsec: arcsec,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: if arcsec == 3.0 { 10 } else { 7 },
            max_data_zoom: grid::base_data_zoom(arcsec),
            epochs: vec![2025],
            attribution: String::new(),
        }
    }

    #[test]
    fn level_rule_matches_the_spec_table() {
        let d3 = test_descriptor(3.0);
        assert_eq!(level_for_zoom(&d3, 3.0), 6); // floor(9.72 - 3) = 6
        assert_eq!(level_for_zoom(&d3, 9.0), 0);
        assert_eq!(level_for_zoom(&d3, 16.0), 0);
        assert_eq!(level_for_zoom(&d3, 0.0), 9);
        let d30 = test_descriptor(30.0);
        assert_eq!(level_for_zoom(&d30, 6.0), 0);
        assert_eq!(level_for_zoom(&d30, 0.0), 6);
    }

    #[test]
    fn a_fresh_build_puts_the_view_mid_window() {
        assert_eq!(ppd_build_for(1.0), 1.0);
        assert_eq!(ppd_build_for(1.2), 1.0);
        for ppd in [1.5f32, 3.9, 2000.0, 1024.0] {
            let built = ppd_build_for(ppd);
            assert!((ppd / built - std::f32::consts::SQRT_2).abs() < 1e-5, "{ppd} built at {built}");
            assert!(!needs_rebuild(built, ppd * 1.4) && !needs_rebuild(built, ppd / 1.4));
        }
        assert!(!needs_rebuild(1024.0, 2000.0));
        assert!(needs_rebuild(1024.0, 1000.0));
        assert!(needs_rebuild(1024.0, 4097.0));
        // The 2x band (MG-8 ruling): a tile built at ppd/2 still fits.
        assert!(!needs_rebuild(1024.0, 2048.0));
        assert!(needs_rebuild(1024.0, 2049.0));
    }

    #[test]
    fn a_first_estimate_snaps_instead_of_easing_from_one() {
        let mut c = PpdController::new(4_000_000.0);
        assert_eq!(c.snap(8.0e9), 2_000.0);
        // and the next update eases from there, not from 1
        let p = c.update(8.0e9 * 1.02, 1.0 / 60.0);
        assert!((p - 2_000.0).abs() < 1.0, "{p}");
    }

    /// Rebuild EVENTS over 10 s at 60 fps while the visible population
    /// wobbles by `amp` (a fraction) at `hz` around `centre_ppd * budget`,
    /// through the real controller (±10 % dead band, 0.25 s time
    /// constant), `ppd_build_for` and `needs_rebuild`.
    fn rebuilds_under_wobble(centre_ppd: f64, amp: f64, hz: f64) -> u32 {
        let budget = 4_000_000.0;
        let mut c = PpdController::new(budget);
        let mut built = ppd_build_for(c.snap(centre_ppd * budget));
        let mut rebuilds = 0;
        for f in 0..600 {
            let t = f64::from(f) / 60.0;
            let wobble = 1.0 + amp * (2.0 * std::f64::consts::PI * hz * t).sin();
            let ppd = c.update(centre_ppd * budget * wobble, 1.0 / 60.0);
            if needs_rebuild(built, ppd) {
                rebuilds += 1;
                built = ppd_build_for(ppd);
            }
        }
        rebuilds
    }

    /// Review focus 3: a view ppd oscillating around a power of two --
    /// where the old floor-to-a-power-of-two build put every crossing on
    /// a rebuild threshold -- rebuilds at most once in 10 s. The wobble is
    /// well outside the controller's ±10 % dead band, so it reaches ppd.
    #[test]
    fn hysteresis_prevents_a_rebuild_storm_at_the_threshold() {
        for (amp, hz) in [(0.20, 1.0), (0.30, 0.5)] {
            for centre in [1024.0, 1024.0 * 1.2, 1024.0 / 1.2, 700.0] {
                let n = rebuilds_under_wobble(centre, amp, hz);
                assert!(n <= 1, "{n} rebuilds in 10 s of ±{:.0} % at {hz} Hz around ppd {centre}", amp * 100.0);
            }
        }
    }

    #[test]
    fn ppd_never_falls_below_one() {
        let mut c = PpdController::new(4_000_000.0);
        for _ in 0..120 {
            assert!(c.update(10.0, 1.0 / 60.0) >= 1.0);
        }
    }
}
