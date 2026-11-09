//! Level by level: load each data tile's spill, sort by local index and sum
//! duplicates, encode it, and push every cell's value into its PARENT cell
//! `(row / 2, col / 2)` at the next level's spill. A 2×2 sum is exact on a
//! lat/lon grid anchored at 90°N/180°W, so level `k` holds the same people as
//! level 0, less whatever a coarser cell's centre pushes past the Mercator
//! limit (counted per level, never silent).

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;
use gpui_maps::geo::TileId;
use gpui_maps::overlay::dotgrid::{codec, grid, GridDescriptor};
use rayon::prelude::*;

use crate::archive::EncodedStore;
use crate::bin_spill::{read_records, Spill};

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LevelStats {
    pub tiles: u64,
    /// Cells with a value > 0 after summing.
    pub cells: u64,
    /// Cells the codec keeps (value rounds to at least 0.1).
    pub cells_encoded: u64,
    pub sum: f64,
    /// What a reader decodes: Σ round(10 v) / 10 over the kept cells.
    pub sum_encoded: f64,
    pub max_cells_in_a_tile: u64,
    pub max_window_cells: u64,
    /// Encoded bytes of the tiles written (before any PMTiles dedup).
    pub bytes: u64,
    pub dropped_cells: u64,
    pub dropped_population: f64,
}

impl LevelStats {
    pub fn merge(mut self, o: Self) -> Self {
        self.tiles += o.tiles;
        self.cells += o.cells;
        self.cells_encoded += o.cells_encoded;
        self.sum += o.sum;
        self.sum_encoded += o.sum_encoded;
        self.max_cells_in_a_tile = self.max_cells_in_a_tile.max(o.max_cells_in_a_tile);
        self.max_window_cells = self.max_window_cells.max(o.max_window_cells);
        self.bytes += o.bytes;
        self.dropped_cells += o.dropped_cells;
        self.dropped_population += o.dropped_population;
        self
    }
}

/// Sorts by index and sums duplicates (in f64), dropping non-positive sums.
pub fn merge(mut records: Vec<(u32, f32)>) -> Vec<(u32, f64)> {
    records.sort_unstable_by_key(|r| r.0);
    let mut out: Vec<(u32, f64)> = Vec::with_capacity(records.len());
    for (idx, v) in records {
        match out.last_mut() {
            Some(last) if last.0 == idx => last.1 += v as f64,
            _ => out.push((idx, v as f64)),
        }
    }
    out.retain(|c| c.1 > 0.0);
    out
}

/// Parent records of `cells` (one level-`level` tile), grouped by the
/// level-`level + 1` tile each parent's centre falls in, plus the cells and
/// people whose parent lies beyond the Mercator limit.
pub fn parents(
    d: &GridDescriptor,
    level: u8,
    tile: TileId,
    cells: &[(u32, f64)],
) -> (HashMap<TileId, Vec<(u32, f32)>>, u64, f64) {
    let w = grid::cell_window(d, level, tile);
    let up = level + 1;
    let mut windows: HashMap<TileId, grid::CellWindow> = HashMap::new();
    let mut out: HashMap<TileId, Vec<(u32, f32)>> = HashMap::new();
    let (mut dropped_cells, mut dropped_pop) = (0u64, 0f64);
    for &(idx, v) in cells {
        let (row, col) = grid::row_col(&w, idx);
        let (pr, pc) = (row / 2, col / 2);
        let Some(pt) = grid::tile_of_cell(d, up, pr, pc) else {
            dropped_cells += 1;
            dropped_pop += v;
            continue;
        };
        let pw = *windows.entry(pt).or_insert_with(|| grid::cell_window(d, up, pt));
        out.entry(pt).or_default().push((grid::local_index(&pw, pr, pc), v as f32));
    }
    (out, dropped_cells, dropped_pop)
}

/// Sees every level's merged cells of one tile (value > 0, unrounded) as the
/// pyramid makes them: `--load-clickhouse` streams them into the table, so
/// the table holds the converter's own pyramid, not a second computation.
pub type CellSink<'a> = dyn Fn(u8, TileId, &grid::CellWindow, &[(u32, f64)]) -> Result<()> + Sync + 'a;

/// Runs levels `start..`: encodes into `store` and feeds parents back into
/// `spill`. A level's spill files are deleted only once the whole level is
/// done, its parents are on disk and `on_level(level, stats)` has returned
/// (the caller checkpoints there), so a killed run resumes at a level
/// boundary. `stats` holds every level's stats so far (level 0's
/// `dropped_*` seeded from the bin pass); the finished stats are returned.
pub fn build(
    d: &GridDescriptor,
    epoch: u16,
    spill: &Mutex<Spill>,
    store: &EncodedStore,
    sink: Option<&CellSink>,
    start: u8,
    mut stats: Vec<LevelStats>,
    mut on_level: impl FnMut(u8, &[LevelStats]) -> Result<()>,
) -> Result<Vec<LevelStats>> {
    stats.resize(d.levels as usize, LevelStats::default());
    for level in start..d.levels {
        let tiles = spill.lock().unwrap().finish_level(level)?;
        let has_parent = level + 1 < d.levels;
        let (level_stats, dropped_up) = tiles
            .par_iter()
            .map(|&tile| -> Result<(LevelStats, (u64, f64))> {
                let path = spill.lock().unwrap().path_of(level, tile);
                let cells = merge(read_records(&path)?);
                let w = grid::cell_window(d, level, tile);
                if let Some(sink) = sink {
                    sink(level, tile, &w, &cells)?;
                }
                let window_cells = w.rows * w.cols;
                let as_f32: Vec<(u32, f32)> = cells.iter().map(|&(i, v)| (i, v as f32)).collect();
                let bytes = codec::encode(level, epoch, window_cells, &as_f32);
                let mut s = LevelStats { tiles: 1, max_window_cells: window_cells as u64, ..Default::default() };
                for &(_, v) in &as_f32 {
                    let deci = (v as f64 * 10.0).round();
                    s.cells += 1;
                    s.sum += v as f64;
                    if deci > 0.0 {
                        s.cells_encoded += 1;
                        s.sum_encoded += deci / 10.0;
                    }
                }
                s.max_cells_in_a_tile = s.cells_encoded;
                if s.cells_encoded > 0 {
                    s.bytes = bytes.len() as u64;
                    store.put(tile, &bytes)?;
                }
                let mut dropped = (0, 0.0);
                if has_parent {
                    let (ups, dc, dp) = parents(d, level, tile, &cells);
                    dropped = (dc, dp);
                    let mut sp = spill.lock().unwrap();
                    for (pt, recs) in ups {
                        sp.append(level + 1, pt, &recs)?;
                    }
                }
                Ok((s, dropped))
            })
            .try_reduce(
                || (LevelStats::default(), (0, 0.0)),
                |a, b| Ok((a.0.merge(b.0), (a.1 .0 + b.1 .0, a.1 .1 + b.1 .1))),
            )?;
        let seeded = std::mem::take(&mut stats[level as usize]);
        stats[level as usize] = seeded.merge(level_stats);
        if has_parent {
            stats[level as usize + 1].dropped_cells += dropped_up.0;
            stats[level as usize + 1].dropped_population += dropped_up.1;
            spill.lock().unwrap().finish_level(level + 1)?;
        }
        on_level(level, &stats)?;
        spill.lock().unwrap().remove_level(level)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d30() -> GridDescriptor {
        GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: 6,
            epochs: vec![2025],
            attribution: String::new(),
        }
    }

    #[test]
    fn the_parent_of_four_children_is_their_sum() {
        let d = d30();
        // A 4×4 block of level-0 cells just south-east of (0°, 0°): rows
        // 10800..10804, cols 21600..21604, all in one z6 tile.
        let (r0, c0) = (10_800u32, 21_600u32);
        let tile = grid::tile_of_cell(&d, 0, r0, c0).unwrap();
        let w = grid::cell_window(&d, 0, tile);
        let mut cells = Vec::new();
        for dr in 0..4 {
            for dc in 0..4 {
                assert_eq!(grid::tile_of_cell(&d, 0, r0 + dr, c0 + dc), Some(tile));
                cells.push((grid::local_index(&w, r0 + dr, c0 + dc), (1 + dr * 4 + dc) as f32));
            }
        }
        // Children arrive split and duplicated across appends, as a spill does.
        let mut recs = cells.clone();
        recs.push((cells[0].0, 0.5));
        let merged = merge(recs);
        assert_eq!(merged.len(), 16);
        assert_eq!(merged[0].1, 1.5);

        let (ups, dropped, _) = parents(&d, 0, tile, &merged);
        assert_eq!(dropped, 0);
        let mut got: Vec<(u32, u32, f64)> = Vec::new();
        for (pt, recs) in ups {
            let pw = grid::cell_window(&d, 1, pt);
            for (idx, v) in merge(recs) {
                let (r, c) = grid::row_col(&pw, idx);
                got.push((r, c, v));
            }
        }
        got.sort_by_key(|g| (g.0, g.1));
        let v = |dr: u32, dc: u32| (1 + dr * 4 + dc) as f64 + if (dr, dc) == (0, 0) { 0.5 } else { 0.0 };
        let want: Vec<(u32, u32, f64)> = (0..2)
            .flat_map(|pr| (0..2).map(move |pc| (pr, pc)))
            .map(|(pr, pc)| {
                let s = v(2 * pr, 2 * pc) + v(2 * pr, 2 * pc + 1) + v(2 * pr + 1, 2 * pc) + v(2 * pr + 1, 2 * pc + 1);
                (r0 / 2 + pr, c0 / 2 + pc, s)
            })
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn a_parent_beyond_the_mercator_limit_is_dropped_and_counted() {
        let d = d30();
        // Some level has a row whose cell centre is inside the Mercator
        // limit while its parent's (half a coarser cell further north) is not.
        let (level, row) = (0..d.levels - 1)
            .flat_map(|l| (0..4000u32).map(move |r| (l, r)))
            .find(|&(l, r)| grid::tile_of_cell(&d, l, r, 0).is_some() && grid::tile_of_cell(&d, l + 1, r / 2, 0).is_none())
            .expect("a straddling row exists");
        let tile = grid::tile_of_cell(&d, level, row, 0).unwrap();
        let w = grid::cell_window(&d, level, tile);
        let (ups, dc, dp) = parents(&d, level, tile, &[(grid::local_index(&w, row, 0), 3.0)]);
        assert_eq!((ups.len(), dc, dp), (0, 1, 3.0));
    }
}
