//! Where an overlay source sits in the frame relative to the basemap's
//! labels and buildings.
//!
//! A frame is drawn in passes, and the pass boundaries are the only places an
//! overlay can be inserted:
//!
//! 1. the **tile pass** -- space, imagery, EVERY basemap fill and line layer
//!    (one draw per tile, layers interleaved inside its mesh, wherever the
//!    style lists them relative to its symbol layers), then the sources a
//!    caller asked to keep UNDER THE BUILDINGS, the dots, the nav route, and
//!    last the style's `fill-extrusion` buildings;
//! 2. the **above-buildings pass** -- the overlay sources below the labels,
//!    by default, whenever the frame draws buildings ([`overlay_pass`]);
//!    on a fresh depth buffer, so neither a roof nor a wall covers them;
//! 3. the **symbol pass** -- every basemap symbol layer, text or icon-only
//!    (one-way arrows, shields), the overlay's labels, the attribution plate
//!    and the compass;
//! 4. the **above-labels overlay pass** -- the overlay sources that sit
//!    ABOVE LABELS, when there are any (no pass is opened otherwise);
//! 5. the nav chrome.
//!
//! So a data source draws over every non-symbol basemap layer and under
//! every symbol layer, by default. That is NOT MapLibre's "before the first
//! symbol layer": in OpenMapTiles-lineage styles (Liberty, OpenFreeMap
//! Streets) the first symbol layer is the icon-only `road_one_way_arrow`,
//! listed before the bridges, `building` and the translucent `building-3d`,
//! so a MapLibre layer inserted there sits under the station roofs and
//! reads washed out. Kepler.gl's split is the one followed here: the data
//! canvas over the whole basemap, then every symbol layer (its `label`
//! group matches `type: symbol`, icons included) moved over the data.
//!
//! MapLibre's `map.addLayer(layer, beforeId)` is the model for asking for a
//! slot by name ([`resolve_before_layer`], [`building_slot_before`]): a
//! `beforeId` naming a symbol layer, or any layer after the first symbol
//! layer, puts the source under the labels; one naming a fill-extrusion
//! layer (or a fill/line listed before the last of them) also puts it
//! under the buildings; no `beforeId` puts it on top of everything the
//! style draws, labels included, as `addLayer(layer)` does.
//!
//! The one thing MapLibre can do that this cannot: insert BETWEEN two basemap
//! fill/line layers (under the roads, say). A basemap tile is one mesh with
//! every fill and line layer in it, so there is no draw boundary there to
//! insert at; [`resolve_before_layer`] refuses such a layer rather than
//! silently drawing somewhere else.
//!
//! Free of `gpui` and `wgpu`: `MapView::paint`, the browser and phone shells
//! (`ShellOverlays::push_draws_split`) and `capture` route their overlay
//! draws through [`partition_overlay_order`] and [`overlay_pass`], so the
//! tests pin the order the shipped frame uses.

use crate::style::StyleLayer;

/// Which side of the symbol pass an overlay source is drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelSlot {
    /// In the tile pass, after every basemap fill and line: the basemap's
    /// labels and road shields draw over it. The default.
    #[default]
    BelowLabels,
    /// In its own pass after the symbol pass: it covers the labels too.
    AboveLabels,
}

/// Why [`resolve_before_layer`] could not place a source before a layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeLayerError {
    /// No layer in the style has this id. MapLibre throws for the same
    /// thing (`Cannot add layer "x" before non-existing layer "y"`).
    UnknownLayer(String),
    /// The layer is a basemap fill, line, raster or background below every
    /// symbol layer. Its geometry shares one mesh per tile with the layers
    /// around it, so the frame has no boundary to insert an overlay at.
    NotAtALabelBoundary(String),
}

impl std::fmt::Display for BeforeLayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownLayer(id) => write!(f, "no style layer has id {id:?}"),
            Self::NotAtALabelBoundary(id) => write!(
                f,
                "style layer {id:?} is a basemap fill/line layer below every label; an overlay can only be \
                 inserted below the labels (before a symbol layer) or above them (no layer)"
            ),
        }
    }
}

impl std::error::Error for BeforeLayerError {}

/// MapLibre's `beforeId`, resolved against a compiled style's layer list.
///
/// - `None`: on top of the whole style -> [`LabelSlot::AboveLabels`].
/// - a symbol layer, or any layer at or after the style's first symbol
///   layer -> [`LabelSlot::BelowLabels`]: after every basemap fill and line
///   and before every label. (A line layer listed after the first symbol
///   layer is still drawn in the tile pass, so "before it" can only mean
///   the label boundary.)
/// - a layer below every symbol layer -> [`BeforeLayerError::NotAtALabelBoundary`].
/// - an id no layer has -> [`BeforeLayerError::UnknownLayer`].
pub fn resolve_before_layer(layers: &[StyleLayer], before: Option<&str>) -> Result<LabelSlot, BeforeLayerError> {
    let Some(before) = before else {
        return Ok(LabelSlot::AboveLabels);
    };
    let at = layers
        .iter()
        .position(|layer| layer.id() == before)
        .ok_or_else(|| BeforeLayerError::UnknownLayer(before.to_string()))?;
    let first_symbol = layers.iter().position(|layer| matches!(layer, StyleLayer::Symbol { .. }));
    match first_symbol {
        Some(first) if at >= first => Ok(LabelSlot::BelowLabels),
        _ => Err(BeforeLayerError::NotAtALabelBoundary(before.to_string())),
    }
}

/// Which side of the basemap's 3D buildings a below-labels source is drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BuildingSlot {
    /// Over the buildings, in the above-buildings pass: a roof never covers
    /// the data. The default.
    #[default]
    OverBuildings,
    /// In the tile pass, before the extrusions: a wall or a roof covers what
    /// lies behind or under it (`set_source_above_buildings(name, false)`).
    UnderBuildings,
}

/// The building side of MapLibre's `beforeId`, resolved against a compiled
/// style's layer list (its label side is [`resolve_before_layer`]'s).
///
/// Under the buildings when `before` names a `fill-extrusion` layer, or a
/// fill/line/background listed before the style's last one -- MapLibre
/// would draw the source under that extrusion. Over them for no layer, a
/// symbol layer (a caller passing MapLibre's "first symbol layer" means
/// "under the labels", not "under the bridges and roofs listed after an
/// early one-way arrow"), any layer after the last extrusion, an unknown
/// id, and every style without buildings.
pub fn building_slot_before(layers: &[StyleLayer], before: Option<&str>) -> BuildingSlot {
    let Some(before) = before else {
        return BuildingSlot::OverBuildings;
    };
    let last_extrusion = layers.iter().rposition(|layer| matches!(layer, StyleLayer::Extrusion { .. }));
    let at = layers.iter().position(|layer| layer.id() == before);
    match (at, last_extrusion) {
        (Some(at), Some(last)) if at <= last && !matches!(layers[at], StyleLayer::Symbol { .. }) => {
            BuildingSlot::UnderBuildings
        }
        _ => BuildingSlot::OverBuildings,
    }
}

/// The pass a source draws in, from its two slots and whether this frame
/// draws any basemap building (`frame_plan::extrusion_plan`'s `steps`) and
/// none of the caller's own 3D (overlay extrusions, models).
///
/// With no building drawn, a source over the buildings stays in the tile
/// pass: there is nothing to cover it, and no extra pass is opened. With
/// the caller's own columns drawn it stays there too: they share the tile
/// pass's depth with the buildings, and a lifted fill would paint over a
/// data column standing in it.
pub(crate) fn overlay_pass(
    label: LabelSlot,
    building: BuildingSlot,
    buildings_drawn: bool,
) -> crate::overlay::host::OverlayPass {
    use crate::overlay::host::OverlayPass;
    match (label, building) {
        (LabelSlot::AboveLabels, _) => OverlayPass::AboveLabels,
        (LabelSlot::BelowLabels, BuildingSlot::OverBuildings) if buildings_drawn => OverlayPass::AboveBuildings,
        (LabelSlot::BelowLabels, _) => OverlayPass::Tiles,
    }
}

/// Splits the overlay's draw order (the store's source order) into the
/// sources drawn in the tile pass and those drawn after the symbol pass,
/// each keeping its relative order -- a source pushed later still draws over
/// one pushed earlier within its slot.
pub fn partition_overlay_order<T: Clone>(order: &[T], slot_of: impl Fn(&T) -> LabelSlot) -> (Vec<T>, Vec<T>) {
    let mut below = Vec::new();
    let mut above = Vec::new();
    for item in order {
        match slot_of(item) {
            LabelSlot::BelowLabels => below.push(item.clone()),
            LabelSlot::AboveLabels => above.push(item.clone()),
        }
    }
    (below, above)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(spec: &str) -> Vec<StyleLayer> {
        crate::style_spec::parse_and_compile(spec.as_bytes()).expect("the test style compiles").layers
    }

    const STYLE: &str = r##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},"layers":[
        {"id":"bg","type":"background","paint":{"background-color":"#fff"}},
        {"id":"water","type":"fill","source":"s","source-layer":"water","paint":{"fill-color":"#00f"}},
        {"id":"road","type":"line","source":"s","source-layer":"road","paint":{"line-color":"#888"}},
        {"id":"road-label","type":"symbol","source":"s","source-layer":"road","layout":{"text-field":"{name}"}},
        {"id":"bridge","type":"line","source":"s","source-layer":"road","paint":{"line-color":"#888"}},
        {"id":"place-label","type":"symbol","source":"s","source-layer":"place","layout":{"text-field":"{name}"}}
    ]}"##;

    #[test]
    fn before_a_symbol_layer_is_below_the_labels_and_no_layer_is_above_them() {
        let layers = style(STYLE);
        assert_eq!(resolve_before_layer(&layers, Some("road-label")), Ok(LabelSlot::BelowLabels));
        assert_eq!(resolve_before_layer(&layers, Some("place-label")), Ok(LabelSlot::BelowLabels));
        // A line after the first label still draws in the tile pass: the
        // only boundary "before it" can mean is the label one.
        assert_eq!(resolve_before_layer(&layers, Some("bridge")), Ok(LabelSlot::BelowLabels));
        assert_eq!(resolve_before_layer(&layers, None), Ok(LabelSlot::AboveLabels));
    }

    #[test]
    fn before_a_basemap_fill_or_an_unknown_layer_is_refused() {
        let layers = style(STYLE);
        assert_eq!(
            resolve_before_layer(&layers, Some("water")),
            Err(BeforeLayerError::NotAtALabelBoundary("water".into()))
        );
        assert_eq!(resolve_before_layer(&layers, Some("nope")), Err(BeforeLayerError::UnknownLayer("nope".into())));
        let no_labels = style(
            r##"{"version":8,"sources":{},"layers":[{"id":"bg","type":"background","paint":{"background-color":"#fff"}}]}"##,
        );
        assert_eq!(
            resolve_before_layer(&no_labels, Some("bg")),
            Err(BeforeLayerError::NotAtALabelBoundary("bg".into()))
        );
    }

    #[test]
    fn the_default_slot_is_below_the_labels() {
        assert_eq!(LabelSlot::default(), LabelSlot::BelowLabels);
    }

    #[test]
    fn partitioning_keeps_the_source_order_inside_each_slot() {
        let order = ["a", "b", "c", "d"];
        let (below, above) = partition_overlay_order(&order, |name| {
            if *name == "b" || *name == "d" {
                LabelSlot::AboveLabels
            } else {
                LabelSlot::BelowLabels
            }
        });
        assert_eq!(below, ["a", "c"]);
        assert_eq!(above, ["b", "d"]);
        let (below, above) = partition_overlay_order(&order, |_| LabelSlot::default());
        assert_eq!(below, order);
        assert!(above.is_empty(), "nothing moves above the labels unless asked");
    }

    /// Whether `layers` draws a building at z17 over Shinagawa: the frame
    /// plan every shell's paint asks.
    fn buildings_drawn(layers: &[StyleLayer]) -> bool {
        !crate::render::frame_plan::extrusion_plan(layers, 17.0, 35.6285, 0.0, Default::default()).steps.is_empty()
    }

    /// The pass a source left at its defaults draws in, and the pass a
    /// source placed with MapLibre's `beforeId` draws in.
    fn default_pass(layers: &[StyleLayer]) -> crate::overlay::host::OverlayPass {
        overlay_pass(LabelSlot::default(), BuildingSlot::default(), buildings_drawn(layers))
    }
    fn pass_before(layers: &[StyleLayer], before: &str) -> crate::overlay::host::OverlayPass {
        let label = resolve_before_layer(layers, Some(before)).expect("a label boundary");
        overlay_pass(label, building_slot_before(layers, Some(before)), buildings_drawn(layers))
    }

    /// Liberty's and OpenFreeMap Streets' order: an icon-only one-way arrow
    /// is the first symbol layer, and the bridges, `building` and the
    /// translucent `building-3d` follow it, before the first text label.
    const EARLY_ARROW: &str = r##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},"layers":[
        {"id":"bg","type":"background","paint":{"background-color":"#fff"}},
        {"id":"landuse","type":"fill","source":"s","source-layer":"landuse","paint":{"fill-color":"#eee"}},
        {"id":"road","type":"line","source":"s","source-layer":"transportation","paint":{"line-color":"#888"}},
        {"id":"road_one_way_arrow","type":"symbol","source":"s","source-layer":"transportation","layout":{"icon-image":"arrow"}},
        {"id":"bridge","type":"line","source":"s","source-layer":"transportation","paint":{"line-color":"#888"}},
        {"id":"building","type":"fill","source":"s","source-layer":"building","paint":{"fill-color":"#ddd"}},
        {"id":"building-3d","type":"fill-extrusion","source":"s","source-layer":"building","minzoom":14,
         "paint":{"fill-extrusion-color":"#e6ded4","fill-extrusion-height":20,"fill-extrusion-opacity":0.8}},
        {"id":"boundary","type":"line","source":"s","source-layer":"boundary","paint":{"line-color":"#888"}},
        {"id":"station_label","type":"symbol","source":"s","source-layer":"poi","layout":{"text-field":"{name}"}}
    ]}"##;

    #[test]
    fn an_early_arrow_does_not_put_the_data_under_the_buildings_listed_after_it() {
        use crate::overlay::host::OverlayPass;
        let layers = style(EARLY_ARROW);
        assert!(buildings_drawn(&layers), "the fixture's building-3d draws at z17");
        // The default: over the bridge, the building fill AND the
        // translucent extrusion roof -- the user's washed-out station dot
        // was this source drawn in the tile pass, under `building-3d`.
        assert_eq!(default_pass(&layers), OverlayPass::AboveBuildings);
        // MapLibre's "before the first symbol layer" names the arrow; it
        // means "under the labels", and gets the same slot. Both arrow and
        // label stay over the data: every symbol is in the symbol pass.
        assert_eq!(pass_before(&layers, "road_one_way_arrow"), OverlayPass::AboveBuildings);
        assert_eq!(pass_before(&layers, "station_label"), OverlayPass::AboveBuildings);
        // A fill/line listed after the arrow and before the extrusion is
        // MapLibre's "under the buildings"; one after it is not.
        assert_eq!(pass_before(&layers, "building"), OverlayPass::Tiles);
        assert_eq!(pass_before(&layers, "bridge"), OverlayPass::Tiles);
        assert_eq!(pass_before(&layers, "boundary"), OverlayPass::AboveBuildings);
        assert_eq!(building_slot_before(&layers, Some("road")), BuildingSlot::UnderBuildings);
        assert_eq!(building_slot_before(&layers, None), BuildingSlot::OverBuildings);
        // The opt-outs still hold.
        assert_eq!(overlay_pass(LabelSlot::BelowLabels, BuildingSlot::UnderBuildings, true), OverlayPass::Tiles);
        assert_eq!(overlay_pass(LabelSlot::AboveLabels, BuildingSlot::UnderBuildings, true), OverlayPass::AboveLabels);
        assert_eq!(overlay_pass(LabelSlot::AboveLabels, BuildingSlot::OverBuildings, false), OverlayPass::AboveLabels);
    }

    #[test]
    fn an_extrusion_listed_after_the_labels_is_still_under_the_data() {
        use crate::overlay::host::OverlayPass;
        let layers = style(
            r##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},"layers":[
            {"id":"water","type":"fill","source":"s","source-layer":"water","paint":{"fill-color":"#00f"}},
            {"id":"place-label","type":"symbol","source":"s","source-layer":"place","layout":{"text-field":"{name}"}},
            {"id":"building-3d","type":"fill-extrusion","source":"s","source-layer":"building",
             "paint":{"fill-extrusion-color":"#ccc","fill-extrusion-height":30}}
        ]}"##,
        );
        assert_eq!(default_pass(&layers), OverlayPass::AboveBuildings);
        assert_eq!(pass_before(&layers, "place-label"), OverlayPass::AboveBuildings);
        // Before the extrusion itself: under it, still under the labels.
        assert_eq!(resolve_before_layer(&layers, Some("building-3d")), Ok(LabelSlot::BelowLabels));
        assert_eq!(pass_before(&layers, "building-3d"), OverlayPass::Tiles);
        // Below the buildings' zoom nothing is lifted: no extra pass opens.
        let low = crate::render::frame_plan::extrusion_plan(&layers, 11.0, 35.6, 0.0, Default::default());
        assert!(low.steps.is_empty());
        assert_eq!(overlay_pass(LabelSlot::BelowLabels, BuildingSlot::OverBuildings, false), OverlayPass::Tiles);
    }

    #[test]
    fn a_style_with_no_symbols_draws_the_data_over_all_of_it() {
        use crate::overlay::host::OverlayPass;
        let layers = style(
            r##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},"layers":[
            {"id":"bg","type":"background","paint":{"background-color":"#fff"}},
            {"id":"water","type":"fill","source":"s","source-layer":"water","paint":{"fill-color":"#00f"}},
            {"id":"road","type":"line","source":"s","source-layer":"road","paint":{"line-color":"#888"}}
        ]}"##,
        );
        assert!(!buildings_drawn(&layers));
        // Over every fill and line, in the tile pass: no pass to open.
        assert_eq!(default_pass(&layers), OverlayPass::Tiles);
        assert_eq!(building_slot_before(&layers, Some("water")), BuildingSlot::OverBuildings);
        assert_eq!(resolve_before_layer(&layers, None), Ok(LabelSlot::AboveLabels));
    }

    #[test]
    fn a_label_listed_before_the_roads_leaves_the_roads_under_the_data() {
        use crate::overlay::host::OverlayPass;
        let layers = style(
            r##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},"layers":[
            {"id":"water","type":"fill","source":"s","source-layer":"water","paint":{"fill-color":"#00f"}},
            {"id":"water-label","type":"symbol","source":"s","source-layer":"water","layout":{"text-field":"{name}"}},
            {"id":"road","type":"line","source":"s","source-layer":"road","paint":{"line-color":"#888"}},
            {"id":"building","type":"fill","source":"s","source-layer":"building","paint":{"fill-color":"#ddd"}},
            {"id":"road-label","type":"symbol","source":"s","source-layer":"road","layout":{"text-field":"{name}"}}
        ]}"##,
        );
        // The roads and the building fill draw in the tile pass mesh, before
        // every overlay; the labels on both sides of them draw over it.
        assert_eq!(default_pass(&layers), OverlayPass::Tiles);
        assert_eq!(pass_before(&layers, "road"), OverlayPass::Tiles);
        assert_eq!(pass_before(&layers, "water-label"), OverlayPass::Tiles);
        assert_eq!(resolve_before_layer(&layers, Some("road")), Ok(LabelSlot::BelowLabels));
        assert_eq!(
            resolve_before_layer(&layers, Some("water")),
            Err(BeforeLayerError::NotAtALabelBoundary("water".into()))
        );
    }
}
