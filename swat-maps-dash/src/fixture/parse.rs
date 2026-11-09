//! The parser for the fixture SQL subset (see [`super::eval`]). It reuses
//! the crate's lexer, so strings, quoted identifiers and comments read
//! exactly as the read guard reads them.

use super::db::Val;
use super::eval::EvalError;
use crate::sql::lexer::{lex, Kind, Lexeme};
use crate::sql::Dialect;

/// One token.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A bare word: keyword, identifier or function name (dotted names
    /// such as `t.col` are joined).
    Word(String),
    /// A quoted identifier, unquoted.
    Ident(String),
    /// A number literal's text (`12`, `0.25`).
    Num(String),
    /// A string literal, unescaped.
    Str(String),
    /// A parameter `{name:Type}`: its name.
    Param(String),
    /// Punctuation or an operator.
    Op(&'static str),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

/// Binary arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A parsed expression; names are not resolved yet.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ast {
    Lit(Val),
    Ident(String),
    Param(String),
    Neg(Box<Ast>),
    Not(Box<Ast>),
    And(Box<Ast>, Box<Ast>),
    Or(Box<Ast>, Box<Ast>),
    Bin(BinOp, Box<Ast>, Box<Ast>),
    Cmp(CmpOp, Box<Ast>, Box<Ast>),
    In { e: Box<Ast>, list: Vec<Ast>, neg: bool },
    Between { e: Box<Ast>, lo: Box<Ast>, hi: Box<Ast>, neg: bool },
    IsNull { e: Box<Ast>, neg: bool },
    Tuple(Vec<Ast>),
    /// `f(args)`, or `f(params)(args)` when `params` is set; `star` for `count(*)`.
    Call { name: String, params: Option<Vec<Ast>>, args: Vec<Ast>, star: bool },
}

/// One `SELECT` item.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectItem {
    /// `*`: every input column.
    Star,
    /// An expression, its explicit alias, and its output name (the alias,
    /// the identifier, or the expression's source text).
    Expr { expr: Ast, alias: Option<String>, name: String },
}

/// What a query reads from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FromClause {
    Table(String),
    Sub(Box<Query>),
}

/// One parsed `SELECT`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Query {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: FromClause,
    pub where_: Option<Ast>,
    pub group_by: Vec<Ast>,
    pub having: Option<Ast>,
    /// Expressions and whether each is descending.
    pub order_by: Vec<(Ast, bool)>,
    pub limit: Option<usize>,
    pub offset: usize,
}

/// Words that end an expression and are never a bare identifier.
const RESERVED: &[&str] = &[
    "SELECT", "DISTINCT", "FROM", "WHERE", "PREWHERE", "GROUP", "BY", "HAVING", "ORDER", "LIMIT", "OFFSET", "AS", "AND", "OR",
    "NOT", "IN", "BETWEEN", "IS", "ASC", "DESC", "JOIN", "UNION", "EXCEPT", "INTERSECT", "WITH", "ON", "USING", "LEFT", "RIGHT",
    "INNER", "OUTER", "FULL", "CROSS", "ARRAY", "SETTINGS", "FORMAT", "FINAL", "SAMPLE", "OVER", "LIKE", "ILIKE", "CASE",
    "WINDOW", "NULLS",
];

fn reserved(w: &str) -> bool {
    RESERVED.iter().any(|k| k.eq_ignore_ascii_case(w))
}

/// Parses one statement of the subset.
pub(crate) fn parse(src: &str) -> Result<Query, EvalError> {
    let toks = tokenize(src)?;
    let mut p = Parser { src, toks, pos: 0 };
    let q = p.query()?;
    p.eat_op(";");
    if p.pos < p.toks.len() {
        return Err(p.err("unexpected text after the query"));
    }
    Ok(q)
}

fn unescape(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some(o) => out.push(o),
                None => {}
            },
            '\'' => {
                // Only a doubled quote reaches here (the lexer ends the
                // string at a single one).
                out.push('\'');
                chars.next();
            }
            c => out.push(c),
        }
    }
    out
}

fn unquote_ident(text: &str) -> String {
    let q = text.chars().next().unwrap_or('"');
    let body = text.get(1..text.len().saturating_sub(1)).unwrap_or("");
    let doubled: String = [q, q].iter().collect();
    body.replace(&doubled, &q.to_string()).replace("\\\\", "\\")
}

fn op_of(c: u8) -> Option<&'static str> {
    Some(match c {
        b'=' => "=",
        b'<' => "<",
        b'>' => ">",
        b'+' => "+",
        b'-' => "-",
        b'*' => "*",
        b'/' => "/",
        b'%' => "%",
        b'(' => "(",
        b')' => ")",
        b',' => ",",
        b';' => ";",
        _ => return None,
    })
}

fn op2_of(a: u8, b: u8) -> Option<&'static str> {
    Some(match (a, b) {
        (b'<', b'=') => "<=",
        (b'>', b'=') => ">=",
        (b'!', b'=') => "!=",
        (b'<', b'>') => "<>",
        (b'=', b'=') => "==",
        (b'|', b'|') => "||",
        _ => return None,
    })
}

fn tokenize(src: &str) -> Result<Vec<Token>, EvalError> {
    let lexemes = lex(src, Dialect::ClickHouse)
        .map_err(|u| EvalError::Parse { at: u.at, message: "a string, identifier or comment does not close".into() })?;
    let lx: Vec<Lexeme> = lexemes.into_iter().filter(Lexeme::significant).collect();
    let digits = |l: &Lexeme| l.kind == Kind::Word && l.text(src).bytes().all(|b| b.is_ascii_digit());
    let mut out = Vec::new();
    let mut i = 0;
    while i < lx.len() {
        let l = lx[i];
        let text = l.text(src);
        let parse_err = |m: String| EvalError::Parse { at: l.start, message: m };
        match l.kind {
            Kind::Word => {
                let mut s = text.to_string();
                let mut end = l.end;
                if text.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                    // The lexer splits `0.25` into `0` `.` `25`: join them.
                    if let (Some(dot), Some(frac)) = (lx.get(i + 1), lx.get(i + 2)) {
                        if digits(&l) && dot.kind == Kind::Sym(b'.') && dot.start == end && frac.start == dot.end && digits(frac) {
                            s.push('.');
                            s.push_str(frac.text(src));
                            end = frac.end;
                            i += 2;
                        }
                    }
                    out.push(Token { tok: Tok::Num(s), start: l.start, end });
                } else {
                    // `db.table` and `t.col` read as one dotted name.
                    while let (Some(dot), Some(next)) = (lx.get(i + 1), lx.get(i + 2)) {
                        let joins = dot.kind == Kind::Sym(b'.')
                            && dot.start == end
                            && next.start == dot.end
                            && matches!(next.kind, Kind::Word | Kind::QuotedIdent);
                        if !joins {
                            break;
                        }
                        s.push('.');
                        if next.kind == Kind::QuotedIdent {
                            s.push_str(&unquote_ident(next.text(src)));
                        } else {
                            s.push_str(next.text(src));
                        }
                        end = next.end;
                        i += 2;
                    }
                    out.push(Token { tok: Tok::Word(s), start: l.start, end });
                }
            }
            Kind::Str => {
                let body = text.get(1..text.len().saturating_sub(1)).unwrap_or("");
                out.push(Token { tok: Tok::Str(unescape(body)), start: l.start, end: l.end });
            }
            Kind::QuotedIdent => out.push(Token { tok: Tok::Ident(unquote_ident(text)), start: l.start, end: l.end }),
            Kind::Sym(b'{') => {
                let close = src[l.start..].find('}').map(|o| l.start + o).ok_or_else(|| parse_err("a parameter does not close".into()))?;
                let inner = &src[l.start + 1..close];
                let name = inner.split_once(':').map_or(inner, |(n, _)| n).trim().to_string();
                out.push(Token { tok: Tok::Param(name), start: l.start, end: close + 1 });
                while lx.get(i + 1).is_some_and(|n| n.start <= close) {
                    i += 1;
                }
            }
            Kind::Sym(c) => {
                let two = lx.get(i + 1).filter(|n| n.start == l.end).and_then(|n| match n.kind {
                    Kind::Sym(d) => op2_of(c, d),
                    _ => None,
                });
                if let Some(op) = two {
                    out.push(Token { tok: Tok::Op(op), start: l.start, end: l.end + 1 });
                    i += 1;
                } else if let Some(op) = op_of(c) {
                    out.push(Token { tok: Tok::Op(op), start: l.start, end: l.end });
                } else {
                    return Err(parse_err(format!("unexpected '{text}'")));
                }
            }
            Kind::Template => return Err(parse_err(format!("the template placeholder {text} was not compiled"))),
            Kind::Positional => return Err(EvalError::Unsupported(format!("positional parameter {text}"))),
            Kind::Space | Kind::Comment => {}
        }
        i += 1;
    }
    Ok(out)
}

struct Parser<'s> {
    src: &'s str,
    toks: Vec<Token>,
    pos: usize,
}

type R<T> = Result<T, EvalError>;

impl Parser<'_> {
    fn at(&self) -> usize {
        self.toks.get(self.pos).map_or(self.src.len(), |t| t.start)
    }

    fn prev_end(&self) -> usize {
        self.pos.checked_sub(1).and_then(|i| self.toks.get(i)).map_or(0, |t| t.end)
    }

    fn err(&self, message: &str) -> EvalError {
        let near = self.toks.get(self.pos).map_or("the end".to_string(), |t| format!("'{}'", &self.src[t.start..t.end]));
        EvalError::Parse { at: self.at(), message: format!("{message} (at {near})") }
    }

    fn kw_at(&self, off: usize, kw: &str) -> bool {
        matches!(self.toks.get(self.pos + off), Some(Token { tok: Tok::Word(w), .. }) if w.eq_ignore_ascii_case(kw))
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.kw_at(0, kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        let hit = self.is_kw(kw);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn expect_kw(&mut self, kw: &str) -> R<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {kw}")))
        }
    }

    fn op_at(&self, off: usize, op: &str) -> bool {
        matches!(self.toks.get(self.pos + off), Some(Token { tok: Tok::Op(o), .. }) if *o == op)
    }

    fn is_op(&self, op: &str) -> bool {
        self.op_at(0, op)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        let hit = self.is_op(op);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn expect_op(&mut self, op: &str) -> R<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(self.err(&format!("expected '{op}'")))
        }
    }

    /// The current token is a name that can be an alias.
    fn at_plain_name(&self) -> bool {
        match self.toks.get(self.pos).map(|t| &t.tok) {
            Some(Tok::Word(w)) => !reserved(w),
            Some(Tok::Ident(_)) => true,
            _ => false,
        }
    }

    fn ident(&mut self) -> R<String> {
        let name = match self.toks.get(self.pos).map(|t| &t.tok) {
            Some(Tok::Word(w)) if !reserved(w) => w.clone(),
            Some(Tok::Ident(s)) => s.clone(),
            _ => return Err(self.err("expected a name")),
        };
        self.pos += 1;
        Ok(name)
    }

    fn unsupported_if(&self, words: &[&str], what: &str) -> R<()> {
        match words.iter().find(|w| self.is_kw(w)) {
            Some(w) => Err(EvalError::Unsupported(if what.is_empty() { w.to_ascii_uppercase() } else { what.to_string() })),
            None => Ok(()),
        }
    }

    fn count(&mut self) -> R<usize> {
        match self.toks.get(self.pos).map(|t| &t.tok) {
            Some(Tok::Num(n)) => {
                let v = n.parse::<usize>().map_err(|_| self.err("expected a row count"))?;
                self.pos += 1;
                Ok(v)
            }
            _ => Err(self.err("expected a row count")),
        }
    }

    fn query(&mut self) -> R<Query> {
        self.unsupported_if(&["WITH"], "WITH")?;
        self.expect_kw("SELECT")?;
        let distinct = self.eat_kw("DISTINCT");
        self.unsupported_if(&["ON"], "DISTINCT ON")?;
        let mut items = Vec::new();
        loop {
            if self.is_op("*") {
                self.pos += 1;
                items.push(SelectItem::Star);
            } else {
                let start = self.at();
                let expr = self.expr()?;
                let end = self.prev_end().max(start);
                let alias = if self.eat_kw("AS") || self.at_plain_name() { Some(self.ident()?) } else { None };
                let name = match (&alias, &expr) {
                    (Some(a), _) => a.clone(),
                    (None, Ast::Ident(n)) => n.clone(),
                    _ => self.src[start..end].trim().to_string(),
                };
                items.push(SelectItem::Expr { expr, alias, name });
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_kw("FROM")?;
        let from = if self.eat_op("(") {
            if !self.is_kw("SELECT") {
                return Err(self.err("expected a subquery"));
            }
            let q = self.query()?;
            self.expect_op(")")?;
            if self.eat_kw("AS") || self.at_plain_name() {
                self.ident()?;
            }
            FromClause::Sub(Box::new(q))
        } else {
            let name = self.ident()?;
            if self.is_op("(") {
                return Err(EvalError::Unsupported(format!("the table function {name}()")));
            }
            self.unsupported_if(&["FINAL"], "FINAL")?;
            if self.eat_kw("AS") || self.at_plain_name() {
                self.ident()?;
            }
            FromClause::Table(name)
        };
        self.unsupported_if(&["JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "GLOBAL", "ANY", "ALL", "ASOF", "SEMI", "ANTI", "PASTE"], "JOIN")?;
        self.unsupported_if(&["ARRAY"], "ARRAY JOIN")?;
        if self.is_op(",") {
            return Err(EvalError::Unsupported("JOIN (a comma in FROM)".into()));
        }
        self.unsupported_if(&["SAMPLE", "PREWHERE", "FINAL"], "")?;
        let where_ = if self.eat_kw("WHERE") { Some(self.expr()?) } else { None };
        let mut group_by = Vec::new();
        if self.eat_kw("GROUP") {
            self.expect_kw("BY")?;
            loop {
                group_by.push(self.expr()?);
                if !self.eat_op(",") {
                    break;
                }
            }
            self.unsupported_if(&["WITH"], "GROUP BY … WITH ROLLUP/CUBE/TOTALS")?;
        }
        let having = if self.eat_kw("HAVING") { Some(self.expr()?) } else { None };
        self.unsupported_if(&["WINDOW", "QUALIFY"], "window functions")?;
        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let e = self.expr()?;
                let desc = if self.eat_kw("DESC") || self.eat_kw("DESCENDING") {
                    true
                } else {
                    let _ = self.eat_kw("ASC") || self.eat_kw("ASCENDING");
                    false
                };
                if self.eat_kw("NULLS") {
                    if self.is_kw("FIRST") {
                        return Err(EvalError::Unsupported("NULLS FIRST".into()));
                    }
                    self.expect_kw("LAST")?;
                }
                self.unsupported_if(&["COLLATE"], "COLLATE")?;
                self.unsupported_if(&["WITH"], "ORDER BY … WITH FILL")?;
                order_by.push((e, desc));
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        let (mut limit, mut offset) = (None, 0);
        if self.eat_kw("LIMIT") {
            let a = self.count()?;
            if self.eat_op(",") {
                offset = a;
                limit = Some(self.count()?);
            } else {
                limit = Some(a);
                if self.eat_kw("OFFSET") {
                    offset = self.count()?;
                }
            }
            self.unsupported_if(&["BY"], "LIMIT BY")?;
            self.unsupported_if(&["WITH"], "LIMIT … WITH TIES")?;
        }
        self.unsupported_if(&["UNION", "EXCEPT", "INTERSECT"], "")?;
        self.unsupported_if(&["SETTINGS", "FORMAT", "INTO"], "")?;
        Ok(Query { distinct, items, from, where_, group_by, having, order_by, limit, offset })
    }

    fn expr(&mut self) -> R<Ast> {
        let mut l = self.and()?;
        while self.eat_kw("OR") {
            let r = self.and()?;
            l = Ast::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self) -> R<Ast> {
        let mut l = self.not()?;
        while self.eat_kw("AND") {
            let r = self.not()?;
            l = Ast::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not(&mut self) -> R<Ast> {
        if self.eat_kw("NOT") {
            Ok(Ast::Not(Box::new(self.not()?)))
        } else {
            self.cmp()
        }
    }

    fn cmp(&mut self) -> R<Ast> {
        let l = self.add()?;
        const OPS: [(&str, CmpOp); 8] = [
            ("=", CmpOp::Eq),
            ("==", CmpOp::Eq),
            ("!=", CmpOp::Ne),
            ("<>", CmpOp::Ne),
            ("<", CmpOp::Lt),
            ("<=", CmpOp::Le),
            (">", CmpOp::Gt),
            (">=", CmpOp::Ge),
        ];
        for (op, c) in OPS {
            if self.eat_op(op) {
                let r = self.add()?;
                return Ok(Ast::Cmp(c, Box::new(l), Box::new(r)));
            }
        }
        let neg = self.is_kw("NOT") && ["IN", "BETWEEN", "LIKE", "ILIKE"].iter().any(|k| self.kw_at(1, k));
        if neg {
            self.pos += 1;
        }
        if self.eat_kw("IN") {
            let list = if self.eat_op("(") {
                if self.is_kw("SELECT") {
                    return Err(EvalError::Unsupported("IN (subquery)".into()));
                }
                let mut v = Vec::new();
                if !self.is_op(")") {
                    loop {
                        v.push(self.expr()?);
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                }
                self.expect_op(")")?;
                v
            } else {
                vec![self.primary()?]
            };
            return Ok(Ast::In { e: Box::new(l), list, neg });
        }
        if self.eat_kw("BETWEEN") {
            let lo = self.add()?;
            self.expect_kw("AND")?;
            let hi = self.add()?;
            return Ok(Ast::Between { e: Box::new(l), lo: Box::new(lo), hi: Box::new(hi), neg });
        }
        self.unsupported_if(&["LIKE", "ILIKE"], "LIKE")?;
        if self.eat_kw("IS") {
            let n = self.eat_kw("NOT");
            self.expect_kw("NULL")?;
            return Ok(Ast::IsNull { e: Box::new(l), neg: n });
        }
        Ok(l)
    }

    fn add(&mut self) -> R<Ast> {
        let mut l = self.mul()?;
        loop {
            let op = if self.eat_op("+") {
                BinOp::Add
            } else if self.eat_op("-") {
                BinOp::Sub
            } else if self.eat_op("||") {
                BinOp::Concat
            } else {
                return Ok(l);
            };
            let r = self.mul()?;
            l = Ast::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn mul(&mut self) -> R<Ast> {
        let mut l = self.unary()?;
        loop {
            let op = if self.eat_op("*") {
                BinOp::Mul
            } else if self.eat_op("/") {
                BinOp::Div
            } else if self.eat_op("%") {
                BinOp::Mod
            } else {
                return Ok(l);
            };
            let r = self.unary()?;
            l = Ast::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> R<Ast> {
        if self.eat_op("-") {
            Ok(Ast::Neg(Box::new(self.unary()?)))
        } else if self.eat_op("+") {
            self.unary()
        } else {
            self.primary()
        }
    }

    fn primary(&mut self) -> R<Ast> {
        let Some(t) = self.toks.get(self.pos).cloned() else {
            return Err(self.err("expected an expression"));
        };
        let at = t.start;
        self.pos += 1;
        match t.tok {
            Tok::Num(s) => number(&s).map(Ast::Lit).ok_or_else(|| EvalError::Parse { at, message: format!("'{s}' is not a number") }),
            Tok::Str(s) => Ok(Ast::Lit(Val::Str(s))),
            Tok::Param(name) => Ok(Ast::Param(name)),
            Tok::Ident(s) => Ok(Ast::Ident(s)),
            Tok::Op("(") => {
                if self.is_kw("SELECT") {
                    return Err(EvalError::Unsupported("a scalar subquery".into()));
                }
                let e = self.expr()?;
                if self.eat_op(",") {
                    let mut items = vec![e];
                    loop {
                        items.push(self.expr()?);
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    Ok(Ast::Tuple(items))
                } else {
                    self.expect_op(")")?;
                    Ok(e)
                }
            }
            Tok::Word(w) => {
                if w.eq_ignore_ascii_case("NULL") {
                    return Ok(Ast::Lit(Val::Null));
                }
                if w.eq_ignore_ascii_case("TRUE") {
                    return Ok(Ast::Lit(Val::Int(1)));
                }
                if w.eq_ignore_ascii_case("FALSE") {
                    return Ok(Ast::Lit(Val::Int(0)));
                }
                if w.eq_ignore_ascii_case("CASE") {
                    return Err(EvalError::Unsupported("CASE".into()));
                }
                if w.eq_ignore_ascii_case("INTERVAL") {
                    return Err(EvalError::Unsupported("INTERVAL".into()));
                }
                if self.is_op("(") {
                    return self.call(w);
                }
                if reserved(&w) {
                    return Err(EvalError::Parse { at, message: format!("unexpected keyword {w}") });
                }
                Ok(Ast::Ident(w))
            }
            Tok::Op(o) => Err(EvalError::Parse { at, message: format!("unexpected '{o}'") }),
        }
    }

    fn call(&mut self, name: String) -> R<Ast> {
        self.expect_op("(")?;
        let (args, star) = self.call_args(&name)?;
        if self.eat_op("(") {
            let (args2, star2) = self.call_args(&name)?;
            if star {
                return Err(self.err("'*' is not a parameter"));
            }
            return Ok(Ast::Call { name, params: Some(args), args: args2, star: star2 });
        }
        self.unsupported_if(&["OVER"], "window functions (OVER)")?;
        Ok(Ast::Call { name, params: None, args, star })
    }

    /// The arguments after an opening `(`, through the closing `)`.
    fn call_args(&mut self, name: &str) -> R<(Vec<Ast>, bool)> {
        if self.eat_op(")") {
            return Ok((Vec::new(), false));
        }
        if self.is_op("*") && self.op_at(1, ")") {
            self.pos += 2;
            return Ok((Vec::new(), true));
        }
        if self.is_kw("DISTINCT") {
            return Err(EvalError::Unsupported(format!("{name}(DISTINCT …)")));
        }
        let mut args = Vec::new();
        loop {
            args.push(self.expr()?);
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok((args, false))
    }
}

/// A number literal: an integer when it is all digits and fits, else a float.
fn number(s: &str) -> Option<Val> {
    if s.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(i) = s.parse::<i64>() {
            return Some(Val::Int(i));
        }
    }
    s.parse::<f64>().ok().filter(|f| f.is_finite()).map(Val::Float)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_params_and_dotted_names_tokenize() {
        let q = parse("SELECT t.a, 0.25 AS q FROM db.t WHERE x IN {f_x:Array(String)}").unwrap();
        assert_eq!(q.from, FromClause::Table("db.t".into()));
        let SelectItem::Expr { expr, name, .. } = &q.items[1] else { panic!() };
        assert_eq!(expr, &Ast::Lit(Val::Float(0.25)));
        assert_eq!(name, "q");
        assert_eq!(q.where_, Some(Ast::In { e: Box::new(Ast::Ident("x".into())), list: vec![Ast::Param("f_x".into())], neg: false }));
    }

    #[test]
    fn an_unaliased_item_is_named_by_its_text() {
        let q = parse("SELECT count( * ) , 'it''s' FROM t").unwrap();
        let SelectItem::Expr { name, .. } = &q.items[0] else { panic!() };
        assert_eq!(name, "count( * )");
        let SelectItem::Expr { expr, .. } = &q.items[1] else { panic!() };
        assert_eq!(expr, &Ast::Lit(Val::Str("it's".into())));
    }

    #[test]
    fn joins_unions_and_with_are_refused_by_name() {
        assert_eq!(parse("SELECT a FROM t JOIN u ON t.a = u.a"), Err(EvalError::Unsupported("JOIN".into())));
        assert_eq!(parse("WITH 1 AS x SELECT x FROM t"), Err(EvalError::Unsupported("WITH".into())));
        assert_eq!(parse("SELECT a FROM t UNION ALL SELECT a FROM u"), Err(EvalError::Unsupported("UNION".into())));
        assert_eq!(parse("SELECT a FROM t LIMIT 1 BY a"), Err(EvalError::Unsupported("LIMIT BY".into())));
    }
}
