//! The ClickHouse connector, sans-IO (§8.2, explorer-sql §3.1, D2, D3, D8).
//!
//! [`ClickHouse::send`] is the single send path (C1): every statement,
//! user or internal, is read through [`CompiledQuery::guarded_with`], so
//! the read guard runs before a request is built and a refused statement
//! never reaches the transport. The two exceptions are fixed statements
//! built here: [`ClickHouse::cancel`]'s `KILL QUERY` with a bound query id
//! (D8), and [`ClickHouse::grants`]' `SHOW GRANTS` (explorer-sql P3: what
//! the account may do, information only).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::rowbinary::{self, DecodeError};
use super::{
    redact, ChReadonly, Connection, ConnectorError, HeaderValue, HttpRequest, HttpResponse, HttpTransport, SafeError,
};
use crate::hook::{RawTable, Schema, Source, SourceError};
use crate::sql::{CompiledQuery, Dialect, Param, Scalar};
use crate::BoxFuture;

/// The query id every request carries, so it can be cancelled (D8).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueryId(String);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl QueryId {
    /// A fresh id: `dash-<pid>-<nanos>-<counter>` natively, `dash-<counter>`
    /// in the browser. Only `[a-z0-9-]`.
    pub fn next() -> QueryId {
        let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
            QueryId(format!("dash-{}-{nanos}-{n}", std::process::id()))
        }
        #[cfg(target_arch = "wasm32")]
        {
            QueryId(format!("dash-{n}"))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What to send.
#[derive(Debug, Clone, Copy)]
pub enum Statement<'a> {
    /// A compiled query; its rows are read up to `max_rows`.
    Query(&'a CompiledQuery),
    /// `DESCRIBE (query)` with the same params, for the shape check.
    Describe(&'a CompiledQuery),
    /// `SELECT getSetting('readonly')`, information only.
    ReadonlyProbe,
    /// `SELECT version()`: a host's Test connection (explorer-sql P2 2.4).
    /// Sent with the connection's read-only settings, so a server profile
    /// that refuses them answers 164 here first.
    Version,
    /// `SELECT 1` on the connection's database: reaches it, or answers
    /// `UNKNOWN_DATABASE` (81).
    Ping,
}

/// Why `send` produced no table: the guard refused (zero requests sent), or
/// the connector failed (redacted).
#[derive(Debug, Clone, PartialEq)]
pub enum SendError {
    Refused(crate::hook::Refusal),
    Failed(SafeError),
}

/// What `SELECT getSetting('readonly')` said (§8.2): information only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerReadonly {
    Yes,
    No,
    /// The probe was refused or failed (`readonly_probe_refused`).
    Unknown,
}

/// What `SHOW GRANTS` says this account may do (explorer-sql P3 addition).
/// Information only, like [`ServerReadonly`]: the Studio sends read-only
/// requests whatever it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grants {
    /// Every privilege granted is a read (`SELECT`, `SHOW …`, …).
    SelectOnly,
    /// Some grant is a write or `ALL` (`INSERT`, `ALTER`, `CREATE`, `DROP`,
    /// `TRUNCATE`, `OPTIMIZE`, `SYSTEM`, `ALL`, …).
    CanWrite,
    /// The account may not see its grants, or they arrive only through
    /// roles the server did not expand.
    Unknown,
}

/// Privilege words that change data or schema. A grant naming any of
/// them (or `ALL`) is a write grant.
const WRITE_PRIVILEGES: [&str; 10] = ["ALL", "INSERT", "ALTER", "CREATE", "DROP", "TRUNCATE", "OPTIMIZE", "SYSTEM", "UNDROP", "BACKUP"];

/// Classifies `SHOW GRANTS` output lines (`GRANT SELECT ON db.* TO u`,
/// `GRANT role TO u`, `REVOKE …`). Pure, so the stub cases test it.
pub fn classify_grants(lines: &[String]) -> Grants {
    let mut privileged = false;
    let mut role_only = false;
    for line in lines {
        let l = line.trim();
        let upper = l.to_ascii_uppercase();
        let Some(rest) = upper.strip_prefix("GRANT ") else { continue };
        let Some(on) = rest.find(" ON ") else {
            // `GRANT role TO user`: what the role holds is not shown.
            role_only = true;
            continue;
        };
        privileged = true;
        // Column lists `SELECT(a, b)` hold no privilege words.
        let mut privs = String::new();
        let mut depth = 0;
        for ch in rest[..on].chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ if depth == 0 => privs.push(ch),
                _ => {}
            }
        }
        for p in privs.split(',') {
            let first = p.split_whitespace().next().unwrap_or("");
            if WRITE_PRIVILEGES.contains(&first) {
                return Grants::CanWrite;
            }
        }
    }
    if privileged && !role_only {
        Grants::SelectOnly
    } else {
        Grants::Unknown
    }
}

/// What a cancel achieved. A refused kill is never a confirmed cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The server accepted the kill for a running query.
    Requested,
    /// The server found no such query (it had finished).
    NotRunning,
    /// The server refused the kill or could not be reached.
    Refused(SafeError),
}

fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Per-request read limits and the query cache ([`ClickHouse::request_tuned`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tuning {
    /// `use_query_cache=1`: a repeated tile answers from the server's cache.
    pub use_query_cache: bool,
    /// `max_rows_to_read`: the server stops a query that would read more.
    pub max_rows_to_read: Option<u64>,
    /// `max_bytes_to_read`.
    pub max_bytes_to_read: Option<u64>,
}

/// The ClickHouse connector.
pub struct ClickHouse;

impl ClickHouse {
    /// Builds the request for a statement (no IO). The guard runs here.
    pub fn request(conn: &Connection, stmt: Statement<'_>, id: &QueryId) -> Result<HttpRequest, SendError> {
        Self::request_tuned(conn, stmt, id, &Tuning::default())
    }

    /// [`ClickHouse::request`] with per-request read limits and the
    /// server's query cache (the Explorer's live tiles). A closed set of
    /// settings, never a free key; sent only where the other per-request
    /// settings are (a server profile refuses settings changes).
    pub fn request_tuned(conn: &Connection, stmt: Statement<'_>, id: &QueryId, tuning: &Tuning) -> Result<HttpRequest, SendError> {
        let internal;
        let described;
        let (q, read_rows) = match stmt {
            Statement::Query(q) => (q, true),
            Statement::Describe(q) => {
                described = q.describe();
                (&described, false)
            }
            Statement::ReadonlyProbe => {
                internal = CompiledQuery::internal("SELECT getSetting('readonly') AS readonly", Dialect::ClickHouse);
                (&internal, false)
            }
            Statement::Version => {
                internal = CompiledQuery::internal("SELECT version() AS version", Dialect::ClickHouse);
                (&internal, false)
            }
            Statement::Ping => {
                internal = CompiledQuery::internal("SELECT 1 AS ok", Dialect::ClickHouse);
                (&internal, false)
            }
        };
        if q.max_rows() > conn.max_rows_ceiling {
            return Err(SendError::Failed(redact(ConnectorError::RowCapOverCeiling, conn)));
        }
        let g = q.guarded_with(Dialect::ClickHouse, &conn.guard_options()).map_err(SendError::Refused)?;
        let mut query: Vec<(String, String)> = Vec::new();
        if let Some(db) = &conn.database {
            query.push(("database".into(), db.clone()));
        }
        query.push(("default_format".into(), "RowBinaryWithNamesAndTypes".into()));
        query.push(("query_id".into(), id.0.clone()));
        let probe = matches!(stmt, Statement::ReadonlyProbe);
        if matches!(conn.ch_readonly_setting, ChReadonly::SendPerRequest | ChReadonly::Strict) && !probe {
            if conn.read_only {
                let level = if conn.ch_readonly_setting == ChReadonly::Strict { "1" } else { "2" };
                query.push(("readonly".into(), level.into()));
            }
            query.push(("max_execution_time".into(), conn.timeout.as_secs().max(1).to_string()));
            if read_rows {
                query.push(("max_result_rows".into(), (q.max_rows() + 1).to_string()));
                query.push(("result_overflow_mode".into(), "break".into()));
                query.push(("cancel_http_readonly_queries_on_client_close".into(), "1".into()));
            }
            if tuning.use_query_cache {
                query.push(("use_query_cache".into(), "1".into()));
            }
            if let Some(n) = tuning.max_rows_to_read {
                query.push(("max_rows_to_read".into(), n.to_string()));
            }
            if let Some(n) = tuning.max_bytes_to_read {
                query.push(("max_bytes_to_read".into(), n.to_string()));
            }
        }
        for Param { name, value, .. } in g.params() {
            query.push((format!("param_{name}"), value.clickhouse_text()));
        }
        let qs: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect();
        let url = format!("{}/?{}", conn.url, qs.join("&"));
        let mut headers = Vec::new();
        match conn.credentials.credentials() {
            Ok(Some(c)) => {
                if let Some(u) = c.user {
                    headers.push(("X-ClickHouse-User".to_string(), HeaderValue::Plain(u)));
                }
                if let Some(p) = c.password {
                    headers.push(("X-ClickHouse-Key".to_string(), HeaderValue::Secret(p)));
                }
            }
            Ok(None) => {}
            Err(_) => return Err(SendError::Failed(redact(ConnectorError::Credentials, conn))),
        }
        Ok(HttpRequest { method: "POST", url, headers, body: g.sql().as_bytes().to_vec(), timeout: conn.timeout })
    }

    /// Decodes a response to a query (rows up to the cap, `truncated` past it).
    pub fn decode(conn: &Connection, max_rows: u64, resp: &HttpResponse) -> Result<RawTable, SafeError> {
        if resp.status != 200 {
            return Err(redact(ConnectorError::from_response(resp.status, &resp.body), conn));
        }
        rowbinary::decode(&resp.body, max_rows).map_err(|e: DecodeError| redact(ConnectorError::Decode(e), conn))
    }

    /// The single send path (C1): guard, build, send, decode.
    pub async fn send(conn: &Connection, stmt: Statement<'_>, t: &dyn HttpTransport) -> Result<RawTable, SendError> {
        let id = QueryId::next();
        let req = Self::request(conn, stmt, &id)?;
        let cap = match stmt {
            Statement::Query(q) => q.max_rows(),
            _ => u64::MAX,
        };
        let resp = t.send(req).await.map_err(|e| SendError::Failed(redact(ConnectorError::Transport(e), conn)))?;
        Self::decode(conn, cap, &resp).map_err(SendError::Failed)
    }

    /// The read-only probe (§8.2): information only, never gating.
    pub async fn readonly_probe(conn: &Connection, t: &dyn HttpTransport) -> ServerReadonly {
        match Self::send(conn, Statement::ReadonlyProbe, t).await {
            Ok(table) => match table.cell(0, 0) {
                crate::hook::Cell::Value(v) => match v.as_f64().or_else(|| v.to_text().parse().ok()) {
                    Some(0.0) => ServerReadonly::No,
                    Some(_) => ServerReadonly::Yes,
                    None => ServerReadonly::Unknown,
                },
                _ => ServerReadonly::Unknown,
            },
            Err(_) => ServerReadonly::Unknown,
        }
    }

    /// The server's version (`SELECT version()`), for a host's Test
    /// connection. Every failure is the redacted one `send` gives.
    pub async fn version(conn: &Connection, t: &dyn HttpTransport) -> Result<String, SendError> {
        let table = Self::send(conn, Statement::Version, t).await?;
        Ok(match table.cell(0, 0) {
            crate::hook::Cell::Value(v) => v.to_text(),
            _ => String::new(),
        })
    }

    /// `SELECT 1` on the connection's database (a host's Test connection).
    pub async fn ping(conn: &Connection, t: &dyn HttpTransport) -> Result<(), SendError> {
        Self::send(conn, Statement::Ping, t).await.map(|_| ())
    }

    /// The result's columns before any row is read, or `None` when the
    /// server refused `DESCRIBE` (`describe_refused`).
    pub async fn describe(conn: &Connection, q: &CompiledQuery, t: &dyn HttpTransport) -> Option<Vec<(String, String)>> {
        let table = Self::send(conn, Statement::Describe(q), t).await.ok()?;
        let n = table.columns().first()?;
        let ty = table.columns().get(1)?;
        Some(
            (0..table.row_count())
                .filter_map(|r| match (n.cell(r), ty.cell(r)) {
                    (crate::hook::Cell::Value(Scalar::Str(a)), crate::hook::Cell::Value(Scalar::Str(b))) => Some((a, b)),
                    _ => None,
                })
                .collect(),
        )
    }

    /// The fixed cancel request (D8): `KILL QUERY WHERE query_id =
    /// {qid:String} ASYNC`, the id bound as a parameter. It is the only
    /// statement built outside the user-SQL guard, and it carries no
    /// `readonly` setting (a read-only session may not kill).
    pub fn cancel_request(conn: &Connection, id: &QueryId) -> Result<HttpRequest, SafeError> {
        let mut query = vec![
            ("default_format".to_string(), "TabSeparated".to_string()),
            ("param_qid".to_string(), id.0.clone()),
        ];
        if let Some(db) = &conn.database {
            query.insert(0, ("database".into(), db.clone()));
        }
        let qs: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect();
        let mut headers = Vec::new();
        match conn.credentials.credentials() {
            Ok(Some(c)) => {
                if let Some(u) = c.user {
                    headers.push(("X-ClickHouse-User".to_string(), HeaderValue::Plain(u)));
                }
                if let Some(p) = c.password {
                    headers.push(("X-ClickHouse-Key".to_string(), HeaderValue::Secret(p)));
                }
            }
            Ok(None) => {}
            Err(_) => return Err(redact(ConnectorError::Credentials, conn)),
        }
        Ok(HttpRequest {
            method: "POST",
            url: format!("{}/?{}", conn.url, qs.join("&")),
            headers,
            body: b"KILL QUERY WHERE query_id = {qid:String} ASYNC".to_vec(),
            timeout: conn.timeout,
        })
    }

    /// The fixed grants request: `SHOW GRANTS FINAL` (roles expanded) or,
    /// for a server that does not know `FINAL`, `SHOW GRANTS`. Like the
    /// cancel, a constant statement outside the user-SQL guard (which
    /// admits only `SELECT`/`WITH`); it sends no settings, so a read-only
    /// server profile answers it.
    pub fn grants_request(conn: &Connection, id: &QueryId, expand_roles: bool) -> Result<HttpRequest, SafeError> {
        let query = [("default_format", "RowBinaryWithNamesAndTypes".to_string()), ("query_id", id.0.clone())];
        let qs: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect();
        let mut headers = Vec::new();
        match conn.credentials.credentials() {
            Ok(Some(c)) => {
                if let Some(u) = c.user {
                    headers.push(("X-ClickHouse-User".to_string(), HeaderValue::Plain(u)));
                }
                if let Some(p) = c.password {
                    headers.push(("X-ClickHouse-Key".to_string(), HeaderValue::Secret(p)));
                }
            }
            Ok(None) => {}
            Err(_) => return Err(redact(ConnectorError::Credentials, conn)),
        }
        let body: &[u8] = if expand_roles { b"SHOW GRANTS FINAL" } else { b"SHOW GRANTS" };
        Ok(HttpRequest { method: "POST", url: format!("{}/?{}", conn.url, qs.join("&")), headers, body: body.to_vec(), timeout: conn.timeout })
    }

    /// What the account's grants allow (information only). `SHOW GRANTS
    /// FINAL` first; a server error (an older server's syntax) retries
    /// once without `FINAL`. Any other failure is `Unknown`.
    /// *Uncertain:* `FINAL` is read from ClickHouse's documentation, not
    /// checked against a live server here.
    pub async fn grants(conn: &Connection, t: &dyn HttpTransport) -> Grants {
        for expand in [true, false] {
            let Ok(req) = Self::grants_request(conn, &QueryId::next(), expand) else { return Grants::Unknown };
            let Ok(resp) = t.send(req).await else { return Grants::Unknown };
            match Self::decode(conn, u64::MAX, &resp) {
                Ok(table) => {
                    let lines: Vec<String> = (0..table.row_count())
                        .filter_map(|r| match table.cell(r, 0) {
                            crate::hook::Cell::Value(v) => Some(v.to_text()),
                            _ => None,
                        })
                        .collect();
                    return classify_grants(&lines);
                }
                Err(e) if expand && e.code == "server_error" => continue,
                Err(_) => return Grants::Unknown,
            }
        }
        Grants::Unknown
    }

    /// Cancels a running query by id (D8). Only a 200 that lists the query
    /// is `Requested`; a refusal is never reported as a cancel.
    pub async fn cancel(conn: &Connection, id: &QueryId, t: &dyn HttpTransport) -> CancelOutcome {
        let req = match Self::cancel_request(conn, id) {
            Ok(r) => r,
            Err(e) => return CancelOutcome::Refused(e),
        };
        match t.send(req).await {
            Ok(r) if r.status == 200 => {
                if r.body.iter().any(|b| !b.is_ascii_whitespace()) {
                    CancelOutcome::Requested
                } else {
                    CancelOutcome::NotRunning
                }
            }
            Ok(r) => CancelOutcome::Refused(redact(ConnectorError::from_response(r.status, &r.body), conn)),
            Err(e) => CancelOutcome::Refused(redact(ConnectorError::Transport(e), conn)),
        }
    }
}

/// A ClickHouse table on a registered connection, as a hook `Source`.
pub struct ClickHouseSource {
    pub connection: Arc<Connection>,
    pub schema: Schema,
}

impl Source for ClickHouseSource {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn execute<'a>(&'a self, q: &'a CompiledQuery, t: &'a dyn HttpTransport) -> BoxFuture<'a, Result<RawTable, SourceError>> {
        Box::pin(async move {
            ClickHouse::send(&self.connection, Statement::Query(q), t).await.map_err(|e| match e {
                SendError::Refused(r) => SourceError::Refused(r),
                SendError::Failed(f) => SourceError::Failed(f),
            })
        })
    }

    fn describe<'a>(
        &'a self,
        q: &'a CompiledQuery,
        t: &'a dyn HttpTransport,
    ) -> BoxFuture<'a, Result<Option<Vec<(String, String)>>, SourceError>> {
        Box::pin(async move { Ok(ClickHouse::describe(&self.connection, q, t).await) })
    }
}

/// A transport error as the cancelled outcome, for hosts that drop a
/// request: ClickHouse's 394 and a dropped request both read "cancelled".
pub fn is_cancelled(e: &SafeError) -> bool {
    e.code == "cancelled"
}

#[cfg(test)]
mod tests;
