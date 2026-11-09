//! Connections and sans-IO connectors (§8). Pages name connections; the
//! application registers them in code (§8.0). Every connector builds an
//! [`HttpRequest`] and decodes an [`HttpResponse`]; an [`HttpTransport`]
//! does the IO. No database engine is linked (W6).

pub mod chtypes;
pub mod clickhouse;
#[cfg(feature = "http-native")]
pub mod native;
pub mod rowbinary;
mod secret;
mod transport;
#[cfg(feature = "http-web")]
pub mod web;

pub use secret::{CredentialError, CredentialProvider, Credentials, NoCredentials, Secret, StaticCredentials};
pub use transport::{HeaderValue, HttpRequest, HttpResponse, HttpTransport, TransportError};

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::sql::{GuardOptions, DEFAULT_MAX_ROWS};

/// What a connection speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    ClickHouse,
    /// The Postgres dialect; the host supplies the transport as its own
    /// `hook::Source` (explorer-sql §3.2, D6).
    Postgres,
    Files,
}

/// How ClickHouse is told a request is read-only (§8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChReadonly {
    /// Local servers: `readonly=2`, `max_execution_time` and the row-cap
    /// settings on every request.
    SendPerRequest,
    /// A read-only server profile: only `database`, `default_format`,
    /// `query_id` and the query parameters are sent.
    ServerProfile,
    /// As [`ChReadonly::SendPerRequest`] with `readonly=1` (no setting may
    /// change in the query either): a production replica.
    Strict,
}

/// A connection the application registered (§8.0).
#[derive(Clone)]
pub struct Connection {
    pub name: String,
    pub kind: ConnectionKind,
    /// The base URL, with no user info (C2).
    pub url: String,
    pub database: Option<String>,
    /// Declared by the host; enforced by the framework (C1).
    pub read_only: bool,
    pub ch_readonly_setting: ChReadonly,
    pub timeout: Duration,
    /// The default per-request row cap (C5, D3).
    pub max_rows: u64,
    /// The highest cap a query may ask for (D3); the Explorer registers
    /// 1,000,000.
    pub max_rows_ceiling: u64,
    /// ClickHouse table functions this connection allows despite the
    /// denylist (D5). A non-secret setting.
    pub allowed_table_functions: Vec<String>,
    pub credentials: Arc<dyn CredentialProvider>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").field("name", &self.name).field("kind", &self.kind).finish_non_exhaustive()
    }
}

impl Connection {
    /// A read-only connection with the defaults: 30 s, 100,000 rows,
    /// `readonly=2` per request, no credentials.
    pub fn new(name: &str, kind: ConnectionKind, url: &str) -> Connection {
        Connection {
            name: name.to_string(),
            kind,
            url: url.trim_end_matches('/').to_string(),
            database: None,
            read_only: true,
            ch_readonly_setting: ChReadonly::SendPerRequest,
            timeout: Duration::from_secs(30),
            max_rows: DEFAULT_MAX_ROWS,
            max_rows_ceiling: DEFAULT_MAX_ROWS,
            allowed_table_functions: Vec::new(),
            credentials: Arc::new(NoCredentials),
        }
    }

    pub fn with_credentials(mut self, c: Arc<dyn CredentialProvider>) -> Self {
        self.credentials = c;
        self
    }

    pub fn guard_options(&self) -> GuardOptions {
        GuardOptions { allowed_table_functions: self.allowed_table_functions.clone() }
    }
}

/// Why a registration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    DuplicateName,
    /// The URL carries user info (`https://u:p@…`), code `credentials_in_url`.
    CredentialsInUrl,
}

/// A connection's status, never its details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    NotConfigured,
    Untested,
    Ok,
    /// The redacted reason's code.
    Failed(&'static str),
}

/// Name → connection.
#[derive(Default, Clone, Debug)]
pub struct ConnectionRegistry {
    conns: BTreeMap<String, Arc<Connection>>,
    status: BTreeMap<String, ConnectionStatus>,
}

/// True when `url` has user info in its authority.
pub fn url_has_user_info(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@')
}

impl ConnectionRegistry {
    pub fn register(&mut self, c: Connection) -> Result<(), RegisterError> {
        if url_has_user_info(&c.url) {
            return Err(RegisterError::CredentialsInUrl);
        }
        if self.conns.contains_key(&c.name) {
            return Err(RegisterError::DuplicateName);
        }
        self.status.insert(c.name.clone(), ConnectionStatus::Untested);
        self.conns.insert(c.name.clone(), Arc::new(c));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<Connection>> {
        self.conns.get(name).cloned()
    }

    /// Every registered name (aliases included), in order.
    pub fn names(&self) -> Vec<String> {
        self.conns.keys().cloned().collect()
    }

    pub fn status(&self, name: &str) -> ConnectionStatus {
        self.status.get(name).cloned().unwrap_or(ConnectionStatus::NotConfigured)
    }

    pub fn set_status(&mut self, name: &str, s: ConnectionStatus) {
        if self.conns.contains_key(name) {
            self.status.insert(name.to_string(), s);
        }
    }

    /// The text a widget shows for a connection this app does not have.
    pub fn not_configured_message(name: &str) -> String {
        format!("Connection '{name}' is not configured in this app.")
    }
}

/// A ClickHouse exception, parsed from a response body. Ported from
/// clickpad's `ch-core::error::parse_server_error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerError {
    pub code: i32,
    pub name: String,
    pub message: String,
}

/// Parses `Code: 60. DB::Exception: <message>. (UNKNOWN_TABLE) (version …)`.
/// Ported from clickpad (`ch-core/src/error.rs`), with its tests.
pub fn parse_server_error(body: &str) -> Option<ServerError> {
    let rest = body.trim().strip_prefix("Code: ")?;
    let dot = rest.find('.')?;
    let code: i32 = rest[..dot].trim().parse().ok()?;
    let after = &rest[dot + 1..];
    let message_start = after.find("DB::Exception:")? + "DB::Exception:".len();
    let mut message = after[message_start..].trim().to_string();
    // The name is the last parenthesised SCREAMING_CASE token.
    let mut name = String::new();
    while let Some(open) = message.rfind('(') {
        let close = match message[open..].find(')') {
            Some(c) => open + c,
            None => {
                message.truncate(open);
                continue;
            }
        };
        let candidate = &message[open + 1..close];
        if !candidate.is_empty() && candidate.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()) {
            name = candidate.to_string();
            message.truncate(open);
            break;
        }
        message.truncate(open);
    }
    let message = message.trim().trim_end_matches('.').trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(ServerError { code, name, message })
}

/// An unredacted connector failure. It never leaves the crate's connector
/// path: [`redact`] turns it into a [`SafeError`] first (C3).
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectorError {
    Transport(TransportError),
    /// 516 or 194: the server did not accept the credentials. The message
    /// names the user, so it is redacted like any other.
    Auth(ServerError),
    /// 497: authenticated, but not allowed this query.
    AccessDenied(ServerError),
    /// 164: a setting was rejected (a read-only profile refusing
    /// `readonly=2` or the row-cap settings).
    SettingsRejected(ServerError),
    /// 394: the query was cancelled (not a failure).
    Cancelled,
    Server(ServerError),
    /// A non-200 status with a body that is not a ClickHouse exception.
    Http { status: u16, body: String },
    Decode(rowbinary::DecodeError),
    Credentials,
    /// The query asked for more rows than the connection's ceiling (D3).
    RowCapOverCeiling,
}

impl ConnectorError {
    /// Classifies a non-200 response (the clickpad code table).
    pub fn from_response(status: u16, body: &[u8]) -> ConnectorError {
        let text = String::from_utf8_lossy(body).into_owned();
        match parse_server_error(&text) {
            Some(e) if e.code == 516 || e.code == 194 => ConnectorError::Auth(e),
            Some(e) if e.code == 497 => ConnectorError::AccessDenied(e),
            Some(e) if e.code == 164 => ConnectorError::SettingsRejected(e),
            Some(e) if e.code == 394 => ConnectorError::Cancelled,
            Some(e) => ConnectorError::Server(e),
            None => ConnectorError::Http { status, body: text },
        }
    }

    fn code(&self) -> &'static str {
        match self {
            ConnectorError::Transport(TransportError::Timeout) => "timeout",
            ConnectorError::Transport(_) => "unreachable",
            ConnectorError::Auth(_) => "auth_rejected",
            ConnectorError::AccessDenied(_) => "access_denied",
            ConnectorError::SettingsRejected(_) => "settings_rejected",
            ConnectorError::Cancelled => "cancelled",
            ConnectorError::Server(_) => "server_error",
            ConnectorError::Http { .. } => "http_error",
            ConnectorError::Decode(d) => d.code(),
            ConnectorError::Credentials => "credentials_unavailable",
            ConnectorError::RowCapOverCeiling => "max_rows_over_ceiling",
        }
    }
}

/// A connector error with every secret replaced by `***` (C3). The only
/// error form that reaches a widget, an export, the agent or a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeError {
    pub connection: String,
    pub code: &'static str,
    pub status: Option<u16>,
    pub server_code: Option<i32>,
    pub message: String,
}

impl fmt::Display for SafeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.connection, self.code, self.message)
    }
}

/// Redacts a connector error against its connection: the password, the user
/// name, the full URL and any user info are replaced by `***` (C3).
pub fn redact(e: ConnectorError, c: &Connection) -> SafeError {
    let code = e.code();
    let (status, server_code, raw) = match &e {
        ConnectorError::Transport(TransportError::Timeout) => (None, None, "The request timed out.".to_string()),
        ConnectorError::Transport(TransportError::Io(m)) => (None, None, m.clone()),
        ConnectorError::Auth(s) => (Some(401), Some(s.code), format!("The server did not accept the credentials: {}", s.message)),
        ConnectorError::AccessDenied(s) | ConnectorError::SettingsRejected(s) | ConnectorError::Server(s) => {
            (None, Some(s.code), format!("{} ({})", s.message, s.name))
        }
        ConnectorError::Cancelled => (None, Some(394), "The query was cancelled.".to_string()),
        ConnectorError::Http { status, body } => (Some(*status), None, body.chars().take(500).collect()),
        ConnectorError::Decode(d) => (None, None, format!("{d:?}")),
        ConnectorError::Credentials => (None, None, "The credentials could not be read.".to_string()),
        ConnectorError::RowCapOverCeiling => (None, None, "The row cap is above this connection's ceiling.".to_string()),
    };
    let mut needles: Vec<String> = Vec::new();
    if let Ok(Some(cr)) = c.credentials.credentials() {
        if let Some(p) = &cr.password {
            needles.push(p.expose().to_string());
        }
        if let Some(u) = &cr.user {
            needles.push(u.clone());
        }
    }
    needles.push(c.url.clone());
    if let Some((_, rest)) = c.url.split_once("://") {
        needles.push(rest.to_string());
    }
    needles.retain(|n| n.len() >= 2);
    needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let mut message = raw;
    for n in &needles {
        message = message.replace(n.as_str(), "***");
    }
    // User info in any URL that appears in the text.
    message = scrub_user_info(&message);
    SafeError { connection: c.name.clone(), code, status, server_code, message }
}

fn scrub_user_info(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c == '/' || c.is_whitespace()).unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_unknown_table_error() {
        // Ported from clickpad's ch-core error tests.
        let body = "Code: 60. DB::Exception: Unknown table expression identifier 'bar' \
                    in scope SELECT foo FROM bar. (UNKNOWN_TABLE) (version 26.2.1.587 (official build))";
        let e = parse_server_error(body).expect("should parse");
        assert_eq!(e.code, 60);
        assert_eq!(e.name, "UNKNOWN_TABLE");
        assert!(e.message.starts_with("Unknown table expression identifier 'bar'"));
        assert!(!e.message.contains("version"));
    }

    #[test]
    fn parses_a_syntax_error_whose_message_contains_parentheses() {
        let body = "Code: 62. DB::Exception: Syntax error: failed at position 8 ((): (. \
                    Unmatched parentheses: (. (SYNTAX_ERROR) (version 26.2.1.587 (official build))";
        let e = parse_server_error(body).expect("should parse");
        assert_eq!(e.code, 62);
        assert_eq!(e.name, "SYNTAX_ERROR");
        assert!(e.message.starts_with("Syntax error: failed at position 8"));
    }

    #[test]
    fn a_body_that_is_not_an_error_parses_to_none() {
        assert!(parse_server_error("1\n2\n3\n").is_none());
        assert!(parse_server_error("").is_none());
    }

    #[test]
    fn the_code_table() {
        let auth = b"Code: 516. DB::Exception: synthetic_user: Authentication failed: password is incorrect, or there is no user with such name. (AUTHENTICATION_FAILED) (version 26.2.1.587 (official build))";
        assert!(matches!(ConnectorError::from_response(401, auth), ConnectorError::Auth(_)));
        let wrong = b"Code: 194. DB::Exception: default: Authentication failed. (REQUIRED_PASSWORD)";
        assert!(matches!(ConnectorError::from_response(401, wrong), ConnectorError::Auth(_)));
        let denied = b"Code: 497. DB::Exception: default: Not enough privileges. (ACCESS_DENIED)";
        assert!(matches!(ConnectorError::from_response(403, denied), ConnectorError::AccessDenied(_)));
        let settings = b"Code: 164. DB::Exception: Cannot modify 'readonly' setting in readonly mode. (READONLY)";
        assert!(matches!(ConnectorError::from_response(500, settings), ConnectorError::SettingsRejected(_)));
        let cancelled = b"Code: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED)";
        assert_eq!(ConnectorError::from_response(500, cancelled), ConnectorError::Cancelled);
        assert!(matches!(ConnectorError::from_response(502, b"bad gateway"), ConnectorError::Http { status: 502, .. }));
    }

    #[test]
    fn registration_refuses_user_info() {
        let mut r = ConnectionRegistry::default();
        let c = Connection::new("local", ConnectionKind::ClickHouse, "http://someone:not-a-password@127.0.0.1:8123");
        assert_eq!(r.register(c), Err(RegisterError::CredentialsInUrl));
        let c = Connection::new("local", ConnectionKind::ClickHouse, "http://127.0.0.1:8123");
        assert_eq!(r.register(c.clone()), Ok(()));
        assert_eq!(r.register(c), Err(RegisterError::DuplicateName));
        assert_eq!(r.status("local"), ConnectionStatus::Untested);
        assert_eq!(r.status("other"), ConnectionStatus::NotConfigured);
        assert!(!url_has_user_info("http://127.0.0.1:8123/?q=a@b"));
    }
}
