//! A PMTiles archive over HTTP range requests, for the fetcher's workers:
//! the same `pmtiles` crate reader the desktop's `tile_source::pmtiles`
//! drives (`AsyncPmTilesReader`: header, root and leaf directories, the
//! tile's run, its decompression), on a `ureq` backend instead of
//! `reqwest`'s, run to completion on the worker's own thread with
//! `pollster` (each read is one blocking range GET, so nothing awaits
//! anything else).
//!
//! A style whose source is an archive names its tiles
//! `pmtiles+<archive URL>/{z}/{x}/{y}` (`styles::style_source`); the
//! fetcher hands such a URL here ([`fetch_tile`]) instead of GETting it.
//! One reader per archive is opened on first use and kept, with its
//! directory cache, for the life of the process.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use pmtiles::{AsyncBackend, AsyncPmTilesReader, BackendResponse, HashMapCache, PmtError, PmtResult, TileCoord};

/// The scheme prefix a style source's tile template carries.
pub const PREFIX: &str = "pmtiles+";

/// Where an archive's bytes come from: a URL by range requests, or a
/// local file (`file://<path>`, or an absolute path) by seek and read --
/// an archive the app built or downloaded into its own container.
enum UreqBackend {
    Http { agent: ureq::Agent, url: String },
    File(std::path::PathBuf),
}

/// The local file an archive name points at, if it is one.
fn local_path(archive: &str) -> Option<std::path::PathBuf> {
    archive.strip_prefix("file://").or_else(|| archive.starts_with('/').then_some(archive)).map(std::path::PathBuf::from)
}

impl UreqBackend {
    fn get(&self, offset: usize, length: usize) -> PmtResult<BackendResponse> {
        let (agent, url) = match self {
            Self::Http { agent, url } => (agent, url),
            Self::File(path) => {
                use std::io::{Read as _, Seek as _};
                let mut file = std::fs::File::open(path).map_err(PmtError::Reading)?;
                file.seek(std::io::SeekFrom::Start(offset as u64)).map_err(PmtError::Reading)?;
                let mut bytes = Vec::with_capacity(length);
                file.take(length as u64).read_to_end(&mut bytes).map_err(PmtError::Reading)?;
                return Ok(BackendResponse { bytes: Bytes::from(bytes), data_version_string: None });
            }
        };
        let end = offset + length.max(1) - 1;
        let mut response = agent
            .get(url)
            .header("Range", &format!("bytes={offset}-{end}"))
            // A byte range of the file as stored: never of a compressed
            // representation (a CDN gzipping a 206 hands back a slice of a
            // deflate stream no decoder can finish).
            .header("Accept-Encoding", "identity")
            .call()
            .map_err(|e| PmtError::Reading(std::io::Error::other(e.to_string())))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(PmtError::Reading(std::io::Error::other(format!("HTTP {status}"))));
        }
        let bytes = response.body_mut().with_config().limit(64 << 20).read_to_vec().map_err(|e| PmtError::Reading(std::io::Error::other(e.to_string())))?;
        // A server that ignores `Range` answers the whole file: keep the
        // window asked for.
        let bytes = if status == 200 && bytes.len() > length { bytes[offset.min(bytes.len())..(offset + length).min(bytes.len())].to_vec() } else { bytes };
        Ok(BackendResponse { bytes: Bytes::from(bytes), data_version_string: None })
    }
}

impl AsyncBackend for UreqBackend {
    fn read(&self, offset: usize, length: usize) -> impl Future<Output = PmtResult<BackendResponse>> + Send {
        let result = self.get(offset, length);
        async move { result }
    }
}

type Reader = AsyncPmTilesReader<UreqBackend, HashMapCache>;

fn readers() -> &'static Mutex<HashMap<String, Arc<Reader>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<Reader>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn reader(agent: &ureq::Agent, archive: &str) -> Result<Arc<Reader>, String> {
    if let Some(r) = readers().lock().map_err(|e| e.to_string())?.get(archive) {
        return Ok(r.clone());
    }
    let backend = match local_path(archive) {
        Some(path) => UreqBackend::File(path),
        None => UreqBackend::Http { agent: agent.clone(), url: archive.to_string() },
    };
    let r = Arc::new(pollster::block_on(AsyncPmTilesReader::try_from_cached_source(backend, HashMapCache::default())).map_err(|e| format!("opening the archive: {e}"))?);
    readers().lock().map_err(|e| e.to_string())?.entry(archive.to_string()).or_insert(r.clone());
    Ok(r)
}

/// The archive's highest zoom, once it has been opened.
pub fn max_zoom(archive: &str) -> Option<u8> {
    readers().lock().ok()?.get(archive).map(|r| r.get_header().max_zoom)
}

/// A local archive's bounds from its header, opening it (once per
/// process). Blocking, but a local header is one small read; a remote
/// archive is refused rather than block the caller on the network.
pub fn bounds(archive: &str) -> Result<crate::geo::LonLatBounds, String> {
    if local_path(archive).is_none() {
        return Err("bounds of a remote archive".into());
    }
    let r = reader(&ureq::Agent::new_with_defaults(), archive)?;
    let h = r.get_header();
    Ok(crate::geo::LonLatBounds { min_lon: h.min_longitude, min_lat: h.min_latitude, max_lon: h.max_longitude, max_lat: h.max_latitude })
}

/// Opens `archive` (once per process) and answers its `(min_zoom,
/// max_zoom)` from the header. Blocking: a worker's call.
pub fn open_zooms(agent: &ureq::Agent, archive: &str) -> Result<(u8, u8), String> {
    let r = reader(agent, archive)?;
    let h = r.get_header();
    Ok((h.min_zoom, h.max_zoom))
}

/// `pmtiles+<archive>/{z}/{x}/{y}` filled: the archive and the tile.
pub fn parse(url: &str) -> Option<(&str, u8, u32, u32)> {
    let rest = url.strip_prefix(PREFIX)?;
    let mut parts = rest.rsplitn(4, '/');
    let y = parts.next()?.parse().ok()?;
    let x = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    Some((parts.next()?, z, x, y))
}

/// The tile's bytes, decompressed (`Ok(None)`: the archive has no such
/// tile).
pub fn fetch_tile(agent: &ureq::Agent, url: &str) -> Result<Option<Vec<u8>>, String> {
    let (archive, z, x, y) = parse(url).ok_or_else(|| "not a pmtiles+ tile URL".to_string())?;
    let r = reader(agent, archive)?;
    let coord = TileCoord::new(z, x, y).map_err(|e| e.to_string())?;
    pollster::block_on(r.get_tile_decompressed(coord)).map(|b| b.map(|b| b.to_vec())).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tile_url_names_its_archive_and_tile() {
        let (a, z, x, y) = super::parse("pmtiles+https://example.org/a(b)c.pmtiles/14/8712/5947").unwrap();
        assert_eq!((a, z, x, y), ("https://example.org/a(b)c.pmtiles", 14, 8712, 5947));
        assert!(super::parse("https://example.org/14/1/1").is_none());
    }
}
