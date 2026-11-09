//! The leak test `docs/versioning.md` asks for before 1.0: the heap returns
//! to its steady state after a closed camera tour.
//!
//! Through the decoded-imagery cache every shell holds
//! (`raster_retention::DecodedImagery`), with real 256 px images, over a
//! closed circular tour: lap 1 fills the cache, laps 2-4 walk the same
//! circle again, evicting and re-decoding all the way (the circle holds
//! ~4x the cache's budget). Live heap bytes are counted exactly by a global
//! counting allocator, which is why this is its own test binary.
//!
//! This file must contain exactly one `#[test]`: the counting allocator
//! counts every thread, so a second test running in parallel would add its
//! allocations to this one's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use gpui_maps::geo::TileId;
use gpui_maps::raster_retention::{DecodedImagery, SHELL_DECODED_BUDGET_BYTES};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::SeqCst);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::SeqCst);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            LIVE.fetch_add(new, Ordering::SeqCst);
            LIVE.fetch_sub(l.size(), Ordering::SeqCst);
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const TILE_BYTES: usize = 256 * 256 * 4;

/// One lap of a closed circle round Singapore, breathing z12-z14: the
/// camera's 5x4 window of tiles at each of 1,200 steps.
fn lap() -> impl Iterator<Item = Vec<TileId>> {
    (0..1_200).map(|step| {
        let angle = f64::from(step) / 1_200.0 * std::f64::consts::TAU;
        let z = (13.0 + (angle * 3.0).sin()).round() as u8;
        let n = f64::from(1u32 << z);
        let lon = 103.85 + 0.3 * angle.cos();
        let lat = (1.29 + 0.3 * angle.sin()).to_radians();
        let x = ((lon + 180.0) / 360.0 * n) as i64;
        let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as i64;
        (-2..2).flat_map(|dy| (-2..3).map(move |dx| TileId { z, x: (x + dx) as u32, y: (y + dy) as u32 })).collect()
    })
}

fn run_lap(cache: &mut DecodedImagery<Arc<image::RgbaImage>>) -> usize {
    let mut decoded = 0;
    for cover in lap() {
        for tile in cache.set_cover(&cover) {
            if !cache.contains(&tile) {
                let mut image = image::RgbaImage::new(256, 256);
                image.as_mut().fill(0x5a);
                cache.insert(tile, Arc::new(image));
                decoded += 1;
            }
        }
    }
    decoded
}

#[test]
fn heap_returns_to_steady_state_after_a_closed_tour() {
    let floor = LIVE.load(Ordering::SeqCst);
    let mut cache: DecodedImagery<Arc<image::RgbaImage>> = DecodedImagery::new();
    run_lap(&mut cache);
    let steady = LIVE.load(Ordering::SeqCst);
    let mut redecoded = 0;
    for _ in 0..3 {
        redecoded += run_lap(&mut cache);
    }
    let after = LIVE.load(Ordering::SeqCst);
    eprintln!(
        "closed tour: floor {:.1} MB, steady {:.1} MB, after 3 more laps {:.1} MB, {redecoded} tiles re-decoded",
        floor as f64 / 1e6,
        steady as f64 / 1e6,
        after as f64 / 1e6
    );
    assert!(redecoded * TILE_BYTES > 4 * SHELL_DECODED_BUDGET_BYTES, "the laps must churn the cache: {redecoded}");
    // The cache's own bookkeeping (LRU nodes, the keep set) may sit at a
    // different capacity; one tile's worth covers that, and a leak of even
    // one image per re-decode would be hundreds of MB.
    assert!(
        after <= steady + TILE_BYTES,
        "heap rose {} bytes over three closed laps (steady {steady}, after {after})",
        after.saturating_sub(steady)
    );
    assert!(steady - floor <= SHELL_DECODED_BUDGET_BYTES + 2 * TILE_BYTES + (1 << 20), "steady state is the budget");
    drop(cache);
    assert!(LIVE.load(Ordering::SeqCst) <= floor + (1 << 16), "dropping the cache frees it");
}
