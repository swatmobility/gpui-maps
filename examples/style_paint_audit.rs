//! Headless style-vs-schema audit: which vendored style JSON actually paints
//! anything against real OpenFreeMap (OpenMapTiles-schema) planet tiles.
//!
//! A style written for the Mapbox Streets v8 schema (`road`, `admin`,
//! `structure`, ...) compiles cleanly here and then draws a blank map,
//! because none of its `source-layer` names exist in an OpenMapTiles tile.
//! The only way to tell the two apart without a GPU is to count paint ops.
//!
//! Usage:
//!
//!   TILE_DIR=/path/to/cached/tiles cargo run --release --example style_paint_audit
//!
//! `TILE_DIR` holds files named `<city>_z<z>_<x>_<y>.pbf` -- raw, ungzipped
//! MVT bytes as fetched from
//! `https://tiles.openfreemap.org/planet/<version>/{z}/{x}/{y}.pbf`.
//!
//! Nothing here is engine source: it only calls the public `mvt`,
//! `style_spec` and `style` entry points.
//!
//! # THIS HAS NEVER BEEN RUN
//!
//! Stated at the top because an unrun harness that looks run is worse than no
//! harness: it invites the next reader to trust a number it never produced.
//! Every call site below was checked against the real signatures -- `mvt::
//! decode`, `parse_and_compile`, `match_tile`, `match_tile_symbols`, the
//! `PaintOp::{Background,Fill,Line,Extrusion}` variants and the `StyleLayer` field
//! names -- and `gpui-maps` itself compiled clean underneath it, but this
//! file's own compile unit never got scheduled. The machine's `sccache`
//! server was wedged (every `rustc` on the box parked at 0% CPU, and
//! `~/.cargo/config.toml` sets `rustc-wrapper` so it could not be bypassed
//! without a 1780-crate rebuild), and killing shared build state that other
//! work depends on was the wrong trade for a corroborating number.
//!
//! So the style choice in `ios/src/app.rs` does NOT rest on this file. It
//! rests on two things that were actually done: a static bind of every
//! style's `source-layer` names against the layer names present in eight real
//! tiles, and the device itself -- `mapbox-style.json` compiling to 86 layers
//! with no skips and drawing 12 of 12 tiles with no holes. What is missing is
//! the paint-op COUNT per style, which would say how much each one draws
//! rather than merely that it draws.
//!
//! To finish the job once the toolchain is healthy (tiles cache, no refetch):
//!
//! ```text
//! CARGO_TARGET_DIR="$HOME/.cache/gpui-map-style-check" \
//! TILE_DIR=<the cached tiles> \
//!     cargo run --release --example style_paint_audit
//! ```

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;

use gpui_maps::mvt::{self, DecodedTile};
use gpui_maps::style::{self, PaintOp, Rgba, Style, StyleLayer};

struct TileCase {
    label: String,
    zoom: u8,
    tile: DecodedTile,
    layer_names: BTreeSet<String>,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    background: usize,
    fill: usize,
    line: usize,
    extrusion: usize,
    symbol: usize,
}

impl Counts {
    fn total_paint(self) -> usize {
        self.background + self.fill + self.line + self.extrusion
    }
}

fn hex(c: Rgba) -> String {
    format!(
        "#{:02x}{:02x}{:02x}{}",
        (c.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.b.clamp(0.0, 1.0) * 255.0).round() as u8,
        if (c.a - 1.0).abs() > 0.01 { format!(" a={:.2}", c.a) } else { String::new() }
    )
}

fn style_source_layers(s: &Style) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for l in &s.layers {
        match l {
            StyleLayer::Fill { source_layer, .. }
            | StyleLayer::Extrusion { source_layer, .. }
            | StyleLayer::Line { source_layer, .. }
            | StyleLayer::Symbol { source_layer, .. }
            | StyleLayer::Circle { source_layer, .. } => {
                out.insert(source_layer.clone());
            }
            // Neither has a `source-layer`: a background is not read from
            // a tile at all, and a raster layer's pixels arrive from a
            // separate image source (see `StyleLayer::Raster`'s note on
            // why it carries no `source_layer` field to read).
            StyleLayer::Background { .. } | StyleLayer::Raster { .. } => {}
        }
    }
    out
}

/// One style's row in the final table: file name, layer count, total painted
/// features, background colour as hex, and the per-tile breakdown. A tuple
/// rather than a struct because it is built once and read once, twenty lines
/// apart; named here only because five fields is past what an inline type
/// reads as.
type StyleRow = (String, usize, usize, String, Vec<(String, Counts)>);

fn main() -> anyhow::Result<()> {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tile_dir = PathBuf::from(
        std::env::var("TILE_DIR").expect("set TILE_DIR to the cached-tile directory"),
    );

    // ---- load tiles -------------------------------------------------------
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&tile_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "pbf").unwrap_or(false))
        .collect();
    entries.sort();

    let mut tiles = Vec::new();
    for path in entries {
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        // <city>_z<z>_<x>_<y>
        let zoom = stem
            .split('_')
            .find(|p| p.starts_with('z') && p[1..].chars().all(|c| c.is_ascii_digit()))
            .map(|p| p[1..].parse::<u8>().unwrap())
            .expect("tile filename must carry _z<zoom>_");
        let bytes = std::fs::read(&path)?;
        let raw_len = bytes.len();
        let tile = mvt::decode(bytes)?;
        let layer_names: BTreeSet<String> =
            tile.layers.iter().map(|l| l.name.clone()).collect();
        let features: usize = tile.layers.iter().map(|l| l.features.len()).sum();
        println!(
            "tile {stem:<28} z{zoom:<3} {raw_len:>8} B  {:>3} layers  {features:>6} features",
            tile.layers.len()
        );
        tiles.push(TileCase { label: stem, zoom, tile, layer_names });
    }
    println!();

    let all_tile_layers: BTreeSet<String> =
        tiles.iter().flat_map(|t| t.layer_names.iter().cloned()).collect();
    println!("union of source-layers present in these tiles:");
    println!("  {}", all_tile_layers.iter().cloned().collect::<Vec<_>>().join(", "));
    println!();

    // ---- load styles ------------------------------------------------------
    let style_files = [
        "liberty-style.json",
        "swat-biz-light-style.json",
        "swat-routes-dark-style.json",
        "mapbox-standard-dark-style.json",
        "mapbox-standard-dark-reference-style.json",
        "mapbox-streets-style.json",
        "mapbox-style.json",
    ];

    let mut summary: Vec<StyleRow> = Vec::new();

    for name in style_files {
        let path = repo.join("examples").join(name);
        let bytes = std::fs::read(&path)?;
        println!("================================================================");
        println!("STYLE {name}");
        let compiled = match gpui_maps::style_spec::parse_and_compile(&bytes) {
            Ok(s) => s,
            Err(e) => {
                println!("  parse_and_compile FAILED: {e}");
                println!();
                continue;
            }
        };

        let by_kind = compiled.layers.iter().fold(BTreeMap::new(), |mut m, l| {
            *m.entry(l.kind()).or_insert(0usize) += 1;
            m
        });
        let sl = style_source_layers(&compiled);
        let matched: Vec<&String> = sl.iter().filter(|s| all_tile_layers.contains(*s)).collect();
        let unmatched: Vec<&String> =
            sl.iter().filter(|s| !all_tile_layers.contains(*s)).collect();

        println!(
            "  compiled layers: {} ({})",
            compiled.layers.len(),
            by_kind
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "  source-layers: {} total, {} present in OMT tiles, {} absent",
            sl.len(),
            matched.len(),
            unmatched.len()
        );
        println!(
            "    present: {}",
            matched.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        );
        println!(
            "    ABSENT : {}",
            unmatched.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        );

        // how many compiled layers hang off an absent source-layer
        let dead = compiled
            .layers
            .iter()
            .filter(|l| match l {
                StyleLayer::Fill { source_layer, .. }
                | StyleLayer::Extrusion { source_layer, .. }
                | StyleLayer::Line { source_layer, .. }
                | StyleLayer::Symbol { source_layer, .. }
                | StyleLayer::Circle { source_layer, .. } => {
                    !all_tile_layers.contains(source_layer)
                }
                // Same reason as `style_source_layers` above: neither
                // names a source-layer, so neither can hang off an absent
                // one.
                StyleLayer::Background { .. } | StyleLayer::Raster { .. } => false,
            })
            .count();
        println!(
            "  compiled layers bound to an ABSENT source-layer: {dead}/{}",
            compiled.layers.len()
        );

        let mut bg_hex = String::from("(none)");
        let mut per_tile = Vec::new();
        for case in &tiles {
            let plan = style::match_tile(&case.tile, &compiled, case.zoom);
            let symbols = style::match_tile_symbols(&case.tile, &compiled, case.zoom, 8.0);
            let mut c = Counts { symbol: symbols.features.len(), ..Counts::default() };
            for op in &plan.ops {
                match op {
                    PaintOp::Background { color } => {
                        c.background += 1;
                        if bg_hex == "(none)" {
                            bg_hex = hex(*color);
                        }
                    }
                    PaintOp::Fill { .. } => c.fill += 1,
                    PaintOp::Line { .. } => c.line += 1,
                    PaintOp::Extrusion { .. } => c.extrusion += 1,
                }
            }
            per_tile.push((case.label.clone(), c));
        }

        println!("  background colour: {bg_hex}");
        println!(
            "  {:<28} {:>8} {:>8} {:>8} {:>8} {:>8}",
            "tile", "fill", "line", "extrude", "bg", "symbols"
        );
        for (label, c) in &per_tile {
            println!(
                "  {label:<28} {:>8} {:>8} {:>8} {:>8} {:>8}",
                c.fill, c.line, c.extrusion, c.background, c.symbol
            );
        }
        let tot: usize = per_tile.iter().map(|(_, c)| c.total_paint()).sum();
        let totf: usize = per_tile.iter().map(|(_, c)| c.fill).sum();
        let totl: usize = per_tile.iter().map(|(_, c)| c.line).sum();
        println!("  TOTAL paint ops over all tiles: {tot} (fill {totf}, line {totl})");
        println!();

        summary.push((name.to_string(), compiled.layers.len(), tot, bg_hex, per_tile));
    }

    println!("================================================================");
    println!("RANKING by total paint ops over all tiles");
    summary.sort_by_key(|(_, _, tot, _, _)| std::cmp::Reverse(*tot));
    println!(
        "{:<44} {:>7} {:>10} {:>12}",
        "style", "layers", "paint ops", "background"
    );
    for (name, layers, tot, bg, _) in &summary {
        println!("{name:<44} {layers:>7} {tot:>10} {bg:>12}");
    }

    Ok(())
}
