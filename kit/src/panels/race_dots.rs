//! The 2020 Census race and ethnicity map (after Kyle Walker's) as a
//! gallery panel, and the ONE place its code is included from: the panel
//! ([`panel::RaceMapPanel`]) and its data modules live in `demos/race_dots/`
//! and are included here by `#[path]`; `demos/race_dots.rs` (the standalone
//! window and its `--measure`/`--capture`/`--pan`/`--zoom-sweep` flags)
//! reaches every one of them through
//! `#[path = "../kit/src/panels/race_dots.rs"] mod race_dots_panel;`, as
//! `demos/world_dots.rs` reaches `kit/src/panels/world_dots.rs`. No module
//! is compiled twice in either crate, and neither crate keeps a copy.
//!
//! Crate paths: `crate::shell` is `kit/src/shell.rs` and
//! `crate::kit` is `kit/src/kit.rs`, which every includer declares
//! at its crate root.
//!
//! `#![allow(dead_code)]`: the demo uses the headless helpers (the capture
//! hooks, `open_dots`, the HUD words), the gallery the panel entry points
//! ([`build`], [`requirements`], [`CREDITS`]) -- each item is dead in one
//! of them, as in `kit/src/panels/world_dots.rs`.
#![allow(dead_code)]

#[path = "../../../demos/race_dots/about.rs"]
pub mod about;
#[path = "../../../demos/race_dots/clusters.rs"]
pub mod clusters;
#[path = "../../../demos/race_dots/hexes.rs"]
pub mod hexes;
#[path = "../../../demos/race_dots/panel.rs"]
pub mod panel;
#[path = "../../../demos/race_dots/states.rs"]
pub mod states;
#[path = "../../../demos/race_dots/states_data.rs"]
pub mod states_data;
#[path = "../../../demos/race_dots/walker.rs"]
pub mod walker;

use std::path::Path;

use gpui::{App, AppContext as _, Window};

use crate::kit;
use hexes::HexSourceSpec;
use panel::{Basemap, PanelHost, PanelOptions, RaceMapPanel, RaceSources};

// ------------------------------------------------------------ the gallery

pub const ID: &str = "race-map";

/// `demos/README.md`'s `race_dots` commands, verbatim: the dots (fetch the
/// P.L. 94-171 and DC DHC zips, then stream the TIGER/Line blocks into the
/// archive), the clusters (from that build's `blocks.points`), the
/// hexagons (through a local ClickHouse).
pub const BUILD_DOTS: &str = "scripts/fetch-census-2020.sh --pl --dhc-dc && cargo run --release --manifest-path tools/race-build/Cargo.toml -- build";
pub const BUILD_CLUSTERS: &str = "cargo run --release --manifest-path tools/race-build/Cargo.toml -- export-clusters";
pub const BUILD_HEXES: &str = "cargo run --release --manifest-path tools/race-build/Cargo.toml -- load-clickhouse && cargo run --release --manifest-path tools/race-build/Cargo.toml -- export-hex";

/// The Census counts every dot, hexagon and donut is drawn from.
pub const CREDIT_CENSUS_2020_PL: kit::Credit = kit::Credit {
    what: "Race and ethnicity counts by block",
    holder: "U.S. Census Bureau",
    dataset: "2020 Census Redistricting Data (P.L. 94-171), table P2",
    licence: kit::Licence::PublicDomain,
    url: "https://www.census.gov/programs-surveys/decennial-census/about/rdo/summary-files.html",
    changes: "one dot per person, placed at random inside its block (tools/race-build); hexagons and clusters summed from the blocks",
};
/// The block shapes the dots are placed inside (never drawn themselves).
pub const CREDIT_CENSUS_TIGER_2020: kit::Credit = kit::Credit {
    what: "Block shapes (dot placement)",
    holder: "U.S. Census Bureau",
    dataset: "TIGER/Line Shapefiles 2020, blocks",
    licence: kit::Licence::PublicDomain,
    url: "https://www.census.gov/geographies/mapping-files/time-series/geo/tiger-line-file.html",
    changes: "used to place the dots; not drawn",
};
/// States mode's outlines and pie anchors.
pub const CREDIT_CENSUS_CB_STATES: kit::Credit = kit::Credit {
    what: "State outlines (States mode)",
    holder: "U.S. Census Bureau",
    dataset: "2020 Cartographic Boundary Files, states, 1:20,000,000 (cb_2020_us_state_20m)",
    licence: kit::Licence::PublicDomain,
    url: "https://www.census.gov/geographies/mapping-files/time-series/geo/carto-boundary-file.html",
    changes: "outlines only, rounded to 0.001 degrees; pie anchors are each state's pole of inaccessibility (scripts/gen-race-states.py)",
};
/// Hexagon mode's grid.
pub const CREDIT_H3: kit::Credit = kit::Credit {
    what: "Hexagon grid (hexagon mode)",
    holder: "Uber Technologies, Inc.",
    dataset: "H3 hierarchical hexagonal grid, resolutions 4-8",
    licence: kit::Licence::Apache2,
    url: "https://h3geo.org",
    changes: "",
};
/// Kyle Walker's map: the eight groups, the modes and the zoom hand-overs
/// this panel follows; its colours are the Okabe-Ito palette.
pub const CREDIT_WALKER: kit::Credit = kit::Credit {
    what: "Group definitions, design and palette credit",
    holder: "Kyle Walker",
    dataset: "2020 race and ethnicity map (walker-data.com), after the Racial Dot Map (Dustin Cable, 2013)",
    licence: kit::Licence::Unverified,
    url: about::WALKER_URL,
    changes: "his eight groups, modes and zoom hand-overs, rebuilt natively; colours from the Okabe-Ito colour-blind-safe palette (Okabe & Ito, 2008)",
};
/// Mapbox Light: the Streets v8 tiles under Walker's grey style.
pub const CREDIT_MAPBOX_LIGHT: kit::Credit = kit::Credit {
    what: "Basemap tiles (with a token)",
    holder: "Mapbox, OpenStreetMap contributors",
    dataset: "Mapbox Light v11 (Mapbox Streets v8)",
    licence: kit::Licence::MapboxTos,
    url: "https://www.mapbox.com/about/maps",
    changes: "The Mapbox wordmark is also required by Mapbox's terms and is not drawn by this engine.",
};

/// The data, the grid, Walker's design, and whichever basemap is drawn.
pub const CREDITS: &[kit::Credit] =
    &[CREDIT_CENSUS_2020_PL, CREDIT_CENSUS_TIGER_2020, CREDIT_CENSUS_CB_STATES, CREDIT_H3, CREDIT_WALKER, CREDIT_MAPBOX_LIGHT, kit::CREDIT_OSM_OPENFREEMAP];

/// What the About pane lists.
pub const KEYS: &[(&str, &str)] = &[
    ("Click a legend group", "hide or show it"),
    ("Dots / Hexagons / Clusters / States", "switch the view (top right)"),
    ("Hover a hexagon", "light it and read its groups' shares"),
    ("Click a hexagon", "keep its card open"),
    ("Click a donut", "zoom into its cluster"),
    ("Click a state's pie or bar", "frame it and show its groups"),
    ("T / G", "terminator, globe"),
];

/// Network and a token for Mapbox Light (Positron without one); the dot
/// archive, without which there is nothing to show (the missing-data
/// card); the hexagon and cluster archives, each of which only greys its
/// own mode out (a banner line, the reason beside the radio).
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    requirements_at(&RaceSources::default_dots(), &HexSourceSpec::default_pmtiles(), &clusters::default_pmtiles())
}

/// [`requirements`] with the archives at the given paths: a test names
/// paths that are not there rather than depending on the real `$HOME`.
pub fn requirements_at(dots: &Path, hexes: &Path, clusters: &Path) -> Vec<kit::Requirement> {
    vec![
        kit::Requirement::Network,
        kit::Requirement::MapboxToken { fallback: Some("OpenFreeMap Positron") },
        kit::Requirement::File {
            path: dots.to_path_buf(),
            what: "Race dots archive",
            fetch: BUILD_DOTS,
            size: "~0.79 GB; ~11.3 GB fetched but streamed",
            fallback: None,
            then: None,
        },
        kit::Requirement::File {
            path: hexes.to_path_buf(),
            what: "Hexagon archive",
            fetch: BUILD_HEXES,
            size: "~45.7 MB; needs a local ClickHouse, ~0.67 GB loaded",
            fallback: Some("other views (Hexagons greyed out)"),
            then: None,
        },
        kit::Requirement::File {
            path: clusters.to_path_buf(),
            what: "Cluster archive",
            fetch: BUILD_CLUSTERS,
            size: "~21.5 MB, from the dots build",
            fallback: Some("other views (Clusters greyed out)"),
            then: None,
        },
    ]
}

/// The panel over the three archives at their default paths. A missing
/// hexagon or cluster archive greys its mode out with the reason (the dots
/// are required: `requirements` blocks the panel without them).
pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let sources = RaceSources {
        dots: RaceSources::default_dots(),
        hexes: Some(HexSourceSpec::Pmtiles(HexSourceSpec::default_pmtiles())),
        clusters: Some(clusters::default_pmtiles()),
    };
    build_with(env, sources, window, cx)
}

/// The panel over `sources`, hosted by `env`: its runtime, its token
/// (Mapbox Light with one, Positron without), its cancel flag, and no
/// window title. Cheap: the archives open off the UI thread.
pub fn build_with(env: &kit::PanelEnv, sources: RaceSources, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let basemap = if env.mapbox_token.is_some() { Basemap::MapboxLight } else { Basemap::Positron };
    let mut options = PanelOptions { basemap, size: (env.viewport.0 as f32, env.viewport.1 as f32), ..PanelOptions::default() };
    // `GPUI_MAP_STUDIO_RACE_MODE=hexagons`: open in hexagon mode (for
    // screenshots of the linked group chart).
    if std::env::var("GPUI_MAP_STUDIO_RACE_MODE").is_ok_and(|m| m == "hexagons") {
        options.mode = panel::Mode::Hexagons;
    }
    let host = PanelHost {
        runtime: env.runtime.clone(),
        mapbox_token: env.mapbox_token.clone(),
        title: env.host.title().map(str::to_string),
        cancel: env.cancel.clone(),
        // The gallery's own About pane carries the credits: the in-map
        // card starts collapsed so the two do not say it twice.
        about_open: !env.host.is_gallery(),
    };
    let panel = cx.new(|cx| RaceMapPanel::new(window, cx, sources, options, host));
    // Hexagon mode fills the map with hexagons: its basemap labels go over
    // them (`crate::data_labels`) while it is the mode, and back to the
    // basemap's own paint in the dot, cluster and state views.
    let map_entity = panel.read(cx).map().clone();
    let document = match basemap {
        Basemap::MapboxLight => crate::data_labels::document("demos/mapbox-light-style.json").ok(),
        Basemap::Positron => crate::data_labels::document("demos/positron-style.json").ok(),
        Basemap::None => None,
    };
    let hexagons = |p: &RaceMapPanel| p.mode() == panel::Mode::Hexagons;
    let labels = crate::data_labels::DataLabels::attach(&map_entity, document, hexagons(panel.read(cx)), cx);
    cx.observe(&panel, move |panel, cx| {
        let on = hexagons(panel.read(cx));
        labels.set_active(&map_entity, on, cx);
    })
    .detach();
    let map = panel.read(cx).map().downgrade();
    Ok(kit::Built { view: panel.into(), map: Some(map), start: walker::START })
}

#[cfg(test)]
mod tests {
    // No `use super::*` (gallery test modules name what they use).
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use gpui::Entity;

    use super::hexes::HexSourceSpec;
    use super::panel::{RaceMapPanel, RaceSources};
    use super::{build_with, requirements_at, BUILD_CLUSTERS, BUILD_DOTS, BUILD_HEXES, CREDITS};
    use crate::kit;

    fn env(rt: &tokio::runtime::Runtime) -> kit::PanelEnv {
        kit::PanelEnv {
            host: kit::Host::Gallery,
            runtime: rt.handle().clone(),
            mapbox_token: None,
            viewport: (900.0, 700.0),
            cancel: Default::default(),
            clickhouse: None,
        }
    }

    fn nowhere() -> RaceSources {
        RaceSources {
            dots: PathBuf::from("/nonexistent/race-dots-2020.pmtiles"),
            hexes: Some(HexSourceSpec::Pmtiles(PathBuf::from("/nonexistent/race-hex-2020.pmtiles"))),
            clusters: Some(PathBuf::from("/nonexistent/race-clusters-2020.pmtiles")),
        }
    }

    /// The commands are `demos/README.md`'s, verbatim.
    #[test]
    fn the_build_commands_are_the_readmes() {
        let readme = include_str!("../../../demos/README.md");
        for cmd in BUILD_DOTS.split(" && ").chain(BUILD_HEXES.split(" && ")).chain([BUILD_CLUSTERS]) {
            let cmd = cmd.trim();
            assert!(readme.contains(cmd), "not in demos/README.md: {cmd}");
        }
    }

    /// No dots: the missing-data card (no fallback), with the one command
    /// line that builds them. No hexagons or clusters: a fallback each,
    /// naming the mode that is greyed out; the paths are shown with `~`.
    #[test]
    fn the_dots_block_and_the_other_archives_only_grey_their_mode_out() {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/Users/x".into()));
        let at = |name: &str| home.join(".cache/gpui-map-data/census2020").join(name);
        let reqs = requirements_at(&at("race-dots-2020.pmtiles"), &at("race-hex-2020.pmtiles"), &at("race-clusters-2020.pmtiles"));
        let files: Vec<_> = reqs.iter().filter(|r| matches!(r, kit::Requirement::File { .. })).collect();
        assert_eq!(files.len(), 3);
        let statuses: Vec<kit::Status> = files.iter().map(|r| kit::check(r, &|_| None, &|_| false)).collect();
        let kit::Status::Missing { why } = &statuses[0] else { panic!("the dots block: {:?}", statuses[0]) };
        assert_eq!(why, "Race dots archive not found at ~/.cache/gpui-map-data/census2020/race-dots-2020.pmtiles");
        for (status, mode) in statuses[1..].iter().zip(["Hexagons", "Clusters"]) {
            let kit::Status::Fallback { why } = status else { panic!("{status:?}") };
            assert!(why.contains(&format!("{mode} greyed out")) && why.contains("~/.cache/gpui-map-data/census2020/race-"), "{why}");
        }
        let (fetch, size) = match files[0] {
            kit::Requirement::File { fetch, size, then, .. } => {
                assert!(then.is_none(), "one command line keeps the card compact");
                (*fetch, *size)
            }
            _ => unreachable!(),
        };
        assert_eq!(fetch, BUILD_DOTS);
        assert!(size.contains("0.79 GB") && size.chars().count() <= 48, "{size}");
        // With every archive there, nothing is missing.
        assert!(files.iter().all(|r| kit::check(r, &|_| None, &|_| true) == kit::Status::Met));
        assert!(reqs.iter().any(|r| matches!(r, kit::Requirement::MapboxToken { fallback: Some(_) })), "keyless: Positron");
    }

    /// The Census (P.L. 94-171 and TIGER/Line, public domain), H3, Walker's
    /// groups and palette, and both basemaps.
    #[test]
    fn the_credits_name_the_census_h3_walker_and_both_basemaps() {
        let datasets: Vec<&str> = CREDITS.iter().map(|c| c.dataset).collect();
        assert!(datasets.iter().any(|d| d.contains("P.L. 94-171")));
        assert!(datasets.iter().any(|d| d.contains("TIGER/Line Shapefiles 2020")));
        assert!(datasets.iter().any(|d| d.contains("H3")));
        assert!(datasets.contains(&kit::CREDIT_OSM_OPENFREEMAP.dataset));
        assert!(datasets.iter().any(|d| d.contains("Mapbox Light")));
        let walker = CREDITS.iter().find(|c| c.holder == "Kyle Walker").expect("Walker");
        assert!(walker.changes.contains("Okabe-Ito") && walker.what.contains("palette"));
        assert!(CREDITS.iter().filter(|c| c.holder == "U.S. Census Bureau").all(|c| c.licence == kit::Licence::PublicDomain));
    }

    /// The backgrounds on the window -> `MapView` chain in `panel`'s
    /// source: the panel's render from its root to the map's box, and the
    /// map's box (`map_area`) itself.
    fn chain_backgrounds(panel: &str) -> usize {
        let needle = [".", "bg", "("].concat();
        let root = &panel[panel.find(&["impl Render for ", "RaceMapPanel"].concat()).expect("the render")..];
        let root = &root[..root.find(".child(self.map_area(cx))").expect("the map child")];
        let area = &panel[panel.find(&["fn ", "map_area("].concat()).expect("map_area")..panel.find(&["fn ", "map_child("].concat()).expect("map_child")];
        root.matches(&needle).count() + area.matches(&needle).count()
    }

    /// The window -> `MapView` chain, gallery side: the gallery holds the
    /// `RaceMapPanel` itself (no wrapper view), this file sets no
    /// background, and the panel sets none from its root to the map's box
    /// (also pinned in `demos/race_dots/panel.rs`, so a gallery build
    /// fails too). The check is run on a copy with a background put on the
    /// root, and on one with a background on the map's box: it must catch
    /// both.
    #[test]
    fn the_gallery_chain_down_to_the_map_sets_no_background() {
        let needle = [".", "bg", "("].concat();
        assert!(!include_str!("race_dots.rs").contains(&needle), "the gallery panel sets no background");
        let panel = include_str!("../../../demos/race_dots/panel.rs");
        assert_eq!(chain_backgrounds(panel), 0, "the panel's root or the map's box sets a background");
        let on_root = panel.replacen(".id(\"race-dots-root\")", &[".id(\"race-dots-root\").", "bg(gpui::white())"].concat(), 1);
        assert_eq!(chain_backgrounds(&on_root), 1, "a background on the root is caught");
        let on_area = panel.replacen(".id(\"race-map-area\")", &[".id(\"race-map-area\").", "bg(gpui::white())"].concat(), 1);
        assert_eq!(chain_backgrounds(&on_area), 1, "a background on the map's box is caught");
        // And `build` hands the gallery the panel itself.
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let cx = app.add_empty_window();
        let built = cx.update(|window, cx| build_with(&env, nowhere(), window, cx)).expect("builds");
        assert!(built.view.clone().downcast::<RaceMapPanel>().is_ok(), "the slot's view is the RaceMapPanel");
        drop(built);
        drop(rt);
    }

    /// In the gallery the in-map About card starts collapsed (the gallery
    /// has its own About pane); in its own window it starts open.
    #[test]
    fn the_in_map_about_card_starts_collapsed_only_in_the_gallery() {
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cx = app.add_empty_window();
        let gallery = env(&rt);
        let standalone = kit::PanelEnv { host: kit::Host::Standalone { title: "Race dots".into() }, ..env(&rt) };
        for (env, open) in [(gallery, false), (standalone, true)] {
            let built = cx.update(|window, cx| build_with(&env, nowhere(), window, cx)).expect("builds");
            let panel: Entity<RaceMapPanel> = built.view.clone().downcast().ok().unwrap();
            assert_eq!(panel.read_with(cx, |p, _| p.about_expanded()), open, "{:?}", env.host);
        }
        // The standalone demo's own constructor keeps it open.
        assert!(super::panel::PanelHost::standalone().about_open);
        drop(rt);
    }

    /// Review L3: the cancel flag alone stops an attach. The panel is kept
    /// ALIVE (as for the frame after a switch, while the drawn frame still
    /// holds it), the flag is raised while the archives are still opening
    /// on the background executor, and the open then finishes: nothing may
    /// be attached. The positive control, with the flag down, attaches.
    #[test]
    fn a_raised_cancel_stops_the_attach_even_with_the_panel_alive() {
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cx = app.add_empty_window();
        for raise in [true, false] {
            let env = env(&rt);
            let built = cx.update(|window, cx| build_with(&env, nowhere(), window, cx)).expect("builds");
            let panel: Entity<RaceMapPanel> = built.view.clone().downcast().ok().unwrap();
            // Nothing has run on the background executor yet: the open is
            // still in flight when the switch lands.
            assert!(!panel.read_with(cx, |p, _| p.loaded()));
            if raise {
                env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.run_until_parked();
            let (loaded, hud) = panel.read_with(cx, |p, _| (p.loaded(), p.hud()));
            if raise {
                assert!(!loaded && hud == "loading dots…", "a cancelled panel attached its sources: {hud}");
            } else {
                assert!(loaded && hud.starts_with("no dots:"), "the positive control attached: {hud}");
            }
            drop(panel);
            drop(built);
        }
        drop(rt);
    }

    /// Switching away (the gallery drops `Built` and raises the cancel
    /// flag): the panel, its map and every task holding them go at once --
    /// the 30 Hz tick, the archive open and the sprite fetch hold weak
    /// handles only. The positive control: kept open, the same panel
    /// attaches its (failed) sources.
    #[test]
    fn switching_away_releases_the_panel_and_its_map() {
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let cx = app.add_empty_window();
        // Kept: its sources land.
        let kept = cx.update(|window, cx| build_with(&env, nowhere(), window, cx)).expect("builds");
        let kept_panel: Entity<RaceMapPanel> = kept.view.clone().downcast().ok().unwrap();
        cx.run_until_parked();
        assert!(kept_panel.read_with(cx, |p, _| p.loaded()), "the positive control attached its sources");
        drop(kept_panel);
        drop(kept);
        // Switched away before the sources land.
        let env = kit::PanelEnv { cancel: Default::default(), ..env };
        let built = cx.update(|window, cx| build_with(&env, nowhere(), window, cx)).expect("builds");
        let (view, map) = (built.view.downgrade(), built.map.clone().unwrap());
        assert!(map.upgrade().is_some());
        env.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(built);
        cx.update(|_, _| {});
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(view.upgrade().is_none(), "the panel is released");
        assert!(map.upgrade().is_none(), "the map is released");
        drop(rt);
    }

    /// Time to first dots, headless (no GPU, so up to the dots being
    /// ATTACHED, not drawn): how long `build` holds the UI thread, and how
    /// long the three real archives take to open off it. Needs the
    /// archives in `~/.cache/gpui-map-data/census2020`; run with
    /// `cargo test --release --example studio -- --ignored --nocapture time_to_attach`.
    #[test]
    #[ignore = "needs the real race archives"]
    fn time_to_attach_the_real_archives() {
        let dots = RaceSources::default_dots();
        assert!(Path::new(&dots).exists(), "no archive at {}", dots.display());
        let mut app = gpui::TestAppContext::single();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = env(&rt);
        let cx = app.add_empty_window();
        let sources = RaceSources {
            dots,
            hexes: Some(HexSourceSpec::Pmtiles(HexSourceSpec::default_pmtiles())),
            clusters: Some(super::clusters::default_pmtiles()),
        };
        let t = Instant::now();
        let built = cx.update(|window, cx| build_with(&env, sources, window, cx)).expect("builds");
        let build_ms = t.elapsed().as_secs_f64() * 1e3;
        let panel: Entity<RaceMapPanel> = built.view.clone().downcast().ok().unwrap();
        assert!(!panel.read_with(cx, |p, _| p.loaded()), "nothing was opened on the UI thread");
        cx.run_until_parked();
        let attached_ms = t.elapsed().as_secs_f64() * 1e3;
        let (loaded, lines) = panel.read_with(cx, |p, _| (p.loaded(), p.unavailable_lines()));
        assert!(loaded && lines.is_empty(), "every mode available: {lines:?}");
        let has_layer = built.map.clone().unwrap().upgrade().unwrap().read_with(cx, |m, _| m.dot_stats(super::panel::LAYER).is_some());
        assert!(has_layer, "the dot layer is attached");
        println!("race-map: build held the UI thread {build_ms:.1} ms; all three archives open and attached at {attached_ms:.1} ms");
        drop(panel);
        drop(built);
        drop(rt);
    }
}
