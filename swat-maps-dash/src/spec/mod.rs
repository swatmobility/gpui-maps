//! The page schema (spec §3). Every struct is `deny_unknown_fields`: a typo
//! is an error naming its JSON pointer, never a silently ignored key.
//!
//! Phase 1 types what the server-mode SQL path reads (sources, variables,
//! filters, queries, widgets, the grid layout). Client-mode dimensions, the
//! overlay layout, the page map and label sets are carried as JSON values
//! until their phases type them.

mod page;
mod source;
mod variable;

pub use page::{GridItem, Layout, Page, QueryDecl, WidgetDecl, CURRENT_VERSION};
pub use source::{Execution, Privacy, SourceDecl, SourceKind, CONNECTION_DETAIL_KEYS};
pub use variable::{Clause, Control, FilterBinding, Op, Required, Variable};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A title or label: a string, or an object of locale → string (§3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Localised {
    Plain(String),
    Locales(BTreeMap<String, String>),
}

impl Localised {
    /// The host's locale, then `en`, then the first key.
    pub fn get(&self, locale: &str) -> &str {
        match self {
            Localised::Plain(s) => s,
            Localised::Locales(m) => m
                .get(locale)
                .or_else(|| m.get("en"))
                .or_else(|| m.values().next())
                .map(String::as_str)
                .unwrap_or(""),
        }
    }
}

/// SQL text: a string, or an array of lines joined with `\n` (§3.7). The
/// array form keeps each line so a diagnostic can point at `/…/sql/2`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SqlText {
    One(String),
    Lines(Vec<String>),
}

impl SqlText {
    /// The text the compiler reads.
    pub fn text(&self) -> String {
        match self {
            SqlText::One(s) => s.clone(),
            SqlText::Lines(l) => l.join("\n"),
        }
    }

    /// The JSON pointer suffix of the line holding byte `offset` of
    /// [`SqlText::text`] (`""` for the string form, `/2` for line 3).
    pub fn pointer_of(&self, offset: usize) -> String {
        match self {
            SqlText::One(_) => String::new(),
            SqlText::Lines(lines) => {
                let mut at = 0;
                for (i, l) in lines.iter().enumerate() {
                    if offset <= at + l.len() {
                        return format!("/{i}");
                    }
                    at += l.len() + 1;
                }
                format!("/{}", lines.len().saturating_sub(1))
            }
        }
    }
}

/// `true` when `s` is a column reference the compiler may paste:
/// `^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)?$` (§3.6). Anything
/// else is `column_not_identifier`, so a page cannot smuggle SQL through a
/// column name.
pub fn is_column_identifier(s: &str) -> bool {
    let mut parts = s.split('.');
    let ok = |p: &str| {
        let mut c = p.chars();
        matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
            && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    };
    let first = parts.next().is_some_and(ok);
    let second = parts.next().is_none_or(ok);
    first && second && parts.next().is_none()
}

/// `true` for a variable key: `[a-z_][a-z0-9_]*`.
pub fn is_variable_key(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_lowercase() || ch == '_')
        && c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

/// Keys a variable may not take: the slot names (§3.5).
pub const RESERVED_KEYS: [&str; 5] = ["where", "and_where", "having", "and_having", "viewport"];
