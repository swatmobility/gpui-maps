//! Mapbox Standard Dark: a root style document whose real basemap lives in
//! `imports[0].data` (Mapbox's own Standard style, resolved against this
//! document's `config` -- theme monochrome, lightPreset night), with a thin
//! overlay stacked on top in the root's own `layers`. Shared by
//! `examples/mapbox_standard_dark.rs` and the gallery (`standard-dark`).
//!
//! The composite vector source is deliberately NOT the dark fixture's own
//! top-level `sources.composite` (which names only 2 of the 3 tilesets the
//! Styles API actually resolves this style to, and was never independently
//! confirmed against real tile data): this panel reuses the same,
//! already-verified `examples/mapbox-streets-style.json` composite
//! `kit/src/panels/streets.rs` uses. The imported basemap's layers
//! reference the same `source-layer` names ("water", "landuse", "road", ...)
//! either way, so this is no new source machinery.
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
use crate::shell::DemoShell;

pub const ID: &str = "standard-dark";

/// The vendored fixture this panel compiles.
pub const STYLE: &str = "examples/mapbox-standard-dark-style.json";

/// Where the verified composite source's tileset list comes from -- see the
/// module doc comment on why this is not `STYLE`'s own `sources.composite`.
pub const COMPOSITE_STYLE: &str = "examples/mapbox-streets-style.json";

/// Mapbox Standard Dark with a token, Dark (open) without.
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_MAPBOX, kit::CREDIT_OPENFREEMAP_KEYLESS];

/// There is no keyless Mapbox Standard: without a token the page draws
/// Dark (open), our night-monochrome look-alike over OpenFreeMap, with a
/// fallback note -- never the missing-data card.
pub const KEYLESS: kit::KeylessStyle = kit::KeylessStyle::OpenDark;

pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network, kit::Requirement::MapboxToken { fallback: Some(KEYLESS.label()) }]
}

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with_style(env, STYLE, window, cx)
}

/// [`build`], parameterized on which vendored style document to compile.
/// The gallery only ever calls [`build`] (always `STYLE`, Standard Dark);
/// this exists so the standalone main can honour `GPUI_MAP_STYLE`
/// (`streets`/`dark-reference`/`liberty`/unset) without the gallery
/// panel needing to know that override exists.
pub fn build_with_style(
    env: &kit::PanelEnv,
    style_file: &str,
    _window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<kit::Built> {
    // Without a token, `style_file` (a Mapbox document) cannot be drawn
    // at all; its keyless stand-in is ([`kit::KeylessStyle::for_mapbox`]).
    let kit::PageBasemap { style, source, sprite, sprite_token, .. } =
        kit::mapbox_or_keyless(style_file, COMPOSITE_STYLE, env.mapbox_token.as_deref())?;
    let (lon, lat, zoom) = camera_from_env(env);
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
    // This is the one panel that keeps `DemoShell` rather than the plain
    // `FocusedMap`: it has `T`, the day/night terminator toggle, which only
    // `DemoShell`/`DemoKeys` handle (see `kit/src/shell.rs`).
    let root = match &env.host {
        kit::Host::Standalone { title } => {
            let title = title.clone();
            cx.new(|cx| DemoShell::new(map.clone(), title, cx))
        }
        kit::Host::Gallery => cx.new(|cx| DemoShell::embedded(map.clone(), None, cx)),
    };
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (lon, lat, zoom) })
}

/// The camera this example opens at: Singapore, zoom 12 -- same as
/// `basic_map.rs` -- unless `GPUI_MAP_LON`/`LAT`/`ZOOM` override it, in the
/// gallery as standalone ([`kit::camera_from_env`], the one reading).
fn camera_from_env(_env: &kit::PanelEnv) -> (f64, f64, f64) {
    kit::camera_from_env((103.8198, 1.3521, 12.0))
}

/// The tile URL template the composite source resolves to, WITHOUT a token
/// (`Provenance::tile_url` must never carry one) -- for the standalone
/// main's own debug-port registration, ahead of opening a window.
pub fn composite_tile_url_template() -> anyhow::Result<String> {
    kit::composite_tile_url_template(COMPOSITE_STYLE)
}

/// The standalone main's own skip-cause compile diagnostics, ahead of
/// `build` -- never called from here, so the gallery does not print them
/// on every switch to this panel. The ~150-line implementation is shared
/// with `panels::swat_routes_dark` in `kit::print_skip_census`.
pub fn print_skip_census(style_bytes: &[u8]) {
    kit::print_skip_census("mapbox_standard_dark", style_bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        // An opaque background style call anywhere on the window -> MapView chain hides the
        // map, which draws in a CAMetalLayer behind gpui -- the bug
        // `race_dots` shipped. This panel's own code is what this task
        // touches, so it is what this task pins; `DemoShell`
        // (`kit/src/shell.rs`) is shared and unchanged by it.
        //
        // The needle is built at runtime (never written as one contiguous
        // literal) so this very assertion does not match itself via
        // `include_str!`.
        let needle = [".", "bg", "("].concat();
        let source = include_str!("standard_dark.rs");
        assert!(!source.contains(&needle), "standard_dark.rs must not set an opaque background on the map chain");
    }
}
