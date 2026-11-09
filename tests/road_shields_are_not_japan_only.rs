//! A road shield draws outside Japan, or these fail.
//!
//! # The bug this pins
//!
//! Both shield layers of `examples/mapbox-style.json` used to carry
//!
//! ```text
//! ["match", ["get", "network"], ["jp-expressway", "jp-national", "jp-prefecture"], true, false]
//! ```
//!
//! and none of those three literals exists anywhere in OpenFreeMap's
//! OpenMapTiles planet. Read across 34 tiles in 24 countries, the whole
//! `network` value set the tiles carry is:
//!
//! | value | where |
//! |---|---|
//! | `road` | everywhere -- 239 of 252 tagged features |
//! | `us-interstate`, `us-highway`, `us-state` | United States |
//! | `gb-primary`, `gb-trunk` | United Kingdom |
//! | `ca-provincial`, `ca-transcanada` | Canada |
//! | *absent* | a ref off the way itself, no route relation |
//!
//! Japan's own roads are `network = "road"` like everyone else's, so that
//! filter admitted NOTHING -- not in Singapore, not in China, and not in
//! Tokyo either. The `icon-image` `match` beneath it named `jp-expressway-N`
//! and `jp-national-route-N` on the same never-true labels, so even with the
//! filter opened every shield would still have resolved to `default-N`.
//!
//! That value set is also why this repository does not try to colour a
//! Chinese G-road green and an S-road yellow from `network`: a Chinese
//! `G98`, a German `A 44`, an Indian `NH46` and a Singaporean `AYE` are all
//! literally the same `network` string. Telling them apart needs the `ref`
//! text plus a country, and the tiles carry neither as an attribute.
//!
//! # The fixture
//!
//! `benches/fixtures/singapore_z14.pbf` is a real OpenFreeMap tile. Its
//! `transportation_name` layer holds two line features with a ref --
//! `AYE` and `MCE`, both `ref_length = 3`, both `network = "road"` -- and
//! three POINT features (`1A`, `1`, `2`, motorway exit numbers) which the
//! layers' `geometry-type` clause is supposed to keep out.

use gpui_maps::style::{StyleLayer, SymbolPlan};

const FIXTURE: &str = "benches/fixtures/singapore_z14.pbf";
const TILE_Z: u8 = 14;

/// The bytes of a Mapbox-authored style: from the checkout when it has
/// one, else from `$MAPBOX_FIXTURES` (by file name, as
/// `src/mapbox_fixtures.rs` reads them), else `None` with the reason
/// printed. The open library tree carries no Mapbox style
/// (`scripts/fetch-mapbox-styles.sh` fetches them).
fn mapbox_style(style_path: &str) -> Option<Vec<u8>> {
    if let Ok(bytes) = std::fs::read(style_path) {
        return Some(bytes);
    }
    let name = std::path::Path::new(style_path).file_name()?;
    match std::env::var_os("MAPBOX_FIXTURES").filter(|d| !d.is_empty()) {
        Some(dir) => match std::fs::read(std::path::Path::new(&dir).join(name)) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                eprintln!("skipped: {style_path} is a Mapbox fixture and MAPBOX_FIXTURES lacks it");
                None
            }
        },
        None => {
            eprintln!("skipped: {style_path} is a Mapbox fixture and MAPBOX_FIXTURES is unset (see scripts/fetch-mapbox-styles.sh)");
            None
        }
    }
}

/// The two shield layers, by id, and the plan the Singapore fixture makes,
/// or `None` when the style is absent (see [`mapbox_style`]).
fn singapore_shields(style_path: &str) -> Option<(Vec<usize>, SymbolPlan)> {
    let style_bytes = mapbox_style(style_path)?;
    let style = gpui_maps::style_spec::parse_and_compile(&style_bytes).expect("style compiles");
    let shield_layers: Vec<usize> = style
        .layers
        .iter()
        .enumerate()
        .filter(|(_, layer)| match layer {
            StyleLayer::Symbol { id, .. } => id.contains("road-number-shield"),
            _ => false,
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        shield_layers.len(),
        2,
        "{style_path}: expected road-number-shield and road-number-shield-wide"
    );

    let bytes = std::fs::read(FIXTURE).expect("vendored Singapore tile");
    let decoded = gpui_maps::mvt::decode(bytes).expect("fixture decodes");
    let plan = gpui_maps::style::match_tile_symbols(
        &decoded,
        &style,
        TILE_Z,
        gpui_maps::symbol_line::tile_units_per_px(TILE_Z, TILE_Z),
    );
    Some((shield_layers, plan))
}

#[test]
fn singapores_expressways_get_a_shield() {
    let Some((shield_layers, plan)) = singapore_shields("examples/mapbox-style.json") else { return };
    let shields: Vec<(&str, &str)> = plan
        .features
        .iter()
        .filter(|f| shield_layers.contains(&f.layer_index))
        .filter_map(|f| Some((f.text.as_deref()?, f.icon.as_deref()?)))
        .collect();

    assert!(
        !shields.is_empty(),
        "no shield on any Singapore expressway. The `network` filter is \
         Japan-only again -- see this file's module comment for why no tile \
         anywhere carries a `jp-*` network."
    );
    // Both refs in this tile are three characters, so both want the same
    // plate. `default-3` rather than `default-N` for a literal N is the
    // whole point of the `concat`: one expression serves every ref length.
    for (ref_text, icon) in &shields {
        assert_eq!(
            *icon, "default-3",
            "{ref_text}: ref_length 3 must pick the 3-wide plate, not {icon}"
        );
    }
    let refs: std::collections::BTreeSet<&str> = shields.iter().map(|(t, _)| *t).collect();
    assert!(
        refs.contains("AYE") && refs.contains("MCE"),
        "expected AYE and MCE among the shielded refs, got {refs:?}"
    );
}

#[test]
fn the_exit_numbers_do_not_get_a_shield() {
    // The same tile's `1A`/`1`/`2` are POINT features -- motorway exit
    // numbers, not routes. The layers' `geometry-type` clause is what keeps
    // them out, and it survived the filter rewrite.
    let Some((shield_layers, plan)) = singapore_shields("examples/mapbox-style.json") else { return };
    let point_refs = ["1A", "1", "2"];
    for feature in plan.features.iter().filter(|f| shield_layers.contains(&f.layer_index)) {
        let text = feature.text.as_deref().unwrap_or_default();
        assert!(
            !point_refs.contains(&text),
            "{text:?} is a motorway exit number on a Point, not a route -- \
             the geometry-type clause should have excluded it"
        );
    }
}

#[test]
fn every_shield_plate_the_styles_ask_for_exists_in_the_sheet() {
    // A sprite name that resolves to no rect draws NOTHING -- silently, with
    // no fallback (`symbol_pass.rs`'s `if let Some(rect)`). So the widest
    // ref the wide layer now admits, `ref_length = 6`, has to have a plate,
    // and so does the narrowest.
    let json = std::fs::read("web/sprites/japan-sprite@2x.json").expect("vendored sheet manifest");
    let manifest = gpui_maps::sprite::parse_sprite_manifest(&json).expect("manifest parses");
    for length in 1..=6 {
        for name in [format!("default-{length}"), format!("road_{length}")] {
            assert!(
                manifest.contains_key(&name),
                "{name}: the shield layers can ask for this plate and the sheet has no rect for it"
            );
        }
    }
}
