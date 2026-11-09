//! Reading a template once at load (§4.2.1): its placeholders, its slots
//! and the load-time rules.

use super::guard::{self, GuardOptions};
use super::lexer::{lex, Kind, Lexeme};
use super::Dialect;
use crate::spec::{Control, Privacy};
use crate::validate::Severity;

/// Which half of a range a placeholder reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Whole,
    Lo,
    Hi,
}

/// An expansion slot (§4.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Where,
    AndWhere,
    Having,
    AndHaving,
    Viewport,
}

impl Slot {
    pub fn is_having(self) -> bool {
        matches!(self, Slot::Having | Slot::AndHaving)
    }
}

/// A piece of a read template.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// SQL text, verbatim (comments included).
    Text(String),
    /// `${key}`, `${key.lo}`, `${key.hi}`.
    Var { key: String, part: Part },
    /// `${where}` and the other slots, with `-var` exclusions.
    Slot { slot: Slot, exclude: Vec<String> },
}

/// One problem found in a template, at a byte offset of its text.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlIssue {
    pub offset: usize,
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
}

impl SqlIssue {
    fn error(offset: usize, code: &'static str, message: impl Into<String>) -> Self {
        SqlIssue { offset, severity: Severity::Error, code, message: message.into() }
    }
}

/// A template, read and checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub(crate) dialect: Dialect,
    pub(crate) pieces: Vec<Piece>,
}

/// What the template reader needs to know about the page.
pub struct TemplateContext<'a> {
    pub dialect: Dialect,
    /// The control of a declared variable, or `None` for an unknown key.
    pub control_of: &'a dyn Fn(&str) -> Option<Control>,
    /// The source's privacy: `select_star` is an error on aggregate-only
    /// sources and a warning on open ones (D5, F3).
    pub privacy: Privacy,
    pub guard: &'a GuardOptions,
}

impl Template {
    /// Reads `text`. Errors stop the template; warnings come back with it.
    pub fn parse(text: &str, cx: &TemplateContext<'_>) -> Result<(Template, Vec<SqlIssue>), Vec<SqlIssue>> {
        let mut issues = Vec::new();
        let lexemes = match lex(text, cx.dialect) {
            Ok(l) => l,
            Err(u) => {
                return Err(vec![SqlIssue::error(u.at, "sql_unterminated", "A string, identifier or comment in this SQL never closes.")])
            }
        };
        let sig: Vec<&Lexeme> = lexemes.iter().filter(|l| l.significant()).collect();

        for (i, l) in sig.iter().enumerate() {
            match l.kind {
                Kind::Str if l.text(text).contains("${") => issues.push(SqlIssue::error(
                    l.start,
                    "placeholder_in_string",
                    "A ${...} placeholder sits inside a string literal, where binding would quote it.",
                )),
                Kind::Sym(b'{') => {
                    let w = sig.get(i + 1).filter(|n| n.kind == Kind::Word);
                    match (w, sig.get(i + 2).map(|n| n.kind)) {
                        (Some(_), Some(Kind::Sym(b'}'))) => issues.push(SqlIssue::error(
                            l.start,
                            "bare_brace_placeholder",
                            "{name} is not a placeholder here; write ${name}.",
                        )),
                        (Some(_), Some(Kind::Sym(b':'))) => issues.push(SqlIssue::error(
                            l.start,
                            "native_param_in_template",
                            "A {name:Type} parameter written in a template is never bound; use ${name}.",
                        )),
                        _ => {}
                    }
                }
                Kind::Positional => issues.push(SqlIssue::error(
                    l.start,
                    "native_param_in_template",
                    "A $n parameter written in a template is never bound; use ${name}.",
                )),
                Kind::Sym(b'*') if star_select(text, &sig, i) => {
                    let severity = if cx.privacy == Privacy::AggregateOnly { Severity::Error } else { Severity::Warning };
                    issues.push(SqlIssue {
                        offset: l.start,
                        severity,
                        code: "select_star",
                        message: "SELECT * reads every column; name the columns this widget needs.".into(),
                    });
                }
                _ => {}
            }
        }

        // Pieces, and the placeholder rules.
        let mut pieces = Vec::new();
        let mut buf = String::new();
        for l in &lexemes {
            if l.kind != Kind::Template {
                buf.push_str(l.text(text));
                continue;
            }
            let inner = &text[l.start + 2..l.end - 1];
            match read_placeholder(inner, l.start, cx) {
                Ok(p) => {
                    if !buf.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut buf)));
                    }
                    pieces.push(p);
                }
                Err(e) => issues.push(e),
            }
        }
        if !buf.is_empty() {
            pieces.push(Piece::Text(buf));
        }

        // The statement itself, with every placeholder as a neutral
        // stand-in: the guard's rules (one SELECT/WITH, no `;`, no denied
        // table function, …) hold for the template at load as well as for
        // the compiled statement at send.
        let skeleton: String = pieces
            .iter()
            .map(|p| match p {
                Piece::Text(t) => t.as_str(),
                Piece::Var { .. } => "NULL",
                Piece::Slot { .. } => " ",
            })
            .collect();
        if let Err(code) = guard::check(&skeleton, cx.dialect, cx.guard) {
            let message = match code {
                guard::GuardCode::TableFunction(f) => {
                    format!("The table function {f}() reaches outside this database, and this connection does not allow it.")
                }
                guard::GuardCode::MultipleStatements => "A template holds one statement; remove the ;.".into(),
                _ => "A template must be one SELECT or WITH statement that only reads.".into(),
            };
            issues.push(SqlIssue::error(0, code.code(), message));
        }

        if issues.iter().any(|i| i.severity == Severity::Error) {
            Err(issues)
        } else {
            Ok((Template { dialect: cx.dialect, pieces }, issues))
        }
    }

    /// The variables named in placeholders, in order, without repeats.
    pub fn placeholder_vars(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for p in &self.pieces {
            if let Piece::Var { key, .. } = p {
                if !out.contains(&key.as_str()) {
                    out.push(key);
                }
            }
        }
        out
    }

    /// The slots, with their exclusions.
    pub fn slots(&self) -> impl Iterator<Item = (Slot, &[String])> {
        self.pieces.iter().filter_map(|p| match p {
            Piece::Slot { slot, exclude } => Some((*slot, exclude.as_slice())),
            _ => None,
        })
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }
}

fn read_placeholder(inner: &str, at: usize, cx: &TemplateContext<'_>) -> Result<Piece, SqlIssue> {
    let mut words = inner.split_whitespace();
    let head = words.next().unwrap_or("");
    let slot = match head {
        "where" => Some(Slot::Where),
        "and_where" => Some(Slot::AndWhere),
        "having" => Some(Slot::Having),
        "and_having" => Some(Slot::AndHaving),
        "viewport" => Some(Slot::Viewport),
        _ => None,
    };
    if let Some(slot) = slot {
        let mut exclude = Vec::new();
        for w in words {
            let Some(k) = w.strip_prefix('-') else {
                return Err(SqlIssue::error(at, "placeholder_syntax", "A slot takes only -variable exclusions, as in ${and_where -district}."));
            };
            if (cx.control_of)(k).is_none() {
                return Err(SqlIssue::error(at, "unknown_variable", format!("The exclusion -{k} names no variable on this page.")));
            }
            exclude.push(k.to_string());
        }
        return Ok(Piece::Slot { slot, exclude });
    }
    if words.next().is_some() {
        return Err(SqlIssue::error(at, "placeholder_syntax", "A placeholder is ${name}, ${name.lo} or ${name.hi}."));
    }
    let (key, part) = match head.split_once('.') {
        None => (head, Part::Whole),
        Some((k, "lo")) => (k, Part::Lo),
        Some((k, "hi")) => (k, Part::Hi),
        Some(_) => return Err(SqlIssue::error(at, "placeholder_syntax", "A placeholder is ${name}, ${name.lo} or ${name.hi}.")),
    };
    match (cx.control_of)(key) {
        None => Err(SqlIssue::error(at, "unknown_variable", format!("${{{key}}} names no variable on this page."))),
        Some(c) if part != Part::Whole && !c.is_range() => {
            Err(SqlIssue::error(at, "range_part_on_scalar", format!("{key} is not a range, so it has no .lo or .hi.")))
        }
        Some(_) => Ok(Piece::Var { key: key.to_string(), part }),
    }
}

/// `SELECT *`, `SELECT DISTINCT *`, `, *` and `alias.*`; not `count(*)` or
/// `a * b` (the rule of `src/overlay/dotgrid/sql_guard.rs`).
fn star_select(src: &str, sig: &[&Lexeme], i: usize) -> bool {
    let Some(prev) = i.checked_sub(1).map(|p| sig[p]) else { return false };
    match prev.kind {
        Kind::Sym(b'.') | Kind::Sym(b',') => true,
        Kind::Word => ["SELECT", "DISTINCT", "ALL"].iter().any(|k| prev.text(src).eq_ignore_ascii_case(k)),
        _ => false,
    }
}
