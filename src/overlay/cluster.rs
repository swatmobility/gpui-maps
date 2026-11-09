//! Point clustering and the point level of detail: a port of Supercluster.
//!
//! See `docs/superpowers/specs/2026-09-26-big-point-sources-design.md`.
//!
//! This is Supercluster 8.0.1 (ISC, Mapbox) over KDBush 4.0.2 (ISC,
//! Vladimir Agafonkin), ported to Rust; both notices are in
//! `src/render/PROVENANCE.md`. The algorithm is theirs, unchanged:
//!
//! - every input point is projected to Web Mercator in `[0, 1]` and rounded
//!   to `f32` (`Math.fround` there);
//! - the raw points are level `max_zoom + 1`; each level `z` below is made
//!   from level `z + 1` by walking its items in order and merging every
//!   unprocessed neighbour within `radius / (extent * 2^z)` into a cluster at
//!   the count-weighted centre, when the merged count reaches `min_points`;
//! - each level is a static KD-tree (`Kd`, KDBush's layout: an index
//!   permutation and interleaved coordinates, sorted by alternating-axis
//!   selection, leaves of `NODE_SIZE`);
//! - a cluster's id encodes the item it grew from and its zoom:
//!   `(origin << 5) + (zoom + 1) + point_count_total`, so `children` needs no
//!   stored child list.
//!
//! Two things are added, neither of which changes a clustered result:
//!
//! - `rep`: every item's *representative leaf*, the raw point the item's
//!   chain of origins starts at. The non-clustered level of detail draws a
//!   merged group as that one point, at its own exact position.
//! - `ClusterIndex::build` with `keep_all: false` keeps a level only when it
//!   has at most half the raw count: a non-clustered source at zoom 16 does
//!   not store a second copy of its city. Queries at a dropped level read the
//!   next finer kept level.
//!
//! Aggregates (`clusterProperties`) are restricted to `sum`, `min` and `max`
//! over one numeric column each -- MapLibre's `["+", ["get", k]]`,
//! `["max", ...]`, `["min", ...]`. A leaf with no value for the column does
//! not contribute.

/// KDBush's default leaf size, and Supercluster's.
const NODE_SIZE: usize = 64;

/// "No parent" in [`Level::parent`], and "no aggregate row" in
/// [`Level::agg`].
pub(crate) const NONE: u32 = u32::MAX;

/// The largest point count the id encoding holds in a `u32`:
/// `(origin << 5) + 32 + n` must not overflow.
pub const MAX_POINTS: usize = 100_000_000;

/// How a cluster combines one numeric property of its points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterReduce {
    /// `["+", ["get", column]]`
    Sum,
    /// `["min", ["get", column]]`
    Min,
    /// `["max", ["get", column]]`
    Max,
}

/// One MapLibre `clusterProperties` entry: the cluster gets property
/// `name`, the `reduce` of `column` over its points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterProperty {
    pub name: String,
    pub reduce: ClusterReduce,
    pub column: String,
}

impl ClusterProperty {
    pub fn sum(name: &str, column: &str) -> Self {
        Self { name: name.into(), reduce: ClusterReduce::Sum, column: column.into() }
    }
    pub fn min(name: &str, column: &str) -> Self {
        Self { name: name.into(), reduce: ClusterReduce::Min, column: column.into() }
    }
    pub fn max(name: &str, column: &str) -> Self {
        Self { name: name.into(), reduce: ClusterReduce::Max, column: column.into() }
    }
}

/// A point source's clustering, as MapLibre's `geojson` source options.
///
/// Declared with `MapView::set_source_cluster_options`. `cluster: false`
/// still sends the source down the point path, with a 1 px level of detail
/// instead of clusters.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterOptions {
    /// MapLibre `cluster`.
    pub cluster: bool,
    /// MapLibre `clusterRadius`, in pixels of a 512 px tile. Default 50.
    pub radius_px: f64,
    /// MapLibre `clusterMaxZoom`: the last zoom that clusters. Default 14.
    pub max_zoom: u8,
    /// MapLibre `clusterMinPoints`. Default 2.
    pub min_points: u32,
    /// MapLibre `clusterProperties`.
    pub properties: Vec<ClusterProperty>,
    /// How points group: Supercluster's radius, or a grid roll-up.
    pub mode: ClusterMode,
}

/// How a point source's points group at each zoom.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ClusterMode {
    /// Supercluster's semantics (MapLibre's): at each zoom, greedy
    /// clustering of every point within `radius_px` of an unvisited one, at
    /// the count-weighted centre. A neighbour search per item per zoom.
    #[default]
    Radius,
    /// A Web Mercator quadtree roll-up: at each zoom the points in one cell
    /// of `2 * radius_px` pixels (rounded to a power of two) are one
    /// cluster, at the count-weighted centroid of its points. Each point's
    /// Z-order key is computed once; every coarser level is a run-length
    /// grouping of the level below. O(n) per level, exact counts and
    /// aggregates, no neighbour search -- the fast mode, and the one the
    /// non-clustered level of detail uses. Cluster edges follow the grid,
    /// so two points either side of a cell edge stay apart where the
    /// radius mode would merge them.
    Grid,
    /// An H3 roll-up: at zoom `z` the points in one H3 cell of resolution
    /// `resolutions[z]` are one cluster, at the count-weighted centroid of
    /// its points (not the cell centre: it tracks where the points are, as
    /// the other two modes do). Each point's cell at the finest resolution
    /// is computed once, in parallel; sorting by the cell index makes every
    /// coarser cell's points contiguous, and each level down is a
    /// run-length grouping by `cell.parent(res)`. O(n) per level, exact
    /// counts and aggregates. A cluster is a cell of the H3 HIERARCHY (the
    /// ancestor of its points' finest cells); H3 containment is
    /// approximate, so near a coarse cell's edge a point can sit in its
    /// neighbour geometrically. Needs the `h3` cargo feature (`h3o`, BSD-3);
    /// without it the options are refused. See [`ClusterMode::h3_for`].
    H3 {
        /// Resolution per zoom, `0..=max_zoom`; non-decreasing, each <= 15.
        resolutions: Vec<u8>,
    },
}

impl ClusterMode {
    /// H3 resolutions whose cells are about the radius mode's cluster
    /// size: a resolution-`r` cell's diameter (~2214 km / 2.6458^r) matched
    /// to `2 * radius_px` at zoom `z` (7827 km / 2^z at the default 50 px),
    /// which gives `r = 0.712 z - 1.3 - log(radius_px / 50) / log 2.6458`,
    /// rounded and clamped to 0-15.
    pub fn h3_for(radius_px: f64, max_zoom: u8) -> Self {
        let resolutions = (0..=max_zoom)
            .map(|z| {
                let r = 0.712 * f64::from(z) - 1.3 - (radius_px / 50.0).ln() / 2.6458f64.ln();
                r.round().clamp(0.0, 15.0) as u8
            })
            .collect();
        Self::H3 { resolutions }
    }
}

/// Wall time of each phase of an index build, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BuildPhases {
    /// Projection to Mercator and the Z-order key, in parallel.
    pub project_ms: f64,
    /// The sort along the Z-order curve.
    pub sort_ms: f64,
    /// Every level from the raw points down (the radius mode's per-level
    /// KD-trees included: each is searched by the next level down).
    pub levels_ms: f64,
    /// KD-trees built outside the level loop: the raw level's (radius), or
    /// every kept level's, in parallel (grid).
    pub trees_ms: f64,
}

/// The grid mode's cell size, as a power of two in pixels: `2 * radius`,
/// rounded, clamped to 1-256 px.
pub(crate) fn grid_cell_log2(radius_px: f64) -> u32 {
    (2.0 * radius_px).max(1.0).log2().round().clamp(0.0, 8.0) as u32
}

impl Default for ClusterOptions {
    fn default() -> Self {
        Self {
            cluster: true,
            radius_px: 50.0,
            max_zoom: 14,
            min_points: 2,
            properties: Vec::new(),
            mode: ClusterMode::Radius,
        }
    }
}

impl ClusterOptions {
    /// The non-clustered level of detail: every point drawn, and points in
    /// one 4 px cell drawn once, exact above zoom 16. A grid roll-up.
    pub fn points_only() -> Self {
        Self {
            cluster: false,
            radius_px: LOD_RADIUS_PX,
            max_zoom: 16,
            min_points: 2,
            properties: Vec::new(),
            mode: ClusterMode::Grid,
        }
    }

    /// Refuses what the index cannot honour.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.radius_px.is_finite() && self.radius_px > 0.0) {
            return Err(format!("clusterRadius must be a positive number, got {}", self.radius_px));
        }
        if self.max_zoom > 22 {
            return Err(format!("clusterMaxZoom must be at most 22, got {}", self.max_zoom));
        }
        if let ClusterMode::H3 { resolutions } = &self.mode {
            if !cfg!(feature = "h3") {
                return Err("ClusterMode::H3 needs the `h3` cargo feature, which this build does not have".into());
            }
            if resolutions.len() <= usize::from(self.max_zoom) {
                return Err(format!(
                    "ClusterMode::H3 needs a resolution for every zoom 0..={}, got {}",
                    self.max_zoom,
                    resolutions.len()
                ));
            }
            if resolutions.iter().any(|&r| r > 15) || resolutions.windows(2).any(|w| w[0] > w[1]) {
                return Err("ClusterMode::H3 resolutions must be non-decreasing and at most 15".into());
            }
        }
        if self.min_points < 2 {
            return Err(format!("clusterMinPoints must be at least 2, got {}", self.min_points));
        }
        for (i, p) in self.properties.iter().enumerate() {
            if self.properties[..i].iter().any(|q| q.name == p.name) {
                return Err(format!("clusterProperties: {:?} is declared twice", p.name));
            }
            if matches!(p.name.as_str(), "cluster" | "cluster_id" | "point_count" | "point_count_abbreviated") {
                return Err(format!("clusterProperties: {:?} is a reserved cluster property", p.name));
            }
        }
        Ok(())
    }
}

/// The non-clustered level of detail's `radius_px`: grid cells of twice
/// this, 4 px, hold one drawn point each below zoom 17.
pub const LOD_RADIUS_PX: f64 = 2.0;

/// Supercluster's tile extent: `radius_px` is measured against it.
pub const EXTENT: f64 = 512.0;

/// Longitude to Mercator x in `[0, 1]`.
pub fn lng_x(lng: f64) -> f64 {
    lng / 360.0 + 0.5
}

/// Latitude to Mercator y in `[0, 1]`, clamped, north at 0.
pub fn lat_y(lat: f64) -> f64 {
    let sin = (lat * std::f64::consts::PI / 180.0).sin();
    let y = 0.5 - 0.25 * ((1.0 + sin) / (1.0 - sin)).ln() / std::f64::consts::PI;
    y.clamp(0.0, 1.0)
}

/// Mercator x back to longitude.
pub fn x_lng(x: f64) -> f64 {
    (x - 0.5) * 360.0
}

/// Mercator y back to latitude.
pub fn y_lat(y: f64) -> f64 {
    let y2 = (180.0 - y * 360.0) * std::f64::consts::PI / 180.0;
    360.0 * y2.exp().atan() / std::f64::consts::PI - 90.0
}

/// KDBush: a static KD-tree over `f32` points.
#[derive(Debug, Default)]
pub(crate) struct Kd {
    ids: Vec<u32>,
    coords: Vec<f32>,
}

impl Kd {
    fn build(xs: &[f32], ys: &[f32]) -> Self {
        let n = xs.len();
        let mut ids: Vec<u32> = (0..n as u32).collect();
        let mut coords = Vec::with_capacity(n * 2);
        for i in 0..n {
            coords.push(xs[i]);
            coords.push(ys[i]);
        }
        if n > 0 {
            sort(&mut ids, &mut coords, 0, 0);
        }
        Self { ids, coords }
    }

    fn bytes(&self) -> usize {
        self.ids.capacity() * 4 + self.coords.capacity() * 4
    }

    /// Every id inside the box, inclusive.
    pub(crate) fn range(&self, min_x: f64, min_y: f64, max_x: f64, max_y: f64, out: &mut Vec<u32>) {
        if self.ids.is_empty() {
            return;
        }
        let inside = |x: f64, y: f64| x >= min_x && x <= max_x && y >= min_y && y <= max_y;
        let mut stack: Vec<(usize, usize, u8)> = vec![(0, self.ids.len() - 1, 0)];
        while let Some((left, right, axis)) = stack.pop() {
            if right - left <= NODE_SIZE {
                for i in left..=right {
                    if inside(f64::from(self.coords[2 * i]), f64::from(self.coords[2 * i + 1])) {
                        out.push(self.ids[i]);
                    }
                }
                continue;
            }
            let m = (left + right) >> 1;
            let x = f64::from(self.coords[2 * m]);
            let y = f64::from(self.coords[2 * m + 1]);
            if inside(x, y) {
                out.push(self.ids[m]);
            }
            if (if axis == 0 { min_x <= x } else { min_y <= y }) && m > left {
                stack.push((left, m - 1, 1 - axis));
            }
            if (if axis == 0 { max_x >= x } else { max_y >= y }) && m < right {
                stack.push((m + 1, right, 1 - axis));
            }
        }
    }

    /// Every id within `r` of `(qx, qy)`, inclusive.
    pub(crate) fn within(&self, qx: f64, qy: f64, r: f64, out: &mut Vec<u32>) {
        if self.ids.is_empty() {
            return;
        }
        let r2 = r * r;
        let near = |x: f64, y: f64| {
            let (dx, dy) = (x - qx, y - qy);
            dx * dx + dy * dy <= r2
        };
        let mut stack: Vec<(usize, usize, u8)> = vec![(0, self.ids.len() - 1, 0)];
        while let Some((left, right, axis)) = stack.pop() {
            if right - left <= NODE_SIZE {
                for i in left..=right {
                    if near(f64::from(self.coords[2 * i]), f64::from(self.coords[2 * i + 1])) {
                        out.push(self.ids[i]);
                    }
                }
                continue;
            }
            let m = (left + right) >> 1;
            let x = f64::from(self.coords[2 * m]);
            let y = f64::from(self.coords[2 * m + 1]);
            if near(x, y) {
                out.push(self.ids[m]);
            }
            if (if axis == 0 { qx - r <= x } else { qy - r <= y }) && m > left {
                stack.push((left, m - 1, 1 - axis));
            }
            if (if axis == 0 { qx + r >= x } else { qy + r >= y }) && m < right {
                stack.push((m + 1, right, 1 - axis));
            }
        }
    }
}

/// KDBush's `sort`, on a sub-slice (its `left` is 0 and its `right` is
/// `ids.len() - 1`, which gives the same `m` as the global recursion: `(l +
/// r) >> 1 == l + ((r - l) >> 1)`). The two halves beside the median are
/// disjoint, so the top `PAR_DEPTH` levels of a big tree sort them on
/// separate threads; the result is identical to the sequential sort.
fn sort(ids: &mut [u32], coords: &mut [f32], axis: usize, depth: u32) {
    let n = ids.len();
    if n <= NODE_SIZE + 1 {
        return;
    }
    let m = (n - 1) >> 1;
    select(ids, coords, m, 0, n - 1, axis);
    let (left_ids, right_ids) = ids.split_at_mut(m);
    let (left_coords, right_coords) = coords.split_at_mut(2 * m);
    let (right_ids, right_coords) = (&mut right_ids[1..], &mut right_coords[2..]);
    if depth < PAR_DEPTH && n > PAR_MIN && threads_allowed() {
        std::thread::scope(|scope| {
            scope.spawn(|| sort(left_ids, left_coords, 1 - axis, depth + 1));
            sort(right_ids, right_coords, 1 - axis, depth + 1);
        });
    } else {
        sort(left_ids, left_coords, 1 - axis, depth + 1);
        sort(right_ids, right_coords, 1 - axis, depth + 1);
    }
}

/// The parallel KD sort's fan-out: 2^3 = 8 subtrees.
const PAR_DEPTH: u32 = 3;
/// Below this many items a subtree sorts on the thread it is on.
const PAR_MIN: usize = 50_000;

/// Whether a build may start threads: never on wasm, whose
/// `std::thread::spawn` panics without the atomics target feature.
pub(crate) fn threads_allowed() -> bool {
    !cfg!(target_arch = "wasm32")
}

/// Runs `f` over `out` in contiguous chunks on up to 16 threads; `f` gets
/// the chunk's first index and the chunk.
fn par_chunks<T: Send>(out: &mut [T], f: &(dyn Fn(usize, &mut [T]) + Sync)) {
    let threads = if threads_allowed() {
        std::thread::available_parallelism().map_or(1, |n| n.get()).min(16)
    } else {
        1
    };
    if threads <= 1 || out.len() < 100_000 {
        f(0, out);
        return;
    }
    let chunk = out.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for (i, part) in out.chunks_mut(chunk).enumerate() {
            scope.spawn(move || f(i * chunk, part));
        }
    });
}

/// Interleaves the bits of `x` and `y`: a Z-order (Morton) key, x in the
/// even bits. Sorting by it keeps every quadtree cell contiguous.
fn morton(x: u32, y: u32) -> u64 {
    fn spread(v: u32) -> u64 {
        let mut v = u64::from(v);
        v = (v | (v << 16)) & 0x0000_ffff_0000_ffff;
        v = (v | (v << 8)) & 0x00ff_00ff_00ff_00ff;
        v = (v | (v << 4)) & 0x0f0f_0f0f_0f0f_0f0f;
        v = (v | (v << 2)) & 0x3333_3333_3333_3333;
        v = (v | (v << 1)) & 0x5555_5555_5555_5555;
        v
    }
    spread(x) | (spread(y) << 1)
}

/// The H3 cell of `[lon, lat]` at `res`, as its u64 index (0 for a
/// coordinate H3 refuses; never reached for validated input).
#[cfg(feature = "h3")]
fn h3_cell(p: [f64; 2], res: u8) -> u64 {
    let res = h3o::Resolution::try_from(res).unwrap_or(h3o::Resolution::Fifteen);
    h3o::LatLng::new(p[1], p[0]).map_or(0, |ll| u64::from(ll.to_cell(res)))
}

/// `cell`'s parent at `res`, as its u64 index.
#[cfg(feature = "h3")]
fn h3_parent(cell: u64, res: u8) -> u64 {
    let res = h3o::Resolution::try_from(res).unwrap_or(h3o::Resolution::Zero);
    h3o::CellIndex::try_from(cell).ok().and_then(|c| c.parent(res)).map_or(0, u64::from)
}

#[cfg(not(feature = "h3"))]
fn h3_cell(_: [f64; 2], _: u8) -> u64 {
    unreachable!("ClusterOptions::validate refuses H3 without the `h3` feature")
}

#[cfg(not(feature = "h3"))]
fn h3_parent(_: u64, _: u8) -> u64 {
    unreachable!("ClusterOptions::validate refuses H3 without the `h3` feature")
}

/// A Mercator `[0, 1]` coordinate as a 32-bit grid position.
fn grid32(v: f64) -> u32 {
    (v.clamp(0.0, 1.0) * 4_294_967_296.0).min(4_294_967_295.0) as u32
}

/// Floyd-Rivest selection, as KDBush.
fn select(ids: &mut [u32], coords: &mut [f32], k: usize, mut left: usize, mut right: usize, axis: usize) {
    while right > left {
        if right - left > 600 {
            let n = (right - left + 1) as f64;
            let m = (k - left + 1) as f64;
            let z = n.ln();
            let s = 0.5 * (2.0 * z / 3.0).exp();
            let sd = 0.5 * (z * s * (n - s) / n).sqrt() * if m - n / 2.0 < 0.0 { -1.0 } else { 1.0 };
            let new_left = (left as f64).max((k as f64 - m * s / n + sd).floor()) as usize;
            let new_right = (right as f64).min((k as f64 + (n - m) * s / n + sd).floor()) as usize;
            select(ids, coords, k, new_left, new_right, axis);
        }
        let t = coords[2 * k + axis];
        let mut i = left;
        let mut j = right;
        swap_item(ids, coords, left, k);
        if coords[2 * right + axis] > t {
            swap_item(ids, coords, left, right);
        }
        while i < j {
            swap_item(ids, coords, i, j);
            i += 1;
            j -= 1;
            while coords[2 * i + axis] < t {
                i += 1;
            }
            while coords[2 * j + axis] > t {
                j -= 1;
            }
        }
        if coords[2 * left + axis] == t {
            swap_item(ids, coords, left, j);
        } else {
            j += 1;
            swap_item(ids, coords, j, right);
        }
        if j <= k {
            left = j + 1;
        }
        if k <= j {
            if j == 0 {
                break;
            }
            right = j - 1;
        }
    }
}

fn swap_item(ids: &mut [u32], coords: &mut [f32], i: usize, j: usize) {
    ids.swap(i, j);
    coords.swap(2 * i, 2 * j);
    coords.swap(2 * i + 1, 2 * j + 1);
}

/// Items whose neighbour searches run together, ahead of the greedy walk.
const NEIGHBOUR_BLOCK: usize = 65_536;
/// Below this many items a block searches on one thread. Small under test,
/// so the brute-force comparison runs the threaded search.
const PAR_SEARCH_MIN: usize = if cfg!(test) { 256 } else { 4096 };

/// The sorted neighbour lists of a block of items, searched in parallel:
/// per thread chunk, one flat list and its offsets.
#[derive(Default)]
struct NeighbourBlock {
    chunk: usize,
    parts: Vec<(Vec<u32>, Vec<u32>)>,
}

impl NeighbourBlock {
    /// Every item in `start..end` not yet `done`: its neighbours within `r`
    /// in `level`, ascending. A done item gets an empty list.
    fn search(level: &Level, r: f64, done: &[bool], start: usize, end: usize) -> Self {
        let n = end - start;
        let threads = if threads_allowed() && n >= PAR_SEARCH_MIN {
            std::thread::available_parallelism().map_or(1, |t| t.get()).min(16)
        } else {
            1
        };
        let chunk = n.div_ceil(threads).max(1);
        let work = |from: usize, to: usize| {
            let mut offsets = Vec::with_capacity(to - from + 1);
            let mut flat = Vec::new();
            offsets.push(0u32);
            for (i, &is_done) in done.iter().enumerate().take(to).skip(from) {
                if !is_done {
                    let before = flat.len();
                    level.tree.within(f64::from(level.x[i]), f64::from(level.y[i]), r, &mut flat);
                    flat[before..].sort_unstable();
                }
                offsets.push(flat.len() as u32);
            }
            (offsets, flat)
        };
        let parts = if threads == 1 {
            vec![work(start, end)]
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..threads)
                    .map(|t| {
                        let from = start + t * chunk;
                        let to = (from + chunk).min(end);
                        scope.spawn(move || if from < to { work(from, to) } else { (vec![0], Vec::new()) })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().expect("a neighbour search does not panic")).collect()
            })
        };
        Self { chunk, parts }
    }

    /// The list of the block's `j`th item.
    fn get(&self, j: usize) -> &[u32] {
        let (offsets, flat) = &self.parts[j / self.chunk];
        let local = j % self.chunk;
        &flat[offsets[local] as usize..offsets[local + 1] as usize]
    }
}

/// One zoom level: struct-of-arrays, item `i` in the order it was made.
#[derive(Debug, Default)]
pub(crate) struct Level {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    /// A leaf's index, or a cluster's id.
    pub id: Vec<u32>,
    /// The id of the cluster this item merged into at the next zoom down,
    /// or [`NONE`].
    pub parent: Vec<u32>,
    /// 1 for a leaf.
    pub count: Vec<u32>,
    /// The representative leaf.
    pub rep: Vec<u32>,
    /// The item's row in `ClusterIndex::agg` (clusters with aggregates
    /// only), or [`NONE`].
    pub agg: Vec<u32>,
    pub tree: Kd,
}

impl Level {
    fn len(&self) -> usize {
        self.x.len()
    }

    fn push_from(&mut self, from: &Level, i: usize) {
        self.x.push(from.x[i]);
        self.y.push(from.y[i]);
        self.id.push(from.id[i]);
        self.parent.push(NONE);
        self.count.push(from.count[i]);
        self.rep.push(from.rep[i]);
        self.agg.push(from.agg[i]);
    }

    fn bytes(&self) -> usize {
        (self.x.capacity() + self.y.capacity()) * 4
            + (self.id.capacity() + self.parent.capacity() + self.count.capacity() + self.rep.capacity() + self.agg.capacity())
                * 4
            + self.tree.bytes()
    }
}

/// What a query found: a cluster, or one input point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClusterItem {
    /// A cluster: its id, its weighted centre in lon/lat, how many points it
    /// holds, and its aggregate row (see [`ClusterIndex::aggregates`]).
    Cluster { id: u32, lon_lat: [f64; 2], count: u32, agg: u32 },
    /// Input point `index` (its position in the pushed collection).
    Leaf { index: u32 },
}

/// The index: every level from `min_zoom` to `max_zoom + 1`.
#[derive(Debug)]
pub struct ClusterIndex {
    options: ClusterOptions,
    /// The input point count (the id offset).
    n: u32,
    /// `levels[z]`, `z` in `0..=max_zoom + 1`; `None` for a level dropped
    /// by the non-clustered level of detail.
    levels: Vec<Option<Level>>,
    /// Per cluster with aggregates, `properties.len()` values.
    agg: Vec<f64>,
}

impl ClusterIndex {
    /// Builds the index over `points` (`[lon, lat]`), with `aggregate_inputs`
    /// holding, per `options.properties` entry, each point's value (`NaN`
    /// for none).
    ///
    /// `keep_all`: keep every level (clustering). `false` keeps a level only
    /// when it has at most half the raw count (the level of detail).
    pub fn build(points: &[[f64; 2]], options: &ClusterOptions, aggregate_inputs: &[Vec<f64>], keep_all: bool) -> Self {
        Self::build_timed(points, options, aggregate_inputs, keep_all).0
    }

    /// [`Self::build`], with its wall time per phase.
    pub fn build_timed(
        points: &[[f64; 2]],
        options: &ClusterOptions,
        aggregate_inputs: &[Vec<f64>],
        keep_all: bool,
    ) -> (Self, BuildPhases) {
        Self::build_with(points, options, aggregate_inputs, keep_all, true)
    }

    /// The builder. `presort`: order the raw points along a Z-order curve
    /// first (always, outside the tests that compare the radius mode
    /// against the input-order reference).
    fn build_with(
        points: &[[f64; 2]],
        options: &ClusterOptions,
        aggregate_inputs: &[Vec<f64>],
        keep_all: bool,
        presort: bool,
    ) -> (Self, BuildPhases) {
        assert!(points.len() <= MAX_POINTS, "a point index holds at most {MAX_POINTS} points");
        assert_eq!(aggregate_inputs.len(), options.properties.len(), "one input column per aggregate");
        // Grid and H3 are both roll-ups over a sorted key.
        let grid = options.mode != ClusterMode::Radius;
        let h3_finest = match &options.mode {
            ClusterMode::H3 { resolutions } => Some(resolutions[usize::from(options.max_zoom)]),
            _ => None,
        };
        let mut phases = BuildPhases::default();
        let n = points.len() as u32;
        let top = usize::from(options.max_zoom) + 1;

        // 1. Project, in parallel: f32 Mercator and the Z-order key.
        let started = crate::timing::Instant::now();
        let mut projected: Vec<(u64, u32, f32, f32)> = vec![(0, 0, 0.0, 0.0); points.len()];
        par_chunks(&mut projected, &|first, out| {
            for (k, slot) in out.iter_mut().enumerate() {
                let i = first + k;
                let (x, y) = (lng_x(points[i][0]), lat_y(points[i][1]));
                let key = match h3_finest {
                    Some(res) => h3_cell(points[i], res),
                    None => morton(grid32(x), grid32(y)),
                };
                *slot = (key, i as u32, x as f32, y as f32);
            }
        });
        phases.project_ms = started.elapsed().as_secs_f64() * 1e3;

        // 2. Sort along the curve: every quadtree cell becomes a run.
        let started = crate::timing::Instant::now();
        if presort || grid {
            projected.sort_unstable_by_key(|p| (p.0, p.1));
        }
        phases.sort_ms = started.elapsed().as_secs_f64() * 1e3;

        let mut raw = Level::default();
        let mut keys: Vec<u64> = Vec::with_capacity(if grid { points.len() } else { 0 });
        for &(key, i, x, y) in &projected {
            raw.x.push(x);
            raw.y.push(y);
            raw.id.push(i);
            raw.parent.push(NONE);
            raw.count.push(1);
            raw.rep.push(i);
            raw.agg.push(NONE);
            if grid {
                keys.push(key);
            }
        }
        drop(projected);
        let mut index = Self { options: options.clone(), n, levels: (0..=top).map(|_| None).collect(), agg: Vec::new() };

        // 3. The levels, from the raw points down.
        let started = crate::timing::Instant::now();
        let mut trees_ms = 0.0;
        if !grid {
            let t = crate::timing::Instant::now();
            raw.tree = Kd::build(&raw.x, &raw.y);
            trees_ms += t.elapsed().as_secs_f64() * 1e3;
        }
        let mut finer = raw;
        let raw_count = finer.len();
        let mut kept_finer = true;
        for z in (0..=options.max_zoom).rev() {
            let (coarser, coarser_keys) = if grid {
                index.roll_up(&mut finer, &keys, z, aggregate_inputs)
            } else {
                // Each level's KD-tree is built inside `cluster` (the next
                // level down searches it), so it counts as level time.
                (index.cluster(&mut finer, z, aggregate_inputs), Vec::new())
            };
            let z1 = usize::from(z) + 1;
            // The finer level is final now (its parents are written).
            if kept_finer || z1 == top {
                index.levels[z1] = Some(finer);
            }
            let keep = keep_all || coarser.len() * 2 <= raw_count;
            kept_finer = keep;
            finer = coarser;
            keys = coarser_keys;
        }
        if kept_finer {
            index.levels[0] = Some(finer);
        }
        phases.levels_ms = started.elapsed().as_secs_f64() * 1e3;

        // 4. The grid's KD-trees, one per kept level, in parallel: the
        //    levels are final, so nothing orders them.
        if grid {
            let started = crate::timing::Instant::now();
            let mut kept: Vec<&mut Level> = index.levels.iter_mut().flatten().collect();
            if threads_allowed() {
                std::thread::scope(|scope| {
                    for level in kept.iter_mut() {
                        let level: &mut Level = level;
                        scope.spawn(move || level.tree = Kd::build(&level.x, &level.y));
                    }
                });
            } else {
                for level in kept {
                    level.tree = Kd::build(&level.x, &level.y);
                }
            }
            trees_ms = started.elapsed().as_secs_f64() * 1e3;
        }
        phases.trees_ms = trees_ms;
        (index, phases)
    }

    /// The grid mode's level `zoom` from `finer` (level `zoom + 1`, sorted
    /// along the Z-order curve, `keys[i]` its item `i`'s key): every run of
    /// items in one cell of `2^cell_log2` px merges, when it holds at least
    /// `min_points` points, into a cluster at their count-weighted
    /// centroid. O(items), no neighbour search. Returns the level and its
    /// keys.
    fn roll_up(&mut self, finer: &mut Level, keys: &[u64], zoom: u8, inputs: &[Vec<f64>]) -> (Level, Vec<u64>) {
        let cell_log2 = grid_cell_log2(self.options.radius_px);
        // Grid: cells per axis at this zoom are 2^(zoom + 9 - cell_log2)
        // (512 px tiles). H3: the key's parent at this zoom's resolution.
        let bits = (u32::from(zoom) + 9).saturating_sub(cell_log2).min(32);
        let shift = 2 * (32 - bits);
        let h3_res = match &self.options.mode {
            ClusterMode::H3 { resolutions } => Some(resolutions[usize::from(zoom)]),
            _ => None,
        };
        let cell = |key: u64| match h3_res {
            Some(res) => h3_parent(key, res),
            None if shift >= 64 => 0,
            None => key >> shift,
        };
        let nprops = self.options.properties.len();
        let mut out = Level::default();
        let mut out_keys = Vec::new();
        let mut start = 0;
        while start < finer.len() {
            let c = cell(keys[start]);
            let mut end = start + 1;
            while end < finer.len() && cell(keys[end]) == c {
                end += 1;
            }
            let count: u32 = finer.count[start..end].iter().sum();
            if end - start > 1 && count >= self.options.min_points {
                let id = ((start as u32) << 5) + (u32::from(zoom) + 1) + self.n;
                let (mut wx, mut wy) = (0.0f64, 0.0f64);
                for k in start..end {
                    let w = f64::from(finer.count[k]);
                    wx += f64::from(finer.x[k]) * w;
                    wy += f64::from(finer.y[k]) * w;
                    finer.parent[k] = id;
                }
                // `None` exactly when `nprops == 0`: no properties, no row.
                let agg = match self.agg.len().checked_div(nprops) {
                    Some(row) => {
                        for p in 0..nprops {
                            let mut acc = f64::NAN;
                            for k in start..end {
                                acc = reduce(self.options.properties[p].reduce, acc, self.item_value(finer, k, p, inputs));
                            }
                            self.agg.push(acc);
                        }
                        row as u32
                    }
                    None => NONE,
                };
                let total = f64::from(count);
                out.x.push((wx / total) as f32);
                out.y.push((wy / total) as f32);
                out.id.push(id);
                out.parent.push(NONE);
                out.count.push(count);
                out.rep.push(finer.rep[start]);
                out.agg.push(agg);
                out_keys.push(keys[start]);
            } else {
                for k in start..end {
                    out.push_from(finer, k);
                }
                out_keys.extend_from_slice(&keys[start..end]);
            }
            start = end;
        }
        (out, out_keys)
    }

    /// Supercluster's `_cluster`: level `zoom` from `finer` (level
    /// `zoom + 1`), writing `finer`'s parents.
    fn cluster(&mut self, finer: &mut Level, zoom: u8, inputs: &[Vec<f64>]) -> Level {
        let r = self.options.radius_px / (EXTENT * 2f64.powi(i32::from(zoom)));
        let len = finer.len();
        let mut done = vec![false; len];
        let mut out = Level::default();
        let nprops = self.options.properties.len();
        // The neighbour searches, a block at a time and in parallel, ahead
        // of the greedy walk that consumes them. EXACT, not speculative: a
        // search returns every item within `r` whether or not it is done,
        // and the walk filters by `done` at use time -- precisely what it
        // did when it searched at its own turn. Items already done when a
        // block is searched are skipped (they stay done).
        let mut block = NeighbourBlock::default();
        let mut block_start = 0;
        let mut block_end = 0;
        for i in 0..len {
            if done[i] {
                continue;
            }
            if i >= block_end {
                block_start = i;
                block_end = (i + NEIGHBOUR_BLOCK).min(len);
                block = NeighbourBlock::search(finer, r, &done, block_start, block_end);
            }
            done[i] = true;
            let (x, y) = (f64::from(finer.x[i]), f64::from(finer.y[i]));
            // Ascending, not KD-traversal order (sorted in the search): the
            // order decides where a carried-over group lands in the next
            // level (so which item is an origin there) and the summation
            // order of a centre. Sorted, the result is a function of the
            // input order alone, which is what the brute-force reference in
            // the tests can reproduce.
            let near = block.get(i - block_start);
            let origin_count = finer.count[i];
            let mut count = origin_count;
            for &k in near {
                if !done[k as usize] {
                    count += finer.count[k as usize];
                }
            }
            if count > origin_count && count >= self.options.min_points {
                let mut wx = x * f64::from(origin_count);
                let mut wy = y * f64::from(origin_count);
                let id = ((i as u32) << 5) + (u32::from(zoom) + 1) + self.n;
                // `None` exactly when `nprops == 0`: no properties, no row.
                let agg = match self.agg.len().checked_div(nprops) {
                    Some(row) => {
                        for p in 0..nprops {
                            let v = self.item_value(finer, i, p, inputs);
                            self.agg.push(v);
                        }
                        row as u32
                    }
                    None => NONE,
                };
                for &k in near {
                    let k = k as usize;
                    if done[k] {
                        continue;
                    }
                    done[k] = true;
                    let c = f64::from(finer.count[k]);
                    wx += f64::from(finer.x[k]) * c;
                    wy += f64::from(finer.y[k]) * c;
                    finer.parent[k] = id;
                    if agg != NONE {
                        for p in 0..nprops {
                            let v = self.item_value(finer, k, p, inputs);
                            let at = agg as usize * nprops + p;
                            self.agg[at] = reduce(self.options.properties[p].reduce, self.agg[at], v);
                        }
                    }
                }
                finer.parent[i] = id;
                let total = f64::from(count);
                out.x.push((wx / total) as f32);
                out.y.push((wy / total) as f32);
                out.id.push(id);
                out.parent.push(NONE);
                out.count.push(count);
                out.rep.push(finer.rep[i]);
                out.agg.push(agg);
            } else {
                out.push_from(finer, i);
                if count > 1 {
                    for &k in near {
                        let k = k as usize;
                        if done[k] {
                            continue;
                        }
                        done[k] = true;
                        out.push_from(finer, k);
                    }
                }
            }
        }
        out.tree = Kd::build(&out.x, &out.y);
        out
    }

    /// Item `i` of `level`'s value for aggregate `p`: a leaf's input, or a
    /// cluster's accumulated row.
    fn item_value(&self, level: &Level, i: usize, p: usize, inputs: &[Vec<f64>]) -> f64 {
        if level.count[i] > 1 && level.agg[i] != NONE {
            self.agg[level.agg[i] as usize * self.options.properties.len() + p]
        } else if level.count[i] == 1 {
            inputs[p][level.id[i] as usize]
        } else {
            f64::NAN
        }
    }

    pub fn options(&self) -> &ClusterOptions {
        &self.options
    }

    /// How many points were indexed.
    pub fn len(&self) -> usize {
        self.n as usize
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Resident bytes: every kept level and the aggregate rows.
    pub fn bytes(&self) -> usize {
        self.levels.iter().flatten().map(Level::bytes).sum::<usize>() + self.agg.capacity() * 8
    }

    /// Items per kept level, for reports: `(zoom, items)`.
    pub fn level_sizes(&self) -> Vec<(u8, usize)> {
        self.levels.iter().enumerate().filter_map(|(z, l)| l.as_ref().map(|l| (z as u8, l.len()))).collect()
    }

    /// Supercluster's `_limitZoom`, then the next finer KEPT level.
    pub(crate) fn level_for(&self, zoom: f64) -> (usize, &Level) {
        let top = self.levels.len() - 1;
        let z = (zoom.floor().max(0.0) as usize).min(top);
        (z..=top)
            .find_map(|z| self.levels[z].as_ref().map(|l| (z, l)))
            .expect("the raw level is always kept")
    }

    /// The zoom of the finest (raw, one item per point) level: a query at
    /// or above it reads every point as itself.
    pub(crate) fn raw_zoom(&self) -> u8 {
        u8::try_from(self.levels.len().saturating_sub(1)).unwrap_or(u8::MAX)
    }

    /// Items of the level for `zoom` inside the Mercator box (x, y in
    /// `[0, 1]`, y down), as indices into that level.
    pub(crate) fn range_items(&self, zoom: f64, min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> (usize, Vec<u32>) {
        let (z, level) = self.level_for(zoom);
        let mut out = Vec::new();
        level.tree.range(min_x, min_y, max_x, max_y, &mut out);
        (z, out)
    }

    /// Item `i` of level `z` as a [`ClusterItem`]. In the non-clustered
    /// level of detail, a merged group is its representative leaf.
    pub(crate) fn item(&self, z: usize, i: u32) -> ClusterItem {
        let level = self.levels[z].as_ref().expect("a level returned by level_for");
        let i = i as usize;
        if level.count[i] > 1 && self.options.cluster {
            ClusterItem::Cluster {
                id: level.id[i],
                lon_lat: [x_lng(f64::from(level.x[i])), y_lat(f64::from(level.y[i]))],
                count: level.count[i],
                agg: level.agg[i],
            }
        } else {
            ClusterItem::Leaf { index: level.rep[i] }
        }
    }

    /// Supercluster's `getClusters`, for a lon/lat box `[west, south, east,
    /// north]` (no antimeridian wrap: split the box first).
    pub fn clusters(&self, bbox: [f64; 4], zoom: f64) -> Vec<ClusterItem> {
        let (z, ids) = self.range_items(zoom, lng_x(bbox[0]), lat_y(bbox[3]), lng_x(bbox[2]), lat_y(bbox[1]));
        ids.into_iter().map(|i| self.item(z, i)).collect()
    }

    /// A cluster's aggregate values, in `options.properties` order.
    pub fn aggregates(&self, agg: u32) -> &[f64] {
        let n = self.options.properties.len();
        if agg == NONE || n == 0 {
            return &[];
        }
        &self.agg[agg as usize * n..agg as usize * n + n]
    }

    fn origin(&self, cluster_id: u32) -> Option<(usize, usize)> {
        let rel = cluster_id.checked_sub(self.n)?;
        Some(((rel >> 5) as usize, (rel % 32) as usize))
    }

    /// Supercluster's `getChildren`. `None` for an id that is no cluster.
    pub fn children(&self, cluster_id: u32) -> Option<Vec<ClusterItem>> {
        let (origin, origin_zoom) = self.origin(cluster_id)?;
        let level = self.levels.get(origin_zoom)?.as_ref()?;
        if origin >= level.len() || origin_zoom == 0 {
            return None;
        }
        let mut near = Vec::new();
        if self.options.mode != ClusterMode::Radius {
            // A grid cluster's members are the run that starts at its origin.
            near.extend((origin..level.len()).take_while(|&k| level.parent[k] == cluster_id).map(|k| k as u32));
        } else {
            let r = self.options.radius_px / (EXTENT * 2f64.powi(origin_zoom as i32 - 1));
            level.tree.within(f64::from(level.x[origin]), f64::from(level.y[origin]), r, &mut near);
        }
        let children: Vec<ClusterItem> = near
            .into_iter()
            .filter(|&k| level.parent[k as usize] == cluster_id)
            .map(|k| {
                let k = k as usize;
                if level.count[k] > 1 {
                    ClusterItem::Cluster {
                        id: level.id[k],
                        lon_lat: [x_lng(f64::from(level.x[k])), y_lat(f64::from(level.y[k]))],
                        count: level.count[k],
                        agg: level.agg[k],
                    }
                } else {
                    ClusterItem::Leaf { index: level.id[k] }
                }
            })
            .collect();
        (!children.is_empty()).then_some(children)
    }

    /// Supercluster's `getClusterExpansionZoom`: the zoom at which the
    /// cluster breaks into more than one child.
    pub fn expansion_zoom(&self, cluster_id: u32) -> Option<u8> {
        let (_, origin_zoom) = self.origin(cluster_id)?;
        let mut expansion = origin_zoom as i32 - 1;
        let mut id = cluster_id;
        while expansion <= i32::from(self.options.max_zoom) {
            let children = self.children(id)?;
            expansion += 1;
            match children.as_slice() {
                [ClusterItem::Cluster { id: only, .. }] => id = *only,
                _ => break,
            }
        }
        Some(expansion.max(0) as u8)
    }

    /// Supercluster's `getLeaves`: up to `limit` input point indices, after
    /// skipping `offset`, in the cluster's child order.
    pub fn leaves(&self, cluster_id: u32, limit: usize, offset: usize) -> Option<Vec<u32>> {
        let mut out = Vec::new();
        self.children(cluster_id)?;
        self.append_leaves(&mut out, cluster_id, limit, offset, 0);
        Some(out)
    }

    fn append_leaves(&self, out: &mut Vec<u32>, cluster_id: u32, limit: usize, offset: usize, mut skipped: usize) -> usize {
        let Some(children) = self.children(cluster_id) else { return skipped };
        for child in children {
            match child {
                ClusterItem::Cluster { id, count, .. } => {
                    if skipped + count as usize <= offset {
                        skipped += count as usize;
                    } else {
                        skipped = self.append_leaves(out, id, limit, offset, skipped);
                    }
                }
                ClusterItem::Leaf { index } => {
                    if skipped < offset {
                        skipped += 1;
                    } else {
                        out.push(index);
                    }
                }
            }
            if out.len() == limit {
                break;
            }
        }
        skipped
    }
}

fn reduce(op: ClusterReduce, acc: f64, v: f64) -> f64 {
    if v.is_nan() {
        return acc;
    }
    if acc.is_nan() {
        return v;
    }
    match op {
        ClusterReduce::Sum => acc + v,
        ClusterReduce::Min => acc.min(v),
        ClusterReduce::Max => acc.max(v),
    }
}

/// Supercluster's `point_count_abbreviated`: `"35k"`, `"1.2k"`, or the
/// count itself.
pub fn abbreviate(count: u32) -> String {
    if count >= 10_000 {
        format!("{}k", (f64::from(count) / 1000.0).round())
    } else if count >= 1000 {
        let v = (f64::from(count) / 100.0).round() / 10.0;
        if v.fract() == 0.0 {
            format!("{}k", v as i64)
        } else {
            format!("{v}k")
        }
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seeded splitmix64, so every test run clusters the same points.
    fn rng(seed: u64) -> impl FnMut() -> f64 {
        let mut s = seed;
        move || {
            s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Clumpy points: a few Gaussian-ish cities and a uniform spread.
    fn points(seed: u64, n: usize) -> Vec<[f64; 2]> {
        let mut r = rng(seed);
        let cities: Vec<[f64; 2]> = (0..6).map(|_| [r() * 100.0 - 50.0, r() * 60.0 - 30.0]).collect();
        (0..n)
            .map(|i| {
                if i % 5 == 0 {
                    [r() * 360.0 - 180.0, r() * 160.0 - 80.0]
                } else {
                    let c = cities[i % cities.len()];
                    let s = 0.5 * r() * r();
                    [c[0] + (r() - 0.5) * s * 4.0, c[1] + (r() - 0.5) * s * 2.0]
                }
            })
            .collect()
    }

    /// The reference: the same greedy algorithm, with a brute-force
    /// neighbour scan instead of a KD-tree. Returns per zoom the items as
    /// `(x, y, count, id)`, and per zoom the parents of level `z + 1`.
    fn brute(points: &[[f64; 2]], o: &ClusterOptions) -> Vec<Vec<(f32, f32, u32, u32, u32)>> {
        let n = points.len() as u32;
        let top = usize::from(o.max_zoom) + 1;
        // (x, y, count, id, parent)
        let mut levels: Vec<Vec<(f32, f32, u32, u32, u32)>> = vec![Vec::new(); top + 1];
        let mut cur: Vec<(f32, f32, u32, u32, u32)> = points
            .iter()
            .enumerate()
            .map(|(i, p)| (lng_x(p[0]) as f32, lat_y(p[1]) as f32, 1, i as u32, NONE))
            .collect();
        for z in (0..=o.max_zoom).rev() {
            let r = o.radius_px / (EXTENT * 2f64.powi(i32::from(z)));
            let mut done = vec![false; cur.len()];
            let mut next = Vec::new();
            for i in 0..cur.len() {
                if done[i] {
                    continue;
                }
                done[i] = true;
                let (x, y) = (f64::from(cur[i].0), f64::from(cur[i].1));
                let near: Vec<usize> = (0..cur.len())
                    .filter(|&k| {
                        let (dx, dy) = (f64::from(cur[k].0) - x, f64::from(cur[k].1) - y);
                        dx * dx + dy * dy <= r * r
                    })
                    .collect();
                let mut count = cur[i].2;
                for &k in &near {
                    if !done[k] {
                        count += cur[k].2;
                    }
                }
                if count > cur[i].2 && count >= o.min_points {
                    let id = ((i as u32) << 5) + u32::from(z) + 1 + n;
                    let (mut wx, mut wy) = (x * f64::from(cur[i].2), y * f64::from(cur[i].2));
                    for &k in &near {
                        if done[k] {
                            continue;
                        }
                        done[k] = true;
                        wx += f64::from(cur[k].0) * f64::from(cur[k].2);
                        wy += f64::from(cur[k].1) * f64::from(cur[k].2);
                        cur[k].4 = id;
                    }
                    cur[i].4 = id;
                    next.push(((wx / f64::from(count)) as f32, (wy / f64::from(count)) as f32, count, id, NONE));
                } else {
                    next.push((cur[i].0, cur[i].1, cur[i].2, cur[i].3, NONE));
                    if count > 1 {
                        for &k in &near {
                            if !done[k] {
                                done[k] = true;
                                next.push((cur[k].0, cur[k].1, cur[k].2, cur[k].3, NONE));
                            }
                        }
                    }
                }
            }
            levels[usize::from(z) + 1] = std::mem::take(&mut cur);
            cur = next;
        }
        levels[0] = cur;
        levels
    }

    fn opts(radius: f64, max_zoom: u8) -> ClusterOptions {
        ClusterOptions { radius_px: radius, max_zoom, ..ClusterOptions::default() }
    }

    /// Every level matches the brute-force reference exactly: positions
    /// (to the bit), counts, ids and parents, in order.
    #[test]
    fn the_index_matches_brute_force_greedy_clustering_at_every_zoom() {
        let pts = points(7, 3000);
        let o = opts(40.0, 12);
        let index = ClusterIndex::build_with(&pts, &o, &[], true, false).0;
        let reference = brute(&pts, &o);
        for (z, want) in reference.iter().enumerate() {
            let got = index.levels[z].as_ref().expect("keep_all keeps every level");
            assert_eq!(got.len(), want.len(), "item count at z{z}");
            let a: Vec<(u32, u32, u32, u32, u32)> = (0..got.len())
                .map(|i| (got.x[i].to_bits(), got.y[i].to_bits(), got.count[i], got.id[i], got.parent[i]))
                .collect();
            let b: Vec<(u32, u32, u32, u32, u32)> =
                want.iter().map(|w| (w.0.to_bits(), w.1.to_bits(), w.2, w.3, w.4)).collect();
            assert!(a == b, "level z{z} differs from the reference");
        }
    }

    #[test]
    fn point_count_sums_to_the_input_at_every_zoom() {
        point_count_sums_to_the_input_at_every_zoom_in(ClusterMode::Radius);
        point_count_sums_to_the_input_at_every_zoom_in(ClusterMode::Grid);
        #[cfg(feature = "h3")]
        point_count_sums_to_the_input_at_every_zoom_in(ClusterMode::h3_for(50.0, 14));
    }

    fn point_count_sums_to_the_input_at_every_zoom_in(mode: ClusterMode) {
        let pts = points(11, 20_000);
        let index = ClusterIndex::build(&pts, &ClusterOptions { mode, ..ClusterOptions::default() }, &[], true);
        for (z, level) in index.levels.iter().enumerate() {
            let level = level.as_ref().unwrap();
            let total: u64 = level.count.iter().map(|&c| u64::from(c)).sum();
            assert_eq!(total, pts.len() as u64, "z{z}");
        }
        let world = index.clusters([-180.0, -85.0, 180.0, 85.0], 3.0);
        let total: u64 = world
            .iter()
            .map(|c| match c {
                ClusterItem::Cluster { count, .. } => u64::from(*count),
                ClusterItem::Leaf { .. } => 1,
            })
            .sum();
        let inside = pts.iter().filter(|p| p[1].abs() <= 85.0).count() as u64;
        // A cluster straddling the box edge may pull a point from outside.
        assert!(total.abs_diff(inside) < 50, "{total} vs {inside}");
    }

    #[test]
    fn aggregates_sum_to_the_column_total_and_bound_min_max() {
        aggregates_sum_to_the_column_total_and_bound_min_max_in(ClusterMode::Radius);
        aggregates_sum_to_the_column_total_and_bound_min_max_in(ClusterMode::Grid);
        #[cfg(feature = "h3")]
        aggregates_sum_to_the_column_total_and_bound_min_max_in(ClusterMode::h3_for(50.0, 14));
    }

    fn aggregates_sum_to_the_column_total_and_bound_min_max_in(mode: ClusterMode) {
        let pts = points(3, 5000);
        let values: Vec<f64> = (0..pts.len()).map(|i| if i % 7 == 0 { f64::NAN } else { (i % 13) as f64 }).collect();
        let o = ClusterOptions {
            properties: vec![ClusterProperty::sum("s", "v"), ClusterProperty::max("mx", "v"), ClusterProperty::min("mn", "v")],
            mode,
            ..ClusterOptions::default()
        };
        let index = ClusterIndex::build(&pts, &o, &[values.clone(), values.clone(), values.clone()], true);
        let want: f64 = values.iter().filter(|v| !v.is_nan()).sum();
        let level = index.levels[0].as_ref().unwrap();
        let mut got = 0.0;
        for i in 0..level.len() {
            if level.count[i] > 1 {
                let a = index.aggregates(level.agg[i]);
                if !a[0].is_nan() {
                    got += a[0];
                }
                assert!(a[1] <= 12.0 && a[2] >= 0.0 && a[2] <= a[1]);
            } else if !values[level.id[i] as usize].is_nan() {
                got += values[level.id[i] as usize];
            }
        }
        assert!((got - want).abs() < 1e-6, "{got} vs {want}");
    }

    /// The expansion zoom is the first zoom at which the cluster shows as
    /// more than one item: at `expansion - 1` its points are still one
    /// item, at `expansion` they are several.
    #[test]
    fn the_expansion_zoom_is_where_the_cluster_splits() {
        the_expansion_zoom_is_where_the_cluster_splits_in(ClusterMode::Radius);
        the_expansion_zoom_is_where_the_cluster_splits_in(ClusterMode::Grid);
        #[cfg(feature = "h3")]
        the_expansion_zoom_is_where_the_cluster_splits_in(ClusterMode::h3_for(50.0, 14));
    }

    fn the_expansion_zoom_is_where_the_cluster_splits_in(mode: ClusterMode) {
        let pts = points(5, 8000);
        let index = ClusterIndex::build(&pts, &ClusterOptions { mode, ..ClusterOptions::default() }, &[], true);
        let mut checked = 0;
        for z in 0..=index.options.max_zoom {
            let level = index.levels[usize::from(z)].as_ref().unwrap();
            for i in 0..level.len() {
                if level.count[i] < 2 {
                    continue;
                }
                let id = level.id[i];
                let e = index.expansion_zoom(id).unwrap();
                assert!(e > z, "a cluster at z{z} expands above it, got {e}");
                // Follow the single-child chain to zoom e - 1: still one item.
                let leaves: std::collections::HashSet<u32> =
                    index.leaves(id, usize::MAX, 0).unwrap().into_iter().collect();
                assert_eq!(leaves.len() as u32, level.count[i]);
                if e <= index.options.max_zoom + 1 {
                    let finer = index.levels[usize::from(e)].as_ref().unwrap();
                    // Items at zoom `e` holding any of the leaves: > 1.
                    let holders = (0..finer.len())
                        .filter(|&k| {
                            if finer.count[k] == 1 {
                                leaves.contains(&finer.id[k])
                            } else {
                                index.leaves(finer.id[k], 1, 0).unwrap().iter().any(|l| leaves.contains(l))
                            }
                        })
                        .count();
                    assert!(holders > 1, "cluster {id} from z{z} is still one item at its expansion zoom {e}");
                }
                checked += 1;
                if checked > 300 {
                    return;
                }
            }
        }
        assert!(checked > 20);
    }

    /// Paging `leaves` returns every point of the cluster exactly once.
    #[test]
    fn leaves_round_trip_across_pages() {
        leaves_round_trip_across_pages_in(ClusterMode::Radius);
        leaves_round_trip_across_pages_in(ClusterMode::Grid);
        #[cfg(feature = "h3")]
        leaves_round_trip_across_pages_in(ClusterMode::h3_for(50.0, 14));
    }

    fn leaves_round_trip_across_pages_in(mode: ClusterMode) {
        let pts = points(9, 6000);
        let index = ClusterIndex::build(&pts, &ClusterOptions { mode, ..ClusterOptions::default() }, &[], true);
        let level = index.levels[2].as_ref().unwrap();
        let big = (0..level.len()).max_by_key(|&i| level.count[i]).unwrap();
        let (id, count) = (level.id[big], level.count[big] as usize);
        assert!(count > 100);
        let mut all = Vec::new();
        let mut offset = 0;
        loop {
            let page = index.leaves(id, 37, offset).unwrap();
            if page.is_empty() {
                break;
            }
            offset += page.len();
            all.extend(page);
        }
        assert_eq!(all.len(), count);
        let set: std::collections::HashSet<u32> = all.iter().copied().collect();
        assert_eq!(set.len(), count, "no leaf twice");
        // And paging agrees with one unpaged call.
        let whole: std::collections::HashSet<u32> = index.leaves(id, usize::MAX, 0).unwrap().into_iter().collect();
        assert_eq!(whole, set);
        assert!(index.leaves(3, 10, 0).is_none(), "a leaf index is no cluster");
    }

    /// The level of detail: every level it keeps has at most half the raw
    /// count, and above `max_zoom` every point is its own item.
    #[test]
    fn the_level_of_detail_keeps_only_levels_that_halve_and_is_exact_on_top() {
        let pts = points(13, 30_000);
        let index = ClusterIndex::build(&pts, &ClusterOptions::points_only(), &[], false);
        for (z, n) in index.level_sizes() {
            if usize::from(z) != index.levels.len() - 1 {
                assert!(n * 2 <= pts.len(), "z{z} kept with {n} items");
            }
        }
        let all = index.clusters([-180.0, -85.0, 180.0, 85.0], 30.0);
        let inside = pts.iter().filter(|p| p[1].abs() <= 85.0).count();
        assert_eq!(all.len(), inside);
        assert!(all.iter().all(|c| matches!(c, ClusterItem::Leaf { .. })));
        // A low zoom draws representatives only, fewer than the input.
        let low = index.clusters([-180.0, -85.0, 180.0, 85.0], 2.0);
        assert!(low.len() * 2 <= pts.len());
        assert!(low.iter().all(|c| matches!(c, ClusterItem::Leaf { .. })));
    }

    /// The grid mode against a direct cell count: at every zoom, each cell
    /// of the grid holding two or more points is ONE cluster of exactly
    /// that many points, and each lone point is itself.
    #[test]
    fn the_grid_mode_matches_a_cell_count_reference_at_every_zoom() {
        let pts = points(21, 20_000);
        let o = ClusterOptions { mode: ClusterMode::Grid, ..ClusterOptions::default() };
        let index = ClusterIndex::build(&pts, &o, &[], true);
        let c = grid_cell_log2(o.radius_px);
        for z in 0..=o.max_zoom {
            let bits = (u32::from(z) + 9).saturating_sub(c);
            let mut cells: std::collections::HashMap<(u32, u32), u32> = std::collections::HashMap::new();
            for p in &pts {
                let key = (grid32(lng_x(p[0])) >> (32 - bits), grid32(lat_y(p[1])) >> (32 - bits));
                *cells.entry(key).or_default() += 1;
            }
            let mut want: Vec<u32> = cells.into_values().collect();
            let level = index.levels[usize::from(z)].as_ref().unwrap();
            let mut got = level.count.clone();
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "z{z}");
        }
    }

    /// The H3 mode against a direct count: at every zoom, each H3 cell of
    /// that zoom's resolution (the ancestor of the points' finest cells)
    /// holding two or more points is ONE cluster of exactly that many.
    #[cfg(feature = "h3")]
    #[test]
    fn the_h3_mode_matches_a_cell_count_reference_at_every_zoom() {
        let pts = points(23, 20_000);
        let mode = ClusterMode::h3_for(50.0, 14);
        let ClusterMode::H3 { resolutions } = mode.clone() else { unreachable!() };
        let o = ClusterOptions { mode, ..ClusterOptions::default() };
        o.validate().unwrap();
        let index = ClusterIndex::build(&pts, &o, &[], true);
        for z in 0..=o.max_zoom {
            let mut cells: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
            for p in &pts {
                // The logical hierarchy: the finest cell's ancestor. H3
                // containment is approximate, so near a coarse cell's edge
                // this can differ from the coarse cell holding the point.
                let finest = h3_cell(*p, resolutions[usize::from(o.max_zoom)]);
                *cells.entry(h3_parent(finest, resolutions[usize::from(z)])).or_default() += 1;
            }
            let mut want: Vec<u32> = cells.into_values().collect();
            let mut got = index.levels[usize::from(z)].as_ref().unwrap().count.clone();
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "z{z} (res {})", resolutions[usize::from(z)]);
        }
    }

    #[test]
    fn h3_options_are_refused_without_the_feature_and_checked_with_it() {
        let o = ClusterOptions { mode: ClusterMode::h3_for(50.0, 14), ..ClusterOptions::default() };
        assert_eq!(o.validate().is_ok(), cfg!(feature = "h3"));
        let short = ClusterOptions { mode: ClusterMode::H3 { resolutions: vec![0, 1] }, ..ClusterOptions::default() };
        assert!(short.validate().is_err());
    }

    /// A Z-order presort changes which greedy clustering the radius mode
    /// finds, never what it must satisfy: counts still sum to the input.
    #[test]
    fn the_presorted_radius_build_still_accounts_for_every_point() {
        let pts = points(17, 10_000);
        let (index, phases) = ClusterIndex::build_timed(&pts, &ClusterOptions::default(), &[], true);
        for level in index.levels.iter().flatten() {
            assert_eq!(level.count.iter().map(|&c| u64::from(c)).sum::<u64>(), pts.len() as u64);
        }
        assert!(phases.levels_ms >= 0.0);
    }

    #[test]
    fn abbreviation_matches_supercluster() {
        assert_eq!(abbreviate(999), "999");
        assert_eq!(abbreviate(1000), "1k");
        assert_eq!(abbreviate(1249), "1.2k");
        assert_eq!(abbreviate(9999), "10k");
        assert_eq!(abbreviate(35_400), "35k");
    }

    #[test]
    fn range_and_within_match_a_scan() {
        let mut r = rng(1);
        let xs: Vec<f32> = (0..5000).map(|_| r() as f32).collect();
        let ys: Vec<f32> = (0..5000).map(|_| r() as f32).collect();
        let kd = Kd::build(&xs, &ys);
        let mut got = Vec::new();
        kd.range(0.2, 0.3, 0.5, 0.45, &mut got);
        got.sort_unstable();
        let want: Vec<u32> = (0..5000u32)
            .filter(|&i| {
                let (x, y) = (f64::from(xs[i as usize]), f64::from(ys[i as usize]));
                (0.2..=0.5).contains(&x) && (0.3..=0.45).contains(&y)
            })
            .collect();
        assert_eq!(got, want);
        let mut got = Vec::new();
        kd.within(0.5, 0.5, 0.1, &mut got);
        got.sort_unstable();
        let want: Vec<u32> = (0..5000u32)
            .filter(|&i| {
                let (dx, dy) = (f64::from(xs[i as usize]) - 0.5, f64::from(ys[i as usize]) - 0.5);
                dx * dx + dy * dy <= 0.01
            })
            .collect();
        assert_eq!(got, want);
    }
}
