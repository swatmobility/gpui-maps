//! The gallery's other keyless basemap cards, each one of our own style
//! documents over open tiles, compiled natively:
//!
//! - **Light** (`light`): Graphite's light twin, a calm low-saturation
//!   base for data (`examples/styles-open/open-light-style.json`).
//! - **Terrain** (`terrain`): the same light palette over Natural Earth II
//!   shaded relief (public domain, served by OpenFreeMap, z0-6), over the
//!   Alps (`examples/styles-open/open-terrain-style.json`).
//! - **Satellite** (`satellite`): Sentinel-2 cloudless 2016 imagery by EOX
//!   (CC BY 4.0; every later year is non-commercial) with OpenFreeMap place
//!   labels on top (`examples/satellite-style.json`).
//!
//! None reads a Mapbox token. Each sets its style document on the view so
//! a story and `/capture/image` can load it, and names its raster so the
//! offscreen capture draws (and credits) the same imagery.
//!
//! `#![allow(dead_code)]`: the kit is included by more than one crate,
//! each using a different part of it.
#![allow(dead_code)]

use std::sync::Arc;

use gpui::{App, AppContext as _, Window};
use gpui_maps::map_view::MapView;
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const LIGHT_ID: &str = "light";
pub const TERRAIN_ID: &str = "terrain";
pub const SATELLITE_ID: &str = "satellite";

pub const LIGHT_STYLE: &str = "examples/styles-open/open-light-style.json";
pub const TERRAIN_STYLE: &str = "examples/styles-open/open-terrain-style.json";
pub const SATELLITE_STYLE: &str = "examples/satellite-style.json";

/// Singapore, zoom 12: the other basemap cards' camera.
pub const SINGAPORE: (f64, f64, f64) = (103.8198, 1.3521, 12.0);
/// The Alps, where the relief reads best at the source's z6 ceiling.
pub const ALPS: (f64, f64, f64) = (9.9, 46.4, 5.9);

pub const LIGHT_CREDITS: &[kit::Credit] = &[kit::CREDIT_OPENFREEMAP_KEYLESS];
pub const TERRAIN_CREDITS: &[kit::Credit] = &[kit::CREDIT_NATURAL_EARTH, kit::CREDIT_OPENFREEMAP_KEYLESS];
pub const SATELLITE_CREDITS: &[kit::Credit] = &[kit::CREDIT_S2CLOUDLESS, kit::CREDIT_OSM_OPENFREEMAP];

pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network]
}

fn natural_earth() -> (Arc<dyn TileSource>, u32) {
    let source = HttpTileSource::new(super::basic_map::NATURAL_EARTH, 6, super::basic_map::NATURAL_EARTH_ATTRIBUTION);
    (Arc::new(kit::with_tile_cache(source, kit::Licence::PublicDomain)), 512)
}

fn s2cloudless() -> (Arc<dyn TileSource>, u32) {
    let source = HttpTileSource::new(kit::S2CLOUDLESS_2016_URL, 14, kit::S2CLOUDLESS_2016_ATTRIBUTION);
    (Arc::new(kit::with_tile_cache(source, kit::Licence::CcBy4)), 256)
}

pub fn build_light(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build(env, LIGHT_STYLE, SINGAPORE, None, window, cx)
}

pub fn build_terrain(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build(env, TERRAIN_STYLE, ALPS, Some(natural_earth()), window, cx)
}

pub fn build_satellite(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build(env, SATELLITE_STYLE, SINGAPORE, Some(s2cloudless()), window, cx)
}

/// `style_file` over OpenFreeMap's planet, with `raster` (if any) drawn at
/// the style's own raster layer.
fn build(
    env: &kit::PanelEnv,
    style_file: &str,
    camera: (f64, f64, f64),
    raster: Option<(Arc<dyn TileSource>, u32)>,
    _window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<kit::Built> {
    let style = kit::compile_style(style_file)?;
    let document: serde_json::Value = serde_json::from_slice(&kit::asset_bytes(style_file)?)?;
    let sprite = style.sprite.clone();
    let (lon, lat, zoom) = kit::camera_from_env(camera);
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let source = kit::openfreemap_planet();
    let map = cx.new(|_| {
        let mut view = MapView::new(lon, lat, zoom, w, h, style, source, runtime);
        view.set_style_document(document);
        view.set_basemap_name(Some("openfreemap"));
        if let Some((raster, tile_size)) = raster {
            view.set_raster_source(raster, tile_size);
        }
        if host.is_gallery() {
            view.set_chrome_above_map(true);
        }
        view
    });
    if let Some(sprite) = sprite {
        kit::spawn_sprites(map.downgrade(), sprite, None, env.runtime.clone(), cx);
    }
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let root = cx.new(|_| FocusedMap::new(map.clone(), focus));
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (lon, lat, zoom) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        let needle = [".", "bg", "("].concat();
        let source = include_str!("open_basemaps.rs");
        assert!(!source.contains(&needle), "open_basemaps.rs must not set an opaque background on the map chain");
    }

    /// Each style is bundled, compiles, and names no Mapbox host.
    #[test]
    fn each_style_compiles_keyless() {
        for file in [super::LIGHT_STYLE, super::TERRAIN_STYLE, super::SATELLITE_STYLE] {
            let text = crate::kit::asset_string(file).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert!(!text.contains("mapbox://") && !text.contains("api.mapbox.com"), "{file}");
            crate::kit::compile_style(file).unwrap_or_else(|e| panic!("{file}: {e:#}"));
        }
        for credits in [super::LIGHT_CREDITS, super::TERRAIN_CREDITS, super::SATELLITE_CREDITS] {
            assert!(credits.iter().all(|c| c.licence != crate::kit::Licence::MapboxTos));
        }
    }
}
