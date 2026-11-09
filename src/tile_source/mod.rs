pub mod cache;
/// Cloud-Optimised GeoTIFF: a raster source read by range requests.
pub mod cog;
/// Native only: the browser build has the browser's HTTP cache.
#[cfg(not(target_arch = "wasm32"))]
pub mod disk_cache;
pub mod http;
/// MBTiles (SQLite) files. Native only, behind the default `mbtiles`
/// feature: the module needs a file system and links SQLite.
#[cfg(all(feature = "mbtiles", not(target_arch = "wasm32")))]
pub mod mbtiles;
pub mod pmtiles;
/// OGC WMS as a raster source: a `GetMap` per tile.
pub mod wms;

pub use cache::TileCache;

use crate::geo::TileId;

/// The ceiling a source reports when it has none of its own -- see
/// [`TileSource::max_zoom`]. Effectively unclamped: 22 is deeper than any
/// tileset in existence, so a source that returns it is saying "ask me for
/// whatever you like".
///
/// Named rather than written twice because a source that cannot read its own
/// ceiling has to fall back to exactly this value, and "exactly this value"
/// is a claim a constant can keep and a literal cannot.
pub const DEFAULT_MAX_ZOOM: u8 = 22;

#[async_trait::async_trait]
pub trait TileSource: Send + Sync {
    /// The bytes for `tile`, or `Ok(None)` when this source has no data
    /// there and that is not a problem.
    ///
    /// Absence and failure are different statements about the world, and a
    /// map has to act on them differently, so the return type has to be
    /// able to make both. An offline `PmtilesSource` is the case that
    /// forces it: pan past the edge of a downloaded region and every tile
    /// the camera asks for is one the archive simply does not contain.
    /// That is the normal, expected state of an offline map at its border
    /// -- nothing to report, nothing to retry, nothing to draw. A
    /// timed-out request for that same tile is the opposite: worth a line
    /// on stderr, worth asking again in a moment, and no evidence at all
    /// that the area is empty.
    ///
    /// `Ok(None)` says the first, `Err` says the second. [`MapView`] acts
    /// on the difference: an absent tile is recorded once, never logged,
    /// and never re-requested; a failed one goes into the retry backoff.
    ///
    /// [`MapView`]: crate::map_view::MapView
    ///
    /// # Why `Option` in the success position
    ///
    /// Two alternatives were real candidates, and both were worse:
    ///
    /// - **An error variant the map recognises** -- a `TileError::Absent`,
    ///   or a marker type attached to the `anyhow::Error` and found by
    ///   `downcast_ref`. Its appeal is that the signature does not change.
    ///   But it files absence inside the type whose whole meaning is "this
    ///   did not work", so recognising it stays optional: an implementor
    ///   who does not know about the marker, or a caller who forgets to
    ///   check for it, silently gets the old log-and-retry-forever
    ///   behaviour back, and nothing in the compiler says so.
    /// - **A separate `has_tile(&self, tile)` query.** Two round trips for
    ///   one answer and a race between them, and over HTTP a whole second
    ///   request (or a `HEAD`, which plenty of tile servers answer
    ///   differently from `GET`). It would also need a default, and the
    ///   only possible default is "yes, probably" -- so, again, an
    ///   implementor who did not override it would flood exactly as
    ///   before.
    ///
    /// `Option` makes the distinction total rather than optional: the
    /// compiler asks every implementor which of the two they mean, and
    /// every call site must handle both. The cost is a breaking change to
    /// a public trait, which is why it lands in a minor release.
    async fn fetch(&self, tile: TileId) -> anyhow::Result<Option<Vec<u8>>>;

    /// The highest zoom level this source has real tile data for. Callers
    /// (MapView's tile-selection/prefetch logic) clamp their target zoom to
    /// this value instead of requesting tiles that don't exist above it --
    /// mirrors MapLibre GL JS's real behavior (`coveringTiles()`'s
    /// `maxzoom` clamp in `src/tile/tile_manager.ts`, verified against
    /// their source), which never even attempts a fetch above a source's
    /// real ceiling. Defaults to [`DEFAULT_MAX_ZOOM`] (effectively
    /// unclamped) for sources that don't have a meaningful cap of their own.
    fn max_zoom(&self) -> u8 {
        DEFAULT_MAX_ZOOM
    }

    /// One RASTER tile as pixels, `tile_size` px on an edge: what the
    /// imagery path draws. The default fetches [`Self::fetch`]'s bytes and
    /// decodes them as PNG/JPEG/WebP on the blocking pool
    /// ([`crate::raster::decode_tile_at`], which refuses a tile of another
    /// size). A source that MAKES its pixels -- [`cog::CogSource`]
    /// resamples a GeoTIFF -- overrides it and never encodes an image
    /// only to have it decoded again.
    async fn fetch_rgba(&self, tile: TileId, tile_size: u32) -> anyhow::Result<Option<image::RgbaImage>> {
        let Some(bytes) = self.fetch(tile).await? else {
            return Ok(None);
        };
        // Counted on the worker, where the on-the-wire size is in hand,
        // through the counter the vector path uses: it answers "how much
        // did this map pull down", imagery included.
        #[cfg(feature = "probe")]
        crate::render::note_tile_fetch_bytes(bytes.len());
        tokio::task::spawn_blocking(move || crate::raster::decode_tile_at(&bytes, tile_size, tile).map(Some))
            .await
            .map_err(|join_err| anyhow::anyhow!("raster tile decode task panicked: {join_err}"))?
    }

    /// How this source's data must be credited on screen, already reduced to
    /// display text by [`attribution_text`].
    ///
    /// This is a licence obligation, not decoration. OpenStreetMap-derived
    /// vector tiles are ODbL: a map drawn from them has to credit
    /// OpenStreetMap's contributors. It lives on the source rather than on
    /// `MapView` because the source is what knows where the bytes came
    /// from -- a caller who swaps `HttpTileSource` for `PmtilesSource`
    /// swaps the credit with it, and cannot leave the old one showing.
    ///
    /// `None` means this source states no attribution (`EmptyTileSource`,
    /// which fetches nothing; an archive whose metadata omits it; a caller
    /// serving their own data). Nothing is drawn in that case -- an engine
    /// that invented a credit would be worse than one that shows none.
    fn attribution(&self) -> Option<&str> {
        None
    }

    /// The lowest zoom at which `source_layer` holds data, from the
    /// source's TileJSON `vector_layers[].minzoom`: a PMTiles archive reads
    /// its own metadata; an `HttpTileSource` knows it only when the caller
    /// opts in with `with_tilejson` (nothing fetches TileJSON by itself).
    /// `None` when the source does not say, and the floor takes 13. Read by the building floor
    /// (`cover_plan::CoverZoom::building_floor`): the cover keeps the near
    /// ground at or above the building layer's own minimum zoom.
    fn layer_min_zoom(&self, _source_layer: &str) -> Option<u8> {
        None
    }
}

/// TileJSON `vector_layers` as `id -> minzoom`, for
/// [`TileSource::layer_min_zoom`]. Entries without an integer `minzoom` in
/// `0..=255` are left out.
#[must_use]
pub fn vector_layer_min_zooms(tilejson: &serde_json::Value) -> std::collections::HashMap<String, u8> {
    tilejson
        .get("vector_layers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|layer| {
            let id = layer.get("id")?.as_str()?;
            let min = u8::try_from(layer.get("minzoom")?.as_u64()?).ok()?;
            Some((id.to_string(), min))
        })
        .collect()
}

/// TileJSON's `attribution` as text to draw.
///
/// The field is HTML by specification (TileJSON 3.0.0 §3.3) and every real
/// source uses that licence: OpenFreeMap's own metadata answers with three
/// `<a>` elements and a `&copy;`. `MapView` draws the result with gpui text,
/// which has no markup, so the tags come out and the entities go in.
///
/// Links are therefore dropped, not followed -- the credit reads correctly
/// but is not clickable. That is a deliberate first step (see the module
/// docs on the follow-up), and it is why this returns text rather than
/// anything structured.
pub fn attribution_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        // A `<` with no `>` after it is not a tag -- it is prose that
        // happens to contain the character. Dropping the remainder of the
        // string in that case would silently swallow a whole credit.
        let Some(close) = rest[open..].find('>') else { break };
        out.push_str(&rest[..open]);
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);

    // `&amp;` last: decoding it first would turn the literal text `&amp;lt;`
    // into `<`, inventing markup the source never wrote.
    for (entity, replacement) in [
        ("&copy;", "\u{a9}"),
        ("&nbsp;", " "),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&amp;", "&"),
    ] {
        if out.contains(entity) {
            out = out.replace(entity, replacement);
        }
    }

    // Removing an element leaves the whitespace that surrounded it on both
    // sides, so `</a> <a>` collapses to one gap rather than three.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A `TileSource` that never produces tiles: for a map whose layers bring
/// their own data and need no basemap tiles at all.
pub struct EmptyTileSource;

#[async_trait::async_trait]
impl TileSource for EmptyTileSource {
    /// Absence for every tile, which is the literal truth about a source
    /// that holds none -- not an error.
    ///
    /// This used to `bail!`, on the reasoning that reaching it at all was
    /// a misuse worth a diagnostic. It never was one in practice. What the
    /// old shape did
    /// do was force a source with genuinely no data to lie about why, and
    /// this trait now has a way to say it plainly.
    async fn fetch(&self, _tile: TileId) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The string OpenFreeMap's own TileJSON answers with, verbatim --
    /// fetched from `https://tiles.openfreemap.org/planet`, which is the
    /// source `examples/basic_map.rs` draws from.
    const OPENFREEMAP: &str = concat!(
        r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> "#,
        r#"<a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> "#,
        r#"Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#,
    );

    #[test]
    fn vector_layer_min_zooms_reads_tilejson() {
        let tj = serde_json::json!({"vector_layers": [
            {"id": "building", "minzoom": 14, "maxzoom": 14},
            {"id": "poi", "minzoom": 12},
            {"id": "water"},
            {"id": "bad", "minzoom": -1}
        ]});
        let m = vector_layer_min_zooms(&tj);
        assert_eq!(m.get("building"), Some(&14));
        assert_eq!(m.get("poi"), Some(&12));
        assert_eq!(m.len(), 2);
        assert!(vector_layer_min_zooms(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn a_real_tilejson_attribution_reduces_to_the_credit_it_states() {
        assert_eq!(
            attribution_text(OPENFREEMAP),
            "OpenFreeMap \u{a9} OpenMapTiles Data from OpenStreetMap"
        );
    }

    #[test]
    fn text_with_no_markup_survives_unchanged() {
        assert_eq!(attribution_text("\u{a9} OpenStreetMap contributors"), "\u{a9} OpenStreetMap contributors");
    }

    /// `&amp;` is decoded after every other entity. Decoding it first would
    /// read this input as `<b>`/`</b>` and strip the whole middle.
    #[test]
    fn an_escaped_ampersand_does_not_become_markup() {
        assert_eq!(attribution_text("Tiles &amp;lt;b&amp;gt; Co"), "Tiles &lt;b&gt; Co");
    }

    /// A `<` that opens no tag is prose. An earlier shape of this function
    /// dropped everything after it, which would delete a credit rather than
    /// tidy it.
    #[test]
    fn a_less_than_sign_with_no_closing_bracket_keeps_the_rest_of_the_credit() {
        assert_eq!(attribution_text("Accurate to < 1 km, \u{a9} OpenStreetMap"), "Accurate to < 1 km, \u{a9} OpenStreetMap");
    }

    #[test]
    fn an_empty_attribution_stays_empty() {
        assert_eq!(attribution_text(""), "");
    }

    /// The default is silence. A source that states no credit gets none
    /// invented for it.
    #[test]
    fn a_source_that_states_no_attribution_reports_none() {
        assert_eq!(EmptyTileSource.attribution(), None);
    }

    /// A source that holds no tiles says so, rather than reporting the
    /// absence of data it never claimed to have as a failure to fetch it.
    #[tokio::test]
    async fn a_source_with_no_tiles_reports_absence_rather_than_failure() {
        let result = EmptyTileSource.fetch(TileId { z: 0, x: 0, y: 0 }).await;
        assert!(matches!(result, Ok(None)), "got {result:?}");
    }
}
