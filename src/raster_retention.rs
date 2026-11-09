//! Imagery retention, shared by every shell: what decoded imagery is kept,
//! what may be evicted, and the byte budget it is evicted against.
//!
//! Native `MapView` has held its decoded imagery in a byte-budgeted
//! `TileCache` since the imagery basemap landed, with the frame's WORKING
//! SET -- the cover, each cover tile's ancestor [`RASTER_FLOOR_LEVELS`]
//! zooms up, and the z0 tile -- kept out of eviction. The browser and phone
//! shells held theirs in a plain `HashMap` that nothing ever removed from:
//! every imagery tile a session had ever decoded stayed in RAM, so a long
//! pan over satellite imagery grew the heap by `tile_size^2 * 4` bytes per
//! new tile, forever (256 KB a tile at 256 px; the 5-minute drifting
//! circular tour in `tests` below touches ~1,100 of them, ~290 MB, where
//! this cache holds 32 MB from the first 30 seconds on).
//!
//! This module is that native policy as one definition, `#[path]`-included
//! by `web/src/lib.rs` and `ios/src/lib.rs`, so the shells cannot drift
//! from it: [`working_set`] is the function `MapView::raster_working_set`
//! calls, and [`DecodedImagery`] is the cache the shells hold. Native keeps
//! its own `TileCache` field (it drops a tile's decoded copy at upload and
//! is measured per cache by `drop_cache_for_measurement`), but evicts by
//! the same working set and weighs by the same [`image_bytes`].
//!
//! Pure: no device, no network, no clock -- so the bound is tested here,
//! under a synthetic tour, rather than only observed in a browser.

use std::collections::HashSet;

use crate::geo::TileId;
use crate::tile_source::TileCache;

/// How many zoom levels above the imagery cover its always-kept floor sits.
/// See [`working_set`].
pub const RASTER_FLOOR_LEVELS: u8 = 3;

/// One megabyte, as `map_view.rs` writes its budgets.
const MB: usize = 1_000_000;

/// The shells' decoded-imagery byte budget: **32 MB**, native's
/// `RASTER_DECODED_BUDGET_BYTES` on iOS/wasm32 (128 MB divided by its
/// `CACHE_BUDGET_DIVISOR` of 4), because the browser tab and the phone are
/// exactly the targets that divisor exists for.
///
/// 125 tiles at 256 px, 31 at 512 px. Like native's, it bounds HISTORY: the
/// working set is kept even when it alone is over budget (see
/// [`DecodedImagery::insert`]), so a globe cover larger than the budget is
/// never evicted from under itself.
pub const SHELL_DECODED_BUDGET_BYTES: usize = 32 * MB;

/// The shells' decoded-imagery entry bound: native's `MAX_VISIBLE_TILES`,
/// the most tiles one cover may ask for. At 256 px the byte budget binds
/// first; the entry bound is what holds for tiny tiles.
pub const SHELL_DECODED_MAX_ENTRIES: usize = crate::render::cover_plan::MAX_COVER_TILES;

/// How many known-absent tiles (a 404 from the source) a shell remembers:
/// native's `ABSENT_CAPACITY`, four covers. Without a bound the set grew by
/// one entry per absent tile ever seen; with it, a tile forgotten is at
/// worst asked for once more.
pub const SHELL_ABSENT_CAPACITY: usize = crate::render::cover_plan::MAX_COVER_TILES * 4;

/// The imagery a frame fetches and keeps: `cover`, then its FLOOR -- each
/// cover tile's ancestor [`RASTER_FLOOR_LEVELS`] zooms up, deduplicated --
/// then the z0 tile under everything, in that order; and the same tiles as
/// a set, which is what eviction must spare.
///
/// The floor is what a tile that has not landed yet is drawn from (native's
/// `raster_stand_ins` walks up to the nearest resident ancestor). Three
/// levels up is 1/64th of the cover's tiles plus its edges, so keeping it
/// costs a few percent of the cover; z0 is one tile, for the globe's limb.
pub fn working_set(cover: &[TileId]) -> (Vec<TileId>, HashSet<TileId>) {
    let mut working: Vec<TileId> = cover.to_vec();
    let mut seen: HashSet<TileId> = cover.iter().copied().collect();
    for &tile in cover {
        let levels = RASTER_FLOOR_LEVELS.min(tile.z);
        let floor = TileId { z: tile.z - levels, x: tile.x >> levels, y: tile.y >> levels };
        if seen.insert(floor) {
            working.push(floor);
        }
    }
    let world = TileId { z: 0, x: 0, y: 0 };
    if !cover.is_empty() && seen.insert(world) {
        working.push(world);
    }
    (working, seen)
}

/// What one decoded imagery tile costs in RAM: its RGBA pixels.
pub fn image_bytes(image: &image::RgbaImage) -> usize {
    let (width, height) = image.dimensions();
    (width as usize) * (height as usize) * 4
}

fn weigh<P: std::ops::Deref<Target = image::RgbaImage>>(image: &P) -> usize {
    image_bytes(image)
}

/// A shell's decoded imagery: a byte-budgeted LRU that never evicts the
/// current working set. `P` is the shell's shared pointer (`Rc` in the
/// browser, `Arc` on the phone).
pub struct DecodedImagery<P> {
    cache: TileCache<P>,
    keep: HashSet<TileId>,
}

impl<P: std::ops::Deref<Target = image::RgbaImage>> DecodedImagery<P> {
    /// The shells' cache: [`SHELL_DECODED_MAX_ENTRIES`] and
    /// [`SHELL_DECODED_BUDGET_BYTES`].
    pub fn new() -> Self {
        Self::with_budget(SHELL_DECODED_MAX_ENTRIES, SHELL_DECODED_BUDGET_BYTES)
    }

    pub fn with_budget(max_entries: usize, max_bytes: usize) -> Self {
        Self { cache: TileCache::with_byte_budget(max_entries, max_bytes, weigh::<P>), keep: HashSet::new() }
    }

    /// Records this frame's imagery cover; its [`working_set`] is what
    /// [`Self::insert`] may not evict until the next call. Returns the
    /// working set in fetch order.
    pub fn set_cover(&mut self, cover: &[TileId]) -> Vec<TileId> {
        let (working, keep) = working_set(cover);
        self.keep = keep;
        working
    }

    /// Lands a decoded tile, evicting least-recently-used tiles outside the
    /// working set until the cache is back inside its budget.
    pub fn insert(&mut self, tile: TileId, image: P) {
        let bytes = image_bytes(&image);
        let keep = &self.keep;
        self.cache.put_with_bytes_keeping(tile, image, bytes, |t| keep.contains(t));
        // `put_with_bytes_keeping` bounds the ENTRY count with the keep set
        // spared; the byte budget is enforced here the same way, so a run of
        // large tiles cannot push the working set out either.
        self.trim();
    }

    fn trim(&mut self) {
        let Some(budget) = self.cache.byte_budget() else { return };
        while self.cache.held_bytes() > budget {
            let victim = self.cache.iter().map(|(t, _)| *t).filter(|t| !self.keep.contains(t)).last();
            let Some(victim) = victim else { break };
            self.cache.remove(&victim);
        }
    }

    pub fn contains(&self, tile: &TileId) -> bool {
        self.cache.contains(tile)
    }

    /// The tile, marked as recently used.
    pub fn get(&mut self, tile: &TileId) -> Option<&P> {
        self.cache.get(tile)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    pub fn held_bytes(&self) -> usize {
        self.cache.held_bytes()
    }
}

impl<P: std::ops::Deref<Target = image::RgbaImage>> Default for DecodedImagery<P> {
    fn default() -> Self {
        Self::new()
    }
}

/// Tiles a source has said it has nothing for, bounded by
/// [`SHELL_ABSENT_CAPACITY`] -- native's `absent`/`raster_absent`.
pub struct AbsentTiles(TileCache<()>);

impl AbsentTiles {
    pub fn new() -> Self {
        Self(TileCache::new(SHELL_ABSENT_CAPACITY))
    }

    pub fn insert(&mut self, tile: TileId) {
        self.0.put(tile, ());
    }

    pub fn contains(&self, tile: &TileId) -> bool {
        self.0.contains(tile)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Default for AbsentTiles {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A 5-minute circular tour at 60 frames a second: the camera circles a
    /// point at city zoom, breathing between z12 and z14, and every imagery
    /// tile the cover asks for lands the frame it is asked for -- the worst
    /// case for retention, since nothing is ever still in flight.
    ///
    /// The cover is the camera's 5x4 tile window (a 1280x1024 viewport of
    /// 256 px tiles), not `cover_plan::flat_cover`, so this test is about
    /// the cache and nothing else.
    fn tour_covers(frames: usize, laps: f64) -> impl Iterator<Item = Vec<TileId>> {
        (0..frames).map(move |frame| {
            let t = frame as f64 / frames as f64;
            let angle = t * laps * std::f64::consts::TAU;
            let zoom = 13.0 + (angle * 3.0).sin();
            let z = zoom.round() as u8;
            let n = f64::from(1u32 << z);
            // A 0.3-degree-radius circle around Singapore whose centre walks
            // a degree east over the tour, so later laps reach new ground
            // (`laps` of 1.0 is a closed circle).
            let drift = if laps > 1.0 { t } else { 0.0 };
            let lon = 103.85 + drift + 0.3 * angle.cos();
            let lat: f64 = 1.29 + 0.3 * angle.sin();
            let x = (lon + 180.0) / 360.0 * n;
            let lat_rad = lat.to_radians();
            let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
            let (cx, cy) = (x as i64, y as i64);
            let mut cover = Vec::new();
            for dy in -2..2 {
                for dx in -2..3 {
                    cover.push(TileId { z, x: (cx + dx) as u32, y: (cy + dy) as u32 });
                }
            }
            cover
        })
    }

    /// One zero-filled 256 px tile. Zero-filled, so the allocator hands
    /// back untouched pages and the tour stays cheap in CI.
    fn tile_image() -> Arc<image::RgbaImage> {
        Arc::new(image::RgbaImage::new(256, 256))
    }

    const TILE_BYTES: usize = 256 * 256 * 4;

    #[test]
    fn the_working_set_is_the_cover_its_floor_and_the_world() {
        let cover = [TileId { z: 5, x: 16, y: 10 }, TileId { z: 5, x: 17, y: 10 }];
        let (working, keep) = working_set(&cover);
        assert_eq!(
            working,
            vec![cover[0], cover[1], TileId { z: 2, x: 2, y: 1 }, TileId { z: 0, x: 0, y: 0 }],
            "both cover tiles share one floor tile three levels up, then z0"
        );
        assert_eq!(keep.len(), 4);
        assert!(working_set(&[]).0.is_empty(), "no cover, nothing kept -- not even z0");
        let (shallow, _) = working_set(&[TileId { z: 1, x: 1, y: 0 }]);
        assert_eq!(shallow, vec![TileId { z: 1, x: 1, y: 0 }, TileId { z: 0, x: 0, y: 0 }]);
    }

    /// The bound itself, over the whole 5-minute tour (18,000 frames): the
    /// cache never holds more than its budget plus the working set, while
    /// the unbounded `HashMap` it replaced holds every tile ever decoded.
    #[test]
    fn a_five_minute_tour_stays_inside_the_budget() {
        let mut cache: DecodedImagery<Arc<image::RgbaImage>> = DecodedImagery::new();
        let mut unbounded: HashSet<TileId> = HashSet::new();
        let image = tile_image();
        let mut peak = 0usize;
        let mut largest_working = 0usize;
        for (frame, cover) in tour_covers(18_000, 3.0).enumerate() {
            if frame % 1_800 == 0 {
                // The growth curve, every 30 s of tour: `--nocapture` prints it.
                eprintln!(
                    "tour t={:>3}s bounded {:>6.1} MB ({:>3} tiles) | unbounded HashMap {:>7.1} MB ({:>4} tiles)",
                    frame / 60,
                    cache.held_bytes() as f64 / 1e6,
                    cache.len(),
                    (unbounded.len() * TILE_BYTES) as f64 / 1e6,
                    unbounded.len()
                );
            }
            let working = cache.set_cover(&cover);
            largest_working = largest_working.max(working.len());
            for tile in working {
                if !cache.contains(&tile) {
                    cache.insert(tile, image.clone());
                }
                unbounded.insert(tile);
                assert!(cache.contains(&tile), "a working-set tile was evicted the frame it landed");
            }
            for tile in &cover {
                assert!(cache.contains(tile), "a cover tile was evicted while on screen");
            }
            peak = peak.max(cache.held_bytes());
        }
        let ceiling = SHELL_DECODED_BUDGET_BYTES + largest_working * TILE_BYTES;
        assert!(peak <= ceiling, "peak {peak} over budget + working set {ceiling}");
        assert!(cache.held_bytes() <= SHELL_DECODED_BUDGET_BYTES, "settles inside the budget");
        // The `HashMap` this replaced: every tile the tour ever touched.
        assert!(
            unbounded.len() * TILE_BYTES > 10 * SHELL_DECODED_BUDGET_BYTES,
            "the tour must be long enough to matter: {} tiles",
            unbounded.len()
        );
    }

    /// The byte budget binds even for tiles bigger than the entry bound
    /// assumes: 512 px tiles (1 MiB each) are trimmed to the budget too.
    #[test]
    fn large_tiles_are_bounded_by_bytes_not_entries() {
        let mut cache: DecodedImagery<Arc<image::RgbaImage>> = DecodedImagery::new();
        let big = Arc::new(image::RgbaImage::new(512, 512));
        cache.set_cover(&[]);
        for x in 0..200 {
            cache.insert(TileId { z: 10, x, y: 0 }, big.clone());
        }
        assert!(cache.held_bytes() <= SHELL_DECODED_BUDGET_BYTES);
        assert_eq!(cache.len(), SHELL_DECODED_BUDGET_BYTES / (512 * 512 * 4));
        assert!(cache.contains(&TileId { z: 10, x: 199, y: 0 }), "the newest survives");
        assert!(!cache.contains(&TileId { z: 10, x: 0, y: 0 }), "the oldest goes");
    }

    /// A working set larger than the budget is kept whole -- the cache runs
    /// over rather than evict a tile on screen (native's "black tiles
    /// blink" bug) -- and comes back inside once the cover shrinks.
    #[test]
    fn a_cover_over_budget_is_kept_and_released() {
        let mut cache: DecodedImagery<Arc<image::RgbaImage>> = DecodedImagery::with_budget(512, 10 * TILE_BYTES);
        let cover: Vec<TileId> = (0..20).map(|x| TileId { z: 4, x, y: 0 }).collect();
        let working = cache.set_cover(&cover);
        for &t in &working {
            cache.insert(t, tile_image());
        }
        for t in &working {
            assert!(cache.contains(t));
        }
        cache.set_cover(&[TileId { z: 4, x: 0, y: 0 }]);
        cache.insert(TileId { z: 4, x: 0, y: 1 }, tile_image());
        assert!(cache.held_bytes() <= 10 * TILE_BYTES);
    }

    #[test]
    fn absent_tiles_are_bounded() {
        let mut absent = AbsentTiles::new();
        for x in 0..(SHELL_ABSENT_CAPACITY as u32 * 3) {
            absent.insert(TileId { z: 14, x, y: 0 });
        }
        assert_eq!(absent.len(), SHELL_ABSENT_CAPACITY);
        assert!(absent.contains(&TileId { z: 14, x: SHELL_ABSENT_CAPACITY as u32 * 3 - 1, y: 0 }));
    }

    /// The leak test, deterministic half: every image the cache was ever
    /// handed is tracked by a `Weak`, so what is still ALIVE after a closed
    /// tour is measured exactly, whatever else the test process allocates.
    /// Lap 1 fills the cache; laps 2-4 walk the same circle again, evicting
    /// and re-decoding all the way (the circle holds ~4x the budget). After
    /// them the live images are exactly what the cache holds -- nothing an
    /// eviction dropped is kept alive by anything -- and no more than it
    /// held after lap 1. The process-heap half, which needs a process of its
    /// own, is `tests/memory_leak.rs`.
    #[test]
    fn heap_returns_to_steady_state_after_a_closed_tour() {
        fn run_lap(cache: &mut DecodedImagery<Arc<image::RgbaImage>>, made: &mut Vec<std::sync::Weak<image::RgbaImage>>) {
            for cover in tour_covers(1_200, 1.0) {
                for tile in cache.set_cover(&cover) {
                    if !cache.contains(&tile) {
                        let image = tile_image();
                        made.push(Arc::downgrade(&image));
                        cache.insert(tile, image);
                    }
                }
            }
        }
        let live_bytes = |made: &[std::sync::Weak<image::RgbaImage>]| -> usize {
            made.iter().filter_map(std::sync::Weak::upgrade).map(|i| image_bytes(&i)).sum()
        };
        let mut made = Vec::new();
        let mut cache: DecodedImagery<Arc<image::RgbaImage>> = DecodedImagery::new();
        run_lap(&mut cache, &mut made);
        let steady = live_bytes(&made);
        let after_first = made.len();
        for _ in 0..3 {
            run_lap(&mut cache, &mut made);
        }
        let redecoded = made.len() - after_first;
        assert!(redecoded * TILE_BYTES > 4 * SHELL_DECODED_BUDGET_BYTES, "the laps must churn the cache: {redecoded}");
        let after = live_bytes(&made);
        assert_eq!(after, cache.held_bytes(), "an evicted image is still alive");
        assert!(after <= steady, "live imagery rose from {steady} to {after} bytes over three closed laps");
    }
}
