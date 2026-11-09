//! Sources (§3.3), with explorer-sql D6 (`postgres`) and D3/F2 (`max_rows`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::Localised;

/// The connector a source uses (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Clickhouse,
    /// The Postgres dialect over whatever transport the host registered for
    /// the connection (D6). `postgres_http` is an accepted alias, normalised
    /// to `postgres` on load (F1): the transport is the connection's
    /// business, not the page's.
    #[serde(alias = "postgres_http")]
    Postgres,
    DuckdbHttp,
    Geoparquet,
    Csv,
    Pmtiles,
    DotgridPmtiles,
    HexPmtiles,
    ClusterPmtiles,
    Inline,
    Host,
}

impl SourceKind {
    /// The kinds that run SQL on a server through a connection.
    pub fn is_sql(self) -> bool {
        matches!(self, SourceKind::Clickhouse | SourceKind::Postgres | SourceKind::DuckdbHttp)
    }
}

/// Where a source's queries run (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Execution {
    Server,
    Client,
    Engine,
}

/// A page's privacy *claim* (§5.3). The host's registration decides; a page
/// can only make a source stricter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Privacy {
    #[default]
    Open,
    AggregateOnly,
}

/// Keys that would put connection details in a page; refused inside a SQL
/// source with `connection_details_in_page` (§8.0, F6).
pub const CONNECTION_DETAIL_KEYS: [&str; 6] = ["url", "host", "user", "password", "user_env", "password_env"];

/// One entry of a page's `sources` object (§3.3).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDecl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SourceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// File kinds only: a URL or a collection-relative path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<Execution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<Privacy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Localised>,
    /// Server sources: the per-request row cap (F2, D3). Absent means the
    /// connection's cap (100,000); above the host's ceiling is
    /// `max_rows_over_ceiling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u64>,
}

impl SourceDecl {
    /// The kind, defaulting to `clickhouse` when a connection is named.
    pub fn effective_kind(&self) -> Option<SourceKind> {
        self.kind.or(self.connection.as_ref().map(|_| SourceKind::Clickhouse))
    }

    /// The execution mode, defaulting by kind (§3.3).
    pub fn effective_execution(&self) -> Execution {
        self.execution.unwrap_or(match self.effective_kind() {
            Some(k) if k.is_sql() => Execution::Server,
            Some(SourceKind::Pmtiles | SourceKind::DotgridPmtiles | SourceKind::HexPmtiles | SourceKind::ClusterPmtiles) => {
                Execution::Engine
            }
            _ => Execution::Client,
        })
    }
}
