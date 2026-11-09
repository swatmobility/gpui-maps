//! `check_collection` (§11.2): a stable public API a consumer runs in its
//! own CI over its real pages. It parses, validates, compiles every SQL
//! string with stub values and solves the layout at the three width
//! classes. It opens no connection and sends nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::layout;
use crate::spec::{Control, Page, Variable};
use crate::sql::{check_page, CheckOptions, Pending, Scalar, VarValue};
use crate::validate::Diagnostic;

/// One page file's findings.
#[derive(Debug, Clone)]
pub struct PageReport {
    pub file: PathBuf,
    pub id: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Every page in a collection, and the duplicate ids.
#[derive(Debug, Clone, Default)]
pub struct CollectionReport {
    pub pages: Vec<PageReport>,
    pub duplicates: Vec<Diagnostic>,
}

impl CollectionReport {
    /// The number of errors across every page and the duplicates.
    pub fn errors(&self) -> usize {
        self.pages.iter().flat_map(|p| &p.diagnostics).chain(&self.duplicates).filter(|d| d.is_error()).count()
    }

    /// Panics with every error's file, JSON pointer, code and message.
    pub fn assert_clean(&self) {
        let mut lines = Vec::new();
        for p in &self.pages {
            for d in p.diagnostics.iter().filter(|d| d.is_error()) {
                lines.push(format!("{}: {} [{}] {}", p.file.display(), d.path, d.code, d.message));
            }
        }
        for d in self.duplicates.iter().filter(|d| d.is_error()) {
            lines.push(format!("{} [{}] {}", d.path, d.code, d.message));
        }
        assert!(lines.is_empty(), "check_collection found {} error(s):\n{}", lines.len(), lines.join("\n"));
    }
}

/// A stub value of the variable's shape and type, so every template
/// compiles to `(sql, params)` without a server.
fn stub(v: &Variable) -> VarValue {
    let ty = v.ty.as_deref().map(crate::sql::strip_wrappers).unwrap_or("String");
    let scalar = if ty.starts_with("Int") || ty.starts_with("UInt") {
        Scalar::Int(1)
    } else if ty.starts_with("Float") || ty.starts_with("Decimal") {
        Scalar::Float(1.0)
    } else if ty == "Bool" {
        Scalar::Bool(true)
    } else if ty.starts_with("Date") {
        Scalar::Str("2026-01-01".into())
    } else {
        Scalar::Str("stub".into())
    };
    match v.control {
        Control::Multi => VarValue::Many(vec![scalar]),
        Control::Range | Control::DateRange => VarValue::Range { lo: Some(scalar.clone()), hi: Some(scalar) },
        _ => VarValue::One(scalar),
    }
}

/// Checks one page's text: everything `check_collection` does for a file.
pub fn check_page_text(json: &str) -> (Option<String>, Vec<Diagnostic>) {
    let (page, mut d) = match Page::from_json(json) {
        Ok(ok) => ok,
        Err(d) => return (None, d),
    };
    let sql = check_page(&page, &CheckOptions::default());
    d.extend(sql.diagnostics.iter().cloned());
    let values: BTreeMap<String, VarValue> = page.variables.iter().map(|v| (v.key.clone(), stub(v))).collect();
    for key in sql.templates.keys() {
        match sql.compile(&page, key, &values, &BTreeMap::new()) {
            Some(Ok(q)) => {
                if let Err(r) = q.guarded(q.dialect()) {
                    d.push(Diagnostic::error("", r.code(), format!("{key:?} compiled to a statement the guard refuses.")));
                }
            }
            Some(Err(Pending::Unbound { var } | Pending::ChooseOne { var } | Pending::TooMany { var })) => {
                d.push(Diagnostic::error("", "stub_unbound", format!("{key:?} could not bind {var} with a stub value.")));
            }
            None => {}
        }
    }
    let declared: Vec<&str> = page.sources.keys().map(String::as_str).collect();
    let mut used: Vec<&str> = page.source.as_deref().into_iter().collect();
    used.extend(page.widgets.iter().filter_map(|w| w.source.as_deref()));
    used.extend(page.queries.values().filter_map(|q| q.source.as_deref()));
    used.sort_unstable();
    used.dedup();
    for s in used {
        if !declared.contains(&s) {
            d.push(Diagnostic::info("/source", "source_from_host", format!("Source \"{s}\" is not declared on the page; the host registers it.")));
        }
    }
    for w in [390.0, 800.0, 1280.0] {
        let _ = layout::solve(&page.layout, w);
    }
    (Some(page.id), d)
}

/// Checks every `*.json` directly in `path` (subdirectories are ignored).
#[cfg(feature = "fs")]
pub fn check_collection(path: impl AsRef<std::path::Path>) -> CollectionReport {
    let mut files: Vec<PathBuf> = std::fs::read_dir(path.as_ref())
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "json") && p.is_file()).collect())
        .unwrap_or_default();
    files.sort();
    let mut report = CollectionReport::default();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for file in files {
        let (id, diagnostics) = match std::fs::read_to_string(&file) {
            Ok(text) => check_page_text(&text),
            Err(e) => (None, vec![Diagnostic::error("", "file_unreadable", format!("The file could not be read: {e}."))]),
        };
        if let Some(id) = &id {
            if let Some(first) = seen.get(id) {
                report.duplicates.push(Diagnostic::error(
                    file.display().to_string(),
                    "duplicate_page_id",
                    format!("Page id \"{id}\" is already used by {}; this page does not appear.", first.display()),
                ));
            } else {
                seen.insert(id.clone(), file.clone());
            }
        }
        report.pages.push(PageReport { file, id, diagnostics });
    }
    report
}
