//! The native transport (`http-native`): reqwest, one client per
//! connection, run by the host's executor inside a tokio context (reqwest
//! needs one). No database engine; HTTP only (W6).

use super::{HttpRequest, HttpResponse, HttpTransport, TransportError};
use crate::BoxFuture;

/// One reqwest client. Make one per connection, so a connection's pool and
/// TLS state are its own.
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<ReqwestTransport, TransportError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("swat-maps-dash/", env!("CARGO_PKG_VERSION"), " (+https://github.com/swatmobility/gpui-maps)"))
            .build()
            .map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(ReqwestTransport { client })
    }
}

/// The error and its sources, `: `-joined. reqwest's own text is only
/// "error sending request for url (...)"; the cause a host classifies (a
/// refused connection, a certificate the TLS layer rejected) is in the
/// source chain. The text is redacted (`redact`) before anyone sees it.
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

/// A reqwest failure as the transport reports it: a timeout (while
/// connecting, sending or reading the body) is `Timeout`; anything else
/// is `Io` with the whole cause chain.
fn classify(e: reqwest::Error) -> TransportError {
    if e.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Io(chain(&e))
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| TransportError::Io(e.to_string()))?;
            let mut b = self.client.request(method, &req.url).timeout(req.timeout).body(req.body);
            for (k, v) in &req.headers {
                b = b.header(k.as_str(), v.expose());
            }
            let resp = b.send().await.map_err(classify)?;
            let status = resp.status().as_u16();
            // The timeout covers the body too: a response whose headers
            // arrived and whose body did not in time timed out; it did
            // not become unreachable.
            let body = resp.bytes().await.map_err(classify)?.to_vec();
            Ok(HttpResponse { status, body })
        })
    }
}
