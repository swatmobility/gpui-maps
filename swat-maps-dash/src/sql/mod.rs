//! Server mode (§4.2): the template lexer and compiler, the generated-SQL
//! compiler, the ClickHouse and Postgres dialects (Postgres is explorer-sql
//! D1), and the read guard (C1, D1, D5).
//!
//! Variables bind as parameters, never as text: `${day_type}` compiles to
//! `{p_day_type:String}` for ClickHouse and to `$1` with a typed parameter
//! for Postgres. A compiled statement's text is readable only through
//! [`CompiledQuery::guarded`] (D2).

pub mod catalogue;
mod compile;
mod dialect;
pub mod guard;
pub mod lexer;
pub mod tables;
mod template;
mod value;

pub use compile::{
    generated_text, CompileInput, CompiledQuery, GuardedSql, Param, ParamValue, Pending, DEFAULT_MAX_ROWS,
};
pub use dialect::{strip_wrappers, Dialect};
pub use guard::{GuardCode, GuardOptions, CLICKHOUSE_DENIED_TABLE_FUNCTIONS};
pub use template::{Part, Piece, Slot, SqlIssue, Template, TemplateContext};
pub use value::{decimal_text, decimal_text_from_str, Scalar, VarValue};

use std::collections::BTreeMap;

use crate::spec::{Clause, Control, Page, Privacy, SourceKind, SqlText};
use crate::validate::{pointer_token, Diagnostic, Severity};

/// Where a SQL string sits on a page.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueryKey {
    /// A widget's inline `sql`.
    Widget(String),
    /// A named entry of `queries`.
    Query(String),
    /// A variable's `options_sql`.
    Options(String),
}

/// What the host tells the checker. The defaults are what
/// `check_collection` uses with no host: the 100,000-row ceiling and no
/// allowed table functions.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// The highest `max_rows` a source may ask for (D3; the Explorer
    /// registers 1,000,000).
    pub max_rows_ceiling: u64,
    pub guard: GuardOptions,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions { max_rows_ceiling: DEFAULT_MAX_ROWS, guard: GuardOptions::default() }
    }
}

/// A page's SQL, read once at load.
#[derive(Debug, Clone, Default)]
pub struct PageSql {
    pub templates: BTreeMap<QueryKey, (String, Template)>,
    pub diagnostics: Vec<Diagnostic>,
}

impl PageSql {
    /// Compiles the template at `key` with the committed `values`.
    pub fn compile(
        &self,
        page: &Page,
        key: &QueryKey,
        values: &BTreeMap<String, VarValue>,
        inferred_types: &BTreeMap<String, String>,
    ) -> Option<Result<CompiledQuery, Pending>> {
        let (source, t) = self.templates.get(key)?;
        let max_rows = page.sources.get(source).and_then(|s| s.max_rows).unwrap_or(DEFAULT_MAX_ROWS);
        Some(t.compile(&CompileInput { page, source, values, inferred_types, max_rows }))
    }
}

/// The dialect of a source: declared `clickhouse` or `postgres`; an
/// undeclared, host-registered source reads as ClickHouse. `None` for a
/// kind that has no dialect in this version.
pub fn dialect_of(page: &Page, source: &str) -> Option<Dialect> {
    match page.sources.get(source).and_then(|s| s.effective_kind()) {
        None | Some(SourceKind::Clickhouse) => Some(Dialect::ClickHouse),
        Some(SourceKind::Postgres) => Some(Dialect::Postgres),
        _ => None,
    }
}

/// Reads and checks every SQL string on the page (§4.2.1, §4.2.3 routing,
/// §4.2.5 cycles, D1 `having_alias_unsupported`, D3 `max_rows_over_ceiling`).
pub fn check_page(page: &Page, opts: &CheckOptions) -> PageSql {
    let mut out = PageSql::default();
    let controls: BTreeMap<&str, Control> = page.variables.iter().map(|v| (v.key.as_str(), v.control)).collect();
    let control_of = |k: &str| controls.get(k).copied();

    for (name, s) in &page.sources {
        if let Some(n) = s.max_rows {
            if n > opts.max_rows_ceiling {
                out.diagnostics.push(Diagnostic::error(
                    format!("/sources/{}/max_rows", pointer_token(name)),
                    "max_rows_over_ceiling",
                    format!("Source \"{name}\" asks for {n} rows, more than this application allows ({}).", opts.max_rows_ceiling),
                ));
            }
        }
    }
    for (i, f) in page.filters.iter().enumerate() {
        let src = f.source.as_deref().or(page.source.as_deref()).unwrap_or("");
        if f.clause == Clause::Having && dialect_of(page, src) == Some(Dialect::Postgres) {
            out.diagnostics.push(
                Diagnostic::error(
                    format!("/filters/{i}/clause"),
                    "having_alias_unsupported",
                    "Postgres does not accept output aliases in HAVING, so this filter cannot be a having filter.",
                )
                .with_hint("Filter the aggregate in an outer query on a where slot instead."),
            );
        }
    }

    // Every SQL string with its pointer and source.
    let mut sqls: Vec<(QueryKey, String, String, SqlText)> = Vec::new();
    let page_source = page.source.clone().unwrap_or_default();
    for (i, w) in page.widgets.iter().enumerate() {
        if let Some(sql) = &w.sql {
            let src = w.source.clone().unwrap_or_else(|| page_source.clone());
            sqls.push((QueryKey::Widget(w.id.clone()), format!("/widgets/{i}/sql"), src, sql.clone()));
        }
    }
    for (name, q) in &page.queries {
        let src = q.source.clone().unwrap_or_else(|| page_source.clone());
        let p = format!("/queries/{}", pointer_token(name));
        if let Some(sql) = &q.sql {
            sqls.push((QueryKey::Query(name.clone()), format!("{p}/sql"), src, sql.clone()));
        } else if !q.agg.is_empty() || !q.group.is_empty() {
            let Some(d) = dialect_of(page, &src) else { continue };
            let is_sql = page.sources.get(&src).and_then(|s| s.effective_kind()).is_none_or(SourceKind::is_sql);
            if !is_sql {
                continue; // client and engine sources run in their own modes
            }
            match generated_text(q, d) {
                Ok(text) => sqls.push((QueryKey::Query(name.clone()), p, src, SqlText::One(text))),
                Err(e) => out.diagnostics.push(Diagnostic::error(p, e.code, e.message)),
            }
        }
    }
    for (i, v) in page.variables.iter().enumerate() {
        if let Some(sql) = &v.options_sql {
            sqls.push((QueryKey::Options(v.key.clone()), format!("/variables/{i}/options_sql"), page_source.clone(), sql.clone()));
        }
    }

    for (key, path, source, sql) in sqls {
        let Some(dialect) = dialect_of(page, &source) else {
            out.diagnostics.push(Diagnostic::error(
                &path,
                "dialect_unavailable",
                format!("Source \"{source}\" has no SQL dialect in this version of the library."),
            ));
            continue;
        };
        let privacy = page.sources.get(&source).and_then(|s| s.privacy).unwrap_or(Privacy::Open);
        let cx = TemplateContext { dialect, control_of: &control_of, privacy, guard: &opts.guard };
        let text = sql.text();
        let (template, issues) = match Template::parse(&text, &cx) {
            Ok((t, issues)) => (Some(t), issues),
            Err(issues) => (None, issues),
        };
        for is in issues {
            let p = format!("{path}{}", sql.pointer_of(is.offset));
            out.diagnostics.push(Diagnostic { severity: is.severity, path: p, code: is.code, message: is.message, hint: None });
        }
        let Some(template) = template else { continue };
        // Routing (§4.2.3): a having filter on this source needs a having
        // slot, unless the query excludes its variable.
        let slots: Vec<(Slot, Vec<String>)> = template.slots().map(|(s, e)| (s, e.to_vec())).collect();
        let excluded = |var: &str| slots.iter().any(|(_, e)| e.iter().any(|x| x == var));
        let has_having = slots.iter().any(|(s, _)| s.is_having());
        let has_where = slots.iter().any(|(s, _)| matches!(s, Slot::Where | Slot::AndWhere));
        let mut where_unused = false;
        for f in &page.filters {
            if f.source.as_deref().unwrap_or(&page_source) != source || f.column.is_none() {
                continue;
            }
            if f.clause == Clause::Having && has_where && !has_having && !excluded(&f.var) {
                out.diagnostics.push(Diagnostic::error(
                    &path,
                    "having_filter_without_slot",
                    format!("The filter on {} is a having filter, but this query has no ${{having}} or ${{and_having}} slot.", f.var),
                ));
            }
            if f.clause == Clause::Where && !has_where && !matches!(key, QueryKey::Options(_)) {
                where_unused = true;
            }
        }
        if where_unused {
            out.diagnostics.push(Diagnostic::info(&path, "filters_not_used", "This query has no where slot, so the page's filters do not narrow it."));
        }
        out.templates.insert(key, (source, template));
    }

    // Cycles among options queries (§4.2.5).
    let deps_of = |k: &str| -> Vec<String> {
        let Some((src, t)) = out.templates.get(&QueryKey::Options(k.to_string())) else { return Vec::new() };
        let mut d: Vec<String> = t.placeholder_vars().into_iter().map(String::from).collect();
        for (slot, ex) in t.slots() {
            for f in &page.filters {
                let reaches = match f.clause {
                    Clause::Where => matches!(slot, Slot::Where | Slot::AndWhere),
                    Clause::Having => slot.is_having(),
                };
                if reaches && f.source.as_deref().unwrap_or(&page_source) == src && !ex.contains(&f.var) && f.var != k {
                    d.push(f.var.clone());
                }
            }
        }
        d
    };
    let mut reported: Vec<(String, String)> = Vec::new();
    for v in &page.variables {
        // Depth-first from v; a path back to v is a cycle.
        let mut stack: Vec<(String, Vec<String>)> = vec![(v.key.clone(), vec![v.key.clone()])];
        let mut seen: Vec<String> = Vec::new();
        while let Some((k, path)) = stack.pop() {
            for d in deps_of(&k) {
                if d == v.key && path.len() > 1 {
                    let mut pair = (v.key.clone(), path[1].clone());
                    if pair.0 > pair.1 {
                        pair = (pair.1, pair.0);
                    }
                    if !reported.contains(&pair) {
                        out.diagnostics.push(Diagnostic::error(
                            "/variables",
                            "dependency_cycle",
                            format!("The options of {} and {} depend on each other.", pair.0, pair.1),
                        ));
                        reported.push(pair);
                    }
                } else if d == v.key && path.len() == 1 {
                    // A variable's options narrowing by itself is not a cycle.
                } else if !seen.contains(&d) {
                    seen.push(d.clone());
                    let mut p = path.clone();
                    p.push(d.clone());
                    stack.push((d, p));
                }
            }
        }
    }
    out.diagnostics.sort_by_key(|d| d.severity != Severity::Error);
    out
}
