//! Which basemap a shell starts on -- the tile source and the style, chosen
//! TOGETHER -- and the one derivation of the Mapbox `composite` tile URL.
//!
//! # Why the two halves are one decision
//!
//! A style whose `source-layer` names come from one schema compiles cleanly
//! against tiles of the other and then draws almost nothing:
//! `style::match_tile` matches by name, so `examples/mapbox-streets-style.json`
//! (Streets v8: `landcover`, `road`, `place_label`) over OpenFreeMap's
//! OpenMapTiles tiles (`landcover`, `transportation`, `place`) keeps the
//! background and a handful of fills. That is what the browser build looked
//! like beside the native Mapbox demo: flat grey land, no greenery, no
//! labels. So a shell never picks a style and a tile URL separately; it asks
//! this module for the pair.
//!
//! # Where each shell's token comes from
//!
//! * native (`examples/mapbox_streets_demo.rs`, `examples/globe_capture.rs`):
//!   `MAPBOX_ACCESS_TOKEN` at run time;
//! * iOS (`ios/src/app.rs`) and the browser (`web/src/app.rs`):
//!   `option_env!("MAPBOX_ACCESS_TOKEN")`, baked in at BUILD time by
//!   `ios/run-device.sh` / `web/build.sh` when the variable is exported,
//!   never read from a tracked file. The browser also takes a run-time
//!   `?access_token=` for local testing.
//!
//! All of them turn the token into a tile URL through
//! [`mapbox_composite_tile_url`], and sprites through
//! `sprite::resolve_sprite_url`, so no shell can build either differently.
//! Every string that may carry the resulting URL goes through
//! `sprite::redact_access_token` before it is logged or shown.
//!
//! gpui-free, clock-free and browser-free, so the browser shell -- which
//! cannot run tests (see `web/src/lib.rs`) -- shares tested logic.

/// The public, keyless vector tile source every shell without a Mapbox
/// token opens on: OpenFreeMap's planet build, OpenMapTiles schema,
/// zoom 0-14, built from OpenStreetMap data (ODbL).
///
/// One constant rather than a literal repeated in each shell, so a browser
/// build, an iOS build and `examples/basic_map.rs` cannot drift onto three
/// different dated paths. This exact path was verified live (real MVT bytes
/// back for a Singapore z12/z13 tile) and cross-checked against the current
/// TileJSON at <https://tiles.openfreemap.org/planet> -- if it 404s later,
/// re-fetch that TileJSON and read its `tiles` array for the current path,
/// the same way this one was found.
pub const OPENFREEMAP_PLANET_TILE_URL: &str =
    "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf";

/// [`OPENFREEMAP_PLANET_TILE_URL`]'s own real-data ceiling, per its TileJSON
/// `maxzoom` field. See [`crate::source_zoom::OPENMAPTILES_MAX_ZOOM`], which
/// is this same number kept where the zoom-ceiling logic lives.
pub const OPENFREEMAP_PLANET_MAX_ZOOM: u8 = crate::source_zoom::OPENMAPTILES_MAX_ZOOM;

/// The two schemas a shell's tile source can speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileSchema {
    /// OpenMapTiles vocabulary: OpenFreeMap, any other OMT-schema tile
    /// source a `?tiles=` override names, `examples/mapbox-style.json` and
    /// `examples/liberty-style.json`.
    Omt,
    /// Mapbox's `composite` source (streets-v8 + terrain-v2 +
    /// bathymetry-v2), Streets v8 vocabulary:
    /// `examples/mapbox-streets-style.json`.
    MapboxComposite,
}

/// A token worth using: trimmed, and `None` when absent or blank. An empty
/// `MAPBOX_ACCESS_TOKEN=` exported by a script is "no token", not a token
/// that makes every request 401.
pub fn usable_token(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|t| !t.is_empty())
}

/// Mapbox's raw v4 tile endpoint for a style's `sources.composite`, as a
/// `{z}/{x}/{y}` template with `token` as the query parameter -- exactly
/// what `mapbox://a,b,c` resolves to server-side, and the URL
/// `examples/mapbox_streets_demo.rs` fetches natively.
///
/// The tileset ids are read out of the style document itself rather than
/// written a second time here, so a re-vendored style cannot drift from
/// its tiles. The returned string EMBEDS the token: never log it except
/// through `sprite::redact_access_token`. Errors never contain the token.
pub fn mapbox_composite_tile_url(style_json: &[u8], token: &str) -> anyhow::Result<String> {
    let tileset_ids = composite_tileset_ids(style_json)?;
    Ok(format!("https://api.mapbox.com/v4/{tileset_ids}/{{z}}/{{x}}/{{y}}.mvt?access_token={token}"))
}

/// `sources.composite.url` minus its `mapbox://` scheme: the comma-joined
/// tileset id list.
fn composite_tileset_ids(style_json: &[u8]) -> anyhow::Result<String> {
    let json: serde_json::Value = serde_json::from_slice(style_json)
        .map_err(|e| anyhow::anyhow!("the style failed to parse as JSON: {e}"))?;
    let url = json["sources"]["composite"]["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the style has no sources.composite.url"))?;
    url.strip_prefix("mapbox://")
        .filter(|ids| !ids.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("sources.composite.url {url:?} is not a mapbox:// URL"))
}

/// Which schema a style document is written against: `MapboxComposite`
/// when it declares a `mapbox://` vector source named `composite`, `Omt`
/// otherwise (including a document that does not parse -- the caller's own
/// compile reports that, and OMT is the schema every other style here is).
pub fn style_schema(style_json: &[u8]) -> TileSchema {
    if composite_tileset_ids(style_json).is_ok() {
        TileSchema::MapboxComposite
    } else {
        TileSchema::Omt
    }
}

/// Which schema a `{z}/{x}/{y}` tile template speaks, judged by host the
/// same way `source_zoom::max_zoom_for_tile_url` judges its ceiling (so the
/// two cannot disagree): Mapbox's tile API is `MapboxComposite`, anything
/// else -- OpenFreeMap, the local corpus, an unrecognised override -- `Omt`.
pub fn tile_url_schema(template: &str) -> TileSchema {
    if crate::source_zoom::max_zoom_for_tile_url(template) == crate::source_zoom::MAPBOX_COMPOSITE_MAX_ZOOM {
        TileSchema::MapboxComposite
    } else {
        TileSchema::Omt
    }
}

/// The style a shell with a style picker opens on: an explicit `?style=`,
/// else Mapbox Streets (`mapbox_style`) when a usable token exists -- the
/// startup choice iOS makes -- else `omt_style`, the tokenless public
/// build's behaviour unchanged.
///
/// A `?tiles=` does NOT keep a token-bearing page off Mapbox Streets: it
/// is the source for its own schema's styles (see
/// [`tile_template_for_style`]), so a URL that has always carried
/// `?tiles=<OpenFreeMap>` opens on the same map as native and still draws
/// its OpenFreeMap tiles under the picker's OMT styles.
pub fn startup_style_url(token: Option<&str>, query_style: Option<&str>, omt_style: &str, mapbox_style: &str) -> String {
    match query_style {
        Some(style) => style.to_string(),
        None if usable_token(token).is_some() => mapbox_style.to_string(),
        None => omt_style.to_string(),
    }
}

/// The tile template to pair with the style whose bytes are `style_json`,
/// and the schema of that template.
///
/// * A `?tiles=` speaking the style's own schema is used verbatim.
/// * Without a token, a `?tiles=` is used verbatim whatever it speaks --
///   nothing could be paired better, and that is what the page always did.
/// * Otherwise an OMT style gets `omt_tiles`, and a Mapbox-schema style
///   the composite URL built from the token.
///
/// `Err` only for a Mapbox-schema style with neither a token nor a
/// `?tiles=`, so the caller can say why nothing matches. Errors name
/// neither the token nor a URL.
pub fn tile_template_for_style(
    style_json: &[u8],
    token: Option<&str>,
    query_tiles: Option<&str>,
    omt_tiles: &str,
) -> anyhow::Result<(TileSchema, String)> {
    let schema = style_schema(style_json);
    let token = usable_token(token);
    if let Some(tiles) = query_tiles {
        if tile_url_schema(tiles) == schema || token.is_none() {
            return Ok((tile_url_schema(tiles), tiles.to_string()));
        }
    }
    match (schema, token) {
        (TileSchema::Omt, _) => Ok((TileSchema::Omt, omt_tiles.to_string())),
        (TileSchema::MapboxComposite, Some(token)) => {
            Ok((TileSchema::MapboxComposite, mapbox_composite_tile_url(style_json, token)?))
        }
        (TileSchema::MapboxComposite, None) => {
            anyhow::bail!("a Mapbox-schema style needs a Mapbox access token for its tiles")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sprite::redact_access_token;

    /// Fake, and shaped like nothing real: a test must never carry a token.
    const TOKEN: &str = "pk.test-token-not-real";
    const OMT_TILES: &str = "http://127.0.0.1:8080/tiles/{z}/{x}/{y}.mvt";
    const OFM_TILES: &str = "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf";
    const OMT_STYLE: &str = "./mapbox-style.json";
    const MAPBOX_STYLE: &str = "./mapbox-streets-style.json";

    /// The `sources` block of Mapbox Streets v12 -- all these functions
    /// read -- without its Mapbox-authored layers, which are not in the
    /// public tree (see `src/mapbox_fixtures.rs`).
    fn streets() -> Vec<u8> {
        br#"{"version":8,"sources":{"composite":{"type":"vector",
            "url":"mapbox://mapbox.mapbox-streets-v8,mapbox.mapbox-terrain-v2,mapbox.mapbox-bathymetry-v2"}},
            "layers":[]}"#
            .to_vec()
    }
    /// An OMT-schema style whose source is a plain TileJSON URL, as the
    /// keyless styles declare it.
    fn omt_imitation() -> Vec<u8> {
        br#"{"version":8,"sources":{"openmaptiles":{"type":"vector",
            "url":"https://tiles.openfreemap.org/planet"}},"layers":[]}"#
            .to_vec()
    }
    fn liberty() -> Vec<u8> {
        include_bytes!("../examples/liberty-style.json").to_vec()
    }

    #[test]
    fn a_blank_token_is_no_token() {
        assert_eq!(usable_token(None), None);
        assert_eq!(usable_token(Some("")), None);
        assert_eq!(usable_token(Some("  \n")), None);
        assert_eq!(usable_token(Some(" pk.x ")), Some("pk.x"));
    }

    #[test]
    fn the_composite_url_carries_all_three_tilesets_and_the_token() {
        let url = mapbox_composite_tile_url(&streets(), TOKEN).unwrap();
        assert_eq!(
            url,
            "https://api.mapbox.com/v4/mapbox.mapbox-streets-v8,mapbox.mapbox-terrain-v2,\
             mapbox.mapbox-bathymetry-v2/{z}/{x}/{y}.mvt?access_token=pk.test-token-not-real"
        );
        assert_eq!(crate::source_zoom::max_zoom_for_tile_url(&url), 16);
    }

    #[test]
    fn a_style_without_a_composite_source_derives_no_url_and_names_no_token() {
        let err = mapbox_composite_tile_url(&omt_imitation(), TOKEN).unwrap_err().to_string();
        assert!(!err.contains(TOKEN), "error leaked the token: {err}");
    }

    #[test]
    fn styles_are_classified_by_their_sources() {
        assert_eq!(style_schema(&streets()), TileSchema::MapboxComposite);
        assert_eq!(style_schema(&omt_imitation()), TileSchema::Omt);
        assert_eq!(style_schema(&liberty()), TileSchema::Omt);
        assert_eq!(style_schema(b"not json"), TileSchema::Omt);
    }

    #[test]
    fn with_a_token_the_page_opens_on_mapbox_streets_over_the_composite() {
        let style = startup_style_url(Some(TOKEN), None, OMT_STYLE, MAPBOX_STYLE);
        assert_eq!(style, MAPBOX_STYLE);
        let (schema, tiles) = tile_template_for_style(&streets(), Some(TOKEN), None, OMT_TILES).unwrap();
        assert_eq!(schema, TileSchema::MapboxComposite);
        assert!(tiles.starts_with("https://api.mapbox.com/v4/mapbox.mapbox-streets-v8,"), "{tiles}");
        assert_eq!(crate::source_zoom::max_zoom_for_tile_url(&tiles), 16);
    }

    #[test]
    fn without_a_token_the_page_opens_exactly_as_before() {
        let style = startup_style_url(None, None, OMT_STYLE, MAPBOX_STYLE);
        assert_eq!(style, OMT_STYLE);
        let (schema, tiles) = tile_template_for_style(&omt_imitation(), None, None, OMT_TILES).unwrap();
        assert_eq!((schema, tiles.as_str()), (TileSchema::Omt, OMT_TILES));
        // `?tiles=` pointing at OpenFreeMap, the tunnel's usual URL, is
        // untouched too.
        let (schema, tiles) = tile_template_for_style(&omt_imitation(), None, Some(OFM_TILES), OMT_TILES).unwrap();
        assert_eq!((schema, tiles.as_str()), (TileSchema::Omt, OFM_TILES));
        assert_eq!(crate::source_zoom::max_zoom_for_tile_url(&tiles), 14);
    }

    #[test]
    fn an_explicit_style_beats_the_token() {
        assert_eq!(
            startup_style_url(Some(TOKEN), Some("./liberty-style.json"), OMT_STYLE, MAPBOX_STYLE),
            "./liberty-style.json"
        );
    }

    /// The tunnel's URL carries `?tiles=<OpenFreeMap>`. With a token that
    /// must still open on Mapbox Streets over the composite -- the same map
    /// as native -- and the `?tiles=` is what the OMT styles in the picker
    /// then draw over.
    #[test]
    fn a_tiles_override_is_the_source_for_its_own_schema_only() {
        let mapbox_v4 = "https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token=pk.other";
        assert_eq!(tile_url_schema(OFM_TILES), TileSchema::Omt);
        assert_eq!(tile_url_schema(mapbox_v4), TileSchema::MapboxComposite);

        let (schema, tiles) = tile_template_for_style(&streets(), Some(TOKEN), Some(OFM_TILES), OMT_TILES).unwrap();
        assert_eq!(schema, TileSchema::MapboxComposite);
        assert!(tiles.starts_with("https://api.mapbox.com/v4/mapbox.mapbox-streets-v8,"), "{tiles}");

        let (schema, tiles) = tile_template_for_style(&liberty(), Some(TOKEN), Some(OFM_TILES), OMT_TILES).unwrap();
        assert_eq!((schema, tiles.as_str()), (TileSchema::Omt, OFM_TILES));

        let (schema, tiles) = tile_template_for_style(&streets(), Some(TOKEN), Some(mapbox_v4), OMT_TILES).unwrap();
        assert_eq!((schema, tiles.as_str()), (TileSchema::MapboxComposite, mapbox_v4));

        let (schema, tiles) = tile_template_for_style(&liberty(), Some(TOKEN), Some(mapbox_v4), OMT_TILES).unwrap();
        assert_eq!((schema, tiles.as_str()), (TileSchema::Omt, OMT_TILES));
    }

    /// Without a token nothing is re-paired: `?tiles=` is used verbatim,
    /// whatever it is, exactly as the page always did.
    #[test]
    fn without_a_token_a_tiles_override_is_used_verbatim() {
        let mapbox_v4 = "https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.mvt?access_token=pk.other";
        let (schema, tiles) = tile_template_for_style(&omt_imitation(), None, Some(mapbox_v4), OMT_TILES).unwrap();
        assert_eq!((schema, tiles.as_str()), (TileSchema::MapboxComposite, mapbox_v4));
        let (schema, tiles) = tile_template_for_style(&streets(), None, Some(OFM_TILES), OMT_TILES).unwrap();
        assert_eq!((schema, tiles.as_str()), (TileSchema::Omt, OFM_TILES));
    }

    #[test]
    fn an_omt_style_keeps_omt_tiles_even_with_a_token() {
        let (schema, tiles) = tile_template_for_style(&liberty(), Some(TOKEN), None, OMT_TILES).unwrap();
        assert_eq!((schema, tiles.as_str()), (TileSchema::Omt, OMT_TILES));
    }

    #[test]
    fn a_mapbox_style_without_a_token_is_refused_not_mismatched() {
        let err = tile_template_for_style(&streets(), None, None, OMT_TILES).unwrap_err();
        assert!(err.to_string().contains("token"), "{err}");
    }

    #[test]
    fn the_web_url_builders_redact_to_no_token() {
        let tiles = mapbox_composite_tile_url(&streets(), TOKEN).unwrap();
        let shown = redact_access_token(&tiles);
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.ends_with("access_token=[REDACTED]"), "{shown}");
        let filled = tiles.replace("{z}", "3").replace("{x}", "1").replace("{y}", "2");
        let failure = format!("fetch {filled} returned HTTP 401");
        assert!(!redact_access_token(&failure).contains(TOKEN));

        let sprite = crate::sprite::resolve_sprite_url("mapbox://sprites/mapbox/streets-v12", Some(TOKEN)).unwrap();
        let (json, png) = crate::sprite::sprite_urls(&sprite);
        for url in [json, png] {
            assert!(url.contains(TOKEN), "the request itself must carry it: {url}");
            let shown = redact_access_token(&format!("sprites: {url} 404 -- icons will not draw"));
            assert!(!shown.contains(TOKEN), "{shown}");
        }
    }
}
