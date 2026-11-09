//! The phone's COG reads (`feature = "cog_ureq"`, turned on by the iOS
//! crate, which reaches this module by `#[path]`): a local file by seek,
//! bytes in memory, or an http(s) URL by blocking `Range` GETs through a
//! `ureq` agent -- the iOS shell's own HTTP client, with no tokio and no
//! `reqwest` in its graph. A server that ignores `Range` (a 200) is read
//! whole once, within [`CogOptions::max_whole_file_bytes`], and sliced
//! from then on, as on native.
//!
//! Every read completes before its future is first polled, so the async
//! core (`with_tiles`, `join_bounded`) runs to the end under a plain
//! executor (`pollster` on a fetcher worker, [`super::block_ready`] in a
//! test); a map tile's requests run one after another on that worker, and
//! the fetcher's workers give the concurrency across map tiles. Decoding
//! and the resample run on the calling thread -- never the UI thread on
//! the phone, which only ever asks through the fetcher.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{header_from, header_step, join_bounded, Cog, CogError, CogOptions, CogSource, RangeGroup, FIRST_HEADER_READ};
use crate::geo::TileId;

/// Where the bytes come from.
pub(crate) enum Reader {
    Bytes(Arc<Vec<u8>>),
    File { path: PathBuf, len: u64 },
    Http { agent: ureq::Agent, url: String, max_whole: u64, whole: Mutex<Option<Arc<Vec<u8>>>>, len: Mutex<Option<u64>> },
}

fn slice(whole: &[u8], start: u64, len: u64) -> Vec<u8> {
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(whole.len());
    let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(whole.len());
    whole[start..end].to_vec()
}

/// A transport failure, as text with no URL in it (one may carry a token).
fn fail(e: &ureq::Error) -> CogError {
    match e {
        ureq::Error::Timeout(_) => CogError::Timeout,
        other => CogError::Http(crate::sprite::redact_url_credentials(&other.to_string())),
    }
}

impl Reader {
    fn file_len(&self) -> Option<u64> {
        match self {
            Reader::Bytes(b) => Some(b.len() as u64),
            Reader::File { len, .. } => Some(*len),
            Reader::Http { len, .. } => *len.lock().expect("len"),
        }
    }

    /// `len` bytes from `start` (fewer at the end of the file). Async in
    /// name only: the read is done before the future returns.
    #[allow(clippy::unused_async)]
    async fn read(&self, start: u64, len: u64) -> Result<Vec<u8>, CogError> {
        self.read_now(start, len)
    }

    fn read_now(&self, start: u64, len: u64) -> Result<Vec<u8>, CogError> {
        match self {
            Reader::Bytes(b) => Ok(slice(b, start, len)),
            Reader::File { path, len: file_len } => {
                let start = start.min(*file_len);
                let len = len.min(file_len - start);
                let io = |e: std::io::Error| CogError::Io(e.to_string());
                let mut file = std::fs::File::open(path).map_err(io)?;
                file.seek(SeekFrom::Start(start)).map_err(io)?;
                let mut out = vec![0; usize::try_from(len).unwrap_or(usize::MAX)];
                file.read_exact(&mut out).map_err(io)?;
                Ok(out)
            }
            Reader::Http { agent, url, max_whole, whole, len: known } => {
                if let Some(w) = whole.lock().expect("whole").clone() {
                    return Ok(slice(&w, start, len));
                }
                if len == 0 || known.lock().expect("len").is_some_and(|l| start >= l) {
                    return Ok(Vec::new());
                }
                let range = format!("bytes={start}-{}", start + len - 1);
                let mut response = agent
                    .get(url)
                    .header("Range", &range)
                    // A byte range of the file as stored, never of a
                    // compressed representation.
                    .header("Accept-Encoding", "identity")
                    .call()
                    .map_err(|e| fail(&e))?;
                let status = response.status().as_u16();
                match status {
                    206 => {
                        if let Some(total) = response
                            .headers()
                            .get("content-range")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.rsplit('/').next()?.trim().parse::<u64>().ok())
                        {
                            *known.lock().expect("len") = Some(total);
                        }
                        response.body_mut().with_config().limit(len.saturating_add(1)).read_to_vec().map_err(|e| fail(&e))
                    }
                    200 => {
                        let body = response.body_mut().with_config().limit(*max_whole).read_to_vec().map_err(|e| match e {
                            ureq::Error::BodyExceedsLimit(_) => CogError::TooLarge { limit: *max_whole, got: max_whole.saturating_add(1) },
                            other => fail(&other),
                        })?;
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
    /// (one or two requests) and nothing else. Blocking: call it off the
    /// UI thread for a URL.
    pub async fn open(location: &str, options: CogOptions) -> Result<Self, CogError> {
        let reader = if location.starts_with("http://") || location.starts_with("https://") {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .user_agent(super::COG_USER_AGENT)
                .timeout_global(Some(options.timeout))
                .timeout_connect(Some(options.timeout.min(std::time::Duration::from_secs(10))))
                .http_status_as_error(false)
                .build()
                .into();
            Reader::Http { agent, url: location.to_string(), max_whole: options.max_whole_file_bytes, whole: Mutex::new(None), len: Mutex::new(None) }
        } else {
            let path = PathBuf::from(location);
            let len = std::fs::metadata(&path).map_err(|e| CogError::Io(format!("{}: {e}", path.display())))?.len();
            Reader::File { path, len }
        };
        Self::with_reader(reader, options).await
    }

    /// [`Self::open`], run to its end here.
    pub fn open_blocking(location: &str, options: CogOptions) -> Result<Self, CogError> {
        super::block_ready(Self::open(location, options))
    }

    /// Reads an in-memory COG.
    pub fn from_bytes(bytes: Vec<u8>, options: CogOptions) -> Result<Self, CogError> {
        super::block_ready(Self::with_reader(Reader::Bytes(Arc::new(bytes)), options))
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

    /// The values a ramp colours on map tile `tile` -- see the native
    /// `values_tile`.
    pub async fn values_tile(&self, tile: TileId, size: u32) -> Result<Option<Vec<f32>>, CogError> {
        self.with_tiles(tile, size, |cog, plan, have| cog.values(plan, have)).await
    }

    /// Plans `tile`, fetches what the cache lacks (coalesced), decodes,
    /// then runs `finish`, all on the calling thread.
    async fn with_tiles<T>(
        &self,
        tile: TileId,
        size: u32,
        finish: impl FnOnce(&Cog, &super::Plan, &std::collections::HashMap<super::Key, Arc<super::Pixels>>) -> T,
    ) -> Result<Option<T>, CogError> {
        let Some(plan) = self.cog.plan(tile, size) else { return Ok(None) };
        let super::Fetch { mut have, need, wait, claim } = self.cog.missing(&plan);
        let groups = self.cog.groups(&need);
        let bodies = fetch_groups(&self.reader, &groups, self.cog.options.max_concurrent).await?;
        self.cog.count_fetch(groups.len() as u64, bodies.iter().map(|b| b.len() as u64).sum());
        self.cog.decode_groups(&groups, &bodies, &mut have, &claim)?;
        super::await_shared(wait, &mut have).await?;
        Ok(Some(finish(&self.cog, &plan, &have)))
    }
}

async fn fetch_groups(reader: &Reader, groups: &[RangeGroup], limit: usize) -> Result<Vec<Vec<u8>>, CogError> {
    let futures: Vec<_> = groups.iter().map(|g| reader.read(g.start, g.len)).collect();
    join_bounded(futures, limit).await.into_iter().collect()
}
