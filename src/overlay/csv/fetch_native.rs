//! Reading a CSV from a local file or an http(s) URL, blocking: call these
//! from a worker thread (`MapView::load_source_csv` does), never from a
//! frame callback.
//!
//! `#[cfg(not(target_arch = "wasm32"))]` at the module declaration, for
//! `overlay::dotgrid::fetch_native`'s reason: the phone compiles this crate's
//! `src/` by `#[path]`, and `target_arch` is the one gate that reads the
//! same for its device and host builds. A file read works on every
//! non-wasm shell; the URL half needs `reqwest`, which only this crate
//! links, so it is behind `sprite_http_fetch` like every other `reqwest`
//! caller here.

use super::{CsvError, CsvOptions, CsvSource};

impl CsvSource {
    /// Reads and parses a local CSV/TSV file. Refused unread when it is
    /// larger than [`CsvOptions::max_bytes`].
    pub fn from_path(path: impl AsRef<std::path::Path>, options: &CsvOptions) -> Result<Self, CsvError> {
        let path = path.as_ref();
        let io = |e: std::io::Error| CsvError::Io(format!("{}: {e}", path.display()));
        let len = std::fs::metadata(path).map_err(io)?.len();
        if len > options.max_bytes {
            return Err(CsvError::TooLarge { limit: options.max_bytes, got: len });
        }
        let bytes = std::fs::read(path).map_err(io)?;
        Self::from_bytes(&bytes, options)
    }

    /// Fetches and parses a CSV over http(s), blocking. The body is
    /// streamed and refused at the chunk that crosses
    /// [`CsvOptions::max_bytes`] (or before the first, when the server
    /// declares a larger `Content-Length`); the whole request is bounded by
    /// [`CsvOptions::timeout`].
    pub fn from_url(url: &str, options: &CsvOptions) -> Result<Self, CsvError> {
        let bytes = http_get(url, options)?;
        Self::from_bytes(&bytes, options)
    }
}

/// The blocking runtime the fetch drives `reqwest` on. Shared: a runtime's
/// worker threads are not free to stand up per request.
#[cfg(feature = "sprite_http_fetch")]
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("gpui-maps-csv-fetch")
            .enable_all()
            .build()
            .expect("failed to start the CSV fetch runtime")
    })
}

#[cfg(feature = "sprite_http_fetch")]
fn http_get(url: &str, options: &CsvOptions) -> Result<Vec<u8>, CsvError> {
    let limit = options.max_bytes;
    let timeout = options.timeout;
    // A URL may carry a token: the error text drops the URL, and what is
    // left is redacted anyway (`sprite::redact_url_credentials`).
    let fail = |e: reqwest::Error| {
        if e.is_timeout() {
            return CsvError::Timeout;
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
        CsvError::Http(crate::sprite::redact_url_credentials(&text))
    };
    let client = reqwest::Client::builder()
        .user_agent(super::CSV_USER_AGENT)
        .timeout(timeout)
        .connect_timeout(timeout.min(std::time::Duration::from_secs(10)))
        .build()
        .map_err(fail)?;
    let url = url.to_string();
    let fetch = async move {
        let response = client.get(&url).send().await.map_err(fail)?;
        let status = response.status();
        if !status.is_success() {
            return Err(CsvError::Http(format!("HTTP {}", status.as_u16())));
        }
        if let Some(got) = response.content_length().filter(|&got| got > limit) {
            return Err(CsvError::TooLarge { limit, got });
        }
        let mut response = response;
        let mut body = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        while let Some(chunk) = response.chunk().await.map_err(fail)? {
            let got = (body.len() + chunk.len()) as u64;
            if got > limit {
                return Err(CsvError::TooLarge { limit, got });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    };
    // `block_on` on a runtime of our own: the caller is a plain worker
    // thread (or tokio's blocking pool, which is not an async context).
    runtime().block_on(fetch)
}

#[cfg(not(feature = "sprite_http_fetch"))]
fn http_get(_url: &str, _options: &CsvOptions) -> Result<Vec<u8>, CsvError> {
    Err(CsvError::Http("no HTTP client compiled into this build (the `sprite_http_fetch` feature is off)".into()))
}
