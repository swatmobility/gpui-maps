//! What the view shows of a frame's dots: which cell centres are on
//! screen, and the people in them -- the counter's "people in view"
//! (`DotStats::people_in_view`).
//!
//! A cell is in view when its CENTRE is: the rule `grid::cell_window`
//! uses to give a cell to one tile, so a cell is counted once. The view is
//! a set of half-spaces -- the flat camera's frustum on the ground plane,
//! or the globe's frustum and horizon on the unit sphere -- and every one
//! of them cuts a row of cells (one latitude) in an interval of
//! longitude: an interval of columns. With each drawn tile's cells in a
//! [`CellIndex`] (its columns and people, row by row, with a prefix sum
//! every 64 cells), a row's people is two binary searches and a few
//! additions, so a frame costs its rows, not its cells.

use super::grid::{self, CellWindow};
use super::{GridDescriptor, GridTile, GridTileKey};
use crate::geo::TileId;
use crate::overlay::geom::lon_lat_to_world;
use crate::render::WORLD_SIZE;
use std::f64::consts::PI;

/// A half-space `x * v.x + y * v.y + z * v.z + v.w <= 0`: on the ground
/// plane (unit-world Mercator `x, y`, `z` unused) or on the unit sphere.
type HalfSpace = glam::DVec4;

/// The part of the world a frame shows, as half-spaces a point must be
/// inside every one of.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewRegion {
    /// Unit-world Mercator `x, y` on the ground plane (a flat camera's
    /// frustum, or a box).
    Flat(Vec<HalfSpace>),
    /// Points `grid`'s cells put on the unit sphere with
    /// `globe::mercator_to_sphere`.
    Globe(Vec<SphereHalfSpace>),
}

/// A unit-sphere half-space `dot(n, p) + w <= 0` in the form a row of
/// cells cuts it in: `n.xz`'s length and angle, once per frame, so a row
/// costs one `acos` per half-space that crosses it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SphereHalfSpace {
    h: HalfSpace,
    /// `|n.xz|` and `atan2(n.x, n.z)`: `n.x sin sx + n.z cos sx =
    /// xz_len cos(sx - xz_angle)`.
    xz_len: f64,
    xz_angle: f64,
}

impl SphereHalfSpace {
    fn new(h: HalfSpace) -> Self {
        Self { h, xz_len: h.x.hypot(h.z), xz_angle: h.x.atan2(h.z) }
    }
}

impl ViewRegion {
    /// The unit-world Mercator box `min..max`.
    #[must_use]
    pub fn mercator_box(min: [f64; 2], max: [f64; 2]) -> Self {
        Self::Flat(vec![
            HalfSpace::new(-1.0, 0.0, 0.0, min[0]),
            HalfSpace::new(1.0, 0.0, 0.0, -max[0]),
            HalfSpace::new(0.0, -1.0, 0.0, min[1]),
            HalfSpace::new(0.0, 1.0, 0.0, -max[1]),
        ])
    }

    /// A flat camera's frustum on the ground: the clip-space conditions
    /// `-w <= x, y <= w`, `0 <= z <= w` and `w > 0` of
    /// `Camera::view_projection_f64` (world units), turned into unit-world
    /// half-spaces of the ground plane `z = 0`. Exact at any pitch.
    #[must_use]
    pub fn flat(camera: &crate::render::Camera) -> Self {
        let m = camera.view_projection_f64();
        let (r0, r1, r2, r3) = (m.row(0), m.row(1), m.row(2), m.row(3));
        let world = f64::from(WORLD_SIZE);
        // (a, b, _, c) . (X, Y, 0, 1) with X = x * world.
        let ground = |v: glam::DVec4| HalfSpace::new(v.x * world, v.y * world, 0.0, v.w);
        let mut ahead = ground(-r3);
        ahead.w += 1e-9;
        Self::Flat(vec![ground(r0 - r3), ground(-r0 - r3), ground(r1 - r3), ground(-r1 - r3), ground(r2 - r3), ground(-r2), ahead])
    }

    /// A globe frame's visible cap: its frustum and its horizon
    /// (`GlobeCover::visible_half_spaces`).
    #[must_use]
    pub fn globe(globe: &crate::render::globe::GlobeCover) -> Self {
        Self::Globe(globe.visible_half_spaces().into_iter().map(SphereHalfSpace::new).collect())
    }

    /// The view for a frame: the globe's when it has one, else the flat
    /// camera's frustum.
    #[must_use]
    pub fn for_frame(camera: &crate::render::Camera, globe: &crate::render::globe::GlobeCover) -> Self {
        if globe.is_globe() { Self::globe(globe) } else { Self::flat(camera) }
    }

    /// Whether the unit-world Mercator point `(x, y)` is in view.
    #[must_use]
    pub fn contains(&self, x: f64, y: f64) -> bool {
        match self {
            Self::Flat(hs) => hs.iter().all(|h| h.x * x + h.y * y + h.w <= 0.0),
            Self::Globe(hs) => {
                let p = crate::render::globe::mercator_to_sphere(x, y);
                hs.iter().all(|s| s.h.truncate().dot(p) + s.h.w <= 0.0)
            }
        }
    }

    /// The unit-world Mercator `y` range the view can reach: a flat
    /// view's from the corners of its ground polygon (every pair of its
    /// half-spaces' edges that meet inside all the others), unbounded for
    /// a globe cap (rows are then sampled at their middle).
    #[must_use]
    pub fn y_extent(&self) -> (f64, f64) {
        match self.flat_extent() {
            Some((_, y)) => y,
            None => (f64::NEG_INFINITY, f64::INFINITY),
        }
    }

    /// A flat view's ground polygon's `x` and `y` ranges (unit-world,
    /// `x` unwrapped), from its corners; `None` for a globe cap or a
    /// view with no corners.
    fn flat_extent(&self) -> Option<((f64, f64), (f64, f64))> {
        let Self::Flat(hs) = self else { return None };
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut xlo, mut xhi) = (f64::INFINITY, f64::NEG_INFINITY);
        for (i, a) in hs.iter().enumerate() {
            for b in &hs[i + 1..] {
                let det = a.x * b.y - a.y * b.x;
                if det.abs() < 1e-300 {
                    continue;
                }
                // a.x x + a.y y = -a.w, b.x x + b.y y = -b.w
                let x = (-a.w * b.y + b.w * a.y) / det;
                let y = (-a.x * b.w + b.x * a.w) / det;
                let slack = |h: &HalfSpace| h.x * x + h.y * y + h.w <= 1e-9 * (h.x.abs() + h.y.abs() + h.w.abs());
                if hs.iter().all(slack) {
                    lo = lo.min(y);
                    hi = hi.max(y);
                    xlo = xlo.min(x);
                    xhi = xhi.max(x);
                }
            }
        }
        (lo <= hi).then_some(((xlo, xhi), (lo, hi)))
    }

    /// The row at unit-world Mercator `y`'s in-view `x`, as disjoint
    /// intervals `[x0, x1]` inside `[0, 1]` (a view across the antimeridian
    /// gives two). No allocation: this runs once per row of every drawn
    /// tile.
    #[must_use]
    pub fn x_intervals(&self, y: f64) -> Spans {
        self.spans(y, None)
    }

    /// [`Self::x_intervals`], given the row's latitude too when the caller
    /// has it: a globe row then skips the Mercator round trip.
    fn spans(&self, y: f64, lat_deg: Option<f64>) -> Spans {
        match self {
            Self::Flat(hs) => {
                let (mut lo, mut hi) = (f64::NEG_INFINITY, f64::INFINITY);
                for h in hs {
                    // h.x * x <= -(h.y * y + h.w)
                    let k = -(h.y * y + h.w);
                    if h.x > 0.0 {
                        hi = hi.min(k / h.x);
                    } else if h.x < 0.0 {
                        lo = lo.max(k / h.x);
                    } else if k < 0.0 {
                        return Spans::EMPTY;
                    }
                }
                wrap_unit(lo, hi)
            }
            Self::Globe(hs) => {
                let (sin_lat, cos_lat) = match lat_deg {
                    Some(lat) => lat.to_radians().sin_cos(),
                    None => crate::render::globe::mercator_y_to_sin_cos_lat(y),
                };
                // Allowed spherical_x, as intervals of [0, 2 pi).
                let mut allowed = Spans::one(0.0, 2.0 * PI);
                for s in hs {
                    // (nx sin sx + nz cos sx) cos_lat + ny sin_lat + w <= 0
                    // with nx sin sx + nz cos sx = |n.xz| cos(sx - phi).
                    let r = s.xz_len * cos_lat;
                    let k = -(s.h.y * sin_lat + s.h.w);
                    let arc = if r <= 1e-15 {
                        if k >= 0.0 { continue } else { return Spans::EMPTY }
                    } else {
                        let q = k / r;
                        if q >= 1.0 {
                            continue;
                        }
                        if q < -1.0 {
                            return Spans::EMPTY;
                        }
                        // cos(sx - phi) <= q: sx - phi in [acos q, 2 pi - acos q].
                        let a = q.acos();
                        (s.xz_angle + a, s.xz_angle + 2.0 * PI - a)
                    };
                    allowed = allowed.intersect(&circle_pieces(arc.0, arc.1));
                    if allowed.is_empty() {
                        return allowed;
                    }
                }
                // spherical_x = 2 pi x + pi.
                let mut out = Spans::EMPTY;
                for &(a, b) in allowed.as_slice() {
                    for &piece in wrap_unit((a - PI) / (2.0 * PI), (b - PI) / (2.0 * PI)).as_slice() {
                        out.push(piece);
                    }
                }
                out
            }
        }
    }
}

/// A few disjoint intervals, on the stack.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spans {
    v: [(f64, f64); 8],
    n: usize,
}

impl Spans {
    const EMPTY: Self = Self { v: [(0.0, 0.0); 8], n: 0 };

    fn one(a: f64, b: f64) -> Self {
        let mut s = Self::EMPTY;
        s.push((a, b));
        s
    }

    /// Past eight pieces (the intersection of a view's six arcs is one or
    /// two) the rest are dropped -- never reached by a real view.
    fn push(&mut self, piece: (f64, f64)) {
        debug_assert!(self.n < self.v.len(), "more pieces than a view makes");
        if self.n < self.v.len() {
            self.v[self.n] = piece;
            self.n += 1;
        }
    }

    #[must_use]
    pub fn as_slice(&self) -> &[(f64, f64)] {
        &self.v[..self.n]
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn intersect(&self, other: &Self) -> Self {
        let mut out = Self::EMPTY;
        for &(a0, a1) in self.as_slice() {
            for &(b0, b1) in other.as_slice() {
                let (lo, hi) = (a0.max(b0), a1.min(b1));
                if lo <= hi {
                    out.push((lo, hi));
                }
            }
        }
        out
    }
}

/// `[lo, hi]` (any real `x`) folded onto `[0, 1]`: the whole world when it
/// spans one, else its one or two pieces.
fn wrap_unit(lo: f64, hi: f64) -> Spans {
    if !(lo <= hi) {
        return Spans::EMPTY;
    }
    if hi - lo >= 1.0 {
        return Spans::one(0.0, 1.0);
    }
    let shift = lo.floor();
    let (lo, hi) = (lo - shift, hi - shift);
    if hi <= 1.0 {
        Spans::one(lo, hi)
    } else {
        let mut s = Spans::one(lo, 1.0);
        s.push((0.0, hi - 1.0));
        s
    }
}

/// The arc `[a, b]` (radians, `b - a <= 2 pi`) as pieces of `[0, 2 pi)`.
fn circle_pieces(a: f64, b: f64) -> Spans {
    let tau = 2.0 * PI;
    let mut s = wrap_unit(a / tau, b / tau);
    for piece in &mut s.v[..s.n] {
        *piece = (piece.0 * tau, piece.1 * tau);
    }
    s
}

/// A tile's populated cells row by row of its window: each cell's column
/// and people, where each row's cells start, and the people before every
/// [`CellIndex::STRIDE`]th cell. The people in any run of one row's
/// columns is two binary searches within that row and at most
/// `STRIDE / 2` additions at each end, and an empty row costs nothing.
///
/// About 6 bytes a populated cell: a `u16` column (a `u32` for a window
/// wider than 65,536 columns, [`Cols`]), the cell's `f32` people exactly
/// as the tile holds them, and an `f64` prefix every 64 cells -- so a sum
/// is exact to `f64` rounding. (An `f32` running sum was 6 bytes flat but
/// lost up to 7e-4 of a short run in a row of 2.5e7-people cells.) 4
/// bytes a row. Built the first time the counter needs it, for a tile the
/// view's boundary cuts (`layer::HeldCells`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellIndex {
    /// Local column of each cell, row by row, ascending within a row.
    col: Cols,
    /// People in each cell.
    pop: Vec<f32>,
    /// `prefix[j]` = people in cells `0..j * STRIDE` (all of them for the
    /// last); `n / STRIDE + 2` long.
    prefix: Vec<f64>,
    /// Live epochs (Task 15): epoch B's people in each cell and their
    /// prefix, as `pop` and `prefix` -- empty when every cell's `pop_b`
    /// equals its `pop` (a single-epoch tile), which reads as epoch A.
    pop_b: Vec<f32>,
    prefix_b: Vec<f64>,
    /// `row_start[r]..row_start[r + 1]` = the cells of window row `r`;
    /// one longer than the window's rows.
    row_start: Vec<u32>,
    cols: u32,
}

/// A [`CellIndex`]'s columns: `u16` while the window's columns fit one,
/// `u32` past that -- a descriptor's window width is not bounded by type,
/// so a wide one keeps exact columns rather than panicking.
#[derive(Debug, Clone, PartialEq)]
enum Cols {
    Narrow(Vec<u16>),
    Wide(Vec<u32>),
}

impl Default for Cols {
    fn default() -> Self {
        Self::Narrow(Vec::new())
    }
}

impl Cols {
    fn with_capacity(cols: u32, n: usize) -> Self {
        if cols <= 1 << 16 { Self::Narrow(Vec::with_capacity(n)) } else { Self::Wide(Vec::with_capacity(n)) }
    }

    fn push(&mut self, c: u32) {
        match self {
            // `with_capacity` chose Narrow only for columns below 2^16.
            Self::Narrow(v) => v.push(c as u16),
            Self::Wide(v) => v.push(c),
        }
    }

    fn get(&self, p: usize) -> u32 {
        match self {
            Self::Narrow(v) => u32::from(v[p]),
            Self::Wide(v) => v[p],
        }
    }

    /// The first position in `a..b` whose column is not below `c`.
    fn lower_bound(&self, a: usize, b: usize, c: u32) -> usize {
        a + match self {
            Self::Narrow(v) => v[a..b].partition_point(|&i| u32::from(i) < c),
            Self::Wide(v) => v[a..b].partition_point(|&i| i < c),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Narrow(v) => v.len(),
            Self::Wide(v) => v.len(),
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::Narrow(v) => v.capacity() * size_of::<u16>(),
            Self::Wide(v) => v.capacity() * size_of::<u32>(),
        }
    }
}

impl CellIndex {
    /// Cells between two `f64` prefixes.
    pub const STRIDE: usize = 64;

    /// `tile`'s cells, for its window `w` (`grid::cell_window`).
    #[must_use]
    pub fn new(tile: &GridTile, w: &CellWindow) -> Self {
        // A decoded tile's cells are in index order: no sorted copy then.
        let sorted;
        let cells: &[(u32, f32, f32)] = if tile.cells.is_sorted_by_key(|c| c.0) {
            &tile.cells
        } else {
            let mut c = tile.cells.clone();
            c.sort_by_key(|c| c.0);
            sorted = c;
            &sorted
        };
        let cols = w.cols.max(1);
        let rows = w.rows as usize;
        let n = cells.partition_point(|c| u64::from(c.0) < u64::from(w.rows) * u64::from(cols));
        let mut col = Cols::with_capacity(cols, n);
        let live = cells[..n].iter().any(|c| c.2 != c.1);
        let mut pop = Vec::with_capacity(n);
        let mut prefix = Vec::with_capacity(n / Self::STRIDE + 2);
        let (mut pop_b, mut prefix_b) = if live {
            (Vec::with_capacity(n), Vec::with_capacity(n / Self::STRIDE + 2))
        } else {
            (Vec::new(), Vec::new())
        };
        let mut run_b = 0.0f64;
        let mut row_start = Vec::with_capacity(rows + 1);
        row_start.push(0);
        let (mut r, mut run) = (0usize, 0.0f64);
        for (p, &(i, people, people_b)) in cells[..n].iter().enumerate() {
            let ri = (i / cols) as usize;
            while r < ri {
                r += 1;
                row_start.push(p as u32);
            }
            if p % Self::STRIDE == 0 {
                prefix.push(run);
                if live {
                    prefix_b.push(run_b);
                }
            }
            col.push(i % cols);
            pop.push(people);
            run += f64::from(people);
            if live {
                pop_b.push(people_b);
                run_b += f64::from(people_b);
            }
        }
        while r < rows {
            r += 1;
            row_start.push(n as u32);
        }
        // The prefix at `n` itself, and at the next stride when `n` ends one.
        prefix.push(run);
        if n % Self::STRIDE == 0 {
            prefix.push(run);
        }
        if live {
            prefix_b.push(run_b);
            if n % Self::STRIDE == 0 {
                prefix_b.push(run_b);
            }
        }
        Self { col, pop, prefix, pop_b, prefix_b, row_start, cols }
    }

    /// People in cells `0..p` of one epoch's `pop`/`prefix`: the nearer
    /// stride prefix and the cells between it and `p`.
    fn prefix_of(pop: &[f32], prefix: &[f64], p: usize) -> f64 {
        let n = pop.len();
        let j = p / Self::STRIDE;
        let (lo, hi) = (j * Self::STRIDE, ((j + 1) * Self::STRIDE).min(n));
        if p - lo <= Self::STRIDE / 2 || hi <= p {
            prefix[j] + pop[lo..p].iter().map(|&x| f64::from(x)).sum::<f64>()
        } else {
            prefix[j + 1] - pop[p..hi].iter().map(|&x| f64::from(x)).sum::<f64>()
        }
    }

    /// One epoch's people in cells `a..b`: summed directly when short, so
    /// a short run is as exact as its cells.
    fn range_of(pop: &[f32], prefix: &[f64], a: usize, b: usize) -> f64 {
        if b <= a {
            0.0
        } else if b - a <= Self::STRIDE {
            pop[a..b].iter().map(|&x| f64::from(x)).sum()
        } else {
            Self::prefix_of(pop, prefix, b) - Self::prefix_of(pop, prefix, a)
        }
    }

    /// People in cells `a..b` at epoch mix `t` (`[0, 1]`): epoch A's at 0,
    /// B's at 1, linear between -- the count the dots at `t` draw
    /// (`DotParams::t` mixes each cell's `pop` the same way). A
    /// single-epoch index is epoch A's at every `t`.
    fn range(&self, a: usize, b: usize, t: f32) -> f64 {
        let at_a = || Self::range_of(&self.pop, &self.prefix, a, b);
        let at_b = || Self::range_of(&self.pop_b, &self.prefix_b, a, b);
        if self.pop_b.is_empty() || t <= 0.0 {
            at_a()
        } else if t >= 1.0 {
            at_b()
        } else {
            let t = f64::from(t);
            at_a() * (1.0 - t) + at_b() * t
        }
    }

    /// Where local column `c` would sit in row `r`'s cells `a..b`, and
    /// past it when the row holds it.
    fn at(&self, a: usize, b: usize, c: u32) -> (usize, usize) {
        let p = self.col.lower_bound(a, b, c);
        (p, if p < b && self.col.get(p) == c { p + 1 } else { p })
    }

    /// People in window row `r`'s cells with local column in `c0..c1`
    /// (epoch A's; [`Self::pop_in_row_at`] for a mix).
    #[must_use]
    pub fn pop_in_row(&self, r: u32, c0: u32, c1: u32) -> f64 {
        self.pop_in_row_at(r, c0, c1, 0.0)
    }

    /// [`Self::pop_in_row`] at epoch mix `t`.
    #[must_use]
    pub fn pop_in_row_at(&self, r: u32, c0: u32, c1: u32, t: f32) -> f64 {
        let r = r as usize;
        if c0 >= c1 || r + 1 >= self.row_start.len() {
            return 0.0;
        }
        let (a, b) = (self.row_start[r] as usize, self.row_start[r + 1] as usize);
        let (lo, _) = self.at(a, b, c0);
        let (hi, _) = self.at(a, b, c1);
        self.range(lo, hi, t)
    }

    /// Window row `r`'s people in local column `cf`, in local column `cl`
    /// (`cl >= cf`; 0 when they are the same column), and in the columns
    /// strictly between: two binary searches, as [`Self::pop_in_row`].
    fn row_ends(&self, r: u32, cf: u32, cl: u32, t: f32) -> (f64, f64, f64) {
        let r = r as usize;
        if r + 1 >= self.row_start.len() {
            return (0.0, 0.0, 0.0);
        }
        let (a, b) = (self.row_start[r] as usize, self.row_start[r + 1] as usize);
        let (p0, p0_end) = self.at(a, b, cf);
        let first = self.range(p0, p0_end, t);
        if cl == cf {
            return (first, 0.0, 0.0);
        }
        let (p1, p1_end) = self.at(a, b, cl);
        (first, self.range(p1, p1_end, t), self.range(p0_end, p1, t))
    }

    /// The heap bytes the index holds: its vectors' capacity.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.col.heap_bytes()
            + self.pop.capacity() * size_of::<f32>()
            + self.prefix.capacity() * size_of::<f64>()
            + self.pop_b.capacity() * size_of::<f32>()
            + self.prefix_b.capacity() * size_of::<f64>()
            + self.row_start.capacity() * size_of::<u32>()
    }

    fn is_empty(&self) -> bool {
        self.col.len() == 0
    }

    /// The first and last window rows holding a cell.
    fn populated_rows(&self) -> Option<(u32, u32)> {
        let full = |r: &usize| self.row_start[*r] < self.row_start[*r + 1];
        let rows = self.row_start.len().saturating_sub(1);
        let first = (0..rows).find(full)?;
        let last = (0..rows).rev().find(full)?;
        Some((first as u32, last as u32))
    }

    fn row_is_empty(&self, r: u32) -> bool {
        let r = r as usize;
        r + 1 >= self.row_start.len() || self.row_start[r] == self.row_start[r + 1]
    }
}

/// Where a tile's cell window lies against a view: what decides whether
/// its people in view need its [`CellIndex`] at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// No point of the window is in view: its people in view are 0.
    Outside,
    /// Every point of the window is in view: its people in view are all
    /// of its drawn cells' (less the ones a stand-in's boxes exclude,
    /// [`excluded_pop`]).
    Inside,
    /// The view's boundary may cut it: [`pop_in_view`], cell by cell.
    Edge,
}

/// Where `key`'s cell window (its whole lat/lon rectangle, every cell
/// of it) lies against `view`. Conservative: `Inside` and `Outside` are
/// only said when they hold for every point of the window (to a relative
/// `1e-12` of each half-space), else `Edge` -- whose exact
/// [`pop_in_view`] is right either way. A flat view is tested at every
/// whole-world shift of the window its `x` range can reach, as its rows
/// are folded onto `[0, 1]`.
///
/// A part-tile key (C7) has no cell window: [`part_coverage`].
#[must_use]
pub fn coverage(d: &GridDescriptor, key: &GridTileKey, view: &ViewRegion) -> Coverage {
    if key.is_part() {
        return part_coverage(key.tile, 0.0, view);
    }
    let w = grid::cell_window(d, key.level, key.tile);
    if w.rows == 0 || w.cols == 0 {
        return Coverage::Outside;
    }
    let arc = grid::cell_arcsec(d, key.level) / 3600.0;
    let x0 = (d.origin_lon + f64::from(w.col0) * arc + 180.0) / 360.0;
    let x1 = (d.origin_lon + f64::from(w.col0 + w.cols) * arc + 180.0) / 360.0;
    let lat_top = d.origin_lat - f64::from(w.row0) * arc;
    let lat_bottom = d.origin_lat - f64::from(w.row0 + w.rows) * arc;
    rect_coverage(view, x0, x1, lat_top, lat_bottom)
}

/// Where Mercator tile `tile`, grown by `grow` of a tile on every side
/// (the part-tile overhang, `layer::PART_OVERHANG_TILES`; 0 for the tile
/// itself), lies against `view`. Conservative as [`coverage`] is.
#[must_use]
pub fn part_coverage(tile: TileId, grow: f64, view: &ViewRegion) -> Coverage {
    let n = f64::from(1u32 << tile.z);
    let x0 = (f64::from(tile.x) - grow) / n;
    let x1 = (f64::from(tile.x) + 1.0 + grow) / n;
    let y0 = ((f64::from(tile.y) - grow) / n).max(0.0);
    let y1 = ((f64::from(tile.y) + 1.0 + grow) / n).min(1.0);
    let lat = |y: f64| (PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
    rect_coverage(view, x0, x1, lat(y0), lat(y1))
}

/// People in part tile `tile` whose part's centroid is in `view`: the
/// rule the converter gives a part to one tile by (the owner holds its
/// centroid), so a part counts once, as a cell counts by its centre.
#[must_use]
pub fn part_pop_in_view(tile: &GridTile, view: &ViewRegion) -> f64 {
    tile.parts
        .iter()
        .filter(|p| {
            let cx = (p.a[0] + p.b[0] + p.c[0]) / 3.0;
            let cy = (p.a[1] + p.b[1] + p.c[1]) / 3.0;
            // A flat view's x is unwrapped: the part's copies a world
            // either side, as `coverage` shifts a window.
            [0.0, -1.0, 1.0].iter().any(|k| view.contains(cx + k, cy))
        })
        .map(|p| p.counts.iter().map(|&c| f64::from(c)).sum::<f64>())
        .sum()
}

/// [`coverage`] of the unit-world `x0..x1` (unwrapped), latitude
/// `lat_bottom..lat_top` rectangle.
fn rect_coverage(view: &ViewRegion, x0: f64, x1: f64, lat_top: f64, lat_bottom: f64) -> Coverage {
    const EPS: f64 = 1e-12;
    match view {
        ViewRegion::Flat(hs) => {
            let Some(((vx0, vx1), _)) = view.flat_extent() else { return Coverage::Edge };
            let world = f64::from(WORLD_SIZE);
            let y0 = lon_lat_to_world(0.0, lat_top).1 / world;
            let y1 = lon_lat_to_world(0.0, lat_bottom).1 / world;
            let (k0, k1) = ((vx0 - x1).floor(), (vx1 - x0).ceil());
            if !(k0.is_finite() && k1.is_finite()) || k1 - k0 > 8.0 {
                return Coverage::Edge;
            }
            let mut all_out = true;
            let mut k = k0;
            while k <= k1 {
                let (mut inside, mut out) = (true, false);
                for h in hs {
                    let tol = EPS * (h.x.abs() + h.y.abs() + h.w.abs());
                    let (ax, bx) = (h.x * (x0 + k), h.x * (x1 + k));
                    let (ay, by) = (h.y * y0, h.y * y1);
                    let hi = ax.max(bx) + ay.max(by) + h.w;
                    let lo = ax.min(bx) + ay.min(by) + h.w;
                    if hi > -tol {
                        inside = false;
                    }
                    if lo > tol {
                        out = true;
                    }
                }
                if inside {
                    return Coverage::Inside;
                }
                all_out &= out;
                k += 1.0;
            }
            if all_out { Coverage::Outside } else { Coverage::Edge }
        }
        ViewRegion::Globe(hs) => {
            // spherical_x = 2 pi x + pi; the window never wraps.
            let (s0, s1) = (2.0 * PI * x0 + PI, 2.0 * PI * x1 + PI);
            let (lo_lat, hi_lat) = (lat_bottom.to_radians(), lat_top.to_radians());
            let cos_lat = (
                lo_lat.cos().min(hi_lat.cos()),
                if lo_lat <= 0.0 && hi_lat >= 0.0 { 1.0 } else { lo_lat.cos().max(hi_lat.cos()) },
            );
            let sin_lat = (lo_lat.sin(), hi_lat.sin());
            let (mut inside, mut out) = (true, false);
            for s in hs {
                // f = xz_len cos(sx - phi) cos_lat + n.y sin_lat + w, bounded
                // over the rectangle term by term.
                let c = cos_range(s0 - s.xz_angle, s1 - s.xz_angle);
                let p = [c.0 * cos_lat.0, c.0 * cos_lat.1, c.1 * cos_lat.0, c.1 * cos_lat.1].map(|v| v * s.xz_len);
                let (pa, pb) = (s.h.y * sin_lat.0, s.h.y * sin_lat.1);
                let hi = p.iter().copied().fold(f64::NEG_INFINITY, f64::max) + pa.max(pb) + s.h.w;
                let lo = p.iter().copied().fold(f64::INFINITY, f64::min) + pa.min(pb) + s.h.w;
                if hi > -EPS {
                    inside = false;
                }
                if lo > EPS {
                    out = true;
                }
            }
            if out {
                Coverage::Outside
            } else if inside {
                Coverage::Inside
            } else {
                Coverage::Edge
            }
        }
    }
}

/// `cos` over `[a, b]` (`b - a <= 2 pi`): its least and greatest value.
fn cos_range(a: f64, b: f64) -> (f64, f64) {
    let tau = 2.0 * PI;
    let hits = |at: f64| ((a - at) / tau).ceil() * tau + at <= b;
    let hi = if hits(0.0) { 1.0 } else { a.cos().max(b.cos()) };
    let lo = if hits(PI) { -1.0 } else { a.cos().min(b.cos()) };
    (lo, hi)
}

/// People in `key`'s cells that a stand-in's `exclude` boxes keep from
/// drawing: the cells whose SW corner (tile-local lyon units, f32) is in
/// a box -- exactly the cells [`pop_in_view`] cuts. A walk of the tile's
/// cells, no index: for a tile [`Coverage::Inside`] the view.
#[must_use]
pub fn excluded_pop(d: &GridDescriptor, key: &GridTileKey, tile: &GridTile, exclude: &[[f32; 4]; 2]) -> f64 {
    excluded_pops(d, key, tile, exclude).0
}

/// [`excluded_pop`] for both epochs of a live tile, `(pop, pop_b)`: the
/// people at a mix `t` are `pop + (pop_b - pop) * t`.
#[must_use]
pub fn excluded_pops(d: &GridDescriptor, key: &GridTileKey, tile: &GridTile, exclude: &[[f32; 4]; 2]) -> (f64, f64) {
    let boxes: Vec<[f32; 4]> = exclude.iter().copied().filter(|b| b[0] <= b[2] && b[1] <= b[3]).collect();
    if boxes.is_empty() {
        return (0.0, 0.0);
    }
    let w = grid::cell_window(d, key.level, key.tile);
    if w.rows == 0 || w.cols == 0 {
        return (0.0, 0.0);
    }
    let arc = grid::cell_arcsec(d, key.level) / 3600.0;
    let world = f64::from(WORLD_SIZE);
    let n = f64::from(1u32 << key.tile.z);
    let (ox, oy) = (f64::from(key.tile.x) / n, f64::from(key.tile.y) / n);
    let l = super::blocks::OVERLAY_LYON_PER_UNIT_WORLD;
    let sw_x = |col: f64| (((d.origin_lon + col * arc + 180.0) / 360.0 - ox) * l) as f32;
    let sw_y = |row: f64| ((lon_lat_to_world(0.0, d.origin_lat - (row + 1.0) * arc).1 / world - oy) * l) as f32;
    let mut row_y = (u32::MAX, 0.0f32);
    let (mut sum, mut sum_b) = (0.0, 0.0);
    for &(i, pop, pop_b) in &tile.cells {
        let r = i / w.cols;
        if r >= w.rows {
            continue;
        }
        if row_y.0 != r {
            row_y = (r, sw_y(f64::from(w.row0 + r)));
        }
        let (x, y) = (sw_x(f64::from(w.col0 + i % w.cols)), row_y.1);
        if boxes.iter().any(|b| y >= b[1] && y <= b[3] && x >= b[0] && x <= b[2]) {
            sum += f64::from(pop);
            sum_b += f64::from(pop_b);
        }
    }
    (sum, sum_b)
}

/// Columns `[c0, c1)` of window `w` whose value `at(col)` -- increasing
/// in `col`, and linear up to float rounding -- lies in `[lo, hi]`: a
/// linear estimate across the window, then settled on `at` itself at both
/// ends (a lyon-unit `at` is rounded to f32).
fn cols_where(w: &CellWindow, at: impl Fn(f64) -> f64, lo: f64, hi: f64) -> (u32, u32) {
    let (first, last) = (w.col0, w.col0 + w.cols - 1);
    if w.cols == 0 || at(f64::from(first)) > hi || at(f64::from(last)) < lo {
        return (0, 0);
    }
    let a = at(f64::from(first));
    let b = if w.cols > 1 { (at(f64::from(last)) - a) / f64::from(w.cols - 1) } else { 1.0 };
    let clamp = |v: f64| v.clamp(f64::from(first), f64::from(last) + 1.0) as u32;
    let mut c0 = clamp(f64::from(first) + ((lo - a) / b).ceil());
    let mut c1 = clamp(f64::from(first) + ((hi - a) / b).floor() + 1.0);
    let ok = |c: u32| {
        let v = at(f64::from(c));
        v >= lo && v <= hi
    };
    while c0 > first && ok(c0 - 1) {
        c0 -= 1;
    }
    while c0 <= last && c0 < c1 && !ok(c0) {
        c0 += 1;
    }
    while c1 <= last && ok(c1) {
        c1 += 1;
    }
    while c1 > c0 && !ok(c1 - 1) {
        c1 -= 1;
    }
    if c1 <= c0 { (0, 0) } else { (c0, c1) }
}

/// People in `key`'s drawn cells that `view` shows: each cell's
/// population times the share of its area in view (its lat/lon rectangle
/// in the Mercator plane), so a view smaller than one cell -- a street
/// zoom over 30″ cells -- still counts the people it shows (the user's
/// "0 people in view"). Every populated cell of the tile counts, less those
/// a stand-in's `exclude` boxes (tile-local lyon units, tested on the
/// cell's SW corner exactly as `vs_dot` tests a block's origin) keep from
/// being drawn.
///
/// Exact for a Mercator box. For a frustum or a globe cap a row is sampled
/// along its height: at its middle when the view spans the whole row, at
/// four lines through the part of it inside the view's `y` extent when
/// the view's north or south edge crosses it; across each line the share
/// of every column is exact.
#[must_use]
pub fn pop_in_view(d: &GridDescriptor, key: &GridTileKey, index: &CellIndex, exclude: &[[f32; 4]; 2], view: &ViewRegion) -> f64 {
    pop_in_view_at(d, key, index, exclude, view, 0.0)
}

/// [`pop_in_view`] at epoch mix `t` (Task 15): every cell's people are
/// `pop + (pop_b - pop) * t`, as the dots at `t` draw them. `t` 0, or a
/// single-epoch index, is [`pop_in_view`].
#[must_use]
pub fn pop_in_view_at(
    d: &GridDescriptor,
    key: &GridTileKey,
    index: &CellIndex,
    exclude: &[[f32; 4]; 2],
    view: &ViewRegion,
    t: f32,
) -> f64 {
    let w = grid::cell_window(d, key.level, key.tile);
    if index.is_empty() || w.rows == 0 || w.cols == 0 || index.cols != w.cols {
        return 0.0;
    }
    let arc = grid::cell_arcsec(d, key.level) / 3600.0;
    let world = f64::from(WORLD_SIZE);
    let n = f64::from(1u32 << key.tile.z);
    let (ox, oy) = (f64::from(key.tile.x) / n, f64::from(key.tile.y) / n);
    let l = super::blocks::OVERLAY_LYON_PER_UNIT_WORLD;
    let sw_x = |col: f64| (((d.origin_lon + col * arc + 180.0) / 360.0 - ox) * l) as f32;
    // Column edges in unit-world x: linear in the column.
    let edge_x = |col: f64| (d.origin_lon + col * arc + 180.0) / 360.0;
    let col_w = arc / 360.0;
    let (win_x0, win_x1) = (edge_x(f64::from(w.col0)), edge_x(f64::from(w.col0 + w.cols)));
    let (vy0, vy1) = view.y_extent();
    let merc_y = |lat: f64| lon_lat_to_world(0.0, lat).1 / world;
    let boxes: Vec<[f32; 4]> = exclude.iter().copied().filter(|b| b[0] <= b[2] && b[1] <= b[3]).collect();
    // Index ranges only from the first to the last populated row.
    let Some((first_row, last_row)) = index.populated_rows() else { return 0.0 };
    let mut sum = 0.0;
    let mut cut: Vec<(u32, u32)> = Vec::new();
    let unbounded = !(vy0.is_finite() && vy1.is_finite());
    let last = w.col0 + w.cols - 1;
    // Row `r`'s people along the line at `y`, its cells cut by `cut`.
    let line_pop = |r: u32, y: f64, lat: Option<f64>, cut: &[(u32, u32)]| -> f64 {
        let local = |c: u32| c - w.col0;
        let mut total = 0.0;
        for &(x0, x1) in view.spans(y, lat).as_slice() {
            let (x0, x1) = (x0.max(win_x0), x1.min(win_x1));
            if x0 >= x1 {
                continue;
            }
            let at = |x: f64| ((x - win_x0) / col_w).floor().clamp(0.0, f64::from(w.cols - 1)) as u32 + w.col0;
            let (cf, cl) = (at(x0), at(x1));
            let share = |c: u32| {
                let (a, b) = (edge_x(f64::from(c)), edge_x(f64::from(c) + 1.0));
                ((x1.min(b) - x0.max(a)) / col_w).clamp(0.0, 1.0)
            };
            let in_cut = |c: u32| cut.iter().any(|&(e0, e1)| c >= e0 && c < e1);
            // The end columns by their share, the ones between whole.
            let (pop_first, pop_last, between) = index.row_ends(r, local(cf), local(cl), t);
            if !in_cut(cf) {
                total += pop_first * share(cf);
            }
            if cl > cf && !in_cut(cl) {
                total += pop_last * share(cl);
            }
            if cl > cf + 1 {
                let (i0, i1) = (cf + 1, cl.min(last + 1));
                total += between;
                // Two boxes of one stand-in never overlap
                // (`exclusion_rects` takes disjoint rectangles), so nothing
                // is subtracted twice.
                for &(e0, e1) in cut {
                    let (s0, s1) = (e0.max(i0), e1.min(i1));
                    if s0 < s1 {
                        total -= index.pop_in_row_at(r, local(s0), local(s1), t);
                    }
                }
            }
        }
        total
    };
    // The last row's south edge: the next row's north edge.
    let mut prev_bottom: Option<(u32, f64)> = None;
    for r in first_row..=last_row.min(w.rows - 1) {
        if index.row_is_empty(r) {
            continue;
        }
        let row = w.row0 + r;
        // A stand-in's boxes cut this row where its SW corner is in one.
        cut.clear();
        if !boxes.is_empty() {
            let sw_y = ((merc_y(d.origin_lat - (f64::from(row) + 1.0) * arc) - oy) * l) as f32;
            for b in boxes.iter().filter(|b| sw_y >= b[1] && sw_y <= b[3]) {
                let (c0, c1) = cols_where(&w, |c| f64::from(sw_x(c)), f64::from(b[0]), f64::from(b[2]));
                if c0 < c1 {
                    cut.push((c0, c1));
                }
            }
        }
        if unbounded {
            // A globe cap: every row is sampled at its middle, and its
            // edges are not needed (one Mercator conversion, not two).
            let lat = d.origin_lat - (f64::from(row) + 0.5) * arc;
            sum += match view {
                ViewRegion::Globe(_) => line_pop(r, f64::NAN, Some(lat), &cut),
                ViewRegion::Flat(_) => line_pop(r, merc_y(lat), None, &cut),
            };
            continue;
        }
        let y_top = match prev_bottom {
            Some((p, y)) if p + 1 == r => y,
            _ => merc_y(d.origin_lat - f64::from(row) * arc),
        };
        let y_bottom = merc_y(d.origin_lat - (f64::from(row) + 1.0) * arc);
        prev_bottom = Some((r, y_bottom));
        let (lo, hi) = (y_top.max(vy0), y_bottom.min(vy1));
        if lo >= hi {
            continue;
        }
        let lines = if lo == y_top && hi == y_bottom { 1 } else { 4 };
        let weight = (hi - lo) / (y_bottom - y_top) / f64::from(lines);
        for k in 0..lines {
            let y = lo + (f64::from(k) + 0.5) * (hi - lo) / f64::from(lines);
            sum += weight * line_pop(r, y, None, &cut);
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::TileId;

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (*seed >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Row intervals agree with the point test, flat and pitched: every
    /// sampled point is in an interval of its row exactly when `contains`
    /// says so, and `contains` agrees with projecting through the camera.
    #[test]
    fn a_flat_frustums_row_intervals_are_its_points() {
        for (pitch, bearing) in [(0.0f32, 0.0f32), (45.0, 30.0), (60.0, -100.0)] {
            let camera = crate::render::Camera {
                center_lon: 43.95,
                center_lat: 56.30,
                zoom: 9.5,
                pitch,
                bearing,
                viewport_width_px: 2000.0,
                viewport_height_px: 1260.0,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let view = ViewRegion::flat(&camera);
            let m = camera.view_projection_f64();
            let world = f64::from(WORLD_SIZE);
            let (cx, cy) = lon_lat_to_world(43.95, 56.30);
            let (cx, cy) = (cx / world, cy / world);
            assert!(view.contains(cx, cy), "the centre is in view");
            let mut seed = 7u64;
            let (mut inside, mut outside) = (0, 0);
            for _ in 0..20_000 {
                let x = cx + (lcg(&mut seed) - 0.5) * 0.012;
                let y = cy + (lcg(&mut seed) - 0.5) * 0.012;
                let clip = m * glam::DVec4::new(x * world, y * world, 0.0, 1.0);
                let projected = clip.w > 0.0 && clip.x.abs() <= clip.w && clip.y.abs() <= clip.w && clip.z >= 0.0 && clip.z <= clip.w;
                let c = view.contains(x, y);
                // Points within float noise of an edge may go either way.
                let margin = (clip.x.abs() - clip.w).abs().min((clip.y.abs() - clip.w).abs()) / clip.w.abs();
                if margin > 1e-9 {
                    assert_eq!(c, projected, "pitch {pitch} at ({x}, {y})");
                }
                let in_row = view.x_intervals(y).as_slice().iter().any(|&(a, b)| x >= a && x <= b);
                assert_eq!(in_row, c, "pitch {pitch}: the row interval disagrees with contains at ({x}, {y})");
                if c { inside += 1 } else { outside += 1 }
            }
            assert!(inside > 1_000 && outside > 1_000, "pitch {pitch}: {inside} in, {outside} out");
        }
    }

    /// The globe's row intervals are its half-spaces, which are the
    /// globe frame's own projection: a point on the visible cap projects
    /// inside the clip volume, in front of the horizon plane.
    #[test]
    fn a_globe_views_row_intervals_are_its_points() {
        for (zoom, lat) in [(3.0, 20.0), (5.0, 22.0), (9.5, 56.3)] {
            let camera = crate::render::Camera {
                center_lon: 78.9,
                center_lat: lat,
                zoom,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: 1280.0,
                viewport_height_px: 800.0,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let frame = crate::render::globe::globe_frame(&camera);
            let globe = crate::render::globe::GlobeCover::new(&frame);
            assert!(globe.is_globe());
            let view = ViewRegion::globe(&globe);
            let world = f64::from(WORLD_SIZE);
            let (cx, cy) = lon_lat_to_world(78.9, lat);
            let (cx, cy) = (cx / world, cy / world);
            assert!(view.contains(cx, cy));
            let span = 3.0 / 2f64.powf(zoom);
            let mut seed = 11u64;
            let (mut inside, mut outside) = (0, 0);
            for _ in 0..20_000 {
                let x = (cx + (lcg(&mut seed) - 0.5) * span).rem_euclid(1.0);
                let y = (cy + (lcg(&mut seed) - 0.5) * span).clamp(0.0, 1.0);
                let p = crate::render::globe::mercator_to_sphere(x, y);
                let clip = frame.globe_view_proj * p.extend(1.0);
                let n = glam::DVec4::from_array(frame.clipping_plane);
                let projected = clip.w > 0.0
                    && clip.x.abs() <= clip.w
                    && clip.y.abs() <= clip.w
                    && clip.z >= 0.0
                    && n.truncate().dot(p) + n.w >= 0.0;
                let margin = (clip.x.abs() - clip.w).abs().min((clip.y.abs() - clip.w).abs()) / clip.w.abs();
                let c = view.contains(x, y);
                if margin > 1e-7 && (n.truncate().dot(p) + n.w).abs() > 1e-9 {
                    assert_eq!(c, projected, "z{zoom} at ({x}, {y})");
                }
                let spans = view.x_intervals(y);
                let in_row = spans.as_slice().iter().any(|&(a, b)| x >= a - 1e-12 && x <= b + 1e-12);
                let near_edge = spans.as_slice().iter().any(|&(a, b)| (x - a).abs() < 1e-9 || (x - b).abs() < 1e-9);
                if !near_edge {
                    assert_eq!(in_row, c, "z{zoom}: the row interval disagrees with contains at ({x}, {y})");
                }
                if c { inside += 1 } else { outside += 1 }
            }
            assert!(inside > 500 && outside > 500, "z{zoom}: {inside} in, {outside} out");
        }
    }

    /// `coverage` is never wrong: every point sampled in a window it calls
    /// `Inside` is in view, none in one it calls `Outside` -- flat,
    /// pitched and on the globe -- and it does say both.
    #[test]
    fn coverage_inside_and_outside_hold_for_every_point_of_the_window() {
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: String::new(),
        };
        let cam = |zoom: f64, pitch: f32, bearing: f32| crate::render::Camera {
            center_lon: 78.9,
            center_lat: 22.0,
            zoom,
            pitch,
            bearing,
            viewport_width_px: 1280.0,
            viewport_height_px: 800.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let mut views = vec![
            (ViewRegion::flat(&cam(7.5, 0.0, 0.0)), 6u8, 7u8),
            (ViewRegion::flat(&cam(7.5, 50.0, 30.0)), 6, 7),
            (ViewRegion::mercator_box([0.71, 0.43], [0.73, 0.44]), 6, 7),
        ];
        for zoom in [3.0, 5.0] {
            let frame = crate::render::globe::globe_frame(&cam(zoom, 0.0, 0.0));
            views.push((ViewRegion::globe(&crate::render::globe::GlobeCover::new(&frame)), 4, if zoom < 4.0 { 8 } else { 3 }));
        }
        let (mut inside, mut outside) = (0, 0);
        let mut seed = 3u64;
        for (view, z, reach) in &views {
            let n = 1u32 << z;
            let (cx, cy) = lon_lat_to_world(78.9, 22.0);
            let (cx, cy) = ((cx / f64::from(WORLD_SIZE) * f64::from(n)) as i64, (cy / f64::from(WORLD_SIZE) * f64::from(n)) as i64);
            let reach = i64::from(*reach);
            for ty in (cy - reach).max(0)..=(cy + reach).min(i64::from(n) - 1) {
                for tx in cx - reach..=cx + reach {
                    let tile = TileId { z: *z, x: tx.rem_euclid(i64::from(n)) as u32, y: ty as u32 };
                    let level = grid::data_zoom(&d, 0) - *z;
                    let key = GridTileKey { epoch: 2025, epoch_b: None, level, tile };
                    let c = coverage(&d, &key, view);
                    if c == Coverage::Edge {
                        continue;
                    }
                    if c == Coverage::Inside { inside += 1 } else { outside += 1 }
                    let w = grid::cell_window(&d, level, tile);
                    let arc = grid::cell_arcsec(&d, level) / 3600.0;
                    for _ in 0..300 {
                        let lon = d.origin_lon + (f64::from(w.col0) + lcg(&mut seed) * f64::from(w.cols)) * arc;
                        let lat = d.origin_lat - (f64::from(w.row0) + lcg(&mut seed) * f64::from(w.rows)) * arc;
                        let (x, y) = lon_lat_to_world(lon, lat);
                        let (x, y) = (x / f64::from(WORLD_SIZE), y / f64::from(WORLD_SIZE));
                        let shown = view.contains(x, y) || view.contains(x + 1.0, y) || view.contains(x - 1.0, y);
                        assert_eq!(shown, c == Coverage::Inside, "{c:?} tile {tile:?} at ({lon}, {lat})");
                    }
                }
            }
        }
        assert!(inside > 0 && outside > 0, "{inside} inside, {outside} outside");
    }

    #[test]
    fn wrap_unit_folds_a_view_across_the_antimeridian() {
        assert_eq!(wrap_unit(0.2, 0.3).as_slice(), &[(0.2, 0.3)]);
        let w = wrap_unit(0.9, 1.1);
        let w = w.as_slice();
        assert_eq!(w.len(), 2);
        assert!((w[0].0 - 0.9).abs() < 1e-12 && w[0].1 == 1.0 && w[1].0 == 0.0 && (w[1].1 - 0.1).abs() < 1e-12);
        assert_eq!(wrap_unit(-0.5, 0.7).as_slice(), &[(0.0, 1.0)]);
        assert!(wrap_unit(0.5, 0.4).is_empty());
    }

    #[test]
    fn a_cell_index_sums_any_index_range() {
        // A 3 x 4 window: cells 2 (row 0), 5 (row 1) and 9 (row 2), unsorted.
        let tile = GridTile { cells: vec![(9, 4.0, 4.0), (2, 1.0, 1.0), (5, 2.0, 2.0)], total: 7.0, ..Default::default() };
        let index = CellIndex::new(&tile, &CellWindow { row0: 10, col0: 20, rows: 3, cols: 4 });
        assert_eq!(index.pop_in_row(0, 0, 4), 1.0);
        assert_eq!(index.pop_in_row(0, 3, 4), 0.0);
        assert_eq!(index.pop_in_row(1, 1, 2), 2.0);
        assert_eq!(index.pop_in_row(1, 2, 4), 0.0);
        assert_eq!(index.pop_in_row(2, 0, 4), 4.0);
        assert_eq!(index.pop_in_row(2, 1, 1), 0.0);
        assert_eq!(index.pop_in_row(3, 0, 4), 0.0, "past the window");
        assert!(!index.row_is_empty(0) && index.row_is_empty(3));
        // A window past 65,536 columns keeps exact `u32` columns.
        let wide = GridTile { cells: vec![(70_001, 3.0, 3.0), (70_000 + 100_000 + 5, 5.0, 5.0)], total: 8.0, ..Default::default() };
        let index = CellIndex::new(&wide, &CellWindow { row0: 0, col0: 0, rows: 2, cols: 100_000 });
        assert!(matches!(index.col, Cols::Wide(_)));
        assert_eq!(index.pop_in_row(0, 70_001, 70_002), 3.0);
        assert_eq!(index.pop_in_row(0, 70_001 - 65_536, 70_002 - 65_536), 0.0, "not aliased to a u16 column");
        assert_eq!(index.pop_in_row(1, 70_005, 70_006), 5.0);
        let _ = TileId { z: 0, x: 0, y: 0 };
    }
}
