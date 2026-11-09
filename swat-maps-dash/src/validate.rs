//! Load-time diagnostics (spec §2.2 `validate`).
//!
//! [`Diagnostic`] is serde-identical to the story `Diagnostic`
//! (`src/story/script.rs`): a JSON pointer, a stable code, a message and an
//! optional hint, so an agent fixes a page the way it fixes a story.

use serde::{Deserialize, Serialize};

/// How serious a [`Diagnostic`] is. Only `Error` blocks a page or a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// One thing wrong (or worth mentioning) about a page.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// A JSON pointer (RFC 6901) to the offending value; `""` is the root.
    pub path: String,
    /// A stable, machine-checkable code such as `column_not_identifier`.
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn error(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Error, path: path.into(), code, message: message.into(), hint: None }
    }

    pub fn warning(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Warning, path: path.into(), code, message: message.into(), hint: None }
    }

    pub fn info(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Info, path: path.into(), code, message: message.into(), hint: None }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Escapes one JSON-pointer reference token (`~` → `~0`, `/` → `~1`).
pub fn pointer_token(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// True when any diagnostic is an error.
pub fn has_errors(d: &[Diagnostic]) -> bool {
    d.iter().any(Diagnostic::is_error)
}
