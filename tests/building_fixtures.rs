//! The building fixtures the extrusion tessellator and the `buildings` perf
//! gate section measure. Real OpenFreeMap z14 tiles (OpenStreetMap data,
//! ODbL; see `benches/fixtures/README.md`). The counts below are the tiles'
//! own and are pinned so a re-fetched fixture cannot silently change the
//! perf gate's exact bands.

const FIXTURES: &[(&str, usize)] = &[
    ("benches/fixtures/shinjuku_z14.pbf", SHINJUKU_BUILDINGS),
    ("benches/fixtures/manhattan_z14.pbf", MANHATTAN_BUILDINGS),
    ("benches/fixtures/singapore_z14.pbf", SINGAPORE_BUILDINGS),
];
/// Spec §5.1 measured 268 on build 20260913_164504_pt.
const SHINJUKU_BUILDINGS: usize = 268;
/// Spec §5.1 measured 1,488 on build 20260913_164504_pt.
const MANHATTAN_BUILDINGS: usize = 1488;
/// The existing fixture (x=12918 y=8134); spec §5.4 says 74.
const SINGAPORE_BUILDINGS: usize = 74;

fn building_layer(path: &str) -> gpui_maps::mvt::Layer {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let tile = gpui_maps::mvt::decode(bytes).expect("fixture decodes");
    tile.layers.into_iter().find(|l| l.name == "building").expect("fixture has a building layer")
}

#[test]
fn each_building_fixture_has_the_measured_feature_count() {
    for &(path, expected) in FIXTURES {
        assert_eq!(building_layer(path).features.len(), expected, "{path}");
    }
}

#[test]
fn every_fixture_building_carries_render_height() {
    for &(path, _) in FIXTURES {
        let layer = building_layer(path);
        let with_height = layer.features.iter().filter(|f| f.tags.get("render_height").is_some()).count();
        assert_eq!(with_height, layer.features.len(), "{path}: every OpenMapTiles z14 building has render_height");
    }
}
