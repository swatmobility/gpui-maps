//! Keyless basemaps: ready-to-mount styles over OpenFreeMap's planet tiles,
//! with no account, no token and no Mapbox host anywhere in the chain.
//!
//! ```no_run
//! use gpui_maps::basemaps::{self, Basemap};
//!
//! let b = basemaps::load(Basemap::StreetsLight)?;
//! // b.style   -> MapView::new(cx, b.style, b.source, ..)
//! // b.source  -> OpenFreeMap planet; its attribution is drawn on the map
//! // b.sprite  -> basemaps::spawn_sprites(map.downgrade(), s, None, rt, cx)
//! assert_eq!(b.basemap.attribution(), basemaps::OPENFREEMAP_ATTRIBUTION);
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! # What is embedded
//!
//! Six style documents, compiled into the crate with `include_bytes!` from
//! where the repository has always kept them (`examples/`, `demos/`,
//! `examples/styles-open/`) -- one copy, so the examples, the iOS and web
//! shells, the story fixtures and this module can never draw different
//! versions of the same style. Each one's origin and licence is in
//! `src/basemaps/PROVENANCE.md`.
//!
//! # What goes over the network
//!
//! Tiles from `tiles.openfreemap.org/planet` (OpenMapTiles schema, ODbL) and
//! the sprite sheet from `tiles.openfreemap.org/sprites/...`, both with the
//! crate's [`TILE_USER_AGENT`](crate::tile_source::http::TILE_USER_AGENT).
//! Labels are shaped from system fonts (`font_fallback`), so the styles'
//! `glyphs` URL is never fetched. Nothing is cached on disk unless the caller
//! passes a [`DiskCache`](crate::tile_source::disk_cache::DiskCache) to
//! [`load_with_disk_cache`] -- the crate's usual "off unless asked" rule.
//!
//! # Attribution
//!
//! The source's credit is OpenFreeMap's TileJSON `attribution`
//! ([`OPENFREEMAP_ATTRIBUTION_HTML`]); the map draws it automatically from
//! [`TileSource::attribution`]. Do not pass an empty credit when rebuilding
//! the source yourself: the data is ODbL.

use std::sync::Arc;

use anyhow::Context as _;

use crate::style::Style;
use crate::tile_source::http::HttpTileSource;
use crate::tile_source::TileSource;

/// OpenFreeMap's credit, verbatim from `tiles.openfreemap.org/planet`'s
/// TileJSON `attribution`.
pub const OPENFREEMAP_ATTRIBUTION_HTML: &str = concat!(
    r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> "#,
    r#"<a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> "#,
    r#"Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#,
);

/// [`OPENFREEMAP_ATTRIBUTION_HTML`] as the map draws it (tags dropped,
/// `&copy;` decoded).
pub const OPENFREEMAP_ATTRIBUTION: &str = "OpenFreeMap \u{a9} OpenMapTiles Data from OpenStreetMap";

/// A keyless basemap. Every one draws over OpenFreeMap's planet tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Basemap {
    /// A quiet slate backdrop for route lines, near-black water. Liberty
    /// recoloured (`examples/styles-open/open-swat-routes-dark-style.json`).
    SwatDark,
    /// Light and warm streets: teal water, orange motorways, yellow trunks.
    /// Liberty recoloured (`examples/styles-open/open-streets-style.json`).
    StreetsLight,
    /// OpenFreeMap Positron: pale and minimal, no POIs.
    Positron,
    /// OpenFreeMap Liberty: the full-colour general-purpose map.
    Liberty,
    /// OpenFreeMap Dark (Dark Matter).
    Dark,
    /// Neutral night monochrome, quiet grey admin lines, no POIs or road
    /// names. Liberty recoloured (`examples/styles-open/open-dark-style.json`).
    OpenDark,
}

impl Basemap {
    /// Every basemap, in menu order.
    pub const ALL: [Basemap; 6] =
        [Basemap::StreetsLight, Basemap::Liberty, Basemap::Positron, Basemap::Dark, Basemap::OpenDark, Basemap::SwatDark];

    /// The user-facing name.
    pub const fn name(self) -> &'static str {
        match self {
            Basemap::SwatDark => "Graphite",
            Basemap::StreetsLight => "Streets Light",
            Basemap::Positron => "Positron",
            Basemap::Liberty => "Liberty",
            Basemap::Dark => "Dark",
            Basemap::OpenDark => "Dark Grey",
        }
    }

    /// Whether the map is dark: pick light chrome and overlay colours.
    pub const fn is_dark(self) -> bool {
        matches!(self, Basemap::SwatDark | Basemap::Dark | Basemap::OpenDark)
    }

    /// The credit owed while this basemap is drawn, as plain text. The map
    /// draws it automatically from the tile source.
    pub const fn attribution(self) -> &'static str {
        OPENFREEMAP_ATTRIBUTION
    }

    /// [`Self::attribution`] with OpenFreeMap's links, for an HTML surface.
    pub const fn attribution_html(self) -> &'static str {
        OPENFREEMAP_ATTRIBUTION_HTML
    }

    /// The repository path the embedded document was vendored from.
    pub const fn vendored_path(self) -> &'static str {
        match self {
            Basemap::SwatDark => "examples/styles-open/open-swat-routes-dark-style.json",
            Basemap::StreetsLight => "examples/styles-open/open-streets-style.json",
            Basemap::Positron => "demos/positron-style.json",
            Basemap::Liberty => "examples/liberty-style.json",
            Basemap::Dark => "demos/openfreemap-dark-style.json",
            Basemap::OpenDark => "examples/styles-open/open-dark-style.json",
        }
    }

    /// The embedded style document's bytes.
    pub const fn style_json(self) -> &'static [u8] {
        match self {
            Basemap::SwatDark => include_bytes!("../../examples/styles-open/open-swat-routes-dark-style.json"),
            Basemap::StreetsLight => include_bytes!("../../examples/styles-open/open-streets-style.json"),
            Basemap::Positron => include_bytes!("../../demos/positron-style.json"),
            Basemap::Liberty => include_bytes!("../../examples/liberty-style.json"),
            Basemap::Dark => include_bytes!("../../demos/openfreemap-dark-style.json"),
            Basemap::OpenDark => include_bytes!("../../examples/styles-open/open-dark-style.json"),
        }
    }

    /// The same style served live by OpenFreeMap, for a MapLibre GL JS
    /// map elsewhere. `None` for our recoloured ones, which nobody hosts (load
    /// [`Self::style_json`] instead).
    pub const fn hosted_url(self) -> Option<&'static str> {
        match self {
            Basemap::Liberty => Some("https://tiles.openfreemap.org/styles/liberty"),
            Basemap::Dark => Some("https://tiles.openfreemap.org/styles/dark"),
            Basemap::Positron => Some("https://tiles.openfreemap.org/styles/positron"),
            Basemap::SwatDark | Basemap::StreetsLight | Basemap::OpenDark => None,
        }
    }

    /// The embedded document, parsed.
    pub fn document(self) -> anyhow::Result<serde_json::Value> {
        serde_json::from_slice(self.style_json()).with_context(|| format!("parsing basemap {}", self.name()))
    }

    /// The embedded document, compiled.
    pub fn style(self) -> anyhow::Result<Style> {
        crate::style_spec::parse_and_compile(self.style_json()).with_context(|| format!("compiling basemap {}", self.name()))
    }
}

impl std::fmt::Display for Basemap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A [`Basemap`] ready to mount.
pub struct LoadedBasemap {
    pub basemap: Basemap,
    /// The compiled style, for `MapView::new` / `MapView::set_style`.
    pub style: Style,
    /// OpenFreeMap's planet tiles, credited.
    pub source: Arc<dyn TileSource>,
    /// The sprite sheet's base URL (keyless: pass no token to
    /// [`spawn_sprites`]). `None` only if a style had no sprite.
    pub sprite: Option<String>,
    /// The raw style document, for a caller that edits or inspects it
    /// (e.g. swapping `text-field` to `name:ja`).
    pub document: serde_json::Value,
}

/// OpenFreeMap's planet build as a tile source: OpenMapTiles schema,
/// zoom 0-14, credited with [`OPENFREEMAP_ATTRIBUTION_HTML`], on the crate's
/// tile client (project User-Agent, connect and read-idle timeouts). No disk
/// cache: add one with `HttpTileSource::with_disk_cache` if you want it.
pub fn openfreemap_planet() -> HttpTileSource {
    HttpTileSource::new(
        crate::basemap::OPENFREEMAP_PLANET_TILE_URL,
        crate::basemap::OPENFREEMAP_PLANET_MAX_ZOOM,
        OPENFREEMAP_ATTRIBUTION_HTML,
    )
}

/// `b`'s compiled style, OpenFreeMap's planet source (no disk cache), its
/// sprite URL and its document.
pub fn load(b: Basemap) -> anyhow::Result<LoadedBasemap> {
    assemble(b, openfreemap_planet())
}

/// [`load`] with the planet tiles behind `cache` when it is `Some`.
/// OpenFreeMap serves its tiles with long `max-age`s and invites caching.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_with_disk_cache(
    b: Basemap,
    cache: Option<crate::tile_source::disk_cache::DiskCache>,
) -> anyhow::Result<LoadedBasemap> {
    let source = match cache {
        Some(cache) => openfreemap_planet().with_disk_cache(cache),
        None => openfreemap_planet(),
    };
    assemble(b, source)
}

fn assemble(b: Basemap, source: HttpTileSource) -> anyhow::Result<LoadedBasemap> {
    let document = b.document()?;
    let style = b.style()?;
    let sprite = style.sprite.clone();
    Ok(LoadedBasemap { basemap: b, style, source: Arc::new(source), sprite, document })
}

/// Fetches a style's sprite sheet OFF the UI thread (on `runtime`) and
/// hands it to `map` when it lands. A failure costs icons only and is
/// logged with any credential redacted. `token` is for a `mapbox://`
/// sprite; a [`Basemap`]'s is keyless, so pass `None`.
///
/// A sheet that lands after the map's style changed is dropped
/// ([`sprites_land`]), so a late sheet never replaces the one the new style
/// asked for.
///
/// Without the `sprite_http_fetch` feature this compiles and logs that
/// icons were not fetched; the map draws without them.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_sprites(
    map: gpui::WeakEntity<crate::map_view::MapView>,
    sprite: String,
    token: Option<String>,
    runtime: tokio::runtime::Handle,
    cx: &mut gpui::App,
) {
    let fetch = runtime.spawn(async move {
        let url = crate::sprite::resolve_sprite_url(&sprite, token.as_deref())?;
        #[cfg(feature = "sprite_http_fetch")]
        return crate::sprite::fetch_sprite_atlas(&url, &crate::tile_source::http::tile_client()).await;
        #[cfg(not(feature = "sprite_http_fetch"))]
        {
            let _ = url;
            Err(anyhow::anyhow!("built without the sprite_http_fetch feature"))
        }
    });
    let epoch = map.upgrade().map(|m| m.read(cx).style_epoch());
    cx.spawn(async move |cx| match fetch.await {
        Ok(Ok(atlas)) => {
            let _ = map.update(cx, |view, cx| {
                if sprites_land(view, epoch, Arc::new(atlas)) {
                    cx.notify();
                }
            });
        }
        Ok(Err(err)) => eprintln!(
            "gpui-maps: sprite sheet not loaded, icons will not render: {}",
            crate::sprite::redact_url_credentials(&format!("{err:#}"))
        ),
        Err(_) => {}
    })
    .detach();
}

/// Hands `atlas` to `view` unless its style changed since the fetch was
/// spawned under `epoch` (`MapView::style_epoch`). `false` when dropped.
#[cfg(not(target_arch = "wasm32"))]
pub fn sprites_land(
    view: &mut crate::map_view::MapView,
    epoch: Option<u64>,
    atlas: Arc<crate::sprite::SpriteAtlas>,
) -> bool {
    if epoch.is_some_and(|e| e != view.style_epoch()) {
        return false;
    }
    view.set_sprites(atlas);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_basemap_loads_and_compiles() {
        for b in Basemap::ALL {
            let loaded = load(b).unwrap_or_else(|e| panic!("{b:?}: {e:#}"));
            assert_eq!(loaded.basemap, b);
            assert!(!loaded.style.layers.is_empty(), "{b:?} compiled to no layers");
            assert_eq!(loaded.document["version"], 8, "{b:?}");
            assert_eq!(loaded.sprite.as_deref(), loaded.document["sprite"].as_str(), "{b:?}");
        }
    }

    #[test]
    fn all_lists_each_variant_once() {
        let mut seen = std::collections::HashSet::new();
        for b in Basemap::ALL {
            assert!(seen.insert(b), "{b:?} twice");
        }
        assert_eq!(seen.len(), 6);
        let names: std::collections::HashSet<_> = Basemap::ALL.iter().map(|b| b.name()).collect();
        assert_eq!(names.len(), 6, "names must be distinct");
    }

    #[test]
    fn sprite_url_is_keyless_openfreemap() {
        for b in Basemap::ALL {
            let sprite = load(b).unwrap().sprite.unwrap_or_else(|| panic!("{b:?} has no sprite"));
            assert!(sprite.starts_with("https://tiles.openfreemap.org/sprites/"), "{b:?}: {sprite}");
            assert!(!sprite.contains('?') && !sprite.contains("token") && !sprite.contains('@'), "{b:?}: {sprite}");
            // Resolves with no token at all.
            let url = crate::sprite::resolve_sprite_url(&sprite, None).unwrap();
            assert_eq!(url, sprite);
        }
    }

    #[test]
    fn attribution_is_openfreemaps_and_is_what_the_source_draws() {
        assert_eq!(OPENFREEMAP_ATTRIBUTION, "OpenFreeMap © OpenMapTiles Data from OpenStreetMap");
        assert_eq!(crate::tile_source::attribution_text(OPENFREEMAP_ATTRIBUTION_HTML), OPENFREEMAP_ATTRIBUTION);
        for b in Basemap::ALL {
            assert_eq!(b.attribution(), OPENFREEMAP_ATTRIBUTION);
            assert_eq!(load(b).unwrap().source.attribution(), Some(OPENFREEMAP_ATTRIBUTION), "{b:?}");
        }
    }

    #[test]
    fn no_mapbox_url_or_token_in_any_embedded_style() {
        for b in Basemap::ALL {
            let text = std::str::from_utf8(b.style_json()).unwrap().to_ascii_lowercase();
            for needle in ["mapbox://", "mapbox.com", "access_token", "pk.ey", "sk.ey", "key=", "token="] {
                assert!(!text.contains(needle), "{b:?} ({}) contains {needle:?}", b.vendored_path());
            }
            // Every URL the renderer fetches is OpenFreeMap's.
            let doc = b.document().unwrap();
            let mut urls: Vec<&str> = vec![doc["sprite"].as_str().unwrap(), doc["glyphs"].as_str().unwrap()];
            for source in doc["sources"].as_object().unwrap().values() {
                urls.extend(source["url"].as_str());
                urls.extend(source["tiles"].as_array().into_iter().flatten().filter_map(|t| t.as_str()));
            }
            assert!(urls.len() >= 4, "{b:?}: {urls:?}");
            for url in urls {
                assert!(url.starts_with("https://tiles.openfreemap.org/"), "{b:?}: {url}");
            }
        }
    }

    #[test]
    fn source_is_the_openfreemap_planet() {
        let b = load(Basemap::Liberty).unwrap();
        assert_eq!(b.source.max_zoom(), crate::basemap::OPENFREEMAP_PLANET_MAX_ZOOM);
        let doc = Basemap::Liberty.document().unwrap();
        assert_eq!(doc["sources"]["openmaptiles"]["url"], "https://tiles.openfreemap.org/planet");
    }

    #[test]
    fn dark_flags() {
        let dark: Vec<_> = Basemap::ALL.into_iter().filter(|b| b.is_dark()).collect();
        assert_eq!(dark, [Basemap::Dark, Basemap::OpenDark, Basemap::SwatDark]);
    }

    #[test]
    fn embedded_bytes_are_the_vendored_files() {
        for b in Basemap::ALL {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(b.vendored_path());
            assert_eq!(std::fs::read(&path).unwrap(), b.style_json(), "{b:?}");
        }
    }
}
