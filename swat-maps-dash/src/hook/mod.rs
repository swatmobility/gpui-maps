//! The mandatory result hook (§5). No path from a row to a consumer skips
//! it: a source is registered only with a hook ([`GuardedSource::new`]),
//! and [`run_query`] is the only maker of [`Results`].

mod refusal;
pub mod reference;
mod results;
mod table;

pub use refusal::{Refusal, RuleId, SchemaColumn, Threshold, VarKey};
pub use results::{ColumnRef, Results};
pub use table::{Bitmap, Cell, Column, ColumnData, RawTable, StrColumn};

pub use crate::spec::Privacy;

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::connect::{HttpTransport, SafeError};
use crate::sql::CompiledQuery;
use crate::BoxFuture;

/// What a source declares about its columns (§5.3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    /// Declared columns and their types.
    pub columns: Vec<(String, String)>,
    /// Columns no consumer may see (a card id). A hook refuses any query
    /// that projects one.
    pub forbidden_columns: Vec<String>,
}

/// Why a source failed. Connector errors arrive redacted (C3).
#[derive(Debug, Clone, PartialEq)]
pub enum SourceError {
    Refused(Refusal),
    Failed(SafeError),
}

/// A data source. It returns a [`RawTable`] and never sees [`Results`] (C6).
pub trait Source: Send + Sync {
    fn schema(&self) -> &Schema;

    fn execute<'a>(&'a self, q: &'a CompiledQuery, t: &'a dyn HttpTransport) -> BoxFuture<'a, Result<RawTable, SourceError>>;

    /// The result's column names and types before any row is read
    /// (`DESCRIBE (query)`, §5.3), or `None` when the source cannot say.
    fn describe<'a>(
        &'a self,
        _q: &'a CompiledQuery,
        _t: &'a dyn HttpTransport,
    ) -> BoxFuture<'a, Result<Option<Vec<(String, String)>>, SourceError>> {
        Box::pin(async { Ok(None) })
    }
}

/// A predicate a hook injects into every server query on its source.
/// *Phase 1:* injection is not built yet, so [`run_query`] refuses any
/// query whose hook returns one (fail-closed, `ScopeUnavailable`).
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub column: String,
    pub value: crate::sql::Scalar,
}

/// The host's privacy rules for one source (§5.1).
pub trait ResultHook: Send + Sync {
    /// Before anything runs: refuse a query shape.
    fn check(&self, shape: &QueryShape) -> Result<(), Refusal>;
    /// After it runs: suppress cells, drop columns. Must not add rows.
    fn apply(&self, shape: &QueryShape, raw: RawTable) -> Result<RawTable, Refusal>;
    /// A predicate for every server query on this source, fail-closed.
    fn scope(&self, _shape: &QueryShape) -> Result<Option<Predicate>, Refusal> {
        Ok(None)
    }
    fn allows_reaggregation(&self) -> bool {
        false
    }
    /// What a suppressed cell reads as ("under 10"), per locale.
    fn suppressed_label(&self, locale: &str) -> String;
}

/// The explicit "no rules" hook, for public data. Visible in review.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHook;

impl ResultHook for NoHook {
    fn check(&self, _shape: &QueryShape) -> Result<(), Refusal> {
        Ok(())
    }
    fn apply(&self, _shape: &QueryShape, raw: RawTable) -> Result<RawTable, Refusal> {
        Ok(raw)
    }
    fn allows_reaggregation(&self) -> bool {
        true
    }
    fn suppressed_label(&self, locale: &str) -> String {
        match refusal::Lang::of(locale) {
            refusal::Lang::En => "hidden",
            refusal::Lang::Ja => "非表示",
            refusal::Lang::Vi => "đã ẩn",
            refusal::Lang::Th => "ซ่อนไว้",
            refusal::Lang::Id => "disembunyikan",
            refusal::Lang::Zh => "已隐藏",
        }
        .to_string()
    }
}

/// A source with its hook and privacy class. There is no way to register a
/// source without one.
///
/// ```compile_fail
/// // A bare Source is not a GuardedSource.
/// fn f(r: &mut swat_maps_dash::hook::SourceRegistry, s: std::sync::Arc<dyn swat_maps_dash::hook::Source>) {
///     r.register("x", s);
/// }
/// ```
#[derive(Clone)]
pub struct GuardedSource {
    source: Arc<dyn Source>,
    hook: Arc<dyn ResultHook>,
    privacy: Privacy,
    thresholds: BTreeMap<String, u64>,
    rules: Vec<String>,
}

impl GuardedSource {
    pub fn new(source: Arc<dyn Source>, hook: Arc<dyn ResultHook>, privacy: Privacy) -> Self {
        GuardedSource { source, hook, privacy, thresholds: BTreeMap::new(), rules: Vec::new() }
    }

    /// Declares a threshold constant the hook may cite in a refusal.
    pub fn threshold(mut self, name: &str, value: u64) -> Self {
        self.thresholds.insert(name.to_string(), value);
        self
    }

    /// Declares a rule id (`[a-z0-9_]{1,40}`) the hook may cite. An id
    /// that does not match is ignored.
    pub fn rule(mut self, id: &str) -> Self {
        if refusal::is_rule_id(id) {
            self.rules.push(id.to_string());
        }
        self
    }

    pub fn privacy(&self) -> Privacy {
        self.privacy
    }
}

/// Named sources. [`SourceRegistry::register`] takes a [`GuardedSource`]
/// only.
#[derive(Clone, Default)]
pub struct SourceRegistry {
    sources: BTreeMap<String, GuardedSource>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: &str, source: GuardedSource) {
        self.sources.insert(name.to_string(), source);
    }

    pub fn get(&self, name: &str) -> Option<&GuardedSource> {
        self.sources.get(name)
    }
}

/// What a hook sees of a query: the source, its privacy, its declared and
/// output columns, and the constants and rules declared at registration.
#[derive(Debug, Clone)]
pub struct QueryShape {
    source: String,
    privacy: Privacy,
    declared: Vec<(String, String)>,
    forbidden: Vec<String>,
    /// The output columns, when known (from `DESCRIBE` or the result).
    output: Option<Vec<(String, String)>>,
    vars: Vec<String>,
    thresholds: BTreeMap<String, u64>,
    rules: Vec<String>,
}

impl QueryShape {
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn privacy(&self) -> Privacy {
        self.privacy
    }
    /// The output columns, `None` when the server could not describe them
    /// before the run (`describe_refused`).
    pub fn output_columns(&self) -> Option<&[(String, String)]> {
        self.output.as_deref()
    }
    pub fn forbidden_columns(&self) -> &[String] {
        &self.forbidden
    }

    /// A column the source declared or the output carries; `None` for any
    /// other text, so a value cannot pose as a column name.
    pub fn column(&self, name: &str) -> Option<SchemaColumn> {
        let declared = self.declared.iter().any(|(n, _)| n == name);
        let output = self.output.iter().flatten().any(|(n, _)| n == name);
        let forbidden = self.forbidden.iter().any(|n| n == name);
        (declared || output || forbidden).then(|| SchemaColumn(name.to_string()))
    }

    /// A variable the compiled query read.
    pub fn var(&self, key: &str) -> Option<VarKey> {
        self.vars.iter().any(|v| v == key).then(|| VarKey(key.to_string()))
    }

    /// A threshold declared at registration.
    pub fn threshold(&self, name: &str) -> Option<Threshold> {
        self.thresholds.get(name).map(|v| Threshold(*v))
    }

    /// A rule id declared at registration.
    pub fn rule(&self, id: &str) -> Option<RuleId> {
        self.rules.iter().any(|r| r == id).then(|| RuleId(id.to_string()))
    }

    /// The first output (or, before the run, declared) column the host
    /// forbids.
    pub fn first_forbidden(&self) -> Option<SchemaColumn> {
        let out = self.output.as_ref()?;
        out.iter().find(|(n, _)| self.forbidden.contains(n)).map(|(n, _)| SchemaColumn(n.clone()))
    }
}

/// Why [`run_query`] produced no `Results`.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryError {
    /// No source of that name is registered.
    UnknownSource,
    Refused(Refusal),
    Failed(SafeError),
}

impl From<SourceError> for QueryError {
    fn from(e: SourceError) -> Self {
        match e {
            SourceError::Refused(r) => QueryError::Refused(r),
            SourceError::Failed(s) => QueryError::Failed(s),
        }
    }
}

/// Runs a compiled query through its source's hook: `scope`, `check`
/// (with the `DESCRIBE`d columns when the source can give them), execute,
/// `check` again with the real columns, `apply`, then wrap. The only maker
/// of [`Results`].
pub async fn run_query(
    registry: &SourceRegistry,
    source: &str,
    q: &CompiledQuery,
    transport: &dyn HttpTransport,
    locale: &str,
) -> Result<Results, QueryError> {
    let g = registry.get(source).ok_or(QueryError::UnknownSource)?;
    let schema = g.source.schema();
    let mut shape = QueryShape {
        source: source.to_string(),
        privacy: g.privacy,
        declared: schema.columns.clone(),
        forbidden: schema.forbidden_columns.clone(),
        output: None,
        vars: q.deps().to_vec(),
        thresholds: g.thresholds.clone(),
        rules: g.rules.clone(),
    };
    match g.hook.scope(&shape).map_err(QueryError::Refused)? {
        None => {}
        Some(_) => return Err(QueryError::Refused(Refusal::ScopeUnavailable { rule: None })),
    }
    if g.privacy == Privacy::AggregateOnly {
        // A failed or refused DESCRIBE leaves the columns unknown; the
        // check after the run still sees them (§8.2 `describe_refused`).
        if let Ok(Some(cols)) = g.source.describe(q, transport).await {
            shape.output = Some(cols);
        }
    }
    g.hook.check(&shape).map_err(QueryError::Refused)?;
    let raw = g.source.execute(q, transport).await?;
    shape.output = Some(raw.columns().iter().map(|c| (c.name().to_string(), c.sql_type().to_string())).collect());
    g.hook.check(&shape).map_err(QueryError::Refused)?;
    let rows_before = raw.row_count();
    let hooked = g.hook.apply(&shape, raw).map_err(QueryError::Refused)?;
    if hooked.row_count() > rows_before {
        return Err(QueryError::Refused(Refusal::PolicyDenied { rule: RuleId("hook_added_rows".into()) }));
    }
    let (columns, rows, truncated) = hooked.into_columns();
    Ok(Results::from_columns(columns, rows, truncated, g.hook.suppressed_label(locale)))
}
