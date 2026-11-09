use std::future::Future;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Mutex;

use ::pmtiles::{
    AsyncBackend, AsyncPmTilesReader, DirEntry, Directory, DirectoryCache, HttpBackend,
    MmapBackend, PmtResult, TileCoord,
};
use lru::LruCache;

use crate::geo::TileId;
use crate::tile_source::{TileSource, DEFAULT_MAX_ZOOM};

/// Protomaps' public demo archive: Florence, PMTiles v3, ~6.6 MB, served
/// from GitHub Pages with `Accept-Ranges: bytes`.
///
/// Named here rather than only in the example so this module's own network
/// test and the example cannot drift apart. `protomaps.github.io/PMTiles/…`
/// -- the URL the `pmtiles` crate's own tests use -- 301s to this one, and a
/// redirect on every range request is a round trip nobody needs.
pub const DEMO_ARCHIVE_URL: &str = "https://pmtiles.io/protomaps(vector)ODbL_firenze.pmtiles";

/// How many PMTiles leaf directories are kept parsed in memory.
///
/// The number is chosen against the largest archive anyone will point this
/// at, not the smallest. Protomaps' planet build (123 GB, 161M tile entries)
/// splits into roughly 3,500 leaves of ~46k entries each, which parse to
/// ~1.1 MB apiece -- so this bound is worth ~18 MB there, and a few hundred
/// kilobytes for a city extract with two or three leaves in total.
///
/// It does not need to be larger. Leaf directories are cut from the entry
/// list in tile-id order, and tile ids run zoom-major then Hilbert, so the
/// tiles covering one viewport at one zoom are a contiguous run that lands
/// in one or two leaves. Sixteen holds those plus the parent zooms a pan
/// walks through.
const LEAF_DIRECTORY_CACHE_ENTRIES: usize = 16;

/// A bounded [`DirectoryCache`], because over HTTP the alternative is not
/// "slightly slower".
///
/// A PMTiles reader keeps the root directory in memory from the moment it
/// opens, but leaf directories it does not: with the crate's `NoCache` every
/// tile lookup that goes through a leaf re-reads that leaf, decompresses it
/// and re-parses its varints. Against an mmapped file that is wasted CPU.
/// Against a bucket it is a second HTTP round trip per tile, so a screenful
/// of 20 tiles costs 40 requests instead of 20 -- and the leaf being fetched
/// over and over is the same one every time, because the tiles on screen are
/// neighbours.
///
/// `pmtiles` ships two caches of its own and neither fits. `HashMapCache` is
/// unbounded, which on a planet archive means a long pan accumulates leaves
/// until the process dies. `MokaCache` is bounded but arrives with the whole
/// `moka` crate for what is, here, an LRU of sixteen entries -- and this
/// crate already depends on `lru` for [`crate::tile_source::TileCache`].
struct LeafDirectoryCache {
    /// `Mutex` rather than `RwLock`: every read is also a write (an LRU hit
    /// reorders the list), so a reader-writer lock would take the write path
    /// on every lookup anyway and only add the cost of deciding not to.
    directories: Mutex<LruCache<usize, Directory>>,
}

impl LeafDirectoryCache {
    fn new() -> Self {
        Self {
            directories: Mutex::new(LruCache::new(
                NonZeroUsize::new(LEAF_DIRECTORY_CACHE_ENTRIES).expect("bound is a literal > 0"),
            )),
        }
    }
}

impl DirectoryCache for LeafDirectoryCache {
    /// The returned future has to be `Send`, so the guard is taken and
    /// dropped inside the block below -- holding a `MutexGuard` across the
    /// `fetcher.await` would both make the future non-`Send` and serialise
    /// every concurrent tile fetch behind one network round trip.
    ///
    /// Two misses on the same leaf can therefore both fetch it. That is the
    /// deliberate trade: one duplicated request on a cold leaf, against
    /// funnelling the whole tile pipeline through a single lock.
    async fn get_dir_entry_or_insert(
        &self,
        offset: usize,
        tile_id: ::pmtiles::TileId,
        fetcher: impl Future<Output = PmtResult<Directory>> + Send,
    ) -> PmtResult<Option<DirEntry>> {
        // The outer `Option` is "was this leaf cached", the inner one is
        // "does that leaf hold this tile". Collapsing them would turn a
        // cached leaf that legitimately has no entry for this tile into a
        // cache miss, and re-fetch the leaf to be told the same thing.
        let cached = {
            let mut directories = self.directories.lock().expect("leaf cache mutex poisoned");
            directories
                .get(&offset)
                .map(|directory| directory.find_tile_id(tile_id).cloned())
        };
        if let Some(entry) = cached {
            return Ok(entry);
        }

        let directory = fetcher.await?;
        let entry = directory.find_tile_id(tile_id).cloned();
        self.directories
            .lock()
            .expect("leaf cache mutex poisoned")
            .put(offset, directory);
        Ok(entry)
    }
}

/// Which backend an open archive is being read through.
///
/// Two variants rather than two public types, because everything a caller
/// does with an archive is the same on both sides: the metadata is TileJSON
/// in the same place, the header states the same zoom ceiling, and a tile is
/// found by the same directory walk. Only the bytes' provenance differs. A
/// caller who has been developing against a local extract and switches to
/// the bucket it was cut from changes one constructor call and nothing else
/// -- not the type they store, not the `Arc<dyn TileSource>` they hand to
/// `MapView`.
///
/// `AsyncBackend` is not object-safe (its `read` is an `async fn`), so this
/// cannot be a `Box<dyn>` and has to be an enum.
enum Archive {
    Local(AsyncPmTilesReader<MmapBackend, LeafDirectoryCache>),
    Remote(AsyncPmTilesReader<HttpBackend, LeafDirectoryCache>),
}

pub struct PmtilesSource {
    archive: Archive,
    /// Empty when the archive's metadata states no credit. Read once, at
    /// open -- see [`PmtilesSource::open`].
    attribution: String,
    /// The archive's own header `max_zoom`. Read once, at open -- see
    /// [`PmtilesSource::max_zoom`].
    max_zoom: u8,
    /// The metadata's `vector_layers` minzooms ([`TileSource::layer_min_zoom`]).
    layer_min_zooms: std::collections::HashMap<String, u8>,
}

impl PmtilesSource {
    /// A PMTiles archive on disk, read through mmap.
    ///
    /// Unlike [`crate::tile_source::http::HttpTileSource`], this asks for no
    /// attribution: a PMTiles archive carries its own. The metadata block a
    /// writer embeds is TileJSON, so `attribution` is already in the file
    /// next to the tiles it covers, and reading it here means an archive
    /// cannot be served without the credit that travelled with it.
    ///
    /// Metadata that is absent, is not JSON, or states no `attribution`
    /// leaves the credit empty. None of those is an error: an archive is
    /// still perfectly readable without metadata, and refusing to open one
    /// over a missing string would break every archive that has none.
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let reader =
            AsyncPmTilesReader::new_with_cached_path(LeafDirectoryCache::new(), path).await?;
        let (attribution, max_zoom, layer_min_zooms) = describe(&reader).await;
        Ok(Self {
            archive: Archive::Local(reader),
            attribution,
            max_zoom,
            layer_min_zooms,
        })
    }

    /// The same archive, left where it lies: read over HTTP range requests
    /// from a static bucket -- R2, S3, Hugging Face, nginx, GitHub Pages --
    /// with no tile server anywhere.
    ///
    /// This is the whole reason PMTiles exists as a format. The archive is
    /// laid out so that the header and root directory sit in the first 16
    /// KiB, and everything after that is addressed by byte offset, so a
    /// reader can find and pull one tile out of a 123 GB planet file with
    /// two `Range` requests and never see the rest. Opening one costs a
    /// single 16 KiB request; a tile costs one request when its leaf
    /// directory is already cached (see [`LeafDirectoryCache`]) and two when
    /// it is not.
    ///
    /// The credit and the zoom ceiling come from the same places as they do
    /// for a local archive, over two more range requests at open -- there is
    /// no second, weaker path for remote metadata.
    ///
    /// The `reqwest::Client` is built here rather than accepted, on purpose.
    /// `pmtiles` links its own reqwest (0.13, where this crate's own is
    /// 0.12), so a client parameter would put a type from a second, mostly
    /// invisible copy of reqwest into this crate's public signature, and a
    /// caller could not name it without depending on `pmtiles` themselves.
    ///
    /// A server that ignores `Range` and answers `200 OK` with the whole
    /// file is rejected at open with `RangeRequestsUnsupported`, rather than
    /// silently downloading gigabytes to read a header.
    pub async fn open_url(url: impl AsRef<str>) -> anyhow::Result<Self> {
        let reader = AsyncPmTilesReader::new_with_cached_url(
            LeafDirectoryCache::new(),
            pmtiles_client(),
            url.as_ref(),
        )
        .await
        // pmtiles' reqwest error prints the URL, which may carry a token.
        .map_err(|e| anyhow::anyhow!("{}", crate::sprite::redact_access_token(&e.to_string())))?;
        let (attribution, max_zoom, layer_min_zooms) = describe(&reader).await;
        Ok(Self {
            archive: Archive::Remote(reader),
            attribution,
            max_zoom,
            layer_min_zooms,
        })
    }

    /// Where in the world this archive holds anything, as its own header
    /// states it.
    ///
    /// A tileset served over XYZ is implicitly global and a caller opens the
    /// camera wherever they like. An archive is not: it is a bounded extract,
    /// and pointing a camera at the wrong continent shows an empty page that
    /// looks exactly like a broken renderer. This is what lets a caller who
    /// was handed nothing but a URL -- `examples/pmtiles_remote.rs` is
    /// exactly that caller -- open on the data instead of on the Atlantic.
    pub fn coverage(&self) -> Coverage {
        let header = match &self.archive {
            Archive::Local(reader) => reader.get_header(),
            Archive::Remote(reader) => reader.get_header(),
        };
        Coverage {
            min_lon: header.min_longitude,
            min_lat: header.min_latitude,
            max_lon: header.max_longitude,
            max_lat: header.max_latitude,
            center_lon: header.center_longitude,
            center_lat: header.center_latitude,
            // Zero here means the same thing it means for `max_zoom`: the
            // writer left the field alone. Handing it back verbatim would be
            // indistinguishable from a deliberate "open at the whole world",
            // so it is surfaced as an `Option` and the caller decides.
            center_zoom: (header.center_zoom != 0).then_some(header.center_zoom),
            min_zoom: header.min_zoom,
            max_zoom: self.max_zoom,
        }
    }
}

/// What a PMTiles header says about the ground its archive covers -- see
/// [`PmtilesSource::coverage`].
///
/// Fields are public because an archive's own numbers are the only thing
/// here; [`Coverage::opening_camera`] is what turns them into a camera, and
/// exists so that every caller handed nothing but a URL does not have to
/// re-derive it -- badly -- from a header field that is usually zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coverage {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
    pub center_lon: f64,
    pub center_lat: f64,
    /// The zoom the writer suggests opening at, or `None` when it stated
    /// none.
    pub center_zoom: Option<u8>,
    pub min_zoom: u8,
    /// Already passed through the same "zero means unstated" reading
    /// [`PmtilesSource::max_zoom`] reports.
    pub max_zoom: u8,
}

/// The default margin [`Coverage::opening_camera`] leaves around an
/// archive's bounds: half a zoom level, i.e. the data drawn at about 70% of
/// the viewport rather than touching its edges.
///
/// Half a level rather than a whole one because the two neighbouring
/// integer zooms are the two style buckets on either side of the data, and
/// a full level of slack would routinely open a city extract one bucket
/// coarser than the one it was cut for.
const OPENING_MARGIN_ZOOM_LEVELS: f64 = 0.5;

/// The zoom [`Coverage::opening_camera`] falls back to when the archive's
/// bounds say nothing about scale.
///
/// A bounded extract is nearly always a city, and z12 is a city. It is a
/// guess, but it is a guess about a file that declined to describe itself,
/// and every alternative (z0, the min_zoom, the max_zoom) is a worse one.
const OPENING_FALLBACK_ZOOM: f64 = 12.0;

impl Coverage {
    /// Where to point a camera at an archive nobody has described, given
    /// the viewport it will be drawn into, as `(lon, lat, zoom)`.
    ///
    /// A tileset served over XYZ is implicitly global, so a caller opens
    /// the camera wherever they like. An archive is not: it is a bounded
    /// extract, and a camera that misses it draws an empty page that looks
    /// exactly like a broken renderer. This is the function that keeps a
    /// caller who was handed nothing but a URL from having to guess.
    ///
    /// The header's own `center_zoom` is honoured when the writer set one.
    /// **Zero is read as "the writer stated none"**, which is the same
    /// reading [`PmtilesSource::max_zoom`] gives the header's own
    /// `max_zoom` field, and for the same reason: the v3 header spells both
    /// as a `u8` with no null, so a writer that did not fill one in leaves
    /// it at zero. Taking that
    /// literally opens a five-kilometre city extract on the whole planet --
    /// which is precisely the failure this function exists to prevent, and
    /// which the Protomaps Florence demo archive (`center_zoom: 0`,
    /// `bbox` 0.066 deg x 0.044 deg) produces on every run. An archive that
    /// genuinely wants to open at z0 is indistinguishable from one that
    /// stated nothing, and gets the framed view instead; that is the right
    /// side to err on, because it shows the data either way.
    ///
    /// The framing itself is done on BOTH axes, in Web Mercator, and takes
    /// whichever fits less. Longitude alone would be enough for a wide
    /// extract and wrong for a tall one -- a country like Chile fits its
    /// longitude span in the window at a zoom that pushes most of its
    /// latitude span off the top and bottom, which hides data rather than
    /// showing it. Latitude has to be projected first, not measured in
    /// degrees: Mercator's vertical scale is `1/cos(lat)`, so 0.04 degrees
    /// at Florence is a third again as many pixels as it is at the equator.
    #[must_use]
    pub fn opening_camera(
        &self,
        viewport_width_px: f64,
        viewport_height_px: f64,
    ) -> (f64, f64, f64) {
        let zoom = match self.center_zoom {
            Some(stated) if stated != 0 => f64::from(stated),
            _ => self.zoom_that_frames(viewport_width_px, viewport_height_px),
        };
        (self.center_lon, self.center_lat, zoom)
    }

    /// The deepest zoom at which the archive's whole bounding box still
    /// fits the viewport, less [`OPENING_MARGIN_ZOOM_LEVELS`] and clamped
    /// to what the archive can actually serve.
    ///
    /// Worked in unit tile coordinates at z0 (`lon_lat_to_tile_xy` with
    /// `zoom = 0.0`), where the world is exactly one [`TILE_SIZE`]-pixel
    /// tile on each axis, so a span of `s` tile units needs
    /// `s * TILE_SIZE * 2^z` pixels at zoom `z` -- and the zoom that fits
    /// it into `px` pixels is `log2(px / (s * TILE_SIZE))`.
    ///
    /// [`TILE_SIZE`]: crate::geo::TILE_SIZE
    fn zoom_that_frames(&self, viewport_width_px: f64, viewport_height_px: f64) -> f64 {
        let (min_x, max_y) = crate::geo::lon_lat_to_tile_xy(self.min_lon, self.min_lat, 0.0);
        let (max_x, min_y) = crate::geo::lon_lat_to_tile_xy(self.max_lon, self.max_lat, 0.0);

        // Each axis contributes a fit only if it has a span to fit. A
        // degenerate axis (a bbox that is a point, or a line) is not a
        // constraint of zero -- it is no constraint at all, and dividing by
        // it would produce an infinite zoom rather than deferring to the
        // other axis.
        let fit = |span: f64, px: f64| {
            (span.is_finite() && span > 0.0 && px > 0.0)
                .then(|| (px / (span * crate::geo::TILE_SIZE)).log2())
        };
        let fitted = match (
            fit((max_x - min_x).abs(), viewport_width_px),
            fit((max_y - min_y).abs(), viewport_height_px),
        ) {
            // Whichever axis fits LESS well decides: it is the one that
            // would overflow the viewport at the other's zoom.
            (Some(x), Some(y)) => x.min(y),
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => {
                return OPENING_FALLBACK_ZOOM
                    .clamp(f64::from(self.min_zoom), f64::from(self.max_zoom))
            }
        };

        // Clamped last, so an extract whose bounds frame deeper than its
        // own ceiling opens at the ceiling rather than above it, asking for
        // tiles the archive does not have.
        (fitted - OPENING_MARGIN_ZOOM_LEVELS)
            .clamp(f64::from(self.min_zoom), f64::from(self.max_zoom))
    }
}

/// The two things read once at open, for either backend: the credit the
/// archive states and the zoom it stops at.
///
/// Generic over the backend so that a remote archive cannot quietly grow a
/// different -- or absent -- reading of its own metadata than a local one.
async fn describe<B, C>(reader: &AsyncPmTilesReader<B, C>) -> (String, u8, std::collections::HashMap<String, u8>)
where
    B: AsyncBackend + Sync + Send,
    C: DirectoryCache + Sync + Send,
{
    let meta = reader.get_metadata().await.ok().and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let layer_min_zooms = meta.as_ref().map(crate::tile_source::vector_layer_min_zooms).unwrap_or_default();
    let attribution = meta
        .and_then(|meta| meta.get("attribution")?.as_str().map(str::to_string))
        .map(|html| crate::tile_source::attribution_text(&html))
        .unwrap_or_default();

    // Zero is how the v3 header spells "not stated" -- the field is a `u8`
    // with no null, so a writer that did not know its own ceiling leaves it
    // at zero. Reporting that verbatim would clamp the map to z0 and draw
    // one tile of the whole planet forever, which is a far worse failure
    // than not clamping at all. A genuinely z0-only archive is
    // indistinguishable from an unstated one here and pays the old
    // behaviour: requests above z0 that the archive answers with an error.
    let max_zoom = match reader.get_header().max_zoom {
        0 => DEFAULT_MAX_ZOOM,
        stated => stated,
    };

    (attribution, max_zoom, layer_min_zooms)
}

#[async_trait::async_trait]
impl TileSource for PmtilesSource {
    /// A tile the archive does not contain is absence, not failure.
    ///
    /// This is the whole point of an offline region file. An archive
    /// covering one city holds nothing for the rest of the planet, so the
    /// first pan past its edge asks for tiles it will never have -- for as
    /// long as the user keeps panning. Reporting each of those as an error
    /// (which this did) made the ordinary border of a downloaded region
    /// look like a broken map: a line on stderr per tile, a retry per
    /// backoff window, forever.
    ///
    /// `TileCoord::new` still fails loudly, because its complaint is about
    /// the REQUEST rather than the data: an `x`/`y` outside the 2^z grid
    /// is a coordinate that cannot exist in any archive, and silently
    /// answering "no data there" would hide a caller's arithmetic bug.
    async fn fetch(&self, tile: TileId) -> anyhow::Result<Option<Vec<u8>>> {
        let coord = TileCoord::new(tile.z, tile.x, tile.y)?;
        let bytes = match &self.archive {
            Archive::Local(reader) => reader.get_tile_decompressed(coord).await?,
            Archive::Remote(reader) => reader
                .get_tile_decompressed(coord)
                .await
                .map_err(|e| anyhow::anyhow!("{}", crate::sprite::redact_access_token(&e.to_string())))?,
        };
        Ok(bytes.map(|bytes| bytes.to_vec()))
    }

    /// The archive's own ceiling, taken from its header rather than from the
    /// trait's default of [`DEFAULT_MAX_ZOOM`].
    ///
    /// A PMTiles archive is a CLOSED set of tiles: whatever is not in the
    /// file does not exist anywhere, because there is no server behind it to
    /// ask. The trait's default therefore fails differently here than it
    /// does for [`crate::tile_source::http::HttpTileSource`] -- an HTTP
    /// source asked above its ceiling could in principle be pointed
    /// somewhere deeper, whereas an archive asked above its ceiling can only
    /// ever fail, once per tile, for every tile on screen, for as long as
    /// the user stays zoomed in.
    ///
    /// Every writer records this: `pmtiles convert`, `pmtiles extract` and
    /// Planetiler all fill the header's `max_zoom`, and the field is
    /// mandatory in the PMTiles v3 spec, so there is nothing to guess for
    /// any archive built by a real tool.
    fn max_zoom(&self) -> u8 {
        self.max_zoom
    }

    fn attribution(&self) -> Option<&str> {
        (!self.attribution.is_empty()).then_some(self.attribution.as_str())
    }

    fn layer_min_zoom(&self, source_layer: &str) -> Option<u8> {
        self.layer_min_zooms.get(source_layer).copied()
    }
}

/// The HTTP client remote archives are read with: the project's
/// User-Agent and the tile timeouts, as every other tile request
/// (`http::tile_client`), on `pmtiles`' own reqwest. Some CDNs refuse a
/// request with no User-Agent at all -- tiles.overturemaps.org answers
/// 403 to every range read without one -- and `Client::new()` sends none.
fn pmtiles_client() -> ::pmtiles::reqwest::Client {
    // `build` fails only if the TLS backend cannot initialise, which
    // `Client::new()` would panic on anyway.
    ::pmtiles::reqwest::Client::builder()
        .user_agent(super::http::TILE_USER_AGENT)
        .connect_timeout(super::http::HTTP_CONNECT_TIMEOUT)
        .read_timeout(super::http::HTTP_READ_IDLE_TIMEOUT)
        .build()
        .unwrap_or_else(|_| ::pmtiles::reqwest::Client::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::TileId;
    use ::pmtiles::{NoCache, PmTilesWriter, TileCoord, TileType};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn build_archive_bytes(tiles: &[(TileCoord, &[u8])]) -> Vec<u8> {
        build_archive_bytes_with_metadata(tiles, None)
    }

    fn build_archive_bytes_with_metadata(
        tiles: &[(TileCoord, &[u8])],
        metadata: Option<&str>,
    ) -> Vec<u8> {
        build_archive(tiles, metadata, None)
    }

    fn build_archive(
        tiles: &[(TileCoord, &[u8])],
        metadata: Option<&str>,
        max_zoom: Option<u8>,
    ) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut builder = PmTilesWriter::new(TileType::Mvt);
            if let Some(metadata) = metadata {
                builder = builder.metadata(metadata);
            }
            if let Some(max_zoom) = max_zoom {
                builder = builder.max_zoom(max_zoom);
            }
            let mut writer = builder
                .create(&mut buffer)
                .expect("failed to create pmtiles writer");
            for (coord, data) in tiles {
                writer.add_tile(*coord, data).expect("failed to add tile");
            }
            writer.finalize().expect("failed to finalize archive");
        }
        buffer.into_inner()
    }

    fn write_archive(archive_bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pmtiles");
        std::fs::write(&path, archive_bytes).unwrap();
        (dir, path)
    }

    /// A static file server that honours `Range`, which is the only server
    /// behaviour a remote PMTiles archive actually depends on -- and which
    /// counts what it was asked for, so a test can assert on the number of
    /// round trips rather than on wall-clock time.
    struct RangeServer {
        archive: Vec<u8>,
        requests: Arc<AtomicUsize>,
    }

    impl Respond for RangeServer {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            self.requests.fetch_add(1, Ordering::SeqCst);
            let Some(range) = request
                .headers
                .get("range")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("bytes="))
            else {
                // A PMTiles reader never sends a rangeless request, so this
                // is a bug in the test rather than a case to emulate.
                return ResponseTemplate::new(200).set_body_bytes(self.archive.clone());
            };
            let (start, end) = range.split_once('-').expect("malformed range");
            let start: usize = start.parse().expect("malformed range start");
            // The last byte is inclusive, and a reader routinely asks past
            // the end of a small archive (the opening read is always for 16
            // KiB), so this clamps rather than rejecting.
            let end: usize = end
                .parse::<usize>()
                .expect("malformed range end")
                .min(self.archive.len() - 1);
            let body = self.archive[start..=end].to_vec();
            ResponseTemplate::new(206)
                .insert_header(
                    "content-range",
                    format!("bytes {start}-{end}/{}", self.archive.len()).as_str(),
                )
                .set_body_bytes(body)
        }
    }

    /// Serves `archive_bytes` over HTTP with range support. Returns the
    /// server (which must outlive the source), its URL, and the counter of
    /// requests it has answered.
    async fn serve(archive_bytes: Vec<u8>) -> (MockServer, String, Arc<AtomicUsize>) {
        let server = MockServer::start().await;
        let requests = Arc::new(AtomicUsize::new(0));
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(RangeServer {
                archive: archive_bytes,
                requests: Arc::clone(&requests),
            })
            .mount(&server)
            .await;
        let url = format!("{}/archive.pmtiles", server.uri());
        (server, url, requests)
    }

    /// A server that refuses requests without the project's User-Agent
    /// (tiles.overturemaps.org answers 403 to an empty one) still opens:
    /// every range read carries it.
    #[tokio::test]
    async fn remote_reads_carry_the_project_user_agent() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let archive = build_archive_bytes(&[(coord, &[7, 7])]);
        let server = MockServer::start().await;
        let requests = Arc::new(AtomicUsize::new(0));
        Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::header("user-agent", crate::tile_source::http::TILE_USER_AGENT))
            .respond_with(RangeServer { archive, requests: Arc::clone(&requests) })
            .mount(&server)
            .await;
        // Anything else (no or another User-Agent) is refused, as the CDN does.
        Mock::given(wiremock::matchers::method("GET")).respond_with(wiremock::ResponseTemplate::new(403)).mount(&server).await;
        let source = PmtilesSource::open_url(format!("{}/archive.pmtiles", server.uri())).await.expect("opens with the User-Agent");
        assert_eq!(source.fetch(TileId { z: 3, x: 1, y: 2 }).await.unwrap(), Some(vec![7, 7]));
        assert!(requests.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn fetches_a_tile_present_in_the_archive() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let (_dir, path) = write_archive(&build_archive_bytes(&[(coord, &[9, 9, 9])]));

        let source = PmtilesSource::open(&path).await.unwrap();
        let bytes = source.fetch(TileId { z: 3, x: 1, y: 2 }).await.unwrap();
        assert_eq!(bytes, Some(vec![9, 9, 9]));
    }

    /// The offline case, stated directly: a tile outside the downloaded
    /// region reports absence, and absence is not an error. Everything
    /// `MapView` does at the border of an archive -- draw nothing, log
    /// nothing, never ask again -- hangs off this being `Ok(None)`.
    #[tokio::test]
    async fn a_tile_the_archive_does_not_hold_is_absent_not_an_error() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let (_dir, path) = write_archive(&build_archive_bytes(&[(coord, &[9, 9, 9])]));

        let source = PmtilesSource::open(&path).await.unwrap();
        let result = source.fetch(TileId { z: 3, x: 5, y: 5 }).await;
        assert!(
            matches!(result, Ok(None)),
            "a tile outside the archive is the normal state of an offline map at its \
             border, not a failure -- got {result:?}"
        );
    }

    /// Absence is about the DATA. A coordinate that cannot exist on any
    /// z=3 grid (x=8 when only 0..8 exist) is a caller bug, and answering
    /// "no data there" would hide it.
    #[tokio::test]
    async fn an_impossible_coordinate_is_still_an_error() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let archive_bytes = build_archive_bytes(&[(coord, &[9, 9, 9])]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pmtiles");
        std::fs::write(&path, &archive_bytes).unwrap();

        let source = PmtilesSource::open(&path).await.unwrap();
        assert!(source.fetch(TileId { z: 3, x: 8, y: 0 }).await.is_err());
    }

    async fn source_from(metadata: Option<&str>) -> (tempfile::TempDir, PmtilesSource) {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let (dir, path) = write_archive(&build_archive_bytes_with_metadata(
            &[(coord, &[9, 9, 9])],
            metadata,
        ));
        let source = PmtilesSource::open(&path).await.unwrap();
        // The directory is returned so it outlives the mmap-backed reader.
        (dir, source)
    }

    /// The archive credits its own data, with no help from the caller.
    #[tokio::test]
    async fn the_archives_own_metadata_supplies_the_attribution() {
        let (_dir, source) = source_from(Some(
            r#"{"attribution":"<a href=\"https://osm.org/copyright\">&copy; OpenStreetMap</a> contributors"}"#,
        ))
        .await;
        assert_eq!(
            source.attribution(),
            Some("\u{a9} OpenStreetMap contributors")
        );
    }

    /// Metadata with no `attribution` key opens fine and states no credit.
    #[tokio::test]
    async fn metadata_without_an_attribution_key_opens_and_reports_none() {
        let (_dir, source) = source_from(Some(r#"{"name":"test archive"}"#)).await;
        assert_eq!(source.attribution(), None);
        assert_eq!(
            source.fetch(TileId { z: 3, x: 1, y: 2 }).await.unwrap(),
            Some(vec![9, 9, 9])
        );
    }

    /// Metadata that is not JSON at all is not a reason to refuse an
    /// otherwise readable archive.
    #[tokio::test]
    async fn unparseable_metadata_still_opens_the_archive() {
        let (_dir, source) = source_from(Some("not json at all")).await;
        assert_eq!(source.attribution(), None);
        assert_eq!(
            source.fetch(TileId { z: 3, x: 1, y: 2 }).await.unwrap(),
            Some(vec![9, 9, 9])
        );
    }

    /// The archive states its own zoom ceiling, and the source reports that
    /// rather than the trait's default.
    ///
    /// Without this, a z0-14 archive -- what `pmtiles extract`, `pmtiles
    /// convert` and Planetiler all produce -- is asked for z15 and deeper
    /// the moment someone zooms in, and every one of those requests can only
    /// fail: there is no server behind the file to go deeper.
    #[tokio::test]
    async fn the_archives_own_header_supplies_the_zoom_ceiling() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let (_dir, path) = write_archive(&build_archive(&[(coord, &[9, 9, 9])], None, Some(14)));

        let source = PmtilesSource::open(&path).await.unwrap();
        assert_eq!(source.max_zoom(), 14);
    }

    /// An archive says where it is, so a caller handed nothing but a URL can
    /// point a camera at the data instead of at the Atlantic. A `center_zoom`
    /// the writer never set comes back as `None` rather than as z0, which
    /// would read as a deliberate "open on the whole planet".
    #[tokio::test]
    async fn the_archive_states_its_own_coverage() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = PmTilesWriter::new(TileType::Mvt)
                .max_zoom(14)
                .bounds(11.2211, 43.7451, 11.2875, 43.7893)
                .center(11.2543, 43.7672)
                .create(&mut buffer)
                .unwrap();
            writer.add_tile(coord, &[9, 9, 9]).unwrap();
            writer.finalize().unwrap();
        }
        let (_dir, path) = write_archive(&buffer.into_inner());

        let coverage = PmtilesSource::open(&path).await.unwrap().coverage();
        assert!((coverage.center_lon - 11.2543).abs() < 1e-6, "{coverage:?}");
        assert!((coverage.max_lat - 43.7893).abs() < 1e-6, "{coverage:?}");
        assert_eq!(coverage.center_zoom, None);
        assert_eq!(coverage.max_zoom, 14);
    }

    /// An unstated ceiling keeps the trait's default. The v3 header has no
    /// null for this field, so "not stated" and "z0 only" are both zero --
    /// and reporting zero would clamp the map to a single world tile, which
    /// is a much worse way to be wrong than not clamping.
    #[tokio::test]
    async fn an_archive_that_states_no_ceiling_keeps_the_default() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let (_dir, path) = write_archive(&build_archive(&[(coord, &[9, 9, 9])], None, None));

        let source = PmtilesSource::open(&path).await.unwrap();
        assert_eq!(source.max_zoom(), DEFAULT_MAX_ZOOM);
    }

    /// The remote path is the local path with a different backend: same
    /// tiles, same credit out of the same TileJSON, same ceiling out of the
    /// same header. This is the test that keeps `open_url` from acquiring a
    /// weaker reading of an archive than `open` has.
    #[tokio::test]
    async fn a_remote_archive_reads_tiles_credit_and_ceiling_exactly_as_a_local_one() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let archive_bytes = build_archive(
            &[(coord, &[9, 9, 9])],
            Some(
                r#"{"attribution":"<a href=\"https://osm.org/copyright\">&copy; OpenStreetMap</a> contributors"}"#,
            ),
            Some(14),
        );
        let (_server, url, _requests) = serve(archive_bytes).await;

        let source = PmtilesSource::open_url(&url).await.unwrap();
        assert_eq!(
            source.fetch(TileId { z: 3, x: 1, y: 2 }).await.unwrap(),
            Some(vec![9, 9, 9])
        );
        assert_eq!(
            source.attribution(),
            Some("\u{a9} OpenStreetMap contributors")
        );
        assert_eq!(source.max_zoom(), 14);
    }

    /// A tile the remote archive does not hold is absence -- the same
    /// statement the local path makes, for the same reason. Absence is
    /// settled by the directory walk, which is backend-independent: the
    /// entry is simply not in the root or leaf directory, and no request
    /// for tile bytes is ever issued. Nothing about being on a bucket
    /// turns "this extract stops here" into a fault.
    ///
    /// This is the case the Florence demo archive hits on every run: at z2
    /// it holds exactly one tile, so the other three in view are answered
    /// from the directory with nothing.
    #[tokio::test]
    async fn a_missing_tile_in_a_remote_archive_is_absent_not_an_error() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let (_server, url, _requests) = serve(build_archive_bytes(&[(coord, &[9, 9, 9])])).await;

        let source = PmtilesSource::open_url(&url).await.unwrap();
        let result = source.fetch(TileId { z: 3, x: 5, y: 5 }).await;
        assert!(matches!(result, Ok(None)), "got {result:?}");
    }

    /// An archive big enough that its entry list does not fit in the 16 KiB
    /// root directory, so the writer cuts leaf directories -- which is the
    /// only case in which the directory cache does anything at all.
    ///
    /// Every tile gets distinct bytes on purpose: the writer deduplicates
    /// identical tiles and run-length-encodes the entries that point at
    /// them, so an archive of 20,000 copies of the same three bytes
    /// collapses to a single entry and never grows a leaf.
    fn build_archive_with_leaf_directories() -> (Vec<u8>, Vec<TileCoord>) {
        let coords: Vec<TileCoord> = (0..128)
            .flat_map(|x| (0..128).map(move |y| TileCoord::new(7, x, y).unwrap()))
            .collect();
        let payloads: Vec<[u8; 4]> = (0..coords.len() as u32).map(u32::to_le_bytes).collect();
        let tiles: Vec<(TileCoord, &[u8])> = coords
            .iter()
            .zip(&payloads)
            .map(|(coord, payload)| (*coord, payload.as_slice()))
            .collect();
        (build_archive_bytes(&tiles), coords)
    }

    /// The measurement the remote path exists or dies by.
    ///
    /// Twenty neighbouring tiles is one screenful. Every one of them
    /// resolves through the same leaf directory, so with the crate's
    /// `NoCache` that leaf is fetched, gunzipped and re-parsed twenty times
    /// and the screenful costs two round trips per tile. With the cache it
    /// is fetched once and the screenful costs one round trip per tile plus
    /// that one.
    ///
    /// Measured, not assumed: 40 requests uncached against 21 cached, for
    /// the same 20 tiles out of the same archive over the same server. The
    /// uncached figure is asserted exactly because 2N is what `NoCache`
    /// means; the cached one has one request of slack in it because how many
    /// leaves a 4x5 block straddles is the writer's business, not ours.
    #[tokio::test]
    async fn the_leaf_directory_cache_halves_the_requests_a_screenful_costs() {
        let (archive_bytes, coords) = build_archive_with_leaf_directories();
        // `Header::leaf_length` is private, so this reads the field where the
        // v3 spec puts it: a little-endian u64 at byte 48. Asserting on it is
        // the point -- an archive whose entries all fit in the root directory
        // has nothing for a leaf cache to cache, and would let this whole
        // test pass while measuring nothing.
        let leaf_length = u64::from_le_bytes(archive_bytes[48..56].try_into().unwrap());
        assert!(
            leaf_length > 0,
            "this archive is supposed to have leaf directories, and has none"
        );

        // A contiguous 4x5 block, the shape of a viewport rather than a
        // scatter, so the leaf being asked for really is the same one.
        let screenful: Vec<TileCoord> = coords
            .iter()
            .copied()
            .filter(|coord| (40..44).contains(&coord.x()) && (40..45).contains(&coord.y()))
            .collect();
        assert_eq!(screenful.len(), 20);

        let (_server, url, requests) = serve(archive_bytes.clone()).await;
        let uncached = AsyncPmTilesReader::new_with_cached_url(
            NoCache,
            ::pmtiles::reqwest::Client::new(),
            url.as_str(),
        )
        .await
        .unwrap();
        let after_open = requests.load(Ordering::SeqCst);
        for coord in &screenful {
            assert!(uncached.get_tile(*coord).await.unwrap().is_some());
        }
        let uncached_requests = requests.load(Ordering::SeqCst) - after_open;

        let (_server, url, requests) = serve(archive_bytes).await;
        let cached = PmtilesSource::open_url(&url).await.unwrap();
        let after_open = requests.load(Ordering::SeqCst);
        for coord in &screenful {
            assert!(cached
                .fetch(TileId {
                    z: 7,
                    x: coord.x(),
                    y: coord.y()
                })
                .await
                .is_ok());
        }
        let cached_requests = requests.load(Ordering::SeqCst) - after_open;

        assert_eq!(
            uncached_requests,
            2 * screenful.len(),
            "without a directory cache every tile pays for its leaf directory again"
        );
        assert!(
            cached_requests <= screenful.len() + 2,
            "a screenful behind one leaf directory must cost one request per tile plus the \
             leaf itself, not two per tile -- got {cached_requests} for {} tiles",
            screenful.len()
        );
    }

    /// Range support is the entire contract a bucket has to honour. A server
    /// that answers the whole file for a range request must be rejected at
    /// open, not discovered after downloading a planet.
    #[tokio::test]
    async fn a_server_that_ignores_range_is_refused_at_open() {
        let coord = TileCoord::new(3, 1, 2).unwrap();
        let archive_bytes = build_archive_bytes(&[(coord, &[9, 9, 9])]);
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive_bytes))
            .mount(&server)
            .await;

        let result = PmtilesSource::open_url(format!("{}/archive.pmtiles", server.uri())).await;
        assert!(
            result.is_err(),
            "a 200 to a range request is not a usable archive"
        );
    }

    /// The real thing, over the real network, against the archive
    /// `examples/pmtiles_remote.rs` defaults to. `#[ignore]`d because a test
    /// that fails when the wifi drops is a test nobody trusts:
    /// `cargo test --release -- --ignored` runs it deliberately.
    #[tokio::test]
    #[ignore = "hits the network"]
    async fn the_public_protomaps_demo_archive_reads_over_http() {
        let source = PmtilesSource::open_url(crate::tile_source::pmtiles::DEMO_ARCHIVE_URL)
            .await
            .unwrap();
        assert_eq!(source.max_zoom(), 15);
        assert_eq!(
            source.attribution(),
            Some("\u{a9} OpenStreetMap contributors")
        );
        // The z14 tile over the Duomo, 11.2558E 43.7696N -- inside the
        // archive's stated bbox, so it must be there.
        let bytes = source
            .fetch(TileId {
                z: 14,
                x: 8704,
                y: 5972,
            })
            .await
            .unwrap()
            .expect("a tile inside the archive's own bbox is present, not absent");
        assert!(!bytes.is_empty());
    }

    /// The Protomaps Florence demo archive's header, verbatim -- the case
    /// in the bug report. Its `center_zoom` is `0`, which
    /// [`PmtilesSource::coverage`] already reads as unstated, so it arrives
    /// here as `None`; the `Some(0)` spelling is covered separately below.
    fn florence() -> Coverage {
        Coverage {
            min_lon: 11.2211,
            min_lat: 43.7451,
            max_lon: 11.2875,
            max_lat: 43.7893,
            center_lon: 11.2543,
            center_lat: 43.7672,
            center_zoom: None,
            min_zoom: 0,
            max_zoom: 15,
        }
    }

    /// A whole-planet archive: the other end of the range the same
    /// arithmetic has to cover, and the one that catches a formula that
    /// only happens to work on small spans.
    fn planet() -> Coverage {
        Coverage {
            min_lon: -180.0,
            min_lat: -85.0511,
            max_lon: 180.0,
            max_lat: 85.0511,
            center_lon: 0.0,
            center_lat: 0.0,
            center_zoom: None,
            min_zoom: 0,
            max_zoom: 15,
        }
    }

    /// Five kilometres of data opens as five kilometres of data. Anything
    /// near z0 here draws the eastern hemisphere out of an archive that
    /// holds one city, which looks exactly like a broken renderer.
    #[test]
    fn a_city_extract_that_states_no_centre_zoom_opens_on_the_city() {
        let (lon, lat, zoom) = florence().opening_camera(1024.0, 768.0);
        assert!(
            (12.0..14.0).contains(&zoom),
            "Florence's 0.066 x 0.044 degree bbox frames at roughly z13 in a 1024x768 window \
             -- got z{zoom}"
        );
        assert!((lon - 11.2543).abs() < 1e-9 && (lat - 43.7672).abs() < 1e-9);
    }

    /// The same arithmetic on a planet archive gives back the planet. The
    /// height binds here (768 px against a square world), which is why the
    /// answer lands below z1 rather than at it.
    #[test]
    fn a_planet_archive_opens_on_the_planet() {
        let (_, _, zoom) = planet().opening_camera(1024.0, 768.0);
        assert!(
            (0.0..1.0).contains(&zoom),
            "the whole world has to fit a 1024x768 window, which it does just under z1 -- \
             got z{zoom}"
        );
    }

    /// A stated non-zero `center_zoom` is the writer's own answer and wins.
    /// The framing is the fallback for archives that gave none, not a
    /// second-guessing of the ones that did.
    #[test]
    fn a_stated_centre_zoom_is_honoured() {
        let mut coverage = florence();
        coverage.center_zoom = Some(9);
        assert_eq!(coverage.opening_camera(1024.0, 768.0).2, 9.0);
    }

    /// Zero is not a zoom, it is an unfilled field -- the same reading
    /// `describe` gives the header's `max_zoom`. A `Coverage` built by hand
    /// (the fields are public) can still carry `Some(0)`, so the reading
    /// lives in the function that acts on it and not only in the
    /// constructor that usually strips it.
    #[test]
    fn a_centre_zoom_of_zero_is_read_as_unstated_not_as_the_whole_world() {
        let mut coverage = florence();
        coverage.center_zoom = Some(0);
        let (_, _, zoom) = coverage.opening_camera(1024.0, 768.0);
        assert!(zoom > 10.0, "z{zoom} is not a five-kilometre extract");
    }

    /// A tall, narrow extract is framed by its HEIGHT. Longitude alone
    /// would fit the narrow axis and push most of the latitude span off the
    /// top and bottom of the window, hiding the data the camera exists to
    /// show.
    #[test]
    fn a_tall_narrow_extract_is_framed_by_its_height_not_its_width() {
        let tall = Coverage {
            min_lon: -71.0,
            min_lat: -55.0,
            max_lon: -70.0,
            max_lat: -18.0,
            center_lon: -70.5,
            center_lat: -36.5,
            center_zoom: None,
            min_zoom: 0,
            max_zoom: 15,
        };
        let (_, _, zoom) = tall.opening_camera(1024.0, 768.0);
        let width_alone = (1024.0f64 / ((1.0 / 360.0) * crate::geo::TILE_SIZE)).log2() - 0.5;
        assert!(
            zoom < width_alone - 3.0,
            "a 37-degree latitude span cannot be framed at the zoom its 1-degree longitude \
             span allows -- got z{zoom} against z{width_alone} for the width alone"
        );
    }

    /// A bbox that is a single point constrains nothing, and dividing by
    /// its zero span would give an infinite zoom. The fallback is a guess,
    /// but it is a guess about an archive that declined to describe itself.
    #[test]
    fn a_degenerate_bbox_falls_back_rather_than_producing_an_infinite_zoom() {
        let point = Coverage {
            min_lon: 11.25,
            min_lat: 43.77,
            max_lon: 11.25,
            max_lat: 43.77,
            center_lon: 11.25,
            center_lat: 43.77,
            center_zoom: None,
            min_zoom: 0,
            max_zoom: 15,
        };
        assert_eq!(point.opening_camera(1024.0, 768.0).2, OPENING_FALLBACK_ZOOM);
    }

    /// The framing never asks for a zoom the archive cannot serve. An
    /// extract cut at z10 whose bounds would frame at z13 opens at z10,
    /// rather than at a zoom whose every tile is above the ceiling.
    #[test]
    fn the_framed_zoom_is_clamped_to_what_the_archive_holds() {
        let mut coverage = florence();
        coverage.max_zoom = 10;
        assert_eq!(coverage.opening_camera(1024.0, 768.0).2, 10.0);
    }
}
