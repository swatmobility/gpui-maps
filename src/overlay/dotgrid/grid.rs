//! Cell geometry: where a GHSL grid cell sits in lon/lat, which Mercator
//! data tile its centre falls in, and the row/col window a tile covers.

use super::{GridDescriptor, GridTileKey};
use crate::geo::{tile_bounds_lonlat, TileId};
use crate::overlay::geom::lon_lat_to_world;
use crate::render::WORLD_SIZE;

/// Mercator latitude limit -- the same value `camera_control` clamps a
/// camera target to (a centre beyond it has no tile to stand on).
/// Re-exported rather than redeclared: one source of truth for the same
/// physical constant.
pub use crate::camera_control::MAX_MERCATOR_LAT;

/// log2(40_075_016.686 / 512 / 30.87 / arcsec) rounded down: the data zoom
/// where a level-0 cell is ≈ 1 px. `40_075_016.686` is the equatorial
/// circumference in metres; `512` is the tile pixel size
/// (`crate::geo::TILE_SIZE`); `30.87` m is a 1″ cell's width at the
/// equator. 9 for 3″, 6 for 30″.
pub fn base_data_zoom(base_arcsec: f64) -> u8 {
    (40_075_016.686 / 512.0 / 30.87 / base_arcsec).log2().floor() as u8
}

/// The continuous crossover zoom zc = log2(2535.5 / base_arcsec) used by
/// level selection (9.72 for 3″, 6.40 for 30″). `2535.5` is the same
/// constant `base_data_zoom` computes exactly
/// (`40_075_016.686 / 512.0 / 30.87`) before flooring.
pub fn crossover_zoom(base_arcsec: f64) -> f64 {
    (2535.5 / base_arcsec).log2()
}

/// Arc-seconds of a cell at `level` (level 0 is the descriptor's own
/// `base_arcsec`; each level up doubles it, like a mipmap).
pub fn cell_arcsec(d: &GridDescriptor, level: u8) -> f64 {
    assert_pyramid_level(level);
    d.base_arcsec * (1u64 << level) as f64
}

/// A part-tile key's level ([`GridTileKey::PART_LEVEL`], C7) is not a
/// pyramid level: no zoom, cell size or window derives from it. Handing it
/// to a grid helper is a caller bug, caught here in debug builds.
fn assert_pyramid_level(level: u8) {
    debug_assert!(level != GridTileKey::PART_LEVEL, "a part-tile key's level is not a pyramid level (C7)");
}

/// Data-tile zoom a level bakes into: coarser levels (bigger cells) live at
/// shallower zooms.
pub fn data_zoom(d: &GridDescriptor, level: u8) -> u8 {
    assert_pyramid_level(level);
    d.max_data_zoom - level
}

/// Lon/lat of a cell's centre. Row 0's centre sits half a cell south of
/// `origin_lat`; col 0's, half a cell east of `origin_lon`.
pub fn cell_centre(d: &GridDescriptor, level: u8, row: u32, col: u32) -> (f64, f64) {
    let arc_deg = cell_arcsec(d, level) / 3600.0;
    let lat = d.origin_lat - (row as f64 + 0.5) * arc_deg;
    let lon = d.origin_lon + (col as f64 + 0.5) * arc_deg;
    (lat, lon)
}

/// The data tile whose window contains this cell's centre, or `None` when
/// the centre lies beyond ±[`MAX_MERCATOR_LAT`] (this renderer's Mercator
/// square has no tile there -- GHSL's polar rows are dropped, not drawn).
pub fn tile_of_cell(d: &GridDescriptor, level: u8, row: u32, col: u32) -> Option<TileId> {
    let (lat, lon) = cell_centre(d, level, row, col);
    if lat.abs() > MAX_MERCATOR_LAT {
        return None;
    }
    let z = data_zoom(d, level);
    let n = 1i64 << z;
    let (wx, wy) = lon_lat_to_world(lon, lat);
    let x = (wx / WORLD_SIZE as f64 * n as f64).floor() as i64;
    let y = (wy / WORLD_SIZE as f64 * n as f64).floor() as i64;
    // Longitude is a cylinder (wraps); latitude is clamped -- same
    // reasoning as `crate::geo::tile_id_at`.
    let x = x.rem_euclid(n) as u32;
    let y = y.clamp(0, n - 1) as u32;
    Some(TileId { z, x, y })
}

/// A tile's window into the grid: rows `[row0, row0+rows)`, cols
/// `[col0, col0+cols)` (inclusive min, exclusive max). See [`cell_window`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellWindow {
    pub row0: u32,
    pub col0: u32,
    pub rows: u32,
    pub cols: u32,
}

/// The row/col window whose centres fall in `tile` (inclusive min,
/// exclusive max). Inverts the tile's lon/lat bounds back to row/col via
/// the same `(index + 0.5) * arc_deg` relationship `cell_centre` uses, so a
/// centre exactly on a tile edge lands in the same tile `tile_of_cell`'s
/// `floor` puts it in (the property `every_cell_centre_lands_in_exactly_one_tile`
/// checks).
pub fn cell_window(d: &GridDescriptor, level: u8, tile: TileId) -> CellWindow {
    let arc = cell_arcsec(d, level);
    let arc_deg = arc / 3600.0;
    let bounds = tile_bounds_lonlat(tile);

    let rows_total = (180.0 * 3600.0 / arc) as u32;
    let cols_total = (360.0 * 3600.0 / arc) as u32;

    let clamp_row = |v: f64| (v.ceil().max(0.0) as u32).min(rows_total);
    let clamp_col = |v: f64| (v.ceil().max(0.0) as u32).min(cols_total);

    let row0 = clamp_row((d.origin_lat - bounds.max_lat) / arc_deg - 0.5);
    let row_end = clamp_row((d.origin_lat - bounds.min_lat) / arc_deg - 0.5);
    let col0 = clamp_col((bounds.min_lon - d.origin_lon) / arc_deg - 0.5);
    let col_end = clamp_col((bounds.max_lon - d.origin_lon) / arc_deg - 0.5);

    CellWindow { row0, col0, rows: row_end.saturating_sub(row0), cols: col_end.saturating_sub(col0) }
}

/// A cell's position inside its tile's window, row-major:
/// `(row - row0) * cols + (col - col0)`.
pub fn local_index(w: &CellWindow, row: u32, col: u32) -> u32 {
    (row - w.row0) * w.cols + (col - w.col0)
}

/// Inverse of [`local_index`].
pub fn row_col(w: &CellWindow, idx: u32) -> (u32, u32) {
    (w.row0 + idx / w.cols, w.col0 + idx % w.cols)
}

/// The cell's south-west corner (west edge, south edge), in Mercator world
/// units [0, 1) -- what [`crate::overlay::geom::lon_lat_to_world`] returns,
/// divided by [`WORLD_SIZE`]. `cell_centre` gives the middle; this gives
/// the corner `render::dots::build_blocks` (Task 3) places a block's origin
/// from.
pub fn cell_sw_mercator(d: &GridDescriptor, level: u8, row: u32, col: u32) -> (f64, f64) {
    let arc_deg = cell_arcsec(d, level) / 3600.0;
    let lat_south = d.origin_lat - (row as f64 + 1.0) * arc_deg;
    let lon_west = d.origin_lon + col as f64 * arc_deg;
    let (wx, wy) = lon_lat_to_world(lon_west, lat_south);
    (wx / WORLD_SIZE as f64, wy / WORLD_SIZE as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(arcsec: f64) -> GridDescriptor {
        GridDescriptor { base_arcsec: arcsec, origin_lat: 90.0, origin_lon: -180.0, levels: if arcsec == 3.0 { 10 } else { 7 },
            max_data_zoom: base_data_zoom(arcsec), epochs: vec![2025], attribution: String::new() }
    }

    #[test]
    fn base_data_zoom_is_nine_for_3_and_six_for_30() {
        assert_eq!(base_data_zoom(3.0), 9);
        assert_eq!(base_data_zoom(30.0), 6);
        assert!((crossover_zoom(3.0) - 9.72).abs() < 0.01);
        assert!((crossover_zoom(30.0) - 6.40).abs() < 0.01);
    }

    #[test]
    fn every_cell_centre_lands_in_exactly_one_tile() {
        // Sample 20k cells across the globe at every level; each must appear in its own tile's
        // window and in no neighbour's window.
        for g in [d(3.0), d(30.0)] {
            for level in 0..g.levels {
                let arc = cell_arcsec(&g, level);
                let rows = (180.0 * 3600.0 / arc) as u32;
                let cols = (360.0 * 3600.0 / arc) as u32;
                let mut s = 0x9e37_79b9u32;
                for _ in 0..2_000 {
                    s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let row = s % rows;
                    s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let col = s % cols;
                    let Some(t) = tile_of_cell(&g, level, row, col) else { continue };
                    let w = cell_window(&g, level, t);
                    assert!(row >= w.row0 && row < w.row0 + w.rows && col >= w.col0 && col < w.col0 + w.cols,
                        "level {level} cell ({row},{col}) not in its tile {t:?} window {:?}", (w.row0, w.col0, w.rows, w.cols));
                    for (dx, dy) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                        let n = 1i64 << t.z;
                        let (nx, ny) = (((t.x as i64 + dx) % n + n) % n, t.y as i64 + dy);
                        if ny < 0 || ny >= n { continue; }
                        let nt = TileId { z: t.z, x: nx as u32, y: ny as u32 };
                        // At z0 (one global tile) every "neighbour" -- wrapped
                        // in x, clamped away in y -- is the tile itself; that
                        // is not a collision to check for.
                        if nt == t { continue; }
                        let nw = cell_window(&g, level, nt);
                        let inside = row >= nw.row0 && row < nw.row0 + nw.rows && col >= nw.col0 && col < nw.col0 + nw.cols;
                        assert!(!inside, "cell ({row},{col}) also in neighbour {nt:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn cells_beyond_the_mercator_limit_are_counted_as_dropped() {
        let g = d(30.0);
        assert_eq!(tile_of_cell(&g, 0, 0, 100), None);          // 89.99°N
        let last_row = (180.0 * 3600.0 / 30.0) as u32 - 1;
        assert_eq!(tile_of_cell(&g, 0, last_row, 100), None);   // 89.99°S
        assert!(tile_of_cell(&g, 0, 21600 / 2, 100).is_some()); // equator
    }

    #[test]
    fn antimeridian_columns_map_to_the_edge_tiles_not_across_the_world() {
        let g = d(30.0);
        let row = 21600 / 2;
        let west = tile_of_cell(&g, 0, row, 0).unwrap();
        let east = tile_of_cell(&g, 0, row, 43199).unwrap();
        assert_eq!(west.x, 0);
        assert_eq!(east.x, (1 << west.z) - 1);
    }

    #[test]
    fn local_index_round_trips() {
        let w = CellWindow { row0: 100, col0: 200, rows: 845, cols: 846 };
        for (r, c) in [(100, 200), (944, 1045), (500, 700)] {
            assert_eq!(row_col(&w, local_index(&w, r, c)), (r, c));
        }
    }

    /// Pins the contract `render::dots::build_blocks` (Task 3) relies on:
    /// `cell_sw_mercator` returns the SW corner in UNIT-world coordinates
    /// (`lon_lat_to_world` divided by `WORLD_SIZE`, i.e. in `[0, 1)`), not
    /// raw world-pixel coordinates -- left untested by Task 1. Round-tripped
    /// through `geo::tile_xy_to_lon_lat` (the same unit-fraction convention:
    /// dividing `lon_lat_to_world`'s output by `WORLD_SIZE` is exactly
    /// `lon_lat_to_tile_xy(.., zoom: 0.0)`) and checked against
    /// `cell_centre`/`cell_arcsec`: the SW corner sits exactly half a cell
    /// south-west of the centre.
    #[test]
    fn cell_sw_mercator_returns_unit_world_not_world_pixels() {
        let g = d(30.0);
        let level = 0;
        let (row, col) = (5_000u32, 10_000u32);
        let (wx, wy) = cell_sw_mercator(&g, level, row, col);
        // Raw world-pixel coordinates would be WORLD_SIZE (512) times
        // bigger than this and fail the range check immediately.
        assert!((0.0..1.0).contains(&wx), "wx {wx} out of unit range -- cell_sw_mercator must divide by WORLD_SIZE");
        assert!((0.0..1.0).contains(&wy), "wy {wy} out of unit range -- cell_sw_mercator must divide by WORLD_SIZE");

        let (lon, lat) = crate::geo::tile_xy_to_lon_lat(wx, wy, 0.0);
        let (centre_lat, centre_lon) = cell_centre(&g, level, row, col);
        let arc_deg = cell_arcsec(&g, level) / 3600.0;
        assert!((lon - (centre_lon - arc_deg / 2.0)).abs() < 1e-6, "lon {lon} vs expected {}", centre_lon - arc_deg / 2.0);
        assert!((lat - (centre_lat - arc_deg / 2.0)).abs() < 1e-6, "lat {lat} vs expected {}", centre_lat - arc_deg / 2.0);
    }

    /// MG-1. Prints, and asserts the ≤ 1,048,576 criterion. Record the printed table.
    #[test]
    fn mg1_max_cells_per_data_tile() {
        for g in [d(3.0), d(30.0)] {
            for level in 0..g.levels {
                let z = data_zoom(&g, level);
                // Equatorial tile (widest in cells vertically is the equator: Mercator stretch is 1 there).
                let t = TileId { z, x: 0, y: (1u32 << z) / 2 };
                let w = cell_window(&g, level, t);
                let cells = w.rows as u64 * w.cols as u64;
                println!("MG-1 base {}″ level {level} data z{z}: {} x {} = {cells}", g.base_arcsec, w.rows, w.cols);
                assert!(cells <= 1 << 20, "MG-1 FAIL: {cells} cells in one tile");
            }
        }
    }
}
