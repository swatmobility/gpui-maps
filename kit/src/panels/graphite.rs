//! Graphite: SWAT's own dark basemap (`kit::KeylessStyle::OpenSwatRoutesDark`),
//! keyless, over OpenFreeMap's planet tiles (OSM, ODbL). The gallery's
//! Basemaps card (`graphite`); it replaced the private "SWAT Routes Dark"
//! card, whose Mapbox style needs a SWAT-account token (the user,
//! 2026-10-23: the card shows Graphite, not the private style). The old
//! ids (`swat-routes-dark`, `swat_routes_dark`, `swat-dark`) are this
//! page's registry aliases, so a saved session or `--open` still lands
//! here. Never reads a Mapbox token.
//!
//! `#![allow(dead_code)]`: the kit is included by more than one crate,
//! each using a different part of it.
#![allow(dead_code)]

use gpui::{App, AppContext as _, Window};
use gpui_maps::map_view::MapView;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "graphite";

/// The style this page draws, token or not.
pub const STYLE: kit::KeylessStyle = kit::KeylessStyle::OpenSwatRoutesDark;

/// Singapore, zoom 12: the same opening camera as the other basemap
/// cards, so they compare directly.
pub const CAMERA: (f64, f64, f64) = (103.8198, 1.3521, 12.0);

/// The open tiles and the style's design; no Mapbox credit.
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_OPENFREEMAP_KEYLESS];

pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network]
}

pub fn build(env: &kit::PanelEnv, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let kit::KeylessBasemap { style, source, sprite, document, .. } = kit::keyless_basemap(STYLE)?;
    let (lon, lat, zoom) = kit::camera_from_env(CAMERA);
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(lon, lat, zoom, w, h, style, source, runtime);
        // The document too, so a story, `/capture/image` and the style
        // chooser can load this page's style.
        view.set_style_document(document);
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
        // The needle is built at runtime so this assertion does not match itself.
        let needle = [".", "bg", "("].concat();
        let source = include_str!("graphite.rs");
        assert!(!source.contains(&needle), "graphite.rs must not set an opaque background on the map chain");
    }

    #[test]
    fn graphite_is_the_keyless_swat_dark_style() {
        assert_eq!(super::STYLE.label(), "Graphite");
        assert!(super::CREDITS.iter().all(|c| c.licence != crate::kit::Licence::MapboxTos));
    }
}
