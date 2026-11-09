//! The tile stage: parts → 3″ categorical cell pyramid (levels 0–9, data
//! zooms 9–0) + z12 part tiles, streamed from `parts.spill` in latitude
//! bands so memory stays bounded.
//!
//! **Why bands work.** A part's level-0 cell is the 3″ cell holding its
//! centroid; a level-`k` cell is level-0 cells `(row >> k, col >> k)` summed
//! (2×2 per level, exact on a lat/lon grid anchored at 90°N/180°W). A band
//! of level-0 rows aligned to [`ROWS_PER_BAND`] = 2^9 therefore holds every
//! child of every level-`k ≤ 9` cell it touches, so each cell of each level
//! is complete inside exactly one band. A *tile* can span bands, so its
//! cells wait in `pending` until the bands have passed its southern edge,
//! then it is encoded once. Part tiles (z12, owned by the tile holding the
//! part's centroid) wait the same way.
//!
//! Every tile is decoded back (`codec_cat::decode_any`) right after
//! encoding and its group sums are what the stage reports (MG-R6): the
//! check is on the bytes the reader will see, not on the pre-encode maps.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{bail, Context, Result};
use gpui_maps::geo::TileId;
use gpui_maps::overlay::dotgrid::codec_cat::{decode_any, decode_any_at, encode_cells, encode_parts, MAX_PARTS_PER_TILE};
use gpui_maps::overlay::dotgrid::grid::{self, CellWindow};
use gpui_maps::overlay::dotgrid::parts::Part;
use gpui_maps::overlay::dotgrid::GridDescriptor;

use crate::spill::{for_each_part, SpillPart};
use crate::tiger::unproject;

/// Level-0 (3″) cells per degree.
const CELLS_PER_DEG: f64 = 1200.0;
/// Pyramid levels (3″ … 1536″) and level 0's data zoom (C7).
pub const LEVELS: u8 = 10;
pub const MAX_DATA_ZOOM: u8 = 9;
/// The part tiles' zoom (C7 `part_zoom`).
pub const PART_ZOOM: u8 = 12;
/// Level-0 rows per band: 2^(LEVELS-1), so every level's cells are whole
/// inside one aligned band.
pub const ROWS_PER_BAND: u32 = 1 << (LEVELS - 1);
/// Level-0 rows on the globe (180° × 1200) and the band count covering them.
pub const ROWS_TOTAL: u32 = 180 * 1200;
pub const BANDS: usize = ROWS_TOTAL.div_ceil(ROWS_PER_BAND) as usize;

/// The archive metadata, verbatim per ruling C7.
pub const METADATA: &str = r#"{"dotgrid":{"version":1,"base_arcsec":3,"origin_lat":90,"origin_lon":-180,"levels":10,"max_data_zoom":9,"epochs":[2020]},"race":{"kind":"race-dots","groups":8,"part_zoom":12,"crossover":9.72},"attribution":"U.S. Census Bureau"}"#;

/// The grid the metadata describes.
pub fn descriptor() -> GridDescriptor {
    GridDescriptor {
        base_arcsec: 3.0,
        origin_lat: 90.0,
        origin_lon: -180.0,
        levels: LEVELS,
        max_data_zoom: MAX_DATA_ZOOM,
        epochs: vec![2020],
        attribution: "U.S. Census Bureau".to_string(),
    }
}

/// The level-0 `(row, col)` holding a point in world-grid quanta.
pub fn level0_cell(p: [f64; 2]) -> (u32, u32) {
    let (lat, lon) = unproject(p);
    let row = ((90.0 - lat) * CELLS_PER_DEG).floor().clamp(0.0, f64::from(ROWS_TOTAL - 1)) as u32;
    let col = ((lon + 180.0) * CELLS_PER_DEG).floor().clamp(0.0, 360.0 * CELLS_PER_DEG - 1.0) as u32;
    (row, col)
}

/// The band of a part (its centroid's level-0 row / [`ROWS_PER_BAND`]).
pub fn band_of(p: &SpillPart) -> usize {
    (level0_cell(p.centroid()).0 / ROWS_PER_BAND) as usize
}

/// The z12 tile owning a part: the tile holding its centroid.
///
/// The contract for readers (review F4):
/// - **One owner.** `x = floor(cx) >> 19`, `y = floor(cy) >> 19`, with the
///   centroid computed in f64 from the i32 vertices. No part is in two tiles.
/// - **Overhang.** `split_part` caps a part's bbox side at `MAX_PART_EXTENT`
///   = 2^17 quanta, so a vertex is at most 2/3 · 2^17 from the centroid. A
///   part therefore reaches at most **1/6 of a z12 tile** past its owner's
///   edge on each side.
/// - **Encoding.** Vertices are tile-relative zig-zag i32s, so they may be
///   negative or above 2^19. Readers must not clamp them.
/// - **Rendering.** Never clip a part tile to its own rect. Cull z12 tiles
///   on bounds grown by 1/6 of a tile, or load the neighbour ring.
/// - **Crossover.** The level-0 cell and the part tile are both keyed on
///   this same centroid.
pub fn part_tile(p: &SpillPart) -> TileId {
    let c = p.centroid();
    let shift = 31 - u32::from(PART_ZOOM);
    TileId { z: PART_ZOOM, x: (c[0].floor() as i64 >> shift) as u32, y: (c[1].floor() as i64 >> shift) as u32 }
}

/// The last level-0 row a centroid in part tile `t` can fall in: the row of
/// the tile's southern edge (`level0_cell` is monotone in y).
fn part_tile_last_row(t: TileId) -> u32 {
    let shift = 31 - u32::from(t.z);
    level0_cell([0.0, ((i64::from(t.y) + 1) << shift) as f64]).0
}

/// What one level (or the part tiles) holds, from the decoded tiles.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LevelStats {
    /// Pyramid level, or `GridTileKey::PART_LEVEL` (255) for the part tiles.
    pub level: u8,
    pub tiles: u64,
    /// Cells (or parts) stored.
    pub records: u64,
    pub max_records_in_a_tile: u64,
    /// Encoded bytes.
    pub bytes: u64,
    pub sums: [u64; 8],
}

impl LevelStats {
#[cfg_attr(not(test), allow(dead_code))] // tests, and the readers of later tasks
    pub fn sum_groups(&self) -> [u64; 8] {
        self.sums
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PyramidStats {
    pub levels: Vec<LevelStats>,
    pub parts: LevelStats,
    /// Level-0 group sums per state FIPS (binned before cells merge across
    /// state lines).
    pub level0_by_state: BTreeMap<u8, [u64; 8]>,
}

pub type TileSink<'a> = dyn FnMut(TileId, &[u8]) -> Result<()> + 'a;

pub struct Pyramid<'a> {
    d: GridDescriptor,
    windows: HashMap<(u8, TileId), CellWindow>,
    pending_cells: HashMap<(u8, TileId), Vec<(u32, [u32; 8])>>,
    pending_parts: HashMap<TileId, Vec<SpillPart>>,
    rows_done: u32,
    sink: &'a mut TileSink<'a>,
    pub stats: PyramidStats,
}

fn add(acc: &mut [u32; 8], c: &[u32; 8]) {
    for (a, &v) in acc.iter_mut().zip(c) {
        *a += v;
    }
}

impl<'a> Pyramid<'a> {
    pub fn new(sink: &'a mut TileSink<'a>) -> Self {
        let levels = (0..LEVELS).map(|level| LevelStats { level, ..Default::default() }).collect();
        Self {
            d: descriptor(),
            windows: HashMap::new(),
            pending_cells: HashMap::new(),
            pending_parts: HashMap::new(),
            rows_done: 0,
            sink,
            stats: PyramidStats {
                levels,
                parts: LevelStats { level: gpui_maps::overlay::dotgrid::GridTileKey::PART_LEVEL, ..Default::default() },
                level0_by_state: BTreeMap::new(),
            },
        }
    }

    fn window(&mut self, level: u8, tile: TileId) -> CellWindow {
        let d = &self.d;
        *self.windows.entry((level, tile)).or_insert_with(|| grid::cell_window(d, level, tile))
    }

    /// Adds every part whose centroid's level-0 row is in
    /// `[rows_done, row_end)` (`row_end` a multiple of [`ROWS_PER_BAND`]),
    /// then encodes every tile those rows completed.
    pub fn add_band(&mut self, parts: &[SpillPart], row_end: u32) -> Result<()> {
        if row_end % ROWS_PER_BAND != 0 || row_end <= self.rows_done {
            bail!("band end {row_end} is not an aligned row past {}", self.rows_done);
        }
        let mut cells: HashMap<(u32, u32), [u32; 8]> = HashMap::new();
        for p in parts {
            let (row, col) = level0_cell(p.centroid());
            if row < self.rows_done || row >= row_end {
                bail!("part {} of block {} (row {row}) is outside the band [{}, {row_end})", p.part, p.geoid, self.rows_done);
            }
            let c = p.part().c;
            add(cells.entry((row, col)).or_default(), &c);
            let s = self.stats.level0_by_state.entry(p.state).or_default();
            for (a, &v) in s.iter_mut().zip(&c) {
                *a += u64::from(v);
            }
            self.pending_parts.entry(part_tile(p)).or_default().push(*p);
        }
        for level in 0..LEVELS {
            let mut next: HashMap<(u32, u32), [u32; 8]> = HashMap::new();
            for (&(row, col), c) in &cells {
                let tile = grid::tile_of_cell(&self.d, level, row, col)
                    .with_context(|| format!("level {level} cell ({row}, {col}) lies beyond the Mercator limit"))?;
                let w = self.window(level, tile);
                if row < w.row0 || row >= w.row0 + w.rows || col < w.col0 || col >= w.col0 + w.cols {
                    bail!("level {level} cell ({row}, {col}) is outside its own tile {tile:?}'s window");
                }
                self.pending_cells.entry((level, tile)).or_default().push((grid::local_index(&w, row, col), *c));
                if level + 1 < LEVELS {
                    add(next.entry((row >> 1, col >> 1)).or_default(), c);
                }
            }
            cells = next;
        }
        self.rows_done = row_end;
        self.flush(row_end)
    }

    /// Encodes the tiles every later band leaves untouched.
    fn flush(&mut self, row_end: u32) -> Result<()> {
        let windows = &self.windows;
        let mut done: Vec<(u8, TileId)> = self
            .pending_cells
            .keys()
            .filter(|(level, tile)| {
                let w = windows[&(*level, *tile)];
                (u64::from(w.row0 + w.rows) << level) <= u64::from(row_end)
            })
            .copied()
            .collect();
        done.sort_by_key(|&(l, t)| (l, t.z, t.y, t.x));
        for key in done {
            let cells = self.pending_cells.remove(&key).unwrap();
            self.emit_cells(key.0, key.1, cells)?;
        }
        let mut done: Vec<TileId> = self.pending_parts.keys().filter(|t| part_tile_last_row(**t) < row_end).copied().collect();
        done.sort_by_key(|t| (t.y, t.x));
        for tile in done {
            let parts = self.pending_parts.remove(&tile).unwrap();
            self.emit_parts(tile, parts)?;
        }
        Ok(())
    }

    fn emit_cells(&mut self, level: u8, tile: TileId, mut cells: Vec<(u32, [u32; 8])>) -> Result<()> {
        cells.sort_unstable_by_key(|c| c.0);
        if cells.windows(2).any(|w| w[0].0 == w[1].0) {
            bail!("level {level} tile {tile:?} received one cell from two bands");
        }
        let w = self.windows[&(level, tile)];
        let bytes = encode_cells(level, w.rows * w.cols, &cells);
        let decoded = decode_any(&bytes).map_err(|e| anyhow::anyhow!("level {level} tile {tile:?} does not decode: {e:?}"))?;
        let st = &mut self.stats.levels[usize::from(level)];
        for c in &decoded.cats {
            for (s, &v) in st.sums.iter_mut().zip(c) {
                *s += u64::from(v);
            }
        }
        st.tiles += 1;
        st.records += decoded.cats.len() as u64;
        st.max_records_in_a_tile = st.max_records_in_a_tile.max(decoded.cats.len() as u64);
        st.bytes += bytes.len() as u64;
        (self.sink)(tile, &bytes)
    }

    fn emit_parts(&mut self, tile: TileId, recs: Vec<SpillPart>) -> Result<()> {
        // encode_parts panics past this (Task 4 contract): refuse loudly here.
        if recs.len() as u64 > u64::from(MAX_PARTS_PER_TILE) {
            bail!("part tile {tile:?} holds {} parts, past MAX_PARTS_PER_TILE {MAX_PARTS_PER_TILE}", recs.len());
        }
        let parts: Vec<Part> = recs.iter().map(SpillPart::part).collect();
        let bytes = encode_parts(tile, &parts);
        let decoded = decode_any_at(&bytes, tile).map_err(|e| anyhow::anyhow!("part tile {tile:?} does not decode: {e:?}"))?;
        let st = &mut self.stats.parts;
        for p in &decoded.parts {
            for (s, &v) in st.sums.iter_mut().zip(&p.counts) {
                *s += u64::from(v);
            }
        }
        st.tiles += 1;
        st.records += decoded.parts.len() as u64;
        st.max_records_in_a_tile = st.max_records_in_a_tile.max(decoded.parts.len() as u64);
        st.bytes += bytes.len() as u64;
        (self.sink)(tile, &bytes)
    }

    /// Encodes everything still pending.
    pub fn finish(mut self) -> Result<PyramidStats> {
        self.flush(u32::MAX)?;
        debug_assert!(self.pending_cells.is_empty() && self.pending_parts.is_empty());
        Ok(self.stats)
    }
}

/// Groups consecutive bands into passes of at most `max_parts` parts each
/// (a single band over the cap is its own pass). Returns `[lo, hi)` band
/// ranges covering every band with parts.
pub fn plan_passes(hist: &[u64], max_parts: u64) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut lo = None;
    let mut acc = 0u64;
    for (b, &n) in hist.iter().enumerate() {
        if n == 0 {
            continue;
        }
        match lo {
            None => {
                lo = Some(b);
                acc = n;
            }
            Some(l) if acc + n > max_parts => {
                out.push((l, b));
                lo = Some(b);
                acc = n;
            }
            Some(_) => acc += n,
        }
    }
    if let Some(l) = lo {
        let last = hist.iter().rposition(|&n| n > 0).unwrap();
        out.push((l, last + 1));
    }
    out
}

/// The whole tile stage over a spill file: one read of the spill per pass.
pub fn build_from_spill(spill: &Path, hist: &[u64], max_parts: u64, sink: &mut TileSink<'_>) -> Result<PyramidStats> {
    // Re-borrow the sink with the Pyramid's lifetime.
    let mut sink_ref = |t: TileId, b: &[u8]| sink(t, b);
    let mut pyr = Pyramid::new(&mut sink_ref);
    let passes = plan_passes(hist, max_parts);
    for (i, &(lo, hi)) in passes.iter().enumerate() {
        let started = std::time::Instant::now();
        let mut parts = Vec::with_capacity(hist[lo..hi].iter().sum::<u64>() as usize);
        let f = File::open(spill).with_context(|| format!("opening {}", spill.display()))?;
        for_each_part(BufReader::with_capacity(1 << 22, f), |p| {
            let b = band_of(&p);
            if (lo..hi).contains(&b) {
                parts.push(p);
            }
            Ok(())
        })?;
        let expect: u64 = hist[lo..hi].iter().sum();
        if parts.len() as u64 != expect {
            bail!("pass {i}: {} parts in bands {lo}..{hi}, the histogram says {expect}", parts.len());
        }
        pyr.add_band(&parts, hi as u32 * ROWS_PER_BAND)?;
        eprintln!("tiles: pass {}/{} bands {lo}..{hi}, {} parts, {:.1}s", i + 1, passes.len(), parts.len(), started.elapsed().as_secs_f64());
    }
    pyr.finish()
}

/// Group sums of a slice of parts.
#[cfg_attr(not(test), allow(dead_code))] // tests, and the readers of later tasks
pub fn sum_groups(parts: &[SpillPart]) -> [u64; 8] {
    let mut s = [0u64; 8];
    for p in parts {
        for (a, &v) in s.iter_mut().zip(&p.c) {
            *a += u64::from(v);
        }
    }
    s
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use gpui_maps::overlay::dotgrid::hash::pcg;

    /// `n` seeded parts across the contiguous US (small triangles, 1–400
    /// people each), plus a few in Alaska and Hawaii.
    pub fn synthetic_parts(n: usize, seed: u32) -> Vec<SpillPart> {
        let mut s = pcg(seed);
        let mut next = || {
            s = pcg(s);
            s
        };
        (0..n)
            .map(|i| {
                let (lon, lat) = match i % 50 {
                    0 => (-150.0 + f64::from(next() % 1000) / 100.0, 61.0 + f64::from(next() % 500) / 100.0),
                    1 => (-157.9 + f64::from(next() % 100) / 100.0, 21.3 + f64::from(next() % 50) / 100.0),
                    _ => (-124.0 + f64::from(next() % 5500) / 100.0, 25.0 + f64::from(next() % 2300) / 100.0),
                };
                let a = crate::tiger::project(lon, lat);
                let (w, h) = (i64::from(next() % 20_000) + 1, i64::from(next() % 20_000) + 1);
                let mut c = [0u16; 8];
                for g in c.iter_mut() {
                    *g = if next() % 3 == 0 { (next() % 50) as u16 } else { 0 };
                }
                c[(next() % 8) as usize] += 1;
                SpillPart { geoid: i as u64, part: 0, state: (i % 56) as u8, v: [[a[0] as i32, a[1] as i32], [(a[0] + w) as i32, a[1] as i32], [a[0] as i32, (a[1] + h) as i32]], c, ..Default::default() }
            })
            .collect()
    }

    /// Every tile of a build, in emission order, and its stats.
    pub fn build_pyramid_in_bands(parts: &[SpillPart], max_parts: u64) -> (Vec<(TileId, Vec<u8>)>, PyramidStats) {
        let mut hist = vec![0u64; BANDS];
        for p in parts {
            hist[band_of(p)] += 1;
        }
        let mut tiles = Vec::new();
        let mut sink = |t: TileId, b: &[u8]| {
            tiles.push((t, b.to_vec()));
            Ok(())
        };
        let mut pyr = Pyramid::new(&mut sink);
        for (lo, hi) in plan_passes(&hist, max_parts) {
            let band: Vec<SpillPart> = parts.iter().filter(|p| (lo..hi).contains(&band_of(p))).copied().collect();
            pyr.add_band(&band, hi as u32 * ROWS_PER_BAND).unwrap();
        }
        let stats = pyr.finish().unwrap();
        (tiles, stats)
    }

    pub fn build_pyramid(parts: &[SpillPart]) -> Vec<LevelStats> {
        build_pyramid_in_bands(parts, u64::MAX).1.levels
    }

    #[test]
    fn every_level_sums_to_the_same_integers() {
        let parts = synthetic_parts(10_000, 7); // seeded, in code
        let pyr = build_pyramid(&parts);
        let want = sum_groups(&parts);
        assert_eq!(pyr.len(), 10);
        for lvl in &pyr {
            assert_eq!(lvl.sum_groups(), want, "level {}", lvl.level);
        }
        // Coarser levels hold fewer cells, and level 9 is one z0 tile.
        assert!(pyr.windows(2).all(|w| w[1].records <= w[0].records));
        assert_eq!(pyr[9].tiles, 1);
    }

    #[test]
    fn banded_passes_write_the_same_tiles_as_one_pass() {
        let parts = synthetic_parts(10_000, 11);
        let (mut one, s1) = build_pyramid_in_bands(&parts, u64::MAX);
        let (mut many, s2) = build_pyramid_in_bands(&parts, 700);
        assert!(plan_passes(&{
            let mut h = vec![0u64; BANDS];
            for p in &parts {
                h[band_of(p)] += 1;
            }
            h
        }, 700)
        .len() > 5);
        one.sort_by_key(|t| (t.0.z, t.0.x, t.0.y));
        many.sort_by_key(|t| (t.0.z, t.0.x, t.0.y));
        assert_eq!(one, many);
        assert_eq!(s1, s2);
        assert_eq!(s1.parts.sum_groups(), sum_groups(&parts));
        assert_eq!(s1.parts.records, parts.len() as u64);
    }

    #[test]
    fn part_tiles_hold_their_centroids_parts_and_decode() {
        let parts = synthetic_parts(2_000, 3);
        let (tiles, _) = build_pyramid_in_bands(&parts, u64::MAX);
        let mut n = 0;
        for (t, bytes) in tiles.iter().filter(|t| t.0.z == PART_ZOOM) {
            let tile = decode_any(bytes).unwrap();
            for p in &tile.parts {
                let c = [(p.a[0] + p.b[0] + p.c[0]) / 3.0 * 4096.0, (p.a[1] + p.b[1] + p.c[1]) / 3.0 * 4096.0];
                assert_eq!((c[0].floor() as u32, c[1].floor() as u32), (t.x, t.y));
            }
            n += tile.parts.len();
        }
        assert_eq!(n, 2_000);
    }

    #[test]
    fn plan_passes_covers_every_populated_band_once() {
        let hist = [0, 5, 5, 0, 20, 1, 0, 0, 3, 0];
        assert_eq!(plan_passes(&hist, 10), vec![(1, 4), (4, 5), (5, 9)]);
        assert_eq!(plan_passes(&hist, 1000), vec![(1, 9)]);
        assert!(plan_passes(&[0, 0], 10).is_empty());
    }

    #[test]
    fn the_metadata_is_the_c7_string_and_matches_the_descriptor() {
        let v: serde_json::Value = serde_json::from_str(METADATA).unwrap();
        let d = descriptor();
        assert_eq!(v["dotgrid"]["levels"], u64::from(d.levels));
        assert_eq!(v["dotgrid"]["max_data_zoom"], u64::from(d.max_data_zoom));
        assert_eq!(v["race"]["part_zoom"], u64::from(PART_ZOOM));
        assert_eq!(v["attribution"], "U.S. Census Bureau");
        assert_eq!(v["race"]["kind"], "race-dots");
        assert_eq!(v["race"]["groups"], 8);
        assert_eq!(v["race"]["crossover"], 9.72);
        // Ruling C7's literal, verbatim.
        assert_eq!(
            METADATA,
            r#"{"dotgrid":{"version":1,"base_arcsec":3,"origin_lat":90,"origin_lon":-180,"levels":10,"max_data_zoom":9,"epochs":[2020]},"race":{"kind":"race-dots","groups":8,"part_zoom":12,"crossover":9.72},"attribution":"U.S. Census Bureau"}"#
        );
        assert_eq!(grid::base_data_zoom(3.0), MAX_DATA_ZOOM);
        assert!((grid::crossover_zoom(3.0) - 9.72).abs() < 0.005);
    }
}
