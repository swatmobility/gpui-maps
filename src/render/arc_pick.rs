//! Picking origin-destination arcs (`render::arcs`): which arc of an
//! [`ArcSet`] is under a screen point, matching what `vs_arc` draws.
//!
//! - **What is hit**: the arc's polyline as the GPU cuts it this frame
//!   ([`segments_for`] its length class, [`segment_ratio`] spacing),
//!   projected by [`ArcFrame::project`] -- the CPU mirror of `arc_clip`, so
//!   flat, globe and the blend between them -- at the drawn width
//!   (`max(width, 1)` plus the shader's half-pixel feather) plus a caller's
//!   tolerance. The segment the flat arc fades out at the world seam, a
//!   point the planet hides, and an arc the filter window hides are not
//!   hit.
//! - **Cheap at Kepler's scale**: [`ArcPickIndex`] is built once per set: a
//!   grid over Web Mercator in which every arc is entered in each cell its
//!   CHORD crosses (the flat arc stands in the vertical plane over its
//!   chord, and the globe arc over the great circle near it), each cell's
//!   arcs sorted tallest first. A pick walks the cells under the cursor's
//!   ray -- from where it meets the ground back up to the tallest arc's
//!   peak -- and takes a cell's arcs only while they stand tall enough to
//!   reach the ray there, so the exact test sees tens of arcs, not 680k.
//!   `arcs.pick_ms` in the perf gate is that query at Kepler's UK camera.
//!
//! No gpui, wgpu or DOM: the browser shell picks the same way.

use std::collections::HashMap;
use std::f64::consts::TAU;

use glam::{DVec2, DVec3};

use super::arcs::{clip_segment, mercator, segment_ratio, segments_for, shortest_target_lon, ArcFrame, ArcSet, ArcStyle};

/// The most grid cells one arc is entered in; a longer chord (a
/// transcontinental arc on a fine grid) goes on [`ArcPickIndex`]'s
/// always-tested list instead.
pub const MAX_CELLS_PER_ARC: usize = 4096;
/// The coarsest and finest grids: `2^level` cells per world side.
const MIN_LEVEL: u8 = 2;
const MAX_LEVEL: u8 = 16;
/// How many cursor-ray samples a query walks at most.
const MAX_RAY_SAMPLES: usize = 20_000;
/// A world pixel at the camera's centre covers `1 / 2^zoom` world units;
/// under pitch a far pixel covers up to ~3x that. The broad phase widens
/// its slack by this much so the exact test never misses for it.
const PITCH_SLACK: f64 = 4.0;

/// A centreline this close to the cursor (logical px) passes through the
/// cursor's own pixel: the topmost such arc is the pick, nearer ones below
/// it notwithstanding.
pub const UNDER_CURSOR_PX: f64 = 0.5;

/// One arc under the cursor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArcHit {
    /// The arc's index in its [`ArcSet::instances`].
    pub index: usize,
    /// From the cursor to the drawn centreline, logical pixels.
    pub distance_px: f64,
}

/// A set's arcs bucketed by the Web Mercator cells their chords cross.
/// Build once per set ([`Self::new`]); a query reads it.
#[derive(Debug, Clone, Default)]
pub struct ArcPickIndex {
    /// `2^level` cells per world side.
    level: u8,
    /// Occupied cells, sorted; `offsets[k]..offsets[k + 1]` of `entries`
    /// are cell `keys[k]`'s arcs, tallest within the cell first.
    keys: Vec<u64>,
    offsets: Vec<u32>,
    entries: Vec<u32>,
    /// Per entry, the arc's greatest height inside that cell per unit of
    /// style height: `max sqrt(r (1 - r))` over the chord's stretch in the
    /// cell, times the Mercator chord. Times `height * WORLD_SIZE` it is
    /// the flat arc's height in world units there; times `height * 2 pi`
    /// it bounds the globe arc's in radii.
    heights: Vec<f32>,
    /// Arcs too long to enter cell by cell: tested on every query.
    long: Vec<u32>,
    /// Each instance's chord ends in Web Mercator (the target the shortest
    /// way), `f32`: what the world-space prefilter reads instead of two
    /// `ln(tan)` per arc per query. Conservative only -- the prefilter
    /// widens its slack by the rounding -- and the exact test reads the
    /// instance's own lon/lat.
    merc: Vec<[f32; 4]>,
    max_chord: f64,
    max_width_px: f32,
}

fn chord_ends(set: &ArcSet, i: usize) -> (DVec2, DVec2) {
    let a = &set.instances[i];
    let (slon, slat) = (f64::from(a.source[0]), f64::from(a.source[1]));
    let (tlon, tlat) = (f64::from(a.target[0]), f64::from(a.target[1]));
    (mercator(slon, slat), mercator(shortest_target_lon(slon, tlon), tlat))
}

fn ends(set: &ArcSet, i: usize) -> ((f64, f64), (f64, f64)) {
    let a = &set.instances[i];
    ((f64::from(a.source[0]), f64::from(a.source[1])), (f64::from(a.target[0]), f64::from(a.target[1])))
}

fn key(cx: u32, cy: u32) -> u64 {
    (u64::from(cy) << 32) | u64::from(cx)
}

impl ArcPickIndex {
    /// Indexes `set`: each arc in the cells its chord crosses, at a grid
    /// about twice as fine as the median chord.
    #[must_use]
    pub fn new(set: &ArcSet) -> Self {
        let n = set.len();
        let merc: Vec<[f32; 4]> = (0..n)
            .map(|i| {
                let (s, t) = chord_ends(set, i);
                [s.x as f32, s.y as f32, t.x as f32, t.y as f32]
            })
            .collect();
        let chord: Vec<f32> = merc.iter().map(|m| (m[2] - m[0]).hypot(m[3] - m[1])).collect();
        let mut sorted: Vec<f32> = chord.iter().copied().filter(|c| c.is_finite() && *c > 0.0).collect();
        let level = if sorted.is_empty() {
            MIN_LEVEL
        } else {
            let mid = sorted.len() / 2;
            let median = f64::from(*sorted.select_nth_unstable_by(mid, f32::total_cmp).1);
            (1.0 / (2.0 * median)).log2().round().clamp(f64::from(MIN_LEVEL), f64::from(MAX_LEVEL)) as u8
        };
        let side = 1u64 << level;
        let cell = 1.0 / side as f64;
        let mut pairs: Vec<(u64, f32, u32)> = Vec::with_capacity(n * 2);
        let mut long = Vec::new();
        // (cell, lowest r, highest r) of the chord's samples in it.
        let mut cells: Vec<(u64, f64, f64)> = Vec::new();
        let unit_lift = |r: f64| (r * (1.0 - r)).max(0.0).sqrt();
        for i in 0..n as u32 {
            let (s, t) = chord_ends(set, i as usize);
            let len = (t - s).length();
            let steps = (len / (cell * 0.5)).ceil();
            if !steps.is_finite() || steps as usize > MAX_CELLS_PER_ARC {
                long.push(i);
                continue;
            }
            let steps = steps as usize;
            cells.clear();
            for k in 0..=steps {
                let r = if steps == 0 { 0.0 } else { k as f64 / steps as f64 };
                let p = s.lerp(t, r);
                let x = p.x.rem_euclid(1.0);
                let cx = ((x * side as f64) as u64).min(side - 1) as u32;
                let cy = ((p.y.clamp(0.0, 1.0) * side as f64) as u64).min(side - 1) as u32;
                let k = key(cx, cy);
                match cells.iter_mut().find(|c| c.0 == k) {
                    Some(c) => {
                        c.1 = c.1.min(r);
                        c.2 = c.2.max(r);
                    }
                    None => cells.push((k, r, r)),
                }
            }
            // A sample stands for half a step either side of it.
            let half = if steps == 0 { 0.0 } else { 0.5 / steps as f64 };
            for &(k, r0, r1) in &cells {
                let (r0, r1) = ((r0 - half).max(0.0), (r1 + half).min(1.0));
                let top = if (r0..=r1).contains(&0.5) { 0.5 } else { unit_lift(r0).max(unit_lift(r1)) };
                pairs.push((k, (top * len) as f32, i));
            }
        }
        pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
        let mut keys = Vec::new();
        let mut offsets = Vec::new();
        let mut entries = Vec::with_capacity(pairs.len());
        let mut heights = Vec::with_capacity(pairs.len());
        for (k, h, i) in pairs {
            if keys.last() != Some(&k) {
                keys.push(k);
                offsets.push(entries.len() as u32);
            }
            entries.push(i);
            heights.push(h);
        }
        offsets.push(entries.len() as u32);
        let max_chord = chord.iter().copied().fold(0.0f32, f32::max);
        let max_width_px = set.instances.iter().map(|a| a.width_px).fold(0.0f32, f32::max);
        Self { level, keys, offsets, entries, heights, long, merc, max_chord: f64::from(max_chord), max_width_px }
    }

    /// The arcs indexed: the set's [`ArcSet::len`] when it was built.
    #[must_use]
    pub fn arcs(&self) -> usize {
        self.merc.len()
    }

    /// CPU bytes the index holds.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.keys.len() * 8 + (self.offsets.len() + self.entries.len() + self.heights.len() + self.long.len()) * 4 + self.merc.len() * 16
    }

    /// `2^level` cells per world side.
    #[must_use]
    pub fn level(&self) -> u8 {
        self.level
    }

    /// Cell `key`'s arcs and their heights in it, tallest first.
    /// Arc `i`'s chord ends as the prefilter reads them.
    fn chord_ends(&self, i: usize) -> (DVec2, DVec2) {
        let m = self.merc[i];
        (DVec2::new(f64::from(m[0]), f64::from(m[1])), DVec2::new(f64::from(m[2]), f64::from(m[3])))
    }

    fn cell(&self, key: u64) -> (&[u32], &[f32]) {
        match self.keys.binary_search(&key) {
            Ok(k) => {
                let r = self.offsets[k] as usize..self.offsets[k + 1] as usize;
                (&self.entries[r.clone()], &self.heights[r])
            }
            Err(_) => (&[], &[]),
        }
    }

    /// Every cell the box of Mercator half-size `rad` around `m` touches,
    /// with the lowest height (`z`) the ray reaches near it, folded into
    /// `cells`.
    fn note(&self, cells: &mut HashMap<u64, f64>, m: DVec2, rad: f64, z: f64) {
        let side = 1i64 << self.level;
        let n = side as f64;
        let (x0, x1) = (((m.x - rad) * n).floor() as i64, ((m.x + rad) * n).floor() as i64);
        let (y0, y1) = (((m.y - rad) * n).floor() as i64, ((m.y + rad) * n).floor() as i64);
        for y in y0.max(0)..=y1.min(side - 1) {
            for x in x0..=x1.min(x0 + side - 1) {
                let e = cells.entry(key(x.rem_euclid(side) as u32, y as u32)).or_insert(f64::INFINITY);
                *e = e.min(z);
            }
        }
    }

    /// The arcs that may be under the cursor at logical `(x, y)`: those
    /// whose cell the cursor's ray passes over and that stand tall enough
    /// to reach it there, plus the long ones, each once and only if `keep`
    /// says so. `slack_px`: the widest an arc's hit area reaches from its
    /// centreline.
    #[must_use]
    pub fn candidates(&self, frame: &ArcFrame, x: f64, y: f64, height: f64, slack_px: f64) -> Vec<u32> {
        let world_per_px = FlatFootprint::new(frame, x, y, 0.5 * height * self.max_chord * f64::from(super::WORLD_SIZE)).map(|f| f.world_per_px);
        self.candidates_where(frame, (x, y), height, slack_px, world_per_px, |_| true)
    }

    fn candidates_where(
        &self,
        frame: &ArcFrame,
        (x, y): (f64, f64),
        height: f64,
        slack_px: f64,
        world_per_px: Option<f64>,
        mut keep: impl FnMut(u32) -> bool,
    ) -> Vec<u32> {
        let mut cells: HashMap<u64, f64> = HashMap::new();
        // Two scales: `world` for the flat frame's peaks (world units), and
        // radians of the unit sphere for the globe's.
        let world = f64::from(super::WORLD_SIZE);
        let guess = PITCH_SLACK / frame.zoom.exp2();
        let cell = 1.0 / f64::from(1u32 << self.level);
        let mut flat_peak_scale = None;
        let mut globe_peak_scale = None;
        if frame.transition < 1.0 {
            let peak_scale = height * world;
            let slack = slack_px * world_per_px.map_or(guess, |w| w * 1.34);
            self.flat_cells(frame, x, y, 0.5 * self.max_chord * peak_scale, slack, &mut cells);
            flat_peak_scale = Some(peak_scale);
        }
        if frame.transition > 0.0 {
            // A great circle is never longer than the rhumb line, whose
            // ground length is at most its Mercator length (x 2 pi radians).
            let peak_scale = height * TAU;
            let slack_rad = slack_px * guess / world * TAU;
            self.globe_cells(frame, x, y, 0.5 * self.max_chord * peak_scale, slack_rad, cell, &mut cells);
            globe_peak_scale = Some(peak_scale);
        }
        let mut out: Vec<u32> = self.long.iter().copied().filter(|&i| keep(i)).collect();
        // The flat height in world units, the globe's in radii; the cell's
        // lowest ray height is in whichever frame noted it, so an arc is
        // kept if it reaches it in either.
        let scale = flat_peak_scale.unwrap_or(0.0f64).max(globe_peak_scale.unwrap_or(0.0));
        for (k, zmin) in cells {
            let (arcs, heights) = self.cell(k);
            for (&i, &h) in arcs.iter().zip(heights) {
                if f64::from(h) * scale < zmin {
                    break;
                }
                if keep(i) {
                    out.push(i);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The flat frame's cells under the ray, heights in world units.
    fn flat_cells(&self, frame: &ArcFrame, x: f64, y: f64, top: f64, slack: f64, cells: &mut HashMap<u64, f64>) {
        let Some((p0, dir)) = frame.flat_ray(x, y) else { return };
        let world = f64::from(super::WORLD_SIZE);
        let (lo_z, hi_z) = (-slack, top + slack);
        let (t0, t1) = if dir.z.abs() < 1e-12 {
            if !(lo_z..=hi_z).contains(&p0.z) {
                return;
            }
            (0.0, 1.0)
        } else {
            let (a, b) = ((lo_z - p0.z) / dir.z, (hi_z - p0.z) / dir.z);
            (a.min(b).max(0.0), a.max(b).min(1.0))
        };
        if !(t1 >= t0) {
            return;
        }
        let cell_world = world / f64::from(1u32 << self.level);
        let horizontal = dir.truncate().length() * (t1 - t0);
        let samples = ((horizontal / (cell_world * 0.5)).ceil() as usize).clamp(1, MAX_RAY_SAMPLES);
        let step = horizontal / samples as f64;
        // Each sample stands for the ray within half a step of it, widened
        // by the slack: a box that wide around it covers every cell the
        // hit area can reach.
        let rad_world = slack + step * 0.5;
        // How much the ray's height changes across that box: a cell's arcs
        // must reach the LOWEST height the ray has over it.
        let dz_per_xy = if dir.truncate().length() > 0.0 { dir.z.abs() / dir.truncate().length() } else { f64::INFINITY };
        let drop = rad_world * 2.0 * dz_per_xy;
        let origin = frame.origin_world / world;
        for k in 0..=samples {
            let t = t0 + (t1 - t0) * k as f64 / samples as f64;
            let p: DVec3 = p0 + dir * t;
            let m = origin + p.truncate() / world;
            let zmin = if drop.is_finite() { (p.z - drop - slack).max(0.0) } else { 0.0 };
            self.note(cells, m, rad_world / world, zmin);
        }
    }

    /// The globe's cells under the ray, heights in Earth radii above the
    /// sphere: the ray from where it enters the shell the tallest arc
    /// reaches to where it meets the planet (behind which nothing draws).
    #[allow(clippy::too_many_arguments)]
    fn globe_cells(&self, frame: &ArcFrame, x: f64, y: f64, top: f64, slack: f64, cell: f64, cells: &mut HashMap<u64, f64>) {
        let Some((p0, dir)) = frame.globe_ray(x, y) else { return };
        let roots = |r: f64| -> Option<(f64, f64)> {
            let (a, b, c) = (dir.length_squared(), 2.0 * p0.dot(dir), p0.length_squared() - r * r);
            let disc = b * b - 4.0 * a * c;
            (disc >= 0.0 && a > 0.0).then(|| ((-b - disc.sqrt()) / (2.0 * a), (-b + disc.sqrt()) / (2.0 * a)))
        };
        let Some((t_in, t_out)) = roots(1.0 + top + slack) else { return };
        let t0 = t_in.max(0.0);
        let t1 = match roots(1.0) {
            Some((hit, _)) if hit >= 0.0 => hit.min(t_out),
            _ => t_out,
        };
        if !(t1 >= t0) {
            return;
        }
        // A cell spans `cell * 2 pi * cos(lat)` radians; sample at half of
        // the narrowest (70 degrees north or south) a pick is asked about.
        let cos = 70f64.to_radians().cos();
        let cell_rad = cell * TAU * cos;
        let span = dir.length() * (t1 - t0);
        let samples = ((span / (cell_rad * 0.5)).ceil() as usize).clamp(1, MAX_RAY_SAMPLES);
        let rad = (slack + span / samples as f64) / (TAU * cos);
        let mut previous: Option<f64> = None;
        for k in 0..=samples {
            let t = t0 + (t1 - t0) * k as f64 / samples as f64;
            let p = p0 + dir * t;
            let len = p.length();
            if !(len > 0.0) {
                continue;
            }
            let u = p / len;
            let lon = u.x.atan2(u.z).to_degrees();
            let lat = u.y.clamp(-1.0, 1.0).asin().to_degrees();
            let m = mercator(lon, lat.clamp(-85.05, 85.05));
            let altitude = len - 1.0;
            let step = previous.map_or(0.0, |q: f64| (altitude - q).abs());
            previous = Some(altitude);
            let zmin = (altitude - step * 4.0 - slack).max(0.0);
            self.note(cells, m, rad, zmin);
        }
    }
}

/// The flat frame's cursor ray between the ground and the tallest arc's
/// peak, in world units relative to the camera's centre: the exact
/// world-space prefilter [`pick`] runs before projecting any polyline.
///
/// A flat arc stands in the vertical plane over its chord, so it can meet
/// the ray only where the chord crosses the ray's footprint on the
/// ground, and only if the arc's height there is the ray's. Both are a
/// few multiplications, where the projected test is a matrix product per
/// polyline point: at Kepler's UK camera a London cell holds tens of
/// thousands of arcs, nearly all of which cross the footprint nowhere.
struct FlatFootprint {
    /// The ray at its lowest (`a`) and highest (`b`) height of interest.
    a: DVec3,
    b: DVec3,
    origin: DVec2,
    /// World units a logical pixel spans along the footprint, at its
    /// farthest from the eye: the slack the screen tolerance becomes.
    world_per_px: f64,
}

impl FlatFootprint {
    fn new(frame: &ArcFrame, x: f64, y: f64, top: f64) -> Option<Self> {
        let (p0, dir) = frame.flat_ray(x, y)?;
        if dir.z.abs() < 1e-12 {
            return None;
        }
        let (ta, tb) = ((0.0 - p0.z) / dir.z, (top - p0.z) / dir.z);
        let (t0, t1) = (ta.min(tb).max(0.0), ta.max(tb).min(1.0));
        if !(t1 >= t0) {
            return None;
        }
        let (a, b) = (p0 + dir * t0, p0 + dir * t1);
        let (a, b) = if a.z <= b.z { (a, b) } else { (b, a) };
        // Pixels per world unit at each end, from a unit step projected.
        let scale = |p: DVec3| -> Option<f64> {
            let e = 1e-3;
            let (s0, s1, s2) = (frame.project_flat_rel(p)?, frame.project_flat_rel(p + DVec3::X * e)?, frame.project_flat_rel(p + DVec3::Y * e)?);
            let d = ((s1.0 - s0.0).hypot(s1.1 - s0.1)).min((s2.0 - s0.0).hypot(s2.1 - s0.1));
            (d > 0.0).then_some(d / e)
        };
        let px_per_world = scale(a)?.min(scale(b)?);
        Some(Self { a, b, origin: frame.origin_world / f64::from(super::WORLD_SIZE), world_per_px: 1.0 / px_per_world })
    }

    /// Whether the flat arc over Mercator chord `s`-`t` (unwrapped, the
    /// shortest way) may pass within `slack` world units of the ray.
    fn may_hit(&self, s: DVec2, t: DVec2, height: f64, slack: f64) -> bool {
        let world = f64::from(super::WORLD_SIZE);
        // The chord in the footprint's space, on the world copy nearest it.
        let shift = ((s.x + t.x) * 0.5 - (self.origin.x + self.a.x / world)).round();
        let shift = DVec2::new(shift, 0.0);
        let (cs, ct) = ((s - shift - self.origin) * world, (t - shift - self.origin) * world);
        let chord_world = (ct - cs).length();
        let (fa, fb) = (self.a.truncate(), self.b.truncate());
        let (dc, df) = (ct - cs, fb - fa);
        let cross = dc.perp_dot(df);
        let distance = segments_distance(cs, ct, fa, fb);
        if distance > slack {
            return false;
        }
        let (lc, lf) = (dc.length(), df.length());
        if lc < 1e-12 || lf < 1e-12 || cross.abs() < 1e-9 * lc * lf {
            // Parallel, or a ray straight down: no single crossing to read
            // heights at. The projected test decides.
            return true;
        }
        // Where the two lines cross, and how far either parameter can move
        // while staying within `slack` of the other line.
        let w = fa - cs;
        let r = w.perp_dot(df) / cross;
        let u = w.perp_dot(dc) / cross;
        let sin = cross.abs() / (lc * lf);
        let (dr, du) = (slack / (sin * lc), slack / (sin * lf));
        let (r0, r1) = ((r - dr).max(0.0), (r + dr).min(1.0));
        let (u0, u1) = ((u - du).max(0.0), (u + du).min(1.0));
        if r0 > r1 || u0 > u1 {
            return false;
        }
        // The arc's height over [r0, r1]: `lift` rises to r = 0.5, then falls.
        let z = |r: f64| super::arcs::lift(r, height) * chord_world;
        let arc_lo = z(r0).min(z(r1));
        let arc_hi = if (r0..=r1).contains(&0.5) { z(0.5) } else { z(r0).max(z(r1)) };
        let ray = |u: f64| self.a.z + (self.b.z - self.a.z) * u;
        let (ray_lo, ray_hi) = (ray(u0), ray(u1));
        arc_hi + slack >= ray_lo && arc_lo - slack <= ray_hi
    }
}

/// The least distance between segments `a0`-`a1` and `b0`-`b1`.
fn segments_distance(a0: DVec2, a1: DVec2, b0: DVec2, b1: DVec2) -> f64 {
    let (da, db) = (a1 - a0, b1 - b0);
    let cross = da.perp_dot(db);
    if cross.abs() > 0.0 {
        let w = b0 - a0;
        let (r, u) = (w.perp_dot(db) / cross, w.perp_dot(da) / cross);
        if (0.0..=1.0).contains(&r) && (0.0..=1.0).contains(&u) {
            return 0.0;
        }
    }
    let point = |p: DVec2, q0: DVec2, q1: DVec2| segment_distance((p.x, p.y), (q0.x, q0.y), (q1.x, q1.y));
    point(a0, b0, b1).min(point(a1, b0, b1)).min(point(b0, a0, a1)).min(point(b1, a0, a1))
}

/// The arc of `set` under logical screen point `(x, y)` on `frame`: the
/// topmost (latest drawn) arc whose centreline passes within
/// [`UNDER_CURSOR_PX`] of the cursor, else the nearest drawn centreline
/// within its half width (at least half a pixel) plus the shader's
/// half-pixel feather plus `tolerance_px`, a tie going to the arc drawn
/// later (on top). `filter` is the relative window
/// the layer draws with ([`ArcSet::relative_window`]); an arc it hides is
/// never hit.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn pick(
    frame: &ArcFrame,
    set: &ArcSet,
    index: &ArcPickIndex,
    style: &ArcStyle,
    filter: Option<[f32; 2]>,
    x: f64,
    y: f64,
    tolerance_px: f64,
) -> Option<ArcHit> {
    pick_inner(frame, set, index, style, filter, (x, y), tolerance_px, true)
}

#[allow(clippy::too_many_arguments)]
fn pick_inner(
    frame: &ArcFrame,
    set: &ArcSet,
    index: &ArcPickIndex,
    style: &ArcStyle,
    filter: Option<[f32; 2]>,
    (x, y): (f64, f64),
    tolerance_px: f64,
    prefilter: bool,
) -> Option<ArcHit> {
    if set.is_empty() {
        return None;
    }
    let height = f64::from(style.height);
    let slack = f64::from(index.max_width_px.max(1.0)) * 0.5 + 0.5 + tolerance_px;
    let mut best: Option<ArcHit> = None;
    let mut clips: Vec<glam::DVec4> = Vec::with_capacity(64);
    let top = 0.5 * height * index.max_chord * f64::from(super::WORLD_SIZE);
    let footprint = if prefilter && frame.transition <= 0.0 { FlatFootprint::new(frame, x, y, top) } else { None };
    // A third more than the screen slack: the footprint's pixel scale is a
    // finite difference, and the exact test below is the arbiter.
    // Plus the `f32` chord ends' rounding (a few 1e-8 of the world).
    let world_slack = footprint.as_ref().map_or(0.0, |f| slack * f.world_per_px * 1.34 + 1e-5);
    let keep = |i: u32| {
        let i = i as usize;
        set.passes_filter(i, filter)
            && footprint.as_ref().is_none_or(|f| {
                let (s, t) = index.chord_ends(i);
                f.may_hit(s, t, height, world_slack)
            })
    };
    // Topmost first (a later instance draws over an earlier one), and done
    // at the first centreline under the cursor's own pixel: over London a
    // cursor has thousands of arcs within its tolerance, and the one drawn
    // on top there is the answer.
    for i in index.candidates_where(frame, (x, y), height, slack, footprint.as_ref().map(|f| f.world_per_px), keep).into_iter().rev() {
        let i = i as usize;
        let Some(class) = set.class_of(i) else { continue };
        let a = &set.instances[i];
        let reach = f64::from(a.width_px.max(1.0)) * 0.5 + 0.5 + tolerance_px;
        let n = segments_for(class, frame.zoom, frame.pitch, style.max_segments);
        let (s, t) = ends(set, i);
        clips.clear();
        clips.extend((0..=n).map(|j| frame.clip(s, t, segment_ratio(j, n), height)));
        let mut nearest = f64::INFINITY;
        for j in 0..n as usize {
            // Clipped to the near and far planes BEFORE the divide, as the
            // GPU clips the strip: a point past the eye projects anywhere.
            let Some((ca, cb)) = clip_segment(clips[j], clips[j + 1]) else { continue };
            let (Some(p), Some(q)) = (frame.screen_of_clip(ca), frame.screen_of_clip(cb)) else { continue };
            if frame.transition < 1.0 {
                let (r0, r1) = (segment_ratio(j as u32, n), segment_ratio(j as u32 + 1, n));
                if (frame.wrapped_flat_x(s, t, r0) - frame.wrapped_flat_x(s, t, r1)).abs() > 0.5 {
                    continue;
                }
            }
            nearest = nearest.min(segment_distance((x, y), p, q));
        }
        if nearest <= reach && best.is_none_or(|b| nearest < b.distance_px) {
            best = Some(ArcHit { index: i, distance_px: nearest });
            if nearest <= UNDER_CURSOR_PX {
                break;
            }
        }
    }
    best
}

/// Every arc of `set` the slow way: the exact projected test against all
/// of them, no index and no prefilter. The reference [`pick`] is tested
/// against.
#[must_use]
pub fn pick_scan(frame: &ArcFrame, set: &ArcSet, style: &ArcStyle, filter: Option<[f32; 2]>, x: f64, y: f64, tolerance_px: f64) -> Option<ArcHit> {
    let everything = ArcPickIndex { long: (0..set.len() as u32).collect(), ..ArcPickIndex::new(&ArcSet::default()) };
    let everything = ArcPickIndex { max_width_px: set.instances.iter().map(|a| a.width_px).fold(0.0, f32::max), ..everything };
    pick_inner(frame, set, &everything, style, filter, (x, y), tolerance_px, false)
}

/// From `p` to the segment `a`-`b`, in the same units.
fn segment_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (cx, cy) = (a.0 + dx * t, a.1 + dy * t);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::arcs::{pack_color, ArcFilterInfo, ArcInstance};
    use crate::render::Camera;
    use crate::style::Rgba;

    fn arc(s: (f64, f64), t: (f64, f64), width: f32) -> ArcInstance {
        let c = pack_color(Rgba { r: 1.0, g: 0.5, b: 0.0, a: 1.0 });
        ArcInstance { source: [s.0 as f32, s.1 as f32], target: [t.0 as f32, t.1 as f32], source_color: c, target_color: c, width_px: width, filter: 0.0 }
    }

    fn camera(lon: f64, lat: f64, zoom: f64, pitch_deg: f32) -> Camera {
        Camera { center_lon: lon, center_lat: lat, zoom, pitch: pitch_deg.to_radians(), bearing: 0.0, viewport_width_px: 800.0, viewport_height_px: 600.0, fov_y: Camera::DEFAULT_FOV_Y }
    }

    fn flat(cam: &Camera) -> ArcFrame {
        ArcFrame::new(cam, &crate::render::globe::globe_frame(cam).flattened(), 2.0)
    }

    fn pick_at(frame: &ArcFrame, set: &ArcSet, filter: Option<[f32; 2]>, p: (f64, f64)) -> Option<ArcHit> {
        let index = ArcPickIndex::new(set);
        pick(frame, set, &index, &ArcStyle::default(), filter, p.0, p.1, 3.0)
    }

    /// On the curve hits, a few pixels off it still hits (the tolerance),
    /// and well off it -- including the chord on the ground under the
    /// raised arc -- does not. Flat and pitched.
    #[test]
    fn an_arc_is_hit_on_and_near_its_curve_and_not_off_it_flat() {
        let (s, t) = ((-0.30, 51.45), (0.10, 51.55));
        let set = ArcSet::new(vec![arc(s, t, 2.0)]);
        for pitch in [0.0f32, 55.0] {
            let frame = flat(&camera(-0.10, 51.50, 9.0, pitch));
            for r in [0.2, 0.5, 0.8] {
                let on = frame.project(s, t, r, 1.0).expect("on screen");
                let hit = pick_at(&frame, &set, None, on).unwrap_or_else(|| panic!("on the curve at r={r}, pitch {pitch}"));
                assert_eq!(hit.index, 0);
                assert!(hit.distance_px < 0.75, "{hit:?}");
                // Four pixels to the side: inside 1 + 0.5 + 3.
                let near = frame.project(s, t, r + 0.002, 1.0).unwrap();
                let (dx, dy) = (near.0 - on.0, near.1 - on.1);
                let len = (dx * dx + dy * dy).sqrt();
                let side = (on.0 - dy / len * 4.0, on.1 + dx / len * 4.0);
                assert!(pick_at(&frame, &set, None, side).is_some(), "4 px off the curve is within the tolerance (r={r}, pitch {pitch})");
                let far = (on.0 - dy / len * 12.0, on.1 + dx / len * 12.0);
                assert!(pick_at(&frame, &set, None, far).is_none(), "12 px off is not (r={r}, pitch {pitch})");
            }
            // Under pitch the raised arc leaves its chord (seen from
            // straight above, it covers it).
            let ground = frame.project(s, t, 0.5, 0.0).unwrap();
            if pitch > 0.0 {
                assert!(pick_at(&frame, &set, None, ground).is_none(), "the chord under the arc is empty (pitch {pitch})");
            }
        }
    }

    /// On a full globe frame the arc is hit where it stands above the
    /// planet, and a far-side arc the planet hides is not hit at all.
    #[test]
    fn an_arc_is_hit_on_the_globe_and_not_behind_it() {
        // North-south at 40 degrees east of the view's centre: lifted
        // radially, the arc stands off its great circle on screen.
        let (s, t) = ((40.0, -20.0), (40.0, 20.0));
        let cam = camera(0.0, 0.0, 1.0, 0.0);
        let globe = crate::render::globe::globe_frame(&cam);
        assert!(globe.transition >= 1.0);
        let frame = ArcFrame::new(&cam, &globe, 2.0);
        let set = ArcSet::new(vec![arc(s, t, 2.0), arc((160.0, 0.0), (-160.0, 0.0), 2.0)]);
        let front = set.instances.iter().position(|a| a.source[0] == 40.0).unwrap();
        for r in [0.15, 0.5, 0.85] {
            let on = frame.project(s, t, r, 1.0).expect("visible");
            assert_eq!(pick_at(&frame, &set, None, on).map(|h| h.index), Some(front), "on the globe arc at r={r}");
        }
        let apex = frame.project(s, t, 0.5, 1.0).unwrap();
        assert!(pick_at(&frame, &set, None, (apex.0 + 15.0, apex.1)).is_none(), "15 px beside the apex is off it");
        let ground = frame.project(s, t, 0.5, 0.0).unwrap();
        assert!((apex.0 - ground.0).abs() > 20.0, "the apex stands off the ground on screen: {apex:?} {ground:?}");
        assert!(pick_at(&frame, &set, None, ground).is_none(), "the great circle on the ground under the arc is empty");
        // The far arc projects nowhere (hidden): nothing on screen picks it.
        for k in 0..20 {
            let p = (400.0 + f64::from(k) * 3.0, 300.0 + f64::from(k));
            assert_ne!(pick_at(&frame, &set, None, p).map(|h| h.index), Some(1 - front), "the far-side arc is behind the planet");
        }
    }

    /// An arc the filter window hides is not hit, one inside it is.
    #[test]
    fn a_filtered_out_arc_does_not_pick() {
        let (s, t) = ((-0.30, 51.45), (0.10, 51.55));
        let mut one = arc(s, t, 2.0);
        one.filter = 5.0;
        let mut set = ArcSet::new(vec![one]);
        set.filter = Some(ArcFilterInfo { base: 0.0, domain: [0.0, 10.0] });
        let frame = flat(&camera(-0.10, 51.50, 9.0, 30.0));
        let on = frame.project(s, t, 0.5, 1.0).unwrap();
        assert!(pick_at(&frame, &set, set.relative_window([4.0, 6.0]), on).is_some(), "inside the window");
        assert!(pick_at(&frame, &set, set.relative_window([6.0, 9.0]), on).is_none(), "outside the window");
        assert!(pick_at(&frame, &set, None, on).is_some(), "no window");
    }

    /// Of two crossing arcs the nearer centreline wins; an exact tie goes
    /// to the one drawn later; indices are the set's (sorted) ones and
    /// `rows` maps them back to the input.
    #[test]
    fn the_nearest_arc_wins_and_rows_map_back_to_the_input() {
        let a = arc((-0.30, 51.45), (0.10, 51.55), 2.0);
        let long = arc((-2.0, 51.0), (1.0, 52.0), 2.0);
        let set = ArcSet::new(vec![a, long]);
        assert_eq!(set.instances[0].source, long.source, "the long arc sorts first");
        assert_eq!(set.row(0), Some(1));
        assert_eq!(set.row(1), Some(0));
        let frame = flat(&camera(-0.10, 51.50, 8.0, 0.0));
        let p = frame.project((-0.30, 51.45), (0.10, 51.55), 0.5, 1.0).unwrap();
        assert_eq!(pick_at(&frame, &set, None, p).map(|h| h.index), Some(1));
        let twins = ArcSet::new(vec![a, a]);
        assert_eq!(pick_at(&frame, &twins, None, p).map(|h| h.index), Some(1), "a tie goes to the arc on top");
    }

    /// `n` random arcs around `(lon, lat)`, `spread` degrees across, a
    /// tenth of them `long` degrees long and the rest a tenth of that.
    fn random_arcs(n: usize, (lon, lat): (f64, f64), spread: f64, long: f64, seed: u64) -> ArcSet {
        let mut seed = seed;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        ArcSet::new(
            (0..n)
                .map(|_| {
                    let s = (lon + (rnd() - 0.5) * spread, lat + (rnd() - 0.5) * spread * 0.6);
                    let d = if rnd() < 0.1 { long } else { long * 0.1 };
                    arc(s, (s.0 + (rnd() - 0.5) * d, s.1 + (rnd() - 0.5) * d), 1.0)
                })
                .collect(),
        )
    }

    /// The index and the full scan agree at every cursor of a grid over
    /// `(x0, y0)`-`(x1, y1)`, and at least `min_hits` of them hit.
    fn agree(frame: &ArcFrame, set: &ArcSet, (x0, y0, x1, y1): (f64, f64, f64, f64), steps: u32, min_hits: usize) {
        let index = ArcPickIndex::new(set);
        let style = ArcStyle::default();
        let mut hits = 0;
        for gy in 0..steps {
            for gx in 0..steps {
                let x = x0 + (x1 - x0) * (f64::from(gx) + 0.5) / f64::from(steps);
                let y = y0 + (y1 - y0) * (f64::from(gy) + 0.5) / f64::from(steps);
                let fast = pick(frame, set, &index, &style, None, x, y, 3.0);
                let slow = pick_scan(frame, set, &style, None, x, y, 3.0);
                assert_eq!(fast.map(|h| h.index), slow.map(|h| h.index), "at ({x}, {y}), transition {}", frame.transition);
                hits += usize::from(slow.is_some());
            }
        }
        assert!(hits >= min_hits, "the grid hits arcs: {hits} (transition {})", frame.transition);
    }

    /// On a full globe frame, the index agrees with the scan.
    #[test]
    fn the_index_agrees_with_the_scan_on_the_globe() {
        let cam = camera(-1.0, 52.2, 3.5, 0.0);
        let globe = crate::render::globe::globe_frame(&cam);
        assert!(globe.transition >= 1.0, "z3.5 is a full globe");
        let frame = ArcFrame::new(&cam, &globe, 2.0);
        let set = random_arcs(800, (-1.0, 52.2), 8.0, 4.0, 0xabcd_ef01);
        let c = frame.project((-1.0, 52.2), (-0.9, 52.2), 0.0, 1.0).expect("the UK faces us");
        agree(&frame, &set, (c.0 - 90.0, c.1 - 70.0, c.0 + 90.0, c.1 + 70.0), 18, 10);
    }

    /// Inside the flat-to-globe blend (z11-z12), the index agrees with the
    /// scan: the candidates come from both projections' rays.
    #[test]
    fn the_index_agrees_with_the_scan_in_the_blend() {
        use crate::render::globe::{PROJECTION_BLEND_ZOOM_MAX, PROJECTION_BLEND_ZOOM_MIN};
        let cam = camera(-0.1, 51.5, 0.5 * (PROJECTION_BLEND_ZOOM_MIN + PROJECTION_BLEND_ZOOM_MAX), 30.0);
        let globe = crate::render::globe::globe_frame(&cam);
        assert!(globe.transition > 0.0 && globe.transition < 1.0, "the blend: {}", globe.transition);
        let frame = ArcFrame::new(&cam, &globe, 2.0);
        let set = random_arcs(800, (-0.1, 51.5), 0.3, 0.2, 0x1357_9bdf);
        agree(&frame, &set, (40.0, 40.0, 760.0, 560.0), 18, 10);
    }

    /// The index answers what the full scan answers, over a dense random
    /// set at a pitched camera, everywhere on a grid of cursors.
    #[test]
    fn the_index_agrees_with_the_full_scan() {
        let mut seed = 0x1234_5678u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let arcs: Vec<ArcInstance> = (0..3000)
            .map(|_| {
                let s = (-3.0 + rnd() * 4.0, 51.0 + rnd() * 2.5);
                let long = rnd() < 0.1;
                let d = if long { 1.5 } else { 0.15 };
                arc(s, (s.0 + (rnd() - 0.5) * d, s.1 + (rnd() - 0.5) * d), 1.0)
            })
            .collect();
        let set = ArcSet::new(arcs);
        let index = ArcPickIndex::new(&set);
        let frame = flat(&camera(-1.0, 52.2, 7.5, 50.0));
        let style = ArcStyle::default();
        let (mut hits, mut checked) = (0, 0);
        for gy in 0..15 {
            for gx in 0..20 {
                let (x, y) = (10.0 + f64::from(gx) * 39.0, 10.0 + f64::from(gy) * 39.0);
                let fast = pick(&frame, &set, &index, &style, None, x, y, 3.0);
                let slow = pick_scan(&frame, &set, &style, None, x, y, 3.0);
                assert_eq!(fast.map(|h| h.index), slow.map(|h| h.index), "at ({x}, {y})");
                hits += usize::from(slow.is_some());
                checked += 1;
            }
        }
        assert!(hits > checked / 10, "a dense set is hit often: {hits}/{checked}");
    }
}
