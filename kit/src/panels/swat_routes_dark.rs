//! SWAT Routes Dark: the dark basemap SWAT draws its routes over (the
//! vendored "SWATroutes dark map" style), drawn here on its own -- the page
//! shows no routes. The fixture is 50 basemap layers (land, water, roads,
//! boundaries, labels) on its own `sources.composite`
//! (mapbox-streets-v8); it has no route source or route layer. Unlike
//! `panels::standard_dark`, this fixture's own composite IS the one used --
//! both style compile and tileset derivation read the same file. Without a
//! token the page draws SWAT Routes Dark (open) instead ([`KEYLESS`]).
//! Used by `examples/swat_routes_dark.rs`; the gallery's Basemaps card is
//! Graphite (`panels::graphite`) instead, and `swat-routes-dark` is its alias. No navigation, no `T`: the standalone example never
//! wrapped this map in `DemoShell`, only in the plain focus-and-click-through
//! root (`FocusedMap`), which is what this panel keeps.
//!
//! The skip-cause compile diagnostics (`print_skip_census`) are the
//! standalone main's own, called ahead of `build` -- never from here, so the
//! gallery does not print them on every switch to this panel.
//!
//! `#![allow(dead_code)]`: this file is included by two crates (the
//! standalone example and the gallery) that each use a different half of
//! it, so some items are dead in one of them.
#![allow(dead_code)]

use gpui::{App, AppContext as _, Window};
use gpui_maps::map_view::MapView;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "swat-routes-dark";

/// The vendored fixture this panel compiles AND reads its composite
/// tileset list from -- see the module doc comment.
pub const STYLE: &str = "examples/swat-routes-dark-style.json";

/// The camera this example always opens at: Singapore, zoom 12 -- same as
/// `basic_map.rs` and `panels::standard_dark`, so screenshots compare
/// directly. No `GPUI_MAP_LON`/`LAT`/`ZOOM` override: the original
/// standalone example never read one either.
const CAMERA: (f64, f64, f64) = (103.8198, 1.3521, 12.0);

/// The private Mapbox style with a token, its open look-alike without.
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_MAPBOX, kit::CREDIT_OPENFREEMAP_KEYLESS];

/// Without a token (or with Mapbox off): SWAT Routes Dark (open), our look-alike over OpenFreeMap.
pub const KEYLESS: kit::KeylessStyle = kit::KeylessStyle::OpenSwatRoutesDark;

pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network, kit::Requirement::MapboxToken { fallback: Some(KEYLESS.label()) }]
}

/// [`kit::Availability::Probe`]'s probe: a GET for z0/x0/y0 of this
/// style's own composite tileset (the same `sources.composite` --
/// see the module doc comment -- that [`build`] actually draws), with the
/// configured token. [`kit::ProbeTarget::Keyless`] without one: the page
/// then draws [`KEYLESS`] and is always shown, with no request made. The
/// token never appears in anything this returns to a caller: it is
/// spliced into the URL handed straight to `reqwest`, and the probe's own
/// result is a bare `bool`.
pub fn availability_probe(env: &kit::PanelEnv) -> kit::ProbeTarget {
    let Some(token) = env.mapbox_token.as_deref() else { return kit::ProbeTarget::Keyless };
    let Ok(template) = kit::composite_tile_url_template(STYLE) else { return kit::ProbeTarget::Unavailable };
    let tile = template.replace("{z}", "0").replace("{x}", "0").replace("{y}", "0");
    kit::ProbeTarget::Get(format!("{tile}?access_token={token}"))
}

pub fn build(env: &kit::PanelEnv, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let kit::PageBasemap { style, source, sprite, sprite_token, .. } =
        kit::mapbox_or_keyless(STYLE, STYLE, env.mapbox_token.as_deref())?;
    let (lon, lat, zoom) = CAMERA;
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(lon, lat, zoom, w, h, style, source, runtime);
        if host.is_gallery() {
            view.set_chrome_above_map(true);
        }
        view
    });
    if let Some(sprite) = sprite {
        kit::spawn_sprites(map.downgrade(), sprite, sprite_token, env.runtime.clone(), cx);
    }
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let root = cx.new(|_| FocusedMap::new(map.clone(), focus));
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (lon, lat, zoom) })
}

/// The standalone main's own skip-cause compile diagnostics, ahead of
/// `build` -- never called from here, so the gallery does not print them
/// on every switch to this panel. The ~150-line implementation is shared
/// with `panels::standard_dark` in `kit::print_skip_census`.
pub fn print_skip_census(style_bytes: &[u8]) {
    kit::print_skip_census("swat_routes_dark", style_bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        // An opaque background style call anywhere on the window -> MapView chain hides the
        // map, which draws in a CAMetalLayer behind gpui -- the bug
        // `race_dots` shipped. `FocusedMap` (`kit/src/shell.rs`) is
        // shared and unchanged by this task; this pins this panel's own
        // code, which is what the task touches. The needle is built at
        // runtime so this assertion does not match itself.
        let needle = [".", "bg", "("].concat();
        let source = include_str!("swat_routes_dark.rs");
        assert!(!source.contains(&needle), "swat_routes_dark.rs must not set an opaque background on the map chain");
    }
}
