//! The SQL lexer shared by the template reader (§4.2.1) and the read guard
//! (C1). It is not a parser: it only knows which bytes are strings,
//! identifiers, comments, words and symbols, per dialect, so neither the
//! template rules nor the guard are fooled by text inside a string or a
//! comment.
//!
//! The comment and quoting conventions follow the scanner in clickpad's
//! `ch-core::sql_scan` (our own ClickHouse explorer), extended with the
//! Postgres rules of explorer-sql D1: `$tag$…$tag$` dollar quoting, `E'…'`
//! escape strings, standard strings where a backslash is literal, and
//! nested `/* */` comments.

use super::Dialect;

/// What a lexeme is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Space,
    Comment,
    /// A string literal (`'…'`, `E'…'`, `$tag$…$tag$`).
    Str,
    /// A quoted identifier (`"…"`, and `` `…` `` in ClickHouse).
    QuotedIdent,
    /// A bare word: keyword, identifier or number.
    Word,
    /// One punctuation byte.
    Sym(u8),
    /// A template placeholder `${…}`.
    Template,
    /// A Postgres positional parameter `$1`.
    Positional,
}

/// One lexeme: its kind and its byte range in the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexeme {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
}

impl Lexeme {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }

    /// True for a lexeme that carries meaning (not space or a comment).
    pub fn significant(&self) -> bool {
        !matches!(self.kind, Kind::Space | Kind::Comment)
    }
}

/// A string, identifier or comment that never closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unterminated {
    pub at: usize,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Splits `src` into lexemes under `dialect`'s quoting and comment rules.
pub fn lex(src: &str, dialect: Dialect) -> Result<Vec<Lexeme>, Unterminated> {
    let b = src.as_bytes();
    let pg = dialect == Dialect::Postgres;
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let next = b.get(i + 1).copied();
        let kind = if c.is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            Kind::Space
        } else if (c == b'-' && next == Some(b'-')) || (!pg && c == b'#' && matches!(next, Some(b' ' | b'!'))) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            Kind::Comment
        } else if c == b'/' && next == Some(b'*') {
            // Postgres comments nest; ClickHouse's end at the first `*/`.
            let mut depth = 1;
            i += 2;
            while depth > 0 {
                if i + 1 >= b.len() {
                    return Err(Unterminated { at: start });
                }
                if b[i] == b'*' && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else if pg && b[i] == b'/' && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Kind::Comment
        } else if c == b'\'' || (pg && (c == b'E' || c == b'e') && next == Some(b'\'')) {
            // ClickHouse strings take backslash escapes; Postgres standard
            // strings do not (a backslash is literal), its E'' strings do.
            let backslash = !pg || c != b'\'';
            i += if c == b'\'' { 1 } else { 2 };
            loop {
                match b.get(i) {
                    None => return Err(Unterminated { at: start }),
                    Some(b'\\') if backslash => i += 2,
                    Some(b'\'') if b.get(i + 1) == Some(&b'\'') => i += 2,
                    Some(b'\'') => {
                        i += 1;
                        break;
                    }
                    Some(_) => i += 1,
                }
            }
            Kind::Str
        } else if c == b'"' || (!pg && c == b'`') {
            i += 1;
            loop {
                match b.get(i) {
                    None => return Err(Unterminated { at: start }),
                    Some(b'\\') if !pg => i += 2,
                    Some(&q) if q == c && b.get(i + 1) == Some(&c) => i += 2,
                    Some(&q) if q == c => {
                        i += 1;
                        break;
                    }
                    Some(_) => i += 1,
                }
            }
            Kind::QuotedIdent
        } else if c == b'$' && next == Some(b'{') {
            match src[i..].find('}') {
                Some(off) => i += off + 1,
                None => return Err(Unterminated { at: start }),
            }
            Kind::Template
        } else if c == b'$' && next.is_some_and(|d| d.is_ascii_digit()) {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            Kind::Positional
        } else if c == b'$' && dollar_tag(b, i).is_some() {
            // `$tag$ … $tag$`: Postgres dollar quoting, and ClickHouse's
            // heredoc string literal, which has the same form.
            let tag_end = dollar_tag(b, i).unwrap_or(i + 1);
            let tag = &src[i..tag_end];
            match src[tag_end..].find(tag) {
                Some(off) => i = tag_end + off + tag.len(),
                None => return Err(Unterminated { at: start }),
            }
            Kind::Str
        } else if is_word_byte(c) {
            while i < b.len() && is_word_byte(b[i]) {
                i += 1;
            }
            Kind::Word
        } else {
            i += 1;
            Kind::Sym(c)
        };
        out.push(Lexeme { kind, start, end: i });
    }
    Ok(out)
}

/// The end (one past the closing `$`) of a dollar-quote opening tag at `i`:
/// `$$` or `$ident$`, where the identifier does not start with a digit.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str, d: Dialect) -> Vec<Kind> {
        lex(s, d).unwrap().into_iter().filter(Lexeme::significant).map(|l| l.kind).collect()
    }

    #[test]
    fn strings_hide_their_semicolons() {
        assert_eq!(kinds("SELECT 'a;b'", Dialect::ClickHouse), [Kind::Word, Kind::Str]);
        assert_eq!(kinds("SELECT 'it\\'s;'", Dialect::ClickHouse), [Kind::Word, Kind::Str]);
        // In Postgres a backslash is literal in a standard string, so the
        // string ends at the second quote and the `;` is live.
        let pg = kinds("SELECT '\\'; DROP TABLE t; --'", Dialect::Postgres);
        assert!(pg.contains(&Kind::Sym(b';')), "{pg:?}");
        assert_eq!(kinds("SELECT E'\\';'", Dialect::Postgres), [Kind::Word, Kind::Str]);
    }

    #[test]
    fn dollar_quotes_and_nested_comments() {
        assert_eq!(kinds("SELECT $q$ ; $q$", Dialect::Postgres), [Kind::Word, Kind::Str]);
        assert_eq!(kinds("SELECT $$;$$", Dialect::Postgres), [Kind::Word, Kind::Str]);
        assert_eq!(kinds("SELECT /* a /* b */ ; */ 1", Dialect::Postgres), [Kind::Word, Kind::Word]);
        // ClickHouse comments do not nest: the `;` after the first `*/` is live.
        assert!(kinds("SELECT /* a /* b */ ; */ 1", Dialect::ClickHouse).contains(&Kind::Sym(b';')));
        assert_eq!(kinds("SELECT $1, ${x}", Dialect::Postgres), [Kind::Word, Kind::Positional, Kind::Sym(b','), Kind::Template]);
    }

    #[test]
    fn unterminated_is_an_error() {
        assert!(lex("SELECT 'a", Dialect::ClickHouse).is_err());
        assert!(lex("SELECT /* a", Dialect::Postgres).is_err());
        assert!(lex("SELECT $q$ a", Dialect::Postgres).is_err());
    }
}
