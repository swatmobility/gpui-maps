//! A procedural, seeded stand-in for the GHSL 30″ world: no data file, no
//! download, the same answer on every machine. `world_dots --synthetic`
//! draws it, and `--measure` (the perf gate's `dots.*` section) runs on it.
//!
//! Land is [`LAND`] -- 14 rough continental lon/lat boxes -- and a land
//! cell is populated with probability [`OCCUPANCY`] (1 near a city), which
//! puts about 20 M populated cells at level 0 (the real 30″ world has
//! 42.6 M, MG-3). A populated cell holds a seeded lognormal count
//! (`μ` 1.0, `σ` 1.6, capped at [`CELL_CAP`]), times `1 + 400·exp(-d²/2σ²)` for the nearest of
//! [`CITIES`] (`σ` 30 km). Every value is a function of `(row, col)` alone
//! through `dotgrid::hash::pcg`, so any tile generates on its own, in any
//! order, and a coarser level's cell is the sum of its level-0 children
//! computed on demand.

use gpui_maps::overlay::dotgrid::{grid, hash::pcg, GridDescriptor, GridTile, GridTileKey, MemoryGridSource};

/// Level-0 cells per degree at 30″.
const PER_DEG: f64 = 120.0;

/// `(lon_min, lat_min, lon_max, lat_max)`: rough continents, disjoint.
pub const LAND: [(f64, f64, f64, f64); 14] = [
    (-125.0, 25.0, -66.0, 49.0), // North America
    (-117.0, 8.0, -83.0, 25.0),  // Mexico and Central America
    (-80.0, -20.0, -35.0, 10.0), // South America, north
    (-73.0, -55.0, -53.0, -20.0), // South America, south
    (-10.0, 36.0, 40.0, 60.0),   // Europe
    (-10.0, 28.0, 33.0, 36.0),   // North Africa's coast
    (-17.0, -35.0, 45.0, 14.0),  // Africa south of the Sahel's edge
    (35.0, 14.0, 60.0, 36.0),    // Middle East
    (68.0, 8.0, 97.0, 32.0),     // India and Bangladesh
    (100.0, 20.0, 123.0, 41.0),  // China
    (97.0, -8.0, 125.0, 20.0),   // South-east Asia
    (126.0, 31.0, 142.0, 42.0),  // Korea and Japan
    (40.0, 46.0, 90.0, 58.0),    // Russia and Central Asia
    (113.0, -38.0, 153.0, -12.0), // Australia
];

/// `(name, lon, lat)`: the hotspots' centres.
pub const CITIES: [(&str, f64, f64); 12] = [
    ("Dhaka", 90.4125, 23.8103),
    ("Delhi", 77.2090, 28.6139),
    ("Mumbai", 72.8777, 19.0760),
    ("Shanghai", 121.4737, 31.2304),
    ("Tokyo", 139.6917, 35.6895),
    ("Lagos", 3.3792, 6.5244),
    ("Cairo", 31.2357, 30.0444),
    ("São Paulo", -46.6333, -23.5505),
    ("Mexico City", -99.1332, 19.4326),
    ("New York", -74.0060, 40.7128),
    ("London", -0.1276, 51.5072),
    ("Jakarta", 106.8456, -6.2088),
];

/// Share of land cells populated away from a city. 0.105 of the ≈ 186.5 M
/// level-0 cells [`LAND`] holds is ≈ 19.6 M; `--measure` counts 19,660,771
/// populated cells with the cities.
pub const OCCUPANCY: f64 = 0.105;
const MU: f64 = 1.0;
const SIGMA: f64 = 1.6;
const CITY_SIGMA_KM: f64 = 30.0;
const CITY_GAIN: f64 = 400.0;
/// No cell holds more: the lognormal's far tail times a city's 401x would
/// otherwise put ~10 M people in one 30″ cell (real maxima are ~10^5).
const CELL_CAP: f64 = 50_000.0;
const SEED: u32 = 0x5eed_d075;

/// The 30″ descriptor the real archive has (MG-2's lattice), 7 levels.
pub fn descriptor() -> GridDescriptor {
    GridDescriptor {
        base_arcsec: 30.0,
        origin_lat: 90.0,
        origin_lon: -180.0,
        levels: 7,
        max_data_zoom: grid::base_data_zoom(30.0),
        epochs: vec![2025],
        attribution: "Synthetic population (procedural, seeded)".to_string(),
    }
}

pub fn source() -> MemoryGridSource {
    let d = descriptor();
    let dd = d.clone();
    MemoryGridSource::new(d, move |key| tile(&dd, key))
}

fn unit(x: u32) -> f64 {
    f64::from(x >> 8) / 16_777_216.0
}

/// Level-0 `[row0, row1] x [col0, col1]` (inclusive) whose centres fall in
/// a box.
fn box_cells(b: (f64, f64, f64, f64)) -> (u32, u32, u32, u32) {
    let (lon0, lat0, lon1, lat1) = b;
    let r0 = ((90.0 - lat1) * PER_DEG - 0.5).ceil() as u32;
    let r1 = ((90.0 - lat0) * PER_DEG - 0.5).floor() as u32;
    let c0 = ((lon0 + 180.0) * PER_DEG - 0.5).ceil() as u32;
    let c1 = ((lon1 + 180.0) * PER_DEG - 0.5).floor() as u32;
    (r0, r1, c0, c1)
}

fn land_cells() -> [(u32, u32, u32, u32); 14] {
    LAND.map(box_cells)
}

/// The strongest city pull at a point: `exp(-d²/2σ²)` for the nearest.
fn city_pull(lon: f64, lat: f64) -> f64 {
    let mut g: f64 = 0.0;
    for &(_, clon, clat) in &CITIES {
        let dlat = lat - clat;
        if dlat.abs() > 2.5 {
            continue;
        }
        let kx = 111.32 * clat.to_radians().cos();
        let dx = (lon - clon) * kx;
        let dy = dlat * 110.57;
        let d2 = dx * dx + dy * dy;
        if d2 > 300.0 * 300.0 {
            continue;
        }
        g = g.max((-d2 / (2.0 * CITY_SIGMA_KM * CITY_SIGMA_KM)).exp());
    }
    g
}

/// [`cell_pop`] with the land boxes built for the one call.
#[cfg_attr(not(test), allow(dead_code))]
pub fn cell_pop_at(row: u32, col: u32) -> f32 {
    cell_pop(&land_cells(), row, col)
}

/// Level-0 cell `(row, col)`'s population: 0 off land, and otherwise a
/// seeded draw. A function of `(row, col)` alone.
pub fn cell_pop(land: &[(u32, u32, u32, u32); 14], row: u32, col: u32) -> f32 {
    if !land.iter().any(|&(r0, r1, c0, c1)| row >= r0 && row <= r1 && col >= c0 && col <= c1) {
        return 0.0;
    }
    let lat = 90.0 - (f64::from(row) + 0.5) / PER_DEG;
    let lon = -180.0 + (f64::from(col) + 0.5) / PER_DEG;
    let h = pcg(col ^ pcg(row ^ SEED));
    let g = city_pull(lon, lat);
    if unit(h) >= OCCUPANCY + (1.0 - OCCUPANCY) * g {
        return 0.0;
    }
    let a = pcg(h);
    let u1 = unit(a).max(1.0 / 33_554_432.0);
    let u2 = unit(pcg(a));
    let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
    ((MU + SIGMA * z).exp() * (1.0 + CITY_GAIN * g)).min(CELL_CAP) as f32
}

/// One count tile at any level: every cell of `key`'s window that some
/// land box touches, each the sum of its `2^level x 2^level` level-0
/// children (in f64, stored as f32). Rows are split across threads.
pub fn tile(d: &GridDescriptor, key: &GridTileKey) -> GridTile {
    let w = grid::cell_window(d, key.level, key.tile);
    if w.rows == 0 || w.cols == 0 {
        return GridTile::default();
    }
    let land = land_cells();
    let s = key.level;
    let span = 1u32 << s;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min((w.rows as usize / 16).max(1));
    let chunk = (w.rows as usize).div_ceil(threads).max(1) as u32;
    let rows: Vec<Vec<(u32, f32, f32)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads as u32)
            .map(|t| {
                let land = &land;
                scope.spawn(move || {
                    let mut out = Vec::new();
                    let mut ranges: Vec<(u32, u32)> = Vec::new();
                    let lo = w.row0 + t * chunk;
                    let hi = (lo + chunk).min(w.row0 + w.rows);
                    for r in lo..hi {
                        ranges.clear();
                        for &(r0, r1, c0, c1) in land.iter() {
                            if r < r0 >> s || r > r1 >> s {
                                continue;
                            }
                            let a = (c0 >> s).max(w.col0);
                            let b = (c1 >> s).min(w.col0 + w.cols - 1);
                            if a <= b && w.cols > 0 {
                                ranges.push((a, b));
                            }
                        }
                        ranges.sort_unstable();
                        let mut next = 0u32;
                        for &(a, b) in &ranges {
                            for c in a.max(next)..=b {
                                let mut sum = 0f64;
                                for rr in r * span..(r + 1) * span {
                                    for cc in c * span..(c + 1) * span {
                                        sum += f64::from(cell_pop(land, rr, cc));
                                    }
                                }
                                if sum > 0.0 {
                                    out.push((grid::local_index(&w, r, c), sum as f32, sum as f32));
                                }
                            }
                            next = next.max(b + 1);
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("a synthetic row worker")).collect()
    });
    let cells: Vec<(u32, f32, f32)> = rows.into_iter().flatten().collect();
    let total = cells.iter().map(|c| f64::from(c.1)).sum();
    GridTile { cells, total, ..Default::default() }
}
