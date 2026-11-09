//! The IO boundary: connectors build an [`HttpRequest`] and decode an
//! [`HttpResponse`]; an [`HttpTransport`] moves the bytes.

use std::fmt;
use std::time::Duration;

use super::secret::Secret;
use crate::BoxFuture;

/// A header value: plain, or a secret that prints `***`.
#[derive(Clone, PartialEq, Eq)]
pub enum HeaderValue {
    Plain(String),
    Secret(Secret),
}

impl HeaderValue {
    /// The bytes to send.
    pub fn expose(&self) -> &str {
        match self {
            HeaderValue::Plain(s) => s,
            HeaderValue::Secret(s) => s.expose(),
        }
    }
}

impl fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderValue::Plain(s) => write!(f, "{s:?}"),
            HeaderValue::Secret(_) => f.write_str("***"),
        }
    }
}

/// One request. Its URL never carries user info (C2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, HeaderValue)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v)
    }
}

/// One response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A transport failure, before any HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Timeout,
    /// Could not connect or the connection broke; the text is the
    /// transport's own and is redacted before anyone sees it.
    Io(String),
}

/// Moves bytes. `http-native` (reqwest) natively, `http-web` (`fetch`) in
/// the browser; tests use recording stubs.
pub trait HttpTransport: Send + Sync {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>>;
}
