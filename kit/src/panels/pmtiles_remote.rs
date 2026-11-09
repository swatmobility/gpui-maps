//! A map drawn straight out of a PMTiles archive over HTTP range requests,
//! with no tile server anywhere in the picture. Shared by
//! `examples/pmtiles_remote.rs` and the gallery (`pmtiles-remote`).
//!
//! The gallery's own entry (`build`) always opens Protomaps' public Florence
//! demo archive against the vendored Protomaps basemap style: it never reads
//! the process's `argv`. The standalone example still takes an optional
//! `archive` URL/path and `style` file on the command line -- see
//! `examples/pmtiles_remote.rs`'s module doc -- and passes them through
//! [`build_with`] explicitly, so this panel itself has exactly one thing to
//! decide about `argv`: never look at it.
//!
//! `#![allow(dead_code)]`: this file is included by two crates (the
//! standalone example and the gallery) that each use a different half of
//! it, so some items are dead in one of them.
#![allow(dead_code)]

use std::sync::Arc;

use gpui::{App, AppContext as _, Window};
use gpui_maps::map_view::MapView;
use gpui_maps::tile_source::pmtiles::{PmtilesSource, DEMO_ARCHIVE_URL};
use gpui_maps::tile_source::TileSource;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "pmtiles-remote";

/// Hand-authored against the Protomaps basemap v3 schema the Florence demo
/// archive (and `DEMO_ARCHIVE_URL` generally) is built to -- NOT the
/// OpenMapTiles schema the vendored Liberty style targets. Pointing this
/// style at an OpenMapTiles archive (or vice versa) compiles cleanly and
/// draws an empty page: the two share no `source-layer` name at all.
pub const DEFAULT_STYLE: &str = "examples/protomaps-basemap-style.json";

/// One 16 KiB request to open the archive header, then one or two more per
/// tile -- nothing is downloaded up front. `Network` is the only
/// requirement: there is no token to gate this on.
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network]
}

/// Protomaps' basemap archive, ODbL via OpenStreetMap -- the only source
/// this panel ever draws inside the gallery (always Florence; see `build`).
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_PROTOMAPS_FLORENCE];

/// The gallery's panel: always the Protomaps demo archive over Florence.
pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, DEMO_ARCHIVE_URL, DEFAULT_STYLE, window, cx)
}

/// `archive` is a URL (`https://`/`http://`) or a local path -- one string
/// either way, the same bytes read the same way whether they are on disk or
/// behind a `Range` header (see `examples/pmtiles_remote.rs`'s module doc).
/// `style` names the vendored style file to compile against it.
///
/// Read from `argv` by the standalone `main` ONLY: this function takes them
/// as plain arguments rather than reading `std::env::args()` itself, so the
/// gallery's own `pmtiles-remote` (which calls [`build`], never this
/// directly with anything but its own defaults) always opens Florence no
/// matter how the gallery process itself was invoked.
pub fn build_with(
    env: &kit::PanelEnv,
    archive: &str,
    style: &str,
    _window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<kit::Built> {
    let style_compiled = kit::compile_style(style)?;

    // Anything that is not `http(s)://` is a path, rather than the other way
    // round: a mistyped URL should fail as a missing file, not be silently
    // fetched from somewhere unintended.
    let is_url = archive.starts_with("https://") || archive.starts_with("http://");
    let source = env.runtime.block_on(async {
        if is_url { PmtilesSource::open_url(archive).await } else { PmtilesSource::open(archive).await }
    })?;

    let coverage = source.coverage();
    let (w, h) = env.viewport;
    // The framing lives on `Coverage`, not here: the naive version -- trust
    // the header's `center_zoom` -- opens a five-kilometre extract on the
    // whole planet. See `Coverage::opening_camera`.
    let (lon, lat, zoom) = coverage.opening_camera(w, h);
    eprintln!("archive:     {archive}");
    eprintln!(
        "coverage:    {:.4},{:.4} .. {:.4},{:.4}  z{}-{}",
        coverage.min_lon, coverage.min_lat, coverage.max_lon, coverage.max_lat, coverage.min_zoom, coverage.max_zoom
    );
    eprintln!("opening at:  {lon:.4},{lat:.4} z{zoom:.1}");
    // A licence obligation the map also draws on screen -- see
    // `TileSource::attribution`. An archive that states none says so here
    // rather than silently showing nothing.
    eprintln!("attribution: {}", source.attribution().unwrap_or("(none stated)"));

    let source: Arc<dyn TileSource> = Arc::new(source);
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(lon, lat, zoom, w, h, style_compiled, source, runtime);
        if let Some(title) = host.title() {
            view.set_window_title_prefix(title.to_string());
        }
        if host.is_gallery() {
            view.set_chrome_above_map(true);
        }
        view
    });
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let root = cx.new(|_| FocusedMap::new(map.clone(), focus));
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (lon, lat, zoom) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        // An opaque background anywhere on the window -> MapView chain hides
        // the map, which draws in a CAMetalLayer behind gpui -- the bug
        // `race_dots` shipped. The needle is built at runtime so this
        // assertion does not match itself.
        let needle = [".", "bg", "("].concat();
        let source = include_str!("pmtiles_remote.rs");
        assert!(!source.contains(&needle), "pmtiles_remote.rs must not set an opaque background on the map chain");
    }
}
