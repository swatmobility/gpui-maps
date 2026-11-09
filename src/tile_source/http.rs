use crate::geo::TileId;
use crate::tile_source::TileSource;

/// How long connecting to the tile server may take before a request is an
/// error.
pub const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a request may go without receiving anything (headers or the
/// next body bytes) before it is an error. Without it a stalled TCP
/// connection never answers, and whatever waits on it (a map's in-flight
/// slot) is held forever.
///
/// There is deliberately NO total timeout: a 1 MB satellite tile on a
/// 300 kbps link takes ~30 s while making steady progress, and a total
/// cap would fail it, the live map's backoff would retry it from scratch,
/// and it would fail again forever. A caller that does want a total cap
/// (the story prefetcher's `fetch_timeout`) applies its own.
pub const HTTP_READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The User-Agent every tile request carries: the project and its
/// repository, never a user's email or a token. OpenStreetMap-family tile
/// policies ask for an identifying one.
pub const TILE_USER_AGENT: &str = concat!("gpui-maps/", env!("CARGO_PKG_VERSION"), " (+https://github.com/swatmobility/gpui-maps)");

/// A client with the project's User-Agent and the tile timeouts: what
/// every tile request in this crate goes out on (`fetch` in the browser).
pub fn tile_client() -> reqwest::Client {
    client_with_timeouts(HTTP_CONNECT_TIMEOUT, HTTP_READ_IDLE_TIMEOUT)
}

fn client_with_timeouts(connect: std::time::Duration, read_idle: std::time::Duration) -> reqwest::Client {
    // `build` fails only if the TLS backend cannot initialise, which
    // `Client::new()` would panic on anyway; fall back to it so the
    // failure (if any) is the one it always was.
    reqwest::Client::builder()
        .user_agent(TILE_USER_AGENT)
        .connect_timeout(connect)
        .read_timeout(read_idle)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

pub struct HttpTileSource {
    url_template: String,
    client: reqwest::Client,
    max_zoom: u8,
    /// Empty for a caller who states no credit -- see [`HttpTileSource::new`].
    attribution: String,
    /// TileJSON `vector_layers` minzooms, when the caller hands them over
    /// ([`HttpTileSource::with_tilejson`]).
    layer_min_zooms: std::collections::HashMap<String, u8>,
    /// Off unless [`HttpTileSource::with_disk_cache`] turns it on.
    #[cfg(not(target_arch = "wasm32"))]
    disk_cache: Option<crate::tile_source::disk_cache::DiskCacheStore>,
}

impl HttpTileSource {
    /// `attribution` is the credit this source's data carries, as the
    /// provider's own TileJSON states it. HTML is expected and is reduced
    /// for display by [`crate::tile_source::attribution_text`].
    ///
    /// It is a required argument rather than a builder step because a
    /// forgotten credit is a licence breach that looks exactly like a
    /// working map: OpenStreetMap-derived tiles are ODbL, and nothing else
    /// in this pipeline can notice the omission. Pass `""` to state
    /// deliberately that this source needs none -- your own data, an
    /// internal basemap -- and nothing is drawn.
    pub fn new(url_template: impl Into<String>, max_zoom: u8, attribution: impl Into<String>) -> Self {
        Self {
            url_template: url_template.into(),
            client: client_with_timeouts(HTTP_CONNECT_TIMEOUT, HTTP_READ_IDLE_TIMEOUT),
            max_zoom,
            attribution: crate::tile_source::attribution_text(&attribution.into()),
            layer_min_zooms: std::collections::HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            disk_cache: None,
        }
    }

    /// This source behind an HTTP-semantics disk cache
    /// ([`crate::tile_source::disk_cache`]): `Cache-Control` lifetimes,
    /// `no-store`/`private` never written, `ETag`/`Last-Modified`
    /// revalidation, stale-if-error bounded by `max_stale` (and never for
    /// `must-revalidate`), LRU eviction to `max_bytes`, atomic writes, and
    /// keys that never hold the URL's token.
    ///
    /// **The key drops the URL's credentials**, so two tokens share one
    /// entry: right for a public tileset, but for a PRIVATE tileset the
    /// only isolation between tokens is the server answering
    /// `Cache-Control: private` (never written). Do not enable the cache
    /// for a private tileset unless its server marks its tiles `private`.
    /// The redaction also blanks any parameter ending in `key=` or
    /// `token=`, so two URLs differing only in such a non-credential value
    /// share an entry.
    ///
    /// **Off by default, and the developer's decision.** Whether a
    /// provider's tiles may be stored on disk is a matter of its terms --
    /// open sources like OpenFreeMap invite it with year-long `max-age`s,
    /// Mapbox restricts it. Caching a provider's tiles is the developer's
    /// responsibility under that provider's terms. Native only.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn with_disk_cache(mut self, cache: crate::tile_source::disk_cache::DiskCache) -> Self {
        self.disk_cache = Some(crate::tile_source::disk_cache::DiskCacheStore::new(cache));
        self
    }

    /// This source with a read-idle timeout other than
    /// [`HTTP_READ_IDLE_TIMEOUT`] (the connect timeout is unchanged).
    #[must_use]
    pub fn with_read_idle_timeout(mut self, read_idle: std::time::Duration) -> Self {
        self.client = client_with_timeouts(HTTP_CONNECT_TIMEOUT, read_idle);
        self
    }

    /// Reads the provider's TileJSON `vector_layers` (each layer's
    /// `minzoom`), for [`TileSource::layer_min_zoom`].
    #[must_use]
    pub fn with_tilejson(mut self, tilejson: &serde_json::Value) -> Self {
        self.layer_min_zooms = crate::tile_source::vector_layer_min_zooms(tilejson);
        self
    }

    fn url_for(&self, tile: TileId) -> String {
        self.url_template
            .replace("{z}", &tile.z.to_string())
            .replace("{x}", &tile.x.to_string())
            .replace("{y}", &tile.y.to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HttpTileSource {
    /// [`TileSource::fetch`] with the disk cache: a fresh entry is served
    /// as is, a stale one is revalidated (and served if the server cannot
    /// be reached or answers 5xx), a miss is fetched and written as its
    /// headers allow. Errors are redacted exactly as the uncached path's.
    async fn fetch_through_disk_cache(
        &self,
        store: &crate::tile_source::disk_cache::DiskCacheStore,
        tile: TileId,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        use crate::tile_source::disk_cache::{DiskCacheStore, Policy};
        let url = self.url_for(tile);
        let key = DiskCacheStore::key_for(&url);
        let held = store.read(&key).await;
        let now = std::time::SystemTime::now();
        if let Some(entry) = &held {
            if entry.is_fresh(now) {
                return Ok(Some(entry.body.clone()));
            }
        }
        // What stands in when the server cannot answer: the held entry, if
        // it is not `must-revalidate` and not past `max_stale`.
        let stale = held.as_ref().filter(|e| e.can_serve_stale(now, store.max_stale())).map(|e| e.body.clone());
        let redacted = |e: reqwest::Error| anyhow::anyhow!("{}", crate::sprite::redact_url_credentials(&e.to_string()));
        let mut request = self.client.get(&url);
        if let Some(entry) = held.as_ref().filter(|e| e.can_revalidate()) {
            for (name, value) in entry.validators() {
                request = request.header(name, value);
            }
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => return stale.map(Some).ok_or_else(|| redacted(e)),
        };
        let status = response.status();
        let policy = Policy::from_headers(response.headers(), std::time::SystemTime::now());
        if status == reqwest::StatusCode::NOT_MODIFIED {
            // Only ever an answer to our own validators.
            let Some(entry) = held else { anyhow::bail!("a 304 for a tile this cache does not hold") };
            store.refresh(&key, &entry, &policy).await;
            return Ok(Some(entry.body));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            store.remove(&key).await;
            return Ok(None);
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if let Some(body) = stale {
                return Ok(Some(body));
            }
        }
        let response = response.error_for_status().map_err(redacted)?;
        let body = match response.bytes().await {
            Ok(body) => body.to_vec(),
            // The connection dropped mid-body: as unreachable.
            Err(e) => return stale.map(Some).ok_or_else(|| redacted(e)),
        };
        store.write(&key, &body, &policy, None).await;
        Ok(Some(body))
    }
}

#[async_trait::async_trait]
impl TileSource for HttpTileSource {
    /// `404 Not Found` is absence, not failure.
    ///
    /// It is the same statement a PMTiles archive makes by not holding a
    /// tile, only over the wire: the server was reached, it understood the
    /// request, and it says there is no data at that coordinate. Sparse
    /// coverage -- a tileset for one city, one country, one bus network --
    /// answers that way for most of the world, and treating it as an error
    /// would log and retry every tile outside the covered area forever.
    ///
    /// This is what MapLibre GL JS does with the same status (a 404 marks
    /// the tile loaded-and-empty rather than errored), and it is why this
    /// is checked before `error_for_status`: every other non-success code
    /// -- 500, 502, 429, a proxy's 403 -- is a real failure and stays one.
    /// A server that answers 200 with an empty body is also not absence:
    /// that is a zero-feature tile, which decodes fine and draws nothing.
    ///
    /// **An error never carries the URL's access token.** `reqwest::Error`'s
    /// `Display` appends ` for url (...)` with the whole query string, so a
    /// 401/403 or a network failure from a `?access_token=pk...` template
    /// printed the token wherever the error was shown (a panic, a log).
    /// Every error leaving here is redacted by
    /// [`crate::sprite::redact_url_credentials`]: an `access_token=`, a
    /// `key=`/`token=`/`password=` argument and a `user:pass@` authority
    /// all become `[REDACTED]`.
    async fn fetch(&self, tile: TileId) -> anyhow::Result<Option<Vec<u8>>> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(store) = &self.disk_cache {
            return self.fetch_through_disk_cache(store, tile).await;
        }
        let url = self.url_for(tile);
        let redacted = |e: reqwest::Error| anyhow::anyhow!("{}", crate::sprite::redact_url_credentials(&e.to_string()));
        let response = self.client.get(&url).send().await.map_err(redacted)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response.error_for_status().map_err(redacted)?;
        Ok(Some(response.bytes().await.map_err(redacted)?.to_vec()))
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::TileId;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A server that sends nothing (here: headers 10 s late) is an error
    /// once the read-idle timeout passes, not a request held forever.
    #[tokio::test]
    async fn a_stalled_request_times_out_as_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1]).set_delay(std::time::Duration::from_secs(10)))
            .mount(&server)
            .await;
        let source = HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri()), 20, "")
            .with_read_idle_timeout(std::time::Duration::from_millis(200));
        let started = std::time::Instant::now();
        let result = source.fetch(TileId { z: 3, x: 1, y: 2 }).await;
        assert!(result.is_err(), "{result:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    }

    // The slow-but-steady body tests (F11) need a raw socket server, and
    // `agent::tests::the_library_binds_no_socket` forbids one under src/:
    // they are in tests/http_read_idle_timeout.rs.

    #[tokio::test]
    async fn fetches_tile_bytes_from_a_templated_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/1/2.mvt"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3, 4]))
            .mount(&server)
            .await;

        let source = HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri()), 20, "");
        let bytes = source.fetch(TileId { z: 3, x: 1, y: 2 }).await.unwrap();
        assert_eq!(bytes, Some(vec![1, 2, 3, 4]));
    }

    /// Fix round 3: an HTTP error from a tokenised URL does not carry the
    /// token (`reqwest::Error`'s `Display` prints the URL).
    #[tokio::test]
    async fn an_http_error_never_carries_the_access_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(403)).mount(&server).await;
        let source = HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt?access_token=SECRET", server.uri()), 20, "");
        let err = source.fetch(TileId { z: 3, x: 1, y: 2 }).await.expect_err("a 403 is an error");
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("SECRET"), "the token leaked: {text}");
        assert!(text.contains("403"), "the status is still said: {text}");

        // A connection failure too: nothing listens on a dropped server.
        let dead = HttpTileSource::new("http://127.0.0.1:9/{z}/{x}/{y}.mvt?access_token=SECRET", 20, "");
        let err = dead.fetch(TileId { z: 3, x: 1, y: 2 }).await.expect_err("nothing listens");
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("SECRET"), "the token leaked: {text}");
        // F1: every credential form -- a `key=`, a `user:pass@` -- as well.
        let keyed = format!("{}/{{z}}/{{x}}/{{y}}.mvt?access_token=SECRET1&key=SECRET2", server.uri().replace("://", "://user:SECRET3@"));
        let err = HttpTileSource::new(keyed, 20, "").fetch(TileId { z: 3, x: 1, y: 2 }).await.expect_err("a 403 is an error");
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("SECRET"), "a credential leaked: {text}");
        let dead = HttpTileSource::new("http://user:SECRET3@127.0.0.1:9/{z}/{x}/{y}.mvt?key=SECRET2", 20, "");
        let err = dead.fetch(TileId { z: 3, x: 1, y: 2 }).await.expect_err("nothing listens");
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("SECRET"), "a credential leaked: {text}");
    }

    /// The credit is reduced once, at construction, and read back as text.
    #[test]
    fn a_markup_attribution_is_reduced_for_display() {
        let source = HttpTileSource::new(
            "https://example.invalid/{z}/{x}/{y}.mvt",
            14,
            r#"<a href="https://www.openstreetmap.org/copyright">&copy; OpenStreetMap</a> contributors"#,
        );
        assert_eq!(source.attribution(), Some("\u{a9} OpenStreetMap contributors"));
    }

    /// An empty credit is a stated absence, not an empty label to draw.
    #[test]
    fn an_empty_attribution_reports_none_rather_than_an_empty_string() {
        let source = HttpTileSource::new("https://example.invalid/{z}/{x}/{y}.mvt", 14, "");
        assert_eq!(source.attribution(), None);
    }

    /// Markup that reduces to nothing is the same stated absence.
    #[test]
    fn an_attribution_of_only_markup_reports_none() {
        let source = HttpTileSource::new("https://example.invalid/{z}/{x}/{y}.mvt", 14, "<span></span>");
        assert_eq!(source.attribution(), None);
    }

    /// A server's 404 is the same statement an archive makes by not
    /// holding a tile: sparse coverage, not a broken request. A tileset
    /// for one city answers this way for most of the planet.
    #[tokio::test]
    async fn a_404_is_absence_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/9/1/1.mvt"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let source = HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri()), 20, "");
        let result = source.fetch(TileId { z: 9, x: 1, y: 1 }).await;
        assert!(
            matches!(result, Ok(None)),
            "404 means the server has no data at this coordinate, which is not a failure \
             to fetch -- got {result:?}"
        );
    }

    /// Every OTHER non-success status is still a failure. This is the test
    /// that keeps the 404 special case from becoming "any non-200 is an
    /// empty tile", which would draw a blank map over a broken backend and
    /// never say so.
    #[tokio::test]
    async fn a_server_error_is_still_an_error() {
        let server = MockServer::start().await;
        for (tile_path, status) in [("/9/1/2.mvt", 500), ("/9/1/3.mvt", 429), ("/9/1/4.mvt", 403)] {
            Mock::given(method("GET"))
                .and(path(tile_path))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
        }

        let source = HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri()), 20, "");
        for y in 2..=4 {
            assert!(
                source.fetch(TileId { z: 9, x: 1, y }).await.is_err(),
                "a non-404 failure status must stay an error so the map retries and reports it"
            );
        }
    }

    /// A 200 with no body is a tile with no features, not an absent tile
    /// -- it decodes to an empty `DecodedTile` and draws nothing, and it
    /// must NOT be recorded as "this source has no data here".
    #[tokio::test]
    async fn an_empty_200_body_is_a_present_but_featureless_tile() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/9/2/2.mvt"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(Vec::<u8>::new()))
            .mount(&server)
            .await;

        let source = HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri()), 20, "");
        assert_eq!(source.fetch(TileId { z: 9, x: 2, y: 2 }).await.unwrap(), Some(Vec::new()));
    }
}

/// The opt-in disk cache ([`HttpTileSource::with_disk_cache`]) against a
/// local mock server: hit, miss, `304` revalidation, `no-store`, eviction,
/// concurrent writers, and the credential rule.
#[cfg(test)]
mod disk_cache_tests {
    use super::*;
    use crate::tile_source::disk_cache::DiskCache;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const T: TileId = TileId { z: 3, x: 1, y: 2 };

    fn cached(server: &MockServer, dir: &std::path::Path, max_bytes: u64) -> HttpTileSource {
        HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri()), 20, "")
            .with_disk_cache(DiskCache::new(dir, max_bytes))
    }

    fn entries(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    #[tokio::test]
    async fn a_fresh_entry_is_served_without_asking() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/1/2.mvt"))
            .respond_with(ResponseTemplate::new(200).insert_header("cache-control", "public, max-age=3600").set_body_bytes(vec![7; 100]))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let source = cached(&server, dir.path(), 1 << 20);
        assert_eq!(source.fetch(T).await.unwrap(), Some(vec![7; 100]), "miss: fetched");
        assert_eq!(source.fetch(T).await.unwrap(), Some(vec![7; 100]), "hit: from disk");
        // A second source on the same directory (another panel, a restart).
        assert_eq!(cached(&server, dir.path(), 1 << 20).fetch(T).await.unwrap(), Some(vec![7; 100]));
        assert_eq!(entries(dir.path()).len(), 1);
        // `expect(1)` is verified when the server drops.
    }

    #[tokio::test]
    async fn a_stale_entry_is_revalidated_and_a_304_serves_the_held_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304).insert_header("cache-control", "max-age=3600"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "no-cache")
                    .insert_header("etag", "\"v1\"")
                    .insert_header("last-modified", "Tue, 01 Aug 2017 09:58:11 GMT")
                    .set_body_bytes(b"tile-v1".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let source = cached(&server, dir.path(), 1 << 20);
        assert_eq!(source.fetch(T).await.unwrap().as_deref(), Some(&b"tile-v1"[..]), "miss");
        // `no-cache`: stored, but asked about -- the 304 serves the bytes
        // and its max-age makes the entry fresh from then on.
        assert_eq!(source.fetch(T).await.unwrap().as_deref(), Some(&b"tile-v1"[..]), "304");
        assert_eq!(source.fetch(T).await.unwrap().as_deref(), Some(&b"tile-v1"[..]), "fresh after the 304");
        let sent = server.received_requests().await.unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].headers.get("if-modified-since").unwrap(), "Tue, 01 Aug 2017 09:58:11 GMT");
    }

    #[tokio::test]
    async fn no_store_and_private_are_never_written() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/3/1/2.mvt"))
            .respond_with(ResponseTemplate::new(200).insert_header("cache-control", "no-store").set_body_bytes(vec![1]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/3/1/3.mvt"))
            .respond_with(ResponseTemplate::new(200).insert_header("cache-control", "private, max-age=600").set_body_bytes(vec![2]))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let source = cached(&server, dir.path(), 1 << 20);
        for _ in 0..2 {
            assert_eq!(source.fetch(T).await.unwrap(), Some(vec![1]));
            assert_eq!(source.fetch(TileId { y: 3, ..T }).await.unwrap(), Some(vec![2]));
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 4, "every fetch went to the server");
        assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    }

    /// A stale entry is served when the server is gone; a 404 drops it.
    #[tokio::test]
    async fn a_stale_entry_outlives_the_server_but_not_a_404() {
        let dir = tempfile::tempdir().unwrap();
        // Not `MockServer::start()`: a pooled server is not shut down by
        // `drop`, only returned to the pool, where a test running alongside
        // takes it -- and "offline" then reaches that test's mocks.
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"a\"").set_body_bytes(vec![9]))
            .mount(&server)
            .await;
        let template = format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri());
        let source = || HttpTileSource::new(template.clone(), 20, "").with_disk_cache(DiskCache::new(dir.path(), 1 << 20));
        assert_eq!(source().fetch(T).await.unwrap(), Some(vec![9]));
        drop(server);
        assert_eq!(source().fetch(T).await.unwrap(), Some(vec![9]), "offline: the stale tile");

        // A server that had the tile and now says it has none: a 404 drops it.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"b\"").set_body_bytes(vec![8]))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        let gone = HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri()), 20, "")
            .with_disk_cache(DiskCache::new(dir.path(), 1 << 20));
        // Another tile: the new server may reuse the old one's port.
        let other = TileId { y: 7, ..T };
        assert_eq!(gone.fetch(other).await.unwrap(), Some(vec![8]));
        assert_eq!(entries(dir.path()).len(), 2);
        assert_eq!(gone.fetch(other).await.unwrap(), None);
        assert_eq!(entries(dir.path()).len(), 1, "the 404 dropped its entry");
    }

    #[tokio::test]
    async fn the_directory_is_held_to_its_budget_least_recently_used_first() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("cache-control", "max-age=3600").set_body_bytes(vec![5; 1000]))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        // Room for about five 1 KB entries.
        let source = cached(&server, dir.path(), 5 * 1200);
        for y in 0..5 {
            source.fetch(TileId { y, ..T }).await.unwrap();
        }
        assert_eq!(entries(dir.path()).len(), 5);
        // Touch y=0 so it is the most recently used, then add two more.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        source.fetch(TileId { y: 0, ..T }).await.unwrap();
        for y in 5..7 {
            source.fetch(TileId { y, ..T }).await.unwrap();
        }
        let total: u64 = entries(dir.path()).iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
        assert!(total <= 5 * 1200, "{total} bytes held");
        let requests_before = server.received_requests().await.unwrap().len();
        source.fetch(TileId { y: 0, ..T }).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), requests_before, "the recently used tile was kept");
        source.fetch(TileId { y: 1, ..T }).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), requests_before + 1, "the least recently used was evicted");
    }

    /// Many fetches of the same and different tiles at once, through two
    /// sources on one directory: every answer is whole, every file is a
    /// readable entry, and no temp file is left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_writers_never_tear_an_entry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("cache-control", "max-age=0").insert_header("etag", "\"e\"").set_body_bytes(vec![3; 50_000]))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let a = std::sync::Arc::new(cached(&server, dir.path(), 1 << 30));
        let b = std::sync::Arc::new(cached(&server, dir.path(), 1 << 30));
        let mut joins = Vec::new();
        for i in 0..64u32 {
            let source = if i % 2 == 0 { a.clone() } else { b.clone() };
            joins.push(tokio::spawn(async move { source.fetch(TileId { y: i % 4, ..T }).await }));
        }
        for j in joins {
            assert_eq!(j.await.unwrap().unwrap(), Some(vec![3; 50_000]));
        }
        let files = entries(dir.path());
        assert_eq!(files.len(), 4, "{files:?}");
        for f in files {
            assert!(f.extension().is_some_and(|e| e == "tile"), "a temp file was left: {f:?}");
            assert!(std::fs::metadata(&f).unwrap().len() as usize > 50_000);
        }
        // And they read back (304s from the `e` etag would also serve them).
        assert_eq!(a.fetch(TileId { y: 2, ..T }).await.unwrap(), Some(vec![3; 50_000]));
    }

    /// The credential rule: a tokenised URL's token is in no file name and
    /// no file's bytes.
    #[tokio::test]
    async fn a_token_bearing_url_never_reaches_the_disk() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=3600")
                    .insert_header("etag", "\"tag\"")
                    .set_body_bytes(b"tile bytes".to_vec()),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let url = format!("{}/{{z}}/{{x}}/{{y}}.mvt?access_token=pk.SECRETTOKEN&key=SECRETKEY", server.uri().replace("://", "://user:SECRETPASS@"));
        let source = HttpTileSource::new(url, 20, "").with_disk_cache(DiskCache::new(dir.path(), 1 << 20));
        for y in 0..3 {
            source.fetch(TileId { y, ..T }).await.unwrap();
        }
        let files = entries(dir.path());
        assert_eq!(files.len(), 3);
        for f in &files {
            let name = f.file_name().unwrap().to_string_lossy();
            let bytes = std::fs::read(f).unwrap();
            for secret in ["SECRET", "access_token", "127.0.0.1", "user:"] {
                assert!(!name.contains(secret), "{secret} in the name {name}");
                assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} in {name}");
            }
        }
    }

    /// Absurd lifetimes neither panic nor overflow: `max-age=u64::MAX`, a
    /// `max-age` past u64, and an `Expires` in the year 9999 or 10000.
    #[tokio::test]
    async fn absurd_lifetimes_never_panic() {
        let server = MockServer::start().await;
        let cases = [
            ("/3/1/0.mvt", "cache-control", format!("max-age={}", u64::MAX)),
            ("/3/1/1.mvt", "cache-control", "max-age=999999999999999999999999999999".to_string()),
            ("/3/1/2.mvt", "expires", "Fri, 31 Dec 9999 23:59:59 GMT".to_string()),
            ("/3/1/3.mvt", "expires", "Sat, 01 Jan 10000 00:00:00 GMT".to_string()),
            ("/3/1/4.mvt", "expires", "Mon, 01 Jan 99999999999999999999 00:00:00 GMT".to_string()),
        ];
        for (p, name, value) in &cases {
            Mock::given(method("GET"))
                .and(path(*p))
                .respond_with(ResponseTemplate::new(200).insert_header(*name, value.as_str()).insert_header("etag", "\"x\"").set_body_bytes(vec![4]))
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let source = cached(&server, dir.path(), 1 << 20);
        for y in 0..5 {
            for _ in 0..2 {
                assert_eq!(source.fetch(TileId { y, ..T }).await.unwrap(), Some(vec![4]), "y={y}");
            }
        }
        // The two capped max-ages and the year-9999 Expires were served from
        // disk the second time; the year-10000 ones (unparseable: expired)
        // were revalidated.
        let sent = server.received_requests().await.unwrap();
        assert_eq!(sent.len(), 7, "{:?}", sent.iter().map(|r| r.url.path().to_string()).collect::<Vec<_>>());
    }

    /// Stale-if-error: a 5xx or a 429 serves the stale entry; not when it
    /// was stored `must-revalidate`, and not past `max_stale`.
    #[tokio::test]
    async fn stale_if_error_is_bounded() {
        let server = MockServer::start().await;
        for (p, cc) in [("/3/1/0.mvt", "max-age=0"), ("/3/1/1.mvt", "max-age=0, must-revalidate"), ("/3/1/2.mvt", "max-age=0, proxy-revalidate")] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).insert_header("cache-control", cc).insert_header("etag", "\"s\"").set_body_bytes(vec![6]))
                .up_to_n_times(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("GET")).and(path("/3/1/0.mvt")).respond_with(ResponseTemplate::new(503)).up_to_n_times(1).mount(&server).await;
        Mock::given(method("GET")).and(path("/3/1/0.mvt")).respond_with(ResponseTemplate::new(429)).mount(&server).await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let source = cached(&server, dir.path(), 1 << 20);
        for y in 0..3 {
            assert_eq!(source.fetch(TileId { y, ..T }).await.unwrap(), Some(vec![6]));
        }
        assert_eq!(source.fetch(TileId { y: 0, ..T }).await.unwrap(), Some(vec![6]), "a 503 serves stale");
        assert_eq!(source.fetch(TileId { y: 0, ..T }).await.unwrap(), Some(vec![6]), "a 429 serves stale");
        assert!(source.fetch(TileId { y: 1, ..T }).await.is_err(), "must-revalidate is never served stale");
        assert!(source.fetch(TileId { y: 2, ..T }).await.is_err(), "proxy-revalidate is never served stale");
        // max_stale = 0: an entry already past its freshness is not served.
        let strict = HttpTileSource::new(format!("{}/{{z}}/{{x}}/{{y}}.mvt", server.uri()), 20, "")
            .with_disk_cache(DiskCache::new(dir.path(), 1 << 20).with_max_stale(std::time::Duration::ZERO));
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(strict.fetch(TileId { y: 0, ..T }).await.is_err(), "past max_stale");
    }
}
