//! The browser's FlatGeobuf reads: `window.fetch` with a `Range` header,
//! each request aborted at [`super::FgbOptions::timeout`]. The same
//! async read core as native (`super::search`, `super::read_hits`), so a
//! viewport query is the same handful of range requests in a tab.
//!
//! Compiled only for `wasm32` (the web shell includes `src/overlay/` by
//! `#[path]`), where `web-sys`, `js-sys` and `wasm-bindgen-futures` are the
//! shell's own dependencies. The browser sends its own User-Agent, and the
//! server must allow the `Range` header cross-origin (CORS) -- static hosts
//! such as GitHub Pages and S3 do.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use super::{FgbError, FgbOptions, FgbQueryStats, FgbRemote, FgbSource, RangeRead};

impl FgbSource {
    /// Fetches a FlatGeobuf in the browser: the whole file, or with
    /// [`FgbOptions::bbox`] only what the box reaches, by range requests.
    pub async fn from_url(url: &str, options: &FgbOptions) -> Result<Self, FgbError> {
        let mut remote = FgbRemote::open(url, options).await?;
        remote.read(options.bbox).await
    }
}

impl FgbRemote {
    /// Opens a remote FlatGeobuf (its header, by one range request).
    pub async fn open(url: &str, options: &FgbOptions) -> Result<Self, FgbError> {
        let mut reader = HttpReader::new(url, options);
        let layout = super::open(&mut reader, options).await?;
        Ok(Self { reader, layout, max_bytes: options.max_bytes })
    }

    /// Reads the whole file (`bbox` `None`) or one box's features.
    pub async fn read(&mut self, bbox: Option<[f64; 4]>) -> Result<FgbSource, FgbError> {
        let layout = self.layout.clone();
        let mut rows = super::Rows::new(&layout.header);
        let mut report = super::FgbReport::new(&layout.header);
        report.warnings.extend(layout.crs_warning.clone());
        super::read_into(&mut self.reader, &layout, bbox.as_ref(), self.max_bytes, &mut rows, &mut report).await?;
        report.finish(&rows);
        let (bytes, requests) = self.reader.counters();
        report.bytes_read = bytes;
        report.requests = requests;
        Ok(FgbSource { rows, header: layout.header, report })
    }

    /// The features in `bbox` not in `loaded` -- see the native `query`.
    pub async fn query(&mut self, bbox: [f64; 4], loaded: &HashSet<u64>) -> Result<(FgbSource, FgbQueryStats), FgbError> {
        self.query_async(bbox, loaded).await
    }
}

pub(crate) struct HttpReader {
    url: String,
    max_bytes: u64,
    timeout_ms: i32,
    len: Option<u64>,
    whole: Option<Vec<u8>>,
    read: u64,
    requests: u32,
}

impl HttpReader {
    /// A reader of `url` (nothing fetched yet), bounded by
    /// `options.max_bytes` and `options.timeout` per request.
    pub(crate) fn new(url: &str, options: &FgbOptions) -> Self {
        Self {
            url: url.to_string(),
            max_bytes: options.max_bytes,
            timeout_ms: i32::try_from(options.timeout.as_millis()).unwrap_or(i32::MAX),
            len: None,
            whole: None,
            read: 0,
            requests: 0,
        }
    }
}

fn slice(whole: &[u8], start: u64, len: u64) -> Vec<u8> {
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(whole.len());
    let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(whole.len());
    whole[start..end].to_vec()
}

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

    /// One `Range` fetch, aborted at the timeout: `(status, total, body)`,
    /// before it touches the reader's state -- so several can be in flight
    /// against one `&HttpReader` at once.
    async fn request(&self, start: u64, len: u64) -> Result<(u16, Option<u64>, Vec<u8>), FgbError> {
        let range = if len == u64::MAX { format!("bytes={start}-") } else { format!("bytes={start}-{}", start + len - 1) };
        let window = web_sys::window().ok_or_else(|| FgbError::Http("no window".into()))?;
        let abort = web_sys::AbortController::new().map_err(|_| FgbError::Http("no AbortController".into()))?;
        let timed_out = Rc::new(Cell::new(false));
        let fire = {
            let (abort, timed_out) = (abort.clone(), timed_out.clone());
            Closure::once(move || {
                timed_out.set(true);
                abort.abort();
            })
        };
        let timer = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(fire.as_ref().unchecked_ref(), self.timeout_ms)
            .ok();
        let got = fetch_range(&window, &self.url, &range, &abort, &timed_out).await;
        if let Some(handle) = timer {
            window.clear_timeout_with_handle(handle);
        }
        drop(fire);
        let (status, total, body) = got?;
        if body.len() as u64 > self.max_bytes {
            return Err(FgbError::TooLarge { limit: self.max_bytes, got: body.len() as u64 });
        }
        Ok((status, total, body))
    }

    /// A reply into the reader's state and the bytes asked for.
    fn settle(&mut self, reply: (u16, Option<u64>, Vec<u8>), start: u64, len: u64) -> Result<Vec<u8>, FgbError> {
        let (status, total, body) = reply;
        self.read += body.len() as u64;
        match status {
            206 => {
                if total.is_some() {
                    self.len = total;
                }
                Ok(body)
            }
            200 => {
                self.len = Some(body.len() as u64);
                let out = slice(&body, start, len);
                self.whole = Some(body);
                Ok(out)
            }
            416 => Ok(Vec::new()),
            other => Err(FgbError::Http(format!("HTTP {other}"))),
        }
    }
}

impl RangeRead for HttpReader {
    async fn read(&mut self, start: u64, len: u64) -> Result<Vec<u8>, FgbError> {
        if let Some(bytes) = self.without_request(start, len)? {
            return Ok(bytes);
        }
        self.requests += 1;
        let reply = self.request(start, len).await?;
        self.settle(reply, start, len)
    }

    /// Concurrent once a 206 has shown the server honours `Range` -- see
    /// the native reader's `read_many`.
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
                out[i] = Some(self.settle(reply?, ranges[i].0, ranges[i].1)?);
            }
        }
        Ok(out.into_iter().map(|b| b.expect("every range read")).collect())
    }
    fn file_len(&self) -> Option<u64> {
        self.len
    }
    fn counters(&self) -> (u64, u32) {
        (self.read, self.requests)
    }
}

async fn fetch_range(
    window: &web_sys::Window,
    url: &str,
    range: &str,
    abort: &web_sys::AbortController,
    timed_out: &Rc<Cell<bool>>,
) -> Result<(u16, Option<u64>, Vec<u8>), FgbError> {
    // Never the URL in the text: it may carry a token.
    let failed = |_: JsValue| if timed_out.get() { FgbError::Timeout } else { FgbError::Http("fetch failed".into()) };
    let headers = web_sys::Headers::new().map_err(failed)?;
    headers.set("Range", range).map_err(failed)?;
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_headers(&headers);
    init.set_signal(Some(&abort.signal()));
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(failed)?;
    let response: web_sys::Response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(failed)?
        .dyn_into()
        .map_err(failed)?;
    let status = response.status();
    let total = response
        .headers()
        .get("content-range")
        .ok()
        .flatten()
        .and_then(|v| v.rsplit('/').next().and_then(|t| t.trim().parse::<u64>().ok()));
    let buffer = wasm_bindgen_futures::JsFuture::from(response.array_buffer().map_err(failed)?).await.map_err(failed)?;
    Ok((status, total, js_sys::Uint8Array::new(&buffer).to_vec()))
}
