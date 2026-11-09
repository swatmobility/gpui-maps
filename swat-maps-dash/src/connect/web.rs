//! The browser transport (`http-web`): `fetch`. A browser connection points
//! at a same-origin proxy that adds auth server-side, so its requests carry
//! no auth header (C4): [`FetchTransport`] drops any it is handed.

use super::{HttpRequest, HttpResponse, HttpTransport, TransportError};
use crate::BoxFuture;

/// Headers a browser request never carries (C4).
pub const AUTH_HEADERS: [&str; 3] = ["authorization", "x-clickhouse-user", "x-clickhouse-key"];

/// Removes every auth header: what reaches `fetch`.
pub fn strip_auth(mut req: HttpRequest) -> HttpRequest {
    req.headers.retain(|(k, _)| !AUTH_HEADERS.iter().any(|a| k.eq_ignore_ascii_case(a)));
    req
}

/// `window.fetch`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchTransport;

#[cfg(target_arch = "wasm32")]
impl HttpTransport for FetchTransport {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        use wasm_bindgen::JsCast;
        Box::pin(async move {
            let req = strip_auth(req);
            let err = |e: wasm_bindgen::JsValue| TransportError::Io(format!("{e:?}"));
            let init = web_sys::RequestInit::new();
            init.set_method(req.method);
            let body = js_sys::Uint8Array::from(req.body.as_slice());
            init.set_body(&body);
            let headers = web_sys::Headers::new().map_err(err)?;
            for (k, v) in &req.headers {
                headers.set(k, v.expose()).map_err(err)?;
            }
            init.set_headers(&headers);
            let request = web_sys::Request::new_with_str_and_init(&req.url, &init).map_err(err)?;
            let window = web_sys::window().ok_or_else(|| TransportError::Io("no window".into()))?;
            let resp = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request)).await.map_err(err)?;
            let resp: web_sys::Response = resp.dyn_into().map_err(err)?;
            let status = resp.status();
            let buf = wasm_bindgen_futures::JsFuture::from(resp.array_buffer().map_err(err)?).await.map_err(err)?;
            Ok(HttpResponse { status, body: js_sys::Uint8Array::new(&buf).to_vec() })
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HttpTransport for FetchTransport {
    fn send(&self, _req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async { Err(TransportError::Io("fetch is only available in the browser".into())) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::clickhouse::{ClickHouse, QueryId, Statement};
    use crate::connect::{Connection, ConnectionKind, Credentials, Secret, StaticCredentials};
    use crate::sql::{CompiledQuery, Dialect};
    use std::sync::Arc;

    #[test]
    fn a_browser_request_carries_no_auth_header() {
        // Even a connection wrongly given credentials sends none through fetch.
        let conn = Connection::new("proxy", ConnectionKind::ClickHouse, "https://dash.example.invalid/ch").with_credentials(Arc::new(
            StaticCredentials(Credentials { user: Some("synthetic".into()), password: Some(Secret::new("synthetic-pw")) }),
        ));
        let q = CompiledQuery::forced_for_test("SELECT 1", Dialect::ClickHouse);
        let req = ClickHouse::request(&conn, Statement::Query(&q), &QueryId::next()).unwrap();
        assert!(req.header("X-ClickHouse-Key").is_some());
        let req = strip_auth(req);
        for h in AUTH_HEADERS {
            assert!(req.header(h).is_none(), "{h}");
        }
    }
}
