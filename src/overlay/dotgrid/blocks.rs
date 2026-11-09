//! The block builder: turns one decoded [`GridTile`] into the bucketed
//! [`DotBlock`] vectors `vs_dot` (`render::dots`) draws from.
//!
//! A cell's dot count (`ceil(pop / ppd_build)`) is greedily decomposed into
//! blocks of [`DOT_BUCKETS`] sizes, largest first, each block getting a
//! `first_dot` that continues the cell's own dot numbering. Because the
//! bucket sizes are powers of four, greedy decomposition is exact -- see
//! `DOT_BUCKETS`'s doc comment -- so [`TileBlocks::slots`] is always exactly
//! the sum of every cell's count, never more and never less.

use super::codec_cat::PartRecord;
use super::grid;
use super::{GridDescriptor, GridTile, GridTileKey};
use crate::geo::TileId;
use crate::render::dots::{DotBlock, DotParams, DOT_BUCKETS};

/// One data tile's dots, bucketed by block size so each bucket can be drawn
/// with a fixed dot count per block (`render::dots`' `vs_dot`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TileBlocks {
    /// One vector per entry of DOT_BUCKETS.
    pub buckets: [Vec<DotBlock>; 4],
    /// pop_b per block, parallel to `buckets`, empty unless the key has epoch_b.
    pub pop_b: [Vec<f32>; 4],
    pub ppd_build: f32,
    /// Cell width in lyon units of the tile (uniform for the tile).
    pub cell_width: f32,
    /// Slots allocated: sum over blocks of the bucket size.
    pub slots: u64,
    /// The composition texel (`composition::Composition::texel`) of each
    /// block's record, parallel to `buckets`; empty for a tile without
    /// group counts (a v1 `DGT1` cell tile).
    pub comps: [Vec<[u32; 4]>; 4],
    /// Each block's triangle edges `[e1.x, e1.y, e2.x, e2.y]` (`e1 = b -
    /// a`, `e2 = c - a`, tile lyon units, computed in f64), parallel to
    /// `buckets`; empty for a cell tile. A block's `origin` is its part's
    /// vertex `a`.
    pub shapes: [Vec<[f32; 4]>; 4],
}

impl TileBlocks {
    /// The first block of the first non-empty bucket, as `(bucket, index)`.
    ///
    /// # Panics
    /// When every bucket is empty.
    #[must_use]
    pub fn first_nonempty(&self) -> (usize, usize) {
        let bi = self.buckets.iter().position(|b| !b.is_empty()).expect("a tile with at least one block");
        (bi, 0)
    }

    /// The `DotParams::shape` these blocks are drawn with: triangles when
    /// they carry shape texels.
    #[must_use]
    pub fn shape(&self) -> u32 {
        if self.shapes.iter().any(|s| !s.is_empty()) {
            crate::render::dots::SHAPE_TRIANGLE
        } else {
            crate::render::dots::SHAPE_CELL
        }
    }
}

/// Decomposes one cell's dot `count` into blocks of [`DOT_BUCKETS`] sizes,
/// largest bucket first, calling `emit(bucket_index, first_dot)` once per
/// block. `first_dot` starts at 0 for each call and advances by the
/// emitted block's size, so it names the block's first dot in the cell's
/// own numbering (what `vs_dot` adds the pulled vertex's
/// within-block offset to).
fn decompose(count: u64, mut emit: impl FnMut(usize, u32)) {
    let mut first_dot: u32 = 0;
    let mut remaining = count;
    for bucket_idx in (0..DOT_BUCKETS.len()).rev() {
        let size = u64::from(DOT_BUCKETS[bucket_idx]);
        let n = remaining / size;
        for _ in 0..n {
            emit(bucket_idx, first_dot);
            first_dot += size as u32;
        }
        remaining -= n * size;
    }
    debug_assert_eq!(remaining, 0, "DOT_BUCKETS are powers of four; greedy decomposition is exact");
}

/// Lyon units per unit world under the overlay convention
/// (`combined_overlay_matrix`, `GlobeUpload::for_overlay`):
/// `OVERLAY_UNITS_PER_WORLD` per world unit times `WORLD_SIZE` world units.
/// The `lyon_per_world` a `vs_dot` draw's blocks are built with.
pub const OVERLAY_LYON_PER_UNIT_WORLD: f64 =
    crate::overlay::geom::OVERLAY_UNITS_PER_WORLD * crate::render::WORLD_SIZE as f64;

impl DotParams {
    /// A tile's `vs_dot` uniform for this frame, and the tile's NW corner in
    /// world units (what `render::combined_overlay_matrix` and
    /// `GlobeUpload::for_overlay` take for its model uniform) -- shared by
    /// `layer::DotLayersGpu::sync` and `capture`'s dot draws, so the two
    /// cannot place a tile differently. `blocks` were built with
    /// [`OVERLAY_LYON_PER_UNIT_WORLD`]; `ppd` is the VIEW's. No live mix
    /// (`t` 0), ramp row 0; the bucket runs are filled by
    /// `TilePipeline::create_dot_bind_group`.
    #[must_use]
    pub fn for_tile(tile: TileId, blocks: &TileBlocks, exclude: [[f32; 4]; 2], ppd: f32, radius_px: f32) -> (Self, (f64, f64)) {
        let n = f64::from(1u32 << tile.z);
        let (ox, oy) = (f64::from(tile.x) / n, f64::from(tile.y) / n);
        let world = f64::from(crate::render::WORLD_SIZE);
        let params = DotParams {
            exclude_a: exclude[0],
            exclude_b: exclude[1],
            ppd,
            cell_width: blocks.cell_width,
            tile_origin_y: oy as f32,
            radius_px,
            t: 0.0,
            ramp_row: 0,
            unit_world_per_lyon: (1.0 / OVERLAY_LYON_PER_UNIT_WORLD) as f32,
            shape: blocks.shape(),
            category_mask: crate::render::dots::CATEGORY_MASK_ALL,
            ..bytemuck::Zeroable::zeroed()
        };
        (params, (ox * world, oy * world))
    }
}

/// Slots for a cell of population `pop` (live mode: the larger epoch's)
/// built at `ppd_build`, whose hash key is `key`: the dots alive at
/// `ppd_build` -- every index below `floor(pop / ppd_build)`, and the one
/// at it when its `h_t` falls below the fraction (`hash::dot_alive`).
///
/// A tile is drawn at a view ppd in `[ppd_build, 2 ppd_build]`
/// (`level::needs_rebuild`), and a dot alive at a ppd is alive at every
/// smaller one (nesting), so a dot dead at `ppd_build` is dead for as
/// long as the blocks are drawn: it needs no slot. `ceil` gave it one,
/// and every populated cell at least one block, so sparse cells -- most of
/// them at a world or continent view -- each cost a texel and, with
/// `first_dot` 0 always in the live prefix, a slot in every draw (fix
/// round 1). The expected count is `pop / ppd_build` exactly.
pub fn slot_count(pop: f32, ppd_build: f32, key: u32) -> u64 {
    let q = pop / ppd_build;
    let whole = q.floor().max(0.0);
    let n = whole as u64;
    n + u64::from(super::hash::dot_alive(pop, ppd_build, n as u32, key))
}

/// Builds `tile`'s dot blocks at build budget `ppd_build` (population per
/// dot -- see `level::ppd_build_for`), placing each block's origin in the
/// tile's own lyon units.
///
/// `lyon_per_world` converts Mercator world units `[0, 1)` (what
/// `grid::cell_sw_mercator` returns) to the tile's lyon units, as
/// `CircleInstance` centres do.
///
/// **It is per WHOLE world, so it already carries the data tile's `2^z`**:
/// an origin is a unit-world delta inside the tile, in `[0, 1/2^z)`, so a
/// caller whose lyon units span ONE tile (extent `E`) must pass
/// `E * 2^z`, not `E`. The overlay convention `vs_dot` draws with
/// (`combined_overlay_matrix` off the tile's NW corner) is
/// [`OVERLAY_LYON_PER_UNIT_WORLD`], already whole-world; capture's
/// `a_dot_lands_inside_its_own_cell_in_a_z6_data_tile` fails if it is not.
///
/// A block's origin is its cell's SW corner minus the tile's own NW corner
/// (`key.tile.x / 2^z`, `key.tile.y / 2^z` -- the west and NORTH edges, since
/// Mercator y grows south; the same unit-world fraction `grid::tile_of_cell`
/// inverts back from), times `lyon_per_world`, cast to f32 -- so `origin`
/// is tile-local, in `[0, lyon_per_world / 2^z]` plus at most one cell for
/// a cell straddling the tile's edge.
///
/// Live mode (`key.epoch_b.is_some()`): a cell's count is decomposed from
/// `max(pop, pop_b)` -- the larger of the two epochs' populations, so every
/// dot either epoch might show has a slot -- and each block's `pop_b` is
/// pushed to the matching bucket of `TileBlocks::pop_b`, parallel to
/// `buckets`.
///
/// A categorical tile (`tile.cats` non-empty) also fills
/// [`TileBlocks::comps`]; a part tile (a [`GridTileKey::part`] key, or
/// `tile.parts` non-empty -- the `DGP1` magic's decode) is built by
/// [`build_part_blocks`], and its key's level is never read as a pyramid
/// level (C7): an empty part tile builds no blocks.
pub fn build_blocks(d: &GridDescriptor, key: &GridTileKey, tile: &GridTile, ppd_build: f32, lyon_per_world: f64) -> TileBlocks {
    if key.is_part() || !tile.parts.is_empty() {
        return build_part_blocks(key, &tile.parts, ppd_build, lyon_per_world);
    }
    let w = grid::cell_window(d, key.level, key.tile);
    let n = 1u64 << key.tile.z;
    let origin_x = f64::from(key.tile.x) / n as f64;
    let origin_y = f64::from(key.tile.y) / n as f64;

    // Longitude maps to world-x linearly (`geo::lon_lat_to_tile_xy`), so a
    // cell's width in unit-world is the same at every row/col of the tile --
    // unlike its height, which the Mercator latitude stretch varies by row.
    let arc_deg = grid::cell_arcsec(d, key.level) / 3600.0;
    let cell_width = (arc_deg / 360.0 * lyon_per_world) as f32;

    let has_epoch_b = key.epoch_b.is_some();
    let categorical = !tile.cats.is_empty();

    let mut buckets: [Vec<DotBlock>; 4] = Default::default();
    let mut pop_b: [Vec<f32>; 4] = Default::default();
    let mut comps: [Vec<[u32; 4]>; 4] = Default::default();
    let mut slots: u64 = 0;

    for (at, &(idx, pop, pop_b_val)) in tile.cells.iter().enumerate() {
        let (row, col) = grid::row_col(&w, idx);
        let (sw_x, sw_y) = grid::cell_sw_mercator(d, key.level, row, col);
        let origin = [((sw_x - origin_x) * lyon_per_world) as f32, ((sw_y - origin_y) * lyon_per_world) as f32];

        let count = slot_count(pop.max(pop_b_val), ppd_build, super::hash::cell_key(origin));
        if count == 0 {
            continue;
        }
        slots += count;
        let comp = categorical.then(|| super::composition::Composition::from_counts(&tile.cats[at]).texel());

        decompose(count, |bucket_idx, first_dot| {
            buckets[bucket_idx].push(DotBlock { origin, pop, first_dot });
            if has_epoch_b {
                pop_b[bucket_idx].push(pop_b_val);
            }
            if let Some(c) = comp {
                comps[bucket_idx].push(c);
            }
        });
    }

    let mut shapes: [Vec<[f32; 4]>; 4] = Default::default();
    sort_for_upload(&mut buckets, &mut pop_b, &mut comps, &mut shapes, has_epoch_b);
    TileBlocks { buckets, pop_b, ppd_build, cell_width, slots, comps, shapes }
}

/// Builds a part tile's dot blocks (spec §3.2): one record per
/// [`PartRecord`], its `origin` the triangle's vertex `a` and its shape
/// texel the edges `e1 = b - a`, `e2 = c - a`, all in the tile's lyon
/// units (`lyon_per_world` as [`build_blocks`] takes it, computed in f64
/// and cast once). The hash key is `parts::part_key` of those exact f32s
/// -- what `vs_dot` computes from the texels -- and the composition texel
/// is the part's counts'. `cell_width` is 0: a triangle tile's style draws
/// no density term (C12).
pub fn build_part_blocks(key: &GridTileKey, parts: &[PartRecord], ppd_build: f32, lyon_per_world: f64) -> TileBlocks {
    let n = f64::from(1u32 << key.tile.z);
    let (ox, oy) = (f64::from(key.tile.x) / n, f64::from(key.tile.y) / n);
    let mut buckets: [Vec<DotBlock>; 4] = Default::default();
    let mut comps: [Vec<[u32; 4]>; 4] = Default::default();
    let mut shapes: [Vec<[f32; 4]>; 4] = Default::default();
    let mut slots: u64 = 0;
    for p in parts {
        let origin = [((p.a[0] - ox) * lyon_per_world) as f32, ((p.a[1] - oy) * lyon_per_world) as f32];
        let e1 = [((p.b[0] - p.a[0]) * lyon_per_world) as f32, ((p.b[1] - p.a[1]) * lyon_per_world) as f32];
        let e2 = [((p.c[0] - p.a[0]) * lyon_per_world) as f32, ((p.c[1] - p.a[1]) * lyon_per_world) as f32];
        let people: u64 = p.counts.iter().map(|&c| u64::from(c)).sum();
        let pop = people as f32;
        let count = slot_count(pop, ppd_build, super::parts::part_key(origin, e1, e2));
        if count == 0 {
            continue;
        }
        slots += count;
        let comp = super::composition::Composition::from_counts(&p.counts).texel();
        let shape = [e1[0], e1[1], e2[0], e2[1]];
        decompose(count, |bucket_idx, first_dot| {
            buckets[bucket_idx].push(DotBlock { origin, pop, first_dot });
            comps[bucket_idx].push(comp);
            shapes[bucket_idx].push(shape);
        });
    }
    let mut pop_b: [Vec<f32>; 4] = Default::default();
    sort_for_upload(&mut buckets, &mut pop_b, &mut comps, &mut shapes, false);
    TileBlocks { buckets, pop_b, ppd_build, cell_width: 0.0, slots, comps, shapes }
}

/// Puts each bucket -- and its parallel `pop_b`, `comps` and `shapes`,
/// where non-empty -- in the order `TilePipeline::create_dot_tile` draws
/// it.
fn sort_for_upload(
    buckets: &mut [Vec<DotBlock>; 4],
    pop_b: &mut [Vec<f32>; 4],
    comps: &mut [Vec<[u32; 4]>; 4],
    shapes: &mut [Vec<[f32; 4]>; 4],
    has_epoch_b: bool,
) {
    // Each bucket in the order `TilePipeline::create_dot_tile` draws it:
    // descending `max(pop, pop_b) / first_dot` (`first_dot` 0 first),
    // stable. Sorted here, on the worker, the upload's own stable sort
    // meets a sorted run and costs O(n) on the paint thread (fix round 2:
    // the sort there took 6-15 ms per tile and made the sweep's frames
    // over 33 ms).
    for i in 0..buckets.len() {
        let key = |b: &DotBlock, pb: f32| {
            if b.first_dot == 0 {
                f32::INFINITY
            } else {
                (if has_epoch_b { b.pop.max(pb) } else { b.pop }) / b.first_dot as f32
            }
        };
        let pbs = &pop_b[i];
        let bs = &buckets[i];
        // Keys once, then an unstable sort on (key descending, index):
        // the same order a stable sort by key gives, without recomputing
        // a key per comparison.
        let mut order: Vec<(f32, u32)> =
            bs.iter().enumerate().map(|(j, b)| (key(b, pbs.get(j).copied().unwrap_or(0.0)), j as u32)).collect();
        order.sort_unstable_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
        let order: Vec<usize> = order.into_iter().map(|(_, j)| j as usize).collect();
        buckets[i] = order.iter().map(|&j| bs[j]).collect();
        if has_epoch_b {
            pop_b[i] = order.iter().map(|&j| pbs[j]).collect();
        }
        if !comps[i].is_empty() {
            comps[i] = order.iter().map(|&j| comps[i][j]).collect();
        }
        if !shapes[i].is_empty() {
            shapes[i] = order.iter().map(|&j| shapes[i][j]).collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 30″ `GridDescriptor` (same field values as `grid::tests::d(30.0)`
    /// and `level::tests::test_descriptor(30.0)`) and a key for the
    /// equatorial level-0 tile: `base_data_zoom(30.0)` is 6, and the
    /// equator sits at data tile `(32, 32)` of the 64x64 grid a z6 pyramid
    /// has.
    fn fixture_30s() -> (GridDescriptor, GridTileKey) {
        let d = GridDescriptor {
            base_arcsec: 30.0,
            origin_lat: 90.0,
            origin_lon: -180.0,
            levels: 7,
            max_data_zoom: grid::base_data_zoom(30.0),
            epochs: vec![2025],
            attribution: String::new(),
        };
        let key = GridTileKey { tile: TileId { z: 6, x: 32, y: 32 }, level: 0, epoch: 2025, epoch_b: None };
        (d, key)
    }

    #[test]
    fn part_blocks_carry_composition_and_edges() {
        use crate::overlay::dotgrid::composition::Composition;
        let key = GridTileKey { tile: TileId { z: 12, x: 0, y: 0 }, level: 0, epoch: 2020, epoch_b: None };
        let p = PartRecord { a: [0.0, 0.0], b: [1e-5, 0.0], c: [0.0, 1e-5], counts: [2, 1, 0, 0, 0, 0, 0, 0] };
        let tb = build_part_blocks(&key, &[p], 1.0, OVERLAY_LYON_PER_UNIT_WORLD);
        assert_eq!(tb.slots, 3);
        let (bi, i) = tb.first_nonempty();
        assert_eq!(tb.comps[bi][i], Composition::from_counts(&[2, 1, 0, 0, 0, 0, 0, 0]).texel());
        assert!(tb.shapes[bi][i][0] > 0.0 && tb.shapes[bi][i][3] > 0.0);
        assert_eq!(tb.shape(), crate::render::dots::SHAPE_TRIANGLE);
    }

    /// Every part's dots, at every `ppd` its rebuild window draws, have a
    /// slot, keyed by `part_key` of the texels the shader reads; the
    /// parallel vectors stay parallel through the upload sort; and a
    /// part's origin is its vertex `a`, tile-local.
    #[test]
    fn part_blocks_give_every_live_dot_a_slot_keyed_by_part_key() {
        use crate::overlay::dotgrid::{hash, parts};
        let key = GridTileKey::part(TileId { z: 12, x: 1_000, y: 1_500 }, 2020);
        let n = 4096.0f64;
        let (ox, oy) = (1_000.0 / n, 1_500.0 / n);
        let recs: Vec<PartRecord> = (0..300u32)
            .map(|k| {
                let (x, y) = (ox + f64::from(k % 20) * 1e-5, oy + f64::from(k / 20) * 1e-5);
                let c = |g: u32| (hash::pcg(k * 8 + g) % 9) * u32::from(g % 3 != 2);
                PartRecord { a: [x, y], b: [x + 9e-6, y], c: [x, y + 7e-6], counts: std::array::from_fn(|g| c(g as u32)) }
            })
            .filter(|p| p.counts.iter().any(|&c| c > 0))
            .collect();
        let ppd_build = 1.5;
        let tb = build_part_blocks(&key, &recs, ppd_build, OVERLAY_LYON_PER_UNIT_WORLD);
        for i in 0..4 {
            assert_eq!(tb.comps[i].len(), tb.buckets[i].len());
            assert_eq!(tb.shapes[i].len(), tb.buckets[i].len());
        }
        let mut slots_of: std::collections::HashMap<u32, (f32, u32, [u32; 4])> = Default::default();
        for (bi, bucket) in tb.buckets.iter().enumerate() {
            for (j, blk) in bucket.iter().enumerate() {
                let s = tb.shapes[bi][j];
                let k = parts::part_key(blk.origin, [s[0], s[1]], [s[2], s[3]]);
                let e = slots_of.entry(k).or_insert((blk.pop, 0, tb.comps[bi][j]));
                assert_eq!(e.2, tb.comps[bi][j], "one part, one composition");
                e.1 = e.1.max(blk.first_dot + DOT_BUCKETS[bi]);
                assert!(blk.origin[0] >= 0.0 && blk.origin[1] >= 0.0, "tile-local");
            }
        }
        for (k, (pop, n, _)) in &slots_of {
            for ppd in [ppd_build, ppd_build * 2.0] {
                for i in 0..(pop / ppd).ceil() as u32 + 2 {
                    if hash::dot_alive(*pop, ppd, i, *k) {
                        assert!(i < *n, "dot {i} of a part of {pop} alive at {ppd} has no slot");
                    }
                }
            }
        }
    }

    /// A categorical cell tile's blocks carry their cell's composition.
    #[test]
    fn categorical_cell_blocks_carry_their_composition() {
        use crate::overlay::dotgrid::composition::Composition;
        let (d, key) = fixture_30s();
        let cats = vec![[5, 0, 0, 0, 0, 0, 0, 0], [1, 2, 3, 4, 5, 6, 7, 8]];
        let tile = GridTile { cells: vec![(0, 5.0, 5.0), (3, 36.0, 36.0)], total: 41.0, cats: cats.clone(), ..Default::default() };
        let b = build_blocks(&d, &key, &tile, 1.0, 1.0);
        assert_eq!(b.shape(), crate::render::dots::SHAPE_CELL);
        for (bi, bucket) in b.buckets.iter().enumerate() {
            assert_eq!(b.comps[bi].len(), bucket.len());
            for (j, blk) in bucket.iter().enumerate() {
                let want = if blk.pop == 5.0 { &cats[0] } else { &cats[1] };
                assert_eq!(b.comps[bi][j], Composition::from_counts(want).texel());
            }
        }
        let plain = GridTile { cats: Vec::new(), ..tile };
        assert!(build_blocks(&d, &key, &plain, 1.0, 1.0).comps.iter().all(Vec::is_empty));
    }

    #[test]
    fn a_block_is_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<DotBlock>(), 16);
    }

    #[test]
    fn slots_equal_the_ceiling_count_and_first_dots_are_contiguous() {
        let (d, key) = fixture_30s();
        let tile = GridTile { cells: vec![(0, 70.0, 70.0), (5, 1.0, 1.0), (9, 0.3, 0.3)], total: 71.3, ..Default::default() };
        let b = build_blocks(&d, &key, &tile, 1.0, 1.0);
        assert_eq!(b.slots, 70 + 1 + 1);
        // 70 = 64 + 4 + 1 + 1
        assert_eq!(b.buckets[3].len(), 1);
        assert_eq!(b.buckets[1].len(), 1);
        assert_eq!(b.buckets[0].len(), 4); // 2 for cell 0, 1 each for cells 5 and 9
        let mut firsts: Vec<u32> = b.buckets.iter().flatten().filter(|x| x.pop == 70.0).map(|x| x.first_dot).collect();
        firsts.sort();
        assert_eq!(firsts, vec![0, 64, 68, 69]);
    }

    #[test]
    fn every_live_dot_has_a_slot_up_to_four_times_ppd_build() {
        use crate::overlay::dotgrid::hash;
        let (d, key) = fixture_30s();
        let tile = GridTile { cells: (0..2_000).map(|i| (i, (i % 300) as f32 * 1.3, 0.0)).map(|(i, p, _)| (i, p, p)).collect(), total: 0.0, ..Default::default() };
        let b = build_blocks(&d, &key, &tile, 8.0, 1.0);
        // Each cell's key and population, from its blocks (a cell whose
        // dots are all dead at ppd_build has none, and none alive above).
        let cells: std::collections::HashMap<[u32; 2], f32> =
            b.buckets.iter().flatten().map(|x| ([x.origin[0].to_bits(), x.origin[1].to_bits()], x.pop)).collect();
        for ppd in [8.0f32, 11.0, 31.9] {
            let live: u64 = cells
                .iter()
                .map(|(o, pop)| {
                    let k = hash::cell_key([f32::from_bits(o[0]), f32::from_bits(o[1])]);
                    (0..(pop / ppd).ceil() as u32 + 1).filter(|&i| hash::dot_alive(*pop, ppd, i, k)).count() as u64
                })
                .sum();
            assert!(b.slots >= live);
            assert!(b.slots as f64 <= 4.0 * live as f64 + tile.cells.len() as f64, "waste over 4x at ppd {ppd}");
        }
    }

    /// Fix round 1 (MG-11): a slot only for a dot that can be alive in the
    /// tile's rebuild window, `ppd >= ppd_build` -- index `i` with
    /// `i + h_t < pop / ppd_build`. A sparse cell whose one dot is dead at
    /// `ppd_build` is dead at every ppd the tile is drawn at, so it gets no
    /// block; before, every populated cell got at least one.
    #[test]
    fn a_dot_dead_at_ppd_build_gets_no_slot_and_every_dot_alive_above_it_does() {
        use crate::overlay::dotgrid::hash;
        let (d, key) = fixture_30s();
        let w = crate::overlay::dotgrid::grid::cell_window(&d, key.level, key.tile);
        let cells: Vec<(u32, f32, f32)> =
            (0..5_000u32).map(|i| (i * 7 % (w.rows * w.cols), 0.05 + (i % 97) as f32 * 0.9, 0.0)).map(|(i, p, _)| (i, p, p)).collect();
        let tile = GridTile { cells, total: 0.0, ..Default::default() };
        let ppd_build = 8.0;
        let b = build_blocks(&d, &key, &tile, ppd_build, OVERLAY_LYON_PER_UNIT_WORLD);
        // Per cell: the dots its blocks give slots to, from first_dot.
        let mut slots_of: std::collections::HashMap<[u32; 2], (f32, u32)> = std::collections::HashMap::new();
        for (bi, bucket) in b.buckets.iter().enumerate() {
            for blk in bucket {
                let e = slots_of.entry([blk.origin[0].to_bits(), blk.origin[1].to_bits()]).or_insert((blk.pop, 0));
                e.1 = e.1.max(blk.first_dot + DOT_BUCKETS[bi]);
            }
        }
        let mut alive_at_build = 0u64;
        for (origin, (pop, n)) in &slots_of {
            let k = hash::cell_key([f32::from_bits(origin[0]), f32::from_bits(origin[1])]);
            for ppd in [ppd_build, ppd_build * 1.3, ppd_build * 2.0] {
                for i in 0..(pop / ppd).ceil() as u32 + 2 {
                    if hash::dot_alive(*pop, ppd, i, k) {
                        assert!(i < *n, "dot {i} of a cell of {pop} alive at ppd {ppd} has no slot ({n})");
                    }
                }
            }
            alive_at_build += (0..*n).filter(|&i| hash::dot_alive(*pop, ppd_build, i, k)).count() as u64;
        }
        assert_eq!(b.slots, alive_at_build, "every slot is a dot alive at ppd_build");
        let ceil: u64 = tile.cells.iter().map(|c| (c.1 / ppd_build).ceil() as u64).sum();
        assert!(b.slots < ceil, "{} slots against {ceil} ceiling slots", b.slots);
        assert!(slots_of.len() < tile.cells.len(), "a cell whose dot is dead at ppd_build has no block");
    }

    /// Fix round 2: the blocks come out in the order the upload draws
    /// them (descending `max(pop, pop_b) / first_dot`), so the paint
    /// thread's stable sort meets sorted runs -- it took 6-15 ms a tile
    /// and caused the zoom sweep's frames over 33 ms.
    #[test]
    fn buckets_come_out_in_the_uploads_draw_order() {
        let (d, mut key) = fixture_30s();
        for live in [false, true] {
            key.epoch_b = live.then_some(2030);
            let tile = GridTile {
                cells: (0..3_000u32).map(|i| (i, (i * 37 % 1_000) as f32 + 0.5, (i * 53 % 900) as f32)).collect(),
                total: 0.0,
                ..Default::default()
            };
            let b = build_blocks(&d, &key, &tile, 1.0, OVERLAY_LYON_PER_UNIT_WORLD);
            for (i, bucket) in b.buckets.iter().enumerate() {
                let k = |j: usize| {
                    let x = bucket[j];
                    let top = if live { x.pop.max(b.pop_b[i][j]) } else { x.pop };
                    if x.first_dot == 0 { f32::INFINITY } else { top / x.first_dot as f32 }
                };
                assert!((1..bucket.len()).all(|j| k(j - 1) >= k(j)), "bucket {i} (live {live}) is not in draw order");
            }
        }
    }

    #[test]
    fn live_mode_counts_use_the_larger_epoch() {
        let (d, mut key) = fixture_30s();
        key.epoch_b = Some(2030);
        let tile = GridTile { cells: vec![(0, 3.0, 10.0), (1, 10.0, 3.0)], total: 13.0, ..Default::default() };
        let b = build_blocks(&d, &key, &tile, 1.0, 1.0);
        assert_eq!(b.slots, 20);
        assert_eq!(b.pop_b.iter().map(Vec::len).sum::<usize>(), b.buckets.iter().map(Vec::len).sum::<usize>());
    }

    #[test]
    fn origins_are_tile_local_and_within_the_tile_plus_one_cell() {
        let (d, key) = fixture_30s();
        let w = crate::overlay::dotgrid::grid::cell_window(&d, key.level, key.tile);
        let tile = GridTile { cells: vec![(0, 1.0, 1.0), (w.rows * w.cols - 1, 1.0, 1.0)], total: 2.0, ..Default::default() };
        let b = build_blocks(&d, &key, &tile, 1.0, 4096.0);
        for blk in b.buckets.iter().flatten() {
            assert!(blk.origin[0] > -b.cell_width && blk.origin[0] < 4096.0 + b.cell_width);
        }
    }

    /// Final review: the one place a dot tile's uniform and model origin
    /// are made (sync and capture both call it).
    #[test]
    fn dot_params_for_a_tile_place_it_at_its_nw_corner() {
        let blocks = TileBlocks { cell_width: 3.5, ..TileBlocks::default() };
        let exclude = [[0.0, 0.0, 1.0, 1.0], DotParams::NONE_EXCLUDED];
        let (p, origin) = DotParams::for_tile(TileId { z: 6, x: 16, y: 48 }, &blocks, exclude, 250.0, 1.5);
        let world = f64::from(crate::render::WORLD_SIZE);
        assert_eq!(origin, (0.25 * world, 0.75 * world));
        assert_eq!(p.tile_origin_y, 0.75);
        assert_eq!((p.exclude_a, p.exclude_b), (exclude[0], exclude[1]));
        assert_eq!((p.ppd, p.cell_width, p.radius_px, p.t, p.ramp_row, p.live), (250.0, 3.5, 1.5, 0.0, 0, 0));
        assert_eq!(p.unit_world_per_lyon, (1.0 / OVERLAY_LYON_PER_UNIT_WORLD) as f32);
        assert_eq!((p.shape, p.category_mask, p.color_mode), (crate::render::dots::SHAPE_CELL, 0xFF, 0));
    }
}
