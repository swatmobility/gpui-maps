//! Style-matching micro-benchmark: `mvt::decode`, `match_tile_static`,
//! `resolve_plan` and `match_tile_symbols` p50/p90 over N reps, on any
//! (tile, style) pair.
//!
//! ```text
//! cargo run --release --example match_bench -- TILE.pbf STYLE.json [REPS]
//! ```
use std::time::Instant;

use gpui_maps::mvt;
use gpui_maps::style::{match_tile_static, match_tile_symbols, resolve_plan};
use gpui_maps::style_spec;

fn pct(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn time<T>(reps: usize, mut f: impl FnMut() -> T) -> (f64, f64) {
    for _ in 0..3 {
        std::hint::black_box(f());
    }
    let samples: Vec<f64> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(f());
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    (pct(samples.clone(), 0.5), pct(samples, 0.9))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tile_path = &args[1];
    let style_path = &args[2];
    let reps: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(50);
    let bytes = std::fs::read(tile_path).expect("tile");
    let style = style_spec::parse_and_compile(&std::fs::read(style_path).expect("style")).expect("style compiles");
    let tile = mvt::decode(bytes.clone()).expect("decodes");
    let features: usize = tile.layers.iter().map(|l| l.features.len()).sum();
    println!("{tile_path} + {style_path}: {} bytes, {features} features, {} style layers", bytes.len(), style.layers.len());
    if std::env::var_os("MATCH_ONLY_SECS").is_some() {
        // For a sampling profiler: spin on `match_tile_static` alone.
        let secs: f64 = std::env::var("MATCH_ONLY_SECS").unwrap().parse().unwrap_or(8.0);
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < secs {
            std::hint::black_box(match_tile_static(&tile, &style));
        }
        return;
    }
    let m = match_tile_static(&tile, &style);
    let rows: [(&str, (f64, f64)); 5] = [
        ("decode", time(reps, || mvt::decode(bytes.clone()).unwrap())),
        ("match_tile_static", time(reps, || match_tile_static(&tile, &style))),
        ("resolve_plan z14", time(reps, || resolve_plan(&tile, &style, &m, 14))),
        ("resolve_plan z12", time(reps, || resolve_plan(&tile, &style, &m, 12))),
        ("match_tile_symbols z14", time(reps, || match_tile_symbols(&tile, &style, 14, 1.0))),
    ];
    for (name, (p50, p90)) in rows {
        println!("  {name:<24} p50 {p50:>8.3} ms  p90 {p90:>8.3} ms");
    }
}
