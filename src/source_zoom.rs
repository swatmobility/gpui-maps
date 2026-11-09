//! The zoom ceiling a vector tile source's REAL data goes to, as a property
//! of the SOURCE rather than a shell constant.
//!
//! # The defect this exists to fix
//!
//! `ios/src/app.rs` and `web/src/app.rs` each hardcoded their own
//! `const SOURCE_MAX_ZOOM: u8 = 14`, which is OpenFreeMap/OpenMapTiles'
//! ceiling and nothing else's. Mapbox's `mapbox.mapbox-streets-v8`
//! `composite` vector source goes two levels deeper, to z16 -- and its
//! style thins dense layers out of anything shallower, so a build pinned to
//! z14 never fetches the tile that has the `building` layer's data in it at
//! all. Measured over Midtown Manhattan (40.7505, -73.9934), `building`
//! features per tile:
//!
//! ```text
//! z13    9
//! z14   49
//! z15  977
//! z16  256   (one quarter of the z15 area -- z15 itself is the dense one)
//! ```
//!
//! `examples/mapbox-streets-style.json`'s `building` fill layer has
//! `minzoom: 15`. A shell capped at z14 draws no buildings on this style,
//! anywhere dense enough for Mapbox to have thinned them -- which is every
//! city, and is exactly the New York report this module exists to close.
//!
//! # Why this is a shared function and not two constants
//!
//! Native (`src/map_view.rs`'s `cover_tile_zoom`, `set_raster_source`) has
//! never had this defect: its `MapView::source` is `Arc<dyn
//! crate::tile_source::TileSource>`, and every zoom-selection site reads
//! `source.max_zoom()` off it directly, so a new source (or a style switch
//! that replaces it) is picked up the next time a frame asks. Every native
//! example already passes the right literal at construction --
//! `HttpTileSource::new(url, 16, ..)` for a Mapbox composite endpoint,
//! `HttpTileSource::new(url, 14, ..)` for OpenFreeMap -- because that
//! constructor takes the ceiling as an argument rather than assuming one.
//!
//! `ios/src/app.rs` and `web/src/app.rs` cannot reuse `TileSource` itself --
//! neither crate carries `tile_source` (it pulls in `reqwest` and pmtiles'
//! mmap, both absent from an iOS-spike/wasm build's dependency set; see
//! each crate's own `Cargo.toml` note on why it reaches the engine by
//! `#[path]` source inclusion instead of a path dependency). What they CAN
//! share is the deliberately dependency-free decision this file makes: the
//! two ceilings, named once, and the one piece of real logic (a URL
//! template to a ceiling) that both shells' own "which source is active"
//! bookkeeping can be run through. iOS already knows its answer
//! structurally (`TileSchema::source_max_zoom`, since it never guesses a
//! schema from a URL -- see that type); the web shell's tile source is an
//! arbitrary `?tiles=`-overridable string, so it goes through
//! [`max_zoom_for_tile_url`], the same way `attribution_for` already
//! guesses a source's credit from that same string. Either path bottoms out
//! in these two constants, so the two shells cannot answer the same
//! question two different ways.
//!
//! # Overzoom is unaffected
//!
//! A display zoom above the source's ceiling still asks for the ceiling's
//! own tiles and magnifies them -- `tile_zoom()` in both shells is
//! `display_zoom().min(source_max_zoom)`, unchanged in shape, just no
//! longer pinned to one number. Style resolution (`display_zoom()` itself)
//! stays uncapped, so a layer whose `minzoom` sits above the ceiling still
//! renders overzoomed, exactly as before.

/// OpenFreeMap's planet build and every other OpenMapTiles-schema tileset:
/// the schema's own documented ceiling, and the depth this project's other
/// OMT corpora are actually built to.
pub const OPENMAPTILES_MAX_ZOOM: u8 = 14;

/// Mapbox's `mapbox.mapbox-streets-v8` `composite` vector source. See this
/// module's own doc comment for the measurement that pins it at 16 rather
/// than OpenMapTiles' 14.
pub const MAPBOX_COMPOSITE_MAX_ZOOM: u8 = 16;

/// The ceiling a source with no more specific answer gets -- the same
/// fallback [`crate::tile_source::TileSource::max_zoom`] would give (that
/// module's `DEFAULT_MAX_ZOOM` is 22, "effectively unclamped"; this one is
/// [`OPENMAPTILES_MAX_ZOOM`], because unlike a native `TileSource` neither
/// shell's `{z}/{x}/{y}` fetch loop has an "ask anyway and see" path for a
/// zoom this module cannot place -- see [`max_zoom_for_tile_url`]).
pub const DEFAULT_MAX_ZOOM: u8 = OPENMAPTILES_MAX_ZOOM;

/// Guesses a vector tile source's real-data ceiling from its `{z}/{x}/{y}`
/// URL template, the same way [`crate::tile_source::attribution_text`]'s
/// sibling `attribution_for` (in `web/src/app.rs`) guesses a source's
/// credit: by matching the host, because neither shell fetches the
/// source's own TileJSON or style `maxzoom` at runtime to read the answer
/// off it directly. A build that DOES have that document (a fetched
/// TileJSON, a parsed style's `sources.<name>.maxzoom`) should read it from
/// there instead and only fall back to this guess when it is absent --
/// see `PmtilesSource`'s own header read (`src/tile_source/pmtiles.rs`) and
/// `HttpTileSource::new`'s callers (`examples/*.rs`), which already do that
/// for native.
///
/// Recognises Mapbox's tile API (`api.mapbox.com`, which every
/// `mapbox://` composite URL this project derives resolves to -- see
/// `ios/src/app.rs`'s `mapbox_tile_url_template`) and the `mapbox://`
/// scheme itself, for a template that has not yet been resolved to the v4
/// endpoint. Everything else -- OpenFreeMap, a self-hosted OMT tile
/// server, an unrecognised override -- gets [`OPENMAPTILES_MAX_ZOOM`],
/// which is [`DEFAULT_MAX_ZOOM`] too: an override this function cannot
/// place is far more likely to be another OMT-schema corpus (every other
/// tileset this project has ever pointed a shell at) than a deeper one, and
/// guessing shallow costs overzoom, while guessing deep would cost fetching
/// tiles a real z15/z16-less source has none of.
pub fn max_zoom_for_tile_url(template: &str) -> u8 {
    if template.contains("api.mapbox.com") || template.contains("mapbox://") {
        MAPBOX_COMPOSITE_MAX_ZOOM
    } else {
        DEFAULT_MAX_ZOOM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mapbox_v4_template_reports_the_composite_ceiling() {
        assert_eq!(
            max_zoom_for_tile_url("https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token=x"),
            MAPBOX_COMPOSITE_MAX_ZOOM
        );
    }

    #[test]
    fn an_unresolved_mapbox_scheme_url_also_reports_the_composite_ceiling() {
        assert_eq!(max_zoom_for_tile_url("mapbox://mapbox.mapbox-streets-v8"), MAPBOX_COMPOSITE_MAX_ZOOM);
    }

    #[test]
    fn openfreemap_reports_the_omt_ceiling() {
        assert_eq!(
            max_zoom_for_tile_url("https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf"),
            OPENMAPTILES_MAX_ZOOM
        );
    }

    #[test]
    fn a_local_omt_server_reports_the_omt_ceiling() {
        assert_eq!(max_zoom_for_tile_url("http://127.0.0.1:8080/tiles/{z}/{x}/{y}.mvt"), OPENMAPTILES_MAX_ZOOM);
    }

    #[test]
    fn an_unrecognised_override_falls_back_to_the_omt_ceiling_not_unclamped() {
        assert_eq!(max_zoom_for_tile_url("https://example.test/{z}/{x}/{y}.pbf"), DEFAULT_MAX_ZOOM);
    }

    /// The property the whole module exists for: two templates naming
    /// different real sources must not answer with the same ceiling, and a
    /// shell that starts pointed at one and is repointed at the other must
    /// see the number change -- there is no cached "the ceiling is 14"
    /// anywhere in this function for a build to have gone stale against.
    #[test]
    fn switching_the_active_source_changes_the_reported_ceiling() {
        let omt = max_zoom_for_tile_url("https://tiles.openfreemap.org/planet/x/{z}/{x}/{y}.pbf");
        let mapbox = max_zoom_for_tile_url("https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token=x");
        assert_ne!(omt, mapbox, "an OMT and a Mapbox composite template must not share a ceiling");
        assert_eq!(omt, 14);
        assert_eq!(mapbox, 16);
    }
}
