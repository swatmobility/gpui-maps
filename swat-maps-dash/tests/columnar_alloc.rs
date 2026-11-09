//! D4: decoding a million rows allocates per column, not per row. Its own
//! test binary, because it installs a counting global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use swat_maps_dash::connect::rowbinary::{decode, encode};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn rowbinary_1m_decode_allocates_one_vec_per_column() {
    const ROWS: u64 = 1_000_000;
    let mut body = encode::header(&[("mesh", "String"), ("moves", "UInt64"), ("share", "Nullable(Float64)")]);
    for i in 0..ROWS {
        encode::string(b"53394611", &mut body);
        body.extend(i.to_le_bytes());
        body.push(0);
        body.extend(0.5f64.to_le_bytes());
    }
    let before = ALLOCS.load(Ordering::Relaxed);
    let t = decode(&body, ROWS).unwrap();
    let allocs = ALLOCS.load(Ordering::Relaxed) - before;
    assert_eq!(t.row_count(), ROWS as usize);
    // Three columns, each a few growing vectors (values, offsets, two
    // bitmaps): each grows by doubling, so ~log2(1M) = 20 reallocations
    // per vector. Per-row allocation would be a million.
    assert!(allocs < 400, "{allocs} allocations for {ROWS} rows");
}
