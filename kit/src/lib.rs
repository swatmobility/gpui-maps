//! `swat-maps-kit`: what the library's examples and demos and Studio
//! share. The window shell and panel contract (`kit`, `shell`, `window`),
//! the map cards (`popup`, `linked_chart`, `time_filter`, `brush`), the
//! label and colouring helpers, the stress scenarios, the agentic HTTP
//! surface, and the demo panels under [`panels`].
//!
//! Every module sits at the crate root under the name it had as an
//! `examples/common/*.rs` include, so a panel's `crate::kit` and
//! `crate::shell` resolve here, and a binary that writes
//! `use swat_maps_kit::{kit, shell};` at its own root keeps its
//! `crate::kit::...` paths unchanged.

pub mod agentic;
pub mod agentic_gate;
pub mod brush;
pub mod data_labels;
pub mod datapack;
pub mod kit;
pub mod linked_chart;
pub mod map_colouring;
pub mod popup;
pub mod range_slider;
pub mod shell;
pub mod speed_menu;
pub mod time_bar;
pub mod story_agentic;
pub mod stress;
pub mod time_filter;
pub mod window;

/// The demo panels the standalone examples and Studio both show.
pub mod panels {
    pub mod basic_map;
    pub mod big_points;
    pub mod census;
    pub mod cog;
    pub mod csv_points;
    pub mod fgb;
    #[cfg(feature = "geoparquet")]
    pub mod geoparquet;
    pub mod graphite;
    #[cfg(feature = "mbtiles")]
    pub mod mbtiles;
    pub mod offline;
    pub mod open_basemaps;
    pub mod overlay_features;
    pub mod pmtiles_remote;
    pub mod race_dots;
    #[cfg(feature = "orbits")]
    pub mod satellites;
    pub mod standard_dark;
    pub mod streets;
    pub mod swat_routes_dark;
    pub mod tokyo;
    pub mod world_dots;
}
