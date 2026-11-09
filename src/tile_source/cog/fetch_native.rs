//! Native reads: a local file by seek, bytes in memory, or an http(s) URL
//! by `Range` requests through this crate's own `reqwest` client (the
//! project User-Agent, `Accept-Encoding: identity`). A server that ignores
//! `Range` (a 200) is read whole once, within
//! [`CogOptions::max_whole_file_bytes`], and sliced from then on.
//!
//! The decode and resample run on tokio's blocking pool
//! (`spawn_blocking`), never on the thread that awaits the fetch.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{header_from, header_step, join_bounded, Cog, CogError, CogOptions, CogSource, RangeGroup, FIRST_HEADER_READ};
use crate::geo::TileId;

/// Where the bytes come from.
pub(crate) enum Reader {
    Bytes(Arc<Vec<u8>>),
    File { path: PathBuf, len: u64 },
    #[cfg(feature = "sprite_http_fetch")]
    Http { client: reqwest::Client, url: String, max_whole: u64, whole: Mutex<Option<Arc<Vec<u8>>>>, len: Mutex<Option<u64>> },
}

fn slice(whole: &[u8], start: u64, len: u64) -> Vec<u8> {
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(whole.len());
    let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(whole.len());
    whole[start..end].to_vec()
}

#[cfg(feature = "sprite_http_fetch")]
fn fail(e: reqwest::Error) -> CogError {
    if e.is_timeout() {
        return CogError::Timeout;
    }
    let mut causes = Vec::new();
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        causes.push(c.to_string());
        cause = c.source();
    }
    let mut text = e.without_url().to_string();
    for c in causes {
        text = format!("{text}: {c}");
    }
    CogError::Http(crate::sprite::redact_url_credentials(&text))
}

impl Reader {
    fn file_len(&self) -> Option<u64> {
        match self {
            Reader::Bytes(b) => Some(b.len() as u64),
            Reader::File { len, .. } => Some(*len),
            #[cfg(feature = "sprite_http_fetch")]
            Reader::Http { len, .. } => *len.lock().expect("len"),
        }
    }

    /// `len` bytes from `start` (fewer at the end of the file).
    async fn read(&self, start: u64, len: u64) -> Result<Vec<u8>, CogError> {
        match self {
            Reader::Bytes(b) => Ok(slice(b, start, len)),
            Reader::File { path, len: file_len } => {
                let start = start.min(*file_len);
                let len = len.min(file_len - start);
                let io = |e: std::io::Error| CogError::Io(e.to_string());
                let mut file = std::fs::File::open(path).map_err(io)?;
                file.seek(SeekFrom::Start(start)).map_err(io)?;
                let mut out = vec![0; len as usize];
                file.read_exact(&mut out).map_err(io)?;
                Ok(out)
            }
            #[cfg(feature = "sprite_http_fetch")]
            Reader::Http { client, url, max_whole, whole, len: known } => {
                if let Some(w) = whole.lock().expect("whole").clone() {
                    return Ok(slice(&w, start, len));
                }
                if len == 0 || known.lock().expect("len").is_some_and(|l| start >= l) {
                    return Ok(Vec::new());
                }
                let range = format!("bytes={start}-{}", start + len - 1);
                let response = client
                    .get(url)
                    .header(reqwest::header::RANGE, range)
                    .header(reqwest::header::ACCEPT_ENCODING, "identity")
                    .send()
                    .await
                    .map_err(fail)?;
                let status = response.status().as_u16();
                match status {
                    206 => {
                        if let Some(total) = response
                            .headers()
                            .get(reqwest::header::CONTENT_RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.rsplit('/').next()?.trim().parse::<u64>().ok())
                        {
                            *known.lock().expect("len") = Some(total);
                        }
                        Ok(response.bytes().await.map_err(fail)?.to_vec())
                    }
                    200 => {
                        if let Some(got) = response.content_length().filter(|&g| g > *max_whole) {
                            return Err(CogError::TooLarge { limit: *max_whole, got });
                        }
                        let body = response.bytes().await.map_err(fail)?.to_vec();
                        if body.len() as u64 > *max_whole {
                            return Err(CogError::TooLarge { limit: *max_whole, got: body.len() as u64 });
                        }
                        *known.lock().expect("len") = Some(body.len() as u64);
                        let out = slice(&body, start, len);
                        *whole.lock().expect("whole") = Some(Arc::new(body));
                        Ok(out)
                    }
                    416 => Ok(Vec::new()),
                    other => Err(CogError::Http(format!("HTTP {other}"))),
                }
            }
        }
    }
}

/// Reads the header: 64 KiB, then (rarely) one larger prefix.
async fn read_header(reader: &Reader) -> Result<super::Header, CogError> {
    let mut prefix = reader.read(0, FIRST_HEADER_READ).await?;
    let mut requests = 1;
    while let Some(want) = header_step(&prefix)? {
        if reader.file_len().is_some_and(|l| prefix.len() as u64 >= l) {
            return Err(CogError::NotTiff("the header runs past the end of the file".into()));
        }
        let more = reader.read(prefix.len() as u64, want - prefix.len() as u64).await?;
        if more.is_empty() {
            return Err(CogError::NotTiff("the header runs past the end of the file".into()));
        }
        prefix.extend_from_slice(&more);
        requests += 1;
    }
    header_from(&prefix, requests)
}

impl CogSource {
    /// Opens a COG at a local path or an http(s) URL: reads its header
    /// (one or two requests) and nothing else. Needs a tokio runtime for a
    /// URL; see [`Self::open_blocking`] outside one.
    pub async fn open(location: &str, options: CogOptions) -> Result<Self, CogError> {
        let reader = if location.starts_with("http://") || location.starts_with("https://") {
            Self::http_reader(location, &options)?
        } else {
            let path = PathBuf::from(location);
            let len = std::fs::metadata(&path).map_err(|e| CogError::Io(format!("{}: {e}", path.display())))?.len();
            Reader::File { path, len }
        };
        Self::with_reader(reader, options).await
    }

    #[cfg(feature = "sprite_http_fetch")]
    fn http_reader(url: &str, options: &CogOptions) -> Result<Reader, CogError> {
        let client = reqwest::Client::builder()
            .user_agent(super::COG_USER_AGENT)
            .timeout(options.timeout)
            .connect_timeout(options.timeout.min(std::time::Duration::from_secs(10)))
            .build()
            .map_err(fail)?;
        Ok(Reader::Http { client, url: url.to_string(), max_whole: options.max_whole_file_bytes, whole: Mutex::new(None), len: Mutex::new(None) })
    }

    #[cfg(not(feature = "sprite_http_fetch"))]
    fn http_reader(_url: &str, _options: &CogOptions) -> Result<Reader, CogError> {
        Err(CogError::Http("no HTTP client compiled into this build (the `sprite_http_fetch` feature is off)".into()))
    }

    /// [`Self::open`], blocking on a small runtime of its own. Call it off
    /// the UI thread for a URL.
    pub fn open_blocking(location: &str, options: CogOptions) -> Result<Self, CogError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CogError::Io(format!("a runtime for the open: {e}")))?;
        runtime.block_on(Self::open(location, options))
    }

    /// Reads an in-memory COG.
    pub fn from_bytes(bytes: Vec<u8>, options: CogOptions) -> Result<Self, CogError> {
        let reader = Reader::Bytes(Arc::new(bytes));
        super::block_ready(Self::with_reader(reader, options))
    }

    async fn with_reader(reader: Reader, options: CogOptions) -> Result<Self, CogError> {
        let header = read_header(&reader).await?;
        let cog = Cog::new(header, options, reader.file_len())?;
        Ok(Self { cog: Arc::new(cog), reader: Arc::new(reader) })
    }

    /// [`Self::render_tile`] at an explicit edge.
    pub async fn render_tile_sized(&self, tile: TileId, size: u32) -> Result<Option<image::RgbaImage>, CogError> {
        self.with_tiles(tile, size, |cog, plan, have| cog.render(plan, have)).await
    }

    /// The values a ramp colours, resampled onto map tile `tile` at `size`
    /// px: `size * size` row-major, NaN outside the image and at nodata.
    /// `Ok(None)` when the tile misses the image. For an elevation readout
    /// or an export; the RGB rendering has no values and answers NaN.
    pub async fn values_tile(&self, tile: TileId, size: u32) -> Result<Option<Vec<f32>>, CogError> {
        self.with_tiles(tile, size, |cog, plan, have| cog.values(plan, have)).await
    }

    /// Plans `tile`, fetches what the cache lacks (coalesced, concurrent),
    /// then decodes and runs `finish` on the blocking pool.
    async fn with_tiles<T: Send + 'static>(
        &self,
        tile: TileId,
        size: u32,
        finish: impl FnOnce(&Cog, &super::Plan, &std::collections::HashMap<super::Key, Arc<super::Pixels>>) -> T + Send + 'static,
    ) -> Result<Option<T>, CogError> {
        let Some(plan) = self.cog.plan(tile, size) else { return Ok(None) };
        let super::Fetch { mut have, need, wait, claim } = self.cog.missing(&plan);
        let groups = self.cog.groups(&need);
        let bodies = fetch_groups(&self.reader, &groups, self.cog.options.max_concurrent).await?;
        self.cog.count_fetch(groups.len() as u64, bodies.iter().map(|b| b.len() as u64).sum());
        // Own tiles decoded first (others may be waiting on them), then
        // the ones another map tile is fetching, then the resample.
        let cog = self.cog.clone();
        let mut have = off_thread(move || cog.decode_groups(&groups, &bodies, &mut have, &claim).map(|()| have)).await?;
        super::await_shared(wait, &mut have).await?;
        let cog = self.cog.clone();
        off_thread(move || Ok(finish(&cog, &plan, &have))).await.map(Some)
    }
}

/// CPU work on tokio's blocking pool; outside a runtime (a test, a
/// `from_bytes` caller polling by hand), here.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> Result<T, CogError> + Send + 'static) -> Result<T, CogError> {
    match tokio::runtime::Handle::try_current() {
        Ok(_) => tokio::task::spawn_blocking(work).await.map_err(|e| CogError::Decode(format!("the decode task panicked: {e}")))?,
        Err(_) => work(),
    }
}

async fn fetch_groups(reader: &Reader, groups: &[RangeGroup], limit: usize) -> Result<Vec<Vec<u8>>, CogError> {
    let futures: Vec<_> = groups.iter().map(|g| reader.read(g.start, g.len)).collect();
    join_bounded(futures, limit).await.into_iter().collect()
}

#[async_trait::async_trait]
impl crate::tile_source::TileSource for CogSource {
    /// PNG of the rendered tile -- for a caller that wants bytes. The map
    /// itself takes [`Self::fetch_rgba`]'s pixels and never encodes.
    async fn fetch(&self, tile: TileId) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(image) = self.render_tile(tile).await? else { return Ok(None) };
        let mut out = Vec::new();
        image.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
        Ok(Some(out))
    }

    async fn fetch_rgba(&self, tile: TileId, tile_size: u32) -> anyhow::Result<Option<image::RgbaImage>> {
        Ok(self.render_tile_sized(tile, tile_size).await?)
    }

    fn max_zoom(&self) -> u8 {
        self.cog.meta.max_zoom
    }

    fn attribution(&self) -> Option<&str> {
        self.cog.options.attribution.as_deref()
    }
}
