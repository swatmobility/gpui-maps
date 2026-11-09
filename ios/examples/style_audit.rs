//! Headless: what each style in the switcher actually DRAWS, per zoom.
//!
//! The parent repo has `examples/style_paint_audit.rs`, which asks a smaller
//! version of this question and has never been run -- it lives in the
//! `gpui-maps` package, so running it means building gpui, tokio, reqwest and
//! ~1700 other crates through a `sccache` that is not always healthy. This
//! one lives in the iOS spike instead, whose whole dependency graph is the
//! tessellator plus winit, and it answers the question this app needs:
//!
//! * paint ops per style per zoom -- `Background`/`Fill`/`Line`, which is
//!   everything the iOS build can currently put on the glass;
//! * symbol features per style per zoom, split into the ones carrying TEXT
//!   and the ones carrying an ICON, which is everything it currently cannot;
//! * of the icon-bearing ones, how many would become an actual quad against a
//!   given sprite sheet -- a resolved `icon-image` that names nothing in the
//!   sheet draws nothing at all, silently, and that gap is invisible from the
//!   style JSON alone because the names are data-driven (`["get", "class"]`).
//!
//! The third is the number a sprite sheet has to be chosen on. A sheet that
//! loads perfectly and matches no name is a 100 KB no-op.
//!
//! Usage:
//!
//! ```text
//! TILE_DIR=/path/to/cached/tiles cargo run --release --example style_audit
//! ```
//!
//! `TILE_DIR` holds files named `<place>_z<z>_<x>_<y>.pbf` -- raw, ungzipped
//! MVT bytes as fetched from
//! `https://tiles.openfreemap.org/planet/<version>/{z}/{x}/{y}.pbf`.
//!
//! `SPRITE_JSON` is a comma-separated list of sprite MANIFESTS to score the
//! icon names against (the `.json` half of a Mapbox sprite pair; the `.png`
//! is not read, because a name that is absent from the manifest can never
//! reach the atlas). Defaults to the two sheets this repo vendors.
//!
//! Nothing here is engine source. It calls only the public `mvt`,
//! `style_spec`, `style` and `symbol_line` entry points, and parses the
//! sprite manifest with `serde_json` rather than through `sprite::
//! parse_sprite_manifest` -- deliberately, because `src/sprite.rs` names
//! `reqwest::Client` and so cannot be `#[path]`-included into this crate at
//! all. That is the same wall the on-device icon work runs into; see
//! `ios/STYLE-SWITCHER.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use gpui_maps_ios::mvt::{self, DecodedTile};
use gpui_maps_ios::style::{self, PaintOp};
use gpui_maps_ios::styles::STYLES;

struct TileCase {
    label: String,
    /// The tile's own zoom, which is also the zoom its data was cut for.
    tile_z: u8,
    /// The zoom the STYLE is resolved at. Equal to `tile_z` for an ordinary
    /// tile and greater for an overzoomed one -- the phone sits at z16-18
    /// over a source that stops at z14, and a style resolved four buckets up
    /// is a different style, so the audit has to ask both.
    display_z: u8,
    tile: DecodedTile,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    background: usize,
    fill: usize,
    line: usize,
    symbols: usize,
    with_text: usize,
    with_icon: usize,
}

fn main() -> anyhow::Result<()> {
    let tile_dir: PathBuf = std::env::var("TILE_DIR")
        .map_err(|_| anyhow::anyhow!("set TILE_DIR to a directory of <place>_z<z>_<x>_<y>.pbf"))?
        .into();

    let sheets = load_sheets()?;
    let cases = load_tiles(&tile_dir)?;
    anyhow::ensure!(!cases.is_empty(), "no *.pbf found under {}", tile_dir.display());

    println!("tiles:   {} cases from {}", cases.len(), tile_dir.display());
    for (name, names) in &sheets {
        println!("sprites: {name} -- {} entries", names.len());
    }
    println!();

    for choice in STYLES {
        let style = gpui_maps_ios::style_spec::parse_and_compile(choice.json)
            .map_err(|e| anyhow::anyhow!("{}: {e:#}", choice.name))?;
        println!("=== {} ({} B, {} compiled layers)", choice.name, choice.json.len(), style.layers.len());
        println!(
            "{:<26} {:>5} {:>6} {:>6} {:>7} {:>6} {:>6}   {}",
            "tile", "bg", "fill", "line", "symbol", "text", "icon", "icon quads by sheet",
        );

        // Every icon name this style resolved anywhere, and how often -- so a
        // miss can be reported as a NAME rather than as a shortfall in a
        // count.
        let mut wanted: BTreeMap<String, usize> = BTreeMap::new();

        for case in &cases {
            let mut c = Counts::default();
            for op in style::match_tile(&case.tile, &style, case.display_z).ops {
                match op {
                    PaintOp::Background { .. } => c.background += 1,
                    PaintOp::Fill { .. } => c.fill += 1,
                    PaintOp::Line { .. } => c.line += 1,
                }
            }
            let units_per_px =
                gpui_maps_ios::symbol_line::tile_units_per_px(case.display_z, case.tile_z);
            let plan = style::match_tile_symbols(&case.tile, &style, case.display_z, units_per_px);
            let mut quads: BTreeMap<&str, usize> = sheets.keys().map(|k| (k.as_str(), 0)).collect();
            for f in &plan.features {
                c.symbols += 1;
                if f.text.is_some() {
                    c.with_text += 1;
                }
                if let Some(icon) = &f.icon {
                    c.with_icon += 1;
                    *wanted.entry(icon.clone()).or_default() += 1;
                    for (sheet, names) in &sheets {
                        if names.contains(icon.as_str()) {
                            *quads.get_mut(sheet.as_str()).expect("seeded above") += 1;
                        }
                    }
                }
            }
            let by_sheet: Vec<String> =
                quads.iter().map(|(sheet, n)| format!("{sheet}={n}")).collect();
            println!(
                "{:<26} {:>5} {:>6} {:>6} {:>7} {:>6} {:>6}   {}",
                case.label,
                c.background,
                c.fill,
                c.line,
                c.symbols,
                c.with_text,
                c.with_icon,
                by_sheet.join("  "),
            );
        }

        report_misses(&wanted, &sheets);
        println!();
    }
    Ok(())
}

/// The icon names this style asked for that a sheet does not carry, worst
/// first. This is the failure mode the whole harness exists to surface:
/// `icon-image` is data-driven, so the names come out of the TILES, and no
/// amount of reading the style JSON reveals that a sheet is missing
/// `bus_stop` until a tile hands one over.
fn report_misses(wanted: &BTreeMap<String, usize>, sheets: &BTreeMap<String, BTreeSet<String>>) {
    let total: usize = wanted.values().sum();
    println!("  distinct icon names resolved: {} over {total} features", wanted.len());
    for (sheet, names) in sheets {
        let mut missing: Vec<(&String, &usize)> =
            wanted.iter().filter(|(n, _)| !names.contains(n.as_str())).collect();
        missing.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let lost: usize = missing.iter().map(|(_, n)| **n).sum();
        let shown: Vec<String> =
            missing.iter().take(8).map(|(n, c)| format!("{n}({c})")).collect();
        println!(
            "  {sheet}: {} of {} names missing, {lost} of {total} features lost{}{}",
            missing.len(),
            wanted.len(),
            if shown.is_empty() { "" } else { " -- " },
            shown.join(" "),
        );
    }
}

/// The sprite manifests to score against. Only the KEYS matter here: an entry
/// exists or it does not, and `pack_icon_atlas` skips a name it cannot find.
fn load_sheets() -> anyhow::Result<BTreeMap<String, BTreeSet<String>>> {
    let list = std::env::var("SPRITE_JSON").unwrap_or_else(|_| {
        // Relative to the ios crate root, which is where cargo runs an
        // example from.
        // OpenFreeMap's sheet is not in the tree; pass its cached
        // `sprite@2x.json` in SPRITE_JSON to score against it too.
        "assets/japan-sprite@2x.json".to_string()
    });
    let mut out = BTreeMap::new();
    for path in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        let parsed: HashMap<String, serde_json::Value> = serde_json::from_slice(&bytes)?;
        let stem = PathBuf::from(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        out.insert(stem, parsed.into_keys().collect());
    }
    Ok(out)
}

fn load_tiles(dir: &PathBuf) -> anyhow::Result<Vec<TileCase>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "pbf"))
        .collect();
    files.sort();

    let mut cases = Vec::new();
    for path in files {
        let stem = path.file_stem().expect("has an extension").to_string_lossy().into_owned();
        // `<place>_z<z>_<x>_<y>`
        let z: u8 = stem
            .split('_')
            .find_map(|part| part.strip_prefix('z').and_then(|n| n.parse().ok()))
            .ok_or_else(|| anyhow::anyhow!("{stem}: no _z<z>_ in the filename"))?;
        let bytes = std::fs::read(&path)?;
        let tile = mvt::decode(bytes).map_err(|e| anyhow::anyhow!("{stem}: {e:#}"))?;
        // The source stops at z14, so a z14 tile is also asked how it reads
        // four buckets up -- which is where a phone actually spends its time,
        // and where `poi` layers with a `minzoom` finally switch on.
        let display_zooms: &[u8] = if z == 14 { &[14, 17] } else { &[0] };
        for &dz in display_zooms {
            let display_z = if dz == 0 { z } else { dz };
            cases.push(TileCase {
                label: if display_z == z { stem.clone() } else { format!("{stem}@z{display_z}") },
                tile_z: z,
                display_z,
                tile: tile.clone(),
            });
        }
    }
    Ok(cases)
}
