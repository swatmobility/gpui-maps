//! A map that renders with the network unplugged. Shared by
//! `examples/offline_map.rs` and the gallery (`offline`). See that file's
//! module doc for the full pitch: tiles from a PMTiles archive
//! ([`gpui_maps::tile_source::pmtiles::PmtilesSource`], an mmap of one file),
//! style from the vendored `examples/liberty-style.json`, glyphs from the
//! system font book. The one network touch is OpenFreeMap's sprite sheet,
//! fetched into the sprite cache on a first run and read from it after
//! (`gpui_maps::cached_sprite_atlas`); with no cache and no network the map
//! draws without icons.
//!
//! `#![allow(dead_code)]`: this file is included by two crates (the
//! standalone example and the gallery) that each use a different half of
//! it, so some items are dead in one of them.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, AppContext as _, Window};
use gpui_maps::map_view::MapView;
use gpui_maps::sprite::SpriteAtlas;
use gpui_maps::tile_source::pmtiles::PmtilesSource;
use gpui_maps::tile_source::TileSource;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

pub const ID: &str = "offline";

/// The variable an offline archive's path is read from, in both the
/// requirement check and `build`'s own re-read.
pub const PMTILES_PATH_VAR: &str = "PMTILES_PATH";

/// How to get an archive, printed by the standalone `main` instead of a bare
/// "file not found" and shown verbatim by the gallery's missing-data card
/// (`kit::Requirement::EnvPath`'s `how`). Moved verbatim from
/// `examples/offline_map.rs`.
///
/// Somebody meeting this example for the first time has no `.pmtiles` file
/// and no reason to know that one is a thing you build rather than download
/// from this repository. A path error that does not say where the bytes come
/// from turns a five-minute task into a search.
pub const HOW_TO_GET_ONE: &str = "\
An offline map needs a PMTiles archive on disk. This repository does not ship
one -- even a single city is tens of megabytes.

To build the Singapore archive this example was developed against:

  brew install pmtiles      # or: github.com/protomaps/go-pmtiles/releases
  # then follow docs/OFFLINE.md, which has the exact commands.

Point PMTILES_PATH at the result:

  PMTILES_PATH=singapore.pmtiles cargo run --release --example offline_map

Any PMTiles archive in the OpenMapTiles schema works -- the vendored
examples/liberty-style.json names OpenMapTiles source layers (transportation,
water, building, place, ...) and draws nothing from an archive built to a
different schema. docs/OFFLINE.md says which sources produce which.";

/// Gated on `PMTILES_PATH` naming a readable file. The gallery blocks with
/// the missing-data card (`HOW_TO_GET_ONE`, above) instead of calling
/// `build` at all when it does not.
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::EnvPath { var: PMTILES_PATH_VAR, how: HOW_TO_GET_ONE }]
}

/// The archive's basemap (ODbL, via OpenStreetMap/OpenFreeMap) and the
/// committed sprite sheet, whose own licence has not been independently
/// verified -- see `examples/assets/PROVENANCE.md`.
pub const CREDITS: &[kit::Credit] = &[kit::CREDIT_OSM_OPENFREEMAP, kit::CREDIT_OFM_SPRITES_UNVERIFIED];

/// OpenFreeMap's sprite sheet. It is not in the tree (its licence is
/// unverified): the first run fetches it into `~/.cache/gpui-map/sprites/`,
/// and every later run, online or not, reads it from there.
const OFM_SPRITE: &str = "https://tiles.openfreemap.org/sprites/ofm_f384/ofm";

/// The archive's own header, read directly (not through `PmtilesSource`,
/// which deliberately exposes only what a `TileSource` owes its caller --
/// tiles, attribution, the zoom ceiling -- and not the whole header). Two
/// mmaps of one file costs nothing. Returns `(center_lon, center_lat, zoom,
/// min_zoom, max_zoom, bbox)`; `zoom` already applies the same
/// "`center_zoom == 0` means unstated" fallback `examples/offline_map.rs`
/// always has.
fn read_header(
    path: &Path,
    runtime: &tokio::runtime::Handle,
) -> anyhow::Result<(f64, f64, f64, u8, u8, (f64, f64, f64, f64))> {
    let reader = runtime.block_on(::pmtiles::AsyncPmTilesReader::new_with_path(path))?;
    let (center_lon, center_lat, min_zoom, max_zoom, center_zoom, bbox) = {
        let h = reader.get_header();
        (
            h.center_longitude,
            h.center_latitude,
            h.min_zoom,
            h.max_zoom,
            h.center_zoom,
            (h.min_longitude, h.min_latitude, h.max_longitude, h.max_latitude),
        )
    };
    drop(reader);
    // `center_zoom` is 0 in an archive whose writer never set it -- and 0 is
    // the whole world, which for a city extract means one blank tile.
    // Falling back to the archive's own ceiling puts the camera where the
    // data is.
    let zoom = if center_zoom == 0 { f64::from(max_zoom) - 2.0 } else { f64::from(center_zoom) };
    Ok((center_lon, center_lat, zoom, min_zoom, max_zoom, bbox))
}

/// OpenFreeMap's sheet from the sprite cache, fetching it once if it was
/// never cached. A failure is not fatal: the map draws without icons and
/// says why.
fn cached_sprites() -> Option<Arc<SpriteAtlas>> {
    gpui_maps::cached_sprite_atlas(OFM_SPRITE, false)
        .map(Arc::new)
        .inspect_err(|err| eprintln!("offline: no sprite sheet (not cached and the fetch failed: {err:#}); drawing without icons"))
        .ok()
}

/// Reads `PMTILES_PATH` again -- the requirement check has already gated
/// this against the real environment, but `build` trusts nothing it was not
/// handed directly, the same defense in depth every other panel's own token
/// read gives `Requirement::MapboxToken`.
pub fn build(env: &kit::PanelEnv, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    let path = std::env::var(PMTILES_PATH_VAR)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{PMTILES_PATH_VAR} is not set"))?;
    if !path.is_file() {
        anyhow::bail!("{PMTILES_PATH_VAR} points at {}, which is not a readable file", path.display());
    }

    let style = kit::compile_style("examples/liberty-style.json")?;
    let (center_lon, center_lat, zoom, min_zoom, max_zoom, bbox) = read_header(&path, &env.runtime)?;
    println!(
        "offline: {} covers {:.4},{:.4} .. {:.4},{:.4} at z{min_zoom}-{max_zoom}",
        path.display(),
        bbox.0,
        bbox.1,
        bbox.2,
        bbox.3,
    );
    println!(
        "offline: tiles, style and glyphs are all local; the sprite sheet comes from the sprite cache (fetched once on a first run)."
    );
    println!("Drag to pan, scroll to zoom. Panning outside the archive's bounds shows nothing: that is the edge of what was downloaded.");

    let source = env.runtime.block_on(PmtilesSource::open(&path))?;
    // The credit the archive itself carries. `PmtilesSource` reads it out of
    // the file's TileJSON metadata, so an OpenStreetMap-derived extract
    // keeps crediting OpenStreetMap with no help from this panel -- which
    // matters more offline, not less, since there is no attribution control
    // on a remote tile server to fall back on.
    match source.attribution() {
        Some(credit) => println!("offline: attribution from the archive: {credit}"),
        None => eprintln!(
            "offline: this archive states no attribution. If its data is OpenStreetMap-derived, \
             that is a licence problem in the archive, and `pmtiles edit` can add the field."
        ),
    }
    let source: Arc<dyn TileSource> = Arc::new(source);
    let sprites = cached_sprites();

    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    let map = cx.new(|_| {
        let mut view = MapView::new(center_lon, center_lat, zoom, w, h, style, source, runtime);
        if let Some(sprites) = sprites {
            view.set_sprites(sprites);
        }
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
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start: (center_lon, center_lat, zoom) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        let needle = [".", "bg", "("].concat();
        let source = include_str!("offline.rs");
        assert!(!source.contains(&needle), "offline.rs must not set an opaque background on the map chain");
    }
}
