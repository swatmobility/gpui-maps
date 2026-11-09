//! The mask classifier: which basemap layers/classes rank as building /
//! habitable / avoid, for [`crate::overlay::dotgrid::hash::choose`]'s
//! candidate search (Task 3 of
//! `docs/superpowers/plans/2026-09-27-dasymetric-dot-placement.md`).
//!
//! Ranked per matched fill feature, once when a tile's `PaintPlan` is built
//! (`style.rs::resolve_plan`, Task 4) -- not per dot, per frame. Anything
//! this table doesn't name is [`MaskRank::Land`]: farmland, national parks
//! and every street class are deliberately not avoided (controller ruling,
//! `progress.md`).

use crate::mvt::{Tags, TagValue};

/// A building fill ranks [`MaskRank::Building`] only when the style draws
/// it at least this opaque at the plan's (bake) zoom (`style::resolve_plan_with_mask`);
/// fainter, it is land.
pub const BUILDING_MIN_OPACITY: f32 = 0.5;

/// A fill feature's rank for dot relocation, ascending "prefer this more":
/// `Water`/`Green` are avoided, `Land` is neutral, `Building` wins outright.
/// [`hash::RANK_BUILDING`](super::hash::RANK_BUILDING) must equal
/// `Building as u8` -- checked by `the_rank_order_is_building_land_green_water`
/// below, since nothing wires the two together at compile time (T2/T3
/// pre-flight note in `progress.md`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MaskRank {
    Water = 1,
    Green = 2,
    Land = 3,
    Building = 4,
}

/// Ranked fill ops of one `PaintPlan`: `(op index, rank)` for every Fill op
/// whose rank is not [`MaskRank::Land`], ascending by op index. `Land` ops
/// are left out -- they're the majority of any tile's fills and carry no
/// information the relocation search needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaskRanks {
    /// Whether the plan's fills were classified at all: `false` from a
    /// match built unmasked (`style::match_tile_static`, no dot layer
    /// relocating), whose `ops` are empty for want of asking, not because
    /// nothing ranks. A map that starts relocating re-ranks such a tile
    /// before it installs the tile's mask lists.
    pub classified: bool,
    pub ops: Vec<(u32, MaskRank)>,
}

/// One feature's rank, from its tile layer's name and its tags. Pure,
/// evaluated once per matched fill feature when the plan is built.
///
/// `class` and `underground` are read as plain strings; a `TagValue::Bool`
/// on `underground` also counts (Streets encodes some boolean-shaped tags
/// as MVT bools, not the string `"true"`).
pub fn classify(source_layer: &str, tags: &Tags) -> MaskRank {
    let class = match tags.get("class") {
        Some(TagValue::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let underground = match tags.get("underground") {
        Some(TagValue::String(s)) => Some(s.as_str()),
        Some(TagValue::Bool(true)) => Some("true"),
        _ => None,
    };
    classify_parts(source_layer, class, underground)
}

/// Whether any feature of `source_layer` can rank other than
/// [`MaskRank::Land`] -- the source layers [`classify_parts`]' table names.
/// The plan skips the per-feature tag lookups for every other fill layer.
#[must_use]
pub fn can_rank(source_layer: &str) -> bool {
    matches!(source_layer, "building" | "water" | "park" | "landuse" | "landcover")
}

/// The same decision from plain strings (the table tests use it).
///
/// | Schema | Source layer | Condition | Rank |
/// |---|---|---|---|
/// | Streets v8 | `building` | `underground` != `"true"` (any `type`) | Building |
/// | Streets v8 | `water` | always | Water |
/// | Streets v8 | `landuse` | `class` in {`park`, `grass`, `cemetery`} | Green |
/// | Streets v8 | `landcover` | `class` = `grass` | Green |
/// | OpenMapTiles | `building` | always | Building |
/// | OpenMapTiles | `water` | always | Water |
/// | OpenMapTiles | `park` | always | Green |
/// | OpenMapTiles | `landuse` | `class` = `cemetery` | Green |
/// | OpenMapTiles | `landcover` | `class` = `grass` | Green |
///
/// `landcover` class `grass` ranks Green in BOTH schemas, Streets v8 and
/// OpenMapTiles: grass is real ground cover, not noise, while the earlier
/// "not landcover" default meant broad land cover -- every other
/// `landcover` class (farmland, wood, crop, snow, ...) still ranks Land.
///
/// Everything else -- Streets `landuse_overlay` (national parks),
/// `aeroway`, `structure`, `road`, `hillshade`, `depth`, and `landuse`
/// classes `agriculture`/`wood`/`scrub`/`pitch`/`sand`/`glacier`/
/// `residential` -- is [`MaskRank::Land`]. No street class is ranked
/// (user-facing default: no street class).
pub fn classify_parts(source_layer: &str, class: Option<&str>, underground: Option<&str>) -> MaskRank {
    use MaskRank::*;
    match (source_layer, class) {
        ("building", _) if underground != Some("true") => Building,
        ("water", _) => Water,
        ("park", _) => Green, // OpenMapTiles
        ("landuse", Some("park" | "grass" | "cemetery")) => Green,
        ("landcover", Some("grass")) => Green, // both schemas -- see doc comment's ruling
        _ => Land,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streets_v8_table() {
        use MaskRank::*;
        let cases = [
            ("building", None, Some("false"), Building),
            ("building", Some("industrial"), Some("false"), Building),
            ("building", Some("commercial"), None, Building),
            ("building", None, Some("true"), Land),
            ("water", None, None, Water),
            ("landuse", Some("park"), None, Green),
            ("landuse", Some("grass"), None, Green),
            ("landuse", Some("cemetery"), None, Green),
            ("landuse", Some("agriculture"), None, Land),
            ("landuse", Some("wood"), None, Land),
            ("landuse", Some("pitch"), None, Land),
            ("landuse", Some("residential"), None, Land),
            // Controller ruling (progress.md): landcover class grass ranks
            // Green in BOTH Streets v8 and OpenMapTiles -- grass is grass
            // whatever the schema.
            ("landcover", Some("grass"), None, Green),
            ("landuse_overlay", Some("national_park"), None, Land),
            ("aeroway", Some("runway"), None, Land),
            ("road", Some("street"), None, Land), // ruling: no street rank in v1
        ];
        for (layer, class, ug, want) in cases {
            assert_eq!(classify_parts(layer, class, ug), want, "{layer} {class:?} {ug:?}");
        }
    }

    #[test]
    fn openmaptiles_schema_classifies_by_its_own_layer_names() {
        use MaskRank::*;
        for (layer, class, want) in [
            ("building", None, Building),
            ("water", Some("lake"), Water),
            ("park", Some("nature_reserve"), Green),
            ("landuse", Some("cemetery"), Green),
            ("landcover", Some("grass"), Green),
            ("landcover", Some("farmland"), Land),
            ("landcover", Some("wood"), Land),
            ("transportation", None, Land),
        ] {
            assert_eq!(classify_parts(layer, class, None), want);
        }
    }

    #[test]
    fn classify_reads_class_and_underground_from_tags() {
        let park_tags =
            Tags::from_iter([("class".to_string(), TagValue::String("park".to_string()))]);
        assert_eq!(classify("landuse", &park_tags), MaskRank::Green);

        let buried_tags =
            Tags::from_iter([("underground".to_string(), TagValue::Bool(true))]);
        assert_eq!(classify("building", &buried_tags), MaskRank::Land);
    }

    #[test]
    fn the_rank_order_is_building_land_green_water() {
        assert!(MaskRank::Building > MaskRank::Land && MaskRank::Land > MaskRank::Green && MaskRank::Green > MaskRank::Water);
        assert_eq!(MaskRank::Building as u8, crate::overlay::dotgrid::hash::RANK_BUILDING);
    }

    #[test]
    fn a_layer_can_rank_exactly_when_some_class_of_it_is_not_land() {
        let classes = [None, Some("park"), Some("grass"), Some("cemetery"), Some("wood")];
        for layer in ["building", "water", "park", "landuse", "landcover", "road", "landuse_overlay", "aeroway", "structure"] {
            let ranks = classes.iter().any(|&c| classify_parts(layer, c, None) != MaskRank::Land);
            assert_eq!(can_rank(layer), ranks, "{layer}");
        }
    }
}
