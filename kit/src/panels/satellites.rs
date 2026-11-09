//! Every tracked satellite over the globe as a gallery panel, and the ONE
//! place its code is included from: the panel ([`panel::SatellitesPanel`]),
//! its model, its About text and the CelesTrak fetch live in
//! `demos/satellites/` and are included here by `#[path]`;
//! `demos/satellites.rs` (the standalone window and its `--capture` /
//! `--fetch-only` flags) reaches every one of them through
//! `#[path = "../kit/src/panels/satellites.rs"] mod satellites_panel;`, as
//! `demos/race_dots.rs` reaches `kit/src/panels/race_dots.rs`. No module
//! is compiled twice in either crate.
//!
//! Crate paths: `crate::shell` is `kit/src/shell.rs` and
//! `crate::kit` is `kit/src/kit.rs`, which every includer declares
//! at its crate root.
//!
//! **Native only, fetch included** (Task 8 review F10): `fetch` is behind
//! `cfg(not(target_arch = "wasm32"))` -- the browser has no file system
//! and its own fetch -- and so is every call into it in `panel`. The
//! gallery is not part of the web build (`web/` is its own workspace and
//! `scripts/check-wasm.sh` checks `web/` only), so nothing compiles this
//! file for wasm today; if the gallery ever joins it, add
//! `cargo check --target wasm32-unknown-unknown --example studio` there.
//!
//! `#![allow(dead_code)]`: the demo uses the headless helpers (the capture
//! presets, `load`), the gallery the panel entry points ([`build`],
//! [`requirements`], [`CREDITS`]) -- each item is dead in one of them, as
//! in `kit/src/panels/race_dots.rs`.
#![allow(dead_code)]

#[path = "../../../demos/satellites/about.rs"]
pub mod about;
#[cfg(not(target_arch = "wasm32"))]
#[path = "../../../demos/satellites/fetch.rs"]
pub mod fetch;
#[path = "../../../demos/satellites/charts.rs"]
pub mod charts;
#[path = "../../../demos/satellites/model.rs"]
pub mod model;
#[path = "../../../demos/satellites/panel.rs"]
pub mod panel;

use gpui::{App, AppContext as _, Window};

use crate::kit;
use panel::{Basemap, DataSource, PanelHost, PanelOptions, SatellitesPanel};

// ------------------------------------------------------------ the gallery

pub const ID: &str = "satellites";

/// The element sets: US Space Force data, redistributed by CelesTrak.
pub const CREDIT_CELESTRAK: kit::Credit = kit::Credit {
    what: "Orbits (GP element sets)",
    holder: "CelesTrak (T.S. Kelso); data from the US Space Force (18th/19th SDS) via Space-Track",
    dataset: "CelesTrak GP data: active, stations, gps-ops, galileo, glo-ops, beidou, geo (OMM JSON); SATCAT records of the active objects (owners, JSON)",
    licence: kit::Licence::Unverified,
    url: "https://celestrak.org",
    changes: "propagated with SGP4; objects with identical elements (docked modules) drawn as one dot; fetched at most once per group every 2 h",
};
/// The propagator (a code credit).
pub const CREDIT_SGP4: kit::Credit = kit::Credit {
    what: "Orbit propagation",
    holder: "International Centre for Neuromorphic Systems",
    dataset: "sgp4 2.4 (Rust SGP4 after Vallado's), MIT; Copyright (c) 2020 International Centre for Neuromorphic Systems",
    licence: kit::Licence::Mit,
    url: "https://github.com/neuromorphicsystems/sgp4",
    changes: "",
};
/// Mapbox Light (the Map and Night Earths) and Mapbox Streets (the
/// Streets Earth), with a token.
pub const CREDIT_MAPBOX_LIGHT: kit::Credit = kit::Credit {
    what: "Basemap tiles (with a token)",
    holder: "Mapbox, OpenStreetMap contributors",
    dataset: "Mapbox Light v11 and Mapbox Streets v12 styles (Mapbox Streets v8 tiles)",
    licence: kit::Licence::MapboxTos,
    url: "https://www.mapbox.com/about/maps",
    changes: "The Mapbox wordmark is also required by Mapbox's terms and is not drawn by this engine.",
};

/// The orbits, the propagator, and whichever basemap is drawn.
pub const CREDITS: &[kit::Credit] = &[CREDIT_CELESTRAK, CREDIT_SGP4, CREDIT_MAPBOX_LIGHT, kit::CREDIT_OSM_OPENFREEMAP, CREDIT_NATURAL_EARTH];

/// "Earth: real" (the default with the glowing dots).
pub const CREDIT_NATURAL_EARTH: kit::Credit = kit::Credit {
    what: "Earth imagery (\"Earth: real\")",
    holder: "Natural Earth",
    dataset: "Natural Earth II shaded relief, served by OpenFreeMap",
    licence: kit::Licence::PublicDomain,
    url: "https://www.naturalearthdata.com",
    changes: "drawn at 0.6 opacity over near-black, the basemap's water drawn over it in ocean blue (#2f6fa8 at 0.85), and its border lines only",
};

/// What the About pane lists.
pub const KEYS: &[(&str, &str)] = &[
    ("Click a chip", "filter by type or country; every count cross-filters"),
    ("Drag a chart", "brush the altitude or inclination range"),
    ("Hover a dot", "its card and its orbit, faint"),
    ("Click a dot", "select it: ring, ground track, Follow"),
    ("Show GEO / Show LEO", "zoom out to the shell"),
    ("Time bar", "play, speed, ±24 h scrubber, Now"),
    ("Esc", "deselect"),
    ("T / G", "terminator, globe"),
];

/// Network (CelesTrak, the basemap) and a token for Mapbox Light
/// (Positron without one). No file: an empty cache is fetched on open, and
/// without a network the panel offers the synthetic set.
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network, kit::Requirement::MapboxToken { fallback: Some("OpenFreeMap Positron") }]
}

/// The panel over the default cache, refreshed once on open.
pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, DataSource::default_cache(), window, cx)
}

/// The panel over `data`, hosted by `env`. Cheap: the catalogue is read
/// and built off the UI thread.
pub fn build_with(env: &kit::PanelEnv, data: DataSource, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let basemap = Basemap::for_token(env.mapbox_token.as_deref());
    let options = PanelOptions { basemap, data, size: (env.viewport.0 as f32, env.viewport.1 as f32), ..PanelOptions::default() };
    let host = PanelHost {
        runtime: env.runtime.clone(),
        mapbox_token: env.mapbox_token.clone(),
        title: env.host.title().map(str::to_string),
        cancel: env.cancel.clone(),
        about_open: false,
    };
    let panel = cx.new(|cx| SatellitesPanel::new(window, cx, options, host));
    let map = panel.read(cx).map().downgrade();
    Ok(kit::Built { view: panel.into(), map: Some(map), start: panel::START })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use gpui::Entity;

    use super::panel::{DataSource, SatellitesPanel};
    use super::{build_with, requirements, CREDITS};
    use crate::kit;

    fn env(rt: &tokio::runtime::Runtime) -> kit::PanelEnv {
        kit::PanelEnv { host: kit::Host::Gallery, runtime: rt.handle().clone(), mapbox_token: None, viewport: (900.0, 700.0), cancel: Default::default(), clickhouse: None }
    }

    fn nowhere() -> DataSource {
        DataSource::Cache { dir: PathBuf::from("/nonexistent/gpui-map-satellites-cache"), fetch: false }
    }

    #[test]
    fn the_credits_name_celestrak_the_space_force_sgp4_and_both_basemaps() {
        assert!(CREDITS.iter().any(|c| c.holder.contains("CelesTrak") && c.holder.contains("US Space Force")));
        assert!(CREDITS.iter().any(|c| c.dataset.contains("sgp4") && c.dataset.contains("MIT")));
        assert!(CREDITS.iter().any(|c| c.dataset.contains("Mapbox Light")));
        assert!(CREDITS.iter().any(|c| c.dataset == kit::CREDIT_OSM_OPENFREEMAP.dataset));
        let reqs = requirements(&env(&tokio::runtime::Runtime::new().unwrap()));
        assert!(reqs.iter().any(|r| matches!(r, kit::Requirement::MapboxToken { fallback: Some(_) })), "keyless: Positron");
        assert!(!reqs.iter().any(|r| matches!(r, kit::Requirement::File { .. })), "an empty cache is not a blocker");
    }

    /// The gallery holds the `SatellitesPanel` itself, and this file sets
    /// no background (the panel's own root is pinned in `panel.rs`).
    #[test]
    fn the_gallery_holds_the_panel_itself_and_sets_no_background() {
        let needle = [".", "bg", "("].concat();
        assert!(!include_str!("satellites.rs").contains(&needle));
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| build_with(&env, nowhere(), window, cx)).expect("builds");
        assert!(built.view.clone().downcast::<SatellitesPanel>().is_ok());
        drop(built);
        drop(rt);
    }

    /// Switching away (the gallery drops `Built` and raises the cancel
    /// flag): the panel and its map go, and a load landing afterwards is
    /// never attached.
    #[test]
    fn switching_away_releases_the_panel_and_its_map() {
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| build_with(&env, DataSource::Synthetic, window, cx)).expect("builds");
        let panel: Entity<SatellitesPanel> = built.view.clone().downcast().ok().unwrap();
        let (view, map) = (built.view.downgrade(), built.map.clone().unwrap());
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |p, _| p.loaded()), "a cancelled panel attached its catalogue");
        drop(panel);
        drop(built);
        cx.update(|_, _| {});
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(view.upgrade().is_none(), "the panel is released");
        assert!(map.upgrade().is_none(), "the map is released");
        drop(rt);
    }
}
