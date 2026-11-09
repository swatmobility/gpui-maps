//! Internal geojson-vt-style tiling of a runtime overlay source.
//!
//! The algorithm is geojson-vt 4.0.2's (`index.js`, `tile.js`, `convert.js`;
//! ISC, Copyright (c) 2015, Mapbox), with three deliberate deviations, the
//! first two for the spec's 3 GB RSS target:
//! 1. ONE geometry copy (u32 fixed-point Mercator + a u8 minimum zoom per
//!    vertex, 9 B/vertex) instead of f64 triples plus a clipped copy per
//!    index level. Index tiles hold feature REFERENCES.
//! 2. `cut` filters by minimum zoom first and clips second. The kept vertex
//!    set is identical to geojson-vt's (importance is per source vertex).
//! 3. A polygon too small to keep at a zoom is not just dropped: its area
//!    accumulates into tolerance-sized squares, tippecanoe's default
//!    tiny-polygon reduction (`TinyArea`), so dense areas keep their fill.
//!
//! geojson-vt is distributed under the ISC License:
//!
//! > Copyright (c) 2015, Mapbox
//! >
//! > Permission to use, copy, modify, and/or distribute this software for any
//! > purpose with or without fee is hereby granted, provided that the above
//! > copyright notice and this permission notice appear in all copies.
//! >
//! > THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//! > WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//! > MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//! > ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//! > WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//! > ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//! > OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

pub(crate) mod clip;
pub(crate) mod simplify;
mod zeroed;

use crate::geo::TileId;
use crate::overlay::geojson::OverlayGeometry;
use crate::overlay::OverlayFeature;
use std::collections::HashMap;
use std::sync::Arc;
use zeroed::ZeroedBuf;

/// The class a feature that must not be tiled carries: `build` skips it.
pub const NO_CLASS: u16 = u16::MAX;
const FIXED: f64 = 4_294_967_296.0; // 2^32 per world
/// `render::WORLD_SIZE` (512.0, f32) as f64: the world square `lon_lat_to_world` returns.
pub(crate) const WORLD: f64 = crate::render::WORLD_SIZE as f64;

/// geojson-vt's options, in its own units: tile-local MVT units for
/// `tolerance` and `buffer`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileIndexOptions {
    pub extent: u32,
    pub tolerance: f64,
    pub buffer: f64,
    pub max_zoom: u8,
    pub index_max_zoom: u8,
    pub index_max_points: u64,
    /// tippecanoe's default tiny-polygon reduction (`TinyArea`): polygons
    /// under one tolerance-square of area are accumulated into squares of
    /// that area instead of dropped. `false` is geojson-vt's (and
    /// MapLibre's GeoJSON source's) behaviour.
    pub tiny_polygon_reduction: bool,
}

impl Default for TileIndexOptions {
    fn default() -> Self {
        // 0.375 px * 4096/512 = 3.0; 128 px * 4096/512 = 1024 (MapLibre geojson_source.ts:217-218).
        Self {
            extent: crate::mvt::MVT_EXTENT,
            tolerance: 3.0,
            buffer: 1024.0,
            // MapLibre's GeoJSON source `maxzoom` default (18). At 14 a
            // z18 frame drew the z14 tile overzoomed, where one MVT unit is
            // 2 px and polygon edges snapped visibly. The index's memory is
            // per vertex (`xy`, `min_zoom`) and does not grow with this; the
            // eager split still stops at `index_max_zoom`, and deeper tiles
            // are cut lazily from their nearest indexed ancestor.
            max_zoom: 18,
            index_max_zoom: 5,
            index_max_points: 100_000,
            tiny_polygon_reduction: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Polygon,
    Line,
}

/// One ring (or line part): a run of `xy`/`min_zoom`. `size` is |area|
/// (polygon) or length (line), in [0,1] Mercator units, as geojson-vt's
/// `geom.size`.
#[derive(Debug, Clone, Copy)]
struct Ring {
    start: u32,
    len: u32,
    size: f32,
}

/// One polygon (exterior first, then holes) or one line part.
#[derive(Debug, Clone, Copy)]
struct Part {
    first_ring: u32,
    rings: u32,
}

#[derive(Debug, Clone, Copy)]
struct IndexedFeature {
    source_index: u32,
    kind: Kind,
    /// `false` for a tombstone (`applied`'s remove): no class, no geometry,
    /// in no tile list. Kept so slots stay stable until `compacted`.
    live: bool,
    /// Which geometry segment (`TileIndex::segs`) its rings live in.
    seg: u16,
    class: u16,
    first_part: u32,
    parts: u32,
    points: u32,
    /// `[min_x, min_y, max_x, max_y]`, fixed point.
    bbox: [u32; 4],
}

/// A source's polygons and lines, held once, ready to be cut into tiles.
pub struct TileIndex {
    options: TileIndexOptions,
    sq_tol: Vec<f64>, // z in 0..max_zoom
    sq_floor: f64,
    /// The geometry, in immutable segments: the build's base first, then
    /// one per incremental apply that brought new geometry (`applied`).
    /// Each behind an `Arc`, so a re-class (`reclassified`) or an apply
    /// shares every segment it does not add: once adopted, these are the
    /// source's ONLY geometry copy.
    segs: Vec<Arc<IndexGeometry>>,
    /// Sorted by `source_index` (rows only ever append), so a tile list
    /// sorted by slot is in painter's order.
    features: Vec<IndexedFeature>,
    tiles: HashMap<TileId, Arc<[u32]>>,
    /// Tombstoned slots, for the compaction trigger.
    dead: usize,
}

impl std::fmt::Debug for TileIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileIndex")
            .field("features", &self.features.len())
            .field("vertices", &self.segs.iter().map(|g| g.xy.len()).sum::<usize>())
            .field("segments", &self.segs.len())
            .field("indexed_tiles", &self.tiles.len())
            .finish_non_exhaustive()
    }
}

/// One geometry segment: rings' fixed-point vertices and their minimum
/// zooms, and the ring and part records over them.
struct IndexGeometry {
    xy: ZeroedBuf<[u32; 2]>,
    min_zoom: ZeroedBuf<u8>,
    rings: Vec<Ring>,
    parts: Vec<Part>,
}

/// One tile's worth of a source, clipped and simplified.
#[derive(Debug, Clone, PartialEq)]
pub struct TileCut {
    pub tile: TileId,
    pub features: Vec<CutFeature>,
    pub points: usize,
}

/// One feature of a cut. `feature` is the source feature's position in the
/// slice `build` was given.
#[derive(Debug, Clone, PartialEq)]
pub struct CutFeature {
    pub feature: u32,
    pub class: u16,
    pub geometry: CutGeometry,
}

/// Tile-local MVT units, in `[-buffer, extent + buffer]`; rings are closed.
#[derive(Debug, Clone, PartialEq)]
pub enum CutGeometry {
    Polygons(Vec<Vec<Vec<[i32; 2]>>>),
    Lines(Vec<Vec<[i32; 2]>>),
}

/// One polygon's rings (or one line part), borrowed from the source.
type RingGroup<'a> = Vec<&'a Vec<(f64, f64)>>;

pub(crate) fn to_fixed(lon_lat: (f64, f64)) -> [u32; 2] {
    let (x, y) = crate::overlay::geom::lon_lat_to_world(lon_lat.0, lon_lat.1);
    let f = |w: f64| (w / WORLD * FIXED).round().clamp(0.0, FIXED - 1.0) as u32;
    [f(x), f(y)]
}

/// The inverse of [`to_fixed`], to `(lon, lat)`.
pub(crate) fn from_fixed(q: [u32; 2]) -> (f64, f64) {
    let (x, y) = (f64::from(q[0]) / FIXED, f64::from(q[1]) / FIXED);
    let lon = x * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
    (lon, lat)
}

/// geojson-vt's `Math.round`: `floor(v + 0.5)`, which is shift-invariant
/// (`round_half_up(v + 4096) == round_half_up(v) + 4096` for every `v`).
/// `f64::round` rounds half AWAY from zero, so a vertex at -0.5 in one tile
/// (-> -1) and at 4095.5 in its left neighbour (-> 4096) would land one
/// unit apart: a seam in the buffer.
pub(crate) fn round_half_up(v: f64) -> i32 {
    (v + 0.5).floor() as i32
}

/// One change to an index, by source row. See [`TileIndex::applied`].
#[derive(Debug, Clone, Copy)]
pub enum IndexEdit<'a> {
    /// The row leaves the index (a removed feature, or one whose new
    /// geometry is not a polygon or line).
    Remove(u32),
    /// The row's class, and -- when `geometry` is `Some` -- its geometry:
    /// replaced when the row is indexed, added when it is not.
    Upsert { row: u32, class: u16, geometry: Option<&'a OverlayGeometry> },
}

/// What [`TileIndex::applied`] changed: every changed feature's box
/// before and after (fixed point, `[min_x, min_y, max_x, max_y]`), for the
/// caller's tile invalidation. A class-only change lists its box once.
#[derive(Debug, Clone, Default)]
pub struct IndexDelta {
    /// Boxes whose geometry or membership changed.
    pub boxes: Vec<[u32; 4]>,
    /// Rows that only changed class: `(row, old class, box)`. No tile list
    /// moves; a baked tile holding the row changes only its paint.
    pub recolored: Vec<(u32, u16, [u32; 4])>,
    /// Eager tile lists rewritten.
    pub lists_rewritten: usize,
    /// Whether the apply re-split the eager index (an out-of-order insert).
    pub resplit: bool,
}

/// Pass 1: ring/part/feature records and each ring's source slice.
struct Records<'a> {
    rings: Vec<Ring>,
    parts: Vec<Part>,
    feats: Vec<IndexedFeature>,
    sources: Vec<(&'a [(f64, f64)], bool)>,
    total: u64,
}

/// Pass 1 over `items`, `(source_index, geometry, class)` each.
fn records<'a>(items: impl Iterator<Item = (u32, &'a OverlayGeometry, u16)>, seg: u16) -> Records<'a> {
    let mut r = Records { rings: Vec::new(), parts: Vec::new(), feats: Vec::new(), sources: Vec::new(), total: 0 };
    for (i, geometry, class) in items {
        if class == NO_CLASS {
            continue;
        }
        let Some((kind, groups)) = ring_groups(geometry) else { continue };
        let first_part = r.parts.len() as u32;
        let mut points = 0u32;
        for g in &groups {
            r.parts.push(Part { first_ring: r.rings.len() as u32, rings: g.len() as u32 });
            for ring in g {
                r.rings.push(Ring { start: r.total as u32, len: ring.len() as u32, size: 0.0 });
                r.sources.push((ring.as_slice(), kind == Kind::Polygon));
                r.total += ring.len() as u64;
                points += ring.len() as u32;
            }
        }
        r.feats.push(IndexedFeature {
            source_index: i,
            kind,
            live: true,
            seg,
            class,
            first_part,
            parts: groups.len() as u32,
            points,
            bbox: [0; 4],
        });
    }
    assert!(r.total < u64::from(u32::MAX), "a single source over 4 G vertices");
    r
}

/// The rings `build` indexes for one geometry, or `None` when it indexes
/// none (a point, empty, degenerate, or already `Indexed`).
fn ring_groups(geometry: &OverlayGeometry) -> Option<(Kind, Vec<RingGroup<'_>>)> {
    let (kind, groups): (Kind, Vec<RingGroup<'_>>) = match geometry {
        OverlayGeometry::Polygon(polys) => (
            Kind::Polygon,
            polys.iter().filter(|p| p.first().is_some_and(|o| o.len() >= 4)).map(|p| p.iter().collect()).collect(),
        ),
        OverlayGeometry::Line(lines) => (Kind::Line, lines.iter().filter(|l| l.len() >= 2).map(|l| vec![l]).collect()),
        OverlayGeometry::Point(_) | OverlayGeometry::Empty | OverlayGeometry::Indexed { .. } => return None,
    };
    (!groups.is_empty()).then_some((kind, groups))
}

/// Pass 2 (parallel): project, DP importance, min zoom, ring size -- then
/// every record's bbox. `None` once `cancel` answers true.
fn project(
    mut r: Records<'_>,
    sq_tol: &[f64],
    sq_floor: f64,
    cancel: &(dyn Fn() -> bool + Sync),
) -> Option<(IndexGeometry, Vec<IndexedFeature>)> {
    let total = r.total;
    // Mapped on their own, not `vec![0; n]`: see `zeroed`.
    let mut xy: ZeroedBuf<[u32; 2]> = ZeroedBuf::zeroed(total as usize);
    let mut min_zoom: ZeroedBuf<u8> = ZeroedBuf::zeroed(total as usize);
    // One chunk, run inline, where a thread cannot start (wasm): see
    // `cluster::threads_allowed`.
    let threads = if crate::overlay::cluster::threads_allowed() {
        std::thread::available_parallelism().map_or(4, |n| n.get()).min(16)
    } else {
        1
    };
    let per = (total as usize / threads).max(1);
    let mut chunks: Vec<std::ops::Range<usize>> = Vec::new(); // ring ranges
    let (mut start, mut acc) = (0usize, 0usize);
    for (k, ring) in r.rings.iter().enumerate() {
        acc += ring.len as usize;
        if acc >= per {
            chunks.push(start..k + 1);
            start = k + 1;
            acc = 0;
        }
    }
    if start < r.rings.len() {
        chunks.push(start..r.rings.len());
    }
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let sources = &r.sources;
    #[allow(clippy::type_complexity)]
    let mut jobs: Vec<Box<dyn FnOnce() + Send + '_>> = Vec::with_capacity(chunks.len());
    {
        let (mut xy_rest, mut mz_rest, mut ring_rest) = (&mut xy[..], &mut min_zoom[..], &mut r.rings[..]);
        for chunk in &chunks {
            let n_rings = chunk.len();
            let n_pts: usize = ring_rest[..n_rings].iter().map(|r| r.len as usize).sum();
            let (xy_c, a) = std::mem::take(&mut xy_rest).split_at_mut(n_pts);
            let (mz_c, b) = std::mem::take(&mut mz_rest).split_at_mut(n_pts);
            let (rg_c, c) = std::mem::take(&mut ring_rest).split_at_mut(n_rings);
            (xy_rest, mz_rest, ring_rest) = (a, b, c);
            let src = &sources[chunk.clone()];
            let cancelled = &cancelled;
            jobs.push(Box::new(move || {
                let (mut proj, mut imp) = (Vec::new(), Vec::new());
                let mut at = 0usize;
                for (ring, (coords, is_polygon)) in rg_c.iter_mut().zip(src) {
                    if cancel() {
                        cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                    proj.clear();
                    for (j, &c) in coords.iter().enumerate() {
                        let f = to_fixed(c);
                        xy_c[at + j] = f;
                        proj.push([f64::from(f[0]) / FIXED, f64::from(f[1]) / FIXED]);
                    }
                    imp.resize(proj.len(), 0.0);
                    simplify::importance(&proj, sq_floor, &mut imp);
                    for (j, &v) in imp.iter().enumerate() {
                        mz_c[at + j] = simplify::min_zoom_of(v, sq_tol);
                    }
                    ring.size = if *is_polygon {
                        clip::shoelace(&proj).abs() as f32
                    } else {
                        proj.windows(2).map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1])).sum::<f64>() as f32
                    };
                    at += coords.len();
                }
            }));
        }
    }
    if crate::overlay::cluster::threads_allowed() {
        std::thread::scope(|s| {
            for job in jobs {
                s.spawn(job);
            }
        });
    } else {
        jobs.into_iter().for_each(|job| job());
    }
    if cancelled.into_inner() || cancel() {
        return None;
    }
    let geom = IndexGeometry { xy, min_zoom, rings: r.rings, parts: r.parts };
    let mut feats = r.feats;
    for f in &mut feats {
        f.bbox = geom.bbox_of(f);
    }
    Some((geom, feats))
}

impl IndexGeometry {
    fn bbox_of(&self, f: &IndexedFeature) -> [u32; 4] {
        let mut b = [u32::MAX, u32::MAX, 0, 0];
        for p in &self.parts[f.first_part as usize..(f.first_part + f.parts) as usize] {
            for r in &self.rings[p.first_ring as usize..(p.first_ring + p.rings) as usize] {
                for q in &self.xy[r.start as usize..(r.start + r.len) as usize] {
                    b = [b[0].min(q[0]), b[1].min(q[1]), b[2].max(q[0]), b[3].max(q[1])];
                }
            }
        }
        b
    }

    fn bytes(&self) -> usize {
        use std::mem::size_of;
        self.xy.held_bytes()
            + self.min_zoom.held_bytes()
            + self.rings.capacity() * size_of::<Ring>()
            + self.parts.capacity() * size_of::<Part>()
    }
}

impl TileIndex {
    /// Projects, simplifies and indexes `features`. `class_of[i]` is feature
    /// `i`'s paint class; [`NO_CLASS`] (or a missing entry) skips it, as do
    /// points and empty geometry, which stay on the whole-source path.
    /// Returns `None` once `cancel` answers true.
    pub fn build(
        features: &[OverlayFeature],
        class_of: &[u16],
        options: TileIndexOptions,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Option<Self> {
        assert!(options.max_zoom <= 24 && options.index_max_zoom <= options.max_zoom);
        let e = f64::from(options.extent);
        let sq_tol: Vec<f64> = (0..options.max_zoom)
            .map(|z| (options.tolerance / (e * f64::from(1u32 << z))).powi(2))
            .collect();
        let sq_floor = (options.tolerance / (e * f64::from(1u32 << options.max_zoom))).powi(2);
        let r = records(
            features
                .iter()
                .enumerate()
                .map(|(i, f)| (i as u32, &f.geometry, class_of.get(i).copied().unwrap_or(NO_CLASS))),
            0,
        );
        let (geom, feats) = project(r, &sq_tol, sq_floor, cancel)?;
        let mut index = Self {
            options,
            sq_tol,
            sq_floor,
            segs: vec![Arc::new(geom)],
            features: feats,
            tiles: HashMap::new(),
            dead: 0,
        };
        index.split_eagerly();
        Some(index)
    }

    fn seg(&self, f: &IndexedFeature) -> &IndexGeometry {
        &self.segs[usize::from(f.seg)]
    }

    /// geojson-vt's first-pass `splitTile`: index tiles down to
    /// `index_max_zoom`, splitting only tiles over `index_max_points`.
    fn split_eagerly(&mut self) {
        self.tiles.clear();
        let all: Arc<[u32]> = (0..self.features.len() as u32).filter(|&s| self.features[s as usize].live).collect();
        let mut stack = vec![(TileId { z: 0, x: 0, y: 0 }, all)];
        while let Some((tile, list)) = stack.pop() {
            let points: u64 = list.iter().map(|&f| u64::from(self.features[f as usize].points)).sum();
            self.tiles.insert(tile, Arc::clone(&list));
            if tile.z >= self.options.index_max_zoom || points <= self.options.index_max_points {
                continue;
            }
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let child = TileId { z: tile.z + 1, x: tile.x * 2 + dx, y: tile.y * 2 + dy };
                let sub = self.filter(&list, child);
                if !sub.is_empty() {
                    stack.push((child, sub.into()));
                }
            }
        }
    }

    /// The tile's box plus its buffer, in fixed point.
    fn tile_box(&self, t: TileId) -> [i64; 4] {
        let span = 1i64 << (32 - u32::from(t.z));
        let buf = (self.options.buffer * span as f64 / f64::from(self.options.extent)).ceil() as i64;
        let (x, y) = (i64::from(t.x), i64::from(t.y));
        [x * span - buf, y * span - buf, (x + 1) * span + buf, (y + 1) * span + buf]
    }

    fn filter(&self, list: &[u32], tile: TileId) -> Vec<u32> {
        let b = self.tile_box(tile);
        list.iter()
            .copied()
            .filter(|&f| {
                let bb = self.features[f as usize].bbox;
                i64::from(bb[2]) >= b[0]
                    && i64::from(bb[0]) <= b[2]
                    && i64::from(bb[3]) >= b[1]
                    && i64::from(bb[1]) <= b[3]
            })
            .collect()
    }

    /// The lazy drill-down: the nearest indexed ancestor's list, filtered.
    fn candidates(&self, tile: TileId) -> Vec<u32> {
        let mut a = tile;
        loop {
            if let Some(list) = self.tiles.get(&a) {
                return if a == tile { list.to_vec() } else { self.filter(list, tile) };
            }
            if a.z == 0 {
                return Vec::new();
            }
            a = TileId { z: a.z - 1, x: a.x >> 1, y: a.y >> 1 };
        }
    }

    /// Whether no feature's box (plus the buffer) reaches `tile`.
    pub fn is_empty_at(&self, tile: TileId) -> bool {
        self.candidates(tile).is_empty()
    }

    pub fn max_zoom(&self) -> u8 {
        self.options.max_zoom
    }

    /// How many tiles the eager index holds.
    pub fn indexed_tiles(&self) -> usize {
        self.tiles.len()
    }

    /// Capacity-based census of what the index holds.
    pub fn bytes(&self) -> usize {
        use std::mem::size_of;
        self.segs.iter().map(|g| g.bytes()).sum::<usize>()
            + self.sq_tol.capacity() * size_of::<f64>()
            + self.features.capacity() * size_of::<IndexedFeature>()
            + self.tiles.capacity() * (size_of::<TileId>() + size_of::<Arc<[u32]>>())
            + self.tiles.values().map(|l| l.len() * size_of::<u32>() + 2 * size_of::<usize>()).sum::<usize>()
    }

    /// Where source feature `source_index` sits in the index, or `None`
    /// when `build` skipped it (a point, empty, or [`NO_CLASS`]).
    /// `features` is pushed in source order, so it is sorted.
    pub fn feature_slot(&self, source_index: u32) -> Option<u32> {
        self.features.binary_search_by_key(&source_index, |f| f.source_index).ok().map(|i| i as u32)
    }

    /// The exact hit test on the stored geometry: `world` in world units
    /// (`[0, WORLD)`, what `lon_lat_to_world` returns). A line hits within
    /// `tolerance_world` of a segment; a polygon hits inside its exterior
    /// and outside its holes (even-odd per polygon, as `hit::hits` does
    /// for lon/lat), with no tolerance -- the lon/lat test has none either.
    ///
    /// The geometry is the build's full-resolution copy (every vertex,
    /// whatever its minimum zoom), rounded to 2^-32 of the world: under
    /// 1 cm on the ground, and under one pixel at z22.
    pub fn feature_hit(&self, source_index: u32, world: (f64, f64), tolerance_world: f64) -> bool {
        let Some(slot) = self.feature_slot(source_index) else { return false };
        let f = self.features[slot as usize];
        if !f.live {
            return false;
        }
        let k = FIXED / WORLD;
        let (px, py) = (world.0 * k, world.1 * k);
        let tol = tolerance_world * k;
        let g = self.seg(&f);
        let parts = &g.parts[f.first_part as usize..(f.first_part + f.parts) as usize];
        let pt = |q: [u32; 2]| (f64::from(q[0]), f64::from(q[1]));
        let seg_dist2 = |a: (f64, f64), b: (f64, f64)| {
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len2 = dx * dx + dy * dy;
            let t = if len2 == 0.0 { 0.0 } else { (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0) };
            let (x, y) = (a.0 + dx * t - px, a.1 + dy * t - py);
            x * x + y * y
        };
        match f.kind {
            Kind::Line => parts.iter().any(|p| {
                let r = g.rings[p.first_ring as usize];
                let line = &g.xy[r.start as usize..(r.start + r.len) as usize];
                if line.len() == 1 {
                    return seg_dist2(pt(line[0]), pt(line[0])) <= tol * tol;
                }
                line.windows(2).any(|w| seg_dist2(pt(w[0]), pt(w[1])) <= tol * tol)
            }),
            Kind::Polygon => parts.iter().any(|p| {
                let mut inside = false;
                for r in &g.rings[p.first_ring as usize..(p.first_ring + p.rings) as usize] {
                    let ring = &g.xy[r.start as usize..(r.start + r.len) as usize];
                    // Closed or not: the edge back to the first vertex too.
                    let n = ring.len();
                    for i in 0..n {
                        let (a, b) = (pt(ring[i]), pt(ring[(i + 1) % n]));
                        if (a.1 > py) != (b.1 > py) && px < (b.0 - a.0) * (py - a.1) / (b.1 - a.1) + a.0 {
                            inside = !inside;
                        }
                    }
                }
                inside
            }),
        }
    }

    /// Source feature `source_index`'s box as `(min_lon, min_lat,
    /// max_lon, max_lat)`, or `None` when the index does not hold it.
    pub fn feature_bbox_lon_lat(&self, source_index: u32) -> Option<(f64, f64, f64, f64)> {
        let f = self.features[self.feature_slot(source_index)? as usize];
        if !f.live {
            return None;
        }
        let b = f.bbox;
        let (w, n) = from_fixed([b[0], b[1]]);
        let (e, s) = from_fixed([b[2], b[3]]);
        Some((w, s, e, n))
    }

    /// Source feature `source_index`'s geometry back in `(lon, lat)`, from
    /// the fixed-point copy (2^-32 of the world: under 1 cm). For the
    /// paths that need lon/lat after the index became the one copy --
    /// the untiled fallback (`OverlayStore::force_untiled`).
    pub fn feature_geometry(&self, source_index: u32) -> Option<OverlayGeometry> {
        let f = self.features[self.feature_slot(source_index)? as usize];
        if !f.live {
            return None;
        }
        let g = self.seg(&f);
        let ring = |r: &Ring| -> Vec<(f64, f64)> {
            g.xy[r.start as usize..(r.start + r.len) as usize].iter().map(|&q| from_fixed(q)).collect()
        };
        let parts = &g.parts[f.first_part as usize..(f.first_part + f.parts) as usize];
        Some(match f.kind {
            Kind::Line => OverlayGeometry::Line(parts.iter().map(|p| ring(&g.rings[p.first_ring as usize])).collect()),
            Kind::Polygon => OverlayGeometry::Polygon(
                parts
                    .iter()
                    .map(|p| g.rings[p.first_ring as usize..(p.first_ring + p.rings) as usize].iter().map(ring).collect())
                    .collect(),
            ),
        })
    }

    /// `features` with every `Indexed` geometry this index holds put back
    /// into `(lon, lat)` (see `feature_geometry`); everything else cloned.
    pub(crate) fn restore_geometry(&self, features: &[OverlayFeature]) -> Vec<OverlayFeature> {
        features
            .iter()
            .enumerate()
            .map(|(at, f)| OverlayFeature {
                index: f.index,
                id: f.id.clone(),
                geometry: match &f.geometry {
                    OverlayGeometry::Indexed { .. } => {
                        self.feature_geometry(at as u32).unwrap_or(OverlayGeometry::Empty)
                    }
                    g => g.clone(),
                },
                paint: f.paint.clone(),
            })
            .collect()
    }

    /// The same geometry and eager tiles, re-classed: what a paint update
    /// to an adopted source rebuilds, since the lon/lat `build` reads is
    /// gone. `class_of[i]` is source feature `i`'s class; [`NO_CLASS`]
    /// makes `cut` skip it. Shares the geometry (`Arc`): no second copy.
    pub fn reclassified(&self, class_of: &[u16]) -> Self {
        let features = self
            .features
            .iter()
            .map(|f| IndexedFeature {
                class: if f.live { class_of.get(f.source_index as usize).copied().unwrap_or(NO_CLASS) } else { NO_CLASS },
                ..*f
            })
            .collect();
        Self {
            options: self.options,
            sq_tol: self.sq_tol.clone(),
            sq_floor: self.sq_floor,
            segs: self.segs.clone(),
            features,
            tiles: self.tiles.clone(),
            dead: self.dead,
        }
    }

    /// The slot of `source_index` among `features` (sorted by it).
    fn slot_in(features: &[IndexedFeature], source_index: u32) -> Result<usize, usize> {
        features.binary_search_by_key(&source_index, |f| f.source_index)
    }

    fn meets(&self, bb: [u32; 4], tile: TileId) -> bool {
        let b = self.tile_box(tile);
        i64::from(bb[2]) >= b[0] && i64::from(bb[0]) <= b[2] && i64::from(bb[3]) >= b[1] && i64::from(bb[1]) <= b[3]
    }

    /// This index with `edits` applied, in order, and what changed -- the
    /// incremental path (see the incremental-updates spec, "Tile index").
    ///
    /// New and replaced geometry is projected and simplified exactly as
    /// `build` does it, into ONE new segment; every other segment is shared.
    /// Records change in place at their slot (a remove is a tombstone), an
    /// added row is pushed at the end, and only the eager tile lists whose
    /// buffered box meets a changed feature's old or new box are rewritten:
    /// the rest are shared `Arc`s. The result cuts exactly what `build`
    /// over the post-edit features cuts (`an_applied_index_cuts_what_a_rebuild_cuts`).
    ///
    /// A row added BELOW an indexed row (a point that became a polygon) is
    /// inserted in order and the eager index re-split: rare, and still exact.
    pub fn applied(&self, edits: &[IndexEdit<'_>]) -> (Self, IndexDelta) {
        let delta = IndexDelta::default();
        let seg_no = u16::try_from(self.segs.len()).expect("compaction keeps segments far below u16::MAX");
        // Every geometry-carrying upsert, in edit order: whether it yields a
        // record decides between replace and remove.
        let geo: Vec<(u32, &OverlayGeometry, u16)> = edits
            .iter()
            .filter_map(|e| match *e {
                IndexEdit::Upsert { row, class, geometry: Some(g) } => Some((row, g, class)),
                _ => None,
            })
            .collect();
        let yields: Vec<bool> =
            geo.iter().map(|(_, g, class)| *class != NO_CLASS && ring_groups(g).is_some()).collect();
        let mut next = self.shallow();
        let fresh: std::collections::VecDeque<IndexedFeature> = if yields.iter().any(|&y| y) {
            let r = records(geo.iter().copied(), seg_no);
            let (geom, feats) = project(r, &self.sq_tol, self.sq_floor, &|| false).expect("never cancelled");
            next.segs.push(Arc::new(geom));
            feats.into()
        } else {
            std::collections::VecDeque::new()
        };
        self.apply_with(edits, &geo, &yields, fresh, next, delta)
    }

    /// Everything but the feature records and tile lists, shared.
    fn shallow(&self) -> Self {
        Self {
            options: self.options,
            sq_tol: self.sq_tol.clone(),
            sq_floor: self.sq_floor,
            segs: self.segs.clone(),
            features: Vec::new(),
            tiles: HashMap::new(),
            dead: self.dead,
        }
    }

    fn apply_with(
        &self,
        edits: &[IndexEdit<'_>],
        geo: &[(u32, &OverlayGeometry, u16)],
        yields: &[bool],
        mut fresh: std::collections::VecDeque<IndexedFeature>,
        mut next: Self,
        mut delta: IndexDelta,
    ) -> (Self, IndexDelta) {
        let mut features = self.features.clone();
        let mut dead = self.dead;
        let mut changed: Vec<u32> = Vec::new();
        // Boxes whose tile lists move (geometry or membership changed).
        let mut list_boxes: Vec<[u32; 4]> = Vec::new();
        let mut out_of_order = false;
        let mut g = 0usize;
        let kill = |f: &mut IndexedFeature, dead: &mut usize, boxes: &mut Vec<[u32; 4]>| {
            if f.live {
                boxes.push(f.bbox);
                f.live = false;
                f.class = NO_CLASS;
                f.parts = 0;
                f.points = 0;
                *dead += 1;
            }
        };
        for edit in edits {
            match *edit {
                IndexEdit::Remove(row) => {
                    if let Ok(slot) = Self::slot_in(&features, row) {
                        kill(&mut features[slot], &mut dead, &mut list_boxes);
                        changed.push(slot as u32);
                    }
                }
                IndexEdit::Upsert { row, class, geometry: None } => {
                    if let Ok(slot) = Self::slot_in(&features, row) {
                        let f = &mut features[slot];
                        if f.live && f.class != class {
                            delta.recolored.push((row, f.class, f.bbox));
                            f.class = class;
                        }
                    }
                }
                IndexEdit::Upsert { row, geometry: Some(_), .. } => {
                    debug_assert_eq!(geo[g].0, row);
                    let rec = if yields[g] { fresh.pop_front() } else { None };
                    g += 1;
                    match (Self::slot_in(&features, row), rec) {
                        (Ok(slot), Some(rec)) => {
                            let old = features[slot];
                            if old.live {
                                list_boxes.push(old.bbox);
                            } else {
                                dead -= 1;
                            }
                            features[slot] = rec;
                            list_boxes.push(rec.bbox);
                            changed.push(slot as u32);
                        }
                        (Ok(slot), None) => {
                            kill(&mut features[slot], &mut dead, &mut list_boxes);
                            changed.push(slot as u32);
                        }
                        (Err(at), Some(rec)) => {
                            if at < features.len() {
                                out_of_order = true;
                            }
                            features.insert(at, rec);
                            list_boxes.push(rec.bbox);
                            changed.push(at as u32);
                        }
                        (Err(_), None) => {}
                    }
                }
            }
        }
        delta.boxes.extend_from_slice(&list_boxes);
        next.features = features;
        next.dead = dead;
        if out_of_order {
            next.split_eagerly();
            delta.resplit = true;
            return (next, delta);
        }
        next.tiles = self.tiles.clone();
        if changed.is_empty() {
            return (next, delta);
        }
        changed.sort_unstable();
        changed.dedup();
        let mut bits = vec![0u64; next.features.len().div_ceil(64)];
        for &s in &changed {
            bits[s as usize / 64] |= 1 << (s % 64);
        }
        // The eager tiles any moved box reaches: a walk down from z0 through
        // the tiles that exist (a child exists only under a split parent).
        let mut touched: std::collections::HashSet<TileId> = std::collections::HashSet::new();
        for b in &list_boxes {
            let mut stack = vec![TileId { z: 0, x: 0, y: 0 }];
            while let Some(t) = stack.pop() {
                if !self.tiles.contains_key(&t) || !self.meets(*b, t) {
                    continue;
                }
                touched.insert(t);
                if t.z < self.options.index_max_zoom {
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        stack.push(TileId { z: t.z + 1, x: t.x * 2 + dx, y: t.y * 2 + dy });
                    }
                }
            }
        }
        for t in touched {
            let old = &self.tiles[&t];
            let adds: Vec<u32> = changed
                .iter()
                .copied()
                .filter(|&s| {
                    let f = &next.features[s as usize];
                    f.live && self.meets(f.bbox, t)
                })
                .collect();
            let mut out: Vec<u32> = Vec::with_capacity(old.len() + adds.len());
            let mut a = adds.into_iter().peekable();
            for &s in old.iter().filter(|&&s| bits[s as usize / 64] & (1 << (s % 64)) == 0) {
                while let Some(&x) = a.peek().filter(|&&x| x < s) {
                    out.push(x);
                    a.next();
                }
                out.push(s);
            }
            out.extend(a);
            next.tiles.insert(t, out.into());
            delta.lists_rewritten += 1;
        }
        (next, delta)
    }

    /// Whether the segments or tombstones have grown enough that the next
    /// apply should compact: over 64 segments, segment vertices past 25% of
    /// the base's, or tombstones past 25% of the slots.
    pub fn wants_compaction(&self) -> bool {
        let base = self.segs.first().map_or(0, |g| g.xy.len());
        let extra: usize = self.segs.iter().skip(1).map(|g| g.xy.len()).sum();
        self.segs.len() > 64 || extra * 4 > base.max(1 << 16) || self.dead * 4 > self.features.len().max(1 << 16)
    }

    /// Tombstoned slots.
    pub fn dead(&self) -> usize {
        self.dead
    }

    /// Geometry segments (1 after `build` or `compacted`).
    pub fn segments(&self) -> usize {
        self.segs.len()
    }

    /// Every live feature's geometry copied into one fresh segment, the
    /// tombstones dropped, and the eager index re-split. `remap[row]` is the
    /// row's new number (`u32::MAX`: dropped, whatever it holds); `None`
    /// keeps rows as they are. `remap` must be increasing over kept rows,
    /// which is what keeps painter's order.
    pub fn compacted(&self, remap: Option<&[u32]>) -> Self {
        let keep: Vec<IndexedFeature> = self
            .features
            .iter()
            .filter(|f| f.live)
            .filter_map(|f| {
                let row = match remap {
                    Some(m) => *m.get(f.source_index as usize)?,
                    None => f.source_index,
                };
                (row != u32::MAX).then_some(IndexedFeature { source_index: row, ..*f })
            })
            .collect();
        let total: usize = keep.iter().map(|f| f.points as usize).sum();
        let mut xy: ZeroedBuf<[u32; 2]> = ZeroedBuf::zeroed(total);
        let mut min_zoom: ZeroedBuf<u8> = ZeroedBuf::zeroed(total);
        let (mut rings, mut parts) = (Vec::new(), Vec::new());
        let mut at = 0usize;
        let mut features = Vec::with_capacity(keep.len());
        for f in keep {
            let g = self.seg(&f);
            let first_part = parts.len() as u32;
            for p in &g.parts[f.first_part as usize..(f.first_part + f.parts) as usize] {
                parts.push(Part { first_ring: rings.len() as u32, rings: p.rings });
                for r in &g.rings[p.first_ring as usize..(p.first_ring + p.rings) as usize] {
                    let (s, n) = (r.start as usize, r.len as usize);
                    xy[at..at + n].copy_from_slice(&g.xy[s..s + n]);
                    min_zoom[at..at + n].copy_from_slice(&g.min_zoom[s..s + n]);
                    rings.push(Ring { start: at as u32, len: r.len, size: r.size });
                    at += n;
                }
            }
            features.push(IndexedFeature { seg: 0, first_part, ..f });
        }
        let mut next = Self {
            options: self.options,
            sq_tol: self.sq_tol.clone(),
            sq_floor: self.sq_floor,
            segs: vec![Arc::new(IndexGeometry { xy, min_zoom, rings, parts })],
            features,
            tiles: HashMap::new(),
            dead: 0,
        };
        next.split_eagerly();
        next
    }

    /// One ring's vertices at zoom `z`, in tile-local f64 units (exact).
    fn ring_at(&self, g: &IndexGeometry, r: Ring, tile: TileId, exact: bool, out: &mut Vec<[f64; 2]>) {
        out.clear();
        let s = f64::from(self.options.extent) * f64::from(1u32 << tile.z) / FIXED; // a power of two
        let (ox, oy) = (
            f64::from(tile.x) * f64::from(self.options.extent),
            f64::from(tile.y) * f64::from(self.options.extent),
        );
        let range = r.start as usize..(r.start + r.len) as usize;
        for (p, &mz) in g.xy[range.clone()].iter().zip(&g.min_zoom[range]) {
            if exact || mz <= tile.z {
                out.push([f64::from(p[0]) * s - ox, f64::from(p[1]) * s - oy]);
            }
        }
    }

    /// The squares one dropped outer ring `r` carries in `tile` (see
    /// [`TinyArea`], in a tile that owns the feature): none unless its area
    /// tips the running total. `kept`: for a ring larger than a tolerance-square
    /// that simplification emptied, the vertices it kept at this zoom (tile
    /// units); it counts only when they span a compact box.
    #[allow(clippy::too_many_arguments)]
    fn tiny_squares(
        &self,
        tiny: &mut TinyArea,
        g: &IndexGeometry,
        r: Ring,
        tile: TileId,
        kept: Option<&[[f64; 2]]>,
        scratch: &mut Vec<[f64; 2]>,
        polys: &mut Vec<Vec<Vec<[i32; 2]>>>,
    ) -> usize {
        let e = f64::from(self.options.extent);
        let units = e * f64::from(1u32 << tile.z);
        let k = units / FIXED;
        let (ox, oy) = (f64::from(tile.x) * e, f64::from(tile.y) * e);
        let at = |q: [u32; 2]| [f64::from(q[0]) * k - ox, f64::from(q[1]) * k - oy];
        if let Some(kept) = kept {
            // Douglas-Peucker keeps a ring's farthest point from its first,
            // so a sliver's kept vertices span its length.
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            for p in kept {
                for i in 0..2 {
                    lo[i] = lo[i].min(p[i]);
                    hi[i] = hi[i].max(p[i]);
                }
            }
            let limit = TinyArea::MAX_COLLAPSED_SIDE * self.options.tolerance;
            if hi[0] - lo[0] > limit || hi[1] - lo[1] > limit {
                return 0; // a sliver: geojson-vt's drop
            }
        }
        let n = tiny.add(f64::from(r.size) * units * units);
        if n == 0 {
            return 0;
        }
        // An interior point from at most ~16 of the ring's vertices: the
        // ring spans a few tolerances at most and the square snaps to a
        // tolerance grid, so the full ring (hundreds of vertices for a
        // census block group) would buy no placement, only time.
        let v = at(g.xy[r.start as usize]);
        scratch.clear();
        let step = (r.len as usize / 16).max(1);
        let range = r.start as usize..(r.start + r.len) as usize;
        scratch.extend(g.xy[range.clone()].iter().step_by(step).map(|&q| at(q)));
        scratch.push(v);
        // For a COLLAPSED ring (up to 3 tolerances a side, bigger than its
        // square) the sample can put a point in a notch the true ring does
        // not cover, and hover (which tests the true ring) would then name
        // another feature than the square's colour: keep the first
        // candidate the FULL ring holds, else a chord of the full ring. A
        // tiny ring (under one square of area) is not checked: any point of
        // its sample lies in its own box, a few units across, so the grid
        // cell it snaps to is the same or the next one over; checking it
        // cost ~2.5 ms (+4.5%) of the national bake, a full read of ~46k
        // rings.
        let at = if kept.is_some() {
            checked_interior(scratch, &g.xy[range], at)
        } else {
            interior_candidates(scratch).first().copied()
        }
        .unwrap_or(v);
        polys.extend((0..n).map(|_| vec![tiny.place(at)]));
        5 * n
    }

    /// Clips and simplifies every candidate feature to `tile`, with the
    /// buffer. `tile.z` must not exceed `max_zoom`: overzoom is the
    /// caller's, who draws the `max_zoom` ancestor.
    pub fn cut(&self, tile: TileId) -> TileCut {
        assert!(tile.z <= self.options.max_zoom, "overzoom is the caller's: draw the max_zoom ancestor");
        let exact = tile.z == self.options.max_zoom;
        let sq_tol = if exact { 0.0 } else { self.sq_tol[tile.z as usize] };
        let (k1, k2) = (-self.options.buffer, f64::from(self.options.extent) + self.options.buffer);
        let bx = self.tile_box(tile);
        let round = |v: &[[f64; 2]]| -> Vec<[i32; 2]> {
            let mut out: Vec<[i32; 2]> = Vec::with_capacity(v.len());
            for p in v {
                let q = [round_half_up(p[0]), round_half_up(p[1])];
                if out.last() != Some(&q) {
                    out.push(q);
                }
            }
            out
        };
        let (mut raw, mut a, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let mut cut = TileCut { tile, features: Vec::new(), points: 0 };
        // Tiny-polygon reduction (tippecanoe's default; see `TinyArea`).
        let mut tiny = (self.options.tiny_polygon_reduction && !exact).then(|| TinyArea::new(self.options.tolerance, self.options.extent));
        for fi in self.candidates(tile) {
            let f = self.features[fi as usize];
            if f.class == NO_CLASS {
                continue; // re-classed away (`reclassified`)
            }
            let inside = i64::from(f.bbox[0]) >= bx[0]
                && i64::from(f.bbox[1]) >= bx[1]
                && i64::from(f.bbox[2]) <= bx[2]
                && i64::from(f.bbox[3]) <= bx[3];
            let g = self.seg(&f);
            let parts = &g.parts[f.first_part as usize..(f.first_part + f.parts) as usize];
            // Tiny-polygon ownership: only the tile whose own box holds the
            // feature's bbox centre accounts for its tiny rings, so a buffer
            // overlap never counts one twice (and no ring's geometry is read
            // to decide it).
            let owned = {
                let span = 1i64 << (32 - u32::from(tile.z));
                let (cx, cy) = ((i64::from(f.bbox[0]) + i64::from(f.bbox[2])) / 2, (i64::from(f.bbox[1]) + i64::from(f.bbox[3])) / 2);
                cx / span == i64::from(tile.x) && cy / span == i64::from(tile.y)
            };
            let geometry = match f.kind {
                Kind::Polygon => {
                    let mut polys = Vec::new();
                    'poly: for p in parts {
                        let mut out_rings: Vec<Vec<[i32; 2]>> = Vec::new();
                        let rings = &g.rings[p.first_ring as usize..(p.first_ring + p.rings) as usize];
                        for (k, &r) in rings.iter().enumerate() {
                            // tile.js addLine: a polygon ring under one tolerance-pixel of area is dropped.
                            if !exact && f64::from(r.size) < sq_tol {
                                if k == 0 {
                                    // ... unless its area goes to a square.
                                    if let (Some(tiny), true) = (tiny.as_mut(), owned) {
                                        cut.points += self.tiny_squares(tiny, g, r, tile, None, &mut raw, &mut polys);
                                    }
                                    continue 'poly;
                                }
                                continue;
                            }
                            self.ring_at(g, r, tile, exact, &mut raw);
                            let ring = if inside {
                                round(&raw)
                            } else {
                                clip::clip_ring(&raw, k1, k2, 0, &mut a);
                                clip::clip_ring(&a, k1, k2, 1, &mut b);
                                round(&b)
                            };
                            if ring.len() >= 4 && ring.first() == ring.last() {
                                cut.points += ring.len();
                                out_rings.push(ring);
                            } else if k == 0 {
                                // No outer ring: its holes must not paint. A
                                // ring Douglas-Peucker reduced to nothing is
                                // dropped as geojson-vt drops it -- unless it
                                // is COMPACT (`TinyArea::MAX_COLLAPSED_SIDE`),
                                // when its area goes to squares one cell at
                                // a time, as a tiny ring's does.
                                if let (Some(tiny), true) = (tiny.as_mut(), !exact && inside && owned) {
                                    cut.points += self.tiny_squares(tiny, g, r, tile, Some(raw.as_slice()), &mut a, &mut polys);
                                }
                                continue 'poly;
                            }
                        }
                        polys.push(out_rings);
                    }
                    if polys.is_empty() {
                        continue;
                    }
                    CutGeometry::Polygons(polys)
                }
                Kind::Line => {
                    let tol = sq_tol.sqrt();
                    let mut lines = Vec::new();
                    for p in parts {
                        let r = g.rings[p.first_ring as usize];
                        if !exact && f64::from(r.size) < tol {
                            continue;
                        }
                        self.ring_at(g, r, tile, exact, &mut raw);
                        let mut slices = Vec::new();
                        if inside {
                            slices.push(raw.clone());
                        } else {
                            let mut xs = Vec::new();
                            clip::clip_line(&raw, k1, k2, 0, &mut xs);
                            for s in &xs {
                                clip::clip_line(s, k1, k2, 1, &mut slices);
                            }
                        }
                        for s in slices {
                            let l = round(&s);
                            if l.len() >= 2 {
                                cut.points += l.len();
                                lines.push(l);
                            }
                        }
                    }
                    if lines.is_empty() {
                        continue;
                    }
                    CutGeometry::Lines(lines)
                }
            };
            cut.features.push(CutFeature { feature: f.source_index, class: f.class, geometry });
        }
        cut
    }
}

/// Tiny-polygon reduction, as tippecanoe does by default (its
/// `--no-tiny-polygon-reduction` turns it off; here
/// `TileIndexOptions::tiny_polygon_reduction`, per source
/// `SourceOptions::no_tiny_polygon_reduction`): a polygon whose outer ring
/// is under one tolerance-square of area at a zoom is not simply dropped,
/// as geojson-vt (and so MapLibre's GeoJSON source) drops it. Its area
/// accumulates, in painter's order across the tile, and each time the total
/// reaches one tolerance-square the polygon that tipped it is drawn as one
/// square of that area, near a point inside it, carrying its own feature's
/// class. A city of block groups each far under a pixel therefore keeps its
/// coverage (to within one square a tile) instead of vanishing, and its
/// colour mix is kept statistically: a polygon's chance of carrying a
/// square is proportional to its area.
///
/// Polygons tiny BEFORE simplification take part, as in tippecanoe. A
/// larger ring that Douglas-Peucker reduces to nothing takes part only when
/// it is compact (`MAX_COLLAPSED_SIDE`), one square at a time on the same
/// grid; a sliver is dropped exactly as geojson-vt drops it.
///
/// The squares depend on the order the tile's polygons arrive in, so a
/// live edit (`TileIndex::applied`) that adds, removes or resizes a tiny
/// polygon re-deals which later polygons in the same tile carry squares
/// (never how many beyond one, nor where the area is): the tile is
/// re-cut anyway, and the result is exactly what a rebuild would cut.
///
/// Without it, a z3-z5 view of the US draws every metro as bare basemap:
/// over Chicago at z3 the geojson-vt rule keeps 9 of 3,766 block groups,
/// 5% of their area.
///
/// The squares are one tolerance (0.375 px) on a side, far under
/// `tile_plan::OUTLINE_MIN_EXTENT_PX`, so they never get a
/// `fill-outline-color` hairline: no invented boundaries.
struct TinyArea {
    /// One square's side (whole tile units) and area.
    side: i32,
    area: f64,
    acc: f64,
    /// The last whole cell inside the tile's own box, per axis: squares
    /// never straddle the edge, where the draw's clip would cut them.
    last_cell: i32,
    /// The one-square grid cells already drawn in this tile. Squares snap
    /// to that grid and step aside to a free cell nearby, so a dense city's
    /// squares tile the ground instead of piling up: without it ~1/3 of a
    /// dense core samples as bare basemap (random 0.4 px squares overlap
    /// as a Poisson process: uncovered = e^-density).
    /// A bitset over the tile's `(last_cell + 1)^2` cells (~230 KB at
    /// extent 4096), allocated at the first square.
    taken: Vec<u64>,
}

impl TinyArea {
    fn new(tolerance: f64, extent: u32) -> Self {
        let side = tolerance.round().max(1.0) as i32;
        let last_cell = (extent as i32 / side - 1).max(0);
        Self { side, area: f64::from(side * side), acc: 0.0, last_cell, taken: Vec::new() }
    }

    /// A ring that simplification emptied although it was larger than one
    /// tolerance-square counts only when its bounding box is at most this
    /// many tolerances on a side: a small compact polygon (a city block
    /// group of ~1-3 squares at z3-z5, the next size class up from tiny),
    /// whose squares land within a cell or two of it. A longer ring -- a
    /// sliver, a river bank -- is dropped exactly as geojson-vt drops it,
    /// rather than moved into squares away from where it lies.
    const MAX_COLLAPSED_SIDE: f64 = 3.0;

    /// Adds one ring's area (tile units squared); how many squares it
    /// tipped, each one tolerance-square taken off the total. A tiny ring
    /// tips at most one; a compact collapsed ring (at most 9 squares of
    /// bounding box) a few.
    fn add(&mut self, area: f64) -> usize {
        self.acc += area;
        let n = (self.acc / self.area).floor();
        self.acc -= n * self.area;
        n as usize
    }

    /// The closed square ring for a tipping polygon with interior point `c`:
    /// the grid cell holding `c`, or the nearest free one within two cells.
    fn place(&mut self, c: [f64; 2]) -> Vec<[i32; 2]> {
        let row = self.last_cell as usize + 1;
        if self.taken.is_empty() {
            self.taken = vec![0u64; (row * row).div_ceil(64)];
        }
        let bit = |q: (i32, i32)| q.1 as usize * row + q.0 as usize;
        let s = f64::from(self.side);
        let clamp = |v: f64| ((v / s).floor() as i32).clamp(0, self.last_cell);
        let home = (clamp(c[0]), clamp(c[1]));
        let mut cell = home;
        'search: for r in 0..=2i32 {
            let mut best: Option<((i32, i32), i32)> = None;
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs().max(dy.abs()) != r {
                        continue;
                    }
                    let q = (home.0 + dx, home.1 + dy);
                    let d = dx * dx + dy * dy;
                    let within = (0..=self.last_cell).contains(&q.0) && (0..=self.last_cell).contains(&q.1);
                    if within && self.taken[bit(q) / 64] & (1 << (bit(q) % 64)) == 0 && best.is_none_or(|(_, bd)| d < bd) {
                        best = Some((q, d));
                    }
                }
            }
            if let Some((q, _)) = best {
                cell = q;
                break 'search;
            }
        }
        let b = bit(cell);
        self.taken[b / 64] |= 1 << (b % 64);
        let (x0, y0) = (cell.0 * self.side, cell.1 * self.side);
        let (x1, y1) = (x0 + self.side, y0 + self.side);
        vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]
    }
}

/// Candidate interior points of closed ring `r` (tile units), cheaply
/// and best first: its vertex centroid if that is inside `r`, then the
/// midpoints of the chords along the centroid's row, longest first. `r`
/// is a SAMPLE of the true ring, so the caller checks each candidate
/// against the full ring (`TileIndex::inside_ring`).
fn interior_candidates(r: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let Some(n) = r.len().checked_sub(1).filter(|&n| n >= 3) else { return Vec::new() };
    let (sx, sy) = r[..n].iter().fold((0.0, 0.0), |(x, y), p| (x + p[0], y + p[1]));
    let c = [sx / n as f64, sy / n as f64];
    let mut xs: Vec<f64> = Vec::new();
    for w in r.windows(2) {
        let (a, b) = (w[0], w[1]);
        if (a[1] > c[1]) != (b[1] > c[1]) {
            xs.push(a[0] + (c[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0]));
        }
    }
    xs.sort_by(f64::total_cmp);
    let mut out = Vec::with_capacity(1 + xs.len() / 2);
    if xs.iter().filter(|&&x| x < c[0]).count() % 2 == 1 {
        out.push(c); // even-odd: the centroid is inside the sample
    }
    let mut chords: Vec<&[f64; 2]> = xs.as_chunks::<2>().0.iter().collect();
    chords.sort_by(|p, q| (q[1] - q[0]).total_cmp(&(p[1] - p[0])));
    out.extend(chords.iter().map(|p| [(p[0] + p[1]) * 0.5, c[1]]));
    out
}

/// A point inside the FULL ring `full` (fixed point; `to_tile` maps a
/// vertex to tile units, the units of `sample` and of the answer): the
/// first of `sample`'s [`interior_candidates`] that `full` holds (one
/// even-odd pass each), else the midpoint of `full`'s own longest chord
/// along the sample centroid's row (one more pass). `None` for a ring the
/// row misses.
fn checked_interior(sample: &[[f64; 2]], full: &[[u32; 2]], to_tile: impl Fn([u32; 2]) -> [f64; 2]) -> Option<[f64; 2]> {
    // Crossings of `full` with the row `y`, sorted.
    let crossings = |y: f64| -> Vec<f64> {
        let mut xs = Vec::new();
        let n = full.len();
        for i in 0..n {
            let (a, b) = (to_tile(full[i]), to_tile(full[(i + 1) % n]));
            if (a[1] > y) != (b[1] > y) {
                xs.push(a[0] + (y - a[1]) / (b[1] - a[1]) * (b[0] - a[0]));
            }
        }
        xs.sort_by(f64::total_cmp);
        xs
    };
    let candidates = interior_candidates(sample);
    for &p in &candidates {
        if crossings(p[1]).iter().filter(|&&x| x < p[0]).count() % 2 == 1 {
            return Some(p);
        }
    }
    let y = candidates.first().map(|p| p[1])?;
    crossings(y).as_chunks::<2>().0.iter().max_by(|p, q| (p[1] - p[0]).total_cmp(&(q[1] - q[0]))).map(|p| [(p[0] + p[1]) * 0.5, y])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::OverlayStore;

    /// The seam test below uses a wobbly circle, whose vertices never land
    /// on an exact negative half, so it cannot tell `floor(v + 0.5)` from
    /// `f64::round`. This can.
    #[test]
    fn rounding_is_shift_invariant_at_negative_halves() {
        for v in [-0.5, -1.5, -2.5, -1023.5, -0.49999, 0.5, 1.5, 4095.5] {
            assert_eq!(round_half_up(v) + 4096, round_half_up(v + 4096.0), "{v}");
            assert_eq!(round_half_up(v) - 4096, round_half_up(v - 4096.0), "{v}");
        }
        assert_eq!(round_half_up(-0.5), 0);
        assert_eq!(round_half_up(-1.5), -1);
        // What `f64::round` would have done: -0.5 -> -1 but 4095.5 -> 4096.
        assert_ne!((-0.5f64).round() as i32 + 4096, (4095.5f64).round() as i32);
    }

    fn features_of(json: &str) -> std::sync::Arc<Vec<crate::overlay::OverlayFeature>> {
        let mut store = OverlayStore::new();
        store.replace(json, 3).expect("valid GeoJSON");
        store.source_snapshot(crate::overlay::DEFAULT_SOURCE).unwrap().features.clone()
    }

    fn polygon_json(rings: &[Vec<[f64; 2]>]) -> String {
        let rings: Vec<String> = rings
            .iter()
            .map(|r| format!("[{}]", r.iter().map(|p| format!("[{},{}]", p[0], p[1])).collect::<Vec<_>>().join(",")))
            .collect();
        format!(
            r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","id":1,"properties":{{}},"geometry":{{"type":"Polygon","coordinates":[{}]}}}}]}}"#,
            rings.join(",")
        )
    }

    /// A 3,000-vertex wobbly circle around (lon, lat), `r` degrees.
    fn wobbly(lon: f64, lat: f64, r: f64) -> Vec<[f64; 2]> {
        let n = 3000;
        let mut ring: Vec<[f64; 2]> = (0..n)
            .map(|i| {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                let rr = r * (1.0 + 0.05 * (a * 37.0).sin());
                [lon + rr * a.cos(), lat + rr * a.sin()]
            })
            .collect();
        ring.push(ring[0]);
        ring
    }

    fn build(json: &str) -> TileIndex {
        let f = features_of(json);
        TileIndex::build(&f, &vec![0u16; f.len()], TileIndexOptions::default(), &|| false).unwrap()
    }

    /// Area of a cut, clipped to the tile's OWN box (what `TileClip`
    /// lets through), in world units squared ([0,1] Mercator).
    fn own_area(cut: &TileCut) -> f64 {
        let e = 4096.0;
        let scale = e * f64::from(1u32 << cut.tile.z);
        let mut total = 0.0;
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for f in &cut.features {
            let CutGeometry::Polygons(polys) = &f.geometry else { continue };
            for poly in polys {
                for (k, ring) in poly.iter().enumerate() {
                    let r: Vec<[f64; 2]> = ring.iter().map(|p| [f64::from(p[0]), f64::from(p[1])]).collect();
                    clip::clip_ring(&r, 0.0, e, 0, &mut a);
                    clip::clip_ring(&a, 0.0, e, 1, &mut b);
                    let s = clip::shoelace(&b).abs() / (scale * scale);
                    total += if k == 0 { s } else { -s };
                }
            }
        }
        total
    }

    fn tiles_at(z: u8) -> impl Iterator<Item = TileId> {
        let n = 1u32 << z;
        (0..n).flat_map(move |x| (0..n).map(move |y| TileId { z, x, y }))
    }

    #[test]
    fn the_cut_tiles_cover_the_source_area_within_tolerance() {
        let ring = wobbly(-98.0, 39.0, 6.0);
        let index = build(&polygon_json(std::slice::from_ref(&ring)));
        // Source area and perimeter in [0,1] Mercator units.
        let proj: Vec<[f64; 2]> = ring
            .iter()
            .map(|p| {
                let (x, y) = crate::overlay::geom::lon_lat_to_world(p[0], p[1]);
                [x / WORLD, y / WORLD]
            })
            .collect();
        let source = clip::shoelace(&proj).abs();
        let perimeter: f64 = proj.windows(2).map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1])).sum();
        for z in [2u8, 4, 6, 9] {
            let got: f64 = tiles_at(z).filter(|t| !index.is_empty_at(*t)).map(|t| own_area(&index.cut(t))).sum();
            // DP bound: every kept edge is within tol of the original, plus half a unit of rounding.
            let tol_world = (3.0 + 0.5) / (4096.0 * f64::from(1u32 << z));
            assert!((got - source).abs() <= perimeter * tol_world, "z{z}: {got} vs {source}");
        }
    }

    #[test]
    fn at_max_zoom_every_vertex_is_kept_exactly() {
        // Small enough to sit inside one z18 tile (~0.0014 degrees).
        let ring = wobbly(-77.0300, 38.8900, 0.0002);
        let index = build(&polygon_json(std::slice::from_ref(&ring)));
        assert_eq!(index.max_zoom(), 18, "MapLibre's GeoJSON maxzoom");
        let (wx, wy) = crate::overlay::geom::lon_lat_to_world(ring[0][0], ring[0][1]);
        let z = index.max_zoom();
        let n = f64::from(1u32 << z);
        let tile = TileId { z, x: (wx / WORLD * n) as u32, y: (wy / WORLD * n) as u32 };
        let cut = index.cut(tile);
        let CutGeometry::Polygons(polys) = &cut.features[0].geometry else { panic!() };
        let got = &polys[0][0];
        // Reference: the same fixed-point rounding, straight from lon/lat.
        let expect: Vec<[i32; 2]> = ring
            .iter()
            .map(|p| {
                let f = to_fixed((p[0], p[1]));
                let s = 4096.0 * n / 4_294_967_296.0;
                [
                    (f64::from(f[0]) * s - f64::from(tile.x) * 4096.0 + 0.5).floor() as i32,
                    (f64::from(f[1]) * s - f64::from(tile.y) * 4096.0 + 0.5).floor() as i32,
                ]
            })
            .fold(Vec::new(), |mut v, p| {
                if v.last() != Some(&p) {
                    v.push(p)
                }
                v
            });
        assert_eq!(got, &expect, "tolerance 0 at max zoom: no vertex dropped or moved");
        // And exact against the f64 projection itself, not only against the
        // fixed-point copy: at z18 an MVT unit is 1/8 px, and the 2^32 grid
        // is four steps per unit, so every vertex lands within half a unit
        // plus one grid step of where the source puts it. (Overzoomed from
        // z14, the same vertex could be 8 units, 1 px, off.)
        for p in &ring {
            let f = to_fixed((p[0], p[1]));
            let s = 4096.0 * n / 4_294_967_296.0;
            let q = [
                (f64::from(f[0]) * s - f64::from(tile.x) * 4096.0 + 0.5).floor(),
                (f64::from(f[1]) * s - f64::from(tile.y) * 4096.0 + 0.5).floor(),
            ];
            assert!(got.contains(&[q[0] as i32, q[1] as i32]), "every source vertex is in the z18 cut");
            let (x, y) = crate::overlay::geom::lon_lat_to_world(p[0], p[1]);
            let tx = x / WORLD * n * 4096.0 - f64::from(tile.x) * 4096.0;
            let ty = y / WORLD * n * 4096.0 - f64::from(tile.y) * 4096.0;
            assert!(
                (q[0] - tx).abs() <= 0.75 && (q[1] - ty).abs() <= 0.75,
                "z18 vertex {q:?} vs the source's ({tx:.3}, {ty:.3})"
            );
        }
    }

    #[test]
    fn neighbours_agree_exactly_in_their_shared_buffer() {
        let index = build(&polygon_json(&[wobbly(-98.0, 39.0, 6.0)]));
        let z = 5;
        for x in 0..(1u32 << z) - 1 {
            for y in 0..1u32 << z {
                let (l, r) = (TileId { z, x, y }, TileId { z, x: x + 1, y });
                if index.is_empty_at(l) || index.is_empty_at(r) {
                    continue;
                }
                let strip = |cut: &TileCut, lo: f64, hi: f64, shift: f64| -> Vec<Vec<[i64; 2]>> {
                    let mut out = Vec::new();
                    let (mut a, mut b) = (Vec::new(), Vec::new());
                    for f in &cut.features {
                        let CutGeometry::Polygons(p) = &f.geometry else { continue };
                        for ring in p.iter().flatten() {
                            let r: Vec<[f64; 2]> =
                                ring.iter().map(|q| [f64::from(q[0]) + shift, f64::from(q[1])]).collect();
                            clip::clip_ring(&r, lo, hi, 0, &mut a);
                            clip::clip_ring(&a, 0.0, 4096.0, 1, &mut b);
                            if b.len() >= 4 {
                                let mut v: Vec<[i64; 2]> =
                                    b.iter().map(|q| [(q[0] * 1e6) as i64, (q[1] * 1e6) as i64]).collect();
                                v.pop();
                                let m = v.iter().enumerate().min_by_key(|(_, q)| **q).unwrap().0;
                                v.rotate_left(m);
                                out.push(v);
                            }
                        }
                    }
                    out.sort();
                    out
                };
                // The strip 256 units either side of the shared edge, in L's frame.
                let a = strip(&index.cut(l), 4096.0 - 256.0, 4096.0 + 256.0, 0.0);
                let b = strip(&index.cut(r), 4096.0 - 256.0, 4096.0 + 256.0, 4096.0);
                assert_eq!(a, b, "seam between {l:?} and {r:?}");
            }
        }
    }

    #[test]
    fn a_hole_crossing_a_tile_edge_stays_a_hole() {
        let outer = vec![[-100.0, 30.0], [-80.0, 30.0], [-80.0, 45.0], [-100.0, 45.0], [-100.0, 30.0]];
        let hole = vec![[-92.0, 36.0], [-92.0, 40.0], [-88.0, 40.0], [-88.0, 36.0], [-92.0, 36.0]];
        let index = build(&polygon_json(&[outer, hole]));
        let with_hole: f64 = tiles_at(4).filter(|t| !index.is_empty_at(*t)).map(|t| own_area(&index.cut(t))).sum();
        let index2 =
            build(&polygon_json(&[vec![[-100.0, 30.0], [-80.0, 30.0], [-80.0, 45.0], [-100.0, 45.0], [-100.0, 30.0]]]));
        let without: f64 = tiles_at(4).filter(|t| !index2.is_empty_at(*t)).map(|t| own_area(&index2.cut(t))).sum();
        assert!(with_hole < without * 0.97, "the hole's area is subtracted in every tile it crosses");
    }

    #[test]
    fn a_dropped_outer_ring_drops_its_holes() {
        // A ring far below 0.375 px at z0, with a hole: nothing at z0.
        let outer = wobbly(-77.0, 38.9, 0.0005);
        let hole = wobbly(-77.0, 38.9, 0.0002);
        let index = build(&polygon_json(&[outer, hole]));
        let cut = index.cut(TileId { z: 0, x: 0, y: 0 });
        assert!(cut.features.is_empty(), "outer ring under one tolerance-pixel: polygon and hole both dropped");
        // At max zoom the same polygon, hole included, is back.
        let (wx, wy) = crate::overlay::geom::lon_lat_to_world(-77.0, 38.9);
        let n = f64::from(1u32 << 14);
        let t = TileId { z: 14, x: (wx / WORLD * n) as u32, y: (wy / WORLD * n) as u32 };
        let CutGeometry::Polygons(p) = &index.cut(t).features[0].geometry else { panic!() };
        assert_eq!(p[0].len(), 2, "outer + hole");
    }

    /// `n` x `n` tiny squares (`side` degrees, `step` apart) from (lon, lat),
    /// every other one of class 1, as a FeatureCollection.
    fn tiny_grid(lon: f64, lat: f64, n: usize, step: f64, side: impl Fn(usize) -> f64) -> (String, Vec<u16>) {
        let mut feats = Vec::new();
        let mut classes = Vec::new();
        for i in 0..n * n {
            let (x, y) = (lon + (i % n) as f64 * step, lat + (i / n) as f64 * step);
            let d = side(i);
            feats.push(format!(
                r#"{{"type":"Feature","id":{i},"properties":{{}},"geometry":{{"type":"Polygon","coordinates":[[[{x},{y}],[{},{y}],[{},{}],[{x},{}],[{x},{y}]]]}}}}"#,
                x + d,
                x + d,
                y + d,
                y + d
            ));
            classes.push((i % 2) as u16);
        }
        (format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, feats.join(",")), classes)
    }

    fn source_area(json: &str) -> f64 {
        features_of(json)
            .iter()
            .map(|f| {
                let OverlayGeometry::Polygon(p) = &f.geometry else { return 0.0 };
                let proj: Vec<[f64; 2]> = p[0][0]
                    .iter()
                    .map(|&(lon, lat)| {
                        let (x, y) = crate::overlay::geom::lon_lat_to_world(lon, lat);
                        [x / WORLD, y / WORLD]
                    })
                    .collect();
                clip::shoelace(&proj).abs()
            })
            .sum()
    }

    #[test]
    fn a_city_of_sub_tolerance_polygons_keeps_its_area_as_squares() {
        // 40 x 40 block groups ~0.01 degrees apart around Chicago: each far
        // under one tolerance-pixel at z3, together ~0.4 x 0.4 degrees.
        let (json, _) = tiny_grid(-88.0, 41.6, 40, 0.01, |_| 0.008);
        let index = build(&json);
        let source = source_area(&json);
        for z in [2u8, 3, 4] {
            let cuts: Vec<TileCut> = tiles_at(z).filter(|t| !index.is_empty_at(*t)).map(|t| index.cut(t)).collect();
            let got: f64 = cuts.iter().map(own_area).sum();
            let n: usize = cuts.iter().map(|c| c.features.len()).sum();
            assert!(n > 0 && n < 1600, "z{z}: {n} stand-in squares, not every feature and not none");
            // They tile the ground rather than pile up: no two share a corner.
            let mut corners = std::collections::HashSet::new();
            for c in &cuts {
                for f in &c.features {
                    let CutGeometry::Polygons(p) = &f.geometry else { panic!() };
                    for poly in p {
                        assert!(corners.insert((c.tile, poly[0][0])), "z{z}: two squares at {:?}", poly[0][0]);
                    }
                }
            }
            // Under one square's worth of remainder per tile, nothing more.
            let square = (3.0 / (4096.0 * f64::from(1u32 << z))).powi(2);
            assert!(got <= source && source - got < square * cuts.len() as f64, "z{z}: kept {got}, source {source}");
            // One tolerance on a side: under the outline's level of detail,
            // so a square never gets a fill-outline hairline.
            assert!(3 < crate::overlay::tile_plan::outline_min_extent_units(TileId { z, x: 0, y: 0 }, 18));
        }
    }

    #[test]
    fn a_city_on_a_tile_corner_is_counted_once() {
        // Centred on (-90, 0): a corner of z2 tile (0,1) and of every
        // z3/z4 tile meeting there, so each square could be claimed by up
        // to four tiles' buffers. Emitted twice, the area would double.
        let (json, _) = tiny_grid(-90.2, -0.2, 40, 0.01, |_| 0.008);
        let index = build(&json);
        let source = source_area(&json);
        for z in [2u8, 3, 4] {
            let cuts: Vec<TileCut> = tiles_at(z).filter(|t| !index.is_empty_at(*t)).map(|t| index.cut(t)).collect();
            let with_squares = cuts.iter().filter(|c| !c.features.is_empty()).count();
            assert_eq!(with_squares, 4, "z{z}: the city straddles four tiles");
            let got: f64 = cuts.iter().map(own_area).sum();
            let square = (3.0 / (4096.0 * f64::from(1u32 << z))).powi(2);
            assert!(got <= source && source - got < square * 4.0, "z{z}: kept {got}, source {source}");
        }
    }

    #[test]
    fn a_sliver_simplified_away_gets_no_square() {
        // ~3 degrees long, ~0.01 wide: hundreds of tolerance-squares of
        // area at z3, but Douglas-Peucker reduces it to a back-and-forth
        // line. Not tiny by area, so it is dropped as geojson-vt drops it,
        // and never becomes one big square.
        let mut ring: Vec<[f64; 2]> = (0..=60).map(|i| [-100.0 + 0.05 * f64::from(i), 40.0]).collect();
        ring.extend((0..=60).rev().map(|i| [-100.0 + 0.05 * f64::from(i), 40.01]));
        ring.push(ring[0]);
        let f = features_of(&polygon_json(&[ring]));
        let index = TileIndex::build(&f, &[0], TileIndexOptions::default(), &|| false).unwrap();
        let off = TileIndexOptions { tiny_polygon_reduction: false, ..TileIndexOptions::default() };
        let plain = TileIndex::build(&f, &[0], off, &|| false).unwrap();
        // It really is simplified away at z3 (not dropped for size), and drawn by z10.
        assert!(tiles_at(3).all(|t| plain.cut(t).features.is_empty()));
        let (wx, wy) = crate::overlay::geom::lon_lat_to_world(-98.5, 40.005);
        let t10 = TileId { z: 10, x: (wx / WORLD * 1024.0) as u32, y: (wy / WORLD * 1024.0) as u32 };
        assert!(!plain.cut(t10).features.is_empty());
        // Exactly what the reduction-off (geojson-vt) index cuts: no square
        // of any size, anywhere.
        for z in [2u8, 3, 4] {
            assert_same_cuts(&index, &plain, &tiles_at(z).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_compact_ring_simplified_away_keeps_its_area_next_to_itself() {
        // ~4 x 4 units at z3 (~1.8 tolerance-squares): not tiny, but its
        // corners sit under one tolerance off the diagonal, so
        // Douglas-Peucker empties it. Compact, so it keeps one square.
        let (lon, lat, w, h) = (-98.0, 40.0, 0.044, 0.034);
        let ring = vec![[lon, lat], [lon + w, lat], [lon + w, lat + h], [lon, lat + h], [lon, lat]];
        let f = features_of(&polygon_json(&[ring]));
        let off = TileIndexOptions { tiny_polygon_reduction: false, ..TileIndexOptions::default() };
        let plain = TileIndex::build(&f, &[0], off, &|| false).unwrap();
        let index = TileIndex::build(&f, &[0], TileIndexOptions::default(), &|| false).unwrap();
        let (wx, wy) = crate::overlay::geom::lon_lat_to_world(lon + w / 2.0, lat + h / 2.0);
        let (px, py) = (wx / WORLD * 8.0 * 4096.0, wy / WORLD * 8.0 * 4096.0);
        let t = TileId { z: 3, x: (px / 4096.0) as u32, y: (py / 4096.0) as u32 };
        assert!(plain.cut(t).features.is_empty(), "geojson-vt empties it at z3");
        let cut = index.cut(t);
        let CutGeometry::Polygons(p) = &cut.features[0].geometry else { panic!() };
        assert_eq!(p.len(), 1, "one square: 16 units of area, 9 a square");
        let sq = &p[0][0];
        let (sx, sy) = (f64::from(sq[0][0]) + 1.5, f64::from(sq[0][1]) + 1.5);
        let (cx, cy) = (px - f64::from(t.x) * 4096.0, py - f64::from(t.y) * 4096.0);
        assert!((sx - cx).abs() <= 6.0 && (sy - cy).abs() <= 6.0, "square at ({sx},{sy}), ring centre ({cx},{cy})");
        assert_eq!((sq[2][0] - sq[0][0], sq[2][1] - sq[0][1]), (3, 3), "one tolerance cell");
    }

    #[test]
    fn a_sampled_interior_point_in_a_notch_falls_back_to_one_inside_the_ring() {
        // A 100 x 100 square with a notch (x 30..100, y 40..60) cut from its
        // right side: the outer boundary densely sampled, the notch by its
        // four corners only, so a 16-vertex sample skips the notch and its
        // centroid (~50, 50) sits in it.
        let mut ring: Vec<[u32; 2]> = Vec::new();
        ring.extend((0..=100).map(|x| [x, 0]));
        ring.extend((1..=40).map(|y| [100, y]));
        ring.extend([[30, 40], [30, 60], [100, 60]]);
        ring.extend((61..=100).map(|y| [100, y]));
        ring.extend((0..100).rev().map(|x| [x, 100]));
        ring.extend((1..100).rev().map(|y| [0, y]));
        ring.push([0, 0]);
        let step = ring.len() / 16;
        let mut sample: Vec<[f64; 2]> = ring.iter().step_by(step).map(|p| [f64::from(p[0]), f64::from(p[1])]).collect();
        sample.push(sample[0]);
        let inside = |p: [f64; 2]| {
            let n = ring.len();
            (0..n).filter(|&i| {
                let (a, b) = (ring[i].map(f64::from), ring[(i + 1) % n].map(f64::from));
                (a[1] > p[1]) != (b[1] > p[1]) && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0])
            })
            .count()
                % 2
                == 1
        };
        let first = interior_candidates(&sample)[0];
        assert!(!inside(first), "the sample's own best point {first:?} is in the notch");
        let p = checked_interior(&sample, &ring, |q| q.map(f64::from)).expect("a point the full ring holds");
        assert!(inside(p), "{p:?}");
        assert!(!(30.0..=100.0).contains(&p[0]) || !(40.0..=60.0).contains(&p[1]), "{p:?} is not in the notch");
    }

    #[test]
    fn the_reduction_can_be_turned_off() {
        let (json, _) = tiny_grid(-88.0, 41.6, 40, 0.01, |_| 0.008);
        let f = features_of(&json);
        let off = TileIndexOptions { tiny_polygon_reduction: false, ..TileIndexOptions::default() };
        let index = TileIndex::build(&f, &vec![0; f.len()], off, &|| false).unwrap();
        assert!(tiles_at(3).all(|t| index.cut(t).features.is_empty()), "geojson-vt: every sub-tolerance polygon dropped");
    }

    #[test]
    fn an_applied_edit_cuts_the_squares_a_rebuild_cuts() {
        let (json, classes) = tiny_grid(-88.0, 41.6, 40, 0.01, |i| 0.004 + 0.00001 * (i % 97) as f64);
        let before = features_of(&json);
        let index = TileIndex::build(&before, &classes, TileIndexOptions::default(), &|| false).unwrap();
        let (more, _) = tiny_grid(-87.9, 41.7, 10, 0.013, |_| 0.007);
        let more = features_of(&more);
        let mut after: Vec<OverlayFeature> = before.to_vec();
        let mut class_after = classes.clone();
        let mut edits: Vec<IndexEdit<'_>> = Vec::new();
        for row in (2..1600).step_by(7) {
            class_after[row] = 1 - class_after[row];
            edits.push(IndexEdit::Upsert { row: row as u32, class: class_after[row], geometry: None });
        }
        for row in (3..1600).step_by(11) {
            after[row].geometry = OverlayGeometry::Empty;
            class_after[row] = NO_CLASS;
            edits.push(IndexEdit::Remove(row as u32));
        }
        for row in (5..1600).step_by(13).filter(|r| class_after[*r] != NO_CLASS) {
            after[row].geometry = more[row % 100].geometry.clone();
            edits.push(IndexEdit::Upsert { row: row as u32, class: class_after[row], geometry: Some(&more[row % 100].geometry) });
        }
        for (k, m) in more.iter().enumerate() {
            let row = 1600 + k;
            let mut f = m.clone();
            f.index = row;
            after.push(f);
            class_after.push((k % 2) as u16);
            edits.push(IndexEdit::Upsert { row: row as u32, class: class_after[row], geometry: Some(&m.geometry) });
        }
        let (applied, _) = index.applied(&edits);
        let fresh = TileIndex::build(&after, &class_after, TileIndexOptions::default(), &|| false).unwrap();
        let tiles: Vec<TileId> = [2u8, 3, 4, 5].into_iter().flat_map(tiles_at).filter(|t| !fresh.is_empty_at(*t)).collect();
        assert!(tiles.iter().any(|t| !fresh.cut(*t).features.is_empty()), "squares present");
        assert_same_cuts(&applied, &fresh, &tiles);
    }

    #[test]
    fn a_stand_in_square_carries_one_feature_picked_by_area() {
        // Interleaved: class 0 three times the area of class 1, so class 0
        // should carry ~3/4 of the squares (tippecanoe's accumulator: the
        // feature that tips the running area over one square carries it).
        let (json, classes) = tiny_grid(-88.0, 41.6, 60, 0.006, |i| if i % 2 == 0 { 0.005 * 3f64.sqrt() } else { 0.005 });
        let f = features_of(&json);
        let index = TileIndex::build(&f, &classes, TileIndexOptions::default(), &|| false).unwrap();
        let (mut c0, mut c1) = (0usize, 0usize);
        for t in tiles_at(3).filter(|t| !index.is_empty_at(*t)) {
            for cf in index.cut(t).features {
                assert_eq!(cf.class, classes[cf.feature as usize], "a square is its carrier's class");
                if cf.class == 0 {
                    c0 += 1;
                } else {
                    c1 += 1;
                }
            }
        }
        let share = c0 as f64 / (c0 + c1) as f64;
        assert!(c0 + c1 > 50 && (share - 0.75).abs() < 0.08, "class 0 carries {c0}, class 1 {c1}: share {share}");
    }

    #[test]
    fn an_antimeridian_ring_is_clamped_not_smeared() {
        // A ring given as 179.9 .. 180.0 must not produce geometry at x ≈ 0.
        let ring = vec![[179.9, 51.0], [180.0, 51.0], [180.0, 52.0], [179.9, 52.0], [179.9, 51.0]];
        let index = build(&polygon_json(&[ring]));
        assert!(index.is_empty_at(TileId { z: 3, x: 0, y: 2 }));
        assert!(!index.is_empty_at(TileId { z: 3, x: 7, y: 2 }));
    }

    #[test]
    fn the_eager_index_splits_only_where_points_exceed_the_budget() {
        let opts = TileIndexOptions { index_max_points: 1000, ..TileIndexOptions::default() };
        let f = features_of(&polygon_json(&[wobbly(-98.0, 39.0, 6.0)]));
        let index = TileIndex::build(&f, &[0], opts, &|| false).unwrap();
        assert!(index.indexed_tiles() > 1 && index.indexed_tiles() <= 1 + 4 + 16 + 64 + 256 + 1024);
        // A z9 cut below the eager index (the lazy drill-down) finds the ring's
        // edge: the tile holding the ring's easternmost vertex (-92 deg, 39 deg).
        let (wx, wy) = crate::overlay::geom::lon_lat_to_world(-98.0 + 6.0, 39.0);
        let n = f64::from(1u32 << 9);
        let edge = TileId { z: 9, x: (wx / WORLD * n) as u32, y: (wy / WORLD * n) as u32 };
        assert!(!index.cut(edge).features.is_empty());
        assert!(index.cut(TileId { z: 9, x: 0, y: 0 }).features.is_empty());
    }

    /// `n` wobbly squares on a jittered grid over the US, some with a hole.
    fn grid_json(n: usize, seed: u64) -> String {
        let mut x = seed;
        let mut rnd = || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 11) as f64 / (1u64 << 53) as f64
        };
        let side = (n as f64).sqrt().ceil() as usize;
        let mut feats = Vec::new();
        for i in 0..n {
            let (gx, gy) = ((i % side) as f64, (i / side) as f64);
            let lon = -120.0 + 50.0 * (gx + 0.2 * rnd()) / side as f64;
            let lat = 28.0 + 20.0 * (gy + 0.2 * rnd()) / side as f64;
            let r = 0.3 * 50.0 / side as f64 * (0.5 + rnd());
            let ring = wobbly(lon, lat, r);
            let ring: Vec<[f64; 2]> = ring.iter().step_by(40).copied().chain(std::iter::once(ring[0])).collect();
            let fmt = |r: &[[f64; 2]]| format!("[{}]", r.iter().map(|p| format!("[{},{}]", p[0], p[1])).collect::<Vec<_>>().join(","));
            let mut rings = vec![fmt(&ring)];
            if i % 7 == 0 {
                let hole = [[lon - r * 0.2, lat - r * 0.2], [lon - r * 0.2, lat + r * 0.2], [lon + r * 0.2, lat + r * 0.2], [lon + r * 0.2, lat - r * 0.2], [lon - r * 0.2, lat - r * 0.2]];
                rings.push(fmt(&hole));
            }
            feats.push(format!(
                r#"{{"type":"Feature","id":{i},"properties":{{}},"geometry":{{"type":"Polygon","coordinates":[{}]}}}}"#,
                rings.join(",")
            ));
        }
        format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, feats.join(","))
    }

    /// Every non-empty tile a fresh index has, at `zooms`, sampled.
    fn sample_tiles(index: &TileIndex, zooms: &[u8]) -> Vec<TileId> {
        let mut out = Vec::new();
        for &z in zooms {
            let n = 1u32 << z;
            // US: lon -125..-65, lat 25..50.
            let (x0, y0) = to_tile(-126.0, 51.0, n);
            let (x1, y1) = to_tile(-64.0, 24.0, n);
            let step = ((x1 - x0 + 1) * (y1 - y0 + 1) / 400).max(1);
            let mut k = 0u32;
            for x in x0..=x1 {
                for y in y0..=y1 {
                    k += 1;
                    if k.is_multiple_of(step) && !index.is_empty_at(TileId { z, x, y }) {
                        out.push(TileId { z, x, y });
                    }
                }
            }
        }
        out
    }

    fn to_tile(lon: f64, lat: f64, n: u32) -> (u32, u32) {
        let (wx, wy) = crate::overlay::geom::lon_lat_to_world(lon, lat);
        ((wx / WORLD * f64::from(n)) as u32, (wy / WORLD * f64::from(n)) as u32)
    }

    fn assert_same_cuts(a: &TileIndex, b: &TileIndex, tiles: &[TileId]) {
        for &t in tiles {
            assert_eq!(a.cut(t), b.cut(t), "cut of {t:?}");
        }
    }

    #[test]
    fn an_applied_index_cuts_what_a_rebuild_cuts() {
        let opts = TileIndexOptions { index_max_points: 2000, ..TileIndexOptions::default() };
        let before = features_of(&grid_json(900, 1));
        let classes: Vec<u16> = (0..before.len()).map(|i| (i % 5) as u16).collect();
        let index = TileIndex::build(&before, &classes, opts, &|| false).unwrap();
        assert!(index.indexed_tiles() > 20, "the eager index splits: {}", index.indexed_tiles());
        // Replacement geometry (moved) and added features, from another grid.
        let other = features_of(&grid_json(900, 7));
        let mut after: Vec<OverlayFeature> = before.to_vec();
        let mut class_after = classes.clone();
        let mut edits: Vec<IndexEdit<'_>> = Vec::new();
        // Re-class 100 (class-only).
        for row in (3..900).step_by(9) {
            class_after[row] = 4 - class_after[row] % 5;
            edits.push(IndexEdit::Upsert { row: row as u32, class: class_after[row], geometry: None });
        }
        // Move 60: new geometry at the same row.
        for row in (5..900).step_by(15) {
            after[row].geometry = other[(row * 7 + 1) % 900].geometry.clone();
            edits.push(IndexEdit::Upsert { row: row as u32, class: class_after[row], geometry: Some(&other[(row * 7 + 1) % 900].geometry) });
        }
        // Remove 50.
        for row in (1..900).step_by(18) {
            after[row].geometry = OverlayGeometry::Empty;
            class_after[row] = NO_CLASS;
            edits.push(IndexEdit::Remove(row as u32));
        }
        // Add 100 at the end.
        for k in 0..100 {
            let row = 900 + k;
            let mut f = other[k * 3].clone();
            f.index = row;
            after.push(f);
            class_after.push((k % 5) as u16);
            edits.push(IndexEdit::Upsert { row: row as u32, class: class_after[row], geometry: Some(&other[k * 3].geometry) });
        }
        let (applied, delta) = index.applied(&edits);
        assert!(delta.lists_rewritten > 0 && !delta.resplit);
        assert_eq!(applied.segments(), 2);
        assert_eq!(applied.dead(), 50);
        let fresh = TileIndex::build(&after, &class_after, opts, &|| false).unwrap();
        let tiles = sample_tiles(&fresh, &[0, 3, 5, 8, 12]);
        assert!(tiles.len() > 50, "{} tiles sampled", tiles.len());
        assert_same_cuts(&applied, &fresh, &tiles);
        // z18: a few tiles on moved and added features' first vertex.
        let z18: Vec<TileId> = after[900..]
            .iter()
            .chain(std::iter::once(&after[5]))
            .filter_map(|f| match &f.geometry {
                OverlayGeometry::Polygon(p) => {
                    let (x, y) = to_tile(p[0][0][0].0, p[0][0][0].1, 1 << 18);
                    Some(TileId { z: 18, x, y })
                }
                _ => None,
            })
            .collect();
        assert_same_cuts(&applied, &fresh, &z18);
        // Compaction: one segment, no tombstones, the same cuts.
        let compact = applied.compacted(None);
        assert_eq!((compact.segments(), compact.dead()), (1, 0));
        assert_same_cuts(&compact, &fresh, &tiles);
        // Compaction with the rows renumbered, as the store compacts them.
        let mut remap = vec![u32::MAX; after.len()];
        let mut kept = Vec::new();
        let mut kept_classes = Vec::new();
        for (row, f) in after.iter().enumerate() {
            if !matches!(f.geometry, OverlayGeometry::Empty) {
                remap[row] = kept.len() as u32;
                let mut f = f.clone();
                f.index = kept.len();
                kept.push(f);
                kept_classes.push(class_after[row]);
            }
        }
        let renumbered = applied.compacted(Some(&remap));
        let fresh2 = TileIndex::build(&kept, &kept_classes, opts, &|| false).unwrap();
        assert_same_cuts(&renumbered, &fresh2, &tiles);
        // Hit-testing reads the moved and added geometry, and not the removed.
        let hit_at = |ix: &TileIndex, row: usize, f: &OverlayFeature| {
            let OverlayGeometry::Polygon(p) = &f.geometry else { return None };
            let (lon, lat) = p[0][0][0];
            let (w, n, e, s) = (lon, lat, lon, lat);
            let _ = (w, n, e, s);
            // The ring's centroid-ish: the mean of its vertices.
            let m = p[0][0].iter().fold((0.0, 0.0), |a, q| (a.0 + q.0, a.1 + q.1));
            let c = (m.0 / p[0][0].len() as f64, m.1 / p[0][0].len() as f64);
            let world = crate::overlay::geom::lon_lat_to_world(c.0, c.1);
            Some(ix.feature_hit(row as u32, world, 0.0))
        };
        assert_eq!(hit_at(&applied, 5, &after[5]), Some(true), "moved: hits at its new place");
        assert_eq!(hit_at(&applied, 5, &before[5]), Some(false), "moved: misses at its old place");
        assert_eq!(hit_at(&applied, 950, &after[950]), Some(true), "added");
        assert_eq!(hit_at(&applied, 1, &before[1]), Some(false), "removed");
        assert!(applied.feature_bbox_lon_lat(1).is_none());
    }

    #[test]
    fn an_insert_below_an_indexed_row_resplits_and_stays_exact() {
        let before = features_of(&grid_json(100, 3));
        let mut classes = vec![0u16; 100];
        classes[10] = NO_CLASS; // row 10 not indexed at build
        let index = TileIndex::build(&before, &classes, TileIndexOptions::default(), &|| false).unwrap();
        let (applied, delta) = index.applied(&[IndexEdit::Upsert { row: 10, class: 0, geometry: Some(&before[10].geometry) }]);
        assert!(delta.resplit);
        let fresh = TileIndex::build(&before, &[0u16; 100], TileIndexOptions::default(), &|| false).unwrap();
        assert_same_cuts(&applied, &fresh, &sample_tiles(&fresh, &[0, 4, 7]));
    }

    #[test]
    fn cancel_stops_the_build() {
        let f = features_of(&polygon_json(&[wobbly(-98.0, 39.0, 6.0)]));
        assert!(TileIndex::build(&f, &[0], TileIndexOptions::default(), &|| true).is_none());
    }
}
