//! Cloud-Optimised GeoTIFFs as a raster source, for the fetcher's workers:
//! the desktop's own reader (`tile_source::cog`, by `#[path]`), on this
//! crate's `ureq` (`cog/fetch_ureq.rs`), each map tile rendered to RGBA by
//! the same code the desktop's `set_raster_source` path runs and handed to
//! the raster decoder as a PNG.
//!
//! A raster template names its tiles `cog+<source>/{z}/{x}/{y}`, where
//! `<source>` is a key a host [`register`]ed (a bundled file with its
//! ramp and hillshade, as a desktop card opens it) or else an http(s) URL,
//! opened on first use with the desktop's options for an arbitrary COG
//! (a hillshade of 1). The fetcher hands such a URL here ([`fetch_tile`])
//! instead of GETting it. A source is kept, with its decoded-tile cache,
//! for the life of the process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::cog::{CogOptions, CogSource};
use crate::geo::TileId;

/// The scheme prefix a raster template carries.
pub const PREFIX: &str = "cog+";

fn sources() -> &'static Mutex<HashMap<String, Arc<CogSource>>> {
    static S: OnceLock<Mutex<HashMap<String, Arc<CogSource>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Makes `key` name `source` in a `cog+<key>/{z}/{x}/{y}` template. A key
/// already registered keeps its first source (and its cache).
pub fn register(key: &str, source: CogSource) {
    if let Ok(mut map) = sources().lock() {
        map.entry(key.to_string()).or_insert_with(|| Arc::new(source));
    }
}

/// Whether `key` names a source already.
pub fn is_registered(key: &str) -> bool {
    sources().lock().is_ok_and(|m| m.contains_key(key))
}

/// The template that draws `key`'s source.
pub fn template(key: &str) -> String {
    format!("{PREFIX}{key}/{{z}}/{{x}}/{{y}}")
}

fn source(key: &str) -> Result<Arc<CogSource>, String> {
    if let Some(s) = sources().lock().map_err(|e| e.to_string())?.get(key) {
        return Ok(s.clone());
    }
    if !(key.starts_with("https://") || key.starts_with("http://")) {
        return Err("no such COG source".into());
    }
    let opened = Arc::new(CogSource::open_blocking(key, CogOptions::default().with_hillshade(1.0)).map_err(|e| format!("opening the COG: {e}"))?);
    Ok(sources().lock().map_err(|e| e.to_string())?.entry(key.to_string()).or_insert(opened).clone())
}

/// `cog+<source>/{z}/{x}/{y}` filled: the source and the tile.
pub fn parse(url: &str) -> Option<(&str, TileId)> {
    let rest = url.strip_prefix(PREFIX)?;
    let mut parts = rest.rsplitn(4, '/');
    let y = parts.next()?.parse().ok()?;
    let x = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    Some((parts.next()?, TileId { z, x, y }))
}

/// The tile as PNG bytes (`Ok(None)`: the image does not reach it).
pub fn fetch_tile(url: &str) -> Result<Option<Vec<u8>>, String> {
    let (key, tile) = parse(url).ok_or_else(|| "not a cog+ tile URL".to_string())?;
    let cog = source(key)?;
    let Some(image) = pollster::block_on(cog.render_tile(tile)).map_err(|e| e.to_string())? else { return Ok(None) };
    let mut out = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_url_names_its_source_and_tile() {
        let (k, t) = parse("cog+https://example.org/a/b.tif/10/163/357").unwrap();
        assert_eq!((k, t), ("https://example.org/a/b.tif", TileId { z: 10, x: 163, y: 357 }));
        assert_eq!(parse(&template("rainier").replace("{z}", "9").replace("{x}", "1").replace("{y}", "2")), Some(("rainier", TileId { z: 9, x: 1, y: 2 })));
        assert!(parse("https://example.org/1/1/1").is_none());
    }
}
