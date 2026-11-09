//! Hostile track files are refused within bounded memory (Task 10 review,
//! F1): the 64 MiB byte cap alone let a line of commas or a GeoJSON
//! `Value` tree allocate gigabytes before any check ran.
//!
//! This file must contain exactly one `#[test]`: the global counting
//! allocator counts every thread, so a second test running in parallel
//! would add its allocations to this one's peak.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use gpui_maps::story::script::TrackStyle;
use gpui_maps::story::tracks::{parse_track, MAX_TRACK_BYTES, MAX_TRACK_POINTS};

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
    // No `realloc` override: a growth's transient is counted in full.
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

const MIB: usize = 1 << 20;

#[test]
fn hostile_tracks_are_refused_in_bounded_memory() {
    // 1. A 60 MiB line of commas: refused at its length, allocating
    //    nothing per field (was ~2.5 GB of `Vec<&str>` + `Vec<String>`).
    let mut commas = b"t,lon,lat\n".to_vec();
    commas.resize(60 * MIB, b',');
    let (r, peak) = peak_of(|| parse_track(&commas, "c", TrackStyle::default(), 0.0).map(|_| ()));
    let e = r.expect_err("a 60 MiB line is refused");
    println!("comma line: {} at {} -- peak {peak} B", e.code, e.path);
    assert_eq!((e.code, e.path.as_str()), ("track_bad_row", "/lines/2"));
    assert!(peak < 64 << 10, "a refused comma line peaked at {peak} B");
    drop(commas);

    // 2. A large valid GeoJSON (400k vertices, ~13 MB). Its Track holds
    //    48 B a vertex; the parse may peak at a small multiple of that
    //    (the typed Vecs' doubling, the samples, the Arc copies), never
    //    the ~100 B a vertex a `Value` tree costs on top.
    let n = 400_000;
    let coords: Vec<String> = (0..n).map(|i| format!("[{:.5},1.3]", 103.0 + f64::from(i) * 1e-5)).collect();
    let times: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    let json = format!(
        r#"{{"type":"Feature","properties":{{"name":"x","times":[{}]}},"geometry":{{"type":"LineString","coordinates":[{}]}}}}"#,
        times.join(","),
        coords.join(",")
    );
    drop((coords, times));
    let (r, peak) = peak_of(|| parse_track(json.as_bytes(), "g", TrackStyle::default(), 0.0));
    let track = r.expect("a valid large track");
    assert_eq!(track.span(), (0.0, f64::from(n - 1)));
    let output = 48 * n as usize;
    println!("large GeoJSON: {} bytes in, output ~{output} B, peak {peak} B ({:.2}x)", json.len(), peak as f64 / output as f64);
    assert!(peak <= 3 * output, "a {n}-vertex GeoJSON peaked at {peak} B, over 3x its {output} B output");
    drop((track, json));

    // 3. A GeoJSON past the vertex cap, under the byte cap: refused with
    //    the typed Vecs stopped at the cap (24 + 16 B a vertex).
    let over = MAX_TRACK_POINTS + 10;
    let mut big = String::from(r#"{"type":"Feature","properties":{"times":["#);
    big.push_str(&vec!["0"; over].join(","));
    big.push_str(r#"]},"geometry":{"type":"LineString","coordinates":["#);
    big.push_str(&vec!["[0,0]"; over].join(","));
    big.push_str("]}}");
    assert!(big.len() <= MAX_TRACK_BYTES);
    let (r, peak) = peak_of(|| parse_track(big.as_bytes(), "o", TrackStyle::default(), 0.0).map(|_| ()));
    let e = r.expect_err("past the vertex cap");
    println!("over-cap GeoJSON: {} bytes, {} -- peak {peak} B", big.len(), e.code);
    assert_eq!((e.code, e.path.as_str()), ("track_too_large", "/geometry/coordinates"));
    // The capped Vecs: 1M x 24 B coordinates + 1M x 16 B times, each up to
    // 2x from doubling.
    let bound = 2 * (24 + 16) * MAX_TRACK_POINTS;
    assert!(peak <= bound, "an over-cap GeoJSON peaked at {peak} B, over {bound} B");
}
