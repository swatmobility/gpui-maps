//! The page (§3.2), its queries, widgets and layout, and loading from JSON
//! with JSON-pointer diagnostics.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use super::{
    is_column_identifier, is_variable_key, FilterBinding, Localised, SourceDecl, SqlText, Variable,
    CONNECTION_DETAIL_KEYS, RESERVED_KEYS,
};
use crate::validate::{pointer_token, Diagnostic};

/// The page format version this library reads and writes.
pub const CURRENT_VERSION: u32 = 1;

/// A named query (§3.7).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryDecl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<SqlText>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_stat: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group: Vec<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agg: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crossfilter: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub with_inside: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub viewport: bool,
}

/// A widget (§3.7). Phase 1 types the keys every widget shares and the
/// encoding keys of the first set; interaction objects stay JSON values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WidgetDecl {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Localised>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<SqlText>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brush: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hover: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fly_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_totals: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col_totals: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grand_total: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals_marker: Option<String>,
    /// `bar`: `"h"` draws horizontal bars; absent or `"v"` vertical (§6.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orient: Option<String>,
    /// `map`: its layers (§7.1), each `{ id, type, title, query, geometry,
    /// measure, visible, color, click }`. Kept as JSON values until the map
    /// binding types them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<Value>,
}

/// One grid item (§3.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridItem {
    pub widget: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// The layout (§3.8): a grid or an overlay, never both (`layout_both`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<GridItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<Vec<Value>>,
}

/// A dashboard page (§3.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    /// The format version; absent is read as 1 (`version_assumed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub id: String,
    pub title: Localised,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, SourceDecl>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dimensions: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<Variable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<FilterBinding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub queries: BTreeMap<String, QueryDecl>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<WidgetDecl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<Value>,
    #[serde(default)]
    pub layout: Layout,
}

impl Page {
    /// Parses a page. On success the page comes with its non-error
    /// diagnostics (`version_assumed`, `profile_not_checked`, warnings);
    /// on failure every error found, each at its JSON pointer.
    pub fn from_json(json: &str) -> Result<(Page, Vec<Diagnostic>), Vec<Diagnostic>> {
        let root: Value = serde_json::from_str(json).map_err(|e| {
            vec![Diagnostic::error(
                "",
                "json_syntax",
                format!("The page is not valid JSON at line {}, column {}: {e}.", e.line(), e.column()),
            )]
        })?;
        let mut diags = prescan(&root);
        if diags.iter().any(Diagnostic::is_error) {
            return Err(diags);
        }
        // Typed, section by section, so an error lands at the element that
        // holds it rather than at the root.
        let typed = typed_errors(&root);
        if !typed.is_empty() {
            return Err(typed);
        }
        let page: Page = serde_json::from_value(root).map_err(|e| vec![serde_diag("", &e)])?;
        if page.dashboard.is_none() {
            diags.push(Diagnostic::info(
                "",
                "version_assumed",
                "The page has no \"dashboard\" version, so it was read as version 1.",
            ));
        }
        diags.extend(page.structural_diagnostics());
        if diags.iter().any(Diagnostic::is_error) {
            return Err(diags);
        }
        Ok((page, diags))
    }

    /// The page as JSON (`postgres_http` is written back as `postgres`).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// The variable with this key.
    pub fn variable(&self, key: &str) -> Option<&Variable> {
        self.variables.iter().find(|v| v.key == key)
    }

    /// Checks that need the whole typed page but no SQL: identifiers,
    /// reserved keys, unknown references and the layout form.
    pub fn structural_diagnostics(&self) -> Vec<Diagnostic> {
        let mut d = Vec::new();
        if let Some(v) = self.dashboard {
            if v > CURRENT_VERSION {
                d.push(Diagnostic::error(
                    "/dashboard",
                    "version_too_new",
                    format!("This page is format version {v}, newer than this library reads ({CURRENT_VERSION})."),
                ));
            }
        }
        for (i, v) in self.variables.iter().enumerate() {
            let p = format!("/variables/{i}/key");
            if RESERVED_KEYS.contains(&v.key.as_str()) {
                d.push(Diagnostic::error(
                    &p,
                    "variable_reserved",
                    format!("\"{}\" is a slot name and cannot be a variable key.", v.key),
                ));
            } else if !is_variable_key(&v.key) {
                d.push(Diagnostic::error(
                    &p,
                    "variable_not_identifier",
                    format!("Variable key \"{}\" must match [a-z_][a-z0-9_]*.", v.key),
                ));
            }
            if self.variables[..i].iter().any(|w| w.key == v.key) {
                d.push(Diagnostic::error(&p, "duplicate_variable", format!("Variable \"{}\" is declared twice.", v.key)));
            }
        }
        for (i, f) in self.filters.iter().enumerate() {
            if self.variable(&f.var).is_none() {
                d.push(Diagnostic::error(
                    format!("/filters/{i}/var"),
                    "unknown_variable",
                    format!("Filter {i} names the variable \"{}\", which the page does not declare.", f.var),
                ));
            }
            match (&f.column, &f.dimension) {
                (Some(c), None) => {
                    if !is_column_identifier(c) {
                        d.push(
                            Diagnostic::error(
                                format!("/filters/{i}/column"),
                                "column_not_identifier",
                                "A filter column must be a plain column name, optionally qualified by a table.",
                            )
                            .with_hint("Use letters, digits and underscores, as in day_type or t.day_type."),
                        );
                    }
                }
                (None, Some(_)) => {}
                _ => d.push(Diagnostic::error(
                    format!("/filters/{i}"),
                    "filter_target",
                    format!("Filter {i} needs exactly one of column and dimension."),
                )),
            }
        }
        for (name, q) in &self.queries {
            let p = format!("/queries/{}", pointer_token(name));
            if let Some(t) = &q.table {
                if !is_column_identifier(t) {
                    d.push(Diagnostic::error(
                        format!("{p}/table"),
                        "column_not_identifier",
                        "A query table must be a plain table name, optionally qualified by a database.",
                    ));
                }
            }
            let forms = q.sql.is_some() as u8 + (!q.agg.is_empty() || !q.group.is_empty()) as u8 + q.engine_stat.is_some() as u8;
            if forms != 1 {
                d.push(Diagnostic::error(
                    &p,
                    "query_form",
                    format!("Query \"{name}\" needs exactly one of sql, group/agg and engine_stat."),
                ));
            }
        }
        for (i, w) in self.widgets.iter().enumerate() {
            if self.widgets[..i].iter().any(|o| o.id == w.id) {
                d.push(Diagnostic::error(format!("/widgets/{i}/id"), "duplicate_widget", format!("Widget \"{}\" is declared twice.", w.id)));
            }
            if let Some(Value::String(q)) = &w.query {
                if !self.queries.contains_key(q) {
                    d.push(Diagnostic::error(
                        format!("/widgets/{i}/query"),
                        "unknown_query",
                        format!("Widget \"{}\" names the query \"{q}\", which the page does not declare.", w.id),
                    ));
                }
            }
        }
        let l = &self.layout;
        if l.items.is_some() && l.overlay.is_some() {
            d.push(Diagnostic::error("/layout", "layout_both", "A layout is a grid or an overlay, not both."));
        }
        if let Some(c) = l.columns {
            if c != 12 {
                d.push(Diagnostic::error("/layout/columns", "columns_not_12", format!("A grid has 12 columns in version 1, not {c}.")));
            }
        }
        for (i, it) in l.items.iter().flatten().enumerate() {
            if !self.widgets.iter().any(|w| w.id == it.widget) {
                d.push(Diagnostic::error(
                    format!("/layout/items/{i}/widget"),
                    "unknown_widget",
                    format!("The layout places \"{}\", which is not a widget on this page.", it.widget),
                ));
            }
        }
        d
    }
}

/// Checks made on the raw JSON before typing, for keys whose own code is
/// more useful than "unknown field" (`connection_details_in_page`,
/// `profile_unknown`).
fn prescan(root: &Value) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    match root.get("profile").map(|p| p.as_str()) {
        None | Some(Some("dashboard")) => {}
        Some(Some("map")) => d.push(Diagnostic::info(
            "/profile",
            "profile_not_checked",
            "This page uses the map profile, which this version loads without checking.",
        )),
        Some(_) => d.push(Diagnostic::error("/profile", "profile_unknown", "A profile is \"dashboard\" or \"map\".")),
    }
    if let Some(Value::Object(sources)) = root.get("sources") {
        for (name, src) in sources {
            let Value::Object(fields) = src else { continue };
            let kind = fields.get("kind").and_then(Value::as_str);
            let sql_kind = fields.contains_key("connection")
                || matches!(kind, Some("clickhouse" | "postgres" | "postgres_http" | "duckdb_http"));
            for key in CONNECTION_DETAIL_KEYS {
                if fields.contains_key(key) && (sql_kind || key != "url") {
                    d.push(
                        Diagnostic::error(
                            format!("/sources/{}/{key}", pointer_token(name)),
                            "connection_details_in_page",
                            format!("Source \"{name}\" carries \"{key}\", but connection details belong to the application."),
                        )
                        .with_hint("Name a connection the application registers, and remove the key from the page."),
                    );
                }
            }
        }
    }
    d
}

fn serde_diag(base: &str, e: &serde_json::Error) -> Diagnostic {
    let msg = e.to_string();
    // "unknown field `x`, expected …": point at the key itself.
    let path = msg
        .strip_prefix("unknown field `")
        .and_then(|r| r.split('`').next())
        .map(|f| format!("{base}/{}", pointer_token(f)))
        .unwrap_or_else(|| base.to_string());
    let code = if msg.starts_with("unknown field") { "unknown_key" } else { "invalid_value" };
    Diagnostic::error(path, code, format!("{msg}."))
}

fn typed_errors(root: &Value) -> Vec<Diagnostic> {
    fn each<T: serde::de::DeserializeOwned>(v: Option<&Value>, base: &str, out: &mut Vec<Diagnostic>) {
        match v {
            Some(Value::Array(items)) => {
                for (i, it) in items.iter().enumerate() {
                    if let Err(e) = serde_json::from_value::<T>(it.clone()) {
                        out.push(serde_diag(&format!("{base}/{i}"), &e));
                    }
                }
            }
            Some(Value::Object(items)) => {
                for (k, it) in items {
                    if let Err(e) = serde_json::from_value::<T>(it.clone()) {
                        out.push(serde_diag(&format!("{base}/{}", pointer_token(k)), &e));
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    each::<SourceDecl>(root.get("sources"), "/sources", &mut out);
    each::<Variable>(root.get("variables"), "/variables", &mut out);
    each::<FilterBinding>(root.get("filters"), "/filters", &mut out);
    each::<QueryDecl>(root.get("queries"), "/queries", &mut out);
    each::<WidgetDecl>(root.get("widgets"), "/widgets", &mut out);
    if let Some(l) = root.get("layout") {
        if let Err(e) = serde_json::from_value::<Layout>(l.clone()) {
            out.push(serde_diag("/layout", &e));
        }
    }
    if out.is_empty() {
        if let Err(e) = serde_json::from_value::<Page>(root.clone()) {
            out.push(serde_diag("", &e));
        }
    }
    out
}
