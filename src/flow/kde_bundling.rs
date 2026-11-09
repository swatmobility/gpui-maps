//! Density-based edge bundling, after Hurter, Ersoy and Telea, "Graph
//! Bundling by Kernel Density Estimation", Computer Graphics Forum 31(3)
//! (EuroVis 2012), doi:10.1111/j.1467-8659.2012.03079.x -- KDEEB.
//!
//! Written from the paper's description; no implementation was read or
//! ported.
//!
//! # Why a second bundler
//!
//! [`super::edge_path_bundling`] routes a corridor through the OTHER
//! corridors: it needs long edges with chains of shorter ones beneath them.
//! Commuting is dense and local, so on a national commute table it draws
//! the neighbour graph between places (straight hops) plus loops where a
//! long corridor threads a chain of them. KDEEB has no graph at all: every
//! corridor is a polyline that drifts uphill on the density of all the
//! others, so strands that run roughly the same way anywhere along their
//! length gather into one smooth trunk. That is the fluid, organic picture
//! of a dense flow table.
//!
//! # The algorithm (the paper's section 3)
//!
//! 1. Resample every corridor into points at a spacing.
//! 2. For `iterations` rounds, the kernel shrinking each round:
//!    - splat every point, weighted by its corridor's flow, into a density
//!      grid and blur it with a kernel of radius `h` (here three box blurs,
//!      a Gaussian of `sigma = h / 2` in effect);
//!    - move every interior point up the density gradient. The step is the
//!      mean-shift step `sigma^2 * grad(rho) / rho` -- the move to the
//!      kernel-weighted mean of the points around it -- so a point climbs
//!      onto a ridge and stops there rather than overshooting it;
//!    - Laplacian-smooth each polyline;
//!    - resample it evenly along its new length.
//! 3. Relax every point a little back toward its straight line, so the
//!    bundles do not over-collapse (the paper's relaxation, section 4.3).
//!
//! The ends never move. Near them both the pull and the point's own weight
//! taper to nothing ([`DensityBundlingParams::end_taper`]), so the corridors
//! leaving one place fan out from it before they join a trunk rather than
//! every place becoming one attractor.
//!
//! Everything runs in a local equirectangular metric (metres, longitude
//! scaled by the cosine of the data's middle latitude), as the rest of
//! [`super`] measures.
//!
//! # Determinism and threads
//!
//! The splat and the load totals run serially in corridor order; the blur,
//! the advection, the smoothing and the resampling are per row or per
//! corridor, independent of each other, and run on scoped threads (never
//! on wasm). Same input, same output, whatever the thread count.

use super::edge_path_bundling::{BundledPath, PathEdge};

/// Tuning. The lengths are shares of the corridors' median length, so one
/// set of numbers suits a city and a country alike -- and a table whose
/// corridors are short against its extent (a country's commute: 5-30 km
/// hops over 600 km) still gets kernels and points on the scale of its
/// corridors. (Shares of the extent were tried first: the commute's hops
/// got three points each and a kernel wider than they are, and stayed
/// straight -- renders, 2026-10-09.)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DensityBundlingParams {
    /// Rounds of splat, advect, smooth and resample.
    pub iterations: usize,
    /// The most density grid cells along the extent's longer side (the
    /// grid is sized so the last round's kernel spans about two cells).
    pub grid: usize,
    /// The first round's kernel radius `h`, a share of the median corridor.
    pub kernel_start: f64,
    /// `h` is multiplied by this every round (the paper's `lambda`).
    pub kernel_decay: f64,
    /// How far of the mean-shift step a point takes per round (1: all of
    /// it).
    pub step: f64,
    /// Laplacian smoothing per round: the share of the way each interior
    /// point moves toward its neighbours' midpoint, and how many passes.
    pub smoothing: f64,
    pub smoothing_passes: usize,
    /// The spacing between a corridor's points while bundling, a share of
    /// the median corridor, and the most points one corridor gets (at
    /// least five).
    pub spacing: f64,
    pub max_points: usize,
    /// The most points one drawn corridor gets.
    pub out_points: usize,
    /// The final pull of every point back toward its straight line (0:
    /// none, 1: unbundled).
    pub relax: f64,
    /// A corridor's flow enters the density as `weight ^ weight_power`: 1
    /// is the paper's plain weighting, lower keeps the busiest trunks from
    /// swallowing everything near them.
    pub weight_power: f64,
    /// Over this share of a corridor's length at each end the pull and the
    /// splat ramp up from nothing.
    pub end_taper: f64,
}

impl Default for DensityBundlingParams {
    fn default() -> Self {
        Self {
            iterations: 12,
            grid: 1024,
            kernel_start: 0.4,
            kernel_decay: 0.82,
            step: 1.0,
            smoothing: 0.5,
            smoothing_passes: 1,
            spacing: 1.0 / 12.0,
            max_points: 40,
            out_points: 32,
            relax: 0.25,
            weight_power: 0.5,
            end_taper: 0.12,
        }
    }
}

impl DensityBundlingParams {
    /// A key over every parameter, for a cache.
    pub fn key(&self) -> u64 {
        let mut h: u64 = 0x6a09_e667_f3bc_c908;
        let mut mix = |v: u64| h = (h.rotate_left(7) ^ v).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        mix(self.iterations as u64);
        mix(self.grid as u64);
        mix(self.kernel_start.to_bits());
        mix(self.kernel_decay.to_bits());
        mix(self.step.to_bits());
        mix(self.smoothing.to_bits());
        mix(self.smoothing_passes as u64);
        mix(self.spacing.to_bits());
        mix(self.max_points as u64);
        mix(self.out_points as u64);
        mix(self.relax.to_bits());
        mix(self.weight_power.to_bits());
        mix(self.end_taper.to_bits());
        h
    }
}

/// Metres per degree of latitude.
const M_PER_DEG: f64 = 111_320.0;

/// Bundles `edges` by kernel density: one [`BundledPath`] per input edge,
/// in the same order, its ends exactly the edge's.
///
/// `weights` is positionally aligned with `edges` (missing: 1). A point's
/// `load` is the total weight of the corridors passing through the same
/// final-kernel-sized cell of the map -- the flow sharing that stretch of
/// trunk -- and `corridors` how many they are. `routed` says the corridor
/// visibly left its straight line.
pub fn bundle_density(edges: &[PathEdge], weights: &[f64], p: DensityBundlingParams) -> Vec<BundledPath> {
    if edges.is_empty() {
        return Vec::new();
    }
    let weight_of = |i: usize| weights.get(i).copied().filter(|w| w.is_finite() && *w > 0.0).unwrap_or(if weights.len() > i { 0.0 } else { 1.0 });

    // The local metric.
    let (mut lat_lo, mut lat_hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for e in edges {
        for q in [e.from, e.to] {
            if q[1].is_finite() {
                lat_lo = lat_lo.min(q[1]);
                lat_hi = lat_hi.max(q[1]);
            }
        }
    }
    let lat0 = if lat_lo <= lat_hi { (lat_lo + lat_hi) / 2.0 } else { 0.0 };
    let cos = lat0.to_radians().cos().abs().max(0.01);
    let fwd = |q: [f64; 2]| [q[0] * cos * M_PER_DEG, q[1] * M_PER_DEG];
    let back = |q: [f64; 2]| [q[0] / (cos * M_PER_DEG), q[1] / M_PER_DEG];

    let mut lo = [f64::INFINITY; 2];
    let mut hi = [f64::NEG_INFINITY; 2];
    for e in edges {
        for q in [fwd(e.from), fwd(e.to)] {
            if q[0].is_finite() && q[1].is_finite() {
                for k in 0..2 {
                    lo[k] = lo[k].min(q[k]);
                    hi[k] = hi[k].max(q[k]);
                }
            }
        }
    }
    if !(lo[0] <= hi[0]) {
        return edges.iter().enumerate().map(|(i, e)| straight(e, weight_of(i))).collect();
    }
    let extent = (hi[0] - lo[0]).max(hi[1] - lo[1]).max(1.0);
    // The corridors' median length: the scale every length is a share of.
    let mut lengths: Vec<f64> = edges.iter().map(|e| {
        let (a, b) = (fwd(e.from), fwd(e.to));
        (b[0] - a[0]).hypot(b[1] - a[1])
    }).filter(|l| l.is_finite() && *l > 0.0).collect();
    lengths.sort_by(f64::total_cmp);
    let scale = lengths.get(lengths.len() / 2).copied().unwrap_or(extent).max(1e-6);
    let h0 = p.kernel_start.max(1e-6) * scale;
    let h_end = h0 * p.kernel_decay.clamp(0.1, 1.0).powi(p.iterations.saturating_sub(1) as i32);
    let margin = 1.5 * h0;
    let cells = ((extent + 2.0 * margin) / (h_end / 2.0)).clamp(64.0, p.grid.clamp(64, 4096) as f64);
    let field = Field::new([lo[0] - margin, lo[1] - margin], [hi[0] + margin, hi[1] + margin], (extent + 2.0 * margin) / cells);

    // The corridors as point strands, straight to begin with.
    let spacing = (p.spacing.max(1e-6) * scale).max(1e-9);
    let max_points = p.max_points.max(2);
    let mut strands: Vec<Strand> = edges
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let (a, b) = (fwd(e.from), fwd(e.to));
            let len = (b[0] - a[0]).hypot(b[1] - a[1]);
            let live = len.is_finite() && len > 0.0;
            let n = if live { ((len / spacing).round() as usize + 1).clamp(5.min(max_points), max_points) } else { 2 };
            let pts = (0..n).map(|k| lerp2(a, b, k as f64 / (n - 1) as f64)).collect();
            let w = weight_of(i);
            Strand { pts, a, b, splat: if live { w.powf(p.weight_power) } else { 0.0 }, live }
        })
        .collect();
    // Corridors whose ends coincide are not drawn as a strand at all, but
    // the ends of a near-zero corridor must not divide by zero either.
    let taper_span = p.end_taper.clamp(0.0, 0.5);

    let mut density = vec![0.0f64; field.nx * field.ny];
    let mut h = h0;
    for _ in 0..p.iterations {
        let sigma = (h / 2.0).max(field.cell * 0.5);
        field.splat(&strands, taper_span, &mut density);
        field.blur(&mut density, sigma / field.cell);
        let step = p.step.clamp(0.0, 2.0);
        let (field, density) = (&field, &density);
        par_each(&mut strands, &|_, s: &mut Strand| {
            if !s.live {
                return;
            }
            let n = s.pts.len();
            for i in 1..n - 1 {
                let t = taper(i as f64 / (n - 1) as f64, taper_span);
                if t <= 0.0 {
                    continue;
                }
                let q = s.pts[i];
                let Some(m) = field.mean_shift(density, q, sigma) else { continue };
                let k = step * t;
                let (mut dx, mut dy) = (m[0] * k, m[1] * k);
                let len = dx.hypot(dy);
                if len > h {
                    dx *= h / len;
                    dy *= h / len;
                }
                s.pts[i] = [q[0] + dx, q[1] + dy];
            }
            smooth(&mut s.pts, p.smoothing.clamp(0.0, 1.0), p.smoothing_passes);
            let pts = resample(&s.pts, n);
            s.pts = pts;
        });
        h = (h * p.kernel_decay.clamp(0.1, 1.0)).max(field.cell);
    }

    // Relax toward the straight line, and draw with at most `out_points`.
    let relax = p.relax.clamp(0.0, 1.0);
    let out_points = p.out_points.max(2);
    par_each(&mut strands, &|_, s: &mut Strand| {
        if !s.live {
            s.pts = vec![s.a, s.b];
            return;
        }
        let n = s.pts.len();
        for (i, q) in s.pts.iter_mut().enumerate() {
            let line = lerp2(s.a, s.b, i as f64 / (n - 1) as f64);
            *q = lerp2(*q, line, relax);
        }
        let chord = (s.b[0] - s.a[0]).hypot(s.b[1] - s.a[1]);
        let off = s.pts.iter().map(|q| distance_to_segment(*q, s.a, s.b)).fold(0.0, f64::max);
        if off < chord * 0.005 {
            s.pts = vec![s.a, s.b];
        } else if n > out_points {
            s.pts = resample(&s.pts, out_points);
        }
        let last = s.pts.len() - 1;
        s.pts[0] = s.a;
        s.pts[last] = s.b;
    });

    // The shared load: every corridor's weight added once to each cell of a
    // bundle-wide grid it passes through.
    let load_cell = h.max(field.cell);
    let loads = Field::new(field.origin, field.far, load_cell);
    let mut visits: Vec<Vec<u32>> = vec![Vec::new(); strands.len()];
    {
        let strands = &strands;
        let loads = &loads;
        par_each(&mut visits, &|i, v: &mut Vec<u32>| *v = loads.cells_on(&strands[i].pts));
    }
    let mut total = vec![0.0f64; loads.nx * loads.ny];
    let mut count = vec![0u32; loads.nx * loads.ny];
    for (i, v) in visits.iter().enumerate() {
        let w = weight_of(i);
        for &c in v {
            total[c as usize] += w;
            count[c as usize] += 1;
        }
    }

    strands
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let e = &edges[i];
            if !s.live {
                return straight(e, weight_of(i));
            }
            let n = s.pts.len();
            let mut points: Vec<[f64; 2]> = s.pts.iter().map(|q| back(*q)).collect();
            points[0] = e.from;
            points[n - 1] = e.to;
            let at: Vec<usize> = s.pts.iter().map(|q| loads.index(*q)).collect();
            let load = at.iter().map(|c| total[*c].max(weight_of(i))).collect();
            let corridors = at.iter().map(|c| count[*c].max(1)).collect();
            BundledPath { points, load, corridors, routed: n > 2 }
        })
        .collect()
}

fn straight(e: &PathEdge, w: f64) -> BundledPath {
    BundledPath { points: vec![e.from, e.to], load: vec![w; 2], corridors: vec![1; 2], routed: false }
}

struct Strand {
    /// Projected, metres.
    pts: Vec<[f64; 2]>,
    a: [f64; 2],
    b: [f64; 2],
    /// What each unit of its length adds to the density.
    splat: f64,
    /// Has length (a corridor within one place does not move).
    live: bool,
}

/// `0` at an end, rising smoothly to `1` at `span` of the length in.
fn taper(s: f64, span: f64) -> f64 {
    if span <= 0.0 {
        return 1.0;
    }
    let x = (s.min(1.0 - s) / span).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn lerp2(a: [f64; 2], b: [f64; 2], t: f64) -> [f64; 2] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

fn distance_to_segment(q: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let l2 = dx * dx + dy * dy;
    let t = if l2 > 0.0 { (((q[0] - a[0]) * dx + (q[1] - a[1]) * dy) / l2).clamp(0.0, 1.0) } else { 0.0 };
    (q[0] - a[0] - t * dx).hypot(q[1] - a[1] - t * dy)
}

/// Jacobi Laplacian smoothing, ends fixed.
fn smooth(pts: &mut [[f64; 2]], alpha: f64, passes: usize) {
    let n = pts.len();
    if n < 3 || alpha <= 0.0 {
        return;
    }
    let mut prev = pts.to_vec();
    for _ in 0..passes {
        prev.copy_from_slice(pts);
        for i in 1..n - 1 {
            let mid = [(prev[i - 1][0] + prev[i + 1][0]) / 2.0, (prev[i - 1][1] + prev[i + 1][1]) / 2.0];
            pts[i] = lerp2(prev[i], mid, alpha);
        }
    }
}

/// `n` points evenly along `pts` by arc length, the ends exact.
fn resample(pts: &[[f64; 2]], n: usize) -> Vec<[f64; 2]> {
    let m = pts.len();
    if m < 2 || n < 2 {
        return pts.to_vec();
    }
    let mut along = Vec::with_capacity(m);
    along.push(0.0f64);
    for i in 1..m {
        along.push(along[i - 1] + (pts[i][0] - pts[i - 1][0]).hypot(pts[i][1] - pts[i - 1][1]));
    }
    let total = along[m - 1];
    if !(total > 0.0) {
        return (0..n).map(|k| lerp2(pts[0], pts[m - 1], k as f64 / (n - 1) as f64)).collect();
    }
    let mut out = Vec::with_capacity(n);
    out.push(pts[0]);
    let mut seg = 1usize;
    for k in 1..n - 1 {
        let want = total * k as f64 / (n - 1) as f64;
        while seg < m - 1 && along[seg] < want {
            seg += 1;
        }
        let span = along[seg] - along[seg - 1];
        let t = if span > 0.0 { (want - along[seg - 1]) / span } else { 0.0 };
        out.push(lerp2(pts[seg - 1], pts[seg], t));
    }
    out.push(pts[m - 1]);
    out
}

/// A regular grid over the projected frame.
struct Field {
    origin: [f64; 2],
    far: [f64; 2],
    cell: f64,
    nx: usize,
    ny: usize,
}

impl Field {
    fn new(origin: [f64; 2], far: [f64; 2], cell: f64) -> Self {
        let cell = cell.max(1e-9);
        let nx = (((far[0] - origin[0]) / cell).ceil() as usize + 1).clamp(2, 8192);
        let ny = (((far[1] - origin[1]) / cell).ceil() as usize + 1).clamp(2, 8192);
        Self { origin, far, cell, nx, ny }
    }

    /// Grid coordinates (cell centres at integers).
    fn grid_xy(&self, q: [f64; 2]) -> [f64; 2] {
        [(q[0] - self.origin[0]) / self.cell, (q[1] - self.origin[1]) / self.cell]
    }

    fn index(&self, q: [f64; 2]) -> usize {
        let g = self.grid_xy(q);
        let x = (g[0].round().max(0.0) as usize).min(self.nx - 1);
        let y = (g[1].round().max(0.0) as usize).min(self.ny - 1);
        y * self.nx + x
    }

    /// Every strand's points into `out` (cleared first), bilinearly, each
    /// weighted by its corridor's splat, its taper and the length it
    /// stands for.
    fn splat(&self, strands: &[Strand], taper_span: f64, out: &mut [f64]) {
        out.iter_mut().for_each(|v| *v = 0.0);
        for s in strands {
            if !s.live || s.splat <= 0.0 {
                continue;
            }
            let n = s.pts.len();
            for i in 1..n - 1 {
                let t = taper(i as f64 / (n - 1) as f64, taper_span);
                if t <= 0.0 {
                    continue;
                }
                let span = 0.5 * ((s.pts[i + 1][0] - s.pts[i - 1][0]).hypot(s.pts[i + 1][1] - s.pts[i - 1][1]));
                let w = s.splat * t * span;
                let g = self.grid_xy(s.pts[i]);
                let (x0, y0) = (g[0].floor(), g[1].floor());
                let (fx, fy) = (g[0] - x0, g[1] - y0);
                let (x0, y0) = (x0 as isize, y0 as isize);
                for (dx, dy, k) in [(0, 0, (1.0 - fx) * (1.0 - fy)), (1, 0, fx * (1.0 - fy)), (0, 1, (1.0 - fx) * fy), (1, 1, fx * fy)] {
                    let (x, y) = (x0 + dx, y0 + dy);
                    if x >= 0 && y >= 0 && (x as usize) < self.nx && (y as usize) < self.ny {
                        out[y as usize * self.nx + x as usize] += w * k;
                    }
                }
            }
        }
    }

    /// Three box blurs along each axis: a Gaussian of `sigma` cells.
    fn blur(&self, grid: &mut [f64], sigma: f64) {
        // Three boxes of width w have variance 3 (w^2 - 1) / 12.
        let w = (4.0 * sigma * sigma + 1.0).sqrt();
        let r = (((w - 1.0) / 2.0).round() as usize).max(1);
        let (nx, ny) = (self.nx, self.ny);
        let mut rows: Vec<&mut [f64]> = grid.chunks_mut(nx).collect();
        par_each(&mut rows, &|_, row: &mut &mut [f64]| {
            let mut tmp = vec![0.0; row.len()];
            for _ in 0..3 {
                box_line(row, &mut tmp, r);
            }
        });
        let mut t = vec![0.0; nx * ny];
        transpose(grid, &mut t, nx, ny);
        let mut cols: Vec<&mut [f64]> = t.chunks_mut(ny).collect();
        par_each(&mut cols, &|_, col: &mut &mut [f64]| {
            let mut tmp = vec![0.0; col.len()];
            for _ in 0..3 {
                box_line(col, &mut tmp, r);
            }
        });
        transpose(&t, grid, ny, nx);
    }

    fn sample(&self, grid: &[f64], g: [f64; 2]) -> f64 {
        let x = g[0].clamp(0.0, (self.nx - 1) as f64);
        let y = g[1].clamp(0.0, (self.ny - 1) as f64);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.nx - 1), (y0 + 1).min(self.ny - 1));
        let (fx, fy) = (x - x0 as f64, y - y0 as f64);
        let at = |x: usize, y: usize| grid[y * self.nx + x];
        (at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx) * (1.0 - fy) + (at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx) * fy
    }

    /// The mean-shift vector at `q`, metres: `sigma^2 grad(rho) / rho`.
    fn mean_shift(&self, grid: &[f64], q: [f64; 2], sigma: f64) -> Option<[f64; 2]> {
        let g = self.grid_xy(q);
        let rho = self.sample(grid, g);
        if !(rho > 1e-300) {
            return None;
        }
        let gx = (self.sample(grid, [g[0] + 1.0, g[1]]) - self.sample(grid, [g[0] - 1.0, g[1]])) / (2.0 * self.cell);
        let gy = (self.sample(grid, [g[0], g[1] + 1.0]) - self.sample(grid, [g[0], g[1] - 1.0])) / (2.0 * self.cell);
        let k = sigma * sigma / rho;
        let m = [gx * k, gy * k];
        (m[0].is_finite() && m[1].is_finite()).then_some(m)
    }

    /// The cells a polyline passes through, each once, ascending.
    fn cells_on(&self, pts: &[[f64; 2]]) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for w in pts.windows(2) {
            let len = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
            let steps = ((len / (self.cell * 0.5)).ceil() as usize).clamp(1, 1 << 16);
            for k in 0..=steps {
                let c = self.index(lerp2(w[0], w[1], k as f64 / steps as f64)) as u32;
                if out.last() != Some(&c) {
                    out.push(c);
                }
            }
        }
        if out.is_empty() {
            if let Some(q) = pts.first() {
                out.push(self.index(*q) as u32);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// One box blur of radius `r` along `line`, zero beyond its ends.
fn box_line(line: &mut [f64], tmp: &mut [f64], r: usize) {
    let n = line.len();
    let norm = 1.0 / (2 * r + 1) as f64;
    // Prefix sums keep it exact enough and O(n).
    let mut acc = 0.0;
    for v in line.iter().take(r.min(n)) {
        acc += *v;
    }
    for i in 0..n {
        if i + r < n {
            acc += line[i + r];
        }
        if i > r {
            acc -= line[i - r - 1];
        }
        tmp[i] = acc * norm;
    }
    line.copy_from_slice(tmp);
}

fn transpose(src: &[f64], dst: &mut [f64], w: usize, h: usize) {
    for y in 0..h {
        for x in 0..w {
            dst[x * h + y] = src[y * w + x];
        }
    }
}

/// Runs `f` on every item, on scoped threads where allowed. Each item is
/// independent, so the result does not depend on the split.
fn par_each<T: Send>(items: &mut [T], f: &(dyn Fn(usize, &mut T) + Sync)) {
    let threads = if cfg!(target_arch = "wasm32") { 1 } else { std::thread::available_parallelism().map_or(1, |n| n.get()).min(16) };
    if threads <= 1 || items.len() < 64 {
        for (i, t) in items.iter_mut().enumerate() {
            f(i, t);
        }
        return;
    }
    let chunk = items.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for (c, part) in items.chunks_mut(chunk).enumerate() {
            scope.spawn(move || {
                for (j, t) in part.iter_mut().enumerate() {
                    f(c * chunk + j, t);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(from: [f64; 2], to: [f64; 2]) -> PathEdge {
        PathEdge { from, to }
    }

    fn metres(a: [f64; 2], b: [f64; 2]) -> f64 {
        let cos = ((a[1] + b[1]) / 2.0).to_radians().cos();
        ((b[0] - a[0]) * cos * M_PER_DEG).hypot((b[1] - a[1]) * M_PER_DEG)
    }

    fn midpoint(p: &BundledPath) -> [f64; 2] {
        // By arc length: the drawn points are even along it.
        p.points[p.points.len() / 2]
    }

    /// A field of corridors over England-sized extents: a few parallel
    /// bundles' worth of strands plus scatter, deterministic.
    fn field(n: usize) -> (Vec<PathEdge>, Vec<f64>) {
        let mut s: u64 = 0x2545_f491_4f6c_dd1d;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut edges = Vec::with_capacity(n);
        let mut weights = Vec::with_capacity(n);
        for _ in 0..n {
            let a = [-5.0 + 7.0 * rnd(), 50.0 + 5.0 * rnd()];
            let b = [-5.0 + 7.0 * rnd(), 50.0 + 5.0 * rnd()];
            edges.push(e(a, b));
            weights.push(1.0 + 1000.0 * rnd() * rnd());
        }
        (edges, weights)
    }

    #[test]
    fn the_ends_stay_exactly_where_the_movement_is() {
        let (edges, weights) = field(300);
        let out = bundle_density(&edges, &weights, DensityBundlingParams::default());
        assert_eq!(out.len(), edges.len());
        for (p, e) in out.iter().zip(&edges) {
            assert_eq!(p.points[0], e.from);
            assert_eq!(*p.points.last().unwrap(), e.to);
            assert_eq!(p.points.len(), p.load.len());
            assert_eq!(p.points.len(), p.corridors.len());
        }
        assert!(out.iter().filter(|p| p.routed).count() > 200, "a dense field bundles");
    }

    #[test]
    fn the_same_input_bundles_the_same_way() {
        let (edges, weights) = field(400);
        let a = bundle_density(&edges, &weights, DensityBundlingParams::default());
        let b = bundle_density(&edges, &weights, DensityBundlingParams::default());
        assert_eq!(a, b);
    }

    #[test]
    fn two_parallel_neighbours_converge() {
        // Two 200 km corridors 8 km apart, among a few far-off others that
        // set the extent.
        let edges = [e([-2.0, 52.0], [1.0, 52.0]), e([-2.0, 52.072], [1.0, 52.072]), e([-4.0, 50.0], [-3.9, 50.1]), e([2.0, 55.0], [2.1, 54.9])];
        let before = metres([-0.5, 52.0], [-0.5, 52.072]);
        let out = bundle_density(&edges, &[1.0, 1.0, 1.0, 1.0], DensityBundlingParams::default());
        let after = metres(midpoint(&out[0]), midpoint(&out[1]));
        assert!(after < before * 0.5, "the midpoints close from {before:.0} m to {after:.0} m");
        assert!(out[0].routed && out[1].routed);
    }

    #[test]
    fn a_lone_corridor_stays_near_its_straight_line() {
        let edges = [e([-2.0, 52.0], [1.0, 53.0]), e([-4.0, 50.0], [-3.95, 50.05]), e([2.0, 55.0], [2.05, 55.05])];
        let out = bundle_density(&edges, &[5.0, 1.0, 1.0], DensityBundlingParams::default());
        let chord = metres(edges[0].from, edges[0].to);
        let (a, b) = (edges[0].from, edges[0].to);
        let worst = out[0]
            .points
            .iter()
            .map(|q| {
                // Distance from the chord, in metres.
                let t = ((q[0] - a[0]) * (b[0] - a[0]) + (q[1] - a[1]) * (b[1] - a[1])) / ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2));
                metres(*q, lerp2(a, b, t.clamp(0.0, 1.0)))
            })
            .fold(0.0, f64::max);
        assert!(worst < chord * 0.01, "{worst:.0} m off a {chord:.0} m chord");
    }

    #[test]
    fn weights_are_preserved_in_the_load() {
        // A lone corridor carries its own weight; two that merge carry the
        // sum where they share the trunk, and never less than their own.
        let edges = [e([-2.0, 52.0], [1.0, 52.0]), e([-2.0, 52.03], [1.0, 52.03]), e([-4.0, 50.0], [-3.0, 50.2]), e([2.0, 55.0], [2.1, 54.9])];
        let w = [300.0, 100.0, 7.0, 1.0];
        let out = bundle_density(&edges, &w, DensityBundlingParams::default());
        assert!(out[2].load.iter().all(|l| (*l - 7.0).abs() < 1e-9), "lone: {:?}", out[2].load);
        let trunk = out[0].load.iter().copied().fold(0.0, f64::max);
        assert!((trunk - 400.0).abs() < 1e-9, "shared trunk: {:?}", out[0].load);
        for (p, w) in out.iter().zip(w) {
            assert!(p.load.iter().all(|l| *l >= w), "never less than its own");
        }
        assert_eq!(out[0].corridors.iter().max(), Some(&2));
    }

    #[test]
    fn degenerate_corridors_are_drawn_straight() {
        let edges = [e([0.0, 50.0], [0.0, 50.0]), e([0.0, 50.0], [1.0, 50.0])];
        let out = bundle_density(&edges, &[1.0, 2.0], DensityBundlingParams::default());
        assert_eq!(out[0].points, vec![[0.0, 50.0], [0.0, 50.0]]);
        assert!(!out[0].routed);
        assert!(bundle_density(&[], &[], DensityBundlingParams::default()).is_empty());
    }

    /// The budget: about 300 ms for 10k corridors at 40 points on a 512
    /// grid. Release only (`cargo test --release -- --ignored`).
    #[test]
    #[ignore = "timing; run in release with --nocapture"]
    fn ten_thousand_corridors_within_the_budget() {
        let (edges, weights) = field(10_000);
        let p = DensityBundlingParams { spacing: 1.0 / 400.0, grid: 512, ..DensityBundlingParams::default() };
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let out = bundle_density(&edges, &weights, p);
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
            assert_eq!(out.len(), 10_000);
        }
        let d = bundle_density(&edges, &weights, DensityBundlingParams::default());
        let points: usize = d.iter().map(|p| p.points.len()).sum();
        eprintln!("kdeeb: 10k corridors x 40 points, 512 grid, {} iterations: best {best:.0} ms; default spacing draws {points} points", p.iterations);
        if !cfg!(debug_assertions) {
            assert!(best < 400.0, "{best:.0} ms");
        }
    }
}
