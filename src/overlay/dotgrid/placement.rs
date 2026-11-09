//! The CPU mirror of the relocating vertex shader, for the headless gates
//! (Task 8 of `docs/superpowers/plans/2026-09-27-dasymetric-dot-placement.md`):
//! every live dot of a tile's [`TileBlocks`], placed exactly as `vs_dot`'s
//! `dot_slot` places it against a mask -- candidate `j`'s position through
//! the shader's `dot_at`, its texel through [`MaskTileLookup::texel`], the
//! pick through [`hash::choose`] -- and counted by the rank it lands on.
//!
//! Pure: no GPU, no IO. The texels are a read-back (`DotMaskGpu::read_back`,
//! test and probe builds) or a [`SyntheticMask`]; nothing here is ever
//! written anywhere (DP-8).

use std::collections::HashMap;

use super::blocks::TileBlocks;
use super::hash;
use super::mask_class::MaskRank;
use crate::render::dot_mask::{MaskGrid, MaskTileLookup, MASK_CLEAR_RANK};
use crate::render::dots::{DOT_BUCKETS, MASK_K_MAX};

/// What the live dots of one or more tiles landed on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlacementReport {
    pub live: u64,
    pub in_building: u64,
    pub on_water: u64,
    pub on_green: u64,
    /// sum over cells of live_c * (1 - (1 - f_c)^K), f_c = building share of the cell's texels.
    pub analytic_in_building: f64,
    /// Dots on water with a candidate that has data and is not water, and
    /// none without data (must be 0).
    pub avoidable_on_water: u64,
}

impl PlacementReport {
    /// Adds `other`'s counts to these (several tiles into one view).
    pub fn add(&mut self, other: &PlacementReport) {
        self.live += other.live;
        self.in_building += other.in_building;
        self.on_water += other.on_water;
        self.on_green += other.on_green;
        self.analytic_in_building += other.analytic_in_building;
        self.avoidable_on_water += other.avoidable_on_water;
    }
}

/// One tile's placement inputs: the tile's lookup into the mask, the
/// shader's f32 constants, and the texels.
struct Placer<'a> {
    lookup: MaskTileLookup,
    grid: &'a MaskGrid,
    texels: &'a [u8],
    tile_origin_y: f32,
    unit_world_per_lyon: f32,
    cell_width: f32,
    k: u32,
}

/// One placed dot: its cell key and index, the candidate chosen, the
/// rank it landed on, and whether any of its candidates was not water.
struct Placed {
    key: u32,
    dot: u32,
    chosen: u8,
    rank: u8,
    any_dry: bool,
}

impl Placer<'_> {
    /// `render::dot_mask::MaskTileLookup::texel`'s texel of lyon point `p`,
    /// or [`MASK_CLEAR_RANK`] (no data) outside the mask -- the shader's
    /// `mask_rank`.
    fn rank(&self, p: [f32; 2]) -> u8 {
        let [x, y] = self.lookup.texel(p);
        let [w, h] = self.grid.size;
        if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
            return MASK_CLEAR_RANK as u8;
        }
        self.texels[y as usize * w as usize + x as usize]
    }

    /// The cell's Mercator stretch at its SW corner, in f32 as `dot_slot`
    /// computes it.
    fn stretch(&self, origin: [f32; 2]) -> f32 {
        let y_world = self.tile_origin_y + origin[1] * self.unit_world_per_lyon;
        (std::f32::consts::PI * (1.0 - 2.0 * y_world)).cosh()
    }

    /// `shader.wgsl`'s `dot_at`, operation for operation.
    fn dot_at(&self, origin: [f32; 2], u: f32, v: f32, stretch: f32) -> [f32; 2] {
        [origin[0] + u * self.cell_width, origin[1] + -v * self.cell_width * stretch]
    }

    /// Every live dot of `blocks` at `ppd`, placed.
    fn each(&self, blocks: &TileBlocks, ppd: f32, mut visit: impl FnMut(&Placed, [f32; 2], f32)) {
        let k = self.k.clamp(1, MASK_K_MAX) as usize;
        let mut ranks = [0u8; MASK_K_MAX as usize];
        for (b, bucket) in blocks.buckets.iter().enumerate() {
            for block in bucket {
                let key = hash::cell_key(block.origin);
                let stretch = self.stretch(block.origin);
                for dot in block.first_dot..block.first_dot + DOT_BUCKETS[b] {
                    if !hash::dot_alive(block.pop, ppd, dot, key) {
                        continue;
                    }
                    for (r, (u, v)) in ranks.iter_mut().zip(hash::dot_candidates(key, dot, self.k)) {
                        *r = self.rank(self.dot_at(block.origin, u, v, stretch));
                    }
                    let chosen = hash::choose(ranks[..k].iter().copied());
                    // Dry: a candidate with data that is not water. A dot that
                    // read no data kept candidate 0 by rule (DP-5 ruling): its
                    // water is not avoidable.
                    let any_dry = ranks[..k].iter().any(|&r| r > MaskRank::Water as u8)
                        && !ranks[..k].iter().any(|&r| u32::from(r) == crate::render::dot_mask::MASK_NO_DATA_RANK);
                    visit(&Placed { key, dot, chosen: chosen as u8, rank: ranks[chosen], any_dry }, block.origin, stretch);
                }
            }
        }
    }

    /// The building share of the cell at `origin`: its texels weighted by
    /// the area of each the cell covers (texels outside the mask are land).
    fn building_share(&self, origin: [f32; 2], stretch: f32) -> f64 {
        let cw = f64::from(self.cell_width);
        let tpl = f64::from(self.lookup.texels_per_lyon);
        let axis = |p: f64, k: usize| p * tpl + f64::from(self.lookup.frac[k]) + f64::from(self.lookup.int_offset[k]);
        let (x0, x1) = (axis(f64::from(origin[0]), 0), axis(f64::from(origin[0]) + cw, 0));
        let (y0, y1) = (axis(f64::from(origin[1]) - cw * f64::from(stretch), 1), axis(f64::from(origin[1]), 1));
        let [w, h] = self.grid.size;
        let (mut building, mut area) = (0.0, 0.0);
        for ty in y0.floor() as i64..y1.ceil() as i64 {
            let dy = (y1.min(ty as f64 + 1.0) - y0.max(ty as f64)).max(0.0);
            for tx in x0.floor() as i64..x1.ceil() as i64 {
                let a = dy * (x1.min(tx as f64 + 1.0) - x0.max(tx as f64)).max(0.0);
                area += a;
                let inside = (0..i64::from(w)).contains(&tx) && (0..i64::from(h)).contains(&ty);
                if inside && self.texels[ty as usize * w as usize + tx as usize] == hash::RANK_BUILDING {
                    building += a;
                }
            }
        }
        if area > 0.0 { building / area } else { 0.0 }
    }
}

fn placer<'a>(
    blocks: &TileBlocks,
    tile_origin_unit_world: (f64, f64),
    unit_world_per_lyon: f64,
    k: u32,
    grid: &'a MaskGrid,
    texels: &'a [u8],
) -> Placer<'a> {
    assert_eq!(texels.len(), grid.size[0] as usize * grid.size[1] as usize, "one byte per texel");
    Placer {
        lookup: grid.for_tile(tile_origin_unit_world, unit_world_per_lyon),
        grid,
        texels,
        tile_origin_y: tile_origin_unit_world.1 as f32,
        unit_world_per_lyon: unit_world_per_lyon as f32,
        cell_width: blocks.cell_width,
        k,
    }
}

/// Every live dot of `blocks` at `ppd`, placed by the CPU mirror against `texels`
/// (the read-back or synthetic mask, `grid.size` row-major).
#[must_use]
pub fn placement_report(
    blocks: &TileBlocks,
    tile_origin_unit_world: (f64, f64),
    unit_world_per_lyon: f64,
    ppd: f32,
    k: u32,
    grid: &MaskGrid,
    texels: &[u8],
) -> PlacementReport {
    let placer = placer(blocks, tile_origin_unit_world, unit_world_per_lyon, k, grid, texels);
    let mut report = PlacementReport::default();
    // Per cell (keyed by its origin's bits: a cell's blocks share it):
    // live dots, and the SW corner + stretch its share is measured over.
    let mut cells: HashMap<(u32, u32), (u64, [f32; 2], f32)> = HashMap::new();
    placer.each(blocks, ppd, |p, origin, stretch| {
        report.live += 1;
        match p.rank {
            r if r == MaskRank::Building as u8 => report.in_building += 1,
            r if r == MaskRank::Water as u8 => {
                report.on_water += 1;
                report.avoidable_on_water += u64::from(p.any_dry);
            }
            r if r == MaskRank::Green as u8 => report.on_green += 1,
            _ => {}
        }
        cells.entry((origin[0].to_bits(), origin[1].to_bits())).or_insert((0, origin, stretch)).0 += 1;
    });
    let k = f64::from(k.clamp(1, MASK_K_MAX));
    report.analytic_in_building = cells
        .values()
        .map(|&(live, origin, stretch)| live as f64 * (1.0 - (1.0 - placer.building_share(origin, stretch)).powf(k)))
        .sum();
    report
}

/// (cell key, dot index) -> chosen j, for DP-5.
#[must_use]
pub fn chosen_map(
    blocks: &TileBlocks,
    tile_origin_unit_world: (f64, f64),
    unit_world_per_lyon: f64,
    ppd: f32,
    k: u32,
    grid: &MaskGrid,
    texels: &[u8],
) -> HashMap<(u32, u32), u8> {
    let placer = placer(blocks, tile_origin_unit_world, unit_world_per_lyon, k, grid, texels);
    let mut map = HashMap::new();
    placer.each(blocks, ppd, |p, _, _| {
        map.insert((p.key, p.dot), p.chosen);
    });
    map
}

/// Every live dot's (cell key, dot index), folded into one order-free
/// digest, and the count -- the per-cell alive sets DP-1 compares, with
/// no per-dot memory. `placed` walks the dots through the relocating
/// mirror (against `grid`/`texels` at `k`); otherwise through
/// [`hash::dot_alive`] alone, today's path.
#[must_use]
pub fn alive_digest(
    blocks: &TileBlocks,
    tile_origin_unit_world: (f64, f64),
    unit_world_per_lyon: f64,
    ppd: f32,
    placed: Option<(u32, &MaskGrid, &[u8])>,
) -> (u64, u64) {
    let term = |key: u32, dot: u32| u64::from(hash::pcg(key ^ hash::pcg(dot))) << 32 | u64::from(hash::pcg(dot ^ hash::pcg(key)));
    let (mut count, mut digest) = (0u64, 0u64);
    match placed {
        Some((k, grid, texels)) => placer(blocks, tile_origin_unit_world, unit_world_per_lyon, k, grid, texels).each(blocks, ppd, |p, _, _| {
            count += 1;
            digest = digest.wrapping_add(term(p.key, p.dot));
        }),
        None => {
            for (b, bucket) in blocks.buckets.iter().enumerate() {
                for block in bucket {
                    let key = hash::cell_key(block.origin);
                    for dot in block.first_dot..block.first_dot + DOT_BUCKETS[b] {
                        if hash::dot_alive(block.pop, ppd, dot, key) {
                            count += 1;
                            digest = digest.wrapping_add(term(key, dot));
                        }
                    }
                }
            }
        }
    }
    (count, digest)
}

/// Dots present in both maps whose chosen candidate differs.
#[must_use]
pub fn chosen_changes(a: &HashMap<(u32, u32), u8>, b: &HashMap<(u32, u32), u8>) -> u64 {
    a.iter().filter(|(key, j)| b.get(key).is_some_and(|other| other != *j)).count() as u64
}

/// The synthetic masks DP-1 and DP-2 (`capture::tests::mg8_vertex_cost`'s
/// `MG8_MASK`) place against: no basemap, a fixed pattern of ranks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticMask {
    /// `tenths` / 10 of the texels building, the rest land, spread as a
    /// diagonal lattice (`(x + 7 y) mod 10 < tenths`): at any cell size a
    /// candidate hits a building with probability about `tenths / 10`.
    Checker { tenths: u32 },
    /// The west half of the grid water, the east half land: no candidate
    /// finds a building, so every dot runs its whole candidate loop.
    WaterHalf,
}

impl SyntheticMask {
    /// `MG8_MASK`'s names: `checker10`, `checker30`, `water_half`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "checker10" => Some(Self::Checker { tenths: 1 }),
            "checker30" => Some(Self::Checker { tenths: 3 }),
            "water_half" => Some(Self::WaterHalf),
            _ => None,
        }
    }

    /// The texels of `grid`, row-major.
    #[must_use]
    pub fn texels(self, grid: &MaskGrid) -> Vec<u8> {
        let [w, h] = grid.size;
        let mut out = Vec::with_capacity(w as usize * h as usize);
        for y in 0..h {
            for x in 0..w {
                out.push(match self {
                    Self::Checker { tenths } if (x + 7 * y) % 10 < tenths => MaskRank::Building,
                    Self::Checker { .. } => MaskRank::Land,
                    Self::WaterHalf if x < w / 2 => MaskRank::Water,
                    Self::WaterHalf => MaskRank::Land,
                } as u8);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::TileId;
    use crate::overlay::dotgrid::blocks::{build_blocks, OVERLAY_LYON_PER_UNIT_WORLD};
    use crate::overlay::dotgrid::{grid, GridDescriptor, GridTile, GridTileKey};

    const TILE: TileId = TileId { z: 6, x: 47, y: 27 };

    /// A 30x30-cell patch of a 30″ z6 tile, 40 people per cell, its unit-
    /// world origin, and a zoom-12 mask grid over the patch.
    fn patch() -> (TileBlocks, (f64, f64), MaskGrid) {
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: String::new(),
        };
        let key = GridTileKey { tile: TILE, level: 0, epoch: 2025, epoch_b: None };
        let w = grid::cell_window(&d, 0, TILE);
        let cells: Vec<(u32, f32, f32)> =
            (0..30).flat_map(|r| (0..30).map(move |c| (r * w.cols + c, 40.0, 40.0))).collect();
        let tile = GridTile { total: cells.len() as f64 * 40.0, cells, ..Default::default() };
        let blocks = build_blocks(&d, &key, &tile, 1.0, OVERLAY_LYON_PER_UNIT_WORLD);
        let n = f64::from(1u32 << TILE.z);
        let origin = (f64::from(TILE.x) / n, f64::from(TILE.y) / n);
        let upl = 1.0 / OVERLAY_LYON_PER_UNIT_WORLD;
        let mut region = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for b in blocks.buckets.iter().flatten() {
            let (x, y) = (origin.0 + f64::from(b.origin[0]) * upl, origin.1 + f64::from(b.origin[1]) * upl);
            region = [region[0].min(x), region[1].min(y), region[2].max(x), region[3].max(y)];
        }
        let cell = f64::from(blocks.cell_width) * upl;
        let grid = MaskGrid::plan(12.0, region, 2.0 * cell, 4096).expect("a grid");
        assert_eq!(grid.zm, 12, "the patch fits at zoom 12");
        (blocks, origin, grid)
    }

    #[test]
    fn an_all_land_mask_places_every_dot_at_candidate_zero() {
        let (blocks, origin, grid) = patch();
        let land = vec![MaskRank::Land as u8; grid.size[0] as usize * grid.size[1] as usize];
        let upl = 1.0 / OVERLAY_LYON_PER_UNIT_WORLD;
        let map = chosen_map(&blocks, origin, upl, 1.0, 8, &grid, &land);
        assert!(map.len() > 30_000, "{} dots", map.len());
        assert!(map.values().all(|&j| j == 0));
        let report = placement_report(&blocks, origin, upl, 1.0, 8, &grid, &land);
        assert_eq!(report.live, map.len() as u64);
        assert_eq!((report.in_building, report.on_water, report.on_green, report.avoidable_on_water), (0, 0, 0, 0));
        assert_eq!(report.analytic_in_building, 0.0);
    }

    #[test]
    fn the_report_matches_the_analytic_rate_on_a_checkerboard() {
        // f = 0.3 checkerboard, K = 8: in_building within 1 pp of analytic_in_building / live * live.
        let (blocks, origin, grid) = patch();
        let texels = SyntheticMask::Checker { tenths: 3 }.texels(&grid);
        let upl = 1.0 / OVERLAY_LYON_PER_UNIT_WORLD;
        let report = placement_report(&blocks, origin, upl, 1.0, 8, &grid, &texels);
        let (hit, analytic) = (report.in_building as f64 / report.live as f64, report.analytic_in_building / report.live as f64);
        assert!((analytic - (1.0 - 0.7f64.powi(8))).abs() < 0.01, "analytic {analytic}: every cell is ~30 % building");
        assert!((hit - analytic).abs() < 0.01, "hit {hit} vs analytic {analytic}");
        assert_eq!(report.on_water, 0);
        // K = 1 is today's placement: the hit rate is the building share.
        let k1 = placement_report(&blocks, origin, upl, 1.0, 1, &grid, &texels);
        assert!((k1.in_building as f64 / k1.live as f64 - 0.3).abs() < 0.01, "{k1:?}");
    }

    #[test]
    fn no_dot_stays_on_water_when_a_candidate_is_dry() {
        let (blocks, origin, grid) = patch();
        let texels = SyntheticMask::WaterHalf.texels(&grid);
        let upl = 1.0 / OVERLAY_LYON_PER_UNIT_WORLD;
        let off = placement_report(&blocks, origin, upl, 1.0, 1, &grid, &texels);
        let on = placement_report(&blocks, origin, upl, 1.0, 8, &grid, &texels);
        assert!(off.on_water > 1_000, "{off:?}");
        assert_eq!(on.avoidable_on_water, 0);
        assert_eq!(on.live, off.live);
        // Only cells cut by the shore can move a dot off the water.
        assert!(on.on_water < off.on_water, "{on:?} vs {off:?}");
    }

    #[test]
    fn chosen_changes_counts_only_dots_present_in_both() {
        let a: HashMap<(u32, u32), u8> = [((1, 0), 0), ((1, 1), 3), ((2, 0), 5)].into_iter().collect();
        let b: HashMap<(u32, u32), u8> = [((1, 0), 0), ((1, 1), 4), ((3, 0), 1)].into_iter().collect();
        assert_eq!(chosen_changes(&a, &b), 1);
        assert_eq!(chosen_changes(&b, &a), 1);
        assert_eq!(chosen_changes(&a, &HashMap::new()), 0);
    }

    /// DP-6 through the whole mirror: with the mask fixed, every dot alive
    /// at `ppd` is alive at `ppd / 2` and picks the same candidate.
    #[test]
    fn the_dots_alive_at_ppd_keep_their_place_at_half_the_ppd() {
        let (blocks, origin, grid) = patch();
        let texels = SyntheticMask::Checker { tenths: 1 }.texels(&grid);
        let upl = 1.0 / OVERLAY_LYON_PER_UNIT_WORLD;
        for ppd in [2.0f32, 5.0, 16.0] {
            let coarse = chosen_map(&blocks, origin, upl, ppd, 8, &grid, &texels);
            let fine = chosen_map(&blocks, origin, upl, ppd / 2.0, 8, &grid, &texels);
            assert!(coarse.keys().all(|k| fine.contains_key(k)), "nesting at ppd {ppd}");
            assert_eq!(chosen_changes(&coarse, &fine), 0, "ppd {ppd}");
            assert!(fine.len() > coarse.len());
        }
    }

    #[test]
    fn the_alive_digest_is_the_same_through_the_relocating_mirror() {
        let (blocks, origin, grid) = patch();
        let texels = SyntheticMask::Checker { tenths: 3 }.texels(&grid);
        let upl = 1.0 / OVERLAY_LYON_PER_UNIT_WORLD;
        let today = alive_digest(&blocks, origin, upl, 3.0, None);
        assert!(today.0 > 10_000);
        assert_eq!(alive_digest(&blocks, origin, upl, 3.0, Some((8, &grid, &texels))), today);
        assert_ne!(alive_digest(&blocks, origin, upl, 2.0, None), today, "the digest sees the alive set");
    }
}
