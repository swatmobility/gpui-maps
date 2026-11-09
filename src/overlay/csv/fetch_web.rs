//! The browser's CSV fetch: `window.fetch`, aborted at
//! [`super::CsvOptions::timeout`], the body read chunk by chunk off its
//! `ReadableStream` and refused at the chunk that crosses
//! [`super::CsvOptions::max_bytes`] -- the same rules as native's
//! `fetch_native`, and `web/src/app.rs`'s `fetch_grid` pattern.
//!
//! Compiled only for `wasm32` (the web shell includes `src/overlay/` by
//! `#[path]`), where `web-sys`, `js-sys` and `wasm-bindgen-futures` are the
//! shell's own dependencies. A browser has no filesystem, so
//! [`CsvSource::from_bytes`] and this are the two doors there.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use super::{CsvError, CsvOptions, CsvSource};

impl CsvSource {
    /// Fetches and parses a CSV in the browser. The page's own origin rules
    /// (CORS) apply; the browser sends its own User-Agent.
    pub async fn from_url(url: &str, options: &CsvOptions) -> Result<Self, CsvError> {
        let window = web_sys::window().ok_or_else(|| CsvError::Http("no window".into()))?;
        let abort = web_sys::AbortController::new().map_err(|_| CsvError::Http("no AbortController".into()))?;
        let timed_out = Rc::new(Cell::new(false));
        let fire = {
            let (abort, timed_out) = (abort.clone(), timed_out.clone());
            Closure::once(move || {
                timed_out.set(true);
                abort.abort();
            })
        };
        let ms = i32::try_from(options.timeout.as_millis()).unwrap_or(i32::MAX);
        let timer = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(fire.as_ref().unchecked_ref(), ms)
            .ok();
        let body = fetch_within(&window, url, options.max_bytes, &abort, &timed_out).await;
        if let Some(handle) = timer {
            window.clear_timeout_with_handle(handle);
        }
        drop(fire);
        Self::from_bytes(&body?, options)
    }
}

async fn fetch_within(
    window: &web_sys::Window,
    url: &str,
    limit: u64,
    abort: &web_sys::AbortController,
    timed_out: &Rc<Cell<bool>>,
) -> Result<Vec<u8>, CsvError> {
    // Never the URL in the text: it may carry a token.
    let failed = |_: JsValue| if timed_out.get() { CsvError::Timeout } else { CsvError::Http("fetch failed".into()) };
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_signal(Some(&abort.signal()));
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(failed)?;
    let response: web_sys::Response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(failed)?
        .dyn_into()
        .map_err(failed)?;
    if !response.ok() {
        abort.abort();
        return Err(CsvError::Http(format!("HTTP {}", response.status())));
    }
    let declared = response.headers().get("content-length").ok().flatten().and_then(|v| v.trim().parse::<u64>().ok());
    if let Some(got) = declared.filter(|&got| got > limit) {
        abort.abort();
        return Err(CsvError::TooLarge { limit, got });
    }
    let Some(stream) = response.body() else { return Ok(Vec::new()) };
    let reader: web_sys::ReadableStreamDefaultReader =
        stream.get_reader().dyn_into().map_err(|o: js_sys::Object| failed(o.into()))?;
    let mut body = Vec::new();
    loop {
        let chunk = wasm_bindgen_futures::JsFuture::from(reader.read()).await.map_err(failed)?;
        let done = js_sys::Reflect::get(&chunk, &JsValue::from_str("done")).ok().and_then(|d| d.as_bool()).unwrap_or(true);
        if done {
            break;
        }
        let value = js_sys::Reflect::get(&chunk, &JsValue::from_str("value")).map_err(failed)?;
        let bytes = js_sys::Uint8Array::new(&value);
        let got = body.len() as u64 + u64::from(bytes.length());
        if got > limit {
            abort.abort();
            return Err(CsvError::TooLarge { limit, got });
        }
        let at = body.len();
        body.resize(at + bytes.length() as usize, 0);
        bytes.copy_to(&mut body[at..]);
    }
    Ok(body)
}
