//! Mapbox Streets v12 over Tokyo Station at z15: dense Japanese labels and
//! road shields, the same composite source and style
//! `kit/src/panels/streets.rs` uses, just a different starting camera and
//! no navigation. Shared by `examples/mapbox_tokyo.rs` and the gallery
//! (`tokyo`).
//!
//! `#![allow(dead_code)]`: this file is included by two crates (the
//! standalone example and the gallery) that each use a different half of
//! it, so some items are dead in one of them.
#![allow(dead_code)]

use crate::kit;

pub const ID: &str = "tokyo";
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_MAPBOX, kit::CREDIT_OPENFREEMAP_KEYLESS];
pub const STYLE: &str = "examples/mapbox-streets-style.json";

/// Without a token: Streets (open), our Mapbox Streets look-alike, whose OSM labels are Japanese
/// too (`name` first), in place of Mapbox Streets.
pub const KEYLESS: kit::KeylessStyle = kit::KeylessStyle::OpenStreets;

pub fn requirements(_: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network, kit::Requirement::MapboxToken { fallback: Some(KEYLESS.label()) }]
}

pub fn build(env: &kit::PanelEnv, _window: &mut gpui::Window, cx: &mut gpui::App) -> anyhow::Result<kit::Built> {
    let kit::PageBasemap { style, source, sprite, sprite_token, .. } =
        kit::mapbox_or_keyless(STYLE, STYLE, env.mapbox_token.as_deref())?;
    // The zoom argv override is a standalone-only convenience: a panel
    // embedded in the gallery must open at the same place every time.
    let zoom = std::env::args().nth(1).and_then(|z| z.parse().ok()).filter(|_| !env.host.is_gallery()).unwrap_or(15.0);
    let (w, h) = env.viewport;
    let (host, runtime) = (env.host.clone(), env.runtime.clone());
    let map = gpui::AppContext::new(cx, |_| {
        let mut v = gpui_maps::map_view::MapView::new(139.7671, 35.6812, zoom, w, h, style, source, runtime);
        if let Some(t) = host.title() {
            v.set_window_title_prefix(t.to_string());
        }
        if host.is_gallery() {
            v.set_chrome_above_map(true);
        }
        v
    });
    if let Some(s) = sprite {
        kit::spawn_sprites(map.downgrade(), s, sprite_token, env.runtime.clone(), cx);
    }
    let focus = if env.host.is_gallery() { crate::shell::FocusPolicy::Once } else { crate::shell::FocusPolicy::EveryFrame };
    let root = gpui::AppContext::new(cx, |_| crate::shell::FocusedMap::new(map.clone(), focus));
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (139.7671, 35.6812, zoom) })
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
        let source = include_str!("tokyo.rs");
        assert!(!source.contains(&needle), "tokyo.rs must not set an opaque background on the map chain");
    }
}
