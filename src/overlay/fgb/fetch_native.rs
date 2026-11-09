//! Reading a FlatGeobuf from a local file or an http(s) URL, blocking:
//! call these from a worker thread (`MapView::load_source_fgb` does), never
//! from a frame callback.
//!
//! `#[cfg(not(target_arch = "wasm32"))]` at the module declaration, for
//! `overlay::csv::fetch_native`'s reason. A file works on every non-wasm
//! shell; the URL half needs `reqwest`, which only this crate links, so it
//! is behind `sprite_http_fetch` like every other `reqwest` caller here.
//!
//! The HTTP reader is this crate's own `reqwest` client with the project
//! User-Agent, not a FlatGeobuf crate's: one TLS stack, one runtime, one
//! timeout policy for every fetch the engine makes.

use std::collections::HashSet;
use std::io::{Read as _, Seek as _, SeekFrom};

use super::{block_ready, read_source, FgbError, FgbOptions, FgbQueryStats, FgbRemote, FgbSource, RangeRead};

impl FgbSource {
    /// Reads a local FlatGeobuf. With no [`FgbOptions::bbox`] the file is
    /// read whole (refused unread past [`FgbOptions::max_bytes`]); with
    /// one, an indexed file is read by seeking to the index nodes and the
    /// features the box reaches.
    pub fn from_path(path: impl AsRef<std::path::Path>, options: &FgbOptions) -> Result<Self, FgbError> {
        let path = path.as_ref();
        let io = |e: std::io::Error| FgbError::Io(format!("{}: {e}", path.display()));
        let len = std::fs::metadata(path).map_err(io)?.len();
        if options.bbox.is_none() {
            if len > options.max_bytes {
                return Err(FgbError::TooLarge { limit: options.max_bytes, got: len });
            }
            let bytes = std::fs::read(path).map_err(io)?;
            return Self::from_bytes(&bytes, options);
        }
        let file = std::fs::File::open(path).map_err(io)?;
        let mut reader = FileReader { file, len, read: 0, requests: 0 };
        block_ready(read_source(&mut reader, options))
    }

    /// Fetches a FlatGeobuf over http(s), blocking: the whole file, or with
    /// [`FgbOptions::bbox`] only the index nodes and features the box
    /// reaches, by `Range` requests. A server that ignores `Range` (a 200)
    /// is read whole, within [`FgbOptions::max_bytes`].
    pub fn from_url(url: &str, options: &FgbOptions) -> Result<Self, FgbError> {
        let mut remote = FgbRemote::open(url, options)?;
        remote.read(options.bbox)
    }
}

impl FgbRemote {
    /// Opens a remote FlatGeobuf: one range request for the header (two
    /// when the header is over 16 KiB). Blocking.
    pub fn open(url: &str, options: &FgbOptions) -> Result<Self, FgbError> {
        let mut reader = HttpReader::new(url, options)?;
        let layout = run(super::open(&mut reader, options))?;
        Ok(Self { reader, layout, max_bytes: options.max_bytes })
    }

    /// Reads the whole file (`bbox` `None`) or one box's features.
    /// Blocking.
    pub fn read(&mut self, bbox: Option<[f64; 4]>) -> Result<FgbSource, FgbError> {
        let options = FgbOptions { bbox, max_bytes: self.max_bytes, accept_any_crs: true, ..FgbOptions::default() };
        let layout = self.layout.clone();
        let reader = &mut self.reader;
        let mut source = run(async {
            let mut rows = super::Rows::new(&layout.header);
            let mut report = super::FgbReport::new(&layout.header);
            report.warnings.extend(layout.crs_warning.clone());
            super::read_into(reader, &layout, options.bbox.as_ref(), options.max_bytes, &mut rows, &mut report).await?;
            report.finish(&rows);
            Ok::<_, FgbError>(FgbSource { rows, header: layout.header.clone(), report })
        })?;
        let (bytes, requests) = self.reader.counters();
        source.report.bytes_read = bytes;
        source.report.requests = requests;
        Ok(source)
    }

    /// The features in `bbox` (`[west, south, east, north]`) that are not
    /// in `loaded` (file indices, [`FgbSource::file_index`]): the index
    /// levels the box reaches, then the new features' byte ranges, merged.
    /// Blocking.
    pub fn query(&mut self, bbox: [f64; 4], loaded: &HashSet<u64>) -> Result<(FgbSource, FgbQueryStats), FgbError> {
        run(self.query_async(bbox, loaded))
    }
}

/// A local file by seek.
struct FileReader {
    file: std::fs::File,
    len: u64,
    read: u64,
    requests: u32,
}

impl RangeRead for FileReader {
    async fn read(&mut self, start: u64, len: u64) -> Result<Vec<u8>, FgbError> {
        let start = start.min(self.len);
        let len = len.min(self.len - start);
        let io = |e: std::io::Error| FgbError::Io(e.to_string());
        self.file.seek(SeekFrom::Start(start)).map_err(io)?;
        let mut out = vec![0; usize::try_from(len).map_err(|_| FgbError::TooLarge { limit: usize::MAX as u64, got: len })?];
        self.file.read_exact(&mut out).map_err(io)?;
        self.read += len;
        self.requests += 1;
        Ok(out)
    }
    fn file_len(&self) -> Option<u64> {
        Some(self.len)
    }
    fn counters(&self) -> (u64, u32) {
        (self.read, self.requests)
    }
}

/// The blocking runtime the fetch drives `reqwest` on, shared.
#[cfg(feature = "sprite_http_fetch")]
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("gpui-maps-fgb-fetch")
            .enable_all()
            .build()
            .expect("failed to start the FlatGeobuf fetch runtime")
    })
}

#[cfg(feature = "sprite_http_fetch")]
pub(crate) fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    runtime().block_on(future)
}

#[cfg(not(feature = "sprite_http_fetch"))]
pub(crate) fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    block_ready(future)
}

/// HTTP `Range` reads of one URL. A 206 is the range; a 200 (a server
/// that ignores `Range`) is the whole file, kept and sliced from then on.
pub(crate) struct HttpReader {
    #[cfg(feature = "sprite_http_fetch")]
    client: reqwest::Client,
    #[cfg_attr(not(feature = "sprite_http_fetch"), allow(dead_code))]
    url: String,
    #[cfg_attr(not(feature = "sprite_http_fetch"), allow(dead_code))]
    max_bytes: u64,
    len: Option<u64>,
    whole: Option<Vec<u8>>,
    read: u64,
    requests: u32,
}

impl HttpReader {
    pub(crate) fn new(url: &str, options: &FgbOptions) -> Result<Self, FgbError> {
        Ok(Self {
            #[cfg(feature = "sprite_http_fetch")]
            client: reqwest::Client::builder()
                .user_agent(super::FGB_USER_AGENT)
                .timeout(options.timeout)
                .connect_timeout(options.timeout.min(std::time::Duration::from_secs(10)))
                .build()
                .map_err(fail)?,
            url: url.to_string(),
            max_bytes: options.max_bytes,
            len: None,
            whole: None,
            read: 0,
            requests: 0,
        })
    }
}

/// A `reqwest` error, without the URL (it may carry a token) and redacted.
#[cfg(feature = "sprite_http_fetch")]
fn fail(e: reqwest::Error) -> FgbError {
    if e.is_timeout() {
        return FgbError::Timeout;
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
    FgbError::Http(crate::sprite::redact_url_credentials(&text))
}

/// `bytes 0-99/1234` -> 1234.
pub(crate) fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

fn slice(whole: &[u8], start: u64, len: u64) -> Vec<u8> {
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(whole.len());
    let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(whole.len());
    whole[start..end].to_vec()
}

/// One range request's answer, before it touches the reader's state --
/// so several can be in flight against one `&HttpReader` at once.
#[cfg(feature = "sprite_http_fetch")]
struct Reply {
    status: u16,
    total: Option<u64>,
    body: Vec<u8>,
}

#[cfg(feature = "sprite_http_fetch")]
impl HttpReader {
    /// What `read` answers without a request: a slice of a file already
    /// read whole, nothing past the end, or a range over the cap.
    fn without_request(&self, start: u64, len: u64) -> Result<Option<Vec<u8>>, FgbError> {
        if let Some(whole) = &self.whole {
            return Ok(Some(slice(whole, start, len)));
        }
        if len == 0 || self.len.is_some_and(|l| start >= l) {
            return Ok(Some(Vec::new()));
        }
        if len != u64::MAX && len > self.max_bytes {
            return Err(FgbError::TooLarge { limit: self.max_bytes, got: len });
        }
        Ok(None)
    }

    /// One `Range` request, its body read within `max_bytes`.
    async fn request(&self, start: u64, len: u64) -> Result<Reply, FgbError> {
        let range = if len == u64::MAX { format!("bytes={start}-") } else { format!("bytes={start}-{}", start + len - 1) };
        // `identity`: the client is built with gzip, and a server that
        // honours both answers a range of the GZIPPED representation
        // (GitHub Pages does: `Content-Range: .../7936003` for a 14 MB
        // file), which neither decodes alone nor lines up with the index.
        let response = self
            .client
            .get(&self.url)
            .header(reqwest::header::RANGE, range)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(fail)?;
        let status = response.status().as_u16();
        let total = match status {
            206 => response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(content_range_total),
            200 => None,
            416 => return Ok(Reply { status, total: None, body: Vec::new() }),
            other => return Err(FgbError::Http(format!("HTTP {other}"))),
        };
        let limit = self.max_bytes;
        if let Some(got) = response.content_length().filter(|&got| got > limit) {
            return Err(FgbError::TooLarge { limit, got });
        }
        let mut response = response;
        let mut body = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        while let Some(chunk) = response.chunk().await.map_err(fail)? {
            let got = (body.len() + chunk.len()) as u64;
            if got > limit {
                return Err(FgbError::TooLarge { limit, got });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Reply { status, total, body })
    }

    /// A reply into the reader's state (the length, a whole file kept to
    /// slice from, the byte count) and the bytes asked for.
    fn settle(&mut self, reply: Reply, start: u64, len: u64) -> Vec<u8> {
        let Reply { status, total, body } = reply;
        match status {
            206 => {
                if total.is_some() {
                    self.len = total;
                }
                self.read += body.len() as u64;
                body
            }
            200 => {
                self.read += body.len() as u64;
                self.len = Some(body.len() as u64);
                let out = slice(&body, start, len);
                self.whole = Some(body);
                out
            }
            _ => Vec::new(),
        }
    }
}

impl RangeRead for HttpReader {
    #[cfg(feature = "sprite_http_fetch")]
    async fn read(&mut self, start: u64, len: u64) -> Result<Vec<u8>, FgbError> {
        if let Some(bytes) = self.without_request(start, len)? {
            return Ok(bytes);
        }
        self.requests += 1;
        let reply = self.request(start, len).await?;
        Ok(self.settle(reply, start, len))
    }

    /// Concurrent, once a 206 has shown the server honours `Range` (the
    /// length is known and no whole file is held): before that, a server
    /// that ignores `Range` would send the whole file once per request.
    #[cfg(feature = "sprite_http_fetch")]
    async fn read_many(&mut self, ranges: &[(u64, u64)]) -> Result<Vec<Vec<u8>>, FgbError> {
        let mut out: Vec<Option<Vec<u8>>> = vec![None; ranges.len()];
        let mut todo = Vec::new();
        for (i, &(start, len)) in ranges.iter().enumerate() {
            match self.without_request(start, len)? {
                Some(bytes) => out[i] = Some(bytes),
                None => todo.push(i),
            }
        }
        if self.len.is_none() || todo.len() < 2 {
            for i in todo {
                out[i] = Some(self.read(ranges[i].0, ranges[i].1).await?);
            }
        } else {
            self.requests += todo.len() as u32;
            let this = &*self;
            let futures: Vec<_> = todo.iter().map(|&i| this.request(ranges[i].0, ranges[i].1)).collect();
            let replies = crate::range_fetch::join_bounded(futures, crate::range_fetch::MAX_CONCURRENT_RANGES).await;
            for (&i, reply) in todo.iter().zip(replies) {
                out[i] = Some(self.settle(reply?, ranges[i].0, ranges[i].1));
            }
        }
        Ok(out.into_iter().map(|b| b.expect("every range read")).collect())
    }

    #[cfg(not(feature = "sprite_http_fetch"))]
    async fn read(&mut self, _start: u64, _len: u64) -> Result<Vec<u8>, FgbError> {
        Err(FgbError::Http("no HTTP client compiled into this build (the `sprite_http_fetch` feature is off)".into()))
    }

    fn file_len(&self) -> Option<u64> {
        self.len
    }
    fn counters(&self) -> (u64, u32) {
        (self.read, self.requests)
    }
}
