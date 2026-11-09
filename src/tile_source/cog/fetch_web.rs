//! The browser's COG reads: `window.fetch` with a `Range` header, each
//! request aborted at [`CogOptions::timeout`], several in flight at once
//! (the same bounded join native uses). The decode and resample run on
//! the page's thread -- the browser build has no worker pool -- one map
//! tile at a time between frames.
//!
//! Compiled only for `wasm32` (the web shell includes this module by
//! `#[path]`), where `web-sys`, `js-sys` and `wasm-bindgen-futures` are the
//! shell's own dependencies. The server must allow the `Range` header
//! cross-origin (CORS): S3 and GitHub Pages do.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use super::{header_from, header_step, join_bounded, Cog, CogError, CogOptions, CogSource, RangeGroup, FIRST_HEADER_READ};
use crate::geo::TileId;

/// Where the bytes come from.
pub(crate) enum Reader {
    Bytes(Arc<Vec<u8>>),
    Http { url: String, timeout_ms: i32, max_whole: u64, whole: Mutex<Option<Arc<Vec<u8>>>>, len: Mutex<Option<u64>> },
}

fn slice(whole: &[u8], start: u64, len: u64) -> Vec<u8> {
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(whole.len());
    let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(whole.len());
    whole[start..end].to_vec()
}

impl Reader {
    fn file_len(&self) -> Option<u64> {
        match self {
            Reader::Bytes(b) => Some(b.len() as u64),
            Reader::Http { len, .. } => *len.lock().expect("len"),
        }
    }

    async fn read(&self, start: u64, len: u64) -> Result<Vec<u8>, CogError> {
        let (url, timeout_ms, max_whole, whole, known) = match self {
            Reader::Bytes(b) => return Ok(slice(b, start, len)),
            Reader::Http { url, timeout_ms, max_whole, whole, len } => (url, *timeout_ms, *max_whole, whole, len),
        };
        if let Some(w) = whole.lock().expect("whole").clone() {
            return Ok(slice(&w, start, len));
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        let range = format!("bytes={start}-{}", start + len - 1);
        let window = web_sys::window().ok_or_else(|| CogError::Http("no window".into()))?;
        let abort = web_sys::AbortController::new().map_err(|_| CogError::Http("no AbortController".into()))?;
        let timed_out = Rc::new(Cell::new(false));
        let fire = {
            let (abort, timed_out) = (abort.clone(), timed_out.clone());
            Closure::once(move || {
                timed_out.set(true);
                abort.abort();
            })
        };
        let timer = window.set_timeout_with_callback_and_timeout_and_arguments_0(fire.as_ref().unchecked_ref(), timeout_ms).ok();
        let got = fetch_range(&window, url, &range, &abort, &timed_out).await;
        if let Some(handle) = timer {
            window.clear_timeout_with_handle(handle);
        }
        drop(fire);
        let (status, total, body) = got?;
        match status {
            206 => {
                if total.is_some() {
                    *known.lock().expect("len") = total;
                }
                Ok(body)
            }
            200 => {
                if body.len() as u64 > max_whole {
                    return Err(CogError::TooLarge { limit: max_whole, got: body.len() as u64 });
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

async fn fetch_range(
    window: &web_sys::Window,
    url: &str,
    range: &str,
    abort: &web_sys::AbortController,
    timed_out: &Rc<Cell<bool>>,
) -> Result<(u16, Option<u64>, Vec<u8>), CogError> {
    // Never the URL in the text: it may carry a token.
    let failed = |_: JsValue| if timed_out.get() { CogError::Timeout } else { CogError::Http("fetch failed".into()) };
    let headers = web_sys::Headers::new().map_err(failed)?;
    headers.set("Range", range).map_err(failed)?;
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_headers(&headers);
    init.set_signal(Some(&abort.signal()));
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(failed)?;
    let response: web_sys::Response =
        wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request)).await.map_err(failed)?.dyn_into().map_err(failed)?;
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

async fn read_header(reader: &Reader) -> Result<super::Header, CogError> {
    let mut prefix = reader.read(0, FIRST_HEADER_READ).await?;
    let mut requests = 1;
    while let Some(want) = header_step(&prefix)? {
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
    /// Opens a COG at an http(s) URL in the browser: its header, by one
    /// or two range requests.
    pub async fn open(url: &str, options: CogOptions) -> Result<Self, CogError> {
        let reader = Reader::Http {
            url: url.to_string(),
            timeout_ms: i32::try_from(options.timeout.as_millis()).unwrap_or(i32::MAX),
            max_whole: options.max_whole_file_bytes,
            whole: Mutex::new(None),
            len: Mutex::new(None),
        };
        Self::with_reader(reader, options).await
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
