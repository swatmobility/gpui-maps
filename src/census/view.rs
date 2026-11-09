//! Which block groups the camera shows, and the in-view histogram over
//! them: shared by the native demo's background recount
//! (`demos/us_block_groups.rs`) and the browser's
//! `map_census_histograms_json` (`web/src/app/census_page.rs`), which
//! throttles the same way -- recounting only once the camera has settled
//! since the last count, never walking every block group mid-pan.

use crate::census::hist::{income_bin, Histogram, NO_BIN};
use crate::census::pack::{self, Ring};

/// One block group as the in-view count needs it: its bounding box in
/// Web-Mercator units (`0..1`, y down) and its histogram bar.
#[derive(Clone, Copy, Debug)]
pub struct ViewItem {
    pub bbox: [f32; 4],
    pub bin: u16,
}

/// Web Mercator `0..1`, y down (the engine's world / `WORLD_SIZE`).
pub fn mercator(lon: f64, lat: f64) -> (f64, f64) {
    let lat = lat.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    (lon / 360.0 + 0.5, (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0)
}

/// One feature's `ViewItem` from its income and its rings.
fn view_item_of(income: Option<f64>, rings: &[Ring]) -> ViewItem {
    let (mut south, mut north) = (f64::INFINITY, f64::NEG_INFINITY);
    let extents: Vec<(f64, f64)> = rings
        .iter()
        .map(|ring| {
            let (mut w, mut e) = (f64::INFINITY, f64::NEG_INFINITY);
            for &[lon, lat] in ring {
                (w, e) = (w.min(lon), e.max(lon));
                (south, north) = (south.min(lat), north.max(lat));
            }
            (w, e)
        })
        .collect();
    let (west, east) = lon_extent(&extents);
    // Latitude flips: the north edge is Mercator's smaller y. An extent
    // across the antimeridian keeps x1 > 1.
    let (x0, y0) = mercator(west, north);
    let (x1, y1) = mercator(east, south);
    ViewItem { bbox: [x0 as f32, y0 as f32, x1 as f32, y1 as f32], bin: income.map_or(NO_BIN, |i| income_bin(i) as u16) }
}

/// One `ViewItem` per row, in `rows` order. `rows[i].income` is the ONLY
/// row field this reads; `rings_of(i)` returns that feature's rings (the
/// demo's flat `BlockGroups`, or the web's decoded `pack::StateGeometry`,
/// each feeds its own rings in).
pub fn view_items(rows: &[pack::PackRow], mut rings_of: impl FnMut(usize) -> Vec<Ring>) -> Vec<ViewItem> {
    rows.iter().enumerate().map(|(i, row)| view_item_of(row.income.map(f64::from), &rings_of(i))).collect()
}

/// A block group's longitude extent from its rings' `(west, east)`:
/// plain min/max, unless that is wider than half the world -- TIGER splits
/// a polygon across 180 degrees into parts on both sides (one Aleutian
/// block group spans -179.23..179.86 that way) -- when it is the shortest
/// arc covering the rings, `east` past 180.
pub fn lon_extent(rings: &[(f64, f64)]) -> (f64, f64) {
    let (w, e) = rings.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(w, e), r| (w.min(r.0), e.max(r.1)));
    if e - w <= 180.0 || !(e - w).is_finite() {
        return (w, e);
    }
    let shifted: Vec<(f64, f64)> = rings.iter().map(|&(a, b)| (a + 180.0, b + 180.0)).collect();
    let (a, b) = circular_cover(&shifted, 360.0).expect("some ring");
    (a - 180.0, b - 180.0)
}

/// The shortest arc of a circle of circumference `period` that covers
/// every interval `(a, b)` (`a` in `0..period`, `b >= a`, and `b` may pass
/// `period`: an interval across the seam), as `(start, end)` with `start`
/// in `0..period` and `end >= start` -- `end` past `period` when the arc
/// crosses the seam. The complement of the widest gap between the merged
/// intervals, so a group spanning the antimeridian is the narrow way
/// round.
///
/// Shared with `demos/us_block_groups/breakdowns.rs`'s own, unrelated
/// per-group bounds (`BoundsAcc::bounds`): that module is binary-only and
/// cannot be depended on from this library crate, but it already links
/// `gpui_maps` and imports from `census`, so it calls this one instead of
/// keeping its own copy.
pub fn circular_cover(intervals: &[(f64, f64)], period: f64) -> Option<(f64, f64)> {
    if intervals.is_empty() {
        return None;
    }
    let mut pieces: Vec<(f64, f64)> = Vec::with_capacity(intervals.len() + 4);
    for &(a, b) in intervals.iter() {
        if b - a >= period {
            return Some((0.0, period));
        }
        if b > period {
            pieces.push((a, period));
            pieces.push((0.0, b - period));
        } else {
            pieces.push((a, b));
        }
    }
    pieces.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (a, b) in pieces {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let last = merged.len() - 1;
    let (mut gap, mut at) = (merged[0].0 + period - merged[last].1, last);
    for k in 0..last {
        let g = merged[k + 1].0 - merged[k].1;
        if g > gap {
            gap = g;
            at = k;
        }
    }
    if gap <= 0.0 {
        Some((0.0, period))
    } else if at == last {
        Some((merged[0].0, merged[last].1))
    } else {
        Some((merged[at + 1].0, merged[at].1 + period))
    }
}

/// Which block groups the camera shows, by bounding box -- the engine has
/// no "rendered features" query, so this projects each box itself, through
/// the engine's own matrices:
///
/// - flat (Mercator, any pitch/bearing): the box's four ground corners go
///   through `Camera::view_projection`; the projected quad is convex, so
///   it overlaps the viewport iff no separating axis exists (the square's
///   two, the quad's four). A corner behind the eye falls back to "any
///   corner or the centre on screen".
/// - globe (`render::globe::transition_for_zoom` > 0 and the globe on):
///   nine samples of the box (corners, edge midpoints, centre) blended
///   onto the sphere with `GlobeAnchors` -- the symbol pass's CPU twin of
///   the shader -- and dropped when on the far side of the horizon.
///
/// Plus, either way, a box holding the camera's look-at point is in view
/// (a block group bigger than the screen).
pub fn in_view_indices(items: &[ViewItem], camera: &crate::render::Camera, globe_on: bool) -> Vec<u32> {
    let v = Visibility::new(camera, globe_on);
    (0..items.len() as u32).filter(|&i| v.visible(&items[i as usize])).collect()
}

/// [`in_view_indices`]' per-box test, set up once per camera.
struct Visibility {
    ws: f64,
    vp: glam::DMat4,
    anchors: Option<crate::render::globe::TileAnchors>,
    center: (f64, f64),
}

impl Visibility {
    fn new(camera: &crate::render::Camera, globe_on: bool) -> Self {
        use crate::render::globe::{globe_frame, GlobeAnchors};
        Self {
            ws: f64::from(crate::render::WORLD_SIZE),
            vp: camera.view_projection().as_dmat4(),
            anchors: GlobeAnchors::new(&globe_frame(camera).with_globe_enabled(globe_on))
                .for_tile(crate::geo::TileId { z: 0, x: 0, y: 0 }, 1.0),
            center: mercator(camera.center_lon, camera.center_lat),
        }
    }

    fn clip(&self, x: f64, y: f64) -> Option<glam::DVec4> {
        let flat = self.vp * glam::DVec4::new(x * self.ws, y * self.ws, 0.0, 1.0);
        match &self.anchors {
            None => Some(flat),
            Some(a) => {
                let sphere = a.sphere((x, y));
                (!a.occluded(sphere)).then(|| a.project(flat, sphere))
            }
        }
    }

    fn visible(&self, item: &ViewItem) -> bool {
        use glam::{DVec2, DVec4};
        let on_screen = |c: DVec4| c.w > 0.0 && (c.x / c.w).abs() <= 1.0 && (c.y / c.w).abs() <= 1.0;
        let clip = |x: f64, y: f64| self.clip(x, y);
        let center = self.center;
        let [x0, y0, x1, y1] = item.bbox.map(f64::from);
        if (x0..=x1).contains(&center.0) && (y0..=y1).contains(&center.1) {
            return true;
        }
        if self.anchors.is_some() {
            let (xm, ym) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
            return [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (xm, y0), (x1, ym), (xm, y1), (x0, ym), (xm, ym)]
                .into_iter()
                .any(|(x, y)| clip(x, y).is_some_and(on_screen));
        }
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| clip(x, y).expect("flat"));
        if corners.iter().any(|c| c.w <= 0.0) {
            return corners.iter().any(|&c| on_screen(c))
                || clip((x0 + x1) / 2.0, (y0 + y1) / 2.0).is_some_and(on_screen);
        }
        let q = corners.map(|c| DVec2::new(c.x / c.w, c.y / c.w));
        // The square's axes.
        let (mut lo, mut hi) = (q[0], q[0]);
        for p in &q[1..] {
            lo = lo.min(*p);
            hi = hi.max(*p);
        }
        if lo.x > 1.0 || hi.x < -1.0 || lo.y > 1.0 || hi.y < -1.0 {
            return false;
        }
        // The quad's edge normals.
        let square = [DVec2::new(-1.0, -1.0), DVec2::new(1.0, -1.0), DVec2::new(1.0, 1.0), DVec2::new(-1.0, 1.0)];
        for i in 0..4 {
            let e = q[(i + 1) % 4] - q[i];
            let n = DVec2::new(-e.y, e.x);
            let (qa, qb) = q.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), p| (a.min(n.dot(*p)), b.max(n.dot(*p))));
            let (sa, sb) = square.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), p| (a.min(n.dot(*p)), b.max(n.dot(*p))));
            if qa > sb || qb < sa {
                return false;
            }
        }
        true
    }
}

/// Cells per world side in [`ViewIndex`]'s grid: 256, so a cell is
/// ~1.4 degrees of longitude -- the size of a z9 view, which is where a
/// full walk hurt most.
pub const VIEW_GRID: usize = 256;

/// The in-view count's coarse spatial index, built once as the boxes load
/// ([`ViewIndex::extend`]) so a recount tests only the boxes in cells the
/// camera might see ([`in_view_indices_indexed`]).
///
/// A LOOSE grid over Web Mercator: each box goes in exactly one cell, the
/// one holding its centre, and each cell's bounds grow to the union of its
/// boxes. So a box is never listed twice, and a cell's bounds always
/// contain every point of every box in it -- which is all the camera test
/// needs to be conservative. Coordinates are raw: a box across the
/// antimeridian (Alaska's Aleutians, stored with `x1 > 1`) sits in a cell
/// past `x = 1`, as `in_view_indices` tests it. A box with a non-finite
/// coordinate is kept apart and always tested.
///
/// Memory ([`ViewIndex::footprint_bytes`] counts what is allocated, and
/// [`view_index_bound`] bounds it). Centres are clamped to x in `-1..2`, y
/// in `0..1`, so there are at most `C = 3 * VIEW_GRID^2` = 196,608 cells,
/// and never more than there are boxes `N`. Allocated:
///
/// - the cells: a `Vec<ViewCell>` (56 B each) of capacity < `2C` --
///   262,144 x 56 B = 14.7 MB at the full grid;
/// - the key map: `HashMap<(i32, i32), u32>`, `next_pow2(8C/7)` buckets of
///   12 B + 1 control byte -- 262,144 x 13 B = 3.4 MB;
/// - the lists: a cell's `Vec<u32>` capacity is `max(4, <= 2 x len)`, so
///   <= 16 B a cell + 8 B a box -- 3.1 + 3.2 MB at `MAX_ROWS` boxes.
///
/// Steady: <= ~24.4 MB at `breakdowns::MAX_ROWS` (400,000) boxes over
/// every cell; ~31.7 MB at the peak, while the cells `Vec` doubles (its
/// old buffer, 131,072 x 56 B = 7.3 MB, alive with the new). The real
/// pack's 242,748 US block groups fill 741 cells, 1.46 MB allocated
/// (`the_view_index_memory_is_bounded`, and the real-pack test prints it).
#[derive(Clone, Debug, Default)]
pub struct ViewIndex {
    cells: Vec<ViewCell>,
    at: std::collections::HashMap<(i32, i32), u32>,
    /// Boxes with a non-finite coordinate: always candidates.
    apart: Vec<u32>,
    /// Boxes indexed so far: `items[..len]`.
    len: usize,
}

#[derive(Clone, Debug)]
struct ViewCell {
    /// `[x0, y0, x1, y1]`: the union of its boxes.
    bounds: [f64; 4],
    items: Vec<u32>,
}

impl ViewIndex {
    pub fn build(items: &[ViewItem]) -> Self {
        let mut index = Self::default();
        index.extend(items);
        index
    }

    /// Indexes `items[self.len()..]`: `items` is the whole list so far,
    /// grown since the last call (the pack pushes a state at a time).
    pub fn extend(&mut self, items: &[ViewItem]) {
        let g = VIEW_GRID as f64;
        for (i, item) in items.iter().enumerate().skip(self.len) {
            let b = item.bbox.map(f64::from);
            if !b.iter().all(|v| v.is_finite()) {
                self.apart.push(i as u32);
                continue;
            }
            let cx = ((b[0] + b[2]) / 2.0).clamp(-1.0, 2.0 - 1e-9);
            let cy = ((b[1] + b[3]) / 2.0).clamp(0.0, 1.0 - 1e-9);
            let key = ((cx * g).floor() as i32, (cy * g).floor() as i32);
            let cells = &mut self.cells;
            let at = *self.at.entry(key).or_insert_with(|| {
                cells.push(ViewCell { bounds: b, items: Vec::new() });
                (cells.len() - 1) as u32
            });
            let cell = &mut self.cells[at as usize];
            cell.bounds = [cell.bounds[0].min(b[0]), cell.bounds[1].min(b[1]), cell.bounds[2].max(b[2]), cell.bounds[3].max(b[3])];
            cell.items.push(i as u32);
        }
        self.len = self.len.max(items.len());
    }

    /// Boxes indexed.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Non-empty cells.
    pub fn cells(&self) -> usize {
        self.cells.len()
    }

    /// What the index has allocated: the cells `Vec`'s capacity, the key
    /// map's buckets (control bytes included) and every list's capacity.
    pub fn footprint_bytes(&self) -> usize {
        let lists = self.cells.iter().map(|c| c.items.capacity() * 4).sum::<usize>() + self.apart.capacity() * 4;
        self.cells.capacity() * std::mem::size_of::<ViewCell>() + map_bytes(self.at.capacity()) + lists
    }
}

/// A `HashMap<(i32, i32), u32>` of this `capacity()`'s allocation:
/// hashbrown keeps `capacity = buckets * 7 / 8` (a power of two of
/// buckets), each bucket the entry plus one control byte.
fn map_bytes(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = (capacity * 8).div_ceil(7).next_power_of_two();
    buckets * (std::mem::size_of::<((i32, i32), u32)>() + 1)
}

/// [`ViewIndex::footprint_bytes`]' bound for `boxes` boxes in `cells`
/// cells: `(steady, peak)`, the peak while the cells `Vec` doubles.
pub fn view_index_bound(boxes: usize, cells: usize) -> (usize, usize) {
    let cell = std::mem::size_of::<ViewCell>();
    let cells_cap = cells.next_power_of_two().max(4);
    let steady = cells_cap * cell + map_bytes(cells) + 16 * cells + 8 * boxes;
    (steady, steady + cells_cap / 2 * cell)
}

impl ViewIndex {
}

/// [`in_view_indices`], exactly, testing only the boxes in the cells a
/// conservative camera test ([`CellTest`]) cannot rule out. Falls back to
/// the full walk when the index does not cover `items`.
pub fn in_view_indices_indexed(index: &ViewIndex, items: &[ViewItem], camera: &crate::render::Camera, globe_on: bool) -> Vec<u32> {
    if index.len() != items.len() {
        return in_view_indices(items, camera, globe_on);
    }
    let v = Visibility::new(camera, globe_on);
    let test = CellTest::new(camera, &v);
    let mut candidates: Vec<u32> = index.apart.clone();
    for cell in &index.cells {
        if test.may_see(&cell.bounds) {
            candidates.extend_from_slice(&cell.items);
        }
    }
    // The answer is in index order, as the full walk's. Few candidates
    // (zoomed in): sort them. Many (a national view): a mark per box and
    // one ordered pass is cheaper than sorting most of the boxes.
    if candidates.len() < items.len() / 16 {
        candidates.sort_unstable();
        return candidates.into_iter().filter(|&i| v.visible(&items[i as usize])).collect();
    }
    let mut marked = vec![false; items.len()];
    for &i in &candidates {
        marked[i as usize] = true;
    }
    (0..items.len() as u32).filter(|&i| marked[i as usize] && v.visible(&items[i as usize])).collect()
}

/// A camera's conservative test for a Mercator rectangle: `false` only
/// when no point of it can pass [`Visibility::visible`]'s per-point test.
///
/// Every way a box is visible needs one of its points `p` (a corner, an
/// edge midpoint, the centre, or a point of the projected quad inside the
/// screen) with the clip position `c(p)` on screen: `w > 0`, `|x| <= w`,
/// `|y| <= w` -- four inequalities `L_k(c) = w +- x, w +- y >= 0`, linear in
/// `c` -- and, on the globe, `p` not occluded (`n . g(p) + d >= 0`, `g` its
/// sphere point); or the box holds the camera's look-at point. `c` is
/// affine in `p` on the flat map, affine in `g(p)` on the pure globe, and
/// the shader's blend of the two between (`GlobeAnchors::project`). So a
/// rectangle is ruled out when, for some `k`, the MAXIMUM of `L_k` over it
/// is negative: over the rectangle's corners for the flat part, and over a
/// spherical cap holding `g` of the whole rectangle for the globe part.
/// Each bound is loosened by a small margin, so rounding only ever keeps a
/// cell in.
struct CellTest {
    center: (f64, f64),
    /// `L_k` of the flat clip as `a x + b y + c` over Mercator `(x, y)`.
    flat: [[f64; 3]; 4],
    /// On the globe: `(t, L_k of the globe clip as a . g + b, occlusion
    /// a . g + b)`; `t` the blend, 1 for the pure globe.
    globe: Option<(f64, [(glam::DVec3, f64); 4], (glam::DVec3, f64))>,
}

impl CellTest {
    fn new(camera: &crate::render::Camera, v: &Visibility) -> Self {
        use crate::render::globe::globe_frame;
        let rows = |m: &glam::DMat4| [m.row(0), m.row(1), m.row(3)];
        let [rx, ry, rw] = rows(&v.vp);
        let lin = [rw + rx, rw - rx, rw + ry, rw - ry];
        let flat = lin.map(|r| [r.x * v.ws, r.y * v.ws, r.w]);
        let globe = v.anchors.as_ref().map(|_| {
            // The same frame `Visibility::new` built its anchors from.
            let frame = globe_frame(camera).with_globe_enabled(true);
            let t = f64::from(frame.transition);
            let t = if t > 0.999 { 1.0 } else { t };
            let [gx, gy, gw] = rows(&frame.globe_view_proj);
            let glin = [gw + gx, gw - gx, gw + gy, gw - gy].map(|r| (r.truncate(), r.w));
            let p = glam::DVec4::from_array(frame.clipping_plane);
            (t, glin, (p.truncate(), p.w))
        });
        Self { center: v.center, flat, globe }
    }

    fn may_see(&self, b: &[f64; 4]) -> bool {
        let [x0, y0, x1, y1] = *b;
        if (x0..=x1).contains(&self.center.0) && (y0..=y1).contains(&self.center.1) {
            return true;
        }
        let rect_max = |[a, bb, c]: [f64; 3]| {
            let v = (a * x0).max(a * x1) + (bb * y0).max(bb * y1) + c;
            (v, (a.abs() + bb.abs()) * 2.0 + c.abs())
        };
        match &self.globe {
            None => self.flat.iter().all(|&l| {
                let (m, scale) = rect_max(l);
                !(m < -1e-9 * scale)
            }),
            Some((t, glin, occl)) => {
                let cap = Cap::of_rect(b);
                let (om, oscale) = cap.max(occl.0, occl.1);
                if om < -1e-9 * oscale {
                    return false;
                }
                self.flat.iter().zip(glin).all(|(&l, &(ga, gb))| {
                    let (gm, gscale) = cap.max(ga, gb);
                    let (m, scale) = if *t >= 1.0 {
                        (gm, gscale)
                    } else {
                        let (fm, fscale) = rect_max(l);
                        ((1.0 - t) * fm + t * gm, fscale + gscale)
                    };
                    !(m < -1e-9 * scale)
                })
            }
        }
    }
}

/// A spherical cap -- unit centre `c`, angular radius `rho` -- holding
/// the sphere points (`globe::mercator_to_sphere`) of a Mercator rectangle.
struct Cap {
    c: glam::DVec3,
    cos_rho: f64,
    sin_rho: f64,
    whole: bool,
}

impl Cap {
    /// The rectangle's spherical longitudes `s` (`2 pi x + pi`) and
    /// latitudes `phi`; the centre at their midpoints. Any point is within
    /// `dphi / 2` of the centre's meridian point at its own latitude, and
    /// that within `ds / 2 * max cos(phi)` along the parallel (an arc is no
    /// shorter than the great circle), so `rho = dphi / 2 + ds / 2 *
    /// max cos(phi)` holds the whole rectangle (the triangle inequality),
    /// plus a margin.
    fn of_rect(b: &[f64; 4]) -> Self {
        use std::f64::consts::PI;
        let lat = |y: f64| {
            let (s, c) = crate::render::globe::mercator_y_to_sin_cos_lat(y);
            s.atan2(c)
        };
        let (s0, s1) = (2.0 * PI * b[0] + PI, 2.0 * PI * b[2] + PI);
        let (north, south) = (lat(b[1]), lat(b[3]));
        let cos_max = if south <= 0.0 && north >= 0.0 { 1.0 } else { south.cos().max(north.cos()) };
        let rho = (north - south) / 2.0 + (s1 - s0) / 2.0 * cos_max + 1e-9;
        if !rho.is_finite() || rho >= PI {
            return Self { c: glam::DVec3::Z, cos_rho: -1.0, sin_rho: 0.0, whole: true };
        }
        let (sc, pc) = ((s0 + s1) / 2.0, (north + south) / 2.0);
        let c = glam::DVec3::new(sc.sin() * pc.cos(), pc.sin(), sc.cos() * pc.cos());
        Self { c, cos_rho: rho.cos(), sin_rho: rho.sin(), whole: false }
    }

    /// The maximum of `a . g + b` over the cap's unit points, and the
    /// scale its rounding margin is taken against.
    fn max(&self, a: glam::DVec3, b: f64) -> (f64, f64) {
        let len = a.length();
        let scale = len + b.abs();
        if self.whole || len == 0.0 {
            return (len + b, scale);
        }
        let cos_t = (a.dot(self.c) / len).clamp(-1.0, 1.0);
        if cos_t >= self.cos_rho {
            return (len + b, scale);
        }
        let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
        (len * (cos_t * self.cos_rho + sin_t * self.sin_rho) + b, scale)
    }
}

/// The histogram over [`in_view_indices`]'s result: what a settled camera
/// counts, native and web alike.
pub fn in_view_histogram(items: &[ViewItem], camera: &crate::render::Camera, globe_on: bool) -> Histogram {
    Histogram::of_bins(in_view_indices(items, camera, globe_on).into_iter().map(|i| items[i as usize].bin))
}

/// Whether `map_census_histograms_json` must recount the in-view
/// histogram: the camera is not moving right now, AND `now` -- the
/// current `(camera, block groups pushed so far)` -- differs from `last`,
/// the key it counted at last time.
///
/// Pulled out of `web/src/app/census_page.rs` (which has no running
/// tests -- see `project_web_shell_cannot_be_tested`) so the cache
/// semantics that bit this task once are pinned somewhere a plain `cargo
/// test` can catch a regression:
///
/// - keying `now` on `CameraController::input_generation` instead of the
///   camera's own value would never recount after a programmatic
///   `map_jump_to` (`?camera=`, the perf harness) landed on the same pose
///   twice in a row, since that call never bumps the generation;
/// - dropping the item count from the key would cache whatever the very
///   first settle counted (which can land before every state has been
///   pushed -- the pack streams one state per frame) and never recount
///   again at an unmoved camera, even as `view_items` keeps growing.
///
/// `in_view_recounts_when_the_pack_keeps_streaming_even_at_an_unmoved_camera`
/// below fails on the item-count regression directly, and the
/// generation one by construction -- this function only ever sees a
/// `Camera` value, never a generation counter.
pub fn in_view_needs_recount(moving: bool, last: Option<(crate::render::Camera, usize)>, now: (crate::render::Camera, usize)) -> bool {
    !moving && last != Some(now)
}

/// Every row's box for [`crate::census::breakdowns::Breakdowns::set_bounds`],
/// in row order, from `items` pushed in any order (`rows[i]` is
/// `items[i]`'s row). A row not pushed yet -- the pack still loading, or a
/// load that stopped part-way -- is NaN, which `set_bounds` skips, so the
/// groups that did arrive still get their bounds.
pub fn row_boxes(items: &[ViewItem], rows: &[u32], n_rows: usize) -> Vec<[f32; 4]> {
    let mut boxes = vec![[f32::NAN; 4]; n_rows];
    for (item, &row) in items.iter().zip(rows) {
        if let Some(b) = boxes.get_mut(row as usize) {
            *b = item.bbox;
        }
    }
    boxes
}

#[cfg(test)]
pub fn view_items_from_rings(items: &[(Option<f64>, Vec<Ring>)]) -> Vec<ViewItem> {
    items.iter().map(|(income, rings)| view_item_of(*income, rings)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::census::hist::hist_bins;

    fn camera(lon: f64, lat: f64, zoom: f64, pitch_deg: f32) -> crate::render::Camera {
        crate::render::Camera {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch: pitch_deg.to_radians(),
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    /// A box `dx`,`dy` Mercator units from the centre, `w` x `h` large.
    fn item(c: (f64, f64), dx: f64, dy: f64, w: f64, h: f64, bin: u16) -> ViewItem {
        let (x, y) = (c.0 + dx, c.1 + dy);
        ViewItem { bbox: [(x - w / 2.0) as f32, (y - h / 2.0) as f32, (x + w / 2.0) as f32, (y + h / 2.0) as f32], bin }
    }

    /// Record 207 of `tl_2023_02_bg.shp`: parts at 172.4..179.86 E and
    /// -179.23..-175 W. Its extent runs 172.4 E eastward to 185 (= 175 W),
    /// not round the world; an ordinary block group is untouched.
    #[test]
    fn a_block_group_across_the_antimeridian_keeps_a_narrow_box() {
        let (w, e) = lon_extent(&[(172.4, 179.86), (-179.23, -175.0)]);
        assert!((w - 172.4).abs() < 1e-9 && (e - 185.0).abs() < 1e-9, "{w} {e}");
        assert_eq!(lon_extent(&[(-118.5, -118.2), (-118.4, -118.0)]), (-118.5, -118.0));
    }

    #[test]
    fn in_view_counts_a_known_flat_viewport() {
        // z10, 800 x 600: the screen is ~0.0015 x 0.0011 Mercator units.
        let cam = camera(-90.0, 40.0, 10.0, 0.0);
        let c = mercator(-90.0, 40.0);
        let items = [
            item(c, 0.0, 0.0, 1e-5, 1e-5, 1),       // centre: in
            item(c, 0.0005, 0.0003, 1e-5, 1e-5, 2), // inside, off-centre: in
            item(c, 0.01, 0.0, 1e-5, 1e-5, 3),      // far east: out
            item(c, 0.0, -0.01, 1e-5, 1e-5, 4),     // far north: out
            // Wide and thin, crossing the screen above the centre: every
            // corner is off screen, yet it is in view.
            item(c, 0.0, 0.0002, 0.02, 1e-5, 5),
            // Bigger than the screen: in.
            item(c, 0.0, 0.0, 1.0, 1.0, 6),
            item(c, 0.0, 0.0, 1e-5, 1e-5, NO_BIN), // no data: counted apart
        ];
        let h = in_view_histogram(&items, &cam, false);
        let in_bins: Vec<usize> = (0..hist_bins()).filter(|&k| h.counts[k] > 0).collect();
        assert_eq!(in_bins, vec![1, 2, 5, 6]);
        assert_eq!((h.total, h.no_data), (4, 1));
        // Where the projection is flat, the globe switch changes nothing.
        if crate::render::globe::transition_for_zoom(10.0) == 0.0 {
            assert_eq!(in_view_histogram(&items, &cam, true), h);
        }
    }

    #[test]
    fn a_pitched_camera_sees_ahead_not_behind() {
        let cam = camera(-90.0, 40.0, 12.0, 60.0);
        let c = mercator(-90.0, 40.0);
        let items = [
            item(c, 0.0, 0.0, 1e-6, 1e-6, 1),     // the look-at point
            item(c, 0.0, -0.0006, 1e-6, 1e-6, 2), // ahead (north), towards the horizon: in
            item(c, 0.0, 0.003, 1e-6, 1e-6, 3),   // far behind the eye: out
        ];
        let h = in_view_histogram(&items, &cam, false);
        assert_eq!((h.counts[1], h.counts[2], h.counts[3]), (1, 1, 0));
    }

    #[test]
    fn the_globe_hides_the_far_side() {
        assert_eq!(crate::render::globe::transition_for_zoom(2.0), 1.0);
        let cam = camera(0.0, 0.0, 2.0, 0.0);
        let near = mercator(20.0, 10.0);
        let far = mercator(170.0, 0.0);
        let items = [item(near, 0.0, 0.0, 1e-4, 1e-4, 1), item(far, 0.0, 0.0, 1e-4, 1e-4, 2)];
        let h = in_view_histogram(&items, &cam, true);
        assert_eq!((h.counts[1], h.counts[2]), (1, 0));
    }

    /// A one-ring block group spanning the antimeridian, at a camera
    /// centred on it: in view exactly once, not twice (there is only one
    /// `ViewItem` -- the antimeridian risk is `lon_extent` picking the
    /// wrong (wide) box and the item never matching the centre test, not
    /// double-counting).
    #[test]
    fn in_view_at_the_antimeridian_counts_each_block_group_once() {
        let rings = vec![vec![[179.9, 52.0], [-179.9, 52.0], [-179.9, 52.1], [179.9, 52.1], [179.9, 52.0]]];
        let items = view_items_from_rings(&[(Some(50_000.0), rings)]);
        let cam = crate::render::Camera {
            center_lon: 180.0,
            center_lat: 52.05,
            zoom: 8.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1280.0,
            viewport_height_px: 800.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        assert_eq!(in_view_indices(&items, &cam, false), vec![0]);
        let h = in_view_histogram(&items, &cam, false);
        assert_eq!(h.counts.iter().sum::<u32>(), 1);
    }

    /// Pins `in_view_needs_recount`'s cache key against the exact bug this
    /// task found and fixed: a settled camera that is still loading more
    /// block groups (the pack streams one state per frame) must recount
    /// even though the camera itself has not moved an inch. A regression
    /// back to a camera-only key (or to `CameraController::input_generation`,
    /// which is not even a value this function can see) would make the
    /// "count grew, camera didn't move" case below wrongly say "no need to
    /// recount" -- exactly what shipped, briefly, as 24,027-of-242,748 at
    /// the national camera instead of ~223,000.
    #[test]
    fn in_view_recounts_when_the_pack_keeps_streaming_even_at_an_unmoved_camera() {
        let c1 = camera(-98.35, 39.5, 3.7, 0.0);
        let c2 = camera(-73.98, 40.75, 10.0, 0.0);

        // Nothing changed: no recount.
        assert!(!in_view_needs_recount(false, Some((c1, 100)), (c1, 100)));
        // Moving: never recounts, whatever else changed.
        assert!(!in_view_needs_recount(true, Some((c1, 100)), (c1, 200)));
        assert!(!in_view_needs_recount(true, Some((c1, 100)), (c2, 100)));
        // The camera sits still while the pack keeps pushing states: the
        // item count alone changed. This is the case a key that dropped
        // the count (or that only ever compared generations, which never
        // move for a settle with no gesture) would miss.
        assert!(in_view_needs_recount(false, Some((c1, 100)), (c1, 200)));
        // The camera moved, item count steady: still recounts.
        assert!(in_view_needs_recount(false, Some((c1, 100)), (c2, 100)));
        // Never counted yet: recounts.
        assert!(in_view_needs_recount(false, None, (c1, 100)));
    }

    /// A tiny xorshift: no `rand` dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Boxes over the whole world, denser over the US, with the awkward
    /// ones: across the antimeridian (`x1 > 1`), near both poles, huge,
    /// degenerate, and non-finite.
    fn synthetic_items(n: usize) -> Vec<ViewItem> {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut items = Vec::with_capacity(n + 8);
        let hubs = [(-118.25, 34.05), (-73.98, 40.75), (-98.35, 39.5), (-150.0, 61.2), (179.5, 52.0), (-157.8, 21.3), (0.0, 83.0), (20.0, -70.0)];
        for i in 0..n {
            let (x, y) = if i % 3 == 0 {
                (rng.unit(), 0.02 + 0.96 * rng.unit())
            } else {
                let (lon, lat) = hubs[i % hubs.len()];
                let spread = [0.02, 0.2, 2.0, 8.0][(rng.next() % 4) as usize];
                mercator(lon + (rng.unit() - 0.5) * spread, (lat + (rng.unit() - 0.5) * spread).clamp(-84.0, 84.0))
            };
            let size = [1e-7, 1e-5, 1e-4, 1e-3, 1e-2][(rng.next() % 5) as usize] * (0.2 + rng.unit());
            let (w, h) = (size * (0.3 + rng.unit()), size * (0.3 + rng.unit()));
            items.push(ViewItem { bbox: [x as f32, y as f32, (x + w) as f32, (y + h) as f32], bin: (i % 80) as u16 });
        }
        // The Aleutians' antimeridian box, stored the narrow way (x1 > 1).
        let (a0, ay) = mercator(172.4, 52.2);
        let (a1, ay1) = mercator(185.0 - 360.0, 51.5);
        items.push(ViewItem { bbox: [a0 as f32, ay as f32, (a1 + 1.0) as f32, ay1 as f32], bin: 1 });
        items.push(ViewItem { bbox: [0.999, 0.29, 1.004, 0.30], bin: 2 });
        // Near the poles, huge, a point, and garbage.
        items.push(ViewItem { bbox: [0.4, 0.0005, 0.6, 0.004], bin: 3 });
        items.push(ViewItem { bbox: [0.1, 0.1, 0.9, 0.9], bin: 4 });
        items.push(ViewItem { bbox: [0.3, 0.4, 0.3, 0.4], bin: 5 });
        items.push(ViewItem { bbox: [f32::NAN, 0.4, 0.31, 0.41], bin: 6 });
        items
    }

    fn cameras() -> Vec<crate::render::Camera> {
        let centers = [
            (-98.35, 39.5),  // national
            (-118.25, 34.05), // LA
            (-73.98, 40.75), // NYC
            (180.0, 52.0),   // the antimeridian, Aleutians
            (-150.0, 61.2),  // Alaska
            (0.0, 83.0),     // by the north pole
            (-157.8, 21.3),  // Honolulu
            (20.0, -70.0),   // the south
        ];
        let mut out = Vec::new();
        for (k, &(lon, lat)) in centers.iter().enumerate() {
            for (j, zoom) in [3.0, 4.2, 5.5, 7.0, 9.0, 10.3, 11.0, 11.4, 11.8, 12.0, 13.0].into_iter().enumerate() {
                let (pitch, bearing) = [(0.0f32, 0.0f32), (45.0, 30.0), (60.0, -75.0)][(j + k) % 3];
                out.push(crate::render::Camera {
                    center_lon: lon,
                    center_lat: lat,
                    zoom,
                    pitch: pitch.to_radians(),
                    bearing: bearing.to_radians(),
                    viewport_width_px: 1280.0,
                    viewport_height_px: 800.0,
                    fov_y: crate::render::Camera::DEFAULT_FOV_Y,
                });
            }
        }
        out
    }

    /// The prefiltered walk returns EXACTLY the full walk's set, flat and
    /// globe, zooms 3..13 (through the 11..12 blend), pitched and turned,
    /// at the national view, LA, NYC, the antimeridian, Alaska and near a
    /// pole -- and it actually prunes at the zoomed-in cameras.
    #[test]
    fn the_indexed_walk_is_exactly_the_full_walk() {
        let items = synthetic_items(6_000);
        let index = ViewIndex::build(&items);
        assert_eq!(index.len(), items.len());
        let mut pruned = 0;
        let mut seen_any = 0;
        for cam in cameras() {
            for globe in [true, false] {
                let full = in_view_indices(&items, &cam, globe);
                let fast = in_view_indices_indexed(&index, &items, &cam, globe);
                assert_eq!(fast, full, "z{} at {},{} pitch {} globe {globe}", cam.zoom, cam.center_lon, cam.center_lat, cam.pitch);
                seen_any += usize::from(!full.is_empty());
                let v = Visibility::new(&cam, globe);
                let test = CellTest::new(&cam, &v);
                if index.cells.iter().any(|c| !test.may_see(&c.bounds)) {
                    pruned += 1;
                }
            }
        }
        assert!(seen_any > 100, "the sweep sees things: {seen_any}");
        assert!(pruned > 100, "the index rules cells out: {pruned}");
    }

    /// Streaming: an index extended a state at a time equals one built at
    /// once, and a stale index falls back to the full walk.
    #[test]
    fn an_index_grown_in_pieces_answers_the_same() {
        let items = synthetic_items(2_000);
        let mut grown = ViewIndex::default();
        for end in [0, 500, 501, 1_700, items.len()] {
            grown.extend(&items[..end]);
        }
        let cam = cameras()[14];
        let full = in_view_indices(&items, &cam, true);
        assert_eq!(in_view_indices_indexed(&grown, &items, &cam, true), full);
        let stale = ViewIndex::build(&items[..100]);
        assert_eq!(in_view_indices_indexed(&stale, &items, &cam, true), full);
    }

    #[test]
    fn the_view_index_memory_is_bounded() {
        // The worst case the doc's arithmetic covers: every cell used.
        let g = VIEW_GRID;
        let mut items = Vec::new();
        for cx in 0..3 * g {
            for cy in 0..g {
                let (x, y) = (-1.0 + (cx as f64 + 0.5) / g as f64, (cy as f64 + 0.5) / g as f64);
                items.push(ViewItem { bbox: [x as f32, y as f32, x as f32, y as f32], bin: 0 });
            }
        }
        let index = ViewIndex::build(&items);
        assert_eq!(index.cells(), 3 * g * g, "every clamped cell, once");
        let (steady, _) = view_index_bound(items.len(), index.cells());
        // Measured, by allocation: capacity x size, the map's buckets.
        assert!(index.footprint_bytes() <= steady, "{} > {steady}", index.footprint_bytes());
        assert!(index.footprint_bytes() > 20_000_000, "the full grid really is ~24 MB: {}", index.footprint_bytes());
        // Centres far outside the world clamp into the same bounded grid.
        let wild = [ViewItem { bbox: [-50.0, -9.0, -49.0, -8.0], bin: 0 }, ViewItem { bbox: [70.0, 9.0, 71.0, 10.0], bin: 0 }];
        let mut more = items.clone();
        more.extend(wild);
        assert_eq!(ViewIndex::build(&more).cells(), 3 * g * g);
        // 400,000 rows (breakdowns::MAX_ROWS) over every cell: the doc's
        // ~24.4 MB steady and ~31.7 MB peak.
        let (steady, peak) = view_index_bound(crate::census::breakdowns::MAX_ROWS, 3 * g * g);
        assert!(steady <= 24_500_000 && peak <= 31_800_000, "{steady} {peak}");
        // A few boxes: a small index.
        let small = ViewIndex::build(&items[..1_000]);
        assert!(small.footprint_bytes() <= view_index_bound(1_000, small.cells()).0);
    }

    /// The real pack, when the local cache has it: every state's boxes,
    /// the indexed walk against the full one at the gate's cameras and the
    /// sweep. `cargo test --lib census::view -- --ignored`.
    #[test]
    #[ignore = "needs ~/.cache/gpui-map-data/census-web/us_bg_2023.bgpk"]
    fn the_indexed_walk_is_exact_on_the_real_pack() {
        let path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/gpui-map-data/census-web/us_bg_2023.bgpk");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("no pack at {}: skipped", path.display());
            return;
        };
        let header = pack::parse_header(&bytes).expect("header");
        let tables_end = (header.tables_offset + u64::from(header.tables_len)) as usize;
        let tables = pack::decode_tables(&header, &bytes[header.tables_offset as usize..tables_end]).expect("tables");
        let mut items = Vec::new();
        for row in &header.states {
            let body = &bytes[row.offset as usize..row.offset as usize + row.len as usize];
            let geo = pack::decode_state(row, body).expect("state");
            let rows = &tables.rows[row.first_feature as usize..(row.first_feature + row.feature_count) as usize];
            items.extend(view_items(rows, |i| {
                let (fs, fe) = geo.features[i];
                (fs..fe)
                    .flat_map(|p| {
                        let (rs, re) = geo.polygons[p as usize];
                        (rs..re).map(|r| {
                            let (s, e) = geo.rings[r as usize];
                            geo.coords[s as usize..e as usize].to_vec()
                        })
                    })
                    .collect()
            }));
        }
        assert!(items.len() > 200_000, "{}", items.len());
        let index = ViewIndex::build(&items);
        eprintln!("{} boxes in {} cells, {} bytes", items.len(), index.cells(), index.footprint_bytes());
        for cam in cameras() {
            for globe in [true, false] {
                let t = crate::timing::Instant::now();
                let full = in_view_indices(&items, &cam, globe);
                let tf = t.elapsed();
                let t = crate::timing::Instant::now();
                let fast = in_view_indices_indexed(&index, &items, &cam, globe);
                let ti = t.elapsed();
                assert_eq!(fast.len(), full.len(), "z{} at {},{} globe {globe}", cam.zoom, cam.center_lon, cam.center_lat);
                assert!(fast == full, "z{} at {},{} globe {globe}", cam.zoom, cam.center_lon, cam.center_lat);
                if globe && [9.0, 11.0, 3.0].contains(&cam.zoom) {
                    eprintln!("z{} {},{}: {} in view, full {:?}, indexed {:?}", cam.zoom, cam.center_lon, cam.center_lat, full.len(), tf, ti);
                }
            }
        }
    }

    /// `Cap::of_rect` holds EVERY point of its rectangle (not only the
    /// ones the synthetic boxes happen to land on), and `Cap::max` bounds
    /// `a . g + b` at each: rectangles across the equator, near the poles,
    /// wide in longitude and past `x = 1`.
    #[test]
    fn a_cap_holds_every_point_of_its_rectangle() {
        use crate::render::globe::mercator_to_sphere;
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut rects = vec![
            [0.30, 0.45, 0.32, 0.55],     // across the equator
            [0.10, 0.001, 0.30, 0.02],    // near the north pole, wide
            [0.50, 0.97, 0.70, 0.999],    // near the south pole, wide
            [0.99, 0.29, 1.03, 0.31],     // past x = 1 (the Aleutians)
            [0.0, 0.2, 0.45, 0.3],        // wide in s
            [0.25, 0.49, 0.25, 0.51],     // zero width
            [0.1, 0.1, 0.9, 0.9],         // most of the world
        ];
        for _ in 0..300 {
            let (x, y) = (rng.unit() * 1.2 - 0.1, rng.unit() * 0.98 + 0.01);
            let (w, h) = (rng.unit().powi(3) * 0.4, rng.unit().powi(3) * 0.3);
            rects.push([x, y, x + w, (y + h).min(0.999)]);
        }
        for r in rects {
            let cap = Cap::of_rect(&r);
            let planes: Vec<(glam::DVec3, f64)> = (0..6)
                .map(|_| (glam::DVec3::new(rng.unit() - 0.5, rng.unit() - 0.5, rng.unit() - 0.5) * 4.0, rng.unit() - 0.5))
                .collect();
            let maxes: Vec<f64> = planes.iter().map(|&(a, b)| cap.max(a, b).0).collect();
            let n = 40;
            for i in 0..=n {
                for j in 0..=n {
                    let x = r[0] + (r[2] - r[0]) * i as f64 / n as f64;
                    let y = r[1] + (r[3] - r[1]) * j as f64 / n as f64;
                    let g = mercator_to_sphere(x, y);
                    if !cap.whole {
                        assert!(g.dot(cap.c) >= cap.cos_rho - 1e-12, "{r:?}: ({x}, {y}) outside the cap");
                    }
                    for (&(a, b), &m) in planes.iter().zip(&maxes) {
                        assert!(a.dot(g) + b <= m + 1e-9, "{r:?}: max {m} < {} at ({x}, {y})", a.dot(g) + b);
                    }
                }
            }
        }
    }
}
