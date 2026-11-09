//! The worst-case memory of one cluster tile's decode, measured under a
//! counting allocator rather than restated from the doc (Task 12 review,
//! M1: the doc said 896 KiB while `read_to_end`'s doubling made the body
//! alone 1 MiB, and the unit test only repeated the doc's arithmetic).
//!
//! The worst tile the decoder can be handed: a body of exactly the inflate
//! cap whose header claims the most records a tile may hold, every one of
//! them valid, followed by padding. The decoder inflates all of it
//! (accepted), decodes all 4,096 records (the records `Vec` at its
//! largest) and only then refuses the trailing bytes -- so the body and
//! the largest records `Vec` are live together, which no accepted tile can
//! beat. One test in this binary, so nothing else allocates meanwhile.
//!
//! This file must contain exactly one `#[test]`: the global counting
//! allocator counts every thread, so a second test running in parallel
//! would add its allocations to this one's peak.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use gpui_maps::overlay::clusters_tile::{
    decode, encode, ClusterRecord, DOTS_ZOOM, MAX_INFLATED_BYTES, MAX_RECORDS_PER_TILE,
};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let live = LIVE.fetch_add(l.size(), Ordering::SeqCst) + l.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::SeqCst);
    }
    // No `realloc` override: the default allocates the new block before
    // freeing the old, so a growth's transient is counted in full.
}

#[global_allocator]
static A: Counting = Counting;

/// Peak bytes live above the level at entry, while `f` runs.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = f();
    (out, PEAK.load(Ordering::SeqCst) - base)
}

#[test]
fn a_worst_case_cluster_tile_peaks_at_the_documented_896_kib() {
    let recs: Vec<ClusterRecord> = (0..MAX_RECORDS_PER_TILE)
        .map(|i| ClusterRecord {
            lon: 0.0,
            lat: 0.0,
            blocks: 2,
            pop: 1 + i % 3,
            counts: [1, 0, 0, 0, 0, 0, 0, 0],
            expansion_zoom: DOTS_ZOOM,
            leaf: false,
        })
        .collect();
    let mut body = Vec::new();
    flate2::read::GzDecoder::new(&encode(&recs)[..]).read_to_end(&mut body).unwrap();
    assert!(body.len() < MAX_INFLATED_BYTES as usize);
    body.resize(MAX_INFLATED_BYTES as usize, 0);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&body).unwrap();
    let tile = gz.finish().unwrap();
    drop(body);

    let (result, peak) = peak_of(|| decode(&tile));
    let why = format!("{:?}", result.expect_err("padding after the last record is refused"));
    assert!(why.contains("bytes after the last record"), "{why}");

    // The body at exactly the cap (512 KiB) + the records Vec at 4,096 ×
    // 64 B (256 KiB) + the 2,048-record one it grew from (128 KiB).
    let documented = 896 << 10;
    println!("measured peak of a worst-case cluster tile decode: {peak} B (documented {documented} B)");
    assert!(peak <= documented, "the decode peaked at {peak} B, past the documented {documented} B");
    // And the bound is the real peak, not a loose ceiling: the body and the
    // two record buffers are really live together.
    assert!(peak >= documented - (4 << 10), "the decode peaked at only {peak} B: the doc's {documented} B is stale");
}
